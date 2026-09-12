//! One process, three surfaces, one bus.
//!
//! Every part here is tested in its own crate against its own harness, and none
//! of that says they were wired together. This is the test that would have
//! failed for as long as the kernel existed and no process ran it: the ledger
//! compiled, passed 55 tests, and was reachable from nothing.
//!
//! So what is under test is the assembly, and it is driven the way a plugin
//! drives it — through the sidecar's gRPC surface, against the grant table the
//! deployment actually ships. Calling the bus directly would prove the handlers
//! registered and skip the half that decides whether a connector gets in.

use std::sync::Arc;

use meridian_bus::{Bus, MemoryBackend};
use meridian_pb::v1::sidecar_service_server::SidecarService;
use meridian_pb::v1::{
    CallRequest, ListCustodialPositionsReply, ListCustodialPositionsRequest, RecordHoldingReply,
    RecordHoldingRequest, RecordHoldingsStatementReply, RecordHoldingsStatementRequest,
    RegisterRequest, ResolveIdentifierReply, ResolveIdentifierRequest,
};
use meridian_sidecar::{GrantTable, Sidecar};
use prost::Message;
use tonic::Request;

/// The grants the compose file mounts and the chart's ConfigMap carries.
const GRANTS: &str = include_str!("../../../deploy/grants.example.json");

const NOW: i64 = 1_757_376_000_000_000_000;

/// The wiring `main` does, minus the platform client and the two Postgres
/// stores, which need a network and a database.
fn runtime() -> (Arc<Bus>, Sidecar) {
    let bus = Arc::new(Bus::single("runtime-test", Arc::new(MemoryBackend::new())));

    meridian_kernel::service::serve(
        bus.clone(),
        Arc::new(meridian_kernel::MemoryStore::new()),
        Arc::new(meridian_kernel::service::SystemClock),
    );
    meridian_reference::service::serve_queries(
        &bus,
        Arc::new(meridian_reference::MemoryStore::new()),
    );

    let sidecar = Sidecar::new(bus.clone(), "DEP-test", "v1");
    sidecar.load_grants(GrantTable::from_json(GRANTS).expect("the shipped grants parse"));

    (bus, sidecar)
}

async fn admitted(sidecar: &Sidecar, instance_id: &str, role: &str) {
    let reply = sidecar
        .register(Request::new(RegisterRequest {
            instance_id: instance_id.into(),
            role: role.into(),
            tags: vec![],
            schema_version: "v1".into(),
        }))
        .await
        .expect("register is served")
        .into_inner();
    assert!(reply.admitted, "the shipped grants refuse the {role} role");
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
    admitted(&sidecar, "custody-snaptrade-1", "custody").await;

    // The reference side answers on the same process. Nothing is loaded, so the
    // answer is a miss — which is the honest one, and still proves the handler
    // is registered rather than absent.
    let resolved: ResolveIdentifierReply = call(
        &sidecar,
        meridian_reference::service::RESOLVE_IDENTIFIER,
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

    // The ledger side, on that same bus: open a statement promising one row.
    let opened: RecordHoldingsStatementReply = call(
        &sidecar,
        meridian_kernel::service::RECORD_STATEMENT,
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
        meridian_kernel::service::RECORD_HOLDING,
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
    let dashboard = Sidecar::new(bus, "DEP-test", "v1");
    dashboard.load_grants(GrantTable::from_json(GRANTS).unwrap());
    admitted(&dashboard, "dashboard-1", "dashboard").await;

    let listed: ListCustodialPositionsReply = call(
        &dashboard,
        meridian_kernel::service::LIST_CUSTODIAL_POSITIONS,
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
    assert!(custody.may_publish(meridian_kernel::service::RECORD_STATEMENT));
    assert!(custody.may_publish(meridian_kernel::service::RECORD_HOLDING));
    assert!(custody.may_publish("platform.custody.custody-snaptrade-1.event.sync-status"));
    // A connector states what it holds; it does not announce that a position
    // moved. That is the kernel's to say.
    assert!(!custody.may_publish(meridian_kernel::service::CUSTODIAL_POSITION_UPDATED));

    let dashboard = table.resolve("dashboard", &[]);
    assert!(dashboard.may_publish(meridian_kernel::service::LIST_CUSTODIAL_POSITIONS));
    assert!(dashboard.may_subscribe(meridian_kernel::service::CUSTODIAL_POSITION_UPDATED));
    // Read-only means read-only: nothing a dashboard does writes to the ledger.
    assert!(!dashboard.may_publish(meridian_kernel::service::RECORD_HOLDING));

    // Denial is by absence, so an unknown role gets nothing rather than
    // everything.
    let stranger = table.resolve("not-a-role", &[]);
    assert!(!stranger.may_publish(meridian_kernel::service::RECORD_HOLDING));
    assert!(!stranger.may_subscribe(meridian_kernel::service::CUSTODIAL_POSITION_UPDATED));
}
