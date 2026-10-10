//! The `!Send` bridge: a dedicated thread, a `LocalSet`, and a channel.
//!
//! ## Shape
//!
//! ```text
//!   axum worker threads                 the RPC thread
//!   ───────────────────                 ──────────────
//!   NodeBridge::account(addr)
//!        │  Request{ call, reply }
//!        ├──────────── mpsc ───────────▶ session loop
//!        │                                   │ spawn_local
//!        │                                   ▼
//!        │                              capnp promise (!Send)
//!        │                                   │
//!        ◀──────────── oneshot ──────────────┘ owned reply
//! ```
//!
//! Requests are `spawn_local`-ed rather than awaited inline, so one slow RPC
//! does not stall every other request queued behind it.
//!
//! ## Surviving a node restart
//!
//! [`Supervisor`] runs sessions in a loop. A session ends when the connection
//! drops, the block stream errors, or the RPC system exits; the supervisor
//! then waits out an exponential backoff and starts another. While no session
//! is live the shared [`NodeHealth`] says so, and [`NodeBridge`] fails calls
//! immediately with [`PortError::Unavailable`] instead of parking them on a
//! channel nobody is reading. That is the difference between a `503` the
//! browser can show and a spinner that never resolves.
//!
//! Session establishment sits behind the [`Connector`] trait so the reconnect
//! policy can be tested — including the case where the node is down for
//! several attempts — without a sequencer anywhere near the test.

use super::types::*;
use cb_ports::{PortError, PortResult};
use cb_tx::SignedTx;
use cb_types::{Address, DerivId, MarketId};
use futures::future::LocalBoxFuture;
use parking_lot::RwLock;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot};

/// One request/response pair over the channel to the RPC thread.
#[derive(Debug)]
pub struct Request {
    call: Call,
    reply: oneshot::Sender<PortResult<Reply>>,
}

impl Request {
    /// What the caller asked for.
    pub fn call(&self) -> &Call {
        &self.call
    }

    /// Answer the request. A dropped receiver — the caller timed out — is not
    /// an error worth reporting.
    pub fn answer(self, reply: PortResult<Reply>) {
        let _ = self.reply.send(reply);
    }
}

/// Every RPC the gateway makes, as data.
///
/// Keeping the calls as an enum rather than one channel per method means the
/// session loop has a single place to apply timeouts, tracing and error
/// mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// `chainInfo`.
    ChainInfo,
    /// `getAccount`.
    Account(Address),
    /// `submitTx`.
    SubmitTx(Box<SignedTx>),
    /// `getOrderBook`.
    SpotBook(MarketId, u32),
    /// `getPerpOrderBook`.
    DerivBook(DerivId, u32),
    /// `getPositions`.
    Positions(Address),
    /// `getMarginAccount`.
    MarginAccount(Address),
    /// `getMarkPrice`.
    MarkPrice(DerivId),
    /// `getOraclePrice`.
    OraclePrice(DerivId),
    /// `getFundingIndex`.
    FundingIndex(DerivId),
    /// `listPerpMarkets`.
    PerpMarkets,
    /// `listFutureMarkets`.
    FutureMarkets,
    /// `listSpotMarkets`.
    SpotMarkets,
    /// `getBlock`.
    Block(u64),
}

/// Every RPC response, as owned data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// Chain identity.
    ChainInfo(ChainInfo),
    /// Account state.
    Account(AccountSnapshot),
    /// Submission outcome.
    Submit(SubmitOutcome),
    /// Book depth.
    Book(BookSnapshot),
    /// Open positions.
    Positions(Vec<PositionSnapshot>),
    /// Margin pool.
    Margin(MarginSnapshot),
    /// Mark price.
    Mark(MarkSnapshot),
    /// Oracle price.
    Oracle(OracleSnapshot),
    /// Funding index.
    Funding(FundingSnapshot),
    /// Listed perpetuals.
    PerpMarkets(Vec<PerpMarketSnapshot>),
    /// Listed futures.
    FutureMarkets(Vec<FutureMarketSnapshot>),
    /// Listed spot markets.
    SpotMarkets(Vec<SpotMarketSnapshot>),
    /// One block.
    Block(Box<BlockSummary>),
}

/// Shared state a session needs, none of which is `!Send`.
pub struct SessionCtx {
    /// The request queue, shared across sessions so a reconnect does not lose
    /// callers that are already waiting.
    pub requests: Rc<RefCell<mpsc::Receiver<Request>>>,
    /// Fan-out channel for node events.
    pub events: broadcast::Sender<NodeEvent>,
    /// Link state, read by `/health` and by every outgoing call.
    pub health: Arc<RwLock<NodeHealth>>,
    /// Next block height to stream from. Survives a reconnect so a restart
    /// resumes rather than replaying.
    pub next_height: Arc<AtomicU64>,
    /// Set when the gateway is shutting down.
    pub stopping: Arc<AtomicBool>,
}

impl SessionCtx {
    /// Mark the link up.
    pub fn mark_connected(&self, chain_id: u32) {
        let mut health = self.health.write();
        health.connected = true;
        health.chain_id = Some(chain_id);
        health.last_error = None;
    }

    /// Reconcile where streaming starts with what the node can still serve.
    ///
    /// A gateway configured to replay from height 1 against a node that
    /// prunes block bodies used to ask for a body that no longer existed. The
    /// node had nothing to send and nothing to refuse, so the gateway sat
    /// connected, healthy and at height zero for as long as it ran. Now the
    /// start is floored to the oldest body the node holds, the head is
    /// recorded so `synced` can mean something, and the truncated replay is
    /// logged rather than hidden.
    ///
    /// Returns the height the stream will start from.
    pub fn reconcile_start(&self, info: &ChainInfo) -> u64 {
        {
            let mut health = self.health.write();
            health.head_height = health.head_height.max(info.head_height);
        }
        let wanted = self.next_height.load(Ordering::SeqCst);
        if info.oldest_block > wanted {
            tracing::warn!(
                target: "cb_gateway::node",
                wanted,
                oldest = info.oldest_block,
                head = info.head_height,
                "the node has pruned the blocks a replay from `wanted` needs; streaming from \
                 the oldest body it still holds. Candles and counters start there."
            );
            self.next_height.store(info.oldest_block, Ordering::SeqCst);
            return info.oldest_block;
        }
        wanted
    }

    /// Mark the link down, recording why.
    pub fn mark_disconnected(&self, error: Option<String>) {
        let mut health = self.health.write();
        health.connected = false;
        if error.is_some() {
            health.last_error = error;
        }
    }

    /// Record the highest height seen.
    pub fn observe_height(&self, height: u64) {
        let mut health = self.health.write();
        if height > health.last_height {
            health.last_height = height;
        }
        if height > health.head_height {
            health.head_height = height;
        }
    }
}

/// Establishes and drives one RPC session.
///
/// Implementations run entirely on the RPC thread and may return `!Send`
/// futures. A session returns when the link ends; returning `Ok(())` means a
/// clean shutdown, `Err` means the supervisor should reconnect.
pub trait Connector: Send + Sync + 'static {
    /// A human-readable name for logs.
    fn describe(&self) -> String;

    /// Run one session to completion.
    fn run_session(&self, ctx: Rc<SessionCtx>) -> LocalBoxFuture<'_, Result<(), String>>;
}

/// Exponential backoff with a ceiling.
///
/// Reconnect storms are their own outage: a gateway that retries a restarting
/// sequencer every millisecond keeps it from finishing its restart.
#[derive(Debug, Clone)]
pub struct Backoff {
    base: Duration,
    max: Duration,
    attempt: u32,
}

impl Backoff {
    /// A backoff starting at `base` and doubling to at most `max`.
    pub fn new(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max,
            attempt: 0,
        }
    }

    /// The delay before the next attempt, doubling each time.
    pub fn next_delay(&mut self) -> Duration {
        let shift = self.attempt.min(16);
        let delay = self.base.saturating_mul(1u32 << shift);
        self.attempt = self.attempt.saturating_add(1);
        delay.min(self.max)
    }

    /// Forget the failure history after a successful session.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(Duration::from_millis(100), Duration::from_secs(10))
    }
}

/// How the bridge is configured.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    /// How long a single RPC may take before it is abandoned.
    pub call_timeout: Duration,
    /// Depth of the request queue.
    pub request_queue: usize,
    /// Depth of the event fan-out buffer.
    pub event_buffer: usize,
    /// Reconnect backoff.
    pub backoff: Backoff,
    /// Height to begin streaming blocks from on the first connection.
    pub start_height: u64,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            call_timeout: Duration::from_secs(5),
            request_queue: 1024,
            event_buffer: 4096,
            backoff: Backoff::default(),
            // Block 1 is the first block a chain produces; there is no block
            // 0. `BlockSubServer::next` waits on a notify for a height that
            // will never arrive, so streaming from 0 would leave the gateway
            // connected and permanently blockless.
            start_height: 1,
        }
    }
}

/// The handle every other part of the gateway holds.
pub struct NodeBridge {
    requests: mpsc::Sender<Request>,
    events: broadcast::Sender<NodeEvent>,
    health: Arc<RwLock<NodeHealth>>,
    stopping: Arc<AtomicBool>,
    call_timeout: Duration,
    thread: parking_lot::Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// Dropping the handle stops the RPC thread.
///
/// The flag is set but the thread is not joined: joining in `Drop` would block
/// whatever is tearing the gateway down, and the thread exits on its own
/// within one liveness tick. [`NodeBridge::shutdown`] is the ordered path.
impl Drop for NodeBridge {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
    }
}

impl std::fmt::Debug for NodeBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeBridge")
            .field("health", &*self.health.read())
            .finish()
    }
}

impl NodeBridge {
    /// Start the RPC thread and return a handle to it.
    ///
    /// The thread owns a current-thread runtime and a `LocalSet`; the
    /// connector's futures never leave it.
    pub fn spawn(connector: Arc<dyn Connector>, config: BridgeConfig) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(config.request_queue);
        let (events, _) = broadcast::channel(config.event_buffer);
        let health = Arc::new(RwLock::new(NodeHealth::default()));
        let stopping = Arc::new(AtomicBool::new(false));

        let thread = {
            let events = events.clone();
            let health = Arc::clone(&health);
            let stopping = Arc::clone(&stopping);
            let mut backoff = config.backoff.clone();
            let start_height = config.start_height;
            std::thread::Builder::new()
                .name("cb-gateway-rpc".into())
                .spawn(move || {
                    let runtime = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(rt) => rt,
                        Err(e) => {
                            tracing::error!(error = %e, "rpc thread runtime failed to build");
                            return;
                        }
                    };
                    let local = tokio::task::LocalSet::new();
                    let ctx = Rc::new(SessionCtx {
                        requests: Rc::new(RefCell::new(rx)),
                        events,
                        health,
                        next_height: Arc::new(AtomicU64::new(start_height)),
                        stopping,
                    });
                    local.block_on(&runtime, async move {
                        supervise(connector, ctx, &mut backoff).await;
                    });
                })
                .ok()
        };

        Arc::new(Self {
            requests: tx,
            events,
            health,
            stopping,
            call_timeout: config.call_timeout,
            thread: parking_lot::Mutex::new(thread),
        })
    }

    /// Ask the RPC thread to stop and wait for it.
    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        if let Some(handle) = self.thread.lock().take() {
            let _ = handle.join();
        }
    }

    /// Issue one call, failing fast when the link is down.
    pub(crate) async fn call(&self, call: Call) -> PortResult<Reply> {
        if !self.health.read().connected {
            return Err(PortError::Unavailable(
                "sequencer RPC is not connected".into(),
            ));
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        let request = Request {
            call,
            reply: reply_tx,
        };
        self.requests.try_send(request).map_err(|e| match e {
            // A saturated internal queue is the gateway's problem, not the
            // caller's: `429` would tell the browser to slow down when the
            // honest answer is that the sequencer link is not keeping up.
            mpsc::error::TrySendError::Full(_) => {
                PortError::Unavailable("the sequencer request queue is saturated".into())
            }
            mpsc::error::TrySendError::Closed(_) => {
                PortError::Unavailable("sequencer RPC thread has stopped".into())
            }
        })?;
        match tokio::time::timeout(self.call_timeout, reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(PortError::Unavailable(
                "sequencer RPC dropped the request".into(),
            )),
            Err(_) => Err(PortError::Timeout(self.call_timeout)),
        }
    }

    /// Subscribe to the single upstream event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<NodeEvent> {
        self.events.subscribe()
    }

    /// The current link state.
    pub fn health(&self) -> NodeHealth {
        self.health.read().clone()
    }
}

/// Run sessions forever, backing off between failures.
async fn supervise(connector: Arc<dyn Connector>, ctx: Rc<SessionCtx>, backoff: &mut Backoff) {
    while !ctx.stopping.load(Ordering::SeqCst) {
        let outcome = connector.run_session(Rc::clone(&ctx)).await;
        ctx.mark_disconnected(outcome.as_ref().err().cloned());
        let _ = ctx.events.send(NodeEvent::Disconnected);
        if ctx.stopping.load(Ordering::SeqCst) {
            break;
        }
        match outcome {
            Ok(()) => backoff.reset(),
            Err(ref e) => {
                tracing::warn!(target: "cb_gateway::node", error = %e, "rpc session ended")
            }
        }
        let delay = backoff.next_delay();
        tracing::debug!(target: "cb_gateway::node", ?delay, "reconnecting to sequencer");
        tokio::time::sleep(delay).await;
    }
    ctx.mark_disconnected(None);
}

/// Unwrap a reply variant, turning a mismatch into an internal error rather
/// than a panic. A mismatch means the session loop answered the wrong call,
/// which is a bug — but not one worth taking the process down for.
macro_rules! expect_reply {
    ($reply:expr, $variant:path) => {
        match $reply {
            $variant(value) => Ok(value),
            other => Err(PortError::Internal(format!(
                "rpc bridge returned {other:?} for a different call"
            ))),
        }
    };
}

#[async_trait::async_trait]
impl super::NodeClient for NodeBridge {
    async fn chain_info(&self) -> PortResult<ChainInfo> {
        expect_reply!(self.call(Call::ChainInfo).await?, Reply::ChainInfo)
    }

    async fn account(&self, address: Address) -> PortResult<AccountSnapshot> {
        expect_reply!(self.call(Call::Account(address)).await?, Reply::Account)
    }

    async fn submit_tx(&self, tx: SignedTx) -> PortResult<SubmitOutcome> {
        expect_reply!(
            self.call(Call::SubmitTx(Box::new(tx))).await?,
            Reply::Submit
        )
    }

    async fn spot_book(&self, market: MarketId, depth: u32) -> PortResult<BookSnapshot> {
        expect_reply!(self.call(Call::SpotBook(market, depth)).await?, Reply::Book)
    }

    async fn deriv_book(&self, market: DerivId, depth: u32) -> PortResult<BookSnapshot> {
        expect_reply!(
            self.call(Call::DerivBook(market, depth)).await?,
            Reply::Book
        )
    }

    async fn positions(&self, address: Address) -> PortResult<Vec<PositionSnapshot>> {
        expect_reply!(self.call(Call::Positions(address)).await?, Reply::Positions)
    }

    async fn margin_account(&self, address: Address) -> PortResult<MarginSnapshot> {
        expect_reply!(
            self.call(Call::MarginAccount(address)).await?,
            Reply::Margin
        )
    }

    async fn mark_price(&self, market: DerivId) -> PortResult<MarkSnapshot> {
        expect_reply!(self.call(Call::MarkPrice(market)).await?, Reply::Mark)
    }

    async fn oracle_price(&self, market: DerivId) -> PortResult<OracleSnapshot> {
        expect_reply!(self.call(Call::OraclePrice(market)).await?, Reply::Oracle)
    }

    async fn funding_index(&self, market: DerivId) -> PortResult<FundingSnapshot> {
        expect_reply!(self.call(Call::FundingIndex(market)).await?, Reply::Funding)
    }

    async fn perp_markets(&self) -> PortResult<Vec<PerpMarketSnapshot>> {
        expect_reply!(self.call(Call::PerpMarkets).await?, Reply::PerpMarkets)
    }

    async fn future_markets(&self) -> PortResult<Vec<FutureMarketSnapshot>> {
        expect_reply!(self.call(Call::FutureMarkets).await?, Reply::FutureMarkets)
    }

    async fn spot_markets(&self) -> PortResult<Vec<SpotMarketSnapshot>> {
        expect_reply!(self.call(Call::SpotMarkets).await?, Reply::SpotMarkets)
    }

    async fn block(&self, height: u64) -> PortResult<BlockSummary> {
        expect_reply!(self.call(Call::Block(height)).await?, Reply::Block).map(|b| *b)
    }

    fn subscribe(&self) -> broadcast::Receiver<NodeEvent> {
        NodeBridge::subscribe(self)
    }

    fn health(&self) -> NodeHealth {
        NodeBridge::health(self)
    }
}

mod capnp_session;
pub use capnp_session::TcpConnector;

#[cfg(test)]
mod tests {
    use super::*;

    /// A connector that fails a fixed number of times, then holds a session
    /// open until asked to stop. Enough to exercise the supervisor without a
    /// sequencer.
    struct FlakyConnector {
        failures_remaining: std::sync::atomic::AtomicU32,
        attempts: Arc<AtomicU64>,
    }

    impl Connector for FlakyConnector {
        fn describe(&self) -> String {
            "flaky".into()
        }

        #[allow(clippy::await_holding_refcell_ref)]
        fn run_session(&self, ctx: Rc<SessionCtx>) -> LocalBoxFuture<'_, Result<(), String>> {
            Box::pin(async move {
                self.attempts.fetch_add(1, Ordering::SeqCst);
                let remaining = self
                    .failures_remaining
                    .load(std::sync::atomic::Ordering::SeqCst);
                if remaining > 0 {
                    self.failures_remaining
                        .store(remaining - 1, std::sync::atomic::Ordering::SeqCst);
                    return Err("connection refused".into());
                }
                ctx.mark_connected(42);
                let _ = ctx.events.send(NodeEvent::Connected);
                // Serve requests until told to stop.
                loop {
                    if ctx.stopping.load(Ordering::SeqCst) {
                        return Ok(());
                    }
                    // Only this loop borrows the receiver on a single-threaded
                    // runtime, so holding the borrow across the await is safe.
                    let request = {
                        let mut guard = ctx.requests.borrow_mut();
                        tokio::time::timeout(Duration::from_millis(5), guard.recv()).await
                    };
                    match request {
                        Ok(Some(req)) => {
                            let reply = match req.call() {
                                Call::ChainInfo => Ok(Reply::ChainInfo(ChainInfo {
                                    chain_id: 42,
                                    denom: "USD".into(),
                                    head_height: 0,
                                    oldest_block: 1,
                                })),
                                _ => Err(PortError::Internal("unsupported in test".into())),
                            };
                            req.answer(reply);
                        }
                        Ok(None) => return Ok(()),
                        Err(_) => continue,
                    }
                }
            })
        }
    }

    #[test]
    fn backoff_doubles_then_caps() {
        let mut b = Backoff::new(Duration::from_millis(10), Duration::from_millis(80));
        assert_eq!(b.next_delay(), Duration::from_millis(10));
        assert_eq!(b.next_delay(), Duration::from_millis(20));
        assert_eq!(b.next_delay(), Duration::from_millis(40));
        assert_eq!(b.next_delay(), Duration::from_millis(80));
        assert_eq!(b.next_delay(), Duration::from_millis(80), "capped");
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_millis(10), "reset");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_bridge_reconnects_after_the_node_refuses_twice() {
        use crate::node::NodeClient;
        let attempts = Arc::new(AtomicU64::new(0));
        let connector = Arc::new(FlakyConnector {
            failures_remaining: std::sync::atomic::AtomicU32::new(2),
            attempts: Arc::clone(&attempts),
        });
        let bridge = NodeBridge::spawn(
            connector,
            BridgeConfig {
                backoff: Backoff::new(Duration::from_millis(5), Duration::from_millis(20)),
                call_timeout: Duration::from_millis(500),
                ..Default::default()
            },
        );

        // While the connector is still failing, calls must fail fast rather
        // than hang waiting for a link that is not there.
        let early = bridge.chain_info().await;
        assert!(
            matches!(early, Err(PortError::Unavailable(_))),
            "expected Unavailable while disconnected, got {early:?}"
        );

        // Give the supervisor time to exhaust the two failures and connect.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if bridge.health().connected {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "never reconnected");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let info = bridge
            .chain_info()
            .await
            .expect("connected session answers");
        assert_eq!(info.chain_id, 42);
        assert!(
            attempts.load(Ordering::SeqCst) >= 3,
            "two failures plus the successful session"
        );
        bridge.shutdown();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_call_while_disconnected_returns_promptly() {
        use crate::node::NodeClient;
        struct NeverConnects;
        impl Connector for NeverConnects {
            fn describe(&self) -> String {
                "never".into()
            }
            fn run_session(&self, _ctx: Rc<SessionCtx>) -> LocalBoxFuture<'_, Result<(), String>> {
                Box::pin(async { Err("refused".into()) })
            }
        }
        let bridge = NodeBridge::spawn(
            Arc::new(NeverConnects),
            BridgeConfig {
                backoff: Backoff::new(Duration::from_millis(5), Duration::from_millis(10)),
                call_timeout: Duration::from_secs(30),
                ..Default::default()
            },
        );
        let started = std::time::Instant::now();
        let result = bridge.account(Address([0u8; 20])).await;
        assert!(matches!(result, Err(PortError::Unavailable(_))));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "must not wait for the call timeout when plainly disconnected"
        );
        bridge.shutdown();
    }
}

#[cfg(test)]
mod start_height_tests {
    use super::*;

    fn ctx(start: u64) -> SessionCtx {
        let (_tx, rx) = mpsc::channel::<Request>(1);
        let (events, _) = broadcast::channel(4);
        SessionCtx {
            requests: Rc::new(RefCell::new(rx)),
            events,
            health: Arc::new(RwLock::new(NodeHealth::default())),
            next_height: Arc::new(AtomicU64::new(start)),
            stopping: Arc::new(AtomicBool::new(false)),
        }
    }

    fn info(head: u64, oldest: u64) -> ChainInfo {
        ChainInfo {
            chain_id: 1,
            denom: "USD".into(),
            head_height: head,
            oldest_block: oldest,
        }
    }

    /// The failure this pins: a gateway told to replay from block 1 against
    /// a node whose keep-window had long since pruned block 1 asked for a
    /// body that did not exist, and stayed at height zero for ever.
    #[test]
    fn a_replay_from_a_pruned_height_is_floored_to_the_oldest_body() {
        let ctx = ctx(1);
        assert_eq!(ctx.reconcile_start(&info(1_451_311, 1_451_301)), 1_451_301);
        assert_eq!(ctx.next_height.load(Ordering::SeqCst), 1_451_301);
        assert_eq!(ctx.health.read().head_height, 1_451_311);
    }

    #[test]
    fn a_resume_above_the_oldest_body_is_left_alone() {
        let ctx = ctx(500);
        assert_eq!(ctx.reconcile_start(&info(600, 400)), 500);
        assert_eq!(ctx.next_height.load(Ordering::SeqCst), 500);
    }

    #[test]
    fn connected_below_the_head_is_not_synced() {
        let ctx = ctx(1);
        ctx.mark_connected(1);
        ctx.reconcile_start(&info(10, 1));
        assert!(!ctx.health.read().is_synced(), "no block has arrived yet");
        ctx.observe_height(9);
        assert!(!ctx.health.read().is_synced(), "one short of the head");
        ctx.observe_height(10);
        assert!(ctx.health.read().is_synced());
    }

    /// A fresh chain has produced nothing: head zero, nothing streamed, and
    /// nothing missing. That is synced, and the end-to-end suite boots
    /// exactly this chain.
    #[test]
    fn a_genesis_chain_with_nothing_to_stream_is_synced() {
        let ctx = ctx(1);
        ctx.mark_connected(1);
        ctx.reconcile_start(&info(0, 0));
        assert!(ctx.health.read().is_synced());
    }
}
