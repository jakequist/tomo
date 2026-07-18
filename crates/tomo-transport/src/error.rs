//! The transport crate's error type.
//!
//! Per the hygiene policy each adapter crate owns a `thiserror` enum; the CLI
//! (`tomo`) wraps it with human context. Variants carry the host and the phase
//! (connect / auth / bootstrap / spawn) so a failure names *where* it happened.

use std::path::PathBuf;

/// Anything that can go wrong establishing or using the SSH transport.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The `user@host[:port]` target could not be parsed.
    #[error("invalid ssh target {target:?}: {reason}")]
    HostSpec {
        /// The offending target string.
        target: String,
        /// Why it was rejected.
        reason: String,
    },

    /// The TCP/SSH connection to the host could not be opened.
    #[error("cannot connect to {host}: {source}")]
    Connect {
        /// The `host:port` we tried to reach.
        host: String,
        /// The underlying russh/IO error.
        source: Box<russh::Error>,
    },

    /// The server's host key is not present in `known_hosts`.
    #[error(
        "host key for {host} is not in known_hosts — connect once with \
         `ssh {host}` to record it, then retry"
    )]
    HostKeyUnknown {
        /// The host whose key is unknown.
        host: String,
    },

    /// The server's host key does not match the recorded one (possible MITM).
    #[error(
        "host key for {host} does not match known_hosts line {line} — \
         if the server was legitimately rebuilt, remove that line and re-verify"
    )]
    HostKeyMismatch {
        /// The host whose key changed.
        host: String,
        /// The `known_hosts` line that no longer matches.
        line: usize,
    },

    /// `known_hosts` itself could not be read/parsed.
    #[error("reading known_hosts for {host}: {reason}")]
    KnownHosts {
        /// The host being verified.
        host: String,
        /// What went wrong.
        reason: String,
    },

    /// Every authentication method was exhausted without success.
    #[error("authentication failed for {user}@{host}: {detail}")]
    AuthFailed {
        /// The user we authenticated as.
        user: String,
        /// The host we authenticated to.
        host: String,
        /// A summary of what was tried and why each failed.
        detail: String,
    },

    /// A private key file exists but could not be used (e.g. it is
    /// passphrase-encrypted, which is out of scope for M2).
    #[error("cannot use key {}: {reason}", path.display())]
    KeyFile {
        /// The key file path.
        path: PathBuf,
        /// Why it was unusable.
        reason: String,
    },

    /// A remote command (`uname`, `sha256sum`, …) failed to run or exited
    /// non-zero.
    #[error("remote command `{cmd}` failed on {host}: {detail}")]
    RemoteCommand {
        /// The command that failed.
        cmd: String,
        /// The host it ran on.
        host: String,
        /// Exit status and/or stderr context.
        detail: String,
    },

    /// The remote OS/arch (from `uname`) maps to no supported target triple.
    #[error(
        "unsupported remote target: uname reported {detected:?}, which Tomo has \
         no binary for (supported: {supported}). No external downloads are ever \
         attempted."
    )]
    UnsupportedTarget {
        /// What `uname -s -m` (or a forced override) reported.
        detected: String,
        /// The comma-separated list of triples Tomo can serve.
        supported: String,
    },

    /// An SFTP operation during the binary push failed.
    #[error("sftp {op} {path:?} on {host}: {reason}")]
    Sftp {
        /// The operation (mkdir / write / rename / …).
        op: String,
        /// The remote path involved.
        path: String,
        /// The host.
        host: String,
        /// The underlying error.
        reason: String,
    },

    /// The pushed binary's SHA-256 did not match the local source.
    #[error(
        "integrity check failed for pushed binary on {host}: expected {expected}, \
         remote reported {actual}"
    )]
    Integrity {
        /// The host.
        host: String,
        /// The SHA-256 we sent.
        expected: String,
        /// The SHA-256 the remote computed.
        actual: String,
    },

    /// The local binary to push could not be located or read.
    #[error("locating the local binary to push: {reason}")]
    LocalBinary {
        /// Why the local binary is unavailable.
        reason: String,
    },

    /// Opening or exec-ing the remote sync process failed.
    #[error("spawning remote `serve --stdio` on {host}: {reason}")]
    Spawn {
        /// The host.
        host: String,
        /// What went wrong.
        reason: String,
    },

    /// The internal tokio runtime could not be created.
    #[error("initializing the transport runtime: {source}")]
    Runtime {
        /// The IO error from the runtime builder.
        source: std::io::Error,
    },

    /// The `~/.ssh/config` `ProxyJump` chain for the target is unusable (a
    /// cycle, too deep, or a malformed hop). Config *parse* problems are never
    /// fatal, but a route we cannot build leaves nowhere to connect.
    #[error("ssh config proxy jump for {target:?}: {reason}")]
    ProxyJump {
        /// The target whose route could not be resolved.
        target: String,
        /// Why the route is unusable.
        reason: String,
    },

    /// A `ProxyJump` hop could not be reached (TCP/channel/auth), naming which
    /// hop failed so the user knows where the chain broke.
    #[error("cannot reach ssh jump host {hop}: {reason}")]
    JumpConnect {
        /// The failing hop (`alias`/`host:port`).
        hop: String,
        /// What went wrong opening or authenticating the hop.
        reason: String,
    },
}
