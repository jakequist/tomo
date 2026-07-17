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
/// Fidelity for symlinks and permissions is an explicit open question in
/// `docs/SPEC.md` §12; until it is resolved they are simply not tracked.
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
    }))
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
