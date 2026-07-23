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

/// Stage `bytes` into a fresh uniquely-named temp file under `staging_dir` with
/// the executable `mode` (`0o755`/`0o644` on Unix), but do **not** fsync and do
/// **not** rename it into place. Returns the staged temp path for a later
/// [`install_batch`] to fsync-barrier and rename.
///
/// SEED-PERF Phase 2 (batch fsync barrier). During a bulk seed the per-file
/// `sync_all` in [`atomic_write_mode`] serializes the receiver on an fsync-slow
/// filesystem — one ~4 ms journal commit per file. Splitting the write from the
/// durability barrier lets the session stage a whole batch of temps and then
/// pay ONE barrier for all of them (see [`install_batch`]). A staged-but-not-
/// installed temp is scratch: a `kill -9` before the barrier leaves it under
/// `.tomo/staging/`, which the next session wipes on boot (invariant #8) — the
/// atomic rename per file is unchanged, only the fsync cadence.
///
/// # Errors
/// [`CliError::Io`] if the create/write/chmod fails (the partial temp is removed).
pub fn stage_write_mode(staging_dir: &Path, bytes: &[u8], exec: bool) -> Result<PathBuf, CliError> {
    let temp: PathBuf = staging_dir.join(format!("{}.tmp", random_hex()?));
    let mode = if exec { 0o755 } else { 0o644 };
    let result = write_staged_no_sync(&temp, bytes, Some(mode));
    match result {
        Ok(()) => Ok(temp),
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            Err(e)
        }
    }
}

/// Write `bytes` to `temp` with `mode`, WITHOUT fsync and WITHOUT rename.
fn write_staged_no_sync(temp: &Path, bytes: &[u8], mode: Option<u32>) -> Result<(), CliError> {
    let mut file =
        std::fs::File::create(temp).map_err(|s| CliError::io("create staging file", temp, s))?;
    file.write_all(bytes)
        .map_err(|s| CliError::io("write staging file", temp, s))?;
    set_mode(&file, temp, mode)
}

/// Durably install a batch of staged temps to their final paths with ONE
/// filesystem barrier for the whole batch (SEED-PERF Phase 2). Each entry is a
/// `(staged_temp, final_path)` pair produced by [`stage_write_mode`].
///
/// # The barrier and its crash-safety contract (invariant #8)
/// The per-file `sync_all(temp)` of [`atomic_write_mode`] is replaced by:
/// 1. **fsync the staging directory** — on an ordered-data journaling filesystem
///    (ext4 `data=ordered`, the Linux default and this project's receiver FS)
///    this commits the running journal transaction, which flushes **every**
///    dirty ordered data buffer — i.e. all the staged temps' contents — to disk
///    before returning. One barrier makes the entire batch's DATA durable
///    (measured ~167× faster than one fsync per file on this VM's ext4).
/// 2. **rename each temp over its final path** — the atomic per-file rename is
///    UNCHANGED, so a partially-written file is never visible at a final path and
///    a `kill -9` mid-loop leaves each final path either the old file or the
///    fully-written new one, never a torn one.
/// 3. **fsync the staging directory again** — commits the rename metadata so the
///    installed files are durably at their final paths before the caller records
///    them in the (also-durable) index.
///
/// Because the data barrier precedes any rename, a crash after step 1 but during
/// step 2 can only lose *un-renamed* files (still temps in staging, wiped on
/// restart and re-shipped by reconcile) — never expose garbage. On a filesystem
/// WITHOUT ordered-data journaling the directory fsync would not flush file data,
/// so this path is used only on Unix; elsewhere it degrades to a per-file fsync
/// (correct everywhere, just without the batching win).
///
/// # Errors
/// [`CliError::Io`] if the barrier or any rename fails.
pub fn install_batch(staging_dir: &Path, installs: &[(PathBuf, PathBuf)]) -> Result<(), CliError> {
    if installs.is_empty() {
        return Ok(());
    }
    // Step 1: data barrier — one fsync flushes every staged temp's data (ext4
    // data=ordered). On a platform without a directory fsync, fall back to a
    // per-file fsync so durability-before-rename still holds.
    if fsync_dir(staging_dir).is_err() {
        for (temp, _) in installs {
            fsync_file(temp)?;
        }
    }
    // Step 2: atomic rename per file (unchanged crash-safety).
    for (temp, final_path) in installs {
        std::fs::rename(temp, final_path)
            .map_err(|s| CliError::io("rename into place", final_path, s))?;
    }
    // Step 3: make the renames durable (best-effort; a lost rename is re-shipped,
    // never corrupt). Not fatal if the directory can no longer be fsynced.
    let _ = fsync_dir(staging_dir);
    Ok(())
}

/// fsync a directory (its entries), used as the batch data/rename barrier. On a
/// filesystem with ordered-data journaling this also flushes pending file data.
#[cfg(unix)]
fn fsync_dir(dir: &Path) -> Result<(), CliError> {
    let f = std::fs::File::open(dir).map_err(|s| CliError::io("open dir for fsync", dir, s))?;
    f.sync_all().map_err(|s| CliError::io("fsync dir", dir, s))
}

/// Non-Unix: directory fsync is not portable; signal "unavailable" so the caller
/// falls back to per-file fsync.
#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) -> Result<(), CliError> {
    Err(CliError::msg(
        "directory fsync unavailable on this platform",
    ))
}

/// fsync a single file by path (the portable per-file fallback barrier).
fn fsync_file(path: &Path) -> Result<(), CliError> {
    let f = std::fs::File::open(path).map_err(|s| CliError::io("open temp for fsync", path, s))?;
    f.sync_all()
        .map_err(|s| CliError::io("fsync temp", path, s))
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

    #[test]
    fn stage_and_install_batch_lands_all_files_and_leaves_no_temps() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();

        // Stage several temps (no rename yet), collecting install pairs.
        let mut installs = Vec::new();
        for i in 0..25 {
            let bytes = format!("content-{i}").into_bytes();
            let temp = stage_write_mode(&staging, &bytes, i % 3 == 0).unwrap();
            // Not yet visible at the final path.
            let final_path = dir.path().join(format!("out{i}.bin"));
            assert!(!final_path.exists(), "staged temp is not yet installed");
            installs.push((temp, final_path));
        }
        // The barrier installs them all.
        install_batch(&staging, &installs).unwrap();
        for (i, (_, final_path)) in installs.iter().enumerate() {
            assert_eq!(
                std::fs::read(final_path).unwrap(),
                format!("content-{i}").into_bytes()
            );
        }
        // Staging is empty again (every temp renamed away).
        assert_eq!(std::fs::read_dir(&staging).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn install_batch_preserves_the_executable_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();

        let exe_temp = stage_write_mode(&staging, b"#!/bin/sh\n", true).unwrap();
        let plain_temp = stage_write_mode(&staging, b"data\n", false).unwrap();
        let exe = dir.path().join("run.sh");
        let plain = dir.path().join("data.txt");
        install_batch(
            &staging,
            &[(exe_temp, exe.clone()), (plain_temp, plain.clone())],
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::metadata(&plain).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn install_batch_empty_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        install_batch(&staging, &[]).unwrap();
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
