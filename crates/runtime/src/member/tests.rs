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
