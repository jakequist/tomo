//! `tomo serve --stdio`: the served half of the M1 local transport.
//!
//! Hidden from `--help`. Its stdin/stdout **is** the protocol channel, so this
//! command must never write to stdout — every diagnostic goes to
//! `.tomo/logs/serve.log` via a [`Reporter::Log`] (CLAUDE.md: serve's stdout
//! carries frames only).

use std::fs::OpenOptions;
use std::sync::Mutex;

use tomo_config::Config;

use crate::error::CliError;
use crate::init;
use crate::layout::Layout;
use crate::report::Reporter;
use crate::session::{self, Mode};
use crate::{current_dir, replica};

/// Run `tomo serve --stdio`.
///
/// # Errors
/// [`CliError`] if `--stdio` is absent, the project cannot be initialized, or
/// the sync loop fails.
pub fn run(stdio: bool) -> Result<(), CliError> {
    if !stdio {
        return Err(CliError::msg(
            "`tomo serve` currently supports only --stdio (spawned by `watch --local-peer`)",
        ));
    }

    let root = current_dir()?;
    let layout = Layout::new(&root);
    // The parent auto-initializes us before spawning, but be defensive: a serve
    // process must have a replica id and state dirs. `ensure_initialized` prints
    // nothing, so stdout stays clean.
    init::ensure_initialized(&layout)?;

    let replica = replica::load(&layout.replica())?;
    let config = Config::load(layout.root())?;

    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(layout.serve_log())
        .map_err(|s| CliError::io("open serve log", layout.serve_log(), s))?;
    let reporter = Reporter::Log(Mutex::new(log));

    session::run(layout, config, replica, reporter, Mode::Serve)
}
