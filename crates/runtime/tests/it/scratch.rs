//! The store tests' [`Scratch`], against Postgres (PO 65; review c5p M2):
//! a schema or a database a test made beside the test database's own is
//! dropped with the last clone of its URL, a failed test's too, as its stack
//! unwinds. `make test-store` fails a run that leaves any behind, and this is
//! the failure path that check alone never sees. Run by `make test-store`;
//! fails loudly without a database.

use std::sync::atomic::{AtomicU64, Ordering};

use meridian_changes::scratch::Scratch;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn base_url() -> String {
    std::env::var("MERIDIAN_TEST_DATABASE_URL").expect(
        "MERIDIAN_TEST_DATABASE_URL is not set. These tests need a real Postgres; \
         run them with `make test-store`.",
    )
}

fn unique(tag: &str) -> String {
    let seq = COUNTER.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("scratch_{tag}_{nanos}_{seq}")
}

/// Whether schema `schema` is in the test database, and database `database`
/// on its server.
fn present(schema: &str, database: &str) -> (bool, bool) {
    let mut admin = postgres::Client::connect(&base_url(), postgres::NoTls)
        .expect("could not reach the test database");
    let schemas: i64 = admin
        .query_one(
            "SELECT count(*) FROM pg_namespace WHERE nspname = $1",
            &[&schema],
        )
        .unwrap()
        .get(0);
    let databases: i64 = admin
        .query_one(
            "SELECT count(*) FROM pg_database WHERE datname = $1",
            &[&database],
        )
        .unwrap()
        .get(0);
    (schemas == 1, databases == 1)
}

/// What a test made, used as a store uses it: a table in the schema, and a
/// connection to each, each still open as the Scratch goes (a database's
/// drop ends a pool's connections there).
fn made_and_used(schema: &Scratch, database: &Scratch, held: &mut Vec<postgres::Client>) {
    let mut on_schema = postgres::Client::connect(schema, postgres::NoTls).unwrap();
    on_schema
        .batch_execute("CREATE TABLE made (n bigint)")
        .unwrap();
    held.push(on_schema);
    held.push(postgres::Client::connect(database, postgres::NoTls).unwrap());
}

#[test]
fn what_a_test_made_is_dropped_with_its_last_clone() {
    let (schema, database) = (unique("schema"), unique("database"));
    let mut held = Vec::new();
    {
        let made = Scratch::schema(&base_url(), &schema);
        let on = Scratch::database(&base_url(), &database);
        made_and_used(&made, &on, &mut held);
        let clones = (made.clone(), on.clone());
        drop((made, on));
        assert_eq!(
            present(&schema, &database),
            (true, true),
            "kept while a clone is"
        );
        drop(clones);
    }
    assert_eq!(present(&schema, &database), (false, false));
}

#[test]
fn what_a_failed_test_made_is_dropped_as_it_unwinds() {
    let (schema, database) = (unique("schema"), unique("database"));
    // Outlives the unwind: the connections are open as the Scratch goes.
    let mut held = Vec::new();
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let made = Scratch::schema(&base_url(), &schema);
        let on = Scratch::database(&base_url(), &database);
        made_and_used(&made, &on, &mut held);
        assert_eq!(present(&schema, &database), (true, true));
        panic!("a store test that fails");
    }));
    assert!(failed.is_err());
    assert_eq!(
        present(&schema, &database),
        (false, false),
        "dropped as the failed test's stack unwound"
    );
}
