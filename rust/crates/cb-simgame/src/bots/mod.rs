//! The bot population.
//!
//! Each strategy is a separate type with its own parameters, because the
//! point of the population is that the strategies *disagree*: a market maker
//! that quotes both sides needs someone taking liquidity, a momentum trader
//! needs a trend to chase and a counterparty deep enough to lose to, and a
//! liquidator needs someone to be wrong. A single "random trader" would keep
//! the book alive without ever producing the state a conservation bug hides
//! in.
//!
//! ## The contract
//!
//! A bot returns transaction *bodies*, never signed transactions. The world
//! owns signing and nonce assignment, for a reason that matters: a rejected
//! transaction does not advance the sender's nonce, so every signature must
//! be produced after the previous transaction's outcome is known. If a bot
//! signed its own batch, the [`noise`] bot's deliberately invalid orders
//! would desynchronise every later transaction from the same sender and the
//! run would degenerate into nothing but `BadNonce`.
//!
//! ## Determinism
//!
//! Bots are iterated in a fixed index order and each draws from a generator
//! the world seeds per `(bot index, height)`. No bot may iterate a `HashMap`,
//! consult the clock, or hold state that the world does not reproduce from
//! the seed.

pub mod arbitrageur;
pub mod liquidator;
pub mod market_maker;
pub mod momentum;
pub mod noise;
pub mod whale;

use std::collections::BTreeMap;

use cb_tx::TxBody;
use cb_types::{Address, DerivId, MarketId, Side, H256};
use rand_chacha::ChaCha8Rng;

pub use arbitrageur::Arbitrageur;
pub use liquidator::Liquidator;
pub use market_maker::MarketMaker;
pub use momentum::Momentum;
pub use noise::Noise;
pub use whale::Whale;

/// Which strategy a bot runs. Used for reporting and for the per-strategy
/// population counts on the command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum BotKind {
    /// Quotes both sides, manages inventory.
    MarketMaker,
    /// Chases short-term trends with leverage.
    Momentum,
    /// Random small orders, including deliberately invalid ones.
    Noise,
    /// Occasional large market orders.
    Whale,
    /// Triggers liquidations against underwater accounts.
    Liquidator,
    /// Trades the gap between the book and the index.
    Arbitrageur,
}

impl BotKind {
    /// Every strategy, in a fixed order.
    pub const ALL: [BotKind; 6] = [
        BotKind::MarketMaker,
        BotKind::Momentum,
        BotKind::Noise,
        BotKind::Whale,
        BotKind::Liquidator,
        BotKind::Arbitrageur,
    ];

    /// Short lower-case name.
    pub fn name(self) -> &'static str {
        match self {
            BotKind::MarketMaker => "market_maker",
            BotKind::Momentum => "momentum",
            BotKind::Noise => "noise",
            BotKind::Whale => "whale",
            BotKind::Liquidator => "liquidator",
            BotKind::Arbitrageur => "arbitrageur",
        }
    }
}

/// What a bot can see about one derivative market.
#[derive(Clone, Debug)]
pub struct MarketObs {
    /// On-chain market id.
    pub id: DerivId,
    /// Human name.
    pub label: &'static str,
    /// Chain's mark price, q8.
    pub mark_q8: u128,
    /// Chain's oracle (index) price, q8.
    pub oracle_q8: u128,
    /// Best resting bid, q8.
    pub best_bid: Option<u128>,
    /// Best resting ask, q8.
    pub best_ask: Option<u128>,
    /// Price tick.
    pub tick: u128,
    /// Market is in reduce-only halt or fully paused.
    pub restricted: bool,
    /// Dated future that has already settled.
    pub settled: bool,
}

impl MarketObs {
    /// Book midpoint when both sides exist.
    pub fn mid_q8(&self) -> Option<u128> {
        match (self.best_bid, self.best_ask) {
            (Some(b), Some(a)) => Some((b + a) / 2),
            _ => None,
        }
    }

    /// A price to quote around: the book midpoint if there is one, otherwise
    /// the mark.
    pub fn reference_q8(&self) -> u128 {
        self.mid_q8().unwrap_or(self.mark_q8).max(1)
    }
}

/// What a bot can see about the spot book.
#[derive(Clone, Debug)]
pub struct SpotObs {
    /// Whether the run lists a spot market at all. The long-running
    /// derivative invariant tests switch spot off — see
    /// [`crate::world::WorldConfig::enable_spot`] for why.
    pub enabled: bool,
    /// Market id.
    pub market: MarketId,
    /// Best bid in spot price units (see [`crate::world::spot_price_from_index`]).
    pub best_bid: Option<u128>,
    /// Best ask.
    pub best_ask: Option<u128>,
    /// Reference price derived from the index, in spot price units.
    pub reference: u128,
    /// Minimum quantity increment.
    pub min_qty: u128,
    /// Price tick.
    pub tick: u128,
}

/// What a bot can see about itself.
#[derive(Clone, Debug, Default)]
pub struct SelfObs {
    /// Margin collateral, micro-USD.
    pub collateral: u128,
    /// Unpaid losses.
    pub bad_debt: u128,
    /// Open positions by market: `(side, qty, entry_px_q8)`.
    pub positions: BTreeMap<DerivId, (Side, u128, u128)>,
    /// Own resting derivative orders: `(order_id, market, side, price, remaining)`.
    pub resting: Vec<(H256, DerivId, Side, u128, u128)>,
    /// Own resting spot orders: `(order_id, side, price, remaining)`.
    pub spot_resting: Vec<(H256, Side, u128, u128)>,
    /// Free spot base balance.
    pub spot_base_free: u128,
    /// Free spot quote balance.
    pub spot_quote_free: u128,
}

impl SelfObs {
    /// Signed position size on a market: positive long, negative short.
    pub fn signed_qty(&self, market: DerivId) -> i128 {
        match self.positions.get(&market) {
            Some((Side::Buy, q, _)) => *q as i128,
            Some((Side::Sell, q, _)) => -(*q as i128),
            None => 0,
        }
    }
}

/// Everything a bot sees at the start of a block.
#[derive(Clone, Debug)]
pub struct Observation {
    /// Height of the block being built.
    pub height: u64,
    /// Blocks since the run started.
    pub offset: u64,
    /// Derivative markets, in fixed order.
    pub markets: Vec<MarketObs>,
    /// The spot book.
    pub spot: SpotObs,
    /// The acting bot's own state.
    pub me: SelfObs,
    /// Accounts the chain would currently consider liquidatable, as
    /// `(owner, market)` pairs in address order. Computed once per block by
    /// the world so every liquidator sees the same list.
    pub liquidatable: Vec<(Address, DerivId)>,
}

impl Observation {
    /// Look up a market by id.
    pub fn market(&self, id: DerivId) -> Option<&MarketObs> {
        self.markets.iter().find(|m| m.id == id)
    }

    /// The tradeable derivative markets: not paused, not settled, and with a
    /// usable price.
    pub fn tradeable(&self) -> impl Iterator<Item = &MarketObs> {
        self.markets
            .iter()
            .filter(|m| !m.settled && !m.restricted && m.mark_q8 > 0)
    }
}

/// A trading strategy.
pub trait Bot: Send {
    /// Human label, unique within a run (e.g. `market_maker#2`).
    fn label(&self) -> &str;

    /// Which strategy this is.
    fn kind(&self) -> BotKind;

    /// Decide what to submit this block. Must be a pure function of `obs`,
    /// `rng` and the bot's own seeded state.
    fn act(&mut self, obs: &Observation, rng: &mut ChaCha8Rng) -> Vec<TxBody>;
}

/// Derive a client order id from a bot-local counter, so ids are distinct
/// within a run without needing a global allocator.
pub fn client_id(tag: u8, counter: u64) -> H256 {
    let mut b = [0u8; 32];
    b[0] = tag;
    b[1..9].copy_from_slice(&counter.to_be_bytes());
    H256(b)
}

/// Round a price down to the market's tick, never below one tick.
pub fn on_tick(price_q8: u128, tick: u128) -> u128 {
    let t = tick.max(1);
    let r = (price_q8 / t) * t;
    if r == 0 {
        t
    } else {
        r
    }
}

/// Convenience constructor for a derivative order body.
#[allow(clippy::too_many_arguments)]
pub fn place_perp(
    market: DerivId,
    side: Side,
    price_q8: u128,
    qty: u128,
    tif: cb_types::Tif,
    post_only: bool,
    reduce_only: bool,
    client_order_id: H256,
) -> TxBody {
    TxBody::PlacePerpOrder {
        market,
        side,
        price_q8,
        qty,
        tif,
        post_only,
        reduce_only,
        client_order_id,
    }
}
