//! Per-block in-memory account state. Reads cache through to RocksDB on first access;
//! writes stay in memory until [`BlockState::commit`] flushes everything — accounts CF,
//! trie nodes, and the new accounts_root — in a single atomic RocksDB WriteBatch.
//!
//! This is the key throughput change: before, every Transfer triggered ~14 RocksDB
//! writes (CF_ACCOUNTS × 2, trie nodes ×N, META × 2). With BlockState, a 10K-tx block
//! collapses to ~16 changed accounts → one batched commit.
//!
//! M8: BlockState gained two more buffered "touched-keys" sets — positions and markets
//! — that the executor uses to record CF writes/deletes during the tx loop. On
//! `commit`, those keys feed the positions and markets tries (re-reading the CF for
//! the current value of each). All three trie commits — accounts, positions, markets
//! — land in **one** RocksDB `WriteBatch` with the per-trie meta roots, so a crash
//! after the tx loop can never leave the chain with a partial trie state.
//!
//! # A block's mutation is one write, or none
//!
//! The M8 note above was only ever half true. Balances were buffered here, but
//! everything else — order rows, book index, positions, margin, oracle state,
//! market records — was written *straight to RocksDB* by the executor so that a
//! later transaction in the same block could read it back. That requirement is
//! real; satisfying it that way was not. When a transaction later in the block
//! failed, the buffered balances were discarded and those direct writes were
//! not, which left orders resting on the book with no collateral behind them.
//!
//! `BlockState` now runs the whole block on a **staged** `Database`
//! (`Database::staged`). Writes are buffered in memory and reads see the
//! buffer first, so "a later tx sees what an earlier one wrote" still holds
//! exactly as before — but nothing reaches disk until [`BlockState::commit_into`]
//! folds the stage, all four tries, and (via the caller) the block bytes and
//! the head pointer into **one** `WriteBatch`.
//!
//! On top of that, [`BlockState::savepoint`] / [`BlockState::rollback`] give
//! the block builder per-transaction granularity: a transaction that fails
//! leaves no trace in the stage, in the account map, or in the touched-key
//! sets, so the block's state root is exactly the root of the block without
//! it.

use cb_engine::spot_book::{RestingOrder, SpotBook};
use cb_state::{
    chain_state_hash, diff_hash, AccountState, Accounts, BookIndex, CfMap, Database, DbBatch,
    DbError, MarketConfig, Markets, Meta, Orders, ALL_CFS, CF_ACCOUNTS,
};
use cb_types::{Address, DerivId, MarketId, H256};
use rayon::prelude::*;
// Fx hashing for the per-block maps: keys are addresses and ids the node
// already trusts, iteration order never reaches consensus (the diff is
// sorted), and SipHash was a measurable share of the block loop.
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

/// What committing a block produced. Returned by [`BlockState::commit`] and
/// [`BlockState::commit_into`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitRoots {
    /// The chain's state root after this block: the previous root chained with
    /// the hash of every committed key this block changed (`cb_state::state_hash`).
    pub state_root: H256,
    /// How many changed entries the block's diff hashed.
    pub diff_entries: u64,
}

/// A point a block's mutation can be rewound to. Returned by
/// [`BlockState::savepoint`], consumed by [`BlockState::rollback`].
///
/// Everything a transaction can change is covered: the staged CF writes, the
/// account map (including entries a read merely *cached*, because `commit`
/// writes every entry in that map and a cached row is a trie leaf), and the
/// three touched-key sets.
#[derive(Clone, Copy, Debug)]
pub struct Savepoint {
    stage: usize,
    accounts: usize,
    positions: usize,
    markets: usize,
    margin: usize,
    books: usize,
}

/// What one block hands the next through the database's derived-state slot:
/// the order books and every account row it has held in memory, each equal to
/// what the committed batch left on disk. Taken whole by the next block when
/// it builds on the stamped height; dropped otherwise.
#[derive(Default)]
struct Carried {
    spot_books: HashMap<MarketId, SpotBook>,
    accounts: HashMap<Address, AccountState>,
}

/// What [`preload_state`] put in memory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PreloadStats {
    pub accounts: usize,
    pub markets: usize,
    pub resting_orders: usize,
}

/// Load every account row and every spot market's resting book into the
/// memory the next block starts from, so that no account or book is read from
/// RocksDB on the block path afterwards.
///
/// This is the startup load of ADR 0007: the sequencer calls it once before its
/// first block, and `cb-order-bench` calls it before its measured window opens.
/// It hands the state over through the same derived-state slot a committed
/// block uses, stamped with the current head, so the next `BlockState::new`
/// takes it. Whatever a previous block already carried is kept; rows it did
/// not hold are added. Both come from the same committed state, so they agree.
pub fn preload_state(db: &Database) -> Result<PreloadStats, DbError> {
    let head = Meta::new(db).head_height();
    let mut carried = db
        .derived_take_at(head)
        .and_then(|b| b.downcast::<Carried>().ok())
        .map(|b| *b)
        .unwrap_or_default();
    let mut stats = PreloadStats::default();

    for item in db.iter_prefix(CF_ACCOUNTS, &[])? {
        let (k, v) = item?;
        if k.len() != 20 {
            continue;
        }
        let mut a = [0u8; 20];
        a.copy_from_slice(&k);
        carried
            .accounts
            .entry(Address(a))
            .or_insert_with(|| AccountState::decode(&v).expect("decode AccountState"));
        stats.accounts += 1;
    }

    let market_ids: Vec<MarketId> = db
        .iter_prefix(cb_state::CF_MARKETS, &[])?
        .filter_map(|item| item.ok())
        .filter_map(|(_, v)| bincode::deserialize::<MarketConfig>(&v).ok())
        .map(|m| m.id)
        .collect();
    let orders = Orders::new(db);
    for market in market_ids {
        stats.markets += 1;
        let book = carried.spot_books.entry(market).or_insert_with(|| {
            let mut book = SpotBook::new();
            for o in BookIndex::new(db).list_orders_for_market(&orders, market) {
                book.insert_resting(RestingOrder {
                    order_id: o.order_id,
                    owner: o.owner,
                    side: o.side,
                    price: o.price,
                    qty: o.qty_total,
                    qty_remaining: o.qty_remaining,
                    tif: o.tif,
                    post_only: o.post_only,
                    client_order_id: o.client_order_id,
                    seq: o.seq,
                });
            }
            book
        });
        stats.resting_orders += book.len();
    }

    db.derived_put(head, Box::new(carried));
    Ok(stats)
}

pub struct BlockState<'a> {
    /// The direct handle. Used for exactly one thing: writing the final batch.
    db: &'a Database,
    /// The handle the executor sees. Every `put` and `delete` through it is
    /// buffered until `commit_into`.
    staged: Database,
    accounts: HashMap<Address, AccountState>,
    /// Each cached account's row as RocksDB held it when this block first read
    /// it (`None`: no row). The diff hashes an account only when its final value
    /// differs from this, because the set of accounts merely *read* differs
    /// between a leader and a replica that prefetches, and the state hash must
    /// not.
    account_origin: HashMap<Address, Option<AccountState>>,
    /// Position-CF keys touched during the tx loop. Either a put or a delete
    /// counts as touched; `commit` re-reads each from `CF_POSITIONS` and
    /// drives the positions trie accordingly. `HashSet` over `(Address,
    /// DerivId)` keeps duplicates collapsed — a position written and then
    /// deleted in the same block resolves to whatever the final CF read
    /// returns, which is correct.
    touched_positions: HashSet<(Address, DerivId)>,
    /// Market-CF keys touched during the tx loop. Same shape as
    /// `touched_positions` but keyed by `DerivId` only — perp and future
    /// records share one trie via `MarketsTrie`.
    touched_markets: HashSet<DerivId>,
    /// Margin-account keys touched during the tx loop, by owner. Same shape as
    /// `touched_positions`. Every margin write in the executor registers here;
    /// see `executor::put_margin`, which is the single door they all go
    /// through so a new call site cannot quietly skip the trie.
    touched_margin: HashSet<Address>,
    /// The in-memory spot books this block matches against. Carried over from
    /// the previous block when it left them behind, otherwise hydrated lazily.
    /// See [`BlockState::spot_book_mut`].
    spot_books: HashMap<MarketId, SpotBook>,
    /// Spot market configuration, read once per block per market. Genesis
    /// writes these rows and nothing in the executor changes them.
    spot_market_cfgs: HashMap<MarketId, MarketConfig>,
    /// This chain's id, read once when the block starts instead of once per
    /// transaction. The executor compares every transaction against it; on a
    /// database large enough to thrash the open-file cache that point read
    /// alone was two thirds of the block loop.
    chain_id: u32,
    /// The head height this block builds on. The books are handed to the next
    /// block stamped `parent_height + 1`; see [`BlockState::new`].
    parent_height: u64,
    /// Undo log for `accounts`, in write order: the value the map held for
    /// that address before the write, or `None` when the map had no entry.
    account_journal: Vec<(Address, Option<AccountState>)>,
    /// Undo logs for the touched-key sets: keys this journal window newly
    /// inserted, so a rollback can take them back out.
    position_journal: Vec<(Address, DerivId)>,
    market_journal: Vec<DerivId>,
    margin_journal: Vec<Address>,
    /// Markets whose cached book this journal window changed (a fill, a
    /// resting insert, a cancel), so a rollback drops exactly those.
    book_journal: Vec<MarketId>,
}

impl<'a> BlockState<'a> {
    /// Start a block on top of the current head.
    ///
    /// The previous block leaves its order books in the database handle's
    /// derived-state slot, stamped with the height they describe. If that
    /// stamp is the head this block builds on, they are taken and matched
    /// against directly; otherwise this starts empty and each book is
    /// hydrated on first use. Taking rather than borrowing means a second
    /// `BlockState` on the same handle rebuilds instead of sharing a book
    /// another block is mutating, and a block whose write never landed leaves
    /// a stamp that no later block will accept.
    pub fn new(db: &'a Database) -> Self {
        let meta = Meta::new(db);
        let parent_height = meta.head_height();
        let chain_id = meta.chain_id();
        let carried = db
            .derived_take_at(parent_height)
            .and_then(|b| b.downcast::<Carried>().ok())
            .map(|b| *b)
            .unwrap_or_default();
        Self {
            db,
            staged: db.staged(),
            accounts: carried.accounts,
            account_origin: HashMap::default(),
            touched_positions: HashSet::default(),
            touched_markets: HashSet::default(),
            touched_margin: HashSet::default(),
            spot_books: carried.spot_books,
            spot_market_cfgs: HashMap::default(),
            chain_id,
            parent_height,
            account_journal: Vec::new(),
            position_journal: Vec::new(),
            market_journal: Vec::new(),
            margin_journal: Vec::new(),
            book_journal: Vec::new(),
        }
    }

    /// This chain's id, as genesis set it. Read once per block.
    pub fn chain_id(&self) -> u32 {
        self.chain_id
    }

    /// The configuration of spot `market`, from `CF_MARKETS`, cached for the
    /// rest of the block. `None` if the market is not listed.
    pub fn spot_market_config(&mut self, market: MarketId) -> Option<MarketConfig> {
        if let Some(c) = self.spot_market_cfgs.get(&market) {
            return Some(c.clone());
        }
        let c = Markets::new(&self.staged).get(market)?;
        self.spot_market_cfgs.insert(market, c.clone());
        Some(c)
    }

    /// The block's staged `Database` handle, for CF reads/writes that don't
    /// fit `BlockState`'s account-cache shape (the cache is keyed by
    /// `Address`, not by order id): `CF_ORDERS`, `CF_BOOK_INDEX`,
    /// `CF_PERP_ORDERS`, `CF_PERP_BOOK_INDEX`, `CF_POSITIONS`, oracle,
    /// funding and margin CFs.
    ///
    /// Returned by value — it is two `Arc` clones — rather than as a borrow,
    /// so a handler can hold it across a `&mut self` call the way it always
    /// has. Every handle returned here shares one stage, so writes through any
    /// of them are part of the same block transaction and are visible to
    /// reads through any other.
    pub fn db(&self) -> Database {
        self.staged.clone()
    }

    /// Mark the current position of every undo log. See [`Savepoint`].
    pub fn savepoint(&mut self) -> Savepoint {
        Savepoint {
            stage: self.staged.stage_mark(),
            accounts: self.account_journal.len(),
            positions: self.position_journal.len(),
            markets: self.market_journal.len(),
            margin: self.margin_journal.len(),
            books: self.book_journal.len(),
        }
    }

    /// Undo everything written since `sp`, leaving the block exactly as it was
    /// at that point. Used by the block builder when a transaction fails: the
    /// transaction is then indistinguishable from one that was never
    /// submitted, which is what makes the block's state root independent of
    /// the transactions that failed in it.
    pub fn rollback(&mut self, sp: Savepoint) {
        self.staged.stage_rollback_to(sp.stage);
        while self.account_journal.len() > sp.accounts {
            let (addr, prev) = self.account_journal.pop().expect("len > mark");
            match prev {
                Some(state) => {
                    self.accounts.insert(addr, state);
                }
                None => {
                    self.accounts.remove(&addr);
                }
            }
        }
        while self.position_journal.len() > sp.positions {
            let k = self.position_journal.pop().expect("len > mark");
            self.touched_positions.remove(&k);
        }
        while self.market_journal.len() > sp.markets {
            let k = self.market_journal.pop().expect("len > mark");
            self.touched_markets.remove(&k);
        }
        while self.margin_journal.len() > sp.margin {
            let k = self.margin_journal.pop().expect("len > mark");
            self.touched_margin.remove(&k);
        }
        // The cached books were mutated in place by a placement that is now
        // being undone. Dropping them all keeps the correctness argument to
        // one sentence — after a rollback the cache is gone, so it cannot be
        // stale — at the cost of one rehydration per failed transaction, and
        // failures are the rare path.
        // Only a book the rolled-back transaction changed is out of step
        // with the rows; it is dropped and rehydrated on next use. The rest
        // stay. This used to clear every book, so a block with one refused
        // order in it rebuilt both books from RocksDB on the next placement:
        // with a quarter of a loaded block refused, that rebuild was three
        // quarters of the executor's time, 42 µs a transaction where the
        // matching itself is about one.
        while self.book_journal.len() > sp.books {
            let m = self.book_journal.pop().expect("len > mark");
            self.spot_books.remove(&m);
        }
    }

    /// Declare that everything written so far is final for this block: drop
    /// the undo logs. Called after each transaction succeeds so the journals
    /// stay proportional to one transaction rather than to the whole block.
    pub fn commit_point(&mut self) {
        self.staged.stage_commit_point();
        self.account_journal.clear();
        self.position_journal.clear();
        self.market_journal.clear();
        self.margin_journal.clear();
        self.book_journal.clear();
    }

    fn journal_account(&mut self, addr: Address) {
        // Only the first write to an address within the current window needs
        // recording — the undo log restores the *oldest* value, and later
        // entries would only shadow it. Windows are one transaction long, so
        // this scan is over a handful of entries.
        if self.account_journal.iter().any(|(a, _)| *a == addr) {
            return;
        }
        let prev = self.accounts.get(&addr).cloned();
        self.account_journal.push((addr, prev));
    }

    /// Load (or fetch from RocksDB) the current state for `addr`. Subsequent reads /
    /// writes within this block hit only the in-memory map.
    pub fn load(&mut self, addr: &Address) -> AccountState {
        if let Some(s) = self.accounts.get(addr) {
            // Held from an earlier block or a prefetch, and untouched so far
            // in this one: its value is still the block's starting value.
            if !self.account_origin.contains_key(addr) {
                self.account_origin.insert(*addr, Some(s.clone()));
            }
            return s.clone();
        }
        let row = Accounts::new(&self.staged).get(addr);
        let s = row.clone().unwrap_or_default();
        self.account_origin.entry(*addr).or_insert(row);
        // A cache fill is a mutation as far as `commit` is concerned: it
        // writes every entry in this map, so an address merely *read* by a
        // failing transaction would otherwise be committed as a row (a
        // default one, if it did not exist) and shift the state root.
        self.journal_account(*addr);
        self.accounts.insert(*addr, s.clone());
        s
    }

    /// Bulk-prefetch every address in `addrs` into the in-memory cache, fanned out
    /// across the rayon thread pool. The serial `apply_tx` loop then hits the cache
    /// for every read instead of doing a synchronous RocksDB get on first touch.
    /// On a 666-tx block touching ~1300 unique addresses this turns 1300 serial
    /// I/Os into a parallel batch — measurable win on the replica's apply rate.
    pub fn prefetch(&mut self, addrs: &[Address]) {
        let mut unique: HashMap<Address, ()> =
            HashMap::with_capacity_and_hasher(addrs.len(), Default::default());
        for a in addrs {
            if !self.accounts.contains_key(a) {
                unique.insert(*a, ());
            }
        }
        if unique.is_empty() {
            return;
        }
        let keys: Vec<Address> = unique.into_keys().collect();
        let staged = &self.staged;
        let fetched: Vec<(Address, Option<AccountState>)> = keys
            .par_iter()
            .with_min_len(64)
            .map(|a| (*a, Accounts::new(staged).get(a)))
            .collect();
        for (a, row) in fetched {
            if !self.accounts.contains_key(&a) {
                let s = row.unwrap_or_default();
                // Straight to the journal, skipping `journal_account`'s
                // duplicate scan. Nothing is in the journal without also being
                // in `accounts` (every journal entry is pushed alongside a map
                // write), so the `contains_key` above has already answered the
                // question — and the scan would be quadratic here, where the
                // journal grows by one per address across a block-sized batch.
                self.account_journal.push((a, None));
                self.accounts.insert(a, s);
            }
        }
    }

    pub fn put(&mut self, addr: Address, state: AccountState) {
        if !self.account_origin.contains_key(&addr) {
            let origin = match self.accounts.get(&addr) {
                Some(s) => Some(s.clone()),
                None => Accounts::new(&self.staged).get(&addr),
            };
            self.account_origin.insert(addr, origin);
        }
        self.journal_account(addr);
        self.accounts.insert(addr, state);
    }

    /// Borrow this block's in-memory `SpotBook` for `market`, hydrating it
    /// from `CF_BOOK_INDEX` on first use within the block.
    ///
    /// # Why this exists
    ///
    /// The spot `PlaceOrder` handler used to rebuild the whole book from
    /// RocksDB before *every* order, which made order placement O(resting
    /// depth). A sampling profile of `cb-order-bench in-process` at depth 1000
    /// attributed 72% of total runtime to the `CF_ORDERS` prefix scan plus
    /// bincode decode and a further 16% to re-inserting the rows into a fresh
    /// `SpotBook` — 88.6% of the run spent rebuilding a book that had not
    /// changed, against 6.4% spent actually matching. Hydrating once per
    /// (block, market) turns that into O(depth) per block.
    ///
    /// Hydrating it through `CF_ORDERS` was the second half of the same
    /// problem: that scan reads every order on every market and every key a
    /// fill has deleted, so one block's hydration cost grew with the chain's
    /// whole order history. `BookIndex::list_orders_for_market` reads one
    /// prefix per side instead — see its docs for the profile that found it.
    ///
    /// # Why it stays deterministic
    ///
    /// `SpotBook::place` applies to itself exactly the mutations it emits, and
    /// the executor persists exactly those same mutations to `CF_ORDERS` and
    /// `CF_BOOK_INDEX`. So the cached book after N placements holds the same
    /// orders, prices, sequence numbers and remaining quantities as a book
    /// rehydrated from the column family at that point. `SpotBook` orders
    /// everything through `BTreeMap`s keyed by `(price, seq)` and by order id,
    /// so the contents — and every iteration over them — are independent of
    /// the order rows were inserted in. A replica that rehydrates per order and
    /// a leader that hydrates per block therefore match the same way and
    /// produce the same state root.
    ///
    /// Any path that mutates `CF_ORDERS` without going through
    /// `SpotBook::place` — the two cancel handlers — must call
    /// [`BlockState::invalidate_spot_book`] so the next placement re-reads the
    /// column family.
    pub fn spot_book_mut(&mut self, market: MarketId) -> &mut SpotBook {
        let staged = &self.staged;
        self.spot_books.entry(market).or_insert_with(|| {
            let mut book = SpotBook::new();
            let orders = Orders::new(staged);
            for o in BookIndex::new(staged).list_orders_for_market(&orders, market) {
                book.insert_resting(RestingOrder {
                    order_id: o.order_id,
                    owner: o.owner,
                    side: o.side,
                    price: o.price,
                    qty: o.qty_total,
                    qty_remaining: o.qty_remaining,
                    tif: o.tif,
                    post_only: o.post_only,
                    client_order_id: o.client_order_id,
                    seq: o.seq,
                });
            }
            book
        })
    }

    /// The cached spot book for `market` if this block already holds one —
    /// without hydrating. A cancel uses this to take the order out of the
    /// book it came from; if no book is cached there is nothing to keep in
    /// step, since the next hydration reads the rows the cancel just deleted.
    pub fn spot_book_cached_mut(&mut self, market: MarketId) -> Option<&mut SpotBook> {
        self.spot_books.get_mut(&market)
    }

    /// Drop the cached spot book for `market`, forcing the next
    /// [`BlockState::spot_book_mut`] to rehydrate from `CF_ORDERS`.
    ///
    /// Called by the cancel handlers, which delete rows from `CF_ORDERS` and
    /// `CF_BOOK_INDEX` directly rather than through the matcher. Invalidating
    /// rather than patching the cached book keeps the correctness argument to
    /// one sentence — after a cancel the cache is gone, so it cannot be stale —
    /// at the cost of one rehydration per cancel, which is exactly what the
    /// old code paid for every order.
    pub fn invalidate_spot_book(&mut self, market: MarketId) {
        self.spot_books.remove(&market);
    }

    /// Record that the cached book for `market` was changed by the current
    /// transaction, so that a rollback of it drops that book (and only that
    /// book). The executor calls this after a placement that filled or
    /// rested, and after a cancel took an order out of the cached book; a
    /// placement the matcher rejected changed nothing and is not recorded.
    pub fn note_spot_book_mutated(&mut self, market: MarketId) {
        if self.book_journal.last() != Some(&market) {
            self.book_journal.push(market);
        }
    }

    /// Whether `market`'s book is cached in this block right now. For tests
    /// of what a rollback keeps.
    pub fn has_cached_spot_book(&self, market: MarketId) -> bool {
        self.spot_books.contains_key(&market)
    }

    /// Record that the position row for `(owner, market)` was written or
    /// deleted during this block. Idempotent. The CF-level mutation is the
    /// caller's responsibility (executor still calls `Positions::new(db).put`
    /// directly so reads later in the same block see the new value); this
    /// just registers the key with the trie commit step.
    pub fn touch_position(&mut self, owner: Address, market: DerivId) {
        if self.touched_positions.insert((owner, market)) {
            self.position_journal.push((owner, market));
        }
    }

    /// Record that the market row for `id` was written during this block.
    /// Mirrors `touch_position`.
    pub fn touch_market(&mut self, id: DerivId) {
        if self.touched_markets.insert(id) {
            self.market_journal.push(id);
        }
    }

    /// Record that `owner`'s margin row was written during this block.
    /// Mirrors `touch_position`; `executor::put_margin` calls it on every
    /// write so the margin trie sees the row at commit.
    pub fn touch_margin(&mut self, owner: Address) {
        if self.touched_margin.insert(owner) {
            self.margin_journal.push(owner);
        }
    }

    /// Atomically write all changed accounts (CF + trie + new accounts_root),
    /// flush touched positions into the positions trie, and flush touched
    /// markets into the markets trie — all in **one** RocksDB `WriteBatch`.
    /// Returns the three roots that feed `compute_state_root`.
    ///
    /// A crash between the tx loop and this call leaves the chain exactly at
    /// the previous height: nothing this block did has reached disk, because
    /// every CF write it made is still sitting in the stage. There is no
    /// half-applied state to recover from.
    pub fn commit(self) -> Result<CommitRoots, DbError> {
        let db = self.db;
        let mut batch = db.batch();
        let roots = self.commit_into(&mut batch)?;
        db.write_batch(batch)?;
        Ok(roots)
    }

    /// Fold the whole block into `batch` and return the four roots, **without
    /// writing anything**.
    ///
    /// This is the seam the atomicity guarantee hangs on. The caller can
    /// compute the state root from the returned roots, compare it against the
    /// one the block claims, and simply drop `batch` if they disagree — at
    /// which point the database is byte-for-byte what it was before the block
    /// arrived. When they agree, the caller adds the block bytes and the head
    /// pointer to the same `batch` and writes it once, so a crash can never
    /// leave the head pointing at a block whose state was never written.
    ///
    /// The four tries come first: their `commit_batch` calls re-read the rows
    /// this block wrote through the *staged* handle, so the stage has to be
    /// intact while they run. The staged CF rows go in afterwards. Order
    /// within the batch does not matter here — the tries write only
    /// `CF_TRIE_NODES` and `CF_META`, which the executor never touches, so the
    /// two groups are disjoint — but the batch is applied as one write either
    /// way.
    pub fn commit_into(self, batch: &mut DbBatch<'_>) -> Result<CommitRoots, DbError> {
        let prev_root = Meta::new(self.db).state_root();
        let cfs = self.finish()?;
        let (diff, diff_entries) = diff_hash(&cfs);
        Database::maps_into_batch(batch, &cfs)?;
        Ok(CommitRoots {
            state_root: chain_state_hash(prev_root, diff),
            diff_entries,
        })
    }

    /// End the block: every row it wrote, one map per column family in
    /// `ALL_CFS` order, **moved** out of the stage rather than copied, with the
    /// changed accounts encoded into the accounts map. Hands the order books
    /// and account rows on to the next block.
    ///
    /// Nothing is hashed or written here. [`BlockState::commit_into`] hashes
    /// and batches the maps synchronously; a pipelined leader hands them to
    /// its writer thread to be sealed. Both hash them with
    /// `cb_state::diff_hash`, so both reach the same root.
    pub fn finish(self) -> Result<Vec<CfMap>, DbError> {
        let accounts_idx = ALL_CFS
            .iter()
            .position(|c| *c == CF_ACCOUNTS)
            .expect("accounts is a column family");
        let mut cfs = self.staged.take_stage_maps();
        debug_assert!(
            cfs[accounts_idx].is_empty(),
            "account rows are written from the account map, never through the stage"
        );

        // Accounts: only those whose value moved. `account_origin` holds
        // exactly the accounts this block read or wrote, with their starting
        // values; `accounts` also holds every account carried from earlier
        // blocks, which this block did not touch.
        for (addr, origin) in &self.account_origin {
            // Absent: a cache fill undone by a rolled-back transaction.
            let Some(cur) = self.accounts.get(addr) else {
                continue;
            };
            let moved = match origin {
                Some(o) => o != cur,
                None => *cur != AccountState::default(),
            };
            if moved {
                cfs[accounts_idx].insert(addr.0.to_vec(), Some(cur.encode()));
            }
        }

        // Hand the books and accounts to whichever block comes next, stamped
        // with the height they will describe once these rows are written. If
        // they are not written — a state-root mismatch drops them, and so does
        // any error before the write — the head stays where it is, no later
        // block asks for this stamp, and both are rebuilt from the column
        // families. Nothing has to notice the failure.
        self.db.derived_put(
            self.parent_height + 1,
            Box::new(Carried {
                spot_books: self.spot_books,
                accounts: self.accounts,
            }),
        );
        Ok(cfs)
    }
}
