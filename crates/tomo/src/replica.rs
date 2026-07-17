//! The stable replica identity (docs/SPEC.md §5.2).
//!
//! Each replica gets a random 64-bit id at `tomo init`, persisted as 16 lowercase
//! hex characters in `.tomo/replica`. It seeds the vector clocks
//! ([`tomo_engine::ReplicaId`]); it is never derived from a hostname or wall
//! clock (invariant #7), so two independently-initialized replicas practically
//! never collide.

use std::path::Path;

use tomo_engine::ReplicaId;

use crate::error::CliError;

/// Generate a fresh random replica id from OS entropy.
///
/// # Errors
/// [`CliError::Message`] if the OS entropy source is unavailable.
pub fn generate() -> Result<ReplicaId, CliError> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes)
        .map_err(|e| CliError::msg(format!("could not read OS entropy for replica id: {e}")))?;
    Ok(ReplicaId(u64::from_le_bytes(bytes)))
}

/// Format a replica id as 16 lowercase hex digits.
pub fn format(id: ReplicaId) -> String {
    format!("{:016x}", id.0)
}

/// Parse a replica id from the hex text stored in `.tomo/replica`.
///
/// Trailing whitespace (a stray newline) is tolerated.
///
/// # Errors
/// [`CliError::Message`] if the text is not valid hex.
pub fn parse(text: &str) -> Result<ReplicaId, CliError> {
    let trimmed = text.trim();
    u64::from_str_radix(trimmed, 16)
        .map(ReplicaId)
        .map_err(|_| {
            CliError::msg(format!(
                "malformed replica id in .tomo/replica: {trimmed:?}"
            ))
        })
}

/// Read and parse the replica id from `path` (`.tomo/replica`).
///
/// # Errors
/// [`CliError::Io`] if the file cannot be read, or [`CliError::Message`] if its
/// contents are malformed.
pub fn load(path: &Path) -> Result<ReplicaId, CliError> {
    let text = std::fs::read_to_string(path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            CliError::msg(format!(
                "{} not found — run `tomo init` first",
                path.display()
            ))
        } else {
            CliError::io("read", path, source)
        }
    })?;
    parse(&text)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn format_is_16_hex_digits() {
        assert_eq!(format(ReplicaId(0)), "0000000000000000");
        assert_eq!(format(ReplicaId(0xdead_beef)), "00000000deadbeef");
        assert_eq!(format(ReplicaId(u64::MAX)), "ffffffffffffffff");
    }

    #[test]
    fn parse_round_trips_format() {
        for raw in [0u64, 1, 0x1234_5678_9abc_def0, u64::MAX] {
            let id = ReplicaId(raw);
            assert_eq!(parse(&format(id)).unwrap(), id);
        }
    }

    #[test]
    fn parse_tolerates_trailing_newline() {
        assert_eq!(parse("00000000deadbeef\n").unwrap(), ReplicaId(0xdead_beef));
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse("nothex").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn generate_is_nonzero_and_varies() {
        // Astronomically unlikely to be zero or equal across two draws; if this
        // ever flakes the entropy source is broken, which is the real bug.
        let a = generate().unwrap();
        let b = generate().unwrap();
        assert_ne!(a.0, b.0);
    }
}
