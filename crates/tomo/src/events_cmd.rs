//! `tomo events [--json]`: attach to the running session's control channel and
//! stream its event feed (UX-V2 §2/§5).
//!
//! A thin client of `.tomo/state/ctl.sock`: it sends the events-mode hello and
//! relays every record. `--json` prints the raw versioned records (for scripts
//! and CI, whose assertions the scenarios depend on); the default renders human
//! lines in the same shape the live session prints (reusing `style.rs`), so
//! `tomo events` reads like a detached view of the session's own output.
//!
//! With no running session the socket connect fails; that is reported as a clean
//! "no running session" message, not a crash.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use crate::ctl::proto::{self, ConflictSide, Event};
use crate::error::CliError;
use crate::layout::Layout;
use crate::out::outln;
use crate::style::{self, Style};

/// Run `tomo events [--json]`.
///
/// # Errors
/// [`CliError::Message`] if the project is not initialized or no session is
/// running (the socket cannot be connected).
pub fn run(layout: &Layout, json: bool) -> Result<(), CliError> {
    if !layout.is_initialized() {
        return Err(CliError::msg(
            "not a Tomo project (no .tomo/ here) — run `tomo init` first",
        ));
    }
    let path = layout.ctl_sock();
    let mut stream = UnixStream::connect(&path)
        .map_err(|_| CliError::msg("no running session — start one with `tomo sync`"))?;

    // Select the events channel.
    writeln!(stream, "{}", proto::to_hello_events())
        .and_then(|()| stream.flush())
        .map_err(|s| CliError::io("write to control socket", &path, s))?;

    let style = style::current();
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line.map_err(|s| CliError::io("read from control socket", &path, s))?;
        if line.is_empty() {
            continue;
        }
        if json {
            // Relay the raw versioned record verbatim.
            outln!("{line}");
        } else if let Some(rendered) = render_line(&line, style) {
            outln!("{rendered}");
        }
    }
    Ok(())
}

/// Render one raw event line as a human line, or `None` to skip it (heartbeats
/// and transfer-progress are status-line/transient fodder, not log lines).
fn render_line(line: &str, style: Style) -> Option<String> {
    let event: Event = serde_json::from_str(line).ok()?;
    render_event(&event, style)
}

/// Human rendering of one event, mirroring the live session's output shapes.
fn render_event(event: &Event, style: Style) -> Option<String> {
    match event {
        Event::Connected {
            peer_name,
            peer_addr,
        } => {
            let who = peer_label(peer_name.as_deref(), peer_addr.as_deref());
            Some(if style.enabled() {
                format!("{} connected{who}", style.ok(style.g_dot_on()))
            } else {
                format!("connected{who}")
            })
        }
        Event::Disconnected => Some(if style.enabled() {
            format!("{} disconnected", style.dim(style.g_dot_off()))
        } else {
            "disconnected".to_owned()
        }),
        Event::Synced { path, size } => Some(if style.enabled() {
            format!(
                "{} {path}  {}",
                style.accent(style.g_down()),
                style.dim(&human(*size))
            )
        } else {
            format!("synced {path}")
        }),
        Event::Sent { path, size } => Some(if style.enabled() {
            format!(
                "{} {path}  {}",
                style.accent(style.g_up()),
                style.dim(&human(*size))
            )
        } else {
            format!("sent {path}")
        }),
        Event::Removed { path } => Some(if style.enabled() {
            format!("{} {path} removed", style.err(style.g_cross()))
        } else {
            format!("removed {path}")
        }),
        Event::Conflict {
            path,
            winner,
            adopted,
            ..
        } => {
            let side = match winner {
                ConflictSide::Local => "kept the local version",
                ConflictSide::Peer => "kept the peer's version",
            };
            let body = if *adopted {
                format!("adopted newer copy: {path}")
            } else {
                format!("conflict {path} — {side}")
            };
            Some(if style.enabled() {
                format!("{} {body}", style.warn(style.g_warn()))
            } else {
                body
            })
        }
        Event::Note { message } => Some(if style.enabled() {
            style.dim(message)
        } else {
            message.clone()
        }),
        Event::Error { message } => Some(if style.enabled() {
            format!(
                "{} {} {message}",
                style.err(style.g_cross()),
                style.err("error:")
            )
        } else {
            format!("error: {message}")
        }),
        Event::Lagged => Some(if style.enabled() {
            style.warn("event stream lagged — some events were dropped")
        } else {
            "note: event stream lagged — some events were dropped".to_owned()
        }),
        // Transient/status-line records: not log lines in the human view.
        Event::Transfer { .. } | Event::Heartbeat { .. } => None,
    }
}

/// A ` (name (addr))`-style suffix for a connected line, or empty when unknown.
fn peer_label(name: Option<&str>, addr: Option<&str>) -> String {
    match (name, addr) {
        (Some(n), Some(a)) => format!(" {n} ({a})"),
        (Some(n), None) => format!(" {n}"),
        (None, Some(a)) => format!(" {a}"),
        (None, None) => String::new(),
    }
}

/// A compact human size, matching the live stream's rendering.
fn human(size: u64) -> String {
    crate::history_cmd::human_size(size)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn plain() -> Style {
        Style::default()
    }

    #[test]
    fn renders_synced_and_removed_plain() {
        let synced = render_event(
            &Event::Synced {
                path: "a.txt".to_owned(),
                size: 3,
            },
            plain(),
        )
        .unwrap();
        assert_eq!(synced, "synced a.txt");
        let removed = render_event(
            &Event::Removed {
                path: "b".to_owned(),
            },
            plain(),
        )
        .unwrap();
        assert_eq!(removed, "removed b");
    }

    #[test]
    fn renders_conflict_with_side() {
        let local = render_event(
            &Event::Conflict {
                id: Some(1),
                path: "p".to_owned(),
                winner: ConflictSide::Local,
                adopted: false,
            },
            plain(),
        )
        .unwrap();
        assert_eq!(local, "conflict p — kept the local version");
    }

    #[test]
    fn heartbeat_and_transfer_are_skipped_in_human_view() {
        assert!(render_event(
            &Event::Heartbeat {
                last_sync_ms_ago: Some(1),
                unresolved_conflicts: 0,
            },
            plain()
        )
        .is_none());
        assert!(render_event(
            &Event::Transfer {
                path: "p".to_owned(),
                done: 1,
                total: 2,
            },
            plain()
        )
        .is_none());
    }

    #[test]
    fn render_line_parses_a_raw_record() {
        let line = proto::to_line(&Event::Sent {
            path: "x".to_owned(),
            size: 5,
        });
        assert_eq!(render_line(&line, plain()).as_deref(), Some("sent x"));
    }
}
