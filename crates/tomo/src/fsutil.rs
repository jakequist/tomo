//! Crash-safe filesystem primitives shared across the CLI.
//!
//! Every durable write goes through [`atomic_write`]: bytes land in a uniquely
//! named file under `.tomo/staging/` and are then `rename(2)`d over the final
//! path. Because staging and the target share the `.tomo/` filesystem, the
//! rename is atomic, so a `kill -9` mid-write can never leave a half-written
//! index, status file, or synced file visible at its final path (CLAUDE.md
//! invariant #8).

use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::error::CliError;

/// A short random lowercase-hex token for uniquely naming a staging file.
///
/// # Errors
/// [`CliError::Message`] if the OS entropy source is unavailable.
pub fn random_hex() -> Result<String, CliError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|e| CliError::msg(format!("could not read OS entropy: {e}")))?;
    let mut s = String::with_capacity(32);
    for b in bytes {
        use std::fmt::Write as _;
        // Writing to a String never fails.
        let _ = write!(s, "{b:02x}");
    }
    Ok(s)
}

/// Atomically write `bytes` to `final_path`, staging the temporary file in
/// `staging_dir`.
///
/// Fsyncs the staged file before the rename so the bytes are durable, then
/// renames it into place. On any failure the partial staging file is removed.
///
/// # Errors
/// [`CliError::Io`] if any of the create/write/sync/rename steps fail.
pub fn atomic_write(staging_dir: &Path, final_path: &Path, bytes: &[u8]) -> Result<(), CliError> {
    let temp: PathBuf = staging_dir.join(format!("{}.tmp", random_hex()?));

    let result = write_and_rename(&temp, final_path, bytes);
    if result.is_err() {
        // Best-effort cleanup; the original error is what we return.
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn write_and_rename(temp: &Path, final_path: &Path, bytes: &[u8]) -> Result<(), CliError> {
    {
        let mut file = std::fs::File::create(temp)
            .map_err(|s| CliError::io("create staging file", temp, s))?;
        file.write_all(bytes)
            .map_err(|s| CliError::io("write staging file", temp, s))?;
        file.sync_all()
            .map_err(|s| CliError::io("sync staging file", temp, s))?;
    }
    std::fs::rename(temp, final_path).map_err(|s| CliError::io("rename into place", final_path, s))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn random_hex_is_32_chars_and_varies() {
        let a = random_hex().unwrap();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, random_hex().unwrap());
    }

    #[test]
    fn atomic_write_lands_bytes_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let target = dir.path().join("out.bin");

        atomic_write(&staging, &target, b"hello").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"hello");
        // Staging is empty again (temp renamed away).
        assert_eq!(std::fs::read_dir(&staging).unwrap().count(), 0);
    }

    #[test]
    fn atomic_write_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let target = dir.path().join("out.bin");
        std::fs::write(&target, b"old").unwrap();

        atomic_write(&staging, &target, b"new-and-longer").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new-and-longer");
    }
}
