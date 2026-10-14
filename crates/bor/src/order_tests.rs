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
/// The base currency of the FX tests' pair, EUR/CHF (contract v23): a cash
/// instrument named by its ID alone, never by a code another test resolves.
const EUR: &str = "INS-01J8XQ4M7K00000000CASHEUR";
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
            EUR => Some(record(AssetClass::Cash, "EUR")),
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
            execution_state: 0,
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
    // A recorded order is a record being created.
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

// ── W11.2: record an order the book did not create ──────────────────────────

fn telephoned(venue_order_id: &str) -> RecordOrderRequest {
    RecordOrderRequest {
        account_id: ACC.into(),
        instrument_id: AAPL.into(),
        side: Side::Sell as i32,
        quantity: d("4"),
        order_type: OrderType::Market as i32,
        venue_order_id: venue_order_id.into(),
        origin: OrderOrigin::Telephoned as i32,
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_telephoned_order_is_the_oms_s_for_a_user_and_its_venue_order_id_recorded_once() {
    let w = wired();
    let record = |request: RecordOrderRequest, from: As| {
        let bus = w.bus.clone();
        async move {
            send_from(
                &bus,
                RECORD_ORDER,
                "meridian.v1.RecordOrderRequest",
                &request,
                &from,
            )
            .await
            .map(|reply| OrderReply::decode(&reply[..]).unwrap())
        }
    };
    let recorded = record(telephoned("ALP-55021-889"), person(TRADER))
        .await
        .unwrap();
    let order = recorded.order.unwrap();
    assert_eq!(state(&order), OrderState::Released);
    assert_eq!(order.origin, OrderOrigin::Telephoned as i32);
    assert_eq!(order.account_id, ACC);
    let actor = recorded.entry.unwrap().actor.unwrap();
    assert!(matches!(actor.kind, Some(actor::Kind::Person(ref who)) if who.subject == TRADER));

    // Sent for no one: a recorded order is a user's act, never a venue's
    // fact (ruled 2026-10-13), so nothing is kept unplaced.
    let nobody = record(telephoned("ALP-55021-890"), venue())
        .await
        .unwrap_err();
    assert_eq!(code(&nobody), Some(RefusalReason::ActorRequired));
    // Naming no account: refused naming the field.
    let mut unnamed = telephoned("ALP-55021-891");
    unnamed.account_id = String::new();
    let refused = record(unnamed, person(TRADER)).await.unwrap_err();
    assert_eq!(
        status_of(&refused).unwrap().1,
        vec!["account_id".to_string()]
    );
    // The same venue order ID again: refused, the first kept.
    let again = record(telephoned("ALP-55021-889"), person(TRADER))
        .await
        .unwrap_err();
    assert_eq!(code(&again), Some(RefusalReason::IdentifierInUse));
    // An instrument whose record is incomplete: refused naming what it lacks.
    let mut incomplete = telephoned("ALP-55021-892");
    incomplete.instrument_id = NOCCY.into();
    let lacking = record(incomplete, person(TRADER)).await.unwrap_err();
    assert_eq!(code(&lacking), Some(RefusalReason::Incomplete));
    assert!(w
        .store
        .orders(&OrdersRead {
            scope: Scope::Everything,
            account_id: crate::store::NO_ACCOUNT.into(),
            since: None,
            limit: 100,
            cursor: String::new(),
            execution_state: 0,
        })
        .unwrap()
        .unplaced
        .is_empty());
}

// ── W11.9: a placement's change requests ────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_placement_carries_its_own_change_requests_to_its_connectivity_plugin() {
    let w = wired();
    let order = released(&w, "10").await;
    // At another connectivity instance than this bus's, so the requests
    // below are the ems's and the oms's, never the venue's own.
    let placement = send_from(
        &w.bus,
        CREATE_PLACEMENT,
        "meridian.v1.CreatePlacementRequest",
        &CreatePlacementRequest {
            order_id: order.order_id.clone(),
            connectivity_instance_id: "fix-1".into(),
            venue_account_code: "SNAP-ACCT-9F2".into(),
            quantity: d("10"),
            limit_price: usd("227.50"),
            ..Default::default()
        },
        &person(TRADER),
    )
    .await
    .map(|reply| PlacementReply::decode(&reply[..]).unwrap())
    .unwrap()
    .placement
    .unwrap();
    assert!(placement.change_requests.is_empty());
    let mut placed = w.bus.subscribe(PLACEMENT_UPDATED);
    let replaced = send_from(
        &w.bus,
        REQUEST_PLACEMENT_CHANGE,
        "meridian.v1.RequestPlacementChangeRequest",
        &RequestPlacementChangeRequest {
            placement_id: placement.placement_id.clone(),
            change: Some(PlacementChange {
                kind: PlacementChangeKind::Replace as i32,
                quantity: d("8"),
                limit_price: usd("227.80"),
                time_in_force: TimeInForce::Day as i32,
            }),
            ..Default::default()
        },
        &person(TRADER),
    )
    .await
    .map(|reply| PlacementReply::decode(&reply[..]).unwrap())
    .unwrap()
    .placement
    .unwrap();
    let asked = |placement: &Placement| -> Vec<(i32, Option<String>, i32)> {
        placement
            .change_requests
            .iter()
            .map(|request| {
                let Some(change_request::Change::PlacementChange(change)) = &request.change else {
                    panic!("a placement's change request carries its PlacementChange");
                };
                (
                    change.kind,
                    change.limit_price.as_ref().map(|limit| read(&limit.amount)),
                    request.outcome,
                )
            })
            .collect()
    };
    let replace = (
        PlacementChangeKind::Replace as i32,
        Some("227.80".to_string()),
        ChangeRequestOutcome::Pending as i32,
    );
    assert_eq!(asked(&replaced), vec![replace.clone()]);
    assert_eq!(
        replaced.change_requests[0].placement_id,
        placement.placement_id
    );
    // Heard on the placement, as its connectivity plugin hears it.
    let heard =
        PlacementUpdatedEvent::decode(&next(&mut placed).await.envelope.payload[..]).unwrap();
    assert_eq!(asked(&heard.placement.unwrap()), vec![replace.clone()]);

    // An order cancel carries a cancel to the placement, beside the replace;
    // the order's own request stays on the order.
    let cancelled = order_command(
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
    .unwrap()
    .order
    .unwrap();
    assert!(cancelled
        .change_requests
        .iter()
        .all(|request| request.placement_id.is_empty()));
    let heard = loop {
        let event =
            PlacementUpdatedEvent::decode(&next(&mut placed).await.envelope.payload[..]).unwrap();
        let now = event.placement.unwrap();
        if now.change_requests.len() == 2 {
            break now;
        }
    };
    assert_eq!(
        asked(&heard),
        vec![
            replace,
            (
                PlacementChangeKind::Cancel as i32,
                None,
                ChangeRequestOutcome::Pending as i32
            )
        ]
    );
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
            execution_state: 0,
        })
        .unwrap();
    assert!(scoped
        .unplaced
        .iter()
        .all(|fact| !fact.account_id.is_empty()));
    // A core reader naming the book of no account reads those alone, as a
    // connectivity plugin's sidecar counts them; a scope naming it is refused.
    let none = |scope: Scope| OrdersRead {
        scope,
        account_id: crate::store::NO_ACCOUNT.into(),
        since: None,
        limit: 100,
        cursor: String::new(),
        execution_state: 0,
    };
    let alone = w.store.orders(&none(Scope::Everything)).unwrap();
    assert_eq!(
        alone
            .unplaced
            .iter()
            .map(|fact| fact.unplaced_id.as_str())
            .collect::<Vec<_>>(),
        vec![kept.unplaced_id.as_str()]
    );
    assert!(alone.orders.is_empty() && alone.placements.is_empty());
    assert!(w
        .store
        .orders(&none(Scope::Within(
            [ACC.to_string()].into_iter().collect()
        )))
        .is_err());
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
                execution_state: 0,
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

// ── Contract v23: venue-originated fills (W11.11, W11.23, W11.24) ───────────

/// A venue's fill on no order and no placement, its account stamped from the
/// `ems`'s map.
fn unmatched(quantity: &str, venue_id: &str, side: Side) -> ReportExecutionRequest {
    ReportExecutionRequest {
        placement_id: String::new(),
        instrument_id: AAPL.into(),
        side: side as i32,
        venue_order_id: "ALP-77310-004".into(),
        venue_client_order_id: "CL-77310-004".into(),
        ..fill("", quantity, venue_id)
    }
}

async fn accept(
    w: &Wired,
    execution_ids: &[&str],
    matched_order: Option<accept_execution_request::MatchedOrder>,
    from: &As,
) -> Result<AcceptExecutionReply, String> {
    send_from(
        &w.bus,
        ACCEPT_EXECUTION,
        "meridian.v1.AcceptExecutionRequest",
        &AcceptExecutionRequest {
            execution_ids: execution_ids.iter().map(|id| id.to_string()).collect(),
            matched_order,
            note: "Confirmed with the client by phone".into(),
        },
        from,
    )
    .await
    .map(|reply| AcceptExecutionReply::decode(&reply[..]).unwrap())
}

fn to_new_order(sleeve: &str) -> Option<accept_execution_request::MatchedOrder> {
    Some(accept_execution_request::MatchedOrder::NewOrder(
        NewOrderFromExecutions {
            sleeve: sleeve.into(),
            note: "Sold in the broker's app on Monday".into(),
        },
    ))
}

fn to_order(order_id: &str) -> Option<accept_execution_request::MatchedOrder> {
    Some(accept_execution_request::MatchedOrder::OrderId(
        order_id.into(),
    ))
}

async fn dont_know(
    w: &Wired,
    execution_ids: &[&str],
    reason: DkReason,
    text: &str,
    from: &As,
) -> Result<DontKnowExecutionReply, String> {
    send_from(
        &w.bus,
        DONT_KNOW_EXECUTION,
        "meridian.v1.DontKnowExecutionRequest",
        &DontKnowExecutionRequest {
            execution_ids: execution_ids.iter().map(|id| id.to_string()).collect(),
            reason: reason as i32,
            text: text.into(),
        },
        from,
    )
    .await
    .map(|reply| DontKnowExecutionReply::decode(&reply[..]).unwrap())
}

fn listed(w: &Wired, state: ExecutionState) -> crate::store::OrdersPage {
    w.store
        .orders(&OrdersRead {
            scope: Scope::Everything,
            account_id: String::new(),
            since: None,
            limit: 500,
            cursor: String::new(),
            execution_state: state as i32,
        })
        .unwrap()
}

fn listed_ids(w: &Wired, state: ExecutionState) -> Vec<String> {
    listed(w, state)
        .executions
        .into_iter()
        .map(|execution| execution.execution_id)
        .collect()
}

fn aapl_position(w: &Wired, business_date: &str, at: Option<crate::store::Mark>) -> String {
    let positions = w
        .store
        .positions(&crate::store::PositionsRead {
            scope: Scope::Everything,
            account_id: ACC.into(),
            since: None,
            business_date: business_date.into(),
            at,
            limit: 100,
            cursor: String::new(),
        })
        .unwrap();
    positions
        .records
        .iter()
        .find(|position| position.instrument_id == AAPL)
        .map(|position| read(&position.trade_date_quantity))
        .unwrap_or_else(|| "none".into())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fill_on_no_placement_is_unmatched_names_its_venue_order_and_moves_nothing() {
    let w = wired();
    let mut recorded = w.bus.subscribe(EXECUTION_RECORDED);
    let mut positions = w.bus.subscribe("platform.book.event.position-changed");
    // Each of the instrument, the side and the venue's order ID is required,
    // refused by name (ruling 9).
    for (field, mut request) in [
        ("instrument_id", unmatched("20", "V:0", Side::Sell)),
        ("side", unmatched("20", "V:0", Side::Sell)),
        ("venue_order_id", unmatched("20", "V:0", Side::Sell)),
    ] {
        match field {
            "instrument_id" => request.instrument_id = String::new(),
            "side" => request.side = Side::Unspecified as i32,
            _ => request.venue_order_id = String::new(),
        }
        let refused = execute(&w, &request).await.unwrap_err();
        assert_eq!(
            status_of(&refused),
            Some((
                meridian_bus::RefusalStatus::InvalidArgument,
                vec![field.to_string()]
            )),
            "{field}"
        );
    }
    let kept = execute(&w, &unmatched("20", "V:1", Side::Buy))
        .await
        .unwrap();
    let execution = kept.execution.unwrap();
    assert!(execution.execution_id.starts_with("EXE-"));
    assert_eq!(execution.state, ExecutionState::Unmatched as i32);
    assert_eq!(execution.venue_order_id, "ALP-77310-004");
    assert_eq!(execution.account_id, ACC);
    assert!(execution.order_id.is_empty() && execution.placement_id.is_empty());
    assert!(kept.order.is_none() && kept.placement.is_none());
    let heard = ExecutionRecordedEvent::decode(&next(&mut recorded).await.envelope.payload[..])
        .unwrap()
        .execution
        .unwrap();
    assert_eq!(heard.state, ExecutionState::Unmatched as i32);
    assert_eq!(
        heard.raw_record.unwrap().instance_id,
        ME,
        "the connectivity plugin that reported it hears it by its raw record"
    );
    // Positions don't move for an unmatched fill (ruling 5).
    assert!(
        tokio::time::timeout(Duration::from_millis(300), positions.recv())
            .await
            .is_err(),
        "an unmatched fill moves no position"
    );
    assert_eq!(aapl_position(&w, "", None), "none");
    // The venue account code and client order ID it came with are kept with
    // it in the journal, for the placement an accept creates.
    let journal = w.store.journal(ACC).unwrap();
    let kept_act = journal
        .last()
        .unwrap()
        .body
        .acts
        .iter()
        .find_map(|act| match act {
            crate::orders::Act::Executed {
                venue_account_code,
                venue_client_order_id,
                ..
            } => Some((venue_account_code.clone(), venue_client_order_id.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        kept_act,
        ("SNAP-ACCT-9F2".to_string(), "CL-77310-004".to_string())
    );
    // Listed and counted as unmatched, and the same fact again applied once.
    assert_eq!(
        listed_ids(&w, ExecutionState::Unmatched),
        vec![execution.execution_id.clone()]
    );
    let page = listed(&w, ExecutionState::Unmatched);
    assert!(page.orders.is_empty() && page.placements.is_empty() && page.unplaced.is_empty());
    let entries = w.store.journal(ACC).unwrap().len();
    execute(&w, &unmatched("20", "V:1", Side::Buy))
        .await
        .unwrap();
    assert_eq!(w.store.journal(ACC).unwrap().len(), entries);
    // An unmapped code is still kept unplaced (ruling 1).
    let mut unmapped = unmatched("5", "V:2", Side::Buy);
    unmapped.account_id = String::new();
    let kept = execute(&w, &unmapped).await.unwrap().unplaced.unwrap();
    assert_eq!(kept.reason, UnplacedReason::UnmappedVenueAccount as i32);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bust_closes_an_unmatched_fill_and_a_correction_is_answered_in_its_place() {
    let w = wired();
    let first = execute(&w, &unmatched("20", "V:1", Side::Buy))
        .await
        .unwrap()
        .execution
        .unwrap();
    let second = execute(&w, &unmatched("10", "V:2", Side::Buy))
        .await
        .unwrap()
        .execution
        .unwrap();
    // A bust (H): it names the fill and carries nothing else; the fill stays
    // unmatched, superseded, and neither it nor the bust is listed (e6).
    let bust = ReportExecutionRequest {
        instrument_id: String::new(),
        side: Side::Unspecified as i32,
        venue_order_id: String::new(),
        supersedes: first.execution_id.clone(),
        settlement_date: String::new(),
        computed_settlement_date: String::new(),
        ..unmatched("0", "V:1-bust", Side::Buy)
    };
    let busted = execute(&w, &bust).await.unwrap().execution.unwrap();
    assert_eq!(busted.state, ExecutionState::Unmatched as i32);
    assert_eq!(busted.instrument_id, AAPL, "taken from the fill it busts");
    // A correction (G) supersedes the second and is unmatched in its place
    // (e5).
    let correction = ReportExecutionRequest {
        supersedes: second.execution_id.clone(),
        ..unmatched("12", "V:2-correction", Side::Buy)
    };
    let corrected = execute(&w, &correction).await.unwrap().execution.unwrap();
    assert_eq!(corrected.state, ExecutionState::Unmatched as i32);
    assert_eq!(
        listed_ids(&w, ExecutionState::Unmatched),
        vec![corrected.execution_id.clone()],
        "a busted fill and its bust are left out; a corrected one is counted once, as its \
         correction (gap 2)"
    );
    // Neither the busted fill, its bust, nor the corrected one is answered.
    for id in [
        &first.execution_id,
        &busted.execution_id,
        &second.execution_id,
    ] {
        let refused = accept(&w, &[id], to_new_order(""), &person(TRADER))
            .await
            .unwrap_err();
        assert_eq!(code(&refused), Some(RefusalReason::ExecutionState), "{id}");
        let refused = dont_know(&w, &[id], DkReason::NoMatchingOrder, "", &person(TRADER))
            .await
            .unwrap_err();
        assert_eq!(code(&refused), Some(RefusalReason::ExecutionState), "{id}");
    }
    accept(
        &w,
        &[&corrected.execution_id],
        to_new_order(""),
        &person(TRADER),
    )
    .await
    .unwrap();
    assert!(listed_ids(&w, ExecutionState::Unmatched).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_accept_to_a_new_order_makes_it_filled_with_the_ems_s_placement_as_of_the_trade_date() {
    let w = wired();
    let mut ordered = w.bus.subscribe(ORDER_UPDATED);
    let mut placed = w.bus.subscribe(PLACEMENT_UPDATED);
    // Two prints of one venue order, traded on Friday 9 October.
    let prints: Vec<Execution> = {
        let mut kept = Vec::new();
        for (quantity, id) in [("20", "V:1"), ("10", "V:2")] {
            let mut print = unmatched(quantity, id, Side::Buy);
            print.trade_date = "2026-10-09".into();
            print.venue_id = "VEN-XNAS".into();
            kept.push(execute(&w, &print).await.unwrap().execution.unwrap());
        }
        kept
    };
    let ids: Vec<&str> = prints.iter().map(|e| e.execution_id.as_str()).collect();
    let before = w.store.heads().unwrap();
    // No user behind it: refused, nothing changed.
    let refused = accept(&w, &ids, to_new_order("core"), &venue())
        .await
        .unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::ActorRequired));
    // Neither arm of matched_order: refused naming it.
    let refused = accept(&w, &ids, None, &person(TRADER)).await.unwrap_err();
    assert_eq!(
        status_of(&refused),
        Some((
            meridian_bus::RefusalStatus::InvalidArgument,
            vec!["matched_order".to_string()]
        ))
    );
    let done = accept(&w, &ids, to_new_order("core"), &person(TRADER))
        .await
        .unwrap();
    let order = done.order.unwrap();
    assert_eq!(order.origin, OrderOrigin::AcceptedExecution as i32);
    assert_eq!(order.order_type, OrderType::Market as i32);
    assert_eq!(order.time_in_force, TimeInForce::Day as i32);
    assert_eq!(order.side, Side::Buy as i32);
    assert_eq!(order.sleeve, "core");
    assert_eq!(order.venue_order_id, "ALP-77310-004");
    assert_eq!(state(&order), OrderState::Filled);
    assert_eq!(read(&quantities(&order).ordered), "30");
    assert_eq!(read(&quantities(&order).reported_filled), "30");
    assert!(order.person_roles.as_ref().unwrap().entered_by.is_some());
    assert!(
        !holds(&order, OrderCondition::AwaitingCheck),
        "never gated (ruling 4)"
    );
    let placement = done.placement.unwrap();
    assert_eq!(
        placement.status.unwrap().state,
        PlacementState::Filled as i32
    );
    assert_eq!(placement.routed_at_ns, 0);
    assert_eq!(placement.ems_instance_id, ME);
    assert_eq!(placement.connectivity_instance_id, ME);
    assert_eq!(placement.venue_account_code, "SNAP-ACCT-9F2");
    assert_eq!(placement.venue_order_id, "ALP-77310-004");
    assert_eq!(placement.destination, "VEN-XNAS");
    assert_eq!(read(&placement.quantity), "30");
    for execution in &done.executions {
        assert_eq!(execution.state, ExecutionState::Matched as i32);
        assert_eq!(execution.order_id, order.order_id);
        assert_eq!(execution.placement_id, placement.placement_id);
        assert_eq!(execution.sleeve, "core");
    }
    // Compliance hears the order created after the trade, and the
    // connectivity plugin the placement it sends BN Accepted on.
    let heard = OrderUpdatedEvent::decode(&next(&mut ordered).await.envelope.payload[..]).unwrap();
    assert_eq!(
        heard.order.unwrap().origin,
        OrderOrigin::AcceptedExecution as i32
    );
    let heard =
        PlacementUpdatedEvent::decode(&next(&mut placed).await.envelope.payload[..]).unwrap();
    assert_eq!(heard.placement.unwrap().connectivity_instance_id, ME);
    // As of the trade date, by an entry recorded at the accept (ruling 13).
    let entry = w.store.journal(ACC).unwrap().last().unwrap().clone();
    assert_eq!(entry.meta.kind, "execution-accepted");
    assert_eq!(entry.meta.effective_date, "2026-10-09");
    assert_eq!(entry.meta.reason, "Confirmed with the client by phone");
    assert!(entry.body.acts.iter().any(|act| matches!(act,
        crate::orders::Act::Accepted { new_order_note, .. }
            if new_order_note == "Sold in the broker's app on Monday")));
    let sleeve_position = |business_date: &str, at| {
        w.store
            .positions(&crate::store::PositionsRead {
                scope: Scope::Everything,
                account_id: ACC.into(),
                since: None,
                business_date: business_date.into(),
                at,
                limit: 100,
                cursor: String::new(),
            })
            .unwrap()
            .records
            .into_iter()
            .find(|position| position.instrument_id == AAPL && position.sleeve == "core")
            .map(|position| read(&position.trade_date_quantity))
            .unwrap_or_else(|| "none".into())
    };
    assert_eq!(sleeve_position("2026-10-09", None), "30");
    assert_eq!(sleeve_position("2026-10-08", None), "none");
    assert_eq!(
        sleeve_position("2026-10-09", Some(before)),
        "none",
        "a point-in-time read from before the accept shows no position"
    );
    // Answered once, never again.
    let refused = accept(&w, &ids, to_new_order("core"), &person(TRADER))
        .await
        .unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::ExecutionState));
    // The journal replays to the same records.
    let replayed = crate::Book::replay(ACC, "P0", &w.store.journal(ACC).unwrap()).unwrap();
    assert_eq!(
        replayed.ledger.order_record(&order.order_id, true).unwrap(),
        order_now(&w, &order.order_id)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_accept_to_an_existing_order_is_held_to_its_state_quantity_side_and_account() {
    let w = wired();
    let order = released(&w, "10").await;
    let a = execute(&w, &unmatched("4", "V:1", Side::Buy))
        .await
        .unwrap()
        .execution
        .unwrap();
    let big = execute(&w, &unmatched("40", "V:2", Side::Buy))
        .await
        .unwrap()
        .execution
        .unwrap();
    let sold = execute(&w, &unmatched("3", "V:3", Side::Sell))
        .await
        .unwrap()
        .execution
        .unwrap();
    let mut elsewhere = unmatched("2", "V:4", Side::Buy);
    elsewhere.account_id = OTHER.into();
    let other = execute(&w, &elsewhere).await.unwrap().execution.unwrap();

    // Past the order's ordered quantity (ruling 10).
    let refused = accept(
        &w,
        &[&big.execution_id],
        to_order(&order.order_id),
        &person(TRADER),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::QuantityExceedsOrder));
    // A fill's side the order's opposite; fills of two sides; two accounts.
    for (ids, why) in [
        (vec![sold.execution_id.as_str()], "the wrong side"),
        (
            vec![a.execution_id.as_str(), sold.execution_id.as_str()],
            "mixed sides",
        ),
        (
            vec![a.execution_id.as_str(), other.execution_id.as_str()],
            "mixed accounts",
        ),
    ] {
        let refused = accept(&w, &ids, to_order(&order.order_id), &person(TRADER))
            .await
            .unwrap_err();
        assert_eq!(
            code(&refused),
            Some(RefusalReason::ExecutionsDiffer),
            "{why}"
        );
    }
    // Accepted to the working order: its placement created at filled.
    let done = accept(
        &w,
        &[&a.execution_id],
        to_order(&order.order_id),
        &person(TRADER),
    )
    .await
    .unwrap();
    let after = done.order.unwrap();
    assert_eq!(after.order_id, order.order_id);
    assert_eq!(state(&after), OrderState::Working);
    assert_eq!(read(&quantities(&after).reported_filled), "4");
    assert_eq!(
        done.placement.unwrap().status.unwrap().state,
        PlacementState::Filled as i32
    );
    assert_eq!(aapl_position(&w, "", None), "4");
    // An order ended takes no accept.
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
            reason: "done for today".into(),
            ..Default::default()
        },
        &person(TRADER),
    )
    .await
    .unwrap();
    let small = execute(&w, &unmatched("1", "V:5", Side::Buy))
        .await
        .unwrap()
        .execution
        .unwrap();
    let refused = accept(
        &w,
        &[&small.execution_id],
        to_order(&order.order_id),
        &person(TRADER),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::OrderState));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dk_closes_a_fill_moving_nothing_and_a_later_fill_of_the_venue_order_waits_again() {
    let w = wired();
    let mut recorded = w.bus.subscribe(EXECUTION_RECORDED);
    let fill = execute(&w, &unmatched("20", "V:1", Side::Buy))
        .await
        .unwrap()
        .execution
        .unwrap();
    next(&mut recorded).await;
    let id = fill.execution_id.as_str();
    // Other without text: refused naming text, as FIX requires Text (58).
    let refused = dont_know(&w, &[id], DkReason::Other, " ", &person(TRADER))
        .await
        .unwrap_err();
    assert_eq!(
        status_of(&refused),
        Some((
            meridian_bus::RefusalStatus::InvalidArgument,
            vec!["text".to_string()]
        ))
    );
    let refused = dont_know(&w, &[id], DkReason::UnknownInstrument, "", &venue())
        .await
        .unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::ActorRequired));
    let done = dont_know(&w, &[id], DkReason::Other, "Not our trade", &person(TRADER))
        .await
        .unwrap();
    let answered = &done.executions[0];
    assert_eq!(answered.state, ExecutionState::DontKnow as i32);
    assert_eq!(answered.dk_reason, DkReason::Other as i32);
    assert_eq!(answered.dk_text, "Not our trade");
    // The connectivity plugin that reported it hears it (ruling 15).
    let heard = ExecutionRecordedEvent::decode(&next(&mut recorded).await.envelope.payload[..])
        .unwrap()
        .execution
        .unwrap();
    assert_eq!(heard.state, ExecutionState::DontKnow as i32);
    assert_eq!(heard.raw_record.unwrap().instance_id, ME);
    assert_eq!(aapl_position(&w, "", None), "none");
    assert_eq!(
        listed_ids(&w, ExecutionState::DontKnow),
        vec![id.to_string()]
    );
    // Answered once, never again.
    let refused = dont_know(&w, &[id], DkReason::Other, "again", &person(TRADER))
        .await
        .unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::ExecutionState));
    let refused = accept(&w, &[id], to_new_order(""), &person(TRADER))
        .await
        .unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::ExecutionState));
    // A later fill for the same venue order arrives unmatched on its own (e7).
    let later = execute(&w, &unmatched("5", "V:2", Side::Buy))
        .await
        .unwrap()
        .execution
        .unwrap();
    assert_eq!(later.state, ExecutionState::Unmatched as i32);
    assert_eq!(later.venue_order_id, fill.venue_order_id);
    assert_eq!(
        listed_ids(&w, ExecutionState::Unmatched),
        vec![later.execution_id.clone()]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bust_after_an_accept_reverses_its_own_fill_alone() {
    let w = wired();
    let mut kept = Vec::new();
    for (quantity, id) in [("20", "V:1"), ("10", "V:2")] {
        kept.push(
            execute(&w, &unmatched(quantity, id, Side::Buy))
                .await
                .unwrap()
                .execution
                .unwrap(),
        );
    }
    let ids: Vec<&str> = kept.iter().map(|e| e.execution_id.as_str()).collect();
    let done = accept(&w, &ids, to_new_order(""), &person(TRADER))
        .await
        .unwrap();
    let placement = done.placement.unwrap();
    assert_eq!(aapl_position(&w, "", None), "30");
    // The venue busts the second print, on the placement the accept created.
    let mut bust = fill(&placement.placement_id, "0", "V:2-bust");
    bust.supersedes = kept[1].execution_id.clone();
    bust.computed_settlement_date = String::new();
    let busted = execute(&w, &bust).await.unwrap();
    assert_eq!(aapl_position(&w, "", None), "20");
    let order = busted.order.unwrap();
    assert_eq!(read(&quantities(&order).reported_filled), "20");
}

// ── Contract v23: the accept's refusals as built, approved 2026-10-13 ───────

#[tokio::test(flavor = "multi_thread")]
async fn an_accept_s_fills_share_a_trade_date_and_a_claimed_order_is_the_claimant_s() {
    let w = wired();
    let order = released(&w, "10").await;
    let monday = execute(&w, &unmatched("2", "V:1", Side::Buy))
        .await
        .unwrap()
        .execution
        .unwrap();
    // The venue's client order ID is copied onto the execution (FIX 11, back
    // on BN and Q).
    assert_eq!(monday.venue_client_order_id, "CL-77310-004");
    let mut late = unmatched("3", "V:2", Side::Buy);
    late.trade_date = "2026-10-14".into();
    late.computed_settlement_date = "2026-10-15".into();
    let tuesday = execute(&w, &late).await.unwrap().execution.unwrap();
    // Two trade dates in one accept: one placement, one trade date.
    let refused = accept(
        &w,
        &[&monday.execution_id, &tuesday.execution_id],
        to_new_order(""),
        &person(TRADER),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::ExecutionsDiffer));
    // An execution named twice is malformed, naming the second.
    let refused = accept(
        &w,
        &[&monday.execution_id, &monday.execution_id],
        to_new_order(""),
        &person(TRADER),
    )
    .await
    .unwrap_err();
    assert_eq!(
        status_of(&refused),
        Some((
            meridian_bus::RefusalStatus::InvalidArgument,
            vec!["execution_ids[1]".to_string()]
        ))
    );
    // Claimed by the trader: another user's accept onto it is refused.
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
    let refused = accept(
        &w,
        &[&monday.execution_id],
        to_order(&order.order_id),
        &person(PM),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::Claimed));
    assert_eq!(
        listed_ids(&w, ExecutionState::Unmatched).len(),
        2,
        "a refused accept changes nothing"
    );
    // The claimant's is accepted, the client order ID kept.
    let done = accept(
        &w,
        &[&monday.execution_id],
        to_order(&order.order_id),
        &person(TRADER),
    )
    .await
    .unwrap();
    assert_eq!(done.executions[0].venue_client_order_id, "CL-77310-004");
    assert_eq!(done.executions[0].state, ExecutionState::Matched as i32);
}

// ── Contract v23: financing and commission (the OANDA batch's item 2) ───────

#[tokio::test(flavor = "multi_thread")]
async fn financing_and_commission_are_fee_kinds_each_its_own_cash_line() {
    let w = wired();
    let order = released(&w, "10").await;
    let placement = place(&w, &order.order_id, "10", &person(TRADER))
        .await
        .unwrap()
        .placement
        .unwrap();
    let mut filled = fill(&placement.placement_id, "10", "V:1");
    filled.fees = vec![Fee {
        kind: FeeKind::Commission as i32,
        amount: usd("0.35"),
        ..Default::default()
    }];
    let reply = execute(&w, &filled).await.unwrap();
    assert_eq!(
        reply.execution.unwrap().fees[0].kind,
        FeeKind::Commission as i32
    );
    let charged = send_from(
        &w.bus,
        REPORT_CHARGE,
        "meridian.v1.ReportChargeRequest",
        &ReportChargeRequest {
            placement_id: placement.placement_id.clone(),
            fee: Some(Fee {
                kind: FeeKind::Financing as i32,
                amount: usd("1.37"),
                ..Default::default()
            }),
            trade_date: "2026-10-14".into(),
            settlement_date: "2026-10-14".into(),
            raw_record: Some(meridian_pb::v1::RawRecordRef {
                instance_id: ME.into(),
                key: "transactions/6311:trades/6290".into(),
            }),
        },
        &venue(),
    )
    .await
    .map(|reply| FactReply::decode(&reply[..]).unwrap())
    .unwrap();
    let charge = charged.charge.unwrap();
    assert_eq!(charge.fee.unwrap().kind, FeeKind::Financing as i32);
    // The cash: the notional and the commission pending to the fill's date,
    // the financing to its own.
    let cash = chf_position(&w);
    let pending: Vec<(String, String)> = cash
        .pending
        .iter()
        .map(|line| (line.value_date.clone(), read(&line.quantity)))
        .collect();
    assert_eq!(
        pending,
        vec![("2026-10-14".to_string(), "-2275.82".to_string())]
    );
}

// ── Contract v23: an FX order's quote currency (W11.1) ──────────────────────

fn fx_market(quote: &str) -> ProposeOrderRequest {
    ProposeOrderRequest {
        account_id: ACC.into(),
        instrument_id: EUR.into(),
        side: Side::Buy as i32,
        quantity: d("10000"),
        order_type: OrderType::Market as i32,
        dealt_currency: EUR.into(),
        quote_currency: quote.into(),
        time_in_force: TimeInForce::Day as i32,
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_fx_order_names_its_quote_currency_and_a_limit_or_stop_is_in_it() {
    let w = wired();
    // A market order names its pair through its instrument and the field.
    let order = propose(&w, &fx_market(CHF)).await.unwrap();
    assert_eq!(order.quote_currency, CHF);
    assert_eq!(order.instrument_id, EUR);
    // Its own instrument as the quote currency: a pair is two currencies.
    let refused = propose(&w, &fx_market(EUR)).await.unwrap_err();
    assert_eq!(
        status_of(&refused),
        Some((
            meridian_bus::RefusalStatus::InvalidArgument,
            vec!["quote_currency".to_string()]
        ))
    );
    // A limit, or a stop, priced in another currency.
    let other = "INS-01J8XQ4M7K00000000CASHGBP";
    let priced_in = |instrument: &str| {
        Some(Money {
            amount: d("0.8600"),
            currency_code: String::new(),
            instrument_id: instrument.into(),
        })
    };
    let mut limit = fx_market(other);
    limit.order_type = OrderType::Limit as i32;
    limit.limit_price = usd("0.93");
    let refused = propose(&w, &limit).await.unwrap_err();
    assert_eq!(status_of(&refused).unwrap().1, vec!["limit_price"]);
    let mut stop = fx_market(other);
    stop.order_type = OrderType::Stop as i32;
    stop.stop_price = usd("0.93");
    let refused = propose(&w, &stop).await.unwrap_err();
    assert_eq!(status_of(&refused).unwrap().1, vec!["stop_price"]);
    // In the quote currency, by code or by instrument: proposed.
    let mut by_code = fx_market(CHF);
    by_code.order_type = OrderType::Limit as i32;
    by_code.limit_price = usd("0.93");
    assert_eq!(propose(&w, &by_code).await.unwrap().quote_currency, CHF);
    limit.limit_price = priced_in(other);
    assert_eq!(propose(&w, &limit).await.unwrap().quote_currency, other);
    // No quote currency: checked as before.
    assert!(propose(&w, &buy("10")).await.is_ok());
}

// ── Contract v23: a value-date roll (W11.25) ────────────────────────────────

fn position(w: &Wired, instrument: &str, side: HoldingSide) -> Option<BookPosition> {
    w.store
        .positions(&crate::store::PositionsRead {
            scope: Scope::Everything,
            account_id: ACC.into(),
            since: None,
            business_date: String::new(),
            at: None,
            limit: 100,
            cursor: String::new(),
        })
        .unwrap()
        .records
        .into_iter()
        .find(|held| held.instrument_id == instrument && held.side == side as i32)
}

fn chf_position(w: &Wired) -> BookPosition {
    position(w, CHF, HoldingSide::Long).unwrap()
}

fn pending_of(held: &BookPosition) -> Vec<(String, String)> {
    held.pending
        .iter()
        .map(|line| (line.value_date.clone(), read(&line.quantity)))
        .collect()
}

fn roll(from: &str, to: &str, key: &str) -> ReportValueDateRollRequest {
    ReportValueDateRollRequest {
        account_id: ACC.into(),
        venue_account_code: "SNAP-ACCT-9F2".into(),
        pair: vec![EUR.into(), CHF.into()],
        from_value_date: from.into(),
        to_value_date: to.into(),
        value_date_basis: Some(meridian_pb::v1::DateBasis {
            rule: Some(meridian_pb::v1::DateRuleRef {
                kind: meridian_pb::v1::DateRuleKind::Spot as i32,
                subject: "EUR/CHF".into(),
                version: 1,
            }),
            ..Default::default()
        }),
        rolled_at_ns: NOW + 27_000_000_000_000,
        charge_ids: vec![],
        raw_record: Some(meridian_pb::v1::RawRecordRef {
            instance_id: ME.into(),
            key: format!("transactions/{key}"),
        }),
        idempotency_key: format!("roll:{key}"),
    }
}

async fn report_roll(w: &Wired, request: &ReportValueDateRollRequest) -> Result<FactReply, String> {
    send_from(
        &w.bus,
        REPORT_VALUE_DATE_ROLL,
        "meridian.v1.ReportValueDateRollRequest",
        request,
        &venue(),
    )
    .await
    .map(|reply| FactReply::decode(&reply[..]).unwrap())
}

/// An FX buy of EUR against CHF, placed at this instance on the code and
/// filled, pending to `value_date`: its placement and its execution.
async fn fx_filled(
    w: &Wired,
    quantity: &str,
    venue_id: &str,
    value_date: &str,
) -> (String, String) {
    waits(w, ACC, false).await;
    let mut request = fx_market(CHF);
    request.quantity = d(quantity);
    let order = propose(w, &request).await.unwrap();
    release(w, &order.order_id).await.unwrap();
    let placement = place(w, &order.order_id, quantity, &person(TRADER))
        .await
        .unwrap()
        .placement
        .unwrap();
    let mut filled = fill(&placement.placement_id, quantity, venue_id);
    filled.price = usd("0.93");
    filled.computed_settlement_date = value_date.into();
    let execution = execute(w, &filled).await.unwrap().execution.unwrap();
    (placement.placement_id, execution.execution_id)
}

/// A position's pending lines that hold something.
fn live_pending(held: &BookPosition) -> Vec<(String, String)> {
    held.pending
        .iter()
        .filter(|line| {
            line.quantity
                .as_ref()
                .is_some_and(|quantity| !Exact::from_wire(quantity).unwrap().is_zero())
        })
        .map(|line| (line.value_date.clone(), read(&line.quantity)))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_roll_moves_the_pending_lines_in_the_pair_to_the_next_value_date_and_nothing_else() {
    let w = wired();
    fx_filled(&w, "10000", "V:1", "2026-10-15").await;
    let eur_before = position(&w, EUR, HoldingSide::Long).unwrap();
    assert_eq!(
        pending_of(&eur_before),
        vec![("2026-10-15".into(), "10000".into())]
    );
    assert_eq!(
        pending_of(&chf_position(&w)),
        vec![("2026-10-15".into(), "-9300.00".into())]
    );

    // Malformed, naming the field, changing nothing.
    for (field, change) in [
        (
            "to_value_date",
            Box::new(|r: &mut ReportValueDateRollRequest| {
                r.to_value_date.clear();
                r.value_date_basis = None;
            }) as Box<dyn Fn(&mut ReportValueDateRollRequest)>,
        ),
        (
            "from_value_date",
            Box::new(|r: &mut ReportValueDateRollRequest| r.from_value_date.clear()),
        ),
        (
            "to_value_date",
            Box::new(|r: &mut ReportValueDateRollRequest| r.to_value_date = "2026-10-15".into()),
        ),
        (
            "pair",
            Box::new(|r: &mut ReportValueDateRollRequest| r.pair.truncate(1)),
        ),
        (
            "value_date_basis",
            Box::new(|r: &mut ReportValueDateRollRequest| r.value_date_basis = None),
        ),
        (
            "rolled_at_ns",
            Box::new(|r: &mut ReportValueDateRollRequest| r.rolled_at_ns = 0),
        ),
        (
            "raw_record",
            Box::new(|r: &mut ReportValueDateRollRequest| r.raw_record = None),
        ),
    ] {
        let mut request = roll("2026-10-15", "2026-10-16", "6311");
        change(&mut request);
        let refused = report_roll(&w, &request).await.unwrap_err();
        assert_eq!(
            status_of(&refused),
            Some((
                meridian_bus::RefusalStatus::InvalidArgument,
                vec![field.to_string()]
            )),
            "{field}"
        );
    }

    // Tuesday's roll: one entry, the pending lines moved, a PositionChanged
    // for each position.
    let mut moved = w.bus.subscribe("platform.book.event.position-changed");
    let before = w.store.journal(ACC).unwrap().len();
    let reply = report_roll(&w, &roll("2026-10-15", "2026-10-16", "6311"))
        .await
        .unwrap();
    let entry = reply.entry.unwrap();
    assert_eq!(entry.kind, "value-date-rolled");
    assert!(reply.journal.is_some());
    assert!(reply.execution.is_none() && reply.order.is_none() && reply.placement.is_none());
    let journal = w.store.journal(ACC).unwrap();
    assert_eq!(journal.len(), before + 1);
    assert_eq!(journal.last().unwrap().body.lines.len(), 4);
    let mut heard = Vec::new();
    for _ in 0..2 {
        let event =
            PositionChangedEvent::decode(&next(&mut moved).await.envelope.payload[..]).unwrap();
        heard.push(event.position.unwrap().instrument_id);
    }
    heard.sort();
    assert_eq!(heard, vec![CHF.to_string(), EUR.to_string()]);
    let eur = position(&w, EUR, HoldingSide::Long).unwrap();
    assert_eq!(
        pending_of(&eur),
        vec![("2026-10-16".into(), "10000".into())]
    );
    assert_eq!(
        pending_of(&chf_position(&w)),
        vec![("2026-10-16".into(), "-9300.00".into())]
    );
    // The trade-date quantity and the lots are untouched.
    assert_eq!(read(&eur.trade_date_quantity), "10000");
    assert_eq!(eur.lots, eur_before.lots);

    // The same report again is applied once.
    report_roll(&w, &roll("2026-10-15", "2026-10-16", "6311"))
        .await
        .unwrap();
    assert_eq!(w.store.journal(ACC).unwrap().len(), before + 1);

    // Wednesday's, over the weekend: moved again, the earlier roll counted.
    report_roll(&w, &roll("2026-10-16", "2026-10-19", "6340"))
        .await
        .unwrap();
    assert_eq!(
        pending_of(&position(&w, EUR, HoldingSide::Long).unwrap()),
        vec![("2026-10-19".into(), "10000".into())]
    );

    // A fill booked after the venue rolled its trade pends to its recorded
    // date; the catch-up roll moves it, and only it, to the current one.
    fx_filled(&w, "5000", "V:2", "2026-10-15").await;
    assert_eq!(
        pending_of(&position(&w, EUR, HoldingSide::Long).unwrap()),
        vec![
            ("2026-10-15".into(), "5000".into()),
            ("2026-10-19".into(), "10000".into())
        ]
    );
    report_roll(&w, &roll("2026-10-15", "2026-10-19", "6340:catch-up:6290"))
        .await
        .unwrap();
    let eur = position(&w, EUR, HoldingSide::Long).unwrap();
    assert_eq!(
        pending_of(&eur),
        vec![("2026-10-19".into(), "15000".into())]
    );
    assert_eq!(
        pending_of(&chf_position(&w)),
        vec![("2026-10-19".into(), "-13950.00".into())]
    );

    // Kept unplaced, never refused nor dropped (approved 2026-10-13): a roll
    // meeting no pending line, in the account, heard by operations; one on
    // a code no ems mapped, naming no account. Neither moves a line.
    let mut unplaced = w.bus.subscribe(UNPLACED_FACT_RECORDED);
    let pending_before = (
        pending_of(&position(&w, EUR, HoldingSide::Long).unwrap()),
        pending_of(&chf_position(&w)),
    );
    let before = w.store.journal(ACC).unwrap().len();
    let kept = report_roll(&w, &roll("2026-10-15", "2026-10-16", "6400"))
        .await
        .unwrap()
        .unplaced
        .unwrap();
    assert_eq!(kept.reason, UnplacedReason::NothingPending as i32);
    assert_eq!(kept.fact_kind, "value-date-roll");
    assert_eq!(kept.account_id, ACC);
    assert!(matches!(
        &kept.fact,
        Some(unplaced_fact::Fact::ValueDateRoll(held)) if held.idempotency_key == "roll:6400"
    ));
    let heard =
        UnplacedFactRecordedEvent::decode(&next(&mut unplaced).await.envelope.payload[..]).unwrap();
    assert_eq!(heard.unplaced.unwrap().unplaced_id, kept.unplaced_id);
    assert_eq!(w.store.journal(ACC).unwrap().len(), before + 1);
    // Sent again, kept once.
    report_roll(&w, &roll("2026-10-15", "2026-10-16", "6400"))
        .await
        .unwrap();
    assert_eq!(w.store.journal(ACC).unwrap().len(), before + 1);
    let mut unmapped = roll("2026-10-19", "2026-10-20", "6401");
    unmapped.account_id = String::new();
    unmapped.venue_account_code = "101-001-UNMAPPED".into();
    let kept = report_roll(&w, &unmapped).await.unwrap().unplaced.unwrap();
    assert_eq!(kept.reason, UnplacedReason::UnmappedVenueAccount as i32);
    assert_eq!(kept.fact_kind, "value-date-roll");
    assert_eq!(kept.account_id, "");
    assert_eq!(kept.venue_account_code, "101-001-UNMAPPED");
    assert_eq!(w.store.journal(ACC).unwrap().len(), before + 1);
    assert_eq!(
        pending_before,
        (
            pending_of(&position(&w, EUR, HoldingSide::Long).unwrap()),
            pending_of(&chf_position(&w)),
        )
    );
    // A pair that is not two cash instruments is malformed, on a mapped code
    // or an unmapped one.
    for account in [ACC, ""] {
        let mut equity = roll("2026-10-19", "2026-10-20", "6402");
        equity.account_id = account.into();
        equity.pair = vec![EUR.into(), AAPL.into()];
        let refused = report_roll(&w, &equity).await.unwrap_err();
        assert_eq!(
            status_of(&refused),
            Some((
                meridian_bus::RefusalStatus::InvalidArgument,
                vec!["pair".to_string()]
            )),
            "{account:?}"
        );
    }

    // The journal replays to the same positions.
    let before = (position(&w, EUR, HoldingSide::Long), chf_position(&w));
    w.store.rebuild().unwrap();
    assert_eq!(
        before,
        (position(&w, EUR, HoldingSide::Long), chf_position(&w))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bust_after_a_roll_reverses_the_fill_where_the_rolls_took_it() {
    let w = wired();
    let (placement_id, execution_id) = fx_filled(&w, "10000", "V:1", "2026-10-15").await;
    report_roll(&w, &roll("2026-10-15", "2026-10-16", "6311"))
        .await
        .unwrap();
    report_roll(&w, &roll("2026-10-16", "2026-10-19", "6340"))
        .await
        .unwrap();
    assert_eq!(
        live_pending(&position(&w, EUR, HoldingSide::Long).unwrap()),
        vec![("2026-10-19".into(), "10000".into())]
    );
    // The bust takes the fill's pending lines off at 2026-10-19, where the
    // rolls took them, never at the 2026-10-15 it was recorded with
    // (W11.25; approved 2026-10-13).
    let mut bust = fill(&placement_id, "0", "V:1-bust");
    bust.supersedes = execution_id;
    bust.price = usd("0.93");
    bust.computed_settlement_date = "2026-10-15".into();
    execute(&w, &bust).await.unwrap();
    let eur = position(&w, EUR, HoldingSide::Long).unwrap();
    assert_eq!(live_pending(&eur), Vec::<(String, String)>::new());
    assert_eq!(
        live_pending(&chf_position(&w)),
        Vec::<(String, String)>::new()
    );
    assert_eq!(read(&eur.trade_date_quantity), "0");
    // The journal replays to the same positions.
    let before = (position(&w, EUR, HoldingSide::Long), chf_position(&w));
    w.store.rebuild().unwrap();
    assert_eq!(
        before,
        (position(&w, EUR, HoldingSide::Long), chf_position(&w))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_fx_orders_amended_limit_stays_in_its_quote_currency() {
    let w = wired();
    let mut request = fx_market(CHF);
    request.order_type = OrderType::Limit as i32;
    request.limit_price = usd("0.93");
    let order = propose(&w, &request).await.unwrap();
    let amend = |order_id: &str, limit: Option<Money>| RequestOrderChangeRequest {
        order_id: order_id.into(),
        change: Some(OrderChange {
            kind: OrderChangeKind::Amend as i32,
            limit_price: limit,
            ..Default::default()
        }),
        ..Default::default()
    };
    let gbp = Some(Money {
        amount: d("0.8600"),
        currency_code: String::new(),
        instrument_id: "INS-01J8XQ4M7K00000000CASHGBP".into(),
    });
    // Priced in another currency: malformed, naming change.limit_price
    // (W11.3; approved 2026-10-13).
    let refused = order_command(
        &w,
        REQUEST_ORDER_CHANGE,
        "meridian.v1.RequestOrderChangeRequest",
        &amend(&order.order_id, gbp.clone()),
        &person(TRADER),
    )
    .await
    .unwrap_err();
    assert_eq!(
        status_of(&refused),
        Some((
            meridian_bus::RefusalStatus::InvalidArgument,
            vec!["change.limit_price".to_string()]
        ))
    );
    // In the quote currency, by code or by instrument: amended.
    for limit in [
        usd("0.95"),
        Some(Money {
            amount: d("0.96"),
            currency_code: String::new(),
            instrument_id: CHF.into(),
        }),
    ] {
        order_command(
            &w,
            REQUEST_ORDER_CHANGE,
            "meridian.v1.RequestOrderChangeRequest",
            &amend(&order.order_id, limit),
            &person(TRADER),
        )
        .await
        .unwrap();
    }
    // An order naming no quote currency is checked as before.
    let plain = propose(&w, &buy("10")).await.unwrap();
    order_command(
        &w,
        REQUEST_ORDER_CHANGE,
        "meridian.v1.RequestOrderChangeRequest",
        &amend(&plain.order_id, gbp),
        &person(TRADER),
    )
    .await
    .unwrap();
}
