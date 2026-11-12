//! `MessageMeta.read_after` (contract v30, step 3c stage B; W4.4;
//! spec/a-deployment-scales-out-and-fails-over-by-configuration section 5;
//! approved 2026-11-09, the product owner's rulings of 2026-11-10).
//!
//! **Stamped by the sidecar, never by the plugin.** A plugin's sidecar keeps
//! a [`Session`] on its bus: the highest sequence it has seen on each of a
//! store's partitions, from the plugin's acknowledged writes, the deliveries
//! it passed on and the watermark each of its reads was answered at. Every
//! read it asks of that store names them as `read_after`, so read-your-writes
//! and monotonic reads hold for the instance with no plugin-facing field:
//! session consistency.
//!
//! **Never answered from behind it.** A member serving a store's reads keeps
//! its [`Position`] on its bus. A read naming a watermark the member's record
//! has not reached waits for it, up to the member's bound (its
//! `<store>.readyLagMs`) or the call's remaining deadline less [`MARGIN`],
//! whichever is sooner, so the caller is told the answer or the refusal and
//! never runs out its own deadline. Where the wait ends behind it:
//!
//! - a **reader** member answers from the store's primary database (its
//!   primary's position kept by [`Reads::keep_primary`]), the handler told
//!   so by [`answering_from_primary`] -- once the primary has reached it;
//! - a **writer**, whose record is the primary, refuses UNAVAILABLE with no
//!   reason code: it can be behind only after a database failover that lost
//!   acknowledged rows, and the caller may ask again.
//!
//! The waits are bounded in number too: a read past [`WAITING_MOST`] waiting
//! at once is refused for load, with its retry-after, as every component's
//! overload is (section 10a). A read naming nothing is answered at once,
//! eventually consistent, its reply's watermark saying how current it is.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use meridian_pb::v1::{PartitionSequence, Watermark};

/// What a store's member has reached, per partition, in the record it
/// answers from.
pub trait Position: Send + Sync {
    /// The head of each of `partitions` in this member's record; a partition
    /// it does not hold is absent.
    fn heads(&self, partitions: &[String]) -> Result<HashMap<String, u64>, String>;

    /// Every partition's head in this member's record, where it reads them
    /// whole: what a reader reconciles with each time its writer answers it
    /// again. None where its record cannot list them.
    fn all_heads(&self) -> Option<Result<HashMap<String, u64>, String>> {
        None
    }
}

/// A position read whole -- every partition's head in one look, as a store's
/// own `heads` answers -- and narrowed to the partitions asked.
pub struct AllHeads<F>(pub F);

impl<F> Position for AllHeads<F>
where
    F: Fn() -> Result<std::collections::BTreeMap<String, u64>, String> + Send + Sync,
{
    fn heads(&self, partitions: &[String]) -> Result<HashMap<String, u64>, String> {
        let all = (self.0)()?;
        Ok(partitions
            .iter()
            .filter_map(|partition| all.get(partition).map(|head| (partition.clone(), *head)))
            .collect())
    }

    fn all_heads(&self) -> Option<Result<HashMap<String, u64>, String>> {
        Some((self.0)().map(|all| all.into_iter().collect()))
    }
}

/// A sidecar's session: what it has seen for its instance, and what each of
/// its reads must not be answered behind.
pub trait Session: Send + Sync {
    /// The watermark a read of `domain`'s store names, if anything has been
    /// seen of it.
    fn read_after(&self, domain: &str) -> Option<Watermark>;

    /// An answer this bus was given to a call on `topic`.
    fn answered(&self, topic: &str, payload_type: &str, payload: &[u8]);

    /// A delivery on `topic` it passed on to its plugin, at `sequence` of
    /// `partition` of the store.
    fn delivered(&self, topic: &str, partition: &str, sequence: u64);
}

/// A member's bound where its chart says none: `<store>.readyLagMs`'s
/// default, five seconds.
pub const BOUND_DEFAULT: Duration = Duration::from_millis(5_000);

/// How many reads may wait for their `read_after` at once in one process:
/// half the bus's handler permits, so reads that wait never hold every turn
/// and the rest keep being answered. One more is refused for load.
pub const WAITING_MOST: usize = meridian_budget::HANDLERS_AT_ONCE / 2;

/// How long a head this member's record was seen at answers for it without
/// a look at the record again: a record only moves forward, but after a
/// database failover that lost acknowledged rows it moved back, and a read
/// past what it holds then is refused within this long.
const LOOKED_FOR: Duration = Duration::from_secs(1);

/// How often a waiting read looks at its member's position again: at first
/// soon, then less often, never less often than this.
const LOOK_MOST: Duration = Duration::from_millis(100);

/// What a read's wait leaves of the call's deadline (ruled 2026-11-10): one
/// second, for a reader's answer from the primary and the answer's way back
/// to the caller. At the default 5 s bound and 5 s call timeout a read waits
/// at most 4 s, so its caller hears the answer or the refusal, never
/// DEADLINE_EXCEEDED.
pub const MARGIN: Duration = Duration::from_secs(1);

thread_local! {
    /// When the call being answered on this thread stops being waited for:
    /// set from its timeout as it arrives, before its turn at the gate.
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };

    /// The read being answered on this thread: the watermark it named, and
    /// whether it is answered from the store's primary database.
    static ANSWERING: RefCell<Option<(Watermark, bool)>> = const { RefCell::new(None) };

    /// Whether the store's primary database answered any part of the read
    /// being answered on this thread without its wait: a reader's record
    /// past with no read replica reached, or what its hot layer did not
    /// hold.
    static PRIMARY_ANSWERED: Cell<bool> = const { Cell::new(false) };

    /// Which read this thread is answering: moved by each [`begin_read`], so
    /// what a store noted of one read (a read replica's snapshot, contract
    /// v30, stage C2) is never taken for the next's.
    static READ: Cell<u64> = const { Cell::new(0) };
}

/// Run `answer` with the call's deadline known to it, the deadline being
/// when its caller stops waiting.
pub fn within_deadline<R>(deadline: Option<Instant>, answer: impl FnOnce() -> R) -> R {
    let before = DEADLINE.with(|held| held.replace(deadline));
    let answered = answer();
    DEADLINE.with(|held| held.set(before));
    answered
}

/// The deadline of the call this thread answers, where its caller said one.
pub fn deadline() -> Option<Instant> {
    DEADLINE.with(Cell::get)
}

/// The `read_after` of the read this thread answers, where it named one.
pub fn answering_after() -> Option<Watermark> {
    ANSWERING.with(|held| held.borrow().as_ref().map(|(asked, _)| asked.clone()))
}

/// Whether the read this thread answers is answered from the store's primary
/// database: a reader member behind its `read_after` past its wait.
pub fn answering_from_primary() -> bool {
    ANSWERING.with(|held| held.borrow().as_ref().is_some_and(|(_, primary)| *primary))
}

/// A read begins on this thread: nothing of it answered from the primary
/// yet. [`Bus::serve`] calls it before each store's read; a store's own
/// tests, answering a read with no bus, call it too.
///
/// [`Bus::serve`]: crate::Bus::serve
pub fn begin_read() {
    PRIMARY_ANSWERED.with(|held| held.set(false));
    READ.with(|read| read.set(read.get().wrapping_add(1)));
}

/// Which read this thread is answering, as [`begin_read`] last moved it:
/// what a store keeps of one read is held to it.
pub fn this_read() -> u64 {
    READ.with(Cell::get)
}

/// Part of the read this thread answers was answered from the store's
/// primary database, though it did not wait: a reader member's reply then
/// carries the primary's watermark (spec section 5, "the primary's for an
/// answer from the primary").
pub fn primary_answered_part() {
    PRIMARY_ANSWERED.with(|held| held.set(true));
}

/// Whether the store's primary database answered the read this thread
/// answers, in whole past its wait or in part without one: its reply's
/// watermark is then the primary's.
pub fn answered_by_primary() -> bool {
    answering_from_primary() || PRIMARY_ANSWERED.with(Cell::get)
}

fn answering<R>(asked: &Watermark, from_primary: bool, answer: impl FnOnce() -> R) -> R {
    let before = ANSWERING.with(|held| held.replace(Some((asked.clone(), from_primary))));
    let answered = answer();
    ANSWERING.with(|held| *held.borrow_mut() = before);
    answered
}

/// Where a read held to its `read_after` is answered from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Held {
    /// This member's own record, at or past it.
    Here,
    /// The store's primary database: a reader still behind it.
    FromPrimary,
}

/// The store a topic belongs to: `platform.<domain>.…`.
pub fn domain_of(topic: &str) -> Option<&str> {
    let mut parts = topic.split('.');
    match (parts.next(), parts.next()) {
        (Some("platform"), Some(domain)) if !domain.is_empty() => Some(domain),
        _ => None,
    }
}

/// Whether a topic is a store's read (`platform.<domain>.query.…`).
pub fn is_read(topic: &str) -> bool {
    topic.split('.').nth(2) == Some("query")
}

/// The highest of `at` and each of `seen`, per partition: a session only
/// ever moves forward.
pub fn raise(at: &mut HashMap<String, u64>, seen: impl IntoIterator<Item = (String, u64)>) {
    for (partition, sequence) in seen {
        if partition.is_empty() || sequence == 0 {
            continue;
        }
        let held = at.entry(partition).or_insert(0);
        *held = (*held).max(sequence);
    }
}

/// A watermark of `at`, its partitions in order.
pub fn watermark_of(at: &HashMap<String, u64>) -> Watermark {
    let mut partitions: Vec<PartitionSequence> = at
        .iter()
        .map(|(partition, sequence)| PartitionSequence {
            partition: partition.clone(),
            sequence: *sequence,
        })
        .collect();
    partitions.sort_by(|a, b| a.partition.cmp(&b.partition));
    Watermark { partitions }
}

/// The partitions of `asked` that `heads` has not reached, with how far
/// each is behind.
pub fn behind(asked: &Watermark, heads: &HashMap<String, u64>) -> Vec<(String, u64, u64)> {
    asked
        .partitions
        .iter()
        .filter_map(|wanted| {
            let at = heads.get(&wanted.partition).copied().unwrap_or(0);
            (at < wanted.sequence).then(|| (wanted.partition.clone(), wanted.sequence, at))
        })
        .collect()
}

/// The heads a store's record was seen at, per partition, and when.
type Looked = HashMap<String, (u64, Instant)>;

/// A bus's part in `read_after`: the positions of the stores it serves the
/// reads of, the bound they wait up to, and the sidecar's session where it
/// is one.
pub struct Reads {
    positions: RwLock<HashMap<String, Arc<dyn Position>>>,
    /// A reader member's store's primary database, per store: where a read
    /// still behind its `read_after` at the end of its wait is answered.
    primaries: RwLock<HashMap<String, Arc<dyn Position>>>,
    /// What this process has itself committed, per store and partition: a
    /// read naming no more than these is answered with no look at the record.
    committed: Mutex<HashMap<String, HashMap<String, u64>>>,
    /// The heads its record was last seen at, per store and partition, and
    /// when: a read naming no more is answered with no look for
    /// [`LOOKED_FOR`], so reads that keep naming a session's watermark cost
    /// the record one look a second, not one each.
    looked: Mutex<HashMap<String, Looked>>,
    bound_ms: AtomicU64,
    waiting: AtomicUsize,
    session: RwLock<Option<Arc<dyn Session>>>,
}

impl Default for Reads {
    fn default() -> Self {
        Self {
            positions: RwLock::new(HashMap::new()),
            primaries: RwLock::new(HashMap::new()),
            committed: Mutex::new(HashMap::new()),
            looked: Mutex::new(HashMap::new()),
            bound_ms: AtomicU64::new(BOUND_DEFAULT.as_millis() as u64),
            waiting: AtomicUsize::new(0),
            session: RwLock::new(None),
        }
    }
}

impl Reads {
    /// Keep `domain`'s reads from being answered behind their `read_after`,
    /// by `position`.
    pub fn keep(&self, domain: &str, position: Arc<dyn Position>) {
        self.positions
            .write()
            .expect("positions poisoned")
            .insert(domain.to_string(), position);
    }

    /// A reader member's store's primary database, `primary` its position:
    /// a read of `domain` still behind its `read_after` when its wait ends is
    /// answered from it, where it has reached it.
    pub fn keep_primary(&self, domain: &str, primary: Arc<dyn Position>) {
        self.primaries
            .write()
            .expect("primaries poisoned")
            .insert(domain.to_string(), primary);
    }

    /// The member's bound: how long a read waits for its `read_after`.
    pub fn set_bound(&self, bound: Duration) {
        self.bound_ms
            .store(bound.as_millis() as u64, Ordering::Relaxed);
    }

    pub fn bound(&self) -> Duration {
        Duration::from_millis(self.bound_ms.load(Ordering::Relaxed))
    }

    /// This process committed `partition` of `domain` to `sequence`.
    pub fn committed(&self, domain: &str, partition: &str, sequence: u64) {
        let mut committed = self.committed.lock().expect("committed poisoned");
        raise(
            committed.entry(domain.to_string()).or_default(),
            [(partition.to_string(), sequence)],
        );
    }

    /// What this process committed of `domain`, and the heads its record
    /// was last seen at, forgotten: its database moved to a new timeline,
    /// which may have lost some of it (contract v30, stage C), so the next
    /// read looks at the record.
    pub fn forget(&self, domain: &str) {
        self.committed
            .lock()
            .expect("committed poisoned")
            .remove(domain);
        self.looked.lock().expect("looked poisoned").remove(domain);
    }

    pub fn set_session(&self, session: Arc<dyn Session>) {
        *self.session.write().expect("session poisoned") = Some(session);
    }

    pub fn session(&self) -> Option<Arc<dyn Session>> {
        self.session.read().expect("session poisoned").clone()
    }

    /// Hold a read of `topic` naming `asked` until the member's record has
    /// reached it: at once where it has, waiting where not -- up to the
    /// bound or the call's remaining deadline less [`MARGIN`], whichever is
    /// sooner -- and then answered from the primary by a reader, or refused
    /// UNAVAILABLE by a writer; refused for load where too many already wait.
    /// Never answered from behind it. A blocking wait, on the handler's
    /// thread.
    pub fn hold(&self, topic: &str, asked: &Watermark) -> Result<Held, String> {
        if asked.partitions.is_empty() {
            return Ok(Held::Here);
        }
        let Some(domain) = domain_of(topic) else {
            return Ok(Held::Here);
        };
        if self.reached_by_own_commits(domain, asked) || self.reached_lately(domain, asked) {
            return Ok(Held::Here);
        }
        let position = self
            .positions
            .read()
            .expect("positions poisoned")
            .get(domain)
            .cloned();
        // A process that keeps no position of the store answers nothing
        // of it but what it serves itself, which is never a store's record.
        let Some(position) = position else {
            return Ok(Held::Here);
        };
        let partitions: Vec<String> = asked
            .partitions
            .iter()
            .map(|p| p.partition.clone())
            .collect();
        let look = |position: &Arc<dyn Position>| -> Result<Vec<(String, u64, u64)>, String> {
            let heads = position.heads(&partitions)?;
            self.saw_heads(domain, &heads);
            Ok(behind(asked, &heads))
        };
        let mut still = look(&position)?;
        if still.is_empty() {
            return Ok(Held::Here);
        }
        // Behind: wait the bound, or what the call's deadline leaves less
        // the margin, whichever is sooner (ruled 2026-11-10).
        let bound = self.bound();
        let wait = match deadline() {
            Some(deadline) => bound.min(
                deadline
                    .saturating_duration_since(Instant::now())
                    .saturating_sub(MARGIN),
            ),
            None => bound,
        };
        if !wait.is_zero() {
            let waiting = self.waiting.fetch_add(1, Ordering::AcqRel) + 1;
            let _left = Leave(&self.waiting);
            if waiting > WAITING_MOST {
                return Err(crate::backend::overloaded(&meridian_budget::Overloaded {
                    retry_after_seconds: bound.as_secs().clamp(1, 30) as u32,
                    words: format!(
                        "overloaded: {topic} waits behind its read_after, and {WAITING_MOST} reads already wait"
                    ),
                }));
            }
            let started = Instant::now();
            let mut every = Duration::from_millis(5);
            loop {
                let left = wait.saturating_sub(started.elapsed());
                if left.is_zero() {
                    break;
                }
                std::thread::sleep(every.min(left));
                every = (every * 2).min(LOOK_MOST);
                still = look(&position)?;
                if still.is_empty() {
                    return Ok(Held::Here);
                }
            }
        }
        self.past_the_wait(topic, domain, asked, &partitions, &still, wait)
    }

    /// The wait ended with the member still behind: a reader's read answered
    /// from the store's primary database where it has reached the watermark;
    /// otherwise refused UNAVAILABLE, with no reason code.
    fn past_the_wait(
        &self,
        topic: &str,
        domain: &str,
        asked: &Watermark,
        partitions: &[String],
        still: &[(String, u64, u64)],
        waited: Duration,
    ) -> Result<Held, String> {
        let (partition, wanted, at) = still[0].clone();
        let primary = self
            .primaries
            .read()
            .expect("primaries poisoned")
            .get(domain)
            .cloned();
        if let Some(primary) = primary {
            let heads = primary.heads(partitions)?;
            if behind(asked, &heads).is_empty() {
                tracing::info!(
                    topic,
                    partition,
                    wanted,
                    at,
                    waited_ms = waited.as_millis() as u64,
                    "a reader behind its read_after past its wait: answered from the primary"
                );
                return Ok(Held::FromPrimary);
            }
        }
        tracing::info!(
            topic,
            partition,
            wanted,
            at,
            waited_ms = waited.as_millis() as u64,
            "a read refused: the store has not reached its read_after"
        );
        Err(crate::backend::refusal_with_status(
            crate::backend::RefusalStatus::Unavailable,
            &[],
            format!(
                "behind read_after: partition {partition} is at {at} here, the read names {wanted}; \
                 not answered from behind it (waited {} ms); ask again",
                waited.as_millis()
            ),
        ))
    }

    /// Answer `handler`'s read of `topic` held to `asked`: what [`Bus::serve`]
    /// wraps each store's read in.
    ///
    /// [`Bus::serve`]: crate::Bus::serve
    pub fn answer<R>(
        &self,
        topic: &str,
        asked: &Watermark,
        handler: impl FnOnce() -> Result<R, String>,
    ) -> Result<R, String> {
        let held = self.hold(topic, asked)?;
        answering(asked, held == Held::FromPrimary, handler)
    }

    fn saw_heads(&self, domain: &str, heads: &HashMap<String, u64>) {
        let now = Instant::now();
        let mut looked = self.looked.lock().expect("looked poisoned");
        let held = looked.entry(domain.to_string()).or_default();
        for (partition, head) in heads {
            held.insert(partition.clone(), (*head, now));
        }
    }

    fn reached_lately(&self, domain: &str, asked: &Watermark) -> bool {
        let looked = self.looked.lock().expect("looked poisoned");
        let Some(held) = looked.get(domain) else {
            return false;
        };
        asked.partitions.iter().all(|wanted| {
            held.get(&wanted.partition)
                .is_some_and(|(head, at)| *head >= wanted.sequence && at.elapsed() < LOOKED_FOR)
        })
    }

    fn reached_by_own_commits(&self, domain: &str, asked: &Watermark) -> bool {
        let committed = self.committed.lock().expect("committed poisoned");
        committed
            .get(domain)
            .is_some_and(|heads| behind(asked, heads).is_empty())
    }
}

struct Leave<'a>(&'a AtomicUsize);

impl Drop for Leave<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Lagging(Mutex<HashMap<String, u64>>);

    impl Position for Lagging {
        fn heads(&self, _: &[String]) -> Result<HashMap<String, u64>, String> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    fn mark(partition: &str, sequence: u64) -> Watermark {
        Watermark {
            partitions: vec![PartitionSequence {
                partition: partition.into(),
                sequence,
            }],
        }
    }

    #[test]
    fn a_topic_names_its_store_and_whether_it_reads() {
        assert_eq!(domain_of("platform.lake.query.list-quotes"), Some("lake"));
        assert_eq!(domain_of("plugin.x"), None);
        assert!(is_read("platform.book.query.list-positions"));
        assert!(!is_read("platform.book.command.record-entry"));
    }

    #[test]
    fn a_session_only_moves_forward() {
        let mut at = HashMap::new();
        raise(&mut at, [("P0".to_string(), 7), ("P1".to_string(), 2)]);
        raise(
            &mut at,
            [
                ("P0".to_string(), 3),
                ("".to_string(), 9),
                ("P2".to_string(), 0),
            ],
        );
        assert_eq!(
            watermark_of(&at),
            Watermark {
                partitions: vec![
                    PartitionSequence {
                        partition: "P0".into(),
                        sequence: 7
                    },
                    PartitionSequence {
                        partition: "P1".into(),
                        sequence: 2
                    },
                ]
            }
        );
    }

    #[test]
    fn a_member_at_or_past_read_after_answers_at_once() {
        let reads = Reads::default();
        reads.keep(
            "lake",
            Arc::new(Lagging(Mutex::new([("fx".to_string(), 9)].into()))),
        );
        let started = Instant::now();
        assert!(reads
            .hold("platform.lake.query.list-quotes", &mark("fx", 9))
            .is_ok());
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn a_member_behind_waits_until_it_has_caught_up() {
        let reads = Arc::new(Reads::default());
        let lagging = Arc::new(Lagging(Mutex::new([("fx".to_string(), 3)].into())));
        reads.keep("lake", lagging.clone());
        reads.set_bound(Duration::from_secs(5));
        let catching = Arc::clone(&lagging);
        let caught = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            catching.0.lock().unwrap().insert("fx".into(), 5);
        });
        let started = Instant::now();
        assert!(reads
            .hold("platform.lake.query.list-quotes", &mark("fx", 5))
            .is_ok());
        let took = started.elapsed();
        assert!(
            took >= Duration::from_millis(150),
            "answered behind, after {took:?}"
        );
        assert!(took < Duration::from_secs(5));
        caught.join().unwrap();
    }

    #[test]
    fn a_member_behind_past_its_bound_refuses_and_never_answers() {
        let reads = Reads::default();
        reads.keep(
            "book",
            Arc::new(Lagging(Mutex::new([("P0".to_string(), 3)].into()))),
        );
        reads.set_bound(Duration::from_millis(200));
        let started = Instant::now();
        let refused = reads
            .hold("platform.book.query.list-positions", &mark("P0", 4))
            .unwrap_err();
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert!(refused.contains("behind read_after"), "{refused}");
        // A writer behind its read_after refuses UNAVAILABLE, no reason code
        // (ruled 2026-11-10).
        assert_eq!(
            crate::backend::read_refusal_status(&refused).map(|(status, _, _)| status),
            Some(crate::backend::RefusalStatus::Unavailable)
        );
        assert_eq!(crate::backend::read_refusal(&refused), None);
        // A partition the member does not hold is behind any sequence.
        let refused = reads
            .hold("platform.book.query.list-positions", &mark("P9", 1))
            .unwrap_err();
        assert!(refused.contains("P9"), "{refused}");
    }

    #[test]
    fn its_own_commits_answer_with_no_look_at_the_record() {
        struct Never;
        impl Position for Never {
            fn heads(&self, _: &[String]) -> Result<HashMap<String, u64>, String> {
                Err("looked".into())
            }
        }
        let reads = Reads::default();
        reads.keep("lake", Arc::new(Never));
        reads.committed("lake", "fx", 12);
        assert!(reads
            .hold("platform.lake.query.list-quotes", &mark("fx", 12))
            .is_ok());
        assert_eq!(
            reads.hold("platform.lake.query.list-quotes", &mark("fx", 13)),
            Err("looked".to_string())
        );
    }

    #[test]
    fn reads_past_the_most_waiting_are_refused_for_load_with_a_retry_after() {
        let reads = Arc::new(Reads::default());
        reads.keep("lake", Arc::new(Lagging(Mutex::new(HashMap::new()))));
        reads.set_bound(Duration::from_millis(600));
        let waiting: Vec<_> = (0..WAITING_MOST)
            .map(|_| {
                let reads = Arc::clone(&reads);
                std::thread::spawn(move || {
                    reads.hold("platform.lake.query.list-quotes", &mark("fx", 1))
                })
            })
            .collect();
        std::thread::sleep(Duration::from_millis(150));
        let refused = reads
            .hold("platform.lake.query.list-quotes", &mark("fx", 1))
            .unwrap_err();
        assert_eq!(
            crate::backend::read_refusal(&refused).map(|(reason, _)| reason),
            Some(meridian_pb::v1::RefusalReason::Overloaded as i32)
        );
        assert!(crate::backend::refusal_retry_after(&refused).unwrap_or(0) >= 1);
        for one in waiting {
            assert!(
                one.join().unwrap().is_err(),
                "nothing caught up: each refused"
            );
        }
        assert_eq!(reads.waiting.load(Ordering::Acquire), 0);
    }

    #[test]
    fn a_reader_behind_past_its_wait_answers_from_the_primary() {
        let reads = Reads::default();
        reads.keep(
            "lake",
            Arc::new(Lagging(Mutex::new([("fx".to_string(), 3)].into()))),
        );
        reads.keep_primary(
            "lake",
            Arc::new(Lagging(Mutex::new([("fx".to_string(), 9)].into()))),
        );
        reads.set_bound(Duration::from_millis(150));
        let started = Instant::now();
        let from_primary = reads
            .answer("platform.lake.query.list-quotes", &mark("fx", 7), || {
                Ok((answering_from_primary(), answering_after()))
            })
            .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(150));
        assert_eq!(from_primary, (true, Some(mark("fx", 7))));
        // Told only while it answers.
        assert!(!answering_from_primary());
        // A read it has reached is its own, from memory.
        let here = reads
            .answer("platform.lake.query.list-quotes", &mark("fx", 3), || {
                Ok(answering_from_primary())
            })
            .unwrap();
        assert!(!here);
    }

    #[test]
    fn a_reader_whose_primary_is_behind_too_refuses_unavailable() {
        let reads = Reads::default();
        reads.keep("lake", Arc::new(Lagging(Mutex::new(HashMap::new()))));
        reads.keep_primary(
            "lake",
            Arc::new(Lagging(Mutex::new([("fx".to_string(), 4)].into()))),
        );
        reads.set_bound(Duration::from_millis(50));
        let refused = reads
            .answer("platform.lake.query.list-quotes", &mark("fx", 5), || {
                Ok::<_, String>(())
            })
            .unwrap_err();
        assert_eq!(
            crate::backend::read_refusal_status(&refused).map(|(status, _, _)| status),
            Some(crate::backend::RefusalStatus::Unavailable)
        );
    }

    #[test]
    fn the_wait_ends_at_the_calls_deadline_less_the_margin_when_that_is_sooner() {
        let reads = Reads::default();
        reads.keep(
            "book",
            Arc::new(Lagging(Mutex::new([("P0".to_string(), 1)].into()))),
        );
        reads.set_bound(Duration::from_secs(5));
        // The caller waits 1.3 s in all: the read waits 0.3 s, not 5.
        let deadline = Instant::now() + MARGIN + Duration::from_millis(300);
        let started = Instant::now();
        let refused = within_deadline(Some(deadline), || {
            reads.hold("platform.book.query.list-positions", &mark("P0", 2))
        });
        let took = started.elapsed();
        assert!(refused.is_err());
        assert!(
            took >= Duration::from_millis(250) && took < Duration::from_millis(900),
            "{took:?}"
        );
        // A deadline already inside the margin waits not at all.
        let started = Instant::now();
        let refused = within_deadline(Some(Instant::now() + MARGIN / 2), || {
            reads.hold("platform.book.query.list-positions", &mark("P0", 2))
        });
        assert!(refused.is_err());
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(deadline_now_unset(), None);
    }

    fn deadline_now_unset() -> Option<Instant> {
        deadline()
    }

    #[test]
    fn a_head_seen_answers_for_a_second_and_a_whole_position_is_narrowed() {
        let looks = Arc::new(AtomicUsize::new(0));
        let counting = Arc::clone(&looks);
        let reads = Reads::default();
        reads.keep(
            "lake",
            Arc::new(AllHeads(move || {
                counting.fetch_add(1, Ordering::Relaxed);
                Ok([("fx".to_string(), 9), ("rates".to_string(), 4)].into())
            })),
        );
        for _ in 0..5 {
            assert_eq!(
                reads.hold("platform.lake.query.list-quotes", &mark("fx", 8)),
                Ok(Held::Here)
            );
        }
        assert_eq!(looks.load(Ordering::Relaxed), 1, "one look a second");
        // Past what was seen: looked at again.
        reads.set_bound(Duration::from_millis(20));
        assert!(reads
            .hold("platform.lake.query.list-quotes", &mark("fx", 10))
            .is_err());
        assert!(looks.load(Ordering::Relaxed) > 1);
        let narrowed = AllHeads(|| Ok([("fx".to_string(), 9)].into()))
            .heads(&["fx".to_string(), "absent".to_string()])
            .unwrap();
        assert_eq!(narrowed, [("fx".to_string(), 9)].into());
    }

    #[test]
    fn a_read_naming_nothing_or_a_store_kept_by_nobody_is_answered() {
        let reads = Reads::default();
        assert!(reads
            .hold("platform.lake.query.list-quotes", &Watermark::default())
            .is_ok());
        assert!(reads
            .hold("platform.lake.query.list-quotes", &mark("fx", 1))
            .is_ok());
    }
}
