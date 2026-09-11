//! The ledger, against Postgres.
//!
//! Not against a stand-in. Every property worth having here is a property of
//! the database rather than of the Rust around it: the unique index that makes
//! a redelivery recognisable, the check constraint that refuses a row
//! describing two things or nothing, and the transaction that keeps a row and
//! its position from ever being written apart.
//!
//! Run by `make test-store`, which brings up a database. They fail loudly when
//! it is missing rather than skipping.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use meridian_kernel::amounts::{Money, Quantity};
use meridian_kernel::store::{
    Completion, Counts, Holding, Identifier, Opened, Settled, Statement, Store,
};
use meridian_kernel::PostgresStore;

static COUNTER: AtomicU64 = AtomicU64::new(0);

const NOW: i64 = 1_757_376_000_000_000_000;

fn unique(tag: &str) -> String {
    let seq = COUNTER.fetch_add(1, Ordering::SeqCst);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{tag}-{now}-{seq}")
}

fn store() -> PostgresStore {
    let url = std::env::var("MERIDIAN_TEST_DATABASE_URL").expect(
        "MERIDIAN_TEST_DATABASE_URL is not set. These tests need a real Postgres; \
         run them with `make test-store`.",
    );
    let store = PostgresStore::connect(&url, 4).expect("could not reach the test database");
    store.migrate().expect("could not create the schema");
    store
}

fn opened(store: &PostgresStore) -> Statement {
    let statement = Statement {
        statement_id: unique("STMT"),
        source: "snaptrade".into(),
        external_statement_id: unique("st"),
        as_of_date: "2026-09-08".into(),
        read_at_ns: NOW,
        expected_rows: 1_000,
    };
    store.open(statement).unwrap().0
}

fn resolved(statement: &Statement, instrument_id: &str) -> Holding {
    Holding {
        holding_id: unique("HLD"),
        statement_id: statement.statement_id.clone(),
        account_id: unique("ACC"),
        instrument_id: Some(instrument_id.to_string()),
        unresolved_identifiers: vec![],
        quantity: Quantity::from_scaled(1_250_000_000),
        market_value: Money::from_scaled(281_250_000_000),
        currency: "USD".into(),
        escalated: false,
    }
}

#[test]
fn a_statement_is_opened_once_and_recognised_after_that() {
    let store = store();
    let statement = Statement {
        statement_id: unique("STMT"),
        source: "snaptrade".into(),
        external_statement_id: unique("st"),
        as_of_date: "2026-09-08".into(),
        read_at_ns: NOW,
        expected_rows: 1_000,
    };

    let (first, opened, _) = store.open(statement.clone()).unwrap();
    assert_eq!(opened, Opened::Opened);

    // A redelivery mints a new candidate identifier and must not use it.
    let mut again = statement.clone();
    again.statement_id = unique("STMT");
    let (second, opened, _) = store.open(again).unwrap();

    assert_eq!(opened, Opened::AlreadyRecorded);
    assert_eq!(second.statement_id, first.statement_id);
}

#[test]
fn a_resolved_row_moves_a_position_and_says_what_it_was() {
    let store = store();
    let statement = opened(&store);
    let instrument = unique("INS");
    let holding = resolved(&statement, &instrument);
    let account = holding.account_id.clone();

    match store.record(holding, NOW).unwrap().0 {
        Settled::Changed {
            previous_quantity, ..
        } => assert_eq!(previous_quantity, Quantity::ZERO),
        other => panic!("expected a change, got {other:?}"),
    }

    let position = store.position(&account, &instrument).unwrap().unwrap();
    assert_eq!(position.quantity.scaled(), 1_250_000_000);
    assert_eq!(position.as_of_date, "2026-09-08");
}

#[test]
fn a_second_statement_replaces_the_position_rather_than_adding_to_it() {
    let store = store();
    let instrument = unique("INS");

    let first = opened(&store);
    let holding = resolved(&first, &instrument);
    let account = holding.account_id.clone();
    store.record(holding, NOW).unwrap();

    let second = opened(&store);
    let mut grown = resolved(&second, &instrument);
    grown.account_id = account.clone();
    grown.quantity = Quantity::from_scaled(2_000_000_000);

    match store.record(grown, NOW + 1).unwrap().0 {
        Settled::Changed {
            previous_quantity,
            position,
        } => {
            assert_eq!(previous_quantity.scaled(), 1_250_000_000);
            assert_eq!(position.quantity.scaled(), 2_000_000_000);
        }
        other => panic!("expected a change, got {other:?}"),
    }

    let held = store.position(&account, &instrument).unwrap().unwrap();
    assert_eq!(
        held.quantity.scaled(),
        2_000_000_000,
        "the rows were summed"
    );
}

#[test]
fn a_row_saying_what_the_position_already_held_is_not_a_change() {
    let store = store();
    let instrument = unique("INS");

    let first = opened(&store);
    let holding = resolved(&first, &instrument);
    let account = holding.account_id.clone();
    store.record(holding, NOW).unwrap();

    let second = opened(&store);
    let mut same = resolved(&second, &instrument);
    same.account_id = account;

    assert!(matches!(
        store.record(same, NOW + 1).unwrap().0,
        Settled::Unchanged { .. }
    ));
}

#[test]
fn an_unresolved_row_is_kept_and_moves_nothing() {
    let store = store();
    let statement = opened(&store);
    let symbol = unique("ZZ");

    let mut unresolved = resolved(&statement, "unused");
    unresolved.instrument_id = None;
    unresolved.unresolved_identifiers = vec![Identifier {
        scheme: "symbol".into(),
        value: symbol.clone(),
        source: "snaptrade".into(),
    }];
    let account = unresolved.account_id.clone();

    assert!(matches!(
        store.record(unresolved, NOW).unwrap().0,
        Settled::Unresolved
    ));

    let page = store.page(&account, true, 100, "").unwrap();
    assert!(page.positions.is_empty());
    assert_eq!(page.unresolved.len(), 1);
    assert_eq!(page.unresolved[0].identifiers_value(), symbol);
}

#[test]
fn the_database_refuses_a_row_that_names_both_or_neither() {
    // The same rule the code enforces, enforced again where it is true of the
    // table no matter which path wrote to it.
    let store = store();
    let statement = opened(&store);

    let mut both = resolved(&statement, &unique("INS"));
    both.unresolved_identifiers = vec![Identifier {
        scheme: "symbol".into(),
        value: "AAPL".into(),
        source: "snaptrade".into(),
    }];
    assert!(store.record(both, NOW).is_err());

    let mut neither = resolved(&statement, &unique("INS"));
    neither.instrument_id = None;
    assert!(store.record(neither, NOW).is_err());
}

#[test]
fn a_row_for_a_statement_nobody_opened_is_refused() {
    let store = store();
    let orphan = Holding {
        holding_id: unique("HLD"),
        statement_id: unique("STMT"),
        account_id: unique("ACC"),
        instrument_id: Some(unique("INS")),
        unresolved_identifiers: vec![],
        quantity: Quantity::from_scaled(1),
        market_value: Money::from_scaled(1),
        currency: "USD".into(),
        escalated: false,
    };
    assert!(store.record(orphan, NOW).is_err());
}

#[test]
fn the_counts_add_up() {
    let store = store();
    let statement = opened(&store);
    let account = unique("ACC");

    for n in 0..3 {
        let mut row = resolved(&statement, &format!("{}-{n}", unique("INS")));
        row.account_id = account.clone();
        store.record(row, NOW).unwrap();
    }

    let mut missing = resolved(&statement, "unused");
    missing.account_id = account;
    missing.instrument_id = None;
    missing.unresolved_identifiers = vec![Identifier {
        scheme: "symbol".into(),
        value: unique("ZZ"),
        source: "snaptrade".into(),
    }];
    store.record(missing, NOW).unwrap();

    let counts = store.counts(&statement.statement_id).unwrap();
    assert_eq!(
        counts,
        Counts {
            received: 4,
            resolved: 3,
            unresolved: 1
        }
    );
    assert!(counts.consistent());
}

#[test]
fn concurrent_rows_for_one_position_leave_one_row_and_no_lost_update() {
    let store = Arc::new(store());
    let instrument = unique("INS");
    let account = unique("ACC");

    let mut racing = Vec::new();
    for n in 1..=8 {
        let store = store.clone();
        let instrument = instrument.clone();
        let account = account.clone();
        racing.push(std::thread::spawn(move || {
            let statement = opened(&store);
            let mut row = resolved(&statement, &instrument);
            row.account_id = account;
            row.quantity = Quantity::from_scaled(n * 100_000_000);
            store.record(row, NOW + n).unwrap()
        }));
    }
    for thread in racing {
        thread.join().unwrap();
    }

    let page = store.page(&account, false, 100, "").unwrap();
    assert_eq!(
        page.positions.len(),
        1,
        "one account and one instrument is one position"
    );
}

#[test]
fn an_identifier_survives_the_round_trip_through_json() {
    // Hand-rolled, so the characters that break a hand-rolled encoder are the
    // ones worth sending through it.
    let store = store();
    let statement = opened(&store);
    let awkward = r#"a"b\c
d	e"#;

    let mut row = resolved(&statement, "unused");
    row.instrument_id = None;
    row.unresolved_identifiers = vec![Identifier {
        scheme: "symbol".into(),
        value: awkward.into(),
        source: "snap\"trade".into(),
    }];
    let account = row.account_id.clone();
    store.record(row, NOW).unwrap();

    let page = store.page(&account, true, 100, "").unwrap();
    assert_eq!(page.unresolved[0].unresolved_identifiers[0].value, awkward);
    assert_eq!(
        page.unresolved[0].unresolved_identifiers[0].source,
        "snap\"trade"
    );
}

/// A small reach-through so the assertions above read as sentences.
trait FirstIdentifier {
    fn identifiers_value(&self) -> String;
}

impl FirstIdentifier for Holding {
    fn identifiers_value(&self) -> String {
        self.unresolved_identifiers
            .first()
            .map(|identifier| identifier.value.clone())
            .unwrap_or_default()
    }
}

#[test]
fn a_statement_completes_once_and_only_once_under_concurrency() {
    // Eight rows landing at the same moment against a statement expecting
    // eight. Exactly one of them is the row that completed it, because two
    // announcements would make a subscriber's arithmetic depend on timing.
    let store = Arc::new(store());
    let statement = Statement {
        statement_id: unique("STMT"),
        source: "snaptrade".into(),
        external_statement_id: unique("st"),
        as_of_date: "2026-09-08".into(),
        read_at_ns: NOW,
        expected_rows: 8,
    };
    let statement = store.open(statement).unwrap().0;
    let account = unique("ACC");

    let mut racing = Vec::new();
    for n in 0..8 {
        let store = store.clone();
        let statement = statement.clone();
        let account = account.clone();
        racing.push(std::thread::spawn(move || {
            let mut row = resolved(&statement, &format!("INS-{n}"));
            row.account_id = account;
            store.record(row, NOW + n).unwrap().1
        }));
    }

    let completions = racing
        .into_iter()
        .filter(|_| true)
        .map(|thread| thread.join().unwrap())
        .filter(|completion| *completion == Completion::JustCompleted)
        .count();

    assert_eq!(
        completions, 1,
        "the statement completed {completions} times"
    );
    assert_eq!(store.counts(&statement.statement_id).unwrap().received, 8);
}

#[test]
fn a_statement_promising_no_rows_completes_when_it_opens() {
    let store = store();
    let (_, opened, completion) = store
        .open(Statement {
            statement_id: unique("STMT"),
            source: "snaptrade".into(),
            external_statement_id: unique("st"),
            as_of_date: "2026-09-08".into(),
            read_at_ns: NOW,
            expected_rows: 0,
        })
        .unwrap();

    assert_eq!(opened, Opened::Opened);
    assert_eq!(completion, Completion::JustCompleted);
}
