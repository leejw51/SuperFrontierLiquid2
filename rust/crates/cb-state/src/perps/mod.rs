//! Derivatives state column families.
//!
//! M1 ships the persistence shapes for the 9 CFs introduced by the perps
//! protocol design (`docs/design/perps-protocol.md`). The matching engine,
//! funding ticks, liquidation drain, and futures expiry — i.e. anything that
//! mutates these CFs as a side-effect of executing user txs — are all later
//! milestones (M2–M5). This module exposes only:
//!
//! - Canonical key encoders (so different milestones can't accidentally pick
//!   different layouts).
//! - Bincode value codecs (one per record type).
//! - `Get` / `Put` / `Delete` wrappers per CF, used in tests today and from
//!   the executor in M2+.
//!
//! Every wrapper uses `&[u8]` keys against `Database::{put,get,delete}` — no
//! trie mutation. The two trie-committed surfaces (positions trie and markets
//! trie) live in the parent `lib.rs` `compute_state_root` plumbing.

pub mod book_index;
pub mod funding_indices;
pub mod futures_markets;
pub mod keys;
pub mod liquidation_queue;
pub mod margin_accounts;
pub mod markets_trie;
pub mod oracle_state;
pub mod perp_markets;
pub mod perp_orders;
pub mod positions;

pub use book_index::PerpBookIndex;
pub use funding_indices::FundingIndices;
pub use futures_markets::FuturesMarkets;
pub use liquidation_queue::LiquidationQueue;
pub use margin_accounts::MarginAccounts;
pub use markets_trie::MarketsTrie;
pub use oracle_state::{
    OracleState, ORACLE_KIND_MARK, ORACLE_KIND_MARK_HISTORY, ORACLE_KIND_ORACLE,
};
pub use perp_markets::PerpMarkets;
pub use perp_orders::PerpOrders;
pub use positions::Positions;
