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

    /// Show the version history of a path (lands at M3).
    Log {
        /// The path whose history to show.
        path: PathBuf,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Restore a path to a previous version (lands at M3).
    Restore {
        /// The path to restore.
        path: PathBuf,
        /// The version id to restore (defaults to the latest).
        #[arg(long)]
        version: Option<String>,
    },

    /// List or resolve conflicts (lands at M4).
    Conflicts {
        /// `list` (default) or `resolve`.
        action: Option<String>,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
}
