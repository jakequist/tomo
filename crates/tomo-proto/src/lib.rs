//! Wire protocol: length-prefixed frames, handshake, chunk transfer (docs/SPEC.md §8).
//!
//! Pure crate (no I/O): types + (de)serialization only. Interleave to avoid head-of-line blocking.
//! See docs/SPEC.md and CLAUDE.md before implementing. Build test-first.

/// Placeholder so the workspace compiles; remove with the first real feature.
pub fn crate_name() -> &'static str {
    "tomo-proto"
}

#[cfg(test)]
mod tests {
    #[test]
    fn scaffold_smoke() {
        assert_eq!(super::crate_name(), "tomo-proto");
    }
}
