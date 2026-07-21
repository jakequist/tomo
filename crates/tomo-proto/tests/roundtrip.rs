//! Framing round-trip and stream-reassembly tests for `tomo-proto`.
//!
//! These exercise the public API only (`encode` + `FrameDecoder`), which is the
//! exact surface `tomo-transport` will use.

// Test code may panic on Results/Options; the library code paths may not.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;

use tomo_engine::{
    ChangeKind, ContentHash, ContentSig, Entry, EntryState, Index, RelPath, RemoteChange,
    ReplicaId, VectorClock,
};
use tomo_proto::{frame, Message, ProtoError, MAX_FRAME_LEN, PROTOCOL_VERSION};

// ---- Fixtures -------------------------------------------------------------

/// A clock ticked `count` times for each `(replica, count)` pair.
fn clock(spec: &[(u64, u64)]) -> VectorClock {
    let mut v = VectorClock::new();
    for &(r, count) in spec {
        for _ in 0..count {
            v.tick(ReplicaId(r));
        }
    }
    v
}

fn sig(byte: u8, size: u64) -> ContentSig {
    ContentSig {
        hash: ContentHash([byte; 32]),
        size,
        exec: false,
        mtime_ms: u64::from(byte) * 1000,
    }
}

/// A realistic index: present files, a tombstone, and multi-replica clocks.
fn realistic_index() -> Index {
    let mut idx = Index::new();
    idx.upsert(
        RelPath::new("src/main.rs").unwrap(),
        Entry::single(
            clock(&[(1, 3), (2, 1)]),
            EntryState::Present(sig(0xaa, 4096)),
        ),
    );
    idx.upsert(
        RelPath::new("README.md").unwrap(),
        Entry::single(clock(&[(1, 1)]), EntryState::Present(sig(0x11, 128))),
    );
    idx.upsert(
        RelPath::new("docs/old.txt").unwrap(),
        Entry::single(clock(&[(2, 5)]), EntryState::Tombstone),
    );
    idx
}

/// One instance of every `Message` variant, using realistic payloads.
fn sample_messages() -> Vec<Message> {
    vec![
        Message::Hello {
            protocol: PROTOCOL_VERSION,
            binary_version: env!("CARGO_PKG_VERSION").to_owned(),
            replica: ReplicaId(42),
        },
        Message::IndexExchange(realistic_index()),
        Message::Change {
            change: RemoteChange {
                path: RelPath::new("src/lib.rs").unwrap(),
                kind: ChangeKind::Modified(sig(0x7f, 5)),
                version: clock(&[(1, 4), (2, 2)]),
            },
            bytes: Some(b"hello".to_vec()),
        },
        Message::Change {
            change: RemoteChange {
                path: RelPath::new("gone.txt").unwrap(),
                kind: ChangeKind::Removed,
                version: clock(&[(2, 9)]),
            },
            bytes: None,
        },
        Message::Ping { nonce: 0xdead_beef },
        Message::Pong { nonce: 0xdead_beef },
    ]
}

// ---- Example tests --------------------------------------------------------

#[test]
fn every_variant_round_trips_single_push() {
    for msg in sample_messages() {
        let bytes = frame::encode(&msg).unwrap();
        let mut dec = frame::FrameDecoder::new();
        dec.push(&bytes);
        assert_eq!(dec.next().unwrap(), Some(msg.clone()), "round-trip {msg:?}");
        // Nothing left over.
        assert_eq!(dec.next().unwrap(), None);
    }
}

#[test]
fn hello_round_trips_realistic_index() {
    // Hello carries no index, but IndexExchange does; verify a realistic index
    // (present entries, a tombstone, multi-replica clocks) survives the frame.
    let idx = realistic_index();
    let msg = Message::IndexExchange(idx.clone());
    let bytes = frame::encode(&msg).unwrap();
    let mut dec = frame::FrameDecoder::new();
    dec.push(&bytes);
    match dec.next().unwrap() {
        Some(Message::IndexExchange(got)) => assert_eq!(got, idx),
        other => panic!("expected IndexExchange, got {other:?}"),
    }
}

#[test]
fn many_messages_one_stream() {
    let msgs = sample_messages();
    let mut stream = Vec::new();
    for m in &msgs {
        stream.extend_from_slice(&frame::encode(m).unwrap());
    }
    let mut dec = frame::FrameDecoder::new();
    dec.push(&stream);
    let mut got = Vec::new();
    while let Some(m) = dec.next().unwrap() {
        got.push(m);
    }
    assert_eq!(got, msgs);
}

#[test]
fn incomplete_frame_yields_none_until_complete() {
    let msg = Message::IndexExchange(realistic_index());
    let bytes = frame::encode(&msg).unwrap();
    let mut dec = frame::FrameDecoder::new();

    // Feed everything but the last byte: never completes.
    let (head, tail) = bytes.split_at(bytes.len() - 1);
    dec.push(head);
    assert_eq!(dec.next().unwrap(), None);
    assert_eq!(dec.next().unwrap(), None); // repeatable

    // The final byte completes exactly one message.
    dec.push(tail);
    assert_eq!(dec.next().unwrap(), Some(msg));
    assert_eq!(dec.next().unwrap(), None);
}

#[test]
fn only_length_prefix_is_incomplete() {
    let msg = Message::Ping { nonce: 1 };
    let bytes = frame::encode(&msg).unwrap();
    let mut dec = frame::FrameDecoder::new();
    // Fewer than 4 bytes: cannot even read the length prefix.
    dec.push(&bytes[..2]);
    assert_eq!(dec.next().unwrap(), None);
    dec.push(&bytes[2..]);
    assert_eq!(dec.next().unwrap(), Some(msg));
}

#[test]
fn oversize_declared_length_is_fatal() {
    // Craft a frame whose declared length exceeds MAX_FRAME_LEN.
    let mut bytes = (MAX_FRAME_LEN + 1).to_le_bytes().to_vec();
    bytes.extend_from_slice(&[0u8; 8]); // some payload bytes; never inspected
    let mut dec = frame::FrameDecoder::new();
    dec.push(&bytes);
    match dec.next() {
        Err(ProtoError::FrameTooLarge { len }) => assert_eq!(len, MAX_FRAME_LEN + 1),
        other => panic!("expected FrameTooLarge, got {other:?}"),
    }
    // Terminal + stable: re-calling reports the same error (no silent skip).
    assert!(matches!(
        dec.next(),
        Err(ProtoError::FrameTooLarge { len }) if len == MAX_FRAME_LEN + 1
    ));
}

#[test]
fn corrupt_payload_is_decode_error() {
    let msg = Message::Ping { nonce: 5 };
    let mut bytes = frame::encode(&msg).unwrap();
    // Corrupt the enum discriminant byte (first payload byte) to an invalid
    // variant index so postcard rejects it.
    let payload_start = 4;
    bytes[payload_start] = 0xff;
    let mut dec = frame::FrameDecoder::new();
    dec.push(&bytes);
    match dec.next() {
        Err(ProtoError::Decode(_)) => {}
        other => panic!("expected Decode error, got {other:?}"),
    }
    // Stable: the decoder did not advance past the bad frame.
    assert!(matches!(dec.next(), Err(ProtoError::Decode(_))));
}

// ---- Property strategies --------------------------------------------------

fn arb_relpath() -> impl Strategy<Value = RelPath> {
    proptest::collection::vec("[a-z][a-z0-9]{0,4}", 1..4)
        .prop_map(|parts| RelPath::new(&parts.join("/")).expect("generated path is valid"))
}

fn arb_clock() -> impl Strategy<Value = VectorClock> {
    proptest::collection::btree_map(0u64..4, 0u64..6, 0..4).prop_map(|m| {
        let mut c = VectorClock::new();
        for (r, count) in m {
            for _ in 0..count {
                c.tick(ReplicaId(r));
            }
        }
        c
    })
}

fn arb_sig() -> impl Strategy<Value = ContentSig> {
    (any::<u8>(), 0u64..1_000_000).prop_map(|(b, size)| sig(b, size))
}

fn arb_entry() -> impl Strategy<Value = Entry> {
    let state = prop_oneof![
        arb_sig().prop_map(EntryState::Present),
        Just(EntryState::Tombstone),
    ];
    (arb_clock(), state).prop_map(|(version, state)| Entry::single(version, state))
}

fn arb_index() -> impl Strategy<Value = Index> {
    proptest::collection::btree_map("[a-z]{1,4}", arb_entry(), 0..4).prop_map(|m| {
        let mut idx = Index::new();
        for (name, e) in m {
            idx.upsert(RelPath::new(&name).expect("valid"), e);
        }
        idx
    })
}

fn arb_message() -> impl Strategy<Value = Message> {
    let hello = (any::<u16>(), ".*", any::<u64>()).prop_map(|(protocol, binary_version, r)| {
        Message::Hello {
            protocol,
            binary_version,
            replica: ReplicaId(r),
        }
    });
    let index = arb_index().prop_map(Message::IndexExchange);
    // Realistic pairing: Modified carries bytes, Removed carries none.
    let change = (arb_relpath(), arb_clock()).prop_flat_map(|(path, version)| {
        prop_oneof![
            (arb_sig(), proptest::collection::vec(any::<u8>(), 0..64)).prop_map({
                let path = path.clone();
                let version = version.clone();
                move |(s, bytes)| Message::Change {
                    change: RemoteChange {
                        path: path.clone(),
                        kind: ChangeKind::Modified(s),
                        version: version.clone(),
                    },
                    bytes: Some(bytes),
                }
            }),
            Just(Message::Change {
                change: RemoteChange {
                    path,
                    kind: ChangeKind::Removed,
                    version,
                },
                bytes: None,
            }),
        ]
    });
    let probe = any::<u64>().prop_map(|nonce| Message::Ping { nonce });
    let reply = any::<u64>().prop_map(|nonce| Message::Pong { nonce });
    prop_oneof![hello, index, change, probe, reply]
}

// ---- Property tests -------------------------------------------------------

proptest! {
    /// Any single message survives encode → decode unchanged.
    #[test]
    fn single_message_round_trips(msg in arb_message()) {
        let bytes = frame::encode(&msg).unwrap();
        let mut dec = frame::FrameDecoder::new();
        dec.push(&bytes);
        prop_assert_eq!(dec.next().unwrap(), Some(msg));
        prop_assert_eq!(dec.next().unwrap(), None);
    }

    /// Any sequence of messages, encoded and concatenated, then fed to the
    /// decoder in ARBITRARY chunk sizes (including 1-byte feeds), yields
    /// exactly the original sequence in order.
    #[test]
    fn arbitrary_chunking_preserves_sequence(
        msgs in proptest::collection::vec(arb_message(), 0..12),
        chunk_sizes in proptest::collection::vec(1usize..=17, 1..40),
    ) {
        let mut stream = Vec::new();
        for m in &msgs {
            stream.extend_from_slice(&frame::encode(m).unwrap());
        }

        let mut dec = frame::FrameDecoder::new();
        let mut got = Vec::new();
        let mut offset = 0;
        let mut cut = 0;
        while offset < stream.len() {
            let size = chunk_sizes[cut % chunk_sizes.len()];
            cut += 1;
            let end = (offset + size).min(stream.len());
            dec.push(&stream[offset..end]);
            offset = end;
            while let Some(m) = dec.next().unwrap() {
                got.push(m);
            }
        }
        // Drain any final buffered message (defensive; loop above already does).
        while let Some(m) = dec.next().unwrap() {
            got.push(m);
        }

        prop_assert_eq!(got, msgs);
    }

    /// One-byte-at-a-time feeding is the hardest split and must still work.
    #[test]
    fn single_byte_feeds_preserve_sequence(msgs in proptest::collection::vec(arb_message(), 0..8)) {
        let mut stream = Vec::new();
        for m in &msgs {
            stream.extend_from_slice(&frame::encode(m).unwrap());
        }
        let mut dec = frame::FrameDecoder::new();
        let mut got = Vec::new();
        for b in &stream {
            dec.push(std::slice::from_ref(b));
            while let Some(m) = dec.next().unwrap() {
                got.push(m);
            }
        }
        prop_assert_eq!(got, msgs);
    }
}
