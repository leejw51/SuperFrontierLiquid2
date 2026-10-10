//! Catch-up and gossip under message loss, duplication and reordering.
//!
//! Three to five in-process engines ([`cb_net::p2p::sim`]) with seeded faults
//! on every message: some are lost, some delivered twice, some held back and
//! delivered after later traffic. One node produces blocks throughout. The
//! property is the one that matters for a chain: once production stops, every
//! node reaches the producer's head, and every node agrees on each block —
//! the toy chain refuses a block whose hash is not the canonical one, so
//! reaching height `h` means holding the same `h` blocks.
//!
//! The faults are seeded, so a failure names the seed that reproduces it.

use cb_net::p2p::config::P2pConfig;
use cb_net::p2p::sim::{mem_network, Faults, MemChain, SimNetwork};
use cb_net::p2p::ChainAccess;
use proptest::prelude::*;
use std::time::Duration;

fn cfg() -> P2pConfig {
    P2pConfig {
        max_peers: 16,
        target_outbound: 8,
        max_known_peers: 64,
        peers_per_response: 16,
        max_range_blocks: 8,
        request_timeout: Duration::from_secs(2),
        discovery_interval: Duration::from_secs(1),
        ping_interval: Duration::from_secs(1),
        peer_timeout: Duration::from_secs(3_600),
        ..P2pConfig::default()
    }
}

fn heads(net: &SimNetwork<MemChain>) -> Vec<u64> {
    (0..net.len())
        .map(|i| net.node(i).chain.head_height())
        .collect()
}

/// Run one faulty network to the end and report the heads it settled on.
///
/// Blocks are produced during the faults, then the producer goes quiet and
/// the network is ticked — faults still on — until every node has the
/// producer's head or the budget runs out.
fn run(nodes: usize, blocks: u64, faults: Faults) -> (Vec<u64>, SimNetwork<MemChain>) {
    let mut net = mem_network(nodes, |_| cfg());
    net.set_faults(faults);
    net.bootstrap();
    for _ in 0..4 {
        net.tick(1_100);
    }
    for _ in 0..blocks {
        let (hash, height) = net.node_mut(0).chain.produce();
        net.announce_block(0, hash, height);
        net.tick(300);
    }
    // Quiet period: no production, faults on. A node that missed the last
    // announcements must still get there — by a range from a peer whose head
    // it learned some other way, or by the next time it hears one.
    for _ in 0..200 {
        if heads(&net).iter().all(|h| *h == blocks) {
            break;
        }
        net.tick(1_100);
    }
    (heads(&net), net)
}

fn assert_converged(nodes: usize, blocks: u64, faults: Faults) {
    let (heads, net) = run(nodes, blocks, faults);
    assert!(
        heads.iter().all(|h| *h == blocks),
        "{nodes} nodes, {faults:?}: heads {heads:?}, faults injected (lost, duplicated, \
         delayed) = {:?}",
        net.fault_counts
    );
}

#[test]
fn three_to_five_nodes_converge_under_loss_duplication_and_reordering() {
    for nodes in 3..=5 {
        for seed in 1..=16u64 {
            assert_converged(
                nodes,
                24,
                Faults {
                    drop_pct: 10,
                    dup_pct: 5,
                    delay_pct: 10,
                    seed,
                },
            );
        }
    }
}

#[test]
fn heavy_loss_alone_still_converges() {
    for seed in 1..=4u64 {
        assert_converged(
            4,
            16,
            Faults {
                drop_pct: 30,
                dup_pct: 0,
                delay_pct: 0,
                seed,
            },
        );
    }
}

#[test]
fn heavy_duplication_still_converges() {
    // Duplicates are the one fault an honest peer cannot be told apart from
    // spam by: a repeated range reply is, to the receiver, a reply to nothing.
    // TCP never repeats a message, so on a real network a repeat is the
    // sender's doing and scoring it as unsolicited is right — and at this
    // rate some links are banned for it. The property is that sync survives
    // that: bans are per link, and the chain still reaches every node.
    for seed in 1..=4u64 {
        assert_converged(
            4,
            16,
            Faults {
                drop_pct: 0,
                dup_pct: 30,
                delay_pct: 0,
                seed,
            },
        );
    }
}

#[test]
fn heavy_reordering_alone_still_converges() {
    for seed in 1..=4u64 {
        assert_converged(
            5,
            16,
            Faults {
                drop_pct: 0,
                dup_pct: 0,
                delay_pct: 40,
                seed,
            },
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn any_seed_and_mild_fault_mix_converges(
        nodes in 3usize..=5,
        seed in any::<u64>(),
        drop_pct in 0u8..=15,
        dup_pct in 0u8..=10,
        delay_pct in 0u8..=20,
    ) {
        let blocks = 12;
        let (heads, net) = run(nodes, blocks, Faults { drop_pct, dup_pct, delay_pct, seed });
        prop_assert!(
            heads.iter().all(|h| *h == blocks),
            "heads {:?}, injected {:?}", heads, net.fault_counts
        );
    }
}

/// The same property over 900 networks and a harsher mix: a fifth of all
/// messages lost, a tenth duplicated, a fifth delayed past later traffic. A
/// couple of seconds in release, so it runs with the suite.
#[test]
fn many_seeds_under_a_harsh_fault_mix() {
    for nodes in 3..=5 {
        for seed in 1..=300u64 {
            assert_converged(
                nodes,
                40,
                Faults {
                    drop_pct: 20,
                    dup_pct: 10,
                    delay_pct: 20,
                    seed: seed * 7_919 + nodes as u64,
                },
            );
        }
    }
}
