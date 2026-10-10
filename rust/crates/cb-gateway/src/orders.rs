//! Order validation and transaction construction.
//!
//! ## Why the gateway validates at all
//!
//! The engine validates everything that matters — it has to, since anyone can
//! submit a transaction. The gateway validates *again*, before signing,
//! for two reasons that have nothing to do with trust:
//!
//! * **Error quality.** The sequencer's rejection vocabulary is nine codes
//!   wide. The contract's is fifteen, and the frontend shows a different
//!   message for each. "Your size is below the lot size" is actionable;
//!   `invalidAmount` is not.
//! * **Nonce economy.** A rejected transaction still burns a round trip and
//!   forces the nonce cache to resynchronise. Catching an off-tick price here
//!   costs a comparison.
//!
//! Where the two disagree, the chain wins: everything here is a *superset*
//! check that fails requests the chain would also fail, never one that lets
//! through something the chain would refuse.
//!
//! ## Reduce-only and post-only
//!
//! Both are checked against live state — the position for reduce-only, the
//! book for post-only — and both are therefore racy by nature: the book can
//! move between the check and the block. The engine re-checks post-only and
//! rejects authoritatively, so the gateway's check is an early-out, not a
//! guarantee. Reduce-only has no engine-side equivalent for the *spot* path
//! at all, which is why spot orders reject the flag outright rather than
//! accepting and ignoring it.

use crate::decimal::parse_scaled;
use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::node::{BookSnapshot, PositionSnapshot};
use crate::symbols::{InstrumentKind, MarketEntry, MarketRef};
use cb_ports::MarketMeta;
use cb_tx::TxBody;
use cb_types::{Side, Tif, H256};
use serde_json::json;

/// What kind of order was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderType {
    /// A price-bounded order.
    Limit,
    /// Take whatever the book offers.
    Market,
}

impl OrderType {
    /// Parse the contract's `type` field.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "limit" => Some(Self::Limit),
            "market" => Some(Self::Market),
            _ => None,
        }
    }

    /// The wire representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Limit => "limit",
            Self::Market => "market",
        }
    }
}

/// Parse the contract's `side` field.
pub fn parse_side(value: &str) -> Option<Side> {
    match value {
        "buy" => Some(Side::Buy),
        "sell" => Some(Side::Sell),
        _ => None,
    }
}

/// The wire representation of a side.
pub fn side_str(side: Side) -> &'static str {
    match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

/// Parse the contract's `tif` field.
pub fn parse_tif(value: &str) -> Option<Tif> {
    match value {
        "gtc" => Some(Tif::Gtc),
        "ioc" => Some(Tif::Ioc),
        "fok" => Some(Tif::Fok),
        _ => None,
    }
}

/// The wire representation of a time in force.
pub fn tif_str(tif: Tif) -> &'static str {
    match tif {
        Tif::Gtc => "gtc",
        Tif::Ioc => "ioc",
        Tif::Fok => "fok",
    }
}

/// An order request after parsing and validation, in chain units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedOrder {
    /// The market's symbol, as the caller wrote it.
    pub symbol: String,
    /// The chain handle.
    pub market: MarketRef,
    /// Direction.
    pub side: Side,
    /// Limit or market.
    pub order_type: OrderType,
    /// Price in the market's wire price scale. For a market order this is the
    /// protective bound the gateway chose, never a price the user gave.
    pub price: u128,
    /// Quantity in the market's wire quantity scale.
    pub qty: u128,
    /// Time in force.
    pub tif: Tif,
    /// Whether the order must not take liquidity.
    pub post_only: bool,
    /// Whether the order may only shrink a position.
    pub reduce_only: bool,
    /// Idempotency key, hashed into the on-chain client order id.
    pub client_order_id: Option<String>,
}

/// The price a market order is submitted at.
///
/// The chain has no market-order variant on the spot path, and the perp
/// `OpenPosition` variant cannot express post-only or a client order id. Both
/// are therefore expressed as an aggressive limit at a protective bound: the
/// far side of the book crossed by a slippage allowance, with IOC or FOK time
/// in force. An unbounded market order — `u128::MAX` for a buy — would be a
/// gift to anyone who can move the book in the same block.
pub const MARKET_ORDER_SLIPPAGE_BPS: u128 = 500;

/// Build the aggressive bound a market order is submitted at.
///
/// Returns `None` when the relevant side of the book is empty, which the
/// caller turns into a rejection: a market order against no liquidity should
/// fail loudly, not rest.
pub fn market_order_bound(book: &BookSnapshot, side: Side) -> Option<u128> {
    let best = match side {
        Side::Buy => book.asks.first()?.price,
        Side::Sell => book.bids.first()?.price,
    };
    Some(match side {
        Side::Buy => best.saturating_mul(10_000 + MARKET_ORDER_SLIPPAGE_BPS) / 10_000,
        Side::Sell => best.saturating_mul(10_000 - MARKET_ORDER_SLIPPAGE_BPS) / 10_000,
    })
}

/// What the caller sent, before validation.
#[derive(Debug, Clone, Default)]
pub struct OrderInput {
    /// Market symbol.
    pub symbol: String,
    /// `buy` or `sell`.
    pub side: String,
    /// `limit` or `market`.
    pub order_type: String,
    /// Decimal price string; absent for market orders.
    pub price: Option<String>,
    /// Decimal quantity string.
    pub qty: String,
    /// `gtc`, `ioc` or `fok`; defaults to `gtc`.
    pub tif: Option<String>,
    /// Post-only flag.
    pub post_only: bool,
    /// Reduce-only flag.
    pub reduce_only: bool,
    /// Idempotency key.
    pub client_order_id: Option<String>,
}

/// Everything validation needs to look at.
pub struct ValidationContext<'a> {
    /// The resolved market.
    pub entry: &'a MarketEntry,
    /// Published metadata: tick, lot, minimum notional, status.
    pub meta: &'a MarketMeta,
    /// Current depth, for the post-only and market-order checks.
    pub book: &'a BookSnapshot,
    /// The caller's open position on this market, if any.
    pub position: Option<&'a PositionSnapshot>,
    /// Whether a fresh mark price exists. A derivative cannot be margined
    /// against a stale oracle.
    pub mark_is_fresh: bool,
    /// The market's current mark, `1e8`-scaled, when there is one. A
    /// derivative order's price must lie in the chain's price band around it
    /// (`cb_engine::DERIVATIVE_PRICE_BAND_BPS`); `None` skips the check here
    /// and leaves it to the chain.
    pub mark_px_q8: Option<u128>,
    /// Margin the caller can put behind a new derivative position, in
    /// micro-USD: cross collateral plus unrealised PnL. `None` on spot, where
    /// the concept does not apply.
    pub available_margin: Option<u128>,
    /// What the caller can spend on a spot market. `None` on a derivative,
    /// where the balance is not what an order is admitted against.
    pub spot_funds: Option<SpotFunds>,
}

/// The two balances a spot order is paid for out of.
///
/// Both are *available* balances — the chain's balance less what is already
/// locked against resting orders — because an amount reserved for one order
/// cannot also pay for the next.
#[derive(Debug, Clone, Copy, Default)]
pub struct SpotFunds {
    /// Available base asset, in its base units. What a sell is paid with.
    pub base_available: u128,
    /// Available quote asset, in its base units. What a buy is paid with.
    pub quote_available: u128,
}

/// Validate an order request against a market.
pub fn validate(input: &OrderInput, ctx: &ValidationContext<'_>) -> ApiResult<ValidatedOrder> {
    let scales = ctx.entry.scales;

    match ctx.meta.status.as_str() {
        "trading" => {}
        "settling" if input.reduce_only => {}
        status => {
            return Err(ApiError::new(
                ErrorCode::MarketHalted,
                format!("{} is {status}", ctx.entry.symbol),
            )
            .with_details(json!({ "status": status })))
        }
    }

    let side = parse_side(&input.side)
        .ok_or_else(|| ApiError::validation("side must be `buy` or `sell`"))?;
    let order_type = OrderType::parse(&input.order_type)
        .ok_or_else(|| ApiError::validation("type must be `limit` or `market`"))?;
    let tif = match input.tif.as_deref() {
        None | Some("") => Tif::Gtc,
        Some(value) => parse_tif(value)
            .ok_or_else(|| ApiError::validation("tif must be `gtc`, `ioc` or `fok`"))?,
    };

    let qty = parse_scaled(&input.qty, scales.qty_decimals)
        .map_err(|e| ApiError::validation(format!("qty: {e}")))?;
    if qty == 0 {
        return Err(ApiError::validation("qty must be greater than zero"));
    }

    // Lot size, from the published metadata, in wire units.
    let lot = wire_increment(&ctx.meta.lot_size, scales.qty_decimals);
    if lot > 0 && qty % lot != 0 {
        return Err(ApiError::validation(format!(
            "qty must be a multiple of the lot size {}",
            ctx.meta.lot_size
        ))
        .with_details(json!({ "lot_size": ctx.meta.lot_size.to_string() })));
    }

    let tick = wire_increment(&ctx.meta.tick_size, scales.price_decimals);
    let (price, tif) = match order_type {
        OrderType::Limit => {
            let raw = input
                .price
                .as_deref()
                .filter(|p| !p.is_empty())
                .ok_or_else(|| ApiError::validation("a limit order needs a price"))?;
            let price = parse_scaled(raw, scales.price_decimals)
                .map_err(|e| ApiError::validation(format!("price: {e}")))?;
            if price == 0 {
                return Err(ApiError::validation("price must be greater than zero"));
            }
            if tick > 0 && price % tick != 0 {
                return Err(ApiError::validation(format!(
                    "price must be a multiple of the tick size {}",
                    ctx.meta.tick_size
                ))
                .with_details(json!({ "tick_size": ctx.meta.tick_size.to_string() })));
            }
            (price, tif)
        }
        OrderType::Market => {
            if input.price.as_deref().is_some_and(|p| !p.is_empty()) {
                return Err(ApiError::validation(
                    "a market order must not carry a price",
                ));
            }
            if input.post_only {
                return Err(ApiError::new(
                    ErrorCode::PostOnlyWouldCross,
                    "a market order takes liquidity by definition",
                ));
            }
            let bound = market_order_bound(ctx.book, side).ok_or_else(|| {
                ApiError::validation("there is no liquidity on that side of the book")
            })?;
            // Round the protective bound onto the tick grid, outward, so it
            // stays at least as aggressive as intended.
            let bound = if tick > 0 {
                match side {
                    Side::Buy => bound.div_ceil(tick).saturating_mul(tick),
                    Side::Sell => (bound / tick).saturating_mul(tick),
                }
            } else {
                bound
            };
            if bound == 0 {
                return Err(ApiError::validation(
                    "the protective bound for this market order rounds to zero",
                ));
            }
            // A market order that rests is not a market order.
            let tif = if tif == Tif::Gtc { Tif::Ioc } else { tif };
            (bound, tif)
        }
    };

    // The chain's price band: a derivative order is refused unless its price
    // lies within `DERIVATIVE_PRICE_BAND_BPS` of the mark. A limit order
    // outside it is refused here with a code the client can act on, rather
    // than submitted, answered `201 pending`, and refused by the chain. A
    // market order's protective bound is a limit the gateway chose, not the
    // trader, so it is tightened into the band instead. The refusal is
    // reported after the minimum-notional check, so a malformed order still
    // gets its `400` first.
    let (price, out_of_band) = match (ctx.entry.kind, ctx.mark_px_q8) {
        (InstrumentKind::Perp | InstrumentKind::Future, Some(mark)) if mark > 0 => {
            let (lo, hi) = cb_engine::derivative_price_band(mark);
            let price = match order_type {
                OrderType::Limit => price,
                OrderType::Market => match side {
                    Side::Buy => price.min(hi.checked_div(tick).map_or(hi, |n| n * tick)),
                    Side::Sell => price.max(if tick > 0 {
                        lo.div_ceil(tick).saturating_mul(tick)
                    } else {
                        lo
                    }),
                },
            };
            let out_of_band = (price < lo || price > hi).then(|| {
                ApiError::new(
                    ErrorCode::PriceOutOfBand,
                    format!(
                        "a {} order must be priced within {}% of the mark",
                        ctx.entry.symbol,
                        cb_engine::DERIVATIVE_PRICE_BAND_BPS / 100
                    ),
                )
                .with_details(json!({
                    "min_price": crate::decimal::format_scaled(
                        lo,
                        scales.price_decimals,
                        ctx.meta.price_decimals
                    ),
                    "max_price": crate::decimal::format_scaled(
                        hi,
                        scales.price_decimals,
                        ctx.meta.price_decimals
                    ),
                }))
            });
            (price, out_of_band)
        }
        _ => (price, None),
    };

    // Minimum notional, evaluated in micro-USD the way the chain computes it.
    let notional = notional_micro_usd(ctx.entry, price, qty);
    let min_notional = wire_increment(&ctx.meta.min_notional, 6);
    if notional < min_notional {
        return Err(ApiError::validation(format!(
            "order notional is below the {} minimum for {}",
            ctx.meta.min_notional, ctx.entry.symbol
        ))
        .with_details(json!({
            "min_notional": ctx.meta.min_notional.to_string(),
            "notional": crate::decimal::format_scaled(notional, 6, 2),
        })));
    }
    if let Some(error) = out_of_band {
        return Err(error);
    }

    if input.post_only && would_cross(ctx.book, side, price) {
        return Err(ApiError::new(
            ErrorCode::PostOnlyWouldCross,
            "a post-only order at that price would take liquidity",
        ));
    }

    if input.reduce_only {
        match ctx.entry.kind {
            InstrumentKind::Spot => {
                // The spot transaction has no reduce-only field, so accepting
                // the flag would mean silently dropping it.
                return Err(ApiError::validation(
                    "reduce-only is not available on spot markets",
                ));
            }
            _ => {
                check_reduce_only(ctx.position, side, qty)?;
            }
        }
    }

    // A spot order is paid for out of a balance, and the chain will reject one
    // that is not. Checking here is what turns "accepted, and pending forever"
    // into the contract's `INSUFFICIENT_BALANCE` — an order the sequencer
    // refuses is submitted, answered `201 pending`, and then simply never
    // appears, which reads to the user as the exchange having lost it.
    if ctx.entry.kind == InstrumentKind::Spot {
        if let Some(funds) = ctx.spot_funds {
            let (required, available, asset, decimals) = match side {
                // A buy spends quote: the notional is already in the quote
                // asset's base units, by the same arithmetic the executor uses.
                Side::Buy => (
                    notional,
                    funds.quote_available,
                    ctx.entry.quote,
                    scales.quote_decimals,
                ),
                // A sell spends base: the quantity itself.
                Side::Sell => (
                    qty,
                    funds.base_available,
                    ctx.entry.base,
                    scales.qty_decimals,
                ),
            };
            if required > available {
                return Err(ApiError::new(
                    ErrorCode::InsufficientBalance,
                    format!(
                        "this order needs {} {} and {} is available",
                        crate::decimal::format_scaled(
                            required,
                            decimals,
                            crate::symbols::asset_display_decimals(asset)
                        ),
                        asset.symbol(),
                        crate::decimal::format_scaled(
                            available,
                            decimals,
                            crate::symbols::asset_display_decimals(asset)
                        ),
                    ),
                )
                .with_details(json!({
                    "asset": asset.symbol(),
                    "required": crate::decimal::format_scaled(
                        required,
                        decimals,
                        crate::symbols::asset_display_decimals(asset)
                    ),
                    "available": crate::decimal::format_scaled(
                        available,
                        decimals,
                        crate::symbols::asset_display_decimals(asset)
                    ),
                })));
            }
        }
    }

    if matches!(
        ctx.entry.kind,
        InstrumentKind::Perp | InstrumentKind::Future
    ) {
        if !ctx.mark_is_fresh {
            return Err(ApiError::new(
                ErrorCode::OracleStale,
                format!("{} has no fresh mark price", ctx.entry.symbol),
            ));
        }
        // The sequencer has no distinct rejection for an under-margined
        // order — it answers `insufficientBalance`, which the contract
        // reserves for spot. Checking here is the only way the frontend ever
        // sees `INSUFFICIENT_MARGIN`. A reduce-only order releases margin
        // rather than consuming it, so it is exempt.
        if !input.reduce_only {
            if let (Some(available), Some(bps)) =
                (ctx.available_margin, ctx.meta.initial_margin_bps)
            {
                let required = notional.saturating_mul(bps as u128).div_ceil(10_000);
                if required > available {
                    return Err(ApiError::new(
                        ErrorCode::InsufficientMargin,
                        "this order would breach the initial margin requirement",
                    )
                    .with_details(json!({
                        "required": crate::decimal::format_scaled(required, 6, 2),
                        "available": crate::decimal::format_scaled(available, 6, 2),
                    })));
                }
            }
        }
    }

    if let Some(id) = &input.client_order_id {
        if id.len() > 64 {
            return Err(ApiError::validation(
                "client_order_id must be at most 64 characters",
            ));
        }
    }

    Ok(ValidatedOrder {
        symbol: ctx.entry.symbol.clone(),
        market: ctx.entry.market,
        side,
        order_type,
        price,
        qty,
        tif,
        post_only: input.post_only,
        reduce_only: input.reduce_only,
        client_order_id: input.client_order_id.clone(),
    })
}

/// Whether a resting order at `price` would immediately take liquidity.
pub fn would_cross(book: &BookSnapshot, side: Side, price: u128) -> bool {
    match side {
        Side::Buy => book.asks.first().is_some_and(|ask| price >= ask.price),
        Side::Sell => book.bids.first().is_some_and(|bid| price <= bid.price),
    }
}

/// Reject a reduce-only order that would grow, flip or exceed the position.
fn check_reduce_only(position: Option<&PositionSnapshot>, side: Side, qty: u128) -> ApiResult<()> {
    let reduce_only_error =
        |message: &str| ApiError::new(ErrorCode::ReduceOnlyWouldIncrease, message.to_string());
    let Some(position) = position.filter(|p| p.qty > 0) else {
        return Err(reduce_only_error(
            "reduce-only was set but there is no open position to reduce",
        ));
    };
    // Reducing a long means selling; reducing a short means buying.
    let reducing_side = match position.side {
        Side::Buy => Side::Sell,
        Side::Sell => Side::Buy,
    };
    if side != reducing_side {
        return Err(reduce_only_error(
            "a reduce-only order must be on the opposite side to the position",
        ));
    }
    if qty > position.qty {
        return Err(reduce_only_error(
            "a reduce-only order may not exceed the position size",
        ));
    }
    Ok(())
}

/// Notional in micro-USD, computed exactly as the chain does it.
///
/// The two formulas differ — `price * qty / 1e8` for spot against
/// `qty * px_q8 / 1e10` for derivatives — and using the wrong one is a
/// hundred-fold error, so the market's kind picks it rather than a guess.
pub fn notional_micro_usd(entry: &MarketEntry, price: u128, qty: u128) -> u128 {
    match entry.kind {
        InstrumentKind::Spot => price.saturating_mul(qty) / 100_000_000u128,
        InstrumentKind::Perp | InstrumentKind::Future => {
            qty.saturating_mul(price) / 10_000_000_000u128
        }
    }
}

/// A published increment (a `Decimal`) back in wire units.
fn wire_increment(value: &rust_decimal::Decimal, decimals: u32) -> u128 {
    crate::decimal::parse_scaled(&value.normalize().to_string(), decimals).unwrap_or(0)
}

/// Turn the caller's idempotency key into the chain's 32-byte client order id.
///
/// Hashing rather than truncating means any string the contract allows maps to
/// a valid id, and two different keys cannot collide into one on-chain id by
/// sharing a prefix. The user id is mixed in so two users may independently
/// use `"cli_1"` without colliding on chain.
pub fn client_order_id_hash(user_id: &str, client_order_id: Option<&str>) -> H256 {
    match client_order_id {
        Some(id) => {
            let mut preimage = Vec::with_capacity(user_id.len() + id.len() + 1);
            preimage.extend_from_slice(user_id.as_bytes());
            preimage.push(0);
            preimage.extend_from_slice(id.as_bytes());
            cb_crypto::keccak256(&preimage)
        }
        None => H256([0u8; 32]),
    }
}

/// Build the transaction body for a validated order.
pub fn to_tx_body(order: &ValidatedOrder, client_order_id: H256) -> ApiResult<TxBody> {
    Ok(match order.market {
        MarketRef::Spot(market) => {
            if order.reduce_only {
                return Err(ApiError::validation(
                    "reduce-only is not available on spot markets",
                ));
            }
            TxBody::PlaceOrder {
                market,
                side: order.side,
                price: order.price,
                qty: order.qty,
                tif: order.tif,
                post_only: order.post_only,
                client_order_id,
            }
        }
        MarketRef::Deriv(market) => TxBody::PlacePerpOrder {
            market,
            side: order.side,
            price_q8: order.price,
            qty: order.qty,
            tif: order.tif,
            post_only: order.post_only,
            reduce_only: order.reduce_only,
            client_order_id,
        },
    })
}

/// Build the cancel transaction for an order on a given market.
pub fn cancel_tx_body(market: MarketRef, order_id: H256) -> TxBody {
    match market {
        MarketRef::Spot(_) => TxBody::CancelOrder { order_id },
        MarketRef::Deriv(_) => TxBody::CancelPerpOrder { order_id },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::BookLevel;
    use crate::symbols::SymbolTable;
    use rust_decimal::Decimal;

    fn perp_entry() -> MarketEntry {
        MarketEntry {
            symbol: "BTC-PERP".into(),
            market: MarketRef::Deriv(cb_types::DerivId::perp(0)),
            kind: InstrumentKind::Perp,
            base: cb_types::AssetId::Btc,
            quote: cb_types::AssetId::Usd,
            scales: crate::symbols::MarketScales::deriv(),
            asset_class: "crypto",
            display_name: "Bitcoin Perpetual".into(),
        }
    }

    fn meta() -> MarketMeta {
        MarketMeta {
            symbol: "BTC-PERP".into(),
            kind: "perp".into(),
            asset_class: "crypto".into(),
            base: "BTC".into(),
            quote: "USD".into(),
            display_name: "Bitcoin Perpetual".into(),
            status: "trading".into(),
            tick_size: Decimal::new(10, 1),      // 1.0
            lot_size: Decimal::new(1, 4),        // 0.0001
            min_notional: Decimal::new(1000, 2), // 10.00
            price_decimals: 2,
            size_decimals: 4,
            max_leverage: Some(Decimal::from(50)),
            initial_margin_bps: Some(200),
            maintenance_margin_bps: Some(100),
            funding_interval_seconds: Some(3600),
            expiry: None,
            maker_fee_bps: -1,
            taker_fee_bps: 4,
        }
    }

    fn book() -> BookSnapshot {
        BookSnapshot {
            bids: vec![BookLevel {
                price: 6_424_900_000_000,
                qty: 100_000_000,
            }],
            asks: vec![BookLevel {
                price: 6_425_100_000_000,
                qty: 100_000_000,
            }],
            height: 10,
        }
    }

    fn input() -> OrderInput {
        OrderInput {
            symbol: "BTC-PERP".into(),
            side: "buy".into(),
            order_type: "limit".into(),
            price: Some("64000".into()),
            qty: "1".into(),
            tif: Some("gtc".into()),
            post_only: false,
            reduce_only: false,
            client_order_id: Some("cli_a1b2c3".into()),
        }
    }

    fn ctx<'a>(
        entry: &'a MarketEntry,
        meta: &'a MarketMeta,
        book: &'a BookSnapshot,
        position: Option<&'a PositionSnapshot>,
    ) -> ValidationContext<'a> {
        ValidationContext {
            entry,
            meta,
            book,
            position,
            mark_is_fresh: true,
            mark_px_q8: None,
            available_margin: None,
            spot_funds: None,
        }
    }

    #[test]
    fn a_derivative_limit_order_outside_the_price_band_is_refused() {
        let (entry, meta, book) = (perp_entry(), meta(), book());
        let mut context = ctx(&entry, &meta, &book, None);
        // Mark 70,000: the band is [63,000, 77,000], and 64,000 is inside.
        context.mark_px_q8 = Some(70_000 * 100_000_000);
        validate(&input(), &context).expect("in band");
        // Mark 72,000: the band is [64,800, 79,200], and 64,000 is below it.
        context.mark_px_q8 = Some(72_000 * 100_000_000);
        let err = validate(&input(), &context).expect_err("below the band");
        assert_eq!(err.code, ErrorCode::PriceOutOfBand);
        assert_eq!(err.code.status().as_u16(), 422);
        assert_eq!(err.code.as_str(), "PRICE_OUT_OF_BAND");
        // The edge itself is accepted.
        let mut edge = input();
        edge.price = Some("64800".into());
        validate(&edge, &context).expect("the band's edge is in band");
    }

    #[test]
    fn a_well_formed_limit_order_is_accepted_and_scaled() {
        let (entry, meta, book) = (perp_entry(), meta(), book());
        let order = validate(&input(), &ctx(&entry, &meta, &book, None)).expect("valid");
        assert_eq!(order.price, 6_400_000_000_000, "64000 at q8");
        assert_eq!(order.qty, 100_000_000, "1 contract unit at 1e8");
        assert_eq!(order.tif, Tif::Gtc);
    }

    #[test]
    fn a_price_off_the_tick_grid_is_rejected() {
        let (entry, meta, book) = (perp_entry(), meta(), book());
        let mut bad = input();
        bad.price = Some("64000.25".into());
        let err = validate(&bad, &ctx(&entry, &meta, &book, None)).expect_err("off tick");
        assert_eq!(err.code, ErrorCode::ValidationFailed);
        assert!(err.message.contains("tick size"), "{}", err.message);
    }

    #[test]
    fn a_quantity_off_the_lot_grid_is_rejected() {
        let (entry, meta, book) = (perp_entry(), meta(), book());
        let mut bad = input();
        bad.qty = "1.00005".into();
        let err = validate(&bad, &ctx(&entry, &meta, &book, None)).expect_err("off lot");
        assert!(err.message.contains("lot size"), "{}", err.message);
    }

    #[test]
    fn precision_finer_than_the_market_is_rejected_not_truncated() {
        let (entry, meta, book) = (perp_entry(), meta(), book());
        let mut bad = input();
        bad.qty = "1.0000000001".into();
        let err = validate(&bad, &ctx(&entry, &meta, &book, None)).expect_err("too precise");
        assert_eq!(err.code, ErrorCode::ValidationFailed);
    }

    #[test]
    fn an_order_below_the_minimum_notional_is_rejected() {
        let (entry, meta, book) = (perp_entry(), meta(), book());
        let mut small = input();
        small.qty = "0.0001".into();
        small.price = Some("50".into());
        let err = validate(&small, &ctx(&entry, &meta, &book, None)).expect_err("too small");
        assert!(err.message.contains("below"), "{}", err.message);
        assert!(err.details.is_some(), "the frontend needs the threshold");
    }

    #[test]
    fn a_post_only_order_that_would_cross_is_rejected_with_its_own_code() {
        let (entry, meta, book) = (perp_entry(), meta(), book());
        let mut crossing = input();
        crossing.post_only = true;
        crossing.price = Some("64252".into());
        let err = validate(&crossing, &ctx(&entry, &meta, &book, None)).expect_err("crosses");
        assert_eq!(err.code, ErrorCode::PostOnlyWouldCross);

        // ...and the same order one tick below the ask is fine.
        let mut resting = input();
        resting.post_only = true;
        resting.price = Some("64250".into());
        validate(&resting, &ctx(&entry, &meta, &book, None)).expect("rests");
    }

    #[test]
    fn a_reduce_only_order_that_would_increase_a_position_is_rejected() {
        let (entry, meta, book) = (perp_entry(), meta(), book());
        let long = PositionSnapshot {
            market: cb_types::DerivId::perp(0),
            side: Side::Buy,
            qty: 150_000_000,
            entry_px_q8: 6_300_000_000_000,
            margin_mode: 0,
            isolated_margin: 0,
            entry_funding_index: 0,
            last_funding_height: 0,
        };

        // Buying while long grows it.
        let mut growing = input();
        growing.reduce_only = true;
        let err = validate(&growing, &ctx(&entry, &meta, &book, Some(&long))).expect_err("grows");
        assert_eq!(err.code, ErrorCode::ReduceOnlyWouldIncrease);

        // Selling more than the position flips it.
        let mut flipping = input();
        flipping.reduce_only = true;
        flipping.side = "sell".into();
        flipping.qty = "2".into();
        flipping.price = Some("64000".into());
        let err = validate(&flipping, &ctx(&entry, &meta, &book, Some(&long))).expect_err("flips");
        assert_eq!(err.code, ErrorCode::ReduceOnlyWouldIncrease);

        // Selling within the position is what reduce-only is for.
        let mut reducing = input();
        reducing.reduce_only = true;
        reducing.side = "sell".into();
        reducing.qty = "1".into();
        validate(&reducing, &ctx(&entry, &meta, &book, Some(&long))).expect("reduces");
    }

    #[test]
    fn reduce_only_without_a_position_is_rejected() {
        let (entry, meta, book) = (perp_entry(), meta(), book());
        let mut order = input();
        order.reduce_only = true;
        order.side = "sell".into();
        let err =
            validate(&order, &ctx(&entry, &meta, &book, None)).expect_err("nothing to reduce");
        assert_eq!(err.code, ErrorCode::ReduceOnlyWouldIncrease);
    }

    #[test]
    fn a_halted_market_refuses_orders() {
        let (entry, book) = (perp_entry(), book());
        let mut halted = meta();
        halted.status = "halted".into();
        let err = validate(&input(), &ctx(&entry, &halted, &book, None)).expect_err("halted");
        assert_eq!(err.code, ErrorCode::MarketHalted);
    }

    #[test]
    fn a_settling_market_takes_closing_flow_only() {
        let (entry, book) = (perp_entry(), book());
        let mut settling = meta();
        settling.status = "settling".into();
        assert_eq!(
            validate(&input(), &ctx(&entry, &settling, &book, None))
                .expect_err("opening flow")
                .code,
            ErrorCode::MarketHalted
        );

        let long = PositionSnapshot {
            market: cb_types::DerivId::perp(0),
            side: Side::Buy,
            qty: 200_000_000,
            entry_px_q8: 1,
            margin_mode: 0,
            isolated_margin: 0,
            entry_funding_index: 0,
            last_funding_height: 0,
        };
        let mut closing = input();
        closing.reduce_only = true;
        closing.side = "sell".into();
        validate(&closing, &ctx(&entry, &settling, &book, Some(&long)))
            .expect("closing is allowed");
    }

    #[test]
    fn a_stale_mark_price_blocks_a_derivative_order() {
        let (entry, meta, book) = (perp_entry(), meta(), book());
        let mut context = ctx(&entry, &meta, &book, None);
        context.mark_is_fresh = false;
        assert_eq!(
            validate(&input(), &context).expect_err("stale").code,
            ErrorCode::OracleStale
        );
    }

    #[test]
    fn an_under_margined_order_is_rejected_with_its_own_code() {
        let (entry, meta, book) = (perp_entry(), meta(), book());
        let mut context = ctx(&entry, &meta, &book, None);
        // 1 BTC at $64,000 needs 2% = $1,280 of margin. Offer $1,000.
        context.available_margin = Some(1_000_000_000);
        let err = validate(&input(), &context).expect_err("under-margined");
        assert_eq!(err.code, ErrorCode::InsufficientMargin);
        let details = err.details.expect("the frontend shows the shortfall");
        assert_eq!(details["required"], "1280.00");
        assert_eq!(details["available"], "1000.00");

        // With enough margin the same order is fine.
        context.available_margin = Some(2_000_000_000);
        validate(&input(), &context).expect("margined");
    }

    #[test]
    fn a_reduce_only_order_is_exempt_from_the_margin_check() {
        let (entry, meta, book) = (perp_entry(), meta(), book());
        let long = PositionSnapshot {
            market: cb_types::DerivId::perp(0),
            side: Side::Buy,
            qty: 100_000_000,
            entry_px_q8: 6_300_000_000_000,
            margin_mode: 0,
            isolated_margin: 0,
            entry_funding_index: 0,
            last_funding_height: 0,
        };
        let mut context = ctx(&entry, &meta, &book, Some(&long));
        context.available_margin = Some(0);
        let mut closing = input();
        closing.reduce_only = true;
        closing.side = "sell".into();
        validate(&closing, &context).expect("closing frees margin, it does not need it");
    }

    #[test]
    fn a_market_order_gets_a_protective_bound_and_never_rests() {
        let (entry, meta, book) = (perp_entry(), meta(), book());
        let mut market = input();
        market.order_type = "market".into();
        market.price = None;
        let order = validate(&market, &ctx(&entry, &meta, &book, None)).expect("valid");
        assert_eq!(order.tif, Tif::Ioc, "a market order must not rest");
        assert!(
            order.price > book.asks[0].price,
            "the bound must cross the book"
        );
        assert!(
            order.price < book.asks[0].price * 2,
            "but it must still be a bound, not infinity"
        );
        assert_eq!(
            order.price % 100_000_000,
            0,
            "and it must sit on the tick grid"
        );
    }

    #[test]
    fn a_market_order_against_an_empty_book_is_rejected() {
        let (entry, meta) = (perp_entry(), meta());
        let empty = BookSnapshot::default();
        let mut market = input();
        market.order_type = "market".into();
        market.price = None;
        assert!(validate(&market, &ctx(&entry, &meta, &empty, None)).is_err());
    }

    #[test]
    fn a_market_order_carrying_a_price_is_a_client_bug_and_is_reported_as_one() {
        let (entry, meta, book) = (perp_entry(), meta(), book());
        let mut market = input();
        market.order_type = "market".into();
        assert_eq!(
            validate(&market, &ctx(&entry, &meta, &book, None))
                .expect_err("price on a market order")
                .code,
            ErrorCode::ValidationFailed
        );
    }

    #[test]
    fn spot_orders_refuse_reduce_only_rather_than_dropping_it() {
        let table = SymbolTable::with_spot_markets();
        let entry = table.get("BTC-USD").expect("listed").clone();
        let mut spot_meta = meta();
        spot_meta.symbol = "BTC-USD".into();
        spot_meta.kind = "spot".into();
        spot_meta.tick_size = Decimal::new(1, 2);
        spot_meta.lot_size = Decimal::new(1, 4);
        let book = BookSnapshot {
            bids: vec![BookLevel {
                price: 64_249_000_000,
                qty: 100_000_000,
            }],
            asks: vec![BookLevel {
                price: 64_251_000_000,
                qty: 100_000_000,
            }],
            height: 1,
        };
        let mut order = input();
        order.symbol = "BTC-USD".into();
        order.reduce_only = true;
        let err = validate(&order, &ctx(&entry, &spot_meta, &book, None)).expect_err("spot");
        assert_eq!(err.code, ErrorCode::ValidationFailed);
        assert!(err.message.contains("reduce-only"), "{}", err.message);
    }

    #[test]
    fn the_client_order_id_hash_separates_users() {
        let a = client_order_id_hash("usr_a", Some("cli_1"));
        let b = client_order_id_hash("usr_b", Some("cli_1"));
        assert_ne!(a, b, "two users may both use `cli_1`");
        assert_eq!(a, client_order_id_hash("usr_a", Some("cli_1")), "stable");
        assert_eq!(client_order_id_hash("usr_a", None), H256([0u8; 32]));
    }

    #[test]
    fn the_transaction_body_matches_the_market_kind() {
        let (entry, meta, book) = (perp_entry(), meta(), book());
        let order = validate(&input(), &ctx(&entry, &meta, &book, None)).expect("valid");
        let body = to_tx_body(&order, H256([7u8; 32])).expect("body");
        assert!(matches!(body, TxBody::PlacePerpOrder { .. }));
        assert!(matches!(
            cancel_tx_body(entry.market, H256([1u8; 32])),
            TxBody::CancelPerpOrder { .. }
        ));
        assert!(matches!(
            cancel_tx_body(MarketRef::Spot(cb_types::MarketId::BtcUsd), H256([1u8; 32])),
            TxBody::CancelOrder { .. }
        ));
    }

    #[test]
    fn notional_uses_the_right_formula_for_each_market_kind() {
        let perp = perp_entry();
        // 1 BTC at $64,250 on a q8 market.
        assert_eq!(
            notional_micro_usd(&perp, 6_425_000_000_000, 100_000_000),
            64_250_000_000
        );
        let table = SymbolTable::with_spot_markets();
        let spot = table.get("BTC-USD").expect("listed");
        // The same trade on spot, where price is 1e6-scaled.
        assert_eq!(
            notional_micro_usd(spot, 64_250_000_000, 100_000_000),
            64_250_000_000
        );
    }
}
