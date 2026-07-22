//! The conflict-facing CLI commands: `tomo conflicts list|show|resolve`
//! (docs/SPEC.md §5.3, §9).
//!
//! Conflicts are detected and resolved deterministically by the engine
//! (last-writer-wins) and recorded in the history DB during a `watch` session —
//! **this module never decides anything about sync** (invariant #5: conflicts
//! never block sync). It only *surfaces* what already happened, non-blockingly:
//! it lists the recorded conflict rows, shows one in detail (`show <id-or-path>`,
//! with the UX-V2 §3b on-disk/in-history framing and a winner-vs-loser diff),
//! and resolves one (`resolve <id-or-path>`): acknowledge (`--keep-current`),
//! adopt the preserved loser (`--take-loser`), or keep both (`--both`). An
//! id-or-path argument that is not an integer targets that path's newest
//! unresolved conflict (UX-V2 §4.2). `--all` mass-acknowledges; `--interactive`
//! walks every unresolved conflict with a plain prompt loop (UX-V2 §4.5).
//!
//! `--take-loser` writes the loser's bytes back — and `--both` writes them to a
//! `<path>.theirs` sidecar — through the same crash-safe staging + atomic-rename
//! plumbing every other apply uses; a running `watch` then syncs those bytes to
//! the peer as an ordinary local edit. That is by design — resolving a conflict
//! is just another authored change.
//!
//! Only this crate renders to humans (rust-hygiene): the store returns data,
//! these functions format it.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

use serde::Serialize;
use tomo_engine::{EntryState, RelPath};
use tomo_history::{ConflictRecord, HistoryStore, Origin, VersionId, VersionMeta};

use crate::error::CliError;
use crate::history_cmd::{
    format_relative, format_utc, human_size, origin_str, to_relpath, LogEntryJson,
};
use crate::layout::Layout;
use crate::out::outln;
use crate::status::now_unix_ms;
use crate::textdiff::{diffable, line_diff, DIFF_MAX_LINES};

/// Guard: every conflict command requires an initialized project.
fn require_initialized(layout: &Layout) -> Result<(), CliError> {
    if layout.is_initialized() {
        Ok(())
    } else {
        Err(CliError::msg(
            "not a Tomo project (no .tomo/ here) — run `tomo init` first",
        ))
    }
}

// ---- JSON shapes ----------------------------------------------------------

/// One conflict as rendered by `tomo conflicts list --json`: the record plus
/// the joined metadata of both heads.
#[derive(Debug, Serialize)]
struct ConflictJson {
    id: i64,
    path: String,
    wall_unix_ms: u64,
    resolved: bool,
    winner: LogEntryJson,
    loser: LogEntryJson,
}

impl ConflictJson {
    fn build(record: &ConflictRecord, winner: &VersionMeta, loser: &VersionMeta) -> Self {
        Self {
            id: record.id.0,
            path: record.path.as_str().to_owned(),
            wall_unix_ms: record.wall_ms,
            resolved: record.resolved,
            winner: LogEntryJson::from_meta(winner),
            loser: LogEntryJson::from_meta(loser),
        }
    }
}

/// `tomo conflicts show <id> --json`: a [`ConflictJson`] plus the diff outcome.
#[derive(Debug, Serialize)]
struct ConflictDetailJson {
    #[serde(flatten)]
    conflict: ConflictJson,
    /// Whether both heads were text small enough to diff inline.
    diffable: bool,
    /// The rendered unified-style diff (loser → winner), when `diffable`.
    diff: Option<Vec<String>>,
}

// ---- pure helpers (unit-tested) -------------------------------------------

/// The badge line shown by `tomo status` when unresolved conflicts exist, or
/// `None` when the tree is clean. Non-blocking surfacing per invariant #5.
pub(crate) fn conflict_badge(unresolved: u64) -> Option<String> {
    if unresolved == 0 {
        None
    } else {
        let plural = if unresolved == 1 { "" } else { "s" };
        Some(format!(
            "⚠ {unresolved} unresolved conflict{plural} — see `tomo conflicts list`"
        ))
    }
}

/// Which kind of single-conflict resolution the flags request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolveKind {
    /// Keep the current file, acknowledge the conflict; tree untouched.
    Keep,
    /// Adopt the preserved loser into the tree, then acknowledge.
    Take,
    /// Materialize the loser alongside the winner as `<path>.theirs`, then
    /// acknowledge (UX-V2 §4.4).
    Both,
}

/// The validated resolution mode, decided purely from flags + whether a target
/// was supplied. Kept separate from I/O so flag validation is unit-tested.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ResolveMode {
    /// Mass-acknowledge every unresolved conflict, tree untouched.
    KeepAll,
    /// Resolve the single conflict named by `target` (an id or a path).
    Single {
        /// The raw id-or-path argument, resolved to a record later (needs I/O).
        target: String,
        /// The kind of resolution requested.
        kind: ResolveKind,
    },
}

/// Validate the flag combination for `resolve` and decide the [`ResolveMode`].
/// Never guesses: an ambiguous or empty combination is a hard error naming the
/// options (invariant #5 keeps this a user decision, never an automatic one).
/// `--interactive` is handled before this and never reaches here.
// The four bools are the mutually-exclusive `resolve` flags, validated here as
// a set; a struct would not clarify their exclusivity.
#[allow(clippy::fn_params_excessive_bools)]
fn plan_resolve(
    target: Option<&str>,
    keep_current: bool,
    take_loser: bool,
    both: bool,
    all: bool,
) -> Result<ResolveMode, CliError> {
    if all {
        if take_loser || both {
            return Err(CliError::msg(
                "--all supports only mass acknowledgement (--keep-current); resolve individual \
                 conflicts with `tomo conflicts resolve <id-or-path> --take-loser|--both`",
            ));
        }
        if target.is_some() {
            return Err(CliError::msg(
                "choose either a single conflict <id-or-path> or --all, not both",
            ));
        }
        // --all alone or with --keep-current both mean mass-ack.
        return Ok(ResolveMode::KeepAll);
    }

    let target = target
        .ok_or_else(|| {
            CliError::msg(
                "specify a conflict id or path (see `tomo conflicts list`), or --all / \
                 --interactive",
            )
        })?
        .to_owned();

    let kind =
        match (keep_current, take_loser, both) {
            (true, false, false) => ResolveKind::Keep,
            (false, true, false) => ResolveKind::Take,
            (false, false, true) => ResolveKind::Both,
            (false, false, false) => return Err(CliError::msg(
                "how should this conflict be resolved? pass --keep-current to keep the current \
                 file, --take-loser to replace it with the preserved losing version, or --both \
                 to keep both (write the loser alongside as `<path>.theirs`)",
            )),
            _ => {
                return Err(CliError::msg(
                    "choose exactly one of --keep-current, --take-loser, or --both",
                ))
            }
        };
    Ok(ResolveMode::Single { target, kind })
}

// ---- selecting a conflict by id or path (pure picking) --------------------

/// A `resolve`/`show` target: an explicit conflict id, or a project-relative
/// path whose newest unresolved conflict is meant (UX-V2 §4.2).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Selector {
    /// An explicit conflict id.
    Id(i64),
    /// A path; its newest unresolved conflict is the target.
    Path(RelPath),
}

/// Classify a raw target: an argument that parses as an integer is a conflict
/// id; anything else is a project-relative path (normalized against `root`).
///
/// # Errors
/// [`CliError::Message`] if a non-integer argument is not a valid in-tree path.
fn parse_selector(raw: &str, root: &Path) -> Result<Selector, CliError> {
    match raw.parse::<i64>() {
        Ok(id) => Ok(Selector::Id(id)),
        Err(_) => Ok(Selector::Path(to_relpath(root, Path::new(raw))?)),
    }
}

/// The newest unresolved conflict on a path, plus the ids of any older
/// unresolved conflicts on the same path (surfaced as a disambiguation note).
#[derive(Debug)]
struct PathPick {
    /// The chosen (newest) conflict.
    chosen: ConflictRecord,
    /// The ids of the other unresolved conflicts on the same path, newest first.
    others: Vec<i64>,
}

/// Pick the newest unresolved conflict on `path` from `unresolved` (the full
/// unresolved set). Newest = greatest `wall_ms`, ties broken by greatest id.
/// A path with no unresolved conflict is a clear error naming `conflicts list`.
/// Pure: no I/O, so the disambiguation is unit-tested directly.
///
/// # Errors
/// [`CliError::Message`] if no unresolved conflict exists on `path`.
fn pick_newest_on_path(
    unresolved: &[ConflictRecord],
    path: &RelPath,
) -> Result<PathPick, CliError> {
    let mut on_path: Vec<&ConflictRecord> = unresolved.iter().filter(|r| &r.path == path).collect();
    if on_path.is_empty() {
        return Err(CliError::msg(format!(
            "no unresolved conflict on {path} (see `tomo conflicts list`)"
        )));
    }
    // Newest first: greatest wall_ms, then greatest id as a stable tiebreak.
    on_path.sort_by(|a, b| b.wall_ms.cmp(&a.wall_ms).then_with(|| b.id.0.cmp(&a.id.0)));
    let chosen = on_path[0].clone();
    let others = on_path[1..].iter().map(|r| r.id.0).collect();
    Ok(PathPick { chosen, others })
}

/// Resolve a [`Selector`] to a concrete unresolved [`ConflictRecord`], printing
/// a disambiguation note when a path carries several unresolved conflicts. Used
/// by `resolve` (which only ever acts on unresolved conflicts).
fn select_unresolved(store: &HistoryStore, sel: &Selector) -> Result<ConflictRecord, CliError> {
    let unresolved = store.conflicts(true)?;
    match sel {
        Selector::Id(id) => find_record(&unresolved, *id),
        Selector::Path(path) => {
            let pick = pick_newest_on_path(&unresolved, path)?;
            if !pick.others.is_empty() {
                let others = pick
                    .others
                    .iter()
                    .map(|i| format!("#{i}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                outln!(
                    "note: {path} has {n} unresolved conflicts; resolving the newest (#{id}); \
                     others: {others} (resolve those by id)",
                    n = pick.others.len() + 1,
                    id = pick.chosen.id.0,
                );
            }
            Ok(pick.chosen)
        }
    }
}

/// Resolve a [`Selector`] for `show`: an id matches any conflict (resolved or
/// not, mirroring `list --all`); a path picks its newest *unresolved* conflict.
fn select_for_show(store: &HistoryStore, sel: &Selector) -> Result<ConflictRecord, CliError> {
    match sel {
        Selector::Id(id) => find_record(&store.conflicts(false)?, *id),
        Selector::Path(path) => Ok(pick_newest_on_path(&store.conflicts(true)?, path)?.chosen),
    }
}

/// Find the [`ConflictRecord`] with `id` among `records`, or a helpful error.
fn find_record(records: &[ConflictRecord], id: i64) -> Result<ConflictRecord, CliError> {
    records
        .iter()
        .find(|r| r.id.0 == id)
        .cloned()
        .ok_or_else(|| CliError::msg(format!("no conflict #{id} (see `tomo conflicts list`)")))
}

/// Look up the two heads of `record` in the store, returning `(winner, loser)`
/// metadata. Both heads are versions of the conflict's path.
fn heads(
    store: &HistoryStore,
    record: &ConflictRecord,
) -> Result<(VersionMeta, VersionMeta), CliError> {
    let versions = store.log(&record.path)?;
    let winner = find_version(&versions, record.winner, &record.path)?;
    let loser = find_version(&versions, record.loser, &record.path)?;
    Ok((winner, loser))
}

/// Find one version by id in a path's log.
fn find_version(
    versions: &[VersionMeta],
    id: VersionId,
    path: &RelPath,
) -> Result<VersionMeta, CliError> {
    versions
        .iter()
        .find(|v| v.id == id)
        .cloned()
        .ok_or_else(|| {
            CliError::msg(format!(
                "version #{} of {path} is missing from history",
                id.0
            ))
        })
}

// ---- tomo conflicts list --------------------------------------------------

/// Run `tomo conflicts list [--all] [--json]`.
///
/// Default lists only unresolved conflicts; `--all` includes acknowledged ones.
///
/// # Errors
/// [`CliError::Message`] if the project is not initialized; [`CliError::History`]
/// on a store error.
pub fn run_list(layout: &Layout, all: bool, json: bool) -> Result<(), CliError> {
    require_initialized(layout)?;
    // Read-only (never blocks or races a running session's store).
    let Some(store) = HistoryStore::open_readonly(layout.root())? else {
        // No database yet — render exactly like an empty list.
        if json {
            outln!("[]");
        } else if all {
            outln!("no conflicts recorded 🎉");
        } else {
            outln!("no unresolved conflicts 🎉");
        }
        return Ok(());
    };
    let records = store.conflicts(!all)?;

    let mut joined = Vec::with_capacity(records.len());
    for record in &records {
        let (winner, loser) = heads(&store, record)?;
        joined.push((record.clone(), winner, loser));
    }

    if json {
        let entries: Vec<ConflictJson> = joined
            .iter()
            .map(|(r, w, l)| ConflictJson::build(r, w, l))
            .collect();
        let out = serde_json::to_string_pretty(&entries)
            .map_err(|e| CliError::msg(format!("could not serialize conflicts: {e}")))?;
        outln!("{out}");
        return Ok(());
    }

    if joined.is_empty() {
        if all {
            outln!("no conflicts recorded 🎉");
        } else {
            outln!("no unresolved conflicts 🎉");
        }
        return Ok(());
    }

    let now = now_unix_ms();
    let scope = if all { "all" } else { "unresolved" };
    outln!("{} {scope} conflict(s) (oldest first):", joined.len());
    for (record, winner, loser) in &joined {
        print_conflict_row(record, winner, loser, now);
    }
    Ok(())
}

/// Print one human `tomo conflicts list` row.
fn print_conflict_row(
    record: &ConflictRecord,
    winner: &VersionMeta,
    loser: &VersionMeta,
    now_ms: u64,
) {
    let style = crate::style::current();
    if style.enabled() {
        // OPEN in amber with a ⚠; acked dim with a ✓.
        let marker = if record.resolved {
            format!("{} acked", style.dim(style.g_ok()))
        } else {
            format!("{} OPEN", style.warn(style.g_warn()))
        };
        outln!(
            "  {} [{marker}] {}  {}",
            style.accent(&format!("#{}", record.id.0)),
            style.bold(record.path.as_str()),
            style.dim(&format_relative(now_ms, record.wall_ms)),
        );
    } else {
        let marker = if record.resolved { "acked" } else { "OPEN " };
        outln!(
            "  #{id:<4} [{marker}] {path}  {when}",
            id = record.id.0,
            marker = marker,
            path = record.path,
            when = format_relative(now_ms, record.wall_ms),
        );
    }
    // `ok`/`err` no-op when disabled, keeping these two lines byte-identical.
    outln!("        {} {}", style.ok("winner"), head_summary(winner));
    outln!("        {} {}", style.err("loser "), head_summary(loser));
}

/// A one-line summary of one conflict head for human output.
fn head_summary(meta: &VersionMeta) -> String {
    let state = match meta.state {
        EntryState::Present(sig) => format!("present {}", human_size(sig.size)),
        EntryState::Tombstone => "deleted".to_owned(),
    };
    format!(
        "#{id} {state}  replica {replica}  {origin}",
        id = meta.id.0,
        state = state,
        replica = crate::replica::format(meta.replica),
        origin = origin_str(meta.origin),
    )
}

// ---- tomo conflicts show --------------------------------------------------

/// Run `tomo conflicts show <id-or-path> [--json]` (UX-V2 §4.3).
///
/// An integer argument is a conflict id; anything else is a project-relative
/// path whose newest unresolved conflict is shown. Read-only, so it works
/// against a live session.
///
/// # Errors
/// [`CliError::Message`] if the project is not initialized or the target is
/// unknown; [`CliError::History`] on a store error.
pub fn run_show(layout: &Layout, target: &str, json: bool) -> Result<(), CliError> {
    require_initialized(layout)?;
    let sel = parse_selector(target, layout.root())?;
    let Some(store) = HistoryStore::open_readonly(layout.root())? else {
        return Err(CliError::msg(format!(
            "no conflict for {target} (no history recorded yet)"
        )));
    };
    let record = select_for_show(&store, &sel)?;
    let (winner, loser) = heads(&store, &record)?;
    let diff = compute_diff(&store, &winner, &loser)?;

    if json {
        let detail = ConflictDetailJson {
            conflict: ConflictJson::build(&record, &winner, &loser),
            diffable: diff.is_some(),
            diff: diff.clone(),
        };
        let out = serde_json::to_string_pretty(&detail)
            .map_err(|e| CliError::msg(format!("could not serialize conflict: {e}")))?;
        outln!("{out}");
        return Ok(());
    }

    let peer = crate::status::persisted_peer_name(layout);
    render_show_human(&record, &winner, &loser, peer.as_deref(), diff.as_deref());
    Ok(())
}

/// The inline winner-vs-loser diff (loser → winner) of a conflict's two heads,
/// or `None` when either head is a tombstone or the content is binary/oversized
/// (the same fallback `tomo diff` uses). `store` reads are the only I/O.
fn compute_diff(
    store: &HistoryStore,
    winner: &VersionMeta,
    loser: &VersionMeta,
) -> Result<Option<Vec<String>>, CliError> {
    match (&loser.state, &winner.state) {
        (EntryState::Present(_), EntryState::Present(_)) => {
            let loser_bytes = store.get_content(loser.id)?;
            let winner_bytes = store.get_content(winner.id)?;
            if diffable(&loser_bytes, &winner_bytes) {
                // diffable guarantees valid UTF-8.
                let l = String::from_utf8_lossy(&loser_bytes);
                let w = String::from_utf8_lossy(&winner_bytes);
                Ok(Some(line_diff(&l, &w, DIFF_MAX_LINES)))
            } else {
                Ok(None)
            }
        }
        _ => Ok(None),
    }
}

/// The human name for one side of a conflict, per UX-V2 §3b: the local replica
/// is "you"; the remote replica is the peer's name when known, else "peer".
fn side_label(origin: Origin, peer_name: Option<&str>) -> String {
    match origin {
        Origin::Local => "you".to_owned(),
        Origin::Remote => peer_name.unwrap_or("peer").to_owned(),
    }
}

/// One §3b framing line: `<where> — <side>, <time>` (display-only wall time).
/// Pure, so the framing shape is unit-tested.
fn frame_line(where_: &str, side: &str, wall_ms: u64) -> String {
    format!("{where_} — {side}, {}", format_utc(wall_ms))
}

/// Render one conflict for humans: the §3b framing (on disk now / in history)
/// followed by the inline winner-vs-loser diff. Shared by `show` and the
/// interactive loop.
fn render_show_human(
    record: &ConflictRecord,
    winner: &VersionMeta,
    loser: &VersionMeta,
    peer: Option<&str>,
    diff: Option<&[String]>,
) {
    let style = crate::style::current();
    let marker = if style.enabled() {
        if record.resolved {
            format!("{} acknowledged", style.dim(style.g_ok()))
        } else {
            format!("{} unresolved", style.warn(style.g_warn()))
        }
    } else if record.resolved {
        "acknowledged".to_owned()
    } else {
        "unresolved".to_owned()
    };
    outln!(
        "conflict {} on {} ({marker})",
        style.accent(&format!("#{}", record.id.0)),
        style.bold(record.path.as_str()),
    );

    // §3b framing: the winner is what is on disk now; the loser is in history.
    let winner_side = side_label(winner.origin, peer);
    let loser_side = side_label(loser.origin, peer);
    let on_disk = frame_line("on disk now", &winner_side, winner.wall_ms);
    let in_history = frame_line("in history", &loser_side, loser.wall_ms);
    outln!(
        "  {} {}",
        style.ok(&on_disk),
        style.dim(&head_summary(winner))
    );
    outln!(
        "  {} {}",
        style.err(&in_history),
        style.dim(&head_summary(loser))
    );
    outln!();

    if let Some(lines) = diff {
        outln!(
            "{}",
            style.header("diff (in history → on disk, - loser / + winner):")
        );
        for line in lines {
            outln!("{}", crate::diff_cmd::color_diff_line(line, style));
        }
    } else {
        let both_present = matches!(loser.state, EntryState::Present(_))
            && matches!(winner.state, EntryState::Present(_));
        if both_present {
            outln!("binary or oversized contents; use `tomo restore --stdout` to inspect");
        } else {
            outln!(
                "one head is a deletion (tombstone); use `tomo restore --stdout` to inspect \
                 the present side"
            );
        }
    }
}

// ---- tomo conflicts resolve -----------------------------------------------

/// Run `tomo conflicts resolve <id-or-path> --keep-current|--take-loser|--both`,
/// `tomo conflicts resolve --all` (mass acknowledgement), or
/// `tomo conflicts resolve --interactive` (a prompt loop, UX-V2 §4.5).
///
/// # Errors
/// [`CliError::Message`] if the project is not initialized, the flags are
/// ambiguous/empty, or the target is unknown; [`CliError`] on an I/O or store
/// error.
// The five bools mirror the `resolve` clap flags 1:1; grouping them into a
// struct would only re-describe the CLI surface at the call boundary.
#[allow(clippy::fn_params_excessive_bools)]
pub fn run_resolve(
    layout: &Layout,
    target: Option<&str>,
    keep_current: bool,
    take_loser: bool,
    both: bool,
    all: bool,
    interactive: bool,
) -> Result<(), CliError> {
    require_initialized(layout)?;
    if interactive {
        return run_interactive(layout);
    }
    let mode = plan_resolve(target, keep_current, take_loser, both, all)?;
    let mut store = HistoryStore::open(layout.root())?;

    match mode {
        ResolveMode::KeepAll => resolve_keep_all(&mut store),
        ResolveMode::Single { target, kind } => {
            let sel = parse_selector(&target, layout.root())?;
            let record = select_unresolved(&store, &sel)?;
            apply_kind(layout, &mut store, &record, kind)
        }
    }
}

/// Execute one [`ResolveKind`] against an already-selected conflict record.
fn apply_kind(
    layout: &Layout,
    store: &mut HistoryStore,
    record: &ConflictRecord,
    kind: ResolveKind,
) -> Result<(), CliError> {
    match kind {
        ResolveKind::Keep => resolve_keep_one(store, record),
        ResolveKind::Take => resolve_take_loser(layout, store, record),
        ResolveKind::Both => resolve_both(layout, store, record),
    }
}

/// Acknowledge every unresolved conflict, leaving the tree untouched.
fn resolve_keep_all(store: &mut HistoryStore) -> Result<(), CliError> {
    let open = store.conflicts(true)?;
    if open.is_empty() {
        outln!("no unresolved conflicts to acknowledge 🎉");
        return Ok(());
    }
    let mut acked = 0u64;
    for record in &open {
        if store.mark_conflict_resolved(record.id)? {
            acked += 1;
        }
    }
    let plural = if acked == 1 { "" } else { "s" };
    outln!("acknowledged {acked} conflict{plural}");
    Ok(())
}

/// Acknowledge one conflict, leaving the tree untouched, and print the outcome.
fn resolve_keep_one(store: &mut HistoryStore, record: &ConflictRecord) -> Result<(), CliError> {
    let id = record.id.0;
    let report = ack_one(store, record)?;
    if report.newly {
        outln!(
            "acknowledged conflict #{id} on {} (kept current file)",
            report.path
        );
    } else {
        outln!("conflict #{id} on {} was already resolved", report.path);
    }
    Ok(())
}

/// Adopt the preserved loser of `record` into the tree, then print the outcome.
fn resolve_take_loser(
    layout: &Layout,
    store: &mut HistoryStore,
    record: &ConflictRecord,
) -> Result<(), CliError> {
    let id = record.id.0;
    let report = take_loser_one(layout, store, record)?;
    outln!("took loser of conflict #{id}: {}", report.detail);
    Ok(())
}

/// The outcome of acknowledging one conflict (`--keep-current` semantics), for
/// machine reporting over the control channel.
pub(crate) struct KeepReport {
    /// The conflict's path.
    pub path: String,
    /// Whether this call newly resolved it (false if already acknowledged).
    pub newly: bool,
}

/// The outcome of adopting one conflict's loser (`--take-loser` semantics) or
/// materializing it as a sidecar (`--both`).
pub(crate) struct TakeReport {
    /// The conflict's path.
    pub path: String,
    /// A one-line human description of what was written (or deleted).
    pub detail: String,
}

/// Acknowledge one conflict: mark it resolved. The non-printing core shared by
/// the CLI (`resolve_keep_one`) and the control channel — identical semantics,
/// no I/O beyond the store.
pub(crate) fn ack_one(
    store: &mut HistoryStore,
    record: &ConflictRecord,
) -> Result<KeepReport, CliError> {
    let newly = store.mark_conflict_resolved(record.id)?;
    Ok(KeepReport {
        path: record.path.as_str().to_owned(),
        newly,
    })
}

/// Adopt the preserved loser of `record` into the tree through the same
/// crash-safe staging + atomic-rename path every apply uses, then acknowledge
/// it. A running session's watcher ships the adopted bytes as an ordinary local
/// edit. The non-printing core shared by the CLI (`resolve_take_loser`) and the
/// control channel.
pub(crate) fn take_loser_one(
    layout: &Layout,
    store: &mut HistoryStore,
    record: &ConflictRecord,
) -> Result<TakeReport, CliError> {
    let (_winner, loser) = heads(store, record)?;

    let detail = match loser.state {
        EntryState::Present(sig) => {
            let bytes = store.get_content(loser.id)?;
            crate::apply::apply_present(
                layout.root(),
                &layout.staging(),
                &record.path,
                &sig,
                &bytes,
            )?;
            format!(
                "wrote {size} to {path} (loser #{lid})",
                size = human_size(sig.size),
                path = record.path,
                lid = loser.id.0,
            )
        }
        EntryState::Tombstone => {
            crate::apply::apply_absent(layout.root(), &record.path)?;
            format!(
                "deleted {path} (loser #{lid} was a deletion)",
                path = record.path,
                lid = loser.id.0,
            )
        }
    };
    store.mark_conflict_resolved(record.id)?;
    Ok(TakeReport {
        path: record.path.as_str().to_owned(),
        detail,
    })
}

// ---- control-channel entry points (reuse the CLI cores) -------------------

/// The conflict list as a JSON value (the same shape `conflicts list --json`
/// produces), for the control channel's `conflicts_list` command. Read-only
/// (never takes a write lock on the store).
///
/// # Errors
/// [`CliError`] if the project is not initialized or the store cannot be read.
pub(crate) fn list_value(layout: &Layout, all: bool) -> Result<serde_json::Value, CliError> {
    require_initialized(layout)?;
    let Some(store) = HistoryStore::open_readonly(layout.root())? else {
        return Ok(serde_json::Value::Array(Vec::new()));
    };
    let records = store.conflicts(!all)?;
    let mut entries = Vec::with_capacity(records.len());
    for record in &records {
        let (winner, loser) = heads(&store, record)?;
        entries.push(ConflictJson::build(record, &winner, &loser));
    }
    serde_json::to_value(&entries)
        .map_err(|e| CliError::msg(format!("could not serialize conflicts: {e}")))
}

/// Acknowledge one conflict from the control channel (`conflicts_resolve` with
/// `keep`). Opens the store with the same 5 s busy timeout the CLI uses.
///
/// # Errors
/// [`CliError`] if the project is not initialized, the id is unknown, or the
/// store cannot be opened/updated.
pub(crate) fn ack_conflict_ctl(layout: &Layout, id: i64) -> Result<KeepReport, CliError> {
    require_initialized(layout)?;
    let mut store = HistoryStore::open(layout.root())?;
    let record = find_record(&store.conflicts(false)?, id)?;
    ack_one(&mut store, &record)
}

/// Adopt one conflict's loser from the control channel (`conflicts_resolve` with
/// `take`). Identical to a second-terminal `tomo conflicts resolve <id>
/// --take-loser`: crash-safe apply, DB write under the 5 s busy timeout.
///
/// # Errors
/// [`CliError`] if the project is not initialized, the id is unknown, or the
/// store/apply fails.
pub(crate) fn take_loser_ctl(layout: &Layout, id: i64) -> Result<TakeReport, CliError> {
    require_initialized(layout)?;
    let mut store = HistoryStore::open(layout.root())?;
    let record = find_record(&store.conflicts(false)?, id)?;
    take_loser_one(layout, &mut store, &record)
}

/// Keep both from the control channel (`conflicts_resolve` with `both`).
/// Identical to a second-terminal `tomo conflicts resolve <id> --both`.
///
/// # Errors
/// [`CliError`] if the project is not initialized, the id is unknown, the loser
/// is a deletion (nothing to write alongside), or the store/apply fails.
pub(crate) fn both_ctl(layout: &Layout, id: i64) -> Result<TakeReport, CliError> {
    require_initialized(layout)?;
    let mut store = HistoryStore::open(layout.root())?;
    let record = find_record(&store.conflicts(false)?, id)?;
    both_one(layout, &mut store, &record)
}

/// The first free `<base>.theirs`, `<base>.theirs-2`, `<base>.theirs-3`, … name
/// for a `--both` sidecar, given an `exists` predicate. Pure over the predicate,
/// so collision handling is unit-tested without touching the filesystem.
fn sidecar_name(base: &str, exists: impl Fn(&str) -> bool) -> String {
    let first = format!("{base}.theirs");
    if !exists(&first) {
        return first;
    }
    let mut n = 2u32;
    loop {
        let cand = format!("{base}.theirs-{n}");
        if !exists(&cand) {
            return cand;
        }
        n += 1;
    }
}

/// Keep both (UX-V2 §4.4): materialize the preserved loser NEXT TO the winner as
/// `<path>.theirs` (colliding to `.theirs-2`, …) through the crash-safe
/// staging + atomic-rename path, then acknowledge. The sidecar syncs like any
/// ordinary file, so the manual merge can happen on either machine.
fn resolve_both(
    layout: &Layout,
    store: &mut HistoryStore,
    record: &ConflictRecord,
) -> Result<(), CliError> {
    let id = record.id.0;
    let report = both_one(layout, store, record)?;
    outln!(
        "kept both for conflict #{id} on {}: {}",
        report.path,
        report.detail
    );
    Ok(())
}

/// The non-printing `--both` core shared by the CLI (`resolve_both`) and the
/// control channel (`both_ctl`): materialize the preserved loser as the
/// `<path>.theirs` sidecar, then acknowledge.
pub(crate) fn both_one(
    layout: &Layout,
    store: &mut HistoryStore,
    record: &ConflictRecord,
) -> Result<TakeReport, CliError> {
    let id = record.id.0;
    let (_winner, loser) = heads(store, record)?;
    let EntryState::Present(sig) = loser.state else {
        return Err(CliError::msg(format!(
            "the preserved loser of conflict #{id} on {} is a deletion — there is nothing to \
             write alongside; use --keep-current or --take-loser",
            record.path
        )));
    };
    let bytes = store.get_content(loser.id)?;

    let root = layout.root();
    let name = sidecar_name(record.path.as_str(), |cand| {
        RelPath::new(cand).is_ok_and(|rp| crate::apply::join(root, &rp).exists())
    });
    let sidecar = RelPath::new(&name)
        .map_err(|e| CliError::msg(format!("could not form sidecar path {name}: {e}")))?;
    crate::apply::apply_present(root, &layout.staging(), &sidecar, &sig, &bytes)?;
    store.mark_conflict_resolved(record.id)?;
    Ok(TakeReport {
        path: record.path.as_str().to_owned(),
        detail: format!(
            "wrote the loser to {sidecar} ({size}); merge by hand — the sidecar syncs like any file",
            size = human_size(sig.size),
        ),
    })
}

// ---- tomo conflicts resolve --interactive ---------------------------------

/// A single-conflict choice in the interactive loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    /// Keep the current file (acknowledge).
    Keep,
    /// Take the preserved loser.
    Take,
    /// Keep both (`<path>.theirs` sidecar).
    Both,
    /// Skip this conflict, leaving it unresolved.
    Skip,
    /// Stop the loop.
    Quit,
}

/// Parse one interactive answer. Accepts the single-letter key or its full word,
/// case-insensitively; anything else (including a blank line) is `None`. Pure.
fn parse_choice(input: &str) -> Option<Choice> {
    match input.trim().to_ascii_lowercase().as_str() {
        "k" | "keep" => Some(Choice::Keep),
        "t" | "take" => Some(Choice::Take),
        "b" | "both" => Some(Choice::Both),
        "s" | "skip" => Some(Choice::Skip),
        "q" | "quit" => Some(Choice::Quit),
        _ => None,
    }
}

/// Read answers from `read` (a line source) until a valid [`Choice`] is parsed,
/// calling `prompt` before each attempt (so invalid input re-prompts). `None`
/// means the input ended (EOF) before any valid choice. Pure over its injected
/// I/O, so the loop control is unit-tested with a scripted reader.
fn read_choice(
    read: &mut dyn FnMut() -> Option<String>,
    mut prompt: impl FnMut(),
) -> Option<Choice> {
    loop {
        prompt();
        let line = read()?;
        if let Some(choice) = parse_choice(&line) {
            return Some(choice);
        }
        // Invalid: fall through to re-prompt.
    }
}

/// Run `tomo conflicts resolve --interactive`: for each unresolved conflict,
/// print its §3-style diff and prompt keep/take/both/skip/quit, acting
/// immediately. Requires a terminal on stdin. Works alongside a live session
/// (the store's busy timeout handles concurrent writes).
///
/// # Errors
/// [`CliError::Message`] if stdin is not a terminal; [`CliError`] on a store or
/// apply error.
fn run_interactive(layout: &Layout) -> Result<(), CliError> {
    if !io::stdin().is_terminal() {
        return Err(CliError::msg(
            "`tomo conflicts resolve --interactive` needs an interactive terminal on stdin; \
             resolve by id or path instead (see `tomo conflicts list`)",
        ));
    }
    let mut store = HistoryStore::open(layout.root())?;
    let peer = crate::status::persisted_peer_name(layout);
    let records = store.conflicts(true)?;
    if records.is_empty() {
        outln!("no unresolved conflicts 🎉");
        return Ok(());
    }
    outln!(
        "{} unresolved conflict(s) — [k]eep / [t]ake / [b]oth / [s]kip / [q]uit",
        records.len()
    );

    let stdin = io::stdin();
    let mut resolved = 0u64;
    let mut skipped = 0u64;
    for record in &records {
        let (winner, loser) = heads(&store, record)?;
        let diff = compute_diff(&store, &winner, &loser)?;
        outln!();
        render_show_human(record, &winner, &loser, peer.as_deref(), diff.as_deref());

        let mut reader = || {
            let mut line = String::new();
            match stdin.lock().read_line(&mut line) {
                Ok(0) | Err(_) => None,
                Ok(_) => Some(line),
            }
        };
        let choice = read_choice(&mut reader, prompt_choice);
        match choice {
            None | Some(Choice::Quit) => {
                outln!("stopped ({resolved} resolved, {skipped} skipped)");
                return Ok(());
            }
            Some(Choice::Skip) => {
                skipped += 1;
                outln!("skipped #{}", record.id.0);
            }
            Some(Choice::Keep) => {
                resolve_keep_one(&mut store, record)?;
                resolved += 1;
            }
            Some(Choice::Take) => {
                resolve_take_loser(layout, &mut store, record)?;
                resolved += 1;
            }
            Some(Choice::Both) => {
                resolve_both(layout, &mut store, record)?;
                resolved += 1;
            }
        }
    }
    outln!("done ({resolved} resolved, {skipped} skipped)");
    Ok(())
}

/// Print the interactive prompt (no trailing newline) and flush it so it shows
/// before the user's line. Best-effort: a flush failure never aborts resolving.
fn prompt_choice() {
    print!("[k]eep / [t]ake / [b]oth / [s]kip / [q]uit? ");
    let _ = io::stdout().flush();
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tomo_history::ConflictId;

    #[test]
    fn conflict_badge_is_none_when_clean() {
        assert_eq!(conflict_badge(0), None);
    }

    #[test]
    fn conflict_badge_pluralizes() {
        let one = conflict_badge(1).unwrap();
        assert!(one.contains('1'));
        assert!(one.contains("conflict "), "singular: {one}");
        let many = conflict_badge(3).unwrap();
        assert!(many.contains('3'));
        assert!(many.contains("conflicts"), "plural: {many}");
    }

    // ---- plan_resolve: flag validation (target, keep, take, both, all) ----

    fn single(target: &str, kind: ResolveKind) -> ResolveMode {
        ResolveMode::Single {
            target: target.to_owned(),
            kind,
        }
    }

    #[test]
    fn plan_resolve_keep_one() {
        assert_eq!(
            plan_resolve(Some("5"), true, false, false, false).unwrap(),
            single("5", ResolveKind::Keep)
        );
    }

    #[test]
    fn plan_resolve_take_loser_one() {
        assert_eq!(
            plan_resolve(Some("7"), false, true, false, false).unwrap(),
            single("7", ResolveKind::Take)
        );
    }

    #[test]
    fn plan_resolve_both_one_carries_the_target() {
        // A path target is carried verbatim (resolved to a record later).
        assert_eq!(
            plan_resolve(Some("src/main.rs"), false, false, true, false).unwrap(),
            single("src/main.rs", ResolveKind::Both)
        );
    }

    #[test]
    fn plan_resolve_all_is_mass_keep() {
        assert_eq!(
            plan_resolve(None, false, false, false, true).unwrap(),
            ResolveMode::KeepAll
        );
        // --all --keep-current is also mass-ack.
        assert_eq!(
            plan_resolve(None, true, false, false, true).unwrap(),
            ResolveMode::KeepAll
        );
    }

    #[test]
    fn plan_resolve_rejects_no_flag() {
        // Must never guess how to resolve.
        assert!(plan_resolve(Some("1"), false, false, false, false).is_err());
    }

    #[test]
    fn plan_resolve_rejects_conflicting_flags() {
        assert!(plan_resolve(Some("1"), true, true, false, false).is_err());
        // --both is mutually exclusive with keep/take.
        assert!(plan_resolve(Some("1"), false, true, true, false).is_err());
        assert!(plan_resolve(Some("1"), true, false, true, false).is_err());
        assert!(plan_resolve(Some("1"), true, true, true, false).is_err());
    }

    #[test]
    fn plan_resolve_rejects_all_with_take_or_both() {
        assert!(plan_resolve(None, false, true, false, true).is_err());
        assert!(plan_resolve(None, false, false, true, true).is_err());
    }

    #[test]
    fn plan_resolve_rejects_missing_target_without_all() {
        assert!(plan_resolve(None, true, false, false, false).is_err());
    }

    #[test]
    fn plan_resolve_rejects_target_with_all() {
        assert!(plan_resolve(Some("1"), false, false, false, true).is_err());
    }

    // ---- id-vs-path disambiguation ----------------------------------------

    fn rel(s: &str) -> RelPath {
        RelPath::new(s).unwrap()
    }

    #[test]
    fn parse_selector_treats_integers_as_ids() {
        let root = Path::new("/proj");
        assert_eq!(parse_selector("42", root).unwrap(), Selector::Id(42));
    }

    #[test]
    fn parse_selector_treats_non_integers_as_paths() {
        let root = Path::new("/proj");
        assert_eq!(
            parse_selector("src/main.rs", root).unwrap(),
            Selector::Path(rel("src/main.rs"))
        );
    }

    fn record(id: i64, path: &str, wall_ms: u64) -> ConflictRecord {
        ConflictRecord {
            id: ConflictId(id),
            path: rel(path),
            winner: VersionId(id * 10 + 1),
            loser: VersionId(id * 10 + 2),
            wall_ms,
            resolved: false,
        }
    }

    #[test]
    fn pick_newest_on_path_picks_greatest_wall_then_id() {
        let recs = vec![
            record(1, "a.txt", 100),
            record(2, "a.txt", 300), // newest by wall
            record(3, "a.txt", 300), // same wall, greater id → wins the tie
            record(4, "b.txt", 999),
        ];
        let pick = pick_newest_on_path(&recs, &rel("a.txt")).unwrap();
        assert_eq!(pick.chosen.id.0, 3);
        // The other two a.txt conflicts are listed for disambiguation.
        assert_eq!(pick.others.len(), 2);
        assert!(pick.others.contains(&1) && pick.others.contains(&2));
        // b.txt is unrelated and never appears.
        assert!(!pick.others.contains(&4));
    }

    #[test]
    fn pick_newest_on_path_single_has_no_others() {
        let recs = vec![record(7, "only.txt", 5)];
        let pick = pick_newest_on_path(&recs, &rel("only.txt")).unwrap();
        assert_eq!(pick.chosen.id.0, 7);
        assert!(pick.others.is_empty());
    }

    #[test]
    fn pick_newest_on_path_errors_when_none() {
        let recs = vec![record(1, "a.txt", 1)];
        let err = pick_newest_on_path(&recs, &rel("missing.txt")).unwrap_err();
        // The error must point the user at `tomo conflicts list`.
        assert!(format!("{err}").contains("conflicts list"), "{err}");
    }

    // ---- sidecar naming / collision ---------------------------------------

    #[test]
    fn sidecar_name_uses_theirs_when_free() {
        assert_eq!(sidecar_name("src/main.rs", |_| false), "src/main.rs.theirs");
    }

    #[test]
    fn sidecar_name_bumps_on_collision() {
        // `.theirs` and `.theirs-2` taken → `.theirs-3`.
        let taken = ["src/main.rs.theirs", "src/main.rs.theirs-2"];
        let name = sidecar_name("src/main.rs", |cand| taken.contains(&cand));
        assert_eq!(name, "src/main.rs.theirs-3");
    }

    // ---- interactive choice parsing + loop control ------------------------

    #[test]
    fn parse_choice_accepts_keys_and_words_case_insensitively() {
        assert_eq!(parse_choice("k"), Some(Choice::Keep));
        assert_eq!(parse_choice("  T  "), Some(Choice::Take));
        assert_eq!(parse_choice("Both"), Some(Choice::Both));
        assert_eq!(parse_choice("S\n"), Some(Choice::Skip));
        assert_eq!(parse_choice("QUIT"), Some(Choice::Quit));
        assert_eq!(parse_choice(""), None);
        assert_eq!(parse_choice("x"), None);
    }

    #[test]
    fn read_choice_reprompts_past_invalid_input() {
        let mut scripted = vec!["x".to_owned(), String::new(), "t".to_owned()].into_iter();
        let mut prompts = 0u32;
        let mut read = || scripted.next();
        let choice = read_choice(&mut read, || prompts += 1);
        assert_eq!(choice, Some(Choice::Take));
        // Prompted once per read attempt (2 invalid + 1 valid).
        assert_eq!(prompts, 3);
    }

    #[test]
    fn read_choice_returns_none_at_eof() {
        let mut scripted = vec!["x".to_owned()].into_iter();
        let mut read = || scripted.next();
        assert_eq!(read_choice(&mut read, || {}), None);
    }

    #[test]
    fn read_choice_stops_on_quit() {
        let mut scripted = vec!["q".to_owned(), "k".to_owned()].into_iter();
        let mut read = || scripted.next();
        assert_eq!(read_choice(&mut read, || {}), Some(Choice::Quit));
    }

    // ---- §3b framing ------------------------------------------------------

    #[test]
    fn side_label_names_you_and_the_peer() {
        assert_eq!(side_label(Origin::Local, Some("vm8")), "you");
        assert_eq!(side_label(Origin::Remote, Some("vm8")), "vm8");
        assert_eq!(side_label(Origin::Remote, None), "peer");
    }

    #[test]
    fn frame_line_has_the_where_side_time_shape() {
        // 0 ms since the epoch renders the epoch instant (display only).
        let line = frame_line("on disk now", "vm8", 0);
        assert_eq!(line, "on disk now — vm8, 1970-01-01 00:00:00Z");
    }
}
