//! Executing the engine's tree-mutating actions on disk.
//!
//! The engine decides *what* the tree should look like ([`tomo_engine::Action::Apply`]);
//! this module makes it so, with the crash-safety and integrity guarantees the
//! adapter is responsible for:
//! - **Staging + atomic rename** for every write (invariant #8) — a partially
//!   transferred file is never visible at its final path.
//! - **Integrity check**: received bytes must hash to the signature the engine
//!   expects, or the apply is a fatal protocol error (a corrupted/forged frame).
//! - **`.tomo` safety**: paths are [`RelPath`]s, which can never name `.tomo`,
//!   and deletion pruning stops at the project root.

use std::path::{Path, PathBuf};

use tomo_engine::{ContentSig, RelPath};

use crate::error::CliError;
use crate::fsutil::atomic_write;

/// Join a repo-relative [`RelPath`] onto `root`, component by component, so its
/// `/` separators are interpreted portably rather than as one opaque segment.
pub fn join(root: &Path, rel: &RelPath) -> PathBuf {
    let mut full = root.to_path_buf();
    for comp in rel.components() {
        full.push(comp);
    }
    full
}

/// Whether `bytes` match `sig` (size then BLAKE3 hash).
///
/// Used both to verify received content before applying it, and to decide
/// whether a queued `Send` still reflects the file on disk.
pub fn matches_sig(bytes: &[u8], sig: &ContentSig) -> bool {
    bytes.len() as u64 == sig.size && blake3::hash(bytes).as_bytes() == &sig.hash.0
}

/// Decide whether a `Send` for a `Modified` change should still ship.
///
/// The engine queued the send against a signature captured when the change was
/// observed. By the time we execute it the file may have changed again; if the
/// current bytes no longer hash to that signature we **drop** the send, because
/// the watcher's follow-up event will ship the newer state — invariant #3 ships
/// the latest bytes, never a stale snapshot. A vanished file (`None`) also
/// drops (its removal event is coming).
pub fn should_send(current: Option<&[u8]>, expected: &ContentSig) -> bool {
    matches!(current, Some(bytes) if matches_sig(bytes, expected))
}

/// Apply a "present with this content" state at `rel`.
///
/// Verifies `bytes` against `expected` (mismatch is fatal), creates the parent
/// directories, then stages and atomically renames the file into place.
///
/// # Errors
/// [`CliError::Message`] if `bytes` do not match `expected` (integrity failure);
/// [`CliError::Io`] if a directory or the atomic write fails.
pub fn apply_present(
    root: &Path,
    staging: &Path,
    rel: &RelPath,
    expected: &ContentSig,
    bytes: &[u8],
) -> Result<(), CliError> {
    if !matches_sig(bytes, expected) {
        return Err(CliError::msg(format!(
            "integrity check failed applying {rel}: received {} bytes hashing to {} \
             but expected {} bytes hashing to {}",
            bytes.len(),
            blake3::hash(bytes).to_hex(),
            expected.size,
            expected.hash,
        )));
    }
    let full = join(root, rel);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|s| CliError::io("create parent directory", parent, s))?;
    }
    atomic_write(staging, &full, bytes)
}

/// Apply an "absent" (deleted) state at `rel`: remove the file (a missing file
/// is fine) and prune now-empty parent directories, stopping at the project
/// root and never touching `.tomo/` (unreachable via [`RelPath`]).
///
/// # Errors
/// [`CliError::Io`] if the removal fails for a reason other than "not found".
pub fn apply_absent(root: &Path, rel: &RelPath) -> Result<(), CliError> {
    let full = join(root, rel);
    match std::fs::remove_file(&full) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(CliError::io("remove file", &full, source)),
    }
    prune_empty_parents(root, &full);
    Ok(())
}

/// Remove empty ancestor directories of `full`, from its parent upward, stopping
/// at (and never removing) `root`. `remove_dir` only succeeds on an empty
/// directory, so a non-empty ancestor naturally halts the walk.
fn prune_empty_parents(root: &Path, full: &Path) {
    let mut dir = full.parent();
    while let Some(d) = dir {
        if d == root || !d.starts_with(root) {
            break;
        }
        match std::fs::remove_dir(d) {
            Ok(()) => dir = d.parent(),
            // Non-empty (or otherwise unremovable): stop pruning here.
            Err(_) => break,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tomo_engine::ContentHash;

    fn sig_of(bytes: &[u8]) -> ContentSig {
        ContentSig {
            hash: ContentHash(*blake3::hash(bytes).as_bytes()),
            size: bytes.len() as u64,
        }
    }

    fn rel(s: &str) -> RelPath {
        RelPath::new(s).unwrap()
    }

    fn staging_in(dir: &Path) -> PathBuf {
        let s = dir.join(".tomo/staging");
        std::fs::create_dir_all(&s).unwrap();
        s
    }

    #[test]
    fn matches_sig_checks_size_and_hash() {
        assert!(matches_sig(b"hello", &sig_of(b"hello")));
        assert!(!matches_sig(b"hello!", &sig_of(b"hello")));
        // Same length, different content → different hash → no match.
        assert!(!matches_sig(b"world", &sig_of(b"hello")));
    }

    #[test]
    fn should_send_drops_stale_and_missing() {
        let sig = sig_of(b"v2");
        assert!(should_send(Some(b"v2"), &sig)); // current == expected
        assert!(!should_send(Some(b"v3-newer"), &sig)); // changed again
        assert!(!should_send(None, &sig)); // vanished
    }

    #[test]
    fn apply_present_writes_via_staging_and_creates_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let staging = staging_in(dir.path());
        let bytes = b"deep content";
        apply_present(
            dir.path(),
            &staging,
            &rel("a/b/c.txt"),
            &sig_of(bytes),
            bytes,
        )
        .unwrap();
        assert_eq!(std::fs::read(dir.path().join("a/b/c.txt")).unwrap(), bytes);
        // Staging left clean.
        assert_eq!(std::fs::read_dir(&staging).unwrap().count(), 0);
    }

    #[test]
    fn apply_present_rejects_hash_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let staging = staging_in(dir.path());
        // Expected signature is for different bytes than we hand it.
        let err = apply_present(
            dir.path(),
            &staging,
            &rel("f.txt"),
            &sig_of(b"expected"),
            b"actually different",
        )
        .unwrap_err();
        assert!(matches!(err, CliError::Message(_)));
        // Nothing was written.
        assert!(!dir.path().join("f.txt").exists());
    }

    #[test]
    fn apply_absent_removes_and_prunes_empty_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let staging = staging_in(dir.path());
        let bytes = b"x";
        apply_present(
            dir.path(),
            &staging,
            &rel("a/b/c.txt"),
            &sig_of(bytes),
            bytes,
        )
        .unwrap();

        apply_absent(dir.path(), &rel("a/b/c.txt")).unwrap();
        assert!(!dir.path().join("a/b/c.txt").exists());
        // Empty a/b and a pruned away.
        assert!(!dir.path().join("a/b").exists());
        assert!(!dir.path().join("a").exists());
        // Root survives.
        assert!(dir.path().exists());
    }

    #[test]
    fn apply_absent_keeps_nonempty_parents() {
        let dir = tempfile::tempdir().unwrap();
        let staging = staging_in(dir.path());
        apply_present(
            dir.path(),
            &staging,
            &rel("a/keep.txt"),
            &sig_of(b"k"),
            b"k",
        )
        .unwrap();
        apply_present(
            dir.path(),
            &staging,
            &rel("a/drop.txt"),
            &sig_of(b"d"),
            b"d",
        )
        .unwrap();

        apply_absent(dir.path(), &rel("a/drop.txt")).unwrap();
        // Sibling keeps the directory alive.
        assert!(dir.path().join("a/keep.txt").exists());
        assert!(dir.path().join("a").exists());
    }

    #[test]
    fn apply_absent_missing_file_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        apply_absent(dir.path(), &rel("never/existed.txt")).unwrap();
    }
}
