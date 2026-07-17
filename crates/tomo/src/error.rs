//! The one binary-level error type.
//!
//! Library crates return their own `thiserror` enums (per the hygiene policy);
//! `tomo` wraps them here with human context and is the *only* place that
//! renders an error to a person (see [`render`]). Every variant maps to a
//! process exit code via [`CliError::exit_code`].

use std::path::{Path, PathBuf};

/// A user-facing CLI error.
///
/// Wraps each adapter crate's error plus the CLI's own failure modes. The
/// `main` entry point renders the whole `source` chain and exits with
/// [`CliError::exit_code`].
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// A plain diagnostic with no wrapped cause.
    #[error("{0}")]
    Message(String),

    /// A command that is recognized but not yet implemented at this milestone.
    /// Rendered like any error but exits `2` so scripts can tell it apart from
    /// a genuine failure.
    #[error("{0}")]
    Unimplemented(String),

    /// A filesystem operation failed at a specific path.
    #[error("{op} {}: {source}", path.display())]
    Io {
        /// What was being attempted (e.g. `"create directory"`).
        op: String,
        /// The path the failing operation touched.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// Configuration could not be loaded or parsed.
    #[error(transparent)]
    Config(#[from] tomo_config::ConfigError),

    /// The filesystem watcher or a scan failed.
    #[error(transparent)]
    Watch(#[from] tomo_watch::WatchError),

    /// The wire protocol could not be framed or decoded.
    #[error(transparent)]
    Proto(#[from] tomo_proto::ProtoError),

    /// A state file could not be (de)serialized.
    #[error("{context}: {source}")]
    Codec {
        /// What was being (de)serialized.
        context: String,
        /// The underlying `postcard` error.
        source: postcard::Error,
    },
}

impl CliError {
    /// Build an [`CliError::Io`] from an operation description, path, and cause.
    pub fn io(op: impl Into<String>, path: impl AsRef<Path>, source: std::io::Error) -> Self {
        CliError::Io {
            op: op.into(),
            path: path.as_ref().to_path_buf(),
            source,
        }
    }

    /// Build a bare [`CliError::Message`].
    pub fn msg(text: impl Into<String>) -> Self {
        CliError::Message(text.into())
    }

    /// The process exit code this error should produce: `2` for an
    /// unimplemented command, `1` for everything else.
    pub fn exit_code(&self) -> i32 {
        match self {
            CliError::Unimplemented(_) => 2,
            _ => 1,
        }
    }
}

/// Render an error and its full `source` chain to `stderr` as
/// `error: <top>: <cause>: …`.
pub fn render(err: &CliError) {
    let mut out = format!("error: {err}");
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        // `transparent` variants already print their inner error, so avoid
        // repeating an identical segment.
        let segment = cause.to_string();
        if !out.ends_with(&segment) {
            out.push_str(": ");
            out.push_str(&segment);
        }
        source = cause.source();
    }
    eprintln!("{out}");
}
