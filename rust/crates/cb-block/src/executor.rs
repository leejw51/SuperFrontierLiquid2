//! Apply a single verified tx to in-memory `BlockState`. Sender is taken from the
//! cached `VerifiedTx::sender` — no ECDSA recovery on the hot path.

use crate::block::RejectedTx;
use crate::state::BlockState;
use cb_engine::{
    accrue_index_delta, apply_fill_to_position, compute_funding_rate, compute_liquidation_orders,
    compute_mark_price, compute_settlement_pnl, compute_settlement_price, dated_mark_clamp_k,
    derivative_price_band, liquidation_fee_split, match_order, oracle as engine_oracle, spot_book,
    BookMutation, BookSnapshot, FundingClamp, IncomingOrder, MarkClampK, MatchOutcome,
    SelfTradeAction, DEFAULT_TIERS, MARK_HISTORY_BLOCKS,
};
use cb_state::{
    BookIndex, FundingIndices, FuturesMarkets, LiquidationQueue, MarginAccounts, OracleState,
    Orders, PerpBookIndex, PerpMarkets, PerpOrders, Positions, SpotOrder,
};
use cb_tx::{TxBody, VerifiedTx};
use cb_types::{
    Address, DerivId, FundingIndex, FutureMarket, LiquidationEntry, MarginAccount, MarkPrice,
    MarketId, OraclePrice, PerpMarket, PerpOrder, Position, Side, Signed, Tif, H256,
};
use thiserror::Error;

/// Maximum age of a usable derivative mark or oracle submission, in blocks.
/// Consensus constant: matches the gateway's default freshness budget.
pub const MAX_MARK_AGE_BLOCKS: u64 = 600;

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ExecError {
    #[error("bad signature")]
    BadSignature,
    #[error("bad nonce: expected {expected}, got {got}")]
    BadNonce { expected: u64, got: u64 },
    /// The transaction was signed for a different chain.
    ///
    /// `Transaction::chain_id` is covered by the signature specifically so that
    /// a transaction minted for one chain cannot be replayed onto another —
    /// which is exactly what happens if nobody compares it against the chain's
    /// own id. Checked before the nonce so a cross-chain replay is reported as
    /// what it is rather than as a nonce mismatch.
    #[error("wrong chain id: this chain is {expected}, transaction is for {got}")]
    WrongChainId { expected: u32, got: u32 },
    #[error("insufficient balance")]
    Insufficient,
    #[error("not authorized")]
    NotAuthorized,
    #[error("unsupported in this phase")]
    Unsupported,
    #[error("storage error: {0}")]
    Storage(String),
    /// An account balance operation hit an arithmetic edge — an overflow, a
    /// spend below zero, or (the one that mattered) an unlock of a lock that
    /// was never taken. These used to be `expect`s inside `AccountState`, so
    /// the first one killed the sequencer process and stopped the chain. They
    /// are now transaction-level: the transaction is rejected and rolled back
    /// whole, and the node stays up.
    #[error("account balance: {0}")]
    Balance(#[from] cb_state::BalanceError),
    /// Every transaction drained into this block was rejected, so there is
    /// nothing to produce. Block-level, not per-transaction: the builder
    /// returns it instead of emitting an empty block.
    #[error("no applicable transactions: all {} were rejected", .rejected.len())]
    NoApplicableTxs { rejected: Vec<RejectedTx> },
    /// `OracleUpdate.submitted_height` is older than the existing oracle
    /// row's `source_height`. Stale prices don't replace fresher ones. (M2)
    #[error("oracle update is stale")]
    OracleStale,
    /// `OracleUpdate.submitted_height` is above the height of the block it
    /// is executing in. A price cannot have been observed at a height the
    /// chain has not reached — and without this bound a single post carrying
    /// `u64::MAX` would make every later update "stale" forever, with no
    /// operator action short of a new genesis able to undo it.
    #[error("oracle update claims height {submitted} but the block is at {height}")]
    OracleFromFuture { submitted: u64, height: u64 },
    /// The block carries more transactions than `pack_seq` can number.
    /// Block-level: the builder and the verifier both refuse it, because
    /// two orders sharing a `seq` share a book-index key.
    #[error("block has {got} transactions; the maximum is {max}")]
    TooManyTxs { got: usize, max: usize },
    /// The market referenced by the tx isn't registered (no `PerpMarket` row).
    /// Shared by M2 (oracle/funding) and M3 (place/cancel) handlers.
    #[error("unknown market")]
    UnknownMarket,
    /// The block crossed a funding-interval boundary for a registered perp
    /// but did not include a matching `FundingTick` system tx signed by the
    /// proposer. Followers reject such blocks because the funding action is
    /// committed to the tx_root, not an out-of-band hook. (M2)
    #[error("missing required FundingTick at height {height} for market {market}")]
    MissingRequiredFundingTick {
        height: u64,
        market: cb_types::DerivId,
    },
    /// `PlacePerpOrder` requires the signer to have a `MarginAccount` row.
    /// M3 doesn't gate on collateral (M4 owns the IM check); we still require
    /// the row to exist so positions don't pile up against an account that
    /// has never deposited a USD margin balance.
    #[error("no margin account")]
    NoMarginAccount,
    /// Reduce-only order from an account with no opposing position. (M3)
    #[error("reduce_only without opposing position")]
    ReduceOnlyNoPos,
    /// Reduce-only order larger than the position it claims to reduce.
    ///
    /// The order is **rejected**, not clamped. Clamping was the alternative and
    /// it loses on three counts. A clamped order is not the order the trader
    /// signed, so the fill they get back does not match the transaction in the
    /// block — the silent mismatch is precisely the kind of thing that makes a
    /// wrong size look like a matching bug. Rejecting also keeps one rule in
    /// one sentence ("a reduce-only order is never bigger than the position it
    /// closes"), which the matcher can then enforce independently at fill time
    /// without the two layers having to agree on a clamped quantity. And it
    /// fails closed, which is the right default for a consensus-critical gate
    /// that, when it was absent, let ten dollars of collateral open half a
    /// million dollars of notional. (QA F1)
    #[error("reduce_only qty exceeds the position it would reduce")]
    ReduceOnlyTooLarge,
    /// `PlacePerpOrder` with `qty == 0` or `price_q8 == 0`.
    ///
    /// `cb_tx::validate_static` rejects these at mempool admission, but the
    /// executor is the consensus-critical layer and must reject rather than
    /// rely on that: a replica re-executing a block a Byzantine proposer built
    /// has to answer "invalid tx", not abort. (QA F10)
    #[error("perp order qty and price must both be > 0")]
    BadPerpOrder,
    /// Market is paused (governance / safety halt). (M3)
    #[error("market paused")]
    MarketPaused,
    /// Matcher rejected the order (post-only would cross, FOK kill, STP).
    /// Carries the matcher's `MatchOutcome` so callers can log the specific
    /// case. Renders generically; for now the wire-level error code is one
    /// channel, the matcher outcome the other. (M3)
    #[error("order rejected by matcher")]
    OrderRejected,
    /// Pre-trade margin gate rejected: account would not satisfy initial
    /// margin requirements after worst-case fill at the order's limit
    /// price. Reduce-only orders skip this check (they only ever decrease
    /// position size). (M4)
    #[error("insufficient margin")]
    InsufficientMargin,
    /// Market is in `paused_reduce_only_only` halt (insurance-fund
    /// exhaustion or governance halt) and the order is not reduce-only.
    /// Reduce-only orders are still accepted in this mode. (M4)
    #[error("reduce-only halt: market accepts only reduce_only orders")]
    ReduceOnlyOnly,
    /// `LiquidationTrigger` re-check found the account was actually solvent.
    /// The trigger fails closed and the keeper earns no bounty. (M4)
    #[error("account is not liquidatable")]
    NotLiquidatable,
    /// `LiquidationTrigger` references a market with no oracle / mark price
    /// available — we refuse to liquidate without a fresh mark to bound the
    /// reduce-only IOC's price. The market stays open until the next
    /// `OracleUpdate`. (M4)
    ///
    /// Also returned by `PlacePerpOrder` and `ClosePosition` on a market with
    /// no mark: a derivative order's price is banded around the mark (see
    /// `PriceOutOfBand`), and there is no band without one.
    #[error("missing mark price")]
    MissingMarkPrice,
    #[error("stale or future-dated mark price")]
    StaleMarkPrice,
    #[error("position exceeds the market notional cap")]
    PositionLimit,
    #[error("invalid market risk configuration")]
    InvalidRiskConfig,
    /// A derivative order's limit price lies outside the band around the
    /// mark, `mark × (1 ± DERIVATIVE_PRICE_BAND_BPS)`. Applies to every
    /// `PlacePerpOrder`, reduce-only and closing orders included, and to the
    /// order `ClosePosition` generates. Closing orders skip the margin gate,
    /// so without the band a solvent account could close at $1 into a
    /// colluder and leave the loss to the insurance fund.
    #[error("price {price_q8} is outside the band [{lo_q8}, {hi_q8}] around the mark")]
    PriceOutOfBand {
        price_q8: u128,
        lo_q8: u128,
        hi_q8: u128,
    },
    /// The block crossed an expiry boundary for a registered futures market
    /// but did not include a matching `FuturesSettle` system tx signed by
    /// the proposer. Mirror of `MissingRequiredFundingTick`. (M5)
    #[error("missing required FuturesSettle at height {height} for market {market}")]
    MissingRequiredFuturesSettle {
        height: u64,
        market: cb_types::DerivId,
    },
    /// Trader tried to place an order against a futures market that has
    /// already settled (`final_settlement_px_q8.is_some()`). (M5)
    #[error("market expired")]
    MarketExpired,
    /// `FuturesSettle` tx fired against a market whose `expiry_height` has
    /// not yet been reached. The required-tx check rejects these blocks at
    /// `enforce_required_futures_settle`; this is the runtime backstop. (M5)
    #[error("not yet expiry: market {market} expires at {expiry}, current {current}")]
    NotYetExpiry {
        market: cb_types::DerivId,
        expiry: u64,
        current: u64,
    },
    /// `FuturesSettle` was emitted twice for the same market (or against an
    /// already-settled market). The second tx is a no-op error rather than
    /// silent so replicas reject blocks containing duplicate settles. (M5)
    #[error("market already settled")]
    AlreadySettled,
    // ---------------- S3: spot ----------------
    /// `PlaceOrder` / `BatchCancel` referenced a `MarketId` with no
    /// `MarketConfig` row in `CF_MARKETS`.
    #[error("spot market not found")]
    MarketNotFound,
    /// Order qty is zero or not a multiple of the market's lot: `min_qty` on
    /// a spot market, `lot_size` on a perpetual or a dated future.
    ///
    /// The derivative half of that rule is recent. `lot_size` and
    /// `tick_size` had been on every `PerpMarket` and `FutureMarket` row
    /// since genesis, the gateway refused orders that were not on them, and
    /// the executor read neither — so anything that reached the chain
    /// without going through the gateway could rest a quantity of one base
    /// unit at a price of one q8 unit, and the genesis parameters were
    /// decorative. The chain is the layer that has to hold the rule.
    #[error("qty must be a positive multiple of the market's lot")]
    BadLot,
    /// Order price is not a multiple of the market's `tick_size`. Spot and
    /// derivative alike; see `BadLot`.
    #[error("price must be a multiple of tick_size")]
    BadTick,
    /// Spot order price is zero.
    #[error("price must be > 0")]
    ZeroPrice,
    /// `CancelOrder` referenced an order id with no row in `CF_ORDERS`.
    #[error("order not found")]
    OrderNotFound,
    /// Matcher rejected the spot order: `post_only` order would have crossed
    /// resting liquidity. Locked balance is released.
    #[error("post-only order would cross")]
    PostOnlyCrossed,
    /// Matcher rejected the spot order: FOK could not fully fill. Locked
    /// balance is released.
    #[error("fill-or-kill order could not fully fill")]
    FokUnfilled,
    /// Matcher rejected the spot order: STP fired (taker would cross a
    /// resting order it owns). Locked balance is released.
    #[error("self-trade prevention rejected order")]
    SelfTrade,
}

/// Per-block-apply context.
///
/// `block_height` is the height of the block being applied (= `Meta::head_height + 1`).
/// `proposer_addr` is the address of the leader for this block — system txs
/// (`FundingTick`, `FuturesSettle`) MUST be signed by this address.
/// `treasury_signer` retains its prior role (Deposit / WithdrawSettled authority).
///
/// M3 uses `block_height` (and the `tx_index` passed to `apply_tx_at`) to derive
/// deterministic, monotonic `seq` numbers for resting perp orders. Spot txs
/// don't read `block_height` so their tests can leave it at the default.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExecCtx {
    /// Address authorized to sign Deposit and WithdrawSettled txs.
    pub treasury_signer: Address,
    /// Height of the block we're applying. Used by perp/futures system actions
    /// (FundingTick boundary check, mark recompute, oracle freshness) and by
    /// M3 perp seq derivation.
    pub block_height: u64,
    /// Address of the proposer/sequencer for this block. System txs
    /// (FundingTick, FuturesSettle) MUST be signed by this address.
    pub proposer_addr: Address,
}

impl ExecCtx {
    /// Convenience for tests / call-sites that don't carry a height.
    pub fn for_treasury(treasury_signer: Address) -> Self {
        Self {
            treasury_signer,
            block_height: 0,
            proposer_addr: Address::ZERO,
        }
    }
}

/// Backward-compatible wrapper: applies a tx using `tx_index = 0`. Spot txs
/// are unaffected by `tx_index`; perp tests that need a specific seq should
/// call `apply_tx_at` directly.
pub fn apply_tx(state: &mut BlockState, ctx: &ExecCtx, v: &VerifiedTx) -> Result<(), ExecError> {
    apply_tx_at(state, ctx, v, 0)
}

/// Apply a tx with an explicit `tx_index` within the block. The block builder
/// calls this with `enumerate()`. Resting perp orders use the
/// `(block_height, tx_index, intra_tx_counter)` triple to derive a monotonic,
/// reproducible `seq`; same triple → same seq across replicas.
pub fn apply_tx_at(
    state: &mut BlockState,
    ctx: &ExecCtx,
    v: &VerifiedTx,
    tx_index: u32,
) -> Result<(), ExecError> {
    // Chain-id gate. `Transaction::chain_id` is signed, so a transaction is a
    // commitment to run on one specific chain; without this comparison that
    // commitment is unenforced and a transaction lifted from a testnet replays
    // verbatim against mainnet.
    //
    // Read from `Meta` rather than carried on `ExecCtx` so the check needs no
    // change to the executor's signature, and so every replica compares against
    // *its own* chain's id. `Meta::chain_id` is genesis-set consensus state, so
    // this stays a pure function of state + tx: replicas agree by construction.
    // `Meta` is not part of the state-root preimage, so adding the read does
    // not perturb any committed root.
    //
    // `BlockState` reads it once when the block starts. It used to be read
    // here, per transaction — a point read that cost nothing on a small
    // database and, on a large one, queued behind RocksDB's table-cache mutex
    // for two thirds of the block loop. Same value either way: genesis sets it
    // and nothing changes it.
    let chain_id = state.chain_id();
    if v.signed.tx.chain_id != chain_id {
        return Err(ExecError::WrongChainId {
            expected: chain_id,
            got: v.signed.tx.chain_id,
        });
    }

    let sender = v.sender;
    let mut sender_state = state.load(&sender);
    // A synthetic transaction is the executor's own system action under BFT
    // consensus (`SignedTx::synthetic`): unsigned, nonce-free, and only ever
    // a funding tick or a settlement. Anything else unsigned is refused.
    let synthetic = v.is_synthetic();
    if synthetic {
        if !matches!(
            v.signed.tx.body,
            TxBody::FundingTick { .. } | TxBody::FuturesSettle { .. }
        ) {
            return Err(ExecError::NotAuthorized);
        }
    } else if v.signed.tx.nonce != sender_state.nonce {
        return Err(ExecError::BadNonce {
            expected: sender_state.nonce,
            got: v.signed.tx.nonce,
        });
    }
    let nonce_before = sender_state.nonce;

    let result = match &v.signed.tx.body {
        TxBody::Transfer { to, asset, amount } => {
            if sender_state.available(*asset) < *amount {
                return Err(ExecError::Insufficient);
            }
            sender_state.sub(*asset, *amount)?;
            sender_state.nonce += 1;
            state.put(sender, sender_state);
            let mut to_state = state.load(to);
            to_state.add(*asset, *amount)?;
            state.put(*to, to_state);
            Ok(())
        }
        TxBody::Deposit {
            user,
            asset,
            amount,
            ..
        } => {
            if sender != ctx.treasury_signer {
                return Err(ExecError::NotAuthorized);
            }
            sender_state.nonce += 1;
            state.put(sender, sender_state);
            let mut s = state.load(user);
            s.add(*asset, *amount)?;
            state.put(*user, s);
            Ok(())
        }
        // Withdrawals are refused until there is a withdrawal ledger for them
        // to settle against. `Withdraw` used to lock the amount and
        // `WithdrawSettled` used to advance the treasury's nonce and nothing
        // else: no burn, no release, and no reference the executor could have
        // used to find the lock. Every withdrawal therefore reduced a balance
        // for good. Nothing in the client, gateway or web sends either, so
        // failing closed costs no user a path they had.
        TxBody::Withdraw { .. } | TxBody::WithdrawSettled { .. } => Err(ExecError::Unsupported),
        TxBody::PlaceOrder {
            market,
            side,
            price,
            qty,
            tif,
            post_only,
            client_order_id,
        } => apply_place_spot_order(
            state,
            ctx,
            tx_index,
            sender,
            &mut sender_state,
            *market,
            *side,
            *price,
            *qty,
            *tif,
            *post_only,
            *client_order_id,
        ),
        TxBody::CancelOrder { order_id } => {
            apply_cancel_spot_order(state, sender, &mut sender_state, *order_id)
        }
        TxBody::BatchCancel { market } => {
            apply_batch_cancel_spot(state, sender, &mut sender_state, *market)
        }

        // ---------------- M2: oracle + funding ----------------
        TxBody::OracleUpdate {
            market,
            px_q8,
            submitted_height,
        } => apply_oracle_update(
            state,
            ctx,
            &sender,
            &mut sender_state,
            market,
            *px_q8,
            *submitted_height,
        ),
        TxBody::FundingTick { market } => {
            apply_funding_tick(state, ctx, &sender, &mut sender_state, market)
        }

        // ---------------- M3: perp matching ----------------
        TxBody::PlacePerpOrder {
            market,
            side,
            price_q8,
            qty,
            tif,
            post_only,
            reduce_only,
            client_order_id,
        } => apply_place_perp_order(
            state,
            ctx,
            tx_index,
            sender,
            &mut sender_state,
            *market,
            *side,
            *price_q8,
            *qty,
            *tif,
            *post_only,
            *reduce_only,
            *client_order_id,
        ),
        TxBody::CancelPerpOrder { order_id } => {
            apply_cancel_perp_order(state, sender, &mut sender_state, *order_id)
        }

        // ---------------- M4: liquidation ----------------
        TxBody::LiquidationTrigger { owner, market } => apply_liquidation_trigger(
            state,
            ctx,
            tx_index,
            sender,
            &mut sender_state,
            *owner,
            *market,
        ),

        // ---------------- M5: dated futures settlement ----------------
        TxBody::FuturesSettle { market } => {
            apply_futures_settle(state, ctx, &sender, &mut sender_state, market)
        }

        // ---------------- M5+ (still stubs) ----------------
        // Match explicitly so adding a new tx variant without a handler is a
        // *compile* error rather than a silent fall-through to Unsupported.
        TxBody::ClosePosition {
            market,
            qty,
            max_slippage_bps,
        } => apply_close_position(
            state,
            ctx,
            tx_index,
            sender,
            &mut sender_state,
            *market,
            *qty,
            *max_slippage_bps,
        ),
        TxBody::OpenPosition { .. } | TxBody::AdjustMargin { .. } => Err(ExecError::Unsupported),
    };
    if synthetic && result.is_ok() {
        // The system handlers advance the sender's nonce as any transaction
        // would. A synthetic one consumed none: put it back, so the
        // proposer's own signed transactions are not thrown off by the
        // ticks the executor injected in its name.
        let mut after = state.load(&sender);
        after.nonce = nonce_before;
        state.put(sender, after);
    }
    result
}

// ---------------------------------------------------------------------------
// OracleUpdate (M2)
// ---------------------------------------------------------------------------

fn apply_oracle_update(
    state: &mut BlockState,
    ctx: &ExecCtx,
    sender: &Address,
    sender_state: &mut cb_state::AccountState,
    market: &DerivId,
    px_q8: u128,
    submitted_height: u64,
) -> Result<(), ExecError> {
    let db = &state.db();
    // Authority lookup. Perps and dated futures both have an `oracle_authority`
    // pinned in their market record; the kind tells us which CF to read. M5
    // also reads the dated-future record to drive the per-block clamp `K`
    // (1% in the final hour, 5% otherwise).
    let (authority, future_market) = match market.kind() {
        Ok(cb_types::MarketKind::Perp) => {
            let pm = PerpMarkets::new(db)
                .get(market)
                .ok_or(ExecError::UnknownMarket)?;
            (pm.oracle_authority, None)
        }
        Ok(cb_types::MarketKind::Future) => {
            let fm = FuturesMarkets::new(db)
                .get(market)
                .ok_or(ExecError::UnknownMarket)?;
            // M5 backstop: oracle posts against a settled market do nothing
            // useful and shouldn't update the mark history. Reject explicitly.
            if fm.final_settlement_px_q8.is_some() {
                return Err(ExecError::MarketExpired);
            }
            (fm.oracle_authority, Some(fm))
        }
        Err(_) => return Err(ExecError::UnknownMarket),
    };
    engine_oracle::check_authority(*sender, authority).map_err(|_| ExecError::NotAuthorized)?;

    let store = OracleState::new(db);
    let existing = store.get_oracle(market);
    engine_oracle::check_freshness(submitted_height, existing.as_ref())
        .map_err(|_| ExecError::OracleStale)?;
    if submitted_height > ctx.block_height {
        return Err(ExecError::OracleFromFuture {
            submitted: submitted_height,
            height: ctx.block_height,
        });
    }

    if ctx.block_height - submitted_height > MAX_MARK_AGE_BLOCKS {
        return Err(ExecError::OracleStale);
    }

    let new_oracle = OraclePrice {
        market: *market,
        px_q8,
        source_height: submitted_height,
        authority,
    };
    store.put_oracle(&new_oracle);

    // Recompute mark using the new oracle. M3 wires last-trade price onto the
    // PerpMarket record; M5 mirrors it on FutureMarket so the same code path
    // serves both markets.
    let last_q8 = match market.kind() {
        Ok(cb_types::MarketKind::Perp) => PerpMarkets::new(db).get(market).and_then(|m| {
            if m.last_trade_px_q8 == 0 && m.last_trade_height == 0 {
                None
            } else {
                Some(m.last_trade_px_q8)
            }
        }),
        Ok(cb_types::MarketKind::Future) => future_market.as_ref().and_then(|m| {
            if m.last_trade_px_q8 == 0 && m.last_trade_height == 0 {
                None
            } else {
                Some(m.last_trade_px_q8)
            }
        }),
        _ => None,
    };

    // Clamp factor: perps always get 1% (`MarkClampK::PERP`); dated futures
    // get 5% normally, 1% in the final hour before expiry. This is the M5
    // K-shrink schedule. The two helpers live in different modules so we
    // adapt locally.
    // The order book's midpoint, which the mark price and the funding premium
    // both need. Prior to QA finding F9 `mid` was hard-coded to `None` on every
    // path and nothing anywhere wrote `premium_ema_q9`, so the funding rate was
    // always exactly zero, every funding tick moved nothing, and the
    // "median of three" mark degenerated into a median of
    // `(oracle, oracle, clamped last trade)` — the book never entered the mark
    // at all.
    let mid_q8 = book_mid_price(db, market);

    let mark_px = match market.kind() {
        Ok(cb_types::MarketKind::Future) => {
            let fm = future_market
                .as_ref()
                .expect("future market resolved above");
            let dk = dated_mark_clamp_k(fm, ctx.block_height);
            // Adapt `dated::MarkClampK` to `mark::MarkClampK`: same shape,
            // different module. Both are just a `bps` u16.
            let k = MarkClampK { bps: dk.bps };
            compute_mark_price(px_q8, mid_q8, last_q8, k)
        }
        _ => {
            let k = MarkClampK::for_market(market);
            compute_mark_price(px_q8, mid_q8, last_q8, k)
        }
    };
    let prev_premium = store
        .get_mark(market)
        .map(|m| m.premium_ema_q9)
        .unwrap_or(0);
    let mark = MarkPrice {
        market: *market,
        px_q8: mark_px,
        computed_at_height: ctx.block_height,
        premium_ema_q9: update_premium_ema(prev_premium, mid_q8, px_q8),
    };
    store.put_mark(&mark);

    // M5: persist the mark sample into the per-market ring buffer. Only dated
    // futures need this — perps don't settle, so a TWAP buffer would be wasted
    // storage. Prune samples older than `expiry_height - MARK_HISTORY_BLOCKS`
    // each time we write so the buffer is bounded.
    if let Some(fm) = future_market {
        store.put_mark_history(market, ctx.block_height, mark_px);
        let cutoff = fm.expiry_height.saturating_sub(MARK_HISTORY_BLOCKS);
        store.prune_mark_history_before(market, cutoff);
    }

    sender_state.nonce += 1;
    state.put(*sender, sender_state.clone());
    Ok(())
}

/// Midpoint of the market's order book, `None` unless both sides are quoted.
///
/// Reads the book index directly rather than through a `BookSnapshot`: this is
/// the oracle path, which has no matcher and no order to match. The index is
/// walked in lex order, so the best bid is the last `Buy` key and the best ask
/// the first `Sell` key — a pure, deterministic function of committed state.
///
/// A one-sided book has no midpoint, and inventing one (from the single quote,
/// or from the oracle) would feed the mark a number the market never agreed on.
/// `None` leaves `compute_mark_price` with the oracle in the mid slot, which is
/// its documented no-mid behaviour.
fn book_mid_price(db: &cb_state::Database, market: &DerivId) -> Option<u128> {
    let index = PerpBookIndex::new(db);
    let best_bid = index
        .list_side(market, Side::Buy)
        .last()
        .map(|(p, _, _)| *p)?;
    let best_ask = index
        .list_side(market, Side::Sell)
        .first()
        .map(|(p, _, _)| *p)?;
    // Crossed books are not reachable through the matcher, but average rather
    // than subtract so a malformed index cannot produce a wild mid.
    Some(best_bid.saturating_add(best_ask) / 2)
}

/// Smoothing factor for the premium EMA, as a divisor: `new = prev + (raw -
/// prev) / 8`. One eighth per sample is the design's figure and the same one
/// the simulation harness injects when it stands in for this computation.
const PREMIUM_EMA_DIVISOR: i128 = 8;

/// Fold the current book premium into the exponentially-smoothed premium index.
///
/// The raw premium is `(mid - oracle) / oracle`, in the `1e9` fixed point
/// `compute_funding_rate` consumes. With no midpoint there is no new
/// observation, so the previous value carries forward unchanged rather than
/// decaying toward zero — a market that stops quoting has not told us the
/// premium is gone.
fn update_premium_ema(prev_q9: i64, mid_q8: Option<u128>, oracle_q8: u128) -> i64 {
    let Some(mid) = mid_q8 else {
        return prev_q9;
    };
    if oracle_q8 == 0 {
        return prev_q9;
    }
    let diff = (mid as i128).saturating_sub(oracle_q8 as i128);
    let raw_q9 = diff.saturating_mul(1_000_000_000i128) / (oracle_q8 as i128);
    let prev = prev_q9 as i128;
    // Saturating, like every other step here. `raw_q9` is the saturated
    // product above divided by an oracle price that can be as small as one,
    // so it can sit at `i128::MAX`, and a negative `prev` then took the
    // plain subtraction past it — a wrap in the release build that runs
    // consensus, a panic in a debug one. Reachable from a quote at an
    // absurd price, which nothing forbids.
    let next = prev.saturating_add(raw_q9.saturating_sub(prev) / PREMIUM_EMA_DIVISOR);
    next.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

// ---------------------------------------------------------------------------
// FundingTick (M2)
// ---------------------------------------------------------------------------

fn apply_funding_tick(
    state: &mut BlockState,
    ctx: &ExecCtx,
    sender: &Address,
    sender_state: &mut cb_state::AccountState,
    market: &DerivId,
) -> Result<(), ExecError> {
    if *sender != ctx.proposer_addr {
        return Err(ExecError::NotAuthorized);
    }
    let db = &state.db();
    let pm = PerpMarkets::new(db)
        .get(market)
        .ok_or(ExecError::UnknownMarket)?;

    let funding_cf = FundingIndices::new(db);
    let mut idx = funding_cf
        .get(market)
        .unwrap_or_else(|| FundingIndex::new(*market));

    let height = ctx.block_height;
    let elapsed = if idx.last_tick_height == 0 {
        // First tick ever — accrue zero blocks (we don't retroactively credit
        // funding before the market was actively ticking).
        0
    } else {
        height.saturating_sub(idx.last_tick_height)
    };

    // Compute the new rate. premium_ema lives on MarkPrice; pull it (or 0 if
    // mark hasn't been recomputed yet — first FundingTick before any oracle).
    let store = OracleState::new(db);
    let premium_ema_q9 = store
        .get_mark(market)
        .map(|m| m.premium_ema_q9)
        .unwrap_or(0);
    // `interest_rate_q9` is per-market in the protocol design but is not yet
    // wired through PerpMarket. M2 ships with a flat zero — the interest term
    // is dwarfed by premium clamping at this scale, and bumping it later is
    // a state-only change (no consensus migration).
    let interest_rate_q9 = 0;
    let raw_rate = compute_funding_rate(premium_ema_q9, interest_rate_q9, FundingClamp::DEFAULT);
    // Apply per-market cap on top of the protocol-wide clamp. Both bounds
    // converge on the same intent; whichever is tighter wins.
    let cap = pm.funding_rate_cap_q9.unsigned_abs() as i64;
    let clipped = raw_rate.clamp(-cap, cap);

    // Accrue the cumulative index. `rate_q9` is a per-interval rate — the
    // "0.5% per hour" the clamp documents, the interval being
    // `funding_interval_blocks` — pro-rated over the blocks elapsed. The
    // previous line here said "per-block delta, not per-hour" and multiplied
    // the interval rate by the block count, which charged the hourly cap
    // once per block. (QA F12.)
    //
    // The index is in funding owed per contract unit, so the rate is
    // weighted by the mark it applies at: settlement multiplies by quantity
    // alone. Without a mark nothing accrues — there is no price to charge
    // the rate on, and the band and margin gate refuse trading until one
    // exists anyway.
    let interval = pm.funding_interval_blocks.max(1) as u64;
    let mark_q8 = market_mark_q8(db, market).unwrap_or(0);
    idx.cumulative = accrue_index_delta(idx.cumulative, clipped, mark_q8, elapsed, interval);
    idx.rate_q9 = clipped;
    idx.last_tick_height = height;
    funding_cf.put(&idx);

    // Hour-boundary settlement: if this tick lands on a funding-interval
    // boundary, settle accrued funding into MarginAccount.collateral for every
    // open position in the market. We sort positions by canonical CF byte
    // order before mutating so trie writes (and hence the state root) are
    // deterministic across replicas.
    let on_boundary = height > 0 && height % interval == 0;
    if on_boundary {
        settle_market_funding(state, market, &idx)?;
    }

    sender_state.nonce += 1;
    state.put(*sender, sender_state.clone());
    Ok(())
}

/// Walk every position in `market` (deterministic byte order) and apply the
/// funding delta. The amount each long pays / each short receives is
///
/// ```text
/// delta = qty * (cur_index - entry_funding_index) / RATE_DEN_Q9
/// ```
///
/// in micro-USD. The index already carries the price (see
/// `cb_engine::accrue_index_delta`), so this is notional times rate. Sign convention: positive index delta => longs paid shorts
/// (matches `FundingIndex.cumulative` convention in derivatives.rs).
fn settle_market_funding(
    state: &mut BlockState,
    market: &DerivId,
    idx: &FundingIndex,
) -> Result<(), ExecError> {
    let db = &state.db();
    let positions_cf = Positions::new(db);

    // Collect-then-mutate: holding the iterator across `put_*` calls would
    // deadlock if RocksDB's locking surprised us. Doing it in two passes is
    // safe and the deterministic-order guarantee comes from the CF prefix
    // scan being lex-ordered. Owners are then sorted explicitly so the final
    // mutation order is `(market, owner)` byte-wise.
    let mut victims: Vec<(Address, Position)> = Vec::new();
    let prefix: [u8; 0] = [];
    let it = match db.iter_prefix(cb_state::CF_POSITIONS, &prefix) {
        Ok(it) => it,
        Err(_) => return Ok(()),
    };
    for kv in it {
        let (k, v) = match kv {
            Ok(p) => p,
            Err(_) => break,
        };
        // Position key layout: [owner: 20][deriv_id: 4]. Filter to this market.
        if k.len() < 24 {
            continue;
        }
        let market_bytes = &k[20..24];
        if market_bytes != market.as_bytes().as_slice() {
            continue;
        }
        let p: Position = match bincode::deserialize(&v) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let owner_bytes: [u8; 20] = k[..20].try_into().expect("20 bytes");
        victims.push((Address(owner_bytes), p));
    }
    // Explicit sort. RocksDB iteration order is already deterministic, but
    // this insulates us from any future CF-comparator change.
    victims.sort_by_key(|a| a.0 .0);

    let cur = idx.cumulative.to_i128();
    // Track the position keys we mutate so we can register them with the
    // trie-commit step after dropping the `positions_cf` handle. M8.
    let mut touched: Vec<Address> = Vec::with_capacity(victims.len());
    for (owner, mut pos) in victims {
        if pos.qty == 0 {
            continue;
        }
        let entry = pos.entry_funding_index.to_i128();
        let diff = cur.saturating_sub(entry);

        // delta_micro_usd = qty * diff / RATE_DEN_Q9
        // Sign rule: longs pay positive delta, shorts receive; flipped for shorts.
        // i256 would be ideal but i128 is enough — qty is in base units (e.g.
        // 1e8 for 1 BTC) and diff is rate*blocks (~1e7 per block max), so
        // qty*diff fits in i128 for any realistic OI.
        let qty_signed = pos.qty as i128;
        let raw = qty_signed.saturating_mul(diff);
        let den = cb_engine::RATE_DEN_Q9 as i128;
        let delta = raw / den;

        // Sign by position side. `Side::Buy` = long; positive delta on a long
        // means the long pays (collateral decreases). `Side::Sell` = short
        // flips the sign.
        let delta_signed = match pos.side {
            cb_types::Side::Buy => delta,
            cb_types::Side::Sell => -delta,
        };

        // The sign here is "amount the owner pays". Negative cash remains
        // a liability while positions are open; receipts repay it before
        // increasing spendable collateral. Insurance waits for full close-out.
        apply_collateral_delta(state, owner, -delta_signed, Some(*market));

        pos.entry_funding_index = Signed::from_i128(cur);
        pos.last_funding_height = idx.last_tick_height;
        positions_cf.put(&pos);
        touched.push(owner);
    }
    // M8: register every (owner, market) we wrote with the trie-commit step.
    let market_id = *market;
    for owner in touched {
        state.touch_position(owner, market_id);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// PlacePerpOrder (M3)
// ---------------------------------------------------------------------------

/// Whether a transaction the executor refused still consumes its nonce.
///
/// Under BFT a committed payload is final and the block keeps every
/// transaction in it, refused or not. A refusal that left the sender's nonce
/// where it was punched a hole in that sender's sequence: every later
/// transaction the sender had already signed and pipelined — a client keeps
/// hundreds in flight — was "ahead" of the chain forever, stuck in every
/// validator's mempool until the client noticed and re-signed from the
/// hole. Under load a fifth of all submissions were refused at admission for
/// exactly that, and the stuck transactions filled the mempools. So a
/// transaction that was the sender's next one (it passed the nonce check)
/// consumes the nonce whether or not its body succeeded, as on every
/// account-based chain. What is refused *before* the nonce check — a wrong
/// chain id, a nonce that is not the next — changes nothing, and neither
/// does a synthetic system transaction, which has no nonce.
pub fn failure_consumes_nonce(v: &VerifiedTx, e: &ExecError) -> bool {
    !v.is_synthetic() && error_consumes_nonce(e)
}

/// The error half of [`failure_consumes_nonce`]: whether `e` is a refusal
/// from the body, after the nonce check, rather than from before it.
pub fn error_consumes_nonce(e: &ExecError) -> bool {
    !matches!(
        e,
        ExecError::BadNonce { .. } | ExecError::WrongChainId { .. } | ExecError::BadSignature
    )
}

/// Advance `sender`'s nonce by one after its transaction was rolled back;
/// the only trace a refused transaction leaves.
pub fn consume_nonce(state: &mut BlockState, sender: Address) {
    let mut a = state.load(&sender);
    a.nonce += 1;
    state.put(sender, a);
}

#[allow(clippy::too_many_arguments)]
fn apply_place_perp_order(
    state: &mut BlockState,
    ctx: &ExecCtx,
    tx_index: u32,
    sender: Address,
    sender_state: &mut cb_state::AccountState,
    market: DerivId,
    side: Side,
    price_q8: u128,
    qty: u128,
    tif: cb_types::Tif,
    post_only: bool,
    reduce_only: bool,
    client_order_id: H256,
) -> Result<(), ExecError> {
    let db = &state.db();
    let mas = MarginAccounts::new(db);
    let positions = Positions::new(db);
    let perp_orders = PerpOrders::new(db);
    let perp_book = PerpBookIndex::new(db);

    // 1. Resolve the market record. The two CFs share the order-side fields
    //    (`paused`, `tick_size`, `last_trade_px_q8`, …) — we adapt both into
    //    a `MarketView` so the rest of the function is kind-agnostic.
    //    Futures markets that have settled are rejected with `MarketExpired`;
    //    once `final_settlement_px_q8` is set, the book is closed forever.
    let mut view = load_market_view(db, &market)?;
    if view.paused() {
        return Err(ExecError::MarketPaused);
    }
    if view.expired(ctx.block_height) {
        return Err(ExecError::MarketExpired);
    }

    // 2. Reduce-only halt (M4). Perps and futures both carry the flag.
    if view.paused_reduce_only_only() && !reduce_only {
        return Err(ExecError::ReduceOnlyOnly);
    }

    // 2b. Degenerate order shapes. This has to happen before the margin gate,
    //     which simulates a fill, and before the matcher, which is the layer
    //     that used to own the check. `cb_tx::validate_static` rejects these at
    //     mempool admission, so the only way to reach here is a block built by
    //     a proposer that skipped it — which is exactly the case a consensus
    //     layer must reject cleanly rather than abort on. (QA F10)
    if qty == 0 || price_q8 == 0 {
        return Err(ExecError::BadPerpOrder);
    }

    // 2c. Lot and tick. The market record has carried both since genesis
    //     and the gateway has always refused an order off either; the
    //     executor now holds the same rule, so a transaction that reaches
    //     the chain by any other road is held to it too (STATUS §3.2). A
    //     zero on the record means the market sets no increment. Every fill
    //     quantity is the smaller of two quantities that passed this check,
    //     so positions stay on the lot by construction, and the liquidation
    //     ladder — which prices its tranches off the mark and does not come
    //     through admission — is unaffected.
    if !view.on_lot(qty) {
        return Err(ExecError::BadLot);
    }
    if !view.on_tick(price_q8) {
        return Err(ExecError::BadTick);
    }

    // 2d. Price band. Every derivative order — reduce-only and closing orders
    //     included, which is the point — must be priced within
    //     `DERIVATIVE_PRICE_BAND_BPS` of the mark. Closing orders skip the
    //     margin gate below (an underwater account has to be able to get
    //     out), and the gate itself passes any full close, so before this a
    //     solvent account could sell its whole long at $1 into a colluder and
    //     the insurance fund paid the colluder the loss beyond its collateral.
    //     The band bounds what any one order can realise against the mark.
    //     The matcher applies the same band to fills (`price_band` on the
    //     snapshot), so a resting order the mark has moved away from is not
    //     filled at its stale price either. No mark, no band: refused.
    //     (Test: `margin_gate_worst_case.rs`.)
    let mark_q8 = fresh_mark_q8(db, &market, ctx.block_height)?;
    let (band_lo, band_hi) = derivative_price_band(mark_q8);
    if price_q8 < band_lo || price_q8 > band_hi {
        return Err(ExecError::PriceOutOfBand {
            price_q8,
            lo_q8: band_lo,
            hi_q8: band_hi,
        });
    }

    // 3. Margin account must exist (M3 left this as just-presence; M4 keeps
    //    that requirement and additionally checks IM below).
    let margin_account = mas.get(&sender).ok_or(ExecError::NoMarginAccount)?;

    // 4. Reduce-only admission: the signer must have an opposing position, AND
    //    the order must be no larger than it. The size half of that rule used
    //    to be missing, and because step 5 then skips the initial-margin gate
    //    for reduce-only orders, a single base unit of long position was a
    //    licence to open an arbitrarily large short with no margin check at
    //    all. See `ExecError::ReduceOnlyTooLarge` for why this rejects rather
    //    than clamping.
    //
    //    This is admission, not enforcement: it gives the trader a clean error
    //    at the moment they ask for something impossible. Enforcement lives in
    //    the matcher, which caps the fill plan against the same capacity —
    //    necessary because a reduce-only GTC order that is valid today can be
    //    hit as a maker after the position it was written to close is gone.
    //    Both layers read `reduce_only_capacity_of` so they cannot drift.
    if reduce_only {
        let capacity = reduce_only_capacity_of(&positions, &sender, &market, side);
        if capacity == 0 {
            return Err(ExecError::ReduceOnlyNoPos);
        }
        if qty > capacity {
            return Err(ExecError::ReduceOnlyTooLarge);
        }
    }

    // 5. Pre-trade margin gate (M4). Reduce-only orders skip — and that skip is
    //    only sound because step 4 and the matcher together guarantee such an
    //    order can only ever shrink |position|, never grow or flip it.
    //
    //    Anything else is judged in the worse of the two worlds it can leave
    //    the account in — the order fills in full at its limit price, or it
    //    rests and never fills — against the requirement of everything the
    //    account could come to hold. Three things the earlier gate got wrong,
    //    each a way to rest an order with nothing behind it:
    //
    //    * It charged the requirement against the account's potential
    //      position with the incoming order treated as a *fill*, netted
    //      against the held position and, through `potential_exposures`,
    //      against the resting orders on the other side. Bids and asks
    //      cancelled each other, so a bid and an ask of equal size were
    //      charged nothing, however many pairs were resting (QA F11 fixed
    //      the case of two bids, not this one).
    //    * It took the equity from the filled world alone, and a bid below
    //      the mark filled at its limit is an unrealised gain: an account
    //      with nothing could rest a bid at 45,000 against a 50,000 mark and
    //      be credited the 5,000 it would gain if the bid filled now.
    //    * It dropped the realised PnL of the fill it simulated, so an
    //      account already underwater could sell through its long and open
    //      a short on the other side with the loss nowhere in the equity.
    //
    //    The gate takes the smaller of the two equities;
    //    `potential_exposures` counts the incoming order as one
    //    more resting order on its side. (QA F11)
    //
    //    An order that is reducing in fact, if not in flag, skips the gate as
    //    a reduce-only one does: if everything the account has on this side
    //    — this order plus its resting non-reduce-only orders — is no more
    //    than the position held on the other side, then every fill on this
    //    side closes lots and none opens any. That is how an underwater
    //    account closes with a plain order, which the worst-case gate below
    //    would otherwise refuse (its equity is negative and the position it
    //    still holds carries a requirement). A later order that shrinks the
    //    held position is itself charged with this one counted in.
    if !reduce_only && !closes_only(&positions, &perp_orders, sender, market, side, qty) {
        let held = positions.list_for_owner(&sender);
        let (proposed, fill_pnl_micro) = simulate_worst_case_position_set(
            db,
            &held,
            sender,
            market,
            side,
            qty,
            price_q8,
            ctx.block_height,
        );
        let exposures = potential_exposures(&perp_orders, sender, &held, market, side, qty);
        let mut all_markets: Vec<Position> = held.clone();
        all_markets.extend(proposed.iter().cloned());
        all_markets.extend(exposures.iter().cloned());
        let marks = fresh_marks_for_positions(db, &all_markets, ctx.block_height)?;
        // A market that has never been marked has no price the equity can
        // use, and it had no price the requirement could use either — so the
        // requirement was zero and any funded margin account could open any
        // size on a freshly listed market until its first oracle post. Step
        // 2d now refuses an order on an unmarked market outright
        // (`MissingMarkPrice`), so this market always has a mark here; the
        // fallback below — price the requirement at the order's own limit —
        // is kept as defence in depth. The equity side is left alone, so a
        // held position there is not worth whatever the trader asks.
        let mut requirement_marks = marks.clone();
        if !requirement_marks.iter().any(|(m, _)| *m == market) {
            requirement_marks.push((market, price_q8));
            requirement_marks.sort_by_key(|a| a.0 .0);
        }
        // Only an order that can rest has an unfilled world to be judged in;
        // an IOC or FOK fills now or is gone (see the engine's doc comment).
        let unfilled = match tif {
            Tif::Gtc => Some(held.as_slice()),
            Tif::Ioc | Tif::Fok => None,
        };
        // The orders already resting are judged in their worst world too.
        // Each one was admitted with only its *own* fill-at-limit loss in the
        // equity, so N bids above the mark each passed against the whole
        // collateral and, all filled at once by one sell, took the account
        // far below zero: ten 0.02 BTC bids at 75,000 against a 50,000 mark
        // and $3,000 of collateral all rested, and a single 0.2 BTC sell into
        // them left equity at -$2,000 for the insurance fund to cover. Those
        // orders can fill whether or not this one does, so their adverse loss
        // comes off the equity of both worlds. A favourable limit (a bid below
        // the mark) is never credited; that gain only exists if the market
        // moves to it. (Test: `margin_gate_worst_case.rs`.)
        let judged_account = debited_by(
            &margin_account,
            resting_adverse_loss_micro(db, &perp_orders, sender),
        );
        check_position_caps(db, &exposures, &requirement_marks)?;
        let filled_equity = cb_engine::compute_equity(&judged_account, &proposed, &marks)
            .to_i128()
            .saturating_add(fill_pnl_micro);
        let equity = unfilled.map_or(filled_equity, |held| {
            cb_engine::compute_equity(&judged_account, held, &marks)
                .to_i128()
                .min(filled_equity)
        });
        let required = configured_margin_required(db, &exposures, &requirement_marks, false)?;
        if equity < 0 || (equity as u128) < required {
            return Err(ExecError::InsufficientMargin);
        }
    }

    // 6. Build a `BookSnapshot` over the current CFs and call the matcher.
    //    The matcher path is identical for perps and dated futures — only
    //    `last_trade_price` differs by record source, and the view abstracts
    //    that.
    let last_trade = view.last_trade_price();
    let snapshot = ViewBookSnapshot {
        db,
        height: ctx.block_height,
        perp_orders: &perp_orders,
        perp_book: &perp_book,
        positions: &positions,
        market,
        last_trade,
        band: Some((band_lo, band_hi)),
    };

    // Deterministic `seq` for any resting remainder.
    let seq = pack_seq(ctx.block_height, tx_index, 0);
    let order_id = derive_order_id(sender, ctx.block_height, tx_index, 0);

    let incoming = IncomingOrder {
        order_id,
        owner: sender,
        market,
        side,
        price_q8,
        qty,
        tif,
        post_only,
        reduce_only,
        seq,
        placed_at_height: ctx.block_height,
        client_order_id,
        stp: SelfTradeAction::CancelTaker,
    };

    let result = match_order(&snapshot, incoming).map_err(|_| ExecError::OrderRejected)?;

    match result.outcome {
        MatchOutcome::RejectedPostOnlyCross
        | MatchOutcome::RejectedSelfTrade
        | MatchOutcome::KilledFOK => {
            return Err(ExecError::OrderRejected);
        }
        MatchOutcome::FullyFilled
        | MatchOutcome::PartiallyFilledRemainderCancelled
        | MatchOutcome::PartiallyFilledRemainderResting
        | MatchOutcome::Resting
        | MatchOutcome::NoLiquidity => {}
    }

    // 7. Apply book mutations. Read the makers' `placed_at_height` first: the
    //    maker-rebate wash-trade guard needs it, and the mutations about to be
    //    applied delete the rows a fully-filled maker came from.
    let maker_placed_at: std::collections::HashMap<H256, u64> = result
        .fills
        .iter()
        .filter_map(|f| {
            perp_orders
                .get(&f.maker_order_id)
                .map(|o| (f.maker_order_id, o.placed_at_height))
        })
        .collect();
    apply_book_mutations(&perp_orders, &perp_book, &result.mutations);

    // 8. Apply position deltas + realized PnL credit (M4) for every fill.
    apply_fills_with_pnl(state, db, &result.fills, market, ctx.block_height)?;

    // 8b. Charge trading fees. (QA F8)
    charge_perp_trading_fees(
        state,
        &result.fills,
        market,
        view.taker_fee_bps(),
        view.maker_fee_bps_signed(),
        &maker_placed_at,
        ctx.block_height,
    );

    // 9. Persist last-trade price on the market record (perp or future).
    for m in &result.mutations {
        if let BookMutation::UpdateLastTrade {
            price_q8, height, ..
        } = m
        {
            view.set_last_trade(*price_q8, *height);
        }
    }
    if view.persist(db) {
        // M8: market record changed (last_trade_px / dirty flag) → trie touch.
        state.touch_market(market);
    }

    // 10. Advance sender nonce.
    sender_state.nonce += 1;
    state.put(sender, sender_state.clone());

    Ok(())
}

/// Apply every fill's position delta, then credit realised PnL to the
/// participating MarginAccount.collateral. Bad debt (negative collateral) is
/// recorded on the account's `bad_debt` field. Insurance covers a residual
/// deficit only once all the owner's positions are closed.
fn apply_fills_with_pnl(
    state: &mut BlockState,
    db: &cb_state::Database,
    fills: &[cb_engine::Fill],
    market: DerivId,
    block_height: u64,
) -> Result<(), ExecError> {
    let positions = Positions::new(db);
    // The stamp every new or grown position carries. Read once per call: it
    // cannot change inside a block except through `FundingTick`, which is a
    // separate tx. (QA F2)
    let funding_idx = current_funding_index(db, &market);

    // The matcher already returns deterministic price/time order. Risk
    // simulation and position realization must use exactly that same order.
    for fill in fills {
        let taker_side = fill.taker_side;
        let maker_side = match taker_side {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        };

        // Taker.
        let taker_pre = positions.get(&fill.taker, &market);
        settle_closed_lot_funding(state, taker_pre.as_ref(), taker_side, fill.qty, funding_idx);
        let (taker_new, taker_pnl) = apply_fill_to_position(
            taker_pre.as_ref(),
            fill.taker,
            market,
            taker_side,
            fill.qty,
            fill.price_q8,
            block_height,
            funding_idx,
        );
        positions.put(&taker_new);
        credit_realized_pnl(state, fill.taker, taker_pnl, market);

        // Maker.
        let maker_pre = positions.get(&fill.maker, &market);
        settle_closed_lot_funding(state, maker_pre.as_ref(), maker_side, fill.qty, funding_idx);
        let (maker_new, maker_pnl) = apply_fill_to_position(
            maker_pre.as_ref(),
            fill.maker,
            market,
            maker_side,
            fill.qty,
            fill.price_q8,
            block_height,
            funding_idx,
        );
        positions.put(&maker_new);
        credit_realized_pnl(state, fill.maker, maker_pnl, market);
    }
    // M8: register every fill's taker / maker so the positions trie sees
    // them at commit. Done after the loop so the loop body doesn't have to
    // juggle a `&mut state` borrow against the immutable `positions` handle.
    // (Margin rows register themselves, through `put_margin`.)
    for fill in fills {
        state.touch_position(fill.taker, market);
        state.touch_position(fill.maker, market);
    }
    // A zero-PnL final close still settles a liability retained from earlier
    // funding or another leg. Do this after the complete fill plan.
    let owners: std::collections::BTreeSet<_> =
        fills.iter().flat_map(|f| [f.taker, f.maker]).collect();
    for owner in owners {
        absorb_bad_debt(state, owner, Some(market));
    }
    Ok(())
}

/// Charge the funding a position's closing lots have accrued since their
/// stamp, at the moment they close.
///
/// `settle_market_funding` charges each open position `qty * (cumulative -
/// entry_funding_index)` at an interval boundary, and a fill that reduces a
/// position keeps the surviving lots' stamp — so the lots that *closed*
/// between two boundaries were never charged for the part of the interval
/// they were open, while the counterparty that stayed open was paid for all
/// of it. Every such close minted (or burned) that slice of funding: a long
/// of 2 against a short of 2, the long selling 1 halfway through an interval
/// to a new buyer, and the boundary paid the short for 2 lots over the whole
/// interval and charged the longs for 1 lot over the whole interval plus 1
/// over the second half — a quarter of the flow out of nowhere. Charging the
/// closed lots here, at the live index and with the same rounding the
/// boundary uses, makes each lot pay from its stamp to its close, so funding
/// is zero-sum lot for lot. Dated futures carry a zero index and pay nothing.
/// (Test: `perp_conservation.rs`.)
fn settle_closed_lot_funding(
    state: &mut BlockState,
    pre: Option<&Position>,
    side_of_fill: Side,
    fill_qty: u128,
    current_funding_index: Signed,
) {
    let Some(p) = pre else { return };
    if p.qty == 0 || p.side == side_of_fill {
        return;
    }
    let closed = std::cmp::min(p.qty, fill_qty);
    let diff = current_funding_index
        .to_i128()
        .saturating_sub(p.entry_funding_index.to_i128());
    let owed = (closed as i128).saturating_mul(diff) / cb_engine::RATE_DEN_Q9 as i128;
    if owed == 0 {
        return;
    }
    // Positive index delta: longs pay, shorts receive (`settle_market_funding`).
    let delta = match p.side {
        Side::Buy => -owed,
        Side::Sell => owed,
    };
    apply_collateral_delta(state, p.owner, delta, Some(p.market));
}

/// Credit (or debit) a realised-PnL `Signed` amount onto an owner's
/// `MarginAccount.collateral`. PnL math is in micro-USD, scaled `1e6`, but
/// the engine `apply_fill_to_position` returns `qty * delta_q8` raw (i.e.
/// `1e16` units). Convert here by dividing by `1e10` — same denominator
/// `margin::position_notional_micro_usd` uses. This keeps the conversion in
/// exactly one place.
///
/// Negative residual after debit is recorded into `bad_debt`. The shared
/// collateral path checks insurance eligibility after complete close-out.
fn credit_realized_pnl(state: &mut BlockState, owner: Address, raw_pnl: Signed, market: DerivId) {
    let pnl_micro = pnl_raw_to_micro(raw_pnl);
    if pnl_micro == 0 {
        return;
    }
    apply_collateral_delta(state, owner, pnl_micro, Some(market));
}

/// The engine's raw `qty * delta_q8` PnL as signed micro-USD.
///
/// Scale: qty (base units, e.g. 1e8 for BTC) * px_q8 (1e8) = 1e16 units;
/// divide by 1e10 to land in micro-USD — the same denominator
/// `margin::position_notional_micro_usd` uses. Truncates toward zero on both
/// sides of a fill, so it conserves. Every conversion of a realised PnL in
/// this module goes through here: the fill path, futures settlement, and the
/// margin gate's simulated fill, which has to see the same number the fill
/// would book.
fn pnl_raw_to_micro(raw_pnl: Signed) -> i128 {
    raw_pnl.to_i128() / 10i128.pow(10)
}

/// Apply a signed micro-USD delta to `owner`'s margin account, and put the
/// insurance fund behind the residual deficit only after complete close-out.
///
/// # Why the account is a single signed quantity
///
/// `MarginAccount` stores collateral and bad debt as two unsigned fields, but
/// they are two halves of one number: `net = collateral - bad_debt`. Applying
/// the delta to `net` and splitting it again afterwards is what makes a credit
/// *repay* an outstanding debt rather than sit next to it — which it did not,
/// before QA finding F4: an account could be wiped out, land in debt, be funded
/// again, and open a fresh position as though it owed nothing.
///
/// # Insurance eligibility
///
/// Realizations and funding can leave negative cash while other positions
/// still cover the liability. Keep that signed liability while any position
/// remains open. Insurance covers only the residual deficit of a flat
/// account; later gains must repay cash debt before becoming spendable.
/// `apply_fills_with_pnl` also checks after zero-PnL final closes, and futures
/// settlement deletes its position before judging the residual.
///
/// `market` names the market to halt (`paused_reduce_only_only`) if the
/// insurance fund is exhausted and debt is still outstanding; `None` skips the
/// halt, for call sites with no market in hand.
fn apply_collateral_delta(
    state: &mut BlockState,
    owner: Address,
    delta_micro: i128,
    market: Option<DerivId>,
) {
    let db = &state.db();
    let margins = MarginAccounts::new(db);
    let mut margin = margins
        .get(&owner)
        .unwrap_or_else(|| MarginAccount::new(owner));
    let net = (margin.collateral as i128).saturating_sub(margin.bad_debt as i128);
    let next = net.saturating_add(delta_micro);
    if next >= 0 {
        margin.collateral = next as u128;
        margin.bad_debt = 0;
    } else {
        margin.collateral = 0;
        margin.bad_debt = next.unsigned_abs();
    }
    put_margin(state, &margin);
    if margin.bad_debt > 0 {
        absorb_bad_debt(state, owner, market);
    }
}

/// Persist a margin account and register it with the margin trie.
///
/// Every margin write in the executor goes through this one function.
/// `MarginAccounts::put` on its own leaves the row out of the state root, which
/// is how QA finding F3 happened: six call sites wrote margin rows and none of
/// them was committed, so two replicas could disagree about every trader's
/// collateral and about the insurance fund while reporting byte-identical
/// roots. Routing the writes through one door means a seventh call site cannot
/// quietly reopen the gap.
fn put_margin(state: &mut BlockState, m: &MarginAccount) {
    MarginAccounts::new(&state.db()).put(m);
    state.touch_margin(m.owner);
}

/// Build the post-fill view of the sender's open positions, assuming the
/// new order fills in full at its **limit price** (worst case for the
/// margin gate: matches at the user's worst acceptable price), together
/// with the PnL that fill would realise, in micro-USD. `held` is the
/// account's position list as it stands. The list is sorted by deriv_id for
/// determinism.
///
/// The realised PnL used to be discarded here. A fill that closes lots
/// realises their gain or loss into collateral, and the post-fill position
/// no longer carries it — so an equity computed from the post-fill positions
/// and the *pre-fill* collateral was missing exactly the loss an underwater
/// account would take on the way to its new position.
#[allow(clippy::too_many_arguments)]
fn simulate_worst_case_position_set(
    db: &cb_state::Database,
    held: &[Position],
    sender: Address,
    market: DerivId,
    side: Side,
    qty: u128,
    price_q8: u128,
    block_height: u64,
) -> (Vec<Position>, i128) {
    let funding_idx = current_funding_index(db, &market);
    let mut owned: Vec<Position> = held.to_vec();
    let pre_market = owned
        .iter()
        .position(|p| p.market == market)
        .map(|i| owned.remove(i));

    // Apply a hypothetical fill at limit price.
    let (post_market, pnl) = apply_fill_to_position(
        pre_market.as_ref(),
        sender,
        market,
        side,
        qty,
        price_q8,
        block_height,
        funding_idx,
    );
    owned.push(post_market);
    // Sort for canonical iteration order downstream.
    owned.sort_by_key(|a| a.market.0);
    (owned, pnl_raw_to_micro(pnl))
}

/// What the owner's resting non-reduce-only perp orders would lose, in
/// micro-USD, if every one of them filled at its limit price and was marked
/// at the market's current mark: a bid above the mark loses the difference, an
/// ask below it likewise; an order on the favourable side of the mark counts
/// zero, never a gain. Rounded up, against the account. Orders on a market
/// with no mark are skipped, as the equity skips positions there.
///
/// Reduce-only orders are left out, and that is a gap, not a proof: a
/// reduce-only order resting far through the mark does lose equity when it
/// fills. It is the same gap as the closing orders that skip this gate
/// altogether (`margin_gate_worst_case.rs`,
/// `open_gap_a_solvent_account_can_close_into_a_colluder_at_any_price`), and
/// it closes with that one, by a price band or a gate on closes.
fn resting_adverse_loss_micro(
    db: &cb_state::Database,
    perp_orders: &PerpOrders,
    owner: Address,
) -> u128 {
    let store = OracleState::new(db);
    let mut total: u128 = 0;
    for o in perp_orders.list_for_owner(&owner) {
        if o.reduce_only || o.remaining_qty == 0 {
            continue;
        }
        let Some(mark) = store.get_mark(&o.market).map(|m| m.px_q8) else {
            continue;
        };
        let gap = match o.side {
            Side::Buy => o.price_q8.saturating_sub(mark),
            Side::Sell => mark.saturating_sub(o.price_q8),
        };
        if gap == 0 {
            continue;
        }
        total = total.saturating_add(ceil_div(
            o.remaining_qty.saturating_mul(gap),
            10u128.pow(10),
        ));
    }
    total
}

/// `account` with `loss` taken off its net value (`collateral - bad_debt`),
/// for judging the margin gate; never written back.
fn debited_by(account: &MarginAccount, loss: u128) -> MarginAccount {
    let mut judged = account.clone();
    if loss == 0 {
        return judged;
    }
    let net = (account.collateral as i128)
        .saturating_sub(account.bad_debt as i128)
        .saturating_sub(loss.min(i128::MAX as u128) as i128);
    if net >= 0 {
        judged.collateral = net as u128;
        judged.bad_debt = 0;
    } else {
        judged.collateral = 0;
        judged.bad_debt = net.unsigned_abs();
    }
    judged
}

/// Whether an order of `qty` on `side` can only close lots: the account's
/// whole potential on `side` in `market` — this order plus its resting
/// non-reduce-only orders there — fits inside the position it holds on the
/// other side.
fn closes_only(
    positions: &Positions,
    perp_orders: &PerpOrders,
    owner: Address,
    market: DerivId,
    side: Side,
    qty: u128,
) -> bool {
    let opposite = reduce_only_capacity_of(positions, &owner, &market, side);
    if opposite == 0 {
        return false;
    }
    let resting: u128 = perp_orders
        .list_for_owner(&owner)
        .iter()
        .filter(|o| o.market == market && o.side == side && !o.reduce_only)
        .fold(0u128, |acc, o| acc.saturating_add(o.remaining_qty));
    resting.saturating_add(qty) <= opposite
}

/// The per-market potential position an account could end up holding: for
/// each market, the larger of `(long + Σ bids) − short` and
/// `(short + Σ asks) − long`, where long and short are what is actually held
/// (`held`) and the sums run over every non-reduce-only resting order plus
/// the incoming one, which counts as one more resting order of `qty` on
/// `side`. Reduce-only orders are left out because they can only shrink a
/// position. One row per market, sorted by market, entry at zero: the rows
/// exist to carry a notional, and `initial_margin_required` reads only
/// `qty` and the mark. (QA F11)
///
/// Only the *held* position on the other side is subtracted. It is what the
/// first fills on this side close before they open anything. The resting
/// orders on the other side are not: they may never fill, and the case the
/// gate exists for is the one where only this side does. The earlier version
/// netted the two sides' orders against each other, so a bid and an ask of
/// the same size were charged nothing — and so were ten pairs of them.
fn potential_exposures(
    perp_orders: &PerpOrders,
    owner: Address,
    held: &[Position],
    market: DerivId,
    side: Side,
    qty: u128,
) -> Vec<Position> {
    use std::collections::BTreeMap;
    /// What one market's potential is built from.
    #[derive(Default)]
    struct Book {
        long: u128,
        short: u128,
        bids: u128,
        asks: u128,
    }
    let mut by_market: BTreeMap<DerivId, Book> = BTreeMap::new();
    for p in held {
        if p.qty == 0 {
            continue;
        }
        let e = by_market.entry(p.market).or_default();
        match p.side {
            Side::Buy => e.long = e.long.saturating_add(p.qty),
            Side::Sell => e.short = e.short.saturating_add(p.qty),
        }
    }
    let mut add_order = |market: DerivId, side: Side, qty: u128| {
        let e = by_market.entry(market).or_default();
        match side {
            Side::Buy => e.bids = e.bids.saturating_add(qty),
            Side::Sell => e.asks = e.asks.saturating_add(qty),
        }
    };
    for o in perp_orders.list_for_owner(&owner) {
        if o.reduce_only || o.remaining_qty == 0 {
            continue;
        }
        add_order(o.market, o.side, o.remaining_qty);
    }
    add_order(market, side, qty);
    by_market
        .into_iter()
        .filter_map(|(market, book)| {
            // Each side's potential is what the account would hold if every
            // order on that side filled: the held position on the other side
            // is closed first, not stacked. Long 1 with a 3 ask resting is a
            // potential short of 2, not 3.
            let long_pot = book
                .long
                .saturating_add(book.bids)
                .saturating_sub(book.short);
            let short_pot = book
                .short
                .saturating_add(book.asks)
                .saturating_sub(book.long);
            let qty = long_pot.max(short_pot);
            if qty == 0 {
                return None;
            }
            Some(Position {
                owner,
                market,
                side: if long_pot >= short_pot {
                    Side::Buy
                } else {
                    Side::Sell
                },
                qty,
                entry_funding_index: Signed::ZERO,
                entry_px_q8: 0,
                margin_mode: 0,
                isolated_margin: 0,
                last_funding_height: 0,
            })
        })
        .collect()
}

/// Charge the taker fee and settle the maker fee (or rebate) for every fill of
/// one `PlacePerpOrder`.
///
/// `PerpMarket` and `FutureMarket` have carried `taker_fee_bps` and
/// `maker_fee_bps_signed` since M3, the Cap'n Proto surface publishes both and
/// the dashboard displays them — and until QA finding F8 no code path in
/// `cb-block` read either. On the derivative side only the liquidation fee was
/// ever charged. A 1 BTC fill at 50 000 left the taker, the maker and the
/// insurance fund all exactly where they were.
///
/// Where the money goes:
///
/// * The taker fee is a transfer into `Address::INSURANCE`'s margin account —
///   the same sink the spot side already uses. Nothing is minted, nothing is
///   burned, and the conservation check needs no allowance for it.
/// * A negative `maker_fee_bps_signed` is a rebate, paid **out of** the
///   insurance fund and capped by what the fund actually holds, so the rebate
///   can never mint. A positive one is charged like the taker's.
/// * Liquidation tranches do not come through here. The liquidation fee split
///   is already charged against the same account, and taking a taker fee on top
///   would bill the victim twice for the same close.
///
/// The rebate carries the "rested for at least one block" wash-trade guard that
/// `PerpOrder.placed_at_height` was added for and that nothing read: an account
/// that places and immediately crosses its own quote through a second address
/// would otherwise harvest the rebate on volume it created for that purpose.
fn charge_perp_trading_fees(
    state: &mut BlockState,
    fills: &[cb_engine::Fill],
    market: DerivId,
    taker_fee_bps: u16,
    maker_fee_bps_signed: i16,
    maker_placed_at: &std::collections::HashMap<H256, u64>,
    block_height: u64,
) {
    if fills.is_empty() {
        return;
    }
    // Fees use order-ID order for deterministic allocation of capped rebates.
    // Position realization separately follows the matcher's price/time order.
    let mut sorted: Vec<cb_engine::Fill> = fills.to_vec();
    sorted.sort_by(|a, b| {
        a.taker_order_id
            .0
            .cmp(&b.taker_order_id.0)
            .then(a.maker_order_id.0.cmp(&b.maker_order_id.0))
    });

    for fill in &sorted {
        let notional = cb_engine::position_notional_micro_usd(fill.qty, fill.price_q8);
        if notional == 0 {
            continue;
        }
        // Round the taker fee down, matching the spot side and
        // `position_notional_micro_usd`: rounding in the trader's favour keeps
        // a dust fill from costing more than it trades.
        let taker_fee = notional.saturating_mul(taker_fee_bps as u128) / 10_000u128;
        if taker_fee > 0 {
            apply_collateral_delta(state, fill.taker, -(taker_fee as i128), Some(market));
            credit_collateral(state, Address::INSURANCE, taker_fee);
        }

        if maker_fee_bps_signed > 0 {
            let maker_fee = notional.saturating_mul(maker_fee_bps_signed as u128) / 10_000u128;
            if maker_fee > 0 {
                apply_collateral_delta(state, fill.maker, -(maker_fee as i128), Some(market));
                credit_collateral(state, Address::INSURANCE, maker_fee);
            }
        } else if maker_fee_bps_signed < 0 {
            // Wash-trade guard: the quote must have survived at least one block
            // boundary. A maker order placed in this very block earns nothing.
            let rested = maker_placed_at
                .get(&fill.maker_order_id)
                .is_some_and(|h| *h < block_height);
            if !rested {
                continue;
            }
            let rebate =
                notional.saturating_mul(maker_fee_bps_signed.unsigned_abs() as u128) / 10_000u128;
            if rebate == 0 {
                continue;
            }
            // Paid out of the insurance fund and capped by it: an exhausted
            // fund pays no rebate rather than minting one.
            let available = MarginAccounts::new(&state.db())
                .get(&Address::INSURANCE)
                .map(|m| m.collateral)
                .unwrap_or(0);
            let paid = std::cmp::min(rebate, available);
            if paid > 0 {
                apply_collateral_delta(state, Address::INSURANCE, -(paid as i128), None);
                credit_collateral(state, fill.maker, paid);
            }
        }
    }
}

/// The market's live cumulative funding index — the stamp a position opened
/// now must carry so it is liable for funding from this moment and not from
/// the market's beginning. `Signed::ZERO` when the market has never ticked,
/// and for dated futures, which never accrue funding at all. (QA F2)
fn current_funding_index(db: &cb_state::Database, market: &DerivId) -> Signed {
    FundingIndices::new(db)
        .get(market)
        .map(|i| i.cumulative)
        .unwrap_or(Signed::ZERO)
}

/// Apply book mutations from the matcher to the rocksdb-backed CFs. Walks the
/// list in the order produced; the matcher is the authority on ordering.
fn apply_book_mutations(
    perp_orders: &PerpOrders,
    perp_book: &PerpBookIndex,
    mutations: &[BookMutation],
) {
    for m in mutations {
        match m {
            BookMutation::PutOrder(o) => perp_orders.put(o),
            BookMutation::DeleteOrder { order_id } => perp_orders.delete(order_id),
            BookMutation::PutBookIndex {
                market,
                side,
                price_q8,
                seq,
                order_id,
            } => perp_book.put(market, *side, *price_q8, *seq, order_id),
            BookMutation::DeleteBookIndex {
                market,
                side,
                price_q8,
                seq,
            } => perp_book.delete(market, *side, *price_q8, *seq),
            BookMutation::UpdateLastTrade { .. } => {
                // Handled in `apply_place_perp_order` above; the market record
                // is updated there with the rest of the perp_market mutation.
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ClosePosition
// ---------------------------------------------------------------------------

/// Close all or part of the sender's position on `market` at market: a
/// reduce-only immediate-or-cancel order on the opposite side, limited to the
/// mark moved `max_slippage_bps` against the trader.
///
/// The gateway's `POST /positions/close` has always submitted this
/// transaction and the executor refused every one as `Unsupported`, so the
/// one button a trader reaches for to get out of a position did nothing. It
/// needs no machinery of its own: it is `PlacePerpOrder` with the side taken
/// from the position, the size from the position (`qty == 0` means all of
/// it), the flag set, and a limit derived from the mark — so it goes through
/// the same admission, the same matcher, the same fee and PnL path, and the
/// reduce-only rule caps it at the position exactly as it caps any other
/// close.
///
/// The limit is rounded *toward* the mark onto the market's tick, so the
/// slippage bound is never loosened by the rounding. A market with no mark
/// has no price to bound the close against and is refused with
/// `MissingMarkPrice`; closing against an unpriced book with no limit is
/// the thing the bound exists to prevent.
#[allow(clippy::too_many_arguments)]
fn apply_close_position(
    state: &mut BlockState,
    ctx: &ExecCtx,
    tx_index: u32,
    sender: Address,
    sender_state: &mut cb_state::AccountState,
    market: DerivId,
    qty: u128,
    max_slippage_bps: u16,
) -> Result<(), ExecError> {
    let db = &state.db();
    let view = load_market_view(db, &market)?;
    let position = match Positions::new(db).get(&sender, &market) {
        Some(p) if p.qty > 0 => p,
        _ => return Err(ExecError::ReduceOnlyNoPos),
    };
    let mark_q8 = fresh_mark_q8(db, &market, ctx.block_height)?;
    let side = match position.side {
        Side::Buy => Side::Sell,
        Side::Sell => Side::Buy,
    };
    let qty = if qty == 0 { position.qty } else { qty };
    let bps = u128::from(max_slippage_bps.min(10_000));
    let tick = view.tick();
    let price_q8 = match side {
        // Selling out of a long: no lower than mark - slippage, rounded up.
        // Up at *both* steps: flooring the product first put the limit one q8
        // unit below the bound whenever the tick was 1 or the floor landed on
        // a tick. (Test: `close_position.rs`, the slippage-bound property.)
        Side::Sell => {
            let floor = ceil_div(mark_q8.saturating_mul(10_000 - bps), 10_000);
            if tick > 1 {
                ceil_div(floor, tick).saturating_mul(tick)
            } else {
                floor
            }
        }
        // Buying back a short: no higher than mark + slippage, rounded down.
        Side::Buy => {
            let cap = mark_q8.saturating_mul(10_000 + bps) / 10_000;
            if tick > 1 {
                cap / tick * tick
            } else {
                cap
            }
        }
    };
    // Clamp the slippage bound into the price band, onto the tick and toward
    // the mark. `max_slippage_bps` is the trader's protection, not a request
    // to trade far away, so a wide one (up to 10_000 is well-formed) is
    // tightened to the band rather than refused: the close still goes out, at
    // the widest price admission accepts. The inner `PlacePerpOrder` checks
    // the band again; with a tick coarser than the band itself the clamped
    // price can still fall outside it, and that is refused there.
    let (band_lo, band_hi) = derivative_price_band(mark_q8);
    let price_q8 = match side {
        Side::Sell => {
            let lo = if tick > 1 {
                ceil_div(band_lo, tick).saturating_mul(tick)
            } else {
                band_lo
            };
            price_q8.max(lo)
        }
        Side::Buy => {
            let hi = if tick > 1 {
                band_hi / tick * tick
            } else {
                band_hi
            };
            price_q8.min(hi)
        }
    };
    if price_q8 == 0 {
        return Err(ExecError::BadPerpOrder);
    }
    apply_place_perp_order(
        state,
        ctx,
        tx_index,
        sender,
        sender_state,
        market,
        side,
        price_q8,
        qty,
        Tif::Ioc,
        false,
        true,
        H256::default(),
    )
}

// ---------------------------------------------------------------------------
// CancelPerpOrder (M3)
// ---------------------------------------------------------------------------

fn apply_cancel_perp_order(
    state: &mut BlockState,
    sender: Address,
    sender_state: &mut cb_state::AccountState,
    order_id: H256,
) -> Result<(), ExecError> {
    let db = &state.db();
    let perp_orders = PerpOrders::new(db);
    let perp_book = PerpBookIndex::new(db);

    // Lookup the order. If it's already filled / never existed, the cancel is
    // a no-op (still a valid tx — nonce advances). The spec calls this
    // idempotent (per task brief): a cancel of a filled order returns
    // `Unsupported` from the prior stub; in M3 we relax to a clean nonce-
    // advance "no-op success" because the trader's intent is honoured (the
    // order is gone). We log nothing because there's no event log yet; M6
    // adds RPC observability.
    let order = match perp_orders.get(&order_id) {
        Some(o) => o,
        None => {
            // Idempotent no-op: the order is already gone. Advance the nonce
            // and exit successfully. This matches Hyperliquid's behaviour and
            // prevents a trader from getting their cancel txs stuck behind a
            // race against a concurrent fill.
            sender_state.nonce += 1;
            state.put(sender, sender_state.clone());
            return Ok(());
        }
    };

    // Authorization: only the order's owner may cancel it.
    if order.owner != sender {
        return Err(ExecError::NotAuthorized);
    }

    perp_orders.delete(&order_id);
    perp_book.delete(&order.market, order.side, order.price_q8, order.seq);

    sender_state.nonce += 1;
    state.put(sender, sender_state.clone());
    Ok(())
}

// ---------------------------------------------------------------------------
// Seq + order-id derivation (M3)
// ---------------------------------------------------------------------------

/// Pack `(block_height, tx_index, intra_ctr)` into a single `u64`.
///
/// Layout (most-significant bit first, big-endian semantically):
///   bits 63..24 — `block_height & 0x00ff_ffff_ffff` (40 bits)
///   bits 23..8  — `tx_index & 0xffff` (16 bits)
///   bits  7..0  — `intra_ctr & 0xff` (8 bits)
///
/// Determinism: identical `(block_height, tx_index, intra_ctr)` always
/// produces the same seq. Monotonicity: across a chain, seq strictly
/// increases with `(block_height, tx_index, intra_ctr)` lexicographically —
/// older txs get smaller seqs.
///
/// At chain mass: 40 bits of block_height ≈ 1.1e12 blocks (~35 millennia at
/// 1s blocks); 16 bits of tx_index = 65K txs/block; 8 bits of intra_ctr =
/// 256 perp orders within a single tx. Margins are ample for v1.
fn pack_seq(block_height: u64, tx_index: u32, intra_ctr: u8) -> u64 {
    let h = (block_height & 0x00ff_ffff_ffff) << 24;
    let t = ((tx_index as u64) & 0xffff) << 8;
    let i = intra_ctr as u64;
    h | t | i
}

/// Deterministic `order_id` derivation. Identical inputs yield identical
/// 32-byte ids; replicas reproduce the same id without any global counter.
///
/// Layout: `keccak`-style hash of `[sender || height_be || tx_index_be || intra_be || tag]`
/// would also work, but we don't want a `keccak` dep here. The matching
/// engine treats the id as opaque, so we lay out the input directly:
///   bytes  0..20 — sender
///   bytes 20..28 — block_height BE u64
///   bytes 28..32 — tx_index BE u32 (top 4 bytes); intra_ctr fills the last
/// resulting in total 32 bytes — exactly an `H256`. Collisions are
/// impossible at v1 scale (one perp order per tx; same sender, same height,
/// same tx_index, same intra_ctr is the same tx).
fn derive_order_id(sender: Address, block_height: u64, tx_index: u32, intra_ctr: u8) -> H256 {
    let mut bytes = [0u8; 32];
    bytes[..20].copy_from_slice(&sender.0);
    bytes[20..28].copy_from_slice(&block_height.to_be_bytes());
    bytes[28..32].copy_from_slice(&tx_index.to_be_bytes());
    // Tuck intra_ctr into the bottom byte by XORing, since tx_index BE put it
    // in byte 31. For intra_ctr == 0 (v1 default) this is a no-op; for >0 we
    // disambiguate without growing the layout.
    bytes[31] ^= intra_ctr;
    H256(bytes)
}

// ---------------------------------------------------------------------------
// LiquidationTrigger (M4)
// ---------------------------------------------------------------------------

/// Apply a `LiquidationTrigger` tx. Permissionless: anyone can submit. The
/// engine re-checks `is_liquidatable` against the current mark; if the
/// account is solvent the tx errors with `NotLiquidatable` (no nonce
/// advance — the keeper got the call wrong, no harm done to other state).
///
/// On success we inline-execute the liquidation rather than queueing it for
/// end-of-block. Reasoning: the liquidation tx already commits to the
/// tx_root, replicas re-execute it verbatim, and inline execution simplifies
/// the data flow (no separate drain step). The §4.4 design's "queue then
/// drain" is preserved for adversarial-trigger-frontrunning protection — we
/// re-check eligibility AT the tx, so any trigger placed against a
/// no-longer-liquidatable account fails closed. The queue CF still gets
/// written for audit / observability; nothing reads it back during apply.
///
/// Process per the trading spec §5:
///   1. Recheck eligibility against current mark.
///   2. Walk each market the owner has open positions in. For each one
///      generate `compute_liquidation_orders` and submit each tranche as a
///      reduce-only IOC against the matcher (reduce-only side picked to
///      close the position).
///   3. After fills, compute the realised liquidation notional (sum of
///      `qty * fill_price` in micro-USD) and split via `liquidation_fee_split`:
///      0.3% credits `Address::INSURANCE`, 0.2% credits the keeper (signer).
///      Both come from the LIQUIDATED account's residual collateral.
///   4. If the liquidated account ends with `bad_debt > 0`, drain insurance
///      first; if insurance can't cover, set `paused_reduce_only_only=true`
///      on the affected market and leave the residual on `bad_debt`.
fn apply_liquidation_trigger(
    state: &mut BlockState,
    ctx: &ExecCtx,
    tx_index: u32,
    sender: Address,
    sender_state: &mut cb_state::AccountState,
    owner: Address,
    market: DerivId,
) -> Result<(), ExecError> {
    let db = &state.db();
    let positions_cf = Positions::new(db);
    let margins_cf = MarginAccounts::new(db);

    // Market must exist; trigger can fire on a paused market (in fact, that's
    // EXACTLY when liquidations are most needed) but the mark must be present.
    //
    // Resolved through `load_market_view`, which handles perpetuals and dated
    // futures alike. This used to read `PerpMarkets` directly, so a
    // `LiquidationTrigger` naming a dated future was rejected with
    // `UnknownMarket` and a leveraged futures position could never be
    // liquidated at all — the account simply sat below maintenance margin
    // forever. Found by running the simulation with a future listed into a
    // crash: 1 536 triggers rejected in 400 blocks and invariant 6 violated 28
    // times. Margin is cross-market, so there is no version of this where the
    // liquidation engine gets to know about only one of the two contract kinds.
    let mut view = load_market_view(db, &market)?;

    // 1. Re-check liquidation eligibility. We use the OWNER's full position
    //    set across markets (cross-margin), each priced at its market's mark.
    let owner_positions = positions_cf.list_for_owner(&owner);
    if owner_positions.is_empty() {
        return Err(ExecError::NotLiquidatable);
    }
    let marks = fresh_marks_for_positions(db, &owner_positions, ctx.block_height)?;
    // Mark for the requested market is required — without it we can't price
    // the liquidation IOCs.
    let mark_q8 = fresh_mark_q8(db, &market, ctx.block_height)?;
    let owner_margin = margins_cf
        .get(&owner)
        .unwrap_or_else(|| MarginAccount::new(owner));
    let maintenance = configured_margin_required(db, &owner_positions, &marks, true)?;
    let equity = cb_engine::compute_equity(&owner_margin, &owner_positions, &marks).to_i128();
    if maintenance == 0 || (equity >= 0 && equity as u128 >= maintenance) {
        return Err(ExecError::NotLiquidatable);
    }

    // 2. Append a queue row for audit. The engine doesn't read this back —
    //    inline execution does the work — but it lands in the tx_root via
    //    its enclosing tx and lets observers reconstruct liquidation history.
    let queue = LiquidationQueue::new(db);
    let entry = LiquidationEntry {
        owner,
        market,
        triggered_by: sender,
        triggered_at_height: ctx.block_height,
        mark_at_trigger_q8: mark_q8,
        seq: pack_seq(ctx.block_height, tx_index, 0),
    };
    queue.put(&entry);

    // 3. Cancel every order the OWNER has resting on this market, before any
    //    tranche goes near the book.
    //
    //    An account being liquidated must not still be quoting the market it is
    //    being liquidated on. Concretely, and this is QA finding F5: the
    //    tranches are submitted through the ordinary matcher with
    //    `SelfTradeAction::CancelTaker`, so if the owner had a resting order on
    //    the closing side at a crossing price the matcher hit it first in
    //    price-time order, returned `RejectedSelfTrade` with no fills, and the
    //    trigger — which never inspected the outcome — reported success having
    //    closed nothing and paid the keeper nothing. All three tranches walk
    //    the same book, so one small order defeated the whole liquidation. A
    //    trivially available defence against ever being liquidated.
    //
    //    Cancelling is the fix rather than exempting the tranches from
    //    self-trade prevention (`SelfTradeAction::CancelMaker`), which would
    //    also have unblocked the matcher. Letting the tranche *fill against*
    //    the victim's own quote means the position trades with itself at a
    //    price the victim chose, which is a liquidation price they set
    //    themselves — the opposite of what the ladder's mark-minus-bps bands
    //    exist to enforce. Cancelling first is also what a real venue does, and
    //    it removes the quotes that would otherwise re-open exposure on an
    //    account that has just been closed out.
    let perp_orders_cf = PerpOrders::new(db);
    let perp_book_cf = PerpBookIndex::new(db);
    // `list_for_market` returns rows in `order_id` byte order, so the deletes
    // are deterministic across replicas.
    for o in perp_orders_cf.list_for_market(&market) {
        if o.owner != owner {
            continue;
        }
        perp_book_cf.delete(&o.market, o.side, o.price_q8, o.seq);
        perp_orders_cf.delete(&o.order_id);
    }

    // 4. Generate liquidation orders for the OWNER's position in this market.
    //    v1: trigger is per-market (the tx names a single market). The owner
    //    may have positions in other markets too; those are not touched here.
    let target_position = match positions_cf.get(&owner, &market) {
        Some(p) if p.qty > 0 => p,
        _ => return Err(ExecError::NotLiquidatable),
    };
    let tranches = compute_liquidation_orders(&target_position, mark_q8, market);

    // 5. Submit each tranche as a reduce-only IOC against the public CLOB.
    //    The matcher handles fills + position deltas + last-trade publishing
    //    via the same path PlacePerpOrder uses, EXCEPT we also accumulate
    //    the realised notional so we can split the liquidation fee at the end.
    let mut realised_notional_micro: u128 = 0;
    let mut intra_ctr: u8 = 0;
    let perp_orders = PerpOrders::new(db);
    let perp_book = PerpBookIndex::new(db);
    let mut last_trade_update: Option<(u128, u64)> = None;
    for tranche in tranches {
        // Re-read the position each tranche; previous tranches may have closed
        // it down (or fully). Once flat, we're done.
        let cur = match positions_cf.get(&owner, &market) {
            Some(p) if p.qty > 0 => p,
            _ => break,
        };
        // Cap tranche qty to the current remaining position size (it may have
        // shrunk from a prior tranche).
        let tranche_qty = std::cmp::min(tranche.qty, cur.qty);
        if tranche_qty == 0 {
            continue;
        }

        let last_trade = view.last_trade_price();
        // Fills are banded here exactly as on `PlacePerpOrder`: a maker the
        // mark has left behind is cancelled, not filled. The tranches
        // themselves need no exemption — the ladder prices them at most
        // 200 bps off the mark, well inside the 10% band — and they do not
        // come through `apply_place_perp_order`'s admission check anyway.
        // A stale maker on the favourable side is skipped too, even though
        // filling it would help the account being closed: the band protects
        // the maker, and one rule for every fill is simpler to reason about
        // than a liquidation carve-out.
        let snapshot = ViewBookSnapshot {
            db,
            height: ctx.block_height,
            perp_orders: &perp_orders,
            perp_book: &perp_book,
            positions: &positions_cf,
            market,
            last_trade,
            band: Some(derivative_price_band(mark_q8)),
        };
        let order_id = derive_liquidation_order_id(owner, ctx.block_height, tx_index, intra_ctr);
        let seq = pack_seq(ctx.block_height, tx_index, intra_ctr);
        intra_ctr = intra_ctr.saturating_add(1);
        let incoming = IncomingOrder {
            order_id,
            owner,
            market,
            side: tranche.side,
            price_q8: tranche.price_q8,
            qty: tranche_qty,
            tif: Tif::Ioc,
            post_only: false,
            reduce_only: true,
            seq,
            placed_at_height: ctx.block_height,
            client_order_id: H256::default(),
            stp: SelfTradeAction::CancelTaker,
        };
        let result = match match_order(&snapshot, incoming) {
            Ok(r) => r,
            Err(_) => break,
        };
        // A rejected tranche is a failed tranche, not an empty book. The
        // matcher discards fills it had already planned against other makers
        // when it rejects, so treating a rejection as "no liquidity" silently
        // converted a blocked liquidation into a successful one that closed
        // nothing. Step 3 makes self-trade unreachable here; the other two
        // variants cannot arise from an IOC that is not post-only. Skip to the
        // next band rather than banking a zero. (QA F5)
        match result.outcome {
            MatchOutcome::RejectedSelfTrade
            | MatchOutcome::RejectedPostOnlyCross
            | MatchOutcome::KilledFOK => continue,
            MatchOutcome::FullyFilled
            | MatchOutcome::PartiallyFilledRemainderCancelled
            | MatchOutcome::PartiallyFilledRemainderResting
            | MatchOutcome::Resting
            | MatchOutcome::NoLiquidity => {}
        }
        // Apply book mutations + fills. Note: the OWNER appears here as the
        // taker; their position will get reduced by the fill flow.
        apply_book_mutations(&perp_orders, &perp_book, &result.mutations);
        for fill in &result.fills {
            // Sum notional for fee math.
            let n = cb_engine::position_notional_micro_usd(fill.qty, fill.price_q8);
            realised_notional_micro = realised_notional_micro.saturating_add(n);
        }
        apply_fills_with_pnl(state, db, &result.fills, market, ctx.block_height)?;
        // Track the last UpdateLastTrade for end-of-trigger publish.
        for m in &result.mutations {
            if let BookMutation::UpdateLastTrade {
                price_q8, height, ..
            } = m
            {
                last_trade_update = Some((*price_q8, *height));
            }
        }
        if result.fills.is_empty() {
            // No liquidity at this band; advance to the next tranche.
            continue;
        }
    }

    if let Some((p, h)) = last_trade_update {
        view.set_last_trade(p, h);
        // `persist` re-reads the row and writes back only the last-trade
        // fields, so it cannot undo a `paused_reduce_only_only` that bad-debt
        // absorption set while these fills were being applied.
        if view.persist(db) {
            // M8: market record changed → register with markets trie.
            state.touch_market(market);
        }
    }

    // 6. Liquidation fee split. We charge against the LIQUIDATED owner's
    //    residual collateral. Insurance share lands on `Address::INSURANCE`,
    //    keeper share lands on `sender`. Round insurance up, keeper down.
    let (ins_fee, keeper_fee) = liquidation_fee_split(realised_notional_micro);
    // An owner who triggers their own liquidation is not a keeper. The close
    // still happens — that is what the trigger is for — but the keeper share
    // goes to the insurance fund rather than back to the account being
    // liquidated, so self-triggering cannot discount the fee. (Core review L3)
    let keeper = if sender == owner {
        Address::INSURANCE
    } else {
        sender
    };
    if ins_fee > 0 || keeper_fee > 0 {
        debit_owner_for_fees(state, owner, ins_fee, keeper_fee, keeper);
    }

    // 7. Bad-debt absorption. After fees, if the owner is left with
    //    `bad_debt > 0`, drain `Address::INSURANCE` first; whatever is left
    //    over becomes the trigger for the reduce-only halt on this market.
    absorb_bad_debt(state, owner, Some(market));

    // 8. Sender nonce advances on success.
    sender_state.nonce += 1;
    state.put(sender, sender_state.clone());
    Ok(())
}

/// Debit `owner.collateral` by `ins_fee + keeper_fee` and credit the two
/// destinations. If the owner can't cover the full fee, insurance is paid
/// first (per the trading spec — protocol solvency comes ahead of keeper
/// rebate). Any unpaid keeper fee is dropped (the keeper was supposed to be
/// taking risk; v1 doesn't escalate to socialization for this small slice).
fn debit_owner_for_fees(
    state: &mut BlockState,
    owner: Address,
    ins_fee: u128,
    keeper_fee: u128,
    keeper: Address,
) {
    let mut owner_margin = MarginAccounts::new(&state.db())
        .get(&owner)
        .unwrap_or_else(|| MarginAccount::new(owner));
    let total_fee = ins_fee.saturating_add(keeper_fee);
    let paid = std::cmp::min(owner_margin.collateral, total_fee);
    owner_margin.collateral -= paid;
    put_margin(state, &owner_margin);

    // Distribute `paid` between insurance (priority) and keeper.
    let paid_to_ins = std::cmp::min(paid, ins_fee);
    let paid_to_keeper = paid - paid_to_ins;

    if paid_to_ins > 0 {
        credit_collateral(state, cb_types::Address::INSURANCE, paid_to_ins);
    }
    if paid_to_keeper > 0 {
        credit_collateral(state, keeper, paid_to_keeper);
    }
}

/// Add `amount` to a margin account, creating it if absent. A credit to an
/// account carrying bad debt pays the debt down first — see
/// `apply_collateral_delta`, which owns that rule.
fn credit_collateral(state: &mut BlockState, owner: Address, amount: u128) {
    if amount == 0 {
        return;
    }
    apply_collateral_delta(state, owner, amount as i128, None);
}

/// For a fully closed account, drain insurance to clear `owner.bad_debt`.
/// If insurance exhausts
/// before the debt is fully covered, the residual stays on `bad_debt` AND
/// the affected market's `paused_reduce_only_only` flag flips to true.
fn absorb_bad_debt(state: &mut BlockState, owner: Address, market: Option<DerivId>) {
    let db = &state.db();
    let margins = MarginAccounts::new(db);
    let mut owner_margin = margins
        .get(&owner)
        .unwrap_or_else(|| MarginAccount::new(owner));
    if owner_margin.bad_debt == 0 {
        return;
    }
    // The insurance fund cannot absorb its own debt: draining itself to pay
    // itself would be a no-op that still rewrote two rows.
    if owner == Address::INSURANCE {
        return;
    }
    // Negative cash with live positions is a liability, not bankruptcy.
    // Keep the liability until all legs are closed; subsequent gains repay
    // it automatically. This also avoids insuring profitable legs too early.
    if Positions::new(db)
        .list_for_owner(&owner)
        .iter()
        .any(|p| p.qty > 0)
    {
        return;
    }
    let mut ins = margins
        .get(&Address::INSURANCE)
        .unwrap_or_else(|| MarginAccount::new(Address::INSURANCE));
    let cover = std::cmp::min(ins.collateral, owner_margin.bad_debt);
    if cover > 0 {
        ins.collateral -= cover;
        owner_margin.bad_debt -= cover;
        put_margin(state, &ins);
        put_margin(state, &owner_margin);
    }

    if owner_margin.bad_debt > 0 {
        // Insurance exhausted. Set the market's reduce-only halt flag so
        // no new opening orders are accepted until governance lifts it.
        let Some(market) = market else {
            return;
        };
        match market.kind() {
            Ok(cb_types::MarketKind::Perp) => {
                let pms = PerpMarkets::new(db);
                if let Some(mut pm) = pms.get(&market) {
                    if !pm.paused_reduce_only_only {
                        pm.paused_reduce_only_only = true;
                        pms.put(&pm);
                        // M8: PerpMarket changed → register with markets trie.
                        state.touch_market(market);
                    }
                }
            }
            // A dated future halts the same way. It had no flag to set until
            // core review L4, so a future whose losses had emptied the fund
            // kept accepting opening orders.
            Ok(cb_types::MarketKind::Future) => {
                let fms = FuturesMarkets::new(db);
                if let Some(mut fm) = fms.get(&market) {
                    if !fm.paused_reduce_only_only {
                        fm.paused_reduce_only_only = true;
                        fms.put(&fm);
                        state.touch_market(market);
                    }
                }
            }
            Err(_) => {}
        }
    }
}

/// Distinct order-id derivation for liquidation tranches so their ids don't
/// collide with regular `PlacePerpOrder` derivations within the same block.
/// Sets the high bit of byte 0 (which would otherwise be the sender's first
/// byte) — sender bytes 0..20 don't alias because we also XOR a marker byte
/// at the end. Concretely: same shape as `derive_order_id` but with a tag
/// byte mixed into the trailing slot to keep ids unique.
fn derive_liquidation_order_id(
    sender: Address,
    block_height: u64,
    tx_index: u32,
    intra_ctr: u8,
) -> H256 {
    let mut bytes = [0u8; 32];
    bytes[..20].copy_from_slice(&sender.0);
    bytes[20..28].copy_from_slice(&block_height.to_be_bytes());
    bytes[28..32].copy_from_slice(&tx_index.to_be_bytes());
    bytes[31] ^= intra_ctr;
    // Tag byte: flips a high bit so liquidation-derived ids never collide
    // with regular `derive_order_id` outputs even if every other input matched.
    bytes[0] ^= 0x80;
    H256(bytes)
}

/// Adapter over `PerpMarket` and `FutureMarket` that exposes the order-side
/// fields the matcher and place-order path care about. Lets us write the
/// dispatch once instead of duplicating the matcher path for futures.
///
/// The view holds an owned copy of the underlying record (cheap; both
/// records are <300 bytes) plus a `dirty` flag set by `set_last_trade`.
/// `persist` is a no-op for clean views — important so we don't pointlessly
/// rewrite the row on every place-order tx that doesn't fill.
enum MarketView {
    Perp { record: PerpMarket, dirty: bool },
    Future { record: FutureMarket, dirty: bool },
}

impl MarketView {
    fn risk(&self) -> (u16, u16, u32, u128) {
        match self {
            Self::Perp { record, .. } => (
                record.initial_margin_bps,
                record.maintenance_margin_bps,
                record.max_leverage_x100,
                record.max_position_notional,
            ),
            Self::Future { record, .. } => (
                record.initial_margin_bps,
                record.maintenance_margin_bps,
                record.max_leverage_x100,
                u128::MAX,
            ),
        }
    }

    fn paused(&self) -> bool {
        match self {
            Self::Perp { record, .. } => record.paused,
            Self::Future { record, .. } => record.paused,
        }
    }

    /// Dated futures expire at `expiry_height` — "the height after which the
    /// market is frozen except for `FuturesSettle`", per the field's own
    /// documentation — and stay expired once `final_settlement_px_q8` is set.
    /// Checking only the settlement price, as this used to, left the book
    /// open between expiry and settlement, which with a deferred settlement
    /// can be many blocks. Perps never expire (governance-only halt).
    fn expired(&self, block_height: u64) -> bool {
        match self {
            Self::Perp { .. } => false,
            Self::Future { record, .. } => {
                record.final_settlement_px_q8.is_some() || block_height >= record.expiry_height
            }
        }
    }

    fn paused_reduce_only_only(&self) -> bool {
        match self {
            Self::Perp { record, .. } => record.paused_reduce_only_only,
            Self::Future { record, .. } => record.paused_reduce_only_only,
        }
    }

    /// The market's price increment, q8. Zero means none.
    fn tick(&self) -> u128 {
        match self {
            Self::Perp { record, .. } => record.tick_size,
            Self::Future { record, .. } => record.tick_size,
        }
    }

    /// Whether `qty` is a multiple of the market's `lot_size`. A zero lot
    /// sets no increment.
    fn on_lot(&self, qty: u128) -> bool {
        let lot = match self {
            Self::Perp { record, .. } => record.lot_size,
            Self::Future { record, .. } => record.lot_size,
        };
        lot == 0 || qty % lot == 0
    }

    /// Whether `price_q8` is a multiple of the market's `tick_size`. A zero
    /// tick sets no increment.
    fn on_tick(&self, price_q8: u128) -> bool {
        let tick = match self {
            Self::Perp { record, .. } => record.tick_size,
            Self::Future { record, .. } => record.tick_size,
        };
        tick == 0 || price_q8 % tick == 0
    }

    /// Taker fee in bps. Both record kinds carry it and both charge it.
    fn taker_fee_bps(&self) -> u16 {
        match self {
            Self::Perp { record, .. } => record.taker_fee_bps,
            Self::Future { record, .. } => record.taker_fee_bps,
        }
    }

    /// Maker fee in bps, signed: negative is a rebate paid to the maker,
    /// positive is a fee charged to it.
    fn maker_fee_bps_signed(&self) -> i16 {
        match self {
            Self::Perp { record, .. } => record.maker_fee_bps_signed,
            Self::Future { record, .. } => record.maker_fee_bps_signed,
        }
    }

    fn last_trade_price(&self) -> Option<u128> {
        let (px, h) = match self {
            Self::Perp { record, .. } => (record.last_trade_px_q8, record.last_trade_height),
            Self::Future { record, .. } => (record.last_trade_px_q8, record.last_trade_height),
        };
        if px == 0 && h == 0 {
            None
        } else {
            Some(px)
        }
    }

    fn set_last_trade(&mut self, px_q8: u128, height: u64) {
        match self {
            Self::Perp { record, dirty } => {
                record.last_trade_px_q8 = px_q8;
                record.last_trade_height = height;
                *dirty = true;
            }
            Self::Future { record, dirty } => {
                record.last_trade_px_q8 = px_q8;
                record.last_trade_height = height;
                *dirty = true;
            }
        }
    }

    /// Write back the only fields this view is allowed to change — the
    /// last-trade price and height — onto a **freshly read** record.
    ///
    /// The view holds an owned copy taken at the top of the handler, and other
    /// steps in between can legitimately mutate the same row: bad-debt
    /// absorption sets `paused_reduce_only_only` when the insurance fund is
    /// exhausted. Writing the whole owned copy back would silently undo that —
    /// the market would trade on after the halt that was supposed to stop it.
    /// Re-reading and touching two fields keeps the write to what the matcher
    /// actually decided.
    fn persist(&self, db: &cb_state::Database) -> bool {
        match self {
            Self::Perp { record, dirty } if *dirty => {
                let markets = PerpMarkets::new(db);
                let mut current = markets.get(&record.id).unwrap_or_else(|| record.clone());
                current.last_trade_px_q8 = record.last_trade_px_q8;
                current.last_trade_height = record.last_trade_height;
                markets.put(&current);
                true
            }
            Self::Future { record, dirty } if *dirty => {
                let markets = FuturesMarkets::new(db);
                let mut current = markets.get(&record.id).unwrap_or_else(|| record.clone());
                current.last_trade_px_q8 = record.last_trade_px_q8;
                current.last_trade_height = record.last_trade_height;
                markets.put(&current);
                true
            }
            _ => false,
        }
    }
}

/// Resolve a `DerivId` into either the perp or futures market record.
/// Returns `UnknownMarket` if the id has no row in the corresponding CF.
fn load_market_view(db: &cb_state::Database, market: &DerivId) -> Result<MarketView, ExecError> {
    match market.kind() {
        Ok(cb_types::MarketKind::Perp) => PerpMarkets::new(db)
            .get(market)
            .map(|record| MarketView::Perp {
                record,
                dirty: false,
            })
            .ok_or(ExecError::UnknownMarket),
        Ok(cb_types::MarketKind::Future) => FuturesMarkets::new(db)
            .get(market)
            .map(|record| MarketView::Future {
                record,
                dirty: false,
            })
            .ok_or(ExecError::UnknownMarket),
        Err(_) => Err(ExecError::UnknownMarket),
    }
}

/// `BookSnapshot` impl backed by `cb-state`. Reads only — the executor
/// applies the matcher's mutations against the same CFs after `match_order`
/// returns. Works uniformly for perps and dated futures because the only
/// market-record-derived input the matcher consumes is `last_trade_price`.
struct ViewBookSnapshot<'a> {
    db: &'a cb_state::Database,
    height: u64,
    perp_orders: &'a PerpOrders<'a>,
    perp_book: &'a PerpBookIndex<'a>,
    /// Position rows, so the matcher can enforce the reduce-only cap against
    /// live state instead of trusting the flag on the order.
    positions: &'a Positions<'a>,
    market: DerivId,
    last_trade: Option<u128>,
    /// The price band around the mark for `market`; see
    /// `BookSnapshot::price_band`.
    band: Option<(u128, u128)>,
}

impl<'a> BookSnapshot for ViewBookSnapshot<'a> {
    fn get_perp_order(&self, oid: &H256) -> Option<PerpOrder> {
        self.perp_orders.get(oid)
    }

    fn list_side(&self, market: &DerivId, side: Side) -> Vec<(u128, u64, H256)> {
        self.perp_book.list_side(market, side)
    }

    fn last_trade_price(&self, market: &DerivId) -> Option<u128> {
        if self.market != *market {
            return None;
        }
        self.last_trade
    }

    fn reduce_only_capacity(&self, owner: &Address, market: &DerivId, side: Side) -> u128 {
        reduce_only_capacity_of(self.positions, owner, market, side)
    }

    fn maker_can_fill(&self, maker: &PerpOrder, qty: u128, prior: &[(u128, u128)]) -> bool {
        let total = prior.iter().fold(qty, |n, (_, q)| n.saturating_add(*q));
        if total <= reduce_only_capacity_of(self.positions, &maker.owner, &maker.market, maker.side)
        {
            return true;
        }
        // A maker may have lost collateral, closed its original position, or
        // filled another quote since admission. Simulate all its planned
        // fills, including realized PnL, closing funding and positive fees.
        let mut held = self.positions.list_for_owner(&maker.owner);
        let mut pos = self.positions.get(&maker.owner, &maker.market);
        let index = current_funding_index(self.db, &maker.market);
        let Ok(view) = load_market_view(self.db, &maker.market) else {
            return false;
        };
        let fee_bps = view.maker_fee_bps_signed().max(0) as u128;
        let mut delta = 0i128;
        for &(price, amount) in prior.iter().chain(std::iter::once(&(maker.price_q8, qty))) {
            if let Some(p) = &pos {
                if p.qty > 0 && p.side != maker.side {
                    let owed = (p.qty.min(amount) as i128).saturating_mul(
                        index
                            .to_i128()
                            .saturating_sub(p.entry_funding_index.to_i128()),
                    ) / cb_engine::RATE_DEN_Q9 as i128;
                    delta = delta.saturating_add(if p.side == Side::Buy { -owed } else { owed });
                }
            }
            let (next, pnl) = apply_fill_to_position(
                pos.as_ref(),
                maker.owner,
                maker.market,
                maker.side,
                amount,
                price,
                self.height,
                index,
            );
            delta = delta.saturating_add(pnl_raw_to_micro(pnl));
            let fee = cb_engine::position_notional_micro_usd(amount, price).saturating_mul(fee_bps)
                / 10_000;
            delta = delta.saturating_sub(fee.min(i128::MAX as u128) as i128);
            pos = Some(next);
        }
        held.retain(|p| p.market != maker.market);
        if let Some(p) = pos {
            held.push(p);
        }
        let Ok(marks) = fresh_marks_for_positions(self.db, &held, self.height) else {
            return false;
        };
        if check_position_caps(self.db, &held, &marks).is_err() {
            return false;
        }
        let Ok(required) = configured_margin_required(self.db, &held, &marks, false) else {
            return false;
        };
        let account = MarginAccounts::new(self.db)
            .get(&maker.owner)
            .unwrap_or_else(|| MarginAccount::new(maker.owner));
        let equity = cb_engine::compute_equity(&account, &held, &marks)
            .to_i128()
            .saturating_add(delta);
        equity >= 0 && equity as u128 >= required
    }

    fn price_band(&self, market: &DerivId) -> Option<(u128, u128)> {
        if self.market != *market {
            return None;
        }
        self.band
    }
}

fn fresh_mark_q8(
    db: &cb_state::Database,
    market: &DerivId,
    height: u64,
) -> Result<u128, ExecError> {
    let store = OracleState::new(db);
    let mark = store
        .get_mark(market)
        .filter(|m| m.px_q8 > 0)
        .ok_or(ExecError::MissingMarkPrice)?;
    let source = store
        .get_oracle(market)
        .map_or(mark.computed_at_height, |o| {
            o.source_height.min(mark.computed_at_height)
        });
    if mark.computed_at_height > height || source > height || height - source > MAX_MARK_AGE_BLOCKS
    {
        return Err(ExecError::StaleMarkPrice);
    }
    Ok(mark.px_q8)
}

fn fresh_marks_for_positions(
    db: &cb_state::Database,
    positions: &[Position],
    height: u64,
) -> Result<Vec<(DerivId, u128)>, ExecError> {
    let markets: std::collections::BTreeSet<_> = positions
        .iter()
        .filter(|p| p.qty > 0)
        .map(|p| p.market)
        .collect();
    markets
        .into_iter()
        .map(|m| fresh_mark_q8(db, &m, height).map(|px| (m, px)))
        .collect()
}

/// Each market's configured requirement is a floor underneath the tiered
/// schedule. Sum per-market maxima, not the maximum of two portfolio sums.
fn configured_margin_required(
    db: &cb_state::Database,
    positions: &[Position],
    marks: &[(DerivId, u128)],
    maintenance: bool,
) -> Result<u128, ExecError> {
    let mut total = 0u128;
    for p in positions.iter().filter(|p| p.qty > 0) {
        let (im, mm, leverage, _) = load_market_view(db, &p.market)?.risk();
        if leverage == 0 || im > 10_000 || mm > im {
            return Err(ExecError::InvalidRiskConfig);
        }
        let mark = marks
            .iter()
            .find(|(m, _)| *m == p.market)
            .ok_or(ExecError::MissingMarkPrice)?
            .1;
        let notional = cb_engine::position_notional_micro_usd(p.qty, mark);
        let floor = ceil_div(
            notional.saturating_mul(if maintenance { mm } else { im } as u128),
            10_000,
        );
        let tiers = if maintenance {
            cb_engine::maintenance_margin_required(std::slice::from_ref(p), marks, DEFAULT_TIERS)
        } else {
            cb_engine::initial_margin_required(std::slice::from_ref(p), marks, DEFAULT_TIERS)
        };
        let requirement = if maintenance {
            floor.max(tiers)
        } else {
            floor
                .max(tiers)
                .max(ceil_div(notional.saturating_mul(100), leverage as u128))
        };
        total = total.saturating_add(requirement);
    }
    Ok(total)
}

fn check_position_caps(
    db: &cb_state::Database,
    positions: &[Position],
    marks: &[(DerivId, u128)],
) -> Result<(), ExecError> {
    for p in positions.iter().filter(|p| p.qty > 0) {
        let (_, _, _, cap) = load_market_view(db, &p.market)?.risk();
        let mark = marks
            .iter()
            .find(|(m, _)| *m == p.market)
            .ok_or(ExecError::MissingMarkPrice)?
            .1;
        if cb_engine::position_notional_micro_usd(p.qty, mark) > cap {
            return Err(ExecError::PositionLimit);
        }
    }
    Ok(())
}

/// The current mark on `market`, or `None` if it has never been marked (a
/// zero mark counts as none).
fn market_mark_q8(db: &cb_state::Database, market: &DerivId) -> Option<u128> {
    OracleState::new(db)
        .get_mark(market)
        .map(|m| m.px_q8)
        .filter(|px| *px > 0)
}

/// How much of `owner`'s open position on `market` an order on `side` would
/// close: the position's quantity when it is on the opposite side, and zero
/// when the owner is flat or already on `side`.
///
/// Shared by the matcher's snapshot and by `apply_place_perp_order`'s
/// admission check so the two cannot drift apart — the drift between them is
/// what let a one-unit long authorise an unbounded short.
fn reduce_only_capacity_of(
    positions: &Positions<'_>,
    owner: &Address,
    market: &DerivId,
    side: Side,
) -> u128 {
    match positions.get(owner, market) {
        Some(p) if p.qty > 0 && p.side != side => p.qty,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// FuturesSettle (M5)
// ---------------------------------------------------------------------------

/// Apply a `FuturesSettle` system tx for a dated futures market.
///
/// Authorization: sender MUST equal `ctx.proposer_addr` (same rule as
/// `FundingTick`). The block-level required-tx check
/// (`enforce_required_futures_settle`) makes sure exactly one of these
/// lands at every expiry boundary; here we re-validate that the market
/// really is at-or-past expiry and not already settled, so a leader who
/// emits the tx early (or twice) trips a clean error rather than silently
/// double-settling.
///
/// Steps:
///   1. Authority + market checks.
///   2. Compute settlement price via `compute_settlement_price`. Method 0
///      uses the mark-history TWAP; method 1 uses the latest oracle price.
///   3. Walk every open `Position` on the market in deterministic byte
///      order. For each, compute settlement PnL and credit / debit the
///      owner's `MarginAccount.collateral`. Negative residuals land in
///      `bad_debt`, which the insurance-fund / halt path can absorb later.
///      Delete the position row.
///   4. Cancel every resting order on the market (perp_orders +
///      perp_book_index). Order ids are sorted before mutation so trie
///      writes are deterministic.
///   5. Set `FutureMarket.final_settlement_px_q8 = Some(...)` and clear the
///      mark-history ring buffer.
///   6. Advance proposer nonce.
fn apply_futures_settle(
    state: &mut BlockState,
    ctx: &ExecCtx,
    sender: &Address,
    sender_state: &mut cb_state::AccountState,
    market: &DerivId,
) -> Result<(), ExecError> {
    if *sender != ctx.proposer_addr {
        return Err(ExecError::NotAuthorized);
    }
    let db = &state.db();
    let fms = FuturesMarkets::new(db);
    let fm = fms.get(market).ok_or(ExecError::UnknownMarket)?;

    if fm.final_settlement_px_q8.is_some() {
        return Err(ExecError::AlreadySettled);
    }
    if ctx.block_height < fm.expiry_height {
        return Err(ExecError::NotYetExpiry {
            market: *market,
            expiry: fm.expiry_height,
            current: ctx.block_height,
        });
    }

    // 2. Compute the settlement price. The mark-history buffer was populated
    //    on every `OracleUpdate` in the leadup; oracle price is read from
    //    `OracleState` so settlement_method=1 has a fallback if a TWAP buffer
    //    is empty (and vice versa).
    let store = OracleState::new(db);
    let mark_history = store.list_mark_history(market);
    let oracle_px = store.get_oracle(market).map(|o| o.px_q8);
    let settle_px_q8 = compute_settlement_price(&fm, oracle_px, &mark_history);
    // Defensive: if both inputs were absent (settle_px_q8 == 0) the chain
    // would write a zero settlement price, wiping every long's collateral.
    // The protocol design (§8.3) calls for delaying settlement in this case
    // — but the leader has already emitted FuturesSettle, so we instead
    // refuse to settle at zero. This surfaces as a hard error and is
    // recoverable by the next block (oracle posts → TWAP samples grow).
    if settle_px_q8 == 0 {
        return Err(ExecError::MissingMarkPrice);
    }

    // 3. Walk every position on this market in deterministic order and settle.
    let positions_cf = Positions::new(db);
    // `list_for_market` already sorts by `owner` byte order.
    let positions = positions_cf.list_for_market(market);
    // M8: collect owners we delete so we can register the position-trie
    // touches after the loop (state.touch_position requires `&mut state`,
    // which can't overlap with the `positions_cf` immutable handle's read
    // lifetime in the cleanest way).
    let mut touched_owners: Vec<Address> = Vec::with_capacity(positions.len());
    for pos in positions {
        // Skip flat (already-closed) positions defensively. They're rare
        // (M3's `apply_fill_to_position` produces them on exact closes) but
        // settling a zero-qty row would still touch the trie pointlessly.
        if pos.qty == 0 {
            positions_cf.delete(&pos.owner, market);
            touched_owners.push(pos.owner);
            continue;
        }

        // Same conversion the fill path uses, which keeps fill-time and
        // settlement-time PnL on the same scale.
        let pnl_micro = pnl_raw_to_micro(compute_settlement_pnl(&pos, settle_px_q8));

        // Remove the settled leg before checking insurance eligibility.
        // Other live legs keep the signed liability until full close-out.
        positions_cf.delete(&pos.owner, market);
        apply_collateral_delta(state, pos.owner, pnl_micro, Some(*market));
        touched_owners.push(pos.owner);
    }

    // 4. Cancel every resting order on this market.
    let perp_orders_cf = PerpOrders::new(db);
    let perp_book_cf = PerpBookIndex::new(db);
    // Already sorted by order_id.
    let resting = perp_orders_cf.list_for_market(market);
    for o in resting {
        perp_orders_cf.delete(&o.order_id);
        perp_book_cf.delete(&o.market, o.side, o.price_q8, o.seq);
    }

    // 5. Mark the market settled and clear the history buffer. Re-read the
    //    row first: bad-debt absorption during step 3 may have set
    //    `paused_reduce_only_only`, and writing back the copy taken at the
    //    top would silently undo it (the same hazard `MarketView::persist`
    //    guards against).
    let mut fm = fms.get(market).unwrap_or(fm);
    fm.final_settlement_px_q8 = Some(settle_px_q8);
    // Persist `last_trade_px_q8` as the settlement price. Not strictly
    // required (the market is closed), but lets the dashboard show "last
    // trade = settlement" without a special case. TODO(M6 follow-up):
    // surface the settlement price as a distinct RPC field rather than
    // overloading last_trade_px_q8.
    fm.last_trade_px_q8 = settle_px_q8;
    fm.last_trade_height = ctx.block_height;
    fms.put(&fm);
    store.clear_mark_history(market);

    // M8: register every position deletion AND the futures market mutation
    // with their respective tries. Doing this once at the end of the
    // function keeps the long borrow on `db`-derived handles (positions_cf,
    // fms, store) clean of `&mut state` calls.
    for owner in touched_owners {
        state.touch_position(owner, *market);
    }
    state.touch_market(*market);

    sender_state.nonce += 1;
    state.put(*sender, sender_state.clone());
    Ok(())
}

// Margin-account helper: we don't construct one in M3 directly (no AdjustMargin
// handler yet), but the helper function lives here so the M4 wiring has a
// single place to add the IM/MM gate against `MarginAccount::collateral`.
#[allow(dead_code)]
fn margin_account(db: &cb_state::Database, owner: Address) -> Option<MarginAccount> {
    MarginAccounts::new(db).get(&owner)
}

// ---------------------------------------------------------------------------
// PlaceOrder / CancelOrder / BatchCancel — spot CLOB (S3)
// ---------------------------------------------------------------------------
//
// Determinism contract for the three spot handlers:
//
// * **Admission** uses `MarketConfig` from `CF_MARKETS` only — no time, no
//   randomness. `qty == 0`, `qty % min_qty != 0`, `price == 0`,
//   `price % tick_size != 0` each return a distinct typed error BEFORE any
//   state mutation. Replicas reject the same set of inputs by construction.
//
// * **Balance lock** for buys covers the full notional plus the maximum
//   taker fee the order could incur (`fee_reserve = ceil(notional * fee_bps
//   / 10_000)`). Sells lock `qty` of base — fees on sells come out of the
//   received quote at fill time, so no pre-reserve is needed. This keeps
//   `balance >= locked + fee_reserve` invariant: a fill can never debit a
//   fee that exceeds available balance.
//
//   Deviation from the S3 plan: the plan suggested locking the bare
//   notional. That is unsafe when `available == notional` exactly — the
//   subsequent fee debit would underflow. Locking notional + fee reserve
//   for buys closes that gap with no behavioural change in the typical case.
//
// * **Hydrate** the in-memory `SpotBook` for *this market only* from
//   `CF_ORDERS`, **once per block**, via `BlockState::spot_book_mut`. It used
//   to happen before every `place`, which made an order O(resting depth): a
//   sampling profile of `cb-order-bench in-process` at depth 1000 put 88.6% of
//   total runtime in the rehydration and 6.4% in the matcher. The matcher keeps
//   the cached book in step with the mutations this handler persists, so a
//   per-block book and a per-order book hold the same rows; see
//   `BlockState::spot_book_mut` for the full determinism argument. The two
//   cancel handlers edit `CF_ORDERS` directly and therefore invalidate the
//   cached book.
//
// * **Order id** uses the same `derive_order_id` helper as the perp path.
//   Spot ids share the perp keyspace; collisions are impossible at v1
//   scale (the (sender, height, tx_index, intra_ctr) tuple is unique per
//   call site within a block, and the matching engines route ids to
//   disjoint CFs — `CF_ORDERS` for spot, `CF_PERP_ORDERS` for perp).
//
// * **Seq** packs `(block_height, tx_index, 0)` with the same scheme the
//   perp executor uses, so price-time tie-breaks are reproducible.
//
// * **Fee destination** matches the perp liquidation path: taker fees credit
//   `Address::INSURANCE` on the *spot* `Accounts` CF (not the
//   `MarginAccounts` CF — spot fees are paid in `quote` from the user's
//   spot balance and remain on the spot side).
//
// * **Rejection policy** mirrors the perp executor exactly: post-only-cross,
//   FOK-unfilled, and self-trade rejections do NOT advance the sender's
//   nonce. The locked balance is released before the error returns. See
//   `apply_place_perp_order` for the original convention.

/// The balance an order of `qty` at `price` must reserve, by side.
///
/// This is *the* definition of a spot order's lock, and the only one. An order
/// holds `spot_lock_required(side, its own price, its remaining qty, fee)` at
/// all times: that much at placement, that much less after each fill, and
/// exactly that much released on cancel. Every movement is the difference
/// between two evaluations of this function, which is what makes the releases
/// telescope to exactly the amount taken at placement no matter how many
/// pieces the order fills in.
///
/// QA findings F6 and F7 were both the absence of that rule. The lock was taken
/// once, over the whole order at its limit price, and released **per fill**, at
/// each fill's own price and quantity. Two things then go wrong and they go
/// wrong in opposite directions:
///
/// * The fee reserve rounds up, so the sum of the per-fill ceilings can exceed
///   the single ceiling taken at placement. A resting buy of 2 000 taken by
///   sells of 1 001 and 999 at `fee_bps = 10` released 1 003 + 1 000 = 2 003
///   against 2 002 locked, and `AccountState::unlock` is a `checked_sub`, so
///   the overshoot aborted the executor mid-block — a panic in consensus code,
///   reachable from ordinary trading inside a hundred blocks (F6).
/// * A taker that gets price improvement fills below its limit, so the
///   per-fill releases fall *short*, and the surplus stayed locked for the life
///   of the account — including after the remainder was cancelled (F7).
fn spot_lock_required(side: Side, price: u128, qty: u128, fee_bps: u16) -> u128 {
    match side {
        Side::Buy => spot_buy_lock_amount(price, qty, fee_bps),
        Side::Sell => qty,
    }
}

/// Compute the lock amount for a spot buy: notional + fee reserve.
///
/// `notional = price * qty / 1e8`. Fee reserve is computed against
/// `notional` (the maximum amount the order could trade at if it fully
/// fills against the book at its limit price), rounded up so a low-fee
/// market with a small order still reserves at least 1 unit of fee.
fn spot_buy_lock_amount(price: u128, qty: u128, fee_bps: u16) -> u128 {
    let notional = price.saturating_mul(qty) / 100_000_000u128;
    let fee_reserve = ceil_div(notional.saturating_mul(fee_bps as u128), 10_000u128);
    notional.saturating_add(fee_reserve)
}

/// Notional in quote units for a buy/sell pairing of (`price`, `qty`).
fn spot_notional(price: u128, qty: u128) -> u128 {
    price.saturating_mul(qty) / 100_000_000u128
}

/// Taker fee on a notional in quote units. Rounded down (favours taker;
/// matches the perp `position_notional_micro_usd` rounding direction).
fn spot_taker_fee(notional: u128, fee_bps: u16) -> u128 {
    notional.saturating_mul(fee_bps as u128) / 10_000u128
}

/// `ceil(a / b)`. Caller guarantees `b > 0`.
fn ceil_div(a: u128, b: u128) -> u128 {
    if b == 0 {
        return a;
    }
    a / b + if a % b == 0 { 0 } else { 1 }
}

/// Convert the engine's `RestingOrder` (emitted via `BookMutation::PutOrder`)
/// into a state-side `SpotOrder` for persistence. `created_height` is taken
/// from the apply context.
fn persisted_from_resting(
    o: &spot_book::RestingOrder,
    market: MarketId,
    created_height: u64,
) -> SpotOrder {
    SpotOrder {
        order_id: o.order_id,
        market,
        owner: o.owner,
        side: o.side,
        price: o.price,
        qty_remaining: o.qty_remaining,
        qty_total: o.qty,
        seq: o.seq,
        tif: o.tif,
        post_only: o.post_only,
        client_order_id: o.client_order_id,
        created_height,
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_place_spot_order(
    state: &mut BlockState,
    ctx: &ExecCtx,
    tx_index: u32,
    sender: Address,
    sender_state: &mut cb_state::AccountState,
    market: MarketId,
    side: Side,
    price: u128,
    qty: u128,
    tif: Tif,
    post_only: bool,
    client_order_id: H256,
) -> Result<(), ExecError> {
    // 1. Resolve market config. Missing rows ⇒ `MarketNotFound`. Read once
    //    per block per market: genesis writes this row and nothing in the
    //    executor changes it, and reading it per order was a RocksDB point
    //    read on every placement.
    let cfg = state
        .spot_market_config(market)
        .ok_or(ExecError::MarketNotFound)?;
    let db = &state.db();

    // 2. Admission. Each branch returns BEFORE mutating any state.
    if price == 0 {
        return Err(ExecError::ZeroPrice);
    }
    // A zero `min_qty` is checked before it is used as a divisor: a market
    // row with one would otherwise take the executor down on its first
    // order, and the executor is the layer that must reject rather than
    // abort. Mirrors the `tick_size` guard below.
    if qty == 0 || cfg.min_qty == 0 || qty % cfg.min_qty != 0 {
        return Err(ExecError::BadLot);
    }
    if cfg.tick_size == 0 || price % cfg.tick_size != 0 {
        return Err(ExecError::BadTick);
    }

    // 3. Balance lock. See module doc for the buy-side fee reserve rationale,
    //    and `spot_lock_required` for why the lock is defined as a function of
    //    the order's *remaining* quantity rather than reconciled per fill.
    let lock_asset = match side {
        Side::Buy => cfg.quote,
        Side::Sell => cfg.base,
    };
    let lock_amount = spot_lock_required(side, price, qty, cfg.fee_bps);
    if lock_amount == 0 {
        // Notional underflowed (price * qty < 1e8). At v1 scale this only
        // fires on adversarial dust trades that don't economically clear;
        // reject as `BadLot` rather than admit a no-op order.
        return Err(ExecError::BadLot);
    }
    if sender_state.available(lock_asset) < lock_amount {
        return Err(ExecError::Insufficient);
    }
    sender_state.lock(lock_asset, lock_amount)?;
    state.put(sender, sender_state.clone());

    // 4. Take this block's in-memory book for the market. `BlockState`
    //    hydrates it from `CF_ORDERS` on first use in the block and keeps it
    //    in step with the mutations the matcher emits, so an order no longer
    //    pays to rebuild the whole book before it can match. See
    //    `BlockState::spot_book_mut` for the determinism argument.
    let orders_cf = Orders::new(db);
    let book_index_cf = BookIndex::new(db);

    // 5. Build incoming + place. `seq` and `order_id` are derived the same
    //    way the perp path does — see `derive_order_id` / `pack_seq`.
    let order_id = derive_order_id(sender, ctx.block_height, tx_index, 0);
    let seq = pack_seq(ctx.block_height, tx_index, 0);
    let incoming = spot_book::IncomingOrder {
        order_id,
        owner: sender,
        side,
        price,
        qty,
        tif,
        post_only,
        client_order_id,
        seq,
    };
    // `MatchResult` is owned, so the mutable borrow of the cached book ends
    // here and the settlement below can go back to `state.load` / `state.put`.
    let result = state.spot_book_mut(market).place(incoming);

    // 6. Rejection branch. Unlock the full lock (no fills happened) and
    //    return without advancing nonce — same convention as the perp path.
    if let Some(reason) = result.rejected {
        sender_state.unlock(lock_asset, lock_amount)?;
        state.put(sender, sender_state.clone());
        return Err(match reason {
            spot_book::RejectReason::PostOnlyCrossed => ExecError::PostOnlyCrossed,
            spot_book::RejectReason::FokUnfilled => ExecError::FokUnfilled,
            spot_book::RejectReason::SelfTrade => ExecError::SelfTrade,
        });
    }
    // Past here the book has changed — a fill, a resting insert, or both —
    // and a failure below must drop it. A rejection above changed nothing.
    state.note_spot_book_mutated(market);

    // 7. Settle each fill against the spot Accounts CF.
    //
    //    Per fill we touch four accounts: taker, maker, optionally the
    //    insurance fund (taker fee), and the sender_state (kept in sync via
    //    state.load/put). Order of ops within one fill:
    //      a. unlock taker's matched portion of the lock
    //      b. credit/debit taker's base + quote
    //      c. credit/debit maker's base + quote AND unlock maker's matched
    //         portion of THEIR own lock (the resting maker locked at place
    //         time; the matched portion of that is now consumed)
    //      d. credit insurance fund with the taker fee
    //
    //    `taker_lock_held` tracks how much of the taker's own lock is still
    //    held; step 10 reconciles it against what the resting remainder
    //    actually needs.
    //
    //    Maker releases are computed as the *difference* between the lock its
    //    remaining quantity required before this fill and the lock it requires
    //    after — never as a fresh evaluation at the fill's own size, which is
    //    what made the releases fail to add up to the placement lock (F6/F7).
    //    That needs each maker's pre-fill `qty_remaining`, so read it now:
    //    step 9 is what writes the reduced quantity back to `CF_ORDERS`, and it
    //    has not run yet. Reading it up front rather than inside the loop also
    //    means a missing row is discovered before any balance has moved.
    //
    //    **Ordering dependency:** if step 9 is ever moved before step 7, this
    //    read starts returning post-fill quantities and F6 comes straight back.
    //
    //    The pre-fill quantity comes from the matcher, not from `CF_ORDERS`: a
    //    maker appears in at most one fill of a placement, and the matcher's
    //    mutations say what it holds afterwards (`UpdateRemaining`) or that it
    //    holds nothing (`DeleteOrder`). Before = after + this fill. That is the
    //    value the row held, because the book is the rows' in-memory mirror, and
    //    it costs no RocksDB read per fill.
    let mut maker_remaining_before: std::collections::HashMap<H256, u128> =
        std::collections::HashMap::with_capacity(result.fills.len());
    for m in &result.mutations {
        if let spot_book::BookMutation::UpdateRemaining {
            order_id,
            qty_remaining,
        } = m
        {
            maker_remaining_before.insert(*order_id, *qty_remaining);
        }
    }
    debug_assert!(
        {
            let mut seen = std::collections::HashSet::new();
            result.fills.iter().all(|f| seen.insert(f.maker_order_id))
        },
        "the matcher walks each resting maker once per placement"
    );
    for fill in &result.fills {
        let after = maker_remaining_before
            .get(&fill.maker_order_id)
            .copied()
            .unwrap_or(0);
        maker_remaining_before.insert(fill.maker_order_id, after + fill.qty);
    }

    let mut taker_lock_held: u128 = lock_amount;
    let mut total_taker_fee: u128 = 0;

    for fill in &result.fills {
        let fill_notional = spot_notional(fill.price, fill.qty);
        let fill_fee = spot_taker_fee(fill_notional, cfg.fee_bps);
        total_taker_fee = total_taker_fee.saturating_add(fill_fee);

        // Taker side settlement. Use `state.load`/`put` so the executor's
        // address-cache stays consistent — sender_state may be the same
        // address as the maker if STP is bypassed, but STP rejected those
        // before this loop runs so taker != maker here.
        let mut taker_state = state.load(&fill.taker);
        let mut maker_state = state.load(&fill.maker);

        // The maker's side is the opposite of the taker's, and the fill price
        // IS the maker's resting price (price-time priority), so the maker's
        // own lock curve can be evaluated from the fill alone.
        let maker_side = match side {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        };
        let before = maker_remaining_before
            .get(&fill.maker_order_id)
            .copied()
            .unwrap_or(fill.qty);
        let after = before.saturating_sub(fill.qty);
        let maker_lock_release =
            spot_lock_required(maker_side, fill.price, before, cfg.fee_bps).saturating_sub(
                spot_lock_required(maker_side, fill.price, after, cfg.fee_bps),
            );

        match side {
            Side::Buy => {
                // Taker pays quote (notional + fee), receives base. Release
                // exactly what it spends; step 10 reconciles whatever the
                // limit-price lock reserved over and above that.
                let taker_spend = fill_notional + fill_fee;
                taker_state.unlock(cfg.quote, taker_spend)?;
                taker_lock_held = taker_lock_held.saturating_sub(taker_spend);
                taker_state.sub(cfg.quote, taker_spend)?;
                taker_state.add(cfg.base, fill.qty)?;
                // Maker (Sell side) locked base; the release is the drop in
                // what its remaining quantity reserves, which for a sell is
                // just `fill.qty`.
                maker_state.unlock(cfg.base, maker_lock_release)?;
                maker_state.sub(cfg.base, fill.qty)?;
                maker_state.add(cfg.quote, fill_notional)?;
            }
            Side::Sell => {
                // Taker pays base (qty), receives quote (notional - fee).
                taker_state.unlock(cfg.base, fill.qty)?;
                taker_lock_held = taker_lock_held.saturating_sub(fill.qty);
                taker_state.sub(cfg.base, fill.qty)?;
                taker_state.add(cfg.quote, fill_notional - fill_fee)?;
                // Maker (Buy side) locked notional + fee reserve over its whole
                // order. Release the drop in that reserve — which includes the
                // fee-reserve rounding, and so never over-releases the way a
                // per-fill `ceil` did.
                maker_state.unlock(cfg.quote, maker_lock_release)?;
                maker_state.sub(cfg.quote, fill_notional)?;
                maker_state.add(cfg.base, fill.qty)?;
            }
        }

        state.put(fill.taker, taker_state);
        state.put(fill.maker, maker_state);
    }

    // Refresh `sender_state` from the cache (the fill loop may have written
    // taker_state for the same address as `sender` when the taker is the
    // sender — which is always true here, since the incoming order's owner
    // IS the sender). After this we own the canonical sender state again.
    *sender_state = state.load(&sender);

    // 8. Credit the insurance fund with the accumulated taker fee.
    if total_taker_fee > 0 {
        let mut ins = state.load(&Address::INSURANCE);
        ins.add(cfg.quote, total_taker_fee)?;
        state.put(Address::INSURANCE, ins);
    }

    // 9. Apply book mutations to RocksDB. Walked in matcher-emitted order so
    //    replica state mirrors the leader's exactly.
    for m in &result.mutations {
        match m {
            spot_book::BookMutation::PutOrder(o) => {
                let persisted = persisted_from_resting(o, market, ctx.block_height);
                orders_cf.put(&persisted);
                book_index_cf.put(market, o.side, o.price, o.seq, &o.order_id);
            }
            spot_book::BookMutation::UpdateRemaining {
                order_id,
                qty_remaining,
            } => {
                if let Some(mut existing) = orders_cf.get(order_id) {
                    existing.qty_remaining = *qty_remaining;
                    orders_cf.put(&existing);
                }
                // BookIndex keys use (price, seq) which don't change on a
                // partial fill, so no index mutation is needed here.
            }
            spot_book::BookMutation::DeleteOrder {
                order_id,
                side,
                price,
                seq,
            } => {
                // The matcher hands over the index key, so this is two
                // deletes and no read. A delete of a key that is not there is
                // a no-op in RocksDB, which is what the old `if let Some` on
                // the re-read amounted to.
                book_index_cf.delete(market, *side, *price, *seq);
                orders_cf.delete(order_id);
            }
        }
    }

    // 10. Reconcile the sender's lock against what the resting remainder
    //     actually needs.
    //
    //     The taker still holds `taker_lock_held`, which is the placement lock
    //     less what the fills spent. What it is *entitled* to hold is what its
    //     own remaining order reserves — `spot_lock_required` at its own limit
    //     price — or nothing at all if it did not rest. Releasing the
    //     difference in one step is what returns price improvement to the
    //     account instead of stranding it (F7): a buy that filled below its
    //     limit spent less than the limit-price lock reserved, and the gap
    //     belongs to the trader, not to the order book.
    //
    //     The subtraction cannot go negative. Fills happen at or better than
    //     the limit, so what they spend is at most what the same quantity
    //     reserved at the limit price; the `saturating_sub` is belt, not braces.
    let required_after = match &result.rest {
        Some(r) => spot_lock_required(side, price, r.qty_remaining, cfg.fee_bps),
        None => 0,
    };
    let release = taker_lock_held.saturating_sub(required_after);
    if release > 0 {
        sender_state.unlock(lock_asset, release)?;
    }

    // 11. Advance sender nonce.
    sender_state.nonce += 1;
    state.put(sender, sender_state.clone());

    Ok(())
}

fn apply_cancel_spot_order(
    state: &mut BlockState,
    sender: Address,
    sender_state: &mut cb_state::AccountState,
    order_id: H256,
) -> Result<(), ExecError> {
    let db = &state.db();
    let orders_cf = Orders::new(db);
    let book_index_cf = BookIndex::new(db);

    let order = orders_cf.get(&order_id).ok_or(ExecError::OrderNotFound)?;
    if order.owner != sender {
        return Err(ExecError::NotAuthorized);
    }

    // Lookup market for the asset/fee_bps so the unlock amount matches the
    // place-side lock formula. Treat a missing market row as a fatal-but-
    // recoverable error (the order can't have a market that never existed,
    // but defend anyway).
    let cfg = state
        .spot_market_config(order.market)
        .ok_or(ExecError::MarketNotFound)?;
    let unlock_amount =
        spot_lock_required(order.side, order.price, order.qty_remaining, cfg.fee_bps);
    let unlock_asset = match order.side {
        Side::Buy => cfg.quote,
        Side::Sell => cfg.base,
    };
    sender_state.unlock(unlock_asset, unlock_amount)?;

    book_index_cf.delete(order.market, order.side, order.price, order.seq);
    orders_cf.delete(&order_id);
    // This handler edits `CF_ORDERS` behind the matcher's back. Take the
    // order out of the block's cached book as well, so the book stays what
    // the rows are. It used to drop the whole book instead and let the next
    // placement rehydrate — one full rebuild per cancel, through the index,
    // the stage and every pending overlay, which under a stream of cancels
    // was 42% of the matching thread. If no book is cached there is nothing
    // to keep in step: the next hydration reads rows this delete is gone from.
    if let Some(book) = state.spot_book_cached_mut(order.market) {
        book.cancel(order_id);
        state.note_spot_book_mutated(order.market);
    }

    sender_state.nonce += 1;
    state.put(sender, sender_state.clone());
    Ok(())
}

fn apply_batch_cancel_spot(
    state: &mut BlockState,
    sender: Address,
    sender_state: &mut cb_state::AccountState,
    market: MarketId,
) -> Result<(), ExecError> {
    let db = &state.db();
    let orders_cf = Orders::new(db);
    let book_index_cf = BookIndex::new(db);

    // Resolve config once. A missing market row makes the tx a no-op (we
    // still advance nonce; the alternative — fail closed — would be a sharp
    // edge for clients that batch-cancel against a market that just got
    // delisted). Genesis only seeds two spot markets and there is no listing
    // path yet, so this is purely defensive.
    let cfg = match state.spot_market_config(market) {
        Some(c) => c,
        None => {
            sender_state.nonce += 1;
            state.put(sender, sender_state.clone());
            return Ok(());
        }
    };

    // The sender's resting orders come from the book, not from a scan of the
    // orders column family. That scan read every order on every market and
    // every key a fill had deleted — 46% of the matching thread under a
    // stream of batch cancels — and then the book was thrown away and rebuilt
    // for the next placement. The book holds exactly the persisted rows, and
    // `resting_orders_of` yields them in ascending order-id order, which is
    // the order the scan produced, so the unlocks run in the same sequence
    // and the resulting state is byte-for-byte the same. The difference is
    // O(resting) work on memory instead of O(history) work on RocksDB, and a
    // book that stays cached through the cancel.
    let mine = state.spot_book_mut(market).resting_orders_of(&sender);
    for order in mine {
        let unlock_amount =
            spot_lock_required(order.side, order.price, order.qty_remaining, cfg.fee_bps);
        let unlock_asset = match order.side {
            Side::Buy => cfg.quote,
            Side::Sell => cfg.base,
        };
        sender_state.unlock(unlock_asset, unlock_amount)?;
        book_index_cf.delete(market, order.side, order.price, order.seq);
        orders_cf.delete(&order.order_id);
        if let Some(book) = state.spot_book_cached_mut(market) {
            book.cancel(order.order_id);
            state.note_spot_book_mutated(market);
        }
    }

    sender_state.nonce += 1;
    state.put(sender, sender_state.clone());
    Ok(())
}

#[cfg(test)]
mod seq_tests {
    use super::*;

    #[test]
    fn pack_seq_is_monotonic_in_height() {
        let a = pack_seq(0, 0, 0);
        let b = pack_seq(1, 0, 0);
        assert!(b > a);
    }

    #[test]
    fn pack_seq_is_monotonic_in_tx_index() {
        let a = pack_seq(10, 0, 0);
        let b = pack_seq(10, 1, 0);
        assert!(b > a);
    }

    #[test]
    fn pack_seq_is_monotonic_in_intra_ctr() {
        let a = pack_seq(10, 5, 0);
        let b = pack_seq(10, 5, 1);
        assert!(b > a);
    }

    #[test]
    fn pack_seq_is_deterministic() {
        // Same inputs → same seq, every time. Foundation of replica equality.
        for h in [0u64, 1, 100, 1_000_000] {
            for t in [0u32, 1, 100, 65535] {
                for i in [0u8, 1, 255] {
                    assert_eq!(pack_seq(h, t, i), pack_seq(h, t, i));
                }
            }
        }
    }

    #[test]
    fn order_id_is_deterministic() {
        let s = Address([7; 20]);
        let id1 = derive_order_id(s, 100, 5, 0);
        let id2 = derive_order_id(s, 100, 5, 0);
        assert_eq!(id1, id2);
        // Different inputs produce different ids.
        assert_ne!(id1, derive_order_id(s, 101, 5, 0));
        assert_ne!(id1, derive_order_id(s, 100, 6, 0));
        assert_ne!(id1, derive_order_id(s, 100, 5, 1));
        assert_ne!(id1, derive_order_id(Address([8; 20]), 100, 5, 0));
    }

    #[test]
    fn the_premium_ema_saturates_on_an_absurd_midpoint() {
        // A book quoted at the top of the price range against an oracle of
        // one puts the raw premium at `i128::MAX`; with a negative previous
        // EMA the plain subtraction overflowed. It must clamp, not wrap or
        // panic, and the answer is the largest premium the field holds.
        let next = update_premium_ema(-5, Some(u128::MAX / 2), 1);
        assert_eq!(next, i64::MAX);
        // And from the top of the range, a book below the oracle brings it
        // back down rather than sticking.
        let next = update_premium_ema(i64::MAX, Some(0), 1);
        assert!(
            next < i64::MAX,
            "a midpoint below the oracle pulls the EMA down"
        );
        // The ordinary case is unchanged: one eighth of the step per sample.
        assert_eq!(update_premium_ema(0, Some(1_010), 1_000), 10_000_000 / 8);
    }
}
