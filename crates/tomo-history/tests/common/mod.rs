//! Shared helpers for the `tomo-history` integration tests.
#![allow(dead_code)] // each test file uses a subset

use tempfile::TempDir;
use tomo_engine::{ContentHash, ContentSig, EntryState, RelPath, ReplicaId, VectorClock};
use tomo_history::{HistoryStore, Origin, VersionId};

/// Open a fresh store in a throwaway temp directory, returning both so the
/// directory outlives the store.
pub fn fresh_store() -> (TempDir, HistoryStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = HistoryStore::open(dir.path()).expect("open store");
    (dir, store)
}

/// The BLAKE3 content signature of `bytes`, exactly as the store computes it.
pub fn sig_of(bytes: &[u8]) -> ContentSig {
    ContentSig {
        hash: ContentHash(*blake3::hash(bytes).as_bytes()),
        size: bytes.len() as u64,
    }
}

/// A path from a `&str`, panicking on an invalid one (test convenience).
pub fn rp(s: &str) -> RelPath {
    RelPath::new(s).expect("valid path")
}

/// A single-replica clock ticked `n` times — a stand-in for a per-path version
/// stream on one replica.
pub fn clock_at(replica: u64, n: u64) -> VectorClock {
    let mut c = VectorClock::new();
    for _ in 0..n {
        c.tick(ReplicaId(replica));
    }
    c
}

/// Record a present version of `path` with `bytes`, computing its signature.
pub fn record_present(
    store: &mut HistoryStore,
    path: &RelPath,
    bytes: &[u8],
    replica: u64,
    tick: u64,
) -> VersionId {
    store
        .record_version(
            path,
            &EntryState::Present(sig_of(bytes)),
            &clock_at(replica, tick),
            ReplicaId(replica),
            Origin::Local,
            0,
            Some(bytes),
        )
        .expect("record present version")
}

/// A deterministic pseudorandom byte vector of length `len`, seeded by `seed`.
///
/// A tiny xorshift keeps the tests reproducible without pulling in `rand`.
pub fn pseudorandom(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}
