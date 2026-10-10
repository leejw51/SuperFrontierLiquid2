//! Cross-chain replay protection.
//!
//! `Transaction::chain_id` is covered by the signature, which makes every
//! transaction a commitment to run on one specific chain. That commitment is
//! only worth something if the executor actually compares it against the chain
//! it is running on — otherwise a transaction captured from a testnet, or from
//! a fork, is a valid transaction on mainnet with a valid signature, and the
//! field is decoration.
//!
//! These tests pin the comparison. They are deliberately about the *executor*
//! rather than an admission filter: a check that lives only in the RPC layer is
//! bypassed by any block received from a peer, and consensus would then accept
//! blocks a correct node rejects.

use tempfile::tempdir;

use cb_block::{apply_tx_at, BlockState, ExecCtx, ExecError};
use cb_crypto::{address_from_pubkey, generate_keypair, SecretKey};
use cb_state::{Accounts, Database, Meta};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{Address, AssetId, H256};

/// The chain under test. Deliberately not 1, so a handler that defaulted to 1
/// somewhere would fail these tests rather than pass them by accident.
const THIS_CHAIN: u32 = 7;

fn ctx() -> ExecCtx {
    ExecCtx {
        treasury_signer: Address::ZERO,
        block_height: 1,
        proposer_addr: Address::ZERO,
    }
}

fn signed_transfer(sk: &SecretKey, chain_id: u32, to: Address, amount: u128) -> VerifiedTx {
    SignedTx::sign_with(
        sk,
        Transaction {
            nonce: 0,
            chain_id,
            body: TxBody::Transfer {
                to,
                asset: AssetId::Usd,
                amount,
            },
        },
    )
    .unwrap()
    .verify()
    .unwrap()
}

fn fresh_chain(dir: &tempfile::TempDir, chain_id: u32) -> Database {
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(chain_id);
    db
}

#[test]
fn a_transaction_for_this_chain_is_accepted() {
    let dir = tempdir().unwrap();
    let db = fresh_chain(&dir, THIS_CHAIN);

    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    let bob = Address([9; 20]);
    let accts = Accounts::new(&db);
    let mut s = accts.get(&alice).unwrap_or_default();
    s.add(AssetId::Usd, 1_000).expect("seed balance");
    accts.put(&alice, &s);

    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx(),
        &signed_transfer(&sk, THIS_CHAIN, bob, 100),
        0,
    )
    .expect("matching chain id is accepted");
    state.commit().unwrap();

    assert_eq!(
        Accounts::new(&db).get(&bob).unwrap().balance(AssetId::Usd),
        100
    );
}

#[test]
fn a_transaction_for_another_chain_is_rejected() {
    // The replay: a correctly signed, correctly nonced, fully funded transfer
    // that was minted for chain 8. It must not execute here.
    let dir = tempdir().unwrap();
    let db = fresh_chain(&dir, THIS_CHAIN);

    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    let bob = Address([9; 20]);
    let accts = Accounts::new(&db);
    let mut s = accts.get(&alice).unwrap_or_default();
    s.add(AssetId::Usd, 1_000).expect("seed balance");
    accts.put(&alice, &s);

    let foreign = signed_transfer(&sk, THIS_CHAIN + 1, bob, 100);
    // The signature itself is perfectly valid — that is the whole point.
    assert_eq!(foreign.sender, alice);

    let mut state = BlockState::new(&db);
    let err = apply_tx_at(&mut state, &ctx(), &foreign, 0)
        .expect_err("a transaction for another chain must not execute");
    match err {
        ExecError::WrongChainId { expected, got } => {
            assert_eq!(expected, THIS_CHAIN);
            assert_eq!(got, THIS_CHAIN + 1);
        }
        other => panic!("expected WrongChainId, got {other:?}"),
    }

    // Rejected before any mutation: no transfer, no nonce burn.
    state.commit().unwrap();
    let after = Accounts::new(&db);
    assert_eq!(after.get(&alice).unwrap().balance(AssetId::Usd), 1_000);
    assert_eq!(after.get(&alice).unwrap().nonce, 0);
    assert_eq!(after.get(&bob).unwrap_or_default().balance(AssetId::Usd), 0);
}

#[test]
fn the_chain_id_check_runs_before_the_nonce_check() {
    // A foreign transaction that ALSO has a bad nonce must report the chain
    // mismatch. Reporting `BadNonce` would send an operator looking at their
    // sequencer when the real problem is that they are pointed at the wrong
    // chain entirely.
    let dir = tempdir().unwrap();
    let db = fresh_chain(&dir, THIS_CHAIN);

    let (sk, _pk) = generate_keypair();
    let bad = SignedTx::sign_with(
        &sk,
        Transaction {
            nonce: 42, // wrong too
            chain_id: THIS_CHAIN + 1,
            body: TxBody::Transfer {
                to: Address([9; 20]),
                asset: AssetId::Usd,
                amount: 1,
            },
        },
    )
    .unwrap()
    .verify()
    .unwrap();

    let mut state = BlockState::new(&db);
    let err = apply_tx_at(&mut state, &ctx(), &bad, 0).unwrap_err();
    assert!(
        matches!(err, ExecError::WrongChainId { .. }),
        "expected WrongChainId to take precedence, got {err:?}"
    );
}

#[test]
fn every_transaction_body_is_gated_not_just_transfers() {
    // The check sits at the top of `apply_tx_at`, so it covers every body
    // variant. Spot-check a non-transfer one: an order that would otherwise
    // fail with `MarketNotFound` must fail on the chain id first.
    let dir = tempdir().unwrap();
    let db = fresh_chain(&dir, THIS_CHAIN);

    let (sk, _pk) = generate_keypair();
    let order = SignedTx::sign_with(
        &sk,
        Transaction {
            nonce: 0,
            chain_id: THIS_CHAIN + 1,
            body: TxBody::PlaceOrder {
                market: cb_types::MarketId::BtcUsd,
                side: cb_types::Side::Buy,
                price: 1_000,
                qty: 1,
                tif: cb_types::Tif::Gtc,
                post_only: false,
                client_order_id: H256([1; 32]),
            },
        },
    )
    .unwrap()
    .verify()
    .unwrap();

    let mut state = BlockState::new(&db);
    let err = apply_tx_at(&mut state, &ctx(), &order, 0).unwrap_err();
    assert!(
        matches!(err, ExecError::WrongChainId { .. }),
        "expected WrongChainId, got {err:?}"
    );
}

#[test]
fn a_chain_that_never_declared_an_id_accepts_only_zero() {
    // `Meta::chain_id()` returns 0 for a database genesis never touched. That
    // is treated as a real chain id of 0, not as "accept anything": a
    // permissive default would mean a misconfigured node silently dropped its
    // replay protection, which is the failure you least want to be quiet.
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap(); // no set_chain_id
    assert_eq!(Meta::new(&db).chain_id(), 0);

    let (sk, _pk) = generate_keypair();
    let mut state = BlockState::new(&db);
    let err = apply_tx_at(
        &mut state,
        &ctx(),
        &signed_transfer(&sk, 1, Address([9; 20]), 1),
        0,
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            ExecError::WrongChainId {
                expected: 0,
                got: 1
            }
        ),
        "an undeclared chain id must not act as a wildcard, got {err:?}"
    );
}
