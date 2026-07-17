//! The index model: the engine's authoritative view of the tree.
//!
//! An [`Index`] maps every [`RelPath`] to an [`Entry`]. As of M1 an entry is a
//! **multi-value register** (a Dynamo-sibling-style set of concurrent
//! [`Head`]s), not a single version. Each head is one causally-concurrent
//! version of the path — its vector clock plus the state at that version.
//! Deleted files are retained as [`EntryState::Tombstone`] heads so deletions
//! propagate and delete-vs-edit conflicts remain detectable (docs/SPEC.md
//! §5.3).
//!
//! # Why a head set (invariant #5, convergence)
//! A single-version entry that merged clocks on conflict does **not** converge
//! under arbitrary reordering of superseded, concurrent-lineage versions:
//! content hash is not monotonic along a lineage, so a later same-lineage write
//! can look concurrent and re-open a conflict against a different opponent. A
//! head set is a proper join-semilattice — [`Entry::absorb`] is "union of
//! version-tagged states, then discard causally-dominated ones", which is
//! commutative, associative, and idempotent — so replicas converge regardless
//! of delivery order. The materialized [`Entry::winner`] (what exists on disk)
//! is a deterministic pure function of the head set, identical on both
//! replicas.
//!
//! # Head-set bound
//! Each replica collapses a path's heads to a single head on every local edit
//! (see the engine), so one replica's successive versions of a path are totally
//! ordered and [`Entry::absorb`] keeps at most one head per source replica.
//! With `N` replicas the head set is therefore bounded by `N`; for the v0
//! two-replica topology it never exceeds two.
//!
//! The engine never hashes bytes: [`ContentHash`] values are computed by the
//! `tomo-history` adapter (BLAKE3) and handed in. The engine only compares and
//! serializes them.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;

use crate::clock::{Causality, VectorClock};
use crate::path::RelPath;

/// A 32-byte content hash (BLAKE3), opaque to the engine.
///
/// The engine treats it as an identity token: it is compared and encoded but
/// never computed here (that would be I/O). [`fmt::Display`] renders it as
/// 64 lowercase hex characters.
///
/// ```
/// use tomo_engine::ContentHash;
/// let h = ContentHash([0xab; 32]);
/// assert_eq!(h.to_string(), "ab".repeat(32));
/// ```
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct ContentHash(pub [u8; 32]);

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// The identity of a file's content: its hash and its byte length.
///
/// Size is carried alongside the hash so cheap size checks can short-circuit
/// comparisons and so the canonical digest is unambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContentSig {
    /// Content hash of the file's bytes.
    pub hash: ContentHash,
    /// Length of the file's content in bytes.
    pub size: u64,
}

/// The state of an indexed path at one version: present with content, or a
/// tombstone.
///
/// Tombstones are kept rather than removed so that a deletion is a versioned
/// fact that can propagate to the peer and lose or win a conflict like any
/// other change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EntryState {
    /// The file exists with the given content signature.
    Present(ContentSig),
    /// The file has been deleted; the entry is retained as a tombstone.
    Tombstone,
}

/// One concurrent version of a path: the state and the vector clock at which it
/// was recorded.
///
/// Within an [`Entry`], all heads are pairwise [`Causality::Concurrent`] — a
/// causal antichain. Two heads never share a clock: clocks are minted by
/// [`VectorClock::tick`] on a single replica, so an equal clock implies the
/// same originating event and therefore the same state.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Head {
    /// The vector-clock version of this head.
    pub version: VectorClock,
    /// The state (present with content, or tombstone) at this version.
    pub state: EntryState,
}

impl Head {
    /// Append this head's canonical byte encoding to `out`.
    ///
    /// Format (all integers little-endian): `u64` clock length `n`, then `n`
    /// `(u64 replica, u64 counter)` pairs ascending; then one state tag —
    /// `0` = tombstone, `1` = present — and for present the 32 hash bytes plus
    /// the `u64` size. This is the per-head record used both to sort a head set
    /// deterministically and to build [`Index::canonical_bytes`].
    fn encode(&self, out: &mut Vec<u8>) {
        let clock: Vec<(u64, u64)> = self.version.iter().map(|(r, c)| (r.0, c)).collect();
        out.extend_from_slice(&(clock.len() as u64).to_le_bytes());
        for (replica, counter) in clock {
            out.extend_from_slice(&replica.to_le_bytes());
            out.extend_from_slice(&counter.to_le_bytes());
        }
        match self.state {
            EntryState::Tombstone => out.push(0),
            EntryState::Present(sig) => {
                out.push(1);
                out.extend_from_slice(&sig.hash.0);
                out.extend_from_slice(&sig.size.to_le_bytes());
            }
        }
    }

    /// The canonical encoding as a standalone key (for sorting head sets).
    fn sort_key(&self) -> Vec<u8> {
        let mut key = Vec::new();
        self.encode(&mut key);
        key
    }
}

/// What [`Entry::absorb`] did with an incoming version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbsorbOutcome {
    /// The version was already dominated by (or equal to) an existing head:
    /// nothing changed.
    AlreadyKnown,
    /// The version was integrated into the head set.
    Absorbed {
        /// The materialized [`Entry::winner`] state changed — the on-disk file
        /// must be brought into line.
        winner_changed: bool,
        /// The head count went from one to two-or-more with differing content:
        /// a newly user-visible conflict to surface (identical-content or
        /// all-tombstone sibling sets are not conflicts).
        new_conflict: bool,
        /// The absorbed state's content was not already present among the
        /// heads — a genuinely new version to record in history.
        novel_content: bool,
    },
}

/// An index entry: the set of concurrent [`Head`]s for a path.
///
/// # Invariants (upheld by [`Entry::single`] and [`Entry::absorb`])
/// - non-empty;
/// - heads are pairwise [`Causality::Concurrent`] (a causal antichain);
/// - heads are stored in canonical [`Head::sort_key`] order, so the entry's
///   serialization is replica-independent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    heads: Vec<Head>,
}

impl Entry {
    /// An entry with a single head — the shape after any local edit, and the
    /// starting shape for a freshly seen path.
    pub fn single(version: VectorClock, state: EntryState) -> Self {
        Self {
            heads: vec![Head { version, state }],
        }
    }

    /// The concurrent heads, in canonical order.
    pub fn heads(&self) -> &[Head] {
        &self.heads
    }

    /// The deterministically materialized winner — what exists on disk.
    ///
    /// Total order (identical on every replica, invariant #5):
    /// [`EntryState::Present`] beats [`EntryState::Tombstone`] (an edit survives
    /// a concurrent delete); between two presents the higher `hash.0` wins;
    /// remaining ties (identical content, or two tombstones) break on the
    /// larger canonical clock encoding. Ties only ever occur between
    /// content-identical heads, so the tiebreak never changes what bytes are on
    /// disk — only which clock labels the winner.
    pub fn winner(&self) -> &Head {
        let mut iter = self.heads.iter();
        // Invariant: an `Entry` is never empty (constructed only via
        // `single`/`absorb`, neither of which can empty the head set).
        let Some(mut best) = iter.next() else {
            unreachable!("Entry always has at least one head")
        };
        for head in iter {
            if winner_cmp(head, best) == Ordering::Greater {
                best = head;
            }
        }
        best
    }

    /// The merge of every head's clock — the causal context of the whole entry.
    ///
    /// A local edit stamps its new single head with this (ticked), which is why
    /// each replica's per-path versions stay totally ordered.
    pub fn merged_clock(&self) -> VectorClock {
        let mut clock = VectorClock::new();
        for head in &self.heads {
            clock.merge(&head.version);
        }
        clock
    }

    /// Integrate `(version, state)` into the head set — the lattice join.
    ///
    /// If any head already dominates or equals `version`, this is a stale or
    /// duplicate delivery ([`AbsorbOutcome::AlreadyKnown`]). Otherwise every
    /// head strictly dominated by `version` is dropped, the new head is pushed,
    /// and the set is re-sorted. Because heads are a causal antichain, `version`
    /// cannot be simultaneously dominated by one head and dominate another, so
    /// these two cases are exhaustive and disjoint.
    pub fn absorb(&mut self, version: VectorClock, state: EntryState) -> AbsorbOutcome {
        for head in &self.heads {
            match head.version.compare(&version) {
                // An existing head is newer-or-equal: we already know this.
                Causality::After | Causality::Equal => return AbsorbOutcome::AlreadyKnown,
                Causality::Before | Causality::Concurrent => {}
            }
        }

        let winner_before = self.winner().state;
        let before_len = self.heads.len();
        let novel_content = !self.heads.iter().any(|h| same_content(h.state, state));

        // Drop heads strictly dominated by the incoming version, then add it.
        self.heads
            .retain(|h| !matches!(h.version.compare(&version), Causality::Before));
        self.heads.push(Head { version, state });
        self.heads.sort_by_key(Head::sort_key);

        let after_len = self.heads.len();
        let winner_changed = winner_before != self.winner().state;
        let new_conflict = before_len == 1 && after_len >= 2 && self.heads_conflict();

        AbsorbOutcome::Absorbed {
            winner_changed,
            new_conflict,
            novel_content,
        }
    }

    /// Whether the head set contains two heads with differing content (a
    /// user-visible conflict). Identical-content siblings and all-tombstone
    /// siblings are not conflicts.
    fn heads_conflict(&self) -> bool {
        match self.heads.first() {
            None => false,
            Some(first) => self
                .heads
                .iter()
                .any(|h| !same_content(h.state, first.state)),
        }
    }
}

/// Winner ordering between two heads (`Greater` == better winner). See
/// [`Entry::winner`] for the rationale.
fn winner_cmp(a: &Head, b: &Head) -> Ordering {
    match (a.state, b.state) {
        (EntryState::Present(sa), EntryState::Present(sb)) => sa
            .hash
            .0
            .cmp(&sb.hash.0)
            .then_with(|| clock_key(&a.version).cmp(&clock_key(&b.version))),
        (EntryState::Present(_), EntryState::Tombstone) => Ordering::Greater,
        (EntryState::Tombstone, EntryState::Present(_)) => Ordering::Less,
        (EntryState::Tombstone, EntryState::Tombstone) => {
            clock_key(&a.version).cmp(&clock_key(&b.version))
        }
    }
}

/// Whether two states carry the same content (hash equality, or both
/// tombstones). Size is not consulted: equal hashes mean identical content.
fn same_content(a: EntryState, b: EntryState) -> bool {
    match (a, b) {
        (EntryState::Present(x), EntryState::Present(y)) => x.hash == y.hash,
        (EntryState::Tombstone, EntryState::Tombstone) => true,
        _ => false,
    }
}

/// A clock's canonical byte encoding (ascending `(replica, counter)` pairs),
/// used only as a deterministic tiebreak key.
fn clock_key(version: &VectorClock) -> Vec<u8> {
    let mut out = Vec::new();
    for (replica, counter) in version.iter() {
        out.extend_from_slice(&replica.0.to_le_bytes());
        out.extend_from_slice(&counter.to_le_bytes());
    }
    out
}

/// The engine's map from path to [`Entry`].
///
/// Backed by a `BTreeMap`, so iteration and the canonical digest are
/// deterministic across replicas — a prerequisite for the "equal roots"
/// convergence assertion.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Index {
    entries: BTreeMap<RelPath, Entry>,
}

impl Index {
    /// An empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// The entry for `path`, if any (including tombstoned/conflicted entries).
    pub fn get(&self, path: &RelPath) -> Option<&Entry> {
        self.entries.get(path)
    }

    /// Insert or replace the entry at `path`, returning the previous entry.
    pub fn upsert(&mut self, path: RelPath, entry: Entry) -> Option<Entry> {
        self.entries.insert(path, entry)
    }

    /// Iterate `(path, entry)` pairs in ascending path order.
    pub fn iter(&self) -> impl Iterator<Item = (&RelPath, &Entry)> + '_ {
        self.entries.iter()
    }

    /// Number of entries (present, tombstoned, or conflicted).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the index has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// A deterministic byte serialization of the whole index.
    ///
    /// Adapters hash this to produce a single "root" digest; two indices with
    /// identical logical content always yield identical bytes, and any
    /// difference in content yields different bytes (the encoding is injective,
    /// via length prefixes). This is the mechanism behind the equal-roots
    /// convergence assertion (docs/TESTING.md).
    ///
    /// # Format (all integers little-endian)
    /// A stream of records, one per entry, in ascending [`RelPath`] order:
    /// 1. `u64` path length in bytes, then the UTF-8 path bytes;
    /// 2. `u64` head count `m`, then `m` head records (see [`Head::encode`]) in
    ///    the entry's canonical head order.
    ///
    /// The format is intentionally boring: stability across releases and
    /// replicas is the only goal, so it is documented and length-prefixed
    /// rather than clever.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (path, entry) in &self.entries {
            let path_bytes = path.as_str().as_bytes();
            out.extend_from_slice(&(path_bytes.len() as u64).to_le_bytes());
            out.extend_from_slice(path_bytes);

            out.extend_from_slice(&(entry.heads.len() as u64).to_le_bytes());
            for head in &entry.heads {
                head.encode(&mut out);
            }
        }
        out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // fine in tests
mod tests {
    use super::*;
    use crate::clock::ReplicaId;
    use proptest::prelude::*;

    fn sig(byte: u8, size: u64) -> ContentSig {
        ContentSig {
            hash: ContentHash([byte; 32]),
            size,
        }
    }

    fn present(byte: u8, size: u64) -> Entry {
        Entry::single(VectorClock::new(), EntryState::Present(sig(byte, size)))
    }

    fn clock(pairs: &[(u64, u64)]) -> VectorClock {
        let mut v = VectorClock::new();
        for &(r, count) in pairs {
            for _ in 0..count {
                v.tick(ReplicaId(r));
            }
        }
        v
    }

    #[test]
    fn content_hash_hex_display() {
        let mut bytes = [0u8; 32];
        bytes[0] = 0x0f;
        bytes[31] = 0xa0;
        let h = ContentHash(bytes);
        let s = h.to_string();
        assert_eq!(s.len(), 64);
        assert!(s.starts_with("0f"));
        assert!(s.ends_with("a0"));
    }

    #[test]
    fn upsert_get_and_replace() {
        let mut idx = Index::new();
        assert!(idx.is_empty());
        let p = RelPath::new("a/b.txt").unwrap();

        assert!(idx.upsert(p.clone(), present(1, 10)).is_none());
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.get(&p), Some(&present(1, 10)));

        let prev = idx.upsert(p.clone(), present(2, 20));
        assert_eq!(prev, Some(present(1, 10)));
        assert_eq!(idx.get(&p), Some(&present(2, 20)));
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn single_entry_winner_is_itself() {
        let e = present(1, 5);
        assert_eq!(e.heads().len(), 1);
        assert_eq!(e.winner().state, EntryState::Present(sig(1, 5)));
    }

    #[test]
    fn tombstone_head_is_retained_and_distinct() {
        let mut idx = Index::new();
        let p = RelPath::new("gone.txt").unwrap();
        idx.upsert(p.clone(), present(1, 5));
        let tomb = Entry::single(VectorClock::new(), EntryState::Tombstone);
        idx.upsert(p.clone(), tomb.clone());
        assert_eq!(idx.get(&p), Some(&tomb));
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn iter_is_sorted_by_path() {
        let mut idx = Index::new();
        idx.upsert(RelPath::new("b").unwrap(), present(1, 1));
        idx.upsert(RelPath::new("a").unwrap(), present(1, 1));
        idx.upsert(RelPath::new("a/c").unwrap(), present(1, 1));
        let paths: Vec<&str> = idx.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, ["a", "a/c", "b"]);
    }

    #[test]
    fn canonical_bytes_is_insertion_order_independent() {
        let mut a = Index::new();
        a.upsert(RelPath::new("x").unwrap(), present(1, 1));
        a.upsert(RelPath::new("y").unwrap(), present(2, 2));
        let mut b = Index::new();
        b.upsert(RelPath::new("y").unwrap(), present(2, 2));
        b.upsert(RelPath::new("x").unwrap(), present(1, 1));
        assert_eq!(a.canonical_bytes(), b.canonical_bytes());
    }

    // ---- Head-set / absorb algebra ----------------------------------------

    #[test]
    fn absorb_stale_version_is_already_known() {
        // Head at {A:2}; a stale {A:1} is dominated.
        let mut e = Entry::single(clock(&[(1, 2)]), EntryState::Present(sig(1, 1)));
        let out = e.absorb(clock(&[(1, 1)]), EntryState::Present(sig(2, 2)));
        assert_eq!(out, AbsorbOutcome::AlreadyKnown);
        assert_eq!(e.heads().len(), 1);
    }

    #[test]
    fn absorb_dominating_version_fast_forwards() {
        let mut e = Entry::single(clock(&[(1, 1)]), EntryState::Present(sig(1, 1)));
        let out = e.absorb(clock(&[(1, 2)]), EntryState::Present(sig(2, 2)));
        assert_eq!(
            out,
            AbsorbOutcome::Absorbed {
                winner_changed: true,
                new_conflict: false,
                novel_content: true,
            }
        );
        assert_eq!(e.heads().len(), 1);
        assert_eq!(e.winner().state, EntryState::Present(sig(2, 2)));
    }

    #[test]
    fn absorb_concurrent_creates_conflict() {
        let mut e = Entry::single(clock(&[(1, 1)]), EntryState::Present(sig(9, 1)));
        let out = e.absorb(clock(&[(2, 1)]), EntryState::Present(sig(3, 1)));
        assert!(matches!(
            out,
            AbsorbOutcome::Absorbed {
                new_conflict: true,
                ..
            }
        ));
        assert_eq!(e.heads().len(), 2);
        // Higher hash wins.
        assert_eq!(e.winner().state, EntryState::Present(sig(9, 1)));
    }

    #[test]
    fn absorb_identical_content_concurrently_is_not_a_conflict() {
        let mut e = Entry::single(clock(&[(1, 1)]), EntryState::Present(sig(5, 1)));
        let out = e.absorb(clock(&[(2, 1)]), EntryState::Present(sig(5, 1)));
        assert_eq!(
            out,
            AbsorbOutcome::Absorbed {
                winner_changed: false,
                new_conflict: false,
                novel_content: false,
            }
        );
        assert_eq!(e.heads().len(), 2);
        assert_eq!(e.winner().state, EntryState::Present(sig(5, 1)));
    }

    #[test]
    fn merged_clock_covers_all_heads() {
        let mut e = Entry::single(clock(&[(1, 1)]), EntryState::Present(sig(9, 1)));
        e.absorb(clock(&[(2, 1)]), EntryState::Present(sig(3, 1)));
        let m = e.merged_clock();
        assert_eq!(m.get(ReplicaId(1)), 1);
        assert_eq!(m.get(ReplicaId(2)), 1);
    }

    #[test]
    fn winner_present_beats_tombstone_regardless_of_order() {
        let mut a = Entry::single(clock(&[(1, 1)]), EntryState::Present(sig(1, 1)));
        a.absorb(clock(&[(2, 1)]), EntryState::Tombstone);
        let mut b = Entry::single(clock(&[(2, 1)]), EntryState::Tombstone);
        b.absorb(clock(&[(1, 1)]), EntryState::Present(sig(1, 1)));
        assert_eq!(a.winner().state, EntryState::Present(sig(1, 1)));
        assert_eq!(b.winner().state, EntryState::Present(sig(1, 1)));
        assert!(a.canonical_bytes_eq(&b));
    }

    impl Entry {
        // Test-only helper: do two entries serialize identically?
        fn canonical_bytes_eq(&self, other: &Entry) -> bool {
            let mut a = Vec::new();
            for h in &self.heads {
                h.encode(&mut a);
            }
            let mut b = Vec::new();
            for h in &other.heads {
                h.encode(&mut b);
            }
            a == b
        }
    }

    /// A strategy producing a single head `(clock, state)`.
    ///
    /// The state is derived *deterministically from the clock*, mirroring the
    /// real-world invariant that a vector clock is minted once per event and so
    /// uniquely identifies its content. Generating independent (clock, state)
    /// pairs would let two heads share a clock yet disagree on content — an
    /// input that cannot occur and under which `absorb` is legitimately
    /// order-dependent.
    fn arb_head() -> impl Strategy<Value = (VectorClock, EntryState)> {
        proptest::collection::btree_map(0u64..3, 1u64..4, 0..3).prop_map(|clock_spec| {
            let mut version = VectorClock::new();
            let mut key: u64 = 0;
            for (r, count) in &clock_spec {
                for _ in 0..*count {
                    version.tick(ReplicaId(*r));
                }
                key = key.wrapping_add((r + 1).wrapping_mul(*count));
            }
            let state = if key.is_multiple_of(4) {
                EntryState::Tombstone
            } else {
                EntryState::Present(sig(u8::try_from(key % 7).unwrap_or(0), key))
            };
            (version, state)
        })
    }

    /// A strategy producing small arbitrary entries by absorbing random heads
    /// (so every generated entry satisfies the head-set invariants).
    fn arb_entry() -> impl Strategy<Value = Entry> {
        (arb_head(), proptest::collection::vec(arb_head(), 0..4)).prop_map(|(first, rest)| {
            let mut e = Entry::single(first.0, first.1);
            for (v, s) in rest {
                e.absorb(v, s);
            }
            e
        })
    }

    /// A strategy producing small arbitrary indices.
    fn arb_index() -> impl Strategy<Value = Index> {
        proptest::collection::btree_map("[a-z]{1,4}", arb_entry(), 0..5).prop_map(|m| {
            let mut idx = Index::new();
            for (name, e) in m {
                let path = RelPath::new(&name).expect("generated name is a valid path");
                idx.upsert(path, e);
            }
            idx
        })
    }

    proptest! {
        /// Deterministic: encoding the same index twice yields the same bytes.
        #[test]
        fn canonical_bytes_deterministic(idx in arb_index()) {
            prop_assert_eq!(idx.canonical_bytes(), idx.clone().canonical_bytes());
        }

        /// Injective: two indices are logically equal iff their canonical
        /// bytes are equal (equal ⇒ equal bytes; differ ⇒ differ bytes).
        #[test]
        fn canonical_bytes_iff_equal(a in arb_index(), b in arb_index()) {
            prop_assert_eq!(a == b, a.canonical_bytes() == b.canonical_bytes());
        }

        /// Every entry keeps its heads a causal antichain (pairwise concurrent)
        /// and non-empty.
        #[test]
        fn heads_are_a_nonempty_antichain(e in arb_entry()) {
            prop_assert!(!e.heads().is_empty());
            let heads = e.heads();
            for (i, h) in heads.iter().enumerate() {
                for other in &heads[i + 1..] {
                    prop_assert_eq!(
                        h.version.compare(&other.version),
                        Causality::Concurrent
                    );
                }
            }
        }

        /// Absorb is idempotent: re-absorbing every current head is a no-op.
        #[test]
        fn absorb_is_idempotent(e in arb_entry()) {
            let mut again = e.clone();
            for h in e.heads() {
                let out = again.absorb(h.version.clone(), h.state);
                prop_assert_eq!(out, AbsorbOutcome::AlreadyKnown);
            }
            prop_assert!(e.canonical_bytes_eq(&again));
        }

        /// Absorb is order-insensitive: folding the same head multiset in two
        /// different orders yields byte-identical entries (join commutativity /
        /// associativity — the core of convergence).
        #[test]
        fn absorb_is_order_insensitive(
            heads in proptest::collection::vec(arb_head(), 1..6),
            rot in 0usize..8,
        ) {
            let build = |order: &[(VectorClock, EntryState)]| {
                let mut it = order.iter();
                let (v0, s0) = it.next().expect("non-empty");
                let mut e = Entry::single(v0.clone(), *s0);
                for (v, s) in it {
                    e.absorb(v.clone(), *s);
                }
                e
            };
            let forward = build(&heads);
            // A cheap deterministic reorder: rotate by a generated offset, then
            // reverse — enough to exercise a different fold order.
            let mut shuffled = heads.clone();
            let offset = rot % shuffled.len();
            shuffled.rotate_left(offset);
            shuffled.reverse();
            let other = build(&shuffled);
            prop_assert!(forward.canonical_bytes_eq(&other));
        }
    }
}
