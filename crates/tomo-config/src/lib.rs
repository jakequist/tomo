//! Configuration: path classes (synced+versioned / synced+unversioned / ignored), direction rules, history.mode (docs/SPEC.md §7).
//!
//! Read-only I/O. The .tomo/** ignore is NOT expressible or removable via config.
//! See docs/SPEC.md and CLAUDE.md before implementing. Build test-first.

/// Placeholder so the workspace compiles; remove with the first real feature.
pub fn crate_name() -> &'static str {
    "tomo-config"
}

#[cfg(test)]
mod tests {
    #[test]
    fn scaffold_smoke() {
        assert_eq!(super::crate_name(), "tomo-config");
    }
}
