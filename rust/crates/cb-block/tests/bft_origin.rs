//! Execution under BFT consensus (`execute_block_with` an origin): the
//! required system transactions are injected unsigned and consume no nonce,
//! the block keeps the committed payload whole, and an observer replaying
//! with the same origin reaches the same root.

use cb_block::{
    execute_block_with, synthetic_system_txs, verify_and_commit_with, BlockOrigin, ExecCtx,
    Executed,
};
use cb_crypto::{address_from_pubkey, generate_keypair};
use cb_state::{AccountState, Accounts, Database, FundingIndices, Meta, PerpMarkets};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{Address, AssetId, DerivId, PerpMarket, H256};
use tempfile::tempdir;

fn perp_market(authority: Address) -> PerpMarket {
    PerpMarket {
        id: DerivId::perp(0),
        underlying: AssetId::Btc,
        tick_size: 100,
        lot_size: 1000,
        max_leverage_x100: 5000,
        initial_margin_bps: 200,
        maintenance_margin_bps: 100,
        taker_fee_bps: 3,
        maker_fee_bps_signed: -1,
        funding_interval_blocks: 2,
        funding_rate_cap_q9: 5_000_000,
        oracle_authority: authority,
        max_position_notional: 100_000_000_000_000,
        paused: false,
        last_trade_px_q8: 0,
        last_trade_height: 0,
        paused_reduce_only_only: false,
    }
}

fn db_with(alice: Address, proposer: Address) -> (tempfile::TempDir, Database) {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    Meta::new(&db).set_consensus_scheme(cb_state::CONSENSUS_BFT);
    PerpMarkets::new(&db).put(&perp_market(proposer));
    let mut s = AccountState::default();
    s.add(AssetId::Usd, 1_000).unwrap();
    Accounts::new(&db).put(&alice, &s);
    let mut p = AccountState::default();
    p.add(AssetId::Usd, 1_000).unwrap();
    p.nonce = 7;
    Accounts::new(&db).put(&proposer, &p);
    (dir, db)
}

fn transfer(sk: &secp256k1::SecretKey, nonce: u64, to: Address) -> VerifiedTx {
    SignedTx::sign_with(
        sk,
        Transaction {
            nonce,
            chain_id: 1,
            body: TxBody::Transfer {
                to,
                asset: AssetId::Usd,
                amount: 1,
            },
        },
    )
    .unwrap()
    .verify()
    .unwrap()
}

fn ctx() -> ExecCtx {
    ExecCtx {
        treasury_signer: Address::ZERO,
        ..Default::default()
    }
}

fn commit(db: &Database, ex: Executed) {
    // Land the rows the way the sequencer's synchronous path does, without
    // sealing a header: what these tests compare is the executed state.
    let mut batch = db.batch();
    Database::maps_into_batch(&mut batch, &ex.rows).unwrap();
    db.write_batch(batch).unwrap();
    Meta::new(db).set_head(ex.height, H256([ex.height as u8; 32]), H256::ZERO);
}

#[test]
fn the_funding_tick_is_injected_unsigned_and_consumes_no_nonce() {
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let (_psk, ppk) = generate_keypair();
    let proposer = address_from_pubkey(&ppk);
    let (_d, db) = db_with(alice, proposer);
    let origin = BlockOrigin {
        seed: H256([1; 32]),
        proposer,
    };

    // Height 1: no boundary, nothing injected.
    let ex = execute_block_with(
        &db,
        &ctx(),
        vec![transfer(&alice_sk, 0, proposer)],
        10,
        Some(origin),
        |_, _, _| {},
    )
    .unwrap();
    assert_eq!(ex.txs.len(), 1);
    assert_eq!(ex.applied, vec![true]);
    assert!(synthetic_system_txs(&db, 1, proposer, 1).is_empty());
    commit(&db, ex);

    // Height 2: the funding boundary. One synthetic tick, executed first,
    // not in the block, and the proposer's nonce untouched.
    assert_eq!(synthetic_system_txs(&db, 2, proposer, 1).len(), 1);
    let ex = execute_block_with(
        &db,
        &ctx(),
        vec![transfer(&alice_sk, 1, proposer)],
        20,
        Some(origin),
        |_, _, _| {},
    )
    .unwrap();
    assert_eq!(ex.txs.len(), 1, "the tick is not part of the block");
    assert!(ex.txs.iter().all(|v| !v.is_synthetic()));
    assert!(ex.rejected.is_empty());
    commit(&db, ex);
    assert_eq!(
        FundingIndices::new(&db)
            .get(&DerivId::perp(0))
            .unwrap()
            .last_tick_height,
        2
    );
    assert_eq!(
        Accounts::new(&db).get(&proposer).unwrap().nonce,
        7,
        "no nonce consumed"
    );
}

#[test]
fn a_refused_transaction_stays_in_the_block_and_the_observer_replay_matches() {
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let (_psk, ppk) = generate_keypair();
    let proposer = address_from_pubkey(&ppk);
    let origin = BlockOrigin {
        seed: H256([2; 32]),
        proposer,
    };
    let payload = vec![
        transfer(&alice_sk, 0, proposer),
        transfer(&alice_sk, 9, proposer), // a gap: refused
        transfer(&alice_sk, 1, proposer),
    ];
    let ordered = cb_block::order_for_block(payload, origin.seed);

    // The validator's execution.
    let (_d1, v) = db_with(alice, proposer);
    let ex =
        execute_block_with(&v, &ctx(), ordered.clone(), 10, Some(origin), |_, _, _| {}).unwrap();
    assert_eq!(ex.txs.len(), 3);
    assert_eq!(ex.applied.iter().filter(|a| **a).count(), 2);
    assert_eq!(ex.rejected.len(), 1);
    let (diff, _) = cb_state::diff_hash(&ex.rows);
    let root_v = cb_state::chain_state_hash(Meta::new(&v).state_root(), diff);
    commit(&v, ex);
    assert_eq!(Accounts::new(&v).get(&alice).unwrap().nonce, 2);

    // An observer replaying the block with the skip rule and the synthetic
    // prefix reaches the same root.
    let (_d2, o) = db_with(alice, proposer);
    let mut txs = synthetic_system_txs(&o, 1, proposer, 1);
    txs.extend(ordered.iter().cloned());
    let block_ctx = ExecCtx {
        treasury_signer: Address::ZERO,
        block_height: 1,
        proposer_addr: proposer,
    };
    let root_o = verify_and_commit_with(
        &o,
        &block_ctx,
        &txs,
        1,
        H256([9; 32]),
        b"body",
        root_v,
        10,
        &[],
        true,
        None,
    )
    .expect("observer replays with the same outcome");
    assert_eq!(root_o, root_v);
    assert_eq!(Accounts::new(&o).get(&alice).unwrap().nonce, 2);

    // Without the skip rule the same block fails, as it should under the
    // sequencer scheme where a refused transaction never enters a block.
    let (_d3, strict) = db_with(alice, proposer);
    let err = verify_and_commit_with(
        &strict,
        &block_ctx,
        &txs,
        1,
        H256([9; 32]),
        b"body",
        root_v,
        10,
        &[],
        false,
        None,
    )
    .unwrap_err();
    assert!(matches!(err, cb_block::VerifyError::Apply(_)));
}

#[test]
fn an_unsigned_transaction_that_is_not_a_system_action_is_refused() {
    let (_ask, apk) = generate_keypair();
    let alice = address_from_pubkey(&apk);
    let (_psk, ppk) = generate_keypair();
    let proposer = address_from_pubkey(&ppk);
    let (_d, db) = db_with(alice, proposer);
    let forged = VerifiedTx::synthetic(
        Transaction {
            nonce: 0,
            chain_id: 1,
            body: TxBody::Transfer {
                to: proposer,
                asset: AssetId::Usd,
                amount: 500,
            },
        },
        alice,
    );
    let ex = execute_block_with(
        &db,
        &ctx(),
        vec![forged],
        10,
        Some(BlockOrigin {
            seed: H256::ZERO,
            proposer,
        }),
        |_, _, _| {},
    );
    assert!(matches!(
        ex,
        Err(cb_block::ExecError::NoApplicableTxs { ref rejected }) if rejected.len() == 1
    ));
    assert_eq!(
        Accounts::new(&db)
            .get(&alice)
            .unwrap()
            .available(AssetId::Usd),
        1_000
    );
}
