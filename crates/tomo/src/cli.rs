//! The command-line surface (docs/SPEC.md §9), parsed with `clap` derive.
//!
//! The full surface is visible from day one — even commands whose engines land
//! in later milestones — so the CLI is honest about what exists and what is
//! coming (they exit `2` with a one-line "lands at Mx" message). Informational
//! commands take `--json`, which the e2e scenarios assert against.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Tomo — real-time, two-way file sync with full history.
#[derive(Debug, Parser)]
#[command(name = "tomo", version, about, long_about = None)]
pub struct Cli {
    /// The command to run.
    #[command(subcommand)]
    pub command: Command,
}

/// A Tomo subcommand.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Initialize a Tomo project in the current directory (create `.tomo/`).
    Init,

    /// Record a sync peer for this project (SSH transport lands at M2).
    Connect {
        /// SSH target, e.g. `user@host`.
        target: String,
        /// The peer's project-root path.
        remote_path: String,
    },

    /// Watch this project and sync it in the foreground.
    Watch {
        /// Sync with a local project directory instead of over SSH (M1 local
        /// transport): spawn a served peer rooted there.
        #[arg(long, value_name = "PATH")]
        local_peer: Option<PathBuf>,
        /// Emit machine-readable JSON event lines.
        #[arg(long)]
        json: bool,
    },

    /// Serve this project over stdio (used internally by `--local-peer`).
    #[command(hide = true)]
    Serve {
        /// Speak the protocol over our own stdin/stdout.
        #[arg(long)]
        stdio: bool,
    },

    /// Show sync status for this project.
    Status {
        /// Emit the machine-readable status JSON.
        #[arg(long)]
        json: bool,
    },

    /// Show the version history of a path, newest first.
    Log {
        /// The path whose history to show (repo-relative or absolute).
        path: PathBuf,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
        /// Show at most this many versions (newest first).
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
    },

    /// Restore a path to a previous version.
    ///
    /// With no `--version`, restores the version *before* the current newest —
    /// the common "undo my last save". Pass `--version <id>` (an id from
    /// `tomo log`) to restore an exact version. The restore is written through
    /// staging + atomic rename; if a `tomo watch` session is running it then
    /// syncs the restored bytes to the peer as an ordinary local change.
    Restore {
        /// The path to restore (repo-relative or absolute).
        path: PathBuf,
        /// The exact version id to restore (from `tomo log`). Defaults to the
        /// version before the current newest.
        #[arg(long)]
        version: Option<String>,
        /// Write the restored bytes to stdout instead of the file on disk.
        #[arg(long)]
        stdout: bool,
    },

    /// List or resolve conflicts (lands at M4).
    Conflicts {
        /// `list` (default) or `resolve`.
        action: Option<String>,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Inspect the history database.
    Db {
        /// The database action to run.
        #[command(subcommand)]
        action: DbCommand,
    },
}

/// A `tomo db` subcommand.
#[derive(Debug, Subcommand)]
pub enum DbCommand {
    /// Verify the integrity of the history store (exit `0` healthy, `1` on
    /// problems found).
    Check {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
}
