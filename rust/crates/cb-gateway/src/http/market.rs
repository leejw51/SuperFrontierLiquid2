//! Public market data.
//!
//! Three fields the contract asks for are not available over the node's RPC
//! and are reported as zero rather than invented: `open_interest` (no query
//! exists), and — on any market the gateway has not yet watched trade — the
//! 24-hour statistics, which are computed from the candles the gateway itself
//! aggregated. Each is noted where it is produced.

use super::dto::*;
use super::resolve_market;
use super::Json;
use crate::decimal::{format_port_decimal, format_scaled, format_scaled_signed};
use crate::error::{ApiError, ApiResult};
use crate::marketdata::FundingPoint as CachedFunding;
use crate::node::NodeClient;
use crate::state::AppState;
use crate::symbols::{asset_display_decimals, InstrumentKind, MarketEntry, MarketRef};
use axum::extract::{Path, Query, State};
use cb_ports::{CandleInterval, MarketMeta};
use chrono::{Duration, Utc};
use serde::Deserialize;
use std::sync::Arc;

/// Query for the order-book endpoint.
#[derive(Debug, Deserialize)]
pub struct DepthQuery {
    /// Levels per side.
    #[serde(default)]
    pub depth: Option<u32>,
}

/// Query for endpoints that return a list.
#[derive(Debug, Deserialize)]
pub struct LimitQuery {
    /// How many rows.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Query for the candles endpoint.
#[derive(Debug, Deserialize)]
pub struct CandlesQuery {
    /// Bucket width.
    #[serde(default)]
    pub interval: Option<String>,
    /// How many bars.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// `GET /markets`.
pub async fn list(State(state): State<Arc<AppState>>) -> ApiResult<Json<MarketsResponse>> {
    let catalog = state.markets.snapshot();
    Ok(Json(MarketsResponse {
        markets: catalog.metas.values().map(render_market).collect(),
    }))
}

/// `GET /markets/{symbol}/ticker`.
pub async fn ticker(
    State(state): State<Arc<AppState>>,
    Path(symbol): Path<String>,
) -> ApiResult<Json<Ticker>> {
    let catalog = state.markets.snapshot();
    let (entry, meta) = resolve_market(&catalog, &symbol)?;
    let price_decimals = entry.scales.price_decimals;
    let display = meta.price_decimals;

    let (mark, index, funding, next_funding_at) = match entry.market {
        MarketRef::Deriv(id) => {
            // A derivative market that no oracle has priced yet is an ordinary
            // state of a freshly-started chain, not an internal failure. The
            // node answers "no mark price for perp:0" and propagating that as a
            // 500 took down the whole terminal, because every market on the
            // page fetches a ticker. Report the absence the way the spot branch
            // already does and let the caller render a market with no price.
            let mark = state.node.mark_price(id).await.ok();
            let index = state.node.oracle_price(id).await.ok();
            let funding = state.node.funding_index(id).await.ok();
            let next = funding.as_ref().and_then(|f| {
                let interval = meta.funding_interval_seconds?;
                let elapsed_blocks = state.head_height().saturating_sub(f.last_tick_height);
                let elapsed = elapsed_blocks * state.settings.block_time_ms / 1000;
                Some(state.clock.now() + Duration::seconds(interval as i64 - elapsed as i64))
            });
            (
                mark.map(|m| m.px_q8).unwrap_or(0),
                index.map(|i| i.px_q8).unwrap_or(0),
                funding.map(|f| f.rate_q9),
                next,
            )
        }
        // Spot has no mark, no oracle and no funding. The last trade is the
        // only price the market itself defines.
        MarketRef::Spot(_) => (0, 0, None, None),
    };

    let last = state
        .marketdata
        .last_price(&entry.symbol)
        .unwrap_or(if mark > 0 { mark } else { 0 });
    let stats = day_statistics(&state, &entry.symbol).await?;

    Ok(Json(Ticker {
        symbol: entry.symbol.clone(),
        last_price: format_scaled(last, price_decimals, display),
        mark_price: format_scaled(mark, price_decimals, display),
        index_price: format_scaled(index, price_decimals, display),
        // The node exposes no open-interest query. Reporting zero is the
        // honest answer; inventing a number would be worse.
        open_interest: format_scaled(0, entry.scales.qty_decimals, meta.size_decimals),
        volume_24h: stats.volume,
        change_24h_pct: stats.change_pct,
        high_24h: stats.high,
        low_24h: stats.low,
        funding_rate: funding
            .map(|rate| format_scaled_signed(rate as i128, 9, 6))
            .unwrap_or_else(|| "0".to_string()),
        next_funding_at: next_funding_at.map(|at| at.to_rfc3339()),
    }))
}

/// `GET /markets/{symbol}/orderbook`.
pub async fn orderbook(
    State(state): State<Arc<AppState>>,
    Path(symbol): Path<String>,
    Query(query): Query<DepthQuery>,
) -> ApiResult<Json<OrderBook>> {
    let catalog = state.markets.snapshot();
    let (entry, meta) = resolve_market(&catalog, &symbol)?;
    let depth = state.clamp_depth(query.depth);
    let book = fetch_book(&state, entry, depth).await?;

    let render = |levels: &[crate::node::BookLevel]| -> Vec<[String; 2]> {
        levels
            .iter()
            .map(|level| {
                [
                    format_scaled(
                        level.price,
                        entry.scales.price_decimals,
                        meta.price_decimals,
                    ),
                    format_scaled(level.qty, entry.scales.qty_decimals, meta.size_decimals),
                ]
            })
            .collect()
    };

    Ok(Json(OrderBook {
        symbol: entry.symbol.clone(),
        height: book.height,
        bids: render(&book.bids),
        asks: render(&book.asks),
    }))
}

/// `GET /markets/{symbol}/trades`.
pub async fn trades(
    State(state): State<Arc<AppState>>,
    Path(symbol): Path<String>,
    Query(query): Query<LimitQuery>,
) -> ApiResult<Json<TradesResponse>> {
    let catalog = state.markets.snapshot();
    let (entry, meta) = resolve_market(&catalog, &symbol)?;
    let limit = state.clamp_limit(query.limit, 100) as usize;

    let trades = state
        .marketdata
        .trades(&entry.symbol, limit)
        .into_iter()
        .map(|trade| Trade {
            id: format!("trd_{}_{}", trade.height, trade.price),
            price: format_scaled(
                trade.price,
                entry.scales.price_decimals,
                meta.price_decimals,
            ),
            qty: format_scaled(trade.qty, entry.scales.qty_decimals, meta.size_decimals),
            side: crate::orders::side_str(trade.taker_side).to_string(),
            at: trade.at.to_rfc3339(),
            height: trade.height,
        })
        .collect();
    Ok(Json(TradesResponse { trades }))
}

/// `GET /markets/{symbol}/candles`.
pub async fn candles(
    State(state): State<Arc<AppState>>,
    Path(symbol): Path<String>,
    Query(query): Query<CandlesQuery>,
) -> ApiResult<Json<CandlesResponse>> {
    let catalog = state.markets.snapshot();
    let (entry, meta) = resolve_market(&catalog, &symbol)?;
    let interval_name = query.interval.as_deref().unwrap_or("1m");
    let interval = CandleInterval::parse(interval_name)
        .ok_or_else(|| ApiError::validation("interval must be one of 1m, 5m, 15m, 1h, 4h, 1d"))?;
    let limit = state.clamp_limit(query.limit, 500);

    let mut bars = state
        .candles
        .history(&entry.symbol, interval, limit)
        .await?;
    // The in-progress bar is not yet the newest row in storage on every
    // backend, so fold it in explicitly rather than assume.
    if let Some(live) = state.candles.in_progress(&entry.symbol, interval) {
        if bars.last().map(|b| b.open_time) != Some(live.open_time) {
            bars.push(live);
        } else if let Some(last) = bars.last_mut() {
            *last = live;
        }
    }

    Ok(Json(CandlesResponse {
        symbol: entry.symbol.clone(),
        interval: interval.as_str().to_string(),
        candles: bars
            .into_iter()
            .map(|bar| Candle {
                t: bar.open_time.to_rfc3339(),
                o: format_port_decimal(bar.open, meta.price_decimals),
                h: format_port_decimal(bar.high, meta.price_decimals),
                l: format_port_decimal(bar.low, meta.price_decimals),
                c: format_port_decimal(bar.close, meta.price_decimals),
                v: format_port_decimal(bar.volume, meta.size_decimals),
            })
            .collect(),
    }))
}

/// `GET /markets/{symbol}/funding`.
pub async fn funding(
    State(state): State<Arc<AppState>>,
    Path(symbol): Path<String>,
    Query(query): Query<LimitQuery>,
) -> ApiResult<Json<FundingResponse>> {
    let catalog = state.markets.snapshot();
    let (entry, meta) = resolve_market(&catalog, &symbol)?;
    let limit = state.clamp_limit(query.limit, 100) as usize;

    let render = |point: CachedFunding| FundingPoint {
        at: point.at.to_rfc3339(),
        rate: format_scaled_signed(point.rate_q9 as i128, 9, 6),
        mark_price: format_scaled(
            point.mark_px_q8,
            entry.scales.price_decimals,
            meta.price_decimals,
        ),
        index_price: format_scaled(
            point.index_px_q8,
            entry.scales.price_decimals,
            meta.price_decimals,
        ),
    };

    Ok(Json(FundingResponse {
        history: state
            .marketdata
            .funding(&entry.symbol, limit)
            .into_iter()
            .map(render)
            .collect(),
    }))
}

/// Fetch depth for a market, whichever book it lives on.
pub async fn fetch_book(
    state: &AppState,
    entry: &MarketEntry,
    depth: u32,
) -> ApiResult<crate::node::BookSnapshot> {
    Ok(match entry.market {
        MarketRef::Spot(market) => state.node.spot_book(market, depth).await?,
        MarketRef::Deriv(market) => state.node.deriv_book(market, depth).await?,
    })
}

/// Render one market's metadata.
pub fn render_market(meta: &MarketMeta) -> Market {
    Market {
        symbol: meta.symbol.clone(),
        kind: meta.kind.clone(),
        asset_class: meta.asset_class.clone(),
        base: meta.base.clone(),
        quote: meta.quote.clone(),
        display_name: meta.display_name.clone(),
        status: meta.status.clone(),
        tick_size: format_port_decimal(meta.tick_size, meta.price_decimals),
        lot_size: format_port_decimal(meta.lot_size, meta.size_decimals),
        min_notional: format_port_decimal(meta.min_notional, 2),
        price_decimals: meta.price_decimals,
        size_decimals: meta.size_decimals,
        max_leverage: meta.max_leverage.map(|v| format_port_decimal(v, 0)),
        initial_margin_bps: meta.initial_margin_bps,
        maintenance_margin_bps: meta.maintenance_margin_bps,
        funding_interval_seconds: meta.funding_interval_seconds,
        expiry: meta.expiry.map(|at| at.to_rfc3339()),
        maker_fee_bps: meta.maker_fee_bps,
        taker_fee_bps: meta.taker_fee_bps,
    }
}

/// The 24-hour statistics the ticker publishes.
struct DayStatistics {
    volume: String,
    change_pct: String,
    high: String,
    low: String,
}

/// Compute 24-hour statistics from the hourly bars the gateway aggregated.
///
/// A market the gateway has never seen trade has no bars, and reports zeros
/// rather than borrowing a number from somewhere else.
async fn day_statistics(state: &AppState, symbol: &str) -> ApiResult<DayStatistics> {
    let bars = state
        .candles
        .history(symbol, CandleInterval::H1, 24)
        .await?;
    if bars.is_empty() {
        return Ok(DayStatistics {
            volume: "0.00".into(),
            change_pct: "0.00".into(),
            high: "0.00".into(),
            low: "0.00".into(),
        });
    }
    let cutoff = state.clock.now() - Duration::hours(24);
    let recent: Vec<&cb_ports::Candle> = bars.iter().filter(|b| b.open_time >= cutoff).collect();
    let recent = if recent.is_empty() {
        bars.iter().collect()
    } else {
        recent
    };

    let mut high = recent[0].high;
    let mut low = recent[0].low;
    let mut volume = rust_decimal::Decimal::ZERO;
    for bar in &recent {
        high = high.max(bar.high);
        low = low.min(bar.low);
        volume += bar.volume;
    }
    let open = recent[0].open;
    let close = recent[recent.len() - 1].close;
    let change = if open.is_zero() {
        rust_decimal::Decimal::ZERO
    } else {
        (close - open) / open * rust_decimal::Decimal::from(100)
    };

    Ok(DayStatistics {
        volume: format_port_decimal(volume, 2),
        change_pct: format_port_decimal(change.round_dp(2), 2),
        high: format_port_decimal(high, 2),
        low: format_port_decimal(low, 2),
    })
}

/// Render an asset amount, with that asset's display precision.
pub fn asset_amount(asset: cb_types::AssetId, amount: u128) -> String {
    format_scaled(
        amount,
        asset.decimals() as u32,
        asset_display_decimals(asset),
    )
}

/// Whether a market entry is a derivative.
pub fn is_derivative(entry: &MarketEntry) -> bool {
    matches!(entry.kind, InstrumentKind::Perp | InstrumentKind::Future)
}

/// The current time, as the contract's timestamp string.
pub fn now_string(state: &AppState) -> String {
    let _ = Utc::now();
    state.clock.now().to_rfc3339()
}

/// The `ticker` channel's payload for one market, as contract §8 defines it.
///
/// The WebSocket frame and `GET /markets/{symbol}/ticker` are rendered from the
/// same reads on purpose. An earlier revision built the frame inline in the
/// watcher and sent two of the five fields; the browser's `TickerUpdate`
/// requires `mark_price` and `change_24h_pct`, so every frame failed to decode
/// and every price on the terminal froze at its page-load value while the
/// connection indicator still read "Live" (§8 of the flow review).
///
/// `last_price` is passed in because the caller has usually just observed the
/// print that caused this frame; `None` falls back to the cached last price,
/// and a market with no price at all yields `None` rather than a zero that
/// would overwrite a real quote.
pub async fn ticker_frame(
    state: &AppState,
    entry: &MarketEntry,
    meta: &MarketMeta,
    last_price: Option<u128>,
) -> Option<serde_json::Value> {
    let display = meta.price_decimals;
    let price_decimals = entry.scales.price_decimals;

    let (mark, funding) = match entry.market {
        MarketRef::Deriv(id) => (
            state
                .node
                .mark_price(id)
                .await
                .map(|m| m.px_q8)
                .unwrap_or(0),
            state.node.funding_index(id).await.map(|f| f.rate_q9).ok(),
        ),
        // Spot has neither a mark price nor funding, and says so with zero
        // rather than borrowing the last trade and calling it a mark.
        MarketRef::Spot(_) => (0, None),
    };

    let last = last_price
        .filter(|p| *p > 0)
        .or_else(|| state.marketdata.last_price(&entry.symbol))
        .unwrap_or(mark);
    if last == 0 && mark == 0 {
        return None;
    }

    let stats = day_statistics(state, &entry.symbol).await.ok()?;
    Some(serde_json::json!({
        "symbol": entry.symbol,
        "last_price": format_scaled(last, price_decimals, display),
        "mark_price": format_scaled(mark, price_decimals, display),
        "change_24h_pct": stats.change_pct,
        "funding_rate": funding
            .map(|rate| format_scaled_signed(rate as i128, 9, 6))
            .unwrap_or_else(|| "0.000000".to_string()),
    }))
}
