//! Contract v29, track B and the reporting fields (W11.1, W11.11, W11.27,
//! W9.16; rulings 685, 709 to 723, 750, 751, 986 to 988): a debt order in
//! face and percent of par, yield or spread; a fill's cash per 100 of face,
//! a pool's at the factor it states; an execution's and an allocation's
//! reporting fields kept, and what W11 refuses of them; accrued interest its
//! own cash line at booking, and a coupon's cash split into the accrued the
//! purchase paid and the income.

use super::*;
use crate::booking_service::BOOK_ORDER;
use crate::numbers;
use crate::service::{RECORD_LIFECYCLE_EVENT, RECORD_OPENING_BALANCE};

fn bond_order(quantity: &str) -> ProposeOrderRequest {
    ProposeOrderRequest {
        instrument_id: BOND.into(),
        price_basis: PriceBasis::PercentOfPar as i32,
        limit_price: usd("99.125"),
        ..buy(quantity)
    }
}

fn named(detail: &str) -> Vec<String> {
    status_of(detail)
        .map(|(_, fields)| fields)
        .unwrap_or_default()
}

fn cash_of(w: &Wired) -> String {
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
        .find(|p| p.instrument_id == CHF)
        .map(|p| read(&p.trade_date_quantity))
        .unwrap_or_else(|| "0".into())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_debt_order_states_its_face_and_its_price_by_its_basis() {
    let w = wired();
    let order = propose(&w, &bond_order("50000")).await.unwrap();
    assert_eq!(order.price_basis, PriceBasis::PercentOfPar as i32);
    // A when-issued Treasury in yield: its limit in limit_yield alone.
    let in_yield = ProposeOrderRequest {
        price_basis: PriceBasis::Yield as i32,
        limit_yield: Some(QuotedYield {
            value: Some(UnitValue {
                value: d("4.105"),
                unit: ValueUnit::Percent as i32,
                ..Default::default()
            }),
            kind: YieldKind::ToMaturity as i32,
            ..Default::default()
        }),
        limit_price: None,
        ..bond_order("50000")
    };
    let order = propose(&w, &in_yield).await.unwrap();
    assert!(order.limit_yield.is_some());
    let cases = [
        (
            ProposeOrderRequest {
                limit_price: usd("99.125"),
                ..in_yield.clone()
            },
            "limit_price",
        ),
        (
            ProposeOrderRequest {
                limit_yield: None,
                ..in_yield.clone()
            },
            "limit_yield",
        ),
        (
            ProposeOrderRequest {
                price_basis: PriceBasis::Spread as i32,
                limit_price: None,
                limit_yield: None,
                limit_spread_bps: d("85"),
                ..bond_order("50000")
            },
            "benchmark_instrument_id",
        ),
        (bond_order("500"), "quantity"),
        (bond_order("50500"), "quantity"),
        (
            ProposeOrderRequest {
                price_basis: PriceBasis::PercentOfPar as i32,
                ..buy("10")
            },
            "price_basis",
        ),
    ];
    for (request, field) in cases {
        let refused = propose(&w, &request).await.unwrap_err();
        assert_eq!(named(&refused), vec![field.to_string()], "{refused}");
    }
}

/// A released debt order placed in full: its placement's ID.
async fn placed(w: &Wired, request: ProposeOrderRequest, quantity: &str) -> (Order, String) {
    waits(w, ACC, false).await;
    let order = propose(w, &request).await.unwrap();
    release(w, &order.order_id).await.unwrap();
    let placement = place(w, &order.order_id, quantity, &person(TRADER))
        .await
        .unwrap()
        .placement
        .unwrap();
    (order, placement.placement_id)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bond_fill_is_priced_per_100_of_face_and_keeps_its_reporting_fields() {
    let w = wired();
    let (_, placement) = placed(&w, bond_order("50000"), "50000").await;
    let mut filled = fill(&placement, "50000", "TRACE-1");
    filled.price = usd("98.375");
    filled.price_basis = PriceBasis::PercentOfPar as i32;
    filled.trade_time_precision = TimePrecision::Second as i32;
    filled.contra_party_type = ContraPartyType::Dealer as i32;
    filled.capacity = Capacity::Principal as i32;
    filled.special_price = true;
    filled.special_price_reason = "priced away from the market".into();
    let mut mixed = filled.clone();
    mixed.capacity = Capacity::Mixed as i32;
    assert_eq!(
        named(&execute(&w, &mixed).await.unwrap_err()),
        vec!["capacity"]
    );
    let mut long_reason = filled.clone();
    long_reason.special_price_reason = "x".repeat(51);
    assert_eq!(
        named(&execute(&w, &long_reason).await.unwrap_err()),
        vec!["special_price_reason"]
    );
    let mut quoted = filled.clone();
    quoted.price_basis = PriceBasis::Yield as i32;
    assert_eq!(
        named(&execute(&w, &quoted).await.unwrap_err()),
        vec!["yield"]
    );
    let execution = execute(&w, &filled).await.unwrap().execution.unwrap();
    assert_eq!(execution.capacity, Capacity::Principal as i32);
    assert_eq!(execution.contra_party_type, ContraPartyType::Dealer as i32);
    assert_eq!(execution.trade_time_precision, TimePrecision::Second as i32);
    assert_eq!(
        execution.special_price_reason,
        "priced away from the market"
    );
    // 50,000 of face at 98.375 per 100 is 49,187.50 (987).
    assert_eq!(cash_of(&w), "-49187.50000");
}

/// The lake, stood in on the bus (contract v29, W10.6): the pool's factors
/// as ListFactors answers them, each row key at its latest version, while
/// `publishing` is set; none before.
fn lake_answering(w: &Wired, publishing: Arc<std::sync::atomic::AtomicBool>) {
    w.bus.serve(crate::service::LIST_FACTORS, move |envelope| {
        let asked = ListFactorsRequest::decode(&envelope.payload[..]).unwrap();
        assert_eq!(asked.subjects[0].entity_id, POOL, "the fill's pool asked");
        let row = |month: &str, factor: &str, version: u64| Factor {
            meta: Some(ObservationMeta {
                row_key: format!("FNMA:MA5123:{month}"),
                version,
                ..Default::default()
            }),
            factor: d(factor),
            factor_month: month.into(),
            ..Default::default()
        };
        let factors = if publishing.load(std::sync::atomic::Ordering::SeqCst) {
            // August's; September's republished, at its version 2; and
            // October's, for a fill settling in November.
            vec![
                row("202608", "0.52", 1),
                row("202609", "0.505", 2),
                row("202610", "0.49", 1),
            ]
        } else {
            Vec::new()
        };
        Ok((
            "meridian.v1.ListFactorsReply".into(),
            ListFactorsReply {
                factors,
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pools_fill_is_at_its_factor_or_the_latest_published_for_its_settlement_month() {
    let w = wired();
    let publishing = Arc::new(std::sync::atomic::AtomicBool::new(false));
    lake_answering(&w, publishing.clone());
    let request = ProposeOrderRequest {
        instrument_id: POOL.into(),
        price_basis: PriceBasis::PercentOfPar as i32,
        limit_price: usd("98.50"),
        ..buy("1000000")
    };
    let (_, placement) = placed(&w, request, "1000000").await;
    let mut filled = fill(&placement, "500000", "POOL-1");
    filled.price = usd("98.00");
    // No factor stated and none published: refused naming the field (986).
    assert_eq!(
        named(&execute(&w, &filled).await.unwrap_err()),
        vec!["factor_used"]
    );
    filled.factor_used = d("0.5");
    let execution = execute(&w, &filled).await.unwrap().execution.unwrap();
    assert_eq!(read(&execution.factor_used), "0.5");
    // 500,000 original face at a factor of 0.5, at 98 per 100: 245,000.
    assert_eq!(cash_of(&w), "-245000.00", "to the cent (993)");

    // Published: a fill settling on 2026-10-14 states none and takes
    // September's, the last month before October's, at its republication
    // (amended at core stage C) -- 500,000 x 0.505 x 0.98 is 247,450. Its
    // factor_used stays as the fill stated it; the journal keeps the factor
    // taken, so a rebuild prices it alike.
    publishing.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut second = fill(&placement, "500000", "POOL-2");
    second.price = usd("98.00");
    let execution = execute(&w, &second).await.unwrap().execution.unwrap();
    assert!(execution.factor_used.is_none(), "as the fill stated it");
    assert_eq!(cash_of(&w), "-492450.00");
    let taken = w
        .store
        .journal(ACC)
        .unwrap()
        .iter()
        .flat_map(|entry| entry.body.acts.iter())
        .filter_map(|act| match act {
            crate::orders::Act::Executed { factor_taken, .. } => Some(factor_taken.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(taken, vec![String::new(), "0.505".to_string()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pools_fill_stating_no_factor_is_refused_to_be_tried_again_when_the_lake_does_not_answer()
{
    let w = wired();
    let request = ProposeOrderRequest {
        instrument_id: POOL.into(),
        price_basis: PriceBasis::PercentOfPar as i32,
        limit_price: usd("98.50"),
        ..buy("1000000")
    };
    let (_, placement) = placed(&w, request, "1000000").await;
    let mut filled = fill(&placement, "1000000", "POOL-1");
    filled.price = usd("98.00");
    let refused = execute(&w, &filled).await.unwrap_err();
    assert!(refused.contains("sent again"), "{refused}");
    assert_eq!(cash_of(&w), "0", "nothing recorded");
}

#[tokio::test(flavor = "multi_thread")]
async fn accrued_interest_is_its_own_cash_line_and_the_coupon_returns_it_before_income() {
    let w = wired();
    // A lifecycle event needs the account's opening balance (W9.16).
    send_from(
        &w.bus,
        RECORD_OPENING_BALANCE,
        "meridian.v1.RecordOpeningBalanceRequest",
        &RecordOpeningBalanceRequest {
            account_id: ACC.into(),
            as_of_date: "2026-10-01".into(),
            sources: vec![OpeningSource {
                kind: OpeningSourceKind::Custodian as i32,
                name: "A custodian".into(),
                as_of_date: "2026-10-01".into(),
                basis: PositionBasis::TradeDate as i32,
                ..Default::default()
            }],
            positions: vec![OpeningPosition {
                instrument_id: CHF.into(),
                side: HoldingSide::Long as i32,
                trade_date_quantity: d("100000.00"),
                settled_quantity: d("100000.00"),
                ..Default::default()
            }],
            reason: "Opening balance".into(),
            replaces_entry_id: String::new(),
            idempotency_key: "opening".into(),
        },
        &person(ADMIN),
    )
    .await
    .unwrap();
    let (order, placement) = placed(&w, bond_order("50000"), "50000").await;
    let mut filled = fill(&placement, "50000", "TRACE-2");
    filled.price = usd("98.375");
    filled.computed_settlement_date = "2026-10-14".into();
    let execution = execute(&w, &filled).await.unwrap().execution.unwrap();
    let allocation = |accrued: Option<Money>| Allocation {
        account_id: ACC.into(),
        quantity: d("50000"),
        average_price: usd("98.375"),
        gross_amount: usd("49187.50"),
        net_amount: usd("49187.50"),
        accrued_interest: accrued,
        intended_settlement_date: "2026-10-14".into(),
        ..Default::default()
    };
    let book = |allocations: Vec<Allocation>| {
        let w = &w;
        let order_id = order.order_id.clone();
        let ids = vec![execution.execution_id.clone()];
        async move {
            send_from(
                &w.bus,
                BOOK_ORDER,
                "meridian.v1.BookOrderRequest",
                &BookOrderRequest {
                    order_id,
                    execution_ids: ids,
                    allocations,
                    hand_off: false,
                },
                &person(TRADER),
            )
            .await
        }
    };
    // 751: a markup with no prevailing market price.
    let refused = book(vec![Allocation {
        markup: usd("625.00"),
        capacity: Capacity::RisklessPrincipal as i32,
        ..allocation(usd("531.25"))
    }])
    .await
    .unwrap_err();
    assert_eq!(
        named(&refused),
        vec!["allocations[0].prevailing_market_price"]
    );
    let refused = book(vec![Allocation {
        capacity: Capacity::Mixed as i32,
        ..allocation(usd("531.25"))
    }])
    .await
    .unwrap_err();
    assert_eq!(named(&refused), vec!["allocations[0].capacity"]);
    let before = cash_of(&w);
    let booked = BookingReply::decode(
        &book(vec![Allocation {
            markup: usd("625.00"),
            prevailing_market_price: usd("98.125"),
            prevailing_market_price_basis: PrevailingMarketPriceBasis::ContemporaneousCost as i32,
            capacity: Capacity::RisklessPrincipal as i32,
            execution_time_ns: NOW,
            execution_time_precision: TimePrecision::Second as i32,
            ..allocation(usd("531.25"))
        }])
        .await
        .unwrap()[..],
    )
    .unwrap()
    .booking
    .unwrap();
    let kept = &booked.allocations[0];
    assert_eq!(
        read(&kept.accrued_interest.as_ref().unwrap().amount),
        "531.25"
    );
    assert_eq!(kept.capacity, Capacity::RisklessPrincipal as i32);
    assert!(kept.prevailing_market_price.is_some());
    // 988: settlement is principal plus accrued, the accrued its own line.
    let paid = |text: &str| text.parse::<Exact>().unwrap();
    assert_eq!(
        paid(&cash_of(&w)),
        numbers::add("cash", paid(&before), paid("-531.25")).unwrap()
    );
    // The coupon over the period the purchase settled in: 1,062.50, of which
    // 531.25 returns the accrued paid and 531.25 is income, each a line.
    let before = cash_of(&w);
    let coupon = send_from(
        &w.bus,
        RECORD_LIFECYCLE_EVENT,
        "meridian.v1.RecordLifecycleEventRequest",
        &RecordLifecycleEventRequest {
            event: Some(LifecycleEvent {
                account_id: ACC.into(),
                instrument_id: BOND.into(),
                kind: LifecycleEventKind::CouponReceived as i32,
                quantity: d("50000"),
                event_date: "2026-11-15".into(),
                period_start_date: "2026-05-15".into(),
                period_end_date: "2026-11-15".into(),
                record_date: "2026-11-01".into(),
                cash: usd("1062.50"),
                external_event_id: "CPN-9".into(),
                ..Default::default()
            }),
            idempotency_key: String::new(),
        },
        &venue(),
    )
    .await;
    coupon.unwrap();
    assert_eq!(
        paid(&cash_of(&w)),
        numbers::add("cash", paid(&before), paid("1062.50")).unwrap()
    );
    // Of it, the accrued the purchase paid, returned: the first line.
    let held = w.store.book(ACC).unwrap();
    let event = LifecycleEvent {
        instrument_id: BOND.into(),
        period_start_date: "2026-05-15".into(),
        period_end_date: "2026-11-15".into(),
        ..Default::default()
    };
    assert_eq!(
        crate::lifecycle::purchased_accrued(&held, &event).unwrap(),
        paid("531.25")
    );
}

/// The lake, stood in on the bus with rows a test publishes as it goes
/// (contract v29, W10.6): each row as ListFactors answers it.
fn lake_holding(w: &Wired) -> Arc<std::sync::Mutex<Vec<Factor>>> {
    let rows: Arc<std::sync::Mutex<Vec<Factor>>> = Arc::default();
    let held = rows.clone();
    w.bus.serve(crate::service::LIST_FACTORS, move |_envelope| {
        Ok((
            "meridian.v1.ListFactorsReply".into(),
            ListFactorsReply {
                factors: held.lock().unwrap().clone(),
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    rows
}

fn factor_row(month: &str, factor: &str, version: u64) -> Factor {
    Factor {
        meta: Some(ObservationMeta {
            row_key: format!("FNMA:MA5123:{month}"),
            version,
            ..Default::default()
        }),
        factor: d(factor),
        factor_month: month.into(),
        payment_date: "2025-11-25".into(),
        ..Default::default()
    }
}

/// The book's factor sweep, as its blocking task runs it.
fn swept(w: &Wired) -> usize {
    tokio::task::block_in_place(|| crate::factors::sweep(&w.bus, w.store.as_ref()).unwrap())
}

fn pool_lots(w: &Wired) -> Vec<Lot> {
    position(w, POOL, HoldingSide::Long)
        .map(|held| held.lots)
        .unwrap_or_default()
}

fn revisions(w: &Wired) -> Vec<LifecycleEvent> {
    let mut events: Vec<LifecycleEvent> = w
        .store
        .book(ACC)
        .unwrap()
        .lifecycle_events
        .into_values()
        .filter(|event| event.kind == LifecycleEventKind::FactorRevision as i32)
        .collect();
    events.sort_by(|a, b| a.event_id.cmp(&b.event_id));
    events
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pools_fill_before_its_months_factor_is_provisional_then_revised_then_claimed() {
    // Rulings 991 to 993 (the clock at 2025-10-13): a fill settling in
    // November takes October's
    // factor; unpublished, September's, provisional, until the book revises
    // it; after settlement the difference is a claim a person marks settled.
    let w = wired();
    let rows = lake_holding(&w);
    rows.lock().unwrap().push(factor_row("202509", "0.505", 1));
    let request = ProposeOrderRequest {
        instrument_id: POOL.into(),
        price_basis: PriceBasis::PercentOfPar as i32,
        limit_price: usd("98.50"),
        ..buy("1000000")
    };
    let (order, placement) = placed(&w, request, "1000000").await;
    let mut filled = fill(&placement, "500000", "POOL-1");
    filled.price = usd("98.00");
    filled.settlement_date = "2025-11-12".into();
    let execution = execute(&w, &filled).await.unwrap().execution.unwrap();
    assert_eq!(read(&execution.factor_taken), "0.505");
    assert_eq!(execution.factor_month_awaited, "202510", "provisional");
    assert!(execution.factor_used.is_none());
    // 500,000 x 0.98 x 0.505, to the cent (993).
    assert_eq!(cash_of(&w), "-247450.00");
    let lots = pool_lots(&w);
    assert_eq!(lots.len(), 1);
    assert_eq!(lots[0].factor_month_awaited, "202510");
    assert_eq!(
        read(
            &lots[0]
                .terms
                .as_ref()
                .unwrap()
                .cost
                .as_ref()
                .unwrap()
                .amount
        ),
        "247450.00",
        "the lot's cost is the rounded cash"
    );
    // A booking of it opens its lot marked too.
    let booked = send_from(
        &w.bus,
        BOOK_ORDER,
        "meridian.v1.BookOrderRequest",
        &BookOrderRequest {
            order_id: order.order_id.clone(),
            execution_ids: vec![execution.execution_id.clone()],
            allocations: vec![Allocation {
                account_id: ACC.into(),
                quantity: d("500000"),
                average_price: usd("98.00"),
                gross_amount: usd("247450.00"),
                net_amount: usd("247450.00"),
                ..Default::default()
            }],
            hand_off: false,
        },
        &person(TRADER),
    )
    .await
    .unwrap();
    let booking = BookingReply::decode(&booked[..]).unwrap().booking.unwrap();
    assert!(pool_lots(&w)
        .iter()
        .all(|lot| lot.factor_month_awaited == "202510"));

    // Nothing published: nothing revised.
    assert_eq!(swept(&w), 0);
    // October's published: the book's own revision, before settlement.
    rows.lock().unwrap().push(factor_row("202510", "0.49", 1));
    assert_eq!(swept(&w), 1);
    assert_eq!(swept(&w), 0);
    // 500,000 x 0.98 x 0.49 is 240,100.00: 7,350.00 back, pending to the
    // settlement date.
    assert_eq!(cash_of(&w), "-240100.00");
    let revised = revisions(&w);
    assert_eq!(revised.len(), 1);
    let first = &revised[0];
    assert_eq!(first.state, LifecycleEventState::Booked as i32);
    assert_eq!(first.execution_id, execution.execution_id);
    assert_eq!(read(&first.prior_factor), "0.505");
    assert_eq!(read(&first.factor), "0.49");
    assert_eq!(read(&first.cash.as_ref().unwrap().amount), "7350.00");
    assert_eq!(
        read(&first.cost_change.as_ref().unwrap().amount),
        "-7350.00"
    );
    let book = w.store.book(ACC).unwrap();
    let held = &book.ledger.executions[&execution.execution_id];
    assert_eq!(held.factor_taken, "0.49");
    assert!(held.execution.factor_month_awaited.is_empty(), "final");
    // The booking is held: its correction restates it (W11.27, W11.29).
    assert_eq!(
        book.ledger.bookings[&booking.booking_id].state,
        BookingState::Held
    );
    drop(book);

    // After settlement, a republication: the difference an open claim.
    w.clock.advance(31 * 24 * 3_600 * 1_000_000_000);
    rows.lock().unwrap().push(factor_row("202510", "0.4901", 2));
    assert_eq!(swept(&w), 1);
    let all = revisions(&w);
    let claim = all
        .iter()
        .find(|event| event.state == LifecycleEventState::ClaimOpen as i32)
        .cloned()
        .unwrap_or_else(|| panic!("{all:?}"));
    // 500,000 x 0.98 x 0.0001: 49.00 more paid.
    assert_eq!(read(&claim.cash.as_ref().unwrap().amount), "-49.00");
    let pending = position(&w, CHF, HoldingSide::Long).unwrap().pending;
    assert!(
        pending
            .iter()
            .any(|p| p.value_date.is_empty() && read(&p.quantity) == "-49.00"),
        "{pending:?}"
    );
    let refused = send_from(
        &w.bus,
        crate::service::RELEASE_LIFECYCLE_EVENT,
        "meridian.v1.ReleaseLifecycleEventRequest",
        &ReleaseLifecycleEventRequest {
            event_id: claim.event_id.clone(),
            note: " ".into(),
        },
        &person(ADMIN),
    )
    .await
    .unwrap_err();
    assert!(refused.contains("why"), "{refused}");
    send_from(
        &w.bus,
        crate::service::RELEASE_LIFECYCLE_EVENT,
        "meridian.v1.ReleaseLifecycleEventRequest",
        &ReleaseLifecycleEventRequest {
            event_id: claim.event_id.clone(),
            note: "the counterparty paid the claim".into(),
        },
        &person(ADMIN),
    )
    .await
    .unwrap();
    let settled = &w.store.book(ACC).unwrap().lifecycle_events[&claim.event_id];
    assert_eq!(settled.state, LifecycleEventState::Booked as i32);
    let pending = position(&w, CHF, HoldingSide::Long).unwrap().pending;
    assert!(
        pending.iter().all(|p| !p.value_date.is_empty()),
        "{pending:?}"
    );
    // Marked settled once: a second is refused naming it.
    let again = send_from(
        &w.bus,
        crate::service::RELEASE_LIFECYCLE_EVENT,
        "meridian.v1.ReleaseLifecycleEventRequest",
        &ReleaseLifecycleEventRequest {
            event_id: claim.event_id.clone(),
            note: "again".into(),
        },
        &person(ADMIN),
    )
    .await
    .unwrap_err();
    assert_eq!(named(&again), vec!["event_id"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_booking_of_fills_stating_conflicting_factors_is_refused_naming_them() {
    // Ruling 991: no allocation field carries a factor.
    let w = wired();
    let request = ProposeOrderRequest {
        instrument_id: POOL.into(),
        price_basis: PriceBasis::PercentOfPar as i32,
        limit_price: usd("98.50"),
        ..buy("1000000")
    };
    let (order, placement) = placed(&w, request, "1000000").await;
    let mut ids = Vec::new();
    for (venue_id, factor) in [("POOL-1", "0.5"), ("POOL-2", "0.51")] {
        let mut filled = fill(&placement, "500000", venue_id);
        filled.price = usd("98.00");
        filled.factor_used = d(factor);
        ids.push(
            execute(&w, &filled)
                .await
                .unwrap()
                .execution
                .unwrap()
                .execution_id,
        );
    }
    let refused = send_from(
        &w.bus,
        BOOK_ORDER,
        "meridian.v1.BookOrderRequest",
        &BookOrderRequest {
            order_id: order.order_id.clone(),
            execution_ids: ids,
            allocations: vec![Allocation {
                account_id: ACC.into(),
                quantity: d("1000000"),
                average_price: usd("98.00"),
                gross_amount: usd("494900.00"),
                net_amount: usd("494900.00"),
                ..Default::default()
            }],
            hand_off: false,
        },
        &person(TRADER),
    )
    .await
    .unwrap_err();
    assert_eq!(
        named(&refused),
        vec!["execution_ids[1]".to_string(), "factor_used".to_string()]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fill_whose_settlement_date_is_unknown_keeps_its_cash_pending_undated() {
    // Contract v29 (W11.11, amended at core stage E): never settled.
    let w = wired();
    let (_, placement) = placed(&w, bond_order("50000"), "50000").await;
    let mut filled = fill(&placement, "50000", "WI-1");
    filled.price = usd("98.375");
    filled.computed_settlement_date = String::new();
    filled.settlement_date_unknown = true;
    execute(&w, &filled).await.unwrap();
    let cash = position(&w, CHF, HoldingSide::Long).unwrap();
    assert_eq!(read(&cash.settled_quantity), "0");
    assert!(
        cash.pending
            .iter()
            .any(|p| p.value_date.is_empty() && read(&p.quantity) == "-49187.50000"),
        "{:?}",
        cash.pending
    );
}
