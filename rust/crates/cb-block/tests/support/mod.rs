//! Shared fixture for the property-style executor tests (`spot_conservation`,
//! `perp_conservation`, `liquidation_paths`, `margin_gate_worst_case`,
//! `close_position`, `lot_tick_boundaries`, `arrival_order`,
//! `nonce_and_atomicity`).
//!
//! cb-block has no proptest dependency, and `block_atomicity.rs` explains why
//! that is fine: a seeded xorshift generator is reproducible, costs nothing,
//! and every failure message here carries the seed it came from. Each file
//! that uses this module states its own properties; this module only builds
//! chains and reads them back.
//!
//! The block runner, [`Chain::block`], applies transactions the way the
//! block builder does — a savepoint before each, a rollback if it fails, a
//! commit point after — so a rejected transaction leaves nothing behind, and
//! then commits. It signs each transaction *at apply time* with the sender's
//! live nonce, so a workload generator can describe intent ("alice sells 3")
//! without predicting which of its earlier transactions will be refused.

#![allow(dead_code)]

use cb_block::{apply_tx_at, BlockState, ExecCtx, ExecError};
use cb_crypto::{address_from_pubkey, keypair_from_seed, SecretKey};
use cb_state::{
    Accounts, Database, FundingIndices, FuturesMarkets, MarginAccounts, MarketConfig, Markets,
    Meta, OracleState, PerpMarkets, Positions, CF_MARGIN_ACCOUNTS, CF_POSITIONS,
};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{
    Address, AssetId, DerivId, FutureMarket, MarginAccount, MarkPrice, MarketId, PerpMarket,
    Position, Side, Signed, Tif, H256,
};

pub const Q8: u128 = 100_000_000;
pub const MICRO: u128 = 1_000_000;
pub const ONE_BTC: u128 = 100_000_000;
pub const CHAIN_ID: u32 = 1;
pub const PERP: DerivId = DerivId::perp(0);
pub const SPOT: MarketId = MarketId::BtcUsd;

// ---------------------------------------------------------------------------
// Randomness
// ---------------------------------------------------------------------------

/// Xorshift64. Never seeded with zero (`new` ors in a bit).
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    /// Uniform in `lo..=hi`.
    pub fn range(&mut self, lo: u128, hi: u128) -> u128 {
        lo + (self.next() as u128) % (hi - lo + 1)
    }
    pub fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
    pub fn side(&mut self) -> Side {
        if self.next() & 1 == 0 {
            Side::Buy
        } else {
            Side::Sell
        }
    }
    pub fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            v.swap(i, j);
        }
    }
}

// ---------------------------------------------------------------------------
// Actors and signing
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Actor {
    pub sk: SecretKey,
    pub addr: Address,
}

pub fn actor(seed: &str) -> Actor {
    let (sk, pk) = keypair_from_seed(seed.as_bytes());
    Actor {
        sk,
        addr: address_from_pubkey(&pk),
    }
}

pub fn sign(sk: &SecretKey, nonce: u64, body: TxBody) -> VerifiedTx {
    sign_for(sk, nonce, CHAIN_ID, body)
}

pub fn sign_for(sk: &SecretKey, nonce: u64, chain_id: u32, body: TxBody) -> VerifiedTx {
    SignedTx::sign_with(
        sk,
        Transaction {
            nonce,
            chain_id,
            body,
        },
    )
    .unwrap()
    .verify()
    .unwrap()
}

pub fn ctx_at(height: u64, proposer: Address) -> ExecCtx {
    ExecCtx {
        treasury_signer: Address::ZERO,
        block_height: height,
        proposer_addr: proposer,
    }
}

pub fn opposite(s: Side) -> Side {
    match s {
        Side::Buy => Side::Sell,
        Side::Sell => Side::Buy,
    }
}

pub fn signed_qty(p: &Position) -> i128 {
    match p.side {
        Side::Buy => p.qty as i128,
        Side::Sell => -(p.qty as i128),
    }
}

// ---------------------------------------------------------------------------
// Transaction bodies
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn perp_order(
    market: DerivId,
    side: Side,
    price_q8: u128,
    qty: u128,
    tif: Tif,
    post_only: bool,
    reduce_only: bool,
) -> TxBody {
    TxBody::PlacePerpOrder {
        market,
        side,
        price_q8,
        qty,
        tif,
        post_only,
        reduce_only,
        client_order_id: H256::default(),
    }
}

pub fn spot_order(side: Side, price: u128, qty: u128, tif: Tif, post_only: bool) -> TxBody {
    TxBody::PlaceOrder {
        market: SPOT,
        side,
        price,
        qty,
        tif,
        post_only,
        client_order_id: H256::default(),
    }
}

pub fn tick(market: DerivId) -> TxBody {
    TxBody::FundingTick { market }
}

pub fn oracle(market: DerivId, px_q8: u128, height: u64) -> TxBody {
    TxBody::OracleUpdate {
        market,
        px_q8,
        submitted_height: height,
    }
}

// ---------------------------------------------------------------------------
// Market records
// ---------------------------------------------------------------------------

pub fn perp_market(id: DerivId, authority: Address) -> PerpMarket {
    PerpMarket {
        id,
        underlying: AssetId::Btc,
        tick_size: 1,
        lot_size: 1,
        max_leverage_x100: 5_000,
        initial_margin_bps: 200,
        maintenance_margin_bps: 100,
        taker_fee_bps: 5,
        maker_fee_bps_signed: -1,
        funding_interval_blocks: 4,
        funding_rate_cap_q9: 5_000_000,
        oracle_authority: authority,
        max_position_notional: u128::MAX,
        paused: false,
        last_trade_px_q8: 0,
        last_trade_height: 0,
        paused_reduce_only_only: false,
    }
}

pub fn future_market(id: DerivId, authority: Address, expiry_height: u64) -> FutureMarket {
    FutureMarket {
        id,
        underlying: AssetId::Btc,
        tick_size: 1,
        lot_size: 1,
        max_leverage_x100: 5_000,
        initial_margin_bps: 200,
        maintenance_margin_bps: 100,
        taker_fee_bps: 5,
        maker_fee_bps_signed: 0,
        expiry_height,
        settlement_method: 1,
        settlement_window_blocks: 10,
        oracle_authority: authority,
        paused: false,
        final_settlement_px_q8: None,
        last_trade_px_q8: 0,
        last_trade_height: 0,
        paused_reduce_only_only: false,
    }
}

pub fn spot_market(tick_size: u128, min_qty: u128, fee_bps: u16) -> MarketConfig {
    MarketConfig {
        id: SPOT,
        base: AssetId::Btc,
        quote: AssetId::Usd,
        tick_size,
        min_qty,
        fee_bps,
    }
}

// ---------------------------------------------------------------------------
// The chain
// ---------------------------------------------------------------------------

pub struct Chain {
    _dir: tempfile::TempDir,
    pub db: Database,
    pub height: u64,
    /// The state root the last [`Chain::block`] committed.
    pub last_root: H256,
    pub proposer: Actor,
    pub oracle: Actor,
}

impl Chain {
    /// A fresh chain with id `CHAIN_ID`, a proposer and an oracle authority,
    /// and nothing listed.
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        Meta::new(&db).set_chain_id(CHAIN_ID);
        let proposer = actor("support-proposer");
        let oracle = actor("support-oracle");
        Meta::new(&db).set_proposer_addr(proposer.addr);
        Chain {
            _dir: dir,
            db,
            height: 0,
            last_root: H256::default(),
            proposer,
            oracle,
        }
    }

    pub fn list_perp(&self, m: PerpMarket) {
        PerpMarkets::new(&self.db).put(&m);
    }
    pub fn list_future(&self, m: FutureMarket) {
        FuturesMarkets::new(&self.db).put(&m);
    }
    pub fn list_spot(&self, m: MarketConfig) {
        Markets::new(&self.db).put(&m);
    }

    pub fn fund_margin(&self, owner: Address, collateral: u128) {
        MarginAccounts::new(&self.db).put(&MarginAccount {
            owner,
            collateral,
            unrealized_pnl: Signed::ZERO,
            bad_debt: 0,
        });
    }

    pub fn fund_spot(&self, owner: Address, asset: AssetId, amount: u128) {
        let a = Accounts::new(&self.db);
        let mut s = a.get(&owner).unwrap_or_default();
        s.add(asset, amount).unwrap();
        a.put(&owner, &s);
    }

    /// Write a mark directly, bypassing the oracle transaction and its
    /// median-of-three. For tests that need an exact mark.
    pub fn set_mark(&self, market: DerivId, px_q8: u128) {
        OracleState::new(&self.db).put_mark(&MarkPrice {
            market,
            px_q8,
            computed_at_height: self.height,
            premium_ema_q9: 0,
        });
    }

    pub fn nonce_of(&self, a: Address) -> u64 {
        Accounts::new(&self.db)
            .get(&a)
            .map(|s| s.nonce)
            .unwrap_or(0)
    }

    /// Apply one block of `(signer, body)` intents at the next height. Each
    /// is signed with the signer's live nonce at the moment it applies.
    pub fn block<A: std::borrow::Borrow<Actor>>(
        &mut self,
        txs: &[(A, TxBody)],
    ) -> Vec<Result<(), ExecError>> {
        self.block_with(txs, |_, _, _| {})
    }

    /// [`Chain::block`], calling `obs(db, i, None)` before transaction `i`
    /// and `obs(db, i, Some(result))` after it (after the rollback, if it
    /// failed). `db` is the block's staged view, so reads through it see
    /// every write the block has made so far.
    pub fn block_with<A, F>(
        &mut self,
        txs: &[(A, TxBody)],
        mut obs: F,
    ) -> Vec<Result<(), ExecError>>
    where
        A: std::borrow::Borrow<Actor>,
        F: FnMut(&Database, usize, Option<&Result<(), ExecError>>),
    {
        self.height += 1;
        let ctx = ctx_at(self.height, self.proposer.addr);
        let mut state = BlockState::new(&self.db);
        let mut out = Vec::with_capacity(txs.len());
        let mut kept = 0u32;
        for (i, (who, body)) in txs.iter().enumerate() {
            let who: &Actor = who.borrow();
            let nonce = state.load(&who.addr).nonce;
            let v = sign(&who.sk, nonce, body.clone());
            obs(&state.db(), i, None);
            let sp = state.savepoint();
            let r = apply_tx_at(&mut state, &ctx, &v, kept);
            match r {
                Ok(()) => kept += 1,
                Err(_) => state.rollback(sp),
            }
            state.commit_point();
            obs(&state.db(), i, Some(&r));
            out.push(r);
        }
        self.last_root = state.commit().unwrap().state_root;
        out
    }

    /// Apply already-signed transactions as one block, same rules.
    pub fn block_signed(&mut self, txs: &[VerifiedTx]) -> Vec<Result<(), ExecError>> {
        self.height += 1;
        let ctx = ctx_at(self.height, self.proposer.addr);
        let mut state = BlockState::new(&self.db);
        let mut out = Vec::with_capacity(txs.len());
        let mut kept = 0u32;
        for v in txs {
            let sp = state.savepoint();
            let r = apply_tx_at(&mut state, &ctx, v, kept);
            match r {
                Ok(()) => kept += 1,
                Err(_) => state.rollback(sp),
            }
            state.commit_point();
            out.push(r);
        }
        self.last_root = state.commit().unwrap().state_root;
        out
    }

    // ---- reads ------------------------------------------------------------

    pub fn margin(&self, owner: Address) -> MarginAccount {
        MarginAccounts::new(&self.db)
            .get(&owner)
            .unwrap_or_else(|| MarginAccount::new(owner))
    }

    /// `collateral - bad_debt`: the account's net cash, of which bad debt is
    /// the negative half.
    pub fn net(&self, owner: Address) -> i128 {
        let m = self.margin(owner);
        m.collateral as i128 - m.bad_debt as i128
    }

    pub fn position(&self, owner: Address, market: DerivId) -> Option<Position> {
        Positions::new(&self.db)
            .get(&owner, &market)
            .filter(|p| p.qty > 0)
    }

    /// Every margin account row on the chain.
    pub fn all_margins(&self) -> Vec<MarginAccount> {
        margins_in(&self.db)
    }

    /// Every open (non-zero) position row on the chain.
    pub fn all_positions(&self) -> Vec<Position> {
        positions_in(&self.db)
    }

    /// Σ (collateral − bad_debt) over every margin account.
    pub fn total_net(&self) -> i128 {
        margins_in(&self.db)
            .iter()
            .map(|m| m.collateral as i128 - m.bad_debt as i128)
            .sum()
    }

    pub fn mark(&self, market: DerivId) -> Option<u128> {
        OracleState::new(&self.db)
            .get_mark(&market)
            .map(|m| m.px_q8)
    }

    pub fn funding_cumulative(&self, market: DerivId) -> i128 {
        FundingIndices::new(&self.db)
            .get(&market)
            .map(|i| i.cumulative.to_i128())
            .unwrap_or(0)
    }

    pub fn perp(&self, market: DerivId) -> PerpMarket {
        PerpMarkets::new(&self.db).get(&market).unwrap()
    }
}

// ---------------------------------------------------------------------------
// Reads through any handle (the committed DB or a block's staged view)
// ---------------------------------------------------------------------------

pub fn margins_in(db: &Database) -> Vec<MarginAccount> {
    let empty: [u8; 0] = [];
    db.iter_prefix(CF_MARGIN_ACCOUNTS, &empty)
        .unwrap()
        .flatten()
        .map(|(_, v)| bincode::deserialize::<MarginAccount>(&v).unwrap())
        .collect()
}

pub fn positions_in(db: &Database) -> Vec<Position> {
    let empty: [u8; 0] = [];
    db.iter_prefix(CF_POSITIONS, &empty)
        .unwrap()
        .flatten()
        .map(|(_, v)| bincode::deserialize::<Position>(&v).unwrap())
        .filter(|p| p.qty > 0)
        .collect()
}

pub fn margin_in(db: &Database, owner: Address) -> MarginAccount {
    MarginAccounts::new(db)
        .get(&owner)
        .unwrap_or_else(|| MarginAccount::new(owner))
}

pub fn net_in(db: &Database, owner: Address) -> i128 {
    let m = margin_in(db, owner);
    m.collateral as i128 - m.bad_debt as i128
}

pub fn position_in(db: &Database, owner: Address, market: DerivId) -> Option<Position> {
    Positions::new(db)
        .get(&owner, &market)
        .filter(|p| p.qty > 0)
}

pub fn mark_in(db: &Database, market: DerivId) -> Option<u128> {
    OracleState::new(db).get_mark(&market).map(|m| m.px_q8)
}

/// Every key/value in every column family except the block store, sorted.
/// Two chains with equal snapshots are equal in every way the state can be
/// observed.
pub fn snapshot(db: &Database) -> Vec<(String, Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    for cf in cb_state::ALL_CFS {
        if *cf == cb_state::CF_BLOCKS || *cf == cb_state::CF_BLOCK_HASH {
            continue;
        }
        let empty: [u8; 0] = [];
        let Ok(it) = db.iter_prefix(cf, &empty) else {
            continue;
        };
        for (k, v) in it.flatten() {
            out.push((cf.to_string(), k.to_vec(), v.to_vec()));
        }
    }
    out.sort();
    out
}
