//! Content-addressed history store and `SQLite` metadata (docs/SPEC.md §6).
//!
//! This adapter crate owns Tomo's history: a content-addressed store built on
//! `FastCDC` content-defined chunking, BLAKE3 content identity, and zstd
//! compression, with all chunk BLOBs and version/conflict metadata in a single
//! `SQLite` database at `<project_root>/.tomo/db/history.sqlite`.
//!
//! # Invariants upheld here
//! - **#2 (no global state):** everything lives under `<root>/.tomo/db/`.
//! - **#7 (never trust wall clocks):** ordering is carried by the stored vector
//!   clock; the `wall_ms` field is display-only.
//! - **#8 (crash safety):** every write is a `SQLite` transaction, so a
//!   `kill -9` mid-write cannot tear the tree or leave a dangling chunk.
//!
//! The store is I/O-bearing (unlike `tomo-engine`) but takes no ordering
//! decisions: it stores what the engine decided and reconstructs bytes on
//! demand, verifying integrity end to end.
//!
//! # Example
//! ```
//! use tomo_history::{HistoryStore, Origin};
//! use tomo_engine::{ContentSig, EntryState, RelPath, ReplicaId, VectorClock};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let dir = tempfile::tempdir()?;
//! let mut store = HistoryStore::open(dir.path())?;
//!
//! let bytes = b"hello, tomo";
//! let (hash, _new) = store.store_content(bytes)?;
//!
//! let path = RelPath::new("greeting.txt")?;
//! let mut clock = VectorClock::new();
//! clock.tick(ReplicaId(1));
//! let sig = ContentSig { hash, size: bytes.len() as u64, exec: false, mtime_ms: 0 };
//! let id = store.record_version(
//!     &path,
//!     &EntryState::Present(sig),
//!     &clock,
//!     ReplicaId(1),
//!     Origin::Local,
//!     0,
//!     Some(bytes),
//! )?;
//!
//! assert_eq!(store.get_content(id)?, bytes);
//! # Ok(())
//! # }
//! ```

mod error;
mod store;

pub use error::HistoryError;
pub use store::{
    chunk_bytes, CheckReport, ConflictId, ConflictRecord, HistoryStore, Origin, VersionId,
    VersionMeta,
};
