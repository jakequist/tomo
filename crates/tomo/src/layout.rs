//! The on-disk layout of `<project_root>/.tomo/` (CLAUDE.md invariant #2: all
//! state is project-scoped; nothing is written outside this tree).
//!
//! [`Layout`] is a pure path calculator — it computes the well-known locations
//! but performs no I/O itself; callers create directories via
//! [`crate::init`]. Keeping it a plain value keeps the rest of the crate from
//! sprinkling `.join(".tomo")` everywhere.

use std::path::{Path, PathBuf};

use tomo_config::TOMO_DIR;

/// Computed paths for one project root's `.tomo/` state directory.
#[derive(Debug, Clone)]
pub struct Layout {
    root: PathBuf,
    tomo: PathBuf,
}

impl Layout {
    /// Build the layout for `root` (the project root, i.e. the parent of
    /// `.tomo/`).
    pub fn new(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_path_buf();
        let tomo = root.join(TOMO_DIR);
        Self { root, tomo }
    }

    /// The project root (the tree that is synced).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The `.tomo/` state directory.
    pub fn tomo(&self) -> &Path {
        &self.tomo
    }

    /// Whether this project appears initialized (its `.tomo/` exists).
    pub fn is_initialized(&self) -> bool {
        self.tomo.is_dir()
    }

    /// `.tomo/config.toml`.
    pub fn config(&self) -> PathBuf {
        self.tomo.join("config.toml")
    }

    /// `.tomo/replica` — the hex-encoded stable replica id.
    pub fn replica(&self) -> PathBuf {
        self.tomo.join("replica")
    }

    /// `.tomo/db/` — history content store + metadata (M3).
    pub fn db(&self) -> PathBuf {
        self.tomo.join("db")
    }

    /// `.tomo/staging/` — in-flight writes before atomic rename (invariant #8).
    pub fn staging(&self) -> PathBuf {
        self.tomo.join("staging")
    }

    /// `.tomo/staging/chunks/` — received chunk bytes for in-progress large-file
    /// assemblies (invariant #8: a partial assembly lives entirely here, so a
    /// `kill -9` leaves only garbage in this directory and never a torn file at
    /// its final path). Wiped at startup — assemblies never survive a restart.
    pub fn chunks(&self) -> PathBuf {
        self.staging().join("chunks")
    }

    /// `.tomo/logs/`.
    pub fn logs(&self) -> PathBuf {
        self.tomo.join("logs")
    }

    /// `.tomo/logs/serve.log` — the serve-mode diagnostics sink (stdout is the
    /// protocol channel, so nothing may print there).
    pub fn serve_log(&self) -> PathBuf {
        self.logs().join("serve.log")
    }

    /// `.tomo/state/` — the persisted index and status snapshot.
    pub fn state(&self) -> PathBuf {
        self.tomo.join("state")
    }

    /// `.tomo/state/index.bin` — the postcard-serialized engine index.
    pub fn index(&self) -> PathBuf {
        self.state().join("index.bin")
    }

    /// `.tomo/state/status.json` — the machine-readable status snapshot.
    pub fn status(&self) -> PathBuf {
        self.state().join("status.json")
    }

    /// The subdirectories `tomo init` must create under `.tomo/`.
    pub fn dirs(&self) -> [PathBuf; 4] {
        [self.db(), self.staging(), self.logs(), self.state()]
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_under_tomo() {
        let l = Layout::new("/proj");
        assert_eq!(l.tomo(), Path::new("/proj/.tomo"));
        assert_eq!(l.config(), Path::new("/proj/.tomo/config.toml"));
        assert_eq!(l.replica(), Path::new("/proj/.tomo/replica"));
        assert_eq!(l.index(), Path::new("/proj/.tomo/state/index.bin"));
        assert_eq!(l.status(), Path::new("/proj/.tomo/state/status.json"));
        assert_eq!(l.serve_log(), Path::new("/proj/.tomo/logs/serve.log"));
    }

    #[test]
    fn dirs_lists_the_four_subdirs() {
        let l = Layout::new("/p");
        let dirs = l.dirs();
        assert!(dirs.contains(&PathBuf::from("/p/.tomo/db")));
        assert!(dirs.contains(&PathBuf::from("/p/.tomo/staging")));
        assert!(dirs.contains(&PathBuf::from("/p/.tomo/logs")));
        assert!(dirs.contains(&PathBuf::from("/p/.tomo/state")));
    }
}
