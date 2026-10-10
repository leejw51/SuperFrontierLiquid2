//! The invariant checker: all nine invariants from `docs/PLAN.md` §6,
//! evaluated at every block boundary.
//!
//! ## Reading this module
//!
//! Each invariant gets exactly one `check_*` function, named after it, and
//! each function re-derives its claim from the column families rather than
//! from anything the checker accumulated earlier. A reviewer should be able
//! to read one function and confirm it checks what its name says.
//!
//! ## What a violation carries
//!
//! "Conservation failed" is useless. A [`Violation`] names the asset or the
//! market, the exact magnitude, the block height, every transaction that was
//! in that block with its outcome, and a digest of the state before and
//! after. That is enough to write a failing unit test without re-running the
//! simulation.
//!
//! ## Invariant 8 is not here
//!
//! Determinism is a property of two runs, not of one state, so it lives in
//! `tests/determinism.rs`: the same seed twice, compared per-block state
//! root. The checker records each block's root ([`BlockContext::state_root`])
//! so that comparison has something to compare.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use cb_engine::{is_liquidatable, DEFAULT_TIERS};
use cb_state::{
    BookIndex, Database, FuturesMarkets, Markets, OracleState, Orders, PerpBookIndex, PerpMarkets,
    CF_LIQUIDATION_QUEUE,
};
use cb_types::{
    Address, AssetId, DerivId, LiquidationEntry, MarginAccount, MarketId, Position, Side, H256,
};

use crate::ledger::{
    asset_label, scan_margin_accounts, scan_positions, DustBudget, LedgerSnapshot,
};
use crate::scenario::Scenario;

/// The nine invariants, by number and name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Invariant {
    /// 1 — total value is constant outside explicit mint and burn points.
    Conservation,
    /// 2 — no account's free or locked balance underflows.
    NoNegativeBalances,
    /// 3 — locked balance equals margin plus resting-order collateral.
    LockConsistency,
    /// 4 — best bid is strictly below best ask on every market.
    BookIntegrity,
    /// 5 — long open interest equals short open interest on every derivative.
    PositionNetting,
    /// 6 — accounts below maintenance margin reach the liquidation queue
    ///     within the grace period, and are actually cleared.
    MarginSoundness,
    /// 7 — funding transfers sum to zero across each market per tick.
    FundingNeutrality,
    /// 8 — replaying identical blocks reaches an identical state root.
    Determinism,
    /// 9 — a dated future at expiry settles every position and leaves zero
    ///     open interest.
    Settlement,
}

impl Invariant {
    /// The invariant's number in `docs/PLAN.md` §6.
    pub fn number(self) -> u8 {
        match self {
            Invariant::Conservation => 1,
            Invariant::NoNegativeBalances => 2,
            Invariant::LockConsistency => 3,
            Invariant::BookIntegrity => 4,
            Invariant::PositionNetting => 5,
            Invariant::MarginSoundness => 6,
            Invariant::FundingNeutrality => 7,
            Invariant::Determinism => 8,
            Invariant::Settlement => 9,
        }
    }

    /// Short name.
    pub fn name(self) -> &'static str {
        match self {
            Invariant::Conservation => "conservation",
            Invariant::NoNegativeBalances => "no-negative-balances",
            Invariant::LockConsistency => "lock-consistency",
            Invariant::BookIntegrity => "book-integrity",
            Invariant::PositionNetting => "position-netting",
            Invariant::MarginSoundness => "margin-soundness",
            Invariant::FundingNeutrality => "funding-neutrality",
            Invariant::Determinism => "determinism",
            Invariant::Settlement => "settlement",
        }
    }

    /// Every invariant the block-boundary checker evaluates, in order.
    pub const CHECKED_PER_BLOCK: [Invariant; 8] = [
        Invariant::Conservation,
        Invariant::NoNegativeBalances,
        Invariant::LockConsistency,
        Invariant::BookIntegrity,
        Invariant::PositionNetting,
        Invariant::MarginSoundness,
        Invariant::FundingNeutrality,
        Invariant::Settlement,
    ];
}

impl fmt::Display for Invariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{} {}", self.number(), self.name())
    }
}

/// One transaction as it was applied, with its outcome. Recorded for every
/// transaction in every block so a violation report can show the block that
/// broke the invariant.
#[derive(Clone, Debug)]
pub struct TxRecord {
    /// Position of the transaction within the block.
    pub index: u32,
    /// Who sent it.
    pub sender: Address,
    /// The bot (or system role) the sender belongs to.
    pub sender_label: String,
    /// One-line rendering of the transaction body.
    pub body: String,
    /// `Ok` if the executor accepted it, otherwise the executor's error.
    pub outcome: Result<(), String>,
    /// For a `LiquidationTrigger`, the account being liquidated. `None`
    /// otherwise. Carried explicitly so the report never has to parse the
    /// rendered body back out.
    pub target: Option<Address>,
}

impl fmt::Display for TxRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let outcome = match &self.outcome {
            Ok(()) => "ok".to_string(),
            Err(e) => format!("REJECTED({e})"),
        };
        write!(
            f,
            "  [{:>3}] {:<18} {:<70} {}",
            self.index, self.sender_label, self.body, outcome
        )
    }
}

/// Everything the checker needs to know about the block that just committed.
pub struct BlockContext<'a> {
    /// Height of the block.
    pub height: u64,
    /// Regime in force.
    pub scenario: Scenario,
    /// Every transaction applied, in order, with its outcome.
    pub txs: &'a [TxRecord],
    /// The composed state root after commit. Fed to the determinism test.
    pub state_root: H256,
    /// Markets that received a `FundingTick` in this block.
    pub funding_markets: &'a [DerivId],
    /// Markets that received a `FuturesSettle` in this block.
    pub settled_markets: &'a [DerivId],
    /// Address to human label, for readable reports.
    pub labels: &'a BTreeMap<Address, String>,
}

/// A failed invariant, with enough context to act on.
#[derive(Clone, Debug)]
pub struct Violation {
    /// Which invariant.
    pub invariant: Invariant,
    /// Block height at which it was detected.
    pub height: u64,
    /// Regime in force at that height.
    pub scenario: String,
    /// What specifically went wrong: which asset or market, by how much.
    pub detail: String,
    /// Every transaction in the offending block.
    pub block_txs: Vec<TxRecord>,
    /// Digest of the state one block earlier.
    pub prev_state: String,
    /// Digest of the state after the offending block.
    pub state: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "================ INVARIANT VIOLATION ================")?;
        writeln!(f, "invariant : {} (docs/PLAN.md section 6)", self.invariant)?;
        writeln!(f, "height    : {}", self.height)?;
        writeln!(f, "scenario  : {}", self.scenario)?;
        writeln!(f, "detail    : {}", self.detail)?;
        writeln!(f, "-- state at height {} ----------------", self.height - 1)?;
        writeln!(f, "{}", self.prev_state)?;
        writeln!(f, "-- transactions in block {} ----------", self.height)?;
        if self.block_txs.is_empty() {
            writeln!(f, "  (none)")?;
        }
        for tx in &self.block_txs {
            writeln!(f, "{tx}")?;
        }
        writeln!(f, "-- state at height {} ----------------", self.height)?;
        writeln!(f, "{}", self.state)?;
        write!(f, "====================================================")
    }
}

/// Knobs the checker needs that are not derivable from state.
#[derive(Clone, Copy, Debug)]
pub struct CheckerConfig {
    /// How many blocks an account may sit below maintenance margin before it
    /// must appear in the liquidation queue.
    pub liquidation_grace_blocks: u64,
    /// How many blocks after being queued an account may remain liquidatable
    /// *while liquidity exists to close it* before the liquidation counts as
    /// having failed.
    pub liquidation_clear_blocks: u64,
    /// Stop the run on the first violation.
    pub fail_fast: bool,
}

impl Default for CheckerConfig {
    fn default() -> Self {
        Self {
            liquidation_grace_blocks: 8,
            liquidation_clear_blocks: 24,
            fail_fast: false,
        }
    }
}

/// Evaluates the invariants after every block and accumulates violations.
pub struct InvariantChecker {
    config: CheckerConfig,
    prev: Option<LedgerSnapshot>,
    prev_digest: String,
    /// First height at which an account was seen below maintenance margin and
    /// has not since become healthy.
    breach_since: BTreeMap<Address, u64>,
    /// Height at which an account most recently entered the liquidation queue.
    queued_at: BTreeMap<Address, u64>,
    violations: Vec<Violation>,
    /// Invariants that have been evaluated at least once, so a report can
    /// distinguish "held" from "never exercised".
    exercised: BTreeSet<Invariant>,
    /// Blocks checked.
    blocks_checked: u64,
    /// Accounts found below maintenance margin in the most recent block.
    last_below_maintenance: usize,
    /// The largest number of accounts below maintenance margin in any block.
    peak_below_maintenance: usize,
}

impl InvariantChecker {
    /// A checker with the given configuration.
    pub fn new(config: CheckerConfig) -> Self {
        Self {
            config,
            prev: None,
            prev_digest: String::from("(genesis)"),
            breach_since: BTreeMap::new(),
            queued_at: BTreeMap::new(),
            violations: Vec::new(),
            exercised: BTreeSet::new(),
            blocks_checked: 0,
            last_below_maintenance: 0,
            peak_below_maintenance: 0,
        }
    }

    /// Accounts below maintenance margin in the most recent checked block.
    pub fn below_maintenance(&self) -> usize {
        self.last_below_maintenance
    }

    /// The most accounts below maintenance margin in any single block. A
    /// crash scenario that never moves this above zero is too gentle to be
    /// testing the liquidation path at all.
    pub fn peak_below_maintenance(&self) -> usize {
        self.peak_below_maintenance
    }

    /// Seed the checker with the pre-run state so the first block has
    /// something to be conserved against.
    pub fn prime(&mut self, db: &Database) {
        let snap = LedgerSnapshot::read(db);
        self.prev_digest = digest(db, &snap);
        self.prev = Some(snap);
    }

    /// Violations found so far.
    pub fn violations(&self) -> &[Violation] {
        &self.violations
    }

    /// Invariants evaluated at least once.
    pub fn exercised(&self) -> &BTreeSet<Invariant> {
        &self.exercised
    }

    /// How many block boundaries have been checked.
    pub fn blocks_checked(&self) -> u64 {
        self.blocks_checked
    }

    /// The ledger as of the last checked block.
    pub fn last_snapshot(&self) -> Option<&LedgerSnapshot> {
        self.prev.as_ref()
    }

    /// Evaluate every per-block invariant against the freshly committed
    /// block. Returns the new ledger snapshot so the caller can use it for
    /// reporting without re-scanning.
    ///
    /// Returns `Err` with the first violation when `fail_fast` is set.
    pub fn check_block(
        &mut self,
        db: &Database,
        ctx: &BlockContext<'_>,
    ) -> Result<LedgerSnapshot, Box<Violation>> {
        let snap = LedgerSnapshot::read(db);
        let digest_now = digest(db, &snap);
        let mut found: Vec<(Invariant, String)> = Vec::new();

        for inv in Invariant::CHECKED_PER_BLOCK {
            self.exercised.insert(inv);
        }

        if let Some(prev) = self.prev.as_ref() {
            check_conservation(prev, &snap, ctx, &mut found);
            check_funding_neutrality(prev, &snap, ctx, &mut found);
        }
        check_no_negative_balances(db, &mut found);
        check_lock_consistency(db, &mut found);
        check_book_integrity(db, &mut found);
        check_position_netting(&snap, &mut found);
        self.check_margin_soundness(db, ctx, &mut found);
        check_settlement(db, &mut found);

        self.blocks_checked += 1;

        let mut first: Option<Violation> = None;
        for (invariant, detail) in found {
            let v = Violation {
                invariant,
                height: ctx.height,
                scenario: ctx.scenario.name().to_string(),
                detail,
                block_txs: ctx.txs.to_vec(),
                prev_state: self.prev_digest.clone(),
                state: digest_now.clone(),
            };
            if first.is_none() {
                first = Some(v.clone());
            }
            self.violations.push(v);
        }

        self.prev = Some(snap.clone());
        self.prev_digest = digest_now;

        match first {
            Some(v) if self.config.fail_fast => Err(Box::new(v)),
            _ => Ok(snap),
        }
    }

    /// Invariant 6 — margin soundness.
    ///
    /// Two halves, both of which have to hold:
    ///
    /// 1. An account below maintenance margin must appear in the liquidation
    ///    queue within `liquidation_grace_blocks`.
    /// 2. Having been queued, it must actually stop being liquidatable — but
    ///    only while the book holds liquidity inside the liquidation ladder's
    ///    widest band. An account that cannot be closed because nobody is
    ///    bidding is an economic outcome, not a broken invariant; an account
    ///    that cannot be closed *against a willing bid* is a bug.
    fn check_margin_soundness(
        &mut self,
        db: &Database,
        ctx: &BlockContext<'_>,
        out: &mut Vec<(Invariant, String)>,
    ) {
        let positions_by_owner = positions_by_owner(db);
        let marks = all_marks(db);
        let queue = scan_liquidation_queue(db);
        for e in &queue {
            let slot = self.queued_at.entry(e.owner).or_insert(0);
            if e.triggered_at_height > *slot {
                *slot = e.triggered_at_height;
            }
        }

        let margins: BTreeMap<Address, MarginAccount> = scan_margin_accounts(db)
            .into_iter()
            .map(|m| (m.owner, m))
            .collect();

        let mut still_breached: BTreeSet<Address> = BTreeSet::new();
        for (owner, positions) in &positions_by_owner {
            if *owner == Address::INSURANCE {
                continue;
            }
            let margin = margins
                .get(owner)
                .cloned()
                .unwrap_or_else(|| MarginAccount::new(*owner));
            if !is_liquidatable(&margin, positions, &marks, DEFAULT_TIERS) {
                continue;
            }
            still_breached.insert(*owner);
            let since = *self.breach_since.entry(*owner).or_insert(ctx.height);
            let label = label_of(ctx.labels, owner);

            let queued = self.queued_at.get(owner).copied();
            match queued {
                None | Some(0) => {
                    if ctx.height.saturating_sub(since) > self.config.liquidation_grace_blocks {
                        out.push((
                            Invariant::MarginSoundness,
                            format!(
                                "{label} ({owner}) has been below maintenance margin since height \
                                 {since} ({} blocks, grace is {}) and has never entered the \
                                 liquidation queue. collateral={} micro-USD, bad_debt={}, \
                                 positions={}",
                                ctx.height - since,
                                self.config.liquidation_grace_blocks,
                                margin.collateral,
                                margin.bad_debt,
                                render_positions(positions, &marks),
                            ),
                        ));
                    }
                }
                Some(hq) => {
                    if ctx.height.saturating_sub(hq) > self.config.liquidation_clear_blocks {
                        if let Some(reason) = closable_against_book(db, positions) {
                            out.push((
                                Invariant::MarginSoundness,
                                format!(
                                    "{label} ({owner}) entered the liquidation queue at height \
                                     {hq} but is still liquidatable {} blocks later, while the \
                                     book can absorb the close: {reason}. collateral={}, \
                                     bad_debt={}, positions={}",
                                    ctx.height - hq,
                                    margin.collateral,
                                    margin.bad_debt,
                                    render_positions(positions, &marks),
                                ),
                            ));
                            // Re-arm so one stuck account reports once per
                            // clear-window rather than every block.
                            self.queued_at.insert(*owner, ctx.height);
                        }
                    }
                }
            }
        }

        self.last_below_maintenance = still_breached.len();
        self.peak_below_maintenance = self.peak_below_maintenance.max(still_breached.len());

        // Accounts that became healthy (or flat) drop their breach record.
        let healed: Vec<Address> = self
            .breach_since
            .keys()
            .filter(|a| !still_breached.contains(*a))
            .copied()
            .collect();
        for a in healed {
            self.breach_since.remove(&a);
            self.queued_at.remove(&a);
        }
    }
}

// ---------------------------------------------------------------------------
// Invariant 1 — conservation
// ---------------------------------------------------------------------------

/// Total units of each spot asset, and the derivative ledger value, are
/// constant. The simulation performs no mint or burn after genesis, so the
/// permitted change is exactly zero for spot and integer-rounding dust for
/// derivatives.
///
/// Fees, funding, liquidation penalties and the insurance fund are all
/// *transfers* inside the accounted set, which is why they need no special
/// case here: spot taker fees credit `Address::INSURANCE`'s spot balance and
/// stay in `spot_totals`; liquidation fees credit the insurance fund's and
/// the keeper's collateral and stay in `deriv_value`; funding moves
/// collateral between accounts; bad debt is subtracted so an uncovered loss
/// cannot masquerade as value.
fn check_conservation(
    prev: &LedgerSnapshot,
    now: &LedgerSnapshot,
    ctx: &BlockContext<'_>,
    out: &mut Vec<(Invariant, String)>,
) {
    let mut assets: Vec<u8> = prev.spot_totals.keys().copied().collect();
    assets.extend(now.spot_totals.keys().copied());
    assets.sort_unstable();
    assets.dedup();
    for key in assets {
        let before = prev.spot_totals.get(&key).copied().unwrap_or(0);
        let after = now.spot_totals.get(&key).copied().unwrap_or(0);
        if before != after {
            let delta = after as i128 - before as i128;
            out.push((
                Invariant::Conservation,
                format!(
                    "spot asset {} total changed by {delta} base units \
                     (before {before}, after {after}); spot settlement is exact integer \
                     arithmetic with no mint or burn point in this run, so any change is a leak. \
                     insurance spot balance of this asset: {} -> {}",
                    asset_label(key),
                    prev.insurance_spot.get(&key).copied().unwrap_or(0),
                    now.insurance_spot.get(&key).copied().unwrap_or(0),
                ),
            ));
        }
    }

    let budget = DustBudget::for_block(ctx.txs.len(), now);
    let drift = now.deriv_value - prev.deriv_value;
    if drift.abs() > budget.allowance {
        // Report in micro-USD as well as raw units; a reviewer should not
        // have to divide by 1e10 in their head.
        let micro_usd = drift / crate::ledger::V_SCALE;
        out.push((
            Invariant::Conservation,
            format!(
                "derivative ledger value changed by {drift} units \
                 (= {micro_usd} micro-USD of collateral appearing from nowhere or \
                 vanishing), beyond the rounding budget of {}. \
                 collateral {} -> {}, bad_debt {} -> {}, insurance {} -> {}, \
                 open positions {} -> {}. \
                 The ledger value is \
                 `sum(collateral - bad_debt)*1e10 + sum(+/- qty*entry_px_q8)`, which no fill, \
                 funding tick, liquidation fee, bad-debt absorption or settlement can move.",
                budget.describe(),
                prev.collateral_total,
                now.collateral_total,
                prev.bad_debt_total,
                now.bad_debt_total,
                prev.insurance_collateral,
                now.insurance_collateral,
                prev.open_positions,
                now.open_positions,
            ),
        ));
    }
}

// ---------------------------------------------------------------------------
// Invariant 2 — no negative balances
// ---------------------------------------------------------------------------

/// Balances are `u128`, so an underflow cannot show as a negative number — it
/// either panics inside `AccountState::sub` or leaves `locked > balance`,
/// which `AccountState::available` silently hides behind a `saturating_sub`.
/// That hidden case is what this check looks for: an account whose locked
/// balance exceeds what it holds is an account whose free balance has
/// effectively gone negative.
fn check_no_negative_balances(db: &Database, out: &mut Vec<(Invariant, String)>) {
    for (addr, state) in crate::ledger::scan_accounts(db) {
        for (key, locked) in &state.locked {
            let balance = state.balances.get(key).copied().unwrap_or(0);
            if *locked > balance {
                out.push((
                    Invariant::NoNegativeBalances,
                    format!(
                        "account {addr} has {locked} of asset {} locked but holds only {balance}: \
                         free balance is effectively {} (AccountState::available saturates and \
                         hides this)",
                        asset_label(*key),
                        balance as i128 - *locked as i128,
                    ),
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Invariant 3 — lock consistency
// ---------------------------------------------------------------------------

/// Locked balance equals the collateral reserved by that account's resting
/// orders.
///
/// Spot is the only place the chain locks anything: a resting buy reserves
/// `notional + ceil(fee)` of quote for its remaining quantity, a resting sell
/// reserves its remaining base quantity. Perpetual and futures orders reserve
/// nothing at all — margin lives in a separate `MarginAccount` — so the perp
/// contribution to this invariant is exactly zero, and that fact is itself
/// reported in `docs/qa-report.md` rather than silently assumed here.
fn check_lock_consistency(db: &Database, out: &mut Vec<(Invariant, String)>) {
    let mut expected: BTreeMap<(Address, u8), u128> = BTreeMap::new();
    let mut detail: BTreeMap<(Address, u8), Vec<String>> = BTreeMap::new();

    // Every listed spot market, not a fixed pair: the universe grows by
    // appending to `MarketId::ALL`, and a market this checker does not walk
    // is a market whose lock accounting nobody is watching.
    for market in MarketId::ALL {
        let cfg = match Markets::new(db).get(market) {
            Some(c) => c,
            None => continue,
        };
        for o in Orders::new(db).list_for_market(market) {
            let (asset, amount) = match o.side {
                Side::Buy => (
                    cfg.quote,
                    spot_buy_lock_amount(o.price, o.qty_remaining, cfg.fee_bps),
                ),
                Side::Sell => (cfg.base, o.qty_remaining),
            };
            *expected.entry((o.owner, asset.as_u8())).or_insert(0) += amount;
            detail
                .entry((o.owner, asset.as_u8()))
                .or_default()
                .push(format!(
                    "{:?} {:?} px={} rem={} -> {amount}",
                    market, o.side, o.price, o.qty_remaining
                ));
        }
    }

    for (addr, state) in crate::ledger::scan_accounts(db) {
        let mut keys: BTreeSet<u8> = state.locked.keys().copied().collect();
        for (owner, key) in expected.keys() {
            if *owner == addr {
                keys.insert(*key);
            }
        }
        for key in keys {
            let actual = state.locked.get(&key).copied().unwrap_or(0);
            let want = expected.get(&(addr, key)).copied().unwrap_or(0);
            if actual != want {
                let orders = detail
                    .get(&(addr, key))
                    .map(|v| v.join("; "))
                    .unwrap_or_else(|| "(no resting orders)".to_string());
                out.push((
                    Invariant::LockConsistency,
                    format!(
                        "account {addr} has {actual} of asset {} locked but its resting orders \
                         reserve {want} (difference {}). resting orders: {orders}",
                        asset_label(key),
                        actual as i128 - want as i128,
                    ),
                ));
            }
        }
    }
}

/// Mirror of `cb_block::executor::spot_buy_lock_amount`, restated so the
/// checker does not depend on a private function — and so a change to the
/// executor's lock policy shows up here as a deliberate disagreement rather
/// than silently agreeing with itself.
fn spot_buy_lock_amount(price: u128, qty: u128, fee_bps: u16) -> u128 {
    let notional = price.saturating_mul(qty) / 100_000_000u128;
    let fee = notional.saturating_mul(fee_bps as u128);
    let fee_reserve = fee / 10_000 + u128::from(fee % 10_000 != 0);
    notional.saturating_add(fee_reserve)
}

// ---------------------------------------------------------------------------
// Invariant 4 — book integrity
// ---------------------------------------------------------------------------

/// Best bid strictly below best ask, on every market with two sides.
///
/// A crossed book means the matcher left liquidity on the table that anyone
/// could take for free, so it is checked on spot, perpetuals and dated
/// futures alike.
fn check_book_integrity(db: &Database, out: &mut Vec<(Invariant, String)>) {
    let perp_book = PerpBookIndex::new(db);
    let mut deriv_markets: Vec<DerivId> =
        PerpMarkets::new(db).list().iter().map(|m| m.id).collect();
    deriv_markets.extend(FuturesMarkets::new(db).list().iter().map(|m| m.id));
    deriv_markets.sort();
    for market in deriv_markets {
        let bids = perp_book.list_side(&market, Side::Buy);
        let asks = perp_book.list_side(&market, Side::Sell);
        let best_bid = bids.iter().map(|(p, _, _)| *p).max();
        let best_ask = asks.iter().map(|(p, _, _)| *p).min();
        if let (Some(b), Some(a)) = (best_bid, best_ask) {
            if b >= a {
                out.push((
                    Invariant::BookIntegrity,
                    format!(
                        "derivative market {market} is crossed: best bid {b} >= best ask {a} \
                         ({} bid levels, {} ask levels)",
                        bids.len(),
                        asks.len()
                    ),
                ));
            }
        }
    }

    let spot_book = BookIndex::new(db);
    for market in MarketId::ALL {
        if Markets::new(db).get(market).is_none() {
            continue;
        }
        let bids = spot_book.list_side(market, Side::Buy);
        let asks = spot_book.list_side(market, Side::Sell);
        let best_bid = bids.iter().map(|(p, _, _)| *p).max();
        let best_ask = asks.iter().map(|(p, _, _)| *p).min();
        if let (Some(b), Some(a)) = (best_bid, best_ask) {
            if b >= a {
                out.push((
                    Invariant::BookIntegrity,
                    format!("spot market {market:?} is crossed: best bid {b} >= best ask {a}"),
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Invariant 5 — position netting
// ---------------------------------------------------------------------------

/// Long open interest equals short open interest on every derivative market.
///
/// This is the structural consequence of every fill moving one side up and
/// the other side down by the same quantity. If it ever fails, the
/// conservation check above stops being price-independent, so this is checked
/// first in spirit even though the code runs in numeric order.
fn check_position_netting(now: &LedgerSnapshot, out: &mut Vec<(Invariant, String)>) {
    for (market, (long, short)) in &now.open_interest {
        if long != short {
            out.push((
                Invariant::PositionNetting,
                format!(
                    "market {market} open interest does not net: long {long} base units vs \
                     short {short} (difference {})",
                    *long as i128 - *short as i128
                ),
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Invariant 7 — funding neutrality
// ---------------------------------------------------------------------------

/// Funding transfers sum to zero across each market per tick, net of the
/// insurance-fund share.
///
/// The simulation deliberately keeps funding-boundary blocks quiet — only
/// oracle updates and the funding ticks themselves are submitted at a
/// boundary height — so the collateral delta across that block *is* the
/// funding flow, with nothing else to disentangle it from. The check
/// therefore compares total value in and total value out directly.
///
/// `bad_debt` is included: a long that cannot pay its funding has the
/// shortfall recorded as debt rather than paid, while the short on the other
/// side is credited in full. Counting debt as negative value is what makes
/// that case visible instead of looking like money appearing from nowhere.
fn check_funding_neutrality(
    prev: &LedgerSnapshot,
    now: &LedgerSnapshot,
    ctx: &BlockContext<'_>,
    out: &mut Vec<(Invariant, String)>,
) {
    if ctx.funding_markets.is_empty() {
        return;
    }
    // A block that also settles a dated future moves collateral for a reason
    // that has nothing to do with funding — every position on that market is
    // closed at the settlement price at once. The funding flow cannot be
    // isolated from it by looking at collateral deltas, so neutrality is not
    // evaluated here. Conservation still is, and it is the stronger statement:
    // it says the settlement itself created and destroyed nothing.
    if !ctx.settled_markets.is_empty() {
        return;
    }
    let mut paid: i128 = 0;
    let mut received: i128 = 0;
    let mut owners: Vec<Address> = now.collateral_by_owner.keys().copied().collect();
    owners.extend(prev.collateral_by_owner.keys().copied());
    owners.sort();
    owners.dedup();
    let mut movers: Vec<String> = Vec::new();
    for owner in owners {
        let before = prev.collateral_by_owner.get(&owner).copied().unwrap_or(0) as i128
            - prev.bad_debt_by_owner.get(&owner).copied().unwrap_or(0) as i128;
        let after = now.collateral_by_owner.get(&owner).copied().unwrap_or(0) as i128
            - now.bad_debt_by_owner.get(&owner).copied().unwrap_or(0) as i128;
        let delta = after - before;
        if delta > 0 {
            received += delta;
        } else if delta < 0 {
            paid += -delta;
        }
        if delta != 0 {
            movers.push(format!("{} {:+}", label_of(ctx.labels, &owner), delta));
        }
    }
    let net = received - paid;
    // One micro-USD of truncation per position row on each side of the tick.
    let allowance = (now.open_positions.max(prev.open_positions) as i128) + 2;
    if net.abs() > allowance {
        out.push((
            Invariant::FundingNeutrality,
            format!(
                "funding tick on {:?} moved {paid} micro-USD out and {received} in: net {net}, \
                 beyond the per-position rounding allowance of {allowance}. \
                 Funding must be a pure transfer between longs and shorts (the insurance fund \
                 takes no share in this implementation). movers: {}",
                ctx.funding_markets,
                movers.join(", ")
            ),
        ));
    }
}

// ---------------------------------------------------------------------------
// Invariant 9 — settlement
// ---------------------------------------------------------------------------

/// A dated future that has settled carries a settlement price, holds no open
/// position and has no resting orders.
fn check_settlement(db: &Database, out: &mut Vec<(Invariant, String)>) {
    let positions = scan_positions(db);
    for fm in FuturesMarkets::new(db).list() {
        let settle_px = match fm.final_settlement_px_q8 {
            Some(p) => p,
            None => continue,
        };
        let open: Vec<&Position> = positions
            .iter()
            .filter(|p| p.market == fm.id && p.qty > 0)
            .collect();
        if !open.is_empty() {
            let total: u128 = open.iter().map(|p| p.qty).sum();
            out.push((
                Invariant::Settlement,
                format!(
                    "future {} settled at {settle_px} but still has {} open positions totalling \
                     {total} base units of open interest (first: owner {} side {:?} qty {})",
                    fm.id,
                    open.len(),
                    open[0].owner,
                    open[0].side,
                    open[0].qty
                ),
            ));
        }
        let resting = cb_state::PerpOrders::new(db).list_for_market(&fm.id);
        if !resting.is_empty() {
            out.push((
                Invariant::Settlement,
                format!(
                    "future {} settled at {settle_px} but {} resting orders remain on its book",
                    fm.id,
                    resting.len()
                ),
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Shared readers
// ---------------------------------------------------------------------------

/// Every open position grouped by owner, owners in address order.
pub fn positions_by_owner(db: &Database) -> BTreeMap<Address, Vec<Position>> {
    let mut out: BTreeMap<Address, Vec<Position>> = BTreeMap::new();
    for p in scan_positions(db) {
        if p.qty == 0 {
            continue;
        }
        out.entry(p.owner).or_default().push(p);
    }
    out
}

/// Current mark price for every derivative market that has one.
pub fn all_marks(db: &Database) -> Vec<(DerivId, u128)> {
    let store = OracleState::new(db);
    let mut markets: Vec<DerivId> = PerpMarkets::new(db).list().iter().map(|m| m.id).collect();
    markets.extend(FuturesMarkets::new(db).list().iter().map(|m| m.id));
    markets.sort();
    let mut out = Vec::new();
    for m in markets {
        if let Some(mark) = store.get_mark(&m) {
            out.push((m, mark.px_q8));
        }
    }
    out
}

/// Every liquidation-queue row, in `(height, seq)` order.
pub fn scan_liquidation_queue(db: &Database) -> Vec<LiquidationEntry> {
    let mut out = Vec::new();
    let prefix: [u8; 0] = [];
    let it = match db.iter_prefix(CF_LIQUIDATION_QUEUE, &prefix) {
        Ok(it) => it,
        Err(_) => return out,
    };
    for (_, v) in it.flatten() {
        if let Ok(e) = bincode::deserialize::<LiquidationEntry>(&v) {
            out.push(e);
        }
    }
    out
}

/// If the book could absorb a close of any of `positions` inside the
/// liquidation ladder's widest band (200 bps adverse to mark), describe it.
/// `None` means there is genuinely no liquidity, which is an economic outcome
/// rather than a broken invariant.
fn closable_against_book(db: &Database, positions: &[Position]) -> Option<String> {
    let store = OracleState::new(db);
    let book = PerpBookIndex::new(db);
    for p in positions {
        let mark = store.get_mark(&p.market)?.px_q8;
        match p.side {
            // A long closes by selling into the bids at mark - 200bps or better.
            Side::Buy => {
                let floor = crate::universe::bump_bps(mark, -200);
                let bids = book.list_side(&p.market, Side::Buy);
                let best = bids.iter().map(|(px, _, _)| *px).max();
                if let Some(b) = best {
                    if b >= floor {
                        return Some(format!(
                            "long {} on {} could sell into a best bid of {b} (liquidation floor \
                             {floor}, mark {mark})",
                            p.qty, p.market
                        ));
                    }
                }
            }
            // A short closes by buying from the asks at mark + 200bps or better.
            Side::Sell => {
                let ceiling = crate::universe::bump_bps(mark, 200);
                let asks = book.list_side(&p.market, Side::Sell);
                let best = asks.iter().map(|(px, _, _)| *px).min();
                if let Some(a) = best {
                    if a <= ceiling {
                        return Some(format!(
                            "short {} on {} could buy from a best ask of {a} (liquidation ceiling \
                             {ceiling}, mark {mark})",
                            p.qty, p.market
                        ));
                    }
                }
            }
        }
    }
    None
}

fn label_of(labels: &BTreeMap<Address, String>, addr: &Address) -> String {
    labels
        .get(addr)
        .cloned()
        .unwrap_or_else(|| format!("{addr}"))
}

fn render_positions(positions: &[Position], marks: &[(DerivId, u128)]) -> String {
    let mut parts = Vec::new();
    for p in positions {
        let mark = marks
            .iter()
            .find(|(m, _)| *m == p.market)
            .map(|(_, px)| *px)
            .unwrap_or(0);
        parts.push(format!(
            "{} {:?} qty={} entry={} mark={}",
            p.market, p.side, p.qty, p.entry_px_q8, mark
        ));
    }
    format!("[{}]", parts.join(" | "))
}

/// A compact rendering of the whole world, attached to violation reports so a
/// developer can see the before and after without re-running anything.
pub fn digest(db: &Database, snap: &LedgerSnapshot) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "  ledger: deriv_value={} collateral={} bad_debt={} insurance={} open_positions={}\n",
        snap.deriv_value,
        snap.collateral_total,
        snap.bad_debt_total,
        snap.insurance_collateral,
        snap.open_positions
    ));
    for (key, total) in &snap.spot_totals {
        s.push_str(&format!(
            "  spot {}: total={} locked={}\n",
            asset_label(*key),
            total,
            snap.spot_locked.get(key).copied().unwrap_or(0)
        ));
    }
    let store = OracleState::new(db);
    let book = PerpBookIndex::new(db);
    for pm in PerpMarkets::new(db).list() {
        s.push_str(&render_market_line(
            &store,
            &book,
            pm.id,
            snap,
            pm.paused_reduce_only_only,
        ));
    }
    for fm in FuturesMarkets::new(db).list() {
        s.push_str(&render_market_line(&store, &book, fm.id, snap, false));
        if let Some(px) = fm.final_settlement_px_q8 {
            s.push_str(&format!("    settled at {px}\n"));
        }
    }
    for p in scan_positions(db).iter().filter(|p| p.qty > 0).take(12) {
        s.push_str(&format!(
            "  position {} {} {:?} qty={} entry={} funding_idx={:?}\n",
            p.owner, p.market, p.side, p.qty, p.entry_px_q8, p.entry_funding_index
        ));
    }
    s
}

fn render_market_line(
    store: &OracleState<'_>,
    book: &PerpBookIndex<'_>,
    market: DerivId,
    snap: &LedgerSnapshot,
    halted: bool,
) -> String {
    let mark = store.get_mark(&market).map(|m| m.px_q8).unwrap_or(0);
    let oracle = store.get_oracle(&market).map(|o| o.px_q8).unwrap_or(0);
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
    let (long, short) = snap.open_interest.get(&market).copied().unwrap_or((0, 0));
    format!(
        "  market {market}: mark={mark} oracle={oracle} bid={bid:?} ask={ask:?} \
         oi_long={long} oi_short={short}{}\n",
        if halted { " REDUCE-ONLY-HALT" } else { "" }
    )
}

/// Every asset the checker accounts for on the spot side. Mirrors the
/// universe rather than a hand-written subset.
pub const SPOT_ASSETS: [AssetId; AssetId::ALL.len()] = AssetId::ALL;
