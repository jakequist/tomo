//! The conflict-facing CLI commands: `tomo conflicts list|show|resolve`
//! (docs/SPEC.md §5.3, §9).
//!
//! Conflicts are detected and resolved deterministically by the engine
//! (last-writer-wins) and recorded in the history DB during a `watch` session —
//! **this module never decides anything about sync** (invariant #5: conflicts
//! never block sync). It only *surfaces* what already happened, non-blockingly:
//! it lists the recorded conflict rows, shows a single one in detail (including
//! a textual diff of the two heads), and lets the user acknowledge a conflict
//! (`--keep-current`) or adopt the preserved loser (`--take-loser`).
//!
//! `--take-loser` writes the loser's bytes back through the same crash-safe
//! staging + atomic-rename plumbing every other apply uses; a running `watch`
//! then syncs those bytes to the peer as an ordinary local edit. That is by
//! design — resolving a conflict is just another authored change.
//!
//! Only this crate renders to humans (rust-hygiene): the store returns data,
//! these functions format it.

use serde::Serialize;
use tomo_engine::{EntryState, RelPath};
use tomo_history::{ConflictRecord, HistoryStore, VersionId, VersionMeta};

use crate::error::CliError;
use crate::history_cmd::{format_relative, format_utc, human_size, origin_str, LogEntryJson};
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

/// The concrete action `tomo conflicts resolve` will take, decided purely from
/// its flags. Kept separate from I/O so flag validation is unit-tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvePlan {
    /// Acknowledge one conflict, leaving the tree untouched.
    KeepOne(i64),
    /// Adopt one conflict's preserved loser into the tree, then acknowledge it.
    TakeLoserOne(i64),
    /// Mass-acknowledge every unresolved conflict, tree untouched.
    KeepAll,
}

/// Validate the flag combination for `resolve` and decide the [`ResolvePlan`].
/// Never guesses: an ambiguous or empty combination is a hard error naming both
/// options (invariant #5 keeps this a user decision, never an automatic one).
fn plan_resolve(
    id: Option<i64>,
    keep_current: bool,
    take_loser: bool,
    all: bool,
) -> Result<ResolvePlan, CliError> {
    if all {
        if take_loser {
            return Err(CliError::msg(
                "--all supports only mass acknowledgement (--keep-current); \
                 resolve individual conflicts with `tomo conflicts resolve <id> --take-loser`",
            ));
        }
        if id.is_some() {
            return Err(CliError::msg(
                "choose either a single conflict <id> or --all, not both",
            ));
        }
        // --all alone or with --keep-current both mean mass-ack.
        return Ok(ResolvePlan::KeepAll);
    }

    let id = id.ok_or_else(|| {
        CliError::msg("specify a conflict id (see `tomo conflicts list`) or --all")
    })?;

    match (keep_current, take_loser) {
        (true, true) => Err(CliError::msg(
            "choose exactly one of --keep-current or --take-loser",
        )),
        (true, false) => Ok(ResolvePlan::KeepOne(id)),
        (false, true) => Ok(ResolvePlan::TakeLoserOne(id)),
        (false, false) => Err(CliError::msg(
            "how should this conflict be resolved? pass --keep-current to keep the \
             current file (acknowledge the conflict) or --take-loser to replace it \
             with the preserved losing version",
        )),
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

/// Run `tomo conflicts show <id> [--json]`.
///
/// # Errors
/// [`CliError::Message`] if the project is not initialized or the id is unknown;
/// [`CliError::History`] on a store error.
pub fn run_show(layout: &Layout, id: i64, json: bool) -> Result<(), CliError> {
    require_initialized(layout)?;
    let Some(store) = HistoryStore::open_readonly(layout.root())? else {
        return Err(CliError::msg(format!(
            "no conflict #{id} (no history recorded yet)"
        )));
    };
    let record = find_record(&store.conflicts(false)?, id)?;
    let (winner, loser) = heads(&store, &record)?;

    // Only two present heads can be diffed; a tombstone head has no bytes.
    let diff = match (&loser.state, &winner.state) {
        (EntryState::Present(_), EntryState::Present(_)) => {
            let loser_bytes = store.get_content(loser.id)?;
            let winner_bytes = store.get_content(winner.id)?;
            if diffable(&loser_bytes, &winner_bytes) {
                // diffable guarantees valid UTF-8.
                let l = String::from_utf8_lossy(&loser_bytes);
                let w = String::from_utf8_lossy(&winner_bytes);
                Some(line_diff(&l, &w, DIFF_MAX_LINES))
            } else {
                None
            }
        }
        _ => None,
    };

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

    let now = now_unix_ms();
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
    outln!(
        "{}",
        style.dim(&format!(
            "  recorded {} ({})",
            format_relative(now, record.wall_ms),
            format_utc(record.wall_ms)
        ))
    );
    outln!("  {} {}", style.ok("winner"), head_summary(&winner));
    outln!("  {} {}", style.err("loser "), head_summary(&loser));
    outln!();

    if let Some(lines) = &diff {
        outln!(
            "{}",
            style.header("diff (loser → winner, - loser / + winner):")
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
    Ok(())
}

// ---- tomo conflicts resolve -----------------------------------------------

/// Run `tomo conflicts resolve <id> --keep-current | --take-loser`, or
/// `tomo conflicts resolve --all` for mass acknowledgement.
///
/// # Errors
/// [`CliError::Message`] if the project is not initialized, the flags are
/// ambiguous/empty, or the id is unknown; [`CliError`] on an I/O or store error.
pub fn run_resolve(
    layout: &Layout,
    id: Option<i64>,
    keep_current: bool,
    take_loser: bool,
    all: bool,
) -> Result<(), CliError> {
    require_initialized(layout)?;
    let plan = plan_resolve(id, keep_current, take_loser, all)?;
    let mut store = HistoryStore::open(layout.root())?;

    match plan {
        ResolvePlan::KeepAll => resolve_keep_all(&mut store),
        ResolvePlan::KeepOne(id) => resolve_keep_one(&mut store, id),
        ResolvePlan::TakeLoserOne(id) => resolve_take_loser(layout, &mut store, id),
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
fn resolve_keep_one(store: &mut HistoryStore, id: i64) -> Result<(), CliError> {
    let report = ack_one(store, id)?;
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

/// Adopt the preserved loser of conflict `id` into the tree, then print the
/// outcome.
fn resolve_take_loser(layout: &Layout, store: &mut HistoryStore, id: i64) -> Result<(), CliError> {
    let report = take_loser_one(layout, store, id)?;
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

/// The outcome of adopting one conflict's loser (`--take-loser` semantics).
pub(crate) struct TakeReport {
    /// The conflict's path.
    pub path: String,
    /// A one-line human description of what was written (or deleted).
    pub detail: String,
}

/// Acknowledge one conflict by id: confirm it exists, mark it resolved. The
/// non-printing core shared by the CLI (`resolve_keep_one`) and the control
/// channel (`ack_conflict_ctl`) — identical semantics, no I/O beyond the store.
pub(crate) fn ack_one(store: &mut HistoryStore, id: i64) -> Result<KeepReport, CliError> {
    let record = find_record(&store.conflicts(false)?, id)?;
    let newly = store.mark_conflict_resolved(record.id)?;
    Ok(KeepReport {
        path: record.path.as_str().to_owned(),
        newly,
    })
}

/// Adopt the preserved loser of conflict `id` into the tree through the same
/// crash-safe staging + atomic-rename path every apply uses, then acknowledge
/// it. A running session's watcher ships the adopted bytes as an ordinary local
/// edit. The non-printing core shared by the CLI (`resolve_take_loser`) and the
/// control channel (`take_loser_ctl`).
pub(crate) fn take_loser_one(
    layout: &Layout,
    store: &mut HistoryStore,
    id: i64,
) -> Result<TakeReport, CliError> {
    let record = find_record(&store.conflicts(false)?, id)?;
    let (_winner, loser) = heads(store, &record)?;

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
    ack_one(&mut store, id)
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
    take_loser_one(layout, &mut store, id)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

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

    #[test]
    fn plan_resolve_keep_one() {
        assert_eq!(
            plan_resolve(Some(5), true, false, false).unwrap(),
            ResolvePlan::KeepOne(5)
        );
    }

    #[test]
    fn plan_resolve_take_loser_one() {
        assert_eq!(
            plan_resolve(Some(7), false, true, false).unwrap(),
            ResolvePlan::TakeLoserOne(7)
        );
    }

    #[test]
    fn plan_resolve_all_is_mass_keep() {
        assert_eq!(
            plan_resolve(None, false, false, true).unwrap(),
            ResolvePlan::KeepAll
        );
        // --all --keep-current is also mass-ack.
        assert_eq!(
            plan_resolve(None, true, false, true).unwrap(),
            ResolvePlan::KeepAll
        );
    }

    #[test]
    fn plan_resolve_rejects_no_flag() {
        // Must never guess how to resolve.
        assert!(plan_resolve(Some(1), false, false, false).is_err());
    }

    #[test]
    fn plan_resolve_rejects_conflicting_flags() {
        assert!(plan_resolve(Some(1), true, true, false).is_err());
    }

    #[test]
    fn plan_resolve_rejects_all_with_take_loser() {
        assert!(plan_resolve(None, false, true, true).is_err());
    }

    #[test]
    fn plan_resolve_rejects_missing_id_without_all() {
        assert!(plan_resolve(None, true, false, false).is_err());
    }

    #[test]
    fn plan_resolve_rejects_id_with_all() {
        assert!(plan_resolve(Some(1), false, false, true).is_err());
    }
}
