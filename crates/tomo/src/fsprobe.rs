//! Runtime filesystem-semantics probe (macOS↔Linux filename hazards).
//!
//! The flagship pairing is macOS (APFS — case-insensitive by default, and a
//! *normalizing* filesystem that stores/returns NFD names) ↔ Linux
//! (case-sensitive, byte-preserving). Two behaviors of the *local* filesystem
//! change how Tomo must treat path names, and neither is knowable at compile
//! time (an external/exFAT/SMB volume on Linux can be case-insensitive; a
//! case-sensitive APFS volume exists too). So we probe the actual behavior of
//! the filesystem holding the project once, at session startup:
//!
//! - **`case_insensitive`** — do `Foo` and `foo` name the same file? Drives the
//!   case-collision ingress guard ([`crate::fsguard`]): on such a filesystem an
//!   incoming `foo.txt` would silently overwrite an existing `Foo.txt`.
//! - **`normalizes_unicode`** — does the filesystem return a name in a different
//!   Unicode normalization form than it was written in (APFS: NFC in, NFD out)?
//!   Drives NFC canonicalization of every local-FS-derived path
//!   ([`tomo_watch::norm`]), which prevents NFC/NFD duplicate-file ping-pong.
//!
//! # Structure (purity)
//! The *interpretation* of probe observations is pure and exhaustively
//! unit-tested ([`interpret_case`], [`interpret_norm`]); the [`probe`] shim is a
//! thin I/O wrapper (create probe files, `readdir`, clean up) that feeds those
//! functions. All probe files live under `.tomo/state/` (invariant #2) and are
//! removed immediately, so they never sync, never persist, and never race a
//! second session (the single-session lock is already held).

use std::path::Path;

use serde::{Deserialize, Serialize};

/// What the local filesystem does with names — probed once at session startup.
///
/// Additive and backward compatible in `status.json`: older files lacking the
/// `fs` block deserialize to `None`. Defaults to the safe, byte-preserving,
/// case-sensitive assumption (plain Linux), under which both guards are inert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FsSemantics {
    /// `Foo` and `foo` resolve to the same file (APFS default, exFAT, NTFS).
    pub case_insensitive: bool,
    /// The filesystem returns names in a different normalization form than
    /// written (APFS: write NFC, `readdir` returns NFD).
    pub normalizes_unicode: bool,
}

/// The prefix shared by every probe file, so a `readdir` can pick ours out and
/// a stale one from a crashed run can be swept before we probe.
const PROBE_PREFIX: &str = ".tomo-fsprobe-";

/// Interpret the case probe: `case_insensitive` is exactly whether a lookup of
/// the lower-cased probe name found the mixed-case file we created.
#[must_use]
pub fn interpret_case(lowercase_lookup_exists: bool) -> bool {
    lowercase_lookup_exists
}

/// Interpret the normalization probe from the name we *wrote* and the directory
/// entries observed afterward.
///
/// - If an entry byte-equals `written`, the filesystem handed our bytes back
///   unchanged → **byte-preserving** (`false`).
/// - Otherwise, if some entry is NFC-equal to `written` (i.e. the same visual
///   name in a different form, as APFS's NFD), the filesystem **normalizes**
///   (`true`).
/// - No matching entry at all (probe inconclusive) → `false` (safe default).
#[must_use]
pub fn interpret_norm(written: &str, entries: &[String]) -> bool {
    if entries.iter().any(|e| e == written) {
        return false;
    }
    let written_nfc = tomo_watch::to_nfc(written);
    entries.iter().any(|e| tomo_watch::to_nfc(e) == written_nfc)
}

/// Probe the filesystem holding `state_dir` (normally `.tomo/state/`).
///
/// Any I/O failure degrades gracefully to the corresponding safe default
/// (`false`) rather than erroring — a probe is best-effort telemetry that tunes
/// two guards, never a hard prerequisite for syncing.
#[must_use]
pub fn probe(state_dir: &Path) -> FsSemantics {
    // Debug-only test hook: force a filesystem's semantics without needing the
    // real hardware (APFS is unavailable on the Linux CI/dev VM). Compiled out
    // of release builds entirely. Mirrors the other TOMO_TEST_FORCE_* hooks.
    #[cfg(debug_assertions)]
    if let Some(forced) = std::env::var_os("TOMO_TEST_FORCE_FS") {
        return parse_forced(&forced.to_string_lossy());
    }

    // Sweep any probe files left by a crashed prior run before observing.
    sweep_probes(state_dir);
    let semantics = FsSemantics {
        case_insensitive: probe_case(state_dir).unwrap_or(false),
        normalizes_unicode: probe_norm(state_dir).unwrap_or(false),
    };
    sweep_probes(state_dir);
    semantics
}

/// Parse the debug `TOMO_TEST_FORCE_FS` value into semantics. Recognizes the
/// tokens `case-insensitive` and `normalizing` (also `normalize`/`nfd`), joined
/// by `+`, `,`, or whitespace; `case-sensitive`/`sensitive`/empty means the
/// plain byte-preserving default. Unknown tokens are ignored.
#[cfg(debug_assertions)]
fn parse_forced(value: &str) -> FsSemantics {
    let mut fs = FsSemantics::default();
    for token in value.split(['+', ',', ' ', '\t']) {
        match token.trim().to_ascii_lowercase().as_str() {
            "case-insensitive" | "insensitive" | "ci" | "caseinsensitive" => {
                fs.case_insensitive = true;
            }
            "normalizing" | "normalize" | "nfd" | "unicode" => {
                fs.normalizes_unicode = true;
            }
            // "case-sensitive"/"sensitive"/""/unknown → leave the default.
            _ => {}
        }
    }
    fs
}

/// The case probe: create a mixed-case file and check whether a lower-cased
/// lookup finds it. `None` on any I/O error.
fn probe_case(dir: &Path) -> Option<bool> {
    let created = dir.join(format!("{PROBE_PREFIX}CaseA"));
    let lookup = dir.join(format!("{PROBE_PREFIX}casea"));
    std::fs::write(&created, b"").ok()?;
    // `try_exists` distinguishes "does not exist" (Ok(false)) from an I/O error.
    let exists = std::fs::symlink_metadata(&lookup).is_ok();
    let _ = std::fs::remove_file(&created);
    Some(interpret_case(exists))
}

/// The normalization probe: write an NFC name, then read the directory back and
/// see whether the same name returns in a different form. `None` on I/O error.
fn probe_norm(dir: &Path) -> Option<bool> {
    // "é" precomposed (NFC). This detects an FS that NORMALIZES on store (old
    // HFS+ folded NFC→NFD, so readdir returned a different form). NOTE: modern
    // APFS does NOT normalize — it preserves the exact bytes and is instead
    // normalization-*insensitive* on lookup (NFC and NFD address the same file,
    // like case). This probe therefore reports `false` on APFS, which is honest:
    // there is no stored-byte change to undo. (Validated on real APFS — see
    // docs/NOTES.md. The insensitivity collision is a separate concern, tracked
    // there, not something this store-normalization probe measures.)
    let written = format!("{PROBE_PREFIX}caf\u{e9}");
    let created = dir.join(&written);
    std::fs::write(&created, b"").ok()?;

    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir).ok()? {
        let Ok(entry) = entry else { continue };
        if let Some(name) = entry.file_name().to_str() {
            if name.starts_with(PROBE_PREFIX)
                && !name.ends_with("CaseA")
                && !name.ends_with("casea")
            {
                entries.push(name.to_owned());
            }
        }
    }
    // Remove by the on-disk entry names we actually observed (on a normalizing
    // FS `created`'s NFC path still resolves, but deleting observed names is
    // unambiguous); also try the written path as a fallback.
    for name in &entries {
        let _ = std::fs::remove_file(dir.join(name));
    }
    let _ = std::fs::remove_file(&created);
    Some(interpret_norm(&written, &entries))
}

/// Best-effort removal of any probe files in `dir`.
fn sweep_probes(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            if name.starts_with(PROBE_PREFIX) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const NFC: &str = ".tomo-fsprobe-caf\u{e9}"; // precomposed é
    const NFD: &str = ".tomo-fsprobe-cafe\u{301}"; // decomposed é

    #[test]
    fn case_interpretation_is_the_lookup_result() {
        assert!(interpret_case(true));
        assert!(!interpret_case(false));
    }

    #[test]
    fn norm_byte_preserving_when_exact_name_returns() {
        // The exact NFC bytes came back → not normalizing.
        assert!(!interpret_norm(NFC, &[NFC.to_owned()]));
    }

    #[test]
    fn norm_true_when_only_nfd_variant_returns() {
        // Only the NFD form of our NFC name is present → normalizing.
        assert!(interpret_norm(NFC, &[NFD.to_owned()]));
    }

    #[test]
    fn norm_false_when_probe_inconclusive() {
        // Nothing matching came back at all → safe default.
        assert!(!interpret_norm(NFC, &[]));
        assert!(!interpret_norm(
            NFC,
            &[".tomo-fsprobe-unrelated".to_owned()]
        ));
    }

    #[test]
    fn norm_prefers_exact_over_nfd_if_both_present() {
        // If both the exact name and an NFD sibling exist, the exact match wins
        // (byte-preserving) — we only conclude "normalizing" when our own bytes
        // did NOT survive.
        assert!(!interpret_norm(NFC, &[NFC.to_owned(), NFD.to_owned()]));
    }

    #[test]
    fn default_is_byte_preserving_case_sensitive() {
        let fs = FsSemantics::default();
        assert!(!fs.case_insensitive);
        assert!(!fs.normalizes_unicode);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn forced_parsing_covers_all_combinations() {
        assert_eq!(parse_forced(""), FsSemantics::default());
        assert_eq!(parse_forced("case-sensitive"), FsSemantics::default());
        assert_eq!(
            parse_forced("case-insensitive"),
            FsSemantics {
                case_insensitive: true,
                normalizes_unicode: false
            }
        );
        assert_eq!(
            parse_forced("normalizing"),
            FsSemantics {
                case_insensitive: false,
                normalizes_unicode: true
            }
        );
        assert_eq!(
            parse_forced("case-insensitive+normalizing"),
            FsSemantics {
                case_insensitive: true,
                normalizes_unicode: true
            }
        );
        // Order/separators/casing are all tolerated.
        assert_eq!(
            parse_forced("NFD, CI"),
            FsSemantics {
                case_insensitive: true,
                normalizes_unicode: true
            }
        );
    }

    /// The live probe on THIS filesystem must report byte-preserving and leave no
    /// probe files behind. Byte-preservation is the one cross-platform invariant:
    /// Linux ext4/tmpfs preserve, and — validated on real hardware, see
    /// docs/NOTES.md — modern **APFS also preserves** (it stores your exact NFC
    /// bytes; it is normalization-*insensitive* on lookup but does NOT normalize
    /// on store the way HFS+ once did). Case-sensitivity is platform/volume
    /// dependent (Linux tmpfs is sensitive; default macOS APFS is insensitive),
    /// so it is only asserted where deterministic.
    #[test]
    fn live_probe_reports_byte_preserving_and_leaves_no_residue() {
        let dir = tempfile::tempdir().unwrap();
        let fs = probe_ignoring_env(dir.path());
        assert!(
            !fs.normalizes_unicode,
            "no supported filesystem normalizes on store (APFS preserves bytes)"
        );
        #[cfg(not(target_os = "macos"))]
        assert!(!fs.case_insensitive, "Linux ext4/tmpfs is case-sensitive");
        // No probe residue.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with(PROBE_PREFIX))
            })
            .collect();
        assert!(leftovers.is_empty(), "probe files must be cleaned up");
    }

    /// Run the real I/O probe path, bypassing the debug env hook (so the test is
    /// deterministic even when `TOMO_TEST_FORCE_FS` is set in the environment).
    fn probe_ignoring_env(dir: &Path) -> FsSemantics {
        sweep_probes(dir);
        let fs = FsSemantics {
            case_insensitive: probe_case(dir).unwrap_or(false),
            normalizes_unicode: probe_norm(dir).unwrap_or(false),
        };
        sweep_probes(dir);
        fs
    }
}
