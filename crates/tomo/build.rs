//! Expose the Cargo build target triple to the crate at compile time.
//!
//! The SSH bootstrap (M2) needs to know which triple *this* binary was compiled
//! for so `tomo-transport`'s binary-source logic can decide whether our
//! `current_exe` can serve a given remote triple (see
//! `tomo_transport::binary_for_triple`). Cargo sets `TARGET` for build scripts
//! but not for the crate itself, so we re-export it as `TOMO_BUILD_TARGET`.

fn main() {
    // `TARGET` is always present in a build-script environment.
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());
    println!("cargo:rustc-env=TOMO_BUILD_TARGET={target}");
    // Only re-run if the build script itself changes (TARGET is stable per
    // build invocation).
    println!("cargo:rerun-if-changed=build.rs");
}
