//! `tomo watch`: the foreground sync loop.

use std::path::PathBuf;

use tomo_config::Config;

use crate::error::CliError;
use crate::layout::Layout;
use crate::report::Reporter;
use crate::session::{self, Mode};
use crate::transport::SshParams;
use crate::{current_dir, replica};

/// Run `tomo watch`, optionally against a local peer directory.
///
/// Transport selection: `--local-peer <path>` uses the M1 local transport; a
/// configured `[remote]` (and no `--local-peer`) uses the SSH transport (M2);
/// otherwise it is watch-only.
///
/// # Errors
/// [`CliError`] if the project is not initialized, config/replica cannot be
/// loaded, the peer path is invalid, or the sync loop fails.
pub fn run(local_peer: Option<PathBuf>, json: bool) -> Result<(), CliError> {
    let root = current_dir()?;
    let layout = Layout::new(&root);
    if !layout.is_initialized() {
        return Err(CliError::msg(
            "not a Tomo project (no .tomo/ here) — run `tomo init` first",
        ));
    }

    let replica = replica::load(&layout.replica())?;
    let config = Config::load(layout.root())?;
    let reporter = Reporter::Human { json };

    let mode = match local_peer {
        Some(path) => {
            let resolved = std::fs::canonicalize(&path)
                .map_err(|s| CliError::io("open --local-peer directory", &path, s))?;
            Mode::LocalPeer(resolved)
        }
        None => match &config.remote {
            Some(remote) => Mode::Ssh(SshParams::from_remote(remote)?),
            None => Mode::WatchOnly,
        },
    };

    session::run(layout, config, replica, reporter, mode)
}
