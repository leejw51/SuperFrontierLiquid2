//! Spot CLOB column families.
//!
//! S1 ships persistence shapes only — no matching, no executor wiring (those
//! arrive in S2/S3 per `docs/pm/plan-client-trade.md`). This module mirrors
//! the layout of `crate::perps`:
//!
//! - `keys` — canonical hand-rolled key encoders. Spot reuses two existing
//!   CFs (`CF_ORDERS`, `CF_BOOK_INDEX`) instead of introducing new ones.
//! - `orders` — `SpotOrder` record + `Orders` wrapper around `CF_ORDERS`.
//! - `book_index` — `BookIndex` wrapper around `CF_BOOK_INDEX`, including the
//!   `top_n` aggregator the S4 RPC uses for depth-of-book responses.
//!
//! Like the perp orderbook, neither CF is committed to the trie — matching
//! state recomputes on replay (Hyperliquid-style). See
//! `compute_state_root` in `crate::lib` for the trie-input list.

pub mod book_index;
pub mod keys;
pub mod orders;

pub use book_index::BookIndex;
pub use orders::{Orders, SpotOrder};
