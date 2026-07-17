//! The crate's error type.
//!
//! Every fallible operation in `tomo-watch` returns a [`WatchError`]. As a
//! library crate it never prints: it returns structured errors carrying the
//! offending path and cause so the `tomo` CLI can render them (see the hygiene
//! policy in `CLAUDE.md`).

use std::path::PathBuf;

use tomo_engine::PathError;

/// Something that went wrong while watching, hashing, or scanning the tree.
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    /// A filesystem operation failed at a specific path.
    ///
    /// Carries the path so the caller can report *which* file could not be
    /// stat'd, read, or listed — a bare `io::Error` loses that context.
    #[error("I/O error at {path}: {source}")]
    Io {
        /// The path the failing operation touched.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// The platform watcher dropped events (e.g. inotify `IN_Q_OVERFLOW`).
    ///
    /// The event stream is no longer trustworthy; the caller must recover by
    /// running [`crate::scan_diff`] to reconcile the tree against the index
    /// (`docs/SPEC.md` §5.1). Surfaced on the channel as
    /// [`crate::WatchSignal::NeedsRescan`] rather than as a returned error.
    #[error("watcher event queue overflowed; a full rescan is required")]
    Overflow,

    /// A path could not be represented as a repo-relative [`tomo_engine::RelPath`].
    ///
    /// Reserved for callers that need to distinguish path-shape rejections from
    /// I/O failures; the canonicalizer itself silently *drops* such paths (an
    /// absolute escape, a `.tomo/**` path, or non-UTF-8 bytes is not an error,
    /// it is simply "not a tracked file").
    #[error("invalid repo-relative path: {0}")]
    Path(#[from] PathError),

    /// The platform watcher backend failed to start or register a watch.
    #[error("filesystem watcher backend failed: {0}")]
    Backend(#[from] notify::Error),
}
