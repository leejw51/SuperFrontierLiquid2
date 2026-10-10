//! The wire shapes, field for field from the API contract.
//!
//! Every numeric field that represents money is a `String`. JavaScript numbers
//! cannot hold the on-chain integer range, and a trading UI that silently
//! loses precision on a balance is worse than one that will not compile. The
//! conversion to and from these strings is [`crate::decimal`]'s job and
//! happens here, at the boundary, once.
//!
//! These structs are `Serialize` only. Requests are separate types so an
//! optional field in a response cannot accidentally become an optional field
//! in a request.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// `POST /auth/challenge` request.
#[derive(Debug, Deserialize)]
pub struct ChallengeRequest {
    /// The address that wants to sign in.
    pub address: String,
}

/// `POST /auth/challenge`.
#[derive(Debug, Serialize)]
pub struct ChallengeResponse {
    /// Opaque single-use nonce.
    pub nonce: String,
    /// The exact string to sign, verbatim.
    pub message: String,
    /// When the challenge stops being accepted.
    pub expires_at: String,
}

/// `POST /auth/login` request.
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    /// The claimed address.
    pub address: String,
    /// EIP-191 signature over the challenge message.
    pub signature: String,
    /// Required the first time an address signs in; ignored afterwards.
    #[serde(default)]
    pub handle: Option<String>,
}

/// `POST /auth/dev-login` request.
#[derive(Debug, Deserialize)]
pub struct DevLoginRequest {
    /// BIP-39 mnemonic. Sent to the server, which is exactly what the real
    /// flow avoids — see the contract.
    pub mnemonic: String,
    /// BIP-44 address index. Defaults to zero.
    #[serde(default)]
    pub index: Option<u32>,
    /// Required the first time an address signs in.
    #[serde(default)]
    pub handle: Option<String>,
}

/// The reply to login and development login.
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    /// Stable internal id.
    pub user_id: String,
    /// Display handle.
    pub handle: String,
    /// EIP-55 checksummed address.
    pub address: String,
    /// Bearer token.
    pub session_token: String,
    /// When the session stops being accepted.
    pub expires_at: String,
    /// Whether this login created the account.
    pub is_new: bool,
}

/// `GET /auth/session`.
#[derive(Debug, Serialize)]
pub struct SessionInfo {
    /// Stable internal id.
    pub user_id: String,
    /// Display handle.
    pub handle: String,
    /// The address the user signed in with, EIP-55 checksummed.
    pub login_address: String,
    /// The on-chain account holding their money, EIP-55 checksummed.
    pub trading_address: String,
    /// `user`, plus `admin` for an operator account.
    pub roles: Vec<String>,
    /// When the session was issued.
    pub issued_at: String,
    /// When it stops being accepted.
    pub expires_at: String,
}

// ---------------------------------------------------------------------------
// User page
// ---------------------------------------------------------------------------

/// Lifetime statistics shown on the user page.
#[derive(Debug, Serialize)]
pub struct UserStats {
    /// Trades ever.
    pub trades: u64,
    /// Notional traded in the last 30 days.
    pub volume_30d: String,
    /// Realised profit and loss in the last 30 days.
    pub realized_pnl_30d: String,
    /// How many distinct markets have been traded.
    pub markets_traded: u64,
    /// How many times the account has been liquidated.
    pub liquidations: u64,
    /// How many faucet claims have been made.
    pub faucet_claims: u64,
    /// Best leaderboard rank held, across the stored windows.
    pub best_rank: Option<u32>,
}

/// `GET /user/profile`.
#[derive(Debug, Serialize)]
pub struct UserProfile {
    /// Stable internal id.
    pub user_id: String,
    /// Display handle.
    pub handle: String,
    /// The address the user signed in with.
    pub address: String,
    /// The on-chain account holding their money.
    pub trading_address: String,
    /// When the account was created.
    pub joined_at: String,
    /// When it was last active.
    pub last_seen_at: String,
    /// Whether a simulation bot drives it.
    pub is_bot: bool,
    /// Roles held.
    pub roles: Vec<String>,
    /// Lifetime statistics.
    pub stats: UserStats,
}

/// `PATCH /user/profile` request.
#[derive(Debug, Deserialize)]
pub struct UpdateProfileRequest {
    /// The new handle. The only mutable field.
    #[serde(default)]
    pub handle: Option<String>,
}

/// One open session.
#[derive(Debug, Serialize)]
pub struct SessionSummary {
    /// Short, non-secret identifier.
    pub session_id: String,
    /// When it was issued.
    pub issued_at: String,
    /// When it stops being accepted.
    pub expires_at: String,
    /// When it was last used.
    pub last_used_at: String,
    /// The browser that created it.
    pub user_agent: Option<String>,
    /// A truncated client address, not an exact one.
    pub ip_prefix: Option<String>,
    /// Whether this is the session making the request.
    pub current: bool,
}

/// `GET /user/sessions`.
#[derive(Debug, Serialize)]
pub struct SessionsResponse {
    /// Open sessions, newest first.
    pub sessions: Vec<SessionSummary>,
}

/// `POST /user/sessions/revoke-all`.
#[derive(Debug, Serialize)]
pub struct RevokedResponse {
    /// How many sessions were revoked.
    pub revoked: u64,
}

/// One entry in the activity feed.
#[derive(Debug, Serialize)]
pub struct ActivityItem {
    /// Stable id.
    pub id: String,
    /// What kind of thing happened.
    pub kind: String,
    /// When.
    pub at: String,
    /// A one-line description a person can read.
    pub summary: String,
    /// The market, when there was one.
    pub symbol: Option<String>,
    /// Signed value moved.
    pub amount: String,
    /// Asset the value is in.
    pub asset: String,
    /// The transaction, when one caused it.
    pub tx_hash: Option<String>,
    /// Block height, when known.
    pub height: Option<u64>,
}

/// `GET /user/activity`.
#[derive(Debug, Serialize)]
pub struct ActivityResponse {
    /// Entries, newest first.
    pub entries: Vec<ActivityItem>,
    /// Cursor for the next page, or null at the end.
    pub next_cursor: Option<String>,
}

// ---------------------------------------------------------------------------
// Faucet
// ---------------------------------------------------------------------------

/// One asset and amount.
#[derive(Debug, Serialize)]
pub struct AssetAmount {
    /// Asset symbol.
    pub asset: String,
    /// Decimal amount.
    pub amount: String,
}

/// `GET /faucet`.
#[derive(Debug, Serialize)]
pub struct FaucetStatusResponse {
    /// Whether a claim would succeed now.
    pub available: bool,
    /// When the next claim becomes possible.
    pub next_available_at: Option<String>,
    /// Seconds between claims.
    pub cooldown_seconds: u64,
    /// What a claim grants.
    pub grant: Vec<AssetAmount>,
    /// How many claims this user has made.
    pub claims_made: u64,
}

/// `POST /faucet/claim`.
#[derive(Debug, Serialize)]
pub struct FaucetClaimResponse {
    /// Stable claim id.
    pub claim_id: String,
    /// Transaction that credited the account.
    pub tx_hash: String,
    /// What was granted.
    pub granted: Vec<AssetAmount>,
    /// When the next claim becomes possible.
    pub next_available_at: String,
}

// ---------------------------------------------------------------------------
// Market data
// ---------------------------------------------------------------------------

/// One listed market.
#[derive(Debug, Serialize)]
pub struct Market {
    /// Contract symbol.
    pub symbol: String,
    /// `spot`, `perp` or `future`.
    pub kind: String,
    /// `crypto` or `equity`.
    pub asset_class: String,
    /// Base asset.
    pub base: String,
    /// Quote asset.
    pub quote: String,
    /// Human-facing name.
    pub display_name: String,
    /// `trading`, `halted`, `expired` or `settling`.
    pub status: String,
    /// Minimum price increment.
    pub tick_size: String,
    /// Minimum quantity increment.
    pub lot_size: String,
    /// Smallest permitted order notional.
    pub min_notional: String,
    /// Decimal places to display for price.
    pub price_decimals: u32,
    /// Decimal places to display for size.
    pub size_decimals: u32,
    /// Maximum leverage. Null for spot.
    pub max_leverage: Option<String>,
    /// Initial margin, basis points. Null for spot.
    pub initial_margin_bps: Option<u32>,
    /// Maintenance margin, basis points. Null for spot.
    pub maintenance_margin_bps: Option<u32>,
    /// Seconds between funding payments. Null for spot and futures.
    pub funding_interval_seconds: Option<u64>,
    /// Expiry. Non-null only for futures.
    pub expiry: Option<String>,
    /// Maker fee, basis points; negative is a rebate.
    pub maker_fee_bps: i32,
    /// Taker fee, basis points.
    pub taker_fee_bps: i32,
}

/// `GET /markets`.
#[derive(Debug, Serialize)]
pub struct MarketsResponse {
    /// Every listed market.
    pub markets: Vec<Market>,
}

/// `GET /markets/{symbol}/ticker`.
#[derive(Debug, Serialize)]
pub struct Ticker {
    /// Contract symbol.
    pub symbol: String,
    /// Last traded price.
    pub last_price: String,
    /// Engine mark price.
    pub mark_price: String,
    /// Oracle index price.
    pub index_price: String,
    /// Open interest.
    pub open_interest: String,
    /// Quote volume over 24 hours.
    pub volume_24h: String,
    /// Percentage change over 24 hours.
    pub change_24h_pct: String,
    /// Highest trade over 24 hours.
    pub high_24h: String,
    /// Lowest trade over 24 hours.
    pub low_24h: String,
    /// Latest funding rate.
    pub funding_rate: String,
    /// When funding next accrues.
    pub next_funding_at: Option<String>,
}

/// `GET /markets/{symbol}/orderbook`.
#[derive(Debug, Serialize)]
pub struct OrderBook {
    /// Contract symbol.
    pub symbol: String,
    /// Height the snapshot was taken at.
    pub height: u64,
    /// `[price, quantity]`, highest price first.
    pub bids: Vec<[String; 2]>,
    /// `[price, quantity]`, lowest price first.
    pub asks: Vec<[String; 2]>,
}

/// One printed trade.
#[derive(Debug, Serialize)]
pub struct Trade {
    /// Stable id.
    pub id: String,
    /// Trade price.
    pub price: String,
    /// Trade quantity.
    pub qty: String,
    /// The aggressor's side.
    pub side: String,
    /// When it printed.
    pub at: String,
    /// Block height.
    pub height: u64,
}

/// `GET /markets/{symbol}/trades`.
#[derive(Debug, Serialize)]
pub struct TradesResponse {
    /// Recent trades, newest first.
    pub trades: Vec<Trade>,
}

/// One OHLCV bar.
#[derive(Debug, Serialize)]
pub struct Candle {
    /// Bucket start.
    pub t: String,
    /// Open.
    pub o: String,
    /// High.
    pub h: String,
    /// Low.
    pub l: String,
    /// Close.
    pub c: String,
    /// Base-asset volume.
    pub v: String,
}

/// `GET /markets/{symbol}/candles`.
#[derive(Debug, Serialize)]
pub struct CandlesResponse {
    /// Contract symbol.
    pub symbol: String,
    /// Bucket width.
    pub interval: String,
    /// Bars, oldest first.
    pub candles: Vec<Candle>,
}

/// One funding observation.
#[derive(Debug, Serialize)]
pub struct FundingPoint {
    /// When it accrued.
    pub at: String,
    /// The rate applied.
    pub rate: String,
    /// Mark price at the tick.
    pub mark_price: String,
    /// Index price at the tick.
    pub index_price: String,
}

/// `GET /markets/{symbol}/funding`.
#[derive(Debug, Serialize)]
pub struct FundingResponse {
    /// Observations, newest first.
    pub history: Vec<FundingPoint>,
}

// ---------------------------------------------------------------------------
// Account
// ---------------------------------------------------------------------------

/// One asset balance.
#[derive(Debug, Serialize)]
pub struct Balance {
    /// Asset symbol.
    pub asset: String,
    /// Free balance.
    pub free: String,
    /// Locked balance.
    pub locked: String,
    /// Free plus locked.
    pub total: String,
}

/// Portfolio summary.
#[derive(Debug, Serialize)]
pub struct Portfolio {
    /// Total account value.
    pub equity: String,
    /// Margin available for new positions.
    pub available_margin: String,
    /// Margin already committed.
    pub used_margin: String,
    /// Unrealised profit and loss.
    pub unrealized_pnl: String,
    /// Realised profit and loss over 24 hours.
    pub realized_pnl_24h: String,
    /// Used margin over equity.
    pub margin_ratio: String,
    /// `low`, `medium`, `high` or `critical`.
    pub liquidation_risk: String,
}

/// `GET /account`.
///
/// Both addresses are published, and neither is called `address`. The contract
/// (§2, "Two addresses, and why") makes the distinction part of the API on
/// purpose: the login address is the one the user controls, the trading address
/// is the one holding their money, and a single field named `address` cannot
/// say which of the two it is. An earlier revision emitted exactly that, filled
/// with the trading address, and the browser client — which requires both —
/// could not decode the reply at all.
#[derive(Debug, Serialize)]
pub struct AccountResponse {
    /// The EVM address the user signed in with. They hold its key.
    pub login_address: String,
    /// The on-chain account holding the balances below. The gateway holds its
    /// key, derived from the server secret.
    pub trading_address: String,
    /// Next transaction nonce.
    pub nonce: u64,
    /// Balances per asset.
    pub balances: Vec<Balance>,
    /// Portfolio summary.
    pub portfolio: Portfolio,
}

/// One order.
#[derive(Debug, Serialize)]
pub struct Order {
    /// On-chain order id, or the transaction hash while pending.
    pub order_id: String,
    /// The caller's idempotency key.
    pub client_order_id: Option<String>,
    /// Market symbol.
    pub symbol: String,
    /// `buy` or `sell`.
    pub side: String,
    /// `limit` or `market`.
    #[serde(rename = "type")]
    pub order_type: String,
    /// Limit price.
    pub price: String,
    /// Original quantity.
    pub qty: String,
    /// Quantity filled.
    pub filled_qty: String,
    /// Quantity still working.
    pub remaining_qty: String,
    /// Volume-weighted fill price.
    pub avg_fill_price: Option<String>,
    /// Lifecycle state.
    pub status: String,
    /// Time in force.
    pub tif: String,
    /// Post-only flag.
    pub post_only: bool,
    /// Reduce-only flag.
    pub reduce_only: bool,
    /// When the gateway accepted it.
    pub created_at: String,
    /// When it last changed.
    pub updated_at: String,
}

/// `GET /account/orders`.
#[derive(Debug, Serialize)]
pub struct OrdersResponse {
    /// Matching orders, newest first.
    pub orders: Vec<Order>,
}

/// One open position.
#[derive(Debug, Serialize)]
pub struct Position {
    /// Market symbol.
    pub symbol: String,
    /// `long` or `short`.
    pub side: String,
    /// Size.
    pub qty: String,
    /// Volume-weighted entry price.
    pub entry_price: String,
    /// Current mark price.
    pub mark_price: String,
    /// Estimated liquidation price.
    pub liquidation_price: String,
    /// Position notional.
    pub notional: String,
    /// `cross` or `isolated`.
    pub margin_mode: String,
    /// Effective leverage.
    pub leverage: String,
    /// Margin backing the position.
    pub margin: String,
    /// Unrealised profit and loss.
    pub unrealized_pnl: String,
    /// Unrealised profit and loss as a percentage of margin.
    pub unrealized_pnl_pct: String,
    /// Funding paid so far; negative means received.
    pub funding_paid: String,
    /// When the position was opened.
    pub opened_at: Option<String>,
}

/// `GET /account/positions`.
#[derive(Debug, Serialize)]
pub struct PositionsResponse {
    /// Open positions.
    pub positions: Vec<Position>,
}

/// One fill.
#[derive(Debug, Serialize)]
pub struct Fill {
    /// Stable id.
    pub fill_id: String,
    /// The order it belongs to.
    pub order_id: String,
    /// Market symbol.
    pub symbol: String,
    /// `buy` or `sell`.
    pub side: String,
    /// Fill price.
    pub price: String,
    /// Fill quantity.
    pub qty: String,
    /// Fee paid.
    pub fee: String,
    /// Asset the fee was paid in.
    pub fee_asset: String,
    /// `maker` or `taker`.
    pub role: String,
    /// Realised profit and loss.
    pub realized_pnl: String,
    /// When it happened.
    pub at: String,
    /// Block height.
    pub height: u64,
}

/// `GET /account/fills`.
#[derive(Debug, Serialize)]
pub struct FillsResponse {
    /// Matching fills, newest first.
    pub fills: Vec<Fill>,
}

/// One ledger movement.
#[derive(Debug, Serialize)]
pub struct LedgerEntry {
    /// Stable id.
    pub id: String,
    /// What kind of movement.
    pub kind: String,
    /// Asset moved.
    pub asset: String,
    /// Signed amount.
    pub amount: String,
    /// Free balance afterwards, as the gateway observed it.
    pub balance_after: String,
    /// Free-text note.
    pub memo: Option<String>,
    /// When recorded.
    pub at: String,
    /// Transaction that caused it.
    pub tx_hash: Option<String>,
}

/// `GET /account/transfers`.
#[derive(Debug, Serialize)]
pub struct TransfersResponse {
    /// Movements, newest first.
    pub entries: Vec<LedgerEntry>,
}

// ---------------------------------------------------------------------------
// Trading
// ---------------------------------------------------------------------------

/// `POST /orders` request.
#[derive(Debug, Deserialize)]
pub struct PlaceOrderRequest {
    /// Market symbol.
    pub symbol: String,
    /// `buy` or `sell`.
    pub side: String,
    /// `limit` or `market`.
    #[serde(rename = "type")]
    pub order_type: String,
    /// Limit price; omitted for market orders.
    #[serde(default)]
    pub price: Option<String>,
    /// Quantity.
    pub qty: String,
    /// Time in force.
    #[serde(default)]
    pub tif: Option<String>,
    /// Post-only flag.
    #[serde(default)]
    pub post_only: bool,
    /// Reduce-only flag.
    #[serde(default)]
    pub reduce_only: bool,
    /// Requested leverage; derivative markets only.
    #[serde(default)]
    pub leverage: Option<String>,
    /// `cross` or `isolated`; derivative markets only.
    #[serde(default)]
    pub margin_mode: Option<String>,
    /// Idempotency key.
    #[serde(default)]
    pub client_order_id: Option<String>,
}

/// One fill that happened as the order was placed.
#[derive(Debug, Serialize)]
pub struct ImmediateFill {
    /// Fill price.
    pub price: String,
    /// Fill quantity.
    pub qty: String,
    /// Fee paid.
    pub fee: String,
}

/// `POST /orders`.
#[derive(Debug, Serialize)]
pub struct PlaceOrderResponse {
    /// On-chain order id, or the transaction hash while pending.
    pub order_id: String,
    /// The caller's idempotency key.
    pub client_order_id: Option<String>,
    /// Lifecycle state.
    pub status: String,
    /// The transaction that carried it.
    pub tx_hash: String,
    /// When the gateway accepted it.
    pub accepted_at: String,
    /// Fills observed before the reply.
    pub immediate_fills: Vec<ImmediateFill>,
}

/// `DELETE /orders/{order_id}`.
#[derive(Debug, Serialize)]
pub struct CancelOrderResponse {
    /// The order cancelled.
    pub order_id: String,
    /// Lifecycle state.
    pub status: String,
    /// The transaction that carried it.
    pub tx_hash: String,
}

/// `POST /orders/cancel-all` request.
#[derive(Debug, Default, Deserialize)]
pub struct CancelAllRequest {
    /// Restrict to one market. Omit to cancel everywhere.
    #[serde(default)]
    pub symbol: Option<String>,
}

/// `POST /orders/cancel-all`.
#[derive(Debug, Serialize)]
pub struct CancelAllResponse {
    /// How many orders were cancelled.
    pub cancelled: u32,
    /// The last transaction submitted.
    pub tx_hash: String,
}

/// `POST /positions/close` request.
#[derive(Debug, Deserialize)]
pub struct ClosePositionRequest {
    /// Market symbol.
    pub symbol: String,
    /// How much to close. Omit to close fully.
    #[serde(default)]
    pub qty: Option<String>,
}

/// `POST /positions/close`.
#[derive(Debug, Serialize)]
pub struct ClosePositionResponse {
    /// The transaction that carried it.
    pub tx_hash: String,
    /// How much was closed.
    pub closed_qty: String,
    /// Price the close was submitted against.
    pub avg_price: String,
    /// Realised profit and loss.
    pub realized_pnl: String,
}

/// `POST /positions/margin` request.
#[derive(Debug, Deserialize)]
pub struct AdjustMarginRequest {
    /// Market symbol.
    pub symbol: String,
    /// `add` or `remove`.
    pub action: String,
    /// How much.
    pub amount: String,
}

/// `POST /positions/leverage` request.
#[derive(Debug, Deserialize)]
pub struct SetLeverageRequest {
    /// Market symbol.
    pub symbol: String,
    /// Requested leverage.
    pub leverage: String,
    /// `cross` or `isolated`.
    #[serde(default)]
    pub margin_mode: Option<String>,
}

/// A reply that carries only a transaction.
#[derive(Debug, Serialize)]
pub struct TxResponse {
    /// The transaction submitted.
    pub tx_hash: String,
    /// When it was submitted.
    pub at: String,
}

/// `POST /transfers` request.
#[derive(Debug, Deserialize)]
pub struct TransferRequest {
    /// Recipient address.
    pub to: String,
    /// Asset to move.
    pub asset: String,
    /// How much.
    pub amount: String,
    /// Free-text note, recorded off chain.
    #[serde(default)]
    pub memo: Option<String>,
}

// ---------------------------------------------------------------------------
// Chain and leaderboard
// ---------------------------------------------------------------------------

/// `GET /chain/status`.
#[derive(Debug, Serialize)]
pub struct ChainStatus {
    /// Replay-protection domain.
    pub chain_id: u32,
    /// Head height.
    pub height: u64,
    /// State root at the head.
    pub state_root: String,
    /// Assumed block interval.
    pub block_time_ms: u64,
    /// Transactions per second over the last minute.
    pub tps_1m: String,
    /// Transactions seen since the gateway started.
    pub tx_count: u64,
    /// Transactions waiting in the mempool.
    pub mempool_depth: u64,
    /// How many validators are in the set.
    pub validators: u32,
    /// Whether the gateway is following the chain.
    pub synced: bool,
    /// How long the gateway has been up.
    pub uptime_seconds: u64,
    /// The sequencer's Cap'n Proto RPC address this gateway talks to.
    pub node_rpc: String,
    /// The address this gateway serves HTTP and WebSocket on.
    pub gateway_listen: String,
    /// Path of the WebSocket feed, relative to the gateway's root.
    pub ws_path: String,
    /// The oracle's scenario-control URL, when one is configured.
    pub oracle_control: Option<String>,
    /// The simulation switch position, when a harness is attached.
    pub sim_mode: String,
}

/// One block header.
#[derive(Debug, Serialize)]
pub struct BlockSummary {
    /// Height.
    pub height: u64,
    /// Block hash.
    pub hash: String,
    /// State root after the block.
    pub state_root: String,
    /// How many transactions it carried.
    pub tx_count: u32,
    /// When it was produced.
    pub at: String,
    /// Who proposed it.
    pub proposer: Option<String>,
}

/// `GET /chain/blocks`.
#[derive(Debug, Serialize)]
pub struct BlocksResponse {
    /// Blocks, newest first.
    pub blocks: Vec<BlockSummary>,
}

/// One transaction the chain applied.
#[derive(Debug, Serialize)]
pub struct TxSummary {
    /// Transaction hash.
    pub hash: String,
    /// The block that carried it.
    pub height: u64,
    /// Position in that block.
    pub index: u32,
    /// Who signed it.
    pub sender: String,
    /// The chain's name for what it did.
    pub kind: String,
    /// The market it touched, when it touched one.
    pub market: Option<String>,
    /// `buy` or `sell`, when it has a side.
    pub side: Option<String>,
    /// When its block was produced.
    pub at: String,
}

/// `GET /chain/txs`.
#[derive(Debug, Serialize)]
pub struct TxsResponse {
    /// Transactions, newest first.
    pub txs: Vec<TxSummary>,
}

/// One leaderboard row.
#[derive(Debug, Serialize)]
pub struct LeaderboardRow {
    /// Position, 1-based.
    pub rank: u32,
    /// Player handle.
    pub handle: String,
    /// Player address.
    pub address: String,
    /// Account equity.
    pub equity: String,
    /// Profit and loss over the window.
    pub pnl: String,
    /// Profit and loss as a percentage.
    pub pnl_pct: String,
    /// Notional traded over the window.
    pub volume: String,
    /// Trades over the window.
    pub trades: u64,
    /// Whether a bot holds this position.
    pub is_bot: bool,
}

/// `GET /leaderboard`.
#[derive(Debug, Serialize)]
pub struct LeaderboardResponse {
    /// The window reported.
    pub window: String,
    /// Rows, best first.
    pub entries: Vec<LeaderboardRow>,
}

// ---------------------------------------------------------------------------
// Simulation and health
// ---------------------------------------------------------------------------

/// `GET`/`POST /admin/sim`.
#[derive(Debug, Serialize)]
pub struct SimStatusResponse {
    /// Whether the bot population is running.
    pub running: bool,
    /// The operator's switch: `off`, `on` or `max`.
    pub mode: String,
    /// The active scenario.
    pub scenario: String,
    /// The seed the run was started with.
    pub seed: u64,
    /// How many of each kind of bot are trading.
    pub bots: std::collections::BTreeMap<String, u32>,
    /// How long it has been running.
    pub elapsed_seconds: u64,
    /// Orders the bots have submitted.
    pub orders_submitted: u64,
    /// Invariant violations observed.
    pub invariant_violations: u64,
}

/// `POST /admin/sim` request.
#[derive(Debug, Deserialize)]
pub struct SimCommandRequest {
    /// `start`, `stop`, `scenario` or `mode`.
    pub action: String,
    /// `off`, `on` or `max`. Required with the `mode` action.
    #[serde(default)]
    pub mode: Option<String>,
    /// The scenario to run.
    #[serde(default)]
    pub scenario: Option<String>,
    /// The seed to run with.
    #[serde(default)]
    pub seed: Option<u64>,
    /// How many of each kind of bot to run.
    #[serde(default)]
    pub bots: std::collections::BTreeMap<String, u32>,
}

/// One dependency's health.
#[derive(Debug, Serialize)]
pub struct DependencyHealth {
    /// What it is.
    pub name: String,
    /// `up` or `down`.
    pub status: String,
    /// Why, when it is down.
    pub detail: Option<String>,
}

/// `GET /health`.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    /// `ok` when everything the gateway needs is reachable.
    pub status: String,
    /// How long the gateway has been up.
    pub uptime_seconds: u64,
    /// Each dependency.
    pub dependencies: Vec<DependencyHealth>,
    /// Head height the gateway has seen.
    pub height: u64,
    /// Open WebSocket connections.
    pub websocket_connections: usize,
    /// Markets the catalogue holds.
    pub markets: usize,
}

// ---------------------------------------------------------------------------
// Admin
// ---------------------------------------------------------------------------

/// An asset and amount a caller supplied.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AssetAmountRequest {
    /// Asset symbol.
    pub asset: String,
    /// Decimal amount.
    pub amount: String,
}

/// Chain facts on the operator dashboard.
#[derive(Debug, Serialize)]
pub struct ChainSummary {
    /// Head height.
    pub height: u64,
    /// Assumed block interval.
    pub block_time_ms: u64,
    /// Transactions per second over the last minute.
    pub tps_1m: String,
    /// Transactions waiting in the mempool.
    pub mempool_depth: u64,
    /// Validators in the set.
    pub validators: u32,
    /// Whether the gateway is following the chain.
    pub synced: bool,
    /// State root at the head.
    pub state_root: String,
    /// How long the gateway has been up.
    pub uptime_seconds: u64,
}

/// Account counts.
#[derive(Debug, Serialize)]
pub struct UserCounts {
    /// Every account.
    pub total: u64,
    /// Accounts a person drives.
    pub humans: u64,
    /// Accounts a bot drives.
    pub bots: u64,
    /// Accounts seen in the last 24 hours.
    pub active_24h: u64,
    /// Accounts created in the last 24 hours.
    pub new_24h: u64,
}

/// Market counts.
#[derive(Debug, Serialize)]
pub struct MarketCounts {
    /// Every listed market.
    pub total: u64,
    /// Markets accepting orders.
    pub trading: u64,
    /// Markets suspended.
    pub halted: u64,
    /// Markets that have settled.
    pub expired: u64,
}

/// Traded volume.
#[derive(Debug, Serialize)]
pub struct VolumeSummary {
    /// Notional traded in the last 24 hours.
    pub volume_24h: String,
    /// Trades in the last 24 hours.
    pub trades_24h: u64,
    /// Fees charged in the last 24 hours.
    pub fees_24h: String,
}

/// Faucet activity.
#[derive(Debug, Serialize)]
pub struct FaucetSummary {
    /// Claims in the last 24 hours.
    pub claims_24h: u64,
    /// What was granted in the last 24 hours.
    pub granted_24h: Vec<AssetAmount>,
    /// What has been granted since genesis.
    pub total_granted: Vec<AssetAmount>,
}

/// Risk across the exchange.
#[derive(Debug, Serialize)]
pub struct RiskSummary {
    /// Open interest across every derivative market.
    pub open_interest: String,
    /// What the insurance fund holds.
    pub insurance_fund: String,
    /// Accounts currently below maintenance margin.
    pub accounts_below_maintenance: u64,
    /// Liquidations in the last 24 hours.
    pub liquidations_24h: u64,
}

/// One component's health.
#[derive(Debug, Serialize)]
pub struct ComponentHealth {
    /// What it is.
    pub component: String,
    /// `ok`, `degraded` or `down`.
    pub status: String,
    /// Detail, when there is any.
    pub detail: Option<String>,
}

/// `GET /admin/overview`.
#[derive(Debug, Serialize)]
pub struct AdminOverview {
    /// Chain facts.
    pub chain: ChainSummary,
    /// Account counts.
    pub users: UserCounts,
    /// Market counts.
    pub markets: MarketCounts,
    /// Traded volume.
    pub volume: VolumeSummary,
    /// Faucet activity.
    pub faucet: FaucetSummary,
    /// Risk across the exchange.
    pub risk: RiskSummary,
    /// Component health.
    pub health: Vec<ComponentHealth>,
}

/// One row of the operator's user list.
#[derive(Debug, Serialize)]
pub struct AdminUserRow {
    /// Stable internal id.
    pub user_id: String,
    /// Display handle.
    pub handle: String,
    /// Login address.
    pub address: String,
    /// Whether a bot drives it.
    pub is_bot: bool,
    /// When the account was created.
    pub joined_at: String,
    /// When it was last active.
    pub last_seen_at: String,
    /// Account equity.
    pub equity: String,
    /// How many positions are open.
    pub open_positions: u64,
    /// Trades in the last 24 hours.
    pub trades_24h: u64,
    /// Faucet claims made.
    pub faucet_claims: u64,
    /// `active` or `suspended`.
    pub status: String,
}

/// `GET /admin/users`.
#[derive(Debug, Serialize)]
pub struct AdminUsersResponse {
    /// Matching accounts.
    pub users: Vec<AdminUserRow>,
    /// Cursor for the next page, or null at the end.
    pub next_cursor: Option<String>,
}

/// `GET /admin/users/{user_id}`.
#[derive(Debug, Serialize)]
pub struct AdminUserDetail {
    /// Stable internal id.
    pub user_id: String,
    /// Display handle.
    pub handle: String,
    /// Login address.
    pub address: String,
    /// On-chain trading account.
    pub trading_address: String,
    /// Whether a bot drives it.
    pub is_bot: bool,
    /// When the account was created.
    pub joined_at: String,
    /// When it was last active.
    pub last_seen_at: String,
    /// `active` or `suspended`.
    pub status: String,
    /// Why, when suspended.
    pub suspension_reason: Option<String>,
    /// Next transaction nonce.
    pub nonce: u64,
    /// Balances per asset.
    pub balances: Vec<Balance>,
    /// Open positions.
    pub positions: Vec<serde_json::Value>,
    /// Recent ledger movements.
    pub activity: Vec<serde_json::Value>,
    /// Audit records naming this account.
    pub audit: Vec<AuditRow>,
}

/// `POST /admin/users/{user_id}/suspend` request.
#[derive(Debug, Deserialize)]
pub struct SuspendRequest {
    /// Why, in the operator's words.
    pub reason: String,
}

/// One row of the operator's market list.
#[derive(Debug, Serialize)]
pub struct AdminMarketRow {
    /// Contract symbol.
    pub symbol: String,
    /// `spot`, `perp` or `future`.
    pub kind: String,
    /// Published status.
    pub status: String,
    /// Last traded price.
    pub last_price: String,
    /// Engine mark price.
    pub mark_price: String,
    /// Open interest.
    pub open_interest: String,
    /// How many blocks since the mark was computed.
    pub oracle_age_blocks: Option<u64>,
    /// Whether the mark is fresh enough to trade against.
    pub oracle_fresh: bool,
    /// How many price levels are resting.
    pub resting_levels: u64,
    /// Whether an operator override is in force.
    pub overridden: bool,
    /// How far an override reaches, when one is in force.
    ///
    /// Always `gateway_local`: the chain has no halt transaction and no risk
    /// transaction, so an override changes what *this gateway* accepts and
    /// publishes and binds nothing else. Absent when no override is in force.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub override_scope: Option<String>,
}

/// `GET /admin/markets`.
#[derive(Debug, Serialize)]
pub struct AdminMarketsResponse {
    /// Every market with its live operational state.
    pub markets: Vec<AdminMarketRow>,
}

/// `POST /admin/markets/{symbol}/halt` request.
#[derive(Debug, Deserialize)]
pub struct HaltRequest {
    /// Why, in the operator's words.
    pub reason: String,
}

/// `POST /admin/markets/{symbol}/settle` request.
#[derive(Debug, Deserialize)]
pub struct SettleRequest {
    /// The price every position closes at.
    pub settlement_price: String,
    /// Override the deviation guard.
    #[serde(default)]
    pub force: bool,
}

/// `PATCH /admin/markets/{symbol}/risk` request.
#[derive(Debug, Deserialize)]
pub struct RiskRequest {
    /// New maximum leverage.
    #[serde(default)]
    pub max_leverage: Option<String>,
    /// New initial margin, basis points.
    #[serde(default)]
    pub initial_margin_bps: Option<u32>,
    /// New maintenance margin, basis points.
    #[serde(default)]
    pub maintenance_margin_bps: Option<u32>,
}

/// One account a risk change leaves below maintenance margin.
#[derive(Debug, Serialize)]
pub struct AffectedAccount {
    /// Stable internal id.
    pub user_id: String,
    /// Display handle, so the operator knows whom to warn.
    pub handle: String,
    /// Maintenance requirement over equity before the change.
    pub margin_ratio_before: String,
    /// Maintenance requirement over equity after it.
    pub margin_ratio_after: String,
}

/// `PATCH /admin/markets/{symbol}/risk`.
#[derive(Debug, Serialize)]
pub struct RiskResponse {
    /// Whether the change took effect. `false` for a dry run.
    pub applied: bool,
    /// How many accounts the change leaves below maintenance margin.
    pub accounts_now_below_maintenance: u64,
    /// Which accounts those are.
    pub affected: Vec<AffectedAccount>,
    /// How far the change reaches. Always `gateway_local`.
    pub scope: String,
    /// What actually enforces the numbers, in one sentence an operator can act
    /// on. The chain gates margin on its own table and reads neither of these,
    /// so `affected` is "who this would break if the chain enforced it".
    pub enforcement: String,
}

/// `GET /admin/faucet`.
#[derive(Debug, Serialize)]
pub struct AdminFaucetResponse {
    /// Whether the faucet is open.
    pub enabled: bool,
    /// Seconds between claims.
    pub cooldown_seconds: u64,
    /// What one claim grants.
    pub grant: Vec<AssetAmount>,
    /// How many claims one account may ever make.
    pub max_claims_per_user: Option<u64>,
    /// Claims in the last 24 hours.
    pub claims_24h: u64,
    /// What has been granted since genesis.
    pub granted_total: Vec<AssetAmount>,
    /// What the faucet account has left.
    pub authority_balance: Vec<AssetAmount>,
}

/// `PATCH /admin/faucet` request.
#[derive(Debug, Deserialize, Serialize)]
pub struct AdminFaucetPatch {
    /// Open or close the faucet.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// New cooldown.
    #[serde(default)]
    pub cooldown_seconds: Option<u64>,
    /// New grant.
    #[serde(default)]
    pub grant: Option<Vec<AssetAmountRequest>>,
    /// New per-account claim limit. An explicit null removes the limit.
    #[serde(default, with = "serde_with::rust::double_option")]
    pub max_claims_per_user: Option<Option<u64>>,
}

/// `POST /admin/faucet/grant` request.
#[derive(Debug, Deserialize)]
pub struct AdminGrantRequest {
    /// Who to grant to.
    pub user_id: String,
    /// What to grant.
    pub grants: Vec<AssetAmountRequest>,
    /// Why, in the operator's words.
    pub reason: String,
}

/// `POST /admin/faucet/grant`.
#[derive(Debug, Serialize)]
pub struct AdminGrantResponse {
    /// Stable claim id.
    pub claim_id: String,
    /// The transaction that credited the account.
    pub tx_hash: String,
}

/// One audit record.
#[derive(Debug, Serialize)]
pub struct AuditRow {
    /// Stable id.
    pub record_id: String,
    /// The acting operator's login address.
    pub actor: Option<String>,
    /// What happened.
    pub action: String,
    /// What it happened to.
    pub subject: String,
    /// Structured context.
    pub detail: serde_json::Value,
    /// When.
    pub at: String,
}

/// `GET /admin/audit`.
#[derive(Debug, Serialize)]
pub struct AuditResponse {
    /// Matching records, newest first.
    pub records: Vec<AuditRow>,
    /// Cursor for the next page, or null at the end.
    pub next_cursor: Option<String>,
}
