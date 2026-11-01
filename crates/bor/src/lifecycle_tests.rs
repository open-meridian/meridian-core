//! A listed option's and a future's lifecycle in the book (contract v28;
//! W9.16 to W9.19; rulings 562 and 563), over the bus with the in-memory
//! store and the instrument store stood in: an exercise closing the option's
//! lots and opening the underlying's with the premium in their cost, an
//! expiry closing lots at nothing, a day's variation a cash line alone, a
//! proposal answered by the report and one released with a note, each its
//! own record and heard after it commits; and what W9.16 refuses.

use std::sync::Arc;

use meridian_bus::{Bus, MemoryBackend, Stamp};
use meridian_domain::exact::Exact;
use meridian_domain::v1::*;
use meridian_pb::v1::RefusalReason;
use prost::Message;

use crate::service::*;
use crate::store::{PositionsRead, Scope, Store};
use crate::MemoryStore;

const PERSON: &str = "https://directory.example.org|8812";
const ACC: &str = "ACC-1";
const AAPL: &str = "INS-01J8XQ4M7K0000000000AAPL";
const CALL: &str = "INS-01J8XQ4M7K00000AAPLC230";
const ES: &str = "INS-01J8XQ4M7K0000000000ESZ6";
const USD: &str = "INS-01J8XQ4M7K00000000CASHUSD";
// Contract v29: debt records.
const CORP: &str = "INS-01JC29CORP000000000000001";
const POOL: &str = "INS-01JC29POOL000000000000001";
const POOLA: &str = "INS-01JC29POOLA00000000000001";
const POOLB: &str = "INS-01JC29POOLB00000000000001";
const POOL5: &str = "INS-01JC29POOL5PCT0000000001";
const TBA: &str = "INS-01JC29TBA0000000000000001";
const CMOZ: &str = "INS-01JC29CMOZ000000000000001";
const TSY: &str = "INS-01JC29TSY0000000000000001";
const TINT: &str = "INS-01JC29TINT000000000000001";
const TPRN: &str = "INS-01JC29TPRN000000000000001";

fn d(text: &str) -> Option<meridian_pb::v1::Decimal> {
    Some(text.parse::<Exact>().unwrap().to_wire())
}

fn usd(text: &str) -> Option<Money> {
    Some(Money {
        amount: d(text),
        currency_code: "USD".into(),
        instrument_id: USD.into(),
    })
}

fn wired() -> (Arc<Bus>, Arc<MemoryStore>) {
    let bus = Arc::new(Bus::single(
        "custody-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::ManualClock::at(1_757_462_400_000_000_000)),
    ));
    let store = Arc::new(MemoryStore::new());
    serve(bus.clone(), store.clone(), bus.clock());
    bus.serve(RESOLVE_INSTRUMENT, |envelope| {
        let asked = ResolveInstrumentRequest::decode(&envelope.payload[..]).unwrap();
        let record = |class: AssetClass, kind: InstrumentType| InstrumentRecord {
            instrument_id: asked.instrument_id.clone(),
            asset_class: class as i32,
            instrument_type: kind as i32,
            currency: "USD".into(),
            version: 1,
            ..Default::default()
        };
        let pool = |rate: &str| MbsPool {
            agency: MortgageAgency::Fnma as i32,
            program: "CL".into(),
            pass_through_rate: d(rate),
            ..Default::default()
        };
        let stripped = |from: &str| Bond {
            stripped_from: from.into(),
            ..Default::default()
        };
        let instrument = match asked.instrument_id.as_str() {
            CORP | TSY => Some(InstrumentRecord {
                bond: Some(Bond::default()),
                ..record(AssetClass::Debt, InstrumentType::Bond)
            }),
            TINT | TPRN => Some(InstrumentRecord {
                bond: Some(stripped(TSY)),
                ..record(AssetClass::Debt, InstrumentType::Bond)
            }),
            POOL | POOLA | POOLB => Some(InstrumentRecord {
                mbs_pool: Some(pool("5.5")),
                ..record(AssetClass::Debt, InstrumentType::MbsPool)
            }),
            POOL5 => Some(InstrumentRecord {
                mbs_pool: Some(pool("5.0")),
                ..record(AssetClass::Debt, InstrumentType::MbsPool)
            }),
            TBA => Some(InstrumentRecord {
                tba: Some(Tba {
                    agency: MortgageAgency::Fnma as i32,
                    program: "CL".into(),
                    pass_through_rate: d("5.5"),
                    ..Default::default()
                }),
                ..record(AssetClass::Debt, InstrumentType::Tba)
            }),
            CMOZ => Some(InstrumentRecord {
                cmo_tranche: Some(CmoTranche::default()),
                ..record(AssetClass::Debt, InstrumentType::CmoTranche)
            }),
            AAPL => Some(record(AssetClass::Equity, InstrumentType::Unspecified)),
            CALL => Some(record(AssetClass::Derivative, InstrumentType::ListedOption)),
            ES => Some(record(AssetClass::Derivative, InstrumentType::Future)),
            USD => Some(record(AssetClass::Cash, InstrumentType::Unspecified)),
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
    (bus, store)
}

async fn send<M: Message>(
    bus: &Bus,
    topic: &str,
    payload_type: &str,
    message: &M,
    person: Option<&str>,
) -> Result<Vec<u8>, String> {
    let stamp = Stamp {
        acting_for_subject: person.unwrap_or_default().to_string(),
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

fn lot(quantity: &str, cost: &str) -> OpeningLot {
    OpeningLot {
        quantity: d(quantity),
        terms: Some(LotTerms {
            cost: usd(cost),
            acquired_date: "2026-08-01".into(),
            source: LotSource::OpeningBalance as i32,
            ..Default::default()
        }),
    }
}

fn position(instrument: &str, side: HoldingSide, quantity: &str, cost: &str) -> OpeningPosition {
    let signed = if side == HoldingSide::Short {
        format!("-{quantity}")
    } else {
        quantity.to_string()
    };
    OpeningPosition {
        instrument_id: instrument.into(),
        side: side as i32,
        trade_date_quantity: d(&signed),
        settled_quantity: d(&signed),
        lots: vec![lot(&signed, cost)],
        ..Default::default()
    }
}

/// Two long calls bought for 820 USD, one long future, 50,000 USD.
async fn open(bus: &Bus) {
    send(
        bus,
        RECORD_OPENING_BALANCE,
        "meridian.v1.RecordOpeningBalanceRequest",
        &RecordOpeningBalanceRequest {
            account_id: ACC.into(),
            as_of_date: "2026-09-08".into(),
            sources: vec![OpeningSource {
                kind: OpeningSourceKind::Custodian as i32,
                name: "A broker".into(),
                as_of_date: "2026-09-08".into(),
                basis: PositionBasis::TradeDate as i32,
                ..Default::default()
            }],
            positions: vec![
                position(CALL, HoldingSide::Long, "2", "820.00"),
                position(ES, HoldingSide::Long, "1", "290000.00"),
                position(USD, HoldingSide::Long, "50000.00", "50000.00"),
            ],
            reason: "Opening balance".into(),
            replaces_entry_id: String::new(),
            idempotency_key: "opening".into(),
        },
        Some(PERSON),
    )
    .await
    .unwrap();
}

fn exercise() -> LifecycleEvent {
    LifecycleEvent {
        account_id: ACC.into(),
        instrument_id: CALL.into(),
        kind: LifecycleEventKind::Exercise as i32,
        quantity: d("2"),
        event_date: "2026-11-20".into(),
        deliveries: vec![LifecycleDelivery {
            instrument_id: AAPL.into(),
            quantity: d("200"),
            price: usd("230.00"),
        }],
        cash: usd("-46000.00"),
        external_event_id: "EX-1".into(),
        ..Default::default()
    }
}

async fn record(bus: &Bus, event: LifecycleEvent) -> Result<LifecycleEvent, String> {
    send(
        bus,
        RECORD_LIFECYCLE_EVENT,
        "meridian.v1.RecordLifecycleEventRequest",
        &RecordLifecycleEventRequest {
            event: Some(event),
            idempotency_key: String::new(),
        },
        None,
    )
    .await
    .map(|reply| {
        RecordLifecycleEventReply::decode(&reply[..])
            .unwrap()
            .event
            .unwrap()
    })
}

fn held(store: &MemoryStore, instrument: &str, side: HoldingSide) -> Option<BookPosition> {
    store
        .positions(&PositionsRead {
            scope: Scope::Everything,
            account_id: ACC.into(),
            since: None,
            at: None,
            business_date: String::new(),
            limit: 100,
            cursor: String::new(),
        })
        .unwrap()
        .records
        .into_iter()
        .find(|p| p.instrument_id == instrument && p.side == side as i32)
}

fn quantity(position: &Option<BookPosition>) -> String {
    position
        .as_ref()
        .and_then(|p| p.trade_date_quantity.as_ref())
        .map(|q| Exact::from_wire(q).unwrap().to_string())
        .unwrap_or_else(|| "0".into())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_exercise_closes_the_calls_and_opens_the_shares_with_the_premium_in_their_cost() {
    let (bus, store) = wired();
    open(&bus).await;
    let mut heard = bus.subscribe_every(LIFECYCLE_EVENT_RECORDED);
    let booked = record(&bus, exercise()).await.unwrap();
    assert!(booked.event_id.starts_with("LCE-"));
    assert_eq!(booked.state, LifecycleEventState::Booked as i32);
    assert_eq!(quantity(&held(&store, CALL, HoldingSide::Long)), "0");
    let shares = held(&store, AAPL, HoldingSide::Long).unwrap();
    assert_eq!(quantity(&Some(shares.clone())), "200");
    // 200 at the strike, 46,000, and the 820 the calls cost.
    let cost = shares.lots[0]
        .terms
        .as_ref()
        .unwrap()
        .cost
        .as_ref()
        .unwrap()
        .amount;
    assert_eq!(
        Exact::from_wire(&cost.unwrap()).unwrap(),
        "46820".parse::<Exact>().unwrap()
    );
    assert_eq!(quantity(&held(&store, USD, HoldingSide::Long)), "4000.00");
    let event =
        LifecycleEventRecordedEvent::decode(&heard.recv().await.unwrap().envelope.payload[..])
            .unwrap();
    assert_eq!(event.event.unwrap().event_id, booked.event_id);
    assert!(event.journal.is_some() && event.cause.is_some());
    // Sent twice: answered with the first, nothing booked again.
    let again = record(&bus, exercise()).await.unwrap();
    assert_eq!(again.event_id, booked.event_id);
    assert_eq!(quantity(&held(&store, AAPL, HoldingSide::Long)), "200");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_proposal_is_answered_by_the_report_and_a_variation_released_with_a_note() {
    let (bus, store) = wired();
    open(&bus).await;
    let expiry = LifecycleEvent {
        kind: LifecycleEventKind::Expiry as i32,
        deliveries: vec![],
        cash: None,
        external_event_id: String::new(),
        proposal_basis: usd("228.00"),
        ..exercise()
    };
    let variation = LifecycleEvent {
        instrument_id: ES.into(),
        kind: LifecycleEventKind::SettlementVariation as i32,
        quantity: d("1"),
        event_date: "2026-09-09".into(),
        deliveries: vec![],
        cash: usd("-625.00"),
        settlement_price: usd("5787.50"),
        external_event_id: String::new(),
        ..exercise()
    };
    let proposed = ProposeLifecycleEventsReply::decode(
        &send(
            &bus,
            PROPOSE_LIFECYCLE_EVENTS,
            "meridian.v1.ProposeLifecycleEventsRequest",
            &ProposeLifecycleEventsRequest {
                events: vec![expiry.clone(), variation],
            },
            None,
        )
        .await
        .unwrap()[..],
    )
    .unwrap()
    .events;
    assert_eq!(proposed.len(), 2);
    assert!(proposed
        .iter()
        .all(|e| e.state == LifecycleEventState::Proposed as i32));
    // Proposing moved nothing.
    assert_eq!(quantity(&held(&store, CALL, HoldingSide::Long)), "2");
    // The broker's expiry answers the first.
    let reported = record(
        &bus,
        LifecycleEvent {
            external_event_id: "EXP-1".into(),
            proposal_basis: None,
            ..expiry
        },
    )
    .await
    .unwrap();
    assert_eq!(reported.answers_event_id, proposed[0].event_id);
    assert_eq!(quantity(&held(&store, CALL, HoldingSide::Long)), "0");
    // No report for the variation: released, a note required.
    let release = |note: &str| ReleaseLifecycleEventRequest {
        event_id: proposed[1].event_id.clone(),
        note: note.into(),
    };
    let refused = send(
        &bus,
        RELEASE_LIFECYCLE_EVENT,
        "meridian.v1.ReleaseLifecycleEventRequest",
        &release(" "),
        Some(PERSON),
    )
    .await
    .unwrap_err();
    assert_eq!(
        meridian_bus::read_refusal(&refused).and_then(|(r, _)| RefusalReason::try_from(r).ok()),
        Some(RefusalReason::ReasonRequired)
    );
    let released = ReleaseLifecycleEventReply::decode(
        &send(
            &bus,
            RELEASE_LIFECYCLE_EVENT,
            "meridian.v1.ReleaseLifecycleEventRequest",
            &release("the FCM reports variation only as cash"),
            Some(PERSON),
        )
        .await
        .unwrap()[..],
    )
    .unwrap()
    .event
    .unwrap();
    assert_eq!(released.state, LifecycleEventState::Booked as i32);
    assert_eq!(quantity(&held(&store, USD, HoldingSide::Long)), "49375.00");
    // The future's lot keeps its trade price: the variation is a cash line.
    assert_eq!(quantity(&held(&store, ES, HoldingSide::Long)), "1");
    // Read back: four records, two answered, two booked.
    let listed = ListLifecycleEventsReply::decode(
        &send(
            &bus,
            LIST_LIFECYCLE_EVENTS,
            "meridian.v1.ListLifecycleEventsRequest",
            &ListLifecycleEventsRequest {
                account_id: ACC.into(),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap()[..],
    )
    .unwrap();
    assert_eq!(listed.events.len(), 4);
    assert_eq!(
        listed
            .events
            .iter()
            .filter(|e| e.state == LifecycleEventState::Answered as i32)
            .count(),
        2
    );
    // Contract v29 (ruling 740; W9.19): naming no account answers every
    // account in the reader's scope, each event naming its account.
    let every = ListLifecycleEventsReply::decode(
        &send(
            &bus,
            LIST_LIFECYCLE_EVENTS,
            "meridian.v1.ListLifecycleEventsRequest",
            &ListLifecycleEventsRequest::default(),
            None,
        )
        .await
        .unwrap()[..],
    )
    .unwrap();
    assert_eq!(every.events, listed.events);
    assert!(every.events.iter().all(|e| e.account_id == ACC));
    assert!(every.as_of.is_some_and(|mark| !mark.partitions.is_empty()));
}

#[tokio::test(flavor = "multi_thread")]
async fn what_w9_16_refuses_is_refused_naming_the_field() {
    let (bus, _store) = wired();
    // No opening balance.
    let refused = record(&bus, exercise()).await.unwrap_err();
    assert_eq!(
        meridian_bus::read_refusal(&refused).and_then(|(r, _)| RefusalReason::try_from(r).ok()),
        Some(RefusalReason::NoOpeningBalance)
    );
    open(&bus).await;
    let cases = [
        (
            LifecycleEvent {
                instrument_id: AAPL.into(),
                ..exercise()
            },
            "event.instrument_id",
        ),
        (
            LifecycleEvent {
                instrument_id: ES.into(),
                ..exercise()
            },
            "event.kind",
        ),
        (
            LifecycleEvent {
                kind: LifecycleEventKind::SettlementVariation as i32,
                ..exercise()
            },
            "event.kind",
        ),
        (
            LifecycleEvent {
                quantity: d("3"),
                ..exercise()
            },
            "quantity",
        ),
        (
            LifecycleEvent {
                instrument_id: ES.into(),
                kind: LifecycleEventKind::SettlementVariation as i32,
                deliveries: vec![],
                settlement_price: None,
                ..exercise()
            },
            "event.settlement_price",
        ),
    ];
    for (event, field) in cases {
        let refused = record(&bus, event).await.unwrap_err();
        let named = meridian_bus::read_refusal_status(&refused)
            .map(|(_, fields, _)| fields)
            .unwrap_or_default();
        assert!(named.contains(&field.to_string()), "{field}: {refused}");
    }
}

// ── Contract v29: a debt record's seven records (W9.16 to W9.18) ──────────

/// Face held: a corporate, a pool, a TBA, a Z class and a whole Treasury,
/// each with its cost; 10,000 USD.
async fn open_debt(bus: &Bus) {
    send(
        bus,
        RECORD_OPENING_BALANCE,
        "meridian.v1.RecordOpeningBalanceRequest",
        &RecordOpeningBalanceRequest {
            account_id: ACC.into(),
            as_of_date: "2026-09-08".into(),
            sources: vec![OpeningSource {
                kind: OpeningSourceKind::Custodian as i32,
                name: "A custodian".into(),
                as_of_date: "2026-09-08".into(),
                basis: PositionBasis::TradeDate as i32,
                ..Default::default()
            }],
            positions: vec![
                position(CORP, HoldingSide::Long, "50000", "49500.00"),
                position(POOL, HoldingSide::Long, "1000000", "825271.48"),
                position(TBA, HoldingSide::Long, "2000000", "1970000.00"),
                position(CMOZ, HoldingSide::Long, "500000", "480000.00"),
                position(TSY, HoldingSide::Long, "100000", "99000.00"),
                position(USD, HoldingSide::Long, "10000.00", "10000.00"),
            ],
            reason: "Opening balance".into(),
            replaces_entry_id: String::new(),
            idempotency_key: "opening-debt".into(),
        },
        Some(PERSON),
    )
    .await
    .unwrap();
}

fn debt_event(instrument: &str, kind: LifecycleEventKind, quantity: &str) -> LifecycleEvent {
    LifecycleEvent {
        account_id: ACC.into(),
        instrument_id: instrument.into(),
        kind: kind as i32,
        quantity: d(quantity),
        event_date: "2026-11-25".into(),
        ..Default::default()
    }
}

fn costs(position: &Option<BookPosition>) -> Vec<(String, String)> {
    position
        .as_ref()
        .map(|p| {
            p.lots
                .iter()
                .filter(|lot| {
                    lot.open_quantity
                        .as_ref()
                        .is_some_and(|q| !Exact::from_wire(q).unwrap().is_zero())
                })
                .map(|lot| {
                    let open = Exact::from_wire(lot.open_quantity.as_ref().unwrap()).unwrap();
                    let cost = lot
                        .terms
                        .as_ref()
                        .and_then(|t| t.cost.as_ref())
                        .and_then(|c| c.amount.as_ref())
                        .map(|a| Exact::from_wire(a).unwrap().to_string())
                        .unwrap_or_default();
                    (open.to_string(), cost)
                })
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_coupon_moves_cash_a_partial_call_reduces_lots_and_an_accretion_moves_nothing() {
    let (bus, store) = wired();
    open_debt(&bus).await;
    let coupon = record(
        &bus,
        LifecycleEvent {
            period_start_date: "2026-05-15".into(),
            period_end_date: "2026-11-15".into(),
            record_date: "2026-11-01".into(),
            cash: usd("1062.50"),
            external_event_id: "CPN-1".into(),
            ..debt_event(CORP, LifecycleEventKind::CouponReceived, "50000")
        },
    )
    .await
    .unwrap();
    assert_eq!(coupon.state, LifecycleEventState::Booked as i32);
    assert_eq!(quantity(&held(&store, USD, HoldingSide::Long)), "11062.50");
    assert_eq!(quantity(&held(&store, CORP, HoldingSide::Long)), "50000");
    // A partial call by lottery: 15,000 of face at par, the lots reduced.
    record(
        &bus,
        LifecycleEvent {
            redemption_kind: RedemptionEventKind::PartialCall as i32,
            redemption_price: d("100"),
            cash: usd("15000.00"),
            external_event_id: "CALL-1".into(),
            ..debt_event(CORP, LifecycleEventKind::Redemption, "15000")
        },
    )
    .await
    .unwrap();
    assert_eq!(quantity(&held(&store, CORP, HoldingSide::Long)), "35000");
    assert_eq!(quantity(&held(&store, USD, HoldingSide::Long)), "26062.50");
    // A maturity with no cash stated: face times the price per 100 of face.
    record(
        &bus,
        LifecycleEvent {
            redemption_kind: RedemptionEventKind::Maturity as i32,
            redemption_price: d("100"),
            external_event_id: "MAT-1".into(),
            ..debt_event(CORP, LifecycleEventKind::Redemption, "35000")
        },
    )
    .await
    .unwrap();
    assert_eq!(quantity(&held(&store, CORP, HoldingSide::Long)), "0");
    assert_eq!(quantity(&held(&store, USD, HoldingSide::Long)), "61062.50");
    // A Z class's accretion: no cash, no lot.
    let accretion = record(
        &bus,
        LifecycleEvent {
            prior_factor: d("1.02150000"),
            factor: d("1.02520000"),
            face_change: d("1850.00"),
            external_event_id: "ACC-1".into(),
            ..debt_event(CMOZ, LifecycleEventKind::Accretion, "500000")
        },
    )
    .await
    .unwrap();
    assert_eq!(accretion.state, LifecycleEventState::Booked as i32);
    assert_eq!(
        costs(&held(&store, CMOZ, HoldingSide::Long)),
        vec![("500000".to_string(), "480000.00".to_string())]
    );
    assert_eq!(quantity(&held(&store, USD, HoldingSide::Long)), "61062.50");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paydown_keeps_original_face_and_relieves_cost_pro_rata() {
    let (bus, store) = wired();
    open_debt(&bus).await;
    let paydown = LifecycleEvent {
        record_date: "2026-10-31".into(),
        prior_factor: d("0.84211375"),
        factor: d("0.83102210"),
        ..debt_event(POOL, LifecycleEventKind::Paydown, "1000000")
    };
    // servicing proposes it from the Factor row; no report comes; released.
    let proposed = ProposeLifecycleEventsReply::decode(
        &send(
            &bus,
            PROPOSE_LIFECYCLE_EVENTS,
            "meridian.v1.ProposeLifecycleEventsRequest",
            &ProposeLifecycleEventsRequest {
                events: vec![paydown],
            },
            None,
        )
        .await
        .unwrap()[..],
    )
    .unwrap()
    .events;
    send(
        &bus,
        RELEASE_LIFECYCLE_EVENT,
        "meridian.v1.ReleaseLifecycleEventRequest",
        &ReleaseLifecycleEventRequest {
            event_id: proposed[0].event_id.clone(),
            note: "the custodian reports paydowns only as cash".into(),
        },
        Some(PERSON),
    )
    .await
    .unwrap();
    // Original face kept; cash the fall times original face; the cost
    // 825,271.48 relieved by 0.01109165 / 0.84211375 of it, 10,869.82.
    assert_eq!(
        costs(&held(&store, POOL, HoldingSide::Long)),
        vec![("1000000".to_string(), "814401.66".to_string())]
    );
    assert_eq!(quantity(&held(&store, USD, HoldingSide::Long)), "21091.65");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tba_allocation_and_a_strip_carry_the_cost_to_the_lots_they_open() {
    let (bus, store) = wired();
    open_debt(&bus).await;
    let delivery = |instrument: &str, face: &str, price: Option<&str>| LifecycleDelivery {
        instrument_id: instrument.into(),
        quantity: d(face),
        price: price.and_then(usd),
    };
    // A pool of another coupon is no good delivery.
    let refused = record(
        &bus,
        LifecycleEvent {
            deliveries: vec![delivery(POOL5, "2000000", None)],
            ..debt_event(TBA, LifecycleEventKind::TbaAllocation, "2000000")
        },
    )
    .await
    .unwrap_err();
    let named = meridian_bus::read_refusal_status(&refused)
        .map(|(_, fields, _)| fields)
        .unwrap_or_default();
    assert!(named.contains(&"event.deliveries".to_string()), "{refused}");
    record(
        &bus,
        LifecycleEvent {
            deliveries: vec![
                delivery(POOLA, "800000", None),
                delivery(POOLB, "700000", None),
                delivery(POOL, "500000", None),
            ],
            external_event_id: "TBA-1".into(),
            ..debt_event(TBA, LifecycleEventKind::TbaAllocation, "2000000")
        },
    )
    .await
    .unwrap();
    assert_eq!(quantity(&held(&store, TBA, HoldingSide::Long)), "0");
    assert_eq!(
        costs(&held(&store, POOLA, HoldingSide::Long)),
        vec![("800000".to_string(), "788000.00".to_string())]
    );
    assert_eq!(
        costs(&held(&store, POOLB, HoldingSide::Long)),
        vec![("700000".to_string(), "689500.00".to_string())]
    );
    assert_eq!(quantity(&held(&store, POOL, HoldingSide::Long)), "1500000");
    // A strip: the basis allocated by relative value, 1.9 to 8.0.
    record(
        &bus,
        LifecycleEvent {
            deliveries: vec![
                delivery(TINT, "20000", Some("95")),
                delivery(TPRN, "100000", Some("80")),
            ],
            external_event_id: "STRIP-1".into(),
            ..debt_event(TSY, LifecycleEventKind::Strip, "100000")
        },
    )
    .await
    .unwrap();
    assert_eq!(quantity(&held(&store, TSY, HoldingSide::Long)), "0");
    assert_eq!(
        costs(&held(&store, TINT, HoldingSide::Long)),
        vec![("20000".to_string(), "19000.00".to_string())]
    );
    assert_eq!(
        costs(&held(&store, TPRN, HoldingSide::Long)),
        vec![("100000".to_string(), "80000.00".to_string())]
    );
    // And back: the whole bond's lot opened with both components' cost.
    record(
        &bus,
        LifecycleEvent {
            deliveries: vec![
                delivery(TINT, "-20000", Some("95")),
                delivery(TPRN, "-100000", Some("80")),
            ],
            external_event_id: "RECON-1".into(),
            ..debt_event(TSY, LifecycleEventKind::Reconstitution, "100000")
        },
    )
    .await
    .unwrap();
    assert_eq!(
        costs(&held(&store, TSY, HoldingSide::Long)),
        vec![("100000".to_string(), "99000.00".to_string())]
    );
    assert_eq!(quantity(&held(&store, TINT, HoldingSide::Long)), "0");
}

#[tokio::test(flavor = "multi_thread")]
async fn what_w9_16_refuses_of_a_debt_record_is_refused_naming_the_field() {
    let (bus, _store) = wired();
    open_debt(&bus).await;
    let cases = [
        (
            LifecycleEvent {
                prior_factor: d("0.9"),
                factor: d("0.8"),
                ..debt_event(CORP, LifecycleEventKind::Paydown, "50000")
            },
            "event.kind",
        ),
        (
            LifecycleEvent {
                cash: usd("100.00"),
                period_start_date: "2026-05-15".into(),
                ..debt_event(CORP, LifecycleEventKind::CouponReceived, "50000")
            },
            "event.period_end_date",
        ),
        (
            LifecycleEvent {
                redemption_price: d("100"),
                ..debt_event(CORP, LifecycleEventKind::Redemption, "50000")
            },
            "event.redemption_kind",
        ),
        (
            LifecycleEvent {
                ..debt_event(POOL, LifecycleEventKind::Paydown, "1000000")
            },
            "event.factor",
        ),
        (
            LifecycleEvent {
                deliveries: vec![LifecycleDelivery {
                    instrument_id: TINT.into(),
                    quantity: d("100000"),
                    price: usd("80"),
                }],
                ..debt_event(CORP, LifecycleEventKind::Strip, "100000")
            },
            "event.deliveries",
        ),
        (
            debt_event(CALL, LifecycleEventKind::CouponReceived, "2"),
            "event.kind",
        ),
    ];
    for (event, field) in cases {
        let refused = record(&bus, event).await.unwrap_err();
        let named = meridian_bus::read_refusal_status(&refused)
            .map(|(_, fields, _)| fields)
            .unwrap_or_default();
        assert!(named.contains(&field.to_string()), "{field}: {refused}");
    }
    // A partial call is only reported, never proposed.
    let refused = send(
        &bus,
        PROPOSE_LIFECYCLE_EVENTS,
        "meridian.v1.ProposeLifecycleEventsRequest",
        &ProposeLifecycleEventsRequest {
            events: vec![LifecycleEvent {
                redemption_kind: RedemptionEventKind::PartialCall as i32,
                redemption_price: d("100"),
                ..debt_event(CORP, LifecycleEventKind::Redemption, "15000")
            }],
        },
        None,
    )
    .await
    .unwrap_err();
    let named = meridian_bus::read_refusal_status(&refused)
        .map(|(_, fields, _)| fields)
        .unwrap_or_default();
    assert!(
        named.contains(&"events[0].redemption_kind".to_string()),
        "{refused}"
    );
}

#[test]
fn a_share_is_exact_where_it_ends_and_half_even_where_it_does_not() {
    let e = |t: &str| t.parse::<Exact>().unwrap();
    assert_eq!(
        crate::lifecycle::share("cost", e("99000.00"), e("1900000"), e("9900000")).unwrap(),
        e("19000.00")
    );
    assert_eq!(
        crate::lifecycle::share("cost", e("100.00"), e("1"), e("3")).unwrap(),
        e("33.33")
    );
    assert_eq!(
        crate::lifecycle::share("cost", e("0.05"), e("1"), e("2")).unwrap(),
        e("0.02")
    );
}
