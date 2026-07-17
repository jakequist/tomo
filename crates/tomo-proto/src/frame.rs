//! Length-prefixed framing over the byte stream (docs/SPEC.md §8).
//!
//! Every [`Message`] is serialized with `postcard` and shipped as a frame:
//! a `u32` little-endian payload length followed by exactly that many payload
//! bytes. The framing is pure byte manipulation — no sockets, no files; the
//! `tomo-transport` crate owns the actual stdio channel and feeds the received
//! bytes into a [`FrameDecoder`].
//!
//! # Why push-based decoding
//! The transport delivers bytes in arbitrary chunks (an SSH read may split a
//! frame or coalesce several). [`FrameDecoder`] buffers whatever arrives and
//! yields complete [`Message`]s as they become available, so the caller never
//! has to reason about partial frames.
//!
//! # Errors are fatal
//! A bad length prefix or an undecodable payload means framing is lost, and a
//! length-prefixed stream has no resynchronization point. Such a
//! [`ProtoError`] is terminal: the caller must tear the session down. The
//! decoder does not advance past the offending frame, so it stays in a
//! consistent state (a repeated `next` call reports the same error rather than
//! silently skipping data).

use crate::error::ProtoError;
use crate::message::Message;

/// The number of bytes in the length prefix (`u32`, little-endian).
const LEN_PREFIX: usize = 4;

/// Defensive upper bound on a single frame's payload, in bytes (256 MiB).
///
/// This is a sanity limit that rejects a corrupt or hostile length prefix
/// before allocating for it, not a target size. Real frames shrink once M3
/// replaces whole-file inlining with chunked, content-addressed transfer
/// (docs/SPEC.md §6.1, §8): a change will then carry only the chunks the peer
/// lacks rather than the entire file.
pub const MAX_FRAME_LEN: u32 = 256 * 1024 * 1024;

/// Encode `msg` into a complete frame: the `u32` little-endian length prefix
/// followed by the postcard-serialized payload.
///
/// # Errors
/// - [`ProtoError::Encode`] if `postcard` fails to serialize the message.
/// - [`ProtoError::FrameTooLarge`] if the serialized payload exceeds
///   [`MAX_FRAME_LEN`].
///
/// ```
/// use tomo_proto::{frame, Message, PROTOCOL_VERSION};
/// use tomo_engine::ReplicaId;
/// let msg = Message::Ping { nonce: 7 };
/// let bytes = frame::encode(&msg).expect("encodes");
/// let mut decoder = frame::FrameDecoder::new();
/// decoder.push(&bytes);
/// assert_eq!(decoder.next().expect("no error"), Some(msg));
/// ```
pub fn encode(msg: &Message) -> Result<Vec<u8>, ProtoError> {
    let payload = postcard::to_allocvec(msg).map_err(ProtoError::Encode)?;
    // `usize`-to-`u32`: guard the value, not the cast. A payload at or below
    // MAX_FRAME_LEN fits a u32 by definition; anything larger is rejected here.
    let len =
        u32::try_from(payload.len()).map_err(|_| ProtoError::FrameTooLarge { len: u32::MAX })?;
    if len > MAX_FRAME_LEN {
        return Err(ProtoError::FrameTooLarge { len });
    }
    let mut frame = Vec::with_capacity(LEN_PREFIX + payload.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Incremental, push-based decoder that reassembles [`Message`]s from a byte
/// stream delivered in arbitrary chunks.
///
/// Feed received bytes with [`push`](FrameDecoder::push) and drain complete
/// messages with [`next`](FrameDecoder::next) until it returns `Ok(None)`.
/// Consumed bytes are compacted out of the internal buffer on each push, so
/// memory stays bounded by the largest in-flight frame plus the most recent
/// push.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    /// Bytes received but not yet consumed by a completed message, prefixed by
    /// `pos` already-consumed bytes awaiting compaction.
    buf: Vec<u8>,
    /// Offset into `buf` of the first unconsumed byte.
    pos: usize,
}

impl FrameDecoder {
    /// A fresh decoder with an empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append received `bytes` to the internal buffer.
    ///
    /// Already-consumed bytes are compacted away first, so the buffer never
    /// grows unboundedly across a long-lived session.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.pos > 0 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// Pop the next complete [`Message`] if one is fully buffered.
    ///
    /// Returns `Ok(None)` when the buffer holds only a partial frame — call
    /// again after more bytes arrive via [`push`](FrameDecoder::push).
    ///
    /// # Errors
    /// - [`ProtoError::FrameTooLarge`] if the declared length exceeds
    ///   [`MAX_FRAME_LEN`].
    /// - [`ProtoError::Decode`] if the payload bytes are not a valid
    ///   [`Message`].
    ///
    /// Both are fatal and terminal: the decoder does not advance past the
    /// offending frame, so the stream must be abandoned.
    // Named `next` deliberately (part of the crate's specified API); it returns
    // `Result<Option<_>>`, not an `Iterator::Item`, so it cannot be an Iterator.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Message>, ProtoError> {
        let avail = &self.buf[self.pos..];
        if avail.len() < LEN_PREFIX {
            return Ok(None);
        }
        // Safe: the slice is at least LEN_PREFIX (4) bytes long.
        let len = u32::from_le_bytes([avail[0], avail[1], avail[2], avail[3]]);
        if len > MAX_FRAME_LEN {
            return Err(ProtoError::FrameTooLarge { len });
        }
        let len = len as usize;
        let end = LEN_PREFIX + len;
        if avail.len() < end {
            return Ok(None);
        }
        let payload = &avail[LEN_PREFIX..end];
        // Decode before advancing `pos`: on failure the buffer is untouched, so
        // the error is reported consistently instead of consuming corrupt data.
        let msg = postcard::from_bytes(payload).map_err(ProtoError::Decode)?;
        self.pos += end;
        Ok(Some(msg))
    }
}
