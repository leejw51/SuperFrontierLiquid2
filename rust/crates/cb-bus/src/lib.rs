//! # cb-bus — publish/subscribe, in process
//!
//! [`InMemoryBus`] implements [`cb_ports::EventBus`] as a per-topic ordered
//! log with monotonic offsets and bounded retention, plus [`FaultyBus`], a
//! decorator that injects failures so callers can test their own recovery
//! paths.
//!
//! ## The shape, and why it is a log
//!
//! The obvious in-process bus is a `tokio::sync::broadcast` channel per
//! topic. This is not that, for one reason: a broadcast channel cannot answer
//! "give me everything from offset 412". A gateway that drops its WebSocket
//! connection needs to resume, and a consumer that restarts needs to catch up
//! without a full snapshot. Both want a log, so the log is what the port
//! describes and what this implements.
//!
//! The log is *shared*. A subscription is an `Arc` to the topic's log plus a
//! cursor — there is no per-subscriber buffer anywhere. That single decision
//! is what gives the backpressure guarantee below.
//!
//! ## Guarantees
//!
//! **Ordering.** Every message on a topic goes into one totally ordered log,
//! so all subscribers observe all messages in the same order. Messages
//! sharing a partition key are therefore delivered in publish order, which is
//! what the port promises. Note that this implementation is *stronger* than
//! the contract: it also orders messages with different keys. Do not depend
//! on that (see the Kafka section).
//!
//! **Offsets** are 1-based and monotonic within a topic. The first message
//! published gets offset 1, so [`cb_ports::EventBus::high_watermark`] returns
//! 0 for a topic that has never been published to — an unambiguous "nothing
//! yet" that a 0-based scheme could not express. The resume idiom is
//! `subscribe_from(high_watermark + 1)`.
//!
//! **Backpressure.** Memory is bounded by `retention × topics`, regardless of
//! how many subscribers exist or how slow they are, because subscribers hold
//! cursors rather than copies. A subscriber that falls outside the retention
//! window is not buffered and is not silently skipped ahead: its next
//! [`cb_ports::BusSubscription::next`] returns
//! [`cb_ports::PortError::Contention`], and the subscription is then finished.
//! Silently jumping the cursor forward would hand the caller a gap it could
//! not detect; the error tells it to resubscribe and resynchronise from a
//! snapshot, which is the only correct recovery.
//!
//! ## What a Kafka swap would preserve, and what it would not
//!
//! Preserved:
//!
//! * Per-key ordering — Kafka hashes the key to a partition and orders within
//!   it, which is exactly the contract.
//! * Monotonic per-topic offsets, and replay from a retained offset.
//! * `InvalidArgument` for an offset that has aged out, matching Kafka's
//!   `OffsetOutOfRange`.
//! * At-least-once delivery. Neither this bus nor Kafka deduplicates.
//!
//! Not preserved — code that relies on any of these will break on the swap:
//!
//! * **Cross-key ordering.** Kafka orders within a partition, and two keys
//!   usually land in different partitions. Here everything shares one log.
//!   This is the single most likely thing to be depended on by accident.
//! * **A single offset sequence.** A Kafka topic's offsets are per partition,
//!   so `high_watermark` becomes per partition and the "latest offset" of a
//!   topic is a vector, not a number.
//! * **Immediate visibility.** `publish` here returns after the message is in
//!   the log and visible. Kafka returns after the configured acks, and a
//!   consumer may not see the message for milliseconds.
//! * **Retention semantics.** Retention here is a message count per topic.
//!   Kafka's is bytes and time per partition, so *when* a slow consumer gets
//!   cut off differs.
//! * **Durability.** This bus loses everything when the process exits.
//!
//! ## Concurrency
//!
//! The log is behind a [`parking_lot::Mutex`], which is never held across an
//! `await`. Waiting subscribers park on a [`tokio::sync::watch`] carrying the
//! topic's high watermark rather than a `Notify`: `watch` tracks versions, so
//! a publish landing between "I found nothing" and "I start waiting" still
//! wakes the waiter. The equivalent `Notify` code has a lost-wakeup race
//! unless the permit is armed first, and that is an easy thing to get subtly
//! wrong.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod faulty;
mod memory;

pub use faulty::{BusFaults, FaultyBus};
pub use memory::{InMemoryBus, DEFAULT_RETENTION};
