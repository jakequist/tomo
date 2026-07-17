//! Stdout helpers for informational commands that survive a closed reader.
//!
//! `tomo log <path> | head` closes the pipe once `head` has the lines it wants;
//! the CLI's next write then fails with `EPIPE`. `println!` *panics* on that
//! error ("failed printing to stdout"), so an ordinary, correct shell pipeline
//! would crash the CLI with a backtrace and a non-zero exit.
//!
//! Because `unsafe_code` is forbidden workspace-wide we cannot reset the
//! `SIGPIPE` disposition to `SIG_DFL` via `libc::signal`. Instead every
//! informational command prints through these helpers, which treat a broken
//! pipe as a normal end-of-consumer: they stop writing and exit `0` quietly.
//! Any other write error is swallowed too (there is nothing sensible a print
//! path can do about it), so a print never panics.

use std::io::{self, ErrorKind, Write};
use std::process::exit;

use crate::error::CliError;

/// How a guarded stdout write turned out. Kept separate from the process-exiting
/// wrappers so the broken-pipe classification is a pure, unit-testable function.
#[derive(Debug, PartialEq, Eq)]
enum PipeOutcome {
    /// The write (and flush) succeeded.
    Wrote,
    /// The reader closed the pipe (`EPIPE`); the caller should exit `0` quietly.
    BrokenPipe,
    /// Some other I/O error occurred; nothing a print path can usefully do.
    Failed,
}

/// Write `data` to `w`, flush, and classify the result **without** touching the
/// process. Pure over its writer so the broken-pipe handling is testable with a
/// fake writer (a real broken pipe cannot be produced in a unit test).
fn guarded_write<W: Write>(w: &mut W, data: &[u8]) -> PipeOutcome {
    match w.write_all(data).and_then(|()| w.flush()) {
        Ok(()) => PipeOutcome::Wrote,
        Err(e) if e.kind() == ErrorKind::BrokenPipe => PipeOutcome::BrokenPipe,
        Err(_) => PipeOutcome::Failed,
    }
}

/// Emit `data` to stdout, exiting `0` on a broken pipe.
fn emit(data: &[u8]) {
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    // A closed downstream reader is a normal end-of-output: exit 0 quietly.
    // `Wrote` and `Failed` both fall through — a print helper has no meaningful
    // recovery for a non-pipe write error and must never panic.
    if guarded_write(&mut lock, data) == PipeOutcome::BrokenPipe {
        exit(0);
    }
}

/// Print one line (its arguments plus a trailing newline) to stdout, exiting `0`
/// quietly if the reader has closed the pipe. The stdout replacement for
/// `println!` in every informational command.
pub(crate) fn line(args: std::fmt::Arguments) {
    let mut text = format!("{args}");
    text.push('\n');
    emit(text.as_bytes());
}

/// Write raw bytes to stdout (for `tomo restore --stdout`), exiting `0` on a
/// broken pipe.
///
/// # Errors
/// [`CliError`] on a non-pipe write failure, so the caller surfaces it like any
/// other I/O error; a broken pipe exits the process `0` and never returns.
pub(crate) fn bytes(data: &[u8]) -> Result<(), CliError> {
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    match guarded_write(&mut lock, data) {
        PipeOutcome::Wrote => Ok(()),
        PipeOutcome::BrokenPipe => exit(0),
        PipeOutcome::Failed => Err(CliError::msg("could not write to stdout")),
    }
}

/// `println!`-style line printing that survives a closed pipe. Delegates to
/// [`line`]; use it anywhere an informational command would otherwise
/// `println!`.
macro_rules! outln {
    () => { $crate::out::line(std::format_args!("")) };
    ($($arg:tt)*) => { $crate::out::line(std::format_args!($($arg)*)) };
}

pub(crate) use outln;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A writer that always fails with the given [`ErrorKind`] on the first
    /// write, standing in for a real broken pipe (unproducible in a unit test).
    struct FailingWriter(ErrorKind);
    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(self.0, "simulated"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn broken_pipe_is_classified_not_written() {
        let mut w = FailingWriter(ErrorKind::BrokenPipe);
        assert_eq!(guarded_write(&mut w, b"anything"), PipeOutcome::BrokenPipe);
    }

    #[test]
    fn normal_write_succeeds_and_reaches_the_writer() {
        let mut buf: Vec<u8> = Vec::new();
        assert_eq!(guarded_write(&mut buf, b"hello\n"), PipeOutcome::Wrote);
        assert_eq!(buf, b"hello\n");
    }

    #[test]
    fn other_errors_are_failed_not_pipe() {
        let mut w = FailingWriter(ErrorKind::PermissionDenied);
        assert_eq!(guarded_write(&mut w, b"x"), PipeOutcome::Failed);
    }
}
