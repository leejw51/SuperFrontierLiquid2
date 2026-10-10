//! Plain, `Send` data types the gateway sees instead of Cap'n Proto readers.
//!
//! Cap'n Proto readers borrow from an arena that lives on the RPC thread, so
//! nothing built from one may cross the channel back to an axum handler. Every
//! RPC response is copied into one of these owned structs on the RPC thread
//! before it is sent, which is what makes the facade's futures `Send`.

use cb_types::{Address, AssetId, DerivId, MarketId, Side, H256};
use std::collections::BTreeMap;

/// Chain identity, needed to sign anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainInfo {
    /// Replay-protection domain for signatures.
    pub chain_id: u32,
    /// Display name the chain gives its native USD asset.
    pub denom: String,
    /// The chain head when the node answered.
    pub head_height: u64,
    /// The lowest height whose block body the node still holds. Bodies below
    /// it have been pruned and can never be streamed.
    pub oldest_block: u64,
}

/// An account as the chain sees it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountSnapshot {
    /// Next expected transaction nonce.
    pub nonce: u64,
    /// Free balance per asset, in that asset's base units.
    pub balances: BTreeMap<AssetId, u128>,
    /// Locked balance per asset, in that asset's base units.
    pub locked: BTreeMap<AssetId, u128>,
}

impl AccountSnapshot {
    /// Total holding of one asset, reserved or not.
    ///
    /// **This is what the chain calls the balance**, and the distinction is the
    /// one this type used to get wrong. `cb_state::AccountState` keeps `locked`
    /// as a *subset* of the balance rather than a sibling of it: locking an
    /// order's collateral does not debit the balance, it records how much of
    /// the balance is spoken for. Reading this as the free balance — and then
    /// adding `locked` to it to make a total — reported the reserved amount
    /// twice and told the user that money they had already committed to a
    /// resting order was theirs to spend.
    pub fn balance(&self, asset: AssetId) -> u128 {
        self.balances.get(&asset).copied().unwrap_or(0)
    }

    /// Locked balance for one asset: the part of [`Self::balance`] that is
    /// reserved against resting orders.
    pub fn locked_of(&self, asset: AssetId) -> u128 {
        self.locked.get(&asset).copied().unwrap_or(0)
    }

    /// What can actually be spent: the balance less what is locked.
    ///
    /// Saturating, because a lock larger than the balance is a corrupted
    /// account rather than a negative number, and the honest reading of it is
    /// "nothing available".
    pub fn available(&self, asset: AssetId) -> u128 {
        self.balance(asset).saturating_sub(self.locked_of(asset))
    }
}

/// One aggregated price level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BookLevel {
    /// Price in the market's on-chain price scale.
    pub price: u128,
    /// Resting quantity in the market's on-chain quantity scale.
    pub qty: u128,
}

/// A depth snapshot, best price first on both sides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BookSnapshot {
    /// Bids, highest price first.
    pub bids: Vec<BookLevel>,
    /// Asks, lowest price first.
    pub asks: Vec<BookLevel>,
    /// Height the snapshot was taken at.
    pub height: u64,
}

/// An open derivative position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionSnapshot {
    /// Which contract.
    pub market: DerivId,
    /// Direction. Meaningless when `qty` is zero.
    pub side: Side,
    /// Size in contract base units.
    pub qty: u128,
    /// Volume-weighted entry price, `1e8`-scaled.
    pub entry_px_q8: u128,
    /// `0` for cross, `1` for isolated.
    pub margin_mode: u8,
    /// Margin posted against this position when isolated, in micro-USD.
    pub isolated_margin: u128,
    /// Funding index snapshot taken at the last position update.
    pub entry_funding_index: i128,
    /// Height funding was last applied at.
    pub last_funding_height: u64,
}

/// The cross-margin collateral pool.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MarginSnapshot {
    /// Collateral in micro-USD.
    pub collateral: u128,
    /// Unrealised PnL in micro-USD.
    pub unrealized_pnl: i128,
    /// Bad debt in micro-USD.
    pub bad_debt: u128,
}

/// The engine-derived mark price.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MarkSnapshot {
    /// Mark price, `1e8`-scaled.
    pub px_q8: u128,
    /// Height it was computed at.
    pub computed_at_height: u64,
    /// Premium EMA, `1e9`-scaled.
    pub premium_ema_q9: i64,
}

/// The last oracle-signed price.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OracleSnapshot {
    /// Index price, `1e8`-scaled.
    pub px_q8: u128,
    /// Height the oracle posted it at.
    pub source_height: u64,
}

/// Cumulative funding for one market.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FundingSnapshot {
    /// Cumulative funding index.
    pub cumulative: i128,
    /// Height of the last funding tick.
    pub last_tick_height: u64,
    /// Latest interval rate, `1e9`-scaled.
    pub rate_q9: i64,
}

/// A listed perpetual.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerpMarketSnapshot {
    /// Chain handle.
    pub id: DerivId,
    /// Underlying asset.
    pub underlying: AssetId,
    /// Minimum price increment, in the price scale.
    pub tick_size: u128,
    /// Minimum quantity increment, in the quantity scale.
    pub lot_size: u128,
    /// Maximum leverage times 100.
    pub max_leverage_x100: u32,
    /// Initial margin requirement, basis points.
    pub initial_margin_bps: u16,
    /// Maintenance margin requirement, basis points.
    pub maintenance_margin_bps: u16,
    /// Taker fee, basis points.
    pub taker_fee_bps: u16,
    /// Maker fee, basis points; negative is a rebate.
    pub maker_fee_bps: i16,
    /// Blocks between funding ticks.
    pub funding_interval_blocks: u32,
    /// Largest position notional the market allows, micro-USD.
    pub max_position_notional: u128,
    /// Whether trading is suspended entirely.
    pub paused: bool,
    /// Whether the market accepts reduce-only flow only.
    pub reduce_only_mode: bool,
    /// Last traded price, `1e8`-scaled.
    pub last_trade_px_q8: u128,
    /// Height of the last trade.
    pub last_trade_height: u64,
}

/// A listed spot market, as genesis configured it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotMarketSnapshot {
    /// Chain handle.
    pub id: MarketId,
    /// Base asset.
    pub base: AssetId,
    /// Quote asset.
    pub quote: AssetId,
    /// Minimum price increment, in the market's own wire price scale.
    pub tick_size: u128,
    /// Minimum quantity increment, in the base asset's wire scale.
    pub min_qty: u128,
    /// Taker fee, basis points. Spot makers pay nothing.
    pub fee_bps: u16,
}

/// A listed dated future.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FutureMarketSnapshot {
    /// Chain handle.
    pub id: DerivId,
    /// Underlying asset.
    pub underlying: AssetId,
    /// Minimum price increment, in the price scale.
    pub tick_size: u128,
    /// Minimum quantity increment, in the quantity scale.
    pub lot_size: u128,
    /// Maximum leverage times 100.
    pub max_leverage_x100: u32,
    /// Initial margin requirement, basis points.
    pub initial_margin_bps: u16,
    /// Maintenance margin requirement, basis points.
    pub maintenance_margin_bps: u16,
    /// Taker fee, basis points.
    pub taker_fee_bps: u16,
    /// Maker fee, basis points; negative is a rebate.
    pub maker_fee_bps: i16,
    /// Block height the contract expires at.
    pub expiry_height: u64,
    /// Whether trading is suspended.
    pub paused: bool,
    /// Whether the market accepts reduce-only flow only (insurance exhausted).
    pub reduce_only_mode: bool,
    /// Final settlement price once settled, `1e8`-scaled.
    pub final_settlement_px_q8: Option<u128>,
    /// Last traded price, `1e8`-scaled.
    pub last_trade_px_q8: u128,
    /// Height of the last trade.
    pub last_trade_height: u64,
}

/// One transaction as it appeared in a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockTx {
    /// Under BFT, whether the chain applied this transaction, from the
    /// block's own mask. `None` when the block carries no mask (the
    /// sequencer scheme, or an older node), in which case the watcher falls
    /// back to comparing nonces.
    pub applied: Option<bool>,
    /// Transaction hash.
    pub hash: H256,
    /// Recovered sender.
    pub sender: Address,
    /// Index within the block, which the engine folds into the order id.
    pub index: u32,
    /// What the transaction asked for.
    pub body: cb_tx::TxBody,
}

/// A transaction the sequencer refused and dropped, from `streamRejected`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedTx {
    /// Transaction hash, as the signer recorded it.
    pub hash: H256,
    /// The sender the node recovered.
    pub sender: Address,
    /// The nonce the transaction carried; the chain did not consume it.
    pub nonce: u64,
    /// The block it was dropped from; 0 when no block came of the batch.
    pub height: u64,
    /// The executor's reason, rendered by the node.
    pub reason: String,
}

/// A block, flattened to what the gateway needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockSummary {
    /// Height.
    pub height: u64,
    /// Block hash.
    pub hash: H256,
    /// State root after applying the block.
    pub state_root: H256,
    /// Header timestamp, Unix **milliseconds** — the sequencer stamps blocks
    /// with `as_millis()`, not seconds.
    pub timestamp_ms: u64,
    /// Proposer, recovered from the sequencer signature over the header
    /// preimage. `None` when the signature is absent or unrecoverable.
    pub proposer: Option<Address>,
    /// Transactions, in execution order.
    pub txs: Vec<BlockTx>,
}

impl BlockSummary {
    /// How many transactions the block carried.
    pub fn tx_count(&self) -> u32 {
        u32::try_from(self.txs.len()).unwrap_or(u32::MAX)
    }
}

/// Why the sequencer refused a transaction.
///
/// These map onto contract error codes, which is the whole reason the gateway
/// keeps them as an enum rather than a string: a rejected post-only order must
/// surface as `POST_ONLY_WOULD_CROSS`, not as a generic `500`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitRejection {
    /// Signature did not recover to the claimed sender.
    BadSignature,
    /// Nonce did not match the account's expected next value.
    BadNonce,
    /// Not enough free balance.
    InsufficientBalance,
    /// A post-only order would have taken liquidity.
    PostOnlyWouldCross,
    /// No such market.
    UnknownMarket,
    /// Zero or otherwise unusable amount.
    InvalidAmount,
    /// Sender may not send this transaction.
    NotAuthorized,
    /// The mempool is full.
    MempoolFull,
    /// The sequencer could not classify the failure.
    Internal,
}

/// What came back from `submitTx`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// Accepted into the mempool.
    Accepted,
    /// Refused, with a reason.
    Rejected {
        /// Machine-readable reason.
        code: SubmitRejection,
        /// The sequencer's message.
        message: String,
    },
}

/// Something the node told us about, fanned out to every interested consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeEvent {
    /// A block was produced.
    Block(std::sync::Arc<BlockSummary>),
    /// A market printed a trade.
    ///
    /// The node has no trade stream, so these are **derived** by
    /// [`crate::marketdata`] from per-block book and last-trade changes rather
    /// than reported by the chain. Price is exact where the chain publishes a
    /// last-trade price; quantity is an estimate. See that module.
    Trade(Trade),
    /// The sequencer refused these transactions and dropped them from its
    /// block; the sequencer scheme never records a refusal on chain. The
    /// orders behind them are marked rejected at once rather than left
    /// `pending` until a later nonce proves the point.
    Refused(Vec<RefusedTx>),
    /// The RPC connection came up. Consumers resynchronise from a snapshot.
    Connected,
    /// The RPC connection went away.
    Disconnected,
}

/// One derived trade print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trade {
    /// The market's contract symbol.
    pub symbol: String,
    /// Trade price in the market's wire price scale.
    pub price: u128,
    /// Trade quantity in the market's wire quantity scale. Estimated.
    pub qty: u128,
    /// The aggressor's side, as far as it can be told.
    pub taker_side: Side,
    /// Height the trade executed at.
    pub height: u64,
    /// When the block carrying it was produced.
    pub at: chrono::DateTime<chrono::Utc>,
}

/// What the gateway can say about its link to the sequencer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeHealth {
    /// Whether an RPC connection is currently established.
    pub connected: bool,
    /// Highest block height observed.
    pub last_height: u64,
    /// The head the node reported when the session opened. Zero until a
    /// session has opened. `last_height` catching up to it is what "synced"
    /// means.
    pub head_height: u64,
    /// The most recent connection failure, if any.
    pub last_error: Option<String>,
    /// Chain identity, once known.
    pub chain_id: Option<u32>,
}

impl NodeHealth {
    /// Whether the gateway is following the chain, as opposed to merely
    /// holding a socket to it.
    ///
    /// Synced means the stream has reached the head the node reported when
    /// the session opened. A gateway holding a socket to a node at height
    /// 1.45 million while it has seen no block is connected, not synced; a
    /// gateway on a fresh chain whose head is still genesis is both.
    pub fn is_synced(&self) -> bool {
        self.connected && self.last_height >= self.head_height
    }
}
