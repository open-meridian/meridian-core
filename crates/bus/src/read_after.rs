//! `MessageMeta.read_after` (contract v30, step 3c stage B; W4.4;
//! spec/a-deployment-scales-out-and-fails-over-by-configuration section 5;
//! approved 2026-11-09).
//!
//! **Stamped by the sidecar, never by the plugin.** A plugin's sidecar keeps
//! a [`Session`] on its bus: the highest sequence it has seen on each of a
//! store's partitions, from the plugin's acknowledged writes and the
//! deliveries it passed on. Every read it asks of that store names them as
//! `read_after`, so read-your-writes and monotonic reads hold for the
//! instance with no plugin-facing field: session consistency.
//!
//! **Never answered from behind it.** A member serving a store's reads keeps
//! its [`Position`] on its bus. A read naming a watermark the member's record
//! has not reached waits for it, up to the member's bound (its
//! `<store>.readyLagMs`), and is refused rather than answered from behind it
//! where the bound passes. The waits are bounded in number too: a read past
//! [`WAITING_MOST`] waiting at once is refused for load, with its
//! retry-after, as every component's overload is (section 10a). A read naming
//! nothing is answered at once, eventually consistent, its reply's watermark
//! saying how current it is.

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

/// How often a waiting read looks at its member's position again: at first
/// soon, then less often, never less often than this.
const LOOK_MOST: Duration = Duration::from_millis(100);

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

/// A bus's part in `read_after`: the positions of the stores it serves the
/// reads of, the bound they wait up to, and the sidecar's session where it
/// is one.
pub struct Reads {
    positions: RwLock<HashMap<String, Arc<dyn Position>>>,
    /// What this process has itself committed, per store and partition: a
    /// read naming no more than these is answered with no look at the record.
    committed: Mutex<HashMap<String, HashMap<String, u64>>>,
    bound_ms: AtomicU64,
    waiting: AtomicUsize,
    session: RwLock<Option<Arc<dyn Session>>>,
}

impl Default for Reads {
    fn default() -> Self {
        Self {
            positions: RwLock::new(HashMap::new()),
            committed: Mutex::new(HashMap::new()),
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

    pub fn set_session(&self, session: Arc<dyn Session>) {
        *self.session.write().expect("session poisoned") = Some(session);
    }

    pub fn session(&self) -> Option<Arc<dyn Session>> {
        self.session.read().expect("session poisoned").clone()
    }

    /// Hold a read of `topic` naming `asked` until the member's record has
    /// reached it: at once where it has, waiting up to the bound where not,
    /// and refused where the bound passes or too many already wait. Never
    /// answered from behind it. A blocking wait, on the handler's thread.
    pub fn hold(&self, topic: &str, asked: &Watermark) -> Result<(), String> {
        if asked.partitions.is_empty() {
            return Ok(());
        }
        let Some(domain) = domain_of(topic) else {
            return Ok(());
        };
        if self.reached_by_own_commits(domain, asked) {
            return Ok(());
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
            return Ok(());
        };
        let partitions: Vec<String> = asked
            .partitions
            .iter()
            .map(|p| p.partition.clone())
            .collect();
        let look = |position: &Arc<dyn Position>| -> Result<Vec<(String, u64, u64)>, String> {
            let heads = position.heads(&partitions)?;
            Ok(behind(asked, &heads))
        };
        let mut still = look(&position)?;
        if still.is_empty() {
            return Ok(());
        }
        // Behind: wait, if there is room to.
        let waiting = self.waiting.fetch_add(1, Ordering::AcqRel) + 1;
        let _left = Leave(&self.waiting);
        let bound = self.bound();
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
            let left = bound.saturating_sub(started.elapsed());
            if left.is_zero() {
                let (partition, wanted, at) = still[0].clone();
                tracing::info!(
                    topic,
                    partition,
                    wanted,
                    at,
                    bound_ms = bound.as_millis() as u64,
                    "a read refused: this member is behind its read_after past its bound"
                );
                return Err(format!(
                    "behind read_after: partition {partition} is at {at} here, the read names {wanted}; \
                     not answered from behind it (waited {} ms)",
                    bound.as_millis()
                ));
            }
            std::thread::sleep(every.min(left));
            every = (every * 2).min(LOOK_MOST);
            still = look(&position)?;
            if still.is_empty() {
                return Ok(());
            }
        }
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
