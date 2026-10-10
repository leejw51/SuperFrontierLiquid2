//! The authenticated account views.
//!
//! Balances and positions are read from the chain and are authoritative.
//! Orders and fills come from [`crate::tracker`], which reconstructs them by
//! watching the gateway's own submissions land — the node has no per-account
//! order query and no owner on a fill. Every one of these endpoints is scoped
//! to the caller's own user id; there is no path by which one account's data
//! can be reached through another's session.

use super::dto::*;
use super::Json;
use super::{resolve_market, Caller};
use crate::decimal::{format_port_decimal, format_ratio, format_scaled, format_scaled_signed};
use crate::error::ApiResult;
use crate::node::{NodeClient, PositionSnapshot};
use crate::state::AppState;
use crate::symbols::{asset_display_decimals, MarketRef};
use crate::tracker::{FillRecord, OrderRecord};
use axum::extract::{Query, State};
use cb_types::{AssetId, Side};
use chrono::Duration;
use serde::Deserialize;
use std::sync::Arc;

/// Query for `GET /account/orders`.
#[derive(Debug, Deserialize)]
pub struct OrdersQuery {
    /// `open`, `filled`, `cancelled` or `all`.
    #[serde(default)]
    pub status: Option<String>,
    /// Restrict to one market.
    #[serde(default)]
    pub symbol: Option<String>,
}

/// Query for the list endpoints.
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// How many rows.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Restrict to one market.
    #[serde(default)]
    pub symbol: Option<String>,
}

/// `GET /account`.
pub async fn overview(
    State(state): State<Arc<AppState>>,
    caller: Caller,
) -> ApiResult<Json<AccountResponse>> {
    let address = caller.address()?;
    let account = state.node.account(address).await?;
    let margin = state.node.margin_account(address).await?;

    let mut assets: Vec<AssetId> = account.balances.keys().copied().collect();
    for asset in account.locked.keys() {
        if !assets.contains(asset) {
            assets.push(*asset);
        }
    }
    assets.sort_by_key(|a| a.as_u8());

    let balances: Vec<Balance> = assets
        .into_iter()
        .map(|asset| {
            // `free + locked == total`, because the chain's balance *is* the
            // total and `locked` is the reserved part of it. Reading the
            // balance as free and adding the lock to it counted the reserved
            // amount twice, and labelled money already committed to a resting
            // order as spendable.
            let total = account.balance(asset);
            let locked = account.locked_of(asset);
            let decimals = asset.decimals() as u32;
            let display = asset_display_decimals(asset);
            Balance {
                asset: asset.symbol().to_string(),
                free: format_scaled(account.available(asset), decimals, display),
                locked: format_scaled(locked, decimals, display),
                total: format_scaled(total, decimals, display),
            }
        })
        .collect();

    // Equity, margin and risk are all denominated in micro-USD, which is what
    // the engine settles in.
    // The chain's balance is already the total, locks included.
    let usd_total = account.balance(AssetId::Usd) as i128;
    let equity =
        usd_total + margin.collateral as i128 + margin.unrealized_pnl - margin.bad_debt as i128;
    let positions = state.node.positions(address).await.unwrap_or_default();
    let used_margin = used_margin_of(&state, address, &positions).await;
    let available = (margin.collateral as i128 + margin.unrealized_pnl - used_margin).max(0);
    let realized_24h = realized_pnl_since(&state, &caller.user.user_id, Duration::hours(24)).await;

    Ok(Json(AccountResponse {
        // Both, always, and labelled. The balances below belong to the trading
        // address; the login address is how the person recognises the account
        // as theirs. Publishing one unlabelled address was §7 of the flow
        // review: it left the browser unable to decode this reply at all.
        login_address: super::format_address(&caller.login_address),
        trading_address: super::format_address(&caller.trading_address),
        nonce: account.nonce,
        balances,
        portfolio: Portfolio {
            equity: format_scaled_signed(equity.max(0), 6, 2),
            available_margin: format_scaled_signed(available, 6, 2),
            used_margin: format_scaled_signed(used_margin, 6, 2),
            unrealized_pnl: format_scaled_signed(margin.unrealized_pnl, 6, 2),
            realized_pnl_24h: format_scaled_signed(realized_24h, 6, 2),
            margin_ratio: format_ratio(used_margin, equity.max(1), 3),
            liquidation_risk: liquidation_risk(used_margin, equity).to_string(),
        },
    }))
}

/// `GET /account/orders`.
pub async fn orders(
    State(state): State<Arc<AppState>>,
    caller: Caller,
    Query(query): Query<OrdersQuery>,
) -> ApiResult<Json<OrdersResponse>> {
    let status = query.status.as_deref().unwrap_or("open");
    if !["open", "filled", "cancelled", "all"].contains(&status) {
        return Err(crate::error::ApiError::validation(
            "status must be one of open, filled, cancelled, all",
        ));
    }
    let catalog = state.markets.snapshot();
    if let Some(symbol) = &query.symbol {
        resolve_market(&catalog, symbol)?;
    }

    let records =
        state
            .tracker
            .orders_for_user(&caller.user.user_id, status, query.symbol.as_deref());
    Ok(Json(OrdersResponse {
        orders: records
            .iter()
            .map(|record| render_order(&state, record))
            .collect(),
    }))
}

/// `GET /account/positions`.
pub async fn positions(
    State(state): State<Arc<AppState>>,
    caller: Caller,
) -> ApiResult<Json<PositionsResponse>> {
    let address = caller.address()?;
    let mut rendered = Vec::new();

    for position in state.node.positions(address).await? {
        // The engine keeps a flat row briefly after a close; it is not an open
        // position and must not be listed as one. The socket publishes those
        // rows deliberately — see `render_position` — because a client holding
        // a position needs to be told when it is gone.
        if position.qty == 0 {
            continue;
        }
        if let Some(position) = render_position(&state, &position).await {
            rendered.push(position);
        }
    }

    Ok(Json(PositionsResponse {
        positions: rendered,
    }))
}

/// Render one position as the contract's position object.
///
/// Shared with the `account` WebSocket channel, which publishes exactly this
/// shape. The two used to be rendered separately and the socket sent four of
/// the fifteen fields, so every `position_update` frame was dropped by the
/// browser (§16 of the flow review). One renderer cannot drift from itself.
///
/// A **closed** position renders as an ordinary row with `qty` zero rather than
/// as nothing, because that is how a client is told to remove it; the caller
/// decides whether to show it. `None` means only that the catalogue does not
/// know this market.
pub async fn render_position(state: &AppState, position: &PositionSnapshot) -> Option<Position> {
    let catalog = state.markets.snapshot();
    let entry = catalog
        .symbols
        .by_market(MarketRef::Deriv(position.market))?;
    let meta = catalog.meta(&entry.symbol)?;
    let mark = state
        .node
        .mark_price(position.market)
        .await
        .map(|m| m.px_q8)
        .unwrap_or(position.entry_px_q8);

    let notional = position.qty.saturating_mul(mark) / 10u128.pow(10);
    let pnl = crate::orders::notional_micro_usd(entry, mark, position.qty) as i128
        - crate::orders::notional_micro_usd(entry, position.entry_px_q8, position.qty) as i128;
    let pnl = match position.side {
        Side::Buy => pnl,
        Side::Sell => -pnl,
    };
    let margin = if position.margin_mode == 1 {
        position.isolated_margin
    } else {
        let bps = meta.initial_margin_bps.unwrap_or(0) as u128;
        notional.saturating_mul(bps) / 10_000
    };

    Some(Position {
        symbol: entry.symbol.clone(),
        side: match position.side {
            Side::Buy => "long".into(),
            Side::Sell => "short".into(),
        },
        qty: format_scaled(position.qty, entry.scales.qty_decimals, meta.size_decimals),
        entry_price: format_scaled(
            position.entry_px_q8,
            entry.scales.price_decimals,
            meta.price_decimals,
        ),
        mark_price: format_scaled(mark, entry.scales.price_decimals, meta.price_decimals),
        liquidation_price: format_scaled(
            liquidation_price(position, meta, margin),
            entry.scales.price_decimals,
            meta.price_decimals,
        ),
        notional: format_scaled(notional, 6, 2),
        margin_mode: if position.margin_mode == 1 {
            "isolated".into()
        } else {
            "cross".into()
        },
        leverage: format_ratio(notional as i128, margin.max(1) as i128, 2),
        margin: format_scaled(margin, 6, 2),
        unrealized_pnl: format_scaled_signed(pnl, 6, 2),
        unrealized_pnl_pct: format_ratio(pnl.saturating_mul(100), margin.max(1) as i128, 2),
        // Funding accrual per position is not exposed by the node; the
        // cumulative index is, but the amount charged is not.
        funding_paid: "0.00".into(),
        opened_at: None,
    })
}

/// `GET /account/fills`.
pub async fn fills(
    State(state): State<Arc<AppState>>,
    caller: Caller,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<FillsResponse>> {
    let catalog = state.markets.snapshot();
    let symbol = query.symbol.as_deref().filter(|s| !s.is_empty());
    if let Some(symbol) = symbol {
        resolve_market(&catalog, symbol)?;
    }
    let limit = state.clamp_limit(query.limit, 100) as usize;

    let fills = state
        .tracker
        .fills_for_user(&caller.user.user_id, symbol, limit)
        .into_iter()
        .map(|fill| render_fill(&catalog, &fill))
        .collect();

    Ok(Json(FillsResponse { fills }))
}

/// `GET /account/transfers`.
pub async fn transfers(
    State(state): State<Arc<AppState>>,
    caller: Caller,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<TransfersResponse>> {
    let limit = state.clamp_limit(query.limit, 100);
    let entries = state
        .db
        .ledger()
        .for_user(&caller.user.user_id, None, limit)
        .await?;
    Ok(Json(TransfersResponse {
        entries: entries
            .into_iter()
            .map(|entry| LedgerEntry {
                id: entry.entry_id,
                kind: entry.kind,
                asset: entry.asset,
                amount: format_port_decimal(entry.amount, 2),
                balance_after: format_port_decimal(entry.balance_after, 2),
                memo: entry.memo,
                at: entry.at.to_rfc3339(),
                tx_hash: entry.tx_hash,
            })
            .collect(),
    }))
}

/// Render one derived fill as the contract's fill object.
///
/// Shared with the `account` channel's `fill` frame. The socket used to build
/// its own payload and omit `realized_pnl`, which the browser requires, so
/// every fill notification was dropped (§16 of the flow review).
pub fn render_fill(catalog: &crate::markets::Catalog, fill: &FillRecord) -> Fill {
    let (price_decimals, qty_decimals, price_display, size_display) =
        match (catalog.entry(&fill.symbol), catalog.meta(&fill.symbol)) {
            (Some(entry), Some(meta)) => (
                entry.scales.price_decimals,
                entry.scales.qty_decimals,
                meta.price_decimals,
                meta.size_decimals,
            ),
            _ => (8, 8, 2, 4),
        };
    Fill {
        fill_id: fill.fill_id.clone(),
        order_id: fill.order_id.clone(),
        symbol: fill.symbol.clone(),
        side: crate::orders::side_str(fill.side).to_string(),
        price: format_scaled(fill.price, price_decimals, price_display),
        qty: format_scaled(fill.qty, qty_decimals, size_display),
        fee: format_scaled(fill.fee, 6, 2),
        fee_asset: fill.fee_asset.clone(),
        role: fill.role.to_string(),
        realized_pnl: format_scaled_signed(fill.realized_pnl, 6, 2),
        at: fill.at.to_rfc3339(),
        height: fill.height,
    }
}

/// Render one tracked order.
pub fn render_order(state: &AppState, record: &OrderRecord) -> Order {
    let catalog = state.markets.snapshot();
    let (price_decimals, qty_decimals, price_display, size_display) =
        match (catalog.entry(&record.symbol), catalog.meta(&record.symbol)) {
            (Some(entry), Some(meta)) => (
                entry.scales.price_decimals,
                entry.scales.qty_decimals,
                meta.price_decimals,
                meta.size_decimals,
            ),
            _ => (8, 8, 2, 4),
        };
    Order {
        order_id: record.public_id(),
        client_order_id: record.client_order_id.clone(),
        symbol: record.symbol.clone(),
        side: crate::orders::side_str(record.side).to_string(),
        order_type: record.order_type.to_string(),
        price: format_scaled(record.price, price_decimals, price_display),
        qty: format_scaled(record.qty, qty_decimals, size_display),
        filled_qty: format_scaled(record.filled_qty, qty_decimals, size_display),
        remaining_qty: format_scaled(record.remaining_qty(), qty_decimals, size_display),
        avg_fill_price: record
            .avg_fill_price
            .map(|p| format_scaled(p, price_decimals, price_display)),
        status: record.status.as_str().to_string(),
        tif: crate::orders::tif_str(record.tif).to_string(),
        post_only: record.post_only,
        reduce_only: record.reduce_only,
        created_at: record.created_at.to_rfc3339(),
        updated_at: record.updated_at.to_rfc3339(),
    }
}

/// Margin committed across every open position.
async fn used_margin_of(
    state: &AppState,
    address: cb_types::Address,
    positions: &[PositionSnapshot],
) -> i128 {
    let catalog = state.markets.snapshot();
    let mut used: i128 = 0;
    for position in positions.iter().filter(|p| p.qty > 0) {
        if position.margin_mode == 1 {
            used += position.isolated_margin as i128;
            continue;
        }
        let Some(entry) = catalog.symbols.by_market(MarketRef::Deriv(position.market)) else {
            continue;
        };
        let Some(meta) = catalog.meta(&entry.symbol) else {
            continue;
        };
        let mark = state
            .node
            .mark_price(position.market)
            .await
            .map(|m| m.px_q8)
            .unwrap_or(position.entry_px_q8);
        let notional = position.qty.saturating_mul(mark) / 10u128.pow(10);
        let bps = meta.initial_margin_bps.unwrap_or(0) as u128;
        used += (notional.saturating_mul(bps) / 10_000) as i128;
    }
    let _ = address;
    used
}

/// Realised PnL over a window, from the off-chain ledger.
async fn realized_pnl_since(state: &AppState, user_id: &str, window: Duration) -> i128 {
    let now = state.clock.now();
    let kinds = vec!["realized_pnl".to_string(), "funding".to_string()];
    match state
        .db
        .ledger()
        .for_user(user_id, Some(&kinds), 1_000)
        .await
    {
        Ok(entries) => entries
            .into_iter()
            .filter(|entry| entry.at >= now - window)
            .map(|entry| {
                (entry.amount * rust_decimal::Decimal::from(1_000_000))
                    .round()
                    .try_into()
                    .unwrap_or(0i128)
            })
            .sum(),
        Err(_) => 0,
    }
}

/// How close an account is to liquidation, as the contract's four bands.
///
/// Ratios rather than absolutes, so the bands mean the same thing to a
/// hundred-dollar account and a hundred-thousand-dollar one.
fn liquidation_risk(used_margin: i128, equity: i128) -> &'static str {
    if equity <= 0 {
        return if used_margin > 0 { "critical" } else { "low" };
    }
    let ratio = used_margin.saturating_mul(100) / equity;
    match ratio {
        r if r >= 90 => "critical",
        r if r >= 70 => "high",
        r if r >= 40 => "medium",
        _ => "low",
    }
}

/// Estimate the price at which a position is liquidated.
///
/// The engine's liquidation check runs on total account equity against a
/// maintenance requirement, so a per-position price is necessarily an estimate:
/// it assumes the position's own margin is all that backs it. It is what a
/// trading UI shows, and it is labelled an estimate wherever it appears.
fn liquidation_price(
    position: &PositionSnapshot,
    meta: &cb_ports::MarketMeta,
    margin: u128,
) -> u128 {
    if position.qty == 0 {
        return 0;
    }
    let maintenance_bps = meta.maintenance_margin_bps.unwrap_or(0) as u128;
    // Margin, expressed back in the price scale: how far the mark may move
    // before the margin is exhausted.
    let buffer_q8 = margin.saturating_mul(10u128.pow(10)) / position.qty;
    let maintenance_q8 = position.entry_px_q8.saturating_mul(maintenance_bps) / 10_000;
    match position.side {
        Side::Buy => position
            .entry_px_q8
            .saturating_sub(buffer_q8)
            .saturating_add(maintenance_q8),
        Side::Sell => position
            .entry_px_q8
            .saturating_add(buffer_q8)
            .saturating_sub(maintenance_q8),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_risk_bands_are_ratios_not_absolutes() {
        assert_eq!(liquidation_risk(0, 100_000), "low");
        assert_eq!(liquidation_risk(50_000, 100_000), "medium");
        assert_eq!(liquidation_risk(80_000, 100_000), "high");
        assert_eq!(liquidation_risk(95_000, 100_000), "critical");
        // The same ratio on a much larger account reads the same.
        assert_eq!(liquidation_risk(95_000_000, 100_000_000), "critical");
    }

    #[test]
    fn a_wiped_out_account_with_margin_committed_is_critical() {
        assert_eq!(liquidation_risk(1, 0), "critical");
        assert_eq!(liquidation_risk(0, 0), "low", "nothing at stake");
    }

    #[test]
    fn a_long_liquidates_below_its_entry_and_a_short_above() {
        let meta = cb_ports::MarketMeta {
            symbol: "BTC-PERP".into(),
            kind: "perp".into(),
            asset_class: "crypto".into(),
            base: "BTC".into(),
            quote: "USD".into(),
            display_name: "Bitcoin Perpetual".into(),
            status: "trading".into(),
            tick_size: rust_decimal::Decimal::ONE,
            lot_size: rust_decimal::Decimal::ONE,
            min_notional: rust_decimal::Decimal::ONE,
            price_decimals: 2,
            size_decimals: 4,
            max_leverage: Some(rust_decimal::Decimal::from(50)),
            initial_margin_bps: Some(200),
            maintenance_margin_bps: Some(100),
            funding_interval_seconds: Some(3600),
            expiry: None,
            maker_fee_bps: 0,
            taker_fee_bps: 0,
        };
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
        let margin = 1_260_000_000; // 2% of $63,000
        let price = liquidation_price(&long, &meta, margin);
        assert!(
            price < long.entry_px_q8,
            "a long liquidates on the way down"
        );

        let short = PositionSnapshot {
            side: Side::Sell,
            ..long
        };
        assert!(
            liquidation_price(&short, &meta, margin) > short.entry_px_q8,
            "a short liquidates on the way up"
        );
    }

    #[test]
    fn a_flat_position_has_no_liquidation_price() {
        let meta = cb_ports::MarketMeta {
            symbol: "BTC-PERP".into(),
            kind: "perp".into(),
            asset_class: "crypto".into(),
            base: "BTC".into(),
            quote: "USD".into(),
            display_name: "x".into(),
            status: "trading".into(),
            tick_size: rust_decimal::Decimal::ONE,
            lot_size: rust_decimal::Decimal::ONE,
            min_notional: rust_decimal::Decimal::ONE,
            price_decimals: 2,
            size_decimals: 4,
            max_leverage: None,
            initial_margin_bps: None,
            maintenance_margin_bps: None,
            funding_interval_seconds: None,
            expiry: None,
            maker_fee_bps: 0,
            taker_fee_bps: 0,
        };
        let flat = PositionSnapshot {
            market: cb_types::DerivId::perp(0),
            side: Side::Buy,
            qty: 0,
            entry_px_q8: 1,
            margin_mode: 0,
            isolated_margin: 0,
            entry_funding_index: 0,
            last_funding_height: 0,
        };
        assert_eq!(liquidation_price(&flat, &meta, 0), 0);
    }
}
