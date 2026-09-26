//! The typed operations through the sidecar, against an in-memory bus with a
//! stand-in for each component that answers: the street store records, the
//! conductor holds the plugin's links.

use std::sync::{Arc, Mutex};

use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::v1::{
    ExternalAccountLink, MissingInstrumentDetectedEvent, PluginConfiguration,
    PluginConfigurationChangedEvent, RecordHoldingReply, RecordHoldingRequest, SyncStatusEvent,
};
use meridian_pb::plugin::v1::plugin_operations_server::PluginOperations;
use meridian_pb::plugin::v1::{
    Identifier, RecordHoldingParams, ReportMissingInstrumentParams, ReportSyncStatusParams,
};
use meridian_pb::v1::sidecar_service_server::SidecarService;
use meridian_pb::v1::RegisterRequest;
use prost::Message;
use tonic::{Code, Request};

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
         platform.config.query.plugin-configuration\tquery\tsidecar\tconductor\n",
        "name\tkind\ncustody\trole\nstreet\tcomponent\nsidecar\tcomponent\n",
    )
    .unwrap()
}

/// A registered sidecar for `snaptrade-1` on its own bus, a conductor that
/// links `ext-1` to `ACC-1`, and a street store that keeps what it records.
async fn registered(roles: &[&str]) -> (Sidecar, Arc<Bus>, Arc<Mutex<Vec<RecordHoldingRequest>>>) {
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
    bus.serve(super::PLUGIN_CONFIGURATION, |_| {
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
                    // Another plugin's link to the same external name, which
                    // must not be this one's.
                    ExternalAccountLink {
                        plugin_instance_id: "other-1".into(),
                        external_account_id: "ext-2".into(),
                        account_id: "ACC-9".into(),
                    },
                ],
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    let roles = roles.iter().map(|r| r.to_string()).collect();
    let sidecar = Sidecar::under(
        &contract(),
        Arc::clone(&bus),
        "DEP-test",
        Identity::new("snaptrade-1", roles),
    );
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
    bus.serve(super::PLUGIN_CONFIGURATION, |_| {
        Ok((
            "meridian.v1.PluginConfiguration".into(),
            PluginConfiguration {
                plugin_instance_id: "snaptrade-1".into(),
                links: vec![ExternalAccountLink {
                    plugin_instance_id: "snaptrade-1".into(),
                    external_account_id: "ext-3".into(),
                    account_id: "ACC-3".into(),
                }],
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    bus.publish(
        super::PLUGIN_CONFIGURATION_CHANGED,
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
