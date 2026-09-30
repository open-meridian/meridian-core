//! The dashboard's own tables, against Postgres.
//!
//! What only the database can show: the schema applied over a database made
//! before it had a history, terminal sessions standing across a restart --
//! a second store over the same tables -- ended and lapsed as the memory
//! store ends and lapses them, and no token anywhere in them. Run by
//! `make test-store`; fails loudly without a database.

use std::sync::atomic::{AtomicU64, Ordering};

use meridian_dashboard::accounts::{self, Accounts as _, LocalAccount};
use meridian_dashboard::database::{Database, Unverified};
use meridian_dashboard::session::{ABSOLUTE_NS, IDLE_NS};
use meridian_dashboard::terminal::{hashed, InPostgres, Person, Refusal, TerminalSessions};

static COUNTER: AtomicU64 = AtomicU64::new(0);

const T0: i64 = 1_790_380_800_000_000_000;
const MINUTE_NS: i64 = 60_000_000_000;

fn base_url() -> String {
    std::env::var("MERIDIAN_TEST_DATABASE_URL").expect(
        "MERIDIAN_TEST_DATABASE_URL is not set. These tests need a real Postgres; \
         run them with `make test-store`.",
    )
}

/// A schema of this test's own, and the URL that reaches it.
fn scratch(tag: &str) -> String {
    let seq = COUNTER.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!("dashboard_{tag}_{nanos}_{seq}");
    postgres::Client::connect(&base_url(), postgres::NoTls)
        .expect("could not reach the test database")
        .batch_execute(&format!("CREATE SCHEMA {name}"))
        .expect("could not create a schema");
    format!("{}?options=-c%20search_path%3D{name}", base_url())
}

fn migrated(tag: &str) -> (Database, String) {
    let url = scratch(tag);
    let database = Database::connect(&url, 2).expect("connects");
    assert!(
        matches!(database.verify(), Err(Unverified::NotYet(_))),
        "an empty schema is waited for, not served"
    );
    database.migrate().expect("migrates");
    database.migrate().expect("migrating twice is a no-op");
    database.verify().expect("recognised after migrating");
    (database, url)
}

fn ada(at: i64) -> Person {
    Person {
        subject: "local|ada".into(),
        display_name: "Ada Park".into(),
        directory_groups: vec!["Admins".into(), "desk".into()],
        signed_in_at_ns: at,
    }
}

fn bob(at: i64) -> Person {
    Person {
        subject: "local|bob".into(),
        display_name: "Bob".into(),
        directory_groups: vec![],
        signed_in_at_ns: at,
    }
}

/// Every row of both tables, as text, as a database dump would show them.
fn dump(url: &str) -> String {
    let mut client = postgres::Client::connect(url, postgres::NoTls).unwrap();
    let mut all = String::new();
    for table in [
        "dashboard_terminal_session",
        "dashboard_terminal_session_gone",
    ] {
        for row in client
            .query(&format!("SELECT row_to_json(t)::text FROM {table} t"), &[])
            .unwrap()
        {
            all.push_str(row.get::<_, &str>(0));
            all.push('\n');
        }
    }
    all
}

fn count(url: &str, table: &str) -> i64 {
    postgres::Client::connect(url, postgres::NoTls)
        .unwrap()
        .query_one(&format!("SELECT count(*) FROM {table}"), &[])
        .unwrap()
        .get(0)
}

#[test]
fn a_database_from_before_the_history_is_brought_up_to_date() {
    // What a release before this one left: the accounts table, an account
    // in it, and no record of either.
    let url = scratch("before");
    postgres::Client::connect(&url, postgres::NoTls)
        .unwrap()
        .batch_execute(include_str!("../migrations/0001_local_account.sql"))
        .unwrap();
    let database = Database::connect(&url, 2).unwrap();
    accounts::InPostgres::on(database.clone())
        .put(&LocalAccount {
            name: "ada".into(),
            display_name: "Ada Park".into(),
            password_hash: "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$aGFzaGhhc2g".into(),
            created_at_ns: T0,
            ..Default::default()
        })
        .unwrap();
    assert!(
        matches!(database.verify(), Err(Unverified::NotYet(_))),
        "no history is a schema to migrate"
    );

    database.migrate().expect("migrates");
    database.verify().expect("recognised");
    assert!(
        accounts::InPostgres::on(database)
            .by_name("ada")
            .unwrap()
            .is_some(),
        "and the account made before is still there"
    );
}

#[test]
fn the_local_accounts_are_listed_by_name_with_their_display_names_and_nothing_else() {
    let (database, _) = migrated("people");
    let store = accounts::InPostgres::on(database);
    for (name, display) in [("bob", ""), ("Ada", "Ada Park")] {
        store
            .put(&LocalAccount {
                name: name.into(),
                display_name: display.into(),
                password_hash: "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$aGFzaGhhc2g".into(),
                created_at_ns: T0,
                ..Default::default()
            })
            .unwrap();
    }
    assert_eq!(
        store.people().unwrap(),
        [
            ("ada".to_string(), "Ada Park".to_string()),
            ("bob".to_string(), String::new())
        ]
    );
}

#[test]
fn a_schema_newer_than_this_binary_is_refused_not_waited_for() {
    let (database, url) = migrated("ahead");
    postgres::Client::connect(&url, postgres::NoTls)
        .unwrap()
        .execute(
            "INSERT INTO dashboard_schema_migration (version, name, applied_at_ns)
             VALUES (999, 'from_the_future', 0)",
            &[],
        )
        .unwrap();
    assert!(matches!(database.verify(), Err(Unverified::Ahead(_))));
}

#[test]
fn a_session_stands_across_a_restart_and_is_touched_by_whichever_store_meets_it() {
    let (database, url) = migrated("restart");
    let key = hashed("a-token-a-terminal-holds");
    InPostgres::on(database.clone())
        .keep(&key, &ada(T0), T0)
        .unwrap();
    drop(database);

    // A new dashboard: a new pool, over the same tables.
    let after = InPostgres::on(Database::connect(&url, 2).unwrap());
    assert_eq!(after.find(&key, T0 + 20 * MINUTE_NS).unwrap(), Ok(ada(T0)));
    // Touched then, so idle is counted from that use and not the first.
    assert_eq!(
        after.find(&key, T0 + 20 * MINUTE_NS + IDLE_NS).unwrap(),
        Ok(ada(T0))
    );
    assert!(after.is_live(&key, T0 + 20 * MINUTE_NS + IDLE_NS).unwrap());
    // And never past 12 hours from the sign-in.
    assert_eq!(
        after.find(&key, T0 + ABSOLUTE_NS + 1).unwrap(),
        Err(Refusal::Lapsed)
    );
}

#[test]
fn an_expired_session_is_refused_and_removed_on_use_leaving_only_why() {
    let (database, url) = migrated("expired");
    let store = InPostgres::on(database);
    let key = hashed("an-idle-token");
    store.keep(&key, &ada(T0), T0).unwrap();

    assert_eq!(
        store.find(&key, T0 + IDLE_NS + 1).unwrap(),
        Err(Refusal::Lapsed)
    );
    assert_eq!(count(&url, "dashboard_terminal_session"), 0, "removed then");
    assert_eq!(count(&url, "dashboard_terminal_session_gone"), 1);
    assert_eq!(
        store.find(&key, T0 + IDLE_NS + 2).unwrap(),
        Err(Refusal::Lapsed),
        "and keeps saying so"
    );
    assert!(!store.is_live(&key, T0).unwrap());
}

#[test]
fn signing_out_and_an_admin_ending_them_remove_the_rows_and_say_ended() {
    let (database, url) = migrated("ended");
    let one = InPostgres::on(database.clone());
    let other = InPostgres::on(database);
    let (a, b, c) = (hashed("one"), hashed("two"), hashed("three"));
    one.keep(&a, &ada(T0), T0).unwrap();
    one.keep(&b, &ada(T0), T0).unwrap();
    one.keep(&c, &bob(T0), T0).unwrap();

    // Ended through one, refused through the other: one table, every replica.
    other.end(&a).unwrap();
    assert_eq!(one.find(&a, T0).unwrap(), Err(Refusal::Ended));
    other.end(&a).unwrap();
    other.end(&hashed("never-issued")).unwrap();

    assert_eq!(
        one.holders(T0).unwrap(),
        vec![
            ("local|ada".to_string(), "Ada Park".to_string(), 1),
            ("local|bob".to_string(), "Bob".to_string(), 1),
        ]
    );
    assert_eq!(other.end_person("local|ada").unwrap(), 1, "per person");
    assert_eq!(one.find(&b, T0).unwrap(), Err(Refusal::Ended));
    assert_eq!(one.find(&c, T0).unwrap(), Ok(bob(T0)), "and nobody else's");
    assert_eq!(count(&url, "dashboard_terminal_session"), 1);
    assert_eq!(
        one.holders(T0).unwrap(),
        vec![("local|bob".to_string(), "Bob".to_string(), 1)]
    );
    assert_eq!(
        one.find(&hashed("never-issued"), T0).unwrap(),
        Err(Refusal::Unknown)
    );
}

#[test]
fn a_sweep_removes_what_lapsed_and_forgets_why_once_it_no_longer_matters() {
    let (database, url) = migrated("sweep");
    let store = InPostgres::on(database);
    let (stale, fresh) = (hashed("stale"), hashed("fresh"));
    store.keep(&stale, &ada(T0), T0).unwrap();
    store
        .keep(&fresh, &bob(T0 + IDLE_NS), T0 + IDLE_NS)
        .unwrap();

    store.sweep(T0 + IDLE_NS + MINUTE_NS).unwrap();
    assert_eq!(count(&url, "dashboard_terminal_session"), 1);
    assert_eq!(
        store.find(&stale, T0 + IDLE_NS + MINUTE_NS).unwrap(),
        Err(Refusal::Lapsed)
    );
    assert!(store
        .find(&fresh, T0 + IDLE_NS + MINUTE_NS)
        .unwrap()
        .is_ok());

    store.sweep(T0 + ABSOLUTE_NS + 1).unwrap();
    assert_eq!(
        count(&url, "dashboard_terminal_session_gone"),
        1,
        "the stale one's reason is past its 12 hours; the fresh one just lapsed"
    );
    store.sweep(T0 + IDLE_NS + ABSOLUTE_NS + 1).unwrap();
    assert_eq!(count(&url, "dashboard_terminal_session_gone"), 0);
}

#[test]
fn no_token_is_kept_only_its_hash() {
    let (database, url) = migrated("hash");
    let store = InPostgres::on(database);
    let token = "dG9rZW4tYS10ZXJtaW5hbC1ob2xkcy1hbmQtbm9ib2R5LWVsc2U";
    store.keep(&hashed(token), &ada(T0), T0).unwrap();
    let ended = "ZW5kZWQtdG9rZW4tYS10ZXJtaW5hbC1vbmNlLWhlbGQtbm93LWdvbmU";
    store.keep(&hashed(ended), &ada(T0), T0).unwrap();
    store.end(&hashed(ended)).unwrap();

    let dumped = dump(&url);
    assert!(dumped.contains(&hashed(token)), "{dumped}");
    assert!(dumped.contains(&hashed(ended)), "{dumped}");
    assert!(!dumped.contains(token), "a token in the table: {dumped}");
    assert!(!dumped.contains(ended), "a token in the reasons: {dumped}");
    let reasons: String = postgres::Client::connect(&url, postgres::NoTls)
        .unwrap()
        .query_one(
            "SELECT string_agg(row_to_json(t)::text, '') FROM dashboard_terminal_session_gone t",
            &[],
        )
        .unwrap()
        .get(0);
    assert!(
        !reasons.contains("ada"),
        "a reason row holds nothing about whose it was: {reasons}"
    );
}
