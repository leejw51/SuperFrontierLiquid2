//! One honest node, one victim, and a crowd of hostile raw-socket peers.
//!
//! Every adversary here speaks the real wire format over real TCP to a real
//! [`spawn`]ed node; none of them goes through the engine's front door. The
//! victim starts empty, the adversaries arrive first — one of them claiming
//! the highest head on the network — and only then does the honest node
//! connect with the chain. The properties:
//!
//! * the victim still syncs the honest chain, to the block;
//! * every adversary's connection is closed by the victim, each for its own
//!   reason and inside its own deadline;
//! * the ones that misbehave on a scored path are banned, and the honest
//!   peer is not.
//!
//! Ports are all 0.

use cb_net::p2p::codec;
use cb_net::p2p::config::P2pConfig;
use cb_net::p2p::engine::LocalNode;
use cb_net::p2p::identity::GenesisId;
use cb_net::p2p::message::{Hello, Message};
use cb_net::p2p::sim::MemChain;
use cb_net::p2p::transport::spawn;
use cb_net::p2p::ChainAccess;
use cb_types::H256;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const BLOCKS: u64 = 40;

fn genesis() -> GenesisId {
    GenesisId(H256([0x33; 32]))
}

fn cfg(seeds: Vec<SocketAddr>) -> P2pConfig {
    P2pConfig {
        listen: Some("127.0.0.1:0".parse().expect("addr")),
        seeds,
        max_peers: 24,
        target_outbound: 4,
        max_range_blocks: 16,
        // Handshake budget and request timeout.
        request_timeout: Duration::from_secs(2),
        discovery_interval: Duration::from_secs(1),
        ping_interval: Duration::from_millis(500),
        // Idle read timeout and the engine's staleness bound.
        peer_timeout: Duration::from_secs(4),
        max_messages_per_window: 200,
        rate_window: Duration::from_secs(1),
        ..P2pConfig::default()
    }
}

fn node(seed: &str) -> LocalNode {
    let (sk, _) = cb_crypto::keypair_from_seed(seed.as_bytes());
    LocalNode::new(sk, 1, genesis(), format!("adv/{seed}"))
}

fn hello(seed: &str, chain_id: u32, head: u64) -> Hello {
    let (sk, _) = cb_crypto::keypair_from_seed(seed.as_bytes());
    Hello::signed(
        &sk,
        cb_net::p2p::PROTOCOL_VERSION,
        chain_id,
        genesis(),
        format!("adv/{seed}"),
        head,
        None,
        42,
    )
    .expect("sign")
}

fn frame(msg: &Message) -> Vec<u8> {
    codec::encode_frame(msg).expect("frame").to_vec()
}

/// Read and discard until the node closes the connection, or `wait` passes.
/// Returns how long the close took, or `None` if it never came.
async fn wait_for_close(sock: &mut TcpStream, wait: Duration) -> Option<Duration> {
    let start = Instant::now();
    let mut buf = vec![0u8; 64 * 1024];
    while start.elapsed() < wait {
        match tokio::time::timeout(Duration::from_millis(200), sock.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => return Some(start.elapsed()),
            Ok(Ok(_)) | Err(_) => continue,
        }
    }
    None
}

/// Each adversary: a name, and a future that returns once the victim has
/// closed on it (with the time that took) or panics.
type Outcome = (&'static str, Option<Duration>);

async fn connect(addr: SocketAddr) -> TcpStream {
    TcpStream::connect(addr).await.expect("connect")
}

/// Claims the highest head on the network and never answers anything.
async fn silent_liar(addr: SocketAddr) -> Outcome {
    let mut s = connect(addr).await;
    s.write_all(&frame(&Message::Hello(hello("adv-liar", 1, 10_000))))
        .await
        .expect("hello");
    (
        "silent liar",
        wait_for_close(&mut s, Duration::from_secs(20)).await,
    )
}

/// Floods pings far past the rate limit.
async fn flooder(addr: SocketAddr) -> Outcome {
    let mut s = connect(addr).await;
    s.write_all(&frame(&Message::Hello(hello("adv-flood", 1, 0))))
        .await
        .expect("hello");
    let ping = frame(&Message::Ping { nonce: 1 });
    let mut burst = Vec::with_capacity(ping.len() * 2_000);
    for _ in 0..2_000 {
        burst.extend_from_slice(&ping);
    }
    let _ = s.write_all(&burst).await;
    (
        "flooder",
        wait_for_close(&mut s, Duration::from_secs(10)).await,
    )
}

/// Sends block bodies and ranges nobody asked for, at heights nobody wants.
async fn body_spammer(addr: SocketAddr) -> Outcome {
    let mut s = connect(addr).await;
    s.write_all(&frame(&Message::Hello(hello("adv-bodies", 1, 0))))
        .await
        .expect("hello");
    for h in 0..20u64 {
        let body = MemChain::encode_block(999 + h, H256([0xee; 32]), true);
        let _ = s
            .write_all(&frame(&Message::BlockBody {
                height: 999 + h,
                bytes: body.clone(),
            }))
            .await;
        let _ = s
            .write_all(&frame(&Message::BlockRange {
                blocks: vec![(999 + h, body)],
            }))
            .await;
    }
    (
        "body spammer",
        wait_for_close(&mut s, Duration::from_secs(10)).await,
    )
}

/// Sends its handshake one byte at a time, slower than the handshake budget.
async fn slowloris_before_handshake(addr: SocketAddr) -> Outcome {
    let (mut r, mut w) = connect(addr).await.into_split();
    let h = frame(&Message::Hello(hello("adv-slow-pre", 1, 0)));
    tokio::spawn(async move {
        for b in h {
            if w.write_all(&[b]).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    });
    let mut buf = vec![0u8; 4096];
    let start = Instant::now();
    let closed = loop {
        if start.elapsed() > Duration::from_secs(10) {
            break None;
        }
        match tokio::time::timeout(Duration::from_millis(200), r.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => break Some(start.elapsed()),
            _ => continue,
        }
    };
    ("slowloris before handshake", closed)
}

/// Completes the handshake, then trickles a frame a byte a second: each read
/// makes progress, so only the engine's staleness bound can end it.
async fn slowloris_after_handshake(addr: SocketAddr) -> Outcome {
    let s = connect(addr).await;
    let (mut r, mut w) = s.into_split();
    w.write_all(&frame(&Message::Hello(hello("adv-slow-post", 1, 0))))
        .await
        .expect("hello");
    tokio::spawn(async move {
        let _ = w.write_all(&1_000u32.to_be_bytes()).await;
        for _ in 0..1_000 {
            if w.write_all(&[0]).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
    let mut buf = vec![0u8; 4096];
    let start = Instant::now();
    let closed = loop {
        if start.elapsed() > Duration::from_secs(20) {
            break None;
        }
        match tokio::time::timeout(Duration::from_millis(200), r.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => break Some(start.elapsed()),
            _ => continue,
        }
    };
    ("slowloris after handshake", closed)
}

/// Declares a frame past the maximum after a valid handshake.
async fn oversized(addr: SocketAddr) -> Outcome {
    let mut s = connect(addr).await;
    s.write_all(&frame(&Message::Hello(hello("adv-huge", 1, 0))))
        .await
        .expect("hello");
    let _ = s
        .write_all(&((codec::MAX_FRAME_BYTES + 1) as u32).to_be_bytes())
        .await;
    (
        "oversized frame",
        wait_for_close(&mut s, Duration::from_secs(10)).await,
    )
}

/// Handshake abuse, one connection per variant.
async fn handshake_abuse(addr: SocketAddr, variant: u8) -> Outcome {
    let mut s = connect(addr).await;
    let name = match variant {
        0 => {
            let mut junk = 64u32.to_be_bytes().to_vec();
            junk.extend_from_slice(&[0xa5; 64]);
            let _ = s.write_all(&junk).await;
            "garbage first frame"
        }
        1 => {
            let _ = s.write_all(&frame(&Message::Ping { nonce: 1 })).await;
            "message before hello"
        }
        2 => {
            let h = frame(&Message::Hello(hello("adv-twice", 1, 0)));
            let _ = s.write_all(&h).await;
            let _ = s.write_all(&h).await;
            "hello twice"
        }
        3 => {
            let mut h = hello("adv-forged", 1, 0);
            h.head_height += 1; // no longer what was signed
            let _ = s.write_all(&frame(&Message::Hello(h))).await;
            "forged hello"
        }
        4 => {
            let _ = s
                .write_all(&frame(&Message::Hello(hello("adv-chain", 2, 0))))
                .await;
            "wrong chain"
        }
        _ => {
            let _ = s
                .write_all(&frame(&Message::Hello(hello("adv-oversize-hello", 1, 0))))
                .await;
            // A second, oversized "handshake-sized" claim straight away.
            let _ = s
                .write_all(
                    &(cb_net::p2p::transport::MAX_HANDSHAKE_FRAME_BYTES as u32 * 8).to_be_bytes(),
                )
                .await;
            "hello then oversized"
        }
    };
    (name, wait_for_close(&mut s, Duration::from_secs(10)).await)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_honest_node_syncs_through_a_crowd_of_hostile_peers() {
    let victim_chain = Arc::new(Mutex::new(MemChain::new()));
    let victim = spawn(cfg(vec![]), node("adv-victim"), victim_chain.clone())
        .await
        .expect("spawn victim");
    let addr = victim.listen_addr().expect("bound");

    // The adversaries arrive first.
    let mut tasks = vec![
        tokio::spawn(silent_liar(addr)),
        tokio::spawn(flooder(addr)),
        tokio::spawn(body_spammer(addr)),
        tokio::spawn(slowloris_after_handshake(addr)),
        tokio::spawn(oversized(addr)),
    ];
    for v in 0..6u8 {
        tasks.push(tokio::spawn(handshake_abuse(addr, v)));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Then the honest node, with the chain, dials in.
    let mut honest_chain = MemChain::new();
    for _ in 0..BLOCKS {
        honest_chain.produce();
    }
    let honest_chain = Arc::new(Mutex::new(honest_chain));
    let honest = spawn(cfg(vec![addr]), node("adv-honest"), honest_chain)
        .await
        .expect("spawn honest");

    let started = Instant::now();
    while victim_chain.lock().expect("lock").head_height() < BLOCKS {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the victim stalled at {} of {BLOCKS}",
            victim_chain.lock().expect("lock").head_height()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    for t in tasks {
        let (name, closed) = t.await.expect("adversary task");
        assert!(closed.is_some(), "the victim never closed on the {name}");
    }
    let stats = victim.stats().await.expect("victim alive");
    assert!(
        stats.bans >= 2,
        "the flooder and the body spammer are banned: {stats:?}"
    );
    let honest_stats = honest.stats().await.expect("honest alive");
    assert_eq!(honest_stats.bans, 0, "the honest node banned nobody");
    // Once the dust settles, the only peer left is the honest one.
    let settle = Instant::now();
    loop {
        let s = victim.stats().await.expect("alive");
        if s.connected_peers == 1 {
            break;
        }
        assert!(
            settle.elapsed() < Duration::from_secs(10),
            "victim still has {} peers",
            s.connected_peers
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    victim.shutdown();
    honest.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_handshake_sent_a_byte_at_a_time_is_cut_off_at_the_handshake_budget() {
    let chain = Arc::new(Mutex::new(MemChain::new()));
    let victim = spawn(cfg(vec![]), node("adv-slow-victim"), chain)
        .await
        .expect("spawn");
    let (name, closed) = slowloris_before_handshake(victim.listen_addr().expect("bound")).await;
    let took = closed.unwrap_or_else(|| panic!("the victim never closed on the {name}"));
    assert!(
        took < Duration::from_secs(5),
        "a 2 s handshake budget took {took:?} to enforce"
    );
    victim.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_that_claims_the_top_head_and_never_answers_costs_one_timeout_not_twenty() {
    // With no idle timeout to rescue it (an hour here), the only thing that
    // moves sync off a silent source is the per-request timeout re-routing
    // the range. Before, the silent peer — still the highest claimed head —
    // was asked again after every timeout until its score banned it: twenty
    // 2 s timeouts before the honest peer was asked at all.
    let mut c = cfg(vec![]);
    c.peer_timeout = Duration::from_secs(3_600);
    let victim_chain = Arc::new(Mutex::new(MemChain::new()));
    let victim = spawn(c, node("adv-rr-victim"), victim_chain.clone())
        .await
        .expect("spawn victim");
    let addr = victim.listen_addr().expect("bound");

    let mut liar = connect(addr).await;
    liar.write_all(&frame(&Message::Hello(hello("adv-rr-liar", 1, 1_000_000))))
        .await
        .expect("hello");
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut honest_chain = MemChain::new();
    for _ in 0..BLOCKS {
        honest_chain.produce();
    }
    let honest = spawn(
        cfg(vec![addr]),
        node("adv-rr-honest"),
        Arc::new(Mutex::new(honest_chain)),
    )
    .await
    .expect("spawn honest");

    let started = Instant::now();
    while victim_chain.lock().expect("lock").head_height() < BLOCKS {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the victim is still waiting on the silent peer at {}",
            victim_chain.lock().expect("lock").head_height()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(liar);
    victim.shutdown();
    honest.shutdown();
}
