//! End-to-end harness: the real binaries, driven the way a browser drives them.
//!
//! # Why this crate exists
//!
//! Every other suite in this repository tests a subsystem against a *mock of
//! its neighbour*. `cb-node` is tested alone, `cb-gateway`'s integration tests
//! drive the real router over a scripted sequencer, and the frontend is tested
//! against fixtures. Each of those is green and each of them is worth having,
//! and together they still shipped five faults that a single manual run found
//! in ten minutes:
//!
//! 1. the genesis treasury was a vanity address nobody had a key for, so the
//!    faucet could never mint;
//! 2. a multi-asset faucet grant credited the chain and then returned an error,
//!    because two assets in one grant collided on a primary key;
//! 3. a derivative market no oracle had priced made the ticker return 500,
//!    which blanked the whole terminal;
//! 4. the websocket never connected, because the gateway serves `/ws` at the
//!    root and the frontend asked for `/api/v1/ws`;
//! 5. genesis defaulted to a file without the advertised markets, and
//!    referenced a variable the shipped environment never set.
//!
//! Not one of those is visible from inside a subsystem. They live in the seams:
//! between the shipped configuration and the shipped code, between the gateway
//! and a chain that has never seen an oracle price, between the path one side
//! serves and the path the other side asks for. The only test that sees a seam
//! is one that starts the actual binaries and behaves like a client.
//!
//! # What the harness guarantees
//!
//! * **No human setup.** [`Stack::start`] creates its own data directory,
//!   applies genesis, boots `cb-node` and `cb-gateway`, and waits until the
//!   chain is producing blocks. Nothing has to be running first.
//! * **No port collisions.** Every stack picks its own free ports, so two runs
//!   in parallel — or a run alongside a developer's `make up` — do not fight.
//! * **No leaks.** [`Stack`]'s `Drop` kills both children and removes the data
//!   directory, and `Drop` runs while a panic unwinds, so a failing assertion
//!   cleans up as thoroughly as a passing one.
//! * **No sleeping.** Synchronisation is [`poll_until`], a predicate with a
//!   deadline. A `sleep` that is long enough today is a flaky test tomorrow,
//!   and a flaky test gets deleted by whoever hits it, which is worse than
//!   having no test at all.
//! * **A loud failure.** A missing binary says which `cargo build` to run. A
//!   panicking test dumps the tail of both process logs, because "the gateway
//!   returned 500" is not a diagnosis.
//!
//! # Running it
//!
//! `make e2e` from the repository root, or:
//!
//! ```text
//! cargo build --release -p cb-node -p cb-gateway
//! cargo test -p cb-e2e -- --include-ignored --test-threads=2
//! ```
//!
//! Tests that boot a stack are `#[ignore]` because each one costs a few
//! seconds; the static checks over the shipped genesis, environment and
//! frontend source run in a plain `cargo test`. See `docs/testing.md`.

pub mod client;
pub mod flows;
pub mod journal;
pub mod money;
pub mod shipped;
pub mod stack;
pub mod wait;
pub mod ws;

pub use client::{Api, Reply, Session};
pub use journal::Journal;
pub use stack::{Stack, StackConfig};
pub use wait::{poll_until, poll_until_some, WaitError};
