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

use meridian_street::amounts::{Exact, Money, Quantity};
use meridian_street::store::{
    Cause, Collateral, Completion, Counts, Direction, Figures, Holding, Identifier, Opened, Read,
    Scope, Settled, Side, Statement, Store, StoreError,
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

/// A quantity as a person writes one.
fn units(text: &str) -> Quantity {
    text.parse().unwrap()
}

fn usd(text: &str) -> Money {
    Money::new(text.parse().unwrap(), "USD")
}

fn store() -> PostgresStore {
    let url = std::env::var("MERIDIAN_TEST_DATABASE_URL").expect(
        "MERIDIAN_TEST_DATABASE_URL is not set. These tests need a real Postgres; \
         run them with `make test-store`.",
    );
    let store = PostgresStore::connect(&url, 4).expect("could not reach the test database");
    store
        .migrate(&meridian_clock::SystemClock)
        .expect("could not create the schema");
    store
}

/// A change made at `now` by the custody plugin.
fn at(now: i64) -> Cause {
    Cause {
        instance_id: "custody-snaptrade-1".into(),
        correlation_id: unique("corr"),
        causation_id: unique("msg"),
        committed_at_ns: now,
        ..Default::default()
    }
}

/// A statement as a test opens it: of an account of its own, which every row
/// `resolved` makes for it names, since a statement is one account's (W2.2).
fn statement(account_id: &str) -> Statement {
    Statement {
        statement_id: unique("STMT"),
        source: "snaptrade".into(),
        external_statement_id: unique("st"),
        as_of_date: "2026-09-08".into(),
        read_at_ns: NOW,
        expected_rows: 1_000,
        account_id: account_id.to_string(),
        external_account_id: String::new(),
        institution: String::new(),
        figures: Vec::new(),
        currency_assumed: false,
        security_interest: None,
        raw_record: None,
        provenance: Vec::new(),
        completed: None,
    }
}

/// A read of every account, as a core component makes one.
fn page(
    store: &PostgresStore,
    account_id: &str,
    include_unresolved: bool,
    limit: usize,
    cursor: &str,
) -> meridian_street::store::Page {
    store
        .page(&Read {
            scope: Scope::Everything,
            account_id: account_id.to_string(),
            include_unresolved,
            limit,
            cursor: cursor.to_string(),
            since: None,
        })
        .unwrap()
}

/// A statement naming no account, as one from a plugin before v7 does: its
/// first row gives it one (W2.2).
fn opened(store: &PostgresStore) -> Statement {
    store.open(statement(""), &at(NOW)).unwrap().0
}

fn resolved(statement: &Statement, instrument_id: &str) -> Holding {
    Holding {
        holding_id: unique("HLD"),
        statement_id: statement.statement_id.clone(),
        account_id: if statement.account_id.is_empty() {
            unique("ACC")
        } else {
            statement.account_id.clone()
        },
        instrument_id: Some(instrument_id.to_string()),
        unresolved_identifiers: vec![],
        side: Side::Long,
        quantity: units("12.5"),
        settle_date_quantity: None,
        market_value: Some(usd("2812.50")),
        currency_assumed: false,
        also_counted_in_cash: false,
        cost: Default::default(),
        escalated: false,
    }
}

#[test]
fn a_statement_is_opened_once_and_recognised_after_that() {
    let store = store();
    let statement = statement(&unique("ACC"));

    let (first, opened, _) = store.open(statement.clone(), &at(NOW)).unwrap();
    assert_eq!(opened, Opened::Opened);

    // A redelivery mints a new candidate identifier and must not use it.
    let mut again = statement.clone();
    again.statement_id = unique("STMT");
    let (second, opened, _) = store.open(again, &at(NOW)).unwrap();

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

    match store.record(holding, &at(NOW)).unwrap().0 {
        Settled::Changed {
            previous_quantity, ..
        } => assert_eq!(previous_quantity, Quantity::ZERO),
        other => panic!("expected a change, got {other:?}"),
    }

    let position = store
        .custodial_position(&account, &instrument, Side::Long)
        .unwrap()
        .unwrap();
    assert_eq!(position.quantity.to_string(), "12.5");
    assert_eq!(position.as_of_date, "2026-09-08");
}

#[test]
fn a_second_statement_replaces_the_position_rather_than_adding_to_it() {
    let store = store();
    let instrument = unique("INS");

    let first = opened(&store);
    let holding = resolved(&first, &instrument);
    let account = holding.account_id.clone();
    store.record(holding, &at(NOW)).unwrap();

    let second = opened(&store);
    let mut grown = resolved(&second, &instrument);
    grown.account_id = account.clone();
    grown.quantity = units("20");

    match store.record(grown, &at(NOW + 1)).unwrap().0 {
        Settled::Changed {
            previous_quantity,
            position,
        } => {
            assert_eq!(previous_quantity.to_string(), "12.5");
            assert_eq!(position.quantity.to_string(), "20");
        }
        other => panic!("expected a change, got {other:?}"),
    }

    let held = store
        .custodial_position(&account, &instrument, Side::Long)
        .unwrap()
        .unwrap();
    assert_eq!(held.quantity.to_string(), "20", "the rows were summed");
}

#[test]
fn a_row_saying_what_the_position_already_held_is_not_a_change() {
    let store = store();
    let instrument = unique("INS");

    let first = opened(&store);
    let holding = resolved(&first, &instrument);
    let account = holding.account_id.clone();
    store.record(holding, &at(NOW)).unwrap();

    let second = opened(&store);
    let mut same = resolved(&second, &instrument);
    same.account_id = account;

    assert!(matches!(
        store.record(same, &at(NOW + 1)).unwrap().0,
        Settled::Unchanged { .. }
    ));
}

#[test]
fn the_smallest_and_largest_holdings_read_back_exactly() {
    // spec/quantities-carry-their-own-scale, requirement 7: Alpaca's ninth
    // decimal, a hundred billion units, and those at eighteen decimals, each
    // read back at the scale it was stated with.
    let store = store();
    let statement = opened(&store);
    let account = unique("ACC");
    for (n, quantity) in [
        "0.000000001",
        "100000000000",
        "100000000000.000000000000000001",
    ]
    .into_iter()
    .enumerate()
    {
        let instrument = format!("INS-{n}");
        let mut row = resolved(&statement, &instrument);
        row.account_id = account.clone();
        row.quantity = units(quantity);
        row.market_value = Some(usd("41230.50"));
        store.record(row, &at(NOW)).unwrap();

        let held = store
            .custodial_position(&account, &instrument, Side::Long)
            .unwrap()
            .unwrap();
        assert_eq!(held.quantity.to_string(), quantity);
        assert_eq!(held.market_value.unwrap().to_string(), "41230.50 USD");

        // And the row itself, unresolved, through the other read.
        let mut unresolved = resolved(&statement, "unused");
        unresolved.account_id = account.clone();
        unresolved.instrument_id = None;
        unresolved.unresolved_identifiers = vec![Identifier {
            scheme: "symbol".into(),
            value: format!("ZZ{n}"),
            source: "snaptrade".into(),
        }];
        unresolved.quantity = units(quantity);
        store.record(unresolved, &at(NOW)).unwrap();
    }

    let page = page(&store, &account, true, 100, "");
    let rows: Vec<String> = page
        .unresolved
        .iter()
        .map(|row| row.quantity.to_string())
        .collect();
    assert_eq!(
        rows,
        [
            "0.000000001",
            "100000000000",
            "100000000000.000000000000000001"
        ]
    );
}

#[test]
fn a_restatement_at_another_scale_is_the_same_number_and_keeps_its_own_scale() {
    // Compared as numbers, so 12.5 restated as 12.50 moves nothing to
    // announce; stored as stated, so it reads back as 12.50.
    let store = store();
    let instrument = unique("INS");

    let first = opened(&store);
    let holding = resolved(&first, &instrument);
    let account = holding.account_id.clone();
    store.record(holding, &at(NOW)).unwrap();

    let second = opened(&store);
    let mut restated = resolved(&second, &instrument);
    restated.account_id = account.clone();
    restated.quantity = units("12.50");
    restated.market_value = Some(usd("2812.5"));

    assert!(matches!(
        store.record(restated, &at(NOW + 1)).unwrap().0,
        Settled::Unchanged { .. }
    ));
    let held = store
        .custodial_position(&account, &instrument, Side::Long)
        .unwrap()
        .unwrap();
    assert_eq!(held.quantity.to_string(), "12.50");
    assert_eq!(held.market_value.unwrap().to_string(), "2812.5 USD");
}

#[test]
fn the_database_refuses_a_number_the_wire_could_not_have_carried() {
    // The same range the code enforces, enforced again where it is true of
    // the table whoever wrote to it.
    let store = store();
    let statement = opened(&store);
    let mut client = postgres::Client::connect(&base_url(), postgres::NoTls).unwrap();
    for (quantity, why) in [
        ("0.0000000000000000001", "a nineteenth decimal place"),
        (
            "100000000000000000000000000000000000000",
            "a thirty-ninth digit",
        ),
    ] {
        let refused = client.execute(
            "INSERT INTO holding (holding_id, statement_id, account_id, instrument_id, side,
                                  quantity, market_value, currency)
             VALUES ($1, $2, 'ACC', 'INS', 'long', $3::text::numeric, 0, 'USD')",
            &[&unique("HLD"), &statement.statement_id, &quantity],
        );
        assert!(refused.is_err(), "{why} was stored");
    }
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
        store.record(unresolved, &at(NOW)).unwrap().0,
        Settled::Unresolved
    ));

    let page = page(&store, &account, true, 100, "");
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
    assert!(store.record(both, &at(NOW)).is_err());

    let mut neither = resolved(&statement, &unique("INS"));
    neither.instrument_id = None;
    assert!(store.record(neither, &at(NOW)).is_err());
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
        side: Side::Long,
        quantity: units("1"),
        settle_date_quantity: None,
        market_value: Some(usd("1")),
        currency_assumed: false,
        also_counted_in_cash: false,
        cost: Default::default(),
        escalated: false,
    };
    assert!(store.record(orphan, &at(NOW)).is_err());
}

#[test]
fn the_counts_add_up() {
    let store = store();
    let statement = opened(&store);
    let account = unique("ACC");

    for n in 0..3 {
        let mut row = resolved(&statement, &format!("{}-{n}", unique("INS")));
        row.account_id = account.clone();
        store.record(row, &at(NOW)).unwrap();
    }

    let mut missing = resolved(&statement, "unused");
    missing.account_id = account;
    missing.instrument_id = None;
    missing.unresolved_identifiers = vec![Identifier {
        scheme: "symbol".into(),
        value: unique("ZZ"),
        source: "snaptrade".into(),
    }];
    store.record(missing, &at(NOW)).unwrap();

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
            row.quantity = Quantity::new(Exact::new(n.into(), 0).unwrap());
            store.record(row, &at(NOW + n)).unwrap()
        }));
    }
    for thread in racing {
        thread.join().unwrap();
    }

    let page = page(&store, &account, false, 100, "");
    assert_eq!(
        page.positions.len(),
        1,
        "one account, one instrument and one side is one position"
    );
}

// ── The account side (spec/the-account-side-fits-every-venue) ───────────────

#[test]
fn long_and_short_of_one_instrument_are_two_positions_keyed_by_side() {
    // Schwab's shape: the key was account and instrument, and the short row
    // overwrote the long.
    let store = store();
    let statement = opened(&store);
    let instrument = unique("INS");

    let long = resolved(&statement, &instrument);
    let account = long.account_id.clone();
    let mut short = resolved(&statement, &instrument);
    short.account_id = account.clone();
    short.side = Side::Short;
    short.quantity = units("-50");
    store.record(long, &at(NOW)).unwrap();
    store.record(short, &at(NOW)).unwrap();

    let long = store
        .custodial_position(&account, &instrument, Side::Long)
        .unwrap()
        .unwrap();
    let short = store
        .custodial_position(&account, &instrument, Side::Short)
        .unwrap()
        .unwrap();
    assert_eq!(long.quantity.to_string(), "12.5");
    assert_eq!(short.quantity.to_string(), "-50");
    assert_eq!(page(&store, &account, false, 100, "").positions.len(), 2);
}

#[test]
fn what_the_venue_did_not_report_is_kept_absent_and_what_it_did_exactly() {
    // A settle-date quantity where reported and NULL where not; a market
    // value NULL where not reported, never zero; the assumed currency marked.
    let store = store();
    let statement = opened(&store);
    let account = unique("ACC");

    let mut cash = resolved(&statement, &unique("INS-CASH"));
    cash.account_id = account.clone();
    cash.quantity = units("1520.35");
    cash.settle_date_quantity = Some(units("1020.35"));
    cash.market_value = None;
    cash.currency_assumed = true;
    let cash_instrument = cash.instrument_id.clone().unwrap();
    let holding_id = cash.holding_id.clone();
    store.record(cash, &at(NOW)).unwrap();

    let held = store
        .custodial_position(&account, &cash_instrument, Side::Long)
        .unwrap()
        .unwrap();
    assert_eq!(held.settle_date_quantity.unwrap().to_string(), "1020.35");
    assert_eq!(held.market_value, None);

    let mut client = postgres::Client::connect(&base_url(), postgres::NoTls).unwrap();
    let row = client
        .query_one(
            "SELECT side, settle_date_quantity::text, market_value::text, currency,
                    currency_assumed
               FROM holding WHERE holding_id = $1",
            &[&holding_id],
        )
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "long");
    assert_eq!(row.get::<_, Option<String>>(1).as_deref(), Some("1020.35"));
    assert_eq!(
        row.get::<_, Option<String>>(2),
        None,
        "not reported is NULL"
    );
    assert_eq!(row.get::<_, Option<String>>(3), None);
    assert!(row.get::<_, bool>(4));
}

#[test]
fn a_fund_also_counted_in_cash_is_marked_on_its_row_and_its_position() {
    let store = store();
    let statement = opened(&store);
    let mut fund = resolved(&statement, &unique("INS-SPAXX"));
    fund.quantity = units("500");
    fund.market_value = Some(usd("500.00"));
    fund.also_counted_in_cash = true;
    let (account, instrument) = (fund.account_id.clone(), fund.instrument_id.clone().unwrap());
    let holding_id = fund.holding_id.clone();
    store.record(fund, &at(NOW)).unwrap();

    let held = store
        .custodial_position(&account, &instrument, Side::Long)
        .unwrap()
        .unwrap();
    assert!(held.also_counted_in_cash);
    assert_eq!(held.quantity.to_string(), "500", "kept as reported");

    let mut client = postgres::Client::connect(&base_url(), postgres::NoTls).unwrap();
    let marked: bool = client
        .query_one(
            "SELECT also_counted_in_cash FROM holding WHERE holding_id = $1",
            &[&holding_id],
        )
        .unwrap()
        .get(0);
    assert!(marked);
}

#[test]
fn a_statements_figures_are_kept_as_reported_and_read_back() {
    let store = store();
    let statement = Statement {
        source: "etrade".into(),
        external_statement_id: unique("84001234"),
        expected_rows: 1,
        figures: vec![
            Figures {
                segment: "securities".into(),
                buying_power: Some(usd("41250.00")),
                margin_requirement: Some(usd("18250.00")),
                maintenance_excess: None,
                net_liquidation: Some(usd("93550.00")),
                collateral: vec![Collateral {
                    direction: Direction::Posted,
                    instrument_id: Some("INS-UST10Y".into()),
                    unresolved_identifiers: vec![],
                    quantity: units("500000"),
                    value: Some(usd("487500.00")),
                    haircut: Some(units("0.02")),
                    value_after_haircut: None,
                    held_at: "the prime broker".into(),
                    reusable: Some(false),
                }],
                ..Default::default()
            },
            Figures {
                segment: "commodities".into(),
                initial_margin: Some(usd("8800.00")),
                ..Default::default()
            },
        ],
        currency_assumed: true,
        security_interest: Some(true),
        raw_record: None,
        provenance: Vec::new(),
        ..statement(&unique("ACC"))
    };
    let (opened, _, _) = store.open(statement.clone(), &at(NOW)).unwrap();
    let read = store.statement(&opened.statement_id).unwrap().unwrap();
    assert_eq!(
        read.figures, statement.figures,
        "in the order given, collateral too, reusable as reported"
    );
    assert!(read.currency_assumed);
    assert_eq!(read.security_interest, Some(true), "as reported (v8)");
    assert_eq!(
        read.figures[0].buying_power.as_ref().unwrap().to_string(),
        "41250.00 USD",
        "at the scale it was stated with"
    );

    // And a redelivery hands back the figures first recorded.
    let mut again = statement;
    again.statement_id = unique("STMT");
    let (redelivered, opened, _) = store.open(again, &at(NOW)).unwrap();
    assert_eq!(opened, Opened::AlreadyRecorded);
    assert_eq!(redelivered.figures[0].maintenance_excess, None);
    assert_eq!(redelivered.figures, read.figures);
}

#[test]
fn the_database_refuses_a_side_its_quantity_contradicts_and_a_value_without_its_currency() {
    let store = store();
    let statement = opened(&store);
    let mut client = postgres::Client::connect(&base_url(), postgres::NoTls).unwrap();
    for (side, quantity, value, currency, why) in [
        (
            "short",
            "5",
            Some("1"),
            Some("USD"),
            "a short row stating a positive quantity",
        ),
        (
            "long",
            "-5",
            Some("1"),
            Some("USD"),
            "a long row stating a negative quantity",
        ),
        (
            "sideways",
            "5",
            Some("1"),
            Some("USD"),
            "a side that is neither",
        ),
        (
            "long",
            "5",
            Some("1"),
            None,
            "an amount without its currency",
        ),
        (
            "long",
            "5",
            None,
            Some("USD"),
            "a currency without an amount",
        ),
    ] {
        let refused = client.execute(
            "INSERT INTO holding (holding_id, statement_id, account_id, instrument_id, side,
                                  quantity, market_value, currency)
             VALUES ($1, $2, 'ACC', 'INS', $3, $4::text::numeric, $5::text::numeric, $6)",
            &[
                &unique("HLD"),
                &statement.statement_id,
                &side,
                &quantity,
                &value,
                &currency,
            ],
        );
        assert!(refused.is_err(), "{why} was stored");
    }
}

#[test]
fn a_read_of_every_account_across_pages_sees_each_position_once() {
    // kernel/position-paging-skips-rows. A schema of its own, since every
    // account is every account in it.
    let (url, _client) = own_schema("paging");
    let store = PostgresStore::connect(&url, 1).expect("could not connect");
    store
        .migrate(&meridian_clock::SystemClock)
        .expect("could not apply the schema");
    // A statement each, since a statement is one account's (W2.2).
    let mut statements = std::collections::HashMap::new();
    for account in ["ACC-A", "ACC-B", "ACC-C"] {
        let opened = store.open(statement(account), &at(NOW)).unwrap().0;
        statements.insert(account, opened);
    }

    let rows = [
        ("ACC-A", "INS-Z", Side::Long, "1"),
        ("ACC-B", "INS-A", Side::Long, "2"),
        ("ACC-B", "INS-M", Side::Long, "3"),
        ("ACC-B", "INS-M", Side::Short, "-4"),
        ("ACC-C", "INS-A", Side::Short, "-5"),
    ];
    for (account, instrument, side, held) in rows {
        let mut row = resolved(&statements[account], instrument);
        row.account_id = account.into();
        row.side = side;
        row.quantity = units(held);
        store.record(row, &at(NOW)).unwrap();
    }
    let mut unresolved = resolved(&statements["ACC-B"], "unused");
    unresolved.account_id = "ACC-B".into();
    unresolved.instrument_id = None;
    unresolved.unresolved_identifiers = vec![Identifier {
        scheme: "symbol".into(),
        value: "ZZTOP".into(),
        source: "snaptrade".into(),
    }];
    store.record(unresolved, &at(NOW)).unwrap();

    for size in 1..=6 {
        let mut keys = Vec::new();
        let mut gaps = 0;
        let mut cursor = String::new();
        loop {
            let page = page(&store, "", true, size, &cursor);
            keys.extend(
                page.positions
                    .iter()
                    .map(|p| (p.account_id.clone(), p.instrument_id.clone(), p.side)),
            );
            gaps += page.unresolved.len();
            if page.next_cursor.is_empty() {
                break;
            }
            cursor = page.next_cursor;
        }
        let mut sorted = keys.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            keys, sorted,
            "in key order and none twice, at {size} a page"
        );
        assert_eq!(keys.len(), 5, "none skipped, at {size} a page: {keys:?}");
        assert_eq!(gaps, 1, "the unresolved row once, at {size} a page");
    }
}

#[test]
fn migration_four_reads_an_existing_rows_side_from_its_sign_and_its_silence_as_null() {
    // A database at version 3: a long row, a short one told only by its sign,
    // and a value never reported, kept then as a zero in no currency.
    let (url, mut client) = own_schema("four");
    client
        .batch_execute(meridian_street::migrations::HISTORY)
        .unwrap();
    for migration in &meridian_street::migrations::MIGRATIONS[..3] {
        client.batch_execute(migration.sql).unwrap();
        client
            .execute(
                "INSERT INTO schema_migration (version, name, applied_at_ns) VALUES ($1, $2, 1)",
                &[&migration.version, &migration.name],
            )
            .unwrap();
    }
    client
        .batch_execute(
            "INSERT INTO statement (statement_id, source, external_statement_id, as_of_date,
                                    read_at_ns, expected_rows)
             VALUES ('STMT-3', 'snaptrade', 'st-3', '2026-09-08', 1, 2);
             INSERT INTO holding (holding_id, statement_id, account_id, instrument_id, quantity,
                                  market_value, currency)
             VALUES ('HLD-L', 'STMT-3', 'ACC', 'INS', 12.5, 2812.50, 'USD'),
                    ('HLD-S', 'STMT-3', 'ACC', 'INS-2', -3, 0, '');
             INSERT INTO custodial_position (account_id, instrument_id, quantity, market_value,
                                             currency, last_statement_id, as_of_date,
                                             updated_at_ns)
             VALUES ('ACC', 'INS', 12.5, 2812.50, 'USD', 'STMT-3', '2026-09-08', 1),
                    ('ACC', 'INS-2', -3, 0, '', 'STMT-3', '2026-09-08', 1);",
        )
        .unwrap();

    let store = PostgresStore::connect(&url, 1).expect("could not connect");
    store
        .migrate(&meridian_clock::SystemClock)
        .expect("migration 4 did not apply");
    store.verify().expect("and the database is current");

    let long = store
        .custodial_position("ACC", "INS", Side::Long)
        .unwrap()
        .expect("the long row is long");
    assert_eq!(long.market_value.unwrap().to_string(), "2812.50 USD");
    let short = store
        .custodial_position("ACC", "INS-2", Side::Short)
        .unwrap()
        .expect("the negative row is short");
    assert_eq!(short.quantity.to_string(), "-3");
    assert_eq!(short.market_value, None, "never reported, so NULL");

    // And the key is the three columns: the other side of INS is a second
    // position, not a conflict.
    let mut other_side = resolved(&opened(&store), "INS");
    other_side.account_id = "ACC".into();
    other_side.side = Side::Short;
    other_side.quantity = units("-1");
    store.record(other_side, &at(NOW)).unwrap();
    assert_eq!(page(&store, "ACC", false, 100, "").positions.len(), 3);
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
    store.record(row, &at(NOW)).unwrap();

    let page = page(&store, &account, true, 100, "");
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
    let account = unique("ACC");
    let statement = Statement {
        expected_rows: 8,
        ..statement(&account)
    };
    let statement = store.open(statement, &at(NOW)).unwrap().0;

    let mut racing = Vec::new();
    for n in 0..8 {
        let store = store.clone();
        let statement = statement.clone();
        let account = account.clone();
        racing.push(std::thread::spawn(move || {
            let mut row = resolved(&statement, &format!("INS-{n}"));
            row.account_id = account;
            store.record(row, &at(NOW + n)).unwrap().1
        }));
    }

    let completions = racing
        .into_iter()
        .filter(|_| true)
        .map(|thread| thread.join().unwrap())
        .filter(|completion| matches!(completion, Completion::JustCompleted(_)))
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
        .open(
            Statement {
                expected_rows: 0,
                ..statement(&unique("ACC"))
            },
            &at(NOW),
        )
        .unwrap();

    assert_eq!(opened, Opened::Opened);
    assert!(matches!(completion, Completion::JustCompleted(_)));
}

#[test]
fn a_database_under_the_old_table_name_is_renamed_rather_than_left_behind() {
    // The first schema change the additive mechanism could not absorb, and the
    // reason kernel/ledger-needs-migrations existed. A database created before
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
    store
        .migrate(&meridian_clock::SystemClock)
        .expect("the rename did not apply");

    let carried = store.custodial_position("ACC", "INS", Side::Long).unwrap();
    let carried = carried.expect("the row was left behind under the old table name");
    // A bigint at 1e8 carried at the eight places it was stated with.
    assert_eq!(carried.quantity.to_string(), "0.00000001");

    client
        .batch_execute(&format!("DROP SCHEMA {scratch} CASCADE"))
        .unwrap();
}

// ── A placeholder replaced (W3.9) ──────────────────────────────────────────

/// A statement as of `as_of_date`, holding `quantity` of `instrument_id` in
/// `account`.
fn stated(
    store: &PostgresStore,
    account: &str,
    as_of_date: &str,
    instrument_id: &str,
    quantity: &str,
) -> Holding {
    let statement = Statement {
        as_of_date: as_of_date.into(),
        ..statement(account)
    };
    let statement = store.open(statement, &at(NOW)).unwrap().0;

    let mut holding = resolved(&statement, instrument_id);
    holding.account_id = account.to_string();
    holding.quantity = units(quantity);
    store.record(holding.clone(), &at(NOW)).unwrap();
    holding
}

#[test]
fn a_placeholders_position_moves_and_its_holding_row_keeps_the_placeholder() {
    let store = store();
    let account = unique("ACC");
    let placeholder = format!("LCL-{}", unique("p"));
    let instrument = unique("INS");
    let holding = stated(&store, &account, "2026-09-08", &placeholder, "5");
    assert!(store.instruments_held().unwrap().contains(&placeholder));

    let settled = store
        .move_positions(&placeholder, &instrument, &at(NOW))
        .unwrap();
    match settled.as_slice() {
        [Settled::Changed {
            position,
            previous_quantity,
        }, Settled::Changed {
            position: removed,
            previous_quantity: was,
        }] => {
            assert_eq!(position.instrument_id, instrument);
            assert_eq!(position.quantity.to_string(), "5");
            assert_eq!(*previous_quantity, Quantity::ZERO);
            // The placeholder's, a tombstone numbered after it.
            assert_eq!(removed.instrument_id, placeholder);
            assert!(removed.removed);
            assert_eq!(was.to_string(), "5");
            assert_eq!(removed.last_change.previous, position.last_change.sequence);
        }
        other => panic!("expected one moved position and its removal, got {other:?}"),
    }

    assert!(store
        .custodial_position(&account, &placeholder, Side::Long)
        .unwrap()
        .is_none());
    assert_eq!(
        store
            .custodial_position(&account, &instrument, Side::Long)
            .unwrap()
            .unwrap()
            .as_of_date,
        "2026-09-08"
    );

    // The row records what was reported, and what was reported named the
    // placeholder.
    let mut client = postgres::Client::connect(&base_url(), postgres::NoTls).unwrap();
    let recorded: Option<String> = client
        .query_one(
            "SELECT instrument_id FROM holding WHERE holding_id = $1",
            &[&holding.holding_id],
        )
        .unwrap()
        .get(0);
    assert_eq!(recorded.as_deref(), Some(placeholder.as_str()));

    // Nothing is left for the sweep to ask about.
    assert!(!store.instruments_held().unwrap().contains(&placeholder));

    // And hearing it again moves nothing.
    assert!(store
        .move_positions(&placeholder, &instrument, &at(NOW))
        .unwrap()
        .is_empty());
}

#[test]
fn where_both_are_held_a_later_placeholder_statement_stands() {
    let store = store();
    let account = unique("ACC");
    let placeholder = format!("LCL-{}", unique("p"));
    let instrument = unique("INS");
    stated(&store, &account, "2026-09-08", &instrument, "2");
    stated(&store, &account, "2026-09-09", &placeholder, "5");

    let settled = store
        .move_positions(&placeholder, &instrument, &at(NOW))
        .unwrap();
    match settled.as_slice() {
        [Settled::Changed {
            previous_quantity, ..
        }, Settled::Changed { position, .. }] => {
            assert_eq!(previous_quantity.to_string(), "2");
            assert!(position.removed, "and the placeholder's removed");
        }
        other => panic!("expected the placeholder's to stand, got {other:?}"),
    }

    let standing = store
        .custodial_position(&account, &instrument, Side::Long)
        .unwrap()
        .unwrap();
    assert_eq!(standing.quantity.to_string(), "5");
    assert_eq!(standing.as_of_date, "2026-09-09");
    assert!(store
        .custodial_position(&account, &placeholder, Side::Long)
        .unwrap()
        .is_none());
}

#[test]
fn where_both_are_held_a_later_instrument_statement_stands() {
    let store = store();
    let account = unique("ACC");
    let placeholder = format!("LCL-{}", unique("p"));
    let instrument = unique("INS");
    stated(&store, &account, "2026-09-08", &placeholder, "5");
    stated(&store, &account, "2026-09-09", &instrument, "2");

    let settled = store
        .move_positions(&placeholder, &instrument, &at(NOW))
        .unwrap();
    match settled.as_slice() {
        [Settled::Changed { position, .. }] => assert!(position.removed, "only the removal"),
        other => panic!("expected only the placeholder's removal, got {other:?}"),
    }

    let standing = store
        .custodial_position(&account, &instrument, Side::Long)
        .unwrap()
        .unwrap();
    assert_eq!(standing.quantity.to_string(), "2");
    assert!(store
        .custodial_position(&account, &placeholder, Side::Long)
        .unwrap()
        .is_none());
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

    store
        .migrate(&meridian_clock::SystemClock)
        .expect("could not apply the schema");

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

    store
        .migrate(&meridian_clock::SystemClock)
        .expect("could not apply the schema");
    let first: Vec<(i64, i64)> = client
        .query("SELECT version, applied_at_ns FROM schema_migration", &[])
        .expect("could not read the history")
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();

    store
        .migrate(&meridian_clock::SystemClock)
        .expect("migrating again must be a no-op");

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
        .batch_execute(include_str!("../migrations/0001_street.sql"))
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
    store
        .migrate(&meridian_clock::SystemClock)
        .expect("could not adopt and migrate");

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
    store
        .migrate(&meridian_clock::SystemClock)
        .expect("could not apply the schema");

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
    // Told apart from every other refusal at start, because a starting store
    // waits out the others and must not wait out this one.
    assert!(
        matches!(refused, meridian_street::store::StoreError::SchemaAhead(_)),
        "a schema ahead of this binary is not one waiting fixes: {refused:?}"
    );
}

// ── Every change numbered, read within a scope since a watermark (v7) ───────

fn read(
    store: &PostgresStore,
    scope: Scope,
    account: &str,
    since: Option<u64>,
) -> Vec<(String, u64, u64, bool)> {
    store
        .page(&Read {
            scope,
            account_id: account.to_string(),
            include_unresolved: false,
            limit: 500,
            cursor: String::new(),
            since,
        })
        .unwrap()
        .positions
        .into_iter()
        .map(|p| {
            (
                p.instrument_id,
                p.last_change.sequence,
                p.last_change.previous,
                p.removed,
            )
        })
        .collect()
}

#[test]
fn concurrent_changes_take_distinct_numbers_and_each_names_its_accounts_last() {
    // The head's row orders every change, so eight rows racing into one
    // account's positions take eight numbers, and the account's chain runs
    // through all eight with no gap: each names the one before it.
    let store = Arc::new(store());
    let account = unique("ACC");
    let statement = store
        .open(
            Statement {
                expected_rows: 100,
                ..statement(&account)
            },
            &at(NOW),
        )
        .unwrap()
        .0;
    let mut racing = Vec::new();
    for n in 0..8 {
        let store = store.clone();
        let statement = statement.clone();
        racing.push(std::thread::spawn(move || {
            match store
                .record(resolved(&statement, &format!("INS-{n}")), &at(NOW))
                .unwrap()
                .0
            {
                Settled::Changed { position, .. } => position.last_change,
                other => panic!("a new position is a change: {other:?}"),
            }
        }));
    }
    let mut changes: Vec<_> = racing.into_iter().map(|t| t.join().unwrap()).collect();
    changes.sort_by_key(|change| change.sequence);
    assert_eq!(changes[0].previous, 0, "the account's first");
    for pair in changes.windows(2) {
        assert!(pair[1].sequence > pair[0].sequence);
        assert_eq!(
            pair[1].previous, pair[0].sequence,
            "chained, whatever others used between"
        );
    }
}

#[test]
fn a_read_since_a_watermark_answers_what_changed_a_removal_included() {
    let store = store();
    let account = unique("ACC");
    let placeholder = format!("LCL-{}", unique("p"));
    let instrument = unique("INS");
    stated(&store, &account, "2026-09-08", &placeholder, "5");
    let before = store
        .page(&Read {
            scope: Scope::Everything,
            account_id: account.clone(),
            include_unresolved: false,
            limit: 10,
            cursor: String::new(),
            since: None,
        })
        .unwrap()
        .as_of;

    store
        .move_positions(&placeholder, &instrument, &at(NOW))
        .unwrap();

    let now = read(&store, Scope::Everything, &account, None);
    assert_eq!(now.len(), 1, "no tombstone without a watermark: {now:?}");
    let since = read(&store, Scope::Everything, &account, Some(before));
    let removed: Vec<_> = since.iter().filter(|p| p.3).map(|p| p.0.clone()).collect();
    assert_eq!(
        removed,
        std::slice::from_ref(&placeholder),
        "the removal, as a tombstone"
    );
    assert!(since.iter().all(|p| p.1 > before));
    assert!(store
        .custodial_position(&account, &placeholder, Side::Long)
        .unwrap()
        .is_none());
}

#[test]
fn a_plugins_read_answers_its_scope_an_empty_one_nothing_and_another_account_is_refused() {
    let store = store();
    let mine = unique("ACC");
    let theirs = unique("ACC");
    stated(&store, &mine, "2026-09-08", &unique("INS"), "1");
    stated(&store, &theirs, "2026-09-08", &unique("INS"), "2");

    let within =
        |accounts: &[&str]| Scope::Within(accounts.iter().map(|a| a.to_string()).collect());
    assert_eq!(read(&store, within(&[&mine]), "", None).len(), 1);
    assert!(
        read(&store, within(&[]), "", None).is_empty(),
        "never every account"
    );
    let refused = store
        .page(&Read {
            scope: within(&[&mine]),
            account_id: theirs.clone(),
            include_unresolved: false,
            limit: 10,
            cursor: String::new(),
            since: None,
        })
        .unwrap_err();
    assert!(refused
        .to_string()
        .contains("not in this plugin's read scope"));
}

#[test]
fn completed_statements_are_read_with_their_cause_figures_and_account() {
    let store = store();
    let account = unique("ACC");
    let cause = at(NOW);
    let (opened, _, completion) = store
        .open(
            Statement {
                expected_rows: 0,
                external_account_id: "SNAP-ACC-1".into(),
                institution: "Interactive Brokers".into(),
                figures: vec![Figures {
                    segment: String::new(),
                    buying_power: Some(usd("25000.00")),
                    ..Default::default()
                }],
                ..statement(&account)
            },
            &cause,
        )
        .unwrap();
    let Completion::JustCompleted(change) = completion else {
        panic!("no rows: complete at once")
    };
    // And one still open, which is not listed.
    store.open(statement(&account), &at(NOW)).unwrap();

    let page = store
        .statements(&meridian_street::store::StatementsRead {
            scope: Scope::Within([account.clone()].into_iter().collect()),
            account_id: String::new(),
            as_of_date: String::new(),
            limit: 10,
            cursor: String::new(),
            since: Some(change.sequence - 1),
        })
        .unwrap();
    let [(read, counts)] = page.statements.as_slice() else {
        panic!("one completed statement: {:?}", page.statements)
    };
    assert_eq!(read.statement_id, opened.statement_id);
    assert_eq!(read.institution, "Interactive Brokers");
    assert_eq!(read.figures[0].buying_power, Some(usd("25000.00")));
    assert_eq!(*counts, Counts::default());
    let completed = read.completed.as_ref().unwrap();
    assert_eq!(completed.change, change);
    assert_eq!(completed.cause, cause);
    assert!(page.as_of >= change.sequence);
}

#[test]
fn a_holdings_cost_and_lots_are_kept_on_the_row_and_the_position() {
    let store = store();
    let statement = opened(&store);
    let mut holding = resolved(&statement, &unique("INS"));
    holding.cost = meridian_street::Cost {
        cost_basis: None,
        average_cost: Some(usd("150.00")),
        lots: vec![
            meridian_street::Lot {
                quantity: units("10"),
                cost: Some(usd("-1500.00")),
                acquired_date: "2024-03-11".into(),
            },
            meridian_street::Lot {
                quantity: units("2.5"),
                cost: None,
                acquired_date: String::new(),
            },
        ],
        margin_requirement: Some(usd("703.13")),
        // Available and not, and the sub-balances, as reported, here not
        // fitting the holding's quantity: kept as reported, for the
        // reconciliation to flag.
        available_quantity: Some(units("5")),
        not_available_quantity: None,
        available_basis: 1,
        encumbrances: vec![
            meridian_street::Encumbrance {
                kind: 1,
                quantity: units("4"),
                available: Some(false),
                source_code: "PLED".into(),
                pledgee: "Interactive Brokers".into(),
                held_at: "DTC".into(),
                segment: String::new(),
                detail: String::new(),
            },
            meridian_street::Encumbrance {
                kind: 7,
                quantity: units("1.5"),
                available: None,
                source_code: "Not Segregated".into(),
                pledgee: String::new(),
                held_at: String::new(),
                segment: "securities".into(),
                detail: "as the statement says".into(),
            },
        ],
        raw_record: None,
        provenance: Vec::new(),
        pending: Vec::new(),
    };
    let account = holding.account_id.clone();
    let instrument = holding.instrument_id.clone().unwrap();
    store.record(holding.clone(), &at(NOW)).unwrap();

    let held = store
        .custodial_position(&account, &instrument, Side::Long)
        .unwrap()
        .unwrap();
    assert_eq!(
        held.cost, holding.cost,
        "as reported, the sign and the order kept"
    );
}

fn raw(key: &str) -> Option<meridian_street::RawRecord> {
    Some(meridian_street::RawRecord {
        instance_id: "custody-snaptrade-1".into(),
        key: key.into(),
    })
}

#[test]
fn what_the_edge_keeps_is_kept_on_the_row_the_position_and_the_statement() {
    // Contract v11: a row's raw record, provenance and pending quantities;
    // the position carries them from the row that last stated it.
    let store = store();
    let mut opening = statement(&unique("ACC"));
    opening.raw_record = raw("balances/A/1");
    opening.provenance = vec![meridian_street::Provenance {
        field: "institution".into(),
        kind: 4,
        rule: "the connection's brokerage name".into(),
        ..Default::default()
    }];
    let (opened, _, _) = store.open(opening.clone(), &at(NOW)).unwrap();
    let read = store.statement(&opened.statement_id).unwrap().unwrap();
    assert_eq!(read.raw_record, raw("balances/A/1"));
    assert_eq!(read.provenance, opening.provenance);

    let mut holding = resolved(&opened, &unique("INS"));
    holding.cost.raw_record = raw("positions/A/1");
    holding.cost.pending = vec![meridian_street::Pending {
        value_date: "2026-09-09".into(),
        quantity: units("2.5"),
    }];
    holding.cost.provenance = vec![meridian_street::Provenance {
        field: "settle_date_quantity".into(),
        kind: 4,
        rule: "the quantity less the trades not settled".into(),
        ..Default::default()
    }];
    let account = holding.account_id.clone();
    let instrument = holding.instrument_id.clone().unwrap();
    store.record(holding.clone(), &at(NOW + 1)).unwrap();
    let held = store
        .custodial_position(&account, &instrument, Side::Long)
        .unwrap()
        .unwrap();
    assert_eq!(held.cost.raw_record, raw("positions/A/1"));
    assert_eq!(held.cost.pending, holding.cost.pending);
    assert_eq!(held.cost.provenance, holding.cost.provenance);
}

#[test]
fn a_backfill_is_journaled_beside_the_row_and_run_twice_adds_nothing() {
    // W2.4, contract v11, in the database: an amendment row beside the row
    // as first recorded, which keeps its columns; the position the row last
    // stated takes the field as a change of its own; again, nothing.
    use meridian_street::store::{Amended, Amendment};
    let store = store();
    let opened = store.open(statement(&unique("ACC")), &at(NOW)).unwrap().0;
    let holding = resolved(&opened, &unique("INS"));
    let account = holding.account_id.clone();
    let instrument = holding.instrument_id.clone().unwrap();
    store.record(holding.clone(), &at(NOW + 1)).unwrap();
    let before = store
        .custodial_position(&account, &instrument, Side::Long)
        .unwrap()
        .unwrap();

    let amendment = Amendment {
        statement_id: opened.statement_id.clone(),
        account_id: account.clone(),
        instrument_id: Some(instrument.clone()),
        unresolved_identifiers: vec![],
        side: Side::Long,
        contract_version: "v11".into(),
        field: "raw_record".into(),
        raw_record: raw("positions/A/1"),
        pending: vec![],
        provenance: vec![],
    };
    match store.amend(amendment.clone(), &at(NOW + 2)).unwrap() {
        Amended::Amended(Settled::Changed { position, .. }) => {
            assert_eq!(position.cost.raw_record, raw("positions/A/1"));
            assert!(position.last_change.sequence > before.last_change.sequence);
        }
        other => panic!("the position should have changed: {other:?}"),
    }
    let after = store
        .custodial_position(&account, &instrument, Side::Long)
        .unwrap()
        .unwrap();
    assert_eq!(after.cost.raw_record, raw("positions/A/1"));
    assert_eq!(
        store.amend(amendment.clone(), &at(NOW + 3)).unwrap(),
        Amended::Nothing
    );

    let mut missing = amendment;
    missing.instrument_id = Some(unique("INS"));
    assert!(matches!(
        store.amend(missing, &at(NOW + 4)),
        Err(meridian_street::store::StoreError::NoSuchRow(_))
    ));
}

#[test]
fn migration_five_numbers_nothing_old_and_moves_a_statements_figures_into_a_set() {
    // A database at version 4: a completed statement with its flat figures,
    // and its row, which gives it its account.
    let (url, mut client) = own_schema("five");
    client
        .batch_execute(meridian_street::migrations::HISTORY)
        .unwrap();
    for migration in &meridian_street::migrations::MIGRATIONS[..4] {
        client.batch_execute(migration.sql).unwrap();
        client
            .execute(
                "INSERT INTO schema_migration (version, name, applied_at_ns) VALUES ($1, $2, 1)",
                &[&migration.version, &migration.name],
            )
            .unwrap();
    }
    client
        .batch_execute(
            "INSERT INTO statement (statement_id, source, external_statement_id, as_of_date,
                                    read_at_ns, expected_rows, completed_at_ns, buying_power,
                                    buying_power_currency, currency_assumed)
             VALUES ('STMT-4', 'etrade', 'st-4', '2026-09-08', 1, 1, 2, 41250.00, 'USD', true);
             INSERT INTO holding (holding_id, statement_id, account_id, instrument_id, side,
                                  quantity, market_value, currency)
             VALUES ('HLD-4', 'STMT-4', 'ACC-4', 'INS', 'long', 12.5, 2812.50, 'USD');
             INSERT INTO custodial_position (account_id, instrument_id, side, quantity,
                                             market_value, currency, last_statement_id,
                                             as_of_date, updated_at_ns)
             VALUES ('ACC-4', 'INS', 'long', 12.5, 2812.50, 'USD', 'STMT-4', '2026-09-08', 1);",
        )
        .unwrap();

    let store = PostgresStore::connect(&url, 1).expect("could not connect");
    store
        .migrate(&meridian_clock::SystemClock)
        .expect("migration 5 did not apply");
    store.verify().expect("and the database is current");

    let statement = store.statement("STMT-4").unwrap().unwrap();
    assert_eq!(statement.account_id, "ACC-4", "taken from its rows");
    assert!(statement.currency_assumed);
    let [figures] = statement.figures.as_slice() else {
        panic!("one set: {:?}", statement.figures)
    };
    assert_eq!(figures.segment, "");
    assert_eq!(figures.buying_power, Some(usd("41250.00")));
    let completed = statement.completed.expect("still complete");
    assert_eq!(completed.change.sequence, 0, "numbered before nothing");

    let position = store
        .custodial_position("ACC-4", "INS", Side::Long)
        .unwrap()
        .unwrap();
    assert_eq!(position.last_change.sequence, 0);
    let page = store
        .page(&Read {
            scope: Scope::Everything,
            account_id: String::new(),
            include_unresolved: false,
            limit: 10,
            cursor: String::new(),
            since: None,
        })
        .unwrap();
    assert_eq!(page.as_of, 0);

    // The next change is the partition's first, chained to nothing.
    let mut moved = resolved(&statement_open(&store, "ACC-4"), "INS");
    moved.quantity = units("13");
    match store.record(moved, &at(NOW)).unwrap().0 {
        Settled::Changed { position, .. } => {
            assert_eq!(
                (position.last_change.sequence, position.last_change.previous),
                (1, 0)
            )
        }
        other => panic!("{other:?}"),
    }
}

fn statement_open(store: &PostgresStore, account: &str) -> Statement {
    store.open(statement(account), &at(NOW)).unwrap().0
}

// ── The custodian's activity and each sync status (contract v14: W2.10 to W2.14) ──

fn decimal(text: &str) -> Option<meridian_pb::v1::Decimal> {
    Some(text.parse::<Exact>().unwrap().to_wire())
}

fn reinvestment(
    account: &str,
    id: &str,
    trade_date: &str,
) -> meridian_domain::v1::RecordActivityRequest {
    meridian_domain::v1::RecordActivityRequest {
        account_id: account.into(),
        external_account_id: format!("SNAP-{account}"),
        source: "snaptrade".into(),
        activity: Some(meridian_domain::v1::CustodialActivity {
            external_activity_id: id.into(),
            kind: meridian_domain::v1::ActivityKind::Reinvestment as i32,
            instrument_id: "INS-SPAXX".into(),
            trade_date: trade_date.into(),
            units: decimal("3.27"),
            description: "REINVESTMENT SPAXX".into(),
            ..Default::default()
        }),
    }
}

fn sync(
    account: &str,
    state: meridian_domain::v1::SyncState,
    history_from: &str,
) -> meridian_domain::v1::SyncStatusEvent {
    meridian_domain::v1::SyncStatusEvent {
        source: "snaptrade".into(),
        account_id: account.into(),
        external_account_id: format!("SNAP-{account}"),
        state: state as i32,
        history_from: history_from.into(),
        ..Default::default()
    }
}

fn named(account: &str) -> meridian_domain::v1::ListActivitiesRequest {
    meridian_domain::v1::ListActivitiesRequest {
        account_id: account.into(),
        ..Default::default()
    }
}

#[test]
fn an_activity_is_kept_once_whole_and_numbered_in_the_partition() {
    let store = store();
    let account = unique("ACC");
    let request = reinvestment(&account, "a3f0", "2026-09-30");

    let first = meridian_street::record_activity(&store, &request, &at(NOW)).unwrap();
    assert!(!first.reply.already_recorded);
    let event = first.event.expect("announced");
    assert_eq!(event.activity, request.activity, "kept whole, as sent");
    let journal = event.journal.clone().unwrap();
    assert!(journal.sequence > 0);
    assert_eq!(journal.previous_sequence, 0, "the account's first activity");

    let again = meridian_street::record_activity(&store, &request, &at(NOW + 1)).unwrap();
    assert!(again.reply.already_recorded);
    assert_eq!(again.reply.activity_id, first.reply.activity_id);
    assert!(again.event.is_none());

    let read =
        meridian_street::list_activities(&store, &named(&account), &Scope::Everything).unwrap();
    assert_eq!(read.activities, vec![event]);

    // Nothing is derived from it.
    assert!(store
        .custodial_position(&account, "INS-SPAXX", Side::Long)
        .unwrap()
        .is_none());
}

#[test]
fn activity_is_read_by_trade_date_paged_and_since_in_the_order_recorded() {
    let store = store();
    let account = unique("ACC");
    let mut sequences = Vec::new();
    for (id, date) in [
        ("c", "2026-09-30"),
        ("a", "2026-09-09"),
        ("b", "2026-09-15"),
    ] {
        let recorded =
            meridian_street::record_activity(&store, &reinvestment(&account, id, date), &at(NOW))
                .unwrap();
        sequences.push(recorded.event.unwrap().journal.unwrap().sequence);
    }
    let trade_dates = |reply: &meridian_domain::v1::ListActivitiesReply| -> Vec<String> {
        reply
            .activities
            .iter()
            .map(|a| a.activity.as_ref().unwrap().trade_date.clone())
            .collect()
    };

    let mut request = named(&account);
    request.trade_date_from = "2026-09-09".into();
    request.trade_date_to = "2026-09-30".into();
    request.page_size = 2;
    let first = meridian_street::list_activities(&store, &request, &Scope::Everything).unwrap();
    assert_eq!(trade_dates(&first), ["2026-09-09", "2026-09-15"]);
    request.cursor = first.next_cursor.clone();
    let second = meridian_street::list_activities(&store, &request, &Scope::Everything).unwrap();
    assert_eq!(trade_dates(&second), ["2026-09-30"]);
    assert!(second.next_cursor.is_empty());

    let mut since = named(&account);
    since.since = Some(meridian_domain::v1::Watermark {
        partitions: vec![meridian_domain::v1::PartitionSequence {
            partition: "street".into(),
            sequence: sequences[0],
        }],
    });
    let read = meridian_street::list_activities(&store, &since, &Scope::Everything).unwrap();
    assert_eq!(
        trade_dates(&read),
        ["2026-09-09", "2026-09-15"],
        "in the order recorded"
    );
}

#[test]
fn a_plugins_read_of_activity_is_within_its_scope() {
    let store = store();
    let mine = unique("ACC");
    let other = unique("ACC");
    meridian_street::record_activity(&store, &reinvestment(&mine, "1", "2026-09-01"), &at(NOW))
        .unwrap();
    meridian_street::record_activity(&store, &reinvestment(&other, "2", "2026-09-01"), &at(NOW))
        .unwrap();
    let scope = Scope::Within([mine.clone()].into_iter().collect());
    let read = meridian_street::list_activities(
        &store,
        &meridian_domain::v1::ListActivitiesRequest::default(),
        &scope,
    )
    .unwrap();
    assert!(read.activities.iter().all(|a| a.account_id == mine));
    assert!(!read.activities.is_empty());
    assert!(meridian_street::list_activities(&store, &named(&other), &scope).is_err());
}

#[test]
fn every_sync_status_is_kept_and_the_latest_answers_history_from() {
    let store = store();
    let account = unique("ACC");
    use meridian_domain::v1::SyncState;
    let first = meridian_street::record_sync_status(
        &store,
        &sync(&account, SyncState::Current, "2024-09-08"),
        &at(NOW),
    )
    .unwrap();
    let second = meridian_street::record_sync_status(
        &store,
        &sync(&account, SyncState::NeedsSignIn, "2024-10-04"),
        &at(NOW + 1),
    )
    .unwrap();
    assert_eq!(
        second.journal.as_ref().unwrap().previous_sequence,
        first.journal.as_ref().unwrap().sequence
    );

    assert_eq!(
        meridian_street::list_activities(&store, &named(&account), &Scope::Everything)
            .unwrap()
            .history_from,
        "2024-10-04"
    );

    let latest = meridian_street::list_sync_statuses(
        &store,
        &meridian_domain::v1::ListSyncStatusesRequest {
            account_id: account.clone(),
            ..Default::default()
        },
        &Scope::Everything,
    )
    .unwrap();
    assert_eq!(latest.statuses, vec![second.clone()]);

    let since = meridian_street::list_sync_statuses(
        &store,
        &meridian_domain::v1::ListSyncStatusesRequest {
            account_id: account.clone(),
            since: Some(meridian_domain::v1::Watermark {
                partitions: vec![meridian_domain::v1::PartitionSequence {
                    partition: "street".into(),
                    sequence: first.journal.as_ref().unwrap().sequence - 1,
                }],
            }),
            ..Default::default()
        },
        &Scope::Everything,
    )
    .unwrap();
    assert_eq!(since.statuses, vec![first, second]);
}

#[test]
fn an_unlinked_connections_sync_status_is_kept_and_answered_to_no_plugin() {
    let store = store();
    let external = unique("SNAP-ROTH");
    let mut unlinked = sync("", meridian_domain::v1::SyncState::Disabled, "");
    unlinked.external_account_id = external.clone();
    meridian_street::record_sync_status(&store, &unlinked, &at(NOW)).unwrap();

    let everything = meridian_street::list_sync_statuses(
        &store,
        &meridian_domain::v1::ListSyncStatusesRequest {
            page_size: 500,
            ..Default::default()
        },
        &Scope::Everything,
    )
    .unwrap();
    assert!(everything
        .statuses
        .iter()
        .any(|s| s.status.as_ref().unwrap().external_account_id == external));

    let scoped = meridian_street::list_sync_statuses(
        &store,
        &meridian_domain::v1::ListSyncStatusesRequest {
            page_size: 500,
            ..Default::default()
        },
        &Scope::Within(Default::default()),
    )
    .unwrap();
    assert!(scoped.statuses.is_empty());
}

// ── The gap before the street listened (decisions/031, point 4) ───────────

/// Whether each sync status kept for `external`'s connections, in the order
/// recorded, says nothing is known before it.
fn gaps(client: &mut postgres::Client, external: &str) -> Vec<(String, bool)> {
    client
        .query(
            "SELECT account_id, not_known_before <> '' FROM sync_status
              WHERE external_account_id = $1 ORDER BY sequence",
            &[&external],
        )
        .expect("could not read the sync statuses")
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect()
}

#[test]
fn a_connections_first_sync_status_records_that_nothing_is_known_before_it() {
    let store = store();
    let account = unique("ACC");
    let external = format!("SNAP-{account}");
    use meridian_domain::v1::SyncState;
    // Unlinked first, then linked: two connections, each its own first.
    let mut unlinked = sync("", SyncState::Current, "");
    unlinked.external_account_id = external.clone();
    meridian_street::record_sync_status(&store, &unlinked, &at(NOW)).unwrap();
    for (n, state) in [SyncState::Current, SyncState::NeedsSignIn]
        .into_iter()
        .enumerate()
    {
        meridian_street::record_sync_status(
            &store,
            &sync(&account, state, ""),
            &at(NOW + 1 + n as i64),
        )
        .unwrap();
    }

    let mut client = postgres::Client::connect(&base_url(), postgres::NoTls).unwrap();
    assert_eq!(
        gaps(&mut client, &external),
        vec![
            (String::new(), true),
            (account.clone(), true),
            (account.clone(), false),
        ]
    );
    let read = store
        .sync_statuses(&meridian_street::SyncStatusesRead {
            scope: Scope::Everything,
            account_id: account.clone(),
            limit: 10,
            cursor: String::new(),
            since: Some(0),
        })
        .unwrap();
    assert_eq!(
        read.statuses
            .iter()
            .map(|s| s.not_known_before.as_str())
            .collect::<Vec<_>>(),
        vec![meridian_street::SYNC_STATUS_NOT_KNOWN_BEFORE, ""]
    );

    // Once per connection: the database refuses a second.
    let second = client.execute(
        "UPDATE sync_status SET not_known_before = 'again'
          WHERE account_id = $1 AND not_known_before = ''",
        &[&account],
    );
    assert!(second.is_err(), "a connection was marked twice");
}

#[test]
fn sync_statuses_kept_before_the_gap_was_recorded_have_their_first_marked() {
    // A database that kept sync statuses under 0008: migrating marks each
    // connection's first, saying the migration did, and leaves the rest.
    let (url, mut client) = own_schema("gap");
    client
        .batch_execute(meridian_street::migrations::HISTORY)
        .unwrap();
    for migration in &meridian_street::migrations::MIGRATIONS[..8] {
        client.batch_execute(migration.sql).unwrap();
        client
            .execute(
                "INSERT INTO schema_migration (version, name, applied_at_ns) VALUES ($1, $2, 1)",
                &[&migration.version, &migration.name],
            )
            .unwrap();
    }
    client
        .batch_execute(
            "INSERT INTO sync_status
                    (sequence, previous_sequence, account_id, external_account_id, source,
                     record, recorded_at_ns)
             VALUES (1, 0, 'ACC-1', 'SNAP-1', 'snaptrade', '', 10),
                    (2, 1, 'ACC-1', 'SNAP-1', 'snaptrade', '', 20),
                    (3, 0, '', 'SNAP-9', 'snaptrade', '', 30);",
        )
        .unwrap();

    let store = PostgresStore::connect(&url, 1).unwrap();
    store.migrate(&meridian_clock::SystemClock).unwrap();
    let marked: Vec<(i64, String, i64)> = client
        .query(
            "SELECT sequence, not_known_before, recorded_at_ns FROM sync_status ORDER BY sequence",
            &[],
        )
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect();
    assert!(marked[0].1.contains("marked by migration 0009"));
    assert!(marked[1].1.is_empty());
    assert!(marked[2].1.contains("marked by migration 0009"));
    assert_eq!(
        marked.iter().map(|m| m.2).collect::<Vec<_>>(),
        vec![10, 20, 30],
        "a recorded time is never back-dated"
    );

    // And again, nothing changes.
    store.migrate(&meridian_clock::SystemClock).unwrap();
    let again: i64 = client
        .query_one(
            "SELECT count(*) FROM sync_status WHERE not_known_before <> ''",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(again, 2);
}

// ── An activity re-resolved (contract v15: W2.15, W2.16) ──

fn re_resolution(
    account: &str,
    id: &str,
    instrument: &str,
) -> meridian_domain::v1::ReResolveActivityRequest {
    meridian_domain::v1::ReResolveActivityRequest {
        account_id: account.into(),
        external_account_id: format!("SNAP-{account}"),
        source: "snaptrade".into(),
        external_activity_id: id.into(),
        instrument_id: instrument.into(),
        provenance: Some(meridian_pb::v1::Provenance {
            field: "instrument_id".into(),
            kind: meridian_pb::v1::ProvenanceKind::Supplied as i32,
            person: "Ada Park, in the plan-code links".into(),
            ..Default::default()
        }),
        resolved_at_ns: NOW - 60,
    }
}

#[test]
fn a_re_resolution_is_kept_beside_the_activity_chained_apart_and_read_with_it() {
    let store = store();
    let account = unique("ACC");
    let activity = meridian_street::record_activity(
        &store,
        &reinvestment(&account, "oqkr-1", "2026-09-30"),
        &at(NOW),
    )
    .unwrap()
    .event
    .unwrap();

    let first = meridian_street::re_resolve_activity(
        &store,
        &re_resolution(&account, "oqkr-1", "INS-VIGIX"),
        &at(NOW + 1),
    )
    .unwrap();
    assert_eq!(first.reply.activity_id, activity.activity_id);
    let event = first.event.expect("announced");
    let re = event.re_resolution.clone().unwrap();
    assert_eq!(re.account_id, account);
    assert_eq!(re.recorded_at_ns, NOW + 1);
    assert_eq!(re.resolved_at_ns, NOW - 60);
    assert_eq!(event.journal, re.journal);
    let journal = re.journal.clone().unwrap();
    assert!(journal.sequence > activity.journal.as_ref().unwrap().sequence);
    assert_eq!(
        journal.previous_sequence, 0,
        "chained apart from the activities"
    );

    // Sent again: nothing recorded, nothing numbered.
    let again = meridian_street::re_resolve_activity(
        &store,
        &re_resolution(&account, "oqkr-1", "INS-VIGIX"),
        &at(NOW + 2),
    )
    .unwrap();
    assert!(again.reply.already_recorded && again.event.is_none());
    let changed = meridian_street::re_resolve_activity(
        &store,
        &re_resolution(&account, "oqkr-1", ""),
        &at(NOW + 3),
    )
    .unwrap()
    .event
    .unwrap();
    assert_eq!(
        changed
            .re_resolution
            .as_ref()
            .unwrap()
            .journal
            .as_ref()
            .unwrap()
            .previous_sequence,
        journal.sequence
    );

    let read =
        meridian_street::list_activities(&store, &named(&account), &Scope::Everything).unwrap();
    assert_eq!(read.activities, vec![activity.clone()], "as first recorded");
    assert_eq!(
        read.re_resolutions,
        vec![re.clone(), changed.re_resolution.clone().unwrap()]
    );

    // Since the activity: the two re-resolutions, a page each.
    let mut since = named(&account);
    since.since = Some(meridian_domain::v1::Watermark {
        partitions: vec![meridian_domain::v1::PartitionSequence {
            partition: "street".into(),
            sequence: activity.journal.as_ref().unwrap().sequence,
        }],
    });
    since.page_size = 1;
    let one = meridian_street::list_activities(&store, &since, &Scope::Everything).unwrap();
    assert_eq!(
        (one.activities.len(), one.re_resolutions.clone()),
        (0, vec![re])
    );
    since.cursor = one.next_cursor;
    let two = meridian_street::list_activities(&store, &since, &Scope::Everything).unwrap();
    assert_eq!(two.re_resolutions, vec![changed.re_resolution.unwrap()]);
    assert!(two.next_cursor.is_empty());

    // Another account's scope reads none of it.
    let elsewhere = Scope::Within([unique("ACC")].into_iter().collect());
    let none = meridian_street::list_activities(
        &store,
        &meridian_domain::v1::ListActivitiesRequest {
            since: since.since.clone(),
            ..Default::default()
        },
        &elsewhere,
    )
    .unwrap();
    assert!(none.re_resolutions.is_empty());

    let refused = meridian_street::re_resolve_activity(
        &store,
        &re_resolution(&account, "never-sent", "INS-VIGIX"),
        &at(NOW + 4),
    )
    .unwrap_err();
    assert!(
        matches!(refused, StoreError::NoSuchActivity(_)),
        "{refused}"
    );
}
