//! Canonical encoding, over generated transactions and arbitrary bytes.
//!
//! A transaction's hash is keccak over its canonical Cap'n Proto bytes, so the
//! encoding has to be a function of the message's *content* and nothing else:
//! not of the order fields were set in, not of how the builder happened to
//! allocate segments. And the reader sits on the network, so garbage must
//! come back as an error.
//!
//! What the group proves:
//! - two builds of the same content produce identical canonical bytes and
//!   hash, whatever order the fields were written in;
//! - canonical bytes read back to the same field values and are reported
//!   canonical by the library itself;
//! - re-canonicalising canonical bytes is the identity (the encoding is a
//!   fixed point, so a relayer cannot re-encode a transaction to a new hash);
//! - different content gives different bytes;
//! - arbitrary bytes handed to `read_message` and then walked as a
//!   transaction never panic.

use capnp::message::{Builder, HeapAllocator};
use cb_schema::canonical::{canonical_bytes, canonical_hash, read_message};
use cb_schema::{tx_capnp, types_capnp};
use proptest::prelude::*;

#[derive(Debug, Clone, PartialEq)]
struct Transfer {
    nonce: u64,
    chain_id: u32,
    to: [u8; 20],
    asset: u8,
    hi: u64,
    lo: u64,
}

fn transfer() -> impl Strategy<Value = Transfer> {
    (
        any::<u64>(),
        any::<u32>(),
        any::<[u8; 20]>(),
        0u8..8,
        any::<u64>(),
        any::<u64>(),
    )
        .prop_map(|(nonce, chain_id, to, asset, hi, lo)| Transfer {
            nonce,
            chain_id,
            to,
            asset,
            hi,
            lo,
        })
}

fn asset(i: u8) -> types_capnp::AssetId {
    use types_capnp::AssetId::*;
    [Btc, Eth, Usd, Sol, Aapl, Tsla, Nvda, Spy][i as usize]
}

/// Build the transfer, writing the header fields before or after the body
/// depending on `header_first`, so two builds differ in allocation order.
fn build(t: &Transfer, header_first: bool) -> Builder<HeapAllocator> {
    let mut b = Builder::new_default();
    {
        let mut tx = b.init_root::<tx_capnp::transaction::Builder<'_>>();
        if header_first {
            tx.set_nonce(t.nonce);
            tx.set_chain_id(t.chain_id);
        }
        {
            let body = tx.reborrow().init_body();
            let mut transfer = body.init_transfer();
            transfer.reborrow().set_asset(asset(t.asset));
            {
                let mut amt = transfer.reborrow().init_amount();
                amt.set_lo(t.lo);
                amt.set_hi(t.hi);
            }
            transfer.init_to().set_bytes(&t.to);
        }
        if !header_first {
            tx.set_chain_id(t.chain_id);
            tx.set_nonce(t.nonce);
        }
    }
    b
}

fn read_back(bytes: &[u8]) -> Transfer {
    let msg = read_message(bytes).unwrap();
    let r: tx_capnp::transaction::Reader = msg.get_root().unwrap();
    let body = match r.get_body().which().unwrap() {
        tx_capnp::transaction::body::Which::Transfer(t) => t.unwrap(),
        _ => panic!("not a transfer"),
    };
    let to: [u8; 20] = body
        .get_to()
        .unwrap()
        .get_bytes()
        .unwrap()
        .try_into()
        .unwrap();
    let amount = body.get_amount().unwrap();
    Transfer {
        nonce: r.get_nonce(),
        chain_id: r.get_chain_id(),
        to,
        asset: body.get_asset().unwrap() as u8,
        hi: amount.get_hi(),
        lo: amount.get_lo(),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn the_encoding_depends_on_content_not_on_build_order(t in transfer()) {
        let a = canonical_bytes(&build(&t, true)).unwrap();
        let b = canonical_bytes(&build(&t, false)).unwrap();
        prop_assert_eq!(&a, &b);
        prop_assert_eq!(canonical_hash(&build(&t, true)).unwrap(), canonical_hash(&build(&t, false)).unwrap());
    }

    #[test]
    fn canonical_bytes_read_back_and_are_a_fixed_point(t in transfer()) {
        let bytes = canonical_bytes(&build(&t, true)).unwrap();
        prop_assert_eq!(read_back(&bytes), t);
        let msg = read_message(&bytes).unwrap();
        prop_assert!(msg.is_canonical().unwrap());
        // Re-encode the canonical message: same bytes.
        let mut again = Builder::new_default();
        again.set_root_canonical(msg.get_root::<tx_capnp::transaction::Reader>().unwrap()).unwrap();
        prop_assert_eq!(canonical_bytes(&again).unwrap(), bytes);
    }

    #[test]
    fn different_content_gives_different_bytes(a in transfer(), b in transfer()) {
        prop_assume!(a != b);
        prop_assert_ne!(
            canonical_bytes(&build(&a, true)).unwrap(),
            canonical_bytes(&build(&b, true)).unwrap()
        );
    }

    /// Garbage off the wire: reading, then walking the root as a transaction
    /// the way the node would, returns errors and never panics.
    #[test]
    fn arbitrary_bytes_never_panic_the_reader(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        if let Ok(msg) = read_message(&bytes) {
            if let Ok(r) = msg.get_root::<tx_capnp::transaction::Reader>() {
                let _ = r.get_nonce();
                let _ = r.get_chain_id();
                if let Ok(tx_capnp::transaction::body::Which::Transfer(Ok(t))) = r.get_body().which() {
                    let _ = t.get_to().and_then(|to| to.get_bytes());
                    let _ = t.get_asset();
                    let _ = t.get_amount().map(|a| (a.get_hi(), a.get_lo()));
                }
            }
        }
    }

    /// Structured garbage: a valid single-segment frame header in front of
    /// random words, which gets past the framing and exercises pointer
    /// decoding.
    #[test]
    fn framed_random_words_never_panic_the_reader(words in proptest::collection::vec(any::<u64>(), 1..64)) {
        let mut bytes = Vec::with_capacity(8 + words.len() * 8);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&(words.len() as u32).to_le_bytes());
        for w in &words {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        if let Ok(msg) = read_message(&bytes) {
            if let Ok(r) = msg.get_root::<tx_capnp::transaction::Reader>() {
                let _ = r.get_nonce();
                if let Ok(tx_capnp::transaction::body::Which::Transfer(Ok(t))) = r.get_body().which() {
                    let _ = t.get_to().and_then(|to| to.get_bytes());
                }
            }
            let _ = msg.is_canonical();
        }
    }
}
