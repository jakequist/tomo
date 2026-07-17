//! SSH transport, remote binary bootstrap, and the blocking stdio tunnel
//! (docs/SPEC.md §2–3).
//!
//! This is an I/O adapter crate. It confines the async, tokio-based `russh`
//! stack behind a **blocking facade** so the rest of Tomo — whose sync session
//! loop owns the engine on the main thread — can drive an SSH-tunneled peer
//! through the same `Read`/`Write` shapes it already uses for the M1 local
//! transport. See [`ssh::SshSession`] for the entry point.
//!
//! Layering:
//! - [`hostspec`], [`triple`], [`quote`], [`binsource`], and the [`bootstrap`]
//!   decision are **pure** and exhaustively unit-tested (including hostile
//!   inputs).
//! - [`ssh`] performs the actual network I/O (connect, auth, host-key check,
//!   `exec`, SFTP push, remote spawn).
//!
//! SPEC invariants honored here: SFTP for the push (never shell out to `scp`);
//! exact-version match or re-push (no ranges); SHA-256 verification; an
//! unsupported remote triple is a clean error with **no external downloads**.

mod binsource;
mod bootstrap;
mod error;
mod hostspec;
mod quote;
mod ssh;
mod triple;

pub use binsource::{binary_for_triple, embedded_inventory, plan_binary, BinaryPlan, BinarySource};
pub use bootstrap::{
    binary_name, binary_rel_path, decide, BootstrapDecision, BootstrapReport, REMOTE_BIN_DIR,
};
pub use error::TransportError;
pub use hostspec::{HostSpec, DEFAULT_SSH_PORT};
pub use quote::{shell_line, shell_quote};
pub use ssh::{
    ChannelReader, ChannelWriter, ExecOutput, RemoteChannel, RemoteGuard, Sftp, SshOpts, SshSession,
};
pub use triple::{
    arch_os, parse_uname, supported_list, uname_to_triple, AARCH64_DARWIN, AARCH64_LINUX_MUSL,
    SUPPORTED, X86_64_DARWIN, X86_64_LINUX_MUSL,
};

/// The effective local version string, applying the debug-only
/// `TOMO_TEST_FORCE_LOCAL_VERSION` override.
///
/// `base` is normally the CLI's `CARGO_PKG_VERSION`. The bootstrap binary name
/// (`tomo-<version>-<triple>`) and the `Hello` handshake must use the *same*
/// value, so the CLI computes it once here and threads it through both.
///
/// # Debug-only test hook
/// In **non-release** builds only, if `TOMO_TEST_FORCE_LOCAL_VERSION` is set its
/// value replaces `base`. Scenario 04 uses this to force a version skew and
/// exercise the re-push / handshake-mismatch paths on localhost without an
/// actual version bump. It is **compiled out** of release builds, where the
/// version is always the real `CARGO_PKG_VERSION`.
pub fn effective_local_version(base: &str) -> String {
    #[cfg(debug_assertions)]
    if let Some(forced) = std::env::var_os("TOMO_TEST_FORCE_LOCAL_VERSION") {
        return forced.to_string_lossy().into_owned();
    }
    base.to_owned()
}
