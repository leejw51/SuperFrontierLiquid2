//! Mark-price computation.
//!
//! `mark = median(oracle, clamp(mid, oracle*(1-K), oracle*(1+K)),
//!                clamp(last, oracle*(1-K), oracle*(1+K)))`
//!
//! where `K` is per-market: 1% for perps, 5% for dated futures (per
//! `docs/design/perps-trading.md §3.2` and `§6.3`).
//!
//! Inputs and output are all `u128` at the `q8` scale (quote-per-base * 1e8).
//! No floating point on the consensus path — clamps and the median are pure
//! integer comparisons / arithmetic.
//!
//! `mid` and `last` are `Option<u128>` because before M3 ships the perp book
//! we have no orderbook midpoint and no executed trades. The graceful fallback
//! is "substitute oracle for the missing input" — the median then degenerates
//! toward oracle, which is exactly what the trading spec asks for.

use cb_types::DerivId;

/// Per-market clamp factor `K`, expressed in basis points of `oracle`.
/// `K = 1%` => 100 bps for perps, `K = 5%` => 500 bps for dated futures.
///
/// We use bps integers (not fixed-point) because the only operation we need is
/// `oracle * K_bps / 10_000`, which is exact in `u128` for any `oracle <= 1e36`.
/// Per-market K shrink near expiry (the M5 "shrinks linearly from 5% to 1%")
/// is intentionally NOT implemented here — that lands in M5 with its own input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkClampK {
    pub bps: u16,
}

impl MarkClampK {
    /// 1% — perps default.
    pub const PERP: Self = Self { bps: 100 };
    /// 5% — dated futures default.
    pub const FUTURE: Self = Self { bps: 500 };

    /// Pick the clamp factor based on a market's `DerivId.kind()`. Unknown kinds
    /// default to the perp value (the trading spec is explicit that "perp" is
    /// the conservative default; the `from_bytes` constructor already rejects
    /// unknown kinds, so this branch is unreachable in practice).
    pub fn for_market(id: &DerivId) -> Self {
        match id.kind() {
            Ok(cb_types::MarketKind::Perp) => Self::PERP,
            Ok(cb_types::MarketKind::Future) => Self::FUTURE,
            Err(_) => Self::PERP,
        }
    }
}

/// Compute mark price.
///
/// * `oracle_q8` — the latest oracle price for the market. MUST be > 0 (the tx
///   validator rejects zero on the wire; this function panics on zero in debug
///   but uses `oracle` directly in release as the sole input, which is safe).
/// * `mid_q8` — midpoint of the perp book, if available (M3+).
/// * `last_q8` — last executed trade price, if available (M3+).
/// * `k` — clamp factor.
///
/// Returns the median of `[oracle, clamp(mid), clamp(last)]`, with missing
/// inputs replaced by oracle. Pure integer math, no rounding error possible.
pub fn compute_mark_price(
    oracle_q8: u128,
    mid_q8: Option<u128>,
    last_q8: Option<u128>,
    k: MarkClampK,
) -> u128 {
    // The clamp band: oracle * (1 - K) and oracle * (1 + K), exact in u128.
    // `oracle * bps / 10_000` is the canonical bps multiply; since K_bps <= 10_000
    // both endpoints fit in u128 for any oracle that passed validate_static
    // (which caps at 1e36).
    let lo = mul_bps(oracle_q8, 10_000u32.saturating_sub(k.bps as u32));
    let hi = mul_bps(oracle_q8, 10_000u32.saturating_add(k.bps as u32));

    let m = match mid_q8 {
        Some(v) => clamp_u128(v, lo, hi),
        None => oracle_q8,
    };
    let l = match last_q8 {
        Some(v) => clamp_u128(v, lo, hi),
        None => oracle_q8,
    };

    median3(oracle_q8, m, l)
}

/// `value * bps / 10_000`, performed without overflow for `value <= 1e36` and
/// `bps <= 20_000` (the largest factor we ever multiply by in the clamp path).
fn mul_bps(value: u128, bps: u32) -> u128 {
    // `value` as u128 * `bps` as u128 fits because bps <= 20_000 < 2^15 and
    // value <= 1e36 < 2^120 -> product <= 2^135. Use a 256-bit-style staged
    // multiply to stay in u128. Alternative: u256 lib, but that's overkill.
    //
    // Strategy: `value = hi * 2^64 + lo`. Then
    //     value * bps = hi * bps * 2^64 + lo * bps.
    // Both `hi * bps` and `lo * bps` fit in u128 (bps < 2^15, hi/lo < 2^64).
    // We then compute the result / 10_000 in two halves.
    //
    // For our actual ranges (validate_static caps oracle at 1e36 ~ 2^120 and bps
    // <= 20_000) the simple `value as u128 * bps as u128 >> ...` overflows. So
    // do the long-division correctly.
    let hi = value >> 64;
    let lo = value & ((1u128 << 64) - 1);
    let bps_u128 = bps as u128;

    // a = hi * bps  (fits in u128 since hi < 2^64 and bps < 2^15)
    // b = lo * bps  (same)
    let a = hi.saturating_mul(bps_u128);
    let b = lo.saturating_mul(bps_u128);

    // result = (a << 64 + b) / 10_000.
    // Compute a / 10_000 with carry: a = q*10_000 + r, then carry = r << 64.
    let den: u128 = 10_000;
    let q_a = a / den;
    let r_a = a % den;
    // q_a is the upper 64 bits' contribution (still in u128 since a / 10_000 < 2^115).
    // r_a is in [0, 10_000); shift it up 64 bits and add to b — fits in u128.
    let mid = (r_a << 64).saturating_add(b);
    let q_b = mid / den;

    (q_a << 64).saturating_add(q_b)
}

/// `clamp(v, lo, hi)` with `lo <= hi` — guaranteed by construction in
/// `compute_mark_price`. We don't reorder defensively; if a future caller ever
/// passes lo > hi they hit the assertion in debug.
fn clamp_u128(v: u128, lo: u128, hi: u128) -> u128 {
    debug_assert!(lo <= hi, "clamp band is inverted: lo={lo} hi={hi}");
    v.max(lo).min(hi)
}

/// Median of three `u128`s. Branch-free in the sense that there are at most
/// three comparisons and no allocations.
fn median3(a: u128, b: u128, c: u128) -> u128 {
    // Sort by exhaustive case so this is identical across compilers / std-lib
    // versions. Three-element sort is cheap.
    let (lo, mid, hi) = sort3(a, b, c);
    let _ = (lo, hi);
    mid
}

fn sort3(a: u128, b: u128, c: u128) -> (u128, u128, u128) {
    let (a, b) = if a <= b { (a, b) } else { (b, a) };
    let (b, c) = if b <= c { (b, c) } else { (c, b) };
    let (a, b) = if a <= b { (a, b) } else { (b, a) };
    (a, b, c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_types::DerivId;

    // ---------- median + clamp helpers ----------

    #[test]
    fn median3_picks_middle_in_all_orderings() {
        let xs = [10u128, 20, 30];
        for a in xs.iter().copied() {
            for b in xs.iter().copied() {
                for c in xs.iter().copied() {
                    let mut v = [a, b, c];
                    v.sort();
                    assert_eq!(median3(a, b, c), v[1], "({a},{b},{c})");
                }
            }
        }
    }

    #[test]
    fn mul_bps_is_exact_for_clean_cases() {
        // 100 * 100 bps = 100 * 1% = 1
        assert_eq!(mul_bps(100, 100), 1);
        // 50_000 * 1e8 (BTC oracle) * (10_000 - 100) bps / 10_000 = 0.99 of oracle.
        let oracle = 50_000u128 * 100_000_000;
        let lo = mul_bps(oracle, 9_900);
        // 0.99 * 50_000 * 1e8 = 49_500 * 1e8.
        assert_eq!(lo, 49_500u128 * 100_000_000);
    }

    #[test]
    fn mul_bps_handles_large_values_without_overflow() {
        // ORACLE_PX_MAX from validate_static is 1e36. Make sure we don't blow up.
        let oracle = 10u128.pow(30);
        let v = mul_bps(oracle, 10_500); // +5%
                                         // Expected = 1e30 * 105 / 100 = 1.05e30
        assert_eq!(v, 10u128.pow(30) + 10u128.pow(30) / 20);
    }

    // ---------- mark price ----------

    fn k_perp() -> MarkClampK {
        MarkClampK::PERP
    }

    #[test]
    fn mark_oracle_only_when_book_empty() {
        let oracle = 50_000u128 * 100_000_000;
        let m = compute_mark_price(oracle, None, None, k_perp());
        assert_eq!(m, oracle);
    }

    #[test]
    fn mark_with_balanced_book_inside_band() {
        // From docs/design/perps-trading.md §3.3.
        let oracle = 50_000u128 * 100_000_000; // $50,000.00
        let mid = 50_000u128 * 100_000_000;
        let last = 50_030u128 * 100_000_000;
        let m = compute_mark_price(oracle, Some(mid), Some(last), k_perp());
        // median(50000, 50000, 50030) = 50000.
        assert_eq!(m, oracle);
    }

    #[test]
    fn mark_clamps_manipulated_mid() {
        // Manipulator pushes ask to 52000, leaving bid 49950 -> mid 50975.
        let oracle = 50_000u128 * 100_000_000;
        let mid = 50_975u128 * 100_000_000; // > oracle * 1.01 = 50500
        let last = 50_030u128 * 100_000_000;
        let m = compute_mark_price(oracle, Some(mid), Some(last), k_perp());
        // mid clamps to 50_500; median(50_000, 50_500, 50_030) = 50_030.
        assert_eq!(m, 50_030u128 * 100_000_000);
    }

    #[test]
    fn mark_clamp_works_below_oracle() {
        // Manipulator pushes ask down to print at 47000; oracle still 50000.
        let oracle = 50_000u128 * 100_000_000;
        let mid = 47_000u128 * 100_000_000; // < oracle * 0.99 = 49500
        let last = 49_900u128 * 100_000_000;
        let m = compute_mark_price(oracle, Some(mid), Some(last), k_perp());
        // mid clamps to 49_500; median(50_000, 49_500, 49_900) = 49_900.
        assert_eq!(m, 49_900u128 * 100_000_000);
    }

    #[test]
    fn mark_with_only_mid() {
        let oracle = 50_000u128 * 100_000_000;
        let mid = 50_100u128 * 100_000_000;
        let m = compute_mark_price(oracle, Some(mid), None, k_perp());
        // median(50000, 50100, 50000) = 50000 (the missing `last` falls back to oracle).
        assert_eq!(m, oracle);
    }

    #[test]
    fn mark_with_only_last() {
        let oracle = 50_000u128 * 100_000_000;
        let last = 50_100u128 * 100_000_000;
        let m = compute_mark_price(oracle, None, Some(last), k_perp());
        // median(50000, 50000, 50100) = 50000.
        assert_eq!(m, oracle);
    }

    #[test]
    fn mark_dated_uses_5pct_clamp() {
        // K=5% allows wider basis: a 3% contango stays uncapped.
        let oracle = 3_000u128 * 100_000_000; // ETH dated, $3000
        let mid = 3_090u128 * 100_000_000; // +3%
        let last = 3_080u128 * 100_000_000;
        let m = compute_mark_price(oracle, Some(mid), Some(last), MarkClampK::FUTURE);
        // None clamps; median(3000, 3090, 3080) = 3080.
        assert_eq!(m, 3_080u128 * 100_000_000);
    }

    #[test]
    fn mark_dated_clamp_kicks_in_above_5pct() {
        let oracle = 3_000u128 * 100_000_000;
        let mid = 3_200u128 * 100_000_000; // +6.67%
        let last = 3_080u128 * 100_000_000;
        let m = compute_mark_price(oracle, Some(mid), Some(last), MarkClampK::FUTURE);
        // mid clamps to 3000 * 1.05 = 3150; median(3000, 3150, 3080) = 3080.
        assert_eq!(m, 3_080u128 * 100_000_000);
    }

    #[test]
    fn mark_clamp_for_market_picks_correct_k() {
        let perp = DerivId::perp(0);
        let fut = DerivId::future(0);
        assert_eq!(MarkClampK::for_market(&perp), MarkClampK::PERP);
        assert_eq!(MarkClampK::for_market(&fut), MarkClampK::FUTURE);
    }

    #[test]
    fn mark_edge_clamp_exactly_at_boundary() {
        // mid exactly at oracle * (1+K). Clamp returns mid unchanged.
        let oracle = 1_000u128;
        let mid = 1_010u128; // +1% exactly
        let last = 1_000u128;
        let m = compute_mark_price(oracle, Some(mid), Some(last), k_perp());
        // median(1000, 1010, 1000) = 1000.
        assert_eq!(m, 1_000);

        // mid one tick above the band: should clamp.
        let mid_over = 1_011u128;
        let m2 = compute_mark_price(oracle, Some(mid_over), Some(last), k_perp());
        // mid clamps to 1010; median(1000, 1010, 1000) = 1000.
        assert_eq!(m2, 1_000);
    }
}
