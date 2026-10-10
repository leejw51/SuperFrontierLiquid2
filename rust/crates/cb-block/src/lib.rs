//! Block production and execution.

pub mod block;
pub mod executor;
pub mod header;
pub mod ordering;
pub mod state;

pub use block::{
    build_and_apply, build_and_apply_then, enforce_required_funding_ticks,
    enforce_required_futures_settle, execute_and_hand_off, execute_and_hand_off_with,
    execute_block, execute_block_with, required_system_txs, seal_block, synthetic_system_txs,
    verify_and_commit, verify_and_commit_with, Block, BlockOrigin, BuildResult, Executed,
    HandedOff, RejectedTx, SealChain, SealedBlock, VerifyError, MAX_BLOCK_TXS,
};
pub use executor::{
    apply_tx, apply_tx_at, consume_nonce, error_consumes_nonce, failure_consumes_nonce, ExecCtx,
    ExecError, MAX_MARK_AGE_BLOCKS,
};
pub use header::BlockHeader;
pub use ordering::order_for_block;
pub use state::{preload_state, BlockState, CommitRoots, PreloadStats, Savepoint};
