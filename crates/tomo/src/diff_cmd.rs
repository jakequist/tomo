//! `tomo diff <path>` — a textual diff of a path across recorded versions and
//! the working tree (docs/SPEC.md §9).
//!
//! Two sides are compared, a **base** (left, `-`) and a **target** (right, `+`):
//!
//! - base: the recorded version named by `--version <id>`, or the newest
//!   recorded version by default;
//! - target: the recorded version named by `--against <id>`, or the current
//!   working-tree file by default.
//!
//! So the three documented forms fall out of one rule: `tomo diff f` (newest
//! recorded → working tree), `tomo diff f --version <id>` (that version →
//! working tree), and `tomo diff f --version A --against B` (version A →
//! version B).
//!
//! Exit codes are git-style: `0` when the two sides are identical or the
//! content is binary/oversized (declined, with a `tomo restore --stdout` hint),
//! `1` when they differ. Only this crate renders to humans (rust-hygiene).

use std::path::Path;

use serde::Serialize;
use tomo_engine::{EntryState, RelPath};
use tomo_history::{HistoryStore, VersionMeta};

use crate::error::CliError;
use crate::history_cmd::{open_readonly_required, require_initialized, to_relpath};
use crate::layout::Layout;
use crate::out::outln;
use crate::textdiff::{diffable, line_diff, DIFF_MAX_LINES};

/// The `tomo diff --json` payload.
#[derive(Debug, Serialize)]
struct DiffJson {
    /// Whether the two sides are byte-identical.
    identical: bool,
    /// Whether both sides were text small enough to diff inline.
    diffable: bool,
    /// The rendered diff (base → target), when the sides differ and are
    /// diffable; otherwise `null`.
    diff: Option<Vec<String>>,
}

/// The outcome of comparing two byte blobs. Pure and unit-tested; the process
/// exit code and rendering are decided from it by [`run`].
#[derive(Debug, PartialEq, Eq)]
enum DiffOutcome {
    /// The two sides are byte-identical.
    Identical,
    /// The sides differ but at least one is binary or oversized.
    Undiffable,
    /// The sides differ and are shown as this rendered diff (base → target).
    Different(Vec<String>),
}

/// Compare `base` and `target` bytes into a [`DiffOutcome`]. Absent sides (a
/// tombstone version or a missing working-tree file) are passed as empty
/// slices, so a creation renders as all-additions and a deletion as
/// all-removals.
fn diff_outcome(base: &[u8], target: &[u8], max_lines: usize) -> DiffOutcome {
    if base == target {
        return DiffOutcome::Identical;
    }
    if !diffable(base, target) {
        return DiffOutcome::Undiffable;
    }
    let b = String::from_utf8_lossy(base);
    let t = String::from_utf8_lossy(target);
    DiffOutcome::Different(line_diff(&b, &t, max_lines))
}

/// Find one recorded version by its (stringly-typed) id among a path's log.
fn version_by_id(
    versions: &[VersionMeta],
    raw: &str,
    path: &RelPath,
) -> Result<VersionMeta, CliError> {
    let id: i64 = raw
        .parse()
        .map_err(|_| CliError::msg(format!("invalid version id {raw:?} (expected a number)")))?;
    versions
        .iter()
        .find(|m| m.id.0 == id)
        .cloned()
        .ok_or_else(|| CliError::msg(format!("version {id} not found for {path}")))
}

/// The bytes of a recorded version: its content when present, or empty for a
/// tombstone (an absence, diffed as such).
fn recorded_bytes(store: &HistoryStore, meta: &VersionMeta) -> Result<Vec<u8>, CliError> {
    match meta.state {
        EntryState::Present(_) => Ok(store.get_content(meta.id)?),
        EntryState::Tombstone => Ok(Vec::new()),
    }
}

/// The current working-tree bytes for `rel`, or empty when the file is absent
/// (diffed as an absence). Any other read error is surfaced.
fn working_bytes(root: &Path, rel: &RelPath) -> Result<Vec<u8>, CliError> {
    let abs = root.join(rel.as_str());
    match std::fs::read(&abs) {
        Ok(bytes) => Ok(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(CliError::io("read", &abs, e)),
    }
}

/// A short human label for one side of the diff.
fn version_label(meta: &VersionMeta, newest: bool) -> String {
    let tag = if newest { " (newest recorded)" } else { "" };
    format!("version #{}{tag}", meta.id.0)
}

/// Run `tomo diff <path>`.
///
/// # Errors
/// [`CliError::Message`] if the project is not initialized, the path is invalid
/// or has no history, or a named version is unknown; [`CliError::History`] /
/// [`CliError::Io`] on a store or filesystem error. On a successful run where
/// the two sides *differ*, the process exits `1` (git-style) after printing.
pub fn run(
    layout: &Layout,
    path: &Path,
    version: Option<&str>,
    against: Option<&str>,
    json: bool,
) -> Result<(), CliError> {
    require_initialized(layout)?;
    let rel = to_relpath(layout.root(), path)?;
    let store = open_readonly_required(layout)?;
    let versions = store.log(&rel)?;
    if versions.is_empty() {
        return Err(CliError::msg(format!("no history recorded for {rel}")));
    }

    // Base (left, `-`): a named version, else the newest recorded (index 0).
    let (base_meta, base_is_newest) = match version {
        Some(raw) => (version_by_id(&versions, raw, &rel)?, false),
        None => (versions[0].clone(), true),
    };
    let base_bytes = recorded_bytes(&store, &base_meta)?;
    let base_label = version_label(&base_meta, base_is_newest);

    // Target (right, `+`): a named version, else the working-tree file.
    let (target_bytes, target_label) = match against {
        Some(raw) => {
            let meta = version_by_id(&versions, raw, &rel)?;
            let bytes = recorded_bytes(&store, &meta)?;
            (bytes, version_label(&meta, false))
        }
        None => (
            working_bytes(layout.root(), &rel)?,
            "working tree".to_owned(),
        ),
    };

    let outcome = diff_outcome(&base_bytes, &target_bytes, DIFF_MAX_LINES);

    if json {
        return render_json(&outcome);
    }
    render_human(&rel, &base_label, &target_label, &outcome)
}

/// Render `--json` and exit `1` when the sides differ (git-style).
fn render_json(outcome: &DiffOutcome) -> Result<(), CliError> {
    let payload = match outcome {
        DiffOutcome::Identical => DiffJson {
            identical: true,
            diffable: true,
            diff: None,
        },
        DiffOutcome::Undiffable => DiffJson {
            identical: false,
            diffable: false,
            diff: None,
        },
        DiffOutcome::Different(lines) => DiffJson {
            identical: false,
            diffable: true,
            diff: Some(lines.clone()),
        },
    };
    let out = serde_json::to_string_pretty(&payload)
        .map_err(|e| CliError::msg(format!("could not serialize diff: {e}")))?;
    outln!("{out}");
    exit_for(outcome)
}

/// Render the human diff and exit `1` when the sides differ (git-style).
fn render_human(
    rel: &RelPath,
    base_label: &str,
    target_label: &str,
    outcome: &DiffOutcome,
) -> Result<(), CliError> {
    let style = crate::style::current();
    match outcome {
        DiffOutcome::Identical => {
            // `dim` is a no-op when disabled, so the plain line is unchanged.
            outln!(
                "{}",
                style.dim(&format!(
                    "no differences: {rel} ({base_label} vs {target_label} are identical)"
                ))
            );
        }
        DiffOutcome::Undiffable => {
            outln!(
                "{}",
                style.dim(&format!(
                    "binary or oversized contents ({base_label} vs {target_label}); \
                     use `tomo restore --stdout` to inspect {rel}"
                ))
            );
        }
        DiffOutcome::Different(lines) => {
            outln!(
                "{}",
                style.header(&format!(
                    "diff {rel}: {base_label} → {target_label} (- base / + target):"
                ))
            );
            for line in lines {
                outln!("{}", color_diff_line(line, style));
            }
        }
    }
    exit_for(outcome)
}

/// Colorize one diff line by its prefix (`- ` red, `+ ` green, context dim),
/// returning it unchanged when styling is disabled (byte-identical plain output).
/// Shared with `tomo conflicts show`, which embeds the same diff.
pub(crate) fn color_diff_line(line: &str, style: crate::style::Style) -> String {
    if !style.enabled() {
        return line.to_owned();
    }
    if line.starts_with("- ") {
        style.err(line)
    } else if line.starts_with("+ ") {
        style.ok(line)
    } else {
        style.dim(line)
    }
}

/// Turn a [`DiffOutcome`] into the git-style exit: `0` for identical or
/// declined, `1` for a rendered difference. Exiting here (rather than returning
/// a "difference" error) keeps stderr clean — a diff is not a failure. The same
/// direct-exit pattern the broken-pipe path in [`crate::out`] already uses.
fn exit_for(outcome: &DiffOutcome) -> Result<(), CliError> {
    match outcome {
        DiffOutcome::Different(_) => std::process::exit(1),
        DiffOutcome::Identical | DiffOutcome::Undiffable => Ok(()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn identical_bytes_are_identical() {
        assert_eq!(
            diff_outcome(b"same\ntext", b"same\ntext", DIFF_MAX_LINES),
            DiffOutcome::Identical
        );
        // Two absent sides (both empty) are identical, not a spurious diff.
        assert_eq!(
            diff_outcome(b"", b"", DIFF_MAX_LINES),
            DiffOutcome::Identical
        );
    }

    #[test]
    fn binary_or_oversized_is_undiffable() {
        assert_eq!(
            diff_outcome(&[0xff, 0x00], b"text", DIFF_MAX_LINES),
            DiffOutcome::Undiffable
        );
    }

    #[test]
    fn text_change_renders_a_diff() {
        let outcome = diff_outcome(b"one\ntwo", b"one\nTWO", DIFF_MAX_LINES);
        match outcome {
            DiffOutcome::Different(lines) => {
                assert!(lines.contains(&"- two".to_owned()), "{lines:?}");
                assert!(lines.contains(&"+ TWO".to_owned()), "{lines:?}");
            }
            other => panic!("expected a diff, got {other:?}"),
        }
    }

    #[test]
    fn absent_base_renders_as_all_additions() {
        let outcome = diff_outcome(b"", b"a\nb", DIFF_MAX_LINES);
        match outcome {
            DiffOutcome::Different(lines) => {
                assert_eq!(lines, vec!["+ a".to_owned(), "+ b".to_owned()]);
            }
            other => panic!("expected a diff, got {other:?}"),
        }
    }
}
