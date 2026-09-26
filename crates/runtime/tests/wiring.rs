//! One process, three surfaces, one bus.
//!
//! Every part here is tested in its own crate against its own harness, and none
//! of that says they were wired together. This is the test that would have
//! failed for as long as the street store existed and no process ran it: it
//! compiled, passed 55 tests, and was reachable from nothing.
//!
//! So what is under test is the assembly, and it is driven the way a plugin
//! drives it — through the sidecar's gRPC surface, against the contract the
//! deployment actually ships. Calling the bus directly would prove the handlers
//! registered and skip the half that decides whether a connector gets in.

use std::sync::Arc;

use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::v1::{
    ExternalAccountLink, ListCustodialPositionsReply, ListCustodialPositionsRequest,
    PluginConfiguration,
};
use meridian_pb::plugin::v1::plugin_operations_server::PluginOperations;
use meridian_pb::plugin::v1::{
    RecordHoldingParams, RecordHoldingsStatementParams, ResolveIdentifierParams,
};
use meridian_pb::v1::sidecar_service_server::SidecarService;
use meridian_pb::v1::RegisterRequest;
use meridian_sidecar::{Contract, Identity, Sidecar};
use prost::Message;
use tonic::Request;

const NOW: i64 = 1_757_376_000_000_000_000;

/// The wiring `main` does, minus the platform client and the two Postgres
/// stores, which need a network and a database.
fn runtime() -> (Arc<Bus>, Sidecar) {
    let bus = Arc::new(Bus::single("runtime-test", Arc::new(MemoryBackend::new())));

    meridian_street::service::serve(
        bus.clone(),
        Arc::new(meridian_street::MemoryStore::new()),
        Arc::new(meridian_street::service::SystemClock),
    );
    meridian_instrument::service::serve_queries(
        &bus,
        Arc::new(meridian_instrument::MemoryStore::new()),
    );
    // The conductor's part, stood in for: this plugin's external account
    // `ext-1` is linked to ACC-1, which somebody may write through it.
    bus.serve("platform.config.query.plugin-configuration", |_| {
        Ok((
            "meridian.v1.PluginConfiguration".into(),
            PluginConfiguration {
                plugin_instance_id: "custody-snaptrade-1".into(),
                links: vec![ExternalAccountLink {
                    plugin_instance_id: "custody-snaptrade-1".into(),
                    external_account_id: "ext-1".into(),
                    account_id: "ACC-1".into(),
                }],
                read_account_ids: vec!["ACC-1".into()],
                write_account_ids: vec!["ACC-1".into()],
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });

    // Its grants are the contract's, compiled in: no table to load.
    let sidecar = Sidecar::new(
        bus.clone(),
        "DEP-test",
        Identity::new("custody-snaptrade-1", vec!["custody".to_string()]),
    );

    (bus, sidecar)
}

/// A plugin announcing its arrival. It says nothing about who it is: the
/// sidecar was launched knowing that, and the reply is where the plugin finds
/// out.
async fn admitted(sidecar: &Sidecar, expected_role: &str) {
    let reply = sidecar
        .register(Request::new(RegisterRequest {
            schema_version: "v2".into(),
            ..Default::default()
        }))
        .await
        .expect("register is served")
        .into_inner();
    assert!(
        reply.admitted,
        "the contract refuses the {expected_role} role: {}",
        reply.refusal_reason
    );
    assert_eq!(reply.roles, vec![expected_role.to_string()]);
}

/// A connector's whole path, through the sidecar, against one runtime.
#[tokio::test]
async fn a_connector_records_a_statement_and_a_dashboard_reads_the_position() {
    let (bus, sidecar) = runtime();
    admitted(&sidecar, "custody").await;

    // The reference side answers on the same process. Nothing is loaded, so the
    // answer is a miss — which is the honest one, and still proves the handler
    // is registered rather than absent.
    let resolved = sidecar
        .resolve_identifier(Request::new(ResolveIdentifierParams {
            as_of_ns: NOW,
            ..Default::default()
        }))
        .await
        .expect("resolve is served")
        .into_inner();
    assert!(!resolved.found);

    // The street store side, on that same bus: open a statement promising one
    // row, through the typed operations a plugin has (decisions/013).
    let opened = sidecar
        .record_holdings_statement(Request::new(RecordHoldingsStatementParams {
            source: "snaptrade".into(),
            external_statement_id: "st-1".into(),
            as_of_date: "2026-09-08".into(),
            read_at_ns: NOW,
            expected_rows: 1,
            acting_for: None,
        }))
        .await
        .expect("the statement is opened")
        .into_inner();
    assert!(!opened.already_recorded);

    let recorded = sidecar
        .record_holding(Request::new(RecordHoldingParams {
            statement_id: opened.statement_id,
            instrument_id: "INS-1".into(),
            quantity_scaled_1e8: 1_250_000_000,
            market_value_scaled_1e8: 281_250_000_000,
            currency: "USD".into(),
            external_account_id: "ext-1".into(),
            ..Default::default()
        }))
        .await
        .expect("the row is recorded against the linked account")
        .into_inner();
    assert!(recorded.resolved);

    // The dashboard reading what the connector wrote. One store behind one
    // bus, rather than two of each. The dashboard is a component and asks
    // the bus itself, as it does in a deployment (decisions/020).
    let (_, reply) = bus
        .call(
            meridian_street::service::LIST_CUSTODIAL_POSITIONS,
            "meridian.v1.ListCustodialPositionsRequest",
            ListCustodialPositionsRequest {
                account_id: "ACC-1".into(),
                include_unresolved: true,
                page_size: 100,
                cursor: String::new(),
            }
            .encode_to_vec(),
            None,
            None,
        )
        .await
        .expect("the street store answers");
    let listed = ListCustodialPositionsReply::decode(&reply[..]).expect("the reply decodes");

    assert_eq!(listed.positions.len(), 1);
    assert_eq!(listed.positions[0].quantity_scaled_1e8, 1_250_000_000);
}

/// The contract is what grants, so a revision of it that took away what a
/// role's work needs is a plugin admitted and then refused on its first useful
/// call. Held here against the contract this runtime was built with.
#[test]
fn the_contract_admits_each_role_to_exactly_its_own_work() {
    let contract = Contract::embedded();
    let roles = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();

    let custody = contract.grants_for(&roles(&["custody"])).unwrap();
    assert!(custody.may_publish(meridian_street::service::RECORD_STATEMENT));
    assert!(custody.may_publish(meridian_street::service::RECORD_HOLDING));
    assert!(custody.may_publish("platform.custody.custody-snaptrade-1.event.sync-status"));
    // A connector states what it holds; it does not announce that a position
    // moved. That is the street store's to say.
    assert!(!custody.may_publish(meridian_street::service::CUSTODIAL_POSITION_UPDATED));

    // The dashboard is a component, and reads; nothing it does writes to the
    // street store.
    let dashboard = contract.component("dashboard");
    assert!(dashboard.may_publish(meridian_street::service::LIST_CUSTODIAL_POSITIONS));
    assert!(dashboard.may_subscribe(meridian_street::service::CUSTODIAL_POSITION_UPDATED));
    assert!(!dashboard.may_publish(meridian_street::service::RECORD_HOLDING));

    // Several roles hold the union; a role the contract gives nothing holds
    // nothing, and adds nothing to another.
    let with_oms = contract.grants_for(&roles(&["custody", "oms"])).unwrap();
    assert_eq!(with_oms, custody);

    // Denial is by refusal for a name that is not a role, never by granting it
    // nothing and letting it register.
    assert!(contract.grants_for(&roles(&["not-a-role"])).is_err());
    assert!(contract.grants_for(&roles(&["street"])).is_err());
}

// ── W5.20: components say what they run, inward ─────────────────────────────

#[tokio::test]
async fn a_components_report_reaches_the_one_holding_the_key() {
    // The street store holds no key, so what it runs reaches the platform only by
    // way of the instrument store. This is that path, without a platform: the street store
    // publishes, and what the instrument store would send carries it.
    use meridian_runtime::{collect_inward, report_inward_forever, COMPONENT_REPORT_TOPIC};

    let bus = Arc::new(Bus::single("instrument-1", Arc::new(MemoryBackend::new())));
    let heard = collect_inward(Arc::clone(&bus));

    let publishing = Arc::clone(&bus);
    tokio::spawn(async move { report_inward_forever(publishing, "street", 2).await });

    // The first report goes out immediately; the interval is for the ones
    // after it, which is what makes a restart visible promptly.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(report) = heard.lock().unwrap().get("street") {
            assert_eq!(report.schema_version, 2);
            assert_eq!(report.health, "COMPONENT_HEALTH_SERVING");
            assert!(
                !report.version.is_empty(),
                "a report with no version says nothing"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "nothing arrived on {COMPONENT_REPORT_TOPIC}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
