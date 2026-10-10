//! Core review H3: intra-block ordering must never break an account's nonce
//! order.
//!
//! `order_for_block` ranks cancellations ahead of placements so that, between
//! accounts, a cancel lands before the order flow that would have hit the
//! quote. It used to sort every transaction by `(rank, sender, nonce)`, which
//! also moved an account's *own* cancel ahead of the placement it cancelled
//! whenever both were in one block — where the cancel failed `BadNonce`, was
//! dropped, and the placement executed. The gateway pipelines nonces inside a
//! block interval, so "place, then cancel" within one block is ordinary
//! traffic, and a cancel that silently does nothing is the worst outcome an
//! order-management client can have.

use cb_block::{build_and_apply, order_for_block, ExecCtx};
use cb_crypto::{
    address_from_pubkey, keccak256, keypair_from_seed, sign as crypto_sign, SecretKey,
};
use cb_state::{Accounts, Database, MarketConfig, Markets, Meta, Orders};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{Address, AssetId, MarketId, Side, Tif, H256};
use tempfile::tempdir;

const MARKET: MarketId = MarketId::BtcUsd;
const Q8: u128 = 100_000_000;

fn setup() -> (tempfile::TempDir, Database) {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    Markets::new(&db).put(&MarketConfig {
        id: MARKET,
        base: AssetId::Btc,
        quote: AssetId::Usd,
        tick_size: 1,
        min_qty: 1,
        fee_bps: 10,
    });
    (dir, db)
}

fn actor(seed: &str) -> (SecretKey, Address) {
    let (sk, pk) = keypair_from_seed(seed.as_bytes());
    (sk, address_from_pubkey(&pk))
}

fn fund(db: &Database, addr: Address, asset: AssetId, amount: u128) {
    let accounts = Accounts::new(db);
    let mut s = accounts.get(&addr).unwrap_or_default();
    s.add(asset, amount).expect("seed balance");
    accounts.put(&addr, &s);
}

fn sign(sk: &SecretKey, nonce: u64, body: TxBody) -> VerifiedTx {
    SignedTx::sign_with(
        sk,
        Transaction {
            nonce,
            chain_id: 1,
            body,
        },
    )
    .unwrap()
    .verify()
    .unwrap()
}

fn place(sk: &SecretKey, nonce: u64, side: Side, price: u128, qty: u128) -> VerifiedTx {
    sign(
        sk,
        nonce,
        TxBody::PlaceOrder {
            market: MARKET,
            side,
            price,
            qty,
            tif: Tif::Gtc,
            post_only: false,
            client_order_id: H256([nonce as u8; 32]),
        },
    )
}

/// The id the executor derives for the order placed by `sender` at
/// `tx_index` in the block at `height` — the layout `derive_order_id` uses,
/// so a cancel can name an order that does not exist yet.
fn expected_order_id(sender: Address, height: u64, tx_index: u32) -> H256 {
    let mut bytes = [0u8; 32];
    bytes[..20].copy_from_slice(&sender.0);
    bytes[20..28].copy_from_slice(&height.to_be_bytes());
    bytes[28..32].copy_from_slice(&tx_index.to_be_bytes());
    H256(bytes)
}

#[test]
fn a_cancel_after_a_placement_from_the_same_account_applies_in_nonce_order() {
    let (_dir, db) = setup();
    let (alice_sk, alice) = actor("ordering-alice");
    fund(&db, alice, AssetId::Usd, 1_000_000 * Q8);
    let (proposer_sk, proposer) = actor("ordering-proposer");
    Meta::new(&db).set_proposer_addr(proposer);

    // Both land in block 1. The placement sorts to index 0 of the block, so
    // the id the cancel must name is derived from (alice, 1, 0).
    let placed = place(&alice_sk, 0, Side::Buy, 100 * Q8, 1_000);
    let cancel = sign(
        &alice_sk,
        1,
        TxBody::CancelOrder {
            order_id: expected_order_id(alice, 1, 0),
        },
    );

    // Hand them to the builder cancel-first, the way a mempool drain that
    // happens to iterate by hash could.
    let signer = |preimage: &[u8]| {
        crypto_sign(&proposer_sk, keccak256(preimage))
            .unwrap()
            .to_vec()
    };
    let res = build_and_apply(
        &db,
        &ExecCtx::for_treasury(Address::ZERO),
        vec![cancel, placed],
        0,
        0,
        Vec::new(),
        signer,
    )
    .unwrap();

    assert!(
        res.rejected.is_empty(),
        "both transactions must apply; rejected: {:?}",
        res.rejected
    );
    assert_eq!(res.block.txs.len(), 2);
    assert_eq!(res.block.txs[0].tx.nonce, 0, "the placement executes first");
    assert_eq!(res.block.txs[1].tx.nonce, 1, "then the cancel");
    assert!(
        Orders::new(&db).list_for_market(MARKET).is_empty(),
        "the order was placed and then cancelled"
    );
    let alice_state = Accounts::new(&db).get(&alice).unwrap();
    assert_eq!(alice_state.nonce, 2);
    assert_eq!(
        alice_state.locked_of(AssetId::Usd),
        0,
        "the cancel released the placement's lock"
    );
}

#[test]
fn cancels_still_go_ahead_of_other_accounts_placements() {
    // The rule the sorter exists for is kept: between accounts, a cancel
    // beats a placement regardless of address order. Bob's address sorts
    // after Alice's, and his cancel still comes first.
    let (_dir, db) = setup();
    let (alice_sk, alice) = actor("ordering-alice");
    let (bob_sk, bob) = actor("ordering-bob");
    let (lo, hi, lo_sk, hi_sk) = if alice.0 < bob.0 {
        (alice, bob, alice_sk, bob_sk)
    } else {
        (bob, alice, bob_sk, alice_sk)
    };
    fund(&db, lo, AssetId::Usd, 1_000_000 * Q8);
    fund(&db, hi, AssetId::Usd, 1_000_000 * Q8);

    let lo_place = place(&lo_sk, 0, Side::Buy, 100 * Q8, 1_000);
    let hi_cancel = sign(
        &hi_sk,
        0,
        TxBody::CancelOrder {
            order_id: H256([9; 32]),
        },
    );
    let hi_place = place(&hi_sk, 1, Side::Buy, 100 * Q8, 1_000);

    let ordered = order_for_block(vec![lo_place, hi_place, hi_cancel], H256([3; 32]));
    let senders: Vec<Address> = ordered.iter().map(|v| v.sender).collect();
    let nonces: Vec<u64> = ordered.iter().map(|v| v.signed.tx.nonce).collect();
    assert_eq!(
        senders,
        vec![hi, hi, lo],
        "the cancelling account's group leads"
    );
    assert_eq!(
        nonces,
        vec![0, 1, 0],
        "and its own transactions stay in nonce order"
    );
}
