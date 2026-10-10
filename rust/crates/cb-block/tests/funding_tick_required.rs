//! Block-level enforcement: at every funding-interval boundary, the block MUST
//! contain a `FundingTick` system tx for each registered perp market, signed
//! by the proposer. Validators reject blocks missing it.
//!
//! Tested both via `build_and_apply` (leader path) and the `enforce_required_funding_ticks`
//! helper (used by replicas in `apply_remote_block`).

use cb_block::{build_and_apply, enforce_required_funding_ticks, ExecCtx, ExecError};
use cb_crypto::{address_from_pubkey, generate_keypair, keccak256, sign as crypto_sign};
use cb_state::{Database, Meta, PerpMarkets};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{Address, AssetId, DerivId, PerpMarket};
use tempfile::tempdir;

const PERP_INDEX: u16 = 0;
const TEST_FUNDING_INTERVAL: u32 = 5;

fn perp_market(authority: Address) -> PerpMarket {
    PerpMarket {
        id: DerivId::perp(PERP_INDEX),
        underlying: AssetId::Btc,
        tick_size: 100,
        lot_size: 1000,
        max_leverage_x100: 5000,
        initial_margin_bps: 200,
        maintenance_margin_bps: 100,
        taker_fee_bps: 3,
        maker_fee_bps_signed: -1,
        funding_interval_blocks: TEST_FUNDING_INTERVAL,
        funding_rate_cap_q9: 5_000_000,
        oracle_authority: authority,
        max_position_notional: 100_000_000_000_000,
        paused: false,
        last_trade_px_q8: 0,
        last_trade_height: 0,
        paused_reduce_only_only: false,
    }
}

fn sign_tx(sk: &secp256k1::SecretKey, nonce: u64, body: TxBody) -> VerifiedTx {
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

#[test]
fn enforce_helper_passes_when_required_tick_present() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    // Transactions here are signed for chain 1; the executor rejects a tx whose
    // `chain_id` does not match the chain's own, so the fixture must declare it.
    // `Meta` is outside the state-root preimage, so no asserted root changes.
    Meta::new(&db).set_chain_id(1);
    let (proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    PerpMarkets::new(&db).put(&perp_market(proposer));

    // height = 5 -> boundary. Tick present.
    let tick = sign_tx(
        &proposer_sk,
        0,
        TxBody::FundingTick {
            market: DerivId::perp(PERP_INDEX),
        },
    );
    let res = enforce_required_funding_ticks(&db, 5, proposer, &[tick]);
    assert!(res.is_ok());
}

#[test]
fn enforce_helper_rejects_when_tick_missing_at_boundary() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    PerpMarkets::new(&db).put(&perp_market(proposer));

    // height = 5 -> boundary. Tick absent: a no-op transfer is the only tx.
    let unrelated = sign_tx(
        &proposer_sk,
        0,
        TxBody::Transfer {
            to: Address([1; 20]),
            asset: AssetId::Usd,
            amount: 1,
        },
    );
    let err = enforce_required_funding_ticks(&db, 5, proposer, &[unrelated]).unwrap_err();
    assert_eq!(
        err,
        ExecError::MissingRequiredFundingTick {
            height: 5,
            market: DerivId::perp(PERP_INDEX)
        }
    );
}

#[test]
fn enforce_helper_passes_at_non_boundary_heights() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (_, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    PerpMarkets::new(&db).put(&perp_market(proposer));

    // height = 4 -> not a boundary. No tick required.
    let res = enforce_required_funding_ticks(&db, 4, proposer, &[]);
    assert!(res.is_ok());
}

#[test]
fn enforce_helper_rejects_tick_signed_by_non_proposer() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (_proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    let (attacker_sk, _) = generate_keypair();
    PerpMarkets::new(&db).put(&perp_market(proposer));

    // The required-set check counts only ticks signed by the proposer; an
    // attacker-signed tick is treated as non-existent and the block is
    // rejected as missing-required.
    let tick = sign_tx(
        &attacker_sk,
        0,
        TxBody::FundingTick {
            market: DerivId::perp(PERP_INDEX),
        },
    );
    let err = enforce_required_funding_ticks(&db, 5, proposer, &[tick]).unwrap_err();
    assert!(matches!(err, ExecError::MissingRequiredFundingTick { .. }));
}

#[test]
fn build_and_apply_rejects_block_at_boundary_missing_funding_tick() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    PerpMarkets::new(&db).put(&perp_market(proposer));
    Meta::new(&db).set_proposer_addr(proposer);

    // Walk the head height up to 4 by issuing dummy transfer blocks. Then the
    // next block lands at height 5 -> a funding boundary -> requires FundingTick.
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    {
        // pre-fund Alice so transfers work.
        let mut s = cb_state::AccountState::default();
        s.add(AssetId::Usd, 1_000_000).expect("seed balance");
        cb_state::Accounts::new(&db).put(&alice, &s);
    }

    let sign_block = |preimage: &[u8]| -> Vec<u8> {
        let digest = keccak256(preimage);
        let sig = crypto_sign(&proposer_sk, digest).unwrap();
        sig.to_vec()
    };

    let ctx = ExecCtx {
        treasury_signer: Address::ZERO,
        ..Default::default()
    };

    for n in 0..4u64 {
        let v = sign_tx(
            &alice_sk,
            n,
            TxBody::Transfer {
                to: Address([2; 20]),
                asset: AssetId::Usd,
                amount: 1,
            },
        );
        build_and_apply(&db, &ctx, vec![v], 1, 0, Vec::new(), sign_block).unwrap();
    }
    assert_eq!(Meta::new(&db).head_height(), 4);

    // Now try to land block 5 with a single Transfer (no FundingTick).
    let v = sign_tx(
        &alice_sk,
        4,
        TxBody::Transfer {
            to: Address([2; 20]),
            asset: AssetId::Usd,
            amount: 1,
        },
    );
    let err = match build_and_apply(&db, &ctx, vec![v], 1, 0, Vec::new(), sign_block) {
        Err(e) => e,
        Ok(_) => panic!("expected error, got success"),
    };
    assert!(
        matches!(err, ExecError::MissingRequiredFundingTick { height: 5, .. }),
        "got {err:?}"
    );

    // Including a FundingTick from the proposer next to the transfer makes
    // the block valid.
    let v_xfer = sign_tx(
        &alice_sk,
        4,
        TxBody::Transfer {
            to: Address([2; 20]),
            asset: AssetId::Usd,
            amount: 1,
        },
    );
    let v_tick = sign_tx(
        &proposer_sk,
        0,
        TxBody::FundingTick {
            market: DerivId::perp(PERP_INDEX),
        },
    );
    build_and_apply(
        &db,
        &ctx,
        vec![v_xfer, v_tick],
        1,
        0,
        Vec::new(),
        sign_block,
    )
    .unwrap();
    assert_eq!(Meta::new(&db).head_height(), 5);
}
