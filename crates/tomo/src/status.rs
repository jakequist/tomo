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
    pub fn live(
        index: &Index,
        conflicts: u64,
        conflicts_unresolved: u64,
        net: Net,
        connected: bool,
        reconciling: bool,
        history: Option<History>,
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
            reconciling,
            history,
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
            reconciling: false,
            history,
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
        let index = crate::persist::load_index(&layout.index())?;
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

    if json {
        outln!("{}", status.to_json()?);
    } else {
        // `dir`/`peer` feed the styled header only; plain output ignores them and
        // stays byte-identical to the historical block.
        let dir = layout
            .root()
            .file_name()
            .and_then(|s| s.to_str())
            .map_or_else(|| layout.root().display().to_string(), str::to_owned);
        let peer = tomo_config::Config::load(layout.root())
            .ok()
            .and_then(|c| c.remote.map(|r| r.host));
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
        let s = Status::live(&idx, 1, 2, net, true, false, Some(sample_history()));
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
            "history",
            "versions_recorded",
            "conflicts_recorded",
            "staged",
            "rung",
            "updated_unix_ms",
        ] {
            assert!(json.contains(key), "missing key {key} in {json}");
        }
        let back: Status = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
        assert_eq!(back.history.as_ref().unwrap().versions_recorded, 7);
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
