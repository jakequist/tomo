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

    /// Sync this project with a peer in the foreground (the primary command).
    ///
    /// Name the peer as a single rsync-style `user@host:/remote/path` target
    /// (also `host:~/path` for the remote home). It records the peer (if new)
    /// and starts syncing over SSH in one step — the live session's own
    /// bootstrap and handshake are the validation. An identical already-recorded
    /// peer just runs; a *different* target is refused unless `--force`. With no
    /// target it runs against the recorded `[remote]`, or `--local-peer <path>`
    /// for a local directory, or watch-only if neither is configured.
    Sync {
        /// The peer as a single `user@host:/remote/path` target (also
        /// `host:~/path` for the remote home). Omit to resume the recorded peer.
        target: Option<String>,
        /// Removed: the old two-argument `<host> <path>` form. Still captured as
        /// a hidden positional so a stray second argument produces a helpful
        /// "combine them into `host:/path`" error instead of a bare clap error.
        #[arg(hide = true)]
        legacy_remote_path: Option<String>,
        /// Sync with a local project directory instead of over SSH (spawns a
        /// served peer rooted there). Mutually exclusive with a `<target>`.
        #[arg(long, value_name = "PATH")]
        local_peer: Option<PathBuf>,
        /// Overwrite an existing `[remote]` that points at a different target.
        #[arg(long)]
        force: bool,
        /// Emit machine-readable JSON event lines.
        #[arg(long)]
        json: bool,
    },

    /// Record a sync peer for this project and validate the connection.
    ///
    /// `tomo sync <target>` does this automatically as it starts a session;
    /// `connect` is validation *without* starting one — it records the
    /// `[remote]`, bootstraps the remote binary, exchanges the handshake, and
    /// exits. Accepts the same target shape as `sync`: a single
    /// `user@host:/remote/path` (also `host:~/path`). Idempotent: re-running
    /// with the *same* target revalidates the existing peer instead of erroring.
    /// A *different* target is refused unless `--force`, which overwrites the
    /// recorded `[remote]` and revalidates.
    Connect {
        /// The peer as a single `user@host:/remote/path` target (also
        /// `host:~/path` for the remote home).
        target: String,
        /// Removed: the old two-argument `<host> <path>` form. Still captured as
        /// a hidden positional so a stray second argument produces a helpful
        /// "combine them into `host:/path`" error instead of a bare clap error.
        #[arg(hide = true)]
        legacy_remote_path: Option<String>,
        /// Overwrite an existing `[remote]` that points at a different target.
        /// Not needed to re-validate an identical target (that is always
        /// allowed).
        #[arg(long)]
        force: bool,
        /// Explicit SSH private-key path to authenticate with, recorded in the
        /// `[remote]` so `tomo watch` reuses it. Tried before ssh-agent,
        /// `~/.ssh/config`, and the default `id_ed25519`/`id_rsa`. Use when your
        /// key is neither in the agent nor a default name nor selectable via
        /// `~/.ssh/config`.
        #[arg(long, value_name = "PATH")]
        identity: Option<PathBuf>,
    },

    /// Deprecated alias for bare `tomo sync` (kept working; hidden from help).
    #[command(hide = true)]
    Watch {
        /// Sync with a local project directory instead of over SSH: spawn a
        /// served peer rooted there.
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

    /// Show version history, newest first.
    ///
    /// With a `<path>`, shows that path's full history. With no path, shows
    /// recent activity across *all* paths (defaulting to the 20 newest
    /// versions; raise it with `--limit`).
    Log {
        /// The path whose history to show (repo-relative or absolute). Omit for
        /// repo-wide recent activity.
        path: Option<PathBuf>,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
        /// Show at most this many versions (newest first). Repo-wide `log`
        /// defaults to 20.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
    },

    /// Show a textual diff of a path between recorded versions and/or the
    /// working tree.
    ///
    /// By default diffs the newest recorded version against the current
    /// working-tree file. `--version <id>` picks the recorded (base) side;
    /// `--against <id>` replaces the working-tree (target) side with another
    /// recorded version, so `--version A --against B` diffs two recorded
    /// versions. Exit `0` when identical (or binary/oversized, declined), exit
    /// `1` when they differ (git-style).
    Diff {
        /// The path to diff (repo-relative or absolute).
        path: PathBuf,
        /// The recorded version id to use as the base (left) side. Defaults to
        /// the newest recorded version.
        #[arg(long)]
        version: Option<String>,
        /// A recorded version id to use as the target (right) side instead of
        /// the working-tree file.
        #[arg(long)]
        against: Option<String>,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
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

    /// Stream the running session's event feed (control channel, UX-V2 §2).
    ///
    /// Attaches to the live session over its local control socket and relays
    /// every event — file synced/removed, conflicts, connect/disconnect,
    /// transfer progress, heartbeats. Default output is human lines in the same
    /// shape the live session prints; `--json` emits the raw versioned records
    /// (for scripts/CI). Exits cleanly when the session stops. Errors clearly if
    /// no session is running.
    Events {
        /// Emit the raw machine-readable event records instead of human lines.
        #[arg(long)]
        json: bool,
    },

    /// Inspect the history database.
    Db {
        /// The database action to run.
        #[command(subcommand)]
        action: DbCommand,
    },

    /// Print a shell completion script to stdout.
    ///
    /// Generate for your shell and source it, e.g.
    /// `tomo completions bash > ~/.local/share/bash-completion/completions/tomo`
    /// (or `zsh`/`fish`). Safe to pipe: output stops cleanly on a closed reader.
    Completions {
        /// The shell to generate completions for.
        #[arg(value_name = "SHELL")]
        shell: clap_complete::Shell,
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

    /// Show one conflict in detail, including a winner-vs-loser diff.
    Show {
        /// The conflict id (from `tomo conflicts list`), or a project-relative
        /// path — which shows that path's newest unresolved conflict.
        #[arg(value_name = "ID-OR-PATH")]
        target: String,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Resolve a conflict: acknowledge it (`--keep-current`), adopt the
    /// preserved losing version (`--take-loser`), or keep both (`--both`).
    Resolve {
        /// The conflict id to resolve, or a project-relative path — which
        /// targets that path's newest unresolved conflict. Omit only with
        /// `--all` or `--interactive`.
        #[arg(value_name = "ID-OR-PATH")]
        target: Option<String>,
        /// Keep the current file and mark the conflict acknowledged (the tree
        /// is left untouched).
        #[arg(long)]
        keep_current: bool,
        /// Replace the current file with the preserved losing version, then
        /// mark the conflict resolved. A running `watch` syncs it as a normal
        /// local edit.
        #[arg(long)]
        take_loser: bool,
        /// Keep both: materialize the preserved loser alongside the winner as
        /// `<path>.theirs` (for a manual merge), then acknowledge. The sidecar
        /// syncs like any file. Mutually exclusive with the flags above.
        #[arg(long)]
        both: bool,
        /// Mass-acknowledge every unresolved conflict (only with
        /// `--keep-current` semantics; not valid with `--take-loser`/`--both`).
        #[arg(long)]
        all: bool,
        /// Walk every unresolved conflict interactively: show its diff and
        /// prompt keep/take/both/skip per conflict. Requires a terminal on
        /// stdin; ignores a positional target and the other flags.
        #[arg(long)]
        interactive: bool,
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

    /// Resolve an SSH target through `~/.ssh/config` and print the route Tomo
    /// would take — the direct analogue of `ssh -G <target>`, for diffing.
    ///
    /// Per hop: alias, resolved hostname/port/user, identity files (and whether
    /// ssh-agent is skipped), `StrictHostKeyChecking`, the known-hosts files
    /// consulted (user + global), and the `ProxyJump` chain. Pure resolution —
    /// no network. Honors `TOMO_SSH_CONFIG`.
    SshRoute {
        /// The SSH target (`[user@]host[:port]`, or a `~/.ssh/config` alias).
        #[arg(value_name = "TARGET")]
        target: String,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Send one command line to the running session's control socket and print
    /// the reply (the control-channel analogue of `ssh-route`; for scenarios).
    ///
    /// The argument is the command object JSON, e.g.
    /// `tomo dev ctl '{"type":"conflicts_resolve","id":3,"action":"take"}'`.
    Ctl {
        /// The command object JSON (wrapped in the command-mode envelope).
        #[arg(value_name = "JSON")]
        command: String,
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use clap::Parser;

    /// The single-argument `host:/path` target parses with no legacy positional.
    #[test]
    fn sync_accepts_single_target() {
        let cli = Cli::try_parse_from(["tomo", "sync", "user@host:/srv/app"]).unwrap();
        match cli.command {
            Command::Sync {
                target,
                legacy_remote_path,
                ..
            } => {
                assert_eq!(target.as_deref(), Some("user@host:/srv/app"));
                assert_eq!(legacy_remote_path, None);
            }
            other => panic!("expected Sync, got {other:?}"),
        }
    }

    /// Zero-arg `tomo sync` (resume the recorded peer) must keep parsing.
    #[test]
    fn sync_accepts_no_target() {
        let cli = Cli::try_parse_from(["tomo", "sync"]).unwrap();
        match cli.command {
            Command::Sync {
                target,
                legacy_remote_path,
                ..
            } => {
                assert_eq!(target, None);
                assert_eq!(legacy_remote_path, None);
            }
            other => panic!("expected Sync, got {other:?}"),
        }
    }

    /// The removed two-argument form still *parses* (into the hidden positional)
    /// so `sync` can render the friendly error rather than clap a bare one.
    #[test]
    fn sync_two_arg_form_captured_by_hidden_positional() {
        let cli = Cli::try_parse_from(["tomo", "sync", "myhost", "/remote/path"]).unwrap();
        match cli.command {
            Command::Sync {
                target,
                legacy_remote_path,
                ..
            } => {
                assert_eq!(target.as_deref(), Some("myhost"));
                assert_eq!(legacy_remote_path.as_deref(), Some("/remote/path"));
            }
            other => panic!("expected Sync, got {other:?}"),
        }
    }

    /// Same for `connect`, whose target is required.
    #[test]
    fn connect_two_arg_form_captured_by_hidden_positional() {
        let cli = Cli::try_parse_from(["tomo", "connect", "myhost", "/remote/path"]).unwrap();
        match cli.command {
            Command::Connect {
                target,
                legacy_remote_path,
                ..
            } => {
                assert_eq!(target, "myhost");
                assert_eq!(legacy_remote_path.as_deref(), Some("/remote/path"));
            }
            other => panic!("expected Connect, got {other:?}"),
        }
    }

    /// A third positional is still a genuine clap usage error (nothing captures
    /// it), so we never silently swallow extra arguments.
    #[test]
    fn sync_three_positionals_is_a_clap_error() {
        assert!(Cli::try_parse_from(["tomo", "sync", "a", "b", "c"]).is_err());
    }
}
