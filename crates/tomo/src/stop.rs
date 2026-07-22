//! `tomo stop`: cleanly stop the running background session (UX-V2 §1).
//!
//! A decision ladder over three probed states:
//! 1. **socket responsive** — the control channel accepted our `stop` command;
//!    wait (bounded) for the clean exit and report the pid.
//! 2. **socket dead but lock held** — a session is wedged; fall back to SIGTERM
//!    on the recorded pid (the session's own clean-shutdown path) and report
//!    which route was used. If it still will not die, the message *suggests*
//!    `kill -9` but never runs it.
//! 3. **nothing running** — idempotent: a clean note, exit `0`.
//!
//! The ladder itself ([`stop_decision`]) is a pure function over the probe so it
//! is unit-tested without any I/O; `run` performs the probing and the actions.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use crate::error::CliError;
use crate::layout::Layout;
use crate::out::outln;

/// How long to wait for a session to exit after a `stop` command or SIGTERM.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Poll interval while waiting for the session to release its lock and socket.
const POLL: Duration = Duration::from_millis(100);

/// What a probe of the running session found — the input to [`stop_decision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopProbe {
    /// The control socket answered our `stop` command (the clean path).
    SocketResponsive,
    /// The socket did not answer, but a session still holds the lock (wedged).
    SocketDeadLockHeld,
    /// Nothing is running: no socket answer and the lock is free.
    NothingRunning,
}

/// What `tomo stop` should do, given a [`StopProbe`]. Pure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopDecision {
    /// The control channel accepted the stop; wait for the exit.
    AwaitCleanExit,
    /// Fall back to SIGTERM on the recorded pid (wedged session).
    Sigterm,
    /// Nothing to do — idempotent no-op.
    Noop,
}

/// The decision ladder: map a probed session state to the action to take. Pure.
pub(crate) fn stop_decision(probe: StopProbe) -> StopDecision {
    match probe {
        StopProbe::SocketResponsive => StopDecision::AwaitCleanExit,
        StopProbe::SocketDeadLockHeld => StopDecision::Sigterm,
        StopProbe::NothingRunning => StopDecision::Noop,
    }
}

/// Run `tomo stop`.
///
/// # Errors
/// [`CliError::Message`] if the project is not initialized, a wedged session
/// cannot be signalled (unknown pid), or it will not exit after SIGTERM.
pub fn run(layout: &Layout) -> Result<(), CliError> {
    if !layout.is_initialized() {
        return Err(CliError::msg(
            "not a Tomo project (no .tomo/ here) — run `tomo init` first",
        ));
    }
    // Read the holder's pid up front, before the socket/lock disappear.
    let pid = crate::lockfile::recorded_pid(layout);

    match stop_decision(probe(layout)) {
        StopDecision::AwaitCleanExit => {
            if wait_for_exit(layout, STOP_TIMEOUT) {
                report_stopped(pid);
                Ok(())
            } else {
                // The socket accepted the stop but the session did not exit in
                // time; escalate to the SIGTERM path (same as a wedged session).
                sigterm_path(layout, pid)
            }
        }
        StopDecision::Sigterm => sigterm_path(layout, pid),
        StopDecision::Noop => {
            outln!("no running session — nothing to stop");
            Ok(())
        }
    }
}

/// Probe the session: try the control channel first, then fall back to the lock.
fn probe(layout: &Layout) -> StopProbe {
    if try_send_stop(layout) {
        StopProbe::SocketResponsive
    } else if crate::lockfile::session_running(layout) {
        StopProbe::SocketDeadLockHeld
    } else {
        StopProbe::NothingRunning
    }
}

/// Connect to the control socket, send the `stop` command, and return whether it
/// replied `{"ok":true}`. Any connect/write/parse failure returns `false` (the
/// socket is absent or unresponsive), which drives the ladder to the lock probe.
fn try_send_stop(layout: &Layout) -> bool {
    let Ok(mut stream) = UnixStream::connect(layout.ctl_sock()) else {
        return false;
    };
    let hello = crate::ctl::proto::to_hello_command(&serde_json::json!({ "type": "stop" }));
    if writeln!(stream, "{hello}")
        .and_then(|()| stream.flush())
        .is_err()
    {
        return false;
    }
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(line.trim())
        .ok()
        .and_then(|v| v.get("ok").and_then(serde_json::Value::as_bool))
        .unwrap_or(false)
}

/// Poll until the session has released its lock *and* removed its socket, or the
/// timeout elapses. Returns whether it exited.
fn wait_for_exit(layout: &Layout, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !crate::lockfile::session_running(layout) && !layout.ctl_sock().exists() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        // Bounded poll for the session process to tear down — no event to await
        // across processes, so a short interval is the sanctioned wait.
        std::thread::sleep(POLL);
    }
}

/// The SIGTERM fallback for a wedged (or slow-to-exit) session: signal the
/// recorded pid and wait for the exit. Reports the route used, or errors with a
/// `kill -9` suggestion if it still will not die (we never `-9` it ourselves).
fn sigterm_path(layout: &Layout, pid: Option<u32>) -> Result<(), CliError> {
    let Some(pid) = pid else {
        return Err(CliError::msg(
            "a session appears wedged but its pid is unknown (no lock diagnostics) — \
             cannot signal it; inspect `tomo status` and stop it manually if needed",
        ));
    };
    send_sigterm(pid)?;
    if wait_for_exit(layout, STOP_TIMEOUT) {
        outln!("stopped session (pid {pid}) via SIGTERM (the control socket was unresponsive)");
        Ok(())
    } else {
        Err(CliError::msg(format!(
            "session (pid {pid}) did not exit after SIGTERM — it may be wedged; if it \
             stays stuck, force it with `kill -9 {pid}` (tomo will not do that for you)"
        )))
    }
}

/// Send SIGTERM to `pid` via the POSIX `kill` utility. We shell out rather than
/// call `libc::kill`, because `unsafe_code = "forbid"` rules out the FFI and we
/// add no new dependency for one signal; `kill(1)` is present on every unix.
fn send_sigterm(pid: u32) -> Result<(), CliError> {
    let status = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .map_err(|s| CliError::io("run kill(1)", "kill", s))?;
    if status.success() {
        Ok(())
    } else {
        Err(CliError::msg(format!(
            "could not signal session pid {pid} (already gone?)"
        )))
    }
}

/// Report a clean stop, naming the pid when known.
fn report_stopped(pid: Option<u32>) {
    match pid {
        Some(pid) => outln!("stopped session (pid {pid})"),
        None => outln!("stopped session"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn ladder_socket_responsive_awaits_clean_exit() {
        assert_eq!(
            stop_decision(StopProbe::SocketResponsive),
            StopDecision::AwaitCleanExit
        );
    }

    #[test]
    fn ladder_dead_socket_but_lock_held_falls_back_to_sigterm() {
        assert_eq!(
            stop_decision(StopProbe::SocketDeadLockHeld),
            StopDecision::Sigterm
        );
    }

    #[test]
    fn ladder_nothing_running_is_a_noop() {
        assert_eq!(stop_decision(StopProbe::NothingRunning), StopDecision::Noop);
    }
}
