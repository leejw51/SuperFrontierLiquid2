//! The in-memory bus checked against a model, over generated histories.
//!
//! `bus.rs` asserts each documented guarantee once, with a hand-picked
//! sequence. Here random interleavings of publishes, subscriptions from
//! arbitrary offsets and reads are replayed against both the bus and a plain
//! `Vec` per topic, and every observation is compared.
//!
//! What the group proves:
//! - offsets are 1-based, consecutive and per topic, and the high watermark is
//!   always the count of messages published to that topic;
//! - a subscriber sees exactly the messages from its starting offset onward,
//!   in publish order, with their topic, key and payload intact;
//! - retention bounds the log: `subscribe_from` is accepted exactly for
//!   offsets in `[oldest retained, next]` and refused otherwise, and a reader
//!   that falls out of the window gets one `Contention` and then `None`,
//!   never a silently skipped message;
//! - backpressure never reaches the publisher: an idle subscriber cannot slow
//!   or block publishing, whatever the volume.
//!
//! No test here sleeps. Reads are only issued when the model says a message
//! is waiting, so a missing message shows up as a timeout failure rather than
//! a hang, and the runtime is single-threaded so every interleaving is the
//! one the generated history names.

use cb_bus::InMemoryBus;
use cb_ports::{BusSubscription, EventBus, PortError, Topic};
use proptest::prelude::*;
use std::time::Duration;

const TOPICS: [&str; 3] = ["market.trades", "account.fills", "chain.blocks"];

#[derive(Debug, Clone)]
enum Op {
    Publish {
        topic: usize,
        key: u8,
    },
    /// Subscribe to `topic` at a starting offset chosen relative to the log:
    /// `delta` is added to the oldest retained offset, so it ranges from
    /// before the window to past its end.
    Subscribe {
        topic: usize,
        delta: i64,
    },
    /// Read one message on subscription `sub` (modulo how many exist).
    Read {
        sub: usize,
    },
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (0..TOPICS.len(), any::<u8>()).prop_map(|(topic, key)| Op::Publish { topic, key }),
        1 => (0..TOPICS.len(), -3i64..12).prop_map(|(topic, delta)| Op::Subscribe { topic, delta }),
        4 => any::<usize>().prop_map(|sub| Op::Read { sub }),
    ]
}

struct ModelSub {
    real: Box<dyn BusSubscription>,
    topic: usize,
    cursor: u64,
    done: bool,
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
}

async fn next_now(
    sub: &mut Box<dyn BusSubscription>,
) -> cb_ports::PortResult<Option<cb_ports::EventEnvelope>> {
    tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("a read the model says is ready must not block")
}

fn check(ops: Vec<Op>, retention: usize) -> Result<(), TestCaseError> {
    runtime().block_on(async move {
        let bus = InMemoryBus::with_retention(retention);
        let topics: Vec<Topic> = TOPICS.iter().map(|t| Topic::new(*t)).collect();
        // Every message ever published per topic; offset n is log[n - 1].
        let mut log: Vec<Vec<(String, Vec<u8>)>> = vec![Vec::new(); TOPICS.len()];
        let mut subs: Vec<ModelSub> = Vec::new();

        for (step, op) in ops.into_iter().enumerate() {
            match op {
                Op::Publish { topic, key } => {
                    let payload = format!("{step}").into_bytes();
                    let key = format!("k{}", key % 4);
                    let offset = bus
                        .publish(&topics[topic], &key, payload.clone())
                        .await
                        .unwrap();
                    log[topic].push((key, payload));
                    prop_assert_eq!(
                        offset,
                        log[topic].len() as u64,
                        "offsets are consecutive from 1"
                    );
                }
                Op::Subscribe { topic, delta } => {
                    let n = log[topic].len() as u64;
                    let oldest = n.saturating_sub(retention as u64) + 1;
                    let next = n + 1;
                    let want = oldest as i64 + delta;
                    if want < 0 {
                        continue;
                    }
                    let want = want as u64;
                    match bus.subscribe_from(&topics[topic], want).await {
                        Ok(real) => {
                            prop_assert!(
                                want >= oldest && want <= next && want >= 1,
                                "accepted offset {} outside [{}, {}]",
                                want,
                                oldest,
                                next
                            );
                            subs.push(ModelSub {
                                real,
                                topic,
                                cursor: want,
                                done: false,
                            });
                        }
                        Err(PortError::InvalidArgument(_)) => {
                            prop_assert!(
                                want < oldest || want > next || want == 0,
                                "refused offset {} inside [{}, {}]",
                                want,
                                oldest,
                                next
                            );
                        }
                        Err(e) => prop_assert!(false, "unexpected error {:?}", e),
                    }
                }
                Op::Read { sub } => {
                    if subs.is_empty() {
                        continue;
                    }
                    let i = sub % subs.len();
                    let s = &mut subs[i];
                    let n = log[s.topic].len() as u64;
                    if s.done || s.cursor > n {
                        // Nothing is waiting; reading would park. Skip.
                        continue;
                    }
                    let oldest = n.saturating_sub(retention as u64) + 1;
                    let got = next_now(&mut s.real).await;
                    if s.cursor < oldest {
                        prop_assert!(
                            matches!(got, Err(PortError::Contention(_))),
                            "a reader behind the window must be told, got {:?}",
                            got
                        );
                        s.done = true;
                        let after = next_now(&mut s.real).await;
                        prop_assert!(matches!(after, Ok(None)), "then it is finished");
                    } else {
                        let env = got.unwrap().expect("a message is waiting");
                        let (key, payload) = &log[s.topic][(s.cursor - 1) as usize];
                        prop_assert_eq!(env.offset, s.cursor);
                        prop_assert_eq!(env.topic.as_str(), TOPICS[s.topic]);
                        prop_assert_eq!(&env.key, key);
                        prop_assert_eq!(&env.payload, payload);
                        s.cursor += 1;
                    }
                }
            }
            for (t, topic) in topics.iter().enumerate() {
                prop_assert_eq!(
                    bus.high_watermark(topic).await.unwrap(),
                    log[t].len() as u64
                );
            }
        }
        Ok(())
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// A tight window, so falling behind and aging out happen constantly.
    #[test]
    fn the_bus_matches_the_model_with_a_small_window(
        ops in proptest::collection::vec(op(), 0..200), retention in 1usize..6,
    ) {
        check(ops, retention)?;
    }

    /// A window larger than any history, so nothing ages out and every
    /// subscriber must see everything.
    #[test]
    fn the_bus_matches_the_model_when_nothing_ages_out(
        ops in proptest::collection::vec(op(), 0..200),
    ) {
        check(ops, 10_000)?;
    }
}

#[test]
fn an_idle_subscriber_never_slows_the_publisher() {
    // Backpressure is absorbed by the retention window, not pushed onto the
    // publisher: with a subscriber that never reads, 100,000 publishes still
    // complete without awaiting anything but the bus itself, and the idle
    // reader learns exactly how far behind it fell.
    runtime().block_on(async {
        let bus = InMemoryBus::with_retention(64);
        let t = Topic::new("market.trades");
        let mut idle = bus.subscribe(&t).await.unwrap();
        let mut live = bus.subscribe_from(&t, 1).await.unwrap();
        for i in 0..100_000u64 {
            // `now_or_never` style: a publish that parked would fail the
            // timeout below instead of hanging the suite.
            let offset = tokio::time::timeout(
                Duration::from_secs(5),
                bus.publish(&t, "BTC", i.to_le_bytes().to_vec()),
            )
            .await
            .expect("a publish never waits on a subscriber")
            .unwrap();
            assert_eq!(offset, i + 1);
            if i < 10 {
                let env = next_now(&mut live).await.unwrap().unwrap();
                assert_eq!(env.offset, i + 1, "a reader keeping up is unaffected");
            }
        }
        match next_now(&mut idle).await {
            Err(PortError::Contention(msg)) => {
                // 100,000 published, 64 retained: the oldest is 99,937, and
                // the idle cursor at 1 missed 99,936.
                assert!(msg.contains("99936"), "{msg}");
                assert!(msg.contains("99937"), "{msg}");
            }
            other => panic!("expected Contention, got {other:?}"),
        }
        assert!(matches!(next_now(&mut idle).await, Ok(None)));
        // And a fresh subscriber can resume from exactly the oldest retained.
        assert!(bus.subscribe_from(&t, 99_937).await.is_ok());
        assert!(matches!(
            bus.subscribe_from(&t, 99_936).await,
            Err(PortError::InvalidArgument(_))
        ));
    });
}
