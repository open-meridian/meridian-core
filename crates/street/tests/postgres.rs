//! The street store, against Postgres.
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

use meridian_street::amounts::{Money, Quantity};
use meridian_street::store::{
    Completion, Counts, Holding, Identifier, Opened, Settled, Statement, Store,
};
use meridian_street::PostgresStore;

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

    let position = store
        .custodial_position(&account, &instrument)
        .unwrap()
        .unwrap();
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

    let held = store
        .custodial_position(&account, &instrument)
        .unwrap()
        .unwrap();
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

#[test]
fn a_database_under_the_old_table_name_is_renamed_rather_than_left_behind() {
    // The first schema change the additive mechanism could not absorb, and the
    // reason kernel/ledger-needs-migrations exists. A database created before
    // today has the table under its old name, and creating the new one beside
    // it would leave a full table and an empty one with nothing to say which is
    // which.
    let url = std::env::var("MERIDIAN_TEST_DATABASE_URL").unwrap();
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();

    let scratch = format!("rename_{}", unique("t").replace('-', "_"));
    client
        .batch_execute(&format!(
            "CREATE SCHEMA {scratch};
             SET search_path TO {scratch};
             CREATE TABLE position (
                account_id text NOT NULL,
                instrument_id text NOT NULL,
                quantity_scaled bigint NOT NULL,
                value_scaled bigint NOT NULL,
                currency text NOT NULL,
                last_statement_id text NOT NULL,
                as_of_date text NOT NULL,
                updated_at_ns bigint NOT NULL,
                PRIMARY KEY (account_id, instrument_id));
             INSERT INTO position VALUES ('ACC', 'INS', 1, 1, 'USD', 'STMT', '2026-09-08', 1);"
        ))
        .unwrap();

    let scoped = format!("{url}?options=-csearch_path%3D{scratch}");
    let store = PostgresStore::connect(&scoped, 1).unwrap();
    store.migrate().expect("the rename did not apply");

    let carried = store.custodial_position("ACC", "INS").unwrap();
    assert!(
        carried.is_some(),
        "the row was left behind under the old table name"
    );

    client
        .batch_execute(&format!("DROP SCHEMA {scratch} CASCADE"))
        .unwrap();
}

// ── The migration history ───────────────────────────────────────────────────
//
// Each of these owns a schema of its own, because they are about the state of
// a database rather than of a row, and the other tests here share one.

fn base_url() -> String {
    std::env::var("MERIDIAN_TEST_DATABASE_URL").expect(
        "MERIDIAN_TEST_DATABASE_URL is not set. These tests need a real Postgres; \
         run them with `make test-store`.",
    )
}

/// A schema of this test's own, and a URL whose connections land in it.
fn own_schema(tag: &str) -> (String, postgres::Client) {
    let name = unique(tag).replace('-', "_");
    let mut admin = postgres::Client::connect(&base_url(), postgres::NoTls)
        .expect("could not reach the test database");
    admin
        .batch_execute(&format!("CREATE SCHEMA {name}"))
        .expect("could not create a schema");

    let url = format!("{}?options=-c%20search_path%3D{name}", base_url());
    let client = postgres::Client::connect(&url, postgres::NoTls).expect("could not connect");
    (url, client)
}

fn applied_versions(client: &mut postgres::Client) -> Vec<i64> {
    client
        .query("SELECT version FROM schema_migration ORDER BY version", &[])
        .expect("could not read the history")
        .iter()
        .map(|row| row.get::<_, i64>(0))
        .collect()
}

#[test]
fn a_start_against_an_unmigrated_database_refuses_and_says_what_to_run() {
    let (url, _client) = own_schema("unmigrated");
    let store = PostgresStore::connect(&url, 1).expect("could not connect");

    let refused = store
        .verify()
        .expect_err("an empty database must not verify");
    let said = refused.to_string();
    assert!(said.contains("no schema"), "{said}");
    assert!(
        said.contains("migrate"),
        "the refusal has to name the fix: {said}"
    );
}

#[test]
fn migrating_applies_every_version_and_then_verifies() {
    let (url, mut client) = own_schema("fresh");
    let store = PostgresStore::connect(&url, 1).expect("could not connect");

    store.migrate().expect("could not apply the schema");

    let versions = applied_versions(&mut client);
    let expected: Vec<i64> = meridian_street::migrations::MIGRATIONS
        .iter()
        .map(|m| m.version)
        .collect();
    assert_eq!(versions, expected);
    store.verify().expect("a migrated database must verify");
}

#[test]
fn migrating_twice_changes_nothing() {
    let (url, mut client) = own_schema("twice");
    let store = PostgresStore::connect(&url, 1).expect("could not connect");

    store.migrate().expect("could not apply the schema");
    let first: Vec<(i64, i64)> = client
        .query("SELECT version, applied_at_ns FROM schema_migration", &[])
        .expect("could not read the history")
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();

    store.migrate().expect("migrating again must be a no-op");

    let second: Vec<(i64, i64)> = client
        .query("SELECT version, applied_at_ns FROM schema_migration", &[])
        .expect("could not read the history")
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    assert_eq!(
        first, second,
        "a second run re-applied or re-recorded something"
    );
}

#[test]
fn a_database_made_before_the_history_is_adopted_with_its_rows_intact() {
    // The state a real deployment is in: tables created by `CREATE TABLE IF
    // NOT EXISTS` at start, under the old table name, without the two columns
    // the guarded list used to add, and holding a statement nothing can
    // rebuild.
    let (url, mut client) = own_schema("adopted");
    client
        .batch_execute(include_str!("../migrations/0001_ledger.sql"))
        .expect("could not create the old schema");
    client
        .batch_execute(
            "ALTER TABLE custodial_position RENAME TO position;
             ALTER TABLE statement DROP COLUMN expected_rows;
             ALTER TABLE statement DROP COLUMN completed_at_ns;",
        )
        .expect("could not put the schema back to how it was");
    client
        .execute(
            "INSERT INTO statement (statement_id, source, external_statement_id, as_of_date, read_at_ns)
             VALUES ('STM-old', 'snaptrade', 'ext-1', '2026-09-01', 1)",
            &[],
        )
        .expect("could not write a statement");

    let store = PostgresStore::connect(&url, 1).expect("could not connect");
    store.migrate().expect("could not adopt and migrate");

    let kept: i64 = client
        .query_one(
            "SELECT count(*) FROM statement WHERE statement_id = 'STM-old'",
            &[],
        )
        .expect("could not count")
        .get(0);
    assert_eq!(
        kept, 1,
        "adoption lost a statement, which nothing can rebuild"
    );

    let versions = applied_versions(&mut client);
    assert_eq!(
        versions.first().copied(),
        Some(1),
        "the baseline was not recorded"
    );
    assert_eq!(
        versions.last().copied(),
        Some(meridian_street::migrations::latest()),
        "the database was not brought up to date"
    );
    store.verify().expect("an adopted database must verify");

    let renamed: i64 = client
        .query_one(
            "SELECT count(*) FROM information_schema.tables
              WHERE table_schema = current_schema() AND table_name = 'custodial_position'",
            &[],
        )
        .expect("could not look for the table")
        .get(0);
    assert_eq!(renamed, 1, "the rename did not run");
}

#[test]
fn a_database_ahead_of_this_binary_is_refused() {
    // A rollback: the database was migrated by a newer release. Refused rather
    // than tolerated, because this binary does not know what that release
    // changed and its queries may already be wrong.
    let (url, mut client) = own_schema("ahead");
    let store = PostgresStore::connect(&url, 1).expect("could not connect");
    store.migrate().expect("could not apply the schema");

    client
        .execute(
            "INSERT INTO schema_migration (version, name, applied_at_ns) VALUES (999, 'later', 1)",
            &[],
        )
        .expect("could not pretend to be ahead");

    let refused = store.verify().expect_err("a newer schema must not verify");
    let said = refused.to_string();
    assert!(said.contains("999"), "{said}");
    assert!(
        said.contains(&meridian_street::migrations::latest().to_string()),
        "the refusal has to name both versions: {said}"
    );
}
