//! Round-trip fidelity: content stored and recorded comes back byte-identical,
//! across empty, tiny, chunk-boundary, and multi-megabyte inputs.
#![allow(clippy::unwrap_used, clippy::expect_used)] // tests

mod common;

use common::{fresh_store, pseudorandom, record_present, rp, sig_of};

/// `FastCDC` boundaries configured in the store (16 KiB / 64 KiB / 256 KiB).
const MIN: usize = 16 * 1024;
const AVG: usize = 64 * 1024;
const MAX: usize = 256 * 1024;

fn round_trips(bytes: &[u8]) {
    let (_dir, mut store) = fresh_store();
    let path = rp("file.bin");
    let id = record_present(&mut store, &path, bytes, 1, 1);
    let got = store.get_content(id).expect("get_content");
    assert_eq!(got.len(), bytes.len(), "length mismatch");
    assert_eq!(got, bytes, "content mismatch");
}

#[test]
fn empty_file_round_trips() {
    round_trips(&[]);
}

#[test]
fn one_byte_round_trips() {
    round_trips(&[0x42]);
}

#[test]
fn boundary_sizes_round_trip() {
    for &size in &[MIN - 1, MIN, MIN + 1, AVG, MAX - 1, MAX, MAX + 1, 3 * MAX] {
        let bytes = pseudorandom(size, 0x51 ^ size as u64);
        round_trips(&bytes);
    }
}

#[test]
fn five_mib_random_round_trips() {
    let bytes = pseudorandom(5 * 1024 * 1024, 0xF00D);
    round_trips(&bytes);
}

#[test]
fn store_content_reports_content_hash() {
    let (_dir, mut store) = fresh_store();
    let bytes = pseudorandom(200_000, 7);
    let (hash, new_bytes) = store.store_content(&bytes).expect("store_content");
    assert_eq!(
        hash,
        sig_of(&bytes).hash,
        "reported hash is the whole-file BLAKE3"
    );
    assert!(new_bytes > 0, "a fresh multi-chunk file writes new bytes");
}

#[test]
fn get_content_verifies_whole_file_hash_ok() {
    // A healthy round-trip passes the internal whole-file hash verification.
    let (_dir, mut store) = fresh_store();
    let bytes = pseudorandom(AVG * 3, 99);
    let id = record_present(&mut store, &rp("v.bin"), &bytes, 1, 1);
    assert_eq!(store.get_content(id).unwrap(), bytes);
}
