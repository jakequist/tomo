//! The sync engine transition function: the pure core of Tomo.
//!
//! [`Engine::handle`] is the whole product in miniature: `(index, event) →
//! (index', actions)` with **no I/O, no clocks, no threads** (CLAUDE.md
//! invariant #6). Adapters feed it [`Event`]s and execute the [`Action`]s it
//! returns; every ordering decision is made by vector clocks alone (invariant
//! #7), and conflicts are resolved locally and identically on both replicas
//! with zero negotiation (invariant #5).
//!
//! # Multi-value convergence
//! Each path's [`Entry`] is a set of concurrent [`crate::index::Head`]s (a
//! Dynamo-sibling register). Applying a change is [`Entry::absorb`] — a proper
//! join-semilattice operation (union of version-tagged states, discard
//! causally-dominated ones) — so replicas converge to byte-identical indices
//! under *arbitrary* delivery order, including reordered and superseded
//! intermediate versions. The engine materializes [`Entry::winner`] to decide
//! what belongs on disk; the winner is a deterministic pure function of the
//! head set, so both replicas agree without negotiation.
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

use std::collections::{BTreeMap, BTreeSet};

use crate::clock::{Causality, ReplicaId, VectorClock};
use crate::event::{ChangeKind, LocalChange, RemoteChange};
use crate::index::{AbsorbOutcome, ContentSig, Entry, EntryState, Index};
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
        /// The winner head's vector clock.
        winner_version: VectorClock,
        /// The state that lost (preserved as a sibling head, never dropped).
        loser: EntryState,
        /// The loser head's vector clock.
        loser_version: VectorClock,
        /// Whether the resolution used the genesis *adoption* rule (the entry
        /// was in [`Entry::adoption_mode`], so the newer-mtime copy was adopted
        /// rather than the standard hash tiebreak). Purely for how the CLI words
        /// the non-blocking note; the winner itself is fully determined by the
        /// engine either way.
        adopted: bool,
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

    /// Whether an outstanding expectation matching `observed` is journaled at
    /// `path`, **without** retiring it (read-only).
    fn contains(&self, path: &RelPath, observed: &Expectation) -> bool {
        self.outstanding
            .get(path)
            .is_some_and(|list| list.iter().any(|e| e == observed))
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
/// let sig = ContentSig { hash: ContentHash([7; 32]), size: 3, exec: false, mtime_ms: 0 };
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

    /// Whether the echo journal currently holds an outstanding expectation for
    /// `path` matching `observed` — i.e. the engine emitted an
    /// [`Action::Apply`] with this target that has not yet been retired by its
    /// echoing local watcher event. Read-only: it does **not** retire anything.
    ///
    /// The apply adapter uses this to distinguish disk that reflects the
    /// engine's own pending write (an echo — safe to overwrite) from disk that
    /// holds an unobserved concurrent local edit (must be preserved, invariant
    /// #5 — nothing is lost).
    pub fn is_expected_echo(&self, path: &RelPath, observed: &Expectation) -> bool {
        self.echo.contains(path, observed)
    }

    /// Advance the state machine by one event, returning the side effects to
    /// perform. The engine's index is updated in place; the returned actions
    /// are the adapter's to execute.
    pub fn handle(&mut self, event: Event) -> Vec<Action> {
        match event {
            Event::Local(change) => self.handle_local(change),
            Event::Remote(change) => self.handle_remote(change),
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

    /// A local edit collapses any head set to a single head stamped with the
    /// merged clock ticked once — this keeps a replica's per-path versions
    /// totally ordered (and the head set bounded by the replica count).
    fn local_modified(&mut self, path: RelPath, sig: ContentSig) -> Vec<Action> {
        let mut version = match self.index.get(&path) {
            Some(entry) => {
                // Spurious watcher event: disk (the winner) already shows this.
                if entry.winner().state == EntryState::Present(sig) {
                    return Vec::new();
                }
                entry.merged_clock()
            }
            None => VectorClock::new(),
        };
        version.tick(self.replica);
        let state = EntryState::Present(sig);
        self.index
            .upsert(path.clone(), Entry::single(version.clone(), state));
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
                // Already deleted on disk (winner is a tombstone): nothing to do.
                if entry.winner().state == EntryState::Tombstone {
                    return Vec::new();
                }
                entry.merged_clock()
            }
            // Never-seen path removed locally: nothing to delete or ship.
            None => return Vec::new(),
        };
        version.tick(self.replica);
        let state = EntryState::Tombstone;
        self.index
            .upsert(path.clone(), Entry::single(version.clone(), state));
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

    fn handle_remote(&mut self, change: RemoteChange) -> Vec<Action> {
        let RemoteChange {
            path,
            kind,
            version,
        } = change;
        self.absorb_remote(path, state_from_kind(&kind), version)
    }

    /// The shared core for a single incoming `(path, state, version)` — used by
    /// both [`Event::Remote`] and per-head [`Event::PeerIndex`] reconciliation.
    /// Never emits `Send`: the peer converges independently (invariant #5).
    fn absorb_remote(
        &mut self,
        path: RelPath,
        state: EntryState,
        version: VectorClock,
    ) -> Vec<Action> {
        let Some(mut entry) = self.index.get(&path).cloned() else {
            return self.remote_unseen(path, state, version);
        };
        match entry.absorb(version.clone(), state) {
            AbsorbOutcome::AlreadyKnown => Vec::new(),
            AbsorbOutcome::Absorbed {
                winner_changed,
                new_conflict,
                novel_content,
            } => {
                let mut actions = Vec::new();
                let winner = entry.winner().clone();
                // Bring disk into line only when the materialized winner moved.
                if winner_changed {
                    self.emit_apply(
                        &mut actions,
                        path.clone(),
                        expectation_from_state(winner.state),
                    );
                }
                // Surface a freshly user-visible conflict, preserving each loser.
                if new_conflict {
                    let adopted = entry.adoption_mode();
                    for head in entry.heads() {
                        if *head != winner {
                            actions.push(Action::ConflictResolved {
                                path: path.clone(),
                                winner: winner.state,
                                winner_version: winner.version.clone(),
                                loser: head.state,
                                loser_version: head.version.clone(),
                                adopted,
                            });
                        }
                    }
                }
                // Record only genuinely new content (identical-content merges
                // add no version — nothing to store).
                if novel_content {
                    actions.push(Action::RecordVersion {
                        path: path.clone(),
                        state,
                        version,
                    });
                }
                self.index.upsert(path, entry);
                actions
            }
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
        self.index
            .upsert(path.clone(), Entry::single(version.clone(), state));
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
            if let Some(remote_entry) = remote.get(&path) {
                // Absorb every remote head (heads in canonical order).
                for head in remote_entry.heads() {
                    actions.extend(self.absorb_remote(
                        path.clone(),
                        head.state,
                        head.version.clone(),
                    ));
                }
                // Ship any local head the peer's head set does not cover.
                if let Some(local_entry) = self.index.get(&path) {
                    for local_head in local_entry.heads() {
                        let covered = remote_entry.heads().iter().any(|remote_head| {
                            matches!(
                                local_head.version.compare(&remote_head.version),
                                Causality::Before | Causality::Equal
                            )
                        });
                        if !covered {
                            actions.push(Action::Send(RemoteChange {
                                path: path.clone(),
                                kind: kind_from_state(local_head.state),
                                version: local_head.version.clone(),
                            }));
                        }
                    }
                }
            } else if let Some(local_entry) = self.index.get(&path) {
                // Local-only path: the peer has never seen it. Ship every head.
                for local_head in local_entry.heads() {
                    actions.push(Action::Send(RemoteChange {
                        path: path.clone(),
                        kind: kind_from_state(local_head.state),
                        version: local_head.version.clone(),
                    }));
                }
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

/// The state an index head takes on for a given change kind.
fn state_from_kind(kind: &ChangeKind) -> EntryState {
    match kind {
        ChangeKind::Modified(sig) => EntryState::Present(*sig),
        ChangeKind::Removed => EntryState::Tombstone,
    }
}

/// The change kind that would reproduce a given head state (for reconciliation
/// and for shipping local heads the peer has never seen).
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // fine in tests
mod tests {
    use super::*;
    use crate::index::ContentHash;
    use proptest::prelude::*;

    const A: ReplicaId = ReplicaId(1);
    const B: ReplicaId = ReplicaId(2);

    // Size is tied to the hash byte so equal hashes always imply equal
    // signatures — matching the "equal hash ⇒ identical content" contract.
    fn sig(byte: u8) -> ContentSig {
        ContentSig {
            hash: ContentHash([byte; 32]),
            size: u64::from(byte),
            exec: false,
            mtime_ms: 0,
        }
    }

    /// The same content as [`sig`] but marked executable — for the chmod-only
    /// change and same-content-different-exec conflict tests.
    fn sig_x(byte: u8) -> ContentSig {
        ContentSig {
            exec: true,
            ..sig(byte)
        }
    }

    /// The same content as [`sig`] but stamped with a specific mtime (carried
    /// metadata) — for the genesis adoption tests.
    fn sig_t(byte: u8, mtime_ms: u64) -> ContentSig {
        ContentSig {
            mtime_ms,
            ..sig(byte)
        }
    }

    fn modified(path: &str, byte: u8) -> Event {
        Event::Local(LocalChange {
            path: RelPath::new(path).unwrap(),
            kind: ChangeKind::Modified(sig(byte)),
        })
    }

    /// A genesis-style concurrent pair for "f": A holds `a` at {A:1}, B holds
    /// `b` at {B:1}, returning each side's outbound change (as if freshly
    /// scanned from two pre-existing trees).
    fn genesis_pair(a: ContentSig, b: ContentSig) -> (Engine, Engine, RemoteChange, RemoteChange) {
        let mut ea = engine(A);
        let mut eb = engine(B);
        let a_send = one_send(ea.handle(Event::Local(LocalChange {
            path: rp("f"),
            kind: ChangeKind::Modified(a),
        })));
        let b_send = one_send(eb.handle(Event::Local(LocalChange {
            path: rp("f"),
            kind: ChangeKind::Modified(b),
        })));
        (ea, eb, a_send, b_send)
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

    fn winner_state(e: &Engine, path: &str) -> EntryState {
        e.index().get(&rp(path)).unwrap().winner().state
    }

    // ---- Echo journal accessor -------------------------------------------

    #[test]
    fn is_expected_echo_reflects_outstanding_apply_read_only() {
        let mut e = engine(A);
        // A remote change for an unseen path emits an Apply, journaling an echo
        // expectation for its materialized winner.
        let mut clock = VectorClock::new();
        clock.tick(B);
        let actions = e.handle(Event::Remote(RemoteChange {
            path: rp("a.txt"),
            kind: ChangeKind::Modified(sig(9)),
            version: clock,
        }));
        assert!(actions.iter().any(|a| matches!(a, Action::Apply { .. })));

        // The journal expects exactly Present(sig(9)) at a.txt — nothing else.
        assert!(e.is_expected_echo(&rp("a.txt"), &Expectation::Present(sig(9))));
        assert!(!e.is_expected_echo(&rp("a.txt"), &Expectation::Present(sig(7))));
        assert!(!e.is_expected_echo(&rp("a.txt"), &Expectation::Absent));
        assert!(!e.is_expected_echo(&rp("b.txt"), &Expectation::Present(sig(9))));

        // Read-only: asking again still reports it (nothing was retired).
        assert!(e.is_expected_echo(&rp("a.txt"), &Expectation::Present(sig(9))));

        // The echoing local event retires it; then it is no longer expected.
        assert!(e.handle(modified("a.txt", 9)).is_empty(), "echo swallowed");
        assert!(!e.is_expected_echo(&rp("a.txt"), &Expectation::Present(sig(9))));
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
        assert_eq!(entry.winner().state, EntryState::Present(sig(1)));
        assert_eq!(entry.winner().version.get(A), 1);
    }

    #[test]
    fn local_modified_same_content_is_spurious() {
        let mut e = engine(A);
        e.handle(modified("a.txt", 1));
        let actions = e.handle(modified("a.txt", 1));
        assert!(actions.is_empty());
        assert_eq!(
            e.index().get(&rp("a.txt")).unwrap().winner().version.get(A),
            1
        );
    }

    #[test]
    fn local_chmod_only_is_not_spurious_and_propagates() {
        // Same bytes, execute bit flipped on: a real change (sig equality
        // includes exec), so it ticks the clock, ships, and records a version —
        // it is NOT swallowed as a spurious same-content event.
        let mut e = engine(A);
        e.handle(modified("s", 1)); // sig(1), non-exec
        let actions = e.handle(Event::Local(LocalChange {
            path: rp("s"),
            kind: ChangeKind::Modified(sig_x(1)),
        }));
        assert_eq!(actions.len(), 2, "chmod ships + records");
        assert!(matches!(
            &actions[0],
            Action::Send(rc) if rc.kind == ChangeKind::Modified(sig_x(1))
        ));
        let entry = e.index().get(&rp("s")).unwrap();
        assert_eq!(entry.winner().state, EntryState::Present(sig_x(1)));
        assert_eq!(entry.winner().version.get(A), 2);
    }

    #[test]
    fn remote_chmod_applies_the_new_mode() {
        // A peer flips the execute bit on a file we already hold: the winner
        // moves, so we Apply the executable signature.
        let mut e = engine(A);
        e.handle(modified("s", 1)); // {A:1}, non-exec
        let mut v = VectorClock::new();
        v.tick(A);
        v.tick(A); // {A:2} dominates: fast-forward the chmod
        let actions = e.handle(Event::Remote(RemoteChange {
            path: rp("s"),
            kind: ChangeKind::Modified(sig_x(1)),
            version: v,
        }));
        assert!(actions.iter().any(
            |a| matches!(a, Action::Apply { target: Expectation::Present(s), .. } if *s == sig_x(1))
        ));
        assert_eq!(winner_state(&e, "s"), EntryState::Present(sig_x(1)));
    }

    #[test]
    fn concurrent_same_content_different_exec_converges() {
        // A marks a file executable; B keeps the identical bytes non-executable;
        // concurrent. Both replicas converge to the identical winner with the
        // executable head winning the hash tie (deterministic, invariant #5).
        let mut a = engine(A);
        let mut b = engine(B);
        let a_send = one_send(a.handle(Event::Local(LocalChange {
            path: rp("f"),
            kind: ChangeKind::Modified(sig_x(5)),
        })));
        let b_send = one_send(b.handle(modified("f", 5)));
        a.handle(Event::Remote(b_send));
        b.handle(Event::Remote(a_send));
        assert_eq!(a.index().canonical_bytes(), b.index().canonical_bytes());
        assert_eq!(winner_state(&a, "f"), EntryState::Present(sig_x(5)));
        assert_eq!(winner_state(&b, "f"), EntryState::Present(sig_x(5)));
    }

    #[test]
    fn local_modified_existing_ticks_clock() {
        let mut e = engine(A);
        e.handle(modified("a.txt", 1));
        e.handle(modified("a.txt", 2));
        let entry = e.index().get(&rp("a.txt")).unwrap();
        assert_eq!(entry.winner().state, EntryState::Present(sig(2)));
        assert_eq!(entry.winner().version.get(A), 2);
        assert_eq!(entry.heads().len(), 1);
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
        assert_eq!(winner_state(&e, "a.txt"), EntryState::Tombstone);
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
        assert_eq!(winner_state(&e, "a.txt"), EntryState::Present(sig(8)));
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
        assert_eq!(entry.winner().version, v);
        assert_eq!(entry.winner().state, EntryState::Present(sig(5)));
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
        assert_eq!(winner_state(&e, "a.txt"), EntryState::Tombstone);
    }

    #[test]
    fn remote_stale_change_is_ignored() {
        let mut e = engine(A);
        e.handle(modified("a.txt", 2)); // {A:1}
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
        let entry = e.index().get(&rp("a.txt")).unwrap();
        assert_eq!(entry.winner().version, v);
        assert_eq!(entry.heads().len(), 1);
    }

    #[test]
    fn remote_same_content_advances_clock_silently() {
        // A dominating remote version carrying identical content: the clock
        // advances but there is no new content — no Apply, no RecordVersion.
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
        assert!(
            actions.is_empty(),
            "identical content is not versioned again"
        );
        assert_eq!(e.index().get(&rp("a.txt")).unwrap().winner().version, v);
    }

    // ---- Conflicts --------------------------------------------------------

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
        assert_eq!(winner_state(&a, "f"), EntryState::Present(sig(9)));
        assert_eq!(winner_state(&b, "f"), EntryState::Present(sig(9)));
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
        // Both keep two heads (the loser is preserved).
        assert_eq!(a.index().get(&rp("f")).unwrap().heads().len(), 2);
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
        assert_eq!(winner_state(&a, "f"), EntryState::Present(sig(7)));
        assert_eq!(winner_state(&b, "f"), EntryState::Present(sig(7)));
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
    fn genesis_adopts_newer_mtime_on_both_sides() {
        // First contact between two pre-existing trees: A's copy has the HIGHER
        // hash (would win the standard rule) but an OLDER mtime; B's copy is
        // newer. Adoption must pick B's copy on BOTH replicas, and mark the
        // ConflictResolved as `adopted` so the CLI words it as a first-sync
        // adoption rather than a mid-session clash.
        let a = sig_t(9, 100); // higher hash, older mtime
        let b = sig_t(3, 500); // lower hash, newer mtime
        let (mut ea, mut eb, a_send, b_send) = genesis_pair(a, b);
        let a_actions = ea.handle(Event::Remote(b_send));
        let b_actions = eb.handle(Event::Remote(a_send));
        assert_eq!(winner_state(&ea, "f"), EntryState::Present(b));
        assert_eq!(winner_state(&eb, "f"), EntryState::Present(b));
        assert_eq!(ea.index().canonical_bytes(), eb.index().canonical_bytes());
        // Both surface the resolution, flagged adopted.
        for actions in [&a_actions, &b_actions] {
            assert!(actions
                .iter()
                .any(|x| matches!(x, Action::ConflictResolved { adopted: true, .. })));
        }
        // A (the loser) applies B's bytes; B (the winner) applies nothing new.
        assert!(a_actions.iter().any(
            |x| matches!(x, Action::Apply { target: Expectation::Present(s), .. } if *s == b)
        ));
        assert!(!b_actions.iter().any(|x| matches!(x, Action::Apply { .. })));
    }

    #[test]
    fn genesis_adopts_local_when_it_is_newer() {
        // Symmetric: A's copy is the newer one, so A's bytes win on both sides —
        // proving the rule follows mtime, not "remote always wins".
        let a = sig_t(3, 900); // lower hash, newer mtime
        let b = sig_t(9, 100); // higher hash, older mtime
        let (mut ea, mut eb, a_send, b_send) = genesis_pair(a, b);
        ea.handle(Event::Remote(b_send));
        eb.handle(Event::Remote(a_send));
        assert_eq!(winner_state(&ea, "f"), EntryState::Present(a));
        assert_eq!(winner_state(&eb, "f"), EntryState::Present(a));
    }

    #[test]
    fn concurrent_identical_content_is_not_a_conflict() {
        let (mut a, _b, _a_send, b_send) = concurrent_pair(5, 5);
        let actions = a.handle(Event::Remote(b_send));
        assert!(!actions
            .iter()
            .any(|x| matches!(x, Action::ConflictResolved { .. })));
        assert!(!actions.iter().any(|x| matches!(x, Action::Apply { .. })));
        assert_eq!(winner_state(&a, "f"), EntryState::Present(sig(5)));
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
            Entry::single(v, EntryState::Present(sig(2))),
        );
        let actions = a.handle(Event::PeerIndex(peer));
        assert!(actions.iter().any(|x| matches!(
            x,
            Action::Apply {
                target: Expectation::Present(_),
                ..
            }
        )));
        assert!(!actions.iter().any(|x| matches!(x, Action::Send(_))));
        assert!(a.index().get(&rp("from_b.txt")).is_some());
    }

    // ---- The former divergence hole now CONVERGES (MVR) -------------------

    /// The exact counterexample that diverged under the single-entry model:
    /// A produces two causally-sequential concurrent-lineage versions
    /// (a1 then a2) and BOTH are delivered to B, unfiltered, around B's
    /// concurrent edit. With the head-set register the two replicas now reach
    /// byte-identical indices.
    #[test]
    fn intermediate_reorder_now_converges() {
        let mut a = engine(A);
        let a1 = one_send(a.handle(modified("f", 9))); // {A:1}
        let a2 = one_send(a.handle(modified("f", 7))); // {A:2}

        let mut b = engine(B);
        let b1 = one_send(b.handle(modified("f", 5))); // {B:1}

        // A receives only b1 (its state is a2).
        a.handle(Event::Remote(b1));

        // B receives a1 then a2 (intermediate NOT coalesced).
        b.handle(Event::Remote(a1));
        b.handle(Event::Remote(a2));

        assert_eq!(a.index().canonical_bytes(), b.index().canonical_bytes());
        // Both materialize the same winner: sig(7) (a2) beats sig(5) (b1); the
        // superseded a1 (sig9) was dropped from the antichain.
        assert_eq!(winner_state(&a, "f"), EntryState::Present(sig(7)));
        assert_eq!(a.index().get(&rp("f")).unwrap().heads().len(), 2);
    }

    // ---- Property tests ---------------------------------------------------

    /// A pure two-replica simulator with UNFILTERED, adversarially reorderable
    /// delivery: every `Send` a replica emits is queued and delivered exactly
    /// once, in an order the test controls (including superseded intermediate
    /// versions). This is the strong convergence model the head-set register
    /// must satisfy.
    struct Sim {
        a: Engine,
        b: Engine,
        pending: Vec<(bool, RemoteChange)>, // (deliver_to_a, change)
    }

    #[derive(Debug, Clone)]
    enum Op {
        Edit { on_a: bool, path: u8, byte: u8 },
        Delete { on_a: bool, path: u8 },
        Deliver(usize),
    }

    impl Sim {
        fn new() -> Self {
            Self {
                a: engine(A),
                b: engine(B),
                pending: Vec::new(),
            }
        }

        fn local(&mut self, on_a: bool, ev: Event) {
            let deliver_to_a = !on_a;
            let acts = if on_a {
                self.a.handle(ev)
            } else {
                self.b.handle(ev)
            };
            for act in acts {
                if let Action::Send(rc) = act {
                    self.pending.push((deliver_to_a, rc));
                }
            }
        }

        fn deliver_one(&mut self, sel: usize) {
            if self.pending.is_empty() {
                return;
            }
            let idx = sel % self.pending.len();
            let (to_a, rc) = self.pending.remove(idx);
            // Remote handling never emits Send, so nothing to re-queue.
            let _ = if to_a {
                self.a.handle(Event::Remote(rc))
            } else {
                self.b.handle(Event::Remote(rc))
            };
        }

        fn drain(&mut self) {
            while let Some((to_a, rc)) = self.pending.pop() {
                let _ = if to_a {
                    self.a.handle(Event::Remote(rc))
                } else {
                    self.b.handle(Event::Remote(rc))
                };
            }
        }

        fn run(&mut self, ops: &[Op]) {
            for op in ops {
                match *op {
                    Op::Edit { on_a, path, byte } => {
                        self.local(
                            on_a,
                            Event::Local(LocalChange {
                                path: rp(&format!("p{path}")),
                                kind: ChangeKind::Modified(sig(byte)),
                            }),
                        );
                    }
                    Op::Delete { on_a, path } => {
                        self.local(
                            on_a,
                            Event::Local(LocalChange {
                                path: rp(&format!("p{path}")),
                                kind: ChangeKind::Removed,
                            }),
                        );
                    }
                    Op::Deliver(sel) => self.deliver_one(sel),
                }
            }
            self.drain();
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
            (0usize..8).prop_map(Op::Deliver),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(500))]

        /// Convergence under ARBITRARY delivery order, unfiltered (every Send,
        /// including superseded intermediates): both replicas reach identical
        /// index roots (docs/TESTING.md Level 1). This is what the head-set
        /// register buys over the single-entry model.
        #[test]
        fn convergence(ops in proptest::collection::vec(arb_op(), 0..60)) {
            let mut sim = Sim::new();
            sim.run(&ops);
            prop_assert_eq!(
                sim.a.index().canonical_bytes(),
                sim.b.index().canonical_bytes()
            );
        }

        /// Head-set bound: with two replicas, no entry ever holds more than two
        /// heads (each replica collapses its lineage on every local edit).
        #[test]
        fn head_set_bounded_by_replica_count(
            ops in proptest::collection::vec(arb_op(), 0..60)
        ) {
            let mut sim = Sim::new();
            sim.run(&ops);
            for engine in [&sim.a, &sim.b] {
                for (path, entry) in engine.index().iter() {
                    prop_assert!(
                        entry.heads().len() <= 2,
                        "path {} has {} heads",
                        path,
                        entry.heads().len()
                    );
                }
            }
        }

        /// After convergence, exchanging full indices yields NO `Send` and NO
        /// `Apply` (quiet-network invariant, pure version).
        #[test]
        fn no_spurious_traffic_after_convergence(
            ops in proptest::collection::vec(arb_op(), 0..60)
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
            let back = e.handle(Event::Local(LocalChange {
                path: rp("f"),
                kind: echo_kind,
            }));
            prop_assert!(back.is_empty(), "echo produced actions: {:?}", back);
            prop_assert_eq!(before, e.index().canonical_bytes());
        }

        /// Deterministic conflict winner: for an arbitrary concurrent pair,
        /// both replicas end with byte-identical indices, regardless of which
        /// side each processed first.
        #[test]
        fn deterministic_conflict_winner(
            a_byte in 0u8..12,
            b_byte in 0u8..12,
            a_present in any::<bool>(),
            b_present in any::<bool>(),
        ) {
            let (mut a, mut b, a_send, b_send) =
                seed_concurrent(a_byte, b_byte, a_present, b_present);
            a.handle(Event::Remote(b_send));
            b.handle(Event::Remote(a_send));
            prop_assert_eq!(
                a.index().canonical_bytes(),
                b.index().canonical_bytes()
            );
        }

        /// Resolution stability: after both resolve the same concurrent pair,
        /// re-delivering a resolved winner head is `AlreadyKnown` — no
        /// re-fire, no further action.
        #[test]
        fn resolution_is_stable(a_byte in 0u8..12, b_byte in 0u8..12) {
            let (mut a, mut b, a_send, b_send) = concurrent_pair(a_byte, b_byte);
            a.handle(Event::Remote(b_send));
            b.handle(Event::Remote(a_send));
            prop_assert_eq!(a.index().canonical_bytes(), b.index().canonical_bytes());
            let a_winner = a.index().get(&rp("f")).unwrap().winner().clone();
            let re = b.handle(Event::Remote(RemoteChange {
                path: rp("f"),
                kind: kind_from_state(a_winner.state),
                version: a_winner.version,
            }));
            prop_assert!(re.is_empty());
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

    // ==== H5: reconcile-batching convergence (docs/SEED-PERF.md §2) ========
    //
    // Phase 1 of the seed-perf work will BATCH and REORDER how reconcile-
    // produced changes are shipped and applied (ship the next file's frames
    // without waiting for the previous apply's echo). These tests prove that
    // is safe at the *engine* level and are the license to do it: for an
    // arbitrary diverged index pair, the reconcile Send stream delivered to the
    // peer in ANY batch partition / batch size / order, with duplicate
    // (crash-retry) redelivery across a batch boundary, and interleaved with
    // the peer's own concurrently-generated local edits, drives both replicas
    // to byte-identical indices and identical per-path winners -- including
    // genesis adoption-mode (disjoint-clock) entries.

    /// A local op used to build one replica's pre-reconcile divergence. Edits
    /// carry an mtime so genesis adoption (the mtime-first tiebreak) is
    /// genuinely exercised, not just adoption *mode*.
    #[derive(Debug, Clone)]
    enum SeedOp {
        Edit { path: u8, byte: u8, mtime: u16 },
        Delete { path: u8 },
    }

    fn seed_path(p: u8) -> RelPath {
        rp(&format!("p{p}"))
    }

    /// Apply build ops to a replica, discarding the emitted actions (we only
    /// want the resulting diverged index).
    fn apply_seed(e: &mut Engine, ops: &[SeedOp]) {
        for op in ops {
            let ev = match *op {
                SeedOp::Edit { path, byte, mtime } => Event::Local(LocalChange {
                    path: seed_path(path),
                    kind: ChangeKind::Modified(sig_t(byte, u64::from(mtime))),
                }),
                SeedOp::Delete { path } => Event::Local(LocalChange {
                    path: seed_path(path),
                    kind: ChangeKind::Removed,
                }),
            };
            let _ = e.handle(ev);
        }
    }

    /// Collect every `Send`'s change from an action list.
    fn sends(actions: Vec<Action>) -> Vec<RemoteChange> {
        actions
            .into_iter()
            .filter_map(|a| match a {
                Action::Send(rc) => Some(rc),
                _ => None,
            })
            .collect()
    }

    /// The reconcile Send stream `from` would ship to a peer holding
    /// `peer_index` -- computed on a throwaway clone so `from`'s real index is
    /// untouched. The peer will learn `from`'s state ONLY through this
    /// delivered stream, exactly as the batched transport ships it (rather than
    /// getting a free full-index absorb), which is what makes the batching
    /// dimension load-bearing here.
    fn reconcile_stream(from: &Engine, peer_index: &Index) -> Vec<RemoteChange> {
        let mut probe = from.clone();
        sends(probe.handle(Event::PeerIndex(peer_index.clone())))
    }

    /// Build a diverged `(a, b)` pair. Without `pre_sync` the two replicas
    /// never met, so same-path heads have disjoint clock support -> genesis
    /// adoption mode. With `pre_sync` they first fully converge (a mutual
    /// absorb) and then re-diverge via the post ops, whose local edits stamp
    /// merged both-replica clocks -> overlapping support, i.e. steady-state
    /// (non-adoption) shared history. H5 requires both shapes.
    fn build_pair(
        a_ops: &[SeedOp],
        b_ops: &[SeedOp],
        pre_sync: bool,
        a_post: &[SeedOp],
        b_post: &[SeedOp],
    ) -> (Engine, Engine) {
        let mut a = engine(A);
        let mut b = engine(B);
        apply_seed(&mut a, a_ops);
        apply_seed(&mut b, b_ops);
        if pre_sync {
            let ia = a.index().clone();
            let ib = b.index().clone();
            let _ = a.handle(Event::PeerIndex(ib));
            let _ = b.handle(Event::PeerIndex(ia));
            apply_seed(&mut a, a_post);
            apply_seed(&mut b, b_post);
        }
        (a, b)
    }

    /// A two-replica delivery harness for reconcile streams under adversarial
    /// batching. `to_a`/`to_b` are the pending deliveries for each side; a
    /// "batch" is any run of items popped in one call. `dealt_*` remembers what
    /// has already been delivered so a duplicate (crash-retry) redelivery can
    /// be replayed.
    struct Wire {
        a: Engine,
        b: Engine,
        to_a: Vec<RemoteChange>,
        to_b: Vec<RemoteChange>,
        dealt_a: Vec<RemoteChange>,
        dealt_b: Vec<RemoteChange>,
    }

    impl Wire {
        fn new(a: Engine, b: Engine) -> Self {
            Self {
                a,
                b,
                to_a: Vec::new(),
                to_b: Vec::new(),
                dealt_a: Vec::new(),
                dealt_b: Vec::new(),
            }
        }

        /// A concurrently-generated local event on the receiving side; its
        /// resulting `Send`s are queued for the peer (Remote handling emits no
        /// `Send`, so only local events feed the queues).
        fn local(&mut self, on_a: bool, ev: Event) {
            let out = if on_a {
                self.a.handle(ev)
            } else {
                self.b.handle(ev)
            };
            for rc in sends(out) {
                if on_a {
                    self.to_b.push(rc);
                } else {
                    self.to_a.push(rc);
                }
            }
        }

        /// Deliver a batch of up to `n` pending changes to one side, in a
        /// selector-driven order (arbitrary partition + arbitrary order).
        fn deliver_batch(&mut self, to_a: bool, sel: usize, n: usize) {
            if to_a {
                Self::deal(&mut self.a, &mut self.to_a, &mut self.dealt_a, sel, n);
            } else {
                Self::deal(&mut self.b, &mut self.to_b, &mut self.dealt_b, sel, n);
            }
        }

        fn deal(
            eng: &mut Engine,
            queue: &mut Vec<RemoteChange>,
            dealt: &mut Vec<RemoteChange>,
            sel: usize,
            n: usize,
        ) {
            for _ in 0..n {
                if queue.is_empty() {
                    break;
                }
                let idx = sel % queue.len();
                let rc = queue.remove(idx);
                let _ = eng.handle(Event::Remote(rc.clone()));
                dealt.push(rc);
            }
        }

        fn deliver_all_to(&mut self, to_a: bool) {
            let n = if to_a {
                self.to_a.len()
            } else {
                self.to_b.len()
            };
            self.deliver_batch(to_a, 0, n);
        }

        /// Re-deliver up to `n` already-delivered changes (duplicate delivery
        /// across a batch boundary -- the crash-retry case).
        fn redeliver(&mut self, to_a: bool, sel: usize, n: usize) {
            let (eng, dealt): (&mut Engine, &Vec<RemoteChange>) = if to_a {
                (&mut self.a, &self.dealt_a)
            } else {
                (&mut self.b, &self.dealt_b)
            };
            if dealt.is_empty() {
                return;
            }
            for k in 0..n {
                let idx = (sel + k) % dealt.len();
                let rc = dealt[idx].clone();
                let _ = eng.handle(Event::Remote(rc));
            }
        }

        /// Flush every remaining pending change both ways (Remote handling
        /// emits no `Send`, so this terminates after one pass).
        fn drain(&mut self) {
            while !self.to_a.is_empty() || !self.to_b.is_empty() {
                while let Some(rc) = self.to_a.pop() {
                    let _ = self.a.handle(Event::Remote(rc));
                }
                while let Some(rc) = self.to_b.pop() {
                    let _ = self.b.handle(Event::Remote(rc));
                }
            }
        }

        /// Assert both replicas converged: byte-identical indices AND an
        /// identical materialized winner at every path in the union.
        fn assert_converged(&self) -> Result<(), proptest::test_runner::TestCaseError> {
            prop_assert_eq!(
                self.a.index().canonical_bytes(),
                self.b.index().canonical_bytes()
            );
            let mut paths: BTreeSet<RelPath> = BTreeSet::new();
            for (p, _) in self.a.index().iter() {
                paths.insert(p.clone());
            }
            for (p, _) in self.b.index().iter() {
                paths.insert(p.clone());
            }
            for p in paths {
                let wa = self.a.index().get(&p).map(|e| e.winner().state);
                let wb = self.b.index().get(&p).map(|e| e.winner().state);
                prop_assert_eq!(wa, wb, "winner mismatch at {}", p);
            }
            Ok(())
        }
    }

    #[derive(Debug, Clone)]
    enum WireOp {
        Deliver {
            to_a: bool,
            sel: u8,
            n: u8,
        },
        Redeliver {
            to_a: bool,
            sel: u8,
            n: u8,
        },
        Edit {
            on_a: bool,
            path: u8,
            byte: u8,
            mtime: u16,
        },
        Delete {
            on_a: bool,
            path: u8,
        },
    }

    fn arb_seed_op() -> impl Strategy<Value = SeedOp> {
        prop_oneof![
            3 => (0u8..5, 1u8..7, 0u16..8)
                .prop_map(|(path, byte, mtime)| SeedOp::Edit { path, byte, mtime }),
            1 => (0u8..5).prop_map(|path| SeedOp::Delete { path }),
        ]
    }

    fn arb_wire_op() -> impl Strategy<Value = WireOp> {
        prop_oneof![
            4 => (any::<bool>(), any::<u8>(), 1u8..6)
                .prop_map(|(to_a, sel, n)| WireOp::Deliver { to_a, sel, n }),
            1 => (any::<bool>(), any::<u8>(), 1u8..4)
                .prop_map(|(to_a, sel, n)| WireOp::Redeliver { to_a, sel, n }),
            2 => (any::<bool>(), 0u8..5, 1u8..7, 0u16..8)
                .prop_map(|(on_a, path, byte, mtime)| WireOp::Edit { on_a, path, byte, mtime }),
            1 => (any::<bool>(), 0u8..5).prop_map(|(on_a, path)| WireOp::Delete { on_a, path }),
        ]
    }

    /// Set up a diverged pair, compute both reconcile streams, and stage them
    /// in a fresh `Wire` ready for adversarial delivery.
    fn wire_from(
        a_ops: &[SeedOp],
        b_ops: &[SeedOp],
        pre_sync: bool,
        a_post: &[SeedOp],
        b_post: &[SeedOp],
    ) -> Wire {
        let (a, b) = build_pair(a_ops, b_ops, pre_sync, a_post, b_post);
        let idx_a = a.index().clone();
        let idx_b = b.index().clone();
        let to_b = reconcile_stream(&a, &idx_b); // A's heads B is missing
        let to_a = reconcile_stream(&b, &idx_a); // B's heads A is missing
        let mut w = Wire::new(a, b);
        w.to_a = to_a;
        w.to_b = to_b;
        w
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// H5 headline: the reconcile action stream converges both replicas to
        /// identical canonical bytes AND identical per-path winners REGARDLESS
        /// of batch partitioning, batch sizes, delivery order, duplicate
        /// redelivery, and interleaving with concurrent local edits on the
        /// receiving side. Covers genesis (disjoint-support/adoption) and
        /// shared-history pairs (via `pre_sync`).
        #[test]
        fn prop_reconcile_batching_converges(
            a_ops in proptest::collection::vec(arb_seed_op(), 0..12),
            b_ops in proptest::collection::vec(arb_seed_op(), 0..12),
            pre_sync in any::<bool>(),
            a_post in proptest::collection::vec(arb_seed_op(), 0..4),
            b_post in proptest::collection::vec(arb_seed_op(), 0..4),
            ops in proptest::collection::vec(arb_wire_op(), 0..40),
        ) {
            let mut w = wire_from(&a_ops, &b_ops, pre_sync, &a_post, &b_post);
            for op in &ops {
                match *op {
                    WireOp::Deliver { to_a, sel, n } => {
                        w.deliver_batch(to_a, usize::from(sel), usize::from(n));
                    }
                    WireOp::Redeliver { to_a, sel, n } => {
                        w.redeliver(to_a, usize::from(sel), usize::from(n));
                    }
                    WireOp::Edit { on_a, path, byte, mtime } => {
                        w.local(on_a, Event::Local(LocalChange {
                            path: seed_path(path),
                            kind: ChangeKind::Modified(sig_t(byte, u64::from(mtime))),
                        }));
                    }
                    WireOp::Delete { on_a, path } => {
                        w.local(on_a, Event::Local(LocalChange {
                            path: seed_path(path),
                            kind: ChangeKind::Removed,
                        }));
                    }
                }
            }
            w.drain();
            w.assert_converged()?;
        }

        /// H5 idempotence: after delivering the full reconcile streams (both
        /// replicas converged), re-delivering an ARBITRARY prefix/suffix batch
        /// -- the crash-retry-across-a-boundary case -- changes nothing on
        /// either side. This is what makes retrying a batch after a crash safe.
        #[test]
        fn prop_reconcile_redelivery_is_idempotent(
            a_ops in proptest::collection::vec(arb_seed_op(), 0..12),
            b_ops in proptest::collection::vec(arb_seed_op(), 0..12),
            pre_sync in any::<bool>(),
            a_post in proptest::collection::vec(arb_seed_op(), 0..4),
            b_post in proptest::collection::vec(arb_seed_op(), 0..4),
            dup_sel in any::<u8>(),
            dup_n in 0u8..12,
        ) {
            let mut w = wire_from(&a_ops, &b_ops, pre_sync, &a_post, &b_post);
            w.deliver_all_to(true);
            w.deliver_all_to(false);
            let a_bytes = w.a.index().canonical_bytes();
            let b_bytes = w.b.index().canonical_bytes();
            prop_assert_eq!(&a_bytes, &b_bytes);
            // Duplicate delivery of any already-shipped batch is a total no-op.
            w.redeliver(true, usize::from(dup_sel), usize::from(dup_n));
            w.redeliver(false, usize::from(dup_sel), usize::from(dup_n));
            prop_assert_eq!(w.a.index().canonical_bytes(), a_bytes);
            prop_assert_eq!(w.b.index().canonical_bytes(), b_bytes);
        }
    }

    /// H5 concrete: genesis adoption survives adversarial batching. Two
    /// pre-existing trees, never met; the same path is edited on both with
    /// different content AND different mtime -> genesis adoption picks the
    /// newer-mtime copy on BOTH replicas even though the older copy has the
    /// higher hash. Deliver each reconcile stream as [one item, a duplicate of
    /// it, then the rest] and assert both converge to the identical adopted
    /// winner.
    #[test]
    fn reconcile_genesis_adoption_converges_under_batching() {
        let mut a = engine(A);
        let mut b = engine(B);
        // A: p0 older (mtime 100) with the HIGHER hash; p1 only on A.
        apply_seed(
            &mut a,
            &[
                SeedOp::Edit {
                    path: 0,
                    byte: 9,
                    mtime: 100,
                },
                SeedOp::Edit {
                    path: 1,
                    byte: 4,
                    mtime: 100,
                },
            ],
        );
        // B: p0 newer (mtime 500) with the LOWER hash; p2 only on B.
        apply_seed(
            &mut b,
            &[
                SeedOp::Edit {
                    path: 0,
                    byte: 3,
                    mtime: 500,
                },
                SeedOp::Edit {
                    path: 2,
                    byte: 5,
                    mtime: 100,
                },
            ],
        );
        let idx_a = a.index().clone();
        let idx_b = b.index().clone();
        let to_b = reconcile_stream(&a, &idx_b);
        let to_a = reconcile_stream(&b, &idx_a);
        let mut w = Wire::new(a, b);
        w.to_a = to_a;
        w.to_b = to_b;
        // Adversarial split, each direction: a single item, a duplicate, rest.
        w.deliver_batch(false, 0, 1);
        w.redeliver(false, 0, 1);
        w.deliver_all_to(false);
        w.deliver_batch(true, 0, 1);
        w.redeliver(true, 0, 1);
        w.deliver_all_to(true);
        w.drain();

        assert_eq!(w.a.index().canonical_bytes(), w.b.index().canonical_bytes());
        // The newer-mtime copy (B's byte 3) wins p0 on both replicas.
        assert_eq!(winner_state(&w.a, "p0"), EntryState::Present(sig_t(3, 500)));
        assert_eq!(winner_state(&w.b, "p0"), EntryState::Present(sig_t(3, 500)));
        // p0 really is a genesis (disjoint-support) adoption entry on both.
        assert!(w.a.index().get(&rp("p0")).unwrap().adoption_mode());
        assert!(w.b.index().get(&rp("p0")).unwrap().adoption_mode());
        // Disjoint files propagated both directions.
        assert_eq!(winner_state(&w.a, "p1"), EntryState::Present(sig_t(4, 100)));
        assert_eq!(winner_state(&w.b, "p2"), EntryState::Present(sig_t(5, 100)));
    }
}
