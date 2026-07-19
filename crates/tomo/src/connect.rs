//! `tomo connect`: record a sync peer in `.tomo/config.toml` and validate it.
//!
//! Writes the `[remote]` section, then does a live validation pass over SSH
//! (M2): connect, bootstrap the remote binary (pushing if needed), spawn
//! `serve --stdio`, exchange the `Hello` handshake, and shut down cleanly —
//! printing a summary (triple, pushed/reused, remote version). If validation
//! fails the config is still written (so it can be corrected/retried) but the
//! command exits non-zero with the error.

use std::fmt::Write as _;
use std::sync::mpsc;
use std::time::Duration;

use tomo_config::{Config, Remote};
use tomo_proto::{Message, PROTOCOL_VERSION};

use crate::error::CliError;
use crate::layout::Layout;
use crate::replica;
use crate::session::Incoming;
use crate::transport::{self, SshParams};

/// How long to wait for the remote peer's opening `Hello` before giving up.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// What [`run`] should do about the config, decided purely from the existing
/// `[remote]` (if any), the requested target, and `--force`. Kept separate from
/// I/O so the idempotence rules are unit-tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectAction {
    /// No matching `[remote]` yet, or `--force` over a different one: (re)write
    /// the `[remote]` section, then validate.
    WriteAndValidate,
    /// An identical `[remote]` is already recorded: skip the write and just
    /// revalidate the live connection (idempotent health check).
    RevalidateExisting,
}

/// Decide what `tomo connect` does given any already-recorded `[remote]`.
///
/// - No existing remote → write it and validate.
/// - Existing remote with the **same** host+path → revalidate, no rewrite
///   (idempotent).
/// - Existing remote with a **different** target → refuse, unless `force`, which
///   overwrites and validates.
///
/// # Errors
/// [`CliError::Message`] for a different target without `--force`.
fn decide_connect(
    existing: Option<&Remote>,
    target: &str,
    remote_path: &str,
    force: bool,
) -> Result<ConnectAction, CliError> {
    match existing {
        None => Ok(ConnectAction::WriteAndValidate),
        Some(remote) if remote.host == target && remote.path == remote_path => {
            Ok(ConnectAction::RevalidateExisting)
        }
        Some(remote) if force => {
            let _ = remote; // overwriting; the old target is discarded.
            Ok(ConnectAction::WriteAndValidate)
        }
        Some(remote) => Err(CliError::msg(format!(
            "a different [remote] is already configured ({}:{}); re-run with --force to \
             overwrite it, or use the same target to revalidate",
            remote.host, remote.path
        ))),
    }
}

/// Run `tomo connect <target> <remote_path> [--force] [--identity <path>]`.
///
/// `identity` records an explicit SSH private-key path in the `[remote]` so
/// every later `tomo watch` reuses it; it is tried before ssh-agent,
/// `~/.ssh/config`, and the default keys. Only applied when the `[remote]` is
/// (re)written — a pure revalidation of an unchanged target keeps its recorded
/// identity.
///
/// # Errors
/// [`CliError`] if the project is not initialized, a different remote is already
/// configured without `--force`, the config cannot be read/written, or the live
/// SSH validation fails.
pub fn run(
    layout: &Layout,
    target: &str,
    remote_path: Option<&str>,
    force: bool,
    identity: Option<&str>,
) -> Result<(), CliError> {
    // Accept both the two-argument and the single-argument `host:/path` forms
    // (and the local-`~` guard) exactly as `tomo sync` does.
    let (host, path) = crate::target::resolve(target, remote_path)?;
    let (remote, action) = apply_remote_config(layout, &host, &path, force, identity)?;

    match action {
        ConnectAction::WriteAndValidate => {
            step(&format!(
                "recorded remote {host}:{path} in {}",
                layout.config().display()
            ));
        }
        ConnectAction::RevalidateExisting => {
            step(&format!(
                "remote {host}:{path} already recorded — revalidating"
            ));
        }
    }

    let params = SshParams::from_remote(&remote)?;
    let replica = replica::load(&layout.replica())?;

    println!("validating SSH connection and bootstrapping remote binary…");
    validate(&params, replica)?;
    Ok(())
}

/// Print one checklist step: `✓ <msg>` when styling is enabled, or `<msg>`
/// unchanged when disabled (byte-identical to the historical plain line).
fn step(msg: &str) {
    let style = crate::style::current();
    if style.enabled() {
        println!("{} {msg}", style.ok(style.g_ok()));
    } else {
        println!("{msg}");
    }
}

/// Record (or confirm) a `[remote]` in `.tomo/config.toml` and return the
/// effective [`Remote`] plus the [`ConnectAction`] taken — **without** any live
/// validation or printing. Shared by `tomo connect` (which then validates) and
/// `tomo sync <target> <path>` (which goes straight into the live session, whose
/// bootstrap/handshake *is* the validation).
///
/// - No existing remote → write it (`WriteAndValidate`).
/// - Identical target already recorded → no rewrite (`RevalidateExisting`),
///   preserving whatever identity the recorded `[remote]` carries.
/// - Different target → refuse unless `force`, which overwrites.
///
/// # Errors
/// [`CliError`] if the project is not initialized, a different remote is already
/// configured without `--force`, or the config cannot be read/written.
pub(crate) fn apply_remote_config(
    layout: &Layout,
    target: &str,
    remote_path: &str,
    force: bool,
    identity: Option<&str>,
) -> Result<(Remote, ConnectAction), CliError> {
    if !layout.is_initialized() {
        return Err(CliError::msg(
            "not a Tomo project (no .tomo/ here) — run `tomo init` first",
        ));
    }

    let path = layout.config();
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => return Err(CliError::io("read config", &path, source)),
    };

    // Parse the existing document to compare against the recorded remote (if
    // any) rather than string-scanning for a `[remote]` header.
    let existing_remote = Config::from_toml_str(&existing)?.remote;
    let action = decide_connect(existing_remote.as_ref(), target, remote_path, force)?;

    if action == ConnectAction::WriteAndValidate {
        // Drop any existing [remote] (a --force overwrite) before appending the
        // new one, so we never end up with two [remote] tables.
        let mut updated = strip_remote_section(&existing);
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        // Writing to a String is infallible.
        let _ = write!(
            updated,
            "\n[remote]\nhost = \"{target}\"\npath = \"{remote_path}\"\n"
        );
        // An explicit --identity is recorded so a later `tomo sync` reuses it.
        // The path is TOML-escaped (backslash and quote) so a Windows-style or
        // unusual path can never produce a config we then fail to load back.
        if let Some(id) = identity {
            let _ = writeln!(updated, "identity = \"{}\"", toml_escape(id));
        }

        // Parse the result so we never write a config we cannot load back.
        Config::from_toml_str(&updated)?;
        std::fs::write(&path, &updated).map_err(|s| CliError::io("write config", &path, s))?;
    }

    // On a fresh/overwritten write use the passed --identity; on a pure
    // revalidation keep whatever the recorded [remote] already carries.
    let effective_identity = if action == ConnectAction::WriteAndValidate {
        identity.map(str::to_owned)
    } else {
        existing_remote.as_ref().and_then(|r| r.identity.clone())
    };
    let remote = Remote {
        host: target.to_owned(),
        path: remote_path.to_owned(),
        identity: effective_identity,
    };
    Ok((remote, action))
}

/// Remove a top-level `[remote]` table from a TOML document.
///
/// Drops the `[remote]` header line and every following line up to (but not
/// including) the next table header (`[…]`) or end of file. Used by a `--force`
/// overwrite so the freshly appended `[remote]` is the only one. Other sections
/// and comments are preserved.
/// Escape a string for a TOML basic (double-quoted) value: backslash and the
/// double quote itself. Sufficient for filesystem paths, which is the only thing
/// we write this way.
fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn strip_remote_section(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_remote = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "[remote]" {
            in_remote = true;
            continue;
        }
        if in_remote {
            // A new table header ends the [remote] section.
            if trimmed.starts_with('[') {
                in_remote = false;
            } else {
                continue; // still inside [remote]; drop the line.
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Connect, bootstrap, spawn the remote peer, exchange `Hello`, and tear down —
/// printing a summary of what happened.
fn validate(params: &SshParams, replica: tomo_engine::ReplicaId) -> Result<(), CliError> {
    let (tx, rx) = mpsc::channel::<Incoming>();
    let (mut t, report) = transport::ssh(params, &tx, false)?;

    match &report {
        tomo_transport::BootstrapReport::Reused {
            triple, version, ..
        } => {
            step(&format!(
                "  bootstrap: reused existing binary (tomo {version}, {triple})"
            ));
        }
        tomo_transport::BootstrapReport::Pushed {
            triple,
            version,
            bytes,
            embedded,
            dev_substitution,
            ..
        } => {
            let origin = if *embedded {
                " [embedded static artifact]"
            } else {
                ""
            };
            step(&format!(
                "  bootstrap: pushed tomo {version} for {triple} ({bytes} bytes){origin}"
            ));
            if *dev_substitution {
                println!(
                    "  WARNING: dev-mode substitution — pushed this build's own non-musl \
                     binary to a musl remote (debug-only; release builds embed static musl \
                     binaries at M6)"
                );
            }
        }
    }

    // Send our Hello, then wait for the peer's.
    t.tx.send(&Message::Hello {
        protocol: PROTOCOL_VERSION,
        binary_version: params.version.clone(),
        replica,
    })?;

    let peer_version = wait_for_hello(&rx, &t)?;
    step(&format!(
        "  handshake: remote reports tomo {peer_version} (protocol v{PROTOCOL_VERSION})"
    ));

    // Clean shutdown: retire the reader and drop the transport (tears down SSH).
    t.deactivate();
    drop(t);
    step("connection OK — remote is reachable and ready to sync");
    Ok(())
}

/// Block until the peer's `Hello` arrives (or a terminal condition / timeout).
/// Returns the peer's reported binary version.
fn wait_for_hello(
    rx: &mpsc::Receiver<Incoming>,
    transport: &transport::Transport,
) -> Result<String, CliError> {
    loop {
        match rx.recv_timeout(HANDSHAKE_TIMEOUT) {
            Ok(Incoming::Message(Message::Hello {
                protocol,
                binary_version,
                ..
            })) => {
                if protocol != PROTOCOL_VERSION {
                    return Err(CliError::msg(format!(
                        "protocol mismatch: remote speaks v{protocol}, we speak v{PROTOCOL_VERSION}"
                    )));
                }
                return Ok(binary_version);
            }
            // Any other pre-Hello frame (e.g. an eager IndexExchange) or a local
            // watch event is ignored for validation; keep waiting for the Hello.
            Ok(Incoming::Message(_) | Incoming::Watch(_)) => {}
            Ok(Incoming::PeerEof) => {
                return Err(remote_died("the remote closed the connection", transport));
            }
            Ok(Incoming::ProtoError(e)) => {
                return Err(remote_died(&format!("transport error: {e}"), transport));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(remote_died(
                    "timed out waiting for the remote's handshake",
                    transport,
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(remote_died("the transport reader stopped", transport));
            }
        }
    }
}

/// Build an error for a failed handshake, appending any captured remote stderr.
fn remote_died(reason: &str, transport: &transport::Transport) -> CliError {
    match transport.stderr_tail() {
        Some(tail) => CliError::msg(format!(
            "remote bootstrap failed while validating: {reason}\nremote stderr:\n{tail}"
        )),
        None => CliError::msg(format!(
            "remote bootstrap failed while validating: {reason}"
        )),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn remote(host: &str, path: &str) -> Remote {
        Remote {
            host: host.to_owned(),
            path: path.to_owned(),
            identity: None,
        }
    }

    #[test]
    fn no_existing_remote_writes_and_validates() {
        assert_eq!(
            decide_connect(None, "u@h", "/p", false).unwrap(),
            ConnectAction::WriteAndValidate
        );
    }

    #[test]
    fn identical_target_revalidates_without_force() {
        let existing = remote("u@h", "/p");
        assert_eq!(
            decide_connect(Some(&existing), "u@h", "/p", false).unwrap(),
            ConnectAction::RevalidateExisting
        );
        // --force on an identical target is harmless (still just revalidates the
        // same target — the target did not change, so nothing is overwritten).
        assert_eq!(
            decide_connect(Some(&existing), "u@h", "/p", true).unwrap(),
            ConnectAction::RevalidateExisting
        );
    }

    #[test]
    fn different_target_is_refused_without_force() {
        let existing = remote("u@h", "/p");
        // Different host.
        assert!(decide_connect(Some(&existing), "u@other", "/p", false).is_err());
        // Same host, different path.
        assert!(decide_connect(Some(&existing), "u@h", "/other", false).is_err());
    }

    #[test]
    fn different_target_with_force_overwrites() {
        let existing = remote("u@h", "/p");
        assert_eq!(
            decide_connect(Some(&existing), "u@other", "/q", true).unwrap(),
            ConnectAction::WriteAndValidate
        );
    }

    #[test]
    fn strip_remote_removes_only_that_section() {
        let doc = "\
[history]
mode = \"adaptive\"

[remote]
host = \"u@h\"
path = \"/p\"

[[rules]]
pattern = \"target/\"
class = \"ignored\"
";
        let stripped = strip_remote_section(doc);
        assert!(!stripped.contains("[remote]"));
        assert!(!stripped.contains("u@h"));
        // Surrounding sections survive.
        assert!(stripped.contains("[history]"));
        assert!(stripped.contains("mode = \"adaptive\""));
        assert!(stripped.contains("[[rules]]"));
        assert!(stripped.contains("pattern = \"target/\""));
        // The result must still parse, with the remote gone.
        assert!(Config::from_toml_str(&stripped).unwrap().remote.is_none());
    }

    #[test]
    fn strip_remote_at_end_of_file() {
        let doc = "[history]\nmode = \"off\"\n\n[remote]\nhost = \"h\"\npath = \"/x\"\n";
        let stripped = strip_remote_section(doc);
        assert!(!stripped.contains("[remote]"));
        assert!(stripped.contains("mode = \"off\""));
        assert!(Config::from_toml_str(&stripped).unwrap().remote.is_none());
    }

    #[test]
    fn strip_remote_no_op_when_absent() {
        let doc = "[history]\nmode = \"adaptive\"\n";
        let stripped = strip_remote_section(doc);
        assert!(stripped.contains("[history]"));
        assert!(Config::from_toml_str(&stripped).unwrap().remote.is_none());
    }

    #[test]
    fn force_overwrite_round_trip_leaves_single_remote() {
        // Simulate the force path: strip then append, and confirm exactly one
        // [remote] with the new target parses back.
        let original = "[remote]\nhost = \"old@h\"\npath = \"/old\"\n";
        let mut updated = strip_remote_section(original);
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str("\n[remote]\nhost = \"new@h\"\npath = \"/new\"\n");
        assert_eq!(updated.matches("[remote]").count(), 1);
        let parsed = Config::from_toml_str(&updated).unwrap().remote.unwrap();
        assert_eq!(parsed.host, "new@h");
        assert_eq!(parsed.path, "/new");
    }
}
