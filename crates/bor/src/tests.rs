//! The book's rules, held over the bus with the in-memory store: each command
//! as an `operations` plugin's sidecar would send it, and what it journals,
//! answers and publishes (fixtures/book/ in meridian-design).

use std::sync::Arc;
use std::time::Duration;

use meridian_bus::{Bus, MemoryBackend, Stamp, Subscription};
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
const USD: &str = "INS-01J8XQ4M7K00000000CASHUSD";

/// The book alone, with no instrument store beside it: every command naming
/// an instrument is refused to be tried again (contract v10).
fn wired_alone() -> (Arc<Bus>, Arc<MemoryStore>) {
    let bus = Arc::new(Bus::single(
        "operations-sample-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::ManualClock::at(1_757_462_400_000_000_000)),
    ));
    let store = Arc::new(MemoryStore::new());
    serve(bus.clone(), store.clone(), bus.clock());
    (bus, store)
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
        currency_code: "USD".into(),
    })
}

/// A command as a sidecar sends it: for a person, or as the plugin itself.
async fn send<M: Message>(
    bus: &Bus,
    topic: &str,
    payload_type: &str,
    message: &M,
    person: Option<&str>,
) -> Result<Vec<u8>, String> {
    let stamp = Stamp {
        acting_for_subject: person.unwrap_or_default().to_string(),
        account_scope: None,
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

/// The code a refusal carries on the bus.
fn code(detail: &str) -> Option<RefusalReason> {
    meridian_bus::read_refusal(detail).and_then(|(reason, _)| RefusalReason::try_from(reason).ok())
}

fn source() -> OpeningSource {
    OpeningSource {
        kind: OpeningSourceKind::Custodian as i32,
        name: "Interactive Brokers".into(),
        as_of_date: "2026-09-08".into(),
        basis: PositionBasis::TradeDate as i32,
        street_records: vec![StreetRecordRef {
            statement_id: "STMT-1".into(),
            ..Default::default()
        }],
    }
}

fn opening() -> RecordOpeningBalanceRequest {
    RecordOpeningBalanceRequest {
        account_id: ACC.into(),
        as_of_date: "2026-09-08".into(),
        sources: vec![source()],
        positions: vec![
            OpeningPosition {
                instrument_id: AAPL.into(),
                side: HoldingSide::Long as i32,
                trade_date_quantity: d("12.5"),
                settled_quantity: d("12.5"),
                pending: vec![],
                lots: vec![OpeningLot {
                    quantity: d("12.5"),
                    terms: Some(LotTerms {
                        cost: usd("2250.00"),
                        acquired_date: "2025-03-14".into(),
                        source: LotSource::OpeningBalance as i32,
                        ..Default::default()
                    }),
                }],
            },
            OpeningPosition {
                instrument_id: USD.into(),
                side: HoldingSide::Long as i32,
                trade_date_quantity: d("1000.00"),
                settled_quantity: d("1000.00"),
                ..Default::default()
            },
        ],
        reason: "Opening balance from the statement of 2026-09-08".into(),
        replaces_entry_id: String::new(),
        idempotency_key: "opening-balance:ACC-1:STMT-1".into(),
    }
}

async fn open(bus: &Bus) -> BookEntryReply {
    let reply = send(
        bus,
        RECORD_OPENING_BALANCE,
        "meridian.v1.RecordOpeningBalanceRequest",
        &opening(),
        Some(PERSON),
    )
    .await
    .unwrap();
    BookEntryReply::decode(&reply[..]).unwrap()
}

fn aapl_break() -> RecordBreakRequest {
    RecordBreakRequest {
        account_id: ACC.into(),
        subject: Some(record_break_request::Subject::Position(PositionKey {
            instrument_id: AAPL.into(),
            side: HoldingSide::Long as i32,
        })),
        category: BreakCategory::TradeDateQuantity as i32,
        differences: vec![BreakDifference {
            field: "trade_date_quantity".into(),
            book: Some(BreakValue {
                value: Some(break_value::Value::Quantity(d("12.5").unwrap())),
            }),
            street: Some(BreakValue {
                value: Some(break_value::Value::Quantity(d("15").unwrap())),
            }),
        }],
        business_date: "2026-09-09".into(),
        candidate_causes: vec![BreakCause {
            category: BreakCauseCategory::UnbookedTrade as i32,
            item: Some(break_cause::Item::NoneFound(true)),
            note: String::new(),
        }],
        ..Default::default()
    }
}

async fn record_break(bus: &Bus, request: &RecordBreakRequest) -> Result<BookEntryReply, String> {
    send(
        bus,
        RECORD_BREAK,
        "meridian.v1.RecordBreakRequest",
        request,
        None,
    )
    .await
    .map(|reply| BookEntryReply::decode(&reply[..]).unwrap())
}

fn adjustment(effective_date: &str, opens_lot: bool) -> ResolveBreakRequest {
    ResolveBreakRequest {
        account_id: ACC.into(),
        break_ids: vec![],
        reason: "Books the buy placed at the broker".into(),
        resolution: Some(resolve_break_request::Resolution::Adjustment(Adjustment {
            effective_date: effective_date.into(),
            lines: vec![MovementLine {
                instrument_id: AAPL.into(),
                side: HoldingSide::Long as i32,
                bucket: SettlementBucket::Settled as i32,
                quantity: d("2.5"),
                opens_lot: opens_lot.then(|| LotTerms {
                    cost: usd("567.50"),
                    acquired_date: "2026-09-09".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        })),
        idempotency_key: String::new(),
    }
}

async fn resolve(
    bus: &Bus,
    request: &ResolveBreakRequest,
    person: Option<&str>,
) -> Result<BookEntryReply, String> {
    send(
        bus,
        RESOLVE_BREAK,
        "meridian.v1.ResolveBreakRequest",
        request,
        person,
    )
    .await
    .map(|reply| BookEntryReply::decode(&reply[..]).unwrap())
}

async fn next(subscription: &mut Subscription) -> meridian_bus::Delivery {
    tokio::time::timeout(Duration::from_secs(5), subscription.recv())
        .await
        .expect("nothing was published within five seconds")
        .expect("the bus shut down")
}

async fn quiet(subscription: &mut Subscription) -> bool {
    tokio::time::timeout(Duration::from_millis(100), subscription.recv())
        .await
        .is_err()
}

fn position<'a>(reply: &'a BookEntryReply, instrument: &str) -> &'a BookPosition {
    reply
        .positions
        .iter()
        .find(|held| held.instrument_id == instrument)
        .unwrap_or_else(|| panic!("no {instrument} in {:?}", reply.positions))
}

fn journal(partition: &str, sequence: u64, previous: u64) -> Option<JournalRef> {
    Some(JournalRef {
        partition: partition.into(),
        sequence,
        previous_sequence: previous,
    })
}

// ── W9.1, W9.2 ──────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn an_opening_balance_opens_every_position_and_lot_from_zero() {
    let (bus, _) = wired();
    let mut positions = bus.subscribe(POSITION_CHANGED);
    let mut attributes = bus.subscribe(ACCOUNT_ATTRIBUTE_CHANGED);

    let reply = open(&bus).await;

    let entry = reply.entry.as_ref().unwrap();
    assert_eq!(entry.kind, "opening-balance");
    assert_eq!(entry.effective_date, "2026-09-08");
    assert!(matches!(
        entry.actor.as_ref().unwrap().kind,
        Some(actor::Kind::Person(ref person)) if person.subject == PERSON
    ));
    assert_eq!(reply.journal, journal("P0", 1, 0));

    // Each record its own number, chained per row and account.
    let aapl = position(&reply, AAPL);
    assert_eq!(aapl.last_change, journal("P0", 1, 0));
    assert_eq!(read(&aapl.trade_date_quantity), "12.5");
    assert_eq!(read(&aapl.settled_quantity), "12.5");
    assert_eq!(read(&aapl.not_stated_quantity), "0");
    let [lot] = aapl.lots.as_slice() else {
        panic!("one lot: {:?}", aapl.lots)
    };
    assert!(lot.lot_id.starts_with("LOT-"));
    assert_eq!(read(&lot.open_quantity), "12.5");
    assert_eq!(read(&lot.original_quantity), "12.5");
    assert_eq!(lot.opened_by, journal("P0", 1, 0));
    assert_eq!(aapl.opened_from, vec![source()]);

    let usd = position(&reply, USD);
    assert_eq!(usd.last_change, journal("P0", 2, 1));
    assert!(usd.lots.is_empty(), "cash has no lots");

    let held = reply.attributes.as_ref().unwrap();
    let standing = held.opening_balance.as_ref().unwrap();
    assert_eq!(standing.entry_id, entry.entry_id);
    assert_eq!(standing.as_of_date, "2026-09-08");
    assert_eq!(standing.journal, journal("P0", 1, 0));
    assert_eq!(held.last_change, journal("P0", 3, 0));

    // Published after the commit, whole: two positions and the attributes.
    for _ in 0..2 {
        let event =
            PositionChangedEvent::decode(&next(&mut positions).await.envelope.payload[..]).unwrap();
        assert_eq!(read(&event.previous_trade_date_quantity), "0");
        assert_eq!(event.journal, event.position.as_ref().unwrap().last_change);
        assert_eq!(event.cause.unwrap().acting_for_subject, PERSON);
    }
    let event =
        AccountAttributeChangedEvent::decode(&next(&mut attributes).await.envelope.payload[..])
            .unwrap();
    assert!(event.attributes.unwrap().opening_balance.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_command_again_is_applied_once_and_answered_as_the_first() {
    let (bus, store) = wired();
    let first = open(&bus).await;
    let mut positions = bus.subscribe(POSITION_CHANGED);

    // A retry the sidecar gave a new message identifier, by its key (Q12).
    let again = open(&bus).await;
    assert_eq!(again, first);
    assert!(quiet(&mut positions).await, "a duplicate published again");
    assert_eq!(store.journal(ACC).unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_opening_balance_is_refused_with_its_code() {
    let (bus, _) = wired();
    open(&bus).await;
    let mut second = opening();
    second.idempotency_key = "another".into();
    let refused = send(
        &bus,
        RECORD_OPENING_BALANCE,
        "meridian.v1.RecordOpeningBalanceRequest",
        &second,
        Some(PERSON),
    )
    .await
    .unwrap_err();
    assert_eq!(
        code(&refused),
        Some(RefusalReason::OpeningBalanceRecorded),
        "{refused}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_opening_balance_needs_a_person_and_a_reason() {
    let (bus, _) = wired();
    let topic = "meridian.v1.RecordOpeningBalanceRequest";
    let refused = send(&bus, RECORD_OPENING_BALANCE, topic, &opening(), None)
        .await
        .unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::ActorRequired));

    let mut no_reason = opening();
    no_reason.reason = " ".into();
    let refused = send(
        &bus,
        RECORD_OPENING_BALANCE,
        topic,
        &no_reason,
        Some(PERSON),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::ReasonRequired));
}

#[tokio::test(flavor = "multi_thread")]
async fn lots_that_do_not_sum_are_refused() {
    let (bus, _) = wired();
    let mut short = opening();
    short.positions[0].lots[0].quantity = d("10");
    let refused = send(
        &bus,
        RECORD_OPENING_BALANCE,
        "meridian.v1.RecordOpeningBalanceRequest",
        &short,
        Some(PERSON),
    )
    .await
    .unwrap_err();
    assert_eq!(
        code(&refused),
        Some(RefusalReason::LotsUnbalanced),
        "{refused}"
    );
}

/// The record ID SnapTrade reported without an asset class: held, and
/// lacking its class and currency until the deployment admin completes it.
const SNAP: &str = "LCL-01J8XQ4M7K00000000000001";
const ZZTP_LOCAL: &str = "LCL-01J8XQ4M7K0000000000ZZTP";
const ZZTP_KEPT: &str = "LCL-01J8XQ4M7K0000000000ZZT2";

/// The book as an operations plugin's sidecar reaches it, beside an
/// instrument store whose records say AAPL is an equity in USD, USD is cash
/// in USD, the two ZZTOP records equities in USD, and SNAP1 nothing yet; and
/// which holds no record of anything else.
fn wired() -> (Arc<Bus>, Arc<MemoryStore>) {
    let (bus, store) = wired_alone();
    bus.serve(RESOLVE_INSTRUMENT, |envelope| {
        let asked = ResolveInstrumentRequest::decode(&envelope.payload[..]).unwrap();
        let record = |asset_class: AssetClass, currency: &str| InstrumentRecord {
            instrument_id: asked.instrument_id.clone(),
            asset_class: asset_class as i32,
            currency: currency.into(),
            version: 1,
            ..Default::default()
        };
        let instrument = match asked.instrument_id.as_str() {
            AAPL | ZZTP_LOCAL | ZZTP_KEPT => Some(record(AssetClass::Equity, "USD")),
            USD => Some(record(AssetClass::Cash, "USD")),
            SNAP => Some(record(AssetClass::Unspecified, "")),
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

fn wired_with_records() -> (Arc<Bus>, Arc<MemoryStore>) {
    wired()
}

/// The fields a refusal names as left out.
fn fields(detail: &str) -> Vec<String> {
    meridian_bus::refusal_fields(detail)
}

async fn opening_refused(bus: &Bus, request: &RecordOpeningBalanceRequest) -> String {
    send(
        bus,
        RECORD_OPENING_BALANCE,
        "meridian.v1.RecordOpeningBalanceRequest",
        request,
        Some(PERSON),
    )
    .await
    .unwrap_err()
}

#[tokio::test(flavor = "multi_thread")]
async fn what_the_source_did_not_state_is_refused_naming_each_field() {
    // Contract v9 (sdk-contract/the-book-refuses-what-downstream-cannot-use):
    // the not-stated bucket, pending with no date and a lot of unknown cost
    // are no longer admitted; every missing field is named at once.
    let (bus, store) = wired_with_records();
    let mut request = opening();
    // Trade date only, and no lots: settled and lots missing.
    request.positions[0].settled_quantity = None;
    request.positions[0].lots = vec![];
    // Settled 600 of 1000, nothing pending stated: the rest is pending with
    // no quantity or date given. Cash, so no lots asked.
    request.positions[1].settled_quantity = d("600");
    let refused = opening_refused(&bus, &request).await;
    assert_eq!(code(&refused), Some(RefusalReason::Incomplete), "{refused}");
    assert_eq!(
        fields(&refused),
        [
            "positions[0].settled_quantity",
            "positions[0].lots",
            "positions[1].pending",
        ]
    );
    assert!(refused.contains("positions[0].lots"), "{refused}");
    // Nothing applied.
    assert!(store.journal(ACC).unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lot_without_its_cost_or_date_and_a_pending_without_its_date_are_refused() {
    let (bus, _) = wired_with_records();
    let mut request = opening();
    request.positions[0].settled_quantity = d("10");
    request.positions[0].pending = vec![PendingSettlement {
        value_date: String::new(),
        quantity: d("2.5"),
        state: None,
    }];
    request.positions[0].lots[0].terms = Some(LotTerms {
        source: LotSource::OpeningBalance as i32,
        ..Default::default()
    });
    request.sources[0].name = String::new();
    let refused = opening_refused(&bus, &request).await;
    assert_eq!(code(&refused), Some(RefusalReason::Incomplete), "{refused}");
    assert_eq!(
        fields(&refused),
        [
            "sources[0].name",
            "positions[0].pending[0].value_date",
            "positions[0].lots[0].terms.cost",
            "positions[0].lots[0].terms.acquired_date",
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_complete_opening_balance_is_recorded_and_cash_needs_no_lots() {
    let (bus, _) = wired_with_records();
    let reply = BookEntryReply::decode(
        &send(
            &bus,
            RECORD_OPENING_BALANCE,
            "meridian.v1.RecordOpeningBalanceRequest",
            &opening(),
            Some(PERSON),
        )
        .await
        .unwrap()[..],
    )
    .unwrap();
    let usd = position(&reply, USD);
    assert!(usd.lots.is_empty());
    assert_eq!(read(&usd.settled_quantity), "1000.00");
    assert_eq!(read(&usd.not_stated_quantity), "0");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_instrument_whose_record_lacks_its_class_or_currency_is_refused_naming_both() {
    // Contract v10 (the completion spec's requirement 15 and Q7): "flagged,
    // not blocking" ends. A record SnapTrade reported without an asset class,
    // and one the store does not hold, each lack both.
    let (bus, store) = wired();
    let mut request = opening();
    request.positions.push(OpeningPosition {
        instrument_id: SNAP.into(),
        side: HoldingSide::Long as i32,
        trade_date_quantity: d("40"),
        settled_quantity: d("40"),
        ..Default::default()
    });
    request.positions.push(OpeningPosition {
        instrument_id: "LCL-01J8XQ4M7K0000000000NONE".into(),
        side: HoldingSide::Long as i32,
        trade_date_quantity: d("1"),
        settled_quantity: d("1"),
        ..Default::default()
    });
    let refused = opening_refused(&bus, &request).await;
    assert_eq!(code(&refused), Some(RefusalReason::Incomplete), "{refused}");
    assert_eq!(
        fields(&refused),
        [
            "positions[2].instrument.asset_class",
            "positions[2].instrument.currency",
            "positions[3].instrument.asset_class",
            "positions[3].instrument.currency",
        ]
    );
    assert!(store.journal(ACC).unwrap().is_empty(), "nothing applied");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_the_instrument_store_cannot_answer_for_is_refused_to_be_tried_again() {
    // Never admitted unchecked, and nothing recorded (contract v10).
    let (bus, store) = wired_alone();
    let refused = opening_refused(&bus, &opening()).await;
    assert_eq!(
        code(&refused),
        Some(RefusalReason::ReferenceUnavailable),
        "{refused}"
    );
    assert!(store.journal(ACC).unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_person_acting_through_a_client_is_recorded_with_the_delegation_and_the_client() {
    // sdk-contract/the-book-records-the-delegation: the entry's actor names
    // the person, the delegation and the client, from the envelope's stamp.
    let (bus, _) = wired();
    let stamp = Stamp {
        acting_for_subject: PERSON.into(),
        acting_through_delegation: "DLG-1".into(),
        acting_through_client: "meridian on ada-laptop".into(),
        account_scope: None,
    };
    let (_, payload) = bus
        .call_stamped(
            RECORD_OPENING_BALANCE,
            "meridian.v1.RecordOpeningBalanceRequest",
            opening().encode_to_vec(),
            None,
            None,
            &stamp,
        )
        .await
        .unwrap();
    let reply = BookEntryReply::decode(&payload[..]).unwrap();
    let actor = reply.entry.unwrap().actor.unwrap();
    assert_eq!(
        actor.kind,
        Some(actor::Kind::Person(PersonActor {
            subject: PERSON.into(),
            delegation_id: "DLG-1".into(),
            client_name: "meridian on ada-laptop".into(),
        }))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn lots_across_buckets_sum_to_both() {
    // Settled 10 and a failing pending 2.5, two lots of 5 and 7.5.
    let (bus, _) = wired();
    let mut request = opening();
    request.positions[0].settled_quantity = d("10");
    request.positions[0].pending = vec![PendingSettlement {
        value_date: "2026-09-09".into(),
        quantity: d("2.5"),
        state: Some(PendingState {
            failing: true,
            fail_reason: "counterparty short".into(),
            expected_date: "2026-09-11".into(),
        }),
    }];
    let terms = || {
        Some(LotTerms {
            cost: usd("900.00"),
            acquired_date: "2025-03-14".into(),
            source: LotSource::OpeningBalance as i32,
            ..Default::default()
        })
    };
    request.positions[0].lots = vec![
        OpeningLot {
            quantity: d("5"),
            terms: terms(),
        },
        OpeningLot {
            quantity: d("7.5"),
            terms: terms(),
        },
    ];
    let reply = BookEntryReply::decode(
        &send(
            &bus,
            RECORD_OPENING_BALANCE,
            "meridian.v1.RecordOpeningBalanceRequest",
            &request,
            Some(PERSON),
        )
        .await
        .unwrap()[..],
    )
    .unwrap();
    let aapl = position(&reply, AAPL);
    let opens: Vec<String> = aapl
        .lots
        .iter()
        .map(|lot| read(&lot.open_quantity))
        .collect();
    assert_eq!(opens, ["5", "7.5"]);
    let originals: Vec<String> = aapl
        .lots
        .iter()
        .map(|lot| read(&lot.original_quantity))
        .collect();
    assert_eq!(originals, ["5", "7.5"]);
    assert_eq!(
        aapl.pending[0].state.as_ref().unwrap().fail_reason,
        "counterparty short"
    );
    assert_eq!(read(&aapl.settled_quantity), "10");
}

// ── W9.4 to W9.7 ────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_break_is_a_finding_that_moves_nothing() {
    let (bus, store) = wired();
    open(&bus).await;
    let mut breaks = bus.subscribe(BREAK_CHANGED);
    let mut positions = bus.subscribe(POSITION_CHANGED);

    let reply = record_break(&bus, &aapl_break()).await.unwrap();
    let [record] = reply.breaks.as_slice() else {
        panic!("one break")
    };
    assert_eq!(record.state, BreakState::Open as i32);
    assert_eq!(record.first_seen_date, "2026-09-09");
    assert!(matches!(
        record.recorded_by.as_ref().unwrap().kind,
        Some(actor::Kind::System(ref system)) if system.instance_id == "operations-sample-1"
    ));
    assert_eq!(record.last_change, journal("P0", 4, 0));
    next(&mut breaks).await;
    assert!(quiet(&mut positions).await, "a break moved a position");

    // Brought up to date the next day: the same break.
    let mut again = aapl_break();
    again.break_id = record.break_id.clone();
    again.business_date = "2026-09-10".into();
    let updated = record_break(&bus, &again).await.unwrap();
    assert_eq!(updated.breaks[0].break_id, record.break_id);
    assert_eq!(updated.breaks[0].first_seen_date, "2026-09-09");
    assert_eq!(updated.breaks[0].last_seen_date, "2026-09-10");
    assert_eq!(updated.breaks[0].last_change, journal("P0", 5, 4));

    let positions = store
        .positions(&PositionsRead {
            scope: Scope::Everything,
            account_id: ACC.into(),
            since: None,
            business_date: String::new(),
            at: None,
            limit: 100,
            cursor: String::new(),
        })
        .unwrap();
    assert_eq!(read(&positions.records[0].trade_date_quantity), "12.5");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cause_linking_the_custodians_activity_is_recorded_as_given() {
    // Contract v14 (the plan's Q3): income the custodian reinvested, linked to
    // the activity that explains it, and a split under corporate action,
    // recorded as given; the book reads no other store, so an activity it has
    // never heard of is recorded too, and nothing moves.
    let (bus, _) = wired();
    open(&bus).await;
    let mut positions = bus.subscribe(POSITION_CHANGED);
    let reinvested = BreakCause {
        category: BreakCauseCategory::IncomeReinvested as i32,
        item: Some(break_cause::Item::Activity(ActivityRef {
            activity_id: "ACT-01J8XQ5N2P0000000000001".into(),
            change: journal("street", 71, 0),
            trade_date: "2026-09-30".into(),
        })),
        note: "SPAXX's September dividend, reinvested".into(),
    };
    let split = BreakCause {
        category: BreakCauseCategory::CorporateAction as i32,
        item: Some(break_cause::Item::Activity(ActivityRef {
            activity_id: "ACT-NEVER-HEARD-OF".into(),
            change: None,
            trade_date: String::new(),
        })),
        note: String::new(),
    };
    let mut request = aapl_break();
    request.candidate_causes = vec![reinvested.clone(), split.clone()];

    let reply = record_break(&bus, &request).await.unwrap();
    assert_eq!(
        reply.breaks[0].candidate_causes,
        vec![reinvested.clone(), split]
    );
    assert!(quiet(&mut positions).await, "a break moved a position");

    // Confirmed by a person under the new category, it is kept as given too.
    let handled = send(
        &bus,
        HANDLE_BREAK,
        "meridian.v1.HandleBreakRequest",
        &HandleBreakRequest {
            account_id: ACC.into(),
            break_id: reply.breaks[0].break_id.clone(),
            confirmed_cause: Some(reinvested.clone()),
            handling: None,
            reason: "Fidelity reinvested the dividend".into(),
            idempotency_key: String::new(),
        },
        Some(PERSON),
    )
    .await
    .unwrap();
    let handled = BookEntryReply::decode(&handled[..]).unwrap();
    assert_eq!(handled.breaks[0].confirmed_cause, Some(reinvested));
    assert!(
        quiet(&mut positions).await,
        "confirming a cause moved a position"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_break_needs_an_opening_balance() {
    let (bus, _) = wired();
    let refused = record_break(&bus, &aapl_break()).await.unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::NoOpeningBalance));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_figures_are_kept_per_agreement() {
    let (bus, _) = wired();
    open(&bus).await;
    let agreement = |segment: &str| MarginAgreementRef {
        agreement: Some(margin_agreement_ref::Agreement::StatementSegment(
            StatementSegmentRef {
                external_account_id: "SNAP-ACC-1".into(),
                segment: segment.into(),
                counterparty: "Interactive Brokers".into(),
            },
        )),
    };
    let request = RecordAccountFiguresRequest {
        account_id: ACC.into(),
        business_date: "2026-09-09".into(),
        source: None,
        agreements: vec![
            AgreementFigures {
                agreement: Some(agreement("")),
                figures: Some(StatementFigures {
                    segment: String::new(),
                    net_liquidation: usd("2400000.00"),
                    ..Default::default()
                }),
                position_values: vec![ReportedPositionValue {
                    instrument_id: AAPL.into(),
                    side: HoldingSide::Long as i32,
                    market_value: usd("3412.50"),
                    margin_requirement: None,
                }],
            },
            AgreementFigures {
                agreement: Some(agreement("futures")),
                figures: Some(StatementFigures {
                    segment: "futures".into(),
                    initial_margin: usd("48000.00"),
                    ..Default::default()
                }),
                position_values: vec![],
            },
        ],
        idempotency_key: "figures:ACC-1:STMT-2".into(),
    };
    let reply = BookEntryReply::decode(
        &send(
            &bus,
            RECORD_ACCOUNT_FIGURES,
            "meridian.v1.RecordAccountFiguresRequest",
            &request,
            None,
        )
        .await
        .unwrap()[..],
    )
    .unwrap();
    assert_eq!(reply.figures.len(), 2);
    assert_eq!(reply.figures[0].last_change, journal("P0", 4, 0));
    assert_eq!(reply.figures[1].last_change, journal("P0", 5, 4));

    let mut unequal = request.clone();
    unequal.idempotency_key.clear();
    unequal.agreements[1].figures.as_mut().unwrap().segment = "securities".into();
    let refused = send(
        &bus,
        RECORD_ACCOUNT_FIGURES,
        "meridian.v1.RecordAccountFiguresRequest",
        &unequal,
        None,
    )
    .await
    .unwrap_err();
    assert!(refused.contains("securities"), "{refused}");
    assert_eq!(code(&refused), None, "no code: the words say what to fix");
}

#[tokio::test(flavor = "multi_thread")]
async fn encumbrances_are_an_attribute_and_free_is_derived_from_them() {
    let (bus, store) = wired();
    open(&bus).await;
    let mut positions = bus.subscribe(POSITION_CHANGED);
    let source = StreetRecordRef {
        statement_id: "STMT-2".into(),
        as_of_date: "2026-09-09".into(),
        ..Default::default()
    };
    let pledged = |quantity: &str| Encumbrance {
        kind: EncumbranceKind::Pledged as i32,
        quantity: d(quantity),
        pledgee: "Interactive Brokers".into(),
        held_at: "DTC".into(),
        source_code: "PLED".into(),
        ..Default::default()
    };
    let request = |date: &str, set: Vec<Encumbrance>| RecordEncumbrancesRequest {
        account_id: ACC.into(),
        business_date: date.into(),
        source: Some(source.clone()),
        positions: vec![PositionEncumbrances {
            instrument_id: AAPL.into(),
            side: HoldingSide::Long as i32,
            encumbrances: set,
        }],
        idempotency_key: String::new(),
    };
    let record = |message: RecordEncumbrancesRequest| {
        let bus = bus.clone();
        async move {
            send(
                &bus,
                RECORD_ENCUMBRANCES,
                "meridian.v1.RecordEncumbrancesRequest",
                &message,
                None,
            )
            .await
            .map(|bytes| BookEntryReply::decode(&bytes[..]).unwrap())
        }
    };

    // Before any is recorded, free is the settled quantity.
    let before = store.positions(&everything()).unwrap();
    let aapl = before
        .records
        .iter()
        .find(|held| held.instrument_id == AAPL)
        .unwrap();
    assert_eq!(read(&aapl.free_quantity), "12.5");
    assert_eq!(aapl.free_basis, FreeBasis::Settled as i32);

    let reply = record(request("2026-09-09", vec![pledged("4")]))
        .await
        .unwrap();
    let [held] = reply.positions.as_slice() else {
        panic!("one position changed: {:?}", reply.positions)
    };
    assert_eq!(reply.entry.as_ref().unwrap().kind, "encumbrances-recorded");
    assert_eq!(read(&held.trade_date_quantity), "12.5", "nothing moved");
    assert_eq!(
        read(&held.free_quantity),
        "8.5",
        "settled less what is pledged"
    );
    let [encumbrance] = held.encumbrances.as_slice() else {
        panic!("one encumbrance: {:?}", held.encumbrances)
    };
    assert_eq!(encumbrance.since_date, "2026-09-09");
    assert_eq!(encumbrance.source.as_ref(), Some(&source));
    // Numbered as any change, chained to the account's last position change.
    assert_eq!(held.last_change, journal("P0", 4, 2));
    assert_eq!(encumbrance.set_by, held.last_change);
    let delivered = next(&mut positions).await;
    let event = PositionChangedEvent::decode(&delivered.envelope.payload[..]).unwrap();
    assert_eq!(read(&event.previous_trade_date_quantity), "12.5");

    // Restated the next day: its first date kept; then released.
    let again = record(request("2026-09-10", vec![pledged("4")]))
        .await
        .unwrap();
    assert_eq!(again.positions[0].encumbrances[0].since_date, "2026-09-09");
    let released = record(request("2026-09-11", vec![])).await.unwrap();
    assert!(released.positions[0].encumbrances.is_empty());
    assert_eq!(read(&released.positions[0].free_quantity), "12.5");

    // The street's own kinds, OTHER with no code, and a position the book
    // does not hold are refused naming the field.
    let mut lent = pledged("1");
    lent.kind = EncumbranceKind::Rehypothecated as i32;
    let refused = record(request("2026-09-11", vec![lent])).await.unwrap_err();
    assert!(refused.contains("the street's alone"), "{refused}");
    let mut other = pledged("1");
    other.kind = EncumbranceKind::Other as i32;
    other.source_code.clear();
    let refused = record(request("2026-09-11", vec![other]))
        .await
        .unwrap_err();
    assert!(refused.contains("source_code"), "{refused}");
    let mut elsewhere = request("2026-09-11", vec![pledged("1")]);
    elsewhere.positions[0].instrument_id = "INS-01J8XQ4M7K0000000000MSFT".into();
    let refused = record(elsewhere).await.unwrap_err();
    assert!(refused.contains("break of its own"), "{refused}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_adjustment_resolves_the_break_in_the_same_act() {
    let (bus, _) = wired();
    open(&bus).await;
    let recorded = record_break(&bus, &aapl_break()).await.unwrap();
    let break_id = recorded.breaks[0].break_id.clone();

    let handled = send(
        &bus,
        HANDLE_BREAK,
        "meridian.v1.HandleBreakRequest",
        &HandleBreakRequest {
            account_id: ACC.into(),
            break_id: break_id.clone(),
            confirmed_cause: Some(BreakCause {
                category: BreakCauseCategory::UnbookedTrade as i32,
                item: Some(break_cause::Item::NoneFound(true)),
                note: "a buy placed at the broker".into(),
            }),
            handling: Some(BreakHandling {
                owner_subject: PERSON.into(),
                escalation_level: 2,
                due_date: "2026-09-14".into(),
            }),
            reason: "Confirmed with the desk".into(),
            idempotency_key: String::new(),
        },
        Some(PERSON),
    )
    .await
    .unwrap();
    let handled = BookEntryReply::decode(&handled[..]).unwrap();
    assert_eq!(handled.breaks[0].state, BreakState::Open as i32);
    assert_eq!(
        handled.breaks[0]
            .handling
            .as_ref()
            .unwrap()
            .escalation_level,
        2
    );

    // Before the opening balance's date: refused.
    let mut early = adjustment("2026-09-08", true);
    early.break_ids = vec![break_id.clone()];
    let refused = resolve(&bus, &early, Some(PERSON)).await.unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::BeforeOpeningBalance));

    // A line moving a position with lots and no lot: refused.
    let mut unbalanced = adjustment("2026-09-09", false);
    unbalanced.break_ids = vec![break_id.clone()];
    let refused = resolve(&bus, &unbalanced, Some(PERSON)).await.unwrap_err();
    assert_eq!(
        code(&refused),
        Some(RefusalReason::LotsUnbalanced),
        "{refused}"
    );

    // No person: refused.
    let mut request = adjustment("2026-09-09", true);
    request.break_ids = vec![break_id.clone()];
    let refused = resolve(&bus, &request, None).await.unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::ActorRequired));

    let reply = resolve(&bus, &request, Some(PERSON)).await.unwrap();
    assert_eq!(reply.entry.as_ref().unwrap().kind, "adjustment");
    let aapl = position(&reply, AAPL);
    assert_eq!(read(&aapl.trade_date_quantity), "15.0");
    assert_eq!(aapl.lots.len(), 2);
    assert_eq!(aapl.last_change, journal("P0", 6, 2));
    let resolved = &reply.breaks[0];
    assert_eq!(resolved.state, BreakState::Resolved as i32);
    assert_eq!(
        resolved.resolution.as_ref().unwrap().entries,
        vec![journal("P0", 6, 2).unwrap()]
    );
    assert_eq!(resolved.last_change, journal("P0", 7, 5));

    // Resolved: no longer handled, updated or resolved.
    let refused = resolve(&bus, &request, Some(PERSON)).await.unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::BreakState));
    let mut update = aapl_break();
    update.break_id = break_id;
    let refused = record_break(&bus, &update).await.unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::BreakState));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_break_is_closed_with_an_explanation_or_as_cleared_and_nothing_moves() {
    let (bus, _) = wired();
    open(&bus).await;
    let first = record_break(&bus, &aapl_break()).await.unwrap().breaks[0]
        .break_id
        .clone();
    let mut other = aapl_break();
    other.category = BreakCategory::SettledQuantity as i32;
    let second = record_break(&bus, &other).await.unwrap().breaks[0]
        .break_id
        .clone();
    let mut positions = bus.subscribe(POSITION_CHANGED);

    let closed = resolve(
        &bus,
        &ResolveBreakRequest {
            account_id: ACC.into(),
            break_ids: vec![first],
            reason: "custodian error".into(),
            resolution: Some(resolve_break_request::Resolution::Explanation(
                "the custodian reported it on the wrong account".into(),
            )),
            idempotency_key: String::new(),
        },
        Some(PERSON),
    )
    .await
    .unwrap();
    assert_eq!(closed.entry.as_ref().unwrap().kind, "break-closed");
    assert_eq!(closed.breaks[0].state, BreakState::Closed as i32);

    let clearing = CloseBreaksAsClearedRequest {
        account_id: ACC.into(),
        break_ids: vec![second],
        cleared_at: Some(StreetRecordRef {
            statement_id: "STMT-3".into(),
            as_of_date: "2026-09-10".into(),
            ..Default::default()
        }),
        reason: "the difference was gone at the next statement".into(),
        idempotency_key: String::new(),
    };
    let topic = "meridian.v1.CloseBreaksAsClearedRequest";
    let refused = send(&bus, CLOSE_BREAKS_AS_CLEARED, topic, &clearing, None)
        .await
        .unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::ActorRequired));
    let cleared = BookEntryReply::decode(
        &send(
            &bus,
            CLOSE_BREAKS_AS_CLEARED,
            topic,
            &clearing,
            Some(PERSON),
        )
        .await
        .unwrap()[..],
    )
    .unwrap();
    assert_eq!(cleared.entry.as_ref().unwrap().kind, "break-closed");
    assert_eq!(cleared.entry.as_ref().unwrap().effective_date, "2026-09-10");
    let resolution = cleared.breaks[0].resolution.as_ref().unwrap();
    assert_eq!(
        resolution.cleared_at.as_ref().unwrap().statement_id,
        "STMT-3"
    );
    assert!(quiet(&mut positions).await, "closing moved a position");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reversal_negates_and_the_opening_balance_is_reversed_only_alone() {
    let (bus, store) = wired();
    let opened = open(&bus).await;
    let opening_id = opened.entry.unwrap().entry_id;
    let b1 = record_break(&bus, &aapl_break()).await.unwrap().breaks[0]
        .break_id
        .clone();
    let mut request = adjustment("2026-09-09", true);
    request.break_ids = vec![b1];
    let adjusted = resolve(&bus, &request, Some(PERSON)).await.unwrap();
    let adjustment_id = adjusted.entry.unwrap().entry_id;

    // The opening balance, while the adjustment stands: refused.
    let mut b2 = aapl_break();
    b2.category = BreakCategory::CostOrLots as i32;
    let b2 = record_break(&bus, &b2).await.unwrap().breaks[0]
        .break_id
        .clone();
    let reverse = |entry_id: &str, breaks: Vec<String>| ResolveBreakRequest {
        account_id: ACC.into(),
        break_ids: breaks,
        reason: "reversed".into(),
        resolution: Some(resolve_break_request::Resolution::Reversal(Reversal {
            entry_id: entry_id.into(),
        })),
        idempotency_key: String::new(),
    };
    let refused = resolve(&bus, &reverse(&opening_id, vec![b2.clone()]), Some(PERSON))
        .await
        .unwrap_err();
    assert_eq!(
        code(&refused),
        Some(RefusalReason::LaterEntriesStand),
        "{refused}"
    );

    // The adjustment's reversal relieves the lot it opened.
    let reversed = resolve(&bus, &reverse(&adjustment_id, vec![b2]), Some(PERSON))
        .await
        .unwrap();
    assert_eq!(reversed.entry.as_ref().unwrap().kind, "reversal");
    let aapl = position(&reversed, AAPL);
    assert_eq!(read(&aapl.trade_date_quantity), "12.5");
    assert_eq!(
        aapl.lots.len(),
        1,
        "the reversed lot is closed: {:?}",
        aapl.lots
    );

    // Now the opening balance alone: reversed, the standing one cleared.
    let mut b3 = aapl_break();
    b3.category = BreakCategory::BookOnly as i32;
    let b3 = record_break(&bus, &b3).await.unwrap().breaks[0]
        .break_id
        .clone();
    let cleared = resolve(&bus, &reverse(&opening_id, vec![b3]), Some(PERSON))
        .await
        .unwrap();
    assert!(cleared.attributes.unwrap().opening_balance.is_none());

    // And a new one, naming the reversed.
    let mut again = opening();
    again.idempotency_key = "opening-balance:ACC-1:STMT-1:again".into();
    let refused = send(
        &bus,
        RECORD_OPENING_BALANCE,
        "meridian.v1.RecordOpeningBalanceRequest",
        &again,
        Some(PERSON),
    )
    .await
    .unwrap_err();
    assert!(refused.contains("names it"), "{refused}");
    again.replaces_entry_id = opening_id;
    let replaced = BookEntryReply::decode(
        &send(
            &bus,
            RECORD_OPENING_BALANCE,
            "meridian.v1.RecordOpeningBalanceRequest",
            &again,
            Some(PERSON),
        )
        .await
        .unwrap()[..],
    )
    .unwrap();
    assert_eq!(read(&position(&replaced, AAPL).trade_date_quantity), "12.5");

    // Rebuilt from the journal alone, the same book.
    let before = store.positions(&everything()).unwrap().records;
    store.rebuild().unwrap();
    assert_eq!(store.positions(&everything()).unwrap().records, before);
}

fn everything() -> PositionsRead {
    PositionsRead {
        scope: Scope::Everything,
        account_id: String::new(),
        since: None,
        business_date: String::new(),
        at: None,
        limit: 100,
        cursor: String::new(),
    }
}

// ── W9.10 to W9.14 ──────────────────────────────────────────────────────────

/// A plugin's read, its scope stamped and marked.
async fn read_as_a_plugin<M: Message>(
    bus: &Bus,
    topic: &str,
    payload_type: &str,
    message: &M,
    scope: &[&str],
) -> Result<Vec<u8>, String> {
    let stamp = Stamp {
        acting_for_subject: String::new(),
        account_scope: Some(scope.iter().map(|account| account.to_string()).collect()),
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
    .map_err(|failed| failed.to_string())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_answers_within_the_scope_since_a_watermark_and_by_date() {
    let (bus, _) = wired();
    open(&bus).await;
    let b = record_break(&bus, &aapl_break()).await.unwrap().breaks[0]
        .break_id
        .clone();
    let mut request = adjustment("2026-09-09", true);
    request.break_ids = vec![b];
    resolve(&bus, &request, Some(PERSON)).await.unwrap();
    let topic = "meridian.v1.ListPositionsRequest";
    let list = |request: ListPositionsRequest, scope: &'static [&'static str]| {
        let bus = bus.clone();
        async move {
            read_as_a_plugin(&bus, LIST_POSITIONS, topic, &request, scope)
                .await
                .map(|reply| ListPositionsReply::decode(&reply[..]).unwrap())
        }
    };

    let within = list(ListPositionsRequest::default(), &[ACC]).await.unwrap();
    assert_eq!(within.positions.len(), 2);
    assert_eq!(
        within.as_of.unwrap().partitions[0],
        PartitionSequence {
            partition: "P0".into(),
            sequence: 6
        }
    );
    let none = list(ListPositionsRequest::default(), &[]).await.unwrap();
    assert!(none.positions.is_empty(), "an empty scope reads nothing");
    let outside = list(
        ListPositionsRequest {
            account_id: ACC.into(),
            ..Default::default()
        },
        &["ACC-2"],
    )
    .await
    .unwrap_err();
    assert!(
        outside.contains("not in this plugin's read scope"),
        "{outside}"
    );

    let since = |sequence| Watermark {
        partitions: vec![PartitionSequence {
            partition: "P0".into(),
            sequence,
        }],
    };
    let changed = list(
        ListPositionsRequest {
            since: Some(since(3)),
            ..Default::default()
        },
        &[ACC],
    )
    .await
    .unwrap();
    assert_eq!(changed.positions.len(), 1);
    assert_eq!(changed.positions[0].instrument_id, AAPL);

    // As of the opening balance's date, as known at the opening balance.
    let dated = list(
        ListPositionsRequest {
            business_date: "2026-09-08".into(),
            at: Some(since(1)),
            ..Default::default()
        },
        &[ACC],
    )
    .await
    .unwrap();
    let aapl = dated
        .positions
        .iter()
        .find(|held| held.instrument_id == AAPL)
        .unwrap();
    assert_eq!(read(&aapl.trade_date_quantity), "12.5");
    // Known now, still the end of D0: the adjustment is effective after it.
    let dated_now = list(
        ListPositionsRequest {
            business_date: "2026-09-08".into(),
            ..Default::default()
        },
        &[ACC],
    )
    .await
    .unwrap();
    let aapl = dated_now
        .positions
        .iter()
        .find(|held| held.instrument_id == AAPL)
        .unwrap();
    assert_eq!(read(&aapl.trade_date_quantity), "12.5");

    let both = list(
        ListPositionsRequest {
            since: Some(since(3)),
            business_date: "2026-09-08".into(),
            ..Default::default()
        },
        &[ACC],
    )
    .await
    .unwrap_err();
    assert!(both.contains("takes no `since`"), "{both}");
}

#[tokio::test(flavor = "multi_thread")]
async fn attributes_are_set_for_a_person_before_or_after_an_opening_balance() {
    let (bus, _) = wired();
    let set = |code: &str| SetAccountAttributeRequest {
        account_id: "ACC-2".into(),
        attribute: Some(set_account_attribute_request::Attribute::BaseCurrencyCode(
            code.into(),
        )),
        reason: "the fund reports in it".into(),
    };
    let reply = send(
        &bus,
        SET_ACCOUNT_ATTRIBUTE,
        "meridian.v1.SetAccountAttributeRequest",
        &set("EUR"),
        Some("local|harness"),
    )
    .await
    .unwrap();
    let reply = AccountAttributeReply::decode(&reply[..]).unwrap();
    assert_eq!(reply.attributes.as_ref().unwrap().base_currency_code, "EUR");
    assert!(reply.attributes.unwrap().opening_balance.is_none());
    assert_eq!(reply.entry.unwrap().kind, "attribute-set");

    let refused = send(
        &bus,
        SET_ACCOUNT_ATTRIBUTE,
        "meridian.v1.SetAccountAttributeRequest",
        &set("eur"),
        Some("local|harness"),
    )
    .await
    .unwrap_err();
    assert!(refused.contains("three capital letters"), "{refused}");
    let refused = send(
        &bus,
        SET_ACCOUNT_ATTRIBUTE,
        "meridian.v1.SetAccountAttributeRequest",
        &set("EUR"),
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::ActorRequired));

    let listed = read_as_a_plugin(
        &bus,
        LIST_ACCOUNT_ATTRIBUTES,
        "meridian.v1.ListAccountAttributesRequest",
        &ListAccountAttributesRequest::default(),
        &["ACC-2"],
    )
    .await
    .unwrap();
    let listed = ListAccountAttributesReply::decode(&listed[..]).unwrap();
    assert_eq!(listed.attributes.len(), 1);
}

// ── W9.9 ────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_merged_record_is_followed_its_lots_keeping_their_identifiers() {
    let (bus, store) = wired();
    let store: Arc<dyn Store> = store;
    tokio::spawn(follow_replacements(bus.clone(), store.clone()));
    let placeholder = ZZTP_LOCAL;
    let instrument = ZZTP_KEPT;
    let mut request = opening();
    request.positions[0].instrument_id = placeholder.into();
    let opened = BookEntryReply::decode(
        &send(
            &bus,
            RECORD_OPENING_BALANCE,
            "meridian.v1.RecordOpeningBalanceRequest",
            &request,
            Some(PERSON),
        )
        .await
        .unwrap()[..],
    )
    .unwrap();
    // A record the deployment minted is a record like any other
    // (decisions/030): it enters the opening balance once it is complete.
    let lot_id = position(&opened, placeholder).lots[0].lot_id.clone();
    let mut on_it = aapl_break();
    on_it.subject = Some(record_break_request::Subject::Position(PositionKey {
        instrument_id: placeholder.into(),
        side: HoldingSide::Long as i32,
    }));
    record_break(&bus, &on_it).await.unwrap();
    assert!(store
        .instruments_held()
        .unwrap()
        .contains(&placeholder.to_string()));
    let mut positions = bus.subscribe(POSITION_CHANGED);

    bus.publish(
        INSTRUMENT_REPLACED,
        "meridian.v1.InstrumentReplacedEvent",
        InstrumentReplacedEvent {
            replaced_instrument_id: placeholder.into(),
            instrument: Some(InstrumentRecord {
                instrument_id: instrument.into(),
                ..Default::default()
            }),
            replaced_at_ns: 1,
        }
        .encode_to_vec(),
        Some("CORR-REPLACED"),
        None,
    )
    .unwrap();

    let mut seen = Vec::new();
    for _ in 0..2 {
        let delivered = next(&mut positions).await;
        assert_eq!(
            delivered.envelope.meta.as_ref().unwrap().correlation_id,
            "CORR-REPLACED"
        );
        let event = PositionChangedEvent::decode(&delivered.envelope.payload[..]).unwrap();
        assert_eq!(event.entry.as_ref().unwrap().kind, "instrument-merged");
        seen.push(event.position.unwrap());
    }
    let moved = seen
        .iter()
        .find(|held| held.instrument_id == instrument)
        .unwrap();
    assert_eq!(read(&moved.trade_date_quantity), "12.5");
    assert_eq!(moved.lots[0].lot_id, lot_id, "the lot keeps its identifier");
    assert_eq!(read(&moved.lots[0].original_quantity), "12.5");
    let tombstone = seen
        .iter()
        .find(|held| held.instrument_id == placeholder)
        .unwrap();
    assert!(tombstone.removed);

    assert!(!store
        .instruments_held()
        .unwrap()
        .contains(&placeholder.to_string()));
    let read = store.positions(&everything()).unwrap();
    assert!(read
        .records
        .iter()
        .all(|held| held.instrument_id != placeholder));
    let breaks = store
        .breaks(&crate::store::BreaksRead {
            scope: Scope::Everything,
            account_id: String::new(),
            states: vec![],
            since: None,
            limit: 10,
            cursor: String::new(),
        })
        .unwrap();
    assert!(matches!(
        breaks.records[0].subject,
        Some(r#break::Subject::Position(ref key)) if key.instrument_id == instrument
    ));
}

// ── The sample operations plugin's build: Q12's conflicts, a stated cost, an
// undated pending line ───────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_key_names_one_command_and_another_under_it_is_refused() {
    let (bus, store) = wired();
    open(&bus).await;
    let mut other = opening();
    other.reason = "the same key, another command".into();
    let refused = send(
        &bus,
        RECORD_OPENING_BALANCE,
        "meridian.v1.RecordOpeningBalanceRequest",
        &other,
        Some(PERSON),
    )
    .await
    .unwrap_err();
    assert_eq!(
        code(&refused),
        Some(RefusalReason::IdempotencyConflict),
        "{refused}"
    );
    assert_eq!(store.journal(ACC).unwrap().len(), 1);
    // A key is the account's: another account may use the same.
    let mut elsewhere = opening();
    elsewhere.account_id = "ACC-2".into();
    send(
        &bus,
        RECORD_OPENING_BALANCE,
        "meridian.v1.RecordOpeningBalanceRequest",
        &elsewhere,
        Some(PERSON),
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_known_cost_moves_by_an_amount_and_is_never_stated_again() {
    // From v9 the book opens no lot of unknown cost (W9.1, W9.7), so every
    // lot a v9 command opens has a cost to change: stated_cost remains for a
    // lot a v8 book recorded without one.
    let (bus, _) = wired();
    let opened = open(&bus).await;
    let lot_id = position(&opened, AAPL).lots[0].lot_id.clone();
    let mut costing = aapl_break();
    costing.category = BreakCategory::CostOrLots as i32;
    let break_id = record_break(&bus, &costing).await.unwrap().breaks[0]
        .break_id
        .clone();
    let state = |lot: &str, cost: basis_adjustment::Cost| ResolveBreakRequest {
        account_id: ACC.into(),
        break_ids: vec![break_id.clone()],
        reason: "the custodian's cost, now reported".into(),
        resolution: Some(resolve_break_request::Resolution::Adjustment(Adjustment {
            effective_date: "2026-09-09".into(),
            basis_adjustments: vec![BasisAdjustment {
                lot_id: lot.into(),
                cost: Some(cost),
                holding_period_start: String::new(),
            }],
            ..Default::default()
        })),
        idempotency_key: String::new(),
    };
    let refused = resolve(
        &bus,
        &state(
            &lot_id,
            basis_adjustment::Cost::StatedCost(usd("2250.00").unwrap()),
        ),
        Some(PERSON),
    )
    .await
    .unwrap_err();
    assert!(refused.contains("is known"), "{refused}");
    let changed = resolve(
        &bus,
        &state(
            &lot_id,
            basis_adjustment::Cost::CostChange(usd("-112.50").unwrap()),
        ),
        Some(PERSON),
    )
    .await
    .unwrap();
    let lot = &position(&changed, AAPL).lots[0];
    assert_eq!(
        read(&lot.terms.as_ref().unwrap().cost.as_ref().unwrap().amount),
        "2137.50"
    );
    assert_eq!(lot.adjusted_by.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_adjustment_opening_a_lot_without_its_cost_or_date_is_refused() {
    let (bus, _) = wired();
    open(&bus).await;
    let break_id = record_break(&bus, &aapl_break()).await.unwrap().breaks[0]
        .break_id
        .clone();
    let mut request = adjustment("2026-09-09", true);
    request.break_ids = vec![break_id];
    if let Some(resolve_break_request::Resolution::Adjustment(adjustment)) =
        request.resolution.as_mut()
    {
        adjustment.lines[0].opens_lot = Some(LotTerms {
            source: LotSource::Adjustment as i32,
            ..Default::default()
        });
    }
    let refused = resolve(&bus, &request, Some(PERSON)).await.unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::Incomplete), "{refused}");
    assert_eq!(
        fields(&refused),
        [
            "adjustment.lines[0].opens_lot.cost",
            "adjustment.lines[0].opens_lot.acquired_date",
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pending_line_may_name_no_value_date() {
    let (bus, _) = wired();
    open(&bus).await;
    let break_id = record_break(&bus, &aapl_break()).await.unwrap().breaks[0]
        .break_id
        .clone();
    let mut request = adjustment("2026-09-09", true);
    request.break_ids = vec![break_id];
    if let Some(resolve_break_request::Resolution::Adjustment(adjustment)) =
        request.resolution.as_mut()
    {
        adjustment.lines[0].bucket = SettlementBucket::Pending as i32;
    }
    let reply = resolve(&bus, &request, Some(PERSON)).await.unwrap();
    let aapl = position(&reply, AAPL);
    let [pending] = aapl.pending.as_slice() else {
        panic!("one pending: {:?}", aapl.pending)
    };
    assert_eq!(pending.value_date, "", "date not stated");
    assert_eq!(read(&pending.quantity), "2.5");
    assert_eq!(read(&aapl.settled_quantity), "12.5");
}
