//! Networked mode: the same bot population, driving a real node over
//! Cap'n Proto RPC.
//!
//! This is the mode that makes the frontend feel alive — a human who joins
//! the exchange finds a two-sided book, a moving price and counterparties who
//! react to them, because it is the *same* strategies from [`crate::bots`]
//! reading state off the node instead of out of RocksDB.
//!
//! ## What is different from the in-process mode
//!
//! * **Nonces come from the node.** Each bot syncs its nonce from
//!   `getAccount` at startup and after any rejection, rather than reading a
//!   live `BlockState`. A rejection still does not advance the nonce, so the
//!   resync is what keeps the noise bot's deliberately invalid orders from
//!   wedging that bot for the rest of the session.
//! * **Invariants are not checked here.** The node owns its state; the
//!   checker needs the whole ledger, which RPC deliberately does not expose
//!   in one call. Correctness is proven in-process, where a full scan is
//!   cheap and the run is reproducible; this mode exists to keep a live
//!   deployment interesting.
//! * **The oracle is optional.** Only the key named as a market's
//!   `oracle_authority` may post prices. Pass `drive_oracle` with a seed that
//!   matches the deployment's genesis authority to have the scenario engine
//!   drive the index; otherwise the node's own oracle keeps running and the
//!   bots trade against whatever it publishes.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Result;
use cb_crypto::{address_from_pubkey, keypair_from_seed, SecretKey};
use cb_rpc::conv::{address_to_capnp, deriv_id_to_capnp, signed_tx_to_capnp};
use cb_schema::rpc_capnp::submit_result;
use cb_tx::{SignedTx, Transaction, TxBody};
use cb_types::{Address, DerivId, MarketId, Side};
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;

use crate::bots::{Bot, MarketObs, Observation, SelfObs, SpotObs};
use crate::scenario::{PriceEngine, ScenarioTimeline};
use crate::universe::SIM_MARKETS;

/// Configuration for a networked session.
#[derive(Clone, Debug)]
pub struct NetConfig {
    /// Node to connect to.
    pub sequencer: SocketAddr,
    /// Chain id for signing.
    pub chain_id: u32,
    /// Master seed for bot decisions and the price path.
    pub seed: u64,
    /// Scenario schedule driving the oracle, when `drive_oracle` is set.
    pub timeline: ScenarioTimeline,
    /// Bot population.
    pub bots: crate::world::BotCounts,
    /// Blocks' worth of ticks to run. `None` runs until interrupted.
    pub ticks: Option<u64>,
    /// Wall-clock gap between ticks.
    pub tick_interval: Duration,
    /// Post oracle prices from the scenario engine. Requires the seed below
    /// to match the deployment's genesis oracle authority.
    pub drive_oracle: bool,
    /// Seed phrase for the oracle key, when `drive_oracle` is set.
    pub oracle_seed: String,
    /// Spot market the bots quote.
    pub spot_market: MarketId,
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            sequencer: "127.0.0.1:7700".parse().expect("valid"),
            chain_id: 1,
            seed: 0xC0FFEE,
            timeline: ScenarioTimeline::parse("calm").expect("valid"),
            bots: crate::world::BotCounts::default(),
            ticks: None,
            tick_interval: Duration::from_millis(250),
            drive_oracle: false,
            oracle_seed: "cb-simgame-oracle".to_string(),
            spot_market: MarketId::BtcUsd,
        }
    }
}

struct NetBot {
    bot: Box<dyn Bot>,
    sk: SecretKey,
    addr: Address,
    nonce: u64,
}

/// Run the bot population against a live node.
///
/// Must be called inside a `tokio::task::LocalSet`: the Cap'n Proto client is
/// `!Send`, the same constraint `cb-sim` works under.
pub async fn run(cfg: NetConfig) -> Result<()> {
    let client = cb_rpc::connect(cfg.sequencer).await?;
    tracing::info!(sequencer = %cfg.sequencer, "cb-simgame networked mode connected");

    let mut labels = BTreeMap::new();
    let mut bots: Vec<NetBot> = Vec::new();
    for (label, kind_bot) in crate::world::describe_population(&cfg.bots) {
        let (sk, pk) = keypair_from_seed(format!("cb-simgame-{label}").as_bytes());
        let addr = address_from_pubkey(&pk);
        labels.insert(addr, label.clone());
        bots.push(NetBot {
            bot: kind_bot,
            sk,
            addr,
            nonce: 0,
        });
    }

    let (oracle_sk, oracle_pk) = keypair_from_seed(cfg.oracle_seed.as_bytes());
    let oracle_addr = address_from_pubkey(&oracle_pk);
    let mut oracle_nonce: u64 = 0;

    // Sync nonces so a restart against a live chain does not stall behind a
    // wall of BadNonce.
    for b in bots.iter_mut() {
        b.nonce = fetch_nonce(&client, b.addr).await?;
    }
    // `apply_oracle_update` rejects a post whose `submitted_height` is below
    // the stored `source_height`, so the heights this session submits have to
    // continue from whatever the node already holds rather than restarting at
    // zero.
    let mut oracle_height: u64 = 0;
    if cfg.drive_oracle {
        oracle_nonce = fetch_nonce(&client, oracle_addr).await?;
        for sm in SIM_MARKETS.iter() {
            if let Ok(h) = read_oracle_source_height(&client, sm.id).await {
                oracle_height = oracle_height.max(h);
            }
        }
        tracing::info!(
            oracle = %oracle_addr,
            from_height = oracle_height,
            "driving the oracle from the scenario engine"
        );
    }

    let mut prices = PriceEngine::new(cfg.seed, cfg.timeline.clone());
    let mut tick: u64 = 0;
    loop {
        if let Some(limit) = cfg.ticks {
            if tick >= limit {
                break;
            }
        }
        prices.step(tick);

        if cfg.drive_oracle {
            oracle_height += 1;
            for sm in SIM_MARKETS.iter() {
                let px = prices.price_q8(sm.underlying);
                let height = oracle_height;
                let body = TxBody::OracleUpdate {
                    market: sm.id,
                    px_q8: px,
                    submitted_height: height,
                };
                if submit(&client, &oracle_sk, oracle_nonce, cfg.chain_id, body).await? {
                    oracle_nonce += 1;
                } else {
                    oracle_nonce = fetch_nonce(&client, oracle_addr).await?;
                }
            }
        }

        let markets = read_markets(&client).await?;
        let spot = SpotObs {
            // Networked mode drives the derivative books; the spot lock
            // defects in `docs/qa-report.md` make unattended spot quoting a
            // way to panic the node, so the bots leave it alone here.
            enabled: false,
            market: cfg.spot_market,
            best_bid: None,
            best_ask: None,
            reference: crate::world::spot_price_from_index(
                prices.price_q8(crate::universe::Underlying::Btc),
            ),
            min_qty: 1,
            tick: 1,
        };

        for (idx, slot_addr) in bots.iter().map(|b| b.addr).enumerate().collect::<Vec<_>>() {
            let me = read_self(&client, slot_addr).await?;
            let obs = Observation {
                height: tick,
                offset: tick,
                markets: markets.clone(),
                spot: spot.clone(),
                me,
                // Liquidation targets need a ledger-wide scan, which RPC does
                // not offer; in networked mode the liquidator idles.
                liquidatable: Vec::new(),
            };
            let mut rng = ChaCha8Rng::seed_from_u64(
                cfg.seed
                    .wrapping_mul(0x2545_F491_4F6C_DD1D)
                    .wrapping_add(tick.wrapping_mul(1_000_003))
                    .wrapping_add(idx as u64),
            );
            let bodies = bots[idx].bot.act(&obs, &mut rng);
            for body in bodies {
                let (sk, nonce, addr) = (bots[idx].sk, bots[idx].nonce, bots[idx].addr);
                if submit(&client, &sk, nonce, cfg.chain_id, body).await? {
                    bots[idx].nonce += 1;
                } else {
                    bots[idx].nonce = fetch_nonce(&client, addr).await?;
                }
            }
        }

        tick += 1;
        tokio::time::sleep(cfg.tick_interval).await;
    }
    tracing::info!(ticks = tick, "cb-simgame networked session finished");
    Ok(())
}

async fn fetch_nonce(client: &cb_rpc::client::Client, addr: Address) -> Result<u64> {
    let mut req = client.cap.get_account_request();
    address_to_capnp(req.get().init_addr(), &addr);
    let resp = req.send().promise.await?;
    Ok(resp.get()?.get_state()?.get_nonce())
}

/// Submit a signed transaction. Returns `true` when the node accepted it.
async fn submit(
    client: &cb_rpc::client::Client,
    sk: &SecretKey,
    nonce: u64,
    chain_id: u32,
    body: TxBody,
) -> Result<bool> {
    let signed = SignedTx::sign_with(
        sk,
        Transaction {
            nonce,
            chain_id,
            body,
        },
    )?;
    let mut req = client.cap.submit_tx_request();
    signed_tx_to_capnp(req.get().init_tx(), &signed);
    let resp = req.send().promise.await?;
    let result = resp.get()?.get_result()?;
    Ok(matches!(result.which()?, submit_result::Ok(())))
}

async fn read_markets(client: &cb_rpc::client::Client) -> Result<Vec<MarketObs>> {
    let mut out = Vec::new();
    for sm in SIM_MARKETS.iter() {
        let mark = read_mark(client, sm.id).await.unwrap_or(0);
        if mark == 0 {
            continue;
        }
        let oracle = read_oracle(client, sm.id).await.unwrap_or(mark);
        let (best_bid, best_ask, tick) = read_book(client, sm.id).await?;
        out.push(MarketObs {
            id: sm.id,
            label: sm.label,
            mark_q8: mark,
            oracle_q8: oracle,
            best_bid,
            best_ask,
            tick,
            restricted: false,
            settled: false,
        });
    }
    Ok(out)
}

async fn read_mark(client: &cb_rpc::client::Client, market: DerivId) -> Result<u128> {
    let mut req = client.cap.get_mark_price_request();
    deriv_id_to_capnp(req.get().init_market(), &market);
    let resp = req.send().promise.await?;
    let mark = resp.get()?.get_mark()?;
    Ok(cb_rpc::conv::u128_from_capnp(mark.get_px_q8()?))
}

/// The `source_height` the node currently holds for a market, so this session
/// can continue from it instead of posting a stale height.
async fn read_oracle_source_height(
    client: &cb_rpc::client::Client,
    market: DerivId,
) -> Result<u64> {
    let mut req = client.cap.get_oracle_price_request();
    deriv_id_to_capnp(req.get().init_market(), &market);
    let resp = req.send().promise.await?;
    Ok(resp.get()?.get_oracle()?.get_source_height())
}

async fn read_oracle(client: &cb_rpc::client::Client, market: DerivId) -> Result<u128> {
    let mut req = client.cap.get_oracle_price_request();
    deriv_id_to_capnp(req.get().init_market(), &market);
    let resp = req.send().promise.await?;
    let o = resp.get()?.get_oracle()?;
    Ok(cb_rpc::conv::u128_from_capnp(o.get_px_q8()?))
}

async fn read_book(
    client: &cb_rpc::client::Client,
    market: DerivId,
) -> Result<(Option<u128>, Option<u128>, u128)> {
    let mut req = client.cap.get_perp_order_book_request();
    {
        let mut b = req.get();
        b.set_depth(1);
        deriv_id_to_capnp(b.init_market(), &market);
    }
    let resp = req.send().promise.await?;
    let snap = resp.get()?.get_snapshot()?;
    let bids = snap.get_bids()?;
    let asks = snap.get_asks()?;
    let best_bid = if bids.is_empty() {
        None
    } else {
        Some(cb_rpc::conv::u128_from_capnp(bids.get(0).get_price_q8()?))
    };
    let best_ask = if asks.is_empty() {
        None
    } else {
        Some(cb_rpc::conv::u128_from_capnp(asks.get(0).get_price_q8()?))
    };
    Ok((best_bid, best_ask, 1))
}

async fn read_self(client: &cb_rpc::client::Client, addr: Address) -> Result<SelfObs> {
    let mut obs = SelfObs::default();
    {
        let mut req = client.cap.get_margin_account_request();
        address_to_capnp(req.get().init_addr(), &addr);
        let resp = req.send().promise.await?;
        let m = resp.get()?.get_margin()?;
        obs.collateral = cb_rpc::conv::u128_from_capnp(m.get_collateral()?);
        obs.bad_debt = cb_rpc::conv::u128_from_capnp(m.get_bad_debt()?);
    }
    {
        let mut req = client.cap.get_positions_request();
        address_to_capnp(req.get().init_addr(), &addr);
        let resp = req.send().promise.await?;
        for p in resp.get()?.get_positions()? {
            let pos = cb_rpc::conv::position_from_capnp(p)?;
            if pos.qty > 0 {
                obs.positions
                    .insert(pos.market, (pos.side, pos.qty, pos.entry_px_q8));
            }
        }
    }
    {
        let mut req = client.cap.get_account_request();
        address_to_capnp(req.get().init_addr(), &addr);
        let resp = req.send().promise.await?;
        let state = resp.get()?.get_state()?;
        for e in state.get_balances()? {
            let amount = cb_rpc::conv::u128_from_capnp(e.get_amount()?);
            match cb_rpc::conv::asset_from_capnp(e.get_asset()?) {
                cb_types::AssetId::Btc => obs.spot_base_free = amount,
                cb_types::AssetId::Usd => obs.spot_quote_free = amount,
                _ => {}
            }
        }
    }
    Ok(obs)
}

/// Placeholder so the unused `Side` import stays meaningful if the book
/// reader grows a side-aware path.
#[allow(dead_code)]
fn side_label(s: Side) -> &'static str {
    match s {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}
