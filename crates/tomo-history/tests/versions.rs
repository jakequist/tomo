//! Version-log fidelity (docs/TESTING.md scenario 05, Level-1 shadow), plus
//! tombstones, conflicts, and the `latest_version_id` accessor.
#![allow(clippy::unwrap_used, clippy::expect_used)] // tests

mod common;

use common::{clock_at, fresh_store, record_present, rp, sig_of};
use tomo_engine::{EntryState, ReplicaId};
use tomo_history::Origin;

#[test]
fn n_sequential_versions_log_in_order_and_restore() {
    let (_dir, mut store) = fresh_store();
    let path = rp("src/main.rs");

    let n = 12u64;
    let mut expected = Vec::new();
    for i in 1..=n {
        let bytes = format!("edit number {i}\n")
            .repeat(usize::try_from(i).unwrap())
            .into_bytes();
        let id = record_present(&mut store, &path, &bytes, 1, i);
        expected.push((id, bytes));
    }

    // log() is newest-first and returns exactly N entries.
    let log = store.log(&path).expect("log");
    assert_eq!(
        log.len(),
        usize::try_from(n).unwrap(),
        "one version per edit"
    );
    let logged_ids: Vec<_> = log.iter().map(|m| m.id).collect();
    let mut expected_ids: Vec<_> = expected.iter().map(|(id, _)| *id).collect();
    expected_ids.reverse(); // newest first
    assert_eq!(logged_ids, expected_ids, "log is newest-first");

    // Every recorded version restores byte-identically.
    for (id, bytes) in &expected {
        assert_eq!(&store.get_content(*id).unwrap(), bytes, "version {id:?}");
    }

    // latest_version_id points at the final edit.
    assert_eq!(
        store.latest_version_id(&path).unwrap(),
        Some(expected.last().unwrap().0)
    );
}

#[test]
fn log_carries_clock_replica_and_origin() {
    let (_dir, mut store) = fresh_store();
    let path = rp("a.txt");
    let bytes = b"hello";
    let id = store
        .record_version(
            &path,
            &EntryState::Present(sig_of(bytes)),
            &clock_at(2, 3),
            ReplicaId(2),
            Origin::Remote,
            123_456,
            Some(bytes),
        )
        .unwrap();

    let log = store.log(&path).unwrap();
    assert_eq!(log.len(), 1);
    let m = &log[0];
    assert_eq!(m.id, id);
    assert_eq!(m.replica, ReplicaId(2));
    assert_eq!(m.origin, Origin::Remote);
    assert_eq!(m.wall_ms, 123_456);
    assert_eq!(m.clock, clock_at(2, 3));
    assert_eq!(m.content_hash, Some(sig_of(bytes).hash));
    assert_eq!(m.size, Some(bytes.len() as u64));
    assert_eq!(m.state, EntryState::Present(sig_of(bytes)));
}

#[test]
fn tombstone_versions_round_trip_without_content() {
    let (_dir, mut store) = fresh_store();
    let path = rp("gone.txt");

    let present_id = record_present(&mut store, &path, b"here", 1, 1);
    let tomb_id = store
        .record_version(
            &path,
            &EntryState::Tombstone,
            &clock_at(1, 2),
            ReplicaId(1),
            Origin::Local,
            0,
            None,
        )
        .expect("record tombstone");

    let log = store.log(&path).unwrap();
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].id, tomb_id);
    assert_eq!(log[0].state, EntryState::Tombstone);
    assert_eq!(log[0].content_hash, None);
    assert_eq!(log[0].size, None);

    // The present version still restores; the tombstone has no content.
    assert_eq!(store.get_content(present_id).unwrap(), b"here");
    assert!(
        store.get_content(tomb_id).is_err(),
        "tombstone has no content"
    );
}

#[test]
fn present_version_without_bytes_is_rejected() {
    let (_dir, mut store) = fresh_store();
    let path = rp("x");
    let err = store
        .record_version(
            &path,
            &EntryState::Present(sig_of(b"data")),
            &clock_at(1, 1),
            ReplicaId(1),
            Origin::Local,
            0,
            None,
        )
        .expect_err("present without bytes must fail");
    assert!(matches!(
        err,
        tomo_history::HistoryError::MissingContent { .. }
    ));
    // Nothing was persisted (the transaction rolled back).
    assert_eq!(store.log(&path).unwrap().len(), 0);
}

#[test]
fn mismatched_signature_is_rejected() {
    let (_dir, mut store) = fresh_store();
    let path = rp("x");
    // Declare the signature of different bytes than we pass.
    let wrong_sig = sig_of(b"the declared content");
    let err = store
        .record_version(
            &path,
            &EntryState::Present(wrong_sig),
            &clock_at(1, 1),
            ReplicaId(1),
            Origin::Local,
            0,
            Some(b"the actual content"),
        )
        .expect_err("signature mismatch must fail");
    assert!(matches!(
        err,
        tomo_history::HistoryError::SigMismatch { .. }
    ));
    assert_eq!(store.log(&path).unwrap().len(), 0, "rolled back");
}

#[test]
fn conflicts_are_recorded_and_queried() {
    let (_dir, mut store) = fresh_store();
    let path = rp("contested.txt");
    let winner = record_present(&mut store, &path, b"winner", 1, 1);
    let loser = record_present(&mut store, &path, b"loser", 2, 1);

    let cid = store.record_conflict(&path, winner, loser, 999).unwrap();

    let all = store.conflicts(false).unwrap();
    assert_eq!(all.len(), 1);
    let c = &all[0];
    assert_eq!(c.id, cid);
    assert_eq!(c.path, path);
    assert_eq!(c.winner, winner);
    assert_eq!(c.loser, loser);
    assert_eq!(c.wall_ms, 999);
    assert!(!c.resolved);

    // unresolved_only returns the same open conflict.
    assert_eq!(store.conflicts(true).unwrap().len(), 1);
}

#[test]
fn mark_conflict_resolved_flips_the_flag_idempotently() {
    let (_dir, mut store) = fresh_store();
    let path = rp("contested.txt");
    let winner = record_present(&mut store, &path, b"winner", 1, 1);
    let loser = record_present(&mut store, &path, b"loser", 2, 1);
    let cid = store.record_conflict(&path, winner, loser, 999).unwrap();

    // Present in the unresolved set.
    assert_eq!(store.conflicts(true).unwrap().len(), 1);

    // First resolve flips the flag and reports the change.
    assert!(store.mark_conflict_resolved(cid).unwrap());
    assert_eq!(store.conflicts(true).unwrap().len(), 0);
    // The row still exists in the full listing, now marked resolved.
    let all = store.conflicts(false).unwrap();
    assert_eq!(all.len(), 1);
    assert!(all[0].resolved);

    // Resolving again is a no-op that reports nothing changed.
    assert!(!store.mark_conflict_resolved(cid).unwrap());
    // An unknown id also reports no change (no error).
    assert!(!store
        .mark_conflict_resolved(tomo_history::ConflictId(9999))
        .unwrap());
}

#[test]
fn conflict_referencing_unknown_version_is_rejected() {
    // Foreign keys are ON: a conflict cannot reference a nonexistent version.
    let (_dir, mut store) = fresh_store();
    let path = rp("p");
    let real = record_present(&mut store, &path, b"x", 1, 1);
    let bogus = tomo_history::VersionId(9999);
    assert!(store.record_conflict(&path, real, bogus, 0).is_err());
}

#[test]
fn latest_version_id_is_none_for_unknown_path() {
    let (_dir, store) = fresh_store();
    assert_eq!(store.latest_version_id(&rp("never.txt")).unwrap(), None);
}
