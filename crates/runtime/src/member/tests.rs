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
