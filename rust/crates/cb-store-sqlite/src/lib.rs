//! # cb-store-sqlite — the application database, on SQLite
//!
//! An implementation of [`cb_ports::AppDatabase`] and every repository trait
//! beneath it. Nothing above this crate names SQLite; a composition root
//! builds a [`SqliteDb`] once and hands out `&dyn UserRepository` and friends.
//!
//! ## Why SQLite holds application state
//!
//! The exchange keeps two stores on purpose. Consensus state — balances,
//! orders, positions — lives in RocksDB, where write throughput and
//! deterministic iteration order decide block time. Application state —
//! accounts, sessions, faucet grants, candles, audit trail — lives here,
//! where relational queries, foreign keys and versioned migrations matter and
//! consensus does not. SQLite gives all three in a single file with no
//! server, which is what makes the simulator something a person can clone and
//! run.
//!
//! What SQLite costs is concurrent writers: there is exactly one, whatever
//! the pool size. That is the right trade for a gateway whose write rate is
//! bounded by human joins and one candle per market per minute, and it is the
//! first thing that would push a deployment to PostgreSQL. When it does, the
//! seam is [`cb_ports::AppDatabase`] and nothing above has to change.
//!
//! ## Concurrency configuration, and why each part is needed
//!
//! * **WAL journal mode.** In the default rollback journal, a writer blocks
//!   every reader. Under WAL, readers see a consistent snapshot while the
//!   writer works, which is the whole shape of this workload: many gateway
//!   reads, one aggregation writer.
//! * **A busy timeout.** WAL still serialises writers. Without a timeout the
//!   second writer fails instantly with `SQLITE_BUSY`; with one it waits.
//!   Five seconds is long enough to absorb a candle flush and short enough
//!   that a genuinely stuck writer is reported rather than hidden. When it
//!   does elapse, the failure arrives as [`cb_ports::PortError::Contention`]
//!   — retryable, but back off.
//! * **`synchronous = NORMAL`.** Under WAL this risks losing the last
//!   transactions on an OS crash, not corruption. Application state is
//!   rebuildable from the chain; paying a full fsync per commit for it is not
//!   worth the write amplification.
//! * **Foreign keys on.** SQLite disables them per connection by default,
//!   which would make every `REFERENCES` clause in the schema decorative.
//!
//! ## Encodings
//!
//! Money, instants and counts each need a deliberate representation, because
//! SQLite has no type for any of them. Those decisions and their trade-offs
//! live in the [`codec`] module header. The short version: money is
//! normalised decimal TEXT, never REAL; instants are integer microseconds,
//! never RFC 3339 text.
//!
//! ## Error mapping
//!
//! Every sqlx failure is classified into the port's closed error enum, so a
//! caller can branch on `Conflict` versus `Unavailable` without knowing which
//! database is underneath. The table is in the [`error`] module header.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod codec;
mod error;
mod repos;
mod store;

pub use store::SqliteDb;
