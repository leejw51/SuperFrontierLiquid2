//! The TCP listener outlives a connection that dies under it.
//!
//! `serve` used to `?` both `accept` and `set_nodelay`, so one failure ended
//! the loop and the node served no RPC until restarted. Descriptor exhaustion
//! (EMFILE) cannot be arranged portably in a test; what can be is the other
//! half — clients that connect and vanish at once, abruptly, before and while
//! the server is handling them — followed by a real call that must still be
//! answered. It proves the loop keeps going past dead sockets, not that it
//! survives EMFILE, which is argued from the code.

use cb_rpc::server::SequencerServer;
use cb_state::{Database, Meta};
use std::sync::Arc;
use tempfile::tempdir;
use tokio::sync::Notify;
use tokio::task::LocalSet;

#[test]
fn the_listener_answers_after_clients_that_vanished() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(7);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    LocalSet::new().block_on(&rt, async move {
        // Port 0: never a port anything else on this machine is using.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = SequencerServer::without_voting(db, Arc::new(Notify::new()));
        tokio::task::spawn_local(cb_rpc::serve_on(listener, server));

        for _ in 0..32 {
            let socket = tokio::net::TcpSocket::new_v4().unwrap();
            // Linger zero: close with a reset, not a FIN.
            #[allow(deprecated)] // SO_LINGER=0 is the point: it makes the close a reset.
            socket.set_linger(Some(std::time::Duration::ZERO)).unwrap();
            let stream = socket.connect(addr).await.unwrap();
            drop(stream);
        }
        tokio::task::yield_now().await;

        let client = cb_rpc::connect(addr)
            .await
            .expect("the listener is still up");
        let reply = client
            .cap
            .chain_info_request()
            .send()
            .promise
            .await
            .expect("answered");
        let info = reply.get().unwrap().get_info().unwrap();
        assert_eq!(info.get_chain_id(), 7);
    });
}
