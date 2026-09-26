//! The plugin report, from what the sidecar saw.

use std::sync::Arc;
use std::time::Duration;

use meridian_bus::{Bus, MemoryBackend, Subscription};
use meridian_domain::v1::PluginReport;
use meridian_pb::plugin::v1::plugin_operations_server::PluginOperations;
use meridian_pb::plugin::v1::RecordHoldingsStatementParams;
use meridian_pb::v1::sidecar_service_server::SidecarService;
use meridian_pb::v1::{HeartbeatRequest, LeaveRequest, RegisterRequest};
use prost::Message;
use tonic::Request;

use super::*;
use crate::grants::Contract;
use crate::service::Identity;

fn sidecar(bus: Arc<Bus>) -> Arc<Sidecar> {
    let contract = Contract::parse(
        "topic\tkind\tpublisher\tsubscriber\n\
         platform.deployment.event.plugin-report\tevent\tsidecar\tconductor\n",
        "name\tkind\ncustody\trole\nsidecar\tcomponent\n",
    )
    .unwrap();
    Arc::new(Sidecar::under(
        &contract,
        bus,
        "dep-local-1",
        Identity::new("snaptrade-1", vec!["custody".into()]).with_tags(vec!["holdings".into()]),
    ))
}

fn memory() -> Arc<Bus> {
    Arc::new(Bus::single("snaptrade-1", Arc::new(MemoryBackend::new())))
}

async fn register(sidecar: &Sidecar) {
    let reply = sidecar
        .register(Request::new(RegisterRequest {
            schema_version: "v2".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(reply.admitted, "{}", reply.refusal_reason);
}

#[tokio::test]
async fn before_registering_a_plugin_is_reported_as_what_it_was_launched_as() {
    let report = sidecar(memory()).report(7);
    assert_eq!(report.plugin_instance_id, "snaptrade-1");
    assert_eq!(report.roles, vec!["custody"]);
    assert_eq!(report.tags, vec!["holdings"]);
    assert!(!report.registered && !report.healthy);
    assert_eq!(report.reported_at_ns, 7);
}

#[tokio::test]
async fn what_the_plugin_said_and_what_it_was_refused_are_reported() {
    let sidecar = sidecar(memory());
    register(&sidecar).await;
    sidecar
        .heartbeat(Request::new(HeartbeatRequest {
            healthy: false,
            detail: "required setting api_key is not set".into(),
        }))
        .await
        .unwrap();
    // Custody holds no row in this contract, so each is refused its grant.
    for _ in 0..2 {
        let statement = RecordHoldingsStatementParams {
            source: "snaptrade".into(),
            expected_rows: 1,
            ..Default::default()
        };
        assert!(sidecar
            .record_holdings_statement(Request::new(statement))
            .await
            .is_err());
    }

    let report = sidecar.report(7);
    assert!(report.registered);
    assert!(!report.healthy);
    assert_eq!(report.health_detail, "required setting api_key is not set");
    assert!(report.last_heartbeat_at_ns > 0);
    assert_eq!(report.contract_version, "v2");
    assert_eq!(report.refused_grants, 2);
    assert_eq!(
        report.last_refusal_reason,
        "no grant for platform.street.command.record-statement: this plugin holds custody"
    );

    sidecar
        .leave(Request::new(LeaveRequest {
            reason: "redeploy".into(),
        }))
        .await
        .unwrap();
    assert!(
        !sidecar.report(8).registered,
        "a plugin that left is not registered"
    );
}

/// The next report, well inside the 30 seconds between scheduled ones.
async fn next(reports: &mut Subscription) -> meridian_bus::Delivery {
    tokio::time::timeout(Duration::from_secs(2), reports.recv())
        .await
        .expect("a report")
        .expect("the bus is open")
}

#[tokio::test]
async fn a_report_goes_out_at_start_and_again_at_once_when_the_plugin_registers() {
    let bus = memory();
    let mut reports = bus.subscribe(PLUGIN_REPORT);
    let sidecar = sidecar(Arc::clone(&bus));
    tokio::spawn(report_forever(Arc::clone(&sidecar)));

    let first = PluginReport::decode(&next(&mut reports).await.envelope.payload[..]).unwrap();
    assert!(!first.registered);
    register(&sidecar).await;
    let second = PluginReport::decode(&next(&mut reports).await.envelope.payload[..]).unwrap();
    assert!(second.registered);
}
