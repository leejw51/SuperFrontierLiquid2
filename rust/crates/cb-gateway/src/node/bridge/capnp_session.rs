//! The real session: Cap'n Proto over TCP, driven on the `LocalSet` thread.
//!
//! Everything in this file runs on the RPC thread. The capnp readers it
//! touches borrow from the RPC arena, so each response is copied into the
//! owned types from [`crate::node::types`] before it is handed back over the
//! oneshot channel. That copy is what makes the whole facade `Send`.
//!
//! One long-lived task runs alongside the request loop: **the block stream**,
//! which is the gateway's clock. Order inclusion, trade derivation, candles,
//! the chain channel and the leaderboard all hang off it. If it fails the
//! whole session is torn down and the supervisor reconnects, because a gateway
//! that is connected but no longer receiving blocks is worse than one that
//! knows it is disconnected.
//!
//! ## There is no trade stream
//!
//! The schema declares `streamMarket`, and the node answers it with
//! `streamMarket not implemented` (`cb_rpc::server`). The gateway therefore
//! does **not** open it: a session that did would die on its first block and
//! reconnect forever. Trades are derived above this layer, in
//! [`crate::marketdata`], from per-block book and last-trade changes. That is
//! recorded as a contract gap rather than worked around silently.

use super::{Call, Reply, SessionCtx};
use crate::node::types::*;
use cb_ports::PortError;
use cb_rpc::conv;
use cb_schema::rpc_capnp;
use cb_types::{Address, DerivId, MarketId};
use futures::future::LocalBoxFuture;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Shorthand for the bootstrap capability.
type Cap = rpc_capnp::super_frontier_liquid::Client;

/// How long the request loop waits before re-checking liveness flags.
const LIVENESS_TICK: Duration = Duration::from_millis(50);

/// Connects to a sequencer over TCP and speaks Cap'n Proto to it.
pub struct TcpConnector {
    addr: SocketAddr,
}

impl TcpConnector {
    /// A connector for the sequencer at `addr`.
    pub fn new(addr: SocketAddr) -> Self {
        Self { addr }
    }
}

impl super::Connector for TcpConnector {
    fn describe(&self) -> String {
        format!("capnp://{}", self.addr)
    }

    // The receiver is borrowed across an await only by this loop, on a
    // single-threaded runtime, so the borrow cannot conflict.
    #[allow(clippy::await_holding_refcell_ref)]
    fn run_session(&self, ctx: Rc<SessionCtx>) -> LocalBoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let client = cb_rpc::connect(self.addr)
                .await
                .map_err(|e| format!("connect to {}: {e}", self.addr))?;
            let cap = client.cap.clone();

            let info = rpc_chain_info(&cap)
                .await
                .map_err(|e| format!("chainInfo: {e}"))?;
            ctx.mark_connected(info.chain_id);
            let from = ctx.reconcile_start(&info);
            let _ = ctx.events.send(NodeEvent::Connected);
            tracing::info!(
                target: "cb_gateway::node",
                addr = %self.addr,
                chain_id = info.chain_id,
                head = info.head_height,
                from,
                "connected to sequencer"
            );

            // Stream tasks report their death here; any one of them ends the
            // session.
            let (dead_tx, mut dead_rx) = tokio::sync::mpsc::channel::<String>(8);

            {
                let cap = cap.clone();
                let ctx = Rc::clone(&ctx);
                let dead = dead_tx.clone();
                tokio::task::spawn_local(async move {
                    if let Err(e) = block_stream(cap, ctx).await {
                        let _ = dead.send(format!("block stream: {e}")).await;
                    }
                });
            }
            {
                let cap = cap.clone();
                let ctx = Rc::clone(&ctx);
                tokio::task::spawn_local(async move {
                    // Not fatal to the session. A validator answers
                    // `unimplemented`, because under BFT a refusal stays in
                    // the block and the watcher reads it from the mask; an
                    // older sequencer answers the same, and refusals are then
                    // inferred from nonces as they always were.
                    if let Err(e) = rejected_stream(cap, ctx).await {
                        tracing::info!(
                            target: "cb_gateway::node",
                            error = %e,
                            "refused-transaction stream ended; refusals will be inferred from nonces"
                        );
                    }
                });
            }
            drop(dead_tx);

            loop {
                if ctx.stopping.load(Ordering::SeqCst) {
                    return Ok(());
                }
                if let Ok(reason) = dead_rx.try_recv() {
                    return Err(reason);
                }
                // The borrow is confined to this block. Only the request loop
                // ever borrows the receiver, and the thread is single
                // threaded, so the borrow cannot conflict.
                let received = {
                    let mut guard = ctx.requests.borrow_mut();
                    tokio::time::timeout(LIVENESS_TICK, guard.recv()).await
                };
                match received {
                    // Handling is spawned rather than awaited so one slow RPC
                    // does not serialise everything queued behind it.
                    Ok(Some(request)) => {
                        let cap = cap.clone();
                        tokio::task::spawn_local(async move {
                            let reply = dispatch(&cap, request.call().clone()).await;
                            request.answer(reply);
                        });
                    }
                    // The gateway dropped the sender, so there will never be
                    // another request. Set the stop flag before returning:
                    // otherwise the supervisor reconnects into the same closed
                    // channel and spins forever.
                    Ok(None) => {
                        ctx.stopping.store(true, Ordering::SeqCst);
                        return Ok(());
                    }
                    Err(_) => continue,
                }
            }
        })
    }
}

/// Map one [`Call`] onto its RPC and copy the answer into owned data.
async fn dispatch(cap: &Cap, call: Call) -> Result<Reply, PortError> {
    let result = match call {
        Call::ChainInfo => rpc_chain_info(cap).await.map(Reply::ChainInfo),
        Call::Account(addr) => rpc_account(cap, addr).await.map(Reply::Account),
        Call::SubmitTx(tx) => rpc_submit(cap, &tx).await.map(Reply::Submit),
        Call::SpotBook(market, depth) => rpc_spot_book(cap, market, depth).await.map(Reply::Book),
        Call::DerivBook(market, depth) => rpc_deriv_book(cap, market, depth).await.map(Reply::Book),
        Call::Positions(addr) => rpc_positions(cap, addr).await.map(Reply::Positions),
        Call::MarginAccount(addr) => rpc_margin(cap, addr).await.map(Reply::Margin),
        Call::MarkPrice(market) => rpc_mark(cap, market).await.map(Reply::Mark),
        Call::OraclePrice(market) => rpc_oracle(cap, market).await.map(Reply::Oracle),
        Call::FundingIndex(market) => rpc_funding(cap, market).await.map(Reply::Funding),
        Call::PerpMarkets => rpc_perp_markets(cap).await.map(Reply::PerpMarkets),
        Call::FutureMarkets => rpc_future_markets(cap).await.map(Reply::FutureMarkets),
        Call::SpotMarkets => rpc_spot_markets(cap).await.map(Reply::SpotMarkets),
        Call::Block(height) => rpc_block(cap, height)
            .await
            .map(|b| Reply::Block(Box::new(b))),
    };
    result.map_err(classify)
}

/// Turn a capnp failure into a port error.
///
/// A disconnected or overloaded RPC system is [`PortError::Unavailable`], which
/// the HTTP layer renders as `503`; anything the server explicitly refused is
/// [`PortError::NotFound`] or [`PortError::Internal`]. The distinction matters
/// because the frontend retries one and not the other.
fn classify(error: capnp::Error) -> PortError {
    match error.kind {
        capnp::ErrorKind::Disconnected => PortError::Unavailable(error.to_string()),
        capnp::ErrorKind::Overloaded => PortError::RateLimited { retry_after: None },
        _ => {
            let text = error.to_string();
            if text.contains("not found") || text.contains("unknown") {
                PortError::NotFound(text)
            } else {
                PortError::Internal(text)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Individual RPCs
// ---------------------------------------------------------------------------

async fn rpc_chain_info(cap: &Cap) -> capnp::Result<ChainInfo> {
    let response = cap.chain_info_request().send().promise.await?;
    let info = response.get()?.get_info()?;
    Ok(ChainInfo {
        chain_id: info.get_chain_id(),
        denom: info.get_denom()?.to_str()?.to_string(),
        head_height: info.get_head_height(),
        oldest_block: info.get_oldest_block(),
    })
}

async fn rpc_account(cap: &Cap, address: Address) -> capnp::Result<AccountSnapshot> {
    let mut request = cap.get_account_request();
    conv::address_to_capnp(request.get().init_addr(), &address);
    let response = request.send().promise.await?;
    let state = response.get()?.get_state()?;

    let mut snapshot = AccountSnapshot {
        nonce: state.get_nonce(),
        ..Default::default()
    };
    let balances = state.get_balances()?;
    for i in 0..balances.len() {
        let entry = balances.get(i);
        let asset = conv::asset_from_capnp(entry.get_asset()?);
        snapshot
            .balances
            .insert(asset, conv::u128_from_capnp(entry.get_amount()?));
    }
    let locked = state.get_locked()?;
    for i in 0..locked.len() {
        let entry = locked.get(i);
        let asset = conv::asset_from_capnp(entry.get_asset()?);
        snapshot
            .locked
            .insert(asset, conv::u128_from_capnp(entry.get_amount()?));
    }
    Ok(snapshot)
}

async fn rpc_submit(cap: &Cap, tx: &cb_tx::SignedTx) -> capnp::Result<SubmitOutcome> {
    let mut request = cap.submit_tx_request();
    conv::signed_tx_to_capnp(request.get().init_tx(), tx);
    let response = request.send().promise.await?;
    let result = response.get()?.get_result()?;
    match result.which()? {
        rpc_capnp::submit_result::Ok(()) => Ok(SubmitOutcome::Accepted),
        rpc_capnp::submit_result::Rejected(rejection) => {
            let rejection = rejection?;
            let code = match rejection.get_code() {
                Ok(c) => rejection_code(c),
                Err(_) => SubmitRejection::Internal,
            };
            Ok(SubmitOutcome::Rejected {
                code,
                message: rejection.get_message()?.to_string()?,
            })
        }
    }
}

fn rejection_code(code: rpc_capnp::SubmitErrorCode) -> SubmitRejection {
    use rpc_capnp::SubmitErrorCode as E;
    match code {
        E::BadSignature => SubmitRejection::BadSignature,
        E::BadNonce => SubmitRejection::BadNonce,
        E::InsufficientBalance => SubmitRejection::InsufficientBalance,
        E::PostOnlyWouldCross => SubmitRejection::PostOnlyWouldCross,
        E::UnknownMarket => SubmitRejection::UnknownMarket,
        E::InvalidAmount => SubmitRejection::InvalidAmount,
        E::NotAuthorized => SubmitRejection::NotAuthorized,
        E::MempoolFull => SubmitRejection::MempoolFull,
        E::Internal => SubmitRejection::Internal,
    }
}

async fn rpc_spot_book(cap: &Cap, market: MarketId, depth: u32) -> capnp::Result<BookSnapshot> {
    let mut request = cap.get_order_book_request();
    {
        let mut params = request.get();
        params.set_market(conv::market_to_capnp(market));
        params.set_depth(depth);
    }
    let response = request.send().promise.await?;
    let snapshot = conv::spot_book_snapshot_from_capnp(response.get()?.get_snapshot()?)?;
    Ok(BookSnapshot {
        bids: snapshot
            .bids
            .iter()
            .map(|l| BookLevel {
                price: l.price,
                qty: l.qty,
            })
            .collect(),
        asks: snapshot
            .asks
            .iter()
            .map(|l| BookLevel {
                price: l.price,
                qty: l.qty,
            })
            .collect(),
        height: snapshot.as_of_height,
    })
}

async fn rpc_deriv_book(cap: &Cap, market: DerivId, depth: u32) -> capnp::Result<BookSnapshot> {
    let mut request = cap.get_perp_order_book_request();
    {
        let mut params = request.get();
        conv::deriv_id_to_capnp(params.reborrow().init_market(), &market);
        params.set_depth(depth);
    }
    let response = request.send().promise.await?;
    let snapshot = conv::perp_book_snapshot_from_capnp(response.get()?.get_snapshot()?)?;
    Ok(BookSnapshot {
        bids: snapshot
            .bids
            .iter()
            .map(|l| BookLevel {
                price: l.price_q8,
                qty: l.qty,
            })
            .collect(),
        asks: snapshot
            .asks
            .iter()
            .map(|l| BookLevel {
                price: l.price_q8,
                qty: l.qty,
            })
            .collect(),
        height: snapshot.as_of_height,
    })
}

async fn rpc_positions(cap: &Cap, address: Address) -> capnp::Result<Vec<PositionSnapshot>> {
    let mut request = cap.get_positions_request();
    conv::address_to_capnp(request.get().init_addr(), &address);
    let response = request.send().promise.await?;
    let list = response.get()?.get_positions()?;
    let mut out = Vec::with_capacity(list.len() as usize);
    for i in 0..list.len() {
        let position = conv::position_from_capnp(list.get(i))?;
        out.push(PositionSnapshot {
            market: position.market,
            side: position.side,
            qty: position.qty,
            entry_px_q8: position.entry_px_q8,
            margin_mode: position.margin_mode,
            isolated_margin: position.isolated_margin,
            entry_funding_index: position.entry_funding_index.to_i128(),
            last_funding_height: position.last_funding_height,
        });
    }
    Ok(out)
}

async fn rpc_margin(cap: &Cap, address: Address) -> capnp::Result<MarginSnapshot> {
    let mut request = cap.get_margin_account_request();
    conv::address_to_capnp(request.get().init_addr(), &address);
    let response = request.send().promise.await?;
    let margin = conv::margin_account_from_capnp(response.get()?.get_margin()?)?;
    Ok(MarginSnapshot {
        collateral: margin.collateral,
        unrealized_pnl: margin.unrealized_pnl.to_i128(),
        bad_debt: margin.bad_debt,
    })
}

async fn rpc_mark(cap: &Cap, market: DerivId) -> capnp::Result<MarkSnapshot> {
    let mut request = cap.get_mark_price_request();
    conv::deriv_id_to_capnp(request.get().init_market(), &market);
    let response = request.send().promise.await?;
    let mark = conv::mark_price_from_capnp(response.get()?.get_mark()?)?;
    Ok(MarkSnapshot {
        px_q8: mark.px_q8,
        computed_at_height: mark.computed_at_height,
        premium_ema_q9: mark.premium_ema_q9,
    })
}

async fn rpc_oracle(cap: &Cap, market: DerivId) -> capnp::Result<OracleSnapshot> {
    let mut request = cap.get_oracle_price_request();
    conv::deriv_id_to_capnp(request.get().init_market(), &market);
    let response = request.send().promise.await?;
    let oracle = conv::oracle_price_from_capnp(response.get()?.get_oracle()?)?;
    Ok(OracleSnapshot {
        px_q8: oracle.px_q8,
        source_height: oracle.source_height,
    })
}

async fn rpc_funding(cap: &Cap, market: DerivId) -> capnp::Result<FundingSnapshot> {
    let mut request = cap.get_funding_index_request();
    conv::deriv_id_to_capnp(request.get().init_market(), &market);
    let response = request.send().promise.await?;
    let index = conv::funding_index_from_capnp(response.get()?.get_index()?)?;
    Ok(FundingSnapshot {
        cumulative: index.cumulative.to_i128(),
        last_tick_height: index.last_tick_height,
        rate_q9: index.rate_q9,
    })
}

async fn rpc_perp_markets(cap: &Cap) -> capnp::Result<Vec<PerpMarketSnapshot>> {
    let response = cap.list_perp_markets_request().send().promise.await?;
    let list = response.get()?.get_markets()?;
    let mut out = Vec::with_capacity(list.len() as usize);
    for i in 0..list.len() {
        let reader = list.get(i);
        let reduce_only = conv::perp_market_paused_reduce_only_only_from_capnp(reader);
        let market = conv::perp_market_from_capnp(reader)?;
        out.push(PerpMarketSnapshot {
            id: market.id,
            underlying: market.underlying,
            tick_size: market.tick_size,
            lot_size: market.lot_size,
            max_leverage_x100: market.max_leverage_x100,
            initial_margin_bps: market.initial_margin_bps,
            maintenance_margin_bps: market.maintenance_margin_bps,
            taker_fee_bps: market.taker_fee_bps,
            maker_fee_bps: market.maker_fee_bps_signed,
            funding_interval_blocks: market.funding_interval_blocks,
            max_position_notional: market.max_position_notional,
            paused: market.paused,
            reduce_only_mode: reduce_only,
            last_trade_px_q8: market.last_trade_px_q8,
            last_trade_height: market.last_trade_height,
        });
    }
    Ok(out)
}

async fn rpc_spot_markets(cap: &Cap) -> capnp::Result<Vec<SpotMarketSnapshot>> {
    let response = cap.list_spot_markets_request().send().promise.await?;
    let list = response.get()?.get_markets()?;
    let mut out = Vec::with_capacity(list.len() as usize);
    for i in 0..list.len() {
        let market = conv::spot_market_from_capnp(list.get(i))?;
        out.push(SpotMarketSnapshot {
            id: market.id,
            base: market.base,
            quote: market.quote,
            tick_size: market.tick_size,
            min_qty: market.min_qty,
            fee_bps: market.fee_bps,
        });
    }
    Ok(out)
}

async fn rpc_future_markets(cap: &Cap) -> capnp::Result<Vec<FutureMarketSnapshot>> {
    let response = cap.list_future_markets_request().send().promise.await?;
    let list = response.get()?.get_markets()?;
    let mut out = Vec::with_capacity(list.len() as usize);
    for i in 0..list.len() {
        let market = conv::future_market_from_capnp(list.get(i))?;
        out.push(FutureMarketSnapshot {
            id: market.id,
            underlying: market.underlying,
            tick_size: market.tick_size,
            lot_size: market.lot_size,
            max_leverage_x100: market.max_leverage_x100,
            initial_margin_bps: market.initial_margin_bps,
            maintenance_margin_bps: market.maintenance_margin_bps,
            taker_fee_bps: market.taker_fee_bps,
            maker_fee_bps: market.maker_fee_bps_signed,
            expiry_height: market.expiry_height,
            paused: market.paused,
            reduce_only_mode: market.paused_reduce_only_only,
            final_settlement_px_q8: market.final_settlement_px_q8,
            last_trade_px_q8: market.last_trade_px_q8,
            last_trade_height: market.last_trade_height,
        });
    }
    Ok(out)
}

async fn rpc_block(cap: &Cap, height: u64) -> capnp::Result<BlockSummary> {
    let mut request = cap.get_block_request();
    request.get().set_height(height);
    let response = request.send().promise.await?;
    let block = conv::block_from_capnp(response.get()?.get_block()?)?;
    Ok(summarise(&block))
}

// ---------------------------------------------------------------------------
// Streams
// ---------------------------------------------------------------------------

/// Flatten a decoded block into the gateway's owned summary.
///
/// Senders are pre-recovered by the sequencer on modern blocks; for older ones
/// the signature is recovered here, which is the only place the gateway pays
/// for ECDSA.
pub(crate) fn summarise(block: &cb_block::Block) -> BlockSummary {
    let header = &block.header;
    let mut txs = Vec::with_capacity(block.txs.len());
    for (index, signed) in block.txs.iter().enumerate() {
        let sender = block
            .senders
            .get(index)
            .copied()
            .or_else(|| signed.sender().ok())
            .unwrap_or(Address::ZERO);
        txs.push(BlockTx {
            applied: block.applied.get(index).copied(),
            hash: signed.hash(),
            sender,
            index: u32::try_from(index).unwrap_or(u32::MAX),
            body: signed.tx.body.clone(),
        });
    }
    BlockSummary {
        height: header.height,
        hash: header.hash(),
        state_root: header.state_root,
        timestamp_ms: header.timestamp,
        proposer: recover_proposer(header),
        txs,
    }
}

/// Recover the proposer from the sequencer's header signature.
///
/// The header has no explicit proposer field — replicas recover it exactly
/// this way — so the gateway does the same rather than reporting a placeholder
/// address on `/chain/blocks`.
fn recover_proposer(header: &cb_block::BlockHeader) -> Option<Address> {
    let signature = cb_crypto::Signature::from_bytes(&header.sequencer_sig).ok()?;
    let digest = cb_crypto::keccak256(&header.signing_bytes());
    cb_crypto::recover(&signature, digest).ok()
}

/// Follow the node's block stream, publishing every block to the fan-out.
async fn block_stream(cap: Cap, ctx: Rc<SessionCtx>) -> capnp::Result<()> {
    let from = ctx.next_height.load(Ordering::SeqCst);
    let mut request = cap.stream_blocks_request();
    request.get().set_from_height(from);
    let response = request.send().promise.await?;
    let sub = response.get()?.get_sub()?;

    loop {
        if ctx.stopping.load(Ordering::SeqCst) {
            return Ok(());
        }
        let next = sub.next_request().send().promise.await?;
        let block = conv::block_from_capnp(next.get()?.get_block()?)?;
        let summary = summarise(&block);
        ctx.observe_height(summary.height);
        ctx.next_height.store(summary.height + 1, Ordering::SeqCst);
        // A send failure means nobody is listening yet, which is fine.
        let _ = ctx
            .events
            .send(NodeEvent::Block(std::sync::Arc::new(summary)));
    }
}

/// Follow the sequencer's refusals, publishing each batch to the fan-out.
async fn rejected_stream(cap: Cap, ctx: Rc<SessionCtx>) -> capnp::Result<()> {
    let mut request = cap.stream_rejected_request();
    request.get().set_from_seq(0);
    let response = request.send().promise.await?;
    let sub = response.get()?.get_sub()?;

    loop {
        if ctx.stopping.load(Ordering::SeqCst) {
            return Ok(());
        }
        let next = sub.next_request().send().promise.await?;
        let reply = next.get()?;
        let list = reply.get_entries()?;
        let mut refused = Vec::with_capacity(list.len() as usize);
        for i in 0..list.len() {
            let entry = conv::rejected_entry_from_capnp(list.get(i))?;
            refused.push(RefusedTx {
                hash: entry.hash,
                sender: entry.sender,
                nonce: entry.nonce,
                height: entry.height,
                reason: entry.reason,
            });
        }
        if !refused.is_empty() {
            let _ = ctx.events.send(NodeEvent::Refused(refused));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disconnect_is_unavailable_not_internal() {
        let error = capnp::Error::disconnected("peer went away".into());
        assert!(matches!(classify(error), PortError::Unavailable(_)));
    }

    #[test]
    fn an_unknown_entity_is_not_found() {
        let error = capnp::Error::failed("unknown market".into());
        assert!(matches!(classify(error), PortError::NotFound(_)));
    }

    #[test]
    fn an_unclassified_failure_is_internal() {
        let error = capnp::Error::failed("something odd".into());
        assert!(matches!(classify(error), PortError::Internal(_)));
    }
}
