//! Publish/subscribe messaging.
//!
//! The default implementation is an in-process broadcast. The trait is shaped
//! after a partitioned log — topic, key, payload, offset — so that swapping in
//! Kafka or NATS is a wiring change rather than a redesign.
//!
//! The `key` is the partition key. Messages sharing a key must be delivered in
//! publish order; messages with different keys carry no ordering guarantee.
//! Ordering per market symbol is what an order-book consumer actually needs,
//! and it is what a partitioned log can actually promise.

use crate::error::PortResult;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::fmt;

/// A named channel. Topic names are lowercase dotted paths, such as
/// `market.trades` or `account.fills`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Topic(String);

impl Topic {
    /// Build a topic from a dotted name.
    pub fn new(name: impl Into<String>) -> Self {
        Topic(name.into())
    }

    /// The underlying name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Topic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for Topic {
    fn from(s: &str) -> Self {
        Topic::new(s)
    }
}

/// One message on the bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventEnvelope {
    /// Topic the message was published to.
    pub topic: Topic,
    /// Partition key. Messages with equal keys keep their relative order.
    pub key: String,
    /// Opaque payload, typically JSON.
    pub payload: Vec<u8>,
    /// Monotonic position within the topic, assigned by the bus.
    pub offset: u64,
    /// When the broker accepted the message.
    pub published_at: DateTime<Utc>,
}

/// A live subscription. Dropping it unsubscribes.
#[async_trait]
pub trait BusSubscription: Send + Sync {
    /// The next message, or `None` once the bus has shut down.
    ///
    /// A subscriber that falls too far behind may be disconnected rather than
    /// buffered without bound; implementations report that as
    /// [`crate::PortError::Contention`] so the caller can resubscribe and
    /// resynchronise from a snapshot.
    async fn next(&mut self) -> PortResult<Option<EventEnvelope>>;
}

/// A publish/subscribe message bus.
#[async_trait]
pub trait EventBus: Send + Sync + 'static {
    /// Publish `payload` to `topic` under partition key `key`.
    ///
    /// Returns the assigned offset.
    async fn publish(&self, topic: &Topic, key: &str, payload: Vec<u8>) -> PortResult<u64>;

    /// Subscribe to a topic, receiving messages published from now on.
    async fn subscribe(&self, topic: &Topic) -> PortResult<Box<dyn BusSubscription>>;

    /// Subscribe starting at a historical offset, where the backend retains
    /// history. Implementations without retention return
    /// [`crate::PortError::InvalidArgument`].
    async fn subscribe_from(
        &self,
        topic: &Topic,
        offset: u64,
    ) -> PortResult<Box<dyn BusSubscription>>;

    /// Current highest assigned offset for a topic.
    async fn high_watermark(&self, topic: &Topic) -> PortResult<u64>;

    /// Whether the backend is reachable.
    async fn health_check(&self) -> PortResult<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_round_trip_and_compare() {
        let t: Topic = "market.trades".into();
        assert_eq!(t.as_str(), "market.trades");
        assert_eq!(t.to_string(), "market.trades");
        assert_eq!(t, Topic::new("market.trades"));
        assert_ne!(t, Topic::new("market.fills"));
    }
}
