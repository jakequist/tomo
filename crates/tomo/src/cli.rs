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

    /// List, inspect, or resolve conflicts.
    ///
    /// Conflicts are resolved automatically and never block sync (last-writer-
    /// wins); the loser is always preserved in history. These commands surface
    /// that record non-blockingly and let you recover a losing version. With no
    /// subcommand, lists the unresolved conflicts.
    Conflicts {
        /// The conflict action to run (defaults to `list`).
        #[command(subcommand)]
        action: Option<ConflictCommand>,
    },

    /// Inspect the history database.
    Db {
        /// The database action to run.
        #[command(subcommand)]
        action: DbCommand,
    },

    /// Hidden developer/diagnostic commands (not part of the stable surface).
    #[command(hide = true)]
    Dev {
        /// The diagnostic action to run.
        #[command(subcommand)]
        action: DevCommand,
    },
}

/// A `tomo conflicts` subcommand.
#[derive(Debug, Subcommand)]
pub enum ConflictCommand {
    /// List recorded conflicts (unresolved only unless `--all`).
    List {
        /// Include already-acknowledged conflicts, not just unresolved ones.
        #[arg(long)]
        all: bool,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Show one conflict in detail, including a diff of the two heads.
    Show {
        /// The conflict id (from `tomo conflicts list`).
        id: i64,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Resolve a conflict: acknowledge it (`--keep-current`) or adopt the
    /// preserved losing version (`--take-loser`).
    Resolve {
        /// The conflict id to resolve. Omit only with `--all`.
        id: Option<i64>,
        /// Keep the current file and mark the conflict acknowledged (the tree
        /// is left untouched).
        #[arg(long)]
        keep_current: bool,
        /// Replace the current file with the preserved losing version, then
        /// mark the conflict resolved. A running `watch` syncs it as a normal
        /// local edit.
        #[arg(long)]
        take_loser: bool,
        /// Mass-acknowledge every unresolved conflict (only with
        /// `--keep-current` semantics; not valid with `--take-loser`).
        #[arg(long)]
        all: bool,
    },
}

/// A `tomo dev` subcommand (hidden diagnostics; not the stable surface).
#[derive(Debug, Subcommand)]
pub enum DevCommand {
    /// List the release binaries embedded into this build's bootstrap payload.
    ///
    /// Empty in ordinary dev builds; populated only when compiled with
    /// `--features embed-binaries` (see `scripts/release.sh`,
    /// `docs/RELEASING.md`). Used to verify a fat binary's embedded inventory.
    EmbeddedBinaries {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
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
