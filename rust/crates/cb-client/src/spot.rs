//! Spot trade surface for the unified `place` / `cancel` / `book` REPL
//! commands.
//!
//! The client routes one `<market>` argument into either the spot CLOB
//! (`TxBody::PlaceOrder` / `CancelOrder`) or the perp/futures book
//! (`TxBody::PlacePerpOrder` / `CancelPerpOrder`). The chain itself doesn't
//! mix the two — the spot executor lives in `cb-block::executor` (S3) and
//! the perp executor lives behind `PlacePerpOrder`. This module holds the
//! parser and the spot-side submit/RPC wrappers; the perp side stays in
//! `derivs.rs`.

use anyhow::{anyhow, Context, Result};
use cb_rpc::conv::{self, SpotOrderBookSnapshot};
use cb_tx::{SignedTx, Transaction, TxBody};
use cb_types::{Address, DerivId, MarketId, Side, Tif, H256};
use std::net::SocketAddr;

use crate::client::{run_call, SubmitOutcome};

/// Routing target for an unprefixed market reference.
///
/// `Spot` lands on the spot CLOB (`PlaceOrder` / `CancelOrder`); `Deriv`
/// covers both `perp:N` (linear perp) and `future:N` (dated future) since
/// they share `TxBody::PlacePerpOrder` on-wire — the executor distinguishes
/// them by the `DerivId` discriminant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarketRef {
    Spot(MarketId),
    Deriv(DerivId),
}

/// Parse `spot:btc_usd` / `spot:eth_usd` / `perp:N` / `future:N` (case
/// insensitive on both sides of the colon).
///
/// The unprefixed form (`btc_usd`) is rejected: every `<market>` arg in the
/// trade surface must declare which book it targets so the dispatch layer
/// can pick the right TxBody variant.
pub fn parse_market_ref(s: &str) -> Result<MarketRef> {
    let (kind, rest) = s.split_once(':').ok_or_else(|| {
        anyhow!("market must be `spot:<id>`, `perp:N`, or `future:N` (got `{s}`)")
    })?;
    if rest.is_empty() {
        return Err(anyhow!(
            "market id must follow the colon (got `{s}`); try `spot:btc_usd` or `perp:0`"
        ));
    }
    match kind.to_ascii_lowercase().as_str() {
        "spot" => Ok(MarketRef::Spot(parse_spot_id(rest)?)),
        "perp" => {
            let n: u16 = rest
                .parse()
                .map_err(|_| anyhow!("perp index must be u16, got `{rest}`"))?;
            Ok(MarketRef::Deriv(DerivId::perp(n)))
        }
        "future" | "futures" => {
            let n: u16 = rest
                .parse()
                .map_err(|_| anyhow!("future index must be u16, got `{rest}`"))?;
            Ok(MarketRef::Deriv(DerivId::future(n)))
        }
        other => Err(anyhow!(
            "unknown market kind `{other}` (expected `spot`, `perp`, or `future`)"
        )),
    }
}

/// Parse a spot market identifier (`btc_usd`, `btcusd`, …; case-insensitive).
///
/// Delegates to `MarketId`'s `FromStr`, which is driven by `MarketId::ALL`, so
/// the accepted set is exactly the listed universe and cannot drift from it.
/// Note that the chain a client is talking to may not have *listed* every
/// market this parser accepts — genesis decides that, and the node answers
/// `MarketNotFound`. Rejecting here is only about catching typos early.
pub fn parse_spot_id(s: &str) -> Result<MarketId> {
    s.parse::<MarketId>().map_err(|_| {
        let listed: Vec<&str> = MarketId::ALL.iter().map(|m| m.genesis_id()).collect();
        anyhow!(
            "unknown spot market `{}` (expected one of: {})",
            s,
            listed.join(", ")
        )
    })
}

/// Parse the trailing `[gtc|ioc|fok|post-only]*` flags for a spot order.
///
/// `reduce-only` is rejected here: the spot executor never honors it (the
/// `TxBody::PlaceOrder` variant has no such field), and silently dropping
/// the flag would mislead operators who copy-pasted from a perp invocation.
pub fn parse_spot_flags(flags: &[String]) -> Result<(Tif, bool)> {
    let mut tif = Tif::Gtc;
    let mut tif_set = false;
    let mut post_only = false;
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
            "reduce-only" | "reduce_only" | "reduceonly" => {
                return Err(anyhow!(
                    "reduce-only is not valid for spot orders \
                     (use `perp:` or `future:` for reduce-only)"
                ));
            }
            other => return Err(anyhow!("unknown spot flag `{other}`")),
        }
    }
    Ok((tif, post_only))
}

/// Submit a `PlaceOrder` (spot CLOB) tx with the current wallet.
#[allow(clippy::too_many_arguments)]
pub fn submit_spot_place(
    rpc: SocketAddr,
    chain_id: u32,
    sk: cb_crypto::SecretKey,
    sender: Address,
    market: MarketId,
    side: Side,
    qty: u128,
    price: u128,
    tif: Tif,
    post_only: bool,
    client_order_id: H256,
) -> Result<(H256, u64, SubmitOutcome)> {
    run_call(rpc, move |c| async move {
        let st = crate::client::get_account(&c, &sender).await?;
        let nonce = st.nonce;
        let tx = Transaction {
            nonce,
            chain_id,
            body: TxBody::PlaceOrder {
                market,
                side,
                price,
                qty,
                tif,
                post_only,
                client_order_id,
            },
        };
        let signed = SignedTx::sign_with(&sk, tx).context("sign tx")?;
        let hash = signed.hash();
        let outcome = crate::client::submit_tx(&c, &signed).await?;
        Ok::<_, anyhow::Error>((hash, nonce, outcome))
    })
}

/// Submit a `CancelOrder` (spot CLOB) tx with the current wallet.
pub fn submit_spot_cancel(
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
            body: TxBody::CancelOrder { order_id },
        };
        let signed = SignedTx::sign_with(&sk, tx).context("sign tx")?;
        let hash = signed.hash();
        let outcome = crate::client::submit_tx(&c, &signed).await?;
        Ok::<_, anyhow::Error>((hash, nonce, outcome))
    })
}

/// RPC wrapper around the spot `getOrderBook` call. Wired to S4's real
/// implementation; if S4 hasn't landed locally the server returns a
/// `not implemented` error, which the caller renders as a plain string
/// rather than crashing the REPL.
pub async fn rpc_get_spot_book(
    c: &cb_rpc::Client,
    market: MarketId,
    depth: u32,
) -> Result<SpotOrderBookSnapshot> {
    let mut req = c.cap.get_order_book_request();
    let mut p = req.get();
    p.set_market(conv::market_to_capnp(market));
    p.set_depth(depth);
    let resp = req.send().promise.await.context("getOrderBook RPC")?;
    Ok(conv::spot_book_snapshot_from_capnp(
        resp.get()?.get_snapshot()?,
    )?)
}

/// Render a spot orderbook snapshot using the same column layout as
/// `derivs::render_book` so operators see one consistent table no matter
/// which book they query.
pub fn render_spot_book(snap: &SpotOrderBookSnapshot, json: bool) -> String {
    if json {
        let bids: Vec<_> = snap
            .bids
            .iter()
            .map(|l| serde_json::json!({"price": l.price.to_string(), "qty": l.qty.to_string()}))
            .collect();
        let asks: Vec<_> = snap
            .asks
            .iter()
            .map(|l| serde_json::json!({"price": l.price.to_string(), "qty": l.qty.to_string()}))
            .collect();
        return format!(
            "{}\n",
            serde_json::json!({
                "market": format!("spot:{}", spot_label(snap.market)),
                "as_of_height": snap.as_of_height,
                "bids": bids,
                "asks": asks,
            })
        );
    }
    let mut out = String::new();
    out.push_str(&format!(
        "market: spot:{}   as_of_height: {}\n",
        spot_label(snap.market),
        snap.as_of_height
    ));
    out.push_str("side  price                 qty\n");
    for l in &snap.asks {
        out.push_str(&format!("ask   {:<20}  {}\n", l.price, l.qty));
    }
    for l in &snap.bids {
        out.push_str(&format!("bid   {:<20}  {}\n", l.price, l.qty));
    }
    out
}

/// Stable display name for a `MarketId`. Mirrors the genesis-file market
/// labels so operators see the same string in `markets`, `place`, and `book`
/// output.
pub fn spot_label(m: MarketId) -> &'static str {
    m.genesis_id()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_market_ref_accepts_spot_perp_future() {
        assert_eq!(
            parse_market_ref("spot:btc_usd").unwrap(),
            MarketRef::Spot(MarketId::BtcUsd)
        );
        assert_eq!(
            parse_market_ref("SPOT:ETH_USD").unwrap(),
            MarketRef::Spot(MarketId::EthUsd)
        );
        assert_eq!(
            parse_market_ref("perp:0").unwrap(),
            MarketRef::Deriv(DerivId::perp(0))
        );
        assert_eq!(
            parse_market_ref("future:3").unwrap(),
            MarketRef::Deriv(DerivId::future(3))
        );
        assert_eq!(
            parse_market_ref("futures:9").unwrap(),
            MarketRef::Deriv(DerivId::future(9))
        );
    }

    #[test]
    fn parse_market_ref_rejects_unknown_or_unprefixed() {
        // No prefix.
        assert!(parse_market_ref("btc_usd").is_err());
        // Empty body.
        assert!(parse_market_ref("spot:").is_err());
        assert!(parse_market_ref("perp:").is_err());
        // Unknown spot pair.
        assert!(parse_market_ref("spot:doge_usd").is_err());
        // Bad index.
        assert!(parse_market_ref("perp:abc").is_err());
        // Unknown kind.
        assert!(parse_market_ref("forex:btc_usd").is_err());
    }

    #[test]
    fn parse_spot_flags_default_is_gtc_no_postonly() {
        let (tif, p) = parse_spot_flags(&[]).unwrap();
        assert_eq!(tif, Tif::Gtc);
        assert!(!p);
    }

    #[test]
    fn parse_spot_flags_compose_tif_and_postonly() {
        let (tif, p) = parse_spot_flags(&["ioc".into(), "post-only".into()]).unwrap();
        assert_eq!(tif, Tif::Ioc);
        assert!(p);
    }

    #[test]
    fn parse_spot_flags_rejects_reduce_only() {
        let err = parse_spot_flags(&["reduce-only".into()]).unwrap_err();
        assert!(
            err.to_string()
                .contains("reduce-only is not valid for spot"),
            "{err}"
        );
    }

    #[test]
    fn parse_spot_flags_rejects_conflicting_tifs() {
        assert!(parse_spot_flags(&["ioc".into(), "fok".into()]).is_err());
    }

    #[test]
    fn parse_spot_id_accepts_either_separator() {
        assert_eq!(parse_spot_id("btc_usd").unwrap(), MarketId::BtcUsd);
        assert_eq!(parse_spot_id("BTCUSD").unwrap(), MarketId::BtcUsd);
        assert_eq!(parse_spot_id("eth_usd").unwrap(), MarketId::EthUsd);
        assert!(parse_spot_id("doge").is_err());
    }
}
