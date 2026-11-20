//! Scenario 4: a writer-group raise and lower over the lake's one
//! write-ahead stream (`ha`), each group keeping its own place in it
//! (resilience spec section 4 and 6.1; review-c5w3 H1; 2ebc4ac9).
//!
//! The stream holds every dataset's writes; a dataset is group
//! `dataset % count`'s. A group's writer takes the stream from its place,
//! commits the rows of its own datasets and its new place together, and
//! each of its pods runs at the count it was started with. The raise is
//! rolling with one pod surged: group 1's pod starts at the new count, and
//! while group 0's pods are replaced its old-count standby may hold group
//! 0's lease for a while. One database, one stream identity.
//!
//! Today (core main 8599e1fd): a group past the first with no place takes
//! from the first write the stream holds and counts nothing before it (spec
//! section 4, item 203); an old-count member starting alone takes every
//! range held apart over (`crates/lake/src/postgres.rs:614-635`) and takes
//! from its own place; lowering the count in `ha` is refused (item 111).
//!
//! Variants:
//! - `FromOne` (before 2ebc4ac9): a never-started group takes from sequence
//!   1, where the stream's first held write is later, and counts a gap.
//! - `Markers` (the held take-back rework, review-c5w3): lowering allowed in
//!   `ha`; a group taking a left group's datasets takes its own place back to
//!   the left group's, once, a marker saying it did; an old-count member
//!   starting alone does the same to every group past its count. A marker is
//!   deleted only where its place no longer exists (H1).
//!
//! Invariants: every write the stream acknowledged is recorded by the end
//! (spec section 4: "nothing acknowledged is ever trimmed"); and no group
//! counts a gap over writes another group recorded or the stream never
//! dropped.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::time::Duration;

use crate::world::{self, Draw, Outcome, Tally};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Design {
    Today,
    FromOne,
    Markers,
}

const DATASETS: u64 = 4;

#[derive(Debug, Hash)]
enum Ev {
    Count(u64),
    Pod(u32, u64, bool),
    TookBack(u32, u64, u64),
    Gap(u32, u64, u64),
    Marked(u32),
    Passed(u32),
}

struct World {
    design: Design,
    draw: Draw,
    /// The stream: each write's dataset, by sequence from 1; the first
    /// `trimmed` aged out before any group took them (none acknowledged).
    stream: Vec<u64>,
    trimmed: u64,
    rows: BTreeSet<u64>,
    places: BTreeMap<u32, u64>,
    markers: BTreeSet<u32>,
    /// Each group's running pod: the count it was started with.
    pods: BTreeMap<u32, u64>,
    gaps: Vec<(u32, u64, u64)>,
    events: Vec<Ev>,
}

type Shared = Rc<RefCell<World>>;

impl World {
    /// A pod of `group` started at `count`, alone in its group.
    fn start(&mut self, group: u32, count: u64) {
        self.pods.insert(group, count);
        self.events.push(Ev::Pod(group, count, true));
        if self.design == Design::Markers {
            // Every group at or past its count is a place left to it: taken
            // back to, once each, a marker saying so.
            let mine = self.places.get(&group).copied().unwrap_or(0);
            let pending: Vec<(u32, u64)> = self
                .places
                .iter()
                .filter(|(g, _)| {
                    u64::from(**g) >= count && **g != group && !self.markers.contains(g)
                })
                .map(|(g, p)| (*g, *p))
                .collect();
            if let Some(back) = pending.iter().map(|(_, p)| *p).min() {
                if back < mine {
                    self.places.insert(group, back);
                    self.events.push(Ev::TookBack(group, mine, back));
                }
                for (g, _) in pending {
                    self.markers.insert(g);
                    self.events.push(Ev::Marked(g));
                    // Its place deleted: the taker reads its writes now.
                    self.places.remove(&g);
                }
            }
        }
    }

    fn stop(&mut self, group: u32) {
        if let Some(count) = self.pods.remove(&group) {
            self.events.push(Ev::Pod(group, count, false));
        }
    }

    /// One batch of `group`'s running pod: up to 16 writes past its place.
    fn batch(&mut self, group: u32) {
        let Some(count) = self.pods.get(&group).copied() else {
            return;
        };
        let first_held = self.trimmed + 1;
        let from = match self.places.get(&group) {
            Some(place) => *place + 1,
            None if group == 0
                || self.design == Design::Today
                || self.design == Design::Markers =>
            {
                first_held
            }
            None => 1,
        };
        if from < first_held {
            // The stream skips: a gap counted from where it took.
            self.gaps.push((group, from, first_held - 1));
            self.events.push(Ev::Gap(group, from, first_held - 1));
        }
        let from = from.max(first_held);
        let last = (from + 15).min(self.stream.len() as u64);
        if from > last {
            return;
        }
        for seq in from..=last {
            let dataset = self.stream[(seq - 1) as usize];
            if dataset % count == u64::from(group) {
                self.rows.insert(seq);
            }
        }
        self.places.insert(group, last);
        if self.design == Design::Markers {
            // `write_ahead_passed`: a marker whose place no longer exists,
            // passed, is deleted -- only while its group is past the count.
            let passed: Vec<u32> = self
                .markers
                .iter()
                .filter(|g| u64::from(**g) >= count && !self.places.contains_key(g))
                .copied()
                .collect();
            for g in passed {
                self.markers.remove(&g);
                self.events.push(Ev::Passed(g));
            }
        }
    }
}

async fn writer(world: Shared, group: u32) -> turmoil::Result {
    loop {
        world.borrow_mut().batch(group);
        let pause = world.borrow_mut().draw.ms(2, 20);
        tokio::time::sleep(pause).await;
    }
}

fn run(design: Design, seed: u64) -> Outcome {
    let mut draw = Draw::new(seed);
    let trimmed = draw.below(40);
    let world: Shared = Rc::new(RefCell::new(World {
        design,
        draw: Draw::new(seed.wrapping_add(7)),
        stream: (0..trimmed).map(|i| i % DATASETS).collect(),
        trimmed,
        rows: BTreeSet::new(),
        places: BTreeMap::new(),
        markers: BTreeSet::new(),
        pods: BTreeMap::new(),
        gaps: Vec::new(),
        events: Vec::new(),
    }));
    let mut sim = world::sim(seed, 30);
    for group in [0u32, 1] {
        let world = world.clone();
        let name = if group == 0 { "w0" } else { "w1" };
        sim.host(name, move || writer(world.clone(), group));
    }
    let plugin = world.clone();
    sim.client("plugin", async move {
        // Writes all along, every one acknowledged by the stream.
        for _ in 0..1_000 {
            let dataset = plugin.borrow_mut().draw.below(DATASETS);
            plugin.borrow_mut().stream.push(dataset);
            let gap = plugin.borrow_mut().draw.ms(1, 8);
            tokio::time::sleep(gap).await;
        }
        Ok(())
    });
    let roll = world.clone();
    sim.client("rollout", async move {
        let w = &roll;
        let ms = |lo, hi| w.borrow_mut().draw.ms(lo, hi);
        w.borrow_mut().start(0, 1);
        tokio::time::sleep(ms(300, 1_000)).await;
        // The raise, rolling: group 1's pod at 2; group 0's old pod
        // replaced, its old-count standby holding the lease a while first.
        w.borrow_mut().events.push(Ev::Count(2));
        w.borrow_mut().start(1, 2);
        tokio::time::sleep(ms(0, 400)).await;
        w.borrow_mut().stop(0);
        if w.borrow_mut().draw.chance(1, 2) {
            tokio::time::sleep(ms(5, 50)).await;
            w.borrow_mut().start(0, 1);
            tokio::time::sleep(ms(50, 800)).await;
            w.borrow_mut().stop(0);
        }
        tokio::time::sleep(ms(5, 50)).await;
        w.borrow_mut().start(0, 2);
        tokio::time::sleep(ms(500, 2_000)).await;
        if w.borrow().design == Design::Markers {
            // The lower, 2 to 1: group 1's pod stopped with its place behind,
            // group 0's restarted at 1.
            w.borrow_mut().events.push(Ev::Count(1));
            w.borrow_mut().stop(1);
            tokio::time::sleep(ms(0, 300)).await;
            w.borrow_mut().stop(0);
            tokio::time::sleep(ms(5, 50)).await;
            w.borrow_mut().start(0, 1);
        }
        // To the stream's end.
        tokio::time::sleep(Duration::from_secs(4)).await;
        Ok(())
    });
    sim.run().expect("the model runs");
    let world = world.borrow();
    judge(&world)
}

fn judge(w: &World) -> Outcome {
    let mut outcome = Outcome::default();
    let acknowledged = (w.trimmed + 1)..=(w.stream.len() as u64);
    let lost: Vec<u64> = acknowledged.filter(|seq| !w.rows.contains(seq)).collect();
    if let Some(first) = lost.first() {
        outcome.broken.push(format!(
            "acked-kept: {} writes the stream acknowledged were never recorded, from sequence {first}",
            lost.len()
        ));
    }
    if let Some((group, from, to)) = w.gaps.first() {
        outcome.broken.push(format!(
            "no-false-gap: group {group} counted a gap {from}..={to} the stream dropped before any group took it"
        ));
    }
    for ev in &w.events {
        match ev {
            Ev::TookBack(..) => outcome.count("take-backs", 1),
            Ev::Marked(..) => outcome.count("markers made", 1),
            Ev::Passed(..) => outcome.count("markers deleted", 1),
            _ => {}
        }
    }
    outcome.digest = Outcome::digest_of(&w.events);
    outcome
}

#[test]
fn fast_tier() {
    let today = Tally::of(world::fast(), |seed| run(Design::Today, seed));
    let one = Tally::of(world::fast(), |seed| run(Design::FromOne, seed));
    let markers = Tally::of(world::fast(), |seed| run(Design::Markers, seed));
    eprintln!(
        "{}\n{}\n{}",
        today.say("groups today"),
        one.say("groups from one"),
        markers.say("groups markers")
    );
    assert!(today.red.is_empty(), "{}", today.say("groups today"));
    assert!(one.broke("no-false-gap"), "{}", one.say("groups from one"));
    assert!(
        markers.broke("acked-kept"),
        "{}",
        markers.say("groups markers")
    );
}

#[test]
fn the_same_seed_is_the_same_run() {
    for design in [Design::Today, Design::FromOne, Design::Markers] {
        assert_eq!(run(design, 3).digest, run(design, 3).digest, "{design:?}");
    }
}

#[test]
#[ignore = "the sweep: SIM_SEEDS=<n> cargo test -p meridian-sim -- --ignored"]
fn sweep() {
    for design in [Design::Today, Design::FromOne, Design::Markers] {
        let tally = Tally::of(world::sweep(), |seed| run(design, seed));
        eprintln!("{}", tally.say(&format!("groups {design:?}")));
    }
}
