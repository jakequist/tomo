//! Filesystem re-stat + BLAKE3 hashing: [`PendingChange`] → [`LocalChange`].
//!
//! This is the one place in the pipeline that touches file *contents*. It is
//! kept separate from the pure [`crate::canon`] layer precisely so the
//! canonicalizer stays testable without a real filesystem; this module, being a
//! plain file reader, *is* unit-tested against `tempfile` fixtures.

use std::path::{Path, PathBuf};

use tomo_engine::{ChangeKind, ContentHash, ContentSig, LocalChange};

use crate::canon::{PendingChange, PendingKind};
use crate::error::WatchError;

/// Stat, read, and BLAKE3-hash the regular file at `root`/`rel`.
///
/// Returns:
/// - `Ok(Some(sig))` for a regular file that exists and was read;
/// - `Ok(None)` if the path does not exist (a deletion that raced the event),
///   or is not a regular file.
///
/// # Symlinks and directories (v0 policy)
/// Only regular files carry content Tomo versions, so symlinks and directories
/// resolve to `Ok(None)`. Metadata is read with [`std::fs::symlink_metadata`]
/// (an `lstat`, which does **not** follow the link), so a symlink is judged on
/// its own type — never followed — which also avoids symlink-cycle traversal.
/// Fidelity for symlinks and full permissions is an explicit open question in
/// `docs/SPEC.md` §12; the sole permission bit tracked in v0 is the Unix
/// user-execute bit (git's model), captured into
/// [`ContentSig::exec`](tomo_engine::ContentSig::exec).
///
/// # Executable bit
/// On Unix the `exec` field reflects the file's user-execute bit
/// (`mode & 0o100`); on non-Unix platforms it is always `false`. The bit is
/// read from the same `lstat` used for the type check, so a chmod-only change
/// yields a different signature than the pre-chmod one and thus a real change.
///
/// # Errors
/// [`WatchError::Io`] (carrying `root/rel`) if the file exists but its metadata
/// or bytes cannot be read.
pub fn snapshot(root: &Path, rel: &tomo_engine::RelPath) -> Result<Option<ContentSig>, WatchError> {
    let full = join(root, rel);
    let meta = match std::fs::symlink_metadata(&full) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(WatchError::Io { path: full, source }),
    };
    if !meta.file_type().is_file() {
        // Directory or symlink: not a versioned regular file (v0).
        return Ok(None);
    }
    let exec = is_executable(&meta);
    let mtime_ms = mtime_ms(&meta);
    let bytes = match std::fs::read(&full) {
        Ok(bytes) => bytes,
        // The file vanished between stat and read — treat as a raced deletion.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(WatchError::Io { path: full, source }),
    };
    let hash = blake3::hash(&bytes);
    Ok(Some(ContentSig {
        hash: ContentHash(*hash.as_bytes()),
        size: bytes.len() as u64,
        exec,
        mtime_ms,
    }))
}

/// Whether `meta` describes a file with the Unix user-execute bit set. Always
/// `false` off Unix (where the concept — and git's executable bit — do not
/// apply); the whole permissions surface stays `[open]` there (docs/SPEC.md §12).
#[cfg(unix)]
pub(crate) fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode() & 0o100 != 0
}

/// Non-Unix stub: no executable-bit concept, so never executable.
#[cfg(not(unix))]
pub(crate) fn is_executable(_meta: &std::fs::Metadata) -> bool {
    false
}

/// The file's modification time as nanoseconds since the Unix epoch, or `0` when
/// unavailable (a pre-epoch or unreadable mtime). Used as one half of the scan
/// cache's quick-check key ([`crate::scancache`]); `0` simply forces a hash, so
/// an unreadable mtime is always safe.
pub(crate) fn mtime_ns(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| u64::try_from(d.as_nanos()).ok())
        .unwrap_or(0)
}

/// The file's modification time as **milliseconds** since the Unix epoch, or `0`
/// when unavailable. Carried into [`ContentSig::mtime_ms`](tomo_engine::ContentSig::mtime_ms)
/// as metadata (never identity): it feeds only the genesis adoption tiebreak in
/// the engine, and milliseconds is ample resolution to order two humans' (or a
/// clone's) edits while staying comfortably inside `u64`. Never an ordering
/// authority for anything the vector clocks can decide (invariant #7).
pub(crate) fn mtime_ms(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// Resolve a [`PendingChange`] into a concrete [`LocalChange`] by consulting the
/// filesystem.
///
/// A [`PendingKind::Gone`] becomes [`ChangeKind::Removed`]. A
/// [`PendingKind::Dirty`] is re-stat'd: an existing regular file becomes
/// [`ChangeKind::Modified`] with its fresh signature, while a Dirty path that
/// turns out to be absent (a create-then-delete burst, or a rename's ambiguous
/// half) safely downgrades to [`ChangeKind::Removed`]. This self-correction is
/// why the canonicalizer can map ambiguous events to `Dirty` without risking a
/// phantom modification.
///
/// # Errors
/// [`WatchError::Io`] if the underlying [`snapshot`] read fails.
pub fn resolve(root: &Path, pending: &PendingChange) -> Result<LocalChange, WatchError> {
    let kind = match pending.kind {
        PendingKind::Gone => ChangeKind::Removed,
        PendingKind::Dirty => match snapshot(root, &pending.rel)? {
            Some(sig) => ChangeKind::Modified(sig),
            None => ChangeKind::Removed,
        },
    };
    Ok(LocalChange {
        path: pending.rel.clone(),
        kind,
    })
}

/// Join a repo-relative [`RelPath`](tomo_engine::RelPath) onto `root`
/// component-by-component, so its `/` separators are interpreted portably
/// rather than as a single opaque segment.
pub(crate) fn join(root: &Path, rel: &tomo_engine::RelPath) -> PathBuf {
    let mut full = root.to_path_buf();
    for comp in rel.components() {
        full.push(comp);
    }
    full
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // panics are fine in tests
mod tests {
    use super::*;
    use tomo_engine::RelPath;

    fn rel(s: &str) -> RelPath {
        RelPath::new(s).unwrap()
    }

    #[test]
    fn snapshot_hashes_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.txt"), b"hello").unwrap();

        let sig = snapshot(dir.path(), &rel("src/a.txt")).unwrap().unwrap();
        assert_eq!(sig.size, 5);
        assert_eq!(sig.hash, ContentHash(*blake3::hash(b"hello").as_bytes()));
        // A freshly written non-executable file reports exec = false.
        assert!(!sig.exec);
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_reads_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("build.sh");
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();

        // Non-executable first.
        let plain = snapshot(dir.path(), &rel("build.sh")).unwrap().unwrap();
        assert!(!plain.exec);

        // chmod +x → same bytes/hash, but exec now true (a distinct signature).
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let exec = snapshot(dir.path(), &rel("build.sh")).unwrap().unwrap();
        assert!(exec.exec);
        assert_eq!(exec.hash, plain.hash, "content hash is unchanged by chmod");
        assert_ne!(
            exec, plain,
            "the signatures differ (exec is part of identity)"
        );
    }

    #[test]
    fn snapshot_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(snapshot(dir.path(), &rel("nope.txt")).unwrap().is_none());
    }

    #[test]
    fn snapshot_directory_is_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        assert!(snapshot(dir.path(), &rel("d")).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_symlink_is_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real"), b"x").unwrap();
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
        // The symlink is judged on its own type (lstat) and is not followed.
        assert!(snapshot(dir.path(), &rel("link")).unwrap().is_none());
    }

    /// A named pipe (FIFO) in the tree must be treated exactly like any other
    /// non-regular file — skipped — and, crucially, `snapshot`/`resolve` must not
    /// *block* on it. Opening a FIFO for reading blocks until a writer appears, so
    /// a naive `read` would hang the whole session forever. `snapshot` decides on
    /// the `lstat` type alone (never opening the FIFO), so it returns quickly. The
    /// test guards against a regression with a real timeout: the work runs on a
    /// thread and the assertion fails if it does not finish promptly.
    #[cfg(unix)]
    #[test]
    fn fifo_is_skipped_without_blocking() {
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("spawn mkfifo");
        assert!(status.success(), "mkfifo failed");
        // No writer is ever opened, so anything that blocks on the FIFO hangs.

        let root = dir.path().to_path_buf();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let snap = snapshot(&root, &rel("pipe"));
            let res = resolve(
                &root,
                &PendingChange {
                    rel: rel("pipe"),
                    kind: PendingKind::Dirty,
                },
            );
            let _ = tx.send((snap, res));
        });
        let (snap, res) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("snapshot/resolve of a FIFO must not block");
        // Not a regular file → no signature.
        assert!(snap.unwrap().is_none(), "a FIFO yields no ContentSig");
        // A Dirty pending on a FIFO downgrades to Removed (it is not a versioned
        // regular file), never a hang or a spurious modification.
        assert_eq!(res.unwrap().kind, ChangeKind::Removed);
    }

    #[test]
    fn resolve_dirty_existing_is_modified() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), b"data").unwrap();
        let change = resolve(
            dir.path(),
            &PendingChange {
                rel: rel("f"),
                kind: PendingKind::Dirty,
            },
        )
        .unwrap();
        assert_eq!(change.path, rel("f"));
        match change.kind {
            ChangeKind::Modified(sig) => assert_eq!(sig.size, 4),
            ChangeKind::Removed => panic!("expected Modified, got Removed"),
        }
    }

    /// A `Dirty` pending whose path is a **symlink** resolves to `Removed`
    /// (docs/SPEC.md §5.4 "File→symlink replacement"): a file becoming a symlink
    /// is observed as a deletion, because `snapshot` returns `None` for a
    /// non-regular file and a Dirty with no signature downgrades to a removal.
    #[cfg(unix)]
    #[test]
    fn resolve_dirty_symlink_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real"), b"x").unwrap();
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("was_a_file")).unwrap();
        let change = resolve(
            dir.path(),
            &PendingChange {
                rel: rel("was_a_file"),
                kind: PendingKind::Dirty,
            },
        )
        .unwrap();
        assert_eq!(change.kind, ChangeKind::Removed);
    }

    #[test]
    fn resolve_dirty_missing_downgrades_to_removed() {
        let dir = tempfile::tempdir().unwrap();
        let change = resolve(
            dir.path(),
            &PendingChange {
                rel: rel("ghost"),
                kind: PendingKind::Dirty,
            },
        )
        .unwrap();
        assert_eq!(change.kind, ChangeKind::Removed);
    }

    #[test]
    fn resolve_gone_is_removed_without_touching_disk() {
        // Even with no such file, Gone resolves to Removed.
        let dir = tempfile::tempdir().unwrap();
        let change = resolve(
            dir.path(),
            &PendingChange {
                rel: rel("whatever"),
                kind: PendingKind::Gone,
            },
        )
        .unwrap();
        assert_eq!(change.kind, ChangeKind::Removed);
    }
}
