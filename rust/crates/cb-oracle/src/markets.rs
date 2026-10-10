//! What a discovered market is, and how its underlying maps to a feed symbol.
//!
//! # Discovery, not a table
//!
//! The market list comes from the node — `listPerpMarkets` and
//! `listFutureMarkets` — on every connect, never from a constant in this
//! crate. Two things follow. Adding a market to `genesis-full.jsonl` needs no
//! edit here: the next connect finds it and starts pricing it. And a node
//! re-initialised onto a different genesis is picked up rather than being fed
//! prices for markets it does not have, which is the failure a hardcoded list
//! produces and which looks, from the outside, exactly like a working oracle.
//!
//! # The symbol map is one line, deliberately
//!
//! A derivative market record carries only its underlying [`AssetId`]. The
//! feed is keyed by symbol. The bridge is
//! `AssetId::spec().symbol` + `"-USD"` — `AssetId::Aapl` → `"AAPL-USD"` —
//! which reuses the authoritative asset table in `cb-types` rather than
//! introducing a second one. `cb-feed`'s universe is keyed by exactly those
//! strings, and `symbols_match_the_feeds_universe` below pins that they still
//! line up: a rename on either side fails a test rather than presenting as one
//! market that quietly never gets a price.

use cb_types::{Address, AssetId, DerivId};

/// A derivative market this oracle is responsible for pricing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleMarket {
    /// Chain identifier, as the `OracleUpdate` will name it.
    pub market: DerivId,
    /// The asset the contract is priced on.
    pub underlying: AssetId,
    /// The feed symbol carrying that asset's price.
    pub feed_symbol: String,
    /// The authority the chain will accept an update from. Compared against
    /// this process's own address before anything is signed.
    pub authority: Address,
}

impl OracleMarket {
    /// Build a market entry from what the node reported.
    pub fn new(market: DerivId, underlying: AssetId, authority: Address) -> Self {
        Self {
            market,
            underlying,
            feed_symbol: feed_symbol(underlying),
            authority,
        }
    }
}

/// The feed symbol carrying an asset's price.
///
/// Derived from `cb-types`' asset table so there is exactly one place that
/// decides what an asset is called.
///
/// # Equities are priced around the clock here
///
/// `AAPL-PERP` and its siblings are cash-settled synthetics on a venue that
/// never closes, so this oracle publishes for them at every tick regardless of
/// what `cb_feed::calendar` says about US market hours. That is a deliberate
/// simulator choice, not an oversight: a perpetual whose mark price freezes
/// overnight cannot be margined or liquidated overnight either, and positions
/// stay open across the session boundary. An oracle over a real equity feed
/// would instead hold the last close outside the session and say so, because
/// there the printed price genuinely stops existing.
pub fn feed_symbol(asset: AssetId) -> String {
    format!("{}-USD", asset.spec().symbol)
}

/// Why a market the node reported is not being priced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Excluded {
    /// The market's `oracle_authority` is some other address. On
    /// `genesis-perps.jsonl` the authority is split per market, so this is
    /// normal rather than an error — it just means somebody else's feed.
    NotOurs {
        /// The market in question.
        market: DerivId,
        /// Who the chain will accept updates from.
        authority: Address,
    },
    /// A dated future that has already settled. The executor answers
    /// `MarketExpired` to any further oracle post, so posting would be noise.
    Settled(DerivId),
    /// The node listed an underlying the configured feed does not carry.
    /// Surfaced loudly: this is what "added a market to genesis and the oracle
    /// silently ignored it" looks like from the inside.
    NoFeedSymbol {
        /// The market in question.
        market: DerivId,
        /// The symbol that was looked for.
        symbol: String,
    },
}

impl std::fmt::Display for Excluded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Excluded::NotOurs { market, authority } => write!(
                f,
                "{market}: authority is 0x{} — not this process's key",
                hex::encode(authority.0)
            ),
            Excluded::Settled(market) => write!(f, "{market}: already settled"),
            Excluded::NoFeedSymbol { market, symbol } => {
                write!(f, "{market}: the feed does not carry `{symbol}`")
            }
        }
    }
}

/// Everything one discovery round found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Discovery {
    /// Markets to publish for, in a stable order: perps by index, then futures.
    pub markets: Vec<OracleMarket>,
    /// Markets found but not priced, with the reason.
    pub excluded: Vec<Excluded>,
}

impl Discovery {
    /// Drop markets whose underlying the feed cannot serve, recording each.
    ///
    /// Called with the feed's own `supported_symbols`, so an oracle pointed at
    /// a narrow feed reports the gap instead of quietly leaving markets dead.
    pub fn restrict_to_feed(&mut self, supported: &[String]) {
        let mut kept = Vec::with_capacity(self.markets.len());
        for m in std::mem::take(&mut self.markets) {
            if supported.iter().any(|s| s == &m.feed_symbol) {
                kept.push(m);
            } else {
                self.excluded.push(Excluded::NoFeedSymbol {
                    market: m.market,
                    symbol: m.feed_symbol,
                });
            }
        }
        self.markets = kept;
    }

    /// The feed symbols this discovery needs, in market order.
    pub fn symbols(&self) -> Vec<String> {
        self.markets.iter().map(|m| m.feed_symbol.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(b: u8) -> Address {
        Address([b; 20])
    }

    #[test]
    fn symbols_match_the_feeds_universe() {
        // Every underlying that `cb_types::symbols` lists a derivative for
        // must resolve to a symbol `cb-feed` actually carries. A rename on
        // either side breaks here rather than in production, where it would
        // present as one market quietly never getting a price.
        let carried: Vec<&str> = cb_feed::UNIVERSE.iter().map(|s| s.symbol).collect();
        for spec in cb_types::symbols::PERPS.iter() {
            let s = feed_symbol(spec.underlying);
            assert!(carried.contains(&s.as_str()), "{} -> {s}", spec.symbol);
        }
        for spec in cb_types::symbols::FUTURES.iter() {
            let s = feed_symbol(spec.underlying);
            assert!(carried.contains(&s.as_str()), "{} -> {s}", spec.symbol);
        }
    }

    #[test]
    fn equity_underlyings_get_their_ticker_not_their_name() {
        assert_eq!(feed_symbol(AssetId::Aapl), "AAPL-USD");
        assert_eq!(feed_symbol(AssetId::Spy), "SPY-USD");
        assert_eq!(feed_symbol(AssetId::Btc), "BTC-USD");
    }

    #[test]
    fn a_market_the_feed_cannot_serve_is_reported_not_dropped() {
        let mut d = Discovery {
            markets: vec![
                OracleMarket::new(DerivId::perp(0), AssetId::Btc, addr(1)),
                OracleMarket::new(DerivId::perp(9), AssetId::Sol, addr(1)),
            ],
            excluded: vec![],
        };
        d.restrict_to_feed(&["BTC-USD".to_string()]);
        assert_eq!(d.markets.len(), 1);
        assert_eq!(
            d.excluded,
            vec![Excluded::NoFeedSymbol {
                market: DerivId::perp(9),
                symbol: "SOL-USD".into()
            }]
        );
    }
}
