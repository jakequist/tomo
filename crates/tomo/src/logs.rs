//! `tomo logs [-f] [-n N]`: print (and optionally follow) the detached session
//! log at `.tomo/logs/session.log` (UX-V2 §1).
//!
//! A `tomo sync -d` child has its stdout/stderr redirected into that file, so
//! this is a plain tail over it. It works with or without a running session (it
//! only reads the file); `-f` follows by polling every [`FOLLOW_POLL`]. Ctrl-C
//! exits (the default SIGINT terminates this client and never touches the
//! session).

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::Path;
use std::time::Duration;

use crate::error::CliError;
use crate::layout::Layout;
use crate::out::outln;

/// Trailing lines shown when `-n` is not given.
const DEFAULT_TAIL: usize = 50;

/// Poll interval for follow mode (`-f`).
const FOLLOW_POLL: Duration = Duration::from_millis(200);

/// Run `tomo logs`.
///
/// # Errors
/// [`CliError::Message`] if the project is not initialized; [`CliError::Io`] on
/// a read/seek failure while following.
pub fn run(layout: &Layout, follow: bool, lines: Option<usize>) -> Result<(), CliError> {
    if !layout.is_initialized() {
        return Err(CliError::msg(
            "not a Tomo project (no .tomo/ here) — run `tomo init` first",
        ));
    }
    let path = layout.session_log();
    let n = lines.unwrap_or(DEFAULT_TAIL);

    // The whole current file (empty string when absent). Its byte length is the
    // follow offset — everything appended after this point is streamed by `-f`.
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    if content.is_empty() && !path.exists() {
        outln!(
            "no session log yet at {} — start a background session with `tomo sync -d`",
            path.display()
        );
    } else {
        for line in tail_lines(&content, n) {
            outln!("{line}");
        }
    }

    if follow {
        follow_log(&path, content.len() as u64)?;
    }
    Ok(())
}

/// The last `n` lines of `content` (split on `'\n'`), oldest-first. A single
/// trailing newline is not treated as a final empty line; `n == 0` yields
/// nothing. Pure — the tail selection, unit-tested over a line buffer.
pub(crate) fn tail_lines(content: &str, n: usize) -> Vec<&str> {
    if n == 0 {
        return Vec::new();
    }
    let trimmed = content.strip_suffix('\n').unwrap_or(content);
    if trimmed.is_empty() {
        return Vec::new();
    }
    let all: Vec<&str> = trimmed.split('\n').collect();
    let start = all.len().saturating_sub(n);
    all[start..].to_vec()
}

/// Follow the log from byte `start`, printing complete lines as they are
/// appended. Reopens the file each poll so a not-yet-created (or rotated) log is
/// picked up; a shrink (truncate/rotate) restarts from the new beginning. Only
/// whole lines are emitted — a partial trailing write is held until its newline
/// arrives. Runs until interrupted (Ctrl-C).
fn follow_log(path: &Path, start: u64) -> Result<(), CliError> {
    let mut offset = start;
    loop {
        if let Ok(mut f) = std::fs::File::open(path) {
            let len = f.metadata().map_or(offset, |m| m.len());
            if len < offset {
                offset = 0; // truncated or rotated: start over
            }
            if len > offset {
                f.seek(SeekFrom::Start(offset))
                    .map_err(|s| CliError::io("seek session log", path, s))?;
                let mut buf = Vec::new();
                f.read_to_end(&mut buf)
                    .map_err(|s| CliError::io("read session log", path, s))?;
                // Advance only past the last complete line; hold any remainder.
                if let Some(nl) = buf.iter().rposition(|&b| b == b'\n') {
                    let complete = String::from_utf8_lossy(&buf[..nl]);
                    for line in complete.split('\n') {
                        outln!("{line}");
                    }
                    offset += (nl as u64) + 1;
                }
            }
        }
        // Bounded poll for appended bytes — there is no event to await on a
        // plain file, so a short interval is the sanctioned tail-follow wait.
        std::thread::sleep(FOLLOW_POLL);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn tail_returns_last_n_lines() {
        assert_eq!(tail_lines("a\nb\nc\n", 2), vec!["b", "c"]);
        assert_eq!(tail_lines("a\nb\nc", 2), vec!["b", "c"]);
    }

    #[test]
    fn tail_caps_at_available_lines() {
        assert_eq!(tail_lines("a\nb\n", 10), vec!["a", "b"]);
    }

    #[test]
    fn tail_handles_empty_and_zero() {
        assert!(tail_lines("", 5).is_empty());
        assert!(tail_lines("\n", 5).is_empty());
        assert!(tail_lines("a\nb\n", 0).is_empty());
    }

    #[test]
    fn tail_single_trailing_newline_not_a_blank_line() {
        // One trailing newline is the line terminator, not an extra empty line.
        assert_eq!(tail_lines("only\n", 3), vec!["only"]);
    }
}
