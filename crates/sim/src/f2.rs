//! Scenario 2: the F2 window (review-c5n5 F2; the architecture review's Q8).
//!
//! Today (core main 8599e1fd): a member that receives a write of another
//! writer group's decides the owner off the runtime, checks its `refusing`
//! flag, and publishes the write to the owner's internal topic with the
//! caller's reply address (`crates/bus/src/nats.rs:1067-1110`). A write that
//! spans groups is handed on part by part from its handler, each part after
//! the same check (`hand_unless_refusing`, `crates/bus/src/router.rs:1999`).
//! The check and the publish are two steps: the fence
//! (`refuse_from_here`, `router.rs:778`) is set by another thread and may
//! land between them. The model puts the publish's own await
//! (async-nats's send into its connection's queue) between the two, which
//! is where a single-threaded run can interleave them; on the real
//! multi-threaded runtime the flag may flip at any instant there.
//!
//! The invariant (ruling 120): no hand-on that entered the member's
//! outbound order after its fence is put on the wire (strict), or acted on
//! by its owner. Beside it: every request is answered at most once; a write
//! answered as recorded has every part committed; and a part is committed
//! under its owner's epoch.
//!
//! Variants: Q8's options (a) one ordered outbound queue per member with the
//! fence a marker in it, and (b) the sender's epoch carried with each
//! hand-on and refused by the owner where it is stale.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;
use std::time::Duration;

use crate::world::{self, now, Draw, Outcome, Tally};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Design {
    /// Core main: the check, then the publish.
    Today,
    /// Q8 (a): the fence is a marker in the member's one outbound queue.
    OrderedQueue,
    /// Q8 (b): the owner refuses a hand-on whose sender's epoch is stale.
    SenderEpoch,
}

/// The directed case's point: a hand-on past its `refusing` check and not
/// yet in its connection's order.
#[derive(Debug)]
pub struct PastTheCheck {
    pub member: &'static str,
}

/// Each member, its writer group and whether it starts as its group's
/// writer: `b0` is group 0's standby, which takes over and fences `a0`.
const MEMBERS: [(&str, u32, bool); 3] = [("a0", 0, true), ("b0", 0, false), ("a1", 1, true)];
const WRITES: u64 = 48;
/// The caller's wait for one answer, and its retry-after.
const WAIT: Duration = Duration::from_millis(1500);
const RETRY_AFTER: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Refusal {
    /// UNAVAILABLE with a retry-after: ask again.
    AskAgain,
    /// The writer's commit refused by its group's epoch: the write fails.
    Fenced,
}

#[derive(Clone, Copy, Debug)]
enum Reply {
    Caller { id: u64, attempt: u32 },
    Handler { member: &'static str, token: u64 },
}

#[derive(Clone, Debug)]
enum Msg {
    /// A write on the store's public topic: one or two groups' parts.
    Write {
        id: u64,
        attempt: u32,
        parts: Vec<u32>,
    },
    /// A part handed to its group on the internal topic.
    Handed {
        id: u64,
        part: u32,
        from_group: u32,
        sender_epoch: u64,
        /// The model's own note, never read by the design: whether this
        /// hand-on entered its sender's order after the sender's fence.
        after_fence: bool,
        reply: Reply,
    },
    Answer {
        reply: Reply,
        said: Result<(), Refusal>,
    },
}

enum Out {
    Hand(Msg),
    Fence,
}

#[derive(Clone, Copy, Debug)]
enum To {
    Public,
    Group(u32),
    Caller,
    Member(&'static str),
}

#[derive(Debug, Hash)]
enum Ev {
    Fenced(&'static str, u64),
    Enqueued(&'static str, u64, u32, bool),
    Wired(&'static str, u64, u32, bool),
    Committed(&'static str, u64, u32, u64, Option<bool>),
    Refused(&'static str, u64, u32),
    Answered(u64, u32, Result<(), Refusal>),
}

struct Member {
    group: u32,
    /// The epoch this member writes under: 0 until it holds.
    epoch: u64,
    public: bool,
    internal: bool,
    refusing: bool,
    /// Option (a): the fence marker passed by the drain.
    drained_fence: bool,
    inbox: VecDeque<Msg>,
    outbound: VecDeque<Out>,
    waiting: BTreeMap<u64, Option<Result<(), Refusal>>>,
}

struct World {
    design: Design,
    draw: Draw,
    epoch: [u64; 2],
    members: BTreeMap<&'static str, Member>,
    flight: Vec<(Duration, To, Msg)>,
    caller: BTreeMap<(u64, u32), Vec<Result<(), Refusal>>>,
    committed: BTreeMap<(u64, u32), (&'static str, u64)>,
    repeats: u64,
    finals: BTreeMap<u64, Result<(), Refusal>>,
    parts: BTreeMap<u64, Vec<u32>>,
    events: Vec<Ev>,
    token: u64,
    /// The directed case sends its first write to this member.
    first_to: Option<&'static str>,
}

type Shared = Rc<RefCell<World>>;

impl World {
    fn send(&mut self, to: To, msg: Msg) {
        let at = now() + self.draw.ms(1, 3);
        self.flight.push((at, to, msg));
    }

    fn member(&mut self, name: &'static str) -> &mut Member {
        self.members.get_mut(name).expect("a member of the model")
    }

    /// Commit a part under `name`'s epoch, as its group's writer: refused
    /// where the epoch moved on.
    fn commit(
        &mut self,
        name: &'static str,
        id: u64,
        part: u32,
        via: Option<bool>,
    ) -> Result<(), Refusal> {
        let epoch = self.members[name].epoch;
        if epoch == 0 || self.epoch[part as usize] != epoch {
            self.events.push(Ev::Refused(name, id, part));
            return Err(Refusal::Fenced);
        }
        match self.committed.entry((id, part)) {
            // Its row key: recorded once.
            std::collections::btree_map::Entry::Occupied(_) => self.repeats += 1,
            std::collections::btree_map::Entry::Vacant(at) => {
                at.insert((name, epoch));
            }
        }
        self.events.push(Ev::Committed(name, id, part, epoch, via));
        Ok(())
    }

    /// The fence lands: `refuse_from_here`, and the internal topic
    /// withdrawn.
    fn refuse_from_here(&mut self, name: &'static str) {
        let design = self.design;
        let member = self.member(name);
        if member.refusing {
            return;
        }
        member.refusing = true;
        member.internal = false;
        if design == Design::OrderedQueue {
            member.outbound.push_back(Out::Fence);
        }
        let n = self.events.len() as u64;
        self.events.push(Ev::Fenced(name, n));
    }

    /// A part handed on: into the member's order.
    fn enqueue(&mut self, name: &'static str, id: u64, part: u32, reply: Reply) {
        let design = self.design;
        let sender_epoch = self.members[name].epoch;
        let member = self.member(name);
        let after_fence = match design {
            // In one queue with the marker: after it where the marker went
            // in first.
            Design::OrderedQueue => {
                member.outbound.iter().any(|o| matches!(o, Out::Fence)) || member.drained_fence
            }
            _ => member.refusing,
        };
        let group = member.group;
        member.outbound.push_back(Out::Hand(Msg::Handed {
            id,
            part,
            from_group: group,
            sender_epoch,
            after_fence,
            reply,
        }));
        self.events.push(Ev::Enqueued(name, id, part, after_fence));
    }

    /// The member's connection: its order on the wire.
    fn drain(&mut self, name: &'static str) {
        loop {
            let design = self.design;
            let member = self.member(name);
            let Some(out) = member.outbound.pop_front() else {
                return;
            };
            match out {
                Out::Fence => member.drained_fence = true,
                Out::Hand(msg) => {
                    let Msg::Handed {
                        id,
                        part,
                        after_fence,
                        reply,
                        ..
                    } = msg
                    else {
                        unreachable!()
                    };
                    if design == Design::OrderedQueue && member.drained_fence {
                        // After the marker: refused, ask again.
                        self.answer(reply, Err(Refusal::AskAgain));
                        continue;
                    }
                    self.events.push(Ev::Wired(name, id, part, after_fence));
                    self.send(To::Group(part), msg);
                }
            }
        }
    }

    fn answer(&mut self, reply: Reply, said: Result<(), Refusal>) {
        match reply {
            Reply::Caller { .. } => self.send(To::Caller, Msg::Answer { reply, said }),
            Reply::Handler { member, .. } => {
                self.send(To::Member(member), Msg::Answer { reply, said })
            }
        }
    }

    /// The broker: each message whose time came delivered, a queue group's
    /// to one of its members by the seed, none answered "no responders".
    fn deliver(&mut self) {
        let at = now();
        let (due, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.flight)
            .into_iter()
            .partition(|(when, _, _)| *when <= at);
        self.flight = kept;
        for (_, to, msg) in due {
            let pick = |world: &mut World, ok: &dyn Fn(&Member) -> bool| {
                let serving: Vec<&'static str> = world
                    .members
                    .iter()
                    .filter(|(_, m)| ok(m))
                    .map(|(n, _)| *n)
                    .collect();
                if serving.is_empty() {
                    None
                } else {
                    let i = world.draw.below(serving.len() as u64) as usize;
                    Some(serving[i])
                }
            };
            match to {
                To::Caller => {
                    if let Msg::Answer {
                        reply: Reply::Caller { id, attempt },
                        said,
                    } = msg
                    {
                        self.events.push(Ev::Answered(id, attempt, said));
                        self.caller.entry((id, attempt)).or_default().push(said);
                    }
                }
                To::Member(name) => self.member(name).inbox.push_back(msg),
                To::Public => {
                    let first = match &msg {
                        Msg::Write {
                            id: 0, attempt: 0, ..
                        } => self.first_to,
                        _ => None,
                    };
                    match first.or_else(|| pick(self, &|m| m.public)) {
                        Some(name) => self.member(name).inbox.push_back(msg),
                        None => {
                            if let Msg::Write { id, attempt, .. } = msg {
                                self.answer(Reply::Caller { id, attempt }, Err(Refusal::AskAgain));
                            }
                        }
                    }
                }
                To::Group(group) => match pick(self, &|m| m.internal && m.group == group) {
                    Some(name) => self.member(name).inbox.push_back(msg),
                    None => {
                        if let Msg::Handed { reply, .. } = msg {
                            self.answer(reply, Err(Refusal::AskAgain));
                        }
                    }
                },
            }
        }
    }
}

/// The publish's own await: the fence may land here.
async fn publish_await() {
    tokio::task::yield_now().await;
}

/// A member's handler of a write it serves, on its own task.
async fn handle(world: Shared, name: &'static str, id: u64, attempt: u32, parts: Vec<u32>) {
    let caller = Reply::Caller { id, attempt };
    let group = world.borrow().members[name].group;
    // The pre-route: a write of one other group's, decided off the runtime
    // (spawn_blocking), checked, and published to its owner.
    if parts.len() == 1 && parts[0] != group {
        let decide = world.borrow_mut().draw.ms(0, 2);
        tokio::time::sleep(decide).await;
        // Under (a) there is no check apart from the order: the marker
        // decides, whenever the hand-on goes in.
        let refuses =
            world.borrow().design != Design::OrderedQueue && world.borrow().members[name].refusing;
        if !refuses {
            turmoil::barriers::trigger(PastTheCheck { member: name }).await;
            publish_await().await;
            world.borrow_mut().enqueue(name, id, parts[0], caller);
            return;
        }
    }
    // Its handler, admitted on the blocking pool.
    if world.borrow().members[name].refusing {
        world.borrow_mut().answer(caller, Err(Refusal::AskAgain));
        return;
    }
    let mut said = Ok(());
    for part in parts {
        let one = if part == group {
            world.borrow_mut().commit(name, id, part, None)
        } else {
            hand(&world, name, id, part).await
        };
        if said.is_ok() {
            said = one;
        }
    }
    world.borrow_mut().answer(caller, said);
}

/// `hand_unless_refusing`: a part handed to its owner and waited on.
async fn hand(world: &Shared, name: &'static str, id: u64, part: u32) -> Result<(), Refusal> {
    let design = world.borrow().design;
    if design != Design::OrderedQueue && world.borrow().members[name].refusing {
        return Err(Refusal::AskAgain);
    }
    turmoil::barriers::trigger(PastTheCheck { member: name }).await;
    publish_await().await;
    let token = {
        let mut w = world.borrow_mut();
        w.token += 1;
        let token = w.token;
        w.member(name).waiting.insert(token, None);
        w.enqueue(
            name,
            id,
            part,
            Reply::Handler {
                member: name,
                token,
            },
        );
        token
    };
    let until = now() + WAIT - Duration::from_millis(250);
    loop {
        if let Some(Some(said)) = world.borrow_mut().member(name).waiting.get(&token).copied() {
            return said;
        }
        if now() >= until {
            return Err(Refusal::AskAgain);
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// A member: its inbox served, its connection drained, its group's epoch
/// watched.
async fn member(world: Shared, name: &'static str) -> turmoil::Result {
    let look = world.borrow_mut().draw.ms(1, 5);
    let mut looked = now();
    loop {
        let msgs: Vec<Msg> = world.borrow_mut().member(name).inbox.drain(..).collect();
        for msg in msgs {
            match msg {
                Msg::Write { id, attempt, parts } => {
                    tokio::task::spawn_local(handle(world.clone(), name, id, attempt, parts));
                }
                Msg::Handed {
                    id,
                    part,
                    from_group,
                    sender_epoch,
                    after_fence,
                    reply,
                    ..
                } => {
                    let mut w = world.borrow_mut();
                    let stale = w.design == Design::SenderEpoch
                        && w.epoch[from_group as usize] != sender_epoch;
                    let said = if stale {
                        Err(Refusal::AskAgain)
                    } else {
                        w.commit(name, id, part, Some(after_fence))
                    };
                    w.answer(reply, said);
                }
                Msg::Answer {
                    reply: Reply::Handler { token, .. },
                    said,
                } => {
                    if let Some(slot) = world.borrow_mut().member(name).waiting.get_mut(&token) {
                        *slot = Some(said);
                    }
                }
                Msg::Answer { .. } => {}
            }
        }
        if now() >= looked + look {
            looked = now();
            let mut w = world.borrow_mut();
            let group = w.members[name].group;
            let (epoch, held) = (w.epoch[group as usize], w.members[name].epoch);
            if held != 0 && epoch != held {
                w.refuse_from_here(name);
            }
        }
        world.borrow_mut().drain(name);
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// `b0` takes over group 0: the epoch raised, then it serves.
fn take_over(world: &Shared) {
    let mut w = world.borrow_mut();
    w.epoch[0] += 1;
    let epoch = w.epoch[0];
    let b0 = w.member("b0");
    b0.epoch = epoch;
    b0.public = true;
    b0.internal = true;
}

fn world(design: Design, seed: u64, first_to: Option<&'static str>) -> Shared {
    let mut members = BTreeMap::new();
    for (name, group, writer) in MEMBERS {
        members.insert(
            name,
            Member {
                group,
                epoch: u64::from(writer),
                public: writer,
                internal: writer,
                refusing: false,
                drained_fence: false,
                inbox: VecDeque::new(),
                outbound: VecDeque::new(),
                waiting: BTreeMap::new(),
            },
        );
    }
    Rc::new(RefCell::new(World {
        design,
        draw: Draw::new(seed),
        epoch: [1, 1],
        members,
        flight: Vec::new(),
        caller: BTreeMap::new(),
        committed: BTreeMap::new(),
        repeats: 0,
        finals: BTreeMap::new(),
        parts: BTreeMap::new(),
        events: Vec::new(),
        token: 0,
        first_to,
    }))
}

/// The caller's write: asked again on "ask again" or no answer, until its
/// own deadline.
async fn write(world: Shared, id: u64, parts: Vec<u32>) {
    let mut attempt = 0;
    let given_up = now() + Duration::from_secs(8);
    loop {
        world.borrow_mut().send(
            To::Public,
            Msg::Write {
                id,
                attempt,
                parts: parts.clone(),
            },
        );
        let until = now() + WAIT;
        let said = loop {
            if let Some(said) = world
                .borrow()
                .caller
                .get(&(id, attempt))
                .and_then(|s| s.first())
            {
                break Some(*said);
            }
            if now() >= until {
                break None;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        };
        match said {
            Some(Ok(())) | Some(Err(Refusal::Fenced)) => {
                world
                    .borrow_mut()
                    .finals
                    .insert(id, said.expect("answered"));
                return;
            }
            _ if now() + RETRY_AFTER >= given_up => {
                world
                    .borrow_mut()
                    .finals
                    .insert(id, said.unwrap_or(Err(Refusal::AskAgain)));
                return;
            }
            _ => {
                tokio::time::sleep(RETRY_AFTER).await;
                attempt += 1;
            }
        }
    }
}

fn hosts(sim: &mut turmoil::Sim<'_>, world: &Shared) {
    // The broker: one host, every tick.
    let broker = world.clone();
    sim.host("nats", move || {
        let world = broker.clone();
        async move {
            loop {
                world.borrow_mut().deliver();
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    });
    for (name, _, _) in MEMBERS {
        let world = world.clone();
        sim.host(name, move || member(world.clone(), name));
    }
}

/// The caller's writes: a burst around the takeover, each of group 1's
/// alone or spanning both.
async fn caller(world: Shared, take_at: Duration) -> turmoil::Result {
    let mut tasks = Vec::new();
    for id in 0..WRITES {
        let (at, parts) = {
            let mut w = world.borrow_mut();
            let around = take_at.saturating_sub(Duration::from_millis(30));
            let at = around + w.draw.ms(0, 60);
            let parts = if w.draw.chance(1, 3) {
                vec![0, 1]
            } else {
                vec![1]
            };
            w.parts.insert(id, parts.clone());
            (at, parts)
        };
        let world = world.clone();
        tasks.push(tokio::task::spawn_local(async move {
            tokio::time::sleep_until(tokio::time::Instant::now() + at.saturating_sub(now())).await;
            write(world, id, parts).await;
        }));
    }
    for task in tasks {
        task.await?;
    }
    // Let in-flight hand-ons land before the run ends.
    tokio::time::sleep(Duration::from_secs(2)).await;
    Ok(())
}

/// One run of `design` under `seed`.
fn run(design: Design, seed: u64) -> Outcome {
    let world = world(design, seed, None);
    let mut sim = world::sim(seed, 40);
    hosts(&mut sim, &world);
    let take_at = Duration::from_millis(200) + world.borrow_mut().draw.ms(0, 400);
    let driver = world.clone();
    sim.client("caller", caller(world.clone(), take_at));
    sim.client("takeover", async move {
        tokio::time::sleep(take_at).await;
        take_over(&driver);
        Ok(())
    });
    sim.run().expect("the model runs");
    let world = world.borrow();
    judge(&world)
}

/// The invariants, read from the run's history.
fn judge(w: &World) -> Outcome {
    let mut outcome = Outcome::default();
    for ev in &w.events {
        match ev {
            Ev::Wired(member, id, part, true) => outcome.broken.push(format!(
                "ruling-120-wire: {member} put write {id} part {part} on the wire after its fence"
            )),
            Ev::Committed(owner, id, part, epoch, Some(true)) => outcome.broken.push(format!(
                "ruling-120-acted: {owner} committed write {id} part {part} at epoch {epoch}, handed on after its sender's fence"
            )),
            Ev::Fenced(..) => outcome.count("fences", 1),
            Ev::Enqueued(_, _, _, true) => outcome.count("hand-ons ordered after the fence", 1),
            Ev::Refused(..) => outcome.count("commits refused by the epoch", 1),
            _ => {}
        }
    }
    for ((id, attempt), said) in &w.caller {
        if said.len() > 1 {
            outcome.broken.push(format!(
                "answered-once: write {id} attempt {attempt} answered {} times: {said:?}",
                said.len()
            ));
        }
    }
    for (id, said) in &w.finals {
        let parts = &w.parts[id];
        let recorded = parts
            .iter()
            .filter(|p| w.committed.contains_key(&(*id, **p)))
            .count();
        match said {
            Ok(()) if recorded < parts.len() => outcome.broken.push(format!(
                "answered-recorded: write {id} answered recorded with {recorded} of {} parts committed",
                parts.len()
            )),
            Ok(()) => outcome.count("writes recorded", 1),
            Err(Refusal::Fenced) if recorded > 0 => outcome.count("writes failed but partly recorded (F1)", 1),
            Err(_) => outcome.count("writes not recorded, refused", 1),
        }
    }
    outcome.count("parts committed again (idempotent)", w.repeats);
    outcome.digest = Outcome::digest_of(&w.events);
    outcome
}

/// The directed case: the barrier holds `a0`'s first hand-on past its check;
/// `b0` takes over and `a0`'s fence lands; the barrier lets it go.
fn directed(design: Design) -> Outcome {
    let world = world(design, 1, Some("a0"));
    let mut sim = world::sim(1, 40);
    hosts(&mut sim, &world);
    let mut barrier = turmoil::barriers::Barrier::build(
        turmoil::barriers::Reaction::Suspend,
        |t: &PastTheCheck| t.member == "a0",
    );
    let at_the_check = world.clone();
    sim.client("test", async move {
        let held = barrier.wait().await;
        take_over(&at_the_check);
        at_the_check.borrow_mut().refuse_from_here("a0");
        drop(held);
        Ok(())
    });
    let only = world.clone();
    sim.client("caller", async move {
        only.borrow_mut().parts.insert(0, vec![1]);
        write(only.clone(), 0, vec![1]).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        Ok(())
    });
    sim.run().expect("the model runs");
    let world = world.borrow();
    judge(&world)
}

#[test]
fn directed_a_fence_between_the_check_and_the_publish_lets_the_hand_on_out_today() {
    let today = directed(Design::Today);
    assert!(
        today
            .broken
            .iter()
            .any(|b| b.starts_with("ruling-120-wire")),
        "{today:?}"
    );
    assert!(
        today
            .broken
            .iter()
            .any(|b| b.starts_with("ruling-120-acted")),
        "{today:?}"
    );
}

#[test]
fn directed_q8_a_orders_the_hand_on_before_or_after_the_fence() {
    let queue = directed(Design::OrderedQueue);
    assert!(queue.broken.is_empty(), "{queue:?}");
    assert_eq!(
        queue.counts.get("hand-ons ordered after the fence"),
        Some(&1),
        "{queue:?}"
    );
}

#[test]
fn directed_q8_b_lets_the_hand_on_out_but_its_owner_refuses_it() {
    let token = directed(Design::SenderEpoch);
    assert!(
        token
            .broken
            .iter()
            .any(|b| b.starts_with("ruling-120-wire")),
        "{token:?}"
    );
    assert!(
        !token
            .broken
            .iter()
            .any(|b| b.starts_with("ruling-120-acted")),
        "{token:?}"
    );
}

#[test]
fn fast_tier() {
    let today = Tally::of(world::fast(), |seed| run(Design::Today, seed));
    let queue = Tally::of(world::fast(), |seed| run(Design::OrderedQueue, seed));
    let token = Tally::of(world::fast(), |seed| run(Design::SenderEpoch, seed));
    eprintln!(
        "{}\n{}\n{}",
        today.say("f2 today"),
        queue.say("f2 (a)"),
        token.say("f2 (b)")
    );
    for tally in [&today, &queue, &token] {
        assert!(!tally.broke("answered-once"), "{tally:?}");
        assert!(!tally.broke("answered-recorded"), "{tally:?}");
    }
    assert!(queue.red.is_empty(), "{}", queue.say("f2 (a)"));
    assert!(!token.broke("ruling-120-acted"), "{}", token.say("f2 (b)"));
}

#[test]
fn the_same_seed_is_the_same_run() {
    for design in [Design::Today, Design::OrderedQueue, Design::SenderEpoch] {
        assert_eq!(run(design, 7).digest, run(design, 7).digest, "{design:?}");
    }
}

#[test]
#[ignore = "the sweep: SIM_SEEDS=<n> cargo test -p meridian-sim -- --ignored"]
fn sweep() {
    for design in [Design::Today, Design::OrderedQueue, Design::SenderEpoch] {
        let tally = Tally::of(world::sweep(), |seed| run(design, seed));
        eprintln!("{}", tally.say(&format!("f2 {design:?}")));
    }
}
