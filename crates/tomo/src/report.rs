//! Where a running sync loop sends its human-facing output.
//!
//! In `watch` mode notable happenings go to stdout in a deliberately
//! grep-friendly shape (`synced <path>`, `removed <path>`, `conflict <path>`) so
//! the e2e scenarios can assert on them; errors go to stderr. In `serve` mode
//! **stdout is the protocol channel** and must stay pristine, so everything is
//! redirected to `.tomo/logs/serve.log` (CLAUDE.md: libraries never print, and
//! serve's stdout carries frames only).

use std::fs::File;
use std::io::Write as _;
use std::sync::Mutex;

use serde_json::json;

/// A sink for a sync loop's diagnostics.
pub enum Reporter {
    /// `watch` mode: human lines to stdout, errors to stderr. `json` switches
    /// the event lines to compact JSON objects.
    Human {
        /// Emit machine-readable JSON event lines instead of human text.
        json: bool,
    },
    /// `serve` mode: everything to the serve log (stdout is the wire).
    Log(Mutex<File>),
}

impl Reporter {
    /// A file crumb was synced from the peer into the tree.
    pub fn synced(&self, path: &str) {
        self.event("synced", path);
    }

    /// A file was removed as a result of a peer deletion.
    pub fn removed(&self, path: &str) {
        self.event("removed", path);
    }

    /// A concurrent edit was resolved (surfaced non-blockingly, invariant #5).
    pub fn conflict(&self, path: &str) {
        self.event("conflict", path);
    }

    /// A one-off note not tied to a path (e.g. `peer connected`).
    pub fn note(&self, message: &str) {
        match self {
            Reporter::Human { json } => {
                if *json {
                    println!("{}", json!({ "event": "note", "message": message }));
                } else {
                    println!("{message}");
                }
            }
            Reporter::Log(file) => log_line(file, &format!("note: {message}")),
        }
    }

    /// A non-fatal error worth surfacing.
    pub fn error(&self, message: &str) {
        match self {
            Reporter::Human { .. } => eprintln!("error: {message}"),
            Reporter::Log(file) => log_line(file, &format!("error: {message}")),
        }
    }

    fn event(&self, kind: &str, path: &str) {
        match self {
            Reporter::Human { json } => {
                if *json {
                    println!("{}", json!({ "event": kind, "path": path }));
                } else {
                    println!("{kind} {path}");
                }
            }
            Reporter::Log(file) => log_line(file, &format!("{kind} {path}")),
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
