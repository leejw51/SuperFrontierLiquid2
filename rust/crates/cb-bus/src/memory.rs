//! The in-process log.

use async_trait::async_trait;
use cb_ports::bus::{BusSubscription, EventBus, EventEnvelope, Topic};
use cb_ports::clock::{Clock, SystemClock};
use cb_ports::{PortError, PortResult};
use dashmap::DashMap;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::watch;

/// Messages retained per topic when none is configured.
///
/// Ten thousand messages is roughly a minute of a busy market's trade feed —
/// long enough that a subscriber can reconnect after a transient network
/// failure and resume, short enough that a hundred topics cost tens of
/// megabytes rather than gigabytes. A caller that needs a different trade-off
/// sets it explicitly with [`InMemoryBus::with_retention`].
pub const DEFAULT_RETENTION: usize = 10_000;

/// An in-process event bus with per-topic ordered logs.
///
/// Cheap to clone; clones share the same logs. See the crate documentation for
/// the ordering and backpressure guarantees, and for what a Kafka swap would
/// and would not preserve.
#[derive(Clone)]
pub struct InMemoryBus {
    inner: Arc<BusState>,
}

struct BusState {
    topics: DashMap<Topic, Arc<TopicLog>>,
    retention: usize,
    /// Set by [`InMemoryBus::shutdown`]. Subscribers observe it and finish.
    closed: AtomicBool,
    clock: Arc<dyn Clock>,
}

struct TopicLog {
    state: Mutex<LogState>,
    /// Carries the topic's high watermark. Subscribers park on this rather
    /// than on a `Notify`, because `watch` versions cannot lose a wakeup that
    /// lands between "the log had nothing for me" and "I started waiting".
    watermark: watch::Sender<u64>,
}

struct LogState {
    /// Retained messages, oldest first. The offset of `entries[i]` is
    /// `next_offset - entries.len() + i`, so no per-entry bookkeeping is
    /// needed to locate a cursor.
    entries: VecDeque<EventEnvelope>,
    /// The offset the next publish will be assigned. Starts at 1.
    next_offset: u64,
}

impl LogState {
    /// The oldest offset still retained. Equal to `next_offset` when the log
    /// is empty, which correctly makes `subscribe_from(next_offset)` legal on
    /// a topic with no history.
    fn oldest_offset(&self) -> u64 {
        self.next_offset - self.entries.len() as u64
    }
}

impl std::fmt::Debug for InMemoryBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryBus")
            .field("topics", &self.inner.topics.len())
            .field("retention", &self.inner.retention)
            .field("closed", &self.inner.closed.load(Ordering::Acquire))
            .finish()
    }
}

impl Default for InMemoryBus {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryBus {
    /// A bus retaining [`DEFAULT_RETENTION`] messages per topic.
    pub fn new() -> Self {
        Self::with_retention(DEFAULT_RETENTION)
    }

    /// A bus retaining `retention` messages per topic.
    ///
    /// # Panics
    ///
    /// Panics when `retention` is zero. A log that retains nothing cannot
    /// deliver anything — a subscriber would be evicted by the very publish it
    /// was waiting for — so this is a configuration error, not a degenerate
    /// case worth supporting.
    pub fn with_retention(retention: usize) -> Self {
        assert!(
            retention > 0,
            "bus retention must be at least 1; a zero-length log can never \
             deliver a message to anyone"
        );
        Self::build(retention, Arc::new(SystemClock))
    }

    /// A bus that stamps messages from an injected clock.
    ///
    /// The simulation replays scenarios against a [`cb_ports::FixedClock`] so
    /// that a recorded run and its replay carry identical `published_at`
    /// values; wall-clock stamps would differ on every run and make the two
    /// undiffable.
    pub fn with_clock(retention: usize, clock: Arc<dyn Clock>) -> Self {
        assert!(retention > 0, "bus retention must be at least 1");
        Self::build(retention, clock)
    }

    fn build(retention: usize, clock: Arc<dyn Clock>) -> Self {
        Self {
            inner: Arc::new(BusState {
                topics: DashMap::new(),
                retention,
                closed: AtomicBool::new(false),
                clock,
            }),
        }
    }

    /// Close the bus.
    ///
    /// Every live subscription's next [`BusSubscription::next`] drains
    /// whatever it has already missed and then returns `Ok(None)`, and further
    /// publishes fail with [`PortError::Unavailable`]. The port has no
    /// shutdown method because a broker is not something an application
    /// shuts down; this exists for tests and for a clean process exit.
    pub fn shutdown(&self) {
        self.inner.closed.store(true, Ordering::Release);
        // Bump every topic's watermark so parked subscribers wake, notice the
        // flag and finish. Without this they would wait for a publish that is
        // never coming.
        for log in self.inner.topics.iter() {
            let current = *log.watermark.borrow();
            log.watermark.send_replace(current);
        }
    }

    /// How many messages this bus retains per topic.
    pub fn retention(&self) -> usize {
        self.inner.retention
    }

    fn log_for(&self, topic: &Topic) -> Arc<TopicLog> {
        if let Some(existing) = self.inner.topics.get(topic) {
            return Arc::clone(existing.value());
        }
        // `entry` rather than a second `get`: two tasks publishing to a new
        // topic at once must end up sharing one log, not racing to install
        // two and losing one's messages.
        Arc::clone(
            self.inner
                .topics
                .entry(topic.clone())
                .or_insert_with(|| {
                    Arc::new(TopicLog {
                        state: Mutex::new(LogState {
                            entries: VecDeque::new(),
                            next_offset: 1,
                        }),
                        watermark: watch::channel(0).0,
                    })
                })
                .value(),
        )
    }

    fn ensure_open(&self) -> PortResult<()> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(PortError::Unavailable("the event bus is shut down".into()));
        }
        Ok(())
    }
}

#[async_trait]
impl EventBus for InMemoryBus {
    async fn publish(&self, topic: &Topic, key: &str, payload: Vec<u8>) -> PortResult<u64> {
        self.ensure_open()?;

        let log = self.log_for(topic);
        let published_at = self.inner.clock.now();

        let offset = {
            let mut state = log.state.lock();
            let offset = state.next_offset;
            state.entries.push_back(EventEnvelope {
                topic: topic.clone(),
                key: key.to_string(),
                payload,
                offset,
                published_at,
            });
            state.next_offset += 1;

            // Evict from the front, which is what bounds memory. Anyone whose
            // cursor pointed at the evicted message learns about it on their
            // next `next()` call, as Contention.
            while state.entries.len() > self.inner.retention {
                state.entries.pop_front();
            }
            offset
        };

        // Sent after the lock is released, so a woken subscriber never
        // contends with the publisher for the log it is about to read.
        log.watermark.send_replace(offset);
        Ok(offset)
    }

    /// Delivers messages published from this call onward. Equivalent to
    /// `subscribe_from(high_watermark + 1)`, and implemented that way so the
    /// two cannot drift.
    async fn subscribe(&self, topic: &Topic) -> PortResult<Box<dyn BusSubscription>> {
        self.ensure_open()?;
        let log = self.log_for(topic);
        let cursor = log.state.lock().next_offset;
        Ok(Box::new(LogSubscription::new(
            Arc::clone(&self.inner),
            topic.clone(),
            log,
            cursor,
        )))
    }

    /// Resumes from `offset`, which must be in `[oldest_retained, next]`.
    ///
    /// Three ways to be out of range, all [`PortError::InvalidArgument`]:
    /// offset 0 does not exist because offsets are 1-based; an offset below
    /// the retention window has aged out and cannot be served; an offset above
    /// the next one has not happened yet, and accepting it would silently
    /// skip whatever fills the gap.
    async fn subscribe_from(
        &self,
        topic: &Topic,
        offset: u64,
    ) -> PortResult<Box<dyn BusSubscription>> {
        self.ensure_open()?;

        if offset == 0 {
            return Err(PortError::InvalidArgument(
                "bus offsets start at 1; offset 0 never exists".into(),
            ));
        }

        let log = self.log_for(topic);
        {
            let state = log.state.lock();
            let oldest = state.oldest_offset();
            if offset < oldest {
                return Err(PortError::InvalidArgument(format!(
                    "offset {offset} on `{topic}` has aged out of the retention \
                     window; the oldest available is {oldest}"
                )));
            }
            if offset > state.next_offset {
                return Err(PortError::InvalidArgument(format!(
                    "offset {offset} on `{topic}` is beyond the end of the log; \
                     the next offset to be assigned is {}",
                    state.next_offset
                )));
            }
        }

        Ok(Box::new(LogSubscription::new(
            Arc::clone(&self.inner),
            topic.clone(),
            log,
            offset,
        )))
    }

    /// Zero for a topic nothing has been published to.
    ///
    /// Reading a topic does not create it. A metrics scrape iterating over
    /// candidate topic names must not be able to grow the topic map.
    async fn high_watermark(&self, topic: &Topic) -> PortResult<u64> {
        self.ensure_open()?;
        Ok(self
            .inner
            .topics
            .get(topic)
            .map(|log| log.state.lock().next_offset - 1)
            .unwrap_or(0))
    }

    async fn health_check(&self) -> PortResult<()> {
        self.ensure_open()
    }
}

/// A cursor over one topic's log.
///
/// Holds no messages of its own. Falling behind costs nothing but a stale
/// cursor, which is why a slow subscriber cannot grow the bus's memory.
struct LogSubscription {
    bus: Arc<BusState>,
    topic: Topic,
    log: Arc<TopicLog>,
    watermark: watch::Receiver<u64>,
    /// The next offset this subscriber wants.
    cursor: u64,
    /// Set once the subscription has reported falling behind. A caller that
    /// keeps polling after `Contention` gets `Ok(None)` rather than the same
    /// error forever, because the subscription is genuinely finished: its
    /// cursor points into history that no longer exists.
    finished: bool,
}

impl LogSubscription {
    fn new(bus: Arc<BusState>, topic: Topic, log: Arc<TopicLog>, cursor: u64) -> Self {
        let watermark = log.watermark.subscribe();
        Self {
            bus,
            topic,
            log,
            watermark,
            cursor,
            finished: false,
        }
    }
}

#[async_trait]
impl BusSubscription for LogSubscription {
    async fn next(&mut self) -> PortResult<Option<EventEnvelope>> {
        if self.finished {
            return Ok(None);
        }

        loop {
            // Scoped so the lock is released before any await below. Holding a
            // parking_lot guard across an await would deadlock the publisher
            // as soon as this task is parked.
            {
                let state = self.log.state.lock();
                let oldest = state.oldest_offset();

                if self.cursor < oldest {
                    self.finished = true;
                    let missed = oldest - self.cursor;
                    let topic = &self.topic;
                    return Err(PortError::Contention(format!(
                        "subscriber on `{topic}` fell {missed} message(s) behind \
                         the retention window; resubscribe from offset {oldest} \
                         or later and resynchronise from a snapshot"
                    )));
                }

                if self.cursor < state.next_offset {
                    let index = (self.cursor - oldest) as usize;
                    let envelope = state.entries[index].clone();
                    self.cursor += 1;
                    return Ok(Some(envelope));
                }
            }

            // Caught up. Check for shutdown before parking, so a bus that was
            // closed while we were draining does not leave us waiting.
            if self.bus.closed.load(Ordering::Acquire) {
                self.finished = true;
                return Ok(None);
            }

            if self.watermark.changed().await.is_err() {
                // The topic log outlives its sender only if the bus is gone.
                self.finished = true;
                return Ok(None);
            }
        }
    }
}
