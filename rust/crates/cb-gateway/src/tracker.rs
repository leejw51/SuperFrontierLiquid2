//! Order and fill bookkeeping, and why the gateway has to do it at all.
//!
//! ## The node cannot answer "what are my orders?"
//!
//! There is no per-account order query in the RPC surface, no owner on a fill
//! event, and no receipts on a block. What a block does carry is every
//! transaction and its recovered sender — so the gateway reconstructs order
//! state by watching its own submissions land.
//!
//! **This is a derived view, not consensus truth.** Balances and positions are
//! read from the chain and are authoritative; orders and fills in this module
//! are the gateway's best reconstruction. The distinction is worth keeping in
//! mind when reading `/account/orders`: a gateway that restarts loses its
//! order history, because the chain never had it to give back.
//!
//! ## Order ids
//!
//! The engine derives an order id from `(sender, block_height, tx_index)` —
//! see `cb_block::executor::derive_order_id`, which this module mirrors byte
//! for byte in [`derive_order_id`]. None of those inputs is known at
//! submission time, so an order has no id until the block carrying it
//! arrives. Until then it is `pending` and addressed by its transaction hash,
//! which is what `POST /orders` returns and what `DELETE /orders/{id}`
//! accepts alongside a real id.
//!
//! ## Inclusion is not acceptance
//!
//! Under the sequencer scheme a transaction in a block had applied, because
//! the block builder dropped anything the executor refused. Under BFT ordering
//! (ADR 0008) the block keeps every committed transaction, refused or not, and
//! carries no receipt saying which. So a placement that reaches a block is
//! **included** and still `pending` until the chain's own state answers for
//! it. The answer is the account nonce — an applied transaction consumes it, a
//! refused one does not — which the gateway knows for every transaction it
//! signed ([`crate::signer`]). [`OrderTracker::apply_block`] takes that
//! verdict per transaction; a placement with none stays pending, included,
//! until [`OrderTracker::reconcile`] reads the nonce on a later block. What
//! nothing here can say is *why* the chain refused an order: the executor's
//! reason is not on the wire, so a refused placement reads `rejected` and no
//! more.
//!
//! A cancel is the same story. `DELETE /orders/{id}` answers `cancelled` on
//! mempool acceptance, as the contract has always said; the order itself is
//! moved only once the block carrying the cancel has been answered for, and a
//! refused cancel leaves it where it was and says so on the account channel.
//!
//! ## Idempotency
//!
//! `client_order_id` is reserved *before* the order is submitted, not checked
//! after. Two simultaneous requests carrying the same id must place one
//! order; a lookup-then-submit would let both through the gap between the two
//! steps. The loser of the race waits on the reservation and is handed the
//! winner's order.

use crate::node::{BlockSummary, BlockTx};
use crate::symbols::MarketRef;
use cb_tx::TxBody;
use cb_types::{Address, Side, Tif, H256};
use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use std::sync::Arc;

/// How long a `client_order_id` stays reserved, per the API contract.
pub const IDEMPOTENCY_WINDOW_HOURS: i64 = 24;

/// Where an order is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderStatus {
    /// Submitted, not yet in a block.
    Pending,
    /// Resting on the book.
    Open,
    /// Partly filled and still resting.
    PartiallyFilled,
    /// Completely filled.
    Filled,
    /// Cancelled, by the user or by its time in force.
    Cancelled,
    /// Refused.
    Rejected,
}

impl OrderStatus {
    /// The wire representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Open => "open",
            Self::PartiallyFilled => "partially_filled",
            Self::Filled => "filled",
            Self::Cancelled => "cancelled",
            Self::Rejected => "rejected",
        }
    }

    /// Whether the order can still trade.
    pub fn is_live(self) -> bool {
        matches!(self, Self::Pending | Self::Open | Self::PartiallyFilled)
    }

    /// Whether the order matches the contract's `status` query filter.
    pub fn matches_filter(self, filter: &str) -> bool {
        match filter {
            "all" => true,
            "open" => self.is_live(),
            "filled" => self == Self::Filled,
            "cancelled" => matches!(self, Self::Cancelled | Self::Rejected),
            _ => false,
        }
    }
}

/// One tracked order.
#[derive(Debug, Clone)]
pub struct OrderRecord {
    /// The on-chain order id, once the block carrying it has arrived.
    pub order_id: Option<H256>,
    /// The transaction that placed it. Doubles as the id while pending.
    pub tx_hash: H256,
    /// Who placed it.
    pub user_id: String,
    /// Their address.
    pub address: Address,
    /// Market symbol.
    pub symbol: String,
    /// Chain handle.
    pub market: MarketRef,
    /// Direction.
    pub side: Side,
    /// `limit` or `market`.
    pub order_type: &'static str,
    /// Limit price, or the protective bound of a market order.
    pub price: u128,
    /// Original quantity.
    pub qty: u128,
    /// Quantity filled so far.
    pub filled_qty: u128,
    /// Volume-weighted fill price, when anything has filled.
    pub avg_fill_price: Option<u128>,
    /// Lifecycle state.
    pub status: OrderStatus,
    /// Time in force.
    pub tif: Tif,
    /// Post-only flag.
    pub post_only: bool,
    /// Reduce-only flag.
    pub reduce_only: bool,
    /// The caller's idempotency key.
    pub client_order_id: Option<String>,
    /// The nonce the placing transaction was signed with. The chain's nonce
    /// for `address` moving past it is what says the placement applied.
    pub nonce: u64,
    /// The height of the block that carried the placing transaction, once
    /// one has. Set whether or not the chain applied it.
    pub included_at: Option<u64>,
    /// The placing transaction's index within that block, which the engine
    /// folds into the order id.
    pub tx_index: Option<u32>,
    /// When the gateway accepted it.
    pub created_at: DateTime<Utc>,
    /// When it last changed.
    pub updated_at: DateTime<Utc>,
}

impl OrderRecord {
    /// The identifier the contract publishes: the order id once known, the
    /// transaction hash while still pending.
    pub fn public_id(&self) -> String {
        let bytes = self.order_id.unwrap_or(self.tx_hash);
        format!("0x{}", hex::encode(bytes.0))
    }

    /// Quantity still working.
    pub fn remaining_qty(&self) -> u128 {
        self.qty.saturating_sub(self.filled_qty)
    }
}

/// One tracked fill.
#[derive(Debug, Clone)]
pub struct FillRecord {
    /// Stable id.
    pub fill_id: String,
    /// The order it belongs to, as the contract publishes it.
    pub order_id: String,
    /// Whose fill.
    pub user_id: String,
    /// Market symbol.
    pub symbol: String,
    /// Direction taken.
    pub side: Side,
    /// Fill price in the market's price scale.
    pub price: u128,
    /// Fill quantity in the market's quantity scale.
    pub qty: u128,
    /// Fee paid, micro-USD.
    pub fee: u128,
    /// Asset the fee was paid in.
    pub fee_asset: String,
    /// `maker` or `taker`.
    pub role: &'static str,
    /// Realised PnL, micro-USD.
    pub realized_pnl: i128,
    /// When it happened.
    pub at: DateTime<Utc>,
    /// Block height.
    pub height: u64,
}

/// Mirror of `cb_block::executor::derive_order_id`.
///
/// Layout: `sender[0..20] || height_be[20..28] || tx_index_be[28..32]`, with
/// the intra-transaction counter XORed into the last byte. That function is
/// private to the executor, so this is a copy — and copies drift, which is
/// why [`tests::the_order_id_layout_matches_the_engines`] pins the layout
/// rather than trusting the comment.
pub fn derive_order_id(sender: Address, height: u64, tx_index: u32, intra: u8) -> H256 {
    let mut bytes = [0u8; 32];
    bytes[..20].copy_from_slice(&sender.0);
    bytes[20..28].copy_from_slice(&height.to_be_bytes());
    bytes[28..32].copy_from_slice(&tx_index.to_be_bytes());
    bytes[31] ^= intra;
    H256(bytes)
}

/// What the chain did with one of the gateway's own transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The executor applied it: the sender's nonce moved past it.
    Applied,
    /// The executor refused it: the sender's nonce did not move.
    Refused,
}

/// The verdicts for the gateway's own transactions in one block, by hash.
///
/// A transaction with no entry is either somebody else's — a bot's, submitted
/// over RPC, whose nonce the gateway does not know — or one whose sender the
/// gateway could not read the account of when the block arrived. The first is
/// assumed applied, which is the assumption the sequencer scheme made for
/// everything and the one gap this module cannot close without a receipt on
/// the block; the second stays unresolved until a later read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlockVerdicts {
    by_hash: std::collections::HashMap<H256, Verdict>,
    /// The gateway's own transactions in this block whose sender could not be
    /// read. Kept apart from "somebody else's": the gateway signed these, so
    /// it knows it cannot say, and must not assume they applied.
    unresolved: std::collections::HashSet<H256>,
}

impl BlockVerdicts {
    /// No verdicts at all.
    pub fn none() -> Self {
        Self::default()
    }

    /// Record the verdict for one transaction.
    pub fn record(&mut self, hash: H256, verdict: Verdict) {
        self.by_hash.insert(hash, verdict);
    }

    /// The verdict for one transaction, if the gateway has one.
    pub fn verdict(&self, hash: &H256) -> Option<Verdict> {
        self.by_hash.get(hash).copied()
    }

    /// Whether a transaction applied, for the code that derives ledger entries
    /// and fills from a block. `None` means the gateway cannot say.
    pub fn applied(&self, hash: &H256) -> Option<bool> {
        self.verdict(hash).map(|v| v == Verdict::Applied)
    }

    /// Note one of the gateway's own transactions whose verdict could not be
    /// read with this block.
    pub fn mark_unresolved(&mut self, hash: H256) {
        self.unresolved.insert(hash);
    }

    /// Whether this is one of the gateway's own transactions that the block
    /// carried and nobody could answer for.
    pub fn is_unresolved(&self, hash: &H256) -> bool {
        self.unresolved.contains(hash)
    }

    /// How many verdicts there are.
    pub fn len(&self) -> usize {
        self.by_hash.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.by_hash.is_empty()
    }
}

/// What a cancel transaction the gateway submitted was meant to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelTarget {
    /// One order, by its on-chain id.
    Order(H256),
    /// Every live order the sender has on one spot market.
    Market(MarketRef),
}

/// A cancel the gateway submitted and the chain has not yet answered for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingCancel {
    address: Address,
    nonce: u64,
    target: CancelTarget,
    /// The height of the block that carried it, once one has.
    included_at: Option<u64>,
    /// When the chain answered for it. A resolved cancel is kept, not
    /// removed, because the same block is folded by both the watcher and
    /// `POST /orders`: forgetting it would make the second fold take the
    /// cancel for somebody else's and apply it, refused or not. Pruned with
    /// the orders.
    resolved_at: Option<DateTime<Utc>>,
}

/// A held reservation on a `client_order_id`.
///
/// While one is alive no other request may place an order under the same
/// `(user, client_order_id)`. Dropping it without recording an order releases
/// the reservation, so a failed submission does not poison the key for a day.
pub struct Reservation {
    key: (String, String),
    slot: Arc<tokio::sync::Mutex<Option<ReservedOrder>>>,
    guard: tokio::sync::OwnedMutexGuard<Option<ReservedOrder>>,
    tracker: Arc<OrderTracker>,
}

#[derive(Debug, Clone)]
struct ReservedOrder {
    tx_hash: H256,
    at: DateTime<Utc>,
}

impl Reservation {
    /// Record the order this reservation now stands for.
    pub fn fulfil(mut self, tx_hash: H256, at: DateTime<Utc>) {
        *self.guard = Some(ReservedOrder { tx_hash, at });
    }

    /// Give up the reservation, leaving the key free.
    pub fn release(self) {
        self.tracker.reservations.remove(&self.key);
        drop(self.slot);
    }
}

/// What a reservation attempt found.
pub enum Reserved {
    /// The caller owns the key and must place the order.
    Fresh(Box<Reservation>),
    /// An order already exists under this key; return it unchanged.
    Existing(Box<OrderRecord>),
}

/// The gateway's order and fill book.
#[derive(Default)]
pub struct OrderTracker {
    orders: DashMap<H256, OrderRecord>,
    by_order_id: DashMap<H256, H256>,
    reservations: DashMap<(String, String), Arc<tokio::sync::Mutex<Option<ReservedOrder>>>>,
    fills: DashMap<String, Vec<FillRecord>>,
    /// Cancels the gateway submitted, by the cancel transaction's hash.
    cancels: DashMap<H256, PendingCancel>,
    sequence: std::sync::atomic::AtomicU64,
}

impl std::fmt::Debug for OrderTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrderTracker")
            .field("orders", &self.orders.len())
            .finish_non_exhaustive()
    }
}

impl OrderTracker {
    /// An empty tracker.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Take the `client_order_id` reservation, or hand back the order that
    /// already holds it.
    ///
    /// A caller without a `client_order_id` gets a reservation on a synthetic
    /// key, which nobody else can collide with.
    pub async fn reserve(
        self: &Arc<Self>,
        user_id: &str,
        client_order_id: Option<&str>,
        now: DateTime<Utc>,
    ) -> Reserved {
        let key = (
            user_id.to_string(),
            match client_order_id {
                Some(id) => id.to_string(),
                None => format!(
                    "\u{0}anonymous\u{0}{}",
                    self.sequence
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ),
            },
        );
        let slot = Arc::clone(
            self.reservations
                .entry(key.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None)))
                .value(),
        );
        // Whoever gets the lock either places the order or finds the previous
        // placement waiting for them. The second caller blocks here rather
        // than racing ahead into a duplicate submission.
        let guard = Arc::clone(&slot).lock_owned().await;
        if let Some(existing) = guard.clone() {
            let fresh_enough = now - existing.at < Duration::hours(IDEMPOTENCY_WINDOW_HOURS);
            if fresh_enough {
                if let Some(record) = self.orders.get(&existing.tx_hash) {
                    return Reserved::Existing(Box::new(record.clone()));
                }
            }
        }
        Reserved::Fresh(Box::new(Reservation {
            key,
            slot,
            guard,
            tracker: Arc::clone(self),
        }))
    }

    /// Record a freshly submitted order.
    #[allow(clippy::too_many_arguments)]
    pub fn record_placement(&self, record: OrderRecord) {
        self.orders.insert(record.tx_hash, record);
    }

    /// Remember a cancel the gateway submitted, so the block carrying it can
    /// be answered for like a placement.
    pub fn record_cancel(&self, tx_hash: H256, address: Address, nonce: u64, target: CancelTarget) {
        self.cancels.insert(
            tx_hash,
            PendingCancel {
                address,
                nonce,
                target,
                included_at: None,
                resolved_at: None,
            },
        );
    }

    /// Fold a block into the tracker.
    ///
    /// Every transaction the gateway submitted is looked up in `verdicts`. A
    /// placement the chain applied becomes `open` and receives its on-chain
    /// id; one the chain refused becomes `rejected`; one with no verdict yet
    /// is marked included and stays `pending` until [`Self::reconcile`]. A
    /// cancel the chain applied closes what it named; a refused cancel leaves
    /// the order as it was, and reports it, because the cancel endpoint
    /// already told the client otherwise.
    ///
    /// Returns every order whose state this block changed, so the caller can
    /// publish an `order_update` for it. The transitions are one-way and each
    /// is guarded, so folding the same block twice reports the change once,
    /// which is what lets both the watcher and `POST /orders`' inclusion wait
    /// call this without the client seeing a duplicate.
    pub fn apply_block(
        &self,
        block: &BlockSummary,
        verdicts: &BlockVerdicts,
        at: DateTime<Utc>,
    ) -> Vec<OrderRecord> {
        let mut changed = Vec::new();
        for tx in &block.txs {
            let verdict = verdicts.verdict(&tx.hash);
            match &tx.body {
                TxBody::PlaceOrder { .. } | TxBody::PlacePerpOrder { .. } => {
                    changed.extend(self.confirm_placement(tx, block.height, verdict, at));
                }
                TxBody::CancelOrder { order_id } | TxBody::CancelPerpOrder { order_id } => {
                    changed.extend(self.confirm_cancel_tx(
                        tx,
                        block.height,
                        CancelTarget::Order(*order_id),
                        verdict,
                        at,
                    ));
                }
                TxBody::BatchCancel { market } => {
                    changed.extend(self.confirm_cancel_tx(
                        tx,
                        block.height,
                        CancelTarget::Market(MarketRef::Spot(*market)),
                        verdict,
                        at,
                    ));
                }
                _ => {}
            }
        }
        changed
    }

    fn confirm_placement(
        &self,
        tx: &BlockTx,
        height: u64,
        verdict: Option<Verdict>,
        at: DateTime<Utc>,
    ) -> Option<OrderRecord> {
        let Some(mut record) = self.orders.get_mut(&tx.hash) else {
            // Somebody else's order — a bot's, most likely. The gateway has no
            // business inventing a record for flow it did not submit.
            return None;
        };
        if record.included_at.is_some() || !record.status.is_live() {
            // Seen before, or already settled by the retry path. Nothing a
            // block can add.
            return None;
        }
        record.included_at = Some(height);
        record.tx_index = Some(tx.index);
        let tx_hash = record.tx_hash;
        match verdict {
            Some(Verdict::Applied) => {
                let order_id = derive_order_id(tx.sender, height, tx.index, 0);
                record.order_id = Some(order_id);
                if record.status == OrderStatus::Pending {
                    record.status = OrderStatus::Open;
                }
                record.updated_at = at;
                let changed = record.clone();
                drop(record);
                self.by_order_id.insert(order_id, tx_hash);
                Some(changed)
            }
            Some(Verdict::Refused) => {
                // No on-chain id: the chain never issued one, so nothing is
                // indexed under it.
                record.status = OrderStatus::Rejected;
                record.updated_at = at;
                Some(record.clone())
            }
            // Included, unresolved. `reconcile` finishes the job once the
            // sender's nonce can be read; the record stays live so the watcher
            // keeps looking at the account.
            None => None,
        }
    }

    /// A cancel transaction reached a block.
    ///
    /// One the gateway submitted is answered for by its verdict. One it did
    /// not — which cannot happen for an order the gateway placed, since only
    /// the holder of the trading key can sign a cancel for it — is taken at
    /// face value, as every cancel was before verdicts existed.
    fn confirm_cancel_tx(
        &self,
        tx: &BlockTx,
        height: u64,
        target: CancelTarget,
        verdict: Option<Verdict>,
        at: DateTime<Utc>,
    ) -> Vec<OrderRecord> {
        let Some(mut pending) = self.cancels.get_mut(&tx.hash) else {
            return self.apply_cancel(tx.sender, target, at);
        };
        if pending.included_at.is_some() {
            return Vec::new();
        }
        pending.included_at = Some(height);
        let sender = pending.address;
        if verdict.is_some() {
            pending.resolved_at = Some(at);
        }
        drop(pending);
        match verdict {
            Some(Verdict::Applied) => self.apply_cancel(sender, target, at),
            Some(Verdict::Refused) => self.refuse_cancel(sender, target, at),
            None => Vec::new(),
        }
    }

    /// The chain applied a cancel: close what it named.
    fn apply_cancel(
        &self,
        sender: Address,
        target: CancelTarget,
        at: DateTime<Utc>,
    ) -> Vec<OrderRecord> {
        match target {
            CancelTarget::Order(order_id) => {
                self.confirm_cancel(order_id, at).into_iter().collect()
            }
            CancelTarget::Market(market) => self.confirm_batch_cancel(sender, market, at),
        }
    }

    /// The chain refused a cancel: the orders it named are exactly as they
    /// were, and their owner was told `cancelled` by the endpoint, so each is
    /// reported again with its real status.
    fn refuse_cancel(
        &self,
        sender: Address,
        target: CancelTarget,
        at: DateTime<Utc>,
    ) -> Vec<OrderRecord> {
        let named: Vec<OrderRecord> = match target {
            CancelTarget::Order(order_id) => self
                .by_order_id
                .get(&order_id)
                .map(|e| *e.value())
                .and_then(|tx_hash| self.orders.get(&tx_hash).map(|r| r.clone()))
                .into_iter()
                .collect(),
            CancelTarget::Market(market) => self
                .orders
                .iter()
                .filter(|r| r.address == sender && r.market == market && r.status.is_live())
                .map(|r| r.clone())
                .collect(),
        };
        for record in &named {
            tracing::info!(
                target: "cb_gateway::tracker",
                order = %record.public_id(),
                status = record.status.as_str(),
                "the chain refused a cancel; the order stands"
            );
        }
        let _ = at;
        named
    }

    fn confirm_cancel(&self, order_id: H256, at: DateTime<Utc>) -> Option<OrderRecord> {
        let tx_hash = self.by_order_id.get(&order_id).map(|e| *e.value())?;
        let mut record = self.orders.get_mut(&tx_hash)?;
        if !record.status.is_live() {
            return None;
        }
        record.status = OrderStatus::Cancelled;
        record.updated_at = at;
        Some(record.clone())
    }

    fn confirm_batch_cancel(
        &self,
        sender: Address,
        market: MarketRef,
        at: DateTime<Utc>,
    ) -> Vec<OrderRecord> {
        let mut changed = Vec::new();
        for mut record in self.orders.iter_mut() {
            if record.address == sender && record.market == market && record.status.is_live() {
                record.status = OrderStatus::Cancelled;
                record.updated_at = at;
                changed.push(record.clone());
            }
        }
        changed
    }

    /// Answer for everything of `address`'s that reached a block without a
    /// verdict, now that the chain's nonce for it can be read.
    ///
    /// Only included transactions are resolved: the nonce says nothing about
    /// one still in the mempool. `chain_nonce` is read at or after the block
    /// that carried them, so a nonce past a transaction's own means it
    /// applied and a nonce at or below it means it was refused.
    pub fn reconcile(
        &self,
        address: Address,
        chain_nonce: u64,
        at: DateTime<Utc>,
    ) -> Vec<OrderRecord> {
        let mut changed = Vec::new();
        let unresolved: Vec<H256> = self
            .orders
            .iter()
            .filter(|r| {
                r.address == address && r.status == OrderStatus::Pending && r.included_at.is_some()
            })
            .map(|r| r.tx_hash)
            .collect();
        for tx_hash in unresolved {
            let Some(mut record) = self.orders.get_mut(&tx_hash) else {
                continue;
            };
            let (Some(height), Some(index)) = (record.included_at, record.tx_index) else {
                continue;
            };
            record.updated_at = at;
            if chain_nonce > record.nonce {
                let order_id = derive_order_id(record.address, height, index, 0);
                record.order_id = Some(order_id);
                record.status = OrderStatus::Open;
                let changed_record = record.clone();
                drop(record);
                self.by_order_id.insert(order_id, tx_hash);
                changed.push(changed_record);
            } else {
                record.status = OrderStatus::Rejected;
                changed.push(record.clone());
            }
        }

        let cancels: Vec<(H256, u64, CancelTarget)> = self
            .cancels
            .iter()
            .filter(|c| c.address == address && c.included_at.is_some() && c.resolved_at.is_none())
            .map(|c| (*c.key(), c.nonce, c.target))
            .collect();
        for (cancel_hash, nonce, target) in cancels {
            if let Some(mut cancel) = self.cancels.get_mut(&cancel_hash) {
                cancel.resolved_at = Some(at);
            }
            if chain_nonce > nonce {
                changed.extend(self.apply_cancel(address, target, at));
            } else {
                changed.extend(self.refuse_cancel(address, target, at));
            }
        }
        changed
    }

    /// The submitter has proven these transactions can never apply: the
    /// chain handed their nonce back, or nothing answered for them in an
    /// hour. Their placements are refused; their cancels are dropped.
    pub fn mark_dead(&self, hashes: &[H256], at: DateTime<Utc>) -> Vec<OrderRecord> {
        let mut changed = Vec::new();
        for hash in hashes {
            if let Some(mut record) = self.orders.get_mut(hash) {
                // Only a placement the chain has not answered for can be dead.
                // One that is open or part-filled was answered `applied` —
                // by a block's verdict or by a later `reconcile` — and its
                // submission lingering in the signer's registry until the
                // hourly sweep says nothing about it.
                if record.status == OrderStatus::Pending {
                    record.status = OrderStatus::Rejected;
                    record.updated_at = at;
                    changed.push(record.clone());
                }
                continue;
            }
            let unresolved = self
                .cancels
                .get_mut(hash)
                .filter(|c| c.resolved_at.is_none())
                .map(|mut c| {
                    c.resolved_at = Some(at);
                    (c.address, c.target)
                });
            if let Some((address, target)) = unresolved {
                changed.extend(self.refuse_cancel(address, target, at));
            }
        }
        changed
    }

    /// Apply a fill the account watcher derived from the user's own state.
    ///
    /// Fills are never taken from the public trade stream: a trade print says
    /// nothing about whose order it was, and attributing one by guesswork
    /// would eventually show a user somebody else's trade.
    /// Returns the order the fill was applied to, when it belonged to one, so
    /// the caller can publish the resulting `order_update`.
    pub fn apply_fill(&self, tx_hash: H256, fill: FillRecord) -> Option<OrderRecord> {
        let mut changed = None;
        if let Some(mut record) = self.orders.get_mut(&tx_hash) {
            let previous_notional = record
                .avg_fill_price
                .unwrap_or(0)
                .saturating_mul(record.filled_qty);
            record.filled_qty = record.filled_qty.saturating_add(fill.qty);
            let total_notional =
                previous_notional.saturating_add(fill.price.saturating_mul(fill.qty));
            if let Some(avg) = total_notional.checked_div(record.filled_qty) {
                record.avg_fill_price = Some(avg);
            }
            record.status = if record.filled_qty >= record.qty {
                OrderStatus::Filled
            } else {
                OrderStatus::PartiallyFilled
            };
            record.updated_at = fill.at;
            changed = Some(record.clone());
        }
        self.fills
            .entry(fill.user_id.clone())
            .or_default()
            .push(fill);
        changed
    }

    /// Mark an order rejected, for a submission the sequencer refused.
    pub fn reject(&self, tx_hash: H256, at: DateTime<Utc>) {
        if let Some(mut record) = self.orders.get_mut(&tx_hash) {
            record.status = OrderStatus::Rejected;
            record.updated_at = at;
        }
    }

    /// Look an order up by the transaction that placed it.
    pub fn find_by_tx_hash(&self, tx_hash: H256) -> Option<OrderRecord> {
        self.orders.get(&tx_hash).map(|r| r.clone())
    }

    /// Look an order up by the identifier the contract publishes — either a
    /// real order id or the transaction hash of one still pending.
    pub fn find(&self, public_id: &str) -> Option<OrderRecord> {
        let hex = public_id.trim().trim_start_matches("0x");
        let bytes = hex::decode(hex).ok()?;
        if bytes.len() != 32 {
            return None;
        }
        let mut raw = [0u8; 32];
        raw.copy_from_slice(&bytes);
        let id = H256(raw);
        if let Some(tx_hash) = self.by_order_id.get(&id).map(|e| *e.value()) {
            return self.orders.get(&tx_hash).map(|r| r.clone());
        }
        self.orders.get(&id).map(|r| r.clone())
    }

    /// One user's orders, newest first.
    pub fn orders_for_user(
        &self,
        user_id: &str,
        status_filter: &str,
        symbol: Option<&str>,
    ) -> Vec<OrderRecord> {
        let mut found: Vec<OrderRecord> = self
            .orders
            .iter()
            .filter(|r| r.user_id == user_id)
            .filter(|r| r.status.matches_filter(status_filter))
            .filter(|r| symbol.is_none_or(|s| r.symbol.eq_ignore_ascii_case(s)))
            .map(|r| r.clone())
            .collect();
        found.sort_by_key(|a| std::cmp::Reverse(a.created_at));
        found
    }

    /// One user's live orders on a market, for the cancel-all path.
    pub fn live_orders_for_user(&self, user_id: &str, symbol: Option<&str>) -> Vec<OrderRecord> {
        self.orders
            .iter()
            .filter(|r| r.user_id == user_id && r.status.is_live())
            .filter(|r| symbol.is_none_or(|s| r.symbol.eq_ignore_ascii_case(s)))
            .map(|r| r.clone())
            .collect()
    }

    /// Every address with a live order, for the account watcher.
    pub fn addresses_with_live_orders(&self) -> Vec<(String, Address)> {
        let mut found: Vec<(String, Address)> = self
            .orders
            .iter()
            .filter(|r| r.status.is_live())
            .map(|r| (r.user_id.clone(), r.address))
            .collect();
        found.sort();
        found.dedup();
        found
    }

    /// One user's fills, newest first.
    pub fn fills_for_user(
        &self,
        user_id: &str,
        symbol: Option<&str>,
        limit: usize,
    ) -> Vec<FillRecord> {
        let mut found: Vec<FillRecord> = self
            .fills
            .get(user_id)
            .map(|entry| {
                entry
                    .iter()
                    .filter(|f| symbol.is_none_or(|s| f.symbol.eq_ignore_ascii_case(s)))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        found.sort_by_key(|a| std::cmp::Reverse(a.at));
        found.truncate(limit);
        found
    }

    /// Mint a fill id.
    pub fn next_fill_id(&self) -> String {
        format!(
            "fll_{:012x}",
            self.sequence
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )
    }

    /// Drop settled orders and spent reservations older than the idempotency
    /// window, so a long-running gateway does not grow without bound.
    pub fn prune(&self, now: DateTime<Utc>) {
        let cutoff = now - Duration::hours(IDEMPOTENCY_WINDOW_HOURS);
        let stale: Vec<H256> = self
            .orders
            .iter()
            .filter(|r| !r.status.is_live() && r.updated_at < cutoff)
            .map(|r| r.tx_hash)
            .collect();
        for tx_hash in stale {
            if let Some((_, record)) = self.orders.remove(&tx_hash) {
                if let Some(order_id) = record.order_id {
                    self.by_order_id.remove(&order_id);
                }
            }
        }
        self.cancels
            .retain(|_, cancel| cancel.resolved_at.is_none_or(|resolved| resolved >= cutoff));
        self.reservations.retain(|_, slot| match slot.try_lock() {
            Ok(guard) => guard.as_ref().is_none_or(|order| order.at >= cutoff),
            // Held right now, so it is certainly not stale.
            Err(_) => true,
        });
    }

    /// How many orders are tracked. For `/health` and tests.
    pub fn len(&self) -> usize {
        self.orders.len()
    }

    /// Whether anything is tracked.
    pub fn is_empty(&self) -> bool {
        self.orders.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_types::{AssetId, MarketId};

    fn address(byte: u8) -> Address {
        Address([byte; 20])
    }

    fn record(user: &str, tx: u8) -> OrderRecord {
        OrderRecord {
            order_id: None,
            tx_hash: H256([tx; 32]),
            user_id: user.into(),
            address: address(tx),
            symbol: "BTC-PERP".into(),
            market: MarketRef::Deriv(cb_types::DerivId::perp(0)),
            side: Side::Buy,
            order_type: "limit",
            price: 6_400_000_000_000,
            qty: 100_000_000,
            filled_qty: 0,
            avg_fill_price: None,
            status: OrderStatus::Pending,
            tif: Tif::Gtc,
            post_only: false,
            reduce_only: false,
            client_order_id: Some("cli_1".into()),
            nonce: 0,
            included_at: None,
            tx_index: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    /// The chain applied every one of these.
    fn applied(hashes: &[H256]) -> BlockVerdicts {
        let mut verdicts = BlockVerdicts::none();
        for hash in hashes {
            verdicts.record(*hash, Verdict::Applied);
        }
        verdicts
    }

    /// The chain refused every one of these.
    fn refused(hashes: &[H256]) -> BlockVerdicts {
        let mut verdicts = BlockVerdicts::none();
        for hash in hashes {
            verdicts.record(*hash, Verdict::Refused);
        }
        verdicts
    }

    fn block_with(txs: Vec<BlockTx>, height: u64) -> BlockSummary {
        BlockSummary {
            height,
            hash: H256([9; 32]),
            state_root: H256([8; 32]),
            timestamp_ms: 0,
            proposer: None,
            txs,
        }
    }

    fn place_tx(sender: Address, hash: H256, index: u32) -> BlockTx {
        BlockTx {
            applied: None,
            hash,
            sender,
            index,
            body: TxBody::PlacePerpOrder {
                market: cb_types::DerivId::perp(0),
                side: Side::Buy,
                price_q8: 1,
                qty: 1,
                tif: Tif::Gtc,
                post_only: false,
                reduce_only: false,
                client_order_id: H256([0; 32]),
            },
        }
    }

    #[test]
    fn the_order_id_layout_matches_the_engines() {
        // Pinned against `cb_block::executor::derive_order_id`, which is
        // private. If the engine's layout changes, this test is the tripwire.
        let id = derive_order_id(Address([0xab; 20]), 0x0102030405060708, 0x0a0b0c0d, 0);
        assert_eq!(&id.0[..20], &[0xab; 20]);
        assert_eq!(&id.0[20..28], &0x0102030405060708u64.to_be_bytes());
        assert_eq!(&id.0[28..32], &0x0a0b0c0du32.to_be_bytes());
        let with_intra = derive_order_id(Address([0xab; 20]), 1, 1, 3);
        assert_ne!(with_intra, derive_order_id(Address([0xab; 20]), 1, 1, 0));
    }

    #[test]
    fn a_pending_order_is_addressed_by_its_transaction_hash() {
        let tracker = OrderTracker::new();
        let order = record("usr_a", 1);
        let expected = format!("0x{}", hex::encode([1u8; 32]));
        tracker.record_placement(order);
        let found = tracker.find(&expected).expect("addressable while pending");
        assert_eq!(found.status, OrderStatus::Pending);
        assert_eq!(found.public_id(), expected);
    }

    #[test]
    fn inclusion_promotes_a_pending_order_and_gives_it_an_id() {
        let tracker = OrderTracker::new();
        let order = record("usr_a", 1);
        let (sender, tx_hash) = (order.address, order.tx_hash);
        tracker.record_placement(order);

        let block = block_with(vec![place_tx(sender, tx_hash, 3)], 184_203);
        tracker.apply_block(&block, &applied(&[tx_hash]), Utc::now());

        let found = tracker
            .find(&format!("0x{}", hex::encode(tx_hash.0)))
            .expect("found");
        assert_eq!(found.status, OrderStatus::Open);
        let order_id = found.order_id.expect("assigned");
        assert_eq!(order_id, derive_order_id(sender, 184_203, 3, 0));
        // ...and it is now reachable by the real id too.
        assert_eq!(
            tracker
                .find(&format!("0x{}", hex::encode(order_id.0)))
                .expect("by id")
                .tx_hash,
            tx_hash
        );
    }

    #[test]
    fn another_traders_order_in_the_same_block_is_ignored() {
        let tracker = OrderTracker::new();
        tracker.record_placement(record("usr_a", 1));
        // A bot's transaction, which the gateway never submitted.
        let block = block_with(vec![place_tx(address(200), H256([200; 32]), 0)], 10);
        tracker.apply_block(&block, &BlockVerdicts::none(), Utc::now());
        assert_eq!(tracker.len(), 1, "no record invented for foreign flow");
    }

    #[test]
    fn a_cancel_transaction_closes_the_order_it_names() {
        let tracker = OrderTracker::new();
        let order = record("usr_a", 1);
        let (sender, tx_hash) = (order.address, order.tx_hash);
        tracker.record_placement(order);
        tracker.apply_block(
            &block_with(vec![place_tx(sender, tx_hash, 0)], 10),
            &applied(&[tx_hash]),
            Utc::now(),
        );
        let order_id = tracker
            .find(&format!("0x{}", hex::encode(tx_hash.0)))
            .expect("found")
            .order_id
            .expect("assigned");

        let cancel = BlockTx {
            applied: None,
            hash: H256([2; 32]),
            sender,
            index: 0,
            body: TxBody::CancelPerpOrder { order_id },
        };
        tracker.apply_block(
            &block_with(vec![cancel], 11),
            &BlockVerdicts::none(),
            Utc::now(),
        );
        assert_eq!(
            tracker
                .find(&format!("0x{}", hex::encode(order_id.0)))
                .expect("found")
                .status,
            OrderStatus::Cancelled
        );
    }

    #[test]
    fn a_batch_cancel_closes_every_live_spot_order_of_that_sender() {
        let tracker = OrderTracker::new();
        let mut order = record("usr_a", 1);
        order.market = MarketRef::Spot(MarketId::BtcUsd);
        order.symbol = "BTC-USD".into();
        order.status = OrderStatus::Open;
        let sender = order.address;
        tracker.record_placement(order);

        let batch = BlockTx {
            applied: None,
            hash: H256([3; 32]),
            sender,
            index: 0,
            body: TxBody::BatchCancel {
                market: MarketId::BtcUsd,
            },
        };
        tracker.apply_block(
            &block_with(vec![batch], 12),
            &BlockVerdicts::none(),
            Utc::now(),
        );
        assert!(tracker.orders_for_user("usr_a", "open", None).is_empty());
        assert_eq!(tracker.orders_for_user("usr_a", "cancelled", None).len(), 1);
    }

    #[tokio::test]
    async fn one_client_order_id_places_exactly_one_order() {
        let tracker = OrderTracker::new();
        let now = Utc::now();

        let first = tracker.reserve("usr_a", Some("cli_1"), now).await;
        let Reserved::Fresh(reservation) = first else {
            panic!("the first caller must win the key");
        };
        tracker.record_placement(record("usr_a", 1));
        reservation.fulfil(H256([1; 32]), now);

        let second = tracker.reserve("usr_a", Some("cli_1"), now).await;
        match second {
            Reserved::Existing(order) => assert_eq!(order.tx_hash, H256([1; 32])),
            Reserved::Fresh(_) => panic!("a repeat id must not place a second order"),
        }
    }

    #[tokio::test]
    async fn two_users_may_share_a_client_order_id() {
        let tracker = OrderTracker::new();
        let now = Utc::now();
        let Reserved::Fresh(a) = tracker.reserve("usr_a", Some("cli_1"), now).await else {
            panic!("first");
        };
        tracker.record_placement(record("usr_a", 1));
        a.fulfil(H256([1; 32]), now);

        assert!(
            matches!(
                tracker.reserve("usr_b", Some("cli_1"), now).await,
                Reserved::Fresh(_)
            ),
            "the key is scoped to the user"
        );
    }

    #[tokio::test]
    async fn a_released_reservation_frees_the_key_for_a_retry() {
        let tracker = OrderTracker::new();
        let now = Utc::now();
        let Reserved::Fresh(reservation) = tracker.reserve("usr_a", Some("cli_1"), now).await
        else {
            panic!("first");
        };
        // The submission failed, so the key must not be poisoned for a day.
        reservation.release();
        assert!(matches!(
            tracker.reserve("usr_a", Some("cli_1"), now).await,
            Reserved::Fresh(_)
        ));
    }

    #[tokio::test]
    async fn an_expired_idempotency_key_may_be_reused() {
        let tracker = OrderTracker::new();
        let now = Utc::now();
        let Reserved::Fresh(reservation) = tracker.reserve("usr_a", Some("cli_1"), now).await
        else {
            panic!("first");
        };
        tracker.record_placement(record("usr_a", 1));
        reservation.fulfil(H256([1; 32]), now);

        let later = now + Duration::hours(IDEMPOTENCY_WINDOW_HOURS + 1);
        assert!(
            matches!(
                tracker.reserve("usr_a", Some("cli_1"), later).await,
                Reserved::Fresh(_)
            ),
            "the contract's window is 24 hours, not forever"
        );
    }

    #[tokio::test]
    async fn concurrent_requests_with_one_id_produce_one_order() {
        let tracker = OrderTracker::new();
        let now = Utc::now();
        let placed = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let tracker = Arc::clone(&tracker);
            let placed = Arc::clone(&placed);
            handles.push(tokio::spawn(async move {
                match tracker.reserve("usr_a", Some("cli_1"), now).await {
                    Reserved::Fresh(reservation) => {
                        let n = placed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        tracker.record_placement(record("usr_a", 1));
                        reservation.fulfil(H256([1; 32]), now);
                        n
                    }
                    Reserved::Existing(_) => usize::MAX,
                }
            }));
        }
        for handle in handles {
            handle.await.expect("join");
        }
        assert_eq!(
            placed.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "eight simultaneous requests, one order"
        );
    }

    #[test]
    fn fills_accumulate_into_the_order_and_the_users_history() {
        let tracker = OrderTracker::new();
        let order = record("usr_a", 1);
        let tx_hash = order.tx_hash;
        tracker.record_placement(order);

        tracker.apply_fill(
            tx_hash,
            FillRecord {
                fill_id: "fll_1".into(),
                order_id: "0x01".into(),
                user_id: "usr_a".into(),
                symbol: "BTC-PERP".into(),
                side: Side::Buy,
                price: 6_400_000_000_000,
                qty: 25_000_000,
                fee: 640_000,
                fee_asset: "USD".into(),
                role: "taker",
                realized_pnl: 0,
                at: Utc::now(),
                height: 10,
            },
        );

        let found = tracker
            .find(&format!("0x{}", hex::encode(tx_hash.0)))
            .expect("found");
        assert_eq!(found.filled_qty, 25_000_000);
        assert_eq!(found.remaining_qty(), 75_000_000);
        assert_eq!(found.status, OrderStatus::PartiallyFilled);
        assert_eq!(tracker.fills_for_user("usr_a", None, 10).len(), 1);
        assert!(tracker.fills_for_user("usr_b", None, 10).is_empty());
    }

    #[test]
    fn one_users_orders_and_fills_never_appear_under_another_user() {
        let tracker = OrderTracker::new();
        tracker.record_placement(record("usr_a", 1));
        tracker.record_placement(record("usr_b", 2));
        assert_eq!(tracker.orders_for_user("usr_a", "all", None).len(), 1);
        assert_eq!(
            tracker.orders_for_user("usr_a", "all", None)[0].user_id,
            "usr_a"
        );
    }

    #[test]
    fn the_status_filter_follows_the_contracts_vocabulary() {
        assert!(OrderStatus::Open.matches_filter("open"));
        assert!(OrderStatus::PartiallyFilled.matches_filter("open"));
        assert!(OrderStatus::Pending.matches_filter("open"));
        assert!(!OrderStatus::Filled.matches_filter("open"));
        assert!(OrderStatus::Filled.matches_filter("filled"));
        assert!(OrderStatus::Cancelled.matches_filter("cancelled"));
        assert!(OrderStatus::Rejected.matches_filter("cancelled"));
        assert!(OrderStatus::Filled.matches_filter("all"));
        assert!(!OrderStatus::Filled.matches_filter("nonsense"));
    }

    #[test]
    fn settled_orders_are_pruned_and_live_ones_are_kept() {
        let tracker = OrderTracker::new();
        let now = Utc::now();
        let mut old = record("usr_a", 1);
        old.status = OrderStatus::Filled;
        old.updated_at = now - Duration::hours(IDEMPOTENCY_WINDOW_HOURS + 1);
        tracker.record_placement(old);
        let mut live = record("usr_a", 2);
        live.status = OrderStatus::Open;
        live.updated_at = now - Duration::hours(IDEMPOTENCY_WINDOW_HOURS + 1);
        tracker.record_placement(live);

        tracker.prune(now);
        assert_eq!(tracker.len(), 1, "a live order is never pruned");
    }

    #[test]
    fn a_malformed_order_id_is_not_found_rather_than_a_panic() {
        let tracker = OrderTracker::new();
        assert!(tracker.find("nonsense").is_none());
        assert!(tracker.find("0x1234").is_none());
        assert!(tracker.find("").is_none());
    }

    #[test]
    fn a_placement_the_chain_refused_is_rejected_and_gets_no_id() {
        // Under BFT the block carries the refused transaction like any other;
        // the verdict is what tells it apart. A refused order has no on-chain
        // id, because the chain never issued one, so nothing is indexed under
        // the id it would have had.
        let tracker = OrderTracker::new();
        let order = record("usr_a", 1);
        let (sender, tx_hash) = (order.address, order.tx_hash);
        tracker.record_placement(order);

        let block = block_with(vec![place_tx(sender, tx_hash, 3)], 184_203);
        let changed = tracker.apply_block(&block, &refused(&[tx_hash]), Utc::now());
        assert_eq!(changed.len(), 1, "the owner is told");
        assert_eq!(changed[0].status, OrderStatus::Rejected);

        let found = tracker
            .find(&format!("0x{}", hex::encode(tx_hash.0)))
            .expect("found");
        assert_eq!(found.status, OrderStatus::Rejected);
        assert_eq!(found.included_at, Some(184_203));
        assert!(found.order_id.is_none());
        let would_have_been = derive_order_id(sender, 184_203, 3, 0);
        assert!(
            tracker
                .find(&format!("0x{}", hex::encode(would_have_been.0)))
                .is_none(),
            "no index entry for an id the chain never issued"
        );
        assert!(
            tracker.addresses_with_live_orders().is_empty(),
            "a rejected order is not live, so the watcher stops polling the account"
        );
        // Folding the same block again reports nothing.
        assert!(tracker
            .apply_block(&block, &refused(&[tx_hash]), Utc::now())
            .is_empty());
    }

    #[test]
    fn a_placement_without_a_verdict_stays_pending_until_the_nonce_is_read() {
        let tracker = OrderTracker::new();
        let mut order = record("usr_a", 1);
        order.nonce = 4;
        let (sender, tx_hash) = (order.address, order.tx_hash);
        tracker.record_placement(order);

        let block = block_with(vec![place_tx(sender, tx_hash, 2)], 50);
        assert!(
            tracker
                .apply_block(&block, &BlockVerdicts::none(), Utc::now())
                .is_empty(),
            "nothing to report: included is not a state the client sees"
        );
        let found = tracker
            .find(&format!("0x{}", hex::encode(tx_hash.0)))
            .expect("found");
        assert_eq!(found.status, OrderStatus::Pending);
        assert_eq!(found.included_at, Some(50));
        assert!(found.order_id.is_none());
        assert_eq!(
            tracker.addresses_with_live_orders().len(),
            1,
            "still live, so the account keeps being read"
        );

        // The chain's nonce has not moved past 4: refused.
        let mut other = record("usr_a", 2);
        other.address = sender;
        other.nonce = 5;
        let other_hash = other.tx_hash;
        tracker.record_placement(other);
        tracker.apply_block(
            &block_with(vec![place_tx(sender, other_hash, 0)], 51),
            &BlockVerdicts::none(),
            Utc::now(),
        );

        // A nonce of 5 answers for the first (4 < 5: applied) and not the
        // second (5 is not past 5: refused).
        let changed = tracker.reconcile(sender, 5, Utc::now());
        assert_eq!(changed.len(), 2);
        let first = tracker
            .find(&format!("0x{}", hex::encode(tx_hash.0)))
            .expect("found");
        assert_eq!(first.status, OrderStatus::Open);
        assert_eq!(first.order_id, Some(derive_order_id(sender, 50, 2, 0)));
        assert_eq!(
            tracker
                .find(&format!("0x{}", hex::encode(first.order_id.expect("id").0)))
                .expect("reachable by its id")
                .tx_hash,
            tx_hash
        );
        let second = tracker
            .find(&format!("0x{}", hex::encode(other_hash.0)))
            .expect("found");
        assert_eq!(second.status, OrderStatus::Rejected);
        assert!(second.order_id.is_none());

        // Reconciling again changes nothing.
        assert!(tracker.reconcile(sender, 5, Utc::now()).is_empty());
    }

    #[test]
    fn reconcile_leaves_an_order_that_has_not_reached_a_block_alone() {
        // The nonce says nothing about a transaction still in the mempool.
        let tracker = OrderTracker::new();
        let order = record("usr_a", 1);
        let sender = order.address;
        tracker.record_placement(order);
        assert!(tracker.reconcile(sender, 0, Utc::now()).is_empty());
        assert!(tracker.reconcile(sender, 9, Utc::now()).is_empty());
        assert_eq!(
            tracker
                .find(&format!("0x{}", hex::encode([1u8; 32])))
                .expect("found")
                .status,
            OrderStatus::Pending
        );
    }

    #[test]
    fn an_order_the_chain_answered_for_cannot_be_declared_dead_afterwards() {
        // The signer's hourly sweep gives up submissions it never saw
        // answered. A placement answered `applied` — here by a block's
        // verdict, in production also by a late `reconcile` — is resting on
        // the book, and a stale registry entry must not flip it to rejected.
        let tracker = OrderTracker::new();
        let order = record("usr_a", 1);
        let (sender, tx_hash) = (order.address, order.tx_hash);
        tracker.record_placement(order);
        let block = block_with(vec![place_tx(sender, tx_hash, 0)], 10);
        tracker.apply_block(&block, &applied(&[tx_hash]), Utc::now());

        assert!(tracker.mark_dead(&[tx_hash], Utc::now()).is_empty());
        assert_eq!(
            tracker
                .find(&format!("0x{}", hex::encode(tx_hash.0)))
                .expect("found")
                .status,
            OrderStatus::Open
        );
    }

    #[test]
    fn a_dead_submission_rejects_its_order_before_any_block_says_so() {
        let tracker = OrderTracker::new();
        let order = record("usr_a", 1);
        let (sender, tx_hash) = (order.address, order.tx_hash);
        tracker.record_placement(order);

        let changed = tracker.mark_dead(&[tx_hash, H256([42; 32])], Utc::now());
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].status, OrderStatus::Rejected);

        // The block that carried it arrives later — a refusal that was
        // declared early is not resurrected by inclusion, whatever the verdict.
        let block = block_with(vec![place_tx(sender, tx_hash, 0)], 10);
        assert!(tracker
            .apply_block(&block, &applied(&[tx_hash]), Utc::now())
            .is_empty());
        assert_eq!(
            tracker
                .find(&format!("0x{}", hex::encode(tx_hash.0)))
                .expect("found")
                .status,
            OrderStatus::Rejected
        );
        assert!(tracker.mark_dead(&[tx_hash], Utc::now()).is_empty(), "once");
    }

    #[test]
    fn a_cancel_the_chain_refused_leaves_the_order_open_and_says_so() {
        let tracker = OrderTracker::new();
        let order = record("usr_a", 1);
        let (sender, tx_hash) = (order.address, order.tx_hash);
        tracker.record_placement(order);
        tracker.apply_block(
            &block_with(vec![place_tx(sender, tx_hash, 0)], 10),
            &applied(&[tx_hash]),
            Utc::now(),
        );
        let order_id = tracker
            .find(&format!("0x{}", hex::encode(tx_hash.0)))
            .expect("found")
            .order_id
            .expect("assigned");

        let cancel_hash = H256([2; 32]);
        tracker.record_cancel(cancel_hash, sender, 1, CancelTarget::Order(order_id));
        let cancel = BlockTx {
            applied: None,
            hash: cancel_hash,
            sender,
            index: 0,
            body: TxBody::CancelPerpOrder { order_id },
        };
        let changed = tracker.apply_block(
            &block_with(vec![cancel.clone()], 11),
            &refused(&[cancel_hash]),
            Utc::now(),
        );
        assert_eq!(
            changed.len(),
            1,
            "the owner was told `cancelled` and must be corrected"
        );
        assert_eq!(changed[0].status, OrderStatus::Open);
        assert_eq!(
            tracker
                .find(&format!("0x{}", hex::encode(order_id.0)))
                .expect("found")
                .status,
            OrderStatus::Open
        );

        // The cancel is answered for once; the same block again is silent.
        assert!(tracker
            .apply_block(
                &block_with(vec![cancel], 11),
                &refused(&[cancel_hash]),
                Utc::now()
            )
            .is_empty());
    }

    #[test]
    fn a_cancel_without_a_verdict_waits_for_the_nonce() {
        let tracker = OrderTracker::new();
        let order = record("usr_a", 1);
        let (sender, tx_hash) = (order.address, order.tx_hash);
        tracker.record_placement(order);
        tracker.apply_block(
            &block_with(vec![place_tx(sender, tx_hash, 0)], 10),
            &applied(&[tx_hash]),
            Utc::now(),
        );
        let order_id = tracker
            .find(&format!("0x{}", hex::encode(tx_hash.0)))
            .expect("found")
            .order_id
            .expect("assigned");
        let cancel_hash = H256([2; 32]);
        tracker.record_cancel(cancel_hash, sender, 1, CancelTarget::Order(order_id));
        let cancel = BlockTx {
            applied: None,
            hash: cancel_hash,
            sender,
            index: 0,
            body: TxBody::CancelPerpOrder { order_id },
        };
        assert!(tracker
            .apply_block(
                &block_with(vec![cancel], 11),
                &BlockVerdicts::none(),
                Utc::now()
            )
            .is_empty());
        assert_eq!(
            tracker
                .find(&format!("0x{}", hex::encode(order_id.0)))
                .expect("found")
                .status,
            OrderStatus::Open,
            "not cancelled on inclusion alone"
        );
        let changed = tracker.reconcile(sender, 2, Utc::now());
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].status, OrderStatus::Cancelled);
    }

    #[test]
    fn a_refused_batch_cancel_reports_every_order_it_would_have_closed() {
        let tracker = OrderTracker::new();
        let mut order = record("usr_a", 1);
        order.market = MarketRef::Spot(MarketId::BtcUsd);
        order.symbol = "BTC-USD".into();
        order.status = OrderStatus::Open;
        let sender = order.address;
        tracker.record_placement(order);
        let batch_hash = H256([3; 32]);
        tracker.record_cancel(
            batch_hash,
            sender,
            1,
            CancelTarget::Market(MarketRef::Spot(MarketId::BtcUsd)),
        );
        let batch = BlockTx {
            applied: None,
            hash: batch_hash,
            sender,
            index: 0,
            body: TxBody::BatchCancel {
                market: MarketId::BtcUsd,
            },
        };
        let changed = tracker.apply_block(
            &block_with(vec![batch], 12),
            &refused(&[batch_hash]),
            Utc::now(),
        );
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].status, OrderStatus::Open);
        assert_eq!(tracker.orders_for_user("usr_a", "open", None).len(), 1);
    }

    #[test]
    fn addresses_with_live_orders_are_deduplicated() {
        let tracker = OrderTracker::new();
        let mut first = record("usr_a", 1);
        first.status = OrderStatus::Open;
        let mut second = record("usr_a", 1);
        second.tx_hash = H256([5; 32]);
        second.status = OrderStatus::Open;
        tracker.record_placement(first);
        tracker.record_placement(second);
        assert_eq!(tracker.addresses_with_live_orders().len(), 1);
        let _ = AssetId::Usd;
    }
}
