//! `tomo completions <shell>` — shell completion script generation (§9).
//!
//! Generates a bash/zsh/fish (etc.) completion script straight from the parsed
//! `clap` command and writes it to stdout through the [`crate::out`] helpers, so
//! `tomo completions bash | head` (or redirecting into a completions dir) is
//! pipe-safe and never panics on a closed reader.

use clap::CommandFactory;
use clap_complete::{generate, Shell};

use crate::cli::Cli;
use crate::error::CliError;

/// Run `tomo completions <shell>`: write the completion script to stdout.
///
/// # Errors
/// [`CliError`] on a non-pipe stdout write failure (a broken pipe exits `0`
/// quietly, like every other informational command).
pub fn run(shell: Shell) -> Result<(), CliError> {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_owned();
    // Generate into a buffer, then emit through the broken-pipe-safe writer
    // rather than letting `clap_complete` write straight to a raw stdout handle.
    let mut buf: Vec<u8> = Vec::new();
    generate(shell, &mut cmd, name, &mut buf);
    crate::out::bytes(&buf)
}
