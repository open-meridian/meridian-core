//! The plugin report, from what the sidecar saw.

use std::sync::Arc;
use std::time::Duration;

use meridian_bus::{Bus, MemoryBackend, Subscription};
use meridian_domain::v1::PluginReport;
use meridian_pb::plugin::v1::plugin_operations_server::PluginOperations;
use meridian_pb::plugin::v1::RecordHoldingsStatementParams;
use meridian_pb::v1::plugin_figure::Value;
use meridian_pb::v1::sidecar_service_server::SidecarService;
use meridian_pb::v1::{
    FigureState, HeartbeatRequest, LeaveRequest, PluginFigure, RegisterRequest, SettingDeclaration,
    SettingType,
};
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
        Identity::new("snaptrade-1", vec!["custody".into()]),
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
            ..Default::default()
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

#[tokio::test]
async fn what_the_plugin_declared_is_reported_and_nothing_once_it_leaves() {
    let sidecar = sidecar(memory());
    let declared = vec![
        SettingDeclaration {
            name: "api_key".into(),
            r#type: SettingType::String as i32,
            required: true,
            secret: true,
            description: "The venue's API key.".into(),
            label: "API key".into(),
            ..Default::default()
        },
        SettingDeclaration {
            name: "poll_minutes".into(),
            r#type: SettingType::Integer as i32,
            ..Default::default()
        },
    ];
    assert!(sidecar.report(1).declared_settings.is_empty());
    let reply = sidecar
        .register(Request::new(RegisterRequest {
            schema_version: "v2".into(),
            settings: declared.clone(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(reply.admitted, "{}", reply.refusal_reason);
    assert_eq!(sidecar.report(2).declared_settings, declared);

    sidecar
        .leave(Request::new(LeaveRequest {
            reason: "redeploy".into(),
        }))
        .await
        .unwrap();
    assert!(
        sidecar.report(3).declared_settings.is_empty(),
        "and the conductor keeps what it last declared"
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

/// SnapTrade's figures, as the plugin-report fixture carries them.
fn snaptrade() -> Vec<PluginFigure> {
    vec![
        PluginFigure {
            label: "Connections".into(),
            value: Some(Value::Count(3)),
            state: FigureState::Warn as i32,
            why: "1 connection needs attention: the brokerage asked to reconnect".into(),
            ..Default::default()
        },
        PluginFigure {
            label: "Accounts reached".into(),
            value: Some(Value::Count(7)),
            ..Default::default()
        },
        PluginFigure {
            label: "Last read".into(),
            value: Some(Value::AtNs(1_790_380_500_000_000_000)),
            ..Default::default()
        },
    ]
}

async fn beat(sidecar: &Sidecar, figures: Vec<PluginFigure>) -> Result<(), tonic::Status> {
    sidecar
        .heartbeat(Request::new(HeartbeatRequest {
            healthy: true,
            figures,
            ..Default::default()
        }))
        .await
        .map(|_| ())
}

#[tokio::test]
async fn the_report_carries_the_last_accepted_heartbeats_figures_in_its_order() {
    let sidecar = sidecar(memory());
    assert!(
        sidecar.report(1).figures.is_empty(),
        "none while not registered"
    );
    register(&sidecar).await;
    assert!(
        sidecar.report(2).figures.is_empty(),
        "none until it reports"
    );

    beat(&sidecar, snaptrade()).await.unwrap();
    let report = sidecar.report(3);
    assert!(report.healthy);
    assert_eq!(report.figures, snaptrade());

    // Each heartbeat replaces the last, and one with none clears them.
    beat(&sidecar, snaptrade()[1..].to_vec()).await.unwrap();
    assert_eq!(sidecar.report(4).figures, snaptrade()[1..].to_vec());
    beat(&sidecar, vec![]).await.unwrap();
    assert!(sidecar.report(5).figures.is_empty());

    beat(&sidecar, snaptrade()).await.unwrap();
    sidecar
        .leave(Request::new(LeaveRequest {
            reason: "redeploy".into(),
        }))
        .await
        .unwrap();
    assert!(
        sidecar.report(6).figures.is_empty(),
        "none once it has left"
    );
}

#[tokio::test]
async fn a_refused_heartbeat_is_alive_not_healthy_with_the_refusal_and_no_figures() {
    let sidecar = sidecar(memory());
    register(&sidecar).await;
    beat(&sidecar, snaptrade()).await.unwrap();
    let before = sidecar.report(1).last_heartbeat_at_ns;

    let nine = (0..9)
        .map(|i| PluginFigure {
            label: format!("Figure {i}"),
            value: Some(Value::Count(i)),
            ..Default::default()
        })
        .collect();
    let refused = beat(&sidecar, nine).await.unwrap_err();
    assert_eq!(refused.code(), tonic::Code::InvalidArgument);
    assert_eq!(refused.message(), "9 figures; a plugin reports at most 8");

    // The fixture's case: alive, so not silent; not healthy with the refusal
    // as its reason; no figures rather than stale ones.
    let report = sidecar.report(2);
    assert!(report.registered);
    assert!(!report.healthy);
    assert_eq!(
        report.health_detail,
        "the plugin's heartbeat was refused: 9 figures; a plugin reports at most 8"
    );
    assert!(report.figures.is_empty());
    assert!(report.last_heartbeat_at_ns >= before);

    // Until a heartbeat is accepted.
    beat(&sidecar, snaptrade()).await.unwrap();
    let report = sidecar.report(3);
    assert!(report.healthy && report.health_detail.is_empty());
    assert_eq!(report.figures, snaptrade());
}

#[tokio::test]
async fn a_refused_label_names_the_figure_the_field_and_the_bound() {
    let sidecar = sidecar(memory());
    register(&sidecar).await;
    let long = PluginFigure {
        label: "Connections that need the admin to reconnect".into(),
        value: Some(Value::Count(1)),
        ..Default::default()
    };
    let refused = beat(&sidecar, vec![long]).await.unwrap_err();
    assert_eq!(refused.code(), tonic::Code::InvalidArgument);
    assert_eq!(
        refused.message(),
        "figures[0].label is 44 characters; a label is at most 40"
    );
    let undefined = PluginFigure {
        state: 7,
        ..snaptrade().remove(0)
    };
    let refused = beat(&sidecar, vec![undefined]).await.unwrap_err();
    assert_eq!(
        refused.message(),
        "figures[0].state is 7, which the contract does not define"
    );
}

#[tokio::test]
async fn a_report_goes_out_at_once_when_the_figures_change_and_not_for_a_repeat() {
    let bus = memory();
    let mut reports = bus.subscribe(PLUGIN_REPORT);
    let sidecar = sidecar(Arc::clone(&bus));
    tokio::spawn(report_forever(Arc::clone(&sidecar)));
    next(&mut reports).await;
    register(&sidecar).await;
    next(&mut reports).await;

    beat(&sidecar, snaptrade()).await.unwrap();
    let report = PluginReport::decode(&next(&mut reports).await.envelope.payload[..]).unwrap();
    assert_eq!(report.figures, snaptrade());

    beat(&sidecar, snaptrade()).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), reports.recv())
            .await
            .is_err(),
        "a heartbeat repeating the last sends no report of its own"
    );
}
