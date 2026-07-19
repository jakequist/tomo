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

use crate::error::CliError;
use crate::fsutil::atomic_write;

/// Load the persisted index from `path`, tolerating an undecodable file.
///
/// Returns `(index, recovered)`:
/// - an absent file loads as an empty index with `recovered == false` (a fresh
///   project);
/// - a file whose bytes are not a decodable [`Index`] loads as an **empty**
///   index with `recovered == true`. This is the expected outcome after an
///   on-disk format change (e.g. adding the executable bit to `ContentSig`
///   bumps the `postcard` layout): the index is a reconstructible cache, so the
///   caller warns and the startup `scan_diff` re-indexes the tree (a one-time
///   re-index churn, never data loss — invariant #8's durable state is the
///   history DB and the tree itself, not this cache). The caller surfaces the
///   `recovered` flag to the user.
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
    match postcard::from_bytes(&bytes) {
        Ok(index) => Ok((index, false)),
        // Undecodable (older format / corruption): fall back to empty + rescan.
        Err(_) => Ok((Index::new(), true)),
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
