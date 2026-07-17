//! Tomo CLI entry point.
//!
//! Commands (docs/SPEC.md §9): init, connect, watch, serve, status, log,
//! restore, conflicts. Informational commands support `--json` from day one —
//! the e2e scenarios assert against it. Libraries return data; only this crate
//! renders output and errors (CLAUDE.md hygiene policy).

mod apply;
mod buildinfo;
mod cli;
mod connect;
mod error;
mod fsutil;
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

use crate::cli::{Cli, Command};
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
        Command::Log { .. } => Err(CliError::Unimplemented(
            "`tomo log` lands at M3 (history)".to_owned(),
        )),
        Command::Restore { .. } => Err(CliError::Unimplemented(
            "`tomo restore` lands at M3 (history)".to_owned(),
        )),
        Command::Conflicts { .. } => Err(CliError::Unimplemented(
            "`tomo conflicts` lands at M4 (conflict tooling)".to_owned(),
        )),
    }
}
