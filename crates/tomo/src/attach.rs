//! `tomo attach [--plain|--json]`: join the running session and stream its view
//! (UX-V2 §1).
//!
//! Attach is a thin client of the control socket, exactly like `tomo events`,
//! with two differences: the human view is prefaced by a one-line state summary
//! (peer, connection, unresolved conflicts) queried over the command channel,
//! and the "nothing running" hint mentions the detached form. The renderer is a
//! small dispatch ([`Renderer`]) so a TUI default can slot in later (UX-V2 §3)
//! without reworking attach.
//!
//! `--json` streams the raw versioned records (identical to `tomo events
//! --json`); `--plain` (and the current default) renders human lines. Ctrl-C
//! detaches — the default SIGINT terminates this client and never touches the
//! session.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;

use crate::error::CliError;
use crate::layout::Layout;
use crate::out::outln;

/// The "nothing running" hint (UX-V2 §1 wording), naming the detached form too.
const NO_SESSION: &str = "no running session — start one with 'tomo sync' (or 'tomo sync -d')";

/// How an attach client renders the event feed. A dispatch point, not a
/// rewrite: a `Tui` variant joins here later without touching the stream plumbing.
enum Renderer {
    /// Human lines, in the same shape the live session prints (today's default).
    Plain,
    /// Raw versioned event records (identical to `tomo events --json`).
    Json,
}

/// Run `tomo attach`.
///
/// # Errors
/// [`CliError::Message`] if the project is not initialized or no session is
/// running; [`CliError::Io`] on a control-socket read failure.
pub fn run(layout: &Layout, plain: bool, json: bool) -> Result<(), CliError> {
    if !layout.is_initialized() {
        return Err(CliError::msg(
            "not a Tomo project (no .tomo/ here) — run `tomo init` first",
        ));
    }
    // Default on a real terminal: the TUI (UX-V2 §3). `--plain` keeps the
    // line stream, `--json` the raw records; pipes fall back to plain
    // automatically (clap already refuses `--plain --json` together).
    if !plain && !json && stdio_is_interactive() {
        return match crate::tui::run(layout, false)? {
            crate::tui::TuiExit::Stopped => Ok(()),
            crate::tui::TuiExit::Detached => {
                outln!("detached — session still running · stop: tomo stop");
                Ok(())
            }
        };
    }
    let renderer = if json {
        Renderer::Json
    } else {
        Renderer::Plain
    };

    // Human mode leads with a status summary so an attach reads like a status +
    // live view. Best-effort: a summary failure never blocks attaching.
    if matches!(renderer, Renderer::Plain) {
        if let Some(summary) = state_summary(layout) {
            outln!("{summary}");
        }
    }

    let stream = crate::events_cmd::connect_events(layout, NO_SESSION)?;
    let want_json = matches!(renderer, Renderer::Json);
    crate::events_cmd::stream_feed(stream, want_json, &layout.ctl_sock())
}

/// Whether both stdin and stdout are real terminals (the TUI-default gate).
fn stdio_is_interactive() -> bool {
    use std::io::IsTerminal as _;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Query the session's live `status` over the command channel and render the
/// one-line summary (`attached — <peer> · <connected|offline> · N unresolved
/// conflict(s)`). Best-effort: any failure yields `None` and the summary is
/// simply skipped.
fn state_summary(layout: &Layout) -> Option<String> {
    let mut stream = UnixStream::connect(layout.ctl_sock()).ok()?;
    let hello = crate::ctl::proto::to_hello_command(&serde_json::json!({ "type": "status" }));
    writeln!(stream, "{hello}").ok()?;
    stream.flush().ok()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let reply: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let status = reply.get("status")?;

    let peer = status
        .get("peer")
        .and_then(|p| p.get("name"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("no peer");
    let connected = status
        .get("connected")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let conn = if connected { "connected" } else { "offline" };
    let unresolved = status
        .get("conflicts_unresolved")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let plural = if unresolved == 1 { "" } else { "s" };
    Some(format!(
        "attached — {peer} · {conn} · {unresolved} unresolved conflict{plural}"
    ))
}
