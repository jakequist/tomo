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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tomo_config::Config;
use tomo_engine::{
    Action, CaptureDecision, CaptureInput, Causality, ChangeKind, ContentSig, Engine, EntryState,
    Event, Expectation, Index, PressureConfig, PressureController, RelPath, RemoteChange,
    ReplicaId, VectorClock,
};
use tomo_history::{HistoryStore, Origin, VersionId};
use tomo_proto::{Message, PROTOCOL_VERSION};
use tomo_watch::{scan_diff, WatchSignal, Watcher};

use crate::apply::{apply_absent, apply_present, join, matches_sig, should_send};
use crate::buildinfo;
use crate::error::CliError;
use crate::layout::Layout;
use crate::persist::{load_index, store_index};
use crate::report::Reporter;
use crate::status::{now_unix_ms, write_status, History as HistoryStatus, Status};
use crate::transport::{self, SshParams, Transport};

/// How often, at most, the status file is refreshed while otherwise idle.
const STATUS_CADENCE: Duration = Duration::from_secs(2);

/// Minimum gap between dirty-driven persistence writes (index + status).
///
/// Without this, an event storm fsyncs the full index file once per event
/// (~200k atomic writes for a 5s tight-loop storm), which is what actually
/// throttled convergence — not the sync path. The index is a reconstructible
/// cache (startup `scan_diff` reconciles after a crash), so a ≤2s stale
/// window on disk costs nothing in correctness (invariant #8 still holds:
/// every write is still staging + atomic rename).
const PERSIST_THROTTLE: Duration = Duration::from_millis(250);

/// How long the session must be free of processed changes before a deferred
/// rescan may run (see `WatchSignal::NeedsRescan` handling).
const RESCAN_QUIESCENT: Duration = Duration::from_millis(500);

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
    /// The content-addressed history store; every recorded version and conflict
    /// lands here. Opened at startup (a failure to open is fatal).
    history: HistoryStore,
    /// The pure adaptive-capture controller. Immediate captures are recorded
    /// inline; deferred ones are staged here and flushed by the main loop when
    /// their deadline elapses (invariants #3, #4).
    pressure: PressureController,
    /// The peer's replica id, learned at the handshake — the authoring replica
    /// attributed to peer-origin versions in history. `None` before handshake.
    peer_replica: Option<ReplicaId>,
    /// Monotonic time origin for the pressure controller's `now_ms` (never a
    /// wall clock — that would violate invariant #7; this is debounce timing).
    started: Instant,
    /// Count of versions this session has recorded (for the status block).
    versions_recorded: u64,
    /// Count of conflict records this session has written.
    conflicts_recorded: u64,
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
    last_index_persist: Instant,
    rescan_pending: bool,
    last_activity: Instant,
    shutdown: Arc<AtomicBool>,
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

    // Open the history store up front: a failure here is fatal — we will not run
    // a sync loop that cannot capture history (the milestone's whole point).
    let history = HistoryStore::open(layout.root()).map_err(|source| {
        CliError::msg(format!(
            "cannot open history store under {}: {source}",
            layout.tomo().display()
        ))
    })?;
    let pressure = PressureController::new(
        crate::histmode::to_engine(&config.history.mode),
        PressureConfig::default(),
    );

    let (tx, rx) = mpsc::channel::<Incoming>();

    // Clean shutdown on SIGTERM/SIGINT: the pump loop polls this flag so a
    // terminated watch still flushes index/status/history and reaps its serve
    // child (previously SIGTERM's default action orphaned the child and left
    // a stale "connected" status behind).
    let shutdown = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        let _ = signal_hook::flag::register(sig, Arc::clone(&shutdown));
    }

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
        history,
        pressure,
        peer_replica: None,
        started: Instant::now(),
        versions_recorded: 0,
        conflicts_recorded: 0,
        ssh_params: None,
        repush_requested: false,
        connected: false,
        hello_received: false,
        conflicts: BTreeSet::new(),
        index_dirty: false,
        status_dirty: true,
        last_status: Instant::now(),
        last_index_persist: Instant::now(),
        rescan_pending: false,
        last_activity: Instant::now(),
        shutdown,
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

    // Flush every staged history capture before we go, so a burst whose final
    // version was still debouncing at shutdown is never lost (invariant #4).
    let drained = session.drain_history();
    // Flush final state regardless of how we exited — and record that we are
    // no longer connected, so `tomo status` never shows a live session for a
    // process that has exited (the stale `connected: true` dogfood bug).
    session.connected = false;
    session.status_dirty = true;
    let flush = session.persist(true);
    if let Some(mut t) = session.transport.take() {
        t.join_reader();
    }
    outcome.and(drained).and(flush)
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
            self.execute(actions, None, None)?;
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
            // Wake at the sooner of the status cadence and the next staged
            // history deadline, so deferred captures flush promptly (invariant
            // #4) without busy-waiting.
            let timeout = self.recv_deadline();
            match rx.recv_timeout(timeout) {
                Ok(Incoming::Watch(signal)) => {
                    self.last_activity = Instant::now();
                    self.on_watch(signal)?;
                }
                Ok(Incoming::Message(msg)) => {
                    self.last_activity = Instant::now();
                    self.on_message(msg)?;
                }
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
            // SIGTERM/SIGINT: leave the loop; run() flushes history/index/
            // status and drops the transport (reaping the serve child).
            if self.shutdown.load(Ordering::Relaxed) {
                self.reporter.note("shutting down (signal)");
                return Ok(());
            }
            // Flush any history captures now due and feed the back-pressure
            // signal, then persist. Status refreshes on the idle cadence so
            // counters stay current even when nothing changes.
            self.maybe_rescan()?;
            self.pump_history()?;
            self.persist(false)?;
        }
    }

    /// The recv timeout: the sooner of the status cadence, the time until the
    /// next staged history capture is due, and (when a rescan is pending) the
    /// quiescence window, so a deferred rescan runs promptly once things calm.
    fn recv_deadline(&self) -> Duration {
        let base = match self.pressure.next_due_ms() {
            Some(due) => {
                let now = self.now_ms();
                Duration::from_millis(due.saturating_sub(now)).min(STATUS_CADENCE)
            }
            None => STATUS_CADENCE,
        };
        if self.rescan_pending {
            base.min(RESCAN_QUIESCENT)
        } else {
            base
        }
    }

    /// Feed the controller its queue-depth signal (staged length is the honest,
    /// cheap proxy) and record every capture whose deadline has elapsed.
    fn pump_history(&mut self) -> Result<(), CliError> {
        let now = self.now_ms();
        let depth = self.pressure.staged_len() as u64;
        self.pressure.signals(depth, now);
        for (path, cap) in self.pressure.poll_due(now) {
            self.record_version(&path, cap.state, &cap.version, cap.origin_is_local)?;
        }
        Ok(())
    }

    /// Drain every staged capture at shutdown, polling at each successive
    /// deadline (never a synthetic "far future" now, which would skew the
    /// controller's decay) until nothing remains staged (invariant #4).
    fn drain_history(&mut self) -> Result<(), CliError> {
        let mut now = self.now_ms();
        while self.pressure.staged_len() > 0 {
            let Some(due) = self.pressure.next_due_ms() else {
                break;
            };
            now = now.max(due);
            for (path, cap) in self.pressure.poll_due(now) {
                self.record_version(&path, cap.state, &cap.version, cap.origin_is_local)?;
            }
        }
        Ok(())
    }

    // ---- Event handlers ---------------------------------------------------

    fn on_watch(&mut self, signal: WatchSignal) -> Result<(), CliError> {
        match signal {
            // Resolve (stat + hash) HERE, on the session thread, not in the
            // watcher: this thread also executes applies, so by construction
            // the sig reflects every write we have performed so far and an
            // echo can never present a stale hash to the journal (the phantom
            // -conflict storm bug).
            WatchSignal::Pending(pending) => {
                let Ok(change) = tomo_watch::resolve(self.layout.root(), &pending) else {
                    // Transient read failure: reconcile via rescan rather
                    // than dropping the change silently.
                    self.rescan_pending = true;
                    return Ok(());
                };
                let actions = self.engine.handle(Event::Local(change));
                if !actions.is_empty() {
                    self.mark_dirty();
                }
                self.execute(actions, None, None)
            }
            // NEVER rescan inline: during an apply storm the disk lags the
            // index (applies queued but not yet executed), so a scan taken now
            // reads stale bytes and fabricates "local edits" of old content —
            // spurious conflicts and reverse traffic (found via the unthrottled
            // storm repro). Defer until the session is quiescent.
            WatchSignal::NeedsRescan => {
                self.rescan_pending = true;
                Ok(())
            }
        }
    }

    /// Run a deferred reconciling rescan once no change has been processed for
    /// [`RESCAN_QUIESCENT`]. See the `NeedsRescan` arm for why deferral is a
    /// correctness requirement, not an optimization.
    fn maybe_rescan(&mut self) -> Result<(), CliError> {
        if !self.rescan_pending || self.last_activity.elapsed() < RESCAN_QUIESCENT {
            return Ok(());
        }
        self.rescan_pending = false;
        let changes = scan_diff(self.layout.root(), self.engine.index(), &self.config)?;
        for change in changes {
            let actions = self.engine.handle(Event::Local(change));
            if !actions.is_empty() {
                self.mark_dirty();
            }
            self.execute(actions, None, None)?;
        }
        Ok(())
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
                // Keep the incoming version so history attributes these captures
                // (and any conflict heads) to the peer, not to us.
                let remote_version = change.version.clone();
                let actions = self.engine.handle(Event::Remote(change));
                self.mark_dirty();
                self.execute(actions, bytes.as_deref(), Some(&remote_version))
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
        replica: ReplicaId,
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
        self.peer_replica = Some(replica);
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

    /// Execute an engine action batch in two passes.
    ///
    /// `remote_bytes` is the content of the triggering [`Message::Change`], if
    /// any — the source of bytes for an [`Action::Apply`] and for conflict heads
    /// that arrived in-frame. `remote_version` is that change's clock, used to
    /// attribute captures (and conflict heads) to the peer.
    ///
    /// **Pass 1** captures the bytes every [`Action::ConflictResolved`] needs
    /// *before* any [`Action::Apply`] can overwrite a loser on disk (invariant
    /// #5). **Pass 2** runs every action in order: applies, sends, and history
    /// captures (versions via the pressure controller, conflicts immediately).
    fn execute(
        &mut self,
        actions: Vec<Action>,
        remote_bytes: Option<&[u8]>,
        remote_version: Option<&VectorClock>,
    ) -> Result<(), CliError> {
        let now_ms = self.now_ms();

        // Pass 1: capture conflict bytes while the tree still holds the losers.
        let mut captures: Vec<ConflictCapture> = Vec::new();
        for action in &actions {
            if let Action::ConflictResolved {
                path,
                winner,
                winner_version,
                loser,
                loser_version,
            } = action
            {
                captures.push(ConflictCapture {
                    path: path.clone(),
                    winner: *winner,
                    winner_version: winner_version.clone(),
                    winner_is_local: remote_version != Some(winner_version),
                    winner_bytes: self.capture_conflict_bytes(path, *winner, remote_bytes),
                    loser: *loser,
                    loser_version: loser_version.clone(),
                    loser_is_local: remote_version != Some(loser_version),
                    loser_bytes: self.capture_conflict_bytes(path, *loser, remote_bytes),
                });
            }
        }
        // A version a same-batch conflict already preserves must not also be
        // routed through the controller — that would record it twice.
        let conflict_versions: Vec<(RelPath, VectorClock)> = captures
            .iter()
            .flat_map(|c| {
                [
                    (c.path.clone(), c.winner_version.clone()),
                    (c.path.clone(), c.loser_version.clone()),
                ]
            })
            .collect();

        // Pass 2: everything, in emitted order.
        let mut next_capture = 0usize;
        for action in actions {
            match action {
                Action::Send(change) => self.do_send(change)?,
                Action::Apply { path, target } => self.do_apply(&path, target, remote_bytes)?,
                Action::RecordVersion {
                    path,
                    state,
                    version,
                } => {
                    if conflict_versions
                        .iter()
                        .any(|(p, v)| *p == path && *v == version)
                    {
                        continue;
                    }
                    let origin_is_local = remote_version != Some(&version);
                    self.note_version(&path, state, &version, origin_is_local, now_ms)?;
                }
                Action::ConflictResolved { path, .. } => {
                    let capture = &captures[next_capture];
                    next_capture += 1;
                    self.record_conflict_capture(capture)?;
                    if self.conflicts.insert(path.clone()) {
                        self.status_dirty = true;
                    }
                    self.reporter.conflict(path.as_str());
                }
            }
        }
        Ok(())
    }

    // ---- History capture --------------------------------------------------

    /// Monotonic milliseconds since the session started — the pressure
    /// controller's `now_ms`. This is debounce timing only, never an ordering
    /// input (invariant #7), and [`Instant`] guarantees it never goes backward.
    fn now_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// The `(origin, authoring replica)` for a version of the given locality.
    /// Local versions are ours; remote versions are attributed to the peer (we
    /// only fall back to our own replica before the handshake, which is
    /// unreachable for a peer-origin version).
    fn attribution(&self, origin_is_local: bool) -> (Origin, ReplicaId) {
        if origin_is_local {
            (Origin::Local, self.engine.replica())
        } else {
            (
                Origin::Remote,
                self.peer_replica.unwrap_or_else(|| self.engine.replica()),
            )
        }
    }

    /// Read `path` from disk and return its bytes iff they match `sig`.
    fn read_verified(&self, path: &RelPath, sig: &ContentSig) -> Option<Vec<u8>> {
        let full = join(self.layout.root(), path);
        match std::fs::read(&full) {
            Ok(bytes) if matches_sig(&bytes, sig) => Some(bytes),
            _ => None,
        }
    }

    /// Route a `RecordVersion` through the pressure controller: record it now if
    /// the controller says immediate, otherwise leave it staged for the main
    /// loop to flush at its deadline. `Dropped` (history off) records nothing.
    fn note_version(
        &mut self,
        path: &RelPath,
        state: EntryState,
        version: &VectorClock,
        origin_is_local: bool,
        now_ms: u64,
    ) -> Result<(), CliError> {
        let size_hint = match state {
            EntryState::Present(sig) => sig.size,
            EntryState::Tombstone => 0,
        };
        let input = CaptureInput {
            state,
            version: version.clone(),
            origin_is_local,
            size_hint,
        };
        match self.pressure.note(path.clone(), input, now_ms) {
            CaptureDecision::Immediate => {
                self.record_version(path, state, version, origin_is_local)
            }
            CaptureDecision::Deferred { .. } | CaptureDecision::Dropped => Ok(()),
        }
    }

    /// Record one version, reading present content from disk **at record time**
    /// and verifying it against `state`'s signature.
    ///
    /// A mismatch or missing file means this capture was superseded by a newer
    /// one (already staged or in flight): skip it, logged but non-fatally — the
    /// newest capture for the path is always still staged or already recorded,
    /// so invariant #4 holds. Tombstones store no bytes.
    fn record_version(
        &mut self,
        path: &RelPath,
        state: EntryState,
        version: &VectorClock,
        origin_is_local: bool,
    ) -> Result<(), CliError> {
        let (origin, replica) = self.attribution(origin_is_local);
        let bytes = match state {
            EntryState::Present(sig) => {
                if let Some(bytes) = self.read_verified(path, &sig) {
                    Some(bytes)
                } else {
                    self.reporter.note(&format!(
                        "history: skipped superseded capture of {path} (bytes changed before \
                         record — a newer version is staged or in flight)"
                    ));
                    return Ok(());
                }
            }
            EntryState::Tombstone => None,
        };
        self.history.record_version(
            path,
            &state,
            version,
            replica,
            origin,
            now_unix_ms(),
            bytes.as_deref(),
        )?;
        self.versions_recorded += 1;
        self.status_dirty = true;
        Ok(())
    }

    /// Capture the bytes for one conflict head, preferring the triggering
    /// frame's in-hand bytes and falling back to the current (verified) on-disk
    /// content. Called in pass 1, before any Apply overwrites the tree.
    fn capture_conflict_bytes(
        &self,
        path: &RelPath,
        state: EntryState,
        remote_bytes: Option<&[u8]>,
    ) -> Captured {
        let sig = match state {
            EntryState::Tombstone => return Captured::Tombstone,
            EntryState::Present(sig) => sig,
        };
        if let Some(bytes) = remote_bytes {
            if matches_sig(bytes, &sig) {
                return Captured::Present(bytes.to_vec());
            }
        }
        match self.read_verified(path, &sig) {
            Some(bytes) => Captured::Present(bytes),
            None => Captured::Unobtainable,
        }
    }

    /// Record both heads of a resolved conflict and the conflict row, bypassing
    /// the pressure controller entirely (a coalescing slot could swallow the
    /// loser — invariant #5 requires losers always preserved). The loser is
    /// recorded first. If a head's bytes are genuinely unobtainable we record
    /// what we can and warn loudly, never crashing or blocking sync.
    fn record_conflict_capture(&mut self, capture: &ConflictCapture) -> Result<(), CliError> {
        let path = &capture.path;
        let loser_id = self.find_or_record(
            path,
            capture.loser,
            &capture.loser_version,
            &capture.loser_bytes,
            capture.loser_is_local,
        )?;
        let winner_id = self.find_or_record(
            path,
            capture.winner,
            &capture.winner_version,
            &capture.winner_bytes,
            capture.winner_is_local,
        )?;
        match (winner_id, loser_id) {
            (Some(winner), Some(loser)) => {
                self.history
                    .record_conflict(path, winner, loser, now_unix_ms())?;
                self.conflicts_recorded += 1;
                self.status_dirty = true;
            }
            _ => {
                self.reporter.error(&format!(
                    "history: could not fully preserve the conflict on {path} (a version's \
                     bytes were unavailable); sync is unaffected"
                ));
            }
        }
        Ok(())
    }

    /// Return the id of an already-stored version of `path` matching
    /// `(version, state)`, or record it fresh with the captured bytes. Returns
    /// `None` only when a present version must be recorded but its bytes are
    /// unobtainable.
    fn find_or_record(
        &mut self,
        path: &RelPath,
        state: EntryState,
        version: &VectorClock,
        captured: &Captured,
        origin_is_local: bool,
    ) -> Result<Option<VersionId>, CliError> {
        for meta in self.history.log(path)? {
            if &meta.clock == version && same_state(meta.state, state) {
                return Ok(Some(meta.id));
            }
        }
        let (origin, replica) = self.attribution(origin_is_local);
        let bytes = match (state, captured) {
            (EntryState::Tombstone, _) => None,
            (EntryState::Present(_), Captured::Present(bytes)) => Some(bytes.as_slice()),
            // Present but unobtainable: cannot record content-addressed bytes.
            (EntryState::Present(_), _) => return Ok(None),
        };
        let id = self.history.record_version(
            path,
            &state,
            version,
            replica,
            origin,
            now_unix_ms(),
            bytes,
        )?;
        self.versions_recorded += 1;
        self.status_dirty = true;
        Ok(Some(id))
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
                Some(bytes) if !matches_sig(bytes, &sig) => {
                    // A frame whose bytes do not hash to the expected sig must
                    // never be written — but it must not kill the session
                    // either (a raced frame under churn once did exactly that,
                    // orphaning the peer). Refuse the bytes, warn loudly, and
                    // schedule a reconciling rescan so disk truth re-anchors
                    // the index (the storm's follow-up frames converge us).
                    self.reporter.error(&format!(
                        "refused {path}: frame bytes do not match the declared \
                         content hash (raced or corrupt frame); scheduling rescan"
                    ));
                    self.rescan_pending = true;
                }
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
        if self.index_dirty && (force || self.last_index_persist.elapsed() >= PERSIST_THROTTLE) {
            store_index(
                &self.layout.staging(),
                &self.layout.index(),
                self.engine.index(),
            )?;
            self.index_dirty = false;
            self.status_dirty = true;
            self.last_index_persist = Instant::now();
        }
        let due = force
            || (self.status_dirty && self.last_status.elapsed() >= PERSIST_THROTTLE)
            || self.last_status.elapsed() >= STATUS_CADENCE;
        if due {
            let net = self
                .transport
                .as_ref()
                .map(|t| t.counters.snapshot())
                .unwrap_or_default();
            let conflicts = self.conflicts.len() as u64;
            // Authoritative unresolved count from the history DB (a `resolve`
            // from another process may have acknowledged rows this session
            // surfaced), so the status badge stays accurate.
            let conflicts_unresolved = self.history.conflicts(true)?.len() as u64;
            let history = HistoryStatus {
                mode: crate::histmode::label(&self.config.history.mode).to_owned(),
                versions_recorded: self.versions_recorded,
                conflicts_recorded: self.conflicts_recorded,
                staged: self.pressure.staged_len() as u64,
                rung: self.pressure.rung() as u64,
            };
            let status = Status::live(
                self.engine.index(),
                conflicts,
                conflicts_unresolved,
                net,
                self.connected,
                self.rescan_pending,
                Some(history),
            );
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

/// Bytes captured for one head of a conflict during pass 1, before any Apply in
/// the same batch can overwrite a loser on disk.
enum Captured {
    /// Present content, ready to store.
    Present(Vec<u8>),
    /// The head is a tombstone; there is nothing to store.
    Tombstone,
    /// A present head whose bytes could not be obtained (neither in-frame nor a
    /// verified on-disk read). Recorded as "unobtainable" so the loser warning
    /// fires rather than storing wrong bytes.
    Unobtainable,
}

/// Everything one [`Action::ConflictResolved`] needs to record both heads and
/// the conflict row, captured in pass 1 so the loser's bytes survive a
/// same-batch Apply (invariant #5).
struct ConflictCapture {
    path: RelPath,
    winner: EntryState,
    winner_version: VectorClock,
    winner_is_local: bool,
    winner_bytes: Captured,
    loser: EntryState,
    loser_version: VectorClock,
    loser_is_local: bool,
    loser_bytes: Captured,
}

/// Whether two states carry identical content (present with the same signature,
/// or both tombstones) — used to match a stored version against a conflict head.
fn same_state(a: EntryState, b: EntryState) -> bool {
    match (a, b) {
        (EntryState::Present(x), EntryState::Present(y)) => x.hash == y.hash && x.size == y.size,
        (EntryState::Tombstone, EntryState::Tombstone) => true,
        _ => false,
    }
}
