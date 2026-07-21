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
use tomo_engine::{ChangeKind, ContentSig, EntryState, Index, LocalChange, RelPath};

use crate::error::WatchError;
use crate::scancache::{self, CacheEntry, ScanCache, ScanDecision};
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
    normalize_unicode: bool,
) -> Result<Vec<LocalChange>, WatchError> {
    // The cache-free path: an empty cache and `now_ns = 0` mean every file is
    // hashed (a miss on the empty cache, and `0` also trips the recent-write
    // guard), reproducing the original always-hash behavior exactly.
    Ok(scan_diff_cached(root, index, config, normalize_unicode, &ScanCache::new(), 0)?.0)
}

/// Like [`scan_diff`], but consults `cache` to skip re-hashing files whose
/// `(mtime_ns, size)` are unchanged (the startup-scan optimization).
///
/// Returns the diff **and** a freshly rebuilt [`ScanCache`] covering every
/// present regular file observed this scan (reused-or-freshly-hashed), which the
/// caller should persist for the next startup. `now_ns` is the current wall time
/// in nanoseconds since the epoch, used **only** for the recent-write guard
/// ([`scancache::decide`]) — never for ordering (invariant #7); a file modified
/// within [`scancache::RECENT_WINDOW_NS`] of `now_ns` is always hashed.
///
/// # Errors
/// [`WatchError::Io`] if a directory cannot be listed or a file cannot be read.
pub fn scan_diff_cached(
    root: &Path,
    index: &Index,
    config: &Config,
    normalize_unicode: bool,
    cache: &ScanCache,
    now_ns: u64,
) -> Result<(Vec<LocalChange>, ScanCache), WatchError> {
    // Collect the current on-disk state first, then diff. Using a map keyed by
    // RelPath gives us both O(1) membership for the removal pass and the
    // ascending order the contract promises (via the final BTree merge).
    let mut on_disk: std::collections::BTreeMap<RelPath, ContentSig> =
        std::collections::BTreeMap::new();
    let mut fresh_cache = ScanCache::new();
    walk(
        root,
        root,
        config,
        normalize_unicode,
        cache,
        now_ns,
        &mut on_disk,
        &mut fresh_cache,
    )?;

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

    Ok((changes.into_values().collect(), fresh_cache))
}

/// Recursively walk `dir`, recording each tracked regular file's signature —
/// hashing it, or reusing the cached hash when its `(mtime_ns, size)` still
/// match (`old` cache, gated by the recent-write guard against `now_ns`). Every
/// present regular file's fresh `(mtime_ns, size, sig)` is recorded into
/// `fresh` so the caller can persist an up-to-date cache.
///
/// `root` is the fixed project root used to compute repo-relative paths;
/// `dir` is the directory currently being listed.
#[allow(clippy::too_many_arguments)] // one cohesive recursive walk; splitting would obscure it
fn walk(
    root: &Path,
    dir: &Path,
    config: &Config,
    normalize_unicode: bool,
    old: &ScanCache,
    now_ns: u64,
    out: &mut std::collections::BTreeMap<RelPath, ContentSig>,
    fresh: &mut ScanCache,
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
        let Some(rel) = relativize(root, &path, normalize_unicode) else {
            continue;
        };
        if config.classify(rel.as_str()).class == PathClass::Ignored {
            continue;
        }

        if file_type.is_dir() {
            walk(
                root,
                &path,
                config,
                normalize_unicode,
                old,
                now_ns,
                out,
                fresh,
            )?;
        } else if file_type.is_file() {
            // Quick-check: if this file's (mtime_ns, size) still match the cache
            // and it was not modified recently, reuse the stored hash and skip
            // reading + BLAKE3-ing the bytes entirely. Exec is always taken from
            // the fresh lstat (a chmod bumps ctime, not mtime — see scancache).
            let mtime_ns = sig::mtime_ns(&meta);
            let size = meta.len();
            let sig = match scancache::decide(old.get(&rel), mtime_ns, size, now_ns) {
                ScanDecision::Reuse(hash) => Some(ContentSig {
                    hash,
                    size,
                    exec: sig::is_executable(&meta),
                }),
                // Miss/stale/recent: read + hash as before. `snapshot` re-stats,
                // so a file that vanished mid-walk safely yields None.
                ScanDecision::Hash => sig::snapshot(root, &rel)?,
            };
            if let Some(sig) = sig {
                fresh.insert(
                    rel.clone(),
                    CacheEntry {
                        mtime_ns,
                        size,
                        sig,
                    },
                );
                out.insert(rel, sig);
            }
        }
        // Symlinks and other special files are ignored (v0).
    }
    Ok(())
}

/// Build a repo-relative [`RelPath`] for `path` under `root`, or `None` if it
/// escapes the root, is non-UTF-8, or is `.tomo/**`.
///
/// When `normalize_unicode` is set (a normalizing local FS such as APFS), the
/// derived name is canonicalized to NFC via [`crate::norm`], so an NFD name a
/// normalizing filesystem returns from `readdir` collapses to the same
/// `RelPath` as its NFC original (docs/NOTES.md ledger item 3b).
fn relativize(root: &Path, path: &Path, normalize_unicode: bool) -> Option<RelPath> {
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
    let joined = parts.join("/");
    let canonical = crate::norm::canonicalize_fs_path(&joined, normalize_unicode);
    RelPath::new(&canonical).ok()
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
            exec: false,
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

        let changes = scan_diff(dir.path(), &Index::new(), &Config::default(), false).unwrap();
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

        assert!(scan_diff(dir.path(), &index, &Config::default(), false)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn changed_content_is_modified() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f", b"new");
        let mut index = Index::new();
        index.upsert(RelPath::new("f").unwrap(), present(sig_of(b"old")));

        let changes = scan_diff(dir.path(), &index, &Config::default(), false).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Modified(sig_of(b"new")));
    }

    #[test]
    fn missing_present_entry_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        // Nothing on disk; index says "f" is present.
        let mut index = Index::new();
        index.upsert(RelPath::new("f").unwrap(), present(sig_of(b"data")));

        let changes = scan_diff(dir.path(), &index, &Config::default(), false).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, RelPath::new("f").unwrap());
        assert_eq!(changes[0].kind, ChangeKind::Removed);
    }

    /// A tracked regular file replaced on disk by a **symlink** is reported as a
    /// deletion (docs/SPEC.md §5.4 "File→symlink replacement"): symlinks are never
    /// synced in v0, so the scan judges the path on its `lstat` (non-regular →
    /// skipped from `on_disk`), and the index's present entry with no disk match
    /// becomes `Removed`. The peer then tombstones it; the last file bytes stay in
    /// history (invariant #5).
    #[cfg(unix)]
    #[test]
    fn file_replaced_by_symlink_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        // The index knows `link` as a present regular file...
        let mut index = Index::new();
        index.upsert(
            RelPath::new("link").unwrap(),
            present(sig_of(b"was-a-file")),
        );
        // ...but on disk it is now a symlink (pointing anywhere — never followed).
        std::fs::write(dir.path().join("real-target"), b"t").unwrap();
        std::os::unix::fs::symlink(dir.path().join("real-target"), dir.path().join("link"))
            .unwrap();

        let changes = scan_diff(dir.path(), &index, &Config::default(), false).unwrap();
        // `link` is reported Removed; the symlink is not itself surfaced.
        let link_changes: Vec<&LocalChange> = changes
            .iter()
            .filter(|c| c.path.as_str() == "link")
            .collect();
        assert_eq!(link_changes.len(), 1);
        assert_eq!(link_changes[0].kind, ChangeKind::Removed);
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

        let changes = scan_diff(dir.path(), &index, &Config::default(), false).unwrap();
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
        let changes = scan_diff(dir.path(), &Index::new(), &cfg, false).unwrap();
        let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["src/main.rs"]);
    }

    #[test]
    fn upgrade_to_default_git_ignore_does_not_delete_synced_git_tree() {
        // Regression for the default-ignore rollout: a peer that synced a `.git/`
        // (or `node_modules/`) tree under an OLDER Tomo — before those built-in
        // ignores existed — must not have that tree mass-deleted after upgrading
        // to a Tomo whose Config::default() now ignores it. The files are still
        // on disk AND in the index; the scan must report NOTHING (not Modified,
        // not Removed) — walk() skips them as ignored and the deletion pass skips
        // now-ignored paths.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".git/HEAD", b"ref: refs/heads/main\n");
        write(dir.path(), ".git/config", b"[core]\n");
        write(
            dir.path(),
            "node_modules/react/index.js",
            b"module.exports={}",
        );
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
            RelPath::new("node_modules/react/index.js").unwrap(),
            present(sig_of(b"module.exports={}")),
        );
        index.upsert(
            RelPath::new("src/main.rs").unwrap(),
            present(sig_of(b"code")),
        );

        // Default config now carries the built-in `.git` and `node_modules`
        // ignores.
        let changes = scan_diff(dir.path(), &index, &Config::default(), false).unwrap();
        let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
        assert!(
            paths.is_empty(),
            "no newly-ignored change should be reported after the default-ignore upgrade, got {paths:?}"
        );
    }

    #[test]
    fn relativize_normalizes_nfd_to_nfc_only_when_flag_set() {
        // The scanner's name-derivation step. An NFD on-disk name ("e" +
        // combining acute) becomes the NFC RelPath when the local FS normalizes
        // (APFS), and stays byte-faithful otherwise (Linux). Tested at
        // `relativize` so no real filesystem is needed: an end-to-end scan on a
        // real normalizing FS is a Mac-session validation item (the NFC RelPath
        // then also *reads* back, because APFS normalizes lookups too — a
        // property this Linux VM cannot exercise). See docs/HANDOFF-MACOS.md.
        let root = Path::new("/proj");
        let nfd_path = root.join("caf\u{65}\u{301}.txt"); // decomposed é
        assert_eq!(
            relativize(root, &nfd_path, true).unwrap().as_str(),
            "caf\u{e9}.txt",
            "normalizing FS must yield the NFC RelPath"
        );
        assert_eq!(
            relativize(root, &nfd_path, false).unwrap().as_str(),
            "caf\u{65}\u{301}.txt",
            "byte-preserving FS must keep the NFD name"
        );
    }

    /// A FIFO in the tree is skipped by the scan walk (it is not a regular file)
    /// and, crucially, the walk does not block on it — opening a FIFO to read
    /// would hang until a writer appears, but the walk decides on the `lstat`
    /// type alone. Guarded with a real timeout via a worker thread.
    #[cfg(unix)]
    #[test]
    fn scan_skips_fifo_without_blocking() {
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "real.txt", b"content");
        let fifo = dir.path().join("pipe");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("spawn mkfifo");
        assert!(status.success(), "mkfifo failed");

        let root = dir.path().to_path_buf();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(scan_diff(&root, &Index::new(), &Config::default(), false));
        });
        let changes = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("scan must not block on a FIFO")
            .unwrap();
        let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
        // Only the regular file is reported; the FIFO is silently skipped.
        assert_eq!(paths, ["real.txt"]);
    }

    // A hash that is deliberately WRONG for any real content, used to prove
    // whether the scan reused a cached hash (no re-read) or actually hashed.
    fn wrong_sig(size: u64) -> ContentSig {
        ContentSig {
            hash: ContentHash([0xAB; 32]),
            size,
            exec: false,
        }
    }

    /// The cache quick-check reuses the stored hash without reading the file: we
    /// seed a cache entry whose hash is deliberately WRONG for the on-disk bytes
    /// but whose `(mtime_ns, size)` match, and an index carrying that same wrong
    /// hash. With `now_ns` far past the file's mtime (recent-write guard off), the
    /// scan trusts the cache → produces the wrong hash → sees NO change. Had it
    /// actually hashed, the real hash would differ and a change would be reported.
    #[test]
    fn cache_hit_reuses_hash_without_reading() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f", b"hello");
        let meta = std::fs::metadata(dir.path().join("f")).unwrap();
        let mtime = crate::sig::mtime_ns(&meta);

        let mut cache = ScanCache::new();
        cache.insert(
            RelPath::new("f").unwrap(),
            CacheEntry {
                mtime_ns: mtime,
                size: 5,
                sig: wrong_sig(5),
            },
        );
        // Index agrees with the (wrong) cached hash, so a reuse yields no change.
        let mut index = Index::new();
        index.upsert(RelPath::new("f").unwrap(), present(wrong_sig(5)));

        let now = mtime + 100 * scancache::RECENT_WINDOW_NS; // guard well clear
        let (changes, fresh) =
            scan_diff_cached(dir.path(), &index, &Config::default(), false, &cache, now).unwrap();
        assert!(
            changes.is_empty(),
            "a cache hit must reuse the stored hash (no re-hash), so no change is seen"
        );
        // The rebuilt cache carries the reused (wrong) hash forward.
        assert_eq!(
            fresh.get(&RelPath::new("f").unwrap()).unwrap().sig.hash,
            ContentHash([0xAB; 32])
        );
    }

    /// The recent-write guard defeats the cache: with the identical setup but
    /// `now_ns` close to the file's mtime, the scan distrusts the mtime and
    /// actually hashes — the real hash of "hello" differs from the wrong index
    /// entry, so a Modified change IS reported.
    #[test]
    fn recent_write_forces_hash_despite_cache_hit() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f", b"hello");
        let meta = std::fs::metadata(dir.path().join("f")).unwrap();
        let mtime = crate::sig::mtime_ns(&meta);

        let mut cache = ScanCache::new();
        cache.insert(
            RelPath::new("f").unwrap(),
            CacheEntry {
                mtime_ns: mtime,
                size: 5,
                sig: wrong_sig(5),
            },
        );
        let mut index = Index::new();
        index.upsert(RelPath::new("f").unwrap(), present(wrong_sig(5)));

        // now within the window of mtime → distrust → hash.
        let now = mtime + scancache::RECENT_WINDOW_NS / 2;
        let (changes, _) =
            scan_diff_cached(dir.path(), &index, &Config::default(), false, &cache, now).unwrap();
        assert_eq!(
            changes.len(),
            1,
            "a recently-written file must be re-hashed"
        );
        assert_eq!(changes[0].kind, ChangeKind::Modified(sig_of(b"hello")));
    }

    /// A stale cache entry (mtime moved on) is ignored and the file re-hashed —
    /// the correctness backstop that makes the between-scan cache safe.
    #[test]
    fn stale_cache_entry_is_rehashed() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f", b"world");
        let meta = std::fs::metadata(dir.path().join("f")).unwrap();
        let real_mtime = crate::sig::mtime_ns(&meta);

        // Cache remembers an OLDER mtime (and a wrong hash); mtime mismatch → hash.
        let mut cache = ScanCache::new();
        cache.insert(
            RelPath::new("f").unwrap(),
            CacheEntry {
                mtime_ns: real_mtime.wrapping_sub(1_000_000_000),
                size: 5,
                sig: wrong_sig(5),
            },
        );
        let now = real_mtime + 100 * scancache::RECENT_WINDOW_NS;
        let (changes, fresh) = scan_diff_cached(
            dir.path(),
            &Index::new(),
            &Config::default(),
            false,
            &cache,
            now,
        )
        .unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Modified(sig_of(b"world")));
        // The rebuilt cache now holds the REAL hash, not the stale wrong one.
        assert_eq!(
            fresh.get(&RelPath::new("f").unwrap()).unwrap().sig,
            sig_of(b"world")
        );
    }

    /// Manual measurement (not a CI assertion): build a synthetic 20k-small-file
    /// tree and time a cold startup scan (no cache) against a warm one (cache from
    /// the cold run, index matching so nothing is reported). Run with:
    ///   `cargo test -p tomo-watch --release -- --ignored --nocapture scancache_speedup`
    #[ignore = "manual perf measurement; run with --ignored --nocapture"]
    #[test]
    fn scancache_speedup_measurement() {
        use std::time::Instant;

        const N: usize = 20_000;
        let dir = tempfile::tempdir().unwrap();
        // Spread across 200 subdirs of 100 files, ~256 bytes each.
        for i in 0..N {
            let sub = dir.path().join(format!("d{:03}", i / 100));
            std::fs::create_dir_all(&sub).unwrap();
            let body = format!("file {i} ").repeat(24);
            std::fs::write(sub.join(format!("f{i:05}.txt")), body.as_bytes()).unwrap();
        }
        let cfg = Config::default();
        let now = crate::sig::mtime_ns(&std::fs::metadata(dir.path()).unwrap())
            + 100 * scancache::RECENT_WINDOW_NS; // clear the recent-write guard

        // Cold: empty cache, hashes every file.
        let t0 = Instant::now();
        let (cold_changes, warm_cache) = scan_diff_cached(
            dir.path(),
            &Index::new(),
            &cfg,
            false,
            &ScanCache::new(),
            now,
        )
        .unwrap();
        let cold = t0.elapsed();
        assert_eq!(cold_changes.len(), N);

        // Build an index matching the tree, so the warm scan reports nothing.
        let mut index = Index::new();
        for c in &cold_changes {
            if let ChangeKind::Modified(sig) = c.kind {
                index.upsert(c.path.clone(), present(sig));
            }
        }

        // Warm: full cache + matching index; every file is a quick-check hit.
        let t1 = Instant::now();
        let (warm_changes, _) =
            scan_diff_cached(dir.path(), &index, &cfg, false, &warm_cache, now).unwrap();
        let warm = t1.elapsed();
        assert!(warm_changes.is_empty());

        eprintln!(
            "scancache 20k-file startup scan: cold(hash-all)={cold:?}  warm(cache-hit)={warm:?}  \
             speedup={:.1}x",
            cold.as_secs_f64() / warm.as_secs_f64().max(1e-9)
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

        assert!(scan_diff(dir.path(), &index, &cfg, false)
            .unwrap()
            .is_empty());
    }
}
