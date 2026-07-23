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

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tomo_config::Config;
use tomo_engine::{
    Action, CaptureDecision, CaptureInput, Causality, ChangeKind, ContentSig, Engine, EntryState,
    Event, Expectation, Index, LocalChange, PressureConfig, PressureController, RelPath,
    RemoteChange, ReplicaId, VectorClock,
};
use tomo_history::{ConflictId, HistoryStore, Origin, VersionId};
use tomo_proto::{ChunkHash, Message, INLINE_THRESHOLD, PROTOCOL_VERSION};
use tomo_watch::{scan_diff_cached, ScanCache, WatchSignal, Watcher};

use crate::apply::{
    apply_absent, apply_present, join, matches_sig, path_is_dir, set_exec_mode, should_send,
    type_collision, TypeCollision,
};
use crate::applyguard;
use crate::buildinfo;
use crate::chunkxfer::{self, ByteSource};
use crate::error::CliError;
use crate::layout::Layout;
use crate::persist::{load_index, load_scan_cache, store_index, store_scan_cache};
use crate::report::Reporter;
use crate::status::{now_unix_ms, write_status, History as HistoryStatus, Status};
use crate::transport::{self, InFlight, SshParams, Transport};

/// How often, at most, the status file is refreshed while otherwise idle.
const STATUS_CADENCE: Duration = Duration::from_secs(2);

/// How often a `heartbeat` event is published while a control-channel subscriber
/// is attached (the TUI status line lives off it). Only fires when someone is
/// watching — an idle session with no subscriber stays fully idle.
const HEARTBEAT_CADENCE: Duration = Duration::from_secs(1);

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

/// One-shot hold before shipping an EMPTY observation of a previously
/// non-empty file, so a truncate-then-write save's zero-byte window is
/// re-checked rather than mirrored to the peer (SPEC §5.1). Genuine
/// truncations to empty survive the re-check and ship ~30ms later.
const EMPTY_HOLD: Duration = Duration::from_millis(30);

/// First reconnect back-off after a peer disconnect (watch modes only).
const RECONNECT_MIN: Duration = Duration::from_secs(2);

/// Back-off ceiling: reconnect attempts never wait longer than this.
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// Default bytes-in-flight window for the outbound bulk stream (SEED-PERF
/// Phase 1). Per pump iteration we drain queued bulk frames (chunk data and
/// reconcile-queued `Change`/`ChangeManifest` frames) up to this many wire
/// bytes, then return to the recv path so the loop's periodic work and — over a
/// pipe transport — accumulated backpressure get a turn. It replaces the old
/// "≤4 chunk frames per 2 ms tick" interleave cap (which throttled a bulk
/// transfer to a per-tick trickle) with an honest byte budget large enough to
/// keep the wire full without unbounded memory. Overridable via
/// `TOMO_SEND_WINDOW_BYTES` for tests. See [`Session::pump_outbound`].
const DEFAULT_SEND_WINDOW_BYTES: usize = 16 * 1024 * 1024;

/// How many bulk frames are coalesced into one batched write before the pump
/// checks the priority lane for a live change (SEED-PERF Phase 1, invariant #3).
///
/// Small enough that a live edit made mid-seed is serviced within one sub-batch
/// — on a blocking pipe transport, at most this many frames' worth of receiver
/// apply time — and shipped ahead of the remaining bulk backlog; large enough
/// that the coalesced write amortizes the syscall/packet/wakeup per file.
const SEND_BATCH_FRAMES: usize = 24;

/// Read the send-window byte budget, honoring `TOMO_SEND_WINDOW_BYTES` (a test
/// hook for exercising stall/resume and small-window backpressure). A missing,
/// empty, zero, or unparseable value falls back to [`DEFAULT_SEND_WINDOW_BYTES`].
fn send_window_bytes() -> usize {
    std::env::var("TOMO_SEND_WINDOW_BYTES")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_SEND_WINDOW_BYTES)
}

/// When an apply stalls because the local filesystem is full, how long to wait
/// before re-requesting the missing content from the peer (by re-sending our
/// index so its reconcile reships whatever we still lack). Keeps a full-disk
/// session alive and self-healing once space is freed (invariants #5/#8).
const STALL_RETRY: Duration = Duration::from_secs(3);

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
    /// A clean-shutdown request from the control channel (`stop` command). Takes
    /// the same exit path as SIGTERM: the pump returns and `run` flushes state.
    Shutdown,
    /// A pause request from the control channel (`pause` command). The pump
    /// reconciles its effective pause state against the shared flag, emitting the
    /// event, telling the peer, and suspending transfers on the transition.
    Pause,
    /// A resume request from the control channel (`resume` command). The pump
    /// reconciles its effective pause state, draining both queues via a fresh
    /// index exchange.
    Resume,
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

/// How the session re-establishes a peer after a disconnect. Watch modes keep
/// running offline and reconnect with back-off (invariant #3: local changes are
/// still watched, indexed, and versioned while the peer is gone); `serve` and
/// watch-only have no peer to chase and simply exit on EOF.
#[derive(Debug, Clone)]
enum ReconnectPlan {
    /// No reconnection (watch-only or serve): a disconnect ends the session.
    None,
    /// Re-spawn `serve --stdio` rooted at this local peer path.
    LocalPeer(PathBuf),
    /// Re-run the SSH bootstrap (reusing the remote binary when its version
    /// matches) and re-spawn the remote `serve`.
    Ssh(SshParams),
}

/// One in-progress inbound large-file assembly (docs/SPEC.md §8).
///
/// The change is **not** absorbed into the engine until assembly completes:
/// absorbing at manifest arrival would put the index into a "present" state the
/// disk does not yet hold, and persisting that phantom state means a `kill -9`
/// mid-assembly leaves the index claiming a file the tree lacks — on restart the
/// startup scan then reads that as a local *deletion* and propagates it,
/// destroying the real file on the peer. Instead the change is held here and
/// absorbed + applied atomically at completion, exactly as an inline
/// [`Message::Change`] is (a same-path change arriving meanwhile still supersedes
/// this assembly via [`Session::abandon_superseded`], so the clock is not needed
/// early). Received chunk bytes live as files under `.tomo/staging/chunks/`
/// (invariant #8), tracked by hash in `have`; `requested` is the set already
/// asked for so batches do not overlap.
struct Assembly {
    /// The deferred change (path, `Modified` kind with signature, and clock),
    /// absorbed into the engine only when assembly completes.
    change: RemoteChange,
    /// The whole-file signature the reassembled bytes must match.
    sig: ContentSig,
    /// The ordered chunk-hash manifest from the [`Message::ChangeManifest`].
    manifest: Vec<ChunkHash>,
    /// Set view of `manifest` for O(1) membership — the receiver handles one
    /// lookup per arriving chunk, and a Vec scan made a 1 GiB debug transfer
    /// quadratic (~111s; found by scenario 11).
    manifest_set: HashSet<ChunkHash>,
    /// Chunk hashes whose bytes have arrived and been written to a chunk file.
    have: HashSet<ChunkHash>,
    /// Chunk hashes already requested (in flight), so batches don't overlap.
    requested: HashSet<ChunkHash>,
    /// The declared total content size (equals `sig.size`).
    total_size: u64,
    /// Bytes of accepted chunks so far, for the transient progress line.
    received_bytes: u64,
}

/// Mutable session state owned by the main thread.
// The bools are independent facets of one small state machine (peer liveness,
// handshake progress, and two write-coalescing dirty bits); bundling them into
// sub-structs would obscure rather than clarify.
#[allow(clippy::struct_excessive_bools)]
struct Session {
    layout: Layout,
    config: Config,
    /// The local filesystem's naming semantics, probed once at startup. Drives
    /// the case-collision ingress guard and NFC path canonicalization
    /// (macOS↔Linux filename hazards). Recorded (additively) in `status.json`.
    fs: crate::fsprobe::FsSemantics,
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
    /// Who is on the other end, as cheaply known at connect time (SSH env on the
    /// serving side, the configured `[remote]` on the initiator side). Recorded
    /// in `status.json` and referenced by `.tomo/README.md`. `None` for a
    /// watch-only session or when nothing is known.
    peer_identity: Option<crate::status::Peer>,
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
    /// Top-level path prefixes for which a "not synced" ingress/egress note has
    /// already been emitted, so an ignored tree (e.g. `.git`) yields one dim
    /// note, not one per file.
    noted_ignored: HashSet<String>,
    index_dirty: bool,
    status_dirty: bool,
    last_status: Instant,
    last_index_persist: Instant,
    rescan_pending: bool,
    /// Startup-scan mtime+size cache: lets a scan skip re-hashing unchanged files
    /// (docs/NOTES.md tier-2). Rebuilt wholesale by each full scan and nudged
    /// incrementally as changes are applied/observed; persisted to
    /// `.tomo/state/scancache.bin` for the next startup.
    scan_cache: ScanCache,
    /// Whether `scan_cache` has changed since it was last persisted.
    scan_cache_dirty: bool,
    /// Set when an inbound apply could not be written because the local
    /// filesystem is full. The session stays alive and periodically re-requests
    /// the missing content (docs/NOTES.md tier-2 disk-full degradation); cleared
    /// optimistically on each retry and re-set if the disk is still full.
    disk_stalled: bool,
    /// When the last disk-full retry fired (throttles re-requests to [`STALL_RETRY`]).
    last_stall_retry: Instant,
    last_activity: Instant,
    /// When the last actual file sync happened (an apply/send/remove), driving
    /// the `heartbeat` event's `last_sync_ms_ago`. `None` until the first sync.
    last_sync: Option<Instant>,
    /// When the last `heartbeat` event was published (throttled to
    /// [`HEARTBEAT_CADENCE`]).
    last_heartbeat: Instant,
    /// The most recently computed unresolved-conflict count (from the DB, cached
    /// by `persist`), reported in the `heartbeat` event without a re-query.
    last_unresolved: u64,
    shutdown: Arc<AtomicBool>,
    /// The shared pause flag (docs/SPEC.md §13): set/cleared by the control
    /// channel's `pause`/`resume` commands and read on the main thread's outbound
    /// (`do_send`) and inbound (`on_message`) paths. Authoritative and instantly
    /// effective; the atomic makes a `pause` command block sends before the pump
    /// even wakes. In-memory only — a restarted session always comes up unpaused.
    paused: Arc<AtomicBool>,
    /// The pause state the main thread has already *acted on* (emitted the event,
    /// told the peer, suspended transfers). Compared against [`Session::paused`]
    /// so the transition side effects run exactly once per edge (idempotent
    /// `pause`/`resume`).
    paused_acted: bool,
    /// Whether the *peer* has told us it paused ([`Message::Pause`]): our own
    /// outbound is held (queued into the index) until it resumes, exactly like
    /// the offline queue — but the transport stays connected. Main-thread only.
    peer_paused: bool,
    /// A clone of the unified-channel sender, so a reconnect can hand the new
    /// transport's reader thread the same channel the loop drains.
    tx: Sender<Incoming>,
    /// How to re-establish the peer after a disconnect (watch modes only).
    reconnect_plan: ReconnectPlan,
    /// When offline: the instant the peer dropped (drives the status/report).
    offline_since: Option<Instant>,
    /// When offline: the earliest instant to try reconnecting again.
    next_reconnect_at: Option<Instant>,
    /// Current reconnect back-off (doubles per failure, capped at [`RECONNECT_MAX`]).
    backoff: Duration,
    /// Inbound large-file assemblies in progress, keyed by target path.
    assemblies: HashMap<RelPath, Assembly>,
    /// Sender side: for each path we announced via [`Message::ChangeManifest`],
    /// its chunk hashes paired with their byte ranges in the file. Serving a
    /// [`Message::ChunkRequest`] then `pread`s exactly the requested ranges and
    /// re-verifies each against its hash (so a since-changed file is caught,
    /// invariant #3) — no chunk *bytes* are retained (only these tiny ranges),
    /// and the work is O(bytes requested), never a full re-chunk per batch.
    outbound_manifests: HashMap<RelPath, Vec<(ChunkHash, Range<usize>)>>,
    /// Sender side: the FIFO of [`Message::ChunkData`] frames awaiting shipment,
    /// drained a window at a time so live `Change`s interleave (docs/SPEC.md §8).
    pending_chunks: VecDeque<Message>,
    /// Sender side: the FIFO of **bulk** `Change`s queued by [`Session::reconcile`]
    /// (a first-ever seed or a reconnect/resume re-ship), drained by
    /// [`Session::pump_outbound`] within the bytes-in-flight window. Live changes
    /// (a local watch event's [`Action::Send`]) never enter this queue — they ship
    /// immediately via [`Session::do_send`], the priority lane that keeps a live
    /// edit at normal latency ahead of a running bulk seed (SEED-PERF Phase 1,
    /// invariant #3). `pending_sends_paths` mirrors the queued paths for O(1)
    /// de-duplication when reconcile runs again over an undrained backlog.
    pending_sends: VecDeque<RemoteChange>,
    pending_sends_paths: HashSet<RelPath>,
    /// Bytes-in-flight window for the bulk drain (see [`DEFAULT_SEND_WINDOW_BYTES`]).
    send_window_bytes: usize,
    /// The receive-side half of the same window: shared with every transport's
    /// reader thread, it caps un-applied inbound content bytes so a slow receiver
    /// backpressures the sender (the flow control that makes the priority lane
    /// effective; SEED-PERF Phase 1, bug B3). Session-owned so it survives a
    /// transport swap on reconnect; reset when transfers are abandoned.
    inflight: Arc<InFlight>,
}

/// Run the sync loop to completion.
///
/// # Errors
/// Propagates a fatal error (handshake mismatch, apply failure, framing error,
/// or I/O on a state file). Normal peer disconnect returns `Ok(())`.
// A linear startup orchestration (lock → load index/history → build the session
// → staging reset → startup scan → connect → pump → flush): its length is the
// field-by-field session construction, not branching complexity. Phase 1's three
// new outbound-queue fields nudged it three lines over the pedantic ceiling.
#[allow(clippy::too_many_lines)]
pub fn run(
    layout: Layout,
    config: Config,
    replica: ReplicaId,
    mut reporter: Reporter,
    mode: Mode,
) -> Result<(), CliError> {
    // Single-session lock (both sides): refuse a second sync/serve session for
    // this project. Acquired first so it fails fast — before the history store or
    // watcher — and held for the whole session (dropped when `run` returns; a
    // `kill -9` releases it via the kernel, no staleness logic — see `lockfile`).
    let mode_label = match mode {
        Mode::Serve => "serve",
        Mode::WatchOnly | Mode::LocalPeer(_) | Mode::Ssh(_) => "sync",
    };
    let _session_lock = crate::lockfile::SessionLock::acquire(&layout, mode_label)?;

    // The index is a reconstructible cache. If it is undecodable — the expected
    // outcome the first time an older `index.bin` is opened after a format
    // change (e.g. the executable bit widening `ContentSig`) — load empty and
    // let the startup scan below re-index the tree (a one-time re-index churn).
    let (index, index_recovered) = load_index(&layout.index())?;
    if index_recovered {
        reporter.note(
            "index.bin was unreadable (likely an older on-disk format after an upgrade) — \
             starting from empty and re-indexing the tree",
        );
    }
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
    // The shared pause flag (docs/SPEC.md §13): the control channel flips it; the
    // main loop reads it on the send/apply paths. Always false at startup — a
    // restarted (or `kill -9`'d then relaunched) session comes up unpaused.
    let paused = Arc::new(AtomicBool::new(false));

    // Probe the local filesystem's naming semantics ONCE, before the watcher
    // starts, so its canonicalizer and every scan normalize (or don't) to match
    // the FS (macOS↔Linux filename hazards). Best-effort: any I/O failure falls
    // back to the safe byte-preserving, case-sensitive default, under which both
    // filename guards are inert.
    let fs = probe_fs(&layout, &reporter);

    // Load the persisted startup-scan cache (absent/corrupt/old → empty, rebuilt
    // by the startup scan). Path captured before `layout` moves into the session.
    let scancache_path = layout.scancache();

    // Watcher → forwarder thread → unified channel.
    let (ws_tx, ws_rx) = mpsc::channel::<WatchSignal>();
    let _watcher: Watcher =
        Watcher::start(layout.root(), config.clone(), fs.normalizes_unicode, ws_tx)?;
    spawn_watch_forwarder(ws_rx, tx.clone());

    // Control channel (UX-V2 §2): a per-session unix socket serving the event
    // stream and command channel. The reporter is tapped so its existing call
    // sites also publish structured records; the command handlers reuse the CLI
    // one-shot functions (status/conflicts). `_ctl` lives to the end of `run` —
    // its `Drop` closes subscribers and removes the socket on clean shutdown.
    let _ctl = start_control_channel(&layout, &mut reporter, &shutdown, &paused, &tx)?;

    let mut session = Session {
        layout,
        config,
        fs,
        engine,
        reporter,
        binary_version: buildinfo::binary_version(),
        transport: None,
        history,
        pressure,
        peer_replica: None,
        peer_identity: None,
        started: Instant::now(),
        versions_recorded: 0,
        conflicts_recorded: 0,
        ssh_params: None,
        repush_requested: false,
        connected: false,
        hello_received: false,
        conflicts: BTreeSet::new(),
        noted_ignored: HashSet::new(),
        index_dirty: false,
        status_dirty: true,
        last_status: Instant::now(),
        last_index_persist: Instant::now(),
        rescan_pending: false,
        scan_cache: load_scan_cache(&scancache_path),
        scan_cache_dirty: false,
        disk_stalled: false,
        last_stall_retry: Instant::now(),
        last_activity: Instant::now(),
        last_sync: None,
        last_heartbeat: Instant::now(),
        last_unresolved: 0,
        shutdown,
        paused,
        paused_acted: false,
        peer_paused: false,
        tx: tx.clone(),
        reconnect_plan: ReconnectPlan::None,
        offline_since: None,
        next_reconnect_at: None,
        backoff: RECONNECT_MIN,
        assemblies: HashMap::new(),
        outbound_manifests: HashMap::new(),
        pending_chunks: VecDeque::new(),
        pending_sends: VecDeque::new(),
        pending_sends_paths: HashSet::new(),
        send_window_bytes: send_window_bytes(),
        inflight: Arc::new(InFlight::new(send_window_bytes() as u64)),
    };

    // Everything under `.tomo/staging/` at boot is scratch from a previous,
    // now-dead session (received chunk files, or an interrupted atomic-write
    // temp) — a `kill -9` can only ever leave garbage there, never a torn file
    // at a final path (invariant #8). The single-session lock we just acquired
    // guarantees no other live session is using staging, so wipe it before
    // doing anything else.
    session.reset_staging()?;

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
    // Publish a final session-state event to any attached control-channel
    // subscriber before the socket closes (best-effort — `_ctl`'s Drop closes
    // subscribers right after `run` returns).
    session.reporter.emit_disconnected();
    let flush = session.persist(true);
    if let Some(mut t) = session.transport.take() {
        t.join_reader();
    }
    outcome.and(drained).and(flush)
}

/// Bring up the control channel: create the event broadcaster, tap the reporter
/// with it, and start the [`ControlServer`] bound at `.tomo/state/ctl.sock`. The
/// returned server tears the socket down (and closes subscribers) on `Drop`.
fn start_control_channel(
    layout: &Layout,
    reporter: &mut Reporter,
    shutdown: &Arc<AtomicBool>,
    paused: &Arc<AtomicBool>,
    tx: &Sender<Incoming>,
) -> Result<crate::ctl::ControlServer, CliError> {
    let broadcaster = crate::ctl::broadcast::Broadcaster::new();
    reporter.attach_events(crate::ctl::EventSink::new(Arc::clone(&broadcaster)));
    crate::ctl::ControlServer::start(
        layout,
        broadcaster,
        crate::ctl::CommandContext::new(
            layout.clone(),
            Arc::clone(shutdown),
            Arc::clone(paused),
            tx.clone(),
        ),
    )
}

/// Probe the local filesystem's naming semantics and emit a one-line note when
/// either filename guard will be active (case-insensitive or unicode-normalizing).
fn probe_fs(layout: &Layout, reporter: &Reporter) -> crate::fsprobe::FsSemantics {
    let fs = crate::fsprobe::probe(&layout.state());
    if fs.case_insensitive || fs.normalizes_unicode {
        let mut traits = Vec::new();
        if fs.case_insensitive {
            traits.push("case-insensitive");
        }
        if fs.normalizes_unicode {
            traits.push("unicode-normalizing");
        }
        reporter.note(&format!(
            "filesystem: {} — filename guards active",
            traits.join(", ")
        ));
    }
    fs
}

/// Build the initiator-side peer block from the SSH params: the configured
/// target host as the name, and the `~/.ssh/config`-resolved host as the address
/// when it is known and differs from the raw target. `source` is `config`.
fn peer_from_ssh_params(params: &SshParams) -> crate::status::Peer {
    let addr = tomo_transport::resolve_route(&params.target, &params.opts)
        .ok()
        .map(|r| r.target.host_name);
    crate::status::Peer {
        name: Some(params.target.clone()),
        addr,
        source: Some("config".to_owned()),
    }
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
        let changes = self.rescan_with_cache()?;
        for change in changes {
            let actions = self.engine.handle(Event::Local(change));
            self.execute(actions, None, None)?;
        }
        Ok(())
    }

    /// Run a full `scan_diff` consulting (and rebuilding) the startup-scan cache,
    /// so unchanged files skip re-hashing. Replaces `self.scan_cache` with the
    /// freshly rebuilt one and marks it for persistence. Shared by the startup
    /// scan and the deferred reconciling rescan.
    fn rescan_with_cache(&mut self) -> Result<Vec<LocalChange>, CliError> {
        let (changes, fresh) = scan_diff_cached(
            self.layout.root(),
            self.engine.index(),
            &self.config,
            self.fs.normalizes_unicode,
            &self.scan_cache,
            wall_ns(),
        )?;
        self.scan_cache = fresh;
        self.scan_cache_dirty = true;
        Ok(changes)
    }

    /// Update the scan cache to reflect that `path` now holds `sig` on disk (after
    /// applying a remote change or observing a local edit), so the next startup
    /// scan can skip re-hashing it. Best-effort: if the file cannot be stat'd as a
    /// regular file the entry is dropped (forcing a fresh hash later).
    fn cache_note_present(&mut self, path: &RelPath, sig: ContentSig) {
        let full = join(self.layout.root(), path);
        match tomo_watch::stat_entry(&full, sig) {
            Some(entry) => self.scan_cache.insert(path.clone(), entry),
            None => self.scan_cache.remove(path),
        }
        self.scan_cache_dirty = true;
    }

    /// Drop `path` from the scan cache (it was removed on disk).
    fn cache_note_absent(&mut self, path: &RelPath) {
        self.scan_cache.remove(path);
        self.scan_cache_dirty = true;
    }

    /// Bring up the selected transport and send our opening [`Message::Hello`].
    fn connect(&mut self, mode: Mode, tx: &Sender<Incoming>) -> Result<(), CliError> {
        // `peer` describes the far side for the styled startup banner; `None`
        // suppresses the banner for `serve` (its stdout is the wire anyway).
        // `side` selects the wording of the agent-context README below.
        let mut side = crate::readme::Side::Initiator;
        let inflight = Arc::clone(&self.inflight);
        let (transport, peer): (Option<Transport>, Option<String>) = match mode {
            Mode::WatchOnly => {
                self.reporter
                    .note("watch-only (no peer) — maintaining index and status");
                (None, None)
            }
            Mode::LocalPeer(path) => {
                crate::init::ensure_initialized(&Layout::new(&path))?;
                self.reporter
                    .note(&format!("local peer at {}", path.display()));
                self.reconnect_plan = ReconnectPlan::LocalPeer(path.clone());
                let peer = path.display().to_string();
                (
                    Some(transport::local_peer(&path, tx, &inflight)?),
                    Some(peer),
                )
            }
            Mode::Serve => {
                // Serving side: learn who connected from the SSH environment the
                // initiator prepended (TOMO_PEER_NAME) plus SSH_CONNECTION.
                side = crate::readme::Side::Serving;
                self.peer_identity = crate::status::peer_from_ssh_env();
                (Some(transport::stdio(tx, &inflight)), None)
            }
            Mode::Ssh(params) => {
                let peer = transport::describe_route(&params);
                // Initiator side: record the peer from the configured [remote]
                // (the target host, and the resolved host as the address).
                self.peer_identity = Some(peer_from_ssh_params(&params));
                self.reporter
                    .note(&format!("connecting to {peer} over SSH"));
                let (t, report) = transport::ssh(&params, tx, &inflight, false)?;
                for note in t.notes() {
                    self.reporter.note(&note);
                }
                self.report_bootstrap(&report);
                self.reconnect_plan = ReconnectPlan::Ssh(params.clone());
                self.ssh_params = Some(params);
                (Some(t), Some(peer))
            }
        };

        // Refresh the agent-context README now that the peer is (cheaply) known,
        // on BOTH sides — so a pre-existing project gets it on the next sync and
        // the bootstrapped remote gets it at serve startup. Best-effort: a write
        // failure is noted and ignored (syncing matters more than the README).
        self.write_agent_readme(side);

        // The banner is styled-only (no plain/JSON equivalent); it prints for a
        // peer session, never for `serve` (peer is `None` there).
        if let Some(peer) = &peer {
            let (version, dir) = (self.binary_version.clone(), self.dir_label());
            self.reporter.banner(&version, &dir, peer);
        }
        if let Some(t) = transport {
            self.transport = Some(t);
            self.send_opening_hello()?;
        }
        Ok(())
    }

    /// A short human label for this project's directory, used in the banner: the
    /// root's final path component, or its full display path as a fallback.
    fn dir_label(&self) -> String {
        self.layout
            .root()
            .file_name()
            .and_then(|s| s.to_str())
            .map_or_else(|| self.layout.root().display().to_string(), str::to_owned)
    }

    /// (Re)write `.tomo/README.md` for this session's `side`, embedding the path
    /// to the binary that serves this project (this process's `current_exe`,
    /// which on a bootstrapped remote is the pushed `.tomo/bin/tomo-…`) and the
    /// peer identity known at connect time. Best-effort — a failure is noted and
    /// swallowed so it can never abort a sync.
    fn write_agent_readme(&self, side: crate::readme::Side) {
        let (peer_name, peer_addr) = self
            .peer_identity
            .as_ref()
            .map_or((None, None), |p| (p.name.clone(), p.addr.clone()));
        let ctx = crate::readme::ReadmeContext {
            tomo_version: self.binary_version.clone(),
            binary_path: crate::readme::current_binary_path(),
            side,
            peer_name,
            peer_addr,
        };
        if let Err(e) = crate::readme::write_if_needed(&self.layout, &ctx) {
            self.reporter
                .note(&format!("could not write .tomo/README.md: {e}"));
        }
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
                embedded,
                dev_substitution,
                ..
            } => {
                let origin = if *embedded {
                    " [embedded static artifact]"
                } else {
                    ""
                };
                self.reporter.note(&format!(
                    "pushed remote binary tomo {version} ({triple}, {bytes} bytes){origin}"
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
        self.inflight.reset();

        let (t, report) = transport::ssh(&params, tx, &Arc::clone(&self.inflight), true)?;
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

    // ---- Offline / reconnect ---------------------------------------------

    /// Drop into the offline state: retire the transport, keep watching and
    /// versioning locally, and schedule a reconnect. Idempotent — a second call
    /// while already offline is a no-op (both the reader thread's `PeerEof` and
    /// a failed send may report the same drop). Any in-flight transfers are
    /// abandoned; the head-shipping reconcile on reconnect re-ships whatever the
    /// peer missed (invariant #5: dropped sends never block or lose data).
    fn go_offline(&mut self, reason: &str) {
        if self.transport.is_none() {
            return; // already offline
        }
        // Surface the remote peer's dying words (SSH transport captures stderr;
        // the local-peer child inherits stderr, so its message already reached
        // the terminal). This turns a bare "EOF" into an actionable reason —
        // e.g. the peer refused because its own session lock is held.
        let tail = self.transport.as_ref().and_then(Transport::stderr_tail);
        if let Some(tail) = &tail {
            self.reporter
                .note(&format!("remote reported:\n{}", tail.trim_end()));
        }
        if let Some(t) = self.transport.take() {
            t.deactivate();
            // Dropping the transport tears the peer down (child reaped / SSH
            // session closed).
        }
        self.connected = false;
        self.hello_received = false;
        self.status_dirty = true;
        self.reporter.emit_disconnected();
        // Abandon in-flight transfers in both directions — a fresh reconcile
        // rebuilds what's needed after reconnect.
        self.suspend_transfers();
        let now = Instant::now();
        self.offline_since = Some(now);
        self.backoff = RECONNECT_MIN;
        self.next_reconnect_at = Some(now + self.backoff);
        self.reporter
            .note(&format!("peer disconnected — queueing changes ({reason})"));
    }

    /// If offline and the back-off has elapsed, attempt to re-establish the
    /// peer. On success the handshake resumes (fresh `Hello` → `IndexExchange` →
    /// reconcile, which re-ships the offline queue); on failure the back-off
    /// doubles up to [`RECONNECT_MAX`].
    fn maybe_reconnect(&mut self) -> Result<(), CliError> {
        if self.transport.is_some() {
            return Ok(());
        }
        if !matches!(self.next_reconnect_at, Some(at) if Instant::now() >= at) {
            return Ok(());
        }
        let plan = self.reconnect_plan.clone();
        let tx = self.tx.clone();
        let inflight = Arc::clone(&self.inflight);
        match plan {
            ReconnectPlan::None => Ok(()),
            ReconnectPlan::LocalPeer(path) => match transport::local_peer(&path, &tx, &inflight) {
                Ok(t) => self.on_reconnected(t),
                Err(e) => {
                    self.reconnect_failed(&e.to_string());
                    Ok(())
                }
            },
            ReconnectPlan::Ssh(params) => match transport::ssh(&params, &tx, &inflight, false) {
                Ok((t, report)) => {
                    self.report_bootstrap(&report);
                    self.on_reconnected(t)
                }
                Err(e) => {
                    self.reconnect_failed(&e.to_string());
                    Ok(())
                }
            },
        }
    }

    /// Install a freshly reconnected transport and re-open the handshake.
    fn on_reconnected(&mut self, transport: Transport) -> Result<(), CliError> {
        self.transport = Some(transport);
        self.offline_since = None;
        self.next_reconnect_at = None;
        self.backoff = RECONNECT_MIN;
        self.status_dirty = true;
        self.reporter.note("reconnected");
        self.send_opening_hello()
    }

    // ---- Pause / resume (docs/SPEC.md §13) --------------------------------

    /// Whether this session has paused itself (the shared control-channel flag).
    /// Instantly effective: a `pause` command flips it before the pump even wakes.
    fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// Whether outbound changes must be held (queued) right now: either we paused
    /// ourselves, or the peer told us it paused. In both cases we keep absorbing
    /// and versioning local changes; the resume-time index reconcile re-ships
    /// whatever accumulated (the offline-queue model, invariant #5).
    fn outbound_suspended(&self) -> bool {
        self.is_paused() || self.peer_paused
    }

    /// Bring the main thread's *acted* pause state in line with the shared flag,
    /// running the transition side effects exactly once per edge. Idempotent, so
    /// a double `pause`/`resume` (or a coalesced pair) does nothing extra.
    fn reconcile_pause_state(&mut self) -> Result<(), CliError> {
        let want = self.is_paused();
        if want == self.paused_acted {
            return Ok(());
        }
        self.paused_acted = want;
        if want {
            self.enter_pause()
        } else {
            self.exit_pause()
        }
    }

    /// Transition into the paused state: surface it, tell the peer so it holds
    /// its own outbound queue rather than ship into a void, and tear down any
    /// in-flight transfer (a fresh reconcile on resume rebuilds what is needed —
    /// exactly as `go_offline` does). Local observation and history capture keep
    /// running (invariants #3/#4 apply to the resumed state).
    fn enter_pause(&mut self) -> Result<(), CliError> {
        self.reporter.paused();
        self.status_dirty = true;
        self.suspend_transfers();
        // Best-effort peer notification. If we are offline the frame is skipped;
        // `on_hello` re-announces the pause once the handshake completes.
        self.send(&Message::Pause)
    }

    /// Transition out of the paused state: surface it, tell the peer, and drive a
    /// bidirectional index reconcile that drains both queues and converges (the
    /// same head-shipping reconcile the handshake and a reconnect use). Any
    /// conflict that materializes here surfaces non-blockingly (invariant #5).
    fn exit_pause(&mut self) -> Result<(), CliError> {
        self.reporter.resumed();
        self.status_dirty = true;
        self.send(&Message::Resume)?;
        // Re-ship our index so the peer reconciles and drains what we queued for
        // it; the peer answers `Resume` with its own index so we reconcile and
        // drain what it queued for us (see `on_peer_resume`).
        self.resync_indices()
    }

    /// The peer told us it paused ([`Message::Pause`]): hold our outbound queue
    /// (our edits keep absorbing into the index) and surface it non-blockingly as
    /// a note + status. Idempotent. Tear down in-flight transfers, mirroring the
    /// pauser — the resume-time reconcile rebuilds them.
    fn on_peer_pause(&mut self) {
        if self.peer_paused {
            return;
        }
        self.peer_paused = true;
        self.status_dirty = true;
        self.reporter
            .note("peer paused syncing — queueing local changes until it resumes");
        self.suspend_transfers();
    }

    /// The peer resumed ([`Message::Resume`]): clear the hold and re-ship our
    /// index so the peer reconciles and drains what we queued for it. Idempotent.
    fn on_peer_resume(&mut self) -> Result<(), CliError> {
        let was_paused = self.peer_paused;
        self.peer_paused = false;
        self.status_dirty = true;
        if was_paused {
            self.reporter.note("peer resumed syncing");
        }
        self.resync_indices()
    }

    /// Ship our full index to the peer, driving its head-shipping reconcile — the
    /// mechanism that drains a queue after pause/resume, exactly as the handshake
    /// and reconnect do. A no-op when there is no transport (offline / watch-only).
    fn resync_indices(&mut self) -> Result<(), CliError> {
        if self.transport.is_none() {
            return Ok(());
        }
        let index_snapshot: Index = self.engine.index().clone();
        self.send(&Message::IndexExchange(index_snapshot))
    }

    /// Abandon every in-flight transfer in both directions (assemblies, queued
    /// chunk data, announced manifests). Shared by `go_offline` and the pause
    /// transitions — a fresh reconcile re-establishes whatever is still needed.
    fn suspend_transfers(&mut self) {
        self.abandon_all_assemblies();
        self.pending_chunks.clear();
        self.pending_sends.clear();
        self.pending_sends_paths.clear();
        self.outbound_manifests.clear();
        // Zero the receive window and wake a (now-retired) reader blocked in
        // `acquire`, so a fresh transport starts from an empty window.
        self.inflight.reset();
    }

    // ---- Disk-full degradation (docs/NOTES.md tier-2) --------------------

    /// Enter the disk-full stall: note loudly and arm the retry. Never fatal
    /// (invariant #5) and never leaves anything partial at a final path
    /// (invariant #8 — the atomic write cleaned up, and an assembly is abandoned
    /// before absorb). The retry re-requests the missing content once space frees.
    fn note_disk_full(&mut self, ctx: &str) {
        self.reporter.error(&format!(
            "disk full while {ctx}: the local filesystem is out of space — stalling this \
             transfer (nothing was partially written, no data lost). Will re-request it \
             automatically once space is freed."
        ));
        self.disk_stalled = true;
        self.status_dirty = true;
        // Schedule (not fire) the first retry a short interval out.
        self.last_stall_retry = Instant::now();
    }

    /// While stalled on a full disk, periodically re-request whatever we still
    /// lack by re-sending our index — the peer's reconcile then reships every
    /// head we do not cover (a disk-full drop never absorbed the head, so the
    /// missing file is uncovered). Cleared optimistically; a still-full disk
    /// re-sets it on the next failed apply, so this self-heals the instant space
    /// is freed and costs nothing once converged.
    fn maybe_retry_stall(&mut self) -> Result<(), CliError> {
        if !self.disk_stalled {
            return Ok(());
        }
        // Only meaningful once connected and past the handshake (the peer must be
        // able to answer an IndexExchange).
        if self.transport.is_none() || !self.hello_received {
            return Ok(());
        }
        if self.last_stall_retry.elapsed() < STALL_RETRY {
            return Ok(());
        }
        self.last_stall_retry = Instant::now();
        self.disk_stalled = false;
        self.status_dirty = true;
        self.reporter
            .note("retrying after a disk-full stall: re-requesting missing files from the peer");
        let index_snapshot: Index = self.engine.index().clone();
        self.send(&Message::IndexExchange(index_snapshot))
    }

    /// Record a failed reconnect attempt and back off (doubling, capped).
    fn reconnect_failed(&mut self, reason: &str) {
        self.backoff = (self.backoff * 2).min(RECONNECT_MAX);
        self.next_reconnect_at = Some(Instant::now() + self.backoff);
        self.reporter.note(&format!(
            "reconnect failed ({reason}); retrying in {}s",
            self.backoff.as_secs()
        ));
    }

    /// The blocking main loop. In watch modes it runs until shutdown (a peer
    /// disconnect drops into offline+reconnect, never ending the loop); `serve`
    /// and watch-only return on peer EOF. Returns `Ok` on a clean exit or a
    /// fatal error otherwise.
    fn pump(&mut self, rx: &mpsc::Receiver<Incoming>) -> Result<(), CliError> {
        loop {
            // Drain a bytes-in-flight window of the outbound bulk stream (queued
            // chunk data + reconcile-queued Changes), coalescing the frames and
            // servicing the priority lane (live changes) between sub-batches, so a
            // bulk seed streams at full rate without ever starving a live edit
            // (docs/SPEC.md §8, invariant #3; SEED-PERF Phase 1).
            if self.pump_outbound(rx)? {
                return Ok(());
            }
            if self.repush_requested {
                return Ok(());
            }

            // Wake at the sooner of the status cadence, the next staged history
            // deadline, a pending reconnect, and (while a bulk backlog remains)
            // immediately — so nothing waits behind a long idle timeout.
            let timeout = self.recv_deadline();
            let first = match rx.recv_timeout(timeout) {
                Ok(item) => Some(item),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            };
            if let Some(first) = first {
                // Drain the rest of the burst already queued and coalesce
                // redundant same-path watch signals, so a large-file write's
                // flood of intermediate `Dirty` events resolves once (each
                // resolve hashes the whole file) instead of hundreds of times —
                // which otherwise starves live small-file changes and stalls the
                // bulk transfer (invariant #3 still ships the final state).
                if self.process_burst(first, rx)? {
                    return Ok(());
                }
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
            // signal, attempt a reconnect if we are offline and it is due, then
            // persist. Status refreshes on the idle cadence so counters stay
            // current even when nothing changes.
            self.maybe_rescan()?;
            self.pump_history()?;
            self.maybe_reconnect()?;
            self.maybe_retry_stall()?;
            self.persist(false)?;
            self.maybe_emit_heartbeat();
        }
    }

    /// Coalesce and process a burst that begins with `first` plus everything
    /// already queued. Returns `Ok(true)` when the caller should leave [`pump`].
    fn process_burst(
        &mut self,
        first: Incoming,
        rx: &mpsc::Receiver<Incoming>,
    ) -> Result<bool, CliError> {
        let batch = coalesce_burst(drain_burst(first, rx));
        for item in batch {
            if self.process_incoming(item)? {
                return Ok(true);
            }
            if self.repush_requested {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Service the priority lane without blocking: process every [`Incoming`] that
    /// is already queued (a live watch event ships its change immediately via
    /// [`do_send`], jumping ahead of the bulk backlog) and return. Returns
    /// `Ok(true)` when the caller should leave [`pump`].
    ///
    /// This is what upholds invariant #3 during a bulk seed: [`pump_outbound`]
    /// calls it between bulk sub-batches, so a live edit is picked up and shipped
    /// within one sub-batch rather than queued behind thousands of seeded files.
    fn service_ready(&mut self, rx: &mpsc::Receiver<Incoming>) -> Result<bool, CliError> {
        match rx.try_recv() {
            Ok(first) => self.process_burst(first, rx),
            Err(_) => Ok(false),
        }
    }

    /// Drain the outbound bulk stream within one bytes-in-flight window, then
    /// return so the loop can block for the next event and run its periodic work.
    /// Returns `Ok(true)` when a serviced live event asks the caller to leave
    /// [`pump`] (shutdown / peer EOF / repush).
    ///
    /// Each sub-batch coalesces up to [`SEND_BATCH_FRAMES`] queued frames — chunk
    /// data first (keep the puller fed), then reconcile-queued `Change`s — into a
    /// single batched write, then [`service_ready`] gives the priority lane a
    /// turn. Draining stops when the queues empty or the window budget is spent;
    /// with a backlog still present, [`recv_deadline`] returns zero so the next
    /// loop pass resumes immediately. Over a pipe transport the batched writes
    /// block on backpressure at the receiver's apply rate — bounding a live edit's
    /// wait to one sub-batch — while over the SSH channel's unbounded queue the
    /// window instead bounds the frames buffered per pass.
    fn pump_outbound(&mut self, rx: &mpsc::Receiver<Incoming>) -> Result<bool, CliError> {
        if self.transport.is_none() {
            return Ok(false);
        }
        let budget = self.send_window_bytes as u64;
        let mut spent: u64 = 0;
        loop {
            let wrote = self.ship_bulk_subbatch()?;
            spent += wrote;
            // A send may have dropped us offline; the queues were cleared there.
            if self.transport.is_none() {
                return Ok(false);
            }
            // Priority lane between sub-batches (may itself enqueue more bulk).
            if self.service_ready(rx)? {
                return Ok(true);
            }
            if self.repush_requested {
                return Ok(true);
            }
            if wrote == 0 || spent >= budget {
                return Ok(false);
            }
        }
    }

    /// Ship one coalesced sub-batch (≤ [`SEND_BATCH_FRAMES`] frames) of the
    /// outbound bulk queues as a single batched write, returning the wire bytes
    /// emitted (0 when both queues are drained). Chunk data drains before
    /// reconcile-queued `Change`s so a receiver's requested chunks stay ahead.
    fn ship_bulk_subbatch(&mut self) -> Result<u64, CliError> {
        let mut batch: Vec<Message> = Vec::new();
        while batch.len() < SEND_BATCH_FRAMES {
            if let Some(msg) = self.pending_chunks.pop_front() {
                batch.push(msg);
                continue;
            }
            let Some(change) = self.pending_sends.pop_front() else {
                break;
            };
            self.pending_sends_paths.remove(&change.path);
            // Re-reads the file and drops a stale/ignored/suspended change.
            if let Some(msg) = self.prepare_send(change) {
                batch.push(msg);
            }
        }
        if batch.is_empty() {
            return Ok(0);
        }
        let Some(t) = self.transport.as_mut() else {
            return Ok(0);
        };
        match t.tx.send_batch(&batch) {
            Ok(bytes) => Ok(bytes),
            Err(e) if self.reconnecting() => {
                self.go_offline(&format!("send failed: {e}"));
                Ok(0)
            }
            Err(e) => Err(e),
        }
    }

    /// Publish a periodic `heartbeat` event while a control-channel subscriber is
    /// attached. Skipped entirely when nobody is watching, so an idle session
    /// with no observer never wakes for or emits heartbeats.
    fn maybe_emit_heartbeat(&mut self) {
        if !self.reporter.has_event_subscribers() {
            return;
        }
        if self.last_heartbeat.elapsed() < HEARTBEAT_CADENCE {
            return;
        }
        self.last_heartbeat = Instant::now();
        let ago = self
            .last_sync
            .map(|t| u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX));
        self.reporter
            .emit_heartbeat(ago, self.last_unresolved, self.is_paused());
    }

    /// Process one (already coalesced) incoming item. Returns `Ok(true)` when
    /// the caller should return from [`pump`] (a clean exit); `Ok(false)` to
    /// keep looping.
    fn process_incoming(&mut self, item: Incoming) -> Result<bool, CliError> {
        match item {
            Incoming::Watch(signal) => {
                self.last_activity = Instant::now();
                self.on_watch(signal)?;
                Ok(false)
            }
            Incoming::Message(msg) => {
                self.last_activity = Instant::now();
                // Release this frame's reservation in the bytes-in-flight window
                // once it is applied, freeing the reader to pull the next (the
                // receive half of the flow control — matches the reader's
                // `acquire`). Released even on error so the window never leaks.
                let cost = transport::inflight_cost(&msg);
                let result = self.on_message(msg);
                self.inflight.release(cost);
                result?;
                Ok(false)
            }
            Incoming::PeerEof => {
                if self.reconnecting() {
                    self.go_offline("peer disconnected");
                    Ok(false)
                } else {
                    self.reporter.note("peer disconnected");
                    Ok(true)
                }
            }
            Incoming::ProtoError(e) => {
                if self.reconnecting() {
                    self.go_offline(&format!("transport error: {e}"));
                    Ok(false)
                } else {
                    Err(CliError::msg(format!("transport error: {e}")))
                }
            }
            // A control-channel `stop`: exit the loop cleanly regardless of mode
            // (even a reconnecting sync session), the same terminal path as a
            // SIGTERM. `run` then flushes history/index/status and tears down.
            Incoming::Shutdown => {
                self.reporter.note("shutting down (control request)");
                Ok(true)
            }
            // A control-channel `pause`/`resume`: reconcile the effective pause
            // state against the shared flag (idempotent; the flag already gates
            // sends/applies — this drives the one-time transition side effects).
            Incoming::Pause | Incoming::Resume => {
                self.reconcile_pause_state()?;
                Ok(false)
            }
        }
    }

    /// Whether this session chases a dropped peer (watch modes) rather than
    /// exiting on disconnect.
    fn reconnecting(&self) -> bool {
        matches!(
            self.reconnect_plan,
            ReconnectPlan::LocalPeer(_) | ReconnectPlan::Ssh(_)
        )
    }

    /// The recv timeout: the sooner of the status cadence, the time until the
    /// next staged history capture is due, a pending reconnect, and (when a
    /// rescan is pending) the quiescence window. While a bulk backlog remains the
    /// loop does not block at all — it returns to [`pump_outbound`] at once so the
    /// window keeps draining (the priority lane is serviced there, not here).
    fn recv_deadline(&self) -> Duration {
        if !self.pending_chunks.is_empty() || !self.pending_sends.is_empty() {
            return Duration::ZERO;
        }
        let mut base = match self.pressure.next_due_ms() {
            Some(due) => {
                let now = self.now_ms();
                Duration::from_millis(due.saturating_sub(now)).min(STATUS_CADENCE)
            }
            None => STATUS_CADENCE,
        };
        if self.rescan_pending {
            base = base.min(RESCAN_QUIESCENT);
        }
        if self.disk_stalled {
            base = base.min(STALL_RETRY);
        }
        if let Some(at) = self.next_reconnect_at {
            base = base.min(at.saturating_duration_since(Instant::now()));
        }
        // Wake often enough to publish heartbeats while a subscriber is watching
        // (never when idle with no observer — that stays fully idle).
        if self.reporter.has_event_subscribers() {
            base = base.min(HEARTBEAT_CADENCE);
        }
        base
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
                let Ok(mut change) = tomo_watch::resolve(self.layout.root(), &pending) else {
                    // Transient read failure: reconcile via rescan rather
                    // than dropping the change silently.
                    self.rescan_pending = true;
                    return Ok(());
                };
                // A `Gone` pending resolves to `Removed` *unconditionally* (no
                // re-stat). If a concurrent apply re-created the file since the
                // raw deletion event — e.g. our own delete lost a delete-vs-edit
                // conflict and the winning edit was just written — that `Removed`
                // is stale and would spuriously re-delete a file that exists on
                // disk (and propagate the deletion to the peer). Trust disk: if
                // the path is present now, treat the event as a `Modified` of the
                // current content (which the echo journal then swallows).
                if matches!(change.kind, ChangeKind::Removed) {
                    if let Ok(Some(sig)) = tomo_watch::snapshot(self.layout.root(), &change.path) {
                        change.kind = ChangeKind::Modified(sig);
                    }
                }
                // Truncate-then-write saves pass through a genuinely-empty
                // disk state; if we sample inside that window the peer briefly
                // shows a zero-byte file at the target path, which SPEC §5.1
                // forbids ("never a zero-byte intermediate" — caught by
                // scenario 03 on slow CI runners where the window is wide).
                // Narrow hold: an EMPTY observation of a file whose current
                // winner is non-empty gets one short re-resolve; a real
                // truncation to empty survives the re-check and ships
                // normally. Bounded, one-shot, empty-transitions only — the
                // live-latency invariant (#3) is untouched for every other
                // change.
                if let ChangeKind::Modified(sig) = change.kind {
                    if sig.size == 0 && self.winner_is_nonempty(&change.path) {
                        std::thread::sleep(EMPTY_HOLD);
                        if let Ok(Some(now_sig)) =
                            tomo_watch::snapshot(self.layout.root(), &change.path)
                        {
                            change.kind = ChangeKind::Modified(now_sig);
                        }
                    }
                }
                // Keep the scan cache current with what the watcher just observed
                // on disk, so a future startup scan can skip re-hashing this file.
                match &change.kind {
                    ChangeKind::Modified(sig) => self.cache_note_present(&change.path, *sig),
                    ChangeKind::Removed => self.cache_note_absent(&change.path),
                }
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

    /// Whether the engine's current winner for `path` is a non-empty present
    /// file (drives the [`EMPTY_HOLD`] truncate-window re-check).
    fn winner_is_nonempty(&self, path: &tomo_engine::RelPath) -> bool {
        self.engine.index().get(path).is_some_and(
            |e| matches!(e.winner().state, tomo_engine::EntryState::Present(sig) if sig.size > 0),
        )
    }

    /// Run a deferred reconciling rescan once no change has been processed for
    /// [`RESCAN_QUIESCENT`]. See the `NeedsRescan` arm for why deferral is a
    /// correctness requirement, not an optimization.
    fn maybe_rescan(&mut self) -> Result<(), CliError> {
        if !self.rescan_pending || self.last_activity.elapsed() < RESCAN_QUIESCENT {
            return Ok(());
        }
        self.rescan_pending = false;
        let changes = self.rescan_with_cache()?;
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
        // While we are paused we apply nothing inbound: drop content-bearing
        // frames outright. The peer stops shipping the moment it sees our
        // `Message::Pause`, so at most one in-flight window's worth ever arrives
        // here; dropping it is safe because the resume-time index reconcile
        // re-fetches whatever we missed (the peer's unsent heads are its queue —
        // invariant #5, nothing lost). Liveness (Ping/Pong), the handshake, and
        // the pause/resume control frames still flow so the link stays healthy.
        if self.is_paused() && is_content_frame(&msg) {
            return Ok(());
        }
        match msg {
            Message::Hello {
                protocol,
                binary_version,
                replica,
            } => self.on_hello(protocol, &binary_version, replica),
            Message::IndexExchange(peer_index) => {
                self.reconcile(&peer_index);
                Ok(())
            }
            Message::Change { change, bytes } => {
                // Ingress filter: an ignored-class (or wrong-direction) path is
                // refused here — never applied, absorbed, or versioned — even if
                // a peer on an older binary still ships it (e.g. a `.git` tree).
                if !self.allow_crossing(&change.path, crate::crossing::Flow::Inbound) {
                    return Ok(());
                }
                // Case-collision guard (case-insensitive FS): refuse an apply
                // that would overwrite a different, case-folded-equal existing
                // file — preserve the incoming bytes to history instead. Runs
                // before any absorb so the engine never learns the refused path.
                if self.case_collision_refused(&change, bytes.as_deref())? {
                    return Ok(());
                }
                // If our own bulk reconcile frame for this path is still queued,
                // ship it before the apply overwrites the source — so the peer
                // sees our version and both sides resolve the conflict the same
                // way (invariant #5; SEED-PERF Phase 1).
                self.flush_queued_send(&change.path)?;
                // A live small-file change supersedes any large assembly still
                // in flight for the same path (invariant #3).
                self.abandon_superseded(&change.path);
                // Reconcile any unobserved local edit at this path FIRST, so the
                // incoming apply can never silently clobber it (invariant #5).
                self.reconcile_unobserved_local(&change.path)?;
                // Keep the incoming version so history attributes these captures
                // (and any conflict heads) to the peer, not to us.
                let remote_version = change.version.clone();
                let actions = self.engine.handle(Event::Remote(change));
                self.mark_dirty();
                self.execute(actions, bytes.as_deref(), Some(&remote_version))
            }
            Message::ChangeManifest {
                change,
                total_size,
                manifest,
            } => self.on_change_manifest(change, total_size, manifest),
            Message::ChunkRequest { hashes } => {
                self.on_chunk_request(&hashes);
                Ok(())
            }
            Message::ChunkData { hash, bytes } => self.on_chunk_data(hash, &bytes),
            Message::Ping { nonce } => self.send(&Message::Pong { nonce }),
            // Liveness replies carry no state.
            Message::Pong { .. } => Ok(()),
            // The peer paused/resumed (docs/SPEC.md §13): hold or drain our own
            // outbound queue accordingly and surface it non-blockingly.
            Message::Pause => {
                self.on_peer_pause();
                Ok(())
            }
            Message::Resume => self.on_peer_resume(),
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
        self.reporter.connected();
        // Structured `connected` event with the peer identity known at connect
        // time (the control channel's session-state feed).
        let (name, addr) = self
            .peer_identity
            .as_ref()
            .map_or((None, None), |p| (p.name.clone(), p.addr.clone()));
        self.reporter
            .emit_connected(name.as_deref(), addr.as_deref());
        // Ship our full index for reconciliation now that the peer is known good.
        let index_snapshot: Index = self.engine.index().clone();
        self.send(&Message::IndexExchange(index_snapshot))?;
        // If we (re)connected while paused, re-announce it so the fresh peer
        // holds its outbound queue instead of shipping into our not-applying
        // inbound path (a pause survives a reconnect; docs/SPEC.md §13).
        if self.is_paused() {
            self.send(&Message::Pause)?;
        }
        Ok(())
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
    fn reconcile(&mut self, peer: &Index) {
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
        // Queue the reconcile backlog rather than shipping it inline: a first-ever
        // seed can be thousands of files, and a monolithic send loop blocks the
        // pump thread — on a pipe transport, backpressured for the entire seed —
        // so a live edit made meanwhile is stuck behind the bulk (invariant #3,
        // bug B3). `pump_outbound` drains this within the bytes-in-flight window,
        // interleaving the priority lane. De-dup against the undrained backlog so
        // a re-reconcile (reconnect/resume) does not double-queue a path.
        for change in to_send {
            self.enqueue_bulk_send(change);
        }
    }

    /// Enqueue a bulk (reconcile-originated) change for windowed shipment,
    /// skipping a path already queued. The staleness of a queued change is not a
    /// concern: when it is finally drained, [`Session::prepare_send`] re-reads the
    /// file and drops it if the bytes no longer match (a live edit superseded it).
    fn enqueue_bulk_send(&mut self, change: RemoteChange) {
        if self.pending_sends_paths.insert(change.path.clone()) {
            self.pending_sends.push_back(change);
        }
    }

    /// If a bulk reconcile change for `path` is still queued, ship it NOW, before
    /// an inbound change for the same path is applied.
    ///
    /// Phase 1 drains the bulk backlog interleaved with inbound applies. Without
    /// this, an inbound conflicting change would overwrite `path` on disk before
    /// its queued outbound frame drained — [`prepare_send`] would then re-read the
    /// overwritten content and drop the send, so the peer never learns our
    /// version and the two replicas record different conflicts and diverge (the
    /// old synchronous reconcile shipped the whole snapshot before applying
    /// anything). Flushing the one queued path first restores that ordering per
    /// path and keeps conflict resolution symmetric on both sides (invariant #5).
    fn flush_queued_send(&mut self, path: &RelPath) -> Result<(), CliError> {
        if !self.pending_sends_paths.remove(path) {
            return Ok(());
        }
        let Some(pos) = self.pending_sends.iter().position(|c| &c.path == path) else {
            return Ok(());
        };
        let Some(change) = self.pending_sends.remove(pos) else {
            return Ok(());
        };
        if let Some(msg) = self.prepare_send(change) {
            self.send(&msg)?;
        }
        Ok(())
    }

    /// Whether a change for `path` may cross the sync boundary in `flow`, per the
    /// LOCAL config's class + direction ([`crate::crossing::decide`]) — enforced
    /// on receive as well as send. On a `Drop` it emits at most ONE dim note per
    /// top-level path prefix (so an ignored `.git` tree does not spam a line per
    /// file) and returns `false`; the caller then skips the change entirely —
    /// never applying, absorbing, versioning, or shipping it.
    ///
    /// This is what keeps two independent git repos isolated even when a peer on
    /// an older binary still pushes `.git`, or a stale pre-upgrade index head is
    /// re-shipped during reconcile.
    fn allow_crossing(&mut self, path: &RelPath, flow: crate::crossing::Flow) -> bool {
        let c = self.config.classify(path.as_str());
        match crate::crossing::decide(c.class, c.direction, flow) {
            crate::crossing::Crossing::Ship | crate::crossing::Crossing::Apply => true,
            crate::crossing::Crossing::Drop => {
                let key = crate::crossing::note_prefix(path.as_str()).to_owned();
                if self.noted_ignored.insert(key.clone()) {
                    let verb = match flow {
                        crate::crossing::Flow::Inbound => "not applying incoming",
                        crate::crossing::Flow::Outbound => "not shipping",
                    };
                    self.reporter.note(&format!(
                        "{verb} {key} — excluded by local config (ignore/direction rule); \
                         not synced"
                    ));
                }
                false
            }
        }
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
                adopted,
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
                    adopted: *adopted,
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
                    let adopted = capture.adopted;
                    let winner_is_local = capture.winner_is_local;
                    // The recorded conflict's id makes the surfaced line
                    // actionable (UX-V2 §4.1) and lets the control channel's
                    // `conflict` event match `tomo conflicts list`. `None` only
                    // in the rare byte-unobtainable case (no row to resolve).
                    let id = self.record_conflict_capture(capture)?.map(|cid| cid.0);
                    if self.conflicts.insert(path.clone()) {
                        self.status_dirty = true;
                    }
                    let peer_name = self.peer_identity.as_ref().and_then(|p| p.name.as_deref());
                    // Structured `conflict` event for the control channel.
                    self.reporter
                        .emit_conflict(id, path.as_str(), winner_is_local, adopted);
                    if adopted {
                        // Genesis first sync: word it as an intentional adoption
                        // of the more recently modified copy, not a mid-session
                        // clash (the loser is still preserved in history).
                        self.reporter
                            .conflict_adopted(path.as_str(), id, winner_is_local);
                    } else {
                        // The deterministic winner is already decided by the
                        // engine; surface it non-blockingly with the ready-to-run
                        // command that adopts the preserved loser instead.
                        self.reporter
                            .conflict(path.as_str(), id, winner_is_local, peer_name);
                    }
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
    ///
    /// Returns the new conflict row's id (so the actionable conflict line and
    /// the control channel's `conflict` event carry an id that matches
    /// `tomo conflicts list`), or `None` when a head's bytes were unobtainable
    /// and no row could be written.
    fn record_conflict_capture(
        &mut self,
        capture: &ConflictCapture,
    ) -> Result<Option<ConflictId>, CliError> {
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
        if let (Some(winner), Some(loser)) = (winner_id, loser_id) {
            let id = self
                .history
                .record_conflict(path, winner, loser, now_unix_ms())?;
            self.conflicts_recorded += 1;
            self.status_dirty = true;
            Ok(Some(id))
        } else {
            self.reporter.error(&format!(
                "history: could not fully preserve the conflict on {path} (a version's \
                 bytes were unavailable); sync is unaffected"
            ));
            Ok(None)
        }
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
    ///
    /// Content below [`INLINE_THRESHOLD`] rides inline in a [`Message::Change`];
    /// larger content ships as a [`Message::ChangeManifest`] whose chunks the
    /// peer then pulls (docs/SPEC.md §8).
    fn do_send(&mut self, change: RemoteChange) -> Result<(), CliError> {
        // The priority lane: a live change ships immediately, jumping ahead of any
        // bulk backlog still draining from `pending_sends` (invariant #3).
        if let Some(msg) = self.prepare_send(change) {
            self.send(&msg)?;
        }
        Ok(())
    }

    /// Resolve a change into the exact frame to put on the wire — re-reading the
    /// file so we ship the latest bytes — or `None` when it should be dropped
    /// (offline/paused/ignored/gone-stale). The single choke point for *all*
    /// outbound content; the caller decides whether to ship it now ([`do_send`],
    /// the live path) or coalesce it into a batched bulk write
    /// ([`Session::ship_bulk_subbatch`]).
    ///
    /// Content below [`INLINE_THRESHOLD`] rides inline in a [`Message::Change`];
    /// larger content becomes a [`Message::ChangeManifest`] whose chunks the peer
    /// pulls (docs/SPEC.md §8) — the manifest's ranges are recorded here so a
    /// later [`Message::ChunkRequest`] can be served regardless of when the frame
    /// actually goes out.
    fn prepare_send(&mut self, change: RemoteChange) -> Option<Message> {
        // Watch-only / offline / pre-handshake: nothing to ship.
        self.transport.as_ref()?;
        if self.outbound_suspended() {
            // Paused (or the peer paused): hold the change. It is already in the
            // engine/index and history; the resume-time reconcile re-ships it
            // (the offline-queue model, invariant #5 — nothing is lost).
            return None;
        }
        // Egress filter — the single choke point for ALL outbound changes,
        // including the reconcile head-shipping loop. An ignored / pull-only path
        // is never shipped, so a stale pre-upgrade `.git` index head that survived
        // into this session goes inert instead of re-contaminating the peer.
        if !self.allow_crossing(&change.path, crate::crossing::Flow::Outbound) {
            return None;
        }
        match change.kind {
            ChangeKind::Modified(sig) => {
                let full = join(self.layout.root(), &change.path);
                let current = std::fs::read(&full).ok();
                if !should_send(current.as_deref(), &sig) {
                    // The file changed again (or vanished); the watcher's
                    // follow-up event ships the newer state. Drop this one.
                    return None;
                }
                // `should_send` guaranteed `Some`; default is unreachable.
                let bytes = current.unwrap_or_default();
                let path = change.path.as_str().to_owned();
                let size = bytes.len() as u64;
                let msg = if bytes.len() >= INLINE_THRESHOLD {
                    self.build_manifest(change, &bytes)
                } else {
                    Message::Change {
                        change,
                        bytes: Some(bytes),
                    }
                };
                self.reporter.sent(&path, size);
                self.note_sync();
                Some(msg)
            }
            ChangeKind::Removed => Some(Message::Change {
                change,
                bytes: None,
            }),
        }
    }

    /// Build a large `Modified` change's chunk-manifest frame and remember which
    /// chunk hashes belong to this path so a later [`Message::ChunkRequest`] can
    /// be served by re-reading and re-chunking the current file (no chunk bytes
    /// are retained — invariant #3 keeps the sender stateless). Recording the
    /// ranges is a side effect that must happen when the manifest is *built*, not
    /// when it is written, so a request that races the batched write is still
    /// serviceable.
    fn build_manifest(&mut self, change: RemoteChange, bytes: &[u8]) -> Message {
        let chunks = tomo_history::chunk_bytes(bytes);
        let manifest: Vec<ChunkHash> = chunks.iter().map(|(h, _)| h.0).collect();
        let ranges: Vec<(ChunkHash, Range<usize>)> =
            chunks.into_iter().map(|(h, r)| (h.0, r)).collect();
        self.outbound_manifests.insert(change.path.clone(), ranges);
        let total_size = bytes.len() as u64;
        Message::ChangeManifest {
            change,
            total_size,
            manifest,
        }
    }

    /// Case-collision ingress guard (macOS↔Linux filename semantics, edge 3a).
    ///
    /// On a **case-insensitive** local filesystem, an inbound `Modified` change
    /// for path `P` would silently overwrite a *different* existing file `Q` when
    /// `casefold(P) == casefold(Q)` (e.g. peer ships both `Foo.txt` and
    /// `foo.txt`, distinct on Linux, the same file here). Rather than clobber
    /// `Q`, we **refuse** the apply: the incoming bytes (in hand from the frame
    /// or the CAS) are preserved in history under `P` — recoverable via
    /// `tomo log P` — the path is counted as a conflict, and a non-blocking note
    /// is emitted. Returns `Ok(true)` when a collision was handled (the caller
    /// must NOT absorb/apply the change); `Ok(false)` to proceed normally.
    ///
    /// Inert on a case-sensitive filesystem (the common Linux case), and never
    /// blocks sync (invariant #5) — the session stays connected and A is
    /// unaffected. First-writer-wins: `Q`, already present, is the keeper.
    fn case_collision_refused(
        &mut self,
        change: &RemoteChange,
        remote_bytes: Option<&[u8]>,
    ) -> Result<bool, CliError> {
        if !self.fs.case_insensitive {
            return Ok(false);
        }
        // Only a present (Modified) incoming change can collide on disk; a
        // removal names no new file, and a tombstoned existing path holds none.
        let ChangeKind::Modified(sig) = change.kind else {
            return Ok(false);
        };
        let incoming = change.path.as_str();
        let existing: Vec<String> = self
            .engine
            .index()
            .iter()
            .filter(|(_, e)| matches!(e.winner().state, EntryState::Present(_)))
            .map(|(p, _)| p.as_str().to_owned())
            .collect();
        let Some(collides_with) =
            crate::fsguard::first_collision(incoming, existing.iter().map(String::as_str))
        else {
            return Ok(false);
        };
        let collides_with = collides_with.to_owned();

        // Preserve the incoming version so nothing is lost (invariant #5).
        self.preserve_collided_incoming(&change.path, sig, &change.version, remote_bytes)?;

        if self.conflicts.insert(change.path.clone()) {
            self.status_dirty = true;
        }
        // Emitted through `note` (not `conflict`) so the full explanation reaches
        // the log verbatim — the served peer logs to serve.log, where the
        // scenario asserts this line.
        self.reporter.note(&format!(
            "\u{26a0} case collision: '{incoming}' collides with existing '{collides_with}' \
             on this filesystem — kept '{collides_with}', incoming preserved in history"
        ));
        Ok(true)
    }

    /// Record the refused-collision incoming version+bytes into history under
    /// `path` (idempotent: skipped if that exact version is already stored). The
    /// bytes come from the triggering frame when they verify, else the CAS; if
    /// neither can supply them we warn and record nothing (never wrong bytes),
    /// leaving the on-disk keeper untouched.
    fn preserve_collided_incoming(
        &mut self,
        path: &RelPath,
        sig: ContentSig,
        version: &VectorClock,
        remote_bytes: Option<&[u8]>,
    ) -> Result<(), CliError> {
        let state = EntryState::Present(sig);
        // Idempotent: a re-shipped collision (e.g. after a reconnect) must not
        // accrue duplicate history rows.
        if self
            .history
            .log(path)?
            .iter()
            .any(|m| &m.clock == version && same_state(m.state, state))
        {
            return Ok(());
        }
        let payload: Option<Vec<u8>> = match remote_bytes {
            Some(b) if matches_sig(b, &sig) => Some(b.to_vec()),
            _ => self.history.content_by_hash(&sig.hash)?,
        };
        let Some(bytes) = payload else {
            self.reporter.error(&format!(
                "case collision on '{path}': incoming bytes unavailable (frame/CAS) — \
                 refused apply, on-disk file untouched, but could not preserve to history"
            ));
            return Ok(());
        };
        // Peer-authored: attribute the preserved version to the remote replica.
        let (origin, replica) = self.attribution(false);
        self.history.record_version(
            path,
            &state,
            version,
            replica,
            origin,
            now_unix_ms(),
            Some(&bytes),
        )?;
        self.versions_recorded += 1;
        self.status_dirty = true;
        Ok(())
    }

    /// Bring the tree at `path` into line with `target`.
    fn do_apply(
        &mut self,
        path: &RelPath,
        target: Expectation,
        remote_bytes: Option<&[u8]>,
    ) -> Result<(), CliError> {
        match target {
            Expectation::Present(sig) => self.apply_present_by_sig(path, &sig, remote_bytes),
            Expectation::Absent => self.apply_absent_guarded(path),
        }
    }

    /// Apply an "absent" (deleted) state, guarding the two Item-B/A hazards:
    ///
    /// - **Symlink-escape** (Item A): [`apply_absent`] refuses to remove
    ///   *through* a symlinked parent, returning [`CliError::Refused`] — caught
    ///   here and downgraded to a note + rescan (never fatal, invariant #5).
    /// - **File→dir replacement** (Item B, docs/SPEC.md §5.4): the sender deleted
    ///   a *file*, but this path is now a *directory* locally (a type flip we
    ///   have not observed yet). A file-removal must NEVER `rm -r` a directory,
    ///   so we keep it, note, and rescan — the rescan re-derives the local
    ///   directory's children and converges without data loss.
    fn apply_absent_guarded(&mut self, path: &RelPath) -> Result<(), CliError> {
        let full = join(self.layout.root(), path);
        if path_is_dir(&full) {
            self.reporter.note(&format!(
                "not deleting {path}: it is now a directory locally (a file-removal never \
                 removes a directory); scheduling rescan"
            ));
            self.rescan_pending = true;
            return Ok(());
        }
        match apply_absent(self.layout.root(), path) {
            Ok(()) => {
                self.reporter.removed(path.as_str());
                self.note_sync();
                self.cache_note_absent(path);
                Ok(())
            }
            Err(CliError::Refused(msg)) => {
                self.note_apply_refusal(&msg);
                Ok(())
            }
            Err(other) => Err(other),
        }
    }

    /// Report a non-fatal applier refusal and schedule a reconciling rescan
    /// (invariant #5: a refusal never ends the session).
    fn note_apply_refusal(&mut self, msg: &str) {
        self.reporter.error(msg);
        self.rescan_pending = true;
    }

    /// The current on-disk state of `path` as an [`Expectation`]
    /// (`Present(sig)` / `Absent`), hashing the file if present.
    ///
    /// # Errors
    /// [`CliError`] if the file exists but cannot be read/hashed.
    fn disk_expectation(&self, path: &RelPath) -> Result<Expectation, CliError> {
        let sig = tomo_watch::snapshot(self.layout.root(), path).map_err(|e| {
            CliError::msg(format!("cannot snapshot {path} for the apply guard: {e}"))
        })?;
        Ok(sig.map_or(Expectation::Absent, Expectation::Present))
    }

    /// The disk-facing state the engine currently believes at `path` — the
    /// winner's state as an [`Expectation`], or `Absent` for a never-seen path.
    fn prior_expectation(&self, path: &RelPath) -> Expectation {
        self.engine
            .index()
            .get(path)
            .map_or(Expectation::Absent, |e| expectation_of(e.winner().state))
    }

    /// Reconcile an **unobserved concurrent local edit** at `path` into the
    /// engine *before* a remote change for the same path is absorbed, so the
    /// incoming apply can never silently clobber it (docs/NOTES.md "Storm
    /// cluster" item 3; invariant #5 — nothing is lost).
    ///
    /// The race: on a parted/frozen link this replica's own watcher event for a
    /// local write may not be dequeued before the peer's frame is processed. If
    /// disk differs from what the engine believes (`prior`) and is not our own
    /// pending echo, we feed the observed disk state as an `Event::Local` **now**
    /// — creating a head stamped *concurrent* to the incoming remote head (it is
    /// ticked from the pre-absorb clock). The caller then absorbs the remote
    /// normally, and the ordinary conflict machinery decides the deterministic
    /// winner and preserves the loser as a head (e.g. Present-beats-Tombstone for
    /// delete-vs-edit) — exactly as if the watcher event had arrived first.
    ///
    /// This is preferable to preserving the local edit *after* the absorb (which
    /// would stamp it causally-after and force local-wins without a conflict
    /// row); feeding it first yields a true concurrent conflict on both sides.
    fn reconcile_unobserved_local(&mut self, path: &RelPath) -> Result<(), CliError> {
        let disk = self.disk_expectation(path)?;
        let prior = self.prior_expectation(path);
        if !applyguard::needs_local_reconcile(
            &disk,
            &prior,
            self.engine.is_expected_echo(path, &disk),
        ) {
            return Ok(());
        }
        self.reporter.note(&format!(
            "reconciled unobserved local edit on {path} before applying incoming change"
        ));
        let kind = match disk {
            Expectation::Present(sig) => ChangeKind::Modified(sig),
            Expectation::Absent => ChangeKind::Removed,
        };
        let actions = self.engine.handle(Event::Local(LocalChange {
            path: path.clone(),
            kind,
        }));
        self.mark_dirty();
        // Local-event actions carry no Apply, so this never recurses.
        self.execute(actions, None, None)
    }

    /// Materialize `sig` at `path`, sourcing the bytes by signature rather than
    /// blindly trusting the triggering frame (docs/NOTES.md — the multi-head
    /// apply fix). Preference order, exactly [`chunkxfer::byte_source`]'s a/b/c/d:
    /// (a) the triggering frame's bytes when they hash to `sig`; (b) the current
    /// disk content when it already matches (skip the write — idempotent);
    /// (c) the content-addressed history store, which holds the chunks; else
    /// (d) warn and reconcile via a rescan instead of writing wrong content.
    ///
    /// This is what lets a frame whose bytes belong to one conflict head still
    /// drive an Apply whose target is a *different* concurrent head, instead of
    /// the old blind refuse-and-rescan.
    fn apply_present_by_sig(
        &mut self,
        path: &RelPath,
        sig: &ContentSig,
        remote_bytes: Option<&[u8]>,
    ) -> Result<(), CliError> {
        // Item B (docs/SPEC.md §5.4): resolve a file↔dir type collision before
        // writing. `false` means the apply is SKIPPED non-fatally (the directory
        // won; the follow-up rescan converges the file head to a tombstone).
        if !self.prepare_present_target(path, sig, remote_bytes)? {
            return Ok(());
        }

        let frame_matches = remote_bytes.is_some_and(|b| matches_sig(b, sig));
        let disk_matches = !frame_matches && self.read_verified(path, sig).is_some();
        // Only probe the CAS when neither cheaper source matches.
        let cas_bytes = if frame_matches || disk_matches {
            None
        } else {
            self.history.content_by_hash(&sig.hash)?
        };

        match chunkxfer::byte_source(frame_matches, disk_matches, cas_bytes.is_some()) {
            ByteSource::Frame => {
                let bytes = remote_bytes.unwrap_or_default();
                self.write_present(path, sig, bytes)?;
            }
            ByteSource::DiskSkip => {
                // The file already holds this exact content. Only the executable
                // bit can still differ (a chmod-only change whose bytes match
                // disk, or a multi-head apply landing on already-correct bytes),
                // so enforce the mode without rewriting the file — the sig's
                // exec bit is authoritative (git's model).
                set_exec_mode(self.layout.root(), path, sig.exec)?;
                self.reporter.applied(path.as_str(), sig.size);
                self.note_sync();
                self.cache_note_present(path, *sig);
            }
            ByteSource::Cas => {
                let bytes = cas_bytes.unwrap_or_default();
                self.write_present(path, sig, &bytes)?;
            }
            ByteSource::Unavailable => {
                // Neither the frame, disk, nor history can supply these bytes
                // (a raced/corrupt frame, or a head whose content hasn't landed
                // yet). Never write wrong content and never kill the session —
                // schedule a reconciling rescan; follow-up frames converge us.
                self.reporter.error(&format!(
                    "cannot materialize {path}: content unavailable from frame, disk, or \
                     history; scheduling rescan"
                ));
                self.rescan_pending = true;
            }
        }
        Ok(())
    }

    /// Write `bytes` for a present file, catching the applier's non-fatal
    /// [`CliError::Refused`] (a symlink-escape guard trip — Item A) and turning
    /// it into a note + rescan instead of tearing the session down (invariant
    /// #5). A genuine I/O or integrity error still propagates.
    fn write_present(
        &mut self,
        path: &RelPath,
        sig: &ContentSig,
        bytes: &[u8],
    ) -> Result<(), CliError> {
        match apply_present(self.layout.root(), &self.layout.staging(), path, sig, bytes) {
            Ok(()) => {
                self.reporter.applied(path.as_str(), sig.size);
                self.note_sync();
                self.cache_note_present(path, *sig);
                Ok(())
            }
            Err(CliError::Refused(msg)) => {
                self.note_apply_refusal(&msg);
                Ok(())
            }
            // Disk full: the atomic write cleaned up its temp, so nothing partial
            // is visible at the final path (invariant #8). Stall loudly rather
            // than die (invariant #5); the retry re-requests once space is freed.
            Err(other) if is_disk_full(&other) => {
                self.note_disk_full(&format!("applying {path}"));
                Ok(())
            }
            Err(other) => Err(other),
        }
    }

    // ---- Item B: file↔dir type-collision resolution (docs/SPEC.md §5.4) ----

    /// Resolve a file↔dir type collision at `path` before writing the incoming
    /// file. Returns `true` when the caller should proceed to write, `false`
    /// when the apply must be **skipped** non-fatally (the directory won).
    ///
    /// The deterministic rule (docs/SPEC.md §5.4): **the directory always wins**.
    /// A directory is the implicit container of one or more *present* synced
    /// descendants — real data that cannot be dropped — whereas the colliding
    /// file's bytes are preserved to history and its head converges to a
    /// tombstone via the follow-up rescan. "Has a present descendant" is a pure
    /// function of the (converged) index, so both replicas reach the identical
    /// outcome without negotiation.
    ///
    /// Two shapes of collision, mirror images of each other:
    /// - **Target is a directory** (Item B: the file is a descendant elsewhere,
    ///   e.g. we are replica B whose `foo/` holds `foo/x` while the peer ships a
    ///   file `foo`): keep the directory, preserve the incoming file version to
    ///   history, skip. The rescan emits `Removed(foo)` → the file head becomes a
    ///   tombstone on both sides.
    /// - **A parent component is a file** (e.g. we are replica A applying `foo/x`
    ///   while `foo` is still a file locally): the directory `foo/` must exist to
    ///   hold the synced child, so clear the obstructing file (preserving its
    ///   bytes to history first) and proceed. The rescan emits `Removed(foo)`.
    fn prepare_present_target(
        &mut self,
        path: &RelPath,
        sig: &ContentSig,
        remote_bytes: Option<&[u8]>,
    ) -> Result<bool, CliError> {
        let full = join(self.layout.root(), path);
        match type_collision(self.layout.root(), &full) {
            None => Ok(true),
            Some(TypeCollision::TargetIsDir) => {
                self.preserve_incoming_file(path, sig, remote_bytes)?;
                self.reporter.note(&format!(
                    "kept the directory at {path}: an incoming file collides with a local \
                     directory (directory wins, §5.4); the file version is preserved in history"
                ));
                self.rescan_pending = true;
                Ok(false)
            }
            Some(TypeCollision::ParentIsFile { ancestor }) => {
                if self.preserve_and_clear_obstructing_file(&ancestor)? {
                    // The obstruction is gone and its bytes are in history; the
                    // rescan will tombstone that file head. Proceed to write.
                    self.rescan_pending = true;
                    Ok(true)
                } else {
                    // Could not preserve its bytes (raced/untracked) — refuse
                    // rather than destroy (invariant #5). The rescan self-heals
                    // once the peer's Removed for the parent arrives.
                    self.reporter.error(&format!(
                        "refused to replace file '{}' with a directory needed by {path}: \
                         could not preserve its bytes to history; scheduling rescan",
                        ancestor.display()
                    ));
                    self.rescan_pending = true;
                    Ok(false)
                }
            }
        }
    }

    /// Preserve the incoming (losing) file version to history when the directory
    /// wins a `TargetIsDir` collision, so the file's bytes remain retrievable
    /// (invariant #5). Best-effort: sources bytes from the triggering frame or
    /// the CAS, and the clock from the engine's matching head; if either is
    /// unobtainable it notes and moves on (sync is unaffected).
    fn preserve_incoming_file(
        &mut self,
        path: &RelPath,
        sig: &ContentSig,
        remote_bytes: Option<&[u8]>,
    ) -> Result<(), CliError> {
        let bytes = if remote_bytes.is_some_and(|b| matches_sig(b, sig)) {
            remote_bytes.map(<[u8]>::to_vec)
        } else {
            self.history.content_by_hash(&sig.hash)?
        };
        let (Some(bytes), Some(clock)) = (bytes, self.head_clock_for(path, sig)) else {
            self.reporter.note(&format!(
                "history: could not preserve the colliding file version of {path} \
                 (bytes or clock unavailable); sync is unaffected"
            ));
            return Ok(());
        };
        self.record_present_if_absent(path, sig, &clock, &bytes, false)
    }

    /// Preserve an obstructing parent file's bytes to history, then remove it so
    /// the directory a synced child needs can be created. Returns `true` on
    /// success (bytes preserved, file gone), `false` if its bytes/clock could
    /// not be obtained — in which case the caller refuses rather than deletes.
    fn preserve_and_clear_obstructing_file(&mut self, ancestor: &Path) -> Result<bool, CliError> {
        let Some(anc_rel) = self.rel_under_root(ancestor) else {
            return Ok(false);
        };
        // Re-read + hash the file so we preserve exactly what is on disk.
        let sig = match tomo_watch::snapshot(self.layout.root(), &anc_rel) {
            Ok(Some(sig)) => sig,
            // Already gone (raced) — nothing to preserve; the directory is clear.
            Ok(None) => return Ok(true),
            Err(_) => return Ok(false),
        };
        let bytes = match std::fs::read(ancestor) {
            Ok(bytes) if matches_sig(&bytes, &sig) => bytes,
            // Vanished or changed under us: do not guess. Let the rescan heal.
            _ => return Ok(false),
        };
        let Some(clock) = self.head_clock_for(&anc_rel, &sig).or_else(|| {
            self.engine
                .index()
                .get(&anc_rel)
                .map(|e| e.winner().version.clone())
        }) else {
            // Untracked obstruction (not in the index): refuse rather than
            // destroy something we cannot version.
            return Ok(false);
        };
        self.record_present_if_absent(&anc_rel, &sig, &clock, &bytes, true)?;
        std::fs::remove_file(ancestor)
            .map_err(|s| CliError::io("remove obstructing file", ancestor, s))?;
        self.reporter.note(&format!(
            "cleared file '{}' to make way for a directory (its version is preserved in history)",
            ancestor.display()
        ));
        Ok(true)
    }

    /// Record a present version of `path` at `clock` unless history already
    /// holds it (dedup by clock + present-state), attributing per `origin_local`.
    fn record_present_if_absent(
        &mut self,
        path: &RelPath,
        sig: &ContentSig,
        clock: &VectorClock,
        bytes: &[u8],
        origin_local: bool,
    ) -> Result<(), CliError> {
        let already = self
            .history
            .log(path)?
            .iter()
            .any(|m| &m.clock == clock && matches!(m.state, EntryState::Present(_)));
        if already {
            return Ok(());
        }
        let (origin, replica) = self.attribution(origin_local);
        self.history.record_version(
            path,
            &EntryState::Present(*sig),
            clock,
            replica,
            origin,
            now_unix_ms(),
            Some(bytes),
        )?;
        self.versions_recorded += 1;
        self.status_dirty = true;
        Ok(())
    }

    /// The vector clock of the engine head at `path` whose state is exactly
    /// `Present(sig)`, if any (used to attribute a preserved version to its true
    /// version rather than fabricating a new clock).
    fn head_clock_for(&self, path: &RelPath, sig: &ContentSig) -> Option<VectorClock> {
        self.engine.index().get(path).and_then(|entry| {
            entry
                .heads()
                .iter()
                .find(|h| matches!(h.state, EntryState::Present(s) if s == *sig))
                .map(|h| h.version.clone())
        })
    }

    /// Turn an absolute path under the project root into a [`RelPath`], or `None`
    /// if it escapes the root or names an untracked/exotic component.
    fn rel_under_root(&self, full: &Path) -> Option<RelPath> {
        let rel = full.strip_prefix(self.layout.root()).ok()?;
        let mut parts = Vec::new();
        for comp in rel.components() {
            match comp {
                std::path::Component::Normal(os) => parts.push(os.to_str()?),
                _ => return None,
            }
        }
        if parts.is_empty() {
            return None;
        }
        RelPath::new(&parts.join("/")).ok()
    }

    // ---- Chunked, interleaved content transfer (docs/SPEC.md §8) ----------

    /// Wipe every stale entry under `.tomo/staging/` at startup — received chunk
    /// files (`chunks/`) *and* any loose atomic-write temp (an interrupted
    /// index/status persist or file apply, e.g. `<name>.tmp`).
    ///
    /// Everything in staging is scratch: real tree files only ever appear at
    /// their final path via atomic rename (invariant #8), and in-progress
    /// assemblies are pure memory that never survive a restart. So anything here
    /// at boot is garbage from a previous, now-dead session — and the
    /// single-session lock (held before we get here) guarantees no *other* live
    /// session is using staging concurrently, making the wipe unconditionally
    /// safe. Clearing it keeps a quiescent `.tomo/staging/` empty (the scenarios'
    /// convergence invariant) even after a peer's serve child was `kill`ed
    /// mid-persist. The directory itself is kept; its contents are recreated
    /// lazily as transfers/persists need them.
    fn reset_staging(&self) -> Result<(), CliError> {
        let dir = self.layout.staging();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(CliError::io("read staging directory", &dir, e)),
        };
        for entry in entries {
            let path = entry
                .map_err(|s| CliError::io("read staging entry", &dir, s))?
                .path();
            let removed = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            removed.map_err(|s| CliError::io("clear stale staging entry", &path, s))?;
        }
        Ok(())
    }

    /// Remove the chunk staging directory once no assembly needs it, so it does
    /// not linger as (empty) leftover under `.tomo/staging/`. `remove_dir` only
    /// succeeds on an empty directory, so an in-flight transfer keeps it.
    fn prune_chunk_staging(&self) {
        if self.assemblies.is_empty() {
            let _ = std::fs::remove_dir(self.layout.chunks());
        }
    }

    /// Serve a peer's chunk request: for each announced path holding any wanted
    /// hash, re-read and re-chunk the *current* file and queue its matching
    /// chunks. Hashes the current file no longer contains are silently skipped —
    /// the file changed and a fresh manifest is already on the way (invariant #3).
    fn on_chunk_request(&mut self, hashes: &[ChunkHash]) {
        let mut want: HashSet<ChunkHash> = hashes.iter().copied().collect();
        if want.is_empty() {
            return;
        }
        let paths: Vec<RelPath> = self
            .outbound_manifests
            .iter()
            .filter(|(_, ranges)| ranges.iter().any(|(h, _)| want.contains(h)))
            .map(|(p, _)| p.clone())
            .collect();
        for path in paths {
            if want.is_empty() {
                break;
            }
            // Clone the tiny range table so the file read below borrows nothing.
            let Some(ranges) = self.outbound_manifests.get(&path).cloned() else {
                continue;
            };
            let full = join(self.layout.root(), &path);
            let Ok(mut file) = std::fs::File::open(&full) else {
                continue; // file gone/unreadable — skip; a fresh change is coming.
            };
            for (hash, range) in &ranges {
                if !want.contains(hash) {
                    continue;
                }
                // `pread` exactly this chunk's bytes and re-verify: if the file
                // changed, the range's content no longer hashes to `hash` and we
                // skip it (invariant #3 — a fresh manifest is already coming).
                let mut buf = vec![0u8; range.len()];
                if file.seek(SeekFrom::Start(range.start as u64)).is_err()
                    || file.read_exact(&mut buf).is_err()
                {
                    break; // file shrank/unreadable — stop serving it.
                }
                if blake3::hash(&buf).as_bytes() == hash {
                    want.remove(hash);
                    self.pending_chunks.push_back(Message::ChunkData {
                        hash: *hash,
                        bytes: buf,
                    });
                }
            }
        }
    }

    /// Begin an inbound large-file assembly. The change is recorded but **not**
    /// absorbed into the engine yet — it is absorbed and applied atomically once
    /// every chunk has arrived and the whole reassembles to `sig` (see
    /// [`Assembly`] for why deferring the absorb is a crash-safety requirement).
    fn on_change_manifest(
        &mut self,
        change: RemoteChange,
        total_size: u64,
        manifest: Vec<ChunkHash>,
    ) -> Result<(), CliError> {
        let path = change.path.clone();
        // Ingress filter (same as the inline Change path): refuse an ignored /
        // wrong-direction path before starting any assembly or requesting chunks.
        if !self.allow_crossing(&path, crate::crossing::Flow::Inbound) {
            return Ok(());
        }
        // Ship our own still-queued reconcile frame for this path first, so the
        // peer sees our version before this assembly completes and overwrites it
        // (invariant #5; see `flush_queued_send`).
        self.flush_queued_send(&path)?;
        let sig = match &change.kind {
            ChangeKind::Modified(sig) => *sig,
            // A manifest only ever describes Modified content; ignore otherwise.
            ChangeKind::Removed => return Ok(()),
        };
        // A newer manifest for the same path supersedes an in-flight assembly.
        self.abandon_superseded(&path);
        self.assemblies.insert(
            path.clone(),
            Assembly {
                change,
                sig,
                manifest_set: manifest.iter().copied().collect(),
                manifest,
                have: HashSet::new(),
                requested: HashSet::new(),
                total_size,
                received_bytes: 0,
            },
        );
        // Degenerate empty manifest completes at once; otherwise pull chunks.
        if self
            .assemblies
            .get(&path)
            .is_some_and(|a| chunkxfer::is_complete(&a.manifest, &a.have))
        {
            self.complete_assembly(&path)
        } else {
            self.request_next_batch(&path)
        }
    }

    /// Request the next batch of missing, not-yet-requested chunks for `path`.
    fn request_next_batch(&mut self, path: &RelPath) -> Result<(), CliError> {
        let batch = {
            let Some(a) = self.assemblies.get(path) else {
                return Ok(());
            };
            chunkxfer::next_request_batch(&a.manifest, &a.have, &a.requested)
        };
        if batch.is_empty() {
            return Ok(());
        }
        if let Some(a) = self.assemblies.get_mut(path) {
            for h in &batch {
                a.requested.insert(*h);
            }
        }
        self.send(&Message::ChunkRequest { hashes: batch })
    }

    /// Store one received chunk (verifying its content addressing), then request
    /// the next batch or complete the assembly.
    fn on_chunk_data(&mut self, hash: ChunkHash, bytes: &[u8]) -> Result<(), CliError> {
        let Some(path) = self
            .assemblies
            .iter()
            .find(|(_, a)| !a.have.contains(&hash) && a.manifest_set.contains(&hash))
            .map(|(p, _)| p.clone())
        else {
            return Ok(()); // unsolicited or duplicate — ignore.
        };
        if blake3::hash(bytes).as_bytes() != &hash {
            self.reporter
                .error("received chunk failed its hash check; will re-request");
            if let Some(a) = self.assemblies.get_mut(&path) {
                a.requested.remove(&hash);
            }
            return self.request_next_batch(&path);
        }
        if let Err(e) = self.write_chunk_file(&hash, bytes) {
            // Disk full while staging a chunk: abandon this assembly (freeing its
            // partial chunk files), stall loudly, and let the retry re-request the
            // whole file once space is freed. The change was never absorbed into
            // the index (absorb happens only at completion), so there is no
            // phantom "present" head and nothing partial at the final path
            // (invariants #5/#8). A non-ENOSPC error is still fatal.
            if is_disk_full(&e) {
                self.abandon_assembly(&path);
                self.note_disk_full(&format!("receiving {path}"));
                return Ok(());
            }
            return Err(e);
        }
        if let Some(a) = self.assemblies.get_mut(&path) {
            a.have.insert(hash);
            a.received_bytes = a.received_bytes.saturating_add(bytes.len() as u64);
        }
        if self
            .assemblies
            .get(&path)
            .is_some_and(|a| chunkxfer::is_complete(&a.manifest, &a.have))
        {
            self.complete_assembly(&path)
        } else {
            // Redraw the transient progress line (styled tty only; a no-op
            // otherwise) as the transfer advances.
            if let Some((got, total)) = self
                .assemblies
                .get(&path)
                .map(|a| (a.received_bytes, a.total_size))
            {
                self.reporter.progress(path.as_str(), got, total);
            }
            self.request_next_batch(&path)
        }
    }

    /// Reassemble a completed assembly, verify the whole-file hash, then absorb
    /// the change and apply it atomically — the assembled bytes stand in for a
    /// large inline frame, so `Apply`, `RecordVersion`, and conflict capture all
    /// work exactly as for an inline `Change`. A verification failure abandons
    /// the assembly and schedules a rescan rather than writing corrupt content.
    fn complete_assembly(&mut self, path: &RelPath) -> Result<(), CliError> {
        let Some(asm) = self.assemblies.remove(path) else {
            return Ok(());
        };
        let mut bytes = Vec::with_capacity(usize::try_from(asm.total_size).unwrap_or(0));
        let mut intact = true;
        for h in &asm.manifest {
            let Some(chunk) = self.read_chunk_file(h)? else {
                intact = false;
                break;
            };
            bytes.extend_from_slice(&chunk);
        }
        if !intact || !matches_sig(&bytes, &asm.sig) {
            self.reporter.error(&format!(
                "assembled content for {path} failed whole-file verification; re-syncing"
            ));
            self.clean_assembly_chunks(&asm);
            self.prune_chunk_staging();
            self.rescan_pending = true;
            return Ok(());
        }
        self.clean_assembly_chunks(&asm);
        self.prune_chunk_staging();
        // Case-collision guard (parity with the inline `Change` path): on a
        // case-insensitive FS, refuse a large inbound file that case-folds onto
        // a different existing file, preserving the assembled bytes to history.
        if self.case_collision_refused(&asm.change, Some(&bytes))? {
            return Ok(());
        }
        // Reconcile any local edit made to this path during assembly BEFORE the
        // absorb, so it becomes a concurrent head rather than being clobbered
        // (invariant #5) — the same guard as the inline path.
        self.reconcile_unobserved_local(&asm.change.path)?;
        // Absorb + apply atomically now (the bytes exist), never earlier.
        let remote_version = asm.change.version.clone();
        let actions = self.engine.handle(Event::Remote(asm.change));
        self.mark_dirty();
        self.execute(actions, Some(&bytes), Some(&remote_version))
    }

    /// Abandon any in-flight assembly superseded by an incoming change for the
    /// same path (invariant #3), discarding its partial chunk files.
    fn abandon_superseded(&mut self, incoming: &RelPath) {
        let victims: Vec<RelPath> = self
            .assemblies
            .keys()
            .filter(|p| chunkxfer::supersedes(p, incoming))
            .cloned()
            .collect();
        for p in victims {
            if let Some(a) = self.assemblies.remove(&p) {
                self.clean_assembly_chunks(&a);
            }
        }
        self.prune_chunk_staging();
    }

    /// Abandon a single in-flight assembly (e.g. its chunk staging hit a full
    /// disk), discarding its received chunk files so the space is reclaimed.
    fn abandon_assembly(&mut self, path: &RelPath) {
        if let Some(a) = self.assemblies.remove(path) {
            self.clean_assembly_chunks(&a);
        }
        self.prune_chunk_staging();
    }

    /// Abandon every in-flight assembly (on going offline), discarding chunks.
    fn abandon_all_assemblies(&mut self) {
        let all: Vec<Assembly> = self.assemblies.drain().map(|(_, a)| a).collect();
        for a in &all {
            self.clean_assembly_chunks(a);
        }
        self.prune_chunk_staging();
    }

    /// Delete an abandoned/completed assembly's received chunk files, keeping any
    /// chunk another live assembly still needs (content-addressed dedup).
    fn clean_assembly_chunks(&self, asm: &Assembly) {
        for h in &asm.have {
            let still_needed = self
                .assemblies
                .values()
                .any(|other| other.manifest_set.contains(h));
            if !still_needed {
                let _ = std::fs::remove_file(self.chunk_path(h));
            }
        }
    }

    /// The staging path a received chunk's bytes live at, named by its hash.
    fn chunk_path(&self, hash: &ChunkHash) -> PathBuf {
        self.layout.chunks().join(hex32(hash))
    }

    /// Write a verified chunk's bytes to its staging file.
    ///
    /// Deliberately *not* fsynced (unlike a final tree write): chunk files are
    /// transient — wiped at startup, re-requestable, and BLAKE3-verified on read
    /// — so per-chunk durability buys nothing and an fsync per chunk would make
    /// a bulk transfer's chunk burst block interleaved live changes behind
    /// hundreds of syncs. A crash simply discards the whole assembly (invariant
    /// #8 is about the *final* path, which is still written atomically on apply).
    fn write_chunk_file(&self, hash: &ChunkHash, bytes: &[u8]) -> Result<(), CliError> {
        let dir = self.layout.chunks();
        // Created lazily (and pruned when idle) so a quiescent staging dir stays
        // empty; `create_dir_all` is a no-op once it exists.
        std::fs::create_dir_all(&dir).map_err(|s| CliError::io("create chunk staging", &dir, s))?;
        let path = dir.join(hex32(hash));
        std::fs::write(&path, bytes).map_err(|s| CliError::io("write chunk file", &path, s))
    }

    /// Read a chunk's staging file back, returning its bytes only if they still
    /// hash to `hash` (a torn/missing file yields `None` — the caller re-syncs).
    fn read_chunk_file(&self, hash: &ChunkHash) -> Result<Option<Vec<u8>>, CliError> {
        let path = self.chunk_path(hash);
        match std::fs::read(&path) {
            Ok(b) if blake3::hash(&b).as_bytes() == hash => Ok(Some(b)),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(CliError::io("read chunk file", &path, e)),
        }
    }

    // ---- Persistence ------------------------------------------------------

    fn mark_dirty(&mut self) {
        self.index_dirty = true;
        self.status_dirty = true;
    }

    /// Record that a real file sync just happened (an apply, send, or remove),
    /// so the `heartbeat` event's `last_sync_ms_ago` reflects genuine sync
    /// activity — not mere channel wakeups (pings, timeouts).
    fn note_sync(&mut self) {
        self.last_sync = Some(Instant::now());
    }

    /// Persist the index (if changed) and the status file (if changed or the
    /// idle cadence elapsed, or `force`).
    fn persist(&mut self, force: bool) -> Result<(), CliError> {
        // The index and the startup-scan cache are both reconstructible caches
        // persisted on the same throttle (or on `force` at shutdown). A
        // stale-by-≤2s on-disk copy of either costs nothing in correctness
        // (invariant #8 still holds — every write is staging + atomic rename; a
        // mismatched scan-cache entry merely forces a hash next startup).
        let persist_due = force || self.last_index_persist.elapsed() >= PERSIST_THROTTLE;
        if persist_due && (self.index_dirty || self.scan_cache_dirty) {
            if self.index_dirty {
                store_index(
                    &self.layout.staging(),
                    &self.layout.index(),
                    self.engine.index(),
                )?;
                self.index_dirty = false;
                self.status_dirty = true;
            }
            if self.scan_cache_dirty {
                store_scan_cache(
                    &self.layout.staging(),
                    &self.layout.scancache(),
                    &self.scan_cache,
                )?;
                self.scan_cache_dirty = false;
            }
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
            // Cache for the heartbeat event so it needs no extra DB query.
            self.last_unresolved = conflicts_unresolved;
            let history = HistoryStatus {
                mode: crate::histmode::label(&self.config.history.mode).to_owned(),
                versions_recorded: self.versions_recorded,
                conflicts_recorded: self.conflicts_recorded,
                staged: self.pressure.staged_len() as u64,
                rung: self.pressure.rung() as u64,
            };
            let mut status = Status::live(
                self.engine.index(),
                conflicts,
                conflicts_unresolved,
                net,
                self.connected,
                self.rescan_pending,
                Some(history),
                Some(self.fs),
            );
            status.peer.clone_from(&self.peer_identity);
            status.paused = self.is_paused();
            status.peer_paused = self.peer_paused;
            write_status(&self.layout, &status)?;
            self.last_status = Instant::now();
            self.status_dirty = false;
        }
        Ok(())
    }

    fn send(&mut self, msg: &Message) -> Result<(), CliError> {
        let result = match self.transport.as_mut() {
            Some(t) => t.tx.send(msg),
            None => return Ok(()),
        };
        match result {
            Ok(()) => Ok(()),
            // The peer went away mid-write: in a watch mode, queue changes and
            // reconnect rather than dying (invariant #3). Elsewhere it's fatal.
            Err(e) if self.reconnecting() => {
                self.go_offline(&format!("send failed: {e}"));
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

/// How many already-queued items one pump iteration drains and coalesces.
/// Bounded so an unbounded producer can never starve the loop's periodic work
/// (persist, reconnect, shutdown check); the remainder waits for the next pass.
const BURST_DRAIN_LIMIT: usize = 16384;

/// Collect `first` plus everything currently queued (up to [`BURST_DRAIN_LIMIT`])
/// without blocking, so one iteration can coalesce a whole event burst.
fn drain_burst(first: Incoming, rx: &mpsc::Receiver<Incoming>) -> Vec<Incoming> {
    let mut batch = Vec::with_capacity(8);
    batch.push(first);
    while batch.len() < BURST_DRAIN_LIMIT {
        match rx.try_recv() {
            Ok(item) => batch.push(item),
            Err(_) => break,
        }
    }
    batch
}

/// Coalesce a drained burst: drop every [`WatchSignal::Pending`] for a path that
/// a later `Pending` in the same burst supersedes, keeping only its final state
/// (invariant #3 ships the latest bytes; invariant #4 still versions the last
/// state, which is the one kept). All other items are preserved in order.
fn coalesce_burst(batch: Vec<Incoming>) -> Vec<Incoming> {
    // Index of the last Pending for each path in the burst.
    let mut last: HashMap<RelPath, usize> = HashMap::new();
    for (i, item) in batch.iter().enumerate() {
        if let Incoming::Watch(WatchSignal::Pending(p)) = item {
            last.insert(p.rel.clone(), i);
        }
    }
    if last.len() == batch.len() {
        return batch; // nothing to coalesce (fast path)
    }
    batch
        .into_iter()
        .enumerate()
        .filter_map(|(i, item)| match &item {
            Incoming::Watch(WatchSignal::Pending(p)) => {
                (last.get(&p.rel) == Some(&i)).then_some(item)
            }
            _ => Some(item),
        })
        .collect()
}

/// Wall-clock nanoseconds since the Unix epoch, for the scan cache's
/// recent-write guard **only** (never an ordering input — invariant #7; ordering
/// is always vector clocks). Must share the epoch/units of the file mtimes it is
/// compared against (`tomo_watch::sig::mtime_ns`). An unreadable clock returns
/// `u64::MAX`, which makes every file "recently modified" and thus always
/// hashed — the safe degradation.
fn wall_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_nanos()).ok())
        .unwrap_or(u64::MAX)
}

/// Whether a [`CliError`] is a "no space left on device" (ENOSPC) I/O failure —
/// the signal to stall a transfer rather than tear the session down. ENOSPC is
/// errno 28 on Linux and the BSDs/macOS alike (the two platforms Tomo targets).
fn is_disk_full(err: &CliError) -> bool {
    matches!(err, CliError::Io { source, .. } if source.raw_os_error() == Some(28))
}

/// Lowercase-hex encode a 32-byte chunk hash for its staging-file name.
fn hex32(hash: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(64);
    for b in hash {
        // Writing to a String never fails.
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// A head/winner [`EntryState`] as the [`Expectation`] the apply guard compares
/// disk against.
fn expectation_of(state: EntryState) -> Expectation {
    match state {
        EntryState::Present(sig) => Expectation::Present(sig),
        EntryState::Tombstone => Expectation::Absent,
    }
}

/// Whether a message carries file *content* (as opposed to handshake, liveness,
/// or control state). While locally paused we drop these inbound — a resume
/// re-exchanges indices and reconciles, so nothing is lost (docs/SPEC.md §13).
fn is_content_frame(msg: &Message) -> bool {
    matches!(
        msg,
        Message::Change { .. }
            | Message::ChangeManifest { .. }
            | Message::ChunkRequest { .. }
            | Message::ChunkData { .. }
    )
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
    /// Whether the engine resolved this via the genesis adoption rule (newer
    /// mtime adopted). Selects the first-sync note wording in pass 2.
    adopted: bool,
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn is_disk_full_matches_only_enospc_io_errors() {
        // ENOSPC (28) → treated as a disk-full stall signal.
        let enospc = CliError::io(
            "write",
            std::path::Path::new("/x"),
            std::io::Error::from_raw_os_error(28),
        );
        assert!(is_disk_full(&enospc));

        // A different errno (EACCES = 13) is NOT disk-full.
        let eacces = CliError::io(
            "write",
            std::path::Path::new("/x"),
            std::io::Error::from_raw_os_error(13),
        );
        assert!(!is_disk_full(&eacces));

        // A non-Io error is never disk-full.
        assert!(!is_disk_full(&CliError::msg("boom")));
    }

    #[test]
    fn content_frames_are_the_ones_dropped_while_paused() {
        // While locally paused, only file-content frames are dropped inbound;
        // the handshake, liveness, and pause/resume control frames still flow so
        // the link stays healthy (docs/SPEC.md §13).
        let sig = ContentSig {
            hash: tomo_engine::ContentHash([0; 32]),
            size: 0,
            exec: false,
            mtime_ms: 0,
        };
        let path = RelPath::new("a.txt").unwrap();
        let content = [
            Message::Change {
                change: RemoteChange {
                    path: path.clone(),
                    kind: ChangeKind::Modified(sig),
                    version: VectorClock::new(),
                },
                bytes: Some(vec![]),
            },
            Message::ChangeManifest {
                change: RemoteChange {
                    path,
                    kind: ChangeKind::Modified(sig),
                    version: VectorClock::new(),
                },
                total_size: 0,
                manifest: vec![],
            },
            Message::ChunkRequest { hashes: vec![] },
            Message::ChunkData {
                hash: [0; 32],
                bytes: vec![],
            },
        ];
        for msg in &content {
            assert!(is_content_frame(msg), "{msg:?} should be a content frame");
        }
        let control = [
            Message::Ping { nonce: 1 },
            Message::Pong { nonce: 1 },
            Message::Pause,
            Message::Resume,
            Message::IndexExchange(Index::default()),
        ];
        for msg in &control {
            assert!(
                !is_content_frame(msg),
                "{msg:?} must keep flowing while paused"
            );
        }
    }

    // ======================================================================
    // SEED-PERF H6 (chunk-assembly invariants) + H7 (echo suppression under
    // batched applies).
    //
    // ROUTE (reported in the handoff): the pure decision pieces H6 cares about
    // already live in `crate::chunkxfer` (missing/next-batch/is_complete/
    // supersedes) and are unit-tested there. The STATEFUL assembly logic
    // (chunk routing across many concurrent assemblies, superseding manifests,
    // unknown/abandoned-chunk handling) and the echo-journal integration live
    // in `Session` methods wrapped in real disk + history I/O — not purely
    // extractable without an invasive refactor. So these are covered at the
    // INTEGRATION level with a scripted frame sequence driving a real, no-peer
    // `Session` (transport `None`, so `send` is a no-op) directly through
    // `on_change_manifest` / `on_chunk_data` / `on_message` / `on_watch`. No
    // production code was extracted or changed. `session_for_test` is the only
    // new seam (a test-only constructor).
    // ======================================================================

    use tomo_engine::ContentHash;
    use tomo_watch::{PendingChange, PendingKind};

    /// A present [`ContentSig`] for `bytes` (mtime is excluded from `ContentSig`
    /// equality, so `0` is fine — it never affects echo matching or apply).
    fn test_sig(bytes: &[u8]) -> ContentSig {
        ContentSig {
            hash: ContentHash(*blake3::hash(bytes).as_bytes()),
            size: bytes.len() as u64,
            exec: false,
            mtime_ms: 0,
        }
    }

    /// A peer-authored clock ticked `n` times for replica 2.
    fn peer_clock(n: u32) -> VectorClock {
        let mut v = VectorClock::new();
        for _ in 0..n {
            v.tick(ReplicaId(2));
        }
        v
    }

    /// Draw a value in `0..bound` from an xorshift state (deterministic; no
    /// `rand`, no narrowing `as` casts — keeps clippy pedantic happy).
    fn bounded(state: &mut u64, bound: usize) -> usize {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        usize::try_from(*state % u64::try_from(bound).unwrap()).unwrap()
    }

    /// In-place Fisher–Yates shuffle seeded deterministically.
    fn shuffle<T>(v: &mut [T], seed: u64) {
        let mut state = seed | 1;
        for i in (1..v.len()).rev() {
            let j = bounded(&mut state, i + 1);
            v.swap(i, j);
        }
    }

    /// Build a minimal in-process [`Session`] rooted at `root` with NO transport
    /// (SEED-PERF H6/H7). Sends are no-ops, so the inbound assembly and echo
    /// paths exercise the real state machine plus real disk + history I/O
    /// without a live peer. Test-only; mirrors `run`'s field initialization.
    fn session_for_test(root: &Path) -> Session {
        std::fs::create_dir_all(root.join(".tomo/staging")).unwrap();
        let layout = Layout::new(root);
        let config = Config::default();
        let fs = crate::fsprobe::FsSemantics::default();
        let engine = Engine::new(ReplicaId(1), Index::new());
        // Route reporter output to a throwaway log file so tests stay quiet.
        let log = std::fs::File::create(root.join(".tomo/test.log")).unwrap();
        let reporter = Reporter::log(log);
        let history = HistoryStore::open(root).unwrap();
        let pressure = PressureController::new(
            crate::histmode::to_engine(&config.history.mode),
            PressureConfig::default(),
        );
        let (tx, _rx) = mpsc::channel::<Incoming>();
        let now = Instant::now();
        Session {
            layout,
            config,
            fs,
            engine,
            reporter,
            binary_version: "test".to_owned(),
            transport: None,
            history,
            pressure,
            peer_replica: Some(ReplicaId(2)),
            peer_identity: None,
            started: now,
            versions_recorded: 0,
            conflicts_recorded: 0,
            ssh_params: None,
            repush_requested: false,
            connected: false,
            hello_received: false,
            conflicts: BTreeSet::new(),
            noted_ignored: HashSet::new(),
            index_dirty: false,
            status_dirty: false,
            last_status: now,
            last_index_persist: now,
            rescan_pending: false,
            scan_cache: ScanCache::new(),
            scan_cache_dirty: false,
            disk_stalled: false,
            last_stall_retry: now,
            last_activity: now,
            last_sync: None,
            last_heartbeat: now,
            last_unresolved: 0,
            shutdown: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            paused_acted: false,
            peer_paused: false,
            tx,
            reconnect_plan: ReconnectPlan::None,
            offline_since: None,
            next_reconnect_at: None,
            backoff: RECONNECT_MIN,
            assemblies: HashMap::new(),
            outbound_manifests: HashMap::new(),
            pending_chunks: VecDeque::new(),
            pending_sends: VecDeque::new(),
            pending_sends_paths: HashSet::new(),
            send_window_bytes: send_window_bytes(),
            inflight: Arc::new(InFlight::new(send_window_bytes() as u64)),
        }
    }

    /// A large-file transfer as a manifest of distinct 1 KiB chunks whose
    /// concatenation is the file content. Each chunk is a constant byte so its
    /// hash is a pure function of its seed — two files sharing a seed share that
    /// exact chunk (content dedup), and distinct seeds never collide.
    struct FakeFile {
        path: RelPath,
        content: Vec<u8>,
        chunks: Vec<(ChunkHash, Vec<u8>)>,
        sig: ContentSig,
        version: VectorClock,
    }

    fn fake_file(path: &str, chunk_seeds: &[u8], version_ticks: u32) -> FakeFile {
        let mut content = Vec::new();
        let mut chunks = Vec::new();
        for &s in chunk_seeds {
            let chunk = vec![s; 1024];
            let hash = *blake3::hash(&chunk).as_bytes();
            content.extend_from_slice(&chunk);
            chunks.push((hash, chunk));
        }
        let sig = test_sig(&content);
        FakeFile {
            path: RelPath::new(path).unwrap(),
            content,
            chunks,
            sig,
            version: peer_clock(version_ticks),
        }
    }

    /// Announce `f`'s manifest to the session (begins an inbound assembly, as a
    /// `Message::ChangeManifest` would).
    fn start_assembly(s: &mut Session, f: &FakeFile) {
        let change = RemoteChange {
            path: f.path.clone(),
            kind: ChangeKind::Modified(f.sig),
            version: f.version.clone(),
        };
        let manifest: Vec<ChunkHash> = f.chunks.iter().map(|(h, _)| *h).collect();
        s.on_change_manifest(change, f.content.len() as u64, manifest)
            .unwrap();
    }

    fn on_disk(s: &Session, path: &RelPath) -> Option<Vec<u8>> {
        std::fs::read(join(s.layout.root(), path)).ok()
    }

    // ---- H6: chunk-assembly invariants at high interleave -----------------

    #[test]
    fn h6_many_concurrent_out_of_order_assemblies_each_complete_correctly() {
        // MANY assemblies in flight at once; every chunk delivered in a shuffled,
        // cross-file-interleaved order. Each file must reassemble to exactly its
        // own manifest content — never cross-contaminated by another assembly's
        // chunks (SEED-PERF H6). All chunks are distinct across files, so each
        // hash belongs to exactly one assembly and routing is unambiguous.
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_for_test(dir.path());

        let mut seed = 0u8;
        let files: Vec<FakeFile> = (0..8u32)
            .map(|i| {
                let cs = [seed, seed + 1, seed + 2];
                seed += 3;
                fake_file(&format!("dir{i}/big{i}.bin"), &cs, i + 1)
            })
            .collect();

        for f in &files {
            start_assembly(&mut s, f);
        }
        assert_eq!(s.assemblies.len(), files.len(), "all assemblies in flight");

        // Every chunk across every file, delivered in one shuffled stream.
        let mut deliveries: Vec<(ChunkHash, Vec<u8>)> = files
            .iter()
            .flat_map(|f| f.chunks.iter().cloned())
            .collect();
        shuffle(&mut deliveries, 0x5EED_1234);
        for (hash, bytes) in &deliveries {
            s.on_chunk_data(*hash, bytes).unwrap();
        }

        for f in &files {
            assert_eq!(
                on_disk(&s, &f.path).as_deref(),
                Some(f.content.as_slice()),
                "file {} must reassemble to its own content",
                f.path
            );
            assert!(
                s.engine.index().get(&f.path).is_some(),
                "completed file is an index head"
            );
        }
        assert!(s.assemblies.is_empty(), "every assembly completed");
        // Chunk staging is pruned once no assembly needs it.
        let staged = std::fs::read_dir(s.layout.chunks()).map_or(0, std::iter::Iterator::count);
        assert_eq!(staged, 0, "no chunk debris left in staging");
    }

    #[test]
    fn h6_shared_chunk_routes_to_one_assembly_at_a_time_no_cross_contamination() {
        // Two files SHARE their middle chunk (same seed) but differ elsewhere.
        // The current rule: one `ChunkData` delivery satisfies the FIRST
        // assembly that still wants that hash; the other must be re-served. Pin
        // that both files ultimately reassemble correctly with no bleed-over.
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_for_test(dir.path());

        let a = fake_file("a.bin", &[10, 50, 11], 1);
        let b = fake_file("b.bin", &[20, 50, 21], 1);
        assert_eq!(a.chunks[1].0, b.chunks[1].0, "middle chunk is shared");
        start_assembly(&mut s, &a);
        start_assembly(&mut s, &b);

        // Deliver each file's UNIQUE (unshared) chunks.
        for (hash, bytes) in [&a.chunks[0], &a.chunks[2], &b.chunks[0], &b.chunks[2]] {
            s.on_chunk_data(*hash, bytes).unwrap();
        }
        // Deliver the SHARED chunk once: it completes exactly ONE assembly.
        let (shared_hash, shared_bytes) = (a.chunks[1].0, a.chunks[1].1.clone());
        s.on_chunk_data(shared_hash, &shared_bytes).unwrap();
        let a_done = s.engine.index().get(&a.path).is_some();
        let b_done = s.engine.index().get(&b.path).is_some();
        assert!(
            a_done ^ b_done,
            "one shared-chunk delivery completes exactly one assembly"
        );
        assert_eq!(s.assemblies.len(), 1, "the other assembly still awaits it");

        // Serve the shared chunk again → the remaining assembly completes.
        s.on_chunk_data(shared_hash, &shared_bytes).unwrap();
        assert_eq!(on_disk(&s, &a.path).as_deref(), Some(a.content.as_slice()));
        assert_eq!(on_disk(&s, &b.path).as_deref(), Some(b.content.as_slice()));
        assert!(s.assemblies.is_empty());
    }

    #[test]
    fn h6_superseding_manifest_abandons_the_in_flight_assembly() {
        // A NEWER manifest (newer version) for the SAME path supersedes the
        // in-flight assembly: its partial chunks are discarded and the new one
        // takes over (invariant #3 — latest bytes win). SEED-PERF H6.
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_for_test(dir.path());

        let v1 = fake_file("f.bin", &[1, 2, 3], 1);
        start_assembly(&mut s, &v1);
        s.on_chunk_data(v1.chunks[0].0, &v1.chunks[0].1).unwrap();
        let v1_chunk0 = s.chunk_path(&v1.chunks[0].0);
        assert!(v1_chunk0.exists(), "v1's partial chunk is staged");

        // Superseding manifest for the same path (disjoint chunks, newer clock).
        let v2 = fake_file("f.bin", &[7, 8, 9], 2);
        start_assembly(&mut s, &v2);
        assert!(
            !v1_chunk0.exists(),
            "the superseded assembly's chunk was discarded"
        );
        assert_eq!(
            s.assemblies.len(),
            1,
            "only the superseding assembly remains"
        );

        for (hash, bytes) in &v2.chunks {
            s.on_chunk_data(*hash, bytes).unwrap();
        }
        assert_eq!(
            on_disk(&s, &v2.path).as_deref(),
            Some(v2.content.as_slice()),
            "the superseding version's content is what lands"
        );
    }

    #[test]
    fn h6_duplicate_manifest_for_a_path_restarts_the_assembly() {
        // A duplicate manifest for a path mid-assembly resolves by the SAME
        // path-keyed supersede rule (`chunkxfer::supersedes` is path equality):
        // the old assembly is abandoned and a fresh one (empty `have`) replaces
        // it, so already-received chunks must be re-delivered. SEED-PERF H6.
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_for_test(dir.path());

        let f = fake_file("dup.bin", &[4, 5, 6], 1);
        start_assembly(&mut s, &f);
        s.on_chunk_data(f.chunks[0].0, &f.chunks[0].1).unwrap();
        assert_eq!(s.assemblies.get(&f.path).map(|a| a.have.len()), Some(1));

        // Re-announce the identical manifest: abandon + fresh assembly.
        start_assembly(&mut s, &f);
        assert_eq!(s.assemblies.len(), 1);
        assert_eq!(
            s.assemblies.get(&f.path).map(|a| a.have.len()),
            Some(0),
            "the restarted assembly starts with nothing received"
        );
        assert!(
            !s.chunk_path(&f.chunks[0].0).exists(),
            "the pre-duplicate partial chunk was discarded"
        );

        // Re-deliver everything → completes correctly.
        for (hash, bytes) in &f.chunks {
            s.on_chunk_data(*hash, bytes).unwrap();
        }
        assert_eq!(on_disk(&s, &f.path).as_deref(), Some(f.content.as_slice()));
    }

    #[test]
    fn h6_chunk_data_for_unknown_assembly_is_ignored() {
        // A `ChunkData` frame for a hash no assembly is waiting on is ignored
        // without touching any state or staging a file (SEED-PERF H6).
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_for_test(dir.path());

        let orphan = vec![42u8; 512];
        let orphan_hash = *blake3::hash(&orphan).as_bytes();
        s.on_chunk_data(orphan_hash, &orphan).unwrap();
        assert!(s.assemblies.is_empty());
        assert!(
            !s.chunk_path(&orphan_hash).exists(),
            "an unknown chunk is never staged"
        );

        // With an unrelated assembly in flight, a chunk outside its manifest is
        // still ignored and leaves that assembly untouched.
        let f = fake_file("f.bin", &[1, 2, 3], 1);
        start_assembly(&mut s, &f);
        s.on_chunk_data(orphan_hash, &orphan).unwrap();
        assert!(!s.chunk_path(&orphan_hash).exists());
        assert_eq!(
            s.assemblies.get(&f.path).map(|a| a.have.len()),
            Some(0),
            "the in-flight assembly is undisturbed"
        );
    }

    #[test]
    fn h6_chunk_data_after_abandon_is_ignored() {
        // A late chunk for an assembly that has been abandoned (as going offline
        // or a supersede does) is ignored — no state, no staged file, no panic.
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_for_test(dir.path());

        let f = fake_file("f.bin", &[1, 2, 3], 1);
        start_assembly(&mut s, &f);
        s.abandon_assembly(&f.path);
        assert!(s.assemblies.is_empty());

        s.on_chunk_data(f.chunks[0].0, &f.chunks[0].1).unwrap();
        assert!(s.assemblies.is_empty(), "no assembly is resurrected");
        assert!(
            !s.chunk_path(&f.chunks[0].0).exists(),
            "a chunk for an abandoned assembly is not staged"
        );
    }

    // ---- H7: echo suppression under batched applies -----------------------

    /// Apply a peer `Change` for `path` (writes to disk, journals the echo).
    fn apply_remote(s: &mut Session, path: &str, bytes: &[u8], ticks: u32) -> RelPath {
        let p = RelPath::new(path).unwrap();
        let change = RemoteChange {
            path: p.clone(),
            kind: ChangeKind::Modified(test_sig(bytes)),
            version: peer_clock(ticks),
        };
        s.on_message(Message::Change {
            change,
            bytes: Some(bytes.to_vec()),
        })
        .unwrap();
        p
    }

    /// Feed the local watcher signal for a modified `path` (an echo of an apply,
    /// or a genuine local edit — resolved from disk exactly as production does).
    fn feed_watch(s: &mut Session, path: &RelPath) {
        s.on_watch(WatchSignal::Pending(PendingChange {
            rel: path.clone(),
            kind: PendingKind::Dirty,
        }))
        .unwrap();
    }

    #[test]
    fn h7_clustered_echoes_after_batched_applies_fabricate_no_outbound() {
        // A cluster of applies (Phase 1/2's batch shape: many applies inside one
        // watcher-latency window), then their echoes arrive clustered and
        // shuffled — tighter than watcher latency. EVERY echo must be swallowed
        // by the journal: zero index change, zero new versions, no dirty flip
        // (a fabricated outbound would move the index and record a version).
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_for_test(dir.path());

        let files: Vec<(RelPath, Vec<u8>)> = (0..12u32)
            .map(|i| {
                let bytes = format!("content-number-{i}").into_bytes();
                (
                    apply_remote(&mut s, &format!("f{i}.txt"), &bytes, i + 1),
                    bytes,
                )
            })
            .collect();
        for (p, bytes) in &files {
            assert_eq!(on_disk(&s, p).as_deref(), Some(bytes.as_slice()), "applied");
        }

        // Snapshot AFTER applies, BEFORE echoes, and isolate the echo effect.
        let index_before = s.engine.index().canonical_bytes();
        let versions_before: Vec<usize> = files
            .iter()
            .map(|(p, _)| s.history.log(p).unwrap().len())
            .collect();
        s.index_dirty = false;

        // Deliver the echoes clustered + shuffled.
        let mut order: Vec<usize> = (0..files.len()).collect();
        shuffle(&mut order, 0x9E37_79B9);
        for &i in &order {
            feed_watch(&mut s, &files[i].0);
        }

        assert_eq!(
            s.engine.index().canonical_bytes(),
            index_before,
            "clustered echoes must not change the index"
        );
        for (i, (p, _)) in files.iter().enumerate() {
            assert_eq!(
                s.history.log(p).unwrap().len(),
                versions_before[i],
                "an echo must record no new version for {p}"
            );
        }
        assert!(
            !s.index_dirty,
            "no echo may mark state dirty (no fabricated Send/RecordVersion)"
        );
    }

    #[test]
    fn h7_real_edit_to_a_different_path_passes_through_the_echo_cluster() {
        // Amid a cluster of echoes, a GENUINE local edit to a different,
        // never-applied path must NOT be over-suppressed: it advances the index
        // and fires an outbound change (invariant #3), while the echoes stay
        // swallowed. SEED-PERF H7.
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_for_test(dir.path());

        let applied: Vec<RelPath> = (0..5u32)
            .map(|i| apply_remote(&mut s, &format!("a{i}.txt"), b"applied-bytes", i + 1))
            .collect();
        s.index_dirty = false;

        // A real user edit to a fresh path lands on disk, then is observed.
        let real = RelPath::new("user-edit.txt").unwrap();
        std::fs::write(join(s.layout.root(), &real), b"a genuine local edit").unwrap();

        // Interleave: some echoes, the real edit, then the rest of the echoes.
        for p in &applied[..2] {
            feed_watch(&mut s, p);
        }
        feed_watch(&mut s, &real);
        for p in &applied[2..] {
            feed_watch(&mut s, p);
        }

        assert!(
            s.engine.index().get(&real).is_some(),
            "the real edit became an index head"
        );
        assert!(
            matches!(
                s.engine.index().get(&real).unwrap().winner().state,
                EntryState::Present(sig) if sig == test_sig(b"a genuine local edit")
            ),
            "the real edit's bytes are the winner"
        );
        assert!(
            s.index_dirty,
            "the real edit fired an outbound change (not over-suppressed)"
        );
        // The echoed paths stayed exactly at their applied state.
        for p in &applied {
            assert!(matches!(
                s.engine.index().get(p).unwrap().winner().state,
                EntryState::Present(sig) if sig == test_sig(b"applied-bytes")
            ));
        }
    }

    #[test]
    fn h7_real_edit_with_different_bytes_to_an_applied_path_is_not_over_suppressed() {
        // A real local edit to a JUST-APPLIED path but with DIFFERENT bytes is
        // not an echo (the journaled expectation is the applied sig) and must
        // pass through: the index winner advances to the new bytes. Guards
        // against an over-broad, path-only suppression. SEED-PERF H7.
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_for_test(dir.path());

        let p = apply_remote(&mut s, "f.txt", b"v1-applied-bytes", 1);
        let applied_head = s.engine.index().get(&p).unwrap().winner().state;
        s.index_dirty = false;

        // The user overwrites the same path with different content.
        let edited = b"v2-user-edited-different-bytes";
        std::fs::write(join(s.layout.root(), &p), edited).unwrap();
        feed_watch(&mut s, &p);

        let new_head = s.engine.index().get(&p).unwrap().winner().state;
        assert_ne!(
            new_head, applied_head,
            "a different-bytes edit advances the head"
        );
        assert!(matches!(
            new_head,
            EntryState::Present(sig) if sig == test_sig(edited)
        ));
        assert!(
            s.index_dirty,
            "the genuine edit fires an outbound change (not swallowed as an echo)"
        );
    }
}
