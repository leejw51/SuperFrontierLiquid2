//! Gossip against a real database.
//!
//! The protocol tests in `p2p_protocol.rs` use a toy chain so they can assert
//! on propagation and scoring without paying for execution. This file does the
//! opposite: a small network, real RocksDB-backed nodes, real blocks, and the
//! real `apply_remote_block`. It exists to check the one claim that a toy
//! chain cannot check — that a node which catches up over gossip ends at
//! byte-identical state, and that it gets there by re-executing rather than by
//! trusting whoever handed it the bytes.

use cb_block::{build_and_apply, Block, ExecCtx};
use cb_crypto::{address_from_pubkey, generate_keypair, keccak256, sign, SecretKey};
use cb_net::p2p::chain::{ChainAccess, DbChain};
use cb_net::p2p::config::P2pConfig;
use cb_net::p2p::engine::RejectReason;
use cb_net::p2p::identity::GenesisId;
use cb_net::p2p::sim::{local_node, sim_addr, SimNetwork};
use cb_state::{AccountState, Accounts, Database, Meta};
use cb_tx::{SignedTx, Transaction, TxBody};
use cb_types::{Address, AssetId, H256};
use std::time::Duration;
use tempfile::TempDir;

const CHAIN_ID: u32 = 1;

/// A node's storage plus the identities the fixture needs to drive it.
struct Fixture {
    _dir: TempDir,
    db: Database,
}

fn open_node(proposer: Address, alice: Address) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Database::open(dir.path()).expect("open db");
    // The executor rejects a transaction whose chain id does not match the
    // chain's own, so the fixture has to declare one — and it is the same
    // value the handshake compares, by construction.
    Meta::new(&db).set_chain_id(CHAIN_ID);
    Meta::new(&db).set_proposer_addr(proposer);
    let mut s = AccountState::default();
    s.add(AssetId::Usd, 1_000_000).expect("seed balance");
    Accounts::new(&db).put(&alice, &s);
    Fixture { _dir: dir, db }
}

fn transfer(sk: &SecretKey, nonce: u64, to: Address, amount: u128) -> cb_tx::VerifiedTx {
    let tx = Transaction {
        nonce,
        chain_id: CHAIN_ID,
        body: TxBody::Transfer {
            to,
            asset: AssetId::Usd,
            amount,
        },
    };
    SignedTx::sign_with(sk, tx)
        .expect("sign")
        .verify()
        .expect("verify")
}

fn ctx() -> ExecCtx {
    ExecCtx {
        treasury_signer: Address::ZERO,
        ..Default::default()
    }
}

/// Produce `n` blocks on `db`, returning the head hash and height.
fn produce(db: &Database, proposer_sk: &SecretKey, alice_sk: &SecretKey, n: u64) -> (H256, u64) {
    let mut last = (H256::ZERO, 0u64);
    for i in 0..n {
        let v = transfer(alice_sk, i, Address([9; 20]), 10);
        let signer = |preimage: &[u8]| {
            sign(proposer_sk, keccak256(preimage))
                .expect("sign header")
                .to_vec()
        };
        let res = build_and_apply(db, &ctx(), vec![v], 1_000 + i, 0, Vec::new(), signer)
            .expect("build block");
        last = (res.block.header.hash(), res.block.header.height);
    }
    last
}

fn test_cfg(index: u16, seeds: Vec<std::net::SocketAddr>) -> P2pConfig {
    P2pConfig {
        listen: Some(sim_addr(index)),
        seeds,
        max_peers: 8,
        target_outbound: 4,
        request_timeout: Duration::from_secs(2),
        discovery_interval: Duration::from_secs(1),
        ping_interval: Duration::from_secs(1),
        peer_timeout: Duration::from_secs(3_600),
        ..P2pConfig::default()
    }
}

#[test]
fn a_straggler_catches_up_over_gossip_and_reproduces_the_state_root() {
    let (proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);

    let leader = open_node(proposer, alice);
    let relay = open_node(proposer, alice);
    let straggler = open_node(proposer, alice);

    let (head_hash, head_height) = produce(&leader.db, &proposer_sk, &alice_sk, 6);
    let leader_root = Meta::new(&leader.db).state_root();
    assert_eq!(head_height, 6);

    let genesis = GenesisId::from_db(&leader.db);
    assert_eq!(
        genesis,
        GenesisId::from_db(&relay.db),
        "identically configured nodes must agree on the genesis fingerprint"
    );

    let mut net: SimNetwork<DbChain> = SimNetwork::new();
    net.add(
        test_cfg(0, vec![]),
        local_node("leader", CHAIN_ID, genesis),
        DbChain::new(leader.db.clone(), ctx(), None),
    );
    net.add(
        test_cfg(1, vec![sim_addr(0)]),
        local_node("relay", CHAIN_ID, genesis),
        DbChain::new(relay.db.clone(), ctx(), None),
    );
    net.add(
        test_cfg(2, vec![sim_addr(0), sim_addr(1)]),
        local_node("straggler", CHAIN_ID, genesis),
        DbChain::new(straggler.db.clone(), ctx(), None),
    );

    // The straggler is offline from the start: its dials go nowhere.
    net.partition(&[0, 0, 1]);
    net.bootstrap();

    net.announce_block(0, head_hash, head_height);
    for _ in 0..8 {
        net.tick(1_100);
    }

    assert_eq!(Meta::new(&relay.db).head_height(), 6);
    assert_eq!(
        Meta::new(&relay.db).state_root(),
        leader_root,
        "the relay must reproduce the leader's state root, not copy it"
    );
    assert_eq!(Meta::new(&straggler.db).head_height(), 0);

    // It comes back into a pocket that does not contain the leader. The only
    // available source is the relay, which is exactly the case the star
    // topology could not serve.
    net.partition(&[1, 0, 0]);
    // A node coming back up dials its seeds again.
    net.bootstrap();
    for _ in 0..12 {
        net.tick(1_100);
    }

    assert_eq!(
        Meta::new(&straggler.db).head_height(),
        6,
        "the straggler did not catch up from a non-leader peer"
    );
    assert_eq!(
        Meta::new(&straggler.db).state_root(),
        leader_root,
        "catching up from a peer must land on the identical state root"
    );
    for h in 1..=6u64 {
        assert_eq!(
            cb_state::Blocks::new(&straggler.db).get(h),
            cb_state::Blocks::new(&leader.db).get(h),
            "block {h} differs from the leader's"
        );
    }
}

#[test]
fn a_block_with_a_tampered_state_root_is_refused_and_changes_nothing() {
    let (proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);

    let leader = open_node(proposer, alice);
    let follower = open_node(proposer, alice);
    produce(&leader.db, &proposer_sk, &alice_sk, 1);

    let bytes = cb_state::Blocks::new(&leader.db).get(1).expect("block 1");
    let mut block = Block::decode(&bytes).expect("decode");
    block.header.state_root = H256([0xAB; 32]);

    let before_root = Meta::new(&follower.db).state_root();
    let mut chain = DbChain::new(follower.db.clone(), ctx(), None);
    let err = chain
        .verify_and_apply(&block.encode())
        .expect_err("a tampered state root must be refused");

    // The tamper also breaks the proposer signature (the root is inside the
    // signed preimage), so the node refuses it at the earlier of the two
    // checks. Either verdict is a refusal, and neither is an adoption — which
    // is the property under test.
    assert!(
        matches!(
            err,
            RejectReason::StateRootMismatch | RejectReason::BadProposerSignature
        ),
        "unexpected rejection: {err:?}"
    );
    assert_eq!(Meta::new(&follower.db).head_height(), 0);
    assert_eq!(Meta::new(&follower.db).state_root(), before_root);
}

#[test]
fn an_order_submitted_to_a_replica_reaches_the_sequencer() {
    // The end the star topology could not serve: a client submits to whichever
    // node is closest, and the transaction finds the block producer by gossip.
    let (proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);

    let sequencer = open_node(proposer, alice);
    let replica = open_node(proposer, alice);
    // A block or two so the two nodes are not both at genesis, which is the
    // uninteresting case.
    produce(&sequencer.db, &proposer_sk, &alice_sk, 2);
    let genesis = GenesisId::from_db(&sequencer.db);

    let mut net: SimNetwork<DbChain> = SimNetwork::new();
    net.add(
        test_cfg(0, vec![]),
        local_node("sequencer", CHAIN_ID, genesis),
        DbChain::new(sequencer.db.clone(), ctx(), None),
    );
    let relay = net.add(
        test_cfg(1, vec![sim_addr(0)]),
        local_node("replica", CHAIN_ID, genesis),
        DbChain::new(replica.db.clone(), ctx(), None),
    );
    net.bootstrap();

    let signed = SignedTx::sign_with(
        &alice_sk,
        Transaction {
            nonce: 2,
            chain_id: CHAIN_ID,
            body: TxBody::Transfer {
                to: Address([7; 20]),
                asset: AssetId::Usd,
                amount: 25,
            },
        },
    )
    .expect("sign");
    let bytes = bincode::serialize(&signed).expect("encode");

    assert!(cb_state::Mempool::new(&sequencer.db).is_empty());
    // This is what the replica's RPC ingress does on a submitTx.
    let hash = net
        .node_mut(relay)
        .chain
        .accept_tx(&bytes)
        .expect("replica accepts the order");
    net.announce_tx(relay, hash);

    assert!(
        !cb_state::Mempool::new(&sequencer.db).is_empty(),
        "the order never reached the sequencer's mempool"
    );
}

#[test]
fn a_gossiped_transaction_lands_in_the_mempool_of_the_node_that_receives_it() {
    let (_, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let node = open_node(proposer, alice);

    let signed = SignedTx::sign_with(
        &alice_sk,
        Transaction {
            nonce: 0,
            chain_id: CHAIN_ID,
            body: TxBody::Transfer {
                to: Address([4; 20]),
                asset: AssetId::Usd,
                amount: 5,
            },
        },
    )
    .expect("sign");
    let bytes = bincode::serialize(&signed).expect("encode");

    let mut chain = DbChain::new(node.db.clone(), ctx(), None);
    assert!(cb_state::Mempool::new(&node.db).is_empty());
    let hash = chain.accept_tx(&bytes).expect("accept");
    assert_eq!(hash, signed.hash());
    assert!(!cb_state::Mempool::new(&node.db).is_empty());
    // And the node can now serve that transaction to a peer that asks.
    assert_eq!(chain.tx_bytes(&hash).as_deref(), Some(bytes.as_slice()));

    // Garbage is refused rather than stored.
    assert!(matches!(
        chain.accept_tx(b"not a transaction"),
        Err(RejectReason::Undecodable) | Err(RejectReason::InvalidTransaction)
    ));
}
