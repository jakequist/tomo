//! Unicode normalization (NFC) of filesystem-derived path strings.
//!
//! # Why this exists (macOS↔Linux filename semantics, Tier-1 edge case 3b)
//! APFS is a *normalizing* filesystem: a name written as NFC ("é" = U+00E9) is
//! stored and **returned by `readdir` as NFD** ("e" + U+0301). Linux is
//! byte-preserving. Without intervention, a Linux replica that ships an NFC
//! name and a Mac replica that scans it back as NFD see two *different*
//! [`RelPath`](tomo_engine::RelPath)s for the same visual file — an endless
//! duplicate-file ping-pong.
//!
//! The cure is to canonicalize every path string entering the system **from the
//! local filesystem** (the watcher's and the scanner's relativize step) to NFC,
//! but only when the local filesystem is itself the normalizer. That way an
//! APFS-`readdir`'d NFD name and the Linux NFC original collapse to the same
//! `RelPath` and the ping-pong is impossible, while a Linux user with
//! genuinely-NFD filenames keeps them byte-for-byte (we normalize only when the
//! FS would have normalized anyway — see [`canonicalize_fs_path`]).
//!
//! The wire protocol and the engine stay byte-faithful; normalization is applied
//! exactly once, at local-FS ingress, and is a pure function of `(name, flag)`.

use std::borrow::Cow;

use unicode_normalization::UnicodeNormalization;

/// Return `s` in Unicode Normalization Form C, borrowing unchanged when it is
/// already NFC (the common case — pure ASCII is always NFC).
#[must_use]
pub fn to_nfc(s: &str) -> Cow<'_, str> {
    if unicode_normalization::is_nfc(s) {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(s.nfc().collect())
    }
}

/// Canonicalize a filesystem-derived path string to NFC iff `normalize` is set.
///
/// `normalize` reflects the *local* filesystem's behavior (probed at session
/// startup): true only for a normalizing FS such as APFS. When false the string
/// is returned byte-faithful (borrowed), so a byte-preserving FS (Linux) keeps
/// genuinely-NFD names exactly as authored — Tomo never over-normalizes a
/// filesystem that would not have normalized itself.
///
/// ```
/// use tomo_watch::norm::canonicalize_fs_path;
/// // Pure ASCII is unaffected either way.
/// assert_eq!(canonicalize_fs_path("src/main.rs", true), "src/main.rs");
/// // An NFD "é" name: normalized to NFC only when the FS normalizes.
/// let nfd = "caf\u{65}\u{301}"; // "cafe" + combining acute
/// let nfc = "caf\u{e9}";        // "café" precomposed
/// assert_eq!(canonicalize_fs_path(nfd, true), nfc);
/// assert_eq!(canonicalize_fs_path(nfd, false), nfd);
/// ```
#[must_use]
pub fn canonicalize_fs_path(s: &str, normalize: bool) -> Cow<'_, str> {
    if normalize {
        to_nfc(s)
    } else {
        Cow::Borrowed(s)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // panics are fine in tests
mod tests {
    use super::*;

    // "é": precomposed (NFC, one scalar) vs decomposed (NFD, base + combining).
    const NFC_E_ACUTE: &str = "\u{e9}";
    const NFD_E_ACUTE: &str = "e\u{301}";

    #[test]
    fn ascii_is_unchanged_and_borrowed() {
        let out = to_nfc("plain/ascii_path-1.txt");
        assert_eq!(out, "plain/ascii_path-1.txt");
        assert!(matches!(out, Cow::Borrowed(_)), "ASCII must not allocate");
    }

    #[test]
    fn nfc_input_is_unchanged() {
        assert_eq!(to_nfc(NFC_E_ACUTE), NFC_E_ACUTE);
        assert!(matches!(to_nfc(NFC_E_ACUTE), Cow::Borrowed(_)));
    }

    #[test]
    fn nfd_input_becomes_nfc() {
        let out = to_nfc(NFD_E_ACUTE);
        assert_eq!(out, NFC_E_ACUTE);
        assert!(matches!(out, Cow::Owned(_)));
    }

    #[test]
    fn mixed_components_normalize_each_part() {
        // A path mixing an ASCII component, an NFD component, and an
        // already-NFC component all collapse to a single NFC string.
        let mixed = format!("dir/{NFD_E_ACUTE}/{NFC_E_ACUTE}.txt");
        let expected = format!("dir/{NFC_E_ACUTE}/{NFC_E_ACUTE}.txt");
        assert_eq!(to_nfc(&mixed), expected);
    }

    #[test]
    fn flag_gates_normalization() {
        // Enabled: NFD → NFC. Disabled: byte-faithful passthrough.
        assert_eq!(canonicalize_fs_path(NFD_E_ACUTE, true), NFC_E_ACUTE);
        assert_eq!(canonicalize_fs_path(NFD_E_ACUTE, false), NFD_E_ACUTE);
        // Enabled but already NFC: unchanged.
        assert_eq!(canonicalize_fs_path(NFC_E_ACUTE, true), NFC_E_ACUTE);
    }

    #[test]
    fn idempotent() {
        let once = to_nfc(NFD_E_ACUTE).into_owned();
        let twice = to_nfc(&once).into_owned();
        assert_eq!(once, twice);
    }
}
