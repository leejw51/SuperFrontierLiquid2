//! The background loop that turns one block stream into everything else.
//!
//! ## One subscription, many consumers
//!
//! The gateway holds exactly one subscription against the node. Every block it
//! delivers drives, in order:
//!
//! 1. the **chain log** and the `chain` WebSocket channel;
//! 2. the **order tracker**, which promotes pending orders to open and closes
//!    cancelled ones — once the chain has answered for the transaction, not
//!    merely carried it (see [`verdicts_for`]);
//! 3. the **books** of every market anybody is watching, diffed into deltas;
//! 4. **trade prints** derived from those diffs and from each derivative
//!    market's published last trade, which feed the `trades:` channel and the
//!    candle aggregator;
//! 5. **account deltas** for the users who had something at stake in that
//!    block, which is where fills come from.
//!
//! ## Fills come from the user's own state, never from a trade print
//!
//! A trade print says a trade happened; it does not say whose. Attributing one
//! by looking at which orders were in the same block would be wrong in the
//! ordinary case — bot traders submit directly over RPC, so most blocks carry
//! flow the gateway never saw — and being wrong here means showing one user
//! another user's trade.
//!
//! So a fill is inferred from the **user's own position and balance changing**
//! across the block. That cannot attribute anybody else's activity to them,
//! because nobody else's activity moves their account. The cost is that the
//! maker/taker role and the exact fee are not recoverable, and they are
//! reported as the conservative reading rather than guessed.
//!
//! Both halves of the account matter, not just the position: a **spot** trade
//! changes balances and nothing else, and deriving fills from position deltas
//! alone left every spot order reading `open` for ever however much of it had
//! traded. Base and quote moving in opposite directions is a trade; a lock
//! moving in or out of `locked` while the totals hold still is an order resting
//! or being cancelled, and is not.
//!
//! ## What is inferred, and what is read
//!
//! The distinction runs through this module and is worth stating once.
//!
//! * **Read from the chain, and therefore fact:** balances, positions,
//!   collateral, and the transactions in a block. The ledger entries written
//!   here — a faucet grant, a transfer out, the matching transfer in, a
//!   liquidation charge — come from transactions the chain applied, with the
//!   balance the chain reports afterwards and the height they landed at. None
//!   of them is written when the gateway *submits* something: submission
//!   returns on mempool acceptance, and a ledger that records an intention as a
//!   movement is a ledger that lies whenever a transaction dies at block build.
//! * **Inferred, and labelled as such:** fills, their prices, the maker/taker
//!   role, and a spot market's trade prints. Each is documented where it is
//!   derived, with what it cannot recover.
//!
//! An inference needs something to difference against, and that is
//! [`AccountMirror`]. When there is nothing — at start-up, after a reconnect,
//! after a lagged subscription — the account is **seeded silently**: the
//! difference between "no knowledge" and an account that already held a
//! position is not a trade, and publishing it as one wrote a fabricated fill
//! for every open position on every restart.

use crate::decimal::to_port_decimal;
use crate::error::ApiResult;
use crate::marketdata::FundingPoint;
use crate::node::{
    AccountSnapshot, BlockSummary, BlockTx, MarginSnapshot, NodeClient, NodeEvent, PositionSnapshot,
};
use crate::state::AppState;
use crate::symbols::{InstrumentKind, MarketRef};
use crate::tracker::{BlockVerdicts, FillRecord, Verdict};
use crate::ws::{Channel, Envelope};
use cb_tx::TxBody;
use cb_types::{Address, AssetId, Side};
use chrono::{DateTime, Utc};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Follow the node's event stream until the gateway shuts down.
///
/// A lagging subscriber is resubscribed rather than left behind: the whole
/// point of this loop is that somebody is always reading the stream.
pub async fn run(state: Arc<AppState>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let mut events = state.node.subscribe();

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!(target: "cb_gateway::watcher", "block watcher stopping");
                    return;
                }
            }
            received = events.recv() => match received {
                Ok(NodeEvent::Block(block)) => {
                    if let Err(e) = on_block(&state, &block).await {
                        tracing::warn!(
                            target: "cb_gateway::watcher",
                            height = block.height,
                            error = %e,
                            "block processing failed; continuing with the next"
                        );
                    }
                }
                Ok(NodeEvent::Connected) => {
                    // A reconnect may have skipped blocks, so every cached
                    // book is suspect. Dropping them forces a fresh snapshot,
                    // which is also what every subscriber needs.
                    tracing::info!(target: "cb_gateway::watcher", "resynchronising after a reconnect");
                    state.mirror.clear();
                    // The catalogue's periodic refresh fires once at startup,
                    // before this connection exists, and then not for another
                    // period (30 s shipped). Until it succeeds every spot
                    // market carries the configured default tick, and a
                    // market order rounded to that tick is refused by the
                    // executor. Read the chain's listing as soon as it can be
                    // read.
                    if let Err(e) = state.markets.refresh(state.head_height()).await {
                        tracing::warn!(
                            target: "cb_gateway::watcher",
                            error = %e,
                            "market refresh after connecting failed; the periodic refresh will retry"
                        );
                    }
                }
                Ok(NodeEvent::Refused(refused)) => {
                    let now = state.clock.now();
                    for r in &refused {
                        tracing::info!(
                            target: "cb_gateway::watcher",
                            tx = %hex::encode(r.hash.0),
                            address = %hex::encode(r.sender.0),
                            nonce = r.nonce,
                            height = r.height,
                            reason = %r.reason,
                            "the sequencer refused a transaction and dropped it from the block"
                        );
                        // The node forgot the nonce it was expecting from this
                        // sender; the signer's copy would be one ahead of the
                        // chain, and the next submission would have to learn
                        // that the slow way, by being refused at admission.
                        state.submitter.invalidate(r.sender);
                    }
                    let hashes: Vec<_> = refused.iter().map(|r| r.hash).collect();
                    for record in state.tracker.mark_dead(&hashes, now) {
                        publish_order_update(&state, &record);
                    }
                }
                Ok(NodeEvent::Disconnected) | Ok(NodeEvent::Trade(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!(
                        target: "cb_gateway::watcher",
                        missed,
                        "the watcher fell behind the block stream; resynchronising"
                    );
                    state.mirror.clear();
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    // The bridge stopped. Resubscribing gives a receiver that
                    // works again once it comes back.
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    events = state.node.subscribe();
                }
            },
        }
    }
}

/// Everything one block causes.
pub async fn on_block(state: &AppState, block: &BlockSummary) -> ApiResult<()> {
    let now = state.clock.now();

    // 1. The chain log and the `chain` channel.
    {
        let catalog = state.markets.snapshot();
        state.chain_log.observe_with(block, &|market| {
            // A market the gateway does not list (ETH/USD, whose prices are
            // not representable) still has a name on chain, and a dashboard
            // row that says which market a transaction hit is better than a
            // dash.
            catalog
                .symbols
                .symbol_for(market)
                .map(str::to_string)
                .or_else(|| match market {
                    crate::symbols::MarketRef::Spot(id) => Some(crate::symbols::spot_symbol(id)),
                    crate::symbols::MarketRef::Deriv(id) => {
                        Some(format!("deriv:0x{}", hex::encode(id.0)))
                    }
                })
        });
    }
    if let Some(record) = state.chain_log.recent(1).into_iter().next() {
        state.hub.publish(
            &Channel::Chain,
            &crate::http::chain::block_envelope(&record),
        );
    }

    // 2. Order lifecycle. Every transition the block caused is published to
    // the order's owner, which is where `account/order_update` comes from.
    // Submissions the signer has proven dead are refused first, so a block
    // arriving afterwards cannot revive them.
    let verdicts = verdicts_for(state, block).await;
    for record in state.tracker.mark_dead(&state.submitter.take_dead(), now) {
        publish_order_update(state, &record);
    }
    for record in state.tracker.apply_block(block, &verdicts, now) {
        publish_order_update(state, &record);
    }

    // 3, 4. Books, deltas and trade prints for the markets worth looking at.
    let touched = markets_touched(state, block);
    for symbol in touched {
        if let Err(e) = refresh_market(state, &symbol, now).await {
            tracing::debug!(
                target: "cb_gateway::watcher",
                symbol = %symbol,
                error = %e,
                "market refresh failed"
            );
        }
    }

    // 5. Account deltas, and the fills they imply.
    let watched = accounts_to_check(state, block, &verdicts).await;
    for (user_id, address) in watched {
        if let Err(e) = refresh_account(state, &user_id, address, block, &verdicts, now).await {
            tracing::debug!(
                target: "cb_gateway::watcher",
                user = %user_id,
                error = %e,
                "account refresh failed"
            );
        }
    }

    Ok(())
}

/// Which markets need a fresh book after this block.
///
/// A market nobody is watching and nothing touched costs nothing to skip; a
/// market that a transaction in this block traded on has to be re-read whether
/// or not a browser is looking, because the candle history depends on it.
fn markets_touched(state: &AppState, block: &BlockSummary) -> Vec<String> {
    let catalog = state.markets.snapshot();
    let mut symbols: BTreeSet<String> = state.hub.watched_symbols().into_iter().collect();

    // The `ticker` channel is about every market at once and names none of
    // them, so it appears in no symbol subscription. A gateway that only
    // refreshed the markets somebody was watching a *book* on therefore never
    // produced a ticker for the rest, and a terminal subscribed to `ticker`
    // alone saw nothing move. Only derivative markets are added: they are the
    // ones that publish a last trade, which is what a print is derived from.
    if state.hub.has_subscribers(&Channel::Ticker) {
        for entry in catalog.symbols.entries() {
            if matches!(entry.kind, InstrumentKind::Perp | InstrumentKind::Future) {
                symbols.insert(entry.symbol.clone());
            }
        }
    }

    for tx in &block.txs {
        let market = match &tx.body {
            TxBody::PlaceOrder { market, .. } | TxBody::BatchCancel { market } => {
                Some(MarketRef::Spot(*market))
            }
            TxBody::PlacePerpOrder { market, .. }
            | TxBody::OpenPosition { market, .. }
            | TxBody::ClosePosition { market, .. }
            | TxBody::LiquidationTrigger { market, .. }
            | TxBody::FundingTick { market }
            | TxBody::FuturesSettle { market }
            | TxBody::OracleUpdate { market, .. } => Some(MarketRef::Deriv(*market)),
            _ => None,
        };
        if let Some(market) = market {
            if let Some(symbol) = catalog.symbols.symbol_for(market) {
                symbols.insert(symbol.to_string());
            }
        }
    }
    symbols.into_iter().collect()
}

/// Re-read one market's book, publish what changed, and print any trade.
async fn refresh_market(state: &AppState, symbol: &str, now: DateTime<Utc>) -> ApiResult<()> {
    let catalog = state.markets.snapshot();
    let Some(entry) = catalog.entry(symbol) else {
        return Ok(());
    };
    let Some(meta) = catalog.meta(symbol) else {
        return Ok(());
    };

    let previous = state.marketdata.book(symbol).unwrap_or_default();
    let book = crate::http::market::fetch_book(state, entry, state.settings.max_depth).await?;
    let delta = state.marketdata.observe_book(symbol, book.clone());

    if let Some(delta) = delta {
        let channel = Channel::OrderBook(symbol.to_string());
        if state.hub.has_subscribers(&channel) {
            let render = |levels: &[crate::node::BookLevel]| -> Vec<[String; 2]> {
                levels
                    .iter()
                    .map(|level| {
                        [
                            crate::decimal::format_scaled(
                                level.price,
                                entry.scales.price_decimals,
                                meta.price_decimals,
                            ),
                            crate::decimal::format_scaled(
                                level.qty,
                                entry.scales.qty_decimals,
                                meta.size_decimals,
                            ),
                        ]
                    })
                    .collect()
            };
            state.hub.publish(
                &channel,
                &Envelope::new(
                    channel.name(),
                    "delta",
                    json!({
                        "height": delta.height,
                        "bids": render(&delta.bids),
                        "asks": render(&delta.asks),
                    }),
                ),
            );
        }
    }

    // Derivative markets publish their last trade; spot markets do not, so
    // only the former can produce a price-exact print.
    if let MarketRef::Deriv(id) = entry.market {
        let (last_px, last_height) = last_trade_of(state, id, entry.kind).await;
        if let Some(trade) =
            state
                .marketdata
                .observe_last_trade(symbol, last_px, last_height, &previous, &book, now)
        {
            announce_trade(state, entry, meta, trade, now).await;
        }

        // Funding, when the index has ticked since we last looked.
        if entry.kind == InstrumentKind::Perp {
            if let Ok(index) = state.node.funding_index(id).await {
                let mark = state
                    .node
                    .mark_price(id)
                    .await
                    .map(|m| m.px_q8)
                    .unwrap_or(0);
                let oracle = state
                    .node
                    .oracle_price(id)
                    .await
                    .map(|o| o.px_q8)
                    .unwrap_or(0);
                state.marketdata.observe_funding(
                    symbol,
                    FundingPoint {
                        at: now,
                        rate_q9: index.rate_q9,
                        mark_px_q8: mark,
                        index_px_q8: oracle,
                        height: index.last_tick_height,
                    },
                );
            }
        }
    }

    Ok(())
}

async fn last_trade_of(
    state: &AppState,
    id: cb_types::DerivId,
    kind: InstrumentKind,
) -> (u128, u64) {
    match kind {
        InstrumentKind::Future => state
            .node
            .future_markets()
            .await
            .ok()
            .and_then(|markets| markets.into_iter().find(|m| m.id == id))
            .map(|m| (m.last_trade_px_q8, m.last_trade_height))
            .unwrap_or((0, 0)),
        _ => state
            .node
            .perp_markets()
            .await
            .ok()
            .and_then(|markets| markets.into_iter().find(|m| m.id == id))
            .map(|m| (m.last_trade_px_q8, m.last_trade_height))
            .unwrap_or((0, 0)),
    }
}

/// The last chain state the gateway saw for one account.
///
/// Every derived fact about a user — a fill, a spot trade, a liquidation, a
/// ledger entry — is a *difference* between one of these and the state after a
/// block. That is the only attribution the gateway can make honestly: nobody
/// else's trading moves this user's balances or positions, so a change in them
/// is theirs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountBaseline {
    /// The trading address this state belongs to.
    pub address: Address,
    /// Balances, locks and nonce.
    pub account: AccountSnapshot,
    /// Open derivative positions.
    pub positions: Vec<PositionSnapshot>,
    /// Cross-margin collateral.
    pub margin: MarginSnapshot,
}

impl AccountBaseline {
    /// Total holding of one asset: free plus locked.
    ///
    /// A resting spot order moves base or quote from free to locked without
    /// changing the total, which is exactly how a cancel is told apart from a
    /// fill.
    pub fn total(&self, asset: AssetId) -> u128 {
        self.account.balance(asset)
    }

    /// The open position on one market, if any.
    pub fn position(&self, market: cb_types::DerivId) -> Option<&PositionSnapshot> {
        self.positions.iter().find(|p| p.market == market)
    }
}

/// The last account state the gateway saw for each user it is watching.
///
/// Shared, rather than owned by the watcher task, because the submission path
/// **primes** it: `POST /orders` and `POST /transfers` already read the account
/// before they submit, and recording that read gives the next block a baseline
/// that predates the transaction. Without priming, a user's first watched block
/// is usually the block carrying their first order, the baseline is empty, and
/// the resulting "everything appeared at once" difference is not a fill — which
/// is why an unprimed account is seeded silently instead (§15 of the flow
/// review: a gateway restart used to invent a fill for every open position).
#[derive(Debug, Default)]
pub struct AccountMirror {
    users: parking_lot::Mutex<BTreeMap<String, AccountBaseline>>,
}

impl AccountMirror {
    /// An empty mirror.
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget everything, after a gap in the stream.
    ///
    /// Called on reconnect and on a lagged subscription. Everything the gateway
    /// believed about these accounts may have changed while it was not looking,
    /// so the next block re-seeds rather than differencing against stale state.
    pub fn clear(&self) {
        self.users.lock().clear();
    }

    /// The baseline for one user, if the mirror has one.
    pub fn baseline(&self, user_id: &str) -> Option<AccountBaseline> {
        self.users.lock().get(user_id).cloned()
    }

    /// Replace one user's baseline.
    pub fn record(&self, user_id: &str, baseline: AccountBaseline) {
        self.users.lock().insert(user_id.to_string(), baseline);
    }

    /// Record a baseline only if there is not one already.
    ///
    /// Returns whether it was stored. Never overwrites: a baseline the watcher
    /// wrote is newer than anything a request handler has read.
    pub fn prime(&self, user_id: &str, baseline: AccountBaseline) -> bool {
        use std::collections::btree_map::Entry;
        match self.users.lock().entry(user_id.to_string()) {
            Entry::Occupied(_) => false,
            Entry::Vacant(slot) => {
                slot.insert(baseline);
                true
            }
        }
    }

    /// Users the gateway last saw holding an open position, with their address.
    ///
    /// These are checked on every block whether or not they did anything: a
    /// liquidation is triggered by a keeper and a funding tick is sent by the
    /// proposer, so the two events that move a position holder's money without
    /// their participation are invisible to a watcher that only follows senders
    /// and resting orders (§17 of the flow review).
    pub fn holders_of_open_positions(&self) -> Vec<(String, Address)> {
        self.users
            .lock()
            .iter()
            .filter(|(_, baseline)| baseline.positions.iter().any(|p| p.qty > 0))
            .map(|(user_id, baseline)| (user_id.clone(), baseline.address))
            .collect()
    }

    /// How many accounts are mirrored.
    pub fn len(&self) -> usize {
        self.users.lock().len()
    }

    /// Whether anything is mirrored.
    pub fn is_empty(&self) -> bool {
        self.users.lock().is_empty()
    }
}

/// Answer for the gateway's own transactions in a block.
///
/// Under BFT ordering (ADR 0008) a block keeps every committed transaction
/// whether or not the executor applied it, and carries nothing that says
/// which. The chain does say it another way: an applied transaction consumes
/// its sender's nonce and a refused one does not. For every transaction in
/// the block that this process signed — which is every one the submitter
/// still holds in flight — the sender's account is read once, and the nonce
/// having moved past the transaction's own is the verdict. Bots' transactions
/// are not the gateway's to answer for and get none.
///
/// A sender whose account cannot be read right now gets no verdict either;
/// the tracker holds those as included-and-pending and `refresh_account`
/// resolves them on the next read, since a pending order keeps its address
/// watched.
///
/// `POST /orders` also calls this for the block it waited for, so a verdict
/// can be read twice for one block. The transitions it drives are guarded, so
/// the second read changes nothing; it costs one `getAccount`.
pub async fn verdicts_for(state: &AppState, block: &BlockSummary) -> BlockVerdicts {
    let mut verdicts = BlockVerdicts::none();
    let mut by_sender: BTreeMap<Address, Vec<(cb_types::H256, u64)>> = BTreeMap::new();
    for tx in &block.txs {
        let in_flight = state.submitter.in_flight(&tx.hash);
        // A BFT block says outright which of its transactions applied —
        // anybody's, not only the ones this gateway signed — and under BFT
        // a refused transaction consumes its nonce, so the nonce comparison
        // below would call it applied. The mask wins, and it is what keeps a
        // stranger's refused deposit or transfer out of a user's ledger.
        if let Some(applied) = tx.applied {
            let verdict = if applied {
                Verdict::Applied
            } else {
                Verdict::Refused
            };
            match (&verdict, &in_flight) {
                (Verdict::Refused, Some(in_flight)) => tracing::info!(
                    target: "cb_gateway::watcher",
                    height = block.height,
                    tx = %hex::encode(tx.hash.0),
                    address = %hex::encode(in_flight.address.0),
                    nonce = in_flight.nonce,
                    "the chain committed a transaction the gateway signed and the executor refused it"
                ),
                (Verdict::Refused, None) => tracing::debug!(
                    target: "cb_gateway::watcher",
                    height = block.height,
                    tx = %hex::encode(tx.hash.0),
                    sender = %hex::encode(tx.sender.0),
                    "a committed transaction the executor refused"
                ),
                _ => {}
            }
            verdicts.record(tx.hash, verdict);
            if in_flight.is_some() {
                state.submitter.settle(&tx.hash);
            }
            continue;
        }
        if let Some(in_flight) = in_flight {
            by_sender
                .entry(in_flight.address)
                .or_default()
                .push((tx.hash, in_flight.nonce));
        }
    }
    for (address, txs) in by_sender {
        let chain_nonce = match state.node.account(address).await {
            Ok(account) => account.nonce,
            Err(e) => {
                // Unresolved, not unknown: these are the gateway's own, and
                // nothing derived from this block may treat them as applied.
                for (hash, _nonce) in &txs {
                    verdicts.mark_unresolved(*hash);
                }
                tracing::debug!(
                    target: "cb_gateway::watcher",
                    height = block.height,
                    address = %hex::encode(address.0),
                    error = %e,
                    "cannot read the sender's nonce; its transactions stay unresolved"
                );
                continue;
            }
        };
        for (hash, nonce) in txs {
            let verdict = if chain_nonce > nonce {
                Verdict::Applied
            } else {
                Verdict::Refused
            };
            if verdict == Verdict::Refused {
                tracing::info!(
                    target: "cb_gateway::watcher",
                    height = block.height,
                    tx = %hex::encode(hash.0),
                    address = %hex::encode(address.0),
                    nonce,
                    chain_nonce,
                    "the chain committed a transaction the gateway signed and the executor refused it"
                );
            }
            verdicts.record(hash, verdict);
            state.submitter.settle(&hash);
        }
    }
    verdicts
}

/// Whether a transaction in a block actually applied.
///
/// Answered exactly for a transaction the gateway signed, from the verdicts
/// [`verdicts_for`] read off the sender's nonce. For anybody else's — a bot's
/// transfer into a gateway account, a keeper's liquidation trigger — the
/// gateway holds no nonce to compare and assumes the transaction applied,
/// which is the assumption the sequencer scheme made for every transaction
/// and the one gap a receipt on the block would close. Every derivation in
/// this module that turns a transaction into a user-visible record — a ledger
/// entry, a liquidation, a fill's attribution — goes through this predicate.
fn tx_applied(verdicts: &BlockVerdicts, tx: &BlockTx) -> bool {
    // One of the gateway's own whose sender could not be read is *not*
    // assumed applied: the assumption is for flow the gateway cannot know
    // about, and this is flow it knows it cannot answer for. A ledger row it
    // then misses is a gap; one written for a refused transfer is a lie.
    if verdicts.is_unresolved(&tx.hash) {
        return false;
    }
    verdicts.applied(&tx.hash).unwrap_or(true)
}

/// Which users had something at stake in this block.
///
/// A user with a live order **or an open position** can be affected by
/// anybody's transaction, so they are checked on every block. A user who sent a
/// transaction in this block is checked because they plainly changed, as is the
/// subject of a deposit, the recipient of a transfer and the victim of a
/// liquidation, none of whom is the sender. Everyone else is skipped.
///
/// The cost is worth an operator knowing: this is `O(live orders + position
/// holders)` per block, at three RPC calls each — account, positions, margin.
/// Position holders were not checked at all before, and the two events that
/// move their money without their participation, a liquidation and a funding
/// tick, were invisible because of it. Trading that number back down means
/// either a per-account query the node does not have, or accepting that a
/// liquidated account finds out late.
async fn accounts_to_check(
    state: &AppState,
    block: &BlockSummary,
    verdicts: &BlockVerdicts,
) -> Vec<(String, Address)> {
    let mut watched: BTreeSet<(String, Address)> = state
        .tracker
        .addresses_with_live_orders()
        .into_iter()
        .collect();
    watched.extend(state.mirror.holders_of_open_positions());

    // Addresses this block plainly moved: senders, and the subjects of a
    // deposit, an incoming transfer or a liquidation, who are not the sender.
    let mut candidates: BTreeSet<Address> = block.txs.iter().map(|tx| tx.sender).collect();
    for tx in block.txs.iter().filter(|tx| tx_applied(verdicts, tx)) {
        match &tx.body {
            TxBody::Deposit { user, .. } => {
                candidates.insert(*user);
            }
            TxBody::Transfer { to, .. } => {
                candidates.insert(*to);
            }
            TxBody::LiquidationTrigger { owner, .. } => {
                candidates.insert(*owner);
            }
            _ => {}
        }
    }
    // The addresses in a block are *trading* addresses, which are derived
    // rather than stored, so they are resolved through the auth service's
    // index rather than by a column lookup. Most of them are bots trading over
    // RPC, which the gateway has no account for; those resolve to nothing and
    // are skipped.
    for address in candidates {
        if let Ok(Some(user)) = state.auth.user_by_trading_address(&address).await {
            watched.insert((user.user_id, address));
        }
    }
    watched.into_iter().collect()
}

/// Re-read one account, publish what changed, and record what the chain did.
async fn refresh_account(
    state: &AppState,
    user_id: &str,
    address: Address,
    block: &BlockSummary,
    verdicts: &BlockVerdicts,
    now: DateTime<Utc>,
) -> ApiResult<()> {
    let current = AccountBaseline {
        address,
        account: state.node.account(address).await?,
        positions: state.node.positions(address).await.unwrap_or_default(),
        margin: state.node.margin_account(address).await.unwrap_or_default(),
    };

    // Anything of this account's that reached a block without a verdict —
    // the nonce could not be read at the time — is answered for by this read,
    // which is at or after that block. Done before the fills are derived so a
    // fill lands on an order that is known to be open.
    for record in state.tracker.reconcile(address, current.account.nonce, now) {
        // Answered, so no longer the signer's to wait on: left registered, the
        // hourly sweep would later give up a placement the chain applied.
        state.submitter.settle(&record.tx_hash);
        publish_order_update(state, &record);
    }

    // What the chain itself did to this account in this block. It is read from
    // the block's transactions rather than inferred from a difference, so it is
    // written whether or not there is a baseline to difference against — a
    // gateway that has just started still has to record the deposit it can see.
    record_chain_ledger(state, user_id, &current, block, verdicts).await;

    let Some(previous) = state.mirror.baseline(user_id) else {
        // No baseline: the gateway has just started, has just resynchronised,
        // or has never watched this account. The difference between "nothing"
        // and the account's whole existing state is not a fill, a transfer or a
        // liquidation, and publishing it as one puts events that never happened
        // into a user's history. Seed quietly instead. The cost is that an
        // event in the very first block after a resync is not derived; it is
        // still visible in the balances and positions the next block publishes,
        // which are read from the chain.
        tracing::debug!(
            target: "cb_gateway::watcher",
            user = %user_id,
            height = block.height,
            "seeding an account baseline; no history is inferred from the first sighting"
        );
        state.mirror.record(user_id, current);
        return Ok(());
    };

    if previous.account != current.account {
        publish_balances(state, user_id, &current.account);
    }

    // Markets where the engine, not this user, closed the position — held
    // against the transaction that triggered it, which the ledger entry is
    // keyed on.
    let liquidated: BTreeMap<cb_types::DerivId, &BlockTx> = block
        .txs
        .iter()
        .filter(|tx| tx_applied(verdicts, tx))
        .filter_map(|tx| match &tx.body {
            TxBody::LiquidationTrigger { owner, market } if *owner == address => {
                Some((*market, tx))
            }
            _ => None,
        })
        .collect();

    // A position whose size changed means a fill, and it is *this* user's fill
    // because nobody else's trading moves this user's position.
    for position in current.positions.iter() {
        let before = previous.position(position.market);
        let previous_qty = before.map(|p| p.qty).unwrap_or(0);
        if position.qty == previous_qty {
            continue;
        }
        if let Some(trigger) = liquidated.get(&position.market) {
            announce_liquidation(
                state,
                user_id,
                position,
                previous_qty,
                &previous,
                &current,
                trigger,
                block,
                now,
            )
            .await;
        } else {
            record_position_fill(state, user_id, position, previous_qty, before, block, now);
        }
        publish_position(state, user_id, position).await;
    }
    for stale in previous.positions.iter().filter(|p| p.qty > 0) {
        if !current.positions.iter().any(|p| p.market == stale.market) {
            // The row is gone entirely, which is a full close.
            let closed = PositionSnapshot {
                qty: 0,
                ..stale.clone()
            };
            if let Some(trigger) = liquidated.get(&stale.market) {
                announce_liquidation(
                    state, user_id, &closed, stale.qty, &previous, &current, trigger, block, now,
                )
                .await;
            } else {
                record_position_fill(state, user_id, &closed, stale.qty, Some(stale), block, now);
            }
            publish_position(state, user_id, &closed).await;
        }
    }

    // Spot fills, which move balances and nothing else.
    record_spot_fills(state, user_id, &previous, &current, block, verdicts, now).await;

    // Funding, which moves collateral and touches nothing else at all.
    record_funding(state, user_id, &previous, &current, block, verdicts).await;

    state.mirror.record(user_id, current);
    Ok(())
}

/// Write the value this block moved for one user into the off-chain ledger.
///
/// **Entries are written when the chain applies the transaction, never when the
/// gateway submits one.** `POST /transfers` used to append a `transfer_out`
/// immediately after submission, with a balance it had computed rather than
/// observed; a transaction that never landed left a permanent entry describing
/// a transfer that never happened and a balance that never existed (§20 of the
/// flow review). The ledger is what a person reads to answer "where did my
/// money go", so it records only what the chain did.
///
/// `entry_id` is derived from the transaction hash and the entry's own shape,
/// so replaying the chain — which a gateway started with `--start-height 1`
/// does on every boot — rewrites the same id and the store rejects the
/// duplicate instead of doubling the history.
async fn record_chain_ledger(
    state: &AppState,
    user_id: &str,
    current: &AccountBaseline,
    block: &BlockSummary,
    verdicts: &BlockVerdicts,
) {
    let mut entries = Vec::new();
    let address = current.address;

    for tx in block.txs.iter().filter(|tx| tx_applied(verdicts, tx)) {
        match &tx.body {
            TxBody::Transfer { to, asset, amount } => {
                if tx.sender == address {
                    entries.push(ledger_entry(
                        user_id,
                        "transfer_out",
                        *asset,
                        -signed_amount(*amount, *asset),
                        current,
                        tx,
                        block,
                        state.take_transfer_memo(&tx.hash),
                    ));
                }
                if *to == address {
                    // The recipient's half. The contract's `kind` vocabulary
                    // has had `transfer_in` in it from the start and nothing
                    // ever produced one.
                    entries.push(ledger_entry(
                        user_id,
                        "transfer_in",
                        *asset,
                        signed_amount(*amount, *asset),
                        current,
                        tx,
                        block,
                        None,
                    ));
                }
            }
            TxBody::Deposit {
                user,
                asset,
                amount,
                external_tx_ref,
            } if *user == address => {
                // The faucet's grant, recorded from the chain's own copy of it
                // rather than from the gateway's intention to grant. The claim
                // id travels on the transaction, so the entry can be matched
                // back to the faucet's record.
                entries.push(ledger_entry(
                    user_id,
                    "faucet",
                    *asset,
                    signed_amount(*amount, *asset),
                    current,
                    tx,
                    block,
                    // The claim id the faucet minted under, so a grant in this
                    // ledger can be matched to the claim that produced it.
                    Some(format!("faucet claim {external_tx_ref}")),
                ));
            }
            _ => {}
        }
    }

    append_ledger(state, entries).await;
}

/// Turn a base-unit amount into the ledger's decimal.
fn signed_amount(amount: u128, asset: AssetId) -> rust_decimal::Decimal {
    crate::decimal::to_port_decimal(amount, asset.decimals() as u32).unwrap_or_default()
}

/// Build one ledger entry from a transaction the chain applied.
#[allow(clippy::too_many_arguments)]
fn ledger_entry(
    user_id: &str,
    kind: &str,
    asset: AssetId,
    amount: rust_decimal::Decimal,
    current: &AccountBaseline,
    tx: &BlockTx,
    block: &BlockSummary,
    memo: Option<String>,
) -> cb_ports::LedgerEntry {
    cb_ports::LedgerEntry {
        entry_id: ledger_entry_id(&tx.hash, user_id, kind, asset.symbol()),
        user_id: user_id.to_string(),
        kind: kind.to_string(),
        asset: asset.symbol().to_string(),
        amount,
        // Observed, not computed: this is the free balance the chain reports
        // after the block that carried the transaction.
        balance_after: crate::decimal::to_port_decimal(
            current.account.balance(asset),
            asset.decimals() as u32,
        )
        .unwrap_or_default(),
        memo,
        tx_hash: Some(crate::http::format_hash(&tx.hash)),
        height: Some(block.height),
        // The block's own timestamp, not the gateway's clock. A gateway started
        // with `--start-height 1` replays the chain, and a movement dated by
        // when the gateway happened to look at it would put a year-old transfer
        // at the top of today's history.
        at: block_time(block),
    }
}

/// When a block was produced, as a time.
///
/// The sequencer stamps blocks in Unix **milliseconds**. A header whose stamp
/// is unrepresentable is not worth failing a ledger write over, so it falls
/// back to the epoch, which is visibly wrong rather than quietly plausible.
fn block_time(block: &BlockSummary) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(block.timestamp_ms as i64).unwrap_or_default()
}

/// A ledger id that is the same every time the same transaction is seen.
fn ledger_entry_id(tx_hash: &cb_types::H256, user_id: &str, kind: &str, asset: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(tx_hash.0);
    hasher.update(user_id.as_bytes());
    hasher.update(kind.as_bytes());
    hasher.update(asset.as_bytes());
    format!("led_{}", hex::encode(&hasher.finalize()[..16]))
}

/// Append entries, treating a duplicate as the replay it is.
///
/// One entry at a time rather than as a batch: the batch is atomic, and a
/// replayed block whose first entry already exists would otherwise abort the
/// entries after it that do not.
async fn append_ledger(state: &AppState, entries: Vec<cb_ports::LedgerEntry>) {
    for entry in entries {
        match state.db.ledger().append(&entry).await {
            Ok(()) => {}
            Err(cb_ports::PortError::Conflict(_)) => {
                tracing::trace!(
                    target: "cb_gateway::watcher",
                    entry = %entry.entry_id,
                    "ledger entry already recorded; this block has been seen before"
                );
            }
            Err(e) => tracing::warn!(
                target: "cb_gateway::watcher",
                error = %e,
                kind = %entry.kind,
                "ledger write failed"
            ),
        }
    }
}

/// Value this block moved into or out of an account for reasons that are not
/// trading: deposits and transfers.
///
/// Subtracted from the balance differences a spot fill is inferred from, so a
/// faucet claim landing in the same block as a trade does not read as a trade
/// at a wild price.
fn external_moves(
    block: &BlockSummary,
    verdicts: &BlockVerdicts,
    address: Address,
) -> BTreeMap<AssetId, i128> {
    let mut moves: BTreeMap<AssetId, i128> = BTreeMap::new();
    for tx in block.txs.iter().filter(|tx| tx_applied(verdicts, tx)) {
        match &tx.body {
            TxBody::Transfer { to, asset, amount } => {
                if tx.sender == address {
                    *moves.entry(*asset).or_default() -= *amount as i128;
                }
                if *to == address {
                    *moves.entry(*asset).or_default() += *amount as i128;
                }
            }
            TxBody::Deposit {
                user,
                asset,
                amount,
                ..
            } if *user == address => {
                *moves.entry(*asset).or_default() += *amount as i128;
            }
            _ => {}
        }
    }
    moves
}

/// Turn a spot market's balance movement into a fill on the user's own ledger.
///
/// A spot trade changes balances and nothing else — there is no position to
/// diff — so the fill that `OrderTracker` needs to move an order past `open`
/// was never derived at all: a filled spot order read as open forever, and
/// cancelling the phantom it left on screen took the block down with it (§10 of
/// the flow review).
///
/// The inference is the same one the derivative path makes, and it is sound for
/// the same reason: nobody else's trading moves this user's balances. Base and
/// quote moving in **opposite** directions is a trade; a resting order that is
/// merely cancelled moves value between `locked` and `free` and leaves both
/// totals alone. Deposits and transfers in the same block are netted out first.
///
/// What it cannot recover: the maker/taker role, the fee — which the engine has
/// already taken out of the amounts this price was recovered from, so charging
/// it again here would count it twice — and the individual executions when one
/// block filled an order several times, which arrive as one aggregate.
async fn record_spot_fills(
    state: &AppState,
    user_id: &str,
    previous: &AccountBaseline,
    current: &AccountBaseline,
    block: &BlockSummary,
    verdicts: &BlockVerdicts,
    now: DateTime<Utc>,
) {
    let catalog = state.markets.snapshot();

    let mut symbols: BTreeSet<String> = state
        .tracker
        .live_orders_for_user(user_id, None)
        .into_iter()
        .filter(|order| matches!(order.market, MarketRef::Spot(_)))
        .map(|order| order.symbol)
        .collect();
    for tx in block.txs.iter().filter(|tx| tx_applied(verdicts, tx)) {
        if tx.sender != current.address {
            continue;
        }
        if let TxBody::PlaceOrder { market, .. } = &tx.body {
            if let Some(symbol) = catalog.symbols.symbol_for(MarketRef::Spot(*market)) {
                symbols.insert(symbol.to_string());
            }
        }
    }
    if symbols.is_empty() {
        return;
    }

    let external = external_moves(block, verdicts, current.address);
    for symbol in symbols {
        let (Some(entry), Some(meta)) = (catalog.entry(&symbol), catalog.meta(&symbol)) else {
            continue;
        };
        if !matches!(entry.market, MarketRef::Spot(_)) || entry.base == entry.quote {
            continue;
        }

        let moved = |asset: AssetId| -> i128 {
            current.total(asset) as i128
                - previous.total(asset) as i128
                - external.get(&asset).copied().unwrap_or(0)
        };
        let base_delta = moved(entry.base);
        let quote_delta = moved(entry.quote);

        let (side, qty, notional) = match (base_delta, quote_delta) {
            (b, q) if b > 0 && q < 0 => (Side::Buy, b as u128, q.unsigned_abs()),
            (b, q) if b < 0 && q > 0 => (Side::Sell, b.unsigned_abs(), q as u128),
            // Same-direction or one-sided movement is not a trade on this
            // market: something else moved the balance and guessing which is
            // how a user ends up reading a fill that never happened.
            _ => continue,
        };
        if qty == 0 {
            continue;
        }
        // `notional = price * qty / 10^8` is the executor's spot arithmetic
        // (`cb_types::scales`), so the price falls out of the two amounts.
        let price = notional
            .saturating_mul(10u128.pow(cb_types::scales::SPOT_NOTIONAL_DIVISOR_DECIMALS as u32))
            / qty;
        if price == 0 {
            continue;
        }

        let order = state
            .tracker
            .live_orders_for_user(user_id, Some(&symbol))
            .into_iter()
            .find(|o| o.side == side)
            .map(|o| (o.public_id(), o.tx_hash));

        let fill = FillRecord {
            fill_id: state.tracker.next_fill_id(),
            order_id: order.as_ref().map(|(id, _)| id.clone()).unwrap_or_default(),
            user_id: user_id.to_string(),
            symbol: symbol.clone(),
            side,
            price,
            qty,
            // Already inside the amounts above. See the function docs.
            fee: 0,
            fee_asset: entry.quote.symbol().to_string(),
            role: "taker",
            realized_pnl: 0,
            at: now,
            height: block.height,
        };
        publish_fill(state, user_id, &fill, order.map(|(_, hash)| hash));

        // The trade itself is public: a spot market publishes no last trade
        // over RPC, so a fill the gateway derived is the only price it can ever
        // print for one. Both sides of a trade between two gateway accounts
        // derive the same print, so a print already recorded at this height and
        // price is left alone rather than counted twice.
        //
        // The price carries this account's fee or rebate, because it is
        // recovered from amounts the engine had already charged against: a
        // maker's print reads a rebate above the traded price and a taker's a
        // fee below it. That is one fee's width — single basis points on the
        // listed markets — and it is the closest a market with no published
        // last trade can be got to. It is stated here because the same number
        // feeds the candles.
        let trade = crate::node::Trade {
            symbol: symbol.clone(),
            price,
            qty,
            taker_side: side,
            height: block.height,
            at: now,
        };
        let already_printed = state
            .marketdata
            .trades(&symbol, 8)
            .iter()
            .any(|seen| seen.height == trade.height && seen.price == trade.price);
        if !already_printed {
            state.marketdata.record_trade(trade.clone());
            announce_trade(state, entry, meta, trade, now).await;
        }
    }
}

/// Record a funding payment or receipt against a position holder.
///
/// The second of the two events that move a position holder's money without
/// their participation (§17 of the flow review): a `FundingTick` is a system
/// transaction the proposer sends, the holder is neither its sender nor
/// anywhere else in the block, and it moves **only** `MarginAccount.collateral`
/// — no balance, no position size, nothing the rest of this module notices. The
/// contract's ledger has had a `funding` kind from the first revision and
/// nothing has ever written one.
///
/// The entry is one per block rather than one per market: the chain charges
/// funding into a single cross-margin pool, so two ticks landing together are
/// one collateral movement and splitting it between the markets would mean
/// inventing the split. The memo names the markets that ticked.
async fn record_funding(
    state: &AppState,
    user_id: &str,
    previous: &AccountBaseline,
    current: &AccountBaseline,
    block: &BlockSummary,
    verdicts: &BlockVerdicts,
) {
    let delta = current.margin.collateral as i128 - previous.margin.collateral as i128;
    if delta == 0 {
        return;
    }
    // Only when this block actually carried a funding tick for a market this
    // account holds. Collateral moves for plenty of other reasons — a fill, a
    // fee, a liquidation — and each of those is recorded by the code that can
    // tell what it was.
    let catalog = state.markets.snapshot();
    let mut ticked: Vec<String> = Vec::new();
    let mut trigger = None;
    for tx in block.txs.iter().filter(|tx| tx_applied(verdicts, tx)) {
        let TxBody::FundingTick { market } = &tx.body else {
            continue;
        };
        if !current
            .positions
            .iter()
            .any(|p| p.market == *market && p.qty > 0)
            && !previous
                .positions
                .iter()
                .any(|p| p.market == *market && p.qty > 0)
        {
            continue;
        }
        if let Some(entry) = catalog.symbols.by_market(MarketRef::Deriv(*market)) {
            ticked.push(entry.symbol.clone());
        }
        trigger.get_or_insert(tx);
    }
    let Some(trigger) = trigger else {
        return;
    };

    let amount = crate::decimal::to_port_decimal(delta.unsigned_abs(), 6).unwrap_or_default();
    let amount = if delta < 0 {
        amount
            .checked_mul(rust_decimal::Decimal::NEGATIVE_ONE)
            .unwrap_or_default()
    } else {
        amount
    };
    append_ledger(
        state,
        vec![cb_ports::LedgerEntry {
            entry_id: ledger_entry_id(&trigger.hash, user_id, "funding", AssetId::Usd.symbol()),
            user_id: user_id.to_string(),
            kind: "funding".into(),
            asset: AssetId::Usd.symbol().to_string(),
            amount,
            // Collateral, which is what funding is charged against, rather than
            // the free spot balance.
            balance_after: crate::decimal::to_port_decimal(current.margin.collateral, 6)
                .unwrap_or_default(),
            memo: Some(format!("funding on {}", ticked.join(", "))),
            tx_hash: Some(crate::http::format_hash(&trigger.hash)),
            height: Some(block.height),
            at: block_time(block),
        }],
    )
    .await;
}

/// Publish a `liquidation`, and record the charge, when the engine closed a
/// position rather than the user.
///
/// A liquidation is not a fill: the user placed no order, and reporting it as
/// one both misdescribes it and leaves the contract's `liquidation` frame —
/// which the browser implements and puts a dismissable toast behind — emitted
/// by nobody (§16 and §17 of the flow review). The dashboard's
/// `liquidations_24h` counts ledger entries of this kind, and this is what
/// writes them.
#[allow(clippy::too_many_arguments)]
async fn announce_liquidation(
    state: &AppState,
    user_id: &str,
    position: &PositionSnapshot,
    previous_qty: u128,
    previous: &AccountBaseline,
    current: &AccountBaseline,
    trigger: &BlockTx,
    block: &BlockSummary,
    now: DateTime<Utc>,
) {
    let catalog = state.markets.snapshot();
    let Some(entry) = catalog.symbols.by_market(MarketRef::Deriv(position.market)) else {
        return;
    };
    let meta = catalog.meta(&entry.symbol);
    let closed = previous_qty.saturating_sub(position.qty);
    let price = state
        .node
        .mark_price(position.market)
        .await
        .map(|m| m.px_q8)
        .unwrap_or(position.entry_px_q8);
    // What the account actually lost, as the chain reports it: the collateral
    // it no longer has. Negative, per the contract.
    let loss = current.margin.collateral as i128 - previous.margin.collateral as i128;

    state.hub.publish_to_user(
        user_id,
        &Envelope::new(
            "account",
            "liquidation",
            json!({
                "symbol": entry.symbol,
                "qty": crate::decimal::format_scaled(closed, entry.scales.qty_decimals, meta.map(|m| m.size_decimals).unwrap_or(4)),
                "price": crate::decimal::format_scaled(price, entry.scales.price_decimals, meta.map(|m| m.price_decimals).unwrap_or(2)),
                "loss": crate::decimal::format_scaled_signed(loss.min(0), 6, 2),
                "at": now.to_rfc3339(),
                "height": block.height,
            }),
        ),
    );

    let entry_id = ledger_entry_id(&trigger.hash, user_id, "liquidation", AssetId::Usd.symbol());
    append_ledger(
        state,
        vec![cb_ports::LedgerEntry {
            entry_id,
            user_id: user_id.to_string(),
            kind: "liquidation".into(),
            asset: AssetId::Usd.symbol().to_string(),
            amount: crate::decimal::to_port_decimal(loss.min(0).unsigned_abs(), 6)
                .unwrap_or_default()
                .checked_mul(rust_decimal::Decimal::NEGATIVE_ONE)
                .unwrap_or_default(),
            // Margin collateral, which is where a liquidation takes its charge
            // from, rather than the free spot balance.
            balance_after: crate::decimal::to_port_decimal(current.margin.collateral, 6)
                .unwrap_or_default(),
            memo: Some(format!("liquidation on {}", entry.symbol)),
            tx_hash: Some(crate::http::format_hash(&trigger.hash)),
            height: Some(block.height),
            at: block_time(block),
        }],
    )
    .await;

    tracing::info!(
        target: "cb_gateway::watcher",
        user = %user_id,
        symbol = %entry.symbol,
        height = block.height,
        "position liquidated"
    );
}

/// Turn a position size change into a fill on the user's own ledger.
#[allow(clippy::too_many_arguments)]
fn record_position_fill(
    state: &AppState,
    user_id: &str,
    position: &PositionSnapshot,
    previous_qty: u128,
    previous: Option<&PositionSnapshot>,
    block: &BlockSummary,
    now: DateTime<Utc>,
) {
    let catalog = state.markets.snapshot();
    let Some(entry) = catalog.symbols.by_market(MarketRef::Deriv(position.market)) else {
        return;
    };
    let delta = position.qty.abs_diff(previous_qty);
    if delta == 0 {
        return;
    }
    // Growing a long is a buy; shrinking it is a sell, and the reverse for a
    // short.
    let side = match (position.side, position.qty > previous_qty) {
        (Side::Buy, true) | (Side::Sell, false) => Side::Buy,
        (Side::Buy, false) | (Side::Sell, true) => Side::Sell,
    };
    // A growing position's entry price is volume weighted, so the fill price
    // is recoverable from it; a shrinking one fills at the mark, which the
    // gateway does not know exactly, so the entry price is the best available
    // reading and is documented as an approximation.
    let price = if position.qty > previous_qty {
        recover_fill_price(position, previous, delta)
    } else {
        position.entry_px_q8
    };

    let order = state
        .tracker
        .live_orders_for_user(user_id, Some(&entry.symbol))
        .into_iter()
        .find(|o| o.side == side)
        .map(|o| o.tx_hash);

    let fill = FillRecord {
        fill_id: state.tracker.next_fill_id(),
        order_id: String::new(),
        user_id: user_id.to_string(),
        symbol: entry.symbol.clone(),
        side,
        price,
        qty: delta,
        // Fees are charged inside the engine and not reported per fill. The
        // market's taker rate is the conservative reading.
        fee: crate::orders::notional_micro_usd(entry, price, delta).saturating_mul(
            catalog
                .meta(&entry.symbol)
                .map(|m| m.taker_fee_bps.max(0) as u128)
                .unwrap_or(0),
        ) / 10_000,
        fee_asset: AssetId::Usd.symbol().to_string(),
        // Maker and taker are indistinguishable from a position delta.
        role: "taker",
        // The engine does not report realised PnL per fill and the exit price
        // is an approximation, so a number here would be a guess. Zero is the
        // field's documented "not known" reading; the account's realised PnL
        // over a window comes from the chain, not from this.
        realized_pnl: 0,
        at: now,
        height: block.height,
    };
    publish_fill(state, user_id, &fill, order);
}

/// Record a fill against its order and tell the owner, on the socket.
///
/// The `fill` frame is rendered by the same function `GET /account/fills` uses,
/// so it carries every field the contract lists. The hand-built payload it
/// replaced omitted `realized_pnl`, which the browser requires, so every fill
/// notification was silently dropped (§16 of the flow review).
fn publish_fill(
    state: &AppState,
    user_id: &str,
    fill: &FillRecord,
    order_tx_hash: Option<cb_types::H256>,
) {
    let mut fill = fill.clone();
    if let Some(tx_hash) = order_tx_hash {
        if let Some(record) = state.tracker.find_by_tx_hash(tx_hash) {
            fill.order_id = record.public_id();
        }
    }
    let changed = state.tracker.apply_fill(
        order_tx_hash.unwrap_or(cb_types::H256([0; 32])),
        fill.clone(),
    );

    let catalog = state.markets.snapshot();
    let payload = crate::http::account::render_fill(&catalog, &fill);
    state.hub.publish_to_user(
        user_id,
        &Envelope::new(
            "account",
            "fill",
            serde_json::to_value(payload).unwrap_or_else(|_| json!({})),
        ),
    );
    if let Some(record) = changed {
        publish_order_update(state, &record);
    }
}

/// Tell an order's owner that it moved.
///
/// The contract has defined `order_update` since the first revision and nothing
/// ever sent one, so a client watching the socket never learned that its order
/// went `pending → open`, filled or was cancelled (§16 of the flow review).
pub fn publish_order_update(state: &AppState, record: &crate::tracker::OrderRecord) {
    let payload = crate::http::account::render_order(state, record);
    state.hub.publish_to_user(
        &record.user_id,
        &Envelope::new(
            "account",
            "order_update",
            serde_json::to_value(payload).unwrap_or_else(|_| json!({})),
        ),
    );
}

/// Back out the price a position grew at from its volume-weighted entry.
///
/// `new_entry * new_qty = old_entry * old_qty + fill_price * fill_qty`, so the
/// fill price falls out. Exact when the engine's weighting is exact, which it
/// is, and it degrades to the new entry price when there was no old position.
fn recover_fill_price(
    position: &PositionSnapshot,
    previous: Option<&PositionSnapshot>,
    delta: u128,
) -> u128 {
    let Some(previous) = previous.filter(|p| p.qty > 0) else {
        return position.entry_px_q8;
    };
    let new_notional = position.entry_px_q8.saturating_mul(position.qty);
    let old_notional = previous.entry_px_q8.saturating_mul(previous.qty);
    if delta == 0 || new_notional < old_notional {
        return position.entry_px_q8;
    }
    (new_notional - old_notional) / delta
}

/// Publish the account's balances.
///
/// The payload is the bare array the contract's `balance_update` carries and
/// the browser deserialises. It was an object wrapping that array, which is a
/// shape the client cannot read at all (§16 of the flow review).
fn publish_balances(state: &AppState, user_id: &str, account: &AccountSnapshot) {
    let mut assets: Vec<AssetId> = account.balances.keys().copied().collect();
    for asset in account.locked.keys() {
        if !assets.contains(asset) {
            assets.push(*asset);
        }
    }
    assets.sort_by_key(|a| a.as_u8());

    let balances: Vec<serde_json::Value> = assets
        .into_iter()
        .map(|asset| {
            let decimals = asset.decimals() as u32;
            let display = crate::symbols::asset_display_decimals(asset);
            json!({
                "asset": asset.symbol(),
                "free": crate::decimal::format_scaled(account.available(asset), decimals, display),
                "locked": crate::decimal::format_scaled(account.locked_of(asset), decimals, display),
                "total": crate::decimal::format_scaled(account.balance(asset), decimals, display),
            })
        })
        .collect();
    state.hub.publish_to_user(
        user_id,
        &Envelope::new("account", "balance_update", json!(balances)),
    );
}

/// Publish one position, in the shape `GET /account/positions` returns.
///
/// A closed position is published as a row with `qty` zero rather than not
/// published at all, because the client needs to be told to remove it.
async fn publish_position(state: &AppState, user_id: &str, position: &PositionSnapshot) {
    let Some(payload) = crate::http::account::render_position(state, position).await else {
        return;
    };
    state.hub.publish_to_user(
        user_id,
        &Envelope::new(
            "account",
            "position_update",
            serde_json::to_value(payload).unwrap_or_else(|_| json!({})),
        ),
    );
}

/// Publish a trade print: the `trades` channel, the candle it moves and the
/// ticker that summarises the market.
async fn announce_trade(
    state: &AppState,
    entry: &crate::symbols::MarketEntry,
    meta: &cb_ports::MarketMeta,
    trade: crate::node::Trade,
    now: DateTime<Utc>,
) {
    let symbol = &entry.symbol;
    let channel = Channel::Trades(symbol.clone());
    if state.hub.has_subscribers(&channel) {
        state.hub.publish(
            &channel,
            &Envelope::new(
                channel.name(),
                "trade",
                json!({
                    "price": crate::decimal::format_scaled(trade.price, entry.scales.price_decimals, meta.price_decimals),
                    "qty": crate::decimal::format_scaled(trade.qty, entry.scales.qty_decimals, meta.size_decimals),
                    "side": crate::orders::side_str(trade.taker_side),
                    "at": trade.at.to_rfc3339(),
                }),
            ),
        );
    }

    // Candles, from the same print.
    let price = to_port_decimal(trade.price, entry.scales.price_decimals);
    let qty = to_port_decimal(trade.qty, entry.scales.qty_decimals);
    if let (Some(price), Some(qty)) = (price, qty) {
        match state.candles.apply_trade(symbol, price, qty, now).await {
            Ok(changed) => {
                for bar in changed {
                    let channel = Channel::Candles(symbol.clone(), bar.interval.clone());
                    if state.hub.has_subscribers(&channel) {
                        state.hub.publish(
                            &channel,
                            &Envelope::new(
                                channel.name(),
                                "candle",
                                json!({
                                    "t": bar.open_time.to_rfc3339(),
                                    "o": crate::decimal::format_port_decimal(bar.open, meta.price_decimals),
                                    "h": crate::decimal::format_port_decimal(bar.high, meta.price_decimals),
                                    "l": crate::decimal::format_port_decimal(bar.low, meta.price_decimals),
                                    "c": crate::decimal::format_port_decimal(bar.close, meta.price_decimals),
                                    "v": crate::decimal::format_port_decimal(bar.volume, meta.size_decimals),
                                }),
                            ),
                        );
                    }
                }
            }
            Err(e) => tracing::debug!(
                target: "cb_gateway::watcher",
                symbol = %symbol,
                error = %e,
                "candle aggregation failed"
            ),
        }
    }

    // The ticker channel carries every market's summary, spot included: a
    // market that never emits one leaves the browser's quote frozen at the
    // value it was seeded with and permanently flagged stale.
    if state.hub.has_subscribers(&Channel::Ticker) {
        if let Some(payload) =
            crate::http::market::ticker_frame(state, entry, meta, Some(trade.price)).await
        {
            state.hub.publish(
                &Channel::Ticker,
                &Envelope::new("ticker", "update", payload),
            );
        }
    }
}

/// Send a fresh order-book snapshot to one connection.
///
/// Served from the cached book the deltas are diffed against, so a subscriber
/// never receives a delta relative to a book it was not given. It costs the
/// node nothing, however many browsers subscribe.
pub async fn send_book_snapshot(
    state: &AppState,
    connection: crate::ws::ConnectionId,
    symbol: &str,
) {
    let catalog = state.markets.snapshot();
    let Some(entry) = catalog.entry(symbol) else {
        state.hub.send(
            connection,
            &Envelope::error(
                crate::error::ErrorCode::NotFound,
                format!("no market `{symbol}` is listed"),
            ),
        );
        return;
    };
    let Some(meta) = catalog.meta(symbol) else {
        return;
    };

    let book = match state.marketdata.book(symbol) {
        Some(book) => book,
        None => match crate::http::market::fetch_book(state, entry, state.settings.max_depth).await
        {
            Ok(book) => {
                state.marketdata.observe_book(symbol, book.clone());
                book
            }
            Err(e) => {
                state.hub.send(
                    connection,
                    &Envelope::error(
                        crate::error::ErrorCode::NodeUnavailable,
                        format!("cannot read the book for {symbol}: {e}"),
                    ),
                );
                return;
            }
        },
    };

    let render = |levels: &[crate::node::BookLevel]| -> Vec<[String; 2]> {
        levels
            .iter()
            .map(|level| {
                [
                    crate::decimal::format_scaled(
                        level.price,
                        entry.scales.price_decimals,
                        meta.price_decimals,
                    ),
                    crate::decimal::format_scaled(
                        level.qty,
                        entry.scales.qty_decimals,
                        meta.size_decimals,
                    ),
                ]
            })
            .collect()
    };

    let channel = Channel::OrderBook(symbol.to_string());
    state.hub.send(
        connection,
        &Envelope::new(
            channel.name(),
            "snapshot",
            json!({
                "height": book.height,
                "bids": render(&book.bids),
                "asks": render(&book.asks),
            }),
        ),
    );
}
