//! # cb-oracle — the price service every derivative market needs
//!
//! Ten of the shipped chain's seventeen markets are derivatives: seven
//! perpetuals and three dated futures, plus whatever a later genesis adds.
//! Every one of them is priced off an **oracle price** posted by a registered
//! authority, and nothing else on the chain writes one. The
//! executor recomputes a market's mark price *only* when an `OracleUpdate`
//! lands (see `cb-block::executor::apply_oracle_update`), and mark price is
//! what margin, funding, liquidation and futures settlement are computed
//! from. Without a process posting those updates continuously the derivative
//! half of the exchange is inert: the ticker reads zero, no position can be
//! opened, funding never moves and a dated future cannot settle.
//!
//! This crate is that process. It runs beside a node, discovers the chain's
//! derivative markets over RPC, takes prices from the [`cb_ports::MarketDataFeed`]
//! port, and signs and submits one `OracleUpdate` per market per tick for as
//! long as it runs.
//!
//! ## What it deliberately is not
//!
//! It is **not a price model**. Prices come from `cb-feed`'s [`SimulatedFeed`],
//! the same seeded geometric Brownian motion the rest of the simulator uses,
//! reached through the `MarketDataFeed` port. A deployment that wanted real
//! prices would swap the implementation behind that port and change nothing
//! here. A second generator living in this crate would mean the oracle and the
//! simulator disagreed about what the market did, which is precisely the bug
//! the ports design exists to prevent.
//!
//! It is also **not a source of truth about which markets exist**. The market
//! list is discovered from the node with `listPerpMarkets` and
//! `listFutureMarkets` on every connect, so adding a market to genesis needs
//! no edit here, and a node restarted onto a different genesis is picked up
//! rather than silently mispriced.
//!
//! ## Layout
//!
//! * [`config`] — the command line and the cadence, with the reasoning.
//! * [`feed`] — driving a simulated source, which the read port does not cover.
//! * [`markets`] — a discovered market, and the underlying-to-feed-symbol map.
//! * [`publisher`] — the pure part: feed + market list + key → signed updates.
//!   Every refusal to publish is decided here and is unit-testable.
//! * [`node`] — the Cap'n Proto link, behind a trait so the reconnect policy
//!   can be tested without a node.
//! * [`supervisor`] — the run loop: reconnect with backoff, never exit.
//! * [`control`] — an optional HTTP surface for changing scenario at runtime.
//!
//! [`SimulatedFeed`]: cb_feed::SimulatedFeed

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod config;
pub mod control;
pub mod feed;
pub mod markets;
pub mod node;
pub mod publisher;
pub mod supervisor;

pub use config::{Cli, OracleConfig, DEFAULT_PERIOD_MS, STALENESS_PERIODS};
pub use feed::FeedDriver;
pub use markets::{feed_symbol, Discovery, Excluded, OracleMarket};
pub use publisher::{PlannedUpdate, PublishPlan, Publisher, SkipReason, Skipped};
pub use supervisor::{Backoff, RunLimit, Stats, Supervisor};
