//! The sync loop shared by `watch` and `serve`, parameterized by transport.
//!
//! # Threads
//! One [`std::sync::mpsc`] channel of [`Incoming`] fans in from two sources: the
//! filesystem watcher ([`tomo_watch::Watcher`], via a small forwarder thread) and
//! the transport reader thread ([`crate::transport`]). The **main thread owns the
//! [`Engine`]** and executes every action synchronously — the engine stays a pure
//! state machine (invariant #6) and all ordering is its vector clocks (#7).
//!
//! # Flow
//! 1. Load the persisted index; build the engine.
//! 2. Start the watcher and run a startup [`scan_diff`] so edits made while Tomo
//!    was down are caught **before** the transport connects.
//! 3. Hand-shake: send [`Message::Hello`], await the peer's, then exchange full
//!    indices ([`Message::IndexExchange`]) and reconcile by shipping any local
//!    head the peer's index does not already cover (as content-bearing
//!    [`Message::Change`]s — see [`Session::reconcile`] for why we do not feed
//!    the peer index as [`Event::PeerIndex`]).
//! 4. Steady state: apply remote changes (staging + atomic rename), ship local
//!    changes (dropping any whose bytes went stale — invariant #3), answer pings.
//! 5. On transport EOF, flush the index and status and exit.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use tomo_config::Config;
use tomo_engine::{
    Action, Causality, ChangeKind, Engine, EntryState, Event, Expectation, Index, RelPath,
    RemoteChange, ReplicaId,
};
use tomo_proto::{Message, PROTOCOL_VERSION};
use tomo_watch::{scan_diff, WatchSignal, Watcher};

use crate::apply::{apply_absent, apply_present, join, should_send};
use crate::buildinfo;
use crate::error::CliError;
use crate::layout::Layout;
use crate::persist::{load_index, store_index};
use crate::report::Reporter;
use crate::status::{write_status, Status};
use crate::transport::{self, SshParams, Transport};

/// How often, at most, the status file is refreshed while otherwise idle.
const STATUS_CADENCE: Duration = Duration::from_secs(2);

/// The unified event the main loop consumes.
#[derive(Debug)]
pub enum Incoming {
    /// A canonical local change (or rescan request) from the watcher.
    Watch(WatchSignal),
    /// A decoded protocol message from the peer.
    Message(Message),
    /// The transport reached end-of-stream (peer closed / child exited).
    PeerEof,
    /// A fatal transport/framing error; the session must tear down.
    ProtoError(String),
}

/// Which transport the loop should run with.
pub enum Mode {
    /// No peer: maintain the index and status file from local changes only.
    WatchOnly,
    /// Spawn `serve --stdio` rooted at this path and sync with it (M1 local
    /// transport).
    LocalPeer(PathBuf),
    /// Be the served peer: our own stdin/stdout is the wire.
    Serve,
    /// Sync with a remote peer over SSH: bootstrap the remote binary, spawn
    /// `serve --stdio` on it, and frame over the tunnel (M2).
    Ssh(SshParams),
}

/// Mutable session state owned by the main thread.
// The four flags are independent facets of one small state machine (peer
// liveness, handshake progress, and two write-coalescing dirty bits); bundling
// them into sub-structs would obscure rather than clarify.
#[allow(clippy::struct_excessive_bools)]
struct Session {
    layout: Layout,
    config: Config,
    engine: Engine,
    reporter: Reporter,
    binary_version: String,
    transport: Option<Transport>,
    /// When set, this session is over SSH and carries the params needed to
    /// re-bootstrap on a version-mismatch retry.
    ssh_params: Option<SshParams>,
    /// Set by the handshake when the peer's binary version differs and we are on
    /// SSH: signals the loop to tear down, re-push, and reconnect once.
    repush_requested: bool,
    connected: bool,
    hello_received: bool,
    conflicts: BTreeSet<RelPath>,
    index_dirty: bool,
    status_dirty: bool,
    last_status: Instant,
}

/// Run the sync loop to completion.
///
/// # Errors
/// Propagates a fatal error (handshake mismatch, apply failure, framing error,
/// or I/O on a state file). Normal peer disconnect returns `Ok(())`.
pub fn run(
    layout: Layout,
    config: Config,
    replica: ReplicaId,
    reporter: Reporter,
    mode: Mode,
) -> Result<(), CliError> {
    let index = load_index(&layout.index())?;
    let engine = Engine::new(replica, index);

    let (tx, rx) = mpsc::channel::<Incoming>();

    // Watcher → forwarder thread → unified channel.
    let (ws_tx, ws_rx) = mpsc::channel::<WatchSignal>();
    let _watcher: Watcher = Watcher::start(layout.root(), config.clone(), ws_tx)?;
    spawn_watch_forwarder(ws_rx, tx.clone());

    let mut session = Session {
        layout,
        config,
        engine,
        reporter,
        binary_version: buildinfo::binary_version(),
        transport: None,
        ssh_params: None,
        repush_requested: false,
        connected: false,
        hello_received: false,
        conflicts: BTreeSet::new(),
        index_dirty: false,
        status_dirty: true,
        last_status: Instant::now(),
    };

    // Catch up on anything that changed while we were down, before connecting.
    session.startup_scan()?;
    session.persist(true)?;

    // Bring up the transport (if any) and open the handshake.
    session.connect(mode, &tx)?;

    // Main loop.
    let mut outcome = session.pump(&rx);

    // SSH only: on a Hello version mismatch, re-push the binary and reconnect
    // exactly once (invariant: any version skew triggers a fresh push, SPEC §3).
    if session.repush_requested {
        outcome = session.retry_ssh_once(&tx, &rx);
    }

    // Flush final state regardless of how we exited.
    let flush = session.persist(true);
    if let Some(mut t) = session.transport.take() {
        t.join_reader();
    }
    outcome.and(flush)
}

/// Forward every [`WatchSignal`] onto the unified channel until either side
/// hangs up.
fn spawn_watch_forwarder(ws_rx: mpsc::Receiver<WatchSignal>, tx: Sender<Incoming>) {
    std::thread::spawn(move || {
        for sig in ws_rx {
            if tx.send(Incoming::Watch(sig)).is_err() {
                break;
            }
        }
    });
}

impl Session {
    /// Diff the on-disk tree against the (freshly loaded) index and feed the
    /// differences as local events. Send actions have nowhere to go yet and are
    /// dropped; the post-handshake index exchange reconciles instead.
    fn startup_scan(&mut self) -> Result<(), CliError> {
        let changes = scan_diff(self.layout.root(), self.engine.index(), &self.config)?;
        for change in changes {
            let actions = self.engine.handle(Event::Local(change));
            self.execute(actions, None)?;
        }
        Ok(())
    }

    /// Bring up the selected transport and send our opening [`Message::Hello`].
    fn connect(&mut self, mode: Mode, tx: &Sender<Incoming>) -> Result<(), CliError> {
        let transport = match mode {
            Mode::WatchOnly => {
                self.reporter
                    .note("watch-only (no peer) — maintaining index and status");
                None
            }
            Mode::LocalPeer(path) => {
                crate::init::ensure_initialized(&Layout::new(&path))?;
                self.reporter
                    .note(&format!("local peer at {}", path.display()));
                Some(transport::local_peer(&path, tx)?)
            }
            Mode::Serve => Some(transport::stdio(tx)),
            Mode::Ssh(params) => {
                self.reporter
                    .note(&format!("connecting to {} over SSH", params.target));
                let (t, report) = transport::ssh(&params, tx, false)?;
                self.report_bootstrap(&report);
                self.ssh_params = Some(params);
                Some(t)
            }
        };

        if let Some(t) = transport {
            self.transport = Some(t);
            self.send_opening_hello()?;
        }
        Ok(())
    }

    /// Send our opening [`Message::Hello`] over the current transport.
    fn send_opening_hello(&mut self) -> Result<(), CliError> {
        let hello = Message::Hello {
            protocol: PROTOCOL_VERSION,
            binary_version: self.binary_version.clone(),
            replica: self.engine.replica(),
        };
        self.send(&hello)
    }

    /// Note what the bootstrap did (pushed vs reused), warning loudly on the
    /// debug-only dev substitution.
    fn report_bootstrap(&self, report: &tomo_transport::BootstrapReport) {
        match report {
            tomo_transport::BootstrapReport::Reused {
                triple, version, ..
            } => {
                self.reporter.note(&format!(
                    "remote binary up to date (tomo {version}, {triple})"
                ));
            }
            tomo_transport::BootstrapReport::Pushed {
                triple,
                version,
                bytes,
                dev_substitution,
                ..
            } => {
                self.reporter.note(&format!(
                    "pushed remote binary tomo {version} ({triple}, {bytes} bytes)"
                ));
                if *dev_substitution {
                    self.reporter.note(
                        "WARNING: dev-mode binary substitution — pushed this build's own \
                         non-musl binary to satisfy a musl remote. This is a debug-only \
                         convenience for localhost testing; release builds embed real \
                         static musl binaries (M6).",
                    );
                }
            }
        }
    }

    /// SSH re-push retry: retire the current transport silently, re-run the
    /// bootstrap with `force_push`, reconnect, and pump again. A second mismatch
    /// is a hard error.
    fn retry_ssh_once(
        &mut self,
        tx: &Sender<Incoming>,
        rx: &mpsc::Receiver<Incoming>,
    ) -> Result<(), CliError> {
        let Some(params) = self.ssh_params.clone() else {
            return Err(CliError::msg(
                "internal: version-mismatch retry requested without SSH params",
            ));
        };
        self.reporter
            .note("binary version mismatch — re-pushing the remote binary and reconnecting once");

        // Retire the superseded transport: mark it dead (so its reader thread
        // stays quiet) and drop it (tearing down the old SSH session).
        if let Some(t) = self.transport.take() {
            t.deactivate();
        }
        self.repush_requested = false;
        self.connected = false;
        self.hello_received = false;
        self.status_dirty = true;

        let (t, report) = transport::ssh(&params, tx, true)?;
        self.report_bootstrap(&report);
        self.transport = Some(t);
        self.send_opening_hello()?;

        let outcome = self.pump(rx);
        if self.repush_requested {
            return Err(CliError::msg(
                "remote binary version still mismatches after a re-push — the remote and \
                 local tomo builds disagree on their version; aborting",
            ));
        }
        outcome
    }

    /// The blocking main loop. Returns on peer EOF (`Ok`) or a fatal error.
    fn pump(&mut self, rx: &mpsc::Receiver<Incoming>) -> Result<(), CliError> {
        loop {
            match rx.recv_timeout(STATUS_CADENCE) {
                Ok(Incoming::Watch(signal)) => self.on_watch(signal)?,
                Ok(Incoming::Message(msg)) => self.on_message(msg)?,
                Ok(Incoming::PeerEof) => {
                    self.reporter.note("peer disconnected");
                    return Ok(());
                }
                Ok(Incoming::ProtoError(e)) => {
                    return Err(CliError::msg(format!("transport error: {e}")));
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
            // A handshake version mismatch over SSH asks us to stop this pass so
            // the caller can re-push and reconnect (handled in `run`).
            if self.repush_requested {
                return Ok(());
            }
            // Persist after each wake (cheap at M1 scale); refresh status on the
            // idle cadence so counters stay current even when nothing changes.
            self.persist(false)?;
        }
    }

    // ---- Event handlers ---------------------------------------------------

    fn on_watch(&mut self, signal: WatchSignal) -> Result<(), CliError> {
        match signal {
            WatchSignal::Change(change) => {
                let actions = self.engine.handle(Event::Local(change));
                if !actions.is_empty() {
                    self.mark_dirty();
                }
                self.execute(actions, None)
            }
            WatchSignal::NeedsRescan => {
                let changes = scan_diff(self.layout.root(), self.engine.index(), &self.config)?;
                for change in changes {
                    let actions = self.engine.handle(Event::Local(change));
                    if !actions.is_empty() {
                        self.mark_dirty();
                    }
                    self.execute(actions, None)?;
                }
                Ok(())
            }
        }
    }

    fn on_message(&mut self, msg: Message) -> Result<(), CliError> {
        match msg {
            Message::Hello {
                protocol,
                binary_version,
                replica,
            } => self.on_hello(protocol, &binary_version, replica),
            Message::IndexExchange(peer_index) => self.reconcile(&peer_index),
            Message::Change { change, bytes } => {
                let actions = self.engine.handle(Event::Remote(change));
                self.mark_dirty();
                self.execute(actions, bytes.as_deref())
            }
            Message::Ping { nonce } => self.send(&Message::Pong { nonce }),
            // Liveness replies carry no state at M1.
            Message::Pong { .. } => Ok(()),
        }
    }

    fn on_hello(
        &mut self,
        protocol: u16,
        binary_version: &str,
        _replica: ReplicaId,
    ) -> Result<(), CliError> {
        if protocol != PROTOCOL_VERSION {
            return Err(CliError::msg(format!(
                "protocol mismatch: peer speaks v{protocol}, we speak v{PROTOCOL_VERSION}"
            )));
        }
        if binary_version != self.binary_version {
            // Over SSH we can fix a version skew by re-pushing the binary and
            // reconnecting once (SPEC §3); the loop handles the retry. On the
            // local/stdio transports there is nothing to re-push, so it is fatal.
            if self.ssh_params.is_some() {
                self.reporter.note(&format!(
                    "peer binary version {binary_version} != ours {}; will re-push",
                    self.binary_version
                ));
                self.repush_requested = true;
                return Ok(());
            }
            return Err(CliError::msg(format!(
                "binary version mismatch: peer is {binary_version}, we are {}",
                self.binary_version
            )));
        }
        self.hello_received = true;
        self.connected = true;
        self.status_dirty = true;
        self.reporter.note("peer connected");
        // Ship our full index for reconciliation now that the peer is known good.
        let index_snapshot: Index = self.engine.index().clone();
        self.send(&Message::IndexExchange(index_snapshot))
    }

    /// Reconcile against the peer's just-received index by shipping every local
    /// head the peer does not already cover, as a content-bearing
    /// [`Message::Change`].
    ///
    /// We deliberately do **not** feed the peer index as [`Event::PeerIndex`]:
    /// that would absorb peer-only present heads into our index *without their
    /// bytes*, after which the peer's content-bearing `Change` for the same head
    /// is dismissed as already-known and the file is never written. Driving
    /// reconciliation through `Change` frames keeps content and index knowledge
    /// together, and the peer converges symmetrically. Skipping heads the peer
    /// already covers keeps a reconnect over an unchanged tree quiet (the
    /// quiet-network invariant).
    fn reconcile(&mut self, peer: &Index) -> Result<(), CliError> {
        let mut to_send = Vec::new();
        for (path, entry) in self.engine.index().iter() {
            for head in entry.heads() {
                let covered = peer.get(path).is_some_and(|peer_entry| {
                    peer_entry.heads().iter().any(|peer_head| {
                        matches!(
                            head.version.compare(&peer_head.version),
                            Causality::Before | Causality::Equal
                        )
                    })
                });
                if !covered {
                    to_send.push(RemoteChange {
                        path: path.clone(),
                        kind: kind_from_state(head.state),
                        version: head.version.clone(),
                    });
                }
            }
        }
        for change in to_send {
            self.do_send(change)?;
        }
        Ok(())
    }

    // ---- Action execution -------------------------------------------------

    /// Execute engine actions. `remote_bytes` is the content of the triggering
    /// [`Message::Change`], if any — the only source of bytes for an
    /// [`Action::Apply`] that materializes present content.
    fn execute(
        &mut self,
        actions: Vec<Action>,
        remote_bytes: Option<&[u8]>,
    ) -> Result<(), CliError> {
        for action in actions {
            match action {
                Action::Send(change) => self.do_send(change)?,
                Action::Apply { path, target } => self.do_apply(&path, target, remote_bytes)?,
                // History adapters are M3; conflicts are surfaced non-blockingly.
                Action::RecordVersion { .. } => {}
                Action::ConflictResolved { path, .. } => {
                    if self.conflicts.insert(path.clone()) {
                        self.status_dirty = true;
                    }
                    self.reporter.conflict(path.as_str());
                }
            }
        }
        Ok(())
    }

    /// Ship a local change to the peer, re-reading the file so we send the
    /// latest bytes (or drop the send if they went stale — invariant #3).
    fn do_send(&mut self, change: RemoteChange) -> Result<(), CliError> {
        if self.transport.is_none() {
            return Ok(()); // watch-only / pre-handshake: nothing to ship.
        }
        let message = match change.kind {
            ChangeKind::Modified(sig) => {
                let full = join(self.layout.root(), &change.path);
                let current = std::fs::read(&full).ok();
                if !should_send(current.as_deref(), &sig) {
                    // The file changed again (or vanished); the watcher's
                    // follow-up event ships the newer state. Drop this one.
                    return Ok(());
                }
                Message::Change {
                    change,
                    bytes: current,
                }
            }
            ChangeKind::Removed => Message::Change {
                change,
                bytes: None,
            },
        };
        self.send(&message)
    }

    /// Bring the tree at `path` into line with `target`.
    fn do_apply(
        &mut self,
        path: &RelPath,
        target: Expectation,
        remote_bytes: Option<&[u8]>,
    ) -> Result<(), CliError> {
        match target {
            Expectation::Present(sig) => match remote_bytes {
                Some(bytes) => {
                    apply_present(
                        self.layout.root(),
                        &self.layout.staging(),
                        path,
                        &sig,
                        bytes,
                    )?;
                    self.reporter.synced(path.as_str());
                }
                None => {
                    // Present content with no accompanying bytes only arises from
                    // reconciling a pre-existing divergent tree at connect; the
                    // content pull that completes it lands with the SSH transport
                    // (M2). Live sync always carries bytes in the Change frame.
                    self.reporter.error(&format!(
                        "cannot materialize {path} yet (initial reconciliation completes at M2)"
                    ));
                }
            },
            Expectation::Absent => {
                apply_absent(self.layout.root(), path)?;
                self.reporter.removed(path.as_str());
            }
        }
        Ok(())
    }

    // ---- Persistence ------------------------------------------------------

    fn mark_dirty(&mut self) {
        self.index_dirty = true;
        self.status_dirty = true;
    }

    /// Persist the index (if changed) and the status file (if changed or the
    /// idle cadence elapsed, or `force`).
    fn persist(&mut self, force: bool) -> Result<(), CliError> {
        if self.index_dirty {
            store_index(
                &self.layout.staging(),
                &self.layout.index(),
                self.engine.index(),
            )?;
            self.index_dirty = false;
            self.status_dirty = true;
        }
        let due = force || self.status_dirty || self.last_status.elapsed() >= STATUS_CADENCE;
        if due {
            let net = self
                .transport
                .as_ref()
                .map(|t| t.counters.snapshot())
                .unwrap_or_default();
            let conflicts = self.conflicts.len() as u64;
            let status = Status::live(self.engine.index(), conflicts, net, self.connected);
            write_status(&self.layout, &status)?;
            self.last_status = Instant::now();
            self.status_dirty = false;
        }
        Ok(())
    }

    fn send(&mut self, msg: &Message) -> Result<(), CliError> {
        match self.transport.as_mut() {
            Some(t) => t.tx.send(msg),
            None => Ok(()),
        }
    }
}

/// The change kind that reproduces a head's state on the wire.
fn kind_from_state(state: EntryState) -> ChangeKind {
    match state {
        EntryState::Present(sig) => ChangeKind::Modified(sig),
        EntryState::Tombstone => ChangeKind::Removed,
    }
}
