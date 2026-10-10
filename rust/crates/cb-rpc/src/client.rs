//! Cap'n Proto client over TCP.

use capnp_rpc::{rpc_twoparty_capnp, twoparty, RpcSystem};
use cb_schema::rpc_capnp;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::task::LocalSet;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// A connected client. The RPC system is driven by a tokio task we spawn here;
/// dropping `Client` drops `_rpc_handle`, which terminates the task.
pub struct Client {
    pub cap: rpc_capnp::super_frontier_liquid::Client,
    #[allow(dead_code)]
    _rpc_handle: tokio::task::JoinHandle<Result<(), capnp::Error>>,
}

impl Client {
    /// Send a vote to the leader. Returns the high-level outcome the leader
    /// reported (accepted, quorum-formed, or rejected with a code).
    pub async fn submit_vote(
        &self,
        vote: &cb_consensus::Vote,
    ) -> anyhow::Result<crate::server::VoteAccepted> {
        let mut req = self.cap.submit_vote_request();
        crate::conv::vote_to_capnp(req.get().init_vote(), vote);
        let resp = req.send().promise.await?;
        let result = resp.get()?.get_result()?;
        match result.which()? {
            rpc_capnp::vote_result::Accepted(()) => Ok(crate::server::VoteAccepted::Stored),
            rpc_capnp::vote_result::Quorum(()) => Ok(crate::server::VoteAccepted::QuorumFormed),
            rpc_capnp::vote_result::Rejected(rej) => {
                let r = rej?;
                let msg = r.get_message()?.to_string()?;
                anyhow::bail!("vote rejected: {msg}");
            }
        }
    }
}

/// Connect inside a `LocalSet`. Most callers should use `connect` inside a running LocalSet.
pub async fn connect(addr: SocketAddr) -> anyhow::Result<Client> {
    let stream = TcpStream::connect(addr).await?;
    stream.set_nodelay(true)?;
    let (read, write) = stream.into_split();
    let read = read.compat();
    let write = write.compat_write();
    let network = Box::new(twoparty::VatNetwork::new(
        read,
        write,
        rpc_twoparty_capnp::Side::Client,
        Default::default(),
    ));
    let mut rpc_system = RpcSystem::new(network, None);
    let cap: rpc_capnp::super_frontier_liquid::Client =
        rpc_system.bootstrap(rpc_twoparty_capnp::Side::Server);
    let handle = tokio::task::spawn_local(rpc_system);
    Ok(Client {
        cap,
        _rpc_handle: handle,
    })
}

/// Spawn an RPC server on `addr`. The returned future completes only if the
/// bind fails or the task is aborted. Designed to be used inside a `LocalSet`.
pub async fn serve(addr: SocketAddr, server: crate::server::SequencerServer) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_on(listener, server).await
}

/// Limits on what one RPC listener serves.
///
/// # Why
///
/// The listener used to take every connection it was offered and keep each
/// one for as long as the peer liked. A client that opened sockets and never
/// spoke held a descriptor and a task each, for ever; enough of them and the
/// node could not accept its own gateway. Each connection's Cap'n Proto
/// reader also took the library's default limit of 64 MB per message, for
/// requests that are all under a kilobyte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServeConfig {
    /// Connections held open at once. One past this is accepted and closed
    /// straight away, so the client sees its connection reset rather than
    /// hanging in the kernel's backlog.
    pub max_connections: usize,
    /// A connection that neither sends nor receives a byte for this long,
    /// and has no call waiting on the server, is closed. Both conditions
    /// matter: a block subscriber parked on `next()` moves no bytes at all
    /// while the chain is quiet (an empty round produces no block), and it
    /// is not idle — it is waiting for the next block. `None` never closes
    /// a connection.
    pub idle_timeout: Option<std::time::Duration>,
    /// Most 8-byte words in one incoming message (Cap'n Proto's traversal
    /// limit). Requests are small; responses are sent, not read, here.
    pub max_message_words: usize,
}

impl ServeConfig {
    pub const DEFAULT_MAX_CONNECTIONS: usize = 1_024;
    pub const DEFAULT_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
    /// 1 Mi words = 8 MiB.
    pub const DEFAULT_MAX_MESSAGE_WORDS: usize = 1 << 20;
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            max_connections: Self::DEFAULT_MAX_CONNECTIONS,
            idle_timeout: Some(Self::DEFAULT_IDLE_TIMEOUT),
            max_message_words: Self::DEFAULT_MAX_MESSAGE_WORDS,
        }
    }
}

/// One held connection slot; dropping it frees the slot.
struct ConnSlot(Arc<AtomicUsize>);

impl ConnSlot {
    /// Take a slot, or `None` when all `max` are held.
    fn try_take(active: &Arc<AtomicUsize>, max: usize) -> Option<ConnSlot> {
        let mut cur = active.load(Ordering::Relaxed);
        loop {
            if cur >= max {
                return None;
            }
            match active.compare_exchange_weak(cur, cur + 1, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => return Some(ConnSlot(active.clone())),
                Err(now) => cur = now,
            }
        }
    }
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Milliseconds since `epoch` of the last byte moved either way, and how
/// many long waits (stream `next()` calls) are parked on this connection.
pub(crate) struct Activity {
    epoch: std::time::Instant,
    last_ms: AtomicU64,
    waiting: AtomicUsize,
}

tokio::task_local! {
    /// The connection whose RPC system is being polled. Method calls run
    /// inside that poll, so a server method can find its connection here.
    static CONNECTION: Arc<Activity>;
}

/// A call parked on the server, counted as activity for as long as it
/// lives. Taken by the stream subscriptions' `next()`; outside a served
/// connection (in-process clients, tests) it counts nothing.
pub(crate) struct Waiting(Option<Arc<Activity>>);

impl Waiting {
    pub(crate) fn begin() -> Self {
        Waiting(
            CONNECTION
                .try_with(|a| {
                    a.waiting.fetch_add(1, Ordering::AcqRel);
                    a.clone()
                })
                .ok(),
        )
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        if let Some(a) = &self.0 {
            a.touch();
            a.waiting.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl Activity {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            epoch: std::time::Instant::now(),
            last_ms: AtomicU64::new(0),
            waiting: AtomicUsize::new(0),
        })
    }
    fn touch(&self) {
        self.last_ms
            .store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
    }
    fn idle_for(&self) -> std::time::Duration {
        let now = self.epoch.elapsed().as_millis() as u64;
        std::time::Duration::from_millis(now.saturating_sub(self.last_ms.load(Ordering::Relaxed)))
    }
}

/// A socket half that records when it last moved a byte.
struct Tracked<T> {
    inner: T,
    activity: Arc<Activity>,
}

impl<T: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Tracked<T> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let r = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if buf.filled().len() > before {
            self.activity.touch();
        }
        r
    }
}

impl<T: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Tracked<T> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let r = std::pin::Pin::new(&mut self.inner).poll_write(cx, buf);
        if matches!(r, std::task::Poll::Ready(Ok(n)) if n > 0) {
            self.activity.touch();
        }
        r
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Drive one accepted connection to its end: the peer hangs up, the RPC
/// system fails, or the connection sits idle past `cfg.idle_timeout`.
/// `_slot` is held for exactly that long. Must run inside a `LocalSet`.
async fn serve_connection(
    stream: TcpStream,
    server: crate::server::SequencerServer,
    cfg: ServeConfig,
    _slot: ConnSlot,
) {
    let activity = Activity::new();
    activity.touch();
    let (read, write) = stream.into_split();
    let read = Tracked {
        inner: read,
        activity: activity.clone(),
    };
    let write = Tracked {
        inner: write,
        activity: activity.clone(),
    };
    let mut opts = capnp::message::ReaderOptions::new();
    opts.traversal_limit_in_words(Some(cfg.max_message_words));
    let network = Box::new(twoparty::VatNetwork::new(
        read.compat(),
        write.compat_write(),
        rpc_twoparty_capnp::Side::Server,
        opts,
    ));
    let cap: rpc_capnp::super_frontier_liquid::Client = capnp_rpc::new_client(server);
    let rpc = RpcSystem::new(network, Some(cap.client));
    let rpc = CONNECTION.scope(activity.clone(), rpc);
    match cfg.idle_timeout {
        None => {
            let _ = rpc.await;
        }
        Some(limit) => {
            let watchdog = async {
                loop {
                    if activity.waiting.load(Ordering::Acquire) > 0 {
                        tokio::time::sleep(limit).await;
                        continue;
                    }
                    let idle = activity.idle_for();
                    if idle >= limit {
                        return;
                    }
                    tokio::time::sleep(limit - idle).await;
                }
            };
            tokio::select! {
                _ = rpc => {}
                _ = watchdog => {
                    tracing::debug!(?limit, "rpc connection idle; closing it");
                }
            }
        }
    }
}

/// Log a refused connection, at most once a second per listener.
fn log_refused(last: &mut Option<std::time::Instant>, max: usize) {
    let now = std::time::Instant::now();
    if last
        .map(|t| now.duration_since(t).as_secs() >= 1)
        .unwrap_or(true)
    {
        *last = Some(now);
        tracing::warn!(max, "rpc connection limit reached; refusing a connection");
    }
}

/// Serve an already-bound listener, for callers that need the port first,
/// with the default [`ServeConfig`].
pub async fn serve_on(
    listener: tokio::net::TcpListener,
    server: crate::server::SequencerServer,
) -> anyhow::Result<()> {
    serve_on_with(listener, server, ServeConfig::default()).await
}

/// Serve an already-bound listener under `cfg`.
///
/// A failed `accept` is logged and waited out rather than returned. It used to
/// end the loop, and with it the listener, for good: running out of file
/// descriptors — which anyone can arrange by opening connections — or a peer
/// that reset between `accept` and `set_nodelay` left the node with no RPC
/// until it was restarted. The pause keeps an exhausted descriptor table from
/// turning the loop into a busy spin.
pub async fn serve_on_with(
    listener: tokio::net::TcpListener,
    server: crate::server::SequencerServer,
    cfg: ServeConfig,
) -> anyhow::Result<()> {
    let active = Arc::new(AtomicUsize::new(0));
    let mut last_refusal_log = None;
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                tracing::warn!("rpc accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
        };
        let Some(slot) = ConnSlot::try_take(&active, cfg.max_connections) else {
            log_refused(&mut last_refusal_log, cfg.max_connections);
            drop(stream);
            continue;
        };
        if let Err(e) = stream.set_nodelay(true) {
            // A socket that cannot take the option is usually already reset;
            // serving it costs nothing and failing it costs the listener.
            tracing::debug!("rpc could not set nodelay: {e}");
        }
        tokio::task::spawn_local(serve_connection(stream, server.clone(), cfg, slot));
    }
}

/// Serve `addr` from a pool of single-threaded runtimes, one connection pinned
/// to one runtime for its lifetime.
///
/// # Why a pool
///
/// Cap'n Proto's generated client is `!Send`, so a connection cannot migrate
/// between threads and the whole server used to live on the node's one
/// `LocalSet` — the same one that builds blocks. A profile of a loaded
/// sequencer put 35% of the process inside `ecdsa_recover`, called once per
/// `submitTx` to learn who signed a transaction, and every one of those
/// recoveries ran on the thread that was also trying to produce blocks. The
/// node held roughly one core of the ten it had.
///
/// A connection never needs to move; only new connections need distributing.
/// So one thread accepts and hands each accepted socket to the next worker in
/// turn, and each worker drives its own connections on its own runtime.
/// Signature recovery then runs on as many cores as there are clients, and
/// block production runs on a thread of its own.
///
/// The listener is bound here, on the caller's thread, so a port already in
/// use is an error the caller gets back rather than a message in a log.
/// Accept-loop failures afterwards are logged and retried: one refused socket
/// should not take the node down.
pub fn run_local_serve_multi(
    addr: SocketAddr,
    server: crate::server::SequencerServer,
    threads: usize,
) -> anyhow::Result<Vec<std::thread::JoinHandle<()>>> {
    run_local_serve_multi_with(addr, server, threads, ServeConfig::default())
}

/// [`run_local_serve_multi`] under `cfg`. The connection limit is one count
/// across every worker.
pub fn run_local_serve_multi_with(
    addr: SocketAddr,
    server: crate::server::SequencerServer,
    threads: usize,
    cfg: ServeConfig,
) -> anyhow::Result<Vec<std::thread::JoinHandle<()>>> {
    run_local_serve_multi_on(std::net::TcpListener::bind(addr)?, server, threads, cfg)
}

/// [`run_local_serve_multi_with`] on a listener the caller bound, for callers
/// that need the port first (tests bind port 0).
pub fn run_local_serve_multi_on(
    listener: std::net::TcpListener,
    server: crate::server::SequencerServer,
    threads: usize,
    cfg: ServeConfig,
) -> anyhow::Result<Vec<std::thread::JoinHandle<()>>> {
    let threads = threads.max(1);
    listener.set_nonblocking(true)?;

    let mut senders = Vec::with_capacity(threads);
    let mut handles = Vec::with_capacity(threads + 1);
    for i in 0..threads {
        let (tx, mut rx) =
            tokio::sync::mpsc::unbounded_channel::<(std::net::TcpStream, ConnSlot)>();
        senders.push(tx);
        let server = server.clone();
        handles.push(
            std::thread::Builder::new()
                .name(format!("cb-rpc-{i}"))
                .spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("build rpc worker runtime");
                    let local = LocalSet::new();
                    rt.block_on(local.run_until(async move {
                        while let Some((std_stream, slot)) = rx.recv().await {
                            let stream = match std_stream
                                .set_nonblocking(true)
                                .and_then(|()| tokio::net::TcpStream::from_std(std_stream))
                            {
                                Ok(s) => s,
                                Err(e) => {
                                    tracing::warn!("rpc worker could not adopt a socket: {e}");
                                    continue;
                                }
                            };
                            tokio::task::spawn_local(serve_connection(
                                stream,
                                server.clone(),
                                cfg,
                                slot,
                            ));
                        }
                    }));
                })?,
        );
    }

    handles.push(
        std::thread::Builder::new()
            .name("cb-rpc-accept".to_string())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build rpc accept runtime");
                rt.block_on(async move {
                    let listener = match tokio::net::TcpListener::from_std(listener) {
                        Ok(l) => l,
                        Err(e) => {
                            tracing::error!("rpc listener could not be adopted: {e}");
                            return;
                        }
                    };
                    let mut next = 0usize;
                    let active = Arc::new(AtomicUsize::new(0));
                    let mut last_refusal_log = None;
                    loop {
                        let (stream, _peer) = match listener.accept().await {
                            Ok(p) => p,
                            Err(e) => {
                                // Paused, not retried at once: under an
                                // exhausted descriptor table every accept
                                // fails, and an immediate retry is a spin
                                // that floods the log.
                                tracing::warn!("rpc accept failed: {e}");
                                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                                continue;
                            }
                        };
                        let Some(slot) = ConnSlot::try_take(&active, cfg.max_connections) else {
                            log_refused(&mut last_refusal_log, cfg.max_connections);
                            drop(stream);
                            continue;
                        };
                        if let Err(e) = stream.set_nodelay(true) {
                            tracing::warn!("rpc could not set nodelay: {e}");
                        }
                        let std_stream = match stream.into_std() {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::warn!("rpc could not detach a socket: {e}");
                                continue;
                            }
                        };
                        // Round robin. Connections are long-lived and few, so
                        // anything cleverer would be measuring noise.
                        if senders[next % senders.len()]
                            .send((std_stream, slot))
                            .is_err()
                        {
                            tracing::error!("an rpc worker is gone; dropping the connection");
                        }
                        next = next.wrapping_add(1);
                    }
                });
            })?,
    );

    Ok(handles)
}

/// Convenience: spawn `serve` on a `LocalSet` running on the current tokio runtime.
/// Returns a join handle to a spawned blocking thread that runs its own single-threaded runtime.
pub fn run_local_serve(
    addr: SocketAddr,
    server: crate::server::SequencerServer,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build single-thread rt");
        let local = LocalSet::new();
        rt.block_on(local.run_until(async move {
            if let Err(e) = serve(addr, server).await {
                tracing::error!("rpc serve error: {e}");
            }
        }));
    })
}
