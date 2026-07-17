//! The remote binary bootstrap (docs/SPEC.md §3, normative).
//!
//! Given a connected [`SshSession`], detect the remote target, decide whether
//! the already-present binary can be reused, and if not push a fresh one over
//! SFTP with a SHA-256 integrity check and an atomic rename. The *decision* is a
//! pure function ([`decide`]) tested in isolation; the orchestration
//! ([`SshSession::bootstrap`]) performs the I/O.

use crate::binsource;
use crate::error::TransportError;

/// The remote-relative directory holding pushed binaries: `.tomo/bin`.
pub const REMOTE_BIN_DIR: &str = ".tomo/bin";

/// The naming scheme for a pushed binary: `tomo-<version>-<triple>`.
///
/// ```
/// use tomo_transport::binary_name;
/// assert_eq!(
///     binary_name("0.0.1", "x86_64-unknown-linux-musl"),
///     "tomo-0.0.1-x86_64-unknown-linux-musl"
/// );
/// ```
pub fn binary_name(version: &str, triple: &str) -> String {
    format!("tomo-{version}-{triple}")
}

/// The remote-relative path of a pushed binary: `.tomo/bin/tomo-<version>-<triple>`.
pub fn binary_rel_path(version: &str, triple: &str) -> String {
    format!("{REMOTE_BIN_DIR}/{}", binary_name(version, triple))
}

/// The pure bootstrap decision: given the file names already present in the
/// remote `.tomo/bin/` directory, the local version, and the detected triple,
/// decide whether to reuse or (re)push, and which stale siblings to remove.
///
/// Only files matching our `tomo-*-<triple>` family for the *same triple* are
/// considered stale candidates — a binary for a different triple (e.g. left by a
/// different client arch) is left untouched. An exact `tomo-<version>-<triple>`
/// match means reuse; anything else means push.
///
/// # Errors
/// This function is infallible; it returns a [`BootstrapDecision`].
pub fn decide(remote_entries: &[String], version: &str, triple: &str) -> BootstrapDecision {
    let wanted = binary_name(version, triple);
    let prefix = "tomo-";
    let suffix = format!("-{triple}");

    let present = remote_entries.iter().any(|e| e == &wanted);

    // Stale = any tomo-*-<triple> that is not exactly the wanted name.
    let stale: Vec<String> = remote_entries
        .iter()
        .filter(|e| e.starts_with(prefix) && e.ends_with(&suffix) && *e != &wanted)
        .cloned()
        .collect();

    if present {
        BootstrapDecision::Reuse {
            name: wanted,
            stale,
        }
    } else {
        BootstrapDecision::Push {
            name: wanted,
            stale,
        }
    }
}

/// What [`decide`] concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapDecision {
    /// The exact binary is present; reuse it (still prune `stale` siblings).
    Reuse {
        /// The binary file name to exec.
        name: String,
        /// Older `tomo-*-<triple>` files to remove for tidiness.
        stale: Vec<String>,
    },
    /// The binary is absent or a version mismatch; push it, then prune `stale`.
    Push {
        /// The binary file name to write.
        name: String,
        /// Older `tomo-*-<triple>` files to remove after the push.
        stale: Vec<String>,
    },
}

/// The result of a completed bootstrap, reported by the CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapReport {
    /// The exact binary was already present; nothing was transferred.
    Reused {
        /// The detected remote triple.
        triple: String,
        /// The version reused.
        version: String,
        /// The remote-relative path of the binary to exec.
        binary_rel: String,
    },
    /// A fresh binary was pushed and verified.
    Pushed {
        /// The detected remote triple.
        triple: String,
        /// The version pushed.
        version: String,
        /// The remote-relative path of the binary to exec.
        binary_rel: String,
        /// How many bytes were transferred.
        bytes: u64,
        /// Whether the pushed binary is the debug-only gnu-for-musl
        /// substitution (the CLI warns loudly when true).
        dev_substitution: bool,
    },
}

impl BootstrapReport {
    /// The remote-relative path of the binary to exec, regardless of variant.
    pub fn binary_rel(&self) -> &str {
        match self {
            BootstrapReport::Reused { binary_rel, .. }
            | BootstrapReport::Pushed { binary_rel, .. } => binary_rel,
        }
    }

    /// The detected remote triple.
    pub fn triple(&self) -> &str {
        match self {
            BootstrapReport::Reused { triple, .. } | BootstrapReport::Pushed { triple, .. } => {
                triple
            }
        }
    }
}

/// Resolve the bytes to push, applying the debug-only substitution rules.
///
/// # Errors
/// Propagates [`binsource::binary_for_triple`] failures.
pub(crate) fn resolve_source(
    detected_triple: &str,
    built_for: &str,
    dev_build: bool,
) -> Result<binsource::BinarySource, TransportError> {
    binsource::binary_for_triple(detected_triple, built_for, dev_build)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const T: &str = "x86_64-unknown-linux-musl";

    #[test]
    fn names() {
        assert_eq!(
            binary_name("0.0.1", T),
            "tomo-0.0.1-x86_64-unknown-linux-musl"
        );
        assert_eq!(
            binary_rel_path("0.0.1", T),
            ".tomo/bin/tomo-0.0.1-x86_64-unknown-linux-musl"
        );
    }

    #[test]
    fn empty_dir_pushes() {
        let d = decide(&[], "0.0.1", T);
        assert_eq!(
            d,
            BootstrapDecision::Push {
                name: binary_name("0.0.1", T),
                stale: vec![],
            }
        );
    }

    #[test]
    fn exact_match_reuses() {
        let entries = vec![binary_name("0.0.1", T)];
        let d = decide(&entries, "0.0.1", T);
        assert!(matches!(d, BootstrapDecision::Reuse { ref stale, .. } if stale.is_empty()));
    }

    #[test]
    fn version_mismatch_pushes_and_marks_old_stale() {
        let entries = vec![binary_name("0.0.0", T)];
        let d = decide(&entries, "0.0.1", T);
        match d {
            BootstrapDecision::Push { name, stale } => {
                assert_eq!(name, binary_name("0.0.1", T));
                assert_eq!(stale, vec![binary_name("0.0.0", T)]);
            }
            BootstrapDecision::Reuse { .. } => panic!("expected Push, got Reuse"),
        }
    }

    #[test]
    fn reuse_still_prunes_older_siblings() {
        let entries = vec![binary_name("0.0.1", T), binary_name("0.0.0", T)];
        let d = decide(&entries, "0.0.1", T);
        match d {
            BootstrapDecision::Reuse { name, stale } => {
                assert_eq!(name, binary_name("0.0.1", T));
                assert_eq!(stale, vec![binary_name("0.0.0", T)]);
            }
            BootstrapDecision::Push { .. } => panic!("expected Reuse, got Push"),
        }
    }

    #[test]
    fn other_triple_binaries_are_left_untouched() {
        let other = "aarch64-apple-darwin";
        let entries = vec![binary_name("0.0.0", other), binary_name("0.0.1", other)];
        let d = decide(&entries, "0.0.1", T);
        // We want a musl binary; the darwin ones are neither reused nor pruned.
        assert_eq!(
            d,
            BootstrapDecision::Push {
                name: binary_name("0.0.1", T),
                stale: vec![],
            }
        );
    }
}
