//! # cb-gateway — the browser-facing translator
//!
//! The browser cannot speak Cap'n Proto RPC, and the chain does not know what
//! a user account is. This crate is both halves of that gap:
//!
//! * **Protocol translation.** JSON + WebSocket facing the browser, Cap'n
//!   Proto facing the sequencer. Money crosses the HTTP boundary as decimal
//!   *strings*; this crate owns every conversion to and from the on-chain
//!   fixed-point integers ([`decimal`], [`symbols`]).
//! * **Everything the chain does not own.** User accounts and custodial
//!   wallets ([`auth`]), sessions, the faucet ([`faucet`]), OHLCV aggregation
//!   ([`candles`]) and the leaderboard ([`leaderboard`]).
//!
//! ## Layering rule
//!
//! Nothing outside `main.rs` names a concrete backend. Handlers take
//! `Arc<dyn Trait>` from [`cb_ports`] and [`node::NodeClient`]; the
//! composition root decides what those are. That is the entire point of the
//! ports design, and it is what lets the whole test suite run with no
//! database and no sequencer.
//!
//! ## The `!Send` bridge
//!
//! `capnp-rpc` futures hold raw pointers into the RPC system and are `!Send`,
//! so they cannot live in a multi-threaded tokio task. [`node::bridge`] runs
//! them on a dedicated thread with a current-thread runtime and a `LocalSet`,
//! and exposes a plain `async fn` API over an mpsc channel of request
//! messages carrying oneshot reply channels. Everything above
//! [`node::NodeClient`] is ordinary `Send` async code.
//!
//! ## What is derived rather than read
//!
//! The node's RPC surface has no per-account order query, no fill ownership
//! and no perp trade stream. Open orders, fills and trades are therefore
//! *derived* in [`orders`] and [`marketdata`] from the block stream, the
//! spot market stream and periodic book snapshots. They are a best-effort
//! view, not consensus truth; the chain remains authoritative for balances
//! and positions, which are read directly.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(clippy::all)]

pub mod auth;
pub mod candles;
pub mod config;
pub mod decimal;
pub mod directory;
pub mod error;
pub mod faucet;
pub mod http;
pub mod leaderboard;
pub mod marketdata;
pub mod markets;
pub mod memory;
pub mod node;
pub mod orders;
pub mod ratelimit;
pub mod signer;
pub mod sim;
pub mod sim_process;
pub mod state;
pub mod symbols;
pub mod testing;
pub mod tracker;
pub mod watcher;
pub mod ws;

pub use config::Config;
pub use error::{ApiError, ApiResult, ErrorCode};
pub use state::AppState;
