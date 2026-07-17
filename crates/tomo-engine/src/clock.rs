//! Vector clocks: the only ordering authority in Tomo.
//!
//! Wall-clock time is never used for ordering decisions (CLAUDE.md invariant
//! #7). Comparing two clocks yields a [`Causality`], and `Concurrent` is what
//! makes something a conflict.

use std::collections::BTreeMap;

/// Stable identifier for a replica (a machine participating in a sync pair).
///
/// Generated once at `tomo init`/first connect and persisted in `.tomo/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReplicaId(pub u64);

/// Result of comparing two vector clocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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

    // TODO(M0): replace hand-picked cases with proptest properties — partial
    // order laws, merge commutativity/associativity, convergence. See
    // docs/TESTING.md Level 1.
}
