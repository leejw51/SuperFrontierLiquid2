//! Funding-rate computation and cumulative-index accrual.
//!
//! Per `docs/design/perps-trading.md §4.2`:
//!
//! ```text
//! raw_funding  = premium_index + clamp(interest - premium, ±0.0005)
//! funding_rate = clamp(raw_funding, ±max_funding)            // ±0.005 / hr
//! ```
//!
//! and `docs/design/perps-protocol.md §4.1`:
//!
//! ```text
//! delta_index   = rate_q9 * elapsed_blocks  (saturating in i128)
//! cumulative   += delta_index
//! ```
//!
//! All rate math uses **q9 fixed-point signed integers** (`i64` rate * 1e9
//! denominator). See the crate-level docs for the rationale on q9 vs q18.

use cb_types::Signed;

/// Denominator for the q9 fixed-point rate. `rate_q9 / RATE_DEN_Q9` recovers
/// the floating-point rate, but we never actually divide on the consensus
/// path — every operation is a fixed-point integer op.
pub const RATE_DEN_Q9: i64 = 1_000_000_000;

/// Funding clamp parameters, all expressed at the q9 scale. The trading spec
/// fixes these protocol-wide; we name them here to keep magic numbers out of
/// callers and to make the asserted invariants in tests explicit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FundingClamp {
    /// Inner clamp on `(interest - premium)`. ±0.05% per hour at q9.
    pub inner_q9: i64,
    /// Outer clamp on the final funding rate. ±0.5% per hour at q9.
    pub outer_q9: i64,
}

impl FundingClamp {
    /// Default per the trading spec: inner = 5 bps/h, outer = 50 bps/h.
    /// Both bounds are nonnegative; the clamp is applied symmetrically.
    pub const DEFAULT: Self = Self {
        inner_q9: 500_000,   // 0.0005 * 1e9
        outer_q9: 5_000_000, // 0.005 * 1e9
    };
}

/// Compute the per-hour funding rate from the current premium-EMA and the
/// constant interest rate. All inputs / output at q9 scale.
///
/// The math is a direct integer translation of the Bitmex/HL formula:
///
/// ```text
///     funding_rate = clamp(
///         premium + clamp(interest - premium, [-inner, +inner]),
///         [-outer, +outer],
///     )
/// ```
///
/// Why widen to `i128` mid-formula: with both inputs in `i64` and the +/-
/// addition we could in principle overflow `i64`. Going via `i128` and then
/// clamping back to `i64` keeps the math infallible without saturating
/// adders. The final clamp is by `outer_q9` which fits in `i64` by
/// construction, so the cast at the end is lossless.
pub fn compute_funding_rate(
    premium_index_q9: i64,
    interest_rate_q9: i64,
    clamp: FundingClamp,
) -> i64 {
    let prem: i128 = premium_index_q9 as i128;
    let int_: i128 = interest_rate_q9 as i128;
    let inner: i128 = clamp.inner_q9 as i128;
    let outer: i128 = clamp.outer_q9 as i128;

    let inner_clamped = clamp_i128(int_ - prem, -inner, inner);
    let raw = prem + inner_clamped;
    let outer_clamped = clamp_i128(raw, -outer, outer);

    // outer_clamped is in [-outer, +outer] which fits in i64 by construction.
    outer_clamped as i64
}

/// The mark price, in q8, at which the index moves one-for-one with the
/// rate: $100. Settlement pays `qty * Δindex / RATE_DEN_Q9` micro-USD and a
/// position's notional is `qty * mark_q8 / 1e10` micro-USD, so an index
/// that accrues `rate * mark_q8 / 1e10` charges exactly `notional * rate`.
pub const FUNDING_PRICE_UNIT_Q8: u128 = 10_000_000_000;

/// Accrue a funding-index delta into the cumulative index.
///
/// `rate_q9` is a **per-interval** rate — the "0.5% per hour" the clamp
/// documents, with the interval being the market's `funding_interval_blocks`.
/// The index is in funding owed per contract unit, so the rate is weighted
/// by the mark it applies at and pro-rated over `elapsed_blocks`:
///
/// ```text
/// delta_index = rate_q9 * mark_q8 * elapsed_blocks
///             / (interval_blocks * FUNDING_PRICE_UNIT_Q8)
/// ```
///
/// saturating in `i128`, rounding toward zero. When ticks land exactly on
/// interval boundaries (the sequencer's behaviour) `elapsed == interval` and
/// the delta is one rate at the mark. (QA F12: the previous formula was
/// `rate * elapsed_blocks`, which applied the hourly cap once per *block* —
/// 3,600 times per hour — and drained the insurance fund in a squeeze. The
/// 2026-09-27 audit: the index left out the price, so funding paid as if
/// every asset traded at $100 — 500x too little on a $50,000 BTC.)
///
/// `cumulative` is sign-magnitude on disk; we reconstitute to `i128`, do the
/// saturating arithmetic, and re-encode. The `Signed::from_i128` helper
/// already handles the `i128::MIN` two-complement edge.
pub fn accrue_index_delta(
    cumulative: Signed,
    rate_q9: i64,
    mark_q8: u128,
    elapsed_blocks: u64,
    interval_blocks: u64,
) -> Signed {
    let cum = cumulative.to_i128();
    let rate = rate_q9 as i128;
    let mark = i128::try_from(mark_q8).unwrap_or(i128::MAX);
    let elapsed = elapsed_blocks as i128;
    let interval = (interval_blocks.max(1)) as i128;
    let den = interval.saturating_mul(FUNDING_PRICE_UNIT_Q8 as i128);

    let delta = rate.saturating_mul(mark).saturating_mul(elapsed) / den;
    let next = cum.saturating_add(delta);
    Signed::from_i128(next)
}

fn clamp_i128(v: i128, lo: i128, hi: i128) -> i128 {
    debug_assert!(lo <= hi, "clamp band inverted");
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // ---------- compute_funding_rate ----------

    #[test]
    fn zero_premium_zero_interest_yields_zero_rate() {
        let r = compute_funding_rate(0, 0, FundingClamp::DEFAULT);
        assert_eq!(r, 0);
    }

    #[test]
    fn small_premium_within_inner_clamp_passes_through() {
        // premium = 0.01% = 100_000 q9, interest = 0 -> inner clamp(int - prem)
        // = clamp(-100_000, ±500_000) = -100_000. raw = 100_000 + (-100_000) = 0.
        let r = compute_funding_rate(100_000, 0, FundingClamp::DEFAULT);
        assert_eq!(r, 0);
    }

    #[test]
    fn large_premium_hits_outer_clamp() {
        // From design doc §4.4: premium ~ 1% (= 10_000_000 q9), interest ~ 0.
        // (int - prem) = -10_000_000 -> inner-clamps to -500_000.
        // raw = 10_000_000 + (-500_000) = 9_500_000.
        // outer clamp at 5_000_000 -> rate = 5_000_000 (= 0.5% / hr cap).
        let r = compute_funding_rate(10_000_000, 0, FundingClamp::DEFAULT);
        assert_eq!(r, 5_000_000);
    }

    #[test]
    fn negative_premium_hits_negative_outer_clamp() {
        let r = compute_funding_rate(-10_000_000, 0, FundingClamp::DEFAULT);
        assert_eq!(r, -5_000_000);
    }

    #[test]
    fn interest_pushes_rate_positive_when_premium_zero() {
        // premium=0, interest=0.01% = 100_000 q9.
        // (int - prem) = 100_000 -> inner clamp ok -> 100_000.
        // raw = 0 + 100_000 = 100_000.
        // outer clamp ok.
        let r = compute_funding_rate(0, 100_000, FundingClamp::DEFAULT);
        assert_eq!(r, 100_000);
    }

    #[test]
    fn sign_symmetry_under_negation() {
        let cases = [
            (123_456i64, 7_000i64),
            (1_000_000, 100_000),
            (10_000_000, 50_000),
        ];
        for (p, i) in cases.iter().copied() {
            let pos = compute_funding_rate(p, i, FundingClamp::DEFAULT);
            let neg = compute_funding_rate(-p, -i, FundingClamp::DEFAULT);
            assert_eq!(pos, -neg, "asymmetric for premium={p} int={i}");
        }
    }

    #[test]
    fn worked_example_from_spec_caps_at_outer() {
        // §4.4: premium = 0.01 (1.00%), interest = 0.0125 bps ≈ 1_250 q9.
        // raw = 0.01 - 0.0005 = 0.00995 (approx), then capped by outer to 0.005.
        let premium_q9 = 10_000_000; // 1.00%
        let interest_q9 = 1_250; // 0.0125 bps = 0.000125 % ~ 1_250 q9
        let r = compute_funding_rate(premium_q9, interest_q9, FundingClamp::DEFAULT);
        assert_eq!(r, 5_000_000);
    }

    // ---------- accrue_index_delta ----------

    /// At $100 the index moves one-for-one with the rate, which keeps the
    /// arithmetic below readable.
    const PAR: u128 = FUNDING_PRICE_UNIT_Q8;

    #[test]
    fn accrue_weights_the_rate_by_the_mark() {
        // 0.05% for one full interval. At $50,000 one contract unit owes
        // 500x what it owes at $100; at $0.10 a thousandth of it.
        let c0 = Signed::ZERO;
        let at_par = accrue_index_delta(c0, 500_000, PAR, 3_600, 3_600).to_i128();
        let btc = accrue_index_delta(c0, 500_000, 5_000_000_000_000, 3_600, 3_600).to_i128();
        let cheap = accrue_index_delta(c0, 500_000, 10_000_000, 3_600, 3_600).to_i128();
        assert_eq!(at_par, 500_000);
        assert_eq!(btc, 250_000_000);
        assert_eq!(cheap, 500);
        // A missing mark accrues nothing rather than guessing a price.
        assert_eq!(accrue_index_delta(c0, 500_000, 0, 3_600, 3_600), c0);
    }

    #[test]
    fn one_btc_long_pays_notional_times_rate() {
        // The end-to-end unit check: 1 BTC (1e8 contract units) long at
        // $50,000, 0.05% for one interval, settles as qty * Δindex / 1e9
        // micro-USD = $25, which is notional ($50,000) times the rate.
        let d = accrue_index_delta(Signed::ZERO, 500_000, 5_000_000_000_000, 3_600, 3_600);
        let owed_micro = 100_000_000i128 * d.to_i128() / RATE_DEN_Q9 as i128;
        assert_eq!(owed_micro, 25_000_000);
    }

    #[test]
    fn accrue_zero_blocks_is_noop() {
        let c0 = Signed::from_i128(123_456);
        let c1 = accrue_index_delta(c0, 1_000, PAR, 0, 1);
        assert_eq!(c1, c0);
    }

    #[test]
    fn accrue_positive_rate_grows_index() {
        let c0 = Signed::ZERO;
        // 100 q9 per block, 10 blocks -> +1_000.
        let c1 = accrue_index_delta(c0, 100, PAR, 10, 1);
        assert_eq!(c1.to_i128(), 1_000);
    }

    #[test]
    fn accrue_negative_rate_shrinks_index() {
        let c0 = Signed::from_i128(5_000);
        let c1 = accrue_index_delta(c0, -100, PAR, 10, 1);
        assert_eq!(c1.to_i128(), 4_000);
    }

    #[test]
    fn accrue_associative_modulo_rounding() {
        // accrue(accrue(c, r, a), r, b) == accrue(c, r, a+b)  for fixed rate.
        let c0 = Signed::from_i128(42);
        let r: i64 = 1_337;
        let combined = accrue_index_delta(c0, r, PAR, 13, 1);
        let stepped = accrue_index_delta(accrue_index_delta(c0, r, PAR, 5, 1), r, PAR, 8, 1);
        assert_eq!(combined, stepped);
    }

    #[test]
    fn accrue_pro_rates_the_interval_rate_over_the_blocks_elapsed() {
        // QA F12. The rate is per interval. A full interval accrues exactly
        // one rate; half an interval accrues half; the per-block reading —
        // one rate per block — is the 3,600x overcharge that drained the
        // insurance fund in a squeeze.
        let c0 = Signed::ZERO;
        assert_eq!(
            accrue_index_delta(c0, 5_000_000, PAR, 3_600, 3_600).to_i128(),
            5_000_000
        );
        assert_eq!(
            accrue_index_delta(c0, 5_000_000, PAR, 1_800, 3_600).to_i128(),
            2_500_000
        );
        assert_eq!(
            accrue_index_delta(c0, 5_000_000, PAR, 1, 3_600).to_i128(),
            1_388
        );
        assert_eq!(
            accrue_index_delta(c0, -5_000_000, PAR, 3_600, 3_600).to_i128(),
            -5_000_000
        );
        // A zero interval is treated as one, never a divide by zero.
        assert_eq!(accrue_index_delta(c0, 7, PAR, 3, 0).to_i128(), 21);
    }

    #[test]
    fn accrue_saturates_on_overflow() {
        // i64::MAX * u64::MAX happens to fit in i128 (2^63 * 2^64 = 2^127 < 2^127 + ...),
        // but pushing it any further must saturate, not wrap. Test the boundary
        // by stacking two saturating_adds.
        let c0 = Signed::from_i128(i128::MAX - 100);
        let r: i64 = 1_000;
        let blocks: u64 = 1_000;
        let c1 = accrue_index_delta(c0, r, PAR, blocks, 1);
        // Saturated to i128::MAX.
        assert_eq!(c1.to_i128(), i128::MAX);
    }

    #[test]
    fn accrue_saturates_on_negative_overflow() {
        let c0 = Signed::from_i128(i128::MIN + 100);
        let r: i64 = -1_000;
        let blocks: u64 = 1_000;
        let c1 = accrue_index_delta(c0, r, PAR, blocks, 1);
        // Saturated to i128::MIN. `Signed` used to round-trip i128::MIN to
        // i128::MIN + 1, because its negative arm saturated the magnitude
        // (2^127) at i128::MAX; `to_i128` is now exact there (see
        // `cb_types::signed::i128_min_round_trips_exactly`), so the floor is
        // the floor.
        assert_eq!(c1.to_i128(), i128::MIN);
    }

    // ---------- proptest: clamp invariants ----------

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1024))]

        #[test]
        fn funding_rate_always_within_outer_clamp(p in any::<i64>(), i in any::<i64>()) {
            let r = compute_funding_rate(p, i, FundingClamp::DEFAULT);
            prop_assert!(r >= -FundingClamp::DEFAULT.outer_q9);
            prop_assert!(r <=  FundingClamp::DEFAULT.outer_q9);
        }

        #[test]
        fn funding_rate_negation_symmetric(p in -1_000_000_000i64..1_000_000_000, i in -1_000_000_000i64..1_000_000_000) {
            let pos = compute_funding_rate(p, i, FundingClamp::DEFAULT);
            let neg = compute_funding_rate(-p, -i, FundingClamp::DEFAULT);
            prop_assert_eq!(pos, -neg);
        }

        /// Custom clamp: the same invariant must hold for any user-provided
        /// (inner, outer) where inner, outer are both non-negative.
        #[test]
        fn funding_rate_within_custom_outer(
            p in any::<i64>(),
            i in any::<i64>(),
            inner in 0i64..1_000_000,
            outer in 1i64..100_000_000,
        ) {
            // outer >= inner is not strictly required by the formula, but mirror
            // the protocol assumption to keep the test space realistic.
            let outer = outer.max(inner);
            let c = FundingClamp { inner_q9: inner, outer_q9: outer };
            let r = compute_funding_rate(p, i, c);
            prop_assert!(r >= -outer && r <= outer);
        }
    }
}
