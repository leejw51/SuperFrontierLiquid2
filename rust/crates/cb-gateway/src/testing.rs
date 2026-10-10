//! A whole gateway, wired to doubles, for tests.
//!
//! Integration tests live in their own crate and cannot see `#[cfg(test)]`
//! modules, so the harness lives here. It builds the *real* router over the
//! real services, with the ports replaced by the in-memory implementations in
//! [`crate::memory`] and the sequencer replaced by [`crate::node::MockNode`].
//!
//! The point is that the thing under test is the production code path. A test
//! that exercised a simplified router would prove nothing about the one that
//! ships.

use crate::auth::AuthService;
use crate::candles::CandleAggregator;
use crate::directory::Directory;
use crate::faucet::{FaucetConfig, FaucetService};
use crate::leaderboard::LeaderboardService;
use crate::marketdata::MarketDataCache;
use crate::markets::{MarketCatalog, SpotDefaults};
use crate::memory::{InMemoryBus, InMemoryDatabase, InMemoryKv};
use crate::node::MockNode;
use crate::ratelimit::RateLimiter;
use crate::signer::TxSubmitter;
use crate::sim::{NoSimulation, SimControl};
use crate::state::{AppState, Settings};
use crate::tracker::OrderTracker;
use crate::ws::Hub;
use cb_ports::{Clock, FixedClock};
use chrono::{DateTime, Utc};
use std::sync::Arc;
use std::time::Duration;

/// A gateway wired to doubles, with handles on the doubles.
pub struct Harness {
    /// The scriptable sequencer.
    pub node: Arc<MockNode>,
    /// The in-memory application database.
    pub db: Arc<InMemoryDatabase>,
    /// The clock the whole gateway reads.
    pub clock: FixedClock,
    /// The assembled state, as the handlers see it.
    pub state: Arc<AppState>,
}

impl Harness {
    /// A harness with default settings and no markets listed.
    pub fn new() -> Self {
        Self::builder().build()
    }

    /// Start configuring a harness.
    pub fn builder() -> HarnessBuilder {
        HarnessBuilder::default()
    }

    /// The axum router, exactly as `main.rs` mounts it, with the development
    /// login available.
    pub fn router(&self) -> axum::Router {
        crate::http::router_with(Arc::clone(&self.state), &[], true)
    }

    /// The router as a production deployment mounts it: no development login.
    pub fn router_without_dev_login(&self) -> axum::Router {
        crate::http::router(Arc::clone(&self.state), &[])
    }

    /// Refresh the market catalogue from whatever the mock currently lists.
    pub async fn refresh_markets(&self) {
        let head = self.state.head_height();
        let _ = self.state.markets.refresh(head).await;
    }

    /// The mnemonic the harness derives its players from.
    ///
    /// The contract's canonical all-zero-entropy phrase, so an address a test
    /// prints is one any EVM tool reproduces.
    pub const MNEMONIC: &'static str = "abandon abandon abandon abandon abandon abandon \
                                        abandon abandon abandon abandon abandon about";

    /// Sign a player in through the development login, which is the only path
    /// that leaves the gateway holding a key it can trade with.
    ///
    /// Returns the account, including both of its addresses.
    pub async fn login(&self, handle: &str, index: u32) -> Player {
        let result = self
            .state
            .auth
            .dev_login(Self::MNEMONIC, index, Some(handle))
            .await
            .expect("the harness login succeeds");
        Player {
            user_id: result.user.user_id,
            token: result.session_token,
            login_address: result.login_address,
            trading_address: result.trading_address,
        }
    }

    /// Sign a player in by signature alone, as a browser would.
    pub async fn login_by_signature(&self, handle: &str, index: u32) -> Player {
        use crate::auth::eth;
        let (secret, address) = eth::derive_from_mnemonic(Self::MNEMONIC, index).expect("derive");
        let challenge = self.state.auth.challenge(&address);
        let signature = eth::sign_personal_sign(&secret, &challenge.message).expect("sign");
        let result = self
            .state
            .auth
            .login(&address, &signature, Some(handle))
            .await
            .expect("the harness signature login succeeds");
        Player {
            user_id: result.user.user_id,
            token: result.session_token,
            login_address: result.login_address,
            trading_address: result.trading_address,
        }
    }
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}

/// Knobs a test may want before the gateway is assembled.
pub struct HarnessBuilder {
    now: DateTime<Utc>,
    settings: Settings,
    faucet: FaucetConfig,
    spot_defaults: SpotDefaults,
    ws_buffer: usize,
    rate_per_session: u32,
    rate_per_ip: u32,
    sim: Arc<dyn SimControl>,
    admin_handles: Vec<String>,
    admin_addresses: Vec<cb_types::Address>,
}

impl Default for HarnessBuilder {
    fn default() -> Self {
        Self {
            now: DateTime::parse_from_rfc3339("2026-09-18T12:00:00Z")
                .expect("static timestamp")
                .with_timezone(&Utc),
            settings: Settings {
                // Tests have no block producer, so a two-second wait for
                // inclusion would be two seconds of nothing on every order.
                order_confirm: Duration::from_millis(20),
                ..Default::default()
            },
            faucet: FaucetConfig::default(),
            spot_defaults: SpotDefaults::default(),
            ws_buffer: 64,
            rate_per_session: 0,
            rate_per_ip: 0,
            sim: Arc::new(NoSimulation),
            admin_handles: Vec::new(),
            admin_addresses: Vec::new(),
        }
    }
}

impl HarnessBuilder {
    /// Pin the clock.
    pub fn at(mut self, now: DateTime<Utc>) -> Self {
        self.now = now;
        self
    }

    /// Override the runtime settings.
    pub fn settings(mut self, settings: Settings) -> Self {
        self.settings = settings;
        self
    }

    /// Override the faucet configuration.
    pub fn faucet(mut self, faucet: FaucetConfig) -> Self {
        self.faucet = faucet;
        self
    }

    /// Turn rate limiting on. Zero, the default, disables it so an unrelated
    /// test cannot fail because it made a hundred requests.
    pub fn rate_limits(mut self, per_session: u32, per_ip: u32) -> Self {
        self.rate_per_session = per_session;
        self.rate_per_ip = per_ip;
        self
    }

    /// Set the per-connection WebSocket queue depth.
    pub fn ws_buffer(mut self, buffer: usize) -> Self {
        self.ws_buffer = buffer;
        self
    }

    /// Attach a simulation harness.
    pub fn sim(mut self, sim: Arc<dyn SimControl>) -> Self {
        self.sim = sim;
        self
    }

    /// Name the handles that hold the operator role.
    ///
    /// This is the deployment's *unsafe* binding — see
    /// [`crate::auth::AuthService::with_admin_handles`] — and a test that uses
    /// it is testing the enforcement mechanism, not the binding.
    pub fn admins(mut self, handles: &[&str]) -> Self {
        self.admin_handles = handles.iter().map(|h| h.to_string()).collect();
        self
    }

    /// Name the login addresses that hold the operator role, which is how a
    /// real deployment is configured.
    pub fn admin_addresses(mut self, addresses: &[cb_types::Address]) -> Self {
        self.admin_addresses = addresses.to_vec();
        self
    }

    /// Assemble the gateway.
    pub fn build(self) -> Harness {
        let node = Arc::new(MockNode::new());
        let db = InMemoryDatabase::shared();
        let kv = Arc::new(InMemoryKv::new());
        let clock = FixedClock::new(self.now);
        let clock_port: Arc<dyn Clock> = Arc::new(clock.clone());

        let directory = Arc::new(Directory::new(kv));
        let auth = Arc::new(
            AuthService::new(
                db.clone(),
                Arc::clone(&directory),
                Arc::clone(&clock_port),
                chrono::Duration::hours(24),
                "harness-server-secret-0f3a91c7e5b24d86",
            )
            .expect("the harness secret is long enough and is not a published placeholder")
            .with_admin_addresses(self.admin_addresses)
            .with_admin_handles(self.admin_handles),
        );
        let submitter = Arc::new(TxSubmitter::with_clock(
            node.clone(),
            Arc::clone(&clock_port),
        ));
        let (treasury_key, _public) = cb_crypto::generate_keypair();
        let faucet = Arc::new(FaucetService::new(
            db.clone(),
            Arc::clone(&submitter),
            Arc::clone(&clock_port),
            self.faucet,
            treasury_key,
        ));
        let markets = Arc::new(MarketCatalog::new(
            node.clone(),
            db.clone(),
            Arc::clone(&clock_port),
            self.settings.block_time_ms,
            self.spot_defaults,
        ));
        let tracker = OrderTracker::new();
        let candles = Arc::new(CandleAggregator::new(db.clone(), Arc::clone(&clock_port)));
        let leaderboard = Arc::new(LeaderboardService::new(
            db.clone(),
            node.clone(),
            Arc::clone(&markets),
            Arc::clone(&tracker),
            Arc::clone(&clock_port),
            Arc::clone(&auth),
        ));

        let state = Arc::new(AppState {
            node: node.clone(),
            db: db.clone(),
            bus: Arc::new(InMemoryBus::default()),
            feed: Arc::new(NullFeed),
            clock: Arc::clone(&clock_port),
            auth,
            faucet,
            submitter,
            markets,
            tracker,
            candles,
            leaderboard,
            marketdata: Arc::new(MarketDataCache::new()),
            hub: Hub::new(self.ws_buffer),
            mirror: Arc::new(crate::watcher::AccountMirror::new()),
            transfer_memos: Default::default(),
            chain_log: Arc::new(crate::http::chain::ChainLog::new()),
            sim: self.sim,
            limits: Arc::new(RateLimiter::new(
                Arc::clone(&clock_port),
                self.rate_per_session,
                self.rate_per_ip,
            )),
            settings: self.settings,
            started_at: self.now,
        });

        Harness {
            node,
            db,
            clock,
            state,
        }
    }
}

/// A signed-in player in the harness.
#[derive(Debug, Clone)]
pub struct Player {
    /// Internal id.
    pub user_id: String,
    /// Bearer token.
    pub token: String,
    /// The address they signed in with.
    pub login_address: cb_types::Address,
    /// The on-chain account holding their money.
    pub trading_address: cb_types::Address,
}

impl Player {
    /// The `Authorization` header value for this player.
    pub fn bearer(&self) -> String {
        format!("Bearer {}", self.token)
    }
}

/// A [`cb_ports::MarketDataFeed`] that carries nothing.
///
/// The gateway's own endpoints do not read the external feed — mark and index
/// prices come from the chain — so the harness supplies an empty one rather
/// than pulling in `cb-feed`, which is being written in parallel.
pub struct NullFeed;

#[async_trait::async_trait]
impl cb_ports::MarketDataFeed for NullFeed {
    fn source_id(&self) -> &str {
        "null"
    }

    async fn latest(&self, symbol: &str) -> cb_ports::PortResult<cb_ports::PriceTick> {
        Err(cb_ports::PortError::NotFound(symbol.to_string()))
    }

    async fn latest_batch(
        &self,
        _symbols: &[String],
    ) -> cb_ports::PortResult<Vec<cb_ports::PriceTick>> {
        Ok(Vec::new())
    }

    async fn subscribe(
        &self,
        _symbols: &[String],
    ) -> cb_ports::PortResult<Box<dyn cb_ports::FeedSubscription>> {
        Err(cb_ports::PortError::Unavailable(
            "no market data feed is attached".into(),
        ))
    }

    async fn supported_symbols(&self) -> cb_ports::PortResult<Vec<String>> {
        Ok(Vec::new())
    }

    async fn health_check(&self) -> cb_ports::PortResult<()> {
        Err(cb_ports::PortError::Unavailable(
            "no market data feed is attached".into(),
        ))
    }
}
