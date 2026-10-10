//! The run report: what happened, what held, and what broke.
//!
//! Numbers here are derived from state, not from counters the simulation
//! trusts itself to keep. Traded volume, for instance, is computed from the
//! change in every account's *signed* position size: a fill of `q` moves the
//! taker by `±q` and the maker by `∓q`, so half the sum of absolute changes
//! is the quantity that actually matched. That cannot drift from reality the
//! way an incremented counter can.

use std::collections::BTreeMap;

use cb_state::{Database, FuturesMarkets, PerpMarkets};
use cb_types::{Address, AssetId, DerivId, H256};

use crate::bots::BotKind;
use crate::invariants::{scan_liquidation_queue, Invariant, TxRecord, Violation};
use crate::ledger::LedgerSnapshot;
use crate::scenario::Scenario;
use crate::universe::{MICRO, NOTIONAL_DEN};

/// The parts of the ledger that the state root does not cover.
///
/// Every field here lives in `CF_MARGIN_ACCOUNTS` or is derived from it, and
/// `compute_state_root` hashes accounts, positions and markets only — so two
/// chains can agree on every root and disagree on all of this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LedgerDigest {
    /// The conserved derivative quantity.
    pub deriv_value: i128,
    /// Total collateral, micro-USD.
    pub collateral_total: u128,
    /// Total unpaid losses, micro-USD.
    pub bad_debt_total: u128,
    /// Insurance-fund collateral, micro-USD.
    pub insurance_collateral: u128,
    /// Number of open positions.
    pub open_positions: usize,
}

/// Per-bot outcome.
#[derive(Clone, Debug)]
pub struct BotOutcome {
    /// Unique label.
    pub label: String,
    /// Strategy.
    pub kind: BotKind,
    /// On-chain address.
    pub address: Address,
    /// Collateral at genesis, micro-USD.
    pub start_collateral: u128,
    /// Spot USD at genesis.
    pub start_spot_usd: u128,
    /// Spot BTC at genesis.
    pub start_spot_btc: u128,
    /// Collateral at the end, micro-USD.
    pub end_collateral: u128,
    /// Unpaid losses at the end.
    pub end_bad_debt: u128,
    /// Mark-to-market equity at the end, micro-USD.
    pub end_equity: i128,
    /// Spot USD at the end.
    pub end_spot_usd: u128,
    /// Spot BTC at the end.
    pub end_spot_btc: u128,
    /// Derivative profit and loss, micro-USD.
    pub deriv_pnl: i128,
    /// Spot profit and loss valued at the final index, micro-USD.
    pub spot_pnl: i128,
    /// Transactions submitted.
    pub txs_submitted: u64,
    /// Transactions the executor rejected.
    pub txs_rejected: u64,
    /// Times this bot was the owner named in a liquidation trigger.
    pub liquidated: u64,
}

impl BotOutcome {
    /// A fresh outcome for a bot that has not traded yet.
    pub fn new(
        label: String,
        kind: BotKind,
        address: Address,
        start_collateral: u128,
        start_spot_usd: u128,
        start_spot_btc: u128,
    ) -> Self {
        Self {
            label,
            kind,
            address,
            start_collateral,
            start_spot_usd,
            start_spot_btc,
            end_collateral: start_collateral,
            end_bad_debt: 0,
            end_equity: start_collateral as i128,
            end_spot_usd: start_spot_usd,
            end_spot_btc: start_spot_btc,
            deriv_pnl: 0,
            spot_pnl: 0,
            txs_submitted: 0,
            txs_rejected: 0,
            liquidated: 0,
        }
    }

    /// Total profit and loss, micro-USD.
    pub fn total_pnl(&self) -> i128 {
        self.deriv_pnl + self.spot_pnl
    }
}

/// The result of a run.
#[derive(Clone, Debug)]
pub struct RunReport {
    /// Seed the run used.
    pub seed: u64,
    /// Scenario timeline, as written on the command line.
    pub timeline: String,
    /// Blocks produced.
    pub blocks_run: u64,
    /// Block boundaries at which the invariants were evaluated.
    pub blocks_checked: u64,
    /// Every violation found.
    pub violations: Vec<Violation>,
    /// Invariants evaluated at least once.
    pub exercised: Vec<Invariant>,
    /// Per-bot outcomes.
    pub bots: Vec<BotOutcome>,
    /// State root after every block, for the determinism comparison.
    pub state_roots: Vec<(u64, H256)>,
    /// Ledger digest after every block: `(deriv_value, collateral, bad_debt,
    /// insurance)`.
    ///
    /// The state root is **not** sufficient on its own to prove two runs are
    /// identical, because margin accounts are outside it (finding F3 in
    /// `docs/qa-report.md`). Two runs could disagree about every account's
    /// collateral from block 50 onward and compare equal on roots alone. This
    /// digest closes that gap for the determinism test — and is a second
    /// demonstration of the finding.
    pub ledger_digests: Vec<(u64, LedgerDigest)>,
    /// Transactions submitted.
    pub txs_submitted: u64,
    /// Transactions accepted.
    pub txs_accepted: u64,
    /// Transactions rejected, by executor error text.
    pub rejections: BTreeMap<String, u64>,
    /// Matched base-unit volume per derivative market.
    pub volume_base: BTreeMap<DerivId, u128>,
    /// Matched notional per derivative market, micro-USD.
    pub volume_notional: BTreeMap<DerivId, u128>,
    /// Liquidation-queue rows at the end of the run.
    pub liquidations: u64,
    /// Total collateral moved out of accounts by funding ticks, micro-USD.
    pub funding_paid: u128,
    /// Insurance-fund collateral at genesis.
    pub insurance_start: u128,
    /// Insurance-fund collateral at the end.
    pub insurance_end: u128,
    /// Lowest insurance-fund collateral seen.
    pub insurance_min: u128,
    /// Total unpaid losses at the end.
    pub bad_debt_end: u128,
    /// Markets that ended in reduce-only halt.
    pub halted_markets: Vec<DerivId>,
    /// Scenario regimes the run passed through, with their block spans.
    pub regimes: Vec<(String, u64, u64)>,
    /// Whether a spot market was listed.
    pub spot_enabled: bool,
    /// The most accounts below maintenance margin the checker saw at a single
    /// block *boundary*.
    ///
    /// This under-counts a cascade on purpose-built runs: an account that
    /// goes under and is liquidated inside the same block is never observed
    /// at a boundary at all. [`RunReport::peak_liquidatable_at_block_start`]
    /// and [`RunReport::peak_liquidations_in_block`] are the honest cascade
    /// measures.
    pub peak_below_maintenance: usize,
    /// The most accounts the chain considered liquidatable at the start of
    /// any single block, before the keepers acted.
    pub peak_liquidatable_at_block_start: usize,
    /// The most liquidations successfully triggered within a single block.
    pub peak_liquidations_in_block: usize,
    /// Dated futures that settled during the run, with their settlement price.
    pub settled_futures: Vec<(DerivId, u128)>,
    /// Open interest per derivative market at the end, as `(long, short)`.
    pub open_interest_end: BTreeMap<DerivId, (u128, u128)>,
    /// Largest open interest seen on each market, in base units.
    pub peak_open_interest: BTreeMap<DerivId, u128>,
    /// A fatal error that stopped the run, if any.
    pub fatal: Option<String>,
    /// Internal: previous snapshot, for per-block deltas.
    prev: Option<LedgerSnapshot>,
}

impl RunReport {
    /// An empty report.
    pub fn new(seed: u64, timeline: String) -> Self {
        Self {
            seed,
            timeline,
            blocks_run: 0,
            blocks_checked: 0,
            violations: Vec::new(),
            exercised: Vec::new(),
            bots: Vec::new(),
            state_roots: Vec::new(),
            ledger_digests: Vec::new(),
            txs_submitted: 0,
            txs_accepted: 0,
            rejections: BTreeMap::new(),
            volume_base: BTreeMap::new(),
            volume_notional: BTreeMap::new(),
            liquidations: 0,
            funding_paid: 0,
            insurance_start: 0,
            insurance_end: 0,
            insurance_min: u128::MAX,
            bad_debt_end: 0,
            halted_markets: Vec::new(),
            regimes: Vec::new(),
            spot_enabled: false,
            peak_below_maintenance: 0,
            peak_liquidatable_at_block_start: 0,
            peak_liquidations_in_block: 0,
            settled_futures: Vec::new(),
            open_interest_end: BTreeMap::new(),
            peak_open_interest: BTreeMap::new(),
            fatal: None,
            prev: None,
        }
    }

    /// True when every invariant held.
    pub fn all_invariants_held(&self) -> bool {
        self.violations.is_empty() && self.fatal.is_none()
    }

    /// Violations of one invariant.
    pub fn violations_of(&self, inv: Invariant) -> Vec<&Violation> {
        self.violations
            .iter()
            .filter(|v| v.invariant == inv)
            .collect()
    }

    /// Record everything observable about a block that just committed.
    #[allow(clippy::too_many_arguments)]
    pub fn observe_block(
        &mut self,
        height: u64,
        scenario: Scenario,
        state_root: H256,
        txs: &[TxRecord],
        snap: &LedgerSnapshot,
        settled_block: bool,
        funding_markets: &[DerivId],
        db: &Database,
    ) {
        self.state_roots.push((height, state_root));
        self.ledger_digests.push((
            height,
            LedgerDigest {
                deriv_value: snap.deriv_value,
                collateral_total: snap.collateral_total,
                bad_debt_total: snap.bad_debt_total,
                insurance_collateral: snap.insurance_collateral,
                open_positions: snap.open_positions,
            },
        ));
        match self.regimes.last_mut() {
            Some((name, _, last)) if *name == scenario.name() => *last = height,
            _ => self
                .regimes
                .push((scenario.name().to_string(), height, height)),
        }

        let mut liquidations_this_block = 0usize;
        for tx in txs {
            self.txs_submitted += 1;
            if let Some(bot) = self.bots.iter_mut().find(|b| b.address == tx.sender) {
                bot.txs_submitted += 1;
            }
            match &tx.outcome {
                Ok(()) => self.txs_accepted += 1,
                Err(e) => {
                    *self.rejections.entry(e.clone()).or_insert(0) += 1;
                    if let Some(bot) = self.bots.iter_mut().find(|b| b.address == tx.sender) {
                        bot.txs_rejected += 1;
                    }
                }
            }
            if let (Some(owner), Ok(())) = (tx.target, &tx.outcome) {
                liquidations_this_block += 1;
                if let Some(bot) = self.bots.iter_mut().find(|b| b.address == owner) {
                    bot.liquidated += 1;
                }
            }
        }
        self.peak_liquidations_in_block =
            self.peak_liquidations_in_block.max(liquidations_this_block);

        // Settlement zeroes every position on a market at once, which is not
        // trading; excluding that block keeps the volume figure meaningful.
        if let (Some(prev), false) = (self.prev.as_ref(), settled_block) {
            let marks = crate::invariants::all_marks(db);
            for (market, base) in per_market_volume(prev, snap) {
                *self.volume_base.entry(market).or_insert(0) += base;
                let mark = marks
                    .iter()
                    .find(|(m, _)| *m == market)
                    .map(|(_, p)| *p)
                    .unwrap_or(0);
                let notional = base.saturating_mul(mark) / NOTIONAL_DEN;
                *self.volume_notional.entry(market).or_insert(0) += notional;
            }
        }

        if !funding_markets.is_empty() {
            if let Some(prev) = self.prev.as_ref() {
                for (owner, before) in &prev.collateral_by_owner {
                    let after = snap.collateral_by_owner.get(owner).copied().unwrap_or(0);
                    if after < *before {
                        self.funding_paid += before - after;
                    }
                }
            }
        }

        for (market, (long, _)) in &snap.open_interest {
            let slot = self.peak_open_interest.entry(*market).or_insert(0);
            *slot = (*slot).max(*long);
        }
        self.insurance_min = self.insurance_min.min(snap.insurance_collateral);
        self.prev = Some(snap.clone());
    }

    /// Fill in the end-of-run figures.
    pub fn finalize(&mut self, db: &Database, snap: &LedgerSnapshot, marks: &[(DerivId, u128)]) {
        self.insurance_end = snap.insurance_collateral;
        if self.insurance_min == u128::MAX {
            self.insurance_min = snap.insurance_collateral;
        }
        self.bad_debt_end = snap.bad_debt_total;
        self.liquidations = scan_liquidation_queue(db).len() as u64;
        self.halted_markets = PerpMarkets::new(db)
            .list()
            .iter()
            .filter(|m| m.paused_reduce_only_only || m.paused)
            .map(|m| m.id)
            .collect();
        self.open_interest_end = snap.open_interest.clone();
        self.settled_futures = FuturesMarkets::new(db)
            .list()
            .iter()
            .filter_map(|m| m.final_settlement_px_q8.map(|px| (m.id, px)))
            .collect();

        let positions = crate::invariants::positions_by_owner(db);
        let final_btc_q8 = marks
            .iter()
            .find(|(m, _)| *m == crate::universe::PERP_BTC)
            .map(|(_, p)| *p)
            .unwrap_or(0);

        for bot in self.bots.iter_mut() {
            let margin = snap
                .collateral_by_owner
                .get(&bot.address)
                .copied()
                .unwrap_or(0);
            bot.end_collateral = margin;
            bot.end_bad_debt = snap
                .bad_debt_by_owner
                .get(&bot.address)
                .copied()
                .unwrap_or(0);
            let mut equity = margin as i128 - bot.end_bad_debt as i128;
            if let Some(ps) = positions.get(&bot.address) {
                for p in ps {
                    if let Some((_, mark)) = marks.iter().find(|(m, _)| *m == p.market) {
                        equity += cb_engine::position_unrealized_pnl_micro_usd(
                            p.side,
                            p.qty,
                            p.entry_px_q8,
                            *mark,
                        )
                        .to_i128();
                    }
                }
            }
            bot.end_equity = equity;
            bot.deriv_pnl = equity - bot.start_collateral as i128;

            let account = cb_state::Accounts::new(db)
                .get(&bot.address)
                .unwrap_or_default();
            bot.end_spot_usd = account.balance(AssetId::Usd);
            bot.end_spot_btc = account.balance(AssetId::Btc);
            // Value the base holding at the final index so a spot position
            // that merely moved with the market does not read as profit.
            let value_of = |usd: u128, btc: u128| -> i128 {
                usd as i128 + (btc.saturating_mul(final_btc_q8) / NOTIONAL_DEN) as i128
            };
            bot.spot_pnl = value_of(bot.end_spot_usd, bot.end_spot_btc)
                - value_of(bot.start_spot_usd, bot.start_spot_btc);
        }
    }

    /// Record how many accounts were liquidatable before the keepers acted
    /// this block. Called by the world, which computes the list anyway to
    /// hand to the liquidator bots.
    pub fn note_liquidatable_at_block_start(&mut self, count: usize) {
        self.peak_liquidatable_at_block_start = self.peak_liquidatable_at_block_start.max(count);
    }

    /// A readable end-of-run summary.
    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str("\n================= cb-simgame run summary =================\n");
        s.push_str(&format!("seed            : {:#x}\n", self.seed));
        s.push_str(&format!("timeline        : {}\n", self.timeline));
        s.push_str(&format!(
            "blocks          : {} produced, {} checked\n",
            self.blocks_run, self.blocks_checked
        ));
        let mut regimes = String::new();
        for (name, first, last) in &self.regimes {
            regimes.push_str(&format!("{name}[{first}..{last}] "));
        }
        s.push_str(&format!("regimes         : {}\n", regimes.trim_end()));
        s.push_str(&format!(
            "transactions    : {} submitted, {} accepted, {} rejected\n",
            self.txs_submitted,
            self.txs_accepted,
            self.txs_submitted - self.txs_accepted
        ));

        s.push_str("\n-- trading ------------------------------------------------\n");
        let total_notional: u128 = self.volume_notional.values().sum();
        s.push_str(&format!(
            "matched volume  : ${} across {} markets\n",
            usd(total_notional),
            self.volume_notional.len()
        ));
        for (market, base) in &self.volume_base {
            s.push_str(&format!(
                "  {market:<16} {} base units, ${}\n",
                base,
                usd(self.volume_notional.get(market).copied().unwrap_or(0))
            ));
        }
        s.push_str(&format!(
            "liquidations    : {} triggered; up to {} in one block; up to {} accounts \
             liquidatable at once ({} still under water at a block boundary)\n",
            self.liquidations,
            self.peak_liquidations_in_block,
            self.peak_liquidatable_at_block_start,
            self.peak_below_maintenance
        ));
        if !self.spot_enabled {
            s.push_str(
                "spot market     : not listed (see docs/qa-report.md; --spot reproduces the \
                 lock-accounting defects)\n",
            );
        }
        s.push_str(&format!("funding paid    : ${}\n", usd(self.funding_paid)));
        s.push_str(&format!(
            "insurance fund  : ${} -> ${} (low water ${})\n",
            usd(self.insurance_start),
            usd(self.insurance_end),
            usd(self.insurance_min)
        ));
        s.push_str(&format!("bad debt        : ${}\n", usd(self.bad_debt_end)));
        if !self.halted_markets.is_empty() {
            s.push_str(&format!(
                "halted markets  : {:?} (reduce-only)\n",
                self.halted_markets
            ));
        }

        s.push_str("\n-- rejection paths exercised ------------------------------\n");
        if self.rejections.is_empty() {
            s.push_str("  (none — the noise bot's invalid orders never ran)\n");
        }
        for (reason, count) in &self.rejections {
            s.push_str(&format!("  {count:>6}  {reason}\n"));
        }

        s.push_str("\n-- profit and loss ----------------------------------------\n");
        s.push_str(&format!(
            "  {:<18} {:>6} {:>14} {:>14} {:>14} {:>7}\n",
            "bot", "kind", "deriv P&L", "spot P&L", "equity", "txs"
        ));
        let mut bots: Vec<&BotOutcome> = self.bots.iter().collect();
        bots.sort_by_key(|b| -b.total_pnl());
        for b in bots {
            s.push_str(&format!(
                "  {:<18} {:>6} {:>14} {:>14} {:>14} {:>7}{}\n",
                b.label,
                short_kind(b.kind),
                signed_usd(b.deriv_pnl),
                signed_usd(b.spot_pnl),
                signed_usd(b.end_equity),
                b.txs_submitted,
                if b.liquidated > 0 {
                    format!("  liquidated x{}", b.liquidated)
                } else {
                    String::new()
                }
            ));
        }

        s.push_str("\n-- invariants ---------------------------------------------\n");
        for inv in Invariant::CHECKED_PER_BLOCK {
            let count = self.violations_of(inv).len();
            let status = if !self.exercised.contains(&inv) {
                "NOT EXERCISED".to_string()
            } else if count == 0 {
                "held".to_string()
            } else {
                format!("VIOLATED x{count}")
            };
            s.push_str(&format!("  {:<28} {}\n", inv.to_string(), status));
        }
        s.push_str(&format!(
            "  {:<28} {}\n",
            Invariant::Determinism.to_string(),
            "checked by tests/determinism.rs"
        ));

        if let Some(f) = &self.fatal {
            s.push_str(&format!("\nFATAL: {f}\n"));
        }
        if !self.violations.is_empty() {
            // One worked example per invariant beats five copies of the same
            // one: a reviewer needs to see every distinct failure mode.
            s.push_str("\n-- first violation of each invariant -----------------------\n");
            let mut seen: Vec<Invariant> = Vec::new();
            for v in &self.violations {
                if seen.contains(&v.invariant) {
                    continue;
                }
                seen.push(v.invariant);
                s.push_str(&format!("{v}\n"));
            }
            s.push_str(&format!(
                "\n{} violation(s) total across {} invariant(s)\n",
                self.violations.len(),
                seen.len()
            ));
        }
        s.push_str("===========================================================\n");
        s
    }
}

fn per_market_volume(prev: &LedgerSnapshot, now: &LedgerSnapshot) -> BTreeMap<DerivId, u128> {
    let mut keys: Vec<(Address, DerivId)> = now.signed_positions.keys().copied().collect();
    keys.extend(prev.signed_positions.keys().copied());
    keys.sort();
    keys.dedup();
    let mut out: BTreeMap<DerivId, u128> = BTreeMap::new();
    for k in keys {
        let before = prev.signed_positions.get(&k).copied().unwrap_or(0);
        let after = now.signed_positions.get(&k).copied().unwrap_or(0);
        if before != after {
            *out.entry(k.1).or_insert(0) += (after - before).unsigned_abs();
        }
    }
    for v in out.values_mut() {
        *v /= 2;
    }
    out
}

fn short_kind(k: BotKind) -> &'static str {
    match k {
        BotKind::MarketMaker => "mm",
        BotKind::Momentum => "mom",
        BotKind::Noise => "noise",
        BotKind::Whale => "whale",
        BotKind::Liquidator => "liq",
        BotKind::Arbitrageur => "arb",
    }
}

/// Render micro-USD as dollars with two decimals.
pub fn usd(micro: u128) -> String {
    format!("{}.{:02}", micro / MICRO, (micro % MICRO) / 10_000)
}

/// Render signed micro-USD as dollars with two decimals.
pub fn signed_usd(micro: i128) -> String {
    let sign = if micro < 0 { "-" } else { "" };
    let m = micro.unsigned_abs();
    format!("{sign}{}.{:02}", m / MICRO, (m % MICRO) / 10_000)
}
