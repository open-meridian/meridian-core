//! One process, three surfaces, one bus.
//!
//! Every part here is tested in its own crate against its own harness, and none
//! of that says they were wired together. This is the test that would have
//! failed for as long as the street store existed and no process ran it: it
//! compiled, passed 55 tests, and was reachable from nothing.
//!
//! So what is under test is the assembly, and it is driven the way a plugin
//! drives it — through the sidecar's gRPC surface, against the grant table the
//! deployment actually ships. Calling the bus directly would prove the handlers
//! registered and skip the half that decides whether a connector gets in.

use std::sync::Arc;

use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::v1::{
    ListCustodialPositionsReply, ListCustodialPositionsRequest, RecordHoldingReply,
    RecordHoldingRequest, RecordHoldingsStatementReply, RecordHoldingsStatementRequest,
    ResolveIdentifierReply, ResolveIdentifierRequest,
};
use meridian_pb::v1::sidecar_service_server::SidecarService;
use meridian_pb::v1::{CallRequest, RegisterRequest};
use meridian_sidecar::{GrantTable, Identity, Sidecar};
use prost::Message;
use tonic::Request;

/// The grants the compose file mounts and the chart's ConfigMap carries.
const GRANTS: &str = include_str!("../../../deploy/grants.example.json");

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

    let sidecar = Sidecar::new(
        bus.clone(),
        "DEP-test",
        Identity::new("custody-snaptrade-1", "custody"),
    );
    sidecar.load_grants(GrantTable::from_json(GRANTS).expect("the shipped grants parse"));

    (bus, sidecar)
}

/// A plugin announcing its arrival. It says nothing about who it is: the
/// sidecar was launched knowing that, and the reply is where the plugin finds
/// out.
async fn admitted(sidecar: &Sidecar, expected_role: &str) {
    let reply = sidecar
        .register(Request::new(RegisterRequest {
            schema_version: "v1".into(),
        }))
        .await
        .expect("register is served")
        .into_inner();
    assert!(
        reply.admitted,
        "the shipped grants refuse the {expected_role} role: {}",
        reply.refusal_reason
    );
    assert_eq!(reply.role, expected_role);
}

/// Call a topic the way a plugin does, and fail loudly rather than decoding
/// whatever a refusal left behind.
async fn call<T: Message + Default>(
    sidecar: &Sidecar,
    topic: &str,
    payload_type: &str,
    body: impl Message,
) -> T {
    let reply = sidecar
        .call(Request::new(CallRequest {
            topic: topic.into(),
            payload_type: payload_type.into(),
            payload: body.encode_to_vec(),
            correlation_id: String::new(),
            timeout_ms: 5_000,
        }))
        .await
        .expect("call is served")
        .into_inner();

    assert!(reply.ok, "{topic} was refused: {}", reply.failure_detail);
    T::decode(&reply.payload[..]).expect("the reply decodes")
}

/// A connector's whole path, through the sidecar, against one runtime.
#[tokio::test]
async fn a_connector_records_a_statement_and_a_dashboard_reads_the_position() {
    let (bus, sidecar) = runtime();
    admitted(&sidecar, "custody").await;

    // The reference side answers on the same process. Nothing is loaded, so the
    // answer is a miss — which is the honest one, and still proves the handler
    // is registered rather than absent.
    let resolved: ResolveIdentifierReply = call(
        &sidecar,
        meridian_instrument::service::RESOLVE_IDENTIFIER,
        "meridian.v1.ResolveIdentifierRequest",
        ResolveIdentifierRequest {
            identifiers: vec![],
            as_of_ns: NOW,
            exchange_mic: String::new(),
            currency: String::new(),
        },
    )
    .await;
    assert!(!resolved.found);

    // The street store side, on that same bus: open a statement promising one row.
    let opened: RecordHoldingsStatementReply = call(
        &sidecar,
        meridian_street::service::RECORD_STATEMENT,
        "meridian.v1.RecordHoldingsStatementRequest",
        RecordHoldingsStatementRequest {
            source: "snaptrade".into(),
            external_statement_id: "st-1".into(),
            as_of_date: "2026-09-08".into(),
            read_at_ns: NOW,
            expected_rows: 1,
        },
    )
    .await;
    assert!(!opened.already_recorded);

    let recorded: RecordHoldingReply = call(
        &sidecar,
        meridian_street::service::RECORD_HOLDING,
        "meridian.v1.RecordHoldingRequest",
        RecordHoldingRequest {
            statement_id: opened.statement_id,
            account_id: "ACC-1".into(),
            instrument_id: "INS-1".into(),
            unresolved_identifiers: vec![],
            quantity_scaled_1e8: 1_250_000_000,
            market_value_scaled_1e8: 281_250_000_000,
            currency: "USD".into(),
        },
    )
    .await;
    assert!(recorded.resolved);

    // A different plugin, a different role, reading what the first one wrote.
    // One store behind one bus, rather than two of each.
    //
    // It needs a second Sidecar because one holds a single registration, so a
    // shared endpoint admits one plugin. That is the deployment shape today and
    // not the one that is wanted; sdk-contract/sidecar-needs-a-bus-across-a-process-boundary is
    // where it changes, and this line is what should stop being necessary.
    let dashboard = Sidecar::new(bus, "DEP-test", Identity::new("dashboard-1", "admin"));
    dashboard.load_grants(GrantTable::from_json(GRANTS).unwrap());
    admitted(&dashboard, "admin").await;

    let listed: ListCustodialPositionsReply = call(
        &dashboard,
        meridian_street::service::LIST_CUSTODIAL_POSITIONS,
        "meridian.v1.ListCustodialPositionsRequest",
        ListCustodialPositionsRequest {
            account_id: "ACC-1".into(),
            include_unresolved: true,
            page_size: 100,
            cursor: String::new(),
        },
    )
    .await;

    assert_eq!(listed.positions.len(), 1);
    assert_eq!(listed.positions[0].quantity_scaled_1e8, 1_250_000_000);
}

/// The grant table is configuration, so a typo in it is a deployment where a
/// plugin is admitted and then refused on its first useful call.
#[test]
fn the_shipped_grants_admit_each_role_to_exactly_its_own_work() {
    let table = GrantTable::from_json(GRANTS).expect("the shipped grants parse");

    let custody = table.resolve("custody", &[]);
    assert!(custody.may_publish(meridian_street::service::RECORD_STATEMENT));
    assert!(custody.may_publish(meridian_street::service::RECORD_HOLDING));
    assert!(custody.may_publish("platform.custody.custody-snaptrade-1.event.sync-status"));
    // A connector states what it holds; it does not announce that a position
    // moved. That is the street store's to say.
    assert!(!custody.may_publish(meridian_street::service::CUSTODIAL_POSITION_UPDATED));

    let dashboard = table.resolve("admin", &[]);
    assert!(dashboard.may_publish(meridian_street::service::LIST_CUSTODIAL_POSITIONS));
    assert!(dashboard.may_subscribe(meridian_street::service::CUSTODIAL_POSITION_UPDATED));
    // Read-only means read-only: nothing a dashboard does writes to the street store.
    assert!(!dashboard.may_publish(meridian_street::service::RECORD_HOLDING));

    // A plugin carries a role and any number of tags, and gets the union. A
    // connector that also reads positions asks for the tag rather than having a
    // bespoke role minted for the combination.
    let both = table.resolve("custody", &["reporting".to_string()]);
    assert!(both.may_publish(meridian_street::service::RECORD_HOLDING));
    assert!(both.may_publish(meridian_street::service::LIST_CUSTODIAL_POSITIONS));
    assert!(both.may_subscribe(meridian_street::service::CUSTODIAL_POSITION_UPDATED));

    // A tag adds and never subtracts, so the role alone is the smaller set.
    assert!(!custody.may_publish(meridian_street::service::LIST_CUSTODIAL_POSITIONS));

    // Denial is by absence, so an unknown role gets nothing rather than
    // everything.
    let stranger = table.resolve("not-a-role", &[]);
    assert!(!stranger.may_publish(meridian_street::service::RECORD_HOLDING));
    assert!(!stranger.may_subscribe(meridian_street::service::CUSTODIAL_POSITION_UPDATED));
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
