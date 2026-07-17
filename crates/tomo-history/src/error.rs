//! Error type for the history store.
//!
//! Every fallible operation in this crate returns [`HistoryError`]. Variants
//! carry the context (path, version, chunk hash) a caller — ultimately the
//! `tomo` CLI — needs to render an actionable message. Library code never
//! panics on a `Result`/`Option` (rust-hygiene): all failure modes are values
//! here.

use std::path::PathBuf;

use tomo_engine::path::RelPath;

/// A byte slice rendered as lowercase hex, for error messages.
pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // Writing to a String is infallible; hex of a byte is two chars.
        s.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('0'));
        s.push(char::from_digit(u32::from(b & 0x0f), 16).unwrap_or('0'));
    }
    s
}

/// Errors produced by [`crate::HistoryStore`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HistoryError {
    /// A filesystem operation (creating `.tomo/db`, opening the database file)
    /// failed.
    #[error("history I/O error at {path}: {source}")]
    Io {
        /// The path being operated on.
        path: PathBuf,
        /// The underlying OS error.
        source: std::io::Error,
    },

    /// An underlying `SQLite` operation failed.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// Serializing or deserializing a stored vector clock (postcard) failed.
    #[error("clock serialization error: {0}")]
    Clock(#[from] postcard::Error),

    /// [`crate::HistoryStore::record_version`] was called for a
    /// [`tomo_engine::EntryState::Present`] state without the file bytes, which
    /// are required to store content.
    #[error("cannot record a present version of {path} without content bytes")]
    MissingContent {
        /// The path whose version was being recorded.
        path: RelPath,
    },

    /// The bytes handed to [`crate::HistoryStore::record_version`] did not hash
    /// (or size) to the [`tomo_engine::ContentSig`] the caller declared — a
    /// caller contract violation caught before anything is persisted.
    #[error("content signature mismatch for {path}: declared {declared}, actual {actual}")]
    SigMismatch {
        /// The path whose version was being recorded.
        path: RelPath,
        /// The hash the caller declared (hex).
        declared: String,
        /// The hash the bytes actually produced (hex).
        actual: String,
    },

    /// A version row referenced by id does not exist.
    #[error("no such version: {0}")]
    NoSuchVersion(i64),

    /// [`crate::HistoryStore::get_content`] was asked for the content of a
    /// tombstone version, which has none.
    #[error("version {0} is a tombstone and has no content")]
    NotPresent(i64),

    /// A version's manifest referenced a chunk that is absent from the store.
    #[error("version {version} references missing chunk {hash}")]
    MissingChunk {
        /// The version whose manifest referenced the chunk.
        version: i64,
        /// The absent chunk's hash (hex).
        hash: String,
    },

    /// A stored chunk failed integrity verification: its compressed bytes did
    /// not decompress to content whose BLAKE3 hash matches its key.
    #[error("corrupt chunk {hash}: {detail}")]
    CorruptChunk {
        /// The chunk's key hash (hex).
        hash: String,
        /// What was wrong (decompression failure, hash/size mismatch).
        detail: String,
    },

    /// A reassembled file's whole-file BLAKE3 hash did not match the content
    /// hash recorded for the version — the retrieved bytes are not trustworthy.
    #[error("content hash mismatch for version {version}: expected {expected}, got {actual}")]
    ContentMismatch {
        /// The version being retrieved.
        version: i64,
        /// The content hash the version row recorded (hex).
        expected: String,
        /// The hash the reassembled bytes produced (hex).
        actual: String,
    },

    /// A stored value had an unexpected shape (e.g. a manifest whose length is
    /// not a multiple of 32, or a hash blob of the wrong length). Indicates
    /// database corruption or a schema/version skew.
    #[error("malformed stored data: {0}")]
    Malformed(String),
}

impl HistoryError {
    /// Build a [`HistoryError::MissingChunk`] from a raw 32-byte hash.
    pub(crate) fn missing_chunk(version: i64, hash: &[u8]) -> Self {
        HistoryError::MissingChunk {
            version,
            hash: hex(hash),
        }
    }

    /// Build a [`HistoryError::CorruptChunk`] from a raw 32-byte hash.
    pub(crate) fn corrupt_chunk(hash: &[u8], detail: impl Into<String>) -> Self {
        HistoryError::CorruptChunk {
            hash: hex(hash),
            detail: detail.into(),
        }
    }
}
