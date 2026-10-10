//! The gateway seam.
//!
//! Everything the interface knows about the exchange arrives through the [`Api`]
//! trait. There are two implementations — [`http::HttpApi`], which speaks to the
//! real gateway, and [`mock::MockApi`], which serves the reference document's own
//! figures — and the application never knows which one it is holding.
//!
//! That is not a testing convenience. The gateway is being built in parallel
//! against the same frozen contract, so the interface has to be demonstrable and
//! reviewable before the gateway exists. Choosing between them is one line in
//! [`crate::config`], with a runtime override for switching without a rebuild.
//!
//! Methods return [`LocalBoxFuture`] rather than `async fn` because the trait has
//! to be object safe: `Arc<dyn Api>` is what lives in the Leptos context. The
//! futures are deliberately **not** `Send` — this is a single-threaded wasm
//! application and `gloo_net`'s futures are not `Send` either — while the trait
//! object itself is, which is what Leptos's context store requires.

pub mod error;
pub mod fixtures;
pub mod http;
pub mod mock;
pub mod types;

use futures::future::LocalBoxFuture;

pub use error::{ApiError, ApiResult, ErrorCode, GatewayError, Remedy};
pub use types::*;

/// A candle interval, exactly the six the contract allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Interval {
    /// One minute.
    M1,
    /// Five minutes.
    M5,
    /// Fifteen minutes.
    M15,
    /// One hour.
    H1,
    /// Four hours.
    H4,
    /// One day.
    D1,
}

impl Interval {
    /// Every interval, in the order the chart toolbar shows them.
    pub const ALL: [Self; 6] = [Self::M1, Self::M5, Self::M15, Self::H1, Self::H4, Self::D1];

    /// The wire value, which is also the button label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::M1 => "1m",
            Self::M5 => "5m",
            Self::M15 => "15m",
            Self::H1 => "1h",
            Self::H4 => "4h",
            Self::D1 => "1d",
        }
    }

    /// How many seconds one period covers.
    #[must_use]
    pub const fn seconds(self) -> i64 {
        match self {
            Self::M1 => 60,
            Self::M5 => 300,
            Self::M15 => 900,
            Self::H1 => 3_600,
            Self::H4 => 14_400,
            Self::D1 => 86_400,
        }
    }
}

/// Which orders `GET /account/orders` should return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderFilter {
    /// Resting orders only. The default.
    Open,
    /// Fully executed orders.
    Filled,
    /// Withdrawn orders.
    Cancelled,
    /// Everything, in any state.
    All,
}

impl OrderFilter {
    /// The wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Filled => "filled",
            Self::Cancelled => "cancelled",
            Self::All => "all",
        }
    }
}

/// The window a leaderboard is ranked over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaderboardWindow {
    /// The last 24 hours.
    Day,
    /// The last seven days.
    Week,
    /// Since genesis.
    AllTime,
}

impl LeaderboardWindow {
    /// Every window, in the order the segmented control shows them.
    pub const ALL: [Self; 3] = [Self::Day, Self::Week, Self::AllTime];

    /// The wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Day => "24h",
            Self::Week => "7d",
            Self::AllTime => "all",
        }
    }

    /// The button label. Sentence case, like every other label in the product.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Day => "24h",
            Self::Week => "7d",
            Self::AllTime => "All time",
        }
    }
}

/// A future returned by an [`Api`] call.
pub type Fut<'a, T> = LocalBoxFuture<'a, ApiResult<T>>;

/// Everything `cb-web` can ask the exchange.
///
/// One method per documented endpoint, named after the endpoint rather than
/// after the screen that calls it, so that a reader can hold this trait and
/// `docs/api-contract.md` side by side and check them off.
pub trait Api: Send + Sync {
    /// Installs, replaces or clears the bearer token used by 🔒 endpoints.
    ///
    /// Called on login, on a restored session, and with `None` on any `401`.
    fn set_token(&self, token: Option<String>);

    /// The token currently in use, if any.
    fn token(&self) -> Option<String>;

    /// `POST /auth/challenge`. Returns the exact message to sign.
    fn challenge<'a>(&'a self, address: &'a str) -> Fut<'a, Challenge>;

    /// `POST /auth/login`, with a signature over the challenge message.
    ///
    /// `handle` is required on a first login and ignored afterwards. The client
    /// cannot know in advance whether an address is new, so the contract has it
    /// retry on `HANDLE_REQUIRED` rather than guess.
    fn login<'a>(
        &'a self,
        address: &'a str,
        signature: &'a str,
        handle: Option<&'a str>,
    ) -> Fut<'a, Session>;

    /// `GET /auth/session`. Used to validate a token restored from storage.
    fn session(&self) -> Fut<'_, SessionInfo>;

    /// `POST /auth/logout`.
    fn logout(&self) -> Fut<'_, ()>;

    /// `GET /markets`.
    fn markets(&self) -> Fut<'_, MarketsResponse>;

    /// `GET /markets/{symbol}/ticker`.
    fn ticker<'a>(&'a self, symbol: &'a str) -> Fut<'a, Ticker>;

    /// `GET /markets/{symbol}/orderbook`.
    fn orderbook<'a>(&'a self, symbol: &'a str, depth: u32) -> Fut<'a, OrderBookPayload>;

    /// `GET /markets/{symbol}/trades`.
    fn trades<'a>(&'a self, symbol: &'a str, limit: u32) -> Fut<'a, TradesResponse>;

    /// `GET /markets/{symbol}/candles`.
    fn candles<'a>(
        &'a self,
        symbol: &'a str,
        interval: Interval,
        limit: u32,
    ) -> Fut<'a, CandlesResponse>;

    /// `GET /account`.
    fn account(&self) -> Fut<'_, Account>;

    /// `GET /account/orders`.
    fn orders(&self, filter: OrderFilter) -> Fut<'_, OrdersResponse>;

    /// `GET /account/positions`.
    fn positions(&self) -> Fut<'_, PositionsResponse>;

    /// `GET /account/fills`.
    fn fills(&self, limit: u32) -> Fut<'_, FillsResponse>;

    /// `GET /account/transfers`, the unified ledger.
    fn ledger(&self, limit: u32) -> Fut<'_, LedgerResponse>;

    /// `POST /orders`.
    fn place_order<'a>(&'a self, request: &'a PlaceOrderRequest) -> Fut<'a, PlaceOrderResponse>;

    /// `DELETE /orders/{order_id}`.
    fn cancel_order<'a>(&'a self, order_id: &'a str) -> Fut<'a, CancelOrderResponse>;

    /// `POST /orders/cancel-all`. `None` cancels across every market.
    fn cancel_all<'a>(&'a self, symbol: Option<&'a str>) -> Fut<'a, CancelAllResponse>;

    /// `POST /positions/close`. `None` closes the position entirely.
    fn close_position<'a>(
        &'a self,
        symbol: &'a str,
        qty: Option<rust_decimal::Decimal>,
    ) -> Fut<'a, ClosePositionResponse>;

    /// `GET /faucet`.
    fn faucet(&self) -> Fut<'_, FaucetStatus>;

    /// `POST /faucet/claim`.
    fn faucet_claim(&self) -> Fut<'_, FaucetClaim>;

    /// `GET /chain/status`.
    fn chain_status(&self) -> Fut<'_, ChainStatus>;

    /// `GET /chain/blocks`.
    fn blocks(&self, limit: u32) -> Fut<'_, BlocksResponse>;

    /// `GET /chain/txs`.
    fn chain_txs(&self, limit: u32) -> Fut<'_, TxsResponse>;

    /// `GET /leaderboard`.
    fn leaderboard(&self, window: LeaderboardWindow, limit: u32) -> Fut<'_, LeaderboardResponse>;

    // ------------------------------------------------------------ user page

    /// `GET /user/profile`.
    fn profile(&self) -> Fut<'_, Profile>;

    /// `PATCH /user/profile`. Only the handle is mutable.
    fn update_handle<'a>(&'a self, handle: &'a str) -> Fut<'a, Profile>;

    /// `GET /user/sessions`.
    fn sessions(&self) -> Fut<'_, SessionsResponse>;

    /// `DELETE /user/sessions/{session_id}`.
    fn revoke_session<'a>(&'a self, session_id: &'a str) -> Fut<'a, ()>;

    /// `POST /user/sessions/revoke-all`. Every session but this one.
    fn revoke_other_sessions(&self) -> Fut<'_, RevokedResponse>;

    /// `GET /user/activity`.
    fn activity<'a>(&'a self, limit: u32, cursor: Option<&'a str>) -> Fut<'a, ActivityResponse>;

    // ---------------------------------------------------------------- admin

    /// `GET /admin/overview`. One call, so the dashboard paints at once.
    fn admin_overview(&self) -> Fut<'_, AdminOverview>;

    /// `GET /admin/users`.
    fn admin_users<'a>(
        &'a self,
        query: &'a str,
        is_bot: Option<bool>,
        limit: u32,
    ) -> Fut<'a, AdminUsersResponse>;

    /// `POST /admin/users/{user_id}/suspend`.
    fn admin_suspend<'a>(&'a self, user_id: &'a str, reason: &'a str) -> Fut<'a, ()>;

    /// `POST /admin/users/{user_id}/unsuspend`.
    fn admin_unsuspend<'a>(&'a self, user_id: &'a str) -> Fut<'a, ()>;

    /// `GET /admin/markets`, with live operational state.
    fn admin_markets(&self) -> Fut<'_, AdminMarketsResponse>;

    /// `POST /admin/markets/{symbol}/halt`.
    fn admin_halt<'a>(&'a self, symbol: &'a str, reason: &'a str) -> Fut<'a, ()>;

    /// `POST /admin/markets/{symbol}/resume`.
    fn admin_resume<'a>(&'a self, symbol: &'a str) -> Fut<'a, ()>;

    /// `POST /admin/markets/{symbol}/settle`. Irreversible.
    fn admin_settle<'a>(
        &'a self,
        symbol: &'a str,
        settlement_price: rust_decimal::Decimal,
        force: bool,
    ) -> Fut<'a, ()>;

    /// `PATCH /admin/markets/{symbol}/risk`.
    ///
    /// With `dry_run`, the gateway computes the consequence and applies
    /// nothing. The dialog uses it to show the operator who the change puts
    /// below maintenance **before** the button that makes it so.
    fn admin_set_risk<'a>(
        &'a self,
        symbol: &'a str,
        max_leverage: rust_decimal::Decimal,
        initial_margin_bps: u32,
        maintenance_margin_bps: u32,
        dry_run: bool,
    ) -> Fut<'a, RiskApplied>;

    /// `GET /admin/faucet`.
    fn admin_faucet(&self) -> Fut<'_, AdminFaucet>;

    /// `PATCH /admin/faucet`.
    fn admin_set_faucet<'a>(
        &'a self,
        enabled: bool,
        cooldown_seconds: i64,
        grant: &'a [Grant],
    ) -> Fut<'a, AdminFaucet>;

    /// `POST /admin/faucet/grant`. Bypasses the cooldown; audited like the rest.
    fn admin_grant<'a>(
        &'a self,
        user_id: &'a str,
        grants: &'a [Grant],
        reason: &'a str,
    ) -> Fut<'a, FaucetClaim>;

    /// `GET /admin/audit`.
    fn admin_audit<'a>(&'a self, action: &'a str, limit: u32) -> Fut<'a, AuditResponse>;

    /// `GET /admin/sim`.
    fn admin_sim(&self) -> Fut<'_, SimStatus>;

    /// `POST /admin/sim`.
    fn admin_set_sim<'a>(&'a self, request: &'a SimRequest) -> Fut<'a, SimStatus>;
}
