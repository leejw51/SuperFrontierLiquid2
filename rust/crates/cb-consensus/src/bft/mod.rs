//! Byzantine-fault-tolerant ordering: a chained HotStuff with a two-chain
//! commit rule and a timeout-certificate pacemaker. See `engine.rs` for the
//! protocol and `types.rs` for what goes over the wire.

pub mod engine;
pub mod sim;
pub mod types;

#[cfg(test)]
mod tests;

pub use engine::{Action, Committed, Config, Dropped, Engine, Ordered, Reputation, Root};
pub use types::*;
