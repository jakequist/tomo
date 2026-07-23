//! Pure bookkeeping for chunked, interleaved content transfer (docs/SPEC.md §8).
//!
//! Large `Modified` content ([`tomo_proto::INLINE_THRESHOLD`] and up) ships as a
//! [`tomo_proto::Message::ChangeManifest`] listing its `FastCDC` chunk hashes;
//! the receiver pulls the chunks it lacks in batches and reassembles them. The
//! decisions that drive that dance — which chunks are still missing, how to
//! batch the requests, when the assembly is complete, whether a new change
//! supersedes an in-flight one, and where an [`tomo_engine::Action::Apply`]'s
//! bytes should come from — are all pure functions kept here so they can be
//! unit-tested without any I/O, threads, or a live transport.
//!
//! The I/O that surrounds them (writing chunk files, reassembling, applying)
//! lives in [`crate::session`].

use std::collections::HashSet;

use tomo_engine::RelPath;
use tomo_proto::ChunkHash;

/// How many chunk hashes the receiver asks for per [`tomo_proto::Message::ChunkRequest`].
///
/// One batch is at most ~8 MiB in flight (32 × 256 KiB max chunk), enough to
/// keep the pipe busy without unbounded outstanding requests.
pub const REQUEST_BATCH: usize = 32;

/// The chunk hashes from `manifest`, in order and de-duplicated, that are not
/// yet in `have`.
///
/// De-duplication matters because content-defined chunking can repeat a chunk
/// within one file; we never request or count the same hash twice.
#[must_use]
pub fn missing_chunks(manifest: &[ChunkHash], have: &HashSet<ChunkHash>) -> Vec<ChunkHash> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for h in manifest {
        if !have.contains(h) && seen.insert(*h) {
            out.push(*h);
        }
    }
    out
}

/// The next batch of at most `REQUEST_BATCH` hashes to request: manifest hashes
/// that are neither already held (`have`) nor already outstanding (`requested`),
/// in manifest order, de-duplicated.
///
/// Returns an empty vector when nothing new needs requesting (everything is
/// either held or already in flight).
#[must_use]
pub fn next_request_batch(
    manifest: &[ChunkHash],
    have: &HashSet<ChunkHash>,
    requested: &HashSet<ChunkHash>,
) -> Vec<ChunkHash> {
    missing_chunks(manifest, have)
        .into_iter()
        .filter(|h| !requested.contains(h))
        .take(REQUEST_BATCH)
        .collect()
}

/// Whether every chunk named by `manifest` has been received into `have`.
#[must_use]
pub fn is_complete(manifest: &[ChunkHash], have: &HashSet<ChunkHash>) -> bool {
    manifest.iter().all(|h| have.contains(h))
}

/// Whether an in-flight assembly for `assembly_path` is superseded — and must be
/// abandoned — by an incoming change for `incoming_path`.
///
/// A newer change for the same path always wins (invariant #3 ships the latest
/// bytes); the stale assembly's partial chunks are discarded. Changes to *other*
/// paths never disturb an assembly.
#[must_use]
pub fn supersedes(assembly_path: &RelPath, incoming_path: &RelPath) -> bool {
    assembly_path == incoming_path
}

/// Where the bytes for a sig-addressed [`tomo_engine::Action::Apply`] should come
/// from, in strict preference order (docs/NOTES.md — the multi-head apply fix).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteSource {
    /// (a) The triggering frame's bytes hash to the target signature — use them.
    Frame,
    /// (b) The current on-disk content already hashes to the target signature —
    /// the file is already correct, so skip the write entirely.
    DiskSkip,
    /// (c) Neither the frame nor disk match, but the content-addressed history
    /// store holds a version with this signature — source the bytes from it.
    Cas,
    /// (d) None of the above — the bytes are unavailable; warn and reconcile via
    /// a rescan rather than writing wrong content.
    Unavailable,
}

/// Choose the byte source for an `Apply { Present(sig) }` given whether each
/// candidate source matches the target signature, in the a/b/c/d order.
#[must_use]
pub fn byte_source(frame_matches: bool, disk_matches: bool, cas_available: bool) -> ByteSource {
    if frame_matches {
        ByteSource::Frame
    } else if disk_matches {
        ByteSource::DiskSkip
    } else if cas_available {
        ByteSource::Cas
    } else {
        ByteSource::Unavailable
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn h(n: u8) -> ChunkHash {
        [n; 32]
    }

    fn have(hs: &[u8]) -> HashSet<ChunkHash> {
        hs.iter().map(|n| h(*n)).collect()
    }

    #[test]
    fn missing_is_ordered_and_deduplicated() {
        let manifest = [h(1), h(2), h(2), h(3), h(1)];
        let held = have(&[2]);
        // 2 held; 1 and 3 missing, each once, in first-seen manifest order.
        assert_eq!(missing_chunks(&manifest, &held), vec![h(1), h(3)]);
    }

    #[test]
    fn missing_empty_when_all_held() {
        let manifest = [h(1), h(2)];
        assert!(missing_chunks(&manifest, &have(&[1, 2])).is_empty());
    }

    #[test]
    fn request_batch_caps_and_excludes_held_and_in_flight() {
        let manifest: Vec<ChunkHash> = (0..40u8).map(h).collect();
        let held = have(&[0, 1]);
        let requested = have(&[2, 3]);
        let batch = next_request_batch(&manifest, &held, &requested);
        assert_eq!(batch.len(), REQUEST_BATCH);
        // Starts at the first not-held, not-requested hash (4).
        assert_eq!(batch[0], h(4));
        assert!(!batch.contains(&h(0)));
        assert!(!batch.contains(&h(2)));
    }

    #[test]
    fn request_batch_empty_when_nothing_new() {
        let manifest = [h(1), h(2)];
        let batch = next_request_batch(&manifest, &have(&[1]), &have(&[2]));
        assert!(batch.is_empty());
    }

    #[test]
    fn completion_detected_only_when_all_present() {
        let manifest = [h(1), h(2), h(3)];
        assert!(!is_complete(&manifest, &have(&[1, 2])));
        assert!(is_complete(&manifest, &have(&[1, 2, 3])));
        // Duplicate chunk: holding the one distinct hash completes it.
        let dup = [h(5), h(5)];
        assert!(is_complete(&dup, &have(&[5])));
    }

    #[test]
    fn supersede_is_same_path_only() {
        let a = RelPath::new("a/big.bin").expect("path");
        let a2 = RelPath::new("a/big.bin").expect("path");
        let b = RelPath::new("a/other.bin").expect("path");
        assert!(supersedes(&a, &a2));
        assert!(!supersedes(&a, &b));
    }

    #[test]
    fn byte_source_follows_abcd_order() {
        // (a) frame wins over everything.
        assert_eq!(byte_source(true, true, true), ByteSource::Frame);
        // (b) disk when frame misses.
        assert_eq!(byte_source(false, true, true), ByteSource::DiskSkip);
        // (c) CAS when frame and disk miss.
        assert_eq!(byte_source(false, false, true), ByteSource::Cas);
        // (d) nothing left.
        assert_eq!(byte_source(false, false, false), ByteSource::Unavailable);
    }
}
