//! Each component's own login and its session's bounds, against Postgres
//! (the resilience spec's section 10a, "The database"; ruled 2026-11-11).
//!
//! Run by `make test-store`; fails loudly without a database. A serving
//! process's login and bounds are its whole process's, so each test here
//! runs in a process of its own, as nextest runs every test.

use std::sync::Arc;
use std::time::{Duration, Instant};

use meridian_bus::{Bus, MemoryBackend};

fn base_url() -> String {
    std::env::var("MERIDIAN_TEST_DATABASE_URL").expect(
        "MERIDIAN_TEST_DATABASE_URL is not set. These tests need a real Postgres; \
         run them with `make test-store`.",
    )
}

/// A connection on `url` as a serving component's: through
/// database_config, as every store's pool's is. Off the async runtime's
/// workers, as a handler is.
fn connect(url: &str) -> Result<postgres::Client, String> {
    let config = meridian_changes::database_config(url).map_err(|failed| failed.to_string())?;
    config
        .connect(meridian_changes::tls::database_tls().map_err(|failed| failed.to_string())?)
        .map_err(|failed| meridian_changes::serving::described(&failed))
}

/// A component's query that runs past its statement_timeout is cancelled
/// by the database and answered with that component's own refusal for
/// load, with a retry-after; another component's request is answered all
/// the while.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_long_query_is_cancelled_with_its_components_own_refusal_and_the_others_answer() {
    meridian_changes::serving::serve_as("lake", Some(256 * 1024 * 1024));
    let (timeout, work_mem) = tokio::task::block_in_place(|| {
        let mut session = connect(&base_url()).unwrap();
        let mut said = |name: &str| -> String {
            session
                .query_one(&format!("SHOW {name}"), &[])
                .unwrap()
                .get(0)
        };
        (said("statement_timeout"), said("work_mem"))
    });
    assert_eq!(timeout, "5s");
    assert_eq!(work_mem, "4MB");

    let bus = Arc::new(Bus::single(
        "tests-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    bus.serve("platform.lake.query.slow-read", move |_| {
        let mut conn = connect(&base_url())?;
        conn.query_one("SELECT pg_sleep(8)", &[])
            .map_err(|failed| meridian_changes::serving::described(&failed))?;
        Ok(("meridian.v1.Empty".into(), Vec::new()))
    });
    bus.serve("platform.book.query.quick-read", move |_| {
        let mut conn = connect(&base_url())?;
        conn.query_one("SELECT 1", &[])
            .map_err(|failed| meridian_changes::serving::described(&failed))?;
        Ok(("meridian.v1.Empty".into(), Vec::new()))
    });

    let began = Instant::now();
    let asking = {
        let bus = Arc::clone(&bus);
        tokio::spawn(async move {
            bus.call(
                "platform.lake.query.slow-read",
                "meridian.v1.Empty",
                Vec::new(),
                None,
                Some(Duration::from_secs(15)),
            )
            .await
        })
    };
    // Meanwhile another component's read is answered, at once.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let quick_began = Instant::now();
    bus.call(
        "platform.book.query.quick-read",
        "meridian.v1.Empty",
        Vec::new(),
        None,
        Some(Duration::from_secs(5)),
    )
    .await
    .expect("the other component's read is answered");
    assert!(quick_began.elapsed() < Duration::from_secs(2));

    let refused = match asking.await.unwrap() {
        Err(meridian_bus::BusError::HandlerFailed { detail, .. }) => detail,
        other => panic!("the long query is cancelled with a refusal: {other:?}"),
    };
    let took = began.elapsed();
    assert!(
        took >= Duration::from_secs(5) && took < Duration::from_secs(8),
        "cancelled at its statement_timeout, after {took:?}"
    );
    assert!(
        refused.contains("statement_timeout"),
        "the lake's own refusal for load: {refused}"
    );
    assert_eq!(
        meridian_bus::refusal_retry_after(&refused),
        Some(5),
        "{refused}"
    );
}

/// A serving component given the serving role's URL connects as its own
/// login once it is made -- a member of the serving role, its password, the
/// serving session's statement_timeout on it -- and the preflight names
/// none missing.
#[test]
fn a_component_connects_as_its_own_login_a_member_of_the_serving_role() {
    let mut admin = postgres::Client::connect(&base_url(), postgres::NoTls)
        .expect("could not reach the test database");
    let database: String = admin
        .query_one("SELECT current_database()", &[])
        .unwrap()
        .get(0);
    admin
        .batch_execute(
            "DO $$ BEGIN
               IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'meridian_app') THEN
                 CREATE ROLE meridian_app LOGIN PASSWORD 'serving-test';
               END IF;
             END $$;
             ALTER ROLE meridian_app WITH LOGIN PASSWORD 'serving-test';",
        )
        .unwrap();
    meridian_changes::serving::ensure_logins(
        &mut admin,
        "meridian_app",
        "serving-test",
        &format!("\"{database}\""),
    )
    .expect("the components' logins are made");
    assert!(meridian_changes::serving::missing_logins(&mut admin)
        .unwrap()
        .is_empty());

    meridian_changes::serving::serve_as("bor", Some(512 * 1024 * 1024));
    let url = format!(
        "postgres://meridian_app:serving-test@{}/{database}",
        base_url()
            .rsplit_once('@')
            .map(|(_, rest)| rest.split('/').next().unwrap_or_default().to_string())
            .unwrap()
    );
    let mut conn = connect(&url).unwrap();
    let row = conn
        .query_one(
            "SELECT current_user::text, pg_has_role(current_user, 'meridian_app', 'MEMBER'), \
                    current_setting('statement_timeout')",
            &[],
        )
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "meridian_bor");
    assert!(row.get::<_, bool>(1));
    assert_eq!(row.get::<_, String>(2), "5s");
}
