//! What a deployment does while the platform is not there.
//!
//! Run by `make demo` with the platform's container stopped, which is the only
//! way to find out. Everything the crate says about outages is a claim until
//! something is actually switched off, and a mocked outage tests the mock.

use std::sync::Arc;
use std::time::Duration;

use meridian_conductor::{Config, DeploymentKey, HttpTransport, Platform};
use meridian_domain::v1::{
    Identifier as PbIdentifier, InstrumentRecord as PbInstrument, ResolveIdentifierRequest,
};
use meridian_instrument::{apply, resolve_identifier, PostgresStore};
use meridian_street::amounts::{Money, Quantity};
use meridian_street::store::{Holding, Settled, Side, Statement, Store as _};
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
fn the_store_keeps_answering_while_the_platform_is_away() {
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
            asset_class: meridian_domain::v1::AssetClass::Equity as i32,
            lifecycle_state: meridian_domain::v1::InstrumentLifecycleState::Active as i32,
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
        now_ns(),
    )
    .unwrap()
    .reply;

    assert!(
        reply.found,
        "the instrument store stopped answering for what it holds"
    );
    assert_eq!(reply.instrument_id, instrument_id);
}

#[test]
fn a_holding_nobody_has_seen_is_recorded_against_a_placeholder_while_the_platform_is_away() {
    // W3.7's reason for being: the holding has a name at once, platform or no
    // platform, and a position under it that every book can use until the
    // INS- ID arrives.
    let url = required("MERIDIAN_TEST_DATABASE_URL");
    let instruments = PostgresStore::connect(&url, 4).unwrap();
    instruments.migrate().unwrap();

    let stamp = now_ns();
    let resolution = resolve_identifier(
        &instruments,
        &ResolveIdentifierRequest {
            identifiers: vec![PbIdentifier {
                scheme: "symbol".into(),
                value: format!("ZZ{stamp}"),
                source: "snaptrade".into(),
            }],
            as_of_ns: stamp,
            exchange_mic: String::new(),
            currency: String::new(),
        },
        now_ns(),
    )
    .unwrap();

    assert!(resolution.reply.found);
    assert!(resolution.reply.placeholder);
    assert!(
        resolution.minted.is_some(),
        "the placeholder was not minted here"
    );
    let placeholder = resolution.reply.instrument_id;
    assert!(placeholder.starts_with("LCL-"), "{placeholder}");

    let street = meridian_street::PostgresStore::connect(&url, 2).unwrap();
    street.migrate().unwrap();
    let (statement, _, _) = street
        .open(Statement {
            statement_id: format!("STMT-outage-{stamp}"),
            source: "snaptrade".into(),
            external_statement_id: format!("st-outage-{stamp}"),
            as_of_date: "2026-09-28".into(),
            read_at_ns: stamp,
            expected_rows: 1,
            figures: Default::default(),
        })
        .unwrap();

    let account = format!("ACC-outage-{stamp}");
    let (settled, _) = street
        .record(
            Holding {
                holding_id: format!("HLD-outage-{stamp}"),
                statement_id: statement.statement_id,
                account_id: account.clone(),
                instrument_id: Some(placeholder.clone()),
                unresolved_identifiers: vec![],
                side: Side::Long,
                quantity: "5".parse::<Quantity>().unwrap(),
                settle_date_quantity: None,
                market_value: Some(Money::new(Default::default(), "USD")),
                currency_assumed: false,
                also_counted_in_cash: false,
                escalated: false,
            },
            now_ns(),
        )
        .unwrap();

    assert!(matches!(settled, Settled::Changed { .. }), "{settled:?}");
    assert!(street
        .custodial_position(&account, &placeholder, Side::Long)
        .unwrap()
        .is_some());
}
