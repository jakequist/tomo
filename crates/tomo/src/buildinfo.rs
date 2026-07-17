//! Compile-time build facts the SSH transport and handshake depend on.
//!
//! Kept in one place so the version reported in the `Hello` handshake and the
//! version used to name the pushed binary (`tomo-<version>-<triple>`) are always
//! the *same* string (see [`binary_version`]).

/// The target triple this binary was compiled for, injected by `build.rs`.
///
/// Used by the bootstrap to decide whether our `current_exe` can serve a given
/// remote triple (M2 dev builds are `…-linux-gnu` but serve `…-linux-musl`
/// remotes — see `tomo_transport::binary_for_triple`).
pub const BUILD_TARGET: &str = env!("TOMO_BUILD_TARGET");

/// Whether this is a non-release (dev) build. Gates the debug-only bootstrap
/// substitution and test hooks.
pub const DEV_BUILD: bool = cfg!(debug_assertions);

/// The effective binary version: `CARGO_PKG_VERSION`, with the debug-only
/// `TOMO_TEST_FORCE_LOCAL_VERSION` override applied by the transport crate.
///
/// This single source feeds both the `Hello` handshake and the bootstrap binary
/// name, so a forced skew is consistent across the two.
pub fn binary_version() -> String {
    tomo_transport::effective_local_version(env!("CARGO_PKG_VERSION"))
}
