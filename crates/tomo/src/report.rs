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
    /// `detail` is a short one-line resolution summary, shown only when styled.
    pub fn conflict(&self, path: &str, detail: Option<&str>) {
        match self {
            Reporter::Human { json, style, .. } => {
                self.clear_progress();
                if *json {
                    println!("{}", json!({ "event": "conflict", "path": path }));
                } else if style.enabled() {
                    let tail = detail.map_or_else(String::new, |d| format!(" — {d}"));
                    println!(
                        "{} conflict {path}{tail} {}",
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
    /// still see it as a conflict.
    pub fn conflict_adopted(&self, path: &str) {
        match self {
            Reporter::Human { json, style, .. } => {
                self.clear_progress();
                if *json {
                    println!("{}", json!({ "event": "conflict", "path": path }));
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
