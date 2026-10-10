//! A bus that misbehaves on purpose.
//!
//! Every caller of [`EventBus`] has failure-handling code — a retry, a
//! reconnect, a circuit breaker — and that code is usually the least tested
//! part of the system, because the happy path is the only one a healthy
//! in-process bus ever takes. [`FaultyBus`] wraps any `EventBus` and makes the
//! unhappy paths reachable from a unit test.
//!
//! Faults are deterministic rather than random: "fail every third publish" is
//! reproducible and can be asserted on, where "fail 33% of publishes" turns a
//! test suite into a flake generator. Configuration lives behind an `Arc` so a
//! test can break the bus partway through a scenario and heal it again.

use async_trait::async_trait;
use cb_ports::bus::{BusSubscription, EventBus, Topic};
use cb_ports::{PortError, PortResult};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Fault configuration, shared with whoever holds the [`FaultyBus`].
///
/// All fields are atomics so a test can change them from another task while a
/// scenario runs, without taking a lock the bus would also need.
#[derive(Debug, Default)]
pub struct BusFaults {
    /// Fail every Nth publish. Zero disables the fault.
    fail_every_nth_publish: AtomicU64,
    /// Artificial delay added to every operation, in milliseconds.
    latency_ms: AtomicU64,
    /// When set, every operation fails with [`PortError::Unavailable`].
    disconnected: AtomicBool,
    /// Publishes attempted so far, which is what `fail_every_nth_publish`
    /// counts against.
    publish_attempts: AtomicU64,
}

impl BusFaults {
    /// No faults.
    pub fn healthy() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Fail every `n`th publish attempt. `0` disables the fault.
    pub fn fail_every_nth_publish(&self, n: u64) {
        self.fail_every_nth_publish.store(n, Ordering::Release);
    }

    /// Delay every operation by `latency`.
    ///
    /// Sub-millisecond values round down to zero; this exists to make a
    /// timeout fire, not to model network jitter faithfully.
    pub fn set_latency(&self, latency: Duration) {
        self.latency_ms
            .store(latency.as_millis() as u64, Ordering::Release);
    }

    /// Drop the connection: every operation fails until [`Self::reconnect`].
    pub fn disconnect(&self) {
        self.disconnected.store(true, Ordering::Release);
    }

    /// Restore the connection.
    pub fn reconnect(&self) {
        self.disconnected.store(false, Ordering::Release);
    }

    /// How many publishes have been attempted, successful or not.
    pub fn publish_attempts(&self) -> u64 {
        self.publish_attempts.load(Ordering::Acquire)
    }

    /// Clear every fault and reset the attempt counter.
    pub fn reset(&self) {
        self.fail_every_nth_publish.store(0, Ordering::Release);
        self.latency_ms.store(0, Ordering::Release);
        self.disconnected.store(false, Ordering::Release);
        self.publish_attempts.store(0, Ordering::Release);
    }

    async fn gate(&self, operation: &str) -> PortResult<()> {
        let latency = self.latency_ms.load(Ordering::Acquire);
        if latency > 0 {
            tokio::time::sleep(Duration::from_millis(latency)).await;
        }
        if self.disconnected.load(Ordering::Acquire) {
            return Err(PortError::Unavailable(format!(
                "injected fault: the bus connection is down ({operation})"
            )));
        }
        Ok(())
    }
}

/// An [`EventBus`] decorator that injects configurable failures.
///
/// Wrapping is generic over the inner bus so that the same decorator works
/// over [`crate::InMemoryBus`] today and over a real broker client later —
/// which matters, because a fault harness that only works against the mock
/// cannot be used to qualify the production wiring.
pub struct FaultyBus<B: EventBus> {
    inner: B,
    faults: Arc<BusFaults>,
}

impl<B: EventBus> FaultyBus<B> {
    /// Wrap `inner`, sharing `faults` with the caller.
    pub fn new(inner: B, faults: Arc<BusFaults>) -> Self {
        Self { inner, faults }
    }

    /// The fault configuration, for a caller that did not keep its own handle.
    pub fn faults(&self) -> &Arc<BusFaults> {
        &self.faults
    }

    /// The wrapped bus, for assertions about what actually got through.
    pub fn inner(&self) -> &B {
        &self.inner
    }
}

impl<B: EventBus + std::fmt::Debug> std::fmt::Debug for FaultyBus<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FaultyBus")
            .field("inner", &self.inner)
            .field("faults", &self.faults)
            .finish()
    }
}

#[async_trait]
impl<B: EventBus> EventBus for FaultyBus<B> {
    /// The counter increments even when the publish is about to fail, so that
    /// "every third attempt" means attempts and not successes. A caller
    /// retrying a failed publish therefore consumes its retry against the
    /// same sequence, which is the behaviour a broker would show.
    async fn publish(&self, topic: &Topic, key: &str, payload: Vec<u8>) -> PortResult<u64> {
        self.faults.gate("publish").await?;

        let attempt = self.faults.publish_attempts.fetch_add(1, Ordering::AcqRel) + 1;
        let nth = self.faults.fail_every_nth_publish.load(Ordering::Acquire);
        if nth > 0 && attempt % nth == 0 {
            return Err(PortError::Unavailable(format!(
                "injected fault: publish attempt {attempt} to `{topic}` rejected \
                 (every {nth}th publish fails)"
            )));
        }

        self.inner.publish(topic, key, payload).await
    }

    async fn subscribe(&self, topic: &Topic) -> PortResult<Box<dyn BusSubscription>> {
        self.faults.gate("subscribe").await?;
        self.inner.subscribe(topic).await
    }

    async fn subscribe_from(
        &self,
        topic: &Topic,
        offset: u64,
    ) -> PortResult<Box<dyn BusSubscription>> {
        self.faults.gate("subscribe_from").await?;
        self.inner.subscribe_from(topic, offset).await
    }

    async fn high_watermark(&self, topic: &Topic) -> PortResult<u64> {
        self.faults.gate("high_watermark").await?;
        self.inner.high_watermark(topic).await
    }

    async fn health_check(&self) -> PortResult<()> {
        self.faults.gate("health_check").await?;
        self.inner.health_check().await
    }
}
