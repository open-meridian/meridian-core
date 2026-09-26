//! Settings, access and scope, against a stand-in conductor whose answer a
//! test can change and announce.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::v1::{
    ExternalAccountLink, PluginConfigurationChangedEvent, PluginReport, PluginSettingValue,
    UnlinkedExternalAccountsEvent,
};
use meridian_pb::plugin::v1::plugin_operations_server::PluginOperations;
use meridian_pb::plugin::v1::RecordHoldingParams;
use meridian_pb::v1::sidecar_service_server::SidecarService;
use meridian_pb::v1::{
    PluginAccessReply, RegisterRequest, SettingType, UserGroupAccess, WatchAccountScopeRequest,
    WatchSettingsRequest,
};
use tokio_stream::StreamExt;
use tonic::{Code, Request};

use super::*;
use crate::configuration::{PLUGIN_CONFIGURATION, PLUGIN_CONFIGURATION_CHANGED};
use crate::grants::Contract;
use crate::report::{report_forever, PLUGIN_REPORT, UNLINKED_EXTERNAL_ACCOUNTS};
use crate::service::Identity;

fn declared(name: &str, required: bool) -> SettingDeclaration {
    SettingDeclaration {
        name: name.into(),
        r#type: SettingType::String as i32,
        required,
        secret: required,
        description: String::new(),
    }
}

fn value(name: &str, value: &str) -> PluginSettingValue {
    PluginSettingValue {
        name: name.into(),
        value: value.into(),
    }
}

#[test]
fn a_plugin_is_given_the_values_of_what_it_declared_and_told_what_is_missing() {
    let configuration = PluginConfiguration {
        settings: vec![
            value("region", "eu"),
            value("api_key", ""),
            value("not_declared", "someone else's"),
        ],
        ..Default::default()
    };
    let delivered = settings(
        &[
            declared("api_key", true),
            declared("region", false),
            declared("client_id", true),
            // Optional, with no value: not missing, since nothing requires it.
            declared("timezone", false),
        ],
        &configuration,
    );
    assert_eq!(
        delivered.values,
        vec![SettingValue {
            name: "region".into(),
            value: "eu".into()
        }]
    );
    assert_eq!(delivered.missing_required, vec!["api_key", "client_id"]);
}

/// A registered plugin declaring `api_key` as required, and a conductor
/// answering with whatever `held` says.
async fn registered() -> (Arc<Sidecar>, Arc<Bus>, Arc<Mutex<PluginConfiguration>>) {
    let bus = Arc::new(Bus::single("snaptrade-1", Arc::new(MemoryBackend::new())));
    let held = Arc::new(Mutex::new(PluginConfiguration {
        plugin_instance_id: "snaptrade-1".into(),
        read_account_ids: vec!["ACC-1".into()],
        ..Default::default()
    }));
    let answering = Arc::clone(&held);
    bus.serve(PLUGIN_CONFIGURATION, move |_| {
        Ok((
            "meridian.v1.PluginConfiguration".into(),
            answering.lock().unwrap().encode_to_vec(),
        ))
    });
    bus.serve(PLUGIN_ACCESS, |_| {
        Ok((
            "meridian.v1.PluginAccessReply".into(),
            PluginAccessReply {
                user_groups: vec![UserGroupAccess {
                    user_group_id: "UG-1".into(),
                    name: "Operations".into(),
                    access: vec![],
                }],
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    let contract = Contract::parse(
        "topic\tkind\tpublisher\tsubscriber\n\
         platform.street.command.record-holding\tcommand\tcustody\tstreet\n",
        "name\tkind\ncustody\trole\nstreet\tcomponent\n",
    )
    .unwrap();
    let sidecar = Arc::new(Sidecar::under(
        &contract,
        Arc::clone(&bus),
        "dep-local-1",
        Identity::new("snaptrade-1", vec!["custody".into()]),
    ));
    let reply = sidecar
        .register(Request::new(RegisterRequest {
            schema_version: "v2".into(),
            settings: vec![declared("api_key", true)],
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(reply.admitted, "{}", reply.refusal_reason);
    (sidecar, bus, held)
}

fn announce(bus: &Bus) {
    bus.publish(
        PLUGIN_CONFIGURATION_CHANGED,
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
}

async fn next<T>(stream: &mut Following<T>) -> T {
    tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("an item within two seconds")
        .expect("the stream is open")
        .expect("an item, not a refusal")
}

#[tokio::test]
async fn settings_arrive_at_once_and_again_when_the_conductor_says_they_changed() {
    let (sidecar, bus, held) = registered().await;
    let mut settings = sidecar
        .watch_settings(Request::new(WatchSettingsRequest {}))
        .await
        .unwrap()
        .into_inner();
    let first = next(&mut settings).await;
    assert!(first.values.is_empty());
    assert_eq!(first.missing_required, vec!["api_key"]);

    held.lock().unwrap().settings = vec![value("api_key", "sk-123")];
    announce(&bus);
    let second = next(&mut settings).await;
    assert_eq!(
        second.values,
        vec![SettingValue {
            name: "api_key".into(),
            value: "sk-123".into()
        }]
    );
    assert!(second.missing_required.is_empty());

    // A change to something else is not a change to the settings.
    held.lock().unwrap().read_account_ids.push("ACC-2".into());
    announce(&bus);
    assert!(
        tokio::time::timeout(Duration::from_millis(300), settings.next())
            .await
            .is_err(),
        "nothing new to say, so nothing said"
    );
}

#[tokio::test]
async fn the_account_scope_arrives_at_once_and_again_when_it_changes() {
    let (sidecar, bus, held) = registered().await;
    let mut scope = sidecar
        .watch_account_scope(Request::new(WatchAccountScopeRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(next(&mut scope).await.read_account_ids, vec!["ACC-1"]);
    held.lock().unwrap().write_account_ids = vec!["ACC-1".into()];
    announce(&bus);
    let changed = next(&mut scope).await;
    assert_eq!(changed.write_account_ids, vec!["ACC-1"]);
}

#[tokio::test]
async fn the_access_table_is_the_conductors_asked_as_the_plugin() {
    let (sidecar, _, _) = registered().await;
    let table = sidecar
        .plugin_access(Request::new(PluginAccessRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(table.user_groups[0].user_group_id, "UG-1");
}

#[tokio::test]
async fn nothing_is_learned_before_registering() {
    let bus = Arc::new(Bus::single("snaptrade-1", Arc::new(MemoryBackend::new())));
    let sidecar = Sidecar::under(
        &Contract::parse("topic\tkind\tpublisher\tsubscriber\n", "name\tkind\n").unwrap(),
        bus,
        "dep-local-1",
        Identity::new("snaptrade-1", vec![]),
    );
    assert_eq!(
        sidecar
            .watch_settings(Request::new(WatchSettingsRequest {}))
            .await
            .err()
            .unwrap()
            .code(),
        Code::FailedPrecondition
    );
    assert_eq!(
        sidecar
            .plugin_access(Request::new(PluginAccessRequest {}))
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
    assert_eq!(
        sidecar
            .watch_account_scope(Request::new(WatchAccountScopeRequest {}))
            .await
            .err()
            .unwrap()
            .code(),
        Code::FailedPrecondition
    );
}

#[tokio::test]
async fn a_plugin_missing_a_required_setting_is_reported_unhealthy_until_it_arrives() {
    let (sidecar, bus, held) = registered().await;
    let missing = sidecar.report_now(7).await;
    assert!(!missing.healthy);
    assert_eq!(missing.health_detail, "required setting api_key is not set");

    let mut reports = bus.subscribe(PLUGIN_REPORT);
    tokio::spawn(report_forever(Arc::clone(&sidecar)));
    let _ = tokio::time::timeout(Duration::from_secs(2), reports.recv()).await;
    held.lock().unwrap().settings = vec![value("api_key", "sk-123")];
    announce(&bus);
    // Reported again when the configuration changed, well inside the 30
    // seconds between scheduled reports; one already due may come first.
    let healthy = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let report = reports.recv().await.unwrap();
            let report = PluginReport::decode(&report.envelope.payload[..]).unwrap();
            if report.healthy {
                return report;
            }
        }
    })
    .await
    .expect("reported healthy once the setting arrived");
    assert!(healthy.health_detail.is_empty());
}

#[tokio::test]
async fn external_accounts_nobody_linked_are_reported_until_one_is() {
    let (sidecar, bus, held) = registered().await;
    let row = |external: &str| RecordHoldingParams {
        statement_id: "S-1".into(),
        external_account_id: external.into(),
        ..Default::default()
    };
    for _ in 0..2 {
        assert!(sidecar
            .record_holding(Request::new(row("ext-9")))
            .await
            .is_err());
    }
    let unlinked = sidecar.unlinked_now().expect("reported");
    assert_eq!(unlinked.accounts[0].external_account_id, "ext-9");
    assert_eq!(unlinked.accounts[0].refused_rows, 2);

    let mut published = bus.subscribe(UNLINKED_EXTERNAL_ACCOUNTS);
    tokio::spawn(report_forever(Arc::clone(&sidecar)));
    let event = tokio::time::timeout(Duration::from_secs(2), published.recv())
        .await
        .expect("published beside the report")
        .unwrap();
    let event = UnlinkedExternalAccountsEvent::decode(&event.envelope.payload[..]).unwrap();
    assert_eq!(event.plugin_instance_id, "snaptrade-1");

    // Linked, and a row for it gets as far as the scope: no longer unlinked.
    held.lock().unwrap().links = vec![ExternalAccountLink {
        plugin_instance_id: "snaptrade-1".into(),
        external_account_id: "ext-9".into(),
        account_id: "ACC-9".into(),
    }];
    announce(&bus);
    for _ in 0..50 {
        let _ = sidecar.record_holding(Request::new(row("ext-9"))).await;
        if sidecar.unlinked_now().is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(sidecar.unlinked_now().is_none());
}
