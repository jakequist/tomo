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
//! # M1 scope
//! [`Message::Change`] carries whole file bytes inline for a `Modified` change.
//! Chunked, content-addressed transfer that never resends a chunk the peer
//! already holds is M3 (docs/SPEC.md §6.1, §8); until then a change is
//! self-contained and the frame is as large as the file.

use serde::{Deserialize, Serialize};
use tomo_engine::{Index, RemoteChange, ReplicaId};

/// The wire protocol version, negotiated in [`Message::Hello`].
///
/// Bumped whenever the framing or any [`Message`] variant changes shape in a
/// way an older peer could not decode. Two replicas must agree on this value
/// before exchanging indices.
pub const PROTOCOL_VERSION: u16 = 1;

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
    /// `bytes` carries the file's full content for a `Modified` change (M1
    /// ships whole files inline; chunked CAS transfer is M3). It is `None` for
    /// a `Removed` change, which needs no content.
    Change {
        /// The change, with its originating vector clock (causality evidence).
        change: RemoteChange,
        /// Full file content for a `Modified` change; `None` for `Removed`.
        bytes: Option<Vec<u8>>,
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
