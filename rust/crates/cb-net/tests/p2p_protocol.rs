//! Protocol-level tests for the peer-to-peer layer, run in-process.
//!
//! Everything here uses [`cb_net::p2p::sim`]: N engines in one thread, an
//! explicit clock, and no sockets. That is what makes it possible to assert
//! "this node forwarded the block exactly once" or "this peer was banned after
//! the third bad block" at all — with real sockets those are timing
//! statements, and a timing statement about gossip is a flaky test.
//!
//! The transport gets its own tests in `p2p_transport.rs`, and the
//! verify-and-adopt path gets its own in `p2p_chain_sync.rs` against a real
//! database.

use cb_net::p2p::config::P2pConfig;
use cb_net::p2p::identity::{GenesisId, NodeId};
use cb_net::p2p::message::DisconnectReason;
use cb_net::p2p::sim::{local_node, mem_network, mem_node_id, sim_addr, MemChain, SimNetwork};
use cb_net::p2p::ChainAccess;
use cb_types::H256;
use std::time::Duration;

/// A config tuned for a small simulated network: dial everybody, run discovery
/// and timeouts on a short fuse so a test can advance seconds, not minutes.
fn test_cfg() -> P2pConfig {
    P2pConfig {
        max_peers: 16,
        target_outbound: 8,
        max_known_peers: 64,
        peers_per_response: 16,
        request_timeout: Duration::from_secs(2),
        discovery_interval: Duration::from_secs(1),
        ping_interval: Duration::from_secs(1),
        peer_timeout: Duration::from_secs(3_600),
        ..P2pConfig::default()
    }
}

/// Bring a network of `n` nodes from "everyone knows the seed" to "everyone
/// knows everyone", by running discovery rounds.
fn converge(net: &mut SimNetwork<MemChain>, rounds: u32) {
    net.bootstrap();
    for _ in 0..rounds {
        net.tick(1_100);
    }
}

// -------------------------------------------------------------- 0. the config

#[test]
fn the_config_shipped_with_the_cluster_harness_parses() {
    // The scoring thresholds are configuration, not magic numbers, which is
    // only true if the file the cluster actually ships with loads. An unknown
    // key in it is a startup error, so a typo there would strand an operator
    // with a node that will not boot — catch it here instead.
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/supervisord/p2p.toml");
    let cfg = P2pConfig::from_toml_file(&path).expect("the shipped p2p.toml must parse");
    assert!(cfg.score.ban_at_or_below < cfg.score.initial);
    assert!(cfg.score.bad_block < cfg.score.duplicate);
    assert!(cfg.max_range_blocks > 0);
}

// --------------------------------------------------------------- 1. handshake

#[test]
fn handshake_rejects_a_peer_on_a_different_chain() {
    let genesis = GenesisId(H256([1; 32]));
    let mut net: SimNetwork<MemChain> = SimNetwork::new();

    let mut a = test_cfg();
    a.listen = Some(sim_addr(0));
    net.add(a, local_node("chain-a", 1, genesis), MemChain::new());

    let mut b = test_cfg();
    b.listen = Some(sim_addr(1));
    b.seeds = vec![sim_addr(0)];
    // Same genesis fingerprint, different chain id — the case the executor
    // would otherwise catch one transaction at a time.
    net.add(b, local_node("chain-b", 2, genesis), MemChain::new());

    net.bootstrap();

    assert!(
        net.refused(0, DisconnectReason::ChainIdMismatch)
            || net.refused(1, DisconnectReason::ChainIdMismatch),
        "a chain-id mismatch must be refused at connect: {:?}",
        net.disconnects
    );
    assert!(net.node(0).engine.handshaked_peers().is_empty());
    assert!(net.node(1).engine.handshaked_peers().is_empty());
}

#[test]
fn handshake_rejects_a_peer_with_a_different_genesis() {
    let mut net: SimNetwork<MemChain> = SimNetwork::new();

    let mut a = test_cfg();
    a.listen = Some(sim_addr(0));
    net.add(
        a,
        local_node("gen-a", 1, GenesisId(H256([1; 32]))),
        MemChain::new(),
    );

    let mut b = test_cfg();
    b.listen = Some(sim_addr(1));
    b.seeds = vec![sim_addr(0)];
    net.add(
        b,
        local_node("gen-b", 1, GenesisId(H256([2; 32]))),
        MemChain::new(),
    );

    net.bootstrap();

    assert!(
        net.refused(0, DisconnectReason::GenesisMismatch)
            || net.refused(1, DisconnectReason::GenesisMismatch),
        "a genesis mismatch must be refused at connect: {:?}",
        net.disconnects
    );
    assert!(net.node(1).engine.handshaked_peers().is_empty());
}

#[test]
fn handshake_rejects_a_connection_to_ourselves() {
    // Two sim endpoints sharing one identity: the shape a node takes when an
    // operator gives it a second address for itself as a seed.
    let genesis = GenesisId(H256([1; 32]));
    let mut net: SimNetwork<MemChain> = SimNetwork::new();

    let mut a = test_cfg();
    a.listen = Some(sim_addr(0));
    net.add(a, local_node("twin", 1, genesis), MemChain::new());

    let mut b = test_cfg();
    b.listen = Some(sim_addr(1));
    b.seeds = vec![sim_addr(0)];
    net.add(b, local_node("twin", 1, genesis), MemChain::new());

    net.bootstrap();

    assert!(
        net.refused(0, DisconnectReason::SelfConnection)
            || net.refused(1, DisconnectReason::SelfConnection),
        "a node must refuse its own identity: {:?}",
        net.disconnects
    );
}

#[test]
fn a_node_never_dials_its_own_listen_address() {
    let genesis = GenesisId(H256([1; 32]));
    let mut net: SimNetwork<MemChain> = SimNetwork::new();
    let mut cfg = test_cfg();
    cfg.listen = Some(sim_addr(0));
    // An operator pasting the same seed list onto every node gives each one
    // its own address. That must be a no-op, not a connection.
    cfg.seeds = vec![sim_addr(0)];
    let n = net.add(cfg, local_node("solo", 1, genesis), MemChain::new());
    net.bootstrap();
    assert!(net.node(n).engine.handshaked_peers().is_empty());
    assert_eq!(net.node(n).engine.peers().connected_count(), 0);
}

#[test]
fn two_connections_to_the_same_node_leave_exactly_one() {
    let mut net = mem_network(2, |_| test_cfg());
    net.bootstrap();
    // Both sides also dial each other on the discovery tick; whatever races,
    // the invariant is one live connection per node id.
    net.tick(1_100);
    net.tick(1_100);
    for i in 0..2 {
        assert_eq!(
            net.node(i).engine.peers().connected_count(),
            1,
            "node {i} should hold exactly one connection"
        );
        assert_eq!(net.node(i).engine.handshaked_peers().len(), 1);
    }
}

// --------------------------------------------------------------- 2. discovery

#[test]
fn discovery_converges_from_a_single_seed() {
    const N: usize = 6;
    let mut net = mem_network(N, |_| test_cfg());
    converge(&mut net, 6);

    for i in 0..N {
        let known: Vec<NodeId> = (0..N)
            .filter(|j| *j != i)
            .map(mem_node_id)
            .filter(|id| net.node(i).engine.peers().get(id).is_some())
            .collect();
        assert_eq!(
            known.len(),
            N - 1,
            "node {i} learned only {} of {} peers",
            known.len(),
            N - 1
        );
    }
    // And it is a mesh, not a star: nobody is relying on the seed any more.
    for i in 1..N {
        assert!(
            net.node(i).engine.peers().connected_count() > 1,
            "node {i} is still only connected to the seed"
        );
    }
}

#[test]
fn the_peer_table_stays_bounded_under_discovery() {
    let mut net = mem_network(8, |_| P2pConfig {
        max_peers: 3,
        max_known_peers: 4,
        target_outbound: 2,
        ..test_cfg()
    });
    converge(&mut net, 8);
    for i in 0..net.len() {
        assert!(
            net.node(i).engine.peers().known_count() <= 4,
            "node {i} blew past its peer-table bound"
        );
        assert!(net.node(i).engine.peers().connected_count() <= 3);
    }
}

// ------------------------------------------------------------ 3. block gossip

#[test]
fn a_block_reaches_every_node_and_each_forwards_it_once() {
    const N: usize = 6;
    let mut net = mem_network(N, |_| test_cfg());
    converge(&mut net, 6);

    let (hash, height) = net.node_mut(0).chain.produce();
    net.announce_block(0, hash, height);

    for i in 0..N {
        assert_eq!(
            net.node(i).chain.head_height(),
            1,
            "node {i} did not receive the block"
        );
        assert_eq!(
            net.node(i).engine.stats().blocks_forwarded,
            1,
            "node {i} forwarded the block {} times",
            net.node(i).engine.stats().blocks_forwarded
        );
    }
}

#[test]
fn a_chain_of_blocks_propagates_and_bodies_are_fetched_not_pushed() {
    const N: usize = 5;
    let mut net = mem_network(N, |_| test_cfg());
    converge(&mut net, 6);

    for _ in 0..5 {
        let (hash, height) = net.node_mut(0).chain.produce();
        net.announce_block(0, hash, height);
    }

    for i in 0..N {
        assert_eq!(net.node(i).chain.head_height(), 5, "node {i} is behind");
        assert_eq!(net.node(i).engine.stats().blocks_forwarded, 5);
    }
    // Every body on the wire was answered from a request. A node never sends
    // more bodies than it was asked for, which is the property that keeps the
    // leader's egress from scaling with the peer count.
    for i in 0..N {
        assert!(
            net.sent_count(i, "blockBody") <= net.sent_count(i, "getBlock") + net.len(),
            "node {i} pushed bodies nobody asked for"
        );
    }
}

// -------------------------------------------------------------- 4. tx gossip

#[test]
fn a_transaction_submitted_anywhere_reaches_every_node() {
    const N: usize = 5;
    let mut net = mem_network(N, |_| test_cfg());
    converge(&mut net, 6);

    // Submitted to node 3, which is not the block producer.
    let body = b"a signed order".to_vec();
    let hash = net.node_mut(3).chain.insert_tx(body);
    net.announce_tx(3, hash);

    for i in 0..N {
        assert!(
            net.node(i).chain.tx_bytes(&hash).is_some(),
            "node {i} never saw the transaction"
        );
        assert_eq!(
            net.node(i).engine.stats().txs_forwarded,
            1,
            "node {i} forwarded the transaction more than once"
        );
    }
}

// ---------------------------------------------------------------- 5. catch-up

#[test]
fn a_node_that_was_offline_catches_up_from_a_peer_that_is_not_the_leader() {
    const N: usize = 5;
    let mut net = mem_network(N, |_| test_cfg());
    converge(&mut net, 6);

    // Node 3 goes dark.
    net.partition(&[0, 0, 0, 1, 0]);

    for _ in 0..7 {
        let (hash, height) = net.node_mut(0).chain.produce();
        net.announce_block(0, hash, height);
    }
    assert_eq!(net.node(3).chain.head_height(), 0, "node 3 should be stuck");
    assert_eq!(net.node(4).chain.head_height(), 7);

    // It comes back — but into a pocket of the network that does not contain
    // the leader. The only way forward is to catch up from node 4.
    net.partition(&[0, 0, 0, 1, 1]);
    for _ in 0..10 {
        net.tick(1_100);
    }

    assert_eq!(
        net.node(3).chain.head_height(),
        7,
        "node 3 did not catch up from a non-leader peer"
    );
    // Regression: a node replaying history must not announce every block it
    // replays. The first live-cluster run did exactly that, and the peer it
    // was catching up from banned it for the duplicates. At most the new tip
    // is worth gossiping.
    assert!(
        net.node(3).engine.stats().blocks_forwarded <= 2,
        "a catching-up node re-announced {} historical blocks",
        net.node(3).engine.stats().blocks_forwarded
    );
    let node3_id = net.node(3).engine.node_id();
    assert!(
        !net.node(4)
            .engine
            .peers()
            .is_banned(&node3_id, net.now_ms()),
        "the peer we caught up from banned us for catching up"
    );
    for h in 1..=7 {
        assert_eq!(
            net.node(3).chain.block_bytes(h),
            net.node(0).chain.block_bytes(h),
            "block {h} differs from the leader's"
        );
    }
}

// ------------------------------------------------------- 6. scoring & banning

#[test]
fn a_peer_serving_a_bad_state_root_is_scored_down_then_banned() {
    let genesis = GenesisId(H256([1; 32]));
    let mut net: SimNetwork<MemChain> = SimNetwork::new();

    // The victim.
    let mut victim_cfg = test_cfg();
    victim_cfg.listen = Some(sim_addr(0));
    net.add(
        victim_cfg,
        local_node("victim", 1, genesis),
        MemChain::new(),
    );

    // The liar: it holds real heights but serves forged bodies for them, so
    // every body it hands over fails re-execution.
    let mut liar_cfg = test_cfg();
    liar_cfg.listen = Some(sim_addr(1));
    liar_cfg.seeds = vec![sim_addr(0)];
    let mut liar_chain = MemChain::new();
    for _ in 0..3 {
        liar_chain.produce();
    }
    liar_chain.serve_forgeries = true;
    let liar = net.add(liar_cfg, local_node("liar", 1, genesis), liar_chain);

    net.bootstrap();
    let liar_id = net.node(liar).engine.node_id();

    let ban_threshold = test_cfg().score.ban_at_or_below;
    let initial = test_cfg().score.initial;

    // First round of lies: the score must move, and the block must not.
    net.tick(1_100);
    let after_one = net
        .node(0)
        .engine
        .peers()
        .get(&liar_id)
        .map(|r| r.score)
        .unwrap_or(initial);
    assert!(
        after_one < initial || net.node(0).engine.peers().is_banned(&liar_id, net.now_ms()),
        "a bad block must cost the peer score"
    );
    assert_eq!(
        net.node(0).chain.head_height(),
        0,
        "a block that fails re-execution must never be adopted"
    );

    // Keep going: eventually banned, not retried forever.
    for _ in 0..12 {
        net.tick(1_100);
    }
    assert!(
        net.node(0).engine.peers().is_banned(&liar_id, net.now_ms()),
        "the liar should be banned by now (score {after_one}, threshold {ban_threshold})"
    );
    assert!(net.node(0).engine.stats().bans >= 1);
    assert_eq!(
        net.node(0).chain.head_height(),
        0,
        "no forged block was ever adopted"
    );

    // And the ban is enforced at connect, so the liar cannot simply redial.
    for _ in 0..5 {
        net.tick(1_100);
    }
    assert!(
        net.refused(0, DisconnectReason::Banned) || net.refused(0, DisconnectReason::Misbehaving),
        "a banned peer must be refused rather than re-handshaked: {:?}",
        net.disconnects
    );
}

#[test]
fn a_peer_that_has_been_told_it_is_banned_stops_redialling() {
    // Regression: without a back-off, a banned node redialled its victim on
    // every tick, completed a handshake, was refused, and started again — a
    // few times a second, forever.
    let genesis = GenesisId(H256([1; 32]));
    let mut net: SimNetwork<MemChain> = SimNetwork::new();
    let mut a = test_cfg();
    a.listen = Some(sim_addr(0));
    net.add(a, local_node("victim", 1, genesis), MemChain::new());

    let mut b = test_cfg();
    b.listen = Some(sim_addr(1));
    b.seeds = vec![sim_addr(0)];
    let mut liar_chain = MemChain::new();
    liar_chain.produce();
    liar_chain.serve_forgeries = true;
    net.add(b, local_node("liar", 1, genesis), liar_chain);

    net.bootstrap();
    let liar_id = net.node(1).engine.node_id();
    for _ in 0..12 {
        net.tick(1_100);
    }
    assert!(net.node(0).engine.peers().is_banned(&liar_id, net.now_ms()));

    let dials_at_ban = net.dial_count(1);
    for _ in 0..20 {
        net.tick(1_100);
    }
    let extra = net.dial_count(1) - dials_at_ban;
    assert!(
        extra <= 1,
        "the banned peer redialled {extra} more times over 22 simulated seconds"
    );
}

#[test]
fn a_ban_expires_so_an_honest_peer_is_not_exiled_forever() {
    let genesis = GenesisId(H256([1; 32]));
    let mut net: SimNetwork<MemChain> = SimNetwork::new();
    let mut a = test_cfg();
    a.listen = Some(sim_addr(0));
    a.score.ban = Duration::from_secs(30);
    net.add(a, local_node("victim", 1, genesis), MemChain::new());

    let mut b = test_cfg();
    b.listen = Some(sim_addr(1));
    b.seeds = vec![sim_addr(0)];
    let mut liar_chain = MemChain::new();
    liar_chain.produce();
    liar_chain.serve_forgeries = true;
    net.add(b, local_node("liar", 1, genesis), liar_chain);

    net.bootstrap();
    let liar_id = net.node(1).engine.node_id();
    for _ in 0..10 {
        net.tick(1_100);
    }
    assert!(net.node(0).engine.peers().is_banned(&liar_id, net.now_ms()));

    // Stop lying and wait the ban out.
    net.node_mut(1).chain.serve_forgeries = false;
    for _ in 0..30 {
        net.tick(1_100);
    }
    assert!(
        !net.node(0).engine.peers().is_banned(&liar_id, net.now_ms()),
        "bans must expire"
    );
}

#[test]
fn a_peer_claiming_an_impossible_head_cannot_freeze_the_network() {
    // Regression for a design flaw, not a crash: an advertised head is an
    // unverifiable number. A peer that claims `u64::MAX` was elected the best
    // catch-up source forever, answered every range with nothing, took a
    // one-point penalty that can never ban it, and stopped sync dead — while
    // every honest peer, being "behind" it, was never asked. It also made
    // every honest node think it was still catching up, so nobody relayed.
    const N: usize = 4;
    let mut net = mem_network(N, |_| test_cfg());
    converge(&mut net, 6);

    let liar = N - 1;
    net.announce_block(liar, H256([0xff; 32]), u64::MAX);

    for _ in 0..3 {
        let (hash, height) = net.node_mut(0).chain.produce();
        net.announce_block(0, hash, height);
    }
    for _ in 0..4 {
        net.tick(1_100);
    }

    for i in 0..(N - 1) {
        assert_eq!(
            net.node(i).chain.head_height(),
            3,
            "node {i} stalled behind a peer's bogus head claim"
        );
    }
    let liar_id = mem_node_id(liar);
    for i in 0..(N - 1) {
        let claimed = net
            .node(i)
            .engine
            .peers()
            .get(&liar_id)
            .map(|r| r.head_height)
            .unwrap_or(0);
        assert!(
            claimed < u64::MAX,
            "node {i} still believes the liar's head is {claimed}"
        );
    }
}

// -------------------------------------------------------------- 7. partitions

#[test]
fn a_partition_heals_and_both_halves_converge() {
    const N: usize = 6;
    let mut net = mem_network(N, |_| test_cfg());
    converge(&mut net, 6);

    let (hash, height) = net.node_mut(0).chain.produce();
    net.announce_block(0, hash, height);
    for i in 0..N {
        assert_eq!(net.node(i).chain.head_height(), 1);
    }

    // Split. Node 0 is the only block producer in this system, so the honest
    // statement of what a partition does here is: the half with the producer
    // keeps moving, the half without it stalls. There is no fork to reconcile
    // because there is no second proposer — reconciling competing forks is a
    // consensus question, not a gossip one, and this test deliberately does
    // not pretend otherwise.
    net.partition(&[0, 0, 0, 1, 1, 1]);
    for _ in 0..5 {
        let (hash, height) = net.node_mut(0).chain.produce();
        net.announce_block(0, hash, height);
    }
    net.tick(1_100);

    for i in 0..3 {
        assert_eq!(
            net.node(i).chain.head_height(),
            6,
            "node {i} should advance"
        );
    }
    for i in 3..6 {
        assert_eq!(net.node(i).chain.head_height(), 1, "node {i} should stall");
    }

    // Heal.
    net.heal();
    for _ in 0..12 {
        net.tick(1_100);
    }

    for i in 0..N {
        assert_eq!(
            net.node(i).chain.head_height(),
            6,
            "node {i} did not converge after the partition healed"
        );
        for h in 1..=6 {
            assert_eq!(
                net.node(i).chain.block_bytes(h),
                net.node(0).chain.block_bytes(h),
                "node {i} disagrees about block {h}"
            );
        }
    }
}

// ------------------------------------------- a body we did not ask for

/// Handshake two engines over connection `1` and return the victim and the
/// peer's node id.
fn handshaken_pair() -> (cb_net::p2p::P2pEngine, NodeId) {
    use cb_net::p2p::{Action, Event, Message, P2pEngine};
    let genesis = GenesisId(H256([1; 32]));
    let mut victim = P2pEngine::new(test_cfg(), local_node("victim", 1, genesis), 0, 0);
    let mut peer = P2pEngine::new(test_cfg(), local_node("peer", 1, genesis), 0, 0);
    victim.handle(Event::Connected {
        peer: 1,
        addr: None,
        outbound: false,
    });
    let hello = peer
        .handle(Event::Connected {
            peer: 1,
            addr: None,
            outbound: true,
        })
        .into_iter()
        .find_map(|a| match a {
            Action::Send {
                msg: msg @ Message::Hello(_),
                ..
            } => Some(msg),
            _ => None,
        })
        .expect("the dialler says hello");
    victim.handle(Event::Message {
        peer: 1,
        msg: hello,
    });
    let peer_id = peer.node_id();
    assert!(
        victim.handshaked_peers().contains(&peer_id),
        "the handshake completed"
    );
    (victim, peer_id)
}

#[test]
fn a_body_claiming_a_height_nobody_asked_for_is_refused_and_scored() {
    // One open `GetBlock` used to license any number of bodies at any height.
    // Far-future bodies were buffered unverified — so never penalised — until
    // the pending buffer was full, and then the honest `head + 1` body was the
    // one refused as a flood, its honest sender the one penalised, and sync
    // stopped for good.
    use cb_net::p2p::{Action, Event, Message};
    let (mut victim, peer_id) = handshaken_pair();
    let score = |engine: &cb_net::p2p::P2pEngine| {
        engine
            .peers()
            .get(&peer_id)
            .map(|r| r.score)
            .expect("known")
    };

    let hash = H256([7; 32]);
    let asked = victim.handle(Event::Message {
        peer: 1,
        msg: Message::BlockAnnounce { hash, height: 1 },
    });
    assert!(
        asked.iter().any(|a| matches!(
            a,
            Action::Send {
                msg: Message::GetBlock { .. },
                ..
            }
        )),
        "the announcement at head + 1 is fetched"
    );
    let before = score(&victim);

    let answer = victim.handle(Event::Message {
        peer: 1,
        msg: Message::BlockBody {
            height: 50,
            bytes: vec![0; 64],
        },
    });
    assert!(
        !answer
            .iter()
            .any(|a| matches!(a, Action::VerifyBlock { .. })),
        "nothing is verified for it"
    );
    assert!(score(&victim) < before, "and it costs the sender");

    // The honest body at head + 1, on a fresh request, still goes to the
    // verifier: the lie took no buffer slot.
    victim.handle(Event::Message {
        peer: 1,
        msg: Message::BlockAnnounce {
            hash: H256([8; 32]),
            height: 1,
        },
    });
    let honest = victim.handle(Event::Message {
        peer: 1,
        msg: Message::BlockBody {
            height: 1,
            bytes: vec![1; 64],
        },
    });
    assert!(honest
        .iter()
        .any(|a| matches!(a, Action::VerifyBlock { height: 1, .. })));
}

// ------------------------------------------- a source that never answers

/// Handshake `victim` with a fresh engine over connection `conn`, the peer
/// advertising `head`. Returns the peer's node id.
fn attach_peer(victim: &mut cb_net::p2p::P2pEngine, conn: u64, seed: &str, head: u64) -> NodeId {
    use cb_net::p2p::{Action, Event, Message, P2pEngine};
    let genesis = GenesisId(H256([1; 32]));
    let mut peer = P2pEngine::new(test_cfg(), local_node(seed, 1, genesis), head, 0);
    victim.handle(Event::Connected {
        peer: conn,
        addr: None,
        outbound: false,
    });
    let hello = peer
        .handle(Event::Connected {
            peer: conn,
            addr: None,
            outbound: true,
        })
        .into_iter()
        .find_map(|a| match a {
            Action::Send {
                msg: msg @ Message::Hello(_),
                ..
            } => Some(msg),
            _ => None,
        })
        .expect("hello");
    victim.handle(Event::Message {
        peer: conn,
        msg: hello,
    });
    peer.node_id()
}

fn range_requests_to(actions: &[cb_net::p2p::Action], conn: u64) -> usize {
    use cb_net::p2p::{Action, Message};
    actions
        .iter()
        .filter(|a| {
            matches!(a, Action::Send { peer, msg: Message::GetBlockRange { .. } }
                if *peer == conn)
        })
        .count()
}

#[test]
fn a_range_request_that_times_out_is_re_routed_to_another_peer_at_once() {
    // A peer that claims the highest head and never answers used to be asked
    // again on every tick after its timeout: the timeout scored it, but it was
    // still the peer with the highest advertised head, so it was still the
    // chosen source. Twenty timeouts later the score banned it. Until then,
    // sync waited on it.
    use cb_net::p2p::{Event, P2pEngine};
    let genesis = GenesisId(H256([1; 32]));
    let mut victim = P2pEngine::new(test_cfg(), local_node("rr-victim", 1, genesis), 0, 0);
    let silent = attach_peer(&mut victim, 1, "rr-silent", 1_000);
    let _honest = attach_peer(&mut victim, 2, "rr-honest", 50);
    let score = |e: &P2pEngine| e.peers().get(&silent).map(|r| r.score).expect("known");
    let before = score(&victim);

    // The first range went to the silent peer during its handshake. It says
    // nothing. One tick past the request timeout:
    let timeout_ms = test_cfg().request_timeout.as_millis() as u64;
    let actions = victim.handle(Event::Tick {
        now_ms: timeout_ms + 1,
    });
    assert_eq!(
        range_requests_to(&actions, 2),
        1,
        "the same tick asks the next-best peer: {actions:?}"
    );
    assert_eq!(range_requests_to(&actions, 1), 0);
    assert!(score(&victim) < before, "and the silence is scored");
}

#[test]
fn a_block_fetch_that_times_out_moves_catch_up_to_another_peer() {
    // The single-block path: a peer announces `head + 1`, we ask it for the
    // body, and it never sends one. The body is re-fetched from somebody else
    // as a range rather than waiting for the announcer to announce again.
    use cb_net::p2p::{Event, Message, P2pEngine};
    let genesis = GenesisId(H256([1; 32]));
    let mut victim = P2pEngine::new(test_cfg(), local_node("bf-victim", 1, genesis), 0, 0);
    let silent = attach_peer(&mut victim, 1, "bf-silent", 0);
    let honest = attach_peer(&mut victim, 2, "bf-honest", 0);
    // The silent peer has a good record — it has been useful for discovery —
    // so one timeout's worth of score does not drop it below the honest peer.
    // Scoring alone would therefore pick it again; only benching moves on.
    for i in 0..10u8 {
        victim.handle(Event::Message {
            peer: 1,
            msg: Message::Peers(vec![cb_net::p2p::PeerAddr {
                node_id: NodeId(cb_types::Address([100 + i; 20])),
                addr: format!("10.9.9.{i}:7000").parse().expect("addr"),
            }]),
        });
    }
    let score = |id: &NodeId| victim.peers().get(id).map(|r| r.score).expect("known");
    assert!(
        score(&silent) > score(&honest) + 5,
        "the setup favours the silent peer"
    );
    // Both announce block 1; the first announcer gets the fetch.
    victim.handle(Event::Message {
        peer: 1,
        msg: Message::BlockAnnounce {
            hash: H256([9; 32]),
            height: 1,
        },
    });
    victim.handle(Event::Message {
        peer: 2,
        msg: Message::BlockAnnounce {
            hash: H256([9; 32]),
            height: 1,
        },
    });
    let timeout_ms = test_cfg().request_timeout.as_millis() as u64;
    let actions = victim.handle(Event::Tick {
        now_ms: timeout_ms + 1,
    });
    assert_eq!(
        range_requests_to(&actions, 2),
        1,
        "the other peer that has block 1 is asked for it: {actions:?}"
    );
}

// ------------------------------------------- found by the lossy-network tests

#[test]
fn a_node_that_missed_the_last_announcement_still_reaches_the_head() {
    // Heads were learned only from the `Hello` and from announcements, each
    // sent once. Lose the announcement of the last block and the node never
    // heard of it again: on a chain that had stopped, it sat one block behind
    // for ever. The head now rides along with every ping.
    use cb_net::p2p::sim::Faults;
    let mut net = mem_network(2, |_| test_cfg());
    converge(&mut net, 3);
    let (hash, height) = net.node_mut(0).chain.produce();
    net.set_faults(Faults {
        drop_pct: 100,
        ..Faults::default()
    });
    net.announce_block(0, hash, height);
    net.set_faults(Faults::default());
    assert_eq!(
        net.node(1).chain.head_height(),
        0,
        "the announcement was lost"
    );
    for _ in 0..3 {
        net.tick(1_100);
    }
    assert_eq!(net.node(1).chain.head_height(), 1);
}

#[test]
fn a_node_whose_first_handshake_failed_goes_back_to_its_seed() {
    // Seeds are dialed once, at bootstrap, and only enter the peer table
    // once a handshake with them completes. A node whose first handshake
    // failed had nothing left to dial and stayed alone.
    use cb_net::p2p::sim::Faults;
    let mut net = mem_network(2, |_| test_cfg());
    net.set_faults(Faults {
        drop_pct: 100,
        ..Faults::default()
    });
    net.bootstrap();
    net.tick(100);
    net.set_faults(Faults::default());
    assert_eq!(net.node(1).engine.stats().connected_peers, 0);
    // Past the handshake deadline, the half-open connection is dropped and
    // the seed is dialed again.
    for _ in 0..5 {
        net.tick(1_100);
    }
    assert_eq!(net.node(1).engine.stats().connected_peers, 1);
    assert!(net.dial_count(1) >= 2, "the seed was dialed again");
}

#[test]
fn a_peer_we_are_only_backing_off_from_is_not_refused_as_banned() {
    // A dial backoff used to be stored as a ban. A peer that had merely
    // closed on us was then refused as `Banned` when it dialed back — and,
    // told it was banned, backed off from us for the whole ban period.
    use cb_net::p2p::{Action, Event, Message};
    let (mut victim, peer_id) = handshaken_pair();
    // The peer leaves with an ordinary reason: a short dial backoff.
    victim.handle(Event::Message {
        peer: 1,
        msg: Message::Disconnect(DisconnectReason::Timeout),
    });
    // And dials straight back in.
    let genesis = GenesisId(H256([1; 32]));
    let mut peer = cb_net::p2p::P2pEngine::new(test_cfg(), local_node("peer", 1, genesis), 0, 0);
    assert_eq!(peer.node_id(), peer_id);
    victim.handle(Event::Connected {
        peer: 2,
        addr: None,
        outbound: false,
    });
    let hello = peer
        .handle(Event::Connected {
            peer: 2,
            addr: None,
            outbound: true,
        })
        .into_iter()
        .find_map(|a| match a {
            Action::Send {
                msg: msg @ Message::Hello(_),
                ..
            } => Some(msg),
            _ => None,
        })
        .expect("hello");
    let actions = victim.handle(Event::Message {
        peer: 2,
        msg: hello,
    });
    assert!(
        !actions.iter().any(|a| matches!(
            a,
            Action::Disconnect {
                reason: DisconnectReason::Banned,
                ..
            }
        )),
        "{actions:?}"
    );
    assert!(victim.handshaked_peers().contains(&peer_id));
}
