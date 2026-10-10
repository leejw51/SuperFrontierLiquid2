//! Liquidation under randomised price paths.
//!
//! `liquidation.rs` walks the liquidation flow through hand-picked prices;
//! this drives it with seeded random walks, three-per-cent gaps a block, and
//! a population of traders opened at ten to forty times leverage, and checks
//! the three things the liquidation machinery exists to guarantee:
//!
//! 1. **Nobody stays below maintenance unattended.** Each round is two
//!    blocks: the market moves (oracle post, fresh whale depth), then a keeper
//!    triggers every account the committed state says is liquidatable. After
//!    the trigger block, an account still below maintenance must have been
//!    triggered successfully in it *and* hold its position on a market in the
//!    reduce-only halt — the one situation where the depth the tranches need
//!    is gone (the whale cannot requote into a halted market). Anywhere else
//!    the ladder, which prices at mark −50/−100/−200 bps against depth quoted
//!    within 30–200 bps, must have closed the position out completely.
//! 2. **Bad debt only when the insurance fund is exhausted.** Trading fees are
//!    zero on this market, so the only flows into the fund are liquidation
//!    fees taken from accounts that still had collateral. Then, checked per
//!    transaction: any transaction that leaves an account owing more than it
//!    did also leaves the fund at exactly zero.
//! 3. **The reduce-only halt engages**, on the market the debt came from, in
//!    the same transaction — and once engaged, opening orders are refused and
//!    reduce-only ones still accepted.
//!
//! Each property is also checked to be non-vacuous across the seeds:
//! liquidations happened, some were absorbed by the fund, and some exhausted
//! it and halted the market.

mod support;

use cb_block::ExecError;
use cb_engine::{is_liquidatable, DEFAULT_TIERS};
use cb_state::{Database, LiquidationQueue, PerpMarkets, PerpOrders, Positions};
use cb_tx::TxBody;
use cb_types::{Address, DerivId, Side, Tif};
use std::collections::BTreeMap;
use support::*;

const PERP1: DerivId = DerivId::perp(1);
const MARKETS: [DerivId; 2] = [PERP, PERP1];

fn liquidatable(db: &Database, owner: Address) -> bool {
    let held = Positions::new(db).list_for_owner(&owner);
    let marks: Vec<(DerivId, u128)> = MARKETS
        .iter()
        .filter_map(|m| mark_in(db, *m).map(|x| (*m, x)))
        .collect();
    is_liquidatable(&margin_in(db, owner), &held, &marks, DEFAULT_TIERS)
}

fn halted(db: &Database, m: DerivId) -> bool {
    PerpMarkets::new(db)
        .get(&m)
        .unwrap()
        .paused_reduce_only_only
}

#[derive(Default, Debug)]
struct Seen {
    liquidations: usize,
    absorbed: usize,
    halts: usize,
    stuck_in_halt: usize,
}

fn run(seed: u64, rounds: u64, insurance: u128) -> Seen {
    let mut rng = Rng::new(seed);
    let mut chain = Chain::new();
    for m in MARKETS {
        let mut rec = perp_market(m, chain.oracle.addr);
        rec.taker_fee_bps = 0;
        rec.maker_fee_bps_signed = 0;
        rec.funding_interval_blocks = 1_000_000;
        chain.list_perp(rec);
    }
    let whale = actor("liq-whale");
    let keeper = actor("liq-keeper");
    chain.fund_margin(whale.addr, 10_000_000_000 * MICRO);
    chain.fund_margin(keeper.addr, 0);
    chain.fund_margin(Address::INSURANCE, insurance);
    let traders: Vec<Actor> = (0..10)
        .map(|i| actor(&format!("liq-t-{seed}-{i}")))
        .collect();
    let mut px: BTreeMap<DerivId, u128> = MARKETS.iter().map(|m| (*m, 50_000 * Q8)).collect();
    let oracle_actor = chain.oracle.clone();
    let mut seen = Seen::default();

    // Whale depth around the current price, on every market not halted.
    let depth = |chain: &Chain, px: &BTreeMap<DerivId, u128>| -> Vec<(Actor, TxBody)> {
        let mut out = Vec::new();
        for o in PerpOrders::new(&chain.db).list_for_owner(&whale.addr) {
            out.push((
                whale.clone(),
                TxBody::CancelPerpOrder {
                    order_id: o.order_id,
                },
            ));
        }
        for m in MARKETS {
            if halted(&chain.db, m) {
                continue;
            }
            for bps in [30u128, 80, 200] {
                let p = px[&m];
                out.push((
                    whale.clone(),
                    perp_order(
                        m,
                        Side::Buy,
                        p * (10_000 - bps) / 10_000,
                        20 * ONE_BTC,
                        Tif::Gtc,
                        false,
                        false,
                    ),
                ));
                out.push((
                    whale.clone(),
                    perp_order(
                        m,
                        Side::Sell,
                        p * (10_000 + bps) / 10_000,
                        20 * ONE_BTC,
                        Tif::Gtc,
                        false,
                        false,
                    ),
                ));
            }
        }
        out
    };

    // Open: marks posted, depth quoted, every trader levered up at 10–40x.
    let mut open: Vec<(Actor, TxBody)> = MARKETS
        .iter()
        .map(|m| (oracle_actor.clone(), oracle(*m, px[m], 1)))
        .collect();
    open.extend(depth(&chain, &px));
    for t in &traders {
        let collateral = rng.range(1_000, 20_000);
        chain.fund_margin(t.addr, collateral * MICRO);
        let lev = rng.range(10, 40);
        let m = MARKETS[rng.below(2) as usize];
        let qty = collateral * lev * ONE_BTC / 50_000;
        let side = rng.side();
        let limit = match side {
            Side::Buy => px[&m] * 10_030 / 10_000,
            Side::Sell => px[&m] * 9_970 / 10_000,
        };
        open.push((
            t.clone(),
            perp_order(m, side, limit, qty, Tif::Ioc, false, false),
        ));
    }
    let r = chain.block(&open);
    let opened = r.iter().filter(|x| x.is_ok()).count();
    assert!(
        opened > 10,
        "seed {seed}: the opening block must open positions: {r:?}"
    );

    for round in 0..rounds {
        // --- Move the market. -------------------------------------------
        for m in MARKETS {
            let p = px[&m];
            let step = rng.range(0, 600);
            let next = if step >= 300 {
                p * (10_000 + step - 300) / 10_000
            } else {
                p * (10_000 - (300 - step)) / 10_000
            };
            px.insert(m, next.clamp(10_000 * Q8, 200_000 * Q8));
        }
        let h = chain.height + 1;
        let mut move_block: Vec<(Actor, TxBody)> = MARKETS
            .iter()
            .map(|m| (oracle_actor.clone(), oracle(*m, px[m], h)))
            .collect();
        move_block.extend(depth(&chain, &px));
        chain.block(&move_block);

        // --- Liquidate everyone the committed state says is below MM. ----
        let mut triggers: Vec<(Actor, TxBody)> = Vec::new();
        for t in &traders {
            if liquidatable(&chain.db, t.addr) {
                for p in Positions::new(&chain.db)
                    .list_for_owner(&t.addr)
                    .iter()
                    .filter(|p| p.qty > 0)
                {
                    triggers.push((
                        keeper.clone(),
                        TxBody::LiquidationTrigger {
                            owner: t.addr,
                            market: p.market,
                        },
                    ));
                }
            }
        }
        if triggers.is_empty() {
            continue;
        }
        let height = chain.height + 1;
        let mut debt_before: BTreeMap<Address, u128> = BTreeMap::new();
        let mut ins_before = 0u128;
        let mut halted_before: BTreeMap<DerivId, bool> = BTreeMap::new();
        let bodies: Vec<TxBody> = triggers.iter().map(|(_, b)| b.clone()).collect();
        let results = chain.block_with(&triggers, |db, i, r| match r {
            None => {
                debt_before = margins_in(db)
                    .iter()
                    .map(|m| (m.owner, m.bad_debt))
                    .collect();
                ins_before = margin_in(db, Address::INSURANCE).collateral;
                halted_before = MARKETS.iter().map(|m| (*m, halted(db, *m))).collect();
            }
            Some(res) => {
                let ins = margin_in(db, Address::INSURANCE).collateral;
                let mut grew = false;
                for m in margins_in(db) {
                    if m.bad_debt > debt_before.get(&m.owner).copied().unwrap_or(0) {
                        grew = true;
                    }
                }
                if grew {
                    assert!(res.is_ok());
                    assert_eq!(
                        ins, 0,
                        "seed {seed} r{round} tx{i}: bad debt grew while the fund still held {ins}"
                    );
                    let TxBody::LiquidationTrigger { market, .. } = &bodies[i] else {
                        unreachable!()
                    };
                    assert!(
                        halted(db, *market),
                        "seed {seed} r{round} tx{i}: debt with no halt on {market}"
                    );
                }
                if res.is_ok() && ins < ins_before {
                    seen.absorbed += 1;
                }
                for m in MARKETS {
                    if halted(db, m) && !halted_before[&m] {
                        seen.halts += 1;
                    }
                }
            }
        });
        let ok_owners: Vec<(Address, DerivId)> = triggers
            .iter()
            .zip(&results)
            .filter(|(_, r)| r.is_ok())
            .map(|((_, b), _)| match b {
                TxBody::LiquidationTrigger { owner, market } => (*owner, *market),
                _ => unreachable!(),
            })
            .collect();
        seen.liquidations += ok_owners.len();
        for r in &results {
            assert!(
                matches!(r, Ok(()) | Err(ExecError::NotLiquidatable)),
                "seed {seed} r{round}: a trigger failed for a reason other than having been fixed by an earlier tranche: {r:?}"
            );
        }
        assert_eq!(
            LiquidationQueue::new(&chain.db)
                .list_at_height(height)
                .len(),
            ok_owners.len(),
            "seed {seed} r{round}: each successful trigger leaves one audit row"
        );

        // --- Property 1. ------------------------------------------------
        for t in &traders {
            if !liquidatable(&chain.db, t.addr) {
                continue;
            }
            for p in Positions::new(&chain.db)
                .list_for_owner(&t.addr)
                .iter()
                .filter(|p| p.qty > 0)
            {
                assert!(
                    ok_owners.contains(&(t.addr, p.market)),
                    "seed {seed} r{round}: {:?} is below maintenance on {} and was not liquidated",
                    t.addr,
                    p.market
                );
                assert!(
                    halted(&chain.db, p.market),
                    "seed {seed} r{round}: liquidated with depth available, yet still holds {p:?}"
                );
                seen.stuck_in_halt += 1;
            }
        }
    }

    // --- Property 3, second half: a halted market refuses opening orders
    //     and still accepts reduce-only ones. ---------------------------------
    for m in MARKETS {
        if !halted(&chain.db, m) {
            continue;
        }
        let fresh = actor(&format!("liq-fresh-{seed}"));
        chain.fund_margin(fresh.addr, 1_000_000 * MICRO);
        let r = chain.block(&[(
            fresh.clone(),
            perp_order(m, Side::Buy, px[&m], ONE_BTC, Tif::Gtc, false, false),
        )]);
        assert_eq!(
            r,
            vec![Err(ExecError::ReduceOnlyOnly)],
            "seed {seed}: opening order in a halted market"
        );
        let holder = Positions::new(&chain.db)
            .list_for_market(&m)
            .into_iter()
            .find(|p| p.qty > 0 && p.owner != whale.addr);
        if let Some(p) = holder {
            let t = traders.iter().find(|t| t.addr == p.owner).unwrap();
            let r = chain.block(&[(
                t.clone(),
                perp_order(m, opposite(p.side), px[&m], p.qty, Tif::Ioc, false, true),
            )]);
            assert_ne!(
                r[0],
                Err(ExecError::ReduceOnlyOnly),
                "seed {seed}: reduce-only refused in a halt"
            );
        }
    }
    seen
}

#[test]
fn random_price_paths_leave_no_account_below_maintenance_unattended() {
    let mut total = Seen::default();
    for seed in 1..=8u64 {
        let s = run(seed, 30, 3_000 * MICRO);
        eprintln!("seed {seed}: {s:?}");
        total.liquidations += s.liquidations;
        total.absorbed += s.absorbed;
        total.halts += s.halts;
        total.stuck_in_halt += s.stuck_in_halt;
    }
    assert!(total.liquidations >= 20, "{total:?}");
    assert!(
        total.absorbed > 0,
        "the fund never absorbed anything: {total:?}"
    );
    assert!(
        total.halts > 0,
        "the fund was never exhausted, so the halt went untested: {total:?}"
    );
}

/// With a fund far larger than any loss the paths can produce, no debt ever
/// survives and no market ever halts: the halt is a consequence of
/// exhaustion, not of liquidation as such.
#[test]
fn a_deep_insurance_fund_absorbs_everything_and_never_halts() {
    for seed in 21..=24u64 {
        let s = run(seed, 30, 1_000_000_000 * MICRO);
        assert_eq!(s.halts, 0, "seed {seed}: {s:?}");
        assert_eq!(s.stuck_in_halt, 0, "seed {seed}: {s:?}");
    }
}
