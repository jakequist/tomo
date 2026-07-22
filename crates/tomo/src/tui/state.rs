//! The TUI's pure core (UX-V2 §3a/§3b): a [`Model`] plus a single
//! `(Model, Msg) -> Model` [`update`] reducer that holds **all** interaction
//! logic — screen switching, selection movement, filter editing, scrollback and
//! follow state, verdict dispatch, and pending-command bookkeeping.
//!
//! Nothing here does I/O, reads a clock, or touches a terminal, so every
//! behavior is unit-tested without a `ratatui` backend or a live socket. The
//! shell ([`super::run`]) feeds it three kinds of message — a parsed control
//! event, a key, or a tick — plus the outcome of a command it dispatched, and
//! drains the [`Model::outbox`] to issue commands over the control channel.
//!
//! Display-only wall time (invariant #7 untouched): the shell stamps
//! [`Model::now_ms`] before each `update`, and the view reads it to render
//! "2s ago" style recency. The reducer never calls a clock itself.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use crate::ctl::proto::Event;

/// The wall-clock milliseconds one tick advances the model's sense of "now"
/// between heartbeats, so "last sync Ns ago" keeps ticking on a silent screen.
/// The shell drives ticks at this cadence.
pub const TICK_MS: u64 = 500;

/// Ticks a transient footer flash (an error or a hint) stays visible: ~4 s.
const FLASH_TICKS: u32 = 8;

/// Ticks the "0 conflicts 🎉" celebration shows before returning to the stream.
const CELEBRATE_TICKS: u32 = 4;

/// Maximum stream lines retained (older lines are dropped from the front).
const MAX_EVENTS: usize = 2000;

/// A minimum usable terminal; anything smaller renders a single-line fallback
/// rather than a broken layout.
pub const MIN_COLS: u16 = 40;
/// See [`MIN_COLS`].
pub const MIN_ROWS: u16 = 8;

// ---- messages -------------------------------------------------------------

/// A terminal key, decoupled from `crossterm` so the reducer is testable without
/// the input backend. The shell maps `crossterm::event::KeyEvent` to this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A printable character (space included, as `Char(' ')`).
    Char(char),
    /// Return / Enter.
    Enter,
    /// Escape.
    Esc,
    /// Backspace.
    Backspace,
    /// Arrow up.
    Up,
    /// Arrow down.
    Down,
    /// Arrow left.
    Left,
    /// Arrow right.
    Right,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
    /// Home.
    Home,
    /// End.
    End,
}

/// One message into the reducer.
#[derive(Debug, Clone)]
pub enum Msg {
    /// A parsed control-channel event record.
    Event(Event),
    /// A key press.
    Key(Key),
    /// A periodic tick ([`TICK_MS`]).
    Tick,
    /// The outcome of a command the shell dispatched for us.
    Cmd(CmdOutcome),
}

/// A command the reducer wants the shell to run over the control channel. The
/// shell serializes [`CtlRequest::to_json`], sends it on a fresh connection, and
/// feeds the reply back as a [`Msg::Cmd`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutCommand {
    /// Correlates the eventual [`CmdOutcome`] back to this request.
    pub seq: u64,
    /// What to run.
    pub req: CtlRequest,
}

/// A control-channel request the reducer issues. Maps 1:1 to a `cmd` object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CtlRequest {
    /// Refresh the unresolved-conflict list.
    ConflictsList,
    /// Fetch one conflict's winner/loser framing + inline diff.
    ConflictShow {
        /// The conflict id.
        id: i64,
    },
    /// Resolve one conflict (`keep`/`take`/`both`).
    Resolve {
        /// The conflict id.
        id: i64,
        /// The verdict.
        verdict: Verdict,
    },
    /// Fetch the recently-versioned paths for the history picker.
    HistoryPaths {
        /// Maximum number of paths to fetch.
        limit: usize,
    },
    /// Fetch one path's version timeline (newest first).
    HistoryLog {
        /// The repo-relative path.
        path: String,
        /// Maximum number of versions to fetch.
        limit: usize,
    },
    /// Fetch a unified diff between two versions of a path.
    VersionDiff {
        /// The repo-relative path.
        path: String,
        /// The base (older) version id.
        from: i64,
        /// The target (newer) version id.
        to: i64,
    },
    /// Restore a recorded version of a path into the tree (crash-safe apply; the
    /// session ships it as an ordinary edit).
    Restore {
        /// The repo-relative path.
        path: String,
        /// The version id to restore.
        version: i64,
    },
    /// Mark a resolved conflict unresolved again (the TUI's real undo).
    ConflictUnresolve {
        /// The conflict id.
        id: i64,
    },
    /// Pause syncing (docs/SPEC.md §13).
    Pause,
    /// Resume syncing (docs/SPEC.md §13).
    Resume,
    /// Stop the session (foreground `q`, delivered by the shell after
    /// teardown — never queued through the outbox).
    Stop,
}

impl CtlRequest {
    /// The `cmd` object JSON this request sends over the command channel.
    #[must_use]
    pub fn to_json(&self) -> Value {
        match self {
            CtlRequest::ConflictsList => json!({"type": "conflicts_list"}),
            CtlRequest::ConflictShow { id } => json!({"type": "conflict_show", "id": id}),
            CtlRequest::Resolve { id, verdict } => {
                json!({"type": "conflicts_resolve", "id": id, "action": verdict.action_str()})
            }
            CtlRequest::HistoryPaths { limit } => {
                json!({"type": "history_paths", "limit": limit})
            }
            CtlRequest::HistoryLog { path, limit } => {
                json!({"type": "history_log", "path": path, "limit": limit})
            }
            CtlRequest::VersionDiff { path, from, to } => {
                json!({"type": "version_diff", "path": path, "from": from, "to": to})
            }
            CtlRequest::Restore { path, version } => {
                json!({"type": "restore", "path": path, "version": version})
            }
            CtlRequest::ConflictUnresolve { id } => {
                json!({"type": "conflict_unresolve", "id": id})
            }
            CtlRequest::Pause => json!({"type": "pause"}),
            CtlRequest::Resume => json!({"type": "resume"}),
            CtlRequest::Stop => json!({"type": "stop"}),
        }
    }
}

/// A single-key conflict verdict that mutates state (skip is not one; it only
/// moves the selection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Keep the current file, acknowledge (the common case).
    Keep,
    /// Adopt the preserved loser into the tree.
    Take,
    /// Materialize the loser alongside the winner (`.theirs`).
    Both,
}

impl Verdict {
    /// The control-channel `action` string.
    fn action_str(self) -> &'static str {
        match self {
            Verdict::Keep => "keep",
            Verdict::Take => "take",
            Verdict::Both => "both",
        }
    }

    /// The CLI flag echoed in the conflict-center footer.
    #[must_use]
    pub fn cli_flag(self) -> &'static str {
        match self {
            Verdict::Keep => "--keep-current",
            Verdict::Take => "--take-loser",
            Verdict::Both => "--both",
        }
    }
}

/// The outcome of a dispatched [`OutCommand`], correlated by `seq`.
#[derive(Debug, Clone)]
pub struct CmdOutcome {
    /// The originating [`OutCommand::seq`].
    pub seq: u64,
    /// The reply, or a human error string on failure.
    pub result: Result<CmdReply, String>,
}

/// A parsed successful command reply.
#[derive(Debug, Clone)]
pub enum CmdReply {
    /// The refreshed conflict list.
    Conflicts(Vec<ConflictRow>),
    /// One conflict's diff detail.
    Show {
        /// The conflict id the detail is for.
        id: i64,
        /// The framing + diff.
        detail: ConflictDetail,
    },
    /// A verdict succeeded (correlated back by [`CmdOutcome::seq`]).
    Resolved,
    /// The recently-versioned paths for the history picker.
    HistoryPaths(Vec<PathRow>),
    /// One path's version timeline (newest first).
    HistoryLog {
        /// The path the timeline is for.
        path: String,
        /// The versions, newest first.
        versions: Vec<VersionRow>,
    },
    /// A version-to-version diff, keyed back to the requested pair.
    VersionDiff {
        /// The base (older) version id.
        from: i64,
        /// The target (newer) version id.
        to: i64,
        /// The diff outcome.
        detail: DiffDetail,
    },
    /// A restore succeeded, reporting what was written.
    Restored {
        /// The restored path.
        path: String,
        /// The version id restored.
        version: i64,
        /// The content size written (0 for a restored deletion).
        size: u64,
    },
    /// A `conflict_unresolve` succeeded (the TUI's real undo).
    Unresolved,
    /// A `pause`/`resume` succeeded, carrying the authoritative new pause state.
    PauseState(bool),
}

// ---- conflict data (parsed from the ctl JSON) -----------------------------

/// Which replica authored a version, in display terms (UX-V2 §3b framing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// This machine ("you", cyan).
    You,
    /// The peer (magenta).
    Peer,
}

impl Side {
    /// Parse a `LogEntryJson.origin` string (`"local"`/`"remote"`).
    fn from_origin(origin: &str) -> Side {
        if origin == "remote" {
            Side::Peer
        } else {
            Side::You
        }
    }

    /// The display label, using the peer's name when known for the peer side.
    #[must_use]
    pub fn label(self, peer: Option<&str>) -> String {
        match self {
            Side::You => "you".to_owned(),
            Side::Peer => peer.unwrap_or("peer").to_owned(),
        }
    }
}

/// One unresolved conflict row (from `conflicts_list`), reduced to what the
/// list pane renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictRow {
    /// The conflict id (matches `tomo conflicts list`).
    pub id: i64,
    /// The repo-relative path.
    pub path: String,
    /// Wall time the conflict was recorded (display-only).
    pub wall_ms: u64,
    /// Which side's version is on disk now (the winner).
    pub winner: Side,
}

/// One conflict's diff detail (from `conflict_show`), for the right pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictDetail {
    /// The winner (on disk now).
    pub winner: Head,
    /// The loser (in history).
    pub loser: Head,
    /// The winner's version id (the version on disk now). Captured with a `take`
    /// verdict's undo plan so undo can restore it (UX-V2 §3b real undo).
    pub winner_version: i64,
    /// The loser's version id (preserved in history).
    pub loser_version: i64,
    /// The unified-style diff lines (loser → winner), empty when not diffable.
    pub diff: Vec<String>,
    /// Whether both heads were diffable text.
    pub diffable: bool,
}

/// One side's head metadata for the framing lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    /// Which replica authored it.
    pub side: Side,
    /// Wall time (display-only).
    pub wall_ms: u64,
}

/// Parse the `conflicts_list` reply array into rows (newest first).
#[must_use]
pub fn parse_conflicts(v: &Value) -> Vec<ConflictRow> {
    let mut rows: Vec<ConflictRow> = v
        .as_array()
        .map(|arr| arr.iter().filter_map(parse_conflict_row).collect())
        .unwrap_or_default();
    // Newest first: greatest wall time, ties broken by greatest id.
    rows.sort_by(|a, b| b.wall_ms.cmp(&a.wall_ms).then_with(|| b.id.cmp(&a.id)));
    rows
}

fn parse_conflict_row(v: &Value) -> Option<ConflictRow> {
    let id = v.get("id")?.as_i64()?;
    let path = v.get("path")?.as_str()?.to_owned();
    let wall_ms = v.get("wall_unix_ms").and_then(Value::as_u64).unwrap_or(0);
    let winner = v
        .get("winner")
        .and_then(|w| w.get("origin"))
        .and_then(Value::as_str)
        .map_or(Side::You, Side::from_origin);
    Some(ConflictRow {
        id,
        path,
        wall_ms,
        winner,
    })
}

/// Parse the `conflict_show` reply object into a [`ConflictDetail`].
#[must_use]
pub fn parse_detail(v: &Value) -> Option<ConflictDetail> {
    let head = |key: &str| -> Head {
        let obj = v.get(key);
        let side = obj
            .and_then(|o| o.get("origin"))
            .and_then(Value::as_str)
            .map_or(Side::You, Side::from_origin);
        let wall_ms = obj
            .and_then(|o| o.get("wall_unix_ms"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        Head { side, wall_ms }
    };
    let version = |key: &str| -> i64 {
        v.get(key)
            .and_then(|o| o.get("id"))
            .and_then(Value::as_i64)
            .unwrap_or(0)
    };
    // A show reply must at least name a path; guard against a stray object.
    v.get("path")?;
    let diffable = v.get("diffable").and_then(Value::as_bool).unwrap_or(false);
    let diff = v
        .get("diff")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|l| l.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    Some(ConflictDetail {
        winner: head("winner"),
        loser: head("loser"),
        winner_version: version("winner"),
        loser_version: version("loser"),
        diff,
        diffable,
    })
}

// ---- history browser data (parsed from the ctl JSON) ----------------------

/// One path row in the history picker (from `history_paths`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathRow {
    /// The repo-relative path.
    pub path: String,
    /// How many versions of this path are recorded.
    pub versions: u64,
    /// The id of the newest recorded version.
    pub last_version: i64,
    /// Wall time of the newest version (display-only).
    pub last_wall_ms: u64,
}

/// One version row in the history timeline (from `history_log`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionRow {
    /// The version id.
    pub id: i64,
    /// Wall time the version was recorded (display-only).
    pub wall_ms: u64,
    /// The content size in bytes, or `None` for a deletion (tombstone).
    pub size: Option<u64>,
    /// Which side authored the version (you/peer coloring).
    pub side: Side,
    /// Whether this present version's executable bit is set.
    pub exec: bool,
    /// Whether the version is present (vs. a deletion).
    pub present: bool,
}

/// A version-to-version diff outcome (from `version_diff`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffDetail {
    /// Whether the two versions are byte-identical.
    pub identical: bool,
    /// Whether both sides were diffable text.
    pub diffable: bool,
    /// The unified-style diff lines (from → to), empty when not diffable.
    pub diff: Vec<String>,
}

/// Parse a `history_paths` reply's `paths` array into rows (already newest
/// first from the store).
#[must_use]
pub fn parse_history_paths(v: &Value) -> Vec<PathRow> {
    v.as_array()
        .map(|arr| arr.iter().filter_map(parse_path_row).collect())
        .unwrap_or_default()
}

fn parse_path_row(v: &Value) -> Option<PathRow> {
    let path = v.get("path")?.as_str()?.to_owned();
    let versions = v.get("versions").and_then(Value::as_u64).unwrap_or(0);
    let last_version = v.get("last_version").and_then(Value::as_i64).unwrap_or(0);
    let last_wall_ms = v
        .get("last_wall_unix_ms")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(PathRow {
        path,
        versions,
        last_version,
        last_wall_ms,
    })
}

/// Parse a `history_log` reply's `versions` array into rows (newest first).
#[must_use]
pub fn parse_history_log(v: &Value) -> Vec<VersionRow> {
    v.as_array()
        .map(|arr| arr.iter().filter_map(parse_version_row).collect())
        .unwrap_or_default()
}

fn parse_version_row(v: &Value) -> Option<VersionRow> {
    let id = v.get("id")?.as_i64()?;
    let wall_ms = v.get("wall_unix_ms").and_then(Value::as_u64).unwrap_or(0);
    let size = v.get("size").and_then(Value::as_u64);
    let present = v.get("present").and_then(Value::as_bool).unwrap_or(true);
    let exec = v.get("exec").and_then(Value::as_bool).unwrap_or(false);
    let authored = v
        .get("origin")
        .and_then(Value::as_str)
        .map_or(Side::You, Side::from_origin);
    Some(VersionRow {
        id,
        wall_ms,
        size,
        side: authored,
        exec,
        present,
    })
}

/// Parse a `version_diff` reply into a [`DiffDetail`].
#[must_use]
pub fn parse_version_diff(v: &Value) -> DiffDetail {
    let identical = v.get("identical").and_then(Value::as_bool).unwrap_or(false);
    let diffable = v.get("diffable").and_then(Value::as_bool).unwrap_or(false);
    let diff = v
        .get("diff")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|l| l.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    DiffDetail {
        identical,
        diffable,
        diff,
    }
}

/// A single-level undo plan for the conflict center, captured at verdict time
/// (UX-V2 §3b real undo). Records what to run to reverse the last verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoPlan {
    /// The conflict the verdict resolved.
    pub conflict_id: i64,
    /// That conflict's path.
    pub path: String,
    /// The verdict that was applied (decides how to reverse it).
    pub verdict: Verdict,
    /// The version that was on disk before a `take` (the winner), so undo can
    /// restore it. `None` for `keep`/`both` (no tree write to reverse).
    pub pre_version: Option<i64>,
}

// ---- the model ------------------------------------------------------------

/// Which top-level screen is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// The event stream (UX-V2 §3a).
    Main,
    /// The conflict center (UX-V2 §3b).
    Conflicts,
    /// The history browser's path picker (UX-V2 §3, TUI v2): a filterable list of
    /// recently-versioned paths.
    HistoryPicker,
    /// The history browser's version timeline for a selected path, with a live
    /// diff pane.
    HistoryTimeline,
}

/// A modal overlay awaiting confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modal {
    /// "acknowledge all N conflicts?" — `a` on the conflict center.
    AckAll {
        /// How many would be acknowledged.
        count: usize,
    },
    /// "stop syncing?" — `q` on a foreground-started session (UX-V2 §1: quit
    /// stops the session only when it was started foreground-attached).
    StopConfirm,
    /// "restore <path> to #N?" — `r` on a history timeline version (UX-V2 §3).
    RestoreConfirm {
        /// The path to restore.
        path: String,
        /// The version id to restore.
        version: i64,
        /// The version's content size (0 for a deletion), for the prompt.
        size: u64,
        /// The version's wall time, for the "N ago" in the prompt.
        wall_ms: u64,
        /// Whether the version is a deletion (tombstone).
        deleted: bool,
    },
}

/// One retained stream line (a log-worthy event).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamLine {
    /// The originating event (drives glyph, wording, and path filtering).
    pub event: Event,
}

impl StreamLine {
    /// The path this line matches a filter against, if any.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        match &self.event {
            Event::Synced { path, .. }
            | Event::Sent { path, .. }
            | Event::Removed { path }
            | Event::Conflict { path, .. } => Some(path),
            _ => None,
        }
    }
}

/// One in-flight transfer (pinned zone).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    /// The repo-relative path.
    pub path: String,
    /// Bytes done.
    pub done: u64,
    /// Total bytes.
    pub total: u64,
}

/// The complete TUI state.
// The flags below (help/quit/follow/new_activity/filter_editing/connected/
// group_collapsed) are independent UI states, not a packable set; grouping them
// would obscure the model, so we accept the bool count here.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub struct Model {
    /// The active screen.
    pub screen: Screen,
    /// Whether the help overlay is showing.
    pub help: bool,
    /// A pending confirmation modal, if any.
    pub modal: Option<Modal>,
    /// Whether the user asked to quit.
    pub quit: bool,
    /// Foreground mode: this TUI fronts a session the user started with a
    /// plain `tomo sync`, so `q` offers to STOP the session (`d` detaches).
    /// Attached mode (`tomo attach`): both keys just detach.
    pub foreground: bool,
    /// Set when the quit was a confirmed stop — the shell delivers the ctl
    /// `stop` after teardown (synchronously, so it cannot be lost mid-exit).
    pub stopping: bool,
    /// Files synced/sent while this TUI was attached (exit-summary counter).
    pub synced_total: u64,
    /// Conflicts resolved from this TUI (exit-summary counter).
    pub resolved_total: u64,

    /// Display-only wall clock, stamped by the shell before each `update`.
    pub now_ms: u64,

    // -- stream --
    /// The retained stream lines (oldest first).
    pub events: Vec<StreamLine>,
    /// Whether the stream is stuck to the tail.
    pub follow: bool,
    /// Lines scrolled up from the bottom (0 when following).
    pub scroll: usize,
    /// Whether new activity arrived while scrolled back (the nudge).
    pub new_activity: bool,
    /// The active path filter, if any.
    pub filter: Option<String>,
    /// Whether the filter input is being edited.
    pub filter_editing: bool,

    // -- transfers / status --
    /// In-flight transfers (insertion order).
    pub transfers: Vec<Transfer>,
    /// The peer's name, when known.
    pub peer_name: Option<String>,
    /// The peer's address, when known.
    pub peer_addr: Option<String>,
    /// Whether the peer session is up.
    pub connected: bool,
    /// Whether the session is paused (docs/SPEC.md §13). Tracked from the
    /// `paused`/`resumed` events and every heartbeat, so multiple attached
    /// clients stay consistent. `space` toggles it.
    pub paused: bool,
    /// Unresolved-conflict count (from the heartbeat).
    pub unresolved: u64,
    /// Wall time of the last sync, derived from a heartbeat (display-only).
    pub last_sync_wall_ms: Option<u64>,

    // -- conflict center --
    /// The unresolved conflicts (newest first).
    pub conflicts: Vec<ConflictRow>,
    /// Ids optimistically resolved (hidden until confirmed/failed).
    pub pending_resolved: HashSet<i64>,
    /// In-flight resolve commands: `seq → conflict id`, so a failure rolls back
    /// exactly the row it hid (and a success just clears the marker).
    pending_cmds: HashMap<u64, i64>,
    /// Ids known to be adoptions (from `conflict` events), for grouping.
    pub adopted_ids: HashSet<i64>,
    /// Whether the adoption group is collapsed.
    pub group_collapsed: bool,
    /// Selection index into the current visible-row list.
    pub sel: usize,
    /// Cached diffs by conflict id.
    pub diffs: HashMap<i64, ConflictDetail>,
    /// Conflict ids for which a `conflict_show` is already in flight.
    pub diff_requested: HashSet<i64>,
    /// The single-level undo plan for the last conflict verdict (UX-V2 §3b),
    /// captured at verdict time; cleared by a new verdict or a list refresh.
    pub undo: Option<UndoPlan>,

    // -- history browser (UX-V2 §3, TUI v2) --
    /// The recently-versioned paths (newest version first), for the picker.
    pub history_paths: Vec<PathRow>,
    /// The picker's type-to-filter needle.
    pub picker_filter: String,
    /// Selection index into the filtered picker list.
    pub picker_sel: usize,
    /// The path whose timeline is showing (when in `HistoryTimeline`).
    pub history_path: Option<String>,
    /// The selected path's versions, newest first.
    pub timeline: Vec<VersionRow>,
    /// Selection index into the timeline.
    pub timeline_sel: usize,
    /// A marked version id for a two-version compare (`m`), if any.
    pub mark: Option<i64>,
    /// Cached version diffs, keyed by the `(from, to)` id pair.
    pub version_diffs: HashMap<(i64, i64), DiffDetail>,
    /// `(from, to)` pairs whose `version_diff` is already in flight.
    version_diff_requested: HashSet<(i64, i64)>,

    // -- transient chrome --
    /// A transient footer message (error or hint).
    pub flash: Option<String>,
    /// Ticks remaining on [`Model::flash`].
    flash_ticks: u32,
    /// Ticks remaining on the "0 conflicts 🎉" celebration (0 = inactive).
    celebrate_ticks: u32,

    // -- command plumbing --
    /// Commands the shell should dispatch (drained after each `update`).
    pub outbox: Vec<OutCommand>,
    /// Monotonic command sequence.
    next_seq: u64,
}

impl Default for Model {
    fn default() -> Self {
        Model {
            screen: Screen::Main,
            help: false,
            modal: None,
            quit: false,
            foreground: false,
            stopping: false,
            synced_total: 0,
            resolved_total: 0,
            now_ms: 0,
            events: Vec::new(),
            follow: true,
            scroll: 0,
            new_activity: false,
            filter: None,
            filter_editing: false,
            transfers: Vec::new(),
            peer_name: None,
            peer_addr: None,
            connected: false,
            paused: false,
            unresolved: 0,
            last_sync_wall_ms: None,
            conflicts: Vec::new(),
            pending_resolved: HashSet::new(),
            pending_cmds: HashMap::new(),
            adopted_ids: HashSet::new(),
            group_collapsed: false,
            sel: 0,
            diffs: HashMap::new(),
            diff_requested: HashSet::new(),
            undo: None,
            history_paths: Vec::new(),
            picker_filter: String::new(),
            picker_sel: 0,
            history_path: None,
            timeline: Vec::new(),
            timeline_sel: 0,
            mark: None,
            version_diffs: HashMap::new(),
            version_diff_requested: HashSet::new(),
            flash: None,
            flash_ticks: 0,
            celebrate_ticks: 0,
            outbox: Vec::new(),
            next_seq: 0,
        }
    }
}

/// One visible row in the conflict list pane (respecting collapse).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisRow {
    /// A standalone (non-adoption) conflict, indexed into the filtered set.
    Conflict(usize),
    /// The adoption group header.
    GroupHeader,
    /// A child of the adoption group, indexed into the filtered set.
    GroupChild(usize),
}

impl Model {
    /// Whether the terminal is too small to render the full UI.
    #[must_use]
    pub fn too_small(cols: u16, rows: u16) -> bool {
        cols < MIN_COLS || rows < MIN_ROWS
    }

    /// Whether the "0 conflicts 🎉" celebration is showing.
    #[must_use]
    pub fn celebrating(&self) -> bool {
        self.celebrate_ticks > 0
    }

    /// The stream lines matching the active filter (all when none/empty).
    #[must_use]
    pub fn filtered_events(&self) -> Vec<&StreamLine> {
        match self.filter.as_deref().filter(|f| !f.is_empty()) {
            None => self.events.iter().collect(),
            Some(needle) => self
                .events
                .iter()
                .filter(|l| l.path().is_some_and(|p| p.contains(needle)))
                .collect(),
        }
    }

    /// The visible unresolved conflicts (excluding optimistically-resolved ids),
    /// in `(index-into-this-vec, row)` order — newest first.
    #[must_use]
    pub fn visible_conflicts(&self) -> Vec<&ConflictRow> {
        self.conflicts
            .iter()
            .filter(|c| !self.pending_resolved.contains(&c.id))
            .collect()
    }

    /// Build the flattened visible-row list for the conflict pane: standalone
    /// conflicts first (newest first), then a collapsible adoption group.
    #[must_use]
    pub fn vis_rows(&self) -> Vec<VisRow> {
        let visible = self.visible_conflicts();
        let mut rows = Vec::new();
        let mut group_children = Vec::new();
        for (i, c) in visible.iter().enumerate() {
            if self.adopted_ids.contains(&c.id) {
                group_children.push(i);
            } else {
                rows.push(VisRow::Conflict(i));
            }
        }
        if !group_children.is_empty() {
            rows.push(VisRow::GroupHeader);
            if !self.group_collapsed {
                for i in group_children {
                    rows.push(VisRow::GroupChild(i));
                }
            }
        }
        rows
    }

    /// The number of conflicts in the adoption group (visible or not by
    /// collapse). Zero when there is no group.
    #[must_use]
    pub fn group_size(&self) -> usize {
        self.visible_conflicts()
            .iter()
            .filter(|c| self.adopted_ids.contains(&c.id))
            .count()
    }

    /// The conflict currently under the selection, if any.
    #[must_use]
    pub fn selected_conflict(&self) -> Option<&ConflictRow> {
        let rows = self.vis_rows();
        let visible = self.visible_conflicts();
        match rows.get(self.sel)? {
            VisRow::Conflict(i) | VisRow::GroupChild(i) => visible.get(*i).copied(),
            // A header's "representative" is its first child (drives the diff).
            VisRow::GroupHeader => rows
                .iter()
                .skip(self.sel + 1)
                .find_map(|r| match r {
                    VisRow::GroupChild(i) => visible.get(*i).copied(),
                    _ => None,
                })
                // When collapsed there is no child row; find the first adopted.
                .or_else(|| {
                    visible
                        .iter()
                        .copied()
                        .find(|c| self.adopted_ids.contains(&c.id))
                }),
        }
    }

    /// Whether the selection is on the adoption group header.
    #[must_use]
    pub fn on_group_header(&self) -> bool {
        matches!(self.vis_rows().get(self.sel), Some(VisRow::GroupHeader))
    }

    /// The picker paths matching the current filter (all when the filter is
    /// empty), in the store's newest-first order.
    #[must_use]
    pub fn filtered_paths(&self) -> Vec<&PathRow> {
        if self.picker_filter.is_empty() {
            self.history_paths.iter().collect()
        } else {
            self.history_paths
                .iter()
                .filter(|p| p.path.contains(&self.picker_filter))
                .collect()
        }
    }

    /// The path row currently under the picker selection, if any.
    #[must_use]
    pub fn selected_path(&self) -> Option<&PathRow> {
        self.filtered_paths().get(self.picker_sel).copied()
    }

    /// The version row currently under the timeline selection, if any.
    #[must_use]
    pub fn selected_version(&self) -> Option<&VersionRow> {
        self.timeline.get(self.timeline_sel)
    }

    /// The `(from, to)` version-id pair the timeline diff pane should show: the
    /// marked version against the selected one when a mark is set, otherwise the
    /// selected version against its predecessor (the next-older one). `None` when
    /// there is nothing to compare (the oldest version with no mark, or an empty
    /// timeline).
    #[must_use]
    pub fn diff_pair(&self) -> Option<(i64, i64)> {
        let sel = self.selected_version()?.id;
        if let Some(mark) = self.mark {
            return Some((mark, sel));
        }
        // Predecessor: the next-older version (timeline is newest first).
        let prev = self.timeline.get(self.timeline_sel + 1)?;
        Some((prev.id, sel))
    }

    fn enqueue(&mut self, req: CtlRequest) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.outbox.push(OutCommand { seq, req });
        seq
    }

    /// Enqueue a resolve verdict, optimistically hide the row, and remember the
    /// `seq → id` mapping so a failure rolls back exactly that row.
    fn enqueue_resolve(&mut self, id: i64, verdict: Verdict) {
        self.pending_resolved.insert(id);
        let seq = self.enqueue(CtlRequest::Resolve { id, verdict });
        self.pending_cmds.insert(seq, id);
    }

    fn set_flash(&mut self, msg: impl Into<String>) {
        self.flash = Some(msg.into());
        self.flash_ticks = FLASH_TICKS;
    }
}

// ---- the reducer ----------------------------------------------------------

/// The one pure transition. Every interaction routes through here.
#[must_use]
pub fn update(mut model: Model, msg: Msg) -> Model {
    match msg {
        Msg::Event(event) => ingest_event(&mut model, event),
        Msg::Tick => tick(&mut model),
        Msg::Cmd(outcome) => ingest_cmd(&mut model, outcome),
        Msg::Key(key) => key_press(&mut model, key),
    }
    model
}

fn ingest_event(model: &mut Model, event: Event) {
    match &event {
        Event::Connected {
            peer_name,
            peer_addr,
        } => {
            model.connected = true;
            if peer_name.is_some() {
                model.peer_name.clone_from(peer_name);
            }
            if peer_addr.is_some() {
                model.peer_addr.clone_from(peer_addr);
            }
            push_line(model, event);
        }
        Event::Disconnected => {
            model.connected = false;
            push_line(model, event);
        }
        Event::Transfer { path, done, total } => {
            update_transfer(model, path.clone(), *done, *total);
        }
        Event::Heartbeat {
            last_sync_ms_ago,
            unresolved_conflicts,
            paused,
        } => {
            model.unresolved = *unresolved_conflicts;
            // Authoritative shared pause state: keep every attached client in
            // sync (a toggle elsewhere, or our own optimistic flip, converges).
            model.paused = *paused;
            if let Some(ago) = last_sync_ms_ago {
                model.last_sync_wall_ms = Some(model.now_ms.saturating_sub(*ago));
            }
        }
        Event::Paused => {
            model.paused = true;
            push_line(model, event);
        }
        Event::Resumed => {
            model.paused = false;
            push_line(model, event);
        }
        Event::Conflict { id, adopted, .. } => {
            if *adopted {
                if let Some(id) = id {
                    model.adopted_ids.insert(*id);
                }
            }
            // A fresh conflict landed; if we're in the center, re-sync the list.
            if model.screen == Screen::Conflicts {
                model.enqueue(CtlRequest::ConflictsList);
            }
            push_line(model, event);
        }
        Event::Synced { path, .. } | Event::Sent { path, .. } => {
            // A completed transfer's file arrived; clear any pinned progress.
            model.transfers.retain(|t| &t.path != path);
            model.synced_total += 1;
            push_line(model, event);
        }
        Event::Removed { .. } | Event::Note { .. } | Event::Error { .. } | Event::Lagged => {
            push_line(model, event);
        }
    }
}

fn push_line(model: &mut Model, event: Event) {
    model.events.push(StreamLine { event });
    if model.events.len() > MAX_EVENTS {
        let overflow = model.events.len() - MAX_EVENTS;
        model.events.drain(0..overflow);
    }
    if model.follow {
        model.scroll = 0;
    } else {
        model.new_activity = true;
    }
}

fn update_transfer(model: &mut Model, path: String, done: u64, total: u64) {
    let complete = total > 0 && done >= total;
    if complete {
        model.transfers.retain(|t| t.path != path);
        return;
    }
    if let Some(t) = model.transfers.iter_mut().find(|t| t.path == path) {
        t.done = done;
        t.total = total;
    } else {
        model.transfers.push(Transfer { path, done, total });
    }
}

fn tick(model: &mut Model) {
    // `now_ms` is authoritative wall time, stamped by the shell before every
    // message (a tick fires every `TICK_MS` and triggers a redraw, so recency
    // stays live without the reducer touching a clock). Ticks only drive the
    // count-based transient chrome below.
    if model.flash_ticks > 0 {
        model.flash_ticks -= 1;
        if model.flash_ticks == 0 {
            model.flash = None;
        }
    }
    if model.celebrate_ticks > 0 {
        model.celebrate_ticks -= 1;
        if model.celebrate_ticks == 0 {
            model.screen = Screen::Main;
        }
    }
}

fn ingest_cmd(model: &mut Model, outcome: CmdOutcome) {
    match outcome.result {
        Ok(CmdReply::Conflicts(rows)) => {
            model.conflicts = rows;
            // Drop optimistic markers the server no longer lists (idempotent).
            let live: HashSet<i64> = model.conflicts.iter().map(|c| c.id).collect();
            model.pending_resolved.retain(|id| live.contains(id));
            // A fresh list is a new baseline: the single-level undo no longer
            // has a well-defined target (UX-V2 §3b — undo is the LAST verdict
            // only, cleared on a list refresh).
            model.undo = None;
            clamp_selection(model);
            ensure_diff_loaded(model);
        }
        Ok(CmdReply::Show { id, detail }) => {
            model.diff_requested.remove(&id);
            model.diffs.insert(id, detail);
        }
        Ok(CmdReply::Resolved) => {
            // Optimistic UI already advanced; just clear the in-flight marker.
            model.pending_cmds.remove(&outcome.seq);
            model.resolved_total += 1;
        }
        Ok(CmdReply::HistoryPaths(rows)) => {
            model.history_paths = rows;
            clamp_picker(model);
        }
        Ok(CmdReply::HistoryLog { path, versions }) => {
            // Ignore a stale reply for a path we are no longer viewing.
            if model.history_path.as_deref() == Some(path.as_str()) {
                model.timeline = versions;
                model.timeline_sel = 0;
                model.mark = None;
                ensure_version_diff_loaded(model);
            }
        }
        Ok(CmdReply::VersionDiff { from, to, detail }) => {
            model.version_diff_requested.remove(&(from, to));
            model.version_diffs.insert((from, to), detail);
        }
        Ok(CmdReply::Restored {
            path,
            version,
            size,
        }) => {
            // The optimistic flash was set at confirm time; refresh it with the
            // authoritative size the session reported.
            model.set_flash(format!(
                "restored {path} to #{version} ({}) — syncing to peer",
                crate::history_cmd::human_size(size)
            ));
        }
        Ok(CmdReply::Unresolved) => {
            // The undo's list refresh (enqueued alongside) surfaces the reopened
            // conflict; nothing else to do here.
        }
        Ok(CmdReply::PauseState(paused)) => {
            // Authoritative confirmation of the toggle (the optimistic flip
            // already advanced the UI); heartbeats keep it in sync thereafter.
            model.paused = paused;
        }
        Err(e) => {
            // Surface the failure. If it correlates to an optimistic resolve,
            // roll back exactly the row it hid and re-sync the list; a history
            // command's failure just flashes (no optimistic state to unwind).
            model.set_flash(format!("command failed: {e}"));
            if let Some(id) = model.pending_cmds.remove(&outcome.seq) {
                model.pending_resolved.remove(&id);
                model.enqueue(CtlRequest::ConflictsList);
            }
        }
    }
}

/// Clamp the picker selection into the filtered list after it changes.
fn clamp_picker(model: &mut Model) {
    let len = model.filtered_paths().len();
    if len == 0 {
        model.picker_sel = 0;
    } else if model.picker_sel >= len {
        model.picker_sel = len - 1;
    }
}

/// Request the timeline's current diff pair if it is not cached or already in
/// flight (keeps the diff pane in sync with the selection/mark).
fn ensure_version_diff_loaded(model: &mut Model) {
    if model.screen != Screen::HistoryTimeline {
        return;
    }
    let (Some(path), Some((from, to))) = (model.history_path.clone(), model.diff_pair()) else {
        return;
    };
    if model.version_diffs.contains_key(&(from, to))
        || model.version_diff_requested.contains(&(from, to))
    {
        return;
    }
    model.version_diff_requested.insert((from, to));
    model.enqueue(CtlRequest::VersionDiff { path, from, to });
}

fn key_press(model: &mut Model, key: Key) {
    // Overlays and modals capture input first.
    if model.help {
        if matches!(key, Key::Esc | Key::Char('?' | 'q')) {
            model.help = false;
        }
        return;
    }
    if model.modal == Some(Modal::StopConfirm) {
        match key {
            Key::Enter | Key::Char('y') => {
                model.stopping = true;
                model.quit = true;
                model.modal = None;
            }
            Key::Esc | Key::Char('n' | 'q') => model.modal = None,
            _ => {}
        }
        return;
    }
    if let Some(Modal::AckAll { count }) = model.modal.clone() {
        match key {
            Key::Enter | Key::Char('y') => {
                confirm_ack_all(model, count);
                model.modal = None;
            }
            Key::Esc | Key::Char('n') => model.modal = None,
            _ => {}
        }
        return;
    }
    if let Some(Modal::RestoreConfirm { .. }) = &model.modal {
        match key {
            Key::Enter | Key::Char('y') => confirm_restore(model),
            Key::Esc | Key::Char('n') => model.modal = None,
            _ => {}
        }
        return;
    }
    match model.screen {
        Screen::Main => main_key(model, key),
        Screen::Conflicts => conflict_key(model, key),
        Screen::HistoryPicker => picker_key(model, key),
        Screen::HistoryTimeline => timeline_key(model, key),
    }
}

/// `q`: on a foreground-started session, quitting means stopping the sync —
/// confirm first (UX-V2 §1). Attached mode just detaches (the session runs on).
fn request_quit(model: &mut Model) {
    if model.foreground {
        model.modal = Some(Modal::StopConfirm);
    } else {
        model.quit = true;
    }
}

/// `space`: toggle the session pause (docs/SPEC.md §13), from either the main
/// screen or the conflict center. The UI flips optimistically and flashes; the
/// reply and subsequent heartbeats confirm the authoritative shared state, so
/// multiple attached clients converge.
fn toggle_pause(model: &mut Model) {
    let want = !model.paused;
    model.paused = want;
    model.enqueue(if want {
        CtlRequest::Pause
    } else {
        CtlRequest::Resume
    });
    model.set_flash(if want {
        "paused — space to resume"
    } else {
        "resumed — draining queued changes"
    });
}

fn main_key(model: &mut Model, key: Key) {
    if model.filter_editing {
        filter_key(model, key);
        return;
    }
    match key {
        Key::Char('q') => request_quit(model),
        Key::Char('d') => model.quit = true,
        Key::Char('?') => model.help = true,
        Key::Char('c') => enter_conflicts(model),
        Key::Char('h') => enter_history_picker(model),
        Key::Char(' ') => toggle_pause(model),
        Key::Char('/') => {
            model.filter_editing = true;
            if model.filter.is_none() {
                model.filter = Some(String::new());
            }
        }
        Key::Esc => {
            model.filter = None;
        }
        Key::Up => scroll_up(model, 1),
        Key::PageUp => scroll_up(model, 10),
        Key::Down => scroll_down(model, 1),
        Key::PageDown => scroll_down(model, 10),
        Key::End | Key::Char('G') => refollow(model),
        _ => {}
    }
}

fn filter_key(model: &mut Model, key: Key) {
    match key {
        Key::Char(c) => {
            model.filter.get_or_insert_with(String::new).push(c);
        }
        Key::Backspace => {
            if let Some(f) = model.filter.as_mut() {
                f.pop();
            }
        }
        Key::Enter => model.filter_editing = false,
        Key::Esc => {
            model.filter_editing = false;
            model.filter = None;
        }
        _ => {}
    }
}

fn scroll_up(model: &mut Model, step: usize) {
    model.follow = false;
    let max = model.filtered_events().len().saturating_sub(1);
    model.scroll = (model.scroll + step).min(max);
}

fn scroll_down(model: &mut Model, step: usize) {
    model.scroll = model.scroll.saturating_sub(step);
    if model.scroll == 0 {
        refollow(model);
    }
}

fn refollow(model: &mut Model) {
    model.follow = true;
    model.scroll = 0;
    model.new_activity = false;
}

fn enter_conflicts(model: &mut Model) {
    model.screen = Screen::Conflicts;
    model.sel = 0;
    model.enqueue(CtlRequest::ConflictsList);
}

/// How many recently-versioned paths the picker fetches.
const HISTORY_PATHS_LIMIT: usize = 200;
/// How many versions of a path the timeline fetches.
const HISTORY_LOG_LIMIT: usize = 500;

/// Enter the history browser's path picker (`h` from the main screen): fetch the
/// recently-versioned paths and open the filterable list (UX-V2 §3, TUI v2).
fn enter_history_picker(model: &mut Model) {
    model.screen = Screen::HistoryPicker;
    model.picker_filter.clear();
    model.picker_sel = 0;
    model.enqueue(CtlRequest::HistoryPaths {
        limit: HISTORY_PATHS_LIMIT,
    });
}

/// The path picker: type-to-filter (like the main screen's `/`), Up/Down to
/// move, Enter to open the timeline, Esc back to the main screen.
fn picker_key(model: &mut Model, key: Key) {
    match key {
        Key::Esc => model.screen = Screen::Main,
        Key::Enter => open_timeline(model),
        Key::Up => picker_move(model, -1),
        Key::Down => picker_move(model, 1),
        Key::Backspace => {
            model.picker_filter.pop();
            clamp_picker(model);
        }
        // A finder captures every printable key as filter input.
        Key::Char(c) => {
            model.picker_filter.push(c);
            clamp_picker(model);
        }
        _ => {}
    }
}

fn picker_move(model: &mut Model, delta: i32) {
    let len = model.filtered_paths().len();
    if len == 0 {
        model.picker_sel = 0;
        return;
    }
    let cur = i32::try_from(model.picker_sel).unwrap_or(0);
    let max = i32::try_from(len - 1).unwrap_or(0);
    model.picker_sel = usize::try_from((cur + delta).clamp(0, max)).unwrap_or(0);
}

/// Open the selected path's version timeline: fetch its log and reset the
/// timeline selection/mark.
fn open_timeline(model: &mut Model) {
    let Some(row) = model.selected_path() else {
        return;
    };
    let path = row.path.clone();
    model.history_path = Some(path.clone());
    model.screen = Screen::HistoryTimeline;
    model.timeline.clear();
    model.timeline_sel = 0;
    model.mark = None;
    model.enqueue(CtlRequest::HistoryLog {
        path,
        limit: HISTORY_LOG_LIMIT,
    });
}

/// The version timeline: j/k to move (the diff pane live-shows the selected
/// version against its predecessor), `m` to mark for a two-version compare, `r`
/// to restore, Esc to clear the mark or walk back to the picker.
fn timeline_key(model: &mut Model, key: Key) {
    match key {
        Key::Esc => {
            if model.mark.is_some() {
                model.mark = None;
                ensure_version_diff_loaded(model);
            } else {
                model.screen = Screen::HistoryPicker;
            }
        }
        Key::Char('?') => model.help = true,
        Key::Char('q') => request_quit(model),
        Key::Char('d') => model.quit = true,
        Key::Char('j') | Key::Down => timeline_move(model, 1),
        Key::Char('k') | Key::Up => timeline_move(model, -1),
        Key::Char('m') => toggle_mark(model),
        Key::Char('r') => open_restore_confirm(model),
        _ => {}
    }
}

fn timeline_move(model: &mut Model, delta: i32) {
    let len = model.timeline.len();
    if len == 0 {
        model.timeline_sel = 0;
        return;
    }
    let cur = i32::try_from(model.timeline_sel).unwrap_or(0);
    let max = i32::try_from(len - 1).unwrap_or(0);
    model.timeline_sel = usize::try_from((cur + delta).clamp(0, max)).unwrap_or(0);
    ensure_version_diff_loaded(model);
}

/// Toggle the compare mark: set it to the selected version, or clear it if one
/// is already set (`m` again clears; so does Esc — UX-V2 §3).
fn toggle_mark(model: &mut Model) {
    if model.mark.is_some() {
        model.mark = None;
    } else if let Some(v) = model.selected_version() {
        model.mark = Some(v.id);
    }
    ensure_version_diff_loaded(model);
}

/// Open the restore-confirm modal for the selected version.
fn open_restore_confirm(model: &mut Model) {
    let (Some(path), Some(v)) = (model.history_path.clone(), model.selected_version()) else {
        return;
    };
    model.modal = Some(Modal::RestoreConfirm {
        path,
        version: v.id,
        size: v.size.unwrap_or(0),
        wall_ms: v.wall_ms,
        deleted: !v.present,
    });
}

/// Confirm a restore: enqueue the command and flash optimistically (the session
/// ships the restored bytes to the peer — nothing else to do). UX-V2 §3.
fn confirm_restore(model: &mut Model) {
    let Some(Modal::RestoreConfirm { path, version, .. }) = model.modal.take() else {
        return;
    };
    model.enqueue(CtlRequest::Restore {
        path: path.clone(),
        version,
    });
    model.set_flash(format!("restored {path} to #{version} — syncing to peer"));
}

fn conflict_key(model: &mut Model, key: Key) {
    match key {
        Key::Char('c') | Key::Esc => {
            model.screen = Screen::Main;
        }
        Key::Char('?') => model.help = true,
        Key::Char('q') => request_quit(model),
        Key::Char('d') => model.quit = true,
        Key::Char('j') | Key::Down => move_sel(model, 1),
        Key::Char('k') | Key::Up => move_sel(model, -1),
        Key::Enter => {
            if model.on_group_header() {
                model.group_collapsed = !model.group_collapsed;
                clamp_selection(model);
            } else {
                verdict(model, Verdict::Keep);
            }
        }
        Key::Char('h') | Key::Left => {
            if model.on_group_header() {
                model.group_collapsed = true;
                clamp_selection(model);
            }
        }
        Key::Char('l') | Key::Right => {
            if model.on_group_header() {
                model.group_collapsed = false;
                clamp_selection(model);
            }
        }
        Key::Char('t') => verdict(model, Verdict::Take),
        Key::Char('b') => verdict(model, Verdict::Both),
        // `space` toggles the session pause here too (docs/SPEC.md §13),
        // consistent with the main screen; selection advances with j/k.
        Key::Char(' ') => toggle_pause(model),
        Key::Char('a') => {
            let count = model.visible_conflicts().len();
            if count > 0 {
                model.modal = Some(Modal::AckAll { count });
            }
        }
        Key::Char('u') => undo(model),
        _ => {}
    }
}

/// Reverse the last conflict verdict (UX-V2 §3b real undo). Single-level: the
/// plan was captured at verdict time. Every version is in history, so a
/// resolution is itself reversible — the property git conflict UX cannot offer.
fn undo(model: &mut Model) {
    let Some(plan) = model.undo.take() else {
        model.set_flash("nothing to undo");
        return;
    };
    match plan.verdict {
        Verdict::Keep => {
            model.enqueue(CtlRequest::ConflictUnresolve {
                id: plan.conflict_id,
            });
        }
        Verdict::Take => {
            // Restore the version that was on disk before the take, then reopen.
            if let Some(version) = plan.pre_version {
                model.enqueue(CtlRequest::Restore {
                    path: plan.path.clone(),
                    version,
                });
            }
            model.enqueue(CtlRequest::ConflictUnresolve {
                id: plan.conflict_id,
            });
        }
        Verdict::Both => {
            // There is no delete command; the `<path>.theirs` sidecar it wrote
            // stays. Reopen the conflict and be honest about the leftover file.
            model.enqueue(CtlRequest::ConflictUnresolve {
                id: plan.conflict_id,
            });
        }
    }
    // Un-hide the optimistically-resolved row and re-sync so the reopened
    // conflict reappears in the list.
    model.pending_resolved.remove(&plan.conflict_id);
    model.enqueue(CtlRequest::ConflictsList);
    model.set_flash(undo_flash(&plan));
}

/// The footer flash for an executed undo: the CLI equivalent of what it ran,
/// honest about the `both` sidecar that no delete command can remove.
#[must_use]
fn undo_flash(plan: &UndoPlan) -> String {
    match plan.verdict {
        Verdict::Keep => format!("undid keep — reopened conflict #{}", plan.conflict_id),
        Verdict::Take => match plan.pre_version {
            Some(v) => format!(
                "undid take — = tomo restore {} --version {v} · reopened #{}",
                plan.path, plan.conflict_id
            ),
            None => format!("undid take — reopened conflict #{}", plan.conflict_id),
        },
        Verdict::Both => format!(
            "undid keep-both — reopened #{}; {}.theirs remains (delete it by hand)",
            plan.conflict_id, plan.path
        ),
    }
}

fn move_sel(model: &mut Model, delta: i32) {
    let len = model.vis_rows().len();
    if len == 0 {
        model.sel = 0;
        return;
    }
    let cur = i32::try_from(model.sel).unwrap_or(0);
    let max = i32::try_from(len - 1).unwrap_or(0);
    model.sel = usize::try_from((cur + delta).clamp(0, max)).unwrap_or(0);
    ensure_diff_loaded(model);
}

fn clamp_selection(model: &mut Model) {
    let len = model.vis_rows().len();
    if len == 0 {
        model.sel = 0;
    } else if model.sel >= len {
        model.sel = len - 1;
    }
    ensure_diff_loaded(model);
}

/// Dispatch a verdict on the current selection (a single conflict, or the whole
/// adoption group when the header is selected). Optimistically hides the
/// resolved rows and auto-advances (UX-V2 §3b, Gmail-style).
fn verdict(model: &mut Model, v: Verdict) {
    if model.on_group_header() {
        // A group verdict resolves many conflicts at once; single-level undo has
        // no clean multi-target, so a group verdict clears any pending undo.
        let ids: Vec<i64> = model
            .visible_conflicts()
            .iter()
            .filter(|c| model.adopted_ids.contains(&c.id))
            .map(|c| c.id)
            .collect();
        if ids.is_empty() {
            return;
        }
        model.undo = None;
        for id in ids {
            model.enqueue_resolve(id, v);
        }
        after_resolve(model);
        return;
    }
    // A single-conflict verdict: capture its undo plan AT VERDICT TIME, before
    // the row is optimistically hidden (UX-V2 §3b real undo). The pre-take
    // on-disk version is the winner id from the cached conflict detail.
    let Some(conflict) = model.selected_conflict() else {
        return;
    };
    let id = conflict.id;
    let path = conflict.path.clone();
    let pre_version = if v == Verdict::Take {
        model.diffs.get(&id).map(|d| d.winner_version)
    } else {
        None
    };
    model.undo = Some(UndoPlan {
        conflict_id: id,
        path,
        verdict: v,
        pre_version,
    });
    model.enqueue_resolve(id, v);
    after_resolve(model);
}

fn confirm_ack_all(model: &mut Model, _count: usize) {
    // A mass-ack resolves everything; there is no single verdict to undo.
    model.undo = None;
    let ids: Vec<i64> = model.visible_conflicts().iter().map(|c| c.id).collect();
    for id in ids {
        model.enqueue_resolve(id, Verdict::Keep);
    }
    after_resolve(model);
}

/// After a verdict empties or shrinks the list: clamp the selection and, when
/// the last conflict is gone, celebrate briefly before returning to the stream.
fn after_resolve(model: &mut Model) {
    if model.visible_conflicts().is_empty() {
        model.celebrate_ticks = CELEBRATE_TICKS;
        model.sel = 0;
    } else {
        clamp_selection(model);
    }
}

/// Request the selected conflict's diff if it is not cached or already in
/// flight (keeps the right pane in sync with the selection).
fn ensure_diff_loaded(model: &mut Model) {
    if model.screen != Screen::Conflicts {
        return;
    }
    let Some(id) = model.selected_conflict().map(|c| c.id) else {
        return;
    };
    if model.diffs.contains_key(&id) || model.diff_requested.contains(&id) {
        return;
    }
    model.diff_requested.insert(id);
    model.enqueue(CtlRequest::ConflictShow { id });
}

// ---- display helpers (pure) -----------------------------------------------

/// Format a millisecond age as "just now" / "Ns ago" / "Nm ago" / "Nh ago" /
/// "Nd ago" for the status line and conflict rows.
#[must_use]
pub fn format_ago(ms: u64) -> String {
    let secs = ms / 1000;
    if secs == 0 {
        "just now".to_owned()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// The status line's "last sync …" text, or `None` when nothing has synced yet.
#[must_use]
pub fn last_sync_text(model: &Model) -> Option<String> {
    let wall = model.last_sync_wall_ms?;
    Some(format!(
        "last sync {}",
        format_ago(model.now_ms.saturating_sub(wall))
    ))
}

/// The CLI echo for the selected conflict-center action (the default keep
/// verdict), mirroring the scriptable surface (UX-V2 §3b footer).
#[must_use]
pub fn cli_echo(model: &Model) -> Option<String> {
    if model.on_group_header() {
        let n = model.group_size();
        return Some(format!("= keep all {n} adopted files"));
    }
    let id = model.selected_conflict()?.id;
    Some(format!(
        "= tomo conflicts resolve {id} {}",
        Verdict::Keep.cli_flag()
    ))
}

/// The CLI echo for the history timeline's selected version (the magit footer
/// trick): the exact `tomo restore` command the `r` key would run (UX-V2 §3).
#[must_use]
pub fn restore_echo(model: &Model) -> Option<String> {
    let path = model.history_path.as_deref()?;
    let v = model.selected_version()?;
    Some(format!("= tomo restore {path} --version {}", v.id))
}

/// The timeline diff-pane header: "comparing #M ↔ #S" when a compare mark is set
/// (UX-V2 §3), otherwise the predecessor-comparison label, or `None` when there
/// is nothing to compare against.
#[must_use]
pub fn compare_header(model: &Model) -> Option<String> {
    let sel = model.selected_version()?.id;
    match model.mark {
        Some(mark) => Some(format!("comparing #{mark} ↔ #{sel}")),
        None => model
            .timeline
            .get(model.timeline_sel + 1)
            .map(|prev| format!("#{} → #{sel} (vs. predecessor)", prev.id)),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::field_reassign_with_default
)]
mod tests {
    use super::*;
    use crate::ctl::proto::ConflictSide;

    fn ev(model: Model, event: Event) -> Model {
        update(model, Msg::Event(event))
    }
    fn key(model: Model, k: Key) -> Model {
        update(model, Msg::Key(k))
    }
    fn synced(path: &str) -> Event {
        Event::Synced {
            path: path.to_owned(),
            size: 1,
        }
    }

    // ---- stream: follow / scrollback / nudge ------------------------------

    #[test]
    fn follows_tail_by_default() {
        let mut m = Model::default();
        for i in 0..5 {
            m = ev(m, synced(&format!("f{i}.txt")));
        }
        assert!(m.follow);
        assert_eq!(m.scroll, 0);
        assert!(!m.new_activity);
        assert_eq!(m.filtered_events().len(), 5);
    }

    #[test]
    fn scrollback_leaves_follow_and_nudges_on_new_event() {
        let mut m = Model::default();
        for i in 0..20 {
            m = ev(m, synced(&format!("f{i}.txt")));
        }
        m = key(m, Key::Up);
        assert!(!m.follow, "arrow-up leaves follow mode");
        assert!(m.scroll >= 1);
        assert!(!m.new_activity);
        m = ev(m, synced("new.txt"));
        assert!(m.new_activity, "a new event while scrolled shows the nudge");
    }

    #[test]
    fn pageup_then_end_refollows() {
        let mut m = Model::default();
        for i in 0..40 {
            m = ev(m, synced(&format!("f{i}.txt")));
        }
        m = key(m, Key::PageUp);
        assert!(!m.follow);
        assert!(m.scroll >= 10);
        m = ev(m, synced("more.txt"));
        assert!(m.new_activity);
        m = key(m, Key::End);
        assert!(m.follow);
        assert_eq!(m.scroll, 0);
        assert!(!m.new_activity, "End clears the nudge");
    }

    #[test]
    fn scroll_down_to_bottom_refollows() {
        let mut m = Model::default();
        for i in 0..20 {
            m = ev(m, synced(&format!("f{i}.txt")));
        }
        m = key(m, Key::Up); // scroll = 1, follow off
        m = key(m, Key::Down); // back to bottom
        assert!(m.follow);
        assert_eq!(m.scroll, 0);
    }

    #[test]
    fn g_key_refollows() {
        let mut m = Model::default();
        for i in 0..20 {
            m = ev(m, synced(&format!("f{i}.txt")));
        }
        m = key(m, Key::PageUp);
        m = key(m, Key::Char('G'));
        assert!(m.follow);
    }

    // ---- filter editing ---------------------------------------------------

    #[test]
    fn filter_editing_narrows_by_path() {
        let mut m = Model::default();
        m = ev(m, synced("src/train.py"));
        m = ev(m, synced("assets/logo.png"));
        m = ev(m, synced("src/config.yaml"));
        m = key(m, Key::Char('/'));
        assert!(m.filter_editing);
        for c in "src".chars() {
            m = key(m, Key::Char(c));
        }
        assert_eq!(m.filter.as_deref(), Some("src"));
        let shown: Vec<_> = m.filtered_events().iter().map(|l| l.path()).collect();
        assert_eq!(shown, vec![Some("src/train.py"), Some("src/config.yaml")]);
        // Enter commits (keeps the filter, exits editing).
        m = key(m, Key::Enter);
        assert!(!m.filter_editing);
        assert_eq!(m.filter.as_deref(), Some("src"));
    }

    #[test]
    fn filter_backspace_and_esc_clear() {
        let mut m = Model::default();
        m = key(m, Key::Char('/'));
        for c in "ab".chars() {
            m = key(m, Key::Char(c));
        }
        m = key(m, Key::Backspace);
        assert_eq!(m.filter.as_deref(), Some("a"));
        m = key(m, Key::Esc);
        assert!(!m.filter_editing);
        assert_eq!(m.filter, None);
    }

    #[test]
    fn esc_clears_committed_filter_on_main() {
        let mut m = Model::default();
        m = key(m, Key::Char('/'));
        m = key(m, Key::Char('x'));
        m = key(m, Key::Enter);
        assert_eq!(m.filter.as_deref(), Some("x"));
        m = key(m, Key::Esc);
        assert_eq!(m.filter, None);
    }

    // ---- transfers --------------------------------------------------------

    #[test]
    fn transfer_add_update_and_complete() {
        let mut m = Model::default();
        m = ev(
            m,
            Event::Transfer {
                path: "big.bin".to_owned(),
                done: 10,
                total: 100,
            },
        );
        assert_eq!(m.transfers.len(), 1);
        assert_eq!(m.transfers[0].done, 10);
        m = ev(
            m,
            Event::Transfer {
                path: "big.bin".to_owned(),
                done: 60,
                total: 100,
            },
        );
        assert_eq!(m.transfers[0].done, 60);
        // Completion removes it (zero-height pinned zone when idle).
        m = ev(
            m,
            Event::Transfer {
                path: "big.bin".to_owned(),
                done: 100,
                total: 100,
            },
        );
        assert!(m.transfers.is_empty());
        // Transfers are never stream log lines.
        assert!(m.events.is_empty());
    }

    #[test]
    fn synced_clears_pending_transfer() {
        let mut m = Model::default();
        m = ev(
            m,
            Event::Transfer {
                path: "big.bin".to_owned(),
                done: 50,
                total: 100,
            },
        );
        m = ev(m, synced("big.bin"));
        assert!(m.transfers.is_empty());
        assert_eq!(m.filtered_events().len(), 1);
    }

    // ---- heartbeat: badge + last-sync formatting --------------------------

    #[test]
    fn heartbeat_sets_badge_and_last_sync_text() {
        let mut m = Model::default();
        m.now_ms = 1_000_000;
        m = ev(
            m,
            Event::Heartbeat {
                last_sync_ms_ago: Some(2_000),
                unresolved_conflicts: 1,
                paused: false,
            },
        );
        assert_eq!(m.unresolved, 1);
        assert_eq!(last_sync_text(&m).as_deref(), Some("last sync 2s ago"));
        // The shell advances wall time (here simulated) so recency keeps
        // climbing without a new heartbeat — a tick just triggers the redraw.
        // last sync was anchored at now-2000 = 998_000; advance now to 1_002_000
        // → 4s elapsed since that sync.
        m.now_ms = 1_002_000;
        m = update(m, Msg::Tick);
        assert_eq!(last_sync_text(&m).as_deref(), Some("last sync 4s ago"));
    }

    // ---- pause / resume (docs/SPEC.md §13) --------------------------------

    #[test]
    fn space_toggles_pause_and_enqueues_the_command() {
        let mut m = Model::default();
        assert!(!m.paused);
        // First space: optimistic pause + a Pause command.
        m = key(m, Key::Char(' '));
        assert!(m.paused, "optimistically paused");
        assert!(m.outbox.iter().any(|c| c.req == CtlRequest::Pause));
        assert!(m.flash.as_deref().unwrap().contains("paused"));
        // Second space: optimistic resume + a Resume command.
        m.outbox.clear();
        m = key(m, Key::Char(' '));
        assert!(!m.paused, "optimistically resumed");
        assert!(m.outbox.iter().any(|c| c.req == CtlRequest::Resume));
    }

    #[test]
    fn space_toggles_pause_from_the_conflict_center_too() {
        let mut m = one_conflict_center();
        m.outbox.clear();
        assert!(!m.paused);
        m = key(m, Key::Char(' '));
        assert!(m.paused);
        assert!(m.outbox.iter().any(|c| c.req == CtlRequest::Pause));
    }

    #[test]
    fn paused_and_resumed_events_track_state() {
        let mut m = Model::default();
        m = ev(m, Event::Paused);
        assert!(m.paused);
        m = ev(m, Event::Resumed);
        assert!(!m.paused);
    }

    #[test]
    fn heartbeat_carries_authoritative_pause_state() {
        // A heartbeat reconciles a client that missed the paused event (or a
        // second attached client), so multiple clients stay consistent.
        let mut m = Model::default();
        m = ev(
            m,
            Event::Heartbeat {
                last_sync_ms_ago: None,
                unresolved_conflicts: 0,
                paused: true,
            },
        );
        assert!(m.paused, "heartbeat reasserts the shared pause state");
        m = ev(
            m,
            Event::Heartbeat {
                last_sync_ms_ago: None,
                unresolved_conflicts: 0,
                paused: false,
            },
        );
        assert!(!m.paused);
    }

    #[test]
    fn pause_reply_confirms_authoritative_state() {
        let mut m = Model::default();
        m.paused = false;
        let seq = m.enqueue(CtlRequest::Pause);
        m = update(
            m,
            Msg::Cmd(CmdOutcome {
                seq,
                result: Ok(CmdReply::PauseState(true)),
            }),
        );
        assert!(m.paused);
    }

    #[test]
    fn format_ago_units() {
        assert_eq!(format_ago(0), "just now");
        assert_eq!(format_ago(2_000), "2s ago");
        assert_eq!(format_ago(240_000), "4m ago");
        assert_eq!(format_ago(7_200_000), "2h ago");
        assert_eq!(format_ago(172_800_000), "2d ago");
    }

    #[test]
    fn connected_updates_peer_and_state() {
        let mut m = Model::default();
        m = ev(
            m,
            Event::Connected {
                peer_name: Some("vm8".to_owned()),
                peer_addr: Some("192.168.1.40".to_owned()),
            },
        );
        assert!(m.connected);
        assert_eq!(m.peer_name.as_deref(), Some("vm8"));
        assert_eq!(m.peer_addr.as_deref(), Some("192.168.1.40"));
        m = ev(m, Event::Disconnected);
        assert!(!m.connected);
    }

    // ---- conflict list ingest, adoption grouping, collapse ----------------

    fn conflicts_json(ids: &[(i64, &str, &str)]) -> Value {
        let arr: Vec<Value> = ids
            .iter()
            .map(|(id, path, worigin)| {
                json!({
                    "id": id,
                    "path": path,
                    "wall_unix_ms": id,
                    "resolved": false,
                    "winner": {"origin": worigin},
                    "loser": {"origin": "local"},
                })
            })
            .collect();
        Value::Array(arr)
    }

    fn deliver(model: Model, seq: u64, reply: CmdReply) -> Model {
        update(
            model,
            Msg::Cmd(CmdOutcome {
                seq,
                result: Ok(reply),
            }),
        )
    }

    #[test]
    fn ingest_conflicts_newest_first() {
        let m = Model::default();
        let rows = parse_conflicts(&conflicts_json(&[
            (1, "a.txt", "remote"),
            (3, "c.txt", "local"),
            (2, "b.txt", "remote"),
        ]));
        let m = deliver(m, 0, CmdReply::Conflicts(rows));
        let ids: Vec<i64> = m.conflicts.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![3, 2, 1], "newest (greatest wall) first");
        assert_eq!(m.conflicts[0].winner, Side::You);
        assert_eq!(m.conflicts[2].winner, Side::Peer);
    }

    #[test]
    fn adoption_grouping_and_collapse() {
        let mut m = Model::default();
        // Two adoption conflicts (ids 10, 11) plus one normal (id 5).
        for id in [10, 11] {
            m = ev(
                m,
                Event::Conflict {
                    id: Some(id),
                    path: format!("g{id}.txt"),
                    winner: ConflictSide::Peer,
                    adopted: true,
                },
            );
        }
        let rows = parse_conflicts(&conflicts_json(&[
            (5, "normal.txt", "remote"),
            (10, "g10.txt", "remote"),
            (11, "g11.txt", "remote"),
        ]));
        m.screen = Screen::Conflicts;
        m = deliver(m, 0, CmdReply::Conflicts(rows));
        // Expanded: normal conflict + header + 2 children.
        let vis = m.vis_rows();
        assert_eq!(vis.len(), 4);
        assert!(matches!(vis[0], VisRow::Conflict(_)));
        assert_eq!(vis[1], VisRow::GroupHeader);
        assert!(matches!(vis[2], VisRow::GroupChild(_)));
        assert_eq!(m.group_size(), 2);
        // Collapse the group by selecting the header and pressing h.
        m.sel = 1;
        m = key(m, Key::Char('h'));
        assert!(m.group_collapsed);
        assert_eq!(m.vis_rows().len(), 2, "collapsed hides the 2 children");
        // Expand again.
        m = key(m, Key::Char('l'));
        assert!(!m.group_collapsed);
        assert_eq!(m.vis_rows().len(), 4);
    }

    #[test]
    fn selection_moves_across_group_boundary() {
        let mut m = Model::default();
        m.screen = Screen::Conflicts;
        for id in [10, 11] {
            m = ev(
                m,
                Event::Conflict {
                    id: Some(id),
                    path: format!("g{id}.txt"),
                    winner: ConflictSide::Peer,
                    adopted: true,
                },
            );
        }
        let rows = parse_conflicts(&conflicts_json(&[
            (5, "normal.txt", "remote"),
            (10, "g10.txt", "remote"),
            (11, "g11.txt", "remote"),
        ]));
        m = deliver(m, 0, CmdReply::Conflicts(rows));
        m.sel = 0;
        // Move down through: conflict(5) -> header -> child -> child.
        m = key(m, Key::Char('j'));
        assert!(m.on_group_header());
        m = key(m, Key::Char('j'));
        assert!(matches!(m.vis_rows()[m.sel], VisRow::GroupChild(_)));
        // Down past the end clamps.
        m = key(m, Key::Char('j'));
        m = key(m, Key::Char('j'));
        assert_eq!(m.sel, m.vis_rows().len() - 1);
    }

    // ---- verdict dispatch + optimistic advance + rollback -----------------

    fn one_conflict_center() -> Model {
        let mut m = Model::default();
        m.screen = Screen::Conflicts;
        let rows = parse_conflicts(&conflicts_json(&[
            (7, "src/train.py", "remote"),
            (8, "src/config.yaml", "remote"),
        ]));
        deliver(m.clone(), 0, CmdReply::Conflicts(rows))
    }

    #[test]
    fn keep_verdict_enqueues_command_and_advances() {
        let mut m = one_conflict_center();
        m.outbox.clear();
        m.sel = 0; // id 8 is newest (wall 8), so index 0 is id 8.
        let selected = m.selected_conflict().unwrap().id;
        m = key(m, Key::Enter);
        // A resolve command for the selected id was enqueued.
        let resolves: Vec<_> = m
            .outbox
            .iter()
            .filter_map(|c| match &c.req {
                CtlRequest::Resolve { id, verdict } => Some((*id, *verdict)),
                _ => None,
            })
            .collect();
        assert_eq!(resolves, vec![(selected, Verdict::Keep)]);
        // Optimistically hidden and auto-advanced onto the remaining row.
        assert!(m.pending_resolved.contains(&selected));
        assert_eq!(m.visible_conflicts().len(), 1);
    }

    #[test]
    fn take_and_both_map_to_actions() {
        let mut m = one_conflict_center();
        m.outbox.clear();
        m = key(m, Key::Char('t'));
        assert_eq!(
            m.outbox[0].req,
            CtlRequest::Resolve {
                id: m.conflicts[0].id,
                verdict: Verdict::Take
            }
        );
        assert_eq!(
            CtlRequest::Resolve {
                id: 0,
                verdict: Verdict::Take
            }
            .to_json()["action"],
            "take"
        );
        assert_eq!(
            CtlRequest::Resolve {
                id: 0,
                verdict: Verdict::Both
            }
            .to_json()["action"],
            "both"
        );
    }

    #[test]
    fn command_failure_rolls_back_and_resyncs() {
        let mut m = one_conflict_center();
        m.outbox.clear();
        m = key(m, Key::Enter);
        let hidden = *m.pending_resolved.iter().next().unwrap();
        // Correlate the failure back to the exact dispatched resolve command.
        let resolve_seq = m
            .outbox
            .iter()
            .find_map(|c| match c.req {
                CtlRequest::Resolve { .. } => Some(c.seq),
                _ => None,
            })
            .unwrap();
        assert_eq!(m.visible_conflicts().len(), 1);
        // The dispatched resolve fails.
        m = update(
            m,
            Msg::Cmd(CmdOutcome {
                seq: resolve_seq,
                result: Err("busy".to_owned()),
            }),
        );
        assert!(!m.pending_resolved.contains(&hidden), "rolled back");
        assert_eq!(m.visible_conflicts().len(), 2, "row reappears");
        assert!(m.flash.as_deref().unwrap().contains("busy"));
        assert!(
            m.outbox.iter().any(|c| c.req == CtlRequest::ConflictsList),
            "re-syncs the list"
        );
    }

    #[test]
    fn ack_all_confirms_then_resolves_every_conflict() {
        let mut m = one_conflict_center();
        m.outbox.clear();
        m = key(m, Key::Char('a'));
        assert_eq!(m.modal, Some(Modal::AckAll { count: 2 }));
        // Cancel first.
        m = key(m, Key::Char('n'));
        assert_eq!(m.modal, None);
        assert!(m.pending_resolved.is_empty());
        // Now confirm.
        m = key(m, Key::Char('a'));
        m = key(m, Key::Enter);
        assert_eq!(m.modal, None);
        let resolves = m
            .outbox
            .iter()
            .filter(|c| {
                matches!(
                    c.req,
                    CtlRequest::Resolve {
                        verdict: Verdict::Keep,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(resolves, 2);
        assert!(m.celebrating(), "resolving to zero celebrates");
    }

    #[test]
    fn celebration_returns_to_stream() {
        let mut m = one_conflict_center();
        m = key(m, Key::Char('a'));
        m = key(m, Key::Enter);
        assert!(m.celebrating());
        assert_eq!(m.screen, Screen::Conflicts);
        for _ in 0..CELEBRATE_TICKS {
            m = update(m, Msg::Tick);
        }
        assert!(!m.celebrating());
        assert_eq!(m.screen, Screen::Main);
    }

    #[test]
    fn group_verdict_applies_to_all_children() {
        let mut m = Model::default();
        m.screen = Screen::Conflicts;
        for id in [10, 11] {
            m = ev(
                m,
                Event::Conflict {
                    id: Some(id),
                    path: format!("g{id}.txt"),
                    winner: ConflictSide::Peer,
                    adopted: true,
                },
            );
        }
        let rows = parse_conflicts(&conflicts_json(&[
            (10, "g10.txt", "remote"),
            (11, "g11.txt", "remote"),
        ]));
        m = deliver(m, 0, CmdReply::Conflicts(rows));
        m.outbox.clear();
        // Select the header (row 0 is the header since no standalone conflicts).
        m.sel = 0;
        assert!(m.on_group_header());
        m = key(m, Key::Char('t'));
        let takes = m
            .outbox
            .iter()
            .filter(|c| {
                matches!(
                    c.req,
                    CtlRequest::Resolve {
                        verdict: Verdict::Take,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(takes, 2, "a header verdict hits both children");
    }

    // ---- real undo (UX-V2 §3b): per-verdict plans, one level, disabled state --

    #[test]
    fn undo_is_disabled_when_nothing_to_undo() {
        let mut m = one_conflict_center();
        m.outbox.clear();
        m = key(m, Key::Char('u'));
        assert!(m.flash.as_deref().unwrap().contains("nothing to undo"));
        assert!(
            !m.outbox
                .iter()
                .any(|c| matches!(c.req, CtlRequest::ConflictUnresolve { .. })),
            "undo with no plan issues no mutation command"
        );
        assert!(m.undo.is_none());
    }

    #[test]
    fn keep_verdict_records_an_unresolve_only_undo_plan() {
        let mut m = one_conflict_center();
        let id = m.selected_conflict().unwrap().id;
        m.outbox.clear();
        m = key(m, Key::Enter); // keep
        let plan = m.undo.clone().expect("verdict records an undo plan");
        assert_eq!(plan.verdict, Verdict::Keep);
        assert_eq!(plan.conflict_id, id);
        assert!(
            plan.pre_version.is_none(),
            "keep has no tree write to reverse"
        );

        m.outbox.clear();
        m = key(m, Key::Char('u'));
        assert!(m
            .outbox
            .iter()
            .any(|c| c.req == CtlRequest::ConflictUnresolve { id }));
        assert!(
            m.outbox.iter().any(|c| c.req == CtlRequest::ConflictsList),
            "undo re-syncs the list so the reopened conflict reappears"
        );
        assert!(
            !m.pending_resolved.contains(&id),
            "the hidden row is restored"
        );
        assert!(m.undo.is_none(), "the plan is consumed (single level)");
    }

    #[test]
    fn take_verdict_undo_restores_the_pre_take_winner_then_reopens() {
        let mut m = one_conflict_center();
        let id = m.selected_conflict().unwrap().id;
        let path = m.selected_conflict().unwrap().path.clone();
        // Cache the conflict detail so the take undo can carry the winner id.
        let detail = parse_detail(&json!({
            "path": path,
            "diffable": true,
            "diff": ["-a", "+b"],
            "winner": {"origin": "remote", "wall_unix_ms": 100, "id": 501},
            "loser": {"origin": "local", "wall_unix_ms": 90, "id": 502},
        }))
        .unwrap();
        m = deliver(m, 999, CmdReply::Show { id, detail });
        m.outbox.clear();
        m = key(m, Key::Char('t')); // take
        let plan = m.undo.clone().unwrap();
        assert_eq!(plan.verdict, Verdict::Take);
        assert_eq!(
            plan.pre_version,
            Some(501),
            "the take undo carries the winner version that was on disk"
        );

        m.outbox.clear();
        m = key(m, Key::Char('u'));
        assert!(m.outbox.iter().any(|c| c.req
            == CtlRequest::Restore {
                path: path.clone(),
                version: 501
            }));
        assert!(m
            .outbox
            .iter()
            .any(|c| c.req == CtlRequest::ConflictUnresolve { id }));
    }

    #[test]
    fn both_verdict_undo_reopens_only_and_notes_the_sidecar() {
        let mut m = one_conflict_center();
        let id = m.selected_conflict().unwrap().id;
        m.outbox.clear();
        m = key(m, Key::Char('b')); // both
        assert_eq!(m.undo.clone().unwrap().verdict, Verdict::Both);

        m.outbox.clear();
        m = key(m, Key::Char('u'));
        assert!(m
            .outbox
            .iter()
            .any(|c| c.req == CtlRequest::ConflictUnresolve { id }));
        assert!(
            !m.outbox
                .iter()
                .any(|c| matches!(c.req, CtlRequest::Restore { .. })),
            "both undo cannot delete the sidecar — no restore/delete is issued"
        );
        assert!(
            m.flash.as_deref().unwrap().contains("theirs"),
            "the flash is honest about the leftover sidecar: {:?}",
            m.flash
        );
    }

    #[test]
    fn undo_is_single_level_second_press_is_a_no_op() {
        let mut m = one_conflict_center();
        m = key(m, Key::Enter); // keep → plan recorded
        assert!(m.undo.is_some());
        m = key(m, Key::Char('u')); // consumes it
        assert!(m.undo.is_none());
        m.outbox.clear();
        m = key(m, Key::Char('u')); // nothing left
        assert!(m.flash.as_deref().unwrap().contains("nothing to undo"));
        assert!(!m
            .outbox
            .iter()
            .any(|c| matches!(c.req, CtlRequest::ConflictUnresolve { .. })));
    }

    #[test]
    fn undo_plan_is_cleared_by_a_list_refresh_and_by_group_verdicts() {
        // A fresh list clears the single-level undo.
        let mut m = one_conflict_center();
        m = key(m, Key::Enter);
        assert!(m.undo.is_some());
        let rows = parse_conflicts(&conflicts_json(&[(7, "src/train.py", "remote")]));
        m = deliver(m, 42, CmdReply::Conflicts(rows));
        assert!(m.undo.is_none(), "a list refresh clears the undo plan");

        // A group (mass) verdict has no single-target undo.
        let mut g = Model::default();
        g.screen = Screen::Conflicts;
        for id in [10, 11] {
            g = ev(
                g,
                Event::Conflict {
                    id: Some(id),
                    path: format!("g{id}.txt"),
                    winner: ConflictSide::Peer,
                    adopted: true,
                },
            );
        }
        let rows = parse_conflicts(&conflicts_json(&[
            (10, "g10.txt", "remote"),
            (11, "g11.txt", "remote"),
        ]));
        g = deliver(g, 0, CmdReply::Conflicts(rows));
        g.sel = 0;
        assert!(g.on_group_header());
        g = key(g, Key::Char('t')); // group take
        assert!(g.undo.is_none(), "a group verdict records no undo plan");
    }

    // ---- history browser: picker, timeline, mark/compare, restore ----------

    fn history_paths_json(rows: &[(&str, u64, i64, u64)]) -> Value {
        Value::Array(
            rows.iter()
                .map(|(path, versions, last, wall)| {
                    json!({
                        "path": path,
                        "versions": versions,
                        "last_version": last,
                        "last_wall_unix_ms": wall,
                    })
                })
                .collect(),
        )
    }

    #[allow(clippy::type_complexity)] // a compact test-fixture tuple
    fn history_log_json(rows: &[(i64, u64, Option<u64>, &str, bool, bool)]) -> Value {
        Value::Array(
            rows.iter()
                .map(|(id, wall, size, origin, exec, present)| {
                    json!({
                        "id": id,
                        "wall_unix_ms": wall,
                        "size": size,
                        "origin": origin,
                        "exec": exec,
                        "present": present,
                    })
                })
                .collect(),
        )
    }

    /// A model sitting in the history timeline of `a.txt` with three versions
    /// (#30 newest, #20, #10 oldest), diff requests drained.
    fn timeline_model() -> Model {
        let mut m = Model::default();
        m = key(m, Key::Char('h'));
        let paths = parse_history_paths(&history_paths_json(&[("a.txt", 3, 30, 3)]));
        m = deliver(m, 0, CmdReply::HistoryPaths(paths));
        m = key(m, Key::Enter); // open a.txt's timeline
        let versions = parse_history_log(&history_log_json(&[
            (30, 3, Some(100), "local", false, true),
            (20, 2, Some(90), "remote", false, true),
            (10, 1, Some(80), "local", false, true),
        ]));
        m = deliver(
            m,
            1,
            CmdReply::HistoryLog {
                path: "a.txt".to_owned(),
                versions,
            },
        );
        m.outbox.clear();
        m
    }

    #[test]
    fn history_key_opens_picker_and_fetches_paths() {
        let mut m = Model::default();
        m = key(m, Key::Char('h'));
        assert_eq!(m.screen, Screen::HistoryPicker);
        assert!(m
            .outbox
            .iter()
            .any(|c| matches!(c.req, CtlRequest::HistoryPaths { .. })));
    }

    #[test]
    fn picker_type_to_filter_and_select_opens_timeline() {
        let mut m = Model::default();
        m = key(m, Key::Char('h'));
        let paths = parse_history_paths(&history_paths_json(&[
            ("src/train.py", 3, 30, 3),
            ("assets/logo.png", 1, 10, 1),
            ("src/config.yaml", 2, 20, 2),
        ]));
        m = deliver(m, 0, CmdReply::HistoryPaths(paths));
        assert_eq!(m.filtered_paths().len(), 3);
        for c in "src".chars() {
            m = key(m, Key::Char(c));
        }
        assert_eq!(m.picker_filter, "src");
        let shown: Vec<_> = m.filtered_paths().iter().map(|p| p.path.as_str()).collect();
        assert_eq!(shown, vec!["src/train.py", "src/config.yaml"]);
        // Down then Enter opens the selected path's timeline.
        m = key(m, Key::Down);
        assert_eq!(m.picker_sel, 1);
        m.outbox.clear();
        m = key(m, Key::Enter);
        assert_eq!(m.screen, Screen::HistoryTimeline);
        assert_eq!(m.history_path.as_deref(), Some("src/config.yaml"));
        assert!(m.outbox.iter().any(|c| matches!(
            &c.req,
            CtlRequest::HistoryLog { path, .. } if path == "src/config.yaml"
        )));
    }

    #[test]
    fn picker_esc_returns_to_main() {
        let mut m = Model::default();
        m = key(m, Key::Char('h'));
        m = key(m, Key::Esc);
        assert_eq!(m.screen, Screen::Main);
    }

    #[test]
    fn timeline_diffs_against_predecessor_and_requests_it() {
        let m = timeline_model();
        // Newest (#30) selected; its predecessor is #20.
        assert_eq!(m.selected_version().unwrap().id, 30);
        assert_eq!(m.diff_pair(), Some((20, 30)));
        // Moving selection re-targets the diff and requests the new pair.
        let mut m = m;
        m = key(m, Key::Char('j'));
        assert_eq!(m.selected_version().unwrap().id, 20);
        assert_eq!(m.diff_pair(), Some((10, 20)));
        assert!(m.outbox.iter().any(|c| matches!(
            c.req,
            CtlRequest::VersionDiff {
                from: 10,
                to: 20,
                ..
            }
        )));
        // The oldest version has no predecessor → nothing to compare.
        m = key(m, Key::Char('j'));
        assert_eq!(m.selected_version().unwrap().id, 10);
        assert_eq!(m.diff_pair(), None);
    }

    #[test]
    fn timeline_mark_compares_marked_against_selected() {
        let mut m = timeline_model();
        m = key(m, Key::Char('j')); // select #20
        m = key(m, Key::Char('m')); // mark #20
        assert_eq!(m.mark, Some(20));
        m = key(m, Key::Char('k')); // back to #30
        assert_eq!(m.diff_pair(), Some((20, 30)));
        assert!(compare_header(&m).unwrap().contains("comparing #20"));
        // Esc clears the mark first, not the screen.
        m = key(m, Key::Esc);
        assert!(m.mark.is_none());
        assert_eq!(m.screen, Screen::HistoryTimeline);
        // Esc again walks back to the picker.
        m = key(m, Key::Esc);
        assert_eq!(m.screen, Screen::HistoryPicker);
    }

    #[test]
    fn timeline_restore_confirm_flow_and_optimistic_flash() {
        let mut m = timeline_model();
        m = key(m, Key::Char('r'));
        assert!(matches!(
            m.modal,
            Some(Modal::RestoreConfirm { version: 30, .. })
        ));
        // Cancel issues nothing.
        m = key(m, Key::Char('n'));
        assert!(m.modal.is_none());
        assert!(!m
            .outbox
            .iter()
            .any(|c| matches!(c.req, CtlRequest::Restore { .. })));
        // Confirm restores optimistically and flashes.
        m = key(m, Key::Char('r'));
        m.outbox.clear();
        m = key(m, Key::Enter);
        assert!(m.modal.is_none());
        assert!(m.outbox.iter().any(|c| matches!(
            &c.req,
            CtlRequest::Restore { version, .. } if *version == 30
        )));
        let flash = m.flash.as_deref().unwrap();
        assert!(
            flash.contains("restored") && flash.contains("syncing to peer"),
            "{flash}"
        );
    }

    #[test]
    fn version_diff_reply_is_cached_by_pair() {
        let mut m = timeline_model();
        // The predecessor diff (20 → 30) was requested on entry; deliver it.
        m = deliver(
            m,
            7,
            CmdReply::VersionDiff {
                from: 20,
                to: 30,
                detail: parse_version_diff(&json!({
                    "identical": false, "diffable": true, "diff": ["-x", "+y"]
                })),
            },
        );
        assert!(m.version_diffs.contains_key(&(20, 30)));
        assert_eq!(m.version_diffs[&(20, 30)].diff, vec!["-x", "+y"]);
    }

    #[test]
    fn restore_echo_reflects_the_selected_version() {
        let m = timeline_model();
        assert_eq!(
            restore_echo(&m).as_deref(),
            Some("= tomo restore a.txt --version 30")
        );
    }

    // ---- diff loading -----------------------------------------------------

    #[test]
    fn entering_center_requests_list_and_diff() {
        let mut m = Model::default();
        m = key(m, Key::Char('c'));
        assert_eq!(m.screen, Screen::Conflicts);
        assert!(m.outbox.iter().any(|c| c.req == CtlRequest::ConflictsList));
        // Deliver a list; a show for the selected id should be enqueued.
        let rows = parse_conflicts(&conflicts_json(&[(7, "a.txt", "remote")]));
        m.outbox.clear();
        m = deliver(m, 0, CmdReply::Conflicts(rows));
        assert!(
            m.outbox
                .iter()
                .any(|c| c.req == CtlRequest::ConflictShow { id: 7 }),
            "diff for the selected conflict is fetched"
        );
        assert!(m.diff_requested.contains(&7));
        // Delivering the detail caches it and stops re-requesting.
        let detail = parse_detail(&json!({
            "path": "a.txt",
            "diffable": true,
            "diff": ["@@ -1 +1 @@", "-old", "+new"],
            "winner": {"origin": "remote", "wall_unix_ms": 100},
            "loser": {"origin": "local", "wall_unix_ms": 90},
        }))
        .unwrap();
        m = deliver(m, 1, CmdReply::Show { id: 7, detail });
        assert!(m.diffs.contains_key(&7));
        assert!(!m.diff_requested.contains(&7));
        assert_eq!(m.diffs[&7].winner.side, Side::Peer);
        assert_eq!(m.diffs[&7].diff.len(), 3);
    }

    // ---- help overlay + quit + too-small ----------------------------------

    #[test]
    fn help_toggles_and_captures_input() {
        let mut m = Model::default();
        m = key(m, Key::Char('?'));
        assert!(m.help);
        // Keys are captured while help is open (does not quit).
        m = key(m, Key::Char('c'));
        assert!(m.help);
        assert_eq!(m.screen, Screen::Main);
        m = key(m, Key::Esc);
        assert!(!m.help);
    }

    #[test]
    fn q_quits_from_main_and_conflicts() {
        let mut m = Model::default();
        m = key(m, Key::Char('q'));
        assert!(m.quit);
        let mut m2 = one_conflict_center();
        m2 = key(m2, Key::Char('q'));
        assert!(m2.quit);
    }

    #[test]
    fn d_detaches_without_stop_from_both_screens() {
        let mut m = Model::default();
        m.foreground = true; // even foreground: d never stops the session
        m = key(m, Key::Char('d'));
        assert!(m.quit && !m.stopping);
        let mut m2 = one_conflict_center();
        m2 = key(m2, Key::Char('d'));
        assert!(m2.quit && !m2.stopping);
    }

    #[test]
    fn foreground_q_opens_stop_confirm_instead_of_quitting() {
        let mut m = Model::default();
        m.foreground = true;
        m = key(m, Key::Char('q'));
        assert!(!m.quit);
        assert_eq!(m.modal, Some(Modal::StopConfirm));
        // y confirms: quit with the stop flag; the shell delivers the stop.
        m = key(m, Key::Char('y'));
        assert!(m.quit && m.stopping);
        assert_eq!(m.modal, None);
    }

    #[test]
    fn stop_confirm_cancels_with_n_and_esc() {
        for cancel in [Key::Char('n'), Key::Esc] {
            let mut m = Model::default();
            m.foreground = true;
            m = key(m, Key::Char('q'));
            m = key(m, cancel);
            assert!(!m.quit && !m.stopping && m.modal.is_none());
        }
    }

    #[test]
    fn exit_counters_track_synced_and_resolved() {
        let mut m = Model::default();
        m = ev(
            m,
            Event::Synced {
                path: "a.txt".into(),
                size: 1,
            },
        );
        m = ev(
            m,
            Event::Sent {
                path: "b.txt".into(),
                size: 1,
            },
        );
        assert_eq!(m.synced_total, 2);
        let mut m2 = one_conflict_center();
        m2 = key(m2, Key::Enter); // keep → pending command
        let seq = *m2.pending_cmds.keys().next().expect("pending verdict");
        m2 = update(
            m2,
            Msg::Cmd(CmdOutcome {
                seq,
                result: Ok(CmdReply::Resolved),
            }),
        );
        assert_eq!(m2.resolved_total, 1);
    }

    #[test]
    fn too_small_flag() {
        assert!(Model::too_small(39, 24));
        assert!(Model::too_small(80, 7));
        assert!(!Model::too_small(40, 8));
        assert!(!Model::too_small(80, 24));
    }

    #[test]
    fn request_json_shapes() {
        assert_eq!(
            CtlRequest::ConflictsList.to_json(),
            json!({"type": "conflicts_list"})
        );
        assert_eq!(
            CtlRequest::ConflictShow { id: 3 }.to_json(),
            json!({"type": "conflict_show", "id": 3})
        );
        assert_eq!(
            CtlRequest::Resolve {
                id: 7,
                verdict: Verdict::Keep
            }
            .to_json(),
            json!({"type": "conflicts_resolve", "id": 7, "action": "keep"})
        );
        assert_eq!(
            CtlRequest::HistoryPaths { limit: 50 }.to_json(),
            json!({"type": "history_paths", "limit": 50})
        );
        assert_eq!(
            CtlRequest::HistoryLog {
                path: "a".to_owned(),
                limit: 20
            }
            .to_json(),
            json!({"type": "history_log", "path": "a", "limit": 20})
        );
        assert_eq!(
            CtlRequest::VersionDiff {
                path: "a".to_owned(),
                from: 3,
                to: 7
            }
            .to_json(),
            json!({"type": "version_diff", "path": "a", "from": 3, "to": 7})
        );
        assert_eq!(
            CtlRequest::Restore {
                path: "a".to_owned(),
                version: 9
            }
            .to_json(),
            json!({"type": "restore", "path": "a", "version": 9})
        );
        assert_eq!(
            CtlRequest::ConflictUnresolve { id: 4 }.to_json(),
            json!({"type": "conflict_unresolve", "id": 4})
        );
    }

    #[test]
    fn cli_echo_reflects_selection() {
        let m = one_conflict_center();
        // Selected row 0 is the newest (id 8).
        assert_eq!(
            cli_echo(&m).as_deref(),
            Some("= tomo conflicts resolve 8 --keep-current")
        );
    }
}
