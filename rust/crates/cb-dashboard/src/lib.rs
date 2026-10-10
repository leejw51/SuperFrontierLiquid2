//! SuperFrontierLiquid HTTP dashboard.

pub mod metrics;
pub mod perps;
pub mod server;
pub mod templates;

pub use metrics::{Metrics, MetricsSnapshot};
pub use perps::{
    FundingRow, InsuranceSnapshot, LiquidationRow, OracleRow, PerpsPanels, PositionRow,
};
pub use server::{serve, PruningInfo};
pub use templates::{IndexData, PerpSummary};
