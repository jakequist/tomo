//! Hidden developer diagnostics (`tomo dev …`). Not part of the stable CLI
//! surface; these exist to inspect build-time facts that the release tooling and
//! scenarios assert against.

use serde::Serialize;

use crate::error::CliError;

/// One embedded binary, as reported by `tomo dev embedded-binaries --json`.
#[derive(Debug, Serialize)]
struct EmbeddedEntry {
    /// The target triple the embedded binary runs on.
    triple: String,
    /// The exact version it was built at.
    version: String,
    /// The embedded payload size in bytes.
    bytes: usize,
}

/// Print the release binaries embedded into this build's bootstrap payload.
///
/// Empty in ordinary dev builds; populated only when compiled with
/// `--features embed-binaries`. `scripts/release.sh` runs this against the fat
/// binary to verify its embedded inventory (docs/RELEASING.md).
///
/// # Errors
/// [`CliError`] only if JSON serialization fails.
pub fn run_embedded_binaries(json: bool) -> Result<(), CliError> {
    let inventory = tomo_transport::embedded_inventory();

    if json {
        let entries: Vec<EmbeddedEntry> = inventory
            .iter()
            .map(|(triple, version, bytes)| EmbeddedEntry {
                triple: (*triple).to_owned(),
                version: (*version).to_owned(),
                bytes: *bytes,
            })
            .collect();
        let out = serde_json::to_string_pretty(&entries)
            .map_err(|e| CliError::msg(format!("could not serialize embedded inventory: {e}")))?;
        println!("{out}");
    } else if inventory.is_empty() {
        println!("no binaries embedded (dev build; rebuild with --features embed-binaries)");
    } else {
        println!("embedded binaries ({}):", inventory.len());
        for (triple, version, bytes) in &inventory {
            println!("  tomo {version}  {triple}  ({bytes} bytes)");
        }
    }
    Ok(())
}
