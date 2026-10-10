//! Value accounting: the quantities that must not change, and how they are
//! derived from raw state.
//!
//! ## The problem with the obvious conservation check
//!
//! "Total equity is constant" is the intuitive statement, but mark-to-market
//! equity moves every time the mark moves, and each position's unrealised PnL
//! is truncated independently, so the obvious check either drifts with price
//! or needs a tolerance large enough to hide real bugs.
//!
//! ## The quantity that is actually conserved
//!
//! Give each position the cash it would have taken to acquire it: a long of
//! `qty` at `entry` consumed `qty * entry` of value, a short produced it.
//! Then
//!
//! ```text
//! V = Σ_accounts (collateral − bad_debt) · 1e10
//!   + Σ_positions s · qty · entry_px_q8        where s = −1 long, +1 short
//! ```
//!
//! is **independent of price** and does not change under any state transition
//! the exchange performs:
//!
//! | Transition | collateral term | position term | net |
//! |---|---|---|---|
//! | Two accounts open against each other at `p` | `0` | `−qp + qp` | `0` |
//! | One closes a long at `p` (entry `e`), other opens long at `p` | `+q(p−e)` | `+qe − qp` | `0` |
//! | Funding: long pays `d`, short receives `d` | `−d + d` | unchanged | `0` |
//! | Liquidation fee | `−f` on the victim, `+f` split to insurance and keeper | unchanged | `0` |
//! | Bad debt: debit `D` against collateral `C < D` | `−C` collateral, `−(D−C)` via `bad_debt` | unchanged | `−D`, matching the debit |
//! | Bad-debt absorption of `c` from insurance | `−c` insurance, `+c` debt cleared | unchanged | `0` |
//! | Settlement at `s` over matched open interest | `+q(s−e)` long, `+q(e′−s)` short | `+qe − qe′` | `0` |
//!
//! So `V` changing by more than integer-rounding dust is, by construction, a
//! leak: value appearing from nowhere or vanishing.
//!
//! ## The dust budget
//!
//! Three places in the exchange truncate:
//!
//! * `credit_realized_pnl` and futures settlement divide by `1e10` — up to
//!   `1e10` of `V` per event.
//! * funding divides by `RATE_DEN_Q9` per position — up to `1e10` of `V` per
//!   position, and because payer and receiver truncate *independently*,
//!   funding is not exactly zero-sum.
//! * `apply_fill_to_position`'s volume-weighted entry rounds toward zero, and
//!   its residual is bounded by the *new position size*, not by a constant.
//!
//! [`DustBudget`] therefore scales with both the number of value-moving
//! events in a block and the open position sizes. Even so it stays roughly
//! six orders of magnitude below a one-dollar leak (`1e6` micro-USD = `1e16`
//! in `V` units), so it cannot mask a bug anyone would care about.
//!
//! Spot is accounted separately and exactly: spot never truncates, so the sum
//! of each asset's balances across every account is constant to the unit.

use std::collections::BTreeMap;

use cb_state::{Database, CF_ACCOUNTS, CF_MARGIN_ACCOUNTS, CF_POSITIONS};
use cb_types::{Address, AssetId, DerivId, MarginAccount, Position, Side};

/// `1e10`, the denominator that converts `qty * price_q8` into micro-USD.
/// Also the natural size of one truncation event in `V` units.
pub const V_SCALE: i128 = 10_000_000_000;

/// Everything the conservation check needs, read out of the column families.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LedgerSnapshot {
    /// Per-asset total spot balance, keyed by `AssetId::as_u8()`.
    pub spot_totals: BTreeMap<u8, u128>,
    /// Per-asset total *locked* spot balance.
    pub spot_locked: BTreeMap<u8, u128>,
    /// The conserved derivative quantity described in the module docs.
    pub deriv_value: i128,
    /// Σ collateral across every margin account, micro-USD.
    pub collateral_total: u128,
    /// Σ bad debt across every margin account, micro-USD.
    pub bad_debt_total: u128,
    /// The insurance fund's collateral, micro-USD.
    pub insurance_collateral: u128,
    /// The insurance fund's spot balances, keyed by `AssetId::as_u8()` — spot
    /// taker fees land here, so this is the spot fee sink.
    pub insurance_spot: BTreeMap<u8, u128>,
    /// Open interest per derivative market as `(long_qty, short_qty)`.
    pub open_interest: BTreeMap<DerivId, (u128, u128)>,
    /// Σ qty over every open position — the scale term of the dust budget.
    pub total_position_qty: u128,
    /// Number of position rows with `qty > 0`.
    pub open_positions: usize,
    /// Per-owner signed position size per market: `+qty` long, `−qty` short.
    /// Used to measure traded volume exactly (each fill moves two entries by
    /// `±qty`).
    pub signed_positions: BTreeMap<(Address, DerivId), i128>,
    /// Per-owner collateral, so funding flow can be measured per account.
    pub collateral_by_owner: BTreeMap<Address, u128>,
    /// Per-owner bad debt.
    pub bad_debt_by_owner: BTreeMap<Address, u128>,
}

impl LedgerSnapshot {
    /// Read the whole ledger out of RocksDB.
    ///
    /// Deliberately a full scan: a checker that maintained its own running
    /// total could agree with a buggy exchange. Everything here comes from
    /// the same rows a replica would read.
    pub fn read(db: &Database) -> Self {
        let mut snap = LedgerSnapshot::default();

        for (addr, state) in scan_accounts(db) {
            for (asset_key, amount) in &state.balances {
                *snap.spot_totals.entry(*asset_key).or_insert(0) += *amount;
                if addr == Address::INSURANCE {
                    *snap.insurance_spot.entry(*asset_key).or_insert(0) += *amount;
                }
            }
            for (asset_key, amount) in &state.locked {
                *snap.spot_locked.entry(*asset_key).or_insert(0) += *amount;
            }
        }

        for margin in scan_margin_accounts(db) {
            snap.collateral_total = snap.collateral_total.saturating_add(margin.collateral);
            snap.bad_debt_total = snap.bad_debt_total.saturating_add(margin.bad_debt);
            snap.collateral_by_owner
                .insert(margin.owner, margin.collateral);
            snap.bad_debt_by_owner.insert(margin.owner, margin.bad_debt);
            if margin.owner == Address::INSURANCE {
                snap.insurance_collateral = margin.collateral;
            }
            snap.deriv_value = snap
                .deriv_value
                .saturating_add((margin.collateral as i128).saturating_mul(V_SCALE))
                .saturating_sub((margin.bad_debt as i128).saturating_mul(V_SCALE));
        }

        for pos in scan_positions(db) {
            let entry = snap.open_interest.entry(pos.market).or_insert((0, 0));
            let signed_qty = match pos.side {
                Side::Buy => pos.qty as i128,
                Side::Sell => -(pos.qty as i128),
            };
            snap.signed_positions
                .insert((pos.owner, pos.market), signed_qty);
            if pos.qty == 0 {
                continue;
            }
            match pos.side {
                Side::Buy => entry.0 = entry.0.saturating_add(pos.qty),
                Side::Sell => entry.1 = entry.1.saturating_add(pos.qty),
            }
            snap.open_positions += 1;
            snap.total_position_qty = snap.total_position_qty.saturating_add(pos.qty);
            // Cost basis: a long consumed value, a short produced it.
            let basis = (pos.qty as i128).saturating_mul(pos.entry_px_q8 as i128);
            snap.deriv_value = match pos.side {
                Side::Buy => snap.deriv_value.saturating_sub(basis),
                Side::Sell => snap.deriv_value.saturating_add(basis),
            };
        }

        snap
    }

    /// Base-unit volume traded between two snapshots.
    ///
    /// Exact, not a proxy: a fill of `q` moves the taker's signed position by
    /// `±q` and the maker's by `∓q`, so the sum of absolute changes is `2q`.
    pub fn traded_volume_base(&self, prev: &LedgerSnapshot) -> u128 {
        let mut keys: Vec<&(Address, DerivId)> = self.signed_positions.keys().collect();
        for k in prev.signed_positions.keys() {
            if !self.signed_positions.contains_key(k) {
                keys.push(k);
            }
        }
        keys.sort();
        keys.dedup();
        let mut total: u128 = 0;
        for k in keys {
            let before = prev.signed_positions.get(k).copied().unwrap_or(0);
            let after = self.signed_positions.get(k).copied().unwrap_or(0);
            total = total.saturating_add((after - before).unsigned_abs());
        }
        total / 2
    }
}

/// How much `V` is allowed to move in one block purely from integer rounding.
#[derive(Clone, Copy, Debug)]
pub struct DustBudget {
    /// Bound in `V` units.
    pub allowance: i128,
    /// The `events * V_SCALE` component, for the violation report.
    pub events: u64,
    /// The volume-weighted-entry component, for the violation report.
    pub qty_term: u128,
}

impl DustBudget {
    /// Budget for a block that applied `txs` transactions against a ledger
    /// holding `snap`'s positions.
    ///
    /// Every transaction can produce several fills, each fill two realised-PnL
    /// credits; a funding tick or a settlement touches every position row on
    /// its market. `4 * txs + positions + 8` is a deliberately loose bound on
    /// the number of truncating events — the point is to be comfortably above
    /// the true count while staying far below anything a bug would move.
    pub fn for_block(txs: usize, snap: &LedgerSnapshot) -> Self {
        let events = (4 * txs as u64) + snap.open_positions as u64 + 8;
        let qty_term = snap.total_position_qty;
        let allowance = (events as i128)
            .saturating_mul(V_SCALE)
            .saturating_add(qty_term as i128);
        Self {
            allowance,
            events,
            qty_term,
        }
    }

    /// Render the budget for a violation report.
    pub fn describe(&self) -> String {
        format!(
            "{} (= {} truncating events x {} + {} of volume-weighted-entry residue)",
            self.allowance, self.events, V_SCALE, self.qty_term
        )
    }
}

/// Every `(address, AccountState)` row in `CF_ACCOUNTS`, in key order.
pub fn scan_accounts(db: &Database) -> Vec<(Address, cb_state::AccountState)> {
    let mut out = Vec::new();
    let prefix: [u8; 0] = [];
    let it = match db.iter_prefix(CF_ACCOUNTS, &prefix) {
        Ok(it) => it,
        Err(_) => return out,
    };
    for kv in it.flatten() {
        let (k, v) = kv;
        if k.len() != 20 {
            continue;
        }
        let addr = Address(k[..20].try_into().expect("20 bytes"));
        if let Ok(state) = cb_state::AccountState::decode(&v) {
            out.push((addr, state));
        }
    }
    out
}

/// Every `MarginAccount` row, in key order.
pub fn scan_margin_accounts(db: &Database) -> Vec<MarginAccount> {
    let mut out = Vec::new();
    let prefix: [u8; 0] = [];
    let it = match db.iter_prefix(CF_MARGIN_ACCOUNTS, &prefix) {
        Ok(it) => it,
        Err(_) => return out,
    };
    for kv in it.flatten() {
        let (k, v) = kv;
        if k.len() != 20 {
            continue;
        }
        if let Ok(m) = bincode::deserialize::<MarginAccount>(&v) {
            out.push(m);
        }
    }
    out
}

/// Every `Position` row, in `(owner, market)` key order.
pub fn scan_positions(db: &Database) -> Vec<Position> {
    let mut out = Vec::new();
    let prefix: [u8; 0] = [];
    let it = match db.iter_prefix(CF_POSITIONS, &prefix) {
        Ok(it) => it,
        Err(_) => return out,
    };
    for kv in it.flatten() {
        let (k, v) = kv;
        if k.len() != 24 {
            continue;
        }
        if let Ok(p) = bincode::deserialize::<Position>(&v) {
            out.push(p);
        }
    }
    out
}

/// Human label for an `AssetId::as_u8()` key.
///
/// Resolved through `AssetId::from_u8` rather than matched against a fixed
/// list, so an asset added to the universe after this was written still
/// prints its own symbol instead of `UNKNOWN`.
pub fn asset_label(key: u8) -> &'static str {
    AssetId::from_u8(key)
        .map(|a| a.symbol())
        .unwrap_or("UNKNOWN")
}
