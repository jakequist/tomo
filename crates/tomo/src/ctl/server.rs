//! The control socket server: a std-library [`UnixListener`] on a dedicated
//! accept thread, one handler thread per connection. No async runtime, no new
//! dependencies.
//!
//! Every session (a `tomo sync` loop and a remote `tomo serve --stdio` loop
//! alike) runs one of these, bound at `.tomo/state/ctl.sock`. A stale socket
//! from a `kill -9`'d predecessor is removed at [`ControlServer::start`] — the
//! single-session flock (already held) guarantees no live owner — and the socket
//! is removed again on clean shutdown ([`ControlServer::stop`], also run from
//! `Drop`).
//!
//! Command handlers reuse the exact functions the CLI one-shot commands use
//! (`status`, `conflicts_cmd`), so the socket grants no powers the CLI lacks
//! (UX-V2 §5): DB writes go through `HistoryStore::open`'s 5 s busy timeout and
//! tree writes flow through the crash-safe apply path, identical to a
//! second-terminal `tomo conflicts resolve`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use serde_json::json;

use crate::error::CliError;
use crate::layout::Layout;
use crate::session::Incoming;

use super::broadcast::Broadcaster;
use super::proto::{self, ClientHello, ClientMode, Command, ResolveAction};

/// Everything a command handler needs to act with the same authority as the CLI.
pub struct CommandContext {
    layout: Layout,
    /// The session's shutdown flag (set by a `stop` command).
    shutdown: Arc<AtomicBool>,
    /// The session's unified event channel, so `stop` can wake the pump loop
    /// promptly rather than waiting out its idle timeout. Behind a `Mutex` so the
    /// context is `Sync` and can be shared across connection threads (an
    /// `mpsc::Sender` is `Send` but not `Sync`).
    tx: Mutex<Sender<Incoming>>,
}

impl CommandContext {
    /// Build a command context from the session's layout, shutdown flag, and
    /// unified-channel sender.
    #[must_use]
    pub fn new(layout: Layout, shutdown: Arc<AtomicBool>, tx: Sender<Incoming>) -> Self {
        CommandContext {
            layout,
            shutdown,
            tx: Mutex::new(tx),
        }
    }
}

/// A running control server. Owns the accept thread and tears the socket down on
/// [`stop`](ControlServer::stop) / `Drop`.
pub struct ControlServer {
    path: PathBuf,
    shutdown: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
    broadcaster: Arc<Broadcaster>,
    stopped: bool,
}

impl ControlServer {
    /// Bind the control socket and spawn the accept thread.
    ///
    /// # Errors
    /// [`CliError::Io`] if the state directory or socket cannot be created/bound.
    pub fn start(
        layout: &Layout,
        broadcaster: Arc<Broadcaster>,
        ctx: CommandContext,
    ) -> Result<Self, CliError> {
        let path = layout.ctl_sock();
        // Remove any stale socket from a dead predecessor before binding (the
        // held single-session flock guarantees no live owner). A leftover file
        // would otherwise make `bind` fail with EADDRINUSE.
        let _ = std::fs::remove_file(&path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|s| CliError::io("create state dir for control socket", parent, s))?;
        }
        let listener =
            UnixListener::bind(&path).map_err(|s| CliError::io("bind control socket", &path, s))?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let ctx = Arc::new(ctx);
        let accept = {
            let shutdown = Arc::clone(&shutdown);
            let broadcaster = Arc::clone(&broadcaster);
            std::thread::Builder::new()
                .name("tomo-ctl-accept".to_owned())
                .spawn(move || accept_loop(&listener, &shutdown, &broadcaster, &ctx))
                .map_err(|s| CliError::io("spawn control accept thread", &path, s))?
        };

        Ok(ControlServer {
            path,
            shutdown,
            accept: Some(accept),
            broadcaster,
            stopped: false,
        })
    }

    /// Clean shutdown: close event subscribers, stop the accept thread, remove
    /// the socket file. Idempotent.
    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        // End every events subscription so its handler thread unblocks and exits.
        self.broadcaster.close_all();
        // Ask the accept loop to stop, then wake its blocking `accept()` with a
        // throwaway self-connection so it observes the flag and returns.
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.path);
        if let Some(handle) = self.accept.take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Accept connections until the shutdown flag is set (a self-connection wakes
/// the blocking `accept`). Each connection is handled on its own thread.
fn accept_loop(
    listener: &UnixListener,
    shutdown: &AtomicBool,
    broadcaster: &Arc<Broadcaster>,
    ctx: &Arc<CommandContext>,
) {
    for stream in listener.incoming() {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let Ok(stream) = stream else { break };
        let broadcaster = Arc::clone(broadcaster);
        let ctx = Arc::clone(ctx);
        // A failed spawn just drops this connection; the session is unaffected.
        let _ = std::thread::Builder::new()
            .name("tomo-ctl-conn".to_owned())
            .spawn(move || {
                let _ = handle_conn(stream, &broadcaster, &ctx);
            });
    }
}

/// Handle one client connection: read the mode line, then stream events or run
/// one command.
fn handle_conn(
    stream: UnixStream,
    broadcaster: &Arc<Broadcaster>,
    ctx: &Arc<CommandContext>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(()); // client closed before saying anything
    }
    let hello: Option<ClientHello> = serde_json::from_str(line.trim()).ok();
    let Some(hello) = hello else {
        let _ = writeln!(
            writer,
            "{}",
            proto::err_reply("malformed hello (expected {\"v\":1,\"mode\":\"events|command\"})")
        );
        return Ok(());
    };
    if hello.v != proto::PROTOCOL_V {
        let _ = writeln!(
            writer,
            "{}",
            proto::err_reply("unsupported control-protocol version")
        );
        return Ok(());
    }

    match hello.mode {
        ClientMode::Events => {
            stream_events(writer, broadcaster);
            Ok(())
        }
        ClientMode::Command => {
            let reply = run_command(hello.cmd, ctx);
            writeln!(writer, "{reply}")?;
            writer.flush()
        }
    }
}

/// Stream event records to a subscriber until it disconnects or the session
/// shuts down. A subscriber dropped for lagging gets one final `lagged` line.
/// Every write failure just ends the stream (the client went away), so this
/// never surfaces an error to the caller.
fn stream_events(mut writer: UnixStream, broadcaster: &Arc<Broadcaster>) {
    let sub = broadcaster.subscribe();
    while let Some(msg) = sub.recv() {
        if writeln!(writer, "{msg}")
            .and_then(|()| writer.flush())
            .is_err()
        {
            return; // client went away; drop the subscription
        }
    }
    if sub.lagged() {
        // Best-effort: the client may already be gone.
        let _ = writeln!(writer, "{}", proto::to_line(&super::proto::Event::Lagged));
    }
}

/// Execute one command and return its reply line.
fn run_command(cmd: Option<Command>, ctx: &CommandContext) -> String {
    let Some(cmd) = cmd else {
        return proto::err_reply("command mode requires a \"cmd\" object");
    };
    match cmd {
        Command::Ping => proto::ok_reply(&json!({ "pong": true })),
        Command::Status => cmd_status(ctx),
        Command::ConflictsList { all } => cmd_conflicts_list(ctx, all),
        Command::ConflictShow { id } => cmd_conflict_show(ctx, id),
        Command::ConflictsResolve { id, action } => cmd_conflicts_resolve(ctx, id, action),
        Command::Stop => cmd_stop(ctx),
    }
}

/// `status`: return what `status.json` holds, live (the running session keeps it
/// fresh). Reading the file — not the engine — is the intended semantics.
fn cmd_status(ctx: &CommandContext) -> String {
    match std::fs::read_to_string(ctx.layout.status()) {
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(value) => proto::ok_reply(&json!({ "status": value })),
            Err(e) => proto::err_reply(&format!("status file is not valid JSON: {e}")),
        },
        Err(e) => proto::err_reply(&format!("could not read status.json: {e}")),
    }
}

/// `conflicts_list`: the same JSON `tomo conflicts list --json` produces.
fn cmd_conflicts_list(ctx: &CommandContext, all: bool) -> String {
    match crate::conflicts_cmd::list_value(&ctx.layout, all) {
        Ok(conflicts) => proto::ok_reply(&json!({ "conflicts": conflicts })),
        Err(e) => proto::err_reply(&e.to_string()),
    }
}

/// `conflict_show`: the same detail `tomo conflicts show <id> --json` produces
/// (framing + inline diff), flattened into the reply. Read-only.
fn cmd_conflict_show(ctx: &CommandContext, id: i64) -> String {
    match crate::conflicts_cmd::show_value(&ctx.layout, id) {
        Ok(detail) => proto::ok_reply(&detail),
        Err(e) => proto::err_reply(&e.to_string()),
    }
}

/// `conflicts_resolve`: reuse the exact CLI resolution cores (`keep`/`take`/
/// `both` — identical semantics to a second-terminal `tomo conflicts resolve`).
fn cmd_conflicts_resolve(ctx: &CommandContext, id: i64, action: ResolveAction) -> String {
    match action {
        ResolveAction::Both => match crate::conflicts_cmd::both_ctl(&ctx.layout, id) {
            Ok(report) => proto::ok_reply(&json!({
                "path": report.path,
                "detail": report.detail,
            })),
            Err(e) => proto::err_reply(&e.to_string()),
        },
        ResolveAction::Keep => match crate::conflicts_cmd::ack_conflict_ctl(&ctx.layout, id) {
            Ok(report) => proto::ok_reply(&json!({
                "resolved": id,
                "action": "keep",
                "path": report.path,
                "newly_resolved": report.newly,
            })),
            Err(e) => proto::err_reply(&e.to_string()),
        },
        ResolveAction::Take => match crate::conflicts_cmd::take_loser_ctl(&ctx.layout, id) {
            Ok(report) => proto::ok_reply(&json!({
                "resolved": id,
                "action": "take",
                "path": report.path,
                "detail": report.detail,
            })),
            Err(e) => proto::err_reply(&e.to_string()),
        },
    }
}

/// `stop`: clean shutdown, the same path as SIGTERM. Set the flag and wake the
/// pump loop so it exits promptly (flushing history/index/status on the way).
fn cmd_stop(ctx: &CommandContext) -> String {
    ctx.shutdown.store(true, Ordering::SeqCst);
    // Wake the pump promptly. On a poisoned lock we skip the nudge; the flag is
    // set regardless, so the loop still stops within its idle timeout.
    if let Ok(tx) = ctx.tx.lock() {
        let _ = tx.send(Incoming::Shutdown);
    }
    proto::ok_reply(&json!({ "stopping": true }))
}
