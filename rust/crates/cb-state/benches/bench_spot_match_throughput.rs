//! The pure spot matcher, alone: `SpotBook::place` in memory, no executor, no
//! accounts, no stage, no RocksDB, no trie, one thread.
//!
//! Same workload shape as `make orderbench`: a book 500 deep per side over 20
//! price levels, then a stream alternating makers (`Gtc`, resting a random
//! level behind the touch) and takers (`Ioc`, priced through the book), sides
//! at random, makers and takers drawn from disjoint owner sets so no order is
//! refused as a self-trade. Every taker meets a maker, so half the stream
//! fills.
//!
//! This is the number to hold next to the block loop's: whatever the loop
//! does per order beyond this is bookkeeping around the matcher, not matching.

use cb_engine::spot_book::{IncomingOrder, RestingOrder, SpotBook};
use cb_types::{Address, Side, Tif, H256};
use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};

const TICK: u128 = 100_000;
const QTY: u128 = 100_000;
const MID: u128 = 50_000 * 1_000_000;
const LEVELS: u64 = 20;
const DEPTH: u64 = 500;
const ORDERS: usize = 100_000;

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

fn oid(n: u64) -> H256 {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&n.to_be_bytes());
    H256(b)
}

fn owner(i: u64) -> Address {
    let mut a = [0u8; 20];
    a[..8].copy_from_slice(&i.to_be_bytes());
    Address(a)
}

fn resting(side: Side, level: u64) -> u128 {
    match side {
        Side::Buy => MID - TICK - TICK * level as u128,
        Side::Sell => MID + TICK + TICK * level as u128,
    }
}

fn crossing(side: Side) -> u128 {
    let span = TICK * (LEVELS as u128 + 2);
    match side {
        Side::Buy => MID + TICK + span,
        Side::Sell => MID - TICK - span,
    }
}

fn build_book() -> SpotBook {
    let mut book = SpotBook::new();
    let mut seq = 0u64;
    for i in 0..DEPTH {
        for side in [Side::Buy, Side::Sell] {
            seq += 1;
            book.insert_resting(RestingOrder {
                order_id: oid(seq),
                owner: owner((i % 32) * 2), // makers: even owners
                side,
                price: resting(side, i % LEVELS),
                qty: QTY,
                qty_remaining: QTY,
                tif: Tif::Gtc,
                post_only: false,
                client_order_id: H256([0; 32]),
                seq,
            });
        }
    }
    book
}

/// The order stream, generated up front so the timed region is only the
/// matcher.
fn stream() -> Vec<IncomingOrder> {
    let mut rng = Lcg(0xC0FFEE);
    let mut out = Vec::with_capacity(ORDERS);
    for n in 0..ORDERS as u64 {
        let side = if rng.next() % 2 == 0 {
            Side::Buy
        } else {
            Side::Sell
        };
        let maker = n % 2 == 0;
        let (price, tif, who) = if maker {
            (
                resting(side, rng.next() % LEVELS),
                Tif::Gtc,
                (rng.next() % 32) * 2,
            )
        } else {
            (crossing(side), Tif::Ioc, (rng.next() % 32) * 2 + 1)
        };
        out.push(IncomingOrder {
            order_id: oid(1_000_000 + n),
            owner: owner(who),
            side,
            price,
            qty: QTY,
            tif,
            post_only: false,
            client_order_id: H256([0; 32]),
            seq: 1_000_000 + n,
        });
    }
    out
}

fn bench(c: &mut Criterion) {
    let orders = stream();
    let mut g = c.benchmark_group("spot_book_place");
    g.throughput(Throughput::Elements(ORDERS as u64));
    g.sample_size(20);
    g.bench_function(format!("depth={DEPTH}_orders={ORDERS}"), |b| {
        b.iter_batched(
            || (build_book(), orders.clone()),
            |(mut book, orders)| {
                let mut fills = 0usize;
                for o in orders {
                    let r = book.place(o);
                    fills += r.fills.len();
                }
                fills
            },
            BatchSize::LargeInput,
        )
    });
    g.finish();

    // One plain timing so the absolute figure is in the output without
    // criterion's statistics: matched orders per second as well as placed.
    let mut book = build_book();
    let orders = stream();
    let t = std::time::Instant::now();
    let mut matched = 0usize;
    for o in orders {
        if !book.place(o).fills.is_empty() {
            matched += 1;
        }
    }
    let secs = t.elapsed().as_secs_f64();
    eprintln!(
        "\nspot matcher alone: {ORDERS} orders in {secs:.3}s = {:.0} orders/s, {:.0} matched/s\n",
        ORDERS as f64 / secs,
        matched as f64 / secs
    );
}

criterion_group!(benches, bench);
criterion_main!(benches);
