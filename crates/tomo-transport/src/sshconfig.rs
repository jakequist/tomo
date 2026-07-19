//! A minimal, **pure** `ssh_config(5)` reader and host resolver.
//!
//! Tomo authenticates and connects over SSH like a user's system `ssh` already
//! does. The [`SshOpts`](crate::SshOpts) defaults only know the two built-in key
//! names, the default port, and `~/.ssh/known_hosts`; a user whose `~/.ssh/config`
//! selects a differently-named key, rewrites an alias to a real `HostName`,
//! reaches the host through a `ProxyJump`, or relaxes host-key checking would
//! have `ssh host` work while `tomo sync host` failed. This module closes that
//! gap by resolving the directives that decide *where* and *how* to connect.
//!
//! It is deliberately **not** a full `ssh_config` implementation. The parser is
//! pure — text in, structured data out — so it is exhaustively unit-testable and
//! reads no environment for policy (the CLI/adapter supplies the file path and
//! the home directory). The only I/O lives in [`SshConfig::load`], which resolves
//! `Include` directives against the filesystem; everything else operates on an
//! already-loaded line list.
//!
//! ## Supported options (first-obtained-wins per `ssh_config` semantics)
//! - `Host` pattern blocks (whitespace-separated globs with `*`/`?`, `!`
//!   negation) and the global (pre-`Host`) section.
//! - `HostName` (alias → real host; **literal only** — `%h`/other tokens are not
//!   substituted, which is rare in practice), `User`, `Port`.
//! - `IdentityFile` (accumulated in order) and `IdentitiesOnly`.
//! - `StrictHostKeyChecking` (`yes`/`no`/`accept-new`/`ask`; `ask` collapses to
//!   `yes` because Tomo is non-interactive).
//! - `UserKnownHostsFile` (one or more paths; default `~/.ssh/known_hosts` +
//!   `~/.ssh/known_hosts2`) and `GlobalKnownHostsFile` (default
//!   `/etc/ssh/ssh_known_hosts` + `…_known_hosts2`, consulted for lookup only).
//! - `ProxyJump` (comma-separated `[user@]host[:port]` chain, each hop itself
//!   resolved recursively; `none` disables).
//! - `Include` (glob-expanded, processed in place — see [`SshConfig::load`]).
//! - Every other keyword is ignored, but its name is collected so a caller can
//!   surface a single debug line listing what went unhandled. `Match` blocks are
//!   treated as never-applicable (their directives are never attributed to a
//!   host) rather than evaluated.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// OpenSSH's default global known-hosts files, consulted for lookup only when a
/// host declares no `GlobalKnownHostsFile`. Missing files are simply "no
/// entries" (the common case).
const DEFAULT_GLOBAL_KNOWN_HOSTS: &[&str] =
    &["/etc/ssh/ssh_known_hosts", "/etc/ssh/ssh_known_hosts2"];

/// Maximum `ProxyJump` / `Include` recursion depth before we refuse (cycle-cap
/// backstop even when the visited-set guard would already catch a true loop).
const MAX_DEPTH: usize = 8;

/// The host-key checking policy for a host, mirroring OpenSSH's
/// `StrictHostKeyChecking`. `ask` is folded into [`StrictHostKey::Yes`] because
/// Tomo runs non-interactively and cannot prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StrictHostKey {
    /// Reject unknown or changed keys. OpenSSH's default; `ask` collapses here.
    #[default]
    Yes,
    /// Accept any key and never record it — fully unpinned.
    No,
    /// Accept and record a previously-unknown key, but reject a *changed* one.
    AcceptNew,
}

impl StrictHostKey {
    /// Parse a `StrictHostKeyChecking` argument. Unknown values yield `None` so
    /// the directive is ignored (leaving the default), matching `ssh`'s lenience.
    fn parse(arg: &str) -> Option<StrictHostKey> {
        match arg.trim().to_ascii_lowercase().as_str() {
            "yes" | "ask" => Some(StrictHostKey::Yes),
            "no" | "off" => Some(StrictHostKey::No),
            "accept-new" => Some(StrictHostKey::AcceptNew),
            _ => None,
        }
    }
}

/// A fully-resolved connection endpoint: one hop of a route (a jump host or the
/// final target), with every directive that applies to it already decided.
#[derive(Debug, Clone)]
pub struct ResolvedEndpoint {
    /// The alias as named by the user or `ProxyJump` (used in logs and errors).
    pub alias: String,
    /// The host actually connected to (`HostName` if set, else the alias).
    pub host_name: String,
    /// The effective TCP port.
    pub port: u16,
    /// The login user, if the target or config named one; `None` → caller default.
    pub user: Option<String>,
    /// Config-declared identity files for this host, tilde/`%d`-expanded, in order.
    pub identity_files: Vec<PathBuf>,
    /// `IdentitiesOnly yes` → do not offer ssh-agent keys for this hop.
    pub identities_only: bool,
    /// The host-key policy for this hop.
    pub strict: StrictHostKey,
    /// The **user** known-hosts files (`UserKnownHostsFile`, or the OpenSSH
    /// default `~/.ssh/known_hosts` + `~/.ssh/known_hosts2` when unset), in
    /// order. These are consulted for lookup *and* are the only recording
    /// targets for `accept-new`. `/dev/null` is preserved verbatim.
    pub known_hosts_files: Vec<PathBuf>,
    /// The **global** known-hosts files (`GlobalKnownHostsFile`, or the OpenSSH
    /// default `/etc/ssh/ssh_known_hosts` + `…_known_hosts2` when unset), in
    /// order. Consulted for **lookup only** — never recorded into.
    pub global_known_hosts_files: Vec<PathBuf>,
}

impl ResolvedEndpoint {
    /// Every known-hosts file consulted for this hop, user files first then
    /// global — the set used for both host-key verification and the
    /// host-key-algorithm preference scan.
    #[must_use]
    pub fn lookup_known_hosts(&self) -> Vec<PathBuf> {
        let mut files = self.known_hosts_files.clone();
        files.extend(self.global_known_hosts_files.iter().cloned());
        files
    }

    /// The file `accept-new` records a newly-seen key into: the first user file
    /// that is not `/dev/null`. `None` means nothing is ever recorded (e.g. the
    /// only user file is `/dev/null`).
    #[must_use]
    pub fn record_target(&self) -> Option<PathBuf> {
        self.known_hosts_files
            .iter()
            .find(|f| f.as_os_str() != "/dev/null")
            .cloned()
    }
}

/// A fully-resolved route to a target: the ordered jump chain (first hop first)
/// followed by the destination, plus any unhandled option names seen along the
/// way (for a one-time debug line).
#[derive(Debug, Clone)]
pub struct ResolvedRoute {
    /// Jump hosts in connection order (empty for a direct connection).
    pub jumps: Vec<ResolvedEndpoint>,
    /// The final destination endpoint.
    pub target: ResolvedEndpoint,
    /// Names of ignored/unknown options encountered while resolving.
    pub unknown_options: Vec<String>,
}

impl ResolvedRoute {
    /// The full connection chain, jumps first and the target last.
    #[must_use]
    pub fn chain(&self) -> Vec<&ResolvedEndpoint> {
        self.jumps
            .iter()
            .chain(std::iter::once(&self.target))
            .collect()
    }

    /// A short human description of the resolved endpoint for the connect log
    /// line, e.g. `vm1 (10.0.0.71 via p1)` or, when nothing was rewritten and
    /// there are no jumps, just the bare alias `vm1`.
    #[must_use]
    pub fn describe(&self) -> String {
        let rewritten = self.target.host_name != self.target.alias;
        if !rewritten && self.jumps.is_empty() {
            return self.target.alias.clone();
        }
        let mut inner = self.target.host_name.clone();
        if !self.jumps.is_empty() {
            let hops: Vec<&str> = self.jumps.iter().map(|j| j.alias.as_str()).collect();
            inner.push_str(" via ");
            inner.push_str(&hops.join(", "));
        }
        format!("{} ({inner})", self.target.alias)
    }
}

/// Something that made a `ProxyJump` route unbuildable. Parse-level problems
/// (unknown keywords, unreadable includes) are *not* errors — they are ignored —
/// but a cyclic or too-deep jump chain leaves no usable route, so it surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteError {
    /// A `ProxyJump` chain revisits a host (e.g. `p1 → p2 → p1`).
    Cycle {
        /// The alias at which the cycle was detected.
        alias: String,
    },
    /// The jump chain is deeper than [`MAX_DEPTH`] hops.
    DepthExceeded {
        /// The cap that was exceeded.
        max: usize,
    },
    /// A hop spec had no host part (e.g. a stray comma or `user@`).
    EmptyHost {
        /// The offending spec.
        spec: String,
    },
}

impl std::fmt::Display for RouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RouteError::Cycle { alias } => {
                write!(f, "ProxyJump chain cycles back to {alias:?}")
            }
            RouteError::DepthExceeded { max } => {
                write!(f, "ProxyJump chain exceeds the depth cap of {max}")
            }
            RouteError::EmptyHost { spec } => {
                write!(f, "ProxyJump hop {spec:?} has no host")
            }
        }
    }
}

/// One parsed line of a config, after include expansion.
#[derive(Debug, Clone)]
enum Line {
    /// A `Host` block header carrying its raw whitespace-separated patterns.
    Host(String),
    /// A `Match` header — its block is treated as never-applicable.
    Match,
    /// Any other directive: lowercased keyword plus the raw argument remainder.
    Keyword { name: String, args: String },
}

/// A parsed `ssh_config`: an ordered line list ready for host resolution.
#[derive(Debug, Clone, Default)]
pub struct SshConfig {
    lines: Vec<Line>,
}

impl SshConfig {
    /// Parse config `content` **purely** — no filesystem access, so `Include`
    /// directives are ignored. Use [`SshConfig::load`] to honour includes.
    #[must_use]
    pub fn parse(content: &str) -> SshConfig {
        let mut lines = Vec::new();
        for raw in content.lines() {
            let Some((kw, arg)) = split_directive(raw) else {
                continue;
            };
            match kw.to_ascii_lowercase().as_str() {
                "host" => lines.push(Line::Host(arg.to_owned())),
                "match" => lines.push(Line::Match),
                // Include with no filesystem is a no-op in the pure parser.
                "include" => {}
                other => lines.push(Line::Keyword {
                    name: other.to_owned(),
                    args: arg.to_owned(),
                }),
            }
        }
        SshConfig { lines }
    }

    /// Load a config from `path`, expanding `Include` directives in place
    /// (glob-matched, relative includes resolved against `<home>/.ssh`, as
    /// `ssh` does for a user config). A missing or unreadable file — including
    /// the top-level `path` — contributes nothing rather than failing, matching
    /// `ssh`'s lenience. Include recursion is bounded and cycle-guarded by
    /// canonical path.
    #[must_use]
    pub fn load(path: &Path, home: &Path) -> SshConfig {
        let mut lines = Vec::new();
        let mut visited = HashSet::new();
        load_into(path, home, &mut lines, &mut visited, 0);
        SshConfig { lines }
    }

    /// Resolve the directives that apply to `alias`, folding the global section
    /// and every matching `Host` block with first-obtained-wins semantics
    /// (`IdentityFile` accumulates; all other single-valued keywords keep their
    /// first value).
    fn resolve_raw(&self, alias: &str) -> RawHostConfig {
        let mut active = true; // the pre-`Host` section is global.
        let mut raw = RawHostConfig::default();
        for line in &self.lines {
            match line {
                Line::Host(patterns) => active = host_matches(patterns, alias),
                Line::Match => active = false,
                Line::Keyword { name, args } if active => raw.apply(name, args),
                Line::Keyword { .. } => {}
            }
        }
        raw
    }

    /// Resolve `target` into a full [`ResolvedRoute`], recursively resolving any
    /// `ProxyJump` hops through this same config. `home` expands `~`/`%d` in
    /// paths; `default_port` is used where neither the spec nor config sets one.
    ///
    /// # Errors
    /// [`RouteError`] if the jump chain cycles, exceeds the depth cap, or names
    /// an empty host.
    pub fn resolve_route(
        &self,
        target: &str,
        home: &Path,
        default_port: u16,
    ) -> Result<ResolvedRoute, RouteError> {
        let mut jumps = Vec::new();
        let mut unknown = Vec::new();
        let mut visited = HashSet::new();
        let endpoint = self.build_chain(
            target,
            home,
            default_port,
            &mut jumps,
            &mut visited,
            &mut unknown,
            0,
        )?;
        Ok(ResolvedRoute {
            jumps,
            target: endpoint,
            unknown_options: unknown,
        })
    }

    /// Resolve one hop and, before returning it, resolve (and append, in
    /// connection order) every hop reached via its `ProxyJump`. The caller
    /// decides whether the returned endpoint is a jump (pushed onto `jumps`) or
    /// the final target.
    #[allow(clippy::too_many_arguments)] // one cohesive recursion; splitting obscures it.
    fn build_chain(
        &self,
        spec: &str,
        home: &Path,
        default_port: u16,
        jumps: &mut Vec<ResolvedEndpoint>,
        visited: &mut HashSet<String>,
        unknown: &mut Vec<String>,
        depth: usize,
    ) -> Result<ResolvedEndpoint, RouteError> {
        if depth >= MAX_DEPTH {
            return Err(RouteError::DepthExceeded { max: MAX_DEPTH });
        }
        let hop = HopSpec::parse(spec)?;
        if !visited.insert(hop.host.clone()) {
            return Err(RouteError::Cycle {
                alias: hop.host.clone(),
            });
        }
        let raw = self.resolve_raw(&hop.host);
        for name in &raw.unknown {
            if !unknown.contains(name) {
                unknown.push(name.clone());
            }
        }
        // Resolve this hop's own ProxyJump chain first, so it is dialed before it.
        if let Some(pj) = &raw.proxy_jump {
            if !pj.eq_ignore_ascii_case("none") {
                for part in pj.split(',') {
                    let part = part.trim();
                    if part.is_empty() {
                        continue;
                    }
                    let jump = self.build_chain(
                        part,
                        home,
                        default_port,
                        jumps,
                        visited,
                        unknown,
                        depth + 1,
                    )?;
                    jumps.push(jump);
                }
            }
        }
        Ok(raw.into_endpoint(&hop, home, default_port))
    }
}

/// A `[user@]host[:port]` hop spec, tracking whether user/port were explicit so
/// they override the config only when the caller actually supplied them.
#[derive(Debug)]
struct HopSpec {
    user: Option<String>,
    host: String,
    port: Option<u16>,
}

impl HopSpec {
    /// Parse a hop spec, honouring `[ipv6]` bracketing for the host literal.
    fn parse(spec: &str) -> Result<HopSpec, RouteError> {
        let spec = spec.trim();
        let (user, rest) = match spec.split_once('@') {
            Some((u, r)) if !u.is_empty() => (Some(u.to_owned()), r),
            _ => (None, spec),
        };
        let (host, port) = if let Some(inner) = rest.strip_prefix('[') {
            // Bracketed IPv6 literal: `[addr]` or `[addr]:port`.
            match inner.split_once(']') {
                Some((addr, after)) => {
                    let port = after.strip_prefix(':').and_then(|p| p.parse::<u16>().ok());
                    (addr.to_owned(), port)
                }
                None => (rest.to_owned(), None),
            }
        } else {
            match rest.rsplit_once(':') {
                // A single trailing `:port`; multiple colons ⇒ bare IPv6 ⇒ no port.
                Some((h, p)) if !h.contains(':') && p.parse::<u16>().is_ok() => {
                    (h.to_owned(), p.parse::<u16>().ok())
                }
                _ => (rest.to_owned(), None),
            }
        };
        if host.is_empty() {
            return Err(RouteError::EmptyHost {
                spec: spec.to_owned(),
            });
        }
        Ok(HopSpec { user, host, port })
    }
}

/// The raw, first-obtained-wins directive values collected for one host before
/// path expansion and endpoint assembly.
#[derive(Debug, Default)]
struct RawHostConfig {
    host_name: Option<String>,
    user: Option<String>,
    port: Option<u16>,
    identity_files: Vec<String>,
    identities_only: Option<bool>,
    strict: Option<StrictHostKey>,
    known_hosts: Option<Vec<String>>,
    global_known_hosts: Option<Vec<String>>,
    proxy_jump: Option<String>,
    unknown: Vec<String>,
}

impl RawHostConfig {
    /// Fold one directive into the accumulator with `ssh` semantics: `IdentityFile`
    /// accumulates; every other single-valued keyword keeps its first value.
    fn apply(&mut self, name: &str, args: &str) {
        match name {
            "hostname" => set_first(&mut self.host_name, || unquote(args.trim()).to_owned()),
            "user" => set_first(&mut self.user, || unquote(args.trim()).to_owned()),
            "port" => {
                if self.port.is_none() {
                    if let Ok(p) = args.trim().parse::<u16>() {
                        if p != 0 {
                            self.port = Some(p);
                        }
                    }
                }
            }
            "identityfile" => self.identity_files.extend(split_args(args)),
            "identitiesonly" => set_first(&mut self.identities_only, || parse_yes(args)),
            "stricthostkeychecking" => {
                if self.strict.is_none() {
                    self.strict = StrictHostKey::parse(args);
                }
            }
            "userknownhostsfile" => set_first(&mut self.known_hosts, || split_args(args)),
            "globalknownhostsfile" => {
                set_first(&mut self.global_known_hosts, || split_args(args));
            }
            "proxyjump" => set_first(&mut self.proxy_jump, || args.trim().to_owned()),
            other => {
                if !self.unknown.iter().any(|u| u == other) {
                    self.unknown.push(other.to_owned());
                }
            }
        }
    }

    /// Assemble a [`ResolvedEndpoint`], letting explicit spec fields (user/port)
    /// override the config and expanding all paths against `home`.
    fn into_endpoint(self, hop: &HopSpec, home: &Path, default_port: u16) -> ResolvedEndpoint {
        let host_name = self.host_name.unwrap_or_else(|| hop.host.clone());
        let port = hop.port.or(self.port).unwrap_or(default_port);
        let user = hop.user.clone().or(self.user);
        let mut identity_files = Vec::new();
        for f in &self.identity_files {
            let path = expand_path(unquote(f), home);
            if !identity_files.contains(&path) {
                identity_files.push(path);
            }
        }
        // User known-hosts: the directive if present, else OpenSSH's default
        // pair `~/.ssh/known_hosts` + `~/.ssh/known_hosts2`.
        let known_hosts_files = self.known_hosts.map_or_else(
            || {
                vec![
                    home.join(".ssh").join("known_hosts"),
                    home.join(".ssh").join("known_hosts2"),
                ]
            },
            |v| v.iter().map(|k| expand_path(unquote(k), home)).collect(),
        );
        // Global known-hosts: the directive if present, else the OpenSSH default
        // pair under /etc/ssh. Always consulted for lookup.
        let global_known_hosts_files = self.global_known_hosts.map_or_else(
            || {
                DEFAULT_GLOBAL_KNOWN_HOSTS
                    .iter()
                    .map(|p| expand_path(p, home))
                    .collect()
            },
            |v| v.iter().map(|k| expand_path(unquote(k), home)).collect(),
        );
        ResolvedEndpoint {
            alias: hop.host.clone(),
            host_name,
            port,
            user,
            identity_files,
            identities_only: self.identities_only.unwrap_or(false),
            strict: self.strict.unwrap_or_default(),
            known_hosts_files,
            global_known_hosts_files,
        }
    }
}

/// Set `slot` to `value()` only if it is still unset (first-obtained-wins).
fn set_first<T>(slot: &mut Option<T>, value: impl FnOnce() -> T) {
    if slot.is_none() {
        *slot = Some(value());
    }
}

/// Parse a `yes`/`no` argument; anything other than `yes` (case-insensitive) is
/// `false`, matching `ssh`'s treatment of these boolean options.
fn parse_yes(arg: &str) -> bool {
    arg.trim().eq_ignore_ascii_case("yes")
}

/// The `IdentityFile` paths that apply to `host` in the given `ssh_config`
/// `content`, in file order (first declared first), tilde/`%d`-expanded against
/// `home`. Duplicates are removed, preserving first-seen order.
///
/// Global directives that appear before any `Host` line apply to every host
/// (matching `ssh`'s own semantics), so a top-of-file `IdentityFile` is always
/// included. Returns an empty vec when the config names no identity for `host`.
/// This is a thin convenience over [`SshConfig::parse`] +
/// [`ResolvedEndpoint::identity_files`] for callers that only need the keys.
///
/// # Examples
/// ```
/// use std::path::Path;
/// use tomo_transport::identity_files_for;
///
/// let cfg = "IdentityFile ~/.ssh/id_global\n\
///            Host build\n  IdentityFile ~/.ssh/id_build\n";
/// let home = Path::new("/home/u");
/// // The global key applies everywhere; the Host-scoped one only to `build`.
/// assert_eq!(
///     identity_files_for(cfg, "example.com", home),
///     vec![Path::new("/home/u/.ssh/id_global").to_path_buf()]
/// );
/// assert_eq!(
///     identity_files_for(cfg, "build", home),
///     vec![
///         Path::new("/home/u/.ssh/id_global").to_path_buf(),
///         Path::new("/home/u/.ssh/id_build").to_path_buf(),
///     ]
/// );
/// ```
#[must_use]
pub fn identity_files_for(content: &str, host: &str, home: &Path) -> Vec<PathBuf> {
    let raw = SshConfig::parse(content).resolve_raw(host);
    let mut out: Vec<PathBuf> = Vec::new();
    for f in &raw.identity_files {
        let path = expand_path(unquote(f), home);
        if !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

/// Recursively read `path`, splicing `Include` targets in place. Missing files,
/// unreadable files, over-deep nesting, and include cycles are all silently
/// skipped (ssh treats config problems as warnings, never fatal).
fn load_into(
    path: &Path,
    home: &Path,
    out: &mut Vec<Line>,
    visited: &mut HashSet<PathBuf>,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }
    // Canonicalize for the cycle guard where possible; fall back to the raw path
    // (a not-yet-existing file simply won't be read below).
    let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(key) {
        return;
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    for raw in content.lines() {
        let Some((kw, arg)) = split_directive(raw) else {
            continue;
        };
        match kw.to_ascii_lowercase().as_str() {
            "host" => out.push(Line::Host(arg.to_owned())),
            "match" => out.push(Line::Match),
            "include" => {
                for inc in expand_includes(arg, home) {
                    load_into(&inc, home, out, visited, depth + 1);
                }
            }
            other => out.push(Line::Keyword {
                name: other.to_owned(),
                args: arg.to_owned(),
            }),
        }
    }
}

/// Expand an `Include` argument into concrete file paths. Each whitespace token
/// is tilde-expanded; a relative token resolves under `<home>/.ssh` (the base
/// `ssh` uses for a user config). Wildcards (`*`/`?`) in the **final** path
/// component are globbed against that directory (lexically sorted); earlier
/// components must be literal. Non-wildcard tokens are returned as-is.
fn expand_includes(arg: &str, home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for tok in split_args(arg) {
        let base: PathBuf = if let Some(rest) = tok.strip_prefix("~/") {
            home.join(rest)
        } else if Path::new(&tok).is_absolute() {
            PathBuf::from(&tok)
        } else {
            home.join(".ssh").join(&tok)
        };
        if tok.contains('*') || tok.contains('?') {
            let (Some(dir), Some(name)) = (base.parent(), base.file_name()) else {
                continue;
            };
            let pattern = name.to_string_lossy().into_owned();
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut matched: Vec<PathBuf> = entries
                .flatten()
                .filter(|e| glob_match(&pattern, &e.file_name().to_string_lossy()))
                .map(|e| e.path())
                .collect();
            matched.sort();
            out.extend(matched);
        } else {
            out.push(base);
        }
    }
    out
}

/// Split one config line into `(keyword, argument)`, or `None` for blank/comment
/// lines. ssh accepts either `Keyword value` or `Keyword=value`, with arbitrary
/// surrounding whitespace; a `#` at the first non-blank position is a comment.
fn split_directive(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let split_at = trimmed.find(|c: char| c.is_whitespace() || c == '=')?;
    let keyword = &trimmed[..split_at];
    let rest = trimmed[split_at..].trim_start();
    let rest = rest.strip_prefix('=').map_or(rest, str::trim_start);
    if keyword.is_empty() || rest.is_empty() {
        return None;
    }
    Some((keyword, rest))
}

/// Split an argument string into tokens, honouring double-quoting so a path with
/// spaces stays one token. Quote characters are removed.
fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut saw_token = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_quote = !in_quote;
                saw_token = true;
            }
            c if c.is_whitespace() && !in_quote => {
                if saw_token {
                    out.push(std::mem::take(&mut cur));
                    saw_token = false;
                }
            }
            c => {
                cur.push(c);
                saw_token = true;
            }
        }
    }
    if saw_token {
        out.push(cur);
    }
    out
}

/// Does any whitespace-separated pattern in a `Host` line match `host`? A `!`
/// prefix negates: if any negated pattern matches, the host is excluded outright,
/// mirroring `ssh`'s rule that a single negative match wins.
fn host_matches(patterns: &str, host: &str) -> bool {
    let mut positive = false;
    for pat in patterns.split_whitespace() {
        if let Some(neg) = pat.strip_prefix('!') {
            if glob_match(neg, host) {
                return false;
            }
        } else if glob_match(pat, host) {
            positive = true;
        }
    }
    positive
}

/// Case-insensitive glob match supporting `*` (any run) and `?` (one char) —
/// the only wildcards `ssh_config` patterns use. Iterative backtracking so it
/// stays linear-ish without recursion or regex.
fn glob_match(pattern: &str, text: &str) -> bool {
    let needle: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let hay: Vec<char> = text.to_ascii_lowercase().chars().collect();
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while t < hay.len() {
        if p < needle.len() && (needle[p] == '?' || needle[p] == hay[t]) {
            p += 1;
            t += 1;
        } else if p < needle.len() && needle[p] == '*' {
            star = Some(p);
            mark = t;
            p += 1;
        } else if let Some(sp) = star {
            p = sp + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    while p < needle.len() && needle[p] == '*' {
        p += 1;
    }
    p == needle.len()
}

/// Strip one layer of surrounding double quotes. Unquoted input is unchanged.
fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(s)
}

/// Expand a path argument to an absolute path. `~` and `%d` both stand for the
/// home directory; an absolute path is kept as-is; any other relative path
/// resolves under `~/.ssh` (ssh's default base). `/dev/null` is absolute, so it
/// passes through unchanged.
fn expand_path(arg: &str, home: &Path) -> PathBuf {
    if let Some(rest) = arg.strip_prefix("~/").or_else(|| arg.strip_prefix("%d/")) {
        return home.join(rest);
    }
    if arg == "~" || arg == "%d" {
        return home.to_path_buf();
    }
    let path = Path::new(arg);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        home.join(".ssh").join(arg)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // panics are fine in tests
mod tests {
    use super::*;

    /// The SSH default port; local to the tests since production callers pass
    /// [`crate::hostspec::DEFAULT_SSH_PORT`].
    const DEFAULT_PORT: u16 = 22;

    fn home() -> PathBuf {
        PathBuf::from("/home/jake")
    }
    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }
    fn route(cfg: &str, target: &str) -> ResolvedRoute {
        SshConfig::parse(cfg)
            .resolve_route(target, &home(), DEFAULT_PORT)
            .unwrap()
    }

    // ---- identity_files_for (preserved behaviour) ----------------------------

    #[test]
    fn global_identity_applies_to_every_host() {
        let cfg =
            "IdentityFile ~/.ssh/id_tokyo\n\nHost github.com\n  IdentityFile ~/.ssh/id_github\n";
        assert_eq!(
            identity_files_for(cfg, "localhost", &home()),
            vec![p("/home/jake/.ssh/id_tokyo")]
        );
    }

    #[test]
    fn host_block_adds_to_global() {
        let cfg =
            "IdentityFile ~/.ssh/id_tokyo\nHost github.com\n  IdentityFile ~/.ssh/id_github\n";
        assert_eq!(
            identity_files_for(cfg, "github.com", &home()),
            vec![
                p("/home/jake/.ssh/id_tokyo"),
                p("/home/jake/.ssh/id_github"),
            ]
        );
    }

    #[test]
    fn non_matching_host_gets_only_global() {
        let cfg = "Host vm1\n  IdentityFile ~/.ssh/id_m\n";
        assert!(identity_files_for(cfg, "elsewhere", &home()).is_empty());
    }

    #[test]
    fn glob_star_and_question_match() {
        let cfg = "Host 10.0.0.*\n  IdentityFile ~/.ssh/id_lan\nHost web?\n  IdentityFile ~/.ssh/id_web\n";
        assert_eq!(
            identity_files_for(cfg, "10.0.0.71", &home()),
            vec![p("/home/jake/.ssh/id_lan")]
        );
        assert_eq!(
            identity_files_for(cfg, "web3", &home()),
            vec![p("/home/jake/.ssh/id_web")]
        );
        assert!(identity_files_for(cfg, "web42", &home()).is_empty());
    }

    #[test]
    fn multiple_patterns_on_one_host_line() {
        let cfg = "Host alpha beta gamma\n  IdentityFile ~/.ssh/id_greek\n";
        for h in ["alpha", "beta", "gamma"] {
            assert_eq!(
                identity_files_for(cfg, h, &home()),
                vec![p("/home/jake/.ssh/id_greek")],
                "host {h}"
            );
        }
        assert!(identity_files_for(cfg, "delta", &home()).is_empty());
    }

    #[test]
    fn negation_excludes() {
        let cfg = "Host *.internal !secret.internal\n  IdentityFile ~/.ssh/id_int\n";
        assert_eq!(
            identity_files_for(cfg, "box.internal", &home()),
            vec![p("/home/jake/.ssh/id_int")]
        );
        assert!(identity_files_for(cfg, "secret.internal", &home()).is_empty());
    }

    #[test]
    fn equals_form_and_case_insensitive_keyword() {
        let cfg = "identityfile=~/.ssh/id_lower\nHOST=build\n  IDENTITYFILE = ~/.ssh/id_build\n";
        assert_eq!(
            identity_files_for(cfg, "build", &home()),
            vec![p("/home/jake/.ssh/id_lower"), p("/home/jake/.ssh/id_build"),]
        );
    }

    #[test]
    fn comments_and_blanks_ignored() {
        let cfg = "# a comment\n\n  # indented comment\nIdentityFile ~/.ssh/id_ok\n";
        assert_eq!(
            identity_files_for(cfg, "any", &home()),
            vec![p("/home/jake/.ssh/id_ok")]
        );
    }

    #[test]
    fn absolute_and_tilde_and_relative_paths() {
        let cfg = "IdentityFile /abs/key\nIdentityFile ~/tkey\nIdentityFile relkey\n";
        assert_eq!(
            identity_files_for(cfg, "any", &home()),
            vec![
                p("/abs/key"),
                p("/home/jake/tkey"),
                p("/home/jake/.ssh/relkey"),
            ]
        );
    }

    #[test]
    fn quoted_path_is_unquoted() {
        let cfg = "IdentityFile \"~/.ssh/id with space\"\n";
        assert_eq!(
            identity_files_for(cfg, "any", &home()),
            vec![p("/home/jake/.ssh/id with space")]
        );
    }

    #[test]
    fn duplicates_removed_preserving_order() {
        let cfg = "IdentityFile ~/.ssh/id_a\nHost x\n  IdentityFile ~/.ssh/id_a\n  IdentityFile ~/.ssh/id_b\n";
        assert_eq!(
            identity_files_for(cfg, "x", &home()),
            vec![p("/home/jake/.ssh/id_a"), p("/home/jake/.ssh/id_b")]
        );
    }

    #[test]
    fn match_block_keys_are_not_attributed() {
        let cfg =
            "IdentityFile ~/.ssh/id_global\nMatch host secret\n  IdentityFile ~/.ssh/id_secret\n";
        assert_eq!(
            identity_files_for(cfg, "secret", &home()),
            vec![p("/home/jake/.ssh/id_global")]
        );
    }

    #[test]
    fn empty_config_yields_nothing() {
        assert!(identity_files_for("", "any", &home()).is_empty());
    }

    // ---- HostName / User / Port resolution -----------------------------------

    #[test]
    fn hostname_alias_rewrites_to_real_host() {
        let cfg = "Host vm1\n  HostName 10.0.0.71\n  User jake\n  Port 2222\n";
        let r = route(cfg, "vm1");
        assert_eq!(r.target.host_name, "10.0.0.71");
        assert_eq!(r.target.user.as_deref(), Some("jake"));
        assert_eq!(r.target.port, 2222);
        assert_eq!(r.target.alias, "vm1");
    }

    #[test]
    fn no_hostname_keeps_alias_and_default_port() {
        let r = route("", "example.com");
        assert_eq!(r.target.host_name, "example.com");
        assert_eq!(r.target.port, DEFAULT_PORT);
        assert_eq!(r.target.user, None);
    }

    #[test]
    fn first_obtained_wins_for_single_valued_options() {
        // Two matching blocks; the first HostName/User/Port must win.
        let cfg = "Host vm1\n  HostName first\n  User u1\n  Port 111\n\
                   Host vm*\n  HostName second\n  User u2\n  Port 222\n";
        let r = route(cfg, "vm1");
        assert_eq!(r.target.host_name, "first");
        assert_eq!(r.target.user.as_deref(), Some("u1"));
        assert_eq!(r.target.port, 111);
    }

    #[test]
    fn explicit_user_and_port_override_config() {
        let cfg = "Host vm1\n  HostName 10.0.0.71\n  User configured\n  Port 2222\n";
        let r = route(cfg, "admin@vm1:2200");
        // The alias for matching is the bare host; explicit user/port win.
        assert_eq!(r.target.host_name, "10.0.0.71");
        assert_eq!(r.target.user.as_deref(), Some("admin"));
        assert_eq!(r.target.port, 2200);
    }

    // ---- StrictHostKeyChecking / IdentitiesOnly / UserKnownHostsFile ---------

    #[test]
    fn strict_host_key_variants() {
        for (val, want) in [
            ("yes", StrictHostKey::Yes),
            ("no", StrictHostKey::No),
            ("accept-new", StrictHostKey::AcceptNew),
            ("ask", StrictHostKey::Yes), // non-interactive → treat as yes
        ] {
            let cfg = format!("Host h\n  StrictHostKeyChecking {val}\n");
            assert_eq!(route(&cfg, "h").target.strict, want, "value {val}");
        }
        // Unknown value leaves the default (Yes).
        assert_eq!(
            route("Host h\n  StrictHostKeyChecking bogus\n", "h")
                .target
                .strict,
            StrictHostKey::Yes
        );
    }

    #[test]
    fn identities_only_flag() {
        assert!(
            route("Host h\n  IdentitiesOnly yes\n", "h")
                .target
                .identities_only
        );
        assert!(
            !route("Host h\n  IdentitiesOnly no\n", "h")
                .target
                .identities_only
        );
        assert!(!route("Host h\n", "h").target.identities_only);
    }

    #[test]
    fn user_known_hosts_multiple_paths() {
        let cfg = "Host h\n  UserKnownHostsFile /dev/null ~/.ssh/extra\n";
        let r = route(cfg, "h");
        assert_eq!(
            r.target.known_hosts_files,
            vec![p("/dev/null"), p("/home/jake/.ssh/extra")]
        );
    }

    #[test]
    fn dev_null_known_hosts_preserved() {
        let r = route("Host h\n  UserKnownHostsFile /dev/null\n", "h");
        assert_eq!(r.target.known_hosts_files, vec![p("/dev/null")]);
    }

    #[test]
    fn default_known_hosts_are_the_openssh_pair_plus_global() {
        // No directives: user defaults to known_hosts + known_hosts2, global to
        // the /etc pair; lookup is user-then-global; recording targets the first
        // user file.
        let r = route("", "h");
        assert_eq!(
            r.target.known_hosts_files,
            vec![
                p("/home/jake/.ssh/known_hosts"),
                p("/home/jake/.ssh/known_hosts2"),
            ]
        );
        assert_eq!(
            r.target.global_known_hosts_files,
            vec![
                p("/etc/ssh/ssh_known_hosts"),
                p("/etc/ssh/ssh_known_hosts2"),
            ]
        );
        assert_eq!(
            r.target.lookup_known_hosts(),
            vec![
                p("/home/jake/.ssh/known_hosts"),
                p("/home/jake/.ssh/known_hosts2"),
                p("/etc/ssh/ssh_known_hosts"),
                p("/etc/ssh/ssh_known_hosts2"),
            ]
        );
        assert_eq!(
            r.target.record_target(),
            Some(p("/home/jake/.ssh/known_hosts"))
        );
    }

    #[test]
    fn explicit_user_directive_replaces_defaults_but_global_still_appended() {
        let r = route("Host h\n  UserKnownHostsFile ~/.ssh/only\n", "h");
        assert_eq!(r.target.known_hosts_files, vec![p("/home/jake/.ssh/only")]);
        // Global defaults are still consulted for lookup.
        assert_eq!(
            r.target.lookup_known_hosts(),
            vec![
                p("/home/jake/.ssh/only"),
                p("/etc/ssh/ssh_known_hosts"),
                p("/etc/ssh/ssh_known_hosts2"),
            ]
        );
    }

    #[test]
    fn global_known_hosts_file_override() {
        let r = route(
            "Host h\n  GlobalKnownHostsFile /etc/custom/kh /dev/null\n",
            "h",
        );
        assert_eq!(
            r.target.global_known_hosts_files,
            vec![p("/etc/custom/kh"), p("/dev/null")]
        );
        // User set is still the default pair.
        assert_eq!(
            r.target.known_hosts_files,
            vec![
                p("/home/jake/.ssh/known_hosts"),
                p("/home/jake/.ssh/known_hosts2"),
            ]
        );
    }

    #[test]
    fn record_target_skips_dev_null_and_prefers_first_user_file() {
        // /dev/null first ⇒ record into the next user file.
        let r = route("Host h\n  UserKnownHostsFile /dev/null ~/.ssh/kh\n", "h");
        assert_eq!(r.target.record_target(), Some(p("/home/jake/.ssh/kh")));
        // Only /dev/null ⇒ nothing is ever recorded.
        let r2 = route("Host h\n  UserKnownHostsFile /dev/null\n", "h");
        assert_eq!(r2.target.record_target(), None);
        // Recording never targets a global file.
        let r3 = route(
            "Host h\n  UserKnownHostsFile /dev/null\n  GlobalKnownHostsFile /etc/g\n",
            "h",
        );
        assert_eq!(r3.target.record_target(), None);
    }

    #[test]
    fn unknown_options_collected_once() {
        let cfg = "Host h\n  Compression yes\n  ServerAliveInterval 30\n  Compression no\n";
        let r = route(cfg, "h");
        assert_eq!(
            r.unknown_options,
            vec!["compression", "serveraliveinterval"]
        );
    }

    // ---- ProxyJump -----------------------------------------------------------

    #[test]
    fn proxy_jump_single_hop() {
        let cfg = "Host vm1\n  HostName 10.0.0.71\n  ProxyJump p1\n\
                   Host p1\n  HostName 10.0.0.1\n  User jump\n";
        let r = route(cfg, "vm1");
        assert_eq!(r.jumps.len(), 1);
        assert_eq!(r.jumps[0].alias, "p1");
        assert_eq!(r.jumps[0].host_name, "10.0.0.1");
        assert_eq!(r.jumps[0].user.as_deref(), Some("jump"));
        assert_eq!(r.target.host_name, "10.0.0.71");
        // chain() is jumps-first, target-last.
        let chain = r.chain();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].alias, "p1");
        assert_eq!(chain[1].alias, "vm1");
    }

    #[test]
    fn proxy_jump_comma_chain_left_to_right() {
        let cfg = "Host dst\n  ProxyJump a,b,c\n";
        let r = route(cfg, "dst");
        let hops: Vec<&str> = r.jumps.iter().map(|j| j.alias.as_str()).collect();
        assert_eq!(hops, vec!["a", "b", "c"]);
    }

    #[test]
    fn proxy_jump_with_inline_user_and_port() {
        let cfg = "Host dst\n  ProxyJump bob@gate:2022\n";
        let r = route(cfg, "dst");
        assert_eq!(r.jumps[0].host_name, "gate");
        assert_eq!(r.jumps[0].user.as_deref(), Some("bob"));
        assert_eq!(r.jumps[0].port, 2022);
    }

    #[test]
    fn proxy_jump_nested_is_flattened_in_dial_order() {
        // dst → via b; b → via a. Dial order must be a, b, dst.
        let cfg = "Host dst\n  ProxyJump b\nHost b\n  ProxyJump a\n";
        let r = route(cfg, "dst");
        let hops: Vec<&str> = r.jumps.iter().map(|j| j.alias.as_str()).collect();
        assert_eq!(hops, vec!["a", "b"]);
    }

    #[test]
    fn proxy_jump_none_disables() {
        let cfg = "Host dst\n  ProxyJump none\n";
        let r = route(cfg, "dst");
        assert!(r.jumps.is_empty());
    }

    #[test]
    fn proxy_jump_cycle_is_rejected() {
        let cfg = "Host p1\n  ProxyJump p2\nHost p2\n  ProxyJump p1\n";
        let err = SshConfig::parse(cfg)
            .resolve_route("p1", &home(), DEFAULT_PORT)
            .unwrap_err();
        assert!(matches!(err, RouteError::Cycle { .. }));
    }

    #[test]
    fn proxy_jump_self_cycle_is_rejected() {
        let cfg = "Host p1\n  ProxyJump p1\n";
        assert!(matches!(
            SshConfig::parse(cfg)
                .resolve_route("p1", &home(), DEFAULT_PORT)
                .unwrap_err(),
            RouteError::Cycle { .. }
        ));
    }

    #[test]
    fn describe_direct_target_is_bare_alias() {
        assert_eq!(route("", "example.com").describe(), "example.com");
    }

    #[test]
    fn describe_rewrite_and_jump() {
        let cfg = "Host vm1\n  HostName 10.0.0.71\n  ProxyJump p1\n";
        assert_eq!(route(cfg, "vm1").describe(), "vm1 (10.0.0.71 via p1)");
    }

    #[test]
    fn describe_rewrite_without_jump() {
        let cfg = "Host vm1\n  HostName 10.0.0.71\n";
        assert_eq!(route(cfg, "vm1").describe(), "vm1 (10.0.0.71)");
    }

    // ---- Include (filesystem fixture tree) -----------------------------------

    #[test]
    fn include_literal_and_glob_merge_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        let confd = ssh.join("conf.d");
        std::fs::create_dir_all(&confd).unwrap();
        // Top config includes a literal file then a glob directory.
        std::fs::write(
            ssh.join("config"),
            "Include included.conf\nInclude conf.d/*.conf\nHost top\n  User topuser\n",
        )
        .unwrap();
        std::fs::write(
            ssh.join("included.conf"),
            "Host vm1\n  HostName 10.0.0.71\n",
        )
        .unwrap();
        std::fs::write(confd.join("10-a.conf"), "Host vma\n  User aaa\n").unwrap();
        std::fs::write(confd.join("20-b.conf"), "Host vmb\n  User bbb\n").unwrap();

        let cfg = SshConfig::load(&ssh.join("config"), dir.path());
        assert_eq!(
            cfg.resolve_route("vm1", dir.path(), DEFAULT_PORT)
                .unwrap()
                .target
                .host_name,
            "10.0.0.71"
        );
        assert_eq!(
            cfg.resolve_route("vma", dir.path(), DEFAULT_PORT)
                .unwrap()
                .target
                .user
                .as_deref(),
            Some("aaa")
        );
        assert_eq!(
            cfg.resolve_route("vmb", dir.path(), DEFAULT_PORT)
                .unwrap()
                .target
                .user
                .as_deref(),
            Some("bbb")
        );
        assert_eq!(
            cfg.resolve_route("top", dir.path(), DEFAULT_PORT)
                .unwrap()
                .target
                .user
                .as_deref(),
            Some("topuser")
        );
    }

    #[test]
    fn include_ordering_respects_first_obtained_wins() {
        // The included file is processed BEFORE the inline Host block, so its
        // HostName wins over the later one (first-obtained-wins).
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(
            ssh.join("config"),
            "Include first.conf\nHost vm1\n  HostName second\n",
        )
        .unwrap();
        std::fs::write(ssh.join("first.conf"), "Host vm1\n  HostName first\n").unwrap();
        let cfg = SshConfig::load(&ssh.join("config"), dir.path());
        assert_eq!(
            cfg.resolve_route("vm1", dir.path(), DEFAULT_PORT)
                .unwrap()
                .target
                .host_name,
            "first"
        );
    }

    #[test]
    fn include_cycle_is_broken() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        // a includes b, b includes a — must terminate.
        std::fs::write(ssh.join("config"), "Include a.conf\n").unwrap();
        std::fs::write(ssh.join("a.conf"), "Include b.conf\nHost h\n  User ua\n").unwrap();
        std::fs::write(ssh.join("b.conf"), "Include a.conf\nHost h2\n  User ub\n").unwrap();
        let cfg = SshConfig::load(&ssh.join("config"), dir.path());
        assert_eq!(
            cfg.resolve_route("h", dir.path(), DEFAULT_PORT)
                .unwrap()
                .target
                .user
                .as_deref(),
            Some("ua")
        );
    }

    #[test]
    fn missing_include_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(
            ssh.join("config"),
            "Include does-not-exist.conf\nHost h\n  User u\n",
        )
        .unwrap();
        let cfg = SshConfig::load(&ssh.join("config"), dir.path());
        assert_eq!(
            cfg.resolve_route("h", dir.path(), DEFAULT_PORT)
                .unwrap()
                .target
                .user
                .as_deref(),
            Some("u")
        );
    }

    #[test]
    fn missing_top_config_yields_empty() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = SshConfig::load(&dir.path().join("nope"), dir.path());
        let r = cfg.resolve_route("h", dir.path(), DEFAULT_PORT).unwrap();
        assert_eq!(r.target.host_name, "h");
        assert!(r.jumps.is_empty());
    }

    // ---- HopSpec parsing edge cases ------------------------------------------

    #[test]
    fn hopspec_forms() {
        let s = HopSpec::parse("host").unwrap();
        assert_eq!(s.host, "host");
        assert_eq!(s.user, None);
        assert_eq!(s.port, None);

        let s = HopSpec::parse("u@host:2022").unwrap();
        assert_eq!(s.user.as_deref(), Some("u"));
        assert_eq!(s.host, "host");
        assert_eq!(s.port, Some(2022));

        let s = HopSpec::parse("[fe80::1]:22").unwrap();
        assert_eq!(s.host, "fe80::1");
        assert_eq!(s.port, Some(22));

        let s = HopSpec::parse("bare::ipv6").unwrap();
        assert_eq!(s.host, "bare::ipv6");
        assert_eq!(s.port, None);

        assert!(matches!(
            HopSpec::parse("u@").unwrap_err(),
            RouteError::EmptyHost { .. }
        ));
    }
}
