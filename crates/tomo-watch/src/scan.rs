//! Startup / recovery scan: diff the on-disk tree against the engine's index.
//!
//! Two situations need a full walk rather than the live event stream:
//! - **Startup**, to catch changes made while Tomo was not running.
//! - **Overflow recovery**, when the platform watcher drops events
//!   ([`crate::WatchSignal::NeedsRescan`], `docs/SPEC.md` §5.1).
//!
//! [`scan_diff`] produces the same [`LocalChange`] vocabulary the live path
//! emits, so the engine ingests both identically.

use std::path::Path;

use tomo_config::{Config, PathClass};
use tomo_engine::{ChangeKind, EntryState, Index, LocalChange, RelPath};

use crate::error::WatchError;
use crate::sig;

/// Walk `root`, hash every tracked regular file, and diff against `index`.
///
/// Emitted changes, in ascending [`RelPath`] order (deterministic — the engine
/// and tests depend on it):
/// - a file on disk that is absent from `index`, or whose signature differs
///   from the indexed one, or whose index entry is a tombstone →
///   [`ChangeKind::Modified`];
/// - an index entry currently [`EntryState::Present`] whose file is missing on
///   disk (and whose path is not ignored) → [`ChangeKind::Removed`].
///
/// The walk skips the hardcoded `.tomo/` directory (invariant #1), any
/// directory or file classified [`PathClass::Ignored`], and every non-regular
/// file (directories and symlinks — see [`crate::sig::snapshot`]).
///
/// # Errors
/// [`WatchError::Io`] if a directory cannot be listed or a file cannot be read.
pub fn scan_diff(
    root: &Path,
    index: &Index,
    config: &Config,
) -> Result<Vec<LocalChange>, WatchError> {
    // Collect the current on-disk state first, then diff. Using a map keyed by
    // RelPath gives us both O(1) membership for the removal pass and the
    // ascending order the contract promises (via the final BTree merge).
    let mut on_disk: std::collections::BTreeMap<RelPath, tomo_engine::ContentSig> =
        std::collections::BTreeMap::new();
    walk(root, root, config, &mut on_disk)?;

    let mut changes: std::collections::BTreeMap<RelPath, LocalChange> =
        std::collections::BTreeMap::new();

    // Additions and modifications.
    for (rel, sig) in &on_disk {
        // Diff against the winner head: the materialized, disk-facing state.
        let differs = match index.get(rel).map(|e| e.winner().state) {
            Some(EntryState::Present(prev)) => prev != *sig,
            // Absent, or resurrected over a tombstone.
            Some(EntryState::Tombstone) | None => true,
        };
        if differs {
            changes.insert(
                rel.clone(),
                LocalChange {
                    path: rel.clone(),
                    kind: ChangeKind::Modified(*sig),
                },
            );
        }
    }

    // Deletions: present in the index but gone from disk. Skip paths the config
    // now ignores so a newly-ignored tree is not mass-deleted.
    for (rel, entry) in index.iter() {
        if matches!(entry.winner().state, EntryState::Present(_))
            && !on_disk.contains_key(rel)
            && config.classify(rel.as_str()).class != PathClass::Ignored
        {
            changes.insert(
                rel.clone(),
                LocalChange {
                    path: rel.clone(),
                    kind: ChangeKind::Removed,
                },
            );
        }
    }

    Ok(changes.into_values().collect())
}

/// Recursively walk `dir`, recording each tracked regular file's signature.
///
/// `root` is the fixed project root used to compute repo-relative paths;
/// `dir` is the directory currently being listed.
fn walk(
    root: &Path,
    dir: &Path,
    config: &Config,
    out: &mut std::collections::BTreeMap<RelPath, tomo_engine::ContentSig>,
) -> Result<(), WatchError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(source) => {
            return Err(WatchError::Io {
                path: dir.to_path_buf(),
                source,
            })
        }
    };
    for entry in entries {
        let entry = entry.map_err(|source| WatchError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        // lstat: do not follow symlinks (avoids cycles; symlinks are untracked).
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(WatchError::Io { path, source }),
        };
        let file_type = meta.file_type();

        // Compute the repo-relative path; anything unrepresentable (including
        // `.tomo`, via RelPath) is skipped.
        let Some(rel) = relativize(root, &path) else {
            continue;
        };
        if config.classify(rel.as_str()).class == PathClass::Ignored {
            continue;
        }

        if file_type.is_dir() {
            walk(root, &path, config, out)?;
        } else if file_type.is_file() {
            if let Some(sig) = sig::snapshot(root, &rel)? {
                out.insert(rel, sig);
            }
        }
        // Symlinks and other special files are ignored (v0).
    }
    Ok(())
}

/// Build a repo-relative [`RelPath`] for `path` under `root`, or `None` if it
/// escapes the root, is non-UTF-8, or is `.tomo/**`.
fn relativize(root: &Path, path: &Path) -> Option<RelPath> {
    let rel = path.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for comp in rel.components() {
        match comp {
            std::path::Component::Normal(os) => parts.push(os.to_str()?),
            _ => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    RelPath::new(&parts.join("/")).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // panics are fine in tests
mod tests {
    use super::*;
    use tomo_engine::{ContentHash, ContentSig, Entry, VectorClock};

    fn write(root: &Path, rel: &str, bytes: &[u8]) {
        let full = root.join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, bytes).unwrap();
    }

    fn sig_of(bytes: &[u8]) -> ContentSig {
        ContentSig {
            hash: ContentHash(*blake3::hash(bytes).as_bytes()),
            size: bytes.len() as u64,
        }
    }

    fn present(sig: ContentSig) -> Entry {
        Entry::single(VectorClock::new(), EntryState::Present(sig))
    }

    #[test]
    fn empty_index_reports_all_files_sorted() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "b.txt", b"b");
        write(dir.path(), "a/c.txt", b"c");
        write(dir.path(), "a/b.txt", b"ab");

        let changes = scan_diff(dir.path(), &Index::new(), &Config::default()).unwrap();
        let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["a/b.txt", "a/c.txt", "b.txt"]); // ascending
        assert!(changes
            .iter()
            .all(|c| matches!(c.kind, ChangeKind::Modified(_))));
    }

    #[test]
    fn matching_index_reports_nothing() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f", b"data");
        let mut index = Index::new();
        index.upsert(RelPath::new("f").unwrap(), present(sig_of(b"data")));

        assert!(scan_diff(dir.path(), &index, &Config::default())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn changed_content_is_modified() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f", b"new");
        let mut index = Index::new();
        index.upsert(RelPath::new("f").unwrap(), present(sig_of(b"old")));

        let changes = scan_diff(dir.path(), &index, &Config::default()).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Modified(sig_of(b"new")));
    }

    #[test]
    fn missing_present_entry_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        // Nothing on disk; index says "f" is present.
        let mut index = Index::new();
        index.upsert(RelPath::new("f").unwrap(), present(sig_of(b"data")));

        let changes = scan_diff(dir.path(), &index, &Config::default()).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, RelPath::new("f").unwrap());
        assert_eq!(changes[0].kind, ChangeKind::Removed);
    }

    #[test]
    fn tombstone_then_file_on_disk_is_modified() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f", b"back");
        let mut index = Index::new();
        index.upsert(
            RelPath::new("f").unwrap(),
            Entry::single(VectorClock::new(), EntryState::Tombstone),
        );

        let changes = scan_diff(dir.path(), &index, &Config::default()).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Modified(sig_of(b"back")));
    }

    #[test]
    fn ignored_paths_and_tomo_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/main.rs", b"code");
        write(dir.path(), "target/debug/app", b"binary");
        write(dir.path(), ".tomo/db/history.sqlite", b"state");

        let cfg = Config::from_toml_str("[[rules]]\npattern = \"target/\"\nclass = \"ignored\"\n")
            .unwrap();
        let changes = scan_diff(dir.path(), &Index::new(), &cfg).unwrap();
        let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["src/main.rs"]);
    }

    #[test]
    fn upgrade_to_default_git_ignore_does_not_delete_synced_git_tree() {
        // Regression for the `.git` default-ignore rollout: a peer that synced a
        // `.git/` tree under an OLDER Tomo (no built-in .git ignore) must not have
        // that tree mass-deleted after upgrading to a Tomo whose Config::default()
        // now ignores `.git`. The files are still on disk AND in the index; the
        // scan must report NOTHING (not Modified, not Removed) — walk() skips them
        // as ignored and the deletion pass skips now-ignored paths.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".git/HEAD", b"ref: refs/heads/main\n");
        write(dir.path(), ".git/config", b"[core]\n");
        write(dir.path(), "src/main.rs", b"code");

        let mut index = Index::new();
        index.upsert(
            RelPath::new(".git/HEAD").unwrap(),
            present(sig_of(b"ref: refs/heads/main\n")),
        );
        index.upsert(
            RelPath::new(".git/config").unwrap(),
            present(sig_of(b"[core]\n")),
        );
        index.upsert(
            RelPath::new("src/main.rs").unwrap(),
            present(sig_of(b"code")),
        );

        // Default config now carries the built-in `.git` ignore.
        let changes = scan_diff(dir.path(), &index, &Config::default()).unwrap();
        let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
        assert!(
            paths.is_empty(),
            "no .git change should be reported after the default-ignore upgrade, got {paths:?}"
        );
    }

    #[test]
    fn ignored_missing_file_is_not_reported_removed() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::from_toml_str("[[rules]]\npattern = \"target/\"\nclass = \"ignored\"\n")
            .unwrap();
        // Index still lists a now-ignored, on-disk-absent path.
        let mut index = Index::new();
        index.upsert(RelPath::new("target/app").unwrap(), present(sig_of(b"x")));

        assert!(scan_diff(dir.path(), &index, &cfg).unwrap().is_empty());
    }
}
