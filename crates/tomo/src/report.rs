//! Where a running sync loop sends its human-facing output.
//!
//! In `watch`/`sync` mode notable happenings go to stdout. With styling disabled
//! (a pipe, `NO_COLOR`, `--json`) the lines keep their deliberately grep-friendly
//! shape (`synced <path>`, `removed <path>`, `conflict <path>`) so the e2e
//! scenarios can assert on them; errors go to stderr. With styling enabled the
//! same events render as colored, glyph-rich lines (`↓ <path>  <size>`, …) and a
//! transient progress line tracks in-flight transfers. In `serve` mode **stdout
//! is the protocol channel** and must stay pristine, so everything is redirected
//! to `.tomo/logs/serve.log` and never styled (CLAUDE.md: libraries never print,
//! and serve's stdout carries frames only).

use std::cell::RefCell;
use std::fs::File;
use std::io::{self, Write as _};
use std::sync::Mutex;

use serde_json::json;

use crate::history_cmd::human_size;
use crate::style::{ProgressLine, Style};

// ---- Actionable conflict-line formatting (pure, unit-tested) --------------

/// The ready-to-paste `tomo conflicts resolve <id> --take-loser` one-liner that
/// adopts a conflict's preserved loser. Pure.
fn resolve_command(id: i64) -> String {
    format!("tomo conflicts resolve {id} --take-loser")
}

/// The survivor descriptor and loser label for a non-adoption conflict line.
///
/// Returns `("kept <peer>'s copy", "yours")` when the peer's copy won (the
/// local copy is the preserved loser you could adopt), or
/// `("kept your copy", "peer's")` when the local copy won. `peer_name` falls
/// back to "peer" when the other side is not yet identified. Pure.
fn conflict_parts(winner_is_local: bool, peer_name: Option<&str>) -> (String, &'static str) {
    if winner_is_local {
        ("kept your copy".to_owned(), "peer's")
    } else {
        (
            format!("kept {}'s copy", peer_name.unwrap_or("peer")),
            "yours",
        )
    }
}

/// The full human tail of an actionable conflict line (survivor · label:
/// command). The plain-mode rendering and the shape unit tests assert on. Pure.
pub(crate) fn conflict_tail(id: i64, winner_is_local: bool, peer_name: Option<&str>) -> String {
    let (desc, label) = conflict_parts(winner_is_local, peer_name);
    format!("{desc} · {label}: {}", resolve_command(id))
}

/// The command hint appended to an *adoption* line — the survivor is already
/// described by "adopted newer copy", so only the loser label + command remain
/// (`<label>: tomo conflicts resolve <id> --take-loser`). Pure.
pub(crate) fn adoption_tail(id: i64, winner_is_local: bool) -> String {
    let label = if winner_is_local { "peer's" } else { "yours" };
    format!("{label}: {}", resolve_command(id))
}

/// The `--json` conflict event: `event`/`path` unchanged, plus additive
/// `id`/`winner`/`resolve`/`adopted` when the conflict was recorded. Pure.
fn conflict_json(
    path: &str,
    id: Option<i64>,
    winner_is_local: bool,
    adopted: bool,
    _peer_name: Option<&str>,
) -> serde_json::Value {
    match id {
        Some(id) => json!({
            "event": "conflict",
            "path": path,
            "id": id,
            "winner": if winner_is_local { "local" } else { "peer" },
            "resolve": resolve_command(id),
            "adopted": adopted,
        }),
        None => json!({ "event": "conflict", "path": path }),
    }
}

/// A sink for a sync loop's diagnostics.
pub enum Reporter {
    /// `watch`/`sync` mode: human lines to stdout, errors to stderr. `json`
    /// switches the event lines to compact JSON objects; `style` decides colored
    /// vs. plain rendering; `progress` owns the transient transfer line.
    Human {
        /// Emit machine-readable JSON event lines instead of human text.
        json: bool,
        /// The resolved terminal styling capability.
        style: Style,
        /// The single transient progress line, erased before any normal line.
        progress: RefCell<ProgressLine>,
    },
    /// `serve` mode: everything to the serve log (stdout is the wire).
    Log(Mutex<File>),
}

impl Reporter {
    /// Build a `Human` reporter for the given `json`/`style` decision.
    pub fn human(json: bool, style: Style) -> Self {
        Reporter::Human {
            json,
            style,
            progress: RefCell::new(ProgressLine::new(style)),
        }
    }

    /// A file crumb was applied from the peer into the tree (incoming).
    pub fn applied(&self, path: &str, size: u64) {
        match self {
            Reporter::Human { json, style, .. } => {
                self.clear_progress();
                if *json {
                    println!("{}", json!({ "event": "synced", "path": path }));
                } else if style.enabled() {
                    println!(
                        "{} {path}  {}",
                        style.accent(style.g_down()),
                        style.dim(&human_size(size))
                    );
                } else {
                    println!("synced {path}");
                }
            }
            Reporter::Log(file) => log_line(file, &format!("synced {path}")),
        }
    }

    /// A local change was shipped to the peer (outbound). Historically silent, so
    /// this prints **only** with styling enabled (never in `--json`, plain, or
    /// serve-log output), preserving byte-parity with the pre-styling CLI.
    pub fn sent(&self, path: &str, size: u64) {
        if let Reporter::Human {
            json: false, style, ..
        } = self
        {
            if style.enabled() {
                self.clear_progress();
                println!(
                    "{} {path}  {}",
                    style.accent(style.g_up()),
                    style.dim(&human_size(size))
                );
            }
        }
    }

    /// A file was removed as a result of a peer deletion.
    pub fn removed(&self, path: &str) {
        match self {
            Reporter::Human { json, style, .. } => {
                self.clear_progress();
                if *json {
                    println!("{}", json!({ "event": "removed", "path": path }));
                } else if style.enabled() {
                    println!("{} {path} removed", style.err(style.g_cross()));
                } else {
                    println!("removed {path}");
                }
            }
            Reporter::Log(file) => log_line(file, &format!("removed {path}")),
        }
    }

    /// A concurrent edit was resolved (surfaced non-blockingly, invariant #5).
    ///
    /// When the conflict was recorded to history (`id` is `Some`) the line is
    /// *actionable*: it names which copy survived and carries the ready-to-paste
    /// `tomo conflicts resolve <id> --take-loser` that adopts the preserved loser
    /// instead (UX-V2 §4.1). `peer_name` names the other side when known, else
    /// "peer"; `winner_is_local` decides the phrasing. When the conflict could
    /// not be recorded (`id` is `None`, a rare byte-unobtainable case) the line
    /// falls back to the non-actionable form. `--json` gains additive `id`,
    /// `winner`, and `resolve` fields; `event`/`path` are unchanged.
    pub fn conflict(
        &self,
        path: &str,
        id: Option<i64>,
        winner_is_local: bool,
        peer_name: Option<&str>,
    ) {
        match self {
            Reporter::Human { json, style, .. } => {
                self.clear_progress();
                if *json {
                    println!(
                        "{}",
                        conflict_json(path, id, winner_is_local, false, peer_name)
                    );
                } else if let Some(id) = id {
                    if style.enabled() {
                        let (desc, label) = conflict_parts(winner_is_local, peer_name);
                        println!(
                            "{} conflict {path} — {desc} · {} {}",
                            style.warn(style.g_warn()),
                            style.dim(&format!("{label}:")),
                            style.accent(&resolve_command(id)),
                        );
                    } else {
                        println!(
                            "conflict {path} — {}",
                            conflict_tail(id, winner_is_local, peer_name)
                        );
                    }
                } else if style.enabled() {
                    println!(
                        "{} conflict {path} {}",
                        style.warn(style.g_warn()),
                        style.dim("(see tomo conflicts)")
                    );
                } else {
                    println!("conflict {path}");
                }
            }
            Reporter::Log(file) => log_line(file, &format!("conflict {path}")),
        }
    }

    /// A genesis *adoption* was resolved: at first sync between two pre-existing
    /// trees, the more recently modified copy was adopted (the other is kept in
    /// history). Worded so a first sync reads as intentional rather than as a
    /// mid-session clash. Emits the same `conflict` JSON event as
    /// [`Reporter::conflict`] so `tomo conflicts --json` and the event stream
    /// still see it as a conflict. When recorded (`id` is `Some`) it gains the
    /// same actionable `--take-loser` command tail (UX-V2 §4.1).
    pub fn conflict_adopted(&self, path: &str, id: Option<i64>, winner_is_local: bool) {
        match self {
            Reporter::Human { json, style, .. } => {
                self.clear_progress();
                if *json {
                    println!("{}", conflict_json(path, id, winner_is_local, true, None));
                } else if let Some(id) = id {
                    let hint = adoption_tail(id, winner_is_local);
                    let cmd = resolve_command(id);
                    if style.enabled() {
                        let (label, _) = hint.split_once(": ").unwrap_or((&hint, ""));
                        println!(
                            "{} adopted newer copy: {path} · {} {}",
                            style.warn(style.g_warn()),
                            style.dim(&format!("{label}:")),
                            style.accent(&cmd),
                        );
                    } else {
                        println!("adopted newer copy: {path} · {hint}");
                    }
                } else if style.enabled() {
                    println!(
                        "{} adopted newer copy: {path} {}",
                        style.warn(style.g_warn()),
                        style.dim(
                            "(kept the more recently modified version; the other is in history)"
                        )
                    );
                } else {
                    println!("adopted newer copy: {path}");
                }
            }
            Reporter::Log(file) => log_line(file, &format!("adopted newer copy: {path}")),
        }
    }

    /// A one-off note not tied to a path. Rendered dim when styled (secondary
    /// text); byte-identical to the historical plain line otherwise.
    pub fn note(&self, message: &str) {
        match self {
            Reporter::Human { json, style, .. } => {
                self.clear_progress();
                if *json {
                    println!("{}", json!({ "event": "note", "message": message }));
                } else if style.enabled() {
                    println!("{}", style.dim(message));
                } else {
                    println!("{message}");
                }
            }
            Reporter::Log(file) => log_line(file, &format!("note: {message}")),
        }
    }

    /// The peer completed its handshake. Same wire/plain shape as the historical
    /// `peer connected` note; styled with a green filled dot.
    pub fn connected(&self) {
        match self {
            Reporter::Human { json, style, .. } => {
                self.clear_progress();
                if *json {
                    println!(
                        "{}",
                        json!({ "event": "note", "message": "peer connected" })
                    );
                } else if style.enabled() {
                    println!("{} peer connected", style.ok(style.g_dot_on()));
                } else {
                    println!("peer connected");
                }
            }
            Reporter::Log(file) => log_line(file, "note: peer connected"),
        }
    }

    /// The one-line startup banner (`友 tomo <ver> — syncing <dir> ⇄ <peer>`).
    /// Styled-only: it has no plain/JSON equivalent, so it prints nothing unless
    /// color is enabled — never disturbing piped or `--json` output.
    pub fn banner(&self, version: &str, dir: &str, peer: &str) {
        if let Reporter::Human {
            json: false, style, ..
        } = self
        {
            if style.enabled() {
                self.clear_progress();
                let kanji = style.g_kanji();
                let mark = if kanji.is_empty() {
                    String::new()
                } else {
                    format!("{} ", style.accent(kanji))
                };
                println!(
                    "{mark}{} {} — syncing {} {} {}",
                    style.bold("tomo"),
                    style.dim(version),
                    style.accent(dir),
                    style.dim(style.g_sync()),
                    style.accent(peer),
                );
            }
        }
    }

    /// A non-fatal error worth surfacing (to stderr in human mode).
    pub fn error(&self, message: &str) {
        match self {
            Reporter::Human { style, .. } => {
                self.clear_progress();
                if style.enabled() {
                    eprintln!(
                        "{} {} {message}",
                        style.err(style.g_cross()),
                        style.err("error:")
                    );
                } else {
                    eprintln!("error: {message}");
                }
            }
            Reporter::Log(file) => log_line(file, &format!("error: {message}")),
        }
    }

    /// Redraw the transient progress line for an in-flight inbound transfer.
    /// A no-op in `--json`, serve, or plain/non-tty modes.
    pub fn progress(&self, path: &str, got: u64, total: u64) {
        if let Reporter::Human {
            json: false,
            style,
            progress,
        } = self
        {
            if style.enabled() {
                let mut out = io::stdout().lock();
                // Best-effort: a progress redraw failure must never disturb sync.
                let _ = progress
                    .borrow_mut()
                    .update(&mut out, "receiving", path, got, total);
            }
        }
    }

    /// Erase the transient progress line if one is shown. Called before every
    /// normal line so output is never interleaved with a half-drawn progress bar.
    pub fn clear_progress(&self) {
        if let Reporter::Human { progress, .. } = self {
            let mut out = io::stdout().lock();
            let _ = progress.borrow_mut().clear(&mut out);
        }
    }
}

/// Append one line to the serve log, prefixed with a wall-clock timestamp
/// (display only, never used for decisions — invariant #7). Best-effort: a
/// logging failure must never take down the sync loop, so the result is
/// intentionally discarded.
fn log_line(file: &Mutex<File>, line: &str) {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    if let Ok(mut f) = file.lock() {
        let secs = ms / 1000;
        let _ = writeln!(f, "[{}.{:03}] {line}", secs % 100_000, ms % 1000);
        let _ = f.flush();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn conflict_tail_peer_won_names_the_peer_and_offers_yours() {
        // The peer's copy survived → the local copy is the preserved loser.
        let tail = conflict_tail(7, false, Some("vm8"));
        assert_eq!(
            tail,
            "kept vm8's copy · yours: tomo conflicts resolve 7 --take-loser"
        );
    }

    #[test]
    fn conflict_tail_local_won_flips_to_peers() {
        let tail = conflict_tail(7, true, Some("vm8"));
        assert_eq!(
            tail,
            "kept your copy · peer's: tomo conflicts resolve 7 --take-loser"
        );
    }

    #[test]
    fn conflict_tail_falls_back_to_peer_without_a_name() {
        let tail = conflict_tail(3, false, None);
        assert_eq!(
            tail,
            "kept peer's copy · yours: tomo conflicts resolve 3 --take-loser"
        );
    }

    #[test]
    fn adoption_tail_labels_the_loser_side() {
        // Peer's newer copy adopted → your older copy is the loser.
        assert_eq!(
            adoption_tail(5, false),
            "yours: tomo conflicts resolve 5 --take-loser"
        );
        // Local newer copy adopted → peer's older copy is the loser.
        assert_eq!(
            adoption_tail(5, true),
            "peer's: tomo conflicts resolve 5 --take-loser"
        );
    }

    #[test]
    fn conflict_json_is_additive_and_categorical() {
        let v = conflict_json("src/main.rs", Some(7), false, false, Some("vm8"));
        assert_eq!(v["event"], "conflict");
        assert_eq!(v["path"], "src/main.rs");
        assert_eq!(v["id"], 7);
        // winner is categorical local/peer, never the peer's name.
        assert_eq!(v["winner"], "peer");
        assert_eq!(v["resolve"], "tomo conflicts resolve 7 --take-loser");
        assert_eq!(v["adopted"], false);
    }

    #[test]
    fn conflict_json_without_id_keeps_the_legacy_shape() {
        let v = conflict_json("f", None, false, false, None);
        assert_eq!(v, json!({ "event": "conflict", "path": "f" }));
    }
}
