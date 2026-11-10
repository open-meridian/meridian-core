//! A reader member's store (contract v30, stage B): latest reads from a hot
//! layer the change log keeps current, the record's past from a read replica
//! that has reached the read's `read_after`, everything from the primary
//! past the read's wait, and nothing written.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use meridian_bus::read_after::{Position, Reads};
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
    let reader = ReaderStore::new(Arc::clone(&hot), Arc::clone(&primary), None);
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
    let reader = ReaderStore::new(hot, Arc::clone(&primary), None);
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
    let reader = ReaderStore::new(hot, Arc::clone(&primary), Some(Arc::clone(&replica)));
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
    let reader = ReaderStore::new(Arc::clone(&hot), Arc::clone(&primary), None);
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
