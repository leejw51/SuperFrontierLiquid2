//! M12 baseline: funding-index point-lookup scan across N markets.
//!
//! Per-block funding ticks read each market's `FundingIndex` row from
//! `CF_FUNDING_INDICES`. M12 enabled ZSTD compression on this CF (cold,
//! one read per market per tick) and bumped the shared block cache to
//! 64 MiB. With every funding row resident in cache the scan should be
//! ~ns per market.
//!
//! # Recorded baseline (dev MacBook)
//!
//! ```text
//! scan_funding/1               ~ 0.6 µs
//! scan_funding/10              ~ 5   µs
//! scan_funding/100             ~ 42  µs
//! ```
//!
//! Linear with market count; per-market cost is the rocksdb point-lookup
//! itself (cache-resident SST block + bincode decode). The shared cache
//! ensures repeat scans don't pay disk cost.

use cb_state::{Database, FundingIndices};
use cb_types::{DerivId, FundingIndex, Signed};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tempfile::TempDir;

fn populate(db: &Database, n: usize) {
    let f = FundingIndices::new(db);
    for i in 0..n as u16 {
        let m = DerivId::perp(i);
        f.put(&FundingIndex {
            market: m,
            cumulative: Signed::from_i128(i as i128 * 17),
            last_tick_height: i as u64 * 3600,
            rate_q9: 100,
        });
    }
}

fn bench_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("scan_funding");
    for &size in &[1usize, 10, 100] {
        let dir = TempDir::new().expect("tempdir");
        let db = Database::open(dir.path()).expect("open db");
        populate(&db, size);
        let f = FundingIndices::new(&db);

        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &n| {
            b.iter(|| {
                let mut sum: i128 = 0;
                for i in 0..n as u16 {
                    let m = DerivId::perp(i);
                    if let Some(fi) = f.get(&m) {
                        // Touch a field so the compiler can't elide the read.
                        sum = sum.wrapping_add(fi.last_tick_height as i128);
                    }
                }
                criterion::black_box(sum);
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_scan);
criterion_main!(benches);
