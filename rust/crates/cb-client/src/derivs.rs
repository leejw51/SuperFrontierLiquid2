//! Derivatives RPC + dispatcher helpers for the M6 client commands.
//!
//! Each pub function here is a thin wrapper around `cb-rpc` that:
//!   1. drives the LocalSet,
//!   2. extracts the typed Rust struct from the capnp reply,
//!   3. returns either a human-formatted table or a JSON line, controlled by
//!      the `--json` flag on the command.
//!
//! Sign + submit helpers (`perp-place`, `perp-cancel`, `liquidate`) live here
//! too because they share the `DerivId` parser and the small cluster of
//! formatting helpers that the read commands also use.

use anyhow::{anyhow, Context, Result};
use cb_rpc::conv;
use cb_schema::rpc_capnp;
use cb_tx::{SignedTx, Transaction, TxBody};
use cb_types::{
    Address, DerivId, FundingIndex, FutureMarket, MarginAccount, MarkPrice, OraclePrice,
    PerpMarket, Position, Side, Tif, H256,
};
use std::net::SocketAddr;

use crate::client::{run_call, SubmitOutcome};

// ---------------------------------------------------------------------------
// Argument parsing helpers
// ---------------------------------------------------------------------------

/// Parse `perp:N` or `future:N` into a `DerivId`. The display form mirrors
/// `DerivId::Display` so the round-trip through stdout stays stable.
pub fn parse_market(s: &str) -> Result<DerivId> {
    let (kind, idx) = s
        .split_once(':')
        .ok_or_else(|| anyhow!("market must be `perp:N` or `future:N` (got `{s}`)"))?;
    let n: u16 = idx
        .parse()
        .map_err(|_| anyhow!("market index must be u16, got `{idx}`"))?;
    match kind.to_ascii_lowercase().as_str() {
        "perp" => Ok(DerivId::perp(n)),
        "future" | "futures" => Ok(DerivId::future(n)),
        other => Err(anyhow!(
            "unknown market kind `{other}` (expected `perp` or `future`)"
        )),
    }
}

/// Parse `buy`/`sell` (case-insensitive) into a `Side`.
pub fn parse_side(s: &str) -> Result<Side> {
    match s.to_ascii_lowercase().as_str() {
        "buy" => Ok(Side::Buy),
        "sell" => Ok(Side::Sell),
        other => Err(anyhow!("side must be `buy` or `sell`, got `{other}`")),
    }
}

/// Parse the trailing `[gtc|ioc|fok|post-only|reduce-only]*` flags into a
/// `(tif, post_only, reduce_only)` triple. Multiple flags are allowed; the
/// canonical default is `(GTC, false, false)`.
pub fn parse_perp_flags(flags: &[String]) -> Result<(Tif, bool, bool)> {
    let mut tif = Tif::Gtc;
    let mut tif_set = false;
    let mut post_only = false;
    let mut reduce_only = false;
    for raw in flags {
        match raw.to_ascii_lowercase().as_str() {
            "gtc" => {
                if tif_set && tif != Tif::Gtc {
                    return Err(anyhow!(
                        "multiple TIF flags (`{raw}` after a different TIF)"
                    ));
                }
                tif = Tif::Gtc;
                tif_set = true;
            }
            "ioc" => {
                if tif_set && tif != Tif::Ioc {
                    return Err(anyhow!(
                        "multiple TIF flags (`{raw}` after a different TIF)"
                    ));
                }
                tif = Tif::Ioc;
                tif_set = true;
            }
            "fok" => {
                if tif_set && tif != Tif::Fok {
                    return Err(anyhow!(
                        "multiple TIF flags (`{raw}` after a different TIF)"
                    ));
                }
                tif = Tif::Fok;
                tif_set = true;
            }
            "post-only" | "post_only" | "postonly" => post_only = true,
            "reduce-only" | "reduce_only" | "reduceonly" => reduce_only = true,
            other => return Err(anyhow!("unknown perp flag `{other}`")),
        }
    }
    Ok((tif, post_only, reduce_only))
}

/// Parse a hex-encoded H256 (with or without leading `0x`).
pub fn parse_h256(s: &str) -> Result<H256> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(s).context("decode hex")?;
    if bytes.len() != 32 {
        return Err(anyhow!("expected 32 bytes (64 hex), got {}", bytes.len()));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(H256(out))
}

// ---------------------------------------------------------------------------
// Read RPCs
// ---------------------------------------------------------------------------

pub async fn rpc_get_positions(c: &cb_rpc::Client, addr: &Address) -> Result<Vec<Position>> {
    let mut req = c.cap.get_positions_request();
    conv::address_to_capnp(req.get().init_addr(), addr);
    let resp = req.send().promise.await.context("getPositions RPC")?;
    let list = resp.get()?.get_positions()?;
    let mut out = Vec::with_capacity(list.len() as usize);
    for i in 0..list.len() {
        out.push(conv::position_from_capnp(list.get(i))?);
    }
    Ok(out)
}

pub async fn rpc_get_margin(c: &cb_rpc::Client, addr: &Address) -> Result<MarginAccount> {
    let mut req = c.cap.get_margin_account_request();
    conv::address_to_capnp(req.get().init_addr(), addr);
    let resp = req.send().promise.await.context("getMarginAccount RPC")?;
    Ok(conv::margin_account_from_capnp(resp.get()?.get_margin()?)?)
}

pub async fn rpc_get_funding(c: &cb_rpc::Client, market: &DerivId) -> Result<FundingIndex> {
    let mut req = c.cap.get_funding_index_request();
    conv::deriv_id_to_capnp(req.get().init_market(), market);
    let resp = req.send().promise.await.context("getFundingIndex RPC")?;
    Ok(conv::funding_index_from_capnp(resp.get()?.get_index()?)?)
}

pub async fn rpc_get_mark(c: &cb_rpc::Client, market: &DerivId) -> Result<MarkPrice> {
    let mut req = c.cap.get_mark_price_request();
    conv::deriv_id_to_capnp(req.get().init_market(), market);
    let resp = req.send().promise.await.context("getMarkPrice RPC")?;
    Ok(conv::mark_price_from_capnp(resp.get()?.get_mark()?)?)
}

#[allow(dead_code)]
pub async fn rpc_get_oracle(c: &cb_rpc::Client, market: &DerivId) -> Result<OraclePrice> {
    let mut req = c.cap.get_oracle_price_request();
    conv::deriv_id_to_capnp(req.get().init_market(), market);
    let resp = req.send().promise.await.context("getOraclePrice RPC")?;
    Ok(conv::oracle_price_from_capnp(resp.get()?.get_oracle()?)?)
}

pub async fn rpc_list_perp_markets(c: &cb_rpc::Client) -> Result<Vec<PerpMarket>> {
    let req = c.cap.list_perp_markets_request();
    let resp = req.send().promise.await.context("listPerpMarkets RPC")?;
    let list = resp.get()?.get_markets()?;
    let mut out = Vec::with_capacity(list.len() as usize);
    for i in 0..list.len() {
        out.push(conv::perp_market_from_capnp(list.get(i))?);
    }
    Ok(out)
}

pub async fn rpc_list_future_markets(c: &cb_rpc::Client) -> Result<Vec<FutureMarket>> {
    let req = c.cap.list_future_markets_request();
    let resp = req.send().promise.await.context("listFutureMarkets RPC")?;
    let list = resp.get()?.get_markets()?;
    let mut out = Vec::with_capacity(list.len() as usize);
    for i in 0..list.len() {
        out.push(conv::future_market_from_capnp(list.get(i))?);
    }
    Ok(out)
}

pub async fn rpc_get_perp_book(
    c: &cb_rpc::Client,
    market: &DerivId,
    depth: u32,
) -> Result<conv::PerpOrderBookSnapshot> {
    let mut req = c.cap.get_perp_order_book_request();
    let mut p = req.get();
    conv::deriv_id_to_capnp(p.reborrow().init_market(), market);
    p.set_depth(depth);
    let resp = req.send().promise.await.context("getPerpOrderBook RPC")?;
    Ok(conv::perp_book_snapshot_from_capnp(
        resp.get()?.get_snapshot()?,
    )?)
}

// ---------------------------------------------------------------------------
// Render helpers — human + JSON
// ---------------------------------------------------------------------------

fn full_addr(a: &Address) -> String {
    format!("0x{}", hex::encode(a.0))
}

pub fn render_positions(addr: &Address, ps: &[Position], json: bool) -> String {
    if json {
        let v: Vec<serde_json::Value> = ps.iter().map(position_to_json).collect();
        return format!("{}\n", serde_json::Value::Array(v));
    }
    let mut out = String::new();
    out.push_str(&format!(
        "address: {}   open positions: {}\n",
        full_addr(addr),
        ps.len()
    ));
    if ps.is_empty() {
        return out;
    }
    out.push_str("market    side  qty                  entry_px_q8          mode  funding_idx          last_funding_h\n");
    for p in ps {
        let side = match p.side {
            Side::Buy => "buy",
            Side::Sell => "sell",
        };
        let mode = if p.margin_mode == 0 { "cross" } else { "iso" };
        out.push_str(&format!(
            "{market:<9} {side:<5} {qty:<20} {epx:<20} {mode:<5} {fi:<20} {lh}\n",
            market = format!("{}", p.market),
            side = side,
            qty = p.qty,
            epx = p.entry_px_q8,
            mode = mode,
            fi = p.entry_funding_index.to_i128(),
            lh = p.last_funding_height,
        ));
    }
    out
}

pub fn render_margin(addr: &Address, m: &MarginAccount, json: bool) -> String {
    if json {
        return format!(
            "{}\n",
            serde_json::json!({
                "owner": full_addr(addr),
                "collateral": m.collateral.to_string(),
                "unrealized_pnl": m.unrealized_pnl.to_i128().to_string(),
                "bad_debt": m.bad_debt.to_string(),
            })
        );
    }
    let mut out = String::new();
    out.push_str(&format!("address: {}\n", full_addr(addr)));
    out.push_str(&format!("collateral:      {}\n", m.collateral));
    out.push_str(&format!(
        "unrealized_pnl:  {}\n",
        m.unrealized_pnl.to_i128()
    ));
    out.push_str(&format!("bad_debt:        {}\n", m.bad_debt));
    out
}

pub fn render_funding(market: &DerivId, f: &FundingIndex, next_tick_h: u64, json: bool) -> String {
    if json {
        return format!(
            "{}\n",
            serde_json::json!({
                "market": format!("{}", market),
                "cumulative": f.cumulative.to_i128().to_string(),
                "rate_q9": f.rate_q9,
                "last_tick_height": f.last_tick_height,
                "next_tick_height": next_tick_h,
            })
        );
    }
    let mut out = String::new();
    out.push_str(&format!("market:            {}\n", market));
    out.push_str(&format!("cumulative:        {}\n", f.cumulative.to_i128()));
    out.push_str(&format!("rate_q9:           {}\n", f.rate_q9));
    out.push_str(&format!("last_tick_height:  {}\n", f.last_tick_height));
    out.push_str(&format!("next_tick_height:  {}\n", next_tick_h));
    out
}

pub fn render_mark(market: &DerivId, m: &MarkPrice, json: bool) -> String {
    if json {
        return format!(
            "{}\n",
            serde_json::json!({
                "market": format!("{}", market),
                "px_q8": m.px_q8.to_string(),
                "computed_at_height": m.computed_at_height,
                "premium_ema_q9": m.premium_ema_q9,
            })
        );
    }
    let mut out = String::new();
    out.push_str(&format!("market:               {}\n", market));
    out.push_str(&format!("mark_px_q8:           {}\n", m.px_q8));
    out.push_str(&format!("computed_at_height:   {}\n", m.computed_at_height));
    out.push_str(&format!("premium_ema_q9:       {}\n", m.premium_ema_q9));
    out
}

pub fn render_book(snap: &conv::PerpOrderBookSnapshot, json: bool) -> String {
    if json {
        let bids: Vec<_> = snap
            .bids
            .iter()
            .map(|l| serde_json::json!({"price_q8": l.price_q8.to_string(), "qty": l.qty.to_string()}))
            .collect();
        let asks: Vec<_> = snap
            .asks
            .iter()
            .map(|l| serde_json::json!({"price_q8": l.price_q8.to_string(), "qty": l.qty.to_string()}))
            .collect();
        return format!(
            "{}\n",
            serde_json::json!({
                "market": format!("{}", snap.market),
                "as_of_height": snap.as_of_height,
                "bids": bids,
                "asks": asks,
            })
        );
    }
    let mut out = String::new();
    out.push_str(&format!(
        "market: {}   as_of_height: {}\n",
        snap.market, snap.as_of_height
    ));
    out.push_str("side  price_q8              qty\n");
    for l in &snap.asks {
        out.push_str(&format!("ask   {:<20}  {}\n", l.price_q8, l.qty));
    }
    for l in &snap.bids {
        out.push_str(&format!("bid   {:<20}  {}\n", l.price_q8, l.qty));
    }
    out
}

pub fn render_markets(perps: &[PerpMarket], futures: &[FutureMarket], json: bool) -> String {
    if json {
        let perps: Vec<_> = perps.iter().map(perp_market_to_json).collect();
        let futures: Vec<_> = futures.iter().map(future_market_to_json).collect();
        let spot = vec![
            serde_json::json!({"id": "spot:btc_usd", "base": "btc", "quote": "usd"}),
            serde_json::json!({"id": "spot:eth_usd", "base": "eth", "quote": "usd"}),
        ];
        return format!(
            "{}\n",
            serde_json::json!({
                "spot": spot,
                "perps": perps,
                "futures": futures,
            })
        );
    }
    let mut out = String::new();
    // Spot markets are declared in `genesis-perps.jsonl` (kind:"market") and
    // are a closed set today; no list-RPC is needed.
    out.push_str("spot: 2\n");
    out.push_str("market         base   quote\n");
    out.push_str("spot:btc_usd   btc    usd\n");
    out.push_str("spot:eth_usd   eth    usd\n");
    out.push_str(&format!("perps: {}\n", perps.len()));
    if !perps.is_empty() {
        out.push_str("market    underlying  tick    lot     lev_x100  imr_bps  mmr_bps  paused  last_trade_px_q8\n");
        for p in perps {
            out.push_str(&format!(
                "{m:<9} {u:<11} {tick:<7} {lot:<7} {lev:<9} {imr:<8} {mmr:<8} {pa:<7} {ltp}\n",
                m = format!("{}", p.id),
                u = asset_short(p.underlying),
                tick = p.tick_size,
                lot = p.lot_size,
                lev = p.max_leverage_x100,
                imr = p.initial_margin_bps,
                mmr = p.maintenance_margin_bps,
                pa = if p.paused { "yes" } else { "no" },
                ltp = p.last_trade_px_q8,
            ));
        }
    }
    out.push_str(&format!("futures: {}\n", futures.len()));
    if !futures.is_empty() {
        out.push_str(
            "market      underlying  expiry_h    method  window  imr_bps  paused  settled_px_q8\n",
        );
        for f in futures {
            let settled = match f.final_settlement_px_q8 {
                Some(v) => v.to_string(),
                None => "-".to_string(),
            };
            out.push_str(&format!(
                "{m:<11} {u:<11} {ex:<11} {meth:<7} {win:<7} {imr:<8} {pa:<7} {settled}\n",
                m = format!("{}", f.id),
                u = asset_short(f.underlying),
                ex = f.expiry_height,
                meth = f.settlement_method,
                win = f.settlement_window_blocks,
                imr = f.initial_margin_bps,
                pa = if f.paused { "yes" } else { "no" },
                settled = settled,
            ));
        }
    }
    out
}

/// Lower-case underlying label for the derivatives tables and JSON views.
/// Delegates to `cb-types` so the CLI cannot drift from the canonical spelling.
fn asset_short(a: cb_types::AssetId) -> &'static str {
    a.lower_symbol()
}

fn position_to_json(p: &Position) -> serde_json::Value {
    serde_json::json!({
        "owner": full_addr(&p.owner),
        "market": format!("{}", p.market),
        "side": match p.side { Side::Buy => "buy", Side::Sell => "sell" },
        "qty": p.qty.to_string(),
        "entry_px_q8": p.entry_px_q8.to_string(),
        "entry_funding_index": p.entry_funding_index.to_i128().to_string(),
        "margin_mode": p.margin_mode,
        "isolated_margin": p.isolated_margin.to_string(),
        "last_funding_height": p.last_funding_height,
    })
}

fn perp_market_to_json(p: &PerpMarket) -> serde_json::Value {
    serde_json::json!({
        "id": format!("{}", p.id),
        "underlying": asset_short(p.underlying),
        "tick_size": p.tick_size.to_string(),
        "lot_size": p.lot_size.to_string(),
        "max_leverage_x100": p.max_leverage_x100,
        "initial_margin_bps": p.initial_margin_bps,
        "maintenance_margin_bps": p.maintenance_margin_bps,
        "taker_fee_bps": p.taker_fee_bps,
        "maker_fee_bps_signed": p.maker_fee_bps_signed,
        "funding_interval_blocks": p.funding_interval_blocks,
        "funding_rate_cap_q9": p.funding_rate_cap_q9,
        "oracle_authority": full_addr(&p.oracle_authority),
        "max_position_notional": p.max_position_notional.to_string(),
        "paused": p.paused,
        "last_trade_px_q8": p.last_trade_px_q8.to_string(),
        "last_trade_height": p.last_trade_height,
    })
}

fn future_market_to_json(f: &FutureMarket) -> serde_json::Value {
    serde_json::json!({
        "id": format!("{}", f.id),
        "underlying": asset_short(f.underlying),
        "expiry_height": f.expiry_height,
        "settlement_method": f.settlement_method,
        "settlement_window_blocks": f.settlement_window_blocks,
        "initial_margin_bps": f.initial_margin_bps,
        "maintenance_margin_bps": f.maintenance_margin_bps,
        "paused": f.paused,
        "final_settlement_px_q8": f.final_settlement_px_q8.map(|v| v.to_string()),
    })
}

// ---------------------------------------------------------------------------
// Submit RPCs
// ---------------------------------------------------------------------------

/// Submit a `PlacePerpOrder` tx with the current wallet.
#[allow(clippy::too_many_arguments)]
pub fn submit_perp_place(
    rpc: SocketAddr,
    chain_id: u32,
    sk: cb_crypto::SecretKey,
    sender: Address,
    market: DerivId,
    side: Side,
    qty: u128,
    price_q8: u128,
    tif: Tif,
    post_only: bool,
    reduce_only: bool,
    client_order_id: H256,
) -> Result<(H256, u64, SubmitOutcome)> {
    run_call(rpc, move |c| async move {
        let st = crate::client::get_account(&c, &sender).await?;
        let nonce = st.nonce;
        let tx = Transaction {
            nonce,
            chain_id,
            body: TxBody::PlacePerpOrder {
                market,
                side,
                price_q8,
                qty,
                tif,
                post_only,
                reduce_only,
                client_order_id,
            },
        };
        let signed = SignedTx::sign_with(&sk, tx).context("sign tx")?;
        let hash = signed.hash();
        let outcome = crate::client::submit_tx(&c, &signed).await?;
        Ok::<_, anyhow::Error>((hash, nonce, outcome))
    })
}

pub fn submit_perp_cancel(
    rpc: SocketAddr,
    chain_id: u32,
    sk: cb_crypto::SecretKey,
    sender: Address,
    order_id: H256,
) -> Result<(H256, u64, SubmitOutcome)> {
    run_call(rpc, move |c| async move {
        let st = crate::client::get_account(&c, &sender).await?;
        let nonce = st.nonce;
        let tx = Transaction {
            nonce,
            chain_id,
            body: TxBody::CancelPerpOrder { order_id },
        };
        let signed = SignedTx::sign_with(&sk, tx).context("sign tx")?;
        let hash = signed.hash();
        let outcome = crate::client::submit_tx(&c, &signed).await?;
        Ok::<_, anyhow::Error>((hash, nonce, outcome))
    })
}

pub fn submit_liquidate(
    rpc: SocketAddr,
    chain_id: u32,
    sk: cb_crypto::SecretKey,
    sender: Address,
    owner: Address,
    market: DerivId,
) -> Result<(H256, u64, SubmitOutcome)> {
    run_call(rpc, move |c| async move {
        let st = crate::client::get_account(&c, &sender).await?;
        let nonce = st.nonce;
        let tx = Transaction {
            nonce,
            chain_id,
            body: TxBody::LiquidationTrigger { owner, market },
        };
        let signed = SignedTx::sign_with(&sk, tx).context("sign tx")?;
        let hash = signed.hash();
        let outcome = crate::client::submit_tx(&c, &signed).await?;
        Ok::<_, anyhow::Error>((hash, nonce, outcome))
    })
}

/// Submit an `OracleUpdate` tx with the current wallet. The wallet's address
/// MUST match the market's `oracle_authority` set in genesis or the executor
/// rejects with `notAuthorized`. `submitted_height` must be >= the previously
/// stored value for this market (strictly older = `oracleStale`).
pub fn submit_oracle_update(
    rpc: SocketAddr,
    chain_id: u32,
    sk: cb_crypto::SecretKey,
    sender: Address,
    market: DerivId,
    px_q8: u128,
    submitted_height: u64,
) -> Result<(H256, u64, SubmitOutcome)> {
    run_call(rpc, move |c| async move {
        let st = crate::client::get_account(&c, &sender).await?;
        let nonce = st.nonce;
        let tx = Transaction {
            nonce,
            chain_id,
            body: TxBody::OracleUpdate {
                market,
                px_q8,
                submitted_height,
            },
        };
        let signed = SignedTx::sign_with(&sk, tx).context("sign tx")?;
        let hash = signed.hash();
        let outcome = crate::client::submit_tx(&c, &signed).await?;
        Ok::<_, anyhow::Error>((hash, nonce, outcome))
    })
}

// `_` to keep the import live in tests where this module is compiled but
// not all helpers are exercised yet.
#[allow(dead_code)]
fn _keep_rpc_capnp_alive(_: rpc_capnp::super_frontier_liquid::Client) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_market_perp_and_future() {
        assert_eq!(parse_market("perp:0").unwrap(), DerivId::perp(0));
        assert_eq!(parse_market("PERP:7").unwrap(), DerivId::perp(7));
        assert_eq!(parse_market("future:3").unwrap(), DerivId::future(3));
        assert_eq!(parse_market("futures:9").unwrap(), DerivId::future(9));
    }

    #[test]
    fn parse_market_rejects_bad_kind_or_index() {
        assert!(parse_market("spot:0").is_err());
        assert!(parse_market("perp").is_err());
        assert!(parse_market("perp:abc").is_err());
        assert!(parse_market("perp:99999999").is_err());
    }

    #[test]
    fn parse_side_canonical() {
        assert_eq!(parse_side("buy").unwrap(), Side::Buy);
        assert_eq!(parse_side("SELL").unwrap(), Side::Sell);
        assert!(parse_side("upward").is_err());
    }

    #[test]
    fn parse_perp_flags_default_is_gtc_no_postonly_no_reduceonly() {
        let (tif, p, r) = parse_perp_flags(&[]).unwrap();
        assert_eq!(tif, Tif::Gtc);
        assert!(!p);
        assert!(!r);
    }

    #[test]
    fn parse_perp_flags_compose() {
        let (tif, p, r) =
            parse_perp_flags(&["ioc".into(), "post-only".into(), "reduce-only".into()]).unwrap();
        assert_eq!(tif, Tif::Ioc);
        assert!(p);
        assert!(r);
    }

    #[test]
    fn parse_perp_flags_rejects_conflicting_tifs() {
        assert!(parse_perp_flags(&["ioc".into(), "fok".into()]).is_err());
    }

    #[test]
    fn parse_h256_accepts_with_or_without_0x() {
        let s = "00".repeat(32);
        assert!(parse_h256(&s).is_ok());
        let with_prefix = format!("0x{s}");
        assert!(parse_h256(&with_prefix).is_ok());
        // Wrong length.
        assert!(parse_h256(&"00".repeat(31)).is_err());
    }
}
