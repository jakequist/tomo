//! Wire message types and the session flow they encode (docs/SPEC.md §8).
//!
//! A Tomo session between two replicas is a symmetric exchange over the SSH
//! stdio channel. Both directions speak the same [`Message`] enum; there is no
//! client/server asymmetry at this layer. The `tomo-transport` crate moves the
//! framed bytes (see [`crate::frame`]); this module is pure data.
//!
//! # Session flow
//! 1. **[`Message::Hello`]** — the first message each side sends, unprompted, in
//!    both directions. It negotiates the [`PROTOCOL_VERSION`] and announces the
//!    sender's binary version and [`ReplicaId`]. A peer whose `protocol` differs
//!    is incompatible; the transport layer decides how to react (M1 rejects).
//! 2. **[`Message::IndexExchange`]** — after Hello, each side ships its full
//!    [`Index`]. The engine reconciles the two indices to compute what each peer
//!    is missing (fast-forward, no-op, or conflict per invariant #5).
//! 3. **Steady state** — a stream of [`Message::Change`] notifications as files
//!    change, interleaved with [`Message::Ping`] / [`Message::Pong`] liveness
//!    and quiesce probes. This continues for the life of the session.
//!
//! # Content transfer (updated during M5)
//! [`Message::Change`] carries whole file bytes inline for a `Modified` change
//! **only while the content is below [`INLINE_THRESHOLD`]** (1 MiB). Larger
//! content ships out-of-band as a [`Message::ChangeManifest`] (the change plus
//! an ordered list of `FastCDC` chunk hashes), after which the receiver pulls
//! the chunks it lacks with [`Message::ChunkRequest`] and the sender serves
//! them as a stream of [`Message::ChunkData`] frames. Chunk frames are shipped
//! in small interleaved batches so a live small-file `Change` never blocks
//! head-of-line behind a bulk transfer (docs/SPEC.md §8).

use serde::{Deserialize, Serialize};
use tomo_engine::{Index, RemoteChange, ReplicaId};

/// The wire protocol version, negotiated in [`Message::Hello`].
///
/// Bumped whenever the framing or any [`Message`] variant changes shape in a
/// way an older peer could not decode. Two replicas must agree on this value
/// before exchanging indices.
///
/// The M5 chunk-transfer variants ([`Message::ChangeManifest`],
/// [`Message::ChunkRequest`], [`Message::ChunkData`]) were added while the
/// protocol was still unshipped, so they did not move it off `1`.
///
/// **v2** adds the executable bit to `tomo_engine::ContentSig` (git's model),
/// so every `Modified` change and the whole [`Message::IndexExchange`] payload
/// gain one byte per present signature — a shape an older `postcard` decoder
/// would misread. The bump is safe for the SSH bootstrap: an exact-version
/// match reuses the pushed peer binary and *any* mismatch re-pushes a fresh one
/// (docs/SPEC.md §3), and the `Hello` handshake re-checks the binary version and
/// re-pushes on a mid-upgrade skew before any index is exchanged — so after a
/// successful handshake both ends always speak the same protocol version.
pub const PROTOCOL_VERSION: u16 = 2;

/// Inline-content threshold in bytes (1 MiB). A `Modified` [`Message::Change`]
/// carries its bytes inline only while the content is strictly smaller than
/// this; at or above it, the change ships as a [`Message::ChangeManifest`] and
/// the bytes are pulled chunk-by-chunk (docs/SPEC.md §8).
pub const INLINE_THRESHOLD: usize = 1024 * 1024;

/// A 32-byte chunk (or whole-file) hash as it travels on the wire: the raw
/// BLAKE3 digest, matching `tomo_engine::ContentHash`'s inner bytes and the
/// chunk keys `tomo-history` stores.
pub type ChunkHash = [u8; 32];

/// A single protocol message, identical in both directions of the session.
///
/// Encoded to bytes by [`crate::frame::encode`] and recovered by
/// [`crate::frame::FrameDecoder`]. See the [module docs](self) for the flow the
/// variants participate in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    /// First message in each direction. Negotiates the protocol version and
    /// announces who the sender is.
    ///
    /// `binary_version` is the sender's `CARGO_PKG_VERSION`, carried for
    /// diagnostics and future compatibility shims; it is *not* the ordering
    /// authority (that is always vector clocks, invariant #7).
    Hello {
        /// The protocol version the sender speaks (see [`PROTOCOL_VERSION`]).
        protocol: u16,
        /// The sender's binary version (`CARGO_PKG_VERSION`).
        binary_version: String,
        /// The sender's stable replica identity.
        replica: ReplicaId,
    },
    /// The sender's full [`Index`], shipped once after [`Message::Hello`] as the
    /// input to reconciliation.
    IndexExchange(Index),
    /// A change notification for one path.
    ///
    /// `bytes` carries the file's full content for a `Modified` change whose
    /// content is below [`INLINE_THRESHOLD`]; larger `Modified` content is sent
    /// as a [`Message::ChangeManifest`] instead. It is `None` for a `Removed`
    /// change, which needs no content.
    Change {
        /// The change, with its originating vector clock (causality evidence).
        change: RemoteChange,
        /// Inline file content for a small `Modified` change; `None` for
        /// `Removed`.
        bytes: Option<Vec<u8>>,
    },
    /// A `Modified` change whose content is at or above [`INLINE_THRESHOLD`],
    /// announced by its ordered chunk manifest instead of inline bytes.
    ///
    /// The receiver records an in-progress assembly, pulls the chunks it lacks
    /// with [`Message::ChunkRequest`], and applies the change once every chunk
    /// has arrived and the reassembled whole-file hash matches the change's
    /// [`tomo_engine::ContentSig`]. The sender retains no per-transfer state:
    /// it serves chunk requests by re-reading and re-chunking the current file.
    ChangeManifest {
        /// The change, with its originating vector clock (causality evidence).
        /// Its kind is always `Modified` with the whole-file signature.
        change: RemoteChange,
        /// The total content size in bytes (equals the change signature size).
        total_size: u64,
        /// The ordered `FastCDC` chunk hashes composing the content.
        manifest: Vec<ChunkHash>,
    },
    /// A request for the chunk bytes identified by `hashes`.
    ///
    /// The receiver batches the hashes it still lacks; the sender answers each
    /// with a [`Message::ChunkData`], silently skipping any hash the current
    /// file no longer contains (the file changed — a fresh change is already on
    /// the way, invariant #3).
    ChunkRequest {
        /// The chunk hashes the requester wants served.
        hashes: Vec<ChunkHash>,
    },
    /// One chunk's bytes, answering a [`Message::ChunkRequest`]. The receiver
    /// verifies `BLAKE3(bytes) == hash` before storing it.
    ChunkData {
        /// The chunk's BLAKE3 hash (its content-addressed identity).
        hash: ChunkHash,
        /// The chunk's raw bytes.
        bytes: Vec<u8>,
    },
    /// Liveness / quiesce probe. The peer answers with [`Message::Pong`]
    /// carrying the same `nonce`.
    Ping {
        /// Opaque token echoed back in the matching [`Message::Pong`].
        nonce: u64,
    },
    /// Reply to a [`Message::Ping`], echoing its `nonce`.
    Pong {
        /// The `nonce` from the [`Message::Ping`] being answered.
        nonce: u64,
    },
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::frame::{encode, FrameDecoder};
    use proptest::prelude::*;
    use tomo_engine::{ChangeKind, ContentHash, ContentSig, RelPath, ReplicaId, VectorClock};

    fn clock() -> VectorClock {
        let mut v = VectorClock::new();
        v.tick(ReplicaId(3));
        v
    }

    fn manifest_change(size: u64) -> RemoteChange {
        RemoteChange {
            path: RelPath::new("big.bin").unwrap(),
            kind: ChangeKind::Modified(ContentSig {
                hash: ContentHash([9; 32]),
                size,
                exec: false,
            }),
            version: clock(),
        }
    }

    /// Feed `msg`'s frame through the decoder in one push and recover it.
    fn round_trip(msg: &Message) -> Message {
        let bytes = encode(msg).unwrap();
        let mut dec = FrameDecoder::new();
        dec.push(&bytes);
        dec.next().unwrap().unwrap()
    }

    #[test]
    fn change_manifest_round_trips() {
        let msg = Message::ChangeManifest {
            change: manifest_change(3 << 20),
            total_size: 3 << 20,
            manifest: vec![[1; 32], [2; 32], [3; 32]],
        };
        assert_eq!(round_trip(&msg), msg);
    }

    #[test]
    fn chunk_request_round_trips() {
        let msg = Message::ChunkRequest {
            hashes: vec![[7; 32], [8; 32]],
        };
        assert_eq!(round_trip(&msg), msg);
    }

    #[test]
    fn chunk_data_round_trips() {
        let msg = Message::ChunkData {
            hash: [4; 32],
            bytes: vec![0, 1, 2, 3, 4, 5],
        };
        assert_eq!(round_trip(&msg), msg);
    }

    proptest! {
        /// The three chunk-transfer messages survive framing even when the
        /// decoder receives the frame split at an arbitrary byte boundary.
        #[test]
        fn chunk_messages_decode_across_arbitrary_splits(
            manifest in proptest::collection::vec(any::<[u8; 32]>(), 0..40),
            hashes in proptest::collection::vec(any::<[u8; 32]>(), 0..40),
            chunk_bytes in proptest::collection::vec(any::<u8>(), 0..4096),
            hash in any::<[u8; 32]>(),
            total in any::<u64>(),
            split in any::<prop::sample::Index>(),
        ) {
            for msg in [
                Message::ChangeManifest {
                    change: manifest_change(total),
                    total_size: total,
                    manifest: manifest.clone(),
                },
                Message::ChunkRequest { hashes: hashes.clone() },
                Message::ChunkData { hash, bytes: chunk_bytes.clone() },
            ] {
                let framed = encode(&msg).unwrap();
                let at = split.index(framed.len() + 1);
                let mut dec = FrameDecoder::new();
                dec.push(&framed[..at]);
                // A partial frame yields nothing yet (unless the split is at the end).
                if at < framed.len() {
                    prop_assert_eq!(dec.next().unwrap(), None);
                }
                dec.push(&framed[at..]);
                prop_assert_eq!(dec.next().unwrap(), Some(msg));
            }
        }
    }
}
