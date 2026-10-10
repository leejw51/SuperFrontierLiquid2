//! The shared state every handler is given.
//!
//! Every field is a trait object or a service built over trait objects.
//! Nothing here names SQLite, the in-memory bus, the simulated feed or a TCP
//! socket — `main.rs` chooses those once and hands the result over. That is
//! the whole point of the ports design, and it is also why the entire test
//! suite can drive the real router with no database and no sequencer.

use crate::auth::AuthService;
use crate::candles::CandleAggregator;
use crate::faucet::FaucetService;
use crate::leaderboard::LeaderboardService;
use crate::marketdata::MarketDataCache;
use crate::markets::MarketCatalog;
use crate::node::NodeClient;
use crate::ratelimit::RateLimiter;
use crate::signer::TxSubmitter;
use crate::sim::SimControl;
use crate::tracker::OrderTracker;
use crate::ws::Hub;
use cb_ports::{AppDatabase, Clock, EventBus, MarketDataFeed};
use chrono::{DateTime, Utc};
use std::sync::Arc;
use std::time::Duration;

/// Tunables the handlers need at runtime.
#[derive(Debug, Clone)]
pub struct Settings {
    /// How long `POST /orders` waits for the block carrying the order.
    pub order_confirm: Duration,
    /// How long a WebSocket may stay silent.
    pub ws_idle: Duration,
    /// How often an open WebSocket rechecks the session it authenticated with.
    pub ws_session_recheck: Duration,
    /// Assumed block interval, for turning heights into timestamps.
    pub block_time_ms: u64,
    /// Largest `depth` the order-book endpoint will serve.
    pub max_depth: u32,
    /// Largest `limit` any list endpoint will serve.
    pub max_limit: u32,
    /// How stale a mark price may be, in blocks, before orders are refused.
    pub mark_staleness_blocks: u64,
    /// The sequencer RPC address, so the API can say what it fronts.
    pub node_rpc: String,
    /// The address this gateway listens on, as it was configured.
    pub gateway_listen: String,
    /// The oracle's scenario-control URL, when configured.
    pub oracle_control: Option<String>,
    /// Peers whose `x-forwarded-for` header the gateway believes.
    ///
    /// Empty by default, which means the header is ignored and every
    /// address-keyed decision uses the real TCP peer. A forwarded header is a
    /// claim by whoever is talking to us; it is evidence only when the
    /// immediate peer is a proxy the operator named.
    pub trusted_proxies: Arc<std::collections::HashSet<std::net::IpAddr>>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            order_confirm: Duration::from_secs(2),
            ws_idle: Duration::from_secs(90),
            // Revocation is pushed to the socket as it happens; this bounds how
            // long an *expired* session can keep one open.
            ws_session_recheck: Duration::from_secs(30),
            block_time_ms: 100,
            // The contract's documented ceiling.
            max_depth: 200,
            max_limit: 1_000,
            // Six hundred blocks is a minute at the default cadence. Beyond
            // that the oracle has plainly stopped and margining an order
            // against the last price would be guessing.
            mark_staleness_blocks: 600,
            node_rpc: "127.0.0.1:9000".into(),
            gateway_listen: "127.0.0.1:8080".into(),
            oracle_control: None,
            trusted_proxies: Arc::new(std::collections::HashSet::new()),
        }
    }
}

/// Everything a handler can reach.
pub struct AppState {
    /// The sequencer.
    pub node: Arc<dyn NodeClient>,
    /// Application storage.
    pub db: Arc<dyn AppDatabase>,
    /// The message bus.
    pub bus: Arc<dyn EventBus>,
    /// External price data.
    pub feed: Arc<dyn MarketDataFeed>,
    /// Time.
    pub clock: Arc<dyn Clock>,
    /// Registration, sessions and the custodial wallet.
    pub auth: Arc<AuthService>,
    /// The faucet.
    pub faucet: Arc<FaucetService>,
    /// Signing and submission.
    pub submitter: Arc<TxSubmitter>,
    /// The market universe.
    pub markets: Arc<MarketCatalog>,
    /// Derived order and fill state.
    pub tracker: Arc<OrderTracker>,
    /// OHLCV aggregation.
    pub candles: Arc<CandleAggregator>,
    /// Rankings.
    pub leaderboard: Arc<LeaderboardService>,
    /// Cached books, and the deltas derived from them.
    pub marketdata: Arc<MarketDataCache>,
    /// WebSocket fan-out.
    pub hub: Arc<Hub>,
    /// Memos for transfers the gateway has submitted and not yet seen land.
    ///
    /// `TxBody::Transfer` carries no memo, so the chain cannot give one back.
    /// The ledger entry is written when the block arrives, and this is how the
    /// sentence the sender typed reaches it. It is deliberately in-process and
    /// deliberately small: a memo that outlived a restart would have to be
    /// stored somewhere the chain does not know about, and a memo is an
    /// annotation on a movement, not the movement.
    pub transfer_memos: dashmap::DashMap<cb_types::H256, (String, DateTime<Utc>)>,
    /// What the gateway last saw of each watched account on chain.
    ///
    /// The watcher differences every block against this; the submission path
    /// primes it, so the baseline for an account predates the transaction the
    /// gateway is about to send rather than following it.
    pub mirror: Arc<crate::watcher::AccountMirror>,
    /// Recent block headers and the counters derived from them.
    pub chain_log: Arc<crate::http::chain::ChainLog>,
    /// Simulation control, when a harness is attached.
    pub sim: Arc<dyn SimControl>,
    /// Per-session and per-address limits on mutating endpoints.
    pub limits: Arc<RateLimiter>,
    /// Runtime tunables.
    pub settings: Settings,
    /// When the process started, for `/chain/status`.
    pub started_at: DateTime<Utc>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("markets", &self.markets.snapshot().metas.len())
            .field("connections", &self.hub.len())
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl AppState {
    /// Remember the memo a caller attached to a transfer, until the block
    /// carrying it arrives.
    pub fn remember_transfer_memo(&self, tx_hash: cb_types::H256, memo: &str, now: DateTime<Utc>) {
        // Bounded by time rather than by count, and swept on write so an idle
        // gateway does not hold them: a memo whose block has not arrived within
        // an hour is a memo whose transfer is never landing.
        if self.transfer_memos.len() > 256 {
            let cutoff = now - chrono::Duration::hours(1);
            self.transfer_memos.retain(|_, (_, at)| *at >= cutoff);
        }
        self.transfer_memos.insert(tx_hash, (memo.to_string(), now));
    }

    /// Take the memo for a transfer, if one was recorded.
    pub fn take_transfer_memo(&self, tx_hash: &cb_types::H256) -> Option<String> {
        self.transfer_memos
            .remove(tx_hash)
            .map(|(_, (memo, _))| memo)
    }

    /// Record an account's current chain state as the baseline for the next
    /// block, unless one is already held.
    ///
    /// Called from the handlers that are about to submit a transaction for a
    /// user. Without it, a user's first watched block is usually the block
    /// carrying their first order: the mirror is empty, the whole account looks
    /// like it appeared at once, and the difference is not attributable to
    /// anything. With it, the first fill is an ordinary difference.
    ///
    /// Best effort. A node that will not answer means the next block seeds the
    /// baseline instead, which costs one block of derived history and no
    /// correctness.
    pub async fn prime_account_mirror(&self, user_id: &str, address: cb_types::Address) {
        if self.mirror.baseline(user_id).is_some() {
            return;
        }
        let Ok(account) = self.node.account(address).await else {
            return;
        };
        self.mirror.prime(
            user_id,
            crate::watcher::AccountBaseline {
                address,
                account,
                positions: self.node.positions(address).await.unwrap_or_default(),
                margin: self.node.margin_account(address).await.unwrap_or_default(),
            },
        );
    }

    /// The highest block height the gateway has seen. That is the node's
    /// `last_height`; its `head_height` is the node's own head, which the
    /// gateway may not have reached yet.
    #[allow(clippy::misnamed_getters)]
    pub fn head_height(&self) -> u64 {
        self.node.health().last_height
    }

    /// Clamp a caller-supplied `limit` to something the gateway will serve.
    pub fn clamp_limit(&self, requested: Option<u32>, default: u32) -> u32 {
        requested
            .unwrap_or(default)
            .clamp(1, self.max_limit(default))
    }

    fn max_limit(&self, default: u32) -> u32 {
        self.settings.max_limit.max(default)
    }

    /// Clamp a caller-supplied order-book depth.
    pub fn clamp_depth(&self, requested: Option<u32>) -> u32 {
        requested.unwrap_or(50).clamp(1, self.settings.max_depth)
    }

    /// Whether a mark price computed at `height` is fresh enough to trade
    /// against.
    pub fn mark_is_fresh(&self, computed_at_height: u64) -> bool {
        if computed_at_height == 0 {
            return false;
        }
        let head = self.head_height();
        head.saturating_sub(computed_at_height) <= self.settings.mark_staleness_blocks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_and_depths_are_clamped_to_something_servable() {
        let settings = Settings::default();
        assert_eq!(settings.max_depth, 200);
        assert_eq!(settings.ws_idle, Duration::from_secs(90));
    }
}
