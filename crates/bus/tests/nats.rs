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
    Bus::single(
        instance,
        Arc::new(backend().await),
        Arc::new(meridian_clock::SystemClock),
    )
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

/// A refusal whose words span lines, as a database's error does with its
/// DETAIL line, is answered at once, on one line: a header holds no line
/// break, and the answer was never sent (step 3d; CI 43621875619, a
/// writer's refusal its caller waited 19.7 s for).
#[tokio::test]
async fn a_refusal_of_several_lines_crosses_on_one() {
    let asking = bus("asking-lines").await;
    let answering = bus("answering-lines").await;
    let topic = topic("query.refused-lines");

    answering.serve(&topic, |_| {
        Err(
            "db error: ERROR: duplicate key value\nDETAIL: Key (message_id) already exists.\r\n"
                .to_string(),
        )
    });
    settle().await;

    let started = std::time::Instant::now();
    let refused = asking
        .call(
            &topic,
            "meridian.test.Question",
            vec![],
            None,
            Some(Duration::from_secs(5)),
        )
        .await
        .expect_err("the handler refused");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "answered at once, not waited out ({:?})",
        started.elapsed()
    );
    match refused {
        BusError::HandlerFailed { detail, .. } => {
            // Every word, in full: each line break a space, nothing cut
            // (the header's trailing space trimmed by the broker).
            assert!(
                detail.contains(
                    "db error: ERROR: duplicate key value DETAIL: Key (message_id) already exists."
                ),
                "{detail:?}"
            );
            assert!(!detail.contains(['\r', '\n']), "{detail:?}");
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

/// A raw client, past the backend, with the credential a test URL carries.
async fn raw(variable: &str) -> async_nats::Client {
    let url = std::env::var(variable)
        .unwrap_or_else(|_| panic!("{variable} is not set; run `make test-broker`."));
    let (scheme, rest) = url.split_once("://").unwrap();
    let (credential, host) = rest.rsplit_once('@').unwrap();
    let (user, password) = credential.split_once(':').unwrap();
    async_nats::ConnectOptions::with_user_and_password(user.into(), password.into())
        .connect(format!("{scheme}://{host}"))
        .await
        .unwrap_or_else(|failed| panic!("could not reach the test broker as {user}: {failed}"))
}

/// Did this exact payload arrive on a raw subscription, within a window?
async fn heard(subscription: &mut async_nats::Subscriber, wanted: &str) -> bool {
    use futures_util::StreamExt;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while let Ok(Some(message)) = tokio::time::timeout_at(deadline, subscription.next()).await {
        if message.payload.as_ref() == wanted.as_bytes() {
            return true;
        }
    }
    false
}

#[tokio::test]
async fn the_analytics_credential_reaches_its_own_stream_and_not_the_lakes() {
    // Review C (2026-11-20), X1: every credential is in one account, so a
    // grant over JetStream's whole API reached the lake's write-ahead stream
    // from the analytics component. Its credential, under the generated
    // permissions, may call on its units' stream and is refused the lake's:
    // what the broker carries for it, not what the list says. This broker
    // runs no JetStream, so nothing answers; what proves the refusal is that
    // nothing arrives.
    let analytics = raw("MERIDIAN_TEST_BROKER_URL_ANALYTICS").await;
    let listening = raw("MERIDIAN_TEST_BROKER_URL").await;

    let its_own = "$JS.API.STREAM.INFO.ANALYTICS_UNITS";
    let the_lakes = [
        "$JS.API.STREAM.INFO.LAKE_WRITE_AHEAD",
        "$JS.API.STREAM.DELETE.LAKE_WRITE_AHEAD",
        "$JS.API.STREAM.PURGE.LAKE_WRITE_AHEAD",
    ];
    let mut allowed = listening.subscribe(its_own).await.unwrap();
    let mut refused = Vec::new();
    for subject in the_lakes {
        refused.push(listening.subscribe(subject).await.unwrap());
    }
    // And it hears no other client's JetStream call: a credential that could
    // would hear the lake's, and could answer for the server.
    let mut overheard = analytics
        .subscribe("$JS.API.STREAM.INFO.LAKE_WRITE_AHEAD")
        .await
        .unwrap();
    listening.flush().await.unwrap();
    analytics.flush().await.unwrap();
    settle().await;

    let granted = mark("analytics-own-stream");
    analytics
        .publish(its_own, granted.clone().into())
        .await
        .unwrap();
    let mut withheld = Vec::new();
    for subject in the_lakes {
        let sent = mark("analytics-the-lakes-stream");
        analytics
            .publish(subject, sent.clone().into())
            .await
            .unwrap();
        withheld.push(sent);
    }
    // A second granted call after the refused ones: the connection still
    // carries what it may, so nothing arriving above is a refusal, not a
    // connection the broker closed.
    let granted_after = mark("analytics-own-stream-after");
    analytics
        .publish(its_own, granted_after.clone().into())
        .await
        .unwrap();
    let asked_by_another = mark("the-lakes-own-call");
    listening
        .publish(
            "$JS.API.STREAM.INFO.LAKE_WRITE_AHEAD",
            asked_by_another.clone().into(),
        )
        .await
        .unwrap();
    analytics.flush().await.unwrap();
    listening.flush().await.unwrap();

    assert!(
        heard(&mut allowed, &granted).await,
        "the analytics credential was refused its own stream's info"
    );
    for ((subject, subscription), sent) in the_lakes.iter().zip(&mut refused).zip(&withheld) {
        assert!(
            !heard(subscription, sent).await,
            "the broker carried {subject} from the analytics credential"
        );
    }
    assert!(
        heard(&mut allowed, &granted_after).await,
        "the analytics credential's own stream's info was not carried after the refusals"
    );
    assert!(
        !heard(&mut overheard, &asked_by_another).await,
        "the analytics credential heard another client's call on the lake's stream"
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

    // The registry says the street publishes this one. (Not one of the
    // instrument store's, which holds its own credential from contract v20.)
    let its_own = "platform.street.event.activity-recorded";
    let mut allowed = listening.subscribe(its_own);

    // And says only custody publishes this one. A component is not a
    // connector. (Not instrument-missing, which the instrument store now
    // publishes too, for its placeholders: W3.7.)
    let a_plugins = "platform.street.command.record-holding";
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

// ── Each credential's own inboxes ───────────────────────────────────────────
//
// v31a, item 2 (the access audit's S1): with `_INBOX.>` on every credential,
// any one of them heard every answer in the deployment and could answer in
// another's place. Each now holds `_INBOX.<user>.` alone, and its client
// makes its inboxes there.

#[tokio::test]
async fn a_credential_cannot_hear_another_credentials_inbox() {
    let one = raw("MERIDIAN_TEST_BROKER_URL_CUSTODY").await;
    let two = raw("MERIDIAN_TEST_BROKER_URL_CUSTODY_TWO").await;
    let sending = raw("MERIDIAN_TEST_BROKER_URL").await;

    let theirs = "_INBOX.custody-test-2.probe";
    let mut overheard = one.subscribe(theirs).await.unwrap();
    let mut own = two.subscribe(theirs).await.unwrap();
    one.flush().await.unwrap();
    two.flush().await.unwrap();
    settle().await;

    let answer = mark("an-answer-for-custody-test-2");
    sending
        .publish(theirs, answer.clone().into())
        .await
        .unwrap();
    sending.flush().await.unwrap();

    assert!(
        heard(&mut own, &answer).await,
        "a credential did not hear its own inbox"
    );
    assert!(
        !heard(&mut overheard, &answer).await,
        "custody-test-1 heard custody-test-2's inbox"
    );
}

#[tokio::test]
async fn a_credential_cannot_publish_into_another_credentials_inbox() {
    let one = raw("MERIDIAN_TEST_BROKER_URL_CUSTODY").await;
    let two = raw("MERIDIAN_TEST_BROKER_URL_CUSTODY_TWO").await;
    let listening = raw("MERIDIAN_TEST_BROKER_URL").await;

    let theirs = "_INBOX.custody-test-2.forged";
    let mut inbox = listening.subscribe(theirs).await.unwrap();
    listening.flush().await.unwrap();
    settle().await;

    let forged = mark("custody-test-1-answers-for-another");
    one.publish(theirs, forged.clone().into()).await.unwrap();
    one.flush().await.unwrap();
    let own = mark("custody-test-2-on-its-own-inbox");
    two.publish(theirs, own.clone().into()).await.unwrap();
    two.flush().await.unwrap();

    // One window for both: whatever arrives is read once.
    use futures_util::StreamExt;
    let mut arrived = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while let Ok(Some(message)) = tokio::time::timeout_at(deadline, inbox.next()).await {
        arrived.push(String::from_utf8_lossy(&message.payload).to_string());
    }
    assert!(
        !arrived.contains(&forged),
        "custody-test-1 published into custody-test-2's inbox"
    );
    assert!(
        arrived.contains(&own),
        "custody-test-2 was refused its own inbox"
    );
}

/// The second review of v31a-core-2, finding 2 (option A, approved by
/// consensus 2026-11-20), as the broker carries it: into each of a plugin's,
/// a component instance's and an own-credential component's inboxes, a
/// message no request carried is refused from every other credential here --
/// and carried from the runtime's alone, the stated exception, which answers
/// every one of them and so leaves no reply entry on its connections.
#[tokio::test]
async fn only_the_runtime_publishes_into_another_credentials_inbox() {
    const CREDENTIALS: [(&str, &str); 7] = [
        ("custody-test-1", "MERIDIAN_TEST_BROKER_URL_CUSTODY"),
        ("custody-test-2", "MERIDIAN_TEST_BROKER_URL_CUSTODY_TWO"),
        ("reference-1", "MERIDIAN_TEST_BROKER_URL_REFERENCE"),
        ("dashboard-1", "MERIDIAN_TEST_BROKER_URL_DASHBOARD"),
        ("instrument", "MERIDIAN_TEST_BROKER_URL_INSTRUMENT"),
        ("analytics", "MERIDIAN_TEST_BROKER_URL_ANALYTICS"),
        ("runtime", "MERIDIAN_TEST_BROKER_URL_RUNTIME"),
    ];
    let listening = raw("MERIDIAN_TEST_BROKER_URL").await;
    let mut clients = Vec::new();
    for (name, variable) in CREDENTIALS {
        clients.push((name, raw(variable).await));
    }
    for owner in ["custody-test-2", "dashboard-1", "instrument"] {
        let theirs = format!("_INBOX.{owner}.unasked");
        let mut inbox = listening.subscribe(theirs.clone()).await.unwrap();
        listening.flush().await.unwrap();
        settle().await;
        let mut sent = Vec::new();
        for (name, client) in &clients {
            let said = mark(&format!("{name}-into-{owner}"));
            client
                .publish(theirs.clone(), said.clone().into())
                .await
                .unwrap();
            client.flush().await.unwrap();
            sent.push((*name, said));
        }
        use futures_util::StreamExt;
        let mut arrived = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while let Ok(Some(message)) = tokio::time::timeout_at(deadline, inbox.next()).await {
            arrived.push(String::from_utf8_lossy(&message.payload).to_string());
        }
        for (name, said) in sent {
            let carried = arrived.contains(&said);
            let expected = name == owner || name == "runtime";
            assert_eq!(
                carried, expected,
                "{name} into {owner}'s inbox: carried {carried}, expected {expected}"
            );
        }
    }
}

/// The second review of v31a-core-2, option A's amendment (e): a broker
/// configuration reload -- every plugin launch makes one -- registers each
/// client's user again, and its permissions with it, which empties the
/// connection's reply entries. A non-runtime responder (custody-test-1)
/// delivered a request of the runtime's before a reload and answering after
/// it then has no right to the runtime's inbox.
///
/// Run by hand, alone, with the reload made from outside while it waits: it
/// says `RELOAD-NOW` and waits for the broker's `config_load_time` to change,
/// then answers. The configuration the broker reloads must change its users
/// (a launch adds one); a reload of the same file re-registers nobody.
///
/// A known gap, found 2026-11-20 on nats 2.10.29 (`Publish Violation - User
/// "custody-test-1", Subject "_INBOX.runtime..."` after `Reloaded:
/// authorization users`): the answer is refused. Its fix goes through the
/// proposal procedure; until then this stays ignored, and it passes once the
/// fix is in.
#[tokio::test]
#[ignore = "a known gap: a configuration reload drops in-flight reply rights; needs a reload made from outside"]
async fn an_answer_in_flight_across_a_configuration_reload_reaches_the_runtime() {
    use futures_util::StreamExt;
    let runtime = raw("MERIDIAN_TEST_BROKER_URL_RUNTIME").await;
    let responder = raw("MERIDIAN_TEST_BROKER_URL_CUSTODY").await;
    let topic = "platform.config.event.plugin-configuration-changed";
    let mut asked = responder.subscribe(topic).await.unwrap();
    responder.flush().await.unwrap();
    let reply = format!("_INBOX.runtime.{}", mark("across-a-reload"));
    let mut answers = runtime.subscribe(reply.clone()).await.unwrap();
    runtime.flush().await.unwrap();
    settle().await;
    let question = mark("asked-before-the-reload");
    runtime
        .publish_with_reply(topic, reply.clone(), question.clone().into())
        .await
        .unwrap();
    runtime.flush().await.unwrap();
    let request = loop {
        let message = tokio::time::timeout(Duration::from_secs(5), asked.next())
            .await
            .expect("the responder was delivered the request")
            .unwrap();
        if message.payload.as_ref() == question.as_bytes() {
            break message;
        }
    };
    let loaded = |varz: &str| said_text(varz, "config_load_time");
    let before = loaded(&monitored("/varz").await).expect("config_load_time");
    println!("RELOAD-NOW");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        if loaded(&monitored("/varz").await).as_deref() != Some(before.as_str()) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the broker did not reload within two minutes"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    settle().await;
    let answer = mark("answered-after-the-reload");
    responder
        .publish(
            request.reply.clone().expect("a reply subject"),
            answer.clone().into(),
        )
        .await
        .unwrap();
    responder.flush().await.unwrap();
    assert!(
        heard(&mut answers, &answer).await,
        "the answer sent after a configuration reload did not reach the runtime"
    );
}

#[tokio::test]
async fn an_answer_reaches_its_caller_on_the_callers_own_inbox() {
    // Asked as custody-test-1 on what every sidecar asks, answered by the
    // runtime's credential, which holds no inbox of custody-test-1's: the
    // caller's client makes its inbox under its own prefix, and the broker
    // lets the one asked answer once on it.
    let asking = Bus::single(
        "custody-test-1",
        Arc::new(as_custody().await),
        Arc::new(meridian_clock::SystemClock),
    );
    let runtime = NatsBackend::connect(
        &std::env::var("MERIDIAN_TEST_BROKER_URL_RUNTIME")
            .expect("MERIDIAN_TEST_BROKER_URL_RUNTIME is not set; run `make test-broker`."),
    )
    .await
    .expect("could not reach the test broker as the runtime");
    let answering = Bus::single(
        "conductor",
        Arc::new(runtime),
        Arc::new(meridian_clock::SystemClock),
    );
    let topic = "platform.config.query.plugin-configuration";
    let tag = mark("inbox-answer");
    let expected = tag.clone();
    answering.serve(topic, move |envelope| {
        Ok((
            "meridian.test.Answer".to_string(),
            [expected.as_bytes(), b":", &envelope.payload].concat(),
        ))
    });
    settle().await;

    let (_, payload) = asking
        .call(
            topic,
            "meridian.test.Question",
            b"mine".to_vec(),
            None,
            None,
        )
        .await
        .expect("the call was answered on the caller's own inbox");
    assert_eq!(payload, format!("{tag}:mine").into_bytes());
}

/// v31a, item 3 (INTERIM(v32)): a handler knows which credential asked, as
/// the owner of the inbox its answer goes to -- whatever sender the request
/// names, which its sidecar sets.
#[tokio::test]
async fn a_handler_knows_the_credential_that_asked_by_its_inbox() {
    let asking = Bus::single(
        "custody-test-2",
        Arc::new(as_custody().await),
        Arc::new(meridian_clock::SystemClock),
    );
    let runtime = NatsBackend::connect(
        &std::env::var("MERIDIAN_TEST_BROKER_URL_RUNTIME")
            .expect("MERIDIAN_TEST_BROKER_URL_RUNTIME is not set; run `make test-broker`."),
    )
    .await
    .expect("could not reach the test broker as the runtime");
    let answering = Bus::single(
        "conductor",
        Arc::new(runtime),
        Arc::new(meridian_clock::SystemClock),
    );
    // Every sidecar asks it, and the conductor answers it; no other test
    // here serves it.
    let topic = "platform.config.query.plugin-access";
    answering.serve(topic, |_| {
        Ok((
            "meridian.test.Asker".to_string(),
            meridian_bus::inbox::asked_by()
                .unwrap_or_else(|| "nobody".into())
                .into_bytes(),
        ))
    });
    settle().await;

    // The bus names itself custody-test-2; its credential is custody-test-1's.
    let (_, payload) = asking
        .call(topic, "meridian.test.Question", Vec::new(), None, None)
        .await
        .expect("answered");
    assert_eq!(String::from_utf8_lossy(&payload), "custody-test-1");
}

/// The review of v31a-core-2, F1(c), a known gap (INTERIM(v32)): the broker
/// checks no publisher's right on a reply subject, so a credential that names
/// another's inbox as its reply-to is read as that other. custody-test-1
/// sends a take-shard naming custody-test-2, answered on custody-test-2's
/// inbox, and is asked as custody-test-2: the lake would number a new epoch
/// and fence custody-test-2's live holder, as often as it is sent. Ignored
/// until v32 binds the sender at the broker (S2) and its ownership record
/// replaces this take (v32.8); it then passes, and the ignore is removed.
#[tokio::test]
#[ignore = "INTERIM(v32): a known gap, the asker read from the reply inbox; v32.8 turns it green"]
async fn a_take_shard_sent_on_another_credentials_inbox_is_not_asked_as_it() {
    let forging = raw("MERIDIAN_TEST_BROKER_URL_CUSTODY").await;
    let runtime = NatsBackend::connect(
        &std::env::var("MERIDIAN_TEST_BROKER_URL_RUNTIME")
            .expect("MERIDIAN_TEST_BROKER_URL_RUNTIME is not set; run `make test-broker`."),
    )
    .await
    .expect("could not reach the test broker as the runtime");
    let answering = Bus::single(
        "lake",
        Arc::new(runtime),
        Arc::new(meridian_clock::SystemClock),
    );
    // Every sidecar may send it, and the lake answers it; no other test here
    // serves it.
    let topic = "platform.lake.command.take-shard";
    let (told, mut asked) = tokio::sync::mpsc::unbounded_channel();
    answering.serve(topic, move |_| {
        let _ = told.send(meridian_bus::inbox::asked_by());
        Ok(("meridian.test.Taken".to_string(), Vec::new()))
    });
    settle().await;

    let take = Envelope {
        meta: Some(MessageMeta {
            message_id: mark("forged-take"),
            publisher_instance_id: "custody-test-2".into(),
            ..Default::default()
        }),
        payload_type: "meridian.v1.TakeShardRequest".into(),
        payload: b"instance: custody-test-2".to_vec(),
    };
    forging
        .publish_with_reply(
            topic,
            "_INBOX.custody-test-2.x",
            prost::Message::encode_to_vec(&take).into(),
        )
        .await
        .unwrap();
    forging.flush().await.unwrap();

    let asker = tokio::time::timeout(Duration::from_secs(5), asked.recv())
        .await
        .expect("the take reached the lake")
        .expect("its handler said who asked");
    assert_ne!(
        asker.as_deref(),
        Some("custody-test-2"),
        "custody-test-1's take was asked as custody-test-2"
    );
}

/// v31a, item 14 (decision 47), the triage on a real broker: an access
/// change sent by every credential this broker admits but the dashboard's is
/// refused; the dashboard's is carried.
#[tokio::test]
async fn only_the_dashboard_may_publish_an_access_change() {
    let listening = raw("MERIDIAN_TEST_BROKER_URL").await;
    let topic = "platform.config.command.define-user-group";
    let mut heard_on = listening.subscribe(topic).await.unwrap();
    listening.flush().await.unwrap();
    settle().await;

    let mut withheld = Vec::new();
    for credential in [
        "MERIDIAN_TEST_BROKER_URL_CUSTODY",
        "MERIDIAN_TEST_BROKER_URL_REFERENCE",
        "MERIDIAN_TEST_BROKER_URL_RUNTIME",
        "MERIDIAN_TEST_BROKER_URL_INSTRUMENT",
        "MERIDIAN_TEST_BROKER_URL_ANALYTICS",
    ] {
        let client = raw(credential).await;
        let sent = mark(&format!("access-change-by-{credential}"));
        client.publish(topic, sent.clone().into()).await.unwrap();
        client.flush().await.unwrap();
        withheld.push((credential, sent));
    }
    let dashboard = raw("MERIDIAN_TEST_BROKER_URL_DASHBOARD").await;
    let carried = mark("access-change-by-the-dashboard");
    dashboard
        .publish(topic, carried.clone().into())
        .await
        .unwrap();
    dashboard.flush().await.unwrap();

    use futures_util::StreamExt;
    let mut heard = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while let Ok(Some(message)) = tokio::time::timeout_at(deadline, heard_on.next()).await {
        heard.push(String::from_utf8_lossy(&message.payload).to_string());
    }
    assert!(
        heard.contains(&carried),
        "the dashboard's access change was not carried"
    );
    for (credential, sent) in withheld {
        assert!(
            !heard.contains(&sent),
            "{credential} published an access change"
        );
    }
}

#[tokio::test]
async fn the_instrument_store_asks_for_the_plugin_catalogue_with_its_own_credential() {
    // Contract v20 (the product owner, 2026-10-11): the instrument store reads
    // the launch records by its own column in the contract, with a credential
    // of its own; the runtime's shared one no longer carries its topics.
    let url = std::env::var("MERIDIAN_TEST_BROKER_URL_INSTRUMENT")
        .expect("MERIDIAN_TEST_BROKER_URL_INSTRUMENT is not set; run `make test-broker`.");
    let instrument = NatsBackend::connect(&url)
        .await
        .expect("could not reach the test broker as the instrument store");
    let runtime = NatsBackend::connect(
        &std::env::var("MERIDIAN_TEST_BROKER_URL_RUNTIME")
            .expect("MERIDIAN_TEST_BROKER_URL_RUNTIME is not set; run `make test-broker`."),
    )
    .await
    .expect("could not reach the test broker as the runtime");
    let listening = backend().await;

    let catalogue = "platform.config.query.plugin-catalogue";
    let mut asked = listening.subscribe(catalogue);
    let applied = "platform.reference.event.instrument-applied";
    let mut heard = listening.subscribe(applied);
    // A street topic, which is not the instrument store's.
    let street = "platform.street.event.activity-recorded";
    let mut refused = listening.subscribe(street);
    settle().await;

    let ask = mark("instrument-asks-the-catalogue");
    instrument.publish(catalogue, envelope(&ask)).unwrap();
    assert!(
        arrived(&mut asked, &ask).await,
        "the instrument store was refused the plugin catalogue with its own credential"
    );
    let not_runtime = mark("runtime-as-instrument");
    runtime.publish(applied, envelope(&not_runtime)).unwrap();
    assert!(
        !arrived(&mut heard, &not_runtime).await,
        "the runtime's shared credential published the instrument store's topic"
    );
    let not_street = mark("instrument-as-street");
    instrument.publish(street, envelope(&not_street)).unwrap();
    assert!(
        !arrived(&mut refused, &not_street).await,
        "the instrument store published a topic the registry gives the street"
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
    serving.serve_delivered(
        &subject,
        "delivering",
        handler,
        std::sync::Arc::clone(&delivered),
    );
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

// ── A slow subscriber loses messages alone, and the loss is counted ─────────
//
// decisions/004: at-most-once, with a bounded queue per subscriber that drops
// and counts on overflow. The forwarding task once waited for room instead,
// so a slow subscriber backed its stream up and the count every reader of
// losses relies on stayed at zero. The memory backend behaved as documented,
// so nothing that ran without a broker noticed.

/// The backend's per-subscriber bound, which these tests overflow on purpose.
const QUEUE: usize = 1024;

/// Wait for a count to reach what it should, or give up and report it.
async fn reaches(count: impl Fn() -> u64, wanted: u64) -> u64 {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while count() < wanted && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    count()
}

#[tokio::test]
async fn a_full_subscriber_queue_drops_and_counts_and_a_healthy_one_hears_everything() {
    const OVER: usize = 50;
    let publisher = backend().await;
    // Both on one connection, so the slow one is in a position to stall the
    // other if it backs up rather than drops.
    let subscriber = backend().await;
    let topic = topic("event.flood");

    let slow = subscriber.subscribe(&topic);
    let mut healthy = subscriber.subscribe(&topic);
    settle().await;

    let reading = tokio::spawn(async move {
        let mut heard = 0;
        while heard < QUEUE + OVER {
            match next(&mut healthy).await {
                Some(_) => heard += 1,
                None => break,
            }
        }
        (heard, healthy)
    });

    for sent in 0..(QUEUE + OVER) {
        publisher
            .publish(&topic, envelope(&format!("flood-{sent}")))
            .unwrap();
    }

    let (heard, healthy) = reading.await.expect("the reader finished");
    assert_eq!(
        heard,
        QUEUE + OVER,
        "the healthy subscriber missed messages"
    );
    assert_eq!(
        healthy.dropped(),
        0,
        "the healthy subscriber counted a loss"
    );

    let lost = reaches(|| slow.dropped(), OVER as u64).await;
    assert_eq!(
        lost, OVER as u64,
        "the slow subscriber's overflow was not counted"
    );
    assert_eq!(
        subscriber.dropped(),
        OVER as u64,
        "the backend's total disagrees"
    );
}

#[tokio::test]
async fn bytes_that_are_not_an_envelope_are_dropped_and_counted() {
    let subscriber = backend().await;
    let topic = topic("event.not-ours");
    let mut subscription = subscriber.subscribe(&topic);
    settle().await;

    // Straight onto the broker, past the backend, which only ever sends
    // envelopes. The credential is in the URL, as the backend reads it.
    let url = std::env::var("MERIDIAN_TEST_BROKER_URL").unwrap();
    let (scheme, rest) = url.split_once("://").unwrap();
    let (credential, host) = rest.rsplit_once('@').unwrap();
    let (user, password) = credential.split_once(':').unwrap();
    let raw = async_nats::ConnectOptions::with_user_and_password(user.into(), password.into())
        .connect(format!("{scheme}://{host}"))
        .await
        .expect("could not reach the test broker directly");
    raw.publish(topic.clone(), vec![0xff, 0xff, 0xff].into())
        .await
        .unwrap();
    raw.flush().await.unwrap();

    assert_eq!(reaches(|| subscription.dropped(), 1).await, 1);
    assert_eq!(subscriber.dropped(), 1);
    assert!(
        tokio::time::timeout(Duration::from_millis(300), subscription.recv())
            .await
            .is_err(),
        "something that was not an envelope was delivered"
    );
}

// ── Queue groups (contract v25; Q15; kernel/the-bus-has-no-queue-groups) ──

#[tokio::test]
async fn two_copies_of_a_component_share_its_work_and_others_still_hear_each() {
    // Two processes of one component, subscribed as one of us, see each
    // message once between them; a plain subscriber elsewhere hears every
    // one, and another component's copy too.
    let publisher = bus("publisher").await;
    let first = bus("street").await;
    let second = bus("street").await;
    let dashboard = bus("dashboard").await;
    let topic = topic("event.work");

    let mut a = first.subscribe_shared(&topic, "keeping");
    let mut b = second.subscribe_shared(&topic, "keeping");
    let mut every = dashboard.subscribe_every(&topic);
    settle().await;

    let sent = 20;
    for n in 0..sent {
        publisher
            .publish(&topic, "meridian.test.Event", vec![n], None, None)
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let drain = |subscription: &mut Subscription| {
        let mut seen = Vec::new();
        while let Some(delivery) = subscription.try_recv() {
            seen.push(delivery.envelope.payload[0]);
        }
        seen
    };
    let (mut shared, heard) = (drain(&mut a), drain(&mut every));
    shared.extend(drain(&mut b));
    shared.sort();
    assert_eq!(
        shared,
        (0..sent).collect::<Vec<u8>>(),
        "each once between the two"
    );
    assert_eq!(
        heard.len(),
        sent as usize,
        "a plain subscriber hears every one"
    );
}

#[tokio::test]
async fn a_request_is_answered_once_by_one_copy_of_the_component() {
    // Every copy serving a topic is one group: the request is handled once,
    // not once per copy with the extra answers thrown away.
    let asking = bus("asking").await;
    let first = bus("conductor").await;
    let second = bus("conductor").await;
    let topic = topic("query.once");
    let handled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for copy in [&first, &second] {
        let counting = Arc::clone(&handled);
        copy.serve(&topic, move |_| {
            counting.fetch_add(1, Ordering::SeqCst);
            Ok(("meridian.test.Answer".to_string(), Vec::new()))
        });
    }
    settle().await;
    for _ in 0..10 {
        asking
            .call(&topic, "meridian.test.Question", Vec::new(), None, None)
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        handled.load(Ordering::SeqCst),
        10,
        "each request handled once"
    );
}

// ── An answer past the broker's limit ───────────────────────────────────────

/// A catalogue past the broker's 1 MiB: many versions, each with a large
/// declaration -- tools with long descriptions and schemas, as a plugin's
/// documentation makes them.
fn a_catalogue_past_a_mebibyte() -> meridian_domain::v1::PluginCatalogue {
    use meridian_domain::v1::{PluginCatalogue, PluginMetadata, PluginVersion};
    use meridian_pb::v1::{PluginDeclaration, ToolDeclaration};
    let words = "what this tool reads and why, in the vendor's words. ".repeat(40);
    let versions = (0..240)
        .map(|n| PluginVersion {
            metadata: Some(PluginMetadata {
                name: format!("plugin-{}", n % 22),
                version: format!("0.{}.0", n / 22),
                declaration: Some(PluginDeclaration {
                    tools: (0..2)
                        .map(|t| ToolDeclaration {
                            name: format!("read_{t}"),
                            description: words.clone(),
                            input_schema: format!("{{\"description\": \"{words}\"}}"),
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            image_digest: format!("sha256:{n:064x}"),
            ..Default::default()
        })
        .collect();
    PluginCatalogue {
        versions,
        ..Default::default()
    }
}

#[tokio::test]
async fn an_answer_past_the_brokers_limit_is_refused_naming_its_topic_and_size() {
    // Found on dev, 2026-10-26: the plugin catalogue grew to 1,050,251
    // bytes, the broker would not carry it, and every caller waited out ten
    // seconds for an answer that was never sent. Now the caller is told at
    // once, and why.
    use prost::Message as _;
    let asking = bus("asking-for-too-much").await;
    let answering = bus("answering-too-much").await;
    let topic = topic("query.plugin-catalogue");
    let catalogue = a_catalogue_past_a_mebibyte().encode_to_vec();
    let size = catalogue.len();
    assert!(size > 1_048_576, "the catalogue is {size} bytes");

    answering.serve(&topic, move |_| {
        Ok(("meridian.v1.PluginCatalogue".to_string(), catalogue.clone()))
    });
    settle().await;

    let started = std::time::Instant::now();
    let refused = asking
        .call(
            &topic,
            "meridian.v1.PluginCatalogueRequest",
            vec![],
            None,
            Some(Duration::from_secs(10)),
        )
        .await
        .expect_err("the answer is past the broker's limit");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "refused at once, not timed out: {:?}",
        started.elapsed()
    );
    match refused {
        BusError::HandlerFailed {
            topic: named,
            detail,
        } => {
            assert_eq!(named, topic);
            assert!(detail.contains(&format!("`{topic}`")), "{detail}");
            assert!(
                detail.contains("bytes, past the broker's limit of"),
                "{detail}"
            );
            assert!(detail.contains("1048576 bytes"), "{detail}");
        }
        other => panic!("expected the refusal, got {other:?}"),
    }

    // The answering side still answers what fits.
    let small = topic.clone() + "-small";
    answering.serve(&small, |_| {
        Ok(("meridian.v1.PluginCatalogue".to_string(), vec![]))
    });
    settle().await;
    asking
        .call(
            &small,
            "meridian.v1.PluginCatalogueRequest",
            vec![],
            None,
            None,
        )
        .await
        .expect("an answer that fits is answered");
}

#[tokio::test]
async fn a_question_past_the_brokers_limit_is_refused_before_it_is_sent() {
    let asking = bus("asking-too-much").await;
    let topic = topic("query.too-large-to-ask");
    let refused = asking
        .call(
            &topic,
            "meridian.test.Question",
            vec![0u8; 2 * 1_048_576],
            None,
            Some(Duration::from_secs(10)),
        )
        .await
        .expect_err("the question is past the broker's limit");
    assert!(
        matches!(
            refused,
            BusError::TooLarge {
                most: 1_048_576,
                ..
            }
        ),
        "expected TooLarge, got {refused:?}"
    );
    assert!(
        refused.to_string().contains(&format!("`{topic}`")),
        "{refused}"
    );
}

/// A TCP relay to the test broker that can go silent: every connection kept
/// open, nothing carried either way -- a server cut off, or on a node that
/// stopped, which sends no reset and no error.
struct Relay {
    port: u16,
    silent: Arc<std::sync::atomic::AtomicBool>,
    carrying: Arc<AtomicU64>,
}

impl Relay {
    async fn to(broker: &str) -> Relay {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let silent = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let carrying = Arc::new(AtomicU64::new(0));
        let (quiet, count, broker) = (silent.clone(), carrying.clone(), broker.to_string());
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let Ok(server) = tokio::net::TcpStream::connect(&broker).await else {
                    continue;
                };
                count.fetch_add(1, Ordering::SeqCst);
                let (from_client, to_client) = client.into_split();
                let (from_server, to_server) = server.into_split();
                tokio::spawn(carry(from_client, to_server, quiet.clone()));
                tokio::spawn(carry(from_server, to_client, quiet.clone()));
            }
        });
        Relay {
            port,
            silent,
            carrying,
        }
    }
}

/// Bytes from one side to the other, until either closes; while silent,
/// read and dropped, as packets are.
async fn carry(
    mut from: impl tokio::io::AsyncRead + Unpin,
    mut to: impl tokio::io::AsyncWrite + Unpin,
    quiet: Arc<std::sync::atomic::AtomicBool>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match from.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) if quiet.load(Ordering::SeqCst) => {}
            Ok(n) => {
                if to.write_all(&buf[..n]).await.is_err() {
                    return;
                }
            }
        }
    }
}

#[tokio::test]
async fn a_server_gone_silent_is_left_for_another_within_the_heartbeat() {
    // Step 3b: a broker server cut off, then lost, sends its clients nothing.
    // A client publishing often never pinged it (each publish puts the ping
    // off), and a writer's subscriptions on it were served by nobody until
    // the lease's own bound. The heartbeat moves the client within
    // UNANSWERED of the silence, and its subscriptions with it.
    let url =
        std::env::var("MERIDIAN_TEST_BROKER_URL").expect("MERIDIAN_TEST_BROKER_URL is not set");
    let (scheme, rest) = url.split_once("://").unwrap_or(("nats", &url));
    let (credential, hosts) = match rest.rsplit_once('@') {
        Some((credential, hosts)) => (format!("{credential}@"), hosts),
        None => (String::new(), rest),
    };
    let broker = hosts.split(',').next().unwrap().trim().to_string();
    let (a, b) = (Relay::to(&broker).await, Relay::to(&broker).await);
    let through = format!(
        "{scheme}://{credential}127.0.0.1:{},127.0.0.1:{}",
        a.port, b.port
    );
    let hearing = NatsBackend::connect(&through)
        .await
        .expect("the relays carry the broker");
    let telling = backend().await;
    let topic = topic("event.heard-after-a-silence");
    let mut subscription = hearing.subscribe(&topic);
    settle().await;

    let used = if a.carrying.load(Ordering::SeqCst) > 0 {
        &a
    } else {
        &b
    };
    used.silent.store(true, Ordering::SeqCst);
    let silenced = std::time::Instant::now();
    let mut heard = None;
    while silenced.elapsed() < Duration::from_secs(15) {
        telling
            .publish(&topic, envelope("DEP-after"))
            .expect("the publish should have been accepted");
        if let Ok(Some(delivery)) =
            tokio::time::timeout(Duration::from_millis(250), subscription.recv()).await
        {
            heard = Some((delivery, silenced.elapsed()));
            break;
        }
    }
    let (delivery, after) = heard.expect("never heard again after its server went silent");
    assert_eq!(delivery.envelope.meta.unwrap().message_id, "DEP-after");
    // The silence noticed within HEARTBEAT + UNANSWERED, the silent server
    // perhaps tried once more (its own UNANSWERED), and the other taken.
    let most = NatsBackend::HEARTBEAT + NatsBackend::UNANSWERED * 2 + Duration::from_secs(2);
    assert!(
        after < most,
        "heard again only {after:?} after the silence (at most {most:?})"
    );
}

#[tokio::test]
async fn a_connection_lost_counts_a_loss_on_every_subscription_and_hears_again() {
    // The resilience spec's section 10a: whatever the broker held for a
    // client while its connection was down is gone, and the library's
    // resubscription says nothing of it. Counted on each subscription, a
    // sidecar puts a `Lost` in its plugin's stream and the plugin reads again.
    let publisher = backend().await;
    let subscriber = backend().await;
    let (one, two) = (topic("event.before-a-break"), topic("event.also-before"));
    let mut first = subscriber.subscribe(&one);
    let second = subscriber.subscribe(&two);
    settle().await;
    assert_eq!((first.dropped(), second.dropped()), (0, 0));

    subscriber
        .reconnect()
        .await
        .expect("the connection is made again");
    assert_eq!(
        reaches(|| first.dropped(), 1).await,
        1,
        "no loss counted on the first"
    );
    assert_eq!(
        reaches(|| second.dropped(), 1).await,
        1,
        "no loss counted on the second"
    );
    assert!(subscriber.dropped() >= 2, "the backend's total disagrees");

    // And heard again once it is back.
    let mut heard = None;
    for attempt in 0..50 {
        publisher
            .publish(&one, envelope(&format!("after-{attempt}")))
            .unwrap();
        if let Ok(Some(delivery)) =
            tokio::time::timeout(Duration::from_millis(200), first.recv()).await
        {
            heard = Some(delivery);
            break;
        }
    }
    assert!(
        heard.is_some(),
        "nothing heard after the connection came back"
    );
}

/// A number the broker said in its JSON, by its key: `"key":123`.
fn said_number(json: &str, key: &str) -> Option<u64> {
    let (_, after) = json.split_once(&format!("\"{key}\":"))?;
    let digits: String = after
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// A string the broker said in its JSON, by its key: `"key": "text"`.
fn said_text(json: &str, key: &str) -> Option<String> {
    let (_, after) = json.split_once(&format!("\"{key}\":"))?;
    let (_, text) = after.trim_start().split_once('"')?;
    text.split_once('"').map(|(said, _)| said.to_string())
}

/// One request to the test broker's monitoring endpoint, its body.
async fn monitored(path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let monitor = std::env::var("MERIDIAN_TEST_BROKER_MONITOR")
        .expect("MERIDIAN_TEST_BROKER_MONITOR is not set; run these with `make test-broker`");
    let host = monitor.trim_start_matches("http://").trim_end_matches('/');
    let mut stream = tokio::net::TcpStream::connect(host)
        .await
        .expect("the broker's monitoring endpoint answers");
    stream
        .write_all(format!("GET {path} HTTP/1.0\r\nHost: {host}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut said = String::new();
    stream.read_to_string(&mut said).await.unwrap();
    said.split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or(said)
}

#[tokio::test]
async fn a_client_that_stops_reading_is_cut_off_alone_and_every_other_keeps_its_deliveries() {
    // The resilience spec's section 10a: the broker bounds each client's
    // pending bytes (`broker.limits.maxPending`, the broker's own 64 MB by
    // default); a client that stops reading is cut off alone, as NATS does
    // with a slow consumer, and a healthy subscriber of the same subject
    // hears every message.
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    const EACH: usize = 256 * 1024;
    const SENT: usize = 512; // 128 MiB, twice the pending bound
    let url =
        std::env::var("MERIDIAN_TEST_BROKER_URL").expect("MERIDIAN_TEST_BROKER_URL is not set");
    let rest = url.split_once("://").map_or(url.as_str(), |(_, rest)| rest);
    let (credential, host) = rest.rsplit_once('@').expect("the test URL names its user");
    let (user, pass) = credential.split_once(':').unwrap();
    let subject = topic("event.read-by-nobody");

    // A client that subscribes and then never reads its socket again.
    let stalled = tokio::net::TcpStream::connect(host).await.unwrap();
    let (reading, mut writing) = stalled.into_split();
    let mut reading = tokio::io::BufReader::new(reading);
    let mut info = String::new();
    reading.read_line(&mut info).await.unwrap();
    let cid = said_number(&info, "client_id").expect("the server names this client");
    writing
        .write_all(
            format!(
                "CONNECT {{\"user\":{user:?},\"pass\":{pass:?},\"verbose\":false,\"pedantic\":false}}\r\n\
                 SUB {subject} 1\r\nPING\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut pong = String::new();
    reading.read_line(&mut pong).await.unwrap();
    assert_eq!(
        pong.trim(),
        "PONG",
        "the stalled client was not admitted: {pong}"
    );

    let subscriber = backend().await;
    let mut healthy = subscriber.subscribe(&subject);
    settle().await;
    let hearing = tokio::spawn(async move {
        let mut heard = 0;
        while heard < SENT {
            match next(&mut healthy).await {
                Some(_) => heard += 1,
                None => break,
            }
        }
        (heard, healthy)
    });

    let publisher = backend().await;
    for sent in 0..SENT {
        let mut message = envelope(&format!("bulk-{sent}"));
        message.payload = vec![7u8; EACH];
        publisher.publish(&subject, message).unwrap();
    }
    let (heard, healthy) = hearing.await.unwrap();
    assert_eq!(heard, SENT, "the healthy subscriber missed messages");
    assert_eq!(
        healthy.dropped(),
        0,
        "the healthy subscriber counted a loss"
    );

    // The stalled one was closed by the broker as a slow consumer, not
    // dropped as stale.
    let mut reason = String::new();
    for _ in 0..50 {
        let closed = monitored(&format!("/connz?state=closed&cid={cid}")).await;
        if let Some(said) = said_text(&closed, "reason") {
            reason = said;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        reason.contains("Slow Consumer"),
        "the stalled client was not cut off as a slow consumer: {reason:?}"
    );
    // And what it was sent ends: closed, short of what was published.
    let mut left = 0usize;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match tokio::time::timeout(Duration::from_secs(5), reading.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => break,
            Ok(Ok(n)) => left += n,
            Err(_) => panic!("the stalled client's connection is still open after {left} bytes"),
        }
    }
    assert!(
        left < SENT * EACH,
        "the stalled client was sent everything: {left} bytes"
    );
}

/// Step 3d (section 6.7): a write the request says another writer group
/// owns is passed to that group's member before it is admitted here, the
/// caller's reply address kept: the owner answers the caller itself, this
/// member's handler never runs, and it waits on nothing.
#[tokio::test]
async fn a_write_another_group_owns_is_passed_to_it_and_answered_by_it() {
    let instance = format!("store-{}", mark("pass"));
    let write = topic("command.record");
    let internal = topic("internal.command");
    let ran = std::sync::Arc::new([AtomicU64::new(0), AtomicU64::new(0)]);
    let mut members = Vec::new();
    for group in 0..2u32 {
        let member = bus(&instance).await;
        member.hand_writes_on(&internal, group, {
            let write = write.clone();
            move |topic| topic == write
        });
        // Every write is group 1's, as its request says.
        member.route_writes_by(|_, _| Some(1));
        let counted = std::sync::Arc::clone(&ran);
        member.serve(&write, move |envelope| {
            counted[group as usize].fetch_add(1, Ordering::SeqCst);
            Ok((
                "meridian.test.Answer".to_string(),
                [format!("group {group}: ").into_bytes(), envelope.payload].concat(),
            ))
        });
        members.push(member);
    }
    settle().await;
    let asking = bus("asking").await;
    let (before, _, _) = meridian_bus::owners::handed_on();
    for n in 0..20u32 {
        let (_, payload) = asking
            .call(
                &write,
                "meridian.test.Question",
                n.to_string().into_bytes(),
                None,
                None,
            )
            .await
            .expect("answered");
        assert_eq!(payload, format!("group 1: {n}").into_bytes());
    }
    assert_eq!(
        ran[0].load(Ordering::SeqCst),
        0,
        "group 0's handler never ran"
    );
    assert_eq!(ran[1].load(Ordering::SeqCst), 20);
    let (after, _, _) = meridian_bus::owners::handed_on();
    assert!(after > before, "the writes group 0 received were passed on");
}

/// Step 3d (section 6.7): a passed write keeps its caller's own envelope --
/// the publisher its bus stamped and the person it acts for -- so the owner
/// admits, shares and authorises it as that caller's, never the passing
/// member's; and keeps what is left of its caller's wait, so one whose wait
/// is spent by the time it reaches the owner is dropped there, never run.
#[tokio::test]
async fn a_passed_write_keeps_its_callers_identity_and_wait() {
    let instance = format!("store-{}", mark("keep"));
    let write = topic("command.record");
    let internal = topic("internal.command");
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let passed = std::sync::Arc::new(AtomicU64::new(0));
    let slow = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut members = Vec::new();
    for group in 0..2u32 {
        let member = bus(&instance).await;
        member.hand_writes_on(&internal, group, {
            let write = write.clone();
            move |topic| topic == write
        });
        let (passed, slow) = (std::sync::Arc::clone(&passed), std::sync::Arc::clone(&slow));
        member.route_writes_by(move |_, _| {
            if group == 0 {
                passed.fetch_add(1, Ordering::SeqCst);
                // Slow to decide: what is left of the caller's wait is spent.
                if slow.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(400));
                }
            }
            Some(1)
        });
        let seeing = std::sync::Arc::clone(&seen);
        member.serve(&write, move |envelope| {
            let meta = envelope.meta.clone().unwrap_or_default();
            seeing.lock().unwrap().push((
                group,
                meta.publisher_instance_id,
                meta.acting_for_subject,
            ));
            Ok(("meridian.test.Answer".to_string(), Vec::new()))
        });
        members.push(member);
    }
    settle().await;
    let asking = bus("asking-plugin-1").await;
    for n in 0..20u32 {
        asking
            .call_for(
                &write,
                "meridian.test.Question",
                n.to_string().into_bytes(),
                None,
                None,
                "https://directory.example.org|8812",
            )
            .await
            .expect("answered");
    }
    assert!(
        passed.load(Ordering::SeqCst) > 0,
        "some reached group 0 and were passed"
    );
    for (group, publisher, person) in seen.lock().unwrap().drain(..) {
        assert_eq!(group, 1);
        assert_eq!(publisher, "asking-plugin-1", "the caller's own publisher");
        assert_eq!(person, "https://directory.example.org|8812", "and person");
    }

    // A wait spent on the way: dropped at the owner, never run.
    slow.store(true, Ordering::SeqCst);
    let before = passed.load(Ordering::SeqCst);
    let mut timed_out = 0;
    for n in 0..20u32 {
        if asking
            .call(
                &write,
                "meridian.test.Question",
                n.to_string().into_bytes(),
                None,
                Some(Duration::from_millis(250)),
            )
            .await
            .is_err()
        {
            timed_out += 1;
        }
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
    let reached_group_0 = passed.load(Ordering::SeqCst) - before;
    let ran = seen.lock().unwrap().len() as u64;
    assert!(reached_group_0 > 0);
    assert_eq!(
        timed_out, reached_group_0,
        "each one passed late timed out for its caller"
    );
    assert_eq!(ran, 20 - reached_group_0, "and was never run at the owner");
}

/// Step 3d, stage T: a component says it serves once `subscribed` returns,
/// and from then the broker holds every topic it served. A broker's
/// subscriptions are made on tasks of their own after `serve` returns: a
/// store that said it was ready on `serve` alone was ready before the broker
/// held its topics, its rollout stopped the member that did serve them, and
/// its callers were told nothing serves (PR CI 43636763392). No settling
/// sleep here: the first call after `subscribed` is answered, each round on
/// connections of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_made_once_subscribed_returns_is_never_told_nothing_serves() {
    for round in 0..25 {
        let asking = bus("asking-at-once").await;
        let answering = bus("answering-at-once").await;
        let topics: Vec<String> = (0..40).map(|_| topic("query.at-once")).collect();
        for topic in &topics {
            answering.serve(topic, |_| Ok((String::new(), vec![])));
        }
        answering
            .subscribed()
            .await
            .expect("the broker holds what was served");
        // Every topic, the last served first (the review's finding 12: only
        // the last was asked).
        for topic in topics.iter().rev() {
            let answered = asking
                .call(topic, "meridian.test.Question", vec![], None, None)
                .await;
            assert!(
                answered.is_ok(),
                "round {round}: {topic}, asked once subscribed returned: {answered:?}"
            );
        }
    }
}

/// Stage T's review, finding 5: a subscription the broker refused is never
/// counted as made. Its refusal is said after the fact, and the round trip
/// that follows the subscriptions still comes back; a member said it
/// served a topic the broker did not hold for it. It is never made later,
/// so the member never says it serves.
#[tokio::test]
async fn a_member_whose_subscription_the_broker_refused_never_says_it_is_subscribed() {
    let custody = as_custody().await;
    let granted = "platform.config.event.plugin-configuration-changed";
    let _allowed = custody.subscribe(granted);
    custody
        .subscribed()
        .await
        .expect("what it may subscribe to is held");
    // The contract gives custody no subscription to another role's events.
    let forbidden = "platform.street.event.custodial-position-updated";
    let _refused = custody.subscribe(forbidden);
    let said = custody.subscribed().await;
    let Err(BusError::HandlerFailed { detail, .. }) = &said else {
        panic!("a refused subscription counted as made: {said:?}");
    };
    assert!(detail.contains("could not be made"), "{detail}");
    assert!(
        custody.subscribed().await.is_err(),
        "never made later: asked again, refused again"
    );
}

/// Two servers of the test broker's three-server cluster.
fn cluster() -> (String, String) {
    let urls = std::env::var("MERIDIAN_TEST_BROKER_CLUSTER_URLS").expect(
        "MERIDIAN_TEST_BROKER_CLUSTER_URLS is not set: two servers of a three-server \
         cluster, comma-separated; run these with `make test-broker`.",
    );
    let (a, b) = urls
        .split_once(',')
        .expect("two servers of the cluster, comma-separated");
    (a.trim().to_string(), b.trim().to_string())
}

async fn bus_at(instance: &str, url: &str) -> Bus {
    Bus::single(
        instance,
        Arc::new(
            NatsBackend::connect(url)
                .await
                .expect("could not reach a server of the test cluster"),
        ),
        Arc::new(meridian_clock::SystemClock),
    )
}

/// Stage T's review, finding 5: the broker is three servers in `ha`, and a
/// member's subscriptions reach the servers it is not connected to after
/// the one it is. `subscribed` returns once every server holds them: a
/// call through another server, made at once, is never told nothing
/// serves. The member connects to one server and its caller to another,
/// each round on connections of its own, with no settling sleep.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_through_another_server_once_subscribed_returns_is_never_told_nothing_serves() {
    let (a, b) = cluster();
    for round in 0..40 {
        let asking = bus_at("asking-across", &b).await;
        let answering = bus_at("answering-across", &a).await;
        let topics: Vec<String> = (0..40).map(|_| topic("query.across")).collect();
        for topic in &topics {
            answering.serve(topic, |_| Ok((String::new(), vec![])));
        }
        answering
            .subscribed()
            .await
            .expect("every server holds what was served");
        for topic in topics.iter().rev() {
            let answered = asking
                .call(topic, "meridian.test.Question", vec![], None, None)
                .await;
            assert!(
                answered.is_ok(),
                "round {round}: {topic}, asked through another server: {answered:?}"
            );
        }
    }
}

/// Stage T's review, finding 5: `subscribed` asks through each other
/// server, and is refused while one does not hold the subscriptions. A
/// server whose route to the member's is cut holds none of them: here the
/// member is at a server of its own, outside the cluster, and the probe is
/// shown to go through the cluster's servers by a member inside it being
/// answered and one outside it never claiming the cluster.
#[tokio::test]
async fn subscribed_goes_through_every_server_the_broker_advertises() {
    let (a, _) = cluster();
    let inside = NatsBackend::connect(&a).await.unwrap();
    let before = meridian_bus::servers_asked();
    let _held = inside.subscribe(&topic("event.across"));
    inside.subscribed().await.expect("held by every server");
    let asked = meridian_bus::servers_asked() - before;
    assert!(
        asked >= 2,
        "the two other servers of the three were each asked: {asked}"
    );
}

/// Stage T's review, finding 3 (ruled 2026-11-15): a store's writer told
/// to stop where no lease fences its group writes nothing more and keeps
/// the store's write topics served. Every write it receives is handed to
/// its group, where its replacement -- a member of the same group, started
/// beside it -- answers; it no longer offers its group's internal topic,
/// so a write another group hands to theirs reaches the replacement alone;
/// and the reads it leaves are the replacement's too. Before, both
/// members' handlers ran, each write by whichever member received it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_writer_told_to_stop_hands_every_write_on_and_its_handler_never_runs() {
    let instance = format!("store-{}", mark("stop"));
    let write = topic("command.record");
    let read = topic("query.read");
    let internal = topic("internal.command");
    // The first group's member, the second's old writer, and its replacement.
    let ran = Arc::new([AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)]);
    let read_by = Arc::new([AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)]);
    let mut members = Vec::new();
    for (member, group) in [(0usize, 0u32), (1, 1), (2, 1)] {
        let bus = bus(&instance).await;
        bus.hand_writes_on(&internal, group, {
            let write = write.clone();
            move |topic| topic == write
        });
        // Every write is the second group's, as its request says.
        bus.route_writes_by(|_, _| Some(1));
        let counted = Arc::clone(&ran);
        bus.serve(&write, move |envelope| {
            counted[member].fetch_add(1, Ordering::SeqCst);
            Ok((
                "meridian.test.Answer".to_string(),
                [format!("member {member}: ").into_bytes(), envelope.payload].concat(),
            ))
        });
        let counted = Arc::clone(&read_by);
        bus.serve(&read, move |_| {
            counted[member].fetch_add(1, Ordering::SeqCst);
            Ok(("meridian.test.Answer".to_string(), vec![]))
        });
        bus.subscribed().await.expect("subscribed");
        members.push(bus);
    }
    let asking = bus("asking").await;
    let ask = |topic: String, n: u32| {
        let asking = &asking;
        async move {
            asking
                .call(
                    &topic,
                    "meridian.test.Question",
                    n.to_string().into_bytes(),
                    None,
                    None,
                )
                .await
        }
    };
    for n in 0..30u32 {
        ask(write.clone(), n).await.expect("answered");
    }
    assert_eq!(ran[0].load(Ordering::SeqCst), 0, "never the first group's");
    assert!(
        ran[1].load(Ordering::SeqCst) > 0 && ran[2].load(Ordering::SeqCst) > 0,
        "both members of the second group wrote, side by side"
    );
    // The old writer is told to stop.
    let was = ran[1].load(Ordering::SeqCst);
    assert!(members[1].stop_writing_here({
        let read = read.clone();
        move |topic| topic == read
    }));
    let reads_before = read_by[1].load(Ordering::SeqCst);
    for n in 0..60u32 {
        let (_, payload) = ask(write.clone(), n)
            .await
            .expect("answered, by its replacement");
        assert_eq!(payload, format!("member 2: {n}").into_bytes());
    }
    assert_eq!(
        ran[1].load(Ordering::SeqCst),
        was,
        "told to stop, its handler never ran again"
    );
    // Its reads are left to the members that serve them, once the broker
    // has taken its leave of them.
    settle().await;
    let left = read_by[1].load(Ordering::SeqCst);
    for n in 0..30u32 {
        ask(read.clone(), n).await.expect("read");
    }
    assert_eq!(
        read_by[1].load(Ordering::SeqCst),
        left,
        "no read answered by it"
    );
    assert!(left - reads_before <= 1, "at most what had already arrived");
    assert!(members[1].writes_nothing_here());
    // A member with no hand-off configured has nothing to stop.
    assert!(!asking.stop_writing_here(|_| true));
}

/// Item 110 (ruled 2026-11-18): a later group's writer fenced while it
/// serves on, told to stop -- the count lowered to one group, whose writer
/// took every partition over and does not serve yet -- answers each
/// request "ask again", UNAVAILABLE with a retry-after: no handler run, and
/// nothing passed on by the owners it last read, to a group whose members
/// are gone. Its caller asks again, and once the one group's writer serves
/// it is answered there. Before, the fenced writer ended at its fence and
/// nothing served the topic; and a write it passed on was never answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_writer_fenced_while_it_serves_on_refuses_every_request_to_be_asked_again() {
    let instance = format!("store-{}", mark("fenced"));
    let write = topic("command.record");
    let read = topic("query.read");
    let internal = topic("internal.command");
    // The second group's old writer: every write is the first group's, by
    // the owners it last read.
    let ran = Arc::new(AtomicU64::new(0));
    let old = bus(&instance).await;
    old.hand_writes_on(&internal, 1, {
        let write = write.clone();
        move |topic| topic == write
    });
    old.route_writes_by(|_, _| Some(0));
    for served in [&write, &read] {
        let counted = Arc::clone(&ran);
        old.serve(served, move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(("meridian.test.Answer".to_string(), b"old".to_vec()))
        });
    }
    old.subscribed().await.expect("subscribed");
    let asking = bus("asking").await;
    let ask = |topic: String| {
        let asking = &asking;
        async move {
            asking
                .call(
                    &topic,
                    "meridian.test.Question",
                    Vec::new(),
                    None,
                    Some(Duration::from_millis(500)),
                )
                .await
        }
    };
    // Fenced while it serves on: it refuses from here.
    old.refuse_from_here();
    for served in [&write, &read] {
        for _ in 0..10 {
            let detail = match ask(served.clone()).await {
                Err(BusError::HandlerFailed { detail, .. }) => detail,
                other => panic!("the fenced writer serving on, {served} was answered {other:?}"),
            };
            assert_eq!(
                meridian_bus::read_refusal_status(&detail).map(|(status, _, _)| status),
                Some(meridian_bus::RefusalStatus::Unavailable),
                "{detail}"
            );
            assert_eq!(meridian_bus::refusal_retry_after(&detail), Some(1));
        }
    }
    assert_eq!(ran.load(Ordering::SeqCst), 0, "a handler of it ran");
    // The one group's writer serves: every request is answered there, or
    // refused to be asked again by the old one, and never by nobody.
    let new = bus(&instance).await;
    new.serve(&write, |_| {
        Ok(("meridian.test.Answer".to_string(), b"new".to_vec()))
    });
    new.subscribed().await.expect("subscribed");
    settle().await;
    let mut answered = 0;
    for _ in 0..40 {
        match ask(write.clone()).await {
            Ok((_, payload)) => {
                assert_eq!(payload, b"new".to_vec());
                answered += 1;
            }
            Err(BusError::HandlerFailed { detail, .. }) => {
                assert_eq!(
                    meridian_bus::refusal_retry_after(&detail),
                    Some(1),
                    "{detail}"
                );
            }
            Err(other) => panic!("neither answered nor to be asked again: {other:?}"),
        }
    }
    assert!(answered > 0, "the writer that serves answered none");
    assert_eq!(ran.load(Ordering::SeqCst), 0, "a handler of it ran");
}

/// Item 120 (ruled 2026-11-18): a later group's writer with no lease, told
/// to stop -- every write handed on, the internal topic withdrawn -- and
/// fenced while it serves on refuses to be asked again, UNAVAILABLE with
/// retry-after 1, at once, as any fenced writer does. Before, it went on
/// handing each write on by the owners it last read: one handed to a group
/// with no members (the count lowered) was refused only at its caller's
/// deadline, with no retry-after (review-c5n2 L3).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_writer_handing_every_write_on_fenced_while_it_serves_on_refuses_at_once() {
    let instance = format!("store-{}", mark("handing-fenced"));
    let write = topic("command.record");
    let internal = topic("internal.command");
    // The second group's old writer: the write is the second group's, by
    // the owners it last read (stale once the count is lowered).
    let old = bus(&instance).await;
    old.hand_writes_on(&internal, 1, {
        let write = write.clone();
        move |topic| topic == write
    });
    old.route_writes_by(|_, _| Some(1));
    old.serve(&write, |_| {
        Ok(("meridian.test.Answer".to_string(), b"old".to_vec()))
    });
    // The one group's writer: offers the internal topic to group 0, and does
    // not serve the public topic yet.
    let new = bus(&instance).await;
    new.hand_writes_on(&internal, 0, {
        let write = write.clone();
        move |topic| topic == write
    });
    old.subscribed().await.expect("subscribed");
    new.subscribed().await.expect("subscribed");
    // Told to stop with no lease, then fenced while it serves on.
    assert!(old.stop_writing_here(|_| false));
    old.refuse_from_here();
    settle().await;
    let asking = bus("asking").await;
    for _ in 0..5 {
        let began = std::time::Instant::now();
        let answered = asking
            .call(
                &write,
                "meridian.test.Question",
                Vec::new(),
                None,
                Some(Duration::from_millis(2_000)),
            )
            .await;
        let took = began.elapsed();
        let detail = match answered {
            Err(BusError::HandlerFailed { detail, .. }) => detail,
            other => panic!("the fenced writer handing on answered {other:?}"),
        };
        assert_eq!(
            meridian_bus::read_refusal_status(&detail).map(|(status, _, _)| status),
            Some(meridian_bus::RefusalStatus::Unavailable),
            "{detail}"
        );
        assert_eq!(
            meridian_bus::refusal_retry_after(&detail),
            Some(1),
            "{detail}"
        );
        assert!(
            took < Duration::from_millis(1_000),
            "refused after {took:?}, at the caller's deadline: {detail}"
        );
    }
}

/// A write whose owner was being decided when its writer was fenced while
/// it served on is refused to be asked again -- UNAVAILABLE, retry-after 1
/// -- and never passed on by the owners it last read (ruled 2026-11-18,
/// item 120; review-c5n5 F2): the fence lands between the owner's decision
/// and the publish, which looks again. Before, a decision begun before the
/// fence was passed on all the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_whose_owner_was_being_decided_at_the_fence_is_refused_not_passed_on() {
    let instance = format!("store-{}", mark("deciding-fenced"));
    let write = topic("command.record");
    let internal = topic("internal.command");
    // The second group's old writer, whose owners say the write is the
    // first group's: its decision held until the test lets it go.
    let (entered, mut deciding) = tokio::sync::mpsc::unbounded_channel::<()>();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let released = std::sync::Mutex::new(released);
    let old = bus(&instance).await;
    old.hand_writes_on(&internal, 1, {
        let write = write.clone();
        move |topic| topic == write
    });
    old.route_writes_by(move |_, _| {
        let _ = entered.send(());
        let _ = released
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10));
        Some(0)
    });
    old.serve(&write, |_| {
        Ok(("meridian.test.Answer".to_string(), b"old".to_vec()))
    });
    // The first group's writer: offers the internal topic to group 0.
    let new = bus(&instance).await;
    new.hand_writes_on(&internal, 0, {
        let write = write.clone();
        move |topic| topic == write
    });
    old.subscribed().await.expect("subscribed");
    new.subscribed().await.expect("subscribed");
    settle().await;
    let asking = bus("asking").await;
    let asked = {
        let write = write.clone();
        tokio::spawn(async move {
            asking
                .call(
                    &write,
                    "meridian.test.Question",
                    Vec::new(),
                    None,
                    Some(Duration::from_millis(3_000)),
                )
                .await
        })
    };
    // Its owner being decided, the writer is fenced while it serves on.
    tokio::time::timeout(Duration::from_secs(5), deciding.recv())
        .await
        .expect("the write's owner was never decided")
        .unwrap();
    old.refuse_from_here();
    release.send(()).unwrap();
    let detail = match asked.await.unwrap() {
        Err(BusError::HandlerFailed { detail, .. }) => detail,
        other => panic!("fenced while its owner was decided, the write was answered {other:?}"),
    };
    assert_eq!(
        meridian_bus::read_refusal_status(&detail).map(|(status, _, _)| status),
        Some(meridian_bus::RefusalStatus::Unavailable),
        "{detail}"
    );
    assert_eq!(
        meridian_bus::refusal_retry_after(&detail),
        Some(1),
        "{detail}"
    );
}
