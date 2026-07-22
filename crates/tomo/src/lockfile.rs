//! Single-session lock, one per project (`.tomo/state/session.lock`).
//!
//! Only ever **one** live `tomo sync` / `tomo serve` session may touch a
//! project's tree and history DB at a time — two racing sessions would fight
//! over the index, staging, and the `SQLite` store. [`SessionLock`] enforces that
//! with an exclusive advisory `flock` (via `fd-lock`) held for the session's
//! whole lifetime; dropping it releases the lock.
//!
//! **The lock is the flock, never the file's contents.** The kernel drops an
//! `flock` when the holding process exits — including a `kill -9` — so there is
//! deliberately no stale-pidfile / staleness logic to get wrong. The bytes we
//! write into the file (`pid`, `mode`, `since_unix_ms`) are pure diagnostics,
//! read back only to make the "already running" error message helpful.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Seek, SeekFrom, Write};

use fd_lock::{RwLock, RwLockWriteGuard};

use crate::error::CliError;
use crate::layout::Layout;
use crate::status::now_unix_ms;

/// A held single-session lock. Alive for as long as the value is kept; its
/// `Drop` releases the underlying `flock`.
#[derive(Debug)]
pub struct SessionLock {
    // The guard's `Drop` runs `flock(LOCK_UN)`, releasing the lock. It borrows
    // the `RwLock<File>`, which we intentionally leak (see `acquire`) to give
    // the guard a `'static` lifetime: the lock lives for the entire session, so
    // a one-time leak of a single open file handle is the price of a `'static`
    // guard with no `unsafe` and no self-referential struct. Named `_guard`
    // because it is never read — only its `Drop` matters.
    _guard: RwLockWriteGuard<'static, File>,
}

impl SessionLock {
    /// Acquire the project's single-session lock, writing `mode` diagnostics on
    /// success.
    ///
    /// `mode` is `"sync"` or `"serve"` — recorded in the file so a contending
    /// process's error message can say what already holds it.
    ///
    /// # Errors
    /// [`CliError::Message`] if another session already holds the lock (with the
    /// holder's pid and age when readable), or [`CliError::Io`] if the state
    /// directory or lock file cannot be created/opened.
    pub fn acquire(layout: &Layout, mode: &str) -> Result<SessionLock, CliError> {
        let dir = layout.state();
        std::fs::create_dir_all(&dir)
            .map_err(|s| CliError::io("create state directory", &dir, s))?;
        let path = layout.session_lock();
        // Open without truncating: a running holder's diagnostics must survive
        // until *we* win the lock and overwrite them ourselves.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|s| CliError::io("open session lock", &path, s))?;

        // Leak the `RwLock<File>` so the write guard can be `'static`. The lock
        // is process-lifetime; releasing it happens via the guard's `Drop`, not
        // by freeing this allocation.
        let lock: &'static mut RwLock<File> = Box::leak(Box::new(RwLock::new(file)));
        match lock.try_write() {
            Ok(mut guard) => {
                write_diagnostics(&mut guard, mode)?;
                Ok(SessionLock { _guard: guard })
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => Err(already_running_error(&path)),
            Err(e) => Err(CliError::io("acquire session lock", &path, e)),
        }
    }
}

/// Overwrite the lock file with this session's diagnostics (pid, mode, start
/// time). Called only after the exclusive lock is held, so no other session can
/// observe a torn write.
fn write_diagnostics(
    guard: &mut RwLockWriteGuard<'static, File>,
    mode: &str,
) -> Result<(), CliError> {
    let body = format!(
        "pid={}\nmode={}\nsince_unix_ms={}\n",
        std::process::id(),
        mode,
        now_unix_ms()
    );
    let file: &mut File = guard;
    file.set_len(0)
        .map_err(|s| CliError::io("truncate session lock", "<session.lock>", s))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|s| CliError::io("rewind session lock", "<session.lock>", s))?;
    file.write_all(body.as_bytes())
        .map_err(|s| CliError::io("write session lock", "<session.lock>", s))?;
    file.flush()
        .map_err(|s| CliError::io("flush session lock", "<session.lock>", s))
}

/// Whether a live session currently holds this project's single-session lock.
///
/// A non-destructive probe used by `tomo stop` to tell a wedged-but-running
/// session apart from nothing-running: it opens the lock file and *tries* the
/// exclusive `flock`. A `WouldBlock` means a live session holds it (a session is
/// running); a successful acquire means the lock is free (nothing running) — we
/// release it immediately by dropping the guard **without** writing diagnostics,
/// so the probe never disturbs the file a real session would write. A missing
/// lock file (no session ever started) also reads as not-running.
///
/// The kernel releases an `flock` on process exit (even `kill -9`), so this is
/// authoritative with no staleness logic (the same guarantee [`SessionLock`]
/// relies on).
#[must_use]
pub fn session_running(layout: &Layout) -> bool {
    let path = layout.session_lock();
    let Ok(file) = OpenOptions::new()
        .read(true)
        .write(true)
        .create(false)
        .open(&path)
    else {
        // No lock file at all → no session has ever started here.
        return false;
    };
    let mut lock = RwLock::new(file);
    let running = match lock.try_write() {
        // We won the lock → nobody holds it → no live session. Drop the guard
        // (releases immediately) without writing anything.
        Ok(_guard) => false,
        // Contended → a live session holds it.
        Err(e) if e.kind() == ErrorKind::WouldBlock => true,
        // Any other error (permissions, etc.): treat as not-running so `stop`
        // falls back to its idempotent no-op rather than a spurious SIGTERM.
        Err(_) => false,
    };
    running
}

/// The pid recorded in this project's session-lock file, if any — the holding
/// session's process id (its diagnostics). Best-effort: `None` when the file is
/// missing, unreadable, or records no pid. Used by `tomo sync -d` (to confirm
/// its own child bound the session) and `tomo stop` (the SIGTERM-fallback
/// target).
#[must_use]
pub fn recorded_pid(layout: &Layout) -> Option<u32> {
    let text = std::fs::read_to_string(layout.session_lock()).ok()?;
    parse_diagnostics(&text).pid
}

/// The diagnostics recorded in a lock file by its holder.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Diagnostics {
    /// The holder's process id, if recorded.
    pub pid: Option<u32>,
    /// The holder's session mode (`sync`/`serve`), if recorded.
    pub mode: Option<String>,
    /// The wall-clock start time in unix milliseconds (display only), if
    /// recorded.
    pub since_unix_ms: Option<u64>,
}

/// Parse the `key=value` diagnostics a holder wrote into the lock file. Unknown
/// or malformed lines are ignored so a partially written or future-format file
/// never turns a lock error into a parse crash.
pub fn parse_diagnostics(text: &str) -> Diagnostics {
    let mut d = Diagnostics::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "pid" => d.pid = value.trim().parse().ok(),
            "mode" => d.mode = Some(value.trim().to_owned()),
            "since_unix_ms" => d.since_unix_ms = value.trim().parse().ok(),
            _ => {}
        }
    }
    d
}

/// Build the "another session is already running" error, reading the holder's
/// diagnostics for a helpful message (best-effort — an unreadable file still
/// yields a clear, if vaguer, error).
fn already_running_error(path: &std::path::Path) -> CliError {
    let who = match std::fs::read_to_string(path) {
        Ok(text) => describe_holder(&parse_diagnostics(&text)),
        Err(_) => "another process holds the lock".to_owned(),
    };
    CliError::msg(format!(
        "another tomo session is already running for this project ({who}) — stop it \
         first, or wait for it to exit"
    ))
}

/// Render a holder's diagnostics as a human phrase, e.g.
/// `pid 4213 (sync) since 12s ago`.
fn describe_holder(d: &Diagnostics) -> String {
    use std::fmt::Write as _;
    let mut s = match d.pid {
        Some(pid) => format!("pid {pid}"),
        None => "another process".to_owned(),
    };
    // Writing to a String is infallible.
    if let Some(mode) = &d.mode {
        let _ = write!(s, " ({mode})");
    }
    if let Some(since) = d.since_unix_ms {
        let _ = write!(s, " since {}", humanize_since(since));
    }
    s
}

/// Render a unix-millisecond start time as a coarse "N ago" phrase, using the
/// wall clock for **display only** (never for ordering — invariant #7).
fn humanize_since(since_unix_ms: u64) -> String {
    let now = now_unix_ms();
    let secs = now.saturating_sub(since_unix_ms) / 1000;
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::layout::Layout;

    fn temp_layout() -> (tempfile::TempDir, Layout) {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        std::fs::create_dir_all(layout.state()).unwrap();
        (dir, layout)
    }

    #[test]
    fn acquire_writes_diagnostics() {
        let (_dir, layout) = temp_layout();
        let _lock = SessionLock::acquire(&layout, "sync").unwrap();
        let text = std::fs::read_to_string(layout.session_lock()).unwrap();
        let d = parse_diagnostics(&text);
        assert_eq!(d.pid, Some(std::process::id()));
        assert_eq!(d.mode.as_deref(), Some("sync"));
        assert!(d.since_unix_ms.is_some());
    }

    #[test]
    fn second_handle_contends() {
        let (_dir, layout) = temp_layout();
        let first = SessionLock::acquire(&layout, "sync").unwrap();
        // A second acquire on the same project must fail while the first is held.
        let err = SessionLock::acquire(&layout, "serve").unwrap_err();
        assert!(
            err.to_string().contains("already running"),
            "unexpected error: {err}"
        );
        drop(first);
    }

    #[test]
    fn releases_on_drop() {
        let (_dir, layout) = temp_layout();
        let first = SessionLock::acquire(&layout, "sync").unwrap();
        drop(first);
        // Once dropped, the lock is free to re-acquire (kernel released the flock).
        let second = SessionLock::acquire(&layout, "sync").unwrap();
        drop(second);
    }

    #[test]
    fn parse_diagnostics_tolerates_junk() {
        let d =
            parse_diagnostics("pid=99\ngarbage line\nmode=serve\nsince_unix_ms=1700000000000\n");
        assert_eq!(d.pid, Some(99));
        assert_eq!(d.mode.as_deref(), Some("serve"));
        assert_eq!(d.since_unix_ms, Some(1_700_000_000_000));

        // A totally empty / malformed file yields all-None, never a panic.
        let empty = parse_diagnostics("");
        assert_eq!(empty, Diagnostics::default());
    }

    #[test]
    fn describe_holder_is_readable() {
        let d = Diagnostics {
            pid: Some(4213),
            mode: Some("sync".to_owned()),
            since_unix_ms: Some(now_unix_ms()),
        };
        let s = describe_holder(&d);
        assert!(s.contains("pid 4213"), "got: {s}");
        assert!(s.contains("sync"), "got: {s}");
        assert!(s.contains("ago"), "got: {s}");
    }
}
