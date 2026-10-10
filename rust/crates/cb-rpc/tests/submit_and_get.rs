//! End-to-end RPC test for `submitTx` and `getAccount`.
//!
//! Exercises the `SequencerServer` capability *in-process* via
//! `capnp_rpc::new_client` — no TCP, no network, no LocalSet over a real
//! sequencer task. We:
//!
//!   1. fund alice in the underlying RocksDB,
//!   2. build a signed Transfer tx,
//!   3. call `submit_tx` over the capnp capability — this is the same code
//!      path the real network server runs, so we exercise capnp encode +
//!      decode, sender recovery, mempool persistence, and the tx-validation
//!      branches inside `submitTx`,
//!   4. assert the tx landed in the mempool with its sender pre-recovered
//!      (so the block builder can skip ECDSA on the hot path),
//!   5. call `get_account(alice)` over the same capability and assert the
//!      returned state matches what the executor (would) update.
//!
//! Notes:
//! - The server doesn't apply the tx — that's the sequencer task's job. We
//!   verify the *RPC plumbing* end-to-end: bytes in, mempool record out,
//!   account read-through working. Block-building is already covered by
//!   `crates/cb-node/tests/e2e_inprocess.rs`.
//! - Tests run inside a `current_thread` runtime + `LocalSet` because capnp
//!   futures are `!Send`.

use cb_crypto::{address_from_pubkey, generate_keypair};
use cb_rpc::{conv, server::SequencerServer};
use cb_schema::rpc_capnp;
use cb_state::{AccountState, Accounts, Database, Mempool, Meta};
use cb_tx::{MempoolRecord, SignedTx, Transaction, TxBody};
use cb_types::{Address, AssetId};
use std::sync::Arc;
use tempfile::tempdir;
use tokio::sync::Notify;
use tokio::task::LocalSet;

fn run_local<F, Fut, R>(f: F) -> R
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = R>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = LocalSet::new();
    rt.block_on(local.run_until(f()))
}

fn make_server(db: Database) -> (rpc_capnp::super_frontier_liquid::Client, Arc<Notify>) {
    let notify = Arc::new(Notify::new());
    let server = SequencerServer::without_voting(db, notify.clone());
    let cap: rpc_capnp::super_frontier_liquid::Client = capnp_rpc::new_client(server);
    (cap, notify)
}

#[test]
fn submit_tx_persists_mempool_record_with_recovered_sender() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    // Transactions here are signed for chain 1; the executor rejects a tx whose
    // `chain_id` does not match the chain's own, so the fixture must declare it.
    // `Meta` is outside the state-root preimage, so no asserted root changes.
    Meta::new(&db).set_chain_id(1);

    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    let bob = Address([0xbb; 20]);
    {
        let accts = Accounts::new(&db);
        let mut s = AccountState::default();
        s.add(AssetId::Usd, 5_000).expect("seed balance");
        accts.put(&alice, &s);
    }

    let tx = Transaction {
        nonce: 0,
        chain_id: 1,
        body: TxBody::Transfer {
            to: bob,
            asset: AssetId::Usd,
            amount: 1_234,
        },
    };
    let signed = SignedTx::sign_with(&sk, tx).unwrap();
    let signed_for_check = signed.clone();
    let db_for_check = db.clone();

    run_local(|| async move {
        let (cap, _notify) = make_server(db);
        let mut req = cap.submit_tx_request();
        conv::signed_tx_to_capnp(req.get().init_tx(), &signed);
        let resp = req.send().promise.await.expect("submit_tx send");
        let result = resp.get().unwrap().get_result().unwrap();
        match result.which().unwrap() {
            rpc_capnp::submit_result::Ok(()) => {}
            rpc_capnp::submit_result::Rejected(r) => {
                let m = r.unwrap().get_message().unwrap().to_string().unwrap();
                panic!("expected Ok, got Rejected: {m}");
            }
        }
    });

    // Verify the mempool now contains a record for our tx hash, that the
    // record decodes back to a `MempoolRecord`, and that the cached sender
    // is alice (so the block builder won't re-run ECDSA recovery).
    let blob = Mempool::new(&db_for_check)
        .get(&signed_for_check.hash())
        .expect("mempool record exists");
    let rec: MempoolRecord = bincode::deserialize(&blob).expect("decode MempoolRecord");
    assert_eq!(rec.signed.hash(), signed_for_check.hash());
    assert_eq!(
        rec.sender, alice,
        "submitTx must cache the recovered sender"
    );
    assert!(
        !Mempool::new(&db_for_check).is_empty(),
        "mempool must be non-empty after a successful submitTx",
    );
}

#[test]
fn submit_tx_zero_amount_is_rejected_with_invalid_amount_code() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (sk, _) = generate_keypair();
    let tx = Transaction {
        nonce: 0,
        chain_id: 1,
        body: TxBody::Transfer {
            to: Address([2; 20]),
            asset: AssetId::Usd,
            amount: 0, // rejected by validate_static
        },
    };
    let signed = SignedTx::sign_with(&sk, tx).unwrap();
    let db_for_check = db.clone();

    run_local(|| async move {
        let (cap, _notify) = make_server(db);
        let mut req = cap.submit_tx_request();
        conv::signed_tx_to_capnp(req.get().init_tx(), &signed);
        let resp = req.send().promise.await.expect("submit_tx send");
        let result = resp.get().unwrap().get_result().unwrap();
        match result.which().unwrap() {
            rpc_capnp::submit_result::Rejected(r) => {
                let r = r.unwrap();
                assert_eq!(
                    r.get_code().unwrap(),
                    rpc_capnp::SubmitErrorCode::InvalidAmount
                );
            }
            rpc_capnp::submit_result::Ok(()) => panic!("expected Rejected for zero amount"),
        }
    });

    // Mempool must remain empty.
    assert!(Mempool::new(&db_for_check).is_empty());
}

#[test]
fn get_account_returns_funded_balance_and_default_for_unknown() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (_, pk) = generate_keypair();
    let known = address_from_pubkey(&pk);
    let unknown = Address([0xff; 20]);
    {
        let accts = Accounts::new(&db);
        let mut s = AccountState::default();
        s.add(AssetId::Usd, 7_777).expect("seed balance");
        s.nonce = 4;
        accts.put(&known, &s);
    }

    run_local(|| async move {
        let (cap, _notify) = make_server(db);

        // Known address.
        let mut req = cap.get_account_request();
        conv::address_to_capnp(req.get().init_addr(), &known);
        let resp = req.send().promise.await.expect("get_account known");
        let st = resp.get().unwrap().get_state().unwrap();
        assert_eq!(st.get_nonce(), 4);
        // Walk the balances list to find USD.
        let bals = st.get_balances().unwrap();
        let mut got_usd = 0u128;
        for i in 0..bals.len() {
            let e = bals.get(i);
            if e.get_asset().unwrap() as u8 == AssetId::Usd.as_u8() {
                got_usd = conv::u128_from_capnp(e.get_amount().unwrap());
            }
        }
        assert_eq!(got_usd, 7_777);

        // Unknown address — must return a default state (nonce 0, no balances).
        let mut req = cap.get_account_request();
        conv::address_to_capnp(req.get().init_addr(), &unknown);
        let resp = req.send().promise.await.expect("get_account unknown");
        let st = resp.get().unwrap().get_state().unwrap();
        assert_eq!(st.get_nonce(), 0);
        let bals = st.get_balances().unwrap();
        assert_eq!(bals.len(), 0);
    });
}

/// A tiny block body for the stream tests. What is inside does not matter to
/// the subscription server; that it decodes does.
fn store_block(db: &Database, height: u64) {
    use cb_block::{Block, BlockHeader};
    let block = Block {
        header: BlockHeader {
            height,
            prev_hash: cb_types::H256([0; 32]),
            tx_root: cb_types::H256([0; 32]),
            state_root: cb_types::H256([height as u8; 32]),
            timestamp: height,
            view: Default::default(),
            parent_qc: Vec::new(),
            sequencer_sig: Vec::new(),
        },
        txs: Vec::new(),
        senders: Vec::new(),
        applied: Vec::new(),
    };
    let bytes = block.encode();
    cb_state::Blocks::new(db).put(height, block.header.hash(), &bytes);
    Meta::new(db).set_head(height, block.header.hash(), block.header.state_root);
}

/// The failure this pins: a client asking to replay from a height whose body
/// has been pruned used to be handed a subscription that waited on a notify
/// that would never fire. The gateway sat connected and blockless for ever.
/// Now `chainInfo` says where the bodies begin, `streamBlocks` starts there,
/// and a body that vanishes mid-stream is an error rather than a hang.
#[test]
fn stream_blocks_from_a_pruned_height_starts_at_the_oldest_body_held() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    for h in 1..=5 {
        store_block(&db, h);
    }
    // Prune 1 and 2, the way the sequencer's keep-window does.
    for h in 1..=2 {
        let bytes = cb_state::Blocks::new(&db).get(h).unwrap();
        let block = cb_block::Block::decode(&bytes).unwrap();
        cb_state::Blocks::new(&db).prune_at(h, block.header.hash());
    }
    let (cap, _notify) = make_server(db);

    run_local(|| async move {
        let info = cap.chain_info_request().send().promise.await.unwrap();
        let info = info.get().unwrap().get_info().unwrap();
        assert_eq!(info.get_head_height(), 5);
        assert_eq!(
            info.get_oldest_block(),
            3,
            "chainInfo names the oldest body held"
        );

        let mut request = cap.stream_blocks_request();
        request.get().set_from_height(1);
        let response = request.send().promise.await.unwrap();
        let sub = response.get().unwrap().get_sub().unwrap();

        let first = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            sub.next_request().send().promise,
        )
        .await
        .expect("the stream answers instead of waiting for a pruned body")
        .unwrap();
        let height = first
            .get()
            .unwrap()
            .get_block()
            .unwrap()
            .get_header()
            .unwrap()
            .get_height();
        assert_eq!(
            height, 3,
            "the stream starts at the oldest body, not at the request"
        );
    });
}

#[test]
fn a_body_pruned_mid_stream_is_an_error_not_a_hang() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    for h in 1..=3 {
        store_block(&db, h);
    }
    let prune_db = db.clone();
    let (cap, _notify) = make_server(db);

    run_local(|| async move {
        let mut request = cap.stream_blocks_request();
        request.get().set_from_height(1);
        let response = request.send().promise.await.unwrap();
        let sub = response.get().unwrap().get_sub().unwrap();
        let _ = sub.next_request().send().promise.await.unwrap();

        // Block 2 disappears before it is asked for.
        let bytes = cb_state::Blocks::new(&prune_db).get(2).unwrap();
        let block = cb_block::Block::decode(&bytes).unwrap();
        cb_state::Blocks::new(&prune_db).prune_at(2, block.header.hash());

        let next = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            sub.next_request().send().promise,
        )
        .await
        .expect("a pruned body fails the request rather than parking it");
        let err = match next {
            Ok(_) => panic!("a missing body must be an error"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("pruned"), "{err}");
    });
}
