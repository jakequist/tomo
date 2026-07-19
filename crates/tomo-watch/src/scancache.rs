//! Startup-scan mtime+size cache: skip re-hashing unchanged files.
//!
//! A cold startup [`scan_diff`](crate::scan_diff) BLAKE3-hashes every tracked
//! regular file in the tree to diff it against the index. On a large tree
//! (tens of thousands of files) that hashing dominates the time-to-connected,
//! even though almost nothing changed while Tomo was down.
//!
//! This cache remembers, per path, the `(mtime_ns, size, ContentSig)` observed
//! at the last scan. On the next scan, a file whose `(mtime_ns, size)` still
//! match the cache is assumed unchanged and its stored hash is reused **without
//! reading or hashing the bytes** — the same quick-check `rsync` uses. A
//! mismatch, or an absent entry, falls back to hashing exactly as before.
//!
//! # Safety (why this cannot silently miss a change)
//! - A content write always advances the file's mtime, so changed bytes yield a
//!   different `mtime_ns` and are re-hashed. The only way to defeat the check is
//!   to rewrite a file with an identical size *and* forcibly restore its old
//!   mtime — a pathological act, accepted exactly as `rsync`'s default does.
//! - **Recent-write guard**: a file whose mtime is within
//!   [`RECENT_WINDOW_NS`] of now may still be mid-mutation (its mtime already
//!   bumped while more bytes are landing), so [`decide`] never trusts it and
//!   always hashes. This is why the cache stores nanosecond mtimes.
//! - A **corrupt or older-format** cache file is discarded silently ([`decode`]
//!   returns `None`), degrading to a full cold scan — never an error, never
//!   wrong data (the cache is a pure optimization).
//!
//! Note the exec bit is *not* part of the quick-check: a `chmod` changes ctime,
//! not mtime, so the scanner takes the **fresh** exec bit from the `lstat` it
//! already performs and only reuses the cached content *hash* — a chmod-only
//! change is still detected. See [`crate::scan`].

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tomo_engine::{ContentHash, ContentSig, RelPath};

/// Do-not-trust window: a file modified within this many nanoseconds of "now" is
/// always hashed rather than trusted, because its mtime may already have bumped
/// while the write is still in flight. 2 seconds (matches the session's other
/// recent-write guards).
pub const RECENT_WINDOW_NS: u64 = 2_000_000_000;

/// On-disk format version. Bumped if [`CacheEntry`]'s shape changes; a file
/// carrying any other version is discarded (a full cold scan rebuilds it).
const CACHE_VERSION: u32 = 1;

/// One remembered file: the metadata quick-check keys plus the content signature
/// to reuse when they still match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEntry {
    /// File modification time in nanoseconds since the Unix epoch.
    pub mtime_ns: u64,
    /// File size in bytes.
    pub size: u64,
    /// The signature observed when this entry was recorded. Only its content
    /// [`hash`](ContentSig::hash) is reused; size/exec are re-derived from the
    /// fresh `lstat` at scan time.
    pub sig: ContentSig,
}

/// The scan cache: path → last-observed `(mtime_ns, size, sig)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanCache {
    entries: BTreeMap<RelPath, CacheEntry>,
}

/// The serialized wrapper, carrying the format version for forward/backward
/// compatibility.
#[derive(Serialize, Deserialize)]
struct OnDisk {
    version: u32,
    entries: BTreeMap<RelPath, CacheEntry>,
}

/// What [`decide`] concluded for one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanDecision {
    /// Read and hash the file (cache miss, stale, or recently written).
    Hash,
    /// Reuse this content hash (cache hit); combine it with the fresh size/exec.
    Reuse(ContentHash),
}

/// Decide whether a file's cached hash may be reused, given its fresh
/// `(mtime_ns, size)` and the current time `now_ns`.
///
/// Returns [`ScanDecision::Reuse`] only when a cache entry exists with matching
/// `mtime_ns` **and** `size` **and** the file was not modified within
/// [`RECENT_WINDOW_NS`] of `now_ns`. Otherwise [`ScanDecision::Hash`].
#[must_use]
pub fn decide(cached: Option<&CacheEntry>, mtime_ns: u64, size: u64, now_ns: u64) -> ScanDecision {
    // Recent-write guard: never trust an mtime within the window of now.
    if mtime_ns.saturating_add(RECENT_WINDOW_NS) >= now_ns {
        return ScanDecision::Hash;
    }
    match cached {
        Some(e) if e.mtime_ns == mtime_ns && e.size == size => ScanDecision::Reuse(e.sig.hash),
        _ => ScanDecision::Hash,
    }
}

/// Build a cache entry for the regular file at `full`, pairing its on-disk
/// `(mtime_ns, size)` with the already-known `sig`. Returns `None` when the path
/// is not a regular file or cannot be stat'd, so the caller drops (rather than
/// stores) the cache entry — a subsequent scan then hashes it afresh.
///
/// Used by the session to keep the in-memory cache current between full scans
/// (after applying a change or observing a local edit), so the *next* startup
/// scan benefits from files touched during this session.
#[must_use]
pub fn stat_entry(full: &Path, sig: ContentSig) -> Option<CacheEntry> {
    let meta = std::fs::symlink_metadata(full).ok()?;
    if !meta.file_type().is_file() {
        return None;
    }
    Some(CacheEntry {
        mtime_ns: crate::sig::mtime_ns(&meta),
        size: meta.len(),
        sig,
    })
}

impl ScanCache {
    /// An empty cache (also [`Default`]).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The remembered entry for `path`, if any.
    #[must_use]
    pub fn get(&self, path: &RelPath) -> Option<&CacheEntry> {
        self.entries.get(path)
    }

    /// Record (or replace) the entry for `path`.
    pub fn insert(&mut self, path: RelPath, entry: CacheEntry) {
        self.entries.insert(path, entry);
    }

    /// Forget `path` (e.g. it was deleted).
    pub fn remove(&mut self, path: &RelPath) {
        self.entries.remove(path);
    }

    /// Number of remembered files.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Serialize to the versioned on-disk byte form (for atomic persistence by
    /// the caller).
    ///
    /// # Errors
    /// [`postcard::Error`] if serialization fails (unreachable for this data).
    pub fn encode(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(&OnDisk {
            version: CACHE_VERSION,
            entries: self.entries.clone(),
        })
    }

    /// Decode a cache from bytes, returning `None` when the bytes are corrupt or
    /// carry a different format version (the caller then starts from empty).
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let on_disk: OnDisk = postcard::from_bytes(bytes).ok()?;
        if on_disk.version != CACHE_VERSION {
            return None;
        }
        Some(Self {
            entries: on_disk.entries,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn sig(hash_byte: u8, size: u64) -> ContentSig {
        ContentSig {
            hash: ContentHash([hash_byte; 32]),
            size,
            exec: false,
        }
    }

    fn entry(mtime_ns: u64, size: u64, hash_byte: u8) -> CacheEntry {
        CacheEntry {
            mtime_ns,
            size,
            sig: sig(hash_byte, size),
        }
    }

    // A "now" far past any test mtime, so the recent-write guard is inactive
    // unless a test deliberately places an mtime near it.
    const NOW: u64 = 1_000_000 * RECENT_WINDOW_NS;

    #[test]
    fn fresh_match_reuses_the_hash() {
        let e = entry(100, 42, 7);
        match decide(Some(&e), 100, 42, NOW) {
            ScanDecision::Reuse(h) => assert_eq!(h, ContentHash([7; 32])),
            ScanDecision::Hash => panic!("expected reuse on an exact match"),
        }
    }

    #[test]
    fn absent_entry_hashes() {
        assert_eq!(decide(None, 100, 42, NOW), ScanDecision::Hash);
    }

    #[test]
    fn mtime_mismatch_hashes() {
        let e = entry(100, 42, 7);
        assert_eq!(decide(Some(&e), 101, 42, NOW), ScanDecision::Hash);
    }

    #[test]
    fn size_mismatch_hashes() {
        // Same mtime, different size → the content changed → hash.
        let e = entry(100, 42, 7);
        assert_eq!(decide(Some(&e), 100, 43, NOW), ScanDecision::Hash);
    }

    #[test]
    fn recently_modified_file_always_hashes() {
        // The entry matches exactly, but the file's mtime is within the recent
        // window of now — it may be mid-write, so never trust it.
        let mtime = NOW - RECENT_WINDOW_NS / 2;
        let e = entry(mtime, 42, 7);
        assert_eq!(decide(Some(&e), mtime, 42, NOW), ScanDecision::Hash);
        // Exactly at the window boundary is still distrusted (>= is inclusive).
        let at = NOW - RECENT_WINDOW_NS;
        let e2 = entry(at, 42, 7);
        assert_eq!(decide(Some(&e2), at, 42, NOW), ScanDecision::Hash);
        // Just outside the window is trusted again.
        let old = NOW - RECENT_WINDOW_NS - 1;
        let e3 = entry(old, 42, 7);
        assert_eq!(
            decide(Some(&e3), old, 42, NOW),
            ScanDecision::Reuse(ContentHash([7; 32]))
        );
    }

    #[test]
    fn round_trip_preserves_entries() {
        let mut c = ScanCache::new();
        c.insert(RelPath::new("src/main.rs").unwrap(), entry(123, 456, 9));
        c.insert(RelPath::new("a/b/c.txt").unwrap(), entry(1, 2, 3));
        let bytes = c.encode().unwrap();
        let back = ScanCache::decode(&bytes).expect("decodes");
        assert_eq!(back, c);
        assert_eq!(back.len(), 2);
        assert_eq!(
            back.get(&RelPath::new("src/main.rs").unwrap())
                .unwrap()
                .sig
                .hash,
            ContentHash([9; 32])
        );
    }

    #[test]
    fn corrupt_bytes_decode_to_none() {
        assert!(ScanCache::decode(b"\xff not postcard \x00\x01").is_none());
    }

    #[test]
    fn wrong_version_decodes_to_none() {
        // Hand-encode an OnDisk with a bogus version.
        let bytes = postcard::to_allocvec(&OnDisk {
            version: CACHE_VERSION + 99,
            entries: BTreeMap::new(),
        })
        .unwrap();
        assert!(ScanCache::decode(&bytes).is_none());
    }

    #[test]
    fn remove_forgets_an_entry() {
        let mut c = ScanCache::new();
        let p = RelPath::new("gone").unwrap();
        c.insert(p.clone(), entry(1, 1, 1));
        assert!(c.get(&p).is_some());
        c.remove(&p);
        assert!(c.get(&p).is_none());
    }
}
