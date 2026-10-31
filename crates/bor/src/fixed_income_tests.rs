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
    status_of(detail).map(|(_, fields)| fields).unwrap_or_default()
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
    assert_eq!(execution.special_price_reason, "priced away from the market");
    // 50,000 of face at 98.375 per 100 is 49,187.50 (987).
    assert_eq!(cash_of(&w), "-49187.50000");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pools_fill_states_its_factor_and_its_cash_is_original_face_times_it() {
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
    assert_eq!(
        named(&execute(&w, &filled).await.unwrap_err()),
        vec!["factor_used"]
    );
    filled.factor_used = d("0.5");
    let execution = execute(&w, &filled).await.unwrap().execution.unwrap();
    assert_eq!(read(&execution.factor_used), "0.5");
    // 1,000,000 original face at a factor of 0.5, at 98 per 100: 490,000.
    assert_eq!(cash_of(&w), "-490000.00000");
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
    assert_eq!(read(&kept.accrued_interest.as_ref().unwrap().amount), "531.25");
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
