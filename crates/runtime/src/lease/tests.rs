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
