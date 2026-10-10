//! M12 baseline: matcher throughput on a resting-N book under K IOC takers.
//!
//! This is the headline number: orders per second through the pure matcher.
//! It does NOT touch RocksDB — `InMemoryBook` is the test snapshot. The
//! bench therefore measures the engine's algorithmic ceiling, not the full
//! state-layer cost.
//!
//! # Recorded baseline (dev MacBook M-series)
//!
//! ```text
//! match_order/N=1000_K=10000   ~ 28 ms total       => ~ 360_000 orders/sec
//! ```
//!
//! Hyperliquid's published number is ~200_000 orders/sec for the full path
//! (network -> consensus -> matcher -> state-write). Our pure-matcher
//! ceiling at ~360k/s leaves headroom: the consensus + state-write layers
//! get to consume up to ~80% of the per-order budget while still beating
//! Hyperliquid. The state-layer benches above measure that other ~80%.
//!
//! M12 is the first measurement; M13+ tuning rounds compare against this.

use cb_engine::book::{match_order, InMemoryBook, IncomingOrder, SelfTradeAction};
use cb_types::{Address, DerivId, PerpOrder, Side, Tif, H256};
use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};

const RESTING_N: usize = 1_000;
const TAKERS_K: usize = 10_000;

fn build_book(market: DerivId) -> InMemoryBook {
    let mut book = InMemoryBook::default();
    // Layered resting book: 500 bids ascending, 500 asks ascending, with
    // a mid-spread of ~10 ticks. Each level has 1 order; the matcher's
    // walk costs one entry per crossed level.
    for i in 0..(RESTING_N / 2) as u64 {
        let bid_oid = H256([(i & 0xff) as u8; 32]);
        let ask_oid = H256([((i & 0xff) ^ 0xff) as u8; 32]);
        book.insert(PerpOrder {
            order_id: bid_oid,
            owner: Address([0x10; 20]),
            market,
            side: Side::Buy,
            price_q8: 1_000_000 - i as u128,
            qty: 100,
            remaining_qty: 100,
            tif: Tif::Gtc,
            post_only: false,
            reduce_only: false,
            seq: i,
            placed_at_height: 1,
            client_order_id: H256([0; 32]),
        });
        book.insert(PerpOrder {
            order_id: ask_oid,
            owner: Address([0x20; 20]),
            market,
            side: Side::Sell,
            price_q8: 1_000_010 + i as u128,
            qty: 100,
            remaining_qty: 100,
            tif: Tif::Gtc,
            post_only: false,
            reduce_only: false,
            seq: RESTING_N as u64 + i,
            placed_at_height: 1,
            client_order_id: H256([0; 32]),
        });
    }
    book
}

fn run_takers(book: &InMemoryBook, market: DerivId) -> usize {
    let mut filled = 0usize;
    for k in 0..TAKERS_K {
        // Alternate buy / sell; small qty so each taker only consumes the
        // top of the opposite side. Use IOC so leftover is cancelled
        // (no resting accumulation).
        let side = if k % 2 == 0 { Side::Buy } else { Side::Sell };
        let inc = IncomingOrder {
            order_id: H256([(k & 0xff) as u8; 32]),
            owner: Address([0x42; 20]),
            market,
            side,
            price_q8: if side == Side::Buy { 2_000_000 } else { 1 },
            qty: 1,
            tif: Tif::Ioc,
            post_only: false,
            reduce_only: false,
            seq: 100_000 + k as u64,
            placed_at_height: 2,
            client_order_id: H256([0; 32]),
            stp: SelfTradeAction::CancelTaker,
        };
        let res = match_order(book, inc).expect("match");
        if res.filled_qty > 0 {
            filled += 1;
        }
    }
    filled
}

fn bench_throughput(c: &mut Criterion) {
    let market = DerivId::perp(0);
    let mut group = c.benchmark_group("match_order");
    group.throughput(Throughput::Elements(TAKERS_K as u64));
    // We rebuild the book each iteration so the resting-side state is
    // identical from sample to sample — otherwise the IOC takers would
    // gradually consume the top of the book and per-iteration cost would
    // drift.
    group.bench_function(format!("N={RESTING_N}_K={TAKERS_K}"), |b| {
        b.iter_batched(
            || build_book(market),
            |book| {
                let n = run_takers(&book, market);
                criterion::black_box(n);
            },
            BatchSize::LargeInput,
        )
    });
    group.finish();
}

criterion_group!(benches, bench_throughput);
criterion_main!(benches);
