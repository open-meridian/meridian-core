//! The book, against Postgres.
//!
//! What only the database makes true: the journal refusing an update or a
//! delete whoever asks, a duplicate recognised by its unique index, numbers
//! without holes under concurrent commands, the reads' order and scope in
//! SQL, and a rebuild from the journal reproducing every projection.
//!
//! Run by `make test-store`, which brings up a database. They fail loudly when
//! it is missing rather than skipping. The database is shared by every run, so
//! nothing here assumes a partition's numbers start anywhere: each test uses
//! accounts of its own and reads its numbers relative to the head.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use meridian_bor::service::*;
use meridian_bor::store::{BreaksRead, PositionsRead, Scope, Store};
use meridian_bor::PostgresStore;
use meridian_bus::{Bus, Envelope, MemoryBackend, Stamp};
use meridian_changes::scratch::Scratch;
use meridian_domain::exact::Exact;
use meridian_domain::v1::*;
use prost::Message;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A rebuild is the whole book's, every account's (W9.10): it clears every
/// projection and snapshot and makes them again at its own store's snapshot
/// interval. These tests share one database and run at once, so a rebuild
/// by a store at the default interval (1,000) clears the snapshots the
/// snapshot test's store (every 3) made for its account, and makes none
/// again for its 12 entries -- the test then found none (`RowCount`, or 0
/// kept). In a deployment one bor rebuilds, at one interval. The snapshot
/// test holds this for its whole run, and every other rebuild takes it.
static REBUILDS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A rebuild made while no snapshot test is running.
async fn rebuild(store: &Arc<PostgresStore>) -> usize {
    let _alone = REBUILDS.lock().await;
    blocking(|| store.rebuild()).unwrap()
}

const PERSON: &str = "https://directory.example.org|8812";

fn unique(tag: &str) -> String {
    let seq = COUNTER.fetch_add(1, Ordering::SeqCst);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{tag}-{now}-{seq}")
}

fn url() -> String {
    std::env::var("MERIDIAN_TEST_DATABASE_URL").expect(
        "MERIDIAN_TEST_DATABASE_URL is not set. These tests need a real Postgres; \
         run them with `make test-store`.",
    )
}

fn store() -> Arc<PostgresStore> {
    let store = PostgresStore::connect(&url(), 8).expect("could not reach the test database");
    store
        .migrate(&meridian_clock::SystemClock)
        .expect("could not create the schema");
    store
        .verify()
        .expect("the schema is not the one this binary expects");
    Arc::new(store)
}

/// The store is synchronous, as the bus's handlers are: a test calls it off
/// the async runtime's workers, as a handler is called.
fn blocking<T>(f: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(f)
}

fn wired() -> (Arc<Bus>, Arc<PostgresStore>) {
    wired_with(blocking(store))
}

fn wired_with(store: Arc<PostgresStore>) -> (Arc<Bus>, Arc<PostgresStore>) {
    let bus = Arc::new(Bus::single(
        "operations-test-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    serve(bus.clone(), store.clone(), bus.clock());
    resolving_equities(&bus);
    (bus, store)
}

fn resolving_equities(bus: &Bus) {
    // An instrument store whose every record names an equity in US dollars:
    // from contract v10 the book requires each instrument's asset class and
    // currency, and refuses a command it cannot check.
    bus.serve(RESOLVE_INSTRUMENT, |envelope| {
        let asked = ResolveInstrumentRequest::decode(&envelope.payload[..]).unwrap();
        Ok((
            "meridian.v1.ResolveInstrumentReply".into(),
            ResolveInstrumentReply {
                found: true,
                instrument: Some(InstrumentRecord {
                    instrument_id: asked.instrument_id,
                    asset_class: AssetClass::Equity as i32,
                    currency: "USD".into(),
                    version: 1,
                    ..Default::default()
                }),
            }
            .encode_to_vec(),
        ))
    });
}

fn d(text: &str) -> Option<meridian_pb::v1::Decimal> {
    Some(text.parse::<Exact>().unwrap().to_wire())
}

fn read(value: &Option<meridian_pb::v1::Decimal>) -> String {
    Exact::from_wire(value.as_ref().unwrap())
        .unwrap()
        .to_string()
}

async fn send<M: Message>(
    bus: &Bus,
    topic: &str,
    message: &M,
    person: Option<&str>,
) -> Result<Vec<u8>, String> {
    let payload_type = format!(
        "meridian.v1.{}",
        std::any::type_name::<M>().rsplit("::").next().unwrap()
    );
    let stamp = Stamp {
        acting_for_subject: person.unwrap_or_default().to_string(),
        account_scope: None,
        ..Default::default()
    };
    bus.call_stamped(
        topic,
        &payload_type,
        message.encode_to_vec(),
        None,
        None,
        &stamp,
    )
    .await
    .map(|(_, payload)| payload)
    .map_err(|failed| failed.to_string())
}

fn opening(account: &str, instrument: &str) -> RecordOpeningBalanceRequest {
    RecordOpeningBalanceRequest {
        account_id: account.into(),
        as_of_date: "2026-09-08".into(),
        sources: vec![OpeningSource {
            kind: OpeningSourceKind::Custodian as i32,
            name: "Interactive Brokers".into(),
            as_of_date: "2026-09-08".into(),
            basis: PositionBasis::TradeDate as i32,
            street_records: vec![],
        }],
        positions: vec![OpeningPosition {
            instrument_id: instrument.into(),
            side: HoldingSide::Long as i32,
            trade_date_quantity: d("12.5"),
            settled_quantity: d("12.5"),
            pending: vec![],
            lots: vec![OpeningLot {
                quantity: d("12.5"),
                terms: Some(LotTerms {
                    cost: Some(Money {
                        amount: d("2250.00"),
                        currency_code: "USD".into(),
                        instrument_id: String::new(),
                    }),
                    acquired_date: "2025-03-14".into(),
                    ..Default::default()
                }),
            }],
        }],
        reason: "opening".into(),
        replaces_entry_id: String::new(),
        idempotency_key: unique("key"),
    }
}

async fn open(bus: &Bus, account: &str, instrument: &str) -> BookEntryReply {
    BookEntryReply::decode(
        &send(
            bus,
            RECORD_OPENING_BALANCE,
            &opening(account, instrument),
            Some(PERSON),
        )
        .await
        .unwrap()[..],
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_journal_refuses_an_update_or_a_delete() {
    let (bus, _) = wired();
    let account = unique("ACC");
    open(&bus, &account, "INS-AAPL").await;
    blocking(|| {
        let mut client = postgres::Client::connect(&url(), postgres::NoTls).unwrap();
        for statement in [
            "UPDATE book_entry SET kind = 'adjustment' WHERE account_id = $1",
            "DELETE FROM book_entry WHERE account_id = $1",
        ] {
            let refused = client.execute(statement, &[&account]).unwrap_err();
            assert!(
                format!("{refused:?}").contains("append-only"),
                "{statement}: {refused:?}"
            );
        }
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn an_opening_balance_is_journalled_once_and_answered_as_the_first() {
    let (bus, store) = wired();
    let account = unique("ACC");
    let request = opening(&account, "INS-AAPL");
    let first = send(&bus, RECORD_OPENING_BALANCE, &request, Some(PERSON))
        .await
        .unwrap();
    let again = send(&bus, RECORD_OPENING_BALANCE, &request, Some(PERSON))
        .await
        .unwrap();
    assert_eq!(
        first, again,
        "a duplicate by its key is answered as the first"
    );
    assert_eq!(blocking(|| store.journal(&account).unwrap().len()), 1);

    let mut second = opening(&account, "INS-AAPL");
    second.reason = "again".into();
    let refused = send(&bus, RECORD_OPENING_BALANCE, &second, Some(PERSON))
        .await
        .unwrap_err();
    assert!(refused.contains("opening balance standing"), "{refused}");
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_commands_are_numbered_without_holes() {
    let (bus, store) = wired();
    let accounts: Vec<String> = (0..6).map(|_| unique("ACC")).collect();
    let before = blocking(|| store.heads().unwrap()["P0"]);
    let mut sending = Vec::new();
    for account in &accounts {
        let bus = bus.clone();
        let request = opening(account, "INS-AAPL");
        sending.push(tokio::spawn(async move {
            send(&bus, RECORD_OPENING_BALANCE, &request, Some(PERSON)).await
        }));
    }
    for sent in sending {
        sent.await.unwrap().unwrap();
    }
    // Each entry took the next numbers, two each (a position and the
    // attributes), whoever else was writing: no number twice, none skipped
    // within an entry.
    let mut numbers: Vec<(u64, u64)> = accounts
        .iter()
        .flat_map(|account| blocking(|| store.journal(account).unwrap()))
        .map(|entry| (entry.first_sequence, entry.last_sequence))
        .collect();
    numbers.sort();
    for window in numbers.windows(2) {
        assert!(window[0].1 < window[1].0, "{numbers:?}");
    }
    for (first, last) in &numbers {
        assert_eq!(last - first, 1);
        assert!(*first > before);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_answer_in_order_within_their_scope_and_since_a_watermark() {
    let (bus, store) = wired();
    let account = unique("ACC");
    let other = unique("ACC");
    let opened = open(&bus, &account, "INS-AAPL").await;
    open(&bus, &other, "INS-MSFT").await;
    let at = opened.journal.unwrap().sequence;

    let read = |scope: Scope, account_id: &str, since: Option<u64>, cursor: &str| PositionsRead {
        scope,
        account_id: account_id.to_string(),
        since: since.map(|sequence| [("P0".to_string(), sequence)].into_iter().collect()),
        business_date: String::new(),
        at: None,
        limit: 1,
        cursor: cursor.to_string(),
    };
    let within = Scope::Within([account.clone(), other.clone()].into_iter().collect());
    let first = blocking(|| {
        store
            .positions(&read(within.clone(), "", None, ""))
            .unwrap()
    });
    assert_eq!(first.records.len(), 1);
    assert!(!first.next_cursor.is_empty());
    let second = blocking(|| {
        store
            .positions(&read(within.clone(), "", None, &first.next_cursor))
            .unwrap()
    });
    assert_eq!(second.records.len(), 1);
    assert_ne!(first.records[0].account_id, second.records[0].account_id);

    let only = Scope::Within([account.clone()].into_iter().collect());
    let page = blocking(|| store.positions(&read(only.clone(), "", None, "")).unwrap());
    assert!(page.records.iter().all(|held| held.account_id == account));
    let none = blocking(|| {
        store
            .positions(&read(Scope::Within(Default::default()), "", None, ""))
            .unwrap()
    });
    assert!(none.records.is_empty(), "an empty scope reads nothing");
    assert!(blocking(|| store.positions(&read(only, &other, None, ""))).is_err());

    let since = blocking(|| {
        store
            .positions(&read(within, &account, Some(at), ""))
            .unwrap()
    });
    assert!(
        since.records.is_empty(),
        "nothing of {account} changed since {at}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rebuild_reproduces_every_projection() {
    let (bus, store) = wired();
    let account = unique("ACC");
    open(&bus, &account, "INS-AAPL").await;
    let recorded = BookEntryReply::decode(
        &send(
            &bus,
            RECORD_BREAK,
            &RecordBreakRequest {
                account_id: account.clone(),
                subject: Some(record_break_request::Subject::Position(PositionKey {
                    instrument_id: "INS-AAPL".into(),
                    side: HoldingSide::Long as i32,
                })),
                category: BreakCategory::TradeDateQuantity as i32,
                differences: vec![BreakDifference {
                    field: "trade_date_quantity".into(),
                    book: None,
                    street: None,
                }],
                business_date: "2026-09-09".into(),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap()[..],
    )
    .unwrap();
    let break_id = recorded.breaks[0].break_id.clone();
    send(
        &bus,
        RESOLVE_BREAK,
        &ResolveBreakRequest {
            account_id: account.clone(),
            break_ids: vec![break_id],
            reason: "books the buy".into(),
            resolution: Some(resolve_break_request::Resolution::Adjustment(Adjustment {
                effective_date: "2026-09-09".into(),
                lines: vec![MovementLine {
                    instrument_id: "INS-AAPL".into(),
                    side: HoldingSide::Long as i32,
                    bucket: SettlementBucket::Settled as i32,
                    quantity: d("2.5"),
                    opens_lot: Some(LotTerms {
                        cost: Some(Money {
                            amount: d("567.50"),
                            currency_code: "USD".into(),
                            instrument_id: String::new(),
                        }),
                        acquired_date: "2026-09-09".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            })),
            idempotency_key: String::new(),
        },
        Some(PERSON),
    )
    .await
    .unwrap();

    let positions = |store: &PostgresStore| {
        store
            .positions(&PositionsRead {
                scope: Scope::Within([account.clone()].into_iter().collect()),
                account_id: account.clone(),
                since: None,
                business_date: String::new(),
                at: None,
                limit: 100,
                cursor: String::new(),
            })
            .unwrap()
            .records
    };
    let breaks = |store: &PostgresStore| {
        store
            .breaks(&BreaksRead {
                scope: Scope::Within([account.clone()].into_iter().collect()),
                account_id: account.clone(),
                states: vec![],
                since: None,
                limit: 100,
                cursor: String::new(),
            })
            .unwrap()
            .records
    };
    let (before_positions, before_breaks) = blocking(|| (positions(&store), breaks(&store)));
    assert_eq!(read(&before_positions[0].trade_date_quantity), "15.0");
    rebuild(&store).await;
    assert_eq!(blocking(|| positions(&store)), before_positions);
    assert_eq!(blocking(|| breaks(&store)), before_breaks);

    // By replay, the end of D0 as known now: the adjustment came after it.
    let dated = blocking(|| {
        store
            .positions(&PositionsRead {
                scope: Scope::Within([account.clone()].into_iter().collect()),
                account_id: account.clone(),
                since: None,
                business_date: "2026-09-08".into(),
                at: None,
                limit: 100,
                cursor: String::new(),
            })
            .unwrap()
    });
    assert_eq!(read(&dated.records[0].trade_date_quantity), "12.5");
}

#[test]
fn a_codes_resolution_is_kept_once_in_the_book() {
    use meridian_domain::money::Resolution;
    let store = store();
    let resolution = |instrument: &str| Resolution {
        code: "XTS".into(),
        instrument_id: instrument.into(),
        resolved_at_ns: 1,
        backfilled: true,
    };
    store.keep_cash_instrument(&resolution("LCL-XTS")).unwrap();
    store
        .keep_cash_instrument(&resolution("LCL-OTHER"))
        .unwrap();
    assert!(store
        .cash_instruments()
        .unwrap()
        .contains(&resolution("LCL-XTS")));
    for code in store.currency_codes().unwrap() {
        assert!(meridian_domain::money::is_iso4217(&code), "{code}");
    }
}

// ── The order path (W11, contract v22) ──────────────────────────────────────

/// A command as a sidecar sends it on the order path: for a person, with
/// the write scope of a command naming its record by ID alone and a
/// placement's link verdict.
async fn send_order<M: Message>(
    bus: &Bus,
    topic: &str,
    message: &M,
    person: Option<&str>,
    scope: Option<Vec<String>>,
) -> Result<Vec<u8>, String> {
    let payload_type = format!(
        "meridian.v1.{}",
        std::any::type_name::<M>().rsplit("::").next().unwrap()
    );
    let stamp = Stamp {
        acting_for_subject: person.unwrap_or_default().to_string(),
        account_scope: scope,
        connectivity_allowed: true,
        ..Default::default()
    };
    bus.call_stamped(
        topic,
        &payload_type,
        message.encode_to_vec(),
        None,
        None,
        &stamp,
    )
    .await
    .map(|(_, payload)| payload)
    .map_err(|failed| failed.to_string())
}

fn usd(text: &str) -> Option<Money> {
    Some(Money {
        amount: d(text),
        currency_code: "USD".into(),
        instrument_id: String::new(),
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn an_order_reaches_its_fill_through_postgres_and_a_rebuild_reproduces_its_records() {
    use meridian_bor::order_service::*;
    let (bus, store) = wired();
    meridian_domain::money::learn("USD", "INS-CASH-USD");
    let account = unique("ACC");
    let scope = Some(vec![account.clone()]);
    send_order(
        &bus,
        SET_ACCOUNT_ATTRIBUTE,
        &SetAccountAttributeRequest {
            account_id: account.clone(),
            attribute: Some(set_account_attribute_request::Attribute::WaitsForCompliance(false)),
            reason: "no compliance plugin in this test".into(),
        },
        Some(PERSON),
        None,
    )
    .await
    .unwrap();
    let mut proposal = ProposeOrderRequest {
        account_id: account.clone(),
        instrument_id: "INS-AAPL".into(),
        sleeve: "core".into(),
        side: Side::Buy as i32,
        quantity: d("10"),
        order_type: OrderType::Limit as i32,
        limit_price: usd("227.50"),
        time_in_force: TimeInForce::Day as i32,
        idempotency_key: unique("propose"),
        ..Default::default()
    };
    let order = OrderReply::decode(
        &send_order(&bus, PROPOSE_ORDER, &proposal, Some(PERSON), None)
            .await
            .unwrap()[..],
    )
    .unwrap()
    .order
    .unwrap();
    // The same proposal under its key again is answered as the first.
    let again = OrderReply::decode(
        &send_order(&bus, PROPOSE_ORDER, &proposal, Some(PERSON), None)
            .await
            .unwrap()[..],
    )
    .unwrap()
    .order
    .unwrap();
    assert_eq!(again.order_id, order.order_id);
    proposal.quantity = d("11");
    let conflict = send_order(&bus, PROPOSE_ORDER, &proposal, Some(PERSON), None)
        .await
        .unwrap_err();
    assert!(conflict.contains("[refusal-reason 10]"), "{conflict}");

    assert_eq!(
        blocking(|| store
            .owner(meridian_bor::store::Record::Order, &order.order_id)
            .unwrap()),
        Some(account.clone())
    );
    // Outside the write scope: denied, nothing changed.
    let denied = send_order(
        &bus,
        RELEASE_ORDER,
        &ReleaseOrderRequest {
            order_id: order.order_id.clone(),
            idempotency_key: String::new(),
        },
        Some(PERSON),
        Some(vec!["ACC-elsewhere".into()]),
    )
    .await
    .unwrap_err();
    assert!(
        denied.contains("[refusal-status permission-denied]"),
        "{denied}"
    );
    send_order(
        &bus,
        RELEASE_ORDER,
        &ReleaseOrderRequest {
            order_id: order.order_id.clone(),
            idempotency_key: String::new(),
        },
        Some(PERSON),
        scope.clone(),
    )
    .await
    .unwrap();
    let placement = PlacementReply::decode(
        &send_order(
            &bus,
            CREATE_PLACEMENT,
            &CreatePlacementRequest {
                order_id: order.order_id.clone(),
                connectivity_instance_id: "operations-test-1".into(),
                quantity: d("10"),
                ..Default::default()
            },
            Some(PERSON),
            scope.clone(),
        )
        .await
        .unwrap()[..],
    )
    .unwrap()
    .placement
    .unwrap();
    let filled = FactReply::decode(
        &send_order(
            &bus,
            REPORT_EXECUTION,
            &ReportExecutionRequest {
                placement_id: placement.placement_id.clone(),
                account_id: account.clone(),
                venue_execution_id: unique("fill"),
                kind: ExecutionKind::VenueReported as i32,
                quantity: d("10"),
                price: usd("227.41"),
                trade_date: "2026-10-13".into(),
                settlement_date: "2026-10-14".into(),
                ..Default::default()
            },
            None,
            scope.clone(),
        )
        .await
        .unwrap()[..],
    )
    .unwrap();
    assert_eq!(
        filled.order.unwrap().status.unwrap().state,
        OrderState::Filled as i32
    );

    // A fill whose venue account code no ems mapped: kept naming no account,
    // read by a core reader naming the book of no account (W11.13).
    let astray = FactReply::decode(
        &send_order(
            &bus,
            REPORT_EXECUTION,
            &ReportExecutionRequest {
                placement_id: placement.placement_id.clone(),
                venue_execution_id: unique("astray"),
                venue_account_code: "UNMAPPED-1".into(),
                raw_record: Some(meridian_pb::v1::RawRecordRef {
                    instance_id: "fix-1".into(),
                    key: "orders/astray".into(),
                }),
                kind: ExecutionKind::VenueReported as i32,
                quantity: d("1"),
                price: usd("227.41"),
                trade_date: "2026-10-13".into(),
                settlement_date: "2026-10-14".into(),
                ..Default::default()
            },
            None,
            scope.clone(),
        )
        .await
        .unwrap()[..],
    )
    .unwrap()
    .unplaced
    .unwrap();
    assert!(astray.account_id.is_empty());
    let unplaced = |store: &PostgresStore| {
        store
            .orders(&meridian_bor::store::OrdersRead {
                scope: Scope::Everything,
                account_id: meridian_bor::store::NO_ACCOUNT.into(),
                since: None,
                limit: 500,
                cursor: String::new(),
                execution_state: 0,
            })
            .unwrap()
    };
    let none = blocking(|| unplaced(&store));
    assert!(none.orders.is_empty() && none.executions.is_empty());
    assert!(none
        .unplaced
        .iter()
        .any(|fact| fact.unplaced_id == astray.unplaced_id));

    let orders = |store: &PostgresStore| {
        store
            .orders(&meridian_bor::store::OrdersRead {
                scope: Scope::Within([account.clone()].into_iter().collect()),
                account_id: account.clone(),
                since: None,
                limit: 100,
                cursor: String::new(),
                execution_state: 0,
            })
            .unwrap()
    };
    let before = blocking(|| orders(&store));
    assert_eq!(before.orders.len(), 1);
    assert_eq!(before.placements.len(), 1);
    assert_eq!(before.executions.len(), 1);
    assert_eq!(before.orders[0].executions.len(), 1);
    // Paged one record at a time, the stream reads the same records.
    let mut cursor = String::new();
    let mut paged = 0;
    loop {
        let page = blocking(|| {
            store
                .orders(&meridian_bor::store::OrdersRead {
                    scope: Scope::Within([account.clone()].into_iter().collect()),
                    account_id: String::new(),
                    since: None,
                    limit: 1,
                    cursor: cursor.clone(),
                    execution_state: 0,
                })
                .unwrap()
        });
        paged += page.orders.len() + page.placements.len() + page.executions.len();
        if page.next_cursor.is_empty() {
            break;
        }
        cursor = page.next_cursor;
    }
    assert_eq!(paged, 3);
    let history = blocking(|| {
        store
            .order_history(&meridian_bor::store::HistoryRead {
                scope: Scope::Everything,
                account_id: account.clone(),
                order_id: order.order_id.clone(),
                limit: 100,
                cursor: String::new(),
            })
            .unwrap()
    });
    let kinds: Vec<String> = history
        .records
        .iter()
        .map(|entry| entry.kind.clone())
        .collect();
    assert_eq!(
        kinds,
        [
            "order-proposed",
            "order-released",
            "placement-created",
            "execution-recorded"
        ]
    );
    // The position is the sleeve's, with its lot.
    let positions = blocking(|| {
        store
            .positions(&PositionsRead {
                scope: Scope::Within([account.clone()].into_iter().collect()),
                account_id: account.clone(),
                since: None,
                business_date: String::new(),
                at: None,
                limit: 100,
                cursor: String::new(),
            })
            .unwrap()
            .records
    });
    let aapl = positions
        .iter()
        .find(|position| position.instrument_id == "INS-AAPL")
        .unwrap();
    assert_eq!(
        (aapl.sleeve.as_str(), read(&aapl.trade_date_quantity)),
        ("core", "10".into())
    );
    rebuild(&store).await;
    let after = blocking(|| orders(&store));
    assert_eq!(before.orders, after.orders);
    assert_eq!(before.placements, after.placements);
    assert_eq!(before.executions, after.executions);
}

/// Step 3d, PO item 74 (review c5k, L4): a charge reported again under its
/// key, after a lost reply, is answered from the book's own key index in
/// Postgres, and no second charge is minted.
#[tokio::test(flavor = "multi_thread")]
async fn a_charge_reported_again_under_its_key_is_recorded_once_in_postgres() {
    use meridian_bor::order_service::*;
    let (bus, store) = wired();
    meridian_domain::money::learn("USD", "INS-CASH-USD");
    let account = unique("ACC");
    let scope = Some(vec![account.clone()]);
    send_order(
        &bus,
        SET_ACCOUNT_ATTRIBUTE,
        &SetAccountAttributeRequest {
            account_id: account.clone(),
            attribute: Some(set_account_attribute_request::Attribute::WaitsForCompliance(false)),
            reason: "no compliance plugin in this test".into(),
        },
        Some(PERSON),
        None,
    )
    .await
    .unwrap();
    let order = OrderReply::decode(
        &send_order(
            &bus,
            PROPOSE_ORDER,
            &ProposeOrderRequest {
                account_id: account.clone(),
                instrument_id: "INS-AAPL".into(),
                side: Side::Buy as i32,
                quantity: d("10"),
                order_type: OrderType::Limit as i32,
                limit_price: usd("227.50"),
                time_in_force: TimeInForce::Day as i32,
                ..Default::default()
            },
            Some(PERSON),
            None,
        )
        .await
        .unwrap()[..],
    )
    .unwrap()
    .order
    .unwrap();
    send_order(
        &bus,
        RELEASE_ORDER,
        &ReleaseOrderRequest {
            order_id: order.order_id.clone(),
            idempotency_key: String::new(),
        },
        Some(PERSON),
        scope.clone(),
    )
    .await
    .unwrap();
    let placement = PlacementReply::decode(
        &send_order(
            &bus,
            CREATE_PLACEMENT,
            &CreatePlacementRequest {
                order_id: order.order_id.clone(),
                connectivity_instance_id: "operations-test-1".into(),
                quantity: d("10"),
                ..Default::default()
            },
            Some(PERSON),
            scope.clone(),
        )
        .await
        .unwrap()[..],
    )
    .unwrap()
    .placement
    .unwrap();
    let on = placement.placement_id.clone();
    let charge = |amount: &str, key: &str| ReportChargeRequest {
        placement_id: on.clone(),
        fee: Some(Fee {
            kind: FeeKind::Commission as i32,
            amount: usd(amount),
            ..Default::default()
        }),
        trade_date: "2026-10-13".into(),
        settlement_date: "2026-10-14".into(),
        idempotency_key: key.into(),
        ..Default::default()
    };
    let report = |request: ReportChargeRequest| {
        let (bus, scope) = (bus.clone(), scope.clone());
        async move {
            send_order(&bus, REPORT_CHARGE, &request, None, scope)
                .await
                .map(|reply| FactReply::decode(&reply[..]).unwrap())
        }
    };
    let entries = || blocking(|| store.journal(&account).unwrap().len());
    let key = format!("charge:{}|{on}", unique("venue"));
    let before = entries();
    let first = report(charge("1.00", &key)).await.unwrap();
    assert_eq!(entries(), before + 1);

    // The same key again: the first's answer, no second charge.
    let again = report(charge("1.00", &key)).await.unwrap();
    assert_eq!(again, first);
    assert_eq!(entries(), before + 1);

    // The same key with other content: refused, nothing applied.
    let conflict = report(charge("2.00", &key)).await.unwrap_err();
    assert!(conflict.contains("[refusal-reason 10]"), "{conflict}");
    assert_eq!(entries(), before + 1);

    // A new key: a second charge.
    let second = report(charge("1.00", &format!("{key}-2"))).await.unwrap();
    assert_ne!(
        second.charge.unwrap().charge_id,
        first.charge.unwrap().charge_id
    );
    assert_eq!(entries(), before + 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn instructions_stand_in_their_partitions_and_a_day_order_comes_due() {
    use meridian_bor::order_service::*;
    let (bus, store) = wired();
    let account = unique("ACC");
    let target = unique("plugin");
    let set = SetTradingInstructionRequest {
        target: Some(TradingInstructionTarget {
            instance_id: target.clone(),
            ..Default::default()
        }),
        value: TradingInstructionValue::Pause as i32,
        reason: "pause the plugin".into(),
        setter_level: SetterLevel::PluginAdmin as i32,
    };
    let reply = TradingInstructionReply::decode(
        &send_order(&bus, SET_TRADING_INSTRUCTION, &set, Some(PERSON), None)
            .await
            .unwrap()[..],
    )
    .unwrap();
    assert_eq!(reply.journal.unwrap().partition, "control");
    let listed = blocking(|| {
        store
            .instructions(&meridian_bor::store::InstructionsRead {
                since: None,
                limit: 500,
                cursor: String::new(),
            })
            .unwrap()
    });
    assert!(listed.records.iter().any(|held| held
        .target
        .as_ref()
        .is_some_and(|named| named.instance_id == target)));
    // Read since the control partition's head: nothing new.
    let since = listed.as_of.clone();
    let none = blocking(|| {
        store
            .instructions(&meridian_bor::store::InstructionsRead {
                since: Some(since.clone()),
                limit: 500,
                cursor: String::new(),
            })
            .unwrap()
    });
    assert!(none.records.is_empty());

    send_order(
        &bus,
        SET_ACCOUNT_ATTRIBUTE,
        &SetAccountAttributeRequest {
            account_id: account.clone(),
            attribute: Some(set_account_attribute_request::Attribute::WaitsForCompliance(false)),
            reason: "no compliance plugin".into(),
        },
        Some(PERSON),
        None,
    )
    .await
    .unwrap();
    let order = OrderReply::decode(
        &send_order(
            &bus,
            PROPOSE_ORDER,
            &ProposeOrderRequest {
                account_id: account.clone(),
                instrument_id: "INS-AAPL".into(),
                side: Side::Buy as i32,
                quantity: d("1"),
                time_in_force: TimeInForce::Day as i32,
                ..Default::default()
            },
            Some(PERSON),
            None,
        )
        .await
        .unwrap()[..],
    )
    .unwrap()
    .order
    .unwrap();
    send_order(
        &bus,
        RELEASE_ORDER,
        &ReleaseOrderRequest {
            order_id: order.order_id.clone(),
            idempotency_key: String::new(),
        },
        Some(PERSON),
        None,
    )
    .await
    .unwrap();
    let due = |at: i64| blocking(|| store.expiring(at).unwrap());
    let now = meridian_clock::Clock::now_ns(&meridian_clock::SystemClock);
    assert!(!due(now).iter().any(|(_, id)| *id == order.order_id));
    let later = now + 2 * 86_400 * 1_000_000_000;
    assert!(due(later)
        .iter()
        .any(|(held, id)| *id == order.order_id && *held == account));
}

/// Contract v23 (W11.11, W11.17, W11.23, W11.24): fills on no placement kept
/// unmatched, found by their IDs alone for an accept or a DK, listed by
/// state in SQL -- a busted fill and its bust left out, a corrected one
/// listed as its correction -- and rebuilt from the journal alike.
#[tokio::test(flavor = "multi_thread")]
async fn unmatched_fills_are_found_by_id_listed_by_state_and_rebuilt() {
    use meridian_bor::order_service::*;
    let (bus, store) = wired();
    meridian_domain::money::learn("USD", "INS-CASH-USD");
    let account = unique("ACC-v23");
    let scope = Some(vec![account.clone()]);
    let fill = |quantity: &str, supersedes: &str| ReportExecutionRequest {
        account_id: account.clone(),
        venue_account_code: "SNAP-1".into(),
        venue_execution_id: unique("v23"),
        kind: ExecutionKind::VenueReported as i32,
        quantity: d(quantity),
        price: usd("227.41"),
        trade_date: "2026-10-13".into(),
        settlement_date: if quantity == "0" {
            String::new()
        } else {
            "2026-10-14".into()
        },
        instrument_id: "INS-AAPL".into(),
        side: Side::Buy as i32,
        venue_order_id: "ALP-1".into(),
        supersedes: supersedes.into(),
        ..Default::default()
    };
    let mut kept = Vec::new();
    for _ in 0..4 {
        let reply = send_order(&bus, REPORT_EXECUTION, &fill("5", ""), None, scope.clone())
            .await
            .unwrap();
        let execution = FactReply::decode(&reply[..]).unwrap().execution.unwrap();
        assert_eq!(execution.state, ExecutionState::Unmatched as i32);
        kept.push(execution.execution_id);
    }
    // A bust of the first; a correction of the second.
    send_order(
        &bus,
        REPORT_EXECUTION,
        &fill("0", &kept[0]),
        None,
        scope.clone(),
    )
    .await
    .unwrap();
    let corrected = FactReply::decode(
        &send_order(
            &bus,
            REPORT_EXECUTION,
            &fill("6", &kept[1]),
            None,
            scope.clone(),
        )
        .await
        .unwrap()[..],
    )
    .unwrap()
    .execution
    .unwrap()
    .execution_id;
    // The third accepted to a new order; the fourth answered DK.
    let accepted = AcceptExecutionReply::decode(
        &send_order(
            &bus,
            ACCEPT_EXECUTION,
            &AcceptExecutionRequest {
                execution_ids: vec![kept[2].clone()],
                matched_order: Some(accept_execution_request::MatchedOrder::NewOrder(
                    NewOrderFromExecutions::default(),
                )),
                note: String::new(),
            },
            Some(PERSON),
            scope.clone(),
        )
        .await
        .unwrap()[..],
    )
    .unwrap();
    assert_eq!(
        accepted.order.unwrap().origin,
        OrderOrigin::AcceptedExecution as i32
    );
    send_order(
        &bus,
        DONT_KNOW_EXECUTION,
        &DontKnowExecutionRequest {
            execution_ids: vec![kept[3].clone()],
            reason: DkReason::NoMatchingOrder as i32,
            text: String::new(),
        },
        Some(PERSON),
        scope.clone(),
    )
    .await
    .unwrap();
    let listed = |store: &PostgresStore, state: ExecutionState| {
        let page = store
            .orders(&meridian_bor::store::OrdersRead {
                scope: Scope::Within([account.clone()].into_iter().collect()),
                account_id: account.clone(),
                since: None,
                limit: 100,
                cursor: String::new(),
                execution_state: state as i32,
            })
            .unwrap();
        assert!(page.orders.is_empty() && page.placements.is_empty() && page.unplaced.is_empty());
        let mut ids: Vec<String> = page
            .executions
            .into_iter()
            .map(|execution| execution.execution_id)
            .collect();
        ids.sort();
        ids
    };
    let check = |store: &PostgresStore| {
        assert_eq!(
            listed(store, ExecutionState::Unmatched),
            vec![corrected.clone()]
        );
        assert_eq!(
            listed(store, ExecutionState::Matched),
            vec![kept[2].clone()]
        );
        assert_eq!(
            listed(store, ExecutionState::DontKnow),
            vec![kept[3].clone()]
        );
    };
    blocking(|| check(&store));
    rebuild(&store).await;
    blocking(|| check(&store));
}

/// Contract v25 (W9.10): every so many entries the book keeps the account's
/// projection, a command and a dated read start from the latest and replay
/// the tail, answering exactly what a full replay does; a snapshot that does
/// not read is passed over; the latest few are kept; a rebuild makes them
/// again.
#[tokio::test(flavor = "multi_thread")]
async fn a_snapshot_and_its_tail_answer_as_the_whole_journal_does() {
    // No other test's rebuild while this one counts its snapshots.
    let _alone = REBUILDS.lock().await;
    let store = blocking(|| {
        let store = PostgresStore::connect(&url(), 8)
            .expect("could not reach the test database")
            .with_snapshot_interval(3);
        store.migrate(&meridian_clock::SystemClock).unwrap();
        Arc::new(store)
    });
    let (bus, store) = wired_with(store);
    let account = unique("ACC");
    open(&bus, &account, "INS-AAPL").await;
    let mut watermarks = Vec::new();
    for day in 9..=19 {
        let reply = BookEntryReply::decode(
            &send(
                &bus,
                RECORD_BREAK,
                &RecordBreakRequest {
                    account_id: account.clone(),
                    subject: Some(record_break_request::Subject::Position(PositionKey {
                        instrument_id: "INS-AAPL".into(),
                        side: HoldingSide::Long as i32,
                    })),
                    category: BreakCategory::TradeDateQuantity as i32,
                    differences: vec![BreakDifference {
                        field: "trade_date_quantity".into(),
                        book: None,
                        street: None,
                    }],
                    business_date: format!("2026-09-{day:02}"),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap()[..],
        )
        .unwrap();
        watermarks.push(reply.journal.unwrap().sequence);
    }
    let client = || postgres::Client::connect(&url(), postgres::NoTls).unwrap();
    let kept: i64 = blocking(|| {
        client()
            .query_one(
                "SELECT count(*) FROM book_snapshot WHERE account_id = $1",
                &[&account],
            )
            .unwrap()
            .get(0)
    });
    assert!(
        (1..=meridian_bor::snapshot::KEPT as i64).contains(&kept),
        "{kept} kept"
    );
    let journal = blocking(|| store.journal(&account)).unwrap();
    assert_eq!(journal.len(), 12);
    // The book's health (ruling 155): the journal, the tail since the last
    // snapshot, and when it was taken.
    let health = blocking(|| store.book_health(Some(&account), "", 10)).unwrap();
    assert_eq!(health.len(), 1);
    assert_eq!(health[0].journal_entries, 12);
    assert!(health[0].entries_since_snapshot < 3, "{health:?}");
    assert!(health[0].last_snapshot_at_ns > 0);
    let whole = meridian_bor::Book::replay(&account, &journal[0].partition, &journal).unwrap();
    assert_eq!(blocking(|| store.book(&account)).unwrap(), whole);
    // A dated read at each watermark, from a snapshot where one fits.
    let partition = journal[0].partition.clone();
    for (n, watermark) in watermarks.iter().enumerate() {
        let at: meridian_bor::store::Mark = [(partition.clone(), *watermark)].into_iter().collect();
        let date = format!("2026-09-{:02}", 9 + n);
        let read = blocking(|| store.book_as_of(&account, &at, Some(&date))).unwrap();
        let replayed = meridian_bor::Book::as_of(
            &account,
            &partition,
            &journal,
            Some(*watermark),
            Some(&date),
        )
        .unwrap();
        assert_eq!(read, replayed, "at {watermark}, {date}");
    }
    // A snapshot that does not read is passed over, and the book stands.
    blocking(|| {
        client()
            .execute(
                "UPDATE book_snapshot SET snapshot = 'not a snapshot' WHERE account_id = $1",
                &[&account],
            )
            .unwrap()
    });
    assert_eq!(blocking(|| store.book(&account)).unwrap(), whole);
    // A rebuild makes them again from the journal.
    blocking(|| store.rebuild()).unwrap();
    let rebuilt: Vec<u8> = blocking(|| {
        client()
            .query_one(
                "SELECT snapshot FROM book_snapshot WHERE account_id = $1
                  ORDER BY last_sequence DESC LIMIT 1",
                &[&account],
            )
            .unwrap()
            .get(0)
    });
    assert_eq!(meridian_bor::snapshot::decode(&rebuilt).unwrap(), whole);
    let health = blocking(|| store.book_health(Some(&account), "", 10)).unwrap();
    assert_eq!(health[0].entries_since_snapshot, 0, "{health:?}");
}

/// v26's core (W9.10's note "when a snapshot is taken", decided 2026-10-18,
/// found on the development deployment): an account no command writes to,
/// or one whose snapshots are of an older format, is not replayed whole on
/// every read. The start's backfill keeps a snapshot for one past the
/// interval with none of this format, and a read replaying past it keeps
/// one too, replacing an older format's as at the same entry.
#[tokio::test(flavor = "multi_thread")]
async fn a_quiet_or_upgraded_account_is_snapshotted_at_start_and_by_a_read() {
    let _alone = REBUILDS.lock().await;
    let store = blocking(|| {
        let store = PostgresStore::connect(&url(), 8)
            .expect("could not reach the test database")
            .with_snapshot_interval(3);
        store.migrate(&meridian_clock::SystemClock).unwrap();
        Arc::new(store)
    });
    let (bus, store) = wired_with(store);
    let account = unique("ACC");
    open(&bus, &account, "INS-AAPL").await;
    for day in 9..=13 {
        send(
            &bus,
            RECORD_BREAK,
            &RecordBreakRequest {
                account_id: account.clone(),
                subject: Some(record_break_request::Subject::Position(PositionKey {
                    instrument_id: "INS-AAPL".into(),
                    side: HoldingSide::Long as i32,
                })),
                category: BreakCategory::TradeDateQuantity as i32,
                differences: vec![BreakDifference {
                    field: "trade_date_quantity".into(),
                    book: None,
                    street: None,
                }],
                business_date: format!("2026-09-{day:02}"),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    }
    let client = || postgres::Client::connect(&url(), postgres::NoTls).unwrap();
    let journal = blocking(|| store.journal(&account)).unwrap();
    assert_eq!(journal.len(), 6);
    let whole = meridian_bor::Book::replay(&account, &journal[0].partition, &journal).unwrap();
    // As an upgraded account's: every snapshot of an older format.
    let age = || {
        blocking(|| {
            client()
                .execute(
                    "UPDATE book_snapshot SET format = 1 WHERE account_id = $1",
                    &[&account],
                )
                .unwrap()
        })
    };
    let since =
        || blocking(|| store.book_health(Some(&account), "", 1)).unwrap()[0].entries_since_snapshot;
    let latest = || -> (i32, Vec<u8>) {
        blocking(|| {
            let row = client()
                .query_one(
                    "SELECT format, snapshot FROM book_snapshot WHERE account_id = $1
                      ORDER BY last_sequence DESC LIMIT 1",
                    &[&account],
                )
                .unwrap();
            (row.get(0), row.get(1))
        })
    };
    age();
    assert_eq!(since(), 6, "none of this format");
    // The start's backfill.
    assert_eq!(
        blocking(|| store.backfill_snapshots(Some(&account))).unwrap(),
        1
    );
    assert_eq!(since(), 0);
    let (format, bytes) = latest();
    assert_eq!(format, meridian_bor::snapshot::FORMAT);
    assert_eq!(meridian_bor::snapshot::decode(&bytes).unwrap(), whole);
    // Nothing due again.
    assert_eq!(
        blocking(|| store.backfill_snapshots(Some(&account))).unwrap(),
        0
    );
    // A read replaying past the interval keeps one, replacing the older
    // format's kept as at the same entry.
    age();
    assert_eq!(since(), 6);
    assert_eq!(blocking(|| store.book(&account)).unwrap(), whole);
    assert_eq!(since(), 0);
    assert_eq!(latest().0, meridian_bor::snapshot::FORMAT);
    // And a read of the book now by a watermark, with no date, the same.
    age();
    let heads = blocking(|| store.heads()).unwrap();
    let read = blocking(|| store.book_as_of(&account, &heads, None)).unwrap();
    assert_eq!(read, whole);
    assert_eq!(since(), 0);
}

// ── The change log (step 3a) ──

/// A store of a small pool: the suite's tests run at once, and the test
/// database admits so many connections.
fn small() -> Arc<PostgresStore> {
    let store = PostgresStore::connect(&url(), 2).expect("could not reach the test database");
    store
        .migrate(&meridian_clock::SystemClock)
        .expect("could not create the schema");
    Arc::new(store)
}

/// A store over a schema of its own, so a takeover raises no other test's
/// epoch. The schema is dropped with the last clone of its URL (PO 65).
fn isolated() -> (Scratch, Arc<PostgresStore>) {
    let url = Scratch::schema(&url(), &unique("bor_fence").replace('-', "_"));
    let store = PostgresStore::connect(&url, 2).expect("could not reach the test database");
    store
        .migrate(&meridian_clock::SystemClock)
        .expect("could not create the schema");
    (url, Arc::new(store))
}

fn followed(store: &PostgresStore, partition: &str, page: usize) -> Vec<meridian_changes::Change> {
    let mut out = Vec::new();
    meridian_changes::catch_up(
        |after| store.read_changes(partition, after, page),
        0,
        |change| out.push(change.clone()),
    )
    .expect("the log reads");
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn a_follower_given_the_journal_in_any_page_split_builds_the_writers_books() {
    let (bus, store) = wired_with(blocking(small));
    let accounts: Vec<String> = (0..3).map(|_| unique("ACC")).collect();
    for account in &accounts {
        open(&bus, account, &unique("INS")).await;
    }
    for page in [1usize, 2_000, meridian_changes::PAGE_MOST] {
        let changes = blocking(|| followed(&store, "P0", page));
        // No holes: each entry starts where the one before it ended.
        let entries: Vec<meridian_bor::journal::Entry> = changes
            .iter()
            .map(|change| meridian_bor::journal::Entry::from_change(change).unwrap())
            .collect();
        for pair in entries.windows(2) {
            assert_eq!(
                pair[1].first_sequence,
                pair[0].last_sequence + 1,
                "page {page}"
            );
        }
        for account in &accounts {
            let mine: Vec<_> = entries
                .iter()
                .filter(|entry| &entry.account_id == account)
                .cloned()
                .collect();
            let journal = blocking(|| store.journal(account)).unwrap();
            assert_eq!(mine, journal, "page {page}");
            let built = meridian_bor::book::Book::replay(account, "P0", &mine).unwrap();
            let held = blocking(|| store.book(account)).unwrap();
            assert_eq!(built.positions, held.positions, "page {page}");
        }
    }
}

/// A writer stopped mid-entry -- its node paused, its process frozen --
/// holds the head row it took until its client moves again, and the server
/// never hears it go: the takeover ends its session rather than wait on it
/// (#99's ha run 42496994613: node-freeze's raise waited 35 s, under load).
#[test]
fn a_takeover_is_not_held_up_by_a_deposed_writer_stopped_mid_transaction() {
    // Synchronous throughout: the stalled writer is a plain client, and a
    // client dropped on an async runtime's worker cannot close itself.
    let (url, writer) = isolated();
    assert_eq!(writer.adopt().unwrap(), 1);
    let mut stalled = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    stalled
        .batch_execute("BEGIN; SELECT epoch FROM book_partition_head FOR UPDATE")
        .unwrap();
    let standby = PostgresStore::connect(&url, 1).unwrap();
    let began = std::time::Instant::now();
    assert_eq!(
        standby
            .take_over(&meridian_changes::TookOver::unnamed("a stalled writer"))
            .unwrap(),
        2
    );
    assert!(
        began.elapsed() < std::time::Duration::from_secs(5),
        "the takeover waited {:?} on the stalled writer",
        began.elapsed()
    );
    // Its transaction ended with its session: nothing it does commits.
    assert!(stalled.batch_execute("COMMIT").is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deposed_writers_command_is_refused_after_a_takeover_and_leaves_no_hole() {
    let (url, writer) = blocking(isolated);
    let (bus, writer) = wired_with(writer);
    assert_eq!(blocking(|| writer.adopt()).unwrap(), 1);
    let account = unique("ACC");
    open(&bus, &account, &unique("INS")).await;
    let standby = Arc::new(PostgresStore::connect(&url, 1).unwrap());
    assert_eq!(
        blocking(|| standby.take_over(&meridian_changes::TookOver::unnamed("a test"))).unwrap(),
        2
    );
    // The takeover is its own record, in its own transaction with the
    // epoch it raised (step 3b, `WriterTookOver`).
    let took = {
        let url = url.clone();
        let read = move || {
            let mut held = postgres::Client::connect(&url, postgres::NoTls).unwrap();
            let row = held.query_one("SELECT why, coalesce(array_length(partitions, 1), 0) FROM book_writer_took_over WHERE epoch = 2", &[]).unwrap();
            (row.get::<_, String>(0), row.get::<_, i32>(1))
        };
        blocking(read)
    };
    assert_eq!(took.0, "a test");
    assert!(
        took.1 >= 2,
        "the partitions it took, control among them: {took:?}"
    );
    let refused = send(
        &bus,
        RECORD_OPENING_BALANCE,
        &opening(&unique("ACC"), &unique("INS")),
        Some(PERSON),
    )
    .await
    .unwrap_err();
    assert!(refused.contains(meridian_changes::FENCED), "{refused}");
    assert!(blocking(|| writer.rebuild()).is_err());
    let (bus, standby) = wired_with(standby);
    open(&bus, &unique("ACC"), &unique("INS")).await;
    let changes = blocking(|| followed(&standby, "P0", meridian_changes::PAGE_MOST));
    let epochs: Vec<u64> = changes.iter().map(|change| change.epoch).collect();
    assert_eq!(epochs, vec![1, 2]);
    let entries: Vec<_> = changes
        .iter()
        .map(|change| meridian_bor::journal::Entry::from_change(change).unwrap())
        .collect();
    assert_eq!(entries[1].first_sequence, entries[0].last_sequence + 1);
    let reply =
        blocking(|| standby.read_changes("control", 0, meridian_changes::PAGE_MOST)).unwrap();
    assert_eq!(reply.epoch, 2);
}

/// A command a fenced writer's commit refuses -- its epoch passed by a
/// takeover, or the writer told to stop (`stop_committing`, a writer fenced
/// while it serves on) -- is answered UNAVAILABLE with retry-after 1, to be
/// asked again of the writer that serves by then (ruled 2026-11-18, items
/// 110 and 120; spec section 5): nothing of it committed. Before, it was
/// answered in plain words, which the sidecar says as ABORTED, with no
/// retry-after (review-c5n5 F1).
#[tokio::test(flavor = "multi_thread")]
async fn a_fenced_writers_command_is_refused_unavailable_to_be_asked_again() {
    let (url, writer) = blocking(isolated);
    let (bus, writer) = wired_with(writer);
    assert_eq!(blocking(|| writer.adopt()).unwrap(), 1);
    open(&bus, &unique("ACC"), &unique("INS")).await;
    let refused_with = |refused: &str, words: &str| {
        assert!(refused.contains(words), "{refused}");
        // The bus's error says the topic first, then the handler's detail.
        let refused = &refused[refused.find("[refusal-status").unwrap_or(0)..];
        assert_eq!(
            meridian_bus::read_refusal_status(refused).map(|(status, _, _)| status),
            Some(meridian_bus::RefusalStatus::Unavailable),
            "{refused}"
        );
        assert_eq!(
            meridian_bus::refusal_retry_after(refused),
            Some(1),
            "{refused}"
        );
    };
    // Its epoch passed: another writer took its partitions over (its
    // client made and dropped off the runtime's threads).
    let took = blocking(|| {
        PostgresStore::connect(&url, 1)
            .unwrap()
            .take_over(&meridian_changes::TookOver::unnamed("a test"))
    });
    assert_eq!(took.unwrap(), 2);
    let refused = send(
        &bus,
        RECORD_OPENING_BALANCE,
        &opening(&unique("ACC"), &unique("INS")),
        Some(PERSON),
    )
    .await
    .unwrap_err();
    refused_with(&refused, meridian_changes::FENCED);
    // Told to stop: it commits nothing more, whatever the epoch.
    writer.stop_committing();
    let refused = send(
        &bus,
        RECORD_OPENING_BALANCE,
        &opening(&unique("ACC"), &unique("INS")),
        Some(PERSON),
    )
    .await
    .unwrap_err();
    refused_with(&refused, meridian_changes::STOPPED);
}

// ── The hot layer (step 3a) ──

/// Each account's book at its head, held by the writer, is the book the
/// record builds from its snapshot and tail, under any bound.
#[tokio::test(flavor = "multi_thread")]
async fn a_book_held_at_its_head_is_the_book_the_record_builds() {
    let record = blocking(small);
    for bound in [meridian_bor::writer::DEFAULT_BOOKS_BYTES, 3_000] {
        let bus = Arc::new(Bus::single(
            "operations-test-1",
            Arc::new(MemoryBackend::new()),
            Arc::new(meridian_clock::SystemClock),
        ));
        let writer = Arc::new(meridian_bor::writer::Writer::bounded(
            record.clone(),
            bus.clone(),
            bound,
        ));
        serve(bus.clone(), writer.clone(), bus.clock());
        // The instrument store, stood in, as wired_with does.
        bus.serve(RESOLVE_INSTRUMENT, |envelope| {
            let asked = ResolveInstrumentRequest::decode(&envelope.payload[..]).unwrap();
            Ok((
                "meridian.v1.ResolveInstrumentReply".into(),
                ResolveInstrumentReply {
                    found: true,
                    instrument: Some(InstrumentRecord {
                        instrument_id: asked.instrument_id,
                        asset_class: AssetClass::Equity as i32,
                        currency: "USD".into(),
                        version: 1,
                        ..Default::default()
                    }),
                }
                .encode_to_vec(),
            ))
        });
        let accounts: Vec<String> = (0..4).map(|_| unique("ACC")).collect();
        for account in &accounts {
            // Asked before it holds anything, then after each commit.
            let before = blocking(|| writer.book(account)).unwrap();
            assert_eq!(before, blocking(|| record.book(account)).unwrap());
            open(&bus, account, &unique("INS")).await;
        }
        for account in &accounts {
            let held = blocking(|| writer.book(account)).unwrap();
            let built = blocking(|| record.book(account)).unwrap();
            assert_eq!(held, built, "bound {bound}");
        }
        let (_, bytes) = writer.held();
        assert!(bound == meridian_bor::writer::DEFAULT_BOOKS_BYTES || bytes <= bound);
    }
}

/// The writer adopts its epoch before it takes a connection: a store of one
/// connection -- `meridian-bor rebuild`'s -- rebuilds, never waiting on
/// itself for a second (found by e2e-book, step 3a).
#[test]
fn a_store_of_one_connection_adopts_its_epoch_and_rebuilds() {
    let migrated = PostgresStore::connect(&url(), 1).unwrap();
    migrated.migrate(&meridian_clock::SystemClock).unwrap();
    drop(migrated);
    let alone = PostgresStore::connect(&url(), 1).unwrap();
    let _held = REBUILDS.blocking_lock();
    alone.rebuild().unwrap();
    assert!(alone.epoch() >= 1);
}

// ── Integrity at rest (step 3b) ─────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn the_journal_is_a_chain_its_scrub_names_a_changed_entry_and_a_replay_refuses_it() {
    let (url, store) = blocking(isolated);
    let (bus, store) = wired_with(store);
    let accounts: Vec<String> = (0..3).map(|n| unique(&format!("CHAIN{n}"))).collect();
    for account in &accounts {
        open(&bus, account, "INS-AAPL").await;
    }
    // Every entry chained to the one before it in its partition, the head
    // holding the chain's end, and the change carrying the entry's digest.
    let changes = blocking(|| followed(&store, "P0", 0));
    assert_eq!(changes.len(), 3);
    let mut chain = meridian_changes::integrity::Chain::default();
    for change in &changes {
        assert!(
            !change.digest.is_empty(),
            "entry {} has no digest",
            change.sequence
        );
        chain
            .follow(change)
            .expect("each entry follows the one before it");
    }
    let clean = blocking(|| store.scrub()).expect("scrubs");
    assert_eq!(clean.checked, 3);
    assert!(clean.findings.is_empty(), "{:?}", clean.findings);

    // An entry altered beneath the book (its append-only rule set aside,
    // as only someone with the database's own rights could).
    let altered = changes[1].sequence as i64;
    blocking(|| {
        let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
        client
            .batch_execute(&format!(
                "ALTER TABLE book_entry DISABLE TRIGGER book_entry_append_only;
                 UPDATE book_entry SET message_id = message_id || '-altered'
                  WHERE last_sequence = {altered};
                 ALTER TABLE book_entry ENABLE TRIGGER book_entry_append_only;"
            ))
            .unwrap();
    });
    let found = blocking(|| store.scrub()).expect("scrubs");
    assert_eq!(found.findings.len(), 1, "{:?}", found.findings);
    assert_eq!(found.findings[0].partition, "P0");
    assert_eq!(found.findings[0].sequence as i64, altered);
    blocking(|| store.record_scrub(&found, "test", 1, 2)).expect("kept");
    blocking(|| store.record_scrub(&found, "test", 3, 4)).expect("kept again, once");
    let kept: i64 = blocking(|| {
        postgres::Client::connect(&url, postgres::NoTls)
            .unwrap()
            .query_one("SELECT count(*) FROM book_integrity_finding", &[])
            .unwrap()
            .get(0)
    });
    assert_eq!(kept, 1, "a finding is kept once");
    // The altered entry's account no longer replays: loudly, by sequence.
    let replayed = blocking(|| store.journal(&accounts[1]));
    let refused = replayed.expect_err("an altered entry is not replayed");
    assert!(
        refused
            .to_string()
            .contains(&format!("entry {altered} of partition P0")),
        "{refused}"
    );
}

#[test]
fn an_entry_kept_before_digests_is_chained_by_migrate_even_one_the_book_cannot_read() {
    let (url, store) = isolated();
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    // As a restore of an older book leaves it: no digest, and here a body
    // that does not decode.
    client
        .execute(
            "INSERT INTO book_entry (entry_id, account_id, partition, first_sequence, last_sequence,
                                     kind, effective_date, committed_at_ns, meta, cause, body, reply)
             VALUES ('ENT-OLD', 'ACC-OLD', 'P0', 900001, 900001, 'opening-balance', '2026-01-01',
                     1, ''::bytea, ''::bytea, ''::bytea, ''::bytea)",
            &[],
        )
        .unwrap();
    store
        .migrate(&meridian_clock::SystemClock)
        .expect("migrating chains it, whatever it holds");
    let chained: bool = client
        .query_one(
            "SELECT digest IS NOT NULL FROM book_entry WHERE entry_id = 'ENT-OLD'",
            &[],
        )
        .unwrap()
        .get(0);
    assert!(chained);
    let scrubbed = store.scrub().expect("scrubs");
    assert!(scrubbed.findings.is_empty(), "{:?}", scrubbed.findings);
}

/// Stage M (the store memory audit of 2026-11-09, item 7): a keyed page --
/// locates, trading instructions, bookings, alerts -- reads its own rows
/// from the database, after its cursor and in its key's order, never every
/// row decoded and sorted for one page. 3,000 instructions, each with a long
/// reason, read ten at a time: every one once, in order, and a page's heap
/// a small part of the whole.
#[tokio::test(flavor = "multi_thread")]
async fn a_keyed_page_reads_its_own_rows_never_every_row() {
    use meridian_bor::order_service::*;
    let (bus, store) = wired();
    let tag = unique("plugin");
    let reason = "a pause set for the memory bound of a page of instructions ".repeat(6);
    for n in 0..3_000 {
        let set = SetTradingInstructionRequest {
            target: Some(TradingInstructionTarget {
                // Sorted after every other test's targets, which read
                // their first page and look for their own on it.
                instance_id: format!("zz-{tag}-{n:05}"),
                ..Default::default()
            }),
            value: TradingInstructionValue::Pause as i32,
            reason: reason.clone(),
            setter_level: SetterLevel::PluginAdmin as i32,
        };
        send_order(&bus, SET_TRADING_INSTRUCTION, &set, Some(PERSON), None)
            .await
            .unwrap();
    }
    let page = |cursor: &str, limit: usize| {
        let cursor = cursor.to_string();
        let store = Arc::clone(&store);
        blocking(move || {
            store
                .instructions(&meridian_bor::store::InstructionsRead {
                    since: None,
                    limit,
                    cursor,
                })
                .unwrap()
        })
    };
    // Warm the pool's connections, then measured.
    page("", 10);
    let first = tokio::task::block_in_place(|| {
        counted::take();
        let first = store
            .instructions(&meridian_bor::store::InstructionsRead {
                since: None,
                limit: 10,
                cursor: String::new(),
            })
            .unwrap();
        (first, counted::take().1)
    });
    let (first, peak) = first;
    assert_eq!(first.records.len(), 10);
    let whole_mib = 3_000.0 * reason.len() as f64 / 1048576.0;
    println!("KEYED heap_peak_mib={peak:.2} reasons_alone_mib={whole_mib:.2}");
    assert!(
        peak * 4.0 < whole_mib,
        "a page's heap peaked at {peak:.2} MiB; the reasons alone are {whole_mib:.2} MiB"
    );
    // Every instruction of the run, once, in order, ten at a time.
    let mut seen = Vec::new();
    let mut cursor = String::new();
    loop {
        let read = page(&cursor, 10);
        for held in &read.records {
            let named = held
                .target
                .as_ref()
                .map(|t| t.instance_id.clone())
                .unwrap_or_default();
            if named.starts_with(&format!("zz-{tag}")) {
                seen.push(named);
            }
        }
        if read.next_cursor.is_empty() {
            break;
        }
        cursor = read.next_cursor;
    }
    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(seen.len(), 3_000, "every one once");
    assert_eq!(seen, sorted, "in the key's order");
}

mod counted {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    /// Counted on the thread that allocates, so what a read holds is told
    /// apart from what the book's own background work does meanwhile.
    pub struct Counting;

    std::thread_local! {
        static LIVE: Cell<isize> = const { Cell::new(0) };
        static PEAK: Cell<isize> = const { Cell::new(0) };
    }

    fn by(change: isize) {
        let _ = LIVE.try_with(|live| {
            live.set(live.get() + change);
            let _ = PEAK.try_with(|peak| peak.set(peak.get().max(live.get())));
        });
    }

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let at = unsafe { System.alloc(layout) };
            if !at.is_null() {
                by(layout.size() as isize);
            }
            at
        }
        unsafe fn dealloc(&self, at: *mut u8, layout: Layout) {
            unsafe { System.dealloc(at, layout) };
            by(-(layout.size() as isize));
        }
    }

    /// This thread's heap peak since the last take, above where it stood
    /// then, in MiB; and where it stands now.
    pub fn take() -> (f64, f64) {
        let live = LIVE.with(Cell::get);
        let peak = PEAK.with(|peak| peak.replace(live));
        (
            live as f64 / 1048576.0,
            (peak - live).max(0) as f64 / 1048576.0,
        )
    }
}

#[global_allocator]
static COUNTING: counted::Counting = counted::Counting;

/// A new database timeline raises the epoch (contract v30, stage C; the
/// resilience spec's section 3): the identity a writer first sees is kept;
/// another raises every partition's epoch, recorded with that cause.
#[test]
fn a_new_database_timeline_raises_the_books_epoch() {
    use meridian_changes::timeline::Timeline;
    let (url, store) = isolated();
    let epoch = store.adopt().unwrap();
    assert_eq!(store.follow_timeline().unwrap(), Timeline::First);
    assert_eq!(store.follow_timeline().unwrap(), Timeline::Same);
    let mut admin = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    admin
        .execute(
            "UPDATE book_partition_head SET database_identity = 'another/00000001'
              WHERE partition = 'control'",
            &[],
        )
        .unwrap();
    assert_eq!(store.follow_timeline().unwrap(), Timeline::Moved(epoch + 1));
    assert_eq!(store.epoch(), epoch + 1);
    let why: String = admin
        .query_one(
            "SELECT why FROM book_writer_took_over WHERE epoch = $1",
            &[&((epoch + 1) as i64)],
        )
        .unwrap()
        .get(0);
    assert_eq!(why, meridian_changes::timeline::DATABASE_TIMELINE);
    assert_eq!(store.head_epoch().unwrap(), epoch + 1);
}

/// PR CI 44283011761 (the lake's writer stepped down from its own raise):
/// the store's own raise on a new database timeline is its epoch from the
/// moment the raise can be read, and a raise whose commit fails leaves the
/// epoch it had. Held at the raise's commit by a deferred trigger waiting
/// on an advisory lock this test holds.
#[test]
fn a_raise_on_a_new_timeline_is_the_books_epoch_before_its_commit_can_be_read() {
    use meridian_changes::timeline::Timeline;
    let (url, store) = isolated();
    let epoch = store.adopt().unwrap();
    assert_eq!(store.follow_timeline().unwrap(), Timeline::First);
    let mut admin = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let hold: i64 = 0x6335_7233;
    admin
        .batch_execute(&format!(
            "CREATE TABLE raise_fails (fails boolean NOT NULL);
             INSERT INTO raise_fails VALUES (false);
             CREATE FUNCTION held_raise() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
               PERFORM pg_advisory_xact_lock({hold});
               IF (SELECT fails FROM raise_fails) THEN
                 RAISE EXCEPTION 'the raise''s commit fails';
               END IF;
               RETURN NULL;
             END $$;
             CREATE CONSTRAINT TRIGGER held_raise AFTER UPDATE ON book_partition_head
               DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
               WHEN (NEW.epoch <> OLD.epoch) EXECUTE FUNCTION held_raise();"
        ))
        .unwrap();
    let moved = |admin: &mut postgres::Client, to: &str| {
        admin
            .execute(
                "UPDATE book_partition_head SET database_identity = $1 WHERE partition = 'control'",
                &[&to],
            )
            .unwrap();
    };
    moved(&mut admin, "another/00000002");
    admin
        .execute("SELECT pg_advisory_lock($1)", &[&hold])
        .unwrap();
    let following = Arc::clone(&store);
    let raise = std::thread::spawn(move || following.follow_timeline());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let waiting: i64 = admin
            .query_one(
                "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND NOT granted
                   AND classid = 0 AND objid = $1::bigint::oid AND objsubid = 1",
                &[&hold],
            )
            .unwrap()
            .get(0);
        if waiting > 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the raise never reached its commit"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(
        store.epoch(),
        epoch + 1,
        "the raise can be read the moment it commits, and the writer's epoch was not yet its own"
    );
    admin
        .execute("SELECT pg_advisory_unlock($1)", &[&hold])
        .unwrap();
    assert_eq!(raise.join().unwrap().unwrap(), Timeline::Moved(epoch + 1));
    assert_eq!(store.head_epoch().unwrap(), epoch + 1);
    // Another new timeline, its raise's commit failing: the epoch it had.
    admin
        .execute("UPDATE raise_fails SET fails = true", &[])
        .unwrap();
    moved(&mut admin, "yet another/00000003");
    assert!(
        store.follow_timeline().is_err(),
        "the raise's commit failed"
    );
    assert_eq!(store.epoch(), epoch + 1);
    assert_eq!(store.head_epoch().unwrap(), epoch + 1);
}

/// A record read on a read replica is stamped from its own snapshot
/// (contract v30, stage C2; the resilience spec's section 5): asked, the
/// book's reads of its past each answer from one repeatable-read snapshot
/// and say its partitions' head rows; not asked, nothing. A reader member
/// answering the record's past from the replica says those heads as the
/// read's position -- never its own applied position -- and the next read
/// says its own again.
#[tokio::test(flavor = "multi_thread")]
async fn a_record_read_says_the_heads_of_the_snapshot_it_answered_from() {
    use meridian_bor::postgres::{snapshot_heads, stamp_from_snapshot};
    let (bus, store) = wired();
    let account = unique("ACC-STAMP");
    open(&bus, &account, &unique("INS-STAMP")).await;
    blocking(|| {
        stamp_from_snapshot();
        let entries = store.journal(&account).unwrap();
        let said = snapshot_heads().expect("the snapshot's heads");
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        let head = said[&entry.partition];
        assert!(
            head >= entry.last_sequence && head <= store.heads().unwrap()[&entry.partition],
            "the snapshot's head ({head}) holds the entry it answered and no more than the record"
        );
        // Not asked: nothing said, and nothing kept from before.
        store.journal(&account).unwrap();
        assert!(snapshot_heads().is_none());

        // A reader member whose replica is this store, applied to nothing.
        let reader = meridian_bor::reader::ReaderStore::new(
            store.clone(),
            Some(store.clone() as Arc<dyn Store>),
            1 << 20,
            Arc::new(meridian_bor::store::Mark::new),
        );
        meridian_bus::read_after::begin_read();
        let past = Store::journal(&reader, &account).unwrap();
        assert_eq!(past.len(), 1);
        let stamped = Store::heads(&reader).unwrap();
        assert!(
            stamped.get(&entry.partition).copied().unwrap_or(0) >= entry.last_sequence,
            "the replica's snapshot, not the member's applied position: {stamped:?}"
        );
        meridian_bus::read_after::begin_read();
        assert!(
            Store::heads(&reader).unwrap().is_empty(),
            "the next read says the member's own position again"
        );
    });
}

fn a_break(account: &str) -> RecordBreakRequest {
    RecordBreakRequest {
        account_id: account.into(),
        subject: Some(record_break_request::Subject::Position(PositionKey {
            instrument_id: "INS-AAPL".into(),
            side: HoldingSide::Long as i32,
        })),
        category: BreakCategory::TradeDateQuantity as i32,
        differences: vec![BreakDifference {
            field: "trade_date_quantity".into(),
            book: None,
            street: None,
        }],
        business_date: "2026-09-09".into(),
        ..Default::default()
    }
}

/// A write, then a record read naming its watermark, against a read replica
/// held behind (contract v30, stage C2; the resilience spec's section 5): the
/// reader never answers with less than the watermark. Its replica's snapshot
/// behind it, the primary answers, the write in what it said, and the
/// read's position is the primary's; once the replica holds the write, the
/// replica answers, its position its own snapshot's, at or past the
/// watermark.
#[tokio::test(flavor = "multi_thread")]
async fn a_record_read_naming_its_watermark_is_never_answered_from_a_replica_behind_it() {
    use meridian_bus::read_after::{answered_by_primary, begin_read, Reads};
    use meridian_pb::v1::{PartitionSequence, Watermark};
    let (_primary_schema, primary) = blocking(isolated);
    let (_replica_schema, replica) = blocking(isolated);
    let (bus, primary) = wired_with(primary);
    let (replica_bus, replica) = wired_with(replica);
    let account = unique("ACC-LAG");
    // Both hold the account's opening; the replica is held there.
    open(&bus, &account, "INS-AAPL").await;
    open(&replica_bus, &account, "INS-AAPL").await;
    // The write the read names: on the primary alone.
    send(&bus, RECORD_BREAK, &a_break(&account), None)
        .await
        .unwrap();
    let written = blocking(|| primary.journal(&account)).unwrap();
    assert_eq!(written.len(), 2);
    let last = written.last().unwrap();
    let watermark = Watermark {
        partitions: vec![PartitionSequence {
            partition: last.partition.clone(),
            sequence: last.last_sequence,
        }],
    };
    let reader = meridian_bor::reader::ReaderStore::new(
        primary.clone(),
        Some(replica.clone() as Arc<dyn Store>),
        1 << 20,
        Arc::new(meridian_bor::store::Mark::new),
    );
    let reads = Reads::default();
    let read = |reads: &Reads| {
        begin_read();
        reads.answer(
            "platform.bor.query.list-lifecycle-events",
            &watermark,
            || Store::journal(&reader, &account).map_err(|failed| failed.to_string()),
        )
    };
    blocking(|| {
        let said = read(&reads).unwrap();
        assert_eq!(said.len(), 2, "the write named is in what the read said");
        assert!(
            answered_by_primary(),
            "the replica behind it: the primary answered"
        );
        let at = Store::heads(&reader).unwrap();
        assert!(at[&last.partition] >= last.last_sequence, "{at:?}");
    });
    // The replica takes the write: it answers, stamped from its snapshot.
    send(&replica_bus, RECORD_BREAK, &a_break(&account), None)
        .await
        .unwrap();
    blocking(|| {
        let held = replica.journal(&account).unwrap();
        assert_eq!(held.last().unwrap().last_sequence, last.last_sequence);
        let said = read(&reads).unwrap();
        assert_eq!(said.len(), 2);
        assert!(
            !answered_by_primary(),
            "the replica, at the watermark, answered"
        );
        let at = Store::heads(&reader).unwrap();
        assert!(at[&last.partition] >= last.last_sequence, "{at:?}");
    });
}

/// Group commit (step 3d; the resilience spec's sections 6.5 and 15): the
/// commands arriving while a partition's transaction is in flight join the
/// next, each decided and refused alone, the epoch checked once and the
/// batch's commits told in their order; and a batch an unexpected database
/// error broke is tried again one command at a time, so only the command it
/// was is refused.
///
/// Over a schema of its own: a broken batch's commands commit one
/// transaction each, and another test's store writing the shared database's
/// P0 between two of them took a number of the page told (the run of
/// 2026-11-14, `[126, 128, ...]`). A partition has one writer. And nothing
/// here is slept for: the first batch is held in its decision until each of
/// the others waits in the lane, in its order, so which command of the two
/// took the entry ID first is never the scheduler's to say.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn group_commit_refuses_each_command_alone_and_a_broken_batch_is_tried_alone() {
    use meridian_bor::decide::{set_attribute, Context};
    use meridian_bor::store::{Acted, Decided, StoreError};
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;
    /// Waited for, never slept for; its bound only ends a run that failed.
    fn until(what: &str, met: impl Fn() -> bool) {
        let bound = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !met() {
            assert!(std::time::Instant::now() < bound, "never: {what}");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    let (_schema, store) = blocking(isolated);
    // Each batch's commits as the store tells them: how many, and their
    // sequences, which run on from one to the next.
    let batches: Arc<Mutex<Vec<Vec<u64>>>> = Arc::default();
    {
        let batches = Arc::clone(&batches);
        assert!(store.on_commit(Box::new(move |_, decided| {
            batches.lock().unwrap().push(
                decided
                    .iter()
                    .map(|decided| decided.entry.first_sequence)
                    .collect(),
            );
        })));
    }
    let run = unique("GC");
    let shared_entry = format!("ENT-{run}-shared");
    const COMMANDS: usize = 24;
    const REFUSED: usize = 7;
    const BROKEN: usize = 11;
    const SAME_ENTRY_AS: usize = 3;
    // The first command's transaction: in flight, then let go.
    let in_flight = Arc::new(AtomicBool::new(false));
    let let_go = Arc::new(AtomicBool::new(false));
    let act = |n: usize| {
        let store = Arc::clone(&store);
        let (in_flight, let_go) = (Arc::clone(&in_flight), Arc::clone(&let_go));
        let (account, shared) = (format!("ACC-{run}-{n}"), shared_entry.clone());
        std::thread::spawn(move || {
            let request = SetAccountAttributeRequest {
                account_id: account.clone(),
                attribute: Some(
                    set_account_attribute_request::Attribute::WaitsForCompliance(false),
                ),
                reason: "group commit's test".into(),
            };
            let ctx = Context {
                acting_for: PERSON.into(),
                ..Default::default()
            };
            Store::act(
                store.as_ref(),
                &account,
                "",
                &format!("{account}-key"),
                &request.encode_to_vec(),
                &mut |book, head| {
                    if n == 0 {
                        // The first batch held in flight until the others
                        // wait behind it.
                        in_flight.store(true, Ordering::SeqCst);
                        until("the first batch is let go", || {
                            let_go.load(Ordering::SeqCst)
                        });
                    }
                    if n == REFUSED {
                        return Err(StoreError::Invalid(format!("{n} is refused alone")));
                    }
                    let mut made = set_attribute(book, head, &ctx, &request)?;
                    if n == SAME_ENTRY_AS || n == BROKEN {
                        // An entry ID taken twice: the second's insert breaks
                        // its batch's transaction.
                        made.entry.entry_id = shared.clone();
                    }
                    Ok(Decided {
                        entry: made.entry,
                        book: made.book,
                        changes: made.changes,
                        reply: n.to_string().into_bytes(),
                    })
                },
            )
        })
    };
    let first = act(0);
    until("the first command's transaction is in flight", || {
        in_flight.load(Ordering::SeqCst)
    });
    // Each of the others waiting in the lane before the next is asked, so
    // they wait in their order.
    let others: Vec<_> = (1..COMMANDS)
        .map(|n| {
            let other = act(n);
            until("a command waits behind the first", || {
                store.waiting("P0") == n
            });
            other
        })
        .collect();
    let_go.store(true, Ordering::SeqCst);
    let mut outcomes = vec![first.join().unwrap()];
    outcomes.extend(others.into_iter().map(|other| other.join().unwrap()));
    let mut committed = 0;
    for (n, outcome) in outcomes.into_iter().enumerate() {
        match (n, outcome) {
            (REFUSED, Err(StoreError::Invalid(words))) => assert!(words.contains("alone")),
            (REFUSED, other) => panic!("{n} was not refused alone: {:?}", other.is_ok()),
            (BROKEN, Err(StoreError::Unavailable(words))) => {
                assert!(words.contains("duplicate key"), "{words}")
            }
            (BROKEN, other) => panic!("{n} committed an entry ID taken: {:?}", other.is_ok()),
            (_, Ok(Acted::Committed(decided))) => {
                assert_eq!(decided.reply, n.to_string().into_bytes());
                committed += 1;
            }
            (_, Err(failed)) => panic!("{n} was refused for another's sake: {failed}"),
            (_, Ok(Acted::Duplicate(_))) => panic!("{n} answered as a duplicate"),
        }
    }
    assert_eq!(committed, COMMANDS - 2);
    // Each command's account holds its one entry, the refused none.
    blocking(|| {
        for n in 0..COMMANDS {
            let held = store.journal(&format!("ACC-{run}-{n}")).unwrap().len();
            let expected = usize::from(n != REFUSED && n != BROKEN);
            assert_eq!(held, expected, "account {n}");
        }
    });
    let batches = batches.lock().unwrap().clone();
    let told: usize = batches.iter().map(Vec::len).sum();
    assert_eq!(told, committed, "every commit told once: {batches:?}");
    assert_eq!(
        batches.iter().map(Vec::len).collect::<Vec<_>>(),
        [1, COMMANDS - 3],
        "the first alone, then those waiting behind it told together: {batches:?}"
    );
    for batch in &batches {
        assert!(
            batch.windows(2).all(|pair| pair[1] == pair[0] + 1),
            "a batch's commits told in their order: {batch:?}"
        );
    }
    // Its connections closed off the runtime's threads.
    blocking(move || drop(store));
}

/// An act reads its account's partition under that partition's lock (step
/// 3d; the resilience spec's section 6.3): an account found in another
/// partition than the one its act was queued in -- moved between the
/// guess and the lock -- is acted on in its partition now, numbered there.
#[tokio::test(flavor = "multi_thread")]
async fn an_act_reads_its_partition_under_the_lock_and_follows_the_account_moved() {
    let (url, store) = blocking(isolated);
    let (bus, store) = wired_with(store);
    let mut admin = blocking(|| postgres::Client::connect(&url, postgres::NoTls).unwrap());
    // A second partition beside P0, as `bor.partitions` makes one (stage B).
    blocking(|| {
        admin
            .batch_execute(
                "INSERT INTO book_partition_head (partition, sequence, epoch, origin)
                 SELECT 'P1', 0, epoch, origin FROM book_partition_head WHERE partition = 'P0'",
            )
            .unwrap()
    });
    let account = unique("MOVED");
    let set = |reason: &str| SetAccountAttributeRequest {
        account_id: account.clone(),
        attribute: Some(set_account_attribute_request::Attribute::WaitsForCompliance(false)),
        reason: reason.into(),
    };
    send(&bus, SET_ACCOUNT_ATTRIBUTE, &set("first"), Some(PERSON))
        .await
        .unwrap();
    // Moved beneath the store, which last found it in P0.
    blocking(|| {
        admin
            .execute(
                "UPDATE book_account SET partition = 'P1' WHERE account_id = $1",
                &[&account],
            )
            .unwrap()
    });
    send(&bus, SET_ACCOUNT_ATTRIBUTE, &set("second"), Some(PERSON))
        .await
        .unwrap();
    let journal = blocking(|| store.journal(&account)).unwrap();
    let partitions: Vec<&str> = journal.iter().map(|e| e.partition.as_str()).collect();
    assert_eq!(partitions, ["P0", "P1"], "the second act numbered in P1");
    let heads = blocking(|| store.heads()).unwrap();
    assert_eq!(heads.get("P1"), Some(&1));
    blocking(move || {
        drop(admin);
        drop(store);
    });
}

/// The broker, stood in for two writer groups' members (step 3d): each
/// bus's publishes and subscriptions the in-process backend's, and a
/// hand-off addressed to a writer group answered by that group's member
/// alone, as each group's own queue group on the internal topic gives it.
struct GroupsBroker {
    inner: MemoryBackend,
    groups: std::sync::Mutex<std::collections::BTreeMap<(String, u32), meridian_bus::Handler>>,
    handed: AtomicU64,
}

impl meridian_bus::Backend for GroupsBroker {
    fn publish(&self, topic: &str, envelope: Envelope) -> Result<u64, meridian_bus::BusError> {
        self.inner.publish(topic, envelope)
    }

    fn subscribe(&self, pattern: &str) -> meridian_bus::Subscription {
        self.inner.subscribe(pattern)
    }

    fn subscribe_shared(&self, pattern: &str, group: &str) -> meridian_bus::Subscription {
        self.inner.subscribe_shared(pattern, group)
    }

    fn dropped(&self) -> u64 {
        self.inner.dropped()
    }

    fn serve_group(&self, topic: &str, _: &str, group: u32, handler: meridian_bus::Handler) {
        self.groups
            .lock()
            .unwrap()
            .insert((topic.to_string(), group), handler);
    }

    fn request_to_group<'a>(
        &'a self,
        topic: &'a str,
        envelope: Envelope,
        timeout: std::time::Duration,
        group: u32,
    ) -> meridian_bus::Answer<'a> {
        self.handed.fetch_add(1, Ordering::SeqCst);
        let handler = self
            .groups
            .lock()
            .unwrap()
            .get(&(topic.to_string(), group))
            .cloned();
        Box::pin(async move {
            let Some(handler) = handler else {
                return Err(meridian_bus::BusError::NoHandler(topic.to_string()));
            };
            // Its caller's deadline known to the handler, as the broker's
            // path makes it.
            let deadline = std::time::Instant::now() + timeout;
            match tokio::task::spawn_blocking(move || {
                meridian_bus::read_after::within_deadline(Some(deadline), || handler(envelope))
            })
            .await
            .unwrap()
            {
                Ok((payload_type, payload)) => Ok(Envelope {
                    meta: None,
                    payload_type,
                    payload,
                }),
                Err(detail) => Err(meridian_bus::BusError::HandlerFailed {
                    topic: topic.to_string(),
                    detail,
                }),
            }
        })
    }
}

/// Two writer groups of one book (step 3d; the resilience spec's sections
/// 6.3 and 6.7): a command sent to the member of the group that does not
/// own its account's partition is handed to the owner's writer, which
/// commits it; and an account found under the partition's lock in a
/// partition another group owns -- moved beneath the act -- is handed to
/// that group and numbered there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writer_groups_hand_a_command_to_its_partitions_owner_and_follow_a_move() {
    use meridian_bor::writer::{is_command, Writer, INTERNAL_COMMAND};
    use meridian_changes::groups::{owner, Groups};
    // At two groups: control and P3 group 0's, P0 group 1's.
    assert_eq!((owner("P0", 2), owner("P3", 2)), (1, 0));
    let (url, migrated) = blocking(isolated);
    let mut admin = blocking(|| postgres::Client::connect(&url, postgres::NoTls).unwrap());
    blocking(|| {
        admin
            .batch_execute(
                "INSERT INTO book_partition_head (partition, sequence, epoch, origin)
                 SELECT 'P3', 0, epoch, origin FROM book_partition_head WHERE partition = 'P0'",
            )
            .unwrap()
    });
    let broker = Arc::new(GroupsBroker {
        inner: MemoryBackend::new(),
        groups: Default::default(),
        handed: AtomicU64::new(0),
    });
    let member = |group: u32| {
        let store = blocking(|| {
            Arc::new(
                PostgresStore::connect(&url, 4)
                    .unwrap()
                    .with_groups(Groups::new(group, 2).unwrap()),
            )
        });
        let bus = Arc::new(Bus::single(
            "bor-groups-test",
            Arc::clone(&broker) as Arc<dyn meridian_bus::Backend>,
            Arc::new(meridian_clock::SystemClock),
        ));
        bus.hand_writes_on(INTERNAL_COMMAND, group, is_command);
        let writer: Arc<dyn Store> = Arc::new(Writer::new(
            Arc::clone(&store) as Arc<dyn Store>,
            Arc::clone(&bus),
        ));
        serve(bus.clone(), writer, bus.clock());
        (bus, store)
    };
    let (zero, zero_store) = member(0);
    let (one, one_store) = member(1);
    let account = unique("GROUPED");
    let set = |reason: &str| SetAccountAttributeRequest {
        account_id: account.clone(),
        attribute: Some(set_account_attribute_request::Attribute::WaitsForCompliance(false)),
        reason: reason.into(),
    };
    // Sent to group 0's member: the account starts in P0, group 1's.
    send(&zero, SET_ACCOUNT_ATTRIBUTE, &set("first"), Some(PERSON))
        .await
        .unwrap();
    assert_eq!(broker.handed.load(Ordering::SeqCst), 1, "handed once");
    let journal = blocking(|| one_store.journal(&account)).unwrap();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].partition, "P0");
    assert!(
        journal[0].cause.committed_at_ns > 0,
        "committed by group 1's writer"
    );
    // Its epochs its own: group 0's raised past P0's as it adopted.
    let (zero_epoch, one_epoch) = (
        Store::epoch(zero_store.as_ref()),
        Store::epoch(one_store.as_ref()),
    );
    assert!(zero_epoch > 0 && one_epoch > 0 && zero_epoch != one_epoch);
    // Moved to P3 beneath group 1's member, which last found it in P0.
    blocking(|| {
        admin
            .execute(
                "UPDATE book_account SET partition = 'P3' WHERE account_id = $1",
                &[&account],
            )
            .unwrap()
    });
    send(&one, SET_ACCOUNT_ATTRIBUTE, &set("second"), Some(PERSON))
        .await
        .unwrap();
    assert_eq!(broker.handed.load(Ordering::SeqCst), 2, "handed to group 0");
    let journal = blocking(|| zero_store.journal(&account)).unwrap();
    let partitions: Vec<&str> = journal.iter().map(|e| e.partition.as_str()).collect();
    assert_eq!(
        partitions,
        ["P0", "P3"],
        "the second numbered in P3, by group 0"
    );
    // A command handed to a group whose partition it is not -- a stale
    // route -- is refused, never handed on again.
    let refused = blocking(|| {
        let handed = Envelope {
            meta: Some(meridian_bus::MessageMeta {
                topic: SET_ACCOUNT_ATTRIBUTE.into(),
                acting_for_subject: PERSON.into(),
                message_id: unique("STALE"),
                ..Default::default()
            }),
            payload_type: "meridian.v1.SetAccountAttributeRequest".into(),
            payload: set("third").encode_to_vec(),
        };
        let handler = broker
            .groups
            .lock()
            .unwrap()
            .get(&(INTERNAL_COMMAND.to_string(), 1))
            .cloned()
            .unwrap();
        handler(handed)
    });
    let refused = refused.unwrap_err();
    assert!(refused.contains("does not own its partition"), "{refused}");
    blocking(move || {
        drop(admin);
        drop((zero_store, one_store, migrated));
    });
}

/// An account's move between two writer groups' partitions is opened by
/// the new partition's writer whichever member was asked, and a close left
/// without its open is opened by the control group's sweep there too (step
/// 3d; sections 6.7 and 6.8; CI 43662032539, tolerant: ACC-DRIVE-00's move
/// was asked of the member whose group did not write its partition, handed
/// to the one that did, closed there, and its open -- the other group's --
/// refused "handed to this writer group, which does not own its
/// partition". The account stayed closed and never opened, every order on
/// it refused "found in another partition 4 times", and the sweep, which
/// answers no request, had nothing to hand its open on).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m11_an_accounts_move_between_groups_is_opened_whichever_member_was_asked() {
    use meridian_bor::writer::{is_command, Writer, INTERNAL_COMMAND};
    use meridian_changes::groups::{owner, Groups};
    use meridian_domain::v1::{MoveAccountReply, MoveAccountRequest};
    // At two groups: control and P3 group 0's, P0 group 1's.
    assert_eq!((owner("P0", 2), owner("P3", 2)), (1, 0));
    let (url, migrated) = blocking(isolated);
    let mut admin = blocking(|| postgres::Client::connect(&url, postgres::NoTls).unwrap());
    blocking(|| {
        admin
            .batch_execute(
                "INSERT INTO book_partition_head (partition, sequence, epoch, origin)
                 SELECT 'P3', 0, epoch, origin FROM book_partition_head WHERE partition = 'P0'",
            )
            .unwrap()
    });
    let broker = Arc::new(GroupsBroker {
        inner: MemoryBackend::new(),
        groups: Default::default(),
        handed: AtomicU64::new(0),
    });
    let member = |group: u32| {
        let store = blocking(|| {
            Arc::new(
                PostgresStore::connect(&url, 4)
                    .unwrap()
                    .with_groups(Groups::new(group, 2).unwrap()),
            )
        });
        let bus = Arc::new(Bus::single(
            "bor-m11-test",
            Arc::clone(&broker) as Arc<dyn meridian_bus::Backend>,
            Arc::new(meridian_clock::SystemClock),
        ));
        bus.hand_writes_on(INTERNAL_COMMAND, group, is_command);
        let writer: Arc<dyn Store> = Arc::new(Writer::new(
            Arc::clone(&store) as Arc<dyn Store>,
            Arc::clone(&bus),
        ));
        serve(bus.clone(), writer.clone(), bus.clock());
        (bus, store, writer)
    };
    let (zero, zero_store, zero_writer) = member(0);
    let (one, one_store, _one_writer) = member(1);
    let account = unique("M11-MOVED");
    let set = |reason: &str| SetAccountAttributeRequest {
        account_id: account.clone(),
        attribute: Some(set_account_attribute_request::Attribute::WaitsForCompliance(false)),
        reason: reason.into(),
    };
    // The account begins in P0, group 1's.
    send(&one, SET_ACCOUNT_ATTRIBUTE, &set("first"), Some(PERSON))
        .await
        .unwrap();
    // Its move to P3, group 0's, asked of group 0's member: handed to group
    // 1 for the close, and on from there to group 0 for the open.
    let moved = send(
        &zero,
        meridian_bor::moves::MOVE_ACCOUNT,
        &MoveAccountRequest {
            account_id: account.clone(),
            target_partition: "P3".into(),
            reason: "group 1 is busy".into(),
        },
        Some(PERSON),
    )
    .await
    .expect("moved, asked of the member whose group does not write its partition");
    let moved = MoveAccountReply::decode(moved.as_slice())
        .unwrap()
        .moved
        .unwrap();
    assert_eq!(moved.to_partition, "P3");
    assert!(moved.opening_sequence > 0, "opened: {moved:?}");
    // Written where it is now, asked of either member.
    send(&one, SET_ACCOUNT_ATTRIBUTE, &set("second"), Some(PERSON))
        .await
        .expect("written after its move");
    // Home again, its member stopped between the close and the open.
    meridian_changes::moves::points::arm(
        "book.account.closed",
        &account,
        Arc::new(|_| Err("stopped after the close".to_string())),
    );
    let stopped = send(
        &zero,
        meridian_bor::moves::MOVE_ACCOUNT,
        &MoveAccountRequest {
            account_id: account.clone(),
            target_partition: "P0".into(),
            reason: "home again".into(),
        },
        Some(PERSON),
    )
    .await;
    meridian_changes::moves::points::disarm("book.account.closed", &account);
    assert!(stopped.is_err(), "stopped between its close and its open");
    // The control group's sweep: the open is group 1's, and asked of it.
    let opened = blocking(|| {
        meridian_bor::moves::finish_opens(
            &zero,
            zero_writer.as_ref(),
            std::slice::from_ref(&zero_store),
            &meridian_changes::moves::Settings::default(),
            meridian_clock::Clock::now_ns(&meridian_clock::SystemClock) + 1_000_000_000,
        )
    });
    assert_eq!(opened, 1, "the sweep had its open made by group 1's writer");
    send(&zero, SET_ACCOUNT_ATTRIBUTE, &set("third"), Some(PERSON))
        .await
        .expect("written once the sweep opened it");
    let journal = blocking(|| one_store.journal(&account)).unwrap();
    assert_eq!(
        journal.last().map(|entry| entry.partition.as_str()),
        Some("P0"),
        "numbered in P0 again"
    );
    blocking(move || {
        drop(admin);
        drop((zero_store, one_store, migrated));
    });
}

/// An account's orders read in the order they were recorded across its
/// move (step 3d; section 6.8, "an account's move is a continuation"; CI
/// 43662032539, ha: Elle's incompatible orders on ACC-DRIVE-00). A
/// partition numbers its own sequences, so an order recorded after the move
/// may carry a lower one than an order recorded before it: `sequence` alone
/// orders nothing across a move. Each order's `last_change` names its
/// partition and the account's previous change of an order, and that chain
/// crosses the move unbroken: the first order in the new partition names
/// the last one's sequence in the old. What the failover driver sorts by.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m11_an_accounts_orders_chain_in_order_across_its_move() {
    use meridian_bor::order_service::*;
    use meridian_domain::v1::MoveAccountRequest;
    let (_url, migrated) = blocking(isolated);
    blocking(|| migrated.make_partitions(4).unwrap());
    let (bus, store) = wired_with(Arc::clone(&migrated));
    meridian_domain::money::learn("USD", "INS-CASH-USD");
    let account = unique("ACC-CHAIN");
    send_order(
        &bus,
        SET_ACCOUNT_ATTRIBUTE,
        &SetAccountAttributeRequest {
            account_id: account.clone(),
            attribute: Some(set_account_attribute_request::Attribute::WaitsForCompliance(false)),
            reason: "no compliance plugin in this test".into(),
        },
        Some(PERSON),
        None,
    )
    .await
    .unwrap();
    let propose = || ProposeOrderRequest {
        account_id: account.clone(),
        instrument_id: "INS-AAPL".into(),
        sleeve: "core".into(),
        side: Side::Buy as i32,
        quantity: d("10"),
        order_type: OrderType::Limit as i32,
        limit_price: usd("227.50"),
        time_in_force: TimeInForce::GoodTillCancelled as i32,
        idempotency_key: unique("chain"),
        ..Default::default()
    };
    // Each order's ID, in the order they were recorded.
    let mut recorded: Vec<String> = Vec::new();
    for _ in 1..=2 {
        let made = send_order(&bus, PROPOSE_ORDER, &propose(), Some(PERSON), None)
            .await
            .unwrap();
        recorded.push(
            OrderReply::decode(&made[..])
                .unwrap()
                .order
                .unwrap()
                .order_id,
        );
    }
    let from = blocking(|| Store::partition_of(store.as_ref(), &account)).unwrap();
    let to = (0..4)
        .map(meridian_bor::store::partition_named)
        .find(|partition| *partition != from)
        .unwrap();
    send(
        &bus,
        meridian_bor::moves::MOVE_ACCOUNT,
        &MoveAccountRequest {
            account_id: account.clone(),
            target_partition: to.clone(),
            reason: "its partition is busy".into(),
        },
        Some(PERSON),
    )
    .await
    .unwrap();
    for _ in 3..=4 {
        let made = send_order(&bus, PROPOSE_ORDER, &propose(), Some(PERSON), None)
            .await
            .unwrap();
        recorded.push(
            OrderReply::decode(&made[..])
                .unwrap()
                .order
                .unwrap()
                .order_id,
        );
    }
    let orders = blocking(|| {
        store
            .orders(&meridian_bor::store::OrdersRead {
                scope: Scope::Within([account.clone()].into_iter().collect()),
                account_id: account.clone(),
                since: None,
                limit: 100,
                cursor: String::new(),
                execution_state: 0,
            })
            .unwrap()
    })
    .orders;
    // (its number, its partition, its sequence, the one before it).
    let mut refs: Vec<(usize, String, u64, u64)> = orders
        .iter()
        .map(|order| {
            let at = order.last_change.clone().unwrap();
            (
                recorded
                    .iter()
                    .position(|id| *id == order.order_id)
                    .unwrap(),
                at.partition,
                at.sequence,
                at.previous_sequence,
            )
        })
        .collect();
    refs.sort();
    assert_eq!(refs.len(), 4, "{refs:?}");
    assert_eq!(
        refs.iter().map(|r| r.1.as_str()).collect::<Vec<_>>(),
        [from.as_str(), from.as_str(), to.as_str(), to.as_str()],
        "{refs:?}"
    );
    assert!(
        refs[2].2 < refs[1].2,
        "the new partition numbers from its own head: sequence alone would read the third          order before the second ({refs:?})"
    );
    assert_eq!(refs[0].3, 0, "the first names none before it: {refs:?}");
    for (before, after) in refs.iter().zip(refs.iter().skip(1)) {
        assert_eq!(
            after.3, before.2,
            "each names the one recorded before it, across the move: {refs:?}"
        );
    }
    blocking(move || drop((store, migrated)));
}

/// A read naming an account is answered whole though the account moves to
/// another database while it is read (step 3d; sections 6.4 and 6.8; the
/// latent race part 10 named in Spread's named-account reads): the member
/// looked up the database holding the account, and the move opened it in
/// the other and let go of the first before the read was made there. The
/// read is made again where the account went, never answered empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn m11_a_named_read_follows_its_account_to_the_database_it_moved_to_meanwhile() {
    use meridian_changes::groups::Groups;
    use meridian_changes::moves::points;
    use meridian_domain::v1::MoveAccountRequest;
    let urls: Vec<Scratch> = blocking(|| {
        (0..2)
            .map(|_| {
                let (url, migrated) = isolated();
                migrated.make_partitions(4).unwrap();
                url
            })
            .collect()
    });
    let spread = |group: u32| {
        let groups = Groups::new(group, 2).unwrap();
        let stores = urls
            .iter()
            .map(|url| {
                Arc::new(
                    PostgresStore::connect(url, 4)
                        .unwrap()
                        .with_groups(groups)
                        .with_partitions(4),
                )
            })
            .collect();
        Arc::new(meridian_bor::spread::Spread::new(stores, groups))
    };
    // Each group's writer, and a third member that only reads.
    let (zero, one, reading) = blocking(|| (spread(0), spread(1), spread(0)));
    let groups = Groups::new(0, 2).unwrap();
    // An account of group 1's, in database 1; a partition of group 0's, in
    // database 0, to move it to.
    let account = (0..)
        .map(|n| unique(&format!("ACC-M11-READ-{n}")))
        .find(|account| groups.owner(&meridian_bor::store::placement_of(account, 4)) == 1)
        .unwrap();
    blocking(|| one.place_first(&account, &account)).unwrap();
    let to = (0..4)
        .map(meridian_bor::store::partition_named)
        .find(|partition| groups.owner(partition) == 0)
        .unwrap();
    let bus = |name: &str, store: Arc<meridian_bor::spread::Spread>| {
        let bus = Arc::new(Bus::single(
            name,
            Arc::new(MemoryBackend::new()),
            Arc::new(meridian_clock::SystemClock),
        ));
        serve(bus.clone(), store, bus.clock());
        resolving_equities(&bus);
        bus
    };
    let (bus_zero, bus_one) = (
        bus("m11-read-0", zero.clone()),
        bus("m11-read-1", one.clone()),
    );
    open(&bus_one, &account, &unique("INS-M11")).await;
    let read = PositionsRead {
        scope: Scope::Everything,
        account_id: account.clone(),
        since: None,
        business_date: String::new(),
        at: None,
        limit: 10,
        cursor: String::new(),
    };
    assert_eq!(
        blocking(|| reading.positions(&read)).unwrap().records.len(),
        1,
        "its opening balance, before the move"
    );
    // The move made between the member's look and its read, once.
    let moved = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let (moved, account, to) = (moved.clone(), account.clone(), to.clone());
        let (bus_zero, bus_one) = (bus_zero.clone(), bus_one.clone());
        let runtime = tokio::runtime::Handle::current();
        points::arm(
            "book.read.held",
            &account.clone(),
            Arc::new(move |_| {
                if !moved.swap(true, Ordering::SeqCst) {
                    let request = MoveAccountRequest {
                        account_id: account.clone(),
                        target_partition: to.clone(),
                        reason: "moved while it was read".into(),
                    };
                    // Closed by group 1 in database 1; opened by group 0 in
                    // database 0, which then lets database 1 go of it.
                    let _ = runtime.block_on(send(
                        &bus_one,
                        meridian_bor::moves::MOVE_ACCOUNT,
                        &request,
                        Some(PERSON),
                    ));
                    runtime
                        .block_on(send(
                            &bus_zero,
                            meridian_bor::moves::MOVE_ACCOUNT,
                            &request,
                            Some(PERSON),
                        ))
                        .map_err(|failed| format!("the move failed: {failed}"))?;
                }
                Ok(())
            }),
        );
    }
    let after = blocking(|| reading.positions(&read));
    points::disarm("book.read.held", &account);
    assert!(moved.load(Ordering::SeqCst), "the account moved meanwhile");
    assert_eq!(
        after.unwrap().records.len(),
        1,
        "read where it went, never as empty where it was"
    );
    blocking(move || drop((zero, one, reading)));
    drop((bus_zero, bus_one));
}

/// A partition is moved home by a raised database count only once the
/// book's rollout is complete (step 3d; sections 6.4 and 6.8; `book_member`,
/// ruled 2026-11-15, item 9; PR CI 43656183991, ha: ACC-DRIVE-00 read as
/// empty twice). The count alone was raised, the release the same: a member
/// started before it, reading one database, is still seen. While it is, the
/// control group's sweep moves nothing home -- that member would answer an
/// account whose partition left its database as empty. Once it is no longer
/// seen within the interval, the sweep moves the partition home.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn m11_a_partition_is_moved_home_only_once_every_member_seen_runs_the_sweeps_revision() {
    use meridian_bor::partition_moves::{rebalance, Ended, Mover, Settings};
    use meridian_bor::spread::Spread;
    use meridian_changes::groups::{owner, Groups};
    use meridian_changes::members::revision;
    let urls: Vec<Scratch> = blocking(|| {
        (0..2)
            .map(|_| {
                let (url, migrated) = isolated();
                migrated.make_partitions(8).unwrap();
                apart(&url);
                url
            })
            .collect()
    });
    // A member of `group` over the first `databases` databases.
    let member = |group: u32, databases: usize| {
        let groups = Groups::new(group, 2).unwrap();
        let stores: Vec<Arc<PostgresStore>> = urls[..databases]
            .iter()
            .map(|url| {
                let store = PostgresStore::connect(url, 6)
                    .unwrap()
                    .with_groups(groups)
                    .with_partitions(8);
                store.pin_owners().unwrap();
                Arc::new(store)
            })
            .collect();
        let spread = Arc::new(Spread::new(stores, groups));
        let bus = Arc::new(Bus::single(
            "bor-m11-rollout-test",
            Arc::new(MemoryBackend::new()),
            Arc::new(meridian_clock::SystemClock),
        ));
        serve(bus.clone(), spread.clone(), bus.clock());
        resolving_equities(&bus);
        (bus, spread)
    };
    let partition = ["P4", "P5", "P6", "P7"]
        .into_iter()
        .find(|p| owner(p, 2) == 1)
        .unwrap()
        .to_string();
    let account = (0..)
        .map(|k| unique(&format!("ACC-ROLLOUT-{k}")))
        .find(|a| meridian_bor::store::placement_of(a, 8) == partition)
        .unwrap();
    // One database: group 1 writes its partition there.
    {
        let (bus_one, one) = blocking(|| member(1, 1));
        blocking(|| one.place_first(&account, &account)).unwrap();
        open(&bus_one, &account, "INS-AAPL").await;
        blocking(move || drop((one, bus_one)));
    }
    // The count raised to two databases: group 1's home is the second.
    let (bus_zero, zero) = blocking(|| member(0, 2));
    // Group 1's writer at two databases started, as the runtime starts it:
    // its epoch taken at its home, where its partition moves.
    let (bus_one, one) = blocking(|| member(1, 2));
    blocking(|| Store::adopt(one.as_ref())).unwrap();
    let fresh = |d: usize| PostgresStore::connect(&urls[d], 1).unwrap();
    let head = |d: usize| {
        blocking(|| fresh(d).tip(&partition))
            .unwrap()
            .map_or(0, |tip| tip.head)
    };
    assert!(head(0) > 0 && head(1) == 0);
    // The same release, the count alone raised: one database against two.
    let (before, after) = (
        revision("0.1.540", 1, 2, Some(8)),
        revision("0.1.540", 2, 2, Some(8)),
    );
    let within = std::time::Duration::from_secs(6);
    let now = meridian_clock::Clock::now_ns(&meridian_clock::SystemClock);
    // The sweep's own member, and one started before the raise, both seen.
    blocking(|| {
        let home = zero.database(0);
        home.member_seen("bor-0-new", &after, now, now).unwrap();
        home.member_seen("bor-group-1-old", &before, now - 60_000_000_000, now)
            .unwrap();
    });
    let mover = Mover::new(
        vec![zero.database(0).clone(), zero.database(1).clone()],
        zero.database(0).groups(),
        Settings {
            switch: std::time::Duration::from_secs(10),
            ..Default::default()
        },
        Arc::new(meridian_clock::SystemClock),
    )
    .waiting_for_rollout(after.clone(), within);
    let held = blocking(|| rebalance(&bus_zero, zero.as_ref(), &mover));
    assert!(
        held.is_empty(),
        "held: a member seen lately reads one database ({held:?})"
    );
    assert_eq!(head(1), 0, "nothing of it moved to its home");
    // That member gone: its row last seen before the interval.
    blocking(|| {
        zero.database(0)
            .member_seen(
                "bor-group-1-old",
                &before,
                now - 60_000_000_000,
                meridian_clock::Clock::now_ns(&meridian_clock::SystemClock)
                    - within.as_nanos() as i64
                    - 1_000_000_000,
            )
            .unwrap()
    });
    let ended = blocking(|| rebalance(&bus_zero, zero.as_ref(), &mover));
    let moved: Vec<String> = ended
        .iter()
        .map(|e| match e {
            Ended::Moved(m) => m.partition.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert!(
        moved.contains(&partition),
        "moved home once it was gone: {moved:?}"
    );
    assert!(head(1) >= head(0), "at home, level with where it was");
    blocking(move || drop((zero, bus_zero, one, bus_one)));
}

/// A move by hand to another database waits for the book's rollout too
/// (step 3d; sections 6.4 and 6.8; ruled 2026-11-15): during one it would
/// cause the read Elle caught, as the sweep's move home did. The count
/// alone raised, a member reading one database still seen:
/// - a partition's move, whose reply said it had begun, waits in the
///   background and runs once that member is gone;
/// - an account's move, answered when done, is refused OVERLOADED with a
///   retry-after saying a rollout is in progress, nothing closed, and made
///   once that member is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn m11_a_move_by_hand_to_another_database_waits_for_the_rollout() {
    use meridian_bor::partition_moves::{Asked, Ended, Mover, Settings};
    use meridian_bor::spread::Spread;
    use meridian_changes::groups::Groups;
    use meridian_changes::members::revision;
    use meridian_domain::v1::{MoveAccountReply, MoveAccountRequest};
    let urls: Vec<Scratch> = blocking(|| {
        (0..2)
            .map(|_| {
                let (url, migrated) = isolated();
                migrated.make_partitions(4).unwrap();
                url
            })
            .collect()
    });
    // The same release, the count alone raised: one database against two.
    let (before, after) = (
        revision("0.1.540", 1, 2, Some(4)),
        revision("0.1.540", 2, 2, Some(4)),
    );
    let within = std::time::Duration::from_secs(6);
    let spread = |group: u32| {
        let groups = Groups::new(group, 2).unwrap();
        let stores = urls
            .iter()
            .map(|url| {
                Arc::new(
                    PostgresStore::connect(url, 4)
                        .unwrap()
                        .with_groups(groups)
                        .with_partitions(4),
                )
            })
            .collect();
        Arc::new(Spread::new(stores, groups).waiting_for_rollout(
            after.clone(),
            within,
            Arc::new(meridian_clock::SystemClock),
        ))
    };
    let (zero, one) = blocking(|| (spread(0), spread(1)));
    let groups = Groups::new(0, 2).unwrap();
    // Two accounts of group 1's, in database 1, in two of its partitions
    // where it has two; a partition of group 0's, in database 0.
    let account = (0..)
        .map(|n| unique(&format!("ACC-M11-HAND-{n}")))
        .find(|account| groups.owner(&meridian_bor::store::placement_of(account, 4)) == 1)
        .unwrap();
    blocking(|| one.place_first(&account, &account)).unwrap();
    let from = blocking(|| Store::partition_of(one.as_ref(), &account)).unwrap();
    let to = (0..4)
        .map(meridian_bor::store::partition_named)
        .find(|partition| groups.owner(partition) == 0)
        .unwrap();
    let bus = |name: &str, store: Arc<Spread>| {
        let bus = Arc::new(Bus::single(
            name,
            Arc::new(MemoryBackend::new()),
            Arc::new(meridian_clock::SystemClock),
        ));
        serve(bus.clone(), store, bus.clock());
        resolving_equities(&bus);
        bus
    };
    let (bus_zero, bus_one) = (
        bus("m11-hand-0", zero.clone()),
        bus("m11-hand-1", one.clone()),
    );
    open(&bus_one, &account, &unique("INS-M11")).await;
    let now = || meridian_clock::Clock::now_ns(&meridian_clock::SystemClock);
    // A member started before the raise, seen: and then not.
    let old_member = |seen_at: i64| {
        blocking(|| {
            zero.database(0)
                .member_seen("bor-group-1-old", &before, 1, seen_at)
                .unwrap()
        })
    };
    let gone = || old_member(now() - within.as_nanos() as i64 - 1_000_000_000);
    old_member(now());

    // The account's move to group 0's partition, in database 0: refused.
    let request = MoveAccountRequest {
        account_id: account.clone(),
        target_partition: to.clone(),
        reason: "group 1 is busy".into(),
    };
    let refused = send(
        &bus_one,
        meridian_bor::moves::MOVE_ACCOUNT,
        &request,
        Some(PERSON),
    )
    .await
    .unwrap_err();
    let overloaded = format!(
        "refusal-reason {}",
        meridian_pb::v1::RefusalReason::Overloaded as i32
    );
    assert!(
        refused.contains(&overloaded)
            && refused.contains("retry=")
            && refused.contains("a rollout in progress"),
        "{refused}"
    );
    blocking(|| {
        assert!(
            !one.journal(&account)
                .unwrap()
                .iter()
                .any(|entry| entry.idempotency_key.starts_with("account-move:")),
            "nothing closed"
        );
    });

    // Its partition's move to group 0, in database 0: begun, and waiting.
    let mover = Arc::new(
        Mover::new(
            vec![zero.database(0).clone(), zero.database(1).clone()],
            zero.database(0).groups(),
            Settings {
                switch: std::time::Duration::from_secs(10),
                ..Default::default()
            },
            Arc::new(meridian_clock::SystemClock),
        )
        .waiting_for_rollout(after.clone(), within),
    );
    let moving = {
        let (mover, partition) = (mover.clone(), from.clone());
        std::thread::spawn(move || {
            assert!(mover.begin(&partition));
            mover.run(
                &partition,
                0,
                &Asked {
                    by_hand: true,
                    moved_by: "admin-1".into(),
                    reason: "moved by hand during a rollout".into(),
                },
            )
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert!(
        !moving.is_finished(),
        "the partition's move waits while a member reads one database"
    );
    // The account's move, while that member is seen again and again: still
    // refused, the same way.
    old_member(now());
    assert!(send(
        &bus_one,
        meridian_bor::moves::MOVE_ACCOUNT,
        &request,
        Some(PERSON),
    )
    .await
    .is_err_and(|refused| refused.contains("a rollout in progress")));
    // That member gone: the partition's move runs.
    gone();
    let ended = blocking(|| moving.join().unwrap()).unwrap();
    match &ended {
        Ended::Moved(moved) => assert_eq!((moved.from_group, moved.to_group), (1, 0), "{moved:?}"),
        other => panic!("{other:?}"),
    }
    // And an account's move across databases is made: back to a partition
    // of group 1's, in database 1, from where its partition now is.
    let back = (0..4)
        .map(meridian_bor::store::partition_named)
        .find(|partition| groups.owner(partition) == 1 && *partition != from)
        .or_else(|| Some(to.clone()))
        .unwrap();
    blocking(|| {
        for member in [&zero, &one] {
            PostgresStore::read_owners_from(&[
                member.database(0).clone(),
                member.database(1).clone(),
            ])
            .unwrap();
        }
    });
    let request = MoveAccountRequest {
        account_id: account.clone(),
        target_partition: back.clone(),
        reason: "the rollout is done".into(),
    };
    // Closed by the group that writes it now, opened by the other.
    let mut moved = None;
    let mut said = Vec::new();
    for bus in [&bus_zero, &bus_one, &bus_zero, &bus_one] {
        match send(
            bus,
            meridian_bor::moves::MOVE_ACCOUNT,
            &request,
            Some(PERSON),
        )
        .await
        {
            Ok(reply) => {
                moved = MoveAccountReply::decode(reply.as_slice()).unwrap().moved;
                break;
            }
            Err(refused) => said.push(refused),
        }
    }
    let moved = moved.unwrap_or_else(|| panic!("moved once the rollout was complete: {said:?}"));
    assert_eq!(moved.to_partition, back);
    assert!(moved.opening_sequence > 0);
    blocking(move || drop((zero, one, mover)));
    drop((bus_zero, bus_one));
}

/// A write held by a move's switch is answered before its caller stops
/// waiting (step 3d; section 6.8, the hold: "at most moves.holdSeconds,
/// never past the caller's Meridian-Timeout-Ms"; PR CI 43675491409, ha).
/// The switch holds the partition's lock. A write whose caller waits less
/// than the hold was held the whole hold all the same, and its caller
/// timed out having heard nothing. It is now refused OVERLOADED with a
/// retry-after while its caller still listens.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m11_a_write_held_by_a_switch_is_answered_before_its_callers_deadline() {
    let (bus, store) = wired();
    let account = unique("ACC-M11-HELD");
    write_once(&bus, &account, 1).await.unwrap();
    let partition = blocking(|| Store::partition_of(store.as_ref(), &account))
        .unwrap_or_else(|| "P0".to_string());
    // The partition's lock held, as a switch holds it, for three seconds.
    let holding = {
        let key = meridian_bor::postgres::partition_lock_key(&partition);
        std::thread::spawn(move || {
            let mut held = postgres::Client::connect(&url(), postgres::NoTls).unwrap();
            let mut tx = held.transaction().unwrap();
            tx.execute("SELECT pg_advisory_xact_lock($1)", &[&key])
                .unwrap();
            std::thread::sleep(std::time::Duration::from_secs(3));
            tx.commit().unwrap();
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    // A caller that waits a second and a half.
    let began = std::time::Instant::now();
    let said = bus
        .call_stamped(
            SET_ACCOUNT_ATTRIBUTE,
            "meridian.v1.SetAccountAttributeRequest",
            SetAccountAttributeRequest {
                account_id: account.clone(),
                attribute: Some(set_account_attribute_request::Attribute::WaitsForCompliance(true)),
                reason: "held by a switch".into(),
            }
            .encode_to_vec(),
            None,
            Some(std::time::Duration::from_millis(1_500)),
            &Stamp {
                acting_for_subject: PERSON.to_string(),
                account_scope: None,
                ..Default::default()
            },
        )
        .await
        .map(|_| ())
        .map_err(|failed| failed.to_string());
    let took = began.elapsed();
    let refused = said.expect_err("held past its caller's wait: refused");
    let overloaded = format!(
        "refusal-reason {}",
        meridian_pb::v1::RefusalReason::Overloaded as i32
    );
    assert!(
        refused.contains(&overloaded) && refused.contains("retry="),
        "answered OVERLOADED with a retry-after, never left to time out: {refused}"
    );
    assert!(
        took < std::time::Duration::from_millis(1_500),
        "answered while its caller still waited: {took:?}"
    );
    blocking(move || holding.join().unwrap());
    blocking(move || drop(store));
}

/// The book's proposals at a wealth platform's volume (stage M2;
/// kernel/throughput-at-wealth-platform-scale: a 10,000- and a
/// 100,000-order proposal end to end through the book), against the
/// sidecar's per-plugin caps and the broker's pending bound: `make bench`
/// runs it by hand, never the gate.
///
/// One plugin proposing `MERIDIAN_BENCH_ORDERS` orders (10,000 by default)
/// across `MERIDIAN_BENCH_ACCOUNTS` accounts (1,000), with
/// `MERIDIAN_BENCH_IN_FLIGHT` requests at once (each of 1, 8, 16, 32 and
/// 64 in turn): there is no bulk proposal, so each order is one request,
/// as through a sidecar. Per level it prints the book's rate and each
/// request's latency (p50, p99, the most), and per order the bytes asked,
/// answered and published on the book's events -- what a subscriber's
/// pending bytes at the broker grow by while it falls behind. The book's
/// pool and its handlers at once are the deployment's (8; 16).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "the capacity bench: make bench"]
async fn capacity_book_proposals_at_wealth_scale() {
    use meridian_bor::order_service::*;
    let from_env =
        |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.into());
    let orders: usize = from_env("MERIDIAN_BENCH_ORDERS", "10000").parse().unwrap();
    let accounts: usize = from_env("MERIDIAN_BENCH_ACCOUNTS", "1000").parse().unwrap();
    let levels: Vec<usize> = from_env("MERIDIAN_BENCH_IN_FLIGHT", "1,8,16,32,64")
        .split(',')
        .map(|n| n.trim().parse().unwrap())
        .collect();
    let (bus, _store) = wired();
    meridian_domain::money::learn("USD", "INS-CASH-USD");
    let run = unique("BENCH");
    let accounts: Vec<String> = (0..accounts).map(|n| format!("ACC-{run}-{n}")).collect();
    for account in &accounts {
        send_order(
            &bus,
            SET_ACCOUNT_ATTRIBUTE,
            &SetAccountAttributeRequest {
                account_id: account.clone(),
                attribute: Some(
                    set_account_attribute_request::Attribute::WaitsForCompliance(false),
                ),
                reason: "the bench proposes without a compliance plugin".into(),
            },
            Some(PERSON),
            None,
        )
        .await
        .unwrap();
    }
    // What the book publishes, counted as a subscriber's pending bytes would.
    let published = Arc::new(AtomicU64::new(0));
    let events = Arc::new(AtomicU64::new(0));
    let mut heard = bus.subscribe_every("platform.book.event.**");
    let drops = heard.drop_count();
    let counting = {
        let (published, events) = (Arc::clone(&published), Arc::clone(&events));
        tokio::spawn(async move {
            while let Some(delivery) = heard.recv().await {
                published.fetch_add(delivery.envelope.encoded_len() as u64, Ordering::Relaxed);
                events.fetch_add(1, Ordering::Relaxed);
            }
        })
    };
    for at_once in levels {
        let (published_before, events_before) = (
            published.load(Ordering::Relaxed),
            events.load(Ordering::Relaxed),
        );
        let next = Arc::new(AtomicU64::new(0));
        let asked_bytes = Arc::new(AtomicU64::new(0));
        let answered_bytes = Arc::new(AtomicU64::new(0));
        let started = std::time::Instant::now();
        let mut workers = Vec::new();
        for _ in 0..at_once {
            let (bus, accounts, next, run) = (
                Arc::clone(&bus),
                accounts.clone(),
                Arc::clone(&next),
                run.clone(),
            );
            let (asked_bytes, answered_bytes) =
                (Arc::clone(&asked_bytes), Arc::clone(&answered_bytes));
            workers.push(tokio::spawn(async move {
                let mut took = Vec::new();
                loop {
                    let n = next.fetch_add(1, Ordering::Relaxed) as usize;
                    if n >= orders {
                        return took;
                    }
                    let proposal = ProposeOrderRequest {
                        account_id: accounts[n % accounts.len()].clone(),
                        instrument_id: "INS-AAPL".into(),
                        sleeve: "core".into(),
                        side: Side::Buy as i32,
                        quantity: d("10"),
                        order_type: OrderType::Limit as i32,
                        limit_price: usd("227.50"),
                        time_in_force: TimeInForce::Day as i32,
                        idempotency_key: format!("{run}-{at_once}-{n}"),
                        ..Default::default()
                    };
                    asked_bytes.fetch_add(proposal.encoded_len() as u64, Ordering::Relaxed);
                    let began = std::time::Instant::now();
                    let answer = send_order(&bus, PROPOSE_ORDER, &proposal, Some(PERSON), None)
                        .await
                        .unwrap_or_else(|refused| panic!("order {n} refused: {refused}"));
                    took.push(began.elapsed());
                    answered_bytes.fetch_add(answer.len() as u64, Ordering::Relaxed);
                }
            }));
        }
        let mut took = Vec::new();
        for worker in workers {
            took.extend(worker.await.unwrap());
        }
        let elapsed = started.elapsed();
        // The events of the last few orders are still on their way.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        took.sort();
        let at = |q: f64| took[((took.len() as f64 * q) as usize).min(took.len() - 1)];
        let per_order = |bytes: u64| bytes as f64 / orders as f64;
        println!(
            "CAPACITY book proposals orders={orders} in_flight={at_once} rate_per_s={:.0} \
             p50_ms={:.2} p99_ms={:.2} most_ms={:.2} asked_bytes={:.0} answered_bytes={:.0} \
             events_per_order={:.2} event_bytes_per_order={:.0} events_dropped={}",
            orders as f64 / elapsed.as_secs_f64(),
            at(0.50).as_secs_f64() * 1e3,
            at(0.99).as_secs_f64() * 1e3,
            took.last().unwrap().as_secs_f64() * 1e3,
            per_order(asked_bytes.load(Ordering::Relaxed)),
            per_order(answered_bytes.load(Ordering::Relaxed)),
            (events.load(Ordering::Relaxed) - events_before) as f64 / orders as f64,
            per_order(published.load(Ordering::Relaxed) - published_before),
            drops.get(),
        );
    }
    counting.abort();
}

/// What a command handed to its writer group costs (step 3d; the resilience
/// spec's section 6.7: measured against decisions/024's budget, a bus hop's
/// 5 ms and a command to its journal entry's 25 ms, p99), through a real
/// broker: `make bench`'s, by hand, MERIDIAN_BENCH_BROKER_URL naming one.
///
/// At one group and at two, each over a database of its own, the book's
/// members serving its commands in one queue group, as a deployment runs
/// them: `MERIDIAN_BENCH_COMMANDS` proposals (2,000) asked one at a time by
/// a plugin's bus. At two the broker gives about half to group 0's member,
/// which hands each to group 1's, P0's owner. It prints each run's latency
/// (p50, p90, p99, the most) and, at two, the hand-offs' count, mean and
/// most, as the handing member timed them.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "the capacity bench: make bench"]
async fn capacity_book_hand_off() {
    use meridian_bor::order_service::*;
    use meridian_bor::writer::{is_command, Writer, INTERNAL_COMMAND};
    use meridian_changes::groups::Groups;
    let broker = std::env::var("MERIDIAN_BENCH_BROKER_URL")
        .expect("MERIDIAN_BENCH_BROKER_URL names the broker the bench asks through");
    let commands: usize = std::env::var("MERIDIAN_BENCH_COMMANDS")
        .unwrap_or_else(|_| "2000".into())
        .parse()
        .unwrap();
    meridian_domain::money::learn("USD", "INS-CASH-USD");
    for groups in [1u32, 2] {
        let (url, migrated) = blocking(isolated);
        let run = unique("HANDOFF");
        let instance = format!("bor-bench-{run}");
        let mut held = vec![migrated];
        for group in 0..groups {
            let store = blocking(|| {
                Arc::new(
                    PostgresStore::connect(&url, 8)
                        .unwrap()
                        .with_groups(Groups::new(group, groups).unwrap()),
                )
            });
            let backend = Arc::new(meridian_bus::NatsBackend::connect(&broker).await.unwrap());
            let bus = Arc::new(Bus::single(
                instance.clone(),
                backend,
                Arc::new(meridian_clock::SystemClock),
            ));
            if groups > 1 {
                bus.hand_writes_on(INTERNAL_COMMAND, group, is_command);
                // The owner decided from the request first, as the book's
                // binary routes (the ownership pre-check, 2026-11-13).
                let placing = Arc::clone(&store);
                bus.route_writes_by(move |topic, envelope| {
                    let account =
                        meridian_bor::service::account_of_write(topic, &envelope.payload)?;
                    placing.writer_group_of(&account)
                });
            }
            let writer: Arc<dyn Store> = Arc::new(Writer::new(
                Arc::clone(&store) as Arc<dyn Store>,
                Arc::clone(&bus),
            ));
            meridian_bor::service::serve(bus.clone(), writer, bus.clock());
            bus.serve(RESOLVE_INSTRUMENT, |envelope| {
                let asked = ResolveInstrumentRequest::decode(&envelope.payload[..]).unwrap();
                Ok((
                    "meridian.v1.ResolveInstrumentReply".into(),
                    ResolveInstrumentReply {
                        found: true,
                        instrument: Some(InstrumentRecord {
                            instrument_id: asked.instrument_id,
                            asset_class: AssetClass::Equity as i32,
                            currency: "USD".into(),
                            version: 1,
                            ..Default::default()
                        }),
                    }
                    .encode_to_vec(),
                ))
            });
            held.push(store);
        }
        let client = Arc::new(Bus::single(
            format!("operations-bench-{run}"),
            Arc::new(meridian_bus::NatsBackend::connect(&broker).await.unwrap()),
            Arc::new(meridian_clock::SystemClock),
        ));
        // The broker learns every member's subscriptions before the first ask.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let accounts: Vec<String> = (0..50).map(|n| format!("ACC-{run}-{n}")).collect();
        for account in &accounts {
            send_order(
                &client,
                SET_ACCOUNT_ATTRIBUTE,
                &SetAccountAttributeRequest {
                    account_id: account.clone(),
                    attribute: Some(
                        set_account_attribute_request::Attribute::WaitsForCompliance(false),
                    ),
                    reason: "the bench proposes without a compliance plugin".into(),
                },
                Some(PERSON),
                None,
            )
            .await
            .unwrap();
        }
        let (count_before, total_before, _) = meridian_bus::owners::handed_on();
        let mut took = Vec::with_capacity(commands);
        let started = std::time::Instant::now();
        for n in 0..commands {
            let proposal = ProposeOrderRequest {
                account_id: accounts[n % accounts.len()].clone(),
                instrument_id: "INS-AAPL".into(),
                sleeve: "core".into(),
                side: Side::Buy as i32,
                quantity: d("10"),
                order_type: OrderType::Limit as i32,
                limit_price: usd("227.50"),
                time_in_force: TimeInForce::Day as i32,
                idempotency_key: format!("{run}-{n}"),
                ..Default::default()
            };
            let began = std::time::Instant::now();
            send_order(&client, PROPOSE_ORDER, &proposal, Some(PERSON), None)
                .await
                .unwrap_or_else(|refused| panic!("proposal {n} refused: {refused}"));
            took.push(began.elapsed());
        }
        let elapsed = started.elapsed();
        took.sort();
        let at = |q: f64| took[((took.len() as f64 * q) as usize).min(took.len() - 1)];
        let (count_after, total_after, most) = meridian_bus::owners::handed_on();
        let handed = count_after - count_before;
        let mean_ms = if handed > 0 {
            (total_after - total_before).as_secs_f64() * 1e3 / handed as f64
        } else {
            0.0
        };
        println!(
            "CAPACITY book hand-off groups={groups} commands={commands} rate_per_s={:.0} \
             p50_ms={:.2} p90_ms={:.2} p99_ms={:.2} most_ms={:.2} handed={handed} \
             handed_mean_ms={mean_ms:.2} handed_most_ms={:.2}",
            commands as f64 / elapsed.as_secs_f64(),
            at(0.50).as_secs_f64() * 1e3,
            at(0.90).as_secs_f64() * 1e3,
            at(0.99).as_secs_f64() * 1e3,
            took.last().unwrap().as_secs_f64() * 1e3,
            most.as_secs_f64() * 1e3,
        );
        blocking(move || drop(held));
    }
}

/// What an open-loop run measured: each ask's time from when it was due, in
/// the order asked, how many were in flight at the most, and how long the
/// whole run took.
struct OpenLoop {
    took: Vec<std::time::Duration>,
    most_in_flight: usize,
    elapsed: std::time::Duration,
}

/// `count` asks at a fixed arrival rate, `per_second` (step 3d, stage T;
/// the bench's open-loop mode, PO 2026-11-14): ask `n` is due at
/// `n / per_second` from the start and is made then, whether or not the
/// asks before it were answered, so the load offered never slows with the
/// answers. Each ask's time counts from when it was due, never from when
/// it was sent: a closed loop, which asks again only once answered, hides
/// the wait of every ask it did not make meanwhile.
async fn open_loop<F, W>(count: usize, per_second: f64, ask: F) -> OpenLoop
where
    F: Fn(usize) -> W,
    W: std::future::Future<Output = ()> + Send + 'static,
{
    let started = tokio::time::Instant::now();
    let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let most = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut asked = Vec::with_capacity(count);
    for n in 0..count {
        let due = started + std::time::Duration::from_secs_f64(n as f64 / per_second);
        tokio::time::sleep_until(due).await;
        let (in_flight, most) = (Arc::clone(&in_flight), Arc::clone(&most));
        most.fetch_max(
            in_flight.fetch_add(1, Ordering::SeqCst) + 1,
            Ordering::SeqCst,
        );
        let work = ask(n);
        asked.push(tokio::spawn(async move {
            work.await;
            in_flight.fetch_sub(1, Ordering::SeqCst);
            due.elapsed()
        }));
    }
    let mut took = Vec::with_capacity(count);
    for ask in asked {
        took.push(ask.await.unwrap());
    }
    OpenLoop {
        took,
        most_in_flight: most.load(Ordering::SeqCst),
        elapsed: started.elapsed(),
    }
}

/// The open loop keeps its arrival rate whatever the answers take: 50 asks
/// at 10 a second, each answered in a second, are all made within 4.9 s --
/// ten in flight at once -- and done at 5.9 s, each taking its one second.
/// And with one ask stuck for 30 s the ones after it are made on time and
/// take their one second each: its wait is its own. Asked one at a time, as
/// a closed loop asks, the 50 take 50 s and the stuck one delays every ask
/// after it unseen.
#[tokio::test(start_paused = true)]
async fn an_open_loop_asks_at_its_rate_whatever_the_answers_take() {
    let second = std::time::Duration::from_secs(1);
    let ran = open_loop(50, 10.0, |_| tokio::time::sleep(second)).await;
    assert_eq!(ran.took.len(), 50);
    assert!(
        ran.took.iter().all(|took| *took == second),
        "each ask took its own second from when it was due: {:?}",
        ran.took.iter().max()
    );
    assert_eq!(
        ran.elapsed,
        std::time::Duration::from_millis(5_900),
        "the last was due at 4.9 s and took its second"
    );
    // The ask due as the first is answered may be made before or after it.
    assert!(
        (10..=11).contains(&ran.most_in_flight),
        "a second's arrivals at once: {}",
        ran.most_in_flight
    );
    // One ask stuck: nothing after it waits on it.
    let ran = open_loop(50, 10.0, |n| {
        tokio::time::sleep(if n == 5 { 30 * second } else { second })
    })
    .await;
    assert_eq!(ran.took[5], 30 * second);
    assert!(
        ran.took
            .iter()
            .enumerate()
            .all(|(n, took)| n == 5 || *took == second),
        "the asks after the stuck one were made when due"
    );
    assert!(
        (11..=12).contains(&ran.most_in_flight),
        "a second's arrivals and the stuck one: {}",
        ran.most_in_flight
    );
}

/// The book across writer groups and partitions (step 3d, stage B; the
/// brief's benches, figures private): `MERIDIAN_BENCH_PROPOSALS`
/// proposals (100,000) at `MERIDIAN_BENCH_IN_FLIGHT` at once (64), through
/// a real broker, at one group over one partition, two over two and two
/// over four; then the light-load check, `MERIDIAN_BENCH_LIGHT` (2,000)
/// asked one at a time, at each. The book's members serve its commands in
/// one queue group as a deployment runs them, each handing what is not its
/// group's to its owner; each account placed at its first act by its ID,
/// the conductor's accounts read answered by a stand-in. It prints each
/// run's rate and latency (p50, p90, p99, the most), and the hand-offs'
/// count, mean and most.
///
/// The open-loop mode (stage T; PO 2026-11-14): with `MERIDIAN_BENCH_RATE`
/// set -- proposals a second, or several rates by commas -- each shape
/// then takes `MERIDIAN_BENCH_PROPOSALS` proposals at each rate, each due
/// at its own time whatever the answers take ([`open_loop`]), its latency
/// from when it was due: a point of the latency/load curve a rate.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "the capacity bench: make bench"]
async fn capacity_book_partitions() {
    use meridian_bor::order_service::*;
    use meridian_bor::writer::{is_command, Writer, INTERNAL_COMMAND};
    use meridian_changes::groups::Groups;
    let broker = std::env::var("MERIDIAN_BENCH_BROKER_URL")
        .expect("MERIDIAN_BENCH_BROKER_URL names the broker the bench asks through");
    let from_env =
        |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.into());
    let proposals: usize = from_env("MERIDIAN_BENCH_PROPOSALS", "100000")
        .parse()
        .unwrap();
    let at_once: usize = from_env("MERIDIAN_BENCH_IN_FLIGHT", "64").parse().unwrap();
    let light: usize = from_env("MERIDIAN_BENCH_LIGHT", "2000").parse().unwrap();
    // The open loop's arrival rates, proposals a second: none, and the
    // closed-loop runs alone, as before.
    let rates: Vec<f64> = from_env("MERIDIAN_BENCH_RATE", "")
        .split(',')
        .filter(|rate| !rate.trim().is_empty())
        .map(|rate| {
            let rate: f64 = rate.trim().parse().expect("MERIDIAN_BENCH_RATE: a rate");
            assert!(rate > 0.0, "MERIDIAN_BENCH_RATE: a rate above 0");
            rate
        })
        .collect();
    let shapes: Vec<(u32, u32)> = from_env("MERIDIAN_BENCH_SHAPES", "1x1,2x2,2x4")
        .split(',')
        .map(|shape| {
            let (g, p) = shape.trim().split_once('x').unwrap();
            (g.parse().unwrap(), p.parse().unwrap())
        })
        .collect();
    meridian_domain::money::learn("USD", "INS-CASH-USD");
    // The conductor's accounts read, as the book asks it at an account's
    // first act: every bench account of every shape, no owner label, so each
    // is placed by its ID. One stand-in for the whole run.
    let known: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    // `make bench-book`'s: every run's figures, written as JSON where
    // MERIDIAN_BENCH_JSON names a file.
    let mut figures: Vec<serde_json::Value> = Vec::new();
    let conductor = Arc::new(Bus::single(
        format!("conductor-bench-{}", unique("C")),
        Arc::new(meridian_bus::NatsBackend::connect(&broker).await.unwrap()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let listing = Arc::clone(&known);
    conductor.serve("platform.config.query.accounts", move |_| {
        let accounts = listing
            .lock()
            .unwrap()
            .iter()
            .map(|account| AccountRecord {
                account_id: account.clone(),
                ..Default::default()
            })
            .collect();
        Ok((
            "meridian.v1.Accounts".into(),
            Accounts { accounts }.encode_to_vec(),
        ))
    });
    for (groups, partitions) in shapes {
        let (url, migrated) = blocking(isolated);
        blocking(|| migrated.make_partitions(partitions).unwrap());
        let run = unique("PARTS");
        let instance = format!("bor-bench-{run}");
        let accounts: Vec<String> = (0..1000).map(|n| format!("ACC-{run}-{n}")).collect();
        let mut held = vec![migrated];
        let mut buses = Vec::new();
        for group in 0..groups {
            let store = blocking(|| {
                Arc::new(
                    PostgresStore::connect(&url, 8)
                        .unwrap()
                        .with_groups(Groups::new(group, groups).unwrap())
                        .with_partitions(partitions),
                )
            });
            let backend = Arc::new(meridian_bus::NatsBackend::connect(&broker).await.unwrap());
            let bus = Arc::new(Bus::single(
                instance.clone(),
                backend,
                Arc::new(meridian_clock::SystemClock),
            ));
            if groups > 1 {
                bus.hand_writes_on(INTERNAL_COMMAND, group, is_command);
                let placing = Arc::clone(&store);
                bus.route_writes_by(move |topic, envelope| {
                    let account =
                        meridian_bor::service::account_of_write(topic, &envelope.payload)?;
                    placing.writer_group_of(&account)
                });
            }
            let writer: Arc<dyn Store> = Arc::new(Writer::new(
                Arc::clone(&store) as Arc<dyn Store>,
                Arc::clone(&bus),
            ));
            meridian_bor::service::serve(bus.clone(), writer, bus.clock());
            resolving_equities(&bus);
            held.push(store);
            buses.push(bus);
        }
        let client = Arc::new(Bus::single(
            format!("operations-bench-{run}"),
            Arc::new(meridian_bus::NatsBackend::connect(&broker).await.unwrap()),
            Arc::new(meridian_clock::SystemClock),
        ));
        known.lock().unwrap().extend(accounts.iter().cloned());
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        for account in &accounts {
            send_order(
                &client,
                SET_ACCOUNT_ATTRIBUTE,
                &SetAccountAttributeRequest {
                    account_id: account.clone(),
                    attribute: Some(
                        set_account_attribute_request::Attribute::WaitsForCompliance(false),
                    ),
                    reason: "the bench proposes without a compliance plugin".into(),
                },
                Some(PERSON),
                None,
            )
            .await
            .unwrap();
        }
        for (label, count, in_flight) in [("load", proposals, at_once), ("light", light, 1)] {
            let (count_before, total_before, _) = meridian_bus::owners::handed_on();
            let next = Arc::new(AtomicU64::new(0));
            let retried = Arc::new(AtomicU64::new(0));
            let timeouts = Arc::new(AtomicU64::new(0));
            let started = std::time::Instant::now();
            let mut workers = Vec::new();
            for _ in 0..in_flight {
                let (client, accounts, next, run, retried, timeouts) = (
                    Arc::clone(&client),
                    accounts.clone(),
                    Arc::clone(&next),
                    run.clone(),
                    Arc::clone(&retried),
                    Arc::clone(&timeouts),
                );
                workers.push(tokio::spawn(async move {
                    let mut took = Vec::new();
                    loop {
                        let n = next.fetch_add(1, Ordering::Relaxed) as usize;
                        if n >= count {
                            return took;
                        }
                        let proposal = ProposeOrderRequest {
                            account_id: accounts[n % accounts.len()].clone(),
                            instrument_id: "INS-AAPL".into(),
                            sleeve: "core".into(),
                            side: Side::Buy as i32,
                            quantity: d("10"),
                            order_type: OrderType::Limit as i32,
                            limit_price: usd("227.50"),
                            time_in_force: TimeInForce::Day as i32,
                            idempotency_key: format!("{run}-{label}-{n}"),
                            ..Default::default()
                        };
                        // Asked again after a refusal's retry-after, as a
                        // plugin's SDK does; its latency counts from the
                        // first ask.
                        let began = std::time::Instant::now();
                        loop {
                            match send_order(&client, PROPOSE_ORDER, &proposal, Some(PERSON), None)
                                .await
                            {
                                Ok(_) => break,
                                Err(refused) if refused.contains("timed out after") => {
                                    // Its caller stopped waiting: counted, and
                                    // asked again at once.
                                    timeouts.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(refused) => {
                                    let detail = refused
                                        .find('[')
                                        .map_or(refused.as_str(), |at| &refused[at..]);
                                    let Some(wait) = meridian_bus::refusal_retry_after(detail)
                                    else {
                                        panic!("proposal {n} refused: {refused}")
                                    };
                                    retried.fetch_add(1, Ordering::Relaxed);
                                    tokio::time::sleep(std::time::Duration::from_secs(u64::from(
                                        wait,
                                    )))
                                    .await;
                                }
                            }
                        }
                        took.push(began.elapsed());
                    }
                }));
            }
            let mut took = Vec::new();
            for worker in workers {
                took.extend(worker.await.unwrap());
            }
            let elapsed = started.elapsed();
            took.sort();
            let at = |q: f64| took[((took.len() as f64 * q) as usize).min(took.len() - 1)];
            let (count_after, total_after, most) = meridian_bus::owners::handed_on();
            let handed = count_after - count_before;
            let mean_ms = if handed > 0 {
                (total_after - total_before).as_secs_f64() * 1e3 / handed as f64
            } else {
                0.0
            };
            figures.push(serde_json::json!({
                "run": label,
                "groups": groups,
                "partitions": partitions,
                "proposals": count,
                "in_flight": in_flight,
                "rate_per_s": count as f64 / elapsed.as_secs_f64(),
                "p50_ms": at(0.50).as_secs_f64() * 1e3,
                "p99_ms": at(0.99).as_secs_f64() * 1e3,
                "max_ms": took.last().unwrap().as_secs_f64() * 1e3,
                "timeouts": timeouts.load(Ordering::Relaxed),
                "asked_again": retried.load(Ordering::Relaxed),
                "handed": handed,
                "handed_mean_ms": mean_ms,
            }));
            println!(
                "CAPACITY book partitions groups={groups} partitions={partitions} run={label} \
                 proposals={count} in_flight={in_flight} rate_per_s={:.0} p50_ms={:.2} \
                 p90_ms={:.2} p99_ms={:.2} most_ms={:.2} handed={handed} \
                 handed_mean_ms={mean_ms:.2} handed_most_ms={:.2} asked_again={}",
                count as f64 / elapsed.as_secs_f64(),
                at(0.50).as_secs_f64() * 1e3,
                at(0.90).as_secs_f64() * 1e3,
                at(0.99).as_secs_f64() * 1e3,
                took.last().unwrap().as_secs_f64() * 1e3,
                most.as_secs_f64() * 1e3,
                retried.load(Ordering::Relaxed),
            );
        }
        // The open loop (stage T): at each rate, every proposal due at its
        // own time, asked again after a refusal's retry-after as above,
        // its latency from when it was due.
        for rate in &rates {
            let label = format!("open-{rate}");
            let (count_before, total_before, _) = meridian_bus::owners::handed_on();
            let retried = Arc::new(AtomicU64::new(0));
            let timeouts = Arc::new(AtomicU64::new(0));
            let ran = open_loop(proposals, *rate, |n| {
                let (client, retried, timeouts) = (
                    Arc::clone(&client),
                    Arc::clone(&retried),
                    Arc::clone(&timeouts),
                );
                let proposal = ProposeOrderRequest {
                    account_id: accounts[n % accounts.len()].clone(),
                    instrument_id: "INS-AAPL".into(),
                    sleeve: "core".into(),
                    side: Side::Buy as i32,
                    quantity: d("10"),
                    order_type: OrderType::Limit as i32,
                    limit_price: usd("227.50"),
                    time_in_force: TimeInForce::Day as i32,
                    idempotency_key: format!("{run}-{label}-{n}"),
                    ..Default::default()
                };
                async move {
                    loop {
                        match send_order(&client, PROPOSE_ORDER, &proposal, Some(PERSON), None)
                            .await
                        {
                            Ok(_) => break,
                            Err(refused) if refused.contains("timed out after") => {
                                timeouts.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(refused) => {
                                let detail = refused
                                    .find('[')
                                    .map_or(refused.as_str(), |at| &refused[at..]);
                                let Some(wait) = meridian_bus::refusal_retry_after(detail) else {
                                    panic!("proposal {n} refused: {refused}")
                                };
                                retried.fetch_add(1, Ordering::Relaxed);
                                tokio::time::sleep(std::time::Duration::from_secs(u64::from(wait)))
                                    .await;
                            }
                        }
                    }
                }
            })
            .await;
            let mut took = ran.took;
            took.sort();
            let at = |q: f64| took[((took.len() as f64 * q) as usize).min(took.len() - 1)];
            let (count_after, total_after, _) = meridian_bus::owners::handed_on();
            let handed = count_after - count_before;
            let mean_ms = if handed > 0 {
                (total_after - total_before).as_secs_f64() * 1e3 / handed as f64
            } else {
                0.0
            };
            figures.push(serde_json::json!({
                "run": "open",
                "groups": groups,
                "partitions": partitions,
                "proposals": proposals,
                "offered_per_s": rate,
                "rate_per_s": proposals as f64 / ran.elapsed.as_secs_f64(),
                "most_in_flight": ran.most_in_flight,
                "p50_ms": at(0.50).as_secs_f64() * 1e3,
                "p90_ms": at(0.90).as_secs_f64() * 1e3,
                "p99_ms": at(0.99).as_secs_f64() * 1e3,
                "max_ms": took.last().unwrap().as_secs_f64() * 1e3,
                "timeouts": timeouts.load(Ordering::Relaxed),
                "asked_again": retried.load(Ordering::Relaxed),
                "handed": handed,
                "handed_mean_ms": mean_ms,
            }));
            println!(
                "CAPACITY book partitions groups={groups} partitions={partitions} run=open \
                 proposals={proposals} offered_per_s={rate} rate_per_s={:.0} most_in_flight={} \
                 p50_ms={:.2} p90_ms={:.2} p99_ms={:.2} most_ms={:.2} asked_again={}",
                proposals as f64 / ran.elapsed.as_secs_f64(),
                ran.most_in_flight,
                at(0.50).as_secs_f64() * 1e3,
                at(0.90).as_secs_f64() * 1e3,
                at(0.99).as_secs_f64() * 1e3,
                took.last().unwrap().as_secs_f64() * 1e3,
                retried.load(Ordering::Relaxed),
            );
        }
        // Where the accounts landed, a count a partition.
        let spread: Vec<(String, i64)> = blocking(|| {
            postgres::Client::connect(&url, postgres::NoTls)
                .unwrap()
                .query(
                    "SELECT partition, count(*) FROM book_account WHERE account_id NOT LIKE '~%' \
                     GROUP BY partition ORDER BY partition",
                    &[],
                )
                .unwrap()
                .iter()
                .map(|row| (row.get(0), row.get(1)))
                .collect()
        });
        println!(
            "CAPACITY book partitions groups={groups} partitions={partitions} spread={spread:?}"
        );
        drop(buses);
        blocking(move || drop(held));
    }
    if let Ok(path) = std::env::var("MERIDIAN_BENCH_JSON") {
        // The 1-, 5- and 15-minute loads, as /proc/loadavg says them.
        let averages = |said: &str| -> Option<[f64; 3]> {
            let mut each = said.split_whitespace().map(|n| n.parse::<f64>().ok());
            Some([each.next()??, each.next()??, each.next()??])
        };
        let load = std::fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|said| averages(&said));
        let before = std::env::var("MERIDIAN_BENCH_LOAD_BEFORE").unwrap_or_default();
        let written = serde_json::json!({
            "bench": "book",
            "commit": std::env::var("MERIDIAN_BENCH_COMMIT").unwrap_or_default(),
            "host": {
                "cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
                "load_before": averages(&before).map_or_else(
                    || serde_json::Value::from(before.clone()),
                    |loads| serde_json::json!(loads),
                ),
                "load_after": load,
            },
            "runs": figures,
        });
        std::fs::write(&path, serde_json::to_vec_pretty(&written).unwrap())
            .unwrap_or_else(|failed| panic!("{path} could not be written: {failed}"));
    }
}

/// Step 3d (section 6.3, option (c)): the book's takeovers and findings
/// become `~control` entries carrying their records, the ones kept before
/// backfilled, each record its own time and each entry the backfill's; a
/// second pass journals nothing again; and the control partition's change
/// log carries each record as the conductor's history reads it.
#[tokio::test(flavor = "multi_thread")]
async fn takeovers_and_findings_are_journalled_on_control_once_each() {
    let (url, store) = blocking(isolated);
    let bus = Arc::new(Bus::single(
        "operations-test-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    blocking(|| {
        store
            .take_over(&meridian_changes::TookOver {
                member: "om-bor-1".into(),
                previous: "om-bor-0".into(),
                why: meridian_changes::LEASE_LAPSED.into(),
                at_ns: 7,
            })
            .unwrap();
        let mut raw = postgres::Client::connect(&url, postgres::NoTls).unwrap();
        raw.execute(
            "INSERT INTO book_integrity_finding
                    (partition, sequence, committed_at_ns, what, member, origin, found_at_ns)
             VALUES ('P0', 3, 11, 'an entry does not match its digest', 'om-bor-1', '', 13)",
            &[],
        )
        .unwrap();
    });
    let journalled = blocking(|| meridian_bor::control::journal_owed(&bus, store.as_ref()));
    let again = blocking(|| meridian_bor::control::journal_owed(&bus, store.as_ref()));
    let changes = blocking(|| followed(&store, "control", 1 << 20));
    blocking(move || drop(store));
    assert_eq!(journalled, Ok(2), "the takeover and the finding");
    assert_eq!(again, Ok(0), "never twice");
    let records: Vec<(String, Vec<u8>, i64)> = changes
        .iter()
        .filter_map(|change| {
            meridian_bor::control::record_of_change(change)
                .map(|(kind, body)| (kind, body, change.committed_at_ns))
        })
        .collect();
    assert_eq!(records.len(), 2);
    let took = WriterTookOver::decode(&records[0].1[..]).unwrap();
    assert_eq!(records[0].0, "meridian.v1.WriterTookOver");
    assert_eq!(
        (took.member.as_str(), took.took_over_at_ns),
        ("om-bor-1", 7)
    );
    assert!(
        records[0].2 > 7,
        "the entry committed at the backfill's own time"
    );
    let found = IntegrityFinding::decode(&records[1].1[..]).unwrap();
    assert_eq!(records[1].0, "meridian.v1.IntegrityFinding");
    assert_eq!(
        (
            found.partition.as_str(),
            found.first_bad_sequence,
            found.found_at_ns
        ),
        ("P0", 3, 13)
    );
}

/// Step 3d (section 6.3; the source ruled 2026-11-13): with several
/// partitions, a new account is placed at its first act by rendezvous
/// hashing of its owner label as the conductor holds it, else its ID; the
/// key is kept with the placement, so editing the label moves nothing and
/// the book never asks again. An account the conductor does not hold yet,
/// or a conductor that does not answer, refuses the act UNAVAILABLE with a
/// retry-after: never placed by its ID instead.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_account_is_placed_by_its_owner_label_and_kept_there() {
    use meridian_bor::store::{placement_of, PARTITIONS_VAR};
    assert_eq!(PARTITIONS_VAR, "MERIDIAN_BOR_PARTITIONS");
    assert_eq!(meridian_bor::store::partitions_from(None), Ok(1));
    assert_eq!(meridian_bor::store::partitions_from(Some("4")), Ok(4));
    for bad in ["0", "65", "two"] {
        assert!(
            meridian_bor::store::partitions_from(Some(bad)).is_err(),
            "{bad}"
        );
    }
    let (url, made) = blocking(isolated);
    blocking(move || drop(made));
    let store = blocking(|| {
        let store = PostgresStore::connect(&url, 4).unwrap().with_partitions(4);
        assert_eq!(store.make_partitions(4).unwrap(), 3, "P1 to P3");
        assert_eq!(store.make_partitions(4).unwrap(), 0, "never twice");
        Arc::new(store)
    });
    let bus = Arc::new(Bus::single(
        "bor-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let (fund_a, fund_b, plain, unknown) =
        (unique("ACC"), unique("ACC"), unique("ACC"), unique("ACC"));
    let owners: Arc<std::sync::Mutex<Vec<(String, String)>>> =
        Arc::new(std::sync::Mutex::new(vec![
            (fund_a.clone(), "Fund I".into()),
            (fund_b.clone(), " Fund I ".into()),
            (plain.clone(), String::new()),
        ]));
    let asked = Arc::new(AtomicU64::new(0));
    let (holding, counting) = (owners.clone(), asked.clone());
    bus.serve("platform.config.query.accounts", move |envelope| {
        counting.fetch_add(1, Ordering::SeqCst);
        let meta = envelope.meta.clone().unwrap_or_default();
        assert_eq!(
            meta.publisher_instance_id, "bor-1",
            "the book asks as itself"
        );
        assert!(meta.acting_for_subject.is_empty() && !meta.account_scope_applies);
        Ok((
            "meridian.v1.Accounts".into(),
            Accounts {
                accounts: holding
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(id, owner)| AccountRecord {
                        account_id: id.clone(),
                        owner: owner.clone(),
                        ..Default::default()
                    })
                    .collect(),
            }
            .encode_to_vec(),
        ))
    });
    let writer: Arc<dyn Store> = Arc::new(meridian_bor::writer::Writer::new(
        store.clone() as Arc<dyn Store>,
        bus.clone(),
    ));
    serve(bus.clone(), writer, bus.clock());
    resolving_equities(&bus);
    for account in [&fund_a, &fund_b, &plain] {
        open(&bus, account, "INS-AAPL").await;
    }
    let placed = |account: &str| -> (String, String) {
        let mut raw = postgres::Client::connect(&url, postgres::NoTls).unwrap();
        let row = raw
            .query_one(
                "SELECT partition, placement_key FROM book_account WHERE account_id = $1",
                &[&account],
            )
            .unwrap();
        (row.get(0), row.get(1))
    };
    let fund = blocking(|| placed(&fund_a));
    assert_eq!(fund, (placement_of("Fund I", 4), "Fund I".to_string()));
    assert_eq!(
        blocking(|| placed(&fund_b)),
        fund,
        "a fund's accounts land together"
    );
    assert_eq!(
        blocking(|| placed(&plain)),
        (placement_of(&plain, 4), plain.clone()),
        "no label: by its ID"
    );
    assert_eq!(
        asked.load(Ordering::SeqCst),
        3,
        "asked once each, at its first act"
    );

    // Editing the label moves nothing, and the book never asks again.
    owners.lock().unwrap()[0].1 = "Fund II".into();
    send(
        &bus,
        RECORD_OPENING_BALANCE,
        &opening(&fund_a, "INS-AAPL"),
        Some(PERSON),
    )
    .await
    .ok();
    assert_eq!(blocking(|| placed(&fund_a)), fund);
    assert_eq!(asked.load(Ordering::SeqCst), 3);

    // Not in the conductor's answer yet: refused UNAVAILABLE, retry-after.
    let refused = send(
        &bus,
        RECORD_OPENING_BALANCE,
        &opening(&unknown, "INS-AAPL"),
        Some(PERSON),
    )
    .await
    .unwrap_err();
    assert!(refused.contains("unavailable"), "{refused}");
    assert_eq!(
        meridian_bus::refusal_retry_after(&refused[refused.find("[refusal-status").unwrap()..]),
        Some(5),
        "{refused}"
    );
    let none: Option<String> = blocking(|| {
        postgres::Client::connect(&url, postgres::NoTls)
            .unwrap()
            .query_opt(
                "SELECT partition FROM book_account WHERE account_id = $1",
                &[&unknown],
            )
            .unwrap()
            .map(|row| row.get(0))
    });
    assert_eq!(none, None, "never placed by its ID instead");
    blocking(move || drop(store));
}

/// The conductor silent: an account's first act refused UNAVAILABLE with a
/// retry-after, and nothing placed.
#[tokio::test(flavor = "multi_thread")]
async fn a_first_act_waits_for_the_conductor_and_never_places_by_id() {
    let (url, made) = blocking(isolated);
    blocking(move || drop(made));
    let store = blocking(|| {
        let store = PostgresStore::connect(&url, 4).unwrap().with_partitions(2);
        store.make_partitions(2).unwrap();
        Arc::new(store)
    });
    let bus = Arc::new(Bus::single(
        "bor-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let writer: Arc<dyn Store> = Arc::new(meridian_bor::writer::Writer::new(
        store.clone() as Arc<dyn Store>,
        bus.clone(),
    ));
    serve(bus.clone(), writer, bus.clock());
    resolving_equities(&bus);
    let account = unique("ACC");
    let refused = send(
        &bus,
        RECORD_OPENING_BALANCE,
        &opening(&account, "INS-AAPL"),
        Some(PERSON),
    )
    .await
    .unwrap_err();
    assert!(refused.contains("unavailable"), "{refused}");
    assert_eq!(
        meridian_bus::refusal_retry_after(&refused[refused.find("[refusal-status").unwrap()..]),
        Some(5),
        "{refused}"
    );
    let placed: i64 = blocking(|| {
        postgres::Client::connect(&url, postgres::NoTls)
            .unwrap()
            .query_one(
                "SELECT count(*) FROM book_account WHERE account_id NOT LIKE '~%'",
                &[],
            )
            .unwrap()
            .get(0)
    });
    assert_eq!(placed, 0);
    blocking(move || drop(store));
}

/// Step 3d (the ownership pre-check, with several partitions): a member
/// routes a write by where the record holds its account, learnt where this
/// member has not seen it -- never the partition an account starts in -- and
/// an account not placed yet to no group, so the member it reaches places it
/// and hands it on where it landed.
#[test]
fn the_pre_check_routes_by_the_record_and_leaves_an_unplaced_account_unrouted() {
    use meridian_changes::groups::Groups;
    let (url, migrated) = isolated();
    migrated.make_partitions(4).unwrap();
    drop(migrated);
    let placing = PostgresStore::connect(&url, 2).unwrap().with_partitions(4);
    // An account whose ID lands it in a partition group 1 owns.
    let groups = Groups::new(0, 2).unwrap();
    let account = (0..)
        .map(|n| format!("ACC-ROUTE-{n}"))
        .find(|account| groups.owner(&meridian_bor::store::placement_of(account, 4)) == 1)
        .unwrap();
    let landed = placing.place_first(&account, &account).unwrap();
    assert_eq!(groups.owner(&landed), 1);
    drop(placing);
    // Another member of group 0, its cache cold.
    let member = PostgresStore::connect(&url, 2)
        .unwrap()
        .with_groups(groups)
        .with_partitions(4);
    assert_eq!(member.writer_group_of(&account), Some(1), "by the record");
    assert_eq!(
        member.writer_group_of("ACC-NEVER-PLACED"),
        None,
        "no group's yet"
    );
    assert_eq!(
        member.writer_group_of(meridian_bor::store::CONTROL_ACCOUNT),
        Some(0),
        "the book's own keys where they start"
    );
}

/// Step 3d (the resilience spec's section 6.4): the book on two databases,
/// two writer groups and four partitions. An account is placed, journalled
/// and read in its own group's database alone; a member of the other
/// group finds it there and hands its acts on, never placing it again in
/// its own; a read of every account fans out over both; and database 1's
/// takeover is owed to database 0's `~control`, keyed with its database.
#[tokio::test(flavor = "multi_thread")]
async fn two_groups_keep_their_partitions_in_two_databases() {
    use meridian_changes::groups::Groups;
    let urls: Vec<Scratch> = blocking(|| {
        (0..2)
            .map(|_| {
                let (url, migrated) = isolated();
                migrated.make_partitions(4).unwrap();
                url
            })
            .collect()
    });
    let spread = |group: u32| {
        let groups = Groups::new(group, 2).unwrap();
        let stores = urls
            .iter()
            .map(|url| {
                Arc::new(
                    PostgresStore::connect(url, 4)
                        .unwrap()
                        .with_groups(groups)
                        .with_partitions(4),
                )
            })
            .collect();
        Arc::new(meridian_bor::spread::Spread::new(stores, groups))
    };
    let (zero, one) = blocking(|| (spread(0), spread(1)));
    let groups = Groups::new(0, 2).unwrap();
    let account = (0..)
        .map(|n| unique(&format!("ACC-TWO-{n}")))
        .find(|account| groups.owner(&meridian_bor::store::placement_of(account, 4)) == 1)
        .unwrap();
    // Placed by a member of group 0, in group 1's database.
    let landed = blocking(|| zero.place_first(&account, &account)).unwrap();
    assert_eq!(groups.owner(&landed), 1);
    let fresh = |d: usize| PostgresStore::connect(&urls[d], 1).unwrap();
    blocking(|| {
        assert_eq!(fresh(1).placement_held(&account), Some(landed.clone()));
        assert_eq!(
            fresh(0).placement_held(&account),
            None,
            "never in group 0's database"
        );
    });
    // Journalled by group 1, in its database.
    let bus = Arc::new(Bus::single(
        "operations-test-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    serve(bus.clone(), one.clone(), bus.clock());
    resolving_equities(&bus);
    let instrument = unique("INS-TWO");
    open(&bus, &account, &instrument).await;
    blocking(|| {
        assert_eq!(fresh(1).journal(&account).unwrap().len(), 1);
        assert!(fresh(0).journal(&account).unwrap().is_empty());
        // Read through either member, from its database.
        assert_eq!(zero.journal(&account).unwrap().len(), 1);
        assert_eq!(zero.writer_group_of(&account), Some(1));
        assert!(!zero.writes(&account));
        // A read of every account fans out over both databases.
        let read = meridian_bor::store::PositionsRead {
            scope: meridian_bor::store::Scope::Everything,
            account_id: String::new(),
            since: None,
            business_date: String::new(),
            at: None,
            limit: 10,
            cursor: String::new(),
        };
        let page = zero.positions(&read).unwrap();
        assert!(page
            .records
            .iter()
            .any(|position| position.account_id == account));
        // An act through group 0 is handed on, never placed again here.
        let handed = zero.act(&account, "", "k", b"x", &mut |_, _| {
            Err(meridian_bor::store::StoreError::Invalid(
                "never decided here".into(),
            ))
        });
        assert!(
            matches!(
                handed,
                Err(meridian_bor::store::StoreError::NotOwned { group: 1, .. })
            ),
            "{:?}",
            handed.err()
        );
        assert_eq!(fresh(0).placement_held(&account), None);
        // Database 1's takeover, owed to database 0's ~control.
        one.take_over(&meridian_changes::TookOver::unnamed("a test's takeover"))
            .unwrap();
        let owed = zero.control_records_owed().unwrap();
        assert!(
            owed.iter()
                .any(|owed| owed.key.starts_with("control-record:took-over:1/")),
            "{:?}",
            owed.iter().map(|o| &o.key).collect::<Vec<_>>()
        );
    });
    // Their connections closed off the runtime's workers.
    blocking(move || drop((zero, one, bus)));
}

/// An account moved to a partition another database holds (step 3d; the
/// resilience spec's sections 6.4 and 6.8): the close in its own database,
/// the open handed to the new partition's group, which copies everything
/// the old database holds of the account first; the account replays the
/// same across both, its positions read once, from its new database, the
/// old one keeping its entries and letting go of the rest; each database's
/// chain scrubs clean; and its next act is the new partition's.
#[tokio::test(flavor = "multi_thread")]
async fn an_account_moved_between_databases_is_copied_and_replays_the_same() {
    use meridian_changes::groups::Groups;
    let urls: Vec<Scratch> = blocking(|| {
        (0..2)
            .map(|_| {
                let (url, migrated) = isolated();
                migrated.make_partitions(4).unwrap();
                url
            })
            .collect()
    });
    let spread = |group: u32| {
        let groups = Groups::new(group, 2).unwrap();
        let stores = urls
            .iter()
            .map(|url| {
                Arc::new(
                    PostgresStore::connect(url, 4)
                        .unwrap()
                        .with_groups(groups)
                        .with_partitions(4),
                )
            })
            .collect();
        Arc::new(meridian_bor::spread::Spread::new(stores, groups))
    };
    let (zero, one) = blocking(|| (spread(0), spread(1)));
    let groups = Groups::new(0, 2).unwrap();
    let account = (0..)
        .map(|n| unique(&format!("ACC-CROSS-{n}")))
        .find(|account| groups.owner(&meridian_bor::store::placement_of(account, 4)) == 1)
        .unwrap();
    let from = blocking(|| one.place_first(&account, &account)).unwrap();
    let to = (0..4)
        .map(meridian_bor::store::partition_named)
        .find(|partition| groups.owner(partition) == 0)
        .unwrap();
    let bus = |name: &str, store: Arc<meridian_bor::spread::Spread>| {
        let bus = Arc::new(Bus::single(
            name,
            Arc::new(MemoryBackend::new()),
            Arc::new(meridian_clock::SystemClock),
        ));
        serve(bus.clone(), store, bus.clock());
        resolving_equities(&bus);
        bus
    };
    let (bus_zero, bus_one) = (bus("cross-0", zero.clone()), bus("cross-1", one.clone()));
    let instrument = unique("INS-CROSS");
    open(&bus_one, &account, &instrument).await;
    let request = MoveAccountRequest {
        account_id: account.clone(),
        target_partition: to.clone(),
        reason: "group 1 is busy".into(),
    };
    // The close, by group 1 in its database; its open is group 0's.
    let handed = send(
        &bus_one,
        meridian_bor::moves::MOVE_ACCOUNT,
        &request,
        Some(PERSON),
    )
    .await;
    assert!(handed.is_err(), "the open is the new partition's group's");
    let fresh = |d: usize| PostgresStore::connect(&urls[d], 1).unwrap();
    blocking(|| {
        assert_eq!(fresh(1).account_row(&account).unwrap(), Some(to.clone()));
        assert_eq!(fresh(0).account_row(&account).unwrap(), None);
    });
    // Handed to group 0: copied into its database, and opened there.
    let moved = MoveAccountReply::decode(
        &send(
            &bus_zero,
            meridian_bor::moves::MOVE_ACCOUNT,
            &request,
            Some(PERSON),
        )
        .await
        .unwrap()[..],
    )
    .unwrap()
    .moved
    .unwrap();
    assert_eq!(
        (moved.from_partition.as_str(), moved.to_partition.as_str()),
        (from.as_str(), to.as_str())
    );
    assert!(moved.opening_sequence > 0);
    blocking(|| {
        // Read through either member: each entry once, in the order written.
        let journal = zero.journal(&account).unwrap();
        assert_eq!(journal.len(), 3, "the opening balance, the close, the open");
        assert_eq!(
            journal
                .iter()
                .map(|e| e.partition.as_str())
                .collect::<Vec<_>>(),
            [from.as_str(), from.as_str(), to.as_str()]
        );
        assert_eq!(one.journal(&account).unwrap(), journal);
        let mut replayed = meridian_bor::Book::replay(&account, &from, &journal).unwrap();
        let mut held = zero.book(&account).unwrap();
        replayed.release_bodies();
        held.release_bodies();
        assert_eq!(replayed, held, "it replays the same across both databases");
        // Its position from its new database alone; the old one keeps its
        // entries and nothing else.
        let read = PositionsRead {
            scope: Scope::Everything,
            account_id: account.clone(),
            since: None,
            business_date: String::new(),
            at: None,
            limit: 10,
            cursor: String::new(),
        };
        let mine = |store: &PostgresStore| store.positions(&read).unwrap().records;
        assert_eq!(mine(&fresh(0)).len(), 1);
        assert!(mine(&fresh(1)).is_empty());
        assert_eq!(
            read_position(&zero.positions(&read).unwrap().records[0]),
            "12.5"
        );
        assert_eq!(fresh(1).account_row(&account).unwrap(), None);
        assert_eq!(fresh(1).journal(&account).unwrap().len(), 2);
        assert_eq!(
            zero.accounts()
                .unwrap()
                .iter()
                .filter(|a| **a == account)
                .count(),
            1
        );
        // Each database's chain scrubs clean: the copied entries are the
        // old database's chain, never the new one's.
        for d in 0..2 {
            let scrubbed = fresh(d).scrub().unwrap();
            assert!(
                scrubbed.findings.is_empty(),
                "database {d}: {:?}",
                scrubbed.findings
            );
        }
    });
    // Asked again: answered as it opened, settled again, nothing written.
    let again = MoveAccountReply::decode(
        &send(
            &bus_zero,
            meridian_bor::moves::MOVE_ACCOUNT,
            &request,
            Some(PERSON),
        )
        .await
        .unwrap()[..],
    )
    .unwrap()
    .moved
    .unwrap();
    assert_eq!(again, moved);
    // Its next act is the new partition's, in its new database.
    let next = a_break(&account);
    send(&bus_zero, RECORD_BREAK, &next, Some(PERSON))
        .await
        .unwrap();
    blocking(|| {
        let journal = zero.journal(&account).unwrap();
        assert_eq!(journal.len(), 4);
        assert_eq!(journal[3].partition, to);
        assert_eq!(fresh(0).journal(&account).unwrap().len(), 4);
    });
    blocking(move || drop((zero, one, bus_zero, bus_one)));
}

/// An account's move between databases compares its copy with the source
/// before the close (step 3d; section 6.8, "the target compares its digest
/// at N with the source's"): a warm copies its entries, and only its
/// entries, so it is in one database until the open. A copy changed beneath
/// the target's book -- a real mismatch, no injected file -- aborts the
/// move DIGEST: nothing closed, the account where it was, and
/// PartitionMoveAborted and its IntegrityFinding, naming the changed entry,
/// journalled on `~control`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_account_whose_copy_differs_from_its_source_is_not_moved_and_its_finding_kept() {
    use meridian_changes::groups::Groups;
    let urls: Vec<Scratch> = blocking(|| {
        (0..2)
            .map(|_| {
                let (url, migrated) = isolated();
                migrated.make_partitions(4).unwrap();
                url
            })
            .collect()
    });
    let spread = |group: u32| {
        let groups = Groups::new(group, 2).unwrap();
        let stores = urls
            .iter()
            .map(|url| {
                Arc::new(
                    PostgresStore::connect(url, 4)
                        .unwrap()
                        .with_groups(groups)
                        .with_partitions(4),
                )
            })
            .collect();
        Arc::new(meridian_bor::spread::Spread::new(stores, groups))
    };
    let zero = blocking(|| spread(0));
    let groups = Groups::new(0, 2).unwrap();
    // Group 0's account, in database 0, moving to group 1's partition in
    // database 1: group 0 writes `~control` and journals the abort itself.
    let account = (0..)
        .map(|n| unique(&format!("ACC-VDIFF-{n}")))
        .find(|account| groups.owner(&meridian_bor::store::placement_of(account, 4)) == 0)
        .unwrap();
    let from = blocking(|| zero.place_first(&account, &account)).unwrap();
    let to = (0..4)
        .map(meridian_bor::store::partition_named)
        .find(|partition| groups.owner(partition) == 1)
        .unwrap();
    let bus = Arc::new(Bus::single(
        "vdiff-0",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    serve(bus.clone(), zero.clone(), bus.clock());
    resolving_equities(&bus);
    open(&bus, &account, &unique("INS-VDIFF")).await;
    let fresh = |d: usize| PostgresStore::connect(&urls[d], 1).unwrap();
    // A warm: its entries in database 1, compared equal, and nothing else
    // of it there.
    let warmed = blocking(|| zero.warm_account(&account, &to))
        .unwrap()
        .expect("across databases");
    assert_eq!(warmed.source, warmed.copy);
    assert_eq!(warmed.differs_at, None);
    let before = blocking(|| zero.journal(&account)).unwrap();
    assert_eq!(warmed.last_entry, before.last().unwrap().entry_id);
    blocking(|| {
        assert_eq!(fresh(1).account_row(&account).unwrap(), None);
        assert_eq!(fresh(1).journal(&account).unwrap().len(), before.len());
        assert_eq!(fresh(0).account_row(&account).unwrap(), Some(from.clone()));
    });
    // Its copy changed beneath database 1's book.
    let changed = before[0].entry_id.clone();
    blocking(|| {
        let mut client = postgres::Client::connect(&urls[1], postgres::NoTls).unwrap();
        client
            .batch_execute(&format!(
                "ALTER TABLE book_entry DISABLE TRIGGER book_entry_append_only;
                 UPDATE book_entry SET message_id = message_id || '-altered'
                  WHERE entry_id = '{changed}';
                 ALTER TABLE book_entry ENABLE TRIGGER book_entry_append_only;"
            ))
            .unwrap();
    });
    let request = MoveAccountRequest {
        account_id: account.clone(),
        target_partition: to.clone(),
        reason: "group 0 is busy".into(),
    };
    let refused = send(
        &bus,
        meridian_bor::moves::MOVE_ACCOUNT,
        &request,
        Some(PERSON),
    )
    .await
    .unwrap_err();
    assert!(
        refused.contains("account_id") && refused.contains("digest"),
        "{refused}"
    );
    blocking(|| {
        // Nothing closed: where it was, its journal as it was.
        assert_eq!(zero.journal(&account).unwrap(), before);
        assert_eq!(fresh(0).account_row(&account).unwrap(), Some(from.clone()));
        assert_eq!(fresh(1).account_row(&account).unwrap(), None);
        let control = zero.journal(meridian_bor::store::CONTROL_ACCOUNT).unwrap();
        let ours = |prefix: &str| {
            control
                .iter()
                .filter(|e| {
                    e.idempotency_key
                        .starts_with(&format!("control-record:{prefix}:{from}:"))
                })
                .filter_map(|e| e.body.record.clone())
                .collect::<Vec<_>>()
        };
        let aborted = ours("partition-move-aborted");
        assert_eq!(aborted.len(), 1, "one abort");
        let aborted = PartitionMoveAborted::decode(&aborted[0].1[..]).unwrap();
        assert_eq!(
            (
                aborted.account_id.as_str(),
                aborted.target_partition.as_str(),
                aborted.target_group,
                aborted.cause
            ),
            (
                account.as_str(),
                to.as_str(),
                1,
                MoveAbortCause::Digest as i32
            )
        );
        let found = ours("finding:move");
        assert_eq!(found.len(), 1, "one finding");
        let found = IntegrityFinding::decode(&found[0].1[..]).unwrap();
        assert_eq!(found.check, IntegrityCheck::MemberDigest as i32);
        assert!(found.detail.contains(&changed), "{}", found.detail);
    });
    blocking(move || drop((zero, bus)));
}

fn read_position(position: &BookPosition) -> String {
    read(&position.trade_date_quantity)
}

/// Each writer group's partitions in the database at `url` at an epoch of
/// its own, as a deployment's are once each writer has started: neither
/// takes the other's over -- a takeover ends the sessions holding its
/// partitions' locks, by key, in every schema of the test database.
fn apart(url: &str) {
    let mut admin = postgres::Client::connect(url, postgres::NoTls).unwrap();
    for n in 0..8 {
        let partition = meridian_bor::store::partition_named(n);
        let epoch = 10 + 10 * meridian_changes::groups::owner(&partition, 2) as i64;
        // Pinned to a group already: that group's epoch.
        admin
            .execute(
                "UPDATE book_partition_head
                    SET epoch = coalesce(10 + 10 * writer_group::bigint, $2)
                  WHERE partition = $1",
                &[&partition, &epoch],
            )
            .unwrap();
    }
    admin
        .execute(
            "UPDATE book_partition_head SET epoch = 10 WHERE partition = 'control'",
            &[],
        )
        .unwrap();
}

/// Two members of two writer groups over one database, each serving the
/// book and handing the other's commands on (step 3d), and its admin
/// connection.
type TwoMembers = (
    Arc<GroupsBroker>,
    [(Arc<Bus>, Arc<PostgresStore>); 2],
    meridian_bor::partition_moves::Mover,
);

fn two_members(url: &str) -> TwoMembers {
    two_members_at(url, 8)
}

/// [`two_members`] whose members place accounts in `partitions`
/// partitions, the book's heads made for 8 as they start: a raised
/// `bor.partitions` whose heads are made after.
fn two_members_at(url: &str, partitions: u32) -> TwoMembers {
    let broker = Arc::new(GroupsBroker {
        inner: MemoryBackend::new(),
        groups: Default::default(),
        handed: AtomicU64::new(0),
    });
    // Each group's partitions at an epoch of its own, as a deployment's
    // are once each writer has started: neither takes the other's over --
    // a takeover ends the sessions holding its partitions' locks, by key,
    // in every schema of the test database.
    PostgresStore::connect(url, 1)
        .unwrap()
        .make_partitions(8)
        .unwrap();
    apart(url);
    let (zero, one) = (
        group_member(url, &broker, 0, 2, partitions),
        group_member(url, &broker, 1, 2, partitions),
    );
    // Each writer started: its group's epoch its own (step 3d), so a move
    // to either finds its target ready.
    for (_, store) in [&zero, &one] {
        Store::adopt(store.as_ref()).unwrap();
    }
    let mover = meridian_bor::partition_moves::Mover::new(
        vec![Arc::clone(&zero.1)],
        zero.1.groups(),
        meridian_bor::partition_moves::Settings {
            switch: std::time::Duration::from_secs(2),
            ..Default::default()
        },
        Arc::new(meridian_clock::SystemClock),
    );
    (broker, [zero, one], mover)
}

/// A member of writer group `group` of `count`, serving the book over the
/// database at `url` and handing the other groups' commands on through
/// `broker`, its accounts placed in `partitions` partitions; the book's
/// heads made for 8 and named their owners where none is named yet. Its
/// writer has not started: nothing adopted or taken over.
fn group_member(
    url: &str,
    broker: &Arc<GroupsBroker>,
    group: u32,
    count: u32,
    partitions: u32,
) -> (Arc<Bus>, Arc<PostgresStore>) {
    use meridian_bor::writer::{is_command, Writer, INTERNAL_COMMAND};
    use meridian_changes::groups::Groups;
    let store = Arc::new(
        PostgresStore::connect(url, 4)
            .unwrap()
            .with_groups(Groups::new(group, count).unwrap())
            .with_partitions(partitions),
    );
    store.make_partitions(8).unwrap();
    store.pin_owners().unwrap();
    let bus = Arc::new(Bus::single(
        "bor-moves-test",
        Arc::clone(broker) as Arc<dyn meridian_bus::Backend>,
        Arc::new(meridian_clock::SystemClock),
    ));
    bus.hand_writes_on(INTERNAL_COMMAND, group, is_command);
    let writer: Arc<dyn Store> = Arc::new(Writer::new(
        Arc::clone(&store) as Arc<dyn Store>,
        Arc::clone(&bus),
    ));
    serve(bus.clone(), writer, bus.clock());
    resolving_equities(&bus);
    (bus, store)
}

/// What the book's record breaks of "every epoch above the first raised by
/// a recorded takeover, the takeovers one per epoch with none skipped"
/// (ruling 1105; e2e/failover/run.py `properties`), in the database at
/// `url`: each entry committed, and each head held, at an epoch no
/// takeover recorded, and a takeover count short of the epochs it spans.
fn unrecorded_epochs(url: &str) -> Vec<String> {
    let mut admin = postgres::Client::connect(url, postgres::NoTls).unwrap();
    let mut broken = Vec::new();
    for (what, table) in [
        ("entries committed", "book_entry"),
        ("heads held", "book_partition_head"),
    ] {
        let rows = admin
            .query(
                &format!(
                    "SELECT epoch, count(*) FROM {table}
                      WHERE epoch > 1 AND epoch NOT IN (SELECT epoch FROM book_writer_took_over)
                      GROUP BY epoch ORDER BY epoch"
                ),
                &[],
            )
            .unwrap();
        for row in rows {
            broken.push(format!(
                "{} {what} at epoch {}, which no takeover recorded",
                row.get::<_, i64>(1),
                row.get::<_, i64>(0)
            ));
        }
    }
    let row = admin
        .query_one(
            "SELECT count(*), coalesce(max(epoch) - min(epoch) + 1, 0) FROM book_writer_took_over",
            &[],
        )
        .unwrap();
    let (recorded, spanned): (i64, i64) = (row.get(0), row.get(1));
    if recorded != spanned {
        broken.push(format!(
            "{recorded} takeovers recorded over {spanned} epochs"
        ));
    }
    broken
}

/// A partition moved to a writer group whose writer has not taken over in
/// its database commits nothing at an epoch no takeover recorded (CI
/// 43820357549, reconfigure, "groups raised": "book: 83 commit(s) at an
/// epoch no takeover recorded", then "7 takeovers recorded over 8 epochs").
/// The count raised from one group to two: group 0's writer starts again
/// and rebalances before group 1's writer has started. A move never makes
/// an epoch: group 1 is not ready, so its partitions stay group 0's; once
/// its writer has taken over -- recorded, with its own member and why --
/// the next sweep moves them under that epoch, and group 1 writes there.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_move_to_a_group_whose_writer_has_not_taken_over_commits_nothing_at_an_unrecorded_epoch()
{
    use meridian_bor::partition_moves::{rebalance, Ended};
    use meridian_changes::groups::owner;
    let (url, migrated) = blocking(isolated);
    let (broker, zero, zero_store, mover) = blocking(|| raised_to_two(&url, 15));
    let wanted: Vec<String> = (0..8)
        .map(meridian_bor::store::partition_named)
        .filter(|p| owner(p, 2) == 1)
        .collect();
    assert!(wanted.contains(&"P4".to_string()));
    // The sweep at group 0's start, before group 1's writer has started.
    let early = blocking(|| rebalance(&zero, zero_store.as_ref(), &mover));
    let unready: Vec<(String, Option<u32>)> = blocking(|| {
        wanted
            .iter()
            .map(|p| (p.clone(), zero_store.tip(p).unwrap().unwrap().writer_group))
            .collect()
    });
    // Group 1's writer starts, and the next sweep runs.
    let (one, one_store) = blocking(|| group_member(&url, &broker, 1, 2, 8));
    let epoch = blocking(|| Store::adopt(one_store.as_ref())).unwrap();
    let ended = blocking(|| rebalance(&zero, zero_store.as_ref(), &mover));
    // Group 1 writes into one of its partitions.
    let account = (0..)
        .map(|n| unique(&format!("ACC-UNREC-{n}")))
        .find(|a| meridian_bor::store::placement_of(a, 8) == "P4")
        .unwrap();
    blocking(|| one_store.place_first(&account, &account)).unwrap();
    open(&one, &account, "INS-AAPL").await;
    blocking(|| {
        // Nothing committed or held at an epoch no takeover recorded.
        assert_eq!(unrecorded_epochs(&url), Vec::<String>::new());
        // Nothing moved before group 1's writer took over.
        assert!(
            early.is_empty(),
            "moved before group 1's writer took over: {early:?}"
        );
        assert!(
            unready.iter().all(|(_, group)| *group == Some(0)),
            "{unready:?}"
        );
        // Then moved, under the epoch its takeover recorded, and written.
        let moved: Vec<String> = ended
            .iter()
            .map(|e| match e {
                Ended::Moved(m) => {
                    assert_eq!((m.from_group, m.to_group, m.epoch), (0, 1, epoch));
                    m.partition.clone()
                }
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(moved, wanted);
        let written = numbered(&url, "P4");
        assert!(
            written.iter().any(|(_, _, e)| *e as u64 == epoch),
            "group 1 wrote P4 under its epoch {epoch}: {written:?}"
        );
    });
    blocking(move || drop((zero, one, zero_store, one_store, mover, migrated)));
}

/// Writer groups lowered from two to one in tolerant -- no lease taken, no
/// new database timeline raising every epoch -- and the one writer left
/// writes what group 1 held (the reconfigure check's "groups lowered";
/// c5r2's second finding, the book's side): at its start it takes over
/// every partition held at an epoch not the control partition's, as several
/// groups' adopt does, recorded with why, and started again it adopts that
/// epoch. A store a tool opens at one group, its epoch adopted at its first
/// act, takes nothing over: group 1's writer, still writing, is not fenced
/// by it (ruled 2026-11-18). The one writer's takeover is: group 1's
/// writer, not stopped yet, sees it in its own group's row (review c5r3,
/// M1).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_count_lowered_to_one_writes_what_group_one_held_on_the_same_timeline() {
    use meridian_bor::partition_moves::rebalance;
    use meridian_bor::store::placement_of;
    use meridian_changes::groups::{owner, Groups};
    let (url, migrated) = blocking(isolated);
    let (broker, zero, zero_store, mover) = blocking(|| raised_to_two(&url, 15));
    let (one, one_store) = blocking(|| group_member(&url, &broker, 1, 2, 8));
    let held = blocking(|| Store::adopt(one_store.as_ref())).unwrap();
    blocking(|| rebalance(&zero, zero_store.as_ref(), &mover));
    let in_p4 = |tag: &str| {
        (0..)
            .map(|n| unique(&format!("ACC-LOWERED-{tag}-{n}")))
            .find(|a| placement_of(a, 8) == "P4")
            .unwrap()
    };
    let (first, second, third) = (in_p4("one"), in_p4("still"), in_p4("alone"));
    blocking(|| one_store.place_first(&first, &first)).unwrap();
    open(&one, &first, "INS-AAPL").await;
    blocking(|| {
        assert_eq!(zero_store.tip("P4").unwrap().unwrap().writer_group, Some(1));
    });
    // A tool's store at one group, adopting at its first act: group 1's
    // writer goes on writing.
    let elsewhere = (0..)
        .map(|n| unique(&format!("ACC-LOWERED-TOOL-{n}")))
        .find(|a| owner(&placement_of(a, 8), 2) == 0)
        .unwrap();
    blocking(|| {
        let tool = PostgresStore::connect(&url, 2).unwrap().with_partitions(8);
        let _ = Store::act(&tool, &elsewhere, "", "k", b"x", &mut |_, _| {
            Err(meridian_bor::store::StoreError::Invalid(
                "a tool's act, never decided".into(),
            ))
        });
    });
    blocking(|| one_store.place_first(&second, &second)).unwrap();
    let still = send(
        &one,
        RECORD_OPENING_BALANCE,
        &opening(&second, "INS-AAPL"),
        Some(PERSON),
    )
    .await;
    assert!(
        still.is_ok(),
        "a tool's store fenced group 1's writer: {still:?}"
    );
    // Lowered: the first group's writer alone, started again, no lease.
    let lowered = Arc::new(GroupsBroker {
        inner: MemoryBackend::new(),
        groups: Default::default(),
        handed: AtomicU64::new(0),
    });
    let (alone, alone_store) = blocking(|| group_member(&url, &lowered, 0, 1, 8));
    let epoch = blocking(|| Store::adopt(alone_store.as_ref())).unwrap();
    blocking(|| alone_store.place_first(&third, &third)).unwrap();
    let recorded = send(
        &alone,
        RECORD_OPENING_BALANCE,
        &opening(&third, "INS-AAPL"),
        Some(PERSON),
    )
    .await;
    assert!(
        recorded.is_ok(),
        "the one writer left was refused what group 1 held: {recorded:?}"
    );
    blocking(|| {
        // Group 1's writer, not stopped yet, sees the takeover in its own
        // group's row (review c5r3, M1): its epoch's watch steps it down.
        let (row, mine) = (
            Store::head_epoch(one_store.as_ref()).unwrap(),
            Store::epoch(one_store.as_ref()),
        );
        assert!(
            row > mine,
            "group 1's writer does not see the takeover: its row {row} and its epoch {mine}"
        );
        assert!(epoch > held, "taken over past group 1's {held}: {epoch}");
        assert_eq!(unrecorded_epochs(&url), Vec::<String>::new());
        let mut admin = postgres::Client::connect(&url, postgres::NoTls).unwrap();
        let why: String = admin
            .query_one(
                "SELECT why FROM book_writer_took_over WHERE epoch = $1",
                &[&(epoch as i64)],
            )
            .unwrap()
            .get(0);
        assert_eq!(why, meridian_changes::groups::ASSIGNED);
        // Started again with every partition its own: adopted, not raised
        // again.
        let again = PostgresStore::connect(&url, 2)
            .unwrap()
            .with_groups(Groups::new(0, 1).unwrap())
            .with_partitions(8);
        assert_eq!(Store::adopt(&again).unwrap(), epoch);
    });
    blocking(move || {
        drop((
            zero,
            one,
            alone,
            zero_store,
            one_store,
            alone_store,
            mover,
            migrated,
        ))
    });
}

/// The book at `url` started at one writer group -- every head pinned to
/// group 0, its writer's takeover recorded -- then raised to two: group 0's
/// writer started again, its epoch adopted, and its mover, whose switch is
/// bound by `switch_seconds`; group 1's writer not started.
fn raised_to_two(
    url: &str,
    switch_seconds: u64,
) -> (
    Arc<GroupsBroker>,
    Arc<Bus>,
    Arc<PostgresStore>,
    meridian_bor::partition_moves::Mover,
) {
    use meridian_bor::partition_moves::{Mover, Settings};
    use meridian_changes::groups::Groups;
    let alone = PostgresStore::connect(url, 2)
        .unwrap()
        .with_groups(Groups::new(0, 1).unwrap())
        .with_partitions(8);
    alone.make_partitions(8).unwrap();
    assert_eq!(alone.pin_owners().unwrap(), 8);
    Store::take_over(
        &alone,
        &meridian_changes::TookOver::unnamed(meridian_changes::LEASE_FREE),
    )
    .unwrap();
    let broker = Arc::new(GroupsBroker {
        inner: MemoryBackend::new(),
        groups: Default::default(),
        handed: AtomicU64::new(0),
    });
    let (zero, zero_store) = group_member(url, &broker, 0, 2, 8);
    Store::adopt(zero_store.as_ref()).unwrap();
    // A switch's bound long enough for another test's move of the same
    // partition name: partition locks are keyed by name over every schema
    // of the test database.
    let mover = Mover::new(
        vec![Arc::clone(&zero_store)],
        zero_store.groups(),
        Settings {
            switch: std::time::Duration::from_secs(switch_seconds),
            ..Default::default()
        },
        Arc::new(meridian_clock::SystemClock),
    );
    (broker, zero, zero_store, mover)
}

/// An epoch an earlier release's move made for a writer group with no
/// takeover recorded (7bbbf608 and before: CI 43820357549) is recorded as
/// its writer's own when that writer adopts it, before anything commits
/// under it (review t7, M1): a database upgraded with such a row keeps
/// every epoch above the first raised by a recorded takeover, named by the
/// member that adopted it.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn an_epoch_an_earlier_move_made_with_no_takeover_is_recorded_before_anything_commits_under_it(
) {
    use meridian_bor::partition_moves::{Asked, Ended};
    meridian_changes::set_member("bor-t7-member");
    let (url, migrated) = blocking(isolated);
    let (broker, zero, zero_store, mover) = blocking(|| raised_to_two(&url, 15));
    // Group 1's row as an earlier release's move made it: one past every
    // epoch, no takeover recorded.
    let made: i64 = blocking(|| {
        let mut admin = postgres::Client::connect(&url, postgres::NoTls).unwrap();
        admin
            .query_one(
                "INSERT INTO book_writer_group (writer_group, epoch, origin)
                 SELECT 1, max(epoch) + 1, '' FROM book_partition_head RETURNING epoch",
                &[],
            )
            .unwrap()
            .get(0)
    });
    // P4 moved to group 1 under it, then group 1's writer starts and writes.
    let ended = blocking(|| {
        assert!(mover.begin("P4"));
        mover.run("P4", 1, &Asked::default())
    })
    .unwrap();
    assert!(
        matches!(&ended, Ended::Moved(m) if m.epoch == made as u64),
        "{ended:?}"
    );
    let (one, one_store) = blocking(|| group_member(&url, &broker, 1, 2, 8));
    let epoch = blocking(|| Store::adopt(one_store.as_ref())).unwrap();
    let account = (0..)
        .map(|n| unique(&format!("ACC-MINTED-{n}")))
        .find(|a| meridian_bor::store::placement_of(a, 8) == "P4")
        .unwrap();
    blocking(|| one_store.place_first(&account, &account)).unwrap();
    open(&one, &account, "INS-AAPL").await;
    blocking(|| {
        assert_eq!(unrecorded_epochs(&url), Vec::<String>::new());
        assert_eq!(epoch, made as u64, "adopted, not raised again");
        let mut admin = postgres::Client::connect(&url, postgres::NoTls).unwrap();
        let row = admin
            .query_one(
                "SELECT member, partitions FROM book_writer_took_over WHERE epoch = $1",
                &[&made],
            )
            .unwrap();
        let (member, partitions): (String, Vec<String>) = (row.get(0), row.get(1));
        assert_eq!(
            member, "bor-t7-member",
            "named by the member that adopted it"
        );
        assert!(partitions.contains(&"P4".to_string()), "{partitions:?}");
    });
    blocking(move || drop((zero, one, zero_store, one_store, mover, migrated)));
}

/// A move by hand to a writer group whose writer has not taken over in its
/// database waits at the gate, and aborts for lag past moves.switchSeconds
/// with nothing held: the source still owns the partition, and nothing is
/// at an epoch no takeover recorded (review t7, L1). Its writer taking over
/// while the gate waits, the move switches under that takeover's epoch.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_move_by_hand_to_a_group_not_ready_waits_then_aborts_for_lag_or_moves_once_it_takes_over()
{
    use meridian_bor::partition_moves::{Asked, Ended};
    use meridian_changes::groups::Groups;
    let (url, migrated) = blocking(isolated);
    let (_broker, zero, zero_store, mover) = blocking(|| raised_to_two(&url, 2));
    let asked = Asked {
        by_hand: true,
        moved_by: PERSON.into(),
        reason: "group 1 is to write P4".into(),
    };
    let began = std::time::Instant::now();
    let ended = blocking(|| {
        assert!(mover.begin("P4"));
        mover.run("P4", 1, &asked)
    })
    .unwrap();
    let waited = began.elapsed();
    match &ended {
        Ended::Aborted(aborted, None) => {
            assert_eq!(aborted.cause, MoveAbortCause::Lag as i32, "{aborted:?}")
        }
        other => panic!("{other:?}"),
    }
    assert!(waited >= std::time::Duration::from_secs(2), "{waited:?}");
    blocking(|| {
        assert_eq!(zero_store.tip("P4").unwrap().unwrap().writer_group, Some(0));
        assert_eq!(unrecorded_epochs(&url), Vec::<String>::new());
    });
    // Group 1's writer takes over while the gate waits: moved under its
    // epoch.
    let taking = {
        // Its text: a thread a failed test leaves running never holds the
        // Scratch past the test (review c5p L2).
        let url = url.to_string();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let one = PostgresStore::connect(&url, 2)
                .unwrap()
                .with_groups(Groups::new(1, 2).unwrap())
                .with_partitions(8);
            Store::adopt(&one).unwrap()
        })
    };
    let ended = blocking(|| {
        assert!(mover.begin("P4"));
        mover.run("P4", 1, &asked)
    })
    .unwrap();
    let epoch = taking.join().unwrap();
    assert!(
        matches!(&ended, Ended::Moved(m) if m.epoch == epoch && m.to_group == 1),
        "{ended:?}"
    );
    blocking(|| {
        assert_eq!(zero_store.tip("P4").unwrap().unwrap().writer_group, Some(1));
        assert_eq!(unrecorded_epochs(&url), Vec::<String>::new());
    });
    blocking(move || drop((zero, zero_store, mover, migrated)));
}

/// Writes to `accounts` through `bus` until `stop`: each answered, never
/// refused (section 15: nothing refused inside the hold). Says how many.
fn writing(
    bus: Arc<Bus>,
    accounts: Vec<String>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> tokio::task::JoinHandle<(u64, Vec<String>)> {
    tokio::spawn(async move {
        let (mut written, mut refused) = (0, Vec::new());
        let mut n = 0u64;
        while !stop.load(Ordering::SeqCst) {
            n += 1;
            let account = &accounts[(n as usize) % accounts.len()];
            let request = SetAccountAttributeRequest {
                account_id: account.clone(),
                attribute: Some(
                    set_account_attribute_request::Attribute::WaitsForCompliance(
                        n.is_multiple_of(2),
                    ),
                ),
                reason: format!("write {n}"),
            };
            match send(&bus, SET_ACCOUNT_ATTRIBUTE, &request, Some(PERSON)).await {
                Ok(_) => written += 1,
                Err(failed) => refused.push(failed),
            }
        }
        (written, refused)
    })
}

/// `partition`'s entries in this database, each numbered on from the one
/// before, never a hole: (first, last, epoch) each.
fn numbered(url: &str, partition: &str) -> Vec<(i64, i64, i64)> {
    let mut admin = postgres::Client::connect(url, postgres::NoTls).unwrap();
    let rows: Vec<(i64, i64, i64)> = admin
        .query(
            "SELECT first_sequence, last_sequence, epoch FROM book_entry
              WHERE partition = $1 ORDER BY first_sequence",
            &[&partition],
        )
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect();
    for pair in rows.windows(2) {
        assert_eq!(pair[1].0, pair[0].1 + 1, "{partition}: a hole at {pair:?}");
    }
    rows
}

/// A book partition moved to another writer group under write load, within
/// one database (step 3d; section 6.8; fixtures/book/move-book-partition.
/// yaml): the switch holds the source's next batch, the held writes drain to
/// the target, none refused; the partition's chain runs on unbroken across
/// the switch, the target's epoch from it on; the source's writer is never
/// fenced; a read naming a watermark taken before the switch is answered
/// after it; and the move's record is journalled on `~control`.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_book_partition_moved_under_load_keeps_its_chain_and_its_read_after() {
    use meridian_bor::partition_moves::{journal, Asked, Ended};
    use meridian_pb::v1::{PartitionSequence, Watermark};
    let (url, migrated) = blocking(isolated);
    let (_broker, [(zero, zero_store), (one, one_store)], mover) = blocking(|| two_members(&url));
    // P4 is group 1's at two groups -- a partition no takeover test's
    // lock names -- and accounts placed there.
    assert_eq!(meridian_changes::groups::owner("P4", 2), 1);
    let accounts: Vec<String> = (0..)
        .map(|n| unique(&format!("ACC-PMOVE-{n}")))
        .filter(|a| meridian_bor::store::placement_of(a, 8) == "P4")
        .take(4)
        .collect();
    for account in &accounts {
        blocking(|| one_store.place_first(account, account)).unwrap();
        open(&one, account, "INS-AAPL").await;
    }
    let before = blocking(|| Store::heads(one_store.as_ref())).unwrap()["P4"];
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let load = [
        writing(one.clone(), accounts.clone(), stop.clone()),
        writing(zero.clone(), accounts.clone(), stop.clone()),
    ];
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let request = MovePartitionRequest {
        partition: "P4".into(),
        target_group: 0,
        reason: "spread the book's partitions over both writer groups".into(),
    };
    let ended = blocking(|| {
        assert_eq!(mover.check(&request).unwrap(), 1);
        assert!(mover.begin("P4"));
        // Asked again while under way: refused, naming the partition.
        let again = mover.check(&request).unwrap_err();
        assert!(again.on_the_bus().contains("partition"), "{again}");
        mover.run(
            "P4",
            0,
            &Asked {
                by_hand: true,
                moved_by: PERSON.into(),
                reason: request.reason.clone(),
            },
        )
    })
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    stop.store(true, Ordering::SeqCst);
    let mut written = 0;
    for task in load {
        let (count, refused) = task.await.unwrap();
        assert!(refused.is_empty(), "refused inside the hold: {refused:?}");
        written += count;
    }
    assert!(written > 10, "{written} written under the move");
    let Ended::Moved(moved) = &ended else {
        panic!("{ended:?}");
    };
    assert_eq!((moved.from_group, moved.to_group), (1, 0));
    assert!(moved.by_hand && moved.epoch > 0);
    blocking(|| {
        let tip = zero_store.tip("P4").unwrap().unwrap();
        assert_eq!(tip.writer_group, Some(0));
        assert_eq!(tip.epoch, moved.epoch);
        // Unbroken: no hole, the old epoch up to the switch, the new after.
        let rows = numbered(&url, "P4");
        let switched = rows
            .iter()
            .position(|(_, _, epoch)| *epoch as u64 == moved.epoch)
            .expect("written by the target after the switch");
        assert!(rows[switched..]
            .iter()
            .all(|(_, _, epoch)| *epoch as u64 == moved.epoch));
        assert!(switched > 0, "written by the source before it");
        assert!(zero_store.scrub().unwrap().findings.is_empty());
        // The source hands on, never fenced: its epoch as it was.
        assert_eq!(
            Store::writer_group_of(one_store.as_ref(), &accounts[0]),
            Some(0)
        );
        // A read naming the head before the switch, answered after it.
        let watermark = Watermark {
            partitions: vec![PartitionSequence {
                partition: "P4".into(),
                sequence: before,
            }],
        };
        meridian_bus::read_after::begin_read();
        let read = zero
            .reads_after()
            .answer(LIST_POSITIONS, &watermark, || {
                Store::journal(zero_store.as_ref(), &accounts[0]).map_err(|f| f.to_string())
            })
            .unwrap();
        assert!(read.len() > 1);
        // Its record in ~control, once.
        journal(&zero, zero_store.as_ref(), &ended);
        journal(&zero, zero_store.as_ref(), &ended);
        let control = zero_store
            .journal(meridian_bor::store::CONTROL_ACCOUNT)
            .unwrap();
        let records: Vec<_> = control
            .iter()
            .filter(|e| {
                e.idempotency_key
                    .starts_with("control-record:partition-moved:P4:")
            })
            .collect();
        assert_eq!(records.len(), 1);
        // Moved back is a move like any other; to its owner, refused.
        let refused = mover
            .check(&MovePartitionRequest {
                target_group: 0,
                ..request.clone()
            })
            .unwrap_err();
        assert!(refused.on_the_bus().contains("target_group"), "{refused}");
    });
    blocking(move || drop((zero, one, zero_store, one_store, migrated)));
}

/// Every cause a partition's move aborts for (section 6.8; MoveAbortCause):
/// the injected lag (MERIDIAN_MOVE_LAG_FILE) before anything is held, a
/// switch that cannot finish within moves.switchSeconds (the source's
/// batch in flight never ends), and the injected digest mismatch
/// (MERIDIAN_MOVE_DIGEST_FILE), with an IntegrityFinding. Each leaves the
/// source owning at its epoch, with no gap, and its record on ~control.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_book_partition_move_aborts_for_lag_timeout_and_digest_and_the_source_owns() {
    use meridian_bor::partition_moves::{journal, Asked, Ended, Mover, Settings};
    use meridian_domain::v1::MoveAbortCause;
    let (url, migrated) = blocking(isolated);
    let (_broker, [(zero, zero_store), (one, one_store)], _) = blocking(|| two_members(&url));
    let account = (0..)
        .map(|n| unique(&format!("ACC-ABORT-{n}")))
        .find(|a| meridian_bor::store::placement_of(a, 8) == "P4")
        .unwrap();
    blocking(|| one_store.place_first(&account, &account)).unwrap();
    open(&one, &account, "INS-AAPL").await;
    let files = std::env::temp_dir().join(unique("bor-move-abort"));
    std::fs::create_dir_all(&files).unwrap();
    let (lag_file, digest_file) = (files.join("lag"), files.join("digest"));
    let mover = |settings: Settings| {
        Mover::new(
            vec![Arc::clone(&zero_store)],
            zero_store.groups(),
            settings,
            Arc::new(meridian_clock::SystemClock),
        )
    };
    let short = Settings {
        switch: std::time::Duration::from_secs(1),
        lag_file: Some(lag_file.clone()),
        digest_file: Some(digest_file.clone()),
        ..Default::default()
    };
    let asked = Asked {
        by_hand: true,
        moved_by: PERSON.into(),
        reason: "a test of each abort".into(),
    };
    let epoch_before = blocking(|| zero_store.tip("P4").unwrap().unwrap().epoch);
    let mut causes = Vec::new();
    // Lag: the target never within readyLagMs.
    std::fs::write(&lag_file, "60000").unwrap();
    causes.push(blocking(|| mover(short.clone()).run("P4", 0, &asked)).unwrap());
    std::fs::remove_file(&lag_file).unwrap();
    // Timeout: the source's batch in flight holds the lock past the bound.
    let in_flight = std::thread::spawn({
        // Its text: a thread a failed test leaves running never holds the
        // Scratch past the test (review c5p L2).
        let url = url.to_string();
        move || {
            let mut batch = postgres::Client::connect(&url, postgres::NoTls).unwrap();
            let key = meridian_bor::postgres::partition_lock_key("P4");
            batch
                .execute("SELECT pg_advisory_lock($1)", &[&key])
                .unwrap();
            std::thread::sleep(std::time::Duration::from_secs(3));
            batch
                .execute("SELECT pg_advisory_unlock($1)", &[&key])
                .unwrap();
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    causes.push(blocking(|| mover(short.clone()).run("P4", 0, &asked)).unwrap());
    in_flight.join().unwrap();
    // Digest: the target's digest at N differs.
    std::fs::write(&digest_file, "").unwrap();
    causes.push(blocking(|| mover(short.clone()).run("P4", 0, &asked)).unwrap());
    std::fs::remove_file(&digest_file).unwrap();
    let said: Vec<i32> = causes
        .iter()
        .map(|ended| match ended {
            Ended::Aborted(aborted, _) => aborted.cause,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        said,
        [
            MoveAbortCause::Lag as i32,
            MoveAbortCause::Timeout as i32,
            MoveAbortCause::Digest as i32
        ]
    );
    let Ended::Aborted(_, Some(finding)) = &causes[2] else {
        panic!("a digest abort carries its finding");
    };
    assert_eq!(
        finding.check,
        meridian_domain::v1::IntegrityCheck::MemberDigest as i32
    );
    // The source owns at its epoch, and writes on with no gap.
    blocking(|| {
        let tip = zero_store.tip("P4").unwrap().unwrap();
        assert_eq!(tip.writer_group, Some(1));
        assert_eq!(tip.epoch, epoch_before);
    });
    send(&one, RECORD_BREAK, &a_break(&account), Some(PERSON))
        .await
        .unwrap();
    blocking(|| {
        let rows = numbered(&url, "P4");
        assert!(rows.len() >= 2);
        for ended in &causes {
            journal(&zero, zero_store.as_ref(), ended);
        }
        let control = zero_store
            .journal(meridian_bor::store::CONTROL_ACCOUNT)
            .unwrap();
        let keys: Vec<&str> = control.iter().map(|e| e.idempotency_key.as_str()).collect();
        assert_eq!(
            keys.iter()
                .filter(|k| k.starts_with("control-record:partition-move-aborted:P4:"))
                .count(),
            3,
            "{keys:?}"
        );
        assert_eq!(
            keys.iter()
                .filter(|k| k.starts_with("control-record:finding:move:P4:"))
                .count(),
            1
        );
    });
    let _ = std::fs::remove_dir_all(&files);
    blocking(move || drop((zero, one, zero_store, one_store, migrated)));
}

/// A book partition moved to a writer group in another database under write
/// load (step 3d; sections 6.4 and 6.8): the target's database copies the
/// partition -- its entries and every account it places -- then its change
/// log until within readyLagMs; the switch brings it to the source's close
/// under the lock; the held writes drain to it, none refused. The chain is
/// whole in the new database and scrubs clean in both; each account
/// replays the same through either member, its positions read once, from
/// its new database; a read naming a watermark taken before is answered
/// after.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_book_partition_moved_between_databases_under_load_is_copied_and_switched() {
    use meridian_bor::partition_moves::{Asked, Ended, Mover, Settings};
    use meridian_bor::spread::Spread;
    use meridian_bor::writer::{is_command, INTERNAL_COMMAND};
    use meridian_changes::groups::Groups;
    use meridian_pb::v1::{PartitionSequence, Watermark};
    let urls: Vec<Scratch> = blocking(|| {
        (0..2)
            .map(|_| {
                let (url, migrated) = isolated();
                migrated.make_partitions(8).unwrap();
                apart(&url);
                url
            })
            .collect()
    });
    let broker = Arc::new(GroupsBroker {
        inner: MemoryBackend::new(),
        groups: Default::default(),
        handed: AtomicU64::new(0),
    });
    let member = |group: u32| {
        let groups = Groups::new(group, 2).unwrap();
        let stores: Vec<Arc<PostgresStore>> = urls
            .iter()
            .map(|url| {
                let store = PostgresStore::connect(url, 6)
                    .unwrap()
                    .with_groups(groups)
                    .with_partitions(8);
                store.pin_owners().unwrap();
                Arc::new(store)
            })
            .collect();
        let spread = Arc::new(Spread::new(stores, groups));
        let bus = Arc::new(Bus::single(
            "bor-cross-moves-test",
            Arc::clone(&broker) as Arc<dyn meridian_bus::Backend>,
            Arc::new(meridian_clock::SystemClock),
        ));
        bus.hand_writes_on(INTERNAL_COMMAND, group, is_command);
        serve(bus.clone(), spread.clone(), bus.clock());
        resolving_equities(&bus);
        (bus, spread)
    };
    let ((bus_zero, zero), (bus_one, one)) = blocking(|| (member(0), member(1)));
    let groups = Groups::new(0, 2).unwrap();
    // A partition of group 1's, in database 1, and accounts placed in it.
    // P4: a partition no takeover test's lock names.
    let partition = "P4".to_string();
    assert_eq!(groups.owner(&partition), 1);
    let accounts: Vec<String> = (0..)
        .map(|n| unique(&format!("ACC-PXDB-{n}")))
        .filter(|a| meridian_bor::store::placement_of(a, 8) == partition)
        .take(3)
        .collect();
    for account in &accounts {
        blocking(|| one.place_first(account, account)).unwrap();
        open(&bus_one, account, "INS-AAPL").await;
    }
    let before = blocking(|| one.database(1).tip(&partition))
        .unwrap()
        .unwrap()
        .head;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let load = [
        writing(bus_one.clone(), accounts.clone(), stop.clone()),
        writing(bus_zero.clone(), accounts.clone(), stop.clone()),
    ];
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let mover = Mover::new(
        vec![zero.database(0).clone(), zero.database(1).clone()],
        zero.database(0).groups(),
        Settings {
            switch: std::time::Duration::from_secs(10),
            ..Default::default()
        },
        Arc::new(meridian_clock::SystemClock),
    );
    let ended = blocking(|| {
        mover.begin(&partition);
        mover.run(
            &partition,
            0,
            &Asked {
                by_hand: true,
                moved_by: PERSON.into(),
                reason: "group 1's database is busy".into(),
            },
        )
    })
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    stop.store(true, Ordering::SeqCst);
    let mut written = 0;
    for task in load {
        let (count, refused) = task.await.unwrap();
        assert!(refused.is_empty(), "refused inside the hold: {refused:?}");
        written += count;
    }
    assert!(written > 10, "{written} written under the move");
    let Ended::Moved(moved) = &ended else {
        panic!("{ended:?}");
    };
    assert_eq!((moved.from_group, moved.to_group), (1, 0));
    let fresh = |d: usize| PostgresStore::connect(&urls[d], 1).unwrap();
    blocking(|| {
        let (new, old) = (
            fresh(0).tip(&partition).unwrap().unwrap(),
            fresh(1).tip(&partition).unwrap().unwrap(),
        );
        assert_eq!((new.writer_group, old.writer_group), (Some(0), Some(0)));
        assert!(new.head > old.head, "written on in its new database");
        // Whole in the new database: no hole from 1, the old epoch up to
        // the close, the new after; each database scrubs clean.
        let rows = numbered(&urls[0], &partition);
        assert_eq!(rows[0].0, 1);
        assert_eq!(rows.last().unwrap().1 as u64, new.head);
        let at = rows
            .iter()
            .position(|(first, _, _)| *first as u64 > old.head)
            .expect("written after the switch");
        assert!(rows[at..].iter().all(|(_, _, e)| *e as u64 == moved.epoch));
        for d in 0..2 {
            let scrubbed = fresh(d).scrub().unwrap();
            assert!(scrubbed.findings.is_empty(), "{d}: {:?}", scrubbed.findings);
        }
        for account in &accounts {
            // Read through either member, the same; replayed the same.
            let journal = zero.journal(account).unwrap();
            assert_eq!(one.journal(account).unwrap(), journal);
            let mut replayed = meridian_bor::Book::replay(account, &partition, &journal).unwrap();
            let mut held = zero.book(account).unwrap();
            replayed.release_bodies();
            held.release_bodies();
            assert_eq!(replayed, held);
            // Placed in the new database alone, its positions read once.
            assert_eq!(fresh(1).account_row(account).unwrap(), None);
            assert_eq!(
                fresh(0).account_row(account).unwrap(),
                Some(partition.clone())
            );
            let read = PositionsRead {
                scope: Scope::Everything,
                account_id: String::new(),
                since: None,
                business_date: String::new(),
                at: None,
                limit: 500,
                cursor: String::new(),
            };
            let everyone = zero.positions(&read).unwrap().records;
            assert_eq!(
                everyone.iter().filter(|p| p.account_id == *account).count(),
                1
            );
        }
        // A read naming the head before the move, answered after it.
        let watermark = Watermark {
            partitions: vec![PartitionSequence {
                partition: partition.clone(),
                sequence: before,
            }],
        };
        meridian_bus::read_after::begin_read();
        bus_zero
            .reads_after()
            .answer(LIST_POSITIONS, &watermark, || {
                zero.journal(&accounts[0]).map_err(|f| f.to_string())
            })
            .unwrap();
    });
    blocking(move || drop((zero, one, bus_zero, bus_one, mover)));
}

/// A changed count of writer groups moves each partition its rendezvous
/// owner at the new count is another group's, once, by_hand false (section
/// 6.8, Triggers): the heads pinned at one group keep every partition with
/// group 0 until the move, and a second pass moves nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_changed_count_of_writer_groups_moves_each_partition_once() {
    use meridian_bor::partition_moves::{rebalance, Ended};
    use meridian_changes::groups::{owner, Groups};
    let (url, migrated) = blocking(isolated);
    // Started at one group: every head pinned to group 0.
    blocking(|| {
        let one = PostgresStore::connect(&url, 2)
            .unwrap()
            .with_groups(Groups::new(0, 1).unwrap())
            .with_partitions(8);
        one.make_partitions(8).unwrap();
        assert_eq!(one.pin_owners().unwrap(), 8);
    });
    // Raised to two: nothing has jumped.
    let (_broker, [(zero, zero_store), (one, one_store)], mover) = blocking(|| two_members(&url));
    blocking(|| {
        for n in 0..8 {
            let p = meridian_bor::store::partition_named(n);
            assert_eq!(
                zero_store.tip(&p).unwrap().unwrap().writer_group,
                Some(0),
                "{p}"
            );
        }
    });
    let ended = blocking(|| rebalance(&zero, zero_store.as_ref(), &mover));
    let wanted: Vec<String> = (0..8)
        .map(meridian_bor::store::partition_named)
        .filter(|p| owner(p, 2) == 1)
        .collect();
    let moved: Vec<String> = ended
        .iter()
        .map(|e| match e {
            Ended::Moved(m) => {
                assert!(!m.by_hand && m.moved_by.is_empty() && m.reason.is_empty());
                assert_eq!((m.from_group, m.to_group), (0, 1));
                m.partition.clone()
            }
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(moved, wanted);
    blocking(|| {
        assert!(rebalance(&zero, zero_store.as_ref(), &mover).is_empty());
        for p in &wanted {
            assert_eq!(zero_store.tip(p).unwrap().unwrap().writer_group, Some(1));
        }
        let control = zero_store
            .journal(meridian_bor::store::CONTROL_ACCOUNT)
            .unwrap();
        let records = control
            .iter()
            .filter(|e| {
                e.idempotency_key
                    .starts_with("control-record:partition-moved:")
            })
            .count();
        assert_eq!(records, wanted.len());
    });
    blocking(move || drop((zero, one, zero_store, one_store, migrated)));
}

/// A partition a deployment admin moved by hand stays where she put it
/// (step 3d; section 6.8, ruled 2026-11-14): the sweep, every minute, moves
/// only by a changed count. With the count raised after her move, the
/// other partitions converge on their owners at the new count and hers
/// stays; the sweep then moves nothing more.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_partition_moved_by_hand_stays_through_the_sweep_and_a_raised_count() {
    use meridian_bor::partition_moves::{journal, rebalance, Asked, Ended, Mover, Settings};
    use meridian_changes::groups::{owner, Groups};
    let (url, migrated) = blocking(isolated);
    let (_broker, [(zero, zero_store), (one, one_store)], mover) = blocking(|| two_members(&url));
    // Hers: a partition of group 1's, which a count of three keeps away
    // from group 0, moved to group 0 by hand.
    let mine = (0..8)
        .map(meridian_bor::store::partition_named)
        .find(|p| owner(p, 2) == 1 && owner(p, 3) != 0)
        .expect("a partition of group 1's at two and three");
    let ended = blocking(|| {
        mover.begin(&mine);
        mover.run(
            &mine,
            0,
            &Asked {
                by_hand: true,
                moved_by: PERSON.into(),
                reason: "group 1 is busy".into(),
            },
        )
    })
    .unwrap();
    assert!(matches!(&ended, Ended::Moved(m) if m.by_hand), "{ended:?}");
    blocking(|| journal(&zero, zero_store.as_ref(), &ended));
    // The sweep: nothing moved, hers still group 0's.
    blocking(|| {
        assert!(rebalance(&zero, zero_store.as_ref(), &mover).is_empty());
        assert_eq!(
            zero_store.tip(&mine).unwrap().unwrap().writer_group,
            Some(0)
        );
    });
    // The count raised to three: the others converge, hers stays.
    let three = blocking(|| {
        Arc::new(
            PostgresStore::connect(&url, 4)
                .unwrap()
                .with_groups(Groups::new(0, 3).unwrap())
                .with_partitions(8),
        )
    });
    // Group 2's writer started: it takes over, so a partition moved to it
    // is written under an epoch its takeover recorded.
    blocking(|| {
        let two = PostgresStore::connect(&url, 2)
            .unwrap()
            .with_groups(Groups::new(2, 3).unwrap())
            .with_partitions(8);
        Store::adopt(&two).unwrap();
    });
    let mover_three = Mover::new(
        vec![Arc::clone(&three)],
        three.groups(),
        Settings {
            switch: std::time::Duration::from_secs(2),
            ..Default::default()
        },
        Arc::new(meridian_clock::SystemClock),
    );
    let wanted: Vec<String> = (0..8)
        .map(meridian_bor::store::partition_named)
        .filter(|p| *p != mine && owner(p, 3) != owner(p, 2))
        .collect();
    let moved: Vec<String> = blocking(|| rebalance(&zero, three.as_ref(), &mover_three))
        .into_iter()
        .map(|e| match e {
            Ended::Moved(m) => {
                assert!(!m.by_hand);
                assert_eq!(m.to_group, owner(&m.partition, 3));
                m.partition
            }
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(moved, wanted);
    blocking(|| {
        assert_eq!(three.tip(&mine).unwrap().unwrap().writer_group, Some(0));
        assert!(rebalance(&zero, three.as_ref(), &mover_three).is_empty());
    });
    blocking(move || {
        drop((
            zero,
            one,
            zero_store,
            one_store,
            three,
            mover,
            mover_three,
            migrated,
        ))
    });
}

/// Every cause an account's move aborts for (step 3d; section 6.8; ruled
/// 2026-11-14), each before its close: the injected lag, a close its
/// partition holds past moves.switchSeconds, and the injected digest
/// mismatch. Each leaves the account where it was, its journal unchanged,
/// journals PartitionMoveAborted naming the account and its target on
/// ~control (an IntegrityFinding beside the digest's), and is answered
/// OVERLOADED with a retry-after (lag, timeout) or FAILED_PRECONDITION
/// naming account_id (digest); asked again after, it moves.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn an_account_move_aborts_for_lag_timeout_and_digest_and_stays() {
    use meridian_bor::decide::Context;
    use meridian_bor::moves::move_account_with;
    use meridian_changes::moves::Settings;
    use meridian_domain::v1::MoveAccountRequest;
    let (url, migrated) = blocking(isolated);
    let (_broker, [(one, one_store), (_one, _one_store)], _mover) = blocking(|| two_members(&url));
    // Group 0's -- the control partition's -- account and target: its
    // writer journals the records on ~control itself.
    let zeros: Vec<String> = (0..8)
        .map(meridian_bor::store::partition_named)
        .filter(|p| meridian_changes::groups::owner(p, 2) == 0)
        .collect();
    let (from, target) = (zeros[0].clone(), zeros[1].clone());
    let account = (0..)
        .map(|n| unique(&format!("ACC-AMOVE-{n}")))
        .find(|a| meridian_bor::store::placement_of(a, 8) == from)
        .unwrap();
    blocking(|| one_store.place_first(&account, &account)).unwrap();
    open(&one, &account, "INS-AAPL").await;
    let dir = std::env::temp_dir().join(unique("amove"));
    std::fs::create_dir_all(&dir).unwrap();
    let (lag, digest) = (dir.join("lag"), dir.join("digest"));
    std::fs::write(&lag, "60000").unwrap();
    std::fs::write(&digest, "").unwrap();
    let settings = |lag_file, digest_file| Settings {
        switch: std::time::Duration::from_secs(1),
        lag_file,
        digest_file,
        ..Default::default()
    };
    let request = MoveAccountRequest {
        account_id: account.clone(),
        target_partition: target.clone(),
        reason: "an abort per cause".into(),
    };
    let before = blocking(|| Store::journal(one_store.as_ref(), &account)).unwrap();
    let attempt = |n: i64, settings: Settings| {
        let ctx = Context {
            acting_for: PERSON.into(),
            received_at_ns: 1_000 + n,
            ..Default::default()
        };
        move_account_with(&one, one_store.as_ref(), &ctx, &request, &settings)
            .unwrap_err()
            .on_the_bus()
    };
    let overloaded = format!(
        "refusal-reason {}",
        meridian_pb::v1::RefusalReason::Overloaded as i32
    );
    let lagged = blocking(|| attempt(1, settings(Some(lag.clone()), None)));
    assert!(
        lagged.contains(&overloaded) && lagged.contains("lag"),
        "{lagged}"
    );
    // Timeout: the partition's lock held past the switch's bound.
    let timed_out = blocking(|| {
        let mut holder = postgres::Client::connect(&url, postgres::NoTls).unwrap();
        let mut held = holder.transaction().unwrap();
        held.execute(
            "SELECT pg_advisory_xact_lock($1)",
            &[&meridian_bor::postgres::partition_lock_key(&from)],
        )
        .unwrap();
        let answered = attempt(2, settings(None, None));
        held.rollback().unwrap();
        answered
    });
    assert!(
        timed_out.contains(&overloaded) && timed_out.contains("switchSeconds"),
        "{timed_out}"
    );
    let differs = blocking(|| attempt(3, settings(None, Some(digest.clone()))));
    assert!(differs.contains("account_id"), "{differs}");
    blocking(|| {
        // Where it was, its journal unchanged.
        assert_eq!(
            one_store.partition_of(&account).as_deref(),
            Some(from.as_str())
        );
        assert_eq!(
            Store::journal(one_store.as_ref(), &account).unwrap(),
            before
        );
        let control = one_store
            .journal(meridian_bor::store::CONTROL_ACCOUNT)
            .unwrap();
        // One record a cause, each keyed by when it was asked (lag 1001,
        // timeout 1002, digest 1003), and the digest's finding beside it.
        let keys: Vec<String> = control
            .iter()
            .map(|e| e.idempotency_key.clone())
            .filter(|k| {
                k.contains(&format!(":{from}:")) && (k.contains("abort") || k.contains("finding"))
            })
            .collect();
        assert_eq!(
            keys,
            vec![
                format!("control-record:partition-move-aborted:{from}:1001"),
                format!("control-record:partition-move-aborted:{from}:1002"),
                format!("control-record:partition-move-aborted:{from}:1003"),
                format!("control-record:finding:move:{from}:1003"),
            ]
        );
    });
    // Asked again, with nothing in the way: it moves.
    let ctx = Context {
        acting_for: PERSON.into(),
        received_at_ns: 2_000,
        ..Default::default()
    };
    let moved = blocking(|| {
        move_account_with(
            &one,
            one_store.as_ref(),
            &ctx,
            &request,
            &settings(None, None),
        )
    })
    .unwrap();
    assert_eq!(moved.to_partition, target);
    std::fs::remove_dir_all(&dir).unwrap();
    blocking(move || drop((one, one_store, _one, _one_store, migrated)));
}

/// An account's move aborted on a member of writer group 1 (step 3d;
/// `book_control_owed`, approved 2026-11-14): that member does not write
/// `~control`, so it keeps PartitionMoveAborted -- and a digest's finding
/// -- in its own database, answering its own refusal; group 0's sweep
/// journals each on `~control` once, under its own key, and a sweep run
/// again -- a restarted group 0 -- journals nothing twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn an_account_move_aborted_in_group_one_is_journalled_on_control_once() {
    use meridian_bor::decide::Context;
    use meridian_bor::moves::move_account_with;
    use meridian_changes::groups::Groups;
    use meridian_changes::moves::Settings;
    use meridian_domain::v1::MoveAccountRequest;
    let (url, migrated) = blocking(isolated);
    let (_broker, [(zero, zero_store), (one, one_store)], _mover) = blocking(|| two_members(&url));
    let ones: Vec<String> = (0..8)
        .map(meridian_bor::store::partition_named)
        .filter(|p| meridian_changes::groups::owner(p, 2) == 1)
        .collect();
    let (from, target) = (ones[0].clone(), ones[1].clone());
    let account = (0..)
        .map(|n| unique(&format!("ACC-OWED-{n}")))
        .find(|a| meridian_bor::store::placement_of(a, 8) == from)
        .unwrap();
    blocking(|| one_store.place_first(&account, &account)).unwrap();
    open(&one, &account, "INS-AAPL").await;
    let dir = std::env::temp_dir().join(unique("owed"));
    std::fs::create_dir_all(&dir).unwrap();
    let (lag, digest) = (dir.join("lag"), dir.join("digest"));
    std::fs::write(&lag, "60000").unwrap();
    std::fs::write(&digest, "").unwrap();
    let request = MoveAccountRequest {
        account_id: account.clone(),
        target_partition: target.clone(),
        reason: "an abort in group 1".into(),
    };
    let attempt = |n: i64, lag_file, digest_file| {
        let ctx = Context {
            acting_for: PERSON.into(),
            received_at_ns: 5_000 + n,
            ..Default::default()
        };
        let settings = Settings {
            switch: std::time::Duration::from_secs(1),
            lag_file,
            digest_file,
            ..Default::default()
        };
        move_account_with(&one, one_store.as_ref(), &ctx, &request, &settings)
            .unwrap_err()
            .on_the_bus()
    };
    // Its own refusals, answered by group 1's member.
    let lagged = blocking(|| attempt(1, Some(lag.clone()), None));
    assert!(
        lagged.contains(&format!(
            "refusal-reason {}",
            meridian_pb::v1::RefusalReason::Overloaded as i32
        )),
        "{lagged}"
    );
    let differs = blocking(|| attempt(2, None, Some(digest.clone())));
    assert!(differs.contains("account_id"), "{differs}");
    let keys = [
        format!("control-record:partition-move-aborted:{from}:5001"),
        format!("control-record:partition-move-aborted:{from}:5002"),
        format!("control-record:finding:move:{from}:5002"),
    ];
    let journalled = |store: &PostgresStore| -> Vec<String> {
        let mut said: Vec<String> = store
            .journal(meridian_bor::store::CONTROL_ACCOUNT)
            .unwrap()
            .into_iter()
            .map(|e| e.idempotency_key)
            .filter(|k| keys.contains(k))
            .collect();
        said.sort();
        said
    };
    let mut once = keys.to_vec();
    once.sort();
    blocking(|| {
        // Not on control yet: kept beside group 1's journal, owed.
        assert!(journalled(&zero_store).is_empty());
        let kept: i64 = postgres::Client::connect(&url, postgres::NoTls)
            .unwrap()
            .query_one(
                "SELECT count(*) FROM book_control_owed WHERE key = ANY($1)",
                &[&keys.to_vec()],
            )
            .unwrap()
            .get(0);
        assert_eq!(kept, 3);
        // Group 0's sweep journals each once.
        assert!(meridian_bor::control::journal_owed(&zero, zero_store.as_ref()).unwrap() >= 3);
        assert_eq!(journalled(&zero_store), once);
        // Its sweep again, and a restarted group 0's: nothing twice.
        assert_eq!(
            meridian_bor::control::journal_owed(&zero, zero_store.as_ref()).unwrap(),
            0
        );
        let restarted = Arc::new(
            PostgresStore::connect(&url, 2)
                .unwrap()
                .with_groups(Groups::new(0, 2).unwrap())
                .with_partitions(8),
        );
        meridian_bor::control::journal_owed(&zero, restarted.as_ref()).unwrap();
        assert_eq!(journalled(&zero_store), once);
        // Each record as it was kept: the abort names the account, its
        // target and its cause.
        let control = zero_store
            .journal(meridian_bor::store::CONTROL_ACCOUNT)
            .unwrap();
        let aborted = control
            .iter()
            .find(|e| e.idempotency_key == keys[1])
            .and_then(|e| e.body.record.clone())
            .unwrap();
        let aborted = PartitionMoveAborted::decode(&aborted.1[..]).unwrap();
        assert_eq!(
            (
                aborted.account_id.as_str(),
                aborted.target_partition.as_str(),
                aborted.cause,
                aborted.aborted_at_ns
            ),
            (
                account.as_str(),
                target.as_str(),
                MoveAbortCause::Digest as i32,
                5002
            )
        );
    });
    std::fs::remove_dir_all(&dir).unwrap();
    blocking(move || drop((zero, zero_store, one, one_store, migrated)));
}

/// A partition moved back to a writer group that owned nothing meanwhile
/// (step 3d; `book_writer_group`, approved 2026-11-14): every partition of
/// group 1 moved to group 0, then P4 moved back. It carries group 1's own
/// epoch, kept in its row while it owned nothing, so group 1's same member
/// commits to it at the epoch it held all along: never fenced, never
/// stepping down.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_partition_moved_away_and_back_is_written_by_the_same_member_unfenced() {
    use meridian_bor::partition_moves::{Asked, Ended};
    use meridian_changes::groups::owner;
    let (url, migrated) = blocking(isolated);
    let (_broker, [(zero, zero_store), (one, one_store)], mover) = blocking(|| two_members(&url));
    let accounts: Vec<String> = (0..)
        .map(|n| unique(&format!("ACC-PBACK-{n}")))
        .filter(|a| meridian_bor::store::placement_of(a, 8) == "P4")
        .take(2)
        .collect();
    for account in &accounts {
        blocking(|| one_store.place_first(account, account)).unwrap();
        open(&one, account, "INS-AAPL").await;
    }
    // Group 1's member writes under its epoch: its row's, from its heads.
    let held = blocking(|| Store::epoch(one_store.as_ref()));
    assert!(held > 0);
    let write = |n: u64| SetAccountAttributeRequest {
        account_id: accounts[0].clone(),
        attribute: Some(
            set_account_attribute_request::Attribute::WaitsForCompliance(n.is_multiple_of(2)),
        ),
        reason: format!("write {n}"),
    };
    let asked = Asked {
        by_hand: true,
        moved_by: PERSON.into(),
        reason: "moved away and back".into(),
    };
    // Every partition of group 1 moved to group 0: it owns nothing.
    let ones: Vec<String> = (0..8)
        .map(meridian_bor::store::partition_named)
        .filter(|p| owner(p, 2) == 1)
        .collect();
    assert!(ones.contains(&"P4".to_string()));
    for partition in &ones {
        let ended = blocking(|| {
            assert!(mover.begin(partition));
            mover.run(partition, 0, &asked)
        })
        .unwrap();
        assert!(matches!(ended, Ended::Moved(_)), "{ended:?}");
    }
    send(&one, SET_ACCOUNT_ATTRIBUTE, &write(1), Some(PERSON))
        .await
        .expect("handed to group 0 while group 1 owns nothing");
    // P4 moved back: group 1's own epoch, the one its member holds.
    let ended = blocking(|| {
        assert!(mover.begin("P4"));
        mover.run("P4", 1, &asked)
    })
    .unwrap();
    let Ended::Moved(moved) = &ended else {
        panic!("{ended:?}");
    };
    assert_eq!(
        moved.epoch, held,
        "the group's own epoch, kept while it owned nothing"
    );
    // Written by group 1's same member, from either member: never fenced.
    send(&one, SET_ACCOUNT_ATTRIBUTE, &write(2), Some(PERSON))
        .await
        .expect("group 1's member commits to the partition moved back");
    send(&zero, SET_ACCOUNT_ATTRIBUTE, &write(3), Some(PERSON))
        .await
        .expect("handed to group 1");
    blocking(|| {
        assert_eq!(
            Store::epoch(one_store.as_ref()),
            held,
            "never took over again"
        );
        let tip = zero_store.tip("P4").unwrap().unwrap();
        assert_eq!((tip.writer_group, tip.epoch), (Some(1), held));
        let rows = numbered(&url, "P4");
        let last = rows.last().unwrap();
        assert_eq!(
            last.2 as u64, held,
            "the last entries committed at group 1's epoch"
        );
    });
    blocking(move || drop((zero, one, zero_store, one_store, migrated)));
}

/// A partition moved away from a group past the first while that group's
/// member looks at its database's timeline: the look, its anchor read as the
/// group's before the switch committed, finds the partition another group's
/// under its hold and never steps the member down on that group's epoch
/// (step 3d; CI 43621875619: group 1's member stepped down, "the store's
/// epoch is 1, another writer's", right after P0 was moved to group 0 by
/// hand, and the account's move handed to it went unanswered).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_partition_moved_away_while_its_group_looks_at_the_timeline_steps_no_member_down() {
    use meridian_bor::partition_moves::{Asked, Ended};
    use meridian_changes::groups::owner;
    use meridian_changes::timeline::Timeline;
    let (url, migrated) = blocking(isolated);
    let (_broker, [(zero, zero_store), (one, one_store)], mover) = blocking(|| two_members(&url));
    assert_eq!(owner("P4", 2), 1, "P4 is group 1's");
    let account = (0..)
        .map(|n| unique(&format!("ACC-PLOOK-{n}")))
        .find(|a| meridian_bor::store::placement_of(a, 8) == "P4")
        .unwrap();
    blocking(|| one_store.place_first(&account, &account)).unwrap();
    open(&one, &account, "INS-AAPL").await;
    blocking(|| {
        let held = Store::epoch(one_store.as_ref());
        assert!(held > 0);
        let looked = one_store.follow_timeline_from("P4").unwrap();
        assert!(
            matches!(looked, Timeline::Same | Timeline::First),
            "{looked:?}"
        );
        // P4 moved to group 0 between a look's read of its anchor and its
        // follow.
        assert!(mover.begin("P4"));
        let ended = mover
            .run(
                "P4",
                0,
                &Asked {
                    by_hand: true,
                    moved_by: PERSON.into(),
                    reason: "moved while looked at".into(),
                },
            )
            .unwrap();
        assert!(matches!(ended, Ended::Moved(_)), "{ended:?}");
        let tip = zero_store.tip("P4").unwrap().unwrap();
        assert_eq!(tip.writer_group, Some(0));
        assert_ne!(tip.epoch, held, "P4 carries group 0's epoch now");
        let looked = one_store.follow_timeline_from("P4").unwrap();
        assert!(
            !matches!(looked, Timeline::NotMine(_)),
            "group 1's member is not deposed by group 0's epoch: {looked:?}"
        );
        assert_eq!(Store::epoch(one_store.as_ref()), held);
    });
    blocking(move || drop((zero, one, zero_store, one_store, migrated)));
}

/// An account moved within one database by a request carrying its message
/// ID, as every request through the bus does: its close keeps the ID and
/// its open, its own entry, none, so the book's one-message index never
/// refuses the open (step 3d; CI 43621875619 and 43631481777: "duplicate
/// key value violates unique constraint book_entry_message").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_account_moved_within_one_database_by_a_request_with_its_message_id_opens() {
    use meridian_bor::decide::Context;
    use meridian_bor::moves::move_account_with;
    use meridian_changes::moves::Settings;
    use meridian_domain::v1::MoveAccountRequest;
    let (url, migrated) = blocking(isolated);
    let (_broker, [(zero, zero_store), (one, one_store)], _mover) = blocking(|| two_members(&url));
    let ones: Vec<String> = (0..8)
        .map(meridian_bor::store::partition_named)
        .filter(|p| meridian_changes::groups::owner(p, 2) == 1)
        .collect();
    let (from, target) = (ones[0].clone(), ones[1].clone());
    let account = (0..)
        .map(|n| unique(&format!("ACC-MSGID-{n}")))
        .find(|a| meridian_bor::store::placement_of(a, 8) == from)
        .unwrap();
    blocking(|| one_store.place_first(&account, &account)).unwrap();
    open(&one, &account, "INS-AAPL").await;
    let request = MoveAccountRequest {
        account_id: account.clone(),
        target_partition: target.clone(),
        reason: "moved with its message id".into(),
    };
    let ctx = Context {
        acting_for: PERSON.into(),
        received_at_ns: 5_000,
        message_id: unique("msg-move"),
        ..Default::default()
    };
    let moved = blocking(|| {
        move_account_with(
            &one,
            one_store.as_ref(),
            &ctx,
            &request,
            &Settings::default(),
        )
    })
    .expect("closed and opened within one database");
    assert_eq!(moved.to_partition, target);
    assert!(moved.opening_sequence > 0, "{moved:?}");
    let ids: Vec<String> = blocking(|| one_store.journal(&account).unwrap())
        .into_iter()
        .filter(|entry| meridian_bor::journal::moved_of(entry).is_some())
        .map(|entry| entry.message_id)
        .collect();
    assert_eq!(
        ids,
        vec![ctx.message_id.clone(), String::new()],
        "the close's, then none"
    );
    blocking(move || drop((zero, one, zero_store, one_store, migrated)));
}

/// A partition never written, its head made at the control partition's
/// epoch as a raised `bor.partitions` makes it, is written by its owner
/// group past the first at that group's epoch, never fenced (step 3d; CI
/// 43626539705: an account's move into P1 refused "fenced ... this writer's
/// 2, the partition's 1"). The test harness's `apart` hid it: it gives
/// every head its owner's epoch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_partition_made_at_controls_epoch_is_written_by_its_owner_unfenced() {
    use meridian_changes::groups::owner;
    let (url, migrated) = blocking(isolated);
    let (_broker, [(zero, zero_store), (one, one_store)], _mover) = blocking(|| two_members(&url));
    let ones: Vec<String> = (0..8)
        .map(meridian_bor::store::partition_named)
        .filter(|p| owner(p, 2) == 1)
        .collect();
    let (written, partition) = (ones[0].clone(), ones[1].clone());
    // Group 1's member writes first, so it holds its epoch before the
    // partition's head is made, as a raised count makes it on a deployment
    // already running.
    let first = (0..)
        .map(|n| unique(&format!("ACC-PFIRST-{n}")))
        .find(|a| meridian_bor::store::placement_of(a, 8) == written)
        .unwrap();
    blocking(|| one_store.place_first(&first, &first)).unwrap();
    open(&one, &first, "INS-AAPL").await;
    // As make_partitions made it: never written, at control's epoch.
    blocking(|| {
        postgres::Client::connect(&url, postgres::NoTls)
            .unwrap()
            .execute(
                "UPDATE book_partition_head SET epoch = 10, writer_group = NULL
                  WHERE partition = $1 AND sequence = 0",
                &[&partition],
            )
            .unwrap()
    });
    let account = (0..)
        .map(|n| unique(&format!("ACC-PMADE-{n}")))
        .find(|a| meridian_bor::store::placement_of(a, 8) == partition)
        .unwrap();
    blocking(|| one_store.place_first(&account, &account)).unwrap();
    send(
        &one,
        RECORD_OPENING_BALANCE,
        &opening(&account, "INS-AAPL"),
        Some(PERSON),
    )
    .await
    .expect("written by its owner, never fenced");
    // Group 1's own epoch, as its row holds it once its member adopted.
    let held: i64 = blocking(|| {
        postgres::Client::connect(&url, postgres::NoTls)
            .unwrap()
            .query_one(
                "SELECT epoch FROM book_writer_group WHERE writer_group = 1",
                &[],
            )
            .unwrap()
            .get(0)
    });
    blocking(|| {
        let tip = zero_store.tip(&partition).unwrap().unwrap();
        let now = Store::epoch(one_store.as_ref());
        eprintln!("pmade: held {held}, now {now}, the head's {}", tip.epoch);
        assert_eq!(tip.epoch, now, "at its owner's epoch");
        assert_eq!(now, held as u64, "group 1's own epoch: no takeover");
    });
    blocking(move || drop((zero, one, zero_store, one_store, migrated)));
}

/// A book whose `bor.databases` was raised reads each partition's writer
/// group from its furthest head (step 3d; CI 43626539705): the new
/// database's empty head of P0, pinned to its rendezvous owner, never names
/// it over the head where P0 was moved by hand and is written. Before, read
/// one database after another, the last won: group 1 took P0 as its own
/// and numbered its entries again from 0.
#[test]
fn a_raised_database_count_reads_each_partitions_owner_from_its_furthest_head() {
    use meridian_changes::groups::{owner, Groups};
    assert_eq!(owner("P0", 2), 1, "P0's rendezvous owner is group 1");
    let (first, _) = isolated();
    let (second, _) = isolated();
    let groups = Groups::new(1, 2).unwrap();
    let store = |url: &str| Arc::new(PostgresStore::connect(url, 2).unwrap().with_groups(groups));
    let stores = vec![store(&first), store(&second)];
    let set = |url: &str, sql: &str| {
        postgres::Client::connect(url, postgres::NoTls)
            .unwrap()
            .batch_execute(sql)
            .unwrap()
    };
    // Moved by hand to group 0 and written there; the new database's head
    // of it empty, pinned to group 1.
    set(
        &first,
        "UPDATE book_partition_head SET sequence = 5, writer_group = 0 WHERE partition = 'P0'",
    );
    set(
        &second,
        "UPDATE book_partition_head SET writer_group = 1 WHERE partition = 'P0'",
    );
    PostgresStore::read_owners_from(&stores).unwrap();
    assert_eq!(groups.owner("P0"), 0, "by the head furthest on");
    // One partition read again, from either database: the same.
    groups.assign("P0", None);
    for store in &stores {
        store.link_databases(&stores);
    }
    stores[1].read_owner("P0").unwrap();
    assert_eq!(groups.owner("P0"), 0, "read again where its head is");
}

/// A deployment installed at two writer groups starts each partition with
/// its rendezvous owner, pinned at the control partition's group's start:
/// no partition moves and no PartitionMoved is recorded, as no owner
/// changed (decisions/031: no history that did not happen).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deployment_installed_at_two_writer_groups_moves_nothing() {
    use meridian_bor::partition_moves::rebalance;
    use meridian_changes::groups::owner;
    let (url, migrated) = blocking(isolated);
    let (_broker, [(zero, zero_store), (one, one_store)], mover) = blocking(|| two_members(&url));
    blocking(|| {
        for n in 0..8 {
            let p = meridian_bor::store::partition_named(n);
            assert_eq!(
                zero_store.tip(&p).unwrap().unwrap().writer_group,
                Some(owner(&p, 2)),
                "{p} pinned to its rendezvous owner"
            );
        }
        assert!(rebalance(&zero, zero_store.as_ref(), &mover).is_empty());
        let control = zero_store
            .journal(meridian_bor::store::CONTROL_ACCOUNT)
            .unwrap();
        assert!(!control.iter().any(|e| e
            .idempotency_key
            .starts_with("control-record:partition-moved:")));
    });
    blocking(move || drop((zero, one, zero_store, one_store, migrated)));
}

/// The book's database count raised under load (step 3d; section 6.4 and
/// 6.8, ruled 2026-11-14: each partition is written where its head is).
/// Two writer groups wrote one database; with a second database, writer
/// group 1's home is the new one, but its partitions' heads are in the
/// first: its writes go on there, never held or refused, until the sweep
/// moves each home -- once, from group 1 to group 1, by_hand false -- after
/// which they are written at home alone. Nothing is refused throughout
/// (each switch's hold delays, never refuses); each partition's chain is
/// whole at home from 1, nothing committed in the first database after its
/// close; a second sweep moves nothing; and a read naming a watermark taken
/// before the moves is answered after them.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_raised_database_count_writes_where_each_head_is_and_moves_each_partition_home_once() {
    use meridian_bor::partition_moves::{rebalance, Ended, Mover, Settings};
    use meridian_bor::spread::Spread;
    use meridian_bor::writer::{is_command, INTERNAL_COMMAND};
    use meridian_changes::groups::{owner, Groups};
    use meridian_pb::v1::{PartitionSequence, Watermark};
    let urls: Vec<Scratch> = blocking(|| {
        (0..2)
            .map(|_| {
                let (url, migrated) = isolated();
                migrated.make_partitions(8).unwrap();
                apart(&url);
                url
            })
            .collect()
    });
    let broker = Arc::new(GroupsBroker {
        inner: MemoryBackend::new(),
        groups: Default::default(),
        handed: AtomicU64::new(0),
    });
    // A member of `group` over the first `databases` databases.
    let member = |group: u32, databases: usize| {
        let groups = Groups::new(group, 2).unwrap();
        let stores: Vec<Arc<PostgresStore>> = urls[..databases]
            .iter()
            .map(|url| {
                let store = PostgresStore::connect(url, 6)
                    .unwrap()
                    .with_groups(groups)
                    .with_partitions(8);
                store.pin_owners().unwrap();
                Arc::new(store)
            })
            .collect();
        let spread = Arc::new(Spread::new(stores, groups));
        let bus = Arc::new(Bus::single(
            "bor-raised-databases-test",
            Arc::clone(&broker) as Arc<dyn meridian_bus::Backend>,
            Arc::new(meridian_clock::SystemClock),
        ));
        bus.hand_writes_on(INTERNAL_COMMAND, group, is_command);
        serve(bus.clone(), spread.clone(), bus.clock());
        resolving_equities(&bus);
        (bus, spread)
    };
    // Group 1's partitions P4 and another, and one of group 0's: none a
    // takeover test's lock names.
    let ones: Vec<String> = ["P4", "P5", "P6", "P7"]
        .into_iter()
        .filter(|p| owner(p, 2) == 1)
        .take(2)
        .map(String::from)
        .collect();
    assert_eq!(ones.len(), 2, "two of group 1's partitions");
    let accounts_in = |partition: &str, n: usize| -> Vec<String> {
        (0..)
            .map(|k| unique(&format!("ACC-RAISE-{k}")))
            .filter(|a| meridian_bor::store::placement_of(a, 8) == partition)
            .take(n)
            .collect()
    };
    let accounts: Vec<String> = ones.iter().flat_map(|p| accounts_in(p, 2)).collect();
    // One database: both groups write it.
    {
        let ((bus_zero, zero), (bus_one, one)) = blocking(|| (member(0, 1), member(1, 1)));
        for account in &accounts {
            blocking(|| one.place_first(account, account)).unwrap();
            open(&bus_one, account, "INS-AAPL").await;
        }
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let load = writing(bus_zero.clone(), accounts.clone(), stop.clone());
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        stop.store(true, Ordering::SeqCst);
        let (_, refused) = load.await.unwrap();
        assert!(refused.is_empty(), "{refused:?}");
        blocking(move || drop((zero, one, bus_zero, bus_one)));
    }
    let fresh = |d: usize| PostgresStore::connect(&urls[d], 1).unwrap();
    let heads_before: Vec<u64> = blocking(|| {
        ones.iter()
            .map(|p| fresh(0).tip(p).unwrap().unwrap().head)
            .collect()
    });
    assert!(heads_before.iter().all(|h| *h > 0), "{heads_before:?}");
    // The count raised to two databases: group 1's home is the second.
    let ((bus_zero, zero), (bus_one, one)) = blocking(|| (member(0, 2), member(1, 2)));
    // Each writer started, as the runtime starts it: its epoch adopted at
    // home, so group 1 is ready in the database its partitions move to.
    blocking(|| {
        Store::adopt(zero.as_ref()).unwrap();
        Store::adopt(one.as_ref()).unwrap();
    });
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let load = [
        writing(bus_one.clone(), accounts.clone(), stop.clone()),
        writing(bus_zero.clone(), accounts.clone(), stop.clone()),
    ];
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    // Written on where the heads are, the new database not begun.
    blocking(|| {
        for (p, before) in ones.iter().zip(&heads_before) {
            assert!(fresh(0).tip(p).unwrap().unwrap().head > *before, "{p}");
            assert_eq!(fresh(1).tip(p).unwrap().unwrap().head, 0, "{p}");
        }
    });
    let watermark = blocking(|| Watermark {
        partitions: ones
            .iter()
            .map(|p| PartitionSequence {
                partition: p.clone(),
                sequence: fresh(0).tip(p).unwrap().unwrap().head,
            })
            .collect(),
    });
    // Group 0's sweep, under the load.
    let mover = Mover::new(
        vec![zero.database(0).clone(), zero.database(1).clone()],
        zero.database(0).groups(),
        Settings {
            switch: std::time::Duration::from_secs(10),
            ..Default::default()
        },
        Arc::new(meridian_clock::SystemClock),
    );
    let ended = blocking(|| rebalance(&bus_zero, zero.as_ref(), &mover));
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    stop.store(true, Ordering::SeqCst);
    let mut written = 0;
    for task in load {
        let (count, refused) = task.await.unwrap();
        assert!(refused.is_empty(), "refused: {refused:?}");
        written += count;
    }
    assert!(written > 20, "{written} written across the raise");
    let moved: Vec<(String, u64)> = ended
        .iter()
        .map(|e| match e {
            Ended::Moved(m) => {
                assert!(!m.by_hand && m.moved_by.is_empty(), "{m:?}");
                assert_eq!((m.from_group, m.to_group), (1, 1));
                (m.partition.clone(), m.epoch)
            }
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        moved.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>(),
        ones,
        "each of group 1's partitions moved home once"
    );
    blocking(|| {
        assert!(
            rebalance(&bus_zero, zero.as_ref(), &mover).is_empty(),
            "a second sweep moves nothing"
        );
        for p in &ones {
            let (away, home) = (
                fresh(0).tip(p).unwrap().unwrap(),
                fresh(1).tip(p).unwrap().unwrap(),
            );
            assert!(home.head > away.head, "{p}: written on at home");
            // Nothing committed away after its close; whole at home from 1.
            let left = numbered(&urls[0], p);
            assert_eq!(left.last().unwrap().1 as u64, away.head, "{p}");
            let rows = numbered(&urls[1], p);
            assert_eq!(rows[0].0, 1, "{p}");
            assert_eq!(rows.last().unwrap().1 as u64, home.head, "{p}");
        }
        for d in 0..2 {
            let scrubbed = fresh(d).scrub().unwrap();
            assert!(scrubbed.findings.is_empty(), "{d}: {:?}", scrubbed.findings);
        }
        for account in &accounts {
            let journal = zero.journal(account).unwrap();
            assert_eq!(one.journal(account).unwrap(), journal);
        }
        meridian_bus::read_after::begin_read();
        bus_zero
            .reads_after()
            .answer(LIST_POSITIONS, &watermark, || {
                zero.journal(&accounts[0]).map_err(|f| f.to_string())
            })
            .unwrap();
    });
    blocking(move || drop((zero, one, bus_zero, bus_one, mover)));
}

// ---- The correctness review of the moves (2026-11-14, c30d-m7) ----------

/// Two databases, a member of each of two writer groups over both, and the
/// control group's mover over both: the review's cross-database harness.
struct TwoDatabases {
    zero: (Arc<Bus>, Arc<meridian_bor::spread::Spread>),
    one: (Arc<Bus>, Arc<meridian_bor::spread::Spread>),
    mover: meridian_bor::partition_moves::Mover,
    /// The members' broker: how many writes were handed on.
    broker: Arc<GroupsBroker>,
    // Last, so the schemas are dropped after the stores over them.
    urls: Vec<Scratch>,
}

fn two_databases() -> TwoDatabases {
    two_databases_snapshotting(None)
}

/// [`two_databases`], each member's stores keeping a snapshot every
/// `interval` entries of an account where one is given: a large partition
/// built quickly, each act replaying a short tail.
fn two_databases_snapshotting(interval: Option<u64>) -> TwoDatabases {
    databases(2, interval)
}

/// [`two_databases_snapshotting`] over `count` databases: the two groups'
/// own, and past them databases neither group's home is in.
fn databases(count: usize, interval: Option<u64>) -> TwoDatabases {
    use meridian_bor::writer::{is_command, INTERNAL_COMMAND};
    use meridian_changes::groups::Groups;
    let urls: Vec<Scratch> = (0..count)
        .map(|_| {
            let (url, migrated) = isolated();
            migrated.make_partitions(8).unwrap();
            apart(&url);
            url
        })
        .collect();
    let broker = Arc::new(GroupsBroker {
        inner: MemoryBackend::new(),
        groups: Default::default(),
        handed: AtomicU64::new(0),
    });
    let member = |group: u32| {
        let groups = Groups::new(group, 2).unwrap();
        let stores: Vec<Arc<PostgresStore>> = urls
            .iter()
            .map(|url| {
                let store = PostgresStore::connect(url, 6)
                    .unwrap()
                    .with_groups(groups)
                    .with_partitions(8);
                let store = match interval {
                    Some(interval) => store.with_snapshot_interval(interval),
                    None => store,
                };
                store.pin_owners().unwrap();
                Arc::new(store)
            })
            .collect();
        let spread = Arc::new(meridian_bor::spread::Spread::new(stores, groups));
        let bus = Arc::new(Bus::single(
            "bor-review-test",
            Arc::clone(&broker) as Arc<dyn meridian_bus::Backend>,
            Arc::new(meridian_clock::SystemClock),
        ));
        bus.hand_writes_on(INTERNAL_COMMAND, group, is_command);
        serve(bus.clone(), spread.clone(), bus.clock());
        resolving_equities(&bus);
        (bus, spread)
    };
    let (zero, one) = (member(0), member(1));
    let mover = meridian_bor::partition_moves::Mover::new(
        (0..count as u32)
            .map(|d| zero.1.database(d).clone())
            .collect(),
        zero.1.database(0).groups(),
        meridian_bor::partition_moves::Settings {
            switch: std::time::Duration::from_secs(10),
            ..Default::default()
        },
        Arc::new(meridian_clock::SystemClock),
    );
    TwoDatabases {
        urls,
        zero,
        one,
        mover,
        broker,
    }
}

/// The `nth` partition of writer group `group`'s at two groups that no
/// other test of this file writes (never P0, P1 or P4): each review test
/// its own, so no switch of one holds another's writes.
fn partition_of(group: u32, nth: usize) -> String {
    (2..8)
        .map(meridian_bor::store::partition_named)
        .filter(|p| p != "P4" && meridian_changes::groups::owner(p, 2) == group)
        .cycle()
        .nth(nth)
        .unwrap_or_else(|| panic!("no partition {nth} of group {group}'s"))
}

/// Accounts placed in `partition`, `count` of them, opened through `bus`.
async fn accounts_in(
    tag: &str,
    partition: &str,
    count: usize,
    bus: &Bus,
    store: &dyn Store,
) -> Vec<String> {
    let accounts: Vec<String> = (0..)
        .map(|n| unique(&format!("{tag}-{n}")))
        .filter(|a| meridian_bor::store::placement_of(a, 8) == partition)
        .take(count)
        .collect();
    for account in &accounts {
        blocking(|| store.place_first(account, account)).unwrap();
        open(bus, account, "INS-AAPL").await;
    }
    accounts
}

/// One write to `account` through `bus`: its attribute set.
async fn write_once(bus: &Bus, account: &str, n: u64) -> Result<(), String> {
    let request = SetAccountAttributeRequest {
        account_id: account.to_string(),
        attribute: Some(
            set_account_attribute_request::Attribute::WaitsForCompliance(n.is_multiple_of(2)),
        ),
        reason: format!("write {n}"),
    };
    send(bus, SET_ACCOUNT_ATTRIBUTE, &request, Some(PERSON))
        .await
        .map(|_| ())
}

/// `partition`'s heads in each database, furthest on first.
fn heads_of(urls: &[Scratch], partition: &str) -> Vec<meridian_bor::postgres::Tip> {
    urls.iter()
        .map(|url| {
            PostgresStore::connect(url, 1)
                .unwrap()
                .tip(partition)
                .unwrap()
                .unwrap_or_default()
        })
        .collect()
}

/// C3: a switch between databases stopped between its two heads -- the
/// source let go, the target not yet opened -- never leaves two owners:
/// the heads furthest on that are written name one group, a write to it
/// waits rather than committing in both, and the control group's sweep
/// opens the target, where it is written on, once, unbroken.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn review_c3_a_switch_stopped_between_its_two_databases_leaves_one_owner() {
    use meridian_bor::partition_moves::{rebalance, Asked, Ended};
    use meridian_changes::moves::points;
    let h = blocking(two_databases);
    let partition = partition_of(1, 0);
    let accounts = accounts_in("ACC-C3", &partition, 2, &h.one.0, h.one.1.as_ref()).await;
    for n in 0..4 {
        write_once(&h.one.0, &accounts[n % 2], n as u64)
            .await
            .unwrap();
    }
    points::arm(
        "book.switch.between",
        &partition,
        Arc::new(|_| Err("stopped between the two databases".into())),
    );
    let ended = blocking(|| {
        h.mover.begin(&partition);
        h.mover.run(&partition, 0, &Asked::default())
    });
    points::disarm("book.switch.between", &partition);
    let tips = blocking(|| heads_of(&h.urls, &partition));
    let top = tips.iter().map(|tip| tip.head).max().unwrap();
    let written: std::collections::BTreeSet<Option<u32>> = tips
        .iter()
        .filter(|tip| tip.head == top && tip.epoch != meridian_changes::moves::POINTER)
        .map(|tip| tip.writer_group)
        .collect();
    assert!(written.len() <= 1, "two owners at {top}: {tips:?}");
    assert!(matches!(ended, Ok(Ended::Moved(_))), "{ended:?}");
    // The sweep opens the target; each member's write then commits once.
    blocking(|| rebalance(&h.zero.0, h.zero.1.as_ref(), &h.mover));
    write_once(&h.zero.0, &accounts[0], 10).await.unwrap();
    write_once(&h.one.0, &accounts[1], 11).await.unwrap();
    blocking(|| {
        let tips = heads_of(&h.urls, &partition);
        assert_eq!(tips[0].writer_group, Some(0));
        assert_ne!(tips[0].epoch, 0, "opened: {tips:?}");
        assert_eq!(tips[0].head, top + 2, "{tips:?}");
        let rows = numbered(&h.urls[0], &partition);
        assert_eq!((rows[0].0, rows.last().unwrap().1 as u64), (1, top + 2));
        let old = numbered(&h.urls[1], &partition);
        assert!(
            old.last().unwrap().1 as u64 <= top,
            "written in both: {old:?}"
        );
        for url in &h.urls {
            let scrubbed = PostgresStore::connect(url, 1).unwrap().scrub().unwrap();
            assert!(scrubbed.findings.is_empty(), "{:?}", scrubbed.findings);
        }
    });
    blocking(move || drop(h));
}

/// A1: the source's commit made but its answer lost: the switch reads the
/// source's head again and goes on, never naming the source again on the
/// target -- one owner, the target.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn review_a1_a_lost_answer_to_the_sources_commit_leaves_one_owner() {
    use meridian_bor::partition_moves::{Asked, Ended};
    use meridian_changes::moves::points;
    let h = blocking(two_databases);
    let partition = partition_of(1, 1);
    let accounts = accounts_in("ACC-A1", &partition, 1, &h.one.0, h.one.1.as_ref()).await;
    write_once(&h.one.0, &accounts[0], 1).await.unwrap();
    points::arm(
        "book.switch.committed",
        &partition,
        Arc::new(|_| Err("the commit's answer was lost".into())),
    );
    let ended = blocking(|| {
        h.mover.begin(&partition);
        h.mover.run(&partition, 0, &Asked::default())
    });
    points::disarm("book.switch.committed", &partition);
    let tips = blocking(|| heads_of(&h.urls, &partition));
    assert_eq!(
        (tips[0].writer_group, tips[1].writer_group),
        (Some(0), Some(0)),
        "{tips:?}"
    );
    assert!(matches!(ended, Ok(Ended::Moved(_))), "{ended:?}");
    write_once(&h.one.0, &accounts[0], 2).await.unwrap();
    blocking(|| {
        let tips = heads_of(&h.urls, &partition);
        assert!(tips[0].head > tips[1].head, "{tips:?}");
    });
    blocking(move || drop(h));
}

/// C1: entries committed while a warm round copies -- one account's later
/// entry ahead of another's earlier -- leave no hole: the switch copies from
/// the copy's own watermark, and the target's chain is whole to the close.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn review_c1_entries_committed_during_a_warm_round_leave_no_hole() {
    use meridian_changes::moves::points;
    let h = blocking(two_databases);
    let partition = partition_of(1, 2);
    let accounts = accounts_in("ACC-C1", &partition, 2, &h.one.0, h.one.1.as_ref()).await;
    let (x, y) = (accounts[0].clone(), accounts[1].clone());
    let (source, target) = (h.one.1.database(1).clone(), h.one.1.database(0).clone());
    let warmed = blocking(|| target.copy_partition_from(&source, &partition, 0)).unwrap();
    write_once(&h.one.0, &x, 1).await.unwrap();
    let (bus, fired) = (
        h.one.0.clone(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    );
    let handle = tokio::runtime::Handle::current();
    points::arm(
        "book.copy.read",
        &partition,
        Arc::new(move |_| {
            if !fired.swap(true, Ordering::SeqCst) {
                handle.block_on(async {
                    write_once(&bus, &y, 2).await.unwrap();
                    write_once(&bus, &x, 3).await.unwrap();
                });
            }
            Ok(())
        }),
    );
    let warmed = blocking(|| target.copy_partition_from(&source, &partition, warmed)).unwrap();
    points::disarm("book.copy.read", &partition);
    let switched = blocking(|| {
        source.switch_partition(
            &target,
            &partition,
            0,
            std::time::Duration::from_secs(10),
            false,
            warmed,
        )
    })
    .unwrap();
    assert!(
        matches!(switched, meridian_bor::postgres::Switched::Moved { .. }),
        "{switched:?}"
    );
    blocking(|| {
        let ours = numbered(&h.urls[0], &partition);
        let theirs = numbered(&h.urls[1], &partition);
        assert_eq!(
            ours, theirs,
            "the target's entries differ from the source's"
        );
    });
    blocking(move || drop(h));
}

/// C6: an account's entry committed while a warm round reads which
/// accounts it copies leaves no projection stale: at the switch every
/// account's projections are the source's.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn review_c6_an_entry_committed_during_a_warm_round_leaves_no_projection_stale() {
    use meridian_changes::moves::points;
    let h = blocking(two_databases);
    let partition = partition_of(1, 0);
    let accounts = accounts_in("ACC-C6", &partition, 1, &h.one.0, h.one.1.as_ref()).await;
    let x = accounts[0].clone();
    let (source, target) = (h.one.1.database(1).clone(), h.one.1.database(0).clone());
    let warmed = blocking(|| target.copy_partition_from(&source, &partition, 0)).unwrap();
    let (bus, fired) = (
        h.one.0.clone(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    );
    let handle = tokio::runtime::Handle::current();
    let xx = x.clone();
    points::arm(
        "book.copy.accounts",
        &partition,
        Arc::new(move |_| {
            if !fired.swap(true, Ordering::SeqCst) {
                handle.block_on(write_once(&bus, &xx, 1)).unwrap();
            }
            Ok(())
        }),
    );
    let warmed = blocking(|| target.copy_partition_from(&source, &partition, warmed)).unwrap();
    points::disarm("book.copy.accounts", &partition);
    blocking(|| {
        source
            .switch_partition(
                &target,
                &partition,
                0,
                std::time::Duration::from_secs(10),
                false,
                warmed,
            )
            .unwrap()
    });
    let attributes = |url: &str| -> Vec<Vec<u8>> {
        postgres::Client::connect(url, postgres::NoTls)
            .unwrap()
            .query(
                "SELECT record FROM book_attributes WHERE account_id = $1",
                &[&x],
            )
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect()
    };
    blocking(|| assert_eq!(attributes(&h.urls[0]), attributes(&h.urls[1])));
    blocking(move || drop(h));
}

/// C9: the switch's digest at N is the target's chain taken again, not a
/// copied value: an entry changed in the target after its copy aborts the
/// move, cause digest.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn review_c9_a_copy_changed_after_it_was_made_is_found_at_the_switch() {
    let h = blocking(two_databases);
    let partition = partition_of(1, 1);
    let accounts = accounts_in("ACC-C9", &partition, 1, &h.one.0, h.one.1.as_ref()).await;
    for n in 0..3 {
        write_once(&h.one.0, &accounts[0], n).await.unwrap();
    }
    let (source, target) = (h.one.1.database(1).clone(), h.one.1.database(0).clone());
    let warmed = blocking(|| target.copy_partition_from(&source, &partition, 0)).unwrap();
    blocking(|| {
        // Changed as only someone with the database's own rights could.
        postgres::Client::connect(&h.urls[0], postgres::NoTls)
            .unwrap()
            .batch_execute(&format!(
                "ALTER TABLE book_entry DISABLE TRIGGER book_entry_append_only;
                 UPDATE book_entry SET message_id = message_id || '-altered'
                  WHERE entry_id = (SELECT entry_id FROM book_entry WHERE partition = '{partition}'
                                     ORDER BY first_sequence OFFSET 1 LIMIT 1);
                 ALTER TABLE book_entry ENABLE TRIGGER book_entry_append_only;"
            ))
            .unwrap();
    });
    let switched = blocking(|| {
        source.switch_partition(
            &target,
            &partition,
            0,
            std::time::Duration::from_secs(10),
            false,
            warmed,
        )
    })
    .unwrap();
    assert!(
        matches!(
            switched,
            meridian_bor::postgres::Switched::DigestDiffers { .. }
        ),
        "{switched:?}"
    );
    let tips = blocking(|| heads_of(&h.urls, &partition));
    assert_eq!(
        tips[1].writer_group,
        Some(1),
        "the source still owns: {tips:?}"
    );
    blocking(move || drop(h));
}

/// C5: while a partition warms in another database, and after its move
/// aborts, each account's positions are read once.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn review_c5_a_warming_or_aborted_move_reads_each_position_once() {
    use meridian_bor::partition_moves::{Asked, Ended};
    let h = blocking(two_databases);
    let partition = partition_of(1, 2);
    let accounts = accounts_in("ACC-C5", &partition, 2, &h.one.0, h.one.1.as_ref()).await;
    let read = PositionsRead {
        scope: Scope::Everything,
        account_id: String::new(),
        since: None,
        business_date: String::new(),
        at: None,
        limit: 500,
        cursor: String::new(),
    };
    let once = |when: &str| {
        let everyone = h.zero.1.positions(&read).unwrap().records;
        for account in &accounts {
            let n = everyone.iter().filter(|p| p.account_id == *account).count();
            assert_eq!(n, 1, "{when}: {account} read {n} times");
        }
    };
    let (source, target) = (h.one.1.database(1).clone(), h.one.1.database(0).clone());
    blocking(|| target.copy_partition_from(&source, &partition, 0)).unwrap();
    blocking(|| once("warming"));
    // A move aborted at its switch, its digests made to differ.
    let dir = std::env::temp_dir().join(unique("review-c5"));
    std::fs::write(&dir, b"").unwrap();
    let mover = meridian_bor::partition_moves::Mover::new(
        vec![h.zero.1.database(0).clone(), h.zero.1.database(1).clone()],
        h.zero.1.database(0).groups(),
        meridian_bor::partition_moves::Settings {
            switch: std::time::Duration::from_secs(10),
            digest_file: Some(dir.clone()),
            ..Default::default()
        },
        Arc::new(meridian_clock::SystemClock),
    );
    let ended = blocking(|| {
        mover.begin(&partition);
        mover.run(&partition, 0, &Asked::default())
    })
    .unwrap();
    std::fs::remove_file(&dir).ok();
    assert!(matches!(ended, Ended::Aborted(..)), "{ended:?}");
    blocking(|| once("aborted"));
    blocking(move || drop((h, mover)));
}

/// C5's remainder: a read of every account while a partition switches
/// between databases -- its accounts copied into the target, the source let
/// go, the target not yet opened, then opened with the source's copies not
/// yet let go -- reads each account once, from the database writing it.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn review_c5_a_read_during_the_switch_reads_each_account_once() {
    use meridian_bor::partition_moves::{Asked, Ended};
    use meridian_changes::moves::points;
    let h = blocking(two_databases);
    let partition = partition_of(1, 2);
    let accounts = accounts_in("ACC-C5S", &partition, 2, &h.one.0, h.one.1.as_ref()).await;
    let read = PositionsRead {
        scope: Scope::Everything,
        account_id: String::new(),
        since: None,
        business_date: String::new(),
        at: None,
        limit: 500,
        cursor: String::new(),
    };
    let counted = {
        let reader = Arc::clone(&h.zero.1);
        let accounts = accounts.clone();
        move |read: &PositionsRead| -> Result<Vec<usize>, String> {
            let everyone = reader
                .positions(read)
                .map_err(|failed| failed.to_string())?
                .records;
            Ok(accounts
                .iter()
                .map(|account| everyone.iter().filter(|p| p.account_id == *account).count())
                .collect())
        }
    };
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let steps = [
        "book.switch.copied",
        "book.switch.committed",
        "book.switch.between",
    ];
    for step in steps {
        let (counted, read, seen) = (counted.clone(), read.clone(), Arc::clone(&seen));
        points::arm(
            step,
            &partition,
            Arc::new(move |_| {
                let counts = counted(&read)?;
                seen.lock().unwrap().push((step, counts));
                Ok(())
            }),
        );
    }
    let mover = meridian_bor::partition_moves::Mover::new(
        vec![h.zero.1.database(0).clone(), h.zero.1.database(1).clone()],
        h.zero.1.database(0).groups(),
        meridian_bor::partition_moves::Settings {
            switch: std::time::Duration::from_secs(10),
            ..Default::default()
        },
        Arc::new(meridian_clock::SystemClock),
    );
    let ended = blocking(|| {
        mover.begin(&partition);
        mover.run(&partition, 0, &Asked::default())
    });
    for step in steps {
        points::disarm(step, &partition);
    }
    assert!(matches!(ended, Ok(Ended::Moved(..))), "{ended:?}");
    // Opened in the target, the source's copies not yet let go.
    let opened = blocking(|| counted(&read)).unwrap();
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen.iter().map(|(step, _)| *step).collect::<Vec<_>>(),
        steps,
        "every step of the switch read at"
    );
    for (step, counts) in seen.iter().chain([&("opened", opened)]) {
        assert!(
            counts.iter().all(|n| *n == 1),
            "{step}: the accounts read {counts:?} times"
        );
    }
    blocking(move || drop((h, mover)));
}

/// T1 (CI 43636763392): a partition made by a raise of `bor.partitions`
/// after the members started -- its head naming no writer group -- is named
/// by the control group's sweep, and an act on an account placed in it,
/// sent to the group that does not own it, is answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn t1_a_partition_made_by_a_raised_count_is_named_by_the_sweep_and_answered() {
    use meridian_bor::partition_moves::rebalance;
    let (url, migrated) = blocking(isolated);
    let (_broker, [(zero, zero_store), (one, one_store)], mover) =
        blocking(|| two_members_at(&url, 16));
    // The raise: its heads made after the members started, named by none.
    blocking(|| migrated.make_partitions(16).unwrap());
    let partition = (8..16)
        .map(meridian_bor::store::partition_named)
        .find(|partition| meridian_changes::groups::owner(partition, 2) == 1)
        .expect("a raised partition group 1 owns");
    assert_eq!(
        blocking(|| zero_store.tip(&partition))
            .unwrap()
            .unwrap()
            .writer_group,
        None
    );
    blocking(|| rebalance(&zero, zero_store.as_ref(), &mover));
    assert_eq!(
        blocking(|| zero_store.tip(&partition))
            .unwrap()
            .unwrap()
            .writer_group,
        Some(1),
        "the sweep named the raised partition's writer group"
    );
    let account = (0..)
        .map(|n| unique(&format!("ACC-T1-{n}")))
        .find(|a| meridian_bor::store::placement_of(a, 16) == partition)
        .unwrap();
    blocking(|| one_store.place_first(&account, &account)).unwrap();
    // Sent to group 0, which hands it to group 1 by the record's word.
    open(&zero, &account, "INS-AAPL").await;
    write_once(&zero, &account, 1).await.unwrap();
    assert_eq!(
        blocking(|| zero_store.partition_of(&account)).as_deref(),
        Some(partition.as_str())
    );
    blocking(move || drop((zero, one, zero_store, one_store, migrated)));
}

/// C8: a partition moved by hand back and forth is recorded each time, and
/// the sweep never undoes the last.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn review_c8_moves_by_hand_back_and_forth_are_each_recorded_and_kept() {
    use meridian_bor::partition_moves::{journal, rebalance, Asked, Ended};
    let (url, migrated) = blocking(isolated);
    let (_broker, [(zero, zero_store), (one, one_store)], mover) = blocking(|| two_members(&url));
    // A partition whose rendezvous owner is group 0.
    let partition = partition_of(0, 0);
    let by_hand = Asked {
        by_hand: true,
        moved_by: PERSON.into(),
        reason: "placed by hand".into(),
    };
    for target in [1, 0, 1] {
        let ended = blocking(|| {
            mover.begin(&partition);
            mover.run(&partition, target, &by_hand)
        })
        .unwrap();
        assert!(
            matches!(&ended, Ended::Moved(m) if m.to_group == target),
            "{ended:?}"
        );
        blocking(|| journal(&zero, zero_store.as_ref(), &ended));
    }
    blocking(|| {
        assert!(rebalance(&zero, zero_store.as_ref(), &mover).is_empty());
        assert_eq!(
            zero_store.tip(&partition).unwrap().unwrap().writer_group,
            Some(1)
        );
        let control = zero_store
            .journal(meridian_bor::store::CONTROL_ACCOUNT)
            .unwrap();
        let records = control
            .iter()
            .filter(|e| {
                e.idempotency_key
                    .starts_with(&format!("control-record:partition-moved:{partition}:"))
            })
            .count();
        assert_eq!(records, 3);
    });
    blocking(move || drop((zero, one, zero_store, one_store, migrated)));
}

/// No written head ever carries a move's pointer epoch
/// (`meridian_changes::moves::POINTER_EPOCH`, negative): a fresh install of
/// one writer group, and a database shaped as 3c left it (no writer group
/// named, no group's own epoch) migrated forward and started at two groups,
/// each commit normally, every head's epoch at least 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn review_no_written_head_carries_a_pointers_epoch() {
    let lowest = |url: &str| -> i64 {
        postgres::Client::connect(url, postgres::NoTls)
            .unwrap()
            .query_one("SELECT min(epoch) FROM book_partition_head", &[])
            .unwrap()
            .get(0)
    };
    // A fresh install, one group: its heads as made, before any writer
    // adopts, then written.
    let (url, migrated) = blocking(isolated);
    blocking(|| migrated.make_partitions(8).unwrap());
    assert!(blocking(|| lowest(&url)) >= 1);
    let (bus, _) = wired_with(Arc::clone(&migrated));
    let account = unique("ACC-FRESH");
    open(&bus, &account, "INS-AAPL").await;
    write_once(&bus, &account, 1).await.unwrap();
    assert!(blocking(|| lowest(&url)) >= 1);
    // 3c's shape, migrated forward and started at two groups.
    let (url, migrated) = blocking(isolated);
    blocking(|| {
        migrated.make_partitions(8).unwrap();
        postgres::Client::connect(&url, postgres::NoTls)
            .unwrap()
            .batch_execute(
                "UPDATE book_partition_head SET writer_group = NULL;
                 DELETE FROM book_writer_group;",
            )
            .unwrap();
        migrated.migrate(&meridian_clock::SystemClock).unwrap();
    });
    let (_broker, [(zero, _), (one, one_store)], _mover) = blocking(|| two_members(&url));
    let accounts = accounts_in("ACC-3C", "P4", 1, &one, one_store.as_ref()).await;
    write_once(&zero, &accounts[0], 1).await.unwrap();
    write_once(&one, &accounts[0], 2).await.unwrap();
    assert!(blocking(|| lowest(&url)) >= 1);
    blocking(move || drop((bus, migrated, zero, one, one_store)));
}

/// C7: an account moved to a partition, back, and to it again, by three
/// requests: three moves, each with its own close and open, and the
/// account where the last put it.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn review_c7_an_account_moved_back_to_a_partition_it_left_moves() {
    use meridian_bor::decide::Context;
    use meridian_bor::moves::move_account_with;
    use meridian_changes::moves::Settings;
    use meridian_domain::v1::MoveAccountRequest;
    let (url, migrated) = blocking(isolated);
    let (_broker, [(zero, zero_store), (one, one_store)], _mover) = blocking(|| two_members(&url));
    let ones: Vec<String> = (0..8)
        .map(meridian_bor::store::partition_named)
        .filter(|p| meridian_changes::groups::owner(p, 2) == 1)
        .collect();
    let (a, b) = (ones[0].clone(), ones[1].clone());
    let account = (0..)
        .map(|n| unique(&format!("ACC-C7-{n}")))
        .find(|x| meridian_bor::store::placement_of(x, 8) == a)
        .unwrap();
    blocking(|| one_store.place_first(&account, &account)).unwrap();
    open(&one, &account, "INS-AAPL").await;
    for (n, target) in [&b, &a, &b].into_iter().enumerate() {
        let request = MoveAccountRequest {
            account_id: account.clone(),
            target_partition: target.clone(),
            reason: format!("move {n}"),
        };
        let ctx = Context {
            acting_for: PERSON.into(),
            received_at_ns: 5_000 + n as i64,
            message_id: unique("msg-c7"),
            ..Default::default()
        };
        let moved = blocking(|| {
            move_account_with(
                &one,
                one_store.as_ref(),
                &ctx,
                &request,
                &Settings::default(),
            )
        })
        .unwrap();
        assert_eq!(&moved.to_partition, target, "move {n}: {moved:?}");
    }
    blocking(|| {
        assert_eq!(
            Store::partition_of(one_store.as_ref(), &account),
            Some(b.clone())
        );
        let opens = Store::journal(one_store.as_ref(), &account)
            .unwrap()
            .iter()
            .filter(|e| e.idempotency_key.starts_with("account-open:"))
            .count();
        assert_eq!(opens, 3);
    });
    blocking(move || drop((zero, one, zero_store, one_store, migrated)));
}

/// The Elle read (CI 43631481777): an account between its move's close and
/// its open across databases is read whole through either member -- never
/// empty where its new database holds nothing of it yet -- and the control
/// group's sweep opens it.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn review_elle_an_account_between_its_close_and_its_open_reads_whole() {
    use meridian_changes::groups::Groups;
    let urls: Vec<Scratch> = blocking(|| {
        (0..2)
            .map(|_| {
                let (url, migrated) = isolated();
                migrated.make_partitions(4).unwrap();
                url
            })
            .collect()
    });
    let spread = |group: u32| {
        let groups = Groups::new(group, 2).unwrap();
        let stores = urls
            .iter()
            .map(|url| {
                Arc::new(
                    PostgresStore::connect(url, 4)
                        .unwrap()
                        .with_groups(groups)
                        .with_partitions(4),
                )
            })
            .collect();
        Arc::new(meridian_bor::spread::Spread::new(stores, groups))
    };
    let (zero, one) = blocking(|| (spread(0), spread(1)));
    let groups = Groups::new(0, 2).unwrap();
    let account = (0..)
        .map(|n| unique(&format!("ACC-ELLE-{n}")))
        .find(|account| groups.owner(&meridian_bor::store::placement_of(account, 4)) == 1)
        .unwrap();
    blocking(|| one.place_first(&account, &account)).unwrap();
    let to = (0..4)
        .map(meridian_bor::store::partition_named)
        .find(|partition| groups.owner(partition) == 0)
        .unwrap();
    let bus = |name: &str, store: Arc<meridian_bor::spread::Spread>| {
        let bus = Arc::new(Bus::single(
            name,
            Arc::new(MemoryBackend::new()),
            Arc::new(meridian_clock::SystemClock),
        ));
        serve(bus.clone(), store, bus.clock());
        resolving_equities(&bus);
        bus
    };
    let (bus_zero, bus_one) = (bus("elle-0", zero.clone()), bus("elle-1", one.clone()));
    open(&bus_one, &account, &unique("INS-ELLE")).await;
    let request = MoveAccountRequest {
        account_id: account.clone(),
        target_partition: to.clone(),
        reason: "group 1 is busy".into(),
    };
    // Closed by group 1; its open, group 0's, never asked.
    let handed = send(
        &bus_one,
        meridian_bor::moves::MOVE_ACCOUNT,
        &request,
        Some(PERSON),
    )
    .await;
    assert!(handed.is_err(), "the open is the new partition's group's");
    let read = PositionsRead {
        scope: Scope::Everything,
        account_id: account.clone(),
        since: None,
        business_date: String::new(),
        at: None,
        limit: 500,
        cursor: String::new(),
    };
    blocking(|| {
        for (name, member) in [("zero", &zero), ("one", &one)] {
            let held = member.positions(&read).unwrap().records;
            assert!(
                !held.is_empty(),
                "{name} read the account as empty between its close and its open"
            );
            assert!(!member.journal(&account).unwrap().is_empty(), "{name}");
        }
    });
    // The control group's sweep opens it, in group 0's database.
    let opened = blocking(|| {
        meridian_bor::moves::finish_opens(
            &bus_zero,
            zero.as_ref(),
            &[zero.database(0).clone(), zero.database(1).clone()],
            &meridian_changes::moves::Settings::default(),
            meridian_clock::Clock::now_ns(&meridian_clock::SystemClock) + 1_000_000_000,
        )
    });
    assert_eq!(opened, 1);
    blocking(|| {
        assert_eq!(
            Store::partition_of(zero.as_ref(), &account),
            Some(to.clone())
        );
        let opens = zero
            .journal(&account)
            .unwrap()
            .iter()
            .filter(|e| e.idempotency_key.starts_with("account-open:"))
            .count();
        assert_eq!(opens, 1);
        for member in [&zero, &one] {
            assert!(!member.positions(&read).unwrap().records.is_empty());
        }
        // Nothing more owed.
        assert_eq!(
            meridian_bor::moves::finish_opens(
                &bus_zero,
                zero.as_ref(),
                &[zero.database(0).clone(), zero.database(1).clone()],
                &meridian_changes::moves::Settings::default(),
                meridian_clock::Clock::now_ns(&meridian_clock::SystemClock) + 1_000_000_000,
            ),
            0
        );
    });
    drop((bus_zero, bus_one));
}

/// A2: a writer group still writing a partition in a database past its
/// own (a raised database count) follows that database's timeline too: a
/// new timeline there is reported moved and raises its epoch there.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn review_a2_a_group_writing_away_follows_that_databases_timeline() {
    use meridian_changes::groups::Groups;
    use meridian_changes::timeline::Timeline;
    let urls: Vec<Scratch> = blocking(|| {
        (0..2)
            .map(|_| {
                let (url, migrated) = isolated();
                migrated.make_partitions(8).unwrap();
                apart(&url);
                url
            })
            .collect()
    });
    let partition = partition_of(1, 0);
    // Group 1's partition written in database 0, its home database 1's
    // head behind: the count of databases was raised since.
    blocking(|| {
        postgres::Client::connect(&urls[0], postgres::NoTls)
            .unwrap()
            .execute(
                "UPDATE book_partition_head SET sequence = 5, writer_group = 1 WHERE partition = $1",
                &[&partition],
            )
            .unwrap();
    });
    let groups = Groups::new(1, 2).unwrap();
    let one = blocking(|| {
        let stores: Vec<Arc<PostgresStore>> = urls
            .iter()
            .map(|url| {
                let store = PostgresStore::connect(url, 4)
                    .unwrap()
                    .with_groups(groups)
                    .with_partitions(8);
                store.pin_owners().unwrap();
                Arc::new(store)
            })
            .collect();
        meridian_bor::spread::Spread::new(stores, groups)
    });
    blocking(|| {
        Store::adopt(&one).unwrap();
        Store::follow_timeline(&one).unwrap();
        assert_eq!(Store::follow_timeline(&one).unwrap(), Timeline::Same);
        // Database 0 on a new timeline.
        postgres::Client::connect(&urls[0], postgres::NoTls)
            .unwrap()
            .execute(
                "UPDATE book_partition_head SET database_identity = 'another/00000001'",
                &[],
            )
            .unwrap();
        let found = Store::follow_timeline(&one).unwrap();
        assert!(matches!(found, Timeline::Moved(_)), "{found:?}");
    });
    blocking(move || drop(one));
}

/// m9: a group's look at its own database's heads -- its epoch adopted, its
/// timeline followed -- never names a partition's owner from a head that
/// database does not write (PR CI 43648472142, tolerant: "the book holds no
/// account ACC-DRIVE-01"). A partition moved by hand to group 0 before the
/// count of databases was raised is written in database 0; database 1, which
/// group 1 writes, holds its head at 0, named its rendezvous owner, group 1.
/// Group 1's member read that head as the owner's and took the partition for
/// its own: it read the account's journal from database 1, where none is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m9_a_groups_own_heads_never_name_an_owner_its_database_does_not_write() {
    use meridian_changes::groups::Groups;
    let urls: Vec<Scratch> = blocking(|| {
        (0..2)
            .map(|_| {
                let (url, migrated) = isolated();
                migrated.make_partitions(8).unwrap();
                apart(&url);
                url
            })
            .collect()
    });
    let partition = partition_of(1, 0);
    assert_eq!(meridian_changes::groups::owner(&partition, 2), 1);
    // Moved by hand to group 0 at one database and written on there.
    blocking(|| {
        postgres::Client::connect(&urls[0], postgres::NoTls)
            .unwrap()
            .execute(
                "UPDATE book_partition_head SET sequence = 5, writer_group = 0 WHERE partition = $1",
                &[&partition],
            )
            .unwrap();
    });
    let groups = Groups::new(1, 2).unwrap();
    let one = blocking(|| {
        let stores: Vec<Arc<PostgresStore>> = urls
            .iter()
            .map(|url| {
                let store = PostgresStore::connect(url, 4)
                    .unwrap()
                    .with_groups(groups)
                    .with_partitions(8);
                store.pin_owners().unwrap();
                Arc::new(store)
            })
            .collect();
        PostgresStore::read_owners_from(&stores).unwrap();
        meridian_bor::spread::Spread::new(stores, groups)
    });
    blocking(|| {
        assert_eq!(groups.owner(&partition), 0, "named by its furthest head");
        assert_eq!(one.of(&partition), 0, "written in database 0");
        Store::adopt(&one).unwrap();
        assert_eq!(groups.owner(&partition), 0, "after the epoch is adopted");
        Store::follow_timeline(&one).unwrap();
        Store::head_epoch(&one).unwrap();
        assert_eq!(
            groups.owner(&partition),
            0,
            "after the timeline is looked at"
        );
        assert_eq!(one.of(&partition), 0, "still written in database 0");
    });
    blocking(move || drop(one));
}

/// m9 (part 8's residue): an account moved across databases is in every
/// read of all accounts between its close and its open -- read at its
/// source, whose close names a target that holds nothing of it yet -- and
/// once: never at its source and its target both, its open done and its
/// source's copy not yet let go.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn m9_an_account_between_its_close_and_its_open_is_in_a_read_of_every_account_once() {
    use meridian_changes::groups::Groups;
    let urls: Vec<Scratch> = blocking(|| {
        (0..2)
            .map(|_| {
                let (url, migrated) = isolated();
                migrated.make_partitions(4).unwrap();
                url
            })
            .collect()
    });
    let spread = |group: u32| {
        let groups = Groups::new(group, 2).unwrap();
        let stores = urls
            .iter()
            .map(|url| {
                Arc::new(
                    PostgresStore::connect(url, 4)
                        .unwrap()
                        .with_groups(groups)
                        .with_partitions(4),
                )
            })
            .collect();
        Arc::new(meridian_bor::spread::Spread::new(stores, groups))
    };
    let (zero, one) = blocking(|| (spread(0), spread(1)));
    let groups = Groups::new(0, 2).unwrap();
    let account = (0..)
        .map(|n| unique(&format!("ACC-GAP-{n}")))
        .find(|account| groups.owner(&meridian_bor::store::placement_of(account, 4)) == 1)
        .unwrap();
    blocking(|| one.place_first(&account, &account)).unwrap();
    let to = (0..4)
        .map(meridian_bor::store::partition_named)
        .find(|partition| groups.owner(partition) == 0)
        .unwrap();
    let bus = |name: &str, store: Arc<meridian_bor::spread::Spread>| {
        let bus = Arc::new(Bus::single(
            name,
            Arc::new(MemoryBackend::new()),
            Arc::new(meridian_clock::SystemClock),
        ));
        serve(bus.clone(), store, bus.clock());
        resolving_equities(&bus);
        bus
    };
    let (bus_zero, bus_one) = (bus("gap-0", zero.clone()), bus("gap-1", one.clone()));
    open(&bus_one, &account, &unique("INS-GAP")).await;
    let everyone = PositionsRead {
        scope: Scope::Everything,
        account_id: String::new(),
        since: None,
        business_date: String::new(),
        at: None,
        limit: 500,
        cursor: String::new(),
    };
    let times = |member: &meridian_bor::spread::Spread| {
        member
            .positions(&everyone)
            .unwrap()
            .records
            .iter()
            .filter(|p| p.account_id == account)
            .count()
    };
    blocking(|| assert_eq!((times(&zero), times(&one)), (1, 1), "before its move"));
    let request = MoveAccountRequest {
        account_id: account.clone(),
        target_partition: to.clone(),
        reason: "group 1 is busy".into(),
    };
    // Closed by group 1; its open, group 0's, never asked.
    let handed = send(
        &bus_one,
        meridian_bor::moves::MOVE_ACCOUNT,
        &request,
        Some(PERSON),
    )
    .await;
    assert!(handed.is_err(), "the open is the new partition's group's");
    blocking(|| {
        assert_eq!(
            (times(&zero), times(&one)),
            (1, 1),
            "between its close and its open"
        )
    });
    // The control group's sweep opens it, in group 0's database; the
    // source's copy is let go after.
    let opened = blocking(|| {
        meridian_bor::moves::finish_opens(
            &bus_zero,
            zero.as_ref(),
            &[zero.database(0).clone(), zero.database(1).clone()],
            &meridian_changes::moves::Settings::default(),
            meridian_clock::Clock::now_ns(&meridian_clock::SystemClock) + 1_000_000_000,
        )
    });
    assert_eq!(opened, 1);
    blocking(|| assert_eq!((times(&zero), times(&one)), (1, 1), "after its open"));
    drop((bus_zero, bus_one));
}

// ---- Part 12 (c30d-m12): a short switch between databases ----------------

/// Every row `url`'s database holds of `accounts` in each table a move
/// between databases copies beside their entries, as text, in order.
fn projected(url: &str, accounts: &[String]) -> Vec<(&'static str, Vec<String>)> {
    const TABLES: &[&str] = &[
        "book_account",
        "book_snapshot",
        "book_position",
        "book_lot",
        "book_pending",
        "book_break",
        "book_figures",
        "book_attributes",
        "book_order",
        "book_placement",
        "book_execution",
        "book_unplaced",
        "book_locate",
        "book_instruction",
        "book_change_request",
        "book_order_entry",
        "book_booking",
        "book_alert",
        "book_block_member",
    ];
    let mut admin = postgres::Client::connect(url, postgres::NoTls).unwrap();
    TABLES
        .iter()
        .map(|table| {
            let rows = admin
                .query(
                    &format!("SELECT t::text FROM {table} t WHERE account_id = ANY($1) ORDER BY 1"),
                    &[&accounts],
                )
                .unwrap()
                .iter()
                .map(|row| row.get(0))
                .collect();
            (*table, rows)
        })
        .collect()
}

/// Writes to `accounts` through `bus` until `stop`, as [`writing`]: how
/// many, those refused, and the longest any one waited for its answer.
fn writing_timed(
    bus: Arc<Bus>,
    accounts: Vec<String>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> tokio::task::JoinHandle<(u64, Vec<String>, std::time::Duration)> {
    tokio::spawn(async move {
        let (mut written, mut refused) = (0, Vec::new());
        let mut longest = std::time::Duration::ZERO;
        let mut n = 0u64;
        while !stop.load(Ordering::SeqCst) {
            n += 1;
            let account = &accounts[(n as usize) % accounts.len()];
            let began = std::time::Instant::now();
            match write_once(&bus, account, n).await {
                Ok(_) => written += 1,
                Err(failed) => refused.push(failed),
            }
            longest = longest.max(began.elapsed());
        }
        (written, refused, longest)
    })
}

/// A partition the size CI's failover run moves (35 accounts, more than
/// 11,000 entries, orders among them) switched between databases under
/// write load (step 3d; section 6.8, "the hold is bounded, and nothing is
/// refused inside it"; CI 43675491409 held P0's writes 5.4 s): its hold --
/// from the partition's lock taken to the switch's answer -- stays under a
/// second, because what grows with the partition is copied before the lock
/// and only what changed since under it; no write is refused; the target's
/// chain to the close is the source's, digest for digest; and moved back
/// under the same load, at the switch every account's rows in the target
/// are the source's, none stale (the review's C6), and a read of every
/// account reads each once (C5).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn m12_a_partition_the_size_ci_moves_switches_between_databases_inside_a_short_hold() {
    use meridian_bor::order_service::*;
    use meridian_bor::partition_moves::{Asked, Ended};
    use meridian_changes::moves::points;
    const ACCOUNTS: usize = 35;
    const ORDERS: u64 = 40;
    const WRITES: u64 = 285;
    let h = blocking(|| two_databases_snapshotting(Some(20)));
    meridian_domain::money::learn("USD", "INS-CASH-USD");
    let partition = partition_of(1, 1);
    let accounts = accounts_in("ACC-M12", &partition, ACCOUNTS, &h.one.0, h.one.1.as_ref()).await;
    let built = std::time::Instant::now();
    let building: Vec<_> = accounts
        .iter()
        .cloned()
        .map(|account| {
            let bus = h.one.0.clone();
            tokio::spawn(async move {
                write_once(&bus, &account, 1).await.unwrap();
                for n in 0..ORDERS {
                    let proposal = ProposeOrderRequest {
                        account_id: account.clone(),
                        instrument_id: "INS-AAPL".into(),
                        sleeve: "core".into(),
                        side: Side::Buy as i32,
                        quantity: d("1"),
                        order_type: OrderType::Limit as i32,
                        limit_price: usd("227.50"),
                        time_in_force: TimeInForce::Day as i32,
                        idempotency_key: unique(&format!("m12-{n}")),
                        ..Default::default()
                    };
                    send_order(&bus, PROPOSE_ORDER, &proposal, Some(PERSON), None)
                        .await
                        .unwrap();
                }
                for n in 0..WRITES {
                    write_once(&bus, &account, n).await.unwrap();
                }
            })
        })
        .collect();
    for task in building {
        task.await.unwrap();
    }
    let entries = blocking(|| numbered(&h.urls[1], &partition)).len();
    assert!(entries > 11_000, "{entries} entries: the size CI moves");
    eprintln!(
        "m12: {ACCOUNTS} accounts, {entries} entries built in {:?}",
        built.elapsed()
    );
    let read = PositionsRead {
        scope: Scope::Everything,
        account_id: String::new(),
        since: None,
        business_date: String::new(),
        at: None,
        limit: 500,
        cursor: String::new(),
    };
    let counted = {
        let reader = Arc::clone(&h.zero.1);
        let accounts = accounts.clone();
        move || -> Result<Vec<usize>, String> {
            let everyone = reader
                .positions(&read)
                .map_err(|failed| failed.to_string())?
                .records;
            Ok(accounts
                .iter()
                .map(|account| everyone.iter().filter(|p| p.account_id == *account).count())
                .collect())
        }
    };
    // Written while it moves, as CI's driver writes: a few of its accounts,
    // through either member.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let written: Vec<String> = accounts[..3].to_vec();
    let load = [
        writing_timed(h.one.0.clone(), written.clone(), stop.clone()),
        writing_timed(h.zero.0.clone(), written.clone(), stop.clone()),
    ];
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let moved = std::time::Instant::now();
    let ended = blocking(|| {
        h.mover.begin(&partition);
        h.mover.run(&partition, 0, &Asked::default())
    });
    let took = moved.elapsed();
    let times = h.mover.last_switch().expect("a switch was made");
    eprintln!("m12: moved in {took:?}; its switch {times:?}");
    assert!(matches!(ended, Ok(Ended::Moved(..))), "{ended:?}");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let once = blocking(&counted).unwrap();
    assert!(
        once.iter().all(|n| *n == 1),
        "moved: the accounts read {once:?} times"
    );
    // The target's chain to the close, the source's: each taken again.
    blocking(|| {
        let fresh = |d: usize| PostgresStore::connect(&h.urls[d], 1).unwrap();
        let closing = fresh(1).tip(&partition).unwrap().unwrap().head;
        assert!(closing as usize >= entries, "closed at {closing}");
        let ours = fresh(0).chain_of(&partition, 0, &[], closing).unwrap();
        let theirs = fresh(1).chain_of(&partition, 0, &[], closing).unwrap();
        assert!(ours.is_ok(), "{ours:?}");
        assert_eq!(ours, theirs, "the digests at the close");
        assert!(fresh(0).tip(&partition).unwrap().unwrap().head > closing);
    });
    // Back again under the same load: at the switch, the source held, every
    // account's rows in the target are the source's, and each is read once.
    let seen = Arc::new(std::sync::Mutex::new(None));
    {
        // The URLs' text: the armed point's closure stays in a static map
        // past a failed test, and a Scratch there would never be dropped
        // (review c5p L1).
        let (urls, accounts, counted, seen) = (
            h.urls.iter().map(|url| url.to_string()).collect::<Vec<_>>(),
            accounts.clone(),
            counted.clone(),
            Arc::clone(&seen),
        );
        points::arm(
            "book.switch.copied",
            &partition,
            Arc::new(move |_| {
                let source = projected(&urls[0], &accounts);
                let target = projected(&urls[1], &accounts);
                *seen.lock().unwrap() = Some((source, target, counted()?));
                Ok(())
            }),
        );
    }
    let back = blocking(|| {
        h.mover.begin(&partition);
        h.mover.run(
            &partition,
            1,
            &Asked {
                by_hand: true,
                moved_by: PERSON.into(),
                reason: "back again".into(),
            },
        )
    });
    points::disarm("book.switch.copied", &partition);
    let back_times = h.mover.last_switch().expect("a switch was made");
    eprintln!("m12: moved back; its switch {back_times:?}");
    assert!(matches!(back, Ok(Ended::Moved(..))), "{back:?}");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    stop.store(true, Ordering::SeqCst);
    let (mut count, mut longest) = (0, std::time::Duration::ZERO);
    for task in load {
        let (written, refused, waited) = task.await.unwrap();
        assert!(refused.is_empty(), "refused inside the hold: {refused:?}");
        count += written;
        longest = longest.max(waited);
    }
    eprintln!("m12: {count} written under the two moves, the longest answered in {longest:?}");
    assert!(count > 10, "{count} written under the moves");
    let (source, target, counts) = seen.lock().unwrap().take().expect("read at the switch");
    for ((table, theirs), (_, ours)) in source.iter().zip(&target) {
        assert_eq!(
            ours, theirs,
            "{table}: the target's rows at the switch are not the source's"
        );
    }
    let orders = &source.iter().find(|(t, _)| *t == "book_order").unwrap().1;
    assert_eq!(orders.len() as u64, ACCOUNTS as u64 * ORDERS);
    assert!(
        counts.iter().all(|n| *n == 1),
        "at the switch: the accounts read {counts:?} times"
    );
    let once = blocking(&counted).unwrap();
    assert!(
        once.iter().all(|n| *n == 1),
        "moved back: the accounts read {once:?} times"
    );
    // The hold: what the writers waited, at most.
    for (which, times) in [("there", &times), ("back", &back_times)] {
        assert_eq!(times.placed, ACCOUNTS, "{which}: {times:?}");
        assert!(
            times.copied < ACCOUNTS,
            "{which}: only the accounts written since were copied under the lock: {times:?}"
        );
    }
    assert!(
        times.held < std::time::Duration::from_secs(1),
        "the switch held the partition {:?}: {times:?}",
        times.held
    );
    assert!(
        longest < std::time::Duration::from_secs(5),
        "a write waited {longest:?}, past moves.holdSeconds"
    );
    blocking(move || drop(h));
}

/// A read naming an account, made through a member whose view of the
/// writer groups was read before its partition moved to another group's
/// database and not since, is answered where the partition is written now
/// (step 3d; sections 5 and 6.4; merge-queue run 45104369464, Elle: a read
/// 250 ms after two acknowledged appends answered without them, just after
/// its partition moved from database 0 to database 1). A reader reads the
/// writer groups at its start and every few seconds; its `read_after` is
/// held to every database's heads, so it passed on the target's, while the
/// read went by the view to the source, whose head is the move's pointer
/// and whose copy closed at the switch. Read here as CI's was: after the
/// switch and before the source let its accounts go, which a move does
/// next, and the sweep where that failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_read_naming_an_account_follows_its_partition_past_a_stale_view_of_its_owner() {
    use meridian_bor::order_service::*;
    use meridian_bor::postgres::Switched;
    use meridian_bor::store::OrdersRead;
    use meridian_changes::groups::Groups;
    let h = blocking(two_databases);
    meridian_domain::money::learn("USD", "INS-CASH-USD");
    // Group 0's, in database 0, as CI's P0 was.
    let partition = partition_of(0, 1);
    let account = accounts_in("ACC-STALE", &partition, 1, &h.zero.0, h.zero.1.as_ref())
        .await
        .remove(0);
    // Group 1's writer has written in its own database: a move to it waits
    // for that.
    accounts_in(
        "ACC-STALE-ONE",
        &partition_of(1, 0),
        1,
        &h.one.0,
        h.one.1.as_ref(),
    )
    .await;
    let propose = |n: u32| ProposeOrderRequest {
        account_id: account.clone(),
        instrument_id: "INS-AAPL".into(),
        sleeve: "core".into(),
        side: Side::Buy as i32,
        quantity: d("1"),
        order_type: OrderType::Limit as i32,
        limit_price: usd("227.50"),
        time_in_force: TimeInForce::Day as i32,
        idempotency_key: unique(&format!("stale-{n}")),
        ..Default::default()
    };
    send_order(&h.zero.0, PROPOSE_ORDER, &propose(0), Some(PERSON), None)
        .await
        .unwrap();
    // A reader member: its own view of the writer groups, read once.
    let (reader, view) = blocking(|| {
        let groups = Groups::new(0, 2).unwrap();
        let stores: Vec<Arc<PostgresStore>> = h
            .urls
            .iter()
            .map(|url| {
                Arc::new(
                    PostgresStore::connect(url, 2)
                        .unwrap()
                        .with_groups(groups)
                        .with_partitions(8),
                )
            })
            .collect();
        PostgresStore::read_owners_from(&stores).unwrap();
        let spread = Arc::new(meridian_bor::spread::Spread::new(stores, groups));
        let reader = meridian_bor::reader::ReaderStore::new(
            spread.clone(),
            None,
            1 << 20,
            Arc::new(meridian_bor::store::Mark::new),
        );
        (reader, spread)
    });
    let read = OrdersRead {
        scope: Scope::Everything,
        account_id: account.clone(),
        since: None,
        limit: 100,
        cursor: String::new(),
        execution_state: 0,
    };
    let orders = |reader: &meridian_bor::reader::ReaderStore| {
        meridian_bus::read_after::begin_read();
        Store::orders(reader, &read).unwrap().orders.len()
    };
    // Read where its owner's home is, database 0, from here on.
    assert_eq!(blocking(|| orders(&reader)), 1);
    assert_eq!(blocking(|| view.of(&partition)), 0);
    // A move's warm and switch, as the mover makes them; the source's
    // accounts not let go yet.
    let switched = blocking(|| {
        let (source, to) = (h.zero.1.database(0), h.zero.1.database(1));
        let warmed = to.copy_partition_from(source, &partition, 0).unwrap();
        source.switch_partition(
            to,
            &partition,
            1,
            std::time::Duration::from_secs(10),
            false,
            warmed,
        )
    });
    assert!(
        matches!(switched, Ok(Switched::Moved { .. })),
        "{switched:?}"
    );
    send_order(&h.one.0, PROPOSE_ORDER, &propose(1), Some(PERSON), None)
        .await
        .unwrap();
    blocking(|| {
        // What the read names: the head its order was committed at, in
        // database 1, past the source's close.
        let written = h.one.1.database(1).tip(&partition).unwrap().unwrap();
        assert_eq!(written.writer_group, Some(1));
        let closed = h.one.1.database(0).tip(&partition).unwrap().unwrap();
        assert_eq!(closed.epoch, meridian_changes::moves::POINTER);
        assert!(written.head > closed.head, "{written:?} {closed:?}");
        // The reader's `read_after` passes: every database's heads.
        assert!(Store::heads(view.as_ref()).unwrap()[&partition] >= written.head);
        // Its view of the owner is not read again by any timer here.
        assert_eq!(
            orders(&reader),
            2,
            "a read after the order was acknowledged answered without it"
        );
        // By the heads it read, for itself alone: the member's view is its
        // sweep's to read again (the review of 2026-11-23, finding 3).
        assert_eq!(view.of(&partition), 0);
        let journal = Store::journal(view.as_ref(), &account).unwrap();
        assert_eq!(
            journal.last().map(|entry| entry.last_sequence),
            Some(written.head),
            "its journal, to the head it is written at"
        );
    });
    blocking(move || drop((reader, view, h)));
}

/// A member's stores over `urls`, one per database, sharing `groups`'s
/// view of the writer groups: a reader's, which acts on nothing.
fn reading(urls: &[Scratch], groups: meridian_changes::groups::Groups) -> Vec<Arc<PostgresStore>> {
    urls.iter()
        .map(|url| {
            Arc::new(
                PostgresStore::connect(url, 2)
                    .unwrap()
                    .with_groups(groups)
                    .with_partitions(8),
            )
        })
        .collect()
}

/// `partition` moved from database 0 to group 1's, database 1, as the mover
/// warms and switches it; the source's accounts not let go.
fn switched_to_one(h: &TwoDatabases, partition: &str) {
    use meridian_bor::postgres::Switched;
    let (source, to) = (h.zero.1.database(0), h.zero.1.database(1));
    let warmed = to.copy_partition_from(source, partition, 0).unwrap();
    let switched = source.switch_partition(
        to,
        partition,
        1,
        std::time::Duration::from_secs(10),
        false,
        warmed,
    );
    assert!(
        matches!(switched, Ok(Switched::Moved { .. })),
        "{switched:?}"
    );
}

/// A read answered from a reader's replicas never writes the member's view
/// of the writer groups (the review of 2026-11-23, finding 3): the replicas
/// share that view with the member's own stores, and one behind a move
/// holds the heads as they were before it. Following a partition there by
/// reading its owner again wrote the owner before the move into the view
/// every read of the member's goes by. A read is routed by the heads it
/// just read, for itself alone: the replica's answer is its own state, and
/// the view still names the owner the record does.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_read_from_a_replica_behind_a_move_never_writes_the_members_view_of_the_owner() {
    use meridian_changes::groups::Groups;
    // The record, and what its replicas hold: the state before the move.
    let (h, behind) = blocking(|| (two_databases(), two_databases()));
    let partition = partition_of(0, 2);
    let account = accounts_in("ACC-BEHIND", &partition, 1, &h.zero.0, h.zero.1.as_ref())
        .await
        .remove(0);
    blocking(|| behind.zero.1.place_first(&account, &account)).unwrap();
    open(&behind.zero.0, &account, "INS-AAPL").await;
    accounts_in(
        "ACC-BEHIND-ONE",
        &partition_of(1, 0),
        1,
        &h.one.0,
        h.one.1.as_ref(),
    )
    .await;
    blocking(|| switched_to_one(&h, &partition));
    write_once(&h.one.0, &account, 1).await.unwrap();
    let (reader, groups, record) = blocking(|| {
        let groups = Groups::new(0, 2).unwrap();
        let primary = reading(&h.urls, groups);
        PostgresStore::read_owners_from(&primary).unwrap();
        let record = Arc::new(meridian_bor::spread::Spread::new(primary, groups));
        let replica = Arc::new(meridian_bor::spread::Spread::new(
            reading(&behind.urls, groups),
            groups,
        ));
        let reader = meridian_bor::reader::ReaderStore::new(
            record.clone(),
            Some(replica as Arc<dyn Store>),
            1 << 20,
            Arc::new(meridian_bor::store::Mark::new),
        );
        (reader, groups, record)
    });
    blocking(|| {
        assert_eq!(groups.owner(&partition), 1, "the member's view, read after");
        // The record's past, from the replicas.
        meridian_bus::read_after::begin_read();
        let past = Store::journal(&reader, &account).unwrap();
        assert_eq!(
            groups.owner(&partition),
            1,
            "a read from a replica behind the move wrote the owner before it into the member's view"
        );
        assert_eq!(record.of(&partition), 1);
        assert_eq!(
            past.len(),
            1,
            "the replica's own state, the account as it was before the move: {past:?}"
        );
        // And the record's, where the partition is written now.
        assert_eq!(Store::journal(record.as_ref(), &account).unwrap().len(), 2);
    });
    blocking(move || drop((reader, record, h, behind)));
}

/// A read naming an account asks one head: its partition's, in the database
/// the member's view sends it to, and no other's where that one writes it
/// (the review of 2026-11-23, findings 4 and 7: every database was asked
/// before every such read).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_read_naming_an_account_asks_one_head_where_its_partition_is_written() {
    use meridian_changes::groups::Groups;
    let h = blocking(two_databases);
    let partition = partition_of(0, 0);
    let account = accounts_in("ACC-ONE-HEAD", &partition, 1, &h.zero.0, h.zero.1.as_ref())
        .await
        .remove(0);
    blocking(|| {
        let groups = Groups::new(0, 2).unwrap();
        let stores = reading(&h.urls, groups);
        PostgresStore::read_owners_from(&stores).unwrap();
        let view = meridian_bor::spread::Spread::new(stores.clone(), groups);
        let asked = || stores.iter().map(|store| store.heads_asked()).sum::<u64>();
        // Read where its owner's home is from here on.
        assert_eq!(Store::book(&view, &account).unwrap().entries.len(), 1);
        let before = asked();
        assert_eq!(Store::book(&view, &account).unwrap().entries.len(), 1);
        assert_eq!(asked() - before, 1, "the heads a read of its book asked");
        let before = asked();
        assert_eq!(Store::journal(&view, &account).unwrap().len(), 1);
        assert_eq!(asked() - before, 1, "the heads a read of its journal asked");
    });
    blocking(move || drop(h));
}

/// A database that cannot be reached slows no read of a partition written
/// in another, and a read whose partition it may hold is refused
/// UNAVAILABLE at once, never answered from the copy a move let go (the
/// review of 2026-11-23, finding 4: every database's head was asked before
/// every read naming an account, so one database's outage held each such
/// read for its pool's 30 s and then answered it by the view). Unreachable
/// here as a pool finds it: every connection held, the next checkout
/// waiting.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_database_out_of_reach_slows_no_read_of_a_partition_written_in_another() {
    use meridian_bor::store::StoreError;
    use meridian_changes::groups::Groups;
    let h = blocking(two_databases);
    let (stays, moves) = (partition_of(0, 0), partition_of(0, 1));
    assert_ne!(stays, moves);
    let here = accounts_in("ACC-REACH", &stays, 1, &h.zero.0, h.zero.1.as_ref())
        .await
        .remove(0);
    let gone = accounts_in("ACC-REACH-GONE", &moves, 1, &h.zero.0, h.zero.1.as_ref())
        .await
        .remove(0);
    accounts_in(
        "ACC-REACH-ONE",
        &partition_of(1, 0),
        1,
        &h.one.0,
        h.one.1.as_ref(),
    )
    .await;
    blocking(|| {
        // A reader member: its view of the writer groups read once, before
        // the move.
        let groups = Groups::new(0, 2).unwrap();
        let stores = reading(&h.urls, groups);
        PostgresStore::read_owners_from(&stores).unwrap();
        let view = meridian_bor::spread::Spread::new(stores.clone(), groups);
        for account in [&here, &gone] {
            assert_eq!(Store::book(&view, account).unwrap().entries.len(), 1);
        }
        switched_to_one(&h, &moves);
        // Database 1 out of this member's reach: both connections of its
        // pool taken, each waiting on a lock held here.
        let mut admin = postgres::Client::connect(&h.urls[1], postgres::NoTls).unwrap();
        admin
            .batch_execute("BEGIN; LOCK TABLE book_partition_head IN ACCESS EXCLUSIVE MODE")
            .unwrap();
        let held: Vec<_> = (0..2)
            .map(|_| {
                let (store, partition) = (stores[1].clone(), moves.clone());
                std::thread::spawn(move || drop(store.tip(&partition)))
            })
            .collect();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let started = std::time::Instant::now();
        let read = Store::book(&view, &here).map(|book| book.entries.len());
        let took = started.elapsed();
        assert_eq!(read.ok(), Some(1));
        assert!(
            took < std::time::Duration::from_secs(1),
            "a read of a partition database 0 writes waited {took:?} on database 1"
        );
        // Its partition's head in database 0 a move's pointer, and the
        // database it may have gone to not read: refused, within the bound.
        let started = std::time::Instant::now();
        let refused = Store::book(&view, &gone).map(|book| book.entries.len());
        let took = started.elapsed();
        assert!(
            matches!(refused, Err(StoreError::Unavailable(_))),
            "{refused:?}"
        );
        assert!(
            took < std::time::Duration::from_secs(3),
            "refused only after {took:?}"
        );
        admin.batch_execute("ROLLBACK").unwrap();
        for holder in held {
            holder.join().unwrap();
        }
        // In reach again: read where it is written, the view as it was.
        assert_eq!(Store::book(&view, &gone).unwrap().entries.len(), 1);
        assert_eq!(groups.owner(&moves), 0);
    });
    blocking(move || drop(h));
}

/// A writer's books held in memory are dropped when its view of the writer
/// groups changes (the review of 2026-11-23, finding 5): a partition moved
/// to another group, written there, and moved back is this member's to
/// write again, and the book it held from before the move is not the
/// account's any more.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_writers_held_book_is_dropped_when_its_view_of_the_owners_changes() {
    use meridian_bor::partition_moves::{Asked, Ended};
    let h = blocking(two_databases);
    let partition = partition_of(0, 0);
    let account = accounts_in("ACC-HELD", &partition, 1, &h.zero.0, h.zero.1.as_ref())
        .await
        .remove(0);
    accounts_in(
        "ACC-HELD-ONE",
        &partition_of(1, 0),
        1,
        &h.one.0,
        h.one.1.as_ref(),
    )
    .await;
    let writer = blocking(|| {
        let writer =
            meridian_bor::writer::Writer::new(h.zero.1.clone() as Arc<dyn Store>, h.zero.0.clone());
        assert_eq!(Store::book(&writer, &account).unwrap().entries.len(), 1);
        assert_eq!(writer.held().0, 1, "held from here on");
        writer
    });
    let moved = |to: u32| {
        blocking(|| {
            h.mover.begin(&partition);
            let ended = h.mover.run(
                &partition,
                to,
                &Asked {
                    by_hand: true,
                    moved_by: PERSON.into(),
                    reason: "there and back".into(),
                },
            );
            // Each member's look at the writer groups, as its sweep makes it.
            for member in [&h.zero.1, &h.one.1] {
                PostgresStore::read_owners_from(&[
                    member.database(0).clone(),
                    member.database(1).clone(),
                ])
                .unwrap();
            }
            ended
        })
    };
    let away = moved(1);
    assert!(matches!(away, Ok(Ended::Moved(..))), "{away:?}");
    write_once(&h.one.0, &account, 1).await.unwrap();
    let back = moved(0);
    assert!(matches!(back, Ok(Ended::Moved(..))), "{back:?}");
    blocking(|| {
        assert!(Store::writes(&writer, &account));
        assert_eq!(
            Store::book(&writer, &account).unwrap().entries.len(),
            2,
            "the book held from before its partition moved away, without what was written there"
        );
    });
    blocking(move || drop((writer, h)));
}

/// A head left written behind a move's pointers is never read as the
/// partition's (the review of 2026-11-23 of this routing, finding 1). A
/// move home within one group whose source's commit failed leaves that
/// source's head written for good, at the sequence it closed at. Moved on
/// to another group's database and stopped between its two heads -- the
/// source let go at M, a pointer; the target a pointer until it is opened
/// -- the partition's only written head is that one, behind M, and the
/// read went to it as the written head furthest on: answered without
/// everything written since. A written head is the partition's only at or
/// past every pointer read with it; else the read goes as where every head
/// is a pointer, by the view.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_head_left_written_behind_a_moves_pointers_is_never_read_as_the_partitions() {
    use meridian_bor::postgres::Switched;
    use meridian_changes::groups::Groups;
    use meridian_changes::moves::points;
    // Group 0's database, group 1's, and one neither's home is in.
    let h = blocking(|| databases(3, None));
    let partition = partition_of(0, 3);
    let account = accounts_in("ACC-LEFT", &partition, 1, &h.zero.0, h.zero.1.as_ref())
        .await
        .remove(0);
    accounts_in(
        "ACC-LEFT-ONE",
        &partition_of(1, 0),
        1,
        &h.one.0,
        h.one.1.as_ref(),
    )
    .await;
    for n in 0..2 {
        write_once(&h.zero.0, &account, n).await.unwrap();
    }
    blocking(|| {
        // Database 2's head of it left written at 1, as a move home that
        // failed at its source leaves one.
        let mut admin = postgres::Client::connect(&h.urls[2], postgres::NoTls).unwrap();
        let left = admin
            .execute(
                "UPDATE book_partition_head SET sequence = 1 WHERE partition = $1 AND epoch <> -1",
                &[&partition],
            )
            .unwrap();
        assert_eq!(left, 1);
        // Its move to group 1's database, stopped between its two heads.
        points::arm(
            "book.switch.between",
            &partition,
            Arc::new(|_| Err("stopped between the two databases".into())),
        );
        let (source, to) = (h.zero.1.database(0), h.zero.1.database(1));
        let warmed = to.copy_partition_from(source, &partition, 0).unwrap();
        let switched = source.switch_partition(
            to,
            &partition,
            1,
            std::time::Duration::from_secs(10),
            false,
            warmed,
        );
        points::disarm("book.switch.between", &partition);
        assert!(
            matches!(switched, Ok(Switched::LetGo { .. })),
            "{switched:?}"
        );
        let tips = heads_of(&h.urls, &partition);
        let pointer = meridian_changes::moves::POINTER;
        assert!(tips[0].epoch == pointer && tips[0].head >= 3, "{tips:?}");
        assert_eq!(tips[1].epoch, pointer, "{tips:?}");
        assert!(tips[2].epoch != pointer && tips[2].head == 1, "{tips:?}");
        // A member started now: its view read from these heads.
        let groups = Groups::new(0, 2).unwrap();
        let stores = reading(&h.urls, groups);
        PostgresStore::read_owners_from(&stores).unwrap();
        let view = meridian_bor::spread::Spread::new(stores, groups);
        assert_eq!(
            Store::journal(&view, &account).unwrap().len(),
            3,
            "its journal, read where a head was left written behind the move's pointers"
        );
        assert_eq!(Store::book(&view, &account).unwrap().entries.len(), 3);
    });
    blocking(move || drop(h));
}

/// A read waits for a connection to its own database as any read does (the
/// review of 2026-11-23 of this routing, finding 2): the short wait a head
/// is asked within is for the other databases, whose outage must hold no
/// read of a partition they do not write. Held to it in the database the
/// read is sent to as well, a read was refused UNAVAILABLE whenever that
/// database's pool was busy for a second -- under load, or reconnecting
/// after a failover -- where before it waited and was answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_read_waits_for_a_connection_to_the_database_it_is_sent_to_as_any_read_does() {
    use meridian_changes::groups::Groups;
    let h = blocking(two_databases);
    let partition = partition_of(0, 0);
    let account = accounts_in("ACC-BUSY", &partition, 1, &h.zero.0, h.zero.1.as_ref())
        .await
        .remove(0);
    blocking(|| {
        // A reader member, two connections to each database.
        let groups = Groups::new(0, 2).unwrap();
        let stores = reading(&h.urls, groups);
        PostgresStore::read_owners_from(&stores).unwrap();
        let view = meridian_bor::spread::Spread::new(stores.clone(), groups);
        assert_eq!(Store::book(&view, &account).unwrap().entries.len(), 1);
        // Both of database 0's taken for a second and a half: each waiting
        // on a lock held here.
        let mut admin = postgres::Client::connect(&h.urls[0], postgres::NoTls).unwrap();
        admin
            .batch_execute("BEGIN; LOCK TABLE book_partition_head IN ACCESS EXCLUSIVE MODE")
            .unwrap();
        let held: Vec<_> = (0..2)
            .map(|_| {
                let (store, partition) = (stores[0].clone(), partition.clone());
                std::thread::spawn(move || drop(store.tip(&partition)))
            })
            .collect();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let freed = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1_500));
            admin.batch_execute("ROLLBACK").unwrap();
        });
        let started = std::time::Instant::now();
        let read = Store::book(&view, &account).map(|book| book.entries.len());
        let took = started.elapsed();
        freed.join().unwrap();
        for holder in held {
            holder.join().unwrap();
        }
        assert!(
            matches!(read, Ok(1)),
            "a read of a partition written in a database whose pool was busy: {read:?} after {took:?}"
        );
    });
    blocking(move || drop(h));
}

/// [`a_database_out_of_reach_slows_no_read_of_a_partition_written_in_another`]
/// for a member's first read of a partition (the review of 2026-11-23 of
/// this routing, finding 3): that test read each account once before the
/// outage, and a partition found at its owner's home is read there from
/// then on. Before that, where the read is sent was found by asking every
/// database where the partition stands, each on its pool's own 30 s wait:
/// one database out of reach held a member's first read of every partition
/// that long. A read naming an account now starts from its owner's home by
/// the view and asks its head there, as it does once found.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_database_out_of_reach_slows_no_first_read_of_a_partition_written_in_another() {
    use meridian_bor::store::StoreError;
    use meridian_changes::groups::Groups;
    let h = blocking(two_databases);
    let (stays, moves) = (partition_of(0, 0), partition_of(0, 1));
    assert_ne!(stays, moves);
    let here = accounts_in("ACC-FIRST", &stays, 1, &h.zero.0, h.zero.1.as_ref())
        .await
        .remove(0);
    let gone = accounts_in("ACC-FIRST-GONE", &moves, 1, &h.zero.0, h.zero.1.as_ref())
        .await
        .remove(0);
    accounts_in(
        "ACC-FIRST-ONE",
        &partition_of(1, 0),
        1,
        &h.one.0,
        h.one.1.as_ref(),
    )
    .await;
    blocking(|| {
        // A reader member, started before the move: its view of the writer
        // groups read once, and no account read yet.
        let groups = Groups::new(0, 2).unwrap();
        let stores = reading(&h.urls, groups);
        PostgresStore::read_owners_from(&stores).unwrap();
        let view = meridian_bor::spread::Spread::new(stores.clone(), groups);
        switched_to_one(&h, &moves);
        // Database 1 out of this member's reach: both connections of its
        // pool taken, each waiting on a lock held here.
        let mut admin = postgres::Client::connect(&h.urls[1], postgres::NoTls).unwrap();
        admin
            .batch_execute("BEGIN; LOCK TABLE book_partition_head IN ACCESS EXCLUSIVE MODE")
            .unwrap();
        let held: Vec<_> = (0..2)
            .map(|_| {
                let (store, partition) = (stores[1].clone(), moves.clone());
                std::thread::spawn(move || drop(store.tip(&partition)))
            })
            .collect();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let started = std::time::Instant::now();
        let read = Store::book(&view, &here).map(|book| book.entries.len());
        let took = started.elapsed();
        assert_eq!(read.ok(), Some(1));
        assert!(
            took < std::time::Duration::from_secs(1),
            "a first read of a partition database 0 writes waited {took:?} on database 1"
        );
        // Its partition's head in database 0 a move's pointer, and the
        // database it may have gone to not read: refused, within the bound.
        let started = std::time::Instant::now();
        let refused = Store::book(&view, &gone).map(|book| book.entries.len());
        let took = started.elapsed();
        assert!(
            matches!(refused, Err(StoreError::Unavailable(_))),
            "{refused:?}"
        );
        assert!(
            took < std::time::Duration::from_secs(3),
            "refused only after {took:?}"
        );
        admin.batch_execute("ROLLBACK").unwrap();
        for holder in held {
            holder.join().unwrap();
        }
        // In reach again: read where it is written, the view as it was.
        assert_eq!(Store::book(&view, &gone).unwrap().entries.len(), 1);
        assert_eq!(groups.owner(&moves), 0);
    });
    blocking(move || drop(h));
}

/// A partition's first naming in a member's view of the writer groups is a
/// change of its owner, as any other is (the review of 2026-11-23 of this
/// routing, finding 4): a partition no group was named for, its rendezvous
/// owner's, is named by its first move. Moved to another group, written
/// there and moved back between two of a writer's looks, its view went
/// from no name to its own group's, counted as no change, and the book it
/// held from before the move was answered without what the other group
/// wrote.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_writers_held_book_is_dropped_when_its_partition_is_first_named_an_owner() {
    use meridian_bor::partition_moves::{Asked, Ended};
    use meridian_changes::groups::Groups;
    let h = blocking(two_databases);
    let partition = partition_of(0, 2);
    let account = accounts_in("ACC-NAMED", &partition, 1, &h.zero.0, h.zero.1.as_ref())
        .await
        .remove(0);
    accounts_in(
        "ACC-NAMED-ONE",
        &partition_of(1, 0),
        1,
        &h.one.0,
        h.one.1.as_ref(),
    )
    .await;
    let (writer, stores, groups) = blocking(|| {
        // No group named for it yet, as before any start pinned its
        // owners: its rendezvous owner's, group 0's.
        for url in &h.urls {
            postgres::Client::connect(url, postgres::NoTls)
                .unwrap()
                .execute(
                    "UPDATE book_partition_head SET writer_group = NULL WHERE partition = $1",
                    &[&partition],
                )
                .unwrap();
        }
        // A member of group 0, its view read at its start.
        let groups = Groups::new(0, 2).unwrap();
        let stores = reading(&h.urls, groups);
        PostgresStore::read_owners_from(&stores).unwrap();
        assert_eq!(groups.assigned(&partition), None);
        let spread = Arc::new(meridian_bor::spread::Spread::new(stores.clone(), groups));
        let writer = meridian_bor::writer::Writer::new(spread as Arc<dyn Store>, h.zero.0.clone());
        assert_eq!(Store::book(&writer, &account).unwrap().entries.len(), 1);
        assert_eq!(writer.held().0, 1, "held from here on");
        (writer, stores, groups)
    });
    let moved = |to: u32| {
        blocking(|| {
            h.mover.begin(&partition);
            let ended = h.mover.run(
                &partition,
                to,
                &Asked {
                    by_hand: true,
                    moved_by: PERSON.into(),
                    reason: "there and back".into(),
                },
            );
            // The harness's members' looks; the writer's own is below.
            for member in [&h.zero.1, &h.one.1] {
                PostgresStore::read_owners_from(&[
                    member.database(0).clone(),
                    member.database(1).clone(),
                ])
                .unwrap();
            }
            ended
        })
    };
    let away = moved(1);
    assert!(matches!(away, Ok(Ended::Moved(..))), "{away:?}");
    write_once(&h.one.0, &account, 1).await.unwrap();
    let back = moved(0);
    assert!(matches!(back, Ok(Ended::Moved(..))), "{back:?}");
    blocking(|| {
        // The writer's next look: the partition named, for the first time
        // in its view, and its own group's as before.
        PostgresStore::read_owners_from(&stores).unwrap();
        assert_eq!(groups.assigned(&partition), Some(0));
        assert!(Store::writes(&writer, &account));
        assert_eq!(
            Store::book(&writer, &account).unwrap().entries.len(),
            2,
            "the book held from before its partition was first named, without what was written meanwhile"
        );
    });
    blocking(move || drop((writer, stores, h)));
}

/// A read that finds its account in another partition than this member
/// last found it in follows that partition by its heads too (the review of
/// 2026-11-23 of this routing, finding 6). An account moved to another
/// partition, and that partition then moved to another group's database,
/// both since this member last looked: the read went by the placement it
/// held to the account's old partition's database, which still holds the
/// account's row -- its new partition's source, not let go yet -- and that
/// row's partition was then routed by the view, to the same database: the
/// copy the move closed, answered without what was written after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_read_follows_the_partition_its_account_went_to_past_a_stale_view_of_its_owner() {
    use meridian_bor::decide::Context;
    use meridian_bor::moves::move_account_with;
    use meridian_changes::groups::Groups;
    use meridian_changes::moves::Settings;
    use meridian_domain::v1::MoveAccountRequest;
    let h = blocking(two_databases);
    let (first, second) = (partition_of(0, 0), partition_of(0, 1));
    assert_ne!(first, second);
    let account = accounts_in("ACC-WENT", &first, 1, &h.zero.0, h.zero.1.as_ref())
        .await
        .remove(0);
    accounts_in("ACC-WENT-TWO", &second, 1, &h.zero.0, h.zero.1.as_ref()).await;
    accounts_in(
        "ACC-WENT-ONE",
        &partition_of(1, 0),
        1,
        &h.one.0,
        h.one.1.as_ref(),
    )
    .await;
    // A reader member: its view of the writer groups read once, and the
    // account found in its first partition.
    let (view, groups) = blocking(|| {
        let groups = Groups::new(0, 2).unwrap();
        let stores = reading(&h.urls, groups);
        PostgresStore::read_owners_from(&stores).unwrap();
        let view = meridian_bor::spread::Spread::new(stores, groups);
        assert_eq!(Store::book(&view, &account).unwrap().entries.len(), 1);
        assert_eq!(Store::partition_of(&view, &account), Some(first.clone()));
        (view, groups)
    });
    // The account to its second partition, group 0's too, in database 0.
    let moved = blocking(|| {
        move_account_with(
            &h.zero.0,
            h.zero.1.as_ref(),
            &Context {
                acting_for: PERSON.into(),
                received_at_ns: 5_000,
                message_id: unique("msg-went"),
                ..Default::default()
            },
            &MoveAccountRequest {
                account_id: account.clone(),
                target_partition: second.clone(),
                reason: "to its second partition".into(),
            },
            &Settings::default(),
        )
    })
    .unwrap();
    assert_eq!(moved.to_partition, second, "{moved:?}");
    // That partition to group 1's database, its source's accounts not let
    // go; and a write there.
    blocking(|| switched_to_one(&h, &second));
    write_once(&h.one.0, &account, 1).await.unwrap();
    blocking(|| {
        let written = Store::book(h.one.1.as_ref(), &account)
            .unwrap()
            .entries
            .len();
        let closed = Store::book(&**h.zero.1.database(0), &account)
            .unwrap()
            .entries
            .len();
        assert!(written > closed, "{written} against {closed}");
        // The reader's placement of it and its view, both from before.
        assert_eq!(Store::partition_of(&view, &account), Some(first.clone()));
        assert_eq!(groups.owner(&second), 0);
        assert_eq!(
            Store::book(&view, &account).unwrap().entries.len(),
            written,
            "a read after the write was acknowledged answered from the copy its partition's move closed"
        );
        assert_eq!(groups.owner(&second), 0, "the view, as it was");
    });
    blocking(move || drop((view, h)));
}

/// A write that finds its partition between two databases -- its source
/// let go, its target not yet opened -- longer than its caller waits is
/// answered OVERLOADED with a retry-after while the caller still waits
/// (step 3d; section 6.8, the hold: "never past the caller's
/// Meridian-Timeout-Ms"). Each database's store held it no longer than its
/// caller's deadline (m11), but the book over several databases then asked
/// again for the whole of moves.holdSeconds, and the caller timed out,
/// answered nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn m12_a_write_held_between_two_databases_is_answered_before_its_callers_deadline() {
    use meridian_bor::partition_moves::{Asked, Ended};
    use meridian_changes::moves::points;
    let h = blocking(two_databases);
    let partition = partition_of(1, 0);
    let accounts = accounts_in("ACC-M12-HELD", &partition, 1, &h.one.0, h.one.1.as_ref()).await;
    write_once(&h.one.0, &accounts[0], 1).await.unwrap();
    // The switch stopped between its two databases for three seconds.
    let between = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let between = Arc::clone(&between);
        points::arm(
            "book.switch.between",
            &partition,
            Arc::new(move |_| {
                between.store(true, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_secs(3));
                Ok(())
            }),
        );
    }
    let TwoDatabases {
        urls: _urls,
        zero,
        one,
        mover,
        broker: _broker,
    } = h;
    let moving = {
        let partition = partition.clone();
        tokio::task::spawn_blocking(move || {
            mover.begin(&partition);
            let ended = mover.run(&partition, 0, &Asked::default());
            (mover, ended)
        })
    };
    while !between.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    // A caller that waits a second and a half.
    let began = std::time::Instant::now();
    let said = zero
        .0
        .call_stamped(
            SET_ACCOUNT_ATTRIBUTE,
            "meridian.v1.SetAccountAttributeRequest",
            SetAccountAttributeRequest {
                account_id: accounts[0].clone(),
                attribute: Some(set_account_attribute_request::Attribute::WaitsForCompliance(true)),
                reason: "held between two databases".into(),
            }
            .encode_to_vec(),
            None,
            Some(std::time::Duration::from_millis(1_500)),
            &Stamp {
                acting_for_subject: PERSON.to_string(),
                account_scope: None,
                ..Default::default()
            },
        )
        .await
        .map(|_| ())
        .map_err(|failed| failed.to_string());
    let took = began.elapsed();
    let refused = said.expect_err("held past its caller's wait: refused");
    let overloaded = format!(
        "refusal-reason {}",
        meridian_pb::v1::RefusalReason::Overloaded as i32
    );
    assert!(
        refused.contains(&overloaded) && refused.contains("retry="),
        "answered OVERLOADED with a retry-after, never left to time out: {refused}"
    );
    assert!(
        took < std::time::Duration::from_millis(1_500),
        "answered while its caller still waited: {took:?}"
    );
    let (mover, ended) = moving.await.unwrap();
    points::disarm("book.switch.between", &partition);
    assert!(matches!(ended, Ok(Ended::Moved(_))), "{ended:?}");
    // Asked again once it is opened: written in its new database.
    write_once(&one.0, &accounts[0], 2).await.unwrap();
    blocking(move || drop((zero, one, mover)));
}

/// A read naming an account, where every head of its partition is a move's
/// pointer, goes to the database of the pointer furthest on (the second
/// review of 2026-11-23 of this routing, N3): two moves between two of a
/// member's looks -- the first done and written on in its target, the
/// second stopped between its two heads -- leave the database the view
/// names with the first move's pointer, behind the second's source. Sent
/// there, the read was answered from the copy the first move let go,
/// without what the first target wrote before it let go in turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_read_where_every_head_is_a_pointer_goes_to_the_pointer_furthest_on() {
    use meridian_bor::postgres::Switched;
    use meridian_changes::groups::Groups;
    use meridian_changes::moves::points;
    // Group 0's database, group 1's, and one neither's home is in.
    let h = blocking(|| databases(3, None));
    let partition = partition_of(0, 1);
    let account = accounts_in("ACC-TWICE", &partition, 1, &h.zero.0, h.zero.1.as_ref())
        .await
        .remove(0);
    accounts_in(
        "ACC-TWICE-ONE",
        &partition_of(1, 0),
        1,
        &h.one.0,
        h.one.1.as_ref(),
    )
    .await;
    write_once(&h.zero.0, &account, 0).await.unwrap();
    // Its first move, to group 1's database, done; written on there.
    blocking(|| switched_to_one(&h, &partition));
    for n in 1..3 {
        write_once(&h.one.0, &account, n).await.unwrap();
    }
    blocking(|| {
        // Its second, on to database 2 for group 0, stopped between its
        // two heads.
        points::arm(
            "book.switch.between",
            &partition,
            Arc::new(|_| Err("stopped between the two databases".into())),
        );
        let (source, to) = (h.zero.1.database(1), h.zero.1.database(2));
        let warmed = to.copy_partition_from(source, &partition, 0).unwrap();
        let switched = source.switch_partition(
            to,
            &partition,
            0,
            std::time::Duration::from_secs(10),
            false,
            warmed,
        );
        points::disarm("book.switch.between", &partition);
        assert!(
            matches!(switched, Ok(Switched::LetGo { .. })),
            "{switched:?}"
        );
        let tips = heads_of(&h.urls, &partition);
        let pointer = meridian_changes::moves::POINTER;
        assert!(tips.iter().all(|tip| tip.epoch == pointer), "{tips:?}");
        assert!(
            tips[0].head < tips[1].head && tips[2].head <= tips[1].head,
            "{tips:?}"
        );
        // A member whose view names group 0 its owner: database 0, which
        // holds the first move's pointer.
        let groups = Groups::new(0, 2).unwrap();
        let stores = reading(&h.urls, groups);
        PostgresStore::read_owners_from(&stores).unwrap();
        assert_eq!(groups.owner(&partition), 0, "the member's view");
        let view = meridian_bor::spread::Spread::new(stores, groups);
        assert_eq!(
            Store::book(&view, &account).unwrap().entries.len(),
            4,
            "its book, read where every head of its partition is a pointer"
        );
        assert_eq!(Store::journal(&view, &account).unwrap().len(), 4);
    });
    blocking(move || drop(h));
}

/// Step 3d, stage T (section 15; `writer_group` and `mode` approved
/// 2026-11-15): which of the store's members a writer told to stop may stop
/// for, read from the members' own rows look after look. Only another
/// group's writer that serves, said again since the look before, and that
/// will not stop with it: not there at its first look, or running another
/// revision. Never the writer its own upgrade is about to stop too, a
/// standby, a reader, its own group's, one holding its lease that does not
/// serve yet, one no longer said, or a member of the release before, which
/// says neither its group nor its mode. No time is compared with another
/// member's clock (the review's finding 10): every member here stamps its
/// rows by a clock of its own, hours apart.
#[test]
fn t_a_writer_told_to_stop_stops_only_for_another_groups_writer_serving_anew() {
    use meridian_changes::members::Watch;
    let (_schema, store) = isolated();
    let revision = meridian_changes::members::revision("0.1.900", 1, 2, Some(4));
    let other = meridian_changes::members::revision("0.1.901", 1, 2, Some(4));
    let hour = 3_600_000_000_000_i64;
    let mut tick = 0_i64;
    // Each member says its row by its own clock: `skew` hours from the rest.
    let mut says =
        |member: &str, revision: &str, skew: i64, started: i64, group: u32, mode: &str| {
            tick += 1;
            store
                .member_says(
                    member,
                    revision,
                    skew * hour + started,
                    skew * hour + 1_000 + tick,
                    group,
                    mode,
                )
                .unwrap();
        };
    let mut watch = Watch::new(1, &revision, false);
    let look = |watch: &mut Watch| watch.serves(&store.members_but("one-old").unwrap());
    // The first group's writer of before the upgrade, about to stop too,
    // its clock five hours ahead: "started" long after any tell.
    says("zero-old", &revision, 5, 1, 0, "writer");
    says("one-old", &revision, 0, 1, 1, "writer");
    // A member of the release before: neither group nor mode said.
    store.member_seen("zero-older", &other, 1, 1_000).unwrap();
    assert!(!look(&mut watch), "the first look notes who is there");
    says("zero-old", &revision, 5, 1, 0, "writer");
    store.member_seen("zero-older", &other, 1, 1_001).unwrap();
    assert!(
        !look(&mut watch),
        "the writer about to stop too, and the release before's member"
    );
    // Its own group's new writer, a standby and a reader of the first group.
    says("one-new", &revision, -3, 9, 1, "writer");
    says("zero-standby", &revision, 0, 9, 0, "standby");
    says("zero-reader", &revision, 0, 9, 0, "reader");
    assert!(
        !look(&mut watch),
        "none is another group's writer serving anew"
    );
    // The first group's new member: its lease held, its topics not served.
    says("zero-new", &revision, -7, 9, 0, "");
    assert!(!look(&mut watch), "a writer that does not serve yet");
    says("zero-new", &revision, -7, 9, 0, "writer");
    assert!(
        look(&mut watch),
        "the first group's writer serves anew, its clock seven hours behind"
    );
    // Not said again: stopped, or cut off.
    assert!(
        !look(&mut watch),
        "a writer not said again since the look before"
    );
    // Running another revision, there from the first look: the roll reached
    // that group first.
    let mut rolled = Watch::new(1, &revision, false);
    says("zero-rolled", &other, 2, 1, 0, "writer");
    assert!(!look(&mut rolled));
    assert!(!look(&mut rolled), "not said again");
    says("zero-rolled", &other, 2, 1, 0, "writer");
    assert!(
        look(&mut rolled),
        "another group's writer at another revision, said again"
    );
    // Where no lease fences its group, its own group's new writer too.
    let mut unleased = Watch::new(1, &revision, true);
    assert!(!look(&mut unleased));
    says("one-newer", &revision, 0, 20, 1, "writer");
    assert!(look(&mut unleased), "its own group's replacement serves");
}

/// Stage T's review, finding 9: a member's row never says one process's
/// start beside another's mode. Its container restarted under the same
/// name, the new process says nothing of what it serves as until it
/// serves; before, its row kept the old process's `writer` beside the new
/// start, and a writer told to stop stopped for a member serving nothing.
#[test]
fn t_a_member_started_again_says_nothing_of_the_process_before() {
    let (_schema, store) = isolated();
    let revision = meridian_changes::members::revision("0.1.900", 1, 2, Some(4));
    let mode = |member: &str| {
        store
            .members_but("nobody")
            .unwrap()
            .into_iter()
            .find(|said| said.member == member)
            .map(|said| (said.started_at_ns, said.mode))
            .unwrap()
    };
    store
        .member_says("zero", &revision, 10, 100, 0, "writer")
        .unwrap();
    // The same process, seen again: what it serves as is kept.
    store.member_seen("zero", &revision, 10, 102).unwrap();
    assert_eq!(mode("zero"), (10, "writer".to_string()));
    // Another process under its name: nothing said of what it serves as.
    store.member_seen("zero", &revision, 20, 104).unwrap();
    assert_eq!(mode("zero"), (20, String::new()));
    // And the one statement says both: never the old mode beside the new start.
    store
        .member_says("zero", &revision, 10, 105, 0, "writer")
        .unwrap();
    store
        .member_says("zero", &revision, 30, 106, 0, "")
        .unwrap();
    assert_eq!(mode("zero"), (30, String::new()));
}

/// Stage T's review, finding 3 (ruled 2026-11-15): two writers of one
/// group, the second started while the first serves on, told to stop. No
/// lease fences the group -- the tolerant default -- so the second adopts
/// the epoch the first writes under and neither is fenced: both committed,
/// side by side. Told to stop, the first commits nothing more; only the
/// second commits. No epoch is raised and no takeover recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_a_writer_told_to_stop_commits_nothing_beside_its_replacement() {
    let (url, migrated) = blocking(isolated);
    let (_broker, [(zero, zero_store), (first, first_store)], mover) =
        blocking(|| two_members(&url));
    let partition = (0..8)
        .map(meridian_bor::store::partition_named)
        .find(|p| meridian_changes::groups::owner(p, 2) == 1)
        .unwrap();
    let account = (0..)
        .map(|n| unique(&format!("ACC-TSTOP-{n}")))
        .find(|a| meridian_bor::store::placement_of(a, 8) == partition)
        .unwrap();
    blocking(|| first_store.place_first(&account, &account)).unwrap();
    open(&first, &account, "INS-AAPL").await;
    let read = {
        let (url, account) = (url.clone(), account.clone());
        move || {
            let (url, account) = (url.clone(), account.clone());
            blocking(move || {
                let mut admin = postgres::Client::connect(&url, postgres::NoTls).unwrap();
                let row = admin
                    .query_one(
                        "SELECT (SELECT count(*) FROM book_entry WHERE account_id = $1),
                                (SELECT count(*) FROM book_writer_took_over),
                                (SELECT epoch FROM book_writer_group WHERE writer_group = 1)",
                        &[&account],
                    )
                    .unwrap();
                (
                    row.get::<_, i64>(0),
                    row.get::<_, i64>(1),
                    row.get::<_, i64>(2),
                )
            })
        }
    };
    let (entries, took, epoch) = read();
    let write = |bus: Arc<Bus>, n: u64| {
        let account = account.clone();
        async move {
            let request = SetAccountAttributeRequest {
                account_id: account,
                attribute: Some(
                    set_account_attribute_request::Attribute::WaitsForCompliance(
                        n.is_multiple_of(2),
                    ),
                ),
                reason: format!("write {n}"),
            };
            send(&bus, SET_ACCOUNT_ATTRIBUTE, &request, Some(PERSON)).await
        }
    };
    write(first.clone(), 1)
        .await
        .expect("the first writes, until it is told to stop");
    // Told to stop, it serves on; its replacement starts beside it, a
    // member of the same group on a bus of its own.
    first_store.stop_committing();
    let (_second_broker, [_, (second, second_store)], second_mover) =
        blocking(|| two_members(&url));
    assert_eq!(
        blocking({
            let second_store = second_store.clone();
            move || second_store.adopt()
        })
        .unwrap() as i64,
        epoch,
        "the same epoch: no lease, nobody fenced"
    );
    write(second.clone(), 2).await.expect("the second commits");
    let refused = write(first.clone(), 3)
        .await
        .expect_err("the first commits nothing more");
    assert!(refused.contains(meridian_changes::STOPPED), "{refused}");
    write(second.clone(), 4)
        .await
        .expect("the second commits on");
    assert!(write(first.clone(), 5).await.is_err());
    assert_eq!(
        read(),
        (entries + 3, took, epoch),
        "writes 1, 2 and 4: only the second committed once the first was told, no epoch raised, no takeover recorded"
    );
    blocking(move || {
        drop((
            migrated,
            zero,
            zero_store,
            first,
            first_store,
            mover,
            second,
            second_store,
            second_mover,
        ))
    });
}

// ── A plugin shard's epoch fence (step 3d, checkpoint 5, item 32) ───────────

/// A bus whose sidecar stamps shard 1 at `epoch` on every command, serving
/// the book over `store`: one of an instance's pods, before and after a take.
fn stamped(store: &Arc<PostgresStore>, epoch: u64) -> Arc<Bus> {
    let (bus, _) = wired_with(Arc::clone(store));
    bus.stamp_shard(1, epoch).unwrap();
    bus
}

/// A command stamped with a shard's epoch, checked in its batch's own
/// transaction against `book_shard_fence`: lower is refused and journals
/// nothing, equal passes, higher raises; a raise the lake's control writer
/// asks is never taken back.
#[tokio::test(flavor = "multi_thread")]
async fn a_command_under_a_lower_shard_epoch_is_refused_an_equal_passes_a_higher_raises() {
    let store = blocking(store);
    let instance = "operations-test-1";
    // The instance's earlier takes: its fence row, as the lake's control
    // writer asked this group to raise it.
    let shard_row = |store: &PostgresStore| {
        let mut client = postgres::Client::connect(&url(), postgres::NoTls).unwrap();
        client
            .execute(
                "DELETE FROM book_shard_fence WHERE instance = $1",
                &[&instance],
            )
            .unwrap();
        store.raise_shard(instance, 1, 1).unwrap()
    };
    assert_eq!(blocking(|| shard_row(&store)), 1);
    let (one, two, three) = (stamped(&store, 1), stamped(&store, 2), stamped(&store, 3));
    let account = |tag: &str| unique(&format!("ACC-{tag}"));

    // Equal passes.
    let a = account("a");
    send(
        &one,
        RECORD_OPENING_BALANCE,
        &opening(&a, "INS-AAPL"),
        Some(PERSON),
    )
    .await
    .unwrap();
    // Higher raises, and passes.
    let b = account("b");
    send(
        &three,
        RECORD_OPENING_BALANCE,
        &opening(&b, "INS-AAPL"),
        Some(PERSON),
    )
    .await
    .unwrap();
    // Lower is refused, with the words, and journals nothing.
    let c = account("c");
    let refused = send(
        &two,
        RECORD_OPENING_BALANCE,
        &opening(&c, "INS-AAPL"),
        Some(PERSON),
    )
    .await
    .unwrap_err();
    assert!(
        refused.contains(meridian_changes::shard_fence::WORDS),
        "{refused}"
    );
    assert!(blocking(|| store.journal(&c).unwrap().is_empty()));
    // A raise heard from the lake's control writer, never down; the pod at
    // the old epoch is refused from then on.
    assert_eq!(blocking(|| store.raise_shard(instance, 1, 2).unwrap()), 3);
    assert_eq!(blocking(|| store.raise_shard(instance, 1, 5).unwrap()), 5);
    let d = account("d");
    let refused = send(
        &three,
        RECORD_OPENING_BALANCE,
        &opening(&d, "INS-AAPL"),
        Some(PERSON),
    )
    .await
    .unwrap_err();
    assert!(
        refused.contains(meridian_changes::shard_fence::WORDS),
        "{refused}"
    );
    blocking(move || drop((one, two, three, store)));
}

// ---- A write between a cross-database move's close and its open ----------

/// One write to `account` through `bus`, its caller waiting `within`.
async fn write_within(
    bus: &Bus,
    account: &str,
    reason: &str,
    within: std::time::Duration,
) -> Result<(), String> {
    let request = SetAccountAttributeRequest {
        account_id: account.to_string(),
        attribute: Some(set_account_attribute_request::Attribute::WaitsForCompliance(false)),
        reason: reason.into(),
    };
    let stamp = Stamp {
        acting_for_subject: PERSON.to_string(),
        account_scope: None,
        ..Default::default()
    };
    bus.call_stamped(
        SET_ACCOUNT_ATTRIBUTE,
        "meridian.v1.SetAccountAttributeRequest",
        request.encode_to_vec(),
        None,
        Some(within),
        &stamp,
    )
    .await
    .map(|_| ())
    .map_err(|failed| failed.to_string())
}

/// An account closed in group 1's database toward a partition of group 0's
/// in another, its open not made: asked of group 0's member with nothing
/// handed on, so the store's own answer is read. `looked` has group 0's
/// member find the account where it was before the close, as a member
/// that pre-routed one of its writes has.
async fn held_between_two_databases(tag: &str, looked: bool) {
    use meridian_changes::groups::Groups;
    let urls: Vec<Scratch> = blocking(|| {
        (0..2)
            .map(|_| {
                let (url, migrated) = isolated();
                migrated.make_partitions(4).unwrap();
                url
            })
            .collect()
    });
    let spread = |group: u32| {
        let groups = Groups::new(group, 2).unwrap();
        let stores = urls
            .iter()
            .map(|url| {
                Arc::new(
                    PostgresStore::connect(url, 4)
                        .unwrap()
                        .with_groups(groups)
                        .with_partitions(4),
                )
            })
            .collect();
        Arc::new(meridian_bor::spread::Spread::new(stores, groups))
    };
    let (zero, one) = blocking(|| (spread(0), spread(1)));
    let groups = Groups::new(0, 2).unwrap();
    let account = (0..)
        .map(|n| unique(&format!("{tag}-{n}")))
        .find(|account| groups.owner(&meridian_bor::store::placement_of(account, 4)) == 1)
        .unwrap();
    let from = blocking(|| one.place_first(&account, &account)).unwrap();
    let to = (0..4)
        .map(meridian_bor::store::partition_named)
        .find(|partition| groups.owner(partition) == 0)
        .unwrap();
    let bus = |name: &str, store: Arc<meridian_bor::spread::Spread>| {
        let bus = Arc::new(Bus::single(
            name,
            Arc::new(MemoryBackend::new()),
            Arc::new(meridian_clock::SystemClock),
        ));
        serve(bus.clone(), store, bus.clock());
        resolving_equities(&bus);
        bus
    };
    let (bus_zero, bus_one) = (bus("held-0", zero.clone()), bus("held-1", one.clone()));
    open(&bus_one, &account, &unique("INS-HELD")).await;
    if looked {
        assert_eq!(
            blocking(|| zero.writer_group_of(&account)),
            Some(1),
            "found in {from}, group 1's, before its move"
        );
    }
    let request = MoveAccountRequest {
        account_id: account.clone(),
        target_partition: to.clone(),
        reason: "group 1 is busy".into(),
    };
    // Closed by group 1; its open, group 0's, not asked yet.
    let closed = send(
        &bus_one,
        meridian_bor::moves::MOVE_ACCOUNT,
        &request,
        Some(PERSON),
    )
    .await;
    assert!(closed.is_err(), "the open is the new partition's group's");
    // Never opened within its caller's wait: held that long, then answered
    // "ask again" as a held write is -- never another group's, never
    // "found in another partition".
    let began = std::time::Instant::now();
    let refused = write_within(
        &bus_zero,
        &account,
        "held past its caller's wait",
        std::time::Duration::from_millis(1_500),
    )
    .await
    .expect_err("nothing is written before the open");
    assert!(
        refused.contains("is moving to"),
        "held for its move's open, not refused otherwise: {refused}"
    );
    assert!(
        began.elapsed() >= std::time::Duration::from_millis(700),
        "it waited for the open inside the hold: {:?}",
        began.elapsed()
    );
    // Opened meanwhile: the write waits for it and commits after it.
    let writing = {
        let (bus, account) = (bus_zero.clone(), account.clone());
        tokio::spawn(async move {
            write_within(
                &bus,
                &account,
                "written after the open",
                std::time::Duration::from_secs(5),
            )
            .await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    send(
        &bus_zero,
        meridian_bor::moves::MOVE_ACCOUNT,
        &request,
        Some(PERSON),
    )
    .await
    .expect("opened by group 0");
    writing
        .await
        .unwrap()
        .expect("a write held by the move is written once it opens");
    let journal = blocking(|| zero.journal(&account)).unwrap();
    assert_eq!(
        journal.last().map(|entry| entry.partition.as_str()),
        Some(to.as_str()),
        "numbered in its new partition, after the open"
    );
    blocking(move || drop((zero, one, bus_zero, bus_one)));
}

/// Section 6.8, across databases (PR CI 45339243058, ha: order 1005813,
/// sent between ACC-DRIVE-01's close in database 0 and its open in
/// database 1, was refused in 93 ms): a write to an account closed toward
/// this member's database and not yet opened is held for the open, where
/// this member last found the account in the partition it left. Its new
/// database holds no row of it until the open, and the store read "the
/// record" from the other database's store's memory of where it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_write_between_a_cross_database_close_and_open_is_held_by_a_member_that_found_it_before()
{
    held_between_two_databases("ACC-HELD-LOOKED", true).await;
}

/// The same, by a member that never found the account before its move.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_write_between_a_cross_database_close_and_open_is_held_by_a_member_that_never_found_it() {
    held_between_two_databases("ACC-HELD-FRESH", false).await;
}

/// The same through two members that hand writes on (PR CI 45339243058):
/// asked of either group's member between the close and the open, a write
/// is answered once the account opens, handed on at most once each -- never
/// passed back and forth between the groups, each by "the record", until
/// the hand-ons' margins spent its caller's wait ("the caller's deadline
/// left no time to hand it on").
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_write_between_a_cross_database_close_and_open_is_answered_and_never_handed_back_and_forth(
) {
    let h = blocking(two_databases);
    let (from, to) = (partition_of(1, 2), partition_of(0, 2));
    let account = accounts_in("ACC-HANDED", &from, 1, &h.one.0, h.one.1.as_ref())
        .await
        .remove(0);
    // Group 0's member found it where it was, as one that pre-routed a
    // write of it has.
    assert_eq!(blocking(|| h.zero.1.writer_group_of(&account)), Some(1));
    let request = MoveAccountRequest {
        account_id: account.clone(),
        target_partition: to.clone(),
        reason: "group 1 is busy".into(),
    };
    // Closed by group 1, and stopped before its open is handed to group 0.
    meridian_changes::moves::points::arm(
        "book.account.closed",
        &account,
        Arc::new(|_| Err("stopped after the close".to_string())),
    );
    let stopped = send(
        &h.one.0,
        meridian_bor::moves::MOVE_ACCOUNT,
        &request,
        Some(PERSON),
    )
    .await;
    meridian_changes::moves::points::disarm("book.account.closed", &account);
    assert!(stopped.is_err(), "stopped between its close and its open");
    let handed = h.broker.handed.load(Ordering::SeqCst);
    let write = |bus: &Arc<Bus>, reason: &'static str| {
        let (bus, account) = (bus.clone(), account.clone());
        tokio::spawn(async move {
            write_within(&bus, &account, reason, std::time::Duration::from_secs(5)).await
        })
    };
    let (by_zero, by_one) = (
        write(&h.zero.0, "asked of its new group"),
        write(&h.one.0, "asked of the group it left"),
    );
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    send(
        &h.zero.0,
        meridian_bor::moves::MOVE_ACCOUNT,
        &request,
        Some(PERSON),
    )
    .await
    .expect("opened by group 0");
    by_zero
        .await
        .unwrap()
        .expect("asked of its new group's member: written once it opens");
    by_one
        .await
        .unwrap()
        .expect("asked of the member of the group it left: written once it opens");
    let handed = h.broker.handed.load(Ordering::SeqCst) - handed;
    assert!(
        handed <= 2,
        "two writes and the open, handed on {handed} times"
    );
    blocking(move || drop(h));
}
