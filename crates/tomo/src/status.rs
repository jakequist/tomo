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
    /// Network counters while a session is live; `null` for an offline
    /// computation.
    pub net: Option<Net>,
    /// Whether a peer session is currently connected.
    pub connected: bool,
    /// Wall-clock time of this snapshot in Unix milliseconds. Display only.
    pub updated_unix_ms: u64,
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
    pub fn live(index: &Index, conflicts: u64, net: Net, connected: bool) -> Self {
        let (root, files, tombstones) = summarize(index);
        Self {
            root,
            files,
            tombstones,
            conflicts,
            net: Some(net),
            connected,
            updated_unix_ms: now_unix_ms(),
        }
    }

    /// Build an offline status from a persisted index (no session): `net` null,
    /// `connected` false.
    pub fn offline(index: &Index) -> Self {
        let (root, files, tombstones) = summarize(index);
        Self {
            root,
            files,
            tombstones,
            conflicts: 0,
            net: None,
            connected: false,
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

    let status = if let Some(fresh) = read_fresh(&layout.status()) {
        fresh
    } else {
        let index = crate::persist::load_index(&layout.index())?;
        Status::offline(&index)
    };

    if json {
        println!("{}", status.to_json()?);
    } else {
        print_human(&status);
    }
    Ok(())
}

fn print_human(status: &Status) {
    let conn = if status.connected {
        "connected"
    } else {
        "offline"
    };
    println!("root       {}", status.root);
    println!("files      {}", status.files);
    println!("tombstones {}", status.tombstones);
    println!("conflicts  {}", status.conflicts);
    println!("peer       {conn}");
    if let Some(net) = status.net {
        println!(
            "net        sent {}f/{}B  recv {}f/{}B",
            net.frames_sent, net.bytes_sent, net.frames_recv, net.bytes_recv
        );
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
        let s = Status::offline(&idx);
        assert!(s.net.is_none());
        assert!(!s.connected);
        assert_eq!(s.files, 1);
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
        let s = Status::live(&idx, 1, net, true);
        let json = s.to_json().unwrap();
        for key in [
            "root",
            "files",
            "tombstones",
            "conflicts",
            "net",
            "frames_sent",
            "connected",
            "updated_unix_ms",
        ] {
            assert!(json.contains(key), "missing key {key} in {json}");
        }
        let back: Status = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }
}
