//! Generate the embedded-binary table for the `embed-binaries` feature.
//!
//! When the feature is on, we scan `$TOMO_EMBED_DIR` for release artifacts named
//! `tomo-<version>-<triple>` (any of the four v0 triples) and emit an `EMBEDDED`
//! slice of `include_bytes!` entries into `$OUT_DIR/embedded.rs`, which
//! `binsource.rs` includes. When the feature is off (every dev build) we still
//! write the file, but empty — keeping the edit-compile loop fast and free of
//! any 40 MB payload.
//!
//! Designed for testability: the table is built from *whatever files exist* in
//! `$TOMO_EMBED_DIR`, so a fixture directory of tiny stub files stands in for
//! real binaries. No `unwrap`/`expect` — the workspace lints deny them here too.

use std::fmt::Write as _;
use std::path::PathBuf;

/// The v0 release triples (docs/SPEC.md §3). Kept in sync with `triple.rs`; a
/// build script cannot import the crate it builds, so the list is duplicated.
const SUPPORTED: &[&str] = &[
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
];

/// Parse `tomo-<version>-<triple>` into `(version, triple)` for a known triple.
///
/// The triple itself contains dashes, so we match by known suffix rather than
/// splitting: strip the `tomo-` prefix, then the `-<triple>` suffix; whatever
/// remains is the version (which must be non-empty).
fn parse_artifact(name: &str) -> Option<(String, &'static str)> {
    let rest = name.strip_prefix("tomo-")?;
    for &triple in SUPPORTED {
        let suffix = format!("-{triple}");
        if let Some(version) = rest.strip_suffix(&suffix) {
            if !version.is_empty() {
                return Some((version.to_owned(), triple));
            }
        }
    }
    None
}

fn main() -> std::io::Result<()> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=TOMO_EMBED_DIR");

    let out_dir = match std::env::var_os("OUT_DIR") {
        Some(d) => PathBuf::from(d),
        None => return Err(std::io::Error::other("OUT_DIR is not set")),
    };

    let mut body = String::new();

    // Cargo sets CARGO_FEATURE_<NAME> when a feature is active; only then do we
    // embed anything at all.
    let embed = std::env::var_os("CARGO_FEATURE_EMBED_BINARIES").is_some();
    if embed {
        if let Some(dir) = std::env::var_os("TOMO_EMBED_DIR") {
            let dir = PathBuf::from(&dir);
            println!("cargo:rerun-if-changed={}", dir.display());

            let mut entries: Vec<(String, &'static str, String)> = Vec::new();
            if let Ok(read) = std::fs::read_dir(&dir) {
                for entry in read.flatten() {
                    let fname = entry.file_name();
                    let Some(name) = fname.to_str() else { continue };
                    let Some((version, triple)) = parse_artifact(name) else {
                        continue;
                    };
                    let abs = std::fs::canonicalize(entry.path())?;
                    let abs_str = abs.to_string_lossy().into_owned();
                    println!("cargo:rerun-if-changed={abs_str}");
                    entries.push((version, triple, abs_str));
                }
            }
            // Deterministic order regardless of readdir ordering.
            entries.sort();
            for (version, triple, abs_str) in entries {
                // Writing to a String is infallible; the result is discarded.
                let _ = writeln!(
                    body,
                    "    EmbeddedBinary {{ triple: {triple:?}, version: {version:?}, \
                     bytes: include_bytes!({abs_str:?}) }},"
                );
            }
        }
    }

    let src = format!("pub(super) static EMBEDDED: &[EmbeddedBinary] = &[\n{body}];\n");
    std::fs::write(out_dir.join("embedded.rs"), src)
}
