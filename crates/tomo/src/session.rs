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
use tomo_history::{HistoryStore, Origin, VersionId};
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

/// One-shot hold before shipping an EMPTY observation of a previously
/// non-empty file, so a truncate-then-write save's zero-byte window is
/// re-checked rather than mirrored to the peer (SPEC §5.1). Genuine
/// truncations to empty survive the re-check and ship ~30ms later.
const EMPTY_HOLD: Duration = Duration::from_millis(30);

/// First reconnect back-off after a peer disconnect (watch modes only).
const RECONNECT_MIN: Duration = Duration::from_secs(2);

/// Back-off ceiling: reconnect attempts never wait longer than this.
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// While chunk frames are queued to ship, the loop wakes this often to pump the
/// next small batch (keeping a bulk transfer moving without starving recv).
const CHUNK_PUMP_TICK: Duration = Duration::from_millis(2);

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
    shutdown: Arc<AtomicBool>,
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
    /// drained a few at a time so live `Change`s interleave (docs/SPEC.md §8).
    pending_chunks: VecDeque<Message>,
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
    // Single-session lock (both sides): refuse to start a second sync/serve
    // session for this project. Acquired first so it fails fast — before we
    // open the history store or start the watcher — and held for the whole
    // session (dropped when `run` returns, releasing the flock). A `kill -9`
    // releases it via the kernel; there is no staleness logic (see `lockfile`).
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
        shutdown,
        tx: tx.clone(),
        reconnect_plan: ReconnectPlan::None,
        offline_since: None,
        next_reconnect_at: None,
        backoff: RECONNECT_MIN,
        assemblies: HashMap::new(),
        outbound_manifests: HashMap::new(),
        pending_chunks: VecDeque::new(),
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
    let flush = session.persist(true);
    if let Some(mut t) = session.transport.take() {
        t.join_reader();
    }
    outcome.and(drained).and(flush)
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
                (Some(transport::local_peer(&path, tx)?), Some(peer))
            }
            Mode::Serve => {
                // Serving side: learn who connected from the SSH environment the
                // initiator prepended (TOMO_PEER_NAME) plus SSH_CONNECTION.
                side = crate::readme::Side::Serving;
                self.peer_identity = crate::status::peer_from_ssh_env();
                (Some(transport::stdio(tx)), None)
            }
            Mode::Ssh(params) => {
                let peer = transport::describe_route(&params);
                // Initiator side: record the peer from the configured [remote]
                // (the target host, and the resolved host as the address).
                self.peer_identity = Some(peer_from_ssh_params(&params));
                self.reporter
                    .note(&format!("connecting to {peer} over SSH"));
                let (t, report) = transport::ssh(&params, tx, false)?;
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
        // Abandon in-flight transfers in both directions — a fresh reconcile
        // rebuilds what's needed after reconnect.
        self.abandon_all_assemblies();
        self.pending_chunks.clear();
        self.outbound_manifests.clear();
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
        match plan {
            ReconnectPlan::None => Ok(()),
            ReconnectPlan::LocalPeer(path) => match transport::local_peer(&path, &tx) {
                Ok(t) => self.on_reconnected(t),
                Err(e) => {
                    self.reconnect_failed(&e.to_string());
                    Ok(())
                }
            },
            ReconnectPlan::Ssh(params) => match transport::ssh(&params, &tx, false) {
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
            // Ship a small batch of queued chunk data before blocking on recv,
            // so a bulk transfer keeps moving while live small-file Changes
            // still interleave between batches (docs/SPEC.md §8, invariant #3).
            self.ship_pending_chunks()?;

            // Wake at the sooner of the status cadence, the next staged history
            // deadline, a pending reconnect, and (while chunks are queued) the
            // chunk-pump tick — so nothing waits behind a long idle timeout.
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
                let batch = coalesce_burst(drain_burst(first, rx));
                for item in batch {
                    if self.process_incoming(item)? {
                        return Ok(());
                    }
                    if self.repush_requested {
                        return Ok(());
                    }
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
        }
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
                self.on_message(msg)?;
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
    /// rescan is pending) the quiescence window. While chunk frames are queued
    /// the loop instead wakes on the short chunk-pump tick.
    fn recv_deadline(&self) -> Duration {
        if !self.pending_chunks.is_empty() {
            return CHUNK_PUMP_TICK;
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
        match msg {
            Message::Hello {
                protocol,
                binary_version,
                replica,
            } => self.on_hello(protocol, &binary_version, replica),
            Message::IndexExchange(peer_index) => self.reconcile(&peer_index),
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
                    self.record_conflict_capture(capture)?;
                    if self.conflicts.insert(path.clone()) {
                        self.status_dirty = true;
                    }
                    if adopted {
                        // Genesis first sync: word it as an intentional adoption
                        // of the more recently modified copy, not a mid-session
                        // clash (the loser is still preserved in history).
                        self.reporter.conflict_adopted(path.as_str());
                    } else {
                        // A one-line resolution summary for the styled event line;
                        // the deterministic winner is already decided by the engine.
                        let detail = if winner_is_local {
                            "kept the local version"
                        } else {
                            "kept the peer's version"
                        };
                        self.reporter.conflict(path.as_str(), Some(detail));
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
    ///
    /// Content below [`INLINE_THRESHOLD`] rides inline in a [`Message::Change`];
    /// larger content ships as a [`Message::ChangeManifest`] whose chunks the
    /// peer then pulls (docs/SPEC.md §8).
    fn do_send(&mut self, change: RemoteChange) -> Result<(), CliError> {
        if self.transport.is_none() {
            return Ok(()); // watch-only / offline / pre-handshake: nothing to ship.
        }
        // Egress filter — the single choke point for ALL outbound changes,
        // including the reconcile head-shipping loop. An ignored / pull-only path
        // is never shipped, so a stale pre-upgrade `.git` index head that survived
        // into this session goes inert instead of re-contaminating the peer.
        if !self.allow_crossing(&change.path, crate::crossing::Flow::Outbound) {
            return Ok(());
        }
        match change.kind {
            ChangeKind::Modified(sig) => {
                let full = join(self.layout.root(), &change.path);
                let current = std::fs::read(&full).ok();
                if !should_send(current.as_deref(), &sig) {
                    // The file changed again (or vanished); the watcher's
                    // follow-up event ships the newer state. Drop this one.
                    return Ok(());
                }
                // `should_send` guaranteed `Some`; default is unreachable.
                let bytes = current.unwrap_or_default();
                // Capture the path and size before `change`/`bytes` are moved into
                // the frame, so a styled `↑` send line can be emitted afterward.
                let path = change.path.as_str().to_owned();
                let size = bytes.len() as u64;
                if bytes.len() >= INLINE_THRESHOLD {
                    self.send_manifest(change, &bytes)?;
                } else {
                    self.send(&Message::Change {
                        change,
                        bytes: Some(bytes),
                    })?;
                }
                self.reporter.sent(&path, size);
                Ok(())
            }
            ChangeKind::Removed => self.send(&Message::Change {
                change,
                bytes: None,
            }),
        }
    }

    /// Announce a large `Modified` change as a chunk manifest and remember which
    /// chunk hashes belong to this path so a later [`Message::ChunkRequest`] can
    /// be served by re-reading and re-chunking the current file (no chunk bytes
    /// are retained — invariant #3 keeps the sender stateless).
    fn send_manifest(&mut self, change: RemoteChange, bytes: &[u8]) -> Result<(), CliError> {
        let chunks = tomo_history::chunk_bytes(bytes);
        let manifest: Vec<ChunkHash> = chunks.iter().map(|(h, _)| h.0).collect();
        let ranges: Vec<(ChunkHash, Range<usize>)> =
            chunks.into_iter().map(|(h, r)| (h.0, r)).collect();
        self.outbound_manifests.insert(change.path.clone(), ranges);
        let total_size = bytes.len() as u64;
        self.send(&Message::ChangeManifest {
            change,
            total_size,
            manifest,
        })
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

    /// Ship at most [`chunkxfer::CHUNKS_PER_PUMP`] queued chunk frames, then
    /// return so the loop can service live Changes between batches (invariant #3).
    fn ship_pending_chunks(&mut self) -> Result<(), CliError> {
        for _ in 0..chunkxfer::CHUNKS_PER_PUMP {
            let Some(msg) = self.pending_chunks.pop_front() else {
                break;
            };
            self.send(&msg)?;
            if self.transport.is_none() {
                // A send just dropped us offline; the queue is now moot.
                self.pending_chunks.clear();
                break;
            }
        }
        Ok(())
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
}
