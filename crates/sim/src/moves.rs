//! Scenario 5: a partition's move between two databases, with a crash at
//! each step (resilience spec 6.8; review-moves C3 and A1).
//!
//! Partition P is group 1's, in group 1's database (db1), and moves to group
//! 0, whose home is db0. Each database keeps P's head: the group it names,
//! the epoch it is open under or the pointer epoch under which nothing
//! commits, and its sequence. A writer commits an act to P in its own
//! database only where that database's head names its group, open. A
//! member routes by reading both heads, the furthest open head winning and a
//! tie going to the first database (`read_owners_from`), and keeps that view
//! until a writer answers "not mine" or does not answer.
//!
//! Today (core main 8599e1fd, `crates/lake/src/postgres/moves.rs:302-321`,
//! the book's the same shape): warm; hold the source; the target's head made
//! a pointer to the target; the source lets go, its head a pointer to the
//! target, closed at N; then the target's tail copied and its head opened at
//! N under its group's epoch. A failure between leaves the source's pointer,
//! the durable intent group 0's sweep finishes from; a target pointer whose
//! source never let go is undone.
//!
//! The C3 variant (review-moves, c30d before the fix): the target's head is
//! opened first and committed, then the source lets go; a bound that expires
//! between returns with no compensation, and nothing reconciles at start.
//!
//! Invariants: at most one owner (no sequence committed in both databases
//! as different acts); no acknowledged act missing from the owner's
//! database at the end; and the move ends completed or undone (exactly one
//! open head) once its crashes are over.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

use crate::world::{self, now, Db, Draw, Outcome, Tally};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Design {
    Today,
    /// review-moves C3: the target named owner before the source lets go.
    TargetFirst,
}

const POINTER: i64 = -1;
const HOLD: Duration = Duration::from_secs(5);
const ACTS: u64 = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Head {
    group: u32,
    epoch: i64,
    seq: u64,
}

impl Head {
    fn open(&self) -> bool {
        self.epoch != POINTER
    }
}

/// One database's part of P: its head and its entries by sequence.
#[derive(Clone, Default)]
struct Store {
    head: Option<Head>,
    entries: BTreeMap<u64, u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Step {
    Warm,
    Hold,
    First,
    Second,
    Done,
}

const STEPS: [Step; 5] = [
    Step::Warm,
    Step::Hold,
    Step::First,
    Step::Second,
    Step::Done,
];
const VICTIMS: [&str; 4] = ["w0", "w1", "db0", "db1"];

#[derive(Debug, Hash)]
enum Ev {
    Step(Step),
    Committed(u32, u64, u64),
    Acked(u64, u32),
    Crashed(&'static str),
    FailedOver(u32, usize),
    Swept(&'static str),
    TimedOut,
}

type Answer = Rc<RefCell<Option<bool>>>;
type Inbox = Rc<RefCell<Vec<(u64, Answer)>>>;

struct World {
    design: Design,
    draw: Draw,
    db: [Db<Store>; 2],
    epoch: [i64; 2],
    /// The source's hold, in its member's memory: gone with it.
    hold_until: Option<Duration>,
    step: Option<Step>,
    up: BTreeMap<&'static str, bool>,
    acked: BTreeMap<u64, u32>,
    events: Vec<Ev>,
}

type Shared = Rc<RefCell<World>>;

enum Tried {
    Committed,
    NotMine,
    Held,
}

impl World {
    /// A writer's batch of one act, in its own database.
    fn try_batch(&mut self, group: u32, id: u64) -> Tried {
        if group == 1 && self.hold_until.is_some_and(|until| now() < until) {
            return Tried::Held;
        }
        let epoch = self.epoch[group as usize];
        let db = &mut self.db[group as usize];
        match db.read().head {
            Some(head) if head.group == group && head.epoch == epoch => {
                let seq = head.seq + 1;
                db.commit(true, move |s| {
                    s.head = Some(Head { seq, ..head });
                    s.entries.insert(seq, id);
                });
                self.events.push(Ev::Committed(group, seq, id));
                Tried::Committed
            }
            _ => Tried::NotMine,
        }
    }

    /// `read_owners_from`: the furthest open head, a tie to db0.
    fn owner(&self) -> Option<u32> {
        let heads = [self.db[0].read().head, self.db[1].read().head];
        heads
            .iter()
            .flatten()
            .filter(|h| h.open())
            .fold(None::<Head>, |best, h| match best {
                Some(b) if b.seq >= h.seq => Some(b),
                _ => Some(*h),
            })
            .map(|h| h.group)
    }

    fn at(&mut self, step: Step) {
        self.step = Some(step);
        self.events.push(Ev::Step(step));
    }

    /// The source lets go: its head a pointer to the target, closed at N.
    fn let_go(&mut self) -> u64 {
        let n = self.db[1].read().head.map_or(0, |h| h.seq);
        self.db[1].commit(true, move |s| {
            s.head = Some(Head {
                group: 0,
                epoch: POINTER,
                seq: n,
            })
        });
        n
    }

    /// The target's tail copied up to `n` and its head opened at `n` under
    /// group 0's epoch.
    fn open_target(&mut self, n: u64) {
        let tail: Vec<(u64, u64)> = self.db[1]
            .read()
            .entries
            .range(..=n)
            .map(|(s, i)| (*s, *i))
            .collect();
        let epoch = self.epoch[0];
        self.db[0].commit(true, move |s| {
            for (seq, id) in &tail {
                s.entries.insert(*seq, *id);
            }
            s.head = Some(Head {
                group: 0,
                epoch,
                seq: n,
            });
        });
    }

    /// Group 0's sweep, at its member's start and every second: a source
    /// pointer whose target is not open is finished (today's design only).
    /// A target pointer whose source never let go is left: nothing commits
    /// under it, and the source still owns.
    fn sweep(&mut self) {
        if self.design != Design::Today {
            return;
        }
        let (target, source) = (self.db[0].read().head, self.db[1].read().head);
        if let Some(s) = source {
            if !s.open() && s.group == 0 && !target.is_some_and(|t| t.open()) {
                self.open_target(s.seq);
                self.events.push(Ev::Swept("finished"));
            }
        }
    }
}

/// A writer: its inbox of acts, each answered committed or not mine, or
/// kept to wait under the hold.
async fn writer(world: Shared, group: u32, inbox: Inbox) -> turmoil::Result {
    if group == 0 {
        world.borrow_mut().sweep();
    }
    let mut swept = now();
    loop {
        let asked: Vec<_> = inbox.borrow_mut().drain(..).collect();
        for (id, answer) in asked {
            let tried = world.borrow_mut().try_batch(group, id);
            match tried {
                Tried::Committed => *answer.borrow_mut() = Some(true),
                Tried::NotMine => *answer.borrow_mut() = Some(false),
                Tried::Held => inbox.borrow_mut().push((id, answer)),
            }
        }
        if group == 0 && now() >= swept + Duration::from_secs(1) {
            swept = now();
            world.borrow_mut().sweep();
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// The move, run by group 0's member, and lost with it: a crash leaves the
/// step it reached.
async fn mover(world: Shared) {
    let pause = |world: &Shared| world.borrow_mut().draw.ms(5, 40);
    let design = world.borrow().design;
    world.borrow_mut().at(Step::Warm);
    let warmed = world.borrow().db[1].read().head.map_or(0, |h| h.seq);
    let rows: Vec<(u64, u64)> = world.borrow().db[1]
        .read()
        .entries
        .range(..=warmed)
        .map(|(s, i)| (*s, *i))
        .collect();
    world.borrow_mut().db[0].commit(true, move |s| {
        for (seq, id) in &rows {
            s.entries.insert(*seq, *id);
        }
    });
    tokio::time::sleep(pause(&world)).await;
    world.borrow_mut().at(Step::Hold);
    world.borrow_mut().hold_until = Some(now() + HOLD);
    tokio::time::sleep(pause(&world)).await;
    world.borrow_mut().at(Step::First);
    match design {
        Design::Today => {
            world.borrow_mut().db[0].commit(true, |s| {
                s.head = Some(Head {
                    group: 0,
                    epoch: POINTER,
                    seq: 0,
                })
            });
            tokio::time::sleep(pause(&world)).await;
            world.borrow_mut().at(Step::Second);
            let n = world.borrow_mut().let_go();
            tokio::time::sleep(pause(&world)).await;
            world.borrow_mut().open_target(n);
        }
        Design::TargetFirst => {
            let n = world.borrow().db[1].read().head.map_or(0, |h| h.seq);
            world.borrow_mut().open_target(n);
            tokio::time::sleep(pause(&world)).await;
            world.borrow_mut().at(Step::Second);
            // The bound expired after the target was named: returned with
            // no compensation, one time in four.
            if world.borrow_mut().draw.chance(1, 4) {
                world.borrow_mut().events.push(Ev::TimedOut);
            } else {
                world.borrow_mut().let_go();
            }
        }
    }
    world.borrow_mut().hold_until = None;
    world.borrow_mut().at(Step::Done);
}

/// A member's view of P's owner, as it last read the heads: kept until a
/// writer answers "not mine" or does not answer.
type View = Rc<RefCell<Option<u32>>>;

/// A caller's act, routed by its member's view, asked again where not
/// answered or not its writer's.
async fn act(world: Shared, inboxes: [Inbox; 2], id: u64, view: View) {
    let given_up = now() + Duration::from_secs(15);
    while now() < given_up {
        let read = world.borrow().owner();
        let seen = *view.borrow();
        let Some(group) = seen.or(read) else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        *view.borrow_mut() = Some(group);
        let latency = world.borrow_mut().draw.ms(1, 3);
        tokio::time::sleep(latency).await;
        let answer: Answer = Rc::new(RefCell::new(None));
        let name = if group == 0 { "w0" } else { "w1" };
        if world.borrow().up[name] {
            inboxes[group as usize]
                .borrow_mut()
                .push((id, answer.clone()));
        }
        let until = now() + Duration::from_millis(300) + HOLD;
        loop {
            let said = *answer.borrow();
            match said {
                Some(true) => {
                    let mut w = world.borrow_mut();
                    w.acked.insert(id, group);
                    w.events.push(Ev::Acked(id, group));
                    return;
                }
                Some(false) => break,
                None if now() >= until || !world.borrow().up[name] => break,
                None => tokio::time::sleep(Duration::from_millis(1)).await,
            }
        }
        // Not its writer's, or not answered: the view read again.
        *view.borrow_mut() = None;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn run(design: Design, seed: u64) -> Outcome {
    let mut draw = Draw::new(seed);
    // Each seed one victim and one step, in turn across the seeds.
    let victim = VICTIMS[(seed % 4) as usize];
    let at_step = STEPS[((seed / 4) % 5) as usize];
    let start = Head {
        group: 1,
        epoch: 1,
        seq: 0,
    };
    let world: Shared = Rc::new(RefCell::new(World {
        design,
        draw: Draw::new(seed.wrapping_add(1)),
        db: [
            Db::new(Store::default()),
            Db::new(Store {
                head: Some(start),
                entries: BTreeMap::new(),
            }),
        ],
        epoch: [1, 1],
        hold_until: None,
        step: None,
        up: VICTIMS.iter().map(|v| (*v, true)).collect(),
        acked: BTreeMap::new(),
        events: Vec::new(),
    }));
    let inboxes: [Inbox; 2] = [Rc::default(), Rc::default()];
    let mut sim = world::sim(seed, 90);
    for (group, name) in [(0u32, "w0"), (1, "w1")] {
        let world = world.clone();
        let inbox = inboxes[group as usize].clone();
        sim.host(name, move || {
            let world = world.clone();
            let inbox = inbox.clone();
            async move {
                world.borrow_mut().up.insert(name, true);
                if group == 1 {
                    // A restarted source holds nothing: its hold was memory.
                    world.borrow_mut().hold_until = None;
                }
                if group == 0 && world.borrow().step.is_none() {
                    // The move runs in group 0's member, from its first start.
                    let moving = world.clone();
                    tokio::task::spawn_local(async move {
                        tokio::time::sleep(Duration::from_millis(1_500)).await;
                        mover(moving).await;
                    });
                }
                writer(world, group, inbox).await
            }
        });
    }
    let into_step = draw.ms(0, 30);
    let faults = world::Faults::default();
    let fault = faults.clone();
    let driver = world.clone();
    sim.client("crash", async move {
        // At the step, after a seeded pause into it: the victim lost.
        for _ in 0..60_000 {
            if driver.borrow().step.is_some_and(|s| s >= at_step) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        tokio::time::sleep(into_step).await;
        driver.borrow_mut().events.push(Ev::Crashed(victim));
        match victim {
            "db0" | "db1" => {
                let i = usize::from(victim == "db1");
                let lost = driver.borrow_mut().db[i].fail_over(Duration::from_millis(20));
                driver
                    .borrow_mut()
                    .events
                    .push(Ev::FailedOver(i as u32, lost));
            }
            host => {
                driver.borrow_mut().up.insert(host, false);
                fault.crash(host);
                let down = driver.borrow_mut().draw.ms(200, 2_000);
                tokio::time::sleep(down).await;
                fault.bounce(host);
            }
        }
        Ok(())
    });
    let callers = world.clone();
    sim.client("callers", async move {
        // Two members' views, each act through one of them.
        let views: [View; 2] = [Rc::default(), Rc::default()];
        let mut tasks = Vec::new();
        for id in 0..ACTS {
            let gap = callers.borrow_mut().draw.ms(5, 25);
            tokio::time::sleep(gap).await;
            let view = views[(id % 2) as usize].clone();
            tasks.push(tokio::task::spawn_local(act(
                callers.clone(),
                inboxes.clone(),
                id,
                view,
            )));
        }
        for task in tasks {
            task.await?;
        }
        // The sweep's second, and the hold, before the run is judged.
        tokio::time::sleep(HOLD + Duration::from_secs(2)).await;
        Ok(())
    });
    faults.run(&mut sim).expect("the model runs");
    let world = world.borrow();
    judge(&world)
}

fn judge(w: &World) -> Outcome {
    let mut outcome = Outcome::default();
    let (zero, one) = (w.db[0].read(), w.db[1].read());
    for (seq, id) in &zero.entries {
        if let Some(other) = one.entries.get(seq) {
            if other != id {
                outcome.broken.push(format!(
                    "one-owner: sequence {seq} committed as act {other} in db1 and act {id} in db0"
                ));
                break;
            }
        }
    }
    let open: Vec<u32> = [zero.head, one.head]
        .iter()
        .flatten()
        .filter(|h| h.open())
        .map(|h| h.group)
        .collect();
    match open.as_slice() {
        [owner] => {
            let held = if *owner == 0 { zero } else { one };
            let kept: std::collections::BTreeSet<u64> = held.entries.values().copied().collect();
            if let Some(id) = w.acked.keys().find(|id| !kept.contains(id)) {
                outcome.broken.push(format!(
                    "acked-kept: act {id} was acknowledged and is not in group {owner}'s database"
                ));
            }
        }
        [] => outcome.broken.push(format!(
            "settled: no open head after the move (db0 {:?}, db1 {:?})",
            zero.head, one.head
        )),
        _ => outcome.broken.push(format!(
            "settled: two open heads after the move (db0 {:?}, db1 {:?})",
            zero.head, one.head
        )),
    }
    for ev in &w.events {
        match ev {
            Ev::Swept(_) => outcome.count("sweeps that acted", 1),
            Ev::TimedOut => outcome.count("switches timed out", 1),
            Ev::Acked(..) => outcome.count("acts acknowledged", 1),
            _ => {}
        }
    }
    outcome.count("acts never acknowledged", ACTS - w.acked.len() as u64);
    outcome.digest = Outcome::digest_of(&w.events);
    outcome
}

#[test]
fn fast_tier() {
    let today = Tally::of(world::fast(), |seed| run(Design::Today, seed));
    let first = Tally::of(world::fast(), |seed| run(Design::TargetFirst, seed));
    eprintln!("{}\n{}", today.say("moves today"), first.say("moves C3"));
    assert!(today.red.is_empty(), "{}", today.say("moves today"));
    assert!(
        first.broke("one-owner") || first.broke("settled"),
        "{}",
        first.say("moves C3")
    );
}

#[test]
fn the_same_seed_is_the_same_run() {
    for design in [Design::Today, Design::TargetFirst] {
        assert_eq!(run(design, 11).digest, run(design, 11).digest, "{design:?}");
    }
}

#[test]
#[ignore = "the sweep: SIM_SEEDS=<n> cargo test -p meridian-sim -- --ignored"]
fn sweep() {
    for design in [Design::Today, Design::TargetFirst] {
        let tally = Tally::of(world::sweep(), |seed| run(design, seed));
        eprintln!("{}", tally.say(&format!("moves {design:?}")));
    }
}
