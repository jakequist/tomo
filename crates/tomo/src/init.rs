//! `tomo init`: create and populate `<project_root>/.tomo/`.
//!
//! Idempotent by contract: re-running on an initialized project changes nothing
//! and succeeds. All state is project-scoped — nothing is written outside the
//! project root (CLAUDE.md invariant #2).

use crate::error::CliError;
use crate::layout::Layout;
use crate::replica;

/// A commented starter `config.toml`. Everything is defaulted, so the file is
/// purely documentation until the user edits it.
const CONFIG_TEMPLATE: &str = "\
# Tomo configuration — see docs/SPEC.md §7.
#
# Path classes (git-style globs, last matching rule wins). Every path is one of:
#   synced+versioned    source files: synced and captured in history (default)
#   synced+unversioned  mirrored between machines but never versioned
#   ignored             never crosses the wire, never versioned
#
# .tomo/** is always ignored and cannot be reconfigured.
#
# Built-in ignores are ON by default: editor/tool temp files (*.swp, *.swx,
# .*.sw?, *~, .#*, #*#, 4913), OS metadata (.DS_Store, Thumbs.db), SQLite/*.db
# sidecars (-wal/-shm/-journal), git metadata (.git, and everything under it —
# for the root repo, nested repos, and submodules), and large regenerable
# dependency/cache trees (node_modules, .venv/venv, __pycache__, .pytest_cache,
# .mypy_cache, .ruff_cache, .terraform), and IDE/editor project dirs (.idea,
# .vscode, .vs, .fleet, .zed, plus *.sublime-workspace). Build outputs (target/,
# build/, dist/) and .env are deliberately NOT ignored — artifact flow-back is a
# headline feature, so opt those out yourself if you want them gone. Built-ins
# are applied before anything below, so a user rule for the
# same pattern overrides them (last matching rule wins). Re-including a whole
# ignored TREE takes two rules, just
# like git — one to un-ignore the directory so the scan descends into it, and one
# for its contents:
#
#   [[rules]]
#   pattern = \".git\"           # un-ignore the directory itself
#   class = \"synced+versioned\"
#   [[rules]]
#   pattern = \".git/**\"        # …and everything under it
#   class = \"synced+versioned\"
#
# To disable ALL built-ins, uncomment default_ignores:
#
# [sync]
# default_ignores = false     # ON by default; set false to disable the built-ins
#
# [[rules]]
# pattern = \"target/\"        # trailing slash expands to target/**
# class = \"ignored\"
#
# [[rules]]
# pattern = \"dist/**\"
# class = \"synced+unversioned\"
# direction = \"pull\"          # both (default) | push | pull

# History capture: adaptive (default) | every-change | off | { interval_ms = 5000 }
[history]
mode = \"adaptive\"

# The sync peer is written here by `tomo connect` (SSH transport lands at M2).
# [remote]
# host = \"user@build-server\"
# path = \"/srv/projects/tomo\"
";

/// Ensure `layout`'s project is fully initialized, creating any missing piece
/// of `.tomo/` (subdirs, config template, replica id).
///
/// Idempotent and *completing*: it fills in whatever is absent rather than
/// bailing the moment `.tomo/` exists. This matters for the SSH bootstrap, which
/// creates `.tomo/bin/` (to push the binary) **before** the remote `serve`
/// starts — a `.tomo/` that only holds `bin/` must still be brought up to a full
/// layout so `serve` finds its replica id and state dirs (CLAUDE.md invariant
/// #2: all state is project-scoped and created here, never elsewhere).
///
/// Returns `true` if `.tomo/` did not previously exist (a fresh project),
/// `false` if it was already present (even if partial). Never clobbers an
/// existing config or replica.
///
/// # Errors
/// [`CliError::Io`] if a directory or file cannot be created, or
/// Restrict `.tomo/` to the owner (0o700) — best-effort (a read-only FS or
/// exotic mount must not break init/session start; the sync still works, it is
/// just not hardened). No-op off Unix. Called at init AND at every session
/// start so pre-existing projects tighten on their next sync.
#[cfg(unix)]
pub fn tighten_tomo_dir(layout: &Layout) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = std::fs::set_permissions(layout.tomo(), std::fs::Permissions::from_mode(0o700));
}

/// Non-Unix stub: no Unix permission bits to set.
#[cfg(not(unix))]
pub fn tighten_tomo_dir(_layout: &Layout) {}

/// [`CliError::Message`] if a replica id cannot be generated.
pub fn ensure_initialized(layout: &Layout) -> Result<bool, CliError> {
    let fresh = !layout.is_initialized();

    std::fs::create_dir_all(layout.tomo())
        .map_err(|s| CliError::io("create directory", layout.tomo(), s))?;
    // `.tomo/` holds the full file history, session state, and the control
    // socket — private to the owner. 0o700 on the top-level dir gates
    // everything beneath it via directory traversal, so a shared machine's
    // other users can neither read the history DB nor reach `ctl.sock`.
    tighten_tomo_dir(layout);
    for dir in layout.dirs() {
        std::fs::create_dir_all(&dir).map_err(|s| CliError::io("create directory", &dir, s))?;
    }

    // Write the config template only if absent (never clobber a user's file).
    let config = layout.config();
    if !config.exists() {
        std::fs::write(&config, CONFIG_TEMPLATE)
            .map_err(|s| CliError::io("write config template", &config, s))?;
    }

    // Mint and persist a stable replica id if one is not already present.
    let replica_path = layout.replica();
    if !replica_path.exists() {
        let id = replica::generate()?;
        std::fs::write(&replica_path, format!("{}\n", replica::format(id)))
            .map_err(|s| CliError::io("write replica id", &replica_path, s))?;
    }

    Ok(fresh)
}

/// The line `tomo init` ensures is present in a git repo's `.gitignore`.
const GITIGNORE_ENTRY: &str = ".tomo/";

/// Whether `root` is a git repository — i.e. `<root>/.git` exists, as either a
/// directory (an ordinary clone) or a file (a worktree/submodule `gitdir:`
/// pointer). Uses [`Path::exists`], so a `.git` of either kind counts.
fn is_git_repo(root: &std::path::Path) -> bool {
    root.join(".git").exists()
}

/// Decide the `.gitignore` bytes to write so `<root>/.gitignore` ignores
/// `.tomo/`, or `None` if it already does (nothing to do).
///
/// Pure so the append/create/idempotence rules are unit-tested without disk:
/// - `existing = None` (no file) → create it holding just the entry.
/// - the entry (any common `.tomo`/`/.tomo` slash variant, on its own line and
///   not commented) already present → `None`, leaving the file untouched.
/// - otherwise append the entry, inserting a newline first only when the file
///   does not already end in one — never reordering or rewriting a single
///   existing byte.
fn plan_gitignore(existing: Option<&str>) -> Option<String> {
    match existing {
        None => Some(format!("{GITIGNORE_ENTRY}\n")),
        Some(text) => {
            let already = text.lines().any(|line| {
                let t = line.trim();
                matches!(t, ".tomo/" | ".tomo" | "/.tomo/" | "/.tomo")
            });
            if already {
                return None;
            }
            let mut out = text.to_owned();
            // Newline guard: only add a separating newline when the file does
            // not already end in one (and is non-empty).
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(GITIGNORE_ENTRY);
            out.push('\n');
            Some(out)
        }
    }
}

/// If `root` is a git repo, ensure its `.gitignore` ignores `.tomo/`, creating
/// or appending as needed. Returns `true` iff it actually wrote (so the caller
/// prints the checklist line only when something changed). A no-git-repo project
/// is a silent no-op; an already-ignoring `.gitignore` is left byte-for-byte
/// untouched.
///
/// # Errors
/// [`CliError::Io`] if the `.gitignore` cannot be read or written.
fn ensure_gitignore(root: &std::path::Path) -> Result<bool, CliError> {
    if !is_git_repo(root) {
        return Ok(false);
    }
    let path = root.join(".gitignore");
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => return Err(CliError::io("read .gitignore", &path, source)),
    };
    match plan_gitignore(existing.as_deref()) {
        Some(updated) => {
            std::fs::write(&path, updated)
                .map_err(|s| CliError::io("write .gitignore", &path, s))?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Run the `tomo init` command, printing the outcome.
///
/// # Errors
/// Propagates any failure from [`ensure_initialized`].
pub fn run(layout: &Layout) -> Result<(), CliError> {
    let style = crate::style::current();
    let fresh = ensure_initialized(layout)?;

    // Drop the agent-context README into `.tomo/` (best-effort — a failure to
    // write it must never fail `init`). A session later refreshes it with the
    // live peer identity; here there is no peer yet, so it names none.
    let wrote_readme =
        crate::readme::write_default(layout, &crate::buildinfo::binary_version()).unwrap_or(false);
    let msg = if fresh {
        format!("initialized Tomo project in {}", layout.tomo().display())
    } else {
        format!(
            "already a Tomo project ({} exists) — nothing to do",
            layout.tomo().display()
        )
    };
    let added_gitignore = ensure_gitignore(layout.root())?;
    if style.enabled() {
        println!("{} {msg}", style.ok(style.g_ok()));
        if added_gitignore {
            println!("{} added .tomo/ to .gitignore", style.ok(style.g_ok()));
        }
        if wrote_readme {
            println!(
                "{} wrote .tomo/README.md (agent context)",
                style.ok(style.g_ok())
            );
        }
    } else {
        println!("{msg}");
        if added_gitignore {
            println!("added .tomo/ to .gitignore");
        }
        if wrote_readme {
            println!("wrote .tomo/README.md (agent context)");
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn init_creates_full_layout() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        assert!(ensure_initialized(&layout).unwrap());

        assert!(layout.tomo().is_dir());
        assert!(layout.db().is_dir());
        assert!(layout.staging().is_dir());
        assert!(layout.logs().is_dir());
        assert!(layout.state().is_dir());
        assert!(layout.config().is_file());
        assert!(layout.replica().is_file());

        // The replica file holds a parseable id.
        let text = std::fs::read_to_string(layout.replica()).unwrap();
        assert!(replica::parse(&text).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn init_creates_a_private_tomo_dir_and_tightens_existing() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        ensure_initialized(&layout).unwrap();
        let mode = std::fs::metadata(layout.tomo())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, ".tomo must be owner-only");

        // A pre-existing loose dir (older tomo version) tightens on re-init.
        std::fs::set_permissions(layout.tomo(), std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_initialized(&layout).unwrap();
        let mode = std::fs::metadata(layout.tomo())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "re-init tightens a loose .tomo");
    }

    #[test]
    fn init_is_idempotent_and_preserves_replica() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        assert!(ensure_initialized(&layout).unwrap());
        let id1 = std::fs::read_to_string(layout.replica()).unwrap();

        // Second run is a no-op and must not mint a new id.
        assert!(!ensure_initialized(&layout).unwrap());
        let id2 = std::fs::read_to_string(layout.replica()).unwrap();
        assert_eq!(id1, id2);
    }

    // ---- .gitignore management (Item 2) ---------------------------------

    #[test]
    fn plan_gitignore_creates_when_absent() {
        assert_eq!(plan_gitignore(None).as_deref(), Some(".tomo/\n"));
    }

    #[test]
    fn plan_gitignore_appends_when_missing_entry() {
        // No trailing newline → a separating newline is inserted first.
        assert_eq!(
            plan_gitignore(Some("target/\n*.log")).as_deref(),
            Some("target/\n*.log\n.tomo/\n")
        );
        // Trailing newline already present → no double blank line.
        assert_eq!(
            plan_gitignore(Some("target/\n")).as_deref(),
            Some("target/\n.tomo/\n")
        );
    }

    #[test]
    fn plan_gitignore_idempotent_when_entry_present() {
        // Every accepted slash variant, and a commented lookalike that must NOT
        // count as present.
        for present in [".tomo/", ".tomo", "/.tomo/", "/.tomo"] {
            let doc = format!("target/\n{present}\n*.log\n");
            assert_eq!(plan_gitignore(Some(&doc)), None, "variant {present:?}");
        }
        // A comment mentioning .tomo/ does not satisfy the requirement.
        assert!(plan_gitignore(Some("# ignore .tomo/ maybe\n")).is_some());
    }

    #[test]
    fn ensure_gitignore_creates_in_fresh_repo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        assert!(ensure_gitignore(dir.path()).unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".gitignore")).unwrap(),
            ".tomo/\n"
        );
        // Idempotent second run does nothing.
        assert!(!ensure_gitignore(dir.path()).unwrap());
    }

    #[test]
    fn ensure_gitignore_appends_preserving_existing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "node_modules/\n").unwrap();
        assert!(ensure_gitignore(dir.path()).unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".gitignore")).unwrap(),
            "node_modules/\n.tomo/\n"
        );
    }

    #[test]
    fn ensure_gitignore_no_op_when_already_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let original = "a/\n.tomo/\nb/\n";
        std::fs::write(dir.path().join(".gitignore"), original).unwrap();
        assert!(!ensure_gitignore(dir.path()).unwrap());
        // Untouched byte-for-byte.
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".gitignore")).unwrap(),
            original
        );
    }

    #[test]
    fn ensure_gitignore_silent_without_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        // No .git → no-op, and crucially no .gitignore is created.
        assert!(!ensure_gitignore(dir.path()).unwrap());
        assert!(!dir.path().join(".gitignore").exists());
    }

    #[test]
    fn ensure_gitignore_treats_dot_git_file_as_repo() {
        let dir = tempfile::tempdir().unwrap();
        // Worktrees/submodules use a .git FILE holding a `gitdir:` pointer.
        std::fs::write(
            dir.path().join(".git"),
            "gitdir: /somewhere/.git/worktrees/x\n",
        )
        .unwrap();
        assert!(ensure_gitignore(dir.path()).unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".gitignore")).unwrap(),
            ".tomo/\n"
        );
    }

    #[test]
    fn init_does_not_clobber_existing_config() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        std::fs::create_dir_all(layout.tomo()).unwrap();
        std::fs::write(layout.config(), "# custom\n").unwrap();
        // .tomo exists, so ensure_initialized reports "already initialized" and
        // leaves the config untouched.
        assert!(!ensure_initialized(&layout).unwrap());
        assert_eq!(
            std::fs::read_to_string(layout.config()).unwrap(),
            "# custom\n"
        );
    }
}
