//! The endpoints that move money.
//!
//! ## Placing an order is three steps, and the order of them matters
//!
//! 1. **Reserve the `client_order_id`.** Before anything is signed. Two
//!    simultaneous requests carrying the same id must place one order, and a
//!    lookup-then-submit would let both through the gap.
//! 2. **Subscribe to the node's event stream.** Before submitting, not after.
//!    Blocks arrive in milliseconds; a subscription taken afterwards can miss
//!    the very block being waited for.
//! 3. **Submit, then wait, bounded.** If the block carrying the order arrives
//!    in time, and the chain's nonce says the executor applied it, the reply
//!    is `open` and carries the real on-chain order id. If the block arrives
//!    and the nonce says the executor refused it, the reply is `rejected` —
//!    with no reason, because the chain publishes none (ADR 0008). If nothing
//!    arrives in time the reply is `pending` and carries the transaction
//!    hash, which `DELETE /orders/{id}` also accepts. A gateway that waited
//!    indefinitely would turn a slow block into a hung browser.

use super::dto::*;
use super::Json;
use super::{format_hash, resolve_market, Caller};
use crate::decimal::{format_scaled, parse_scaled};
use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::node::{NodeClient, NodeEvent};
use crate::orders::{self, OrderInput, ValidationContext};
use crate::state::AppState;
use crate::symbols::{InstrumentKind, MarketRef};
use crate::tracker::{CancelTarget, OrderRecord, OrderStatus, Reserved};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use cb_tx::TxBody;
use cb_types::{Side, H256};
use std::sync::Arc;

/// `POST /orders` → `201`.
pub async fn place_order(
    State(state): State<Arc<AppState>>,
    caller: Caller,
    Json(request): Json<PlaceOrderRequest>,
) -> ApiResult<(StatusCode, Json<PlaceOrderResponse>)> {
    let now = state.clock.now();
    let address = caller.address()?;
    let catalog = state.markets.snapshot();
    let (entry, meta) = resolve_market(&catalog, &request.symbol)?;

    // The contract carries `leverage` and `margin_mode` on the order, so they
    // are accepted. This chain carries them on the *position* — leverage is a
    // consequence of the margin backing it, and there is no per-order field to
    // put them in — so they are noted and have no effect here. Rejecting them
    // would fail every order a contract-conformant client sends; on a spot
    // market, where they are meaningless, they are still an error.
    if request.leverage.is_some() || request.margin_mode.is_some() {
        if !matches!(entry.kind, InstrumentKind::Perp | InstrumentKind::Future) {
            return Err(ApiError::validation(
                "leverage and margin_mode apply to derivative markets only",
            ));
        }
        if let Some(mode) = request.margin_mode.as_deref() {
            if !["cross", "isolated"].contains(&mode) {
                return Err(ApiError::validation(
                    "margin_mode must be `cross` or `isolated`",
                ));
            }
        }
        tracing::debug!(
            target: "cb_gateway::trading",
            symbol = %entry.symbol,
            leverage = ?request.leverage,
            margin_mode = ?request.margin_mode,
            "order carried leverage or margin mode; this chain sets both on the position"
        );
    }

    // Everything validation needs, read before the reservation is taken so a
    // rejected order does not hold a key while the node is consulted.
    let book = super::market::fetch_book(&state, entry, state.settings.max_depth).await?;
    let (position, mark_is_fresh, mark_px_q8, available_margin, spot_funds) = match entry.market {
        MarketRef::Deriv(id) => {
            let positions = state.node.positions(address).await.unwrap_or_default();
            let position = positions.into_iter().find(|p| p.market == id && p.qty > 0);
            // A market the oracle has never priced has no mark at all, and the
            // node says so by answering "not found". That is the ordinary state
            // of a freshly started chain, not an internal failure: propagating
            // it turned an order into `500 INTERNAL`, where the contract
            // documents `422 ORACLE_STALE` and the client has a message for it.
            let mark = state.node.mark_price(id).await.ok();
            let mark_is_fresh =
                mark.is_some_and(|mark| state.mark_is_fresh(mark.computed_at_height));
            // Only a fresh mark prices the band pre-check; a stale one is
            // reported as `ORACLE_STALE`, not as a band against old bounds.
            let mark_px_q8 = mark
                .filter(|mark| state.mark_is_fresh(mark.computed_at_height))
                .map(|mark| mark.px_q8);
            let margin = state.node.margin_account(address).await?;
            let available = (margin.collateral as i128 + margin.unrealized_pnl).max(0) as u128;
            (position, mark_is_fresh, mark_px_q8, Some(available), None)
        }
        MarketRef::Spot(_) => {
            // What the order will actually be paid for out of, read before it
            // is validated so an unaffordable order is refused rather than
            // submitted and left pending forever.
            let account = state.node.account(address).await?;
            (
                None,
                true,
                None,
                None,
                Some(orders::SpotFunds {
                    base_available: account.available(entry.base),
                    quote_available: account.available(entry.quote),
                }),
            )
        }
    };

    let input = OrderInput {
        symbol: entry.symbol.clone(),
        side: request.side.clone(),
        order_type: request.order_type.clone(),
        price: request.price.clone(),
        qty: request.qty.clone(),
        tif: request.tif.clone(),
        post_only: request.post_only,
        reduce_only: request.reduce_only,
        client_order_id: request.client_order_id.clone(),
    };
    let validated = orders::validate(
        &input,
        &ValidationContext {
            entry,
            meta,
            book: &book,
            position: position.as_ref(),
            mark_is_fresh,
            mark_px_q8,
            available_margin,
            spot_funds,
        },
    )?;

    // Step 1: the reservation, before anything is signed.
    let reservation = match state
        .tracker
        .reserve(
            &caller.user.user_id,
            validated.client_order_id.as_deref(),
            now,
        )
        .await
    {
        Reserved::Existing(existing) => {
            // The contract: the same id inside 24 hours returns the original
            // order rather than placing a second one.
            return Ok((
                StatusCode::CREATED,
                Json(PlaceOrderResponse {
                    order_id: existing.public_id(),
                    client_order_id: existing.client_order_id.clone(),
                    status: existing.status.as_str().to_string(),
                    tx_hash: format_hash(&existing.tx_hash),
                    accepted_at: existing.created_at.to_rfc3339(),
                    immediate_fills: Vec::new(),
                }),
            ));
        }
        Reserved::Fresh(reservation) => reservation,
    };

    // Step 2: listen before submitting, or risk missing the block, and record
    // the account as it is *now* — before this order can fill — so the watcher
    // has a baseline to difference the next block against. Without it the
    // block carrying this order is the first one the account is watched in,
    // and the fill it may produce is indistinguishable from the account simply
    // coming into view (§15 of the flow review).
    let events = state.node.subscribe();
    state
        .prime_account_mirror(&caller.user.user_id, address)
        .await;

    let client_order_id =
        orders::client_order_id_hash(&caller.user.user_id, validated.client_order_id.as_deref());
    let body = orders::to_tx_body(&validated, client_order_id)?;
    let key = caller.trading_key(&state)?;

    let (tx_hash, nonce) = match state.submitter.submit_with_nonce(&key, address, body).await {
        Ok(submitted) => submitted,
        Err(e) => {
            // The key is free again; a failed submission must not lock out a
            // retry with the same id for a day.
            reservation.release();
            return Err(e);
        }
    };

    let record = OrderRecord {
        order_id: None,
        tx_hash,
        user_id: caller.user.user_id.clone(),
        address,
        symbol: entry.symbol.clone(),
        market: entry.market,
        side: validated.side,
        order_type: validated.order_type.as_str(),
        price: validated.price,
        qty: validated.qty,
        filled_qty: 0,
        avg_fill_price: None,
        status: OrderStatus::Pending,
        tif: validated.tif,
        post_only: validated.post_only,
        reduce_only: validated.reduce_only,
        client_order_id: validated.client_order_id.clone(),
        nonce,
        included_at: None,
        tx_index: None,
        created_at: now,
        updated_at: now,
    };
    state.tracker.record_placement(record);
    reservation.fulfil(tx_hash, now);

    // Step 3: wait, but only for as long as the operator allows.
    let confirmed = await_inclusion(&state, events, tx_hash).await;
    let record = state
        .tracker
        .find(&format_hash(&tx_hash))
        .ok_or_else(|| ApiError::internal("the order the gateway just recorded is missing"))?;

    tracing::info!(
        target: "cb_gateway::trading",
        user = %caller.user.user_id,
        symbol = %entry.symbol,
        order = %record.public_id(),
        confirmed,
        "order submitted"
    );

    Ok((
        StatusCode::CREATED,
        Json(PlaceOrderResponse {
            order_id: record.public_id(),
            client_order_id: record.client_order_id.clone(),
            status: record.status.as_str().to_string(),
            tx_hash: format_hash(&tx_hash),
            accepted_at: now.to_rfc3339(),
            immediate_fills: immediate_fills(&state, &record),
        }),
    ))
}

/// `DELETE /orders/{order_id}`.
pub async fn cancel_order(
    State(state): State<Arc<AppState>>,
    caller: Caller,
    Path(order_id): Path<String>,
) -> ApiResult<Json<CancelOrderResponse>> {
    let record = state
        .tracker
        .find(&order_id)
        .ok_or_else(|| ApiError::not_found(format!("no order `{order_id}`")))?;
    // The tracker knows who placed it, which is what makes `403` meaningful
    // here rather than a `404` that leaks whether the order exists.
    if record.user_id != caller.user.user_id {
        return Err(ApiError::forbidden("that order belongs to another account"));
    }
    if !record.status.is_live() {
        return Err(ApiError::validation(format!(
            "order is already {}",
            record.status.as_str()
        )));
    }
    let Some(chain_id) = record.order_id else {
        return Err(ApiError::validation(
            "this order has not reached a block yet and has no on-chain id to cancel; retry shortly",
        ));
    };

    let key = caller.trading_key(&state)?;
    let body = orders::cancel_tx_body(record.market, chain_id);
    let address = caller.address()?;
    let (tx_hash, nonce) = state
        .submitter
        .submit_with_nonce(&key, address, body)
        .await?;
    // The order is moved when the chain answers for this cancel, not now: the
    // reply below says `cancelled` on mempool acceptance, as the contract has
    // always had it, and a cancel the executor refuses is reported back on
    // the account channel with the order's real status.
    state
        .tracker
        .record_cancel(tx_hash, address, nonce, CancelTarget::Order(chain_id));

    Ok(Json(CancelOrderResponse {
        order_id: record.public_id(),
        status: "cancelled".into(),
        tx_hash: format_hash(&tx_hash),
    }))
}

/// `POST /orders/cancel-all`.
pub async fn cancel_all(
    State(state): State<Arc<AppState>>,
    caller: Caller,
    body: Option<Json<CancelAllRequest>>,
) -> ApiResult<Json<CancelAllResponse>> {
    let request = body.map(|Json(r)| r).unwrap_or_default();
    let catalog = state.markets.snapshot();
    if let Some(symbol) = &request.symbol {
        resolve_market(&catalog, symbol)?;
    }

    let live = state
        .tracker
        .live_orders_for_user(&caller.user.user_id, request.symbol.as_deref());
    if live.is_empty() {
        return Ok(Json(CancelAllResponse {
            cancelled: 0,
            tx_hash: String::new(),
        }));
    }

    let key = caller.trading_key(&state)?;
    let address = caller.address()?;
    let mut cancelled = 0;
    let mut last_hash = String::new();

    // Spot has a batch-cancel transaction; derivatives do not, so those are
    // cancelled one at a time. Sending one transaction where the chain offers
    // one is worth the branch: it is one nonce instead of twenty.
    let mut batched_spot_markets = std::collections::BTreeSet::new();
    for order in &live {
        if let MarketRef::Spot(market) = order.market {
            batched_spot_markets.insert(market);
        }
    }
    for market in batched_spot_markets {
        let (hash, nonce) = state
            .submitter
            .submit_with_nonce(&key, address, TxBody::BatchCancel { market })
            .await?;
        state.tracker.record_cancel(
            hash,
            address,
            nonce,
            CancelTarget::Market(MarketRef::Spot(market)),
        );
        last_hash = format_hash(&hash);
        cancelled += live
            .iter()
            .filter(|o| o.market == MarketRef::Spot(market))
            .count() as u32;
    }
    for order in live
        .iter()
        .filter(|o| matches!(o.market, MarketRef::Deriv(_)))
    {
        let Some(chain_id) = order.order_id else {
            // Not in a block yet, so there is nothing to name in a cancel.
            continue;
        };
        let (hash, nonce) = state
            .submitter
            .submit_with_nonce(
                &key,
                address,
                orders::cancel_tx_body(order.market, chain_id),
            )
            .await?;
        state
            .tracker
            .record_cancel(hash, address, nonce, CancelTarget::Order(chain_id));
        last_hash = format_hash(&hash);
        cancelled += 1;
    }

    Ok(Json(CancelAllResponse {
        cancelled,
        tx_hash: last_hash,
    }))
}

/// `POST /positions/close`.
pub async fn close_position(
    State(state): State<Arc<AppState>>,
    caller: Caller,
    Json(request): Json<ClosePositionRequest>,
) -> ApiResult<Json<ClosePositionResponse>> {
    let address = caller.address()?;
    let catalog = state.markets.snapshot();
    let (entry, meta) = resolve_market(&catalog, &request.symbol)?;
    let MarketRef::Deriv(market) = entry.market else {
        return Err(ApiError::validation(
            "only derivative positions can be closed; sell the asset on a spot market",
        ));
    };

    let position = state
        .node
        .positions(address)
        .await?
        .into_iter()
        .find(|p| p.market == market && p.qty > 0)
        .ok_or_else(|| ApiError::not_found(format!("no open position on {}", entry.symbol)))?;

    let qty = match request.qty.as_deref().filter(|q| !q.is_empty()) {
        Some(text) => {
            let qty = parse_scaled(text, entry.scales.qty_decimals)
                .map_err(|e| ApiError::validation(format!("qty: {e}")))?;
            if qty == 0 {
                return Err(ApiError::validation("qty must be greater than zero"));
            }
            if qty > position.qty {
                return Err(ApiError::new(
                    ErrorCode::ReduceOnlyWouldIncrease,
                    "cannot close more than the position holds",
                ));
            }
            qty
        }
        // The chain's sentinel for "close it all".
        None => 0,
    };

    let mark = state.node.mark_price(market).await?;
    if !state.mark_is_fresh(mark.computed_at_height) {
        return Err(ApiError::new(
            ErrorCode::OracleStale,
            format!("{} has no fresh mark price", entry.symbol),
        ));
    }

    let key = caller.trading_key(&state)?;
    let tx_hash = state
        .submitter
        .submit(
            &key,
            address,
            TxBody::ClosePosition {
                market,
                qty,
                max_slippage_bps: orders::MARKET_ORDER_SLIPPAGE_BPS as u16,
            },
        )
        .await?;

    let closed = if qty == 0 { position.qty } else { qty };
    // Realised PnL is settled by the engine when the block applies, so what
    // the gateway can state here is the marked value of the close, not the
    // final figure. It is reported as such and corrected by the account view.
    let pnl_per_unit = mark.px_q8 as i128 - position.entry_px_q8 as i128;
    let realized = match position.side {
        Side::Buy => pnl_per_unit,
        Side::Sell => -pnl_per_unit,
    }
    .saturating_mul(closed as i128)
        / 10i128.pow(10);

    Ok(Json(ClosePositionResponse {
        tx_hash: format_hash(&tx_hash),
        closed_qty: format_scaled(closed, entry.scales.qty_decimals, meta.size_decimals),
        avg_price: format_scaled(mark.px_q8, entry.scales.price_decimals, meta.price_decimals),
        realized_pnl: crate::decimal::format_scaled_signed(realized, 6, 2),
    }))
}

/// `POST /positions/margin`.
pub async fn adjust_margin(
    State(state): State<Arc<AppState>>,
    caller: Caller,
    Json(request): Json<AdjustMarginRequest>,
) -> ApiResult<Json<TxResponse>> {
    let catalog = state.markets.snapshot();
    let (entry, _meta) = resolve_market(&catalog, &request.symbol)?;
    let MarketRef::Deriv(market) = entry.market else {
        return Err(ApiError::validation(
            "margin applies to derivative positions only",
        ));
    };
    let is_topup = match request.action.as_str() {
        "add" => true,
        "remove" => false,
        other => {
            return Err(ApiError::validation(format!(
                "action must be `add` or `remove`, not `{other}`"
            )))
        }
    };
    // Margin moves in USD, whatever the market's price scale is.
    let amount = parse_scaled(&request.amount, 6)
        .map_err(|e| ApiError::validation(format!("amount: {e}")))?;
    if amount == 0 {
        return Err(ApiError::validation("amount must be greater than zero"));
    }

    // The request is well formed and this chain still cannot do it.
    //
    // `TxBody::AdjustMargin` is the one transaction that would move value
    // between a spot balance and margin collateral, and the executor answers it
    // with `ExecError::Unsupported`. Submitting it anyway would fail at block
    // build — which today takes the whole block down — and the caller used to
    // be told "no open position on BTC-PERP" instead, which sent them looking
    // for a position rather than at the real limit (§9 of the flow review).
    //
    // The consequence is worth stating plainly because it shapes the product:
    // a faucet grant credits the **spot** ledger, margin collateral is a
    // different one, and nothing bridges them. Every account created through
    // the gateway therefore has zero collateral, and only the accounts genesis
    // pre-funded can trade a perpetual or a future at all. The fix belongs in
    // the executor, not here; until it lands this endpoint says so.
    let _ = (market, is_topup, amount, caller.address()?);
    Err(ApiError::new(
        ErrorCode::NotSupported,
        format!(
            "margin cannot be moved on {}: this chain has no implemented transaction that \
             transfers value between a spot balance and margin collateral, so collateral \
             exists only where genesis created it. A faucet grant credits the spot ledger and \
             does not become margin.",
            entry.symbol
        ),
    ))
}

/// `POST /positions/leverage`.
///
/// The chain carries leverage implicitly: a position's leverage is its
/// notional over its margin, and the market's `max_leverage` caps it. There is
/// no transaction that sets a leverage number, so this endpoint validates the
/// request against the market and reports what it would mean, rather than
/// pretending to have changed something.
pub async fn set_leverage(
    State(state): State<Arc<AppState>>,
    caller: Caller,
    Json(request): Json<SetLeverageRequest>,
) -> ApiResult<Json<TxResponse>> {
    let catalog = state.markets.snapshot();
    let (entry, meta) = resolve_market(&catalog, &request.symbol)?;
    if !matches!(entry.kind, InstrumentKind::Perp | InstrumentKind::Future) {
        return Err(ApiError::validation(
            "leverage applies to derivative markets only",
        ));
    }
    let requested = parse_scaled(&request.leverage, 2)
        .map_err(|e| ApiError::validation(format!("leverage: {e}")))?;
    if requested == 0 {
        return Err(ApiError::validation("leverage must be greater than zero"));
    }
    let max = meta
        .max_leverage
        .map(|v| (v * rust_decimal::Decimal::from(100)).trunc())
        .and_then(|v| u128::try_from(v).ok())
        .unwrap_or(0);
    if max > 0 && requested > max {
        return Err(ApiError::new(
            ErrorCode::InsufficientMargin,
            format!(
                "{} allows at most {}x leverage",
                entry.symbol,
                meta.max_leverage.unwrap_or_default()
            ),
        ));
    }
    if let Some(mode) = request.margin_mode.as_deref() {
        if !["cross", "isolated"].contains(&mode) {
            return Err(ApiError::validation(
                "margin_mode must be `cross` or `isolated`",
            ));
        }
    }
    let _ = caller;
    Ok(Json(TxResponse {
        // No transaction: leverage is a consequence of margin on this chain.
        // Saying so is better than returning a hash that names nothing.
        tx_hash: String::new(),
        at: state.clock.now().to_rfc3339(),
    }))
}

/// `POST /transfers`.
pub async fn transfer(
    State(state): State<Arc<AppState>>,
    caller: Caller,
    Json(request): Json<TransferRequest>,
) -> ApiResult<Json<TxResponse>> {
    let to = super::parse_address(&request.to)
        .ok_or_else(|| ApiError::validation("`to` is not a 20-byte address"))?;
    let asset = crate::symbols::parse_asset(&request.asset)
        .ok_or_else(|| ApiError::validation(format!("unknown asset `{}`", request.asset)))?;
    let amount = parse_scaled(&request.amount, asset.decimals() as u32)
        .map_err(|e| ApiError::validation(format!("amount: {e}")))?;
    if amount == 0 {
        return Err(ApiError::validation("amount must be greater than zero"));
    }

    let address = caller.address()?;
    if to == address {
        return Err(ApiError::validation("cannot transfer to your own address"));
    }

    // Checked here so the caller gets `INSUFFICIENT_BALANCE` with numbers in
    // it, rather than the sequencer's bare rejection.
    let account = state.node.account(address).await?;
    // Available, not the balance: an amount reserved against a resting order
    // cannot be transferred, and the chain will refuse it.
    let free = account.available(asset);
    if free < amount {
        return Err(ApiError::new(
            ErrorCode::InsufficientBalance,
            format!(
                "need {} {}, have {}",
                super::market::asset_amount(asset, amount),
                asset.symbol(),
                super::market::asset_amount(asset, free)
            ),
        )
        .with_details(serde_json::json!({
            "required": super::market::asset_amount(asset, amount),
            "available": super::market::asset_amount(asset, free),
        })));
    }

    // The baseline the watcher will difference the next block against, taken
    // before the transfer rather than after it.
    state
        .prime_account_mirror(&caller.user.user_id, address)
        .await;

    let key = caller.trading_key(&state)?;
    let tx_hash = state
        .submitter
        .submit(&key, address, TxBody::Transfer { to, asset, amount })
        .await?;
    let now = state.clock.now();

    // No ledger entry is written here, deliberately.
    //
    // `submit` returns on *mempool acceptance*, which is not the chain having
    // applied anything: the transaction can still fail at block build, and
    // before this change a transfer that never landed left a permanent
    // `transfer_out` row carrying a balance the gateway had computed rather
    // than observed (§20 of the flow review). `/account/transfers` is what a
    // person reads to answer "where did my money go", so it is written by
    // `crate::watcher::record_chain_ledger` when the block carrying this
    // transaction arrives — with the real height and the balance the chain
    // reports — and the recipient gets their `transfer_in` from the same place.
    // The memo the sender typed is held until the block arrives, because the
    // chain has no field to carry it and the entry is written from the chain.
    if let Some(memo) = request.memo.as_deref().filter(|m| !m.trim().is_empty()) {
        state.remember_transfer_memo(tx_hash, memo, now);
    }

    Ok(Json(TxResponse {
        tx_hash: format_hash(&tx_hash),
        at: now.to_rfc3339(),
    }))
}

/// Wait, bounded, for the block carrying `tx_hash`.
///
/// Returns whether it arrived. The tracker is updated by the watcher as blocks
/// come in, so this only has to notice; it does not have to do the folding.
/// When it does notice, it also reads the verdict — one `getAccount` for the
/// sender's nonce — so the reply can say `open` or `rejected` rather than
/// `pending` for an order the chain has already answered for. That read is
/// not bounded by the confirmation deadline; it is one call against a node
/// that just delivered a block.
async fn await_inclusion(
    state: &AppState,
    mut events: tokio::sync::broadcast::Receiver<NodeEvent>,
    tx_hash: H256,
) -> bool {
    let deadline = tokio::time::Instant::now() + state.settings.order_confirm;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Ok(NodeEvent::Block(block))) => {
                if block.txs.iter().any(|tx| tx.hash == tx_hash) {
                    // The watcher folds blocks into the tracker; make sure the
                    // fold has happened before the reply is rendered. Whichever
                    // of the two gets there first publishes the resulting
                    // `order_update`; the transitions are guarded, so the other
                    // one reports nothing and the client sees one frame.
                    let verdicts = crate::watcher::verdicts_for(state, &block).await;
                    for record in state
                        .tracker
                        .apply_block(&block, &verdicts, state.clock.now())
                    {
                        crate::watcher::publish_order_update(state, &record);
                    }
                    return true;
                }
            }
            // The sequencer dropped it: the order is answered now, as
            // `rejected`, instead of sitting `pending` for the client to
            // poll. The watcher does the same for anyone not waiting here.
            Ok(Ok(NodeEvent::Refused(refused))) if refused.iter().any(|r| r.hash == tx_hash) => {
                for record in state.tracker.mark_dead(&[tx_hash], state.clock.now()) {
                    crate::watcher::publish_order_update(state, &record);
                }
                return true;
            }
            Ok(Ok(_)) => continue,
            // Lagged: the gateway is behind, but the order was still
            // submitted. Report it as pending rather than a failure.
            Ok(Err(_)) => return false,
            Err(_) => return false,
        }
    }
}

/// Fills the tracker already knows about for a just-placed order.
fn immediate_fills(state: &AppState, record: &OrderRecord) -> Vec<ImmediateFill> {
    let catalog = state.markets.snapshot();
    let (price_decimals, qty_decimals) = catalog
        .entry(&record.symbol)
        .map(|entry| (entry.scales.price_decimals, entry.scales.qty_decimals))
        .unwrap_or((8, 8));
    state
        .tracker
        .fills_for_user(&record.user_id, Some(&record.symbol), 32)
        .into_iter()
        .filter(|fill| fill.order_id == record.public_id())
        .map(|fill| ImmediateFill {
            price: format_scaled(fill.price, price_decimals, 2),
            qty: format_scaled(fill.qty, qty_decimals, 4),
            fee: format_scaled(fill.fee, 6, 2),
        })
        .collect()
}
