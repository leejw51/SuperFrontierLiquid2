//! SuperFrontierLiquid REPL/CLI client.
//!
//! Speaks the same Cap'n Proto RPC as `cb-cli`/`cb-sim`, but layered with a
//! tiny wallet store under `~/.cb_client/` so an operator can iterate at a
//! prompt without hand-managing keys.

pub mod cli;
pub mod client;
pub mod derivs;
pub mod faucet;
pub mod fmt;
pub mod prompts;
pub mod repl;
pub mod spot;
pub mod store;
pub mod wallet;

pub use cli::{Cli, Command};
pub use prompts::{Prompts, ScriptedPrompts, StdPrompts};
pub use store::Store;
pub use wallet::WalletEntry;
