//! FSEvents/inotify adapters, atomic-save canonicalization, echo suppression (docs/SPEC.md §5.1).
//!
//! I/O crate, kept thin: normalize raw events into canonical change records for tomo-engine. .tomo/** filtered here as a hardcoded constant.
//! See docs/SPEC.md and CLAUDE.md before implementing. Build test-first.

/// Placeholder so the workspace compiles; remove with the first real feature.
pub fn crate_name() -> &'static str {
    "tomo-watch"
}

#[cfg(test)]
mod tests {
    #[test]
    fn scaffold_smoke() {
        assert_eq!(super::crate_name(), "tomo-watch");
    }
}
