//! Pure spot CLOB matching engine.
//!
//! `SpotBook` is the in-memory matcher; `place` is the entry point. It mirrors
//! the design of `crate::book` (the perp matcher) — same price-time priority,
//! same TIF semantics, same self-trade policy (`CancelTaker`) — with a smaller
//! API surface tailored to the spot executor wiring planned for milestone S3.
//!
//! ## Shape divergences from `crate::book` (and why)
//!
//! 1. **No `BookSnapshot` trait.** The perp matcher accepts a read-only
//!    snapshot (it's pure on top of an externally-owned book) and emits
//!    mutations the executor materialises against RocksDB. Here `SpotBook`
//!    owns the resting state directly: simpler call shape for tests and for
//!    callers that hold an in-memory book. The executor (S3) will keep a
//!    `BookSnapshot`-style trait on its own RocksDB-backed wrapper and use
//!    the `BookMutation` list this matcher emits to keep both views in sync.
//! 2. **No `MarketId` on `IncomingOrder`.** A `SpotBook` instance models a
//!    single market. The executor instantiates one `SpotBook` per market (or
//!    builds a per-market view over the global book CFs).
//! 3. **No fees.** Same boundary as `crate::book`: the matcher emits `Fill`
//!    records; the executor applies `MarketConfig.fee_bps` to the taker side
//!    in `quote`. Mirrors `book.rs:50–53`.
//! 4. **Simpler outcome surface.** `MatchResult.rejected: Option<RejectReason>`
//!    replaces `MatchOutcome`; `rest: Option<RestingOrder>` replaces the
//!    plumbed-back resting record. The same information is recoverable: a
//!    full-fill is `rejected=None && rest=None && !fills.empty()`; a kill
//!    is `rejected=Some(_) && fills.empty() && mutations.empty()`.
//!
//! ## Determinism contract
//!
//! - All ordered iteration goes through `BTreeMap` keyed by `(price, seq)`.
//!   No `HashMap` walk on a consensus path.
//! - `seq` is supplied by the caller (the executor packs a chain-monotonic
//!   counter the same way M3 does). The matcher never invents a seq.
//! - Same input book + same `IncomingOrder` ⇒ same `MatchResult`, including
//!   the order of `fills` and `mutations`.
//!
//! ## STP policy
//!
//! `CancelTaker` — if any maker the taker would cross belongs to the same
//! address, the entire taker is rejected with no fills, no rest, no
//! mutations. This matches `crate::book::SelfTradeAction::CancelTaker` (see
//! `crates/cb-engine/src/book.rs:88` — `#[default] CancelTaker`). Spot does
//! not yet expose `CancelMaker`; M5/STP-by-account work would land it here.

use cb_types::{Address, Side, Tif, H256};
use std::collections::BTreeMap;
use thiserror::Error;

/// An order arriving at the matcher. The executor populates `seq` from a
/// chain-monotonic counter before calling `place` — see the doc comment on
/// `crate::book::IncomingOrder::seq`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncomingOrder {
    pub order_id: H256,
    pub owner: Address,
    pub side: Side,
    /// Limit price. Tick-size validation is the executor's job.
    pub price: u128,
    /// Order quantity in base-asset units. Lot-size validation is the
    /// executor's job.
    pub qty: u128,
    pub tif: Tif,
    /// If set, the order must rest as maker. If it would cross at all, the
    /// whole order is rejected (`RejectReason::PostOnlyCrossed`); no partial
    /// fill, no downgrade.
    pub post_only: bool,
    pub client_order_id: H256,
    /// Per-tx, chain-monotonic sequence assigned by the executor. Used as
    /// the time-priority tiebreak.
    pub seq: u64,
}

/// A maker order resting in the book. Constructed either by the matcher (when
/// a GTC remainder rests) or seeded by the test helper.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestingOrder {
    pub order_id: H256,
    pub owner: Address,
    pub side: Side,
    pub price: u128,
    /// Original placed quantity. Useful for client display; matcher consults
    /// `qty_remaining` exclusively.
    pub qty: u128,
    pub qty_remaining: u128,
    pub tif: Tif,
    pub post_only: bool,
    pub client_order_id: H256,
    pub seq: u64,
}

/// One realised fill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fill {
    pub maker_order_id: H256,
    pub taker_order_id: H256,
    pub maker: Address,
    pub taker: Address,
    /// Trade price = maker's resting price (price-time priority).
    pub price: u128,
    pub qty: u128,
}

/// CF mutation the executor applies in addition to mutating its own
/// in-memory book. Emitted in commit order. Mirrors the `BookMutation`
/// shape of `crate::book` but trimmed (no per-mutation `MarketId` field —
/// a `SpotBook` is per-market, the executor tags by the wrapping context).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BookMutation {
    /// Insert a freshly-resting order (used when a GTC remainder rests).
    PutOrder(RestingOrder),
    /// Reduce a partially-filled maker's `qty_remaining` in place. Order id
    /// and (price, seq) book-index key are unchanged.
    UpdateRemaining { order_id: H256, qty_remaining: u128 },
    /// Drop a fully-filled order from both the order CF and the book index.
    ///
    /// Carries the index key. The executor used to re-read the order row to
    /// learn `(side, price, seq)` before it could delete the index entry — a
    /// RocksDB point read per fill, for a maker the matcher had just held in
    /// its hands. A profile of the in-process order benchmark put a fifth of
    /// the executor's time in those re-reads. The matcher emits what it knows.
    DeleteOrder {
        order_id: H256,
        side: Side,
        price: u128,
        seq: u64,
    },
}

/// Why the matcher rejected the whole order without filling. Mirrors the
/// rejection variants of `crate::book::MatchOutcome` (`RejectedPostOnlyCross`,
/// `KilledFOK`, `RejectedSelfTrade`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    /// `post_only` order would have crossed any resting liquidity.
    PostOnlyCrossed,
    /// FOK order could not fully fill against the current book.
    FokUnfilled,
    /// STP fired (`CancelTaker`): the taker would have crossed a maker it
    /// owns.
    SelfTrade,
}

/// Outcome of a `place` call. The executor reads `mutations` to mirror state
/// to RocksDB and reads `fills` to settle balances + apply taker fees.
///
/// Invariants:
///   - `rejected.is_some()` ⇒ `fills.is_empty()` ∧ `mutations.is_empty()`
///     ∧ `rest.is_none()`. STP/post-only/FOK kills leave the book untouched.
///   - `rest.is_some()` ⇒ the resting order is also present as a
///     `BookMutation::PutOrder` in `mutations`. (S3 needs both: the typed
///     handle for follow-on logic, and the mutation list for replay.)
///   - For `Tif::Ioc`, `rest` is always `None`; an unfilled remainder is
///     simply dropped.
///   - For `Tif::Fok`, either `fills` covers the full incoming `qty` and
///     `rest`/`rejected` are `None`, or `rejected = Some(FokUnfilled)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchResult {
    pub fills: Vec<Fill>,
    pub rest: Option<RestingOrder>,
    pub mutations: Vec<BookMutation>,
    pub rejected: Option<RejectReason>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BookError {
    #[error("incoming order qty must be > 0")]
    ZeroQty,
    #[error("incoming order price must be > 0")]
    ZeroPrice,
}

/// In-memory spot CLOB for one market.
///
/// The book is two `BTreeMap<(price, seq), H256>` indexes (one per side) plus
/// a `BTreeMap<H256, RestingOrder>` keyed by id. Both indexes provide
/// deterministic, order-stable iteration; the price index iterates ascending
/// for asks (best ask = lowest price) and is iterated in reverse for bids
/// (best bid = highest price). Within a price level the lower `seq` always
/// comes first.
#[derive(Default, Clone, Debug)]
pub struct SpotBook {
    orders: BTreeMap<H256, RestingOrder>,
    /// Asks sorted by ascending (price, seq). Walk forward for best-ask-first.
    asks: BTreeMap<(u128, u64), H256>,
    /// Bids sorted by ascending (price, seq). Walk backward for
    /// best-bid-first; within a price tier the forward `seq` order is
    /// preserved by reversing the price-major key — see `walk_makers`.
    bids: BTreeMap<(u128, u64), H256>,
}

impl SpotBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// Place an incoming order. Mutates `self` and returns the `MatchResult`
    /// the executor needs to mirror state to RocksDB and settle balances.
    ///
    /// Phases match `crate::book::match_order`:
    ///   1. Plan a fill walk against the opposite side in price-time order.
    ///   2. Reject the whole order (no mutations) if STP, post-only, or FOK
    ///      tripwires fire.
    ///   3. Commit fills + maker mutations.
    ///   4. For GTC, rest the unfilled remainder.
    pub fn place(&mut self, incoming: IncomingOrder) -> MatchResult {
        match self.try_place(incoming) {
            Ok(res) => res,
            Err(BookError::ZeroQty) | Err(BookError::ZeroPrice) => {
                // Mirror `crate::book::match_order`'s validation surface, but
                // expose it through the public API as a no-op rejection
                // distinct from STP/post-only/FOK. Callers who care can
                // reproduce the validation themselves with `validate`.
                //
                // We never want to panic on bad input from the executor —
                // admission is the executor's job, not the matcher's. If a
                // zero qty/price slips through, return an empty result with
                // no mutations rather than poison the book.
                MatchResult {
                    fills: Vec::new(),
                    rest: None,
                    mutations: Vec::new(),
                    rejected: None,
                }
            }
        }
    }

    /// Validate-only entry. Useful for callers that want a typed error rather
    /// than a silent no-op when admission slipped.
    pub fn validate(incoming: &IncomingOrder) -> Result<(), BookError> {
        if incoming.qty == 0 {
            return Err(BookError::ZeroQty);
        }
        if incoming.price == 0 {
            return Err(BookError::ZeroPrice);
        }
        Ok(())
    }

    fn try_place(&mut self, incoming: IncomingOrder) -> Result<MatchResult, BookError> {
        Self::validate(&incoming)?;

        // ----- Plan phase -----
        //
        // Walk the opposite side in price-time priority. Pull each maker's
        // current `qty_remaining` so partial fills earlier in the loop are
        // visible to later iterations without re-reading state.
        let mut plan: Vec<(H256, u128 /* fill qty */)> = Vec::new();
        let mut taker_remaining = incoming.qty;
        let mut self_trade_seen = false;

        for (maker_oid, maker_remaining) in
            self.walk_makers(incoming.side, incoming.price, incoming.qty)
        {
            if taker_remaining == 0 {
                break;
            }
            let maker = match self.orders.get(&maker_oid) {
                Some(o) => o,
                // Stale index entry — should be impossible internally, but
                // tolerate it the same way `crate::book::match_order` does.
                None => continue,
            };

            // Cross condition. Limit semantics:
            //   buy crosses if  taker_price >= maker_price
            //   sell crosses if taker_price <= maker_price
            let crosses = match incoming.side {
                Side::Buy => incoming.price >= maker.price,
                Side::Sell => incoming.price <= maker.price,
            };
            if !crosses {
                break;
            }

            // STP: CancelTaker policy. Cite: see `crates/cb-engine/src/book.rs:88`
            // (`#[default] CancelTaker`). If we're about to cross our own
            // maker, the whole taker is rejected — no fills, no rest, no
            // mutations. We stop the walk immediately.
            if maker.owner == incoming.owner {
                self_trade_seen = true;
                break;
            }

            let fill_qty = std::cmp::min(taker_remaining, maker_remaining);
            if fill_qty == 0 {
                continue;
            }
            plan.push((maker_oid, fill_qty));
            taker_remaining -= fill_qty;
        }

        // ----- Reject gates -----
        //
        // Order matters: STP first (so the trader gets a clearer error than a
        // generic post-only-cross), then post-only, then FOK. Each gate
        // returns with no mutations.

        if self_trade_seen {
            return Ok(MatchResult {
                fills: Vec::new(),
                rest: None,
                mutations: Vec::new(),
                rejected: Some(RejectReason::SelfTrade),
            });
        }

        let filled_qty = incoming.qty - taker_remaining;

        if incoming.post_only && filled_qty > 0 {
            return Ok(MatchResult {
                fills: Vec::new(),
                rest: None,
                mutations: Vec::new(),
                rejected: Some(RejectReason::PostOnlyCrossed),
            });
        }

        if incoming.tif == Tif::Fok && filled_qty < incoming.qty {
            return Ok(MatchResult {
                fills: Vec::new(),
                rest: None,
                mutations: Vec::new(),
                rejected: Some(RejectReason::FokUnfilled),
            });
        }

        // ----- Commit phase -----
        let mut fills: Vec<Fill> = Vec::with_capacity(plan.len());
        let mut mutations: Vec<BookMutation> = Vec::new();

        for (maker_oid, fill_qty) in plan {
            // Re-borrow the maker mutably; the plan walk above only held
            // immutable references.
            let (maker_owner, maker_price, maker_side, maker_seq, maker_remaining_after) = {
                let maker = self
                    .orders
                    .get_mut(&maker_oid)
                    .expect("plan referenced a maker that vanished mid-place");
                let after = maker.qty_remaining - fill_qty;
                maker.qty_remaining = after;
                (maker.owner, maker.price, maker.side, maker.seq, after)
            };

            fills.push(Fill {
                maker_order_id: maker_oid,
                taker_order_id: incoming.order_id,
                maker: maker_owner,
                taker: incoming.owner,
                price: maker_price,
                qty: fill_qty,
            });

            if maker_remaining_after == 0 {
                // Maker fully consumed: drop from order map and price index.
                self.orders.remove(&maker_oid);
                self.index_for(maker_side).remove(&(maker_price, maker_seq));
                mutations.push(BookMutation::DeleteOrder {
                    order_id: maker_oid,
                    side: maker_side,
                    price: maker_price,
                    seq: maker_seq,
                });
            } else {
                mutations.push(BookMutation::UpdateRemaining {
                    order_id: maker_oid,
                    qty_remaining: maker_remaining_after,
                });
            }
        }

        // ----- Resting handling -----
        let remaining_qty = incoming.qty - filled_qty;
        let mut rest: Option<RestingOrder> = None;

        if remaining_qty > 0 {
            // FOK was already kicked above with `FokUnfilled`. IOC drops the
            // remainder. Only GTC rests.
            if incoming.tif == Tif::Gtc {
                let resting = RestingOrder {
                    order_id: incoming.order_id,
                    owner: incoming.owner,
                    side: incoming.side,
                    price: incoming.price,
                    qty: incoming.qty,
                    qty_remaining: remaining_qty,
                    tif: incoming.tif,
                    post_only: incoming.post_only,
                    client_order_id: incoming.client_order_id,
                    seq: incoming.seq,
                };
                self.orders.insert(resting.order_id, resting.clone());
                self.index_for(incoming.side)
                    .insert((incoming.price, incoming.seq), incoming.order_id);
                mutations.push(BookMutation::PutOrder(resting.clone()));
                rest = Some(resting);
            }
            // Tif::Ioc: silent drop. No mutation, no rest.
            // Tif::Fok: unreachable (handled above).
        }

        Ok(MatchResult {
            fills,
            rest,
            mutations,
            rejected: None,
        })
    }

    /// Every order `owner` has resting, in ascending order-id order — the
    /// same order a scan of the persisted rows sorted by id would give, which
    /// is what lets a batch cancel walk the book instead of the column
    /// family and unlock in the same sequence either way.
    pub fn resting_orders_of(&self, owner: &Address) -> Vec<RestingOrder> {
        self.orders
            .values()
            .filter(|o| o.owner == *owner)
            .cloned()
            .collect()
    }

    /// Cancel a resting order. Returns the order if it existed. The caller
    /// (executor) is responsible for any authority check and for unlocking
    /// the matching collateral.
    pub fn cancel(&mut self, order_id: H256) -> Option<RestingOrder> {
        let order = self.orders.remove(&order_id)?;
        self.index_for(order.side).remove(&(order.price, order.seq));
        Some(order)
    }

    /// Number of resting orders.
    pub fn len(&self) -> usize {
        self.orders.len()
    }

    /// Whether the book holds no resting orders.
    pub fn is_empty(&self) -> bool {
        self.orders.is_empty()
    }

    /// Seed a previously-persisted resting order back into the book.
    ///
    /// Non-test callers (the S3 executor's per-tx hydration step) reconstruct
    /// `RestingOrder` from a `SpotOrder` row that was committed by an earlier
    /// block. The matcher does not validate the seed — the caller MUST have
    /// previously persisted this order through `place` and the resulting
    /// `BookMutation::PutOrder`. Skipping that contract is a determinism
    /// hazard (the resting order's `(price, seq)` placement is what gives
    /// price-time priority).
    pub fn insert_resting(&mut self, o: RestingOrder) {
        self.index_for(o.side).insert((o.price, o.seq), o.order_id);
        self.orders.insert(o.order_id, o);
    }

    // -- internals --

    /// Yield `(order_id, qty_remaining)` of the opposite-side makers this taker
    /// can actually reach, in price-time priority. Materialised into a `Vec` so
    /// the matcher can mutate `self` during the commit phase without borrowing
    /// the index.
    ///
    /// # Why it is bounded
    ///
    /// This used to materialise the *entire* opposite side on every `place`,
    /// which made a one-level order cost as much as sweeping the book. A
    /// sampling profile of `cb-order-bench in-process` at a depth of 2000
    /// resting orders per side attributed ~80% of total runtime to building and
    /// walking that vector — for a workload whose orders each consume a single
    /// maker.
    ///
    /// The walk now stops at the first price level that does not cross
    /// `taker_price`, and once the resting quantity already gathered covers
    /// `taker_qty`.
    ///
    /// # Why the result is unchanged
    ///
    /// The output is a **prefix** of the unbounded output, and `try_place`
    /// stops consuming at exactly the same two conditions: it `break`s when a
    /// maker no longer crosses, and when `taker_remaining` reaches zero. A
    /// maker whose `qty_remaining` is zero contributes zero to the accumulator
    /// and is `continue`d by the caller, and a self-trade `break`s the caller's
    /// loop earlier still. So the same makers are filled, in the same order,
    /// for the same quantities, and `MatchResult` is identical — which is what
    /// the state root depends on.
    ///
    /// Whole price levels are always taken. Time priority inside a level runs
    /// on ascending `seq`, and the bid walk sees a level's seqs in descending
    /// order, so truncating mid-level would drop the orders with the *best*
    /// time priority.
    fn walk_makers(
        &self,
        taker_side: Side,
        taker_price: u128,
        taker_qty: u128,
    ) -> Vec<(H256, u128)> {
        let opposite = match taker_side {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        };
        let index = match opposite {
            Side::Sell => &self.asks,
            Side::Buy => &self.bids,
        };
        // Same limit semantics as the caller's cross test: a buy crosses a
        // maker priced at or below it, a sell crosses one priced at or above.
        let crosses = |maker_price: u128| match taker_side {
            Side::Buy => taker_price >= maker_price,
            Side::Sell => taker_price <= maker_price,
        };
        // Asks: ascending (price, seq) — best ask first, already exactly the
        // consumption order, so the walk can stop on any entry.
        // Bids: reverse the BTreeMap so we get descending price; within a
        // price level we still need ascending seq, so we group manually and
        // can only stop on a level boundary.
        match opposite {
            Side::Sell => {
                let mut out: Vec<(H256, u128)> = Vec::new();
                let mut gathered: u128 = 0;
                for ((price, _seq), oid) in index.iter() {
                    if !crosses(*price) {
                        break;
                    }
                    if let Some(o) = self.orders.get(oid) {
                        out.push((*oid, o.qty_remaining));
                        gathered = gathered.saturating_add(o.qty_remaining);
                        if gathered >= taker_qty {
                            break;
                        }
                    }
                }
                out
            }
            Side::Buy => {
                // Group keys by price, then walk price-desc, seq-asc.
                let mut by_price: BTreeMap<u128, Vec<u64>> = BTreeMap::new();
                let mut gathered: u128 = 0;
                let mut level: Option<u128> = None;
                for (price, seq) in index.keys().rev() {
                    if level != Some(*price) {
                        // A new, strictly worse price. Everything at the
                        // previous level is complete, so stopping here cannot
                        // cut a level in half.
                        if gathered >= taker_qty || !crosses(*price) {
                            break;
                        }
                        level = Some(*price);
                    }
                    by_price.entry(*price).or_default().push(*seq);
                    if let Some(oid) = index.get(&(*price, *seq)) {
                        if let Some(o) = self.orders.get(oid) {
                            gathered = gathered.saturating_add(o.qty_remaining);
                        }
                    }
                }
                let mut out: Vec<(H256, u128)> = Vec::new();
                for (price, seqs) in by_price.iter().rev() {
                    // The gather loop above pushed seqs in descending order
                    // (it walks the index in reverse), so the sort is what
                    // restores ascending time priority inside the level.
                    let mut seqs = seqs.clone();
                    seqs.sort_unstable();
                    for seq in seqs {
                        if let Some(oid) = index.get(&(*price, seq)) {
                            if let Some(o) = self.orders.get(oid) {
                                out.push((*oid, o.qty_remaining));
                            }
                        }
                    }
                }
                out
            }
        }
    }

    fn index_for(&mut self, side: Side) -> &mut BTreeMap<(u128, u64), H256> {
        match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(byte: u8) -> Address {
        Address([byte; 20])
    }

    fn oid(byte: u8) -> H256 {
        H256([byte; 32])
    }

    fn mk_incoming(
        oid_byte: u8,
        owner_byte: u8,
        side: Side,
        price: u128,
        qty: u128,
        tif: Tif,
        seq: u64,
    ) -> IncomingOrder {
        IncomingOrder {
            order_id: oid(oid_byte),
            owner: addr(owner_byte),
            side,
            price,
            qty,
            tif,
            post_only: false,
            client_order_id: H256([0; 32]),
            seq,
        }
    }

    /// The unbounded walk the matcher used before `walk_makers` learned to
    /// stop: every opposite-side maker, in price-time priority. Kept here as
    /// the reference the bounded walk is checked against.
    fn walk_makers_unbounded(book: &SpotBook, taker_side: Side) -> Vec<(H256, u128)> {
        let opposite = match taker_side {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        };
        let index = match opposite {
            Side::Sell => &book.asks,
            Side::Buy => &book.bids,
        };
        match opposite {
            Side::Sell => index
                .values()
                .filter_map(|o| book.orders.get(o).map(|r| (*o, r.qty_remaining)))
                .collect(),
            Side::Buy => {
                let mut by_price: BTreeMap<u128, Vec<u64>> = BTreeMap::new();
                for (price, seq) in index.keys() {
                    by_price.entry(*price).or_default().push(*seq);
                }
                let mut out = Vec::new();
                for (price, seqs) in by_price.iter().rev() {
                    let mut seqs = seqs.clone();
                    seqs.sort_unstable();
                    for seq in seqs {
                        if let Some(o) = index.get(&(*price, seq)) {
                            if let Some(r) = book.orders.get(o) {
                                out.push((*o, r.qty_remaining));
                            }
                        }
                    }
                }
                out
            }
        }
    }

    /// The bounded `walk_makers` must return a prefix of the unbounded walk,
    /// and that prefix must be long enough that the matcher's own two stopping
    /// conditions still fire inside it. Together those two properties are the
    /// whole correctness argument for bounding the walk: same makers, same
    /// order, same quantities, therefore same `MatchResult` and same state
    /// root.
    ///
    /// Swept across a multi-level book on both sides, over taker prices that
    /// reach none, some and all of the levels, and taker quantities from one
    /// lot to more than the whole book.
    #[test]
    fn bounded_walk_is_a_sufficient_prefix_of_the_full_walk() {
        for taker_side in [Side::Buy, Side::Sell] {
            let mut book = SpotBook::new();
            // 6 levels a side, 4 orders a level, staggered seqs.
            let mut n: u8 = 0;
            for level in 0..6u128 {
                for k in 0..4u64 {
                    n += 1;
                    book.insert_resting(mk_resting(
                        n,
                        0xB0,
                        Side::Sell,
                        1_100 + level * 10,
                        10 + k as u128,
                        level as u64 * 10 + k,
                    ));
                    n += 1;
                    book.insert_resting(mk_resting(
                        n,
                        0xB1,
                        Side::Buy,
                        1_000 - level * 10,
                        10 + k as u128,
                        500 + level as u64 * 10 + k,
                    ));
                }
            }

            let full = walk_makers_unbounded(&book, taker_side);
            for price in [900u128, 1_000, 1_020, 1_050, 1_100, 1_150, 1_200] {
                for qty in [1u128, 15, 55, 200, 10_000] {
                    let bounded = book.walk_makers(taker_side, price, qty);
                    assert!(
                        bounded.len() <= full.len(),
                        "bounded walk longer than the full walk"
                    );
                    assert_eq!(
                        bounded[..],
                        full[..bounded.len()],
                        "bounded walk is not a prefix (side {taker_side:?}, price {price}, qty {qty})"
                    );

                    // Sufficiency: replay the matcher's consumption over the
                    // FULL walk and check it never needs an entry the bounded
                    // walk dropped.
                    let mut remaining = qty;
                    let mut consumed = 0usize;
                    for (oid, maker_remaining) in &full {
                        if remaining == 0 {
                            break;
                        }
                        let maker = book.orders.get(oid).expect("maker present");
                        let crosses = match taker_side {
                            Side::Buy => price >= maker.price,
                            Side::Sell => price <= maker.price,
                        };
                        if !crosses {
                            break;
                        }
                        consumed += 1;
                        remaining -= std::cmp::min(remaining, *maker_remaining);
                    }
                    assert!(
                        consumed <= bounded.len(),
                        "bounded walk cut {} entries short (side {taker_side:?}, price {price}, qty {qty})",
                        consumed - bounded.len()
                    );
                }
            }
        }
    }

    fn mk_resting(
        oid_byte: u8,
        owner_byte: u8,
        side: Side,
        price: u128,
        qty: u128,
        seq: u64,
    ) -> RestingOrder {
        RestingOrder {
            order_id: oid(oid_byte),
            owner: addr(owner_byte),
            side,
            price,
            qty,
            qty_remaining: qty,
            tif: Tif::Gtc,
            post_only: false,
            client_order_id: H256([0; 32]),
            seq,
        }
    }

    // ---- 1. Empty-book GTC rests on either side ----

    #[test]
    fn empty_book_buy_gtc_rests() {
        let mut book = SpotBook::new();
        let inc = mk_incoming(0xa1, 0xaa, Side::Buy, 100, 5, Tif::Gtc, 1);
        let res = book.place(inc.clone());
        assert!(res.fills.is_empty());
        assert_eq!(res.rejected, None);
        let rest = res.rest.expect("GTC must rest");
        assert_eq!(rest.order_id, inc.order_id);
        assert_eq!(rest.qty_remaining, 5);
        assert_eq!(res.mutations.len(), 1);
        match &res.mutations[0] {
            BookMutation::PutOrder(o) => assert_eq!(o.order_id, inc.order_id),
            other => panic!("expected PutOrder, got {:?}", other),
        }
        assert_eq!(book.len(), 1);
    }

    #[test]
    fn empty_book_sell_gtc_rests() {
        let mut book = SpotBook::new();
        let inc = mk_incoming(0xb1, 0xbb, Side::Sell, 100, 7, Tif::Gtc, 1);
        let res = book.place(inc.clone());
        assert!(res.fills.is_empty());
        assert_eq!(res.rejected, None);
        assert!(res.rest.is_some());
        assert_eq!(book.len(), 1);
    }

    // ---- 2. Simple cross: bid 100 rests; ask 100 hits ----

    #[test]
    fn bid_then_crossing_ask_clears_book() {
        let mut book = SpotBook::new();
        let bid = mk_incoming(0xb1, 0xaa, Side::Buy, 100, 5, Tif::Gtc, 1);
        book.place(bid.clone());
        let ask = mk_incoming(0xa1, 0xbb, Side::Sell, 100, 5, Tif::Gtc, 2);
        let res = book.place(ask.clone());
        assert_eq!(res.rejected, None);
        assert_eq!(res.fills.len(), 1);
        assert_eq!(res.fills[0].price, 100);
        assert_eq!(res.fills[0].qty, 5);
        assert_eq!(res.fills[0].maker_order_id, bid.order_id);
        assert_eq!(res.fills[0].taker_order_id, ask.order_id);
        assert!(res.rest.is_none(), "fully filled, no rest");
        // Maker fully consumed → DeleteOrder mutation.
        assert!(res.mutations.iter().any(|m| matches!(
            m,
            BookMutation::DeleteOrder { order_id, .. } if *order_id == bid.order_id
        )));
        assert_eq!(book.len(), 0);
    }

    // ---- 3. Partial fill: bid size 5 vs ask size 3 ----

    #[test]
    fn partial_fill_leaves_maker_with_remaining_qty() {
        let mut book = SpotBook::new();
        let bid = mk_incoming(0xb1, 0xaa, Side::Buy, 100, 5, Tif::Gtc, 1);
        book.place(bid.clone());
        let ask = mk_incoming(0xa1, 0xbb, Side::Sell, 100, 3, Tif::Gtc, 2);
        let res = book.place(ask);
        assert_eq!(res.rejected, None);
        assert_eq!(res.fills.len(), 1);
        assert_eq!(res.fills[0].qty, 3);
        // Maker is partially filled; mutation is UpdateRemaining, not Delete.
        let upd = res.mutations.iter().find_map(|m| match m {
            BookMutation::UpdateRemaining {
                order_id,
                qty_remaining,
            } if *order_id == bid.order_id => Some(*qty_remaining),
            _ => None,
        });
        assert_eq!(upd, Some(2));
        assert!(!res
            .mutations
            .iter()
            .any(|m| matches!(m, BookMutation::DeleteOrder { .. })));
        // Book state mirrors the mutation.
        assert_eq!(book.len(), 1);
        let still = book.cancel(bid.order_id).expect("bid still resting");
        assert_eq!(still.qty_remaining, 2);
    }

    // ---- 4. Walk through bids: ask 99 size 5 sweeps 101, 100, 99 ----

    #[test]
    fn sell_walks_highest_bid_first_ioc_drops_remainder() {
        let mut book = SpotBook::new();
        // Three bids at 99, 100, 101, each size 1.
        book.place(mk_incoming(0xb1, 0xaa, Side::Buy, 99, 1, Tif::Gtc, 1));
        book.place(mk_incoming(0xb2, 0xaa, Side::Buy, 100, 1, Tif::Gtc, 2));
        book.place(mk_incoming(0xb3, 0xaa, Side::Buy, 101, 1, Tif::Gtc, 3));
        // Different owner sells size 5 limit 99 IOC.
        let ask = mk_incoming(0xa1, 0xbb, Side::Sell, 99, 5, Tif::Ioc, 4);
        let res = book.place(ask);
        assert_eq!(res.rejected, None);
        assert_eq!(res.fills.len(), 3);
        // Fills walk best-bid-first: 101, 100, 99.
        assert_eq!(res.fills[0].price, 101);
        assert_eq!(res.fills[1].price, 100);
        assert_eq!(res.fills[2].price, 99);
        // IOC remainder (qty 2) drops; nothing rests.
        assert!(res.rest.is_none());
        assert_eq!(book.len(), 0);
    }

    #[test]
    fn sell_walks_highest_bid_first_gtc_rests_remainder() {
        let mut book = SpotBook::new();
        book.place(mk_incoming(0xb1, 0xaa, Side::Buy, 99, 1, Tif::Gtc, 1));
        book.place(mk_incoming(0xb2, 0xaa, Side::Buy, 100, 1, Tif::Gtc, 2));
        book.place(mk_incoming(0xb3, 0xaa, Side::Buy, 101, 1, Tif::Gtc, 3));
        let ask = mk_incoming(0xa1, 0xbb, Side::Sell, 99, 5, Tif::Gtc, 4);
        let res = book.place(ask.clone());
        assert_eq!(res.fills.len(), 3);
        let rest = res.rest.expect("GTC remainder rests");
        assert_eq!(rest.order_id, ask.order_id);
        assert_eq!(rest.qty_remaining, 2);
        assert_eq!(rest.price, 99);
        // Remaining ask is in the book at price 99.
        assert_eq!(book.len(), 1);
    }

    // ---- 5. Post-only that would cross is rejected, no mutations ----

    #[test]
    fn post_only_rejected_when_crossing() {
        let mut book = SpotBook::new();
        // A maker ask at 100.
        book.place(mk_incoming(0xa1, 0xbb, Side::Sell, 100, 5, Tif::Gtc, 1));
        // Buyer post-only at 100 would cross.
        let mut po = mk_incoming(0xc1, 0xaa, Side::Buy, 100, 1, Tif::Gtc, 2);
        po.post_only = true;
        let res = book.place(po);
        assert_eq!(res.rejected, Some(RejectReason::PostOnlyCrossed));
        assert!(res.fills.is_empty());
        assert!(res.mutations.is_empty());
        assert!(res.rest.is_none());
        // The original maker is untouched.
        assert_eq!(book.len(), 1);
    }

    // ---- 6. FOK that can't fully fill is rejected, no mutations ----

    #[test]
    fn fok_unfilled_when_book_too_thin() {
        let mut book = SpotBook::new();
        book.place(mk_incoming(0xa1, 0xbb, Side::Sell, 100, 3, Tif::Gtc, 1));
        let fok = mk_incoming(0xc1, 0xaa, Side::Buy, 100, 5, Tif::Fok, 2);
        let res = book.place(fok);
        assert_eq!(res.rejected, Some(RejectReason::FokUnfilled));
        assert!(res.fills.is_empty());
        assert!(res.mutations.is_empty());
        assert!(res.rest.is_none());
        // Maker untouched.
        assert_eq!(book.len(), 1);
    }

    #[test]
    fn fok_succeeds_when_book_has_enough() {
        let mut book = SpotBook::new();
        book.place(mk_incoming(0xa1, 0xbb, Side::Sell, 100, 10, Tif::Gtc, 1));
        let fok = mk_incoming(0xc1, 0xaa, Side::Buy, 100, 5, Tif::Fok, 2);
        let res = book.place(fok);
        assert_eq!(res.rejected, None);
        assert_eq!(res.fills.len(), 1);
        assert_eq!(res.fills[0].qty, 5);
        assert!(res.rest.is_none());
    }

    // ---- 7. STP CancelTaker: same-owner cross rejects taker ----

    #[test]
    fn self_trade_cancels_taker() {
        let mut book = SpotBook::new();
        // Alice places a sell.
        book.place(mk_incoming(0xa1, 0xaa, Side::Sell, 100, 5, Tif::Gtc, 1));
        // Alice places a buy that would cross.
        let taker = mk_incoming(0xc1, 0xaa, Side::Buy, 100, 5, Tif::Ioc, 2);
        let res = book.place(taker);
        assert_eq!(res.rejected, Some(RejectReason::SelfTrade));
        assert!(res.fills.is_empty());
        assert!(res.mutations.is_empty());
        assert!(res.rest.is_none());
        // Maker still in the book.
        assert_eq!(book.len(), 1);
    }

    // ---- 8. Price-time priority: equal price, lower seq fills first ----

    #[test]
    fn equal_price_lower_seq_fills_first() {
        let mut book = SpotBook::new();
        // Two bids at the same price; seq 1 then seq 2.
        book.place(mk_incoming(0xb1, 0xaa, Side::Buy, 100, 1, Tif::Gtc, 1));
        book.place(mk_incoming(0xb2, 0xbb, Side::Buy, 100, 1, Tif::Gtc, 2));
        // Different-owner sell at 100, size 1: must take seq 1.
        let ask = mk_incoming(0xc1, 0xcc, Side::Sell, 100, 1, Tif::Ioc, 3);
        let res = book.place(ask);
        assert_eq!(res.fills.len(), 1);
        assert_eq!(res.fills[0].maker_order_id, oid(0xb1));
        assert_eq!(res.fills[0].maker, addr(0xaa));
        // seq 2 still resting.
        assert_eq!(book.len(), 1);
    }

    // ---- Extra coverage to keep parity with `book.rs` invariants ----

    #[test]
    fn buy_walks_lowest_ask_first() {
        let mut book = SpotBook::new();
        book.place(mk_incoming(0xa1, 0xaa, Side::Sell, 200, 1, Tif::Gtc, 1));
        book.place(mk_incoming(0xa2, 0xbb, Side::Sell, 100, 1, Tif::Gtc, 2));
        let buy = mk_incoming(0xc1, 0xcc, Side::Buy, 250, 1, Tif::Ioc, 3);
        let res = book.place(buy);
        assert_eq!(res.fills.len(), 1);
        assert_eq!(res.fills[0].price, 100);
        assert_eq!(res.fills[0].maker, addr(0xbb));
    }

    #[test]
    fn does_not_cross_through_limit_price() {
        let mut book = SpotBook::new();
        book.place(mk_incoming(0xa1, 0xbb, Side::Sell, 110, 5, Tif::Gtc, 1));
        let buy = mk_incoming(0xc1, 0xaa, Side::Buy, 100, 5, Tif::Ioc, 2);
        let res = book.place(buy);
        assert!(res.fills.is_empty());
        assert!(res.rest.is_none()); // IOC drops
        assert_eq!(res.rejected, None);
        assert_eq!(book.len(), 1); // maker untouched
    }

    #[test]
    fn cancel_removes_resting_order() {
        let mut book = SpotBook::new();
        let inc = mk_incoming(0xa1, 0xbb, Side::Sell, 100, 5, Tif::Gtc, 1);
        book.place(inc.clone());
        assert_eq!(book.len(), 1);
        let cancelled = book.cancel(inc.order_id).expect("cancel hits");
        assert_eq!(cancelled.qty_remaining, 5);
        assert_eq!(book.len(), 0);
        // Idempotent on second call.
        assert!(book.cancel(inc.order_id).is_none());
    }

    #[test]
    fn insert_resting_seeds_book() {
        let mut book = SpotBook::new();
        book.insert_resting(mk_resting(0xa1, 0xbb, Side::Sell, 100, 5, 1));
        // Crossing buy should fill against it.
        let buy = mk_incoming(0xc1, 0xaa, Side::Buy, 100, 5, Tif::Ioc, 2);
        let res = book.place(buy);
        assert_eq!(res.fills.len(), 1);
        assert_eq!(res.fills[0].maker_order_id, oid(0xa1));
        assert_eq!(book.len(), 0);
    }

    #[test]
    fn post_only_rests_if_no_cross() {
        let mut book = SpotBook::new();
        // Maker ask at 200.
        book.place(mk_incoming(0xa1, 0xbb, Side::Sell, 200, 5, Tif::Gtc, 1));
        // Post-only buy at 100 — no cross.
        let mut po = mk_incoming(0xc1, 0xaa, Side::Buy, 100, 5, Tif::Gtc, 2);
        po.post_only = true;
        let res = book.place(po);
        assert_eq!(res.rejected, None);
        assert!(res.fills.is_empty());
        assert!(res.rest.is_some());
        assert_eq!(book.len(), 2);
    }

    #[test]
    fn ioc_no_liquidity_returns_empty_no_rest() {
        let mut book = SpotBook::new();
        let buy = mk_incoming(0xc1, 0xaa, Side::Buy, 100, 5, Tif::Ioc, 1);
        let res = book.place(buy);
        assert_eq!(res.rejected, None);
        assert!(res.fills.is_empty());
        assert!(res.rest.is_none());
        assert!(res.mutations.is_empty());
        assert_eq!(book.len(), 0);
    }

    #[test]
    fn validate_rejects_zero_qty_and_price() {
        let mut zero_qty = mk_incoming(0xc1, 0xaa, Side::Buy, 100, 0, Tif::Gtc, 1);
        assert_eq!(SpotBook::validate(&zero_qty), Err(BookError::ZeroQty));
        zero_qty.qty = 1;
        zero_qty.price = 0;
        assert_eq!(SpotBook::validate(&zero_qty), Err(BookError::ZeroPrice));
    }

    #[test]
    fn fills_walk_multiple_makers_in_priority_order() {
        let mut book = SpotBook::new();
        book.place(mk_incoming(0xa1, 0xb1, Side::Sell, 100, 5, Tif::Gtc, 1));
        book.place(mk_incoming(0xa2, 0xb2, Side::Sell, 110, 5, Tif::Gtc, 2));
        book.place(mk_incoming(0xa3, 0xb3, Side::Sell, 120, 5, Tif::Gtc, 3));
        let buy = mk_incoming(0xc1, 0xaa, Side::Buy, 200, 12, Tif::Ioc, 9);
        let res = book.place(buy);
        assert_eq!(res.fills.len(), 3);
        assert_eq!(res.fills[0].price, 100);
        assert_eq!(res.fills[1].price, 110);
        assert_eq!(res.fills[2].price, 120);
        assert_eq!(res.fills[0].qty + res.fills[1].qty + res.fills[2].qty, 12);
    }
}
