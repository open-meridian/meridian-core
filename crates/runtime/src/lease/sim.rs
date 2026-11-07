//! The lease under a simulated network (step 3b part 3; ruling 1105:
//! turmoil): three members of a component electing a writer through an API
//! server that is a host of its own on a network turmoil runs, in virtual
//! time, with links cut and healed and messages held and let go on a
//! schedule each seed draws differently.
//!
//! Held to the lease's one property, which the epoch backs in the store:
//! at no moment do two members both believe they may write. A member may
//! write from the moment a look answers [`Standing::Held`] until its renew
//! deadline after the renew that answered it, as the member's own loop
//! holds it (step-down before the lease ends); the windows of any two
//! members never overlap, whatever the network did.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::*;

const PORT: u16 = 7000;
const MEMBERS: [&str; 3] = ["lake-a", "lake-b", "lake-c"];

fn to_json(record: &Record) -> Value {
    json!({
        "holder": record.holder,
        "duration_seconds": record.duration_seconds,
        "acquire_time": record.acquire_time,
        "renew_time": record.renew_time,
        "transitions": record.transitions,
        "resource_version": record.resource_version,
    })
}

fn from_json(value: &Value) -> Record {
    Record {
        holder: value["holder"].as_str().unwrap_or_default().into(),
        duration_seconds: value["duration_seconds"].as_u64().unwrap_or(0),
        acquire_time: value["acquire_time"].as_str().unwrap_or_default().into(),
        renew_time: value["renew_time"].as_str().unwrap_or_default().into(),
        transitions: value["transitions"].as_u64().unwrap_or(0),
        resource_version: value["resource_version"]
            .as_str()
            .unwrap_or_default()
            .into(),
    }
}

/// The API server's one Lease, every write conditional on the version read,
/// as Kubernetes keeps it.
fn answer(held: &RefCell<Option<Record>>, asked: &Value) -> Value {
    let mut held = held.borrow_mut();
    match asked["verb"].as_str() {
        Some("get") => json!({ "record": held.as_ref().map(to_json) }),
        Some("create") => {
            if held.is_some() {
                return json!({ "conflict": true });
            }
            let mut record = from_json(&asked["record"]);
            record.resource_version = "1".into();
            *held = Some(record);
            json!({ "version": "1" })
        }
        Some("update") => {
            let record = from_json(&asked["record"]);
            match held.as_ref() {
                Some(current) if current.resource_version == record.resource_version => {
                    let next =
                        (current.resource_version.parse::<u64>().unwrap_or(0) + 1).to_string();
                    let mut record = record;
                    record.resource_version = next.clone();
                    *held = Some(record);
                    json!({ "version": next })
                }
                _ => json!({ "conflict": true }),
            }
        }
        _ => json!({ "error": "no such verb" }),
    }
}

/// The API server reached over turmoil's network, one request a connection,
/// each bounded as [`KubeLeases`] bounds its own by the renew interval.
struct Net {
    within: Duration,
}

impl Net {
    async fn ask(&self, asked: Value) -> Result<Value, String> {
        let exchange = async {
            let stream = turmoil::net::TcpStream::connect(("api", PORT))
                .await
                .map_err(|failed| failed.to_string())?;
            let (read, mut write) = tokio::io::split(stream);
            let mut line = asked.to_string();
            line.push('\n');
            write
                .write_all(line.as_bytes())
                .await
                .map_err(|failed| failed.to_string())?;
            let mut said = String::new();
            BufReader::new(read)
                .read_line(&mut said)
                .await
                .map_err(|failed| failed.to_string())?;
            serde_json::from_str::<Value>(&said).map_err(|failed| failed.to_string())
        };
        tokio::time::timeout(self.within, exchange)
            .await
            .map_err(|_| "the API server did not answer within the renew interval".to_string())?
    }

    fn wrote(said: Value) -> Result<Wrote, String> {
        if said["conflict"].as_bool() == Some(true) {
            return Ok(Wrote::Conflict);
        }
        said["version"]
            .as_str()
            .map(|version| Wrote::Done(version.into()))
            .ok_or_else(|| format!("the API server said {said}"))
    }
}

impl Leases for Net {
    async fn get(&self, _name: &str) -> Result<Option<Record>, String> {
        let said = self.ask(json!({ "verb": "get" })).await?;
        Ok(said["record"]
            .as_object()
            .map(|_| from_json(&said["record"])))
    }

    async fn create(&self, _name: &str, record: &Record) -> Result<Wrote, String> {
        Self::wrote(
            self.ask(json!({ "verb": "create", "record": to_json(record) }))
                .await?,
        )
    }

    async fn update(&self, _name: &str, record: &Record) -> Result<Wrote, String> {
        Self::wrote(
            self.ask(json!({ "verb": "update", "record": to_json(record) }))
                .await?,
        )
    }
}

/// A member's window to write: from when it learned it held the lease to
/// its renew deadline after the renew that told it, in virtual time.
#[derive(Debug, Clone)]
struct Window {
    member: &'static str,
    from: Duration,
    until: Duration,
}

/// A small, seeded generator for the fault schedule: the same seed, the
/// same run.
struct Draw(u64);

impl Draw {
    fn next(&mut self) -> u64 {
        // SplitMix64.
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Virtual time since the simulation began, the same on every host.
fn elapsed() -> Duration {
    turmoil::sim_elapsed().unwrap_or_default()
}

/// One run: the members electing for `seconds` of virtual time while the
/// schedule drawn from `seed` cuts, holds and heals their links to the API
/// server. Every window each member held, and how many times the lease
/// changed hands.
fn election(seed: u64, seconds: u64) -> (Vec<Window>, usize) {
    let windows: Rc<RefCell<Vec<Window>>> = Rc::default();
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(seconds + 30))
        .tick_duration(Duration::from_millis(10))
        .min_message_latency(Duration::from_millis(1))
        .max_message_latency(Duration::from_millis(40))
        .build_with_rng(Box::new(
            <rand::rngs::SmallRng as rand::SeedableRng>::seed_from_u64(seed),
        ));
    let lease: Rc<RefCell<Option<Record>>> = Rc::default();
    let kept = Rc::clone(&lease);
    sim.host("api", move || {
        let held = Rc::clone(&kept);
        async move {
            let listener = turmoil::net::TcpListener::bind(("0.0.0.0", PORT)).await?;
            loop {
                let (stream, _) = listener.accept().await?;
                let held = Rc::clone(&held);
                tokio::task::spawn_local(async move {
                    let (read, mut write) = tokio::io::split(stream);
                    let mut asked = String::new();
                    if BufReader::new(read).read_line(&mut asked).await.is_err() {
                        return;
                    }
                    let Ok(asked) = serde_json::from_str::<Value>(&asked) else {
                        return;
                    };
                    let mut said = answer(&held, &asked).to_string();
                    said.push('\n');
                    let _ = write.write_all(said.as_bytes()).await;
                });
            }
        }
    });
    for member in MEMBERS {
        let windows = Rc::clone(&windows);
        sim.host(member, move || {
            let windows = Rc::clone(&windows);
            async move {
                let settings = Settings {
                    name: "release-lake".into(),
                    holder: member.into(),
                    renew: Duration::from_secs(2),
                    duration: Duration::from_secs(6),
                };
                let deadline = settings.renew_deadline();
                let mut elector = Elector::new(
                    Net {
                        within: settings.renew,
                    },
                    settings,
                );
                loop {
                    let asked_at = elapsed();
                    let now = tokio::time::Instant::now().into_std();
                    let wall = asked_at.as_nanos() as i64;
                    if let Ok(Standing::Held) = elector.look(now, wall).await {
                        windows.borrow_mut().push(Window {
                            member,
                            from: elapsed(),
                            until: asked_at + deadline,
                        });
                    }
                    tokio::time::sleep(elector.settings().look_every()).await;
                }
            }
        });
    }
    // The schedule: every simulated second, perhaps a member's link to the
    // API server cut (a partition), held (messages delayed until let go), or
    // healed; never more than one member cut at a time, so a writer can
    // always be elected and the property is tested, not starved.
    sim.client("schedule", async move {
        let mut draw = Draw(seed);
        let mut cut: Option<&'static str> = None;
        let mut held: Option<&'static str> = None;
        while elapsed() < Duration::from_secs(seconds) {
            tokio::time::sleep(Duration::from_secs(1)).await;
            match draw.below(10) {
                0 | 1 if cut.is_none() => {
                    let member = MEMBERS[draw.below(3) as usize];
                    turmoil::partition(member, "api");
                    cut = Some(member);
                }
                2 if held.is_none() => {
                    let member = MEMBERS[draw.below(3) as usize];
                    turmoil::hold(member, "api");
                    held = Some(member);
                }
                3..=5 => {
                    if let Some(member) = cut.take() {
                        turmoil::repair(member, "api");
                    }
                }
                6 => {
                    if let Some(member) = held.take() {
                        turmoil::release(member, "api");
                    }
                }
                _ => {}
            }
        }
        Ok(())
    });
    sim.run().expect("the simulation runs");
    let changes = lease
        .borrow()
        .as_ref()
        .map_or(0, |r| r.transitions as usize);
    let windows = windows.borrow().clone();
    (windows, changes)
}

/// Each pair of windows held by different members, overlapping.
fn overlaps(windows: &[Window]) -> Vec<(Window, Window)> {
    let mut found = Vec::new();
    for (i, a) in windows.iter().enumerate() {
        for b in &windows[i + 1..] {
            if a.member != b.member && a.from < b.until && b.from < a.until {
                found.push((a.clone(), b.clone()));
            }
        }
    }
    found
}

#[test]
fn under_cut_held_and_healed_links_no_two_members_may_write_at_once() {
    let mut handed = 0;
    for seed in 0..24 {
        let (windows, changes) = election(seed, 240);
        assert!(
            !windows.is_empty(),
            "seed {seed}: no member ever held the lease"
        );
        let both = overlaps(&windows);
        assert!(
            both.is_empty(),
            "seed {seed}: two members could write at once: {:?}",
            &both[..both.len().min(3)]
        );
        handed += changes;
    }
    // The schedule did move the lease about: the property was tested across
    // takeovers, not over one quiet holder.
    assert!(handed > 24, "the lease changed hands only {handed} times");
}

#[test]
fn a_run_is_the_same_run_for_the_same_seed() {
    let (a, ca) = election(7, 60);
    let (b, cb) = election(7, 60);
    assert_eq!(ca, cb);
    let at = |w: &[Window]| {
        w.iter()
            .map(|w| (w.member, w.from, w.until))
            .collect::<Vec<_>>()
    };
    assert_eq!(at(&a), at(&b));
}
