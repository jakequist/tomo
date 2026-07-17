//! The index model: the engine's authoritative view of the tree.
//!
//! An [`Index`] maps every [`RelPath`] to an [`Entry`] carrying its content
//! identity and vector-clock version. Deleted files are retained as
//! [`EntryState::Tombstone`]s so deletions propagate and delete-vs-edit
//! conflicts remain detectable (docs/SPEC.md §5.3).
//!
//! The engine never hashes bytes: [`ContentHash`] values are computed by the
//! `tomo-history` adapter (BLAKE3) and handed in. The engine only compares and
//! serializes them.

use std::collections::BTreeMap;
use std::fmt;

use crate::clock::VectorClock;
use crate::path::RelPath;

/// A 32-byte content hash (BLAKE3), opaque to the engine.
///
/// The engine treats it as an identity token: it is compared and encoded but
/// never computed here (that would be I/O). [`fmt::Display`] renders it as
/// 64 lowercase hex characters.
///
/// ```
/// use tomo_engine::ContentHash;
/// let h = ContentHash([0xab; 32]);
/// assert_eq!(h.to_string(), "ab".repeat(32));
/// ```
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct ContentHash(pub [u8; 32]);

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// The identity of a file's content: its hash and its byte length.
///
/// Size is carried alongside the hash so cheap size checks can short-circuit
/// comparisons and so the canonical digest is unambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContentSig {
    /// Content hash of the file's bytes.
    pub hash: ContentHash,
    /// Length of the file's content in bytes.
    pub size: u64,
}

/// The state of an indexed path: present with content, or a tombstone.
///
/// Tombstones are kept rather than removed so that a deletion is a versioned
/// fact that can propagate to the peer and lose or win a conflict like any
/// other change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EntryState {
    /// The file exists with the given content signature.
    Present(ContentSig),
    /// The file has been deleted; the entry is retained as a tombstone.
    Tombstone,
}

/// An index entry: what a path is, and the version at which it became so.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    /// Vector-clock version stamped when this state was recorded.
    pub version: VectorClock,
    /// Whether the path is present (with content) or tombstoned.
    pub state: EntryState,
}

/// The engine's map from path to [`Entry`].
///
/// Backed by a `BTreeMap`, so iteration and the canonical digest are
/// deterministic across replicas — a prerequisite for the "equal roots"
/// convergence assertion.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Index {
    entries: BTreeMap<RelPath, Entry>,
}

impl Index {
    /// An empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// The entry for `path`, if any (including tombstones).
    pub fn get(&self, path: &RelPath) -> Option<&Entry> {
        self.entries.get(path)
    }

    /// Insert or replace the entry at `path`, returning the previous entry.
    pub fn upsert(&mut self, path: RelPath, entry: Entry) -> Option<Entry> {
        self.entries.insert(path, entry)
    }

    /// Iterate `(path, entry)` pairs in ascending path order.
    pub fn iter(&self) -> impl Iterator<Item = (&RelPath, &Entry)> + '_ {
        self.entries.iter()
    }

    /// Number of entries (present and tombstoned).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the index has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// A deterministic byte serialization of the whole index.
    ///
    /// Adapters hash this to produce a single "root" digest; two indices with
    /// identical logical content always yield identical bytes, and any
    /// difference in content yields different bytes (the encoding is
    /// injective, via length prefixes). This is the mechanism behind the
    /// equal-roots convergence assertion (docs/TESTING.md).
    ///
    /// # Format (all integers little-endian)
    /// A stream of records, one per entry, in ascending [`RelPath`] order:
    /// 1. `u64` path length in bytes, then the UTF-8 path bytes;
    /// 2. `u64` version length `n` (number of clock entries), then `n`
    ///    `(u64 replica_id, u64 counter)` pairs in ascending replica order;
    /// 3. one state-tag byte — `0` = tombstone, `1` = present;
    /// 4. if present: the 32 hash bytes followed by the `u64` size.
    ///
    /// The format is intentionally boring: stability across releases and
    /// replicas is the only goal, so it is documented and length-prefixed
    /// rather than clever.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (path, entry) in &self.entries {
            let path_bytes = path.as_str().as_bytes();
            out.extend_from_slice(&(path_bytes.len() as u64).to_le_bytes());
            out.extend_from_slice(path_bytes);

            let clock: Vec<(u64, u64)> = entry.version.iter().map(|(r, c)| (r.0, c)).collect();
            out.extend_from_slice(&(clock.len() as u64).to_le_bytes());
            for (replica, counter) in clock {
                out.extend_from_slice(&replica.to_le_bytes());
                out.extend_from_slice(&counter.to_le_bytes());
            }

            match entry.state {
                EntryState::Tombstone => out.push(0),
                EntryState::Present(sig) => {
                    out.push(1);
                    out.extend_from_slice(&sig.hash.0);
                    out.extend_from_slice(&sig.size.to_le_bytes());
                }
            }
        }
        out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // fine in tests
mod tests {
    use super::*;
    use crate::clock::ReplicaId;
    use proptest::prelude::*;

    fn sig(byte: u8, size: u64) -> ContentSig {
        ContentSig {
            hash: ContentHash([byte; 32]),
            size,
        }
    }

    fn present(byte: u8, size: u64) -> Entry {
        Entry {
            version: VectorClock::new(),
            state: EntryState::Present(sig(byte, size)),
        }
    }

    #[test]
    fn content_hash_hex_display() {
        let mut bytes = [0u8; 32];
        bytes[0] = 0x0f;
        bytes[31] = 0xa0;
        let h = ContentHash(bytes);
        let s = h.to_string();
        assert_eq!(s.len(), 64);
        assert!(s.starts_with("0f"));
        assert!(s.ends_with("a0"));
    }

    #[test]
    fn upsert_get_and_replace() {
        let mut idx = Index::new();
        assert!(idx.is_empty());
        let p = RelPath::new("a/b.txt").unwrap();

        assert!(idx.upsert(p.clone(), present(1, 10)).is_none());
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.get(&p), Some(&present(1, 10)));

        let prev = idx.upsert(p.clone(), present(2, 20));
        assert_eq!(prev, Some(present(1, 10)));
        assert_eq!(idx.get(&p), Some(&present(2, 20)));
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn tombstone_is_retained_and_distinct() {
        let mut idx = Index::new();
        let p = RelPath::new("gone.txt").unwrap();
        idx.upsert(p.clone(), present(1, 5));
        let tomb = Entry {
            version: VectorClock::new(),
            state: EntryState::Tombstone,
        };
        idx.upsert(p.clone(), tomb.clone());
        assert_eq!(idx.get(&p), Some(&tomb));
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn iter_is_sorted_by_path() {
        let mut idx = Index::new();
        idx.upsert(RelPath::new("b").unwrap(), present(1, 1));
        idx.upsert(RelPath::new("a").unwrap(), present(1, 1));
        idx.upsert(RelPath::new("a/c").unwrap(), present(1, 1));
        let paths: Vec<&str> = idx.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, ["a", "a/c", "b"]);
    }

    #[test]
    fn canonical_bytes_is_insertion_order_independent() {
        let mut a = Index::new();
        a.upsert(RelPath::new("x").unwrap(), present(1, 1));
        a.upsert(RelPath::new("y").unwrap(), present(2, 2));
        let mut b = Index::new();
        b.upsert(RelPath::new("y").unwrap(), present(2, 2));
        b.upsert(RelPath::new("x").unwrap(), present(1, 1));
        assert_eq!(a.canonical_bytes(), b.canonical_bytes());
    }

    /// A strategy producing small arbitrary indices.
    fn arb_index() -> impl Strategy<Value = Index> {
        let entry = (
            proptest::collection::btree_map(0u64..3, 1u64..4, 0..3),
            prop_oneof![
                (any::<u8>(), 0u64..1000).prop_map(|(b, s)| EntryState::Present(sig(b, s))),
                Just(EntryState::Tombstone),
            ],
        )
            .prop_map(|(clock_spec, state)| {
                let mut version = VectorClock::new();
                for (r, count) in clock_spec {
                    for _ in 0..count {
                        version.tick(ReplicaId(r));
                    }
                }
                Entry { version, state }
            });
        proptest::collection::btree_map("[a-z]{1,4}", entry, 0..5).prop_map(|m| {
            let mut idx = Index::new();
            for (name, e) in m {
                let path = RelPath::new(&name).expect("generated name is a valid path");
                idx.upsert(path, e);
            }
            idx
        })
    }

    proptest! {
        /// Deterministic: encoding the same index twice yields the same bytes.
        #[test]
        fn canonical_bytes_deterministic(idx in arb_index()) {
            prop_assert_eq!(idx.canonical_bytes(), idx.clone().canonical_bytes());
        }

        /// Injective: two indices are logically equal iff their canonical
        /// bytes are equal (equal ⇒ equal bytes; differ ⇒ differ bytes).
        #[test]
        fn canonical_bytes_iff_equal(a in arb_index(), b in arb_index()) {
            prop_assert_eq!(a == b, a.canonical_bytes() == b.canonical_bytes());
        }
    }
}
