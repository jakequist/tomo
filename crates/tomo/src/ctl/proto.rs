//! The control-channel wire protocol (docs/SPEC.md §13 "Control channel"):
//! newline-delimited JSON over the `.tomo/state/ctl.sock` unix socket.
//!
//! The schema is **versioned and additive-only** from the moment it ships
//! (UX-V2 §5): every record carries `"v":1`, no field is ever removed or
//! repurposed, and unknown fields are ignored on parse (a newer client/server
//! may add fields a peer does not understand). These types are the single
//! source of truth for that schema; the snapshot test below fails if an event's
//! field names change by accident.
//!
//! Two message directions:
//! - **client → server, first line**: a [`ClientHello`] selecting the mode
//!   (`events` streams records; `command` runs one command and gets one reply).
//! - **server → client**: [`Event`] records (events mode, via [`to_line`]) or a
//!   single command reply object (command mode, via [`ok_reply`]/[`err_reply`]).

use serde::{Deserialize, Serialize};
use serde_json::json;

/// The protocol version carried by every record's `"v"` field.
pub const PROTOCOL_V: u32 = 1;

/// The first line a client sends, selecting the connection mode.
///
/// Additive-only: unknown fields are ignored (no `deny_unknown_fields`), so a
/// future client may add keys without breaking an older server.
#[derive(Debug, Clone, Deserialize)]
pub struct ClientHello {
    /// Protocol version; must equal [`PROTOCOL_V`].
    pub v: u32,
    /// Which channel the client wants.
    pub mode: ClientMode,
    /// The command to run, required for [`ClientMode::Command`], ignored for
    /// [`ClientMode::Events`].
    #[serde(default)]
    pub cmd: Option<Command>,
}

/// The connection mode a [`ClientHello`] selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientMode {
    /// Stream event records until the client disconnects.
    Events,
    /// Execute one command, receive one reply, close.
    Command,
}

/// A command sent over the command channel (v1 surface: UX-V2 §2/§5).
///
/// Internally tagged by `"type"`; every handler reuses the *same* code the
/// equivalent CLI one-shot command runs, so the socket grants no powers the CLI
/// lacks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Liveness check; the reply carries `"pong":true`.
    Ping,
    /// The live status (the contents of `status.json`).
    Status,
    /// The recorded conflicts (unresolved only unless `all`).
    ConflictsList {
        /// Include already-acknowledged conflicts.
        #[serde(default)]
        all: bool,
    },
    /// One conflict's winner/loser framing and inline diff, by id — the same
    /// data `tomo conflicts show <id> --json` produces (read-only). The TUI's
    /// conflict center fetches its diff pane through this (UX-V2 §3b).
    ConflictShow {
        /// The conflict id (from `conflicts_list` / `tomo conflicts list`).
        id: i64,
    },
    /// Resolve one conflict by id, exactly as `tomo conflicts resolve` would.
    ConflictsResolve {
        /// The conflict id (from `conflicts_list` / `tomo conflicts list`).
        id: i64,
        /// How to resolve it.
        action: ResolveAction,
    },
    /// Clean shutdown of the running session (same path as SIGTERM).
    Stop,
}

/// How a `conflicts_resolve` command resolves a conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveAction {
    /// Keep the current file, acknowledge the conflict (tree untouched).
    Keep,
    /// Adopt the preserved losing version into the tree (crash-safe apply).
    Take,
    /// Materialize the loser alongside the winner (keep-both). Not yet wired in
    /// the control channel; replies `{"error":"unsupported"}` until the CLI's
    /// `--both` lands and the lead connects them.
    Both,
}

/// Which side authored the winning version of a resolved conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictSide {
    /// This replica's own version won.
    Local,
    /// The peer's version won.
    Peer,
}

/// One event record streamed to an events-mode subscriber.
///
/// Internally tagged by `"event"`; [`to_line`] renders it with the leading
/// `"v":1`. Every structured line the live session prints has an event here,
/// plus session-state changes (`connected`/`disconnected`), a periodic
/// `heartbeat` for the TUI status line, and the best-effort `lagged` sentinel a
/// slow subscriber receives before being dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The peer completed its handshake.
    Connected {
        /// The peer's name (hostname), when known.
        peer_name: Option<String>,
        /// The peer's address, when known.
        peer_addr: Option<String>,
    },
    /// The peer session dropped (a disconnect or clean shutdown).
    Disconnected,
    /// A file was applied from the peer into the tree (incoming).
    Synced {
        /// The repo-relative path.
        path: String,
        /// The applied content size in bytes.
        size: u64,
    },
    /// A local change was shipped to the peer (outbound).
    Sent {
        /// The repo-relative path.
        path: String,
        /// The shipped content size in bytes.
        size: u64,
    },
    /// A file was removed as a result of a peer deletion.
    Removed {
        /// The repo-relative path.
        path: String,
    },
    /// A concurrent edit was resolved (surfaced non-blockingly, invariant #5).
    Conflict {
        /// The conflict record id in the history DB (matches
        /// `tomo conflicts list`), or `null` if it could not be recorded.
        id: Option<i64>,
        /// The repo-relative path.
        path: String,
        /// Which side's version won.
        winner: ConflictSide,
        /// Whether this was a genesis adoption (first-sync newer-copy adoption)
        /// rather than a mid-session clash.
        adopted: bool,
    },
    /// In-flight transfer progress for a large file.
    Transfer {
        /// The repo-relative path.
        path: String,
        /// Bytes transferred so far.
        done: u64,
        /// Total content size in bytes.
        total: u64,
    },
    /// A one-off informational note not tied to a path.
    Note {
        /// The message text.
        message: String,
    },
    /// A non-fatal error worth surfacing.
    Error {
        /// The message text.
        message: String,
    },
    /// A periodic liveness/status beat for the TUI status line.
    Heartbeat {
        /// Milliseconds since the last file sync (apply/send/remove), or `null`
        /// if nothing has synced yet this session.
        last_sync_ms_ago: Option<u64>,
        /// Count of unresolved conflicts in the history DB.
        unresolved_conflicts: u64,
    },
    /// The final best-effort line a subscriber receives when it fell behind and
    /// was disconnected to protect sync latency (bounded per-subscriber queue).
    Lagged,
}

/// Render one [`Event`] as a single newline-free JSON line carrying `"v":1`.
///
/// Serializing a plain data enum never fails; the fallback line only guards the
/// theoretically-impossible error rather than panicking (hygiene: no `unwrap`).
#[must_use]
pub fn to_line(event: &Event) -> String {
    match serde_json::to_value(event) {
        Ok(mut value) => {
            if let Some(map) = value.as_object_mut() {
                map.insert("v".to_owned(), json!(PROTOCOL_V));
            }
            value.to_string()
        }
        Err(_) => json!({"v": PROTOCOL_V, "event": "error", "message": "event serialize failed"})
            .to_string(),
    }
}

/// Build a successful command reply line, merging `"v":1` and `"ok":true` with
/// the given payload object (a non-object payload is treated as no extra
/// fields).
#[must_use]
pub fn ok_reply(fields: &serde_json::Value) -> String {
    let mut map = serde_json::Map::new();
    map.insert("v".to_owned(), json!(PROTOCOL_V));
    map.insert("ok".to_owned(), json!(true));
    if let Some(obj) = fields.as_object() {
        for (k, v) in obj {
            map.insert(k.clone(), v.clone());
        }
    }
    serde_json::Value::Object(map).to_string()
}

/// Build a failed command reply line: `{"v":1,"ok":false,"error":<msg>}`.
#[must_use]
pub fn err_reply(msg: &str) -> String {
    json!({"v": PROTOCOL_V, "ok": false, "error": msg}).to_string()
}

/// The client's first line to select the events channel.
#[must_use]
pub fn to_hello_events() -> String {
    json!({"v": PROTOCOL_V, "mode": "events"}).to_string()
}

/// The client's first line to run one command: `cmd` is the command object
/// (e.g. `{"type":"ping"}`), wrapped in the command-mode envelope.
#[must_use]
pub fn to_hello_command(cmd: &serde_json::Value) -> String {
    json!({"v": PROTOCOL_V, "mode": "command", "cmd": cmd}).to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_events_mode_hello() {
        let hello: ClientHello = serde_json::from_str(r#"{"v":1,"mode":"events"}"#).unwrap();
        assert_eq!(hello.v, 1);
        assert_eq!(hello.mode, ClientMode::Events);
        assert!(hello.cmd.is_none());
    }

    #[test]
    fn parses_command_mode_hello_with_cmd() {
        let hello: ClientHello =
            serde_json::from_str(r#"{"v":1,"mode":"command","cmd":{"type":"ping"}}"#).unwrap();
        assert_eq!(hello.mode, ClientMode::Command);
        assert_eq!(hello.cmd, Some(Command::Ping));
    }

    #[test]
    fn parses_each_command_shape() {
        let status: Command = serde_json::from_str(r#"{"type":"status"}"#).unwrap();
        assert_eq!(status, Command::Status);

        let list: Command =
            serde_json::from_str(r#"{"type":"conflicts_list","all":true}"#).unwrap();
        assert_eq!(list, Command::ConflictsList { all: true });

        // `all` defaults to false when omitted.
        let list0: Command = serde_json::from_str(r#"{"type":"conflicts_list"}"#).unwrap();
        assert_eq!(list0, Command::ConflictsList { all: false });

        let show: Command = serde_json::from_str(r#"{"type":"conflict_show","id":9}"#).unwrap();
        assert_eq!(show, Command::ConflictShow { id: 9 });

        let resolve: Command =
            serde_json::from_str(r#"{"type":"conflicts_resolve","id":7,"action":"take"}"#).unwrap();
        assert_eq!(
            resolve,
            Command::ConflictsResolve {
                id: 7,
                action: ResolveAction::Take
            }
        );

        let stop: Command = serde_json::from_str(r#"{"type":"stop"}"#).unwrap();
        assert_eq!(stop, Command::Stop);
    }

    #[test]
    fn resolve_action_parses_keep_take_both() {
        for (raw, want) in [
            ("keep", ResolveAction::Keep),
            ("take", ResolveAction::Take),
            ("both", ResolveAction::Both),
        ] {
            let cmd: Command = serde_json::from_str(&format!(
                r#"{{"type":"conflicts_resolve","id":1,"action":"{raw}"}}"#
            ))
            .unwrap();
            assert_eq!(
                cmd,
                Command::ConflictsResolve {
                    id: 1,
                    action: want
                }
            );
        }
    }

    #[test]
    fn unknown_fields_are_ignored_additive_only() {
        // A future client adds fields an older server does not know; they must
        // be ignored, not rejected (the additive-only contract).
        let hello: ClientHello =
            serde_json::from_str(r#"{"v":1,"mode":"events","future_flag":true,"nested":{"x":1}}"#)
                .unwrap();
        assert_eq!(hello.mode, ClientMode::Events);

        let cmd: Command = serde_json::from_str(
            r#"{"type":"conflicts_resolve","id":3,"action":"keep","future":"ok"}"#,
        )
        .unwrap();
        assert_eq!(
            cmd,
            Command::ConflictsResolve {
                id: 3,
                action: ResolveAction::Keep
            }
        );
    }

    /// Every record carries `"v":1` and its `"event"` tag.
    #[test]
    fn to_line_carries_version_and_event_tag() {
        let line = to_line(&Event::Synced {
            path: "a.txt".to_owned(),
            size: 12,
        });
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["v"], json!(1));
        assert_eq!(value["event"], json!("synced"));
        assert_eq!(value["path"], json!("a.txt"));
        assert_eq!(value["size"], json!(12));
    }

    /// Schema snapshot: serialize one of every event and assert the exact field
    /// names, so an accidental rename fails this test (the schema is API).
    #[test]
    fn event_schema_field_names_are_stable() {
        fn fields(event: &Event) -> Vec<String> {
            let value: serde_json::Value = serde_json::from_str(&to_line(event)).unwrap();
            let mut keys: Vec<String> = value
                .as_object()
                .unwrap()
                .keys()
                .map(String::clone)
                .collect();
            keys.sort();
            keys
        }

        let cases: Vec<(Event, Vec<&str>)> = vec![
            (
                Event::Connected {
                    peer_name: Some("box".to_owned()),
                    peer_addr: Some("::1".to_owned()),
                },
                vec!["event", "peer_addr", "peer_name", "v"],
            ),
            (Event::Disconnected, vec!["event", "v"]),
            (
                Event::Synced {
                    path: "p".to_owned(),
                    size: 1,
                },
                vec!["event", "path", "size", "v"],
            ),
            (
                Event::Sent {
                    path: "p".to_owned(),
                    size: 1,
                },
                vec!["event", "path", "size", "v"],
            ),
            (
                Event::Removed {
                    path: "p".to_owned(),
                },
                vec!["event", "path", "v"],
            ),
            (
                Event::Conflict {
                    id: Some(4),
                    path: "p".to_owned(),
                    winner: ConflictSide::Local,
                    adopted: false,
                },
                vec!["adopted", "event", "id", "path", "v", "winner"],
            ),
            (
                Event::Transfer {
                    path: "p".to_owned(),
                    done: 1,
                    total: 2,
                },
                vec!["done", "event", "path", "total", "v"],
            ),
            (
                Event::Note {
                    message: "m".to_owned(),
                },
                vec!["event", "message", "v"],
            ),
            (
                Event::Error {
                    message: "m".to_owned(),
                },
                vec!["event", "message", "v"],
            ),
            (
                Event::Heartbeat {
                    last_sync_ms_ago: Some(10),
                    unresolved_conflicts: 2,
                },
                vec!["event", "last_sync_ms_ago", "unresolved_conflicts", "v"],
            ),
            (Event::Lagged, vec!["event", "v"]),
        ];

        for (event, want) in cases {
            let got = fields(&event);
            let want: Vec<String> = want.into_iter().map(str::to_owned).collect();
            assert_eq!(got, want, "field names changed for {event:?}");
        }
    }

    #[test]
    fn conflict_winner_renders_side() {
        let peer = to_line(&Event::Conflict {
            id: None,
            path: "p".to_owned(),
            winner: ConflictSide::Peer,
            adopted: true,
        });
        let value: serde_json::Value = serde_json::from_str(&peer).unwrap();
        assert_eq!(value["winner"], json!("peer"));
        assert_eq!(value["adopted"], json!(true));
        assert_eq!(value["id"], json!(null));
    }

    #[test]
    fn ok_and_err_replies_carry_version() {
        let ok = ok_reply(&json!({"pong": true}));
        let v: serde_json::Value = serde_json::from_str(&ok).unwrap();
        assert_eq!(v["v"], json!(1));
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["pong"], json!(true));

        let err = err_reply("unsupported");
        let v: serde_json::Value = serde_json::from_str(&err).unwrap();
        assert_eq!(v["v"], json!(1));
        assert_eq!(v["ok"], json!(false));
        assert_eq!(v["error"], json!("unsupported"));
    }
}
