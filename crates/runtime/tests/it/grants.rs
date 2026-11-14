//! What the migration grants the serving role, against Postgres.
//!
//! The gap this closes was found on a cluster: the wizard makes two roles, the
//! migration makes tables owned by the migrating one, and every component then
//! reports an empty schema because the serving role can read none of them.
//! Run by `make test-store`; fails loudly without a database.

use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn base_url() -> String {
    std::env::var("MERIDIAN_TEST_DATABASE_URL").expect(
        "MERIDIAN_TEST_DATABASE_URL is not set. These tests need a real Postgres; \
         run them with `make test-store`.",
    )
}

/// Two roles and a schema of this test's own, as a wizard would leave them:
/// one that may create, one that may not.
fn two_roles(tag: &str) -> (String, String, String) {
    let seq = COUNTER.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let schema = format!("grants_{tag}_{nanos}_{seq}");
    let serving = format!("{schema}_app");
    let migrating = format!("{schema}_migrate");

    let mut admin = postgres::Client::connect(&base_url(), postgres::NoTls)
        .expect("could not reach the test database");
    admin
        .batch_execute(&format!(
            "create schema {schema};
             create role {serving} login password 'serving-test';
             create role {migrating} login password 'migrating-test';
             grant usage on schema {schema} to {migrating};
             grant create on schema {schema} to {migrating};
             -- Usage on the schema is the schema owner's to grant, which here
             -- is whoever made the database. The wizard checks for it and the
             -- migration cannot supply it.
             grant usage on schema {schema} to {serving};"
        ))
        .expect("could not make the roles");

    let schema_for_url = schema.clone();
    let url = |role: &str, password: &str| {
        let schema = &schema_for_url;
        let base = base_url();
        let (scheme, rest) = base.split_once("://").expect("a postgres url");
        let host = rest.rsplit_once('@').map(|(_, host)| host).unwrap_or(rest);
        // The schema in the URL, as a deployment's own connection carries it.
        let separator = if host.contains('?') { "&" } else { "?" };
        format!("{scheme}://{role}:{password}@{host}{separator}options=-c%20search_path%3D{schema}")
    };
    (
        schema,
        url(&serving, "serving-test"),
        url(&migrating, "migrating-test"),
    )
}

#[test]
fn the_migrating_role_works_in_the_schema_its_url_names() {
    // The serving role's own `current_schema()` is null until it has usage,
    // which is the very thing this grants: only the migrating role's is asked.
    let (schema, _serving, migrating) = two_roles("where");
    let seen: String = postgres::Client::connect(&migrating, postgres::NoTls)
        .expect("connects")
        .query_one("select current_schema()::text", &[])
        .expect("asks")
        .get(0);
    assert_eq!(seen, schema);
}

#[test]
fn the_serving_role_can_read_what_the_migrating_role_made() {
    let (schema, serving, migrating) = two_roles("read");

    let mut migrator = postgres::Client::connect(&migrating, postgres::NoTls).expect("connects");
    migrator
        .batch_execute(&format!(
            "create table {schema}.holding (id text primary key)"
        ))
        .expect("the migrating role may create");

    // Before: the table exists and the serving role cannot see into it, which
    // every component reports as a database with no schema.
    let mut server = postgres::Client::connect(&serving, postgres::NoTls).expect("connects");
    assert!(
        server
            .query_one(&format!("select count(*) from {schema}.holding"), &[])
            .is_err(),
        "a table its owner has not shared is not readable"
    );

    meridian_runtime::grant_serving(&migrating, &serving).expect("grants");

    let mut server = postgres::Client::connect(&serving, postgres::NoTls).expect("connects");
    server
        .execute(
            &format!("insert into {schema}.holding (id) values ($1)"),
            &[&"H-1"],
        )
        .expect("and may write");
    let count: i64 = server
        .query_one(&format!("select count(*) from {schema}.holding"), &[])
        .expect("and may read")
        .get(0);
    assert_eq!(count, 1);
}

#[test]
fn a_table_made_after_the_grant_is_readable_too() {
    // Default privileges, so the next release's migration needs no second
    // grant to be readable.
    let (schema, serving, migrating) = two_roles("later");
    meridian_runtime::grant_serving(&migrating, &serving).expect("grants");

    postgres::Client::connect(&migrating, postgres::NoTls)
        .expect("connects")
        .batch_execute(&format!(
            "create table {schema}.later (id text primary key)"
        ))
        .expect("creates");

    let count: i64 = postgres::Client::connect(&serving, postgres::NoTls)
        .expect("connects")
        .query_one(&format!("select count(*) from {schema}.later"), &[])
        .expect("reads a table made after the grant")
        .get(0);
    assert_eq!(count, 0);
}

#[test]
fn one_connection_for_both_is_left_alone() {
    let (_schema, _serving, migrating) = two_roles("same");

    // An administrator who supplied a single URL has one role, and there is
    // nothing to grant: the tables are already theirs.
    meridian_runtime::grant_serving(&migrating, &migrating).expect("a no-op");
}

#[test]
fn a_role_name_that_is_not_one_is_refused() {
    let refusal = meridian_runtime::grant_serving(
        "postgres://migrate:p@db:5432/meridian",
        "postgres://robert%22%3B%20drop%20table%20holding%3B%20--:p@db:5432/meridian",
    )
    .expect_err("a name that is not a name");

    assert!(refusal.contains("is not a role name"), "{refusal}");
}

/// Step 3d (section 6.2): the lake, migrated by one role and serving as
/// another that may not create in the schema, still makes each range's and
/// month's table as its writer first needs them -- through the functions
/// migration 23 made, run as `lake_row`'s owner -- and records there, a
/// split's children's too.
#[test]
fn the_lake_serving_role_makes_its_partitions_through_the_owners_functions() {
    use meridian_domain::v1::{Money, ObservationMeta, Source, SubjectRef, Trade};
    use meridian_lake::row::Observation;
    use meridian_lake::store::Store as _;
    let trade = |row_key: &str, subject: &str, valid_from_ns: i64| {
        Observation::Trade(Box::new(Trade {
            meta: Some(ObservationMeta {
                row_key: row_key.into(),
                subjects: vec![SubjectRef {
                    entity_id: subject.into(),
                    ..Default::default()
                }],
                source: Some(Source {
                    instance: "coinbase-1".into(),
                    dataset: "live".into(),
                    ..Default::default()
                }),
                valid_from_ns,
                ..Default::default()
            }),
            price: Some(Money {
                amount: Some(meridian_pb::v1::Decimal {
                    high: 0,
                    low: 62_000,
                    scale: 0,
                }),
                currency_code: "USD".into(),
                instrument_id: "LCL-USD".into(),
            }),
            quantity: Some(meridian_pb::v1::Decimal {
                high: 0,
                low: 5,
                scale: 1,
            }),
            ..Default::default()
        }))
    };
    let (schema, serving, migrating) = two_roles("lake");
    let migrator = meridian_lake::PostgresStore::connect(&migrating, 2).expect("connects");
    migrator
        .migrate(&meridian_clock::SystemClock)
        .expect("the migrating role migrates");
    drop(migrator);
    meridian_runtime::grant_serving(&migrating, &serving).expect("grants");

    // The serving role may not create a table of its own.
    let mut server = postgres::Client::connect(&serving, postgres::NoTls).expect("connects");
    assert!(
        server
            .batch_execute(&format!("create table {schema}.mine (id text)"))
            .is_err(),
        "the serving role creates nothing itself"
    );
    let leaves = |server: &mut postgres::Client| -> i64 {
        server
            .query_one(
                "SELECT count(*) FROM pg_class WHERE relname LIKE 'lake\\_row\\_%' \
                 AND relkind IN ('r', 'p') AND relnamespace = current_schema()::regnamespace",
                &[],
            )
            .expect("reads")
            .get(0)
    };
    let before = leaves(&mut server);

    let lake = meridian_lake::PostgresStore::connect(&serving, 2).expect("connects");
    let dataset = "coinbase-1:live";
    let month = 1_790_380_800_000_000_000;
    let next_month = month + 31 * 86_400 * 1_000_000_000;
    let recorded = lake
        .record(dataset, vec![trade("T-1", "INS-BTC", month)], month)
        .expect("the serving role records into a range and a month it makes");
    assert_eq!(recorded.recorded, 1);
    lake.record(
        dataset,
        vec![trade("T-2", "INS-BTC", next_month)],
        next_month,
    )
    .expect("and into the next month's");
    let after = leaves(&mut server);
    assert_eq!(after - before, 3, "the range's table and two months'");

    // A split's children's tables, made the same way.
    lake.split_partition(
        dataset,
        "local|grants",
        "a split as the serving role",
        month,
    )
    .expect("the serving role splits");
    lake.record(dataset, vec![trade("T-3", "INS-BTC", month)], month)
        .expect("and records in a child");
    assert!(
        leaves(&mut server) >= after + 3,
        "two children's tables and a month"
    );

    // Only that narrow job: a crafted argument -- a quote, a `;`, another
    // table's name, a range or a month out of its shape -- is refused, and
    // nothing is made or dropped.
    let held = leaves(&mut server);
    let tables = |server: &mut postgres::Client| -> i64 {
        server
            .query_one(
                "SELECT count(*) FROM pg_class WHERE relkind IN ('r', 'p') \
                 AND relnamespace = current_schema()::regnamespace",
                &[],
            )
            .expect("reads")
            .get(0)
    };
    let every = tables(&mut server);
    for partition in [
        "x'); DROP TABLE lake_row; --",
        "live; DROP TABLE lake_latest",
        "lake_latest\"",
        "public.lake_row x",
        "coinbase-1:live/8000-7fff",
        "coinbase-1:live/0000-FFFF",
        "coinbase-1:live/../x",
        "",
        "a b",
    ] {
        assert!(
            server
                .query_one("SELECT lake_make_range($1)", &[&partition])
                .is_err(),
            "refused: {partition:?}"
        );
        assert!(
            server
                .query_one("SELECT lake_make_month($1, $2)", &[&partition, &"2026-10"])
                .is_err(),
            "refused: {partition:?}"
        );
    }
    for month in [
        "2026-13",
        "2026-1",
        "202610",
        "2026-10'; DROP TABLE lake_row; --",
        "1600-01",
        "9999-12",
    ] {
        assert!(
            server
                .query_one("SELECT lake_make_month($1, $2)", &[&dataset, &month])
                .is_err(),
            "refused: {month:?}"
        );
    }
    // A range with no table of lake_row's yet: no month made under it.
    assert!(server
        .query_one(
            "SELECT lake_make_month($1, $2)",
            &[&"coinbase-1:unseen", &"2026-10"]
        )
        .is_err());
    assert_eq!(
        (leaves(&mut server), tables(&mut server)),
        (held, every),
        "nothing made or dropped"
    );
    let present: bool = server
        .query_one(
            "SELECT to_regclass('lake_row') IS NOT NULL AND to_regclass('lake_latest') IS NOT NULL",
            &[],
        )
        .expect("reads")
        .get(0);
    assert!(present);
    // A whole valid call still works, and says so once.
    let made: bool = server
        .query_one("SELECT lake_make_month($1, $2)", &[&dataset, &"2026-12"])
        .expect("a valid month")
        .get(0);
    let again: bool = server
        .query_one("SELECT lake_make_month($1, $2)", &[&dataset, &"2026-12"])
        .expect("a valid month again")
        .get(0);
    assert_eq!((made, again), (true, false));
    // Its bounds computed there: December 2026, UTC, in nanoseconds.
    let bound: String = server
        .query_one(
            "SELECT pg_get_expr(c.relpartbound, c.oid) FROM pg_class c \
             WHERE c.relname LIKE 'lake\\_row\\_%\\_202612' AND c.relkind = 'r' \
             AND c.relnamespace = current_schema()::regnamespace",
            &[],
        )
        .expect("reads")
        .get(0);
    assert_eq!(
        bound,
        "FOR VALUES FROM ('1796083200000000000') TO ('1798761600000000000')"
    );
    // Migration 24: a month's table analysed by its owner, as the serving
    // role asks -- only a month of lake_row's that holds rows and was never
    // analysed, once; every malformed argument refused, nothing named by it.
    for (partition, month) in [
        ("x'); DROP TABLE lake_row; --", "2026-12"),
        ("coinbase-1:live/0000-FFFF", "2026-12"),
        (dataset, "2026-13"),
        (dataset, "2026-12'; ANALYZE; --"),
    ] {
        assert!(
            server
                .query_one("SELECT lake_analyze_month($1, $2)", &[&partition, &month])
                .is_err(),
            "refused: {partition:?} {month:?}"
        );
    }
    let analysed = |server: &mut postgres::Client, month: &str| -> bool {
        server
            .query_one("SELECT lake_analyze_month($1, $2)", &[&dataset, &month])
            .expect("a valid month")
            .get(0)
    };
    assert!(
        !analysed(&mut server, "2026-12"),
        "empty: nothing to analyse"
    );
    assert!(!analysed(&mut server, "2031-01"), "a month not made");
}
