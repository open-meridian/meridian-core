//! Booking's rules (W11.26 to W11.37, contract v24), held over the bus with
//! the in-memory store, as the order path's are: each command as an `oms`,
//! `operations`, `compliance` or connectivity plugin's sidecar sends it, and
//! what the book journals, answers and publishes (fixtures/book/ in
//! meridian-design: book-order, hand-off-booking, correct-booking,
//! report-give-up, record-sleeve-cross, raise-alert, set-local-record-orders).

use super::*;
use crate::booking_service::*;

const FUND1: &str = "ACC-FUND1";
const FUND2: &str = "ACC-FUND2";
const BLOCK: &str = "ACC-BLOCK";
const ERRORS: &str = "ACC-ERRORS";

/// A user with no write scope stamped: a command naming its record by ID
/// alone in any account, as a sidecar does for a plugin scoped to every
/// account.
fn anywhere(subject: &'static str) -> As {
    As {
        person: Some(subject),
        scope: None,
        allowed: true,
    }
}

fn admin() -> As {
    As {
        person: Some(ADMIN),
        ..Default::default()
    }
}

async fn attribute(w: &Wired, account: &str, attribute: set_account_attribute_request::Attribute) {
    send_from(
        &w.bus,
        SET_ACCOUNT_ATTRIBUTE,
        "meridian.v1.SetAccountAttributeRequest",
        &SetAccountAttributeRequest {
            account_id: account.into(),
            attribute: Some(attribute),
            reason: "set by the deployment admin".into(),
        },
        &admin(),
    )
    .await
    .unwrap();
}

/// An account with francs its base currency, releasing without waiting.
async fn account(w: &Wired, account: &str) {
    attribute(
        w,
        account,
        set_account_attribute_request::Attribute::BaseCurrencyCode("CHF".into()),
    )
    .await;
    waits(w, account, false).await;
}

fn chf(text: &str) -> Option<Money> {
    usd(text)
}

fn allocation(account: &str, quantity: &str, gross: &str) -> Allocation {
    Allocation {
        account_id: account.into(),
        quantity: d(quantity),
        average_price: chf("227.41"),
        gross_amount: chf(gross),
        net_amount: chf(gross),
        average_priced: true,
        ..Default::default()
    }
}

async fn book(
    w: &Wired,
    order_id: &str,
    execution_ids: &[String],
    allocations: Vec<Allocation>,
    hand_off: bool,
) -> Result<Booking, String> {
    send_from(
        &w.bus,
        BOOK_ORDER,
        "meridian.v1.BookOrderRequest",
        &BookOrderRequest {
            order_id: order_id.into(),
            execution_ids: execution_ids.to_vec(),
            allocations,
            hand_off,
        },
        &anywhere(TRADER),
    )
    .await
    .map(|reply| BookingReply::decode(&reply[..]).unwrap().booking.unwrap())
}

async fn hand_off(w: &Wired, booking_id: &str, close: bool) -> Result<Booking, String> {
    send_from(
        &w.bus,
        HAND_OFF_BOOKING,
        "meridian.v1.HandOffBookingRequest",
        &HandOffBookingRequest {
            booking_id: booking_id.into(),
            close,
        },
        &anywhere(TRADER),
    )
    .await
    .map(|reply| BookingReply::decode(&reply[..]).unwrap().booking.unwrap())
}

async fn correct(w: &Wired, request: &CorrectBookingRequest) -> Result<Booking, String> {
    send_from(
        &w.bus,
        CORRECT_BOOKING,
        "meridian.v1.CorrectBookingRequest",
        request,
        &anywhere(TRADER),
    )
    .await
    .map(|reply| BookingReply::decode(&reply[..]).unwrap().booking.unwrap())
}

fn booking_state(booking: &Booking) -> BookingState {
    BookingState::try_from(booking.state).unwrap()
}

/// An order of `quantity` in `account`, released, placed and filled in the
/// fills given, each at 227.41: the order and its executions' IDs.
async fn filled_in(
    w: &Wired,
    account: &str,
    quantity: &str,
    fills: &[&str],
) -> (Order, Vec<String>) {
    let mut request = buy(quantity);
    request.account_id = account.into();
    let order = propose(w, &request).await.unwrap();
    order_command(
        w,
        RELEASE_ORDER,
        "meridian.v1.ReleaseOrderRequest",
        &ReleaseOrderRequest {
            order_id: order.order_id.clone(),
            idempotency_key: String::new(),
        },
        &anywhere(TRADER),
    )
    .await
    .unwrap();
    let placed: f64 = fills.iter().map(|q| q.parse::<f64>().unwrap()).sum();
    let placement = place(w, &order.order_id, &placed.to_string(), &anywhere(TRADER))
        .await
        .unwrap()
        .placement
        .unwrap();
    let mut ids = Vec::new();
    for (index, quantity) in fills.iter().enumerate() {
        let mut request = fill(
            &placement.placement_id,
            quantity,
            &format!("{account}:{}:{index}", order.order_id),
        );
        request.account_id = account.into();
        let reply = send_from(
            &w.bus,
            REPORT_EXECUTION,
            "meridian.v1.ReportExecutionRequest",
            &request,
            &As {
                person: None,
                scope: None,
                allowed: false,
            },
        )
        .await
        .unwrap();
        ids.push(
            FactReply::decode(&reply[..])
                .unwrap()
                .execution
                .unwrap()
                .execution_id,
        );
    }
    (order, ids)
}

fn positions_of(w: &Wired, account: &str) -> Vec<BookPosition> {
    w.store
        .positions(&crate::store::PositionsRead {
            scope: Scope::Everything,
            account_id: account.into(),
            since: None,
            business_date: String::new(),
            at: None,
            limit: 100,
            cursor: String::new(),
        })
        .unwrap()
        .records
}

fn aapl_in(w: &Wired, account: &str) -> Option<BookPosition> {
    positions_of(w, account)
        .into_iter()
        .find(|held| held.instrument_id == AAPL && held.side == HoldingSide::Long as i32)
}

fn alerts_in(w: &Wired, account: &str) -> Vec<Alert> {
    w.store
        .alerts(&crate::store::AlertsRead {
            scope: Scope::Everything,
            account_id: account.into(),
            since: None,
            state: 0,
            kind: 0,
            limit: 100,
            cursor: String::new(),
        })
        .unwrap()
        .records
}

#[test]
fn every_booking_row_the_book_names_is_in_the_generated_table() {
    for id in ["b1", "b2", "b3", "b4", "b5", "b6", "b7", "b8", "b9"] {
        assert_eq!(crate::booking::booking_row(id).row, id);
    }
    assert_eq!(crate::transitions::BOOKING_STATES.len(), 5);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_order_is_booked_to_its_account_and_its_lots_re_stated_at_the_average_price() {
    let w = wired();
    account(&w, ACC).await;
    let (order, executions) = filled_in(&w, ACC, "10", &["6", "4"]).await;
    let before = aapl_in(&w, ACC).unwrap();
    assert_eq!(
        before.lots.len(),
        2,
        "a lot per fill, provisional until booked"
    );

    let mut one = allocation(ACC, "10", "2274.10");
    one.fees = vec![Fee {
        kind: FeeKind::Commission as i32,
        amount: chf("0"),
        capitalised: true,
    }];
    let booking = book(&w, &order.order_id, &executions, vec![one], false)
        .await
        .unwrap();
    assert!(booking.booking_id.starts_with("BKG-"));
    assert_eq!(booking_state(&booking), BookingState::Booked);
    assert!(booking.allocations[0].allocation_id.starts_with("ALC-"));
    assert_eq!(
        booking.allocations[0].intended_settlement_date,
        "2026-10-14"
    );

    // The fills' lots closed, one opened at the allocation's average price.
    let after = aapl_in(&w, ACC).unwrap();
    assert_eq!(read(&after.trade_date_quantity), "10");
    let [lot] = after.lots.as_slice() else {
        panic!("one lot per allocation: {:?}", after.lots)
    };
    assert_eq!(read(&lot.open_quantity), "10");
    let terms = lot.terms.clone().unwrap();
    assert_eq!(read(&terms.cost.unwrap().amount), "2274.10");
    assert_eq!(read(&terms.unit_cost.unwrap().amount), "227.41");
    assert_eq!(terms.acquired_date, "2026-10-13");

    // An execution in a live booking is refused another.
    let again = book(
        &w,
        &order.order_id,
        &executions[..1],
        vec![allocation(ACC, "6", "1364.46")],
        false,
    )
    .await
    .unwrap_err();
    assert_eq!(code(&again), Some(RefusalReason::OrderState), "{again}");

    let handed = hand_off(&w, &booking.booking_id, false).await.unwrap();
    assert_eq!(booking_state(&handed), BookingState::HandedOff);
    let twice = hand_off(&w, &booking.booking_id, false).await.unwrap_err();
    assert_eq!(code(&twice), Some(RefusalReason::OrderState), "{twice}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_booking_that_does_not_tie_out_or_names_an_account_not_instructed_is_refused() {
    let w = wired();
    account(&w, ACC).await;
    account(&w, OTHER).await;
    let (order, executions) = filled_in(&w, ACC, "10", &["10"]).await;
    let short = book(
        &w,
        &order.order_id,
        &executions,
        vec![allocation(ACC, "9", "2046.69")],
        false,
    )
    .await
    .unwrap_err();
    assert_eq!(
        code(&short),
        Some(RefusalReason::BookingUnbalanced),
        "{short}"
    );
    let gross = book(
        &w,
        &order.order_id,
        &executions,
        vec![allocation(ACC, "10", "2274.11")],
        false,
    )
    .await
    .unwrap_err();
    assert_eq!(
        code(&gross),
        Some(RefusalReason::BookingUnbalanced),
        "{gross}"
    );
    let elsewhere = book(
        &w,
        &order.order_id,
        &executions,
        vec![
            allocation(ACC, "8", "1819.28"),
            allocation(OTHER, "2", "454.82"),
        ],
        false,
    )
    .await
    .unwrap_err();
    assert_eq!(
        code(&elsewhere),
        Some(RefusalReason::AccountNotInstructed),
        "{elsewhere}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_overfill_is_booked_to_the_error_account_with_its_cause_and_an_alert() {
    let w = wired();
    account(&w, ACC).await;
    account(&w, ERRORS).await;
    attribute(
        &w,
        ERRORS,
        set_account_attribute_request::Attribute::ErrorAccount(true),
    )
    .await;
    let (order, executions) = filled_in(&w, ACC, "10", &["10", "2"]).await;
    assert!(holds(
        &order_now(&w, &order.order_id),
        OrderCondition::DoesNotTieOut
    ));

    // Without its reason, refused naming it.
    let mut excess = allocation(ERRORS, "2", "454.82");
    excess.error_cause = ErrorCause::Overfill as i32;
    let refused = book(
        &w,
        &order.order_id,
        &executions,
        vec![allocation(ACC, "10", "2274.10"), excess.clone()],
        false,
    )
    .await
    .unwrap_err();
    assert_eq!(
        code(&refused),
        Some(RefusalReason::ReasonRequired),
        "{refused}"
    );

    excess.reason = "the venue filled two more than ordered".into();
    let booking = book(
        &w,
        &order.order_id,
        &executions,
        vec![allocation(ACC, "10", "2274.10"), excess],
        true,
    )
    .await
    .unwrap();
    assert_eq!(booking_state(&booking), BookingState::HandedOff);
    // The excess moved out of the client's account into the error account.
    assert_eq!(read(&aapl_in(&w, ACC).unwrap().trade_date_quantity), "10");
    assert_eq!(read(&aapl_in(&w, ERRORS).unwrap().trade_date_quantity), "2");
    // The order ties out, and an alert stands for compliance.
    assert!(!holds(
        &order_now(&w, &order.order_id),
        OrderCondition::DoesNotTieOut
    ));
    let alerts = alerts_in(&w, ACC);
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].kind, AlertKind::ErrorAccountBooking as i32);
    assert_eq!(alerts[0].state, AlertState::Raised as i32);
    assert!(alerts[0].alert_id.starts_with("ALR-"));

    // Compliance acknowledges it, for a person, with a reason.
    let acknowledged = send_from(
        &w.bus,
        ACKNOWLEDGE_ALERT,
        "meridian.v1.AcknowledgeAlertRequest",
        &AcknowledgeAlertRequest {
            alert_id: alerts[0].alert_id.clone(),
            reason: "reviewed: the venue's error".into(),
        },
        &anywhere(PM),
    )
    .await
    .unwrap();
    let alert = AlertReply::decode(&acknowledged[..])
        .unwrap()
        .alert
        .unwrap();
    assert_eq!(alert.state, AlertState::Acknowledged as i32);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_block_is_booked_across_two_funds_and_each_member_is_filled_by_its_allocation() {
    let w = wired();
    for held in [FUND1, FUND2, BLOCK] {
        account(&w, held).await;
    }
    attribute(
        &w,
        BLOCK,
        set_account_attribute_request::Attribute::AllocationAccount(true),
    )
    .await;
    // Each fund's order, released.
    let mut members = Vec::new();
    for (fund, quantity) in [(FUND1, "6"), (FUND2, "4")] {
        let mut request = buy(quantity);
        request.account_id = fund.into();
        let order = propose(&w, &request).await.unwrap();
        order_command(
            &w,
            RELEASE_ORDER,
            "meridian.v1.ReleaseOrderRequest",
            &ReleaseOrderRequest {
                order_id: order.order_id.clone(),
                idempotency_key: String::new(),
            },
            &anywhere(TRADER),
        )
        .await
        .unwrap();
        members.push(order.order_id);
    }
    // The block, in the allocation account, released on creation, its
    // quantity the members' idle.
    let mut request = buy("10");
    request.account_id = BLOCK.into();
    request.quantity = None;
    request.block_member_order_ids = members.clone();
    let block = propose(&w, &request).await.unwrap();
    assert_eq!(state(&block), OrderState::Released);
    assert_eq!(read(&block.quantity), "10");
    assert_eq!(block.block_member_order_ids, members);
    // A member in a live block is refused a second.
    let second = propose(&w, &request).await.unwrap_err();
    assert_eq!(code(&second), Some(RefusalReason::OrderState), "{second}");

    let placement = place(&w, &block.order_id, "10", &anywhere(TRADER))
        .await
        .unwrap()
        .placement
        .unwrap();
    let mut filled = fill(&placement.placement_id, "10", "BLOCK:1");
    filled.account_id = BLOCK.into();
    let execution = FactReply::decode(
        &send_from(
            &w.bus,
            REPORT_EXECUTION,
            "meridian.v1.ReportExecutionRequest",
            &filled,
            &As::default(),
        )
        .await
        .unwrap()[..],
    )
    .unwrap()
    .execution
    .unwrap();
    assert_eq!(read(&aapl_in(&w, BLOCK).unwrap().trade_date_quantity), "10");

    let mut updated = w.bus.subscribe(BOOKING_UPDATED);
    let booking = book(
        &w,
        &block.order_id,
        &[execution.execution_id],
        vec![
            allocation(FUND1, "6", "1364.46"),
            allocation(FUND2, "4", "909.64"),
        ],
        true,
    )
    .await
    .unwrap();
    assert_eq!(booking_state(&booking), BookingState::HandedOff);
    let heard =
        BookingUpdatedEvent::decode(&next(&mut updated).await.envelope.payload[..]).unwrap();
    assert_eq!(heard.booking.unwrap().booking_id, booking.booking_id);

    // The allocation account nets to zero, each fund holds its allocation at
    // the average price, and each member is filled by it.
    assert_eq!(read(&aapl_in(&w, BLOCK).unwrap().trade_date_quantity), "0");
    let fund1 = aapl_in(&w, FUND1).unwrap();
    assert_eq!(read(&fund1.trade_date_quantity), "6");
    assert_eq!(
        read(&fund1.lots[0].terms.clone().unwrap().cost.unwrap().amount),
        "1364.46"
    );
    assert_eq!(read(&aapl_in(&w, FUND2).unwrap().trade_date_quantity), "4");
    for member in &members {
        let order = order_now(&w, member);
        assert_eq!(state(&order), OrderState::Filled, "{member}");
    }
    // The funds' cash moved, the block's back to where it was.
    let francs = |account: &str| {
        positions_of(&w, account)
            .into_iter()
            .find(|held| held.instrument_id == CHF)
            .map(|held| read(&held.trade_date_quantity))
            .unwrap_or_else(|| "0".into())
    };
    assert_eq!(francs(FUND1), "-1364.46");
    assert_eq!(francs(FUND2), "-909.64");
    assert!(
        ["0", "0.00"].contains(&francs(BLOCK).as_str()),
        "{}",
        francs(BLOCK)
    );

    // Operations replaces it after the hand-off, five and five: the funds'
    // accounts are the block's instructed ones still.
    let replaced = correct(
        &w,
        &CorrectBookingRequest {
            booking_id: booking.booking_id.clone(),
            kind: BookingCorrectionKind::Replace as i32,
            allocations: vec![
                allocation(FUND1, "5", "1137.05"),
                allocation(FUND2, "5", "1137.05"),
            ],
            reason: "the allocation was five and five".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(booking_state(&replaced), BookingState::HandedOff);
    assert_eq!(read(&aapl_in(&w, FUND1).unwrap().trade_date_quantity), "5");
    assert_eq!(read(&aapl_in(&w, FUND2).unwrap().trade_date_quantity), "5");

    // Read back and caught up (W11.31).
    let page = w
        .store
        .bookings(&crate::store::BookingsRead {
            scope: Scope::Everything,
            account_id: String::new(),
            since: None,
            state: BookingState::HandedOff as i32,
            limit: 100,
            cursor: String::new(),
        })
        .unwrap();
    assert_eq!(page.records.len(), 1);
    let none_since = w
        .store
        .bookings(&crate::store::BookingsRead {
            scope: Scope::Everything,
            account_id: String::new(),
            since: Some(page.as_of.clone()),
            state: 0,
            limit: 100,
            cursor: String::new(),
        })
        .unwrap();
    assert!(none_since.records.is_empty());
    // The sweep, one account at a time (approved 2026-10-15): each fund
    // received its allocation already, so nothing is owed and nothing made.
    let store: &dyn crate::store::Store = w.store.as_ref();
    assert_eq!(crate::booking_service::sweep_children(&w.bus, store), Ok(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bust_holds_a_booking_its_cancel_clears_the_alert_and_it_is_booked_again() {
    let w = wired();
    account(&w, ACC).await;
    let (order, executions) = filled_in(&w, ACC, "10", &["6", "4"]).await;
    let booking = book(
        &w,
        &order.order_id,
        &executions,
        vec![allocation(ACC, "10", "2274.10")],
        false,
    )
    .await
    .unwrap();
    // A bust of one of its executions (row b5): held, an alert raised.
    let held_order = order_now(&w, &order.order_id);
    let placement_id = held_order.placements[0].placement_id.clone();
    let mut bust = fill(&placement_id, "0", "BUST:1");
    bust.supersedes = executions[1].clone();
    bust.price = None;
    execute(&w, &bust).await.unwrap();
    let held = w
        .store
        .bookings(&crate::store::BookingsRead {
            scope: Scope::Everything,
            account_id: ACC.into(),
            since: None,
            state: BookingState::Held as i32,
            limit: 10,
            cursor: String::new(),
        })
        .unwrap()
        .records;
    assert_eq!(held.len(), 1, "held by the bust");
    assert!(
        held[0].held_reason.contains("busted"),
        "{}",
        held[0].held_reason
    );
    let alerts = alerts_in(&w, ACC);
    assert_eq!(alerts[0].kind, AlertKind::BookingHeld as i32);

    // Its hand-off's check fails: it stays held.
    let still = hand_off(&w, &booking.booking_id, false).await.unwrap();
    assert_eq!(booking_state(&still), BookingState::Held);

    // Cancelled before the hand-off, with a reason: the alert cleared.
    let no_reason = correct(
        &w,
        &CorrectBookingRequest {
            booking_id: booking.booking_id.clone(),
            kind: BookingCorrectionKind::Cancel as i32,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&no_reason), Some(RefusalReason::ReasonRequired));
    let cancelled = correct(
        &w,
        &CorrectBookingRequest {
            booking_id: booking.booking_id.clone(),
            kind: BookingCorrectionKind::Cancel as i32,
            reason: "rebook without the busted fill".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(booking_state(&cancelled), BookingState::Cancelled);
    assert_eq!(alerts_in(&w, ACC)[0].state, AlertState::Cleared as i32);

    // The execution left booked again.
    let again = book(
        &w,
        &order.order_id,
        &executions[..1],
        vec![allocation(ACC, "6", "1364.46")],
        true,
    )
    .await
    .unwrap();
    assert_eq!(booking_state(&again), BookingState::HandedOff);
    assert_eq!(read(&aapl_in(&w, ACC).unwrap().trade_date_quantity), "6");
}

#[tokio::test(flavor = "multi_thread")]
async fn operations_replaces_a_handed_off_booking_and_a_cancel_after_it_is_refused() {
    let w = wired();
    account(&w, ACC).await;
    account(&w, OTHER).await;
    let (order, executions) = filled_in(&w, ACC, "10", &["10"]).await;
    let booking = book(
        &w,
        &order.order_id,
        &executions,
        vec![allocation(ACC, "10", "2274.10")],
        true,
    )
    .await
    .unwrap();
    let cancel = correct(
        &w,
        &CorrectBookingRequest {
            booking_id: booking.booking_id.clone(),
            kind: BookingCorrectionKind::Cancel as i32,
            reason: "too late".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&cancel), Some(RefusalReason::OrderState), "{cancel}");

    // Portfolio adds the account, then operations moves four there.
    let mut instruction = AllocationInstruction::default();
    for (held, quantity) in [(ACC, "10"), (OTHER, "4")] {
        instruction.lines.push(AllocationLine {
            account_id: held.into(),
            quantity: d(quantity),
            ..Default::default()
        });
    }
    order_command(
        &w,
        SET_ALLOCATION_INSTRUCTION,
        "meridian.v1.SetAllocationInstructionRequest",
        &SetAllocationInstructionRequest {
            order_id: order.order_id.clone(),
            allocation_instruction: Some(instruction),
            reason: "the client's instruction arrived".into(),
        },
        &anywhere(PM),
    )
    .await
    .unwrap();
    let replaced = correct(
        &w,
        &CorrectBookingRequest {
            booking_id: booking.booking_id.clone(),
            kind: BookingCorrectionKind::Replace as i32,
            allocations: vec![
                allocation(ACC, "6", "1364.46"),
                allocation(OTHER, "4", "909.64"),
            ],
            reason: "the client's instruction split it".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        booking_state(&replaced),
        BookingState::HandedOff,
        "the state kept"
    );
    assert_eq!(replaced.allocations.len(), 2);
    assert_eq!(read(&aapl_in(&w, ACC).unwrap().trade_date_quantity), "6");
    assert_eq!(read(&aapl_in(&w, OTHER).unwrap().trade_date_quantity), "4");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_give_up_answer_is_recorded_on_the_execution_and_its_allocation() {
    let w = wired();
    account(&w, ACC).await;
    let (order, executions) = filled_in(&w, ACC, "10", &["10"]).await;
    let booking = book(
        &w,
        &order.order_id,
        &executions,
        vec![allocation(ACC, "10", "2274.10")],
        false,
    )
    .await
    .unwrap();
    let reply = send_from(
        &w.bus,
        REPORT_GIVE_UP,
        "meridian.v1.ReportGiveUpRequest",
        &ReportGiveUpRequest {
            execution_id: executions[0].clone(),
            allocation_id: booking.allocations[0].allocation_id.clone(),
            answer: GiveUpAnswer::Rejected as i32,
            prime_reference: "PB-7731".into(),
            reason: "not alleged to us".into(),
            raw_record: None,
        },
        &As::default(),
    )
    .await
    .unwrap();
    let execution = FactReply::decode(&reply[..]).unwrap().execution.unwrap();
    assert_eq!(execution.give_up_answers.len(), 1);
    assert_eq!(execution.give_up_answers[0].prime_reference, "PB-7731");
    assert!(execution.give_up_answers[0].entry.is_some());
    let held = w
        .store
        .bookings(&crate::store::BookingsRead {
            scope: Scope::Everything,
            account_id: ACC.into(),
            since: None,
            state: 0,
            limit: 10,
            cursor: String::new(),
        })
        .unwrap()
        .records;
    assert_eq!(held[0].allocations[0].give_up_answers.len(), 1);
    // A rejection moves no position.
    assert_eq!(read(&aapl_in(&w, ACC).unwrap().trade_date_quantity), "10");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sleeve_cross_fills_its_two_orders_moving_lots_between_sleeves() {
    let w = wired();
    account(&w, ACC).await;
    // Value sleeve holds five, bought.
    let mut value = buy("5");
    value.sleeve = "value".into();
    let order = propose(&w, &value).await.unwrap();
    release(&w, &order.order_id).await.unwrap();
    let placement = place(&w, &order.order_id, "5", &person(TRADER))
        .await
        .unwrap()
        .placement
        .unwrap();
    execute(&w, &fill(&placement.placement_id, "5", "SX:1"))
        .await
        .unwrap();
    // A sale from value and a buy into growth, crossed.
    let mut sell = buy("3");
    sell.side = Side::Sell as i32;
    sell.sleeve = "value".into();
    let sell = propose(&w, &sell).await.unwrap();
    release(&w, &sell.order_id).await.unwrap();
    let mut growth = buy("3");
    growth.sleeve = "growth".into();
    let growth = propose(&w, &growth).await.unwrap();
    release(&w, &growth.order_id).await.unwrap();
    let reply = send_from(
        &w.bus,
        RECORD_SLEEVE_CROSS,
        "meridian.v1.RecordSleeveCrossRequest",
        &RecordSleeveCrossRequest {
            sleeve_cross: Some(SleeveCross {
                account_id: ACC.into(),
                instrument_id: AAPL.into(),
                giving_sleeve: "value".into(),
                receiving_sleeve: "growth".into(),
                quantity: d("3"),
                cross_price: chf("228.00"),
                price_source: "mid at 15:59".into(),
                order_ids: vec![sell.order_id.clone(), growth.order_id.clone()],
                reason: "the PMs' opposite orders crossed".into(),
                ..Default::default()
            }),
        },
        &person(TRADER),
    )
    .await
    .unwrap();
    let cross = SleeveCrossReply::decode(&reply[..])
        .unwrap()
        .sleeve_cross
        .unwrap();
    assert!(cross.sleeve_cross_id.starts_with("SXC-"));
    for order_id in [&sell.order_id, &growth.order_id] {
        let order = order_now(&w, order_id);
        assert_eq!(state(&order), OrderState::Filled, "{order_id}");
        assert_eq!(order.sleeve_cross_ids, vec![cross.sleeve_cross_id.clone()]);
    }
    let by_sleeve = |sleeve: &str| {
        positions_of(&w, ACC)
            .into_iter()
            .find(|held| held.instrument_id == AAPL && held.sleeve == sleeve)
            .map(|held| read(&held.trade_date_quantity))
    };
    assert_eq!(by_sleeve("value").as_deref(), Some("2"));
    assert_eq!(by_sleeve("growth").as_deref(), Some("3"));
}

#[tokio::test(flavor = "multi_thread")]
async fn with_the_escape_hatch_off_an_order_on_a_local_record_is_refused_and_a_recorded_one_is_not()
{
    let w = wired();
    account(&w, ACC).await;
    let mut local = buy("5");
    local.instrument_id = LOCAL.into();
    // On by default: proposed, and counted.
    assert!(propose(&w, &local).await.is_ok());
    let read_setting = || async {
        LocalRecordOrdersSetting::decode(
            &send_from(
                &w.bus,
                READ_LOCAL_RECORD_ORDERS,
                "meridian.v1.ReadLocalRecordOrdersRequest",
                &ReadLocalRecordOrdersRequest {},
                &As::default(),
            )
            .await
            .unwrap()[..],
        )
        .unwrap()
    };
    let before = read_setting().await;
    assert!(before.allows_local_record_orders);
    assert_eq!(before.open_order_count, 1);

    // Turned off by the deployment admin, with a reason.
    let off = send_from(
        &w.bus,
        SET_LOCAL_RECORD_ORDERS,
        "meridian.v1.SetLocalRecordOrdersRequest",
        &SetLocalRecordOrdersRequest {
            allows_local_record_orders: false,
            reason: "orders only on the platform's records".into(),
        },
        &admin(),
    )
    .await
    .unwrap();
    let setting = LocalRecordOrdersSetting::decode(&off[..]).unwrap();
    assert!(!setting.allows_local_record_orders);
    assert_eq!(setting.open_order_count, 1, "the order working continues");

    let refused = propose(&w, &local).await.unwrap_err();
    assert_eq!(
        code(&refused),
        Some(RefusalReason::LocalRecordNotAllowed),
        "{refused}"
    );
    // A telephoned order is recorded whatever the setting (row 23a).
    let mut telephoned = telephoned("PHONE-77");
    telephoned.instrument_id = LOCAL.into();
    let recorded = order_command(
        &w,
        RECORD_ORDER,
        "meridian.v1.RecordOrderRequest",
        &telephoned,
        &person(TRADER),
    )
    .await
    .unwrap();
    assert!(recorded.order.is_some());
    assert_eq!(read_setting().await.open_order_count, 2);
    // An order on an INS- record is untouched.
    assert!(propose(&w, &buy("1")).await.is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_base_currency_is_a_balance_and_every_other_currency_opens_lots() {
    // Contract v24 (W11.13, option (c)): an FX fill's legs follow the account.
    let w = wired();
    // ACC's base is CHF: a EUR/CHF buy opens EUR lots and moves the CHF
    // balance, as before.
    let (_, _) = fx_filled(&w, "1000", "FX:1", "2026-10-15").await;
    let eur = position(&w, EUR, HoldingSide::Long).unwrap();
    assert_eq!(eur.lots.len(), 1);
    assert_eq!(
        read(&eur.lots[0].terms.clone().unwrap().cost.unwrap().amount),
        "930.00",
        "its cost in the base currency"
    );
    let francs = chf_position(&w);
    assert!(francs.lots.is_empty(), "the base currency is a balance");

    // An account with no base set: every currency opens lots (question 23).
    waits(&w, OTHER, false).await;
    let mut request = fx_market(CHF);
    request.account_id = OTHER.into();
    request.quantity = d("100");
    let order = propose(&w, &request).await.unwrap();
    order_command(
        &w,
        RELEASE_ORDER,
        "meridian.v1.ReleaseOrderRequest",
        &ReleaseOrderRequest {
            order_id: order.order_id.clone(),
            idempotency_key: String::new(),
        },
        &anywhere(TRADER),
    )
    .await
    .unwrap();
    let placement = place(&w, &order.order_id, "100", &anywhere(TRADER))
        .await
        .unwrap()
        .placement
        .unwrap();
    let mut filled = fill(&placement.placement_id, "100", "FX:2");
    filled.account_id = OTHER.into();
    filled.price = usd("0.93");
    send_from(
        &w.bus,
        REPORT_EXECUTION,
        "meridian.v1.ReportExecutionRequest",
        &filled,
        &As::default(),
    )
    .await
    .unwrap();
    let paid = positions_of(&w, OTHER)
        .into_iter()
        .find(|held| held.instrument_id == CHF)
        .unwrap();
    assert_eq!(
        paid.side,
        HoldingSide::Short as i32,
        "francs paid beyond its lots"
    );
    assert_eq!(paid.lots.len(), 1);
}

/// Contract v24 (W11's invariant, the one-ID spec's requirement 8): an open
/// order on a local record, the record resolved before its fill -- the fill
/// lands on the `INS-` record, the order keeping its ID; a child naming the
/// record it became is under the same instrument as its parent.
#[tokio::test(flavor = "multi_thread")]
async fn an_open_orders_fill_after_its_record_resolved_lands_on_the_record_it_became() {
    let w = wired();
    account(&w, ACC).await;
    let mut request = buy("5");
    request.instrument_id = RESOLVING.into();
    let order = propose(&w, &request).await.unwrap();
    order_command(
        &w,
        RELEASE_ORDER,
        "meridian.v1.ReleaseOrderRequest",
        &ReleaseOrderRequest {
            order_id: order.order_id.clone(),
            idempotency_key: String::new(),
        },
        &anywhere(TRADER),
    )
    .await
    .unwrap();
    let placement = place(&w, &order.order_id, "5", &anywhere(TRADER))
        .await
        .unwrap()
        .placement
        .unwrap();
    // The platform answers: the local record is replaced (W3.8).
    crate::became::record(RESOLVING, RESOLVED);
    let reply = send_from(
        &w.bus,
        REPORT_EXECUTION,
        "meridian.v1.ReportExecutionRequest",
        &fill(&placement.placement_id, "5", "RSLV-1"),
        &As {
            person: None,
            scope: None,
            allowed: false,
        },
    )
    .await
    .unwrap();
    let execution = FactReply::decode(&reply[..]).unwrap().execution.unwrap();
    assert_eq!(execution.instrument_id, RESOLVING, "kept as named");
    let held = positions_of(&w, ACC);
    assert!(held
        .iter()
        .any(|p| p.instrument_id == RESOLVED && read(&p.trade_date_quantity) == "5"));
    assert!(!held.iter().any(|p| p.instrument_id == RESOLVING));
    // Booked after the resolution: its lots found on the record it became,
    // re-stated there.
    let booking = book(
        &w,
        &order.order_id,
        std::slice::from_ref(&execution.execution_id),
        vec![allocation(ACC, "5", "1137.05")],
        false,
    )
    .await
    .unwrap();
    assert_eq!(booking_state(&booking), BookingState::Booked);
    // A child naming the record it became is the parent's instrument.
    let mut child = buy("1");
    child.instrument_id = RESOLVED.into();
    child.link = Some(meridian_domain::v1::OrderLink {
        parent_order_id: order.order_id.clone(),
        ..Default::default()
    });
    let refused = propose(&w, &child).await.err().unwrap_or_default();
    assert!(!refused.contains("instrument"), "{refused}");
}
