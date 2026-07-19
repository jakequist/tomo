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
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client::{self, Handle};
use russh::keys::agent::client::AgentClient;
use russh::keys::ssh_key::PublicKey;
use russh::keys::{load_secret_key, Algorithm, HashAlg, PrivateKeyWithHashAlg};
use russh::ChannelMsg;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, OpenFlags};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

use crate::bootstrap::{self, BootstrapDecision, BootstrapReport};
use crate::error::TransportError;
use crate::hostspec::DEFAULT_SSH_PORT;
use crate::quote::shell_quote;
use crate::sshconfig::{ResolvedEndpoint, ResolvedRoute, SshConfig, StrictHostKey};
use crate::triple;

/// Options for opening an [`SshSession`].
///
/// These carry the *defaults* and the user's home directory; per-host policy
/// (`HostName`, `User`, `Port`, `IdentityFile`, `StrictHostKeyChecking`,
/// `UserKnownHostsFile`, `ProxyJump`) is resolved from `~/.ssh/config` at connect
/// time via [`resolve_route`].
#[derive(Debug, Clone)]
pub struct SshOpts {
    /// The local login name to use when neither the target nor the config sets one.
    pub default_user: String,
    /// The user's home directory (roots `~`/`%d` expansion and the default config).
    pub home: PathBuf,
    /// Default `known_hosts` path when the config names no `UserKnownHostsFile`.
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
            home: home.to_owned(),
            known_hosts: ssh.join("known_hosts"),
            identity_files: vec![ssh.join("id_ed25519"), ssh.join("id_rsa")],
            connect_timeout: Duration::from_secs(20),
        }
    }
}

/// The `~/.ssh/config` path to consult: the `TOMO_SSH_CONFIG` override when set
/// (test hermeticity and power-user redirection), else the standard
/// `<home>/.ssh/config`. A missing file is tolerated by [`SshConfig::load`].
fn config_path(home: &Path) -> PathBuf {
    std::env::var_os("TOMO_SSH_CONFIG")
        .map_or_else(|| home.join(".ssh").join("config"), PathBuf::from)
}

/// Resolve `target` through `~/.ssh/config` into a full [`ResolvedRoute`]
/// (alias → `HostName`/`User`/`Port`, identity files, host-key policy, and the
/// `ProxyJump` chain). The CLI uses this both to log the resolved endpoint and
/// to drive the connection.
///
/// # Errors
/// [`TransportError::ProxyJump`] if the jump chain is cyclic, too deep, or
/// malformed.
pub fn resolve_route(target: &str, opts: &SshOpts) -> Result<ResolvedRoute, TransportError> {
    let cfg = SshConfig::load(&config_path(&opts.home), &opts.home);
    cfg.resolve_route(target, &opts.home, DEFAULT_SSH_PORT)
        .map_err(|e| TransportError::ProxyJump {
            target: target.to_owned(),
            reason: e.to_string(),
        })
}

/// A live, authenticated SSH session with a running internal runtime. When the
/// route used a `ProxyJump`, the intermediate hop handles are held in `jumps`;
/// dropping them would close the tunnel, so they live as long as the session.
pub struct SshSession {
    runtime: Runtime,
    handle: Handle<Client>,
    host: String,
    /// Jump-host handles kept alive to hold the `direct-tcpip` tunnels open.
    jumps: Vec<Handle<Client>>,
    /// Host-key policy notes (unpinned acceptances, accept-new recordings) for
    /// the CLI to surface; the library never prints.
    notes: Vec<String>,
}

impl SshSession {
    /// Resolve `target` through `~/.ssh/config` and connect over the resulting
    /// route: authenticate each hop (ssh-agent unless `IdentitiesOnly`, then the
    /// configured/​default key files; unencrypted keys only), honour each hop's
    /// host-key policy, and chain `ProxyJump` hops with `direct-tcpip`.
    ///
    /// # Errors
    /// A [`TransportError`] naming the phase that failed (proxy-jump resolution,
    /// connect, host-key, or auth), and for a broken chain naming which hop.
    pub fn connect(target: &str, opts: &SshOpts) -> Result<SshSession, TransportError> {
        let route = resolve_route(target, opts)?;
        Self::connect_route(&route, opts)
    }

    /// Connect over an already-resolved [`ResolvedRoute`] (see [`resolve_route`]).
    ///
    /// # Errors
    /// As for [`SshSession::connect`].
    pub fn connect_route(
        route: &ResolvedRoute,
        opts: &SshOpts,
    ) -> Result<SshSession, TransportError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|source| TransportError::Runtime { source })?;

        let host = route.target.host_name.clone();
        let notes = Arc::new(Mutex::new(Vec::new()));
        let (handle, jumps) =
            runtime.block_on(Self::connect_chain(route, opts, &Arc::clone(&notes)))?;
        let notes = notes.lock().map(|n| n.clone()).unwrap_or_default();

        Ok(SshSession {
            runtime,
            handle,
            host,
            jumps,
            notes,
        })
    }

    /// The host-key policy notes gathered during connect (see [`SshSession::notes`]).
    #[must_use]
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    /// Connect every hop in the route left-to-right. The first hop is a real TCP
    /// connection; each later hop is reached by opening a `direct-tcpip` channel
    /// on the previous hop's session and running a fresh client over that stream.
    /// Returns the target handle plus the (kept-alive) jump handles.
    async fn connect_chain(
        route: &ResolvedRoute,
        opts: &SshOpts,
        notes: &Arc<Mutex<Vec<String>>>,
    ) -> Result<(Handle<Client>, Vec<Handle<Client>>), TransportError> {
        let chain = route.chain();
        let last = chain.len().saturating_sub(1);
        let mut jump_handles: Vec<Handle<Client>> = Vec::new();
        let mut prev: Option<Handle<Client>> = None;

        for (i, ep) in chain.iter().enumerate() {
            let is_target = i == last;
            let verdict = Arc::new(Mutex::new(HostKeyVerdict::Pending));
            // Every file consulted for this hop: the hop's user known-hosts
            // followed by the always-appended global set (lookup only).
            let lookup_files = ep.lookup_known_hosts();
            // Bias host-key-algorithm negotiation toward the types already
            // recorded for this hop (mirroring OpenSSH), so a known_hosts entry
            // whose key type differs from russh's default order still matches.
            // Unpinned (`no`) hops skip the scan; unknown hosts keep the full
            // default set so accept-new/no negotiate normally.
            let known_algos = if ep.strict == StrictHostKey::No {
                Vec::new()
            } else {
                known_key_algos(&lookup_files, &ep.host_name, ep.port)
            };
            let config = Arc::new(client::Config {
                inactivity_timeout: Some(Duration::from_hours(1)),
                keepalive_interval: Some(Duration::from_secs(30)),
                preferred: russh::Preferred {
                    key: preferred_key_order(&known_algos),
                    ..russh::Preferred::DEFAULT
                },
                ..Default::default()
            });
            let handler = build_handler(ep, lookup_files, Arc::clone(&verdict), Arc::clone(notes));

            let mut handle = if let Some(prev_handle) = prev.take() {
                // Later hop: tunnel through the previous session.
                let stream = match prev_handle
                    .channel_open_direct_tcpip(
                        ep.host_name.clone(),
                        u32::from(ep.port),
                        "127.0.0.1",
                        0,
                    )
                    .await
                {
                    Ok(ch) => ch.into_stream(),
                    Err(e) => {
                        return Err(TransportError::JumpConnect {
                            hop: hop_label(ep),
                            reason: format!("opening tunnel through the previous hop: {e}"),
                        })
                    }
                };
                // Keep the previous hop alive for the tunnel's lifetime.
                jump_handles.push(prev_handle);
                match client::connect_stream(Arc::clone(&config), stream, handler).await {
                    Ok(h) => h,
                    Err(e) => {
                        if let Some(err) = host_key_error(&verdict) {
                            return Err(err);
                        }
                        return Err(TransportError::JumpConnect {
                            hop: hop_label(ep),
                            reason: e.to_string(),
                        });
                    }
                }
            } else {
                // First hop: a real TCP connection.
                let connect = client::connect(
                    Arc::clone(&config),
                    (ep.host_name.as_str(), ep.port),
                    handler,
                );
                match tokio::time::timeout(opts.connect_timeout, connect).await {
                    Ok(Ok(h)) => h,
                    Ok(Err(e)) => {
                        if let Some(err) = host_key_error(&verdict) {
                            return Err(err);
                        }
                        return Err(connect_error(ep, is_target, e));
                    }
                    Err(_) => {
                        return Err(connect_error(
                            ep,
                            is_target,
                            russh::Error::ConnectionTimeout,
                        ))
                    }
                }
            };

            let user = ep.user.clone().unwrap_or_else(|| opts.default_user.clone());
            authenticate(&mut handle, ep, &user, opts).await?;
            prev = Some(handle);
        }

        let handle = prev.ok_or_else(|| TransportError::ProxyJump {
            target: route.target.alias.clone(),
            reason: "empty connection chain".to_owned(),
        })?;
        Ok((handle, jump_handles))
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
            jumps,
            notes,
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
                _jumps: jumps,
                notes,
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
    /// Jump-host handles held open for the tunnel's lifetime (empty for a direct
    /// connection); their `Drop` closes the `direct-tcpip` chain.
    _jumps: Vec<Handle<Client>>,
    /// Host-key policy notes gathered during connect, forwarded from the session.
    notes: Vec<String>,
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

    /// The host-key policy notes gathered during connect (unpinned acceptances,
    /// accept-new recordings) — the CLI surfaces these; the library never prints.
    #[must_use]
    pub fn notes(&self) -> &[String] {
        &self.notes
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

/// Build the per-hop russh handler. `lookup_files` is the full consulted set
/// (user + global); recording targets only the hop's first writable user file.
fn build_handler(
    ep: &ResolvedEndpoint,
    lookup_files: Vec<PathBuf>,
    verdict: Arc<Mutex<HostKeyVerdict>>,
    notes: Arc<Mutex<Vec<String>>>,
) -> Client {
    // Human-readable list of every file consulted, for the not-found error.
    let lookup_display = lookup_files
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Client {
        host: ep.host_name.clone(),
        port: ep.port,
        known_hosts_files: lookup_files,
        lookup_display,
        strict: ep.strict,
        // Recording (accept-new) never touches the global set — only the hop's
        // first non-/dev/null user file.
        record_target: ep.record_target(),
        verdict,
        notes,
    }
}

/// The host-key negotiation algorithms already recorded for `host:port` across
/// the hop's `known_hosts` files, mapped to russh's negotiation identifiers.
///
/// This reuses russh's own `known_host_keys_path` line parser (which already
/// handles plain, `[host]:port`, comma-separated, and hashed `|1|salt|hash`
/// entries and yields the recorded [`PublicKey`]s), so no bespoke parser or
/// extra HMAC/SHA-1 dependencies are needed. An `ssh-rsa` entry expands to the
/// whole RSA family (`rsa-sha2-512`/`-256` and legacy `ssh-rsa`) so any
/// RSA-based negotiation the server offers still matches the recorded key.
/// Duplicates are removed, preserving first-seen order.
fn known_key_algos(files: &[PathBuf], host: &str, port: u16) -> Vec<Algorithm> {
    // Port-form types first; if the non-22 lookup finds none, fall back to the
    // plain (port-less) host form — the same OpenSSH compatibility the
    // verification path applies (see `lookup_host_key`), so the negotiation is
    // still biased toward the key type recorded under the plain entry.
    let primary = known_key_algos_form(files, host, port);
    if port != DEFAULT_SSH_PORT && primary.is_empty() {
        return known_key_algos_form(files, host, DEFAULT_SSH_PORT);
    }
    primary
}

/// The recorded key types for a single host form (see [`known_key_algos`]).
fn known_key_algos_form(files: &[PathBuf], host: &str, port: u16) -> Vec<Algorithm> {
    let mut out: Vec<Algorithm> = Vec::new();
    let push = |algo: Algorithm, out: &mut Vec<Algorithm>| {
        if !out.contains(&algo) {
            out.push(algo);
        }
    };
    for file in files {
        let Ok(keys) = russh::keys::known_hosts::known_host_keys_path(host, port, file) else {
            continue;
        };
        for (_line, key) in keys {
            match key.algorithm() {
                Algorithm::Rsa { .. } => {
                    push(
                        Algorithm::Rsa {
                            hash: Some(HashAlg::Sha512),
                        },
                        &mut out,
                    );
                    push(
                        Algorithm::Rsa {
                            hash: Some(HashAlg::Sha256),
                        },
                        &mut out,
                    );
                    push(Algorithm::Rsa { hash: None }, &mut out);
                }
                other => push(other, &mut out),
            }
        }
    }
    out
}

/// The per-hop host-key algorithm preference: the recorded types first (so the
/// server offers a key type we already trust), then russh's remaining defaults.
/// The set is never shrunk — an empty `known` (unknown host / `no` policy)
/// yields the full default order so accept-new and unpinned hops still work.
fn preferred_key_order(known: &[Algorithm]) -> std::borrow::Cow<'static, [Algorithm]> {
    if known.is_empty() {
        return russh::Preferred::DEFAULT.key.clone();
    }
    let mut ordered: Vec<Algorithm> = known.to_vec();
    for algo in russh::Preferred::DEFAULT.key.iter() {
        if !ordered.contains(algo) {
            ordered.push(algo.clone());
        }
    }
    std::borrow::Cow::Owned(ordered)
}

/// Authenticate one hop: ssh-agent first (unless `IdentitiesOnly yes`), then the
/// endpoint's config identities followed by the caller's default key files,
/// unencrypted only. The identity order preserves the caller's list (which the
/// CLI already builds as `--identity` → config → defaults) and appends any
/// endpoint-specific keys not already present.
async fn authenticate(
    handle: &mut Handle<Client>,
    ep: &ResolvedEndpoint,
    user: &str,
    opts: &SshOpts,
) -> Result<(), TransportError> {
    let mut tried: Vec<String> = Vec::new();

    if ep.identities_only {
        tried.push("ssh-agent (skipped: IdentitiesOnly yes)".to_owned());
    } else {
        match SshSession::auth_agent(handle, user).await {
            Ok(true) => return Ok(()),
            Ok(false) => tried.push("ssh-agent (no identity accepted)".to_owned()),
            Err(reason) => tried.push(format!("ssh-agent ({reason})")),
        }
    }

    // De-duplicated identity list: caller defaults first, then endpoint keys.
    let mut identities: Vec<PathBuf> = Vec::new();
    for path in opts.identity_files.iter().chain(ep.identity_files.iter()) {
        if !identities.contains(path) {
            identities.push(path.clone());
        }
    }

    for path in &identities {
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
                // Passphrase-protected keys are out of scope for v0; say so
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
        host: ep.host_name.clone(),
        detail: tried.join("; "),
    })
}

/// Translate a recorded host-key verdict into the specific error, if any.
fn host_key_error(verdict: &Arc<Mutex<HostKeyVerdict>>) -> Option<TransportError> {
    let v = verdict.lock().ok()?;
    match &*v {
        HostKeyVerdict::Unknown {
            lookup,
            host,
            files,
        } => Some(TransportError::HostKeyUnknown {
            lookup: lookup.clone(),
            host: host.clone(),
            files: files.clone(),
        }),
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

/// The first-hop connection error: a `Connect` for the direct target, or a
/// `JumpConnect` naming the hop when the first hop is a jump.
fn connect_error(ep: &ResolvedEndpoint, is_target: bool, e: russh::Error) -> TransportError {
    if is_target {
        TransportError::Connect {
            host: format!("{}:{}", ep.host_name, ep.port),
            source: Box::new(e),
        }
    } else {
        TransportError::JumpConnect {
            hop: hop_label(ep),
            reason: e.to_string(),
        }
    }
}

/// A human label for a hop in an error message: `alias (host:port)`.
fn hop_label(ep: &ResolvedEndpoint) -> String {
    format!("{} ({}:{})", ep.alias, ep.host_name, ep.port)
}

/// The result of looking a server key up across a hop's `known_hosts` files.
#[derive(Debug, Clone, PartialEq, Eq)]
enum KnownHostsLookup {
    /// A recorded key matches.
    Match,
    /// No file records this host.
    NotFound,
    /// A file records this host with a *different* key (possible MITM).
    Changed { line: usize },
    /// A file could not be read/parsed (missing files are `NotFound`, not this).
    ReadError { reason: String },
}

/// What to do about a server key, given the lookup result and the host-key policy.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HostKeyDecision {
    /// Accept and do not record.
    Accept,
    /// Accept and record the key into the hop's writable `known_hosts`.
    AcceptAndRecord,
    /// Reject the connection.
    Reject(RejectReason),
}

/// Why a host key was rejected, mapped to the matching [`TransportError`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum RejectReason {
    Unknown,
    Changed { line: usize },
    ReadError { reason: String },
}

/// The pure host-key policy decision (unit-tested exhaustively). `no` accepts
/// anything; `accept-new` records unknown keys but rejects changed ones;
/// `yes` (and `ask`) rejects both unknown and changed.
fn decide_host_key(lookup: &KnownHostsLookup, strict: StrictHostKey) -> HostKeyDecision {
    match lookup {
        KnownHostsLookup::Match => HostKeyDecision::Accept,
        KnownHostsLookup::NotFound => match strict {
            StrictHostKey::No => HostKeyDecision::Accept,
            StrictHostKey::AcceptNew => HostKeyDecision::AcceptAndRecord,
            StrictHostKey::Yes => HostKeyDecision::Reject(RejectReason::Unknown),
        },
        KnownHostsLookup::Changed { line } => match strict {
            StrictHostKey::No => HostKeyDecision::Accept,
            StrictHostKey::Yes | StrictHostKey::AcceptNew => {
                HostKeyDecision::Reject(RejectReason::Changed { line: *line })
            }
        },
        KnownHostsLookup::ReadError { reason } => match strict {
            StrictHostKey::No => HostKeyDecision::Accept,
            StrictHostKey::Yes | StrictHostKey::AcceptNew => {
                HostKeyDecision::Reject(RejectReason::ReadError {
                    reason: reason.clone(),
                })
            }
        },
    }
}

/// Look a server key up across every configured `known_hosts` file under a
/// **single** host form (the port is baked into russh's lookup key: `[host]:port`
/// for a non-22 port, else the bare `host`). Aggregates the strongest signal:
/// any `Match` wins; otherwise a `Changed` beats a `ReadError` beats `NotFound`.
/// `/dev/null` and missing files yield `NotFound`.
fn aggregate_lookup(files: &[PathBuf], host: &str, port: u16, key: &PublicKey) -> KnownHostsLookup {
    let mut changed: Option<usize> = None;
    let mut read_error: Option<String> = None;
    for f in files {
        match russh::keys::check_known_hosts_path(host, port, key, f) {
            Ok(true) => return KnownHostsLookup::Match,
            Ok(false) => {}
            Err(russh::keys::Error::KeyChanged { line }) => changed = changed.or(Some(line)),
            Err(e) => read_error = read_error.or_else(|| Some(e.to_string())),
        }
    }
    if let Some(line) = changed {
        KnownHostsLookup::Changed { line }
    } else if let Some(reason) = read_error {
        KnownHostsLookup::ReadError { reason }
    } else {
        KnownHostsLookup::NotFound
    }
}

/// Look a server key up with OpenSSH's port fallback. For a non-22 port the
/// `[host]:port` form is tried first; a `Match`/`Changed`/`ReadError` there
/// always takes precedence. Only when it yields `NotFound` do we retry the
/// *same* files with the plain (port-less) `host` form — mirroring OpenSSH's
/// "found matching key w/out port" compatibility with entries recorded before
/// port-qualified `known_hosts` lines existed. A plain-form `Match` returns
/// `Match` with the flag set (so the caller can note the compat path); a
/// plain-form `Changed` participates as a full `Changed`.
///
/// Returns `(outcome, matched_without_port)`.
fn lookup_host_key(
    files: &[PathBuf],
    host: &str,
    port: u16,
    key: &PublicKey,
) -> (KnownHostsLookup, bool) {
    let primary = aggregate_lookup(files, host, port, key);
    if port != DEFAULT_SSH_PORT && matches!(primary, KnownHostsLookup::NotFound) {
        match aggregate_lookup(files, host, DEFAULT_SSH_PORT, key) {
            KnownHostsLookup::Match => return (KnownHostsLookup::Match, true),
            changed @ KnownHostsLookup::Changed { .. } => return (changed, false),
            // A plain-form NotFound/ReadError does not improve on the primary.
            KnownHostsLookup::NotFound | KnownHostsLookup::ReadError { .. } => {}
        }
    }
    (primary, false)
}

/// The russh client handler for one hop: it verifies the server's host key
/// against the hop's `known_hosts` files under its `StrictHostKeyChecking`
/// policy, recording newly-seen keys for `accept-new`.
struct Client {
    host: String,
    port: u16,
    known_hosts_files: Vec<PathBuf>,
    /// Comma-joined display of `known_hosts_files`, for the not-found error.
    lookup_display: String,
    strict: StrictHostKey,
    record_target: Option<PathBuf>,
    verdict: Arc<Mutex<HostKeyVerdict>>,
    notes: Arc<Mutex<Vec<String>>>,
}

impl Client {
    /// Append a host-key policy note for the CLI to surface later.
    fn note(&self, msg: String) {
        if let Ok(mut n) = self.notes.lock() {
            n.push(msg);
        }
    }
}

/// The recorded outcome of host-key verification, read back after connect to
/// produce a specific error.
enum HostKeyVerdict {
    Pending,
    Ok,
    Unknown {
        /// The exact lookup key (`[host]:port` for a non-22 port, else `host`).
        lookup: String,
        /// The bare host, for the `ssh <host>` suggestion.
        host: String,
        /// Every known-hosts file consulted, comma-joined.
        files: String,
    },
    Mismatch {
        host: String,
        line: usize,
    },
    ReadError {
        host: String,
        reason: String,
    },
}

/// The known-hosts lookup key(s) for `host:port`, for the not-found error. For a
/// non-default port both forms are tried (the `[host]:port` form, then the
/// plain-host OpenSSH-compat fallback), so both are named; port 22 uses the bare
/// host alone.
fn lookup_key(host: &str, port: u16) -> String {
    if port == DEFAULT_SSH_PORT {
        host.to_owned()
    } else {
        format!("[{host}]:{port} (and {host} without port)")
    }
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        let (lookup, matched_without_port) = lookup_host_key(
            &self.known_hosts_files,
            &self.host,
            self.port,
            server_public_key,
        );
        let (verdict, accept) = match decide_host_key(&lookup, self.strict) {
            HostKeyDecision::Accept => {
                match &lookup {
                    // Pinned match via the port-less fallback: surface the compat path.
                    KnownHostsLookup::Match if matched_without_port => self.note(format!(
                        "using known_hosts entry for {} without a port (OpenSSH compat)",
                        self.host
                    )),
                    // A normal pinned match is silent.
                    KnownHostsLookup::Match => {}
                    // StrictHostKeyChecking no accepting an unpinned key.
                    _ => self.note(format!(
                        "accepting unverified host key for {} (StrictHostKeyChecking no)",
                        self.host
                    )),
                }
                (HostKeyVerdict::Ok, true)
            }
            HostKeyDecision::AcceptAndRecord => {
                match &self.record_target {
                    Some(path) => match russh::keys::known_hosts::learn_known_hosts_path(
                        &self.host,
                        self.port,
                        server_public_key,
                        path,
                    ) {
                        Ok(()) => self.note(format!(
                            "recorded new host key for {} in {} (accept-new)",
                            self.host,
                            path.display()
                        )),
                        Err(e) => {
                            self.note(format!("could not record host key for {}: {e}", self.host));
                        }
                    },
                    None => self.note(format!(
                        "accepting new host key for {} (not recorded: known_hosts is /dev/null)",
                        self.host
                    )),
                }
                (HostKeyVerdict::Ok, true)
            }
            HostKeyDecision::Reject(RejectReason::Unknown) => (
                HostKeyVerdict::Unknown {
                    lookup: lookup_key(&self.host, self.port),
                    host: self.host.clone(),
                    files: self.lookup_display.clone(),
                },
                false,
            ),
            HostKeyDecision::Reject(RejectReason::Changed { line }) => (
                HostKeyVerdict::Mismatch {
                    host: self.host.clone(),
                    line,
                },
                false,
            ),
            HostKeyDecision::Reject(RejectReason::ReadError { reason }) => (
                HostKeyVerdict::ReadError {
                    host: self.host.clone(),
                    reason,
                },
                false,
            ),
        };
        if let Ok(mut v) = self.verdict.lock() {
            *v = verdict;
        }
        Ok(accept)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // panics are fine in tests
mod tests {
    use super::*;

    fn changed(line: usize) -> KnownHostsLookup {
        KnownHostsLookup::Changed { line }
    }

    #[test]
    fn known_key_always_accepted() {
        for strict in [
            StrictHostKey::Yes,
            StrictHostKey::No,
            StrictHostKey::AcceptNew,
        ] {
            assert_eq!(
                decide_host_key(&KnownHostsLookup::Match, strict),
                HostKeyDecision::Accept
            );
        }
    }

    #[test]
    fn unknown_key_policy() {
        assert_eq!(
            decide_host_key(&KnownHostsLookup::NotFound, StrictHostKey::Yes),
            HostKeyDecision::Reject(RejectReason::Unknown)
        );
        assert_eq!(
            decide_host_key(&KnownHostsLookup::NotFound, StrictHostKey::No),
            HostKeyDecision::Accept
        );
        assert_eq!(
            decide_host_key(&KnownHostsLookup::NotFound, StrictHostKey::AcceptNew),
            HostKeyDecision::AcceptAndRecord
        );
    }

    #[test]
    fn changed_key_policy() {
        // Only `no` tolerates a changed key; yes/accept-new both reject it.
        assert_eq!(
            decide_host_key(&changed(7), StrictHostKey::No),
            HostKeyDecision::Accept
        );
        assert_eq!(
            decide_host_key(&changed(7), StrictHostKey::Yes),
            HostKeyDecision::Reject(RejectReason::Changed { line: 7 })
        );
        assert_eq!(
            decide_host_key(&changed(7), StrictHostKey::AcceptNew),
            HostKeyDecision::Reject(RejectReason::Changed { line: 7 })
        );
    }

    #[test]
    fn read_error_policy() {
        let err = KnownHostsLookup::ReadError {
            reason: "boom".to_owned(),
        };
        assert_eq!(
            decide_host_key(&err, StrictHostKey::No),
            HostKeyDecision::Accept
        );
        assert!(matches!(
            decide_host_key(&err, StrictHostKey::Yes),
            HostKeyDecision::Reject(RejectReason::ReadError { .. })
        ));
    }

    // ---- known_key_algos (host-key-algorithm negotiation biasing) ------------
    //
    // Real public-key blobs harvested from `ssh-keygen`; the hashed line's HMAC
    // salt/hash are for host "testhost" (default port 22).
    const ED: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFIbuJpjslAel+fV5WBhVIXjkRGmo9U3eZxBDW6Ff2Dg";
    // A *different* ed25519 key (same algorithm), for mismatch tests.
    const ED2: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKMEgocw2CgPF9xKhgwL9rfVv91/qTbNSOUUE7L3Ri4K";
    const EC: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBPe9y855GQ2U52Qm5YNs4cfa+PZuLqlKzbEUjBIXZSVCdfAZ+soW+5Vm2xSBv2alitoMyodYLNx/6XCNvAB0Pio=";
    const RSA: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQCv6IlflkBEkXVD45GTTEGr0BgEPIb2wXUwdUrFJTJpCCT1NZwmFk6tqXlG1k2ktrqqzQr0G2sHrPpjgtM1v9YlcNqJRxJFzBVmRWiL52XbqoIwCrdMWhC4uY5XSsSiiTxMO8kzghbkuh/XFMMmZbRfRLHtXY9TH3js6KH9WJTN6fv4XY2wEOBE4E2Ljd5ikoYBRHBl6KRQnSCBgvQJ9EDMqYDkFC2S2RAuDvA+UIsFg90B7iCxFUIhEUCU4PvHfFCbuQhtCcM9C0hu1R4+CCO8lfU1LaQZqWbEYOgRqr4TIqi/FxTDPdQjpOzGNrAopE2pww09ALFHpUYJVRShJ9NF";
    const HASHED_ED_TESTHOST: &str = "|1|cCf/KULfkADNhFLwZNrxAjyW+f8=|Z8qHwzJ4I/dc0Eu5wMJYEX28Ltc= ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFIbuJpjslAel+fV5WBhVIXjkRGmo9U3eZxBDW6Ff2Dg";

    fn ec256() -> Algorithm {
        Algorithm::Ecdsa {
            curve: russh::keys::EcdsaCurve::NistP256,
        }
    }

    fn write_kh(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        std::fs::write(&path, contents).unwrap();
        (dir, path)
    }

    #[test]
    fn algos_plain_ed25519() {
        let (_d, kh) = write_kh(&format!("testhost {ED}\n"));
        assert_eq!(
            known_key_algos(&[kh], "testhost", 22),
            vec![Algorithm::Ed25519]
        );
    }

    #[test]
    fn algos_hashed_line_matches_via_hmac() {
        let (_d, kh) = write_kh(&format!("{HASHED_ED_TESTHOST}\n"));
        assert_eq!(
            known_key_algos(&[kh], "testhost", 22),
            vec![Algorithm::Ed25519]
        );
    }

    #[test]
    fn algos_bracketed_host_port_form() {
        // A non-standard port is recorded as `[host]:port`.
        let (_d, kh) = write_kh(&format!("[testhost]:2222 {EC}\n"));
        assert_eq!(
            known_key_algos(std::slice::from_ref(&kh), "testhost", 2222),
            vec![ec256()]
        );
        // The same host on the default port must NOT match the :2222 entry.
        assert!(known_key_algos(&[kh], "testhost", 22).is_empty());
    }

    #[test]
    fn algos_multiple_types_in_order() {
        let (_d, kh) = write_kh(&format!("testhost {ED}\ntesthost {EC}\n"));
        assert_eq!(
            known_key_algos(&[kh], "testhost", 22),
            vec![Algorithm::Ed25519, ec256()]
        );
    }

    #[test]
    fn algos_ssh_rsa_expands_to_rsa_family() {
        let (_d, kh) = write_kh(&format!("testhost {RSA}\n"));
        assert_eq!(
            known_key_algos(&[kh], "testhost", 22),
            vec![
                Algorithm::Rsa {
                    hash: Some(HashAlg::Sha512)
                },
                Algorithm::Rsa {
                    hash: Some(HashAlg::Sha256)
                },
                Algorithm::Rsa { hash: None },
            ]
        );
    }

    #[test]
    fn algos_no_match_is_empty() {
        let (_d, kh) = write_kh(&format!("otherhost {ED}\n"));
        assert!(known_key_algos(&[kh], "testhost", 22).is_empty());
        // Also empty for a missing file.
        assert!(known_key_algos(&[PathBuf::from("/nonexistent/known_hosts")], "h", 22).is_empty());
    }

    #[test]
    fn algos_dedup_across_lines() {
        let (_d, kh) = write_kh(&format!("testhost {ED}\ntesthost {ED}\n"));
        assert_eq!(
            known_key_algos(&[kh], "testhost", 22),
            vec![Algorithm::Ed25519]
        );
    }

    #[test]
    fn lookup_key_names_both_forms_for_non_default_port() {
        assert_eq!(lookup_key("p1", 22), "p1");
        assert_eq!(lookup_key("p1", 25601), "[p1]:25601 (and p1 without port)");
    }

    // The known_hosts *verification* path (russh's check_known_hosts_path, the
    // same matcher known_key_algos uses) must follow OpenSSH's port-form rule:
    // a plain `host` entry matches only port 22; a `[host]:port` entry matches
    // only that port. These prove the p1 root cause and its fix.
    #[test]
    fn verification_plain_entry_matches_only_port_22() {
        let (_d, kh) = write_kh(&format!("testhost {ED}\n"));
        let key = PublicKey::from_openssh(ED).unwrap();
        assert!(russh::keys::check_known_hosts_path("testhost", 22, &key, &kh).unwrap());
        // A plain entry does NOT match a non-22 lookup (the p1 failure).
        assert!(!russh::keys::check_known_hosts_path("testhost", 25601, &key, &kh).unwrap());
    }

    #[test]
    fn verification_bracketed_entry_matches_only_that_port() {
        let (_d, kh) = write_kh(&format!("[testhost]:25601 {ED}\n"));
        let key = PublicKey::from_openssh(ED).unwrap();
        assert!(russh::keys::check_known_hosts_path("testhost", 25601, &key, &kh).unwrap());
        assert!(!russh::keys::check_known_hosts_path("testhost", 22, &key, &kh).unwrap());
    }

    // ---- OpenSSH port-less fallback (lookup_host_key) ------------------------

    #[test]
    fn fallback_plain_entry_matches_non_default_port() {
        // A plain `testhost` entry authenticates a testhost:25601 connection via
        // the port-less fallback, with the compat flag set (the p1 fix).
        let (_d, kh) = write_kh(&format!("testhost {ED}\n"));
        let key = PublicKey::from_openssh(ED).unwrap();
        let (outcome, without_port) = lookup_host_key(&[kh], "testhost", 25601, &key);
        assert_eq!(outcome, KnownHostsLookup::Match);
        assert!(
            without_port,
            "should have matched via the port-less fallback"
        );
    }

    #[test]
    fn fallback_plain_entry_wrong_key_is_mismatch() {
        // A plain entry whose recorded key differs participates fully as a
        // Mismatch (as in OpenSSH), not silently ignored.
        let (_d, kh) = write_kh(&format!("testhost {ED}\n"));
        let other = PublicKey::from_openssh(ED2).unwrap();
        let (outcome, without_port) = lookup_host_key(&[kh], "testhost", 25601, &other);
        assert!(matches!(outcome, KnownHostsLookup::Changed { .. }));
        assert!(!without_port);
    }

    #[test]
    fn fallback_port_form_takes_precedence_over_plain() {
        // Both a matching [host]:port entry and a *conflicting* plain entry: the
        // port-form Match wins and no fallback is consulted.
        let (_d, kh) = write_kh(&format!("[testhost]:25601 {ED}\ntesthost {ED2}\n"));
        let key = PublicKey::from_openssh(ED).unwrap();
        let (outcome, without_port) = lookup_host_key(&[kh], "testhost", 25601, &key);
        assert_eq!(outcome, KnownHostsLookup::Match);
        assert!(
            !without_port,
            "port-form match must not set the fallback flag"
        );
    }

    #[test]
    fn fallback_not_applied_for_port_22() {
        // Port 22 is already the plain form; a genuinely-unknown key stays
        // NotFound with no second lookup.
        let (_d, kh) = write_kh(&format!("otherhost {ED}\n"));
        let key = PublicKey::from_openssh(ED).unwrap();
        let (outcome, without_port) = lookup_host_key(&[kh], "testhost", 22, &key);
        assert_eq!(outcome, KnownHostsLookup::NotFound);
        assert!(!without_port);
    }

    #[test]
    fn fallback_unknown_when_neither_form_present() {
        let (_d, kh) = write_kh(&format!("otherhost {ED}\n"));
        let key = PublicKey::from_openssh(ED).unwrap();
        let (outcome, without_port) = lookup_host_key(&[kh], "testhost", 25601, &key);
        assert_eq!(outcome, KnownHostsLookup::NotFound);
        assert!(!without_port);
    }

    #[test]
    fn algos_plain_entry_found_for_non_default_port_via_fallback() {
        // A plain entry contributes its types to a non-22 lookup through the
        // port-less fallback (so negotiation is still biased correctly).
        let (_d, kh) = write_kh(&format!("testhost {EC}\n"));
        assert_eq!(
            known_key_algos(std::slice::from_ref(&kh), "testhost", 25601),
            vec![ec256()]
        );
        assert_eq!(known_key_algos(&[kh], "testhost", 22), vec![ec256()]);
    }

    #[test]
    fn preferred_order_puts_known_first_then_defaults() {
        // ecdsa-only recorded ⇒ ecdsa-nistp256 leads; the rest of the default
        // set follows, and nothing is dropped.
        let ordered = preferred_key_order(&[ec256()]);
        assert_eq!(ordered.first(), Some(&ec256()));
        assert!(ordered.contains(&Algorithm::Ed25519));
        assert_eq!(ordered.len(), russh::Preferred::DEFAULT.key.len());
        // Empty ⇒ the untouched default order.
        assert_eq!(
            preferred_key_order(&[]).as_ref(),
            russh::Preferred::DEFAULT.key.as_ref()
        );
    }
}
