//! Content-defined dedup: identical content is free the second time, and a
//! one-byte edit to a large file re-stores only the chunks that changed.
#![allow(clippy::unwrap_used, clippy::expect_used)] // tests

mod common;

use common::{fresh_store, pseudorandom};

/// The store's maximum chunk size; "< 3 chunks' worth" is measured against it.
const MAX_CHUNK: u64 = 256 * 1024;

#[test]
fn storing_identical_content_twice_is_free() {
    let (_dir, mut store) = fresh_store();
    let bytes = pseudorandom(1024 * 1024, 0xABCD);

    let (h1, new1) = store.store_content(&bytes).expect("first store");
    assert!(new1 > 0, "first store writes the whole file");

    let (h2, new2) = store.store_content(&bytes).expect("second store");
    assert_eq!(h1, h2, "same content, same hash");
    assert_eq!(
        new2, 0,
        "re-storing identical content writes zero new bytes"
    );
}

#[test]
fn one_byte_flip_restores_only_local_chunks() {
    let (_dir, mut store) = fresh_store();
    let mut bytes = pseudorandom(10 * 1024 * 1024, 0x1234_5678);

    let (_h1, new1) = store.store_content(&bytes).expect("first store");
    assert!(
        new1 >= u64::try_from(bytes.len() / 2).unwrap(),
        "first store writes most of the file"
    );

    // Flip one byte in the middle and re-store.
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    let (_h2, new2) = store.store_content(&bytes).expect("second store");

    // Content-defined chunking localizes the edit: at most a couple of chunks
    // change (the edited chunk, plus possibly a re-cut neighbour), never the
    // whole 10 MiB.
    assert!(
        new2 < 3 * MAX_CHUNK,
        "one-byte edit re-stored {new2} new bytes, expected < {} (3 max chunks)",
        3 * MAX_CHUNK
    );
    assert!(new2 > 0, "the edited chunk is genuinely new content");
}
