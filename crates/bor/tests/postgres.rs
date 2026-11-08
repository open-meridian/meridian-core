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
use meridian_bus::{Bus, MemoryBackend, Stamp};
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
#[tokio::test(flavor = "multi_thread")]
async fn a_takeover_is_not_held_up_by_a_deposed_writer_stopped_mid_transaction() {
    let (url, writer) = blocking(isolated);
    assert_eq!(blocking(|| writer.adopt()).unwrap(), 1);
    let mut stalled = blocking(|| postgres::Client::connect(&url, postgres::NoTls).unwrap());
    blocking(|| {
        stalled
            .batch_execute("BEGIN; SELECT epoch FROM book_partition_head FOR UPDATE")
            .unwrap()
    });
    let standby = Arc::new(PostgresStore::connect(&url, 1).unwrap());
    let began = std::time::Instant::now();
    assert_eq!(
        blocking(|| standby.take_over(&meridian_changes::TookOver::unnamed("a stalled writer"))).unwrap(),
        2
    );
    assert!(
        began.elapsed() < std::time::Duration::from_secs(5),
        "the takeover waited {:?} on the stalled writer",
        began.elapsed()
    );
    // Its transaction ended with its session: nothing it does commits.
    assert!(blocking(|| stalled.batch_execute("COMMIT")).is_err());
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
