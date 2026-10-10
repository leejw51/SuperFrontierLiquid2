//! # cb-feed — mocked market data for crypto and equities
//!
//! Implements two ports: [`cb_ports::MarketDataFeed`] as [`SimulatedFeed`],
//! and [`cb_ports::EquityReferenceData`] as [`StaticRefData`]. Both ship with
//! a fault-injecting decorator, [`FaultyFeed`], because a source that cannot
//! fail cannot test the code that handles failure.
//!
//! ## What "mocked" has to mean here
//!
//! This exchange marks perpetual positions against an external price, computes
//! funding against an index and settles dated futures at a reference. All
//! three depend on a price nobody on the exchange controls, and in this
//! project that price is simulated. A mock good enough to make the simulator
//! feel real therefore needs three properties beyond "returns a number":
//!
//! * **Reproducibility.** The same seed produces the same path, so a
//!   liquidation cascade can be replayed instead of merely reported. See the
//!   [`sim`] module header for the design decisions this forces.
//! * **Plausible relationships.** A mega-cap equity must not move like a small
//!   token, and a broad index must move least of all. The constants are named
//!   and their ordering is asserted, in [`universe`].
//! * **Regimes on demand.** Waiting for a crash to occur naturally is not a
//!   test strategy, so [`Scenario`] injects one — and a crash is an actual
//!   sharp drawdown, not merely louder noise.
//!
//! ## Layout
//!
//! * [`universe`] — the instruments and their parameters, plus [`Scenario`].
//! * [`sim`] — the price process.
//! * [`calendar`] — US equity session hours, including daylight saving.
//! * [`refdata`] — the reference-data port over the same universe.
//! * [`faulty`] — the fault-injecting decorator.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod calendar;
pub mod faulty;
pub mod refdata;
pub mod sim;
pub mod universe;

pub use faulty::{FaultyFeed, FeedFaults};
pub use refdata::StaticRefData;
pub use sim::{SimulatedFeed, DEFAULT_TICK};
pub use universe::{AssetClass, AssetSpec, Scenario, UNIVERSE};
