//! The sync engine transition function: the pure core of Tomo.
//!
//! [`Engine::handle`] is the whole product in miniature: `(index, event) →
//! (index', actions)` with **no I/O, no clocks, no threads** (CLAUDE.md
//! invariant #6). Adapters feed it [`Event`]s and execute the [`Action`]s it
//! returns; every ordering decision is made by vector clocks alone (invariant
//! #7), and conflicts are resolved locally and identically on both replicas
//! with zero negotiation (invariant #5).
//!
//! # Echo suppression lives here (invariant, not adapter courtesy)
//! When the engine tells an adapter to [`Action::Apply`] a change to the tree,
//! that write will make the local watcher fire an event describing exactly the
//! state we just wrote. If that echo were processed, we would re-version and
//! re-ship a change we originated — ping-pong, or worse, resurrect a file we
//! just deleted. The engine therefore journals every write it emits (the
//! [`Expectation`] at that path) and swallows the matching local event when it
//! arrives. Adapters merely avoid watching `.tomo/**` (staging lives there);
//! the *suppression* is the engine's, so the echo-idempotence property holds
//! by construction in pure code (docs/TESTING.md Level 1).

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use crate::clock::{Causality, ReplicaId, VectorClock};
use crate::event::{ChangeKind, LocalChange, RemoteChange};
use crate::index::{ContentSig, Entry, EntryState, Index};
use crate::path::RelPath;

/// What the tree should look like at a path after an [`Action::Apply`].
///
/// This is both the instruction to the adapter and the fingerprint the echo
/// journal matches the resulting watcher event against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expectation {
    /// The file should exist with this exact content signature.
    Present(ContentSig),
    /// The file should not exist (a deletion was applied).
    Absent,
}

/// An input to the engine's transition function.
///
/// Exhaustive by design: adding a variant forces every decision point to be
/// revisited at compile time (per the hygiene skill's guidance).
#[derive(Debug, Clone)]
pub enum Event {
    /// A canonicalized watcher change or a startup-scan diff observed locally.
    Local(LocalChange),
    /// A change received from the peer replica (carries its vector clock).
    Remote(RemoteChange),
    /// The peer's full index, exchanged at connect for reconciliation.
    PeerIndex(Index),
}

/// A side effect the engine asks an adapter to perform.
///
/// The engine never performs these; it only decides them. Every action is a
/// pure function of the current index and the incoming event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Ship this change (with its clock) to the peer. The transport adapter
    /// attaches the content bytes.
    Send(RemoteChange),
    /// Make the local tree match `target` at `path`. Staging plus atomic
    /// rename (invariant #8) is the adapter's job; the engine has already
    /// journaled this expectation for echo suppression.
    Apply {
        /// The path to bring into line.
        path: RelPath,
        /// The desired post-apply state at that path.
        target: Expectation,
    },
    /// Record this version in history. The history adapter no-ops until M3.
    RecordVersion {
        /// The path whose version is being recorded.
        path: RelPath,
        /// The state recorded at this version.
        state: EntryState,
        /// The vector clock of this version.
        version: VectorClock,
    },
    /// A concurrent pair was resolved by last-writer-wins; the loser must be
    /// preserved in history (M4 wires the recording — the engine emits it now
    /// so the decision is captured at the point it is made). Never blocks sync
    /// (invariant #5).
    ConflictResolved {
        /// The contested path.
        path: RelPath,
        /// The state that won the deterministic tiebreak.
        winner: EntryState,
        /// The winner's pre-merge vector clock.
        winner_version: VectorClock,
        /// The state that lost (preserved, never dropped).
        loser: EntryState,
        /// The loser's pre-merge vector clock.
        loser_version: VectorClock,
    },
}

/// The engine's journal of writes it has emitted but not yet seen echoed.
///
/// A multiset keyed by path: emitting an [`Action::Apply`] pushes the target
/// [`Expectation`]; the matching local watcher event retires one copy. A `Vec`
/// per path is the multiset — matching is by equality, retiring removes one
/// occurrence — which keeps the type free of any `Ord` requirement on
/// [`ContentSig`].
#[derive(Debug, Clone, Default)]
struct EchoJournal {
    outstanding: BTreeMap<RelPath, Vec<Expectation>>,
}

impl EchoJournal {
    /// Record that we expect a local event matching `target` at `path`.
    fn journal(&mut self, path: RelPath, target: Expectation) {
        self.outstanding.entry(path).or_default().push(target);
    }

    /// Retire one outstanding expectation matching `observed` at `path`.
    ///
    /// Returns `true` iff a matching expectation was outstanding (the event is
    /// then an echo to be swallowed).
    fn retire(&mut self, path: &RelPath, observed: Expectation) -> bool {
        let Some(list) = self.outstanding.get_mut(path) else {
            return false;
        };
        let Some(pos) = list.iter().position(|e| *e == observed) else {
            return false;
        };
        list.swap_remove(pos);
        if list.is_empty() {
            self.outstanding.remove(path);
        }
        true
    }
}

/// The pure sync state machine for one replica.
///
/// Construct with [`Engine::new`], then drive it with [`Engine::handle`]. The
/// engine owns its [`Index`] and its echo journal; it never touches the outside
/// world.
///
/// ```
/// use tomo_engine::{
///     Action, ChangeKind, ContentHash, ContentSig, Engine, Event, Index,
///     LocalChange, RelPath, ReplicaId,
/// };
///
/// let mut engine = Engine::new(ReplicaId(1), Index::new());
/// let path = RelPath::new("notes.txt").unwrap();
/// let sig = ContentSig { hash: ContentHash([7; 32]), size: 3 };
///
/// let actions = engine.handle(Event::Local(LocalChange {
///     path: path.clone(),
///     kind: ChangeKind::Modified(sig),
/// }));
///
/// // A brand-new local edit is shipped to the peer and recorded in history.
/// assert!(matches!(actions.first(), Some(Action::Send(_))));
/// assert!(engine.index().get(&path).is_some());
/// ```
#[derive(Debug, Clone)]
pub struct Engine {
    replica: ReplicaId,
    index: Index,
    echo: EchoJournal,
}

impl Engine {
    /// Create an engine for `replica` starting from `index` (empty on a fresh
    /// project, or the persisted index on restart).
    pub fn new(replica: ReplicaId, index: Index) -> Self {
        Self {
            replica,
            index,
            echo: EchoJournal::default(),
        }
    }

    /// This engine's replica id.
    pub fn replica(&self) -> ReplicaId {
        self.replica
    }

    /// The engine's authoritative view of the tree.
    pub fn index(&self) -> &Index {
        &self.index
    }

    /// Advance the state machine by one event, returning the side effects to
    /// perform. The engine's index is updated in place; the returned actions
    /// are the adapter's to execute.
    pub fn handle(&mut self, event: Event) -> Vec<Action> {
        match event {
            Event::Local(change) => self.handle_local(change),
            Event::Remote(change) => self.apply_remote(change),
            Event::PeerIndex(remote) => self.handle_peer_index(&remote),
        }
    }

    // ---- Local events -----------------------------------------------------

    fn handle_local(&mut self, change: LocalChange) -> Vec<Action> {
        let LocalChange { path, kind } = change;
        match kind {
            ChangeKind::Modified(sig) => {
                // Echo of a write we just applied: swallow it. The index was
                // already updated when the Apply was emitted.
                if self.echo.retire(&path, Expectation::Present(sig)) {
                    return Vec::new();
                }
                self.local_modified(path, sig)
            }
            ChangeKind::Removed => {
                if self.echo.retire(&path, Expectation::Absent) {
                    return Vec::new();
                }
                self.local_removed(path)
            }
        }
    }

    fn local_modified(&mut self, path: RelPath, sig: ContentSig) -> Vec<Action> {
        let mut version = match self.index.get(&path) {
            Some(entry) => {
                if entry.state == EntryState::Present(sig) {
                    // Spurious event: the content we already index. No-op.
                    return Vec::new();
                }
                entry.version.clone()
            }
            None => VectorClock::new(),
        };
        version.tick(self.replica);
        let state = EntryState::Present(sig);
        self.index.upsert(
            path.clone(),
            Entry {
                version: version.clone(),
                state,
            },
        );
        vec![
            Action::Send(RemoteChange {
                path: path.clone(),
                kind: ChangeKind::Modified(sig),
                version: version.clone(),
            }),
            Action::RecordVersion {
                path,
                state,
                version,
            },
        ]
    }

    fn local_removed(&mut self, path: RelPath) -> Vec<Action> {
        let mut version = match self.index.get(&path) {
            Some(entry) => {
                if entry.state == EntryState::Tombstone {
                    // Already deleted in our index: nothing to propagate.
                    return Vec::new();
                }
                entry.version.clone()
            }
            // Never-seen path removed locally: nothing to delete or ship.
            None => return Vec::new(),
        };
        version.tick(self.replica);
        let state = EntryState::Tombstone;
        self.index.upsert(
            path.clone(),
            Entry {
                version: version.clone(),
                state,
            },
        );
        vec![
            Action::Send(RemoteChange {
                path: path.clone(),
                kind: ChangeKind::Removed,
                version: version.clone(),
            }),
            Action::RecordVersion {
                path,
                state,
                version,
            },
        ]
    }

    // ---- Remote events ----------------------------------------------------

    fn apply_remote(&mut self, change: RemoteChange) -> Vec<Action> {
        let RemoteChange {
            path,
            kind,
            version: remote_version,
        } = change;
        let remote_state = state_from_kind(&kind);
        match self.index.get(&path).cloned() {
            None => self.remote_unseen(path, remote_state, remote_version),
            Some(local) => match local.version.compare(&remote_version) {
                // Local dominates or is identical: the remote is stale or a
                // duplicate. No action, no index change.
                Causality::Equal | Causality::After => Vec::new(),
                Causality::Before => {
                    self.remote_fast_forward(path, &local, remote_state, &remote_version)
                }
                Causality::Concurrent => {
                    self.remote_conflict(path, &local, remote_state, &remote_version)
                }
            },
        }
    }

    fn remote_unseen(
        &mut self,
        path: RelPath,
        state: EntryState,
        version: VectorClock,
    ) -> Vec<Action> {
        let mut actions = Vec::new();
        // Accept the remote version as-is; there is nothing local to merge.
        self.index.upsert(
            path.clone(),
            Entry {
                version: version.clone(),
                state,
            },
        );
        // A tombstone for a path we have never seen means there is no file on
        // disk to delete: record the fact, emit no Apply.
        if let EntryState::Present(sig) = state {
            self.emit_apply(&mut actions, path.clone(), Expectation::Present(sig));
        }
        actions.push(Action::RecordVersion {
            path,
            state,
            version,
        });
        actions
    }

    fn remote_fast_forward(
        &mut self,
        path: RelPath,
        local: &Entry,
        remote_state: EntryState,
        remote_version: &VectorClock,
    ) -> Vec<Action> {
        // `remote` strictly dominates, so the merge equals `remote_version`;
        // computing it as a merge is robust regardless.
        let mut version = local.version.clone();
        version.merge(remote_version);
        let mut actions = Vec::new();
        self.index.upsert(
            path.clone(),
            Entry {
                version: version.clone(),
                state: remote_state,
            },
        );
        // Skip the disk write when the content is already what we have (the
        // same state can arrive via a different causal path); still record the
        // advanced version.
        if local.state != remote_state {
            self.emit_apply(
                &mut actions,
                path.clone(),
                expectation_from_state(remote_state),
            );
        }
        actions.push(Action::RecordVersion {
            path,
            state: remote_state,
            version,
        });
        actions
    }

    fn remote_conflict(
        &mut self,
        path: RelPath,
        local: &Entry,
        remote_state: EntryState,
        remote_version: &VectorClock,
    ) -> Vec<Action> {
        // Both replicas compute the identical merged clock — NO tick — so any
        // later re-exchange of this resolved version compares `Equal` and the
        // conflict does not re-fire (see `clock_merge_is_deterministic`).
        let mut version = local.version.clone();
        version.merge(remote_version);
        let mut actions = Vec::new();

        match resolve_conflict(local.state, remote_state) {
            // Content-identical concurrent writes (equal hashes, or two
            // tombstones): no user-visible conflict. Merge clocks, keep the
            // agreed state, surface nothing.
            ConflictOutcome::NoConflict { state } => {
                self.index.upsert(path.clone(), Entry { version, state });
                // Unreachable when content is truly identical, but keep the
                // tree correct if a caller ever feeds mismatched sizes.
                if local.state != state {
                    self.emit_apply(&mut actions, path, expectation_from_state(state));
                }
                actions
            }
            ConflictOutcome::Conflict { winner_is_local } => {
                let (winner, winner_version, loser, loser_version) = if winner_is_local {
                    (
                        local.state,
                        local.version.clone(),
                        remote_state,
                        remote_version.clone(),
                    )
                } else {
                    (
                        remote_state,
                        remote_version.clone(),
                        local.state,
                        local.version.clone(),
                    )
                };
                self.index.upsert(
                    path.clone(),
                    Entry {
                        version: version.clone(),
                        state: winner,
                    },
                );
                if local.state != winner {
                    self.emit_apply(&mut actions, path.clone(), expectation_from_state(winner));
                }
                actions.push(Action::ConflictResolved {
                    path: path.clone(),
                    winner,
                    winner_version,
                    loser,
                    loser_version,
                });
                actions.push(Action::RecordVersion {
                    path,
                    state: winner,
                    version,
                });
                // Deliberately NO Send: the peer independently resolves to the
                // identical winner (invariant #5, zero negotiation).
                actions
            }
        }
    }

    // ---- Reconciliation ---------------------------------------------------

    fn handle_peer_index(&mut self, remote: &Index) -> Vec<Action> {
        // Deterministic pass over the union of paths, ascending, so both
        // replicas produce comparable action streams.
        let mut paths: BTreeSet<RelPath> = BTreeSet::new();
        for (p, _) in self.index.iter() {
            paths.insert(p.clone());
        }
        for (p, _) in remote.iter() {
            paths.insert(p.clone());
        }

        let mut actions = Vec::new();
        for path in paths {
            if let Some(entry) = remote.get(&path) {
                // The peer has an opinion on this path: run the single-event
                // remote comparison logic against it.
                actions.extend(self.apply_remote(RemoteChange {
                    path,
                    kind: kind_from_state(entry.state),
                    version: entry.version.clone(),
                }));
            } else if let Some(local) = self.index.get(&path) {
                // Local-only path: the peer has never seen it. Ship it.
                actions.push(Action::Send(RemoteChange {
                    path,
                    kind: kind_from_state(local.state),
                    version: local.version.clone(),
                }));
            }
        }
        actions
    }

    // ---- Helpers ----------------------------------------------------------

    /// Journal `target` at `path` (for echo suppression), then queue the Apply.
    fn emit_apply(&mut self, actions: &mut Vec<Action>, path: RelPath, target: Expectation) {
        self.echo.journal(path.clone(), target);
        actions.push(Action::Apply { path, target });
    }
}

/// The state an index entry takes on for a given change kind.
fn state_from_kind(kind: &ChangeKind) -> EntryState {
    match kind {
        ChangeKind::Modified(sig) => EntryState::Present(*sig),
        ChangeKind::Removed => EntryState::Tombstone,
    }
}

/// The change kind that would reproduce a given entry state (for reconciliation
/// and for shipping local entries the peer has never seen).
fn kind_from_state(state: EntryState) -> ChangeKind {
    match state {
        EntryState::Present(sig) => ChangeKind::Modified(sig),
        EntryState::Tombstone => ChangeKind::Removed,
    }
}

/// The tree expectation a state implies (for the echo journal and adapters).
fn expectation_from_state(state: EntryState) -> Expectation {
    match state {
        EntryState::Present(sig) => Expectation::Present(sig),
        EntryState::Tombstone => Expectation::Absent,
    }
}

/// The outcome of resolving a concurrent pair of states.
enum ConflictOutcome {
    /// The two states carry identical content: no user-visible conflict.
    NoConflict { state: EntryState },
    /// A real conflict with a deterministic winner.
    Conflict { winner_is_local: bool },
}

/// Resolve a concurrent `(local, remote)` state pair with a deterministic total
/// order, identical on both replicas (invariant #5).
///
/// - `Present` beats `Tombstone`: an edit survives a concurrent delete, and the
///   delete is preserved in history as the recoverable loser (docs/SPEC.md
///   §5.3, delete-vs-edit).
/// - Two `Present`s: the higher `hash.0` (lexicographic over the 32 bytes)
///   wins.
/// - Equal hashes ⟹ identical content ⟹ nothing to surface; two tombstones are
///   likewise content-identical.
///
/// # On the replica-id tiebreak
/// SPEC §5.3 lists the tiebreak as "content hash, then replica id". With a
/// *strict* hash order, equal hashes mean identical content, which is not a
/// user-visible conflict at all — so the state selection never reaches a
/// replica-id decision. The replica-id tiebreak is therefore unreachable for
/// choosing the winning *state*; it is kept in the spec for completeness (and
/// would matter only to a design that distinguished equal-hash writes, which
/// this one does not).
fn resolve_conflict(local: EntryState, remote: EntryState) -> ConflictOutcome {
    match (local, remote) {
        (EntryState::Present(a), EntryState::Present(b)) => match a.hash.0.cmp(&b.hash.0) {
            Ordering::Greater => ConflictOutcome::Conflict {
                winner_is_local: true,
            },
            Ordering::Less => ConflictOutcome::Conflict {
                winner_is_local: false,
            },
            // Identical content: keep local (equals remote by hash).
            Ordering::Equal => ConflictOutcome::NoConflict {
                state: EntryState::Present(a),
            },
        },
        (EntryState::Present(_), EntryState::Tombstone) => ConflictOutcome::Conflict {
            winner_is_local: true,
        },
        (EntryState::Tombstone, EntryState::Present(_)) => ConflictOutcome::Conflict {
            winner_is_local: false,
        },
        (EntryState::Tombstone, EntryState::Tombstone) => ConflictOutcome::NoConflict {
            state: EntryState::Tombstone,
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // fine in tests
mod tests {
    use super::*;
    use crate::index::ContentHash;
    use proptest::prelude::*;

    const A: ReplicaId = ReplicaId(1);
    const B: ReplicaId = ReplicaId(2);

    // Size is tied to the hash byte so that equal hashes always imply equal
    // signatures — the property `resolve_conflict` relies on for convergence.
    fn sig(byte: u8) -> ContentSig {
        ContentSig {
            hash: ContentHash([byte; 32]),
            size: u64::from(byte),
        }
    }

    fn modified(path: &str, byte: u8) -> Event {
        Event::Local(LocalChange {
            path: RelPath::new(path).unwrap(),
            kind: ChangeKind::Modified(sig(byte)),
        })
    }

    fn removed(path: &str) -> Event {
        Event::Local(LocalChange {
            path: RelPath::new(path).unwrap(),
            kind: ChangeKind::Removed,
        })
    }

    fn rp(path: &str) -> RelPath {
        RelPath::new(path).unwrap()
    }

    fn engine(replica: ReplicaId) -> Engine {
        Engine::new(replica, Index::new())
    }

    // ---- Local events -----------------------------------------------------

    #[test]
    fn local_modified_new_file_sends_and_records() {
        let mut e = engine(A);
        let actions = e.handle(modified("a.txt", 1));
        assert_eq!(actions.len(), 2);
        match &actions[0] {
            Action::Send(rc) => {
                assert_eq!(rc.path, rp("a.txt"));
                assert_eq!(rc.kind, ChangeKind::Modified(sig(1)));
                assert_eq!(rc.version.get(A), 1);
            }
            other => panic!("expected Send, got {other:?}"),
        }
        assert!(matches!(&actions[1], Action::RecordVersion { .. }));
        let entry = e.index().get(&rp("a.txt")).unwrap();
        assert_eq!(entry.state, EntryState::Present(sig(1)));
        assert_eq!(entry.version.get(A), 1);
    }

    #[test]
    fn local_modified_same_content_is_spurious() {
        let mut e = engine(A);
        e.handle(modified("a.txt", 1));
        let actions = e.handle(modified("a.txt", 1));
        assert!(actions.is_empty());
        // Clock did not advance a second time.
        assert_eq!(e.index().get(&rp("a.txt")).unwrap().version.get(A), 1);
    }

    #[test]
    fn local_modified_existing_ticks_clock() {
        let mut e = engine(A);
        e.handle(modified("a.txt", 1));
        e.handle(modified("a.txt", 2));
        let entry = e.index().get(&rp("a.txt")).unwrap();
        assert_eq!(entry.state, EntryState::Present(sig(2)));
        assert_eq!(entry.version.get(A), 2);
    }

    #[test]
    fn local_removed_present_sends_tombstone() {
        let mut e = engine(A);
        e.handle(modified("a.txt", 1));
        let actions = e.handle(removed("a.txt"));
        assert_eq!(actions.len(), 2);
        assert!(matches!(
            &actions[0],
            Action::Send(rc) if rc.kind == ChangeKind::Removed
        ));
        assert_eq!(
            e.index().get(&rp("a.txt")).unwrap().state,
            EntryState::Tombstone
        );
    }

    #[test]
    fn local_removed_unknown_is_noop() {
        let mut e = engine(A);
        assert!(e.handle(removed("ghost.txt")).is_empty());
        assert!(e.index().get(&rp("ghost.txt")).is_none());
    }

    #[test]
    fn local_removed_already_tombstoned_is_noop() {
        let mut e = engine(A);
        e.handle(modified("a.txt", 1));
        e.handle(removed("a.txt"));
        assert!(e.handle(removed("a.txt")).is_empty());
    }

    // ---- Echo suppression -------------------------------------------------

    #[test]
    fn apply_echo_is_suppressed_and_leaves_index_unchanged() {
        // A remote change makes us Apply Present(sig). The watcher then fires a
        // Local(Modified(sig)) echo, which must be swallowed.
        let mut e = engine(A);
        let mut v = VectorClock::new();
        v.tick(B);
        let apply = e
            .handle(Event::Remote(RemoteChange {
                path: rp("a.txt"),
                kind: ChangeKind::Modified(sig(9)),
                version: v,
            }))
            .into_iter()
            .find(|a| matches!(a, Action::Apply { .. }))
            .expect("remote to unseen path applies");
        let Action::Apply { target, .. } = apply else {
            unreachable!()
        };
        assert_eq!(target, Expectation::Present(sig(9)));

        let before = e.index().canonical_bytes();
        let echo = e.handle(modified("a.txt", 9));
        assert!(echo.is_empty(), "echo must produce no actions");
        assert_eq!(before, e.index().canonical_bytes(), "index must not change");
    }

    #[test]
    fn removal_echo_is_suppressed() {
        let mut e = engine(A);
        e.handle(modified("a.txt", 1)); // clock {A:1}
        let mut v = VectorClock::new();
        v.tick(A);
        v.tick(A); // {A:2} strictly dominates: fast-forward delete
        let actions = e.handle(Event::Remote(RemoteChange {
            path: rp("a.txt"),
            kind: ChangeKind::Removed,
            version: v,
        }));
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::Apply {
                target: Expectation::Absent,
                ..
            }
        )));
        // The delete echo.
        assert!(e.handle(removed("a.txt")).is_empty());
    }

    #[test]
    fn non_matching_local_after_apply_is_processed() {
        // An Apply journals Present(sig9); a genuinely different local edit
        // (sig8) at the same path is NOT an echo and must be processed.
        let mut e = engine(A);
        let mut v = VectorClock::new();
        v.tick(B);
        e.handle(Event::Remote(RemoteChange {
            path: rp("a.txt"),
            kind: ChangeKind::Modified(sig(9)),
            version: v,
        }));
        let actions = e.handle(modified("a.txt", 8));
        assert!(!actions.is_empty(), "distinct local edit must propagate");
        assert_eq!(
            e.index().get(&rp("a.txt")).unwrap().state,
            EntryState::Present(sig(8))
        );
    }

    // ---- Remote events ----------------------------------------------------

    #[test]
    fn remote_unseen_present_applies_and_records() {
        let mut e = engine(A);
        let mut v = VectorClock::new();
        v.tick(B);
        let actions = e.handle(Event::Remote(RemoteChange {
            path: rp("a.txt"),
            kind: ChangeKind::Modified(sig(5)),
            version: v.clone(),
        }));
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::Apply {
                target: Expectation::Present(_),
                ..
            }
        )));
        assert!(actions
            .iter()
            .any(|a| matches!(a, Action::RecordVersion { .. })));
        assert!(!actions.iter().any(|a| matches!(a, Action::Send(_))));
        let entry = e.index().get(&rp("a.txt")).unwrap();
        assert_eq!(entry.version, v);
        assert_eq!(entry.state, EntryState::Present(sig(5)));
    }

    #[test]
    fn remote_removed_for_unseen_path_records_tombstone_without_apply() {
        let mut e = engine(A);
        let mut v = VectorClock::new();
        v.tick(B);
        let actions = e.handle(Event::Remote(RemoteChange {
            path: rp("a.txt"),
            kind: ChangeKind::Removed,
            version: v,
        }));
        assert!(!actions.iter().any(|a| matches!(a, Action::Apply { .. })));
        assert!(actions
            .iter()
            .any(|a| matches!(a, Action::RecordVersion { .. })));
        assert_eq!(
            e.index().get(&rp("a.txt")).unwrap().state,
            EntryState::Tombstone
        );
    }

    #[test]
    fn remote_stale_change_is_ignored() {
        let mut e = engine(A);
        e.handle(modified("a.txt", 2)); // {A:1}
                                        // A remote change with an older/empty-ish clock the local dominates.
        let stale = RemoteChange {
            path: rp("a.txt"),
            kind: ChangeKind::Modified(sig(3)),
            version: VectorClock::new(),
        };
        let before = e.index().canonical_bytes();
        assert!(e.handle(Event::Remote(stale)).is_empty());
        assert_eq!(before, e.index().canonical_bytes());
    }

    #[test]
    fn remote_before_fast_forwards() {
        let mut e = engine(A);
        e.handle(modified("a.txt", 1)); // {A:1}
        let mut v = VectorClock::new();
        v.tick(A);
        v.tick(A); // {A:2}: strictly dominates
        let actions = e.handle(Event::Remote(RemoteChange {
            path: rp("a.txt"),
            kind: ChangeKind::Modified(sig(4)),
            version: v.clone(),
        }));
        assert!(actions.iter().any(
            |a| matches!(a, Action::Apply { target: Expectation::Present(s), .. } if *s == sig(4))
        ));
        assert_eq!(e.index().get(&rp("a.txt")).unwrap().version, v);
    }

    #[test]
    fn remote_before_same_state_skips_apply() {
        let mut e = engine(A);
        e.handle(modified("a.txt", 1)); // {A:1}, content sig(1)
        let mut v = VectorClock::new();
        v.tick(A);
        v.tick(A); // {A:2}, same content sig(1)
        let actions = e.handle(Event::Remote(RemoteChange {
            path: rp("a.txt"),
            kind: ChangeKind::Modified(sig(1)),
            version: v.clone(),
        }));
        assert!(!actions.iter().any(|a| matches!(a, Action::Apply { .. })));
        assert!(actions
            .iter()
            .any(|a| matches!(a, Action::RecordVersion { .. })));
        assert_eq!(e.index().get(&rp("a.txt")).unwrap().version, v);
    }

    // ---- Conflicts --------------------------------------------------------

    /// Build a pair of engines each holding a concurrent Present at `path`.
    fn concurrent_pair(a_byte: u8, b_byte: u8) -> (Engine, Engine, RemoteChange, RemoteChange) {
        let mut a = engine(A);
        let mut b = engine(B);
        let a_send = one_send(a.handle(modified("f", a_byte)));
        let b_send = one_send(b.handle(modified("f", b_byte)));
        (a, b, a_send, b_send)
    }

    fn one_send(actions: Vec<Action>) -> RemoteChange {
        actions
            .into_iter()
            .find_map(|a| match a {
                Action::Send(rc) => Some(rc),
                _ => None,
            })
            .expect("a Send was emitted")
    }

    #[test]
    fn conflict_present_present_higher_hash_wins_both_orders() {
        // a_byte=9 (higher hash) beats b_byte=3 on both replicas.
        let (mut a, mut b, a_send, b_send) = concurrent_pair(9, 3);
        let a_actions = a.handle(Event::Remote(b_send));
        let b_actions = b.handle(Event::Remote(a_send));
        // Both converge to content sig(9).
        assert_eq!(
            a.index().get(&rp("f")).unwrap().state,
            EntryState::Present(sig(9))
        );
        assert_eq!(
            b.index().get(&rp("f")).unwrap().state,
            EntryState::Present(sig(9))
        );
        assert_eq!(a.index().canonical_bytes(), b.index().canonical_bytes());
        // The winner (A, holding sig9) applies nothing new; the loser (B) applies sig9.
        assert!(!a_actions.iter().any(|x| matches!(x, Action::Apply { .. })));
        assert!(b_actions.iter().any(
            |x| matches!(x, Action::Apply { target: Expectation::Present(s), .. } if *s == sig(9))
        ));
        // Both surface a ConflictResolved; neither re-sends.
        assert!(a_actions
            .iter()
            .any(|x| matches!(x, Action::ConflictResolved { .. })));
        assert!(b_actions
            .iter()
            .any(|x| matches!(x, Action::ConflictResolved { .. })));
        assert!(!a_actions.iter().any(|x| matches!(x, Action::Send(_))));
        assert!(!b_actions.iter().any(|x| matches!(x, Action::Send(_))));
    }

    #[test]
    fn conflict_delete_vs_edit_edit_wins() {
        // A edits f; B deletes f; concurrent. Present must beat Tombstone.
        let mut a = engine(A);
        let mut b = engine(B);
        let a_send = one_send(a.handle(modified("f", 7)));
        // Give B a copy first so it has something to delete, concurrently.
        b.handle(Event::Remote(RemoteChange {
            path: rp("f"),
            kind: ChangeKind::Modified(sig(1)),
            version: {
                let mut v = VectorClock::new();
                v.tick(B);
                v
            },
        }));
        let b_send = one_send(b.handle(removed("f")));
        let a_actions = a.handle(Event::Remote(b_send));
        b.handle(Event::Remote(a_send));
        // Edit survives on both.
        assert_eq!(
            a.index().get(&rp("f")).unwrap().state,
            EntryState::Present(sig(7))
        );
        assert_eq!(
            b.index().get(&rp("f")).unwrap().state,
            EntryState::Present(sig(7))
        );
        // A (the editor) surfaces the resolved conflict with the delete as loser.
        let resolved = a_actions
            .iter()
            .find_map(|x| match x {
                Action::ConflictResolved { winner, loser, .. } => Some((*winner, *loser)),
                _ => None,
            })
            .expect("conflict surfaced");
        assert_eq!(resolved.0, EntryState::Present(sig(7)));
        assert_eq!(resolved.1, EntryState::Tombstone);
    }

    #[test]
    fn concurrent_identical_content_is_not_a_conflict() {
        let (mut a, _b, _a_send, b_send) = concurrent_pair(5, 5);
        let actions = a.handle(Event::Remote(b_send));
        assert!(!actions
            .iter()
            .any(|x| matches!(x, Action::ConflictResolved { .. })));
        assert!(!actions.iter().any(|x| matches!(x, Action::Apply { .. })));
        assert_eq!(
            a.index().get(&rp("f")).unwrap().state,
            EntryState::Present(sig(5))
        );
    }

    #[test]
    fn concurrent_tombstones_merge_without_conflict() {
        // Both delete the same (independently-seen) file concurrently.
        let mut a = engine(A);
        let mut b = engine(B);
        for (e, r) in [(&mut a, A), (&mut b, B)] {
            e.handle(Event::Remote(RemoteChange {
                path: rp("f"),
                kind: ChangeKind::Modified(sig(1)),
                version: {
                    let mut v = VectorClock::new();
                    v.tick(r);
                    v
                },
            }));
        }
        let a_send = one_send(a.handle(removed("f")));
        let b_send = one_send(b.handle(removed("f")));
        let a_actions = a.handle(Event::Remote(b_send));
        b.handle(Event::Remote(a_send));
        assert!(!a_actions
            .iter()
            .any(|x| matches!(x, Action::ConflictResolved { .. })));
        assert_eq!(a.index().canonical_bytes(), b.index().canonical_bytes());
    }

    // ---- Reconciliation ---------------------------------------------------

    #[test]
    fn peer_index_ships_local_only_paths() {
        let mut a = engine(A);
        a.handle(modified("only_a.txt", 1));
        let actions = a.handle(Event::PeerIndex(Index::new()));
        let sends: Vec<_> = actions
            .iter()
            .filter_map(|x| match x {
                Action::Send(rc) => Some(rc.path.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(sends, vec![rp("only_a.txt")]);
    }

    #[test]
    fn peer_index_applies_unseen_peer_paths() {
        let mut a = engine(A);
        let mut peer = Index::new();
        let mut v = VectorClock::new();
        v.tick(B);
        peer.upsert(
            rp("from_b.txt"),
            Entry {
                version: v,
                state: EntryState::Present(sig(2)),
            },
        );
        let actions = a.handle(Event::PeerIndex(peer));
        assert!(actions.iter().any(|x| matches!(
            x,
            Action::Apply {
                target: Expectation::Present(_),
                ..
            }
        )));
        assert!(a.index().get(&rp("from_b.txt")).is_some());
    }

    // ---- KNOWN LIMITATION (flagged to the lead) ---------------------------

    /// Characterizes a genuine convergence hole in the specified single-`Entry`
    /// "merge-on-conflict + hash-LWW" model: when one replica produces two
    /// causally-sequential concurrent-lineage versions and BOTH are delivered
    /// to the peer (intermediate not coalesced) around a concurrent edit, the
    /// two replicas can pick different winners for the SAME merged clock.
    ///
    /// Root cause: content hash is not monotonic along a lineage, and merging a
    /// loser's clock into the winner makes a later same-lineage write look
    /// `Concurrent` instead of fast-forwarding. A proper fix is a multi-value
    /// register (track the set of causal heads, argmax hash over the set); that
    /// is out of scope for M1's single-entry model.
    ///
    /// In practice the live sync path ships the latest bytes (invariant #3) and
    /// the transport coalesces intermediates, so this reordering does not occur
    /// — the convergence property test models that realistic delivery. This
    /// test pins the buggy behavior so a future MVR fix is noticed here.
    #[test]
    fn known_limitation_intermediate_reorder_can_diverge() {
        // A: a1 (hash 9), then a2 (hash 7), both before absorbing B's edit.
        let mut a = engine(A);
        let a1 = one_send(a.handle(modified("f", 9))); // {A:1}
        let a2 = one_send(a.handle(modified("f", 7))); // {A:2}

        // B: b1 (hash 5).
        let mut b = engine(B);
        let b1 = one_send(b.handle(modified("f", 5))); // {B:1}

        // A receives only b1 (its state is a2): conflict a2(7) vs b1(5) -> a2.
        a.handle(Event::Remote(b1));

        // B receives a1 then a2 (intermediate NOT coalesced):
        b.handle(Event::Remote(a1)); // conflict a1(9) vs b1(5) -> a1
        b.handle(Event::Remote(a2)); // a2(7) vs a1(9) (now concurrent) -> a1

        // Clocks match, but content diverges: THIS IS THE BUG.
        assert_eq!(
            a.index().get(&rp("f")).unwrap().version,
            b.index().get(&rp("f")).unwrap().version
        );
        assert_ne!(a.index().canonical_bytes(), b.index().canonical_bytes());
    }

    // ---- Property tests ---------------------------------------------------

    /// A pure two-replica simulator.
    ///
    /// Local edits produce `Send`s that are queued per replica outbox. A `Sync`
    /// delivers, for each path, only the LATEST queued change from each side —
    /// modeling the real transport, which ships current file bytes (invariant
    /// #3) and coalesces superseded intermediates. Under this realistic
    /// delivery the single-entry merge model converges.
    struct Sim {
        a: Engine,
        b: Engine,
        out_a: Vec<RemoteChange>, // changes A wants to send to B
        out_b: Vec<RemoteChange>, // changes B wants to send to A
    }

    #[derive(Debug, Clone)]
    enum Op {
        Edit { on_a: bool, path: u8, byte: u8 },
        Delete { on_a: bool, path: u8 },
        Sync,
    }

    impl Sim {
        fn new() -> Self {
            Self {
                a: engine(A),
                b: engine(B),
                out_a: Vec::new(),
                out_b: Vec::new(),
            }
        }

        fn edit(&mut self, on_a: bool, path: &str, byte: u8) {
            let ev = Event::Local(LocalChange {
                path: rp(path),
                kind: ChangeKind::Modified(sig(byte)),
            });
            let (engine, out) = if on_a {
                (&mut self.a, &mut self.out_a)
            } else {
                (&mut self.b, &mut self.out_b)
            };
            for act in engine.handle(ev) {
                if let Action::Send(rc) = act {
                    out.push(rc);
                }
            }
        }

        fn delete(&mut self, on_a: bool, path: &str) {
            let ev = Event::Local(LocalChange {
                path: rp(path),
                kind: ChangeKind::Removed,
            });
            let (engine, out) = if on_a {
                (&mut self.a, &mut self.out_a)
            } else {
                (&mut self.b, &mut self.out_b)
            };
            for act in engine.handle(ev) {
                if let Action::Send(rc) = act {
                    out.push(rc);
                }
            }
        }

        /// Coalesce an outbox to the latest change per path (later local edits
        /// tick a dominating clock, so last-per-path is the maximal one).
        fn coalesce(out: &mut Vec<RemoteChange>) -> Vec<RemoteChange> {
            let mut latest: BTreeMap<RelPath, RemoteChange> = BTreeMap::new();
            for rc in out.drain(..) {
                latest.insert(rc.path.clone(), rc);
            }
            latest.into_values().collect()
        }

        fn sync(&mut self) {
            let to_b = Self::coalesce(&mut self.out_a);
            let to_a = Self::coalesce(&mut self.out_b);
            // Remote handling never emits `Send`, so no cascade to re-queue.
            for rc in to_b {
                self.b.handle(Event::Remote(rc));
            }
            for rc in to_a {
                self.a.handle(Event::Remote(rc));
            }
        }

        fn run(&mut self, ops: &[Op]) {
            for op in ops {
                match op {
                    Op::Edit { on_a, path, byte } => {
                        self.edit(*on_a, &format!("p{path}"), *byte);
                    }
                    Op::Delete { on_a, path } => {
                        self.delete(*on_a, &format!("p{path}"));
                    }
                    Op::Sync => self.sync(),
                }
            }
            // Drain to quiescence: sync until both outboxes are empty.
            for _ in 0..4 {
                self.sync();
            }
        }
    }

    fn arb_op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (any::<bool>(), 0u8..3, 0u8..6).prop_map(|(on_a, path, byte)| Op::Edit {
                on_a,
                path,
                byte
            }),
            (any::<bool>(), 0u8..3).prop_map(|(on_a, path)| Op::Delete { on_a, path }),
            Just(Op::Sync),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(400))]

        /// Convergence: for any interleaving of edits/deletes/syncs, both
        /// replicas reach identical index roots (docs/TESTING.md Level 1).
        #[test]
        fn convergence(ops in proptest::collection::vec(arb_op(), 0..40)) {
            let mut sim = Sim::new();
            sim.run(&ops);
            prop_assert_eq!(
                sim.a.index().canonical_bytes(),
                sim.b.index().canonical_bytes()
            );
        }

        /// After convergence, exchanging full indices yields NO `Send` and NO
        /// `Apply` (quiet-network invariant, pure version).
        #[test]
        fn no_spurious_traffic_after_convergence(
            ops in proptest::collection::vec(arb_op(), 0..40)
        ) {
            let mut sim = Sim::new();
            sim.run(&ops);
            let a_idx = sim.a.index().clone();
            let b_idx = sim.b.index().clone();
            let a_actions = sim.a.handle(Event::PeerIndex(b_idx));
            let b_actions = sim.b.handle(Event::PeerIndex(a_idx));
            for act in a_actions.iter().chain(b_actions.iter()) {
                prop_assert!(
                    !matches!(act, Action::Send(_) | Action::Apply { .. }),
                    "unexpected action after convergence: {:?}",
                    act
                );
            }
        }

        /// Echo idempotence: any emitted `Apply` fed back as its watcher echo
        /// produces zero actions and leaves the index bytes unchanged. Covers
        /// both a Present apply and an Absent (delete) apply.
        #[test]
        fn echo_idempotence(byte in any::<u8>(), del in any::<bool>()) {
            let mut e = engine(A);
            // The local change that echoes the Apply we are about to provoke.
            let echo_kind = if del {
                // Seed a file, then a dominating remote delete -> Apply(Absent).
                e.handle(modified("f", byte));
                let mut v = VectorClock::new();
                v.tick(A);
                v.tick(A); // dominates local {A:1}
                let actions = e.handle(Event::Remote(RemoteChange {
                    path: rp("f"),
                    kind: ChangeKind::Removed,
                    version: v,
                }));
                let applied_absent = actions
                    .iter()
                    .any(|a| matches!(a, Action::Apply { target: Expectation::Absent, .. }));
                prop_assert!(applied_absent);
                ChangeKind::Removed
            } else {
                // Remote modify to an unseen path -> Apply(Present).
                let mut v = VectorClock::new();
                v.tick(B);
                let actions = e.handle(Event::Remote(RemoteChange {
                    path: rp("f"),
                    kind: ChangeKind::Modified(sig(byte)),
                    version: v,
                }));
                let applied = actions.iter().any(|a| matches!(a, Action::Apply { .. }));
                prop_assert!(applied);
                ChangeKind::Modified(sig(byte))
            };

            let before = e.index().canonical_bytes();
            // Feed back the corresponding watcher echo.
            let back = e.handle(Event::Local(LocalChange {
                path: rp("f"),
                kind: echo_kind,
            }));
            prop_assert!(back.is_empty(), "echo produced actions: {:?}", back);
            prop_assert_eq!(before, e.index().canonical_bytes());
        }

        /// Deterministic conflict winner: for an arbitrary concurrent pair,
        /// both replicas end with identical entries (state AND clock),
        /// regardless of which side each processed first.
        #[test]
        fn deterministic_conflict_winner(
            a_byte in 0u8..12,
            b_byte in 0u8..12,
            a_present in any::<bool>(),
            b_present in any::<bool>(),
        ) {
            // Build genuinely concurrent entries on A and B for path "f".
            let (mut a, mut b, a_send, b_send) =
                seed_concurrent(a_byte, b_byte, a_present, b_present);
            a.handle(Event::Remote(b_send));
            b.handle(Event::Remote(a_send));
            prop_assert_eq!(
                a.index().canonical_bytes(),
                b.index().canonical_bytes()
            );
        }

        /// Clock-merge determinism: after both resolve the same concurrent
        /// pair, re-exchanging the resolved change compares `Equal` (no
        /// re-fire) and nothing further happens.
        #[test]
        fn clock_merge_is_deterministic(a_byte in 0u8..12, b_byte in 0u8..12) {
            let (mut a, mut b, a_send, b_send) = concurrent_pair(a_byte, b_byte);
            a.handle(Event::Remote(b_send));
            b.handle(Event::Remote(a_send));
            // Resolved clocks are identical.
            let a_entry = a.index().get(&rp("f")).unwrap().clone();
            let b_entry = b.index().get(&rp("f")).unwrap().clone();
            prop_assert_eq!(a_entry.version.compare(&b_entry.version), Causality::Equal);
            // Re-send each resolved winner to the other: Equal -> no action.
            let re_a = a.handle(Event::Remote(RemoteChange {
                path: rp("f"),
                kind: kind_from_state(b_entry.state),
                version: b_entry.version,
            }));
            prop_assert!(re_a.is_empty());
        }
    }

    /// Seed two engines with concurrent entries at "f", each optionally a
    /// tombstone, returning the change each wants to send.
    fn seed_concurrent(
        a_byte: u8,
        b_byte: u8,
        a_present: bool,
        b_present: bool,
    ) -> (Engine, Engine, RemoteChange, RemoteChange) {
        let mut a = engine(A);
        let mut b = engine(B);
        let a_send = if a_present {
            one_send(a.handle(modified("f", a_byte)))
        } else {
            // Seed then delete so A holds a concurrent tombstone.
            a.handle(modified("f", a_byte));
            one_send(a.handle(removed("f")))
        };
        let b_send = if b_present {
            one_send(b.handle(modified("f", b_byte)))
        } else {
            b.handle(modified("f", b_byte));
            one_send(b.handle(removed("f")))
        };
        (a, b, a_send, b_send)
    }
}
