//! Content-addressed history store and `SQLite` metadata (docs/SPEC.md §6).
//!
//! I/O crate. Invariants: all state under <root>/.tomo; crash-safe; never sacrifice sync latency for history writes.
//! See docs/SPEC.md and CLAUDE.md before implementing. Build test-first.

/// Placeholder so the workspace compiles; remove with the first real feature.
pub fn crate_name() -> &'static str {
    "tomo-history"
}

#[cfg(test)]
mod tests {
    #[test]
    fn scaffold_smoke() {
        assert_eq!(super::crate_name(), "tomo-history");
    }
}
