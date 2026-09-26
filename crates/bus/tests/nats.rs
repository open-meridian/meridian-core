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

// ── Request-reply across processes ──────────────────────────────────────────
//
// A call was answered only by a handler registered in the caller's own
// process, so a plugin asking the street store a question had no path at all once
// they were separate. These run two buses on one broker, which is the
// arrangement the split produces.

use std::sync::Arc;

use meridian_bus::{Bus, BusError};

async fn bus(instance: &str) -> Bus {
    Bus::single(instance, Arc::new(backend().await))
}

#[tokio::test]
async fn a_call_is_answered_by_a_handler_in_another_process() {
    let asking = bus("asking").await;
    let answering = bus("answering").await;
    let topic = topic("query.how-many");

    answering.serve(&topic, |envelope| {
        Ok((
            "meridian.test.Answer".to_string(),
            [b"seen: ".to_vec(), envelope.payload].concat(),
        ))
    });
    settle().await;

    let (payload_type, payload) = asking
        .call(&topic, "meridian.test.Question", b"42".to_vec(), None, None)
        .await
        .expect("the call should have been answered");

    assert_eq!(payload_type, "meridian.test.Answer");
    assert_eq!(payload, b"seen: 42".to_vec());
}

#[tokio::test]
async fn nothing_serving_is_distinct_from_nobody_answering() {
    // The caller's whole diagnosis. One is a deployment missing a component,
    // the other is a component that is too slow or gone, and a transport that
    // reported them alike would send somebody to read the wrong logs.
    let asking = bus("asking-alone").await;
    let unserved = topic("query.nobody-serves-this");

    let refused = asking
        .call(&unserved, "meridian.test.Question", vec![], None, None)
        .await
        .expect_err("nothing serves this topic");
    assert!(
        matches!(refused, BusError::NoHandler(_)),
        "expected NoHandler, got {refused:?}"
    );

    let slow_topic = topic("query.slow");
    let answering = bus("answering-slowly").await;
    answering.serve(&slow_topic, |_| {
        std::thread::sleep(Duration::from_millis(800));
        Ok((String::new(), vec![]))
    });
    settle().await;

    let timed_out = asking
        .call(
            &slow_topic,
            "meridian.test.Question",
            vec![],
            None,
            Some(Duration::from_millis(200)),
        )
        .await
        .expect_err("the handler is slower than the timeout");
    assert!(
        matches!(timed_out, BusError::Timeout { .. }),
        "expected Timeout, got {timed_out:?}"
    );
}

#[tokio::test]
async fn a_caller_waits_as_long_as_it_asked_to_and_no_less() {
    // The client library has a request timeout of its own, ten seconds unless
    // told otherwise, and its expiry was reported as the caller's: a wizard
    // that allowed Apply two minutes was told "did not answer within 120s"
    // ten seconds in, while the first-run Job carried on and finished. Found
    // on the first fresh cluster, where starting the database the wizard
    // chose takes longer than ten seconds; a warm one had always been quicker.
    let asking = bus("asking-patiently").await;
    let answering = bus("answering-in-eleven").await;
    let topic = topic("query.slower-than-ten-seconds");
    answering.serve(&topic, |_| {
        std::thread::sleep(Duration::from_secs(11));
        Ok(("meridian.test.Answer".to_string(), b"late".to_vec()))
    });
    settle().await;

    let (_, payload) = asking
        .call(
            &topic,
            "meridian.test.Question",
            vec![],
            None,
            Some(Duration::from_secs(20)),
        )
        .await
        .expect("answered within the twenty seconds the caller allowed");
    assert_eq!(payload, b"late".to_vec());
}

#[tokio::test]
async fn a_refusal_crosses_with_its_reason() {
    let asking = bus("asking-refused").await;
    let answering = bus("answering-refused").await;
    let topic = topic("query.refused");

    answering.serve(&topic, |_| Err("that instrument is not yours".to_string()));
    settle().await;

    let refused = asking
        .call(&topic, "meridian.test.Question", vec![], None, None)
        .await
        .expect_err("the handler refused");

    match refused {
        BusError::HandlerFailed { detail, .. } => {
            assert!(detail.contains("not yours"), "{detail}")
        }
        other => panic!("expected HandlerFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn a_local_handler_answers_without_the_broker() {
    // A deployment running everything in one process should not need a broker
    // to ask itself a question, and the in-process tests hold that contract
    // without one.
    let single = bus("single").await;
    let topic = topic("query.local");

    single.serve(&topic, |_| Ok(("local".to_string(), b"here".to_vec())));

    // No settle: nothing has to reach a broker for this to work.
    let (payload_type, _) = single
        .call(&topic, "meridian.test.Question", vec![], None, None)
        .await
        .expect("a local handler answers");
    assert_eq!(payload_type, "local");
}

// ── The broker enforces the contract's grants ───────────────────────────────
//
// The sidecar resolves grants locally so a plugin fails at startup. These are
// about the second check, which is what holds when a sidecar is not ours:
// a credential carrying the custody role, used to attempt what custody may not
// do, and refused by the broker rather than by our code.

/// Did this exact message arrive, within a window?
///
/// By identity rather than by silence, because these tests share real topic
/// names — permissions are written against the topics the contract names,
/// so they cannot be randomised — and they run concurrently. A subscriber
/// asserting that nothing at all arrives will eventually see a message another
/// test legitimately published, and fail for a reason that has nothing to do
/// with permissions. Found exactly that way.
async fn arrived(subscription: &mut Subscription, wanted: &str) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while let Ok(Some(delivery)) = tokio::time::timeout_at(deadline, subscription.recv()).await {
        if delivery.envelope.meta.unwrap().message_id == wanted {
            return true;
        }
    }
    false
}

/// A message identity nobody else in this run is using.
fn mark(tag: &str) -> String {
    format!("{tag}-{}", COUNTER.fetch_add(1, Ordering::SeqCst))
}

async fn as_custody() -> NatsBackend {
    let url = std::env::var("MERIDIAN_TEST_BROKER_URL_CUSTODY").expect(
        "MERIDIAN_TEST_BROKER_URL_CUSTODY is not set. These tests need a broker \
         with the generated permissions; run them with `make test-broker`.",
    );
    NatsBackend::connect(&url)
        .await
        .expect("could not reach the test broker as custody")
}

#[tokio::test]
async fn a_role_may_publish_what_the_grant_table_grants_it() {
    let custody = as_custody().await;
    let listening = backend().await;

    // Granted to custody in deploy/grants.example.json.
    let topic = "platform.reference.event.instrument-missing";
    let mut subscription = listening.subscribe(topic);
    settle().await;

    let sent = mark("granted");
    custody.publish(topic, envelope(&sent)).unwrap();

    assert!(
        arrived(&mut subscription, &sent).await,
        "a role was refused a topic its grants allow"
    );
}

#[tokio::test]
async fn the_broker_refuses_a_topic_the_role_does_not_hold() {
    // custody may record a statement; it may not answer queries about
    // positions, which is the admin role's and the reporting role's. The
    // publish call itself is accepted by the client, because a broker reports
    // a permission violation asynchronously; what proves the refusal is that
    // nothing arrives.
    let custody = as_custody().await;
    let listening = backend().await;

    let forbidden = "platform.street.event.custodial-position-updated";
    let mut subscription = listening.subscribe(forbidden);
    settle().await;

    let sent = mark("ungranted");
    custody.publish(forbidden, envelope(&sent)).unwrap();

    assert!(
        !arrived(&mut subscription, &sent).await,
        "the broker carried a message the role's grants do not allow"
    );
}

#[tokio::test]
async fn the_broker_refuses_a_subscription_the_role_does_not_hold() {
    // The contract gives custody no subscription of its own; its credential
    // hears only what every sidecar hears, such as being told its
    // configuration changed (decisions/020). A sidecar that asked for more
    // would be told nothing, rather than quietly receiving another plugin's
    // traffic.
    let custody = as_custody().await;
    let publisher = backend().await;

    let forbidden = "platform.street.event.custodial-position-updated";
    let mut refused = custody.subscribe(forbidden);
    let granted = "platform.config.event.plugin-configuration-changed";
    let mut allowed = custody.subscribe(granted);
    settle().await;

    let withheld = mark("not-for-custody");
    let expected = mark("for-custody");
    publisher.publish(forbidden, envelope(&withheld)).unwrap();
    publisher.publish(granted, envelope(&expected)).unwrap();

    assert!(
        arrived(&mut allowed, &expected).await,
        "a granted subscription heard nothing"
    );
    assert!(
        !arrived(&mut refused, &withheld).await,
        "the broker delivered on a subscription the role's grants do not allow"
    );
}

#[tokio::test]
async fn a_plugin_holding_no_role_speaks_only_its_sidecars_own_traffic() {
    // The reference plugin: admitted, with no topics but the ones every
    // sidecar has for itself (decisions/020).
    let url = std::env::var("MERIDIAN_TEST_BROKER_URL_REFERENCE")
        .expect("MERIDIAN_TEST_BROKER_URL_REFERENCE is not set; run `make test-broker`.");
    let reference = NatsBackend::connect(&url)
        .await
        .expect("could not reach the test broker as the reference plugin");
    let listening = backend().await;

    let its_sidecars = "platform.deployment.event.plugin-report";
    let a_roles = "platform.street.command.record-holding";
    let mut allowed = listening.subscribe(its_sidecars);
    let mut refused = listening.subscribe(a_roles);
    settle().await;

    let report = mark("reference-report");
    let holding = mark("reference-holding");
    reference.publish(its_sidecars, envelope(&report)).unwrap();
    reference.publish(a_roles, envelope(&holding)).unwrap();

    assert!(
        arrived(&mut allowed, &report).await,
        "a sidecar with no role was refused its own report"
    );
    assert!(
        !arrived(&mut refused, &holding).await,
        "a plugin holding no role published a role's topic"
    );
}

#[tokio::test]
async fn a_caller_with_no_credential_is_refused_the_broker_entirely() {
    // What a plugin container can reach: the address, and nothing to present.
    // The seam is the filesystem, and this is the test that tries it rather
    // than a note asking an operator to arrange it.
    let url = std::env::var("MERIDIAN_TEST_BROKER_URL").unwrap();
    let anonymous = url
        .split_once('@')
        .map(|(_, host)| format!("nats://{host}"))
        .expect("the test URL should carry a credential");

    let refused = NatsBackend::connect(&anonymous).await;
    assert!(
        refused.is_err(),
        "the broker accepted a connection with no credential"
    );
}

// ── One credential per instance ─────────────────────────────────────────────
//
// A role-wide credential carries the right to speak as every instance of that
// role, because the contract writes the instance segment as a wildcard. The
// launch identity decision — a plugin is told who it is and never says so —
// means nothing at the broker unless the credential says it too.

async fn as_custody_two() -> NatsBackend {
    let url = std::env::var("MERIDIAN_TEST_BROKER_URL_CUSTODY_TWO")
        .expect("MERIDIAN_TEST_BROKER_URL_CUSTODY_TWO is not set; run `make test-broker`.");
    NatsBackend::connect(&url)
        .await
        .expect("could not reach the test broker as the second instance")
}

#[tokio::test]
async fn an_instance_may_publish_under_its_own_identifier() {
    let custody = as_custody().await;
    let listening = backend().await;

    let own = "platform.custody.custody-test-1.event.sync-status";
    let mut subscription = listening.subscribe(own);
    settle().await;

    let sent = mark("own-identity");
    custody.publish(own, envelope(&sent)).unwrap();

    assert!(
        arrived(&mut subscription, &sent).await,
        "an instance was refused its own instance-scoped topic"
    );
}

#[tokio::test]
async fn an_instance_cannot_publish_as_another_instance_of_its_role() {
    // The property per-instance credentials exist for. Both hold the custody
    // role and identical grants; what differs is who each may claim to be.
    let custody_one = as_custody().await;
    let listening = backend().await;

    let somebody_else = "platform.custody.custody-test-2.event.sync-status";
    let mut subscription = listening.subscribe(somebody_else);
    settle().await;

    let sent = mark("impersonation");
    custody_one.publish(somebody_else, envelope(&sent)).unwrap();

    assert!(
        !arrived(&mut subscription, &sent).await,
        "one instance published under another's identifier"
    );

    // And the instance it tried to impersonate can, which proves the topic
    // itself is carried and the refusal was about identity.
    let custody_two = as_custody_two().await;
    let theirs = mark("their-own");
    custody_two
        .publish(somebody_else, envelope(&theirs))
        .unwrap();
    assert!(
        arrived(&mut subscription, &theirs).await,
        "the instance was refused its own topic"
    );
}

#[tokio::test]
async fn the_runtimes_components_may_only_touch_their_own_topics() {
    // Derived from the registry's publisher and subscriber columns, vendored
    // here by meridian-design. A component that may publish anything can
    // publish as a plugin, and the boundary would hold everywhere except at
    // the thing most able to ignore it.
    let url = std::env::var("MERIDIAN_TEST_BROKER_URL_RUNTIME")
        .expect("MERIDIAN_TEST_BROKER_URL_RUNTIME is not set; run `make test-broker`.");
    let runtime = NatsBackend::connect(&url)
        .await
        .expect("could not reach the test broker as the runtime");
    let listening = backend().await;

    // The registry says reference publishes this one.
    let its_own = "platform.reference.event.instrument-applied";
    let mut allowed = listening.subscribe(its_own);

    // And says custody publishes this one. A component is not a connector.
    let a_plugins = "platform.reference.event.instrument-missing";
    let mut refused = listening.subscribe(a_plugins);
    settle().await;

    let mine = mark("component-own");
    let theirs = mark("component-as-plugin");
    runtime.publish(its_own, envelope(&mine)).unwrap();
    runtime.publish(a_plugins, envelope(&theirs)).unwrap();

    assert!(
        arrived(&mut allowed, &mine).await,
        "a component was refused a topic the registry says it publishes"
    );
    assert!(
        !arrived(&mut refused, &theirs).await,
        "a component published a topic the registry says a plugin publishes"
    );
}

#[tokio::test]
async fn an_answer_is_announced_only_once_it_has_reached_the_broker() {
    // For a component that replies and then stops. Returning from a handler
    // says the answer was composed, not that it was sent: `publish` puts it in
    // this client's write buffer and a process that exits promptly takes the
    // buffer with it. A first run did exactly that on 2026-09-23 -- every
    // Secret written, its own rights given up, and the wizard told the job had
    // not answered in 120 seconds.
    //
    // So the signal has to come from after the send, not from the handler.
    let serving = backend().await;
    let asking = backend().await;
    let subject = topic("delivered");

    let delivered = std::sync::Arc::new(tokio::sync::Notify::new());
    let handler: meridian_bus::Handler =
        std::sync::Arc::new(|_| Ok(("meridian.v1.Answer".to_string(), b"answered".to_vec())));
    serving.serve_delivered(&subject, handler, std::sync::Arc::clone(&delivered));
    settle().await;

    // Nothing has been asked, so nothing can have been delivered.
    assert!(
        tokio::time::timeout(Duration::from_millis(200), delivered.notified())
            .await
            .is_err(),
        "an answer was announced before anybody asked for one"
    );

    let answer = asking
        .request(&subject, envelope("ask"), Duration::from_secs(5))
        .await
        .expect("the handler answered");
    assert_eq!(answer.payload, b"answered");

    // And now it has, because the caller above holds it.
    tokio::time::timeout(Duration::from_secs(5), delivered.notified())
        .await
        .expect("the answer reached the broker but was never announced");
}
