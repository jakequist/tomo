//! The live SSH session: connect, authenticate, verify the host key, run remote
//! commands, push the binary over SFTP, and spawn `serve --stdio` as a blocking
//! byte duplex (docs/SPEC.md §2–3).
//!
//! russh is async and tokio-based; the rest of Tomo's session loop is
//! synchronous. This module confines tokio here and exposes a **blocking
//! facade**: an internal multi-threaded runtime owned by [`SshSession`] drives
//! every operation via `block_on`, and [`spawn_remote`](SshSession::spawn_remote)
//! hands back plain [`std::io::Read`]/[`std::io::Write`] handles bridged to the
//! runtime over channels — exactly the shape the `tomo` crate's reader-thread /
//! `FrameWriter` plumbing already expects.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client::{self, Handle};
use russh::keys::agent::client::AgentClient;
use russh::keys::{load_secret_key, PrivateKeyWithHashAlg};
use russh::ChannelMsg;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, OpenFlags};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

use crate::bootstrap::{self, BootstrapDecision, BootstrapReport};
use crate::error::TransportError;
use crate::hostspec::HostSpec;
use crate::quote::shell_quote;
use crate::triple;

/// Options for opening an [`SshSession`].
#[derive(Debug, Clone)]
pub struct SshOpts {
    /// The local login name to use when the target omits `user@`.
    pub default_user: String,
    /// Path to `known_hosts` (default `~/.ssh/known_hosts`).
    pub known_hosts: PathBuf,
    /// Candidate private-key files, tried in order after ssh-agent.
    pub identity_files: Vec<PathBuf>,
    /// TCP connect timeout.
    pub connect_timeout: Duration,
}

impl SshOpts {
    /// Sensible defaults rooted at the user's `~/.ssh`: agent first, then
    /// `id_ed25519` and `id_rsa`; `known_hosts` in the standard place.
    ///
    /// `home` is the user's home directory and `user` the local login name;
    /// resolving these is the CLI's job (this crate reads no environment for
    /// policy, keeping behaviour explicit and testable).
    pub fn new(home: &std::path::Path, user: &str) -> Self {
        let ssh = home.join(".ssh");
        SshOpts {
            default_user: user.to_owned(),
            known_hosts: ssh.join("known_hosts"),
            identity_files: vec![ssh.join("id_ed25519"), ssh.join("id_rsa")],
            connect_timeout: Duration::from_secs(20),
        }
    }
}

/// A live, authenticated SSH session with a running internal runtime.
pub struct SshSession {
    runtime: Runtime,
    handle: Handle<Client>,
    host: String,
}

impl SshSession {
    /// Connect to `target` (`user@host[:port]`), verify the host key against
    /// `known_hosts`, and authenticate (ssh-agent first, then the configured key
    /// files; unencrypted keys only — an encrypted key is reported clearly).
    ///
    /// # Errors
    /// A [`TransportError`] naming the phase that failed (host-spec, connect,
    /// host-key, or auth).
    pub fn connect(target: &str, opts: &SshOpts) -> Result<SshSession, TransportError> {
        let spec = HostSpec::parse(target)?;
        let user = spec.user_or(&opts.default_user).to_owned();
        let host = spec.host.clone();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|source| TransportError::Runtime { source })?;

        let handle = runtime.block_on(Self::connect_async(&spec, &user, opts))?;

        Ok(SshSession {
            runtime,
            handle,
            host,
        })
    }

    async fn connect_async(
        spec: &HostSpec,
        user: &str,
        opts: &SshOpts,
    ) -> Result<Handle<Client>, TransportError> {
        let config = Arc::new(client::Config {
            inactivity_timeout: Some(Duration::from_hours(1)),
            keepalive_interval: Some(Duration::from_secs(30)),
            ..Default::default()
        });

        let verdict = Arc::new(Mutex::new(HostKeyVerdict::Pending));
        let handler = Client {
            host: spec.host.clone(),
            port: spec.port,
            known_hosts: opts.known_hosts.clone(),
            verdict: Arc::clone(&verdict),
        };

        let host_port = spec.host_port();
        let connect = client::connect(config, (spec.host.as_str(), spec.port), handler);
        let mut handle = match tokio::time::timeout(opts.connect_timeout, connect).await {
            Ok(Ok(h)) => h,
            Ok(Err(e)) => {
                // A rejected host key surfaces as a connection error; translate
                // it into the specific, actionable message.
                if let Some(err) = Self::host_key_error(&verdict) {
                    return Err(err);
                }
                return Err(TransportError::Connect {
                    host: host_port,
                    source: Box::new(e),
                });
            }
            Err(_) => {
                return Err(TransportError::Connect {
                    host: host_port,
                    source: Box::new(russh::Error::ConnectionTimeout),
                });
            }
        };

        Self::authenticate(&mut handle, user, spec, opts).await?;
        Ok(handle)
    }

    /// Translate a recorded host-key verdict into the specific error, if any.
    fn host_key_error(verdict: &Arc<Mutex<HostKeyVerdict>>) -> Option<TransportError> {
        let v = verdict.lock().ok()?;
        match &*v {
            HostKeyVerdict::Unknown { host } => {
                Some(TransportError::HostKeyUnknown { host: host.clone() })
            }
            HostKeyVerdict::Mismatch { host, line } => Some(TransportError::HostKeyMismatch {
                host: host.clone(),
                line: *line,
            }),
            HostKeyVerdict::ReadError { host, reason } => Some(TransportError::KnownHosts {
                host: host.clone(),
                reason: reason.clone(),
            }),
            HostKeyVerdict::Pending | HostKeyVerdict::Ok => None,
        }
    }

    async fn authenticate(
        handle: &mut Handle<Client>,
        user: &str,
        spec: &HostSpec,
        opts: &SshOpts,
    ) -> Result<(), TransportError> {
        let mut tried: Vec<String> = Vec::new();

        // 1. ssh-agent, if one is reachable.
        match Self::auth_agent(handle, user).await {
            Ok(true) => return Ok(()),
            Ok(false) => tried.push("ssh-agent (no identity accepted)".to_owned()),
            Err(reason) => tried.push(format!("ssh-agent ({reason})")),
        }

        // 2. Default key files, unencrypted only.
        for path in &opts.identity_files {
            if !path.exists() {
                continue;
            }
            match load_secret_key(path, None) {
                Ok(key) => {
                    let kwh = PrivateKeyWithHashAlg::new(Arc::new(key), None);
                    match handle.authenticate_publickey(user, kwh).await {
                        Ok(res) if res.success() => return Ok(()),
                        Ok(_) => tried.push(format!("{} (rejected)", path.display())),
                        Err(e) => tried.push(format!("{} ({e})", path.display())),
                    }
                }
                Err(e) => {
                    // Passphrase-protected keys are out of scope for M2; say so
                    // plainly rather than silently skipping.
                    tried.push(format!(
                        "{} (unusable: {e}; passphrase-encrypted keys are not supported yet)",
                        path.display()
                    ));
                }
            }
        }

        Err(TransportError::AuthFailed {
            user: user.to_owned(),
            host: spec.host.clone(),
            detail: tried.join("; "),
        })
    }

    /// Try every identity the agent holds. Returns `Ok(true)` on success,
    /// `Ok(false)` if the agent had no accepted identity, `Err` if the agent is
    /// unreachable.
    async fn auth_agent(handle: &mut Handle<Client>, user: &str) -> Result<bool, String> {
        if std::env::var_os("SSH_AUTH_SOCK").is_none() {
            return Err("no SSH_AUTH_SOCK".to_owned());
        }
        let mut agent = AgentClient::connect_env()
            .await
            .map_err(|e| format!("connect: {e}"))?;
        let identities = agent
            .request_identities()
            .await
            .map_err(|e| format!("request identities: {e}"))?;
        for id in identities {
            // russh 0.62 models agent identities as an `AgentIdentity` enum
            // (plain key or OpenSSH certificate); `authenticate_publickey_with`
            // takes the bare public key, so extract it (owned) for either case.
            let key = id.public_key().into_owned();
            match handle
                .authenticate_publickey_with(user, key, None, &mut agent)
                .await
            {
                Ok(res) if res.success() => return Ok(true),
                Ok(_) => {}
                Err(e) => return Err(format!("sign: {e}")),
            }
        }
        Ok(false)
    }

    /// Run `cmd` on the remote via a session channel and collect its exit code,
    /// stdout, and stderr. `cmd` is passed verbatim to the remote login shell,
    /// so callers must [`shell_quote`] any interpolated values.
    ///
    /// # Errors
    /// [`TransportError::RemoteCommand`] if the channel cannot be opened or the
    /// command cannot be started.
    pub fn exec(&self, cmd: &str) -> Result<ExecOutput, TransportError> {
        self.runtime
            .block_on(Self::exec_async(&self.handle, &self.host, cmd))
    }

    async fn exec_async(
        handle: &Handle<Client>,
        host: &str,
        cmd: &str,
    ) -> Result<ExecOutput, TransportError> {
        let fail = |detail: String| TransportError::RemoteCommand {
            cmd: cmd.to_owned(),
            host: host.to_owned(),
            detail,
        };
        let mut channel = handle
            .channel_open_session()
            .await
            .map_err(|e| fail(format!("open channel: {e}")))?;
        channel
            .exec(true, cmd.as_bytes())
            .await
            .map_err(|e| fail(format!("exec: {e}")))?;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut code: Option<u32> = None;
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
                ChannelMsg::ExtendedData { data, ext: 1 } => stderr.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                _ => {}
            }
        }
        Ok(ExecOutput {
            code: code.map_or(-1, u32::cast_signed),
            stdout,
            stderr,
        })
    }

    /// Open an SFTP subsystem for the binary push. The returned handle borrows
    /// this session's runtime and offers a small blocking file API.
    ///
    /// # Errors
    /// [`TransportError::Sftp`] if the subsystem cannot be started.
    pub fn sftp(&self) -> Result<Sftp<'_>, TransportError> {
        let host = self.host.clone();
        let session = self.runtime.block_on(async {
            let channel = self
                .handle
                .channel_open_session()
                .await
                .map_err(|e| sftp_err("open channel", "", &host, e.to_string()))?;
            channel
                .request_subsystem(true, "sftp")
                .await
                .map_err(|e| sftp_err("request subsystem", "", &host, e.to_string()))?;
            SftpSession::new(channel.into_stream())
                .await
                .map_err(|e| sftp_err("start session", "", &host, e.to_string()))
        })?;
        Ok(Sftp {
            runtime: &self.runtime,
            inner: session,
            host: self.host.clone(),
        })
    }

    /// Detect the remote target, decide reuse-vs-push, and (re)push the binary
    /// if needed — the full bootstrap (docs/SPEC.md §3).
    ///
    /// `built_for` is the triple this binary was compiled for (from the CLI's
    /// `build.rs`); `local_version` is the CLI's `CARGO_PKG_VERSION`;
    /// `force_push` re-pushes even on an exact match (used by the version-
    /// mismatch retry); `dev_build` enables the debug-only gnu→musl
    /// substitution.
    ///
    /// # Errors
    /// Any [`TransportError`] from detection, unsupported targets, SFTP, or the
    /// integrity check.
    pub fn bootstrap(
        &self,
        remote_path: &str,
        built_for: &str,
        local_version: &str,
        force_push: bool,
        dev_build: bool,
    ) -> Result<BootstrapReport, TransportError> {
        let detected = self.detect_triple()?;
        let bin_dir = format!("{remote_path}/{}", bootstrap::REMOTE_BIN_DIR);
        let binary_rel = bootstrap::binary_rel_path(local_version, detected);

        let sftp = self.sftp()?;
        // Ensure the project root and .tomo/bin exist (fresh remote is allowed).
        sftp.mkdir_p(remote_path)?;
        sftp.mkdir_p(&bin_dir)?;

        let entries = sftp.list_names(&bin_dir)?;
        let decision = bootstrap::decide(&entries, local_version, detected);

        match decision {
            BootstrapDecision::Reuse { name, stale } if !force_push => {
                sftp.prune(&bin_dir, &stale);
                Ok(BootstrapReport::Reused {
                    triple: detected.to_owned(),
                    version: local_version.to_owned(),
                    binary_rel: format!("{}/{name}", bootstrap::REMOTE_BIN_DIR),
                })
            }
            BootstrapDecision::Reuse { name, stale } | BootstrapDecision::Push { name, stale } => {
                let source =
                    bootstrap::resolve_source(detected, built_for, local_version, dev_build)?;
                let bytes_len = source.bytes.len() as u64;
                self.push_binary(&sftp, &bin_dir, &name, &source.bytes)?;
                sftp.prune(&bin_dir, &stale);
                Ok(BootstrapReport::Pushed {
                    triple: detected.to_owned(),
                    version: local_version.to_owned(),
                    binary_rel,
                    bytes: bytes_len,
                    embedded: source.embedded,
                    dev_substitution: source.dev_substitution,
                })
            }
        }
    }

    /// Detect the remote target triple via `uname -s -m`, honoring the
    /// debug-only `TOMO_TEST_FORCE_REMOTE_TRIPLE` override.
    fn detect_triple(&self) -> Result<&'static str, TransportError> {
        #[cfg(debug_assertions)]
        if let Some(forced) = std::env::var_os("TOMO_TEST_FORCE_REMOTE_TRIPLE") {
            // DEBUG-ONLY test hook (scenario 04): pretend the remote is a given
            // triple to exercise re-push and unsupported-target paths on
            // localhost. Never compiled into release builds.
            let forced = forced.to_string_lossy().into_owned();
            return triple::SUPPORTED
                .iter()
                .copied()
                .find(|t| *t == forced)
                .ok_or_else(|| TransportError::UnsupportedTarget {
                    detected: format!("{forced} (forced via TOMO_TEST_FORCE_REMOTE_TRIPLE)"),
                    supported: triple::supported_list(),
                });
        }

        let out = self.exec("uname -s -m")?;
        if out.code != 0 {
            return Err(TransportError::RemoteCommand {
                cmd: "uname -s -m".to_owned(),
                host: self.host.clone(),
                detail: format!("exit {}: {}", out.code, out.stderr_lossy()),
            });
        }
        let stdout = out.stdout_lossy();
        let (os, arch) = triple::parse_uname(&stdout)?;
        triple::uname_to_triple(&os, &arch)
    }

    /// Push `bytes` to `bin_dir/name`: write to a temp sibling, chmod 755,
    /// verify SHA-256, atomic rename.
    fn push_binary(
        &self,
        sftp: &Sftp<'_>,
        bin_dir: &str,
        name: &str,
        bytes: &[u8],
    ) -> Result<(), TransportError> {
        let final_path = format!("{bin_dir}/{name}");
        let counter = NEXT_TMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp_path = format!("{bin_dir}/.{name}.tmp.{}.{counter}", std::process::id());

        sftp.write_file(&tmp_path, bytes)?;
        // Set the executable bit via a remote `chmod`. We deliberately avoid an
        // SFTP `setstat`: OpenSSH's internal-sftp rejects a path-based setstat
        // on some configurations ("permission denied"), whereas `chmod` is
        // universally available (like the `uname`/`sha256sum` we already run).
        let chmod = self.exec(&format!("chmod 755 {}", shell_quote(&tmp_path)))?;
        if chmod.code != 0 {
            let _ = sftp.remove(&tmp_path);
            return Err(TransportError::RemoteCommand {
                cmd: "chmod 755".to_owned(),
                host: self.host.clone(),
                detail: format!("exit {}: {}", chmod.code, chmod.stderr_lossy()),
            });
        }

        // Integrity check: prefer a remote hash tool; fall back to SFTP readback.
        let expected = hex_sha256(bytes);
        let actual = self.remote_sha256(&tmp_path).unwrap_or_else(|| {
            sftp.read(&tmp_path)
                .map(|b| hex_sha256(&b))
                .unwrap_or_default()
        });
        if actual != expected {
            let _ = sftp.remove(&tmp_path);
            return Err(TransportError::Integrity {
                host: self.host.clone(),
                expected,
                actual,
            });
        }

        sftp.rename(&tmp_path, &final_path)?;
        Ok(())
    }

    /// Compute the SHA-256 of a remote file using `sha256sum` or `shasum -a
    /// 256`. Returns `None` if neither tool is available, so the caller falls
    /// back to an SFTP readback.
    fn remote_sha256(&self, remote_path: &str) -> Option<String> {
        for tool in ["sha256sum", "shasum -a 256"] {
            let cmd = format!("{tool} {}", shell_quote(remote_path));
            if let Ok(out) = self.exec(&cmd) {
                if out.code == 0 {
                    // Output is `<hex>  <path>`; take the first token.
                    if let Some(hex) = out.stdout_lossy().split_whitespace().next() {
                        if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                            return Some(hex.to_ascii_lowercase());
                        }
                    }
                }
            }
        }
        None
    }

    /// Consume the session and spawn `serve --stdio` at `remote_path` using the
    /// pushed binary `binary_rel`, returning a blocking byte duplex.
    ///
    /// # Errors
    /// [`TransportError::Spawn`] if the channel cannot be opened or exec'd.
    pub fn spawn_remote(
        self,
        remote_path: &str,
        binary_rel: &str,
    ) -> Result<RemoteChannel, TransportError> {
        let SshSession {
            runtime,
            handle,
            host,
            ..
        } = self;

        let cmd = format!(
            "cd {} && exec {} serve --stdio",
            shell_quote(remote_path),
            shell_quote(binary_rel)
        );

        let channel = runtime
            .block_on(async {
                let ch = handle.channel_open_session().await?;
                ch.exec(false, cmd.as_bytes()).await?;
                Ok::<_, russh::Error>(ch)
            })
            .map_err(|e| TransportError::Spawn {
                host: host.clone(),
                reason: e.to_string(),
            })?;

        let (mut read_half, write_half) = channel.split();

        let (to_peer_tx, mut to_peer_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (from_peer_tx, from_peer_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let stderr = Arc::new(Mutex::new(StderrTail::default()));

        // Writer task: drain the outbound queue onto the channel.
        let writer_task = runtime.spawn(async move {
            while let Some(bytes) = to_peer_rx.recv().await {
                if write_half.data(&bytes[..]).await.is_err() {
                    break;
                }
            }
            let _ = write_half.eof().await;
        });

        // Reader task: forward channel data to the blocking reader, capturing
        // stderr and stopping on EOF/close.
        let stderr_task = Arc::clone(&stderr);
        let reader_task = runtime.spawn(async move {
            while let Some(msg) = read_half.wait().await {
                match msg {
                    // The `send` in the guard always runs; a false guard (send
                    // ok) falls through to the `_` no-op arm below.
                    ChannelMsg::Data { data } if from_peer_tx.send(data.to_vec()).is_err() => break,
                    ChannelMsg::ExtendedData { data, ext: 1 } => {
                        if let Ok(mut tail) = stderr_task.lock() {
                            tail.push(&data);
                        }
                    }
                    ChannelMsg::Eof | ChannelMsg::Close => break,
                    _ => {}
                }
            }
        });

        Ok(RemoteChannel {
            reader: ChannelReader {
                rx: from_peer_rx,
                buf: Vec::new(),
                pos: 0,
            },
            writer: ChannelWriter { tx: to_peer_tx },
            guard: RemoteGuard {
                runtime: Some(runtime),
                _handle: handle,
                stderr,
                reader_task,
                writer_task,
            },
        })
    }
}

/// A monotonic counter making temp filenames unique within this process.
static NEXT_TMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The result of a remote command.
#[derive(Debug, Clone)]
pub struct ExecOutput {
    /// The process exit code (`-1` if the channel closed without one).
    pub code: i32,
    /// Raw stdout bytes.
    pub stdout: Vec<u8>,
    /// Raw stderr bytes.
    pub stderr: Vec<u8>,
}

impl ExecOutput {
    /// stdout decoded lossily as UTF-8.
    pub fn stdout_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
    /// stderr decoded lossily as UTF-8.
    pub fn stderr_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// A blocking SFTP facade borrowing an [`SshSession`]'s runtime.
pub struct Sftp<'a> {
    runtime: &'a Runtime,
    inner: SftpSession,
    host: String,
}

impl Sftp<'_> {
    /// `mkdir -p`: create `path` and every missing ancestor. Existing
    /// directories are fine.
    ///
    /// # Errors
    /// [`TransportError::Sftp`] if a component cannot be created.
    pub fn mkdir_p(&self, path: &str) -> Result<(), TransportError> {
        // Build cumulative prefixes and create each in turn; ignore
        // already-exists errors (SFTP reports them as failures).
        let mut cumulative = String::new();
        let absolute = path.starts_with('/');
        for (i, comp) in path.split('/').filter(|c| !c.is_empty()).enumerate() {
            if i > 0 || absolute {
                cumulative.push('/');
            }
            cumulative.push_str(comp);
            let dir = cumulative.clone();
            let exists = self
                .runtime
                .block_on(self.inner.try_exists(dir.clone()))
                .unwrap_or(false);
            if exists {
                continue;
            }
            // Best-effort: a race or an existing dir manifests as an error we
            // re-check below.
            let _ = self.runtime.block_on(self.inner.create_dir(dir.clone()));
            let now_exists = self
                .runtime
                .block_on(self.inner.try_exists(dir.clone()))
                .unwrap_or(false);
            if !now_exists {
                return Err(sftp_err(
                    "mkdir",
                    &dir,
                    &self.host,
                    "could not create".into(),
                ));
            }
        }
        Ok(())
    }

    /// List the bare file names in `dir` (non-recursive). A missing directory
    /// yields an empty list.
    ///
    /// # Errors
    /// Never returns `Err` today (a listing failure is treated as an empty
    /// directory); the `Result` is kept for forward compatibility with stricter
    /// error handling.
    pub fn list_names(&self, dir: &str) -> Result<Vec<String>, TransportError> {
        match self.runtime.block_on(self.inner.read_dir(dir)) {
            Ok(rd) => Ok(rd.map(|entry| entry.file_name()).collect()),
            Err(_) => Ok(Vec::new()),
        }
    }

    /// Write `bytes` to `path`, creating or truncating it. The executable bit is
    /// set separately by the caller via a remote `chmod` (see `push_binary`).
    ///
    /// # Errors
    /// [`TransportError::Sftp`] on any failure.
    pub fn write_file(&self, path: &str, bytes: &[u8]) -> Result<(), TransportError> {
        self.runtime.block_on(async {
            // Creation-time mode hint (0755); some servers honor it, and the
            // caller's `chmod` guarantees it regardless.
            let attrs = FileAttributes {
                permissions: Some(0o755),
                ..Default::default()
            };
            let mut file = self
                .inner
                .open_with_flags_and_attributes(
                    path.to_owned(),
                    OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNCATE,
                    attrs,
                )
                .await
                .map_err(|e| sftp_err("open", path, &self.host, e.to_string()))?;
            file.write_all(bytes)
                .await
                .map_err(|e| sftp_err("write", path, &self.host, e.to_string()))?;
            file.shutdown()
                .await
                .map_err(|e| sftp_err("close", path, &self.host, e.to_string()))?;
            Ok(())
        })
    }

    /// Read the whole contents of `path`.
    ///
    /// # Errors
    /// [`TransportError::Sftp`] on any failure.
    pub fn read(&self, path: &str) -> Result<Vec<u8>, TransportError> {
        self.runtime
            .block_on(self.inner.read(path.to_owned()))
            .map_err(|e| sftp_err("read", path, &self.host, e.to_string()))
    }

    /// Atomically rename `from` to `to`, replacing any existing target.
    ///
    /// # Errors
    /// [`TransportError::Sftp`] on any failure.
    pub fn rename(&self, from: &str, to: &str) -> Result<(), TransportError> {
        self.runtime.block_on(async {
            // POSIX rename replaces atomically, but some servers reject a rename
            // onto an existing name; remove the target first, best-effort.
            let _ = self.inner.remove_file(to.to_owned()).await;
            self.inner
                .rename(from.to_owned(), to.to_owned())
                .await
                .map_err(|e| sftp_err("rename", to, &self.host, e.to_string()))
        })
    }

    /// Remove a file, ignoring "not found".
    ///
    /// # Errors
    /// Never returns `Err` (removal is best-effort); the `Result` shape mirrors
    /// the other SFTP methods for call-site uniformity.
    pub fn remove(&self, path: &str) -> Result<(), TransportError> {
        let _ = self
            .runtime
            .block_on(self.inner.remove_file(path.to_owned()));
        Ok(())
    }

    /// Remove each stale sibling in `dir` (best-effort; failures are ignored so
    /// tidiness never blocks a working bootstrap).
    pub fn prune(&self, dir: &str, names: &[String]) {
        for name in names {
            let path = format!("{dir}/{name}");
            let _ = self.runtime.block_on(self.inner.remove_file(path));
        }
    }
}

fn sftp_err(op: &str, path: &str, host: &str, reason: String) -> TransportError {
    TransportError::Sftp {
        op: op.to_owned(),
        path: path.to_owned(),
        host: host.to_owned(),
        reason,
    }
}

/// Lowercase hex SHA-256 of `bytes`.
fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// A bounded tail of the remote process's stderr, for error reporting.
#[derive(Debug, Default)]
struct StderrTail {
    bytes: Vec<u8>,
}

impl StderrTail {
    /// Cap the retained stderr so a chatty remote cannot grow this unbounded.
    const CAP: usize = 16 * 1024;

    fn push(&mut self, data: &[u8]) {
        self.bytes.extend_from_slice(data);
        if self.bytes.len() > Self::CAP {
            let start = self.bytes.len() - Self::CAP;
            self.bytes.drain(..start);
        }
    }
}

/// The blocking read half of a spawned remote channel.
pub struct ChannelReader {
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.pos < self.buf.len() {
                let n = (self.buf.len() - self.pos).min(out.len());
                out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            // `blocking_recv` is only valid outside a runtime — this reader is
            // owned by the `tomo` crate's plain std reader thread, never a tokio
            // task, so this holds.
            match self.rx.blocking_recv() {
                Some(chunk) => {
                    self.buf = chunk;
                    self.pos = 0;
                }
                None => return Ok(0), // channel closed → EOF
            }
        }
    }
}

/// The blocking write half of a spawned remote channel.
pub struct ChannelWriter {
    tx: mpsc::UnboundedSender<Vec<u8>>,
}

impl Write for ChannelWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.tx
            .send(data.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "remote channel closed"))?;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // The writer task writes each queued buffer to the SSH channel
        // immediately; there is no additional buffering to force out here.
        Ok(())
    }
}

/// Keeps the runtime, session, and bridge tasks alive for the duration of a
/// [`RemoteChannel`]. Dropping it tears the session down.
pub struct RemoteGuard {
    runtime: Option<Runtime>,
    _handle: Handle<Client>,
    stderr: Arc<Mutex<StderrTail>>,
    reader_task: tokio::task::JoinHandle<()>,
    writer_task: tokio::task::JoinHandle<()>,
}

impl RemoteGuard {
    /// The captured tail of the remote process's stderr, decoded lossily —
    /// useful when the session dies unexpectedly (e.g. the remote binary failed
    /// to exec).
    pub fn stderr_tail(&self) -> String {
        self.stderr
            .lock()
            .map(|t| String::from_utf8_lossy(&t.bytes).into_owned())
            .unwrap_or_default()
    }
}

impl Drop for RemoteGuard {
    fn drop(&mut self) {
        // Stop the bridge tasks, then shut the runtime down without blocking on
        // tasks that are parked on the network.
        self.reader_task.abort();
        self.writer_task.abort();
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
    }
}

/// A blocking byte duplex to a spawned remote `serve --stdio`, plus the guard
/// that keeps the SSH session alive.
pub struct RemoteChannel {
    reader: ChannelReader,
    writer: ChannelWriter,
    guard: RemoteGuard,
}

impl RemoteChannel {
    /// Split into the read half, write half, and lifetime guard. The `tomo`
    /// crate boxes the reader/writer into its transport and stores the guard for
    /// the session's lifetime.
    pub fn into_parts(self) -> (ChannelReader, ChannelWriter, RemoteGuard) {
        (self.reader, self.writer, self.guard)
    }
}

/// The russh client handler: its sole job is host-key verification against
/// `known_hosts`.
struct Client {
    host: String,
    port: u16,
    known_hosts: PathBuf,
    verdict: Arc<Mutex<HostKeyVerdict>>,
}

/// The recorded outcome of host-key verification, read back after connect to
/// produce a specific error.
enum HostKeyVerdict {
    Pending,
    Ok,
    Unknown { host: String },
    Mismatch { host: String, line: usize },
    ReadError { host: String, reason: String },
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        let outcome = match russh::keys::check_known_hosts_path(
            &self.host,
            self.port,
            server_public_key,
            &self.known_hosts,
        ) {
            Ok(true) => (HostKeyVerdict::Ok, true),
            Ok(false) => (
                HostKeyVerdict::Unknown {
                    host: self.host.clone(),
                },
                false,
            ),
            Err(russh::keys::Error::KeyChanged { line }) => (
                HostKeyVerdict::Mismatch {
                    host: self.host.clone(),
                    line,
                },
                false,
            ),
            Err(e) => (
                HostKeyVerdict::ReadError {
                    host: self.host.clone(),
                    reason: e.to_string(),
                },
                false,
            ),
        };
        if let Ok(mut v) = self.verdict.lock() {
            *v = outcome.0;
        }
        Ok(outcome.1)
    }
}
