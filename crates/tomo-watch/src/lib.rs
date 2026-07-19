//! Filesystem watcher adapter for Tomo: raw platform events → canonical
//! [`LocalChange`](tomo_engine::LocalChange) records for `tomo-engine`.
//!
//! # Layers (from pure to I/O)
//! - [`canon`] — the **pure** canonicalizer: relativize, validate, ignore-filter,
//!   and reduce raw events to `Dirty`/`Gone` outcomes. No filesystem access, so
//!   editor save patterns are tested with synthetic event sequences
//!   (`CLAUDE.md`: no real-FS watcher unit tests).
//! - [`sig`] — re-stat + BLAKE3 hashing that turns a pending outcome into a
//!   concrete [`LocalChange`](tomo_engine::LocalChange).
//! - [`scan`] — a full-tree diff against the engine index for startup and
//!   overflow recovery.
//! - [`watcher`] — the thin `notify` adapter tying them together and emitting
//!   [`WatchSignal`]s on a channel.
//!
//! # Invariants honored here
//! - **#1** `.tomo/**` is dropped at the lowest layer: it is unrepresentable as
//!   a [`RelPath`](tomo_engine::RelPath), so the canonicalizer and scanner both
//!   silently exclude it — not via config, but structurally.
//! - **#3** Sync latency is never traded for coalescing: the canonicalizer emits
//!   each distinct change immediately and only suppresses exact consecutive
//!   duplicates; it never defers a change waiting for a later event.
//!
//! Echo suppression is **not** in this crate — it belongs to the engine's write
//! journal (`docs/SPEC.md` §5.1). This crate's contract is: watch, filter,
//! canonicalize, hash.

pub mod canon;
pub mod error;
pub mod norm;
pub mod scan;
pub mod sig;
pub mod watcher;

pub use canon::{Canonicalizer, PendingChange, PendingKind, RawEvent, RawKind};
pub use error::WatchError;
pub use norm::{canonicalize_fs_path, to_nfc};
pub use scan::scan_diff;
pub use sig::{resolve, snapshot};
pub use watcher::{map_event, WatchSignal, Watcher};
