//! Wire protocol: length-prefixed frames, handshake, change streaming
//! (docs/SPEC.md §8).
//!
//! Pure crate (no I/O): message types plus their framing and (de)serialization
//! only. The `tomo-transport` crate owns the SSH stdio channel and moves the
//! bytes this crate produces and consumes.
//!
//! # Layout
//! - [`message`] — the [`Message`] enum and the session flow it encodes.
//! - [`frame`] — length-prefixed [`encode`](frame::encode) and the push-based
//!   [`FrameDecoder`](frame::FrameDecoder) for stream reassembly.
//! - [`error`] — the fatal [`ProtoError`].

pub mod error;
pub mod frame;
pub mod message;

pub use error::ProtoError;
pub use frame::{encode, FrameDecoder, MAX_FRAME_LEN};
pub use message::{ChunkHash, Message, INLINE_THRESHOLD, PROTOCOL_VERSION};
