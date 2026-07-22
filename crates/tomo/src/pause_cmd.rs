//! `tomo pause` and `tomo resume` (docs/SPEC.md §13): thin control-channel
//! clients that flip the running session's pause state.
//!
//! Pause is a **session** state: while paused the session keeps observing and
//! versioning local changes and stays connected, but ships nothing outbound and
//! applies nothing inbound — both directions queue until `resume`, which drains
//! and reconciles them. These commands mirror `tomo stop`: one command object
//! over `.tomo/state/ctl.sock`, one reply line, idempotent messaging. A clean
//! "no running session" error when nothing is up.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;

use crate::error::CliError;
use crate::layout::Layout;
use crate::out::outln;

/// Run `tomo pause`.
///
/// # Errors
/// [`CliError::Message`] if the project is not initialized, no session is
/// running, or the control channel refused the command.
pub fn run_pause(layout: &Layout) -> Result<(), CliError> {
    let already = set_paused(layout, true)?;
    if already {
        outln!("already paused");
    } else {
        outln!("paused syncing (both directions queue; resume with `tomo resume`)");
    }
    Ok(())
}

/// Run `tomo resume`.
///
/// # Errors
/// [`CliError::Message`] if the project is not initialized, no session is
/// running, or the control channel refused the command.
pub fn run_resume(layout: &Layout) -> Result<(), CliError> {
    let already = set_paused(layout, false)?;
    if already {
        outln!("already syncing (not paused)");
    } else {
        outln!("resumed syncing (draining queued changes)");
    }
    Ok(())
}

/// Send the `pause`/`resume` command to the running session and return whether it
/// was **already** in the requested state (for the idempotent message).
///
/// # Errors
/// [`CliError::Message`] if the project is not initialized, no session is
/// running (socket unconnectable), or the reply did not report `ok:true`.
fn set_paused(layout: &Layout, paused: bool) -> Result<bool, CliError> {
    if !layout.is_initialized() {
        return Err(CliError::msg(
            "not a Tomo project (no .tomo/ here) — run `tomo init` first",
        ));
    }
    let verb = if paused { "pause" } else { "resume" };
    let mut stream = UnixStream::connect(layout.ctl_sock()).map_err(|_| {
        CliError::msg(format!(
            "no running session to {verb} — start one with `tomo sync`"
        ))
    })?;
    let cmd = serde_json::json!({ "type": verb });
    let hello = crate::ctl::proto::to_hello_command(&cmd);
    writeln!(stream, "{hello}")
        .and_then(|()| stream.flush())
        .map_err(|s| CliError::io("write to control socket", layout.ctl_sock(), s))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|s| CliError::io("read from control socket", layout.ctl_sock(), s))?;
    let value: serde_json::Value = serde_json::from_str(line.trim())
        .map_err(|e| CliError::msg(format!("malformed control-channel reply: {e}")))?;
    if value.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        let msg = value
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("command failed");
        return Err(CliError::msg(format!("could not {verb} syncing: {msg}")));
    }
    Ok(value
        .get("already")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false))
}
