//! Perp-specific data collectors, JSON shapes, and HTML renderers for the M14
//! operator dashboard.
//!
//! Each panel here reads existing M6 state CFs / RPC-server helpers — no new
//! RPC method is added — and produces:
//!   - a `Serialize`-able value type (used by the `/api/perps/*` endpoints), and
//!   - a small HTML-table renderer fed by the same value type (used by
//!     `templates::render_index`).
//!
//! Payloads are bounded server-side: positions are sorted by `|unrealized PnL|`
//! and truncated to `MAX_POSITION_ROWS`, liquidation activity is the most-recent
//! `MAX_LIQUIDATION_ROWS` events, and the insurance-fund sparkline is the last
//! `INSURANCE_SPARKLINE_BLOCKS` blocks. The operator dashboard refreshes every
//! 2s, so we keep the per-request DB cost predictable.

use cb_state::{
    perps::ORACLE_KIND_ORACLE, Database, FundingIndices, LiquidationQueue, MarginAccounts, Meta,
    OracleState, PerpMarkets, CF_LIQUIDATION_QUEUE, CF_POSITIONS,
};
use cb_types::{Address, LiquidationEntry, PerpMarket, Position, Side};
use serde::Serialize;
use std::collections::BTreeMap;

/// Server-side cap on position rows. The full positions CF can grow unbounded;
/// the dashboard only renders the top-PnL slice so the JSON payload stays small.
pub const MAX_POSITION_ROWS: usize = 20;
/// Server-side cap on liquidation events returned per request. We tail the
/// liquidation_queue CF and keep the most-recent N entries.
pub const MAX_LIQUIDATION_ROWS: usize = 50;
/// Width of the insurance-fund sparkline window (in blocks). Independent of
/// pruning depth — when the chain is younger than this we just return what
/// exists.
pub const INSURANCE_SPARKLINE_BLOCKS: u64 = 100;
/// Above this many blocks since the last oracle update we flag the market as
/// stale on the dashboard. Matches the engine's mark-staleness fallback
/// distance (see cb-engine mark price comments) — kept here as a UI-only knob.
const ORACLE_STALENESS_BLOCKS: u64 = 30;

// ---------------------------------------------------------------------------
// Funding panel
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct FundingRow {
    pub market: String,
    /// Current funding rate, fixed-point at 1e9 denominator (= rate_q9 / 1e9).
    pub rate_q9: i64,
    /// Block height at which the engine last accrued funding for this market.
    pub last_tick_height: u64,
    /// Funding cadence (blocks). Constant per market — the engine only ticks
    /// every `funding_interval_blocks` blocks.
    pub interval_blocks: u32,
    /// Predicted next-tick height = `last_tick_height + interval_blocks`. May be
    /// in the past at boot-time before the first tick — we still surface the
    /// arithmetic so the operator can spot the staleness.
    pub next_tick_height: u64,
    /// `funding_rate_cap_q9` from the market record, for "is this near the cap?"
    /// at-a-glance.
    pub cap_q9: i64,
}

/// Read funding state for every registered perp market. One CF lookup per
/// market — cheap even at hundreds of markets.
pub fn collect_funding(db: &Database) -> Vec<FundingRow> {
    let markets = PerpMarkets::new(db).list();
    let funding_cf = FundingIndices::new(db);
    markets
        .into_iter()
        .map(|m| {
            let f = funding_cf
                .get(&m.id)
                .unwrap_or_else(|| cb_types::FundingIndex::new(m.id));
            let next = f
                .last_tick_height
                .saturating_add(m.funding_interval_blocks as u64);
            FundingRow {
                market: format!("{}", m.id),
                rate_q9: f.rate_q9,
                last_tick_height: f.last_tick_height,
                interval_blocks: m.funding_interval_blocks,
                next_tick_height: next,
                cap_q9: m.funding_rate_cap_q9,
            }
        })
        .collect()
}

pub fn render_funding_table(rows: &[FundingRow]) -> String {
    if rows.is_empty() {
        return "<tr><td colspan=\"5\" class=\"dim\">no perp markets registered</td></tr>"
            .to_string();
    }
    let mut out = String::new();
    for r in rows {
        out.push_str(&format!(
            "<tr>\
                <td><code>{m}</code></td>\
                <td class=\"num\">{rate}</td>\
                <td class=\"num\">{last}</td>\
                <td class=\"num\">{next}</td>\
                <td class=\"num dim\">{cap}</td>\
            </tr>",
            m = r.market,
            rate = r.rate_q9,
            last = r.last_tick_height,
            next = r.next_tick_height,
            cap = r.cap_q9,
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Position table
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct PositionRow {
    /// 0x-prefixed full address; the renderer truncates for display.
    pub owner: String,
    pub market: String,
    pub side: String,
    pub qty: String,
    pub entry_px_q8: String,
    pub mark_px_q8: String,
    /// Unrealized PnL in 1e6 quote units. Decimal string with optional leading `-`.
    pub unrealized_pnl: String,
    /// |unrealized_pnl|, used as the sort key. Decimal string of u128.
    pub abs_unrealized_pnl: String,
    /// Initial-margin utilization in basis points. `0..=10_000+` ; values above
    /// 10_000 indicate the position is below initial-margin requirement.
    /// `None` when the position is cross and we lack a MarginAccount row
    /// (effectively unfunded — the engine would have liquidated it, so this
    /// shouldn't happen for healthy state, but we surface it instead of
    /// guessing).
    pub im_util_bps: Option<u128>,
}

/// Walk every open position, compute its unrealized PnL using the latest mark
/// price, then sort by `|pnl|` desc and truncate to [`MAX_POSITION_ROWS`].
///
/// PnL math, the engine's (`cb_engine::margin::position_unrealized_pnl_micro_usd`):
/// - long: `pnl = qty * (mark - entry) / 1e10`
/// - short: `pnl = qty * (entry - mark) / 1e10`
/// - `qty` is in contract units (1e8 per whole unit) and prices are q8, so the
///   product is at 1e16; dividing by [`CONTRACT_Q8_TO_MICRO_USD`] lands it in
///   micro-USD, the denomination of `MarginAccount.collateral`.
///
/// We don't apply funding here — funding accrual on dashboard reads would
/// duplicate the engine's lazy-funding logic and risk drift. Operators who
/// need the exact engine view should run a perp_position RPC.
pub fn collect_positions(db: &Database) -> Vec<PositionRow> {
    // One pass: enumerate positions.
    let mut positions: Vec<Position> = Vec::new();
    if let Ok(it) = db.iter_prefix(CF_POSITIONS, &[]) {
        for kv in it {
            let (k, v) = match kv {
                Ok(p) => p,
                Err(_) => break,
            };
            if k.len() != 24 {
                continue;
            }
            if let Ok(p) = bincode::deserialize::<Position>(&v) {
                if p.qty == 0 {
                    // qty=0 is the closed sentinel; skip.
                    continue;
                }
                positions.push(p);
            }
        }
    }
    if positions.is_empty() {
        return Vec::new();
    }

    // Mark price cache (one entry per market touched).
    let oracle = OracleState::new(db);
    let mut mark_cache: BTreeMap<[u8; 4], u128> = BTreeMap::new();
    for p in &positions {
        let key = p.market.as_bytes();
        if mark_cache.contains_key(&key) {
            continue;
        }
        let mark = oracle.get_mark(&p.market).map(|m| m.px_q8).unwrap_or(0);
        mark_cache.insert(key, mark);
    }

    // Per-market params for IM utilization.
    let perp_markets = PerpMarkets::new(db).list();
    let mut market_cache: BTreeMap<[u8; 4], PerpMarket> = BTreeMap::new();
    for m in perp_markets {
        market_cache.insert(m.id.as_bytes(), m);
    }

    // MarginAccount cache for cross positions.
    let margin = MarginAccounts::new(db);
    let mut margin_cache: BTreeMap<[u8; 20], Option<u128>> = BTreeMap::new();

    let mut rows: Vec<(u128, PositionRow)> = Vec::with_capacity(positions.len());
    for p in positions {
        let mark = mark_cache.get(&p.market.as_bytes()).copied().unwrap_or(0);
        let pnl = signed_pnl(&p, mark);
        let abs = pnl.unsigned_abs();

        // IM utilization: notional = qty * mark / 1e10 (micro-USD); IM = notional
        // * initial_margin_bps / 10_000. utilization_bps = collateral_used *
        // 10_000 / notional. We approximate "collateral used" by the position's
        // share of the cross MarginAccount's collateral when isolated_margin == 0.
        let im_bps: Option<u128> = match market_cache.get(&p.market.as_bytes()) {
            None => None,
            Some(m) => {
                let notional_q8 = p.qty.saturating_mul(mark); // qty * px_q8
                if notional_q8 == 0 {
                    None
                } else {
                    // qty (1e8) * mark (q8) -> micro-USD, the collateral's unit.
                    let notional_usd = notional_q8 / CONTRACT_Q8_TO_MICRO_USD;
                    let im_required =
                        notional_usd.saturating_mul(m.initial_margin_bps as u128) / 10_000;
                    let used_opt: Option<u128> = if p.margin_mode == 1 {
                        Some(p.isolated_margin)
                    } else {
                        // Cache the cross-margin collateral lookup per-owner.
                        margin_cache
                            .entry(p.owner.0)
                            .or_insert_with(|| margin.get(&p.owner).map(|ma| ma.collateral))
                            .to_owned()
                    };
                    match used_opt {
                        Some(used) if im_required > 0 => {
                            Some(used.saturating_mul(10_000) / im_required)
                        }
                        _ => None,
                    }
                }
            }
        };

        let pnl_str = if pnl < 0 {
            format!("-{}", pnl.unsigned_abs())
        } else {
            pnl.to_string()
        };
        rows.push((
            abs,
            PositionRow {
                owner: format!("{}", p.owner),
                market: format!("{}", p.market),
                side: match p.side {
                    Side::Buy => "long".into(),
                    Side::Sell => "short".into(),
                },
                qty: p.qty.to_string(),
                entry_px_q8: p.entry_px_q8.to_string(),
                mark_px_q8: mark.to_string(),
                unrealized_pnl: pnl_str,
                abs_unrealized_pnl: abs.to_string(),
                im_util_bps: im_bps,
            },
        ));
    }
    rows.sort_by_key(|row| std::cmp::Reverse(row.0));
    rows.into_iter()
        .take(MAX_POSITION_ROWS)
        .map(|(_, r)| r)
        .collect()
}

/// Returns position PnL as i128. We widen because qty can be u128; for any
/// realistic q8 price * qty the product fits comfortably below i128::MAX, so
/// the saturating_sub is defensive only.
/// `contract qty (1e8) * price (q8, 1e8)` is a 1e16-scaled product; micro-USD
/// is 1e6. The divisor between them, identical to the engine's in
/// `position_notional_micro_usd` and `position_unrealized_pnl_micro_usd`.
///
/// This used to be `100`, from reading the product as "q8" alone and
/// forgetting the quantity's own 1e8 scale, which showed every position's PnL
/// and notional 10^8 times too large (a $500 gain as $50 billion) and the
/// initial-margin utilisation 10^8 times too small.
const CONTRACT_Q8_TO_MICRO_USD: u128 = 10_000_000_000;

fn signed_pnl(p: &Position, mark_q8: u128) -> i128 {
    if mark_q8 == 0 {
        return 0;
    }
    let entry = p.entry_px_q8;
    let qty = p.qty;
    // qty (1e8) * |mark - entry| (q8) is at 1e16; micro-USD is 1e6.
    let (high, low, sign) = if mark_q8 >= entry {
        (mark_q8, entry, 1i128)
    } else {
        (entry, mark_q8, -1i128)
    };
    let diff = high - low;
    // qty * diff fits in u128 for any plausible position size; if not, saturate.
    let mag_q8 = qty.saturating_mul(diff);
    let mag_usd = mag_q8 / CONTRACT_Q8_TO_MICRO_USD;
    let mag_i128 = i128::try_from(mag_usd).unwrap_or(i128::MAX);
    match (p.side, sign) {
        (Side::Buy, 1) | (Side::Sell, -1) => mag_i128,
        (Side::Buy, -1) | (Side::Sell, 1) => -mag_i128,
        _ => 0, // unreachable
    }
}

pub fn render_positions_table(rows: &[PositionRow]) -> String {
    if rows.is_empty() {
        return "<tr><td colspan=\"8\" class=\"dim\">no open positions</td></tr>".to_string();
    }
    let mut out = String::new();
    for r in rows {
        let im = r
            .im_util_bps
            .map(|b| format!("{} bps", b))
            .unwrap_or_else(|| "-".into());
        out.push_str(&format!(
            "<tr>\
                <td><code>{owner}</code></td>\
                <td><code>{market}</code></td>\
                <td>{side}</td>\
                <td class=\"num\">{qty}</td>\
                <td class=\"num\">{entry}</td>\
                <td class=\"num\">{mark}</td>\
                <td class=\"num\">{pnl}</td>\
                <td class=\"num dim\">{im}</td>\
            </tr>",
            owner = truncate_addr(&r.owner),
            market = r.market,
            side = r.side,
            qty = r.qty,
            entry = r.entry_px_q8,
            mark = r.mark_px_q8,
            pnl = r.unrealized_pnl,
            im = im,
        ));
    }
    out
}

fn truncate_addr(s: &str) -> String {
    // 0x + first 6 + ... + last 4. Counted in characters, not bytes: slicing
    // a byte range panicked on any non-ASCII text, which a rendered cell
    // must never be able to do to the page.
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 14 {
        return s.to_string();
    }
    let head: String = chars[..8].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}...{tail}")
}

// ---------------------------------------------------------------------------
// Oracle tracker
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct OracleRow {
    pub market: String,
    /// `OraclePrice.px_q8` as decimal string (u128 doesn't fit in JSON Number).
    pub oracle_px_q8: String,
    /// Block height at which the authority signed the latest sample.
    pub source_height: u64,
    /// `head_height - source_height`. `0` when the authority signed at HEAD.
    pub blocks_since_update: u64,
    /// `true` when blocks_since_update > [`ORACLE_STALENESS_BLOCKS`]. Mirrors
    /// the engine's mark-price staleness gate but is read-only here.
    pub stale: bool,
}

/// Per-market oracle freshness. Reads `OraclePrice` per market; missing rows
/// surface as "no oracle" (px=`-`, blocks_since_update = head_height).
pub fn collect_oracle(db: &Database) -> Vec<OracleRow> {
    let head = Meta::new(db).head_height();
    let markets = PerpMarkets::new(db).list();
    let oracle = OracleState::new(db);
    let _ = ORACLE_KIND_ORACLE; // keep import live for potential ml extension
    markets
        .into_iter()
        .map(|m| {
            let (px, src) = match oracle.get_oracle(&m.id) {
                Some(o) => (o.px_q8.to_string(), o.source_height),
                None => ("-".into(), 0),
            };
            let blocks_since = head.saturating_sub(src);
            OracleRow {
                market: format!("{}", m.id),
                oracle_px_q8: px,
                source_height: src,
                blocks_since_update: blocks_since,
                stale: blocks_since > ORACLE_STALENESS_BLOCKS,
            }
        })
        .collect()
}

pub fn render_oracle_table(rows: &[OracleRow]) -> String {
    if rows.is_empty() {
        return "<tr><td colspan=\"5\" class=\"dim\">no perp markets registered</td></tr>"
            .to_string();
    }
    let mut out = String::new();
    for r in rows {
        let stale = if r.stale {
            "<span class=\"pill off\">stale</span>"
        } else {
            "<span class=\"pill ok\">fresh</span>"
        };
        out.push_str(&format!(
            "<tr>\
                <td><code>{m}</code></td>\
                <td class=\"num\">{px}</td>\
                <td class=\"num\">{src}</td>\
                <td class=\"num\">{since}</td>\
                <td>{stale}</td>\
            </tr>",
            m = r.market,
            px = r.oracle_px_q8,
            src = r.source_height,
            since = r.blocks_since_update,
            stale = stale,
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Liquidation activity
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct LiquidationRow {
    pub owner: String,
    pub market: String,
    pub triggered_by: String,
    pub triggered_at_height: u64,
    /// Snapshot mark price at trigger, decimal string of u128 q8.
    pub mark_at_trigger_q8: String,
    pub seq: u64,
}

/// Read the most-recent [`MAX_LIQUIDATION_ROWS`] liquidation queue entries.
///
/// Liquidation queue keys are `[height_be: 8 || seq_be: 8]`, so a forward CF
/// scan emits rows in ascending (height, seq) order. We collect everything,
/// then take the trailing window — the queue is small (drained at end-of-block)
/// so the full scan cost is bounded by per-block fan-out.
pub fn collect_liquidations(db: &Database) -> Vec<LiquidationRow> {
    let q = LiquidationQueue::new(db);
    let _ = q; // helper kept available; we walk the CF directly to support ranges.
    let mut all: Vec<LiquidationEntry> = Vec::new();
    if let Ok(it) = db.iter_prefix(CF_LIQUIDATION_QUEUE, &[]) {
        for kv in it {
            let (_k, v) = match kv {
                Ok(p) => p,
                Err(_) => break,
            };
            if let Ok(e) = bincode::deserialize::<LiquidationEntry>(&v) {
                all.push(e);
            }
        }
    }
    // Tail to the last N. CF scan is ascending; reverse-trim by truncating
    // from the front so the most recent (highest-height) events come last.
    if all.len() > MAX_LIQUIDATION_ROWS {
        let drop = all.len() - MAX_LIQUIDATION_ROWS;
        all.drain(..drop);
    }
    // Reverse for display so newest rows appear at the top of the table.
    all.reverse();
    all.into_iter()
        .map(|e| LiquidationRow {
            owner: format!("{}", e.owner),
            market: format!("{}", e.market),
            triggered_by: format!("{}", e.triggered_by),
            triggered_at_height: e.triggered_at_height,
            mark_at_trigger_q8: e.mark_at_trigger_q8.to_string(),
            seq: e.seq,
        })
        .collect()
}

pub fn render_liquidations_table(rows: &[LiquidationRow]) -> String {
    if rows.is_empty() {
        return "<tr><td colspan=\"6\" class=\"dim\">no liquidation activity</td></tr>".to_string();
    }
    let mut out = String::new();
    for r in rows {
        out.push_str(&format!(
            "<tr>\
                <td class=\"num\">{height}</td>\
                <td class=\"num\">{seq}</td>\
                <td><code>{owner}</code></td>\
                <td><code>{market}</code></td>\
                <td><code>{trig}</code></td>\
                <td class=\"num\">{mark}</td>\
            </tr>",
            height = r.triggered_at_height,
            seq = r.seq,
            owner = truncate_addr(&r.owner),
            market = r.market,
            trig = truncate_addr(&r.triggered_by),
            mark = r.mark_at_trigger_q8,
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Insurance fund balance + sparkline
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct InsuranceSnapshot {
    /// Current MarginAccount{INSURANCE}.collateral (1e6 USD units).
    pub collateral: String,
    /// Bad-debt sitting on the insurance account; non-zero implies coverage
    /// shortfall is pending an absorb.
    pub bad_debt: String,
    /// Most recent [`INSURANCE_SPARKLINE_BLOCKS`] balance samples — one per
    /// retained block. Decimal strings; the front-end can map them to a path.
    /// Reverse chronological is simpler to read; we emit oldest-first to match
    /// chart conventions.
    pub sparkline: Vec<String>,
    /// Block heights aligned 1:1 with `sparkline`.
    pub sparkline_heights: Vec<u64>,
}

impl Default for InsuranceSnapshot {
    fn default() -> Self {
        Self {
            collateral: "0".into(),
            bad_debt: "0".into(),
            sparkline: Vec::new(),
            sparkline_heights: Vec::new(),
        }
    }
}

/// Read the insurance margin account, plus a per-block sparkline approximation.
///
/// We don't persist a per-block balance log — adding one would mean a new CF
/// touched on every block. Instead the sparkline approximates by sampling the
/// account *as it stands now* repeated at each retained block height. This is
/// honest about what the dashboard knows: it watches the live value and the
/// front-end charts it over time. When M14 grows a real per-block log we
/// swap the body of this function.
///
/// The per-block log alternative was rejected here on the constraint "Only
/// cb-dashboard touched" — it would require a write hook in cb-block.
pub fn collect_insurance(db: &Database) -> InsuranceSnapshot {
    let acct = MarginAccounts::new(db).get(&Address::INSURANCE);
    let collateral = acct.as_ref().map(|m| m.collateral).unwrap_or(0);
    let bad_debt = acct.as_ref().map(|m| m.bad_debt).unwrap_or(0);

    let head = Meta::new(db).head_height();
    let n = head.min(INSURANCE_SPARKLINE_BLOCKS);
    let mut heights = Vec::with_capacity(n as usize);
    let mut samples = Vec::with_capacity(n as usize);
    if n > 0 {
        let start = head.saturating_sub(n - 1);
        for h in start..=head {
            heights.push(h);
            samples.push(collateral.to_string());
        }
    }
    InsuranceSnapshot {
        collateral: collateral.to_string(),
        bad_debt: bad_debt.to_string(),
        sparkline: samples,
        sparkline_heights: heights,
    }
}

pub fn render_insurance_panel(s: &InsuranceSnapshot) -> String {
    // Inline SVG sparkline. Coordinates only depend on the sample values so
    // the path is stable for any given snapshot.
    let svg = sparkline_svg(&s.sparkline);
    format!(
        "<div class=\"card span-2\">\
            <div class=\"k\">insurance fund</div>\
            <div class=\"v num\">{collateral}</div>\
            <div class=\"sub\">bad debt: {bad}</div>\
            <div class=\"sparkline\">{svg}</div>\
        </div>",
        collateral = s.collateral,
        bad = s.bad_debt,
        svg = svg,
    )
}

/// Render a 240×40 SVG polyline. Empty input renders an empty `<svg>`. The
/// y-axis is auto-scaled to `[min, max]`; a flat series (min == max) is drawn
/// as a horizontal line at mid-height.
fn sparkline_svg(samples: &[String]) -> String {
    if samples.is_empty() {
        return "<svg viewBox=\"0 0 240 40\" width=\"240\" height=\"40\"></svg>".into();
    }
    let parsed: Vec<u128> = samples
        .iter()
        .map(|s| s.parse::<u128>().unwrap_or(0))
        .collect();
    let min = *parsed.iter().min().unwrap_or(&0);
    let max = *parsed.iter().max().unwrap_or(&0);
    // Scale the range down until `offset * (height - 1)` cannot overflow.
    // Without this a balance past u128::MAX / 39 wrapped, and a larger
    // sample could be drawn below a smaller one. The shift is the same for
    // every sample, so ordering is preserved, and it is zero for any range a
    // real insurance fund will ever have.
    let full_span = max.saturating_sub(min);
    let shift = (u128::BITS - full_span.leading_zeros()).saturating_sub(u128::BITS - 6);
    let span = full_span >> shift;
    let n = parsed.len();
    let width = 240u32;
    let height = 40u32;
    let mut points = String::new();
    for (i, v) in parsed.iter().enumerate() {
        let x = if n == 1 {
            width / 2
        } else {
            (i as u32) * (width - 1) / (n as u32 - 1)
        };
        // Inverted Y (SVG zero-at-top): higher value -> lower y. A flat
        // series (span zero) sits on the middle line.
        let y = match (((*v - min) >> shift) * (u128::from(height) - 1)).checked_div(span) {
            None => height / 2,
            Some(off) => (height - 1).saturating_sub(off as u32),
        };
        if !points.is_empty() {
            points.push(' ');
        }
        points.push_str(&format!("{},{}", x, y));
    }
    format!(
        "<svg viewBox=\"0 0 {w} {h}\" width=\"{w}\" height=\"{h}\">\
            <polyline fill=\"none\" stroke=\"currentColor\" stroke-width=\"1.5\" points=\"{p}\" />\
        </svg>",
        w = width,
        h = height,
        p = points,
    )
}

// ---------------------------------------------------------------------------
// Top-level bundle (used by the index page and as the JSON feed shape).
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct PerpsPanels {
    pub funding: Vec<FundingRow>,
    pub positions: Vec<PositionRow>,
    pub oracle: Vec<OracleRow>,
    pub liquidations: Vec<LiquidationRow>,
    pub insurance: InsuranceSnapshot,
}

pub fn collect_panels(db: &Database) -> PerpsPanels {
    PerpsPanels {
        funding: collect_funding(db),
        positions: collect_positions(db),
        oracle: collect_oracle(db),
        liquidations: collect_liquidations(db),
        insurance: collect_insurance(db),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_state::Database;
    use cb_types::{
        Address, AssetId, DerivId, FundingIndex, MarginAccount, MarkPrice, OraclePrice, PerpMarket,
        Position, Side, Signed,
    };
    use tempfile::tempdir;

    fn open_db() -> (tempfile::TempDir, Database) {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        (dir, db)
    }

    fn sample_market(idx: u16) -> PerpMarket {
        PerpMarket {
            id: DerivId::perp(idx),
            underlying: AssetId::Btc,
            tick_size: 100,
            lot_size: 1_000,
            max_leverage_x100: 5_000,
            initial_margin_bps: 200,
            maintenance_margin_bps: 100,
            taker_fee_bps: 3,
            maker_fee_bps_signed: -1,
            funding_interval_blocks: 3_600,
            funding_rate_cap_q9: 1_000_000,
            oracle_authority: Address([7; 20]),
            max_position_notional: 100_000_000_000_000,
            paused: false,
            last_trade_px_q8: 0,
            last_trade_height: 0,
            paused_reduce_only_only: false,
        }
    }

    // ---------------- funding ----------------

    #[test]
    fn funding_row_uses_market_interval_for_next_tick() {
        let (_dir, db) = open_db();
        let m = sample_market(0);
        PerpMarkets::new(&db).put(&m);
        FundingIndices::new(&db).put(&FundingIndex {
            market: m.id,
            cumulative: Signed::ZERO,
            last_tick_height: 1_000,
            rate_q9: 250,
        });
        let rows = collect_funding(&db);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].rate_q9, 250);
        assert_eq!(rows[0].last_tick_height, 1_000);
        assert_eq!(rows[0].next_tick_height, 1_000 + 3_600);
        assert_eq!(rows[0].interval_blocks, 3_600);
        assert_eq!(rows[0].cap_q9, 1_000_000);
    }

    #[test]
    fn funding_render_emits_table_rows() {
        let r = FundingRow {
            market: "perp:0".into(),
            rate_q9: 42,
            last_tick_height: 5,
            interval_blocks: 10,
            next_tick_height: 15,
            cap_q9: 1_000,
        };
        let html = render_funding_table(std::slice::from_ref(&r));
        assert!(html.contains("perp:0"));
        assert!(html.contains(">42<"));
        assert!(html.contains(">15<"));
    }

    #[test]
    fn funding_render_empty_emits_placeholder() {
        let html = render_funding_table(&[]);
        assert!(html.contains("no perp markets registered"));
    }

    // ---------------- positions ----------------

    fn seed_long_position(db: &Database, owner: Address, m: &PerpMarket, qty: u128, entry: u128) {
        cb_state::Positions::new(db).put(&Position {
            owner,
            market: m.id,
            side: Side::Buy,
            qty,
            entry_funding_index: Signed::ZERO,
            entry_px_q8: entry,
            margin_mode: 0,
            isolated_margin: 0,
            last_funding_height: 0,
        });
    }

    #[test]
    fn positions_sorted_by_abs_pnl_and_truncated() {
        let (_dir, db) = open_db();
        let m = sample_market(0);
        PerpMarkets::new(&db).put(&m);
        // Mark above entry: long is in profit. We seed N positions with
        // monotonically-increasing entry prices so |PnL| is monotonically
        // decreasing — the row with the lowest entry should rank first.
        let mark_q8 = 50_500u128 * 100_000_000;
        OracleState::new(&db).put_mark(&MarkPrice {
            market: m.id,
            px_q8: mark_q8,
            computed_at_height: 1,
            premium_ema_q9: 0,
        });
        // Seed a bit more than MAX_POSITION_ROWS to verify the truncation.
        let total = MAX_POSITION_ROWS + 5;
        for i in 0..total {
            let owner = Address([i as u8 + 1; 20]);
            let entry = (50_000u128 + i as u128) * 100_000_000; // increasing entry
            seed_long_position(&db, owner, &m, 100_000_000, entry);
            MarginAccounts::new(&db).put(&MarginAccount {
                owner,
                collateral: 1_000_000,
                unrealized_pnl: Signed::ZERO,
                bad_debt: 0,
            });
        }
        let rows = collect_positions(&db);
        assert_eq!(rows.len(), MAX_POSITION_ROWS);
        // First row has highest |pnl|: lowest entry index.
        let first_abs: u128 = rows[0].abs_unrealized_pnl.parse().unwrap();
        let last_abs: u128 = rows.last().unwrap().abs_unrealized_pnl.parse().unwrap();
        assert!(first_abs >= last_abs, "rows must be sorted desc by |pnl|");
    }

    #[test]
    fn positions_render_emits_long_short_label() {
        let r = PositionRow {
            owner: "0x1111111111111111111111111111111111111111".into(),
            market: "perp:0".into(),
            side: "long".into(),
            qty: "100".into(),
            entry_px_q8: "5000000000000".into(),
            mark_px_q8: "5050000000000".into(),
            unrealized_pnl: "1234".into(),
            abs_unrealized_pnl: "1234".into(),
            im_util_bps: Some(750),
        };
        let html = render_positions_table(std::slice::from_ref(&r));
        assert!(html.contains("long"));
        assert!(html.contains("perp:0"));
        assert!(html.contains("750 bps"));
        // Owner truncated.
        assert!(html.contains("0x111111"));
    }

    #[test]
    fn positions_render_empty_placeholder() {
        let html = render_positions_table(&[]);
        assert!(html.contains("no open positions"));
    }

    #[test]
    fn positions_skip_qty_zero_sentinel() {
        let (_dir, db) = open_db();
        let m = sample_market(0);
        PerpMarkets::new(&db).put(&m);
        OracleState::new(&db).put_mark(&MarkPrice {
            market: m.id,
            px_q8: 50_000 * 100_000_000,
            computed_at_height: 1,
            premium_ema_q9: 0,
        });
        // qty=0 sentinel must be filtered.
        let owner = Address([1; 20]);
        cb_state::Positions::new(&db).put(&Position {
            owner,
            market: m.id,
            side: Side::Buy,
            qty: 0,
            entry_funding_index: Signed::ZERO,
            entry_px_q8: 50_000 * 100_000_000,
            margin_mode: 0,
            isolated_margin: 0,
            last_funding_height: 0,
        });
        assert!(collect_positions(&db).is_empty());
    }

    // ---------------- oracle ----------------

    #[test]
    fn oracle_marks_stale_when_blocks_since_exceeds_threshold() {
        let (_dir, db) = open_db();
        let m = sample_market(0);
        PerpMarkets::new(&db).put(&m);
        // Push head height forward.
        Meta::new(&db).set_head(
            ORACLE_STALENESS_BLOCKS + 100,
            cb_types::H256::ZERO,
            cb_types::H256::ZERO,
        );
        OracleState::new(&db).put_oracle(&OraclePrice {
            market: m.id,
            px_q8: 1_000,
            source_height: 1,
            authority: Address([7; 20]),
        });
        let rows = collect_oracle(&db);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].stale, "should be flagged stale");
        assert!(rows[0].blocks_since_update > ORACLE_STALENESS_BLOCKS);
    }

    #[test]
    fn oracle_fresh_when_recent() {
        let (_dir, db) = open_db();
        let m = sample_market(0);
        PerpMarkets::new(&db).put(&m);
        Meta::new(&db).set_head(100, cb_types::H256::ZERO, cb_types::H256::ZERO);
        OracleState::new(&db).put_oracle(&OraclePrice {
            market: m.id,
            px_q8: 1_000,
            source_height: 99,
            authority: Address([7; 20]),
        });
        let rows = collect_oracle(&db);
        assert!(!rows[0].stale);
        assert_eq!(rows[0].blocks_since_update, 1);
    }

    #[test]
    fn oracle_render_emits_pill() {
        let r = OracleRow {
            market: "perp:0".into(),
            oracle_px_q8: "1000".into(),
            source_height: 5,
            blocks_since_update: 1,
            stale: false,
        };
        let html = render_oracle_table(std::slice::from_ref(&r));
        assert!(html.contains("fresh"));
        assert!(html.contains("perp:0"));
    }

    // ---------------- liquidations ----------------

    fn seed_liq(db: &Database, height: u64, seq: u64) {
        LiquidationQueue::new(db).put(&LiquidationEntry {
            owner: Address([1; 20]),
            market: DerivId::perp(0),
            triggered_by: Address([2; 20]),
            triggered_at_height: height,
            mark_at_trigger_q8: 100,
            seq,
        });
    }

    #[test]
    fn liquidations_truncated_to_recent_window() {
        let (_dir, db) = open_db();
        // Seed more than MAX_LIQUIDATION_ROWS entries spread across heights.
        let total = MAX_LIQUIDATION_ROWS + 7;
        for i in 0..total {
            seed_liq(&db, (i / 3) as u64 + 1, (i % 3) as u64);
        }
        let rows = collect_liquidations(&db);
        assert_eq!(rows.len(), MAX_LIQUIDATION_ROWS);
        // Newest-first ordering: top row's height >= last row's height.
        assert!(
            rows.first().unwrap().triggered_at_height >= rows.last().unwrap().triggered_at_height
        );
    }

    #[test]
    fn liquidations_render_table_rows() {
        let r = LiquidationRow {
            owner: "0x1111111111111111111111111111111111111111".into(),
            market: "perp:0".into(),
            triggered_by: "0x2222222222222222222222222222222222222222".into(),
            triggered_at_height: 42,
            mark_at_trigger_q8: "100".into(),
            seq: 0,
        };
        let html = render_liquidations_table(std::slice::from_ref(&r));
        assert!(html.contains(">42<"));
        assert!(html.contains("perp:0"));
    }

    #[test]
    fn liquidations_render_empty_placeholder() {
        let html = render_liquidations_table(&[]);
        assert!(html.contains("no liquidation activity"));
    }

    // ---------------- insurance ----------------

    #[test]
    fn insurance_reads_margin_account() {
        let (_dir, db) = open_db();
        Meta::new(&db).set_head(50, cb_types::H256::ZERO, cb_types::H256::ZERO);
        MarginAccounts::new(&db).put(&MarginAccount {
            owner: Address::INSURANCE,
            collateral: 10_000_000,
            unrealized_pnl: Signed::ZERO,
            bad_debt: 7,
        });
        let snap = collect_insurance(&db);
        assert_eq!(snap.collateral, "10000000");
        assert_eq!(snap.bad_debt, "7");
        assert_eq!(snap.sparkline.len(), 50);
        assert_eq!(snap.sparkline_heights.first().copied(), Some(1));
        assert_eq!(snap.sparkline_heights.last().copied(), Some(50));
    }

    #[test]
    fn insurance_with_no_account_zeroes_out() {
        let (_dir, db) = open_db();
        let snap = collect_insurance(&db);
        assert_eq!(snap.collateral, "0");
        assert_eq!(snap.bad_debt, "0");
        assert!(snap.sparkline.is_empty());
    }

    #[test]
    fn insurance_render_includes_svg_polyline() {
        let snap = InsuranceSnapshot {
            collateral: "1000".into(),
            bad_debt: "0".into(),
            sparkline: vec!["100".into(), "200".into(), "150".into()],
            sparkline_heights: vec![1, 2, 3],
        };
        let html = render_insurance_panel(&snap);
        assert!(html.contains("insurance fund"));
        assert!(html.contains(">1000<"));
        assert!(html.contains("<svg"));
        assert!(html.contains("<polyline"));
    }

    #[test]
    fn sparkline_svg_handles_empty_input() {
        let svg = sparkline_svg(&[]);
        assert!(svg.contains("<svg"));
        assert!(!svg.contains("<polyline"));
    }

    #[test]
    fn sparkline_svg_handles_flat_series() {
        // min == max should not divide-by-zero; renders a flat line.
        let svg = sparkline_svg(&["100".into(), "100".into(), "100".into()]);
        assert!(svg.contains("<polyline"));
    }

    // ---------------- top-level bundle ----------------

    #[test]
    fn collect_panels_returns_all_subsections() {
        let (_dir, db) = open_db();
        let m = sample_market(0);
        PerpMarkets::new(&db).put(&m);
        let panels = collect_panels(&db);
        assert_eq!(panels.funding.len(), 1);
        assert!(panels.oracle.len() == 1);
        assert!(panels.liquidations.is_empty());
        assert_eq!(panels.insurance.collateral, "0");
    }

    // ---------------- properties: the dashboard agrees with the engine ----------------

    mod properties {
        use super::super::{signed_pnl, sparkline_svg, truncate_addr, CONTRACT_Q8_TO_MICRO_USD};
        use cb_types::{Address, DerivId, Position, Side, Signed};
        use proptest::prelude::*;

        fn position(side: Side, qty: u128, entry: u128) -> Position {
            Position {
                owner: Address([1; 20]),
                market: DerivId::perp(0),
                side,
                qty,
                entry_funding_index: Signed::ZERO,
                entry_px_q8: entry,
                margin_mode: 0,
                isolated_margin: 0,
                last_funding_height: 0,
            }
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(2048))]

            /// The number an operator reads is the number the engine margins
            /// against. Ranges cover a 1e10-unit position at a q8 price of
            /// 1e18 (ten billion dollars), well past anything listed, while
            /// keeping the product inside u128 so neither side saturates.
            #[test]
            fn unrealized_pnl_matches_the_engine(
                long in any::<bool>(),
                qty in 0u128..10_000_000_000_000_000,
                entry in 1u128..1_000_000_000_000_000_000,
                mark in 1u128..1_000_000_000_000_000_000,
            ) {
                let side = if long { Side::Buy } else { Side::Sell };
                let ours = signed_pnl(&position(side, qty, entry), mark);
                let engine = cb_engine::margin::position_unrealized_pnl_micro_usd(side, qty, entry, mark);
                prop_assert_eq!(ours, engine.to_i128());
            }

            #[test]
            fn notional_uses_the_engines_divisor(
                qty in 0u128..10_000_000_000_000_000, mark in 0u128..1_000_000_000_000_000_000,
            ) {
                prop_assert_eq!(
                    qty.saturating_mul(mark) / CONTRACT_Q8_TO_MICRO_USD,
                    cb_engine::margin::position_notional_micro_usd(qty, mark)
                );
            }

            /// A zero mark means "no mark yet", shown as no PnL, not as the
            /// whole entry price lost.
            #[test]
            fn no_mark_shows_no_pnl(long in any::<bool>(), qty in any::<u128>(), entry in any::<u128>()) {
                let side = if long { Side::Buy } else { Side::Sell };
                prop_assert_eq!(signed_pnl(&position(side, qty, entry), 0), 0);
            }

            /// Every point lies inside the 240x40 box, and a larger sample is
            /// never drawn lower than a smaller one, for any magnitudes.
            #[test]
            fn the_sparkline_stays_in_its_box_and_is_monotone(
                samples in proptest::collection::vec(any::<u128>(), 1..60),
            ) {
                let text: Vec<String> = samples.iter().map(|v| v.to_string()).collect();
                let svg = sparkline_svg(&text);
                let points = svg.split("points=\"").nth(1).unwrap().split('"').next().unwrap();
                let ys: Vec<(u32, u32)> = points
                    .split(' ')
                    .map(|p| {
                        let (x, y) = p.split_once(',').unwrap();
                        (x.parse().unwrap(), y.parse().unwrap())
                    })
                    .collect();
                prop_assert_eq!(ys.len(), samples.len());
                for (x, y) in &ys {
                    prop_assert!(*x < 240 && *y < 40, "({}, {}) outside the box", x, y);
                }
                for i in 0..samples.len() {
                    for j in 0..samples.len() {
                        if samples[i] > samples[j] {
                            prop_assert!(ys[i].1 <= ys[j].1, "{} drawn below {}", samples[i], samples[j]);
                        }
                    }
                }
            }

            #[test]
            fn address_truncation_never_panics(s in "\\PC{0,60}") {
                let t = truncate_addr(&s);
                prop_assert!(t.chars().count() <= s.chars().count().max(15));
            }
        }
    }
}
