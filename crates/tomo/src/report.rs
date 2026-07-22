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
//!
//! Independently of where the human line goes, every notable event is also
//! published as a structured record to the control channel's event stream, via
//! an [`crate::ctl::EventSink`] the session attaches at startup. The tap lives
//! here so the same call sites that print also emit — no logic is duplicated.
//! Events that need data the print path lacks (a conflict's DB id and winning
//! side, the connected peer identity, the periodic heartbeat) are emitted by the
//! session through the dedicated `emit_*` helpers below.

use std::cell::RefCell;
use std::fs::File;
use std::io::{self, Write as _};
use std::sync::Mutex;

use serde_json::json;

use crate::ctl::proto::Event;
use crate::ctl::{winner_side, EventSink};
use crate::history_cmd::human_size;
use crate::style::{ProgressLine, Style};

/// A sink for a sync loop's diagnostics: a human/log output target plus a
/// structured event tap.
pub struct Reporter {
    out: Out,
    events: EventSink,
}

/// Where a [`Reporter`]'s human-facing lines go.
enum Out {
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
    /// Build a `Human` reporter for the given `json`/`style` decision, with no
    /// event sink attached yet.
    pub fn human(json: bool, style: Style) -> Self {
        Reporter {
            out: Out::Human {
                json,
                style,
                progress: RefCell::new(ProgressLine::new(style)),
            },
            events: EventSink::default(),
        }
    }

    /// Build a `Log` reporter writing to the serve log, with no event sink yet.
    pub fn log(file: File) -> Self {
        Reporter {
            out: Out::Log(Mutex::new(file)),
            events: EventSink::default(),
        }
    }

    /// Attach the control-channel event sink (called once at session startup).
    pub fn attach_events(&mut self, events: EventSink) {
        self.events = events;
    }

    /// Whether any control-channel client is currently subscribed to events.
    pub fn has_event_subscribers(&self) -> bool {
        self.events.has_subscribers()
    }

    /// A file crumb was applied from the peer into the tree (incoming).
    pub fn applied(&self, path: &str, size: u64) {
        self.events.emit(&Event::Synced {
            path: path.to_owned(),
            size,
        });
        match &self.out {
            Out::Human { json, style, .. } => {
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
            Out::Log(file) => log_line(file, &format!("synced {path}")),
        }
    }

    /// A local change was shipped to the peer (outbound). Historically silent on
    /// the human path, so the printed line appears **only** with styling enabled
    /// (never in `--json`, plain, or serve-log output), preserving byte-parity
    /// with the pre-styling CLI. The structured `sent` event is emitted always.
    pub fn sent(&self, path: &str, size: u64) {
        self.events.emit(&Event::Sent {
            path: path.to_owned(),
            size,
        });
        if let Out::Human {
            json: false, style, ..
        } = &self.out
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
        self.events.emit(&Event::Removed {
            path: path.to_owned(),
        });
        match &self.out {
            Out::Human { json, style, .. } => {
                self.clear_progress();
                if *json {
                    println!("{}", json!({ "event": "removed", "path": path }));
                } else if style.enabled() {
                    println!("{} {path} removed", style.err(style.g_cross()));
                } else {
                    println!("removed {path}");
                }
            }
            Out::Log(file) => log_line(file, &format!("removed {path}")),
        }
    }

    /// A concurrent edit was resolved (surfaced non-blockingly, invariant #5).
    /// `detail` is a short one-line resolution summary, shown only when styled.
    ///
    /// This prints the human line only; the structured `conflict` event (which
    /// carries the DB id and winning side) is emitted by the session via
    /// [`Reporter::emit_conflict`] where that data is known.
    pub fn conflict(&self, path: &str, detail: Option<&str>) {
        match &self.out {
            Out::Human { json, style, .. } => {
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
            Out::Log(file) => log_line(file, &format!("conflict {path}")),
        }
    }

    /// A genesis *adoption* was resolved: at first sync between two pre-existing
    /// trees, the more recently modified copy was adopted (the other is kept in
    /// history). Worded so a first sync reads as intentional rather than as a
    /// mid-session clash. Prints the human line only; the structured event is
    /// emitted by the session via [`Reporter::emit_conflict`] (with `adopted`).
    pub fn conflict_adopted(&self, path: &str) {
        match &self.out {
            Out::Human { json, style, .. } => {
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
            Out::Log(file) => log_line(file, &format!("adopted newer copy: {path}")),
        }
    }

    /// A one-off note not tied to a path. Rendered dim when styled (secondary
    /// text); byte-identical to the historical plain line otherwise.
    pub fn note(&self, message: &str) {
        self.events.emit(&Event::Note {
            message: message.to_owned(),
        });
        match &self.out {
            Out::Human { json, style, .. } => {
                self.clear_progress();
                if *json {
                    println!("{}", json!({ "event": "note", "message": message }));
                } else if style.enabled() {
                    println!("{}", style.dim(message));
                } else {
                    println!("{message}");
                }
            }
            Out::Log(file) => log_line(file, &format!("note: {message}")),
        }
    }

    /// The peer completed its handshake. Same wire/plain shape as the historical
    /// `peer connected` note; styled with a green filled dot. The structured
    /// `connected` event (with the peer identity) is emitted by the session via
    /// [`Reporter::emit_connected`].
    pub fn connected(&self) {
        match &self.out {
            Out::Human { json, style, .. } => {
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
            Out::Log(file) => log_line(file, "note: peer connected"),
        }
    }

    /// The one-line startup banner (`友 tomo <ver> — syncing <dir> ⇄ <peer>`).
    /// Styled-only: it has no plain/JSON equivalent, so it prints nothing unless
    /// color is enabled — never disturbing piped or `--json` output.
    pub fn banner(&self, version: &str, dir: &str, peer: &str) {
        if let Out::Human {
            json: false, style, ..
        } = &self.out
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
        self.events.emit(&Event::Error {
            message: message.to_owned(),
        });
        match &self.out {
            Out::Human { style, .. } => {
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
            Out::Log(file) => log_line(file, &format!("error: {message}")),
        }
    }

    /// Redraw the transient progress line for an in-flight inbound transfer.
    /// The transient tty redraw is a no-op in `--json`, serve, or plain/non-tty
    /// modes; the structured `transfer` event is emitted regardless so headless
    /// event-stream clients see progress too.
    pub fn progress(&self, path: &str, got: u64, total: u64) {
        self.events.emit(&Event::Transfer {
            path: path.to_owned(),
            done: got,
            total,
        });
        if let Out::Human {
            json: false,
            style,
            progress,
        } = &self.out
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
        if let Out::Human { progress, .. } = &self.out {
            let mut out = io::stdout().lock();
            let _ = progress.borrow_mut().clear(&mut out);
        }
    }

    // ---- event-stream-only emitters (no human/log output) -----------------

    /// Emit the structured `connected` event with the peer identity known at
    /// handshake (the human `peer connected` line is emitted separately).
    pub fn emit_connected(&self, peer_name: Option<&str>, peer_addr: Option<&str>) {
        self.events.emit(&Event::Connected {
            peer_name: peer_name.map(str::to_owned),
            peer_addr: peer_addr.map(str::to_owned),
        });
    }

    /// Emit the structured `disconnected` session-state event.
    pub fn emit_disconnected(&self) {
        self.events.emit(&Event::Disconnected);
    }

    /// Emit the structured `conflict` event with the DB id, winning side, and
    /// adoption flag (the human conflict line is emitted separately).
    pub fn emit_conflict(&self, id: Option<i64>, path: &str, winner_is_local: bool, adopted: bool) {
        self.events.emit(&Event::Conflict {
            id,
            path: path.to_owned(),
            winner: winner_side(winner_is_local),
            adopted,
        });
    }

    /// Emit a periodic `heartbeat` for the TUI status line.
    pub fn emit_heartbeat(&self, last_sync_ms_ago: Option<u64>, unresolved_conflicts: u64) {
        self.events.emit(&Event::Heartbeat {
            last_sync_ms_ago,
            unresolved_conflicts,
        });
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
