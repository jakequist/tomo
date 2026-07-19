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
//! - **Symlink write-escape safety** ([`check_parents`]): a write or deletion is
//!   only ever performed through *real directories* inside the project root. A
//!   parent component that is a symlink — even one pointing back inside the root
//!   — is refused (OpenSSH/rsync posture), so a peer (or anything that planted a
//!   symlink) can never make an apply land outside the tree.
//! - **File↔dir type-collision detection** ([`type_collision`]): a path that
//!   flips between file and directory is a real change; the applier reports the
//!   collision so the session can resolve it (directory wins, file preserved to
//!   history — docs/SPEC.md §5.4).

use std::path::{Component, Path, PathBuf};

use tomo_engine::{ContentSig, RelPath};

use crate::error::CliError;
use crate::fsutil::atomic_write_mode;

/// Why a write or deletion at a target path was refused as unsafe.
///
/// Every variant is **non-fatal** (invariant #5): the session notes the reason
/// and schedules a reconciling rescan; the sync session never dies. Rendered
/// for the user via [`ApplyRefused::message`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyRefused {
    /// An existing parent component is a symlink. Tomo writes and deletes only
    /// through real directories, so a symlinked parent — whether or not it
    /// escapes the project — is refused rather than traversed. `escapes`
    /// records whether the link resolves outside the project root.
    SymlinkParent {
        /// The offending component, relative to the project root.
        component: String,
        /// The symlink's immediate target (`readlink`).
        dest: String,
        /// Whether the link resolves to a location outside the project root.
        escapes: bool,
    },
    /// The deepest existing ancestor of the target canonicalizes to a location
    /// outside the project root — a belt-and-suspenders escape check layered on
    /// top of the per-component symlink guard.
    EscapesRoot {
        /// The ancestor that resolved outside, relative to the project root.
        component: String,
        /// The absolute location it resolved to.
        resolved: String,
    },
}

impl ApplyRefused {
    /// A one-line, user-facing explanation naming the offending component.
    #[must_use]
    pub fn message(&self, path: &RelPath) -> String {
        match self {
            ApplyRefused::SymlinkParent {
                component,
                dest,
                escapes: true,
            } => format!(
                "refused {path}: parent '{component}' is a symlink leaving the project (→ {dest})"
            ),
            ApplyRefused::SymlinkParent {
                component,
                dest,
                escapes: false,
            } => format!(
                "refused {path}: parent '{component}' is a symlink \
                 (writes go through real directories only; → {dest})"
            ),
            ApplyRefused::EscapesRoot {
                component,
                resolved,
            } => format!(
                "refused {path}: parent '{component}' resolves outside the project (→ {resolved})"
            ),
        }
    }
}

/// A file↔dir type collision found when preparing an apply (docs/SPEC.md §5.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeCollision {
    /// The target path itself exists on disk as a directory, so a file cannot be
    /// renamed over it. The directory wins (it may hold synced descendants).
    TargetIsDir,
    /// An existing parent component is a regular file, so the directory the
    /// target needs cannot be created through it. `ancestor` is that file
    /// (absolute path). The directory wins; the file is preserved + cleared.
    ParentIsFile {
        /// The obstructing ancestor file (absolute path).
        ancestor: PathBuf,
    },
}

/// The repo-relative components of `full` under `root` (normal components only),
/// or `None` if `full` does not lie under `root` or contains an exotic
/// component. Shared by the two guards below.
fn rel_components<'a>(root: &Path, full: &'a Path) -> Option<Vec<&'a std::ffi::OsStr>> {
    let rel = full.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for comp in rel.components() {
        match comp {
            Component::Normal(os) => parts.push(os),
            // `..`, a root, or a prefix means this is not a plain in-tree path.
            _ => return None,
        }
    }
    Some(parts)
}

/// Verify every EXISTING parent component of `full` (i.e. excluding the final
/// component itself) is a real directory inside the canonicalized `root`.
///
/// This is the symlink write-escape guard (docs/NOTES.md edge-case 4): a write
/// or deletion must only go *through real directories*, so a symlinked parent —
/// even one pointing back inside the root — is refused (OpenSSH/rsync posture).
/// The **final** component is deliberately not checked: a symlink there is
/// simply replaced by the atomic rename (it swaps the link, not its target).
///
/// The check is twofold: (1) a per-component `lstat` walk refusing any symlink
/// parent, then (2) canonicalizing the deepest existing ancestor and requiring
/// it to stay within the root. A parent that does not exist yet stops the walk
/// early — a fresh `mkdir` there is safe.
///
/// # Errors
/// [`ApplyRefused`] naming the first offending component; never an I/O error
/// (stat failures other than "not found" conservatively stop the walk clean).
pub fn check_parents(root: &Path, full: &Path) -> Result<(), ApplyRefused> {
    let croot = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let Some(parts) = rel_components(root, full) else {
        return Ok(());
    };
    if parts.is_empty() {
        return Ok(());
    }
    // Walk parents only (all but the final component).
    let mut cur = root.to_path_buf();
    for os in &parts[..parts.len() - 1] {
        cur.push(os);
        let meta = match std::fs::symlink_metadata(&cur) {
            Ok(meta) => meta,
            // Deeper components cannot exist either; a fresh mkdir here is safe.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            // An unexpected stat error: do not guess, stop the walk (the write
            // itself will surface any real failure).
            Err(_) => return Ok(()),
        };
        if meta.file_type().is_symlink() {
            let dest = std::fs::read_link(&cur)
                .map(|d| d.display().to_string())
                .unwrap_or_default();
            let escapes = !std::fs::canonicalize(&cur).is_ok_and(|c| c.starts_with(&croot));
            return Err(ApplyRefused::SymlinkParent {
                component: cur.strip_prefix(root).unwrap_or(&cur).display().to_string(),
                dest,
                escapes,
            });
        }
    }
    // Belt-and-suspenders: the deepest EXISTING ancestor must canonicalize
    // within the root (catches any escape the per-component walk missed).
    let mut anc = full.parent();
    while let Some(a) = anc {
        match std::fs::canonicalize(a) {
            Ok(c) => {
                if !c.starts_with(&croot) {
                    return Err(ApplyRefused::EscapesRoot {
                        component: a.strip_prefix(root).unwrap_or(a).display().to_string(),
                        resolved: c.display().to_string(),
                    });
                }
                break;
            }
            // This ancestor does not exist yet; check a shallower one.
            Err(_) => anc = a.parent(),
        }
    }
    Ok(())
}

/// Detect a file↔dir type collision at `full` (docs/SPEC.md §5.4, edge-case 5).
///
/// Returns [`TypeCollision::TargetIsDir`] when the target path is itself a
/// directory (a file cannot be renamed over it), or
/// [`TypeCollision::ParentIsFile`] when an existing parent component is a
/// regular file (the directory the target needs cannot be created through it).
/// `None` means the target is clear to write. Symlink parents are *not* reported
/// here — they are [`check_parents`]'s concern and must be checked first.
#[must_use]
pub fn type_collision(root: &Path, full: &Path) -> Option<TypeCollision> {
    if let Ok(meta) = std::fs::symlink_metadata(full) {
        if meta.file_type().is_dir() {
            return Some(TypeCollision::TargetIsDir);
        }
    }
    let parts = rel_components(root, full)?;
    if parts.is_empty() {
        return None;
    }
    let mut cur = root.to_path_buf();
    for os in &parts[..parts.len() - 1] {
        cur.push(os);
        match std::fs::symlink_metadata(&cur) {
            Ok(meta) if meta.file_type().is_file() => {
                return Some(TypeCollision::ParentIsFile { ancestor: cur });
            }
            // A real directory (keep walking) or a symlink (check_parents' job).
            Ok(_) => {}
            // Not created yet: mkdir will make a clean directory here.
            Err(_) => return None,
        }
    }
    None
}

/// Whether `full` exists on disk as a directory (used to refuse deleting a
/// directory on a file-removal — docs/SPEC.md §5.4).
#[must_use]
pub fn path_is_dir(full: &Path) -> bool {
    std::fs::symlink_metadata(full).is_ok_and(|m| m.file_type().is_dir())
}

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
/// directories, then stages and atomically renames the file into place with the
/// final Unix mode dictated by `expected.exec` (`0o755` executable / `0o644`
/// otherwise — the executable bit is part of the signature, git's model).
///
/// # Errors
/// [`CliError::Message`] if `bytes` do not match `expected` (integrity failure);
/// [`CliError::Refused`] (non-fatal) if a parent component is a symlink or the
/// path escapes the root ([`check_parents`]); [`CliError::Io`] if a directory or
/// the atomic write fails.
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
    // Symlink write-escape guard: never create parents / rename *through* a
    // symlinked parent (invariant #5 — a refusal is non-fatal).
    if let Err(refused) = check_parents(root, &full) {
        return Err(CliError::Refused(refused.message(rel)));
    }
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|s| CliError::io("create parent directory", parent, s))?;
    }
    atomic_write_mode(staging, &full, bytes, expected.exec)
}

/// Bring the Unix mode of the file at `rel` into line with `exec` (`0o755` /
/// `0o644`) **without rewriting its bytes** — used when the content already on
/// disk is correct but only the executable bit changed (a chmod-only change
/// whose apply would otherwise be skipped). A no-op off Unix, and when the file
/// is absent (a raced deletion — the follow-up event reconciles it).
///
/// # Errors
/// [`CliError::Io`] if the file exists but its mode cannot be set.
#[cfg(unix)]
pub fn set_exec_mode(root: &Path, rel: &RelPath, exec: bool) -> Result<(), CliError> {
    use std::os::unix::fs::PermissionsExt as _;
    let full = join(root, rel);
    let mode = if exec { 0o755 } else { 0o644 };
    match std::fs::set_permissions(&full, std::fs::Permissions::from_mode(mode)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(CliError::io("set file mode", &full, source)),
    }
}

/// Non-Unix stub: no executable bit to enforce.
#[cfg(not(unix))]
pub fn set_exec_mode(_root: &Path, _rel: &RelPath, _exec: bool) -> Result<(), CliError> {
    Ok(())
}

/// Apply an "absent" (deleted) state at `rel`: remove the file (a missing file
/// is fine) and prune now-empty parent directories, stopping at the project
/// root and never touching `.tomo/` (unreachable via [`RelPath`]).
///
/// # Errors
/// [`CliError::Refused`] (non-fatal) if a parent component is a symlink or the
/// path escapes the root ([`check_parents`]); [`CliError::Io`] if the removal
/// fails for a reason other than "not found".
pub fn apply_absent(root: &Path, rel: &RelPath) -> Result<(), CliError> {
    let full = join(root, rel);
    // Never remove *through* a symlinked parent (it could delete outside the
    // tree). A refusal is non-fatal — the session notes it and rescans.
    if let Err(refused) = check_parents(root, &full) {
        return Err(CliError::Refused(refused.message(rel)));
    }
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
            exec: false,
        }
    }

    fn sig_exec(bytes: &[u8]) -> ContentSig {
        ContentSig {
            exec: true,
            ..sig_of(bytes)
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

    #[cfg(unix)]
    #[test]
    fn apply_present_sets_the_executable_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let staging = staging_in(dir.path());
        let bytes = b"#!/bin/sh\n";
        // Apply as executable → 0o755.
        apply_present(
            dir.path(),
            &staging,
            &rel("run.sh"),
            &sig_exec(bytes),
            bytes,
        )
        .unwrap();
        let m = std::fs::metadata(dir.path().join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(m & 0o777, 0o755);

        // Re-apply the identical bytes as non-executable → mode drops to 0o644.
        apply_present(dir.path(), &staging, &rel("run.sh"), &sig_of(bytes), bytes).unwrap();
        let m = std::fs::metadata(dir.path().join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(m & 0o777, 0o644);
    }

    #[cfg(unix)]
    #[test]
    fn set_exec_mode_flips_the_bit_without_touching_bytes() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let staging = staging_in(dir.path());
        let bytes = b"payload";
        apply_present(dir.path(), &staging, &rel("f"), &sig_of(bytes), bytes).unwrap();

        set_exec_mode(dir.path(), &rel("f"), true).unwrap();
        let m = std::fs::metadata(dir.path().join("f"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(m & 0o777, 0o755);
        // Bytes are untouched by the chmod.
        assert_eq!(std::fs::read(dir.path().join("f")).unwrap(), bytes);
        // Enforcing on a missing path is a no-op, not an error.
        set_exec_mode(dir.path(), &rel("gone"), true).unwrap();
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

    // ---- Item A: symlink write-escape guard ([`check_parents`]) -----------

    #[cfg(unix)]
    #[test]
    fn check_parents_allows_a_plain_real_directory_chain() {
        // Control: every parent is a real directory → Ok, and a normal apply
        // through it succeeds.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        let full = dir.path().join("a/b/c.txt");
        assert_eq!(check_parents(dir.path(), &full), Ok(()));
    }

    #[cfg(unix)]
    #[test]
    fn check_parents_refuses_a_symlink_parent_pointing_outside_root() {
        // `link` → an out-of-root directory. Writing `link/evil.txt` would land
        // OUTSIDE the project — the classic tar/rsync escape. It is refused, and
        // flagged as escaping.
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();

        let full = root.path().join("link/evil.txt");
        let err = check_parents(root.path(), &full).unwrap_err();
        match err {
            ApplyRefused::SymlinkParent {
                component, escapes, ..
            } => {
                assert_eq!(component, "link");
                assert!(escapes, "an out-of-root link must be flagged as escaping");
            }
            ApplyRefused::EscapesRoot { .. } => panic!("expected SymlinkParent, got EscapesRoot"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn check_parents_refuses_an_in_root_symlink_parent() {
        // Even an IN-ROOT symlink parent is refused: writes go through real
        // directories only (OpenSSH/rsync posture). `escapes` is false.
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("real")).unwrap();
        std::os::unix::fs::symlink(root.path().join("real"), root.path().join("link")).unwrap();

        let full = root.path().join("link/inner.txt");
        let err = check_parents(root.path(), &full).unwrap_err();
        match err {
            ApplyRefused::SymlinkParent {
                component, escapes, ..
            } => {
                assert_eq!(component, "link");
                assert!(!escapes, "an in-root link must not be flagged as escaping");
            }
            ApplyRefused::EscapesRoot { .. } => panic!("expected SymlinkParent, got EscapesRoot"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn check_parents_refuses_a_deep_symlink_chain() {
        // A symlink several levels down is still caught, naming that component.
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("a/b")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("a/b/esc")).unwrap();

        let full = root.path().join("a/b/esc/deep/file.txt");
        let err = check_parents(root.path(), &full).unwrap_err();
        match err {
            ApplyRefused::SymlinkParent { component, .. } => assert_eq!(component, "a/b/esc"),
            ApplyRefused::EscapesRoot { .. } => panic!("expected SymlinkParent, got EscapesRoot"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn check_parents_allows_a_symlink_at_the_final_component() {
        // A symlink AT the target path itself is fine — the rename replaces the
        // link, it does not write through it. So check_parents (which guards
        // parents only) returns Ok.
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("real.txt"), b"real").unwrap();
        std::os::unix::fs::symlink(root.path().join("real.txt"), root.path().join("link.txt"))
            .unwrap();

        let full = root.path().join("link.txt");
        assert_eq!(check_parents(root.path(), &full), Ok(()));
    }

    #[cfg(unix)]
    #[test]
    fn apply_present_over_a_symlink_at_final_path_replaces_the_link_not_the_target() {
        // Applying to a path that is itself a symlink replaces the LINK (atomic
        // rename), leaving the link's original target untouched.
        let root = tempfile::tempdir().unwrap();
        let staging = staging_in(root.path());
        std::fs::write(root.path().join("target.txt"), b"original-target").unwrap();
        std::os::unix::fs::symlink(root.path().join("target.txt"), root.path().join("link.txt"))
            .unwrap();

        let bytes = b"applied-content";
        apply_present(
            root.path(),
            &staging,
            &rel("link.txt"),
            &sig_of(bytes),
            bytes,
        )
        .unwrap();

        // link.txt is now a regular file holding the applied bytes...
        let meta = std::fs::symlink_metadata(root.path().join("link.txt")).unwrap();
        assert!(
            meta.file_type().is_file(),
            "the link was replaced by a file"
        );
        assert_eq!(std::fs::read(root.path().join("link.txt")).unwrap(), bytes);
        // ...and the link's former target is untouched (no write escaped through).
        assert_eq!(
            std::fs::read(root.path().join("target.txt")).unwrap(),
            b"original-target"
        );
    }

    #[cfg(unix)]
    #[test]
    fn apply_present_through_a_symlink_parent_is_refused_non_fatally() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let staging = staging_in(root.path());
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();

        let bytes = b"escape";
        let err = apply_present(
            root.path(),
            &staging,
            &rel("link/evil.txt"),
            &sig_of(bytes),
            bytes,
        )
        .unwrap_err();
        assert!(
            matches!(err, CliError::Refused(_)),
            "must be a non-fatal refusal"
        );
        // Nothing landed outside the tree.
        assert!(!outside.path().join("evil.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn apply_absent_through_a_symlink_parent_is_refused_and_deletes_nothing_outside() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("victim.txt"), b"do not delete me").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();

        let err = apply_absent(root.path(), &rel("link/victim.txt")).unwrap_err();
        assert!(matches!(err, CliError::Refused(_)));
        // The out-of-tree file survives.
        assert!(outside.path().join("victim.txt").exists());
    }

    // ---- Item B: file↔dir type-collision detection ([`type_collision`]) ---

    #[test]
    fn type_collision_none_for_a_clear_target() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        assert_eq!(
            type_collision(dir.path(), &dir.path().join("a/b/c.txt")),
            None
        );
        // A brand-new path with no existing parents is also clear.
        assert_eq!(
            type_collision(dir.path(), &dir.path().join("x/y/z.txt")),
            None
        );
    }

    #[test]
    fn type_collision_flags_a_directory_at_the_target_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("foo/child")).unwrap();
        // Applying a FILE at `foo` where `foo` is a directory.
        assert_eq!(
            type_collision(dir.path(), &dir.path().join("foo")),
            Some(TypeCollision::TargetIsDir)
        );
    }

    #[test]
    fn type_collision_flags_a_file_parent_blocking_a_child() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("foo"), b"i am a file").unwrap();
        // Applying `foo/bar` where `foo` is a regular file.
        match type_collision(dir.path(), &dir.path().join("foo/bar")) {
            Some(TypeCollision::ParentIsFile { ancestor }) => {
                assert_eq!(ancestor, dir.path().join("foo"));
            }
            None | Some(TypeCollision::TargetIsDir) => panic!("expected ParentIsFile"),
        }
    }

    #[test]
    fn path_is_dir_distinguishes_dirs_from_files_and_absence() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        std::fs::write(dir.path().join("f"), b"x").unwrap();
        assert!(path_is_dir(&dir.path().join("d")));
        assert!(!path_is_dir(&dir.path().join("f")));
        assert!(!path_is_dir(&dir.path().join("missing")));
    }
}
