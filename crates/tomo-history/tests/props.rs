//! Property tests (docs/TESTING.md Level 1): arbitrary content round-trips
//! byte-identically, and re-storing identical content is always free.
#![allow(clippy::unwrap_used, clippy::expect_used)] // tests

mod common;

use common::{fresh_store, record_present, rp};
use proptest::prelude::*;

proptest! {
    // Vary sizes up to ~1 MiB so multi-chunk files are well represented.
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Any byte vector, stored and recorded, comes back exactly.
    #[test]
    fn arbitrary_content_round_trips(bytes in proptest::collection::vec(any::<u8>(), 0..1_000_000)) {
        let (_dir, mut store) = fresh_store();
        let id = record_present(&mut store, &rp("f.bin"), &bytes, 1, 1);
        let got = store.get_content(id).unwrap();
        prop_assert_eq!(got, bytes);
    }

    /// Storing the same content twice writes zero new chunk-bytes the second
    /// time (dedup), and both stores agree on the content hash.
    #[test]
    fn second_store_of_same_content_is_free(
        bytes in proptest::collection::vec(any::<u8>(), 0..1_000_000)
    ) {
        let (_dir, mut store) = fresh_store();
        let (h1, _new1) = store.store_content(&bytes).unwrap();
        let (h2, new2) = store.store_content(&bytes).unwrap();
        prop_assert_eq!(h1, h2);
        prop_assert_eq!(new2, 0);
    }

    /// A store built from arbitrary content always passes its own integrity
    /// check.
    #[test]
    fn healthy_store_always_checks_green(
        bytes in proptest::collection::vec(any::<u8>(), 0..500_000)
    ) {
        let (_dir, mut store) = fresh_store();
        record_present(&mut store, &rp("f.bin"), &bytes, 1, 1);
        let report = store.check().unwrap();
        prop_assert!(report.ok, "issues: {:?}", report.issues);
    }
}
