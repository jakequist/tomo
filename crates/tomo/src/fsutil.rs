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
    write_staged(staging_dir, final_path, bytes, None)
}

/// Like [`atomic_write`], but sets the final file's Unix mode as part of the
/// atomic swap: `0o755` when `exec`, else `0o644` (git's simple executable-bit
/// model; a umask-aware refinement stays `[open]`, docs/SPEC.md §12). The mode
/// is applied to the staging file **before** the rename, so the file appears at
/// its final path with the correct mode in one atomic step (invariant #8). On
/// non-Unix platforms `exec` is ignored (the OS has no such bit).
///
/// # Errors
/// [`CliError::Io`] if any of the create/write/chmod/sync/rename steps fail.
pub fn atomic_write_mode(
    staging_dir: &Path,
    final_path: &Path,
    bytes: &[u8],
    exec: bool,
) -> Result<(), CliError> {
    let mode = if exec { 0o755 } else { 0o644 };
    write_staged(staging_dir, final_path, bytes, Some(mode))
}

/// Shared body: stage `bytes` in `staging_dir`, optionally set `mode`, fsync,
/// then atomically rename over `final_path`, cleaning up the temp on failure.
fn write_staged(
    staging_dir: &Path,
    final_path: &Path,
    bytes: &[u8],
    mode: Option<u32>,
) -> Result<(), CliError> {
    let temp: PathBuf = staging_dir.join(format!("{}.tmp", random_hex()?));

    let result = write_and_rename(&temp, final_path, bytes, mode);
    if result.is_err() {
        // Best-effort cleanup; the original error is what we return.
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn write_and_rename(
    temp: &Path,
    final_path: &Path,
    bytes: &[u8],
    mode: Option<u32>,
) -> Result<(), CliError> {
    {
        let mut file = std::fs::File::create(temp)
            .map_err(|s| CliError::io("create staging file", temp, s))?;
        file.write_all(bytes)
            .map_err(|s| CliError::io("write staging file", temp, s))?;
        set_mode(&file, temp, mode)?;
        file.sync_all()
            .map_err(|s| CliError::io("sync staging file", temp, s))?;
    }
    std::fs::rename(temp, final_path).map_err(|s| CliError::io("rename into place", final_path, s))
}

/// Apply `mode` (if any) to the just-written staging `file` on Unix. A no-op
/// when `mode` is `None` or off Unix.
#[cfg(unix)]
fn set_mode(file: &std::fs::File, temp: &Path, mode: Option<u32>) -> Result<(), CliError> {
    use std::os::unix::fs::PermissionsExt as _;
    if let Some(mode) = mode {
        file.set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(|s| CliError::io("set staging file mode", temp, s))?;
    }
    Ok(())
}

/// Non-Unix stub: there is no executable bit to set.
#[cfg(not(unix))]
fn set_mode(_file: &std::fs::File, _temp: &Path, _mode: Option<u32>) -> Result<(), CliError> {
    Ok(())
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

    #[cfg(unix)]
    #[test]
    fn atomic_write_mode_sets_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();

        let exe = dir.path().join("run.sh");
        atomic_write_mode(&staging, &exe, b"#!/bin/sh\n", true).unwrap();
        let m = std::fs::metadata(&exe).unwrap().permissions().mode();
        assert_eq!(m & 0o777, 0o755, "executable file is 0o755");

        // A non-exec write of the same path clears it back to 0o644.
        let plain = dir.path().join("data.txt");
        atomic_write_mode(&staging, &plain, b"data\n", false).unwrap();
        let m = std::fs::metadata(&plain).unwrap().permissions().mode();
        assert_eq!(m & 0o777, 0o644, "non-executable file is 0o644");
        assert_eq!(std::fs::read_dir(&staging).unwrap().count(), 0);
    }
}
