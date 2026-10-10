//! Real-socket tests for the peer-to-peer transport.
//!
//! Deliberately few. The protocol is tested exhaustively in-process in
//! `p2p_protocol.rs`; what only a socket can break is framing, connection
//! setup and teardown, and the wiring between the codec and the engine. Those
//! are what this file covers.
//!
//! Every test binds port 0 and reads back the address the OS assigned, so the
//! suite can run in parallel with itself and with anything else on the
//! machine.

use cb_net::p2p::chain::ChainAccess;
use cb_net::p2p::config::P2pConfig;
use cb_net::p2p::engine::LocalNode;
use cb_net::p2p::identity::GenesisId;
use cb_net::p2p::sim::MemChain;
use cb_net::p2p::transport::{spawn, P2pHandle};
use cb_types::H256;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn genesis() -> GenesisId {
    GenesisId(H256([0x5a; 32]))
}

fn cfg(seeds: Vec<std::net::SocketAddr>) -> P2pConfig {
    P2pConfig {
        listen: Some("127.0.0.1:0".parse().expect("addr")),
        seeds,
        max_peers: 8,
        target_outbound: 4,
        request_timeout: Duration::from_secs(2),
        discovery_interval: Duration::from_secs(1),
        ping_interval: Duration::from_secs(1),
        peer_timeout: Duration::from_secs(60),
        ..P2pConfig::default()
    }
}

fn node(seed: &str, chain_id: u32, g: GenesisId) -> LocalNode {
    let (sk, _) = cb_crypto::keypair_from_seed(seed.as_bytes());
    LocalNode::new(sk, chain_id, g, format!("cb-node/test/{seed}"))
}

/// Poll `f` until it is true or the deadline passes. Networking tests need a
/// deadline, not a sleep: a fixed sleep is either flaky or slow, and usually
/// both.
async fn until<F: FnMut() -> bool>(label: &str, mut f: F) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for: {label}");
}

async fn head_of(handle: &P2pHandle) -> usize {
    handle
        .stats()
        .await
        .map(|s| s.connected_peers)
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_handshake_and_sync_over_tcp() {
    let g = genesis();

    let mut a_chain = MemChain::new();
    let (hash, height) = a_chain.produce();
    let a_chain = Arc::new(Mutex::new(a_chain));
    let a = spawn(cfg(vec![]), node("tcp-a", 1, g), a_chain.clone())
        .await
        .expect("spawn a");
    let a_addr = a.listen_addr().expect("bound");

    let b_chain = Arc::new(Mutex::new(MemChain::new()));
    let b = spawn(cfg(vec![a_addr]), node("tcp-b", 1, g), b_chain.clone())
        .await
        .expect("spawn b");

    until("handshake", || {
        futures::executor::block_on(head_of(&b)) == 1
    })
    .await;

    // Catch-up over a real socket: B learned A's head in the handshake and
    // pulled the range it was missing.
    until("b catches up", || {
        b_chain.lock().expect("lock").head_height() == height
    })
    .await;

    // And a freshly produced block propagates by announcement.
    let (hash2, height2) = {
        let mut c = a_chain.lock().expect("lock");
        c.produce()
    };
    assert_ne!(hash, hash2);
    a.announce_block(hash2, height2);
    until("b receives the gossiped block", || {
        b_chain.lock().expect("lock").head_height() == height2
    })
    .await;

    a.shutdown();
    b.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_locally_submitted_transaction_can_be_fetched_by_a_peer() {
    // Regression: the handle used to announce a transaction by hash alone,
    // leaving the body only in whatever store happened to have it. A
    // transaction that arrived through the RPC door was therefore announced
    // and then unservable — every peer waited out a request timeout and scored
    // the submitting node down for it. The body travels with the hash.
    let g = genesis();
    let a_chain = Arc::new(Mutex::new(MemChain::new()));
    let a = spawn(cfg(vec![]), node("tcp-tx-a", 1, g), a_chain.clone())
        .await
        .expect("spawn a");
    let a_addr = a.listen_addr().expect("bound");

    let b_chain = Arc::new(Mutex::new(MemChain::new()));
    let b = spawn(cfg(vec![a_addr]), node("tcp-tx-b", 1, g), b_chain.clone())
        .await
        .expect("spawn b");

    until("handshake", || {
        futures::executor::block_on(head_of(&b)) == 1
    })
    .await;

    // Deliberately NOT inserted into A's chain first: this is what an RPC
    // submission looks like from the gossip layer's point of view.
    let body = b"an order submitted to node a".to_vec();
    let hash = cb_crypto::keccak256(&body);
    assert!(a_chain.lock().expect("lock").tx_bytes(&hash).is_none());
    a.announce_tx(hash, body.clone());

    until("b fetches the body", || {
        b_chain.lock().expect("lock").tx_bytes(&hash).is_some()
    })
    .await;
    assert_eq!(
        b_chain.lock().expect("lock").tx_bytes(&hash),
        Some(body),
        "the body b fetched must be the one a announced"
    );

    a.shutdown();
    b.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_on_another_chain_is_refused_over_tcp() {
    let g = genesis();
    let a_chain = Arc::new(Mutex::new(MemChain::new()));
    let a = spawn(cfg(vec![]), node("tcp-chain-a", 1, g), a_chain)
        .await
        .expect("spawn a");
    let a_addr = a.listen_addr().expect("bound");

    let b_chain = Arc::new(Mutex::new(MemChain::new()));
    // Same genesis fingerprint, different chain id.
    let b = spawn(cfg(vec![a_addr]), node("tcp-chain-b", 2, g), b_chain)
        .await
        .expect("spawn b");

    // Give the handshake several attempts' worth of time to fail.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(
        b.stats().await.map(|s| s.connected_peers),
        Some(0),
        "a node on another chain must not end up in the peer table"
    );
    assert_eq!(a.stats().await.map(|s| s.connected_peers), Some(0));

    a.shutdown();
    b.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_absurd_frame_length_is_refused_before_it_is_allocated() {
    let g = genesis();
    let chain = Arc::new(Mutex::new(MemChain::new()));
    let a = spawn(cfg(vec![]), node("tcp-frame", 1, g), chain)
        .await
        .expect("spawn");
    let addr = a.listen_addr().expect("bound");

    let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
    // Four bytes claiming a 4 GiB message. The node must close rather than
    // reserve the buffer.
    sock.write_all(&u32::MAX.to_be_bytes())
        .await
        .expect("write");
    sock.flush().await.expect("flush");

    let mut buf = [0u8; 64];
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            Instant::now() < deadline,
            "node never closed the connection"
        );
        match tokio::time::timeout(Duration::from_millis(500), sock.read(&mut buf)).await {
            Ok(Ok(0)) => break,    // clean close: what we expect
            Ok(Err(_)) => break,   // reset: also a close
            Ok(Ok(_)) => continue, // the node's own hello, still draining
            Err(_) => continue,    // no data yet
        }
    }

    a.shutdown();
}

// ------------------------------------------------------ raw-socket peers

/// A signed `Hello` frame, length prefix included, as a raw peer sends it.
fn hello_frame(seed: &str, chain_id: u32, g: GenesisId, head: u64) -> Vec<u8> {
    use cb_net::p2p::message::{Hello, Message};
    let (sk, _) = cb_crypto::keypair_from_seed(seed.as_bytes());
    let hello = Hello::signed(
        &sk,
        cb_net::p2p::PROTOCOL_VERSION,
        chain_id,
        g,
        format!("raw/{seed}"),
        head,
        None,
        1,
    )
    .expect("sign");
    cb_net::p2p::codec::encode_frame(&Message::Hello(hello))
        .expect("frame")
        .to_vec()
}

fn frame(msg: &cb_net::p2p::message::Message) -> Vec<u8> {
    cb_net::p2p::codec::encode_frame(msg)
        .expect("frame")
        .to_vec()
}

/// Read one frame from a raw socket. `None` on close or after `wait`.
async fn read_msg(
    sock: &mut tokio::net::TcpStream,
    wait: Duration,
) -> Option<cb_net::p2p::message::Message> {
    let mut len = [0u8; 4];
    tokio::time::timeout(wait, sock.read_exact(&mut len))
        .await
        .ok()?
        .ok()?;
    let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
    tokio::time::timeout(wait, sock.read_exact(&mut body))
        .await
        .ok()?
        .ok()?;
    cb_net::p2p::codec::decode(&body).ok()
}

/// Whether the node has closed its end: reads until EOF or reset, skipping
/// whatever it sent first, within `wait`.
async fn closed_within(sock: &mut tokio::net::TcpStream, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    let mut buf = vec![0u8; 64 * 1024];
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), sock.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => return true,
            Ok(Ok(_)) | Err(_) => continue,
        }
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peers_the_engine_disconnected_do_not_keep_their_inbound_slots() {
    // Finding: the engine's disconnect dropped the writer and nothing else.
    // The reader, past the handshake with no timeout, kept reading from a
    // socket the remote end simply never closed, and held its inbound slot.
    // Two slots here; each is taken by a peer the engine throws out — one at
    // the handshake, one that says `Disconnect` itself — and both then sit
    // on an open socket. The idle timeout is an hour, so only the teardown
    // can free the slots for the third connection.
    let g = genesis();
    let mut c = cfg(vec![]);
    c.max_peers = 2;
    c.target_outbound = 2;
    c.peer_timeout = Duration::from_secs(3_600);
    let chain = Arc::new(Mutex::new(MemChain::new()));
    let node = spawn(c, node("tcp-slots", 1, g), chain)
        .await
        .expect("spawn");
    let addr = node.listen_addr().expect("bound");

    // Rejected at the handshake: wrong genesis.
    let mut rejected = tokio::net::TcpStream::connect(addr).await.expect("connect");
    rejected
        .write_all(&hello_frame(
            "raw-wrong-genesis",
            1,
            GenesisId(H256([1; 32])),
            0,
        ))
        .await
        .expect("hello");
    // Accepted, then leaves politely but keeps the socket.
    let mut polite = tokio::net::TcpStream::connect(addr).await.expect("connect");
    polite
        .write_all(&hello_frame("raw-polite", 1, g, 0))
        .await
        .expect("hello");
    polite
        .write_all(&frame(&cb_net::p2p::message::Message::Disconnect(
            cb_net::p2p::message::DisconnectReason::Shutdown,
        )))
        .await
        .expect("disconnect");

    assert!(
        closed_within(&mut rejected, Duration::from_secs(5)).await,
        "the rejected peer's connection is closed by the node"
    );
    assert!(
        closed_within(&mut polite, Duration::from_secs(5)).await,
        "the peer that said goodbye is closed by the node"
    );

    // Both are still holding their sockets open. A third connection must
    // nevertheless get a slot, which it can tell by being greeted.
    let third_greeted = {
        let mut ok = false;
        for _ in 0..20 {
            let mut third = tokio::net::TcpStream::connect(addr).await.expect("connect");
            if let Some(cb_net::p2p::message::Message::Hello(_)) =
                read_msg(&mut third, Duration::from_secs(1)).await
            {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        ok
    };
    assert!(third_greeted, "the freed slots are available to a new peer");
    drop((rejected, polite));
    node.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_that_stops_reading_is_dropped_while_an_honest_peer_gets_everything() {
    // Per-peer writer queues were unbounded: a peer that completed the
    // handshake and then stopped reading made this node buffer every
    // broadcast for it forever. The queue is now bounded, and the engine
    // drops such a peer rather than waiting on it — so the honest peer next
    // to it still receives every consensus message, and the engine keeps
    // answering throughout.
    let g = genesis();
    let chain = Arc::new(Mutex::new(MemChain::new()));
    let a = spawn(cfg(vec![]), node("tcp-slow-a", 1, g), chain)
        .await
        .expect("spawn a");
    let a_addr = a.listen_addr().expect("bound");

    let b_chain = Arc::new(Mutex::new(MemChain::new()));
    let b = spawn(cfg(vec![a_addr]), node("tcp-slow-b", 1, g), b_chain)
        .await
        .expect("spawn b");
    let (sink_tx, mut sink_rx) = tokio::sync::mpsc::unbounded_channel();
    b.set_consensus_sink(sink_tx);

    let mut slow = tokio::net::TcpStream::connect(a_addr)
        .await
        .expect("connect");
    slow.write_all(&hello_frame("raw-slow", 1, g, 0))
        .await
        .expect("hello");
    until("both peers handshaken", || {
        futures::executor::block_on(head_of(&a)) == 2
    })
    .await;

    // 192 MiB of proposals: well past the slow peer's queue budget plus
    // whatever the kernel buffers for it, in paced bursts that an honest
    // reader on loopback keeps up with.
    const MESSAGES: usize = 192;
    let payload = vec![0x5au8; 1024 * 1024];
    for i in 0..MESSAGES {
        a.broadcast_consensus(payload.clone());
        if i % 8 == 7 {
            let asked = Instant::now();
            assert!(a.stats().await.is_some(), "the engine is answering");
            assert!(
                asked.elapsed() < Duration::from_secs(2),
                "the engine was blocked for {:?}",
                asked.elapsed()
            );
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }

    let mut received = 0usize;
    let deadline = Instant::now() + Duration::from_secs(30);
    while received < MESSAGES && Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(1), sink_rx.recv()).await {
            Ok(Some(m)) => {
                assert_eq!(m.bytes.len(), payload.len());
                received += 1;
            }
            _ => continue,
        }
    }
    assert_eq!(
        received, MESSAGES,
        "the honest peer received every broadcast"
    );
    until("the slow peer is dropped", || {
        futures::executor::block_on(head_of(&a)) == 1
    })
    .await;
    drop(slow);
    a.shutdown();
    b.shutdown();
}
