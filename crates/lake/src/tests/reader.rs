//! A reader member's store (contract v30, stage B): latest reads from a hot
//! layer the change log keeps current, the record's past from a read replica
//! that has reached the read's `read_after`, everything from the primary
//! past the read's wait, nothing written, and each reply's watermark the
//! position it was answered at.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meridian_bus::read_after::{AllHeads, Position, Reads};
use meridian_pb::v1::{PartitionSequence, Watermark};

use super::*;
use crate::hot::{Bounds, HotStore};
use crate::reader::ReaderStore;
use crate::store::{Query, When};

const LIVE: &str = "coinbase-1:live";

fn quote_at(row_key: &str, at: i64, bid: &str) -> Observation {
    Observation::Quote(Box::new(quote(
        row_key,
        at,
        money(bid, "USD", ""),
        money("200", "USD", ""),
    )))
}

fn latest_query(as_of_ns: i64) -> Query {
    Query {
        datasets: vec![LIVE.into()],
        subjects: vec![BTC.into()],
        data_type: DataType::Quote,
        kinds: Vec::new(),
        interval_ns: 0,
        when: When::Latest { at_ns: 0 },
        as_of_ns,
        published_as_of_ns: 0,
        latest_only: true,
    }
}

fn bids(rows: &[Observation]) -> Vec<Option<Decimal>> {
    rows.iter()
        .map(|row| match row {
            Observation::Quote(quote) => quote.bid.clone().and_then(|bid| bid.amount),
            _ => None,
        })
        .collect()
}

fn bid(text: &str) -> Vec<Option<Decimal>> {
    text.split(',').map(decimal).collect()
}

fn mark(sequence: u64) -> Watermark {
    Watermark {
        partitions: vec![PartitionSequence {
            partition: LIVE.into(),
            sequence,
        }],
    }
}

/// A reader whose replies' watermarks these tests do not read.
fn nothing_applied() -> crate::reader::Applied {
    Arc::new(std::collections::BTreeMap::new)
}

/// A member at a fixed position.
struct At(u64);

impl Position for At {
    fn heads(&self, _: &[String]) -> std::result::Result<HashMap<String, u64>, String> {
        Ok([(LIVE.to_string(), self.0)].into())
    }
}

#[test]
fn a_readers_hot_layer_is_kept_current_by_the_writers_change_log() {
    let primary: Arc<dyn Store> = Arc::new(MemoryStore::new());
    primary
        .record(LIVE, vec![quote_at("q1", NOW, "100")], NOW)
        .unwrap();
    let hot = HotStore::new(Arc::clone(&primary), Bounds::DEFAULT);
    let reader = ReaderStore::new(
        Arc::clone(&hot),
        Arc::clone(&primary),
        None,
        nothing_applied(),
    );
    assert_eq!(bids(&reader.read(&latest_query(0)).unwrap()), bid("100"));
    hot.load_wanted();
    let head = primary.heads().unwrap()[LIVE];
    // The writer commits; the reader holds what it held until it follows.
    primary
        .record(LIVE, vec![quote_at("q2", NOW + 1_000, "101")], NOW)
        .unwrap();
    assert_eq!(bids(&reader.read(&latest_query(0)).unwrap()), bid("100"));
    let changes = primary
        .read_changes(LIVE, head, meridian_changes::PAGE_MOST)
        .unwrap()
        .changes;
    assert!(!changes.is_empty());
    hot.follow(LIVE, &changes);
    let after = bids(&reader.read(&latest_query(0)).unwrap());
    assert!(after.contains(&decimal("101")), "{after:?}");
    // A partition it did not follow: dropped, and loaded again.
    hot.forget_dataset(LIVE);
    let again = bids(&reader.read(&latest_query(0)).unwrap());
    assert!(again.contains(&decimal("101")), "{again:?}");
}

#[test]
fn a_reader_writes_nothing() {
    let primary: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let hot = HotStore::new(Arc::clone(&primary), Bounds::DEFAULT);
    let reader = ReaderStore::new(hot, Arc::clone(&primary), None, nothing_applied());
    assert!(reader
        .record(LIVE, vec![quote_at("q1", NOW, "100")], NOW)
        .is_err());
    assert!(reader.keep_alias("a", "b", NOW).is_err());
    assert!(reader.count_miss("x", NOW).is_err());
    assert!(primary.heads().unwrap().get(LIVE).is_none_or(|h| *h == 0));
}

#[test]
fn the_records_past_is_the_replicas_once_it_has_reached_the_read_else_the_primarys() {
    let primary: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let replica: Arc<dyn Store> = Arc::new(MemoryStore::new());
    for store in [&primary, &replica] {
        store
            .record(LIVE, vec![quote_at("q1", NOW, "100")], NOW)
            .unwrap();
    }
    // The primary is one row ahead of its replica.
    primary
        .record(LIVE, vec![quote_at("q2", NOW + 1_000, "101")], NOW)
        .unwrap();
    let ahead = primary.heads().unwrap()[LIVE];
    let behind = replica.heads().unwrap()[LIVE];
    assert!(ahead > behind);
    let hot = HotStore::new(Arc::clone(&primary), Bounds::DEFAULT);
    let reader = ReaderStore::new(
        hot,
        Arc::clone(&primary),
        Some(Arc::clone(&replica)),
        nothing_applied(),
    );
    let past = latest_query(NOW + 10_000_000_000);
    let reads = Reads::default();
    reads.keep("lake", Arc::new(At(ahead)));
    // Naming nothing: the replica answers the record's past.
    assert_eq!(bids(&reader.read(&past).unwrap()), bid("100"));
    // Naming what the replica has reached: the replica.
    let answered = reads
        .answer("platform.lake.query.list-quotes", &mark(behind), || {
            reader.read(&past).map_err(|failed| failed.to_string())
        })
        .unwrap();
    assert_eq!(bids(&answered), bid("100"));
    // Naming past it: the primary, never the replica behind it.
    let answered = reads
        .answer("platform.lake.query.list-quotes", &mark(ahead), || {
            reader.read(&past).map_err(|failed| failed.to_string())
        })
        .unwrap();
    assert_eq!(bids(&answered), bid("100,101"));
}

#[test]
fn past_its_wait_a_readers_read_is_the_primarys_every_part() {
    let primary: Arc<dyn Store> = Arc::new(MemoryStore::new());
    primary
        .record(LIVE, vec![quote_at("q1", NOW, "100")], NOW)
        .unwrap();
    let hot = HotStore::new(Arc::clone(&primary), Bounds::DEFAULT);
    let reader = ReaderStore::new(
        Arc::clone(&hot),
        Arc::clone(&primary),
        None,
        nothing_applied(),
    );
    reader.read(&latest_query(0)).unwrap();
    hot.load_wanted();
    assert_eq!(bids(&reader.read(&latest_query(0)).unwrap()), bid("100"));
    let head = primary.heads().unwrap()[LIVE];
    primary
        .record(LIVE, vec![quote_at("q2", NOW + 1_000, "101")], NOW)
        .unwrap();
    let ahead = primary.heads().unwrap()[LIVE];
    // The reader has applied to `head`; its primary is at `ahead`.
    let reads = Reads::default();
    reads.keep("lake", Arc::new(At(head)));
    reads.keep_primary("lake", Arc::new(At(ahead)));
    reads.set_bound(Duration::from_millis(50));
    let answered = reads
        .answer("platform.lake.query.list-quotes", &mark(ahead), || {
            reader
                .read(&latest_query(0))
                .map_err(|failed| failed.to_string())
        })
        .unwrap();
    assert_eq!(
        bids(&answered),
        bid("100,101"),
        "never the hot layer behind it"
    );
    // And a read it has reached is its hot layer's, as held.
    let answered = reads
        .answer("platform.lake.query.list-quotes", &mark(head), || {
            reader
                .read(&latest_query(0))
                .map_err(|failed| failed.to_string())
        })
        .unwrap();
    assert_eq!(bids(&answered), bid("100"));
}

/// A member at a fixed position of one partition.
struct AtHead(&'static str, u64);

impl Position for AtHead {
    fn heads(&self, _: &[String]) -> std::result::Result<HashMap<String, u64>, String> {
        Ok([(self.0.to_string(), self.1)].into())
    }
}

const DAILY: &str = "coinbase-1:daily";

fn daily_at(sequence: u64) -> Watermark {
    Watermark {
        partitions: vec![PartitionSequence {
            partition: DAILY.into(),
            sequence,
        }],
    }
}

fn kept_close(store: &dyn Store, row_key: &str, date: &str, amount: &str) {
    store
        .record(
            DAILY,
            vec![Observation::Price(Box::new(close(
                "coinbase-1",
                row_key,
                BTC,
                date,
                amount,
            )))],
            NOW,
        )
        .unwrap();
}

fn answered_at(answer: &service::ReadAnswer) -> u64 {
    answer
        .watermark
        .partitions
        .iter()
        .find(|p| p.partition == DAILY)
        .map(|p| p.sequence)
        .unwrap()
}

/// A reader's reply carries the position it was answered at: what it has
/// applied, never the primary's heads beyond it; the primary's for an answer
/// from the primary past its wait; and no further than a read replica that
/// answered it had reached. (The lake cuts every read off at its own now, so
/// a reader's latest read is the record's past here, the replica's.)
#[tokio::test(flavor = "multi_thread")]
async fn a_readers_reply_says_the_position_it_was_answered_at() {
    let clock = Arc::new(ManualClock::at(NOW));
    let bus = Arc::new(Bus::single(
        "lake-1",
        Arc::new(MemoryBackend::new()),
        clock.clone(),
    ));
    stand_ins(&bus);
    bus.serve_only(meridian_bus::read_after::is_read);
    let primary: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let replica: Arc<dyn Store> = Arc::new(MemoryStore::new());
    kept_close(primary.as_ref(), "k1", "2026-10-07", "5");
    kept_close(replica.as_ref(), "k1", "2026-10-07", "5");
    kept_close(primary.as_ref(), "k2", "2026-10-06", "6");
    let (behind, ahead) = (
        replica.heads().unwrap()[DAILY],
        primary.heads().unwrap()[DAILY],
    );
    assert!(behind < ahead);
    // The member has applied the writer's change log to `behind`.
    let applied = Arc::new(Mutex::new(BTreeMap::from([(DAILY.to_string(), behind)])));
    let reading = {
        let applied = Arc::clone(&applied);
        ReaderStore::new(
            HotStore::new(Arc::clone(&primary), Bounds::DEFAULT),
            Arc::clone(&primary),
            Some(Arc::clone(&replica)),
            Arc::new(move || applied.lock().unwrap().clone()),
        )
    };
    let store = Arc::new(MemoryStore::new());
    let lake = service::serve(bus.clone(), reading, clock.clone());
    lake.configured(&configuration(true, &[]));
    let h = Harness {
        bus: bus.clone(),
        store,
        clock,
        lake,
    };
    let latest = || {
        crate::reader::begin();
        read(&h, "reporting-1", &[BTC], "", SourceChoice::default(), 0)
    };
    // Its own position, though the primary is ahead.
    let answer = latest();
    assert!(!answer.rows.is_empty());
    assert_eq!(answered_at(&answer), behind, "its own applied position");

    // Past its wait, from the primary: the primary's.
    let reads = bus.reads_after();
    reads.keep_primary(
        "lake",
        Arc::new(AllHeads({
            let primary = Arc::clone(&primary);
            move || primary.heads().map_err(|failed| failed.to_string())
        })),
    );
    reads.keep("lake", Arc::new(AtHead(DAILY, behind)));
    reads.set_bound(Duration::from_millis(50));
    let answer = reads
        .answer(service::LIST_PRICES, &daily_at(ahead), || Ok(latest()))
        .unwrap();
    assert_eq!(answered_at(&answer), ahead, "the primary's");

    // Applied to the head, a past read the replica answers says no more than
    // the replica had reached.
    *applied.lock().unwrap() = BTreeMap::from([(DAILY.to_string(), ahead)]);
    crate::reader::begin();
    let past = read(
        &h,
        "reporting-1",
        &[BTC],
        "2026-10-07",
        SourceChoice::default(),
        0,
    );
    assert_eq!(past.rows.len(), 1);
    assert_eq!(
        answered_at(&past),
        behind,
        "the replica's, behind what it applied"
    );
}
