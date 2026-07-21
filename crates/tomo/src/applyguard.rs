//! The pure decision behind the apply guard (docs/NOTES.md "Storm cluster"
//! item 3; invariant #5 — nothing is lost).
//!
//! An incoming [`tomo_engine::Action::Apply`] must not silently overwrite a
//! concurrent local edit whose watcher event has not been dequeued yet: on a
//! parted/frozen link the peer's frame can be processed before this replica's
//! own watcher reports its local write, and a blind apply would erase the local
//! bytes without a trace. The cure is to detect that case *before* absorbing the
//! remote change and feed the observed disk state through the engine first, so
//! it becomes a head *concurrent* to the incoming one and the ordinary conflict
//! machinery decides a deterministic winner while preserving the loser.
//!
//! This function is the pure predicate for "does disk hold an unobserved local
//! edit that must be reconciled first?". All the I/O (snapshotting disk, feeding
//! the engine) lives in [`crate::session`].

use tomo_engine::Expectation;

/// Whether disk holds an unobserved local edit that must be reconciled into the
/// engine before a remote change for the same path is absorbed.
///
/// - `disk`: the CURRENT on-disk state (as an [`Expectation`]).
/// - `prior`: the disk-facing state the engine currently believes (a never-seen
///   path ⇒ [`Expectation::Absent`]).
/// - `disk_is_echo`: whether `disk` matches an outstanding echo expectation
///   (i.e. it reflects the engine's own pending write, not a user edit).
///
/// Reconcile exactly when disk diverges from the engine's belief and that
/// divergence is not our own echo. When disk already equals `prior` (the normal
/// case) or is our echo, there is nothing unobserved and the caller absorbs the
/// remote change directly.
#[must_use]
pub fn needs_local_reconcile(disk: &Expectation, prior: &Expectation, disk_is_echo: bool) -> bool {
    disk != prior && !disk_is_echo
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tomo_engine::{ContentHash, ContentSig};

    fn present(byte: u8) -> Expectation {
        Expectation::Present(ContentSig {
            hash: ContentHash([byte; 32]),
            size: u64::from(byte),
            exec: false,
            mtime_ms: 0,
        })
    }

    const ABSENT: Expectation = Expectation::Absent;

    #[test]
    fn no_reconcile_when_disk_matches_belief() {
        // Normal case: disk is exactly what the engine believes.
        assert!(!needs_local_reconcile(&present(1), &present(1), false));
        assert!(!needs_local_reconcile(&ABSENT, &ABSENT, false));
    }

    #[test]
    fn no_reconcile_when_disk_is_our_echo() {
        // Disk reflects our own pending apply (echo), not a user edit — even
        // though it differs from the current belief, it must not be reconciled.
        assert!(!needs_local_reconcile(&present(3), &present(1), true));
        assert!(!needs_local_reconcile(&ABSENT, &present(1), true));
    }

    #[test]
    fn reconcile_on_unobserved_local_edit() {
        // Disk diverges from belief and is not our echo → an unprocessed local
        // edit that must be reconciled first (else the incoming apply clobbers
        // it). Covers modify, local delete, and local create.
        assert!(needs_local_reconcile(&present(9), &present(1), false)); // local re-edit
        assert!(needs_local_reconcile(&ABSENT, &present(1), false)); // local delete
        assert!(needs_local_reconcile(&present(9), &ABSENT, false)); // local create
    }
}
