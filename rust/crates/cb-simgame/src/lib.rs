//! # cb-simgame — a living exchange that proves itself correct
//!
//! `cb-sim` answers "how fast?". This crate answers "is it *right*?".
//!
//! A population of bot traders with distinct, adversarial strategies trades
//! against each other (and, in networked mode, against any human who joins)
//! while a scenario engine drives the price feed through calm markets,
//! trends, crashes, squeezes and chop. After **every** block boundary — not
//! on a sample — the invariant checker re-derives all nine invariants from
//! `docs/PLAN.md` §6 out of raw state and fails loudly, with a report a
//! developer can act on, the moment one breaks.
//!
//! ## Why the pieces are shaped the way they are
//!
//! * **[`world`] drives the executor directly.** Signing each transaction
//!   immediately before applying it — rather than batching a block's worth
//!   of signatures up front — is what lets the [`bots::noise`] bot submit
//!   deliberately invalid orders without desynchronising every later
//!   transaction from the same sender. A rejected tx does not advance the
//!   sender's nonce, so the *next* signature must be produced after the
//!   rejection is known.
//! * **[`ledger`] tracks a mark-independent conserved quantity.** Comparing
//!   mark-to-market equity across blocks would drift with every price move
//!   and every per-position rounding step. The quantity in [`ledger`] is
//!   exact integer arithmetic that provably does not change across opens,
//!   closes, flips, funding, liquidation fees, bad-debt absorption or
//!   settlement — so any change at all is a leak.
//! * **[`invariants`] re-reads state rather than trusting a running total.**
//!   A checker that accumulates its own view of the world can agree with a
//!   buggy exchange. Every check here is computed from the column families.
//!
//! ## Run modes
//!
//! * **In-process** ([`world::World`]) — drives `cb-block`'s executor with no
//!   node and no network. Fast, hermetic, deterministic; this is the mode the
//!   test suite uses.
//! * **Networked** ([`net`]) — drives a running node over Cap'n Proto RPC,
//!   the way `cb-sim` does. This is the mode that makes the frontend feel
//!   alive.
//!
//! ## Determinism
//!
//! The whole run is seeded. Every source of choice — bot decisions, scenario
//! noise, bot iteration order — derives from that seed, and nothing in the
//! hot path iterates a `HashMap`. The same seed produces an identical
//! transaction sequence, identical fills and an identical per-block state
//! root; `tests/determinism.rs` is the crate's single most valuable test,
//! because it is what makes any other failure reproducible.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod bots;
pub mod feed;
pub mod invariants;
pub mod ledger;
pub mod net;
pub mod report;
pub mod scenario;
pub mod universe;
pub mod world;

pub use invariants::{Invariant, InvariantChecker, Violation};
pub use ledger::LedgerSnapshot;
pub use report::RunReport;
pub use scenario::{Scenario, ScenarioSegment, ScenarioTimeline};
pub use world::{BotCounts, World, WorldConfig};
