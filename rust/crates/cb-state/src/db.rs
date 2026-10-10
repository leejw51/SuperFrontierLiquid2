//! RocksDB wrapper. One DB instance, one column family per logical table.
//!
//! # M12 CF tuning rationale
//!
//! RocksDB exposes per-CF tuning surfaces. We classify each CF by access shape
//! and apply options accordingly. The defaults are deliberate: this is a hot
//! exchange path where 10-20% read latency wins compound over millions of
//! orders/sec.
//!
//! ## Shared block cache (64 MiB LRU)
//! All CFs share one LRU block cache. RocksDB's default is a per-CF 8 MiB
//! cache, which fragments memory and forces re-reads of trie nodes that have
//! already been pulled in by a sibling CF. One shared cache lets a hot
//! `trie_nodes` block stay resident even while we're hammering `perp_orders`.
//! 64 MiB is enough to keep an active book + the upper levels of all three
//! tries (accounts, positions, markets) in memory on the hot path.
//!
//! ## Write buffer (32 MiB per CF)
//! Default is 64 MiB which is fine for big CFs but wasteful for the dozen
//! small CFs (meta, qc, validators) that rarely accumulate. 32 MiB strikes a
//! balance: a busy `perp_orders` CF flushes after ~250k orders, which is one
//! second of throughput at 200k orders/sec — short enough not to dominate
//! WAL replay on crash, long enough to coalesce writes across a block.
//!
//! ## Compression
//! - **LZ4 for hot CFs** (`perp_orders`, `perp_book_index`, `oracle_state`,
//!   `trie_nodes`): we read these on every place-perp-order path. LZ4
//!   decompresses at ~4 GB/s; the cost vs uncompressed is in the noise but
//!   the on-disk savings (~2x) keep more of the working set in OS page cache.
//! - **ZSTD for cold CFs** (`positions`, `margin_accounts`, `perp_markets`,
//!   `futures_markets`, `accounts`): these hold longer-lived per-account /
//!   per-market state. Read amplification is lower (one read per tx, not
//!   one per book scan), so we trade a bit of decompression CPU for ~1.4x
//!   tighter compression than LZ4.
//! - **No compression** for small admin CFs (`meta`, `qc`, `validators`,
//!   `mempool`): values are tiny and rare; compression overhead exceeds
//!   savings.
//!
//! ## Bloom filters (10 bits/key)
//! Enabled on point-lookup CFs: `margin_accounts`, `positions`, `perp_orders`,
//! `accounts`, `trie_nodes`. A bloom filter shaves a disk seek when a key is
//! absent — common during margin checks (a fresh trader has no position) and
//! during trie traversal (most empty subtrees stop early). 10 bits/key is the
//! standard tradeoff (~1% false positive). Skipped on iterator-heavy CFs
//! (`perp_book_index`, `liquidation_queue`) where prefix scans dominate and
//! point lookups are rare.
//!
//! ## What we do NOT change
//! No CF byte layouts, no key encoders, no value codecs. None of these
//! options affect the on-wire / on-trie bytes; they're purely a SST-layout
//! and cache-policy tuning pass. Re-genesis is NOT required.

use cb_trie::{KvStore, TrieError};
use cb_types::H256;
use rocksdb::{
    BlockBasedOptions, Cache, ColumnFamilyDescriptor, DBCompressionType, Options, WriteBatch, DB,
};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use thiserror::Error;

/// A cached derived value tagged with the height it was computed at.
type DerivedSlot = (u64, Box<dyn std::any::Any + Send>);

/// One overlay entry: a key and its pending value, `None` meaning a delete.
type OverlayRow = (Vec<u8>, Option<Vec<u8>>);
/// A borrowed walk over one overlay map, in either direction.
type OverlayIter<'a> = Box<dyn Iterator<Item = (&'a Vec<u8>, &'a Option<Vec<u8>>)> + 'a>;

pub const CF_ACCOUNTS: &str = "accounts";
pub const CF_ORDERS: &str = "orders";
pub const CF_BOOK_INDEX: &str = "book_index";
pub const CF_MARKETS: &str = "markets";
pub const CF_BLOCKS: &str = "blocks";
pub const CF_BLOCK_HASH: &str = "block_by_hash";
pub const CF_META: &str = "meta";
pub const CF_MEMPOOL: &str = "mempool";
pub const CF_TRIE_NODES: &str = "trie_nodes";
pub const CF_VALIDATORS: &str = "validators";
pub const CF_QC: &str = "qc";
/// Fast-mirror replica block log. Key = height (BE u64). Value layout:
///   `hash[32] || blocktime_be[8] || capnp_bytes[..]`
/// One row per block, written atomically with the head pointer in `CF_META`.
/// Holds only the four fields the operator asked for (height, hash,
/// blocktime, raw block bytes). Disjoint from `CF_BLOCKS` — strict-sync still
/// uses `CF_BLOCKS`, fast-mirror uses this one.
pub const CF_MIRROR_BLOCKS: &str = "mirror_blocks";

// ---------------------------------------------------------------------------
// Derivatives column families (M1: schema only — handlers ship M2+).
//
// Key/value layouts mirror `docs/design/perps-protocol.md §3.1`. Keys are
// explicit byte strings (never `bincode(struct)`) so prefix iteration is
// deterministic and stable across bincode versions.
// ---------------------------------------------------------------------------

/// `(owner: 20 || deriv_id: 4)` -> `bincode(Position)`. Prefix-scan by owner
/// to enumerate every position the account holds.
pub const CF_POSITIONS: &str = "positions";
/// `owner: 20` -> `bincode(MarginAccount)`. Free-collateral pool per address.
pub const CF_MARGIN_ACCOUNTS: &str = "margin_accounts";
/// `deriv_id: 4` -> `bincode(FundingIndex)`. Cumulative-funding state per perp.
pub const CF_FUNDING_INDICES: &str = "funding_indices";
/// `deriv_id: 4` -> `bincode(PerpMarket)`.
pub const CF_PERP_MARKETS: &str = "perp_markets";
/// `deriv_id: 4` -> `bincode(FutureMarket)`.
pub const CF_FUTURES_MARKETS: &str = "futures_markets";
/// `(deriv_id: 4 || kind: u8)` -> `bincode(OraclePrice)` (kind=0) or
/// `bincode(MarkPrice)` (kind=1).
pub const CF_ORACLE_STATE: &str = "oracle_state";
/// `(height: u64 BE || seq: u64 BE)` -> `bincode(LiquidationEntry)`. Drained
/// at end-of-block in `(height, seq)` order.
pub const CF_LIQUIDATION_QUEUE: &str = "liquidation_queue";
/// `order_id: 32` -> `bincode(perp order record)`. Like `CF_ORDERS` for spot.
/// Auxiliary; not committed to the trie (matching state recomputes on replay).
pub const CF_PERP_ORDERS: &str = "perp_orders";
/// `(deriv_id: 4 || side: 1 || price_q8: 16 BE || seq: 8 BE)` -> `order_id: 32`.
/// Prefix-iterate by `[deriv_id][side]` for price-time-priority traversal.
/// Auxiliary; not committed to the trie.
pub const CF_PERP_BOOK_INDEX: &str = "perp_book_index";
/// `(owner: 20 || order_id: 32)` -> empty. Secondary index over
/// `CF_PERP_ORDERS` so the margin gate can find every order an account has
/// resting without scanning the whole order CF (QA F11). Maintained by
/// `PerpOrders::put` / `delete` only. Auxiliary; not committed to the trie.
pub const CF_PERP_ORDERS_BY_OWNER: &str = "perp_orders_by_owner";

/// Every column family this database holds, in a fixed order. The order is
/// also the stage's per-CF index, and tests that compare two databases walk
/// it to prove a rejected block left no trace.
pub const ALL_CFS: &[&str] = &[
    CF_ACCOUNTS,
    CF_ORDERS,
    CF_BOOK_INDEX,
    CF_MARKETS,
    CF_BLOCKS,
    CF_BLOCK_HASH,
    CF_META,
    CF_MEMPOOL,
    CF_TRIE_NODES,
    CF_VALIDATORS,
    CF_QC,
    CF_MIRROR_BLOCKS,
    // Derivatives:
    CF_POSITIONS,
    CF_MARGIN_ACCOUNTS,
    CF_FUNDING_INDICES,
    CF_PERP_MARKETS,
    CF_FUTURES_MARKETS,
    CF_ORACLE_STATE,
    CF_LIQUIDATION_QUEUE,
    CF_PERP_ORDERS,
    CF_PERP_BOOK_INDEX,
    // Appended last: the stage indexes its per-CF maps by position here.
    CF_PERP_ORDERS_BY_OWNER,
];

/// Per-CF options. Classifies each CF as hot/cold and point-lookup vs. iterator
/// to drive compression and bloom-filter choices. See module docs for the
/// rationale block. The shared `block_cache` is plumbed through every CF so
/// the LRU stays unified — fragmenting per-CF caches would defeat the purpose.
fn cf_options_for(cf: &str, block_cache: &Cache) -> Options {
    let mut opts = Options::default();

    // 32 MiB write buffer per CF — balances per-block coalescing vs WAL replay.
    opts.set_write_buffer_size(32 * 1024 * 1024);

    // Order books are a small live set under enormous churn, and they are
    // *scanned*, not just point-read: `BookIndex::list_orders_for_market`
    // walks one `market ‖ side` prefix per market per block to rebuild the
    // matcher's view. Every filled or cancelled order leaves a delete marker
    // that those scans must step over until a compaction drops it, so on this
    // access pattern the default settings turn a hundred resting orders into a
    // scan over every order the chain has ever matched.
    //
    // A 32 MiB memtable is the worst part of it: at ~60 bytes an index row it
    // buffers on the order of half a million deletes before it flushes, and
    // the memtable iterator walks all of them in memory on every scan. A
    // profile of a loaded sequencer put 75% of the process inside exactly that
    // walk. So for these column families: flush early, and compact L0 as soon
    // as there is anything to compact, which is what actually drops the
    // markers.
    //
    // The cost is more, smaller SST files and more compaction work on a
    // background thread — which this node has spare, being otherwise
    // single-threaded — in exchange for scans that cost what the live data
    // costs. Nothing here changes what is stored or read, only how RocksDB
    // keeps it.
    let churns_hard = matches!(
        cf,
        CF_ORDERS | CF_BOOK_INDEX | CF_PERP_ORDERS | CF_PERP_BOOK_INDEX | CF_PERP_ORDERS_BY_OWNER
    );
    if churns_hard {
        opts.set_write_buffer_size(2 * 1024 * 1024);
        opts.set_max_write_buffer_number(3);
        opts.set_min_write_buffer_number_to_merge(1);
        opts.set_level_zero_file_num_compaction_trigger(2);
        // File size stays at RocksDB's default on purpose. Smaller files drop
        // markers sooner but multiply the file count, and the file count is
        // what the open-file limit above is spent on.
        // Re-compact a file that has sat untouched for a minute, so a quiet
        // period cannot leave a layer of markers behind for the next burst.
        opts.set_periodic_compaction_seconds(60);
    }

    // Compression family. Hot CFs use LZ4; cold CFs use ZSTD; admin CFs
    // stay uncompressed. The classification is conservative: anything that
    // sees a write per tx counts as hot.
    let compression = match cf {
        // Hot: matched per place-perp-order tx and per trie traversal.
        CF_PERP_ORDERS | CF_PERP_BOOK_INDEX | CF_ORACLE_STATE | CF_TRIE_NODES | CF_ORDERS
        | CF_BOOK_INDEX => DBCompressionType::Lz4,
        // Cold: bigger values, fewer reads per tx.
        CF_POSITIONS | CF_MARGIN_ACCOUNTS | CF_PERP_MARKETS | CF_FUTURES_MARKETS | CF_ACCOUNTS
        | CF_BLOCKS | CF_MIRROR_BLOCKS | CF_FUNDING_INDICES => DBCompressionType::Zstd,
        // Admin / small: not worth compressing.
        _ => DBCompressionType::None,
    };
    opts.set_compression_type(compression);

    // Block-based table options: shared cache + (optionally) bloom filter.
    let mut bbt = BlockBasedOptions::default();
    bbt.set_block_cache(block_cache);
    // 16 KiB block size — a touch larger than RocksDB's 4 KiB default. Most
    // of our values are small (Position ~80B, PerpOrder ~140B), so a 16 KiB
    // block holds ~100 rows. Reduces index size + improves block-cache hit
    // rate on prefix scans (book index, position-by-owner).
    bbt.set_block_size(16 * 1024);

    // Bloom filters: enable on point-lookup CFs. 10 bits/key ≈ 1% FP.
    // `set_bloom_filter(bits, block_based_mode=false)` selects the modern
    // full-filter format which performs better than the legacy block-based
    // bloom on point lookups.
    let wants_bloom = matches!(
        cf,
        CF_MARGIN_ACCOUNTS
            | CF_POSITIONS
            | CF_PERP_ORDERS
            | CF_ORDERS
            | CF_ACCOUNTS
            | CF_TRIE_NODES
    );
    if wants_bloom {
        bbt.set_bloom_filter(10.0, false);
        // Cache the index/filter blocks like data blocks. Without this,
        // every cold-cache point lookup pays a disk seek for the filter.
        bbt.set_cache_index_and_filter_blocks(true);
        bbt.set_pin_l0_filter_and_index_blocks_in_cache(true);
    }

    opts.set_block_based_table_factory(&bbt);
    opts
}

#[derive(Debug, Error)]
pub enum DbError {
    #[error("rocksdb: {0}")]
    Rocks(#[from] rocksdb::Error),
    #[error("missing column family: {0}")]
    MissingCf(String),
    /// The asynchronous writer failed, or could not be started. Once a write
    /// has failed every later write reports it: see `PendingWrites`.
    #[error("storage: {0}")]
    Storage(String),
}

/// Position of `cf` in [`ALL_CFS`]. The stage indexes its per-CF maps by this
/// number so a staged key is `(usize, Vec<u8>)` rather than an allocated
/// `String` per lookup.
fn cf_index(cf: &str) -> Option<usize> {
    ALL_CFS.iter().position(|c| *c == cf)
}

/// One pending write recorded by [`Stage`], with enough information to undo it.
struct StagedWrite {
    cf: usize,
    key: Vec<u8>,
    /// What the stage held for this key before the write. `None` means the
    /// stage had no entry at all (so undoing removes the key); `Some(prev)`
    /// restores `prev`, which itself is `None` for a staged delete.
    prev: Option<Option<Vec<u8>>>,
}

/// In-memory write buffer for one block's mutations.
///
/// Values are `Option<Vec<u8>>`: `Some(bytes)` is a pending put, `None` a
/// pending delete (a tombstone, so a read does not fall through to the row
/// still sitting in RocksDB). Each CF gets its own `BTreeMap` so a prefix
/// scan is a range query and a point lookup needs no key allocation.
///
/// `journal` is an undo log in write order. It is what makes a *single
/// transaction* inside a block revertible: the executor marks the journal
/// length before applying a tx and truncates back to that mark if the tx
/// fails, which restores the stage byte-for-byte.
struct Stage {
    cfs: Vec<BTreeMap<Vec<u8>, Option<Vec<u8>>>>,
    journal: Vec<StagedWrite>,
}

impl Stage {
    fn new() -> Self {
        Self {
            cfs: (0..ALL_CFS.len()).map(|_| BTreeMap::new()).collect(),
            journal: Vec::new(),
        }
    }

    fn write(&mut self, cf: usize, key: &[u8], value: Option<Vec<u8>>) {
        let prev = self.cfs[cf].insert(key.to_vec(), value);
        self.journal.push(StagedWrite {
            cf,
            key: key.to_vec(),
            prev,
        });
    }

    fn get(&self, cf: usize, key: &[u8]) -> Option<Option<Vec<u8>>> {
        self.cfs[cf].get(key).cloned()
    }

    fn rollback_to(&mut self, mark: usize) {
        while self.journal.len() > mark {
            let w = self.journal.pop().expect("len > mark");
            match w.prev {
                Some(prev) => {
                    self.cfs[w.cf].insert(w.key, prev);
                }
                None => {
                    self.cfs[w.cf].remove(&w.key);
                }
            }
        }
    }
}

/// The writes of one committed block, as readers must see them until RocksDB
/// has absorbed them. Same shape as a [`Stage`]'s maps: one ordered map per
/// column family, `None` for a delete.
struct Overlay {
    seq: u64,
    cfs: Vec<BTreeMap<Vec<u8>, Option<Vec<u8>>>>,
}

/// One batch waiting for the writer, with what to run once it is durable.
///
/// It shares its rows with the overlay readers see (`Arc`), rather than
/// holding a copy: copying every row of a block once more on the thread that
/// matches orders was pure cost.
struct QueuedWrite {
    seq: u64,
    overlay: Arc<Overlay>,
    /// Run on the writer thread before the write: turns the rows into the
    /// rest of the block (see [`SealFn`]). `None` for a plain batch.
    seal: Option<SealFn>,
    on_durable: Option<Box<dyn FnOnce() + Send>>,
    /// A block whose seal is not known yet (see [`Database::write_deferred`]).
    /// `None` for every other write.
    deferred: Option<Arc<DeferredSlot>>,
}

impl QueuedWrite {
    /// A deferred write at the front of the queue that has had its
    /// [`ReachedFn`] run and is waiting to be sealed or discarded: nothing
    /// behind it may be written until it is.
    fn blocked(&self) -> bool {
        self.deferred.as_ref().is_some_and(|d| {
            d.reached
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none()
                && d.resolution
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_none()
        })
    }
}

/// Runs once a deferred write reaches the front of the queue — on the
/// writer thread, or inline in synchronous mode — before it waits for its
/// seal: everything written before it is then on disk, and its rows are
/// final. What the caller needs from a block before the seal is known (its
/// state root, say) goes here.
pub type ReachedFn = Box<dyn FnOnce(&[crate::state_hash::CfMap]) + Send>;

/// How a deferred write ends.
enum Resolution {
    Seal(SealFn),
    Discard,
}

struct DeferredSlot {
    reached: Mutex<Option<ReachedFn>>,
    resolution: Mutex<Option<Resolution>>,
}

/// A block's rows handed over before its seal is known: visible to every
/// read at once, written only when [`DeferredWrite::seal`] says how to seal
/// them, and dropped without a trace by [`DeferredWrite::discard`] — or by
/// dropping the handle.
///
/// The writer takes writes in order, so while this is unresolved nothing
/// queued after it reaches disk. It is for a block whose seal follows
/// shortly (a BFT proposal waiting for the certificate its successor
/// carries); resolve it before calling [`Database::flush_pending`], which
/// otherwise returns with it still pending.
///
/// Discarding retracts this block's rows only. Writes queued after it stay
/// and are written; the caller must not have queued anything that depends
/// on the discarded rows.
pub struct DeferredWrite {
    db: Database,
    slot: Arc<DeferredSlot>,
    resolved: bool,
}

impl DeferredWrite {
    /// Seal and write the block. In synchronous mode that happens before
    /// this returns, with whatever was queued behind it; with a writer
    /// thread it happens there, in order.
    pub fn seal(mut self, seal: SealFn) -> Result<(), DbError> {
        self.resolve(Resolution::Seal(seal))
    }

    /// Drop the block's rows: reads stop seeing them, nothing is written,
    /// and no error is recorded.
    pub fn discard(mut self) {
        let _ = self.resolve(Resolution::Discard);
    }

    fn resolve(&mut self, r: Resolution) -> Result<(), DbError> {
        self.resolved = true;
        {
            // Under the queue lock, so the writer cannot check and go to
            // sleep between the store and the notification.
            let _q = self
                .db
                .pending
                .queue
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *self
                .slot
                .resolution
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(r);
        }
        self.db.pending.changed.notify_all();
        if self.db.pending.mode() == WriteMode::Sync {
            self.db.drain_ready();
        }
        match self
            .db
            .pending
            .error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            Some(e) => Err(DbError::Storage(e)),
            None => Ok(()),
        }
    }
}

impl Drop for DeferredWrite {
    fn drop(&mut self) {
        if !self.resolved {
            let _ = self.resolve(Resolution::Discard);
        }
    }
}

/// What sealing a block adds to its write: rows that exist only once the
/// block's hash is known (the block body, its hash index, the head hash and
/// state root), and what to run once all of it is on disk.
pub struct Sealed {
    /// `(column family index in ALL_CFS, key, value)` puts.
    pub extra: Vec<(usize, Vec<u8>, Vec<u8>)>,
    pub after: Box<dyn FnOnce() + Send>,
}

/// Seals one block from its rows. Runs on the writer thread for a queued
/// write, inline for a synchronous one, and always in the order blocks were
/// handed over, so a sealer can carry the previous block's root and hash
/// itself instead of reading them back.
pub type SealFn = Box<dyn FnOnce(&[crate::state_hash::CfMap]) -> Result<Sealed, String> + Send>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WriteMode {
    /// `write_batch` writes RocksDB before it returns. The default.
    Sync,
    /// `write_batch` queues; a test calls [`Database::drain_one`].
    Manual,
    /// `write_batch` queues; a background thread drains.
    Threaded,
}

/// Where a block's writes wait between "committed" and "on disk".
///
/// # Why this exists
///
/// A sequencer's block loop used to end with one synchronous RocksDB write,
/// on the same thread that matches orders. On the in-process order benchmark
/// that write was 17% of the thread and persisting the block body another 8%,
/// during which the matcher sat idle. The matcher never needs the write to
/// have *happened*; it needs later reads to *see* it. So the write moves to
/// its own thread, and until it lands the block's rows are served from an
/// overlay that every read on every handle consults between its own stage and
/// RocksDB.
///
/// # The two rules that make it correct
///
/// **Readers look at the overlays before RocksDB, and the writer writes
/// RocksDB before it removes an overlay.** With both, a key is always in at
/// least one of the two places a reader looks. For iterators the same rule is
/// applied by construction: the overlay snapshot is taken *before* the RocksDB
/// iterator is opened, so a write that lands in between is in the snapshot,
/// in the iterator, or both — and [`MergedIter`] lets the overlay shadow the
/// stored row on a tie.
///
/// **Nothing is announced outside the process until the write returns.** The
/// head is visible internally at once, but the notification that drives the
/// gateway's block stream, the leader's own vote and the p2p announcement all
/// run from the writer thread, after the write. A crash therefore loses at
/// most `MAX_PENDING` blocks nobody outside was told about; the restarted node
/// continues from the durable head, and no peer holds a hash this node no
/// longer has. Reads served through the RPC during that window are the one
/// thing this does not protect: a balance read before the crash can revert.
///
/// Depth is bounded. `write_batch` waits when `MAX_PENDING` overlays are
/// outstanding, which caps both memory and how far the durable head can trail
/// the visible one.
struct PendingWrites {
    mode: Mutex<WriteMode>,
    overlays: RwLock<Vec<Arc<Overlay>>>,
    queue: Mutex<std::collections::VecDeque<QueuedWrite>>,
    /// Signalled whenever the queue gains an entry (for the writer thread) or
    /// an overlay is retired (for `write_batch` waiting on depth and for
    /// `flush_pending`).
    changed: std::sync::Condvar,
    next_seq: std::sync::atomic::AtomicU64,
    /// The first write error, if any. Once set, every later `write_batch`
    /// fails with it: a node that cannot persist a block must not go on
    /// producing them.
    error: Mutex<Option<String>>,
    /// The writer thread, once there is one. A write issued *from* that
    /// thread — a durability callback that writes something of its own —
    /// must not be queued: the queue is drained by this very thread, so
    /// waiting for it to drain would be waiting for itself. It is written
    /// straight through instead. Pruning did exactly this and deadlocked the
    /// node: the sequencer blocked on a full queue while the writer sat in
    /// `prune_one`, blocked on the same queue.
    writer_thread: Mutex<Option<std::thread::ThreadId>>,
    /// Held while one write is taken off the queue and written, so two
    /// threads draining in synchronous mode (see
    /// [`Database::write_deferred`]) cannot write out of order.
    drain: Mutex<()>,
}

/// Blocks a sequencer may have committed but not yet written.
pub const MAX_PENDING: usize = 8;

impl PendingWrites {
    fn new() -> Self {
        Self {
            mode: Mutex::new(WriteMode::Sync),
            overlays: RwLock::new(Vec::new()),
            queue: Mutex::new(std::collections::VecDeque::new()),
            changed: std::sync::Condvar::new(),
            next_seq: std::sync::atomic::AtomicU64::new(1),
            error: Mutex::new(None),
            writer_thread: Mutex::new(None),
            drain: Mutex::new(()),
        }
    }

    fn mode(&self) -> WriteMode {
        *self.mode.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether the caller is the writer thread itself.
    fn on_writer_thread(&self) -> bool {
        *self.writer_thread.lock().unwrap_or_else(|e| e.into_inner())
            == Some(std::thread::current().id())
    }

    fn overlays_snapshot(&self) -> Vec<Arc<Overlay>> {
        self.overlays
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Point lookup through the overlays, newest first. `None` means "not
    /// overlaid"; `Some(None)` means "deleted by a pending block".
    fn get(&self, cf: usize, key: &[u8]) -> Option<Option<Vec<u8>>> {
        let g = self.overlays.read().unwrap_or_else(|e| e.into_inner());
        for o in g.iter().rev() {
            if let Some(v) = o.cfs[cf].get(key) {
                return Some(v.clone());
            }
        }
        None
    }

    fn queue_is_empty(&self) -> bool {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }
}

/// A handle on the RocksDB instance.
///
/// # Staged handles
///
/// A handle is either **direct** (`stage: None`), in which case `put` and
/// `delete` go straight to RocksDB, or **staged**, in which case they are
/// buffered in memory and only reach disk when the caller folds the stage
/// into a `WriteBatch`.
///
/// Block execution runs on a staged handle. That is what makes a block's
/// mutation transactional: the executor writes order rows, book-index rows,
/// positions, margin accounts, oracle state and market rows exactly as it
/// always did — `put` and `delete` on this type — but every one of those
/// writes lands in the same atomic batch as the account tries, the block
/// bytes and the head pointer, or lands nowhere at all. Reads on a staged
/// handle see the stage first, so a later transaction in the same block still
/// observes what an earlier one wrote.
///
/// This is the same shape as `mirror_blocks_cf::put_with_meta`, generalised
/// from three keys to the whole block.
#[derive(Clone)]
pub struct Database {
    db: Arc<DB>,
    stage: Option<Arc<RwLock<Stage>>>,
    /// A slot for state a higher layer derives from this database and would
    /// rather not derive again — the matcher's in-memory order books. Opaque
    /// here on purpose: `SpotBook` lives in `cb-engine`, which sits above this
    /// crate, so this crate holds the box and never looks inside it.
    ///
    /// The `u64` is the head height the contents describe. A reader asks for a
    /// specific height and gets nothing if the slot holds another, which is
    /// what makes an abandoned block self-correcting: the block publishes for
    /// height `h`, the head stays at `h - 1` because the write never
    /// happened, and the next block asks for `h - 1` and rebuilds from the
    /// column families instead.
    derived: Arc<Mutex<Option<DerivedSlot>>>,
    /// Transactions admitted but not yet in a block, keyed by transaction
    /// hash. Node-local and deliberately not a column family — see
    /// [`crate::Mempool`] for why it moved out of RocksDB. Shared by every
    /// clone of this handle, including staged ones, so the RPC server and the
    /// block builder see one queue.
    mempool: Arc<Mutex<crate::mempool_cf::MempoolQueue>>,
    /// Committed writes RocksDB has not absorbed yet. See [`PendingWrites`].
    pending: Arc<PendingWrites>,
    /// Whether reads through this handle consult the pending overlays. The
    /// sequencer, admission and account reads must (the next block executes
    /// against that state); anything that publishes blocks outward must not,
    /// because a block is not a block until it is on disk. See
    /// [`Database::durable_view`].
    sees_pending: bool,
}

impl Database {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, DbError> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        // How many SST files this database may keep open at once.
        //
        // This was 64, to stop the parallel test suite — which opens many
        // databases at once — exhausting the process's file-descriptor limit.
        // On a chain of any size it is a throughput cliff instead: a database
        // with 200 SST files and room for 64 readers reopens them constantly,
        // and because `TableCache::FindTable` serialises on a mutex, every
        // point read queues behind the reopen. A profile of a node on a
        // 5.8 GB chain found 64% of the block loop parked in
        // `__psynch_mutexwait` under exactly that path, at 32 transactions a
        // second where a fresh chain did 53,000.
        //
        // 1,024 was the next attempt and it was not enough either: a chain
        // that a day of stress tests had grown to 79 GB held 3,551 SST files,
        // and the same thrash came back until the count was found. So:
        // comfortably more files than a chain of that size has. Not `-1`,
        // which means no limit and makes RocksDB prefetch the index and
        // filter blocks of *every* SST when it opens the database — minutes
        // of startup on a chain this size, during which the node is up and
        // producing nothing. `CB_MAX_OPEN_FILES` is for a test run that opens
        // many databases at once and would rather spend throughput than
        // descriptors.
        let max_open_files = std::env::var("CB_MAX_OPEN_FILES")
            .ok()
            .and_then(|v| v.parse::<i32>().ok())
            .unwrap_or(16_384);
        opts.set_max_open_files(max_open_files);

        // M12: shared 64 MiB LRU block cache across every CF. See module
        // docs for the rationale. Cache is `Arc`-internal, so cloning the
        // handle into each `BlockBasedOptions` still points at one cache.
        let block_cache = Cache::new_lru_cache(64 * 1024 * 1024);

        let cfs = ALL_CFS
            .iter()
            .map(|n| ColumnFamilyDescriptor::new(*n, cf_options_for(n, &block_cache)))
            .collect::<Vec<_>>();
        let db = DB::open_cf_descriptors(&opts, path, cfs)?;
        Ok(Self {
            db: Arc::new(db),
            stage: None,
            derived: Arc::new(Mutex::new(None)),
            mempool: Arc::new(Mutex::new(crate::mempool_cf::MempoolQueue::default())),
            pending: Arc::new(PendingWrites::new()),
            sees_pending: true,
        })
    }

    /// A handle onto the same RocksDB whose writes are buffered in memory
    /// instead of going to disk. See the type-level docs. The caller owns the
    /// stage: dropping the handle discards every buffered write, which is
    /// exactly the rollback semantics a failed block wants.
    ///
    /// Staging is not nestable — a stage on a stage would read through the
    /// wrong overlay — so this asserts the receiver is direct.
    pub fn staged(&self) -> Database {
        assert!(
            self.stage.is_none(),
            "Database::staged called on an already-staged handle"
        );
        Database {
            db: Arc::clone(&self.db),
            stage: Some(Arc::new(RwLock::new(Stage::new()))),
            derived: Arc::clone(&self.derived),
            mempool: Arc::clone(&self.mempool),
            pending: Arc::clone(&self.pending),
            sees_pending: self.sees_pending,
        }
    }

    /// A handle that reads only what RocksDB has: it skips the pending
    /// overlays. The block stream, `getBlock`, the p2p head announcer and the
    /// dashboard read through this, so nothing leaves the process describing
    /// a block a crash could still take back. Never staged; writes through it
    /// are a bug.
    pub fn durable_view(&self) -> Database {
        assert!(
            self.stage.is_none(),
            "durable_view on a staged handle makes no sense"
        );
        Database {
            db: Arc::clone(&self.db),
            stage: None,
            derived: Arc::clone(&self.derived),
            mempool: Arc::clone(&self.mempool),
            pending: Arc::clone(&self.pending),
            sees_pending: false,
        }
    }

    /// Empty the derived-state slot, returning its contents only if they
    /// describe head height `stamp`. Always empties it: whoever asks is about
    /// to mutate what it describes, and a second asker must rebuild rather
    /// than share.
    pub fn derived_take_at(&self, stamp: u64) -> Option<Box<dyn std::any::Any + Send>> {
        let mut g = self.derived.lock().expect("derived slot poisoned");
        match g.take() {
            Some((at, v)) if at == stamp => Some(v),
            _ => None,
        }
    }

    /// Fill the derived-state slot with something that describes head height
    /// `stamp`.
    pub fn derived_put(&self, stamp: u64, value: Box<dyn std::any::Any + Send>) {
        *self.derived.lock().expect("derived slot poisoned") = Some((stamp, value));
    }

    /// The shared pending-transaction queue. `Mempool` is the only caller.
    pub(crate) fn mempool_map(&self) -> &Mutex<crate::mempool_cf::MempoolQueue> {
        &self.mempool
    }

    pub fn is_staged(&self) -> bool {
        self.stage.is_some()
    }

    fn stage_guard(&self) -> Option<&Arc<RwLock<Stage>>> {
        self.stage.as_ref()
    }

    /// Record the current position of the stage's undo log. Pass the result to
    /// [`Database::stage_rollback_to`] to revert every write made since.
    /// Returns 0 on a direct handle (nothing to roll back).
    pub fn stage_mark(&self) -> usize {
        match self.stage_guard() {
            Some(s) => read_stage(s).journal.len(),
            None => 0,
        }
    }

    /// Undo every staged write made after `mark`.
    pub fn stage_rollback_to(&self, mark: usize) {
        if let Some(s) = self.stage_guard() {
            write_stage(s).rollback_to(mark);
        }
    }

    /// Drop the undo log, keeping the buffered writes. Called once a
    /// transaction has succeeded: its writes are now part of the block and can
    /// never be individually reverted, so retaining their `prev` values only
    /// costs memory.
    pub fn stage_commit_point(&self) {
        if let Some(s) = self.stage_guard() {
            write_stage(s).journal.clear();
        }
    }

    /// Number of distinct keys currently buffered. Diagnostics and tests.
    pub fn stage_len(&self) -> usize {
        match self.stage_guard() {
            Some(s) => read_stage(s).cfs.iter().map(|m| m.len()).sum(),
            None => 0,
        }
    }

    /// Copy every buffered write into `batch`. The stage is left intact — the
    /// caller may still need to read through it (the tries re-read the rows
    /// this block wrote) — and is discarded when the handle is dropped. The
    /// caller is responsible for writing `batch`; until it does, nothing this
    /// block did is on disk.
    pub fn stage_into_batch(&self, batch: &mut DbBatch<'_>) -> Result<(), DbError> {
        let Some(s) = self.stage_guard() else {
            return Ok(());
        };
        let g = read_stage(s);
        for (idx, map) in g.cfs.iter().enumerate() {
            let cf = ALL_CFS[idx];
            for (k, v) in map.iter() {
                match v {
                    Some(bytes) => batch.put(cf, k, bytes)?,
                    None => batch.delete(cf, k)?,
                }
            }
        }
        Ok(())
    }

    /// [`Database::stage_into_batch`], feeding every staged write to a
    /// committed column family into `hasher` on the way, in `(cf, key)` order.
    /// The stage keeps one `BTreeMap` per column family, indexed by position in
    /// [`ALL_CFS`], so walking them in index order is already that order.
    ///
    /// `skip_cf` names one column family whose staged writes are left out of
    /// the hash, for a caller that hashes that family itself.
    pub fn stage_into_batch_hashed(
        &self,
        batch: &mut DbBatch<'_>,
        hasher: &mut crate::state_hash::DiffHasher,
        skip_cf: Option<usize>,
    ) -> Result<(), DbError> {
        let Some(s) = self.stage_guard() else {
            return Ok(());
        };
        let g = read_stage(s);
        for (idx, map) in g.cfs.iter().enumerate() {
            let cf = ALL_CFS[idx];
            let hashed = crate::state_hash::is_committed(idx) && Some(idx) != skip_cf;
            for (k, v) in map.iter() {
                if hashed {
                    hasher.entry(idx, k, v.as_deref());
                }
                match v {
                    Some(bytes) => batch.put(cf, k, bytes)?,
                    None => batch.delete(cf, k)?,
                }
            }
        }
        Ok(())
    }

    /// Move every staged write out of this handle's stage, one map per column
    /// family in [`ALL_CFS`] order, leaving the stage empty. No row is copied.
    pub fn take_stage_maps(&self) -> Vec<BTreeMap<Vec<u8>, Option<Vec<u8>>>> {
        let Some(s) = self.stage_guard() else {
            return (0..ALL_CFS.len()).map(|_| BTreeMap::new()).collect();
        };
        let mut g = write_stage(s);
        g.journal.clear();
        std::mem::replace(
            &mut g.cfs,
            (0..ALL_CFS.len()).map(|_| BTreeMap::new()).collect(),
        )
    }

    /// Put every row of `cfs` into `batch`.
    pub fn maps_into_batch(
        batch: &mut DbBatch<'_>,
        cfs: &[BTreeMap<Vec<u8>, Option<Vec<u8>>>],
    ) -> Result<(), DbError> {
        for (idx, map) in cfs.iter().enumerate() {
            let cf = ALL_CFS[idx];
            for (k, v) in map {
                match v {
                    Some(bytes) => batch.put(cf, k, bytes)?,
                    None => batch.delete(cf, k)?,
                }
            }
        }
        Ok(())
    }

    /// Staged writes to `cf`, in key order: `Some` a put, `None` a delete.
    pub fn staged_entries(&self, cf: &str) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
        let (Some(s), Some(idx)) = (self.stage_guard(), cf_index(cf)) else {
            return Vec::new();
        };
        read_stage(s).cfs[idx]
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub fn put(&self, cf: &str, key: &[u8], value: &[u8]) -> Result<(), DbError> {
        if let Some(s) = self.stage_guard() {
            let idx = cf_index(cf).ok_or_else(|| DbError::MissingCf(cf.into()))?;
            write_stage(s).write(idx, key, Some(value.to_vec()));
            return Ok(());
        }
        let h = self
            .db
            .cf_handle(cf)
            .ok_or_else(|| DbError::MissingCf(cf.into()))?;
        self.db.put_cf(&h, key, value).map_err(Into::into)
    }

    pub fn get(&self, cf: &str, key: &[u8]) -> Result<Option<Vec<u8>>, DbError> {
        let idx = cf_index(cf).ok_or_else(|| DbError::MissingCf(cf.into()))?;
        if let Some(s) = self.stage_guard() {
            if let Some(v) = read_stage(s).get(idx, key) {
                return Ok(v);
            }
        }
        // Pending before RocksDB, always — see `PendingWrites`.
        if self.sees_pending {
            if let Some(v) = self.pending.get(idx, key) {
                return Ok(v);
            }
        }
        let h = self
            .db
            .cf_handle(cf)
            .ok_or_else(|| DbError::MissingCf(cf.into()))?;
        Ok(self.db.get_cf(&h, key)?)
    }

    /// Batched read against one CF. Wraps `multi_get_cf` so trie prefetch
    /// (M12) can pull a whole BFS frontier in a single rocksdb call instead
    /// of N independent `get_cf`s. Output preserves input order with `None`
    /// for missing keys; an error on any individual lookup propagates.
    pub fn multi_get(&self, cf: &str, keys: &[&[u8]]) -> Result<Vec<Option<Vec<u8>>>, DbError> {
        let idx = cf_index(cf).ok_or_else(|| DbError::MissingCf(cf.into()))?;
        let mut out: Vec<Option<Vec<u8>>> = vec![None; keys.len()];
        let mut miss_idx: Vec<usize> = Vec::new();
        let mut miss_keys: Vec<&[u8]> = Vec::new();
        // Stage first, then the pending overlays, then one rocksdb round trip
        // for whatever neither had.
        {
            let stage = self.stage_guard().map(read_stage);
            let overlays = if self.sees_pending {
                self.pending.overlays_snapshot()
            } else {
                Vec::new()
            };
            'keys: for (i, k) in keys.iter().enumerate() {
                if let Some(g) = &stage {
                    if let Some(v) = g.get(idx, k) {
                        out[i] = v;
                        continue;
                    }
                }
                for o in overlays.iter().rev() {
                    if let Some(v) = o.cfs[idx].get(*k) {
                        out[i] = v.clone();
                        continue 'keys;
                    }
                }
                miss_idx.push(i);
                miss_keys.push(k);
            }
        }
        if !miss_keys.is_empty() {
            let h = self
                .db
                .cf_handle(cf)
                .ok_or_else(|| DbError::MissingCf(cf.into()))?;
            let results = self.db.multi_get_cf(miss_keys.iter().map(|k| (&h, *k)));
            for (i, r) in miss_idx.into_iter().zip(results) {
                out[i] = r?;
            }
        }
        Ok(out)
    }

    pub fn delete(&self, cf: &str, key: &[u8]) -> Result<(), DbError> {
        if let Some(s) = self.stage_guard() {
            let idx = cf_index(cf).ok_or_else(|| DbError::MissingCf(cf.into()))?;
            write_stage(s).write(idx, key, None);
            return Ok(());
        }
        let h = self
            .db
            .cf_handle(cf)
            .ok_or_else(|| DbError::MissingCf(cf.into()))?;
        self.db.delete_cf(&h, key).map_err(Into::into)
    }

    /// Commit a `DbBatch` atomically. All writes hit RocksDB in one operation, which is
    /// dramatically cheaper than N individual `put`/`delete` calls when applying a block.
    ///
    /// This is the door to disk, and it deliberately ignores the stage: a
    /// batch is already atomic, so there is nothing for the stage to add. That
    /// also means calling it on a staged handle would silently escape the
    /// block's transaction, which is the defect this whole mechanism exists to
    /// close — so it is a debug assertion rather than a comment. Block commit
    /// drains the stage into a batch and writes it through the *direct*
    /// handle.
    pub fn write_batch(&self, batch: DbBatch) -> Result<(), DbError> {
        self.write_batch_then(batch, || {})
    }

    /// Write `batch`, and run `on_durable` once it is on disk.
    ///
    /// In the default synchronous mode that is before this returns. When
    /// asynchronous writes are on (see [`Database::enable_async_writes`]) the
    /// batch's rows become visible to every read on every handle immediately,
    /// through an overlay, and `on_durable` runs on the writer thread after
    /// RocksDB has taken them. Anything that tells the outside world about a
    /// block belongs in `on_durable`; anything that only needs to read the
    /// block's effects does not.
    pub fn write_batch_then<F>(&self, batch: DbBatch, on_durable: F) -> Result<(), DbError>
    where
        F: FnOnce() + Send + 'static,
    {
        debug_assert!(
            !self.is_staged(),
            "write_batch on a staged handle would bypass the block's write stage"
        );
        if let Some(e) = self
            .pending
            .error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Err(DbError::Storage(format!(
                "an earlier asynchronous write failed; refusing further writes: {e}"
            )));
        }
        // A write from the writer thread goes straight to disk: see
        // `PendingWrites::writer_thread`.
        if self.pending.on_writer_thread() {
            let mut wb = WriteBatch::default();
            for (idx, map) in batch.shadow.iter().enumerate() {
                if map.is_empty() {
                    continue;
                }
                let h = self
                    .db
                    .cf_handle(ALL_CFS[idx])
                    .ok_or_else(|| DbError::MissingCf(ALL_CFS[idx].into()))?;
                for (k, v) in map {
                    match v {
                        Some(bytes) => wb.put_cf(&h, k, bytes),
                        None => wb.delete_cf(&h, k),
                    }
                }
            }
            self.db.write(wb)?;
            on_durable();
            return Ok(());
        }
        match self.pending.mode() {
            WriteMode::Sync if self.pending.queue_is_empty() => {
                self.db.write(batch.inner)?;
                on_durable();
                Ok(())
            }
            WriteMode::Sync => {
                // A deferred block is waiting for its seal: this write
                // lands after it, in order, when it does.
                let seq = self
                    .pending
                    .next_seq
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.enqueue(seq, batch.shadow, None, Some(Box::new(on_durable)));
                self.drain_ready();
                Ok(())
            }
            _mode => {
                let seq = self
                    .pending
                    .next_seq
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.enqueue(seq, batch.shadow, None, Some(Box::new(on_durable)));
                Ok(())
            }
        }
    }

    /// Queue one block's rows for the writer: wait for room, publish them as
    /// an overlay, then queue the write. Shared by plain and sealed writes.
    fn enqueue(
        &self,
        seq: u64,
        cfs: Vec<BTreeMap<Vec<u8>, Option<Vec<u8>>>>,
        seal: Option<SealFn>,
        on_durable: Option<Box<dyn FnOnce() + Send>>,
    ) {
        self.enqueue_with(seq, cfs, seal, on_durable, None)
    }

    fn enqueue_with(
        &self,
        seq: u64,
        cfs: Vec<BTreeMap<Vec<u8>, Option<Vec<u8>>>>,
        seal: Option<SealFn>,
        on_durable: Option<Box<dyn FnOnce() + Send>>,
        deferred: Option<Arc<DeferredSlot>>,
    ) {
        // Depth is bounded only when something is draining on its own; a
        // test in manual mode drains when it chooses to. The bound is on
        // overlays, not queue entries: a batch leaves the queue before its
        // write and its overlay retires after, and it is the overlay that is
        // "a block in flight".
        if self.pending.mode() == WriteMode::Threaded {
            let mut q = self.pending.queue.lock().unwrap_or_else(|e| e.into_inner());
            while self.pending_depth() >= MAX_PENDING {
                q = self
                    .pending
                    .changed
                    .wait(q)
                    .unwrap_or_else(|e| e.into_inner());
            }
            drop(q);
        }
        // Overlay first, then queue: a reader that sees the queue entry
        // drained must already have been able to see the rows.
        let overlay = Arc::new(Overlay { seq, cfs });
        self.pending
            .overlays
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::clone(&overlay));
        self.pending
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(QueuedWrite {
                seq,
                overlay,
                seal,
                on_durable,
                deferred,
            });
        self.pending.changed.notify_all();
    }

    /// Hand one block's rows over and have them sealed and written.
    ///
    /// This is the leader's commit when writes are asynchronous: the caller —
    /// the thread that matches orders — only moves the rows it already has into
    /// an overlay, where every later read sees them at once, and goes on. The
    /// writer thread then runs `seal` over those rows (the diff hash, the
    /// header, the signature, the block encoding), adds what it returns to the
    /// same RocksDB batch, writes it, and runs [`Sealed::after`]. Synchronously,
    /// the same happens inline before this returns.
    pub fn write_sealed(
        &self,
        cfs: Vec<BTreeMap<Vec<u8>, Option<Vec<u8>>>>,
        seal: SealFn,
    ) -> Result<(), DbError> {
        if let Some(e) = self
            .pending
            .error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Err(DbError::Storage(format!(
                "an earlier asynchronous write failed; refusing further writes: {e}"
            )));
        }
        if self.pending.on_writer_thread()
            || (self.pending.mode() == WriteMode::Sync && self.pending.queue_is_empty())
        {
            let sealed = seal(&cfs).map_err(DbError::Storage)?;
            let wb = Self::write_batch_from(&self.db, &cfs, &sealed.extra)?;
            self.db.write(wb)?;
            (sealed.after)();
            return Ok(());
        }
        let seq = self
            .pending
            .next_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.enqueue(seq, cfs, Some(seal), None);
        if self.pending.mode() == WriteMode::Sync {
            self.drain_ready();
        }
        Ok(())
    }

    /// Hand one block's rows over before its seal is known. Every read sees
    /// them at once, as with [`Database::write_sealed`]; `reached` runs when
    /// everything queued before them is on disk; and they are written — or
    /// dropped — when the returned handle says how (see [`DeferredWrite`]).
    ///
    /// Works in every write mode. In synchronous mode the rows wait in an
    /// overlay, and any write issued meanwhile queues behind them instead
    /// of going straight to disk, so the order on disk is the order of the
    /// calls. Direct `put`s still go straight to disk: keep them off keys a
    /// deferred block writes.
    pub fn write_deferred(
        &self,
        cfs: Vec<BTreeMap<Vec<u8>, Option<Vec<u8>>>>,
        reached: ReachedFn,
    ) -> Result<DeferredWrite, DbError> {
        if let Some(e) = self
            .pending
            .error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Err(DbError::Storage(format!(
                "an earlier asynchronous write failed; refusing further writes: {e}"
            )));
        }
        debug_assert!(
            !self.pending.on_writer_thread(),
            "a deferred write from the writer thread would wait for itself"
        );
        let slot = Arc::new(DeferredSlot {
            reached: Mutex::new(Some(reached)),
            resolution: Mutex::new(None),
        });
        let seq = self
            .pending
            .next_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.enqueue_with(seq, cfs, None, None, Some(Arc::clone(&slot)));
        if self.pending.mode() == WriteMode::Sync {
            self.drain_ready();
        }
        Ok(DeferredWrite {
            db: self.clone(),
            slot,
            resolved: false,
        })
    }

    /// Synchronous mode: write whatever the queue lets through now.
    fn drain_ready(&self) {
        while Self::drain_one_inner(&self.db, &self.pending) {}
    }

    /// A RocksDB batch of `cfs` plus `extra` puts.
    fn write_batch_from(
        db: &DB,
        cfs: &[BTreeMap<Vec<u8>, Option<Vec<u8>>>],
        extra: &[(usize, Vec<u8>, Vec<u8>)],
    ) -> Result<WriteBatch, DbError> {
        let mut wb = WriteBatch::default();
        for (idx, map) in cfs.iter().enumerate() {
            if map.is_empty() {
                continue;
            }
            let h = db
                .cf_handle(ALL_CFS[idx])
                .ok_or_else(|| DbError::MissingCf(ALL_CFS[idx].into()))?;
            for (k, v) in map {
                match v {
                    Some(bytes) => wb.put_cf(&h, k, bytes),
                    None => wb.delete_cf(&h, k),
                }
            }
        }
        for (idx, k, v) in extra {
            let h = db
                .cf_handle(ALL_CFS[*idx])
                .ok_or_else(|| DbError::MissingCf(ALL_CFS[*idx].into()))?;
            wb.put_cf(&h, k, v);
        }
        Ok(wb)
    }

    /// Switch this database (every handle on it) to asynchronous writes: a
    /// background thread takes each committed batch to RocksDB while the
    /// caller goes on. See [`PendingWrites`] for what that changes and what
    /// it does not. A sequencer turns this on; replicas, tests and tools keep
    /// the synchronous default.
    pub fn enable_async_writes(&self) -> Result<(), DbError> {
        {
            let mut m = self.pending.mode.lock().unwrap_or_else(|e| e.into_inner());
            if *m == WriteMode::Threaded {
                return Ok(());
            }
            *m = WriteMode::Threaded;
        }
        let db = Arc::clone(&self.db);
        let pending = Arc::clone(&self.pending);
        std::thread::Builder::new()
            .name("cb-db-writer".to_string())
            .spawn(move || {
                *pending
                    .writer_thread
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(std::thread::current().id());
                loop {
                    {
                        let mut q = pending.queue.lock().unwrap_or_else(|e| e.into_inner());
                        while q.front().is_none_or(|w| w.blocked()) {
                            q = pending.changed.wait(q).unwrap_or_else(|e| e.into_inner());
                        }
                    }
                    Self::drain_one_inner(&db, &pending);
                }
            })
            .map_err(|e| DbError::Storage(format!("spawn db writer: {e}")))?;
        Ok(())
    }

    /// Queue writes without a writer thread, for tests that want to hold a
    /// block pending and look at the merged view. Drain with
    /// [`Database::drain_one`].
    pub fn enable_manual_writes(&self) {
        *self.pending.mode.lock().unwrap_or_else(|e| e.into_inner()) = WriteMode::Manual;
    }

    /// Write the oldest queued batch to RocksDB and retire its overlay.
    /// Returns whether there was one.
    pub fn drain_one(&self) -> bool {
        Self::drain_one_inner(&self.db, &self.pending)
    }

    fn drain_one_inner(db: &Arc<DB>, pending: &Arc<PendingWrites>) -> bool {
        let _drain = pending.drain.lock().unwrap_or_else(|e| e.into_inner());
        let qw = {
            let mut q = pending.queue.lock().unwrap_or_else(|e| e.into_inner());
            let Some(front) = q.front() else {
                return false;
            };
            if let Some(slot) = front.deferred.clone() {
                // Everything before it is on disk: tell its owner, outside
                // the queue lock (it may hash the whole block).
                // A block already discarded is not reported as reached.
                let hook = slot
                    .reached
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                let discarded = matches!(
                    *slot.resolution.lock().unwrap_or_else(|e| e.into_inner()),
                    Some(Resolution::Discard)
                );
                if let Some(hook) = hook.filter(|_| !discarded) {
                    let overlay = Arc::clone(&front.overlay);
                    drop(q);
                    hook(&overlay.cfs);
                    q = pending.queue.lock().unwrap_or_else(|e| e.into_inner());
                }
                if slot
                    .resolution
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_none()
                {
                    // Not resolved yet: the writer sleeps until it is.
                    drop(q);
                    pending.changed.notify_all();
                    return false;
                }
            }
            q.pop_front().expect("the front is still there")
        };
        let mut seal = qw.seal;
        if let Some(slot) = &qw.deferred {
            match slot
                .resolution
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                Some(Resolution::Seal(f)) => seal = Some(f),
                _ => {
                    // Discarded: retire the overlay, write nothing.
                    pending
                        .overlays
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .retain(|o| o.seq != qw.seq);
                    pending.changed.notify_all();
                    return true;
                }
            }
        }
        // Seal (when asked) and rebuild the RocksDB batch from the overlay's
        // maps here, on the writer, so the committing thread never hashes,
        // encodes or touches RocksDB at all.
        let mut failed: Option<String> = None;
        let mut on_durable = qw.on_durable;
        let extra = match seal {
            None => Vec::new(),
            Some(seal) => match seal(&qw.overlay.cfs) {
                Ok(sealed) => {
                    on_durable = Some(sealed.after);
                    sealed.extra
                }
                Err(e) => {
                    failed = Some(format!("sealing block failed: {e}"));
                    Vec::new()
                }
            },
        };
        if failed.is_none() {
            match Self::write_batch_from(db, &qw.overlay.cfs, &extra) {
                Ok(wb) => {
                    if let Err(e) = db.write(wb) {
                        failed = Some(e.to_string());
                    }
                }
                Err(e) => failed = Some(e.to_string()),
            }
        }
        // Written (or failed) — now retire the overlay. Order matters: RocksDB
        // has the rows before readers stop finding them in the overlay.
        {
            let mut g = pending.overlays.write().unwrap_or_else(|e| e.into_inner());
            g.retain(|o| o.seq != qw.seq);
        }
        match failed {
            None => {
                if let Some(f) = on_durable {
                    f();
                }
            }
            Some(e) => {
                // No logger in this crate; the error is stored and surfaces
                // from the next `write_batch` and from `flush_pending`.
                eprintln!(
                    "asynchronous RocksDB write failed; the node will refuse further writes: {e}"
                );
                let mut slot = pending.error.lock().unwrap_or_else(|e| e.into_inner());
                if slot.is_none() {
                    *slot = Some(e);
                }
            }
        }
        pending.changed.notify_all();
        true
    }

    /// Wait until every queued write is on disk. In manual mode this drains
    /// them itself. Returns the first write error, if one happened.
    pub fn flush_pending(&self) -> Result<(), DbError> {
        match self.pending.mode() {
            WriteMode::Sync | WriteMode::Manual => self.drain_ready(),
            WriteMode::Threaded => {
                // Everything, or everything ahead of a deferred block still
                // waiting for its seal (whose rows, and those behind it,
                // are then the only overlays left).
                let mut q = self.pending.queue.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    let overlays = self
                        .pending
                        .overlays
                        .read()
                        .unwrap_or_else(|e| e.into_inner())
                        .len();
                    let settled = match q.front() {
                        None => overlays == 0,
                        Some(w) => w.blocked() && overlays == q.len(),
                    };
                    if settled {
                        break;
                    }
                    q = self
                        .pending
                        .changed
                        .wait_timeout(q, std::time::Duration::from_millis(5))
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
            }
        }
        match self
            .pending
            .error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            Some(e) => Err(DbError::Storage(e)),
            None => Ok(()),
        }
    }

    /// Blocks committed but not yet written.
    pub fn pending_depth(&self) -> usize {
        self.pending
            .overlays
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    fn writes_are_async(&self) -> bool {
        self.pending.mode() != WriteMode::Sync
    }

    pub fn batch(&self) -> DbBatch<'_> {
        DbBatch {
            db: self,
            inner: WriteBatch::default(),
            shadow: (0..ALL_CFS.len()).map(|_| BTreeMap::new()).collect(),
        }
    }

    /// Iterate keys in `cf` starting from `prefix` (forward direction).
    ///
    /// On a staged handle the RocksDB iterator is merged with the block's
    /// buffered writes: a staged put shadows the stored row, a staged delete
    /// hides it, and staged keys that do not exist on disk appear in their
    /// sorted position. The stage's contribution is snapshotted when the
    /// iterator is built, which matches RocksDB's own implicit-snapshot
    /// semantics — a write made *during* iteration is invisible either way —
    /// and means iteration never holds the stage lock.
    pub fn iter_prefix<'a>(&'a self, cf: &str, prefix: &'a [u8]) -> Result<CfIter<'a>, DbError> {
        // Overlay snapshot first, RocksDB iterator second. A pending write
        // that lands between the two is then in the snapshot, in the
        // iterator, or both; `MergedIter` resolves both to one row.
        let overlay = self.overlay_range(cf, prefix, true)?;
        let h = self
            .db
            .cf_handle(cf)
            .ok_or_else(|| DbError::MissingCf(cf.into()))?;
        let base = self.db.iterator_cf(
            &h,
            rocksdb::IteratorMode::From(prefix, rocksdb::Direction::Forward),
        );
        match overlay {
            None => Ok(Box::new(base)),
            Some(overlay) => Ok(Box::new(MergedIter::new(base, overlay, true))),
        }
    }

    /// Snapshot everything not yet in RocksDB that an iterator seeded at
    /// `from` would have to consider — this handle's stage and every pending
    /// overlay — merged into one ordered list with the newest write winning
    /// per key. `None` means nothing is overlaid in this CF, so the caller can
    /// use the raw RocksDB iterator untouched.
    fn overlay_range(
        &self,
        cf: &str,
        from: &[u8],
        forward: bool,
    ) -> Result<Option<Vec<OverlayRow>>, DbError> {
        let idx = cf_index(cf).ok_or_else(|| DbError::MissingCf(cf.into()))?;
        let overlays = if self.sees_pending {
            self.pending.overlays_snapshot()
        } else {
            Vec::new()
        };
        let mut merged: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
        let mut any = false;
        // Oldest pending first, then this handle's stage: later inserts
        // overwrite, so the newest write is what survives.
        for o in overlays.iter() {
            let map = &o.cfs[idx];
            if map.is_empty() {
                continue;
            }
            any = true;
            let it: OverlayIter<'_> = if forward {
                Box::new(map.range(from.to_vec()..))
            } else {
                Box::new(map.range(..=from.to_vec()))
            };
            for (k, v) in it {
                merged.insert(k.clone(), v.clone());
            }
        }
        if let Some(s) = self.stage_guard() {
            let g = read_stage(s);
            let map = &g.cfs[idx];
            if !map.is_empty() {
                any = true;
                let it: OverlayIter<'_> = if forward {
                    Box::new(map.range(from.to_vec()..))
                } else {
                    Box::new(map.range(..=from.to_vec()))
                };
                for (k, v) in it {
                    merged.insert(k.clone(), v.clone());
                }
            }
        }
        if !any {
            return Ok(None);
        }
        let out: Vec<(Vec<u8>, Option<Vec<u8>>)> = if forward {
            merged.into_iter().collect()
        } else {
            merged.into_iter().rev().collect()
        };
        Ok(Some(out))
    }

    pub fn iter_prefix_reverse<'a>(
        &'a self,
        cf: &str,
        seek_at_or_before: &'a [u8],
    ) -> Result<CfIter<'a>, DbError> {
        let overlay = self.overlay_range(cf, seek_at_or_before, false)?;
        let h = self
            .db
            .cf_handle(cf)
            .ok_or_else(|| DbError::MissingCf(cf.into()))?;
        let base = self.db.iterator_cf(
            &h,
            rocksdb::IteratorMode::From(seek_at_or_before, rocksdb::Direction::Reverse),
        );
        match overlay {
            None => Ok(Box::new(base)),
            Some(overlay) => Ok(Box::new(MergedIter::new(base, overlay, false))),
        }
    }
}

/// What every CF iterator yields. Identical to RocksDB's own item type so a
/// staged and a direct handle are interchangeable at the call site.
pub type CfItem = Result<(Box<[u8]>, Box<[u8]>), rocksdb::Error>;

/// A CF iterator. Boxed because a staged handle merges the stage in and a
/// direct one does not.
pub type CfIter<'a> = Box<dyn Iterator<Item = CfItem> + 'a>;

/// Merges a RocksDB iterator with a snapshot of the stage, preserving key
/// order and letting the stage win on ties. Tombstones (`None`) suppress the
/// stored row instead of being yielded.
struct MergedIter<'a> {
    base: std::iter::Peekable<rocksdb::DBIteratorWithThreadMode<'a, DB>>,
    overlay: std::vec::IntoIter<(Vec<u8>, Option<Vec<u8>>)>,
    pending: Option<(Vec<u8>, Option<Vec<u8>>)>,
    forward: bool,
    done: bool,
}

impl<'a> MergedIter<'a> {
    fn new(
        base: rocksdb::DBIteratorWithThreadMode<'a, DB>,
        overlay: Vec<(Vec<u8>, Option<Vec<u8>>)>,
        forward: bool,
    ) -> Self {
        let mut overlay = overlay.into_iter();
        let pending = overlay.next();
        Self {
            base: base.peekable(),
            overlay,
            pending,
            forward,
            done: false,
        }
    }
}

impl<'a> Iterator for MergedIter<'a> {
    type Item = CfItem;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.done {
                return None;
            }
            // Which side supplies the next key? `forward` is copied out first
            // so the peek borrow ends before the comparison.
            let forward = self.forward;
            let take_overlay = match (&self.pending, self.base.peek()) {
                (None, _) => false,
                (Some(_), None) => true,
                // Surface the rocksdb error rather than racing the overlay
                // against a key we cannot read.
                (Some(_), Some(Err(_))) => false,
                (Some((ok, _)), Some(Ok((bk, _)))) => {
                    let base_first = if forward {
                        bk.as_ref() < ok.as_slice()
                    } else {
                        bk.as_ref() > ok.as_slice()
                    };
                    !base_first
                }
            };

            if take_overlay {
                let (key, value) = self.pending.take().expect("checked above");
                self.pending = self.overlay.next();
                // A key present on both sides: drop the stored row, the stage
                // supersedes it.
                let shadowed =
                    matches!(self.base.peek(), Some(Ok((bk, _))) if bk.as_ref() == key.as_slice());
                if shadowed {
                    let _ = self.base.next();
                }
                match value {
                    Some(bytes) => {
                        return Some(Ok((key.into_boxed_slice(), bytes.into_boxed_slice())))
                    }
                    // Staged delete: skip it entirely.
                    None => continue,
                }
            }

            match self.base.next() {
                None => {
                    if self.pending.is_none() {
                        self.done = true;
                        return None;
                    }
                    continue;
                }
                Some(Err(e)) => {
                    self.done = true;
                    return Some(Err(e));
                }
                Some(Ok(kv)) => return Some(Ok(kv)),
            }
        }
    }
}

fn read_stage(s: &Arc<RwLock<Stage>>) -> std::sync::RwLockReadGuard<'_, Stage> {
    s.read().unwrap_or_else(|e| e.into_inner())
}

fn write_stage(s: &Arc<RwLock<Stage>>) -> std::sync::RwLockWriteGuard<'_, Stage> {
    s.write().unwrap_or_else(|e| e.into_inner())
}

/// Accumulates Put/Delete ops across CFs and commits them atomically. Uses RocksDB's
/// native `WriteBatch` so the cost of a per-block flush is one RocksDB call regardless
/// of how many ops it contains.
pub struct DbBatch<'a> {
    db: &'a Database,
    inner: WriteBatch,
    /// The same writes as `inner`, per column family, for the overlay a
    /// pending write is served from. Filled always; `inner` is filled only
    /// when the database writes synchronously, since the writer thread
    /// rebuilds its own batch from these maps.
    shadow: Vec<BTreeMap<Vec<u8>, Option<Vec<u8>>>>,
}

impl<'a> DbBatch<'a> {
    pub fn put(&mut self, cf: &str, key: &[u8], value: &[u8]) -> Result<(), DbError> {
        let idx = cf_index(cf).ok_or_else(|| DbError::MissingCf(cf.into()))?;
        self.shadow[idx].insert(key.to_vec(), Some(value.to_vec()));
        if !self.db.writes_are_async() {
            let h = self
                .db
                .db
                .cf_handle(cf)
                .ok_or_else(|| DbError::MissingCf(cf.into()))?;
            self.inner.put_cf(&h, key, value);
        }
        Ok(())
    }

    pub fn delete(&mut self, cf: &str, key: &[u8]) -> Result<(), DbError> {
        let idx = cf_index(cf).ok_or_else(|| DbError::MissingCf(cf.into()))?;
        self.shadow[idx].insert(key.to_vec(), None);
        if !self.db.writes_are_async() {
            let h = self
                .db
                .db
                .cf_handle(cf)
                .ok_or_else(|| DbError::MissingCf(cf.into()))?;
            self.inner.delete_cf(&h, key);
        }
        Ok(())
    }
}

/// `KvStore` adapter that buffers all writes in memory and flushes them into a `DbBatch`
/// on `flush_into`. Reads fall through to the buffer first, then to RocksDB. Use for the
/// trie when applying a block so per-node writes don't each round-trip to RocksDB.
pub struct BufferedKv<'a> {
    db: &'a Database,
    cf: &'a str,
    cache: std::collections::HashMap<H256, Option<Vec<u8>>>,
}

impl<'a> BufferedKv<'a> {
    pub fn new(db: &'a Database, cf: &'a str) -> Self {
        Self {
            db,
            cf,
            cache: std::collections::HashMap::new(),
        }
    }

    /// Drain the buffer into the batch as Put / Delete ops.
    pub fn flush_into(self, batch: &mut DbBatch<'_>) -> Result<(), DbError> {
        for (k, v) in self.cache {
            match v {
                Some(bytes) => batch.put(self.cf, &k.0, &bytes)?,
                None => batch.delete(self.cf, &k.0)?,
            }
        }
        Ok(())
    }
}

impl<'a> KvStore for BufferedKv<'a> {
    fn get(&self, key: &H256) -> Result<Option<Vec<u8>>, TrieError> {
        if let Some(v) = self.cache.get(key) {
            return Ok(v.clone());
        }
        self.db
            .get(self.cf, &key.0)
            .map_err(|e| TrieError::Storage(e.to_string()))
    }

    fn put(&mut self, key: H256, value: Vec<u8>) -> Result<(), TrieError> {
        self.cache.insert(key, Some(value));
        Ok(())
    }

    fn delete(&mut self, key: &H256) -> Result<(), TrieError> {
        self.cache.insert(*key, None);
        Ok(())
    }

    /// Batched read. Cache-first: any key already buffered is returned from
    /// memory and we only multi-get the residue against rocksdb. Order is
    /// preserved via index tracking.
    fn multi_get(&self, keys: &[H256]) -> Result<Vec<Option<Vec<u8>>>, TrieError> {
        let mut out: Vec<Option<Vec<u8>>> = vec![None; keys.len()];
        let mut miss_idx: Vec<usize> = Vec::new();
        let mut miss_keys: Vec<&[u8]> = Vec::new();
        for (i, k) in keys.iter().enumerate() {
            if let Some(v) = self.cache.get(k) {
                out[i] = v.clone();
            } else {
                miss_idx.push(i);
                miss_keys.push(&k.0);
            }
        }
        if miss_keys.is_empty() {
            return Ok(out);
        }
        let fetched = self
            .db
            .multi_get(self.cf, &miss_keys)
            .map_err(|e| TrieError::Storage(e.to_string()))?;
        for (i, v) in miss_idx.into_iter().zip(fetched) {
            out[i] = v;
        }
        Ok(out)
    }

    /// Pre-populate the in-memory cache via one batched rocksdb read. The
    /// trie's prefetch_paths walks each level's frontier through this hook.
    /// Keys already present in the cache (touched earlier in the same
    /// commit) are skipped — re-reading would clobber an in-flight Put.
    fn prefetch(&mut self, keys: &[H256]) -> Result<(), TrieError> {
        // Filter to keys we don't already have buffered.
        let mut needed_idx: Vec<usize> = Vec::new();
        let mut needed_keys: Vec<&[u8]> = Vec::new();
        for (i, k) in keys.iter().enumerate() {
            if !self.cache.contains_key(k) {
                needed_idx.push(i);
                needed_keys.push(&k.0);
            }
        }
        if needed_keys.is_empty() {
            return Ok(());
        }
        let fetched = self
            .db
            .multi_get(self.cf, &needed_keys)
            .map_err(|e| TrieError::Storage(e.to_string()))?;
        for (i, v) in needed_idx.into_iter().zip(fetched) {
            self.cache.insert(keys[i], v);
        }
        Ok(())
    }
}

/// `KvStore` adapter for trie nodes living in a chosen CF.
pub struct CfKv<'a> {
    db: &'a Database,
    cf: &'a str,
}

impl<'a> CfKv<'a> {
    pub fn new(db: &'a Database, cf: &'a str) -> Self {
        Self { db, cf }
    }
}

impl<'a> KvStore for CfKv<'a> {
    fn get(&self, key: &H256) -> Result<Option<Vec<u8>>, TrieError> {
        self.db
            .get(self.cf, &key.0)
            .map_err(|e| TrieError::Storage(e.to_string()))
    }

    fn put(&mut self, key: H256, value: Vec<u8>) -> Result<(), TrieError> {
        self.db
            .put(self.cf, &key.0, &value)
            .map_err(|e| TrieError::Storage(e.to_string()))
    }

    fn delete(&mut self, key: &H256) -> Result<(), TrieError> {
        self.db
            .delete(self.cf, &key.0)
            .map_err(|e| TrieError::Storage(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn open_and_put_get() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        db.put(CF_META, b"x", b"1").unwrap();
        assert_eq!(db.get(CF_META, b"x").unwrap().as_deref(), Some(&b"1"[..]));
    }

    #[test]
    fn cfkv_implements_kvstore() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let mut kv = CfKv::new(&db, CF_TRIE_NODES);
        let h = H256([7; 32]);
        kv.put(h, b"hello".to_vec()).unwrap();
        assert_eq!(kv.get(&h).unwrap().as_deref(), Some(&b"hello"[..]));
    }

    // ---------------------------------------------------------------
    // Pending writes: the overlay between "committed" and "on disk".
    // ---------------------------------------------------------------

    fn pending_db() -> (tempfile::TempDir, Database) {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        db.enable_manual_writes();
        (dir, db)
    }

    #[test]
    fn a_pending_write_is_visible_to_every_read_path_and_invisible_to_the_durable_view() {
        let (_d, db) = pending_db();
        // Two rows already on disk: direct puts bypass the queue.
        db.put(CF_ORDERS, b"aa", b"old").unwrap();
        db.put(CF_ORDERS, b"zz", b"stays").unwrap();

        let mut b = db.batch();
        b.put(CF_ORDERS, b"aa", b"new").unwrap(); // overwrite
        b.put(CF_ORDERS, b"mm", b"added").unwrap(); // insert
        b.delete(CF_ORDERS, b"zz").unwrap(); // delete
        db.write_batch(b).unwrap();
        assert_eq!(db.pending_depth(), 1);

        // Point reads.
        assert_eq!(
            db.get(CF_ORDERS, b"aa").unwrap().as_deref(),
            Some(&b"new"[..])
        );
        assert_eq!(
            db.get(CF_ORDERS, b"mm").unwrap().as_deref(),
            Some(&b"added"[..])
        );
        assert_eq!(db.get(CF_ORDERS, b"zz").unwrap(), None);
        // Batched reads.
        let got = db
            .multi_get(CF_ORDERS, &[b"aa", b"mm", b"zz", b"qq"])
            .unwrap();
        assert_eq!(
            got,
            vec![Some(b"new".to_vec()), Some(b"added".to_vec()), None, None]
        );
        // Forward and reverse scans, merged and in order, tombstone suppressed.
        let fwd: Vec<(Vec<u8>, Vec<u8>)> = db
            .iter_prefix(CF_ORDERS, b"")
            .unwrap()
            .map(|r| r.map(|(k, v)| (k.to_vec(), v.to_vec())).unwrap())
            .collect();
        assert_eq!(
            fwd,
            vec![
                (b"aa".to_vec(), b"new".to_vec()),
                (b"mm".to_vec(), b"added".to_vec())
            ]
        );
        let rev: Vec<Vec<u8>> = db
            .iter_prefix_reverse(CF_ORDERS, b"zzzz")
            .unwrap()
            .map(|r| r.map(|(k, _)| k.to_vec()).unwrap())
            .collect();
        assert_eq!(rev, vec![b"mm".to_vec(), b"aa".to_vec()]);

        // The durable view still sees the disk.
        let durable = db.durable_view();
        assert_eq!(
            durable.get(CF_ORDERS, b"aa").unwrap().as_deref(),
            Some(&b"old"[..])
        );
        assert_eq!(durable.get(CF_ORDERS, b"mm").unwrap(), None);
        assert_eq!(
            durable.get(CF_ORDERS, b"zz").unwrap().as_deref(),
            Some(&b"stays"[..])
        );

        // Drain: disk catches up, overlay retires, both views agree.
        assert!(db.drain_one());
        assert_eq!(db.pending_depth(), 0);
        assert!(!db.drain_one());
        for h in [&db, &durable] {
            assert_eq!(
                h.get(CF_ORDERS, b"aa").unwrap().as_deref(),
                Some(&b"new"[..])
            );
            assert_eq!(
                h.get(CF_ORDERS, b"mm").unwrap().as_deref(),
                Some(&b"added"[..])
            );
            assert_eq!(h.get(CF_ORDERS, b"zz").unwrap(), None);
        }
        db.flush_pending().unwrap();
    }

    #[test]
    fn newer_pending_writes_shadow_older_ones_and_the_stage_shadows_both() {
        let (_d, db) = pending_db();
        let mut b1 = db.batch();
        b1.put(CF_ORDERS, b"k", b"v1").unwrap();
        db.write_batch(b1).unwrap();
        let mut b2 = db.batch();
        b2.put(CF_ORDERS, b"k", b"v2").unwrap();
        db.write_batch(b2).unwrap();
        assert_eq!(db.pending_depth(), 2);
        assert_eq!(
            db.get(CF_ORDERS, b"k").unwrap().as_deref(),
            Some(&b"v2"[..])
        );

        // A staged handle on top sees its own write first, then the newest
        // pending one, in scans as well as point reads.
        let staged = db.staged();
        staged.put(CF_ORDERS, b"k", b"v3").unwrap();
        assert_eq!(
            staged.get(CF_ORDERS, b"k").unwrap().as_deref(),
            Some(&b"v3"[..])
        );
        let scan: Vec<Vec<u8>> = staged
            .iter_prefix(CF_ORDERS, b"")
            .unwrap()
            .map(|r| r.map(|(_, v)| v.to_vec()).unwrap())
            .collect();
        assert_eq!(scan, vec![b"v3".to_vec()]);
        drop(staged);

        // Draining in order lands v1 then v2; readers never see v1 win.
        assert!(db.drain_one());
        assert_eq!(
            db.get(CF_ORDERS, b"k").unwrap().as_deref(),
            Some(&b"v2"[..])
        );
        assert!(db.drain_one());
        assert_eq!(
            db.get(CF_ORDERS, b"k").unwrap().as_deref(),
            Some(&b"v2"[..])
        );
        assert_eq!(
            db.durable_view().get(CF_ORDERS, b"k").unwrap().as_deref(),
            Some(&b"v2"[..])
        );
    }

    #[test]
    fn on_durable_runs_after_the_write_not_before() {
        let (_d, db) = pending_db();
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f = fired.clone();
        let mut b = db.batch();
        b.put(CF_META, b"x", b"y").unwrap();
        db.write_batch_then(b, move || {
            f.store(true, std::sync::atomic::Ordering::SeqCst);
        })
        .unwrap();
        assert!(
            !fired.load(std::sync::atomic::Ordering::SeqCst),
            "not durable yet"
        );
        assert!(db.drain_one());
        assert!(fired.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            db.durable_view().get(CF_META, b"x").unwrap().as_deref(),
            Some(&b"y"[..])
        );
    }

    /// A durability callback that writes must not deadlock: it runs on the
    /// writer thread, so its write cannot wait for that thread to drain.
    /// Pruning did exactly this and froze the node.
    #[test]
    fn a_write_from_a_durability_callback_does_not_deadlock() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        db.enable_async_writes().unwrap();
        // Fill the queue so a queued write would have to wait.
        let inner = db.clone();
        for i in 0..(MAX_PENDING as u8 * 2) {
            let mut b = db.batch();
            b.put(CF_ORDERS, &[i], &[i]).unwrap();
            let inner = inner.clone();
            db.write_batch_then(b, move || {
                // The shape of `prune_one`: a write of its own, from here.
                let mut p = inner.batch();
                p.delete(CF_BLOCKS, &[i]).unwrap();
                inner.write_batch(p).expect("callback write");
            })
            .unwrap();
        }
        db.flush_pending().unwrap();
        assert_eq!(db.pending_depth(), 0);
        let durable = db.durable_view();
        for i in 0..(MAX_PENDING as u8 * 2) {
            assert_eq!(
                durable.get(CF_ORDERS, &[i]).unwrap().as_deref(),
                Some(&[i][..])
            );
        }
    }

    #[test]
    fn the_threaded_writer_drains_and_flush_waits_for_it() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        db.enable_async_writes().unwrap();
        for i in 0..(MAX_PENDING as u8 * 3) {
            let mut b = db.batch();
            b.put(CF_ORDERS, &[i], &[i]).unwrap();
            db.write_batch(b).unwrap();
            assert!(db.pending_depth() <= MAX_PENDING);
        }
        db.flush_pending().unwrap();
        assert_eq!(db.pending_depth(), 0);
        let durable = db.durable_view();
        for i in 0..(MAX_PENDING as u8 * 3) {
            assert_eq!(
                durable.get(CF_ORDERS, &[i]).unwrap().as_deref(),
                Some(&[i][..])
            );
        }
    }

    // ---------------------------------------------------------------
    // Deferred writes: rows visible now, sealed or discarded later.
    // ---------------------------------------------------------------

    fn rows(key: &[u8], value: &[u8]) -> Vec<BTreeMap<Vec<u8>, Option<Vec<u8>>>> {
        let mut cfs: Vec<BTreeMap<Vec<u8>, Option<Vec<u8>>>> =
            (0..ALL_CFS.len()).map(|_| BTreeMap::new()).collect();
        cfs[cf_index(CF_ORDERS).unwrap()].insert(key.to_vec(), Some(value.to_vec()));
        cfs
    }

    /// A seal that adds one META row, `sealed` -> `tag`.
    fn seal_tagged(tag: &'static [u8]) -> SealFn {
        Box::new(move |_rows| {
            Ok(Sealed {
                extra: vec![(cf_index(CF_META).unwrap(), b"sealed".to_vec(), tag.to_vec())],
                after: Box::new(|| {}),
            })
        })
    }

    fn reached_counter() -> (Arc<std::sync::atomic::AtomicUsize>, ReachedFn) {
        let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let m = Arc::clone(&n);
        (
            n,
            Box::new(move |_rows| {
                m.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }),
        )
    }

    #[test]
    fn a_deferred_write_is_read_at_once_and_written_only_when_sealed() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap(); // synchronous mode
        let (reached, hook) = reached_counter();
        let d = db.write_deferred(rows(b"k", b"v"), hook).unwrap();
        assert_eq!(reached.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(db.get(CF_ORDERS, b"k").unwrap().as_deref(), Some(&b"v"[..]));
        assert_eq!(db.durable_view().get(CF_ORDERS, b"k").unwrap(), None);
        // A write issued meanwhile waits behind it, in order.
        let mut b = db.batch();
        b.put(CF_ORDERS, b"k", b"later").unwrap();
        db.write_batch(b).unwrap();
        assert_eq!(db.durable_view().get(CF_ORDERS, b"k").unwrap(), None);
        assert_eq!(
            db.get(CF_ORDERS, b"k").unwrap().as_deref(),
            Some(&b"later"[..])
        );
        db.flush_pending().unwrap(); // returns: the deferred write is unresolved
        d.seal(seal_tagged(b"one")).unwrap();
        let durable = db.durable_view();
        assert_eq!(
            durable.get(CF_ORDERS, b"k").unwrap().as_deref(),
            Some(&b"later"[..])
        );
        assert_eq!(
            durable.get(CF_META, b"sealed").unwrap().as_deref(),
            Some(&b"one"[..])
        );
        assert_eq!(db.pending_depth(), 0);
        assert_eq!(reached.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn a_discarded_deferred_write_leaves_no_trace_and_no_error() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        db.put(CF_ORDERS, b"k", b"before").unwrap();
        let (_, hook) = reached_counter();
        let d = db.write_deferred(rows(b"k", b"block"), hook).unwrap();
        let mut b = db.batch();
        b.put(CF_ORDERS, b"other", b"kept").unwrap();
        db.write_batch(b).unwrap();
        d.discard();
        assert_eq!(
            db.get(CF_ORDERS, b"k").unwrap().as_deref(),
            Some(&b"before"[..])
        );
        let durable = db.durable_view();
        assert_eq!(
            durable.get(CF_ORDERS, b"k").unwrap().as_deref(),
            Some(&b"before"[..])
        );
        assert_eq!(
            durable.get(CF_ORDERS, b"other").unwrap().as_deref(),
            Some(&b"kept"[..])
        );
        assert_eq!(db.pending_depth(), 0);
        // Dropping the handle discards too, and the database still writes.
        let (_, hook) = reached_counter();
        drop(db.write_deferred(rows(b"k", b"dropped"), hook).unwrap());
        assert_eq!(
            db.get(CF_ORDERS, b"k").unwrap().as_deref(),
            Some(&b"before"[..])
        );
        let mut b = db.batch();
        b.put(CF_ORDERS, b"after", b"ok").unwrap();
        db.write_batch(b).unwrap();
        assert_eq!(
            db.durable_view()
                .get(CF_ORDERS, b"after")
                .unwrap()
                .as_deref(),
            Some(&b"ok"[..])
        );
    }

    #[test]
    fn the_threaded_writer_waits_for_a_deferred_seal_and_flush_does_not() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        db.enable_async_writes().unwrap();
        let mut b = db.batch();
        b.put(CF_ORDERS, b"first", b"1").unwrap();
        db.write_batch(b).unwrap();
        let (reached, hook) = reached_counter();
        let d = db.write_deferred(rows(b"k", b"v"), hook).unwrap();
        let mut b = db.batch();
        b.put(CF_ORDERS, b"behind", b"2").unwrap();
        db.write_batch(b).unwrap();
        // Flush returns once everything ahead of the deferred block is on
        // disk; it and what is behind it wait.
        db.flush_pending().unwrap();
        let durable = db.durable_view();
        assert_eq!(
            durable.get(CF_ORDERS, b"first").unwrap().as_deref(),
            Some(&b"1"[..])
        );
        assert_eq!(durable.get(CF_ORDERS, b"k").unwrap(), None);
        assert_eq!(durable.get(CF_ORDERS, b"behind").unwrap(), None);
        assert_eq!(reached.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            db.get(CF_ORDERS, b"behind").unwrap().as_deref(),
            Some(&b"2"[..])
        );
        d.seal(seal_tagged(b"t")).unwrap();
        db.flush_pending().unwrap();
        assert_eq!(db.pending_depth(), 0);
        assert_eq!(
            durable.get(CF_ORDERS, b"k").unwrap().as_deref(),
            Some(&b"v"[..])
        );
        assert_eq!(
            durable.get(CF_ORDERS, b"behind").unwrap().as_deref(),
            Some(&b"2"[..])
        );
        assert_eq!(
            durable.get(CF_META, b"sealed").unwrap().as_deref(),
            Some(&b"t"[..])
        );
        // And a discard on the writer thread.
        let (_, hook) = reached_counter();
        let d = db.write_deferred(rows(b"gone", b"x"), hook).unwrap();
        d.discard();
        db.flush_pending().unwrap();
        assert_eq!(db.pending_depth(), 0);
        assert_eq!(db.get(CF_ORDERS, b"gone").unwrap(), None);
        assert_eq!(durable.get(CF_ORDERS, b"gone").unwrap(), None);
    }
}
