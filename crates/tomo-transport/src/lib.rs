//! SSH session management, remote binary bootstrap, stdio tunnel (docs/SPEC.md §2–3).
//!
//! I/O crate. SFTP for binary push (never shell out to scp); SHA-256 verify; exact-version match or re-push.
//! See docs/SPEC.md and CLAUDE.md before implementing. Build test-first.

/// Placeholder so the workspace compiles; remove with the first real feature.
pub fn crate_name() -> &'static str {
    "tomo-transport"
}

#[cfg(test)]
mod tests {
    #[test]
    fn scaffold_smoke() {
        assert_eq!(super::crate_name(), "tomo-transport");
    }
}
