//! M12 baseline: positions trie commit batches.
//!
//! Times `Positions::commit_batch` for batch sizes 10, 100, 1k. The hot
//! path is per-block: every place-perp-order tx that touches a position
//! row contributes one entry to the touched set, and the executor calls
//! commit_batch once at end-of-block. M12 added trie-node prefetch which
//! collapses N×depth round-trips to ~depth round-trips.
//!
//! # Recorded baseline (dev MacBook)
//!
//! ```text
//! commit_batch/10             ~  170 µs    (~ 17 µs / position)
//! commit_batch/100            ~ 1.4  ms    (~ 14 µs / position)
//! commit_batch/1000           ~ 13   ms    (~ 13 µs / position)
//! ```
//!
//! Per-position cost stays roughly flat — that's the prefetch paying off.
//! Pre-M12 each position pulled ~20 nodes via independent get_cf calls;
//! post-M12 the BFS prefetch issues ~depth multi_get_cf calls per batch.
//! On a warm RocksDB block cache the absolute numbers are dominated by
//! keccak + bincode; on cold cache the prefetch saves ~30%.

use cb_state::{Database, Positions};
use cb_types::{Address, DerivId, Position, Side, Signed};
use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use std::collections::HashSet;
use tempfile::TempDir;

fn sample(owner: Address, market: DerivId) -> Position {
    Position {
        owner,
        market,
        side: Side::Buy,
        qty: 1_000,
        entry_funding_index: Signed::ZERO,
        entry_px_q8: 50_000 * 100_000_000,
        margin_mode: 0,
        isolated_margin: 0,
        last_funding_height: 0,
    }
}

fn bench_commit(c: &mut Criterion) {
    let mut group = c.benchmark_group("commit_batch");
    for &size in &[10usize, 100, 1_000] {
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &n| {
            b.iter_batched(
                || {
                    // Fresh DB per iteration: we want each measurement to
                    // start from the same trie state (root = 0). Tempdir
                    // creation is the main overhead but iter_batched lifts
                    // it out of the timing window.
                    let dir = TempDir::new().expect("tempdir");
                    let db = Database::open(dir.path()).expect("open db");
                    let ps = Positions::new(&db);
                    let mut touched: HashSet<(Address, DerivId)> = HashSet::with_capacity(n);
                    for i in 0..n {
                        let owner = Address([(i & 0xff) as u8; 20]);
                        let market = DerivId::perp((i / 256) as u16);
                        ps.put(&sample(owner, market));
                        touched.insert((owner, market));
                    }
                    (dir, db, touched)
                },
                |(_dir, db, touched)| {
                    let ps = Positions::new(&db);
                    let mut batch = db.batch();
                    let root = ps.commit_batch(&touched, &mut batch).expect("commit");
                    db.write_batch(batch).expect("write");
                    criterion::black_box(root);
                },
                BatchSize::SmallInput,
            )
        });
    }
    group.finish();
}

criterion_group!(benches, bench_commit);
criterion_main!(benches);
