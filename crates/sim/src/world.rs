//! What every model shares: the seeded draw, virtual time, a database fake
//! that loses its unsynchronised tail on a failover, the outcome of one run
//! and the sweep over seeds.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

/// A small seeded generator for a model's choices: the same seed, the same
/// choices (SplitMix64).
#[derive(Clone)]
pub struct Draw(u64);

impl Draw {
    pub fn new(seed: u64) -> Self {
        // Kept apart from turmoil's own use of the seed.
        Self(seed ^ 0x005E_ED0F_A51A_u64)
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in `0..n`.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    /// True `in_` times out of `of`.
    pub fn chance(&mut self, in_: u64, of: u64) -> bool {
        self.below(of) < in_
    }

    /// A duration in `lo..=hi` milliseconds.
    pub fn ms(&mut self, lo: u64, hi: u64) -> Duration {
        Duration::from_millis(lo + self.below(hi.saturating_sub(lo) + 1))
    }
}

/// Virtual time since the simulation began, the same on every host.
pub fn now() -> Duration {
    turmoil::sim_elapsed().unwrap_or_default()
}

/// A world of `seconds` virtual seconds, ticking every millisecond, its
/// network, its hosts' order each tick and every host's tokio scheduler
/// seeded from `seed`.
pub fn sim<'a>(seed: u64, seconds: u64) -> turmoil::Sim<'a> {
    turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(seconds))
        .tick_duration(Duration::from_millis(1))
        .min_message_latency(Duration::from_millis(1))
        .max_message_latency(Duration::from_millis(1))
        .rng_seed(seed)
        .enable_random_order()
        .build()
}

/// Hosts a model asks to crash or bring back, from inside the run: turmoil
/// crashes and bounces a host from outside it, between steps.
#[derive(Clone, Default)]
pub struct Faults(Rc<std::cell::RefCell<Vec<(bool, &'static str)>>>);

impl Faults {
    /// `host`'s software lost, its tasks and memory with it.
    pub fn crash(&self, host: &'static str) {
        self.0.borrow_mut().push((true, host));
    }

    /// `host` started again: its software run afresh.
    pub fn bounce(&self, host: &'static str) {
        self.0.borrow_mut().push((false, host));
    }

    /// The run to its end, the faults asked for applied between steps.
    pub fn run(&self, sim: &mut turmoil::Sim<'_>) -> turmoil::Result {
        loop {
            let asked: Vec<_> = self.0.borrow_mut().drain(..).collect();
            for (crash, host) in asked {
                if crash {
                    sim.crash(host);
                } else {
                    sim.bounce(host);
                }
            }
            if sim.step()? {
                return Ok(());
            }
        }
    }
}

/// A database primary with an asynchronous standby, as the models need it:
/// a commit applies at once; a failover keeps every commit the standby
/// holds and loses the rest. The standby holds a commit once a synchronous
/// commit after it was acknowledged (the write-ahead log is one order), or
/// once it is older than the standby's lag.
/// One commit: when, whether on the synchronous standby, and what it did.
type Commit<S> = (Duration, bool, Rc<dyn Fn(&mut S)>);

pub struct Db<S: Clone> {
    base: S,
    log: Vec<Commit<S>>,
    now: S,
    /// Raised by each failover: what a member's connection watch sees.
    pub timeline: u32,
}

impl<S: Clone> Db<S> {
    pub fn new(base: S) -> Self {
        Self {
            now: base.clone(),
            base,
            log: Vec::new(),
            timeline: 1,
        }
    }

    /// What a transaction reads: every commit so far.
    pub fn read(&self) -> &S {
        &self.now
    }

    /// A transaction committed, `sync` on the synchronous standby
    /// (`synchronous_commit = on`) or locally.
    pub fn commit(&mut self, sync: bool, op: impl Fn(&mut S) + 'static) {
        op(&mut self.now);
        self.log.push((now(), sync, Rc::new(op)));
    }

    /// The primary lost and its standby promoted, `lag` behind: the commits
    /// it never held are gone. Answers how many were lost.
    pub fn fail_over(&mut self, lag: Duration) -> usize {
        let at = now();
        let kept = self
            .log
            .iter()
            .rposition(|(when, sync, _)| *sync || at.saturating_sub(*when) >= lag)
            .map_or(0, |i| i + 1);
        let lost = self.log.len() - kept;
        self.log.truncate(kept);
        self.now = self.base.clone();
        for (_, _, op) in &self.log {
            op(&mut self.now);
        }
        self.timeline += 1;
        lost
    }
}

/// One run's verdict: each invariant it broke, in words with the seed's
/// history around it; counts worth reporting; and a digest of its history,
/// for scenario 0.
#[derive(Debug, Default)]
pub struct Outcome {
    pub broken: Vec<String>,
    pub counts: BTreeMap<&'static str, u64>,
    pub digest: u64,
}

impl Outcome {
    pub fn count(&mut self, what: &'static str, n: u64) {
        *self.counts.entry(what).or_default() += n;
    }

    pub fn digest_of<T: Hash>(history: &T) -> u64 {
        // DefaultHasher::new() has fixed keys: the same in every run.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        history.hash(&mut hasher);
        hasher.finish()
    }
}

/// The fast tier's seeds: in the unit tests, under a minute a model.
pub fn fast() -> Range<u64> {
    0..32
}

/// The sweep's seeds: `SIM_SEEDS` of them (default 2,000), from `SIM_FROM`.
pub fn sweep() -> Range<u64> {
    let read = |name: &str, default: u64| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let from = read("SIM_FROM", 0);
    from..from + read("SIM_SEEDS", 2_000)
}

/// Every seed of `seeds` run: the seeds that broke an invariant, the first
/// break of each kind found, and the counts summed.
#[derive(Debug, Default)]
pub struct Tally {
    pub runs: u64,
    pub red: Vec<u64>,
    pub first: BTreeMap<String, (u64, String)>,
    pub counts: BTreeMap<&'static str, u64>,
    pub took: Duration,
}

impl Tally {
    pub fn of(seeds: Range<u64>, run: impl Fn(u64) -> Outcome) -> Self {
        let began = std::time::Instant::now();
        let mut tally = Tally::default();
        for seed in seeds {
            let outcome = run(seed);
            tally.runs += 1;
            if !outcome.broken.is_empty() {
                tally.red.push(seed);
            }
            for broken in outcome.broken {
                let kind = broken.split(':').next().unwrap_or_default().to_string();
                tally.first.entry(kind).or_insert((seed, broken));
            }
            for (what, n) in outcome.counts {
                *tally.counts.entry(what).or_default() += n;
            }
        }
        tally.took = began.elapsed();
        tally
    }

    /// Whether any seed broke the invariant named `kind`.
    pub fn broke(&self, kind: &str) -> bool {
        self.first.contains_key(kind)
    }

    pub fn say(&self, name: &str) -> String {
        let mut said = format!(
            "{name}: {} seeds in {:.1}s, {} red {:?}",
            self.runs,
            self.took.as_secs_f64(),
            self.red.len(),
            &self.red[..self.red.len().min(12)]
        );
        for (kind, (seed, broken)) in &self.first {
            said.push_str(&format!("\n  {kind}: first at seed {seed}: {broken}"));
        }
        said.push_str(&format!("\n  counts: {:?}", self.counts));
        said
    }
}
