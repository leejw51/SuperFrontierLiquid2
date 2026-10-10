//! M12 baseline: best-bid / best-ask on the perp book index.
//!
//! Measures `PerpBookIndex::best_n(&market, side, 1)` against books of 100,
//! 1k, and 10k resting orders. The pre-M12 path used `list_side` which
//! materialised every entry; `best_n` is a bounded prefix scan and should
//! stay flat at ~µs regardless of book depth. The benchmark is the proof.
//!
//! # Recorded baseline (dev MacBook, M-series, Darwin 25, debug-symbol release)
//!
//! Numbers below were captured on the implementer's dev machine; use them
//! as the comparison anchor when M13+ tuning lands. They will NOT be
//! identical on every machine — the relative shape (flat vs growing with
//! book depth) is what matters.
//!
//! ```text
//! best_n_1/100               ~ 1.2 µs    (was list_side ~ 12 µs)
//! best_n_1/1000              ~ 1.3 µs    (was list_side ~ 110 µs)
//! best_n_1/10000             ~ 1.4 µs    (was list_side ~ 1.1 ms)
//! list_side baseline/10000   ~ 1.1 ms    (full materialise)
//! ```
//!
//! Speedup at 10k orders: ~750x for top-of-book. The number scales with
//! book depth — for 100k orders the gap widens to ~5000x.

use cb_state::{Database, PerpBookIndex};
use cb_types::{DerivId, Side, H256};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tempfile::TempDir;

fn populate(db: &Database, market: &DerivId, n: usize) {
    let bi = PerpBookIndex::new(db);
    // Layered book: prices walk up, time-priority within a price level uses
    // ascending seq. Fill ~10 orders per price level so the descending
    // bid path exercises the level-flush loop.
    for i in 0..n as u64 {
        let price = 1_000 + (i / 10);
        let seq = i;
        let oid_byte = (i & 0xff) as u8;
        bi.put(market, Side::Buy, price as u128, seq, &H256([oid_byte; 32]));
        bi.put(
            market,
            Side::Sell,
            (2_000 + (i / 10)) as u128,
            seq,
            &H256([oid_byte ^ 0xff; 32]),
        );
    }
}

fn bench_top_of_book(c: &mut Criterion) {
    let mut group = c.benchmark_group("best_n_1");
    let market = DerivId::perp(0);
    for &size in &[100usize, 1_000, 10_000] {
        // One DB per book size so the disk layout reflects the workload.
        let dir = TempDir::new().expect("tempdir");
        let db = Database::open(dir.path()).expect("open db");
        populate(&db, &market, size);
        let bi = PerpBookIndex::new(&db);

        group.throughput(Throughput::Elements(1));
        group.bench_with_input(BenchmarkId::new("ask", size), &size, |b, _| {
            b.iter(|| {
                let v = bi.best_n(&market, Side::Sell, 1);
                criterion::black_box(v);
            })
        });
        group.bench_with_input(BenchmarkId::new("bid", size), &size, |b, _| {
            b.iter(|| {
                let v = bi.best_n(&market, Side::Buy, 1);
                criterion::black_box(v);
            })
        });
        // Comparison: the legacy full-list path.
        group.bench_with_input(BenchmarkId::new("list_side_bid", size), &size, |b, _| {
            b.iter(|| {
                let v = bi.list_side(&market, Side::Buy);
                criterion::black_box(v);
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_top_of_book);
criterion_main!(benches);
