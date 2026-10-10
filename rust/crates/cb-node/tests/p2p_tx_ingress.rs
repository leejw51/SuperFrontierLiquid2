//! End-to-end: an order submitted to a node that is not the sequencer reaches
//! the sequencer.
//!
//! This is the one path the unit tests cannot reach on their own, because the
//! bug it guards against lives in the seam between three crates. The RPC server
//! accepts a transaction, hands it to the `TxObserver` the node installed, the
//! peer-to-peer handle announces it, a real TCP peer asks for the body, and the
//! sequencer's `DbChain` validates it into its mempool.
//!
//! The wiring under test is exactly what `cb-node`'s replica mode builds: a
//! `SequencerServer` whose `tx_observer` is the `P2pHandle`. An earlier version
//! announced the hash without the body, which every in-crate test passed and
//! which did not work at all in a real node — the announcement was unanswerable
//! and every peer scored the submitting node down for the timeout.

use cb_block::ExecCtx;
use cb_crypto::{address_from_pubkey, generate_keypair};
use cb_net::p2p::chain::DbChain;
use cb_net::p2p::config::P2pConfig;
use cb_net::p2p::engine::LocalNode;
use cb_net::p2p::identity::GenesisId;
use cb_net::p2p::transport::spawn;
use cb_schema::rpc_capnp;
use cb_state::{AccountState, Accounts, Database, Mempool, Meta};
use cb_tx::{SignedTx, Transaction, TxBody};
use cb_types::{Address, AssetId};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::sync::Notify;
use tokio::task::LocalSet;

const CHAIN_ID: u32 = 1;

fn open_node(alice: Address) -> (TempDir, Database) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Database::open(dir.path()).expect("open db");
    Meta::new(&db).set_chain_id(CHAIN_ID);
    let mut s = AccountState::default();
    s.add(AssetId::Usd, 1_000_000).expect("seed balance");
    Accounts::new(&db).put(&alice, &s);
    (dir, db)
}

fn p2p_cfg(seeds: Vec<std::net::SocketAddr>) -> P2pConfig {
    P2pConfig {
        listen: Some("127.0.0.1:0".parse().expect("addr")),
        seeds,
        max_peers: 4,
        target_outbound: 2,
        request_timeout: Duration::from_secs(2),
        discovery_interval: Duration::from_secs(1),
        ping_interval: Duration::from_secs(1),
        peer_timeout: Duration::from_secs(60),
        ..P2pConfig::default()
    }
}

fn local(seed: &str, genesis: GenesisId) -> LocalNode {
    let (sk, _) = cb_crypto::keypair_from_seed(seed.as_bytes());
    LocalNode::new(sk, CHAIN_ID, genesis, "cb-node/test".into())
}

#[test]
fn an_order_submitted_to_a_replica_reaches_the_sequencers_mempool() {
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let (_seq_dir, seq_db) = open_node(alice);
    let (_rep_dir, rep_db) = open_node(alice);
    let genesis = GenesisId::from_db(&seq_db);

    let signed = SignedTx::sign_with(
        &alice_sk,
        Transaction {
            nonce: 0,
            chain_id: CHAIN_ID,
            body: TxBody::Transfer {
                to: Address([0xbb; 20]),
                asset: AssetId::Usd,
                amount: 1_234,
            },
        },
    )
    .expect("sign");
    let expected_hash = signed.hash();

    let seq_db_check = seq_db.clone();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let local_set = LocalSet::new();

    rt.block_on(local_set.run_until(async move {
        let ctx = ExecCtx::default();

        // The sequencer: peer-to-peer only, no seeds; the replica dials it.
        let seq_notify = Arc::new(Notify::new());
        let seq_chain = Arc::new(Mutex::new(DbChain::new(
            seq_db.clone(),
            ctx,
            Some(seq_notify.clone()),
        )));
        let seq_p2p = spawn(p2p_cfg(vec![]), local("seq", genesis), seq_chain)
            .await
            .expect("spawn sequencer p2p");
        let seq_addr = seq_p2p.listen_addr().expect("bound");

        // The replica: peer-to-peer plus an RPC door for clients, wired the
        // same way `cb-node replica --rpc-listen --p2p-*` wires it.
        let rep_notify = Arc::new(Notify::new());
        let rep_chain = Arc::new(Mutex::new(DbChain::new(
            rep_db.clone(),
            ctx,
            Some(rep_notify.clone()),
        )));
        let rep_p2p = spawn(p2p_cfg(vec![seq_addr]), local("rep", genesis), rep_chain)
            .await
            .expect("spawn replica p2p");

        let observer: cb_rpc::TxObserver = {
            let h = rep_p2p.clone();
            Arc::new(move |hash, bytes| h.announce_tx(hash, bytes))
        };
        let mut server = cb_rpc::SequencerServer::without_voting(rep_db.clone(), rep_notify);
        server.tx_observer = Some(observer);
        let cap: rpc_capnp::super_frontier_liquid::Client = capnp_rpc::new_client(server);

        // Wait for the peers to find each other before submitting, so the
        // announcement has somewhere to go.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            assert!(Instant::now() < deadline, "peers never handshaked");
            if rep_p2p.stats().await.map(|s| s.connected_peers) == Some(1) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        // Submit to the replica, exactly as a client would.
        let mut req = cap.submit_tx_request();
        cb_rpc::conv::signed_tx_to_capnp(req.get().init_tx(), &signed);
        let resp = req.send().promise.await.expect("submit");
        let result = resp.get().expect("result").get_result().expect("result");
        assert!(
            matches!(
                result.which().expect("which"),
                rpc_capnp::submit_result::Ok(())
            ),
            "the replica refused the order"
        );

        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if !Mempool::new(&seq_db_check).is_empty() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the order never reached the sequencer's mempool"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        // And it is the order that was submitted, not merely something.
        let drained = Mempool::new(&seq_db_check).drain(10);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].0, expected_hash);

        seq_p2p.shutdown();
        rep_p2p.shutdown();
    }));
}
