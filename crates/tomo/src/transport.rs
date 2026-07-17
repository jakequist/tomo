//! Byte transport for the sync session.
//!
//! The engine and session are transport-agnostic; this module supplies the two
//! M1 transports and the shared plumbing:
//! - **local peer** (`watch --local-peer <path>`): spawn `<self> serve --stdio`
//!   with its working directory at the peer project root and speak frames over
//!   the child's stdin/stdout. This proves the full two-process sync loop before
//!   the SSH transport exists (roadmap M1 → M2).
//! - **stdio** (`serve --stdio`): our *own* stdin/stdout is the channel.
//!
//! A background reader thread reassembles frames with a
//! [`tomo_proto::FrameDecoder`] and forwards decoded [`Message`]s (and terminal
//! conditions) to the session's unified channel.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::JoinHandle;

use tomo_proto::{encode, FrameDecoder, Message};

use crate::error::CliError;
use crate::session::Incoming;
use crate::status::Net;

/// Live, atomically-updated network counters shared between the writer (session
/// thread) and the reader thread.
#[derive(Debug, Default)]
pub struct Counters {
    frames_sent: AtomicU64,
    frames_recv: AtomicU64,
    bytes_sent: AtomicU64,
    bytes_recv: AtomicU64,
}

impl Counters {
    /// A point-in-time snapshot for the status file.
    pub fn snapshot(&self) -> Net {
        Net {
            frames_sent: self.frames_sent.load(Ordering::Relaxed),
            frames_recv: self.frames_recv.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            bytes_recv: self.bytes_recv.load(Ordering::Relaxed),
        }
    }
}

/// The writable half of a transport plus its counters. Owned by the session
/// thread, which is the only writer.
pub struct FrameWriter {
    writer: Box<dyn Write + Send>,
    counters: Arc<Counters>,
}

impl FrameWriter {
    /// Frame and write `msg` to the peer, updating the sent counters.
    ///
    /// # Errors
    /// [`CliError::Proto`] if the message cannot be encoded, or [`CliError::Io`]
    /// if the write or flush fails.
    pub fn send(&mut self, msg: &Message) -> Result<(), CliError> {
        let frame = encode(msg)?;
        self.writer
            .write_all(&frame)
            .map_err(|s| CliError::io("write frame", "<peer>", s))?;
        self.writer
            .flush()
            .map_err(|s| CliError::io("flush frame", "<peer>", s))?;
        self.counters.frames_sent.fetch_add(1, Ordering::Relaxed);
        self.counters
            .bytes_sent
            .fetch_add(frame.len() as u64, Ordering::Relaxed);
        Ok(())
    }
}

/// A live transport: the send half, the reader thread handle, and (for the
/// local-peer transport) the child process guard whose `Drop` reaps it.
pub struct Transport {
    /// The send half (owned by the session thread).
    pub tx: FrameWriter,
    /// Shared counters, also read by the status writer.
    pub counters: Arc<Counters>,
    reader: Option<JoinHandle<()>>,
    _child: Option<ChildGuard>,
}

impl Transport {
    /// Join the reader thread (called during shutdown after EOF).
    pub fn join_reader(&mut self) {
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }
}

/// Build a [`Transport`] from a raw reader/writer pair, spawning the reader
/// thread that forwards decoded messages to `incoming`.
fn build(
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    child: Option<ChildGuard>,
    incoming: &Sender<Incoming>,
) -> Transport {
    let counters = Arc::new(Counters::default());
    let reader_handle = spawn_reader(reader, Arc::clone(&counters), incoming.clone());
    Transport {
        tx: FrameWriter {
            writer,
            counters: Arc::clone(&counters),
        },
        counters,
        reader: Some(reader_handle),
        _child: child,
    }
}

/// The stdio transport used by `serve --stdio`: our own stdin/stdout is the wire.
pub fn stdio(incoming: &Sender<Incoming>) -> Transport {
    build(
        Box::new(std::io::stdin()),
        Box::new(std::io::stdout()),
        None,
        incoming,
    )
}

/// The local-peer transport used by `watch --local-peer <path>`: spawn
/// `<self> serve --stdio` rooted at `peer_path` and frame over its stdio.
///
/// # Errors
/// [`CliError::Message`] if the current executable cannot be located or the
/// child's pipes cannot be captured; [`CliError::Io`] if the spawn fails.
pub fn local_peer(
    peer_path: &std::path::Path,
    incoming: &Sender<Incoming>,
) -> Result<Transport, CliError> {
    let exe = std::env::current_exe()
        .map_err(|e| CliError::msg(format!("cannot locate the tomo executable: {e}")))?;

    let mut child = Command::new(&exe)
        .arg("serve")
        .arg("--stdio")
        .current_dir(peer_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|s| CliError::io("spawn local peer", &exe, s))?;

    let child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| CliError::msg("failed to capture local peer stdin"))?;
    let child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| CliError::msg("failed to capture local peer stdout"))?;

    Ok(build(
        Box::new(child_stdout),
        Box::new(child_stdin),
        Some(ChildGuard(child)),
        incoming,
    ))
}

/// Spawn the reader thread: read bytes, reassemble frames, forward each decoded
/// [`Message`] as [`Incoming::Message`]; on EOF send [`Incoming::PeerEof`], and
/// on a fatal framing error send [`Incoming::ProtoError`].
fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    counters: Arc<Counters>,
    incoming: Sender<Incoming>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut decoder = FrameDecoder::new();
        // Heap-allocated read buffer (a large on-stack array trips clippy and
        // needlessly grows the thread's stack frame).
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => {
                    let _ = incoming.send(Incoming::PeerEof);
                    return;
                }
                Ok(n) => n,
                Err(e) => {
                    let _ = incoming.send(Incoming::ProtoError(format!("transport read: {e}")));
                    return;
                }
            };
            counters.bytes_recv.fetch_add(n as u64, Ordering::Relaxed);
            decoder.push(&buf[..n]);
            loop {
                match decoder.next() {
                    Ok(Some(msg)) => {
                        counters.frames_recv.fetch_add(1, Ordering::Relaxed);
                        if incoming.send(Incoming::Message(msg)).is_err() {
                            return; // session gone
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        let _ = incoming.send(Incoming::ProtoError(e.to_string()));
                        return;
                    }
                }
            }
        }
    })
}

/// Owns the spawned local-peer child and reaps it on drop (invariant: `watch`
/// kills its child when it exits).
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // The child may already have exited; ignore errors.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
