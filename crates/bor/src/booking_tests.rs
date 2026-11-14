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
        ..Default::default()
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
                ..Default::default()
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
    // A booked account's book from a snapshot at any entry (contract v25).
    let journal = w.store.journal(ACC).unwrap();
    let partition = journal[0].partition.clone();
    crate::snapshot::tests::holds_at_every_split(ACC, &partition, &journal);
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

    let mut updated = w.bus.subscribe_every(BOOKING_UPDATED);
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
    // The operations plugin's affirmation deadline, 21:00 in the
    // deployment's time zone (W11.34, contract v26), as the book read it.
    crate::deadline::keep(
        crate::deadline::of_setting(
            &meridian_domain::v1::DeploymentTimeZoneSetting {
                deployment_time_zone: "America/New_York".into(),
                affirmation_deadline: 21 * 60,
                ..Default::default()
            },
            |_| meridian_domain::zone::Zone::of_rule("EST5EDT,M3.2.0,M11.1.0"),
        )
        .unwrap(),
    );
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
    // Due by 21:00 EDT on its trade date, 2026-10-13: 01:00 UTC the next day.
    assert_eq!(alerts[0].due_by, 1_791_939_600 * 1_000_000_000);
    crate::deadline::keep(None);

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
async fn a_give_up_naming_nothing_held_is_kept_unplaced_whole() {
    // Contract v25 (W11.32; decision 30): its own arm, fact kind `give-up`.
    let w = wired();
    account(&w, ACC).await;
    let report = ReportGiveUpRequest {
        execution_id: "EXE-NOT-HELD".into(),
        allocation_id: String::new(),
        answer: GiveUpAnswer::Accepted as i32,
        prime_reference: "PB-1".into(),
        reason: "accepted".into(),
        raw_record: None,
    };
    let reply = send_from(
        &w.bus,
        REPORT_GIVE_UP,
        "meridian.v1.ReportGiveUpRequest",
        &report,
        &As::default(),
    )
    .await
    .unwrap();
    let kept = FactReply::decode(&reply[..]).unwrap().unplaced.unwrap();
    assert_eq!(kept.fact_kind, "give-up");
    // Contract v26 (W11.32): named, where v25 left the reason unspecified.
    assert_eq!(
        kept.reason,
        meridian_domain::v1::UnplacedReason::UnknownExecution as i32
    );
    assert!(kept.account_id.is_empty(), "naming no account");
    assert_eq!(
        kept.fact,
        Some(meridian_domain::v1::unplaced_fact::Fact::GiveUp(
            report.clone()
        ))
    );
    // Still held to its shape: an answer is accepted or rejected.
    let mut unanswered = report.clone();
    unanswered.answer = GiveUpAnswer::Unspecified as i32;
    let refused = send_from(
        &w.bus,
        REPORT_GIVE_UP,
        "meridian.v1.ReportGiveUpRequest",
        &unanswered,
        &As::default(),
    )
    .await
    .unwrap_err();
    assert!(refused.contains("accepted or rejected"), "{refused}");
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
            ..Default::default()
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

// ── Contract v27: FX netting (W11.1, W11.11; the gaps spec's D1 to D7) ────

/// A released FX order of EUR against francs in `account`, for value on
/// `value_date`: its ID.
async fn fx_released(
    w: &Wired,
    account: &str,
    side: Side,
    quantity: &str,
    value_date: &str,
) -> String {
    let request = ProposeOrderRequest {
        account_id: account.into(),
        instrument_id: EUR.into(),
        side: side as i32,
        quantity: d(quantity),
        order_type: OrderType::Market as i32,
        dealt_currency: EUR.into(),
        quote_currency: CHF.into(),
        time_in_force: TimeInForce::Day as i32,
        value_date: value_date.into(),
        ..Default::default()
    };
    let order = propose(w, &request).await.unwrap();
    assert_eq!(order.value_date, value_date);
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
    order.order_id
}

fn net_of(members: &[String]) -> ProposeOrderRequest {
    ProposeOrderRequest {
        account_id: BLOCK.into(),
        order_type: OrderType::Market as i32,
        time_in_force: TimeInForce::Day as i32,
        net_member_order_ids: members.to_vec(),
        ..Default::default()
    }
}

fn fx_execution(order_id: &str, side: Side, quantity: &str) -> ReportExecutionRequest {
    ReportExecutionRequest {
        account_id: BLOCK.into(),
        order_id: order_id.into(),
        kind: ExecutionKind::InternalCross as i32,
        side: side as i32,
        quantity: d(quantity),
        price: chf("0.9312"),
        trade_time_ns: NOW,
        trade_date: "2026-10-13".into(),
        settlement_date: "2026-10-15".into(),
        cross_rate_source: CrossRateSource::NetStreetRate as i32,
        ..Default::default()
    }
}

async fn entered(w: &Wired, request: &ReportExecutionRequest) -> Result<Execution, String> {
    send_from(
        &w.bus,
        REPORT_EXECUTION,
        "meridian.v1.ReportExecutionRequest",
        request,
        &anywhere(TRADER),
    )
    .await
    .map(|reply| FactReply::decode(&reply[..]).unwrap().execution.unwrap())
}

fn eur_in(w: &Wired, account: &str) -> String {
    let mut total = Exact::ZERO;
    for held in positions_of(w, account) {
        if held.instrument_id != EUR {
            continue;
        }
        let quantity: Exact = read(&held.trade_date_quantity).parse().unwrap();
        total = crate::numbers::add("t", total, quantity).unwrap();
    }
    total.to_string()
}

/// D1 to D5: opposite FX orders on one pair and one value date net into
/// one order in the allocation account, released on creation, its quantity
/// buys less sells; each member linked to it; the offset crossed internally
/// with its rate's source; the net's street fill and the crosses booked to
/// the members, the allocation account netting to zero.
#[tokio::test(flavor = "multi_thread")]
async fn opposite_fx_orders_net_to_one_order_and_the_offset_is_crossed_internally() {
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
    let buyer = fx_released(&w, FUND1, Side::Buy, "10000", "2026-10-15").await;
    let seller = fx_released(&w, FUND2, Side::Sell, "6000", "2026-10-15").await;
    let later = fx_released(&w, FUND2, Side::Sell, "1000", "2026-10-16").await;
    let other_buyer = fx_released(&w, FUND2, Side::Buy, "500", "2026-10-15").await;

    // Another value date, one side alone, or a block and a net at once.
    let refused = propose(&w, &net_of(&[buyer.clone(), seller.clone(), later.clone()]))
        .await
        .unwrap_err();
    assert_eq!(
        status_of(&refused).unwrap().1,
        vec!["net_member_order_ids[2]"]
    );
    let refused = propose(&w, &net_of(&[buyer.clone(), other_buyer.clone()]))
        .await
        .unwrap_err();
    assert_eq!(
        status_of(&refused).unwrap().1,
        vec!["net_member_order_ids[1]"]
    );
    let both = ProposeOrderRequest {
        block_member_order_ids: vec![buyer.clone()],
        ..net_of(&[buyer.clone(), seller.clone()])
    };
    assert_eq!(
        status_of(&propose(&w, &both).await.unwrap_err()).unwrap().1,
        vec!["block_member_order_ids", "net_member_order_ids"]
    );

    let net = propose(&w, &net_of(&[buyer.clone(), seller.clone()]))
        .await
        .unwrap();
    assert_eq!(state(&net), OrderState::Released);
    assert_eq!(read(&net.quantity), "4000");
    assert_eq!(net.side, Side::Buy as i32);
    assert_eq!(net.instrument_id, EUR);
    assert_eq!(net.quote_currency, CHF);
    assert_eq!(net.value_date, "2026-10-15");
    // Each member linked to it, by a child entry in its own account.
    for member in [&buyer, &seller] {
        let link = order_now(&w, member).link.unwrap();
        assert_eq!(link.kind, OrderLinkKind::Net as i32);
        assert_eq!(link.parent_order_id, net.order_id);
    }
    assert_eq!(
        w.store.journal(FUND1).unwrap().last().unwrap().meta.kind,
        "net-member-linked"
    );
    // A member in a live net is refused another.
    let again = propose(&w, &net_of(&[seller.clone(), other_buyer.clone()]))
        .await
        .unwrap_err();
    assert_eq!(code(&again), Some(RefusalReason::OrderState), "{again}");

    // The offset, crossed internally: refused with no source, on an order
    // that is not a net, and a source on a street fill.
    let mut no_source = fx_execution(&net.order_id, Side::Buy, "6000");
    no_source.cross_rate_source = 0;
    assert_eq!(
        status_of(&entered(&w, &no_source).await.unwrap_err())
            .unwrap()
            .1,
        vec!["cross_rate_source"]
    );
    let mut not_a_net = fx_execution(&buyer, Side::Buy, "6000");
    not_a_net.account_id = FUND1.into();
    assert_eq!(
        status_of(&entered(&w, &not_a_net).await.unwrap_err())
            .unwrap()
            .1,
        vec!["order_id"]
    );
    let cross_buy = entered(&w, &fx_execution(&net.order_id, Side::Buy, "6000"))
        .await
        .unwrap();
    let cross_sell = entered(&w, &fx_execution(&net.order_id, Side::Sell, "6000"))
        .await
        .unwrap();
    assert_eq!(
        cross_buy.cross_rate_source,
        CrossRateSource::NetStreetRate as i32
    );
    assert_eq!(cross_sell.side, Side::Sell as i32);
    // The crosses offset; they fill nothing of the net.
    assert_eq!(
        read(&quantities(&order_now(&w, &net.order_id)).reported_filled),
        "0"
    );

    // The net goes to market for 4000.
    let placement = place(&w, &net.order_id, "4000", &anywhere(TRADER))
        .await
        .unwrap()
        .placement
        .unwrap();
    let mut street = fill(&placement.placement_id, "4000", "NET:1");
    street.account_id = BLOCK.into();
    street.price = chf("0.9312");
    street.computed_settlement_date = "2026-10-15".into();
    let mut sourced = street.clone();
    sourced.cross_rate_source = CrossRateSource::MidAtNetting as i32;
    let refused = send_from(
        &w.bus,
        REPORT_EXECUTION,
        "meridian.v1.ReportExecutionRequest",
        &sourced,
        &As::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(status_of(&refused).unwrap().1, vec!["cross_rate_source"]);
    let street = FactReply::decode(
        &send_from(
            &w.bus,
            REPORT_EXECUTION,
            "meridian.v1.ReportExecutionRequest",
            &street,
            &As::default(),
        )
        .await
        .unwrap()[..],
    )
    .unwrap()
    .execution
    .unwrap();
    assert_eq!(state(&order_now(&w, &net.order_id)), OrderState::Filled);

    // Booked: the street fill and the buy cross to the buyer, the sell
    // cross to the seller; the allocation account nets to zero.
    let at = |account: &str, quantity: &str, gross: &str| Allocation {
        average_price: chf("0.9312"),
        ..allocation(account, quantity, gross)
    };
    book(
        &w,
        &net.order_id,
        &[street.execution_id.clone(), cross_buy.execution_id.clone()],
        vec![at(FUND1, "10000", "9312.00")],
        true,
    )
    .await
    .unwrap();
    book(
        &w,
        &net.order_id,
        std::slice::from_ref(&cross_sell.execution_id),
        vec![at(FUND2, "6000", "5587.20")],
        true,
    )
    .await
    .unwrap();
    assert_eq!(eur_in(&w, BLOCK), "0");
    assert_eq!(eur_in(&w, FUND1), "10000");
    let short: Vec<(i32, String)> = positions_of(&w, FUND2)
        .into_iter()
        .filter(|held| held.instrument_id == EUR)
        .map(|held| (held.side, read(&held.trade_date_quantity)))
        .collect();
    assert_eq!(
        short,
        vec![(HoldingSide::Short as i32, "-6000".to_string())]
    );
    for member in [&buyer, &seller] {
        assert_eq!(
            state(&order_now(&w, member)),
            OrderState::Filled,
            "{member}"
        );
    }
}

// ── Step 3d: bookings across partitions through `~clearing` ─────────────────

use std::collections::{BTreeMap, BTreeSet};

/// The booking legs' lines and clearing lines netted per instrument, each
/// partition's at every sequence, which every one must hold at zero; and
/// every clearing line's across the deployment.
fn legs_balance(w: &Wired) -> BTreeMap<String, Exact> {
    use crate::booking::{ALLOCATION_MOVED, BOOKED, CANCELLED, CORRECTED, HELD, POSTED};
    let legs = [BOOKED, CORRECTED, CANCELLED, ALLOCATION_MOVED, POSTED, HELD];
    let mut by_partition: BTreeMap<String, Vec<crate::journal::Entry>> = BTreeMap::new();
    for account in w.store.accounts().unwrap() {
        for entry in w.store.journal(&account).unwrap() {
            by_partition
                .entry(entry.partition.clone())
                .or_default()
                .push(entry);
        }
    }
    let add = |sums: &mut BTreeMap<String, Exact>, line: &MovementLine| {
        let held = sums.entry(line.instrument_id.clone()).or_default();
        *held = crate::numbers::add(
            "sum",
            *held,
            Exact::from_wire(line.quantity.as_ref().unwrap()).unwrap(),
        )
        .unwrap();
    };
    let mut cleared = BTreeMap::new();
    for (partition, mut entries) in by_partition {
        entries.sort_by_key(|entry| entry.first_sequence);
        let mut sums: BTreeMap<String, Exact> = BTreeMap::new();
        for entry in entries
            .iter()
            .filter(|entry| legs.contains(&entry.meta.kind.as_str()))
        {
            for line in entry.body.lines.iter().chain(&entry.body.clearing) {
                add(&mut sums, line);
            }
            for line in &entry.body.clearing {
                add(&mut cleared, line);
            }
            for (instrument, sum) in &sums {
                assert!(
                    sum.is_zero(),
                    "partition {partition} does not balance in {instrument} at sequence {} ({}): {sum}",
                    entry.last_sequence,
                    entry.meta.kind
                );
            }
        }
    }
    cleared
}

/// Step 3d (section 6.3; section 15's property test; the names and the bound
/// ruled 2026-11-13): bookings allocating at random across three partitions,
/// the writer stopped at random between their legs -- before a child, before
/// a post -- and resumed by the sweep, as a writer that takes over resumes
/// each pending booking from the legs that survive. Every partition balances
/// at every sequence through its `~clearing/<partition>` account; a booking
/// pending past its bound is held, its alert raised, nothing voided; the
/// clearing accounts net to zero once every booking is complete; and every
/// account's book replays identically, from a snapshot at any entry too.
#[tokio::test(flavor = "multi_thread")]
async fn bookings_across_partitions_balance_at_every_sequence_and_clearing_nets_to_zero() {
    use rand::{Rng, SeedableRng};
    let seed: u64 = std::env::var("MERIDIAN_PROPERTY_SEED")
        .ok()
        .and_then(|seed| seed.parse().ok())
        .unwrap_or(0x3d_c1ea);
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let w = wired_with(MemoryStore::new().with_partitions(3));
    let others: Vec<String> = (2..=7).map(|n| format!("ACC-{n}")).collect();
    account(&w, ACC).await;
    for other in &others {
        account(&w, other).await;
    }
    let partitions: BTreeSet<String> = std::iter::once(ACC.to_string())
        .chain(others.iter().cloned())
        .map(|account| w.store.partition_of(&account).unwrap())
        .collect();
    assert!(
        partitions.len() > 1,
        "seed {seed}: the accounts span partitions"
    );

    let mut bookings = Vec::new();
    let mut held_any = false;
    for round in 0..16 {
        let (order, executions) = filled_in(&w, ACC, "10", &["10"]).await;
        // An allocation to the order's account and up to three others, the
        // ten shares split at random.
        let mut chosen: Vec<String> = Vec::new();
        for _ in 0..rng.gen_range(1..=3) {
            let pick = others[rng.gen_range(0..others.len())].clone();
            if !chosen.contains(&pick) {
                chosen.push(pick);
            }
        }
        let mut left = 10u32;
        let mut split = Vec::new();
        for (index, account) in chosen.iter().enumerate() {
            let most = left - (chosen.len() - index) as u32;
            let take = rng.gen_range(1..=most.max(1)).min(most);
            split.push((account.clone(), take));
            left -= take;
        }
        split.push((ACC.to_string(), left));
        let mut instruction = AllocationInstruction::default();
        for (held, quantity) in &split {
            instruction.lines.push(AllocationLine {
                account_id: held.clone(),
                quantity: d(&quantity.to_string()),
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
                reason: "the client's split".into(),
            },
            &anywhere(PM),
        )
        .await
        .unwrap();
        let allocations: Vec<Allocation> = split
            .iter()
            .filter(|(_, quantity)| *quantity > 0)
            .map(|(held, quantity)| {
                let cents = u64::from(*quantity) * 22_741;
                let gross = format!("{}.{:02}", cents / 100, cents % 100);
                allocation(held, &quantity.to_string(), &gross)
            })
            .collect();

        // The writer stops at random: before each child, before the post.
        let stopped: BTreeSet<String> = chosen
            .iter()
            .filter(|_| rng.gen_bool(0.5))
            .cloned()
            .collect();
        let stop_post = rng.gen_bool(0.3);
        w.store.crash_when(Some(Box::new(move |account, key| {
            (key.starts_with("allocation-in:") && stopped.contains(account))
                || (stop_post && key.starts_with("booking-post:"))
        })));
        let booking = book(&w, &order.order_id, &executions, allocations, false)
            .await
            .unwrap();
        legs_balance(&w);
        let parent = w.store.journal(ACC).unwrap();
        let booked = parent
            .iter()
            .rev()
            .find(|entry| entry.meta.kind == crate::booking::BOOKED)
            .unwrap();
        assert_eq!(
            booked.body.pending.len(),
            chosen.len(),
            "round {round}: pending, naming each child"
        );
        for (account, _, partition) in &booked.body.pending {
            assert_eq!(Some(partition), w.store.partition_of(account).as_ref());
        }

        // Sometimes the writer stays away past the bound: the sweep holds
        // the booking and raises its alert, voiding nothing.
        let held_now = w
            .store
            .book(ACC)
            .unwrap()
            .ledger
            .bookings
            .get(&booking.booking_id)
            .is_some_and(|held| !held.posted);
        if held_now && rng.gen_bool(0.5) {
            w.clock.advance(crate::booking::PENDING_BOUND_NS + 1);
            sweep_children(&w.bus, w.store.as_ref()).unwrap();
            let alerts = alerts_in(&w, ACC);
            assert!(
                alerts
                    .iter()
                    .any(|alert| alert.kind == AlertKind::BookingHeld as i32
                        && alert
                            .subject
                            .as_ref()
                            .is_some_and(|subject| subject.booking_id == booking.booking_id)),
                "round {round}: pending past its bound, held"
            );
            held_any = true;
            legs_balance(&w);
        }

        // A writer takes over: resumed from the legs that survive.
        w.store.crash_when(None);
        sweep_children(&w.bus, w.store.as_ref()).unwrap();
        let held = w.store.book(ACC).unwrap().ledger.bookings[&booking.booking_id].clone();
        assert!(
            held.posted,
            "round {round}: posted once every child is journalled"
        );
        assert_eq!(
            w.store
                .journal(ACC)
                .unwrap()
                .iter()
                .filter(|entry| entry.meta.kind == crate::booking::POSTED
                    && entry.body.acts.iter().any(|act| matches!(act,
                        crate::orders::Act::BookingPosted { booking_id } if *booking_id == booking.booking_id)))
                .count(),
            usize::from(!chosen.is_empty()),
            "round {round}: posted once"
        );
        legs_balance(&w);
        bookings.push(booking);
    }
    assert!(
        held_any,
        "seed {seed}: some booking was held past its bound"
    );

    // Some cancelled: their children withdrawn, the clearing reversed.
    for booking in bookings.iter().filter(|_| rng.gen_bool(0.3)) {
        correct(
            &w,
            &CorrectBookingRequest {
                booking_id: booking.booking_id.clone(),
                kind: BookingCorrectionKind::Cancel as i32,
                reason: "booked in error".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        sweep_children(&w.bus, w.store.as_ref()).unwrap();
        legs_balance(&w);
    }

    // Complete: the clearing accounts net to zero across the deployment.
    for (instrument, net) in legs_balance(&w) {
        assert!(
            net.is_zero(),
            "seed {seed}: ~clearing nets {net} in {instrument}"
        );
    }
    // Replay identical, from a snapshot at any entry too.
    for account in w.store.accounts().unwrap() {
        let journal = w.store.journal(&account).unwrap();
        let partition = journal[0].partition.clone();
        let replayed = crate::Book::replay(&account, &partition, &journal).unwrap();
        let mut held = w.store.book(&account).unwrap();
        held.release_bodies();
        let mut again = replayed.clone();
        again.release_bodies();
        assert_eq!(again, held, "{account}: replay identical");
        crate::snapshot::tests::holds_at_every_split(&account, &partition, &journal);
    }
}

/// With one partition, the default, a booking is as it always was: no
/// clearing line, no pending marker, no post.
#[tokio::test(flavor = "multi_thread")]
async fn with_one_partition_a_booking_is_unchanged() {
    let w = wired();
    account(&w, ACC).await;
    account(&w, OTHER).await;
    let (order, executions) = filled_in(&w, ACC, "10", &["10"]).await;
    let mut instruction = AllocationInstruction::default();
    for (held, quantity) in [(ACC, "6"), (OTHER, "4")] {
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
            reason: "the client's split".into(),
        },
        &anywhere(PM),
    )
    .await
    .unwrap();
    book(
        &w,
        &order.order_id,
        &executions,
        vec![
            allocation(ACC, "6", "1364.46"),
            allocation(OTHER, "4", "909.64"),
        ],
        false,
    )
    .await
    .unwrap();
    assert_eq!(read(&aapl_in(&w, OTHER).unwrap().trade_date_quantity), "4");
    for account in [ACC, OTHER] {
        for entry in w.store.journal(account).unwrap() {
            assert!(entry.body.clearing.is_empty() && entry.body.pending.is_empty());
            assert_ne!(entry.meta.kind, crate::booking::POSTED);
        }
    }
}

/// Step 3d (found by e2e-orders at two partitions): a read of one account is
/// answered at its own partition's head alone, never another partition's.
/// A follower that reads an account holding nothing yet is had to the
/// watermark's partition, and a venue's SDK took P0's head for an account
/// in P1. It then passed over the account's first placement in P1, numbered
/// far below P0's head, as had. A read of no account names every partition,
/// as ever.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_of_one_account_is_answered_at_its_own_partitions_head() {
    let w = wired_with(MemoryStore::new().with_partitions(2));
    let others: Vec<String> = (10..40).map(|n| format!("ACC-{n}")).collect();
    for other in &others {
        account(&w, other).await;
    }
    let partition = |account: &str| w.store.partition_of(account).unwrap();
    let (in_p0, in_p1): (Vec<&String>, Vec<&String>) = others
        .iter()
        .partition(|account| partition(account) == "P0");
    // P0 well ahead of P1, as a busier partition is.
    for account in &in_p0 {
        account_again(&w, account).await;
    }
    let quiet = in_p1[0].as_str();
    let ask = |account: &str| ListOrdersRequest {
        account_id: account.into(),
        ..Default::default()
    };
    let read = |bytes: Vec<u8>| ListOrdersReply::decode(&bytes[..]).unwrap().as_of.unwrap();
    let mine = read(
        send_from(
            &w.bus,
            LIST_ORDERS,
            "meridian.v1.ListOrdersRequest",
            &ask(quiet),
            &anywhere(TRADER),
        )
        .await
        .unwrap(),
    );
    let heads = w.store.heads().unwrap();
    assert!(heads["P0"] > heads["P1"], "P0 is ahead: {heads:?}");
    assert_eq!(
        mine.partitions
            .iter()
            .map(|held| (held.partition.as_str(), held.sequence))
            .collect::<Vec<_>>(),
        [("P1", heads["P1"])],
        "its own partition's head alone"
    );
    let every = read(
        send_from(
            &w.bus,
            LIST_ORDERS,
            "meridian.v1.ListOrdersRequest",
            &ask(""),
            &anywhere(TRADER),
        )
        .await
        .unwrap(),
    );
    assert!(every.partitions.iter().any(|held| held.partition == "P0"));
    assert!(every.partitions.iter().any(|held| held.partition == "P1"));
}

/// An account's attribute set again, one more entry in its partition.
async fn account_again(w: &Wired, account: &str) {
    attribute(
        w,
        account,
        set_account_attribute_request::Attribute::BaseCurrencyCode("EUR".into()),
    )
    .await;
}

/// Step 3d (section 6.4, ruled 2026-11-12; section 15's restore across two
/// databases): a restore takes each database to its own time, so a
/// booking's legs in two partitions can come back at different points. The
/// book completes what survives and reverses what lost its parent, each by
/// an entry of its own, and every partition balances throughout:
/// - the child's database taken to before its child: the child is made
///   again from the parent that survives;
/// - the parent's database taken to before its booking: the child it
///   received is reversed, saying why, and the clearing accounts net to
///   zero.
#[tokio::test(flavor = "multi_thread")]
async fn a_restore_across_databases_completes_what_survives_and_reverses_what_lost_its_parent() {
    let w = wired_with(MemoryStore::new().with_partitions(3));
    account(&w, ACC).await;
    let home = w.store.partition_of(ACC).unwrap();
    let mut other = None;
    for n in 2..=12 {
        let candidate = format!("ACC-{n}");
        account(&w, &candidate).await;
        if w.store.partition_of(&candidate).unwrap() != home {
            other = Some(candidate);
            break;
        }
    }
    let other = other.expect("an account in another partition");
    let away = w.store.partition_of(&other).unwrap();
    let split = |order: &str| SetAllocationInstructionRequest {
        order_id: order.to_string(),
        allocation_instruction: Some(AllocationInstruction {
            lines: vec![
                AllocationLine {
                    account_id: other.clone(),
                    quantity: d("4"),
                    ..Default::default()
                },
                AllocationLine {
                    account_id: ACC.into(),
                    quantity: d("6"),
                    ..Default::default()
                },
            ],
        }),
        reason: "the client's split".into(),
    };
    let allocations = || {
        vec![
            allocation(&other, "4", "909.64"),
            allocation(ACC, "6", "1364.46"),
        ]
    };
    let received = |booking_id: &str| {
        w.store
            .book(&other)
            .unwrap()
            .ledger
            .received
            .iter()
            .any(|((id, _), held)| id == booking_id && !held.withdrawn)
    };

    // The child's database to before its child.
    let (order, executions) = filled_in(&w, ACC, "10", &["10"]).await;
    order_command(
        &w,
        SET_ALLOCATION_INSTRUCTION,
        "meridian.v1.SetAllocationInstructionRequest",
        &split(&order.order_id),
        &anywhere(PM),
    )
    .await
    .unwrap();
    let before_child = w.store.heads().unwrap()[&away];
    let first = book(&w, &order.order_id, &executions, allocations(), false)
        .await
        .unwrap();
    sweep_children(&w.bus, w.store.as_ref()).unwrap();
    assert!(received(&first.booking_id));
    w.store.restore_partition_to(&away, before_child);
    assert!(!received(&first.booking_id), "the restore took the child");
    legs_balance(&w);
    sweep_children(&w.bus, w.store.as_ref()).unwrap();
    assert!(received(&first.booking_id), "made again from the parent");
    legs_balance(&w);

    // The parent's database to before its booking.
    let (order, executions) = filled_in(&w, ACC, "10", &["10"]).await;
    order_command(
        &w,
        SET_ALLOCATION_INSTRUCTION,
        "meridian.v1.SetAllocationInstructionRequest",
        &split(&order.order_id),
        &anywhere(PM),
    )
    .await
    .unwrap();
    let before_booking = w.store.heads().unwrap()[&home];
    let second = book(&w, &order.order_id, &executions, allocations(), false)
        .await
        .unwrap();
    sweep_children(&w.bus, w.store.as_ref()).unwrap();
    assert!(received(&second.booking_id));
    w.store.restore_partition_to(&home, before_booking);
    assert!(!w
        .store
        .book(ACC)
        .unwrap()
        .ledger
        .bookings
        .contains_key(&second.booking_id));
    sweep_children(&w.bus, w.store.as_ref()).unwrap();
    assert!(!received(&second.booking_id), "its child reversed");
    let reversal = w
        .store
        .journal(&other)
        .unwrap()
        .into_iter()
        .rev()
        .find(|entry| {
            entry.body.acts.iter().any(|act| matches!(act,
                crate::orders::Act::AllocationWithdrawn { booking_id, .. } if *booking_id == second.booking_id))
        })
        .expect("an entry of its own");
    assert!(
        reversal.meta.reason.contains("a restore"),
        "{}",
        reversal.meta.reason
    );
    for (instrument, net) in legs_balance(&w) {
        assert!(net.is_zero(), "~clearing nets {net} in {instrument}");
    }
}
