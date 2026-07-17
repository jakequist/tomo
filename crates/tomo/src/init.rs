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
/// [`CliError::Message`] if a replica id cannot be generated.
pub fn ensure_initialized(layout: &Layout) -> Result<bool, CliError> {
    let fresh = !layout.is_initialized();

    std::fs::create_dir_all(layout.tomo())
        .map_err(|s| CliError::io("create directory", layout.tomo(), s))?;
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

/// Run the `tomo init` command, printing the outcome.
///
/// # Errors
/// Propagates any failure from [`ensure_initialized`].
pub fn run(layout: &Layout) -> Result<(), CliError> {
    if ensure_initialized(layout)? {
        println!("initialized Tomo project in {}", layout.tomo().display());
    } else {
        println!(
            "already a Tomo project ({} exists) — nothing to do",
            layout.tomo().display()
        );
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
