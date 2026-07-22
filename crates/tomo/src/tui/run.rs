//! The TUI's I/O shell (UX-V2 §3): the thin, logic-free layer that wires the
//! pure core ([`super::state`]) to the outside world. It:
//!
//! - opens the control socket in events mode on a reader thread, parsing each
//!   record into a [`Msg::Event`];
//! - reads terminal keys (crossterm) and emits ticks;
//! - runs the reducer on each message and renders the resulting [`Model`];
//! - drains [`Model::outbox`], dispatching each command on its own short-lived
//!   connection (one command per connection, like `tomo dev ctl`) and feeding
//!   the reply back as a [`Msg::Cmd`].
//!
//! No interaction logic lives here — every decision is in the reducer. The shell
//! only moves bytes and stamps display-only wall time.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use serde_json::Value;

use crate::ctl::proto::{self, Event};
use crate::error::CliError;
use crate::layout::Layout;

use super::state::{
    parse_conflicts, parse_detail, CmdOutcome, CmdReply, CtlRequest, Key, Model, Msg, OutCommand,
    TICK_MS,
};
use super::view::{self, Theme};

/// Run the TUI against the local project's control socket until the user quits.
///
/// # Errors
/// [`CliError::Message`] if the project is not initialized or no session is
/// running (the socket cannot be connected); [`CliError::Io`] on a terminal
/// setup failure.
pub fn run(layout: &Layout) -> Result<(), CliError> {
    if !layout.is_initialized() {
        return Err(CliError::msg(
            "not a Tomo project (no .tomo/ here) — run `tomo init` first",
        ));
    }
    let sock = layout.ctl_sock();
    // Open the event stream first so a missing session is a clean error before
    // we ever touch the terminal.
    let events_stream = UnixStream::connect(&sock)
        .map_err(|_| CliError::msg("no running session — start one with `tomo sync`"))?;

    let (tx, rx) = mpsc::channel::<Msg>();
    spawn_event_reader(events_stream, tx.clone());
    spawn_input(tx.clone());
    spawn_ticker(tx.clone());

    let theme = Theme::from_style();
    let mut terminal = setup_terminal()?;
    let _guard = TerminalGuard;

    let result = event_loop(&mut terminal, &rx, &tx, &sock, theme);

    // Restore the terminal regardless of how the loop ended (the guard also
    // restores on an early return / panic-unwind path).
    restore_terminal(&mut terminal);
    result
}

/// The render + reduce loop. Blocks on the message channel; each message stamps
/// the model's display clock, runs the reducer, dispatches any queued commands,
/// and redraws.
fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    rx: &Receiver<Msg>,
    tx: &Sender<Msg>,
    sock: &std::path::Path,
    theme: Theme,
) -> Result<(), CliError> {
    let mut model = Model::default();
    draw(terminal, &model, theme)?;
    while let Ok(msg) = rx.recv() {
        model.now_ms = wall_now_ms();
        model = super::state::update(model, msg);
        // Dispatch any commands the reducer queued (each on its own connection).
        for cmd in std::mem::take(&mut model.outbox) {
            dispatch(sock.to_path_buf(), cmd, tx.clone());
        }
        if model.quit {
            break;
        }
        draw(terminal, &model, theme)?;
    }
    Ok(())
}

fn draw(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    model: &Model,
    theme: Theme,
) -> Result<(), CliError> {
    terminal
        .draw(|f| view::render(f, model, theme))
        .map_err(|s| CliError::io("render TUI frame", std::path::Path::new("<terminal>"), s))?;
    Ok(())
}

/// Current wall time in ms since the epoch (display-only, invariant #7).
fn wall_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

// ---- socket: event stream -------------------------------------------------

fn spawn_event_reader(stream: UnixStream, tx: Sender<Msg>) {
    thread::spawn(move || {
        let Ok(mut writer) = stream.try_clone() else {
            return;
        };
        if writeln!(writer, "{}", proto::to_hello_events())
            .and_then(|()| writer.flush())
            .is_err()
        {
            return;
        }
        let reader = BufReader::new(stream);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if line.is_empty() {
                continue;
            }
            if let Ok(event) = serde_json::from_str::<Event>(&line) {
                if tx.send(Msg::Event(event)).is_err() {
                    break; // UI gone
                }
            }
        }
    });
}

// ---- socket: command channel ----------------------------------------------

/// Dispatch one queued command on a short-lived connection and feed the parsed
/// reply back as a [`Msg::Cmd`]. Runs on its own thread so a slow reply never
/// blocks the render loop.
fn dispatch(sock: std::path::PathBuf, cmd: OutCommand, tx: Sender<Msg>) {
    thread::spawn(move || {
        let result = run_request(&sock, &cmd.req);
        let _ = tx.send(Msg::Cmd(CmdOutcome {
            seq: cmd.seq,
            result,
        }));
    });
}

/// Send one command object and parse its reply into a [`CmdReply`].
fn run_request(sock: &std::path::Path, req: &CtlRequest) -> Result<CmdReply, String> {
    let reply = send_command(sock, &req.to_json())?;
    match req {
        CtlRequest::ConflictsList => {
            let arr = reply.get("conflicts").cloned().unwrap_or(Value::Null);
            Ok(CmdReply::Conflicts(parse_conflicts(&arr)))
        }
        CtlRequest::ConflictShow { id } => {
            let detail = reply
                .get("conflict")
                .and_then(parse_detail)
                .or_else(|| parse_detail(&reply))
                .ok_or_else(|| "malformed conflict_show reply".to_owned())?;
            Ok(CmdReply::Show { id: *id, detail })
        }
        CtlRequest::Resolve { .. } => Ok(CmdReply::Resolved),
    }
}

/// One command over the control channel: connect, send the command-mode hello,
/// read the single reply line, and return its payload object on `ok:true` or an
/// error string on `ok:false` / transport failure.
fn send_command(sock: &std::path::Path, cmd: &Value) -> Result<Value, String> {
    let mut stream = UnixStream::connect(sock).map_err(|_| "session not running".to_owned())?;
    writeln!(stream, "{}", proto::to_hello_command(cmd))
        .and_then(|()| stream.flush())
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    let value: Value = serde_json::from_str(line.trim()).map_err(|e| format!("bad reply: {e}"))?;
    if value.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(value)
    } else {
        Err(value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("command failed")
            .to_owned())
    }
}

// ---- input + ticks --------------------------------------------------------

fn spawn_input(tx: Sender<Msg>) {
    thread::spawn(move || loop {
        // Poll so the thread can exit promptly if the UI channel closes.
        match event::poll(Duration::from_millis(100)) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(_) => return,
        }
        let Ok(ct) = event::read() else { return };
        if let Some(key) = map_event(&ct) {
            if tx.send(Msg::Key(key)).is_err() {
                return;
            }
        }
    });
}

fn spawn_ticker(tx: Sender<Msg>) {
    thread::spawn(move || loop {
        thread::sleep(Duration::from_millis(TICK_MS));
        if tx.send(Msg::Tick).is_err() {
            return;
        }
    });
}

/// Map a crossterm event to a reducer [`Key`], or `None` to ignore it. Ctrl-C is
/// folded to `q` (quit).
fn map_event(ct: &CtEvent) -> Option<Key> {
    let CtEvent::Key(k) = ct else { return None };
    // Only key-press events (Windows also emits Release/Repeat).
    if k.kind == KeyEventKind::Release {
        return None;
    }
    if k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('c')) {
        return Some(Key::Char('q'));
    }
    Some(match k.code {
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        _ => return None,
    })
}

// ---- terminal lifecycle ---------------------------------------------------

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>, CliError> {
    enable_raw_mode()
        .map_err(|s| CliError::io("enable raw mode", std::path::Path::new("<terminal>"), s))?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen).map_err(|s| {
        CliError::io(
            "enter alternate screen",
            std::path::Path::new("<terminal>"),
            s,
        )
    })?;
    Terminal::new(CrosstermBackend::new(stdout)).map_err(|s| {
        CliError::io(
            "init terminal backend",
            std::path::Path::new("<terminal>"),
            s,
        )
    })
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
}

/// A last-resort restore on unwind: if a panic escapes the loop, leave the
/// terminal usable. The normal path calls [`restore_terminal`] explicitly.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventState, KeyModifiers};

    fn press(code: KeyCode) -> CtEvent {
        CtEvent::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    #[test]
    fn maps_common_keys() {
        assert_eq!(map_event(&press(KeyCode::Char('c'))), Some(Key::Char('c')));
        assert_eq!(map_event(&press(KeyCode::Enter)), Some(Key::Enter));
        assert_eq!(map_event(&press(KeyCode::Esc)), Some(Key::Esc));
        assert_eq!(map_event(&press(KeyCode::PageUp)), Some(Key::PageUp));
        assert_eq!(map_event(&press(KeyCode::End)), Some(Key::End));
    }

    #[test]
    fn ctrl_c_maps_to_quit() {
        let ev = CtEvent::Key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
        assert_eq!(map_event(&ev), Some(Key::Char('q')));
    }

    #[test]
    fn ignores_key_release() {
        let ev = CtEvent::Key(KeyEvent {
            code: KeyCode::Char('x'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        });
        assert_eq!(map_event(&ev), None);
    }
}
