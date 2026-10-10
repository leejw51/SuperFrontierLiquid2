//! Pure perpetual CLOB matching engine.
//!
//! `match_order` is the ONE entry point. Given a read-only snapshot of book
//! state and an `IncomingOrder`, it produces a `MatchResult` describing every
//! fill, every CF mutation, and the final outcome (rested / IOC remainder
//! cancelled / FOK kill / post-only reject / self-trade reject).
//!
//! ## Why this lives in `cb-engine`
//!
//! `cb-engine` is the pure layer; `cb-block` is its impure caller. The block
//! executor is responsible for:
//!   1. Reading current book state into a `BookSnapshot` impl backed by
//!      `cb-state`.
//!   2. Calling `match_order` to produce a `MatchResult`.
//!   3. Applying the returned `BookMutation`s + position deltas to RocksDB.
//!
//! No file in this module touches RocksDB, system time, the filesystem, or
//! anything else with externally-observable side effects. Same inputs always
//! produce the same `MatchResult` — that's the determinism contract for the
//! consensus path.
//!
//! ## Invariants the matcher enforces
//!
//! - **Price-time priority.** The best price crosses first; within a price
//!   level, the lowest `seq` (oldest order) crosses first. The book index CF
//!   key layout `[deriv_id||side||price_be||seq_be]` already gives us this in
//!   lexicographic order; we walk it ascending for sells (best ask = lowest
//!   price) and reverse for buys (best bid = highest price).
//! - **Self-trade prevention.** v1 default is `CancelTaker`: an incoming
//!   order that would match its own resting maker is rejected outright. The
//!   maker stays in the book.
//! - **Reduce-only revalidation.** A reduce-only order never grows or flips a
//!   position (per docs/design/perps-trading §1.3), and this module is where
//!   that is *enforced*, not merely assumed. The matcher reads the owner's open
//!   quantity through [`BookSnapshot::reduce_only_capacity`] and caps the fill
//!   plan against it — on the taker, and independently on every reduce-only
//!   maker it crosses. A reduce-only maker whose remainder can no longer reduce
//!   anything is deleted from the book rather than left resting.
//!
//!   The executor still rejects an oversized reduce-only order at admission,
//!   because a trader should get an error rather than a silently different
//!   order. But admission cannot be the only gate: a reduce-only GTC order that
//!   was valid when it was placed can be hit as a maker blocks later, by which
//!   time the position it was written to close may be gone. The earlier version
//!   of this comment claimed the cancellation happened here while the code
//!   relied on the executor, and the executor relied on this comment — which is
//!   how a one-unit position became a licence to open an unbounded short.
//! - **Post-only.** If any cross is possible at all, the entire order is
//!   rejected with `RejectedPostOnlyCross`. No silent downgrade.
//! - **FOK.** Either the full requested qty fills against the existing book
//!   or nothing fills. The matcher computes the would-be fill plan, commits
//!   it only if `qty_filled == incoming.qty`, otherwise returns
//!   `KilledFOK` with no mutations.
//! - **IOC.** Cross what we can; cancel the rest.
//! - **GTC.** Cross what we can; rest the remainder in the book.
//!
//! ## What this module does NOT do (M3 boundary)
//!
//! - It does not check margin, collateral, leverage, IM, or MM. M4 owns
//!   those. Pre-M4 a fill can produce a negative-equity position; the M4
//!   margin gate will reject the place-order tx earlier in the executor.
//! - It does not compute fees. Fee accounting lives in the executor (which
//!   has the `MarginAccount` write handle); the matcher only emits fills.
//! - It does not compute mark price, oracle blend, or funding. M2 owns
//!   those; the matcher's job is to publish a clean `last_trade_price`
//!   that M2 can read.

use cb_types::{Address, DerivId, PerpOrder, Side, Tif, H256};
use std::collections::HashMap;
use thiserror::Error;

/// Width of the derivative price band, in basis points of the mark: **10%**.
///
/// Every derivative order's limit price, and every derivative fill's
/// execution price, must lie within `mark × (1 ± 10%)` (bounds rounded
/// inward, see [`derivative_price_band`]). This is Hyperliquid's defence
/// against the same attack, and the reason it exists here is an insurance-fund
/// drain: closing orders skip the pre-trade margin gate — they have to, or an
/// underwater account could never get out — so without a band a solvent
/// account could sell its whole long at $1 into a colluder, realise a loss far
/// beyond its collateral, and have the insurance fund pay the colluder the
/// difference. (Test: `cb-block/tests/margin_gate_worst_case.rs`.)
///
/// Why 10%, one band for every order:
///
/// * The band **bounds** the drain rather than zeroing it. A full close at the
///   band's edge realises at most 10% of notional against the mark, and the
///   position was opened with at least the initial margin (2% at tier 1)
///   behind it, so the fund's worst exposure to one collusive close is about
///   (10% − IM%) of notional — against *unbounded* before. Any band wider
///   than the IM rate leaves some residual; a band as tight as the IM rate
///   (2%) would refuse ordinary closes during any fast move.
/// * It keeps the liquidation ladder (50 / 100 / 200 bps off the mark) and the
///   gateway's market-order slippage (500 bps off the touch) well inside, so
///   neither needs an exemption.
/// * Hyperliquid refuses resting orders far (tens of per cent) from the
///   reference and uses ~8–10% for market-order slippage; a single 10% band
///   is the conservative end of that, and one number is one rule the
///   executor, the matcher and the gateway all read.
pub const DERIVATIVE_PRICE_BAND_BPS: u128 = 1_000;

/// The inclusive `(lo, hi)` price band around `mark_q8`:
/// `lo = ceil(mark × (10_000 − BAND) / 10_000)`,
/// `hi = floor(mark × (10_000 + BAND) / 10_000)`. Both bounds are rounded
/// *toward* the mark, so rounding never widens the band.
pub fn derivative_price_band(mark_q8: u128) -> (u128, u128) {
    let lo = mark_q8
        .saturating_mul(10_000 - DERIVATIVE_PRICE_BAND_BPS)
        .div_ceil(10_000);
    let hi = mark_q8.saturating_mul(10_000 + DERIVATIVE_PRICE_BAND_BPS) / 10_000;
    (lo, hi)
}

/// Whether `price_q8` lies inside the band around `mark_q8`, edges included.
pub fn within_price_band(price_q8: u128, mark_q8: u128) -> bool {
    let (lo, hi) = derivative_price_band(mark_q8);
    lo <= price_q8 && price_q8 <= hi
}

/// Read-only snapshot of book state needed by `match_order`. The executor
/// implements this against `cb-state`'s `PerpOrders` / `PerpBookIndex`. Tests
/// implement it against an in-memory `HashMap`.
///
/// The trait is intentionally narrow: every method is a deterministic
/// function of stored data, no I/O hidden behind it. Implementers must walk
/// the book in canonical (price, seq) order — see `BookSnapshot::list_side`.
pub trait BookSnapshot {
    /// Resolve an order id to its full record. Returns `None` once the order
    /// has been removed from the book.
    fn get_perp_order(&self, oid: &H256) -> Option<PerpOrder>;

    /// Walk a market's side in **lex order** = ascending `(price_q8, seq)`.
    /// Caller (the matcher) reverses for buys to get best-bid-first.
    fn list_side(&self, market: &DerivId, side: Side) -> Vec<(u128, u64, H256)>;

    /// Most recent trade price on this market. M2's mark-price math reads
    /// this; here it's just plumbed through.
    fn last_trade_price(&self, market: &DerivId) -> Option<u128>;

    /// How much quantity an order from `owner` on `side` would *reduce* — the
    /// open size of `owner`'s position on `market` when that position is on the
    /// opposite side, and `0` when the owner is flat or already on `side`.
    ///
    /// This is the matcher's only window onto position state, and it exists so
    /// that the reduce-only rule is enforced where the fill plan is built
    /// rather than assumed to have been enforced somewhere upstream. There is
    /// deliberately **no default implementation**: a default returning
    /// "unlimited" would reintroduce exactly the gap this method closes, in any
    /// snapshot whose author did not notice the method existed.
    fn reduce_only_capacity(&self, owner: &Address, market: &DerivId, side: Side) -> u128;

    /// Recheck a maker against its live margin before adding a fill. `prior`
    /// contains this owner's earlier planned (price, quantity) fills, in
    /// execution order. A refusal cancels this maker and walks to the next.
    /// Pure matcher fixtures have no margin; the chain snapshot enforces it.
    fn maker_can_fill(&self, _maker: &PerpOrder, _qty: u128, _prior: &[(u128, u128)]) -> bool {
        true
    }

    /// The inclusive `(lo, hi)` band a fill's execution price must lie in on
    /// `market` — [`derivative_price_band`] around the current mark — or
    /// `None` when fills are not banded (a snapshot that never matches, or a
    /// unit-test book that sets no mark).
    ///
    /// The matcher reads this to bound *fills*, not just admission: a resting
    /// order was in band when it was placed, but the mark moves and the order
    /// does not. A maker whose price has left the band is cancelled at the
    /// moment an incoming order reaches it, with no fill, exactly like a
    /// reduce-only maker with nothing left to close. Since a fill is always at
    /// the maker's price, that is the same as "no derivative fill executes
    /// outside the band". No default, for the same reason
    /// `reduce_only_capacity` has none: a default of "unbanded" would reopen
    /// the gap in any snapshot whose author did not notice the method.
    fn price_band(&self, market: &DerivId) -> Option<(u128, u128)>;
}

/// What to do when an incoming order would cross with one of its own resting
/// orders. v1 default is `CancelTaker` (Hyperliquid-aligned). `CancelMaker`
/// is wired so the M2/M5 STP-by-account override has a place to land.
/// What to do when an incoming order would cross with one of its own resting
/// orders. v1 default is `CancelTaker` (Hyperliquid-aligned). `CancelMaker`
/// is wired so the M2/M5 STP-by-account override has a place to land.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SelfTradeAction {
    #[default]
    CancelTaker,
    CancelMaker,
}

/// Description of an order arriving at the matcher. The executor builds this
/// from `TxBody::PlacePerpOrder` after authority + reduce-only admission
/// checks have passed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncomingOrder {
    pub order_id: H256,
    pub owner: Address,
    pub market: DerivId,
    pub side: Side,
    pub price_q8: u128,
    pub qty: u128,
    pub tif: Tif,
    pub post_only: bool,
    pub reduce_only: bool,
    /// Sequence number assigned by the executor before calling the matcher.
    /// Globally monotonic across the chain (see `cb-block::seq` packing).
    pub seq: u64,
    pub placed_at_height: u64,
    pub client_order_id: H256,
    pub stp: SelfTradeAction,
}

/// One realised match between a taker and a maker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fill {
    /// The resting maker that crossed.
    pub maker_order_id: H256,
    /// The incoming order id (== `IncomingOrder.order_id`).
    pub taker_order_id: H256,
    pub maker: Address,
    pub taker: Address,
    /// Side of the **taker** — the resting maker is the opposite side.
    pub taker_side: Side,
    /// Trade price = the **maker's** resting price (price-time priority).
    pub price_q8: u128,
    pub qty: u128,
}

/// CF mutation the executor must apply. Emitted by the matcher in commit
/// order; the executor applies them sequentially. We don't put RocksDB writes
/// inline because:
///   - Pure core: makes the matcher unit-testable without a database.
///   - FOK rollback: a FOK that fails to fully fill produces zero mutations,
///     not "applied then reverted".
///   - Determinism: the order of mutations is part of the transition record
///     — not derived from HashMap iteration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BookMutation {
    /// Insert or update an order's record. Used to write the resting GTC
    /// remainder, and to update a partially-filled maker's `remaining_qty`.
    PutOrder(PerpOrder),
    /// Delete an order's record. Used when a maker is fully filled (or the
    /// taker's reduce-only-overflow remainder is silently cancelled).
    DeleteOrder { order_id: H256 },
    /// Insert into the book index. Used only for resting GTC orders.
    PutBookIndex {
        market: DerivId,
        side: Side,
        price_q8: u128,
        seq: u64,
        order_id: H256,
    },
    /// Remove from the book index. Used when a maker is fully filled.
    DeleteBookIndex {
        market: DerivId,
        side: Side,
        price_q8: u128,
        seq: u64,
    },
    /// Persist `last_trade_px_q8` / `last_trade_height` on the market record.
    /// The executor materialises this against `CF_PERP_MARKETS`. Only emitted
    /// when at least one fill happened.
    UpdateLastTrade {
        market: DerivId,
        price_q8: u128,
        height: u64,
    },
}

/// What happened to the incoming order overall.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchOutcome {
    /// Full quantity filled (any TIF).
    FullyFilled,
    /// Some quantity filled; the remainder cancelled (IOC).
    PartiallyFilledRemainderCancelled,
    /// FOK rejected because not the full quantity could fill.
    KilledFOK,
    /// GTC: some quantity filled, remainder rests in the book.
    PartiallyFilledRemainderResting,
    /// GTC: nothing filled, full quantity rests.
    Resting,
    /// Post-only would have crossed; rejected.
    RejectedPostOnlyCross,
    /// STP=CancelTaker fired. Whole order cancelled, no fills, no mutations.
    RejectedSelfTrade,
    /// IOC / market with no liquidity at the limit price.
    NoLiquidity,
}

/// Result of running `match_order`. The executor consumes `mutations` and
/// updates positions / fees from `fills`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchResult {
    pub fills: Vec<Fill>,
    pub mutations: Vec<BookMutation>,
    pub outcome: MatchOutcome,
    /// Quantity left unfilled. For `Resting` this equals the original qty;
    /// for `KilledFOK` and rejected variants it equals the original qty.
    pub remaining_qty: u128,
    /// Sum of all fills' qty. Convenience for callers; equals
    /// `incoming.qty - remaining_qty`.
    pub filled_qty: u128,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BookError {
    #[error("incoming order qty must be > 0")]
    ZeroQty,
    #[error("incoming order price_q8 must be > 0")]
    ZeroPrice,
    #[error("market id mismatch: incoming says {0:?} but order says {1:?}")]
    MarketMismatch(DerivId, DerivId),
}

/// The matching entry point.
///
/// The matcher is split into two phases:
///   1. **Plan.** Walk the opposite side of the book in price-time priority,
///      recording would-be fills, until either qty is exhausted or the next
///      maker price no longer crosses the incoming price.
///   2. **Commit.** If the plan is acceptable for the order's TIF (FOK
///      requires full fill; post-only requires zero fill), materialise the
///      mutations. Otherwise return early with no mutations.
///
/// Self-trade prevention (CancelTaker): if any maker found in the plan has
/// the same owner as the taker, the entire order is rejected. We check this
/// during planning so SelfTrade comes ahead of post-only; an
/// own-maker-cross still counts as a "would cross", but reporting STP gives
/// the trader cleaner feedback than a generic post-only-cross error.
/// One entry in the matcher's plan phase: a maker, how much of it this taker
/// consumes, what is left on it afterwards, and whether that remainder must
/// leave the book rather than keep resting (a reduce-only maker that has run
/// out of position to close).
struct PlannedFill {
    maker: PerpOrder,
    fill_qty: u128,
    maker_remaining_after: u128,
    cancel_maker_remainder: bool,
}

pub fn match_order<S: BookSnapshot>(
    state: &S,
    incoming: IncomingOrder,
) -> Result<MatchResult, BookError> {
    if incoming.qty == 0 {
        return Err(BookError::ZeroQty);
    }
    if incoming.price_q8 == 0 {
        return Err(BookError::ZeroPrice);
    }

    let market = incoming.market;
    let opposite = match incoming.side {
        Side::Buy => Side::Sell,
        Side::Sell => Side::Buy,
    };

    // Walk the opposite side. For sells we want lowest ask first (lex order
    // ascending). For buys we want highest bid first (lex order descending —
    // reverse the ascending walk).
    let mut book = state.list_side(&market, opposite);
    if opposite == Side::Buy {
        // Best bid = highest price, lowest seq within that price. The
        // backing CF iterator yields ascending (price, seq); reversing gives
        // (descending price, descending seq), but within a price level we
        // need ascending seq. Sort explicitly to enforce price-time priority
        // independent of the snapshot's internal walk order.
        book.sort_by(|(pa, sa, _), (pb, sb, _)| pb.cmp(pa).then(sa.cmp(sb)));
    } else {
        // Asks: ascending price, ascending seq within price. The list_side
        // contract already returns this, but sort defensively so a buggy
        // snapshot can't produce nondeterministic fills.
        book.sort_by(|(pa, sa, _), (pb, sb, _)| pa.cmp(pb).then(sa.cmp(sb)));
    }

    // Plan phase. We pull each maker order into memory so we can discount
    // any partially-consumed-earlier-in-this-run state without rewriting
    // book between iterations.
    let mut plan: Vec<PlannedFill> = Vec::new();
    // How much the taker may still fill. For a reduce-only taker this starts
    // capped at the position it is allowed to close, so an oversized flag can
    // never carry the order past flat and out the other side.
    let mut taker_budget = if incoming.reduce_only {
        std::cmp::min(
            incoming.qty,
            state.reduce_only_capacity(&incoming.owner, &market, incoming.side),
        )
    } else {
        incoming.qty
    };
    let mut taker_filled: u128 = 0;
    // Reduce-only capacity per maker owner as it stood before this match,
    // memoised across the plan. One owner can have several reduce-only orders
    // resting at different prices; they share one position, so they must
    // share one budget — what remains of it is this less `maker_filled`.
    let mut maker_ro_capacity: HashMap<Address, u128> = HashMap::new();
    // Everything each maker owner has been planned to fill so far in this
    // match, reduce-only or not. All of one owner's makers sit on the same
    // side, so every one of those fills moves the owner's position the same
    // way, and a plain order that fills ahead of a reduce-only one has
    // already used up (or overshot) the position the reduce-only order was
    // allowed to close. The budget has to be charged for it: without this, a
    // long of 1 with a plain ask of 4 and a reduce-only ask of 20 resting was
    // taken to short 3 by the plain ask and then to short 4 by the
    // "reduce-only" one, which opened size it exists to never open.
    // (Test: `perp_engine_properties::reduce_only_never_grows_a_position_on_either_side_of_a_fill`.)
    let mut maker_filled: HashMap<Address, u128> = HashMap::new();
    let mut maker_planned: HashMap<Address, Vec<(u128, u128)>> = HashMap::new();
    let mut self_trade_seen = false;
    let band = state.price_band(&market);

    for (px, _seq, oid) in &book {
        if taker_budget == 0 {
            break;
        }
        // Cross condition.
        let crosses = match incoming.side {
            Side::Buy => incoming.price_q8 >= *px,
            Side::Sell => incoming.price_q8 <= *px,
        };
        if !crosses {
            break;
        }

        let maker = match state.get_perp_order(oid) {
            Some(o) => o,
            // Stale index entry — skip. Should not normally happen because
            // the executor keeps the index and order CF in lockstep, but a
            // crash mid-write could leave one. Tolerate quietly.
            None => continue,
        };

        // Sanity: index entry pointed at the wrong market or wrong side.
        if maker.market != market || maker.side != opposite {
            continue;
        }

        // Price band. A maker whose price has drifted outside the band
        // around the current mark (the mark moved, the order did not) is
        // cancelled here without a fill and the walk continues past it — the
        // same delete-without-fill path a spent reduce-only maker takes. This
        // runs before self-trade prevention, so the taker's own stale quote is
        // cleared rather than blocking the order. See `price_band`.
        if let Some((lo, hi)) = band {
            if maker.price_q8 < lo || maker.price_q8 > hi {
                plan.push(PlannedFill {
                    maker,
                    fill_qty: 0,
                    maker_remaining_after: 0,
                    cancel_maker_remainder: true,
                });
                continue;
            }
        }

        // Self-trade detection. Flag and stop immediately under CancelTaker;
        // CancelMaker variant emits a delete and continues (M5 territory —
        // here we cancel the maker and skip it from the plan).
        if maker.owner == incoming.owner {
            self_trade_seen = true;
            match incoming.stp {
                SelfTradeAction::CancelTaker => break,
                SelfTradeAction::CancelMaker => {
                    // Delete the maker outright; no fill against it.
                    plan.push(PlannedFill {
                        maker: maker.clone(),
                        fill_qty: 0,
                        maker_remaining_after: 0,
                        cancel_maker_remainder: true,
                    });
                    continue;
                }
            }
        }

        let mut fill_qty = std::cmp::min(taker_budget, maker.remaining_qty);
        // Reduce-only maker revalidation. The flag was checked against the
        // maker's position when the order was placed; that was possibly many
        // blocks ago, and the position may since have been closed, flipped or
        // shrunk. Re-derive the capacity now and cap the fill against it.
        let mut cancel_maker_remainder = false;
        if maker.reduce_only {
            // The position as it stood before this match, memoised...
            let capacity = match maker_ro_capacity.get(&maker.owner) {
                Some(b) => *b,
                None => {
                    let b = state.reduce_only_capacity(&maker.owner, &market, maker.side);
                    maker_ro_capacity.insert(maker.owner, b);
                    b
                }
            };
            // ...less whatever this owner has already filled in the plan.
            let used = maker_filled.get(&maker.owner).copied().unwrap_or(0);
            let budget = capacity.saturating_sub(used);
            fill_qty = std::cmp::min(fill_qty, budget);
            let budget_after = budget - fill_qty;
            // Whatever the maker would still be resting for beyond its
            // remaining capacity can only ever open a position, so it leaves
            // the book instead of waiting to do so.
            cancel_maker_remainder = maker.remaining_qty - fill_qty > budget_after;
        }
        if fill_qty == 0 {
            if cancel_maker_remainder {
                plan.push(PlannedFill {
                    maker,
                    fill_qty: 0,
                    maker_remaining_after: 0,
                    cancel_maker_remainder: true,
                });
            }
            continue;
        }
        let prior = maker_planned.entry(maker.owner).or_default();
        if !state.maker_can_fill(&maker, fill_qty, prior) {
            plan.push(PlannedFill {
                maker,
                fill_qty: 0,
                maker_remaining_after: 0,
                cancel_maker_remainder: true,
            });
            continue;
        }
        prior.push((maker.price_q8, fill_qty));
        let maker_remaining_after = maker.remaining_qty - fill_qty;
        *maker_filled.entry(maker.owner).or_insert(0) += fill_qty;
        plan.push(PlannedFill {
            maker,
            fill_qty,
            maker_remaining_after,
            cancel_maker_remainder,
        });
        taker_budget -= fill_qty;
        taker_filled += fill_qty;
    }

    // STP: CancelTaker rejection. No fills, no mutations.
    if self_trade_seen && incoming.stp == SelfTradeAction::CancelTaker {
        return Ok(MatchResult {
            fills: Vec::new(),
            mutations: Vec::new(),
            outcome: MatchOutcome::RejectedSelfTrade,
            remaining_qty: incoming.qty,
            filled_qty: 0,
        });
    }

    let filled_qty = taker_filled;

    // Post-only: if we'd have filled anything at all, reject.
    if incoming.post_only && filled_qty > 0 {
        return Ok(MatchResult {
            fills: Vec::new(),
            mutations: Vec::new(),
            outcome: MatchOutcome::RejectedPostOnlyCross,
            remaining_qty: incoming.qty,
            filled_qty: 0,
        });
    }

    // FOK: must be full or nothing.
    if incoming.tif == Tif::Fok && filled_qty < incoming.qty {
        return Ok(MatchResult {
            fills: Vec::new(),
            mutations: Vec::new(),
            outcome: MatchOutcome::KilledFOK,
            remaining_qty: incoming.qty,
            filled_qty: 0,
        });
    }

    // Commit phase.
    let mut fills: Vec<Fill> = Vec::with_capacity(plan.len());
    let mut mutations: Vec<BookMutation> = Vec::new();
    for PlannedFill {
        maker,
        fill_qty,
        maker_remaining_after,
        cancel_maker_remainder,
    } in plan
    {
        if fill_qty == 0 {
            // The self-trade-cancel-maker case, a reduce-only maker with no
            // capacity left, or a maker priced outside the band. All drop the
            // maker without a fill.
            mutations.push(BookMutation::DeleteOrder {
                order_id: maker.order_id,
            });
            mutations.push(BookMutation::DeleteBookIndex {
                market,
                side: maker.side,
                price_q8: maker.price_q8,
                seq: maker.seq,
            });
            continue;
        }

        fills.push(Fill {
            maker_order_id: maker.order_id,
            taker_order_id: incoming.order_id,
            maker: maker.owner,
            taker: incoming.owner,
            taker_side: incoming.side,
            price_q8: maker.price_q8,
            qty: fill_qty,
        });

        if maker_remaining_after == 0 || cancel_maker_remainder {
            mutations.push(BookMutation::DeleteOrder {
                order_id: maker.order_id,
            });
            mutations.push(BookMutation::DeleteBookIndex {
                market,
                side: maker.side,
                price_q8: maker.price_q8,
                seq: maker.seq,
            });
        } else {
            // Partial fill: keep the maker in the book with reduced qty.
            let mut updated = maker.clone();
            updated.remaining_qty = maker_remaining_after;
            mutations.push(BookMutation::PutOrder(updated));
            // Book index unchanged: same (price, seq) → same key → already
            // points at the same order_id. No mutation needed.
        }
    }

    // Last-trade publish: only if we actually filled.
    if filled_qty > 0 {
        if let Some(last_fill) = fills.last() {
            mutations.push(BookMutation::UpdateLastTrade {
                market,
                price_q8: last_fill.price_q8,
                height: incoming.placed_at_height,
            });
        }
    }

    // Outcome + resting handling.
    let remaining_qty = incoming.qty - filled_qty;
    let outcome = match incoming.tif {
        Tif::Fok => {
            // We already early-returned for partial; here remaining_qty == 0.
            MatchOutcome::FullyFilled
        }
        Tif::Ioc => {
            if remaining_qty == 0 {
                MatchOutcome::FullyFilled
            } else if filled_qty == 0 {
                MatchOutcome::NoLiquidity
            } else {
                MatchOutcome::PartiallyFilledRemainderCancelled
            }
        }
        Tif::Gtc => {
            if remaining_qty == 0 {
                MatchOutcome::FullyFilled
            } else if incoming.reduce_only
                && remaining_qty
                    > state
                        .reduce_only_capacity(&incoming.owner, &market, incoming.side)
                        .saturating_sub(filled_qty)
            {
                // A reduce-only remainder larger than the position it could
                // still close would be an order that can only open one. Cancel
                // it instead of resting it — the same rule that deletes a stale
                // reduce-only maker above, applied at the moment of resting.
                if filled_qty == 0 {
                    MatchOutcome::NoLiquidity
                } else {
                    MatchOutcome::PartiallyFilledRemainderCancelled
                }
            } else {
                // Rest the remainder. The matcher writes the resting order
                // record AND its book-index entry; the executor then has a
                // single mutation list to apply.
                let resting = PerpOrder {
                    order_id: incoming.order_id,
                    owner: incoming.owner,
                    market,
                    side: incoming.side,
                    price_q8: incoming.price_q8,
                    qty: incoming.qty,
                    remaining_qty,
                    tif: incoming.tif,
                    post_only: incoming.post_only,
                    reduce_only: incoming.reduce_only,
                    seq: incoming.seq,
                    placed_at_height: incoming.placed_at_height,
                    client_order_id: incoming.client_order_id,
                };
                mutations.push(BookMutation::PutOrder(resting));
                mutations.push(BookMutation::PutBookIndex {
                    market,
                    side: incoming.side,
                    price_q8: incoming.price_q8,
                    seq: incoming.seq,
                    order_id: incoming.order_id,
                });
                if filled_qty == 0 {
                    MatchOutcome::Resting
                } else {
                    MatchOutcome::PartiallyFilledRemainderResting
                }
            }
        }
    };

    Ok(MatchResult {
        fills,
        mutations,
        outcome,
        remaining_qty,
        filled_qty,
    })
}

/// Read-only helper. Returns `(best_bid, best_ask)` in `price_q8` units.
/// Either side can be `None` when its book is empty.
///
/// M2's mark-price computation reads this. Pure: derives entirely from the
/// snapshot, no I/O.
pub fn best_bid_ask<S: BookSnapshot>(state: &S, market: &DerivId) -> (Option<u128>, Option<u128>) {
    let bids = state.list_side(market, Side::Buy);
    let asks = state.list_side(market, Side::Sell);
    let best_bid = bids.iter().map(|(p, _, _)| *p).max();
    let best_ask = asks.iter().map(|(p, _, _)| *p).min();
    (best_bid, best_ask)
}

/// Read-only helper. Returns the most recent trade price on a market, or
/// `None` if the market has not yet printed a trade. M2 consumes this for
/// mark-price recompute.
pub fn last_trade_price<S: BookSnapshot>(state: &S, market: &DerivId) -> Option<u128> {
    state.last_trade_price(market)
}

// ---------------------------------------------------------------------------
// In-memory snapshot: used by tests in this crate AND by the integration test
// in cb-block (it lifts a real BlockState into one of these for assertions).
// ---------------------------------------------------------------------------

/// Toy `BookSnapshot` impl for tests. Production code uses the `cb-state`-
/// backed impl in `cb-block::executor`.
#[derive(Default, Clone)]
pub struct InMemoryBook {
    pub orders: HashMap<H256, PerpOrder>,
    pub last_trade_by_market: HashMap<[u8; 4], u128>,
    /// Open positions, keyed by `(owner, market)` as `(side, qty)`. Only the
    /// reduce-only cap reads this; a test that never places a reduce-only
    /// order can leave it empty, which reads as "everyone is flat".
    pub positions: HashMap<(Address, [u8; 4]), (Side, u128)>,
    /// Mark per market, for [`BookSnapshot::price_band`]. A market with no
    /// entry is unbanded, which is what the matcher unit tests want.
    pub mark_by_market: HashMap<[u8; 4], u128>,
}

impl InMemoryBook {
    pub fn insert(&mut self, o: PerpOrder) {
        self.orders.insert(o.order_id, o);
    }

    /// Seed an open position so [`BookSnapshot::reduce_only_capacity`] has
    /// something to report. `qty == 0` means flat.
    pub fn set_position(&mut self, owner: Address, market: DerivId, side: Side, qty: u128) {
        self.positions
            .insert((owner, market.as_bytes()), (side, qty));
    }

    /// Apply the matcher's mutation stream so we can chain calls in tests.
    pub fn apply(&mut self, mutation: &BookMutation) {
        match mutation {
            BookMutation::PutOrder(o) => {
                self.orders.insert(o.order_id, o.clone());
            }
            BookMutation::DeleteOrder { order_id } => {
                self.orders.remove(order_id);
            }
            BookMutation::PutBookIndex { .. } | BookMutation::DeleteBookIndex { .. } => {
                // The in-memory book derives its index from `orders`; nothing
                // to do here.
            }
            BookMutation::UpdateLastTrade {
                market, price_q8, ..
            } => {
                self.last_trade_by_market
                    .insert(market.as_bytes(), *price_q8);
            }
        }
    }

    pub fn apply_all(&mut self, mutations: &[BookMutation]) {
        for m in mutations {
            self.apply(m);
        }
    }
}

impl BookSnapshot for InMemoryBook {
    fn get_perp_order(&self, oid: &H256) -> Option<PerpOrder> {
        self.orders.get(oid).cloned()
    }

    fn list_side(&self, market: &DerivId, side: Side) -> Vec<(u128, u64, H256)> {
        // IMPORTANT: HashMap iteration order is nondeterministic. Sort the
        // output explicitly so the matcher's plan-phase walk is stable —
        // this is the same invariant we enforce on the rocksdb-backed snapshot.
        let mut out: Vec<(u128, u64, H256)> = self
            .orders
            .values()
            .filter(|o| o.market == *market && o.side == side && o.remaining_qty > 0)
            .map(|o| (o.price_q8, o.seq, o.order_id))
            .collect();
        out.sort_by(|(pa, sa, _), (pb, sb, _)| pa.cmp(pb).then(sa.cmp(sb)));
        out
    }

    fn last_trade_price(&self, market: &DerivId) -> Option<u128> {
        self.last_trade_by_market.get(&market.as_bytes()).copied()
    }

    fn reduce_only_capacity(&self, owner: &Address, market: &DerivId, side: Side) -> u128 {
        match self.positions.get(&(*owner, market.as_bytes())) {
            Some((pos_side, qty)) if *pos_side != side => *qty,
            _ => 0,
        }
    }
    fn price_band(&self, market: &DerivId) -> Option<(u128, u128)> {
        self.mark_by_market
            .get(&market.as_bytes())
            .map(|m| derivative_price_band(*m))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_order(
        oid_byte: u8,
        owner_byte: u8,
        side: Side,
        price_q8: u128,
        qty: u128,
        seq: u64,
    ) -> PerpOrder {
        PerpOrder {
            order_id: H256([oid_byte; 32]),
            owner: Address([owner_byte; 20]),
            market: DerivId::perp(0),
            side,
            price_q8,
            qty,
            remaining_qty: qty,
            tif: Tif::Gtc,
            post_only: false,
            reduce_only: false,
            seq,
            placed_at_height: 1,
            client_order_id: H256([0; 32]),
        }
    }

    fn mk_incoming(
        oid_byte: u8,
        owner_byte: u8,
        side: Side,
        price_q8: u128,
        qty: u128,
        tif: Tif,
        seq: u64,
    ) -> IncomingOrder {
        IncomingOrder {
            order_id: H256([oid_byte; 32]),
            owner: Address([owner_byte; 20]),
            market: DerivId::perp(0),
            side,
            price_q8,
            qty,
            tif,
            post_only: false,
            reduce_only: false,
            seq,
            placed_at_height: 2,
            client_order_id: H256([0; 32]),
            stp: SelfTradeAction::CancelTaker,
        }
    }

    #[test]
    fn maker_risk_budget_is_shared_across_quotes_and_fok_stays_atomic() {
        struct CappedBook(InMemoryBook);
        impl BookSnapshot for CappedBook {
            fn get_perp_order(&self, oid: &H256) -> Option<PerpOrder> {
                self.0.get_perp_order(oid)
            }
            fn list_side(&self, market: &DerivId, side: Side) -> Vec<(u128, u64, H256)> {
                self.0.list_side(market, side)
            }
            fn last_trade_price(&self, market: &DerivId) -> Option<u128> {
                self.0.last_trade_price(market)
            }
            fn reduce_only_capacity(&self, owner: &Address, market: &DerivId, side: Side) -> u128 {
                self.0.reduce_only_capacity(owner, market, side)
            }
            fn price_band(&self, market: &DerivId) -> Option<(u128, u128)> {
                self.0.price_band(market)
            }
            fn maker_can_fill(&self, _: &PerpOrder, qty: u128, prior: &[(u128, u128)]) -> bool {
                // Each owner can fund ten units, regardless of quote count.
                qty + prior.iter().map(|(_, qty)| qty).sum::<u128>() <= 10
            }
        }
        let mut book = CappedBook(InMemoryBook::default());
        book.0.insert(mk_order(1, 0xa, Side::Sell, 100, 10, 1));
        book.0.insert(mk_order(2, 0xa, Side::Sell, 101, 10, 2));
        book.0.insert(mk_order(3, 0xb, Side::Sell, 102, 10, 3));
        let fok = match_order(&book, mk_incoming(4, 0xc, Side::Buy, 102, 30, Tif::Fok, 4)).unwrap();
        assert_eq!(fok.outcome, MatchOutcome::KilledFOK);
        assert!(fok.fills.is_empty());
        assert!(fok.mutations.is_empty());

        let ioc = match_order(&book, mk_incoming(4, 0xc, Side::Buy, 102, 30, Tif::Ioc, 4)).unwrap();
        assert_eq!(ioc.filled_qty, 20);
        assert_eq!(ioc.fills.len(), 2);
        assert_eq!(ioc.fills[0].maker, Address([0xa; 20]));
        assert_eq!(ioc.fills[1].maker, Address([0xb; 20]));
        book.0.apply_all(&ioc.mutations);
        assert!(book.0.orders.is_empty(), "unfunded quote was cancelled");
    }

    #[test]
    fn empty_book_gtc_rests_full_qty() {
        let book = InMemoryBook::default();
        let inc = mk_incoming(1, 0xa, Side::Buy, 100, 50, Tif::Gtc, 7);
        let res = match_order(&book, inc).unwrap();
        assert_eq!(res.outcome, MatchOutcome::Resting);
        assert!(res.fills.is_empty());
        assert_eq!(res.remaining_qty, 50);
        assert_eq!(res.filled_qty, 0);
        // Two mutations: PutOrder + PutBookIndex. No UpdateLastTrade since
        // no fill happened.
        assert_eq!(res.mutations.len(), 2);
    }

    #[test]
    fn empty_book_ioc_returns_no_liquidity() {
        let book = InMemoryBook::default();
        let inc = mk_incoming(1, 0xa, Side::Buy, 100, 50, Tif::Ioc, 7);
        let res = match_order(&book, inc).unwrap();
        assert_eq!(res.outcome, MatchOutcome::NoLiquidity);
        assert!(res.fills.is_empty());
        assert!(res.mutations.is_empty());
    }

    #[test]
    fn empty_book_fok_killed() {
        let book = InMemoryBook::default();
        let inc = mk_incoming(1, 0xa, Side::Buy, 100, 50, Tif::Fok, 7);
        let res = match_order(&book, inc).unwrap();
        assert_eq!(res.outcome, MatchOutcome::KilledFOK);
        assert!(res.fills.is_empty());
        assert!(res.mutations.is_empty());
    }

    #[test]
    fn simple_full_cross_buy_lifts_ask() {
        let mut book = InMemoryBook::default();
        book.insert(mk_order(0xab, 0xb1, Side::Sell, 100, 10, 1));
        let inc = mk_incoming(0xcd, 0xa1, Side::Buy, 100, 10, Tif::Ioc, 2);
        let res = match_order(&book, inc).unwrap();
        assert_eq!(res.outcome, MatchOutcome::FullyFilled);
        assert_eq!(res.fills.len(), 1);
        assert_eq!(res.fills[0].qty, 10);
        assert_eq!(res.fills[0].price_q8, 100);
        assert_eq!(res.fills[0].taker_side, Side::Buy);
        // Maker fully consumed ⇒ delete maker order + index.
        let mut deletes = 0usize;
        let mut last_trades = 0usize;
        for m in &res.mutations {
            match m {
                BookMutation::DeleteOrder { .. } | BookMutation::DeleteBookIndex { .. } => {
                    deletes += 1
                }
                BookMutation::UpdateLastTrade { price_q8, .. } => {
                    assert_eq!(*price_q8, 100);
                    last_trades += 1;
                }
                _ => {}
            }
        }
        assert_eq!(deletes, 2);
        assert_eq!(last_trades, 1);
    }

    #[test]
    fn partial_fill_against_thicker_maker_keeps_maker_in_book() {
        let mut book = InMemoryBook::default();
        book.insert(mk_order(0xab, 0xb1, Side::Sell, 100, 50, 1));
        let inc = mk_incoming(0xcd, 0xa1, Side::Buy, 100, 10, Tif::Ioc, 2);
        let res = match_order(&book, inc).unwrap();
        assert_eq!(res.outcome, MatchOutcome::FullyFilled);
        assert_eq!(res.fills[0].qty, 10);
        // Expect a PutOrder for the maker with remaining_qty == 40, NO delete.
        let put_order = res.mutations.iter().find_map(|m| match m {
            BookMutation::PutOrder(o) => Some(o.clone()),
            _ => None,
        });
        assert_eq!(put_order.unwrap().remaining_qty, 40);
        assert!(!res
            .mutations
            .iter()
            .any(|m| matches!(m, BookMutation::DeleteOrder { .. })));
    }

    #[test]
    fn ioc_partial_remainder_cancels_no_resting() {
        let mut book = InMemoryBook::default();
        book.insert(mk_order(0xab, 0xb1, Side::Sell, 100, 5, 1));
        let inc = mk_incoming(0xcd, 0xa1, Side::Buy, 100, 10, Tif::Ioc, 2);
        let res = match_order(&book, inc).unwrap();
        assert_eq!(res.outcome, MatchOutcome::PartiallyFilledRemainderCancelled);
        assert_eq!(res.filled_qty, 5);
        assert_eq!(res.remaining_qty, 5);
        // No new resting order — no PutOrder for the taker.
        let taker_id = H256([0xcd; 32]);
        assert!(!res.mutations.iter().any(|m| matches!(
            m,
            BookMutation::PutOrder(o) if o.order_id == taker_id
        )));
    }

    #[test]
    fn fok_kills_when_book_too_thin() {
        let mut book = InMemoryBook::default();
        book.insert(mk_order(0xab, 0xb1, Side::Sell, 100, 5, 1));
        let inc = mk_incoming(0xcd, 0xa1, Side::Buy, 100, 10, Tif::Fok, 2);
        let res = match_order(&book, inc).unwrap();
        assert_eq!(res.outcome, MatchOutcome::KilledFOK);
        assert!(res.fills.is_empty());
        assert!(res.mutations.is_empty());
    }

    #[test]
    fn fok_succeeds_when_book_has_enough() {
        let mut book = InMemoryBook::default();
        book.insert(mk_order(0xab, 0xb1, Side::Sell, 100, 30, 1));
        let inc = mk_incoming(0xcd, 0xa1, Side::Buy, 100, 10, Tif::Fok, 2);
        let res = match_order(&book, inc).unwrap();
        assert_eq!(res.outcome, MatchOutcome::FullyFilled);
        assert_eq!(res.filled_qty, 10);
    }

    #[test]
    fn post_only_rejected_if_would_cross() {
        let mut book = InMemoryBook::default();
        book.insert(mk_order(0xab, 0xb1, Side::Sell, 100, 10, 1));
        let mut inc = mk_incoming(0xcd, 0xa1, Side::Buy, 100, 10, Tif::Gtc, 2);
        inc.post_only = true;
        let res = match_order(&book, inc).unwrap();
        assert_eq!(res.outcome, MatchOutcome::RejectedPostOnlyCross);
        assert!(res.fills.is_empty());
        assert!(res.mutations.is_empty());
    }

    #[test]
    fn post_only_rests_if_no_cross() {
        let mut book = InMemoryBook::default();
        book.insert(mk_order(0xab, 0xb1, Side::Sell, 200, 10, 1));
        let mut inc = mk_incoming(0xcd, 0xa1, Side::Buy, 100, 10, Tif::Gtc, 2);
        inc.post_only = true;
        let res = match_order(&book, inc).unwrap();
        assert_eq!(res.outcome, MatchOutcome::Resting);
        assert_eq!(res.mutations.len(), 2);
    }

    #[test]
    fn self_trade_prevention_cancels_taker() {
        // Both maker and taker have owner == 0xaa.
        let mut book = InMemoryBook::default();
        book.insert(mk_order(0xab, 0xaa, Side::Sell, 100, 10, 1));
        let inc = mk_incoming(0xcd, 0xaa, Side::Buy, 100, 10, Tif::Ioc, 2);
        let res = match_order(&book, inc).unwrap();
        assert_eq!(res.outcome, MatchOutcome::RejectedSelfTrade);
        assert!(res.fills.is_empty());
        assert!(res.mutations.is_empty());
    }

    #[test]
    fn self_trade_only_fires_when_actually_crossing_own_order() {
        // Maker on the OPPOSITE owner; the taker should fill normally even
        // if a same-owner maker exists deeper in the book.
        let mut book = InMemoryBook::default();
        // Best ask is the OTHER owner — no STP fires.
        book.insert(mk_order(0xab, 0xb1, Side::Sell, 100, 10, 1));
        // Same owner ask further away — would only matter if the taker walks deeper.
        book.insert(mk_order(0xac, 0xaa, Side::Sell, 200, 10, 2));
        let inc = mk_incoming(0xcd, 0xaa, Side::Buy, 100, 5, Tif::Ioc, 3);
        let res = match_order(&book, inc).unwrap();
        assert_eq!(res.outcome, MatchOutcome::FullyFilled);
        assert_eq!(res.fills.len(), 1);
        assert_eq!(res.fills[0].maker, Address([0xb1; 20]));
    }

    #[test]
    fn buy_walks_lowest_ask_first() {
        let mut book = InMemoryBook::default();
        // Two asks: the cheaper one (price=100) MUST cross first.
        book.insert(mk_order(0xa1, 0xb1, Side::Sell, 200, 10, 1));
        book.insert(mk_order(0xa2, 0xb2, Side::Sell, 100, 10, 2));
        let inc = mk_incoming(0xcd, 0xaa, Side::Buy, 250, 10, Tif::Ioc, 3);
        let res = match_order(&book, inc).unwrap();
        assert_eq!(res.fills.len(), 1);
        assert_eq!(res.fills[0].price_q8, 100);
        assert_eq!(res.fills[0].maker, Address([0xb2; 20]));
    }

    #[test]
    fn sell_walks_highest_bid_first() {
        let mut book = InMemoryBook::default();
        book.insert(mk_order(0xa1, 0xb1, Side::Buy, 100, 10, 1));
        book.insert(mk_order(0xa2, 0xb2, Side::Buy, 200, 10, 2));
        let inc = mk_incoming(0xcd, 0xaa, Side::Sell, 50, 10, Tif::Ioc, 3);
        let res = match_order(&book, inc).unwrap();
        assert_eq!(res.fills.len(), 1);
        assert_eq!(res.fills[0].price_q8, 200);
        assert_eq!(res.fills[0].maker, Address([0xb2; 20]));
    }

    #[test]
    fn equal_price_uses_lowest_seq_first() {
        let mut book = InMemoryBook::default();
        // Two asks at the same price; lower seq is older and must fill first.
        book.insert(mk_order(0xa1, 0xb1, Side::Sell, 100, 10, 5));
        book.insert(mk_order(0xa2, 0xb2, Side::Sell, 100, 10, 1));
        let inc = mk_incoming(0xcd, 0xaa, Side::Buy, 100, 5, Tif::Ioc, 9);
        let res = match_order(&book, inc).unwrap();
        assert_eq!(res.fills.len(), 1);
        // seq 1 (oid 0xa2) should match before seq 5 (oid 0xa1).
        assert_eq!(res.fills[0].maker_order_id, H256([0xa2; 32]));
    }

    #[test]
    fn updates_last_trade_price_on_fill() {
        let mut book = InMemoryBook::default();
        book.insert(mk_order(0xa1, 0xb1, Side::Sell, 100, 10, 1));
        let inc = mk_incoming(0xcd, 0xaa, Side::Buy, 100, 10, Tif::Ioc, 2);
        let res = match_order(&book, inc).unwrap();
        book.apply_all(&res.mutations);
        assert_eq!(last_trade_price(&book, &DerivId::perp(0)), Some(100));
    }

    #[test]
    fn best_bid_ask_reads_book() {
        let mut book = InMemoryBook::default();
        assert_eq!(best_bid_ask(&book, &DerivId::perp(0)), (None, None));
        book.insert(mk_order(0xa1, 0xb1, Side::Buy, 100, 10, 1));
        book.insert(mk_order(0xa2, 0xb1, Side::Buy, 90, 10, 2));
        book.insert(mk_order(0xa3, 0xb1, Side::Sell, 110, 10, 3));
        book.insert(mk_order(0xa4, 0xb1, Side::Sell, 120, 10, 4));
        assert_eq!(
            best_bid_ask(&book, &DerivId::perp(0)),
            (Some(100), Some(110))
        );
    }

    #[test]
    fn does_not_cross_through_limit_price() {
        // Taker buy with limit 100; only ask is at 110 → no fill.
        let mut book = InMemoryBook::default();
        book.insert(mk_order(0xa1, 0xb1, Side::Sell, 110, 10, 1));
        let inc = mk_incoming(0xcd, 0xaa, Side::Buy, 100, 10, Tif::Ioc, 2);
        let res = match_order(&book, inc).unwrap();
        assert_eq!(res.outcome, MatchOutcome::NoLiquidity);
        assert!(res.fills.is_empty());
    }

    #[test]
    fn the_band_rounds_inward_and_includes_its_edges() {
        assert_eq!(derivative_price_band(1_000), (900, 1_100));
        // 999 · 0.9 = 899.1 → 900; 999 · 1.1 = 1098.9 → 1098.
        assert_eq!(derivative_price_band(999), (900, 1_098));
        assert!(within_price_band(900, 1_000) && within_price_band(1_100, 1_000));
        assert!(!within_price_band(899, 1_000) && !within_price_band(1_101, 1_000));
    }

    #[test]
    fn a_maker_outside_the_band_is_cancelled_and_the_walk_continues() {
        // Mark 1,000 → band [900, 1,100]. The best ask (850) has been left
        // below the band by a rising mark; the next (950) is inside it.
        let mut book = InMemoryBook::default();
        book.mark_by_market
            .insert(DerivId::perp(0).as_bytes(), 1_000);
        book.insert(mk_order(0xa1, 0xb1, Side::Sell, 850, 5, 1));
        book.insert(mk_order(0xa2, 0xb2, Side::Sell, 950, 5, 2));
        let inc = mk_incoming(0xcd, 0xaa, Side::Buy, 1_000, 10, Tif::Ioc, 9);
        let res = match_order(&book, inc).unwrap();
        assert_eq!(res.fills.len(), 1);
        assert_eq!((res.fills[0].price_q8, res.fills[0].qty), (950, 5));
        assert_eq!(res.outcome, MatchOutcome::PartiallyFilledRemainderCancelled);
        book.apply_all(&res.mutations);
        assert!(
            book.orders.is_empty(),
            "the stale ask is gone, the other filled"
        );

        // A post-only order that reaches only a stale maker is not a cross:
        // it rests, and the stale maker is still cleared.
        let mut book = InMemoryBook::default();
        book.mark_by_market
            .insert(DerivId::perp(0).as_bytes(), 1_000);
        book.insert(mk_order(0xa1, 0xb1, Side::Sell, 850, 5, 1));
        let mut inc = mk_incoming(0xcd, 0xaa, Side::Buy, 950, 5, Tif::Gtc, 9);
        inc.post_only = true;
        let res = match_order(&book, inc).unwrap();
        assert_eq!(res.outcome, MatchOutcome::Resting);
        book.apply_all(&res.mutations);
        assert_eq!(book.orders.len(), 1);
        assert!(book.orders.values().all(|o| o.side == Side::Buy));
    }

    #[test]
    fn rejects_zero_qty() {
        let book = InMemoryBook::default();
        let inc = mk_incoming(0xcd, 0xaa, Side::Buy, 100, 0, Tif::Ioc, 1);
        assert_eq!(match_order(&book, inc), Err(BookError::ZeroQty));
    }

    #[test]
    fn rejects_zero_price() {
        let book = InMemoryBook::default();
        let inc = mk_incoming(0xcd, 0xaa, Side::Buy, 0, 10, Tif::Ioc, 1);
        assert_eq!(match_order(&book, inc), Err(BookError::ZeroPrice));
    }

    #[test]
    fn fills_walk_multiple_makers_in_priority_order() {
        let mut book = InMemoryBook::default();
        // Three asks at increasing prices.
        book.insert(mk_order(0xa1, 0xb1, Side::Sell, 100, 5, 1));
        book.insert(mk_order(0xa2, 0xb2, Side::Sell, 110, 5, 2));
        book.insert(mk_order(0xa3, 0xb3, Side::Sell, 120, 5, 3));
        let inc = mk_incoming(0xcd, 0xaa, Side::Buy, 200, 12, Tif::Ioc, 9);
        let res = match_order(&book, inc).unwrap();
        assert_eq!(res.outcome, MatchOutcome::FullyFilled);
        assert_eq!(res.fills.len(), 3);
        assert_eq!(res.fills[0].price_q8, 100);
        assert_eq!(res.fills[1].price_q8, 110);
        assert_eq!(res.fills[2].price_q8, 120);
        assert_eq!(res.fills[0].qty + res.fills[1].qty + res.fills[2].qty, 12);
        // Last trade is the deepest fill price.
        let utl = res
            .mutations
            .iter()
            .find_map(|m| match m {
                BookMutation::UpdateLastTrade { price_q8, .. } => Some(*price_q8),
                _ => None,
            })
            .unwrap();
        assert_eq!(utl, 120);
    }
}
