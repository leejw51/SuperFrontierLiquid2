//! The guarantees the crate documentation claims, asserted.

use cb_bus::{BusFaults, FaultyBus, InMemoryBus};
use cb_ports::{BusSubscription, EventBus, PortError, Topic};
use std::sync::Arc;
use std::time::Duration;

fn topic(name: &str) -> Topic {
    Topic::new(name)
}

fn body(text: &str) -> Vec<u8> {
    text.as_bytes().to_vec()
}

fn text(payload: &[u8]) -> String {
    String::from_utf8(payload.to_vec()).expect("test payloads are UTF-8")
}

/// Read exactly `n` messages, failing the test rather than hanging forever if
/// the bus stops producing.
async fn take(sub: &mut Box<dyn BusSubscription>, n: usize) -> Vec<cb_ports::EventEnvelope> {
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let next = tokio::time::timeout(Duration::from_secs(5), sub.next())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for message {i} of {n}"))
            .unwrap_or_else(|e| panic!("message {i} failed: {e}"))
            .unwrap_or_else(|| panic!("the bus closed before message {i} of {n}"));
        out.push(next);
    }
    out
}

// ---------------------------------------------------------------- offsets --

#[tokio::test]
async fn offsets_are_one_based_and_monotonic_per_topic() {
    let bus = InMemoryBus::new();
    let trades = topic("market.trades");
    let fills = topic("account.fills");

    assert_eq!(
        bus.high_watermark(&trades).await.expect("hwm"),
        0,
        "a topic nothing has been published to is at watermark 0, which is \
         only unambiguous because offsets start at 1"
    );

    assert_eq!(
        bus.publish(&trades, "BTC", body("a")).await.expect("pub"),
        1
    );
    assert_eq!(
        bus.publish(&trades, "BTC", body("b")).await.expect("pub"),
        2
    );
    assert_eq!(
        bus.publish(&trades, "ETH", body("c")).await.expect("pub"),
        3
    );
    assert_eq!(bus.high_watermark(&trades).await.expect("hwm"), 3);

    assert_eq!(
        bus.publish(&fills, "u1", body("x")).await.expect("pub"),
        1,
        "each topic has its own offset sequence"
    );
    assert_eq!(bus.high_watermark(&trades).await.expect("hwm"), 3);
}

#[tokio::test]
async fn reading_a_watermark_does_not_create_the_topic() {
    let bus = InMemoryBus::new();
    for n in 0..100 {
        assert_eq!(
            bus.high_watermark(&topic(&format!("probe.{n}")))
                .await
                .expect("hwm"),
            0
        );
    }
    let rendered = format!("{bus:?}");
    assert!(
        rendered.contains("topics: 0"),
        "probing topic names must not grow the topic map: {rendered}"
    );
}

#[tokio::test]
async fn subscribe_delivers_only_what_comes_after_it() {
    let bus = InMemoryBus::new();
    let t = topic("market.trades");

    bus.publish(&t, "BTC", body("before")).await.expect("pub");
    let mut sub = bus.subscribe(&t).await.expect("subscribe");
    bus.publish(&t, "BTC", body("after")).await.expect("pub");

    let got = take(&mut sub, 1).await;
    assert_eq!(text(&got[0].payload), "after");
    assert_eq!(got[0].offset, 2);
}

#[tokio::test]
async fn the_resume_idiom_loses_nothing() {
    // The documented pattern: note the watermark, then resume at the next
    // offset. If this were off by one, a reconnecting gateway would either
    // replay a message or skip one.
    let bus = InMemoryBus::new();
    let t = topic("market.trades");

    bus.publish(&t, "BTC", body("old")).await.expect("pub");
    let watermark = bus.high_watermark(&t).await.expect("hwm");

    bus.publish(&t, "BTC", body("new")).await.expect("pub");

    let mut sub = bus
        .subscribe_from(&t, watermark + 1)
        .await
        .expect("resume subscribes");
    let got = take(&mut sub, 1).await;
    assert_eq!(text(&got[0].payload), "new");
}

#[tokio::test]
async fn subscribe_from_replays_retained_history_then_continues_live() {
    let bus = InMemoryBus::new();
    let t = topic("market.trades");
    for n in 0..5 {
        bus.publish(&t, "BTC", body(&format!("m{n}")))
            .await
            .expect("pub");
    }

    let mut sub = bus.subscribe_from(&t, 3).await.expect("subscribe");
    bus.publish(&t, "BTC", body("m5")).await.expect("pub");

    let got = take(&mut sub, 4).await;
    assert_eq!(
        got.iter().map(|e| text(&e.payload)).collect::<Vec<_>>(),
        vec!["m2", "m3", "m4", "m5"],
        "history replays in order and runs straight into live messages"
    );
    assert_eq!(
        got.iter().map(|e| e.offset).collect::<Vec<_>>(),
        vec![3, 4, 5, 6]
    );
}

#[tokio::test]
async fn out_of_range_offsets_are_rejected_in_all_three_directions() {
    let bus = InMemoryBus::with_retention(4);
    let t = topic("market.trades");
    for n in 0..10 {
        bus.publish(&t, "BTC", body(&format!("m{n}")))
            .await
            .expect("pub");
    }
    // Offsets 1..=10 assigned; only 7..=10 retained; next offset is 11.

    for (offset, why) in [
        (0, "offset 0 does not exist"),
        (1, "offset 1 has aged out"),
        (6, "offset 6 has aged out"),
        (12, "offset 12 is beyond the end"),
    ] {
        let err = bus
            .subscribe_from(&t, offset)
            .await
            .err()
            .unwrap_or_else(|| panic!("{why}, so subscribing there must fail"));
        assert!(
            matches!(err, PortError::InvalidArgument(_)),
            "{why}: expected InvalidArgument, got {err:?}"
        );
    }

    bus.subscribe_from(&t, 7)
        .await
        .expect("the oldest retained offset is valid");
    bus.subscribe_from(&t, 11)
        .await
        .expect("the next offset to be assigned is valid: it means 'from now on'");
}

#[tokio::test]
async fn subscribing_from_the_start_of_an_empty_topic_is_legal() {
    let bus = InMemoryBus::new();
    let t = topic("market.trades");
    let mut sub = bus.subscribe_from(&t, 1).await.expect("subscribe");
    bus.publish(&t, "BTC", body("first")).await.expect("pub");

    let got = take(&mut sub, 1).await;
    assert_eq!(got[0].offset, 1);
}

// --------------------------------------------------------------- ordering --

/// The contract's actual promise: same key, publish order.
#[tokio::test]
async fn messages_sharing_a_key_arrive_in_publish_order_when_keys_interleave() {
    let bus = InMemoryBus::new();
    let t = topic("market.trades");
    let mut sub = bus.subscribe(&t).await.expect("subscribe");

    // Deliberately interleaved: if the implementation ever grouped by key,
    // publishing all of one key first would hide the bug.
    let plan = [
        ("BTC", "b0"),
        ("ETH", "e0"),
        ("BTC", "b1"),
        ("SOL", "s0"),
        ("ETH", "e1"),
        ("BTC", "b2"),
        ("ETH", "e2"),
        ("SOL", "s1"),
        ("BTC", "b3"),
        ("SOL", "s2"),
    ];
    for (key, payload) in plan {
        bus.publish(&t, key, body(payload)).await.expect("pub");
    }

    let got = take(&mut sub, plan.len()).await;

    for key in ["BTC", "ETH", "SOL"] {
        let published: Vec<_> = plan
            .iter()
            .filter(|(k, _)| *k == key)
            .map(|(_, p)| p.to_string())
            .collect();
        let delivered: Vec<_> = got
            .iter()
            .filter(|e| e.key == key)
            .map(|e| text(&e.payload))
            .collect();
        assert_eq!(
            delivered, published,
            "messages keyed `{key}` must arrive in the order they were published"
        );
    }

    // This implementation happens to preserve the full interleaving too. The
    // assertion documents that, and the crate docs warn that Kafka would not.
    assert_eq!(
        got.iter().map(|e| text(&e.payload)).collect::<Vec<_>>(),
        plan.iter().map(|(_, p)| p.to_string()).collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_key_order_survives_concurrent_publishers() {
    const PER_KEY: usize = 100;
    let bus = InMemoryBus::new();
    let t = topic("market.trades");
    let mut sub = bus.subscribe(&t).await.expect("subscribe");

    let mut publishers = Vec::new();
    for key in ["BTC", "ETH"] {
        let bus = bus.clone();
        let t = t.clone();
        publishers.push(tokio::spawn(async move {
            for n in 0..PER_KEY {
                bus.publish(&t, key, body(&n.to_string()))
                    .await
                    .expect("publish succeeds");
            }
        }));
    }

    let got = take(&mut sub, PER_KEY * 2).await;
    for p in publishers {
        p.await.expect("publisher finishes");
    }

    for key in ["BTC", "ETH"] {
        let delivered: Vec<usize> = got
            .iter()
            .filter(|e| e.key == key)
            .map(|e| text(&e.payload).parse().expect("payload is a number"))
            .collect();
        assert_eq!(
            delivered,
            (0..PER_KEY).collect::<Vec<_>>(),
            "key `{key}` must be delivered in publish order despite the other \
             publisher interleaving with it"
        );
    }

    // Offsets are still a single dense monotonic sequence.
    assert_eq!(
        got.iter().map(|e| e.offset).collect::<Vec<_>>(),
        (1..=(PER_KEY as u64 * 2)).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn every_subscriber_sees_the_same_order() {
    let bus = InMemoryBus::new();
    let t = topic("market.trades");
    let mut a = bus.subscribe(&t).await.expect("subscribe");
    let mut b = bus.subscribe(&t).await.expect("subscribe");

    for n in 0..20 {
        bus.publish(
            &t,
            if n % 2 == 0 { "BTC" } else { "ETH" },
            body(&n.to_string()),
        )
        .await
        .expect("pub");
    }

    let seen_a = take(&mut a, 20).await;
    let seen_b = take(&mut b, 20).await;
    assert_eq!(seen_a, seen_b, "fan-out must not reorder per subscriber");
}

#[tokio::test]
async fn topics_are_independent() {
    let bus = InMemoryBus::new();
    let trades = topic("market.trades");
    let fills = topic("account.fills");

    let mut on_trades = bus.subscribe(&trades).await.expect("subscribe");
    bus.publish(&fills, "u1", body("not for you"))
        .await
        .expect("pub");
    bus.publish(&trades, "BTC", body("for you"))
        .await
        .expect("pub");

    let got = take(&mut on_trades, 1).await;
    assert_eq!(text(&got[0].payload), "for you");
    assert_eq!(got[0].topic, trades);
}

// ---------------------------------------------------------- backpressure --

/// A subscriber that stops reading must not be able to grow the bus, and must
/// not be silently skipped forward either — a gap it cannot detect is worse
/// than an error it can.
#[tokio::test]
async fn a_subscriber_that_falls_behind_retention_is_dropped_with_contention() {
    let bus = InMemoryBus::with_retention(4);
    let t = topic("market.trades");

    let mut slow = bus.subscribe(&t).await.expect("subscribe");
    for n in 0..10 {
        bus.publish(&t, "BTC", body(&format!("m{n}")))
            .await
            .expect("pub");
    }

    let err = slow
        .next()
        .await
        .expect_err("a subscriber outside the retention window must be told");
    assert!(
        matches!(err, PortError::Contention(_)),
        "falling behind is Contention, so the caller knows to resubscribe: {err:?}"
    );
    assert!(err.is_retryable(), "resubscribing is the retry");

    assert_eq!(
        slow.next()
            .await
            .expect("polling a dead subscription is not an error"),
        None,
        "once dropped the subscription is finished, not stuck reporting the \
         same error forever"
    );

    // And the recovery the error asks for actually works.
    let oldest_available = bus.high_watermark(&t).await.expect("hwm") - 3;
    let mut fresh = bus
        .subscribe_from(&t, oldest_available)
        .await
        .expect("resubscribing from the oldest retained offset works");
    let got = take(&mut fresh, 4).await;
    assert_eq!(
        got.iter().map(|e| text(&e.payload)).collect::<Vec<_>>(),
        vec!["m6", "m7", "m8", "m9"]
    );
}

#[tokio::test]
async fn a_subscriber_that_keeps_up_is_never_dropped_however_long_the_stream() {
    // Twenty-five times the retention window. Memory stays bounded because
    // the subscriber holds a cursor, not a queue.
    let bus = InMemoryBus::with_retention(4);
    let t = topic("market.trades");
    let mut sub = bus.subscribe(&t).await.expect("subscribe");

    for n in 0..100u64 {
        bus.publish(&t, "BTC", body(&n.to_string()))
            .await
            .expect("pub");
        let got = take(&mut sub, 1).await;
        assert_eq!(got[0].offset, n + 1);
    }
}

#[tokio::test]
async fn one_slow_subscriber_does_not_stall_a_fast_one() {
    let bus = InMemoryBus::with_retention(4);
    let t = topic("market.trades");
    let _abandoned = bus.subscribe(&t).await.expect("subscribe");
    let mut attentive = bus.subscribe(&t).await.expect("subscribe");

    for n in 0..20u64 {
        bus.publish(&t, "BTC", body(&n.to_string()))
            .await
            .expect("pub");
        let got = take(&mut attentive, 1).await;
        assert_eq!(got[0].offset, n + 1);
    }
}

#[tokio::test]
async fn shutdown_finishes_subscribers_and_refuses_publishes() {
    let bus = InMemoryBus::new();
    let t = topic("market.trades");
    let mut sub = bus.subscribe(&t).await.expect("subscribe");
    bus.publish(&t, "BTC", body("last")).await.expect("pub");

    let reader = tokio::spawn(async move {
        let first = sub
            .next()
            .await
            .expect("read")
            .expect("one message is pending");
        assert_eq!(text(&first.payload), "last");
        sub.next().await.expect("a closed bus is not an error")
    });

    // Let the reader run until it parks on the watermark, so this exercises
    // waking a parked subscriber rather than one that never waited. The test
    // runtime is single-threaded: `yield_now` hands the thread to the reader,
    // which reads "last" and then blocks in `next()` with nothing left to
    // read, and only then does control come back here. Deterministic, where
    // the sleep this replaces only made parking likely.
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    assert!(
        !reader.is_finished(),
        "the reader must be parked, not done, before shutdown"
    );
    bus.shutdown();

    assert_eq!(
        reader.await.expect("the reader finishes"),
        None,
        "a parked subscriber must be woken by shutdown, not left waiting"
    );

    let err = bus
        .publish(&t, "BTC", body("too late"))
        .await
        .expect_err("a shut-down bus accepts nothing");
    assert!(matches!(err, PortError::Unavailable(_)), "got {err:?}");
    assert!(bus.health_check().await.is_err());
}

// ------------------------------------------------------------- FaultyBus --

#[tokio::test]
async fn faulty_bus_fails_every_nth_publish_and_lets_the_rest_through() {
    let faults = BusFaults::healthy();
    let inner = InMemoryBus::new();
    let bus = FaultyBus::new(inner.clone(), Arc::clone(&faults));
    let t = topic("market.trades");

    faults.fail_every_nth_publish(3);

    let mut failures = 0;
    for n in 1..=9 {
        match bus.publish(&t, "BTC", body(&n.to_string())).await {
            Ok(_) => {}
            Err(PortError::Unavailable(_)) => failures += 1,
            Err(other) => panic!("unexpected error on attempt {n}: {other:?}"),
        }
    }

    assert_eq!(failures, 3, "attempts 3, 6 and 9 fail");
    assert_eq!(
        inner.high_watermark(&t).await.expect("hwm"),
        6,
        "exactly the surviving publishes reached the real bus"
    );
    assert_eq!(faults.publish_attempts(), 9);

    faults.fail_every_nth_publish(0);
    bus.publish(&t, "BTC", body("healed"))
        .await
        .expect("faults can be cleared");
}

#[tokio::test]
async fn faulty_bus_can_drop_and_restore_the_connection() {
    let faults = BusFaults::healthy();
    let bus = FaultyBus::new(InMemoryBus::new(), Arc::clone(&faults));
    let t = topic("market.trades");

    bus.publish(&t, "BTC", body("fine")).await.expect("healthy");

    faults.disconnect();
    for result in [
        bus.publish(&t, "BTC", body("x")).await.map(|_| ()),
        bus.subscribe(&t).await.map(|_| ()),
        bus.subscribe_from(&t, 1).await.map(|_| ()),
        bus.high_watermark(&t).await.map(|_| ()),
        bus.health_check().await,
    ] {
        let err = result.expect_err("every operation fails while disconnected");
        assert!(matches!(err, PortError::Unavailable(_)), "got {err:?}");
        assert!(err.is_retryable());
    }

    faults.reconnect();
    bus.health_check()
        .await
        .expect("reconnecting heals the bus");
    bus.publish(&t, "BTC", body("back"))
        .await
        .expect("healthy again");
}

#[tokio::test]
async fn faulty_bus_can_make_an_operation_slow_enough_to_time_out() {
    let faults = BusFaults::healthy();
    let bus = FaultyBus::new(InMemoryBus::new(), Arc::clone(&faults));
    let t = topic("market.trades");

    faults.set_latency(Duration::from_millis(200));

    let timed_out = tokio::time::timeout(
        Duration::from_millis(20),
        bus.publish(&t, "BTC", body("slow")),
    )
    .await;
    assert!(
        timed_out.is_err(),
        "injected latency must be able to trip a caller's deadline"
    );

    faults.reset();
    bus.publish(&t, "BTC", body("prompt"))
        .await
        .expect("reset clears latency");
    assert_eq!(
        faults.publish_attempts(),
        1,
        "reset also clears the counter"
    );
}

#[tokio::test]
async fn faults_can_be_flipped_from_another_task_mid_scenario() {
    // The scenario publishes until another task pulls the plug, and the very
    // next publish after the plug is pulled must fail. Sequenced with
    // channels rather than sleeps: the breaker waits for the scenario to be
    // under way, disconnects, and says so.
    let faults = BusFaults::healthy();
    let bus = Arc::new(FaultyBus::new(InMemoryBus::new(), Arc::clone(&faults)));
    let t = topic("market.trades");

    let (underway_tx, underway_rx) = tokio::sync::oneshot::channel::<()>();
    let (broken_tx, mut broken_rx) = tokio::sync::oneshot::channel::<()>();
    let breaker = {
        let faults = Arc::clone(&faults);
        tokio::spawn(async move {
            underway_rx.await.expect("the scenario starts");
            faults.disconnect();
            broken_tx.send(()).expect("the scenario is listening");
        })
    };

    let mut underway = Some(underway_tx);
    let mut published = 0u64;
    loop {
        let result = bus.publish(&t, "BTC", body(&published.to_string())).await;
        match broken_rx.try_recv() {
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                assert!(result.is_ok(), "healthy until the breaker runs");
                published += 1;
                if published == 10 {
                    underway.take().unwrap().send(()).unwrap();
                }
                tokio::task::yield_now().await;
            }
            _ => break,
        }
    }
    breaker.await.expect("the breaker task finishes");

    assert!(
        published >= 10,
        "the scenario was in flight when the fault landed"
    );
    let err = bus
        .publish(&t, "BTC", body("after"))
        .await
        .expect_err("a fault injected from another task applies to the next call");
    assert!(matches!(err, PortError::Unavailable(_)), "got {err:?}");
    assert_eq!(
        bus.inner().high_watermark(&t).await.unwrap(),
        published,
        "nothing published after the disconnect reached the inner bus"
    );
}
