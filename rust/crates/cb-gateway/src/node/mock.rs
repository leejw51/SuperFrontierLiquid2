//! A scriptable [`NodeClient`](super::NodeClient) for tests.
//!
//! The gateway's whole test suite runs without a sequencer, which is what
//! makes it fast enough to run on every save. This double lets a test decide
//! what the chain says — balances, books, positions — and, just as
//! importantly, lets it make the chain *unavailable* so the `503` path is
//! exercised rather than assumed.
//!
//! It lives outside `#[cfg(test)]` because integration tests in `tests/` are
//! separate crates and cannot see a test-only module.

use super::types::*;
use super::NodeClient;
use async_trait::async_trait;
use cb_ports::{PortError, PortResult};
use cb_tx::{SignedTx, TxBody};
use cb_types::{Address, AssetId, DerivId, MarketId};
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::broadcast;

/// What the mock will say when asked.
#[derive(Debug, Default)]
pub struct MockState {
    /// Per-address account state.
    pub accounts: BTreeMap<Address, AccountSnapshot>,
    /// Per-market spot depth.
    pub spot_books: BTreeMap<MarketId, BookSnapshot>,
    /// Per-market derivative depth.
    pub deriv_books: BTreeMap<DerivId, BookSnapshot>,
    /// Per-address positions.
    pub positions: BTreeMap<Address, Vec<PositionSnapshot>>,
    /// Per-address margin pools.
    pub margins: BTreeMap<Address, MarginSnapshot>,
    /// Per-market mark prices.
    pub marks: BTreeMap<DerivId, MarkSnapshot>,
    /// Per-market oracle prices.
    pub oracles: BTreeMap<DerivId, OracleSnapshot>,
    /// Per-market funding.
    pub funding: BTreeMap<DerivId, FundingSnapshot>,
    /// Listed perpetuals.
    pub perp_markets: Vec<PerpMarketSnapshot>,
    /// Listed dated futures.
    pub future_markets: Vec<FutureMarketSnapshot>,
    /// Listed spot markets. Empty means the node lists none and the catalogue
    /// keeps its configured defaults.
    pub spot_markets: Vec<SpotMarketSnapshot>,
    /// Blocks by height.
    pub blocks: BTreeMap<u64, BlockSummary>,
    /// Every transaction the gateway submitted, newest last.
    pub submitted: Vec<SignedTx>,
    /// What `submit_tx` should answer. `None` means accept every submission.
    pub submit_reply: Option<SubmitOutcome>,
    /// Scripted answers, consumed one per submission before `submit_reply`
    /// applies. Lets a test say "reject the first, accept the second".
    pub submit_replies: std::collections::VecDeque<SubmitOutcome>,
    /// Transactions the executor refuses: [`MockNode::mine`] puts them in the
    /// block, as BFT ordering does, and applies nothing for them — no balance
    /// moves and, what the gateway reads its verdict from, the sender's nonce
    /// stays where it was.
    pub refused: std::collections::BTreeSet<cb_types::H256>,
    /// When set, every call fails with this error instead of answering.
    pub unavailable: Option<String>,
    /// When set, only `account` fails with this error; everything else
    /// answers. Lets a test make the one read the gateway takes its verdict
    /// from fail while blocks still arrive and submissions still land.
    pub account_read_error: Option<String>,
    /// How many of the next `account` reads fail, counted down one per read.
    /// A transient failure, where [`Self::account_read_error`] is a lasting
    /// one: the read the watcher takes its verdict from fails and the read a
    /// moment later succeeds.
    pub failing_account_reads: u32,
    /// Link state reported to `/health`.
    pub health: NodeHealth,
}

/// A [`NodeClient`](super::NodeClient) whose answers a test writes.
pub struct MockNode {
    state: Arc<Mutex<MockState>>,
    events: broadcast::Sender<NodeEvent>,
    chain_id: u32,
}

impl Default for MockNode {
    fn default() -> Self {
        Self::new()
    }
}

impl MockNode {
    /// A mock that is connected, has no markets and accepts every transaction.
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(1024);
        Self {
            state: Arc::new(Mutex::new(MockState {
                health: NodeHealth {
                    connected: true,
                    last_height: 1,
                    head_height: 1,
                    last_error: None,
                    chain_id: Some(1),
                },
                ..Default::default()
            })),
            events,
            chain_id: 1,
        }
    }

    /// Mutate the scripted state.
    pub fn edit(&self, f: impl FnOnce(&mut MockState)) {
        let mut guard = self.state.lock();
        f(&mut guard);
    }

    /// Read the scripted state.
    pub fn inspect<T>(&self, f: impl FnOnce(&MockState) -> T) -> T {
        let guard = self.state.lock();
        f(&guard)
    }

    /// Make every subsequent call fail as if the sequencer were down.
    pub fn take_offline(&self, reason: &str) {
        let mut guard = self.state.lock();
        guard.unavailable = Some(reason.to_string());
        guard.health.connected = false;
        guard.health.last_error = Some(reason.to_string());
    }

    /// Bring the sequencer back.
    pub fn bring_online(&self) {
        let mut guard = self.state.lock();
        guard.unavailable = None;
        guard.health.connected = true;
    }

    /// Give an address a free balance.
    pub fn fund(&self, address: Address, asset: AssetId, amount: u128) {
        let mut guard = self.state.lock();
        let account = guard.accounts.entry(address).or_default();
        *account.balances.entry(asset).or_insert(0) += amount;
    }

    /// Lock part of an address's balance, as a resting order does.
    ///
    /// The chain's model, which this mirrors: `balances` is the total and
    /// `locked` is the reserved part of it, so locking moves nothing.
    pub fn lock(&self, address: Address, asset: AssetId, amount: u128) {
        let mut guard = self.state.lock();
        let account = guard.accounts.entry(address).or_default();
        *account.locked.entry(asset).or_insert(0) += amount;
    }

    /// Refuse a transaction at execution: it will be carried by the next
    /// block and applied by nobody. See [`MockState::refused`].
    pub fn refuse(&self, hash: cb_types::H256) {
        self.state.lock().refused.insert(hash);
    }

    /// Build the block that carries everything submitted since the last call,
    /// and apply what it does to the mock's accounts.
    ///
    /// The gateway derives fills, ledger entries and liquidations by reading an
    /// account *after* a block and differencing it against what it saw before,
    /// so a test of any of that needs a block whose transactions and whose
    /// resulting balances agree. Building one by hand and forgetting to move
    /// the balance is how such a test passes while the derivation is wrong.
    ///
    /// Deposits and transfers move balances. Orders do not: the mock has no
    /// matching engine, and a test that wants a fill says so by editing the
    /// balances itself. Every applied transaction advances its sender's nonce,
    /// as the executor does, because that is what the gateway reads to learn
    /// whether the chain applied it; a transaction in [`MockState::refused`]
    /// is carried and advances nothing.
    pub fn mine(&self, height: u64) -> BlockSummary {
        let mut guard = self.state.lock();
        let submitted: Vec<SignedTx> = std::mem::take(&mut guard.submitted);
        let mut txs = Vec::new();

        for (index, signed) in submitted.into_iter().enumerate() {
            let Ok(sender) = signed.sender() else {
                continue;
            };
            let hash = signed.hash();
            if guard.refused.remove(&hash) {
                txs.push(crate::node::BlockTx {
                    applied: None,
                    hash,
                    sender,
                    index: index as u32,
                    body: signed.tx.body.clone(),
                });
                continue;
            }
            {
                let account = guard.accounts.entry(sender).or_default();
                account.nonce = account.nonce.max(signed.tx.nonce.saturating_add(1));
            }
            match &signed.tx.body {
                TxBody::Deposit {
                    user,
                    asset,
                    amount,
                    ..
                } => {
                    let account = guard.accounts.entry(*user).or_default();
                    *account.balances.entry(*asset).or_insert(0) += *amount;
                }
                TxBody::Transfer { to, asset, amount } => {
                    let from = guard.accounts.entry(sender).or_default();
                    let held = from.balances.entry(*asset).or_insert(0);
                    *held = held.saturating_sub(*amount);
                    let recipient = guard.accounts.entry(*to).or_default();
                    *recipient.balances.entry(*asset).or_insert(0) += *amount;
                }
                _ => {}
            }
            txs.push(crate::node::BlockTx {
                applied: None,
                hash,
                sender,
                index: index as u32,
                body: signed.tx.body.clone(),
            });
        }

        guard.health.last_height = guard.health.last_height.max(height);
        BlockSummary {
            height,
            hash: cb_types::H256([height as u8; 32]),
            state_root: cb_types::H256([1; 32]),
            timestamp_ms: 1_700_000_000_000 + height * 100,
            proposer: None,
            txs,
        }
    }

    /// Publish an event to every subscriber.
    pub fn emit(&self, event: NodeEvent) {
        let _ = self.events.send(event);
    }

    /// Every transaction submitted so far.
    pub fn submitted(&self) -> Vec<SignedTx> {
        self.state.lock().submitted.clone()
    }

    /// The bodies of every transaction submitted so far.
    pub fn submitted_bodies(&self) -> Vec<TxBody> {
        self.state
            .lock()
            .submitted
            .iter()
            .map(|t| t.tx.body.clone())
            .collect()
    }

    fn guard(&self) -> PortResult<parking_lot::MutexGuard<'_, MockState>> {
        let guard = self.state.lock();
        if let Some(reason) = &guard.unavailable {
            return Err(PortError::Unavailable(reason.clone()));
        }
        Ok(guard)
    }
}

#[async_trait]
impl NodeClient for MockNode {
    async fn chain_info(&self) -> PortResult<ChainInfo> {
        drop(self.guard()?);
        Ok(ChainInfo {
            chain_id: self.chain_id,
            denom: "USD".into(),
            head_height: self.state.lock().health.last_height,
            oldest_block: 1,
        })
    }

    async fn account(&self, address: Address) -> PortResult<AccountSnapshot> {
        let mut guard = self.guard()?;
        if let Some(reason) = &guard.account_read_error {
            return Err(PortError::Unavailable(reason.clone()));
        }
        if guard.failing_account_reads > 0 {
            guard.failing_account_reads -= 1;
            return Err(PortError::Unavailable(
                "scripted transient account read failure".into(),
            ));
        }
        Ok(guard.accounts.get(&address).cloned().unwrap_or_default())
    }

    async fn submit_tx(&self, tx: SignedTx) -> PortResult<SubmitOutcome> {
        let mut guard = self.guard()?;
        guard.submitted.push(tx);
        if let Some(scripted) = guard.submit_replies.pop_front() {
            return Ok(scripted);
        }
        Ok(guard
            .submit_reply
            .clone()
            .unwrap_or(SubmitOutcome::Accepted))
    }

    async fn spot_book(&self, market: MarketId, depth: u32) -> PortResult<BookSnapshot> {
        let guard = self.guard()?;
        Ok(truncate(
            guard.spot_books.get(&market).cloned().unwrap_or_default(),
            depth,
        ))
    }

    async fn deriv_book(&self, market: DerivId, depth: u32) -> PortResult<BookSnapshot> {
        let guard = self.guard()?;
        Ok(truncate(
            guard.deriv_books.get(&market).cloned().unwrap_or_default(),
            depth,
        ))
    }

    async fn positions(&self, address: Address) -> PortResult<Vec<PositionSnapshot>> {
        let guard = self.guard()?;
        Ok(guard.positions.get(&address).cloned().unwrap_or_default())
    }

    async fn margin_account(&self, address: Address) -> PortResult<MarginSnapshot> {
        let guard = self.guard()?;
        Ok(guard.margins.get(&address).cloned().unwrap_or_default())
    }

    async fn mark_price(&self, market: DerivId) -> PortResult<MarkSnapshot> {
        let guard = self.guard()?;
        Ok(guard.marks.get(&market).copied().unwrap_or_default())
    }

    async fn oracle_price(&self, market: DerivId) -> PortResult<OracleSnapshot> {
        let guard = self.guard()?;
        Ok(guard.oracles.get(&market).copied().unwrap_or_default())
    }

    async fn funding_index(&self, market: DerivId) -> PortResult<FundingSnapshot> {
        let guard = self.guard()?;
        Ok(guard.funding.get(&market).copied().unwrap_or_default())
    }

    async fn perp_markets(&self) -> PortResult<Vec<PerpMarketSnapshot>> {
        let guard = self.guard()?;
        Ok(guard.perp_markets.clone())
    }

    async fn future_markets(&self) -> PortResult<Vec<FutureMarketSnapshot>> {
        let guard = self.guard()?;
        Ok(guard.future_markets.clone())
    }

    async fn spot_markets(&self) -> PortResult<Vec<SpotMarketSnapshot>> {
        let guard = self.guard()?;
        Ok(guard.spot_markets.clone())
    }

    async fn block(&self, height: u64) -> PortResult<BlockSummary> {
        let guard = self.guard()?;
        guard
            .blocks
            .get(&height)
            .cloned()
            .ok_or_else(|| PortError::NotFound(format!("block {height}")))
    }

    fn subscribe(&self) -> broadcast::Receiver<NodeEvent> {
        self.events.subscribe()
    }

    fn health(&self) -> NodeHealth {
        self.state.lock().health.clone()
    }
}

fn truncate(mut book: BookSnapshot, depth: u32) -> BookSnapshot {
    let depth = depth as usize;
    book.bids.truncate(depth);
    book.asks.truncate(depth);
    book
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_offline_mock_fails_every_call_with_unavailable() {
        let node = MockNode::new();
        node.take_offline("restarting");
        assert!(matches!(
            node.account(Address([1u8; 20])).await,
            Err(PortError::Unavailable(_))
        ));
        assert!(matches!(
            node.perp_markets().await,
            Err(PortError::Unavailable(_))
        ));
        assert!(!node.health().connected);
    }

    #[tokio::test]
    async fn submitted_transactions_are_recorded_for_assertions() {
        let node = MockNode::new();
        let (sk, _pk) = cb_crypto::generate_keypair();
        let tx = SignedTx::sign_with(
            &sk,
            cb_tx::Transaction {
                nonce: 0,
                chain_id: 1,
                body: TxBody::Transfer {
                    to: Address([2u8; 20]),
                    asset: AssetId::Usd,
                    amount: 5,
                },
            },
        )
        .expect("sign");
        assert_eq!(
            node.submit_tx(tx).await.expect("accepted"),
            SubmitOutcome::Accepted
        );
        assert_eq!(node.submitted().len(), 1);
    }
}
