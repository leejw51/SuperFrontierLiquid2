//! Core review C1 and M3: the sequencer must produce the system transactions
//! the executor demands, and the leader must not count votes for blocks it
//! never produced.
//!
//! Before C1 was fixed, nothing on a live node built a `FundingTick`. The
//! executor requires one at every funding boundary and refuses the block
//! without it, so every chain started from the repository's genesis stopped
//! at height 3,600 — re-draining and dropping the mempool once per interval,
//! for ever. The runbook documented the symptom.

use cb_block::{Block, ExecCtx};
use cb_consensus::{vote_signing_bytes, Vote};
use cb_crypto::{address_from_pubkey, generate_keypair, sign};
use cb_dashboard::Metrics;
use cb_node::sequencer::SequencerVoteHandler;
use cb_rpc::{VoteCode, VoteHandler};
use cb_state::{AccountState, Accounts, Blocks, Database, Mempool, Meta, PerpMarkets, Validators};
use cb_tx::{SignedTx, Transaction, TxBody};
use cb_types::{Address, AssetId, DerivId, PerpMarket, H256};
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;
use tokio::sync::Notify;
use tokio::task::LocalSet;

const FUNDING_INTERVAL: u32 = 2;

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
        funding_interval_blocks: FUNDING_INTERVAL,
        funding_rate_cap_q9: 5_000_000,
        oracle_authority: authority,
        max_position_notional: 100_000_000_000_000,
        paused: false,
        last_trade_px_q8: 0,
        last_trade_height: 0,
        paused_reduce_only_only: false,
    }
}

fn enqueue_transfer(db: &Database, sk: &secp256k1::SecretKey, nonce: u64) {
    let tx = Transaction {
        nonce,
        chain_id: 1,
        body: TxBody::Transfer {
            to: Address([2; 20]),
            asset: AssetId::Usd,
            amount: 1,
        },
    };
    let signed = SignedTx::sign_with(sk, tx).unwrap();
    let sender = signed.sender().unwrap();
    let record = cb_tx::MempoolRecord {
        signed: signed.clone(),
        sender,
    };
    Mempool::new(db).put(
        signed.hash(),
        sender,
        signed.tx.nonce,
        &bincode::serialize(&record).unwrap(),
    );
}

#[test]
fn the_sequencer_injects_the_funding_tick_at_a_boundary() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);

    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    {
        let mut s = AccountState::default();
        s.add(AssetId::Usd, 1_000).unwrap();
        Accounts::new(&db).put(&alice, &s);
    }
    let (proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    Meta::new(&db).set_proposer_addr(proposer);
    PerpMarkets::new(&db).put(&perp_market(Address([0xa0; 20])));

    // Block 1 is not a boundary; block 2 is. One user transaction per block,
    // so the second block exists at all — the chain only builds when there
    // is something to build.
    enqueue_transfer(&db, &alice_sk, 0);

    let db_clone = db.clone();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = LocalSet::new();
    runtime.block_on(local.run_until(async move {
        let notify = Arc::new(Notify::new());
        let metrics = Arc::new(Metrics::new());
        let task = tokio::task::spawn_local(cb_node::sequencer::run_sequencer(
            db_clone.clone(),
            ExecCtx {
                treasury_signer: Address::ZERO,
                ..Default::default()
            },
            notify,
            50,
            10_000,
            metrics,
            None,
            cb_node::sequencer::PruningCfg::disabled(),
            Arc::new(proposer_sk),
            None,
            None,
            None,
        ));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while Meta::new(&db_clone).head_height() < 1 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(Meta::new(&db_clone).head_height(), 1, "block 1 produced");
        enqueue_transfer(&db_clone, &alice_sk, 1);
        while Meta::new(&db_clone).head_height() < 2 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        task.abort();
        let _ = task.await;
    }));

    assert_eq!(
        Meta::new(&db).head_height(),
        2,
        "the boundary block was produced instead of halting the chain"
    );
    let block = Block::decode(&Blocks::new(&db).get(2).expect("block 2 stored")).unwrap();
    let ticks: Vec<(Address, &TxBody)> = block
        .txs
        .iter()
        .zip(block.senders.iter())
        .filter(|(t, _)| matches!(t.tx.body, TxBody::FundingTick { .. }))
        .map(|(t, s)| (*s, &t.tx.body))
        .collect();
    assert_eq!(ticks.len(), 1, "exactly one tick for the one perp market");
    assert_eq!(ticks[0].0, proposer, "signed by the proposer");
    assert_eq!(
        *ticks[0].1,
        TxBody::FundingTick {
            market: DerivId::perp(0)
        }
    );
    assert_eq!(
        Accounts::new(&db).get(&proposer).map(|a| a.nonce),
        Some(1),
        "the proposer's account consumed one nonce for the tick"
    );
    assert!(
        cb_state::FundingIndices::new(&db)
            .get(&DerivId::perp(0))
            .is_some_and(|i| i.last_tick_height == 2),
        "the tick executed"
    );
}

#[test]
fn a_vote_for_a_hash_the_leader_never_produced_is_refused() {
    // Core review M3. The handler rejected votes above the head and nothing
    // else, so a vote naming a hash this node never produced at a real
    // height was accepted into a bucket — a certificate waiting for one more
    // signer. `VoteRejection::UnknownBlock` existed and was never returned.
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    let (sk, pk) = generate_keypair();
    let validator = address_from_pubkey(&pk);
    Validators::new(&db).put_set(&[cb_state::ValidatorInfo {
        addr: validator,
        pubkey: pk.serialize().to_vec(),
        weight: 1,
    }]);
    // A real block 1 exists.
    let real = H256([1; 32]);
    Blocks::new(&db).put(1, real, b"block one");
    Meta::new(&db).set_head(1, real, H256::ZERO);

    let handler = SequencerVoteHandler {
        db: db.clone(),
        pool: cb_node::sequencer::new_vote_pool(),
    };
    let vote_for = |hash: H256| {
        let preimage = H256(vote_signing_bytes(0, 1, hash));
        Vote {
            view: 0,
            height: 1,
            block_hash: hash,
            validator,
            sig: sign(&sk, preimage).unwrap().to_vec(),
        }
    };

    let phantom = H256([0xee; 32]);
    let err = handler.handle(vote_for(phantom)).unwrap_err();
    assert_eq!(err.0, VoteCode::UnknownBlock, "{}", err.1);

    // The real hash at the real height is counted.
    handler.handle(vote_for(real)).unwrap();
}
