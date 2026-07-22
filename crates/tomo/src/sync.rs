//! `tomo sync`: the primary foreground sync command.
//!
//! Unifies what used to be two steps (`tomo connect` then `tomo watch`):
//! - `tomo sync <host:/path>` records the peer if it is new (reusing `connect`'s
//!   write plumbing) and goes **straight** into the live session — the session's
//!   own bootstrap/handshake is the validation, so there is no separate
//!   validation pass.
//! - `tomo sync` with no target syncs against the recorded `[remote]`, or a
//!   `--local-peer <path>` directory, or runs watch-only if neither is set.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tomo_config::Config;

use crate::connect::{self, ConnectAction};
use crate::error::CliError;
use crate::layout::Layout;
use crate::report::Reporter;
use crate::session::{self, Mode};
use crate::transport::SshParams;
use crate::{current_dir, replica};

/// How long the detaching parent waits for its child's session lock + control
/// socket to appear before declaring the launch failed (bounded poll, UX-V2 §1).
const DETACH_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// How many trailing log lines the parent surfaces when a detached child fails
/// to come up in time (so the "already running" flock refusal — or any startup
/// error — reaches the user who ran `tomo sync -d`).
const DETACH_LOG_TAIL: usize = 20;

/// Run `tomo sync`.
///
/// Argument shapes:
/// - `target` (a single `host:/path`): record/confirm the SSH peer and sync over
///   SSH.
/// - `--local-peer <path>` (no target): sync with a local served peer.
/// - none of the above: sync against a configured `[remote]`, else watch-only.
///
/// `legacy_remote_path` is the removed second positional; when present it yields
/// a friendly "the two-argument form was removed" error ([`crate::target`]).
///
/// # Errors
/// [`CliError`] if the project is not initialized, the removed two-argument form
/// is used, `--local-peer` is combined with a target, the config/replica cannot
/// be loaded, the peer path is invalid, or the sync loop fails.
// One cohesive command; the flags (including the two detach bools) are its
// surface, and folding them into enums would only obscure the call site.
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
pub fn run(
    target: Option<String>,
    legacy_remote_path: Option<String>,
    local_peer: Option<PathBuf>,
    force: bool,
    json: bool,
    detach: bool,
    detached_child: bool,
) -> Result<(), CliError> {
    let root = current_dir()?;
    let layout = Layout::new(&root);
    if !layout.is_initialized() {
        return Err(CliError::msg(
            "not a Tomo project (no .tomo/ here) — run `tomo init` first",
        ));
    }

    // `--detach`: this invocation is the *parent*. Re-spawn ourselves detached
    // (same arguments minus `-d`, plus the hidden `--detached-child` marker),
    // wait for the child to bind its session, then print how to attach and exit.
    // The child never sees `detach == true` (the marker replaces it).
    if detach {
        return spawn_detached(&layout);
    }

    // The detached child: a controlling-terminal hangup (the launching terminal
    // closing) must not kill the background session. We cannot `setsid(2)` in the
    // child without `unsafe` (workspace `unsafe_code = "forbid"` rules out
    // `CommandExt::pre_exec`); the parent already put us in our own process group
    // (`CommandExt::process_group`, which is safe), and here we neutralize SIGHUP
    // via signal-hook (an existing dependency) so a stray hangup cannot terminate
    // the loop. SIGTERM/SIGINT still stop it (that is `tomo stop`'s clean path).
    if detached_child {
        ignore_sighup();
    }

    let replica = replica::load(&layout.replica())?;
    let config = Config::load(layout.root())?;
    let reporter = Reporter::human(json, crate::style::current());

    let mode = resolve_mode(
        &layout,
        &config,
        target,
        legacy_remote_path,
        local_peer,
        force,
        &reporter,
    )?;

    session::run(layout, config, replica, reporter, mode)
}

/// Refuse when the peer path overlaps the local project root — equal, an
/// ancestor, or a descendant ([`crate::overlap`]). Both paths are canonicalized so a
/// symlink or `..` cannot disguise the overlap; the local root always
/// canonicalizes (it is a live `.tomo` project), and a peer path that cannot be
/// canonicalized (e.g. a not-yet-created loopback remote dir) falls back to its
/// lexical form. The error names both trees.
///
/// # Errors
/// [`CliError::Message`] when the trees overlap.
fn refuse_if_overlapping(root: &std::path::Path, peer: &std::path::Path) -> Result<(), CliError> {
    let croot = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let cpeer = std::fs::canonicalize(peer).unwrap_or_else(|_| peer.to_path_buf());
    if crate::overlap::paths_overlap(&croot, &cpeer) {
        return Err(CliError::msg(format!(
            "refusing to sync: the peer path and this project overlap — they are the \
             same tree or one contains the other, which would sync the project against \
             itself (an unbounded echo loop).\n  project: {}\n  peer:    {}\n\
             Choose a peer directory outside this project.",
            croot.display(),
            cpeer.display(),
        )));
    }
    Ok(())
}

/// Decide which [`Mode`] this invocation runs in, recording the `[remote]` when
/// a target is supplied. Kept separate so the argument-shape rules read clearly.
#[allow(clippy::too_many_arguments)] // one cohesive decision; splitting would obscure it.
fn resolve_mode(
    layout: &Layout,
    config: &Config,
    target: Option<String>,
    legacy_remote_path: Option<String>,
    local_peer: Option<PathBuf>,
    force: bool,
    reporter: &Reporter,
) -> Result<Mode, CliError> {
    match (target, legacy_remote_path) {
        // A single `host:/path` target (any stray second positional is rejected
        // by crate::target::resolve with a helpful message). Record the peer if
        // new, then sync over SSH.
        (Some(target), legacy_remote_path) => {
            if local_peer.is_some() {
                return Err(CliError::msg(
                    "--local-peer cannot be combined with a host:/path target; \
                     choose one peer",
                ));
            }
            let (host, path) = crate::target::resolve(&target, legacy_remote_path.as_deref())?;
            // Overlapping-tree guard (best effort, SSH). Only decidable when the
            // peer is loopback — then the remote path is on THIS machine, so it
            // can be canonicalized and compared with the local root. A genuinely
            // remote host, or a config alias that merely resolves to localhost,
            // cannot be checked here (documented limit — crate::overlap).
            if crate::overlap::host_is_loopback(&host) {
                refuse_if_overlapping(layout.root(), std::path::Path::new(&path))?;
            }
            let (remote, action) = connect::apply_remote_config(layout, &host, &path, force, None)?;
            match action {
                ConnectAction::WriteAndValidate => {
                    reporter.note(&format!("recorded remote {host}:{path}"));
                }
                ConnectAction::RevalidateExisting => {
                    reporter.note(&format!("remote {host}:{path} already configured"));
                }
            }
            Ok(Mode::Ssh(SshParams::from_remote(&remote)?))
        }
        // Unreachable in practice: clap fills the `target` positional before the
        // hidden legacy one, so a legacy arg cannot appear without a target.
        // Kept total for safety with a message pointing at the single-arg form.
        (None, Some(_)) => Err(CliError::msg(
            "name the peer as a single 'host:/path' target (e.g. `tomo sync \
             user@host:/path`), or omit it to resume the recorded peer",
        )),
        // No target: local peer, else a configured remote, else watch-only.
        (None, None) => {
            if let Some(path) = local_peer {
                let resolved = std::fs::canonicalize(&path)
                    .map_err(|s| CliError::io("open --local-peer directory", &path, s))?;
                // Overlapping-tree guard: a local peer that IS the project, or
                // nests inside/around it, would sync the tree against itself
                // (an unbounded echo loop). Fully decidable here — both roots
                // are real local directories.
                refuse_if_overlapping(layout.root(), &resolved)?;
                Ok(Mode::LocalPeer(resolved))
            } else if let Some(remote) = &config.remote {
                Ok(Mode::Ssh(SshParams::from_remote(remote)?))
            } else {
                reporter.note(
                    "watching only — no peer configured; run `tomo sync user@host:/path` \
                     to connect",
                );
                Ok(Mode::WatchOnly)
            }
        }
    }
}

// ---- Detach (`tomo sync -d`) ---------------------------------------------

/// Re-spawn ourselves as a detached background session and wait for it to bind.
///
/// The child runs the *same* `sync` invocation with `-d`/`--detach` stripped and
/// the hidden `--detached-child` marker appended ([`detach_child_args`]), its
/// stdio redirected into `.tomo/logs/session.log`, and its own process group so
/// terminal signals never reach it. We poll (bounded, [`DETACH_READY_TIMEOUT`])
/// for the child to acquire the single-session lock and bind its control socket;
/// on success we print how to attach and exit `0`. If the child exits first —
/// e.g. the flock refuses a second session — we surface its last log lines.
fn spawn_detached(layout: &Layout) -> Result<(), CliError> {
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe()
        .map_err(|s| CliError::io("locate the tomo executable", "<current_exe>", s))?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let child_args = detach_child_args(&args);

    // The child's stdout+stderr go here (append). Its reporter runs non-tty, so
    // it writes plain, greppable lines; any startup error lands here too.
    std::fs::create_dir_all(layout.logs())
        .map_err(|s| CliError::io("create logs directory", layout.logs(), s))?;
    let log_path = layout.session_log();
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|s| CliError::io("open session log", &log_path, s))?;
    let _ = writeln!(
        log,
        "[{}] session starting (detached)",
        crate::status::now_unix_ms()
    );
    let _ = log.flush();
    let log_err = log
        .try_clone()
        .map_err(|s| CliError::io("duplicate session-log handle", &log_path, s))?;

    let mut cmd = Command::new(&exe);
    cmd.args(&child_args)
        .current_dir(layout.root())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        // Own process group: terminal-generated signals (Ctrl-C at the launching
        // shell) never reach the background session. Safe, unlike a pre_exec
        // setsid (which `unsafe_code = "forbid"` disallows).
        .process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|s| CliError::io("spawn detached session", &exe, s))?;
    let pid = child.id();

    let deadline = Instant::now() + DETACH_READY_TIMEOUT;
    loop {
        // The child exited before binding its session — surface why (the flock
        // "already running" refusal, a bad target, etc.) from its log tail.
        if let Some(status) = child
            .try_wait()
            .map_err(|s| CliError::io("wait on detached session", &exe, s))?
        {
            return Err(CliError::msg(format!(
                "could not start detached session (it exited{}):\n{}",
                exit_suffix(status.code()),
                log_tail(&log_path),
            )));
        }
        if session_ready(layout, pid) {
            crate::out::outln!(
                "session started (pid {pid}) — attach: tomo attach · stop: tomo stop"
            );
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(CliError::msg(format!(
                "detached session did not come up within {}s:\n{}",
                DETACH_READY_TIMEOUT.as_secs(),
                log_tail(&log_path),
            )));
        }
        // Bounded readiness poll for an external process to bind its socket —
        // there is no event to await here, so a short poll interval is the
        // sanctioned wait (mirrors the scenario harness's wait_for).
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Produce the detached child's argument list from this process's own arguments
/// (argv without argv[0]): the same `sync` invocation with the detach flag
/// (`-d`/`--detach`) removed and the hidden `--detached-child` marker appended.
/// Pure — the re-spawn's argv reconstruction, unit-tested without spawning.
fn detach_child_args(args: &[String]) -> Vec<String> {
    let mut out: Vec<String> = args
        .iter()
        .filter(|a| a.as_str() != "-d" && a.as_str() != "--detach")
        .cloned()
        .collect();
    out.push("--detached-child".to_owned());
    out
}

/// Whether the just-spawned child (pid `child_pid`) has bound its session: its
/// control socket is present AND the single-session lock records *its* pid. The
/// pid check is what distinguishes our child coming up from a pre-existing
/// session whose socket was already there (the "already running" case), so we
/// never mistake someone else's live session for our child's success.
fn session_ready(layout: &Layout, child_pid: u32) -> bool {
    use std::os::unix::fs::FileTypeExt as _;
    let sock = layout.ctl_sock();
    let bound = std::fs::metadata(&sock).is_ok_and(|m| m.file_type().is_socket());
    bound && crate::lockfile::recorded_pid(layout) == Some(child_pid)
}

/// The last [`DETACH_LOG_TAIL`] lines of the session log, for surfacing a failed
/// detached launch. Best-effort: an unreadable/absent log yields a short note.
fn log_tail(path: &std::path::Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(text) => crate::logs::tail_lines(&text, DETACH_LOG_TAIL).join("\n"),
        Err(_) => "(no session log to show)".to_owned(),
    }
}

/// A `" (exit N)"` suffix when an exit code is known, else empty.
fn exit_suffix(code: Option<i32>) -> String {
    code.map_or_else(String::new, |c| format!(" (exit {c})"))
}

/// Neutralize SIGHUP in the detached child so the launching terminal closing
/// cannot terminate the background session. Installing any handler replaces the
/// default terminate action; signal-hook retains its own reference to the flag,
/// so dropping ours here is fine. SIGTERM/SIGINT still stop the session (the
/// clean `tomo stop` path).
fn ignore_sighup() {
    let sink = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _ = signal_hook::flag::register(signal_hook::consts::SIGHUP, sink);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn detach_child_args_strips_short_flag_and_adds_marker() {
        let got = detach_child_args(&args(&["sync", "-d", "--local-peer", "/b"]));
        assert_eq!(
            got,
            args(&["sync", "--local-peer", "/b", "--detached-child"])
        );
    }

    #[test]
    fn detach_child_args_strips_long_flag() {
        let got = detach_child_args(&args(&["sync", "--detach", "user@host:/srv"]));
        assert_eq!(got, args(&["sync", "user@host:/srv", "--detached-child"]));
    }

    #[test]
    fn detach_child_args_preserves_other_flags_and_order() {
        let got = detach_child_args(&args(&["sync", "host:/p", "--force", "--json"]));
        assert_eq!(
            got,
            args(&["sync", "host:/p", "--force", "--json", "--detached-child"])
        );
    }

    #[test]
    fn exit_suffix_formats_only_when_known() {
        assert_eq!(exit_suffix(Some(1)), " (exit 1)");
        assert_eq!(exit_suffix(None), "");
    }
}
