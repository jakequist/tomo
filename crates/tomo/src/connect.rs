//! `tomo connect`: record a sync peer in `.tomo/config.toml`.
//!
//! Establishing the SSH session and remote bootstrap is M2; at M1 this command
//! only persists the `[remote]` section so the configuration is honest about
//! its intended peer. Actual syncing then requires either M2 (SSH) or, today,
//! `tomo watch --local-peer <path>`.

use std::fmt::Write as _;

use tomo_config::Config;

use crate::error::CliError;
use crate::layout::Layout;

/// Run `tomo connect <target> <remote_path>`.
///
/// # Errors
/// [`CliError`] if the project is not initialized or the config cannot be
/// read/written.
pub fn run(layout: &Layout, target: &str, remote_path: &str) -> Result<(), CliError> {
    if !layout.is_initialized() {
        return Err(CliError::msg(
            "not a Tomo project (no .tomo/ here) — run `tomo init` first",
        ));
    }

    // Validate that the peer string parses before recording it.
    let path = layout.config();
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => return Err(CliError::io("read config", &path, source)),
    };

    if existing.lines().any(|line| line.trim() == "[remote]") {
        return Err(CliError::msg(
            "a [remote] is already configured; edit .tomo/config.toml to change it",
        ));
    }

    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    // Writing to a String is infallible.
    let _ = write!(
        updated,
        "\n[remote]\nhost = \"{target}\"\npath = \"{remote_path}\"\n"
    );

    // Parse the result so we never write a config we cannot load back.
    Config::from_toml_str(&updated)?;
    std::fs::write(&path, &updated).map_err(|s| CliError::io("write config", &path, s))?;

    println!(
        "recorded remote {target}:{remote_path} in {}",
        path.display()
    );
    println!(
        "note: live SSH sync lands at M2; use `tomo watch --local-peer <path>` for local sync now"
    );
    Ok(())
}
