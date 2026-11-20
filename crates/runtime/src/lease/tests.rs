use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::*;

/// One Lease, kept as the API server keeps it: every write conditional on
/// the version read.
#[derive(Clone, Default)]
struct Memory(Arc<Mutex<Option<Record>>>);

impl Leases for Memory {
    async fn get(&self, _name: &str) -> Result<Option<Record>, String> {
        Ok(self.0.lock().unwrap().clone())
    }

    async fn create(&self, _name: &str, record: &Record) -> Result<Wrote, String> {
        let mut held = self.0.lock().unwrap();
        if held.is_some() {
            return Ok(Wrote::Conflict);
        }
        let mut record = record.clone();
        record.resource_version = "1".into();
        record.uid = "uid-1".into();
        *held = Some(record);
        Ok(Wrote::Done("1".into()))
    }

    async fn update(&self, _name: &str, record: &Record) -> Result<Wrote, String> {
        let mut held = self.0.lock().unwrap();
        let Some(current) = held.as_ref() else {
            return Ok(Wrote::Conflict);
        };
        if current.resource_version != record.resource_version {
            return Ok(Wrote::Conflict);
        }
        let next = (current.resource_version.parse::<u64>().unwrap() + 1).to_string();
        let mut record = record.clone();
        record.resource_version = next.clone();
        record.uid = current.uid.clone();
        *held = Some(record);
        Ok(Wrote::Done(next))
    }
}

fn settings(holder: &str) -> Settings {
    Settings {
        name: "release-lake".into(),
        holder: holder.into(),
        renew: Duration::from_secs(2),
        duration: Duration::from_secs(6),
    }
}

fn run<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

#[test]
fn the_first_member_takes_a_lease_nobody_holds_and_the_second_waits() {
    run(async {
        let api = Memory::default();
        let mut a = Elector::new(api.clone(), settings("lake-a"));
        let mut b = Elector::new(api.clone(), settings("lake-b"));
        let t0 = Instant::now();
        assert_eq!(a.look(t0, 0).await.unwrap(), Standing::Held);
        assert_eq!(
            b.look(t0, 0).await.unwrap(),
            Standing::HeldBy("lake-a".into())
        );
        // Renewing keeps it, and the standby keeps waiting.
        let t1 = t0 + Duration::from_secs(2);
        assert_eq!(a.look(t1, 2_000_000_000).await.unwrap(), Standing::Held);
        assert_eq!(
            b.look(t1, 2_000_000_000).await.unwrap(),
            Standing::HeldBy("lake-a".into())
        );
        assert_eq!(api.0.lock().unwrap().as_ref().unwrap().transitions, 0);
    });
}

#[test]
fn a_lease_unrenewed_for_its_duration_by_the_standbys_own_clock_is_taken() {
    run(async {
        let api = Memory::default();
        let mut a = Elector::new(api.clone(), settings("lake-a"));
        let mut b = Elector::new(api.clone(), settings("lake-b"));
        let t0 = Instant::now();
        a.look(t0, 0).await.unwrap();
        // The standby first sees it at t0 + 1; the holder stops renewing.
        let seen = t0 + Duration::from_secs(1);
        assert!(matches!(
            b.look(seen, 0).await.unwrap(),
            Standing::HeldBy(_)
        ));
        // Not yet: 5.9 s since the standby saw it change.
        let early = seen + Duration::from_millis(5_900);
        assert!(matches!(
            b.look(early, 0).await.unwrap(),
            Standing::HeldBy(_)
        ));
        // Lapsed by the standby's own clock: taken, a transition counted.
        let lapsed = seen + Duration::from_secs(6);
        assert_eq!(b.look(lapsed, 7_000_000_000).await.unwrap(), Standing::Held);
        let record = api.0.lock().unwrap().clone().unwrap();
        assert_eq!(record.holder, "lake-b");
        assert_eq!(record.transitions, 1);
        // The old holder, waking, finds it taken and does not take it back.
        assert_eq!(
            a.look(lapsed + Duration::from_millis(10), 7_100_000_000)
                .await
                .unwrap(),
            Standing::HeldBy("lake-b".into())
        );
    });
}

#[test]
fn a_renewing_holder_is_never_taken_over_whatever_the_wall_clocks_say() {
    run(async {
        let api = Memory::default();
        let mut a = Elector::new(api.clone(), settings("lake-a"));
        let mut b = Elector::new(api.clone(), settings("lake-b"));
        let t0 = Instant::now();
        // The holder's wall clock is an hour behind; the standby never reads it.
        let behind = -3_600_000_000_000;
        for step in 0..10u64 {
            let now = t0 + Duration::from_secs(2 * step);
            assert_eq!(a.look(now, behind).await.unwrap(), Standing::Held);
            assert!(matches!(b.look(now, 0).await.unwrap(), Standing::HeldBy(_)));
        }
    });
}

#[test]
fn two_standbys_taking_a_lapsed_lease_at_once_cannot_both_hold_it() {
    run(async {
        let api = Memory::default();
        let mut a = Elector::new(api.clone(), settings("lake-a"));
        let mut b = Elector::new(api.clone(), settings("lake-b"));
        let mut c = Elector::new(api.clone(), settings("lake-c"));
        let t0 = Instant::now();
        a.look(t0, 0).await.unwrap();
        b.look(t0, 0).await.unwrap();
        c.look(t0, 0).await.unwrap();
        let lapsed = t0 + Duration::from_secs(7);
        // Both read the same version; the API takes the first write only.
        let read = api.get("release-lake").await.unwrap().unwrap();
        let first = b.look(lapsed, 7_000_000_000).await.unwrap();
        // c's write is conditional on what it read before b wrote.
        let mut stale = read.clone();
        stale.holder = "lake-c".into();
        assert_eq!(
            api.update("release-lake", &stale).await.unwrap(),
            Wrote::Conflict
        );
        assert_eq!(first, Standing::Held);
        assert!(
            matches!(c.look(lapsed, 7_000_000_000).await.unwrap(), Standing::HeldBy(h) if h == "lake-b")
        );
    });
}

#[test]
fn a_released_lease_is_taken_at_the_standbys_next_look() {
    run(async {
        let api = Memory::default();
        let mut a = Elector::new(api.clone(), settings("lake-a"));
        let mut b = Elector::new(api.clone(), settings("lake-b"));
        let t0 = Instant::now();
        a.look(t0, 0).await.unwrap();
        assert!(matches!(b.look(t0, 0).await.unwrap(), Standing::HeldBy(_)));
        a.release(1_000_000_000).await;
        assert_eq!(api.0.lock().unwrap().as_ref().unwrap().holder, "");
        // At once: an empty holder is free, lapse or not.
        assert_eq!(
            b.look(t0 + Duration::from_millis(500), 1_500_000_000)
                .await
                .unwrap(),
            Standing::Held
        );
    });
}

/// A store's takeover says why (CI 43920761210): a lease released by its
/// holder is "released", never "lapsed", whoever released it; one taken
/// from a holder that stopped renewing is "lapsed", naming it; one never
/// held is "free".
#[test]
fn a_takeover_says_released_lapsed_or_free_as_it_was() {
    run(async {
        let api = Memory::default();
        let mut a = Elector::new(api.clone(), settings("lake-a"));
        let mut b = Elector::new(api.clone(), settings("lake-b"));
        let t0 = Instant::now();
        assert_eq!(a.look(t0, 0).await.unwrap(), Standing::Held);
        assert_eq!(a.takeover().0, meridian_changes::LEASE_FREE);
        assert!(matches!(b.look(t0, 0).await.unwrap(), Standing::HeldBy(_)));
        a.release(1_000_000_000).await;
        assert_eq!(
            b.look(t0 + Duration::from_millis(500), 1_500_000_000)
                .await
                .unwrap(),
            Standing::Held
        );
        assert_eq!(
            b.takeover(),
            (meridian_changes::LEASE_RELEASED, String::new()),
            "a release said as a lapse"
        );
        // `b` stops renewing; `c` takes it after the lapse.
        let mut c = Elector::new(api.clone(), settings("lake-c"));
        assert!(matches!(
            c.look(t0 + Duration::from_secs(1), 0).await.unwrap(),
            Standing::HeldBy(_)
        ));
        assert_eq!(
            c.look(t0 + Duration::from_secs(8), 8_000_000_000)
                .await
                .unwrap(),
            Standing::Held
        );
        assert_eq!(
            c.takeover(),
            (meridian_changes::LEASE_LAPSED, "lake-b".to_string())
        );
    });
}

/// A member that gave its lease up is stopping: it never takes it back,
/// though a released lease is free to the first member that looks and it
/// may look before its standby does. Its standby takes it.
#[test]
fn a_member_that_released_its_lease_never_takes_it_back() {
    run(async {
        let api = Memory::default();
        let mut a = Elector::new(api.clone(), settings("lake-a"));
        let mut b = Elector::new(api.clone(), settings("lake-b"));
        let t0 = Instant::now();
        assert_eq!(a.look(t0, 0).await.unwrap(), Standing::Held);
        assert!(matches!(b.look(t0, 0).await.unwrap(), Standing::HeldBy(_)));
        assert!(!a.gave_up());
        a.release(1_000_000_000).await;
        assert!(a.gave_up());
        // Its own next look, before its standby's: nothing taken.
        assert_eq!(
            a.look(t0 + Duration::from_secs(2), 2_000_000_000)
                .await
                .unwrap(),
            Standing::HeldBy(String::new())
        );
        assert_eq!(api.0.lock().unwrap().as_ref().unwrap().holder, "");
        assert_eq!(
            b.look(t0 + Duration::from_secs(2), 2_000_000_000)
                .await
                .unwrap(),
            Standing::Held
        );
        // Nor once that one lapses, however long this member lingers.
        assert_eq!(
            a.look(t0 + Duration::from_secs(60), 60_000_000_000)
                .await
                .unwrap(),
            Standing::HeldBy("lake-b".into())
        );
        assert_eq!(api.0.lock().unwrap().as_ref().unwrap().holder, "lake-b");
    });
}

/// The same where the release itself could not be written -- the member
/// had seen no record to release, its last look having failed: it still
/// renews nothing, and the lease lapses to its standby.
#[test]
fn a_member_whose_release_was_not_written_still_renews_nothing() {
    run(async {
        let api = Memory::default();
        let mut a = Elector::new(api.clone(), settings("lake-a"));
        let mut b = Elector::new(api.clone(), settings("lake-b"));
        let t0 = Instant::now();
        assert_eq!(a.look(t0, 0).await.unwrap(), Standing::Held);
        assert!(matches!(b.look(t0, 0).await.unwrap(), Standing::HeldBy(_)));
        a.observed = None;
        a.release(1_000_000_000).await;
        assert_eq!(api.0.lock().unwrap().as_ref().unwrap().holder, "lake-a");
        assert_eq!(
            a.look(t0 + Duration::from_secs(2), 2_000_000_000)
                .await
                .unwrap(),
            Standing::HeldBy("lake-a".into())
        );
        assert_eq!(
            b.look(t0 + Duration::from_secs(7), 7_000_000_000)
                .await
                .unwrap(),
            Standing::Held
        );
    });
}

#[test]
fn the_renew_deadline_falls_before_the_lease_ends() {
    let s = settings("x");
    assert_eq!(s.renew_deadline(), Duration::from_secs(4));
    assert!(s.renew_deadline() < s.duration);
    assert_eq!(s.look_every(), Duration::from_secs(1));
    assert!(s.check().is_ok());
    let mut bad = settings("x");
    bad.duration = Duration::from_secs(2);
    assert!(bad.check().unwrap_err().contains("leases.renewSeconds"));
    // A lease lasts at least three renewals (contract v30, stage C).
    let mut short = settings("x");
    short.duration = Duration::from_secs(5);
    assert!(short
        .check()
        .unwrap_err()
        .contains("at least three times leases.renewSeconds"));
    short.duration = Duration::from_secs(6);
    assert!(short.check().is_ok());
}

/// A standby that could not read the lease while the API server did not
/// answer counts none of that time toward its expiry (contract v30, stage
/// C): answered again, it watches a whole lease duration before it takes
/// it, and the holder, renewing, keeps it.
#[test]
fn a_standby_never_counts_a_stretch_it_could_not_watch_as_the_holders_silence() {
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Flaky {
        inner: Memory,
        down: Arc<AtomicBool>,
    }
    impl Leases for Flaky {
        async fn get(&self, name: &str) -> Result<Option<Record>, String> {
            if self.down.load(Ordering::SeqCst) {
                return Err("the API server did not answer".into());
            }
            self.inner.get(name).await
        }
        async fn create(&self, name: &str, record: &Record) -> Result<Wrote, String> {
            self.inner.create(name, record).await
        }
        async fn update(&self, name: &str, record: &Record) -> Result<Wrote, String> {
            self.inner.update(name, record).await
        }
    }
    run(async {
        let api = Memory::default();
        let down = Arc::new(AtomicBool::new(false));
        let mut writer = Elector::new(api.clone(), settings("writer"));
        let mut standby = Elector::new(
            Flaky {
                inner: api.clone(),
                down: down.clone(),
            },
            settings("standby"),
        );
        let t0 = Instant::now();
        assert_eq!(writer.look(t0, 0).await.unwrap(), Standing::Held);
        assert!(matches!(
            standby.look(t0, 0).await.unwrap(),
            Standing::HeldBy(_)
        ));
        // The API server lost to the standby for 30 s.
        down.store(true, Ordering::SeqCst);
        assert!(standby.look(t0 + Duration::from_secs(15), 0).await.is_err());
        down.store(false, Ordering::SeqCst);
        // Answered again: the record unchanged since the standby last saw it,
        // and still the writer's for a whole lease from now.
        let back = t0 + Duration::from_secs(30);
        assert!(
            matches!(standby.look(back, 0).await.unwrap(), Standing::HeldBy(_)),
            "never taken on time it did not watch"
        );
        assert_eq!(
            writer.look(back + Duration::from_secs(1), 0).await.unwrap(),
            Standing::Held
        );
        assert!(matches!(
            standby
                .look(back + Duration::from_secs(5), 0)
                .await
                .unwrap(),
            Standing::HeldBy(_)
        ));
    });
}

#[test]
fn micro_time_is_rfc3339_with_microseconds() {
    assert_eq!(micro_time(0), "1970-01-01T00:00:00.000000Z");
    // 2026-11-02T12:34:56.789012Z
    assert_eq!(
        micro_time(1_793_622_896_789_012_345),
        "2026-11-02T12:34:56.789012Z"
    );
    assert_eq!(
        micro_time(951_782_400_000_000_000),
        "2000-02-29T00:00:00.000000Z"
    );
}

#[test]
fn a_lease_read_from_the_api_is_its_record() {
    let lease = serde_json::json!({
        "metadata": {"name": "r-lake", "resourceVersion": "812"},
        "spec": {"holderIdentity": "r-lake-7c9f", "leaseDurationSeconds": 6,
                 "acquireTime": "2026-11-02T12:00:00.000000Z",
                 "renewTime": "2026-11-02T12:00:04.000000Z", "leaseTransitions": 3}
    });
    let record = record_of(&lease);
    assert_eq!(record.holder, "r-lake-7c9f");
    assert_eq!(record.duration_seconds, 6);
    assert_eq!(record.transitions, 3);
    assert_eq!(record.resource_version, "812");
}

// ── The client's connections through an outage (CI 43973174092) ─────────

/// A way to the API server whose packets are dropped while it is cut, as
/// the e2e's iptables rules drop them. A connection opened before the cut
/// is lifted stays dead after it, as one whose unacked data waits on TCP's
/// retransmission backoff does; a connection opened after it carries on.
#[derive(Default)]
struct Blackhole {
    cut: std::sync::atomic::AtomicBool,
    /// Connections numbered below this one are dead.
    dead_below: std::sync::atomic::AtomicU64,
    opened: std::sync::atomic::AtomicU64,
}

impl Blackhole {
    fn passes(&self, connection: u64) -> bool {
        use std::sync::atomic::Ordering::SeqCst;
        !self.cut.load(SeqCst) && connection >= self.dead_below.load(SeqCst)
    }

    fn cut(&self) {
        self.cut.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn lift(&self) {
        use std::sync::atomic::Ordering::SeqCst;
        self.dead_below.store(self.opened.load(SeqCst), SeqCst);
        self.cut.store(false, SeqCst);
    }

    /// Bytes from `from` to `to` while they pass; held while they do not.
    async fn carry(
        self: Arc<Self>,
        connection: u64,
        mut from: tokio::net::tcp::OwnedReadHalf,
        mut to: tokio::net::tcp::OwnedWriteHalf,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buffer = vec![0u8; 16 * 1024];
        loop {
            let Ok(read) = from.read(&mut buffer).await else {
                return;
            };
            if read == 0 {
                return;
            }
            while !self.passes(connection) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            if to.write_all(&buffer[..read]).await.is_err() {
                return;
            }
        }
    }

    /// Listen in front of `api`; answers its own address.
    async fn before(self: &Arc<Self>, api: std::net::SocketAddr) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = listener.local_addr().unwrap();
        let hole = Arc::clone(self);
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let connection = hole
                    .opened
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let Ok(server) = tokio::net::TcpStream::connect(api).await else {
                    continue;
                };
                let (client_read, client_write) = client.into_split();
                let (server_read, server_write) = server.into_split();
                tokio::spawn(Arc::clone(&hole).carry(connection, client_read, server_write));
                tokio::spawn(Arc::clone(&hole).carry(connection, server_read, client_write));
            }
        });
        at
    }
}

/// An API server answering every request with one Lease, over HTTP/2 as
/// the cluster's does.
async fn one_lease_api() -> std::net::SocketAddr {
    let lease = serde_json::json!({
        "metadata": { "resourceVersion": "7", "uid": "uid-7" },
        "spec": { "holderIdentity": "a", "leaseDurationSeconds": 6, "leaseTransitions": 1 }
    });
    let router = axum::Router::new().fallback(move || {
        let lease = lease.clone();
        async move { axum::Json(lease) }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    at
}

/// A token file for a test's client, removed when the test ends, passed
/// or not.
struct Token(std::path::PathBuf);

impl Token {
    fn new(test: &str) -> Self {
        let path = std::env::temp_dir().join(format!("lease-token-{test}-{}", std::process::id()));
        std::fs::write(&path, "token").unwrap();
        Self(path)
    }
}

impl Drop for Token {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The client as the sidecar makes it, but over HTTP/2 without TLS: one
/// connection for every request, as ALPN gives it against the API server.
fn leases_through(at: std::net::SocketAddr, token: &Token) -> KubeLeases {
    let timeout = Duration::from_secs(1);
    let make = move || {
        reqwest::Client::builder()
            .http2_prior_knowledge()
            .timeout(timeout)
            .connect_timeout(timeout)
            .build()
            .map_err(|failed| failed.to_string())
    };
    KubeLeases::through(
        format!("http://{at}"),
        "ns",
        token.0.to_str().unwrap(),
        Box::new(make),
    )
    .unwrap()
}

fn opened(hole: &Blackhole) -> u64 {
    hole.opened.load(std::sync::atomic::Ordering::SeqCst)
}

/// CI 43973174092: after a 15 s outage that dropped every packet, a pod's
/// client answers again within one look of the API server being reachable
/// -- never waiting out the dead connection's retransmissions, which left
/// a data standby blind for 11 s. And while it answers, before the outage
/// and after, it keeps its one connection: no dial per request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn after_a_blackhole_a_lease_client_answers_within_one_look_of_the_lift() {
    let token = Token::new("blackhole");
    let hole = Arc::new(Blackhole::default());
    let at = hole.before(one_lease_api().await).await;
    let leases = leases_through(at, &token);
    for _ in 0..3 {
        let read = leases.get("shard-proof-shard-1").await.unwrap().unwrap();
        assert_eq!((read.holder.as_str(), read.uid.as_str()), ("a", "uid-7"));
    }
    assert_eq!(opened(&hole), 1, "a client that answers dials once");
    hole.cut();
    let cut_at = Instant::now();
    while cut_at.elapsed() < Duration::from_secs(15) {
        let began = Instant::now();
        assert!(leases.get("shard-proof-shard-1").await.is_err());
        tokio::time::sleep(Duration::from_secs(1).saturating_sub(began.elapsed())).await;
    }
    hole.lift();
    let (lifted, before) = (Instant::now(), opened(&hole));
    let read = leases.get("shard-proof-shard-1").await;
    assert!(
        read.is_ok() && lifted.elapsed() <= Duration::from_secs(1),
        "{:?} after the lift: {read:?}",
        lifted.elapsed()
    );
    for _ in 0..2 {
        leases.get("shard-proof-shard-1").await.unwrap();
    }
    assert_eq!(
        opened(&hole) - before,
        1,
        "after the lift it dials once, and keeps that connection"
    );
}

/// Review c5a10 L1: a request on a client already replaced fails late --
/// another request on the dead connection timed out first -- and leaves
/// the client made since, which answers, as it is: one dial after the
/// outage, not one per stale request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_failure_on_an_old_client_leaves_the_newer_one_that_answers() {
    let token = Token::new("late");
    let hole = Arc::new(Blackhole::default());
    let at = hole.before(one_lease_api().await).await;
    let leases = Arc::new(leases_through(at, &token));
    leases.get("shard-proof-shard-0").await.unwrap();
    hole.cut();
    // Two reads on the one connection, half a second apart.
    let first = tokio::spawn({
        let leases = Arc::clone(&leases);
        async move { leases.get("shard-proof-shard-0").await }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let second = tokio::spawn({
        let leases = Arc::clone(&leases);
        async move { leases.get("shard-proof-shard-1").await }
    });
    assert!(first.await.unwrap().is_err());
    hole.lift();
    let before = opened(&hole);
    leases.get("shard-proof-shard-0").await.unwrap();
    // The second read, on the old client, fails after the new one answered.
    assert!(second.await.unwrap().is_err());
    leases.get("shard-proof-shard-0").await.unwrap();
    assert_eq!(
        opened(&hole) - before,
        1,
        "a stale request's failure made the client dial again"
    );
}

// ── What a write keeps of the Lease (review c5b M1) ──────────────────────

/// An API server keeping one Lease as Kubernetes does: a PUT replaces the
/// whole object, conditional on its resourceVersion, and what the server
/// keeps itself (`managedFields`) is kept where a write leaves it out.
/// Answers its address and every PUT body it was sent.
async fn a_kept_lease_api(
    lease: serde_json::Value,
) -> (std::net::SocketAddr, Arc<Mutex<Vec<serde_json::Value>>>) {
    let held = Arc::new(Mutex::new(lease));
    let puts = Arc::new(Mutex::new(Vec::new()));
    let (serving, sent) = (Arc::clone(&held), Arc::clone(&puts));
    let router =
        axum::Router::new().fallback(move |method: axum::http::Method, body: axum::body::Bytes| {
            let (held, sent) = (Arc::clone(&serving), Arc::clone(&sent));
            async move {
                use axum::http::StatusCode;
                let mut held = held.lock().unwrap();
                if method != axum::http::Method::PUT {
                    return (StatusCode::OK, axum::Json(held.clone()));
                }
                let mut written: serde_json::Value = serde_json::from_slice(&body).unwrap();
                sent.lock().unwrap().push(written.clone());
                let version = held["metadata"]["resourceVersion"]
                    .as_str()
                    .unwrap()
                    .to_string();
                if written["metadata"]["resourceVersion"].as_str() != Some(version.as_str()) {
                    return (StatusCode::CONFLICT, axum::Json(serde_json::json!({})));
                }
                let next = (version.parse::<u64>().unwrap() + 1).to_string();
                let metadata = &mut written["metadata"];
                metadata["resourceVersion"] = serde_json::json!(next);
                metadata["uid"] = held["metadata"]["uid"].clone();
                if metadata.get("managedFields").is_none() {
                    metadata["managedFields"] = held["metadata"]["managedFields"].clone();
                }
                *held = written;
                (StatusCode::OK, axum::Json(held.clone()))
            }
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    (at, puts)
}

/// Review c5b M1: a take, and a release after it, write the Lease back as
/// it was read but for its spec -- its labels, its annotations (Helm's
/// `keep` among them, so an upgrade that drops the shard keeps its Lease)
/// and its owners all kept -- never a bare name, which a PUT takes as the
/// whole object and so strips them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_take_and_a_release_keep_the_lease_s_labels_annotations_and_owners() {
    let labels = serde_json::json!({
        "meridian.dev/instance": "feed-1",
        "meridian.dev/launched": "true",
        "app.kubernetes.io/managed-by": "Helm",
    });
    let annotations = serde_json::json!({
        "helm.sh/resource-policy": "keep",
        "meta.helm.sh/release-name": "r",
        "meta.helm.sh/release-namespace": "ns",
    });
    let owners = serde_json::json!([{
        "apiVersion": "v1", "kind": "ConfigMap", "name": "owner", "uid": "owner-uid",
    }]);
    let (at, puts) = a_kept_lease_api(serde_json::json!({
        "apiVersion": "coordination.k8s.io/v1",
        "kind": "Lease",
        "metadata": {
            "name": "feed-1-shard-0", "namespace": "ns", "uid": "uid-1", "resourceVersion": "4",
            "creationTimestamp": "2026-11-16T00:00:00Z",
            "labels": labels, "annotations": annotations, "ownerReferences": owners,
            "managedFields": [{"manager": "helm", "operation": "Update"}],
        },
        "spec": {},
    }))
    .await;
    let token = Token::new("kept");
    let settings = Settings {
        name: "feed-1-shard-0".into(),
        holder: "feed-1-a".into(),
        renew: Duration::from_secs(2),
        duration: Duration::from_secs(6),
    };
    let mut elector = Elector::new(leases_through(at, &token), settings).makes_none();
    assert_eq!(
        elector.look(Instant::now(), 1_000_000_000).await,
        Ok(Standing::Held)
    );
    elector.release(2_000_000_000).await;
    let puts = puts.lock().unwrap().clone();
    assert_eq!(puts.len(), 2, "a take and a release: {puts:#?}");
    for (put, what) in puts.iter().zip(["the take", "the release"]) {
        let metadata = &put["metadata"];
        assert_eq!(
            metadata["labels"], labels,
            "{what} kept its labels: {put:#}"
        );
        assert_eq!(
            metadata["annotations"], annotations,
            "{what} kept its annotations: {put:#}"
        );
        assert_eq!(
            metadata["ownerReferences"], owners,
            "{what} kept its owners: {put:#}"
        );
        assert!(
            metadata.get("managedFields").is_none(),
            "{what} left the server's own fields to it: {put:#}"
        );
        assert_eq!(
            metadata["uid"], "uid-1",
            "{what} names the Lease it read: {put:#}"
        );
    }
    assert_eq!(puts[0]["spec"]["holderIdentity"], "feed-1-a");
    assert_eq!(puts[0]["spec"]["leaseTransitions"], 1);
    assert_eq!(puts[1]["spec"]["holderIdentity"], "");
}
