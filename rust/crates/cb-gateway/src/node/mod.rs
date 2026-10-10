//! The sequencer, behind one `Send` async trait.
//!
//! ## The problem this module exists to solve
//!
//! `capnp-rpc` futures hold raw pointers into the RPC system's arena. They are
//! `!Send`, so a `tokio` multi-threaded runtime — which may move a task
//! between worker threads at any await point — cannot drive them at all. The
//! type system says so, and there is no way around it that does not involve
//! lying.
//!
//! The answer, which `cb-client` arrived at first and this module generalises
//! to a long-lived connection, is to give the RPC system a thread of its own:
//! a current-thread runtime plus a `tokio::task::LocalSet`. Requests reach it
//! over an mpsc channel, each carrying a oneshot sender for its reply. Nothing
//! `!Send` ever leaves that thread — replies are copied into the owned structs
//! in [`types`] before they are sent back.
//!
//! The result is [`NodeClient`]: a perfectly ordinary async trait whose
//! futures are `Send`, usable from any axum handler, and — because it is a
//! trait — replaceable in tests by [`mock::MockNode`] so the suite needs no
//! running sequencer.
//!
//! ## Availability, not hanging
//!
//! A gateway that blocks on a dead node is worse than one that fails: the
//! browser sees a spinner forever instead of an error it can act on. Every
//! call through the bridge is bounded by a timeout, and calls made while
//! disconnected fail immediately with [`cb_ports::PortError::Unavailable`],
//! which the HTTP layer renders as `503 NODE_UNAVAILABLE`. The supervisor
//! reconnects underneath with exponential backoff.

pub mod bridge;
pub mod mock;
pub mod types;

pub use bridge::{Connector, NodeBridge, TcpConnector};
pub use mock::MockNode;
pub use types::*;

use async_trait::async_trait;
use cb_ports::PortResult;
use cb_tx::SignedTx;
use cb_types::{Address, DerivId, MarketId};
use tokio::sync::broadcast;

/// Everything the gateway asks of the sequencer.
///
/// Implementations must never block indefinitely: a method that cannot reach
/// the node returns [`cb_ports::PortError::Unavailable`] promptly rather than
/// waiting for a reconnection.
#[async_trait]
pub trait NodeClient: Send + Sync + 'static {
    /// Chain id and native denom. Cached by implementations; signing needs it.
    async fn chain_info(&self) -> PortResult<ChainInfo>;

    /// Balances, locks and the next nonce for one address.
    async fn account(&self, address: Address) -> PortResult<AccountSnapshot>;

    /// Submit a signed transaction to the mempool.
    async fn submit_tx(&self, tx: SignedTx) -> PortResult<SubmitOutcome>;

    /// Aggregated depth for a spot book.
    async fn spot_book(&self, market: MarketId, depth: u32) -> PortResult<BookSnapshot>;

    /// Aggregated depth for a perpetual or dated-future book.
    async fn deriv_book(&self, market: DerivId, depth: u32) -> PortResult<BookSnapshot>;

    /// Open derivative positions for one address.
    async fn positions(&self, address: Address) -> PortResult<Vec<PositionSnapshot>>;

    /// The cross-margin collateral pool for one address.
    async fn margin_account(&self, address: Address) -> PortResult<MarginSnapshot>;

    /// The engine's mark price for one derivative market.
    async fn mark_price(&self, market: DerivId) -> PortResult<MarkSnapshot>;

    /// The last oracle-posted index price for one derivative market.
    async fn oracle_price(&self, market: DerivId) -> PortResult<OracleSnapshot>;

    /// Cumulative funding and the latest rate for one perpetual.
    async fn funding_index(&self, market: DerivId) -> PortResult<FundingSnapshot>;

    /// Every listed perpetual.
    async fn perp_markets(&self) -> PortResult<Vec<PerpMarketSnapshot>>;

    /// Every listed dated future.
    async fn future_markets(&self) -> PortResult<Vec<FutureMarketSnapshot>>;

    /// Every spot market genesis configured, with the tick and lot sizes the
    /// executor enforces.
    async fn spot_markets(&self) -> PortResult<Vec<SpotMarketSnapshot>>;

    /// One block by height.
    async fn block(&self, height: u64) -> PortResult<BlockSummary>;

    /// Subscribe to the single upstream event stream.
    ///
    /// Every consumer — the WebSocket hub, the candle aggregator, the order
    /// tracker — receives from this one broadcast. The gateway opens exactly
    /// one subscription against the node however many browsers are connected.
    fn subscribe(&self) -> broadcast::Receiver<NodeEvent>;

    /// The current state of the link, for `/health`.
    fn health(&self) -> NodeHealth;
}

/// Blanket implementation so an `Arc<T>` is itself a `NodeClient`.
#[async_trait]
impl<T: NodeClient + ?Sized> NodeClient for std::sync::Arc<T> {
    async fn chain_info(&self) -> PortResult<ChainInfo> {
        (**self).chain_info().await
    }
    async fn account(&self, address: Address) -> PortResult<AccountSnapshot> {
        (**self).account(address).await
    }
    async fn submit_tx(&self, tx: SignedTx) -> PortResult<SubmitOutcome> {
        (**self).submit_tx(tx).await
    }
    async fn spot_book(&self, market: MarketId, depth: u32) -> PortResult<BookSnapshot> {
        (**self).spot_book(market, depth).await
    }
    async fn deriv_book(&self, market: DerivId, depth: u32) -> PortResult<BookSnapshot> {
        (**self).deriv_book(market, depth).await
    }
    async fn positions(&self, address: Address) -> PortResult<Vec<PositionSnapshot>> {
        (**self).positions(address).await
    }
    async fn margin_account(&self, address: Address) -> PortResult<MarginSnapshot> {
        (**self).margin_account(address).await
    }
    async fn mark_price(&self, market: DerivId) -> PortResult<MarkSnapshot> {
        (**self).mark_price(market).await
    }
    async fn oracle_price(&self, market: DerivId) -> PortResult<OracleSnapshot> {
        (**self).oracle_price(market).await
    }
    async fn funding_index(&self, market: DerivId) -> PortResult<FundingSnapshot> {
        (**self).funding_index(market).await
    }
    async fn perp_markets(&self) -> PortResult<Vec<PerpMarketSnapshot>> {
        (**self).perp_markets().await
    }
    async fn future_markets(&self) -> PortResult<Vec<FutureMarketSnapshot>> {
        (**self).future_markets().await
    }
    async fn spot_markets(&self) -> PortResult<Vec<SpotMarketSnapshot>> {
        (**self).spot_markets().await
    }
    async fn block(&self, height: u64) -> PortResult<BlockSummary> {
        (**self).block(height).await
    }
    fn subscribe(&self) -> broadcast::Receiver<NodeEvent> {
        (**self).subscribe()
    }
    fn health(&self) -> NodeHealth {
        (**self).health()
    }
}
