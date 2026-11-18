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
/// Its changes are chained, as the book's journal is, and a catch-up reads
/// one to a page.
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
    /// Nothing answered, `control` included: the database failing over.
    down: bool,
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
        let (lost_from, epoch) = {
            let shape = self.shape.lock().unwrap();
            (shape.lost_from, shape.epoch)
        };
        let body = |sequence: u64| vec![if sequence >= lost_from { 2 } else { 1 }; 16];
        let mut digest = Vec::new();
        for at in 1..=sequence {
            digest = meridian_changes::integrity::chained(&digest, &body(at));
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
                let answered = match shape.script.pop_front() {
                    Some(answered) => answered,
                    None => !shape.refusing.contains(partition),
                };
                if !answered {
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

/// A book reader of `record`, started at `start`, ready while its lag is
/// under a second, once the writer has its interest and has been asked.
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
    meridian_changes::serve(&bus, "book", record.clone());
    let applied = Arc::new(Applied::default());
    let member = Member::reader_of("bor", "book", Duration::from_secs(1));
    let followed =
        crate::member::Followed::from_heads(start.iter().map(|(p, h)| (p.to_string(), *h)));
    member
        .read(&bus, followed.clone(), applied.clone(), record.primary())
        .unwrap();
    until("the reader's interest", || {
        meridian_changes::followed(&bus, "book")
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
