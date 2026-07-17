//! Canonical change events fed into the sync engine.
//!
//! These are the *normalized* inputs the engine consumes. The adapters do the
//! messy work of producing them: `tomo-watch` turns raw FSEvents/inotify noise
//! into [`LocalChange`]s, and `tomo-transport` delivers peer changes as
//! [`RemoteChange`]s. The engine's transition function (M1) is defined purely
//! over these types.
//!
//! # Canonicalization contract
//! By the time a change reaches the engine it must already be coherent:
//! - Creation and modification are the *same* event: [`ChangeKind::Modified`].
//!   The engine does not distinguish "new file" from "edited file"; both carry
//!   the resulting [`ContentSig`].
//! - Editor atomic saves (write-temp-then-rename, truncate-then-write) look
//!   like delete+create at the raw-event layer. `tomo-watch` MUST collapse
//!   them into a single [`ChangeKind::Modified`] with the final content — never
//!   a [`ChangeKind::Removed`] followed by a `Modified`, and never a zero-byte
//!   intermediate (docs/SPEC.md §5.1).
//! - A change matching a write Tomo itself just performed (an echo) must be
//!   suppressed by the adapter and never reach the engine.

use crate::clock::VectorClock;
use crate::index::ContentSig;
use crate::path::RelPath;

/// What happened to a path, canonically.
///
/// Only two outcomes exist at the engine boundary: the content is now some
/// signature, or the path is gone. Renames are modeled as a `Removed` at the
/// old path plus a `Modified` at the new one by the watch layer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ChangeKind {
    /// The path now has this content (covers both creation and modification).
    Modified(ContentSig),
    /// The path was deleted.
    Removed,
}

/// A canonicalized change observed on the local filesystem.
///
/// Emitted by `tomo-watch` after echo suppression and atomic-save collapsing.
/// It carries no vector clock: the engine stamps the local version when it
/// ingests the change (that decision is engine-owned, M1).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LocalChange {
    /// The affected path.
    pub path: RelPath,
    /// What happened to it.
    pub kind: ChangeKind,
}

/// A change received from the peer replica.
///
/// Unlike a [`LocalChange`], it arrives with the originating replica's
/// [`VectorClock`] already attached — that clock is the causality evidence the
/// engine compares against its own index to decide fast-forward, no-op, or
/// conflict.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteChange {
    /// The affected path.
    pub path: RelPath,
    /// What happened to it.
    pub kind: ChangeKind,
    /// The peer's vector clock for this change (causality evidence).
    pub version: VectorClock,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // fine in tests
mod tests {
    use super::*;
    use crate::clock::ReplicaId;
    use crate::index::ContentHash;

    fn sig() -> ContentSig {
        ContentSig {
            hash: ContentHash([7; 32]),
            size: 42,
        }
    }

    #[test]
    fn local_change_constructs() {
        let c = LocalChange {
            path: RelPath::new("a/b.txt").unwrap(),
            kind: ChangeKind::Modified(sig()),
        };
        assert_eq!(c.kind, ChangeKind::Modified(sig()));
    }

    #[test]
    fn remote_change_carries_clock() {
        let mut v = VectorClock::new();
        v.tick(ReplicaId(1));
        let c = RemoteChange {
            path: RelPath::new("gone").unwrap(),
            kind: ChangeKind::Removed,
            version: v.clone(),
        };
        assert_eq!(c.version, v);
        assert_eq!(c.kind, ChangeKind::Removed);
    }
}
