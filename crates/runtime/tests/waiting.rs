//! Components started before what they need, against Postgres.
//!
//! An upgrade starts every component beside the Job that migrates for it and
//! the broker that restarts under it. Upgrading a development deployment on
//! 2026-09-28 met the stores exiting on a schema the Job had not reached yet,
//! and the dashboard, started before its database answered, aborting in a
//! destructor (task kernel/upgrading-a-deployment-in-place, faults 3 to 5).
//! These start the real binaries in those states and watch them wait, serve
//! once they can, and stop when asked without a panic. And the migration
//! itself, which on 2026-09-29 met the database restarting under it: it waits
//! for a database that does not answer yet, and fails at once on one that
//! answers and refuses the migration. Run by `make test-store`; fails loudly
//! without a database.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn base_url() -> String {
    std::env::var("MERIDIAN_TEST_DATABASE_URL").expect(
        "MERIDIAN_TEST_DATABASE_URL is not set. These tests need a real Postgres; \
         run them with `make test-store`.",
    )
}

/// A schema of this test's own.
fn scratch_schema(tag: &str) -> String {
    let seq = COUNTER.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let schema = format!("waiting_{tag}_{nanos}_{seq}");
    postgres::Client::connect(&base_url(), postgres::NoTls)
        .expect("could not reach the test database")
        .batch_execute(&format!("CREATE SCHEMA {schema}"))
        .expect("could not create a schema");
    schema
}

/// The test database's URL, reaching `schema`, at `address` in place of its
/// own when one is given.
fn url_for(schema: &str, address: Option<&str>) -> String {
    let base = base_url();
    let (scheme, rest) = base.split_once("://").expect("a postgres url");
    let (credential, at) = rest.rsplit_once('@').expect("a url with a login");
    let (own, path) = at.split_once('/').unwrap_or((at, ""));
    let separator = if path.contains('?') { "&" } else { "?" };
    format!(
        "{scheme}://{credential}@{}/{path}{separator}options=-c%20search_path%3D{schema}",
        address.unwrap_or(own)
    )
}

/// Where the test database is, as `host:port`.
fn database_address() -> String {
    let config: postgres::Config = base_url().parse().expect("a postgres url");
    let host = match &config.get_hosts()[0] {
        postgres::config::Host::Tcp(host) => host.clone(),
        #[cfg(unix)]
        other => panic!("the test database is not on TCP: {other:?}"),
    };
    let port = config.get_ports().first().copied().unwrap_or(5432);
    format!("{host}:{port}")
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("no port")
        .local_addr()
        .unwrap()
        .port()
}

/// From now on, carry every connection to `port` on to `upstream`. Until
/// this is called nothing listens there, which is a database that does not
/// answer.
fn forward(port: u16, upstream: String) {
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("the forwarder's port");
    std::thread::spawn(move || {
        for client in listener.incoming().flatten() {
            let Ok(server) = TcpStream::connect(&upstream) else {
                continue;
            };
            for (mut from, mut to) in [
                (client.try_clone().unwrap(), server.try_clone().unwrap()),
                (server, client),
            ] {
                std::thread::spawn(move || {
                    let _ = std::io::copy(&mut from, &mut to);
                    let _ = to.shutdown(Shutdown::Write);
                });
            }
        }
    });
}

/// A component, started with only the environment it is given, and what it
/// has said so far.
struct Running {
    child: Child,
    said: Arc<Mutex<String>>,
}

impl Running {
    fn start(binary: &str, environment: &[(&str, String)]) -> Self {
        Self::start_with(binary, &[], environment)
    }

    /// Started as `binary args...`: `migrate` is the Job's command.
    fn start_with(binary: &str, args: &[&str], environment: &[(&str, String)]) -> Self {
        let mut child = Command::new(binary)
            .args(args)
            .env_clear()
            .env("RUST_LOG", "info")
            .envs(environment.iter().map(|(name, value)| (*name, value)))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("could not start the component");
        let said = Arc::new(Mutex::new(String::new()));
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        for mut stream in [
            Box::new(stdout) as Box<dyn Read + Send>,
            Box::new(stderr) as Box<dyn Read + Send>,
        ] {
            let said = Arc::clone(&said);
            std::thread::spawn(move || {
                let mut chunk = [0u8; 4096];
                while let Ok(read) = stream.read(&mut chunk) {
                    if read == 0 {
                        break;
                    }
                    said.lock()
                        .unwrap()
                        .push_str(&String::from_utf8_lossy(&chunk[..read]));
                }
            });
        }
        Self { child, said }
    }

    fn said(&self) -> String {
        self.said.lock().unwrap().clone()
    }

    /// Until it has said `line`, or fail saying what it said instead.
    fn until_said(&mut self, line: &str, within: Duration) {
        let started = Instant::now();
        while started.elapsed() < within {
            if self.said().contains(line) {
                return;
            }
            if let Some(exited) = self.child.try_wait().unwrap() {
                panic!(
                    "it exited ({exited}) before saying {line:?}:\n{}",
                    self.said()
                );
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!("it never said {line:?}:\n{}", self.said());
    }

    fn running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    /// How it exited, which it must within `within`.
    fn exited(&mut self, within: Duration) -> std::process::ExitStatus {
        let started = Instant::now();
        let exited = loop {
            if let Some(exited) = self.child.try_wait().unwrap() {
                break exited;
            }
            assert!(
                started.elapsed() < within,
                "it was still running after {within:?}:\n{}",
                self.said()
            );
            std::thread::sleep(Duration::from_millis(200));
        };
        // The readers may still be draining the last lines.
        std::thread::sleep(Duration::from_millis(300));
        exited
    }

    /// Asked to stop, as Kubernetes asks: it exits cleanly, not by a panic
    /// or an abort. A store dropped inside the runtime aborted every
    /// dashboard holding one, on every shutdown.
    fn stopped(mut self) {
        let pid = self.child.id() as libc::pid_t;
        // SAFETY: a signal to a child this test started and still holds.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        let started = Instant::now();
        let exited = loop {
            if let Some(exited) = self.child.try_wait().unwrap() {
                break exited;
            }
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "it did not stop:\n{}",
                self.said()
            );
            std::thread::sleep(Duration::from_millis(100));
        };
        // The readers may still be draining the last lines.
        std::thread::sleep(Duration::from_millis(300));
        let said = self.said();
        assert!(!said.contains("panicked"), "it panicked:\n{said}");
        assert!(exited.success(), "it exited {exited}:\n{said}");
    }
}

/// The first line of an answer at `port`, if anything answers there.
fn answer(port: u16, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut answered = String::new();
    stream.read_to_string(&mut answered).ok()?;
    answered.lines().next().map(str::to_string)
}

#[test]
fn a_dashboard_started_before_its_database_waits_for_it_and_serves() {
    // The dashboard's tables made where it will find them, and the
    // dashboard pointed at a port where nothing answers yet.
    let schema = scratch_schema("dashboard");
    meridian_dashboard::database::Database::connect(&url_for(&schema, None), 1)
        .and_then(|database| database.migrate(&meridian_clock::SystemClock))
        .expect("could not make the dashboard's tables");
    let database = free_port();
    let through = url_for(&schema, Some(&format!("127.0.0.1:{database}")));
    let listen = free_port();

    let mut dashboard = Running::start(
        env!("CARGO_BIN_EXE_meridian-dashboard"),
        &[
            ("MERIDIAN_LOCAL_ACCOUNTS", "on".into()),
            ("MERIDIAN_LOCAL_ACCOUNTS_DATABASE_URL", through),
            ("MERIDIAN_DASHBOARD_LISTEN", format!("127.0.0.1:{listen}")),
        ],
    );

    dashboard.until_said(
        "waiting for the dashboard's database",
        Duration::from_secs(30),
    );
    // Long enough to have looked again, and still waiting, not serving.
    std::thread::sleep(Duration::from_secs(5));
    assert!(
        dashboard.running(),
        "it exited while waiting:\n{}",
        dashboard.said()
    );
    assert!(
        answer(listen, "/healthz").is_none(),
        "it served before it had its database:\n{}",
        dashboard.said()
    );

    forward(database, database_address());

    dashboard.until_said("the dashboard's database is there", Duration::from_secs(30));
    dashboard.until_said("the dashboard is listening", Duration::from_secs(30));
    let first = answer(listen, "/healthz");
    assert!(
        first
            .as_deref()
            .is_some_and(|line| line.starts_with("HTTP/1.1 ")),
        "it does not answer: {first:?}\n{}",
        dashboard.said()
    );
    dashboard.stopped();
}

#[test]
fn a_store_started_before_its_migration_waits_for_it_and_is_ready_after() {
    let url = url_for(&scratch_schema("street"), None);
    let ready = std::env::temp_dir().join(format!(
        "meridian-street-ready-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    // Left by an earlier container of the same pod, which this one must not
    // inherit: it is not ready until it serves.
    std::fs::write(&ready, b"serving\n").unwrap();

    let mut street = Running::start(
        env!("CARGO_BIN_EXE_meridian-street"),
        &[
            ("MERIDIAN_STREET_DATABASE_URL", url.clone()),
            ("MERIDIAN_READY_FILE", ready.display().to_string()),
        ],
    );

    street.until_said(
        "waiting for the street store's database",
        Duration::from_secs(30),
    );
    std::thread::sleep(Duration::from_secs(5));
    assert!(
        street.running(),
        "it exited on a schema the migration had not reached:\n{}",
        street.said()
    );
    assert!(
        !ready.exists(),
        "it said it was ready before it could serve:\n{}",
        street.said()
    );

    // The migration Job, arriving.
    meridian_street::PostgresStore::connect(&url, 1)
        .and_then(|store| store.migrate(&meridian_clock::SystemClock))
        .expect("could not migrate");

    street.until_said("the street store is serving", Duration::from_secs(30));
    assert!(
        ready.exists(),
        "it serves and its readiness probe would not see it:\n{}",
        street.said()
    );
    street.stopped();
    assert!(!ready.exists(), "it stopped and still says it is ready");
}

#[test]
fn a_store_whose_schema_is_newer_than_it_refuses_rather_than_waits() {
    // Waiting never fixes a database a newer release migrated, so this one
    // exits, saying so, where the one above waits.
    let url = url_for(&scratch_schema("ahead"), None);
    meridian_street::PostgresStore::connect(&url, 1)
        .and_then(|store| store.migrate(&meridian_clock::SystemClock))
        .expect("could not migrate");
    postgres::Client::connect(&url, postgres::NoTls)
        .unwrap()
        .execute(
            "INSERT INTO schema_migration (version, name, applied_at_ns) VALUES (999, 'later', 1)",
            &[],
        )
        .expect("could not pretend to be ahead");

    let mut street = Running::start(
        env!("CARGO_BIN_EXE_meridian-street"),
        &[("MERIDIAN_STREET_DATABASE_URL", url)],
    );
    let started = Instant::now();
    let exited = loop {
        if let Some(exited) = street.child.try_wait().unwrap() {
            break exited;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "it waited on a schema newer than it:\n{}",
            street.said()
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    std::thread::sleep(Duration::from_millis(300));
    let said = street.said();
    assert!(!exited.success(), "it started anyway:\n{said}");
    assert!(
        said.contains("newer than this binary understands"),
        "it did not say why:\n{said}"
    );
    assert!(!said.contains("panicked"), "it panicked:\n{said}");
}

#[test]
fn a_migration_started_before_its_database_waits_for_it_and_applies() {
    // An upgrade that restarts the database starts the migration Job beside
    // it. The Job runs with no retries, so a migration that made one attempt
    // while the database was not there failed the release, and every
    // component waited for a schema that never came (CI run 36631603926).
    let schema = scratch_schema("migrate");
    let database = free_port();
    let through = url_for(&schema, Some(&format!("127.0.0.1:{database}")));

    let mut migration = Running::start_with(
        env!("CARGO_BIN_EXE_meridian-conductor"),
        &["migrate"],
        &[("MERIDIAN_CONFIG_DATABASE_URL", through)],
    );

    migration.until_said(
        "waiting for the configuration store's database",
        Duration::from_secs(30),
    );
    std::thread::sleep(Duration::from_secs(5));
    assert!(
        migration.running(),
        "it gave up on a database that was not there yet:\n{}",
        migration.said()
    );

    forward(database, database_address());

    let exited = migration.exited(Duration::from_secs(60));
    let said = migration.said();
    assert!(exited.success(), "it exited {exited}:\n{said}");
    assert!(
        said.contains("the configuration store's schema is applied"),
        "it did not say it migrated:\n{said}"
    );
    meridian_config::PostgresStore::connect(&url_for(&schema, None), 1)
        .and_then(|store| store.verify())
        .expect("it said it migrated and the schema is not there");
}

#[test]
fn a_migration_that_fails_once_connected_fails_at_once() {
    // Only the connection is waited for. A migration that fails against a
    // database that answers stops the release, which is why the Job does not
    // retry, so it must not wait here either.
    let url = url_for(&scratch_schema("broken"), None);
    postgres::Client::connect(&url, postgres::NoTls)
        .unwrap()
        .batch_execute("CREATE VIEW schema_migration AS SELECT 1 AS not_a_version")
        .expect("could not break the schema");

    let mut migration = Running::start_with(
        env!("CARGO_BIN_EXE_meridian-street"),
        &["migrate"],
        &[("MERIDIAN_STREET_DATABASE_URL", url)],
    );
    let exited = migration.exited(Duration::from_secs(20));
    let said = migration.said();
    assert!(
        !exited.success(),
        "it said a broken migration applied:\n{said}"
    );
    assert!(
        said.contains("the street store's schema could not be applied"),
        "it did not say why:\n{said}"
    );
    assert!(
        !said.contains("waiting for"),
        "it waited on a failed migration:\n{said}"
    );
    assert!(!said.contains("panicked"), "it panicked:\n{said}");
}
