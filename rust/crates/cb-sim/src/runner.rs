//! Connect to the sequencer's capnp RPC and submit a stream of synthetic
//! traffic: spot orders from buyers and sellers, transfers, or a mix.
//!
//! Each tick picks a random trader and, with probability `orders_pct`, places a
//! spot order on BTC/USD or ETH/USD; otherwise it submits one Transfer leg
//! between two traders (the historical workload). Traders with an even index
//! are makers (`Gtc`, resting a few ticks away from mid) and traders with an
//! odd index are takers (`Ioc`, priced through the book); both buy and sell at
//! random, so the population is buyers and sellers on both roles and balances
//! stay roughly conserved. The split is what keeps a taker from crossing its
//! own resting order: the executor rejects a self-trade without consuming the
//! nonce, and with several hundred pipelined transactions behind it every one
//! of them then fails `BadNonce`. Every `CANCEL_EVERY` orders a maker sends a
//! `BatchCancel` for the market, which keeps resting depth bounded.
//!
//! Nonces are pipelined: the local nonce advances before the reply arrives,
//! which is what lets one trader have several transactions in flight. The
//! executor does not advance a nonce for a transaction it rejects, so after a
//! rejection the trader's local nonce is ahead of the chain. The runner then
//! marks the trader stale, stops picking it until its in-flight submits have
//! all answered, and re-reads the nonce from the node — the same recovery the
//! gateway and `cb-simgame` use, with the drain added because a nonce read
//! while transactions are still pending is not yet the truth. Outstanding
//! submits are bounded by `max_in_flight`; without that bound a submit rate
//! above what the node applies only builds an unbounded backlog of promises
//! and says nothing about the node.

use crate::traders::TraderSet;
use cb_rpc::conv::{address_to_capnp, signed_tx_to_capnp};
use cb_schema::rpc_capnp::submit_result;
use cb_tx::{SignedTx, Transaction, TxBody};
use cb_types::{MarketId, Side, Tif, H256};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;

/// Resting price levels on each side of the book that makers spread over.
const PRICE_LEVELS: u128 = 20;
/// A trader sends a `BatchCancel` on the market after this many of its own
/// orders, so resting depth on a long run stays bounded.
const CANCEL_EVERY: u32 = 256;
/// Makers only until this many orders have gone out per market, so the first
/// takers find a book to cross rather than cancelling against nothing.
const WARMUP_ORDERS: u64 = 200;

#[derive(Clone, Debug)]
pub struct RunConfig {
    pub sequencer: SocketAddr,
    pub chain_id: u32,
    pub num_traders: u32,
    /// First trader index to derive. Workers running side by side each take a
    /// disjoint range, because a trader's nonce cannot be shared between
    /// processes.
    pub trader_offset: u32,
    pub seed_prefix: String,
    /// Target submission rate (txs per second). The runner sleeps between submits to hit it.
    pub tx_per_sec: u64,
    /// Stop after this many submits. `None` runs forever (until SIGINT).
    pub max_txs: Option<u64>,
    /// PRNG seed for deterministic (otherwise reproducible) order generation.
    pub rng_seed: u64,
    /// Share of submissions that are spot orders, 0..=100. The rest are transfers.
    pub orders_pct: u8,
    /// Ceiling on submits awaiting a reply. When reached the runner waits for
    /// replies instead of sending, which is the backpressure that makes the
    /// accepted rate a statement about the node rather than about this loop.
    ///
    /// It also bounds the blast radius of one rejection. A trader's nonces are
    /// pipelined, so a transaction the executor refuses — a self-trade, say —
    /// strands every submission already in flight behind it on `BadNonce`.
    /// Divided across the worker's traders, this is how many that can be. At
    /// 4,000 over 10 traders the measured rejection rate was 43% and
    /// throughput collapsed to a twentieth; at 1,500 over 16 it is 0.004%.
    pub max_in_flight: usize,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            sequencer: "127.0.0.1:7700".parse().unwrap(),
            chain_id: 1,
            num_traders: 8,
            trader_offset: 0,
            seed_prefix: "cb-sim-trader-".to_string(),
            tx_per_sec: 50,
            max_txs: None,
            rng_seed: 0xC0FFEE,
            orders_pct: 0,
            max_in_flight: 1_500,
        }
    }
}

/// Price and size conventions for one spot market, matching the rows in
/// `genesis-full.jsonl`. The executor's notional is `price * qty / 1e8`
/// whatever the base asset's decimals, so:
///
/// * BTC/USD (8-decimal base) prices are USD base units (6 decimals) per whole
///   BTC: `50_000 * 1e6` is $50,000. Tick $0.10, lot 1e-5 BTC.
/// * ETH/USD (18-decimal base) cannot express a real price: the smallest
///   representable price, 1, is $10,000 per ETH. The gateway omits the market
///   from the web app for this reason. Load here prices it synthetically —
///   the numbers move the matcher and the books, not a believable chart.
#[derive(Clone, Copy, Debug)]
struct SpotMarket {
    id: MarketId,
    tick_size: u128,
    /// Order quantity in base units; a multiple of the market's `min_qty`.
    order_qty: u128,
    /// Mid price in the market's price unit; a multiple of `tick_size`.
    mid_price: u128,
}

impl SpotMarket {
    const BTC: SpotMarket = SpotMarket {
        id: MarketId::BtcUsd,
        tick_size: 100_000,
        order_qty: 100_000, // 0.001 BTC: $50 notional at $50,000
        mid_price: 50_000 * 1_000_000,
    };
    const ETH: SpotMarket = SpotMarket {
        id: MarketId::EthUsd,
        tick_size: 1,
        order_qty: 10_000_000_000_000, // 1e13 wei: $10 notional at the synthetic mid
        mid_price: 100,
    };

    fn best_bid(&self) -> u128 {
        self.mid_price - self.tick_size
    }
    fn best_ask(&self) -> u128 {
        self.mid_price + self.tick_size
    }
    /// A maker's resting price, `level` ticks behind the touch.
    fn resting_price(&self, side: Side, level: u128) -> u128 {
        match side {
            Side::Buy => self.best_bid() - self.tick_size * level,
            Side::Sell => self.best_ask() + self.tick_size * level,
        }
    }
    /// A taker's limit, through every resting level on the far side.
    fn crossing_price(&self, side: Side) -> u128 {
        let span = self.tick_size * (PRICE_LEVELS + 2);
        match side {
            Side::Buy => self.best_ask() + span,
            Side::Sell => self.best_bid().saturating_sub(span).max(self.tick_size),
        }
    }
}

/// Counters shared between the submit loop and the reply tasks.
#[derive(Default)]
struct Stats {
    accepted: Cell<u64>,
    rejected: Cell<u64>,
    in_flight: Cell<usize>,
    /// Rejections by the node's error code, for the periodic stats line.
    rejected_by_code: RefCell<BTreeMap<String, u64>>,
}

/// Per-trader bookkeeping the reply tasks share with the submit loop.
#[derive(Default)]
struct TraderFlight {
    in_flight: Cell<usize>,
    /// Set when a submission was rejected: the local nonce is ahead of the
    /// chain and must be re-read once nothing is pending.
    stale: Cell<bool>,
}

/// Even trader indices rest orders, odd ones cross them. A taker therefore
/// never meets its own order on the book.
fn is_maker(trader_index: u32) -> bool {
    trader_index % 2 == 0
}

pub async fn run(cfg: RunConfig) -> anyhow::Result<()> {
    anyhow::ensure!(cfg.num_traders >= 2, "need at least 2 traders");
    anyhow::ensure!(cfg.tx_per_sec > 0, "tx_per_sec must be > 0");
    anyhow::ensure!(cfg.orders_pct <= 100, "orders_pct is a percentage, 0..=100");
    anyhow::ensure!(cfg.max_in_flight >= 1, "max_in_flight must be >= 1");

    let mut traders = TraderSet::derive_range(&cfg.seed_prefix, cfg.trader_offset, cfg.num_traders);
    tracing::info!(
        traders = traders.len(),
        first_trader = cfg.trader_offset,
        sequencer = %cfg.sequencer,
        tx_per_sec = cfg.tx_per_sec,
        orders_pct = cfg.orders_pct,
        max_in_flight = cfg.max_in_flight,
        "simulator starting"
    );
    for t in &traders.traders {
        tracing::info!(index = t.index, addr = %t.addr, "trader");
    }

    let client = cb_rpc::connect(cfg.sequencer).await?;

    // Sync each trader's nonce from the sequencer so re-runs against an existing chain don't
    // submit stale (BadNonce) txs that would stall block production.
    for t in traders.traders.iter_mut() {
        t.nonce = fetch_nonce(&client, &t.addr).await?;
    }
    tracing::info!("synced trader nonces from sequencer");

    // A taker must never have a resting order, or its next crossing order is
    // a self-trade the executor rejects. An earlier run against this chain
    // may have left some (the roles were not always split), so every taker
    // clears both books once before the load starts. These go out in order
    // and are awaited, so the nonces below start from a settled point.
    if cfg.orders_pct > 0 {
        let mut cleared = 0u32;
        for t in traders.traders.iter_mut().filter(|t| !is_maker(t.index)) {
            for market in [MarketId::BtcUsd, MarketId::EthUsd] {
                // A previous population may have been killed with this
                // trader's transactions still queued in the mempool under
                // nonces the chain has not reached; the nonce read from state
                // is then stale and the cancel is refused. Re-read and retry
                // for a few seconds rather than exiting on the first refusal,
                // which is what a worker launched right after another was
                // stopped used to do.
                let mut attempts = 0u32;
                loop {
                    let tx = Transaction {
                        nonce: t.nonce,
                        chain_id: cfg.chain_id,
                        body: TxBody::BatchCancel { market },
                    };
                    let signed = SignedTx::sign_with(&t.sk, tx)?;
                    let mut req = client.cap.submit_tx_request();
                    signed_tx_to_capnp(req.get().init_tx(), &signed);
                    let resp = req.send().promise.await?;
                    let ok = matches!(
                        resp.get().and_then(|g| g.get_result()).map(|r| r.which()),
                        Ok(Ok(submit_result::Ok(())))
                    );
                    if ok {
                        break;
                    }
                    attempts += 1;
                    anyhow::ensure!(
                        attempts < 40,
                        "clearing taker {} on {market:?} was refused {attempts} times; \
                         another population is probably still trading these accounts",
                        t.index
                    );
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    t.nonce = fetch_nonce(&client, &t.addr).await?;
                }
                t.nonce += 1;
                cleared += 1;
            }
        }
        tracing::info!(cancels = cleared, "takers cleared their resting orders");
    }

    let mut rng = StdRng::seed_from_u64(cfg.rng_seed);
    let interval = Duration::from_micros(1_000_000 / cfg.tx_per_sec);
    let mut submitted: u64 = 0;
    let mut orders_sent: u64 = 0;
    let mut buys: u64 = 0;
    let mut sells: u64 = 0;
    let mut orders_per_market = [0u64; 2];
    let mut orders_by_trader: Vec<u32> = vec![0; traders.len()];
    let session_start = std::time::Instant::now();
    let mut next_tick = tokio::time::Instant::now();
    // Pipeline submits: don't block the next-tx send on the previous reply.
    // Capnp multiplexes promises over the single TCP stream and each spawned
    // task drives its own promise to completion (or error). We deliberately
    // increment nonce before sending so concurrent in-flight submits from the
    // same trader still have distinct nonces.
    let stats = Rc::new(Stats::default());
    let flight: Rc<Vec<TraderFlight>> = Rc::new(
        (0..traders.len())
            .map(|_| TraderFlight::default())
            .collect(),
    );

    loop {
        if let Some(cap) = cfg.max_txs {
            if submitted >= cap {
                break;
            }
        }
        next_tick += interval;
        tokio::time::sleep_until(next_tick).await;
        // Backpressure: hold the loop until replies drain below the ceiling.
        while stats.in_flight.get() >= cfg.max_in_flight {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_micros(200)).await;
        }

        let n = traders.len();
        let market_ix = usize::from(rng.gen_bool(0.5));
        let market = [SpotMarket::BTC, SpotMarket::ETH][market_ix];
        let side = if rng.gen_bool(0.5) {
            Side::Buy
        } else {
            Side::Sell
        };
        let is_order = rng.gen_range(0..100u8) < cfg.orders_pct;
        // Makers only while a market's book is empty, so the first takers
        // find something to cross rather than cancelling against nothing.
        let warming_up = is_order && orders_per_market[market_ix] < WARMUP_ORDERS;

        // Pick a sender that is ready: not stale with submits still pending,
        // and a maker during warm-up. A stale trader whose pipeline has
        // drained gets its nonce re-read here and rejoins.
        let mut sender_ix = None;
        for _ in 0..(4 * n) {
            let ix = rng.gen_range(0..n);
            if warming_up && !is_maker(traders.traders[ix].index) {
                continue;
            }
            let f = &flight[ix];
            if f.stale.get() {
                if f.in_flight.get() > 0 {
                    continue;
                }
                let addr = traders.traders[ix].addr;
                traders.traders[ix].nonce = fetch_nonce(&client, &addr).await?;
                f.stale.set(false);
            }
            sender_ix = Some(ix);
            break;
        }
        let Some(sender_ix) = sender_ix else {
            // Everyone is draining; let replies arrive.
            tokio::time::sleep(Duration::from_millis(1)).await;
            continue;
        };

        let body = if is_order {
            orders_sent += 1;
            orders_per_market[market_ix] += 1;
            match side {
                Side::Buy => buys += 1,
                Side::Sell => sells += 1,
            }
            orders_by_trader[sender_ix] += 1;
            let maker = is_maker(traders.traders[sender_ix].index);
            if maker && orders_by_trader[sender_ix] % CANCEL_EVERY == 0 {
                TxBody::BatchCancel { market: market.id }
            } else {
                let (price, tif) = if maker {
                    let level = rng.gen_range(0..PRICE_LEVELS);
                    (market.resting_price(side, level), Tif::Gtc)
                } else {
                    (market.crossing_price(side), Tif::Ioc)
                };
                TxBody::PlaceOrder {
                    market: market.id,
                    side,
                    price,
                    qty: market.order_qty,
                    tif,
                    post_only: false,
                    client_order_id: client_id(submitted),
                }
            }
        } else {
            let mut receiver_ix = rng.gen_range(0..n);
            while receiver_ix == sender_ix {
                receiver_ix = rng.gen_range(0..n);
            }
            // "buy"  leg = buyer pays quote (USD) to the seller.
            // "sell" leg = seller delivers base (BTC/ETH) to the buyer.
            let (asset, amount) = match side {
                Side::Buy => {
                    let qty_quote = rng.gen_range(1_000u128..=1_000_000u128);
                    (market.id.quote(), qty_quote)
                }
                Side::Sell => {
                    // Scale the random base-unit transfer by the asset's own
                    // decimals so the notional is comparable across markets.
                    // Upper bound is "1e-4 of a whole unit", floored at 1.
                    let base_asset = market.id.base();
                    let span = (base_asset.one_whole() / 10_000).max(1);
                    (market.id.base(), rng.gen_range(1u128..=span))
                }
            };
            TxBody::Transfer {
                to: traders.traders[receiver_ix].addr,
                asset,
                amount,
            }
        };

        let sender = &mut traders.traders[sender_ix];
        let nonce = sender.nonce;
        sender.nonce += 1; // optimistic — see module doc
        let tx = Transaction {
            nonce,
            chain_id: cfg.chain_id,
            body,
        };
        let signed = SignedTx::sign_with(&sender.sk, tx)?;

        // Build + fire the request without awaiting the reply on the hot path.
        let mut req = client.cap.submit_tx_request();
        signed_tx_to_capnp(req.get().init_tx(), &signed);
        let promise = req.send().promise;
        stats.in_flight.set(stats.in_flight.get() + 1);
        flight[sender_ix]
            .in_flight
            .set(flight[sender_ix].in_flight.get() + 1);
        let stats_clone = stats.clone();
        let flight_clone = flight.clone();
        tokio::task::spawn_local(async move {
            // `Ok(None)` is accepted; `Ok(Some(code))` is a rejection the node
            // explained; `Err` is a transport or decode failure.
            let outcome: Result<Option<String>, ()> = match promise.await {
                Ok(resp) => match resp.get().and_then(|g| g.get_result()) {
                    Ok(r) => match r.which() {
                        Ok(submit_result::Ok(())) => Ok(None),
                        Ok(submit_result::Rejected(err)) => Ok(Some(
                            err.ok()
                                .and_then(|e| e.get_code().ok())
                                .map(|c| format!("{c:?}"))
                                .unwrap_or_else(|| "unreadable".to_string()),
                        )),
                        Err(_) => Err(()),
                    },
                    Err(_) => Err(()),
                },
                Err(_) => Err(()),
            };
            match outcome {
                Ok(None) => stats_clone.accepted.set(stats_clone.accepted.get() + 1),
                other => {
                    let code = match other {
                        Ok(Some(code)) => code,
                        _ => "transport".to_string(),
                    };
                    stats_clone.rejected.set(stats_clone.rejected.get() + 1);
                    *stats_clone
                        .rejected_by_code
                        .borrow_mut()
                        .entry(code)
                        .or_insert(0) += 1;
                    flight_clone[sender_ix].stale.set(true);
                }
            }
            stats_clone.in_flight.set(stats_clone.in_flight.get() - 1);
            flight_clone[sender_ix]
                .in_flight
                .set(flight_clone[sender_ix].in_flight.get() - 1);
        });
        submitted += 1;
        if submitted % 1000 == 0 {
            tracing::info!(
                submitted,
                accepted = stats.accepted.get(),
                rejected = stats.rejected.get(),
                orders = orders_sent,
                buys,
                sells,
                btc_orders = orders_per_market[0],
                eth_orders = orders_per_market[1],
                in_flight = stats.in_flight.get(),
                rejected_by_code = ?stats.rejected_by_code.borrow(),
                "stats"
            );
        }
    }

    // Let the tail of the pipeline answer before reporting.
    let drain_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while stats.in_flight.get() > 0 && tokio::time::Instant::now() < drain_deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let (accepted, rejected) = (stats.accepted.get(), stats.rejected.get());
    let elapsed_s = session_start.elapsed().as_secs_f64();
    let sustained_tps = if elapsed_s > 0.0 {
        accepted as f64 / elapsed_s
    } else {
        0.0
    };
    // Grep-friendly final line. The Makefile summary in `startmultisimulation`
    // pulls this with `grep sustained_tps`. Pass floats directly so the
    // tracing fmt subscriber prints `sustained_tps=1234.5` without quotes.
    tracing::info!(
        sustained_tps,
        elapsed_s,
        accepted,
        submitted,
        "sustained_tps"
    );
    tracing::info!(
        submitted,
        accepted,
        rejected,
        orders = orders_sent,
        buys,
        sells,
        rejected_by_code = ?stats.rejected_by_code.borrow(),
        "simulator finished"
    );
    Ok(())
}

async fn fetch_nonce(
    client: &cb_rpc::client::Client,
    addr: &cb_types::Address,
) -> anyhow::Result<u64> {
    let mut req = client.cap.get_account_request();
    address_to_capnp(req.get().init_addr(), addr);
    let resp = req.send().promise.await?;
    Ok(resp.get()?.get_state()?.get_nonce())
}

/// Deterministic per-order client id, so two runs with the same seed produce
/// the same transactions.
fn client_id(counter: u64) -> H256 {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&counter.to_be_bytes());
    H256(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_are_tick_aligned_and_sides_cross() {
        for m in [SpotMarket::BTC, SpotMarket::ETH] {
            assert_eq!(m.mid_price % m.tick_size, 0);
            for level in 0..PRICE_LEVELS {
                assert_eq!(m.resting_price(Side::Buy, level) % m.tick_size, 0);
                assert_eq!(m.resting_price(Side::Sell, level) % m.tick_size, 0);
                assert!(m.resting_price(Side::Buy, level) < m.resting_price(Side::Sell, level));
            }
            // A crossing buy reaches the deepest resting ask and vice versa.
            assert!(m.crossing_price(Side::Buy) >= m.resting_price(Side::Sell, PRICE_LEVELS - 1));
            assert!(m.crossing_price(Side::Sell) <= m.resting_price(Side::Buy, PRICE_LEVELS - 1));
            assert!(m.crossing_price(Side::Sell) >= m.tick_size);
        }
    }

    #[test]
    fn makers_and_takers_are_disjoint_populations() {
        // A taker never has a resting order to cross, whatever the offset a
        // worker starts at.
        for offset in [0u32, 16, 32, 48] {
            let set = TraderSet::derive_range("cb-sim-trader-", offset, 16);
            let makers = set.traders.iter().filter(|t| is_maker(t.index)).count();
            assert_eq!(makers, 8);
        }
    }

    #[test]
    fn quantities_match_genesis_lots() {
        // genesis-full.jsonl: btc_usd min_qty 1000 (eth_usd's min_qty is 1,
        // which every quantity satisfies).
        assert_eq!(SpotMarket::BTC.order_qty % 1_000, 0);
        const { assert!(SpotMarket::ETH.order_qty > 0) };
        // Notional = price * qty / 1e8 must not underflow to zero.
        for m in [SpotMarket::BTC, SpotMarket::ETH] {
            assert!(m.mid_price * m.order_qty / 100_000_000 > 0);
        }
    }

    // ---------------- properties of the load generator's arithmetic ----------------

    use crate::traders::TraderSet;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Every price the generator can emit is on the tick grid and
        /// positive, bids stay strictly below asks at every depth, and a
        /// deeper level is strictly further from the touch. An off-tick
        /// price is a rejected transaction, which the chain counts against
        /// throughput and the stats line would blame on the node.
        #[test]
        fn every_emitted_price_is_on_the_grid_and_uncrossed(level in 0..PRICE_LEVELS, btc in any::<bool>()) {
            let m = if btc { SpotMarket::BTC } else { SpotMarket::ETH };
            let bid = m.resting_price(Side::Buy, level);
            let ask = m.resting_price(Side::Sell, level);
            prop_assert!(bid > 0 && bid % m.tick_size == 0);
            prop_assert!(ask % m.tick_size == 0);
            prop_assert!(bid < ask);
            if level > 0 {
                prop_assert!(bid < m.resting_price(Side::Buy, level - 1));
                prop_assert!(ask > m.resting_price(Side::Sell, level - 1));
            }
            for side in [Side::Buy, Side::Sell] {
                let c = m.crossing_price(side);
                prop_assert!(c > 0 && c % m.tick_size == 0);
            }
        }

        /// Parallel workers take disjoint index ranges; their traders must be
        /// disjoint accounts, or two workers would race one nonce and each
        /// see the other's transactions rejected.
        #[test]
        fn disjoint_ranges_derive_disjoint_accounts(a in 0u32..500, n in 1u32..24, gap in 0u32..50) {
            let first = TraderSet::derive_range("prop-sim-", a, n);
            let second = TraderSet::derive_range("prop-sim-", a + n + gap, n);
            prop_assert_eq!(first.len(), n as usize);
            for (i, t) in first.traders.iter().enumerate() {
                prop_assert_eq!(t.index, a + i as u32);
                prop_assert_eq!(t.nonce, 0);
                prop_assert!(second.traders.iter().all(|u| u.addr != t.addr));
            }
            // Deterministic: the same range derives the same accounts.
            let again = TraderSet::derive_range("prop-sim-", a, n);
            for (x, y) in first.traders.iter().zip(&again.traders) {
                prop_assert_eq!(x.addr, y.addr);
            }
        }

        /// Client order ids are injective in the counter, so no two orders a
        /// run submits collide on the idempotency key.
        #[test]
        fn client_ids_are_distinct_per_counter(a in any::<u64>(), b in any::<u64>()) {
            prop_assert_eq!(client_id(a) == client_id(b), a == b);
        }

        /// Makers and takers alternate, so any window of consecutive traders
        /// is split evenly (to within one).
        #[test]
        fn makers_and_takers_alternate(start in any::<u32>(), len in 0u32..1000) {
            let end = start.saturating_add(len);
            let makers = (start..end).filter(|i| is_maker(*i)).count() as i64;
            let takers = (end - start) as i64 - makers;
            prop_assert!((makers - takers).abs() <= 1);
        }
    }
}
