//! Persistent state for an SuperFrontierLiquid node.

pub mod account;
pub mod accounts_cf;
pub mod blocks_cf;
pub mod db;
pub mod markets_cf;
pub mod mempool_cf;
pub mod meta;
pub mod mirror_blocks_cf;
pub mod perps;
pub mod qc_cf;
pub mod spot;
pub mod state_hash;
pub mod validators_cf;

pub use account::{AccountState, BalanceError};
pub use accounts_cf::Accounts;
pub use blocks_cf::Blocks;
pub use db::{
    BufferedKv, CfItem, CfIter, CfKv, Database, DbBatch, DbError, DeferredWrite, ReachedFn, SealFn,
    Sealed, ALL_CFS, CF_ACCOUNTS, CF_BLOCKS, CF_BLOCK_HASH, CF_BOOK_INDEX, CF_FUNDING_INDICES,
    CF_FUTURES_MARKETS, CF_LIQUIDATION_QUEUE, CF_MARGIN_ACCOUNTS, CF_MARKETS, CF_MEMPOOL, CF_META,
    CF_MIRROR_BLOCKS, CF_ORACLE_STATE, CF_ORDERS, CF_PERP_BOOK_INDEX, CF_PERP_MARKETS,
    CF_PERP_ORDERS, CF_POSITIONS, CF_QC, CF_TRIE_NODES, CF_VALIDATORS,
};
pub use markets_cf::{MarketConfig, Markets};
pub use mempool_cf::Mempool;
pub use meta::{Meta, CONSENSUS_BFT, CONSENSUS_SEQUENCER};
pub use mirror_blocks_cf::MirrorBlocks;
pub use perps::{
    FundingIndices, FuturesMarkets, LiquidationQueue, MarginAccounts, MarketsTrie, OracleState,
    PerpBookIndex, PerpMarkets, PerpOrders, Positions, ORACLE_KIND_MARK, ORACLE_KIND_ORACLE,
};
pub use qc_cf::Qcs;
pub use spot::{BookIndex, Orders, SpotOrder};
pub use state_hash::{
    chain as chain_state_hash, diff_hash, genesis_state_hash, CfMap, DiffHasher, COMMITTED_CFS,
    STATE_HASH_SCHEME,
};
pub use validators_cf::{ValidatorInfo, Validators};

// The state commitment is a hash chain over each block's changes; see
// `state_hash`. It replaced `compute_state_root`, the hash of four merkle trie
// roots, which is gone with the tries it combined (ADR 0007).
