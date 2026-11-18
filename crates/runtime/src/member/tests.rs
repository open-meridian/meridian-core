use std::sync::atomic::AtomicU64;

use meridian_changes::{Change, ChangeLog, Page, ReadChangesReply, ReadError};

use super::*;

/// A writer's log of one partition, `P0`, whose head the test moves.
struct Log {
    head: AtomicU64,
}

fn change(sequence: u64) -> Change {
    Change {
        partition: "P0".into(),
        sequence,
        epoch: 2,
        origin: "DEP-1".into(),
        committed_at_ns: sequence as i64,
        kind: "entry".into(),
        body: vec![1; 16],
        digest: Vec::new(),
    }
}

impl ChangeLog for Log {
    fn read_changes(
        &self,
        partition: &str,
        after: u64,
        most: usize,
    ) -> Result<ReadChangesReply, ReadError> {
        let head = self.head.load(Ordering::SeqCst);
        if partition == meridian_changes::CONTROL {
            return Ok(ReadChangesReply {
                head: 0,
                epoch: 2,
                ..Default::default()
            });
        }
        let mut page = Page::new(most);
        for sequence in (after + 1)..=head {
            if !page.take(change(sequence)) {
                break;
            }
        }
        Ok(ReadChangesReply {
            changes: page.changes,
            head,
            lowest_kept: 1,
            epoch: 2,
            after_sequence: 0,
        })
    }
}

async fn until(what: &str, mut held: impl FnMut() -> bool) {
    for _ in 0..100 {
        if held() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{what} never held");
}

/// A standby follows its writer: its asking is the interest that has the
/// writer publish its tail (ruling 1062), each page following the last is
/// applied, and a page missed is read from the writer rather than inferred.
#[tokio::test(flavor = "multi_thread")]
async fn a_standby_follows_its_writers_tail_and_reads_a_page_it_missed() {
    let bus = Arc::new(Bus::single(
        "member-test-bor",
        Arc::new(meridian_bus::MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let log = Arc::new(Log {
        head: AtomicU64::new(0),
    });
    meridian_changes::serve(&bus, "book", log.clone());
    // Nobody follows yet: the writer publishes nothing.
    assert!(!meridian_changes::followed(&bus, "book"));
    let withheld = meridian_changes::TAIL_WITHHELD.load(Ordering::SeqCst);
    meridian_changes::publish(&bus, "book", 0, &[change(1)]);
    assert!(meridian_changes::TAIL_WITHHELD.load(Ordering::SeqCst) > withheld);

    let following = tokio::spawn(follow(
        Arc::clone(&bus),
        "book",
        Duration::from_secs(5),
        None,
    ));
    until("the standby's interest", || {
        meridian_changes::followed(&bus, "book")
    })
    .await;

    // Pages that follow on, one a transaction.
    log.head.store(3, Ordering::SeqCst);
    meridian_changes::publish(&bus, "book", 1, &[change(2)]);
    until("the first page applied", || {
        said().partitions() == vec!["P0".to_string()]
    })
    .await;
    meridian_changes::publish(&bus, "book", 2, &[change(3)]);
    // Two pages missed (4 and 5), then 6: the standby reads 4 to 6 from the writer.
    log.head.store(6, Ordering::SeqCst);
    meridian_changes::publish(&bus, "book", 5, &[change(6)]);
    until("caught up after the gap", || said().lag_ms() == 0).await;
    following.abort();
}

/// A page missed whose catch-up found the writer's head below what this
/// member applied (a member answering behind, a moment after a takeover)
/// hands the partition to the read-back, and the read-back finding the
/// record holds all it applied leaves it caught up: the gap's own wait is
/// let go, never left to grow the member's lag for good (failover-repeat
/// 43146764089, ha run 1: the lake's reader unready after the apiserver
/// class, no warning said).
#[tokio::test(flavor = "multi_thread")]
async fn a_catch_up_that_found_the_head_below_and_a_read_back_that_found_it_whole_leave_no_lag() {
    let bus = Arc::new(Bus::single(
        "member-test-diverged-kept",
        Arc::new(meridian_bus::MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let log = Arc::new(Log {
        head: AtomicU64::new(0),
    });
    meridian_changes::serve(&bus, "book", log.clone());
    let following = tokio::spawn(follow(
        Arc::clone(&bus),
        "book",
        Duration::from_secs(1),
        None,
    ));
    until("the standby's interest", || {
        meridian_changes::followed(&bus, "book")
    })
    .await;
    log.head.store(3, Ordering::SeqCst);
    meridian_changes::publish(&bus, "book", 2, &[change(3)]);
    until("3 held", || said().partitions() == vec!["P0".to_string()]).await;
    // A page after a gap, read from a log whose head is below what was
    // applied: the catch-up finds it diverged.
    log.head.store(2, Ordering::SeqCst);
    meridian_changes::publish(&bus, "book", 5, &[change(6)]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    // The record holds it all after all: the read-back finds it kept, and
    // the member is not left behind.
    log.head.store(6, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(3)).await;
    until("no lag after the read-back", || said().lag_ms() < 1_000).await;
    following.abort();
}

/// What a reader applied, in order, and what it reset.
#[derive(Default)]
struct Applied {
    changes: Mutex<Vec<u64>>,
    resets: Mutex<Vec<String>>,
}

impl Apply for Applied {
    fn apply(&self, _: &str, changes: &[Change]) {
        self.changes
            .lock()
            .unwrap()
            .extend(changes.iter().map(|c| c.sequence));
    }
    fn reset(&self, partition: &str) {
        self.resets.lock().unwrap().push(partition.to_string());
    }
}

/// A reader follows from the heads it started at, applying each change once
/// and in order, a gap read from the writer; its position is what it has
/// applied, and a partition first heard is reset and held from its head.
#[tokio::test(flavor = "multi_thread")]
async fn a_reader_applies_each_change_in_order_from_the_heads_it_started_at() {
    let bus = Arc::new(Bus::single(
        "member-test-reader",
        Arc::new(meridian_bus::MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let log = Arc::new(Log {
        head: AtomicU64::new(2),
    });
    meridian_changes::serve(&bus, "book", log.clone());
    let applied = Arc::new(Applied::default());
    struct Primary;
    impl meridian_bus::read_after::Position for Primary {
        fn heads(&self, _: &[String]) -> Result<HashMap<String, u64>, String> {
            Ok([("P0".to_string(), 99)].into())
        }
    }
    let member = Member::reader_of("bor", "book", Duration::from_secs(5));
    assert!(member.is_reader());
    let followed = crate::member::Followed::from_heads([("P0".to_string(), 2)]);
    member
        .read(&bus, followed.clone(), applied.clone(), Arc::new(Primary))
        .unwrap();
    until("the reader's interest", || {
        meridian_changes::followed(&bus, "book")
    })
    .await;
    log.head.store(3, Ordering::SeqCst);
    meridian_changes::publish(&bus, "book", 2, &[change(3)]);
    until("3 applied", || followed.heads().get("P0") == Some(&3)).await;
    // 4 and 5 missed, then 6: read from the writer, applied in order.
    log.head.store(6, Ordering::SeqCst);
    meridian_changes::publish(&bus, "book", 5, &[change(6)]);
    until("6 applied", || followed.heads().get("P0") == Some(&6)).await;
    assert_eq!(*applied.changes.lock().unwrap(), vec![3, 4, 5, 6]);
    // Its reads are held to what it applied.
    assert_eq!(
        bus.reads_after().hold(
            "platform.book.query.list-positions",
            &meridian_pb::v1::Watermark {
                partitions: vec![meridian_pb::v1::PartitionSequence {
                    partition: "P0".into(),
                    sequence: 6,
                }],
            },
        ),
        Ok(meridian_bus::read_after::Held::Here)
    );
    assert!(applied.resets.lock().unwrap().is_empty());
}

/// A reader started before its writer served, and so before the writer
/// published its tail: a partition written meanwhile is held from the
/// primary's head as soon as the writer answers, and one it held behind is
/// read from the writer -- never left unfollowed, every read naming it
/// waiting out its bound (CI 42868480584).
#[tokio::test(flavor = "multi_thread")]
async fn a_reader_holds_what_the_primary_holds_as_soon_as_its_writer_answers() {
    let bus = Arc::new(Bus::single(
        "member-test-reconcile",
        Arc::new(meridian_bus::MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    // Written before the reader followed, no page of it heard.
    let log = Arc::new(Log {
        head: AtomicU64::new(3),
    });
    meridian_changes::serve(&bus, "book", log.clone());
    let applied = Arc::new(Applied::default());
    let primary = log.clone();
    // Read as the stores' Postgres clients read: blocking on a runtime of
    // their own, which panics where an async task drives the thread.
    let heads = meridian_bus::read_after::AllHeads(move || {
        let head = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async { primary.head.load(Ordering::SeqCst) });
        Ok(std::collections::BTreeMap::from([("P0".to_string(), head)]))
    });
    let member = Member::reader_of("bor", "book", Duration::from_secs(5));
    // Started knowing nothing of P0.
    let followed = crate::member::Followed::from_heads([]);
    member
        .read(&bus, followed.clone(), applied.clone(), Arc::new(heads))
        .unwrap();
    until("P0 held from the primary's head", || {
        followed.heads().get("P0") == Some(&3)
    })
    .await;
    assert_eq!(*applied.resets.lock().unwrap(), vec!["P0".to_string()]);
    assert!(applied.changes.lock().unwrap().is_empty());
}

/// The instrument store's reader, started before its writer served, catches
/// up as the lake's and the book's do: its primary's position, read from the
/// change log partition by partition, lists every partition the log keeps
/// (`LogHeads::listing`), so the reader holds each from the primary's head
/// as soon as its writer answers -- never left unfollowed, every read naming
/// it waiting out its bound and answered from the primary. A replica's
/// position lists none.
#[tokio::test(flavor = "multi_thread")]
async fn an_instrument_reader_started_before_its_writer_served_catches_up() {
    use meridian_bus::read_after::Position as _;

    let bus = Arc::new(Bus::single(
        "member-test-instrument",
        Arc::new(meridian_bus::MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    // Written before the reader followed, no page of it heard.
    let log = Arc::new(Log {
        head: AtomicU64::new(3),
    });
    assert!(LogHeads::of(log.clone()).all_heads().is_none());
    meridian_changes::serve(&bus, "reference", log.clone());
    let applied = Arc::new(Applied::default());
    let primary = LogHeads::listing(log.clone(), &["P0", meridian_changes::CONTROL]);
    let member = Member::reader_of("instrument", "reference", Duration::from_secs(5));
    // Started knowing nothing, as the instrument reader starts.
    let followed = crate::member::Followed::from_heads([]);
    member
        .read(&bus, followed.clone(), applied.clone(), Arc::new(primary))
        .unwrap();
    until("P0 held from the primary's head", || {
        followed.heads().get("P0") == Some(&3)
    })
    .await;
    assert_eq!(
        followed.heads().get(meridian_changes::CONTROL),
        Some(&0),
        "every partition its log keeps"
    );
    // A read naming the head is its own to answer, at once.
    assert_eq!(
        bus.reads_after().hold(
            "platform.reference.query.resolve-instrument",
            &meridian_pb::v1::Watermark {
                partitions: vec![meridian_pb::v1::PartitionSequence {
                    partition: "P0".into(),
                    sequence: 3,
                }],
            },
        ),
        Ok(meridian_bus::read_after::Held::Here)
    );
    // And what it holds moves on with the writer's tail from there.
    log.head.store(4, Ordering::SeqCst);
    meridian_changes::publish(&bus, "reference", 3, &[change(4)]);
    until("4 applied", || followed.heads().get("P0") == Some(&4)).await;
    assert_eq!(*applied.changes.lock().unwrap(), vec![4]);
}

/// A writer's log of `P0` that, while `down`, answers nobody -- its control
/// partition included -- as a writer does while its database fails over.
struct Down {
    head: AtomicU64,
    down: AtomicBool,
}

impl ChangeLog for Down {
    fn read_changes(
        &self,
        partition: &str,
        after: u64,
        most: usize,
    ) -> Result<ReadChangesReply, ReadError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(ReadError::Unavailable(
                "the database is failing over".into(),
            ));
        }
        Log {
            head: AtomicU64::new(self.head.load(Ordering::SeqCst)),
        }
        .read_changes(partition, after, most)
    }
}

/// A page missed while the writer could not be read leaves a gap; once the
/// writer answers again and the reader's reconciling with the primary reads
/// what it missed, the partition is caught up and follows on, and the gap is
/// let go -- never left to grow the reader's lag for good (CI 44187810396,
/// the ha failover run: the lake's reader, a page missed in the db-primary
/// class unread, then caught up by the reconcile, stayed unready until the
/// apiserver class waited out every member ready).
#[tokio::test(flavor = "multi_thread")]
async fn a_gap_caught_up_by_the_reconcile_leaves_no_lag() {
    let bus = Arc::new(Bus::single(
        "member-test-gap-reconciled",
        Arc::new(meridian_bus::MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let log = Arc::new(Down {
        head: AtomicU64::new(2),
        down: AtomicBool::new(false),
    });
    meridian_changes::serve(&bus, "book", log.clone());
    let applied = Arc::new(Applied::default());
    let primary = log.clone();
    let heads = meridian_bus::read_after::AllHeads(move || {
        Ok(std::collections::BTreeMap::from([(
            "P0".to_string(),
            primary.head.load(Ordering::SeqCst),
        )]))
    });
    let member = Member::reader_of("bor", "book", Duration::from_secs(1));
    let followed = crate::member::Followed::from_heads([("P0".to_string(), 2)]);
    member
        .read(&bus, followed.clone(), applied.clone(), Arc::new(heads))
        .unwrap();
    until("the reader's interest", || {
        meridian_changes::followed(&bus, "book")
    })
    .await;
    log.head.store(3, Ordering::SeqCst);
    meridian_changes::publish(&bus, "book", 2, &[change(3)]);
    until("3 applied", || followed.heads().get("P0") == Some(&3)).await;

    // The writer cannot be read: 4 and 5 missed, 6 heard, its gap unread,
    // and the writer's asks unanswered.
    log.down.store(true, Ordering::SeqCst);
    log.head.store(6, Ordering::SeqCst);
    meridian_changes::publish(&bus, "book", 5, &[change(6)]);
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(followed.heads().get("P0"), Some(&3), "the gap not yet read");

    // It answers again: the reconcile reads 4 to 6, and 7 follows on.
    log.down.store(false, Ordering::SeqCst);
    until("6 read by the reconcile", || {
        followed.heads().get("P0") == Some(&6)
    })
    .await;
    log.head.store(7, Ordering::SeqCst);
    meridian_changes::publish(&bus, "book", 6, &[change(7)]);
    until("7 applied", || followed.heads().get("P0") == Some(&7)).await;
    assert_eq!(*applied.changes.lock().unwrap(), vec![3, 4, 5, 6, 7]);
    // Caught up: its lag is gone, and it is ready again.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    until("no lag once caught up", || said().lag_ms() < 1_000).await;
}

/// A writer's record the test shapes, partition by partition, as a database
/// failover and a loaded writer shape it: each partition's head; the epoch
/// now and the sequence from which each change was written again under it
/// (a tail the failover lost, taken again); the epoch its `control`
/// answers with; and the catch-ups it refuses -- the reads of a whole page
/// (`page_size` 0, which a read-back's and an ask's never are), as a
/// writer's read times out under load -- while it answers everything else.
/// Its changes are chained, as the book's journal is, or each digested alone,
/// as the lake's rows are; and a catch-up reads one to a page.
struct Record {
    shape: Mutex<Shape>,
}

#[derive(Default)]
struct Shape {
    heads: HashMap<String, u64>,
    epoch: u64,
    lost_from: u64,
    control_epoch: u64,
    /// Catch-ups answered (true) or refused (false) in turn, before
    /// `refusing` says.
    script: std::collections::VecDeque<bool>,
    /// The partitions whose catch-ups are refused.
    refusing: std::collections::HashSet<String>,
    /// The partitions whose catch-ups are answered too late, past the
    /// follower's wait, as a writer's reads are under load.
    hang: std::collections::HashSet<String>,
    /// The partitions a catch-up of was refused.
    refused: std::collections::HashSet<String>,
    /// Each catch-up asked: its partition, and the sequence it read after.
    asked: Vec<(String, u64)>,
    /// Read-backs refused: every read of one change, as a writer's reads
    /// time out under load, while its catch-ups and `control` answer.
    refuse_read_backs: bool,
    /// The lowest sequence the record keeps, where it is past 1: a read
    /// after a sequence below it is behind the writer's retention.
    lowest_kept: u64,
    /// Nothing answered, `control` included: the database failing over.
    down: bool,
    /// The lake's record: each change's digest its body's alone.
    lake: bool,
}

impl Record {
    fn of(heads: &[(&str, u64)]) -> Arc<Self> {
        Arc::new(Self {
            shape: Mutex::new(Shape {
                heads: heads.iter().map(|(p, h)| (p.to_string(), *h)).collect(),
                epoch: 2,
                lost_from: u64::MAX,
                control_epoch: 2,
                ..Shape::default()
            }),
        })
    }

    /// The lake's record, each change's digest its body's alone.
    fn lake_of(heads: &[(&str, u64)]) -> Arc<Self> {
        let record = Self::of(heads);
        record.shape(|shape| shape.lake = true);
        record
    }

    fn domain(&self) -> &'static str {
        if self.shape.lock().unwrap().lake {
            "lake"
        } else {
            "book"
        }
    }

    fn shape(&self, change: impl FnOnce(&mut Shape)) {
        change(&mut self.shape.lock().unwrap());
    }

    fn head(&self, partition: &str, head: u64) {
        self.shape(|shape| {
            shape.heads.insert(partition.to_string(), head);
        });
    }

    fn refuse(&self, partition: &str, refused: bool) {
        self.shape(|shape| {
            if refused {
                shape.refusing.insert(partition.to_string());
            } else {
                shape.refusing.remove(partition);
            }
        });
    }

    /// The database lost `partition`'s changes from `from` on and the writer
    /// took over at `epoch`: its head back below them.
    fn lose(&self, partition: &str, from: u64, epoch: u64) {
        self.shape(|shape| {
            shape.heads.insert(partition.to_string(), from - 1);
            shape.lost_from = from;
            shape.epoch = epoch;
        });
    }

    fn change(&self, partition: &str, sequence: u64) -> Change {
        let (lost_from, epoch, lake) = {
            let shape = self.shape.lock().unwrap();
            (shape.lost_from, shape.epoch, shape.lake)
        };
        let body = |sequence: u64| vec![if sequence >= lost_from { 2 } else { 1 }; 16];
        let mut digest = Vec::new();
        if lake {
            digest = meridian_changes::integrity::digest(&body(sequence));
        } else {
            for at in 1..=sequence {
                digest = meridian_changes::integrity::chained(&digest, &body(at));
            }
        }
        Change {
            partition: partition.into(),
            sequence,
            epoch: if sequence >= lost_from { epoch } else { 2 },
            origin: "DEP-1".into(),
            committed_at_ns: sequence as i64,
            kind: "entry".into(),
            body: body(sequence),
            digest,
        }
    }

    fn primary(self: &Arc<Self>) -> Arc<dyn meridian_bus::read_after::Position> {
        let record = Arc::clone(self);
        Arc::new(meridian_bus::read_after::AllHeads(move || {
            let shape = record.shape.lock().unwrap();
            Ok(shape.heads.iter().map(|(p, h)| (p.clone(), *h)).collect())
        }))
    }
}

impl ChangeLog for Record {
    fn read_changes(
        &self,
        partition: &str,
        after: u64,
        most: usize,
    ) -> Result<ReadChangesReply, ReadError> {
        let (head, epoch) = {
            let mut shape = self.shape.lock().unwrap();
            if shape.down {
                return Err(ReadError::Unavailable(
                    "the database is failing over".into(),
                ));
            }
            if partition == meridian_changes::CONTROL {
                return Ok(ReadChangesReply {
                    head: 0,
                    epoch: shape.control_epoch,
                    ..Default::default()
                });
            }
            let catching_up = most == meridian_changes::PAGE_MOST;
            if catching_up {
                shape.asked.push((partition.to_string(), after));
            } else if shape.refuse_read_backs {
                return Err(ReadError::Unavailable("the read timed out".into()));
            }
            if shape.lowest_kept > 1 {
                meridian_changes::check_kept(
                    after,
                    shape.lowest_kept,
                    shape.heads.get(partition).copied().unwrap_or(0),
                )?;
            }
            if catching_up && shape.hang.contains(partition) {
                drop(shape);
                std::thread::sleep(Duration::from_secs(6));
                return Err(ReadError::Unavailable("the read timed out".into()));
            }
            if catching_up {
                let answered = match shape.script.pop_front() {
                    Some(answered) => answered,
                    None => !shape.refusing.contains(partition),
                };
                if !answered {
                    shape.refused.insert(partition.to_string());
                    return Err(ReadError::Unavailable("the read timed out".into()));
                }
            }
            (
                shape.heads.get(partition).copied().unwrap_or(0),
                shape.epoch,
            )
        };
        let changes = ((after + 1)..=head)
            .take(if most == meridian_changes::PAGE_MOST {
                1
            } else {
                most
            })
            .map(|sequence| self.change(partition, sequence))
            .collect();
        Ok(ReadChangesReply {
            changes,
            head,
            lowest_kept: 1,
            epoch,
            after_sequence: after,
        })
    }
}

/// A reader of `record` -- the book's, or the lake's -- started at `start`,
/// ready while its lag is under a second, once the writer has its interest
/// and has been asked.
async fn reading(
    name: &str,
    record: &Arc<Record>,
    start: &[(&str, u64)],
) -> (Arc<Bus>, Arc<Followed>, Arc<Applied>) {
    let bus = Arc::new(Bus::single(
        name,
        Arc::new(meridian_bus::MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let domain = record.domain();
    meridian_changes::serve(&bus, domain, record.clone());
    let applied = Arc::new(Applied::default());
    let component = if domain == "lake" { "lake" } else { "bor" };
    let member = Member::reader_of(component, domain, Duration::from_secs(1));
    let followed =
        crate::member::Followed::from_heads(start.iter().map(|(p, h)| (p.to_string(), *h)));
    member
        .read(&bus, followed.clone(), applied.clone(), record.primary())
        .unwrap();
    until("the reader's interest", || {
        meridian_changes::followed(&bus, domain)
    })
    .await;
    // Reconciled with its primary as its writer first answered.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    (bus, followed, applied)
}

/// Whether this process's member says it is ready.
fn ready() -> bool {
    SERVING.load(Ordering::SeqCst)
}

/// Neither ready nor moved from `head` of `partition`, sampled for `long`.
async fn held_unready(followed: &Followed, partition: &str, head: u64, long: Duration) {
    let until = Instant::now() + long;
    while Instant::now() < until {
        assert_eq!(
            followed.heads().get(partition),
            Some(&head),
            "{partition} moved"
        );
        assert!(
            !ready(),
            "ready at {} ms of lag, {partition} held at {head} with a gap unread",
            said().lag_ms()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A gap whose partition a read-back reloaded below where the gap was found
/// stays the partition's until a page heard past the reload is read: the
/// reload holds the record as it then stood, and the new writer's later
/// pages, missed and unread, leave the reader behind -- never ready while it
/// lacks them (review-c5c-ci1 H1).
#[tokio::test(flavor = "multi_thread")]
async fn a_gap_found_again_after_a_reload_below_it_keeps_the_reader_unready() {
    let record = Record::of(&[("P0", 3)]);
    let (bus, followed, applied) = reading("member-test-gap-reloaded", &record, &[("P0", 3)]).await;
    record.head("P0", 5);
    meridian_changes::publish(
        &bus,
        "book",
        3,
        &[record.change("P0", 4), record.change("P0", 5)],
    );
    until("5 applied", || followed.heads().get("P0") == Some(&5)).await;

    // 6 and 7 missed, 8 heard; its catch-up refused: the gap opens at 5.
    record.refuse("P0", true);
    record.head("P0", 8);
    meridian_changes::publish(&bus, "book", 7, &[record.change("P0", 8)]);
    tokio::time::sleep(Duration::from_millis(200)).await;
    // The database failed over and lost 4 to 8; the new writer writes 4
    // again, its page read back at once from the tail: the partition is
    // reloaded at the record's head, 4.
    record.lose("P0", 4, 3);
    record.head("P0", 4);
    meridian_changes::publish(&bus, "book", 3, &[record.change("P0", 4)]);
    until("P0 reloaded at 4", || {
        followed.heads().get("P0") == Some(&4)
    })
    .await;
    assert_eq!(*applied.resets.lock().unwrap(), vec!["P0".to_string()]);
    // It writes 5 to 7: 5 and 6 missed, 7 heard, its catch-up refused.
    record.head("P0", 7);
    meridian_changes::publish(&bus, "book", 6, &[record.change("P0", 7)]);
    // Behind by 5 to 7, unread: unready, past its bound and a tick.
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    held_unready(&followed, "P0", 4, Duration::from_secs(2)).await;
    // Read once the writer answers the catch-up.
    record.refuse("P0", false);
    record.head("P0", 8);
    meridian_changes::publish(&bus, "book", 7, &[record.change("P0", 8)]);
    until("8 read", || followed.heads().get("P0") == Some(&8)).await;
    until("ready once caught up", || {
        ready() && said().lag_ms() < 1_000
    })
    .await;
}

/// What the reconcile read and applied is what a later read-back checks: a
/// reader caught up by it, whose record then lost the changes it read,
/// drops them when the epoch rises -- never holding a change the record
/// does not, ready while diverged (review-c5c-ci1 M3).
#[tokio::test(flavor = "multi_thread")]
async fn a_reader_drops_what_the_reconcile_applied_once_the_record_lost_it() {
    let record = Record::of(&[("P0", 2)]);
    let (bus, followed, applied) =
        reading("member-test-reconciled-lost", &record, &[("P0", 2)]).await;
    record.head("P0", 3);
    meridian_changes::publish(&bus, "book", 2, &[record.change("P0", 3)]);
    until("3 applied", || followed.heads().get("P0") == Some(&3)).await;
    // The writer cannot be read: 4 and 5 missed, 6 heard, unread.
    record.shape(|shape| shape.down = true);
    record.head("P0", 6);
    meridian_changes::publish(&bus, "book", 5, &[record.change("P0", 6)]);
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    // It answers again: the reconcile reads 4 to 6.
    record.shape(|shape| shape.down = false);
    until("6 read by the reconcile", || {
        followed.heads().get("P0") == Some(&6)
    })
    .await;
    assert_eq!(*applied.changes.lock().unwrap(), vec![3, 4, 5, 6]);

    // The database failed over and lost 5 and 6; the writer, at a raised
    // epoch, writes 5 again, another change.
    record.lose("P0", 5, 3);
    record.shape(|shape| shape.control_epoch = 3);
    record.head("P0", 5);
    meridian_changes::publish(&bus, "book", 4, &[record.change("P0", 5)]);
    until("P0 held at the record's 5", || {
        followed.heads().get("P0") == Some(&5)
    })
    .await;
    assert_eq!(*applied.resets.lock().unwrap(), vec!["P0".to_string()]);
}

/// A gap whose catch-up failed while the writer still answers -- a read
/// timed out under load -- keeps the reader unready, by the gap alone, and is
/// read again: never left until its partition's next page, which a quiet
/// partition may not send for hours (review-c5c-ci1 M2, L2).
#[tokio::test(flavor = "multi_thread")]
async fn a_gap_whose_catch_up_failed_is_read_again_while_the_writer_answers() {
    let record = Record::of(&[("P0", 3)]);
    let (bus, followed, applied) = reading("member-test-gap-again", &record, &[("P0", 3)]).await;
    record.refuse("P0", true);
    record.head("P0", 6);
    meridian_changes::publish(&bus, "book", 5, &[record.change("P0", 6)]);
    // Every ask answered, nothing else behind: unready by the gap alone.
    tokio::time::sleep(Duration::from_millis(2_200)).await;
    held_unready(&followed, "P0", 3, Duration::from_secs(1)).await;
    assert!(said().lag_ms() >= 1_000);
    // The writer reads it now; nothing more is written.
    record.refuse("P0", false);
    until("4 to 6 read again", || {
        followed.heads().get("P0") == Some(&6)
    })
    .await;
    assert_eq!(*applied.changes.lock().unwrap(), vec![4, 5, 6]);
    until("ready once caught up", || {
        ready() && said().lag_ms() < 1_000
    })
    .await;
}

/// A gap whose pages the record lost -- a failover that lost a committed
/// tail -- ends at the record's head: a reader holding all the record holds
/// is not behind, and is ready, though the new writer has not yet written
/// that far again (review-c5c-ci1 M1).
#[tokio::test(flavor = "multi_thread")]
async fn a_gap_whose_pages_the_record_lost_ends_at_the_records_head() {
    let record = Record::of(&[("P0", 2)]);
    let (bus, followed, _) = reading("member-test-gap-lost", &record, &[("P0", 2)]).await;
    record.head("P0", 3);
    meridian_changes::publish(&bus, "book", 2, &[record.change("P0", 3)]);
    until("3 applied", || followed.heads().get("P0") == Some(&3)).await;
    record.refuse("P0", true);
    record.head("P0", 6);
    meridian_changes::publish(&bus, "book", 5, &[record.change("P0", 6)]);
    // Behind by the gap, unread: unready.
    tokio::time::sleep(Duration::from_millis(2_200)).await;
    held_unready(&followed, "P0", 3, Duration::from_millis(500)).await;
    // The database failed over and lost 4 to 6; the writer raised its epoch
    // and has written nothing since.
    record.lose("P0", 4, 3);
    record.shape(|shape| shape.control_epoch = 3);
    until("ready at the record's head", || {
        ready() && said().lag_ms() < 1_000
    })
    .await;
    assert_eq!(followed.heads().get("P0"), Some(&3));
}

/// A book's catch-up cut short after a page it applied follows on from that
/// page: the journal's chain moved only with what was applied, never ahead
/// of where the partition stands (review-c5c-ci1 M4).
#[tokio::test(flavor = "multi_thread")]
async fn a_book_catch_up_cut_short_follows_on_from_what_it_applied() {
    let record = Record::of(&[("P0", 2)]);
    let (bus, followed, applied) = reading("member-test-cut-short", &record, &[("P0", 2)]).await;
    record.head("P0", 3);
    meridian_changes::publish(&bus, "book", 2, &[record.change("P0", 3)]);
    until("3 applied", || followed.heads().get("P0") == Some(&3)).await;
    // 4 and 5 missed, 6 heard: 4's page read, 5's refused once.
    record.shape(|shape| shape.script.extend([true, false]));
    record.head("P0", 6);
    meridian_changes::publish(&bus, "book", 5, &[record.change("P0", 6)]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    record.head("P0", 7);
    meridian_changes::publish(&bus, "book", 6, &[record.change("P0", 7)]);
    until("7 read", || followed.heads().get("P0") == Some(&7)).await;
    assert_eq!(*applied.changes.lock().unwrap(), vec![3, 4, 5, 6, 7]);
}

/// A partition first heard whose page does not verify is not followed: the
/// reader is behind on it until it is held, never ready with it unheld
/// (step 3b's integrity at rest; review-c5c-ci1 L1), and it is held from the
/// record's head as the writer answers, as one first heard is held from its
/// page's -- what it held of it dropped, nothing applied -- never left waiting
/// for a page of it that verifies (review-c5i M3).
#[tokio::test(flavor = "multi_thread")]
async fn a_partition_first_heard_whose_page_does_not_verify_is_held_from_the_records_head() {
    let record = Record::of(&[("P0", 2)]);
    let (bus, followed, applied) = reading("member-test-unverified", &record, &[("P0", 2)]).await;
    record.head("P1", 2);
    let mut broken = record.change("P1", 2);
    broken.digest = vec![9; 32];
    meridian_changes::publish(&bus, "book", 0, &[record.change("P1", 1), broken]);
    until("P1 held from the record's head", || {
        followed.heads().get("P1") == Some(&2)
    })
    .await;
    assert_eq!(*applied.resets.lock().unwrap(), vec!["P1".to_string()]);
    assert!(applied.changes.lock().unwrap().is_empty());
    until("ready once P1 is held", || {
        ready() && said().lag_ms() < 1_000
    })
    .await;
    // And followed on from there, its next change taken as it is.
    record.head("P1", 3);
    meridian_changes::publish(&bus, "book", 2, &[record.change("P1", 3)]);
    until("P1's next page applied", || {
        followed.heads().get("P1") == Some(&3)
    })
    .await;
    assert_eq!(*applied.changes.lock().unwrap(), vec![3]);
}

/// A standby, which never reconciles, whose first page heard of a partition
/// does not verify holds the partition from the record's head as its writer
/// answers, and is ready again within a few ticks: never unready for good
/// for want of a page of it that verifies, which a partition seldom written
/// may not send for hours, a drain of its writer's node refused meanwhile
/// (review-c5i M3).
#[tokio::test(flavor = "multi_thread")]
async fn a_standby_whose_first_page_heard_does_not_verify_is_ready_once_it_holds_the_records_head()
{
    let bus = Arc::new(Bus::single(
        "member-test-standby-unverified",
        Arc::new(meridian_bus::MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let record = Record::of(&[("P1", 2)]);
    meridian_changes::serve(&bus, "book", record.clone());
    let following = tokio::spawn(follow(
        Arc::clone(&bus),
        "book",
        Duration::from_secs(1),
        None,
    ));
    until("the standby's interest", || {
        meridian_changes::followed(&bus, "book")
    })
    .await;
    until("ready", ready).await;
    let mut broken = record.change("P1", 2);
    broken.digest = vec![9; 32];
    meridian_changes::publish(&bus, "book", 0, &[record.change("P1", 1), broken]);
    // Past its bound and a tick: ready, P1 held.
    tokio::time::sleep(Duration::from_millis(2_200)).await;
    until("ready with P1 held from the record's head", || {
        ready() && said().partitions().contains(&"P1".to_string())
    })
    .await;
    following.abort();
}

/// A lake reader whose catch-up crosses into the epoch of a writer that took
/// over after a database failover lost a tail the reader applied drops what
/// it applied of the lost tail: the catch-up stops at the higher epoch and
/// the record is read back first, never applying the new writer's changes
/// over the lost one, which a later read-back would find kept -- the lake's
/// changes checked by their digests alone, no chain to refuse them (review-c5i
/// M1).
#[tokio::test(flavor = "multi_thread")]
async fn a_lake_readers_catch_up_across_a_lost_tail_drops_what_the_record_lost() {
    let record = Record::lake_of(&[("P0", 5)]);
    let (bus, followed, applied) = reading("member-test-lake-lineage", &record, &[("P0", 5)]).await;
    record.head("P0", 6);
    meridian_changes::publish(&bus, "lake", 5, &[record.change("P0", 6)]);
    until("6 applied", || followed.heads().get("P0") == Some(&6)).await;
    // 7 missed; 8's page, of the writer's epoch then, still on its way.
    let late = record.change("P0", 8);
    // The lake's database failed over and lost 6 on; the writer took over at
    // a raised epoch and wrote 6 to 12 again, before the reader asked it.
    let dropped = DROPPED.load(Ordering::SeqCst);
    record.lose("P0", 6, 3);
    record.head("P0", 12);
    meridian_changes::publish(&bus, "lake", 7, &[late]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    record.shape(|shape| shape.control_epoch = 3);
    until(
        "the lost 6 dropped, P0 reloaded at the record's head",
        || {
            applied.resets.lock().unwrap().contains(&"P0".to_string())
                && followed.heads().get("P0") == Some(&12)
        },
    )
    .await;
    assert!(DROPPED.load(Ordering::SeqCst) > dropped);
    until("ready once reloaded", || ready() && said().lag_ms() < 1_000).await;
}

/// A gap read to a record's head below the page heard past it holds the
/// partition at the record's head, never at the page heard: the record lost
/// that page, or its writer answers behind, and the reader's reads are never
/// held to changes it does not hold (review-c5i M4).
#[tokio::test(flavor = "multi_thread")]
async fn a_gap_read_to_a_records_head_below_the_page_heard_holds_the_records_head() {
    let record = Record::of(&[("P0", 5)]);
    let (bus, followed, applied) =
        reading("member-test-gap-below-heard", &record, &[("P0", 5)]).await;
    record.head("P0", 9);
    let heard = [record.change("P0", 8), record.change("P0", 9)];
    // The record's head 7 as the gap is read.
    record.head("P0", 7);
    meridian_changes::publish(&bus, "book", 7, &heard);
    until("6 and 7 read", || {
        *applied.changes.lock().unwrap() == vec![6, 7]
    })
    .await;
    let until_at = Instant::now() + Duration::from_millis(2_500);
    while Instant::now() < until_at {
        assert_eq!(
            followed.heads().get("P0"),
            Some(&7),
            "held past the record's head"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    until("ready at the record's head", || {
        ready() && said().lag_ms() < 1_000
    })
    .await;
}

/// A gap handed to the read-back keeps its wait in the member's lag: a gap
/// past the bound whose catch-up found the record's head below what the
/// reader applied leaves it unready while the read-back goes unanswered,
/// never ready as though it had just fallen behind -- holding what the
/// record lost (review-c5i L1).
#[tokio::test(flavor = "multi_thread")]
async fn a_gap_handed_to_the_read_back_keeps_the_reader_unready_while_it_goes_unanswered() {
    let record = Record::of(&[("P0", 3)]);
    let (bus, followed, _) = reading("member-test-gap-read-back", &record, &[("P0", 3)]).await;
    record.head("P0", 4);
    meridian_changes::publish(&bus, "book", 3, &[record.change("P0", 4)]);
    until("4 applied", || followed.heads().get("P0") == Some(&4)).await;
    // 5 and 6 missed, 7 heard, its catch-up refused: a gap past the bound.
    record.refuse("P0", true);
    record.head("P0", 7);
    meridian_changes::publish(&bus, "book", 6, &[record.change("P0", 7)]);
    tokio::time::sleep(Duration::from_millis(2_200)).await;
    assert!(!ready(), "ready with a gap past its bound");
    // The record's head falls below what the reader applied, and its
    // read-backs go unanswered: the gap, read again, is the read-back's.
    record.shape(|shape| {
        shape.refuse_read_backs = true;
        shape.refusing.clear();
        shape.heads.insert("P0".into(), 3);
    });
    let until_at = Instant::now() + Duration::from_millis(2_500);
    while Instant::now() < until_at {
        assert!(
            !ready(),
            "ready at {} ms of lag, its gap handed to a read-back unanswered",
            said().lag_ms()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The read-back answered: 4 lost, the partition reloaded at 3.
    record.shape(|shape| shape.refuse_read_backs = false);
    until("P0 reloaded at 3", || {
        followed.heads().get("P0") == Some(&3)
    })
    .await;
    until("ready once reloaded", || ready() && said().lag_ms() < 1_000).await;
}

/// A standby on `record`, ready while its lag is under a second, once the
/// writer has its interest.
async fn standing_by_on(
    name: &str,
    record: &Arc<Record>,
) -> (Arc<Bus>, tokio::task::JoinHandle<()>) {
    let bus = Arc::new(Bus::single(
        name,
        Arc::new(meridian_bus::MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    meridian_changes::serve(&bus, "book", record.clone());
    let following = tokio::spawn(follow(
        Arc::clone(&bus),
        "book",
        Duration::from_secs(1),
        None,
    ));
    until("the standby's interest", || {
        meridian_changes::followed(&bus, "book")
    })
    .await;
    (bus, following)
}

/// A page heard while its partition is left to read back is not taken, and
/// is a gap: read once the read-back finds the record holding what the
/// follower applied, never left unread -- the member ready, lag 0, behind --
/// until the partition's next page (review-c5i L3; a standby, which never
/// reconciles).
#[tokio::test(flavor = "multi_thread")]
async fn a_page_heard_while_its_partition_is_read_back_is_read_once_it_is_checked() {
    let record = Record::of(&[("P0", 2)]);
    let (bus, following) = standing_by_on("member-test-heard-unchecked", &record).await;
    record.head("P0", 4);
    meridian_changes::publish(&bus, "book", 2, &[record.change("P0", 3)]);
    meridian_changes::publish(&bus, "book", 3, &[record.change("P0", 4)]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    // A new writer, at a raised epoch, wrote 5 after 4; read-backs go
    // unanswered: P0 is left to read back, its page of 5 not taken.
    record.shape(|shape| shape.refuse_read_backs = true);
    record.lose("P0", 5, 3);
    record.head("P0", 5);
    meridian_changes::publish(&bus, "book", 4, &[record.change("P0", 5)]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    // The read-back answered, the record holding 4: 5 read from the writer.
    record.shape(|shape| shape.refuse_read_backs = false);
    until("5 read from the writer", || {
        record
            .shape
            .lock()
            .unwrap()
            .asked
            .contains(&("P0".to_string(), 4))
    })
    .await;
    until("ready once read", || ready() && said().lag_ms() < 1_000).await;
    following.abort();
}

/// A standby whose gap is behind its writer's retention holds the partition
/// again from the page heard, as a reader does, and is ready: never left
/// unready until a takeover or an epoch rise, the gap read again and warned
/// of every second (review-c5i L2).
#[tokio::test(flavor = "multi_thread")]
async fn a_standby_behind_its_writers_retention_holds_from_the_page_heard_and_is_ready() {
    let record = Record::of(&[("P0", 2)]);
    let (bus, following) = standing_by_on("member-test-standby-behind", &record).await;
    record.head("P0", 3);
    meridian_changes::publish(&bus, "book", 2, &[record.change("P0", 3)]);
    until("ready", ready).await;
    // The writer keeps 6 on: 4 and 5 missed past its retention, 7 heard.
    record.shape(|shape| shape.lowest_kept = 6);
    record.head("P0", 7);
    meridian_changes::publish(&bus, "book", 6, &[record.change("P0", 7)]);
    tokio::time::sleep(Duration::from_millis(2_200)).await;
    until("ready, held from the page heard", || {
        ready() && said().lag_ms() < 1_000
    })
    .await;
    following.abort();
}

/// Each partition's gap is its own, and the lag is the oldest still open:
/// one partition caught up leaves the lag the other's, and the reader is
/// ready only once both are read (CI 44187810396; review-c5c-ci1 L2).
#[tokio::test(flavor = "multi_thread")]
async fn a_readers_lag_is_its_oldest_gap_still_open() {
    let record = Record::of(&[("P0", 2), ("P1", 2)]);
    let (bus, followed, _) =
        reading("member-test-two-gaps", &record, &[("P0", 2), ("P1", 2)]).await;
    record.refuse("P0", true);
    record.refuse("P1", true);
    record.head("P0", 5);
    meridian_changes::publish(&bus, "book", 4, &[record.change("P0", 5)]);
    let first = Instant::now();
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    record.head("P1", 5);
    meridian_changes::publish(&bus, "book", 4, &[record.change("P1", 5)]);
    let second = Instant::now();
    until("the lag P0's", || {
        said().lag_ms() as u128 + 1_200 >= first.elapsed().as_millis() && said().lag_ms() >= 2_000
    })
    .await;
    assert!(!ready());
    // P0 read: the lag is P1's, younger, and still past the bound.
    record.refuse("P0", false);
    record.head("P0", 6);
    meridian_changes::publish(&bus, "book", 5, &[record.change("P0", 6)]);
    until("P0 read", || followed.heads().get("P0") == Some(&6)).await;
    until("the lag P1's", || {
        let lag = said().lag_ms() as u128;
        lag + 1_000 < first.elapsed().as_millis()
            && lag <= second.elapsed().as_millis()
            && lag >= 1_000
    })
    .await;
    assert!(!ready());
    // P1 read: no gap is left.
    record.refuse("P1", false);
    record.head("P1", 6);
    meridian_changes::publish(&bus, "book", 5, &[record.change("P1", 6)]);
    until("P1 read", || followed.heads().get("P1") == Some(&6)).await;
    until("ready once both are read", || {
        ready() && said().lag_ms() < 1_000
    })
    .await;
}

/// A reader whose gaps are read again while each read times out -- the
/// writer answers its asks, its catch-ups too late, as under load -- says it
/// is unready once its oldest gap passes its bound, not once the reads end;
/// and reading them again holds the follow loop for a second and one read at
/// the most, so a page of another partition that follows on is applied
/// meanwhile (review-c5i M2: the retry pass held the loop 5 s a gap, the
/// member ready as the last tick left it for 26 s, the tail undrained).
#[tokio::test(flavor = "multi_thread")]
async fn a_reader_whose_gap_reads_time_out_turns_unready_within_its_bound_and_follows_on() {
    let quiet: Vec<String> = (0..5).map(|n| format!("P{n}")).collect();
    let mut start: Vec<(&str, u64)> = quiet.iter().map(|p| (p.as_str(), 2)).collect();
    start.push(("P9", 2));
    let record = Record::of(&start);
    let (bus, followed, _) = reading("member-test-gaps-timing-out", &record, &start).await;
    until("ready", ready).await;
    // A page missed of each of five partitions, each catch-up refused at
    // once: five gaps open. From now each catch-up times out.
    for partition in &quiet {
        record.refuse(partition, true);
        record.head(partition, 4);
        meridian_changes::publish(&bus, "book", 3, &[record.change(partition, 4)]);
    }
    let opened = Instant::now();
    until("five gaps open", || {
        let shape = record.shape.lock().unwrap();
        quiet
            .iter()
            .all(|partition| shape.refused.contains(partition))
    })
    .await;
    record.shape(|shape| shape.hang = quiet.iter().cloned().collect());
    // Unready within its bound (a second) and a tick, never as the last tick
    // left it.
    let bound = Duration::from_secs(1);
    while ready() {
        assert!(
            opened.elapsed() < bound + ASK_EVERY + Duration::from_millis(500),
            "ready {} ms after five gaps opened, its bound {} ms",
            opened.elapsed().as_millis(),
            bound.as_millis()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // A page of P9 that follows on, heard while the gaps are read again:
    // applied within a tick and one read timing out.
    tokio::time::sleep(Duration::from_millis(500)).await;
    record.head("P9", 3);
    meridian_changes::publish(&bus, "book", 2, &[record.change("P9", 3)]);
    while followed.heads().get("P9") != Some(&3) {
        assert!(
            opened.elapsed() < Duration::from_secs(8),
            "P9's page not applied {} ms after the gaps opened",
            opened.elapsed().as_millis()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!ready(), "ready with five gaps unread");
    // The writer answers again: every gap read, once the read in flight has
    // timed out, and ready.
    record.shape(|shape| {
        shape.hang.clear();
        shape.refusing.clear();
    });
    let answering = Instant::now();
    while !quiet
        .iter()
        .all(|partition| followed.heads().get(partition) == Some(&4))
    {
        assert!(
            answering.elapsed() < Duration::from_secs(10),
            "gaps unread: {:?}",
            followed.heads()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    until("ready once caught up", || {
        ready() && said().lag_ms() < 1_000
    })
    .await;
}

#[test]
fn a_member_with_no_lease_writes_at_once() {
    let member = Member::sole("street");
    assert!(!member.leased());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let bus = Arc::new(Bus::single(
        "member-test-street",
        Arc::new(meridian_bus::MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let holding = runtime.block_on(member.wait_to_write(&bus)).unwrap();
    assert!(!holding.took_over());
    assert_eq!(said().mode(), "writer");
}

#[test]
fn the_tail_is_one_compressed_page_per_transaction() {
    let bus = Arc::new(Bus::single(
        "member-test-lake",
        Arc::new(meridian_bus::MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut tail = bus.subscribe_every("platform.lake.internal.changes.*");
        // A follower's ask, as a standby makes it, is the interest.
        struct Empty;
        impl ChangeLog for Empty {
            fn read_changes(
                &self,
                _: &str,
                _: u64,
                _: usize,
            ) -> Result<ReadChangesReply, ReadError> {
                Ok(ReadChangesReply::default())
            }
        }
        meridian_changes::serve(&bus, "lake", Arc::new(Empty));
        ask_writer(&bus, "lake").await.unwrap();
        assert!(meridian_changes::followed(&bus, "lake"));
        let rows: Vec<Change> = (11..=20)
            .map(|sequence| Change {
                partition: "venue-1:live".into(),
                ..change(sequence)
            })
            .collect();
        meridian_changes::publish(&bus, "lake", 10, &rows);
        let heard = tail.recv().await.unwrap();
        let page =
            meridian_changes::read_tail(&heard.envelope.payload_type, &heard.envelope.payload)
                .unwrap();
        assert_eq!(page.changes, rows, "ten rows, one page");
        assert_eq!((page.after_sequence, page.head), (10, 20));
        assert!(
            heard.envelope.payload.len() < rows.iter().map(|c| c.encoded_len()).sum::<usize>(),
            "compressed"
        );
        assert!(tail.try_recv().is_none(), "and nothing else");
    });
}

/// A writer's log of `P0` across a database failover that lost a tail: each
/// change below `lost_from` as first written, each from it written again
/// under the raised epoch with another body.
struct Failing {
    state: Mutex<(u64, u64, u64)>, // head, lost_from, epoch now
}

impl Failing {
    fn change(&self, sequence: u64) -> Change {
        let (_, lost_from, epoch) = *self.state.lock().unwrap();
        let again = sequence >= lost_from;
        Change {
            partition: "P0".into(),
            sequence,
            epoch: if again { epoch } else { 2 },
            origin: "DEP-1".into(),
            committed_at_ns: sequence as i64,
            kind: "entry".into(),
            body: vec![if again { 2 } else { 1 }; 16],
            digest: Vec::new(),
        }
    }
}

impl ChangeLog for Failing {
    fn read_changes(
        &self,
        partition: &str,
        after: u64,
        most: usize,
    ) -> Result<ReadChangesReply, ReadError> {
        let (head, _, epoch) = *self.state.lock().unwrap();
        if partition == meridian_changes::CONTROL {
            return Ok(ReadChangesReply {
                head: 0,
                epoch,
                ..Default::default()
            });
        }
        let mut page = Page::new(most);
        for sequence in (after + 1)..=head {
            if !page.take(self.change(sequence)) {
                break;
            }
        }
        Ok(ReadChangesReply {
            changes: page.changes,
            head,
            lowest_kept: 1,
            epoch,
            after_sequence: 0,
        })
    }
}

/// A new database timeline raises the epoch (contract v30, stage C; the
/// resilience spec's section 3): a reader that applied a tail the database's
/// failover lost, the writer having taken those sequences again under the
/// raised epoch, reads back what it applied once the epoch rises, finds the
/// record no longer holds it, and drops the partition and reloads it at the
/// record's head -- never holding a change the record does not -- then
/// follows the new epoch's changes.
#[tokio::test(flavor = "multi_thread")]
async fn a_reader_that_applied_a_lost_tail_drops_it_when_the_epoch_rises() {
    let bus = Arc::new(Bus::single(
        "member-test-lineage",
        Arc::new(meridian_bus::MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let log = Arc::new(Failing {
        state: Mutex::new((2, u64::MAX, 2)),
    });
    meridian_changes::serve(&bus, "book", log.clone());
    let applied = Arc::new(Applied::default());
    struct Primary;
    impl meridian_bus::read_after::Position for Primary {
        fn heads(&self, _: &[String]) -> Result<HashMap<String, u64>, String> {
            Ok([("P0".to_string(), 2)].into())
        }
    }
    let member = Member::reader_of("bor", "book", Duration::from_secs(5));
    let followed = crate::member::Followed::from_heads([("P0".to_string(), 2)]);
    member
        .read(&bus, followed.clone(), applied.clone(), Arc::new(Primary))
        .unwrap();
    until("the reader's interest", || {
        meridian_changes::followed(&bus, "book")
    })
    .await;
    log.state.lock().unwrap().0 = 6;
    let first: Vec<Change> = (3..=6).map(|s| log.change(s)).collect();
    meridian_changes::publish(&bus, "book", 2, &first);
    until("6 applied", || followed.heads().get("P0") == Some(&6)).await;
    let dropped_before = DROPPED.load(Ordering::SeqCst);

    // The database failed over and lost 5 and 6; the writer raised its
    // epoch and wrote 5 again, another change.
    *log.state.lock().unwrap() = (5, 5, 3);
    until("P0 dropped and reloaded at the record's head", || {
        followed.heads().get("P0") == Some(&5)
    })
    .await;
    assert_eq!(*applied.resets.lock().unwrap(), vec!["P0".to_string()]);
    assert!(DROPPED.load(Ordering::SeqCst) > dropped_before);

    // The new epoch's next change follows on.
    log.state.lock().unwrap().0 = 6;
    meridian_changes::publish(&bus, "book", 5, &[log.change(6)]);
    until("the new 6 applied", || {
        followed.heads().get("P0") == Some(&6)
    })
    .await;
    assert_eq!(applied.resets.lock().unwrap().len(), 1);
}

/// A writer's takeover raises the epoch with nothing lost: the reader reads
/// back what it applied, finds it held, and drops nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_reader_drops_nothing_when_the_epoch_rises_over_a_record_that_lost_nothing() {
    let bus = Arc::new(Bus::single(
        "member-test-lineage-kept",
        Arc::new(meridian_bus::MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let log = Arc::new(Failing {
        state: Mutex::new((2, u64::MAX, 2)),
    });
    meridian_changes::serve(&bus, "book", log.clone());
    let applied = Arc::new(Applied::default());
    struct Primary;
    impl meridian_bus::read_after::Position for Primary {
        fn heads(&self, _: &[String]) -> Result<HashMap<String, u64>, String> {
            Ok([("P0".to_string(), 2)].into())
        }
    }
    let member = Member::reader_of("bor", "book", Duration::from_secs(5));
    let followed = crate::member::Followed::from_heads([("P0".to_string(), 2)]);
    member
        .read(&bus, followed.clone(), applied.clone(), Arc::new(Primary))
        .unwrap();
    until("the reader's interest", || {
        meridian_changes::followed(&bus, "book")
    })
    .await;
    log.state.lock().unwrap().0 = 4;
    let first: Vec<Change> = (3..=4).map(|s| log.change(s)).collect();
    meridian_changes::publish(&bus, "book", 2, &first);
    until("4 applied", || followed.heads().get("P0") == Some(&4)).await;
    // A takeover: the epoch raised from 5 on, nothing lost.
    *log.state.lock().unwrap() = (4, 5, 3);
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    log.state.lock().unwrap().0 = 5;
    meridian_changes::publish(&bus, "book", 4, &[log.change(5)]);
    until("5 applied", || followed.heads().get("P0") == Some(&5)).await;
    assert!(applied.resets.lock().unwrap().is_empty());
    assert_eq!(*applied.changes.lock().unwrap(), vec![3, 4, 5]);
}

/// A writer hands over cleanly: the new writer answers every ask, so the
/// reader never finds its writer unanswering, and what the new writer
/// committed before this reader first asked it was published to nobody (the
/// tail goes only to a follower that has asked, ruling 1062) -- its own
/// takeover, journalled on `control` as it starts. The reader reads it from
/// the writer once the new writer's epoch is read back, with no page after
/// it to say one was missed (failover-repeat 43697279874, ha run 2: the
/// book's reader stayed one behind on `control` for a minute, every read
/// naming it answered from the primary).
#[tokio::test(flavor = "multi_thread")]
async fn a_reader_reads_what_a_new_writer_committed_before_it_was_first_asked() {
    let bus = Arc::new(Bus::single(
        "member-test-hand-over",
        Arc::new(meridian_bus::MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let log = Arc::new(Failing {
        state: Mutex::new((2, u64::MAX, 2)),
    });
    meridian_changes::serve(&bus, "book", log.clone());
    let applied = Arc::new(Applied::default());
    let record = log.clone();
    let primary = meridian_bus::read_after::AllHeads(move || {
        let head = record.state.lock().unwrap().0;
        Ok(std::collections::BTreeMap::from([("P0".to_string(), head)]))
    });
    let member = Member::reader_of("bor", "book", Duration::from_secs(5));
    let followed = crate::member::Followed::from_heads([("P0".to_string(), 2)]);
    member
        .read(&bus, followed.clone(), applied.clone(), Arc::new(primary))
        .unwrap();
    until("the reader's interest", || {
        meridian_changes::followed(&bus, "book")
    })
    .await;
    log.state.lock().unwrap().0 = 3;
    meridian_changes::publish(&bus, "book", 2, &[log.change(3)]);
    until("3 applied", || followed.heads().get("P0") == Some(&3)).await;

    // The new writer, at a raised epoch: 4 committed and told to nobody,
    // and nothing written after it.
    *log.state.lock().unwrap() = (4, 4, 3);
    until("4 read from the new writer", || {
        followed.heads().get("P0") == Some(&4)
    })
    .await;
    assert_eq!(*applied.changes.lock().unwrap(), vec![3, 4]);
    assert!(applied.resets.lock().unwrap().is_empty());
}

/// A page lost on the way -- the tail is at most once -- of a partition not
/// written again: no later page says one was missed, and the reader's
/// writer never stopped answering. The first read naming it waits out its
/// bound and is answered from the primary, which proves the reader behind;
/// its follower then reads what it lacks from the writer, and the next read
/// is its own record's.
#[tokio::test(flavor = "multi_thread")]
async fn a_reader_a_read_proved_behind_its_primary_reads_what_it_lacks() {
    let bus = Arc::new(Bus::single(
        "member-test-left-behind",
        Arc::new(meridian_bus::MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let log = Arc::new(Failing {
        state: Mutex::new((2, u64::MAX, 2)),
    });
    meridian_changes::serve(&bus, "book", log.clone());
    let applied = Arc::new(Applied::default());
    let record = log.clone();
    let primary = meridian_bus::read_after::AllHeads(move || {
        let head = record.state.lock().unwrap().0;
        Ok(std::collections::BTreeMap::from([("P0".to_string(), head)]))
    });
    let member = Member::reader_of("bor", "book", Duration::from_millis(300));
    let followed = crate::member::Followed::from_heads([("P0".to_string(), 2)]);
    member
        .read(&bus, followed.clone(), applied.clone(), Arc::new(primary))
        .unwrap();
    until("the reader's interest", || {
        meridian_changes::followed(&bus, "book")
    })
    .await;
    // Reconciled with its primary as its writer first answered: nothing
    // lacking then.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 3 committed, its page lost; the epoch as it was, every ask answered.
    log.state.lock().unwrap().0 = 3;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(
        followed.heads().get("P0"),
        Some(&2),
        "nothing tells the reader of the page it missed"
    );
    let naming = |sequence| meridian_pb::v1::Watermark {
        partitions: vec![meridian_pb::v1::PartitionSequence {
            partition: "P0".into(),
            sequence,
        }],
    };
    let reads = bus.clone();
    let held = tokio::task::spawn_blocking(move || {
        reads
            .reads_after()
            .hold("platform.book.query.list-orders", &naming(3))
    })
    .await
    .unwrap();
    assert_eq!(held, Ok(meridian_bus::read_after::Held::FromPrimary));
    until("3 read from the writer", || {
        followed.heads().get("P0") == Some(&3)
    })
    .await;
    assert_eq!(*applied.changes.lock().unwrap(), vec![3]);
    assert!(applied.resets.lock().unwrap().is_empty());
    let reads = bus.clone();
    let held = tokio::task::spawn_blocking(move || {
        reads
            .reads_after()
            .hold("platform.book.query.list-orders", &naming(3))
    })
    .await
    .unwrap();
    assert_eq!(held, Ok(meridian_bus::read_after::Held::Here));
}

/// A writer's log one change of which no longer matches its digest (bit
/// rot): read for it, refused; asked for its head, answered.
struct Rotten {
    head: AtomicU64,
    epoch: AtomicU64,
}

impl ChangeLog for Rotten {
    fn read_changes(
        &self,
        partition: &str,
        after: u64,
        most: usize,
    ) -> Result<ReadChangesReply, ReadError> {
        let (head, epoch) = (
            self.head.load(Ordering::SeqCst),
            self.epoch.load(Ordering::SeqCst),
        );
        if partition == meridian_changes::CONTROL {
            return Ok(ReadChangesReply {
                head: 0,
                epoch,
                ..Default::default()
            });
        }
        if after < head && epoch > 2 {
            return Err(ReadError::Unavailable(format!(
                "change {} of partition P0 does not match its digest",
                after + 1
            )));
        }
        let mut page = Page::new(most);
        for sequence in (after + 1)..=head {
            if !page.take(change(sequence)) {
                break;
            }
        }
        Ok(ReadChangesReply {
            changes: page.changes,
            head,
            lowest_kept: 1,
            epoch,
            after_sequence: 0,
        })
    }
}

/// A reader whose last applied change the record can no longer read back
/// (its digest no longer matches) once the epoch rises holds the partition
/// from the record's head, never asking for ever and staying behind
/// (contract v30, stage C; CI 42965355676's conductor standby).
#[tokio::test(flavor = "multi_thread")]
async fn a_reader_whose_applied_change_cannot_be_read_back_holds_from_the_head() {
    let bus = Arc::new(Bus::single(
        "member-test-rotten",
        Arc::new(meridian_bus::MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let log = Arc::new(Rotten {
        head: AtomicU64::new(2),
        epoch: AtomicU64::new(2),
    });
    meridian_changes::serve(&bus, "book", log.clone());
    let applied = Arc::new(Applied::default());
    struct Primary;
    impl meridian_bus::read_after::Position for Primary {
        fn heads(&self, _: &[String]) -> Result<HashMap<String, u64>, String> {
            Ok([("P0".to_string(), 2)].into())
        }
    }
    let member = Member::reader_of("bor", "book", Duration::from_secs(5));
    let followed = crate::member::Followed::from_heads([("P0".to_string(), 2)]);
    member
        .read(&bus, followed.clone(), applied.clone(), Arc::new(Primary))
        .unwrap();
    until("the reader's interest", || {
        meridian_changes::followed(&bus, "book")
    })
    .await;
    log.head.store(3, Ordering::SeqCst);
    meridian_changes::publish(&bus, "book", 2, &[change(3)]);
    until("3 applied", || followed.heads().get("P0") == Some(&3)).await;
    // The epoch rises; change 3 no longer reads back.
    log.head.store(4, Ordering::SeqCst);
    log.epoch.store(3, Ordering::SeqCst);
    until("P0 held from the record's head", || {
        followed.heads().get("P0") == Some(&4)
    })
    .await;
    assert_eq!(*applied.resets.lock().unwrap(), vec!["P0".to_string()]);
}

/// The member a lease's holder names, for a takeover's `previous_member`
/// (contract v30, stage D): its pod's name, before the process's mark.
#[test]
fn a_lease_holder_names_its_pod() {
    assert_eq!(
        super::holder_member("om-bor-7d9f-x2k_18a3f"),
        "om-bor-7d9f-x2k"
    );
    assert_eq!(super::holder_member("om-bor-0"), "om-bor-0");
    assert_eq!(super::holder_member(""), "");
}

/// Step 3d: a member waiting on what it needs to do its whole job -- the
/// lake on its write-ahead stream, kept on three servers, while the broker's
/// JetStream is not ready -- reports itself degraded, saying why, until it
/// is done waiting.
#[test]
fn a_member_waiting_reports_itself_degraded_and_why() {
    use meridian_domain::v1::ComponentHealth;
    said().set_waiting("taking no write: its write-ahead stream could not be made yet");
    let report = member_report("lake", 0, 0);
    assert_eq!(report.health, ComponentHealth::Degraded as i32);
    assert!(
        report.detail.contains("write-ahead stream"),
        "{}",
        report.detail
    );
    said().done_waiting();
    let report = member_report("lake", 0, 0);
    assert_eq!(report.health, ComponentHealth::Serving as i32);
    assert!(report.detail.is_empty());
}

// ── A writer told to stop serves on until another group serves (stage T) ──

/// A look that says another group's writer serves from `after` on, by the
/// (paused) clock, counting how often it was asked.
fn serves_after(
    after: Duration,
) -> (
    impl Fn(i64, i64) -> Result<bool, String> + Send + Sync + 'static,
    Arc<AtomicU64>,
) {
    let began = tokio::time::Instant::now();
    let asked = Arc::new(AtomicU64::new(0));
    let counted = Arc::clone(&asked);
    (
        move |_, _| {
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(began.elapsed() >= after)
        },
        asked,
    )
}

/// The first group's writer, a store of one group, and a member that is no
/// writer wait on nobody: told to stop, each stops at once, the store's
/// members never read.
#[tokio::test(start_paused = true)]
async fn the_first_groups_writer_one_group_alone_and_no_writer_stop_at_once() {
    for (mine, count, writer) in [(0, 2, true), (0, 1, true), (1, 2, false)] {
        let (look, asked) = serves_after(Duration::from_secs(3600));
        let began = tokio::time::Instant::now();
        let stopped = serve_on_until_another_group_serves(mine, count, writer, 1, look).await;
        assert_eq!(stopped, ServedOn::AtOnce, "group {mine} of {count}");
        assert_eq!(began.elapsed(), Duration::ZERO, "group {mine} of {count}");
        assert_eq!(asked.load(Ordering::SeqCst), 0, "group {mine} of {count}");
    }
}

/// Every group's members are rolled at once. A writer of group 1, told to
/// stop, serves on while no other group's writer serves anew, and stops
/// within a look of one serving: here the first group's writer is back
/// after 7 s. Stopped at once, as before, nothing served the store's public
/// topics for those seconds (PR CI 43636763392).
#[tokio::test(start_paused = true)]
async fn a_later_groups_writer_serves_on_until_another_groups_writer_serves() {
    let (look, asked) = serves_after(Duration::from_secs(7));
    let began = tokio::time::Instant::now();
    let stopped = serve_on_until_another_group_serves(1, 2, true, 1, look).await;
    assert_eq!(stopped, ServedOn::AnotherServes);
    let served_on = began.elapsed();
    assert!(
        served_on >= Duration::from_secs(7) && served_on <= Duration::from_millis(7_500),
        "it served on for {served_on:?}: until the other group's writer served, and a look more at most"
    );
    assert!(
        asked.load(Ordering::SeqCst) >= 14,
        "looked every half second"
    );
}

/// A stop that is no rollout -- its node drained, its pod deleted -- sees no
/// other group's writer serve anew: it serves on for the bound and stops,
/// inside the 30 s its pod is given, with time left to give its lease up.
#[tokio::test(start_paused = true)]
async fn it_stops_at_the_bound_where_no_other_groups_writer_serves_anew() {
    let (look, _) = serves_after(Duration::from_secs(3600));
    let began = tokio::time::Instant::now();
    let stopped = serve_on_until_another_group_serves(3, 4, true, 1, look).await;
    assert_eq!(stopped, ServedOn::Bound);
    let served_on = began.elapsed();
    assert!(
        served_on <= SERVES_ON_AT_MOST && served_on >= SERVES_ON_AT_MOST - Duration::from_secs(1),
        "it served on for {served_on:?}, the bound {SERVES_ON_AT_MOST:?}"
    );
}

/// The store's members unread -- its database 0 restarting with the same
/// upgrade -- is no answer: asked again, never taken as leave to stop, and
/// the time it was told passed to every look as it was.
#[tokio::test(start_paused = true)]
async fn a_look_that_fails_is_asked_again_and_is_no_leave_to_stop() {
    let began = tokio::time::Instant::now();
    let told = Arc::new(Mutex::new(Vec::new()));
    let heard = Arc::clone(&told);
    let look = move |told_at_ns: i64, _now: i64| {
        heard.lock().unwrap().push(told_at_ns);
        if began.elapsed() < Duration::from_secs(4) {
            Err("the database is starting up".to_string())
        } else {
            Ok(true)
        }
    };
    let stopped = serve_on_until_another_group_serves(1, 2, true, 42, look).await;
    assert_eq!(stopped, ServedOn::AnotherServes);
    assert!(began.elapsed() >= Duration::from_secs(4));
    let told = told.lock().unwrap();
    assert!(told.len() >= 8 && told.iter().all(|at| *at == 42));
}

/// Stage T's review, finding 1: a look that does not come back -- the
/// store's pool waiting on a database whose primary is switching -- is
/// given what is left of the bound and no longer, so the member stops and
/// gives its lease up inside the time its pod is given. Before, the bound
/// was read only once the look returned.
#[tokio::test]
async fn a_look_that_does_not_come_back_never_holds_the_stop_past_the_bound() {
    let (let_go, held) = std::sync::mpsc::channel::<()>();
    let held = Mutex::new(held);
    // Each look waits five seconds on a database that does not answer.
    let look = move |_: i64, _: i64| {
        let _ = held.lock().unwrap().recv_timeout(Duration::from_secs(5));
        Ok(false)
    };
    let began = std::time::Instant::now();
    let stopped = super::serve_on_within(
        1,
        2,
        true,
        1,
        look,
        Duration::from_millis(400),
        Duration::from_millis(100),
    )
    .await;
    let took = began.elapsed();
    drop(let_go);
    assert_eq!(stopped, ServedOn::Bound);
    assert!(
        took < Duration::from_millis(1_500),
        "it stopped {took:?} after it was told, its bound 400 ms: a look still out is left behind"
    );
}

/// What a member says it serves as: nothing until it serves, then its mode.
/// A writer holding its lease whose topics are not yet served is not one
/// another group's writer may stop for.
#[test]
fn a_member_says_its_mode_only_once_it_serves() {
    said().set_mode(Mode::Writer);
    set_serving(false);
    assert_eq!(serving_mode(), "");
    set_serving(true);
    assert_eq!(serving_mode(), "writer");
    set_serving(false);
}

/// One Lease as the API server keeps it, each write conditional on the
/// version read, and slow to answer: a write lands at once and its answer
/// is read `answers_in` later, as a request in flight.
#[derive(Clone)]
struct SlowLeases {
    held: Arc<std::sync::Mutex<Option<crate::lease::Record>>>,
    answers_in: Duration,
}

impl Leases for SlowLeases {
    async fn get(&self, _name: &str) -> Result<Option<crate::lease::Record>, String> {
        Ok(self.held.lock().unwrap().clone())
    }

    async fn create(
        &self,
        _name: &str,
        record: &crate::lease::Record,
    ) -> Result<crate::lease::Wrote, String> {
        {
            let mut held = self.held.lock().unwrap();
            if held.is_some() {
                return Ok(crate::lease::Wrote::Conflict);
            }
            let mut record = record.clone();
            record.resource_version = "1".into();
            *held = Some(record);
        }
        tokio::time::sleep(self.answers_in).await;
        Ok(crate::lease::Wrote::Done("1".into()))
    }

    async fn update(
        &self,
        _name: &str,
        record: &crate::lease::Record,
    ) -> Result<crate::lease::Wrote, String> {
        let next = {
            let mut held = self.held.lock().unwrap();
            let Some(current) = held.as_ref() else {
                return Ok(crate::lease::Wrote::Conflict);
            };
            if current.resource_version != record.resource_version {
                return Ok(crate::lease::Wrote::Conflict);
            }
            let next = (current.resource_version.parse::<u64>().unwrap() + 1).to_string();
            let mut record = record.clone();
            record.resource_version = next.clone();
            *held = Some(record);
            next
        };
        tokio::time::sleep(self.answers_in).await;
        Ok(crate::lease::Wrote::Done(next))
    }
}

fn standing_by(
    answers_in: Duration,
) -> (
    SlowLeases,
    Elector<SlowLeases>,
    tokio::sync::watch::Sender<bool>,
    tokio::sync::watch::Receiver<bool>,
) {
    let api = SlowLeases {
        held: Arc::default(),
        answers_in,
    };
    let elector = Elector::new(
        api.clone(),
        crate::lease::Settings {
            name: "release-bor".into(),
            holder: "bor-b_1".into(),
            renew: Duration::from_secs(2),
            duration: Duration::from_secs(6),
        },
    );
    let (tell, told) = tokio::sync::watch::channel(false);
    (api, elector, tell, told)
}

/// A standby is told to stop while its take of the lease is in flight: the
/// write has landed at the API server and its answer is not read yet.
/// Ended where it stood, it stopped holding a lease nobody renews, and its
/// store went unwritten until the lapse. The take is finished and the lease
/// given up: the stop holds nothing, and the lease is free for the next
/// member at once.
#[tokio::test(start_paused = true)]
async fn a_stop_that_lands_while_the_lease_is_taken_gives_it_up_and_never_ends_holding_it() {
    let (api, mut elector, tell, mut told) = standing_by(Duration::from_secs(2));
    let waiting = tokio::spawn(async move {
        stand_by(&mut elector, Duration::from_secs(1), "bor", &mut told, None).await
    });
    // The take is out: written, not yet answered.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        api.held.lock().unwrap().as_ref().map(|r| r.holder.clone()),
        Some("bor-b_1".into()),
        "the take had not landed when the stop was sent"
    );
    tell.send(true).unwrap();
    let holds = waiting.await.unwrap();
    assert!(!holds, "told to stop, it went on to write");
    let left = api.held.lock().unwrap().clone().expect("the lease is gone");
    assert_eq!(
        left.holder, "",
        "it stopped holding the lease: nobody renews it, and nothing is written until it lapses"
    );
}

/// A standby told to stop between its looks holds nothing and stops at
/// once, no look made after; one never told takes a free lease as before.
#[tokio::test(start_paused = true)]
async fn a_standby_told_to_stop_between_looks_stops_at_once_and_one_never_told_takes_the_lease() {
    let (api, mut elector, tell, mut told) = standing_by(Duration::ZERO);
    *api.held.lock().unwrap() = Some(crate::lease::Record {
        holder: "bor-a_1".into(),
        duration_seconds: 6,
        acquire_time: crate::lease::micro_time(now_ns()),
        renew_time: crate::lease::micro_time(now_ns()),
        transitions: 0,
        resource_version: "7".into(),
        uid: "uid-1".into(),
        metadata: serde_json::Value::Null,
    });
    let waiting = tokio::spawn(async move {
        stand_by(
            &mut elector,
            Duration::from_secs(3600),
            "bor",
            &mut told,
            None,
        )
        .await
    });
    tokio::time::sleep(Duration::from_secs(5)).await;
    let began = tokio::time::Instant::now();
    tell.send(true).unwrap();
    assert!(!waiting.await.unwrap());
    assert_eq!(began.elapsed(), Duration::ZERO, "it waited its look out");
    assert_eq!(
        api.held.lock().unwrap().as_ref().unwrap().holder,
        "bor-a_1",
        "the writer's lease was touched"
    );

    let (api, mut elector, _tell, mut told) = standing_by(Duration::ZERO);
    assert!(stand_by(&mut elector, Duration::from_secs(1), "bor", &mut told, None).await);
    assert_eq!(api.held.lock().unwrap().as_ref().unwrap().holder, "bor-b_1");
}

/// A writer stepping down gives its lease up first (ruled 2026-11-16): its
/// standby takes over at its next look, not after the lapse (CI
/// 43788516279: the lake's writer, its epoch passed, stepped down holding
/// its lease). Only its own: a lease another member took meanwhile is never
/// touched. And within its bound, the API server slow to answer.
#[tokio::test(start_paused = true)]
async fn a_writer_stepping_down_gives_its_own_lease_up_and_never_one_another_took() {
    let (api, mut elector, _tell, _told) = standing_by(Duration::ZERO);
    assert_eq!(
        elector.look(Instant::now(), now_ns()).await,
        Ok(Standing::Held)
    );
    let elector = tokio::sync::Mutex::new(elector);
    let_go_stepping_down(&elector, "bor").await;
    let left = api.held.lock().unwrap().clone().unwrap();
    assert_eq!(
        left.holder, "",
        "the lease it held is given up: its standby takes it at once"
    );

    // Another member took it meanwhile: never cleared.
    let (api, mut elector, _tell, _told) = standing_by(Duration::ZERO);
    assert_eq!(
        elector.look(Instant::now(), now_ns()).await,
        Ok(Standing::Held)
    );
    {
        let mut held = api.held.lock().unwrap();
        let current = held.as_mut().unwrap();
        current.holder = "bor-c_1".into();
        current.resource_version = "9".into();
    }
    let elector = tokio::sync::Mutex::new(elector);
    let_go_stepping_down(&elector, "bor").await;
    assert_eq!(
        api.held.lock().unwrap().as_ref().unwrap().holder,
        "bor-c_1",
        "a lease another member took was cleared"
    );

    // The API server slow: given up on within the bound.
    let (api, mut elector, _tell, _told) = standing_by(Duration::ZERO);
    assert_eq!(
        elector.look(Instant::now(), now_ns()).await,
        Ok(Standing::Held)
    );
    let slow = SlowLeases {
        held: Arc::clone(&api.held),
        answers_in: Duration::from_secs(30),
    };
    let mut slowly = Elector::new(slow, elector.settings().clone());
    assert_eq!(
        slowly.look(Instant::now(), now_ns()).await,
        Ok(Standing::Held)
    );
    let began = tokio::time::Instant::now();
    let_go_stepping_down(&tokio::sync::Mutex::new(slowly), "bor").await;
    assert!(
        began.elapsed() <= STEP_DOWN_RELEASE,
        "it waited {:?} to give the lease up",
        began.elapsed()
    );
}

/// A standby defers to a newer member of its group (ruled 2026-11-16): a
/// free lease is left to the member a rollout brought at the new count,
/// which takes it; this member, at the old count, never takes it then (CI
/// 43788516279, "ha groups raised": an old member's takeover fenced the
/// added group's writer).
#[tokio::test(start_paused = true)]
async fn a_standby_leaves_a_free_lease_to_a_newer_member_of_its_group() {
    let (api, mut elector, _tell, mut told) = standing_by(Duration::ZERO);
    let newer: Arc<NewerLook> = Arc::new(|| Ok(true));
    let waiting = tokio::spawn(async move {
        stand_by(
            &mut elector,
            Duration::from_secs(1),
            "bor",
            &mut told,
            Some(newer),
        )
        .await
    });
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        api.held.lock().unwrap().is_none(),
        "the old member took the free lease while a newer member of its group was there"
    );
    // The newer member takes it, and renews it.
    *api.held.lock().unwrap() = Some(crate::lease::Record {
        holder: "bor-new_1".into(),
        duration_seconds: 6,
        acquire_time: crate::lease::micro_time(now_ns()),
        renew_time: crate::lease::micro_time(now_ns()),
        transitions: 1,
        resource_version: "1".into(),
        uid: "uid-1".into(),
        metadata: serde_json::Value::Null,
    });
    for n in 2..12u64 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let mut held = api.held.lock().unwrap();
        let current = held.as_mut().unwrap();
        current.renew_time = crate::lease::micro_time(now_ns() + n as i64);
        current.resource_version = n.to_string();
    }
    assert!(
        !waiting.is_finished(),
        "the old member took the newer member's lease"
    );
    assert_eq!(
        api.held.lock().unwrap().as_ref().unwrap().holder,
        "bor-new_1"
    );
    waiting.abort();
}

/// ... but never leaves nobody to write: where no newer member takes the
/// free lease within one lease duration, the old member takes it -- a
/// writer at the old count is better than none.
#[tokio::test(start_paused = true)]
async fn a_standby_deferring_takes_a_lease_nobody_newer_took_within_its_duration() {
    let (api, mut elector, _tell, mut told) = standing_by(Duration::ZERO);
    let newer: Arc<NewerLook> = Arc::new(|| Ok(true));
    let began = tokio::time::Instant::now();
    let holds = stand_by(
        &mut elector,
        Duration::from_secs(1),
        "bor",
        &mut told,
        Some(newer),
    )
    .await;
    let waited = began.elapsed();
    assert!(holds);
    assert_eq!(api.held.lock().unwrap().as_ref().unwrap().holder, "bor-b_1");
    assert!(
        waited >= Duration::from_secs(6) && waited <= Duration::from_secs(8),
        "it took the lease after {waited:?}: one lease duration (6 s), and a look or two more at most"
    );
}

/// One Lease whose writes land and whose answers are lost: the API server
/// commits the update, and the client is told it failed (a timeout).
#[derive(Clone)]
struct LostAnswers {
    held: Arc<std::sync::Mutex<Option<crate::lease::Record>>>,
}

impl Leases for LostAnswers {
    async fn get(&self, _name: &str) -> Result<Option<crate::lease::Record>, String> {
        Ok(self.held.lock().unwrap().clone())
    }

    async fn create(
        &self,
        _name: &str,
        _record: &crate::lease::Record,
    ) -> Result<crate::lease::Wrote, String> {
        Ok(crate::lease::Wrote::Conflict)
    }

    async fn update(
        &self,
        _name: &str,
        record: &crate::lease::Record,
    ) -> Result<crate::lease::Wrote, String> {
        let mut held = self.held.lock().unwrap();
        let current = held.as_ref().unwrap();
        if current.resource_version != record.resource_version {
            return Ok(crate::lease::Wrote::Conflict);
        }
        let next = (current.resource_version.parse::<u64>().unwrap() + 1).to_string();
        let taking = !record.holder.is_empty();
        let mut record = record.clone();
        record.resource_version = next.clone();
        *held = Some(record);
        // A take's answer is lost; a release's is read.
        if taking {
            Err("the API server did not answer within 2 s".into())
        } else {
            Ok(crate::lease::Wrote::Done(next))
        }
    }
}

/// A standby whose take of a free lease landed while its answer was lost,
/// then told to stop: it gives up the lease that names it (the review's
/// L2), never stopping with the lease left to lapse.
#[tokio::test(start_paused = true)]
async fn a_stop_after_a_take_whose_answer_was_lost_gives_the_lease_up() {
    let api = LostAnswers {
        held: Arc::new(std::sync::Mutex::new(Some(crate::lease::Record {
            holder: String::new(),
            duration_seconds: 1,
            acquire_time: crate::lease::micro_time(now_ns()),
            renew_time: crate::lease::micro_time(now_ns()),
            transitions: 3,
            resource_version: "7".into(),
            uid: "uid-1".into(),
            metadata: serde_json::Value::Null,
        }))),
    };
    let mut elector = Elector::new(
        api.clone(),
        crate::lease::Settings {
            name: "release-bor".into(),
            holder: "bor-b_1".into(),
            renew: Duration::from_secs(2),
            duration: Duration::from_secs(6),
        },
    );
    let (tell, mut told) = tokio::sync::watch::channel(false);
    let waiting = tokio::spawn(async move {
        stand_by(&mut elector, Duration::from_secs(1), "bor", &mut told, None).await
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        api.held.lock().unwrap().as_ref().unwrap().holder,
        "bor-b_1",
        "the take had not landed"
    );
    tell.send(true).unwrap();
    assert!(!waiting.await.unwrap(), "told to stop, it went on to write");
    assert_eq!(
        api.held.lock().unwrap().as_ref().unwrap().holder,
        "",
        "it stopped with the lease naming it: nobody renews it until it lapses"
    );
}

/// A writer with no lease -- tolerant's, one member a group -- steps down
/// once its epoch is passed, as a leased one does (spec section 3, "a
/// frozen writer"; CI 43804841979, reconfigure, groups raised in
/// tolerant: the database moved to a new timeline as the groups' writers
/// started, group 0's writer raised every group's epoch, and group 1's,
/// fenced, refused every write for five minutes, ready all along). Never
/// while the epoch is its own.
#[tokio::test(start_paused = true)]
async fn a_writer_with_no_lease_steps_down_once_its_epoch_is_passed() {
    let holding = Holding {
        elector: None,
        component: "lake",
        why: "",
        previous: String::new(),
    };
    let head = Arc::new(AtomicU64::new(5));
    let (said, mut heard) = tokio::sync::mpsc::unbounded_channel::<String>();
    let reading = Arc::clone(&head);
    holding.keep_or(
        || 5,
        move || Ok(reading.load(Ordering::SeqCst)),
        move |_, why| {
            let _ = said.send(why.to_string());
        },
    );
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert!(
        heard.try_recv().is_err(),
        "a writer under its own epoch stepped down"
    );
    // Another writer raised it: a new timeline's, its group's takeover.
    head.store(6, Ordering::SeqCst);
    let why = tokio::time::timeout(Duration::from_secs(30), heard.recv())
        .await
        .expect("a writer with no lease, its epoch passed, never stepped down")
        .expect("said");
    assert!(
        why.contains("past this writer's 5"),
        "it stepped down saying {why:?}"
    );
}

/// A writer's periodic looks at its store -- its row among the members and,
/// with no lease, its head epoch, each every 2 s -- are made one at a time
/// (c30d-t7): made at once, the looks due together each took a thread of
/// the blocking pool, and the pool kept the extra ones and their memory
/// resident in every writer (the cluster's footprint median at its bound).
#[tokio::test(start_paused = true)]
async fn a_writers_periodic_looks_are_made_one_at_a_time() {
    let in_flight = Arc::new(AtomicU64::new(0));
    let most = Arc::new(AtomicU64::new(0));
    let looked = Arc::new(AtomicU64::new(0));
    let look = {
        let (in_flight, most, looked) = (in_flight.clone(), most.clone(), looked.clone());
        move || {
            let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            most.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(30));
            in_flight.fetch_sub(1, Ordering::SeqCst);
            looked.fetch_add(1, Ordering::SeqCst);
        }
    };
    let seen = look.clone();
    keep_seen("lake", "lake-0", "a revision".into(), move |_, _, _, _| {
        seen();
        Ok(())
    });
    let holding = Holding {
        elector: None,
        component: "lake",
        why: "",
        previous: String::new(),
    };
    holding.keep_or(
        || 5,
        move || {
            look();
            Ok(5)
        },
        |_, why| panic!("stepped down: {why}"),
    );
    tokio::time::sleep(Duration::from_secs(20)).await;
    assert!(
        looked.load(Ordering::SeqCst) >= 10,
        "both looked, again and again"
    );
    assert_eq!(
        most.load(Ordering::SeqCst),
        1,
        "two looks were made at once"
    );
}

/// One Lease whose first read is slow: asked at once, it reads the Lease
/// as the API server holds it `first_read_in` later, and answers then; the
/// reads after answer at once. Says when a take was written.
#[derive(Clone)]
struct SlowFirstRead {
    held: Arc<std::sync::Mutex<Option<crate::lease::Record>>>,
    first_read_in: Duration,
    read: Arc<AtomicBool>,
    taken_at: Arc<std::sync::Mutex<Option<Instant>>>,
}

impl Leases for SlowFirstRead {
    async fn get(&self, _name: &str) -> Result<Option<crate::lease::Record>, String> {
        if !self.read.swap(true, std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(self.first_read_in).await;
        }
        Ok(self.held.lock().unwrap().clone())
    }

    async fn create(
        &self,
        _name: &str,
        _record: &crate::lease::Record,
    ) -> Result<crate::lease::Wrote, String> {
        Ok(crate::lease::Wrote::Conflict)
    }

    async fn update(
        &self,
        _name: &str,
        record: &crate::lease::Record,
    ) -> Result<crate::lease::Wrote, String> {
        let mut held = self.held.lock().unwrap();
        let current = held.as_ref().expect("a Lease");
        if current.resource_version != record.resource_version {
            return Ok(crate::lease::Wrote::Conflict);
        }
        let mut record = record.clone();
        record.resource_version = format!("{}x", current.resource_version);
        *held = Some(record);
        *self.taken_at.lock().unwrap() = Some(Instant::now());
        Ok(crate::lease::Wrote::Done(
            held.as_ref().unwrap().resource_version.clone(),
        ))
    }
}

/// Review c5a10 M1: a store's standby holds a Lease seen when its read
/// answered, never when it was asked: its read slow across the writer's
/// renewal, it takes no sooner than the Lease's duration after that
/// renewal (spec section 3: "when the writer's Lease lapses"). On the real
/// clock, at a 1 s renew and a 3 s Lease.
#[tokio::test]
async fn a_store_standby_whose_read_was_slow_across_a_renewal_takes_no_sooner_than_its_lapse() {
    let record = |version: &str| crate::lease::Record {
        holder: "bor-a_1".into(),
        duration_seconds: 3,
        acquire_time: crate::lease::micro_time(now_ns()),
        renew_time: crate::lease::micro_time(now_ns()),
        transitions: 0,
        resource_version: version.into(),
        uid: "uid-1".into(),
        metadata: serde_json::Value::Null,
    };
    let api = SlowFirstRead {
        held: Arc::new(std::sync::Mutex::new(Some(record("1")))),
        first_read_in: Duration::from_millis(900),
        read: Arc::default(),
        taken_at: Arc::default(),
    };
    let settings = crate::lease::Settings {
        name: "release-bor".into(),
        holder: "bor-b_1".into(),
        renew: Duration::from_secs(1),
        duration: Duration::from_secs(3),
    };
    let look_every = settings.look_every();
    let mut elector = store_elector(api.clone(), settings);
    let (_tell, mut told) = tokio::sync::watch::channel(false);
    let waiting =
        tokio::spawn(
            async move { stand_by(&mut elector, look_every, "bor", &mut told, None).await },
        );
    // The writer renews while the standby's first read is out, 100 ms
    // before that read reads the Lease, and then renews no more.
    tokio::time::sleep(Duration::from_millis(800)).await;
    *api.held.lock().unwrap() = Some(record("2"));
    let renewed = Instant::now();
    assert!(
        waiting.await.unwrap(),
        "the standby never took the lapsed Lease"
    );
    let taken = api.taken_at.lock().unwrap().expect("a take written");
    assert!(
        taken.duration_since(renewed) >= Duration::from_secs(3),
        "taken {:?} after the writer's renewal; the Lease lasts 3 s",
        taken.duration_since(renewed)
    );
}

/// One Lease as the API server keeps it, each write conditional on the
/// version read, with the API server's failures put in by the test, each
/// counted down as it is met: reads that fail, writes that fail unwritten,
/// writes never answered and never written, and writes written whose
/// answer never comes (a request in flight past its bound). Says when each
/// write landed.
#[derive(Clone, Default)]
struct Unreliable {
    held: Arc<std::sync::Mutex<Option<crate::lease::Record>>>,
    reads_failing: Arc<AtomicU64>,
    writes_failing: Arc<AtomicU64>,
    writes_unanswered: Arc<AtomicU64>,
    answers_lost: Arc<AtomicU64>,
    /// Writes that land `late_by` after they were sent, past their bound,
    /// and are never answered.
    writes_late: Arc<AtomicU64>,
    late_by: Duration,
    /// How long a read's answer and a write's request each take.
    latency: Duration,
    landed: Arc<std::sync::Mutex<Vec<tokio::time::Instant>>>,
}

/// Whether one more of `failures` is met now, counting it down.
fn met(failures: &AtomicU64) -> bool {
    failures
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
            left.checked_sub(1)
        })
        .is_ok()
}

impl Leases for Unreliable {
    async fn get(&self, _name: &str) -> Result<Option<crate::lease::Record>, String> {
        if met(&self.reads_failing) {
            return Err("error sending request for url".into());
        }
        let read = self.held.lock().unwrap().clone();
        tokio::time::sleep(self.latency).await;
        Ok(read)
    }

    async fn create(
        &self,
        _name: &str,
        record: &crate::lease::Record,
    ) -> Result<crate::lease::Wrote, String> {
        let mut held = self.held.lock().unwrap();
        if held.is_some() {
            return Ok(crate::lease::Wrote::Conflict);
        }
        let mut record = record.clone();
        record.resource_version = "1".into();
        *held = Some(record);
        self.landed
            .lock()
            .unwrap()
            .push(tokio::time::Instant::now());
        Ok(crate::lease::Wrote::Done("1".into()))
    }

    async fn update(
        &self,
        _name: &str,
        record: &crate::lease::Record,
    ) -> Result<crate::lease::Wrote, String> {
        if met(&self.writes_failing) {
            return Err("error sending request for url".into());
        }
        if met(&self.writes_unanswered) {
            tokio::time::sleep(Duration::from_secs(60)).await;
            return Err("operation timed out".into());
        }
        tokio::time::sleep(self.latency).await;
        if met(&self.writes_late) {
            let (late, record) = (self.clone(), record.clone());
            tokio::spawn(async move {
                tokio::time::sleep(late.late_by).await;
                let _ = late.write(&record);
            });
            tokio::time::sleep(Duration::from_secs(60)).await;
            return Err("operation timed out".into());
        }
        let Some(next) = self.write(record) else {
            return Ok(crate::lease::Wrote::Conflict);
        };
        if met(&self.answers_lost) {
            tokio::time::sleep(Duration::from_secs(60)).await;
            return Err("operation timed out".into());
        }
        Ok(crate::lease::Wrote::Done(next))
    }
}

impl Unreliable {
    /// The write, conditional on its version: the version now, or None.
    fn write(&self, record: &crate::lease::Record) -> Option<String> {
        let next = {
            let mut held = self.held.lock().unwrap();
            let current = held.as_ref()?;
            if current.resource_version != record.resource_version {
                return None;
            }
            let next = (current.resource_version.parse::<u64>().unwrap() + 1).to_string();
            let mut record = record.clone();
            record.resource_version = next.clone();
            *held = Some(record);
            next
        };
        self.landed
            .lock()
            .unwrap()
            .push(tokio::time::Instant::now());
        Some(next)
    }

    fn holder(&self) -> String {
        self.held.lock().unwrap().as_ref().unwrap().holder.clone()
    }
}

/// A writer of `api`'s Lease, renewing every 2 s a Lease of 6 s (the
/// chart's defaults), holding it.
async fn holding_on(api: &Unreliable) -> Elector<Unreliable> {
    let mut elector = Elector::new(
        api.clone(),
        crate::lease::Settings {
            name: "release-bor".into(),
            holder: "bor-a_1".into(),
            renew: Duration::from_secs(2),
            duration: Duration::from_secs(6),
        },
    );
    assert_eq!(
        elector.look(Instant::now(), now_ns()).await,
        Ok(Standing::Held)
    );
    elector
}

/// One renewal the API server never answers is followed by the next at
/// once, so the Lease is never a whole duration unrenewed under a writer
/// serving (the spec's section 2: Patroni's rule, loop_wait + 2 x
/// retry_timeout <= ttl). Tried `renew` after the one that gave up, the
/// next landed a whole Lease after the last, and a standby looking took it
/// as lapsed (c5c-ci1's local reconfigure on 9479a69c: the conductor's
/// writer fbt2r, a renewal unanswered at 01:32:03, its next refused at
/// 01:32:06 by its standby's take, recorded "the writer's lease lapsed").
#[tokio::test(start_paused = true)]
async fn a_renewal_never_answered_is_followed_at_once_and_the_lease_never_goes_its_duration_unrenewed(
) {
    let api = Unreliable::default();
    let elector = holding_on(&api).await;
    api.writes_unanswered.store(1, Ordering::SeqCst);
    let stepped = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let said = Arc::clone(&stepped);
    let renewing = tokio::spawn(renew_while_writing(
        Arc::new(tokio::sync::Mutex::new(elector)),
        "bor",
        move |_, why| said.lock().unwrap().push(why.to_string()),
    ));
    tokio::time::sleep(Duration::from_secs(20)).await;
    renewing.abort();
    assert_eq!(*stepped.lock().unwrap(), Vec::<String>::new());
    let landed = api.landed.lock().unwrap().clone();
    assert!(landed.len() >= 6, "renewed {} times in 20 s", landed.len());
    let longest = landed.windows(2).map(|w| w[1] - w[0]).max().unwrap();
    assert!(
        longest < Duration::from_secs(6),
        "the Lease went {longest:?} between renewals that landed: its whole duration, so a \
         standby takes it as lapsed under a writer serving"
    );
}

/// A planned stop gives the Lease up although the renewal before it
/// failed: what that look left seen is never what a release rests on
/// (reconfigure, "ha databases added": the book's group-1 writer, stopped at
/// 01:31:39, still named on its Lease at 01:32:04, the API server failing
/// reads from 01:32:02; its standby took it as lapsed at 01:32:11).
#[tokio::test(start_paused = true)]
async fn a_planned_stop_gives_the_lease_up_after_a_renewal_that_failed() {
    let api = Unreliable::default();
    let mut elector = holding_on(&api).await;
    api.reads_failing.store(1, Ordering::SeqCst);
    assert!(elector.look(Instant::now(), now_ns()).await.is_err());
    release_on_stop(&tokio::sync::Mutex::new(elector), "bor").await;
    assert_eq!(
        api.holder(),
        "",
        "the stop left its Lease to lapse: its standby waits it out"
    );
}

/// The same where the renewal before it was written and its answer lost,
/// as a renewal cut at its bound is: the release is written on what is
/// there now, never on the version that renewal left seen.
#[tokio::test(start_paused = true)]
async fn a_planned_stop_gives_the_lease_up_after_a_renewal_whose_answer_was_lost() {
    let api = Unreliable::default();
    let mut elector = holding_on(&api).await;
    api.answers_lost.store(1, Ordering::SeqCst);
    assert!(tokio::time::timeout(
        Duration::from_secs(2),
        elector.look(Instant::now(), now_ns())
    )
    .await
    .is_err());
    release_on_stop(&tokio::sync::Mutex::new(elector), "bor").await;
    assert_eq!(
        api.holder(),
        "",
        "the stop left its Lease to lapse: its standby waits it out"
    );
}

/// A planned stop whose release the API server failed tries it again while
/// the Lease lasts: one failed request never leaves the Lease to lapse.
#[tokio::test(start_paused = true)]
async fn a_planned_stop_whose_release_failed_tries_it_again() {
    let api = Unreliable::default();
    let elector = holding_on(&api).await;
    api.writes_failing.store(1, Ordering::SeqCst);
    release_on_stop(&tokio::sync::Mutex::new(elector), "bor").await;
    assert_eq!(
        api.holder(),
        "",
        "the stop left its Lease to lapse: its standby waits it out"
    );
}

/// A planned stop never clears a Lease another member took, and gives up
/// trying within the Lease's duration where the API server never answers:
/// past it, the Lease has lapsed on its own.
#[tokio::test(start_paused = true)]
async fn a_planned_stop_never_clears_a_lease_another_took_and_ends_within_the_lease() {
    let api = Unreliable::default();
    let elector = holding_on(&api).await;
    {
        let mut held = api.held.lock().unwrap();
        let current = held.as_mut().unwrap();
        current.holder = "bor-c_1".into();
        current.resource_version = "9".into();
    }
    release_on_stop(&tokio::sync::Mutex::new(elector), "bor").await;
    assert_eq!(
        api.holder(),
        "bor-c_1",
        "a Lease another member took was cleared"
    );

    let api = Unreliable::default();
    let elector = holding_on(&api).await;
    api.writes_unanswered.store(1_000, Ordering::SeqCst);
    let began = tokio::time::Instant::now();
    release_on_stop(&tokio::sync::Mutex::new(elector), "bor").await;
    assert!(
        began.elapsed() <= Duration::from_secs(6),
        "it waited {:?} to give the Lease up",
        began.elapsed()
    );
}

/// A writer stepping down whose first read of its Lease failed tries again
/// within its bound ([`STEP_DOWN_RELEASE`]): one failed request never
/// leaves the Lease to lapse.
#[tokio::test(start_paused = true)]
async fn a_writer_stepping_down_whose_first_read_failed_gives_its_lease_up_within_the_bound() {
    let api = Unreliable::default();
    let elector = holding_on(&api).await;
    api.reads_failing.store(1, Ordering::SeqCst);
    let began = tokio::time::Instant::now();
    let_go_stepping_down(&tokio::sync::Mutex::new(elector), "bor").await;
    assert!(began.elapsed() <= STEP_DOWN_RELEASE);
    assert_eq!(
        api.holder(),
        "",
        "the step-down left its Lease to lapse: its standby waits it out"
    );
}

/// A renewal cut at its bound whose write lands late, between the next
/// renewal's read and its write, refuses that write as written meanwhile:
/// the Lease still names this writer, which renews on, never stepping down
/// as though another held it ("another member (<its own name>) holds the
/// lease": the conductor's writer in c5c-ci1's local reconfigure).
#[tokio::test(start_paused = true)]
async fn a_renewal_landing_late_is_no_other_holder_and_the_writer_renews_on() {
    let api = Unreliable {
        latency: Duration::from_millis(300),
        late_by: Duration::from_millis(1_900),
        ..Unreliable::default()
    };
    let elector = holding_on(&api).await;
    api.writes_late.store(1, Ordering::SeqCst);
    let stepped = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let said = Arc::clone(&stepped);
    let renewing = tokio::spawn(renew_while_writing(
        Arc::new(tokio::sync::Mutex::new(elector)),
        "bor",
        move |_, why| said.lock().unwrap().push(why.to_string()),
    ));
    tokio::time::sleep(Duration::from_secs(20)).await;
    renewing.abort();
    assert_eq!(*stepped.lock().unwrap(), Vec::<String>::new());
    assert_eq!(api.holder(), "bor-a_1");
    let landed = api.landed.lock().unwrap().clone();
    let longest = landed.windows(2).map(|w| w[1] - w[0]).max().unwrap();
    assert!(
        longest < Duration::from_secs(6),
        "the Lease went {longest:?} between renewals that landed"
    );
}
