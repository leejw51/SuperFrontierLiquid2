use cb_block::{build_and_apply, ExecCtx};
use cb_crypto::{address_from_pubkey, generate_keypair, keccak256, sign};
use cb_dashboard::Metrics;
use cb_net::apply_remote_block;
use cb_state::{AccountState, Accounts, Database, Meta};
use cb_tx::{SignedTx, Transaction, TxBody};
use cb_types::{Address, AssetId};
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;
use tokio::sync::Notify;
use tokio::task::LocalSet;

/// 1 sequencer + 2 replicas should converge on the same state root after each block.
#[test]
fn three_nodes_converge_on_state_root() {
    let seq_dir = tempdir().unwrap();
    let r1_dir = tempdir().unwrap();
    let r2_dir = tempdir().unwrap();
    let seq = Database::open(seq_dir.path()).unwrap();
    // Transactions here are signed for chain 1; the executor rejects a tx whose
    // `chain_id` does not match the chain's own, so the fixture must declare it.
    // `Meta` is outside the state-root preimage, so no asserted root changes.
    Meta::new(&seq).set_chain_id(1);
    let r1 = Database::open(r1_dir.path()).unwrap();
    Meta::new(&r1).set_chain_id(1);
    let r2 = Database::open(r2_dir.path()).unwrap();
    Meta::new(&r2).set_chain_id(1);

    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    let bob = Address([2; 20]);
    for db in [&seq, &r1, &r2] {
        let accts = Accounts::new(db);
        let mut s = AccountState::default();
        s.add(AssetId::Usd, 10_000).expect("seed balance");
        accts.put(&alice, &s);
    }

    // Set up a deterministic proposer key all three nodes agree on.
    let (proposer_sk, proposer_pk) = generate_keypair();
    let proposer_addr = address_from_pubkey(&proposer_pk);
    for db in [&seq, &r1, &r2] {
        Meta::new(db).set_proposer_addr(proposer_addr);
    }

    let ctx = ExecCtx {
        treasury_signer: Address::ZERO,
        ..Default::default()
    };
    for i in 0..5u64 {
        let tx = Transaction {
            nonce: i,
            chain_id: 1,
            body: TxBody::Transfer {
                to: bob,
                asset: AssetId::Usd,
                amount: 1,
            },
        };
        let signed = SignedTx::sign_with(&sk, tx).unwrap();
        let verified = signed.verify().unwrap();
        let signer = |preimage: &[u8]| sign(&proposer_sk, keccak256(preimage)).unwrap().to_vec();
        let res = build_and_apply(&seq, &ctx, vec![verified], i, 0, Vec::new(), signer).unwrap();
        apply_remote_block(&r1, &ctx, &res.block).unwrap();
        apply_remote_block(&r2, &ctx, &res.block).unwrap();
        assert_eq!(Meta::new(&seq).state_root(), Meta::new(&r1).state_root());
        assert_eq!(Meta::new(&seq).state_root(), Meta::new(&r2).state_root());
        assert_eq!(Meta::new(&r1).head_height(), i + 1);
    }
}

/// With an empty mempool the sequencer must NOT advance head height (no-empty-block rule).
#[test]
fn no_empty_block_invariant() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let h_before = Meta::new(&db).head_height();
    assert_eq!(h_before, 0);

    let (proposer_sk, _) = generate_keypair();
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
            db_clone,
            ExecCtx {
                treasury_signer: Address::ZERO,
                ..Default::default()
            },
            notify,
            50, // 50 ms interval for the test
            10_000,
            metrics,
            None,
            cb_node::sequencer::PruningCfg::disabled(),
            Arc::new(proposer_sk),
            None, // no vote pool in this test
            None, // no mempool nonce gate in this test
            None, // no refused-transaction log in this test
        ));
        tokio::time::sleep(Duration::from_millis(250)).await;
        task.abort();
        // Swallow JoinError from abort; what matters is the head_height assertion.
        let _ = task.await;
    }));

    assert_eq!(
        Meta::new(&db).head_height(),
        h_before,
        "no blocks should be produced when mempool is empty"
    );
}

/// Once a tx is in the mempool, the sequencer produces a block within ~one interval.
#[test]
fn sequencer_produces_block_when_tx_arrives() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);

    // Pre-fund alice so the tx is acceptable.
    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    let bob = Address([2; 20]);
    {
        let accts = Accounts::new(&db);
        let mut s = AccountState::default();
        s.add(AssetId::Usd, 1_000).expect("seed balance");
        accts.put(&alice, &s);
    }

    // Configure the proposer key + advertised address.
    let (proposer_sk, proposer_pk) = generate_keypair();
    Meta::new(&db).set_proposer_addr(address_from_pubkey(&proposer_pk));

    // Build and persist a signed transfer in the mempool.
    let tx = Transaction {
        nonce: 0,
        chain_id: 1,
        body: TxBody::Transfer {
            to: bob,
            asset: AssetId::Usd,
            amount: 100,
        },
    };
    let signed = SignedTx::sign_with(&sk, tx).unwrap();
    let sender = signed.sender().unwrap();
    let record = cb_tx::MempoolRecord {
        signed: signed.clone(),
        sender,
    };
    let bytes = bincode::serialize(&record).unwrap();
    cb_state::Mempool::new(&db).put(signed.hash(), sender, signed.tx.nonce, &bytes);

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
            db_clone,
            ExecCtx {
                treasury_signer: Address::ZERO,
                ..Default::default()
            },
            notify,
            50, // 50 ms interval for the test
            10_000,
            metrics,
            None,
            cb_node::sequencer::PruningCfg::disabled(),
            Arc::new(proposer_sk),
            None, // no vote pool in this test
            None, // no mempool nonce gate in this test
            None, // no refused-transaction log in this test
        ));
        // Give the loop ample time to drain the mempool and produce a block.
        tokio::time::sleep(Duration::from_millis(500)).await;
        task.abort();
        // Swallow JoinError from abort; what matters is the head_height assertion.
        let _ = task.await;
    }));

    let m = Meta::new(&db);
    assert_eq!(m.head_height(), 1, "expected one block produced");
    assert!(
        cb_state::Mempool::new(&db).is_empty(),
        "mempool should be drained"
    );
}
