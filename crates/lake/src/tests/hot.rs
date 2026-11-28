//! The lake's hot layer (step 3a): the same answers as the record, from
//! memory once a key is loaded, under any bound, and after a restart.

use std::sync::Arc;

use meridian_domain::v1::QualityState;

use super::*;
use crate::hot::{noted, Bounds, HotStore};

const SUBJECTS: [&str; 3] = [BTC, USDC, AAPL];

fn quote_of(subject: &str, row_key: &str, at: i64, bid: &str) -> Observation {
    let mut quote = quote(row_key, at, money(bid, "USD", ""), money("200", "USD", ""));
    quote.meta.as_mut().unwrap().subjects[0].entity_id = subject.into();
    Observation::Quote(Box::new(quote))
}

fn latest(lake: &Lake, subjects: &[&str]) -> (String, bool) {
    tokio::task::block_in_place(|| {
        noted::begin();
        let answer = lake
            .read(
                ReadRequest {
                    data_type: DataType::Quote,
                    subjects: subjects
                        .iter()
                        .map(|s| SubjectRef {
                            entity_id: (*s).into(),
                            ..Default::default()
                        })
                        .collect(),
                    kinds: vec![],
                    interval_ns: 0,
                    sources: None,
                    at_ns: 0,
                    business_date: String::new(),
                    valid_from_ns: 0,
                    valid_until_ns: 0,
                    as_of_ns: 0,
                    page_size: 0,
                    cursor: String::new(),
                    after: None,
                    tenors: Vec::new(),
                    published_as_of_ns: 0,
                    series_keys: Vec::new(),
                    all_series: false,
                    dates: None,
                    window: Default::default(),
                    fields: Vec::new(),
                },
                &envelope("signal-1", true, ""),
            )
            .unwrap();
        let (from_memory, _) = noted::take();
        (
            format!(
                "{:?}",
                (
                    &answer.rows,
                    &answer.unanswered,
                    &answer.watermark,
                    &answer.next_cursor
                )
            ),
            from_memory,
        )
    })
}

/// A ticking stream, with what changes a key's latest otherwise: a quote
/// out of order, an invalid one, a restatement.
fn tick(hot: &Lake, round: i64) {
    let mut rows = Vec::new();
    for (i, subject) in SUBJECTS.iter().enumerate() {
        let at = NOW + round * 1_000 + i as i64;
        rows.push(quote_of(
            subject,
            &format!("{subject}-{round}"),
            at,
            &format!("{}.{}", 100 + round, i),
        ));
        if round % 3 == 0 {
            // Late: older than what the key holds.
            rows.push(quote_of(
                subject,
                &format!("{subject}-late-{round}"),
                at - 500,
                "99",
            ));
        }
        if round % 4 == 1 {
            let mut bad = quote_of(subject, &format!("{subject}-bad-{round}"), at + 1, "1");
            bad.meta_mut().quality = QualityState::Invalid as i32;
            rows.push(bad);
        }
        if round % 5 == 2 && round > 0 {
            // A restatement of an earlier tick: its value, never its valid
            // time (the lake's rule 5).
            rows.push(quote_of(
                subject,
                &format!("{subject}-{}", round - 1),
                at - 1_000,
                "123",
            ));
        }
    }
    tokio::task::block_in_place(|| {
        hot.record("quotes", rows, "", &envelope("coinbase-1", true, ""))
    })
    .unwrap();
}

fn hot_lake(h: &Harness, inner: Arc<dyn Store>, bounds: Bounds) -> (Arc<Lake>, Arc<HotStore>) {
    let hot = HotStore::new(inner, bounds);
    let lake = tokio::task::block_in_place(|| {
        Lake::new(
            h.bus.clone(),
            hot.clone() as Arc<dyn Store>,
            h.clock.clone(),
        )
    });
    lake.configured(&live_configuration());
    (lake, hot)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_hot_layer_answers_the_same_as_the_record_from_memory_once_loaded() {
    let h = harness();
    h.lake.configured(&live_configuration());
    let (hot_lake, hot) = hot_lake(&h, h.store.clone(), Bounds::DEFAULT);
    let asked: [&[&str]; 4] = [&[BTC], &[USDC, AAPL], &SUBJECTS, &[AAPL, BTC]];
    let mut from_memory = 0;
    for round in 0..24 {
        tick(&hot_lake, round);
        for subjects in asked {
            let (record, _) = latest(&h.lake, subjects);
            let (answered, memory) = latest(&hot_lake, subjects);
            assert_eq!(answered, record, "round {round}, {subjects:?}");
            from_memory += usize::from(memory);
        }
        hot.load_wanted();
    }
    assert!(
        from_memory > 40,
        "memory answered {from_memory} of 96 reads"
    );
    let ((keys, bytes), _) = hot.held();
    assert_eq!(keys, 3);
    assert!(bytes > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn evicted_under_a_small_bound_it_still_answers_the_same() {
    let h = harness();
    h.lake.configured(&live_configuration());
    let tiny = Bounds {
        latest_bytes: 3_000,
        listings_bytes: 1_000,
    };
    let (hot_lake, hot) = hot_lake(&h, h.store.clone(), tiny);
    for round in 0..16 {
        tick(&hot_lake, round);
        hot.load_wanted();
        for subjects in [&[BTC][..], &[USDC], &SUBJECTS] {
            assert_eq!(latest(&hot_lake, subjects).0, latest(&h.lake, subjects).0);
        }
        let ((_, bytes), _) = hot.held();
        assert!(
            bytes <= tiny.latest_bytes,
            "{bytes} bytes held past the bound"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_lake_answers_from_the_record_until_loaded_then_from_memory_equally() {
    let h = harness();
    h.lake.configured(&live_configuration());
    let (before, _) = hot_lake(&h, h.store.clone(), Bounds::DEFAULT);
    for round in 0..10 {
        tick(&before, round);
    }
    // The restart: a new layer over the same record, holding nothing.
    let (after, hot) = hot_lake(&h, h.store.clone(), Bounds::DEFAULT);
    let record = latest(&h.lake, &SUBJECTS).0;
    let (cold, memory) = latest(&after, &SUBJECTS);
    assert!(!memory, "nothing loaded yet: the record answers");
    assert_eq!(cold, record);
    // Under load: batches committed while the keys load are applied above
    // the watermark each key was loaded at.
    tick(&after, 10);
    hot.load_wanted();
    tick(&after, 11);
    let record = latest(&h.lake, &SUBJECTS).0;
    let (warm, memory) = latest(&after, &SUBJECTS);
    assert!(memory, "loaded: memory answers");
    assert_eq!(warm, record);
}

/// How many of the listing's counts' reads the record has begun, each
/// taking `long` and then failing where `fails`.
fn counted_reads(
    record: &MemoryStore,
    long: std::time::Duration,
    fails: bool,
) -> Arc<std::sync::atomic::AtomicUsize> {
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let begun = reads.clone();
    record.on_counts_read(move |_| {
        begun.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(long);
        if fails {
            return Err(crate::store::StoreError::Unavailable(
                "the count was not read".into(),
            ));
        }
        Ok(())
    });
    reads
}

/// `askers` asking `ask` of the hot layer at once, each one's answer.
fn asked_at_once<T: Send + 'static>(
    hot: &Arc<HotStore>,
    askers: usize,
    ask: fn(&HotStore) -> crate::store::Result<T>,
) -> Vec<crate::store::Result<T>> {
    let together = Arc::new(std::sync::Barrier::new(askers));
    let threads: Vec<_> = (0..askers)
        .map(|_| {
            let (hot, together) = (hot.clone(), together.clone());
            std::thread::spawn(move || {
                together.wait();
                ask(&hot)
            })
        })
        .collect();
    threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect()
}

/// The rows in force the quality counts count, every dataset's.
fn in_force(counts: &std::collections::BTreeMap<String, (u64, u64, u64)>) -> u64 {
    counts
        .values()
        .map(|count| count.0 + count.1 + count.2)
        .sum()
}

/// The datasets' listing's counts, one read at a time each (dev,
/// 2026-11-28: each read of one names no month and opens every month table,
/// about 750 MB of the database's memory a copy, and 2 to 8 ran at once):
/// askers that find none held while one is read wait for its answer, never
/// reading it again. Each read it itself before.
#[test]
fn askers_of_a_count_not_held_wait_for_the_one_read_of_it() {
    const ASKERS: usize = 8;
    let record = Arc::new(MemoryStore::new());
    let reads = counted_reads(&record, std::time::Duration::from_millis(200), false);
    let hot = HotStore::new(record.clone() as Arc<dyn Store>, Bounds::DEFAULT);
    hot.record("quotes", vec![quote_of(BTC, "a", NOW, "100")], NOW)
        .unwrap();

    let read = |so_far: usize| reads.load(std::sync::atomic::Ordering::SeqCst) - so_far;
    for answer in asked_at_once(&hot, ASKERS, |hot| hot.unconverted_counts()) {
        answer.expect("each is answered");
    }
    assert_eq!(read(0), 1, "the unconverted counts, read once for all");
    for answer in asked_at_once(&hot, ASKERS, |hot| hot.closed_day_counts()) {
        answer.expect("each is answered");
    }
    assert_eq!(read(1), 1, "the closed-day counts, read once for all");
    for answer in asked_at_once(&hot, ASKERS, |hot| hot.quality_counts()) {
        assert_eq!(in_force(&answer.expect("each is answered")), 1);
    }
    assert_eq!(read(2), 1, "the quality counts, read once for all");
}

/// A read of a count that fails is not read again by each asker that
/// waited for it: they are refused for load with a retry-after, and the
/// one that read it says why it failed. Each read it, and failed, before.
#[test]
fn askers_waiting_for_a_count_whose_read_fails_are_told_to_ask_again() {
    const ASKERS: usize = 8;
    let record = Arc::new(MemoryStore::new());
    let reads = counted_reads(&record, std::time::Duration::from_millis(200), true);
    let hot = HotStore::new(record.clone() as Arc<dyn Store>, Bounds::DEFAULT);

    let answers = asked_at_once(&hot, ASKERS, |hot| hot.quality_counts());
    assert_eq!(
        reads.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "read once, by one of them"
    );
    let mut failed = 0;
    let mut again = 0;
    for answer in answers {
        let refused = answer.expect_err("none is answered").on_the_bus();
        match meridian_bus::refusal_retry_after(&refused) {
            Some(seconds) => {
                assert!(seconds >= 1, "{refused}");
                again += 1;
            }
            None => {
                assert!(refused.contains("the count was not read"), "{refused}");
                failed += 1;
            }
        }
    }
    assert_eq!((failed, again), (1, ASKERS - 1));

    // The next to ask reads it, once.
    hot.quality_counts().expect_err("it fails still");
    assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 2);
}

/// A batch committed leaves the counts held, behind: while one asker reads
/// them again, the others are answered what is held, and a batch committed
/// meanwhile starts no second read -- the answer that read brings is held
/// behind in its turn, and the next asker reads once more. A commit let
/// them go before, and every asker until one landed read them itself.
#[test]
fn a_count_held_and_behind_is_answered_while_one_read_brings_it_up_to_date() {
    use std::sync::atomic::Ordering;
    let record = Arc::new(MemoryStore::new());
    let hot = HotStore::new(record.clone() as Arc<dyn Store>, Bounds::DEFAULT);
    hot.record("quotes", vec![quote_of(BTC, "a", NOW, "100")], NOW)
        .unwrap();
    assert_eq!(in_force(&hot.quality_counts().unwrap()), 1, "held now");
    hot.record("quotes", vec![quote_of(BTC, "b", NOW + 1, "101")], NOW)
        .unwrap();

    // The record's count waits, once begun, to be let go on.
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (begun, has_begun) = std::sync::mpsc::channel::<()>();
    let (go_on, may_go_on) = std::sync::mpsc::channel::<()>();
    let (begun, may_go_on) = (
        std::sync::Mutex::new(begun),
        std::sync::Mutex::new(may_go_on),
    );
    let counted = reads.clone();
    record.on_counts_read(move |_| {
        counted.fetch_add(1, Ordering::SeqCst);
        begun.lock().unwrap().send(()).unwrap();
        may_go_on
            .lock()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("let go on");
        Ok(())
    });
    let reader = {
        let hot = hot.clone();
        std::thread::spawn(move || hot.quality_counts())
    };
    has_begun
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("the one read begins");

    // A batch commits while it reads, and three ask: each is answered what
    // is held, from memory, and none reads.
    hot.record("quotes", vec![quote_of(BTC, "c", NOW + 2, "102")], NOW)
        .unwrap();
    for _ in 0..3 {
        noted::begin();
        let answer = hot.quality_counts().expect("answered what is held");
        assert_eq!(in_force(&answer), 1, "the count as it was last read");
        assert_eq!(noted::take(), (true, "catalogue"));
    }
    assert_eq!(
        reads.load(Ordering::SeqCst),
        1,
        "one read, the first asker's"
    );

    go_on.send(()).unwrap();
    assert_eq!(in_force(&reader.join().unwrap().unwrap()), 3);

    // What it brought is behind the batch committed while it read: the
    // next asker reads once more, and then it is held.
    go_on.send(()).unwrap();
    assert_eq!(in_force(&hot.quality_counts().unwrap()), 3);
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    assert_eq!(in_force(&hot.quality_counts().unwrap()), 3);
    assert_eq!(reads.load(Ordering::SeqCst), 2, "held: no third read");
}

/// The record's reads of the listing's counts, each waiting once begun to
/// be let go on: how many began, each one's beginning, and the letting go.
fn gated_reads(
    record: &MemoryStore,
) -> (
    Arc<std::sync::atomic::AtomicUsize>,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (begun, has_begun) = std::sync::mpsc::channel::<()>();
    let (go_on, may_go_on) = std::sync::mpsc::channel::<()>();
    let (begun, may_go_on) = (
        std::sync::Mutex::new(begun),
        std::sync::Mutex::new(may_go_on),
    );
    let counted = reads.clone();
    record.on_counts_read(move |_| {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        begun.lock().unwrap().send(()).unwrap();
        may_go_on
            .lock()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("let go on");
        Ok(())
    });
    (reads, has_begun, go_on)
}

/// What is held further behind a commit than it may be answered
/// ([`crate::hot::COUNTS_BEHIND_AT_MOST`]) is not answered while it is read
/// again: its asker waits for that read's answer, and reads nothing itself.
#[test]
fn a_count_held_further_behind_than_it_may_be_waits_for_the_read_of_it() {
    use std::sync::atomic::Ordering;
    let record = Arc::new(MemoryStore::new());
    let hot = HotStore::new(record.clone() as Arc<dyn Store>, Bounds::DEFAULT);
    hot.record("quotes", vec![quote_of(BTC, "a", NOW, "100")], NOW)
        .unwrap();
    assert_eq!(in_force(&hot.quality_counts().unwrap()), 1, "held now");
    hot.record("quotes", vec![quote_of(BTC, "b", NOW + 1, "101")], NOW)
        .unwrap();

    let (reads, has_begun, go_on) = gated_reads(&record);
    let reader = {
        let hot = hot.clone();
        std::thread::spawn(move || hot.quality_counts())
    };
    has_begun
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("the one read begins");
    // Behind at all is too far behind, for this asker.
    let asker = {
        let hot = hot.clone();
        std::thread::spawn(move || hot.quality_counts_within(None, std::time::Duration::ZERO))
    };
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(!asker.is_finished(), "it waits, answered nothing held");

    go_on.send(()).unwrap();
    assert_eq!(in_force(&reader.join().unwrap().unwrap()), 2);
    assert_eq!(in_force(&asker.join().unwrap().unwrap()), 2);
    assert_eq!(reads.load(Ordering::SeqCst), 1, "one read for both");
}

/// A request that waits for another's read of a count waits no longer than
/// its own wait (section 10a, overload answered): then it is refused for
/// load with a retry-after, and reads nothing itself.
#[test]
fn an_asker_whose_wait_ends_before_the_read_of_a_count_is_told_to_ask_again() {
    use std::sync::atomic::Ordering;
    let record = Arc::new(MemoryStore::new());
    let hot = HotStore::new(record.clone() as Arc<dyn Store>, Bounds::DEFAULT);
    let (reads, has_begun, go_on) = gated_reads(&record);
    let reader = {
        let hot = hot.clone();
        std::thread::spawn(move || hot.quality_counts())
    };
    has_begun
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("the one read begins");

    let began = std::time::Instant::now();
    let wait = std::time::Duration::from_millis(60);
    let refused = hot
        .quality_counts_within(Some(began + wait), crate::hot::COUNTS_BEHIND_AT_MOST)
        .expect_err("no answer came by the end of its wait")
        .on_the_bus();
    assert!(began.elapsed() >= wait);
    assert!(
        meridian_bus::refusal_retry_after(&refused).is_some_and(|seconds| seconds >= 1),
        "{refused}"
    );
    assert_eq!(reads.load(Ordering::SeqCst), 1, "it read nothing itself");

    go_on.send(()).unwrap();
    reader.join().unwrap().expect("the one read answers");
    hot.quality_counts().expect("held now");
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}
