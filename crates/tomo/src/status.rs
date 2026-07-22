//! The status snapshot: `.tomo/state/status.json` and the `tomo status` command.
//!
//! The running sync loop writes [`Status`] atomically on every change and at
//! least every couple of seconds. A separate `tomo status` invocation reads
//! that file when it is fresh, and otherwise falls back to computing the
//! index-derived fields offline from `.tomo/state/index.bin` (with `net` null
//! and `connected` false), so status works whether or not a `watch` is running.
//!
//! `updated_unix_ms` is wall time carried for **display only** — never an
//! ordering input (CLAUDE.md invariant #7).

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tomo_engine::{EntryState, Index};

use crate::error::CliError;
use crate::fsutil::atomic_write;
use crate::layout::Layout;
use crate::out::outln;

/// A status snapshot serialized to `.tomo/state/status.json`.
///
/// The field set and names are a stable contract: the e2e scenarios assert
/// against `root`, `files`, `tombstones`, `conflicts`, `net`, and `connected`
/// via `--json`.
// The bools (`connected`, `paused`, `peer_paused`, `reconciling`) are
// independent, separately-observed status facets a serialized snapshot must
// expose by name; bundling them into a sub-struct would only obscure the JSON
// contract the scenarios assert on.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// Hex BLAKE3 digest of the index's [`Index::canonical_bytes`] — the "root"
    /// both replicas must agree on after convergence.
    pub root: String,
    /// Number of index entries whose materialized winner is present.
    pub files: u64,
    /// Number of index entries whose materialized winner is a tombstone.
    pub tombstones: u64,
    /// Number of distinct paths currently carrying a surfaced conflict.
    pub conflicts: u64,
    /// Number of conflict records in history not yet acknowledged. Distinct
    /// from `conflicts` (a session-scoped path count): this counts unresolved
    /// rows in the history DB and drives the `tomo status` badge. Additive and
    /// backward compatible — defaulted to `0` when deserializing older files.
    #[serde(default)]
    pub conflicts_unresolved: u64,
    /// Network counters while a session is live; `null` for an offline
    /// computation.
    pub net: Option<Net>,
    /// Whether a peer session is currently connected.
    pub connected: bool,
    /// Whether this session has paused syncing (docs/SPEC.md §13): it keeps
    /// observing and versioning local changes and stays connected, but ships
    /// nothing outbound and applies nothing inbound until resumed. Additive and
    /// backward compatible; defaults `false` for older status files and offline
    /// computations. A restarted session always comes up unpaused.
    #[serde(default)]
    pub paused: bool,
    /// Whether the *peer* has told us it paused (the mirror of `paused` on the
    /// other side): our own edits queue until it resumes. Additive; defaults
    /// `false`.
    #[serde(default)]
    pub peer_paused: bool,
    /// Who is on the other end of the sync, when known. Additive and backward
    /// compatible: absent from older status files (and offline computations
    /// without a configured `[remote]`) and defaulted to `None`. The
    /// `.tomo/README.md` points coding agents at this block.
    #[serde(default)]
    pub peer: Option<Peer>,
    /// Whether a deferred reconciling rescan is pending. True convergence for
    /// the quiet-network invariant means roots equal AND nothing left to
    /// reconcile — scenarios wait for this to clear before observing.
    /// Additive; defaults false for older files.
    #[serde(default)]
    pub reconciling: bool,
    /// History-capture summary. Additive and backward compatible: absent from
    /// older status files and defaulted to `None` when deserializing them.
    #[serde(default)]
    pub history: Option<History>,
    /// The local filesystem semantics probed at session startup (case
    /// sensitivity + Unicode normalization). Additive and backward compatible:
    /// absent from older status files (and offline computations) and defaulted
    /// to `None`. Drives the macOS↔Linux filename guards.
    #[serde(default)]
    pub fs: Option<crate::fsprobe::FsSemantics>,
    /// Wall-clock time of this snapshot in Unix milliseconds. Display only.
    pub updated_unix_ms: u64,
}

/// The history-capture summary block of a [`Status`] snapshot.
///
/// Reports the configured capture mode and running counters so `tomo status`
/// (and the scenarios) can observe that history is being recorded without
/// opening the store. Counters are session-scoped: they reflect what the
/// running `watch` loop has recorded since it started, not the whole DB.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct History {
    /// The active capture mode: `adaptive`, `every-change`, `off`, or
    /// `interval`.
    pub mode: String,
    /// Versions recorded by this session so far.
    pub versions_recorded: u64,
    /// Conflict records written by this session so far.
    pub conflicts_recorded: u64,
    /// Captures currently staged in the pressure controller awaiting flush.
    pub staged: u64,
    /// The controller's current ladder rung (`0` == flush immediately).
    pub rung: u64,
}

/// The peer-identity block of a [`Status`] snapshot.
///
/// Every field is optional: it is filled from whatever the machine cheaply
/// knows about the other end. On the serving side that is the SSH environment
/// (`TOMO_PEER_NAME` prepended by the initiator, and `SSH_CONNECTION`'s client
/// IP); on the initiator side it is the configured `[remote]`. Absent fields are
/// serialized as `null` so the block's shape is stable for consumers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    /// The peer's name (hostname), when known.
    pub name: Option<String>,
    /// The peer's address (client IP on the serving side; resolved host on the
    /// initiator side), when known.
    pub addr: Option<String>,
    /// Where this block came from: `ssh-env` (serving side) or `config`
    /// (initiator side).
    pub source: Option<String>,
}

/// Parse the client IP (the first whitespace-separated field) out of an
/// `SSH_CONNECTION` value (`<client-ip> <client-port> <server-ip>
/// <server-port>`). Returns `None` for an empty or whitespace-only value.
#[must_use]
pub fn client_ip_from_ssh_connection(value: &str) -> Option<String> {
    value
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

/// Build the serving side's peer block from the process environment: the
/// `TOMO_PEER_NAME` the initiator prepended to the remote command, and the
/// client IP from `SSH_CONNECTION`. Returns `None` when neither is present (e.g.
/// a `serve` started by hand outside SSH), so an unknown peer is simply absent.
#[must_use]
pub fn peer_from_ssh_env() -> Option<Peer> {
    peer_from_env_values(
        std::env::var("TOMO_PEER_NAME").ok(),
        std::env::var("SSH_CONNECTION").ok(),
    )
}

/// Pure core of [`peer_from_ssh_env`]: build the serving-side peer block from a
/// raw `TOMO_PEER_NAME` and `SSH_CONNECTION` value (each `None`/empty when
/// unset). Unit-tested without touching the process environment.
#[must_use]
pub fn peer_from_env_values(
    peer_name: Option<String>,
    ssh_connection: Option<String>,
) -> Option<Peer> {
    let name = peer_name.filter(|s| !s.is_empty());
    let addr = ssh_connection.and_then(|v| client_ip_from_ssh_connection(&v));
    if name.is_none() && addr.is_none() {
        return None;
    }
    Some(Peer {
        name,
        addr,
        source: Some("ssh-env".to_owned()),
    })
}

/// Session network counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Net {
    /// Protocol frames written to the peer.
    pub frames_sent: u64,
    /// Protocol frames read from the peer.
    pub frames_recv: u64,
    /// Payload bytes written to the peer (frame bodies).
    pub bytes_sent: u64,
    /// Payload bytes read from the peer (frame bodies).
    pub bytes_recv: u64,
}

/// Current wall time in Unix milliseconds (display only).
pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Derive `(root, files, tombstones)` from an index by inspecting each entry's
/// materialized winner (the disk-facing state).
pub fn summarize(index: &Index) -> (String, u64, u64) {
    let root = blake3::hash(&index.canonical_bytes()).to_hex().to_string();
    let mut files = 0u64;
    let mut tombstones = 0u64;
    for (_, entry) in index.iter() {
        match entry.winner().state {
            EntryState::Present(_) => files += 1,
            EntryState::Tombstone => tombstones += 1,
        }
    }
    (root, files, tombstones)
}

impl Status {
    /// Build a live status from the current index and session state.
    #[allow(clippy::too_many_arguments)] // one cohesive snapshot; splitting obscures it.
    pub fn live(
        index: &Index,
        conflicts: u64,
        conflicts_unresolved: u64,
        net: Net,
        connected: bool,
        reconciling: bool,
        history: Option<History>,
        fs: Option<crate::fsprobe::FsSemantics>,
    ) -> Self {
        let (root, files, tombstones) = summarize(index);
        Self {
            root,
            files,
            tombstones,
            conflicts,
            conflicts_unresolved,
            net: Some(net),
            connected,
            paused: false,
            peer_paused: false,
            peer: None,
            reconciling,
            history,
            fs,
            updated_unix_ms: now_unix_ms(),
        }
    }

    /// Build an offline status from a persisted index (no session): `net` null,
    /// `connected` false. `history` carries the configured mode with no session
    /// counters (there is no running loop to have recorded anything).
    /// `conflicts_unresolved` is filled in by the caller from the history store.
    pub fn offline(index: &Index, conflicts_unresolved: u64, history: Option<History>) -> Self {
        let (root, files, tombstones) = summarize(index);
        Self {
            root,
            files,
            tombstones,
            conflicts: 0,
            conflicts_unresolved,
            net: None,
            connected: false,
            paused: false,
            peer_paused: false,
            peer: None,
            reconciling: false,
            history,
            fs: None,
            updated_unix_ms: now_unix_ms(),
        }
    }

    /// Serialize to pretty JSON.
    ///
    /// # Errors
    /// [`CliError::Message`] if serialization fails (unreachable for this
    /// plain-data type, but surfaced rather than panicked on).
    pub fn to_json(&self) -> Result<String, CliError> {
        serde_json::to_string_pretty(self)
            .map_err(|e| CliError::msg(format!("could not serialize status: {e}")))
    }
}

/// Atomically write `status` to `layout`'s `status.json`.
///
/// # Errors
/// [`CliError::Message`] on serialization failure or [`CliError::Io`] on write
/// failure.
pub fn write_status(layout: &Layout, status: &Status) -> Result<(), CliError> {
    let json = serde_json::to_vec_pretty(status)
        .map_err(|e| CliError::msg(format!("could not serialize status: {e}")))?;
    atomic_write(&layout.staging(), &layout.status(), &json)
}

/// The peer's name from the persisted `status.json`, ignoring freshness — a
/// best-effort read for naming conflict sides in `tomo conflicts show` when no
/// live session is attached. `None` when the file is missing, unreadable, or
/// carries no peer name.
pub(crate) fn persisted_peer_name(layout: &Layout) -> Option<String> {
    let text = std::fs::read_to_string(layout.status()).ok()?;
    let status: Status = serde_json::from_str(&text).ok()?;
    status.peer.and_then(|p| p.name)
}

/// Maximum age of `status.json` at which `tomo status` trusts it as "live".
const FRESH_WINDOW_MS: u128 = 5_000;

/// Read a *fresh* status file (modified within the last [`FRESH_WINDOW_MS`]),
/// returning `None` if it is missing, stale, or unreadable.
fn read_fresh(path: &Path) -> Option<Status> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let age = SystemTime::now().duration_since(modified).ok()?;
    if age.as_millis() > FRESH_WINDOW_MS {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Run `tomo status`: print the live snapshot if fresh, else an offline
/// computation from the persisted index.
///
/// # Errors
/// [`CliError::Message`] if the project is not initialized, or the index /
/// status cannot be read.
pub fn run(layout: &Layout, json: bool) -> Result<(), CliError> {
    if !layout.is_initialized() {
        return Err(CliError::msg(
            "not a Tomo project (no .tomo/ here) — run `tomo init` first",
        ));
    }

    // The unresolved-conflict count comes straight from the history DB so the
    // badge is always accurate, whether or not a `watch` is running and even if
    // an older status file predates the field.
    let unresolved = unresolved_conflicts(layout);

    let mut status = if let Some(fresh) = read_fresh(&layout.status()) {
        fresh
    } else {
        // Tolerate an undecodable (older-format) index: `tomo status` shows an
        // empty tree until a live session rescans, rather than erroring.
        let (index, _recovered) = crate::persist::load_index(&layout.index())?;
        // No live session: surface the configured mode with no counters, so
        // `tomo status` still reports how history would be captured.
        let history = tomo_config::Config::load(layout.root())
            .ok()
            .map(|cfg| History {
                mode: crate::histmode::label(&cfg.history.mode).to_owned(),
                versions_recorded: 0,
                conflicts_recorded: 0,
                staged: 0,
                rung: 0,
            });
        Status::offline(&index, unresolved.unwrap_or(0), history)
    };
    if let Some(n) = unresolved {
        status.conflicts_unresolved = n;
    }
    // For an offline computation (no live session wrote the file) fill the peer
    // block from the configured `[remote]`, so `status --json` names the peer
    // even with nothing running. A fresh live file already carries its own peer
    // block, which we leave untouched.
    if status.peer.is_none() {
        status.peer = tomo_config::Config::load(layout.root())
            .ok()
            .and_then(|c| c.remote)
            .map(|r| Peer {
                name: Some(r.host),
                addr: None,
                source: Some("config".to_owned()),
            });
    }

    if json {
        outln!("{}", status.to_json()?);
    } else {
        // `dir`/`peer` feed the styled header only; plain output ignores them and
        // stays byte-identical to the historical block. Prefer the richer peer
        // block (name + addr) now recorded in the status; fall back to the bare
        // configured host so the label never regresses.
        let dir = layout
            .root()
            .file_name()
            .and_then(|s| s.to_str())
            .map_or_else(|| layout.root().display().to_string(), str::to_owned);
        let peer = peer_label(&status).or_else(|| {
            tomo_config::Config::load(layout.root())
                .ok()
                .and_then(|c| c.remote.map(|r| r.host))
        });
        print_human(&status, &dir, peer.as_deref());
    }
    Ok(())
}

/// Count conflict rows in the history DB still awaiting acknowledgement.
/// Returns `None` if the store cannot be opened or queried, so `tomo status`
/// falls back to whatever the (possibly stale) status file recorded rather than
/// failing outright.
fn unresolved_conflicts(layout: &Layout) -> Option<u64> {
    // Read-only: a status poll must NEVER take write locks on the history DB
    // (a poll racing a starting session's open once killed the session).
    match tomo_history::HistoryStore::open_readonly(layout.root()) {
        Ok(Some(store)) => Some(store.conflicts(true).ok()?.len() as u64),
        Ok(None) => Some(0),
        Err(_) => None,
    }
}

/// A one-line peer label (`name (addr)`, `name`, or `addr`) from the status's
/// peer block, or `None` when the block is absent or wholly empty.
fn peer_label(status: &Status) -> Option<String> {
    let peer = status.peer.as_ref()?;
    match (&peer.name, &peer.addr) {
        (Some(name), Some(addr)) => Some(format!("{name} ({addr})")),
        (Some(name), None) => Some(name.clone()),
        (None, Some(addr)) => Some(addr.clone()),
        (None, None) => None,
    }
}

fn print_human(status: &Status, dir: &str, peer: Option<&str>) {
    let style = crate::style::current();
    if style.enabled() {
        print_human_styled(status, dir, peer, style);
        return;
    }
    // Plain path: byte-identical to the historical `tomo status` block.
    let conn = if status.connected {
        "connected"
    } else {
        "offline"
    };
    outln!("root       {}", status.root);
    outln!("files      {}", status.files);
    outln!("tombstones {}", status.tombstones);
    outln!("conflicts  {}", status.conflicts);
    outln!("peer       {conn}");
    if let Some(h) = &status.history {
        outln!("history    {} ({} versions)", h.mode, h.versions_recorded);
    }
    if let Some(net) = status.net {
        outln!(
            "net        sent {}f/{}B  recv {}f/{}B",
            net.frames_sent,
            net.bytes_sent,
            net.frames_recv,
            net.bytes_recv
        );
    }
    // Non-blocking conflict surfacing (invariant #5): a visible badge, nothing
    // that gates sync.
    if let Some(badge) = crate::conflicts_cmd::conflict_badge(status.conflicts_unresolved) {
        outln!("{badge}");
    }
}

/// The styled `tomo status` block: a 友-marked header with a connection dot, a
/// dimmed/truncated root, a `·`-separated counts line, and an amber conflict
/// badge. The `root` is truncated to 12 hex here (full in plain/JSON).
fn print_human_styled(status: &Status, dir: &str, peer: Option<&str>, style: crate::style::Style) {
    use crate::style::group_thousands;

    let (dot, label) = if status.connected {
        (style.ok(style.g_dot_on()), style.ok("connected"))
    } else {
        (style.dim(style.g_dot_off()), style.dim("offline"))
    };
    let kanji = style.g_kanji();
    let mark = if kanji.is_empty() {
        String::new()
    } else {
        format!("{} ", style.accent(kanji))
    };
    let peer_frag = peer.map_or_else(String::new, |p| {
        format!("  {} {}", style.dim(style.g_sync()), style.accent(p))
    });
    outln!("{mark}{}{peer_frag}  {dot} {label}", style.accent(dir));

    let short: String = status.root.chars().take(12).collect();
    outln!("{}  {}", style.dim("root"), style.dim(&short));

    let sep = format!(" {} ", style.dim("·"));
    let mut parts = vec![
        format!("files {}", style.bold(&group_thousands(status.files))),
        format!("tombstones {}", group_thousands(status.tombstones)),
        format!("conflicts {}", group_thousands(status.conflicts)),
    ];
    if let Some(h) = &status.history {
        parts.push(format!(
            "versions {}",
            style.bold(&group_thousands(h.versions_recorded))
        ));
        parts.push(format!("history {}", h.mode));
    }
    outln!("{}", parts.join(&sep));

    if let Some(net) = status.net {
        outln!(
            "{}",
            style.dim(&format!(
                "net  sent {}f/{}B  recv {}f/{}B",
                net.frames_sent, net.bytes_sent, net.frames_recv, net.bytes_recv
            ))
        );
    }
    // Non-blocking conflict surfacing (invariant #5), rendered amber.
    if let Some(badge) = crate::conflicts_cmd::conflict_badge(status.conflicts_unresolved) {
        outln!("{}", style.warn(&badge));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tomo_engine::{
        ChangeKind, Engine, Event, LocalChange, RelPath, ReplicaId, {ContentHash, ContentSig},
    };

    fn sig(byte: u8) -> ContentSig {
        ContentSig {
            hash: ContentHash([byte; 32]),
            size: u64::from(byte),
            exec: false,
            mtime_ms: 0,
        }
    }

    fn index_with(present: &[&str], removed: &[&str]) -> Index {
        let mut e = Engine::new(ReplicaId(1), Index::new());
        for (i, p) in present.iter().enumerate() {
            e.handle(Event::Local(LocalChange {
                path: RelPath::new(p).unwrap(),
                #[allow(clippy::cast_possible_truncation)]
                kind: ChangeKind::Modified(sig(i as u8 + 1)),
            }));
        }
        for p in removed {
            e.handle(Event::Local(LocalChange {
                path: RelPath::new(p).unwrap(),
                kind: ChangeKind::Modified(sig(200)),
            }));
            e.handle(Event::Local(LocalChange {
                path: RelPath::new(p).unwrap(),
                kind: ChangeKind::Removed,
            }));
        }
        e.index().clone()
    }

    #[test]
    fn summarize_counts_winners() {
        let idx = index_with(&["a.txt", "b/c.txt"], &["gone"]);
        let (root, files, tombstones) = summarize(&idx);
        assert_eq!(files, 2);
        assert_eq!(tombstones, 1);
        assert_eq!(root.len(), 64);
        assert!(root.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn root_matches_blake3_of_canonical_bytes() {
        let idx = index_with(&["a.txt"], &[]);
        let (root, _, _) = summarize(&idx);
        assert_eq!(
            root,
            blake3::hash(&idx.canonical_bytes()).to_hex().to_string()
        );
    }

    #[test]
    fn offline_status_has_null_net_and_disconnected() {
        let idx = index_with(&["a.txt"], &[]);
        let s = Status::offline(&idx, 0, None);
        assert!(s.net.is_none());
        assert!(!s.connected);
        assert_eq!(s.files, 1);
    }

    fn sample_history() -> History {
        History {
            mode: "adaptive".to_owned(),
            versions_recorded: 7,
            conflicts_recorded: 1,
            staged: 2,
            rung: 3,
        }
    }

    #[test]
    fn status_json_round_trips_and_has_expected_keys() {
        let idx = index_with(&["a.txt"], &["g"]);
        let net = Net {
            frames_sent: 3,
            frames_recv: 4,
            bytes_sent: 10,
            bytes_recv: 20,
        };
        let fs = crate::fsprobe::FsSemantics {
            case_insensitive: true,
            normalizes_unicode: true,
        };
        let s = Status::live(
            &idx,
            1,
            2,
            net,
            true,
            false,
            Some(sample_history()),
            Some(fs),
        );
        let json = s.to_json().unwrap();
        for key in [
            "root",
            "files",
            "tombstones",
            "conflicts",
            "conflicts_unresolved",
            "net",
            "frames_sent",
            "connected",
            "paused",
            "peer_paused",
            "peer",
            "history",
            "versions_recorded",
            "conflicts_recorded",
            "staged",
            "rung",
            "fs",
            "case_insensitive",
            "normalizes_unicode",
            "updated_unix_ms",
        ] {
            assert!(json.contains(key), "missing key {key} in {json}");
        }
        let back: Status = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
        assert_eq!(back.history.as_ref().unwrap().versions_recorded, 7);
    }

    #[test]
    fn ssh_connection_client_ip_parsing() {
        // Well-formed: first field is the client IP.
        assert_eq!(
            client_ip_from_ssh_connection("203.0.113.7 51876 10.0.0.2 22").as_deref(),
            Some("203.0.113.7")
        );
        // IPv6 loopback, extra spacing tolerated.
        assert_eq!(
            client_ip_from_ssh_connection("::1  12345 ::1 22").as_deref(),
            Some("::1")
        );
        // Malformed / empty inputs yield None.
        assert_eq!(client_ip_from_ssh_connection(""), None);
        assert_eq!(client_ip_from_ssh_connection("   "), None);
        // A single token (truncated var) still yields that token.
        assert_eq!(
            client_ip_from_ssh_connection("127.0.0.1").as_deref(),
            Some("127.0.0.1")
        );
    }

    #[test]
    fn peer_from_env_values_present_and_absent() {
        // Both present → full block, source ssh-env.
        let p = peer_from_env_values(
            Some("jakes-mbp".to_owned()),
            Some("127.0.0.1 5000 127.0.0.1 22".to_owned()),
        )
        .unwrap();
        assert_eq!(p.name.as_deref(), Some("jakes-mbp"));
        assert_eq!(p.addr.as_deref(), Some("127.0.0.1"));
        assert_eq!(p.source.as_deref(), Some("ssh-env"));

        // Name only (no SSH_CONNECTION) → addr null but block still present.
        let p = peer_from_env_values(Some("host".to_owned()), None).unwrap();
        assert_eq!(p.name.as_deref(), Some("host"));
        assert!(p.addr.is_none());

        // Addr only (env not prepended) → name null.
        let p = peer_from_env_values(None, Some("::1 22 ::1 22".to_owned())).unwrap();
        assert!(p.name.is_none());
        assert_eq!(p.addr.as_deref(), Some("::1"));

        // Neither → no block at all.
        assert!(peer_from_env_values(None, None).is_none());
        // Empty name + malformed connection → no block.
        assert!(peer_from_env_values(Some(String::new()), Some("   ".to_owned())).is_none());
    }

    #[test]
    fn history_block_is_backward_compatible_when_absent() {
        // A status document written before the history block existed must still
        // deserialize, defaulting `history` to None.
        let json = r#"{
            "root": "deadbeef",
            "files": 1,
            "tombstones": 0,
            "conflicts": 0,
            "net": null,
            "connected": false,
            "updated_unix_ms": 123
        }"#;
        let s: Status = serde_json::from_str(json).unwrap();
        assert!(s.history.is_none());
        assert_eq!(s.files, 1);
    }
}
