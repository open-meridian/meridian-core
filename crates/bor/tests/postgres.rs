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
    let bus = Arc::new(Bus::single(
        "operations-test-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let store = blocking(store);
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
    blocking(|| store.rebuild().unwrap());
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
