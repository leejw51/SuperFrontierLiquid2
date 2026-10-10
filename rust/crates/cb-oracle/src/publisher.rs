//! Turning prices into signed `OracleUpdate` transactions.
//!
//! This module is the part with no network in it. Given a feed, a market list,
//! a signing key and a height, it produces the transactions for one tick — and
//! every decision *not* to publish is made here, which is what makes those
//! decisions testable.
//!
//! # Scale
//!
//! `OraclePrice::px_q8` is quote-per-base times `1e8`, flat across every
//! market regardless of the underlying's own decimals. The conversion goes
//! through [`cb_types::scales::parse_price_q8`] rather than a local multiply
//! by `100_000_000`, because a factor-of-ten error here is invisible: it looks
//! like a market that moved, and it reaches margin and liquidation before
//! anyone reads a log. Using the chain's own parser means the oracle and the
//! order path cannot disagree about what "$64,000" is.
//!
//! # Refusing to publish
//!
//! A stale price presented as fresh is worse than no price. The chain has no
//! opinion about age — `cb-engine`'s freshness rule is purely "not an older
//! height than the last one" — so a republished old number is indistinguishable
//! on-chain from a real one, and liquidations would run against it. This
//! module therefore drops any tick older than the configured window, per
//! market, with the reason recorded so the caller can log it.
//!
//! Note the asymmetry: a missing price skips one market, not the venue. If
//! only SOL's feed has stalled there is no reason to stop marking BTC.

use crate::markets::OracleMarket;
use anyhow::{Context, Result};
use cb_crypto::SecretKey;
use cb_ports::clock::Clock;
use cb_ports::feed::{MarketDataFeed, PriceTick};
use cb_tx::{SignedTx, Transaction, TxBody};
use cb_types::{Address, DerivId};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// One market's update, ready to submit.
#[derive(Debug, Clone)]
pub struct PlannedUpdate {
    /// The market being priced.
    pub market: DerivId,
    /// The feed symbol the price came from.
    pub symbol: String,
    /// The price, at the chain's `q8` scale.
    pub px_q8: u128,
    /// The nonce this transaction carries.
    pub nonce: u64,
    /// The signed transaction.
    pub tx: SignedTx,
}

/// Why one market was not priced this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The feed returned no tick for this symbol.
    NoPrice,
    /// The tick was older than the freshness window.
    Stale {
        /// How old the observation was.
        age: Duration,
        /// The window it exceeded.
        limit: Duration,
    },
    /// The price could not be represented at the `q8` scale — zero, negative,
    /// or more precision than the scale carries.
    Unrepresentable(String),
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkipReason::NoPrice => f.write_str("the feed returned no price"),
            SkipReason::Stale { age, limit } => write!(
                f,
                "the price is {age:?} old, past the {limit:?} freshness window"
            ),
            SkipReason::Unrepresentable(why) => write!(f, "the price is unusable: {why}"),
        }
    }
}

/// A market that was not priced, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// The market.
    pub market: DerivId,
    /// The feed symbol that was consulted.
    pub symbol: String,
    /// Why nothing was published.
    pub reason: SkipReason,
}

/// One tick's worth of work.
#[derive(Debug, Clone, Default)]
pub struct PublishPlan {
    /// Transactions to submit, in order. Nonces are consecutive.
    pub updates: Vec<PlannedUpdate>,
    /// Markets deliberately left unpriced.
    pub skipped: Vec<Skipped>,
}

impl PublishPlan {
    /// Whether this tick publishes nothing at all.
    pub fn is_empty(&self) -> bool {
        self.updates.is_empty()
    }

    /// The nonce the next transaction after this plan should carry.
    pub fn next_nonce(&self, first_nonce: u64) -> u64 {
        first_nonce + self.updates.len() as u64
    }
}

/// Builds one tick of signed oracle updates.
pub struct Publisher {
    feed: Arc<dyn MarketDataFeed>,
    clock: Arc<dyn Clock>,
    key: SecretKey,
    authority: Address,
    max_price_age: Duration,
}

impl std::fmt::Debug for Publisher {
    /// The key is deliberately absent.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Publisher")
            .field("source", &self.feed.source_id())
            .field("authority", &self.authority)
            .field("max_price_age", &self.max_price_age)
            .finish()
    }
}

impl Publisher {
    /// Build a publisher over a feed.
    pub fn new(
        feed: Arc<dyn MarketDataFeed>,
        clock: Arc<dyn Clock>,
        key: SecretKey,
        max_price_age: Duration,
    ) -> Self {
        let public = cb_crypto::PublicKey::from_secret_key(&secp256k1::Secp256k1::new(), &key);
        let authority = cb_crypto::address_from_pubkey(&public);
        Self {
            feed,
            clock,
            key,
            authority,
            max_price_age,
        }
    }

    /// The address this publisher signs as.
    pub fn authority(&self) -> Address {
        self.authority
    }

    /// The feed's provenance id, recorded in logs alongside published prices.
    pub fn source_id(&self) -> &str {
        self.feed.source_id()
    }

    /// Ask the feed which symbols it can serve.
    pub async fn supported_symbols(&self) -> Result<Vec<String>> {
        self.feed
            .supported_symbols()
            .await
            .context("ask the market data feed for its symbols")
    }

    /// Build one tick's transactions.
    ///
    /// `first_nonce` is the nonce the authority account's next transaction must
    /// carry; successive updates take successive nonces, because the node's
    /// admission control tracks in-flight nonces per address and rejects a gap.
    /// `height` goes into every update's `submitted_height`.
    pub async fn plan(
        &self,
        markets: &[OracleMarket],
        chain_id: u32,
        height: u64,
        first_nonce: u64,
    ) -> Result<PublishPlan> {
        let symbols = dedup(markets);
        let ticks = self
            .feed
            .latest_batch(&symbols)
            .await
            .context("read the latest prices from the market data feed")?;
        let by_symbol: HashMap<&str, &PriceTick> =
            ticks.iter().map(|t| (t.symbol.as_str(), t)).collect();

        let now = self.clock.now();
        let mut plan = PublishPlan::default();
        let mut nonce = first_nonce;

        for market in markets {
            let Some(tick) = by_symbol.get(market.feed_symbol.as_str()) else {
                plan.skipped.push(Skipped {
                    market: market.market,
                    symbol: market.feed_symbol.clone(),
                    reason: SkipReason::NoPrice,
                });
                continue;
            };

            // Age is measured against the clock port, not `Utc::now()`, so a
            // replay with a fixed clock makes the same freshness decisions the
            // live run made.
            let age = now.signed_duration_since(tick.at);
            if age > chrono::Duration::from_std(self.max_price_age).unwrap_or(chrono::Duration::MAX)
            {
                plan.skipped.push(Skipped {
                    market: market.market,
                    symbol: market.feed_symbol.clone(),
                    reason: SkipReason::Stale {
                        age: age.to_std().unwrap_or(Duration::MAX),
                        limit: self.max_price_age,
                    },
                });
                continue;
            }

            let px_q8 = match to_q8(tick.price) {
                Ok(px) => px,
                Err(why) => {
                    plan.skipped.push(Skipped {
                        market: market.market,
                        symbol: market.feed_symbol.clone(),
                        reason: SkipReason::Unrepresentable(why),
                    });
                    continue;
                }
            };

            let tx = Transaction {
                nonce,
                chain_id,
                body: TxBody::OracleUpdate {
                    market: market.market,
                    px_q8,
                    submitted_height: height,
                },
            };
            let signed = SignedTx::sign_with(&self.key, tx)
                .with_context(|| format!("sign an oracle update for {}", market.market))?;
            plan.updates.push(PlannedUpdate {
                market: market.market,
                symbol: market.feed_symbol.clone(),
                px_q8,
                nonce,
                tx: signed,
            });
            nonce += 1;
        }

        Ok(plan)
    }
}

/// The symbols a market list needs, each asked for once.
///
/// Three markets can share one underlying — BTC has a spot, a perpetual and a
/// dated future — and the port's batch call is a round trip we would rather
/// not pay three times for the same number.
fn dedup(markets: &[OracleMarket]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(markets.len());
    for m in markets {
        if !out.iter().any(|s| s == &m.feed_symbol) {
            out.push(m.feed_symbol.clone());
        }
    }
    out
}

/// Convert a feed price to the chain's `q8` scale.
///
/// Goes through the chain's own decimal parser, which refuses zero-width and
/// over-precise values rather than rounding them, so a feed that started
/// quoting nine decimal places is a loud failure instead of a silent
/// truncation.
fn to_q8(price: rust_decimal::Decimal) -> Result<u128, String> {
    if price <= rust_decimal::Decimal::ZERO {
        return Err(format!("{price} is not a positive price"));
    }
    // `normalize` drops trailing zeros: the feed quotes BTC to two decimals, so
    // `64000.00` would otherwise be handed over as written, which is fine, but
    // a feed quoting to nine would be refused for the right reason rather than
    // for having written `.000000000`.
    let text = price.normalize().to_string();
    cb_types::scales::parse_price_q8(&text).map_err(|e| format!("{text}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    #[test]
    fn whole_dollars_scale_by_exactly_1e8() {
        assert_eq!(to_q8(Decimal::from(64_000)).unwrap(), 6_400_000_000_000);
        assert_eq!(to_q8(Decimal::from(1)).unwrap(), 100_000_000);
    }

    #[test]
    fn cents_and_thousandths_survive_the_conversion() {
        assert_eq!(
            to_q8(Decimal::from_str("3200.57").unwrap()).unwrap(),
            320_057_000_000
        );
        assert_eq!(
            to_q8(Decimal::from_str("145.125").unwrap()).unwrap(),
            14_512_500_000
        );
    }

    #[test]
    fn trailing_zeros_do_not_change_the_value() {
        assert_eq!(
            to_q8(Decimal::from_str("64000.00").unwrap()).unwrap(),
            to_q8(Decimal::from(64_000)).unwrap()
        );
    }

    #[test]
    fn non_positive_prices_are_refused() {
        assert!(to_q8(Decimal::ZERO).is_err());
        assert!(to_q8(Decimal::from(-1)).is_err());
    }

    #[test]
    fn over_precise_prices_are_refused_rather_than_rounded() {
        // Nine decimal places against a scale that carries eight.
        assert!(to_q8(Decimal::from_str("1.123456789").unwrap()).is_err());
    }

    #[test]
    fn one_symbol_is_requested_once_however_many_markets_share_it() {
        use cb_types::{AssetId, DerivId};
        let markets = vec![
            OracleMarket::new(DerivId::perp(0), AssetId::Btc, Address([1; 20])),
            OracleMarket::new(DerivId::future(1), AssetId::Btc, Address([1; 20])),
            OracleMarket::new(DerivId::perp(1), AssetId::Eth, Address([1; 20])),
        ];
        assert_eq!(dedup(&markets), vec!["BTC-USD", "ETH-USD"]);
    }

    // ---------------- the q8 conversion, over generated prices ----------------

    mod properties {
        use super::super::to_q8;
        use proptest::prelude::*;
        use rust_decimal::Decimal;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(2048))]

            /// A positive price with at most eight decimals converts exactly:
            /// the mantissa shifted to the q8 scale, no rounding.
            #[test]
            fn a_price_the_scale_can_hold_converts_exactly(m in 1i64..=i64::MAX, scale in 0u32..=8) {
                let price = Decimal::new(m, scale);
                let expected = m as u128 * 10u128.pow(8 - scale);
                prop_assert_eq!(to_q8(price).unwrap(), expected);
            }

            /// Trailing zeros beyond the eighth place are not precision, and
            /// do not cause a refusal.
            #[test]
            fn trailing_zeros_past_the_scale_are_harmless(m in 1i64..=1_000_000_000_000, extra in 1u32..=10) {
                let exact = Decimal::new(m, 8);
                let padded = Decimal::from_i128_with_scale(m as i128 * 10i128.pow(extra), 8 + extra);
                prop_assert_eq!(padded, exact);
                prop_assert_eq!(to_q8(padded).unwrap(), to_q8(exact).unwrap());
            }

            /// A ninth significant decimal is refused, never rounded: the chain
            /// would otherwise mark positions at a price nobody quoted.
            #[test]
            fn a_ninth_significant_decimal_is_refused(m in 1i64..=i64::MAX, scale in 9u32..=18) {
                let price = Decimal::new(m, scale);
                prop_assume!(price.normalize().scale() > 8);
                prop_assert!(to_q8(price).is_err(), "{} converted", price);
            }

            #[test]
            fn zero_and_negative_prices_are_refused(m in i64::MIN..=0, scale in 0u32..=28) {
                prop_assert!(to_q8(Decimal::new(m, scale)).is_err());
            }

            /// Any decimal at all: an error or an exact conversion, never a
            /// panic, and whatever converts divides back to the same price.
            #[test]
            fn any_decimal_converts_exactly_or_is_refused(
                lo in any::<u32>(), mid in any::<u32>(), hi in any::<u32>(),
                negative in any::<bool>(), scale in 0u32..=28,
            ) {
                let price = Decimal::from_parts(lo, mid, hi, negative, scale);
                if let Ok(q8) = to_q8(price) {
                    // Compared as text: `q8` can exceed what a Decimal's
                    // 96-bit mantissa holds once shifted to scale 8.
                    prop_assert!(price > Decimal::ZERO);
                    let back = cb_types::scales::format_price_q8(q8).unwrap();
                    prop_assert_eq!(back, price.normalize().to_string());
                }
            }
        }
    }
}
