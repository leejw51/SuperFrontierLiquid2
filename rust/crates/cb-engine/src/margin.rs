//! Margin math (M4): equity, piecewise IM/MM, and the pre-trade gate.
//!
//! Pure: every function in this module is a deterministic function of its
//! inputs. No I/O, no state mutation, no time. The `cb-block` executor wraps
//! these helpers around its CF reads/writes.
//!
//! ## Fixed-point conventions
//!
//! - **Prices** (`mark_q8`, `entry_px_q8`): quote-per-base, scaled `1e8`.
//! - **Quantity** (`qty`): contract base units (e.g. satoshis for BtcPerp).
//! - **Notional / collateral** (`u128`): micro-USD, scaled `1e6` (matches
//!   `AssetId::Usd.decimals() == 6`).
//!
//! Therefore `notional_micro_usd = qty * mark_q8 / 1e10` because
//! `qty (1e8) * mark (1e8) / 1e10 = (1e6)` lands us in micro-USD.
//!
//! ## The `1e10` denominator is NOT a per-asset constant
//!
//! This is worth being precise about, because the natural reading is wrong and
//! an earlier version of this comment got it wrong too.
//!
//! `qty` on a derivative is in **contract units**, which are a fixed
//! `10^cb_types::CONTRACT_QTY_DECIMALS` (= 1e8) per whole unit of the
//! underlying, *for every underlying*. It is not the underlying's spot
//! `AssetId::decimals()`. So the denominator decomposes as
//! `1e10 = 1e8 (the q8 price scale) * 1e2 (contract 1e8 -> USD 1e6)`, and
//! neither factor depends on which asset is being traded.
//!
//! Concretely: one AAPL share of AAPL-PERP exposure is `100_000_000` contract
//! units even though one AAPL share is `1_000_000` *spot* units, because AAPL
//! has 6 decimals. The same `1e10` gives the right micro-USD notional for BTC
//! (8 spot decimals), SOL (9), AAPL (6) and ETH (18) alike —
//! `notional_is_independent_of_the_underlyings_spot_decimals` pins exactly that.
//!
//! **Do not "fix" this into a lookup against the per-asset decimals table.**
//! Doing so would double-count: the asset's decimals already enter the system
//! through the spot price scale (`cb_types::scales::spot_price_scale_exp`), not
//! through the derivative notional. It would also change the notional of every
//! open ETH position, which is a consensus change.
//!
//! The hazard that *does* exist here is a caller passing a **spot** quantity
//! where a contract quantity belongs — off by 100x on an equity, 10x on SOL.
//! `cb_types::scales` keeps the two apart behind deliberately distinct names
//! (`spot_qty_from_whole` vs `contract_qty_from_whole`); use them.
//!
//! Per-market contract sizing is nominally owned by `PerpMarket::lot_size`, but
//! note that nothing reads that field today — see the note on
//! `position_notional_micro_usd`.
//!
//! ## Piecewise leverage tiers
//!
//! From `docs/design/perps-trading.md §2.3`:
//!
//! | Tier | Notional band (USD) | IM    | MM    |
//! |------|---------------------|-------|-------|
//! | 1    | 0 – 100k            | 2.0%  | 1.0%  |
//! | 2    | 100k – 1M           | 4.0%  | 2.0%  |
//! | 3    | 1M – 5M             | 10.0% | 5.0%  |
//! | 4    | > 5M                | 20.0% | 10.0% |
//!
//! "Piecewise" means: the IM/MM requirement on a 1.5M-USD position is
//! `100k*2% + 900k*4% + 500k*10% = 88_000 USD`, **not** a flat `1.5M*10%`.
//! This avoids cliff effects at tier boundaries.
//!
//! The default tiers above are encoded in [`DEFAULT_TIERS`]; integration tests
//! and per-market variants can pass a custom slice. The slice MUST be in
//! ascending notional order with the last band's `upper_bound_micro_usd ==
//! u128::MAX` (the catch-all). Callers pass tiers that match the design;
//! we don't validate at runtime to keep the module pure.

use cb_types::{MarginAccount, Position, Side, Signed};

/// One band of the piecewise leverage tier table.
///
/// Bands cover the half-open interval `[prev_upper_bound, upper_bound)` in
/// micro-USD. The first band's lower edge is `0`; the last band's
/// `upper_bound_micro_usd` MUST be `u128::MAX` so the tiers form a partition
/// of `[0, ∞)`. Margin coefficients are bps (10_000 = 100%).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeverageTier {
    /// Inclusive upper bound of this band, in micro-USD. The catch-all band
    /// uses `u128::MAX`.
    pub upper_bound_micro_usd: u128,
    /// Initial-margin coefficient for notional in this band, in bps.
    /// E.g. 200 bps = 2.0%.
    pub initial_margin_bps: u16,
    /// Maintenance-margin coefficient for notional in this band, in bps.
    pub maintenance_margin_bps: u16,
}

/// Default leverage tiers per `docs/design/perps-trading.md §2.3`. Used by
/// `compute_margin_required` when a market doesn't supply its own table; v1
/// listed perps (BtcPerp, EthPerp) all use this shape.
pub const DEFAULT_TIERS: &[LeverageTier] = &[
    // Tier 1: 0 – 100k USD, IM 2%, MM 1%.
    LeverageTier {
        upper_bound_micro_usd: 100_000_000_000,
        initial_margin_bps: 200,
        maintenance_margin_bps: 100,
    },
    // Tier 2: 100k – 1M USD, IM 4%, MM 2%.
    LeverageTier {
        upper_bound_micro_usd: 1_000_000_000_000,
        initial_margin_bps: 400,
        maintenance_margin_bps: 200,
    },
    // Tier 3: 1M – 5M USD, IM 10%, MM 5%.
    LeverageTier {
        upper_bound_micro_usd: 5_000_000_000_000,
        initial_margin_bps: 1_000,
        maintenance_margin_bps: 500,
    },
    // Tier 4: > 5M USD, IM 20%, MM 10%.
    LeverageTier {
        upper_bound_micro_usd: u128::MAX,
        initial_margin_bps: 2_000,
        maintenance_margin_bps: 1_000,
    },
];

/// Which margin coefficient to apply across the tier table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MarginKind {
    Initial,
    Maintenance,
}

/// Convert `(qty, mark_q8)` into a notional in micro-USD (scaled 1e6).
///
/// Formula: `notional_micro = qty * mark_q8 / 1e10`, where the denominator is
/// the q8 price scale (1e8) times the contract-to-USD scale (1e2, from the
/// fixed 1e8 contract quantity scale against USD's 6 decimals).
///
/// **`qty` must be in contract units**, not the underlying's spot base units.
/// The denominator is the same for every underlying precisely because the
/// contract scale is — see the module docs, which explain why turning it into a
/// per-asset lookup would be a double-count rather than a fix.
///
/// `lot_size` on the market record is not consulted here, and in fact is not
/// read anywhere in the executor today: derivative order quantities are
/// currently unconstrained by lot. Enforcing it is a separate change.
///
/// Saturating multiplication. At realistic v1 scales (qty <= 1e16, mark <=
/// 1e14 q8) this stays well inside u128.
pub fn position_notional_micro_usd(qty: u128, mark_q8: u128) -> u128 {
    qty.saturating_mul(mark_q8) / 10u128.pow(10)
}

/// Unrealized PnL on a single position as `Signed` micro-USD.
///
/// Convention: PnL is positive when the position is gaining. Long gains when
/// `mark > entry`; short gains when `entry > mark`. Math runs in `i128` so
/// the result fits without saturation at v1 scales.
pub fn position_unrealized_pnl_micro_usd(
    side: Side,
    qty: u128,
    entry_q8: u128,
    mark_q8: u128,
) -> Signed {
    if qty == 0 {
        return Signed::ZERO;
    }
    let (gain, abs_diff) = match side {
        Side::Buy => {
            if mark_q8 >= entry_q8 {
                (true, mark_q8 - entry_q8)
            } else {
                (false, entry_q8 - mark_q8)
            }
        }
        Side::Sell => {
            if entry_q8 >= mark_q8 {
                (true, entry_q8 - mark_q8)
            } else {
                (false, mark_q8 - entry_q8)
            }
        }
    };
    if abs_diff == 0 {
        return Signed::ZERO;
    }
    // `qty * abs_diff` is in qty(1e8 base) * px_q8(1e8) = 1e16 units; divide
    // by 1e10 to land in micro-USD. Same denominator as `position_notional_micro_usd`.
    let abs = qty.saturating_mul(abs_diff) / 10u128.pow(10);
    if abs == 0 {
        return Signed::ZERO;
    }
    Signed {
        sign: if gain { 1 } else { -1 },
        abs,
    }
}

/// Total equity = collateral − bad debt + Σ unrealized PnL across positions,
/// in micro-USD.
///
/// # Bad debt is negative money, not no money
///
/// `MarginAccount.bad_debt` is what the account owes the protocol after a loss
/// its collateral could not cover. Until QA finding F4 this function read
/// `collateral` alone, which meant an account that owed the protocol money was
/// treated as one with *nothing* rather than one with *less than nothing*: the
/// debt never restricted the account, and an indebted trader could be funded
/// again and pass [`passes_initial_margin`] as though the debt were not there.
/// Subtracting it here is what lets the whole margin system — equity,
/// [`is_liquidatable`], and the pre-trade gate that calls this — see it.
///
/// `marks` is a slice of `(market, mark_q8)` pairs. Positions whose market
/// is missing from `marks` contribute zero PnL — defensive behaviour so a
/// missing oracle on one market doesn't poison the whole equity calc, which
/// is the common case at first-block-after-add.
///
/// Pure: same inputs always produce the same output (saturating arithmetic;
/// no panics).
pub fn compute_equity(
    margin_account: &MarginAccount,
    positions: &[Position],
    marks: &[(cb_types::DerivId, u128)],
) -> Signed {
    let mut acc: i128 =
        (margin_account.collateral as i128).saturating_sub(margin_account.bad_debt as i128);
    for p in positions {
        if p.qty == 0 {
            continue;
        }
        let mark_q8 = match marks.iter().find(|(m, _)| m == &p.market).map(|(_, x)| *x) {
            Some(x) => x,
            None => continue,
        };
        let pnl = position_unrealized_pnl_micro_usd(p.side, p.qty, p.entry_px_q8, mark_q8);
        acc = acc.saturating_add(pnl.to_i128());
    }
    Signed::from_i128(acc)
}

/// Sum the per-position notional in micro-USD across the supplied positions.
/// Missing marks contribute zero.
pub fn total_position_notional_micro_usd(
    positions: &[Position],
    marks: &[(cb_types::DerivId, u128)],
) -> u128 {
    let mut total: u128 = 0;
    for p in positions {
        if p.qty == 0 {
            continue;
        }
        let mark_q8 = match marks.iter().find(|(m, _)| m == &p.market).map(|(_, x)| *x) {
            Some(x) => x,
            None => continue,
        };
        total = total.saturating_add(position_notional_micro_usd(p.qty, mark_q8));
    }
    total
}

/// Piecewise margin requirement for a single notional, walked across the
/// supplied tier table. Bps coefficients are applied band-by-band; the band's
/// portion of the position contributes `band_notional * bps / 10_000` to the
/// requirement.
///
/// Internal — `initial_margin_required` and `maintenance_margin_required` use
/// it with the right `MarginKind`.
fn piecewise_margin_for_notional(
    notional_micro_usd: u128,
    tiers: &[LeverageTier],
    kind: MarginKind,
) -> u128 {
    if notional_micro_usd == 0 || tiers.is_empty() {
        return 0;
    }
    let mut prev_bound: u128 = 0;
    let mut remaining = notional_micro_usd;
    let mut accum: u128 = 0;
    for tier in tiers {
        if remaining == 0 {
            break;
        }
        let band_top = tier.upper_bound_micro_usd;
        // Width of this band that the notional reaches.
        let band_top_clamped = band_top.min(notional_micro_usd);
        let band_width = band_top_clamped.saturating_sub(prev_bound);
        if band_width == 0 {
            // Notional already used up; no contribution from this band.
            prev_bound = band_top;
            continue;
        }
        let bps = match kind {
            MarginKind::Initial => tier.initial_margin_bps,
            MarginKind::Maintenance => tier.maintenance_margin_bps,
        } as u128;
        // Round up so the requirement always covers any rounding loss in our
        // favour (protocol-side rounding). bps is at most 10_000.
        let contrib = band_width.saturating_mul(bps).saturating_add(9_999) / 10_000;
        accum = accum.saturating_add(contrib);
        remaining = remaining.saturating_sub(band_width);
        prev_bound = band_top;
    }
    accum
}

/// Sum of piecewise initial-margin requirements across every open position.
/// Pure: all inputs explicit; same inputs → same output.
pub fn initial_margin_required(
    positions: &[Position],
    marks: &[(cb_types::DerivId, u128)],
    tiers: &[LeverageTier],
) -> u128 {
    let mut total: u128 = 0;
    for p in positions {
        if p.qty == 0 {
            continue;
        }
        let mark_q8 = match marks.iter().find(|(m, _)| m == &p.market).map(|(_, x)| *x) {
            Some(x) => x,
            None => continue,
        };
        let notional = position_notional_micro_usd(p.qty, mark_q8);
        let req = piecewise_margin_for_notional(notional, tiers, MarginKind::Initial);
        total = total.saturating_add(req);
    }
    total
}

/// Sum of piecewise maintenance-margin requirements across every open
/// position. Same shape as `initial_margin_required`.
pub fn maintenance_margin_required(
    positions: &[Position],
    marks: &[(cb_types::DerivId, u128)],
    tiers: &[LeverageTier],
) -> u128 {
    let mut total: u128 = 0;
    for p in positions {
        if p.qty == 0 {
            continue;
        }
        let mark_q8 = match marks.iter().find(|(m, _)| m == &p.market).map(|(_, x)| *x) {
            Some(x) => x,
            None => continue,
        };
        let notional = position_notional_micro_usd(p.qty, mark_q8);
        let req = piecewise_margin_for_notional(notional, tiers, MarginKind::Maintenance);
        total = total.saturating_add(req);
    }
    total
}

/// Pre-trade gate: would the account, after taking on `proposed_positions`,
/// have enough equity to cover the piecewise initial-margin requirement?
///
/// `proposed_positions` is the **post-fill** view of every cross-margin
/// position the account would hold. The caller computes this by simulating
/// a worst-case fill at the order's limit price (worst for the user, best
/// for the protocol) — the executor in `cb-block` does that derivation.
///
/// Returns `true` if the equity covers the IM requirement (allowed to place);
/// `false` otherwise.
pub fn passes_initial_margin(
    margin_account: &MarginAccount,
    proposed_positions: &[Position],
    marks: &[(cb_types::DerivId, u128)],
    tiers: &[LeverageTier],
) -> bool {
    let equity = compute_equity(margin_account, proposed_positions, marks);
    let im_required = initial_margin_required(proposed_positions, marks, tiers);
    // Equity must be >= im_required, treating equity-negative as fail.
    if equity.sign < 0 {
        return im_required == 0;
    }
    let equity_u: u128 = equity.abs;
    equity_u >= im_required
}

/// Pre-trade gate that also charges for what the account's resting orders
/// could become.
///
/// `positions` is the post-fill view the equity is computed from, exactly as
/// in [`passes_initial_margin`]. `exposures` is the per-market *potential*
/// position — the larger of `long + Σ resting bids` and
/// `short + Σ resting asks`, excluding reduce-only orders — and the initial
/// margin requirement is charged against that. Marking every resting order
/// as if it filled is what makes N orders that each pass the gate alone
/// unable to pass it together (QA F11); before this, resting perpetual orders
/// reserved nothing.
///
/// The two lists are separate because they answer different questions: PnL
/// depends on the entry price of what is actually held, the requirement on
/// the notional of what could be held. An exposure row's entry price is
/// irrelevant and callers may pass the mark.
pub fn passes_initial_margin_with_exposure(
    margin_account: &MarginAccount,
    positions: &[Position],
    exposures: &[Position],
    marks: &[(cb_types::DerivId, u128)],
    tiers: &[LeverageTier],
) -> bool {
    let equity = compute_equity(margin_account, positions, marks);
    let im_required = initial_margin_required(exposures, marks, tiers);
    if equity.sign < 0 {
        return im_required == 0;
    }
    equity.abs >= im_required
}

/// Pre-trade gate over the worse of the two worlds an order can leave the
/// account in: the one where it fills in full at its limit price, and the one
/// where it rests and never fills.
///
/// [`passes_initial_margin_with_exposure`] took the equity from the filled
/// world alone, and that world is not the worst one for a resting order. A
/// bid below the mark, filled at its limit, is an instant unrealised gain —
/// so an account with next to no collateral could rest a bid at 45,000
/// against a 50,000 mark, be credited the 5,000 it would gain if the bid
/// filled *now*, and pass. The bid fills when the market has come down to it,
/// at which point the gain is gone and the position has nothing behind it.
/// The gate now takes the smaller of the two equities: `unfilled` is what the
/// account holds if the order rests, `filled` what it holds if the order
/// fills, and `fill_pnl_micro` is what the fill realises on the way — the
/// realised loss of closing an underwater position, which the filled world's
/// positions no longer show and which the earlier gate dropped, letting an
/// account that was already below maintenance flip into a fresh position on
/// the other side.
///
/// An order that cannot rest — immediate-or-cancel, fill-or-kill — has only
/// the filled world, and passes `None` for `unfilled`: it either fills now,
/// at its limit or better, or it is gone, so the account's standing position
/// is not something the order can be charged for. That is what lets an
/// underwater account close with a plain IOC and not only with the
/// reduce-only flag; a resting close is charged, because the position it
/// was written against can be gone by the time it fills.
///
/// The requirement is charged against `exposures`, the per-market potential
/// position (see the executor's `potential_exposures`), at
/// `requirement_marks`; the equities use `marks`. The two lists are separate
/// so the caller can price the requirement of an unmarked market at the
/// order's own limit — there is no other price the chain knows — without
/// letting that limit stand in for a mark on the equity side, where a held
/// position on that market would be worth whatever the trader asked.
///
/// Negative equity passes only a requirement of zero: an underwater account
/// may still close, and may do nothing else.
#[allow(clippy::too_many_arguments)]
pub fn passes_initial_margin_worst_case(
    margin_account: &MarginAccount,
    unfilled: Option<&[Position]>,
    filled: &[Position],
    fill_pnl_micro: i128,
    exposures: &[Position],
    marks: &[(cb_types::DerivId, u128)],
    requirement_marks: &[(cb_types::DerivId, u128)],
    tiers: &[LeverageTier],
) -> bool {
    let filled_equity = compute_equity(margin_account, filled, marks)
        .to_i128()
        .saturating_add(fill_pnl_micro);
    let equity = match unfilled {
        Some(held) => compute_equity(margin_account, held, marks)
            .to_i128()
            .min(filled_equity),
        None => filled_equity,
    };
    let im_required = initial_margin_required(exposures, requirement_marks, tiers);
    if equity < 0 {
        return im_required == 0;
    }
    (equity as u128) >= im_required
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_types::{Address, DerivId};

    // -----------------------------------------------------------------------
    // Per-asset notional pins.
    //
    // Each of these asserts a LITERAL micro-USD notional for a known position
    // size and price on a specific underlying. Their purpose is to fail loudly
    // if anyone ever makes the `1e10` denominator conditional on the asset:
    // a per-asset divisor would leave the BTC case passing and break the
    // others, which is precisely the failure mode that is easy to ship and
    // hard to notice.
    //
    // The quantities come from `contract_qty_from_whole`, the prices from
    // `parse_price_q8`, so the tests exercise the same conversions callers use
    // rather than re-deriving the scaling by hand.
    // -----------------------------------------------------------------------

    mod notional_pins {
        use super::*;
        use cb_types::scales::{contract_qty_from_whole, parse_price_q8};
        use cb_types::AssetId;

        /// One whole unit of an underlying, at a given price, in micro-USD.
        fn one_unit_at(price: &str) -> u128 {
            position_notional_micro_usd(
                contract_qty_from_whole(1).unwrap(),
                parse_price_q8(price).unwrap(),
            )
        }

        #[test]
        fn btc_one_coin_at_64k_is_64k_micro_usd() {
            assert_eq!(one_unit_at("64000"), 64_000_000_000);
        }

        #[test]
        fn eth_one_coin_at_3200_is_3200_usd() {
            // ETH has 18 spot decimals. If the denominator were derived from
            // the asset's decimals this would be off by 10^10.
            assert_eq!(one_unit_at("3200"), 3_200_000_000);
        }

        #[test]
        fn sol_one_coin_at_145_is_145_usd() {
            // SOL has 9 spot decimals: a decimals-derived divisor breaks this
            // by exactly 10x.
            assert_eq!(one_unit_at("145"), 145_000_000);
        }

        #[test]
        fn aapl_one_share_at_225_is_225_usd() {
            // AAPL has 6 spot decimals: a decimals-derived divisor breaks this
            // by exactly 100x — the hundred-fold error in the brief.
            assert_eq!(one_unit_at("225"), 225_000_000);
        }

        #[test]
        fn equity_fractional_size_is_exact() {
            // Half a share of a $225 stock is $112.50, to the micro-USD.
            let half_share = contract_qty_from_whole(1).unwrap() / 2;
            assert_eq!(
                position_notional_micro_usd(half_share, parse_price_q8("225").unwrap()),
                112_500_000
            );
        }

        #[test]
        fn notional_is_independent_of_the_underlyings_spot_decimals() {
            // THE property this module's denominator relies on: contract units
            // are the same scale for every underlying, so one unit at one price
            // is one notional regardless of the asset's spot decimals. The
            // assets below span 6, 8, 9 and 18 decimals.
            let qty = contract_qty_from_whole(3).unwrap();
            let px = parse_price_q8("1234.56").unwrap();
            let expected = position_notional_micro_usd(qty, px);
            assert_eq!(expected, 3_703_680_000, "3 units at $1,234.56 = $3,703.68");

            let spans: Vec<u8> = [AssetId::Aapl, AssetId::Btc, AssetId::Sol, AssetId::Eth]
                .iter()
                .map(|a| a.decimals())
                .collect();
            assert_eq!(spans, vec![6, 8, 9, 18], "test should span four scales");

            // The function takes no AssetId at all — that IS the invariant.
            // Assert it stays that way by checking the value is stable for a
            // quantity built from each asset's contract conversion.
            for asset in [AssetId::Aapl, AssetId::Btc, AssetId::Sol, AssetId::Eth] {
                assert_eq!(
                    position_notional_micro_usd(contract_qty_from_whole(3).unwrap(), px),
                    expected,
                    "{asset} notional must not depend on its spot decimals"
                );
            }
        }

        #[test]
        fn a_spot_quantity_passed_as_a_contract_quantity_is_visibly_wrong() {
            // Documents the hazard that is real, as opposed to the one that
            // isn't. Feeding AAPL *spot* base units (6 decimals) into the
            // contract-unit formula understates the notional by 100x.
            use cb_types::scales::spot_qty_from_whole;
            let px = parse_price_q8("225").unwrap();
            let correct = position_notional_micro_usd(contract_qty_from_whole(1).unwrap(), px);
            let wrong =
                position_notional_micro_usd(spot_qty_from_whole(AssetId::Aapl, 1).unwrap(), px);
            assert_eq!(correct, 225_000_000);
            assert_eq!(wrong, 2_250_000);
            assert_eq!(correct / wrong, 100);
        }

        #[test]
        fn unrealized_pnl_uses_the_same_scale_as_notional() {
            // The two share a denominator; if one were made per-asset and the
            // other not, equity would drift silently.
            let qty = contract_qty_from_whole(2).unwrap();
            let entry = parse_price_q8("100").unwrap();
            let mark = parse_price_q8("110").unwrap();
            let pnl = position_unrealized_pnl_micro_usd(Side::Buy, qty, entry, mark);
            assert_eq!(pnl.sign, 1);
            assert_eq!(pnl.abs, 20_000_000, "2 units x $10 = $20");
            let short = position_unrealized_pnl_micro_usd(Side::Sell, qty, entry, mark);
            assert_eq!(short.sign, -1);
            assert_eq!(short.abs, 20_000_000);
        }
    }

    fn pos(side: Side, qty: u128, entry_q8: u128) -> Position {
        Position {
            owner: Address([1; 20]),
            market: DerivId::perp(0),
            side,
            qty,
            entry_funding_index: Signed::ZERO,
            entry_px_q8: entry_q8,
            margin_mode: 0,
            isolated_margin: 0,
            last_funding_height: 0,
        }
    }

    fn account(collateral: u128) -> MarginAccount {
        MarginAccount {
            owner: Address([1; 20]),
            collateral,
            unrealized_pnl: Signed::ZERO,
            bad_debt: 0,
        }
    }

    // Helpers: constants for the design's worked examples.
    const Q8: u128 = 100_000_000;
    const MICRO: u128 = 1_000_000;

    /// 50_000 USD/BTC = 50_000 * 1e8 q8 = 5e12 q8.
    const PX_50K_Q8: u128 = 50_000 * Q8;

    /// 1 BTC in satoshi base units (8 decimals).
    const ONE_BTC: u128 = 100_000_000;

    // ---------------- notional + pnl ----------------

    #[test]
    fn notional_btc_at_50k_yields_50k_micro_usd_scaled() {
        // 1 BTC * 50_000 USD = $50_000 = 50_000_000_000 micro-USD.
        let n = position_notional_micro_usd(ONE_BTC, PX_50K_Q8);
        assert_eq!(n, 50_000 * MICRO);
    }

    #[test]
    fn long_pnl_positive_when_mark_above_entry() {
        // Long 1 BTC @ 50k, mark 51k → +1_000 USD.
        let pnl = position_unrealized_pnl_micro_usd(Side::Buy, ONE_BTC, PX_50K_Q8, 51_000 * Q8);
        assert_eq!(pnl, Signed::from_i128(1_000 * MICRO as i128));
    }

    #[test]
    fn short_pnl_positive_when_mark_below_entry() {
        let pnl = position_unrealized_pnl_micro_usd(Side::Sell, ONE_BTC, PX_50K_Q8, 49_000 * Q8);
        assert_eq!(pnl, Signed::from_i128(1_000 * MICRO as i128));
    }

    #[test]
    fn pnl_zero_for_zero_qty() {
        let pnl = position_unrealized_pnl_micro_usd(Side::Buy, 0, PX_50K_Q8, 60_000 * Q8);
        assert_eq!(pnl, Signed::ZERO);
    }

    // ---------------- equity ----------------

    #[test]
    fn equity_includes_collateral_plus_pnl() {
        let m = account(10_000 * MICRO); // $10k collateral
        let p = pos(Side::Buy, ONE_BTC, PX_50K_Q8);
        let marks = [(DerivId::perp(0), 51_000 * Q8)];
        let eq = compute_equity(&m, &[p], &marks);
        // 10k + 1_000 = 11_000 USD.
        assert_eq!(eq, Signed::from_i128(11_000 * MICRO as i128));
    }

    #[test]
    fn equity_can_go_negative_on_loss() {
        let m = account(500 * MICRO); // $500
                                      // Long 1 BTC @ 50k, mark 48k → −2_000 USD; equity = 500 - 2_000 = -1_500.
        let p = pos(Side::Buy, ONE_BTC, PX_50K_Q8);
        let marks = [(DerivId::perp(0), 48_000 * Q8)];
        let eq = compute_equity(&m, &[p], &marks);
        assert_eq!(eq.sign, -1);
        assert_eq!(eq.abs, 1_500 * MICRO);
    }

    #[test]
    fn equity_skips_positions_without_marks() {
        // No marks supplied → PnL contribution from the position is zero.
        let m = account(1_000 * MICRO);
        let p = pos(Side::Buy, ONE_BTC, PX_50K_Q8);
        let eq = compute_equity(&m, &[p], &[]);
        assert_eq!(eq, Signed::from_i128(1_000 * MICRO as i128));
    }

    // ---------------- piecewise IM/MM ----------------

    #[test]
    fn piecewise_im_for_50k_position_uses_tier1_only() {
        // Notional 50k → entirely within tier 1 (0..100k @ 2%).
        // IM = 50k * 2% = 1_000 USD.
        let req = piecewise_margin_for_notional(50_000 * MICRO, DEFAULT_TIERS, MarginKind::Initial);
        assert_eq!(req, 1_000 * MICRO);
    }

    #[test]
    fn piecewise_im_for_1_5m_matches_design_example() {
        // The design's worked example: 1.5M position →
        //   tier1 [0,100k]    @ 2%  = 100k  * 2%  = 2_000
        //   tier2 [100k,1M]   @ 4%  = 900k  * 4%  = 36_000
        //   tier3 [1M,5M]     @ 10% = 500k  * 10% = 50_000
        // Total = 88_000 USD.
        let req =
            piecewise_margin_for_notional(1_500_000 * MICRO, DEFAULT_TIERS, MarginKind::Initial);
        assert_eq!(req, 88_000 * MICRO);
    }

    #[test]
    fn piecewise_mm_for_1_5m_is_half_of_im() {
        // MM coefficients are exactly half of IM in the default table:
        //   tier1 1%, tier2 2%, tier3 5% → 1_000 + 18_000 + 25_000 = 44_000.
        let req = piecewise_margin_for_notional(
            1_500_000 * MICRO,
            DEFAULT_TIERS,
            MarginKind::Maintenance,
        );
        assert_eq!(req, 44_000 * MICRO);
    }

    #[test]
    fn piecewise_im_above_5m_lands_in_tier4() {
        // 6M position:
        //   tier1 100k * 2%  = 2_000
        //   tier2 900k * 4%  = 36_000
        //   tier3 4M  * 10%  = 400_000
        //   tier4 1M  * 20%  = 200_000
        //   = 638_000 USD
        let req =
            piecewise_margin_for_notional(6_000_000 * MICRO, DEFAULT_TIERS, MarginKind::Initial);
        assert_eq!(req, 638_000 * MICRO);
    }

    #[test]
    fn piecewise_zero_notional_zero_requirement() {
        let req = piecewise_margin_for_notional(0, DEFAULT_TIERS, MarginKind::Initial);
        assert_eq!(req, 0);
    }

    // ---------------- aggregation across positions ----------------

    #[test]
    fn initial_margin_sums_across_positions() {
        let p1 = pos(Side::Buy, ONE_BTC, PX_50K_Q8); // 50k notional → IM 1_000
        let mut p2 = pos(Side::Sell, ONE_BTC, PX_50K_Q8);
        p2.market = DerivId::perp(1);
        let marks = [(DerivId::perp(0), PX_50K_Q8), (DerivId::perp(1), PX_50K_Q8)];
        let im = initial_margin_required(&[p1, p2], &marks, DEFAULT_TIERS);
        assert_eq!(im, 2_000 * MICRO);
    }

    // ---------------- pre-trade gate ----------------

    #[test]
    fn passes_initial_margin_allows_well_collateralized_account() {
        // Account with $5_000 collateral; 1 BTC long at 50k mark = 1_000 IM.
        let m = account(5_000 * MICRO);
        let p = pos(Side::Buy, ONE_BTC, PX_50K_Q8);
        let marks = [(DerivId::perp(0), PX_50K_Q8)];
        assert!(passes_initial_margin(&m, &[p], &marks, DEFAULT_TIERS));
    }

    #[test]
    fn passes_initial_margin_rejects_undercollateralized() {
        // Collateral $500 < required IM $1_000 → reject.
        let m = account(500 * MICRO);
        let p = pos(Side::Buy, ONE_BTC, PX_50K_Q8);
        let marks = [(DerivId::perp(0), PX_50K_Q8)];
        assert!(!passes_initial_margin(&m, &[p], &marks, DEFAULT_TIERS));
    }

    #[test]
    fn passes_initial_margin_rejects_when_equity_negative() {
        // Collateral too small to cover unrealized loss → equity negative.
        // 1 BTC long opened at 50k, mark 40k → uPnL = -10_000.
        // Collateral 1_000 → equity = -9_000. IM required = 1 BTC * 40k = 40k notional, 2% = 800.
        // Negative equity must fail even if a flat IM ratio looked OK.
        let m = account(1_000 * MICRO);
        let p = pos(Side::Buy, ONE_BTC, PX_50K_Q8);
        let marks = [(DerivId::perp(0), 40_000 * Q8)];
        assert!(!passes_initial_margin(&m, &[p], &marks, DEFAULT_TIERS));
    }

    #[test]
    fn passes_initial_margin_zero_position_zero_requirement() {
        let m = account(1);
        // No positions → zero IM, even with low collateral.
        assert!(passes_initial_margin(&m, &[], &[], DEFAULT_TIERS));
    }

    // ---------------- worst-case gate ----------------

    #[test]
    fn a_hypothetical_gain_from_filling_below_the_mark_does_not_count() {
        // $500 of collateral; a 1 BTC bid at 45k against a 50k mark. Filled
        // at its limit it would be a $5,000 gain, and the earlier gate
        // credited it: 5,500 >= 1,000. The bid only fills once the market
        // has come down to it, so the unfilled world (equity 500) is the one
        // that binds.
        let m = account(500 * MICRO);
        let proposed = [pos(Side::Buy, ONE_BTC, 45_000 * Q8)];
        let exposure = [pos(Side::Buy, ONE_BTC, 0)];
        let marks = [(DerivId::perp(0), PX_50K_Q8)];
        assert!(passes_initial_margin_with_exposure(
            &m,
            &proposed,
            &exposure,
            &marks,
            DEFAULT_TIERS
        ));
        assert!(!passes_initial_margin_worst_case(
            &m,
            Some(&[]),
            &proposed,
            0,
            &exposure,
            &marks,
            &marks,
            DEFAULT_TIERS
        ));
    }

    #[test]
    fn a_hypothetical_loss_from_filling_above_the_mark_does_count() {
        // The filled world is the worse one for a bid above the mark: $1,500
        // of collateral, a 1 BTC bid at 51k against a 50k mark. Filled, it is
        // an instant $1,000 loss, leaving 500 against a requirement of 1,000.
        let m = account(1_500 * MICRO);
        let proposed = [pos(Side::Buy, ONE_BTC, 51_000 * Q8)];
        let exposure = [pos(Side::Buy, ONE_BTC, 0)];
        let marks = [(DerivId::perp(0), PX_50K_Q8)];
        assert!(!passes_initial_margin_worst_case(
            &m,
            Some(&[]),
            &proposed,
            0,
            &exposure,
            &marks,
            &marks,
            DEFAULT_TIERS
        ));
        // At the mark, both worlds agree and 1,500 covers 1,000.
        let at_mark = [pos(Side::Buy, ONE_BTC, PX_50K_Q8)];
        assert!(passes_initial_margin_worst_case(
            &m,
            Some(&[]),
            &at_mark,
            0,
            &exposure,
            &marks,
            &marks,
            DEFAULT_TIERS
        ));
    }

    #[test]
    fn the_realised_loss_of_a_close_is_charged_before_the_new_side_is_opened() {
        // Long 1 BTC from 50k with $1,500, mark 45k: equity is -3,500 and the
        // account is already liquidatable. Selling 2 BTC at 45k would leave a
        // 1 BTC short whose own PnL at the mark is zero — the $5,000 loss is
        // realised, not carried — so a gate that looks only at the resulting
        // positions sees 1,500 against a 900 requirement and lets the account
        // flip into a position it has nothing behind.
        let m = account(1_500 * MICRO);
        let held = [pos(Side::Buy, ONE_BTC, PX_50K_Q8)];
        let proposed = [pos(Side::Sell, ONE_BTC, 45_000 * Q8)];
        let exposure = [pos(Side::Sell, ONE_BTC, 0)];
        let marks = [(DerivId::perp(0), 45_000 * Q8)];
        let realised = -(5_000 * MICRO as i128);
        assert!(passes_initial_margin_with_exposure(
            &m,
            &proposed,
            &exposure,
            &marks,
            DEFAULT_TIERS
        ));
        assert!(!passes_initial_margin_worst_case(
            &m,
            Some(&held),
            &proposed,
            realised,
            &exposure,
            &marks,
            &marks,
            DEFAULT_TIERS
        ));
        // Closing outright is still allowed: nothing is left to require
        // margin for, and negative equity passes a zero requirement.
        assert!(passes_initial_margin_worst_case(
            &m,
            Some(&held),
            &[],
            realised,
            &[],
            &marks,
            &marks,
            DEFAULT_TIERS
        ));
    }

    #[test]
    fn the_requirement_can_be_priced_off_a_mark_the_equity_does_not_use() {
        // An unmarked market: the equity side has no price, the requirement
        // side is handed the order's limit. $100 cannot back 1 BTC at 50k.
        let m = account(100 * MICRO);
        let proposed = [pos(Side::Buy, ONE_BTC, PX_50K_Q8)];
        let exposure = [pos(Side::Buy, ONE_BTC, 0)];
        let at_limit = [(DerivId::perp(0), PX_50K_Q8)];
        assert!(!passes_initial_margin_worst_case(
            &m,
            Some(&[]),
            &proposed,
            0,
            &exposure,
            &[],
            &at_limit,
            DEFAULT_TIERS
        ));
        let funded = account(5_000 * MICRO);
        assert!(passes_initial_margin_worst_case(
            &funded,
            Some(&[]),
            &proposed,
            0,
            &exposure,
            &[],
            &at_limit,
            DEFAULT_TIERS
        ));
    }
}
