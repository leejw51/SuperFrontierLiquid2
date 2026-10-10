//! The in-process world: markets, accounts, bots, and the block loop.
//!
//! This is the hermetic run mode. There is no node and no network — the
//! world drives `cb-block`'s executor against a temporary RocksDB, which
//! makes a run fast, reproducible and suitable for CI.
//!
//! ## Why transactions are signed one at a time
//!
//! A rejected transaction does **not** advance the sender's nonce. If the
//! world signed a block's worth of transactions up front, the noise bot's
//! first deliberately invalid order would desynchronise every later
//! transaction from that sender and the run would collapse into `BadNonce`.
//! So each transaction is signed against the nonce read out of the live
//! `BlockState` immediately before it is applied. Signing is the expensive
//! part, but a few hundred signatures a block is nothing next to the value of
//! never having to reason about nonce skew.
//!
//! ## Funding-boundary blocks are quiet
//!
//! At a funding-interval boundary the world submits only oracle updates and
//! the funding ticks themselves. Nothing else moves collateral in that block,
//! so the collateral delta across it *is* the funding flow — which is what
//! makes invariant 7 measurable instead of merely plausible.
//!
//! ## The premium shim
//!
//! `cb-block` never computes a premium: `apply_oracle_update` carries
//! `MarkPrice::premium_ema_q9` forward unchanged and calls
//! `compute_mark_price` with `mid = None`, so the funding rate is always
//! zero and every funding tick moves nothing. Testing funding against that
//! would be vacuous, so the world writes the premium the chain *should* have
//! derived — book midpoint against oracle, exponentially smoothed — directly
//! into `OracleState` after the oracle updates land. The shim is a simulation
//! input, not a fix: it is deterministic, it is applied identically on every
//! run of a seed, and the underlying defect is recorded in
//! `docs/qa-report.md`.

use std::collections::BTreeMap;
use std::panic::{catch_unwind, AssertUnwindSafe};

use anyhow::Result;
use cb_block::{apply_tx_at, BlockState, ExecCtx};
use cb_crypto::{address_from_pubkey, keypair_from_seed, SecretKey};
use cb_engine::{is_liquidatable, DEFAULT_TIERS};
use cb_state::{
    Accounts, BookIndex, Database, FuturesMarkets, MarginAccounts, MarketConfig, Markets, Meta,
    OracleState, Orders, PerpBookIndex, PerpMarkets, PerpOrders,
};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{
    Address, AssetId, DerivId, FutureMarket, MarginAccount, MarkPrice, PerpMarket, Side, Signed,
    H256,
};
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use tempfile::TempDir;

use crate::bots::{
    arbitrageur::ArbitrageurParams, liquidator::LiquidatorParams, market_maker::MarketMakerParams,
    momentum::MomentumParams, noise::NoiseParams, whale::WhaleParams, Arbitrageur, Bot, BotKind,
    Liquidator, MarketMaker, MarketObs, Momentum, Noise, Observation, SelfObs, SpotObs, Whale,
};
use crate::invariants::{BlockContext, CheckerConfig, InvariantChecker, TxRecord, Violation};
use crate::ledger::LedgerSnapshot;
use crate::report::{BotOutcome, RunReport};
use crate::scenario::{PriceEngine, ScenarioTimeline};
use crate::universe::{FUT_BTC, MICRO, PERP_BTC, PERP_ETH, Q8, SIM_MARKETS, SPOT_BTC};

/// Spot prices are quote-base-units per base-base-unit, scaled `1e8`.
///
/// The derivative side quotes USD per BTC scaled `1e8`; the spot executor's
/// notional formula is `price * qty / 1e8` with the result in the quote
/// asset's own base units (micro-USD). BTC has eight decimals and USD six, so
/// the spot price is the derivative price divided by `1e2`. Getting this
/// wrong inflates every spot notional a hundredfold, so it lives in one named
/// function.
pub fn spot_price_from_index(index_q8: u128) -> u128 {
    (index_q8 / 100).max(1)
}

/// How many bots of each strategy to run.
#[derive(Clone, Copy, Debug)]
pub struct BotCounts {
    /// Number of market makers.
    pub market_makers: usize,
    /// Number of momentum traders.
    pub momentum: usize,
    /// Number of noise traders.
    pub noise: usize,
    /// Number of whales.
    pub whales: usize,
    /// Number of liquidators.
    pub liquidators: usize,
    /// Number of arbitrageurs.
    pub arbitrageurs: usize,
}

impl Default for BotCounts {
    fn default() -> Self {
        Self {
            market_makers: 3,
            momentum: 6,
            noise: 4,
            whales: 2,
            liquidators: 2,
            arbitrageurs: 2,
        }
    }
}

impl BotCounts {
    /// Total population.
    pub fn total(&self) -> usize {
        self.market_makers
            + self.momentum
            + self.noise
            + self.whales
            + self.liquidators
            + self.arbitrageurs
    }
}

/// Everything a run needs to be reproducible.
#[derive(Clone, Debug)]
pub struct WorldConfig {
    /// Master seed. The same seed must produce an identical run.
    pub seed: u64,
    /// Population.
    pub bots: BotCounts,
    /// Scenario schedule.
    pub timeline: ScenarioTimeline,
    /// How many blocks to run.
    pub blocks: u64,
    /// Funding cadence.
    pub funding_interval_blocks: u32,
    /// Height at which the dated future expires. `0` disables the future.
    pub future_expiry_height: u64,
    /// Starting margin collateral per bot, micro-USD.
    pub starting_collateral: u128,
    /// Starting spot USD balance per bot.
    pub starting_spot_usd: u128,
    /// Starting spot BTC balance per bot, in satoshi-scale base units.
    pub starting_spot_btc: u128,
    /// Insurance-fund seed, micro-USD.
    pub insurance_seed: u128,
    /// Write the book-derived funding premium the chain fails to compute.
    ///
    /// `apply_oracle_update` carries `MarkPrice::premium_ema_q9` forward
    /// unchanged and calls `compute_mark_price` with `mid = None`, so the
    /// premium is zero forever and every funding tick moves nothing. With
    /// this off, the simulation reproduces shipped behaviour exactly. With it
    /// on, funding actually flows — which is the only way to exercise the
    /// funding path, and which surfaces the entry-funding-index defect
    /// recorded in `docs/qa-report.md`.
    pub inject_premium: bool,
    /// List a spot market and let the bots trade it.
    ///
    /// Spot has two confirmed lock-accounting defects (see
    /// `docs/qa-report.md`): a partially-filled resting buy over-unlocks by
    /// the fee-reserve rounding residue, which panics
    /// `AccountState::unlock`, and a buy that fills at a better price than
    /// its limit leaves the improvement locked forever. Both are pinned by
    /// their own regression tests. The long derivative runs switch spot off
    /// so those known defects do not drown out anything new — not to make a
    /// test pass.
    pub enable_spot: bool,
    /// Spot taker fee. The fee-reserve rounding defect needs a non-zero fee.
    pub spot_fee_bps: u16,
    /// Chain id used for signing.
    pub chain_id: u32,
    /// Checker configuration.
    pub checker: CheckerConfig,
}

impl Default for WorldConfig {
    fn default() -> Self {
        Self {
            seed: 0xC0FFEE,
            bots: BotCounts::default(),
            timeline: ScenarioTimeline::parse("calm").expect("valid"),
            blocks: 500,
            funding_interval_blocks: 25,
            future_expiry_height: 0,
            starting_collateral: 50_000 * MICRO,
            starting_spot_usd: 200_000 * MICRO,
            starting_spot_btc: 4 * 100_000_000,
            insurance_seed: 2_000_000 * MICRO,
            // Off by default. Spot has two confirmed lock-accounting
            // defects, one of which panics the executor within a few hundred
            // blocks, so leaving it on would end every run before the
            // derivative invariants had been exercised. `--spot` turns it
            // back on to reproduce them.
            inject_premium: true,
            enable_spot: false,
            spot_fee_bps: 10,
            chain_id: 1,
            checker: CheckerConfig::default(),
        }
    }
}

struct BotSlot {
    bot: Box<dyn Bot>,
    sk: SecretKey,
    addr: Address,
    label: String,
    kind: BotKind,
}

/// A running simulated exchange.
pub struct World {
    config: WorldConfig,
    _dir: TempDir,
    db: Database,
    prices: PriceEngine,
    bots: Vec<BotSlot>,
    labels: BTreeMap<Address, String>,
    oracle_sk: SecretKey,
    oracle_addr: Address,
    sequencer_sk: SecretKey,
    sequencer_addr: Address,
    checker: InvariantChecker,
    report: RunReport,
    /// Exponentially smoothed premium per market, q9. See the module docs.
    premium_ema_q9: Vec<(DerivId, i64)>,
    start_height: u64,
    height: u64,
}

impl World {
    /// Build a world: markets, accounts, bots, genesis balances.
    pub fn new(config: WorldConfig) -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let db = Database::open(dir.path())?;

        let (oracle_sk, oracle_pk) = keypair_from_seed(b"cb-simgame-oracle");
        let oracle_addr = address_from_pubkey(&oracle_pk);
        let (sequencer_sk, sequencer_pk) = keypair_from_seed(b"cb-simgame-sequencer");
        let sequencer_addr = address_from_pubkey(&sequencer_pk);

        let mut labels = BTreeMap::new();
        labels.insert(oracle_addr, "oracle".to_string());
        labels.insert(sequencer_addr, "sequencer".to_string());
        labels.insert(Address::INSURANCE, "insurance".to_string());

        seed_markets(&db, oracle_addr, &config);
        Meta::new(&db).set_proposer_addr(sequencer_addr);
        Meta::new(&db).set_chain_id(config.chain_id);

        // The sequencer needs a margin account so `FundingTick` and
        // `FuturesSettle` senders exist in the same shape as everyone else.
        put_margin(&db, sequencer_addr, 0);
        put_margin(&db, oracle_addr, 0);
        put_margin(&db, Address::INSURANCE, config.insurance_seed);

        let bots = build_population(&db, &config, &mut labels);

        let mut report = RunReport::new(config.seed, config.timeline.to_string());
        for slot in &bots {
            report.bots.push(BotOutcome::new(
                slot.label.clone(),
                slot.kind,
                slot.addr,
                starting_collateral_for(slot.kind, config.starting_collateral),
                config.starting_spot_usd,
                config.starting_spot_btc,
            ));
        }
        report.insurance_start = config.insurance_seed;
        report.spot_enabled = config.enable_spot;

        let prices = PriceEngine::new(config.seed, config.timeline.clone());
        let mut checker = InvariantChecker::new(config.checker);
        checker.prime(&db);

        let premium_ema_q9 = SIM_MARKETS.iter().map(|m| (m.id, 0i64)).collect();

        Ok(Self {
            config,
            _dir: dir,
            db,
            prices,
            bots,
            labels,
            oracle_sk,
            oracle_addr,
            sequencer_sk,
            sequencer_addr,
            checker,
            report,
            premium_ema_q9,
            start_height: 1,
            height: 0,
        })
    }

    /// Borrow the underlying database, for tests that want to assert on raw
    /// state after a run.
    pub fn db(&self) -> &Database {
        &self.db
    }

    /// The invariant checker, for tests that want the violation list.
    pub fn checker(&self) -> &InvariantChecker {
        &self.checker
    }

    /// Run every block in the configured schedule and return the report.
    ///
    /// Returns `Err` only on a hard failure — a panic inside the executor, or
    /// the first violation when `fail_fast` is set. Violations found without
    /// `fail_fast` are collected in the report.
    pub fn run(&mut self) -> Result<RunReport> {
        for i in 0..self.config.blocks {
            let height = self.start_height + i;
            self.height = height;
            match self.run_block(height, i) {
                Ok(()) => {}
                Err(RunStop::Violation(v)) => {
                    self.report.violations.push(*v);
                    break;
                }
                Err(RunStop::Fatal(e)) => {
                    self.finish();
                    self.report.fatal = Some(e.to_string());
                    return Ok(self.report.clone());
                }
            }
        }
        self.finish();
        Ok(self.report.clone())
    }

    fn finish(&mut self) {
        self.report.blocks_run = self.height.saturating_sub(self.start_height - 1);
        self.report.blocks_checked = self.checker.blocks_checked();
        for v in self.checker.violations() {
            if !self
                .report
                .violations
                .iter()
                .any(|e| e.height == v.height && e.invariant == v.invariant && e.detail == v.detail)
            {
                self.report.violations.push(v.clone());
            }
        }
        self.report.exercised = self.checker.exercised().iter().copied().collect();
        self.report.peak_below_maintenance = self.checker.peak_below_maintenance();
        let snap = LedgerSnapshot::read(&self.db);
        let marks = crate::invariants::all_marks(&self.db);
        self.report.finalize(&self.db, &snap, &marks);
    }

    fn run_block(&mut self, height: u64, offset: u64) -> std::result::Result<(), RunStop> {
        self.prices.step(offset);
        let scenario = self.prices.scenario_at(offset);
        let funding_boundary = height % self.config.funding_interval_blocks.max(1) as u64 == 0;
        let settle_now = self.config.future_expiry_height > 0
            && height == self.config.future_expiry_height
            && FuturesMarkets::new(&self.db)
                .get(&FUT_BTC)
                .map(|m| m.final_settlement_px_q8.is_none())
                .unwrap_or(false);

        let db = &self.db;
        let chain_id = self.config.chain_id;
        let oracle_sk = self.oracle_sk;
        let oracle_addr = self.oracle_addr;
        let sequencer_sk = self.sequencer_sk;
        let sequencer_addr = self.sequencer_addr;
        let seed = self.config.seed;
        let future_expiry = self.config.future_expiry_height;
        let mut state = BlockState::new(db);
        let ctx = ExecCtx {
            treasury_signer: self.sequencer_addr,
            block_height: height,
            proposer_addr: self.sequencer_addr,
        };
        let mut records: Vec<TxRecord> = Vec::new();
        let mut tx_index: u32 = 0;

        // 1. Oracle updates, one per derivative market, in fixed order.
        for sm in SIM_MARKETS.iter() {
            if sm.id == FUT_BTC && future_expiry == 0 {
                continue;
            }
            if settled(db, sm.id) {
                continue;
            }
            let px = self.prices.price_q8(sm.underlying);
            let body = TxBody::OracleUpdate {
                market: sm.id,
                px_q8: px,
                submitted_height: height,
            };
            apply_one(
                &mut state,
                &ctx,
                chain_id,
                &mut tx_index,
                &mut records,
                oracle_sk,
                oracle_addr,
                "oracle",
                body,
            )?;
        }

        // 2. Premium shim: write the premium the chain would have derived if
        //    it consulted the book. See the module docs.
        if self.config.inject_premium {
            refresh_premium(db, &mut self.premium_ema_q9, height);
        }

        // 3. Bot actions, unless this is a quiet funding-boundary block.
        if !funding_boundary && !settle_now {
            let observation_base = build_observation(db, &self.prices, height, offset);
            self.report
                .note_liquidatable_at_block_start(observation_base.liquidatable.len());
            let addrs: Vec<Address> = self.bots.iter().map(|b| b.addr).collect();
            let mut selves = all_self_obs(db, &addrs);
            for idx in 0..self.bots.len() {
                let mut rng = ChaCha8Rng::seed_from_u64(
                    seed.wrapping_mul(0x2545_F491_4F6C_DD1D)
                        .wrapping_add(height.wrapping_mul(1_000_003))
                        .wrapping_add(idx as u64),
                );
                let mut obs = observation_base.clone();
                obs.me = selves.remove(&self.bots[idx].addr).unwrap_or_default();
                let bodies = self.bots[idx].bot.act(&obs, &mut rng);
                let (sk, addr, label) = (
                    self.bots[idx].sk,
                    self.bots[idx].addr,
                    self.bots[idx].label.clone(),
                );
                for body in bodies {
                    apply_one(
                        &mut state,
                        &ctx,
                        chain_id,
                        &mut tx_index,
                        &mut records,
                        sk,
                        addr,
                        &label,
                        body,
                    )?;
                }
            }
        }

        // 4. Funding ticks at the boundary, signed by the proposer.
        let mut funding_markets: Vec<DerivId> = Vec::new();
        if funding_boundary {
            for pm in PerpMarkets::new(db).list() {
                let body = TxBody::FundingTick { market: pm.id };
                apply_one(
                    &mut state,
                    &ctx,
                    chain_id,
                    &mut tx_index,
                    &mut records,
                    sequencer_sk,
                    sequencer_addr,
                    "sequencer",
                    body,
                )?;
                funding_markets.push(pm.id);
            }
        }

        // 5. Futures settlement at expiry, signed by the proposer.
        let mut settled_markets: Vec<DerivId> = Vec::new();
        if settle_now {
            let body = TxBody::FuturesSettle { market: FUT_BTC };
            apply_one(
                &mut state,
                &ctx,
                chain_id,
                &mut tx_index,
                &mut records,
                sequencer_sk,
                sequencer_addr,
                "sequencer",
                body,
            )?;
            settled_markets.push(FUT_BTC);
        }

        let roots = match state.commit() {
            Ok(r) => r,
            Err(e) => return Err(RunStop::Fatal(anyhow::anyhow!("commit failed: {e}"))),
        };
        let state_root = roots.state_root;
        Meta::new(db).set_head(height, H256::ZERO, state_root);

        let block_ctx = BlockContext {
            height,
            scenario,
            txs: &records,
            state_root,
            funding_markets: &funding_markets,
            settled_markets: &settled_markets,
            labels: &self.labels,
        };
        let snap = match self.checker.check_block(&self.db, &block_ctx) {
            Ok(s) => s,
            Err(v) => return Err(RunStop::Violation(v)),
        };
        self.report.observe_block(
            height,
            scenario,
            state_root,
            &records,
            &snap,
            !settled_markets.is_empty(),
            &funding_markets,
            &self.db,
        );
        Ok(())
    }

    /// Unused placeholder removed; see the free function `apply_one`.
    #[allow(dead_code)]
    fn _apply_one_placeholder(&self) {}
}

/// Sign `body` against the sender's live nonce, apply it, and record the
/// outcome. See the module docs for why signing happens here rather than in
/// the bot.
#[allow(clippy::too_many_arguments)]
fn apply_one(
    state: &mut BlockState<'_>,
    ctx: &ExecCtx,
    chain_id: u32,
    tx_index: &mut u32,
    records: &mut Vec<TxRecord>,
    sk: SecretKey,
    sender: Address,
    label: &str,
    body: TxBody,
) -> std::result::Result<(), RunStop> {
    let nonce = state.load(&sender).nonce;
    let rendered = render_tx(&body);
    let target = match &body {
        TxBody::LiquidationTrigger { owner, .. } => Some(*owner),
        _ => None,
    };
    let verified = match sign(&sk, nonce, chain_id, body) {
        Ok(v) => v,
        Err(e) => return Err(RunStop::Fatal(e)),
    };
    let index = *tx_index;
    *tx_index += 1;
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        apply_tx_at(state, ctx, &verified, index)
    }));
    let record = match outcome {
        Ok(Ok(())) => TxRecord {
            index,
            sender,
            sender_label: label.to_string(),
            body: rendered,
            outcome: Ok(()),
            target,
        },
        Ok(Err(e)) => TxRecord {
            index,
            sender,
            sender_label: label.to_string(),
            body: rendered,
            outcome: Err(e.to_string()),
            target,
        },
        Err(panic) => {
            let msg = panic_message(&panic);
            records.push(TxRecord {
                index,
                sender,
                sender_label: label.to_string(),
                body: rendered.clone(),
                outcome: Err(format!("PANIC: {msg}")),
                target,
            });
            return Err(RunStop::Fatal(anyhow::anyhow!(
                "executor panicked at height {} applying {} from {label} ({sender}): {msg}",
                ctx.block_height,
                rendered
            )));
        }
    };
    records.push(record);
    Ok(())
}

/// Write the premium the chain would have derived from the book, so the
/// funding path is exercised rather than trivially zero.
fn refresh_premium(db: &Database, premium_ema_q9: &mut [(DerivId, i64)], height: u64) {
    let store = OracleState::new(db);
    let book = PerpBookIndex::new(db);
    for slot in premium_ema_q9.iter_mut() {
        let (market, ema) = (slot.0, slot.1);
        let mark = match store.get_mark(&market) {
            Some(m) => m,
            None => continue,
        };
        let oracle = match store.get_oracle(&market) {
            Some(o) => o.px_q8,
            None => continue,
        };
        if oracle == 0 {
            continue;
        }
        let bid = book
            .list_side(&market, Side::Buy)
            .iter()
            .map(|(p, _, _)| *p)
            .max();
        let ask = book
            .list_side(&market, Side::Sell)
            .iter()
            .map(|(p, _, _)| *p)
            .min();
        let sample_q9 = match (bid, ask) {
            (Some(b), Some(a)) => {
                let mid = (b + a) / 2;
                // (mid - oracle) / oracle, at q9.
                let diff = mid as i128 - oracle as i128;
                (diff.saturating_mul(1_000_000_000) / oracle as i128) as i64
            }
            // No two-sided book means no observable premium.
            _ => 0,
        };
        // Smooth with a 1/8 weight on the new sample, the shape the
        // protocol design specifies for the premium EMA.
        let next = ema + (sample_q9 - ema) / 8;
        slot.1 = next;
        store.put_mark(&MarkPrice {
            market,
            px_q8: mark.px_q8,
            computed_at_height: height,
            premium_ema_q9: next,
        });
    }
}

fn build_observation(db: &Database, prices: &PriceEngine, height: u64, offset: u64) -> Observation {
    let store = OracleState::new(db);
    let book = PerpBookIndex::new(db);
    let mut markets = Vec::new();
    for sm in SIM_MARKETS.iter() {
        let (tick, restricted, settled_flag) = match sm.id.kind() {
            Ok(cb_types::MarketKind::Perp) => match PerpMarkets::new(db).get(&sm.id) {
                Some(m) => (m.tick_size, m.paused || m.paused_reduce_only_only, false),
                None => continue,
            },
            Ok(cb_types::MarketKind::Future) => match FuturesMarkets::new(db).get(&sm.id) {
                Some(m) => (m.tick_size, m.paused, m.final_settlement_px_q8.is_some()),
                None => continue,
            },
            Err(_) => continue,
        };
        markets.push(MarketObs {
            id: sm.id,
            label: sm.label,
            mark_q8: store.get_mark(&sm.id).map(|m| m.px_q8).unwrap_or(0),
            oracle_q8: store.get_oracle(&sm.id).map(|o| o.px_q8).unwrap_or(0),
            best_bid: book
                .list_side(&sm.id, Side::Buy)
                .iter()
                .map(|(p, _, _)| *p)
                .max(),
            best_ask: book
                .list_side(&sm.id, Side::Sell)
                .iter()
                .map(|(p, _, _)| *p)
                .min(),
            tick,
            restricted,
            settled: settled_flag,
        });
    }

    let spot_cfg = Markets::new(db).get(SPOT_BTC);
    let spot_book = BookIndex::new(db);
    let spot = SpotObs {
        enabled: spot_cfg.is_some(),
        market: SPOT_BTC,
        best_bid: spot_book
            .list_side(SPOT_BTC, Side::Buy)
            .iter()
            .map(|(p, _, _)| *p)
            .max(),
        best_ask: spot_book
            .list_side(SPOT_BTC, Side::Sell)
            .iter()
            .map(|(p, _, _)| *p)
            .min(),
        reference: spot_price_from_index(prices.price_q8(crate::universe::Underlying::Btc)),
        min_qty: spot_cfg.as_ref().map(|c| c.min_qty).unwrap_or(1),
        tick: spot_cfg.as_ref().map(|c| c.tick_size).unwrap_or(1),
    };

    Observation {
        height,
        offset,
        markets,
        spot,
        me: SelfObs::default(),
        liquidatable: liquidatable_accounts(db),
    }
}

/// Build every bot's self-view in one pass over the column families.
///
/// The obvious shape — ask each bot "what are my resting orders?" — costs a
/// full order-book scan per bot per market, which at twenty bots and three
/// markets is sixty scans a block and dominates the whole run. One pass,
/// bucketed by owner, is the same information for a twentieth of the work.
fn all_self_obs(db: &Database, addrs: &[Address]) -> BTreeMap<Address, SelfObs> {
    let mut out: BTreeMap<Address, SelfObs> =
        addrs.iter().map(|a| (*a, SelfObs::default())).collect();

    for m in crate::ledger::scan_margin_accounts(db) {
        if let Some(slot) = out.get_mut(&m.owner) {
            slot.collateral = m.collateral;
            slot.bad_debt = m.bad_debt;
        }
    }
    for p in crate::ledger::scan_positions(db) {
        if p.qty == 0 {
            continue;
        }
        if let Some(slot) = out.get_mut(&p.owner) {
            slot.positions
                .insert(p.market, (p.side, p.qty, p.entry_px_q8));
        }
    }
    for sm in SIM_MARKETS.iter() {
        for o in PerpOrders::new(db).list_for_market(&sm.id) {
            if let Some(slot) = out.get_mut(&o.owner) {
                slot.resting
                    .push((o.order_id, o.market, o.side, o.price_q8, o.remaining_qty));
            }
        }
    }
    if Markets::new(db).get(SPOT_BTC).is_some() {
        for o in Orders::new(db).list_for_market(SPOT_BTC) {
            if let Some(slot) = out.get_mut(&o.owner) {
                slot.spot_resting
                    .push((o.order_id, o.side, o.price, o.qty_remaining));
            }
        }
        for (addr, state) in crate::ledger::scan_accounts(db) {
            if let Some(slot) = out.get_mut(&addr) {
                slot.spot_base_free = state.available(AssetId::Btc);
                slot.spot_quote_free = state.available(AssetId::Usd);
            }
        }
    }
    out
}

/// Accounts the chain would currently consider liquidatable, computed once
/// per block with the same helper the executor uses.
fn liquidatable_accounts(db: &Database) -> Vec<(Address, DerivId)> {
    let marks = crate::invariants::all_marks(db);
    let margins: BTreeMap<Address, MarginAccount> = crate::ledger::scan_margin_accounts(db)
        .into_iter()
        .map(|m| (m.owner, m))
        .collect();
    let mut out = Vec::new();
    for (owner, positions) in crate::invariants::positions_by_owner(db) {
        if owner == Address::INSURANCE {
            continue;
        }
        let margin = margins
            .get(&owner)
            .cloned()
            .unwrap_or_else(|| MarginAccount::new(owner));
        if !is_liquidatable(&margin, &positions, &marks, DEFAULT_TIERS) {
            continue;
        }
        // Trigger against the largest position first: it is the one
        // carrying the maintenance requirement.
        let mut by_size = positions.clone();
        by_size.sort_by(|a, b| b.qty.cmp(&a.qty).then(a.market.0.cmp(&b.market.0)));
        if let Some(p) = by_size.first() {
            out.push((owner, p.market));
        }
    }
    out
}

/// Why a run stopped early. Boxed because a `Violation` carries a whole
/// block's transaction log and two state digests; a bare enum would make
/// every `Result` on the hot path that wide.
enum RunStop {
    Violation(Box<Violation>),
    Fatal(anyhow::Error),
}

fn settled(db: &Database, market: DerivId) -> bool {
    matches!(market.kind(), Ok(cb_types::MarketKind::Future))
        && FuturesMarkets::new(db)
            .get(&market)
            .map(|m| m.final_settlement_px_q8.is_some())
            .unwrap_or(false)
}

fn sign(sk: &SecretKey, nonce: u64, chain_id: u32, body: TxBody) -> Result<VerifiedTx> {
    let signed = SignedTx::sign_with(
        sk,
        Transaction {
            nonce,
            chain_id,
            body,
        },
    )?;
    Ok(signed.verify()?)
}

fn panic_message(p: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// One-line rendering of a transaction body for violation reports.
pub fn render_tx(body: &TxBody) -> String {
    match body {
        TxBody::PlacePerpOrder {
            market,
            side,
            price_q8,
            qty,
            tif,
            post_only,
            reduce_only,
            ..
        } => format!(
            "PlacePerpOrder {market} {side:?} px={price_q8} qty={qty} {tif:?}{}{}",
            if *post_only { " post_only" } else { "" },
            if *reduce_only { " reduce_only" } else { "" }
        ),
        TxBody::CancelPerpOrder { order_id } => {
            format!("CancelPerpOrder {}", hex::encode(&order_id.0[..8]))
        }
        TxBody::PlaceOrder {
            market,
            side,
            price,
            qty,
            tif,
            post_only,
            ..
        } => format!(
            "PlaceOrder(spot) {market:?} {side:?} px={price} qty={qty} {tif:?}{}",
            if *post_only { " post_only" } else { "" }
        ),
        TxBody::CancelOrder { order_id } => {
            format!("CancelOrder(spot) {}", hex::encode(&order_id.0[..8]))
        }
        TxBody::BatchCancel { market } => format!("BatchCancel(spot) {market:?}"),
        TxBody::OracleUpdate {
            market,
            px_q8,
            submitted_height,
        } => format!("OracleUpdate {market} px={px_q8} at={submitted_height}"),
        TxBody::FundingTick { market } => format!("FundingTick {market}"),
        TxBody::FuturesSettle { market } => format!("FuturesSettle {market}"),
        TxBody::LiquidationTrigger { owner, market } => {
            format!("LiquidationTrigger owner={owner} {market}")
        }
        other => format!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Genesis
// ---------------------------------------------------------------------------

fn seed_markets(db: &Database, oracle_authority: Address, config: &WorldConfig) {
    let perps = PerpMarkets::new(db);
    for (id, underlying) in [(PERP_BTC, AssetId::Btc), (PERP_ETH, AssetId::Eth)] {
        perps.put(&PerpMarket {
            id,
            underlying,
            tick_size: 1_000_000, // a cent, at q8
            lot_size: 1,
            max_leverage_x100: 5_000,
            initial_margin_bps: 200,
            maintenance_margin_bps: 100,
            taker_fee_bps: 3,
            maker_fee_bps_signed: -1,
            funding_interval_blocks: config.funding_interval_blocks,
            funding_rate_cap_q9: 5_000_000,
            oracle_authority,
            max_position_notional: u128::MAX,
            paused: false,
            last_trade_px_q8: 0,
            last_trade_height: 0,
            paused_reduce_only_only: false,
        });
    }

    if config.future_expiry_height > 0 {
        FuturesMarkets::new(db).put(&FutureMarket {
            id: FUT_BTC,
            underlying: AssetId::Btc,
            tick_size: 1_000_000,
            lot_size: 1,
            max_leverage_x100: 2_500,
            initial_margin_bps: 400,
            maintenance_margin_bps: 200,
            taker_fee_bps: 3,
            maker_fee_bps_signed: -1,
            expiry_height: config.future_expiry_height,
            // Oracle settlement: the TWAP buffer is only populated by
            // `OracleUpdate`, and settling against the price the scenario
            // engine actually produced makes the settlement assertion exact.
            settlement_method: 1,
            settlement_window_blocks: 64,
            oracle_authority,
            paused: false,
            final_settlement_px_q8: None,
            last_trade_px_q8: 0,
            last_trade_height: 0,
            paused_reduce_only_only: false,
        });
    }

    if config.enable_spot {
        Markets::new(db).put(&MarketConfig {
            id: SPOT_BTC,
            base: AssetId::Btc,
            quote: AssetId::Usd,
            // A price tick of 1e5 against a minimum lot of 1e3 makes
            // `price * qty` a whole multiple of 1e8, so the spot notional is
            // exact and any lock drift the checker reports comes from the
            // executor's own arithmetic rather than from the lot geometry.
            tick_size: 100_000,
            min_qty: 1_000,
            fee_bps: config.spot_fee_bps,
        });
    }
}

fn put_margin(db: &Database, owner: Address, collateral: u128) {
    MarginAccounts::new(db).put(&MarginAccount {
        owner,
        collateral,
        unrealized_pnl: Signed::ZERO,
        bad_debt: 0,
    });
}

/// Instantiate the bot population without touching any state.
///
/// Shared by the in-process world and [`crate::net`] so a networked session
/// runs exactly the same strategies with exactly the same parameters — the
/// labels are what derive each bot's key, so a bot keeps its address across
/// both modes.
pub fn describe_population(counts: &BotCounts) -> Vec<(String, Box<dyn Bot>)> {
    make_population(counts)
        .into_iter()
        .map(|(label, _, bot)| (label, bot))
        .collect()
}

fn make_population(counts: &BotCounts) -> Vec<(String, BotKind, Box<dyn Bot>)> {
    let mut out: Vec<(String, BotKind, Box<dyn Bot>)> = Vec::new();
    let mut tag: u8 = 1;

    // Market makers first: they must be in the book before anyone takes.
    for i in 0..counts.market_makers {
        let label = format!("market_maker#{i}");
        // Stagger spreads so the makers do not all quote the identical price
        // and pile onto one price level.
        let base = MarketMakerParams::default();
        let params = MarketMakerParams {
            base_half_spread_bps: base.base_half_spread_bps + i as i64 * 3,
            ..base
        };
        out.push((
            label.clone(),
            BotKind::MarketMaker,
            Box::new(MarketMaker::new(label, tag, params)),
        ));
        tag = tag.wrapping_add(1);
    }
    for i in 0..counts.momentum {
        let label = format!("momentum#{i}");
        let params = MomentumParams {
            // Staggered so the cohort does not move as one block.
            leverage_x100: 800 + (i as u128 % 4) * 700,
            trigger_bps: 15 + (i as u128 % 3) * 10,
            ..MomentumParams::default()
        };
        out.push((
            label.clone(),
            BotKind::Momentum,
            Box::new(Momentum::new(label, tag, params)),
        ));
        tag = tag.wrapping_add(1);
    }
    for i in 0..counts.noise {
        let label = format!("noise#{i}");
        out.push((
            label.clone(),
            BotKind::Noise,
            Box::new(Noise::new(label, tag, NoiseParams::default())),
        ));
        tag = tag.wrapping_add(1);
    }
    for i in 0..counts.whales {
        let label = format!("whale#{i}");
        out.push((
            label.clone(),
            BotKind::Whale,
            Box::new(Whale::new(
                label,
                tag,
                (i as u64) * 7,
                WhaleParams::default(),
            )),
        ));
        tag = tag.wrapping_add(1);
    }
    for i in 0..counts.liquidators {
        let label = format!("liquidator#{i}");
        out.push((
            label.clone(),
            BotKind::Liquidator,
            Box::new(Liquidator::new(label, i, LiquidatorParams::default())),
        ));
        tag = tag.wrapping_add(1);
    }
    for i in 0..counts.arbitrageurs {
        let label = format!("arbitrageur#{i}");
        out.push((
            label.clone(),
            BotKind::Arbitrageur,
            Box::new(Arbitrageur::new(label, tag, ArbitrageurParams::default())),
        ));
        tag = tag.wrapping_add(1);
    }
    out
}

/// Starting collateral for a strategy, as a multiple of the configured base.
///
/// The sizes are not decoration: they decide whether a crash actually
/// cascades. A market maker quoting 0.4 of a base unit per side caps how
/// large anyone else's position can get, so a momentum trader funded like a
/// market maker ends up at a fraction of 1x leverage and survives anything.
/// Funding the momentum cohort thinly is what makes the same fill sizes add
/// up to real leverage — and real liquidations.
pub fn starting_collateral_for(kind: BotKind, base: u128) -> u128 {
    match kind {
        // Deep enough to keep quoting through a crash; an insolvent market
        // maker means an empty book exactly when liquidations need one.
        BotKind::MarketMaker => base * 10,
        // Deliberately thin: this is the cohort that gets liquidated.
        BotKind::Momentum => base / 25,
        BotKind::Noise => base / 5,
        BotKind::Whale => base * 4,
        // A keeper needs no inventory, only the bounty.
        BotKind::Liquidator => base / 10,
        BotKind::Arbitrageur => base * 2,
    }
}

fn build_population(
    db: &Database,
    config: &WorldConfig,
    labels: &mut BTreeMap<Address, String>,
) -> Vec<BotSlot> {
    let mut slots: Vec<BotSlot> = Vec::new();
    for (label, kind, bot) in make_population(&config.bots) {
        let (sk, pk) = keypair_from_seed(format!("cb-simgame-{label}").as_bytes());
        let addr = address_from_pubkey(&pk);
        put_margin(
            db,
            addr,
            starting_collateral_for(kind, config.starting_collateral),
        );
        let accounts = Accounts::new(db);
        let mut st = accounts.get(&addr).unwrap_or_default();
        st.add(AssetId::Usd, config.starting_spot_usd)
            .expect("seed balance");
        st.add(AssetId::Btc, config.starting_spot_btc)
            .expect("seed balance");
        accounts.put(&addr, &st);
        labels.insert(addr, label.clone());
        slots.push(BotSlot {
            bot,
            sk,
            addr,
            label,
            kind,
        });
    }
    slots
}

/// Starting index price of BTC, exposed so tests can assert against it.
pub const START_BTC_Q8: u128 = 50_000 * Q8;
