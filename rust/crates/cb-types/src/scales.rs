//! Fixed-point scales: the conversions between human numbers and the integers
//! the chain stores.
//!
//! # The two quantity scales, and the hundred-fold error between them
//!
//! A quantity on this chain is an integer, and **which integer** depends on
//! whether it is a spot quantity or a derivative quantity:
//!
//! | | Scale | One AAPL share is… | One BTC is… |
//! |---|---|---|---|
//! | Spot `qty` | the asset's own `10^decimals()` | `1_000_000` | `100_000_000` |
//! | Perp / future `qty` | a fixed `10^`[`CONTRACT_QTY_DECIMALS`] | `100_000_000` | `100_000_000` |
//!
//! The two agree for BTC, because BTC happens to have 8 decimals, and for
//! nothing else. That coincidence is exactly what makes this dangerous: code
//! written and tested against BTC works, and then silently overstates an AAPL
//! spot balance by 100× the first time someone reuses it. SOL is off by 10× in
//! the other direction; ETH by 10^10.
//!
//! This module therefore exposes the two conversions under **deliberately
//! different names** — [`spot_qty_from_whole`] and [`contract_qty_from_whole`] —
//! rather than one function with a flag. A caller who has to type which one they
//! mean is a caller who had to think about it once.
//!
//! # The price scale
//!
//! Spot notional is computed by the executor as
//! `notional = price * qty / 10^`[`SPOT_NOTIONAL_DIVISOR_DECIMALS`], with `qty`
//! in the base asset's base units and `notional` in the quote asset's base
//! units. Solving for `price` gives the wire price scale:
//!
//! ```text
//! price_wire = price_human * 10^(quote_decimals + 8 - base_decimals)
//! ```
//!
//! which [`spot_price_scale_exp`] computes. It works out at `10^6` for BTC/USD,
//! `10^8` for every equity (see [`crate::ids::ASSET_SPECS`] for why the equity
//! decimals were chosen to land there), and `10^5` for SOL/USD.
//!
//! **ETH/USD is the exception and this module reports it as an error.** ETH's 18
//! decimals against the fixed `10^8` divisor give an exponent of `-4`: the wire
//! price of one ETH would be a fraction. That is a pre-existing wart of the
//! ETH-at-18-decimals choice, which is frozen in consensus state and cannot be
//! revisited here. Rather than hide it behind a rounding, [`spot_price_scale_exp`]
//! returns [`ScaleError::SubUnitPriceScale`] so any caller that tries to quote
//! ETH spot in human terms finds out. Every asset listed since has `decimals <= 8`
//! specifically so it cannot land in that hole.

use thiserror::Error;

use crate::amount::{self, DecimalError};
use crate::ids::{AssetId, MarketId, CONTRACT_QTY_DECIMALS};

/// The decimal exponent the spot executor divides by when turning
/// `price * qty` into a quote-asset notional (`cb_block::executor::spot_notional`).
///
/// A protocol constant, not a property of any asset: it is written into the
/// executor and therefore into consensus. Changing it would reprice every
/// resting order on the chain.
pub const SPOT_NOTIONAL_DIVISOR_DECIMALS: u8 = 8;

/// Why a scale conversion could not be performed.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ScaleError {
    /// The market's decimals make the wire price scale negative — one whole
    /// unit of base would cost less than one wire-price unit, so no human price
    /// can be represented. See the module docs on ETH.
    #[error(
        "{base}/{quote} has a sub-unit price scale (quote_decimals {quote_decimals} + \
         {divisor} - base_decimals {base_decimals} < 0); human prices are not \
         representable on this market"
    )]
    SubUnitPriceScale {
        /// Base asset of the market.
        base: AssetId,
        /// Quote asset of the market.
        quote: AssetId,
        /// `base.decimals()`.
        base_decimals: u8,
        /// `quote.decimals()`.
        quote_decimals: u8,
        /// [`SPOT_NOTIONAL_DIVISOR_DECIMALS`].
        divisor: u8,
    },
    /// The converted value does not fit in a `u128`.
    #[error("value overflows u128 at this scale")]
    Overflow,
    /// The underlying decimal codec rejected the input.
    #[error(transparent)]
    Decimal(#[from] DecimalError),
}

// ---------------------------------------------------------------------------
// Quantities
// ---------------------------------------------------------------------------

/// Convert a whole number of units of `asset` into **spot** base units.
///
/// `spot_qty_from_whole(AssetId::Aapl, 3) == Some(3_000_000)` (3 shares at 6
/// decimals). Use this for spot balances, spot order quantities and genesis
/// seeding — anything that lands in `AccountState`.
///
/// Returns `None` on overflow.
pub fn spot_qty_from_whole(asset: AssetId, whole: u128) -> Option<u128> {
    whole.checked_mul(asset.one_whole())
}

/// Convert a whole number of units of an underlying into **derivative contract**
/// units.
///
/// `contract_qty_from_whole(3) == Some(300_000_000)` — and note that it takes no
/// [`AssetId`], because the contract scale is the same for every underlying.
/// Use this for `Position::qty`, `PerpOrder::qty` and future order quantities.
///
/// Returns `None` on overflow.
pub fn contract_qty_from_whole(whole: u128) -> Option<u128> {
    whole.checked_mul(10u128.pow(CONTRACT_QTY_DECIMALS as u32))
}

/// Parse a human quantity string into **spot** base units for `asset`, rejecting
/// finer precision than the asset can hold.
///
/// This is the boundary guard: `"1.12345678"` against AAPL (6 decimals) is an
/// error, not a truncation. See [`crate::amount::parse_scaled`].
pub fn parse_spot_qty(asset: AssetId, s: &str) -> Result<u128, ScaleError> {
    Ok(amount::parse_scaled(s, asset.decimals())?)
}

/// Parse a human quantity string into **derivative contract** units.
///
/// Always uses [`CONTRACT_QTY_DECIMALS`], whatever the underlying.
pub fn parse_contract_qty(s: &str) -> Result<u128, ScaleError> {
    Ok(amount::parse_scaled(s, CONTRACT_QTY_DECIMALS)?)
}

/// Render **spot** base units of `asset` as a human string.
pub fn format_spot_qty(asset: AssetId, units: u128) -> Result<String, ScaleError> {
    Ok(amount::format_scaled(units, asset.decimals())?)
}

/// Render **derivative contract** units as a human string.
pub fn format_contract_qty(units: u128) -> Result<String, ScaleError> {
    Ok(amount::format_scaled(units, CONTRACT_QTY_DECIMALS)?)
}

// ---------------------------------------------------------------------------
// Prices
// ---------------------------------------------------------------------------

/// Decimal exponent of a spot market's wire price: `quote_decimals + 8 - base_decimals`.
///
/// See the module docs for the derivation and for why ETH/USD errors.
pub fn spot_price_scale_exp(base: AssetId, quote: AssetId) -> Result<u8, ScaleError> {
    let base_decimals = base.decimals();
    let quote_decimals = quote.decimals();
    let numerator = quote_decimals as i32 + SPOT_NOTIONAL_DIVISOR_DECIMALS as i32;
    let exp = numerator - base_decimals as i32;
    if exp < 0 {
        return Err(ScaleError::SubUnitPriceScale {
            base,
            quote,
            base_decimals,
            quote_decimals,
            divisor: SPOT_NOTIONAL_DIVISOR_DECIMALS,
        });
    }
    Ok(exp as u8)
}

/// Same as [`spot_price_scale_exp`], for a listed market.
pub fn market_price_scale_exp(market: MarketId) -> Result<u8, ScaleError> {
    spot_price_scale_exp(market.base(), market.quote())
}

/// `10^`[`spot_price_scale_exp`] — the multiplier from a human price to a wire
/// price on this market.
pub fn market_price_scale(market: MarketId) -> Result<u128, ScaleError> {
    Ok(amount::pow10(market_price_scale_exp(market)?)?)
}

/// Parse a human price (quote-whole per base-whole, e.g. `"190.25"` for AAPL)
/// into this market's wire price, rejecting excess precision.
pub fn parse_spot_price(market: MarketId, s: &str) -> Result<u128, ScaleError> {
    Ok(amount::parse_scaled(s, market_price_scale_exp(market)?)?)
}

/// Render a market's wire price as a human price string.
pub fn format_spot_price(market: MarketId, wire: u128) -> Result<String, ScaleError> {
    Ok(amount::format_scaled(
        wire,
        market_price_scale_exp(market)?,
    )?)
}

/// Parse a human price into the `q8` scale used by every oracle, mark and
/// derivative price on the chain (`OraclePrice::px_q8`, `PerpOrder::price_q8`).
///
/// Unlike [`parse_spot_price`] this does not depend on the underlying: `q8` is a
/// flat quote-per-base scale.
pub fn parse_price_q8(s: &str) -> Result<u128, ScaleError> {
    Ok(amount::parse_scaled(s, 8)?)
}

/// Render a `q8` price as a human price string.
pub fn format_price_q8(px_q8: u128) -> Result<String, ScaleError> {
    Ok(amount::format_scaled(px_q8, 8)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------- the two quantity scales ----------------

    #[test]
    fn spot_and_contract_scales_agree_for_btc_only() {
        // BTC is the coincidence that hides the bug.
        assert_eq!(
            spot_qty_from_whole(AssetId::Btc, 1),
            contract_qty_from_whole(1)
        );
        // Every other asset disagrees, and by how much is the size of the bug
        // you would ship by reusing the BTC code path.
        assert_ne!(
            spot_qty_from_whole(AssetId::Aapl, 1),
            contract_qty_from_whole(1)
        );
        assert_ne!(
            spot_qty_from_whole(AssetId::Sol, 1),
            contract_qty_from_whole(1)
        );
        assert_ne!(
            spot_qty_from_whole(AssetId::Eth, 1),
            contract_qty_from_whole(1)
        );
    }

    #[test]
    fn the_equity_mismatch_is_exactly_one_hundred_fold() {
        // Stated as a literal so the magnitude is impossible to misread.
        let one_share_spot = spot_qty_from_whole(AssetId::Aapl, 1).unwrap();
        let one_share_contract = contract_qty_from_whole(1).unwrap();
        assert_eq!(one_share_spot, 1_000_000);
        assert_eq!(one_share_contract, 100_000_000);
        assert_eq!(one_share_contract / one_share_spot, 100);
    }

    #[test]
    fn spot_qty_uses_each_assets_own_decimals() {
        assert_eq!(spot_qty_from_whole(AssetId::Btc, 2).unwrap(), 200_000_000);
        assert_eq!(spot_qty_from_whole(AssetId::Usd, 2).unwrap(), 2_000_000);
        assert_eq!(spot_qty_from_whole(AssetId::Sol, 2).unwrap(), 2_000_000_000);
        assert_eq!(spot_qty_from_whole(AssetId::Tsla, 2).unwrap(), 2_000_000);
        assert_eq!(
            spot_qty_from_whole(AssetId::Eth, 2).unwrap(),
            2_000_000_000_000_000_000
        );
    }

    #[test]
    fn contract_qty_is_asset_independent() {
        assert_eq!(contract_qty_from_whole(1).unwrap(), 100_000_000);
        assert_eq!(contract_qty_from_whole(7).unwrap(), 700_000_000);
    }

    #[test]
    fn quantity_overflow_returns_none() {
        assert_eq!(spot_qty_from_whole(AssetId::Eth, u128::MAX), None);
        assert_eq!(contract_qty_from_whole(u128::MAX), None);
    }

    // ---------------- quantity parsing ----------------

    #[test]
    fn spot_qty_parses_at_the_assets_own_precision() {
        assert_eq!(parse_spot_qty(AssetId::Aapl, "1.5").unwrap(), 1_500_000);
        assert_eq!(parse_spot_qty(AssetId::Btc, "1.5").unwrap(), 150_000_000);
        assert_eq!(parse_spot_qty(AssetId::Sol, "1.5").unwrap(), 1_500_000_000);
    }

    #[test]
    fn spot_qty_rejects_precision_the_asset_cannot_hold() {
        // A BTC-shaped 8-decimal quantity offered to a 6-decimal equity.
        let err = parse_spot_qty(AssetId::Aapl, "1.12345678").unwrap_err();
        assert!(
            matches!(
                err,
                ScaleError::Decimal(DecimalError::TooManyDecimals { .. })
            ),
            "expected TooManyDecimals, got {err:?}"
        );
        // The same string is fine on BTC, which is the whole point.
        assert_eq!(
            parse_spot_qty(AssetId::Btc, "1.12345678").unwrap(),
            112_345_678
        );
    }

    #[test]
    fn quantity_round_trips_on_both_scales() {
        for asset in AssetId::ALL {
            let units = spot_qty_from_whole(asset, 3).unwrap() + 1;
            let text = format_spot_qty(asset, units).unwrap();
            assert_eq!(parse_spot_qty(asset, &text).unwrap(), units, "{asset}");
        }
        let c = contract_qty_from_whole(3).unwrap() + 1;
        assert_eq!(
            parse_contract_qty(&format_contract_qty(c).unwrap()).unwrap(),
            c
        );
    }

    // ---------------- price scale ----------------

    #[test]
    fn price_scale_exponents_are_as_derived() {
        assert_eq!(market_price_scale_exp(MarketId::BtcUsd).unwrap(), 6);
        assert_eq!(market_price_scale_exp(MarketId::SolUsd).unwrap(), 5);
        for m in [
            MarketId::AaplUsd,
            MarketId::TslaUsd,
            MarketId::NvdaUsd,
            MarketId::SpyUsd,
        ] {
            assert_eq!(
                market_price_scale_exp(m).unwrap(),
                8,
                "{m} should share the q8 price scale"
            );
        }
    }

    #[test]
    fn eth_spot_price_scale_is_reported_not_rounded() {
        let err = market_price_scale_exp(MarketId::EthUsd).unwrap_err();
        match err {
            ScaleError::SubUnitPriceScale {
                base,
                base_decimals,
                quote_decimals,
                divisor,
                ..
            } => {
                assert_eq!(base, AssetId::Eth);
                assert_eq!(base_decimals, 18);
                assert_eq!(quote_decimals, 6);
                assert_eq!(divisor, 8);
            }
            other => panic!("expected SubUnitPriceScale, got {other:?}"),
        }
    }

    #[test]
    fn every_asset_listed_after_eth_avoids_the_sub_unit_hole() {
        // The guard that keeps the ETH wart from being repeated: any asset with
        // more than `quote_decimals + 8` decimals cannot be quoted. Assert that
        // every listed spot market except the known-bad ETH one is quotable.
        for m in MarketId::ALL {
            if m == MarketId::EthUsd {
                continue;
            }
            market_price_scale_exp(m)
                .unwrap_or_else(|e| panic!("{m} must have a representable price scale: {e}"));
        }
    }

    #[test]
    fn spot_prices_convert_to_the_expected_wire_integers() {
        // $50,000.00 per BTC.
        assert_eq!(
            parse_spot_price(MarketId::BtcUsd, "50000").unwrap(),
            50_000_000_000
        );
        // $190.25 per AAPL share.
        assert_eq!(
            parse_spot_price(MarketId::AaplUsd, "190.25").unwrap(),
            19_025_000_000
        );
        // $150.00 per SOL.
        assert_eq!(
            parse_spot_price(MarketId::SolUsd, "150").unwrap(),
            15_000_000
        );
    }

    #[test]
    fn wire_price_times_qty_lands_in_micro_usd() {
        // The executor computes `notional = price * qty / 1e8` in quote base
        // units. Reproduce it here so the scale derivation is checked against
        // the formula it was derived from, not just against itself.
        fn notional(price: u128, qty: u128) -> u128 {
            price * qty / 10u128.pow(SPOT_NOTIONAL_DIVISOR_DECIMALS as u32)
        }
        let micro = 1_000_000u128;

        // 1 BTC at $50,000 => $50,000 in micro-USD.
        let px = parse_spot_price(MarketId::BtcUsd, "50000").unwrap();
        let qty = spot_qty_from_whole(AssetId::Btc, 1).unwrap();
        assert_eq!(notional(px, qty), 50_000 * micro);

        // 10 AAPL at $190.25 => $1,902.50.
        let px = parse_spot_price(MarketId::AaplUsd, "190.25").unwrap();
        let qty = spot_qty_from_whole(AssetId::Aapl, 10).unwrap();
        assert_eq!(notional(px, qty), 1_902_500_000);
        assert_eq!(notional(px, qty), 1_902 * micro + 500_000);

        // 4 SOL at $150 => $600.
        let px = parse_spot_price(MarketId::SolUsd, "150").unwrap();
        let qty = spot_qty_from_whole(AssetId::Sol, 4).unwrap();
        assert_eq!(notional(px, qty), 600 * micro);
    }

    #[test]
    fn spot_price_round_trips() {
        for m in MarketId::ALL {
            if m == MarketId::EthUsd {
                continue;
            }
            for text in ["1", "0.5", "123.25", "99999"] {
                let wire = match parse_spot_price(m, text) {
                    Ok(w) => w,
                    // SOL's 5-decimal price scale cannot hold "0.5"? It can —
                    // but keep the loop honest if a future listing narrows it.
                    Err(ScaleError::Decimal(DecimalError::TooManyDecimals { .. })) => continue,
                    Err(e) => panic!("{m} price `{text}`: {e}"),
                };
                assert_eq!(format_spot_price(m, wire).unwrap(), text, "{m} `{text}`");
            }
        }
    }

    #[test]
    fn q8_prices_round_trip() {
        assert_eq!(parse_price_q8("50000").unwrap(), 50_000 * 100_000_000);
        assert_eq!(parse_price_q8("3000.5").unwrap(), 300_050_000_000);
        assert_eq!(format_price_q8(300_050_000_000).unwrap(), "3000.5");
        assert!(parse_price_q8("1.123456789").is_err());
    }
}
