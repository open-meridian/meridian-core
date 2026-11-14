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
/// epoch.
fn isolated() -> (String, Arc<PostgresStore>) {
    let schema = unique("bor_fence").replace('-', "_");
    let mut admin = postgres::Client::connect(&url(), postgres::NoTls).expect("the test database");
    admin
        .batch_execute(&format!("CREATE SCHEMA {schema}"))
        .expect("a schema of its own");
    let joined = if url().contains('?') { "&" } else { "?" };
    let url = format!("{}{joined}options=-csearch_path%3D{schema}", url());
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
    let (_, primary) = blocking(isolated);
    let (_, replica) = blocking(isolated);
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
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn group_commit_refuses_each_command_alone_and_a_broken_batch_is_tried_alone() {
    use meridian_bor::decide::{set_attribute, Context};
    use meridian_bor::store::{Acted, Decided, StoreError};
    use std::sync::Mutex;
    let store = blocking(store);
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
    let act = |n: usize| {
        let store = Arc::clone(&store);
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
                        // The first batch held in flight while the others
                        // queue behind it.
                        std::thread::sleep(std::time::Duration::from_millis(300));
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
    std::thread::sleep(std::time::Duration::from_millis(100));
    let others: Vec<_> = (1..COMMANDS).map(act).collect();
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
    assert!(
        batches.iter().any(|batch| batch.len() > 1),
        "the commands queued behind the first committed together: {batches:?}"
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
        _: std::time::Duration,
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
            match tokio::task::spawn_blocking(move || handler(envelope))
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
    let urls: Vec<String> = blocking(|| {
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
    let urls: Vec<String> = blocking(|| {
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
    use meridian_bor::writer::{is_command, Writer, INTERNAL_COMMAND};
    use meridian_changes::groups::Groups;
    let broker = Arc::new(GroupsBroker {
        inner: MemoryBackend::new(),
        groups: Default::default(),
        handed: AtomicU64::new(0),
    });
    let member = |group: u32| {
        let store = Arc::new(
            PostgresStore::connect(url, 4)
                .unwrap()
                .with_groups(Groups::new(group, 2).unwrap())
                .with_partitions(8),
        );
        store.make_partitions(8).unwrap();
        store.pin_owners().unwrap();
        let bus = Arc::new(Bus::single(
            "bor-moves-test",
            Arc::clone(&broker) as Arc<dyn meridian_bus::Backend>,
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
    };
    // Each group's partitions at an epoch of its own, as a deployment's
    // are once each writer has started: neither takes the other's over --
    // a takeover ends the sessions holding its partitions' locks, by key,
    // in every schema of the test database.
    PostgresStore::connect(url, 1)
        .unwrap()
        .make_partitions(8)
        .unwrap();
    apart(url);
    let (zero, one) = (member(0), member(1));
    let mover = meridian_bor::partition_moves::Mover::new(
        vec![Arc::clone(&zero.1)],
        zero.1.groups(),
        meridian_bor::partition_moves::Settings {
            switch: std::time::Duration::from_secs(2),
            ..Default::default()
        },
    );
    (broker, [zero, one], mover)
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
        Mover::new(vec![Arc::clone(&zero_store)], zero_store.groups(), settings)
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
        let url = url.clone();
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
    let urls: Vec<String> = blocking(|| {
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
