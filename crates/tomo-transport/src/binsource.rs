//! Where the bytes to push to a remote come from (docs/SPEC.md §3, cross-release
//! skill).
//!
//! At M2 the only binary we can serve is the one we are *running* — read from
//! `current_exe()`. M6 will embed release binaries for every supported triple
//! (`include_bytes!`) and this module grows a lookup; the seam is
//! [`plan_binary`], a pure decision kept separate from the `current_exe` read so
//! it can be unit-tested.
//!
//! ## Dev-mode substitution (loud, debug-only allowance)
//!
//! Our development builds on this Linux VM are `x86_64-unknown-linux-gnu`, but a
//! Linux remote is detected as `x86_64-unknown-linux-musl` (SPEC §3 mandates
//! musl for release). To let localhost end-to-end scenarios exercise the real
//! bootstrap **before** M6 embedding exists, a *non-release* build is allowed to
//! satisfy a musl request with its own gnu binary **when the architecture and OS
//! match**. This is a scaffolding convenience only: [`plan_binary`] refuses the
//! substitution in release builds (`dev_build == false`), where the mapping is
//! strict and a non-embedded triple is [`TransportError::UnsupportedTarget`].
//! The CLI prints a conspicuous warning whenever [`BinarySource::dev_substitution`]
//! is set.

use crate::error::TransportError;
use crate::triple;

/// A resolved binary ready to push: which triple it is *for* and its bytes.
#[derive(Debug, Clone)]
pub struct BinarySource {
    /// The remote triple this binary will be named for.
    pub triple: String,
    /// The binary's bytes (from `current_exe` at M2).
    pub bytes: Vec<u8>,
    /// Whether this is the debug-only cross-runtime substitution (gnu binary
    /// serving a musl request); the CLI warns loudly when true.
    pub dev_substitution: bool,
}

/// The pure decision behind [`binary_for_triple`]: given the remote's requested
/// triple, the triple we were built for, and whether this is a dev build, may
/// we push our own `current_exe`, and is it a substitution?
///
/// # Errors
/// [`TransportError::UnsupportedTarget`] when we have no bytes for `requested`.
pub fn plan_binary(
    requested: &str,
    built_for: &str,
    dev_build: bool,
) -> Result<BinaryPlan, TransportError> {
    if requested == built_for {
        return Ok(BinaryPlan {
            dev_substitution: false,
        });
    }
    // Dev-only allowance: same arch+OS, different C runtime (gnu vs musl).
    if dev_build && triple::arch_os(requested) == triple::arch_os(built_for) {
        return Ok(BinaryPlan {
            dev_substitution: true,
        });
    }
    Err(TransportError::UnsupportedTarget {
        detected: requested.to_owned(),
        supported: triple::supported_list(),
    })
}

/// The outcome of [`plan_binary`]: we can serve `current_exe`, possibly as a
/// dev substitution. (M6 will add an `Embedded(triple)` variant.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BinaryPlan {
    /// Whether this is the debug-only gnu-for-musl substitution.
    pub dev_substitution: bool,
}

/// Resolve the bytes to push for `requested`, using `built_for` (the triple this
/// binary was compiled for; supplied by the CLI from its `build.rs`) and
/// `dev_build` (`true` in non-release builds).
///
/// # Errors
/// [`TransportError::UnsupportedTarget`] if we have no matching binary, or
/// [`TransportError::LocalBinary`] if `current_exe` cannot be read.
pub fn binary_for_triple(
    requested: &str,
    built_for: &str,
    dev_build: bool,
) -> Result<BinarySource, TransportError> {
    let plan = plan_binary(requested, built_for, dev_build)?;
    let exe = std::env::current_exe().map_err(|e| TransportError::LocalBinary {
        reason: format!("cannot locate current executable: {e}"),
    })?;
    let bytes = std::fs::read(&exe).map_err(|e| TransportError::LocalBinary {
        reason: format!("cannot read {}: {e}", exe.display()),
    })?;
    Ok(BinarySource {
        triple: requested.to_owned(),
        bytes,
        dev_substitution: plan.dev_substitution,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_no_substitution() {
        let plan =
            plan_binary(triple::X86_64_LINUX_MUSL, triple::X86_64_LINUX_MUSL, false).unwrap();
        assert!(!plan.dev_substitution);
    }

    #[test]
    fn dev_gnu_serves_musl() {
        let plan =
            plan_binary(triple::X86_64_LINUX_MUSL, "x86_64-unknown-linux-gnu", true).unwrap();
        assert!(plan.dev_substitution);
    }

    #[test]
    fn release_gnu_refuses_musl() {
        let err =
            plan_binary(triple::X86_64_LINUX_MUSL, "x86_64-unknown-linux-gnu", false).unwrap_err();
        assert!(matches!(err, TransportError::UnsupportedTarget { .. }));
    }

    #[test]
    fn different_arch_always_refused() {
        assert!(plan_binary(triple::AARCH64_LINUX_MUSL, "x86_64-unknown-linux-gnu", true).is_err());
    }

    #[test]
    fn different_os_refused() {
        assert!(plan_binary(triple::X86_64_DARWIN, "x86_64-unknown-linux-gnu", true).is_err());
    }
}
