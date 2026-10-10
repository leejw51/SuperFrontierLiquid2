//! JSONL genesis loader + apply.

pub mod build;
pub mod env;
pub mod spec;

pub use build::apply;
pub use env::{apply_dotenv, parse_dotenv};
pub use spec::{
    expand_env_vars, load, ChainRecord, CommunityWalletRecord, CompanyRecord, FutureMarketRecord,
    GenesisRecord, GenesisSpec, MarketRecord, PerpMarketRecord, SimRecord, U128Str,
    ValidatorRecord,
};
