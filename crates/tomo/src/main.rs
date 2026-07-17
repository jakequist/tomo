//! Tomo CLI entry point.
//!
//! Commands (docs/SPEC.md §9): init, connect, watch, serve, status, log,
//! restore, conflicts. Informational commands support `--json` from day one —
//! the e2e scenarios assert against it. Libraries return data; only this crate
//! renders output and errors (CLAUDE.md hygiene policy).

mod apply;
mod buildinfo;
mod chunkxfer;
mod cli;
mod conflicts_cmd;
mod connect;
mod error;
mod fsutil;
mod histmode;
mod history_cmd;
mod init;
mod layout;
mod persist;
mod replica;
mod report;
mod serve;
mod session;
mod status;
mod transport;
mod watch;

use std::path::PathBuf;

use clap::Parser;

use crate::cli::{Cli, Command, ConflictCommand, DbCommand};
use crate::error::CliError;
use crate::layout::Layout;

fn main() {
    let cli = Cli::parse();
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
        } => connect::run(&layout_here()?, &target, &remote_path),
        Command::Watch { local_peer, json } => watch::run(local_peer, json),
        Command::Serve { stdio } => serve::run(stdio),
        Command::Status { json } => status::run(&layout_here()?, json),
        Command::Log { path, json, limit } => {
            history_cmd::run_log(&layout_here()?, &path, json, limit)
        }
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
    }
}
