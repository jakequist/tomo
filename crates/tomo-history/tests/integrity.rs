//! Integrity: `check()` is green on a healthy store, and a chunk corrupted
//! behind the store's back is caught by both `check()` and `get_content`
//! (never returned as silently-wrong bytes).
#![allow(clippy::unwrap_used, clippy::expect_used)] // tests

mod common;

use common::{fresh_store, pseudorandom, record_present, rp};
use rusqlite::{params, Connection};

/// Path to the store's database file inside a project root.
fn db_path(root: &std::path::Path) -> std::path::PathBuf {
    root.join(".tomo").join("db").join("history.sqlite")
}

#[test]
fn check_is_green_on_a_healthy_store() {
    let (_dir, mut store) = fresh_store();
    // A mix of small, boundary, and multi-chunk files, plus a tombstone.
    record_present(&mut store, &rp("a.txt"), b"small", 1, 1);
    record_present(&mut store, &rp("b.bin"), &pseudorandom(500_000, 1), 1, 1);
    record_present(&mut store, &rp("c.bin"), &pseudorandom(2_000_000, 2), 1, 1);

    let report = store.check().expect("check");
    assert!(report.ok, "healthy store should pass: {:?}", report.issues);
    assert!(report.issues.is_empty());
    assert!(report.versions_checked >= 3);
    assert!(report.chunks_checked >= 3);
}

#[test]
fn corrupt_zdata_is_caught_by_check_and_get_content() {
    let (dir, mut store) = fresh_store();
    let path = rp("victim.bin");
    let bytes = pseudorandom(1_500_000, 0xDEAD);
    let id = record_present(&mut store, &path, &bytes, 1, 1);

    // Sanity: healthy before corruption.
    assert!(store.get_content(id).is_ok());
    assert!(store.check().unwrap().ok);

    // Corrupt one chunk's compressed bytes via a raw connection.
    let raw = Connection::open(db_path(dir.path())).unwrap();
    let victim: Vec<u8> = raw
        .query_row("SELECT hash FROM chunks LIMIT 1", [], |r| r.get(0))
        .unwrap();
    let garbage = vec![0xFFu8; 64]; // not a valid zstd frame
    let updated = raw
        .execute(
            "UPDATE chunks SET zdata = ?1 WHERE hash = ?2",
            params![garbage, victim],
        )
        .unwrap();
    assert_eq!(updated, 1);

    // check() reports the bad chunk (no panic, no silent pass).
    let report = store.check().unwrap();
    assert!(!report.ok, "corruption must fail the check");
    assert!(
        report.issues.iter().any(|i| i.contains("corrupt chunk")),
        "issues should name the corrupt chunk: {:?}",
        report.issues
    );

    // get_content refuses to return wrong bytes.
    let err = store
        .get_content(id)
        .expect_err("must not return bad bytes");
    assert!(matches!(
        err,
        tomo_history::HistoryError::CorruptChunk { .. }
    ));
}

#[test]
fn silently_swapped_chunk_content_is_caught_by_hash() {
    // A chunk whose zdata decompresses cleanly but to *different* bytes of the
    // same length must still be rejected — the BLAKE3 key check catches it.
    let (dir, mut store) = fresh_store();
    let path = rp("victim.bin");
    let bytes = pseudorandom(800_000, 0xBEEF);
    let id = record_present(&mut store, &path, &bytes, 1, 1);

    let raw = Connection::open(db_path(dir.path())).unwrap();
    let (victim_hash, size): (Vec<u8>, i64) = raw
        .query_row("SELECT hash, size FROM chunks LIMIT 1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    // Valid zstd of different content of the same size.
    let replacement =
        zstd::encode_all(vec![0x5Au8; usize::try_from(size).unwrap()].as_slice(), 3).unwrap();
    raw.execute(
        "UPDATE chunks SET zdata = ?1 WHERE hash = ?2",
        params![replacement, victim_hash],
    )
    .unwrap();

    let err = store
        .get_content(id)
        .expect_err("hash mismatch must be caught");
    assert!(matches!(
        err,
        tomo_history::HistoryError::CorruptChunk { .. }
    ));
    assert!(!store.check().unwrap().ok);
}

#[test]
fn get_content_of_unknown_version_errors() {
    let (_dir, store) = fresh_store();
    let err = store
        .get_content(tomo_history::VersionId(424_242))
        .expect_err("unknown version");
    assert!(matches!(err, tomo_history::HistoryError::NoSuchVersion(_)));
}
