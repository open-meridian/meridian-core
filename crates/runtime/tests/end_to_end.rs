//! The instrument store and the conductor against a running platform and a
//! real database.
//!
//! Everything the crates claim about asking the platform and keeping its
//! answer is a claim until it has been made against the platform rather than
//! against a scripted transport. This is that run.
//!
//! It signs with the deployment's key, which an operator registered, so a
//! failure to authenticate fails here rather than being mocked away. Run by
//! `make demo`, which brings up the database, registers the deployment and sets
//! the variables below.
//!
//! A record's whole arc under decisions/030 runs through the real pieces on one
//! in-process bus: the instrument store's handlers and reactor over Postgres,
//! and the conductor over the platform. Only the connector, the dashboard and
//! the broker are absent: a resolve and an ask over the bus are what their
//! sidecar and the dashboard make.

use std::sync::Arc;
use std::time::Duration;

use prost::Message;
use tokio::runtime::Runtime;

use meridian_bus::{Bus, MemoryBackend, Subscription};
use meridian_conductor::{
    Conductor, Config, DeploymentKey, HttpTransport, Platform, ASK_PLATFORM_FOR_INSTRUMENT,
};
use meridian_domain::v1::{
    AskPlatformForInstrumentReply, AskPlatformForInstrumentRequest, EscalateInstrumentRequest,
    Identifier as PbIdentifier, InstrumentAppliedEvent, ResolveIdentifierReply,
    ResolveIdentifierRequest, ResolveInstrumentReply, ResolveInstrumentRequest,
};
use meridian_instrument::complete::keep_platform_answer;
use meridian_instrument::service::{
    serve_all, INSTRUMENT_APPLIED, INSTRUMENT_MISSING, INSTRUMENT_PULLED, RESOLVE_IDENTIFIER,
    RESOLVE_INSTRUMENT,
};
use meridian_instrument::store::Store;
use meridian_instrument::{resolve_identifier, PostgresStore, Reactor, GLOBAL_ID};

fn required(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| panic!("{name} is not set. These tests are run by `make demo`."))
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64
}

/// A runtime to make the platform calls on.
///
/// The store is deliberately used outside it. `PostgresStore` is synchronous
/// and drives its own runtime, and a runtime cannot be started from inside
/// another one: doing it aborts the process rather than returning an error.
fn runtime() -> Runtime {
    Runtime::new().expect("could not build a runtime")
}

fn platform() -> Platform {
    let pem = std::fs::read_to_string(required("MERIDIAN_TEST_KEY_PATH"))
        .expect("could not read the deployment key");

    Platform::new(
        Config::new(
            required("MERIDIAN_TEST_PLATFORM_ADDRESS"),
            required("MERIDIAN_TEST_DEPLOYMENT_ID"),
        ),
        DeploymentKey::from_pkcs8_pem(&pem).expect("the deployment key could not be read"),
        Arc::new(
            HttpTransport::new(std::time::Duration::from_secs(10))
                .expect("could not build the client"),
        ),
    )
}

fn store() -> PostgresStore {
    let store = PostgresStore::connect(&required("MERIDIAN_TEST_DATABASE_URL"), 4)
        .expect("could not reach the instrument store's database");
    store.migrate().expect("could not create the schema");
    store
}

/// One identifier set no previous run has used, so a mint is a mint.
fn unknown_identifiers() -> Vec<PbIdentifier> {
    let stamp = now_ns();
    vec![
        PbIdentifier {
            scheme: "symbol".into(),
            value: format!("ZZ{stamp}"),
            source: "snaptrade".into(),
        },
        PbIdentifier {
            scheme: "figi".into(),
            value: format!("BBG{stamp}"),
            source: String::new(),
        },
    ]
}

#[test]
fn the_platform_accepts_this_deployments_signature() {
    // The first thing worth knowing. An unregistered or badly signed assertion
    // comes back as a refusal, not as "no such instrument", so a not-found here
    // is proof the edge read the signature and believed it.
    let found = runtime()
        .block_on(platform().pull_instrument("INS-nobody-has-this", now_ns(), now_ns()))
        .expect("the platform refused the request");

    assert!(found.is_none());
}

/// The next delivery on `subscription` that `wanted` accepts, or a failure
/// rather than a hang. Anything else on the topic is somebody else's.
async fn awaited<T: Message + Default>(
    subscription: &mut Subscription,
    wanted: impl Fn(&T) -> bool,
) -> T {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let delivery = subscription.recv().await.expect("the bus shut down");
            let Ok(message) = T::decode(&delivery.envelope.payload[..]) else {
                continue;
            };
            if wanted(&message) {
                return message;
            }
        }
    })
    .await
    .expect("nothing arrived within a minute")
}

#[test]
fn a_record_minted_here_is_backfilled_with_the_global_id_and_no_key_changes() {
    // decisions/030 end to end (W3.1, W3.7, W3.3, W3.5): nothing matched, so
    // the store mints the deployment's own record; the platform comes to hold
    // the security (here by its own escalation route, the staff's side); a
    // person asks; the conductor pulls by the FIGI alone; the store adds the
    // INS- ID as an identifier, and every key stays the deployment's.
    let store = Arc::new(store());
    let platform = Arc::new(platform());
    let identifiers = unknown_identifiers();
    let asking = ResolveIdentifierRequest {
        identifiers: identifiers.clone(),
        as_of_ns: now_ns(),
        ..Default::default()
    };

    let (local, global) = runtime().block_on(async {
        let bus = Arc::new(Bus::single(
            "instrument-1",
            Arc::new(MemoryBackend::new()),
            Arc::new(meridian_clock::SystemClock),
        ));
        let mut applied = bus.subscribe(INSTRUMENT_APPLIED);

        // Wired as the two processes wire themselves.
        let writing = Arc::new(std::sync::Mutex::new(()));
        serve_all(&bus, store.clone(), bus.clock(), writing.clone());
        let pulled = bus.subscribe(INSTRUMENT_PULLED);
        let missing = bus.subscribe(INSTRUMENT_MISSING);
        tokio::spawn(
            Reactor::sharing(bus.clone(), store.clone(), bus.clock(), writing)
                .consume(pulled, missing),
        );
        Conductor::new(bus.clone(), platform.clone(), bus.clock()).serve();

        let first: ResolveIdentifierReply = ask(
            &bus,
            RESOLVE_IDENTIFIER,
            "meridian.v1.ResolveIdentifierRequest",
            &asking,
        )
        .await;
        assert!(
            first.found && first.minted,
            "nothing matched, and a record is minted"
        );
        assert!(
            first.instrument_id.starts_with("LCL-"),
            "{}",
            first.instrument_id
        );
        let local = first.instrument_id;

        // The platform comes to hold it.
        let held = platform
            .escalate(
                &EscalateInstrumentRequest {
                    source: "snaptrade".into(),
                    identifiers: identifiers.clone(),
                    as_of_ns: now_ns(),
                    placeholder_instrument_id: local.clone(),
                    ..Default::default()
                },
                now_ns(),
            )
            .await
            .expect("the platform did not answer")
            .expect("the platform held nothing and minted nothing");
        assert!(
            held.instrument_id.starts_with("INS-"),
            "{}",
            held.instrument_id
        );

        // A person asks.
        let answer: AskPlatformForInstrumentReply = ask(
            &bus,
            ASK_PLATFORM_FOR_INSTRUMENT,
            "meridian.v1.AskPlatformForInstrumentRequest",
            &AskPlatformForInstrumentRequest {
                instrument_id: local.clone(),
                identifiers: identifiers.clone(),
                as_of_ns: now_ns(),
            },
        )
        .await;
        assert!(answer.reachable && answer.found, "{}", answer.detail);
        let global = answer
            .instrument
            .expect("an answer carries its record")
            .instrument_id;
        assert_eq!(global, held.instrument_id);

        // Kept: the INS- ID joins the deployment's record as an identifier.
        let event: InstrumentAppliedEvent =
            awaited(&mut applied, |event: &InstrumentAppliedEvent| {
                event.instrument.as_ref().is_some_and(|record| {
                    record.instrument_id == local
                        && record.identifiers.iter().any(|identifier| {
                            identifier.scheme == GLOBAL_ID && identifier.value == global
                        })
                })
            })
            .await;
        assert!(event.applied);

        // Every key stays the deployment's: the set resolves to the same
        // record, which answers itself.
        let again: ResolveIdentifierReply = ask(
            &bus,
            RESOLVE_IDENTIFIER,
            "meridian.v1.ResolveIdentifierRequest",
            &asking,
        )
        .await;
        assert_eq!(again.instrument_id, local);
        assert!(!again.minted);
        let read: ResolveInstrumentReply = ask(
            &bus,
            RESOLVE_INSTRUMENT,
            "meridian.v1.ResolveInstrumentRequest",
            &ResolveInstrumentRequest {
                instrument_id: local.clone(),
                as_of_ns: now_ns(),
            },
        )
        .await;
        assert_eq!(
            read.instrument.map(|record| record.instrument_id),
            Some(local.clone())
        );

        (local, global)
    });

    // Outside the runtime: the store's driver starts one of its own.
    assert_eq!(
        store.replacement_of(&local).unwrap(),
        None,
        "nothing was replaced"
    );
    assert!(
        store.by_id(&global).unwrap().is_none(),
        "no record of the platform's own"
    );
}

/// Ask on the bus and read the answer.
async fn ask<Q: Message, R: Message + Default>(
    bus: &Bus,
    topic: &str,
    payload_type: &str,
    question: &Q,
) -> R {
    let (_, payload) = bus
        .call(
            topic,
            payload_type,
            question.encode_to_vec(),
            None,
            Some(Duration::from_secs(30)),
        )
        .await
        .expect("answered");
    R::decode(&payload[..]).expect("the answer decodes")
}

#[test]
fn keeping_the_same_answer_twice_changes_nothing() {
    // A redelivery is the expected case, not the exceptional one, and it has
    // to be harmless against a real database and not only against a HashMap.
    let store = store();
    let platform = platform();
    let identifiers = unknown_identifiers();
    let minted = resolve_identifier(
        &store,
        &ResolveIdentifierRequest {
            identifiers: identifiers.clone(),
            as_of_ns: now_ns(),
            ..Default::default()
        },
        "end-to-end",
        now_ns(),
    )
    .unwrap()
    .reply
    .instrument_id;

    let record = runtime()
        .block_on(platform.escalate(
            &EscalateInstrumentRequest {
                source: "snaptrade".into(),
                identifiers,
                as_of_ns: now_ns(),
                placeholder_instrument_id: minted.clone(),
                ..Default::default()
            },
            now_ns(),
        ))
        .unwrap()
        .expect("the platform held nothing and minted nothing");

    assert!(keep_platform_answer(&store, &minted, &record, now_ns())
        .unwrap()
        .is_some());
    assert!(keep_platform_answer(&store, &minted, &record, now_ns())
        .unwrap()
        .is_none());
}
