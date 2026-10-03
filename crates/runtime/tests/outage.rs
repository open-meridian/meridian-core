//! What a deployment does while the platform is not there.
//!
//! Run by `make demo` with the platform's container stopped, which is the only
//! way to find out. Everything the crate says about outages is a claim until
//! something is actually switched off, and a mocked outage tests the mock.

use std::sync::Arc;
use std::time::Duration;

use meridian_conductor::{Config, DeploymentKey, HttpTransport, Platform};
use meridian_domain::v1::{Identifier as PbIdentifier, ResolveIdentifierRequest};
use meridian_instrument::{resolve_identifier, PostgresStore};
use meridian_street::amounts::{Money, Quantity};
use meridian_street::store::{Cause, Holding, Settled, Side, Statement, Store as _};
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

    // A record the deployment holds: its own, minted for what a plugin
    // reported (decisions/030), before anything is asked of the platform.
    let stamp = now_ns();
    let figi = format!("BBG{stamp}");
    let asking = ResolveIdentifierRequest {
        identifiers: vec![PbIdentifier {
            scheme: "figi".into(),
            value: figi,
            source: String::new(),
        }],
        as_of_ns: stamp,
        ..Default::default()
    };
    let instrument_id = resolve_identifier(&store, &asking, "outage", now_ns())
        .unwrap()
        .reply
        .instrument_id;

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
    let reply = resolve_identifier(&store, &asking, "outage", now_ns())
        .unwrap()
        .reply;

    assert!(
        reply.found,
        "the instrument store stopped answering for what it holds"
    );
    assert_eq!(reply.instrument_id, instrument_id);
}

#[test]
fn a_holding_nobody_has_seen_is_recorded_against_a_record_minted_while_the_platform_is_away() {
    // W3.7's reason for being: the holding has a name at once, platform or no
    // platform -- the deployment's own, for life (decisions/030).
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
            ..Default::default()
        },
        "outage",
        now_ns(),
    )
    .unwrap();

    assert!(resolution.reply.found);
    assert!(resolution.reply.minted);
    assert!(
        resolution.changed.is_some(),
        "the record was not minted here"
    );
    let placeholder = resolution.reply.instrument_id;
    assert!(placeholder.starts_with("LCL-"), "{placeholder}");

    let street = meridian_street::PostgresStore::connect(&url, 2).unwrap();
    street.migrate(&meridian_clock::SystemClock).unwrap();
    let (statement, _, _) = street
        .open(
            Statement {
                statement_id: format!("STMT-outage-{stamp}"),
                source: "snaptrade".into(),
                external_statement_id: format!("st-outage-{stamp}"),
                as_of_date: "2026-09-28".into(),
                read_at_ns: stamp,
                expected_rows: 1,
                account_id: String::new(),
                external_account_id: String::new(),
                institution: String::new(),
                figures: Vec::new(),
                currency_assumed: false,
                security_interest: None,
                raw_record: None,
                provenance: Vec::new(),
                completed: None,
            },
            &Cause {
                committed_at_ns: now_ns(),
                ..Default::default()
            },
        )
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
                cost: Default::default(),
                escalated: false,
            },
            &Cause {
                committed_at_ns: now_ns(),
                ..Default::default()
            },
        )
        .unwrap();

    assert!(matches!(settled, Settled::Changed { .. }), "{settled:?}");
    assert!(street
        .custodial_position(&account, &placeholder, Side::Long)
        .unwrap()
        .is_some());
}
