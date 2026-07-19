//! Tomo CLI entry point.
//!
//! Commands (docs/SPEC.md §9): init, connect, watch, serve, status, log,
//! restore, conflicts. Informational commands support `--json` from day one —
//! the e2e scenarios assert against it. Libraries return data; only this crate
//! renders output and errors (CLAUDE.md hygiene policy).

// musl's default allocator is slow (docs/SPEC.md §3), so release musl builds use
// mimalloc as the global allocator. Registering it needs no `unsafe` in our
// code, so it coexists with the workspace-wide `forbid(unsafe_code)`.
#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod apply;
mod applyguard;
mod buildinfo;
mod chunkxfer;
mod cli;
mod completions;
mod conflicts_cmd;
mod connect;
mod crossing;
mod dev_cmd;
mod diff_cmd;
mod error;
mod fsguard;
mod fsprobe;
mod fsutil;
mod histmode;
mod history_cmd;
mod init;
mod layout;
mod lockfile;
mod out;
mod overlap;
mod persist;
mod replica;
mod report;
mod serve;
mod session;
mod status;
mod style;
mod sync;
mod target;
mod textdiff;
mod transport;
mod watch;

use std::path::PathBuf;

use clap::{CommandFactory, FromArgMatches};

use crate::cli::{Cli, Command, ConflictCommand, DbCommand, DevCommand};
use crate::error::CliError;
use crate::layout::Layout;

fn main() {
    // Detect terminal styling capability exactly once, from stdout, and install
    // it process-wide before anything renders (style.rs). Every rendering helper
    // is a no-op while this stays the disabled default, so a pipe / NO_COLOR /
    // `--json` path is byte-identical to plain output.
    let style = style::detect(&std::io::stdout());
    style::init(style);

    // Parse with our resolved help/usage color scheme (clap detects too, but we
    // pass our decision explicitly so help matches the rest of the CLI).
    let command = Cli::command().styles(style::clap_styles(style));
    let cli = match command.try_get_matches() {
        Ok(matches) => match Cli::from_arg_matches(&matches) {
            Ok(cli) => cli,
            Err(err) => err.exit(),
        },
        Err(err) => err.exit(),
    };

    if let Err(err) = dispatch(cli.command) {
        error::render(&err);
        std::process::exit(err.exit_code());
    }
}

/// The current working directory, which is the project root for `init`, `watch`,
/// `serve`, `status`, and `connect`.
fn current_dir() -> Result<PathBuf, CliError> {
    std::env::current_dir()
        .map_err(|e| CliError::msg(format!("cannot determine the current directory: {e}")))
}

/// A [`Layout`] rooted at the current working directory.
fn layout_here() -> Result<Layout, CliError> {
    Ok(Layout::new(current_dir()?))
}

fn dispatch(command: Command) -> Result<(), CliError> {
    match command {
        Command::Init => init::run(&layout_here()?),
        Command::Connect {
            target,
            remote_path,
            force,
            identity,
        } => {
            let identity = identity.as_ref().map(|p| p.to_string_lossy().into_owned());
            connect::run(
                &layout_here()?,
                &target,
                remote_path.as_deref(),
                force,
                identity.as_deref(),
            )
        }
        Command::Sync {
            target,
            remote_path,
            local_peer,
            force,
            json,
        } => sync::run(target, remote_path, local_peer, force, json),
        Command::Watch { local_peer, json } => watch::run(local_peer, json),
        Command::Serve { stdio } => serve::run(stdio),
        Command::Status { json } => status::run(&layout_here()?, json),
        Command::Log { path, json, limit } => {
            let layout = layout_here()?;
            match path {
                Some(path) => history_cmd::run_log(&layout, &path, json, limit),
                None => history_cmd::run_recent(&layout, json, limit),
            }
        }
        Command::Diff {
            path,
            version,
            against,
            json,
        } => diff_cmd::run(
            &layout_here()?,
            &path,
            version.as_deref(),
            against.as_deref(),
            json,
        ),
        Command::Restore {
            path,
            version,
            stdout,
        } => history_cmd::run_restore(&layout_here()?, &path, version.as_deref(), stdout),
        Command::Conflicts { action } => {
            let layout = layout_here()?;
            match action.unwrap_or(ConflictCommand::List {
                all: false,
                json: false,
            }) {
                ConflictCommand::List { all, json } => conflicts_cmd::run_list(&layout, all, json),
                ConflictCommand::Show { id, json } => conflicts_cmd::run_show(&layout, id, json),
                ConflictCommand::Resolve {
                    id,
                    keep_current,
                    take_loser,
                    all,
                } => conflicts_cmd::run_resolve(&layout, id, keep_current, take_loser, all),
            }
        }
        Command::Db { action } => match action {
            DbCommand::Check { json } => history_cmd::run_db_check(&layout_here()?, json),
        },
        Command::Completions { shell } => completions::run(shell),
        Command::Dev { action } => match action {
            DevCommand::EmbeddedBinaries { json } => dev_cmd::run_embedded_binaries(json),
            DevCommand::SshRoute { target, json } => dev_cmd::run_ssh_route(&target, json),
        },
    }
}
