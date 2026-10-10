//! Transaction atomicity on the derivative side, and the nonce, replay and
//! chain-id edges that the existing files leave open.
//!
//! # Atomicity
//!
//! `block_atomicity.rs` proves, for spot traffic, that a failing transaction
//! inserted anywhere in a block leaves the state root and every row exactly
//! as the block without it. Most executor handlers fail *before* they write
//! anything, which makes that easy; the interesting cases are the ones that
//! fail after writing, and rely on the builder's savepoint to undo it. This
//! file runs the same experiment with one failing transaction per reachable
//! failure stage, across perps, spot, oracle, funding, cancels and
//! transfers — including the one derivative handler found to fail *midway*:
//! a `LiquidationTrigger` naming a market where the (genuinely liquidatable)
//! owner holds only the flat `qty == 0` row an exact close leaves behind. The
//! row puts that market's mark into the re-check, so the trigger gets as far
//! as writing a liquidation-queue row and cancelling every order the owner
//! has resting there before it finds nothing to liquidate. Rollback must put
//! all of it back.
//!
//! For every case and every insertion position: the trial block's root and
//! full column-family snapshot equal the control block's, the other
//! transactions' outcomes are unchanged, and the failure is the one
//! intended — so each case really reached its stage.
//!
//! # Nonces, replay, chain id
//!
//! Gaps and stale nonces report the expected nonce; an included transaction
//! cannot be replayed, in the same block or a later one; a *rejected* one
//! consumes nothing and its exact bytes execute later once they can (the
//! documented replay window); a self-transfer moves nothing and spends one
//! nonce; synthetic system transactions ignore the nonce entirely, cannot be
//! attributed to anyone but the proposer, and are still held to the chain id.

mod support;

use cb_block::{apply_tx_at, BlockState, ExecError};
use cb_state::{Accounts, LiquidationQueue, PerpOrders};
use cb_tx::{Transaction, TxBody, VerifiedTx};
use cb_types::{Address, AssetId, DerivId, Side, Tif};
use support::*;

const PERP1: DerivId = DerivId::perp(1);
const PERP2: DerivId = DerivId::perp(2);

struct Cast {
    whale: Actor,
    trader: Actor,
    poor: Actor,
    victim: Actor,
    keeper: Actor,
    spot_seller: Actor,
    unpriced_trader: Actor,
    unpriced_maker: Actor,
}

/// A deterministic chain with something on every book and one account one
/// price move from liquidation. Built identically for control and trial.
fn world() -> (Chain, Cast) {
    let mut chain = Chain::new();
    for m in [PERP, PERP1, PERP2] {
        let mut rec = perp_market(m, chain.oracle.addr);
        rec.funding_interval_blocks = 1_000_000;
        chain.list_perp(rec);
    }
    chain.list_spot(spot_market(1, 1, 10));
    let cast = Cast {
        whale: actor("atom-whale"),
        trader: actor("atom-trader"),
        poor: actor("atom-poor"),
        victim: actor("atom-victim"),
        keeper: actor("atom-keeper"),
        spot_seller: actor("atom-spot-seller"),
        unpriced_trader: actor("atom-unpriced-trader"),
        unpriced_maker: actor("atom-unpriced-maker"),
    };
    chain.fund_margin(cast.whale.addr, 1_000_000_000 * MICRO);
    chain.fund_margin(cast.trader.addr, 1_000_000 * MICRO);
    chain.fund_margin(cast.poor.addr, 10 * MICRO);
    chain.fund_margin(cast.victim.addr, 1_500 * MICRO);
    chain.fund_margin(cast.keeper.addr, 0);
    // Isolate the deliberately unpriced portfolio: opening exposure now
    // requires fresh marks for every held market, not just the traded one.
    chain.fund_margin(cast.unpriced_trader.addr, 1_000_000 * MICRO);
    chain.fund_margin(cast.unpriced_maker.addr, 1_000_000 * MICRO);
    chain.fund_margin(Address::INSURANCE, 1_000_000 * MICRO);
    chain.fund_spot(cast.trader.addr, AssetId::Usd, 10u128.pow(15));
    chain.fund_spot(cast.spot_seller.addr, AssetId::Btc, 100 * ONE_BTC);
    chain.set_mark(PERP, 50_000 * Q8);
    chain.set_mark(PERP1, 50_000 * Q8);
    // PERP2 is marked only while its position is opened; the mark is taken
    // away below, so "close without a mark" still has a market to fail on.
    chain.set_mark(PERP2, 50_000 * Q8);
    let c = &cast;
    let r = chain.block(&[
        (
            &c.whale,
            perp_order(
                PERP,
                Side::Sell,
                50_000 * Q8,
                10 * ONE_BTC,
                Tif::Gtc,
                false,
                false,
            ),
        ),
        (
            &c.whale,
            perp_order(
                PERP,
                Side::Buy,
                48_800 * Q8,
                10 * ONE_BTC,
                Tif::Gtc,
                false,
                false,
            ),
        ),
        (
            &c.unpriced_maker,
            perp_order(
                PERP2,
                Side::Sell,
                50_000 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                false,
            ),
        ),
        (
            &c.victim,
            perp_order(
                PERP,
                Side::Buy,
                50_000 * Q8,
                ONE_BTC,
                Tif::Ioc,
                false,
                false,
            ),
        ),
        // The victim round-trips a small PERP1 position, which leaves the
        // flat `qty == 0` row an exact close keeps, then rests a bid there.
        (
            &c.whale,
            perp_order(
                PERP1,
                Side::Sell,
                50_000 * Q8,
                ONE_BTC / 100,
                Tif::Gtc,
                false,
                false,
            ),
        ),
        (
            &c.victim,
            perp_order(
                PERP1,
                Side::Buy,
                50_000 * Q8,
                ONE_BTC / 100,
                Tif::Ioc,
                false,
                false,
            ),
        ),
        (
            &c.whale,
            perp_order(
                PERP1,
                Side::Buy,
                50_000 * Q8,
                ONE_BTC / 100,
                Tif::Gtc,
                false,
                false,
            ),
        ),
        (
            &c.victim,
            perp_order(
                PERP1,
                Side::Sell,
                50_000 * Q8,
                ONE_BTC / 100,
                Tif::Ioc,
                false,
                true,
            ),
        ),
        (
            &c.victim,
            perp_order(
                PERP1,
                Side::Buy,
                45_000 * Q8,
                ONE_BTC / 100,
                Tif::Gtc,
                false,
                false,
            ),
        ),
        (
            &c.unpriced_trader,
            perp_order(
                PERP2,
                Side::Buy,
                50_000 * Q8,
                ONE_BTC / 100,
                Tif::Ioc,
                false,
                false,
            ),
        ),
        (
            &c.spot_seller,
            spot_order(Side::Sell, 50_000 * Q8, ONE_BTC, Tif::Gtc, false),
        ),
    ]);
    assert!(r.iter().all(|x| x.is_ok()), "{r:?}");
    chain.set_mark(PERP2, 0);
    let flat = cb_state::Positions::new(&chain.db)
        .get(&cast.victim.addr, &PERP1)
        .expect("a flat row");
    assert_eq!(flat.qty, 0);
    // Equity 1,500 − 1,100 = 400 against maintenance of 489: liquidatable.
    chain.set_mark(PERP, 48_900 * Q8);
    (chain, cast)
}

/// The transactions that succeed around the injected failure.
fn base(c: &Cast, oracle_actor: &Actor, h: u64) -> Vec<(Actor, TxBody)> {
    vec![
        (oracle_actor.clone(), oracle(PERP1, 50_000 * Q8, h)),
        (
            c.trader.clone(),
            perp_order(
                PERP,
                Side::Buy,
                50_000 * Q8,
                ONE_BTC / 10,
                Tif::Ioc,
                false,
                false,
            ),
        ),
        (
            c.trader.clone(),
            spot_order(Side::Buy, 50_000 * Q8, ONE_BTC / 10, Tif::Ioc, false),
        ),
        (
            c.whale.clone(),
            perp_order(
                PERP,
                Side::Sell,
                50_100 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                false,
            ),
        ),
    ]
}

/// (name, the failing transaction given the chain it goes into, what failure it must hit)
type Case = (
    &'static str,
    fn(&Chain, &Cast, u64) -> (Actor, TxBody),
    fn(&ExecError) -> bool,
);

fn cases() -> Vec<Case> {
    vec![
        (
            "liquidation fails midway: owner holds only a flat row on the named market",
            |_, c, _| {
                (
                    c.keeper.clone(),
                    TxBody::LiquidationTrigger {
                        owner: c.victim.addr,
                        market: PERP1,
                    },
                )
            },
            |e| *e == ExecError::NotLiquidatable,
        ),
        (
            "liquidation of a healthy account",
            |_, c, _| {
                (
                    c.keeper.clone(),
                    TxBody::LiquidationTrigger {
                        owner: c.whale.addr,
                        market: PERP,
                    },
                )
            },
            |e| *e == ExecError::NotLiquidatable,
        ),
        (
            "perp order the margin gate refuses",
            |_, c, _| {
                (
                    c.poor.clone(),
                    perp_order(
                        PERP,
                        Side::Buy,
                        49_000 * Q8,
                        10 * ONE_BTC,
                        Tif::Gtc,
                        false,
                        false,
                    ),
                )
            },
            |e| *e == ExecError::InsufficientMargin,
        ),
        (
            "perp FOK the book cannot fill (after the gate)",
            |_, c, _| {
                (
                    c.trader.clone(),
                    perp_order(
                        PERP,
                        Side::Buy,
                        50_000 * Q8,
                        100 * ONE_BTC,
                        Tif::Fok,
                        false,
                        false,
                    ),
                )
            },
            |e| *e == ExecError::OrderRejected,
        ),
        (
            "perp post-only that crosses",
            |_, c, _| {
                (
                    c.trader.clone(),
                    perp_order(PERP, Side::Buy, 50_000 * Q8, ONE_BTC, Tif::Gtc, true, false),
                )
            },
            |e| *e == ExecError::OrderRejected,
        ),
        (
            "close without a mark",
            |_, c, _| {
                (
                    c.unpriced_trader.clone(),
                    TxBody::ClosePosition {
                        market: PERP2,
                        qty: 0,
                        max_slippage_bps: 100,
                    },
                )
            },
            |e| *e == ExecError::MissingMarkPrice,
        ),
        (
            "reduce-only larger than the position",
            |_, c, _| {
                (
                    c.victim.clone(),
                    perp_order(
                        PERP,
                        Side::Sell,
                        48_000 * Q8,
                        2 * ONE_BTC,
                        Tif::Ioc,
                        false,
                        true,
                    ),
                )
            },
            |e| *e == ExecError::ReduceOnlyTooLarge,
        ),
        (
            "spot FOK unfilled (lock taken, then released)",
            |_, c, _| {
                (
                    c.trader.clone(),
                    spot_order(Side::Buy, 50_000 * Q8, 50 * ONE_BTC, Tif::Fok, false),
                )
            },
            |e| *e == ExecError::FokUnfilled,
        ),
        (
            "spot buy beyond the balance",
            |_, c, _| {
                (
                    c.poor.clone(),
                    spot_order(Side::Buy, 50_000 * Q8, ONE_BTC, Tif::Gtc, false),
                )
            },
            |e| *e == ExecError::Insufficient,
        ),
        (
            "oracle post from a future height",
            |chain, _, h| (chain.oracle.clone(), oracle(PERP, 49_000 * Q8, h + 5)),
            |e| matches!(e, ExecError::OracleFromFuture { .. }),
        ),
        (
            "funding tick from someone other than the proposer",
            |_, c, _| (c.trader.clone(), tick(PERP)),
            |e| *e == ExecError::NotAuthorized,
        ),
        (
            "cancel of somebody else's perp order",
            |chain, c, _| {
                (
                    c.trader.clone(),
                    TxBody::CancelPerpOrder {
                        order_id: PerpOrders::new(&chain.db).list_for_owner(&c.whale.addr)[0]
                            .order_id,
                    },
                )
            },
            |e| *e == ExecError::NotAuthorized,
        ),
        (
            "transfer beyond the balance",
            |_, c, _| {
                (
                    c.poor.clone(),
                    TxBody::Transfer {
                        to: c.trader.addr,
                        asset: AssetId::Usd,
                        amount: 1,
                    },
                )
            },
            |e| *e == ExecError::Insufficient,
        ),
        (
            "withdrawal (refused until a ledger exists)",
            |_, c, _| {
                (
                    c.trader.clone(),
                    TxBody::Withdraw {
                        asset: AssetId::Usd,
                        amount: 1,
                        external_address: "bc1q".into(),
                    },
                )
            },
            |e| *e == ExecError::Unsupported,
        ),
    ]
}

#[test]
fn a_failure_at_any_stage_anywhere_in_a_block_leaves_no_trace() {
    for (name, make, expect) in cases() {
        let n = 4; // base(..).len()
        for at in 0..=n {
            let (mut control, cc) = world();
            let h = control.height + 1;
            let oracle_c = control.oracle.clone();
            let control_txs = base(&cc, &oracle_c, h);
            let control_res = control.block(&control_txs);
            assert!(control_res.iter().all(Result::is_ok), "{control_res:?}");

            let (mut trial, tc) = world();
            let oracle_t = trial.oracle.clone();
            let mut trial_txs = base(&tc, &oracle_t, h);
            trial_txs.insert(at, make(&trial, &tc, h));
            let before_victim_orders = PerpOrders::new(&trial.db)
                .list_for_owner(&tc.victim.addr)
                .len();
            let trial_res = trial.block(&trial_txs);

            let failed = &trial_res[at];
            assert!(
                matches!(failed, Err(e) if expect(e)),
                "{name} at {at}: expected its own failure, got {failed:?}"
            );
            let mut others = trial_res.clone();
            let _ = others.remove(at);
            assert_eq!(
                others, control_res,
                "{name} at {at}: the other transactions' outcomes changed"
            );
            assert_eq!(
                trial.last_root, control.last_root,
                "{name} at {at}: root moved"
            );
            assert_eq!(
                snapshot(&trial.db),
                snapshot(&control.db),
                "{name} at {at}: rows differ"
            );
            // The midway case, spelled out.
            assert_eq!(
                PerpOrders::new(&trial.db)
                    .list_for_owner(&tc.victim.addr)
                    .len(),
                before_victim_orders
            );
            assert!(LiquidationQueue::new(&trial.db)
                .list_at_height(h)
                .is_empty());
        }
    }
}

/// The midway case is real: without the rollback the trigger *would* have
/// cancelled the victim's order. Applied bare — no savepoint — the partial
/// writes are visible in the block's own view after the error.
#[test]
fn the_midway_liquidation_failure_does_write_before_it_fails() {
    let (chain, c) = world();
    let mut state = BlockState::new(&chain.db);
    let ctx = ctx_at(chain.height + 1, chain.proposer.addr);
    let v = sign(
        &c.keeper.sk,
        0,
        TxBody::LiquidationTrigger {
            owner: c.victim.addr,
            market: PERP1,
        },
    );
    assert_eq!(
        apply_tx_at(&mut state, &ctx, &v, 0),
        Err(ExecError::NotLiquidatable)
    );
    let db = state.db();
    assert!(
        PerpOrders::new(&db)
            .list_for_owner(&c.victim.addr)
            .is_empty(),
        "orders were cancelled"
    );
    assert_eq!(
        LiquidationQueue::new(&db)
            .list_at_height(chain.height + 1)
            .len(),
        1,
        "a queue row was written"
    );
    // (The state is dropped uncommitted.)
}

// ---------------------------------------------------------------------------
// Nonces, replay, chain id
// ---------------------------------------------------------------------------

fn transfer(to: Address, amount: u128) -> TxBody {
    TxBody::Transfer {
        to,
        asset: AssetId::Usd,
        amount,
    }
}

#[test]
fn gaps_stale_nonces_and_replays_are_refused_with_the_expected_nonce() {
    let mut chain = Chain::new();
    let (a, b) = (actor("nonce-a"), actor("nonce-b"));
    chain.fund_spot(a.addr, AssetId::Usd, 1_000);
    let t0 = sign(&a.sk, 0, transfer(b.addr, 1));
    let t1 = sign(&a.sk, 1, transfer(b.addr, 2));
    let gap = sign(&a.sk, 3, transfer(b.addr, 3));
    // Duplicate in one block, a gap, then the next in line.
    let r = chain.block_signed(&[t0.clone(), t0.clone(), gap.clone(), t1.clone()]);
    assert_eq!(
        r,
        vec![
            Ok(()),
            Err(ExecError::BadNonce {
                expected: 1,
                got: 0
            }),
            Err(ExecError::BadNonce {
                expected: 1,
                got: 3
            }),
            Ok(()),
        ]
    );
    // Exact replays of included bytes in a later block, and a stale nonce.
    let stale = sign(&a.sk, 1, transfer(b.addr, 999));
    let r = chain.block_signed(&[t0, t1, stale]);
    assert!(
        r.iter()
            .all(|x| matches!(x, Err(ExecError::BadNonce { expected: 2, .. }))),
        "{r:?}"
    );
    let acc = Accounts::new(&chain.db);
    assert_eq!(acc.get(&a.addr).unwrap().nonce, 2);
    assert_eq!(acc.get(&a.addr).unwrap().balance(AssetId::Usd), 997);
    assert_eq!(acc.get(&b.addr).unwrap().balance(AssetId::Usd), 3);
    // The gap closes once the nonce catches up.
    let t2 = sign(&a.sk, 2, transfer(b.addr, 1));
    assert_eq!(
        chain.block_signed(&[gap.clone(), t2, gap]),
        vec![
            Err(ExecError::BadNonce {
                expected: 2,
                got: 3
            }),
            Ok(()),
            Ok(())
        ]
    );
}

/// A rejected transaction spends no nonce, so its signed bytes stay valid,
/// and they execute later once the reason for the rejection is gone. The
/// block builder documents this window; here it is, end to end.
#[test]
fn a_rejected_transaction_replays_verbatim_once_it_can_succeed() {
    let mut chain = Chain::new();
    let mut m = perp_market(PERP, chain.oracle.addr);
    m.funding_interval_blocks = 1_000_000;
    chain.list_perp(m);
    chain.set_mark(PERP, 50_000 * Q8);
    let (maker, taker) = (actor("replay-maker"), actor("replay-taker"));
    chain.fund_margin(maker.addr, 1_000_000 * MICRO);
    chain.fund_margin(taker.addr, 1_000_000 * MICRO);
    assert_eq!(
        chain.block(&[(
            maker.clone(),
            perp_order(
                PERP,
                Side::Sell,
                50_000 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                false
            )
        )]),
        vec![Ok(())]
    );
    let post_only = sign(
        &taker.sk,
        0,
        perp_order(PERP, Side::Buy, 50_000 * Q8, ONE_BTC, Tif::Gtc, true, false),
    );
    assert_eq!(
        chain.block_signed(std::slice::from_ref(&post_only)),
        vec![Err(ExecError::OrderRejected)]
    );
    assert_eq!(chain.nonce_of(taker.addr), 0);
    let ask = PerpOrders::new(&chain.db).list_for_owner(&maker.addr)[0].order_id;
    assert_eq!(
        chain.block(&[(maker, TxBody::CancelPerpOrder { order_id: ask })]),
        vec![Ok(())]
    );
    assert_eq!(
        chain.block_signed(std::slice::from_ref(&post_only)),
        vec![Ok(())],
        "same bytes, now valid"
    );
    assert_eq!(chain.nonce_of(taker.addr), 1);
    assert_eq!(
        chain.block_signed(&[post_only]),
        vec![Err(ExecError::BadNonce {
            expected: 1,
            got: 0
        })]
    );
}

#[test]
fn a_self_transfer_moves_nothing_and_spends_one_nonce() {
    let mut chain = Chain::new();
    let a = actor("self-transfer");
    chain.fund_spot(a.addr, AssetId::Usd, 500);
    assert_eq!(
        chain.block(&[(a.clone(), transfer(a.addr, 500))]),
        vec![Ok(())]
    );
    let s = Accounts::new(&chain.db).get(&a.addr).unwrap();
    assert_eq!((s.nonce, s.balance(AssetId::Usd)), (1, 500));
    assert_eq!(
        chain.block(&[(a.clone(), transfer(a.addr, 501))]),
        vec![Err(ExecError::Insufficient)]
    );
}

/// Synthetic system transactions (BFT execution) carry no signature and no
/// nonce. Any nonce is accepted and none is consumed, so the proposer's own
/// next signed transaction is unaffected; only a funding tick or settlement
/// may be synthetic; one attributed to anybody but the proposer is refused;
/// and the chain id still binds.
#[test]
fn synthetic_ticks_ignore_nonces_but_not_authority_or_chain() {
    let mut chain = Chain::new();
    let mut m = perp_market(PERP, chain.oracle.addr);
    m.funding_interval_blocks = 1_000_000;
    chain.list_perp(m);
    let proposer = chain.proposer.addr;
    let stranger = actor("synthetic-stranger");
    let syn = |sender: Address, nonce: u64, chain_id: u32, body: TxBody| {
        VerifiedTx::synthetic(
            Transaction {
                nonce,
                chain_id,
                body,
            },
            sender,
        )
    };
    let r = chain.block_signed(&[
        syn(proposer, 999, CHAIN_ID, tick(PERP)),
        syn(proposer, 0, CHAIN_ID, tick(PERP)),
        syn(stranger.addr, 0, CHAIN_ID, tick(PERP)),
        syn(proposer, 0, CHAIN_ID + 1, tick(PERP)),
        syn(proposer, 0, CHAIN_ID, transfer(stranger.addr, 1)),
        sign(&chain.proposer.sk, 0, tick(PERP)),
    ]);
    assert_eq!(
        r,
        vec![
            Ok(()),
            Ok(()),
            Err(ExecError::NotAuthorized),
            Err(ExecError::WrongChainId {
                expected: CHAIN_ID,
                got: CHAIN_ID + 1
            }),
            Err(ExecError::NotAuthorized),
            Ok(()),
        ]
    );
    assert_eq!(
        chain.nonce_of(proposer),
        1,
        "only the signed tick spent a nonce"
    );
}
