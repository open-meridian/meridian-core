//! The bus across a process boundary, against a real broker. Decision 010.
//!
//! Two backends on one broker, because that is the property being added. The
//! memory backend already proves routing inside a process, and an in-process
//! test of this one would prove nothing about a message leaving.
//!
//! Run by `make test-broker`, which starts the broker. They fail loudly when
//! it is missing rather than skipping: a test that quietly does not run is how
//! a gate reports success without doing its job.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use meridian_bus::{Backend, Delivery, Envelope, MessageMeta, NatsBackend, Subscription};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A topic nobody else in this run is using, so tests may share a broker.
fn topic(tail: &str) -> String {
    let seq = COUNTER.fetch_add(1, Ordering::SeqCst);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("platform.test-{now}-{seq}.{tail}")
}

async fn backend() -> NatsBackend {
    let url = std::env::var("MERIDIAN_TEST_BROKER_URL").expect(
        "MERIDIAN_TEST_BROKER_URL is not set. These tests need a real broker; \
         run them with `make test-broker`.",
    );
    NatsBackend::connect(&url)
        .await
        .expect("could not reach the test broker")
}

fn envelope(deployment: &str) -> Envelope {
    Envelope {
        meta: Some(MessageMeta {
            message_id: deployment.to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Subscriptions at a broker are established asynchronously, so a publish that
/// races the subscribe is lost rather than delayed: at-most-once is the rule
/// here too.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(150)).await;
}

async fn next(subscription: &mut Subscription) -> Option<Delivery> {
    tokio::time::timeout(Duration::from_secs(5), subscription.recv())
        .await
        .ok()
        .flatten()
}

#[tokio::test]
async fn a_message_published_in_one_process_arrives_in_another() {
    // The property decision 010 exists for. Before the broker there was no
    // path at all between two processes of a deployment.
    let publisher = backend().await;
    let subscriber = backend().await;
    let topic = topic("event.something-happened");

    let mut subscription = subscriber.subscribe(&topic);
    settle().await;

    publisher
        .publish(&topic, envelope("DEP-one"))
        .expect("the publish should have been accepted");

    let delivered = next(&mut subscription).await.expect("nothing arrived");
    assert_eq!(delivered.envelope.meta.unwrap().message_id, "DEP-one");
}

#[tokio::test]
async fn a_wildcard_subscription_matches_the_way_the_grammar_says() {
    // Our `*` is one segment and `**` is a tail of one or more. The broker's
    // are `*` and `>`, and a translation that got this wrong would deliver
    // messages nobody subscribed to.
    let publisher = backend().await;
    let subscriber = backend().await;
    let root = topic("custody");

    let mut one = subscriber.subscribe(&format!("{root}.*"));
    let mut tail = subscriber.subscribe(&format!("{root}.**"));
    settle().await;

    publisher
        .publish(&format!("{root}.alpha"), envelope("one-segment"))
        .unwrap();
    publisher
        .publish(&format!("{root}.alpha.beta"), envelope("two-segments"))
        .unwrap();

    let first = next(&mut one)
        .await
        .expect("the single-segment subscriber got nothing");
    assert_eq!(first.envelope.meta.unwrap().message_id, "one-segment");
    assert!(
        next(&mut one).await.is_none(),
        "`*` matched more than one segment"
    );

    let mut seen = vec![];
    while let Some(delivery) = next(&mut tail).await {
        seen.push(delivery.envelope.meta.unwrap().message_id);
        if seen.len() == 2 {
            break;
        }
    }
    seen.sort();
    assert_eq!(seen, vec!["one-segment", "two-segments"]);
}

#[tokio::test]
async fn every_subscriber_gets_its_own_copy() {
    // Publish-subscribe, not a queue. A second subscriber must not take
    // messages from the first, which is the broker default being relied on and
    // worth pinning rather than assuming.
    let publisher = backend().await;
    let first = backend().await;
    let second = backend().await;
    let topic = topic("event.fanned-out");

    let mut one = first.subscribe(&topic);
    let mut two = second.subscribe(&topic);
    settle().await;

    publisher.publish(&topic, envelope("DEP-fan")).unwrap();

    assert!(
        next(&mut one).await.is_some(),
        "the first subscriber got nothing"
    );
    assert!(
        next(&mut two).await.is_some(),
        "the second subscriber got nothing"
    );
}

#[tokio::test]
async fn publishing_to_a_topic_nobody_hears_is_not_a_failure() {
    let publisher = backend().await;
    let subscriber = backend().await;
    let topic = topic("event.gone");

    let subscription = subscriber.subscribe(&topic);
    settle().await;
    drop(subscription);
    settle().await;

    publisher
        .publish(&topic, envelope("DEP-nobody"))
        .expect("publishing to nobody is not a failure");
}

#[tokio::test]
async fn a_pattern_cannot_be_published_to() {
    let publisher = backend().await;
    let refused = publisher
        .publish("platform.custody.*", envelope("DEP-bad"))
        .expect_err("a pattern is not a topic");
    assert!(refused.to_string().contains("not publishable"), "{refused}");
}
