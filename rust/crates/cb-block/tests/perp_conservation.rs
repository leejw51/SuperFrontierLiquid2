//! Derivative executor invariants under a random multi-account workload.
//!
//! Eight traders with uneven collateral, a whale quoting both sides, a keeper
//! and the proposer's funding ticks, on a perpetual and a dated future whose
//! oracle prices random-walk two per cent a block. Traders place limit, IOC,
//! FOK, post-only and reduce-only orders, cancel, and close positions; the
//! keeper liquidates whoever the committed state says is liquidatable. The
//! invariants are checked after **every transaction**, through the block's
//! staged view, not just at block ends:
//!
//! 1. **Value conservation, exactly up to rounding.** The value of the whole
//!    derivatives ledger is
//!
//!    ```text
//!    V = Σ (collateral − bad_debt)            every margin account, incl. insurance
//!      − Σ side·qty·entry_px                  cost basis of every open position
//!      − Σ side·qty·(cumulative − stamp)      funding accrued and not yet settled
//!    ```
//!
//!    and because positions net to zero per market, the first mark-to-market
//!    term of the unrealised PnL cancels and `V` does not depend on any price.
//!    Every fill, fee, rebate, liquidation fee, bad-debt absorption and funding
//!    settlement is a transfer inside `V`; only integer truncation may move it,
//!    by at most a few micro-dollars per fill and per position. A larger move
//!    is value created or destroyed. This invariant found the closed-lot
//!    funding leak fixed alongside it (`settle_closed_lot_funding`).
//! 2. **Positions net to zero on every market.**
//! 3. **Reduce-only never grows a position.** A successful reduce-only order
//!    or `ClosePosition` leaves its sender's |position| no larger, on the same
//!    side or flat.
//! 4. **Margin rows are canonical**: never both collateral and bad debt.
//! 5. **A flat account with debt requires a halted market.** Live positions
//!    retain cash liabilities until close-out. A flat account's debt is
//!    eligible for insurance; an uncovered residual halts its market. A new
//!    halt must therefore be backed by outstanding debt.
//! 6. **Funding ticks** move the cumulative index by exactly
//!    `clamp(rate) · elapsed / interval`, and at a boundary each position is
//!    charged exactly `trunc(qty · (cumulative − stamp) / 1e9)` toward zero —
//!    the rounding dust belongs to nobody and is at most one micro-dollar per
//!    position, which the test also pins.

mod support;

use cb_block::ExecError;
use cb_engine::{
    accrue_index_delta, compute_funding_rate, is_liquidatable, FundingClamp, DEFAULT_TIERS,
    RATE_DEN_Q9,
};
use cb_state::{Database, FundingIndices, FuturesMarkets, OracleState, PerpMarkets, PerpOrders};
use cb_tx::TxBody;
use cb_types::{Address, DerivId, Side, Tif};
use std::collections::BTreeMap;
use support::*;

const FUT: DerivId = DerivId::future(0);
const MARKETS: [DerivId; 2] = [PERP, FUT];

/// `V` of the module header, in units of 1e-10 micro-USD (the raw scale of
/// `qty * px_q8`), so nothing in it is rounded.
fn ledger_value(db: &Database) -> i128 {
    let mut v: i128 = margins_in(db)
        .iter()
        .map(|m| (m.collateral as i128 - m.bad_debt as i128) * 10_000_000_000)
        .sum();
    let idx = FundingIndices::new(db);
    for p in positions_in(db) {
        let s = signed_qty(&p);
        v -= s * p.entry_px_q8 as i128;
        let cur = idx
            .get(&p.market)
            .map(|i| i.cumulative.to_i128())
            .unwrap_or(0);
        // qty · Δindex / 1e9 micro = qty · Δindex · 10 raw units.
        v -= s * (cur - p.entry_funding_index.to_i128()) * 10;
    }
    v
}

fn net_by_market(db: &Database) -> BTreeMap<DerivId, i128> {
    let mut out = BTreeMap::new();
    for p in positions_in(db) {
        *out.entry(p.market).or_insert(0) += signed_qty(&p);
    }
    out
}

fn resting_count(db: &Database) -> usize {
    MARKETS
        .iter()
        .map(|m| PerpOrders::new(db).list_for_market(m).len())
        .sum()
}

fn halted(db: &Database) -> bool {
    PerpMarkets::new(db)
        .get(&PERP)
        .unwrap()
        .paused_reduce_only_only
        || FuturesMarkets::new(db)
            .get(&FUT)
            .unwrap()
            .paused_reduce_only_only
}

struct World {
    chain: Chain,
    traders: Vec<Actor>,
    whale: Actor,
    keeper: Actor,
    px: BTreeMap<DerivId, u128>,
}

fn world(seed: u64, insurance: u128) -> World {
    let mut rng = Rng::new(seed ^ 0xABCD);
    let chain = Chain::new();
    chain.list_perp(perp_market(PERP, chain.oracle.addr));
    chain.list_future(future_market(FUT, chain.oracle.addr, 1_000_000));
    let traders: Vec<Actor> = (0..8)
        .map(|i| actor(&format!("perp-cons-{seed}-{i}")))
        .collect();
    for t in &traders {
        chain.fund_margin(t.addr, rng.range(3_000, 150_000) * MICRO);
    }
    let whale = actor("perp-cons-whale");
    chain.fund_margin(whale.addr, 1_000_000_000 * MICRO);
    let keeper = actor("perp-cons-keeper");
    chain.fund_margin(keeper.addr, 0);
    chain.fund_margin(Address::INSURANCE, insurance);
    let px = MARKETS.iter().map(|m| (*m, 50_000 * Q8)).collect();
    World {
        chain,
        traders,
        whale,
        keeper,
        px,
    }
}

/// One block's intents: funding tick, oracle posts, the whale's fresh quotes,
/// keeper triggers for whoever is liquidatable now, then random trading.
fn block_intents(w: &World, rng: &mut Rng) -> Vec<(Actor, TxBody)> {
    let db = &w.chain.db;
    let h = w.chain.height + 1;
    let mut txs: Vec<(Actor, TxBody)> = vec![(w.chain.proposer.clone(), tick(PERP))];
    for m in MARKETS {
        txs.push((w.chain.oracle.clone(), oracle(m, w.px[&m], h)));
    }
    for o in PerpOrders::new(db).list_for_owner(&w.whale.addr) {
        txs.push((
            w.whale.clone(),
            TxBody::CancelPerpOrder {
                order_id: o.order_id,
            },
        ));
    }
    for m in MARKETS {
        let mark = w.px[&m];
        for bps in [30u128, 80, 200] {
            let q = rng.range(ONE_BTC / 2, 3 * ONE_BTC);
            txs.push((
                w.whale.clone(),
                perp_order(
                    m,
                    Side::Buy,
                    mark * (10_000 - bps) / 10_000,
                    q,
                    Tif::Gtc,
                    false,
                    false,
                ),
            ));
            txs.push((
                w.whale.clone(),
                perp_order(
                    m,
                    Side::Sell,
                    mark * (10_000 + bps) / 10_000,
                    q,
                    Tif::Gtc,
                    false,
                    false,
                ),
            ));
        }
    }
    // Keeper: anyone liquidatable on committed state, on any market held.
    for t in &w.traders {
        let held = cb_state::Positions::new(db).list_for_owner(&t.addr);
        let marks: Vec<(DerivId, u128)> = MARKETS
            .iter()
            .filter_map(|m| mark_in(db, *m).map(|x| (*m, x)))
            .collect();
        if is_liquidatable(&margin_in(db, t.addr), &held, &marks, DEFAULT_TIERS) {
            for p in held.iter().filter(|p| p.qty > 0) {
                txs.push((
                    w.keeper.clone(),
                    TxBody::LiquidationTrigger {
                        owner: t.addr,
                        market: p.market,
                    },
                ));
            }
        }
    }
    for _ in 0..(8 + rng.below(12)) {
        let t = &w.traders[rng.below(w.traders.len() as u64) as usize];
        let m = MARKETS[rng.below(2) as usize];
        let mark = w.px[&m];
        let body = match rng.below(20) {
            0 | 1 => {
                let mine = PerpOrders::new(db).list_for_owner(&t.addr);
                if mine.is_empty() {
                    continue;
                }
                TxBody::CancelPerpOrder {
                    order_id: mine[rng.below(mine.len() as u64) as usize].order_id,
                }
            }
            2 => TxBody::ClosePosition {
                market: m,
                qty: if rng.chance(50) {
                    0
                } else {
                    rng.range(1, ONE_BTC)
                },
                max_slippage_bps: rng.range(10, 400) as u16,
            },
            _ => {
                let side = rng.side();
                let off = rng.range(0, 150);
                // Mostly marketable-ish, sometimes passive.
                let px = if rng.chance(50) {
                    match side {
                        Side::Buy => mark * (10_000 + off) / 10_000,
                        Side::Sell => mark * (10_000 - off) / 10_000,
                    }
                } else {
                    match side {
                        Side::Buy => mark * (10_000 - off) / 10_000,
                        Side::Sell => mark * (10_000 + off) / 10_000,
                    }
                };
                let tif = match rng.below(5) {
                    0 => Tif::Ioc,
                    1 => Tif::Fok,
                    _ => Tif::Gtc,
                };
                perp_order(
                    m,
                    side,
                    px,
                    rng.range(ONE_BTC / 100, 2 * ONE_BTC),
                    tif,
                    rng.chance(10),
                    rng.chance(20),
                )
            }
        };
        txs.push((t.clone(), body));
    }
    txs
}

#[derive(Default, Debug)]
struct Tally {
    outcomes: BTreeMap<String, usize>,
    max_drift_micro: f64,
    /// Σ |value moved by boundary settlements|, for the report.
    boundary_flow_micro: i128,
    liquidations: usize,
    halted: bool,
    funding_moved: bool,
}

fn run(seed: u64, blocks: u64, insurance: u128, step_bps: u128) -> Tally {
    let mut w = world(seed, insurance);
    let mut rng = Rng::new(seed);
    let mut tally = Tally::default();
    for _ in 0..blocks {
        // Random walk the oracle, two per cent a block at most.
        for m in MARKETS {
            let p = w.px[&m];
            let step = rng.range(0, 2 * step_bps);
            let next = if step >= step_bps {
                p * (10_000 + step - step_bps) / 10_000
            } else {
                p * (10_000 - (step_bps - step)) / 10_000
            };
            w.px.insert(m, next.clamp(20_000 * Q8, 120_000 * Q8));
        }
        let intents = block_intents(&w, &mut rng);
        let height = w.chain.height + 1;

        // Per-transaction snapshots taken by the observer.
        let mut v_before = 0i128;
        let mut resting_before = 0usize;
        let mut halted_before = false;
        let mut pos_before: BTreeMap<(Address, DerivId), cb_types::Position> = BTreeMap::new();
        let mut funding_before: Option<(i128, u64, i64)> = None;
        let mut tick_total_before = 0i128;

        let bodies: Vec<TxBody> = intents.iter().map(|(_, b)| b.clone()).collect();
        let senders: Vec<Address> = intents.iter().map(|(a, _)| a.addr).collect();
        let results = w.chain.block_with(&intents, |db, i, r| {
            let body = &bodies[i];
            match r {
                None => {
                    v_before = ledger_value(db);
                    resting_before = resting_count(db);
                    halted_before = halted(db);
                    pos_before = positions_in(db).into_iter().map(|p| ((p.owner, p.market), p)).collect();
                    if let TxBody::FundingTick { market } = body {
                        let idx = FundingIndices::new(db).get(market);
                        let prem = OracleState::new(db).get_mark(market).map(|m| m.premium_ema_q9).unwrap_or(0);
                        funding_before = Some((
                            idx.as_ref().map(|i| i.cumulative.to_i128()).unwrap_or(0),
                            idx.as_ref().map(|i| i.last_tick_height).unwrap_or(0),
                            prem,
                        ));
                        tick_total_before = margins_in(db).iter().map(|m| m.collateral as i128 - m.bad_debt as i128).sum();
                    }
                }
                Some(res) => {
                    let ok = res.is_ok();
                    // 1. Conservation.
                    let v_after = ledger_value(db);
                    let drift = (v_after - v_before).abs();
                    let positions = positions_in(db).len();
                    let bound_micro = 4 * (resting_before as i128 + 3) + 2 * positions as i128 + 2;
                    assert!(
                        drift <= bound_micro * 10_000_000_000,
                        "seed {seed} h{height} tx{i} {body:?} -> {res:?}: ledger moved by {} micro (bound {bound_micro})",
                        drift as f64 / 1e10
                    );
                    tally.max_drift_micro = tally.max_drift_micro.max(drift as f64 / 1e10);
                    // 2. Netting.
                    for (m, n) in net_by_market(db) {
                        assert_eq!(n, 0, "seed {seed} h{height} tx{i}: market {m} nets to {n}");
                    }
                    // 3. Reduce-only.
                    let ro_market = match body {
                        TxBody::PlacePerpOrder { reduce_only: true, market, .. } => Some(*market),
                        TxBody::ClosePosition { market, .. } => Some(*market),
                        _ => None,
                    };
                    if let (true, Some(m)) = (ok, ro_market) {
                        let who = senders[i];
                        let (s0, q0) = pos_before.get(&(who, m)).map(|p| (p.side, p.qty)).expect("a reduce-only order succeeded against no position");
                        if let Some(p) = position_in(db, who, m) {
                            assert!(p.qty <= q0 && p.side == s0, "seed {seed} h{height} tx{i}: reduce-only grew or flipped {q0} -> {p:?}");
                        }
                    }
                    // 4 and 5.
                    let margins = margins_in(db);
                    let mut debt = false;
                    let mut flat_debt = false;
                    let live_positions = positions_in(db);
                    for mrow in &margins {
                        assert!(!(mrow.collateral > 0 && mrow.bad_debt > 0), "seed {seed} h{height}: non-canonical {mrow:?}");
                        if mrow.bad_debt > 0 {
                            assert_ne!(mrow.owner, Address::INSURANCE, "the fund never owes itself");
                            debt = true;
                            if !live_positions.iter().any(|p| p.owner == mrow.owner && p.qty > 0) {
                                flat_debt = true;
                            }
                        }
                    }
                    let halted_now = halted(db);
                    if flat_debt {
                        assert!(halted_now, "seed {seed} h{height} tx{i}: flat-account debt outstanding with no market halted");
                    }
                    if halted_now && !halted_before {
                        assert!(debt, "seed {seed} h{height} tx{i}: a market halted with no bad debt behind it");
                        tally.halted = true;
                    }
                    if ok && matches!(body, TxBody::LiquidationTrigger { .. }) {
                        tally.liquidations += 1;
                    }
                    // 6. Funding.
                    if let (true, TxBody::FundingTick { market }, Some((cum0, last0, prem))) = (ok, body, funding_before.take()) {
                        let pm = PerpMarkets::new(db).get(market).unwrap();
                        let idx = FundingIndices::new(db).get(market).unwrap();
                        let cap = pm.funding_rate_cap_q9.unsigned_abs() as i64;
                        let rate = compute_funding_rate(prem, 0, FundingClamp::DEFAULT).clamp(-cap, cap);
                        assert_eq!(idx.rate_q9, rate, "seed {seed} h{height}: tick rate");
                        let elapsed = if last0 == 0 { 0 } else { height - last0 };
                        let interval = pm.funding_interval_blocks.max(1) as u64;
                        // A tick does not move the mark, so the one read now
                        // is the one the tick weighted the rate by.
                        let mark = OracleState::new(db).get_mark(market).map(|m| m.px_q8).unwrap_or(0);
                        assert_eq!(
                            idx.cumulative.to_i128(),
                            accrue_index_delta(cb_types::Signed::from_i128(cum0), rate, mark, elapsed, interval).to_i128(),
                            "seed {seed} h{height}: index accrual"
                        );
                        if height % interval == 0 {
                            // Every position on the market is re-stamped at
                            // the live index, so nothing it owed is left
                            // accrued; and the whole ledger moved by no more
                            // than the truncation of each position's charge.
                            let cur = idx.cumulative.to_i128();
                            for p in positions_in(db).iter().filter(|p| p.market == *market) {
                                assert_eq!(p.entry_funding_index.to_i128(), cur, "seed {seed} h{height}: position not re-stamped at the boundary");
                            }
                            // Each position pays (or is paid) exactly its own
                            // charge, truncated toward zero; absorption of any
                            // resulting debt is a transfer, so the sum over
                            // every account moves by exactly the sum of the
                            // truncated charges. Not zero: lots that closed
                            // earlier in the interval already paid their share
                            // when they closed.
                            let expected: i128 = pos_before
                                .values()
                                .filter(|p| p.market == *market)
                                .map(|p| -signed_qty(p) * (cur - p.entry_funding_index.to_i128()) / RATE_DEN_Q9 as i128)
                                .sum();
                            let after: i128 = margins_in(db).iter().map(|m| m.collateral as i128 - m.bad_debt as i128).sum();
                            assert_eq!(after - tick_total_before, expected, "seed {seed} h{height}: boundary settlement");
                            tally.boundary_flow_micro += (after - tick_total_before).abs();
                            if after != tick_total_before || rate != 0 {
                                tally.funding_moved = true;
                            }
                        }
                    }
                }
            }
        });
        for r in results {
            let k = match r {
                Ok(()) => "ok".into(),
                Err(e) => match e {
                    ExecError::BadNonce { .. } => "BadNonce".into(),
                    other => format!("{other:?}"),
                },
            };
            *tally.outcomes.entry(k).or_default() += 1;
        }
    }
    tally
}

#[test]
fn random_perp_trading_conserves_the_ledger_and_keeps_every_invariant() {
    for seed in 1..=3u64 {
        let t = run(seed, 70, 5_000_000 * MICRO, 200);
        assert!(
            t.outcomes.get("ok").copied().unwrap_or(0) > 1_000,
            "seed {seed}: {t:?}"
        );
        assert!(
            t.funding_moved,
            "seed {seed}: funding never moved, so invariant 6 is vacuous: {t:?}"
        );
        assert!(
            t.max_drift_micro < 50.0,
            "seed {seed}: drift {}",
            t.max_drift_micro
        );
        eprintln!("seed {seed}: {t:?}");
    }
}

/// The same workload with an insurance fund of one dollar and eight-per-cent
/// price gaps, so positions blow through their margin between blocks, bad
/// debt cannot be absorbed, and the reduce-only halt has to engage.
/// Invariant 5 is then not vacuous, and every other invariant is checked on a
/// chain that is insolvent in places — which is exactly when a conservation
/// leak would hide.
#[test]
fn with_an_empty_insurance_fund_bad_debt_halts_its_market() {
    let mut halted_somewhere = false;
    for seed in 11..=14u64 {
        let t = run(seed, 60, MICRO, 800);
        halted_somewhere |= t.halted;
        eprintln!("seed {seed}: {t:?}");
    }
    assert!(
        halted_somewhere,
        "no seed produced bad debt; the halt path went untested"
    );
}

/// Invariant 6's per-position half, stated directly: at a funding boundary a
/// long of `q` stamped at `s` pays exactly `trunc(q · (cum − s) / 1e9)`, and
/// the short it faces receives exactly its own truncated amount. With sizes
/// chosen so each truncates, the ledger loses the two fractions — the dust is
/// nobody's, and it is at most one micro-dollar per position.
#[test]
fn boundary_settlement_charges_each_position_its_truncated_amount() {
    let mut chain = Chain::new();
    let mut m = perp_market(PERP, chain.oracle.addr);
    m.taker_fee_bps = 0;
    m.maker_fee_bps_signed = 0;
    chain.list_perp(m);
    let (long, short1, short2) = (actor("bs-long"), actor("bs-short1"), actor("bs-short2"));
    for a in [&long, &short1, &short2] {
        chain.fund_margin(a.addr, 1_000_000 * MICRO);
    }
    let p = chain.proposer.clone();
    let premium = |chain: &Chain| {
        OracleState::new(&chain.db).put_mark(&cb_types::MarkPrice {
            market: PERP,
            px_q8: 50_000 * Q8,
            computed_at_height: chain.height,
            premium_ema_q9: 3_333_333,
        })
    };
    premium(&chain);
    // Sizes that do not divide: 0.33333333 and 0.66666667 BTC.
    let (q1, q2) = (33_333_333u128, 66_666_667u128);
    let r = chain.block(&[
        (&p, tick(PERP)),
        (
            &short1,
            perp_order(PERP, Side::Sell, 50_000 * Q8, q1, Tif::Gtc, false, false),
        ),
        (
            &short2,
            perp_order(PERP, Side::Sell, 50_000 * Q8, q2, Tif::Gtc, false, false),
        ),
        (
            &long,
            perp_order(
                PERP,
                Side::Buy,
                50_000 * Q8,
                q1 + q2,
                Tif::Ioc,
                false,
                false,
            ),
        ),
    ]);
    assert!(r.iter().all(|x| x.is_ok()), "{r:?}");
    for _ in 0..3 {
        premium(&chain);
        chain.block(&[(&p, tick(PERP))]);
    }
    assert_eq!(
        chain.height, 4,
        "height 4 is the boundary for an interval of 4"
    );
    let cum = chain.funding_cumulative(PERP);
    assert!(cum > 0, "longs pay");
    let owed = |q: u128| (q as i128 * cum) / RATE_DEN_Q9 as i128;
    let start = 1_000_000 * MICRO as i128;
    assert_eq!(chain.net(long.addr), start - owed(q1 + q2));
    assert_eq!(chain.net(short1.addr), start + owed(q1));
    assert_eq!(chain.net(short2.addr), start + owed(q2));
    let dust = owed(q1 + q2) - owed(q1) - owed(q2);
    assert!((0..=1).contains(&dust), "dust {dust}");
}

/// The closed-lot funding leak, pinned in both regimes it can be seen in.
///
/// Honest proposers tick funding only at interval boundaries
/// (`required_system_txs`), so between boundaries the cumulative index does
/// not move, every open position's stamp equals it, and a lot closed
/// mid-interval owes nothing: the charge `settle_closed_lot_funding` makes is
/// zero. But `apply_funding_tick` accepts a tick from the proposer at *any*
/// height (the tests in this crate tick every block), and each one accrues
/// the index pro rata. A lot closed after such a tick escaped its share while
/// the side that stayed open was paid in full at the boundary: long 2 v. short
/// 2, the long sells 1 halfway to a new buyer, and 250,000 micro-USD appeared
/// from nowhere. Now the closing lot pays its share as it closes.
#[test]
fn a_lot_closed_between_boundaries_pays_the_funding_it_accrued() {
    for tick_every_block in [false, true] {
        let mut chain = Chain::new();
        let mut m = perp_market(PERP, chain.oracle.addr);
        m.taker_fee_bps = 0;
        m.maker_fee_bps_signed = 0;
        m.funding_interval_blocks = 4;
        chain.list_perp(m);
        let (a, b, c) = (actor("fl-a"), actor("fl-b"), actor("fl-c"));
        let start = 1_000_000 * MICRO;
        for x in [&a, &b, &c] {
            chain.fund_margin(x.addr, start);
        }
        let p = chain.proposer.clone();
        let premium = |chain: &Chain| {
            OracleState::new(&chain.db).put_mark(&cb_types::MarkPrice {
                market: PERP,
                px_q8: 50_000 * Q8,
                computed_at_height: chain.height,
                premium_ema_q9: 50_000_000,
            })
        };
        let total0 = chain.total_net();
        let ticks = |h: u64| -> Vec<(Actor, TxBody)> {
            if tick_every_block || h % 4 == 0 || h == 1 {
                vec![(p.clone(), tick(PERP))]
            } else {
                vec![]
            }
        };
        let px = 50_000 * Q8;
        // h1: A long 2 v. B short 2 (the first tick, which accrues nothing).
        premium(&chain);
        let mut txs = ticks(1);
        txs.push((
            b.clone(),
            perp_order(PERP, Side::Sell, px, 2 * ONE_BTC, Tif::Gtc, false, false),
        ));
        txs.push((
            a.clone(),
            perp_order(PERP, Side::Buy, px, 2 * ONE_BTC, Tif::Ioc, false, false),
        ));
        assert!(chain.block(&txs).iter().all(|r| r.is_ok()));
        premium(&chain);
        chain.block(&ticks(2));
        // h3: A sells 1 to C, mid-interval.
        premium(&chain);
        let net_a = chain.net(a.addr);
        let mut txs = ticks(3);
        txs.push((
            c.clone(),
            perp_order(PERP, Side::Buy, px, ONE_BTC, Tif::Gtc, false, false),
        ));
        txs.push((
            a.clone(),
            perp_order(PERP, Side::Sell, px, ONE_BTC, Tif::Ioc, false, false),
        ));
        assert!(chain.block(&txs).iter().all(|r| r.is_ok()));
        let charged_at_close = net_a - chain.net(a.addr);
        premium(&chain);
        chain.block(&ticks(4));

        assert_eq!(
            chain.total_net(),
            total0,
            "tick every block: {tick_every_block}: funding is zero-sum"
        );
        if tick_every_block {
            // Two ticks of a quarter-interval each at 0.5% on the $50,000
            // mark: the index stood at 2.5M · 500 = 1.25e9 when A's lot
            // closed, so 1 BTC paid 1e8 · 1.25e9 / 1e9 = $125 — half an
            // interval of 0.5% on $50,000 of notional.
            assert_eq!(charged_at_close, 125 * MICRO as i128);
        } else {
            assert_eq!(
                charged_at_close, 0,
                "boundary-only ticking: nothing accrues mid-interval"
            );
        }
        // The side that stayed open is paid for the whole interval either way.
        assert_eq!(
            chain.net(b.addr) - start as i128,
            2 * ONE_BTC as i128 * chain.funding_cumulative(PERP) / RATE_DEN_Q9 as i128
        );
    }
}
