//! Concurrent-open smoke test: two store handles on the same WAL database can
//! interleave writes and each other's reads without error.
#![allow(clippy::unwrap_used, clippy::expect_used)] // tests

mod common;

use common::{pseudorandom, record_present, rp};
use tomo_history::HistoryStore;

#[test]
fn two_handles_interleave_writes_and_reads() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = HistoryStore::open(dir.path()).unwrap();
    let mut b = HistoryStore::open(dir.path()).unwrap();

    let pa = rp("from_a.bin");
    let pb = rp("from_b.bin");

    // Interleave writes from both handles.
    let mut ids = Vec::new();
    for i in 1..=5u64 {
        let da = pseudorandom(40_000, 100 + i);
        let db = pseudorandom(40_000, 200 + i);
        ids.push((record_present(&mut a, &pa, &da, 1, i), da));
        ids.push((record_present(&mut b, &pb, &db, 2, i), db));
    }

    // Each handle sees the other's committed versions and restores them.
    for (id, bytes) in &ids {
        assert_eq!(&a.get_content(*id).unwrap(), bytes);
        assert_eq!(&b.get_content(*id).unwrap(), bytes);
    }

    // Both logs reflect all five edits to their respective paths.
    assert_eq!(a.log(&pa).unwrap().len(), 5);
    assert_eq!(b.log(&pb).unwrap().len(), 5);
    assert!(a.check().unwrap().ok);
}
