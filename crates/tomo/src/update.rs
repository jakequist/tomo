//! `tomo update` (alias `upgrade`) — content-addressed self-update.
//!
//! Mirrors the shell installer (`site/install.sh`): it maps this build's target
//! triple to the same stable release asset name the installer downloads
//! (`tomo-linux-x86_64`, `tomo-macos-arm64`, …), fetches the release
//! `SHA256SUMS`, and compares the published hash of *our* asset against the
//! SHA-256 of the running executable. The **content hash is the decision** — we
//! never parse or compare version numbers to decide whether to update, so a
//! rebuilt-but-same-version release still updates and a same-bytes binary is a
//! no-op regardless of what the tag says.
//!
//! When an update is warranted the asset is downloaded next to the current
//! executable, its SHA-256 is verified against `SHA256SUMS` (a mismatch aborts
//! with **nothing replaced**), the exec bit is set, and it is atomically
//! `rename(2)`d over the current executable — the same staging + atomic-rename
//! discipline as [`crate::fsutil`], but staged in the binary's own directory
//! (its own filesystem) rather than `.tomo/` so the rename is atomic
//! (invariant #8).
//!
//! The download base defaults to the GitHub "latest release" download URL and is
//! overridable via the `TOMO_UPDATE_BASE` environment variable — the documented
//! test hook the `29_self_update.sh` scenario points at a localhost file server.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use crate::buildinfo;
use crate::error::CliError;
use crate::fsutil::random_hex;
use crate::out;

/// The default download base: GitHub's stable "latest release" asset URL, the
/// same one `site/install.sh` uses. Overridable via `TOMO_UPDATE_BASE`.
const DEFAULT_BASE: &str = "https://github.com/jakequist/tomo/releases/latest/download";

/// The environment variable that overrides [`DEFAULT_BASE`] (documented test
/// hook; the scenario points it at a localhost server).
const BASE_ENV: &str = "TOMO_UPDATE_BASE";

/// Map a Rust target triple to the stable release asset name, using the *same*
/// os/arch tags as `site/install.sh` (`tomo-<os>-<arch>`).
///
/// The mapping is intentionally tolerant of the C-runtime env field so a dev
/// `…-linux-gnu` build resolves to the same asset as the released
/// `…-linux-musl` binary (they are the same platform to a downloader).
///
/// # Errors
/// [`CliError::Message`] naming the triple if it maps to no released platform.
fn asset_for_triple(triple: &str) -> Result<String, CliError> {
    // Field 0 of `<arch>-<vendor>-<os>[-<env>]` is the architecture.
    let arch = triple.split('-').next().unwrap_or("");
    let arch_tag = match arch {
        "x86_64" => "x86_64",
        "aarch64" | "arm64" => "arm64",
        _ => return Err(unsupported_platform(triple)),
    };
    let os_tag = if triple.contains("linux") {
        "linux"
    } else if triple.contains("darwin") || triple.contains("apple") {
        "macos"
    } else {
        return Err(unsupported_platform(triple));
    };
    Ok(format!("tomo-{os_tag}-{arch_tag}"))
}

/// The clear "this platform has no release" error.
fn unsupported_platform(triple: &str) -> CliError {
    CliError::msg(format!(
        "self-update is not available for this platform ({triple}); \
         supported: linux/macos on x86_64/arm64"
    ))
}

/// Find the published SHA-256 for `asset` in a `SHA256SUMS` body.
///
/// Parses the exact format `sha256sum` emits and `install.sh` consumes —
/// `<hex><spaces>[*]<name>` lines — tolerating one or two spaces and the
/// binary-mode `*` marker. Returns the hash of the first matching line.
fn find_published_hash<'a>(sums: &'a str, asset: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let mut it = line.split_whitespace();
        let hash = it.next()?;
        let name = it.next()?.trim_start_matches('*');
        (name == asset).then_some(hash)
    })
}

/// The content-hash comparison outcome. This — not any version string — is what
/// decides whether `tomo update` does anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    /// The running binary's bytes already match the published asset.
    UpToDate,
    /// The published asset differs from the running binary.
    UpdateAvailable,
}

/// Decide from the current and published hashes (case-insensitive hex compare).
fn decide(current_hash: &str, published_hash: &str) -> Decision {
    if current_hash.eq_ignore_ascii_case(published_hash) {
        Decision::UpToDate
    } else {
        Decision::UpdateAvailable
    }
}

/// The concrete action to take, a pure function of the [`Decision`] and whether
/// `--check` was passed. Kept separate from the I/O so it is unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlannedAction {
    /// Nothing to do; already current (both `--check` and a full run).
    UpToDate,
    /// An update exists but `--check` was passed: report it, download nothing.
    ReportAvailable,
    /// An update exists and this is a full run: download and replace.
    Replace,
}

/// The update plan: `(decision, check_only) → action`.
fn plan(decision: Decision, check_only: bool) -> PlannedAction {
    match (decision, check_only) {
        (Decision::UpToDate, _) => PlannedAction::UpToDate,
        (Decision::UpdateAvailable, true) => PlannedAction::ReportAvailable,
        (Decision::UpdateAvailable, false) => PlannedAction::Replace,
    }
}

/// The effective download base: `TOMO_UPDATE_BASE` if set (test hook), else
/// [`DEFAULT_BASE`], with any trailing slash trimmed for clean URL joins.
fn update_base() -> String {
    let base = std::env::var(BASE_ENV).unwrap_or_else(|_| DEFAULT_BASE.to_owned());
    base.trim_end_matches('/').to_owned()
}

/// The first 12 hex chars of a hash, for human-facing short display.
fn short(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

/// Run `tomo update [--check]` (`buildinfo::BUILD_TARGET` selects the asset).
///
/// # Errors
/// [`CliError`] on an unsupported platform, a network/HTTP failure, a missing
/// checksum entry, a checksum mismatch (nothing is replaced), or an inability to
/// write over the current executable.
pub fn run(check_only: bool) -> Result<(), CliError> {
    let asset = asset_for_triple(buildinfo::BUILD_TARGET)?;
    let base = update_base();

    let sums_url = format!("{base}/SHA256SUMS");
    let sums = http_get_string(&sums_url)?;
    let published = find_published_hash(&sums, &asset).ok_or_else(|| {
        CliError::msg(format!(
            "the release checksums at {sums_url} have no entry for {asset}"
        ))
    })?;

    let exe = current_exe()?;
    let current_hash = hash_file(&exe)?;
    let version = env!("CARGO_PKG_VERSION");

    match plan(decide(&current_hash, published), check_only) {
        PlannedAction::UpToDate => {
            out::outln!("already up to date (tomo {version})");
            Ok(())
        }
        PlannedAction::ReportAvailable => {
            out::outln!("update available");
            out::outln!("  current  {}", short(&current_hash));
            out::outln!("  latest   {}", short(published));
            if let Some(tag) = latest_tag(&base) {
                out::outln!("  tag      {tag}");
            }
            out::outln!("run `tomo update` to install it");
            Ok(())
        }
        PlannedAction::Replace => {
            let asset_url = format!("{base}/{asset}");
            replace_self(&asset_url, published, &exe)?;
            report_updated(&exe, version);
            Ok(())
        }
    }
}

/// The current executable, with symlinks resolved so we always replace the real
/// target file (like installers do), not a symlink pointing at it.
///
/// On Linux/macOS `current_exe` already resolves symlinks (it reads
/// `/proc/self/exe`), but we `canonicalize` defensively so the contract holds
/// everywhere and the atomic rename lands on the actual binary inode.
fn current_exe() -> Result<PathBuf, CliError> {
    let p = std::env::current_exe()
        .map_err(|e| CliError::msg(format!("cannot locate the running executable: {e}")))?;
    p.canonicalize()
        .map_err(|e| CliError::io("resolve the running executable", &p, e))
}

/// SHA-256 of a file's bytes, as lowercase hex.
fn hash_file(path: &Path) -> Result<String, CliError> {
    let mut file =
        std::fs::File::open(path).map_err(|e| CliError::io("open for hashing", path, e))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(|e| CliError::io("read for hashing", path, e))?;
    Ok(hex(&hasher.finalize()))
}

/// Lowercase-hex encode a byte slice.
fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        // Writing to a String never fails.
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Download `asset_url`, verify its SHA-256 against `expected`, and atomically
/// replace `exe` — staging in `exe`'s own directory so the rename is atomic.
///
/// On any failure the partial staging file is removed, so an interrupted or
/// mismatched update never leaves debris next to the binary and never replaces
/// it with unverified bytes.
fn replace_self(asset_url: &str, expected: &str, exe: &Path) -> Result<(), CliError> {
    let dir = exe.parent().ok_or_else(|| {
        CliError::msg(format!(
            "the running executable {} has no parent directory to stage into",
            exe.display()
        ))
    })?;
    let temp = dir.join(format!(".tomo-update-{}.tmp", random_hex()?));

    let result = download_verify_swap(asset_url, expected, exe, dir, &temp);
    if result.is_err() {
        // Best-effort cleanup; the original error is what we return.
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// The staged replace body: download → hash → verify → chmod+x → atomic rename.
fn download_verify_swap(
    asset_url: &str,
    expected: &str,
    exe: &Path,
    dir: &Path,
    temp: &Path,
) -> Result<(), CliError> {
    // Create the staging file first so an unwritable directory fails *before*
    // any download, with a message that names the binary and the fix.
    let mut file = std::fs::File::create(temp).map_err(|e| unwritable(exe, dir, &e))?;

    let got = stream_to_file(asset_url, &mut file)?;
    if !got.eq_ignore_ascii_case(expected) {
        return Err(CliError::msg(format!(
            "checksum mismatch for the downloaded update \
             (expected {expected}, got {got}) — aborting; nothing was replaced"
        )));
    }
    set_exec(&file, temp)?;
    file.sync_all()
        .map_err(|e| CliError::io("flush the staged update", temp, e))?;
    drop(file);

    std::fs::rename(temp, exe).map_err(|e| replace_failed(exe, &e))
}

/// GET a small text resource (the `SHA256SUMS` manifest) as a `String`.
///
/// # Errors
/// [`CliError::Message`] on a connection, HTTP-status, or read failure — an
/// unreachable base or a 404 both surface here, before any file is staged.
fn http_get_string(url: &str) -> Result<String, CliError> {
    ureq::get(url)
        .call()
        .map_err(|e| CliError::msg(format!("could not fetch {url}: {e}")))?
        .body_mut()
        .read_to_string()
        .map_err(|e| CliError::msg(format!("could not read {url}: {e}")))
}

/// Stream an HTTP body into `file`, returning the SHA-256 of the bytes written.
fn stream_to_file(url: &str, file: &mut std::fs::File) -> Result<String, CliError> {
    let mut resp = ureq::get(url)
        .call()
        .map_err(|e| CliError::msg(format!("could not download {url}: {e}")))?;
    let mut reader = resp.body_mut().as_reader();
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| CliError::msg(format!("error downloading {url}: {e}")))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .map_err(|e| CliError::msg(format!("error writing the downloaded update: {e}")))?;
    }
    Ok(hex(&hasher.finalize()))
}

/// Set the executable bit (`0o755`) on the staged file (Unix only).
#[cfg(unix)]
fn set_exec(file: &std::fs::File, temp: &Path) -> Result<(), CliError> {
    use std::os::unix::fs::PermissionsExt as _;
    file.set_permissions(std::fs::Permissions::from_mode(0o755))
        .map_err(|e| CliError::io("set the update's executable bit", temp, e))
}

/// Non-Unix stub: there is no executable bit to set.
#[cfg(not(unix))]
fn set_exec(_file: &std::fs::File, _temp: &Path) -> Result<(), CliError> {
    Ok(())
}

/// The "cannot create a staging file next to the binary" error, naming the path
/// and pointing at the installer as the privileged-reinstall fallback.
fn unwritable(exe: &Path, dir: &Path, source: &std::io::Error) -> CliError {
    CliError::msg(format!(
        "cannot write to {} to install the update ({source}); the binary at {} \
         may be owned by another user — reinstall with the installer: \
         curl -fsSL https://tomo-sync.dev/install.sh | sh",
        dir.display(),
        exe.display()
    ))
}

/// The "download verified but the final rename failed" error.
fn replace_failed(exe: &Path, source: &std::io::Error) -> CliError {
    CliError::msg(format!(
        "the update downloaded and verified but could not replace {} ({source}); \
         reinstall with the installer: curl -fsSL https://tomo-sync.dev/install.sh | sh",
        exe.display()
    ))
}

/// Print the "updated" summary plus the always-on restart/peer reminder.
///
/// The new version is the single source of truth: the just-installed binary run
/// with `--version`. Best-effort — if that fails (e.g. a benign asset that no
/// longer runs) we still confirm the replacement happened.
fn report_updated(exe: &Path, old_version: &str) {
    match new_version(exe) {
        Some(new) => out::outln!("updated tomo {old_version} -> {new}"),
        None => out::outln!("updated tomo (restart running sessions to use it)"),
    }
    // The re-push-on-version-skew claim is verified against
    // `tomo_transport::bootstrap::decide`: a remote lacking the exact
    // `tomo-<version>-<triple>` binary is re-pushed at the next connect.
    out::outln!(
        "note: running sessions keep the old version until restarted; \
         the remote peer auto-updates at the next connect"
    );
}

/// Run the just-installed binary with `--version` and extract its version token.
/// Best-effort: any failure yields `None`.
fn new_version(exe: &Path) -> Option<String> {
    let output = std::process::Command::new(exe)
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    // `clap` prints "tomo <version>"; take the last whitespace-separated token.
    text.split_whitespace().last().map(str::to_owned)
}

/// Best-effort latest-tag display for `--check`: follow the `…/releases/latest`
/// redirect and read the tag off the resolved URL. Trivially cheap with `ureq`
/// (it follows redirects and exposes the final URI); any hiccup yields `None`,
/// and tag display is simply skipped.
fn latest_tag(base: &str) -> Option<String> {
    // The default base ends in `…/releases/latest/download`; the tag lives one
    // level up at `…/releases/latest`, which GitHub redirects to the tag page.
    use ureq::ResponseExt as _;
    let latest_url = base.strip_suffix("/download")?;
    let resp = ureq::get(latest_url).call().ok()?;
    let uri = resp.get_uri().to_string();
    let seg = uri.trim_end_matches('/').rsplit('/').next()?;
    // Only accept something that looks like a version tag (`v1.2.3`).
    let looks_like_tag =
        seg.starts_with('v') && seg[1..].chars().next().is_some_and(|c| c.is_ascii_digit());
    looks_like_tag.then(|| seg.to_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn asset_mapping_covers_every_released_platform() {
        for (triple, asset) in [
            ("x86_64-unknown-linux-musl", "tomo-linux-x86_64"),
            ("x86_64-unknown-linux-gnu", "tomo-linux-x86_64"),
            ("aarch64-unknown-linux-musl", "tomo-linux-arm64"),
            ("aarch64-unknown-linux-gnu", "tomo-linux-arm64"),
            ("x86_64-apple-darwin", "tomo-macos-x86_64"),
            ("aarch64-apple-darwin", "tomo-macos-arm64"),
        ] {
            assert_eq!(asset_for_triple(triple).unwrap(), asset, "triple {triple}");
        }
    }

    #[test]
    fn asset_mapping_rejects_unsupported_platforms() {
        for triple in [
            "riscv64gc-unknown-linux-gnu",
            "x86_64-pc-windows-msvc",
            "powerpc64-unknown-linux-gnu",
            "wasm32-unknown-unknown",
        ] {
            let err = asset_for_triple(triple).unwrap_err();
            let msg = format!("{err}");
            assert!(msg.contains(triple), "error should name the triple: {msg}");
            assert!(msg.contains("not available"), "error should explain: {msg}");
        }
    }

    #[test]
    fn sha256sums_parses_two_space_form() {
        // The exact `sha256sum` text-mode output install.sh consumes.
        let sums = "\
aaaa1111  tomo-linux-x86_64
bbbb2222  tomo-linux-arm64
cccc3333  SHA256SUMS
dddd4444  tomo-macos-arm64
";
        assert_eq!(
            find_published_hash(sums, "tomo-linux-x86_64"),
            Some("aaaa1111")
        );
        assert_eq!(
            find_published_hash(sums, "tomo-macos-arm64"),
            Some("dddd4444")
        );
        assert_eq!(find_published_hash(sums, "tomo-macos-x86_64"), None);
    }

    #[test]
    fn sha256sums_tolerates_single_space_and_binary_marker() {
        // One space (hand-written) and a binary-mode `*name` marker.
        let sums = "abc123 tomo-linux-x86_64\ndef456 *tomo-linux-arm64\n";
        assert_eq!(
            find_published_hash(sums, "tomo-linux-x86_64"),
            Some("abc123")
        );
        assert_eq!(
            find_published_hash(sums, "tomo-linux-arm64"),
            Some("def456")
        );
    }

    #[test]
    fn sha256sums_ignores_blank_and_malformed_lines() {
        let sums = "\n   \ngarbage_without_a_second_field\nfeed  tomo-linux-arm64\n";
        assert_eq!(find_published_hash(sums, "tomo-linux-arm64"), Some("feed"));
    }

    #[test]
    fn decision_is_case_insensitive_hash_equality() {
        assert_eq!(decide("ABCDEF", "abcdef"), Decision::UpToDate);
        assert_eq!(decide("abcdef", "abcdef"), Decision::UpToDate);
        assert_eq!(decide("abcdef", "999999"), Decision::UpdateAvailable);
    }

    #[test]
    fn plan_maps_decision_and_check_flag() {
        // Up to date: never downloads, regardless of --check.
        assert_eq!(plan(Decision::UpToDate, false), PlannedAction::UpToDate);
        assert_eq!(plan(Decision::UpToDate, true), PlannedAction::UpToDate);
        // Update available: --check reports, a full run replaces.
        assert_eq!(
            plan(Decision::UpdateAvailable, true),
            PlannedAction::ReportAvailable
        );
        assert_eq!(
            plan(Decision::UpdateAvailable, false),
            PlannedAction::Replace
        );
    }

    #[test]
    fn short_hash_is_twelve_chars_and_safe_on_short_input() {
        assert_eq!(short("0123456789abcdef0123"), "0123456789ab");
        assert_eq!(short("abc"), "abc");
        assert_eq!(short(""), "");
    }

    #[test]
    fn hex_encodes_lowercase() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    }

    #[test]
    fn update_base_defaults_and_trims() {
        // Explicit override with a trailing slash is trimmed.
        std::env::set_var(BASE_ENV, "http://127.0.0.1:9/x/");
        assert_eq!(update_base(), "http://127.0.0.1:9/x");
        std::env::remove_var(BASE_ENV);
        assert_eq!(update_base(), DEFAULT_BASE);
    }
}
