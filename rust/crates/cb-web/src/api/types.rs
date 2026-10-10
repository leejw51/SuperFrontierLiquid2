//! The wire types of `docs/api-contract.md`, one Rust type per documented shape.
//!
//! This module is a transcription, not a design. Field names, optionality and
//! enum variants match the frozen contract exactly so that a change on either
//! side of the seam fails to compile rather than failing at runtime in front of
//! someone with money on screen.
//!
//! Every monetary quantity is a [`Decimal`]. `rust_decimal`'s `serde-str`
//! feature is enabled crate-wide, so a `Decimal` field serialises to and
//! deserialises from the decimal **string** the contract specifies. There is no
//! path in this crate by which a price becomes an `f64`.

#![allow(clippy::struct_excessive_bools)]

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------- 2. Identity and session

/// `POST /auth/challenge` body.
#[derive(Debug, Clone, Serialize)]
pub struct ChallengeRequest {
    /// The address that intends to sign in. Any case; the gateway checksums it.
    pub address: String,
}

/// The nonce and the exact message to sign.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Challenge {
    /// Single-use nonce, valid for five minutes.
    pub nonce: String,
    /// The string to sign **verbatim**.
    ///
    /// The contract is explicit that the client must not rebuild this text, so
    /// that the gateway can change the wording without breaking every client.
    /// It is passed straight to the signer and never reconstructed here.
    pub message: String,
    /// When the nonce stops being accepted.
    pub expires_at: String,
}

/// `POST /auth/login` body: an address, a signature over the challenge, and —
/// on a first sign-in only — the handle to claim.
#[derive(Debug, Clone, Serialize)]
pub struct LoginRequest {
    /// The EIP-55 address being claimed.
    pub address: String,
    /// The EIP-191 signature over the challenge message, `0x` + `r||s||v`.
    pub signature: String,
    /// The handle to register. Required when the account is new, ignored after.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
}

/// A signed-in session.
///
/// There is no separate registration step: a first successful login creates the
/// account, and `is_new` is what tells the interface to show the hand-off.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Session {
    /// Opaque user identifier, `usr_7c9a1f`.
    pub user_id: String,
    /// The handle the account trades under.
    pub handle: String,
    /// The EIP-55 address.
    pub address: String,
    /// Bearer token for every authenticated request.
    pub session_token: String,
    /// RFC 3339 UTC instant at which the token stops working.
    pub expires_at: String,
    /// True when this login created the account.
    #[serde(default)]
    pub is_new: bool,
}

/// The response to `GET /auth/session`: who the stored token belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionInfo {
    /// Opaque user identifier.
    pub user_id: String,
    /// The handle the account trades under.
    pub handle: String,
    /// The EVM address the user signed in with. They hold this key.
    pub login_address: String,
    /// The on-chain account holding the test money. The gateway holds this key,
    /// deriving it from the login address; see the contract's §2 for why.
    pub trading_address: String,
    /// `user`, and `admin` for an operator.
    ///
    /// Used only to decide whether to show the admin navigation. It is not a
    /// security boundary: the gateway enforces the role on every admin endpoint
    /// regardless of what this client believes.
    #[serde(default)]
    pub roles: Vec<String>,
    /// When the session began.
    #[serde(default)]
    pub issued_at: String,
    /// RFC 3339 UTC session expiry.
    pub expires_at: String,
}

// ----------------------------------------------------------------- 3. Faucet

/// A single asset line in a faucet grant.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Grant {
    /// Asset symbol, `USD` or `BTC`.
    pub asset: String,
    /// Amount granted, as a decimal string on the wire.
    pub amount: Decimal,
}

/// `GET /faucet`: what the faucet would give and whether it will give it now.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct FaucetStatus {
    /// True when a claim would be accepted right now.
    pub available: bool,
    /// When the next claim unlocks; `None` while `available` is true.
    pub next_available_at: Option<String>,
    /// Cooldown between claims, in seconds.
    pub cooldown_seconds: i64,
    /// The grant that would be paid out.
    pub grant: Vec<Grant>,
    /// How many claims this account has made.
    pub claims_made: u32,
}

/// `POST /faucet/claim` response.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct FaucetClaim {
    /// Claim identifier, `clm_1f2e`.
    pub claim_id: String,
    /// The transaction the grant landed in.
    pub tx_hash: String,
    /// What was actually paid out.
    pub granted: Vec<Grant>,
    /// When the next claim unlocks.
    pub next_available_at: String,
}

// ------------------------------------------------------------ 4. Market data

/// Whether an instrument is spot, a perpetual or a dated future.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketKind {
    /// Delivery against the base asset, no margin.
    Spot,
    /// A perpetual swap, funded periodically.
    Perp,
    /// A future with an expiry and a settlement.
    Future,
}

impl MarketKind {
    /// The badge text the design system uses for this kind.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Spot => "Spot",
            Self::Perp => "Perp",
            Self::Future => "Future",
        }
    }

    /// True when the instrument carries leverage, margin and a liquidation price.
    #[must_use]
    pub const fn is_derivative(self) -> bool {
        matches!(self, Self::Perp | Self::Future)
    }
}

/// Which underlying market the instrument references.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetClass {
    /// Crypto underlyings.
    Crypto,
    /// Equity underlyings.
    Equity,
}

impl AssetClass {
    /// The badge text the design system uses for this class.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Crypto => "Crypto",
            Self::Equity => "Equities",
        }
    }
}

/// Whether a market is accepting orders, and if not, why not.
///
/// Three of the nine states the design system requires are derived from this
/// field alone, so it is never collapsed into a boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketStatus {
    /// Normal operation.
    Trading,
    /// Suspended: resting orders survive, the book stops being clickable.
    Halted,
    /// Past expiry and settled; the market keeps its place in the list.
    Expired,
    /// Past expiry, settlement transaction not yet landed.
    Settling,
}

impl MarketStatus {
    /// The badge text for this status.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Trading => "Trading",
            Self::Halted => "Halted",
            Self::Expired => "Settled",
            Self::Settling => "Settling",
        }
    }

    /// True when orders may be placed.
    #[must_use]
    pub const fn is_tradable(self) -> bool {
        matches!(self, Self::Trading)
    }
}

/// A market definition from `GET /markets`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Market {
    /// `BTC-PERP`, `AAPL-USD`, `ETH-260626`.
    pub symbol: String,
    /// Spot, perpetual or dated future.
    pub kind: MarketKind,
    /// Crypto or equity underlying.
    pub asset_class: AssetClass,
    /// Base asset symbol.
    pub base: String,
    /// Quote asset symbol.
    pub quote: String,
    /// Human-facing name, `Bitcoin Perpetual`.
    pub display_name: String,
    /// Trading, halted, expired or settling.
    pub status: MarketStatus,
    /// The market's own price increment; the price stepper steps by this.
    pub tick_size: Decimal,
    /// The market's own size increment; the size stepper steps by this.
    pub lot_size: Decimal,
    /// Smallest order value the market will accept.
    pub min_notional: Decimal,
    /// Decimal places prices are displayed at.
    pub price_decimals: u32,
    /// Decimal places sizes are displayed at.
    pub size_decimals: u32,
    /// Highest leverage the market allows; null for spot.
    pub max_leverage: Option<Decimal>,
    /// Initial margin requirement in basis points; null for spot.
    pub initial_margin_bps: Option<u32>,
    /// Maintenance margin requirement in basis points; null for spot.
    pub maintenance_margin_bps: Option<u32>,
    /// Seconds between funding payments; null for spot.
    pub funding_interval_seconds: Option<i64>,
    /// Expiry instant; non-null only for futures.
    pub expiry: Option<String>,
    /// Maker fee in basis points; negative means a rebate.
    pub maker_fee_bps: i32,
    /// Taker fee in basis points.
    pub taker_fee_bps: i32,
}

/// `GET /markets` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct MarketsResponse {
    /// Every market the exchange lists.
    pub markets: Vec<Market>,
}

/// `GET /markets/{symbol}/ticker`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Ticker {
    /// The market this ticker describes.
    pub symbol: String,
    /// Last traded price.
    pub last_price: Decimal,
    /// Mark price, which margin is computed against.
    pub mark_price: Decimal,
    /// Index price from the oracle.
    pub index_price: Decimal,
    /// Open interest in base units.
    pub open_interest: Decimal,
    /// Rolling 24-hour quote volume.
    pub volume_24h: Decimal,
    /// 24-hour change as a percentage, already scaled: `"2.41"` is 2.41%.
    pub change_24h_pct: Decimal,
    /// 24-hour high.
    pub high_24h: Decimal,
    /// 24-hour low.
    pub low_24h: Decimal,
    /// Funding rate per interval as a fraction; null for spot.
    pub funding_rate: Option<Decimal>,
    /// When funding is next charged; null for spot.
    pub next_funding_at: Option<String>,
}

/// One aggregated order-book level: `["64250.00", "1.2500"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct Level(pub Decimal, pub Decimal);

impl Level {
    /// The price of this level.
    #[must_use]
    pub const fn price(&self) -> Decimal {
        self.0
    }

    /// The resting quantity at this level. A delta carrying `0` removes it.
    #[must_use]
    pub const fn qty(&self) -> Decimal {
        self.1
    }
}

/// `GET /markets/{symbol}/orderbook`, and the `snapshot` websocket message.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct OrderBookPayload {
    /// The block height this book state belongs to.
    pub height: u64,
    /// Bids, best first.
    #[serde(default)]
    pub bids: Vec<Level>,
    /// Asks, best first.
    #[serde(default)]
    pub asks: Vec<Level>,
}

/// Whether the aggressor bought or sold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    /// Bid side: buy, long.
    Buy,
    /// Ask side: sell, short.
    Sell,
}

impl Side {
    /// The word the interface shows. Side is read before its colour is.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Buy => "Buy",
            Self::Sell => "Sell",
        }
    }

    /// The opposite side.
    #[must_use]
    pub const fn flip(self) -> Self {
        match self {
            Self::Buy => Self::Sell,
            Self::Sell => Self::Buy,
        }
    }
}

/// A public trade from the tape.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Trade {
    /// Trade identifier.
    #[serde(default)]
    pub id: String,
    /// Execution price.
    pub price: Decimal,
    /// Executed quantity.
    pub qty: Decimal,
    /// The aggressor's side.
    pub side: Side,
    /// RFC 3339 execution time.
    pub at: String,
    /// The block this trade landed in.
    #[serde(default)]
    pub height: u64,
}

/// `GET /markets/{symbol}/trades` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TradesResponse {
    /// Most recent trades, newest first.
    pub trades: Vec<Trade>,
}

/// A candle. Field names are the contract's single letters.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Candle {
    /// Period open time, RFC 3339.
    pub t: String,
    /// Open.
    pub o: Decimal,
    /// High.
    pub h: Decimal,
    /// Low.
    pub l: Decimal,
    /// Close.
    pub c: Decimal,
    /// Volume in base units.
    pub v: Decimal,
}

/// `GET /markets/{symbol}/candles` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CandlesResponse {
    /// The market.
    pub symbol: String,
    /// The interval these candles are bucketed at.
    pub interval: String,
    /// Oldest first.
    pub candles: Vec<Candle>,
}

/// One historical funding payment.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct FundingEntry {
    /// When funding was charged.
    pub at: String,
    /// The rate applied, as a fraction.
    pub rate: Decimal,
    /// Mark price at the time.
    pub mark_price: Decimal,
    /// Index price at the time.
    pub index_price: Decimal,
}

/// `GET /markets/{symbol}/funding` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct FundingResponse {
    /// Newest first.
    pub history: Vec<FundingEntry>,
}

// ---------------------------------------------------------------- 5. Account

/// A per-asset balance.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Balance {
    /// Asset symbol.
    pub asset: String,
    /// Spendable balance.
    pub free: Decimal,
    /// Balance committed to resting orders and margin.
    pub locked: Decimal,
    /// Free plus locked.
    pub total: Decimal,
}

/// How close the account is to being liquidated.
///
/// The portfolio's four-zone gauge maps exactly onto these four values, so the
/// gauge and the API can never disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LiquidationRisk {
    /// Nothing is drawn. The meter does not appear at all.
    Low,
    /// The proximity meter appears in brass.
    Medium,
    /// The meter turns vermilion and a persistent banner appears.
    High,
    /// Hazard texture, and the banner follows the user to every screen.
    Critical,
}

impl LiquidationRisk {
    /// The word shown beside the gauge.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Low => "Low risk",
            Self::Medium => "Medium risk",
            Self::High => "High risk",
            Self::Critical => "Critical risk",
        }
    }

    /// The `data-risk` attribute the lifted CSS keys its meter colours off.
    #[must_use]
    pub const fn attr(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

/// The portfolio summary embedded in `GET /account`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Portfolio {
    /// Total account value including unrealised profit and loss.
    pub equity: Decimal,
    /// Margin still available to commit.
    pub available_margin: Decimal,
    /// Margin currently committed.
    pub used_margin: Decimal,
    /// Unrealised profit and loss across open positions.
    pub unrealized_pnl: Decimal,
    /// Realised profit and loss over the last 24 hours.
    pub realized_pnl_24h: Decimal,
    /// Used margin over equity, as a fraction: `"0.462"` is 46.2%.
    pub margin_ratio: Decimal,
    /// The bucket the ratio falls in.
    pub liquidation_risk: LiquidationRisk,
}

/// `GET /account`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Account {
    /// The EVM address the user signed in with.
    pub login_address: String,
    /// The on-chain account holding the balances below.
    ///
    /// Shown beside the login address and labelled, never silently substituted
    /// for it: somebody who sees an unfamiliar address holding their money will
    /// reasonably assume something has gone wrong.
    pub trading_address: String,
    /// Next transaction nonce.
    pub nonce: u64,
    /// Per-asset balances.
    pub balances: Vec<Balance>,
    /// Aggregated risk and equity.
    pub portfolio: Portfolio,
}

/// Where an order is in its life.
///
/// Four of the nine states are derived from this field, which is why it is an
/// enum and not a string: `pending` renders differently from `open`, and a
/// partially filled order is never rendered like an unfilled one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    /// Submitted, not yet in a block. Cancel is disabled: there is nothing to cancel.
    Pending,
    /// Accepted and resting.
    Open,
    /// Resting with some quantity already executed.
    PartiallyFilled,
    /// Fully executed.
    Filled,
    /// Withdrawn by the user or the engine.
    Cancelled,
    /// Refused, with a reason.
    Rejected,
}

impl OrderStatus {
    /// The badge text for this status.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Open => "Open",
            Self::PartiallyFilled => "Partial",
            Self::Filled => "Filled",
            Self::Cancelled => "Cancelled",
            Self::Rejected => "Rejected",
        }
    }

    /// The `.badge` modifier class the lifted CSS provides for this status.
    #[must_use]
    pub const fn badge_class(self) -> &'static str {
        match self {
            Self::Pending => "badge pending",
            Self::Open | Self::PartiallyFilled => "badge live",
            Self::Rejected => "badge stop",
            Self::Filled | Self::Cancelled => "badge",
        }
    }

    /// True while the order can still trade.
    #[must_use]
    pub const fn is_live(self) -> bool {
        matches!(self, Self::Pending | Self::Open | Self::PartiallyFilled)
    }
}

/// Limit or market.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OrderType {
    /// Rests at a price.
    Limit,
    /// Takes whatever is there.
    Market,
}

impl OrderType {
    /// The word shown in the blotter.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Limit => "Limit",
            Self::Market => "Market",
        }
    }
}

/// Time in force. Rendered as a letterspaced instrument tag, which is the one
/// place the design system permits letterspaced capitals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TimeInForce {
    /// Good till cancelled.
    Gtc,
    /// Immediate or cancel.
    Ioc,
    /// Fill or kill.
    Fok,
}

impl TimeInForce {
    /// The instrument tag, lower case; the CSS applies the capitals.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Gtc => "gtc",
            Self::Ioc => "ioc",
            Self::Fok => "fok",
        }
    }
}

/// A resting, filled or rejected order.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Order {
    /// On-chain order identifier.
    pub order_id: String,
    /// The idempotency key the client supplied.
    #[serde(default)]
    pub client_order_id: Option<String>,
    /// The market.
    pub symbol: String,
    /// Buy or sell.
    pub side: Side,
    /// Limit or market.
    #[serde(rename = "type")]
    pub order_type: OrderType,
    /// Limit price; absent for market orders.
    pub price: Option<Decimal>,
    /// Original quantity.
    pub qty: Decimal,
    /// Quantity executed so far.
    pub filled_qty: Decimal,
    /// Quantity still resting.
    pub remaining_qty: Decimal,
    /// Average price of the executed quantity.
    pub avg_fill_price: Option<Decimal>,
    /// Where the order is in its life.
    pub status: OrderStatus,
    /// Time in force.
    pub tif: TimeInForce,
    /// True when the order must not take liquidity.
    pub post_only: bool,
    /// True when the order must not grow the position.
    pub reduce_only: bool,
    /// Submission time.
    pub created_at: String,
    /// Last change.
    pub updated_at: String,
    /// The transaction carrying this order; present once it has been submitted.
    #[serde(default)]
    pub tx_hash: Option<String>,
    /// The reason a rejected order was refused. Never inferred: the gateway's
    /// own words, so the client guard and the server rejection say the same thing.
    #[serde(default)]
    pub reject_reason: Option<String>,
}

impl Order {
    /// The executed fraction of the order as a whole percentage, 0–100.
    ///
    /// Computed in [`Decimal`] and emitted as an integer only because it ends up
    /// as a CSS width; the quantities themselves are never rounded.
    #[must_use]
    pub fn filled_pct(&self) -> u32 {
        if self.qty <= Decimal::ZERO {
            return 0;
        }
        let hundred = Decimal::from(100);
        let pct = (self.filled_qty / self.qty) * hundred;
        pct.clamp(Decimal::ZERO, hundred)
            .round()
            .try_into()
            .unwrap_or(0u32)
    }
}

/// `GET /account/orders` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct OrdersResponse {
    /// The orders matching the requested status.
    pub orders: Vec<Order>,
}

/// Long or short.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PositionSide {
    /// Gains when the mark rises.
    Long,
    /// Gains when the mark falls.
    Short,
}

impl PositionSide {
    /// The badge word, read before the colour is.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Long => "Long",
            Self::Short => "Short",
        }
    }
}

/// Cross or isolated margin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MarginMode {
    /// The whole account backs the position.
    Cross,
    /// Only the margin posted to this position backs it.
    Isolated,
}

impl MarginMode {
    /// The word shown in the ticket and the position badge.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Cross => "Cross",
            Self::Isolated => "Isolated",
        }
    }
}

/// An open derivative position.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Position {
    /// The market.
    pub symbol: String,
    /// Long or short.
    pub side: PositionSide,
    /// Size in base units.
    pub qty: Decimal,
    /// Volume-weighted entry price.
    pub entry_price: Decimal,
    /// Current mark price.
    pub mark_price: Decimal,
    /// The price at which the engine closes this position.
    pub liquidation_price: Decimal,
    /// Position value at the mark.
    pub notional: Decimal,
    /// Cross or isolated.
    pub margin_mode: MarginMode,
    /// Leverage multiple.
    pub leverage: Decimal,
    /// Margin committed to this position.
    pub margin: Decimal,
    /// Unrealised profit and loss.
    pub unrealized_pnl: Decimal,
    /// Unrealised profit and loss as a percentage of margin.
    pub unrealized_pnl_pct: Decimal,
    /// Funding paid so far; negative means paid out.
    pub funding_paid: Decimal,
    /// When the position was opened.
    pub opened_at: String,
}

impl Position {
    /// How far the mark has travelled from entry toward liquidation, 0–100.
    ///
    /// This is the proximity meter's width. The contract gives
    /// `liquidation_risk` only at account level, so per-position severity is
    /// derived here from distance and named against the same four zones the
    /// portfolio gauge uses; the account-level value still decides whether any
    /// meter is drawn at all.
    #[must_use]
    pub fn liquidation_proximity(&self) -> u32 {
        let span = (self.entry_price - self.liquidation_price).abs();
        if span <= Decimal::ZERO {
            return 0;
        }
        let travelled = (self.entry_price - self.mark_price).abs();
        let hundred = Decimal::from(100);
        ((travelled / span) * hundred)
            .clamp(Decimal::ZERO, hundred)
            .round()
            .try_into()
            .unwrap_or(0u32)
    }

    /// The severity band this single position sits in.
    ///
    /// The thresholds are the portfolio gauge's own zone boundaries — 40, 65 and
    /// 80 percent — so a meter and the gauge above it can never tell different
    /// stories about the same account.
    #[must_use]
    pub fn risk(&self) -> LiquidationRisk {
        match self.liquidation_proximity() {
            0..=39 => LiquidationRisk::Low,
            40..=64 => LiquidationRisk::Medium,
            65..=79 => LiquidationRisk::High,
            _ => LiquidationRisk::Critical,
        }
    }
}

/// `GET /account/positions` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PositionsResponse {
    /// Every open position.
    pub positions: Vec<Position>,
}

/// Whether a fill added or removed liquidity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FillRole {
    /// Provided liquidity; usually a rebate.
    Maker,
    /// Took liquidity; pays the taker fee.
    Taker,
}

impl FillRole {
    /// The word shown in the fills blotter.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Maker => "Maker",
            Self::Taker => "Taker",
        }
    }
}

/// One execution against one of the account's orders.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Fill {
    /// Fill identifier.
    pub fill_id: String,
    /// The order this fill belongs to.
    pub order_id: String,
    /// The market.
    pub symbol: String,
    /// The account's side.
    pub side: Side,
    /// Execution price.
    pub price: Decimal,
    /// Executed quantity.
    pub qty: Decimal,
    /// Fee charged; negative is a rebate.
    pub fee: Decimal,
    /// The asset the fee was charged in.
    pub fee_asset: String,
    /// Maker or taker.
    pub role: FillRole,
    /// Profit or loss realised by this fill.
    pub realized_pnl: Decimal,
    /// Execution time.
    pub at: String,
    /// The block this fill landed in — on this product, the receipt.
    pub height: u64,
}

/// `GET /account/fills` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct FillsResponse {
    /// Newest first.
    pub fills: Vec<Fill>,
}

/// What moved a balance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerKind {
    /// A faucet grant.
    Faucet,
    /// An inbound transfer.
    TransferIn,
    /// An outbound transfer.
    TransferOut,
    /// A trading fee.
    Fee,
    /// A funding payment.
    Funding,
    /// Profit or loss crystallised by a close.
    RealizedPnl,
    /// A forced close by the engine.
    Liquidation,
    /// A future settling at expiry.
    Settlement,
}

impl LedgerKind {
    /// The sentence-case name the ledger column shows.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Faucet => "Faucet",
            Self::TransferIn => "Transfer in",
            Self::TransferOut => "Transfer out",
            Self::Fee => "Fee",
            Self::Funding => "Funding",
            Self::RealizedPnl => "Realised profit",
            Self::Liquidation => "Liquidation",
            Self::Settlement => "Settlement",
        }
    }
}

/// One line of the append-only account ledger.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LedgerEntry {
    /// Entry identifier.
    pub id: String,
    /// What moved the balance.
    pub kind: LedgerKind,
    /// The asset that moved.
    pub asset: String,
    /// Signed amount.
    pub amount: Decimal,
    /// Balance in that asset after the movement.
    pub balance_after: Decimal,
    /// Free-form description.
    #[serde(default)]
    pub memo: String,
    /// When it happened.
    pub at: String,
    /// The transaction that carried it.
    pub tx_hash: String,
    /// The block it landed in. Every movement here is traceable to a block.
    #[serde(default)]
    pub height: u64,
}

/// `GET /account/transfers` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LedgerResponse {
    /// Newest first.
    pub entries: Vec<LedgerEntry>,
}

// ---------------------------------------------------------------- 6. Trading

/// `POST /orders` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlaceOrderRequest {
    /// The market.
    pub symbol: String,
    /// Buy or sell.
    pub side: Side,
    /// Limit or market.
    #[serde(rename = "type")]
    pub order_type: OrderType,
    /// Limit price; omitted for market orders.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<Decimal>,
    /// Quantity in base units.
    pub qty: Decimal,
    /// Time in force.
    pub tif: TimeInForce,
    /// Refuse the order rather than take liquidity.
    pub post_only: bool,
    /// Refuse the order rather than grow the position.
    pub reduce_only: bool,
    /// Leverage; derivative markets only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leverage: Option<Decimal>,
    /// Margin mode; derivative markets only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub margin_mode: Option<MarginMode>,
    /// Idempotency key. Re-submitting the same id within 24 hours returns the
    /// original order rather than a duplicate.
    pub client_order_id: String,
}

/// An execution that happened at submission time.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ImmediateFill {
    /// Execution price.
    pub price: Decimal,
    /// Executed quantity.
    pub qty: Decimal,
    /// Fee charged.
    pub fee: Decimal,
}

/// `POST /orders` response.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PlaceOrderResponse {
    /// The new order's identifier.
    pub order_id: String,
    /// The idempotency key echoed back.
    pub client_order_id: String,
    /// Where the order is now.
    pub status: OrderStatus,
    /// The transaction carrying it. This is the receipt the toast names.
    pub tx_hash: String,
    /// When the sequencer accepted it.
    pub accepted_at: String,
    /// Anything that executed immediately.
    #[serde(default)]
    pub immediate_fills: Vec<ImmediateFill>,
}

/// `DELETE /orders/{order_id}` response.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CancelOrderResponse {
    /// The order that was cancelled.
    pub order_id: String,
    /// Its new status.
    pub status: OrderStatus,
    /// The transaction that cancelled it.
    pub tx_hash: String,
}

/// `POST /orders/cancel-all` response.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CancelAllResponse {
    /// How many resting orders were withdrawn.
    pub cancelled: u32,
    /// The transaction that withdrew them.
    pub tx_hash: String,
}

/// `POST /positions/close` response.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ClosePositionResponse {
    /// The transaction that closed it.
    pub tx_hash: String,
    /// How much was closed.
    pub closed_qty: Decimal,
    /// The average price it closed at.
    pub avg_price: Decimal,
    /// Profit or loss crystallised.
    pub realized_pnl: Decimal,
}

// --------------------------------------------- 7. Chain and leaderboard

/// `GET /chain/status`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ChainStatus {
    /// Which chain this is.
    pub chain_id: u64,
    /// Current block height.
    pub height: u64,
    /// State root at that height.
    pub state_root: String,
    /// Target block interval in milliseconds.
    pub block_time_ms: u64,
    /// Transactions per second over the last minute.
    pub tps_1m: Decimal,
    /// Transactions since genesis.
    pub tx_count: u64,
    /// Transactions waiting to be included.
    pub mempool_depth: u32,
    /// How many validators are participating.
    pub validators: u32,
    /// False turns the whole rail brass: the one time the rail is loud.
    pub synced: bool,
    /// How long the chain has been up.
    pub uptime_seconds: u64,
    /// The sequencer's Cap'n Proto RPC address the gateway fronts.
    #[serde(default)]
    pub node_rpc: String,
    /// Where the gateway listens.
    #[serde(default)]
    pub gateway_listen: String,
    /// The WebSocket path on the gateway's root.
    #[serde(default = "default_ws_path")]
    pub ws_path: String,
    /// The oracle's scenario-control URL, when configured.
    #[serde(default)]
    pub oracle_control: Option<String>,
    /// The simulation switch position.
    #[serde(default)]
    pub sim_mode: SimMode,
}

fn default_ws_path() -> String {
    "/ws".to_string()
}

/// One transaction the chain applied, as `GET /chain/txs` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ChainTx {
    /// Transaction hash.
    pub hash: String,
    /// The block that carried it.
    pub height: u64,
    /// Position in that block.
    pub index: u32,
    /// Who signed it.
    pub sender: String,
    /// The chain's own name for what it did.
    pub kind: String,
    /// The market it touched, when it touched one.
    #[serde(default)]
    pub market: Option<String>,
    /// `buy` or `sell`, when it has a side.
    #[serde(default)]
    pub side: Option<String>,
    /// When its block was produced.
    pub at: String,
}

/// `GET /chain/txs` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TxsResponse {
    /// Newest first.
    pub txs: Vec<ChainTx>,
}

/// A block header.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Block {
    /// Height.
    pub height: u64,
    /// Block hash.
    pub hash: String,
    /// State root after applying the block.
    pub state_root: String,
    /// How many transactions it carried.
    pub tx_count: u32,
    /// When it was produced.
    pub at: String,
    /// Who proposed it.
    pub proposer: String,
}

/// `GET /chain/blocks` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct BlocksResponse {
    /// Newest first.
    pub blocks: Vec<Block>,
}

/// One row of the leaderboard.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LeaderboardEntry {
    /// Rank, which comes from the API and is never recomputed on the client.
    pub rank: u32,
    /// The trader's handle.
    pub handle: String,
    /// Their address.
    pub address: String,
    /// Their equity.
    pub equity: Decimal,
    /// Profit and loss over the window.
    pub pnl: Decimal,
    /// Return over the window as a percentage.
    pub pnl_pct: Decimal,
    /// Traded volume over the window.
    pub volume: Decimal,
    /// Number of trades.
    pub trades: u64,
    /// True for scripted traders. They are marked, always.
    pub is_bot: bool,
    /// The strategy a bot runs, named on the badge.
    #[serde(default)]
    pub bot_strategy: Option<String>,
}

/// `GET /leaderboard` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LeaderboardResponse {
    /// The window these ranks were computed over.
    pub window: String,
    /// Ranked rows.
    pub entries: Vec<LeaderboardEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::str::FromStr;

    #[test]
    fn a_book_level_deserialises_from_a_pair_of_decimal_strings() {
        let level: Level = serde_json::from_str(r#"["64250.00","1.2500"]"#)
            .expect("a level is a two-element array of decimal strings");
        assert_eq!(
            level.price(),
            Decimal::from_str("64250.00").unwrap_or_default()
        );
        assert_eq!(level.qty(), Decimal::from_str("1.2500").unwrap_or_default());
        // The scale survives the round trip: 1.2500 is not 1.25.
        assert_eq!(format!("{}", level.qty()), "1.2500");
    }

    #[test]
    fn a_ticker_deserialises_from_the_contracts_own_example() {
        let json = r#"{"symbol":"BTC-PERP","last_price":"64250.50","mark_price":"64248.10",
          "index_price":"64251.00","open_interest":"1284.5000","volume_24h":"128450000.00",
          "change_24h_pct":"2.41","high_24h":"64980.00","low_24h":"62100.00",
          "funding_rate":"0.000125","next_funding_at":"2026-09-18T13:00:00Z"}"#;
        let ticker: Ticker = serde_json::from_str(json).expect("contract example parses");
        assert_eq!(crate::format::fixed(ticker.last_price, 2), "64,250.50");
        assert_eq!(
            ticker
                .funding_rate
                .map(crate::format::funding_pct)
                .as_deref(),
            Some("+0.0125%")
        );
    }

    #[test]
    fn an_order_request_serialises_money_as_strings() {
        let request = PlaceOrderRequest {
            symbol: "BTC-PERP".into(),
            side: Side::Buy,
            order_type: OrderType::Limit,
            price: Decimal::from_str("64000.00").ok(),
            qty: Decimal::from_str("1.0000").unwrap_or_default(),
            tif: TimeInForce::Gtc,
            post_only: false,
            reduce_only: false,
            leverage: Decimal::from_str("10").ok(),
            margin_mode: Some(MarginMode::Cross),
            client_order_id: "cli_a1b2c3".into(),
        };
        let json = serde_json::to_string(&request).unwrap_or_default();
        assert!(json.contains(r#""price":"64000.00""#), "{json}");
        assert!(json.contains(r#""qty":"1.0000""#), "{json}");
        assert!(json.contains(r#""type":"limit""#), "{json}");
    }

    #[test]
    fn market_status_and_order_status_parse_the_contracts_spellings() {
        let m: MarketStatus =
            serde_json::from_str(r#""settling""#).unwrap_or(MarketStatus::Trading);
        assert_eq!(m, MarketStatus::Settling);
        let o: OrderStatus =
            serde_json::from_str(r#""partially_filled""#).unwrap_or(OrderStatus::Open);
        assert_eq!(o, OrderStatus::PartiallyFilled);
    }
}

// ------------------------------------------------------------- 9. User page

/// The lifetime statistics on a user's own account page.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProfileStats {
    /// Trades made, all time.
    pub trades: u64,
    /// Volume traded over thirty days.
    pub volume_30d: Decimal,
    /// Profit and loss realised over thirty days.
    pub realized_pnl_30d: Decimal,
    /// How many distinct markets the account has traded.
    pub markets_traded: u32,
    /// How many times the account has been liquidated.
    pub liquidations: u32,
    /// How many faucet grants have been claimed.
    pub faucet_claims: u32,
    /// The best leaderboard rank ever reached.
    pub best_rank: Option<u32>,
}

/// `GET /user/profile`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Profile {
    /// Opaque user identifier.
    pub user_id: String,
    /// The handle, the only mutable field on this record.
    pub handle: String,
    /// The login address, which is the identity and cannot change.
    pub address: String,
    /// When the account was created.
    pub joined_at: String,
    /// When it was last active.
    pub last_seen_at: String,
    /// True for a scripted trader.
    pub is_bot: bool,
    /// `user`, and `admin` for an operator.
    #[serde(default)]
    pub roles: Vec<String>,
    /// Lifetime statistics.
    pub stats: ProfileStats,
}

/// `PATCH /user/profile` body. Only the handle is mutable.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateProfileRequest {
    /// The handle to move to.
    pub handle: String,
}

/// One signed-in session on the account.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct UserSession {
    /// Session identifier, used to revoke this one specifically.
    pub session_id: String,
    /// When it was issued.
    pub issued_at: String,
    /// When it expires.
    pub expires_at: String,
    /// When it was last used.
    pub last_used_at: String,
    /// The browser or client string it was issued to.
    #[serde(default)]
    pub user_agent: String,
    /// A truncated network prefix rather than an exact address: a session list
    /// is a security feature and does not need to be a location log.
    #[serde(default)]
    pub ip_prefix: String,
    /// True for the session making the request, which cannot revoke itself here.
    #[serde(default)]
    pub current: bool,
}

/// `GET /user/sessions` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionsResponse {
    /// Every live session, newest first.
    pub sessions: Vec<UserSession>,
}

/// `POST /user/sessions/revoke-all` response.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RevokedResponse {
    /// How many sessions were ended.
    pub revoked: u32,
}

/// What an activity entry was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    /// An execution.
    Fill,
    /// An order placed, cancelled or rejected.
    Order,
    /// A faucet grant.
    Faucet,
    /// A funding payment.
    Funding,
    /// An inbound transfer.
    TransferIn,
    /// An outbound transfer.
    TransferOut,
    /// A forced close.
    Liquidation,
    /// A future settling.
    Settlement,
    /// A kind this build does not know; the summary still reads.
    #[serde(other)]
    Other,
}

impl ActivityKind {
    /// The sentence-case name shown on the badge.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fill => "Fill",
            Self::Order => "Order",
            Self::Faucet => "Faucet",
            Self::Funding => "Funding",
            Self::TransferIn => "Transfer in",
            Self::TransferOut => "Transfer out",
            Self::Liquidation => "Liquidation",
            Self::Settlement => "Settlement",
            Self::Other => "Activity",
        }
    }

    /// The `.badge` modifier this kind wears.
    #[must_use]
    pub const fn badge_class(self) -> &'static str {
        match self {
            Self::Fill => "badge live",
            Self::Liquidation => "badge stop",
            Self::Funding => "badge warn",
            Self::Order | Self::Settlement => "badge pending",
            _ => "badge",
        }
    }
}

/// One line of the unified activity feed.
///
/// The contract is explicit about why this exists beside the per-type
/// endpoints: this is what a person wants to read, those are what a table wants
/// to render.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ActivityEntry {
    /// Entry identifier.
    pub id: String,
    /// What happened.
    pub kind: ActivityKind,
    /// When.
    pub at: String,
    /// A written sentence, already formatted by the gateway.
    pub summary: String,
    /// The market involved, where there is one.
    #[serde(default)]
    pub symbol: Option<String>,
    /// The signed balance movement, where there was one.
    #[serde(default)]
    pub amount: Option<Decimal>,
    /// The asset that moved.
    #[serde(default)]
    pub asset: Option<String>,
    /// The transaction it landed in.
    #[serde(default)]
    pub tx_hash: Option<String>,
    /// The block it landed in.
    #[serde(default)]
    pub height: u64,
}

/// `GET /user/activity` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ActivityResponse {
    /// Newest first.
    pub entries: Vec<ActivityEntry>,
    /// The cursor for the next page, or null at the end.
    #[serde(default)]
    pub next_cursor: Option<String>,
}

// ----------------------------------------------------------------- 10. Admin

/// The chain block of `GET /admin/overview`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AdminChain {
    /// Current height.
    pub height: u64,
    /// Target block interval.
    pub block_time_ms: u64,
    /// Throughput over the last minute.
    pub tps_1m: Decimal,
    /// Transactions waiting.
    pub mempool_depth: u32,
    /// Participating validators.
    pub validators: u32,
    /// Whether the node is caught up.
    pub synced: bool,
    /// State root at the current height.
    pub state_root: String,
    /// How long the chain has been up.
    pub uptime_seconds: u64,
}

/// The user counts of `GET /admin/overview`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AdminUsers {
    /// Every account.
    pub total: u64,
    /// Accounts that are people.
    pub humans: u64,
    /// Accounts that are bots.
    pub bots: u64,
    /// Accounts active in the last day.
    pub active_24h: u64,
    /// Accounts created in the last day.
    pub new_24h: u64,
}

/// The market counts of `GET /admin/overview`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AdminMarkets {
    /// Every listed market.
    pub total: u32,
    /// Markets accepting orders.
    pub trading: u32,
    /// Markets suspended.
    pub halted: u32,
    /// Markets past expiry.
    pub expired: u32,
}

/// The volume block of `GET /admin/overview`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AdminVolume {
    /// Quote volume over the last day.
    pub volume_24h: Decimal,
    /// Trades over the last day.
    pub trades_24h: u64,
    /// Fees taken over the last day.
    pub fees_24h: Decimal,
}

/// The faucet block of `GET /admin/overview`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AdminFaucetSummary {
    /// Claims in the last day.
    pub claims_24h: u64,
    /// What was granted in the last day.
    pub granted_24h: Vec<Grant>,
    /// What has been granted since genesis.
    pub total_granted: Vec<Grant>,
}

/// The risk block of `GET /admin/overview`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AdminRisk {
    /// Total open interest across the exchange.
    pub open_interest: Decimal,
    /// What the insurance fund holds.
    pub insurance_fund: Decimal,
    /// How many accounts the engine is about to act on.
    pub accounts_below_maintenance: u32,
    /// Forced closes in the last day.
    pub liquidations_24h: u64,
}

/// Whether a component is working.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    /// Working.
    Ok,
    /// Working, but not well.
    Degraded,
    /// Not working.
    Down,
}

impl HealthStatus {
    /// The `.badge` modifier this status wears.
    ///
    /// Brass for degraded and vermilion for down: these are system states, so
    /// they never borrow the market's jade.
    #[must_use]
    pub const fn badge_class(self) -> &'static str {
        match self {
            Self::Ok => "badge live",
            Self::Degraded => "badge warn",
            Self::Down => "badge stop",
        }
    }

    /// The word shown beside the component's name.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Degraded => "Degraded",
            Self::Down => "Down",
        }
    }
}

/// One component's health.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ComponentHealth {
    /// What the component is called.
    pub component: String,
    /// Whether it is working.
    pub status: HealthStatus,
    /// What is wrong, where anything is.
    #[serde(default)]
    pub detail: Option<String>,
}

/// `GET /admin/overview`: the dashboard's single fetch.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AdminOverview {
    /// Chain health.
    pub chain: AdminChain,
    /// Account counts.
    pub users: AdminUsers,
    /// Market counts.
    pub markets: AdminMarkets,
    /// Traded volume.
    pub volume: AdminVolume,
    /// Faucet throughput.
    pub faucet: AdminFaucetSummary,
    /// Exchange-wide risk.
    pub risk: AdminRisk,
    /// Per-component health.
    #[serde(default)]
    pub health: Vec<ComponentHealth>,
}

/// Whether an account may trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AccountStatus {
    /// Trading normally.
    Active,
    /// Stopped by an operator. Existing positions stay open.
    Suspended,
}

impl AccountStatus {
    /// The word shown on the badge.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Active => "Active",
            Self::Suspended => "Suspended",
        }
    }
}

/// One row of the admin user list.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AdminUser {
    /// Opaque user identifier.
    pub user_id: String,
    /// The handle.
    pub handle: String,
    /// The login address.
    pub address: String,
    /// True for a scripted trader.
    pub is_bot: bool,
    /// When the account was created.
    pub joined_at: String,
    /// When it was last active.
    pub last_seen_at: String,
    /// Account equity.
    pub equity: Decimal,
    /// How many positions are open.
    pub open_positions: u32,
    /// Trades in the last day.
    pub trades_24h: u64,
    /// Faucet grants claimed.
    pub faucet_claims: u32,
    /// Active or suspended.
    pub status: AccountStatus,
}

/// `GET /admin/users` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AdminUsersResponse {
    /// Matching accounts.
    pub users: Vec<AdminUser>,
    /// The cursor for the next page, or null at the end.
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// One market's live operational state, from `GET /admin/markets`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AdminMarket {
    /// The market.
    pub symbol: String,
    /// Trading, halted, expired or settling.
    pub status: MarketStatus,
    /// Last traded price.
    pub last_price: Decimal,
    /// The engine's mark, which margin and the settlement guard are computed
    /// against. Zero on a market whose oracle has never published.
    #[serde(default)]
    pub mark_price: Decimal,
    /// Open interest in base units.
    pub open_interest: Decimal,
    /// How many blocks since the mark was computed.
    ///
    /// `None` means the oracle has **never** published, which is not the same
    /// as "published zero blocks ago". Rendering a null age as a duration turned
    /// the most alarming state in this column into the most reassuring one.
    #[serde(default)]
    pub oracle_age_blocks: Option<u64>,
    /// Whether the mark is fresh enough to trade against.
    #[serde(default)]
    pub oracle_fresh: bool,
    /// How many price levels are resting on the book.
    #[serde(default)]
    pub resting_levels: u64,
    /// Whether an operator override is in force.
    #[serde(default)]
    pub overridden: bool,
    /// Highest leverage the market allows.
    ///
    /// `/admin/markets` does not carry the risk parameters; they come from the
    /// public `/markets`. The fields stay here so a gateway that starts sending
    /// them is used in preference, and so the dialog can tell "not sent" from
    /// "one times".
    #[serde(default)]
    pub max_leverage: Option<Decimal>,
    /// Initial margin requirement in basis points.
    #[serde(default)]
    pub initial_margin_bps: Option<u32>,
    /// Maintenance margin requirement in basis points.
    #[serde(default)]
    pub maintenance_margin_bps: Option<u32>,
}

impl AdminMarket {
    /// The mark the settlement guard compares against, if there is one.
    ///
    /// A market whose oracle has never published has **no** mark, and a zero is
    /// not a low price — it is the absence of one. Everything downstream of this
    /// has to distinguish the two, because `0/0` is not a deviation of zero
    /// per cent, it is not a deviation at all.
    #[must_use]
    pub fn mark(&self) -> Option<Decimal> {
        let mark = if self.mark_price.is_zero() {
            self.last_price
        } else {
            self.mark_price
        };
        (!mark.is_zero()).then_some(mark)
    }
}

/// `GET /admin/markets` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AdminMarketsResponse {
    /// Every market.
    pub markets: Vec<AdminMarket>,
}

/// `PATCH /admin/markets/{symbol}/risk` response.
///
/// The count is the whole point of the response: raising a margin requirement
/// can put open accounts below maintenance immediately, and the operator finds
/// out here rather than from the liquidation engine.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RiskApplied {
    /// Whether the change took effect. `false` for a dry run.
    pub applied: bool,
    /// How many accounts the change just put below maintenance.
    pub accounts_now_below_maintenance: u32,
    /// Which accounts those are, so the operator knows whom the change hits.
    #[serde(default)]
    pub affected: Vec<AffectedAccount>,
}

/// One account a risk change puts below maintenance.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AffectedAccount {
    /// Stable internal id.
    pub user_id: String,
    /// Display handle, so the operator knows whom to warn.
    #[serde(default)]
    pub handle: String,
    /// Maintenance requirement over equity before the change.
    #[serde(default)]
    pub margin_ratio_before: Decimal,
    /// Maintenance requirement over equity after it.
    #[serde(default)]
    pub margin_ratio_after: Decimal,
}

/// `GET /admin/faucet`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AdminFaucet {
    /// Whether the faucet is paying out.
    pub enabled: bool,
    /// Cooldown between claims.
    pub cooldown_seconds: i64,
    /// What a claim pays.
    pub grant: Vec<Grant>,
    /// A lifetime cap per account, where there is one.
    #[serde(default)]
    pub max_claims_per_user: Option<u32>,
    /// Claims in the last day.
    pub claims_24h: u64,
    /// What has been granted since genesis.
    pub granted_total: Vec<Grant>,
    /// What the faucet account has left. A faucet that has run dry looks
    /// exactly like a broken faucet to a user, so it is shown.
    pub authority_balance: Vec<Grant>,
}

/// One audited operator action.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AuditRecord {
    /// Record identifier.
    pub record_id: String,
    /// The address that acted, so "who halted this market" always has an answer.
    pub actor: String,
    /// What was done, such as `market.halt`.
    pub action: String,
    /// What it was done to.
    #[serde(default)]
    pub subject: Option<String>,
    /// Free-form context.
    #[serde(default)]
    pub detail: std::collections::BTreeMap<String, String>,
    /// When.
    pub at: String,
}

/// `GET /admin/audit` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AuditResponse {
    /// Newest first.
    pub records: Vec<AuditRecord>,
    /// The cursor for the next page, or null at the end.
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// The market regime the simulator is driving.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scenario {
    /// Quiet two-sided flow.
    Calm,
    /// A sustained rise.
    TrendUp,
    /// A sustained fall.
    TrendDown,
    /// A fast fall with gaps.
    Crash,
    /// A fast rise that hunts shorts.
    Squeeze,
    /// Directionless churn.
    Chop,
}

impl Scenario {
    /// Every scenario, in the order the control shows them.
    pub const ALL: [Self; 6] = [
        Self::Calm,
        Self::TrendUp,
        Self::TrendDown,
        Self::Crash,
        Self::Squeeze,
        Self::Chop,
    ];

    /// The wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Calm => "calm",
            Self::TrendUp => "trend_up",
            Self::TrendDown => "trend_down",
            Self::Crash => "crash",
            Self::Squeeze => "squeeze",
            Self::Chop => "chop",
        }
    }

    /// The sentence-case label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Calm => "Calm",
            Self::TrendUp => "Trend up",
            Self::TrendDown => "Trend down",
            Self::Crash => "Crash",
            Self::Squeeze => "Squeeze",
            Self::Chop => "Chop",
        }
    }
}

/// The operator's three-position simulation switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SimMode {
    /// No default buyer or seller: a lone human finds an empty book.
    #[default]
    Off,
    /// A default buyer and seller, a trickle of orders so the books move.
    On,
    /// The stress test: every funded trader, as fast as the node will take.
    Max,
}

impl SimMode {
    /// Every position, in switch order.
    pub const ALL: [Self; 3] = [Self::Off, Self::On, Self::Max];

    /// The wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
            Self::Max => "max",
        }
    }

    /// The label on the switch.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "SIMULATION OFF",
            Self::On => "SIMULATION ON",
            Self::Max => "MAX SIMULATION",
        }
    }

    /// What the position does, in one sentence.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Off => "No default buyer or seller. The books only move when a person trades.",
            Self::On => "A default buyer and seller: one worker, eight traders, about twenty signed orders a second.",
            Self::Max => "Maximum stress: six workers, sixty traders, tens of thousands of signed orders a second.",
        }
    }
}

/// `GET /admin/sim`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SimStatus {
    /// Whether the bot population is trading.
    pub running: bool,
    /// The switch position.
    #[serde(default)]
    pub mode: SimMode,
    /// The regime being driven.
    pub scenario: Scenario,
    /// The seed the run was started with.
    #[serde(default)]
    pub seed: u64,
    /// How many of each bot strategy are running.
    #[serde(default)]
    pub bots: std::collections::BTreeMap<String, u32>,
    /// How long the run has been going.
    pub elapsed_seconds: u64,
    /// How many orders the population has submitted.
    pub orders_submitted: u64,
    /// How many invariant checks have failed. Anything but zero is a bug in the
    /// exchange, and the dashboard says so in vermilion.
    pub invariant_violations: u64,
}

/// `POST /admin/sim` body.
#[derive(Debug, Clone, Serialize)]
pub struct SimRequest {
    /// `start`, `stop`, `scenario` or `mode`.
    pub action: String,
    /// The switch position to move to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<SimMode>,
    /// The regime to drive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scenario: Option<Scenario>,
    /// The seed to start from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
}
