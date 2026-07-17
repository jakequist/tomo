//! Pure sync engine: state machine, vector clocks, conflict resolution,
//! history pressure controller.
//!
//! # Purity contract (CLAUDE.md invariant #6)
//! This crate performs **no I/O**: no filesystem, no network, no threads, no
//! wall clocks. It is a pure function of its inputs so it can be tested
//! exhaustively with simulated event streams and deterministic time. Adapter
//! crates (`tomo-watch`, `tomo-transport`, `tomo-history`) feed it events and
//! execute the actions it emits.
//!
//! The module below is a seed: a minimal vector clock with the comparison
//! semantics the whole design hangs on. It exists to (a) anchor the purity
//! contract in code and (b) demonstrate the expected TDD style. Extend it
//! test-first (see `docs/TESTING.md` for the required property tests).

pub mod clock;

pub use clock::{Causality, ReplicaId, VectorClock};
