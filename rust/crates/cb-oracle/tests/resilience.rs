//! The node goes away. The oracle does not.
//!
//! The requirement is blunt: an oracle that dies when the node restarts is
//! worse than no oracle, because the markets look alive and are not. That is a
//! property of the run loop, not of the network, so it is tested against a
//! fake node that can be taken down and brought back deterministically — the
//! same argument `cb-gateway` makes for its own `Connector` trait. A test that
//! killed a real sequencer would be slower, flakier, and would still not be
//! able to assert on what happened during the outage.

use async_trait::async_trait;
use cb_feed::{SimulatedFeed, UNIVERSE};
use cb_oracle::feed::FeedDriver;
use cb_oracle::markets::{Discovery, OracleMarket};
use cb_oracle::node::{Connector, NodeLink, Submitted};
use cb_oracle::publisher::Publisher;
use cb_oracle::supervisor::{Backoff, RunLimit, Stats, Supervisor};
use cb_ports::clock::FixedClock;
use cb_tx::{SignedTx, TxBody};
use cb_types::{symbols, Address, DerivId};
use std::cell::RefCell;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

const AUTHORITY_KEY: &str = "0x943f7fb2f31e4f9a82252e317c82fb35408191017f64100b7ce1485d21e56730";
const CHAIN_ID: u32 = 1000;

fn key() -> cb_crypto::SecretKey {
    cb_oracle::config::parse_secret_key(AUTHORITY_KEY).expect("dev key parses")
}

fn authority() -> Address {
    let public = cb_crypto::PublicKey::from_secret_key(&secp256k1::Secp256k1::new(), &key());
    cb_crypto::address_from_pubkey(&public)
}

/// What the fake node recorded, shared between the fake and the assertions.
#[derive(Default)]
struct Ledger {
    /// Every accepted update, as `(market, px_q8, submitted_height, nonce)`.
    accepted: Vec<(DerivId, u128, u64, u64)>,
    /// Connection attempts made.
    connects: u32,
    /// `true` while the node is down.
    down: bool,
    /// Drop the link after this many more accepted submissions, simulating a
    /// node that dies mid-tick rather than between ticks.
    accept_before_death: Option<u32>,
    /// The nonce the fake's chain has reached.
    chain_nonce: u64,
}

impl Ledger {
    fn published_markets(&self) -> Vec<DerivId> {
        let mut ids: Vec<DerivId> = self.accepted.iter().map(|(m, ..)| *m).collect();
        ids.sort_by_key(|d| d.0);
        ids.dedup();
        ids
    }
}

struct FakeNode {
    ledger: Rc<RefCell<Ledger>>,
}

#[async_trait(?Send)]
impl NodeLink for FakeNode {
    async fn chain_id(&self) -> anyhow::Result<u32> {
        self.alive()?;
        Ok(CHAIN_ID)
    }

    async fn discover(&self, authority: Address) -> anyhow::Result<Discovery> {
        self.alive()?;
        Ok(Discovery {
            markets: symbols::PERPS
                .iter()
                .map(|p| OracleMarket::new(p.deriv_id(), p.underlying, authority))
                .chain(
                    symbols::FUTURES
                        .iter()
                        .map(|f| OracleMarket::new(f.deriv_id(), f.underlying, authority)),
                )
                .collect(),
            excluded: vec![],
        })
    }

    async fn account_nonce(&self, _addr: &Address) -> anyhow::Result<u64> {
        self.alive()?;
        Ok(self.ledger.borrow().chain_nonce)
    }

    async fn oracle_height(&self, market: &DerivId) -> anyhow::Result<Option<u64>> {
        self.alive()?;
        Ok(self
            .ledger
            .borrow()
            .accepted
            .iter()
            .filter(|(m, ..)| m == market)
            .map(|(_, _, h, _)| *h)
            .max())
    }

    async fn mark_height(&self, market: &DerivId) -> anyhow::Result<Option<u64>> {
        self.oracle_height(market).await
    }

    async fn submit(&self, tx: &SignedTx) -> anyhow::Result<Submitted> {
        self.alive()?;
        let sender = tx.clone().verify()?.sender;
        anyhow::ensure!(
            sender == authority(),
            "the fake node only accepts the authority"
        );

        let TxBody::OracleUpdate {
            market,
            px_q8,
            submitted_height,
        } = tx.tx.body
        else {
            anyhow::bail!("the oracle submitted something that is not an OracleUpdate");
        };

        let mut ledger = self.ledger.borrow_mut();
        anyhow::ensure!(
            tx.tx.nonce == ledger.chain_nonce,
            "nonce {} arrived when the chain expected {}",
            tx.tx.nonce,
            ledger.chain_nonce
        );
        // The chain's freshness rule: strictly older is refused.
        if let Some(prev) = ledger
            .accepted
            .iter()
            .filter(|(m, ..)| *m == market)
            .map(|(_, _, h, _)| *h)
            .max()
        {
            anyhow::ensure!(
                submitted_height >= prev,
                "{market}: height {submitted_height} is older than the stored {prev} — \
                 the chain would refuse this as stale"
            );
        }

        // The connection drops before this one is recorded, so it is lost
        // rather than half-applied: a transaction the node accepted but never
        // acknowledged is a different, nastier case than this test is about.
        if let Some(left) = ledger.accept_before_death.as_mut() {
            if *left == 0 {
                ledger.down = true;
                ledger.accept_before_death = None;
                drop(ledger);
                anyhow::bail!("the node went away");
            }
            *left -= 1;
        }

        ledger
            .accepted
            .push((market, px_q8, submitted_height, tx.tx.nonce));
        ledger.chain_nonce += 1;
        Ok(Submitted::Accepted)
    }
}

impl FakeNode {
    fn alive(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.ledger.borrow().down, "the node is down");
        Ok(())
    }
}

/// Connects only while the fake node is up, and counts the attempts.
struct FakeConnector {
    ledger: Rc<RefCell<Ledger>>,
    /// Bring the node back up on this connection attempt, so an outage has a
    /// definite length in attempts rather than in wall-clock time.
    revive_on_attempt: u32,
}

#[async_trait(?Send)]
impl Connector for FakeConnector {
    async fn connect(&self) -> anyhow::Result<Rc<dyn NodeLink>> {
        let attempt = {
            let mut l = self.ledger.borrow_mut();
            l.connects += 1;
            if l.connects >= self.revive_on_attempt {
                l.down = false;
            }
            l.connects
        };
        anyhow::ensure!(
            !self.ledger.borrow().down,
            "connection attempt {attempt} refused: the node is down"
        );
        Ok(Rc::new(FakeNode {
            ledger: self.ledger.clone(),
        }))
    }

    fn target(&self) -> String {
        "fake-node".into()
    }
}

fn supervisor(ledger: Rc<RefCell<Ledger>>, revive_on_attempt: u32) -> (Supervisor, Arc<Stats>) {
    let clock = Arc::new(FixedClock::new(
        chrono::DateTime::from_str("2026-01-01T00:00:00Z").expect("a valid instant"),
    ));
    let feed = Arc::new(SimulatedFeed::with_config(
        11,
        UNIVERSE,
        Duration::from_millis(50),
        clock.clone(),
    ));
    let stats = Arc::new(Stats::default());
    let driver: Arc<dyn FeedDriver> = feed.clone();
    (
        Supervisor {
            connector: Rc::new(FakeConnector {
                ledger,
                revive_on_attempt,
            }),
            publisher: Publisher::new(feed, clock, key(), Duration::from_secs(3600)),
            driver: Some(driver),
            period: Duration::from_millis(50),
            chain_id: None,
            backoff: Backoff::new(Duration::from_millis(10), Duration::from_millis(40)),
            stats: stats.clone(),
        },
        stats,
    )
}

#[tokio::test(start_paused = true)]
async fn publishing_resumes_after_the_node_goes_away_and_comes_back() {
    let ledger = Rc::new(RefCell::new(Ledger {
        // Die partway through the second tick: four updates in, which is
        // mid-burst, the awkward case where the local nonce is ahead of
        // nothing in particular.
        accept_before_death: Some(14),
        // The node is rebuilt while the oracle is retrying; the third
        // connection attempt after the outage finds it up.
        ..Ledger::default()
    }));
    let (mut supervisor, stats) = supervisor(ledger.clone(), 4);

    let local = tokio::task::LocalSet::new();
    let outcome = local
        .run_until(async move { supervisor.run(RunLimit::ticks(6)).await })
        .await;

    // The whole point: a node outage is not an oracle outage.
    outcome.expect("the oracle must not exit because the node went away");

    let ledger = ledger.borrow();
    assert!(
        ledger.connects >= 4,
        "the oracle should have retried the downed node, got {} attempts",
        ledger.connects
    );
    assert!(
        stats.sessions() >= 2,
        "publishing should have resumed in a second session, got {}",
        stats.sessions()
    );

    // Updates landed both before the outage and after it.
    let before: Vec<_> = ledger.accepted.iter().take(15).collect();
    assert_eq!(before.len(), 15, "the first tick and part of the second");
    assert!(
        ledger.accepted.len() > 15,
        "nothing was published after the node came back"
    );

    // And every market is being priced again, not just the ones that got in
    // before the node died.
    assert_eq!(ledger.published_markets().len(), 10);
    assert_eq!(stats.published(), ledger.accepted.len() as u64);
}

#[tokio::test(start_paused = true)]
async fn a_node_that_is_down_from_the_start_is_waited_for_not_given_up_on() {
    let ledger = Rc::new(RefCell::new(Ledger {
        down: true,
        ..Ledger::default()
    }));
    // Five refused attempts before it answers: `make up` starting the oracle
    // alongside a node that is still opening its database.
    let (mut supervisor, stats) = supervisor(ledger.clone(), 6);

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move { supervisor.run(RunLimit::ticks(2)).await })
        .await
        .expect("waiting for a node is not a failure");

    assert!(ledger.borrow().connects >= 6);
    assert_eq!(stats.published(), 20, "two ticks across ten markets");
}

#[tokio::test(start_paused = true)]
async fn heights_never_go_backwards_across_a_reconnect() {
    // The fake refuses an update whose height is older than the one it stored,
    // exactly as `cb_engine::oracle::check_freshness` does. If the oracle
    // restarted its height counter on reconnect, `submit` would return an
    // error, the session would end, and the run would never reach its tick
    // budget — so this passing is the assertion.
    let ledger = Rc::new(RefCell::new(Ledger {
        accept_before_death: Some(3),
        ..Ledger::default()
    }));
    let (mut supervisor, _stats) = supervisor(ledger.clone(), 3);

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move { supervisor.run(RunLimit::ticks(5)).await })
        .await
        .expect("the run survives");

    let ledger = ledger.borrow();
    for market in ledger.published_markets() {
        let heights: Vec<u64> = ledger
            .accepted
            .iter()
            .filter(|(m, ..)| *m == market)
            .map(|(_, _, h, _)| *h)
            .collect();
        assert!(
            heights.windows(2).all(|w| w[1] > w[0]),
            "{market} published heights {heights:?}, which is not strictly increasing"
        );
    }
}
