//! Scenario 6: a database timeline change during a shard's takeover (the
//! shard's epoch fence, item 32; reviews c5a H1/H2 and c5a2 N1/N2).
//!
//! A data plugin's shard is held by one pod at a time. A pod that takes it
//! has database 0 number the take -- max(the epoch held + 1, the Lease's
//! transitions) -- and record it (`ShardTookOver`), then every writer
//! group's database raise its fence row to that epoch, keeping the stream's
//! last sequence as it found it (`admit_through`); then the pod stamps every
//! write with the epoch. Each group's writer takes the lake's write-ahead
//! stream from its committed position and checks each write's stamp in the
//! batch's own transaction: lower than its row, refused SHARD_FENCED unless
//! the write is in the stream at or below `admit_through`; higher, the row
//! raised. A batch commits its rows and its position together, locally; a
//! write is answered once its batch commits. A database failover loses the
//! tail its standby never held, and the writer, seeing the new timeline,
//! takes the stream again from the position that survived.
//!
//! Today (core main 8599e1fd, `crates/changes/src/shard_fence.rs:1-60,
//! 207-212, 455-490`): every fence transaction -- the numbering, the
//! record, each raise -- commits on the synchronous standby, and a new
//! timeline seats only rows a database lacks, from database 0's.
//!
//! Variants: c5a (3443012f): fence transactions commit locally, nothing is
//! raised again on a new timeline, and a replay is checked by its stamp
//! alone. c5a2 (94e6bfd5): `admit_through` kept, but the numbering and the
//! record still commit locally, and a new timeline raises every row from
//! database 0's own records with `admit_through` reset to 0.
//!
//! Invariants: no write answered as recorded is lost (H2, N2); no two pods
//! are answered one epoch (N1); and no write a superseded pod sends after a
//! newer take was answered is committed (H1).

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::time::Duration;

use crate::world::{self, now, Db, Draw, Outcome, Tally};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Design {
    Today,
    /// review c5a's code: local fence commits, no re-raise, no admit.
    C5a,
    /// review c5a2's code: local numbering and record; a re-raise from
    /// database 0's records with `admit_through` 0.
    C5a2,
}

const GROUPS: usize = 2;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
struct Fence {
    epoch: u64,
    admit_through: u64,
}

#[derive(Clone, Default)]
struct Store {
    fence: Option<Fence>,
    rows: BTreeSet<u64>,
    position: u64,
    /// Database 0's `ShardTookOver` records: each take's epoch.
    records: Vec<u64>,
}

#[derive(Clone, Copy, Debug)]
struct Write {
    id: u64,
    pod: &'static str,
    stamp: u64,
    group: usize,
    /// When it was sent: the model's own note, for the H1 check.
    sent: Duration,
}

#[derive(Debug, Hash)]
enum Ev {
    Took(&'static str, u64, u64),
    Answered(u64, bool),
    Committed(usize, u64, u64),
    Refused(usize, u64),
    FailedOver(usize, usize),
    Followed(usize),
}

struct World {
    design: Design,
    draw: Draw,
    db: [Db<Store>; GROUPS],
    stream: Vec<Write>,
    /// Each pod's epoch, as its take was answered.
    stamp: BTreeMap<&'static str, u64>,
    /// Each take answered: the pod, its epoch and when.
    takes: Vec<(&'static str, u64, Duration)>,
    answered: BTreeMap<u64, bool>,
    events: Vec<Ev>,
    next_id: u64,
    /// Whether a pod's container may restart and take again.
    retakes: bool,
}

type Shared = Rc<RefCell<World>>;

impl World {
    fn durable(&self) -> bool {
        self.design == Design::Today
    }

    fn last_seq(&self) -> u64 {
        self.stream.len() as u64
    }

    /// Database 0 numbers the take and records it.
    fn number(&mut self, transitions: u64) -> u64 {
        let sync = self.durable();
        let held = self.db[0].read().fence.map_or(0, |f| f.epoch);
        let recorded = match self.design {
            Design::C5a => 0,
            _ => self.db[0].read().records.iter().copied().max().unwrap_or(0),
        };
        let epoch = (held + 1).max(recorded + 1).max(transitions);
        let admit = if self.design == Design::C5a {
            0
        } else {
            self.last_seq()
        };
        self.db[0].commit(sync, move |s| {
            s.fence = Some(Fence {
                epoch,
                admit_through: admit,
            })
        });
        self.db[0].commit(sync, move |s| s.records.push(epoch));
        epoch
    }

    /// A group's raise to the take's epoch; one asked again of an epoch
    /// held keeps the place the first found.
    fn raise(&mut self, group: usize, epoch: u64) {
        let sync = self.durable();
        let admit = if self.design == Design::C5a {
            0
        } else {
            self.last_seq()
        };
        let held = self.db[group].read().fence;
        if held.is_some_and(|f| f.epoch >= epoch) {
            return;
        }
        self.db[group].commit(sync, move |s| {
            s.fence = Some(Fence {
                epoch,
                admit_through: admit,
            })
        });
    }

    /// A writer that saw its database's new timeline.
    fn follow(&mut self, group: usize) {
        self.events.push(Ev::Followed(group));
        match self.design {
            Design::Today => {
                // Seat a row the database lacks, from database 0's.
                if self.db[group].read().fence.is_none() {
                    if let Some(zero) = self.db[0].read().fence {
                        self.db[group].commit(true, move |s| s.fence = Some(zero));
                    }
                }
            }
            Design::C5a => {}
            Design::C5a2 => {
                let highest = self.db[0].read().records.iter().copied().max().unwrap_or(0);
                if self.db[group].read().fence.map_or(0, |f| f.epoch) < highest {
                    self.db[group].commit(false, move |s| {
                        s.fence = Some(Fence {
                            epoch: highest,
                            admit_through: 0,
                        })
                    });
                }
            }
        }
    }

    /// One batch of `group`'s writer: up to eight of its writes past its
    /// position, each checked by the fence, committed with the position.
    fn batch(&mut self, group: usize) {
        let from = self.db[group].read().position as usize;
        let mut fence = self.db[group].read().fence;
        let mut kept = Vec::new();
        let mut seen = from;
        let mut answers = Vec::new();
        for (i, w) in self.stream.iter().enumerate().skip(from) {
            if kept.len() + answers.len() >= 8 {
                break;
            }
            seen = i + 1;
            if w.group != group {
                continue;
            }
            let seq = i as u64 + 1;
            let held = fence.unwrap_or_default();
            if w.stamp < held.epoch && seq > held.admit_through {
                answers.push((w.id, false));
                continue;
            }
            if w.stamp > held.epoch {
                fence = Some(Fence {
                    epoch: w.stamp,
                    admit_through: held.admit_through,
                });
            }
            kept.push(*w);
            answers.push((w.id, true));
        }
        if seen == from {
            return;
        }
        let ids: Vec<u64> = kept.iter().map(|w| w.id).collect();
        let position = seen as u64;
        self.db[group].commit(false, move |s| {
            s.fence = fence;
            s.rows.extend(ids.iter().copied());
            s.position = position;
        });
        for w in &kept {
            self.events.push(Ev::Committed(group, w.id, w.stamp));
        }
        for (id, recorded) in answers {
            if !recorded {
                self.events.push(Ev::Refused(group, id));
            }
            // The plugin hears the first answer only; a replay's goes nowhere.
            if let std::collections::btree_map::Entry::Vacant(at) = self.answered.entry(id) {
                at.insert(recorded);
                self.events.push(Ev::Answered(id, recorded));
            }
        }
    }
}

async fn writer(world: Shared, group: usize) -> turmoil::Result {
    let mut timeline = world.borrow().db[group].timeline;
    loop {
        let now_on = world.borrow().db[group].timeline;
        if now_on != timeline {
            timeline = now_on;
            // Its connections made again, or its member restarted with the
            // database: a while before it takes the stream again.
            let back = world.borrow_mut().draw.ms(0, 1_500);
            tokio::time::sleep(back).await;
            world.borrow_mut().follow(group);
        }
        world.borrow_mut().batch(group);
        let pause = world.borrow_mut().draw.ms(2, 10);
        tokio::time::sleep(pause).await;
    }
}

/// A pod's take: numbered and recorded, raised in every group, answered.
async fn take(world: &Shared, pod: &'static str, transitions: u64) {
    let step = |world: &Shared| world.borrow_mut().draw.ms(1, 30);
    let epoch = world.borrow_mut().number(transitions);
    for group in 1..GROUPS {
        tokio::time::sleep(step(world)).await;
        world.borrow_mut().raise(group, epoch);
    }
    tokio::time::sleep(step(world)).await;
    let mut w = world.borrow_mut();
    w.stamp.insert(pod, epoch);
    w.takes.push((pod, epoch, now()));
    w.events.push(Ev::Took(pod, epoch, transitions));
}

/// A pod writing until `until`, stamping its epoch as last answered.
async fn writes(world: Shared, pod: &'static str, until: Duration) {
    while now() < until {
        {
            let mut w = world.borrow_mut();
            let stamp = w.stamp[pod];
            w.next_id += 1;
            let id = w.next_id;
            let group = w.draw.below(GROUPS as u64) as usize;
            w.stream.push(Write {
                id,
                pod,
                stamp,
                group,
                sent: now(),
            });
        }
        let gap = world.borrow_mut().draw.ms(3, 15);
        tokio::time::sleep(gap).await;
    }
}

fn run(design: Design, seed: u64) -> Outcome {
    run_with(design, seed, true)
}

fn run_with(design: Design, seed: u64, retakes: bool) -> Outcome {
    let world: Shared = Rc::new(RefCell::new(World {
        design,
        draw: Draw::new(seed),
        db: [Db::new(Store::default()), Db::new(Store::default())],
        stream: Vec::new(),
        stamp: BTreeMap::new(),
        takes: Vec::new(),
        answered: BTreeMap::new(),
        events: Vec::new(),
        next_id: 0,
        retakes,
    }));
    let mut sim = world::sim(seed, 30);
    for (group, name) in [(0usize, "w0"), (1, "w1")] {
        let world = world.clone();
        sim.host(name, move || writer(world.clone(), group));
    }
    let pods = world.clone();
    sim.client("pods", async move {
        let w = &pods;
        let ms = |lo, hi| w.borrow_mut().draw.ms(lo, hi);
        // A takes the shard and writes.
        tokio::time::sleep(ms(100, 200)).await;
        take(w, "a", 1).await;
        let b_at = now() + ms(800, 2_000);
        tokio::task::spawn_local(writes(w.clone(), "a", b_at + ms(300, 1_500)));
        // B takes; A, cut from the API, writes on with its old epoch.
        tokio::time::sleep_until(tokio::time::Instant::now() + b_at.saturating_sub(now())).await;
        take(w, "b", 2).await;
        let c_at = now() + ms(1_000, 2_500);
        tokio::task::spawn_local(writes(w.clone(), "b", c_at + ms(300, 1_000)));
        // B's container restarts: a take again at the same transitions.
        if w.borrow_mut().draw.chance(1, 2) && w.borrow().retakes {
            tokio::time::sleep(ms(100, 600)).await;
            take(w, "b", 2).await;
        }
        // A database fails over, its standby some way behind.
        tokio::time::sleep(ms(0, 400)).await;
        let lag = ms(10, 300);
        let which = w.borrow_mut().draw.below(3) as usize;
        for group in 0..GROUPS {
            if which == 2 || which == group {
                let lost = w.borrow_mut().db[group].fail_over(lag);
                w.borrow_mut().events.push(Ev::FailedOver(group, lost));
            }
        }
        // C takes after it; B writes on with its own.
        tokio::time::sleep_until(tokio::time::Instant::now() + c_at.saturating_sub(now())).await;
        take(w, "c", 3).await;
        writes(w.clone(), "c", now() + ms(500, 1_500)).await;
        // The writers take the stream to its end.
        tokio::time::sleep(Duration::from_secs(3)).await;
        Ok(())
    });
    sim.run().expect("the model runs");
    let world = world.borrow();
    judge(&world)
}

fn judge(w: &World) -> Outcome {
    let mut outcome = Outcome::default();
    let rows: Vec<&BTreeSet<u64>> = w.db.iter().map(|db| &db.read().rows).collect();
    let by_id: BTreeMap<u64, Write> = w.stream.iter().map(|x| (x.id, *x)).collect();
    for (id, recorded) in &w.answered {
        let write = by_id[id];
        if *recorded && !rows[write.group].contains(id) {
            outcome.broken.push(format!(
                "acked-kept: write {id} (pod {}, stamp {}) was answered recorded and is not in group {}'s rows",
                write.pod, write.stamp, write.group
            ));
            break;
        }
    }
    let mut epochs: BTreeMap<u64, &'static str> = BTreeMap::new();
    for (pod, epoch, _) in &w.takes {
        if let Some(other) = epochs.insert(*epoch, pod) {
            if other != *pod {
                outcome.broken.push(format!(
                    "one-epoch: pods {other} and {pod} were both answered epoch {epoch}"
                ));
            }
        }
    }
    for (group, rows) in rows.iter().enumerate() {
        let stale = rows.iter().map(|id| by_id[id]).find(|x| {
            w.takes
                .iter()
                .any(|(pod, epoch, at)| *pod != x.pod && *epoch > x.stamp && x.sent > *at)
        });
        if let Some(x) = stale {
            let retook = w.takes.iter().filter(|(pod, _, _)| *pod == "b").count() > 1;
            outcome.broken.push(format!(
                "stale-admitted: group {group} committed write {} of pod {} stamped {}, sent after a newer take was answered{}",
                x.id,
                x.pod,
                x.stamp,
                if retook { " (b took twice)" } else { "" }
            ));
        }
    }
    for ev in &w.events {
        match ev {
            Ev::FailedOver(_, lost) => outcome.count("commits lost to failovers", *lost as u64),
            Ev::Refused(..) => outcome.count("writes refused by the fence", 1),
            _ => {}
        }
    }
    outcome.count(
        "writes answered recorded",
        w.answered.values().filter(|r| **r).count() as u64,
    );
    outcome.digest = Outcome::digest_of(&w.events);
    outcome
}

#[test]
fn fast_tier() {
    let today = Tally::of(world::fast(), |seed| run(Design::Today, seed));
    let c5a = Tally::of(world::fast(), |seed| run(Design::C5a, seed));
    let c5a2 = Tally::of(world::fast(), |seed| run(Design::C5a2, seed));
    eprintln!(
        "{}\n{}\n{}",
        today.say("timeline today"),
        c5a.say("timeline c5a"),
        c5a2.say("timeline c5a2")
    );
    // Today: nothing answered is lost and no epoch is given twice ...
    assert!(
        !today.broke("acked-kept"),
        "{}",
        today.say("timeline today")
    );
    assert!(!today.broke("one-epoch"), "{}", today.say("timeline today"));
    // ... but a later take's place admits a superseded pod's writes sent
    // after an earlier take, "whatever its epoch" (shard_fence.rs:22-30):
    // a finding of this model, to be shown on the code by a crate test.
    assert!(
        today.broke("stale-admitted"),
        "{}",
        today.say("timeline today")
    );
    // The defects the reviews found, found again: H2 and N2.
    assert!(c5a.broke("acked-kept"), "{}", c5a.say("timeline c5a"));
    assert!(c5a2.broke("acked-kept"), "{}", c5a2.say("timeline c5a2"));
}

#[test]
fn today_without_a_same_pod_retake_still_admits_a_stale_write() {
    let tally = Tally::of(world::fast(), |seed| run_with(Design::Today, seed, false));
    eprintln!("{}", tally.say("timeline today, no re-take"));
    assert!(
        !tally.broke("acked-kept"),
        "{}",
        tally.say("timeline today, no re-take")
    );
}

#[test]
fn the_same_seed_is_the_same_run() {
    for design in [Design::Today, Design::C5a, Design::C5a2] {
        assert_eq!(run(design, 5).digest, run(design, 5).digest, "{design:?}");
    }
}

#[test]
#[ignore = "the sweep: SIM_SEEDS=<n> cargo test -p meridian-sim -- --ignored"]
fn sweep() {
    for design in [Design::Today, Design::C5a, Design::C5a2] {
        let tally = Tally::of(world::sweep(), |seed| run(design, seed));
        eprintln!("{}", tally.say(&format!("timeline {design:?}")));
    }
}
