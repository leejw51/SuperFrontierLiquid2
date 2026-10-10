//! Integration tests for `OracleUpdate` execution (M2).
//!
//! Covers the executor's authority check, stale-height rejection, unknown
//! market rejection, and the happy path: state-write of `OraclePrice` plus
//! `MarkPrice` recompute.

use cb_block::{apply_tx, BlockState, ExecCtx, ExecError};
use cb_crypto::{address_from_pubkey, generate_keypair};
use cb_state::{Database, Meta, OracleState, PerpMarkets};
use cb_tx::{SignedTx, Transaction, TxBody};
use cb_types::{Address, AssetId, DerivId, PerpMarket};
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
        funding_interval_blocks: 3600,
        funding_rate_cap_q9: 5_000_000, // 0.5% / hr
        oracle_authority: authority,
        max_position_notional: 100_000_000_000_000,
        paused: false,
        last_trade_px_q8: 0,
        last_trade_height: 0,
        paused_reduce_only_only: false,
    }
}

fn signed_tx(sk: &secp256k1::SecretKey, nonce: u64, body: TxBody) -> SignedTx {
    SignedTx::sign_with(
        sk,
        Transaction {
            nonce,
            chain_id: 1,
            body,
        },
    )
    .unwrap()
}

#[test]
fn oracle_update_happy_path_writes_oracle_and_mark() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    // Transactions here are signed for chain 1; the executor rejects a tx whose
    // `chain_id` does not match the chain's own, so the fixture must declare it.
    // `Meta` is outside the state-root preimage, so no asserted root changes.
    Meta::new(&db).set_chain_id(1);
    let (auth_sk, auth_pk) = generate_keypair();
    let auth = address_from_pubkey(&auth_pk);
    let market = DerivId::perp(0);
    PerpMarkets::new(&db).put(&perp_market(auth));

    let body = TxBody::OracleUpdate {
        market,
        px_q8: 50_000u128 * 100_000_000,
        submitted_height: 100,
    };
    let v = signed_tx(&auth_sk, 0, body).verify().unwrap();
    let mut state = BlockState::new(&db);
    apply_tx(
        &mut state,
        &ExecCtx {
            block_height: 100,
            ..Default::default()
        },
        &v,
    )
    .unwrap();
    state.commit().unwrap();

    let store = OracleState::new(&db);
    let o = store.get_oracle(&market).unwrap();
    assert_eq!(o.px_q8, 50_000u128 * 100_000_000);
    assert_eq!(o.source_height, 100);
    assert_eq!(o.authority, auth);

    // Mark recomputed too — with no book yet, mark == oracle.
    let m = store.get_mark(&market).unwrap();
    assert_eq!(m.px_q8, o.px_q8);
    assert_eq!(m.computed_at_height, 100);
}

#[test]
fn oracle_update_rejects_wrong_authority() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (_auth_sk, auth_pk) = generate_keypair();
    let auth = address_from_pubkey(&auth_pk);
    let (attacker_sk, _) = generate_keypair();
    let market = DerivId::perp(0);
    PerpMarkets::new(&db).put(&perp_market(auth));

    let body = TxBody::OracleUpdate {
        market,
        px_q8: 50_000u128 * 100_000_000,
        submitted_height: 100,
    };
    let v = signed_tx(&attacker_sk, 0, body).verify().unwrap();
    let mut state = BlockState::new(&db);
    let err = apply_tx(
        &mut state,
        &ExecCtx {
            block_height: 100,
            ..Default::default()
        },
        &v,
    )
    .unwrap_err();
    assert_eq!(err, ExecError::NotAuthorized);

    // No oracle row should have been written.
    assert!(OracleState::new(&db).get_oracle(&market).is_none());
}

#[test]
fn oracle_update_rejects_stale_height() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (auth_sk, auth_pk) = generate_keypair();
    let auth = address_from_pubkey(&auth_pk);
    let market = DerivId::perp(0);
    PerpMarkets::new(&db).put(&perp_market(auth));

    // First update lands at height 100.
    let body = TxBody::OracleUpdate {
        market,
        px_q8: 50_000u128 * 100_000_000,
        submitted_height: 100,
    };
    let v = signed_tx(&auth_sk, 0, body).verify().unwrap();
    let mut state = BlockState::new(&db);
    apply_tx(
        &mut state,
        &ExecCtx {
            block_height: 100,
            ..Default::default()
        },
        &v,
    )
    .unwrap();
    state.commit().unwrap();

    // Second tries to land at height 99 — strictly older. Reject.
    let body2 = TxBody::OracleUpdate {
        market,
        px_q8: 51_000u128 * 100_000_000,
        submitted_height: 99,
    };
    let v2 = signed_tx(&auth_sk, 1, body2).verify().unwrap();
    let mut state = BlockState::new(&db);
    let err = apply_tx(
        &mut state,
        &ExecCtx {
            block_height: 101,
            ..Default::default()
        },
        &v2,
    )
    .unwrap_err();
    assert_eq!(err, ExecError::OracleStale);
    // Stored price unchanged.
    let o = OracleState::new(&db).get_oracle(&market).unwrap();
    assert_eq!(o.px_q8, 50_000u128 * 100_000_000);
}

#[test]
fn oracle_update_rejects_unknown_market() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (auth_sk, _) = generate_keypair();
    // Note: no PerpMarket registered.
    let market = DerivId::perp(99);

    let body = TxBody::OracleUpdate {
        market,
        px_q8: 50_000u128 * 100_000_000,
        submitted_height: 100,
    };
    let v = signed_tx(&auth_sk, 0, body).verify().unwrap();
    let mut state = BlockState::new(&db);
    let err = apply_tx(
        &mut state,
        &ExecCtx {
            block_height: 100,
            ..Default::default()
        },
        &v,
    )
    .unwrap_err();
    assert_eq!(err, ExecError::UnknownMarket);
}

#[test]
fn oracle_update_at_equal_height_is_idempotent() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (auth_sk, auth_pk) = generate_keypair();
    let auth = address_from_pubkey(&auth_pk);
    let market = DerivId::perp(0);
    PerpMarkets::new(&db).put(&perp_market(auth));

    let body1 = TxBody::OracleUpdate {
        market,
        px_q8: 50_000u128 * 100_000_000,
        submitted_height: 100,
    };
    let v1 = signed_tx(&auth_sk, 0, body1).verify().unwrap();
    let mut state = BlockState::new(&db);
    apply_tx(
        &mut state,
        &ExecCtx {
            block_height: 100,
            ..Default::default()
        },
        &v1,
    )
    .unwrap();
    state.commit().unwrap();

    // Same height, different price — accepted (leader's prerogative; signed
    // by the authority anyway).
    let body2 = TxBody::OracleUpdate {
        market,
        px_q8: 50_500u128 * 100_000_000,
        submitted_height: 100,
    };
    let v2 = signed_tx(&auth_sk, 1, body2).verify().unwrap();
    let mut state = BlockState::new(&db);
    apply_tx(
        &mut state,
        &ExecCtx {
            block_height: 101,
            ..Default::default()
        },
        &v2,
    )
    .unwrap();
    state.commit().unwrap();

    let o = OracleState::new(&db).get_oracle(&market).unwrap();
    assert_eq!(o.px_q8, 50_500u128 * 100_000_000);
}

#[test]
fn an_oracle_update_from_a_height_the_chain_has_not_reached_is_rejected() {
    // Core review M1. `submitted_height` was bounded below (stale posts are
    // refused) and not above, so one post carrying `u64::MAX` made every later
    // update "stale" for the life of the chain, with no operator action able
    // to undo it. A price cannot have been observed at a height that does not
    // exist yet; the block's own height is the ceiling.
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (auth_sk, auth_pk) = generate_keypair();
    let auth = address_from_pubkey(&auth_pk);
    let market = DerivId::perp(0);
    PerpMarkets::new(&db).put(&perp_market(auth));

    let from_the_future = signed_tx(
        &auth_sk,
        0,
        TxBody::OracleUpdate {
            market,
            px_q8: 50_000u128 * 100_000_000,
            submitted_height: u64::MAX,
        },
    )
    .verify()
    .unwrap();
    let mut state = BlockState::new(&db);
    let err = apply_tx(
        &mut state,
        &ExecCtx {
            block_height: 100,
            ..Default::default()
        },
        &from_the_future,
    )
    .unwrap_err();
    assert_eq!(
        err,
        ExecError::OracleFromFuture {
            submitted: u64::MAX,
            height: 100
        }
    );
    state.commit().unwrap();
    assert!(OracleState::new(&db).get_oracle(&market).is_none());

    // The block's own height is fine — that is what a publisher that reads
    // the head and signs for the next block produces.
    let at_the_head = signed_tx(
        &auth_sk,
        0,
        TxBody::OracleUpdate {
            market,
            px_q8: 50_000u128 * 100_000_000,
            submitted_height: 100,
        },
    )
    .verify()
    .unwrap();
    let mut state = BlockState::new(&db);
    apply_tx(
        &mut state,
        &ExecCtx {
            block_height: 100,
            ..Default::default()
        },
        &at_the_head,
    )
    .unwrap();
    state.commit().unwrap();
    assert_eq!(
        OracleState::new(&db)
            .get_oracle(&market)
            .unwrap()
            .source_height,
        100
    );
}
