//! Mapping `uname -s -m` output to a Rust target triple (docs/SPEC.md §3).
//!
//! Pure module. The mapping is deliberately exhaustive and conservative: an
//! unrecognized OS/arch is an error naming exactly what was detected, never a
//! guess and never a download.

use crate::error::TransportError;

/// Fully-static Linux (musl) on x86-64 — the workhorse server target.
pub const X86_64_LINUX_MUSL: &str = "x86_64-unknown-linux-musl";
/// Fully-static Linux (musl) on ARM64.
pub const AARCH64_LINUX_MUSL: &str = "aarch64-unknown-linux-musl";
/// Intel macOS.
pub const X86_64_DARWIN: &str = "x86_64-apple-darwin";
/// Apple-silicon macOS.
pub const AARCH64_DARWIN: &str = "aarch64-apple-darwin";

/// Every triple Tomo can serve, for error messages.
pub const SUPPORTED: &[&str] = &[
    X86_64_LINUX_MUSL,
    AARCH64_LINUX_MUSL,
    X86_64_DARWIN,
    AARCH64_DARWIN,
];

/// The supported triples as a comma-separated string.
pub fn supported_list() -> String {
    SUPPORTED.join(", ")
}

/// Map the raw output of `uname -s` and `uname -m` to a supported triple.
///
/// `os` is `uname -s` (e.g. `Linux`, `Darwin`); `arch` is `uname -m` (e.g.
/// `x86_64`, `aarch64`, `arm64`). Matching is case-insensitive and tolerant of
/// surrounding whitespace.
///
/// # Errors
/// [`TransportError::UnsupportedTarget`] if the pair maps to no release triple.
pub fn uname_to_triple(os: &str, arch: &str) -> Result<&'static str, TransportError> {
    let os_l = os.trim().to_ascii_lowercase();
    let arch_l = arch.trim().to_ascii_lowercase();
    let triple = match (os_l.as_str(), arch_l.as_str()) {
        ("linux", "x86_64" | "amd64") => X86_64_LINUX_MUSL,
        ("linux", "aarch64" | "arm64") => AARCH64_LINUX_MUSL,
        ("darwin", "x86_64" | "amd64") => X86_64_DARWIN,
        ("darwin", "aarch64" | "arm64") => AARCH64_DARWIN,
        _ => {
            return Err(TransportError::UnsupportedTarget {
                detected: format!("{} {}", os.trim(), arch.trim()),
                supported: supported_list(),
            });
        }
    };
    Ok(triple)
}

/// Parse `uname -s -m` combined stdout (`"Linux x86_64\n"`) into `(os, arch)`.
///
/// # Errors
/// [`TransportError::UnsupportedTarget`] if the output is not two whitespace-
/// separated tokens.
pub fn parse_uname(stdout: &str) -> Result<(String, String), TransportError> {
    let mut parts = stdout.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some(os), Some(arch)) => Ok((os.to_owned(), arch.to_owned())),
        _ => Err(TransportError::UnsupportedTarget {
            detected: stdout.trim().to_owned(),
            supported: supported_list(),
        }),
    }
}

/// The `(arch, os)` class of a triple, ignoring vendor and C-runtime env.
///
/// Used by the dev-mode binary substitution: a build for
/// `x86_64-unknown-linux-gnu` satisfies a request for
/// `x86_64-unknown-linux-musl` because arch and OS match. Returns `None` if the
/// triple is not one we recognize the shape of.
pub fn arch_os(triple: &str) -> Option<(&str, &str)> {
    // Triples are `<arch>-<vendor>-<os>[-<env>]`. We only need arch (field 0)
    // and os (field 2), collapsing linux-gnu/linux-musl to just "linux".
    let mut fields = triple.split('-');
    let arch = fields.next()?;
    let _vendor = fields.next()?;
    let os = fields.next()?;
    Some((arch, os))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn linux_x86_64() {
        assert_eq!(
            uname_to_triple("Linux", "x86_64").unwrap(),
            X86_64_LINUX_MUSL
        );
    }

    #[test]
    fn linux_arm() {
        assert_eq!(
            uname_to_triple("Linux", "aarch64").unwrap(),
            AARCH64_LINUX_MUSL
        );
        assert_eq!(
            uname_to_triple("Linux", "arm64").unwrap(),
            AARCH64_LINUX_MUSL
        );
    }

    #[test]
    fn darwin() {
        assert_eq!(uname_to_triple("Darwin", "x86_64").unwrap(), X86_64_DARWIN);
        assert_eq!(uname_to_triple("Darwin", "arm64").unwrap(), AARCH64_DARWIN);
    }

    #[test]
    fn case_and_whitespace_insensitive() {
        assert_eq!(
            uname_to_triple(" linux\n", " X86_64 ").unwrap(),
            X86_64_LINUX_MUSL
        );
    }

    #[test]
    fn unsupported_os() {
        let err = uname_to_triple("FreeBSD", "amd64").unwrap_err();
        match err {
            TransportError::UnsupportedTarget { detected, .. } => {
                assert!(detected.contains("FreeBSD"));
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn unsupported_arch() {
        assert!(uname_to_triple("Linux", "riscv64").is_err());
    }

    #[test]
    fn parse_uname_ok() {
        assert_eq!(
            parse_uname("Linux x86_64\n").unwrap(),
            ("Linux".to_owned(), "x86_64".to_owned())
        );
    }

    #[test]
    fn parse_uname_bad() {
        assert!(parse_uname("Linux").is_err());
        assert!(parse_uname("").is_err());
    }

    #[test]
    fn arch_os_collapses_env() {
        assert_eq!(
            arch_os("x86_64-unknown-linux-gnu"),
            Some(("x86_64", "linux"))
        );
        assert_eq!(
            arch_os("x86_64-unknown-linux-musl"),
            Some(("x86_64", "linux"))
        );
        assert_eq!(arch_os("aarch64-apple-darwin"), Some(("aarch64", "darwin")));
    }
}
