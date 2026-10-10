//! Resource limits on decoding a frame, measured with a counting allocator.
//!
//! This file is its own test binary on purpose: it installs a global
//! allocator that remembers the largest single request, and that instrument
//! must not be shared with tests that run unrelated work in parallel.
//!
//! The attack it pins down is the zero-sized list. A Cap'n Proto list of
//! structs with no data and no pointers occupies no words on the wire, so its
//! element count is not bounded by the segment. Cap'n Proto charges such a
//! list against the traversal limit instead, and the default limit is 8 Mi
//! words — which admits eight million elements from a frame of a few dozen
//! bytes. A decoder that sized its output with `Vec::with_capacity(list.len())`
//! then asked the allocator for eight million `PeerAddr`s, a few hundred MB,
//! before reading a single element. And it did so before the handshake: any
//! decodable frame is accepted as the first one.

use cb_net::p2p::codec;
use cb_net::p2p::identity::NodeId;
use cb_net::p2p::message::{Message, PeerAddr};
use cb_types::Address;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

struct Counting;

static LARGEST: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LARGEST.fetch_max(layout.size(), Ordering::Relaxed);
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LARGEST.fetch_max(new_size, Ordering::Relaxed);
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// The allocator counter is process-wide; tests that read it take turns.
static SERIAL: Mutex<()> = Mutex::new(());

/// The largest single allocation made while `f` ran.
fn largest_allocation_during<T>(f: impl FnOnce() -> T) -> (T, usize) {
    LARGEST.store(0, Ordering::Relaxed);
    let out = f();
    (out, LARGEST.load(Ordering::Relaxed))
}

/// A `peers` frame whose list claims `count` zero-sized elements.
///
/// Built by encoding a genuine one-element list and then rewriting two words:
/// the list pointer's word count becomes zero, and the list's tag claims
/// `count` elements of no data and no pointers. Everything else is exactly
/// what the encoder produced, so the frame is well formed in every respect
/// except the one under test.
fn zero_sized_peers_frame(count: u32) -> Vec<u8> {
    let one = Message::Peers(vec![PeerAddr {
        node_id: NodeId(Address([9; 20])),
        addr: "127.0.0.1:1".parse().expect("addr"),
    }]);
    let mut frame = codec::encode(&one).expect("encode");
    // Skip the single-segment table: one word holding (count - 1, length).
    let base = 8;
    let words = (frame.len() - base) / 8;
    let word = |f: &[u8], i: usize| {
        u64::from_le_bytes(
            f[base + i * 8..base + i * 8 + 8]
                .try_into()
                .expect("8 bytes"),
        )
    };
    let (ptr_at, tag_at) = (0..words)
        .find_map(|i| {
            let w = word(&frame, i);
            // A list pointer (kind 1) with inline-composite elements (size 7).
            if w & 3 == 1 && (w >> 32) & 7 == 7 {
                let offset = ((w as u32 as i32) >> 2) as isize;
                Some((i, (i as isize + 1 + offset) as usize))
            } else {
                None
            }
        })
        .expect("the encoder wrote an inline-composite list");
    let list_ptr = word(&frame, ptr_at) & ((1 << 35) - 1); // word count := 0
    let tag = u64::from(count) << 2; // struct tag: count elements, 0 data, 0 ptrs
    frame[base + ptr_at * 8..base + ptr_at * 8 + 8].copy_from_slice(&list_ptr.to_le_bytes());
    frame[base + tag_at * 8..base + tag_at * 8 + 8].copy_from_slice(&tag.to_le_bytes());
    frame
}

#[test]
fn a_tiny_frame_cannot_make_the_decoder_allocate_for_millions_of_elements() {
    let _turn = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let frame = zero_sized_peers_frame(8_000_000);
    assert!(
        frame.len() < 256,
        "the attack frame is tiny: {} bytes",
        frame.len()
    );
    let (result, largest) = largest_allocation_during(|| codec::decode(&frame));
    assert!(result.is_err(), "the frame must not decode to a message");
    assert!(
        largest < 1024 * 1024,
        "decoding a {}-byte frame made a single {largest}-byte allocation",
        frame.len()
    );
}

#[test]
fn a_zero_sized_list_small_enough_to_be_harmless_is_still_refused_cleanly() {
    // The same shape at a count that fits any limit: it must fail on its
    // first element (an empty node id), not panic or allocate for the count.
    let _turn = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let frame = zero_sized_peers_frame(10);
    let (result, largest) = largest_allocation_during(|| codec::decode(&frame));
    assert!(result.is_err());
    assert!(
        largest < 64 * 1024,
        "largest allocation was {largest} bytes"
    );
}

#[test]
fn a_legitimate_peers_frame_still_decodes() {
    let _turn = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let list: Vec<PeerAddr> = (0..64u8)
        .map(|i| PeerAddr {
            node_id: NodeId(Address([i; 20])),
            addr: format!("10.0.0.{i}:7700").parse().expect("addr"),
        })
        .collect();
    let msg = Message::Peers(list);
    let frame = codec::encode(&msg).expect("encode");
    assert_eq!(codec::decode(&frame).expect("decode"), msg);
}

// ------------------------------------------------------------------ fuzzing
//
// Every decoder a peer's bytes reach before any trust is established:
// `codec::decode` for every frame, the handshake's signature check for any
// `Hello` that decodes, and the two body decoders the verifier runs on
// gossiped blocks and transactions. Each is fed arbitrary bytes and mutated
// valid encodings. The property is the same for all of them: an error, never
// a panic, and no single allocation larger than a small multiple of the
// input — the class of bug the zero-sized list above belonged to.

use cb_net::p2p::identity::GenesisId;
use cb_net::p2p::message::{DisconnectReason, Hello};
use cb_types::H256;
use proptest::prelude::*;

/// The most a decoder may ask for in one allocation for `input_len` bytes.
fn allocation_budget(input_len: usize) -> usize {
    64 * 1024 + 16 * input_len
}

/// One valid encoding of every message variant.
fn valid_frames() -> Vec<Vec<u8>> {
    let (sk, _) = cb_crypto::keypair_from_seed(b"fuzz");
    let hello = Hello::signed(
        &sk,
        cb_net::p2p::PROTOCOL_VERSION,
        1,
        GenesisId(H256([3; 32])),
        "cb-node/fuzz".into(),
        9,
        Some("10.0.0.1:7701".parse().expect("addr")),
        7,
    )
    .expect("sign");
    let msgs = vec![
        Message::Hello(hello),
        Message::Disconnect(DisconnectReason::Banned),
        Message::GetPeers,
        Message::Peers(
            (0..3u8)
                .map(|i| PeerAddr {
                    node_id: NodeId(Address([i; 20])),
                    addr: format!("10.0.0.{i}:7700").parse().expect("addr"),
                })
                .collect(),
        ),
        Message::BlockAnnounce {
            hash: H256([1; 32]),
            height: 7,
        },
        Message::GetBlock {
            hash: H256([2; 32]),
        },
        Message::BlockBody {
            height: 7,
            bytes: vec![1; 50],
        },
        Message::TxAnnounce {
            hash: H256([4; 32]),
        },
        Message::GetTx {
            hash: H256([4; 32]),
        },
        Message::TxBody { bytes: vec![9; 40] },
        Message::Consensus { bytes: vec![7; 60] },
        Message::GetBlockRange { from: 2, to: 9 },
        Message::BlockRange {
            blocks: vec![(2, vec![0; 8]), (3, vec![1; 8]), (4, vec![])],
        },
        Message::Ping { nonce: 5 },
        Message::Pong { nonce: 5 },
    ];
    msgs.iter()
        .map(|m| codec::encode(m).expect("encode"))
        .collect()
}

/// How a valid frame is damaged.
#[derive(Clone, Debug)]
enum Mutation {
    /// XOR one byte.
    Flip { at: usize, mask: u8 },
    /// Overwrite an aligned word with a value chosen to look like a length,
    /// a count or a pointer at an extreme.
    Word { at: usize, value: u64 },
    /// Cut the frame short.
    Truncate { to: usize },
    /// Append junk.
    Extend { junk: Vec<u8> },
}

fn mutation() -> impl Strategy<Value = Mutation> {
    let extreme = prop_oneof![
        Just(0u64),
        Just(u64::MAX),
        Just(u64::from(u32::MAX)),
        Just(0x7fff_fffc_u64),
        Just(0xffff_fff9_u64 | (0x1fff_ffff_u64 << 35)),
        Just(1 | (7 << 32) | (0x1fff_ffff_u64 << 35)),
        Just((8_000_000u64) << 2),
        any::<u64>(),
    ];
    prop_oneof![
        (any::<usize>(), 1u8..=255).prop_map(|(at, mask)| Mutation::Flip { at, mask }),
        (any::<usize>(), extreme).prop_map(|(at, value)| Mutation::Word { at, value }),
        any::<usize>().prop_map(|to| Mutation::Truncate { to }),
        proptest::collection::vec(any::<u8>(), 1..64).prop_map(|junk| Mutation::Extend { junk }),
    ]
}

fn apply(frame: &mut Vec<u8>, m: &Mutation) {
    if frame.is_empty() {
        return;
    }
    match m {
        Mutation::Flip { at, mask } => {
            let i = at % frame.len();
            frame[i] ^= mask;
        }
        Mutation::Word { at, value } => {
            let words = frame.len() / 8;
            if words > 0 {
                let i = (at % words) * 8;
                frame[i..i + 8].copy_from_slice(&value.to_le_bytes());
            }
        }
        Mutation::Truncate { to } => frame.truncate(to % frame.len()),
        Mutation::Extend { junk } => frame.extend_from_slice(junk),
    }
}

/// Decode `bytes` under the allocation meter and hold the result to the
/// fuzzing property. A frame that still decodes must re-encode, and decode
/// again to the same message.
fn check_frame(bytes: &[u8]) -> Result<(), TestCaseError> {
    let (result, largest) = largest_allocation_during(|| codec::decode(bytes));
    prop_assert!(
        largest <= allocation_budget(bytes.len()),
        "{} input bytes caused a {largest}-byte allocation",
        bytes.len()
    );
    if let Ok(msg) = result {
        if let Message::Hello(h) = &msg {
            // Whatever the signature bytes are, checking them must not panic.
            let _ = h.verify_identity();
        }
        let again = codec::encode(&msg).expect("a decoded message re-encodes");
        prop_assert_eq!(codec::decode(&again).expect("and decodes"), msg);
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2_000, ..ProptestConfig::default() })]

    #[test]
    fn arbitrary_bytes_never_panic_or_over_allocate(
        bytes in proptest::collection::vec(any::<u8>(), 0..2_048),
    ) {
        let _turn = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        check_frame(&bytes)?;
    }

    #[test]
    fn arbitrary_words_behind_a_valid_segment_table_never_panic_or_over_allocate(
        words in proptest::collection::vec(any::<u64>(), 1..64),
    ) {
        // Random bytes almost never get past the segment table. A correct
        // table in front of random words sends every case into the pointer
        // and list decoding, which is where the interesting bugs live.
        let _turn = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let mut frame = Vec::with_capacity(8 + words.len() * 8);
        frame.extend_from_slice(&0u32.to_le_bytes());
        frame.extend_from_slice(&(words.len() as u32).to_le_bytes());
        for w in &words {
            frame.extend_from_slice(&w.to_le_bytes());
        }
        check_frame(&frame)?;
    }

    #[test]
    fn mutated_valid_frames_never_panic_or_over_allocate(
        which in any::<usize>(),
        mutations in proptest::collection::vec(mutation(), 1..4),
    ) {
        let _turn = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let frames = valid_frames();
        let mut frame = frames[which % frames.len()].clone();
        for m in &mutations {
            apply(&mut frame, m);
        }
        check_frame(&frame)?;
    }

    #[test]
    fn gossiped_block_and_transaction_bodies_never_panic_or_over_allocate(
        bytes in proptest::collection::vec(any::<u8>(), 0..1_024),
        length_prefix in prop_oneof![Just(u64::MAX), Just(1u64 << 40), Just(1u64 << 20), any::<u64>()],
        at in 0usize..64,
    ) {
        // The verifier's first step on a gossiped body: `Block::decode` for a
        // block, bincode `SignedTx` for a transaction. Bincode reads a u64
        // length before every sequence, so some cases plant an extreme one.
        let _turn = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let mut planted = bytes.clone();
        if planted.len() >= at + 8 {
            planted[at..at + 8].copy_from_slice(&length_prefix.to_le_bytes());
        }
        for input in [&bytes, &planted] {
            let (_, largest) = largest_allocation_during(|| cb_block::Block::decode(input));
            prop_assert!(
                largest <= allocation_budget(input.len()) + 1024 * 1024,
                "Block::decode of {} bytes allocated {largest}", input.len()
            );
            let (_, largest) = largest_allocation_during(|| {
                bincode::deserialize::<cb_tx::SignedTx>(input)
            });
            prop_assert!(
                largest <= allocation_budget(input.len()) + 1024 * 1024,
                "SignedTx decode of {} bytes allocated {largest}", input.len()
            );
        }
    }
}
