//! SuperFrontierLiquid RPC server.

use crate::{
    conv,
    stream::{BlockBytesSubServer, BlockSubServer, RejectedLog, RejectedSubServer},
};
use capnp::capability::Promise;
use capnp_rpc::pry;
use cb_block::Block;
use cb_engine::book::{best_bid_ask, BookSnapshot};
use cb_schema::rpc_capnp;
use cb_state::{
    Accounts, Blocks, BookIndex, Database, FundingIndices, FuturesMarkets, LiquidationQueue,
    MarginAccounts, Markets, Meta, OracleState, Orders, PerpBookIndex, PerpMarkets, PerpOrders,
    Positions,
};
use cb_types::{DerivId, MarginAccount, MarketId, Side, H256};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// Hook the sequencer-side vote handler. The block-producer wires this with
/// its own `VotePool` mutex; replicas (which don't accept votes) leave it
/// `None` and the server returns an error if a follower mistakenly calls
/// `submitVote` on them.
pub trait VoteHandler: Send + Sync {
    /// Returns Ok(true) on quorum-formed, Ok(false) on accepted-without-quorum,
    /// Err(VoteRejection-as-string-pair) on validation failure.
    fn handle(&self, vote: cb_consensus::Vote) -> Result<VoteAccepted, (VoteCode, String)>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoteAccepted {
    Stored,
    QuorumFormed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoteCode {
    UnknownValidator,
    MalformedSig,
    SignerMismatch,
    StaleHeight,
    UnknownBlock,
    Duplicate,
    Internal,
}

impl VoteCode {
    fn to_capnp(self) -> rpc_capnp::VoteErrorCode {
        match self {
            VoteCode::UnknownValidator => rpc_capnp::VoteErrorCode::UnknownValidator,
            VoteCode::MalformedSig => rpc_capnp::VoteErrorCode::MalformedSig,
            VoteCode::SignerMismatch => rpc_capnp::VoteErrorCode::SignerMismatch,
            VoteCode::StaleHeight => rpc_capnp::VoteErrorCode::StaleHeight,
            VoteCode::UnknownBlock => rpc_capnp::VoteErrorCode::UnknownBlock,
            VoteCode::Duplicate => rpc_capnp::VoteErrorCode::Duplicate,
            VoteCode::Internal => rpc_capnp::VoteErrorCode::Internal,
        }
    }
}

/// Called with the hash *and canonical bytes* of every transaction this node
/// accepts from a local RPC client.
///
/// The peer-to-peer layer installs one so a locally submitted order is
/// announced to the network. Without it a node that is not the sequencer would
/// accept an order into a mempool nobody ever drains — which is precisely the
/// gap transaction gossip exists to close.
/// The bytes travel with the hash because gossip is announce-then-fetch: a
/// node that announces a transaction it cannot then serve leaves every peer
/// waiting out a request timeout, and is scored down by each of them.
pub type TxObserver = Arc<dyn Fn(H256, Vec<u8>) + Send + Sync>;

#[derive(Clone)]
pub struct SequencerServer {
    pub db: Database,
    pub notify: Arc<Notify>,
    /// Optional vote handler — `Some` on the leader, `None` on a pure replica
    /// node (which doesn't accept votes from anyone).
    pub vote_handler: Option<Arc<dyn VoteHandler>>,
    /// Optional hook fired for each accepted local transaction.
    pub tx_observer: Option<TxObserver>,
    /// Per-sender nonce expectations for transactions already in the mempool.
    ///
    /// `Some` on a node whose sequencer reconciles it after each block (see
    /// `cb_node::sequencer`). When it is `Some`, `submitTx` answers `badNonce`
    /// instead of taking a transaction the chain will refuse — which is what
    /// makes a gateway's cached nonce recoverable. `None` disables the gate
    /// entirely, for benchmarks and in-process tests that drive the mempool
    /// themselves.
    pub pending_nonces: Option<Arc<crate::admission::PendingNonces>>,
    /// The parallel verification pipeline. `Some` on a sequencer: `submitTx`
    /// queues the transaction and answers when the coordinator has admitted
    /// it (see [`crate::verify`]). `None` keeps the inline path — recovery on
    /// this connection's thread — for tests and benchmarks that build a
    /// server by hand.
    pub verifier: Option<Arc<crate::verify::Verifier>>,
    /// The sequencer's refusals, for `streamRejected`. `None` on a validator,
    /// where a refused transaction stays in the block with `applied` false.
    pub rejected: Option<Arc<RejectedLog>>,
}

impl SequencerServer {
    pub fn without_voting(db: Database, notify: Arc<Notify>) -> Self {
        Self {
            db,
            notify,
            vote_handler: None,
            tx_observer: None,
            pending_nonces: None,
            verifier: None,
            rejected: None,
        }
    }
}

/// Put an admission decision into a `submitTx` response.
fn write_admission(
    mut results: rpc_capnp::super_frontier_liquid::SubmitTxResults,
    decision: crate::verify::Admission,
) {
    match decision {
        crate::verify::Admission::Ok => results.get().init_result().set_ok(()),
        crate::verify::Admission::Rejected { code, message } => {
            let mut rej = results.get().init_result().init_rejected();
            rej.set_code(code);
            rej.set_message(message.as_str());
        }
    }
}

impl rpc_capnp::super_frontier_liquid::Server for SequencerServer {
    fn submit_tx(
        &mut self,
        params: rpc_capnp::super_frontier_liquid::SubmitTxParams,
        mut results: rpc_capnp::super_frontier_liquid::SubmitTxResults,
    ) -> Promise<(), capnp::Error> {
        let signed = pry!(conv::signed_tx_from_capnp(
            pry!(pry!(params.get()).get_tx())
        ));
        if let Err(e) = cb_tx::validate_static(&signed.tx) {
            let mut rej = results.get().init_result().init_rejected();
            rej.set_code(rpc_capnp::SubmitErrorCode::InvalidAmount);
            rej.set_message(format!("{e}").as_str());
            return Promise::ok(());
        }
        let ctx = crate::verify::AdmissionCtx {
            db: self.db.clone(),
            notify: self.notify.clone(),
            pending_nonces: self.pending_nonces.clone(),
            tx_observer: self.tx_observer.clone(),
        };
        // With a verifier, recovery happens on the pool and admission on the
        // coordinator, in arrival order; this thread only waits for the
        // answer. See `crate::verify` for why the order matters.
        if let Some(verifier) = &self.verifier {
            let outcome = match verifier.submit(signed) {
                Ok(rx) => rx,
                Err(rejected) => {
                    write_admission(results, rejected);
                    return Promise::ok(());
                }
            };
            return Promise::from_future(async move {
                match outcome.await {
                    Ok(decision) => {
                        write_admission(results, decision);
                        Ok(())
                    }
                    Err(_) => Err(capnp::Error::failed(
                        "the verifier stopped before answering".to_string(),
                    )),
                }
            });
        }
        // Inline path: recover here, admit here, answer now.
        let sender = match signed.sender() {
            Ok(s) => s,
            Err(_) => {
                let mut rej = results.get().init_result().init_rejected();
                rej.set_code(rpc_capnp::SubmitErrorCode::BadSignature);
                rej.set_message("signature recovery failed");
                return Promise::ok(());
            }
        };
        let chain_id = cb_state::Meta::new(&self.db).chain_id();
        let decision = crate::verify::admit(&ctx, chain_id, signed, sender);
        if matches!(decision, crate::verify::Admission::Ok) {
            self.notify.notify_waiters();
        }
        write_admission(results, decision);
        Promise::ok(())
    }

    fn get_account(
        &mut self,
        params: rpc_capnp::super_frontier_liquid::GetAccountParams,
        mut results: rpc_capnp::super_frontier_liquid::GetAccountResults,
    ) -> Promise<(), capnp::Error> {
        let addr = pry!(conv::address_from_capnp(
            pry!(pry!(params.get()).get_addr())
        ));
        let s = Accounts::new(&self.db).get(&addr).unwrap_or_default();
        pry!(conv::account_state_to_capnp(results.get().init_state(), &s));
        Promise::ok(())
    }

    fn get_block(
        &mut self,
        params: rpc_capnp::super_frontier_liquid::GetBlockParams,
        mut results: rpc_capnp::super_frontier_liquid::GetBlockResults,
    ) -> Promise<(), capnp::Error> {
        let h = pry!(params.get()).get_height();
        // Blocks leave the process only once they are on disk.
        match Blocks::new(&self.db.durable_view()).get(h) {
            Some(bytes) => {
                let block: Block = pry!(bincode::deserialize(&bytes)
                    .map_err(|e| capnp::Error::failed(format!("decode block: {e}"))));
                conv::block_to_capnp(results.get().init_block(), &block);
                Promise::ok(())
            }
            None => Promise::err(capnp::Error::failed(format!("no block at height {h}"))),
        }
    }

    fn get_order_book(
        &mut self,
        params: rpc_capnp::super_frontier_liquid::GetOrderBookParams,
        mut results: rpc_capnp::super_frontier_liquid::GetOrderBookResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let market = conv::market_from_capnp(pry!(p.get_market()));
        let depth = clamp_book_depth(p.get_depth());
        let snapshot = build_spot_book_snapshot(&self.db, market, depth);
        conv::spot_book_snapshot_to_capnp(results.get().init_snapshot(), &snapshot);
        Promise::ok(())
    }

    fn stream_blocks(
        &mut self,
        params: rpc_capnp::super_frontier_liquid::StreamBlocksParams,
        mut results: rpc_capnp::super_frontier_liquid::StreamBlocksResults,
    ) -> Promise<(), capnp::Error> {
        let from = pry!(params.get()).get_from_height();
        // A client asking for a height this node has pruned would otherwise
        // wait on a notify that never fires: the body is gone and will not
        // be re-emitted. Start it at the oldest body still held instead. The
        // gap is visible to the client through `chainInfo.oldestBlock`.
        let oldest = Blocks::new(&self.db).lowest_height().unwrap_or(from);
        let cursor = Arc::new(Mutex::new(from.max(oldest)));
        let cap: rpc_capnp::block_sub::Client = capnp_rpc::new_client(BlockSubServer {
            db: self.db.durable_view(),
            cursor,
            notify: self.notify.clone(),
        });
        results.get().set_sub(cap);
        Promise::ok(())
    }

    fn stream_blocks_raw(
        &mut self,
        params: rpc_capnp::super_frontier_liquid::StreamBlocksRawParams,
        mut results: rpc_capnp::super_frontier_liquid::StreamBlocksRawResults,
    ) -> Promise<(), capnp::Error> {
        let from = pry!(params.get()).get_from_height();
        let cursor = Arc::new(Mutex::new(from));
        let cap: rpc_capnp::block_bytes_sub::Client = capnp_rpc::new_client(BlockBytesSubServer {
            db: self.db.durable_view(),
            cursor,
            notify: self.notify.clone(),
        });
        results.get().set_sub(cap);
        Promise::ok(())
    }

    fn stream_market(
        &mut self,
        _params: rpc_capnp::super_frontier_liquid::StreamMarketParams,
        _results: rpc_capnp::super_frontier_liquid::StreamMarketResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::failed("streamMarket not implemented".into()))
    }

    fn chain_info(
        &mut self,
        _params: rpc_capnp::super_frontier_liquid::ChainInfoParams,
        mut results: rpc_capnp::super_frontier_liquid::ChainInfoResults,
    ) -> Promise<(), capnp::Error> {
        let meta = Meta::new(&self.db);
        let mut info = results.get().init_info();
        info.set_chain_id(meta.chain_id());
        info.set_denom(meta.denom().as_str());
        info.set_head_height(meta.head_height());
        info.set_oldest_block(Blocks::new(&self.db).lowest_height().unwrap_or(0));
        Promise::ok(())
    }

    fn submit_vote(
        &mut self,
        params: rpc_capnp::super_frontier_liquid::SubmitVoteParams,
        mut results: rpc_capnp::super_frontier_liquid::SubmitVoteResults,
    ) -> Promise<(), capnp::Error> {
        let vote = pry!(conv::vote_from_capnp(pry!(pry!(params.get()).get_vote())));
        let handler = match &self.vote_handler {
            Some(h) => h.clone(),
            None => {
                let mut rej = results.get().init_result().init_rejected();
                rej.set_code(rpc_capnp::VoteErrorCode::Internal);
                rej.set_message("this node is not configured to accept votes");
                return Promise::ok(());
            }
        };
        match handler.handle(vote) {
            Ok(VoteAccepted::Stored) => {
                results.get().init_result().set_accepted(());
            }
            Ok(VoteAccepted::QuorumFormed) => {
                results.get().init_result().set_quorum(());
                self.notify.notify_waiters();
            }
            Err((code, msg)) => {
                let mut rej = results.get().init_result().init_rejected();
                rej.set_code(code.to_capnp());
                rej.set_message(msg.as_str());
            }
        }
        Promise::ok(())
    }

    // ---------- Derivatives reads (M6) ----------

    fn get_positions(
        &mut self,
        params: rpc_capnp::super_frontier_liquid::GetPositionsParams,
        mut results: rpc_capnp::super_frontier_liquid::GetPositionsResults,
    ) -> Promise<(), capnp::Error> {
        let addr = pry!(conv::address_from_capnp(
            pry!(pry!(params.get()).get_addr())
        ));
        // `Positions::list_for_owner` walks the CF prefix `[owner]`. RocksDB
        // returns keys in lex (DerivId byte) order — that's our deterministic
        // response order.
        let positions = Positions::new(&self.db).list_for_owner(&addr);
        let mut out = results.get().init_positions(positions.len() as u32);
        for (i, p) in positions.iter().enumerate() {
            conv::position_to_capnp(out.reborrow().get(i as u32), p);
        }
        Promise::ok(())
    }

    fn get_margin_account(
        &mut self,
        params: rpc_capnp::super_frontier_liquid::GetMarginAccountParams,
        mut results: rpc_capnp::super_frontier_liquid::GetMarginAccountResults,
    ) -> Promise<(), capnp::Error> {
        let addr = pry!(conv::address_from_capnp(
            pry!(pry!(params.get()).get_addr())
        ));
        // Empty/unfunded callers get the default zero-collateral row so clients
        // don't have to special-case "account doesn't exist yet".
        let m = MarginAccounts::new(&self.db)
            .get(&addr)
            .unwrap_or_else(|| MarginAccount::new(addr));
        conv::margin_account_to_capnp(results.get().init_margin(), &m);
        Promise::ok(())
    }

    fn get_funding_index(
        &mut self,
        params: rpc_capnp::super_frontier_liquid::GetFundingIndexParams,
        mut results: rpc_capnp::super_frontier_liquid::GetFundingIndexResults,
    ) -> Promise<(), capnp::Error> {
        let market = pry!(conv::deriv_id_from_capnp(pry!(
            pry!(params.get()).get_market()
        )));
        let f = FundingIndices::new(&self.db)
            .get(&market)
            .unwrap_or_else(|| cb_types::FundingIndex::new(market));
        conv::funding_index_to_capnp(results.get().init_index(), &f);
        Promise::ok(())
    }

    fn get_oracle_price(
        &mut self,
        params: rpc_capnp::super_frontier_liquid::GetOraclePriceParams,
        mut results: rpc_capnp::super_frontier_liquid::GetOraclePriceResults,
    ) -> Promise<(), capnp::Error> {
        let market = pry!(conv::deriv_id_from_capnp(pry!(
            pry!(params.get()).get_market()
        )));
        match OracleState::new(&self.db).get_oracle(&market) {
            Some(o) => conv::oracle_price_to_capnp(results.get().init_oracle(), &o),
            None => {
                return Promise::err(capnp::Error::failed(format!(
                    "no oracle price for {market}",
                )))
            }
        }
        Promise::ok(())
    }

    fn get_mark_price(
        &mut self,
        params: rpc_capnp::super_frontier_liquid::GetMarkPriceParams,
        mut results: rpc_capnp::super_frontier_liquid::GetMarkPriceResults,
    ) -> Promise<(), capnp::Error> {
        let market = pry!(conv::deriv_id_from_capnp(pry!(
            pry!(params.get()).get_market()
        )));
        match OracleState::new(&self.db).get_mark(&market) {
            Some(m) => conv::mark_price_to_capnp(results.get().init_mark(), &m),
            None => {
                return Promise::err(capnp::Error::failed(format!("no mark price for {market}",)))
            }
        }
        Promise::ok(())
    }

    fn get_perp_market(
        &mut self,
        params: rpc_capnp::super_frontier_liquid::GetPerpMarketParams,
        mut results: rpc_capnp::super_frontier_liquid::GetPerpMarketResults,
    ) -> Promise<(), capnp::Error> {
        let market = pry!(conv::deriv_id_from_capnp(pry!(
            pry!(params.get()).get_market()
        )));
        let m = match PerpMarkets::new(&self.db).get(&market) {
            Some(m) => m,
            None => {
                return Promise::err(capnp::Error::failed(format!(
                    "unknown perp market {market}",
                )))
            }
        };
        conv::perp_market_to_capnp(results.get().init_perp(), &m);
        Promise::ok(())
    }

    fn list_perp_markets(
        &mut self,
        _params: rpc_capnp::super_frontier_liquid::ListPerpMarketsParams,
        mut results: rpc_capnp::super_frontier_liquid::ListPerpMarketsResults,
    ) -> Promise<(), capnp::Error> {
        // `PerpMarkets::list` returns RocksDB lex order = DerivId byte order =
        // ascending market index. Deterministic.
        let markets = PerpMarkets::new(&self.db).list();
        let mut out = results.get().init_markets(markets.len() as u32);
        for (i, m) in markets.iter().enumerate() {
            conv::perp_market_to_capnp(out.reborrow().get(i as u32), m);
        }
        Promise::ok(())
    }

    fn get_future_market(
        &mut self,
        params: rpc_capnp::super_frontier_liquid::GetFutureMarketParams,
        mut results: rpc_capnp::super_frontier_liquid::GetFutureMarketResults,
    ) -> Promise<(), capnp::Error> {
        let market = pry!(conv::deriv_id_from_capnp(pry!(
            pry!(params.get()).get_market()
        )));
        let m = match FuturesMarkets::new(&self.db).get(&market) {
            Some(m) => m,
            None => {
                return Promise::err(capnp::Error::failed(format!(
                    "unknown future market {market}",
                )))
            }
        };
        conv::future_market_to_capnp(results.get().init_future(), &m);
        Promise::ok(())
    }

    fn list_future_markets(
        &mut self,
        _params: rpc_capnp::super_frontier_liquid::ListFutureMarketsParams,
        mut results: rpc_capnp::super_frontier_liquid::ListFutureMarketsResults,
    ) -> Promise<(), capnp::Error> {
        let markets = FuturesMarkets::new(&self.db).list();
        let mut out = results.get().init_markets(markets.len() as u32);
        for (i, m) in markets.iter().enumerate() {
            conv::future_market_to_capnp(out.reborrow().get(i as u32), m);
        }
        Promise::ok(())
    }

    fn get_liquidation_queue(
        &mut self,
        params: rpc_capnp::super_frontier_liquid::GetLiquidationQueueParams,
        mut results: rpc_capnp::super_frontier_liquid::GetLiquidationQueueResults,
    ) -> Promise<(), capnp::Error> {
        let market = pry!(conv::deriv_id_from_capnp(pry!(
            pry!(params.get()).get_market()
        )));
        // The CF is keyed `(height, seq)` — a global queue across all markets.
        // For this read we surface every entry whose `market == requested`
        // ordered by `(height, seq)`. RocksDB iter walks the whole CF in BE
        // height/seq order, which is exactly the deterministic order the M4
        // drain step uses.
        let q = LiquidationQueue::new(&self.db);
        let mut all: Vec<cb_types::LiquidationEntry> = Vec::new();
        if let Ok(it) = self.db.iter_prefix(cb_state::CF_LIQUIDATION_QUEUE, &[]) {
            for kv in it {
                let (_k, v) = match kv {
                    Ok(p) => p,
                    Err(_) => break,
                };
                if let Ok(e) = bincode::deserialize::<cb_types::LiquidationEntry>(&v) {
                    if e.market == market {
                        all.push(e);
                    }
                }
            }
        }
        // Tolerate the unused binding for crates without M4 in scope.
        let _ = &q;
        let mut out = results.get().init_entries(all.len() as u32);
        for (i, e) in all.iter().enumerate() {
            conv::liquidation_entry_to_capnp(out.reborrow().get(i as u32), e);
        }
        Promise::ok(())
    }

    fn get_perp_order_book(
        &mut self,
        params: rpc_capnp::super_frontier_liquid::GetPerpOrderBookParams,
        mut results: rpc_capnp::super_frontier_liquid::GetPerpOrderBookResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let market = pry!(conv::deriv_id_from_capnp(pry!(p.get_market())));
        let depth = clamp_book_depth(p.get_depth());
        let snapshot = build_perp_book_snapshot(&self.db, market, depth);
        conv::perp_book_snapshot_to_capnp(results.get().init_snapshot(), &snapshot);
        Promise::ok(())
    }

    fn stream_rejected(
        &mut self,
        params: rpc_capnp::super_frontier_liquid::StreamRejectedParams,
        mut results: rpc_capnp::super_frontier_liquid::StreamRejectedResults,
    ) -> Promise<(), capnp::Error> {
        let Some(log) = self.rejected.clone() else {
            return Promise::err(capnp::Error::unimplemented(
                "this node does not publish refused transactions; under BFT a refused \
                 transaction stays in the block with `applied` false"
                    .into(),
            ));
        };
        let from = pry!(params.get()).get_from_seq();
        // 0 means "from now": a subscriber that just connected is not told
        // about refusals that predate it, which it can only reconcile from
        // nonces anyway.
        let start = if from == 0 { log.next_seq() } else { from };
        let cap: rpc_capnp::rejected_sub::Client = capnp_rpc::new_client(RejectedSubServer {
            log,
            cursor: Arc::new(Mutex::new(start)),
        });
        results.get().set_sub(cap);
        Promise::ok(())
    }

    fn list_spot_markets(
        &mut self,
        _params: rpc_capnp::super_frontier_liquid::ListSpotMarketsParams,
        mut results: rpc_capnp::super_frontier_liquid::ListSpotMarketsResults,
    ) -> Promise<(), capnp::Error> {
        // `MarketId::ALL` is the enum's declaration order, so the listing is
        // deterministic. A market genesis did not configure is simply absent;
        // the executor would refuse an order on it anyway (`MarketNotFound`).
        let markets_cf = Markets::new(&self.db);
        let markets: Vec<_> = MarketId::ALL
            .iter()
            .filter_map(|m| markets_cf.get(*m))
            .collect();
        let mut out = results.get().init_markets(markets.len() as u32);
        for (i, m) in markets.iter().enumerate() {
            conv::spot_market_to_capnp(out.reborrow().get(i as u32), m);
        }
        Promise::ok(())
    }
}

/// The most price levels per side a book query will return.
///
/// `depth` is a client's `UInt32`, and the spot index reserves a vector of
/// that many levels before it has read one: `depth = u32::MAX` asked for
/// roughly 137 GB, which aborts the process on the allocation. One
/// unauthenticated request took the node down. The gateway serves at most
/// 200; a thousand is well past any honest reader.
pub const MAX_BOOK_DEPTH: u32 = 1_000;

/// Hold a requested depth to [`MAX_BOOK_DEPTH`]. Zero passes through: on the
/// spot book it means no levels, on the perp book the whole (capped) side.
pub fn clamp_book_depth(depth: u32) -> u32 {
    depth.min(MAX_BOOK_DEPTH)
}

/// Aggregate the perp book index into price levels, capped at `depth` per
/// side. We aggregate via `PerpOrders` so each level's reported `qty` is the
/// sum of `remaining_qty` of the resting orders at that price — the same view
/// `book::best_bid_ask` exposes for top-of-book.
fn build_perp_book_snapshot(
    db: &Database,
    market: DerivId,
    depth: u32,
) -> conv::PerpOrderBookSnapshot {
    let book = PerpBookIndex::new(db);
    let orders = PerpOrders::new(db);
    let head = Meta::new(db).head_height();

    fn aggregate(
        side_entries: Vec<(u128, u64, H256)>,
        orders: &PerpOrders<'_>,
        descending: bool,
        depth: u32,
    ) -> Vec<conv::PerpBookLevel> {
        // Walk in (price, seq) ascending; reverse later if `descending`.
        let mut by_px: std::collections::BTreeMap<u128, u128> = std::collections::BTreeMap::new();
        for (px, _seq, oid) in side_entries {
            let qty = match orders.get(&oid) {
                Some(o) => o.remaining_qty,
                None => continue,
            };
            *by_px.entry(px).or_insert(0) =
                by_px.get(&px).copied().unwrap_or(0).saturating_add(qty);
        }
        let mut levels: Vec<conv::PerpBookLevel> = by_px
            .into_iter()
            .map(|(price_q8, qty)| conv::PerpBookLevel { price_q8, qty })
            .collect();
        if descending {
            levels.reverse();
        }
        // Zero has always meant "the whole side" here, and still does, up to
        // the same ceiling every other depth is held to.
        let cap = if depth == 0 { MAX_BOOK_DEPTH } else { depth };
        levels.truncate(cap as usize);
        levels
    }

    // Bids: best = highest price first.
    let bid_entries = book.list_side(&market, Side::Buy);
    let bids = aggregate(bid_entries, &orders, /*descending=*/ true, depth);
    // Asks: best = lowest price first; ascending lex order is already correct.
    let ask_entries = book.list_side(&market, Side::Sell);
    let asks = aggregate(ask_entries, &orders, /*descending=*/ false, depth);

    conv::PerpOrderBookSnapshot {
        market,
        bids,
        asks,
        as_of_height: head,
    }
}

/// Aggregate the spot book index into price levels, capped at `depth` per
/// side. Mirrors `build_perp_book_snapshot` but delegates aggregation to
/// `BookIndex::top_n`, which already groups by price and orders correctly
/// (bids descending, asks ascending) for both forward and reverse iteration.
fn build_spot_book_snapshot(
    db: &Database,
    market: MarketId,
    depth: u32,
) -> conv::SpotOrderBookSnapshot {
    let book = BookIndex::new(db);
    let orders = Orders::new(db);
    let head = Meta::new(db).head_height();

    let depth_usize = depth as usize;
    let bid_levels = book.top_n(&orders, market, Side::Buy, depth_usize);
    let ask_levels = book.top_n(&orders, market, Side::Sell, depth_usize);

    let bids = bid_levels
        .into_iter()
        .map(|(price, qty)| conv::SpotBookLevel { price, qty })
        .collect();
    let asks = ask_levels
        .into_iter()
        .map(|(price, qty)| conv::SpotBookLevel { price, qty })
        .collect();

    conv::SpotOrderBookSnapshot {
        market,
        bids,
        asks,
        as_of_height: head,
    }
}

/// Read-only `BookSnapshot` over rocksdb. Used by the dashboard's perps panel
/// to call `best_bid_ask` without materialising every level. The snapshot is
/// short-lived: build, read, drop. Holding one across an `await` would be a
/// lock-tenure smell, so don't.
#[allow(dead_code)]
pub(crate) struct ReadOnlyBookSnapshot<'a> {
    pub orders: PerpOrders<'a>,
    pub book: PerpBookIndex<'a>,
    /// Position rows, so `BookSnapshot::reduce_only_capacity` answers from
    /// real state. This snapshot is read-only and never matches, but the trait
    /// has no default for that method precisely so an implementer cannot
    /// accidentally answer "unlimited".
    pub positions: cb_state::Positions<'a>,
    pub market: DerivId,
    pub last_trade_px_q8: u128,
}

impl<'a> BookSnapshot for ReadOnlyBookSnapshot<'a> {
    fn get_perp_order(&self, oid: &H256) -> Option<cb_types::PerpOrder> {
        self.orders.get(oid)
    }
    fn list_side(&self, market: &DerivId, side: Side) -> Vec<(u128, u64, H256)> {
        self.book.list_side(market, side)
    }
    fn last_trade_price(&self, market: &DerivId) -> Option<u128> {
        if self.market != *market || self.last_trade_px_q8 == 0 {
            None
        } else {
            Some(self.last_trade_px_q8)
        }
    }

    fn reduce_only_capacity(
        &self,
        owner: &cb_types::Address,
        market: &DerivId,
        side: Side,
    ) -> u128 {
        match self.positions.get(owner, market) {
            Some(p) if p.qty > 0 && p.side != side => p.qty,
            _ => 0,
        }
    }

    /// Read-only: this snapshot only answers `best_bid_ask` and never
    /// matches, so it has no fills to band.
    fn price_band(&self, _market: &DerivId) -> Option<(u128, u128)> {
        None
    }
}

/// Helper exported for the dashboard / integration tests: returns
/// `(best_bid, best_ask)` in `price_q8` units for a given market by walking
/// the CFs once. `None` on either side when the book is empty.
pub fn read_best_bid_ask(db: &Database, market: DerivId) -> (Option<u128>, Option<u128>) {
    let last_trade = PerpMarkets::new(db)
        .get(&market)
        .map(|m| m.last_trade_px_q8)
        .unwrap_or(0);
    let snap = ReadOnlyBookSnapshot {
        orders: PerpOrders::new(db),
        book: PerpBookIndex::new(db),
        positions: cb_state::Positions::new(db),
        market,
        last_trade_px_q8: last_trade,
    };
    best_bid_ask(&snap, &market)
}
