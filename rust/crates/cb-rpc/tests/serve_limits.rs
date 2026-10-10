//! The listener's limits: how many connections it holds, how long it keeps
//! an idle one, and how large a message it will read.
//!
//! What this proves, over real sockets on port 0: one connection past
//! `max_connections` is closed while the others keep working, and a slot
//! frees when a client leaves (for the single-runtime server and for the
//! worker pool, where the count spans every worker); a connection with no
//! traffic either way is closed after `idle_timeout` while one that keeps
//! talking is not; and a message past `max_message_words`, a stream of
//! garbage, or a frame header claiming gigabytes costs only that connection.

use cb_rpc::server::SequencerServer;
use cb_rpc::ServeConfig;
use cb_state::{Database, Meta};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::tempdir;
use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;
use tokio::task::LocalSet;

fn db() -> (tempfile::TempDir, Database) {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(7);
    (dir, db)
}

fn run_local<F, Fut, R>(f: F) -> R
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = R>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    LocalSet::new().block_on(&rt, f())
}

async fn serve(db: Database, cfg: ServeConfig) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = SequencerServer::without_voting(db, Arc::new(Notify::new()));
    tokio::task::spawn_local(cb_rpc::serve_on_with(listener, server, cfg));
    addr
}

/// Whether a `chainInfo` on this client is answered, within a second.
async fn answers(c: &cb_rpc::Client) -> bool {
    let call = c.cap.chain_info_request().send().promise;
    matches!(
        tokio::time::timeout(Duration::from_secs(1), call).await,
        Ok(Ok(r)) if r.get().unwrap().get_info().unwrap().get_chain_id() == 7
    )
}

/// Connect and make one call, retrying for up to two seconds: a freed slot
/// is released when the server notices the close, which takes a moment.
async fn connect_when_free(addr: SocketAddr) -> cb_rpc::Client {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let c = cb_rpc::connect(addr).await.unwrap();
        if answers(&c).await {
            return c;
        }
        assert!(Instant::now() < deadline, "no slot came free");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn cfg(max_connections: usize, idle: Option<Duration>) -> ServeConfig {
    ServeConfig {
        max_connections,
        idle_timeout: idle,
        ..ServeConfig::default()
    }
}

#[test]
fn defaults_are_bounded() {
    let d = ServeConfig::default();
    assert_eq!(d.max_connections, ServeConfig::DEFAULT_MAX_CONNECTIONS);
    assert_eq!(d.idle_timeout, Some(ServeConfig::DEFAULT_IDLE_TIMEOUT));
    assert!(
        d.max_message_words < 8 * 1024 * 1024,
        "below capnp's 64 MB default"
    );
}

#[test]
fn one_connection_past_the_limit_is_closed_and_a_slot_frees_on_leave() {
    let (_d, db) = db();
    run_local(|| async move {
        let addr = serve(db, cfg(2, None)).await;
        // A bare socket holds a slot like any client. (It is used here
        // because dropping a `cb_rpc::Client` detaches its connection task
        // rather than closing the socket.)
        let a = tokio::net::TcpStream::connect(addr).await.unwrap();
        let b = cb_rpc::connect(addr).await.unwrap();
        assert!(answers(&b).await);
        let c = cb_rpc::connect(addr).await.unwrap();
        assert!(!answers(&c).await, "the third connection is refused");
        // The held connection is unaffected.
        assert!(answers(&b).await);
        drop(a);
        let d = connect_when_free(addr).await;
        assert!(answers(&d).await && answers(&b).await);
    });
}

#[test]
fn the_worker_pool_counts_connections_across_workers() {
    let (_d, db) = db();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = SequencerServer::without_voting(db, Arc::new(Notify::new()));
    let _threads = cb_rpc::run_local_serve_multi_on(listener, server, 3, cfg(2, None)).unwrap();
    run_local(|| async move {
        let a = cb_rpc::connect(addr).await.unwrap();
        let b = tokio::net::TcpStream::connect(addr).await.unwrap();
        assert!(answers(&a).await);
        // Give the accept thread a moment to hand `b` to its worker.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let c = cb_rpc::connect(addr).await.unwrap();
        assert!(!answers(&c).await, "the limit spans the workers");
        drop(b);
        let d = connect_when_free(addr).await;
        assert!(answers(&a).await && answers(&d).await);
    });
}

#[test]
fn an_idle_connection_is_closed_and_a_talking_one_is_not() {
    let (_d, db) = db();
    run_local(|| async move {
        let addr = serve(db, cfg(16, Some(Duration::from_millis(200)))).await;
        let idle = cb_rpc::connect(addr).await.unwrap();
        let busy = cb_rpc::connect(addr).await.unwrap();
        assert!(answers(&idle).await && answers(&busy).await);
        // `busy` calls every 60 ms for 600 ms; `idle` says nothing.
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(60)).await;
            assert!(answers(&busy).await, "a connection in use is kept");
        }
        assert!(!answers(&idle).await, "the idle connection was closed");
        // And the listener still serves new ones.
        let fresh = cb_rpc::connect(addr).await.unwrap();
        assert!(answers(&fresh).await);
    });
}

/// A block subscriber parked on `next()` over a quiet chain moves no bytes,
/// and is still not idle: the connection is kept while the call waits.
#[test]
fn a_subscriber_waiting_for_a_block_is_not_idle() {
    let (_d, db) = db();
    run_local(|| async move {
        let addr = serve(db, cfg(16, Some(Duration::from_millis(200)))).await;
        let c = cb_rpc::connect(addr).await.unwrap();
        let mut req = c.cap.stream_blocks_request();
        req.get().set_from_height(1);
        let sub = req
            .send()
            .promise
            .await
            .unwrap()
            .get()
            .unwrap()
            .get_sub()
            .unwrap();
        // Parked: there is no block 1 and none will come.
        let pending = sub.next_request().send().promise;
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(answers(&c).await, "a waiting subscriber is kept");
        drop(pending);
        drop(sub);
        // Once nothing waits and nothing moves, it is idle again.
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(!answers(&c).await, "closed once nothing waits");
    });
}

#[test]
fn no_idle_timeout_keeps_a_quiet_connection() {
    let (_d, db) = db();
    run_local(|| async move {
        let addr = serve(db, cfg(16, None)).await;
        let c = cb_rpc::connect(addr).await.unwrap();
        assert!(answers(&c).await);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(answers(&c).await);
    });
}

#[test]
fn an_oversized_message_costs_only_its_connection() {
    let (_d, db) = db();
    run_local(|| async move {
        let addr = serve(
            db,
            ServeConfig {
                max_message_words: 512,
                ..cfg(16, None)
            },
        )
        .await;
        let big = cb_rpc::connect(addr).await.unwrap();
        let mut req = big.cap.submit_tx_request();
        {
            let mut t = req.get().init_tx().init_tx();
            t.set_chain_id(7);
            let mut w = t.init_body().init_withdraw();
            // 64 KB of text: 8,192 words, past the 512-word limit.
            w.set_external_address("z".repeat(64 * 1024));
        }
        let r = tokio::time::timeout(Duration::from_secs(2), req.send().promise).await;
        assert!(
            matches!(r, Ok(Err(_))),
            "the oversized request fails its connection"
        );
        let ok = cb_rpc::connect(addr).await.unwrap();
        assert!(answers(&ok).await);
    });
}

#[test]
fn garbage_and_a_gigabyte_frame_header_cost_only_their_connection() {
    let (_d, db) = db();
    run_local(|| async move {
        let addr = serve(db, cfg(16, None)).await;
        // Random bytes.
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        let junk: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        let _ = s.write_all(&junk).await;
        // A frame header: one segment of 0x0fff_ffff words (2 GiB).
        let mut h = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut hdr = Vec::new();
        hdr.extend_from_slice(&0u32.to_le_bytes());
        hdr.extend_from_slice(&0x0fff_ffffu32.to_le_bytes());
        let _ = h.write_all(&hdr).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let c = cb_rpc::connect(addr).await.unwrap();
        assert!(answers(&c).await, "the listener outlives both");
    });
}
