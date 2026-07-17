//! Vector clocks: the only ordering authority in Tomo.
//!
//! Wall-clock time is never used for ordering decisions (CLAUDE.md invariant
//! #7). Comparing two clocks yields a [`Causality`], and `Concurrent` is what
//! makes something a conflict.

use std::collections::BTreeMap;

/// Stable identifier for a replica (a machine participating in a sync pair).
///
/// Generated once at `tomo init`/first connect and persisted in `.tomo/`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct ReplicaId(pub u64);

/// Result of comparing two vector clocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Causality {
    /// The clocks are identical: same version, nothing to do.
    Equal,
    /// Self happened strictly before other: fast-forward apply other.
    Before,
    /// Self happened strictly after other: other is stale.
    After,
    /// Neither dominates: a true conflict requiring the deterministic
    /// last-writer-wins tiebreak (content hash, then replica id).
    Concurrent,
}

/// A vector clock: per-replica logical counters.
///
/// Sized for N replicas from day one even though v0 pairs are two-replica
/// (docs/SPEC.md §2).
///
/// The map holds only strictly-positive counters: `tick` inserts `≥1` and
/// `merge` takes pointwise maxima, so a missing replica means exactly "zero".
/// Two clocks that [`compare`](VectorClock::compare) as [`Causality::Equal`]
/// therefore hold identical maps, which is what makes the index's canonical
/// digest stable.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VectorClock {
    counters: BTreeMap<ReplicaId, u64>,
}

impl VectorClock {
    /// An empty clock (all counters implicitly zero).
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a local event at `replica`, incrementing its counter.
    pub fn tick(&mut self, replica: ReplicaId) {
        *self.counters.entry(replica).or_insert(0) += 1;
    }

    /// The counter for `replica` (zero if never ticked).
    pub fn get(&self, replica: ReplicaId) -> u64 {
        self.counters.get(&replica).copied().unwrap_or(0)
    }

    /// Iterate the stored `(replica, counter)` pairs in ascending replica
    /// order.
    ///
    /// Only strictly-positive counters are stored, so this never yields a
    /// zero. The deterministic order lets adapters encode a clock into a
    /// stable byte string (see [`crate::Index::canonical_bytes`]).
    pub fn iter(&self) -> impl Iterator<Item = (ReplicaId, u64)> + '_ {
        self.counters.iter().map(|(&r, &c)| (r, c))
    }

    /// Merge `other` into self (pointwise max). Used when applying a remote
    /// version we have accepted.
    pub fn merge(&mut self, other: &Self) {
        for (&r, &c) in &other.counters {
            let e = self.counters.entry(r).or_insert(0);
            if c > *e {
                *e = c;
            }
        }
    }

    /// Compare against `other`.
    pub fn compare(&self, other: &Self) -> Causality {
        let mut self_ahead = false;
        let mut other_ahead = false;
        for &r in self.counters.keys().chain(other.counters.keys()) {
            let (a, b) = (self.get(r), other.get(r));
            if a > b {
                self_ahead = true;
            }
            if b > a {
                other_ahead = true;
            }
        }
        match (self_ahead, other_ahead) {
            (false, false) => Causality::Equal,
            (false, true) => Causality::Before,
            (true, false) => Causality::After,
            (true, true) => Causality::Concurrent,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // fine in tests
mod tests {
    use super::*;

    const A: ReplicaId = ReplicaId(1);
    const B: ReplicaId = ReplicaId(2);

    #[test]
    fn fresh_clocks_are_equal() {
        assert_eq!(
            VectorClock::new().compare(&VectorClock::new()),
            Causality::Equal
        );
    }

    #[test]
    fn tick_establishes_happens_after() {
        let base = VectorClock::new();
        let mut later = base.clone();
        later.tick(A);
        assert_eq!(later.compare(&base), Causality::After);
        assert_eq!(base.compare(&later), Causality::Before);
    }

    #[test]
    fn independent_ticks_are_concurrent() {
        let (mut x, mut y) = (VectorClock::new(), VectorClock::new());
        x.tick(A);
        y.tick(B);
        assert_eq!(x.compare(&y), Causality::Concurrent);
        assert_eq!(y.compare(&x), Causality::Concurrent);
    }

    #[test]
    fn merge_dominates_both_inputs() {
        let (mut x, mut y) = (VectorClock::new(), VectorClock::new());
        x.tick(A);
        y.tick(B);
        let mut merged = x.clone();
        merged.merge(&y);
        assert_eq!(merged.compare(&x), Causality::After);
        assert_eq!(merged.compare(&y), Causality::After);
    }

    #[test]
    fn merge_is_idempotent() {
        let mut x = VectorClock::new();
        x.tick(A);
        x.tick(B);
        let mut m = x.clone();
        m.merge(&x);
        assert_eq!(m.compare(&x), Causality::Equal);
    }

    // ---- Property tests: vector-clock algebra (docs/TESTING.md Level 1) ----

    use proptest::prelude::*;

    /// Merge as a pure function: `a ⊔ b`, leaving inputs untouched.
    fn merged(a: &VectorClock, b: &VectorClock) -> VectorClock {
        let mut m = a.clone();
        m.merge(b);
        m
    }

    /// Strategy for small arbitrary clocks: up to 4 replicas, counters 0..=5.
    ///
    /// A `BTreeMap` input dedupes replica ids; counters are materialized by
    /// ticking, so the resulting clock only ever holds positive counters —
    /// matching the type's real-world invariant.
    fn arb_clock() -> impl Strategy<Value = VectorClock> {
        proptest::collection::btree_map(0u64..4, 0u64..6, 0..5).prop_map(|m| {
            let mut c = VectorClock::new();
            for (r, count) in m {
                for _ in 0..count {
                    c.tick(ReplicaId(r));
                }
            }
            c
        })
    }

    /// Strategy for a short list of replica ids to tick.
    fn arb_ticks() -> impl Strategy<Value = Vec<ReplicaId>> {
        proptest::collection::vec((0u64..4).prop_map(ReplicaId), 0..4)
    }

    proptest! {
        /// Reflexivity: every clock is `Equal` to itself.
        #[test]
        fn compare_reflexive(x in arb_clock()) {
            prop_assert_eq!(x.compare(&x), Causality::Equal);
        }

        /// Antisymmetry / duality: reversing the arguments flips the verdict
        /// exactly (`Before`⇔`After`, `Equal`/`Concurrent` self-dual).
        #[test]
        fn compare_dual(x in arb_clock(), y in arb_clock()) {
            let expected = match x.compare(&y) {
                Causality::Equal => Causality::Equal,
                Causality::Before => Causality::After,
                Causality::After => Causality::Before,
                Causality::Concurrent => Causality::Concurrent,
            };
            prop_assert_eq!(y.compare(&x), expected);
        }

        /// Transitivity of strict happens-before: built over an actual causal
        /// chain `a ≤ b ≤ c`, so the premise fires often.
        #[test]
        fn happens_before_transitive(
            a in arb_clock(),
            t1 in arb_ticks(),
            t2 in arb_ticks(),
        ) {
            let mut b = a.clone();
            for r in &t1 {
                b.tick(*r);
            }
            let mut c = b.clone();
            for r in &t2 {
                c.tick(*r);
            }
            if a.compare(&b) == Causality::Before && b.compare(&c) == Causality::Before {
                prop_assert_eq!(a.compare(&c), Causality::Before);
            }
        }

        /// Merge is commutative: `a ⊔ b == b ⊔ a`.
        #[test]
        fn merge_commutative(a in arb_clock(), b in arb_clock()) {
            prop_assert_eq!(
                merged(&a, &b).compare(&merged(&b, &a)),
                Causality::Equal
            );
        }

        /// Merge is associative: `(a ⊔ b) ⊔ c == a ⊔ (b ⊔ c)`.
        #[test]
        fn merge_associative(a in arb_clock(), b in arb_clock(), c in arb_clock()) {
            let left = merged(&merged(&a, &b), &c);
            let right = merged(&a, &merged(&b, &c));
            prop_assert_eq!(left.compare(&right), Causality::Equal);
        }

        /// Merge is idempotent: `a ⊔ a == a`.
        #[test]
        fn merge_idempotent(a in arb_clock()) {
            prop_assert_eq!(merged(&a, &a).compare(&a), Causality::Equal);
        }

        /// The merge (least upper bound) dominates both inputs: it is
        /// `After` or `Equal` to each.
        #[test]
        fn merge_dominates(a in arb_clock(), b in arb_clock()) {
            let m = merged(&a, &b);
            prop_assert!(matches!(m.compare(&a), Causality::After | Causality::Equal));
            prop_assert!(matches!(m.compare(&b), Causality::After | Causality::Equal));
        }

        /// A tick strictly advances the clock: the ticked clock is `After`
        /// the pre-tick clock (and the original is `Before` it).
        #[test]
        fn tick_strictly_advances(a in arb_clock(), r in (0u64..4).prop_map(ReplicaId)) {
            let mut ticked = a.clone();
            ticked.tick(r);
            prop_assert_eq!(ticked.compare(&a), Causality::After);
            prop_assert_eq!(a.compare(&ticked), Causality::Before);
        }
    }
}
