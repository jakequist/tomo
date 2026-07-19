//! Byte transport for the sync session.
//!
//! The engine and session are transport-agnostic; this module supplies the
//! three transports and the shared plumbing:
//! - **local peer** (`watch --local-peer <path>`): spawn `<self> serve --stdio`
//!   with its working directory at the peer project root and speak frames over
//!   the child's stdin/stdout (roadmap M1).
//! - **stdio** (`serve --stdio`): our *own* stdin/stdout is the channel.
//! - **ssh** (`watch` with a `[remote]`): connect over SSH, bootstrap the remote
//!   binary, spawn `serve --stdio` on it, and frame over the tunneled channel
//!   (roadmap M2). The async russh machinery is confined to `tomo-transport`
//!   behind a blocking `Read`/`Write` facade, so this module treats it exactly
//!   like the local child.
//!
//! A background reader thread reassembles frames with a
//! [`tomo_proto::FrameDecoder`] and forwards decoded [`Message`]s (and terminal
//! conditions) to the session's unified channel.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::JoinHandle;

use tomo_proto::{encode, FrameDecoder, Message};

use crate::buildinfo;
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

/// A lifetime guard for a transport's underlying process/session. Dropping it
/// tears the connection down; some guards can surface a captured stderr tail.
trait Guard: Send {
    /// The remote process's captured stderr, if this transport has one.
    fn stderr_tail(&self) -> Option<String> {
        None
    }

    /// Host-key policy notes gathered during connect (SSH transport only).
    fn notes(&self) -> Vec<String> {
        Vec::new()
    }
}

/// A live transport: the send half, the reader thread handle, the liveness flag
/// that lets us retire a stale reader silently, and the connection guard whose
/// `Drop` tears the peer down.
pub struct Transport {
    /// The send half (owned by the session thread).
    pub tx: FrameWriter,
    /// Shared counters, also read by the status writer.
    pub counters: Arc<Counters>,
    reader: Option<JoinHandle<()>>,
    /// Set by the reader thread just before it exits. Joining is only safe
    /// when this is true: a reader blocked in a blocking `read()` (e.g. serve
    /// mode's stdin while the parent still holds the pipe open) never returns,
    /// and joining it deadlocks shutdown forever — turning a fatal session
    /// error into a zombie process whose error report is never printed (found
    /// via storm-test gdb stacks).
    reader_done: Arc<AtomicBool>,
    /// When cleared, the reader thread exits without emitting a terminal
    /// [`Incoming`] — used to retire a superseded transport during the SSH
    /// re-push retry without injecting a stale `PeerEof` into the new session.
    alive: Arc<AtomicBool>,
    guard: Option<Box<dyn Guard>>,
}

impl Transport {
    /// Join the reader thread if it has already finished; otherwise deactivate
    /// it and detach (process exit reaps it). NEVER blocks on a reader that is
    /// still inside a blocking `read()`.
    pub fn join_reader(&mut self) {
        if let Some(handle) = self.reader.take() {
            if self.reader_done.load(Ordering::Relaxed) {
                let _ = handle.join();
            } else {
                self.deactivate();
                drop(handle);
            }
        }
    }

    /// Mark this transport dead so its reader thread stops forwarding events.
    /// Call before dropping a transport that is being replaced.
    pub fn deactivate(&self) {
        self.alive.store(false, Ordering::Relaxed);
    }

    /// The remote process's captured stderr tail, if any (SSH transport only).
    pub fn stderr_tail(&self) -> Option<String> {
        self.guard.as_ref().and_then(|g| g.stderr_tail())
    }

    /// Host-key policy notes gathered during connect (SSH transport only).
    pub fn notes(&self) -> Vec<String> {
        self.guard.as_ref().map(|g| g.notes()).unwrap_or_default()
    }
}

/// Build a [`Transport`] from a raw reader/writer pair, spawning the reader
/// thread that forwards decoded messages to `incoming`.
fn build(
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    guard: Option<Box<dyn Guard>>,
    incoming: &Sender<Incoming>,
) -> Transport {
    let counters = Arc::new(Counters::default());
    let alive = Arc::new(AtomicBool::new(true));
    let reader_done = Arc::new(AtomicBool::new(false));
    let reader_handle = spawn_reader(
        reader,
        Arc::clone(&counters),
        Arc::clone(&alive),
        Arc::clone(&reader_done),
        incoming.clone(),
    );
    Transport {
        tx: FrameWriter {
            writer,
            counters: Arc::clone(&counters),
        },
        counters,
        reader: Some(reader_handle),
        reader_done,
        alive,
        guard,
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
        Some(Box::new(ChildGuard(child))),
        incoming,
    ))
}

/// Everything needed to (re)establish the SSH transport, kept so the session can
/// re-run the bootstrap with `force_push` on a version-mismatch retry.
#[derive(Debug, Clone)]
pub struct SshParams {
    /// The `user@host[:port]` target.
    pub target: String,
    /// The remote project-root path.
    pub remote_path: String,
    /// SSH auth / host-key options.
    pub opts: tomo_transport::SshOpts,
    /// The effective local version (feeds both bootstrap naming and `Hello`).
    pub version: String,
}

impl SshParams {
    /// Build the SSH parameters for a configured `[remote]`, resolving the local
    /// user's `~/.ssh` (keys, `known_hosts`) and login name from the environment.
    ///
    /// Identity keys are tried after ssh-agent in this order: the recorded
    /// `--identity` (if any), then the `IdentityFile`s that `~/.ssh/config`
    /// declares for the target host, then the built-in `id_ed25519`/`id_rsa`
    /// defaults. This lets `tomo` authenticate wherever the user's own `ssh`
    /// already can (e.g. a global or `Host`-scoped `IdentityFile`), instead of
    /// only the two default names — the common reason `tomo connect` failed on a
    /// machine where `ssh` succeeded.
    ///
    /// # Errors
    /// [`CliError::Message`] if `$HOME` is unset (there is nowhere to find SSH
    /// keys or `known_hosts`).
    pub fn from_remote(remote: &tomo_config::Remote) -> Result<Self, CliError> {
        let home = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| {
                CliError::msg(
                    "cannot locate the home directory ($HOME is unset) to find SSH keys and \
                     known_hosts",
                )
            })?;
        // Falls back to an empty name; it is only consulted when the target
        // omits `user@`, and an empty user makes russh use the login default.
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .unwrap_or_default();
        let mut opts = tomo_transport::SshOpts::new(&home, &user);
        opts.identity_files = Self::resolve_identity_files(remote, &home, &opts.identity_files);
        Ok(SshParams {
            target: remote.host.clone(),
            remote_path: remote.path.clone(),
            opts,
            version: buildinfo::binary_version(),
        })
    }

    /// Assemble the ordered, de-duplicated identity-file list: the recorded
    /// `--identity` first, then any `~/.ssh/config` `IdentityFile`s for the
    /// target host, then the built-in `defaults`. Reading `~/.ssh/config` is
    /// best-effort — a missing or unreadable file simply contributes nothing, so
    /// auth still falls back to agent + defaults.
    fn resolve_identity_files(
        remote: &tomo_config::Remote,
        home: &std::path::Path,
        defaults: &[std::path::PathBuf],
    ) -> Vec<std::path::PathBuf> {
        // The config `Host` patterns match on the bare hostname; strip any
        // `user@`/`:port`. If the target does not parse, match on it verbatim.
        let host = tomo_transport::HostSpec::parse(&remote.host)
            .map_or_else(|_| remote.host.clone(), |spec| spec.host);

        let mut ordered: Vec<std::path::PathBuf> = Vec::new();
        if let Some(id) = &remote.identity {
            ordered.push(expand_tilde(id, home));
        }
        let config_path = home.join(".ssh").join("config");
        if let Ok(content) = std::fs::read_to_string(&config_path) {
            ordered.extend(tomo_transport::identity_files_for(&content, &host, home));
        }
        ordered.extend_from_slice(defaults);

        // De-duplicate, preserving first-seen order (agent-then-keys semantics
        // mean the first working key wins, so order matters).
        let mut seen = std::collections::HashSet::new();
        ordered.retain(|p| seen.insert(p.clone()));
        ordered
    }
}

/// Describe the endpoint the target resolves to through `~/.ssh/config`, for the
/// connect log line — e.g. `vm1 (10.0.0.71 via p1)`, or just `vm1` when nothing
/// was rewritten and there are no jumps. Resolution failures fall back to the
/// bare target (the connect attempt then surfaces the real error).
pub fn describe_route(params: &SshParams) -> String {
    tomo_transport::resolve_route(&params.target, &params.opts)
        .map_or_else(|_| params.target.clone(), |r| r.describe())
}

/// Expand a leading `~/` (or a bare `~`) in a user-supplied path against `home`.
/// Other paths are returned unchanged.
fn expand_tilde(path: &str, home: &std::path::Path) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else if path == "~" {
        home.to_path_buf()
    } else {
        std::path::PathBuf::from(path)
    }
}

/// Connect over SSH, bootstrap the remote binary (pushing when needed), spawn
/// `serve --stdio`, and wrap the tunneled channel as a [`Transport`].
///
/// `force_push` re-pushes even on an exact version match (used by the retry).
/// Returns the transport and the bootstrap report so the caller can summarize
/// what happened (pushed vs reused, triple, dev substitution).
///
/// # Errors
/// Any [`CliError::Transport`] from connect, auth, host-key verification,
/// bootstrap, or the remote spawn.
pub fn ssh(
    params: &SshParams,
    incoming: &Sender<Incoming>,
    force_push: bool,
) -> Result<(Transport, tomo_transport::BootstrapReport), CliError> {
    let session = tomo_transport::SshSession::connect(&params.target, &params.opts)?;
    // Resolve a leading `~` in the remote path against the remote home (SFTP
    // realpath) before any mkdir/bootstrap/serve-spawn. A non-`~` path is
    // returned unchanged with no extra round-trip.
    let remote_path = session.expand_remote_path(&params.remote_path)?;
    let report = session.bootstrap(
        &remote_path,
        buildinfo::BUILD_TARGET,
        &params.version,
        force_push,
        buildinfo::DEV_BUILD,
    )?;
    let channel = session.spawn_remote(&remote_path, report.binary_rel())?;
    let (reader, writer, guard) = channel.into_parts();
    let transport = build(
        Box::new(reader),
        Box::new(writer),
        Some(Box::new(SshGuard(guard))),
        incoming,
    );
    Ok((transport, report))
}

/// Spawn the reader thread: read bytes, reassemble frames, forward each decoded
/// [`Message`] as [`Incoming::Message`]; on EOF send [`Incoming::PeerEof`], and
/// on a fatal framing error send [`Incoming::ProtoError`]. A cleared `alive`
/// flag suppresses the terminal signal so a retired transport stays silent.
fn spawn_reader(
    reader: Box<dyn Read + Send>,
    counters: Arc<Counters>,
    alive: Arc<AtomicBool>,
    reader_done: Arc<AtomicBool>,
    incoming: Sender<Incoming>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        read_loop(reader, &counters, &alive, &incoming);
        // Joining is only safe once this is set (see Transport::join_reader).
        reader_done.store(true, Ordering::Relaxed);
    })
}

fn read_loop(
    mut reader: Box<dyn Read + Send>,
    counters: &Counters,
    alive: &AtomicBool,
    incoming: &Sender<Incoming>,
) {
    {
        let mut decoder = FrameDecoder::new();
        // Heap-allocated read buffer (a large on-stack array trips clippy and
        // needlessly grows the thread's stack frame).
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => {
                    if alive.load(Ordering::Relaxed) {
                        let _ = incoming.send(Incoming::PeerEof);
                    }
                    return;
                }
                Ok(n) => n,
                Err(e) => {
                    if alive.load(Ordering::Relaxed) {
                        let _ = incoming.send(Incoming::ProtoError(format!("transport read: {e}")));
                    }
                    return;
                }
            };
            counters.bytes_recv.fetch_add(n as u64, Ordering::Relaxed);
            decoder.push(&buf[..n]);
            loop {
                match decoder.next() {
                    Ok(Some(msg)) => {
                        counters.frames_recv.fetch_add(1, Ordering::Relaxed);
                        if !alive.load(Ordering::Relaxed)
                            || incoming.send(Incoming::Message(msg)).is_err()
                        {
                            return; // retired or session gone
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        if alive.load(Ordering::Relaxed) {
                            let _ = incoming.send(Incoming::ProtoError(e.to_string()));
                        }
                        return;
                    }
                }
            }
        }
    }
}

/// Owns the spawned local-peer child and reaps it on drop (invariant: `watch`
/// kills its child when it exits).
struct ChildGuard(Child);

impl Guard for ChildGuard {}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // The child may already have exited; ignore errors.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Owns the SSH session, runtime, and bridge tasks for the SSH transport;
/// dropping it tears the session down (its own `Drop`). Also surfaces the
/// remote process's captured stderr for error reporting.
struct SshGuard(tomo_transport::RemoteGuard);

impl Guard for SshGuard {
    fn stderr_tail(&self) -> Option<String> {
        let tail = self.0.stderr_tail();
        if tail.trim().is_empty() {
            None
        } else {
            Some(tail)
        }
    }

    fn notes(&self) -> Vec<String> {
        self.0.notes().to_vec()
    }
}
