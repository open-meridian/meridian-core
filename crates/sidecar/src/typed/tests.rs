//! The typed operations through the sidecar, against an in-memory bus with a
//! stand-in for each component that answers: the street store records, the
//! conductor holds the plugin's links.

use std::sync::{Arc, Mutex};

use ed25519_dalek::{Signer as _, SigningKey};
use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::v1::{
    ExternalAccountLink, MissingInstrumentDetectedEvent, PluginConfiguration,
    PluginConfigurationChangedEvent, RecordHoldingReply, RecordHoldingRequest, SyncStatusEvent,
};
use meridian_pb::plugin::v1::plugin_operations_server::PluginOperations;
use meridian_pb::plugin::v1::{
    Identifier, RecordHoldingParams, RecordHoldingsStatementParams, ReportMissingInstrumentParams,
    ReportSyncStatusParams,
};
use meridian_pb::v1::sidecar_service_server::SidecarService;
use meridian_pb::v1::{CallerAssertion, CallerClaims, RegisterRequest, TagAccess};
use prost::Message;
use tonic::{Code, Request};

use crate::front_door::Verifier;
use crate::grants::Contract;
use crate::service::{Identity, Sidecar};

const RECORD_HOLDING: &str = "platform.street.command.record-holding";
const INSTRUMENT_MISSING: &str = "platform.reference.event.instrument-missing";

fn contract() -> Contract {
    Contract::parse(
        "topic\tkind\tpublisher\tsubscriber\n\
         platform.street.command.record-holding\tcommand\tcustody\tstreet\n\
         platform.reference.event.instrument-missing\tevent\tcustody\tinstrument\n\
         platform.custody.*.event.sync-status\tevent\tcustody\tdashboard\n\
         platform.config.query.plugin-configuration\tquery\tsidecar\tconductor\n\
         platform.street.command.record-statement\tcommand\tcustody\tstreet\n",
        "name\tkind\ncustody\trole\nstreet\tcomponent\nsidecar\tcomponent\n",
    )
    .unwrap()
}

/// A registered sidecar for `snaptrade-1` on its own bus, a conductor that
/// links `ext-1` to `ACC-1` and `ext-out` to `ACC-OUT`, with ACC-1 and ACC-3
/// in the plugin's write scope, and a street store that keeps what it records.
async fn registered(roles: &[&str]) -> (Sidecar, Arc<Bus>, Arc<Mutex<Vec<RecordHoldingRequest>>>) {
    registered_with(roles, None).await
}

async fn registered_with(
    roles: &[&str],
    verifier: Option<Arc<Verifier>>,
) -> (Sidecar, Arc<Bus>, Arc<Mutex<Vec<RecordHoldingRequest>>>) {
    let bus = Arc::new(Bus::single("snaptrade-1", Arc::new(MemoryBackend::new())));
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let keeping = Arc::clone(&recorded);
    bus.serve(RECORD_HOLDING, move |envelope| {
        let row = RecordHoldingRequest::decode(&envelope.payload[..]).map_err(|e| e.to_string())?;
        keeping.lock().unwrap().push(row);
        Ok((
            "meridian.v1.RecordHoldingReply".into(),
            RecordHoldingReply {
                holding_id: "H-1".into(),
                resolved: true,
            }
            .encode_to_vec(),
        ))
    });
    bus.serve(crate::configuration::PLUGIN_CONFIGURATION, |_| {
        Ok((
            "meridian.v1.PluginConfiguration".into(),
            PluginConfiguration {
                plugin_instance_id: "snaptrade-1".into(),
                links: vec![
                    ExternalAccountLink {
                        plugin_instance_id: "snaptrade-1".into(),
                        external_account_id: "ext-1".into(),
                        account_id: "ACC-1".into(),
                    },
                    // Linked, and nobody may write it through this plugin.
                    ExternalAccountLink {
                        plugin_instance_id: "snaptrade-1".into(),
                        external_account_id: "ext-out".into(),
                        account_id: "ACC-OUT".into(),
                    },
                    // Another plugin's link to the same external name, which
                    // must not be this one's.
                    ExternalAccountLink {
                        plugin_instance_id: "other-1".into(),
                        external_account_id: "ext-2".into(),
                        account_id: "ACC-9".into(),
                    },
                ],
                read_account_ids: vec!["ACC-1".into(), "ACC-3".into(), "ACC-R".into()],
                write_account_ids: vec!["ACC-1".into(), "ACC-3".into()],
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    let roles = roles.iter().map(|r| r.to_string()).collect();
    let mut sidecar = Sidecar::under(
        &contract(),
        Arc::clone(&bus),
        "DEP-test",
        Identity::new("snaptrade-1", roles),
    );
    if let Some(verifier) = verifier {
        sidecar = sidecar.with_verifier(verifier);
    }
    let reply = sidecar
        .register(Request::new(RegisterRequest {
            schema_version: "v1".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(reply.admitted, "{}", reply.refusal_reason);
    (sidecar, bus, recorded)
}

/// The next delivery, or a failure rather than a wait that never ends: a
/// message that was not sent is the thing these tests exist to notice.
async fn delivered(listening: &mut meridian_bus::Subscription) -> meridian_bus::Delivery {
    tokio::time::timeout(std::time::Duration::from_secs(2), listening.recv())
        .await
        .expect("nothing was delivered within two seconds")
        .expect("the subscription closed")
}

fn holding(external: &str) -> RecordHoldingParams {
    RecordHoldingParams {
        statement_id: "S-1".into(),
        instrument_id: String::new(),
        unresolved_identifiers: vec![Identifier {
            scheme: "symbol".into(),
            value: "AAPL".into(),
            source: "snaptrade".into(),
        }],
        quantity_scaled_1e8: 1_250_000_000,
        market_value_scaled_1e8: 2_000_000_000_000,
        currency: "USD".into(),
        external_account_id: external.into(),
        acting_for: None,
    }
}

#[tokio::test]
async fn a_holding_is_recorded_against_the_account_its_external_account_is_linked_to() {
    let (sidecar, _, recorded) = registered(&["custody"]).await;
    let result = sidecar
        .record_holding(Request::new(holding("ext-1")))
        .await
        .expect("recorded")
        .into_inner();
    assert_eq!(result.holding_id, "H-1");
    assert!(result.resolved);

    let rows = recorded.lock().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].account_id, "ACC-1", "stamped from the link");
    assert_eq!(rows[0].quantity_scaled_1e8, 1_250_000_000);
    assert_eq!(rows[0].unresolved_identifiers[0].value, "AAPL");
}

#[tokio::test]
async fn an_unlinked_external_account_is_refused_and_nothing_recorded() {
    let (sidecar, _, recorded) = registered(&["custody"]).await;
    // ext-2 is linked, but for another plugin.
    for external in ["ext-2", "never-linked"] {
        let refused = sidecar
            .record_holding(Request::new(holding(external)))
            .await
            .expect_err("refused");
        assert_eq!(refused.code(), Code::FailedPrecondition, "{external}");
        assert!(
            refused.message().contains("not linked"),
            "{}",
            refused.message()
        );
    }
    assert!(recorded.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_plugin_cannot_set_what_the_sidecar_stamps() {
    // Bytes for publisher_instance_id (field 5) appended to the params: the
    // plugin-facing message has no such field, so they are not carried, and
    // the sidecar's own value is what reaches the bus.
    let (sidecar, bus, _) = registered(&["custody"]).await;
    let mut listening = bus.subscribe(INSTRUMENT_MISSING);

    let mut bytes = ReportMissingInstrumentParams {
        source: "snaptrade".into(),
        asset_class: "equity".into(),
        ..Default::default()
    }
    .encode_to_vec();
    bytes.extend(
        MissingInstrumentDetectedEvent {
            publisher_instance_id: "somebody-else".into(),
            ..Default::default()
        }
        .encode_to_vec(),
    );
    let params = ReportMissingInstrumentParams::decode(bytes.as_slice()).unwrap();

    let published = sidecar
        .report_missing_instrument(Request::new(params))
        .await
        .expect("published")
        .into_inner();
    assert!(!published.message_id.is_empty());

    let delivery = delivered(&mut listening).await;
    let event = MissingInstrumentDetectedEvent::decode(&delivery.envelope.payload[..]).unwrap();
    assert_eq!(event.publisher_instance_id, "snaptrade-1");
    assert_eq!(event.source, "snaptrade");
}

#[tokio::test]
async fn a_sync_status_is_published_as_this_instance_and_its_account() {
    let (sidecar, bus, _) = registered(&["custody"]).await;
    let mut listening = bus.subscribe("platform.custody.snaptrade-1.event.sync-status");
    sidecar
        .report_sync_status(Request::new(ReportSyncStatusParams {
            source: "snaptrade".into(),
            connection_healthy: true,
            external_account_id: "ext-1".into(),
            ..Default::default()
        }))
        .await
        .expect("published");
    let delivery = delivered(&mut listening).await;
    let event = SyncStatusEvent::decode(&delivery.envelope.payload[..]).unwrap();
    assert_eq!(event.account_id, "ACC-1");
}

#[tokio::test]
async fn an_operation_no_role_grants_is_refused_naming_the_grant() {
    let (sidecar, _, recorded) = registered(&[]).await;
    let refused = sidecar
        .record_holding(Request::new(holding("ext-1")))
        .await
        .expect_err("no grant");
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert!(
        refused.message().contains(RECORD_HOLDING),
        "{}",
        refused.message()
    );
    assert!(
        refused.message().contains("no role"),
        "{}",
        refused.message()
    );
    assert!(recorded.lock().unwrap().is_empty());
}

#[tokio::test]
async fn nothing_is_served_before_registration() {
    let bus = Arc::new(Bus::single("snaptrade-1", Arc::new(MemoryBackend::new())));
    let sidecar = Sidecar::under(
        &contract(),
        bus,
        "DEP-test",
        Identity::new("snaptrade-1", vec!["custody".into()]),
    );
    let refused = sidecar
        .record_holding(Request::new(holding("ext-1")))
        .await
        .expect_err("not registered");
    assert_eq!(refused.code(), Code::FailedPrecondition);
    assert!(
        refused.message().contains("not registered"),
        "{}",
        refused.message()
    );
}

#[tokio::test]
async fn a_link_made_later_is_used_once_the_conductor_says_so() {
    let (sidecar, bus, recorded) = registered(&["custody"]).await;
    // Read once, with ext-3 unlinked.
    assert!(sidecar
        .record_holding(Request::new(holding("ext-3")))
        .await
        .is_err());

    // The conductor now links it, and announces the change.
    bus.serve(crate::configuration::PLUGIN_CONFIGURATION, |_| {
        Ok((
            "meridian.v1.PluginConfiguration".into(),
            PluginConfiguration {
                plugin_instance_id: "snaptrade-1".into(),
                links: vec![ExternalAccountLink {
                    plugin_instance_id: "snaptrade-1".into(),
                    external_account_id: "ext-3".into(),
                    account_id: "ACC-3".into(),
                }],
                write_account_ids: vec!["ACC-3".into()],
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    bus.publish(
        crate::configuration::PLUGIN_CONFIGURATION_CHANGED,
        "meridian.v1.PluginConfigurationChangedEvent",
        PluginConfigurationChangedEvent {
            plugin_instance_id: "snaptrade-1".into(),
            changed_at_ns: 1,
        }
        .encode_to_vec(),
        None,
        None,
    )
    .unwrap();

    for _ in 0..50 {
        if sidecar
            .record_holding(Request::new(holding("ext-3")))
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let rows = recorded.lock().unwrap();
    assert_eq!(rows.last().map(|r| r.account_id.as_str()), Some("ACC-3"));
}

// ── Write scope, and a person (W4.9) ────────────────────────────────────────

#[tokio::test]
async fn a_linked_account_nobody_may_write_through_the_plugin_is_refused() {
    // The plugin acting as itself: a link says whose row it is, not that
    // anybody may write it through this plugin (requirement 20).
    let (sidecar, _, recorded) = registered(&["custody"]).await;
    let refused = sidecar
        .record_holding(Request::new(holding("ext-out")))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert!(
        refused
            .message()
            .contains("outside this plugin's write scope"),
        "{}",
        refused.message()
    );
    assert!(recorded.lock().unwrap().is_empty());
    assert_eq!(
        sidecar.report(0).refused_grants,
        1,
        "and the plugin report counts it"
    );
}

#[tokio::test]
async fn the_configuration_is_read_again_after_30_seconds_and_trusted_for_10_minutes() {
    let (sidecar, bus, _) = registered(&["custody"]).await;
    const T0: i64 = 1_790_000_000_000_000_000;
    const SECOND: i64 = 1_000_000_000;
    let scope = |c: meridian_domain::v1::PluginConfiguration| c.write_account_ids;
    assert!(scope(sidecar.configuration(T0).await.unwrap()).contains(&"ACC-1".to_string()));

    // The conductor now says ACC-1 is out, and then stops answering.
    let answers = Arc::new(Mutex::new(0));
    let counting = Arc::clone(&answers);
    bus.serve(crate::configuration::PLUGIN_CONFIGURATION, move |_| {
        let mut answered = counting.lock().unwrap();
        *answered += 1;
        if *answered > 1 {
            return Err("the conductor is down".into());
        }
        Ok((
            "meridian.v1.PluginConfiguration".into(),
            PluginConfiguration::default().encode_to_vec(),
        ))
    });
    assert!(
        scope(sidecar.configuration(T0 + 29 * SECOND).await.unwrap())
            .contains(&"ACC-1".to_string()),
        "inside 30 seconds, as last read"
    );
    assert_eq!(*answers.lock().unwrap(), 0);
    assert!(
        scope(sidecar.configuration(T0 + 31 * SECOND).await.unwrap()).is_empty(),
        "past it, read again"
    );
    let read_at = T0 + 31 * SECOND;
    assert!(
        sidecar
            .configuration(read_at + 9 * 60 * SECOND)
            .await
            .is_ok(),
        "the conductor down: as last read, within 10 minutes"
    );
    let refused = sidecar
        .configuration(read_at + 11 * 60 * SECOND)
        .await
        .unwrap_err();
    assert_eq!(
        refused.code(),
        Code::Aborted,
        "and refused past them, with the conductor's reason"
    );
}

const KEY_ID: &str = "dashboard-2026-09-0a1b2c3d";

fn now() -> i64 {
    super::now_ns()
}

/// What the dashboard would have signed for a person holding `access`.
fn assertion(key: &SigningKey, access: Vec<TagAccess>) -> CallerAssertion {
    let issued = now();
    let claims = CallerClaims {
        subject: "local|ada".into(),
        display_name: "Ada".into(),
        audience_instance_id: "snaptrade-1".into(),
        access,
        issued_at_ns: issued,
        expires_at_ns: issued + 60_000_000_000,
        assertion_id: "a-1".into(),
    }
    .encode_to_vec();
    CallerAssertion {
        signature: key.sign(&claims).to_bytes().to_vec(),
        claims,
        key_id: KEY_ID.into(),
    }
}

fn writing(accounts: &[&str]) -> Vec<TagAccess> {
    vec![TagAccess {
        tag: "custody".into(),
        read_account_ids: accounts.iter().map(|a| a.to_string()).collect(),
        write_account_ids: accounts.iter().map(|a| a.to_string()).collect(),
    }]
}

fn reading(accounts: &[&str]) -> Vec<TagAccess> {
    vec![TagAccess {
        tag: "custody".into(),
        read_account_ids: accounts.iter().map(|a| a.to_string()).collect(),
        write_account_ids: vec![],
    }]
}

/// A sidecar holding the dashboard's key, and a street store that keeps whom
/// each row was recorded for.
async fn for_people() -> (Sidecar, SigningKey, Arc<Mutex<Vec<String>>>) {
    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    let verifier = Arc::new(Verifier::holding(
        "snaptrade-1",
        KEY_ID,
        key.verifying_key(),
    ));
    let (sidecar, bus, _) = registered_with(&["custody"], Some(verifier)).await;
    let subjects = Arc::new(Mutex::new(Vec::new()));
    let keeping = Arc::clone(&subjects);
    bus.serve(RECORD_HOLDING, move |envelope| {
        let meta = envelope.meta.clone().unwrap_or_default();
        keeping.lock().unwrap().push(meta.acting_for_subject);
        Ok((
            "meridian.v1.RecordHoldingReply".into(),
            RecordHoldingReply {
                holding_id: "H-1".into(),
                resolved: true,
            }
            .encode_to_vec(),
        ))
    });
    (sidecar, key, subjects)
}

fn for_person(external: &str, assertion: CallerAssertion) -> RecordHoldingParams {
    RecordHoldingParams {
        acting_for: Some(assertion),
        ..holding(external)
    }
}

#[tokio::test]
async fn a_command_sent_for_a_person_who_may_write_the_account_is_stamped_with_them() {
    let (sidecar, key, subjects) = for_people().await;
    sidecar
        .record_holding(Request::new(for_person(
            "ext-1",
            assertion(&key, writing(&["ACC-1"])),
        )))
        .await
        .expect("admitted");
    sidecar
        .record_holding(Request::new(holding("ext-1")))
        .await
        .expect("and the plugin as itself");
    assert_eq!(
        *subjects.lock().unwrap(),
        vec!["local|ada".to_string(), String::new()]
    );
}

#[tokio::test]
async fn a_person_is_refused_an_account_they_may_only_read_or_not_reach_at_all() {
    let (sidecar, key, subjects) = for_people().await;
    for access in [reading(&["ACC-1"]), writing(&["ACC-3"])] {
        let refused = sidecar
            .record_holding(Request::new(for_person("ext-1", assertion(&key, access))))
            .await
            .unwrap_err();
        assert_eq!(refused.code(), Code::PermissionDenied);
        assert!(
            refused.message().contains("may not write account ACC-1"),
            "{}",
            refused.message()
        );
    }
    // Nor does a person widen the plugin: ACC-OUT is outside its scope,
    // whatever they were vouched to write.
    let refused = sidecar
        .record_holding(Request::new(for_person(
            "ext-out",
            assertion(&key, writing(&["ACC-OUT"])),
        )))
        .await
        .unwrap_err();
    assert!(
        refused.message().contains("write scope"),
        "{}",
        refused.message()
    );
    assert!(subjects.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_assertion_the_dashboard_did_not_sign_vouches_for_nobody() {
    let (sidecar, _, subjects) = for_people().await;
    let stranger = SigningKey::generate(&mut rand::rngs::OsRng);
    let refused = sidecar
        .record_holding(Request::new(for_person(
            "ext-1",
            assertion(&stranger, writing(&["ACC-1"])),
        )))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::Unauthenticated);

    // Nor does a sidecar given no keys vouch for anybody.
    let (keyless, _, _) = registered(&["custody"]).await;
    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    let refused = keyless
        .record_holding(Request::new(for_person(
            "ext-1",
            assertion(&key, writing(&["ACC-1"])),
        )))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::Unauthenticated);
    assert!(subjects.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_assertion_the_page_was_opened_with_vouches_for_its_commands_too() {
    // Its id was recorded at the front door; a command carrying it is the
    // plugin acting on that request, not a replay.
    let (sidecar, key, subjects) = for_people().await;
    let once = assertion(&key, writing(&["ACC-1"]));
    for _ in 0..2 {
        sidecar
            .record_holding(Request::new(for_person("ext-1", once.clone())))
            .await
            .expect("admitted");
    }
    assert_eq!(subjects.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_command_naming_no_account_is_sent_for_somebody_who_may_write_something() {
    let (sidecar, key, _) = for_people().await;
    sidecar
        .bus
        .serve("platform.street.command.record-statement", |envelope| {
            let meta = envelope.meta.clone().unwrap_or_default();
            assert_eq!(meta.acting_for_subject, "local|ada");
            Ok((
                "meridian.v1.RecordHoldingsStatementReply".into(),
                meridian_domain::v1::RecordHoldingsStatementReply {
                    statement_id: "S-1".into(),
                    already_recorded: false,
                }
                .encode_to_vec(),
            ))
        });
    let statement = |access| RecordHoldingsStatementParams {
        source: "snaptrade".into(),
        expected_rows: 1,
        acting_for: Some(assertion(&key, access)),
        ..Default::default()
    };
    let refused = sidecar
        .record_holdings_statement(Request::new(statement(reading(&["ACC-1"]))))
        .await
        .unwrap_err();
    assert!(
        refused.message().contains("may write nothing"),
        "{}",
        refused.message()
    );
    sidecar
        .record_holdings_statement(Request::new(statement(writing(&["ACC-1"]))))
        .await
        .expect("admitted");
}
