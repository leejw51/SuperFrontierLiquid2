//! The request decoders under random and mutated input.
//!
//! What this proves: `signed_tx_from_capnp` and `vote_from_capnp` — the two
//! decoders that read client-supplied structs — never panic on any byte
//! string a peer can send, including valid messages with random bytes
//! flipped, truncated or extended; a message that does decode re-encodes to
//! a value that decodes to itself (no decoder accepts something it cannot
//! reproduce); and every well-formed transaction whose text fields are
//! within the cap survives the capnp round trip unchanged, while one past
//! the cap never decodes.

use capnp::message::{Builder, ReaderOptions};
use cb_rpc::conv::{self, MAX_EXTERNAL_REF_LEN, MAX_VOTE_SIG_LEN};
use cb_schema::{rpc_capnp, tx_capnp};
use cb_tx::{SignedTx, Transaction, TxBody};
use cb_types::{Address, AssetId, DerivId, MarketId, Side, Tif, H256};
use proptest::prelude::*;

fn opts() -> ReaderOptions {
    let mut o = ReaderOptions::new();
    o.traversal_limit_in_words(Some(1 << 16));
    o
}

fn encode_tx(s: &SignedTx) -> Vec<u8> {
    let mut m = Builder::new_default();
    conv::signed_tx_to_capnp(m.init_root::<tx_capnp::signed_tx::Builder>(), s);
    capnp::serialize::write_message_to_words(&m)
}

fn decode_tx(bytes: &[u8]) -> Option<capnp::Result<SignedTx>> {
    let mut slice = bytes;
    let msg = capnp::serialize::read_message_from_flat_slice(&mut slice, opts()).ok()?;
    let root = msg.get_root::<tx_capnp::signed_tx::Reader>().ok()?;
    Some(conv::signed_tx_from_capnp(root))
}

fn encode_vote(v: &cb_consensus::Vote) -> Vec<u8> {
    let mut m = Builder::new_default();
    conv::vote_to_capnp(m.init_root::<rpc_capnp::vote::Builder>(), v);
    capnp::serialize::write_message_to_words(&m)
}

fn decode_vote(bytes: &[u8]) -> Option<capnp::Result<cb_consensus::Vote>> {
    let mut slice = bytes;
    let msg = capnp::serialize::read_message_from_flat_slice(&mut slice, opts()).ok()?;
    let root = msg.get_root::<rpc_capnp::vote::Reader>().ok()?;
    Some(conv::vote_from_capnp(root))
}

fn text(max: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(any::<char>(), 0..max).prop_map(|c| c.into_iter().collect())
}

fn body() -> impl Strategy<Value = TxBody> {
    let addr = any::<[u8; 20]>().prop_map(Address);
    let h = any::<[u8; 32]>().prop_map(H256);
    let asset = prop::sample::select(AssetId::ALL.to_vec());
    let market = prop::sample::select(MarketId::ALL.to_vec());
    let side = prop::sample::select(vec![Side::Buy, Side::Sell]);
    let tif = prop::sample::select(vec![Tif::Gtc, Tif::Ioc, Tif::Fok]);
    let deriv = (any::<bool>(), any::<u16>()).prop_map(|(p, i)| {
        if p {
            DerivId::perp(i)
        } else {
            DerivId::future(i)
        }
    });
    prop_oneof![
        (addr.clone(), asset.clone(), any::<u128>())
            .prop_map(|(to, asset, amount)| TxBody::Transfer { to, asset, amount }),
        (addr.clone(), asset.clone(), any::<u128>(), text(64)).prop_map(
            |(user, asset, amount, external_tx_ref)| TxBody::Deposit {
                user,
                asset,
                amount,
                external_tx_ref
            }
        ),
        (asset.clone(), any::<u128>(), text(64)).prop_map(|(asset, amount, external_address)| {
            TxBody::Withdraw {
                asset,
                amount,
                external_address,
            }
        }),
        (h.clone(), text(64)).prop_map(|(withdraw_id, external_tx_ref)| TxBody::WithdrawSettled {
            withdraw_id,
            external_tx_ref
        }),
        (
            market.clone(),
            side.clone(),
            any::<u128>(),
            any::<u128>(),
            tif.clone(),
            any::<bool>(),
            h.clone()
        )
            .prop_map(
                |(market, side, price, qty, tif, post_only, client_order_id)| {
                    TxBody::PlaceOrder {
                        market,
                        side,
                        price,
                        qty,
                        tif,
                        post_only,
                        client_order_id,
                    }
                }
            ),
        h.clone()
            .prop_map(|order_id| TxBody::CancelOrder { order_id }),
        market.prop_map(|market| TxBody::BatchCancel { market }),
        (
            deriv,
            side,
            any::<u128>(),
            any::<u128>(),
            tif,
            any::<bool>(),
            any::<bool>(),
            h
        )
            .prop_map(
                |(market, side, price_q8, qty, tif, post_only, reduce_only, client_order_id)| {
                    TxBody::PlacePerpOrder {
                        market,
                        side,
                        price_q8,
                        qty,
                        tif,
                        post_only,
                        reduce_only,
                        client_order_id,
                    }
                }
            ),
    ]
}

fn signed_tx() -> impl Strategy<Value = SignedTx> {
    (
        any::<u64>(),
        any::<u32>(),
        body(),
        any::<[u8; 32]>(),
        any::<[u8; 32]>(),
        any::<u8>(),
    )
        .prop_map(
            |(nonce, chain_id, body, sig_r, sig_s, sig_recovery)| SignedTx {
                tx: Transaction {
                    nonce,
                    chain_id,
                    body,
                },
                sig_r,
                sig_s,
                sig_recovery,
            },
        )
}

#[derive(Clone, Debug)]
enum Mutation {
    Flip { at: usize, mask: u8 },
    Truncate { to: usize },
    Extend { bytes: Vec<u8> },
    Overwrite { at: usize, word: [u8; 8] },
}

fn mutation() -> impl Strategy<Value = Mutation> {
    prop_oneof![
        4 => (any::<usize>(), 1u8..=255).prop_map(|(at, mask)| Mutation::Flip { at, mask }),
        1 => any::<usize>().prop_map(|to| Mutation::Truncate { to }),
        1 => prop::collection::vec(any::<u8>(), 1..64).prop_map(|bytes| Mutation::Extend { bytes }),
        2 => (any::<usize>(), any::<[u8; 8]>()).prop_map(|(at, word)| Mutation::Overwrite { at, word }),
    ]
}

fn apply(mut b: Vec<u8>, muts: &[Mutation]) -> Vec<u8> {
    for m in muts {
        if b.is_empty() {
            break;
        }
        match m {
            Mutation::Flip { at, mask } => {
                let i = at % b.len();
                b[i] ^= mask;
            }
            Mutation::Truncate { to } => {
                let n = to % b.len();
                b.truncate(n);
            }
            Mutation::Extend { bytes } => b.extend_from_slice(bytes),
            Mutation::Overwrite { at, word } => {
                let i = at % b.len();
                let end = (i + 8).min(b.len());
                b[i..end].copy_from_slice(&word[..end - i]);
            }
        }
    }
    b
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn well_formed_transactions_round_trip(s in signed_tx()) {
        let back = decode_tx(&encode_tx(&s)).expect("frames").expect("decodes");
        prop_assert_eq!(back, s);
    }

    #[test]
    fn mutated_transactions_never_panic_and_what_decodes_is_stable(
        s in signed_tx(),
        muts in prop::collection::vec(mutation(), 1..6),
    ) {
        let bytes = apply(encode_tx(&s), &muts);
        if let Some(Ok(decoded)) = decode_tx(&bytes) {
            let again = decode_tx(&encode_tx(&decoded)).expect("frames").expect("decodes");
            prop_assert_eq!(again, decoded);
        }
    }

    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        let _ = decode_tx(&bytes);
        let _ = decode_vote(&bytes);
    }

    #[test]
    fn mutated_votes_never_panic_and_respect_the_signature_cap(
        view in any::<u64>(),
        height in any::<u64>(),
        sig in prop::collection::vec(any::<u8>(), 0..=MAX_VOTE_SIG_LEN),
        muts in prop::collection::vec(mutation(), 0..6),
    ) {
        let v = cb_consensus::Vote {
            view,
            height,
            block_hash: H256([3; 32]),
            validator: Address([4; 20]),
            sig,
        };
        if muts.is_empty() {
            let back = decode_vote(&encode_vote(&v)).expect("frames").expect("decodes");
            prop_assert_eq!(back, v);
        } else if let Some(Ok(d)) = decode_vote(&apply(encode_vote(&v), &muts)) {
            prop_assert!(d.sig.len() <= MAX_VOTE_SIG_LEN);
        }
    }

    /// Text past the cap never decodes, whatever else the transaction says.
    #[test]
    fn text_past_the_cap_is_refused(extra in 1usize..2048, which in 0u8..3) {
        let long = "q".repeat(MAX_EXTERNAL_REF_LEN + extra);
        let body = match which {
            0 => TxBody::Deposit { user: Address([1; 20]), asset: AssetId::Usd, amount: 1, external_tx_ref: long },
            1 => TxBody::Withdraw { asset: AssetId::Usd, amount: 1, external_address: long },
            _ => TxBody::WithdrawSettled { withdraw_id: H256([1; 32]), external_tx_ref: long },
        };
        let s = SignedTx {
            tx: Transaction { nonce: 0, chain_id: 1, body },
            sig_r: [1; 32],
            sig_s: [1; 32],
            sig_recovery: 0,
        };
        let r = decode_tx(&encode_tx(&s)).expect("frames");
        prop_assert!(r.is_err());
    }
}

/// Multi-byte characters count as bytes: 64 four-byte characters are 256
/// bytes and fit; one more does not.
#[test]
fn the_text_cap_counts_bytes_not_characters() {
    let mk = |s: String| SignedTx {
        tx: Transaction {
            nonce: 0,
            chain_id: 1,
            body: TxBody::Withdraw {
                asset: AssetId::Usd,
                amount: 1,
                external_address: s,
            },
        },
        sig_r: [1; 32],
        sig_s: [1; 32],
        sig_recovery: 0,
    };
    let fits = "\u{1F600}".repeat(MAX_EXTERNAL_REF_LEN / 4);
    assert_eq!(fits.len(), MAX_EXTERNAL_REF_LEN);
    assert!(decode_tx(&encode_tx(&mk(fits))).unwrap().is_ok());
    let over = "\u{1F600}".repeat(MAX_EXTERNAL_REF_LEN / 4 + 1);
    assert!(decode_tx(&encode_tx(&mk(over))).unwrap().is_err());
}
