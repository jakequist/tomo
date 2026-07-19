//! `tomo sync`: the primary foreground sync command.
//!
//! Unifies what used to be two steps (`tomo connect` then `tomo watch`):
//! - `tomo sync <ssh-target> <remote-path>` records the peer if it is new
//!   (reusing `connect`'s write plumbing) and goes **straight** into the live
//!   session — the session's own bootstrap/handshake is the validation, so there
//!   is no separate validation pass.
//! - `tomo sync` with no target syncs against the recorded `[remote]`, or a
//!   `--local-peer <path>` directory, or runs watch-only if neither is set.

use std::path::PathBuf;

use tomo_config::Config;

use crate::connect::{self, ConnectAction};
use crate::error::CliError;
use crate::layout::Layout;
use crate::report::Reporter;
use crate::session::{self, Mode};
use crate::transport::SshParams;
use crate::{current_dir, replica};

/// Run `tomo sync`.
///
/// Argument shapes:
/// - `target` + `remote_path` (both, or neither): record/confirm the SSH peer
///   and sync over SSH.
/// - `--local-peer <path>` (no target): sync with a local served peer.
/// - none of the above: sync against a configured `[remote]`, else watch-only.
///
/// # Errors
/// [`CliError`] if the project is not initialized, exactly one of the target
/// args is given, `--local-peer` is combined with a target, the config/replica
/// cannot be loaded, the peer path is invalid, or the sync loop fails.
pub fn run(
    target: Option<String>,
    remote_path: Option<String>,
    local_peer: Option<PathBuf>,
    force: bool,
    json: bool,
) -> Result<(), CliError> {
    let root = current_dir()?;
    let layout = Layout::new(&root);
    if !layout.is_initialized() {
        return Err(CliError::msg(
            "not a Tomo project (no .tomo/ here) — run `tomo init` first",
        ));
    }

    let replica = replica::load(&layout.replica())?;
    let config = Config::load(layout.root())?;
    let reporter = Reporter::human(json, crate::style::current());

    let mode = resolve_mode(
        &layout,
        &config,
        target,
        remote_path,
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
    remote_path: Option<String>,
    local_peer: Option<PathBuf>,
    force: bool,
    reporter: &Reporter,
) -> Result<Mode, CliError> {
    match (target, remote_path) {
        // A target is given, with or without a second path argument. The
        // two-argument form and the single-argument rsync `host:path` form are
        // both resolved here (crate::target::resolve). Record the peer if new,
        // then sync over SSH.
        (Some(target), remote_path) => {
            if local_peer.is_some() {
                return Err(CliError::msg(
                    "--local-peer cannot be combined with an <ssh-target> <remote-path>; \
                     choose one peer",
                ));
            }
            let (host, path) = crate::target::resolve(&target, remote_path.as_deref())?;
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
        // A lone remote path with no target: the shell almost certainly meant to
        // pass both, or the single-argument `host:/path` form.
        (None, Some(_)) => Err(CliError::msg(
            "provide both an <ssh-target> and a <remote-path> (e.g. `tomo sync \
             user@host /path`), the single-argument form `tomo sync user@host:/path`, \
             or neither",
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
                    "watching only — no peer configured; run `tomo sync user@host /path` \
                     to connect",
                );
                Ok(Mode::WatchOnly)
            }
        }
    }
}
