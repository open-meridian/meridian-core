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
