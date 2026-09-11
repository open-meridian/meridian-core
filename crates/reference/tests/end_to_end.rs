//! The replica against a running platform and a real database.
//!
//! Everything the crate claims about pulling, minting and applying is a claim
//! until it has been made against the platform rather than against a scripted
//! transport. This is that run.
//!
//! It signs with the key the replica generated and an operator registered, so a
//! failure to authenticate fails here rather than being mocked away. Run by
//! `make demo`, which brings up the database, registers the deployment and sets
//! the three variables below.

use std::sync::Arc;

use tokio::runtime::Runtime;

use meridian_pb::v1::{
    Identifier as PbIdentifier, MissReason, MissingInstrumentDetectedEvent,
    ResolveIdentifierRequest,
};
use meridian_reference::store::Store;
use meridian_reference::{
    resolve_identifier, Config, DeploymentKey, HttpTransport, Platform, PostgresStore, Reaction,
};

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
        .expect("could not read the deployment key the replica generated");

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
        .expect("could not reach the replica's database");
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

#[test]
fn a_miss_the_platform_does_not_know_is_minted_applied_and_resolvable() {
    let store = store();
    let platform = platform();

    let identifiers = unknown_identifiers();
    let as_of = now_ns();
    let event = MissingInstrumentDetectedEvent {
        source: "snaptrade".into(),
        asset_class: "EQUITY".into(),
        identifiers: identifiers.clone(),
        as_of_ns: as_of,
        publisher_instance_id: "end-to-end".into(),
        reason: MissReason::NotFound as i32,
        observed_at_ns: as_of,
    };

    let reaction = runtime()
        .block_on(platform.react_to_miss(&event, now_ns()))
        .expect("the platform did not answer");

    let record = match reaction {
        Reaction::Minted(record) => *record,
        other => panic!("expected a mint from a platform that has never seen these, got {other:?}"),
    };

    // Locally minted identity is distinguishable from central identity for the
    // rest of its life, which is what the prefix is for.
    assert!(
        record.instrument_id.starts_with("LCL-"),
        "{}",
        record.instrument_id
    );
    assert_eq!(
        record.lifecycle_state,
        meridian_pb::v1::InstrumentLifecycleState::Define as i32,
        "a stub arrives in DEFINE, awaiting an administrator"
    );

    // W3.5, through the same path a locally defined instrument takes.
    let applied = meridian_reference::apply(&store, record.clone(), now_ns()).unwrap();
    assert!(applied.changed());

    // And now the resolution that missed would not.
    let reply = resolve_identifier(
        &store,
        &ResolveIdentifierRequest {
            identifiers: identifiers.clone(),
            as_of_ns: now_ns(),
            exchange_mic: String::new(),
            currency: String::new(),
        },
    )
    .unwrap();

    assert!(
        reply.found,
        "the replica still cannot resolve what it applied"
    );
    assert_eq!(reply.instrument_id, record.instrument_id);
    assert_eq!(
        store.version_of(&record.instrument_id).unwrap(),
        Some(record.version)
    );
}

#[test]
fn applying_the_same_record_twice_changes_nothing() {
    // A retry after an ambiguous failure is the expected case, not the
    // exceptional one, and it has to be harmless against a real database and
    // not only against a HashMap.
    let store = store();
    let platform = platform();

    let event = MissingInstrumentDetectedEvent {
        source: "snaptrade".into(),
        asset_class: "EQUITY".into(),
        identifiers: unknown_identifiers(),
        as_of_ns: now_ns(),
        publisher_instance_id: "end-to-end".into(),
        reason: MissReason::NotFound as i32,
        observed_at_ns: now_ns(),
    };

    let record = match runtime()
        .block_on(platform.react_to_miss(&event, now_ns()))
        .unwrap()
    {
        Reaction::Minted(record) => *record,
        other => panic!("expected a mint, got {other:?}"),
    };

    assert!(meridian_reference::apply(&store, record.clone(), now_ns())
        .unwrap()
        .changed());
    assert!(!meridian_reference::apply(&store, record, now_ns())
        .unwrap()
        .changed());
}
