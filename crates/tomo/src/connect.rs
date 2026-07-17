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

/// Run `tomo connect <target> <remote_path>`.
///
/// # Errors
/// [`CliError`] if the project is not initialized, the config cannot be
/// read/written, or the live SSH validation fails.
pub fn run(layout: &Layout, target: &str, remote_path: &str) -> Result<(), CliError> {
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

    if existing.lines().any(|line| line.trim() == "[remote]") {
        return Err(CliError::msg(
            "a [remote] is already configured; edit .tomo/config.toml to change it",
        ));
    }

    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    // Writing to a String is infallible.
    let _ = write!(
        updated,
        "\n[remote]\nhost = \"{target}\"\npath = \"{remote_path}\"\n"
    );

    // Parse the result so we never write a config we cannot load back.
    Config::from_toml_str(&updated)?;
    std::fs::write(&path, &updated).map_err(|s| CliError::io("write config", &path, s))?;

    println!(
        "recorded remote {target}:{remote_path} in {}",
        path.display()
    );

    // Live validation pass over SSH.
    let remote = Remote {
        host: target.to_owned(),
        path: remote_path.to_owned(),
    };
    let params = SshParams::from_remote(&remote)?;
    let replica = replica::load(&layout.replica())?;

    println!("validating SSH connection and bootstrapping remote binary…");
    validate(&params, replica)?;
    Ok(())
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
            println!("  bootstrap: reused existing binary (tomo {version}, {triple})");
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
            println!("  bootstrap: pushed tomo {version} for {triple} ({bytes} bytes){origin}");
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
    println!("  handshake: remote reports tomo {peer_version} (protocol v{PROTOCOL_VERSION})");

    // Clean shutdown: retire the reader and drop the transport (tears down SSH).
    t.deactivate();
    drop(t);
    println!("connection OK — remote is reachable and ready to sync");
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
