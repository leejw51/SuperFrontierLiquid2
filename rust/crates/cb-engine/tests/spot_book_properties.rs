//! Property tests for the pure spot matcher, `SpotBook`.
//!
//! `properties.rs` covers the perpetual matcher; nothing exercised `SpotBook`
//! beyond the hand-written cases in its own module. These drive it with random
//! sequences of limit, IOC, FOK and post-only orders and cancels from several
//! owners, and check every step against a *reference model* kept alongside it.
//!
//! The model is a plain list of resting orders rebuilt from nothing but the
//! matcher's own `BookMutation` stream. That is deliberate: the executor never
//! looks inside a `SpotBook`, it persists the mutations and later rehydrates a
//! book from the rows. So the property that matters for consensus is not "the
//! book is right" but "the mutations say exactly what the book did" — if they
//! ever diverge, a replica that rehydrates from rows matches differently from
//! a leader that kept its book in memory, and the chain forks.
//!
//! Against that model each placement is checked for:
//!
//! * **the whole fill plan**, recomputed independently: best price first,
//!   oldest `seq` first within a price, stopping at the limit, at the taker's
//!   quantity, and at the first maker the taker owns (self-trade prevention
//!   cancels the taker, never fills against itself);
//! * **every fill price** is the maker's resting price and inside the
//!   taker's limit;
//! * **price-time priority** directly: no maker that was better placed than a
//!   filled maker is left with quantity it could have given;
//! * **post-only / FOK / IOC** semantics, and that a rejected order leaves the
//!   book exactly as it was;
//! * **quantity conservation**: filled + rested + dropped = ordered;
//! * **the book is never crossed** after any operation.

use cb_engine::spot_book::{
    BookMutation, Fill, IncomingOrder, RejectReason, RestingOrder, SpotBook,
};
use cb_types::{Address, Side, Tif, H256};
use proptest::prelude::*;
use std::collections::BTreeMap;

const OWNERS: u8 = 5;

/// One step of a random workload.
#[derive(Clone, Debug)]
enum Op {
    Place {
        owner: u8,
        side: Side,
        /// Price in ticks around 100, so orders cross often.
        price: u128,
        qty: u128,
        tif: Tif,
        post_only: bool,
    },
    /// Cancel the resting order at this index (mod the book size) of the
    /// model's id-ordered list; a no-op on an empty book.
    Cancel(usize),
}

fn op_strategy() -> impl Strategy<Value = Op> {
    let side = prop_oneof![Just(Side::Buy), Just(Side::Sell)];
    let tif = prop_oneof![
        4 => Just(Tif::Gtc),
        2 => Just(Tif::Ioc),
        1 => Just(Tif::Fok)
    ];
    let place = (
        0..OWNERS,
        side,
        95u128..=105,
        1u128..=20,
        tif,
        prop::bool::weighted(0.2),
    )
        .prop_map(|(owner, side, price, qty, tif, post_only)| Op::Place {
            owner,
            side,
            price,
            qty,
            tif,
            post_only,
        });
    prop_oneof![5 => place, 1 => any::<usize>().prop_map(Op::Cancel)]
}

fn owner(b: u8) -> Address {
    Address([b + 1; 20])
}

fn oid(n: u64) -> H256 {
    let mut h = [0u8; 32];
    h[24..].copy_from_slice(&n.to_be_bytes());
    H256(h)
}

/// The reference matcher: the fills an incoming order *should* produce against
/// `model`, or the reason it should be rejected. Written from the documented
/// rules, not from the implementation.
fn expected_plan(
    model: &BTreeMap<H256, RestingOrder>,
    inc: &IncomingOrder,
) -> Result<Vec<(H256, u128)>, RejectReason> {
    let mut makers: Vec<&RestingOrder> = model
        .values()
        .filter(|o| o.side != inc.side && o.qty_remaining > 0)
        .collect();
    // Best price first; oldest first within a price.
    makers.sort_by(|a, b| {
        let by_price = match inc.side {
            Side::Buy => a.price.cmp(&b.price),
            Side::Sell => b.price.cmp(&a.price),
        };
        by_price.then(a.seq.cmp(&b.seq))
    });
    let mut left = inc.qty;
    let mut plan = Vec::new();
    for m in makers {
        if left == 0 {
            break;
        }
        let crosses = match inc.side {
            Side::Buy => inc.price >= m.price,
            Side::Sell => inc.price <= m.price,
        };
        if !crosses {
            break;
        }
        if m.owner == inc.owner {
            return Err(RejectReason::SelfTrade);
        }
        let q = left.min(m.qty_remaining);
        plan.push((m.order_id, q));
        left -= q;
    }
    let filled = inc.qty - left;
    if inc.post_only && filled > 0 {
        return Err(RejectReason::PostOnlyCrossed);
    }
    if inc.tif == Tif::Fok && filled < inc.qty {
        return Err(RejectReason::FokUnfilled);
    }
    Ok(plan)
}

/// Replay a mutation stream onto the model, checking each mutation is
/// consistent with what the model already holds.
fn apply_mutations(model: &mut BTreeMap<H256, RestingOrder>, muts: &[BookMutation]) {
    for m in muts {
        match m {
            BookMutation::PutOrder(o) => {
                assert!(
                    model.insert(o.order_id, o.clone()).is_none(),
                    "PutOrder for an id already resting"
                );
            }
            BookMutation::UpdateRemaining {
                order_id,
                qty_remaining,
            } => {
                let o = model.get_mut(order_id).expect("update of an unknown order");
                assert!(*qty_remaining > 0, "a zero remainder must be a delete");
                assert!(*qty_remaining < o.qty_remaining, "an update only shrinks");
                o.qty_remaining = *qty_remaining;
            }
            BookMutation::DeleteOrder {
                order_id,
                side,
                price,
                seq,
            } => {
                let o = model.remove(order_id).expect("delete of an unknown order");
                // The index key the executor deletes by must be the order's.
                assert_eq!((o.side, o.price, o.seq), (*side, *price, *seq));
            }
        }
    }
}

fn best(model: &BTreeMap<H256, RestingOrder>, side: Side) -> Option<u128> {
    let prices = model.values().filter(|o| o.side == side).map(|o| o.price);
    match side {
        Side::Buy => prices.max(),
        Side::Sell => prices.min(),
    }
}

/// The book `SpotBook` holds for every owner equals the model, order by
/// order. `resting_orders_of` is the only read-out the executor uses (batch
/// cancel), so it is the one to hold to account.
fn assert_book_matches_model(book: &SpotBook, model: &BTreeMap<H256, RestingOrder>) {
    assert_eq!(
        book.len(),
        model.len(),
        "book size vs mutation-derived model"
    );
    for b in 0..OWNERS {
        let from_book = book.resting_orders_of(&owner(b));
        let from_model: Vec<RestingOrder> = model
            .values()
            .filter(|o| o.owner == owner(b))
            .cloned()
            .collect();
        assert_eq!(from_book, from_model, "owner {b}'s resting orders");
    }
}

fn check_fills_against_limits(
    inc: &IncomingOrder,
    fills: &[Fill],
    before: &BTreeMap<H256, RestingOrder>,
) {
    for f in fills {
        let maker = before
            .get(&f.maker_order_id)
            .expect("fill against a resting maker");
        assert_eq!(f.price, maker.price, "a fill trades at the maker's price");
        assert_ne!(f.maker, f.taker, "no fill against oneself");
        assert_eq!(f.taker, inc.owner);
        assert!(f.qty > 0 && f.qty <= maker.qty_remaining);
        match inc.side {
            Side::Buy => assert!(f.price <= inc.price, "buy filled above its limit"),
            Side::Sell => assert!(f.price >= inc.price, "sell filled below its limit"),
        }
    }
    // Price-time priority, stated without reference to the plan: once a maker
    // has filled, every maker strictly ahead of it in priority must have given
    // everything it had. A later order at the same price never fills before
    // an earlier one.
    let filled: BTreeMap<H256, u128> = fills.iter().map(|f| (f.maker_order_id, f.qty)).collect();
    for f in fills {
        let m = &before[&f.maker_order_id];
        for other in before
            .values()
            .filter(|o| o.side == m.side && o.order_id != m.order_id)
        {
            let ahead = match m.side {
                Side::Sell => {
                    other.price < m.price || (other.price == m.price && other.seq < m.seq)
                }
                Side::Buy => other.price > m.price || (other.price == m.price && other.seq < m.seq),
            };
            if ahead {
                assert_eq!(
                    filled.get(&other.order_id).copied(),
                    Some(other.qty_remaining),
                    "maker {:?} was ahead of a filled maker yet was not exhausted",
                    other.order_id
                );
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Every invariant listed in the module header, over a random workload of
    /// up to 80 operations.
    #[test]
    fn the_spot_matcher_obeys_its_contract_at_every_step(ops in prop::collection::vec(op_strategy(), 1..80)) {
        let mut book = SpotBook::new();
        let mut model: BTreeMap<H256, RestingOrder> = BTreeMap::new();
        let mut seq = 0u64;

        for op in ops {
            match op {
                Op::Place { owner: o, side, price, qty, tif, post_only } => {
                    seq += 1;
                    let inc = IncomingOrder {
                        order_id: oid(seq),
                        owner: owner(o),
                        side,
                        price,
                        qty,
                        tif,
                        post_only,
                        client_order_id: H256::default(),
                        seq,
                    };
                    let before = model.clone();
                    let expected = expected_plan(&model, &inc);
                    let res = book.place(inc.clone());

                    match expected {
                        Err(reason) => {
                            prop_assert_eq!(res.rejected, Some(reason));
                            prop_assert!(res.fills.is_empty() && res.mutations.is_empty() && res.rest.is_none(),
                                "a rejection must change nothing");
                        }
                        Ok(plan) => {
                            prop_assert_eq!(res.rejected, None);
                            let got: Vec<(H256, u128)> = res.fills.iter().map(|f| (f.maker_order_id, f.qty)).collect();
                            prop_assert_eq!(got, plan, "fill plan differs from the reference matcher");
                            check_fills_against_limits(&inc, &res.fills, &before);

                            let filled: u128 = res.fills.iter().map(|f| f.qty).sum();
                            let rested = res.rest.as_ref().map(|r| r.qty_remaining).unwrap_or(0);
                            match tif {
                                Tif::Gtc => prop_assert_eq!(filled + rested, qty, "GTC: filled + rested = ordered"),
                                Tif::Ioc => prop_assert!(res.rest.is_none(), "IOC never rests"),
                                Tif::Fok => {
                                    prop_assert_eq!(filled, qty, "an accepted FOK filled in full");
                                    prop_assert!(res.rest.is_none());
                                }
                            }
                            if let Some(r) = &res.rest {
                                prop_assert_eq!(r.order_id, inc.order_id);
                                prop_assert_eq!(r.price, price);
                                prop_assert!(res.mutations.contains(&BookMutation::PutOrder(r.clone())),
                                    "a resting remainder is announced as a PutOrder");
                            }
                        }
                    }
                    apply_mutations(&mut model, &res.mutations);
                }
                Op::Cancel(i) => {
                    if model.is_empty() {
                        continue;
                    }
                    let id = *model.keys().nth(i % model.len()).unwrap();
                    let removed = book.cancel(id);
                    prop_assert_eq!(removed.as_ref(), model.get(&id), "cancel returns the resting order");
                    model.remove(&id);
                    prop_assert!(book.cancel(id).is_none(), "a second cancel finds nothing");
                }
            }

            assert_book_matches_model(&book, &model);
            if let (Some(bid), Some(ask)) = (best(&model, Side::Buy), best(&model, Side::Sell)) {
                prop_assert!(bid < ask, "book crossed: best bid {} >= best ask {}", bid, ask);
            }
        }
    }

    /// A book rebuilt from its resting orders — which is what every replica
    /// and every new block does, through `insert_resting` — matches the next
    /// order exactly as the book that produced those orders. Insertion order
    /// into the rebuilt book is scrambled to make the point that the rows'
    /// order on disk cannot matter.
    #[test]
    fn a_rehydrated_book_matches_like_the_original(
        ops in prop::collection::vec(op_strategy(), 1..60),
        probe in op_strategy(),
        rotate in any::<usize>(),
    ) {
        let mut book = SpotBook::new();
        let mut seq = 0u64;
        for op in ops {
            if let Op::Place { owner: o, side, price, qty, tif, post_only } = op {
                seq += 1;
                book.place(IncomingOrder {
                    order_id: oid(seq), owner: owner(o), side, price, qty, tif, post_only,
                    client_order_id: H256::default(), seq,
                });
            }
        }
        let mut rows: Vec<RestingOrder> = (0..OWNERS).flat_map(|b| book.resting_orders_of(&owner(b))).collect();
        if !rows.is_empty() {
            let k = rotate % rows.len();
            rows.rotate_left(k);
            rows.reverse();
        }
        let mut rebuilt = SpotBook::new();
        for r in rows {
            rebuilt.insert_resting(r);
        }
        if let Op::Place { owner: o, side, price, qty, tif, post_only } = probe {
            seq += 1;
            let inc = IncomingOrder {
                order_id: oid(seq), owner: owner(o), side, price, qty, tif, post_only,
                client_order_id: H256::default(), seq,
            };
            prop_assert_eq!(book.place(inc.clone()), rebuilt.place(inc));
        }
    }
}

/// The matcher refuses a zero quantity or price with a silent no-op rather
/// than a panic: admission is the executor's job, and a matcher that aborts on
/// bad input takes a replica down with it.
#[test]
fn a_zero_quantity_or_price_is_a_no_op_not_a_panic() {
    let mut book = SpotBook::new();
    book.place(IncomingOrder {
        order_id: oid(1),
        owner: owner(0),
        side: Side::Sell,
        price: 100,
        qty: 5,
        tif: Tif::Gtc,
        post_only: false,
        client_order_id: H256::default(),
        seq: 1,
    });
    for (price, qty) in [(0u128, 5u128), (100, 0)] {
        let res = book.place(IncomingOrder {
            order_id: oid(2),
            owner: owner(1),
            side: Side::Buy,
            price,
            qty,
            tif: Tif::Gtc,
            post_only: false,
            client_order_id: H256::default(),
            seq: 2,
        });
        assert!(res.fills.is_empty() && res.mutations.is_empty() && res.rest.is_none());
        assert_eq!(res.rejected, None);
    }
    assert_eq!(book.len(), 1, "the resting ask is untouched");
}
