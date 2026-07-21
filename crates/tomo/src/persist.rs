//! Persistence of the engine [`Index`] to `.tomo/state/index.bin`.
//!
//! The index is serialized with `postcard` (the same compact codec the wire
//! protocol uses) and written atomically via [`crate::fsutil::atomic_write`].
//! An absent file loads as an empty index — the correct state for a
//! freshly-initialized project.
//!
//! # Perf note (M3)
//! M1 rewrites the whole index after every handled event batch. At M1 scale
//! (small trees, local sync) this is trivially cheap; when history and large
//! trees land in M3 this should become an incremental/journaled write.

use std::path::Path;

use tomo_engine::Index;
use tomo_watch::ScanCache;

use crate::error::CliError;
use crate::fsutil::atomic_write;

/// Load the persisted index from `path`, tolerating an undecodable file.
///
/// Returns `(index, recovered)`:
/// - an absent file loads as an empty index with `recovered == false` (a fresh
///   project);
/// - a current-format file decodes directly, `recovered == false`;
/// - a **pre-`mtime_ms`** file (written by a Tomo before the genesis adoption
///   tiebreak landed) decodes through the [`legacy`] fallback, which rebuilds
///   every entry with `mtime_ms` defaulted to `0`, `recovered == false`. This
///   is the migration that matters: it **preserves every vector clock**, so an
///   upgraded-but-converged project stays converged — no fabricated conflicts,
///   no mass reship, no re-versioning. `mtime_ms = 0` is harmless because mtime
///   is only ever consulted at genesis (disjoint-support clocks), and a
///   migrated entry already shares clock support with its peer;
/// - a genuinely undecodable file (corruption, or a format even older than the
///   legacy layout) loads as an **empty** index with `recovered == true`, and
///   the caller relies on the startup `scan_diff` to rebuild it from the tree
///   (a one-time re-index, never data loss — invariant #8's durable state is
///   the history DB and the tree, not this cache).
///
/// # Errors
/// [`CliError::Io`] if the file exists but cannot be read (a genuine I/O error,
/// distinct from an undecodable-but-readable file).
pub fn load_index(path: &Path) -> Result<(Index, bool), CliError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok((Index::new(), false)),
        Err(source) => return Err(CliError::io("read index", path, source)),
    };
    // Current format first; then the pre-mtime legacy layout (clocks preserved);
    // only a genuinely unreadable file falls back to empty + rescan.
    if let Ok(index) = postcard::from_bytes::<Index>(&bytes) {
        return Ok((index, false));
    }
    if let Some(index) = legacy::decode(&bytes) {
        return Ok((index, false));
    }
    Ok((Index::new(), true))
}

/// Decode support for the pre-`mtime_ms` on-disk index layout.
///
/// `postcard` is not self-describing, so a struct that mirrors the *old*
/// [`tomo_engine::ContentSig`] (hash, size, exec — no `mtime_ms`) decodes an
/// index written before the field existed. The mirror reuses every unchanged
/// engine type (`VectorClock`, `RelPath`, `ContentHash`) so it matches the old
/// byte layout exactly, then rebuilds real engine [`Entry`]s via the public
/// `single`/`absorb` API (their heads are private), defaulting `mtime_ms` to
/// `0`. Rebuilding a valid antichain via `absorb` reproduces it verbatim.
mod legacy {
    use std::collections::BTreeMap;

    use serde::Deserialize;
    use tomo_engine::{ContentHash, ContentSig, Entry, EntryState, Index, RelPath, VectorClock};

    #[derive(Deserialize)]
    struct LegacySig {
        hash: ContentHash,
        size: u64,
        exec: bool,
    }

    #[derive(Deserialize)]
    enum LegacyState {
        Present(LegacySig),
        Tombstone,
    }

    #[derive(Deserialize)]
    struct LegacyHead {
        version: VectorClock,
        state: LegacyState,
    }

    #[derive(Deserialize)]
    struct LegacyEntry {
        heads: Vec<LegacyHead>,
    }

    #[derive(Deserialize)]
    struct LegacyIndex {
        entries: BTreeMap<RelPath, LegacyEntry>,
    }

    fn upgrade_state(state: LegacyState) -> EntryState {
        match state {
            LegacyState::Present(sig) => EntryState::Present(ContentSig {
                hash: sig.hash,
                size: sig.size,
                exec: sig.exec,
                mtime_ms: 0,
            }),
            LegacyState::Tombstone => EntryState::Tombstone,
        }
    }

    /// Decode `bytes` as the legacy layout, or `None` if they are not that
    /// layout either. Never trusts a partial decode: `take_from_bytes` must
    /// consume the whole buffer, which rejects a current-format file that
    /// happened to start plausibly.
    pub(super) fn decode(bytes: &[u8]) -> Option<Index> {
        let (legacy, rest) = postcard::take_from_bytes::<LegacyIndex>(bytes).ok()?;
        if !rest.is_empty() {
            return None;
        }
        let mut index = Index::new();
        for (path, entry) in legacy.entries {
            let mut heads = entry.heads.into_iter();
            let first = heads.next()?;
            let mut rebuilt = Entry::single(first.version, upgrade_state(first.state));
            for head in heads {
                rebuilt.absorb(head.version, upgrade_state(head.state));
            }
            index.upsert(path, rebuilt);
        }
        Some(index)
    }
}

/// Atomically persist `index` to `path`, staging in `staging_dir`.
///
/// # Errors
/// [`CliError::Codec`] if the index cannot be serialized;
/// [`CliError::Io`] if the atomic write fails.
pub fn store_index(staging_dir: &Path, path: &Path, index: &Index) -> Result<(), CliError> {
    let bytes = postcard::to_allocvec(index).map_err(|source| CliError::Codec {
        context: "encode index".to_owned(),
        source,
    })?;
    atomic_write(staging_dir, path, &bytes)
}

/// Load the persisted startup-scan cache from `path`, tolerating absence and
/// corruption.
///
/// An absent, unreadable, corrupt, or older-format file loads as an **empty**
/// [`ScanCache`] — the cache is a pure optimization, never a correctness input,
/// so any problem simply degrades to a full cold scan (never an error). Unlike
/// the index, even a hard read I/O error is swallowed here for the same reason.
#[must_use]
pub fn load_scan_cache(path: &Path) -> ScanCache {
    match std::fs::read(path) {
        Ok(bytes) => ScanCache::decode(&bytes).unwrap_or_default(),
        Err(_) => ScanCache::default(),
    }
}

/// Atomically persist the startup-scan `cache` to `path`, staging in
/// `staging_dir`.
///
/// # Errors
/// [`CliError::Codec`] if the cache cannot be serialized;
/// [`CliError::Io`] if the atomic write fails.
pub fn store_scan_cache(
    staging_dir: &Path,
    path: &Path,
    cache: &ScanCache,
) -> Result<(), CliError> {
    let bytes = cache.encode().map_err(|source| CliError::Codec {
        context: "encode scan cache".to_owned(),
        source,
    })?;
    atomic_write(staging_dir, path, &bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tomo_engine::{ContentHash, ContentSig, Entry, EntryState, RelPath, VectorClock};

    fn sample_index() -> Index {
        let mut idx = Index::new();
        let mut v = VectorClock::new();
        v.tick(tomo_engine::ReplicaId(42));
        idx.upsert(
            RelPath::new("src/main.rs").unwrap(),
            Entry::single(
                v,
                EntryState::Present(ContentSig {
                    hash: ContentHash([9; 32]),
                    size: 123,
                    exec: false,
                    mtime_ms: 7_000,
                }),
            ),
        );
        idx.upsert(
            RelPath::new("gone.txt").unwrap(),
            Entry::single(VectorClock::new(), EntryState::Tombstone),
        );
        idx
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let (idx, recovered) = load_index(&dir.path().join("nope.bin")).unwrap();
        assert!(idx.is_empty());
        assert!(
            !recovered,
            "an absent file is a fresh project, not a recovery"
        );
    }

    #[test]
    fn store_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let path = dir.path().join("index.bin");

        let original = sample_index();
        store_index(&staging, &path, &original).unwrap();
        let (loaded, recovered) = load_index(&path).unwrap();

        assert!(!recovered);
        assert_eq!(original, loaded);
        assert_eq!(original.canonical_bytes(), loaded.canonical_bytes());
    }

    // ---- Legacy (pre-mtime) index migration -------------------------------

    // Mirrors of the OLD on-disk ContentSig/Index layout (no `mtime_ms`), used
    // to synthesize bytes exactly as a pre-adoption Tomo would have written
    // them. Serialize here, decode through `load_index`'s legacy fallback.
    #[derive(serde::Serialize)]
    struct OldSig {
        hash: ContentHash,
        size: u64,
        exec: bool,
    }
    #[derive(serde::Serialize)]
    enum OldState {
        Present(OldSig),
        Tombstone,
    }
    #[derive(serde::Serialize)]
    struct OldHead {
        version: VectorClock,
        state: OldState,
    }
    #[derive(serde::Serialize)]
    struct OldEntry {
        heads: Vec<OldHead>,
    }
    #[derive(serde::Serialize)]
    struct OldIndex {
        entries: std::collections::BTreeMap<RelPath, OldEntry>,
    }

    #[test]
    fn legacy_index_decodes_preserving_clocks_with_zero_mtime() {
        // A pre-mtime index with a present file (clock {7:2}) and a tombstone
        // (clock {9:1}). It must decode WITHOUT recovery, preserving both clocks
        // exactly and defaulting mtime_ms to 0 — the no-mass-resync migration.
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let path = dir.path().join("index.bin");

        let mut present_clock = VectorClock::new();
        present_clock.tick(tomo_engine::ReplicaId(7));
        present_clock.tick(tomo_engine::ReplicaId(7));
        let mut tomb_clock = VectorClock::new();
        tomb_clock.tick(tomo_engine::ReplicaId(9));

        let mut entries = std::collections::BTreeMap::new();
        entries.insert(
            RelPath::new("src/main.rs").unwrap(),
            OldEntry {
                heads: vec![OldHead {
                    version: present_clock.clone(),
                    state: OldState::Present(OldSig {
                        hash: ContentHash([5; 32]),
                        size: 321,
                        exec: true,
                    }),
                }],
            },
        );
        entries.insert(
            RelPath::new("gone.txt").unwrap(),
            OldEntry {
                heads: vec![OldHead {
                    version: tomb_clock.clone(),
                    state: OldState::Tombstone,
                }],
            },
        );
        let old_bytes = postcard::to_allocvec(&OldIndex { entries }).unwrap();
        std::fs::write(&path, &old_bytes).unwrap();

        let (idx, recovered) = load_index(&path).unwrap();
        assert!(!recovered, "a legacy index is migrated, not recovered");
        assert_eq!(idx.len(), 2);

        let present = idx.get(&RelPath::new("src/main.rs").unwrap()).unwrap();
        assert_eq!(present.winner().version, present_clock, "clock preserved");
        match present.winner().state {
            EntryState::Present(sig) => {
                assert_eq!(sig.hash, ContentHash([5; 32]));
                assert_eq!(sig.size, 321);
                assert!(sig.exec);
                assert_eq!(sig.mtime_ms, 0, "legacy entries default mtime to 0");
            }
            EntryState::Tombstone => panic!("expected present"),
        }

        let tomb = idx.get(&RelPath::new("gone.txt").unwrap()).unwrap();
        assert_eq!(tomb.winner().version, tomb_clock);
        assert_eq!(tomb.winner().state, EntryState::Tombstone);
    }

    #[test]
    fn legacy_migration_round_trips_to_current_format() {
        // After a legacy load, re-storing yields a current-format file that
        // reloads identically — the one-time migration then sticks.
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let path = dir.path().join("index.bin");

        let mut clock = VectorClock::new();
        clock.tick(tomo_engine::ReplicaId(1));
        let mut entries = std::collections::BTreeMap::new();
        entries.insert(
            RelPath::new("a").unwrap(),
            OldEntry {
                heads: vec![OldHead {
                    version: clock,
                    state: OldState::Present(OldSig {
                        hash: ContentHash([2; 32]),
                        size: 4,
                        exec: false,
                    }),
                }],
            },
        );
        std::fs::write(&path, postcard::to_allocvec(&OldIndex { entries }).unwrap()).unwrap();

        let (migrated, _) = load_index(&path).unwrap();
        store_index(&staging, &path, &migrated).unwrap();
        let (reloaded, recovered) = load_index(&path).unwrap();
        assert!(!recovered);
        assert_eq!(migrated, reloaded);
        assert_eq!(migrated.canonical_bytes(), reloaded.canonical_bytes());
    }

    #[test]
    fn undecodable_bytes_recover_as_empty_for_rescan() {
        // An older on-disk format (or corruption) is not fatal: the index is a
        // reconstructible cache, so it loads empty with `recovered = true` and
        // the caller relies on the startup rescan to rebuild it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.bin");
        std::fs::write(&path, b"\xff\xff not postcard \x00\x01").unwrap();
        let (idx, recovered) = load_index(&path).unwrap();
        assert!(idx.is_empty());
        assert!(recovered, "undecodable bytes must flag a recovery");
    }
}
