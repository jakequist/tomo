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

/// Load the persisted index from `path`, or an empty index if it does not exist.
///
/// # Errors
/// [`CliError::Io`] if the file exists but cannot be read;
/// [`CliError::Codec`] if its bytes are not a valid serialized [`Index`].
pub fn load_index(path: &Path) -> Result<Index, CliError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Index::new()),
        Err(source) => return Err(CliError::io("read index", path, source)),
    };
    postcard::from_bytes(&bytes).map_err(|source| CliError::Codec {
        context: format!("decode index at {}", path.display()),
        source,
    })
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
        let idx = load_index(&dir.path().join("nope.bin")).unwrap();
        assert!(idx.is_empty());
    }

    #[test]
    fn store_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let path = dir.path().join("index.bin");

        let original = sample_index();
        store_index(&staging, &path, &original).unwrap();
        let loaded = load_index(&path).unwrap();

        assert_eq!(original, loaded);
        assert_eq!(original.canonical_bytes(), loaded.canonical_bytes());
    }

    #[test]
    fn corrupt_bytes_are_a_codec_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.bin");
        std::fs::write(&path, b"\xff\xff not postcard \x00\x01").unwrap();
        assert!(matches!(load_index(&path), Err(CliError::Codec { .. })));
    }
}
