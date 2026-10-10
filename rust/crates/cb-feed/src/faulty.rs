//! A market-data source that misbehaves on purpose.
//!
//! Oracle-freshness logic is the reason this exists. An exchange that marks
//! positions against an external price has to decide what to do when that
//! price stops arriving: halt the market, widen margin, fall back to the last
//! book price. That decision is unreachable from a test as long as the only
//! available source is a simulator that never fails.
//!
//! The three faults map to the three ways a real feed lets you down.
//!
//! * **Staleness** — the connection is up and the data is old. The nastiest
//!   case, because nothing errors; the timestamps just stop moving. Injected
//!   by shifting each tick's `at` into the past, leaving everything else
//!   intact, which is exactly what a caller checking `now - at` must catch.
//! * **Unavailability** — the source is down and says so.
//! * **Rate limiting** — the source is up and refusing you specifically, with
//!   a `retry_after` the caller is expected to honour rather than hammer.
//!
//! Faults are deterministic ("every Nth call"), not probabilistic, so a test
//! that depends on one is reproducible.

use async_trait::async_trait;
use cb_ports::feed::{FeedSubscription, MarketDataFeed, PriceTick};
use cb_ports::{PortError, PortResult};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Fault configuration, shared with whoever holds the [`FaultyFeed`].
#[derive(Debug, Default)]
pub struct FeedFaults {
    stale_by_ms: AtomicU64,
    unavailable: AtomicBool,
    rate_limit_every_nth: AtomicU64,
    retry_after_ms: AtomicU64,
    calls: AtomicU64,
}

impl FeedFaults {
    /// No faults.
    pub fn healthy() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Report every tick as though it were observed `age` ago.
    ///
    /// The prices themselves are untouched: this models a feed that has
    /// stopped updating, which is what a freshness check exists to notice.
    pub fn set_staleness(&self, age: Duration) {
        self.stale_by_ms
            .store(age.as_millis() as u64, Ordering::Release);
    }

    /// Take the source down. Every call fails with
    /// [`PortError::Unavailable`] until [`Self::restore`].
    pub fn take_down(&self) {
        self.unavailable.store(true, Ordering::Release);
    }

    /// Bring the source back up.
    pub fn restore(&self) {
        self.unavailable.store(false, Ordering::Release);
    }

    /// Reject every `n`th call with [`PortError::RateLimited`]. `0` disables.
    pub fn rate_limit_every_nth(&self, n: u64, retry_after: Duration) {
        self.rate_limit_every_nth.store(n, Ordering::Release);
        self.retry_after_ms
            .store(retry_after.as_millis() as u64, Ordering::Release);
    }

    /// How many calls have been made through the decorator.
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Acquire)
    }

    /// Clear every fault and reset the call counter.
    pub fn reset(&self) {
        self.stale_by_ms.store(0, Ordering::Release);
        self.unavailable.store(false, Ordering::Release);
        self.rate_limit_every_nth.store(0, Ordering::Release);
        self.retry_after_ms.store(0, Ordering::Release);
        self.calls.store(0, Ordering::Release);
    }

    /// Decide whether this call is allowed through.
    fn gate(&self) -> PortResult<()> {
        if self.unavailable.load(Ordering::Acquire) {
            return Err(PortError::Unavailable(
                "injected fault: the market data source is down".into(),
            ));
        }

        let call = self.calls.fetch_add(1, Ordering::AcqRel) + 1;
        let nth = self.rate_limit_every_nth.load(Ordering::Acquire);
        if nth > 0 && call % nth == 0 {
            let retry_after_ms = self.retry_after_ms.load(Ordering::Acquire);
            return Err(PortError::RateLimited {
                retry_after: (retry_after_ms > 0).then(|| Duration::from_millis(retry_after_ms)),
            });
        }
        Ok(())
    }

    fn age(&self) -> chrono::Duration {
        chrono::Duration::milliseconds(self.stale_by_ms.load(Ordering::Acquire) as i64)
    }

    fn stale(&self, mut tick: PriceTick) -> PriceTick {
        tick.at -= self.age();
        tick
    }
}

/// A [`MarketDataFeed`] decorator that injects staleness, outages and rate
/// limits.
pub struct FaultyFeed<F: MarketDataFeed> {
    inner: F,
    faults: Arc<FeedFaults>,
}

impl<F: MarketDataFeed> FaultyFeed<F> {
    /// Wrap `inner`, sharing `faults` with the caller.
    pub fn new(inner: F, faults: Arc<FeedFaults>) -> Self {
        Self { inner, faults }
    }

    /// The fault configuration.
    pub fn faults(&self) -> &Arc<FeedFaults> {
        &self.faults
    }

    /// The wrapped feed.
    pub fn inner(&self) -> &F {
        &self.inner
    }
}

impl<F: MarketDataFeed + std::fmt::Debug> std::fmt::Debug for FaultyFeed<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FaultyFeed")
            .field("inner", &self.inner)
            .field("faults", &self.faults)
            .finish()
    }
}

#[async_trait]
impl<F: MarketDataFeed> MarketDataFeed for FaultyFeed<F> {
    fn source_id(&self) -> &str {
        self.inner.source_id()
    }

    async fn latest(&self, symbol: &str) -> PortResult<PriceTick> {
        self.faults.gate()?;
        Ok(self.faults.stale(self.inner.latest(symbol).await?))
    }

    async fn latest_batch(&self, symbols: &[String]) -> PortResult<Vec<PriceTick>> {
        self.faults.gate()?;
        Ok(self
            .inner
            .latest_batch(symbols)
            .await?
            .into_iter()
            .map(|t| self.faults.stale(t))
            .collect())
    }

    /// Staleness applies to streamed ticks too, and it applies at delivery
    /// time rather than subscription time — so a test can start a healthy
    /// stream and make it go stale halfway through, which is how a feed
    /// actually degrades.
    async fn subscribe(&self, symbols: &[String]) -> PortResult<Box<dyn FeedSubscription>> {
        self.faults.gate()?;
        Ok(Box::new(FaultySubscription {
            inner: self.inner.subscribe(symbols).await?,
            faults: Arc::clone(&self.faults),
        }))
    }

    async fn supported_symbols(&self) -> PortResult<Vec<String>> {
        self.faults.gate()?;
        self.inner.supported_symbols().await
    }

    async fn health_check(&self) -> PortResult<()> {
        self.faults.gate()?;
        self.inner.health_check().await
    }
}

struct FaultySubscription {
    inner: Box<dyn FeedSubscription>,
    faults: Arc<FeedFaults>,
}

#[async_trait]
impl FeedSubscription for FaultySubscription {
    async fn next(&mut self) -> PortResult<Option<PriceTick>> {
        self.faults.gate()?;
        Ok(self.inner.next().await?.map(|t| self.faults.stale(t)))
    }
}
