//! The order path's rules (W11, contract v22), held over the bus with the
//! in-memory store: each command and fact as a plugin's sidecar would send
//! it -- stamped with the person, the write scope of a command naming its
//! record by ID alone, and a placement's link verdict -- and what the book
//! journals, answers and publishes (fixtures/book/ in meridian-design).

use std::sync::Arc;
use std::time::Duration;

use meridian_bus::{Bus, MemoryBackend, Stamp, Subscription};
use meridian_domain::exact::Exact;
use meridian_domain::v1::*;
use meridian_pb::v1::RefusalReason;
use prost::Message;

use crate::order_service::*;
use crate::service::{serve, LIST_POSITIONS, RESOLVE_INSTRUMENT, SET_ACCOUNT_ATTRIBUTE};
use crate::store::{OrdersRead, Scope, Store};
use crate::MemoryStore;

const PM: &str = "https://directory.example.org|8812";
const TRADER: &str = "https://directory.example.org|5530";
const ADMIN: &str = "local|admin";
const ACC: &str = "ACC-1";
const OTHER: &str = "ACC-2";
const AAPL: &str = "INS-01J8XQ4M7K0000000000AAPL";
const NOCCY: &str = "INS-01J8XQ4M7K00000000NOCCY";
/// The order tests' cash, in a currency no other test of this process
/// resolves: the code's instrument is learned process-wide (contract v18).
const CHF: &str = "INS-01J8XQ4M7K00000000CASHCHF";
/// The bus's own instance: in these tests the `ems`, the `oms` and the
/// connectivity plugin at once.
const ME: &str = "oems-1";
/// 2026-10-13T13:30:00Z.
const NOW: i64 = 1_760_362_200_000_000_000;

struct Wired {
    bus: Arc<Bus>,
    store: Arc<MemoryStore>,
    clock: Arc<meridian_clock::ManualClock>,
}

fn wired() -> Wired {
    let backend = Arc::new(MemoryBackend::new());
    let clock = Arc::new(meridian_clock::ManualClock::at(NOW));
    let bus = Arc::new(Bus::single(ME, backend, clock.clone()));
    let store = Arc::new(MemoryStore::new());
    serve(bus.clone(), store.clone(), bus.clock());
    bus.serve(RESOLVE_INSTRUMENT, |envelope| {
        let asked = ResolveInstrumentRequest::decode(&envelope.payload[..]).unwrap();
        let record = |asset_class: AssetClass, currency: &str| InstrumentRecord {
            instrument_id: asked.instrument_id.clone(),
            asset_class: asset_class as i32,
            currency: currency.into(),
            version: 3,
            ..Default::default()
        };
        let instrument = match asked.instrument_id.as_str() {
            AAPL => Some(record(AssetClass::Equity, "CHF")),
            NOCCY => Some(record(AssetClass::Equity, "")),
            CHF => Some(record(AssetClass::Cash, "CHF")),
            _ => None,
        };
        Ok((
            "meridian.v1.ResolveInstrumentReply".into(),
            ResolveInstrumentReply {
                found: instrument.is_some(),
                instrument,
            }
            .encode_to_vec(),
        ))
    });
    meridian_domain::money::learn("CHF", CHF);
    Wired { bus, store, clock }
}

fn d(text: &str) -> Option<meridian_pb::v1::Decimal> {
    Some(text.parse::<Exact>().unwrap().to_wire())
}

fn read(value: &Option<meridian_pb::v1::Decimal>) -> String {
    Exact::from_wire(value.as_ref().unwrap())
        .unwrap()
        .to_string()
}

fn usd(text: &str) -> Option<Money> {
    Some(Money {
        amount: d(text),
        currency_code: "CHF".into(),
        instrument_id: String::new(),
    })
}

/// Who a message is sent for, and what the sidecar stamps beside them.
#[derive(Clone, Default)]
struct As {
    person: Option<&'static str>,
    scope: Option<Vec<&'static str>>,
    allowed: bool,
}

fn person(subject: &'static str) -> As {
    As {
        person: Some(subject),
        scope: Some(vec![ACC]),
        allowed: true,
    }
}

fn venue() -> As {
    As {
        person: None,
        scope: Some(vec![ACC]),
        allowed: false,
    }
}

async fn send_from<M: Message>(
    bus: &Bus,
    topic: &str,
    payload_type: &str,
    message: &M,
    from: &As,
) -> Result<Vec<u8>, String> {
    let stamp = Stamp {
        acting_for_subject: from.person.unwrap_or_default().to_string(),
        account_scope: from
            .scope
            .as_ref()
            .map(|accounts| accounts.iter().map(|account| account.to_string()).collect()),
        connectivity_allowed: from.allowed,
        ..Default::default()
    };
    bus.call_stamped(
        topic,
        payload_type,
        message.encode_to_vec(),
        None,
        None,
        &stamp,
    )
    .await
    .map(|(_, payload)| payload)
    .map_err(|failed| match failed {
        meridian_bus::BusError::HandlerFailed { detail, .. } => detail,
        other => other.to_string(),
    })
}

fn code(detail: &str) -> Option<RefusalReason> {
    meridian_bus::read_refusal(detail).and_then(|(reason, _)| RefusalReason::try_from(reason).ok())
}

fn status_of(detail: &str) -> Option<(meridian_bus::RefusalStatus, Vec<String>)> {
    meridian_bus::read_refusal_status(detail).map(|(status, fields, _)| (status, fields))
}

fn buy(quantity: &str) -> ProposeOrderRequest {
    ProposeOrderRequest {
        account_id: ACC.into(),
        instrument_id: AAPL.into(),
        side: Side::Buy as i32,
        quantity: d(quantity),
        order_type: OrderType::Limit as i32,
        limit_price: usd("227.50"),
        time_in_force: TimeInForce::Day as i32,
        ..Default::default()
    }
}

async fn propose(w: &Wired, request: &ProposeOrderRequest) -> Result<Order, String> {
    send_from(
        &w.bus,
        PROPOSE_ORDER,
        "meridian.v1.ProposeOrderRequest",
        request,
        &person(PM),
    )
    .await
    .map(|reply| OrderReply::decode(&reply[..]).unwrap().order.unwrap())
}

async fn order_command<M: Message>(
    w: &Wired,
    topic: &str,
    payload_type: &str,
    message: &M,
    from: &As,
) -> Result<OrderReply, String> {
    send_from(&w.bus, topic, payload_type, message, from)
        .await
        .map(|reply| OrderReply::decode(&reply[..]).unwrap())
}

async fn waits(w: &Wired, account: &str, on: bool) {
    send_from(
        &w.bus,
        SET_ACCOUNT_ATTRIBUTE,
        "meridian.v1.SetAccountAttributeRequest",
        &SetAccountAttributeRequest {
            account_id: account.into(),
            attribute: Some(set_account_attribute_request::Attribute::WaitsForCompliance(on)),
            reason: "set by the deployment admin".into(),
        },
        &As {
            person: Some(ADMIN),
            ..Default::default()
        },
    )
    .await
    .unwrap();
}

async fn release(w: &Wired, order_id: &str) -> Result<OrderReply, String> {
    order_command(
        w,
        RELEASE_ORDER,
        "meridian.v1.ReleaseOrderRequest",
        &ReleaseOrderRequest {
            order_id: order_id.into(),
            idempotency_key: String::new(),
        },
        &person(TRADER),
    )
    .await
}

/// A proposed order, released on an account that does not wait.
async fn released(w: &Wired, quantity: &str) -> Order {
    waits(w, ACC, false).await;
    let order = propose(w, &buy(quantity)).await.unwrap();
    release(w, &order.order_id).await.unwrap().order.unwrap()
}

async fn place(
    w: &Wired,
    order_id: &str,
    quantity: &str,
    from: &As,
) -> Result<PlacementReply, String> {
    send_from(
        &w.bus,
        CREATE_PLACEMENT,
        "meridian.v1.CreatePlacementRequest",
        &CreatePlacementRequest {
            order_id: order_id.into(),
            connectivity_instance_id: ME.into(),
            venue_account_code: "SNAP-ACCT-9F2".into(),
            quantity: d(quantity),
            limit_price: usd("227.50"),
            ..Default::default()
        },
        from,
    )
    .await
    .map(|reply| PlacementReply::decode(&reply[..]).unwrap())
}

async fn report(w: &Wired, placement_id: &str, event: PlacementEvent) -> Result<FactReply, String> {
    send_from(
        &w.bus,
        REPORT_PLACEMENT,
        "meridian.v1.ReportPlacementRequest",
        &ReportPlacementRequest {
            placement_id: placement_id.into(),
            event: event as i32,
            venue_order_id: "ALP-55021-120".into(),
            ..Default::default()
        },
        &venue(),
    )
    .await
    .map(|reply| FactReply::decode(&reply[..]).unwrap())
}

fn fill(placement_id: &str, quantity: &str, venue_id: &str) -> ReportExecutionRequest {
    ReportExecutionRequest {
        placement_id: placement_id.into(),
        account_id: ACC.into(),
        venue_account_code: "SNAP-ACCT-9F2".into(),
        venue_execution_id: venue_id.into(),
        kind: ExecutionKind::VenueReported as i32,
        source: ExecutionSource::Primary as i32,
        quantity: d(quantity),
        price: usd("227.41"),
        trade_time_ns: NOW,
        trade_date: "2026-10-13".into(),
        finality: Some(Finality {
            level: FinalityLevel::Final as i32,
            at_ns: NOW,
        }),
        computed_settlement_date: "2026-10-14".into(),
        raw_record: Some(meridian_pb::v1::RawRecordRef {
            instance_id: ME.into(),
            key: format!("orders/{venue_id}"),
        }),
        ..Default::default()
    }
}

async fn execute(w: &Wired, request: &ReportExecutionRequest) -> Result<FactReply, String> {
    send_from(
        &w.bus,
        REPORT_EXECUTION,
        "meridian.v1.ReportExecutionRequest",
        request,
        &venue(),
    )
    .await
    .map(|reply| FactReply::decode(&reply[..]).unwrap())
}

fn state(order: &Order) -> OrderState {
    OrderState::try_from(order.status.as_ref().unwrap().state).unwrap()
}

fn quantities(order: &Order) -> OrderQuantities {
    order.status.clone().unwrap().quantities.unwrap()
}

fn holds(order: &Order, condition: OrderCondition) -> bool {
    order
        .status
        .as_ref()
        .unwrap()
        .conditions
        .iter()
        .any(|held| held.condition == condition as i32)
}

fn order_now(w: &Wired, order_id: &str) -> Order {
    w.store
        .orders(&OrdersRead {
            scope: Scope::Everything,
            account_id: String::new(),
            since: None,
            limit: 500,
            cursor: String::new(),
        })
        .unwrap()
        .orders
        .into_iter()
        .find(|order| order.order_id == order_id)
        .unwrap()
}

async fn next(subscription: &mut Subscription) -> meridian_bus::Delivery {
    tokio::time::timeout(Duration::from_secs(5), subscription.recv())
        .await
        .expect("nothing was published within five seconds")
        .expect("the bus shut down")
}

// ── The table ───────────────────────────────────────────────────────────────

#[test]
fn every_row_the_book_names_is_in_the_generated_table() {
    for id in [
        "1", "2", "3", "4", "5", "6", "8", "9", "10", "11", "12", "13", "14", "15", "16", "17",
        "18", "19", "20", "21", "22", "24", "26", "27", "28", "29", "30", "31", "i1", "i2", "i3",
        "i4", "i6",
    ] {
        let row = crate::order_decide::row(id);
        assert_eq!(row.row, id);
    }
    assert_eq!(crate::transitions::STATES.len(), 7);
    // A release is allowed only from proposed, to released.
    let release = crate::order_decide::row("1");
    assert!(crate::order_decide::admits(
        release,
        Some(OrderState::Proposed),
        OrderState::Released
    ));
    assert!(!crate::order_decide::admits(
        release,
        Some(OrderState::Working),
        OrderState::Released
    ));
    // A venue-originated order is a record being created.
    let recorded = crate::order_decide::row("23a");
    assert!(crate::order_decide::starts_from(recorded, None));
}

// ── W11.1: propose ──────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_proposal_states_exactly_one_of_quantity_and_cash_quantity() {
    let w = wired();
    let mut both = buy("10");
    both.cash_quantity = usd("2275.00");
    let refused = propose(&w, &both).await.unwrap_err();
    assert_eq!(
        status_of(&refused),
        Some((
            meridian_bus::RefusalStatus::InvalidArgument,
            vec!["quantity".to_string(), "cash_quantity".to_string()]
        ))
    );
    let mut neither = buy("10");
    neither.quantity = None;
    assert_eq!(
        status_of(&propose(&w, &neither).await.unwrap_err())
            .unwrap()
            .1,
        vec!["quantity", "cash_quantity"]
    );
    let pass = buy("0");
    assert_eq!(
        status_of(&propose(&w, &pass).await.unwrap_err()).unwrap().1,
        vec!["quantity"]
    );
    let mut amount = buy("10");
    amount.quantity = None;
    amount.cash_quantity = usd("1000.00");
    amount.order_type = OrderType::Market as i32;
    let order = propose(&w, &amount).await.unwrap();
    assert_eq!(read(&quantities(&order).ordered), "1000.00");
    assert!(!w.store.journal(ACC).unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_proposal_is_proposed_entered_by_its_user_and_names_what_its_instrument_lacks() {
    let w = wired();
    let mut ordered = w.bus.subscribe(ORDER_UPDATED);
    let order = propose(&w, &buy("10")).await.unwrap();
    assert!(order.order_id.starts_with("ORD-"));
    assert_eq!(state(&order), OrderState::Proposed);
    let q = quantities(&order);
    assert_eq!(
        (read(&q.ordered), read(&q.idle), read(&q.leaves)),
        ("10".into(), "0".into(), "10".into())
    );
    let entered = order.person_roles.unwrap().entered_by.unwrap();
    assert!(matches!(entered.kind, Some(actor::Kind::Person(ref p)) if p.subject == PM));
    let heard = OrderUpdatedEvent::decode(&next(&mut ordered).await.envelope.payload[..]).unwrap();
    assert_eq!(heard.order.unwrap().order_id, order.order_id);
    assert_eq!(heard.entry.unwrap().kind, "order-proposed");

    // A new account waits for compliance, recorded in its first entry.
    let attributes = w.store.journal(ACC).unwrap()[0]
        .body
        .attributes
        .clone()
        .unwrap();
    assert!(attributes.waits_for_compliance);

    let mut lacking = buy("10");
    lacking.instrument_id = NOCCY.into();
    let refused = propose(&w, &lacking).await.unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::Incomplete));
    assert_eq!(
        meridian_bus::refusal_fields(&refused),
        vec!["instrument.currency"]
    );

    let nobody = send_from(
        &w.bus,
        PROPOSE_ORDER,
        "meridian.v1.ProposeOrderRequest",
        &buy("10"),
        &As::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&nobody), Some(RefusalReason::ActorRequired));
}

// ── W11.6: the gate ─────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_release_waits_for_compliance_and_is_released_at_the_verdict() {
    let w = wired();
    let order = propose(&w, &buy("10")).await.unwrap();
    let held = release(&w, &order.order_id).await.unwrap().order.unwrap();
    assert_eq!(state(&held), OrderState::Proposed);
    assert!(holds(&held, OrderCondition::AwaitingCheck));

    // A verdict on an earlier version does not answer it.
    let stale = order_command(
        &w,
        RECORD_CHECK,
        "meridian.v1.RecordCheckRequest",
        &RecordCheckRequest {
            order_id: order.order_id.clone(),
            order_version: 1,
            verdict: CheckVerdict::Pass as i32,
            rule: "restricted-list".into(),
            ..Default::default()
        },
        &person(PM),
    )
    .await
    .unwrap()
    .order
    .unwrap();
    assert!(holds(&stale, OrderCondition::AwaitingCheck));

    let answered = order_command(
        &w,
        RECORD_CHECK,
        "meridian.v1.RecordCheckRequest",
        &RecordCheckRequest {
            order_id: order.order_id.clone(),
            verdict: CheckVerdict::Pass as i32,
            rule: "restricted-list".into(),
            ..Default::default()
        },
        &person(PM),
    )
    .await
    .unwrap()
    .order
    .unwrap();
    assert_eq!(state(&answered), OrderState::Released);
    assert!(!holds(&answered, OrderCondition::AwaitingCheck));
    assert_eq!(read(&quantities(&answered).idle), "10");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_gate_refuses_in_its_order_allocation_block_approval_then_state() {
    let w = wired();
    waits(&w, ACC, false).await;
    let mut short = buy("10");
    short.allocation_instruction = Some(AllocationInstruction {
        lines: vec![AllocationLine {
            account_id: ACC.into(),
            quantity: d("8"),
            ..Default::default()
        }],
    });
    let order = propose(&w, &short).await.unwrap();
    let refused = release(&w, &order.order_id).await.unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::AllocationUnbalanced));

    let order = propose(&w, &buy("10")).await.unwrap();
    order_command(
        &w,
        RECORD_CHECK,
        "meridian.v1.RecordCheckRequest",
        &RecordCheckRequest {
            order_id: order.order_id.clone(),
            verdict: CheckVerdict::Block as i32,
            ..Default::default()
        },
        &person(PM),
    )
    .await
    .unwrap();
    assert_eq!(
        code(&release(&w, &order.order_id).await.unwrap_err()),
        Some(RefusalReason::CheckBlocks)
    );
    let no_reason = order_command(
        &w,
        DECIDE_APPROVAL,
        "meridian.v1.DecideApprovalRequest",
        &DecideApprovalRequest {
            order_id: order.order_id.clone(),
            decision: ApprovalDecision::Override as i32,
            ..Default::default()
        },
        &person(PM),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&no_reason), Some(RefusalReason::ReasonRequired));
    for decision in [ApprovalDecision::Override, ApprovalDecision::Require] {
        order_command(
            &w,
            DECIDE_APPROVAL,
            "meridian.v1.DecideApprovalRequest",
            &DecideApprovalRequest {
                order_id: order.order_id.clone(),
                decision: decision as i32,
                reason: "within the desk's limit".into(),
                ..Default::default()
            },
            &person(PM),
        )
        .await
        .unwrap();
    }
    assert_eq!(
        code(&release(&w, &order.order_id).await.unwrap_err()),
        Some(RefusalReason::ApprovalRequired)
    );
    order_command(
        &w,
        DECIDE_APPROVAL,
        "meridian.v1.DecideApprovalRequest",
        &DecideApprovalRequest {
            order_id: order.order_id.clone(),
            decision: ApprovalDecision::Approve as i32,
            reason: "approved".into(),
            ..Default::default()
        },
        &person(PM),
    )
    .await
    .unwrap();
    let out = release(&w, &order.order_id).await.unwrap().order.unwrap();
    assert_eq!(state(&out), OrderState::Released);
    assert_eq!(
        code(&release(&w, &order.order_id).await.unwrap_err()),
        Some(RefusalReason::OrderState)
    );
}

// ── The write scope (W4.9) ──────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_command_naming_its_record_by_id_outside_the_write_scope_is_denied_changing_nothing() {
    let w = wired();
    let order = propose(&w, &buy("10")).await.unwrap();
    let before = w.store.journal(ACC).unwrap().len();
    let outside = As {
        person: Some(TRADER),
        scope: Some(vec![OTHER]),
        allowed: true,
    };
    let refused = order_command(
        &w,
        RELEASE_ORDER,
        "meridian.v1.ReleaseOrderRequest",
        &ReleaseOrderRequest {
            order_id: order.order_id.clone(),
            idempotency_key: String::new(),
        },
        &outside,
    )
    .await
    .unwrap_err();
    assert_eq!(
        status_of(&refused),
        Some((meridian_bus::RefusalStatus::PermissionDenied, vec![]))
    );
    assert_eq!(code(&refused), None);
    assert_eq!(w.store.journal(ACC).unwrap().len(), before);
}

// ── W11.8 to W11.13: the path to a fill ─────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn an_order_is_placed_acknowledged_filled_in_two_and_moves_the_position() {
    let w = wired();
    let mut positions = w.bus.subscribe("platform.book.event.position-changed");
    let mut placed = w.bus.subscribe(PLACEMENT_UPDATED);
    let order = released(&w, "10").await;
    order_command(
        &w,
        SET_WORKING_CONDITION,
        "meridian.v1.SetWorkingConditionRequest",
        &SetWorkingConditionRequest {
            order_id: order.order_id.clone(),
            condition: OrderCondition::Claimed as i32,
            set: true,
            ..Default::default()
        },
        &person(TRADER),
    )
    .await
    .unwrap();
    // Claimed: another user's placement is refused.
    let refused = place(&w, &order.order_id, "10", &person(PM))
        .await
        .unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::Claimed));

    let placement = place(&w, &order.order_id, "10", &person(TRADER))
        .await
        .unwrap()
        .placement
        .unwrap();
    assert!(placement.placement_id.starts_with("PLC-"));
    assert_eq!(
        placement.status.unwrap().state,
        PlacementState::Queued as i32
    );
    let heard =
        PlacementUpdatedEvent::decode(&next(&mut placed).await.envelope.payload[..]).unwrap();
    assert_eq!(heard.placement.unwrap().connectivity_instance_id, ME);
    let working = order_now(&w, &order.order_id);
    assert_eq!(state(&working), OrderState::Working);
    assert_eq!(read(&quantities(&working).committed), "10");
    assert_eq!(read(&quantities(&working).idle), "0");

    let acknowledged = report(&w, &placement.placement_id, PlacementEvent::Acknowledged)
        .await
        .unwrap();
    assert_eq!(
        acknowledged.placement.unwrap().status.unwrap().state,
        PlacementState::Working as i32
    );
    let first = execute(&w, &fill(&placement.placement_id, "4", "ALP-55021-120:4"))
        .await
        .unwrap();
    let after_first = first.order.unwrap();
    let q = quantities(&after_first);
    assert_eq!(
        (
            read(&q.reported_filled),
            read(&q.committed),
            read(&q.leaves)
        ),
        ("4".into(), "6".into(), "6".into())
    );
    assert!(first.execution.unwrap().execution_id.starts_with("EXE-"));

    // The same fact again is recognised and applied once.
    let entries = w.store.journal(ACC).unwrap().len();
    execute(&w, &fill(&placement.placement_id, "4", "ALP-55021-120:4"))
        .await
        .unwrap();
    assert_eq!(w.store.journal(ACC).unwrap().len(), entries);

    let second = execute(&w, &fill(&placement.placement_id, "6", "ALP-55021-120:10"))
        .await
        .unwrap();
    let filled = second.order.unwrap();
    assert_eq!(state(&filled), OrderState::Filled);
    assert_eq!(read(&quantities(&filled).final_filled), "10");

    // Each fill moved the trade-date position: a lot at its price, the
    // security and the cash pending to the settlement date.
    let mut moved = Vec::new();
    while let Ok(Some(delivery)) =
        tokio::time::timeout(Duration::from_millis(300), positions.recv()).await
    {
        moved.push(PositionChangedEvent::decode(&delivery.envelope.payload[..]).unwrap());
    }
    let aapl = moved
        .iter()
        .rev()
        .find(|event| event.position.as_ref().unwrap().instrument_id == AAPL)
        .unwrap()
        .position
        .clone()
        .unwrap();
    assert_eq!(read(&aapl.trade_date_quantity), "10");
    assert_eq!(aapl.lots.len(), 2);
    assert_eq!(
        aapl.lots[0].terms.as_ref().unwrap().source,
        LotSource::Execution as i32
    );
    assert_eq!(aapl.pending[0].value_date, "2026-10-14");
    let cash = moved
        .iter()
        .rev()
        .find(|event| event.position.as_ref().unwrap().instrument_id == CHF)
        .unwrap()
        .position
        .clone()
        .unwrap();
    assert_eq!(read(&cash.trade_date_quantity), "-2274.10");
    let _ = LIST_POSITIONS;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_placement_at_a_connectivity_plugin_the_caller_may_not_route_through_is_refused_and_recorded(
) {
    let w = wired();
    let order = released(&w, "10").await;
    let mut ordered = w.bus.subscribe(ORDER_UPDATED);
    let before = w.store.journal(ACC).unwrap().len();
    let not_allowed = As {
        allowed: false,
        ..person(TRADER)
    };
    let refused = place(&w, &order.order_id, "10", &not_allowed)
        .await
        .unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::ConnectivityNotAllowed));
    let journal = w.store.journal(ACC).unwrap();
    assert_eq!(journal.len(), before + 1, "the refusal is recorded");
    let entry = journal.last().unwrap();
    assert_eq!(entry.meta.kind, "placement-refused");
    assert!(entry
        .meta
        .reason
        .contains("REFUSAL_REASON_CONNECTIVITY_NOT_ALLOWED"));
    let heard = OrderUpdatedEvent::decode(&next(&mut ordered).await.envelope.payload[..]).unwrap();
    assert_eq!(heard.entry.unwrap().kind, "placement-refused");
    // Nothing was placed.
    assert_eq!(state(&order_now(&w, &order.order_id)), OrderState::Released);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fill_with_no_settlement_date_is_malformed_and_a_bust_reverses_the_position() {
    let w = wired();
    let order = released(&w, "10").await;
    let placement = place(&w, &order.order_id, "10", &person(TRADER))
        .await
        .unwrap()
        .placement
        .unwrap();
    let mut undated = fill(&placement.placement_id, "10", "X:1");
    undated.computed_settlement_date = String::new();
    let refused = execute(&w, &undated).await.unwrap_err();
    assert_eq!(
        status_of(&refused),
        Some((
            meridian_bus::RefusalStatus::InvalidArgument,
            vec!["settlement_date".to_string()]
        ))
    );
    let filled = execute(&w, &fill(&placement.placement_id, "10", "X:1"))
        .await
        .unwrap();
    let execution = filled.execution.unwrap();
    assert_eq!(state(&filled.order.unwrap()), OrderState::Filled);
    let mut bust = fill(&placement.placement_id, "0", "X:1-bust");
    bust.supersedes = execution.execution_id.clone();
    let busted = execute(&w, &bust).await.unwrap();
    assert_eq!(state(&busted.order.unwrap()), OrderState::Working);
    let positions = w
        .store
        .positions(&crate::store::PositionsRead {
            scope: Scope::Everything,
            account_id: ACC.into(),
            since: None,
            business_date: String::new(),
            at: None,
            limit: 100,
            cursor: String::new(),
        })
        .unwrap();
    let aapl = positions
        .records
        .iter()
        .find(|position| position.instrument_id == AAPL)
        .unwrap();
    assert_eq!(read(&aapl.trade_date_quantity), "0");
}

// ── Child orders (the split link) ───────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_child_is_released_within_its_parent_commits_it_and_its_fills_are_the_parents() {
    let w = wired();
    let parent = released(&w, "10").await;
    let child_of = |parent: &str, quantity: &str| {
        let mut child = buy(quantity);
        child.link = Some(OrderLink {
            kind: OrderLinkKind::Split as i32,
            parent_order_id: parent.into(),
        });
        child
    };
    let child = send_from(
        &w.bus,
        PROPOSE_ORDER,
        "meridian.v1.ProposeOrderRequest",
        &child_of(&parent.order_id, "2"),
        &person(TRADER),
    )
    .await
    .map(|reply| OrderReply::decode(&reply[..]).unwrap().order.unwrap())
    .unwrap();
    assert_eq!(state(&child), OrderState::Released);
    let parent_now = order_now(&w, &parent.order_id);
    assert_eq!(read(&quantities(&parent_now).committed), "2");
    assert_eq!(read(&quantities(&parent_now).idle), "8");

    // On another side: refused naming the field.
    let mut sold = child_of(&parent.order_id, "2");
    sold.side = Side::Sell as i32;
    let refused = send_from(
        &w.bus,
        PROPOSE_ORDER,
        "meridian.v1.ProposeOrderRequest",
        &sold,
        &person(TRADER),
    )
    .await
    .unwrap_err();
    assert_eq!(status_of(&refused).unwrap().1, vec!["side"]);

    // A child of a child, its fill rolling up to the top.
    let grandchild = send_from(
        &w.bus,
        PROPOSE_ORDER,
        "meridian.v1.ProposeOrderRequest",
        &child_of(&child.order_id, "1"),
        &person(TRADER),
    )
    .await
    .map(|reply| OrderReply::decode(&reply[..]).unwrap().order.unwrap())
    .unwrap();
    let placement = place(&w, &grandchild.order_id, "1", &person(TRADER))
        .await
        .unwrap()
        .placement
        .unwrap();
    execute(&w, &fill(&placement.placement_id, "1", "G:1"))
        .await
        .unwrap();
    let top = order_now(&w, &parent.order_id);
    assert_eq!(read(&quantities(&top).reported_filled), "1");

    // A child under a proposed parent is refused for its state.
    let proposed = propose(&w, &buy("5")).await.unwrap();
    let refused = send_from(
        &w.bus,
        PROPOSE_ORDER,
        "meridian.v1.ProposeOrderRequest",
        &child_of(&proposed.order_id, "1"),
        &person(TRADER),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::OrderState));

    // The parent's cancel asks its live children to cancel; each ends when
    // nothing of it is live. The child works a slice at the venue.
    let slice = place(&w, &child.order_id, "1", &person(TRADER))
        .await
        .unwrap()
        .placement
        .unwrap();
    let cancelled = order_command(
        &w,
        REQUEST_ORDER_CHANGE,
        "meridian.v1.RequestOrderChangeRequest",
        &RequestOrderChangeRequest {
            order_id: parent.order_id.clone(),
            change: Some(OrderChange {
                kind: OrderChangeKind::Cancel as i32,
                ..Default::default()
            }),
            reason: "the model rebalanced away".into(),
            ..Default::default()
        },
        &person(TRADER),
    )
    .await
    .unwrap();
    assert!(cancelled.change_request_id.starts_with("CRQ-"));
    let parent_now = cancelled.order.unwrap();
    assert_eq!(state(&parent_now), OrderState::Working);
    assert!(holds(&parent_now, OrderCondition::ChangeRequested));
    let child_now = order_now(&w, &child.order_id);
    assert!(holds(&child_now, OrderCondition::ChangeRequested));
    report(&w, &slice.placement_id, PlacementEvent::Cancelled)
        .await
        .unwrap();
    assert_eq!(
        state(&order_now(&w, &child.order_id)),
        OrderState::Cancelled
    );
    assert_eq!(
        state(&order_now(&w, &parent.order_id)),
        OrderState::Cancelled
    );
}

// ── W11.3: changes ──────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_quantity_change_by_other_than_the_proposer_must_be_a_justified_alignment() {
    let w = wired();
    let order = released(&w, "10").await;
    let change = |quantity: &str, reason: &str| RequestOrderChangeRequest {
        order_id: order.order_id.clone(),
        change: Some(OrderChange {
            kind: OrderChangeKind::Quantity as i32,
            quantity: d(quantity),
            ..Default::default()
        }),
        reason: reason.into(),
        ..Default::default()
    };
    // Sent through another instance than the one that proposed it.
    let through_another = |request: RequestOrderChangeRequest| {
        let store: &dyn Store = w.store.as_ref();
        let ctx = crate::decide::Context {
            instance_id: "oms-2".into(),
            acting_for: TRADER.into(),
            received_at_ns: NOW,
            account_scope: Some([ACC.to_string()].into_iter().collect()),
            ..Default::default()
        };
        store
            .act(ACC, "", "", &[], &mut |book, head| {
                let (made, _) =
                    crate::order_decide::request_order_change(book, head, &ctx, &request)?;
                Ok(crate::service::decided(made, vec![]))
            })
            .err()
            .map(|failed| failed.on_the_bus())
            .unwrap_or_default()
    };
    let refused = through_another(change("12", "more"));
    assert_eq!(code(&refused), Some(RefusalReason::OrderState), "{refused}");
    let no_reason = through_another(change("0", ""));
    assert_eq!(code(&no_reason), Some(RefusalReason::ReasonRequired));
    // The proposer's own increase is applied.
    let increased = order_command(
        &w,
        REQUEST_ORDER_CHANGE,
        "meridian.v1.RequestOrderChangeRequest",
        &change("15", "the model wants more"),
        &person(PM),
    )
    .await
    .unwrap()
    .order
    .unwrap();
    assert_eq!(read(&quantities(&increased).ordered), "15");
}

// ── Late facts, overfills, unplaced facts ───────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_fill_after_the_cancel_is_a_late_fact_and_an_overfill_does_not_tie_out() {
    let w = wired();
    let order = released(&w, "10").await;
    let placement = place(&w, &order.order_id, "10", &person(TRADER))
        .await
        .unwrap()
        .placement
        .unwrap();
    order_command(
        &w,
        REQUEST_ORDER_CHANGE,
        "meridian.v1.RequestOrderChangeRequest",
        &RequestOrderChangeRequest {
            order_id: order.order_id.clone(),
            change: Some(OrderChange {
                kind: OrderChangeKind::Cancel as i32,
                ..Default::default()
            }),
            ..Default::default()
        },
        &person(TRADER),
    )
    .await
    .unwrap();
    let cancelled = report(&w, &placement.placement_id, PlacementEvent::Cancelled)
        .await
        .unwrap();
    assert_eq!(state(&cancelled.order.unwrap()), OrderState::Cancelled);
    let late = execute(&w, &fill(&placement.placement_id, "12", "L:1"))
        .await
        .unwrap();
    let order_now = late.order.unwrap();
    assert_eq!(state(&order_now), OrderState::Cancelled);
    assert!(holds(&order_now, OrderCondition::LateFact));
    assert!(holds(&order_now, OrderCondition::DoesNotTieOut));
    // Does not tie out is never acknowledged; the late fact is.
    let refused = order_command(
        &w,
        ACKNOWLEDGE_FLAG,
        "meridian.v1.AcknowledgeFlagRequest",
        &AcknowledgeFlagRequest {
            order_id: order.order_id.clone(),
            condition: OrderCondition::DoesNotTieOut as i32,
            ..Default::default()
        },
        &person(TRADER),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::OrderState));
    let acknowledged = order_command(
        &w,
        ACKNOWLEDGE_FLAG,
        "meridian.v1.AcknowledgeFlagRequest",
        &AcknowledgeFlagRequest {
            order_id: order.order_id.clone(),
            condition: OrderCondition::LateFact as i32,
            reason: "late fill confirmed with the broker".into(),
            ..Default::default()
        },
        &person(TRADER),
    )
    .await
    .unwrap()
    .order
    .unwrap();
    assert!(!holds(&acknowledged, OrderCondition::LateFact));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fact_the_book_cannot_place_is_kept_and_placed_when_a_later_record_claims_it() {
    let w = wired();
    let mut unplaced = w.bus.subscribe(UNPLACED_FACT_RECORDED);
    let order = released(&w, "10").await;
    let placement = place(&w, &order.order_id, "10", &person(TRADER))
        .await
        .unwrap()
        .placement
        .unwrap();
    let mut astray = fill("PLC-01JAF0R5N8M0000000000ZZZZ", "4", "U:1");
    astray.account_id = ACC.into();
    let kept = execute(&w, &astray).await.unwrap().unplaced.unwrap();
    assert_eq!(kept.reason, UnplacedReason::UnknownPlacement as i32);
    assert_eq!(kept.account_id, ACC);
    let heard =
        UnplacedFactRecordedEvent::decode(&next(&mut unplaced).await.envelope.payload[..]).unwrap();
    assert_eq!(heard.unplaced.unwrap().unplaced_id, kept.unplaced_id);
    // Sent again on its placement, the same venue execution claims it.
    execute(&w, &fill(&placement.placement_id, "4", "U:1"))
        .await
        .unwrap();
    let heard =
        UnplacedFactRecordedEvent::decode(&next(&mut unplaced).await.envelope.payload[..]).unwrap();
    assert!(heard.unplaced.unwrap().placed_by.is_some());

    // A venue account code no ems mapped: kept naming no account, read by no
    // plugin's scope.
    let mut unmapped = fill(&placement.placement_id, "1", "U:2");
    unmapped.account_id = String::new();
    let kept = execute(&w, &unmapped).await.unwrap().unplaced.unwrap();
    assert_eq!(kept.account_id, "");
    assert_eq!(kept.reason, UnplacedReason::UnmappedVenueAccount as i32);
    let scoped = w
        .store
        .orders(&OrdersRead {
            scope: Scope::Within([ACC.to_string()].into_iter().collect()),
            account_id: String::new(),
            since: None,
            limit: 100,
            cursor: String::new(),
        })
        .unwrap();
    assert!(scoped
        .unplaced
        .iter()
        .all(|fact| !fact.account_id.is_empty()));
}

// ── W11.15, W11.16: trading instructions ────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn trading_instructions_are_journalled_in_their_partitions_and_read_with_the_control_head() {
    let w = wired();
    let mut heard = w.bus.subscribe(TRADING_INSTRUCTION_CHANGED);
    let set = |target: TradingInstructionTarget, value: TradingInstructionValue, reason: &str| {
        // As the dashboard stamps it: here a plugin admin's names their
        // plugin, and every other is a deployment admin's.
        let setter_level = if target.instance_id.is_empty() {
            SetterLevel::DeploymentAdmin
        } else {
            SetterLevel::PluginAdmin
        } as i32;
        SetTradingInstructionRequest {
            target: Some(target),
            value: value as i32,
            reason: reason.into(),
            setter_level,
        }
    };
    let admin = As {
        person: Some(ADMIN),
        ..Default::default()
    };
    let stop = send_from(
        &w.bus,
        SET_TRADING_INSTRUCTION,
        "meridian.v1.SetTradingInstructionRequest",
        &set(
            TradingInstructionTarget {
                all_accounts: true,
                ..Default::default()
            },
            TradingInstructionValue::Stop,
            "market-wide halt",
        ),
        &admin,
    )
    .await
    .map(|reply| TradingInstructionReply::decode(&reply[..]).unwrap())
    .unwrap();
    assert_eq!(stop.journal.unwrap().partition, "control");
    let instruction = stop.instruction.unwrap();
    assert_eq!(
        instruction.setter_level,
        SetterLevel::DeploymentAdmin as i32
    );
    let event =
        TradingInstructionChangedEvent::decode(&next(&mut heard).await.envelope.payload[..])
            .unwrap();
    assert_eq!(
        event.instruction.unwrap().value,
        TradingInstructionValue::Stop as i32
    );

    let pause = send_from(
        &w.bus,
        SET_TRADING_INSTRUCTION,
        "meridian.v1.SetTradingInstructionRequest",
        &set(
            TradingInstructionTarget {
                account_id: ACC.into(),
                ..Default::default()
            },
            TradingInstructionValue::Pause,
            "pause the account",
        ),
        &admin,
    )
    .await
    .map(|reply| TradingInstructionReply::decode(&reply[..]).unwrap())
    .unwrap();
    assert_eq!(pause.journal.unwrap().partition, "P0");
    let plugin = send_from(
        &w.bus,
        SET_TRADING_INSTRUCTION,
        "meridian.v1.SetTradingInstructionRequest",
        &set(
            TradingInstructionTarget {
                instance_id: "snaptrade-1".into(),
                ..Default::default()
            },
            TradingInstructionValue::Stop,
            "stop the plugin",
        ),
        &admin,
    )
    .await
    .map(|reply| TradingInstructionReply::decode(&reply[..]).unwrap())
    .unwrap();
    assert_eq!(
        plugin.instruction.unwrap().setter_level,
        SetterLevel::PluginAdmin as i32
    );

    // A deployment admin's on the same plugin stands apart, at its level.
    let mut deployment_on_plugin = set(
        TradingInstructionTarget {
            instance_id: "snaptrade-1".into(),
            ..Default::default()
        },
        TradingInstructionValue::Pause,
        "pause the plugin, deployment-wide",
    );
    deployment_on_plugin.setter_level = SetterLevel::DeploymentAdmin as i32;
    let deployment = send_from(
        &w.bus,
        SET_TRADING_INSTRUCTION,
        "meridian.v1.SetTradingInstructionRequest",
        &deployment_on_plugin,
        &admin,
    )
    .await
    .map(|reply| TradingInstructionReply::decode(&reply[..]).unwrap())
    .unwrap();
    assert_eq!(
        deployment.instruction.unwrap().setter_level,
        SetterLevel::DeploymentAdmin as i32
    );
    // Never entered: one the dashboard did not stamp is malformed.
    let mut unstamped = deployment_on_plugin.clone();
    unstamped.setter_level = SetterLevel::Unspecified as i32;
    let refused = send_from(
        &w.bus,
        SET_TRADING_INSTRUCTION,
        "meridian.v1.SetTradingInstructionRequest",
        &unstamped,
        &admin,
    )
    .await
    .unwrap_err();
    assert_eq!(status_of(&refused).unwrap().1, vec!["setter_level"]);
    // A user group's persons are not resolved in v22: refused, naming it.
    let group = set(
        TradingInstructionTarget {
            user_group_id: "UG-desk-2".into(),
            ..Default::default()
        },
        TradingInstructionValue::Pause,
        "pause the desk",
    );
    let refused = send_from(
        &w.bus,
        SET_TRADING_INSTRUCTION,
        "meridian.v1.SetTradingInstructionRequest",
        &group,
        &admin,
    )
    .await
    .unwrap_err();
    assert_eq!(status_of(&refused).unwrap().1, vec!["target.user_group_id"]);
    // A plugin admin names their plugin; All accounts is not theirs.
    let mut overreach = set(
        TradingInstructionTarget {
            all_accounts: true,
            ..Default::default()
        },
        TradingInstructionValue::Active,
        "lift it",
    );
    overreach.setter_level = SetterLevel::PluginAdmin as i32;
    let refused = send_from(
        &w.bus,
        SET_TRADING_INSTRUCTION,
        "meridian.v1.SetTradingInstructionRequest",
        &overreach,
        &admin,
    )
    .await
    .unwrap_err();
    assert_eq!(status_of(&refused).unwrap().1, vec!["target"]);

    let no_reason = send_from(
        &w.bus,
        SET_TRADING_INSTRUCTION,
        "meridian.v1.SetTradingInstructionRequest",
        &set(
            TradingInstructionTarget {
                all_accounts: true,
                ..Default::default()
            },
            TradingInstructionValue::Active,
            "",
        ),
        &admin,
    )
    .await
    .unwrap_err();
    assert_eq!(code(&no_reason), Some(RefusalReason::ReasonRequired));

    let listed = send_from(
        &w.bus,
        LIST_TRADING_INSTRUCTIONS,
        "meridian.v1.ListTradingInstructionsRequest",
        &ListTradingInstructionsRequest::default(),
        &As {
            scope: Some(vec![]),
            ..Default::default()
        },
    )
    .await
    .map(|reply| ListTradingInstructionsReply::decode(&reply[..]).unwrap())
    .unwrap();
    assert_eq!(listed.instructions.len(), 4);
    let control_head = listed
        .as_of
        .unwrap()
        .partitions
        .iter()
        .find(|held| held.partition == "control")
        .unwrap()
        .sequence;
    assert_eq!(listed.control_sequence, control_head);
    assert!(control_head > 0);

    // A later order's entry records the control partition's sequence in force.
    let order = propose(&w, &buy("1")).await.unwrap();
    let entry = w
        .store
        .journal(ACC)
        .unwrap()
        .into_iter()
        .rev()
        .find(|entry| {
            entry.body.acts.iter().any(|act| {
                matches!(act, crate::orders::Act::Opened { order: opened, .. } if opened.order_id == order.order_id)
            })
        })
        .unwrap();
    assert_eq!(entry.meta.control_sequence, control_head);
}

// ── W11.13: expiry by the deployment's clock ────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_day_order_expires_at_the_end_of_its_day_its_placements_asked_to_cancel_first() {
    let w = wired();
    let idle = released(&w, "5").await;
    let worked = released(&w, "5").await;
    let placement = place(&w, &worked.order_id, "5", &person(TRADER))
        .await
        .unwrap()
        .placement
        .unwrap();
    // A day order never released, waiting for compliance (row i7), and a
    // good-till-cancelled one never released, which the clock never ends.
    waits(&w, ACC, true).await;
    let unreleased = propose(&w, &buy("2")).await.unwrap();
    release(&w, &unreleased.order_id).await.unwrap();
    let mut forever = buy("2");
    forever.time_in_force = TimeInForce::GoodTillCancelled as i32;
    let forever = propose(&w, &forever).await.unwrap();
    let store: Arc<dyn Store> = w.store.clone();
    assert_eq!(expire_due(&w.bus, &store).unwrap(), 0);
    w.clock.advance(12 * 3_600 * 1_000_000_000);
    let bus = w.bus.clone();
    let expiring = store.clone();
    let acted = tokio::task::spawn_blocking(move || expire_due(&bus, &expiring))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(acted, 3);
    assert_eq!(state(&order_now(&w, &idle.order_id)), OrderState::Expired);
    let lapsed = order_now(&w, &unreleased.order_id);
    assert_eq!(state(&lapsed), OrderState::Expired);
    assert!(!holds(&lapsed, OrderCondition::AwaitingCheck));
    assert_eq!(
        state(&order_now(&w, &forever.order_id)),
        OrderState::Proposed
    );
    let waiting = order_now(&w, &worked.order_id);
    assert_eq!(state(&waiting), OrderState::Working);
    assert!(holds(&waiting, OrderCondition::ChangeRequested));
    report(&w, &placement.placement_id, PlacementEvent::Cancelled)
        .await
        .unwrap();
    assert_eq!(state(&order_now(&w, &worked.order_id)), OrderState::Expired);
}

// ── The upgrade, and the journal's replay ───────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn the_journal_replays_to_the_same_order_records() {
    let w = wired();
    let order = released(&w, "10").await;
    let placement = place(&w, &order.order_id, "10", &person(TRADER))
        .await
        .unwrap()
        .placement
        .unwrap();
    execute(&w, &fill(&placement.placement_id, "3", "R:1"))
        .await
        .unwrap();
    let read_all = |store: &MemoryStore| {
        store
            .orders(&OrdersRead {
                scope: Scope::Everything,
                account_id: String::new(),
                since: None,
                limit: 500,
                cursor: String::new(),
            })
            .unwrap()
    };
    let before = read_all(&w.store);
    w.store.rebuild().unwrap();
    let after = read_all(&w.store);
    assert_eq!(before.orders, after.orders);
    assert_eq!(before.placements, after.placements);
    assert_eq!(before.executions, after.executions);
}

#[tokio::test(flavor = "multi_thread")]
async fn locates_record_their_answers_and_uses_and_an_expired_one_refuses_a_use() {
    let w = wired();
    let locate = send_from(
        &w.bus,
        RECORD_LOCATE,
        "meridian.v1.RecordLocateRequest",
        &RecordLocateRequest {
            account_id: ACC.into(),
            venue_account_code: "PB-ACCT-0042".into(),
            instrument_id: AAPL.into(),
            quantity_requested: d("500"),
            answer: Some(LocateAnswer {
                quantity_approved: d("500"),
                rate: d("0.0025"),
                expires_at_ns: NOW + 3_600 * 1_000_000_000,
                locate_id: "PB-LOC-77120".into(),
            }),
            ..Default::default()
        },
        &venue(),
    )
    .await
    .map(|reply| LocateReply::decode(&reply[..]).unwrap().locate.unwrap())
    .unwrap();
    assert_eq!(read(&locate.remaining), "500");
    let used = send_from(
        &w.bus,
        RECORD_LOCATE_USE,
        "meridian.v1.RecordLocateUseRequest",
        &RecordLocateUseRequest {
            locate_record_id: locate.locate_record_id.clone(),
            r#use: Some(LocateUse {
                kind: LocateUseKind::Reserve as i32,
                quantity: d("300"),
                order_id: String::new(),
            }),
            ..Default::default()
        },
        &person(TRADER),
    )
    .await
    .map(|reply| LocateReply::decode(&reply[..]).unwrap().locate.unwrap())
    .unwrap();
    assert_eq!(read(&used.remaining), "200");
    w.clock.advance(2 * 3_600 * 1_000_000_000);
    let refused = send_from(
        &w.bus,
        RECORD_LOCATE_USE,
        "meridian.v1.RecordLocateUseRequest",
        &RecordLocateUseRequest {
            locate_record_id: locate.locate_record_id.clone(),
            r#use: Some(LocateUse {
                kind: LocateUseKind::Consume as i32,
                quantity: d("100"),
                order_id: String::new(),
            }),
            ..Default::default()
        },
        &person(TRADER),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::OrderState));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sleeved_order_moves_its_sleeves_position_and_the_account_unsleeved_stays_apart() {
    let w = wired();
    waits(&w, ACC, false).await;
    let mut sleeved = buy("3");
    sleeved.sleeve = "momentum".into();
    let order = propose(&w, &sleeved).await.unwrap();
    release(&w, &order.order_id).await.unwrap();
    let placement = place(&w, &order.order_id, "3", &person(TRADER))
        .await
        .unwrap()
        .placement
        .unwrap();
    let filled = execute(&w, &fill(&placement.placement_id, "3", "S:1"))
        .await
        .unwrap();
    assert_eq!(filled.execution.unwrap().sleeve, "momentum");
    let positions = w
        .store
        .positions(&crate::store::PositionsRead {
            scope: Scope::Everything,
            account_id: ACC.into(),
            since: None,
            business_date: String::new(),
            at: None,
            limit: 100,
            cursor: String::new(),
        })
        .unwrap();
    let aapl: Vec<&BookPosition> = positions
        .records
        .iter()
        .filter(|position| position.instrument_id == AAPL)
        .collect();
    assert_eq!(aapl.len(), 1);
    assert_eq!(aapl[0].sleeve, "momentum");
    assert_eq!(aapl[0].lots[0].sleeve, "momentum");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_upgrade_records_once_that_every_account_held_waits_for_compliance() {
    let w = wired();
    // An account the book held before v22: an entry that stated nothing.
    let store: &dyn Store = w.store.as_ref();
    store
        .act("ACC-OLD", "", "", &[], &mut |book, head| {
            let ctx = crate::decide::Context {
                acting_for: ADMIN.into(),
                received_at_ns: NOW,
                ..Default::default()
            };
            let made = crate::decide::made(
                book,
                head,
                &ctx,
                crate::decide::Draft {
                    entry_id: ctx.mint(crate::ids::ENTRY),
                    kind: "attribute-set",
                    effective_date: "2026-10-01".into(),
                    reason: "before v22".into(),
                    break_ids: vec![],
                    idempotency_key: String::new(),
                    actor: ctx.actor(),
                    body: crate::journal::Body {
                        attributes: Some(AccountAttributes {
                            account_id: "ACC-OLD".into(),
                            base_currency_code: "USD".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                },
            )?;
            Ok(crate::service::decided(made, vec![]))
        })
        .unwrap();
    assert_eq!(backfill_waits(&w.bus, store).unwrap(), 1);
    assert_eq!(backfill_waits(&w.bus, store).unwrap(), 0);
    let journal = store.journal("ACC-OLD").unwrap();
    let last = journal.last().unwrap();
    assert_eq!(last.meta.kind, "waits-for-compliance-backfilled");
    assert!(last.body.attributes.as_ref().unwrap().waits_for_compliance);
}
