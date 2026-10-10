//! Stress-test simulator core: deterministic synthetic traders that exercise the
//! sequencer's RPC by submitting BTC/ETH "buy" and "sell" Transfer pairs.
//!
//! M7 also adds an in-process `perps_load` scenario that drives the executor
//! directly (no RPC, no real sequencer) for deterministic replay testing of
//! the full perps stack.

pub mod fee_stress;
pub mod market_collapse;
pub mod perp_shock;
pub mod perps_load;
pub mod runner;
pub mod traders;

pub use fee_stress::{run_fee_stress, FeeStressConfig, FeeStressMetrics};
pub use market_collapse::{
    run_market_collapse, EpochSnapshot, MarketCollapseConfig, MarketCollapseMetrics,
};
pub use perp_shock::{run_perp_shock, PerpShockConfig, PerpShockMetrics};
pub use perps_load::{run_perps_load, PerpsLoadConfig, PerpsLoadMetrics};
pub use runner::{run, RunConfig};
pub use traders::{Trader, TraderSet};
