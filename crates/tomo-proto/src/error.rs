//! Protocol errors.
//!
//! All errors here are *fatal to the stream*: the wire protocol is not
//! resynchronizable, so a framing or decode failure means the session must be
//! torn down rather than skipped past. `tomo-transport` renders the eventual
//! user-facing message; this crate only returns data (no I/O, no printing).

use thiserror::Error;

/// Something went wrong encoding or decoding the wire protocol.
///
/// Every variant is unrecoverable for the current stream: once framing is lost
/// there is no safe resynchronization point, so the caller must close the
/// session.
#[derive(Debug, Error)]
pub enum ProtoError {
    /// A frame declared (or would declare) a payload larger than
    /// [`crate::frame::MAX_FRAME_LEN`]. On decode this means a corrupt or
    /// hostile length prefix; on encode it means a message serialized larger
    /// than the defensive bound allows.
    #[error("frame length {len} exceeds maximum {max}", max = crate::frame::MAX_FRAME_LEN)]
    FrameTooLarge {
        /// The offending declared payload length in bytes.
        len: u32,
    },
    /// A frame's payload bytes could not be decoded into a [`crate::Message`].
    #[error("failed to decode message payload: {0}")]
    Decode(#[source] postcard::Error),
    /// A [`crate::Message`] could not be serialized to bytes.
    #[error("failed to encode message: {0}")]
    Encode(#[source] postcard::Error),
}
