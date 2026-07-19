//! `tomo watch`: deprecated alias for bare `tomo sync` (hidden from `--help`).
//!
//! The command was renamed to `tomo sync` (which also subsumes `tomo connect`
//! when given a target). This shim keeps `tomo watch` — and its
//! `--local-peer`/`--json` flags — working, printing a one-line deprecation
//! note, and forwards to [`crate::sync`].

use std::path::PathBuf;

use crate::error::CliError;
use crate::sync;

/// Run the deprecated `tomo watch`, forwarding to bare `tomo sync`.
///
/// # Errors
/// Whatever [`crate::sync::run`] returns.
pub fn run(local_peer: Option<PathBuf>, json: bool) -> Result<(), CliError> {
    eprintln!("note: `tomo watch` is now `tomo sync`");
    sync::run(None, None, local_peer, false, json)
}
