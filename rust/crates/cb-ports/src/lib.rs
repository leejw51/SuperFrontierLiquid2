//! # cb-ports — the replaceability seam
//!
//! Every dependency that a production deployment would want to swap lives
//! behind a trait defined in this crate: the application database, the message
//! bus, the key-value store, market data, reference data, custody and
//! identity.
//!
//! Two rules keep the seam honest.
//!
//! 1. **No crate above this one may name a concrete backend.** `cb-gateway`
//!    depends on [`AppDatabase`], never on SQLite. Wiring a concrete
//!    implementation happens exactly once, in a binary's composition root.
//! 2. **Nothing in this crate may depend on a backend.** `cb-ports` pulls in
//!    no database driver, no broker client and no HTTP client. If a type here
//!    needs something from a backend, the abstraction is wrong.
//!
//! Every trait is object-safe so that callers can hold `Arc<dyn Trait>` and
//! decide the implementation at runtime from configuration.
//!
//! ## Testing contract
//!
//! Each port ships a fault-injecting test double in the implementing crate.
//! A port is not considered done until its failure paths — not merely its
//! happy path — are exercised by a caller's test.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod bus;
pub mod clock;
pub mod custody;
pub mod db;
pub mod error;
pub mod feed;
pub mod identity;
pub mod kv;
pub mod models;
pub mod refdata;

pub use bus::{BusSubscription, EventBus, EventEnvelope, Topic};
pub use clock::{Clock, FixedClock, SystemClock};
pub use custody::{CustodyProvider, DepositIntent, WithdrawalReceipt};
pub use db::{
    AppDatabase, AuditRepository, CandleRepository, FaucetRepository, LeaderboardRepository,
    LedgerRepository, MarketMetaRepository, SessionRepository, UserRepository,
};
pub use error::{PortError, PortResult};
pub use feed::{FeedSubscription, MarketDataFeed, PriceTick};
pub use identity::{Identity, IdentityProvider};
pub use kv::{KvBatch, KvStore, WriteOp};
pub use models::{
    AuditRecord, Candle, CandleInterval, FaucetClaim, LeaderboardEntry, LeaderboardWindow,
    LedgerEntry, MarketMeta, Session, User,
};
pub use refdata::{EquityReferenceData, InstrumentRef};
