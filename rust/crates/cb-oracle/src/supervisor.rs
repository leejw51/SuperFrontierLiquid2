//! The run loop: connect, discover, publish, survive.
//!
//! # The contract this loop is written to
//!
//! *An oracle that dies when the node restarts is worse than no oracle*, because
//! the markets look alive and are not. So nothing here returns an error to the
//! caller on a transient failure. A dropped connection, a timeout, a node that
//! has not finished booting — each ends the current session, waits out a
//! backoff, and starts another. The only ways out are the tick budget
//! (`--max-ticks`, for tests and demos) and a signal.
//!
//! # Three pieces of state that have to be right
//!
//! **The market list** is re-discovered on every connect, not once at startup.
//! A dev node is usually restarted because its data directory was wiped, and a
//! wipe can mean a different genesis; a cached market list would then price
//! markets that no longer exist and miss the ones that do.
//!
//! **The nonce** is read from the chain once per session and advanced locally
//! after that. It has to be: the node's admission control tracks the nonce each
//! address's next transaction must carry, counting what is already in the
//! mempool, and rejects anything else. Eleven updates a tick therefore need
//! eleven consecutive nonces submitted in order. Re-reading `getAccount`
//! mid-burst would give the *chain's* nonce, which is behind by however many
//! updates are still in flight, so a resync is only ever done after standing
//! still for a full period — long enough at 100 ms blocks for the mempool to
//! drain.
//!
//! **The height** in `submitted_height` must never go backwards for a market,
//! or `cb-engine`'s freshness rule rejects the update as stale — permanently,
//! since every later update would be older still. The anchor is therefore taken
//! from the chain at session start (the highest `source_height` already stored,
//! and the height at which a mark was last computed) and advanced by at least
//! one per tick. Re-reading one market's mark height each tick keeps the value
//! tracking real block heights rather than drifting into a private counter,
//! which is what makes `source_height` readable as a block height by the
//! dashboard and by whoever is debugging at three in the morning.

use crate::feed::FeedDriver;
use crate::markets::{Discovery, OracleMarket};
use crate::node::{Connector, NodeLink, Submitted};
use crate::publisher::Publisher;
use anyhow::{Context, Result};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// Exponential backoff with a ceiling.
#[derive(Debug, Clone)]
pub struct Backoff {
    base: Duration,
    max: Duration,
    current: Duration,
}

impl Backoff {
    /// A backoff starting at `base` and doubling to at most `max`.
    pub fn new(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max,
            current: base,
        }
    }

    /// The next delay, then double.
    pub fn next_delay(&mut self) -> Duration {
        let d = self.current;
        self.current = (self.current * 2).min(self.max);
        d
    }

    /// Back to the floor, after a connection that worked.
    pub fn reset(&mut self) {
        self.current = self.base;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(
            Duration::from_millis(crate::config::DEFAULT_BACKOFF_MIN_MS),
            Duration::from_millis(crate::config::DEFAULT_BACKOFF_MAX_MS),
        )
    }
}

/// How long to run for.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunLimit {
    /// Stop after this many publish ticks. `None` runs until killed.
    pub max_ticks: Option<u64>,
}

impl RunLimit {
    /// A bounded run, for tests and demos.
    pub fn ticks(n: u64) -> Self {
        Self { max_ticks: Some(n) }
    }
}

/// Counters, shared with the control API's health endpoint.
#[derive(Debug, Default)]
pub struct Stats {
    ticks: AtomicU64,
    published: AtomicU64,
    skipped: AtomicU64,
    rejected: AtomicU64,
    sessions: AtomicU64,
    connected: AtomicBool,
    last_height: AtomicU64,
}

impl Stats {
    /// Publish ticks attempted.
    pub fn ticks(&self) -> u64 {
        self.ticks.load(Ordering::Relaxed)
    }
    /// Oracle updates accepted by the node.
    pub fn published(&self) -> u64 {
        self.published.load(Ordering::Relaxed)
    }
    /// Market-ticks deliberately not published, mostly stale prices.
    pub fn skipped(&self) -> u64 {
        self.skipped.load(Ordering::Relaxed)
    }
    /// Submissions the node refused.
    pub fn rejected(&self) -> u64 {
        self.rejected.load(Ordering::Relaxed)
    }
    /// Sessions established, so a reconnect is visible as a number.
    pub fn sessions(&self) -> u64 {
        self.sessions.load(Ordering::Relaxed)
    }
    /// Whether a session is live right now.
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
    /// The `submitted_height` most recently published.
    pub fn last_height(&self) -> u64 {
        self.last_height.load(Ordering::Relaxed)
    }
}

/// Configuration for one running oracle.
pub struct Supervisor {
    /// How to reach the node.
    pub connector: Rc<dyn Connector>,
    /// Builds the signed updates.
    pub publisher: Publisher,
    /// The feed to advance each tick, when the feed is a simulated one.
    pub driver: Option<Arc<dyn FeedDriver>>,
    /// Publish period.
    pub period: Duration,
    /// Chain id override; read from the node when `None`.
    pub chain_id: Option<u32>,
    /// Reconnect policy.
    pub backoff: Backoff,
    /// Shared counters.
    pub stats: Arc<Stats>,
}

impl Supervisor {
    /// Run until the tick budget is spent, or forever.
    ///
    /// Never returns an error for a node that is down, slow or restarting;
    /// the only errors that escape are ones no retry could fix.
    pub async fn run(&mut self, limit: RunLimit) -> Result<()> {
        loop {
            if limit.max_ticks.is_some_and(|n| self.stats.ticks() >= n) {
                return Ok(());
            }

            match self.connector.connect().await {
                Ok(link) => {
                    self.stats.sessions.fetch_add(1, Ordering::Relaxed);
                    self.stats.connected.store(true, Ordering::Relaxed);
                    self.backoff.reset();
                    info!(node = %self.connector.target(), "connected to the node");
                    let outcome = self.session(link.as_ref(), limit).await;
                    self.stats.connected.store(false, Ordering::Relaxed);
                    match outcome {
                        Ok(Stop::BudgetSpent) => return Ok(()),
                        Ok(Stop::SessionEnded) => {
                            warn!("the node session ended; reconnecting");
                        }
                        Err(e) => {
                            warn!(error = %format!("{e:#}"), "the node session failed; reconnecting");
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        node = %self.connector.target(),
                        error = %format!("{e:#}"),
                        "cannot reach the node; will retry"
                    );
                }
            }

            let delay = self.backoff.next_delay();
            tokio::time::sleep(delay).await;
        }
    }

    /// One session against one connection.
    async fn session(&self, link: &dyn NodeLink, limit: RunLimit) -> Result<Stop> {
        let chain_id = match self.chain_id {
            Some(id) => id,
            None => link.chain_id().await.context("read the chain id")?,
        };

        let markets = match self.discover(link).await? {
            Some(m) => m,
            None => {
                // Nothing to do on this chain with this key. Wait a period
                // before reconnecting so a misconfiguration produces one log
                // line per second rather than a spin.
                tokio::time::sleep(self.period).await;
                return Ok(Stop::SessionEnded);
            }
        };

        let mut nonce = link
            .account_nonce(&self.publisher.authority())
            .await
            .context("read the oracle authority's nonce")?;
        let mut height = self.height_anchor(link, &markets).await? + 1;

        info!(
            chain_id,
            markets = markets.len(),
            nonce,
            height,
            source = self.publisher.source_id(),
            period_ms = self.period.as_millis() as u64,
            "publishing oracle prices"
        );

        loop {
            if limit.max_ticks.is_some_and(|n| self.stats.ticks() >= n) {
                return Ok(Stop::BudgetSpent);
            }
            tokio::time::sleep(self.period).await;

            if let Some(driver) = &self.driver {
                driver.step();
            }
            self.stats.ticks.fetch_add(1, Ordering::Relaxed);

            // A feed failure is not a node failure. Tearing the session down
            // would reconnect to a healthy node and change nothing, so this
            // tick simply publishes nothing and the next one tries again.
            let plan = match self.publisher.plan(&markets, chain_id, height, nonce).await {
                Ok(plan) => plan,
                Err(e) => {
                    self.stats
                        .skipped
                        .fetch_add(markets.len() as u64, Ordering::Relaxed);
                    error!(
                        error = %format!("{e:#}"),
                        "the market data feed failed; publishing nothing this tick"
                    );
                    height = self.next_height(link, &markets, height).await?;
                    continue;
                }
            };

            for skip in &plan.skipped {
                self.stats.skipped.fetch_add(1, Ordering::Relaxed);
                warn!(
                    market = %skip.market,
                    symbol = %skip.symbol,
                    "not publishing: {}", skip.reason
                );
            }

            let mut resync = false;
            for update in &plan.updates {
                // Counted as each one lands, not at the end of the tick: a
                // connection that drops mid-burst must not erase the updates
                // that did make it, or the counters disagree with the chain
                // precisely when someone is reading them to find out what
                // happened.
                match link.submit(&update.tx).await? {
                    Submitted::Accepted => {
                        self.stats.published.fetch_add(1, Ordering::Relaxed);
                        self.stats.last_height.store(height, Ordering::Relaxed);
                        debug!(
                            market = %update.market,
                            symbol = %update.symbol,
                            px_q8 = update.px_q8,
                            height,
                            "published"
                        );
                    }
                    Submitted::Rejected { code, message } => {
                        self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                        error!(
                            market = %update.market,
                            code,
                            message,
                            "the node refused an oracle update; standing down for one \
                             period and re-reading the account"
                        );
                        // Abandoning the rest of the tick is deliberate: every
                        // remaining update carries a nonce the node has just
                        // told us is wrong, so submitting them would earn ten
                        // more rejections and leave the account no less stuck.
                        resync = true;
                        break;
                    }
                }
            }

            if resync {
                tokio::time::sleep(self.period).await;
                nonce = link
                    .account_nonce(&self.publisher.authority())
                    .await
                    .context("re-read the oracle authority's nonce")?;
                height = self.height_anchor(link, &markets).await? + 1;
                info!(nonce, height, "resynchronised with the chain");
                continue;
            }

            nonce = plan.next_nonce(nonce);
            height = self.next_height(link, &markets, height).await?;
        }
    }

    /// Discover the markets to price, or `None` when there are none.
    async fn discover(&self, link: &dyn NodeLink) -> Result<Option<Vec<OracleMarket>>> {
        let mut discovery: Discovery = link
            .discover(self.publisher.authority())
            .await
            .context("list the chain's derivative markets")?;
        let supported = self.publisher.supported_symbols().await?;
        discovery.restrict_to_feed(&supported);

        for excluded in &discovery.excluded {
            warn!("skipping a market: {excluded}");
        }

        if discovery.markets.is_empty() {
            error!(
                authority = %format!("0x{}", hex::encode(self.publisher.authority().0)),
                "this node lists no derivative market this key may price — check \
                 ORACLE_AUTHORITY_KEY against the chain's genesis"
            );
            return Ok(None);
        }
        Ok(Some(discovery.markets))
    }

    /// The highest height the chain has already seen for these markets.
    ///
    /// Both sources matter. `source_height` is what the freshness rule compares
    /// against, so it is the floor a restart must clear. `computed_at_height`
    /// is a real block height, which keeps the published value meaningful
    /// rather than merely monotonic.
    async fn height_anchor(&self, link: &dyn NodeLink, markets: &[OracleMarket]) -> Result<u64> {
        let mut anchor = 0u64;
        for m in markets {
            if let Some(h) = link.oracle_height(&m.market).await? {
                anchor = anchor.max(h);
            }
        }
        if let Some(first) = markets.first() {
            if let Some(h) = link.mark_height(&first.market).await? {
                anchor = anchor.max(h);
            }
        }
        Ok(anchor)
    }

    /// The height for the next tick.
    ///
    /// One more than the block the chain says it last recomputed a mark at,
    /// or this tick's height again if the chain has not moved. One extra RPC
    /// per tick against eleven submissions, which buys a `source_height` that
    /// is a block height rather than a count of publishes.
    ///
    /// It used to be "at least one more than this tick's", which drifted
    /// ahead of the chain whenever ticks were faster than blocks — and the
    /// executor now refuses an update whose height the chain has not reached,
    /// since without that bound a single bad post could make every later one
    /// "stale" for ever. Re-submitting at the same height is accepted, so a
    /// tick that lands before the previous one's block is not lost.
    async fn next_height(
        &self,
        link: &dyn NodeLink,
        markets: &[OracleMarket],
        current: u64,
    ) -> Result<u64> {
        let mut next = current;
        if let Some(first) = markets.first() {
            if let Some(h) = link.mark_height(&first.market).await? {
                next = next.max(h + 1);
            }
        }
        Ok(next)
    }
}

/// Why a session stopped.
enum Stop {
    /// The tick budget ran out; the caller should stop too.
    BudgetSpent,
    /// The session is over but the oracle is not.
    SessionEnded,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_then_caps() {
        let mut b = Backoff::new(Duration::from_millis(10), Duration::from_millis(40));
        assert_eq!(b.next_delay(), Duration::from_millis(10));
        assert_eq!(b.next_delay(), Duration::from_millis(20));
        assert_eq!(b.next_delay(), Duration::from_millis(40));
        assert_eq!(b.next_delay(), Duration::from_millis(40));
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_millis(10));
    }
}
