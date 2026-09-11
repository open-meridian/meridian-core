//! What a deployment does while the platform is not there.
//!
//! Run by `make demo` with the platform's container stopped, which is the only
//! way to find out. Everything the crate says about outages is a claim until
//! something is actually switched off, and a mocked outage tests the mock.

use std::sync::Arc;
use std::time::Duration;

use meridian_pb::v1::{
    Identifier as PbIdentifier, InstrumentRecord as PbInstrument, ResolveIdentifierRequest,
};
use meridian_reference::{
    apply, resolve_identifier, Config, DeploymentKey, HttpTransport, Platform, PostgresStore,
};
use tokio::runtime::Runtime;

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

#[test]
fn the_replica_keeps_answering_while_the_platform_is_away() {
    let store = PostgresStore::connect(&required("MERIDIAN_TEST_DATABASE_URL"), 4).unwrap();
    store.migrate().unwrap();

    // Something it was told before the platform went away.
    let stamp = now_ns();
    let instrument_id = format!("INS-outage-{stamp}");
    let figi = format!("BBG{stamp}");
    apply(
        &store,
        PbInstrument {
            instrument_id: instrument_id.clone(),
            identifiers: vec![PbIdentifier {
                scheme: "figi".into(),
                value: figi.clone(),
                source: String::new(),
            }],
            asset_class: "EQUITY".into(),
            lifecycle_state: meridian_pb::v1::InstrumentLifecycleState::Active as i32,
            version: 1,
            valid_from_ns: stamp,
            record_time_ns: stamp,
            ..Default::default()
        },
        now_ns(),
    )
    .unwrap();

    // The platform is not there.
    let pem = std::fs::read_to_string(required("MERIDIAN_TEST_KEY_PATH")).unwrap();
    let platform = Platform::new(
        Config::new(
            required("MERIDIAN_TEST_PLATFORM_ADDRESS"),
            required("MERIDIAN_TEST_DEPLOYMENT_ID"),
        ),
        DeploymentKey::from_pkcs8_pem(&pem).unwrap(),
        Arc::new(HttpTransport::new(Duration::from_secs(2)).unwrap()),
    );

    let failed = Runtime::new()
        .unwrap()
        .block_on(platform.pull_instrument(&instrument_id, now_ns(), now_ns()))
        .expect_err("the platform answered; stop it before running this");

    assert!(
        failed.is_outage(),
        "an unreachable platform should read as an outage, not as {failed}"
    );

    // And none of that reached the question a connector actually asks.
    let reply = resolve_identifier(
        &store,
        &ResolveIdentifierRequest {
            identifiers: vec![PbIdentifier {
                scheme: "figi".into(),
                value: figi,
                source: String::new(),
            }],
            as_of_ns: now_ns(),
            exchange_mic: String::new(),
            currency: String::new(),
        },
    )
    .unwrap();

    assert!(
        reply.found,
        "the replica stopped answering for what it holds"
    );
    assert_eq!(reply.instrument_id, instrument_id);
}
