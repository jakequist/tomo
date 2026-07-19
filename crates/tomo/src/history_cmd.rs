//! The history-facing CLI commands: `tomo log`, `tomo restore`, `tomo db check`
//! (docs/SPEC.md §6, §9).
//!
//! These read (and, for `restore`, write through the crash-safe staging path)
//! the history store directly. They work whether or not a `watch` session is
//! running: the store is opened under WAL journaling, so concurrent reads are
//! fine, and a `restore` writes an ordinary file that a live session will pick
//! up and sync as a normal local change.
//!
//! Only this crate renders to humans (rust-hygiene): the store returns data,
//! these functions format it.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use serde::Serialize;
use tomo_engine::{EntryState, RelPath, VectorClock};
use tomo_history::{HistoryStore, Origin, VersionMeta};

use crate::apply::{apply_absent, apply_present};
use crate::error::CliError;
use crate::layout::Layout;
use crate::out::outln;
use crate::status::now_unix_ms;

/// Guard: every history command requires an initialized project.
pub(crate) fn require_initialized(layout: &Layout) -> Result<(), CliError> {
    if layout.is_initialized() {
        Ok(())
    } else {
        Err(CliError::msg(
            "not a Tomo project (no .tomo/ here) — run `tomo init` first",
        ))
    }
}

/// Turn a CLI path argument (repo-relative or absolute) into a [`RelPath`]
/// anchored at the project root.
///
/// The project root is the process working directory for these commands, so a
/// relative argument is joined onto it and an absolute argument has the root
/// stripped. Anything that escapes the root (`..`, a different absolute prefix)
/// is rejected rather than silently reinterpreted.
pub(crate) fn to_relpath(root: &Path, arg: &Path) -> Result<RelPath, CliError> {
    let abs = if arg.is_absolute() {
        arg.to_path_buf()
    } else {
        root.join(arg)
    };
    let stripped = abs.strip_prefix(root).map_err(|_| {
        CliError::msg(format!(
            "{} is outside the project root {}",
            arg.display(),
            root.display()
        ))
    })?;
    let mut parts: Vec<&str> = Vec::new();
    for comp in stripped.components() {
        match comp {
            Component::Normal(s) => {
                let s = s
                    .to_str()
                    .ok_or_else(|| CliError::msg("path is not valid UTF-8"))?;
                parts.push(s);
            }
            Component::CurDir => {}
            _ => {
                return Err(CliError::msg(format!(
                    "{} must be a simple repo-relative path (no `..` components)",
                    arg.display()
                )))
            }
        }
    }
    let joined = parts.join("/");
    RelPath::new(&joined).map_err(|e| CliError::msg(format!("invalid path {}: {e}", arg.display())))
}

// ---- tomo log -------------------------------------------------------------

/// One version as rendered by `tomo log --json` (and reused by
/// `tomo conflicts` to describe each head of a conflict).
#[derive(Debug, Serialize)]
pub(crate) struct LogEntryJson {
    id: i64,
    present: bool,
    tombstone: bool,
    size: Option<u64>,
    content_hash: Option<String>,
    replica: String,
    replica_id: u64,
    origin: &'static str,
    wall_unix_ms: u64,
    clock: BTreeMap<String, u64>,
}

impl LogEntryJson {
    pub(crate) fn from_meta(meta: &VersionMeta) -> Self {
        let present = matches!(meta.state, EntryState::Present(_));
        Self {
            id: meta.id.0,
            present,
            tombstone: !present,
            size: meta.size,
            content_hash: meta.content_hash.map(|h| h.to_string()),
            replica: crate::replica::format(meta.replica),
            replica_id: meta.replica.0,
            origin: origin_str(meta.origin),
            wall_unix_ms: meta.wall_ms,
            clock: clock_map(&meta.clock),
        }
    }
}

/// One row of repo-wide `tomo log --json`: a [`LogEntryJson`] with the owning
/// path spliced in, since the repo-wide listing spans every path.
#[derive(Debug, Serialize)]
struct RecentEntryJson {
    path: String,
    #[serde(flatten)]
    entry: LogEntryJson,
}

impl RecentEntryJson {
    fn build(path: &RelPath, meta: &VersionMeta) -> Self {
        Self {
            path: path.as_str().to_owned(),
            entry: LogEntryJson::from_meta(meta),
        }
    }
}

/// Run `tomo log <path>`.
///
/// # Errors
/// [`CliError::Message`] if the project is not initialized, the path is invalid,
/// or the path has no recorded history; [`CliError::History`] on a store error.
/// Open the history store read-only for an informational command.
///
/// Read paths must never take write locks (see `HistoryStore::open_readonly`);
/// a missing database renders as "no history recorded yet".
pub(crate) fn open_readonly_required(layout: &Layout) -> Result<HistoryStore, CliError> {
    match HistoryStore::open_readonly(layout.root())? {
        Some(store) => Ok(store),
        None => Err(CliError::msg(
            "no history recorded yet (the database is created by the first `tomo watch`)",
        )),
    }
}

pub fn run_log(
    layout: &Layout,
    path: &Path,
    json: bool,
    limit: Option<usize>,
) -> Result<(), CliError> {
    require_initialized(layout)?;
    let rel = to_relpath(layout.root(), path)?;
    let store = open_readonly_required(layout)?;
    let mut versions = store.log(&rel)?;
    if versions.is_empty() {
        return Err(CliError::msg(format!("no history recorded for {rel}")));
    }
    if let Some(n) = limit {
        versions.truncate(n);
    }

    if json {
        let entries: Vec<LogEntryJson> = versions.iter().map(LogEntryJson::from_meta).collect();
        let out = serde_json::to_string_pretty(&entries)
            .map_err(|e| CliError::msg(format!("could not serialize log: {e}")))?;
        outln!("{out}");
    } else {
        let now = now_unix_ms();
        let style = crate::style::current();
        // `bold` no-ops when disabled, so this line stays byte-identical plain.
        outln!("history of {} (newest first):", style.bold(rel.as_str()));
        for meta in &versions {
            print_log_row(meta, now);
        }
    }
    Ok(())
}

/// Print one human `tomo log` row.
fn print_log_row(meta: &VersionMeta, now_ms: u64) {
    let style = crate::style::current();
    let (state, size) = match meta.state {
        EntryState::Present(sig) => ("present", human_size(sig.size)),
        EntryState::Tombstone => ("deleted", "-".to_owned()),
    };
    if !style.enabled() {
        outln!(
            "  #{id:<6} {state:<7} {size:>10}  replica {replica}  {origin:<6}  {rel} ({abs})  {clock}",
            id = meta.id.0,
            state = state,
            size = size,
            replica = crate::replica::format(meta.replica),
            origin = origin_str(meta.origin),
            rel = format_relative(now_ms, meta.wall_ms),
            abs = format_utc(meta.wall_ms),
            clock = clock_summary(&meta.clock),
        );
        return;
    }
    outln!(
        "  {id:<10} {mark} {state:<7} {size:>10}  {odot} {origin:<6}  {rel} ({abs})",
        id = style.accent(&format!("#{}", meta.id.0)),
        mark = state_mark(meta, style),
        state = state,
        size = size,
        odot = origin_dot(meta.origin, style),
        origin = origin_str(meta.origin),
        rel = style.dim(&format_relative(now_ms, meta.wall_ms)),
        abs = style.dim(&format_utc(meta.wall_ms)),
    );
}

/// The present/deleted glyph for a version, colored (`✓` green / `✗` red).
fn state_mark(meta: &VersionMeta, style: crate::style::Style) -> String {
    match meta.state {
        EntryState::Present(_) => style.ok(style.g_ok()),
        EntryState::Tombstone => style.err(style.g_cross()),
    }
}

/// The origin glyph: a filled accent dot for local, a hollow dim dot for remote.
fn origin_dot(origin: Origin, style: crate::style::Style) -> String {
    match origin {
        Origin::Local => style.accent(style.g_dot_on()),
        Origin::Remote => style.dim(style.g_dot_off()),
    }
}

/// The number of recent versions `tomo log` (no path) shows by default.
const RECENT_DEFAULT_LIMIT: usize = 20;

/// Run repo-wide `tomo log` (no path): recent activity across all paths.
///
/// Unlike per-path `log`, an empty or absent store renders as a friendly
/// "nothing recorded" line rather than an error — there is no specific path the
/// user asked about that could be "not found".
///
/// # Errors
/// [`CliError::Message`] if the project is not initialized; [`CliError::History`]
/// on a store error.
pub fn run_recent(layout: &Layout, json: bool, limit: Option<usize>) -> Result<(), CliError> {
    require_initialized(layout)?;
    let n = limit.unwrap_or(RECENT_DEFAULT_LIMIT);

    // Read-only; a missing database is an empty history, not an error.
    let Some(store) = HistoryStore::open_readonly(layout.root())? else {
        if json {
            outln!("[]");
        } else {
            outln!("no history recorded yet (the database is created by the first `tomo watch`)");
        }
        return Ok(());
    };
    let rows = store.recent(n)?;

    if json {
        let entries: Vec<RecentEntryJson> = rows
            .iter()
            .map(|(path, meta)| RecentEntryJson::build(path, meta))
            .collect();
        let out = serde_json::to_string_pretty(&entries)
            .map_err(|e| CliError::msg(format!("could not serialize log: {e}")))?;
        outln!("{out}");
        return Ok(());
    }

    if rows.is_empty() {
        outln!("no history recorded yet");
        return Ok(());
    }
    let now = now_unix_ms();
    outln!("recent activity across all paths (newest first):");
    for (path, meta) in &rows {
        print_recent_row(path, meta, now);
    }
    Ok(())
}

/// Print one human repo-wide `tomo log` row (path included).
fn print_recent_row(path: &RelPath, meta: &VersionMeta, now_ms: u64) {
    let style = crate::style::current();
    let (state, size) = match meta.state {
        EntryState::Present(sig) => ("present", human_size(sig.size)),
        EntryState::Tombstone => ("deleted", "-".to_owned()),
    };
    if !style.enabled() {
        outln!(
            "  #{id:<6} {state:<7} {size:>10}  replica {replica}  {origin:<6}  {when:<9}  {path}",
            id = meta.id.0,
            state = state,
            size = size,
            replica = crate::replica::format(meta.replica),
            origin = origin_str(meta.origin),
            when = format_relative(now_ms, meta.wall_ms),
            path = path,
        );
        return;
    }
    outln!(
        "  {id:<10} {mark} {state:<7} {size:>10}  {odot} {origin:<6}  {when:<9}  {path}",
        id = style.accent(&format!("#{}", meta.id.0)),
        mark = state_mark(meta, style),
        state = state,
        size = size,
        odot = origin_dot(meta.origin, style),
        origin = origin_str(meta.origin),
        when = style.dim(&format_relative(now_ms, meta.wall_ms)),
        path = style.bold(path.as_str()),
    );
}

// ---- tomo restore ---------------------------------------------------------

/// Choose which version a `restore` targets: an exact `--version <id>` if given,
/// otherwise the version *before* the current newest (the "undo" default).
///
/// `versions` must be newest-first (as [`HistoryStore::log`] returns). Pure and
/// unit-tested: the on-disk work is separate.
///
/// # Errors
/// [`CliError::Message`] if the requested id is unparseable or absent, or if the
/// default is requested but there is no previous version.
fn resolve_restore_version(
    versions: &[VersionMeta],
    requested: Option<&str>,
    path: &RelPath,
) -> Result<VersionMeta, CliError> {
    match requested {
        Some(raw) => {
            let id: i64 = raw.parse().map_err(|_| {
                CliError::msg(format!("invalid version id {raw:?} (expected a number)"))
            })?;
            versions
                .iter()
                .find(|m| m.id.0 == id)
                .cloned()
                .ok_or_else(|| CliError::msg(format!("version {id} not found for {path}")))
        }
        None => versions.get(1).cloned().ok_or_else(|| {
            CliError::msg(format!(
                "{path} has only one recorded version — nothing earlier to restore \
                 (pass --version <id> to pick an exact version)"
            ))
        }),
    }
}

/// Run `tomo restore <path>`.
///
/// # Errors
/// [`CliError`] if the project is not initialized, the path has no history, the
/// requested version is missing, or an I/O / store error occurs.
pub fn run_restore(
    layout: &Layout,
    path: &Path,
    version: Option<&str>,
    stdout: bool,
) -> Result<(), CliError> {
    require_initialized(layout)?;
    let rel = to_relpath(layout.root(), path)?;
    let store = open_readonly_required(layout)?;
    let versions = store.log(&rel)?;
    if versions.is_empty() {
        return Err(CliError::msg(format!("no history recorded for {rel}")));
    }
    let target = resolve_restore_version(&versions, version, &rel)?;

    match target.state {
        EntryState::Present(sig) => {
            let bytes = store.get_content(target.id)?;
            if stdout {
                // Broken-pipe safe: `tomo restore --stdout | head` exits 0.
                crate::out::bytes(&bytes)?;
            } else {
                apply_present(layout.root(), &layout.staging(), &rel, &sig, &bytes)?;
                outln!(
                    "restored {rel} to version #{id} ({size})",
                    rel = rel,
                    id = target.id.0,
                    size = human_size(sig.size)
                );
            }
        }
        EntryState::Tombstone => {
            if stdout {
                return Err(CliError::msg(format!(
                    "version #{id} of {rel} is a deletion (tombstone) — it has no content to \
                     write to stdout",
                    id = target.id.0,
                    rel = rel
                )));
            }
            apply_absent(layout.root(), &rel)?;
            outln!(
                "restored {rel} to version #{id} (deleted)",
                rel = rel,
                id = target.id.0
            );
        }
    }
    Ok(())
}

// ---- tomo db check --------------------------------------------------------

/// The `tomo db check --json` payload.
#[derive(Debug, Serialize)]
struct CheckJson {
    ok: bool,
    versions_checked: u64,
    chunks_checked: u64,
    issues: Vec<String>,
}

/// Run `tomo db check`.
///
/// # Errors
/// [`CliError::History`] if the store cannot be queried at all, or
/// [`CliError::Message`] (exit `1`) if integrity problems were found. Data-level
/// problems are printed as a report, not raised.
pub fn run_db_check(layout: &Layout, json: bool) -> Result<(), CliError> {
    require_initialized(layout)?;
    let store = open_readonly_required(layout)?;
    let report = store.check()?;

    if json {
        let payload = CheckJson {
            ok: report.ok,
            versions_checked: report.versions_checked,
            chunks_checked: report.chunks_checked,
            issues: report.issues.clone(),
        };
        let out = serde_json::to_string_pretty(&payload)
            .map_err(|e| CliError::msg(format!("could not serialize check report: {e}")))?;
        outln!("{out}");
    } else if report.ok {
        outln!(
            "history OK: {} versions, {} chunks verified",
            report.versions_checked,
            report.chunks_checked
        );
    } else {
        outln!(
            "history CHECK FAILED: {} versions, {} chunks verified, {} problem(s):",
            report.versions_checked,
            report.chunks_checked,
            report.issues.len()
        );
        for issue in &report.issues {
            outln!("  - {issue}");
        }
    }

    if report.ok {
        Ok(())
    } else {
        // Exit 1 without re-printing the whole report; the summary is already on
        // stdout, this line lands on stderr via the error renderer.
        Err(CliError::msg(format!(
            "history integrity check found {} problem(s)",
            report.issues.len()
        )))
    }
}

// ---- formatting helpers ---------------------------------------------------

/// The stable lowercase origin label used in `log` output.
pub(crate) fn origin_str(origin: Origin) -> &'static str {
    match origin {
        Origin::Local => "local",
        Origin::Remote => "remote",
    }
}

/// A vector clock as an ordered `{replica_id: counter, …}` map for JSON.
fn clock_map(clock: &VectorClock) -> BTreeMap<String, u64> {
    clock.iter().map(|(r, c)| (r.0.to_string(), c)).collect()
}

/// A vector clock rendered compactly for human output: `{id:counter, …}`.
fn clock_summary(clock: &VectorClock) -> String {
    let parts: Vec<String> = clock
        .iter()
        .map(|(r, c)| format!("{:016x}:{c}", r.0))
        .collect();
    format!("{{{}}}", parts.join(", "))
}

/// A byte count rendered for humans (`B`/`kB`/`MB`/`GB`, display only).
pub(crate) fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    #[allow(clippy::cast_precision_loss)] // display only; magnitudes are tiny
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < MB {
        format!("{:.1} kB", b / KB)
    } else if b < GB {
        format!("{:.1} MB", b / MB)
    } else {
        format!("{:.1} GB", b / GB)
    }
}

/// A coarse "N ago" rendering of `then_ms` relative to `now_ms` (display only).
pub(crate) fn format_relative(now_ms: u64, then_ms: u64) -> String {
    if then_ms > now_ms {
        return "in the future".to_owned();
    }
    let secs = (now_ms - then_ms) / 1000;
    if secs < 2 {
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

/// Format Unix milliseconds as a UTC `YYYY-MM-DD HH:MM:SSZ` string (display
/// only; never an ordering input — invariant #7).
pub(crate) fn format_utc(unix_ms: u64) -> String {
    // Seconds since the epoch; the /1000 keeps this well within i64.
    #[allow(clippy::cast_possible_wrap)] // unix_ms is display wall time, fits i64
    let secs = (unix_ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02} {hh:02}:{mm:02}:{ss:02}Z")
}

/// Convert a day count since 1970-01-01 to a `(year, month, day)` civil date.
///
/// Howard Hinnant's `civil_from_days` algorithm, valid for the entire range of
/// dates this code will ever see. All intermediate casts are on values bounded
/// by the algorithm to small ranges (month/day fit `u32` trivially).
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (y + i64::from(m <= 2), m as u32, d)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tomo_engine::{ContentHash, ContentSig, ReplicaId};
    use tomo_history::VersionId;

    fn meta(id: i64, present: bool) -> VersionMeta {
        let mut clock = VectorClock::new();
        clock.tick(ReplicaId(42));
        let (state, content_hash, size) = if present {
            let sig = ContentSig {
                hash: ContentHash([7; 32]),
                size: 3,
            };
            (EntryState::Present(sig), Some(sig.hash), Some(3))
        } else {
            (EntryState::Tombstone, None, None)
        };
        VersionMeta {
            id: VersionId(id),
            state,
            content_hash,
            size,
            clock,
            replica: ReplicaId(42),
            wall_ms: 0,
            origin: Origin::Local,
        }
    }

    #[test]
    fn to_relpath_handles_relative_and_absolute() {
        let root = Path::new("/proj");
        assert_eq!(
            to_relpath(root, Path::new("src/main.rs")).unwrap().as_str(),
            "src/main.rs"
        );
        assert_eq!(
            to_relpath(root, Path::new("/proj/a/b.txt"))
                .unwrap()
                .as_str(),
            "a/b.txt"
        );
        assert_eq!(to_relpath(root, Path::new("./x")).unwrap().as_str(), "x");
    }

    #[test]
    fn to_relpath_rejects_escaping_paths() {
        let root = Path::new("/proj");
        assert!(to_relpath(root, Path::new("../etc/passwd")).is_err());
        assert!(to_relpath(root, Path::new("/elsewhere/x")).is_err());
    }

    #[test]
    fn restore_defaults_to_previous_version() {
        // Newest first: #3, #2, #1. Default undo targets #2.
        let versions = vec![meta(3, true), meta(2, true), meta(1, true)];
        let rel = RelPath::new("f").unwrap();
        let chosen = resolve_restore_version(&versions, None, &rel).unwrap();
        assert_eq!(chosen.id, VersionId(2));
    }

    #[test]
    fn restore_exact_version_is_selected() {
        let versions = vec![meta(3, true), meta(2, true), meta(1, true)];
        let rel = RelPath::new("f").unwrap();
        let chosen = resolve_restore_version(&versions, Some("1"), &rel).unwrap();
        assert_eq!(chosen.id, VersionId(1));
    }

    #[test]
    fn restore_unknown_version_errors() {
        let versions = vec![meta(3, true), meta(2, true)];
        let rel = RelPath::new("f").unwrap();
        assert!(resolve_restore_version(&versions, Some("99"), &rel).is_err());
        assert!(resolve_restore_version(&versions, Some("notanumber"), &rel).is_err());
    }

    #[test]
    fn restore_single_version_has_no_undo_default() {
        let versions = vec![meta(1, true)];
        let rel = RelPath::new("f").unwrap();
        assert!(resolve_restore_version(&versions, None, &rel).is_err());
        // But an explicit id still resolves.
        assert!(resolve_restore_version(&versions, Some("1"), &rel).is_ok());
    }

    #[test]
    fn log_json_entry_shape() {
        let present = LogEntryJson::from_meta(&meta(5, true));
        assert!(present.present);
        assert!(!present.tombstone);
        assert_eq!(present.size, Some(3));
        assert_eq!(present.origin, "local");
        assert_eq!(present.clock.get("42"), Some(&1));
        assert!(present.content_hash.is_some());

        let tomb = LogEntryJson::from_meta(&meta(6, false));
        assert!(tomb.tombstone);
        assert_eq!(tomb.size, None);
        assert!(tomb.content_hash.is_none());
    }

    #[test]
    fn utc_formatting_matches_known_epochs() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00Z");
        // 2021-01-01T00:00:00Z = 1609459200 s.
        assert_eq!(format_utc(1_609_459_200_000), "2021-01-01 00:00:00Z");
        // A known mid-day timestamp: 2026-07-21T13:14:15Z = 1784639655 s.
        assert_eq!(format_utc(1_784_639_655_000), "2026-07-21 13:14:15Z");
    }

    #[test]
    fn relative_time_buckets() {
        let now = 1_000_000_000;
        assert_eq!(format_relative(now, now), "just now");
        assert_eq!(format_relative(now, now - 30_000), "30s ago");
        assert_eq!(format_relative(now, now - 120_000), "2m ago");
        assert_eq!(format_relative(now, now - 7_200_000), "2h ago");
        assert_eq!(format_relative(now, now - 172_800_000), "2d ago");
        assert_eq!(format_relative(now, now + 5_000), "in the future");
    }

    #[test]
    fn human_size_scales() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 kB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MB");
    }
}
