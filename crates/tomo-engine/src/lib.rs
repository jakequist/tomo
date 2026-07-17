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
//! # M0 foundations
//! The types below are the data model the M1 transition function will operate
//! over:
//! - [`VectorClock`] / [`ReplicaId`] / [`Causality`] — the *only* ordering
//!   authority (invariant #7); wall time is never consulted.
//! - [`RelPath`] — a validated, normalized, repo-relative path. A `.tomo`
//!   path is unrepresentable, enforcing invariant #1 at the type level.
//! - [`Index`] and its parts ([`Entry`], [`EntryState`], [`ContentSig`],
//!   [`ContentHash`]) — the authoritative view of the tree, with tombstones
//!   for deletions and a deterministic canonical digest.
//! - [`LocalChange`] / [`RemoteChange`] / [`ChangeKind`] — the canonical
//!   change events adapters emit into the engine.
//!
//! # M1 transition function
//! [`Engine::handle`] is the pure `(index, event) → (index', actions)` state
//! machine — vector-clock ordering, deterministic conflict resolution, and
//! engine-owned echo suppression. Adapters feed it [`Event`]s and execute the
//! [`Action`]s it returns.

pub mod clock;
pub mod engine;
pub mod event;
pub mod index;
pub mod path;
pub mod pressure;

pub use clock::{Causality, ReplicaId, VectorClock};
pub use engine::{Action, Engine, Event, Expectation};
pub use event::{ChangeKind, LocalChange, RemoteChange};
pub use index::{AbsorbOutcome, ContentHash, ContentSig, Entry, EntryState, Head, Index};
pub use path::{PathError, RelPath};
pub use pressure::{
    CaptureDecision, CaptureInput, HistoryMode, PressureConfig, PressureController, StagedCapture,
};
