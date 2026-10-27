//! The upgrade preflight against Postgres (H1, ruling 586): a deployment
//! whose every store is at this release's schema is ready to upgrade to it;
//! one whose store is newer than the release is blocked, a downgrade; and
//! nothing the check does writes; a database that answers only after the
//! hook has started is waited for, and one that never answers blocks. Run by
//! `make test-store`; fails loudly without a database.

use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use meridian_clock::SystemClock;
use meridian_runtime::preflight::{overall, run, Inputs, Verdict};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn base_url() -> String {
    std::env::var("MERIDIAN_TEST_DATABASE_URL").expect(
        "MERIDIAN_TEST_DATABASE_URL is not set. These tests need a real Postgres; \
         run them with `make test-store`.",
    )
}

/// A schema of this test's own, every store migrated into it, and the URL
/// that reaches it.
fn migrated(tag: &str) -> String {
    let seq = COUNTER.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let schema = format!("preflight_{tag}_{nanos}_{seq}");
    postgres::Client::connect(&base_url(), postgres::NoTls)
        .expect("could not reach the test database")
        .batch_execute(&format!("CREATE SCHEMA {schema}"))
        .expect("could not create a schema");
    let base = base_url();
    let separator = if base.contains('?') { "&" } else { "?" };
    let url = format!("{base}{separator}options=-c%20search_path%3D{schema}");
    meridian_config::PostgresStore::connect(&url, 1)
        .unwrap()
        .migrate(&SystemClock)
        .unwrap();
    meridian_street::PostgresStore::connect(&url, 1)
        .unwrap()
        .migrate(&SystemClock)
        .unwrap();
    meridian_bor::PostgresStore::connect(&url, 1)
        .unwrap()
        .migrate(&SystemClock)
        .unwrap();
    meridian_lake::PostgresStore::connect(&url, 1)
        .unwrap()
        .migrate(&SystemClock)
        .unwrap();
    meridian_dashboard::database::Database::connect(&url, 1)
        .unwrap()
        .migrate(&SystemClock)
        .unwrap();
    url
}

fn inputs(url: &str) -> Inputs {
    Inputs {
        stores: vec![
            ("configuration store", url.to_string()),
            ("street store", url.to_string()),
            ("book of record", url.to_string()),
            ("lake", url.to_string()),
            ("dashboard", url.to_string()),
        ],
        volume: Some(64 * 1024 * 1024 * 1024),
        memory_limit: None,
        max_payload: 1024 * 1024,
        backups: None,
        backup_configured: false,
        now_ns: 1_792_800_000_000_000_000,
        database_waits_at_most: Duration::from_secs(30),
    }
}

#[test]
fn a_deployment_at_this_releases_schema_is_ready_to_upgrade() {
    let url = migrated("ready");
    let findings = run(&inputs(&url));
    for f in &findings {
        eprintln!("{} {} {}", f.verdict.word(), f.check, f.why);
    }
    let schemas: Vec<_> = findings.iter().filter(|f| f.check == "schema").collect();
    assert_eq!(schemas.len(), 5, "{findings:?}");
    assert!(
        schemas.iter().all(|f| f.verdict == Verdict::Ready),
        "{schemas:?}"
    );
    let answers = findings.iter().find(|f| f.check == "bus_answers").unwrap();
    assert_eq!(answers.verdict, Verdict::Ready, "{answers:?}");
    let additive = findings
        .iter()
        .find(|f| f.check == "migrations_additive")
        .unwrap();
    assert_eq!(additive.why, "no migration pending");
    // No backup configured is said, as a warning: the whole is a warning,
    // never blocked.
    assert_eq!(overall(&findings), Verdict::Warning);
    // It wrote nothing: each history is as the migrations left it.
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let lake: i64 = client
        .query_one("SELECT count(*) FROM lake_schema_migration", &[])
        .unwrap()
        .get(0);
    assert_eq!(lake as usize, meridian_lake::migrations::MIGRATIONS.len());
}

#[test]
fn a_store_newer_than_the_release_blocks_the_upgrade() {
    let url = migrated("newer");
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    client
        .execute(
            "INSERT INTO lake_schema_migration (version, name, applied_at_ns) VALUES ($1, 'from_a_later_release', 1)",
            &[&(meridian_lake::migrations::latest() + 1)],
        )
        .unwrap();
    let findings = run(&inputs(&url));
    let lake = findings
        .iter()
        .find(|f| f.check == "schema" && f.why.starts_with("lake:"))
        .unwrap();
    assert_eq!(lake.verdict, Verdict::Blocked, "{lake:?}");
    assert!(lake.why.contains("downgrade"), "{lake:?}");
    assert_eq!(overall(&findings), Verdict::Blocked);
}

#[test]
fn a_database_that_does_not_answer_blocks_rather_than_passes() {
    let mut asked = inputs("postgres://nobody:nothing@127.0.0.1:1/none");
    asked.stores.truncate(1);
    asked.database_waits_at_most = Duration::from_secs(3);
    let findings = run(&asked);
    let database = findings.iter().find(|f| f.check == "database").unwrap();
    assert_eq!(database.verdict, Verdict::Blocked, "{database:?}");
    assert!(database.why.contains("does not answer"), "{database:?}");
    assert!(
        database.why.to_lowercase().contains("refused"),
        "the cause is said, not only the kind: {database:?}"
    );
}

/// The test database, reached through a port that refuses every connection
/// until `after` and then forwards each to the database: what the hook's new
/// pod met on CI's k3d cluster (CI run 40423226625), a database running and
/// the pod's first connections refused until the network policy admitted it.
fn answering_late(url: &str, after: Duration) -> String {
    let (head, rest) = url.split_once('@').expect("a URL with credentials");
    let slash = rest.find('/').unwrap_or(rest.len());
    let target = &rest[..slash];
    let target = if target.contains(':') {
        target.to_string()
    } else {
        format!("{target}:5432")
    };
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    std::thread::spawn(move || {
        std::thread::sleep(after);
        let listener = TcpListener::bind(("127.0.0.1", port)).expect("the late port is taken");
        for client in listener.incoming().flatten() {
            let server = TcpStream::connect(&target).expect("the test database");
            let (mut c_in, mut s_out) = (client.try_clone().unwrap(), server.try_clone().unwrap());
            let (mut s_in, mut c_out) = (server, client);
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut c_in, &mut s_out);
                let _ = s_out.shutdown(std::net::Shutdown::Write);
            });
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut s_in, &mut c_out);
                let _ = c_out.shutdown(std::net::Shutdown::Write);
            });
        }
    });
    format!("{head}@127.0.0.1:{port}{}", &rest[slash..])
}

#[test]
fn a_database_that_answers_after_the_hook_starts_is_waited_for() {
    let url = answering_late(&migrated("late"), Duration::from_secs(3));
    let findings = run(&inputs(&url));
    for f in &findings {
        eprintln!("{} {} {}", f.verdict.word(), f.check, f.why);
    }
    assert!(
        findings.iter().all(|f| f.check != "database"),
        "every store answered, once it could: {findings:?}"
    );
    let schemas: Vec<_> = findings.iter().filter(|f| f.check == "schema").collect();
    assert_eq!(schemas.len(), 5, "{findings:?}");
    assert_eq!(overall(&findings), Verdict::Warning, "{findings:?}");
}
