//! The instrument store against a running platform and a real database.
//!
//! Everything the crate claims about pulling, minting and applying is a claim
//! until it has been made against the platform rather than against a scripted
//! transport. This is that run.
//!
//! It signs with the key the instrument store generated and an operator registered, so a
//! failure to authenticate fails here rather than being mocked away. Run by
//! `make demo`, which brings up the database, registers the deployment and sets
//! the three variables below.
//!
//! The placeholder's whole arc runs through the real pieces on one in-process
//! bus: the instrument store's query and reactor over Postgres, and the
//! conductor over the platform. Only the connector and the broker are absent,
//! and a resolve over the bus is what the connector's sidecar makes.

use std::sync::Arc;
use std::time::Duration;

use prost::Message;
use tokio::runtime::Runtime;

use meridian_bus::{Bus, MemoryBackend, Subscription};
use meridian_conductor::{
    Conductor, Config, DeploymentKey, HttpTransport, Platform, Reaction,
    SystemClock as ConductorClock,
};
use meridian_domain::v1::{
    Identifier as PbIdentifier, InstrumentLifecycleState, InstrumentReplacedEvent,
    PullInstrumentReply, ResolveIdentifierReply, ResolveIdentifierRequest, ResolveInstrumentReply,
    ResolveInstrumentRequest,
};
use meridian_instrument::placeholder::announcement;
use meridian_instrument::service::{
    serve_queries, INSTRUMENT_MISSING, INSTRUMENT_PULLED, INSTRUMENT_REPLACED, RESOLVE_IDENTIFIER,
    RESOLVE_INSTRUMENT,
};
use meridian_instrument::store::Store;
use meridian_instrument::{resolve_identifier, PostgresStore, Reactor, SystemClock};

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
/// Production has the same constraint and answers it the same way, by keeping
/// store work off the async worker.
fn runtime() -> Runtime {
    Runtime::new().expect("could not build a runtime")
}

fn platform() -> Platform {
    let pem = std::fs::read_to_string(required("MERIDIAN_TEST_KEY_PATH"))
        .expect("could not read the deployment key the instrument store generated");

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
fn a_placeholder_is_paired_with_an_ins_id_that_replaces_it_everywhere() {
    // W3.1, W3.7, W3.3, W3.4, W3.5 and W3.8, in one run. Nothing matched, so
    // the store answers a placeholder and announces it; the conductor pulls,
    // misses, escalates it; the platform mints an INS- ID paired with it; the
    // store applies the record and replaces the placeholder.
    let store = Arc::new(store());
    let platform = Arc::new(platform());
    let identifiers = unknown_identifiers();
    let asking = ResolveIdentifierRequest {
        identifiers: identifiers.clone(),
        as_of_ns: now_ns(),
        exchange_mic: String::new(),
        currency: String::new(),
    };

    let (placeholder, record) = runtime().block_on(async {
        let bus = Arc::new(Bus::single("instrument-1", Arc::new(MemoryBackend::new())));
        let mut pulled = bus.subscribe(INSTRUMENT_PULLED);
        let mut replaced = bus.subscribe(INSTRUMENT_REPLACED);

        // Wired as the two processes wire themselves, less the announcing
        // loop, which would carry every placeholder earlier runs left in this
        // database to the platform before this one.
        serve_queries(&bus, store.clone(), Arc::new(SystemClock));
        let applying = bus.subscribe(INSTRUMENT_PULLED);
        tokio::spawn(
            Reactor::new(bus.clone(), store.clone(), Arc::new(SystemClock)).consume(applying),
        );
        let misses = bus.subscribe(INSTRUMENT_MISSING);
        tokio::spawn(
            Conductor::new(bus.clone(), platform.clone(), Arc::new(ConductorClock)).consume(misses),
        );

        let first: ResolveIdentifierReply = ask(&bus, RESOLVE_IDENTIFIER, &asking).await;
        assert!(
            first.found,
            "nothing matched, and a placeholder answers that"
        );
        assert!(first.placeholder);
        assert!(
            first.instrument_id.starts_with("LCL-"),
            "{}",
            first.instrument_id
        );
        let placeholder = first.instrument_id;

        // The escalation's answer, as the conductor published it: the
        // platform's INS- ID, naming the placeholder it replaces.
        let answered: PullInstrumentReply = awaited(&mut pulled, |reply: &PullInstrumentReply| {
            reply.replaces_instrument_id == placeholder
        })
        .await;
        let record = answered.instrument.expect("a pairing carries its record");
        assert!(
            record.instrument_id.starts_with("INS-"),
            "only the platform mints identity, and it mints INS-: {}",
            record.instrument_id
        );
        assert_eq!(
            record.lifecycle_state,
            InstrumentLifecycleState::Define as i32,
            "a stub arrives in DEFINE, awaiting an administrator"
        );

        let event: InstrumentReplacedEvent =
            awaited(&mut replaced, |event: &InstrumentReplacedEvent| {
                event.replaced_instrument_id == placeholder
            })
            .await;
        assert_eq!(
            event.instrument.map(|record| record.instrument_id),
            Some(record.instrument_id.clone())
        );

        // The set answers the INS- ID now, and says it is not a placeholder.
        let again: ResolveIdentifierReply = ask(&bus, RESOLVE_IDENTIFIER, &asking).await;
        assert!(again.found);
        assert!(!again.placeholder);
        assert_eq!(again.instrument_id, record.instrument_id);

        // And the placeholder itself answers the INS- record, so nothing a
        // reader can hold still resolves to an LCL- ID.
        let by_placeholder: ResolveInstrumentReply = ask(
            &bus,
            RESOLVE_INSTRUMENT,
            &ResolveInstrumentRequest {
                instrument_id: placeholder.clone(),
                as_of_ns: now_ns(),
            },
        )
        .await;
        assert!(by_placeholder.found);
        assert_eq!(
            by_placeholder.instrument.map(|record| record.instrument_id),
            Some(record.instrument_id.clone())
        );

        // The platform keeps the pairing for this deployment, and answers it
        // when asked by the placeholder.
        let paired = platform
            .pull_instrument(&placeholder, now_ns(), now_ns())
            .await
            .expect("the platform did not answer")
            .expect("the platform does not know the pairing it made");
        assert_eq!(paired.instrument_id, record.instrument_id);

        (placeholder, record)
    });

    // Outside the runtime: the store's driver starts one of its own.
    assert_eq!(
        store.version_of(&record.instrument_id).unwrap(),
        Some(record.version)
    );
    assert_eq!(
        store.replacement_of(&placeholder).unwrap(),
        Some(record.instrument_id)
    );
}

/// Ask on the bus and read the answer.
async fn ask<Q: Message, R: Message + Default>(bus: &Bus, topic: &str, question: &Q) -> R {
    let payload_type = match topic {
        RESOLVE_IDENTIFIER => "meridian.v1.ResolveIdentifierRequest",
        _ => "meridian.v1.ResolveInstrumentRequest",
    };
    let (_, payload) = bus
        .call(topic, payload_type, question.encode_to_vec(), None, None)
        .await
        .expect("the instrument store answers");
    R::decode(&payload[..]).expect("the answer decodes")
}

#[test]
fn applying_the_same_record_twice_changes_nothing() {
    // A retry after an ambiguous failure is the expected case, not the
    // exceptional one, and it has to be harmless against a real database and
    // not only against a HashMap.
    let store = store();
    let platform = platform();

    let minted = resolve_identifier(
        &store,
        &ResolveIdentifierRequest {
            identifiers: unknown_identifiers(),
            as_of_ns: now_ns(),
            exchange_mic: String::new(),
            currency: String::new(),
        },
        now_ns(),
    )
    .unwrap()
    .minted
    .expect("a set nobody has seen mints a placeholder");
    let event = announcement(&minted, "end-to-end", now_ns());

    let record = match runtime()
        .block_on(platform.react_to_miss(&event, now_ns()))
        .unwrap()
    {
        Reaction::Minted(record) => *record,
        other => panic!("expected a mint, got {other:?}"),
    };
    assert!(
        record.instrument_id.starts_with("INS-"),
        "{}",
        record.instrument_id
    );

    assert!(meridian_instrument::apply(&store, record.clone(), now_ns())
        .unwrap()
        .changed());
    assert!(!meridian_instrument::apply(&store, record, now_ns())
        .unwrap()
        .changed());
}
