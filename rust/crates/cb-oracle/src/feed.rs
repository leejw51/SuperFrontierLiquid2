//! Driving the feed, as opposed to reading it.
//!
//! [`cb_ports::MarketDataFeed`] is a read port: it answers "what is the price".
//! A *simulated* feed additionally has to be told to move, and can be told what
//! kind of market to be. Neither belongs on the port — a vendor API has no
//! `step()` and no `crash` scenario — so the extra verbs live behind this small
//! trait instead, implemented for `cb-feed`'s simulator and absent for anything
//! else.
//!
//! The practical consequence is that swapping in a real feed means passing a
//! [`Publisher`](crate::Publisher) with no driver: the oracle then reads prices
//! that move on their own and the scenario-control API reports that there is
//! nothing to control, rather than this crate needing to change.

use cb_feed::{Scenario, SimulatedFeed};

/// A market data source this process is responsible for advancing.
pub trait FeedDriver: Send + Sync {
    /// Advance every symbol by one tick.
    fn step(&self);

    /// Put every symbol into a scenario.
    fn set_scenario_all(&self, scenario: Scenario);

    /// Put one symbol into a scenario. Returns `false` for a symbol this feed
    /// does not carry, so a typo in a control request is reported rather than
    /// silently doing nothing.
    fn set_scenario(&self, symbol: &str, scenario: Scenario) -> bool;

    /// The scenario a symbol is currently in, if the feed carries it.
    fn scenario(&self, symbol: &str) -> Option<Scenario>;

    /// Every symbol this feed carries, for the control API's listing.
    fn symbols(&self) -> Vec<String>;
}

impl FeedDriver for SimulatedFeed {
    fn step(&self) {
        SimulatedFeed::step(self);
    }

    fn set_scenario_all(&self, scenario: Scenario) {
        SimulatedFeed::set_scenario_all(self, scenario);
    }

    fn set_scenario(&self, symbol: &str, scenario: Scenario) -> bool {
        SimulatedFeed::set_scenario(self, symbol, scenario).is_ok()
    }

    fn scenario(&self, symbol: &str) -> Option<Scenario> {
        SimulatedFeed::scenario(self, symbol).ok()
    }

    fn symbols(&self) -> Vec<String> {
        cb_feed::UNIVERSE
            .iter()
            .map(|s| s.symbol.to_string())
            .filter(|s| SimulatedFeed::scenario(self, s).is_ok())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stepping_moves_the_price_and_reading_does_not() {
        let feed = SimulatedFeed::new(7);
        let before = feed.raw_price("BTC-USD").expect("carried");
        assert_eq!(feed.raw_price("BTC-USD").expect("carried"), before);
        FeedDriver::step(&feed);
        assert_ne!(feed.raw_price("BTC-USD").expect("carried"), before);
    }

    #[test]
    fn an_unknown_symbol_is_reported_rather_than_ignored() {
        let feed = SimulatedFeed::new(7);
        assert!(FeedDriver::set_scenario(&feed, "BTC-USD", Scenario::Crash));
        assert!(!FeedDriver::set_scenario(
            &feed,
            "DOGE-USD",
            Scenario::Crash
        ));
        assert_eq!(
            FeedDriver::scenario(&feed, "BTC-USD"),
            Some(Scenario::Crash)
        );
    }

    #[test]
    fn symbols_lists_what_the_feed_carries() {
        let feed = SimulatedFeed::new(7);
        let symbols = FeedDriver::symbols(&feed);
        assert!(symbols.contains(&"BTC-USD".to_string()));
        assert_eq!(symbols.len(), cb_feed::UNIVERSE.len());
    }
}
