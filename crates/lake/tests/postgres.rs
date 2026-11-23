//! The lake's store against Postgres: the same rules the in-memory store
//! answers, as the database keeps them. Run by `make test-store`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use meridian_changes::scratch::{OffRuntime, Scratch};
use meridian_clock::{Clock, SystemClock};
use meridian_domain::date::Date;
use meridian_domain::v1::{
    EntitlementsChangedEvent, Money, ObservationMeta, Price, Source, SourcePriority, SubjectRef,
};
use meridian_lake::postgres::SCHEMA_LOCK;
use meridian_lake::row::{DataType, Observation};
use meridian_lake::store::{Query, Store, When};
use meridian_lake::PostgresStore;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique(tag: &str) -> String {
    let seq = COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("{tag}-{}-{seq}", SystemClock.now_ns())
}

fn test_database() -> String {
    std::env::var("MERIDIAN_TEST_DATABASE_URL").expect("MERIDIAN_TEST_DATABASE_URL")
}

/// Schema `name` made in the test database, and a URL whose search path is
/// it: dropped with its last clone (PO 65).
fn own_schema(name: &str) -> Scratch {
    Scratch::schema(&test_database(), name)
}

fn store() -> PostgresStore {
    let url = std::env::var("MERIDIAN_TEST_DATABASE_URL").expect(
        "MERIDIAN_TEST_DATABASE_URL is not set. These tests need a real Postgres; run them with \
         `make test-store`.",
    );
    let store = PostgresStore::connect(&url, 4).expect("could not reach the test database");
    store
        .migrate(&SystemClock)
        .expect("could not create the schema");
    store.verify().expect("the schema verifies once migrated");
    store
}

fn close(dataset: &str, row_key: &str, subject: &str, date: &str, amount: i64) -> Observation {
    Observation::Price(Box::new(Price {
        meta: Some(ObservationMeta {
            row_key: row_key.into(),
            subjects: vec![SubjectRef {
                entity_id: subject.into(),
                ..Default::default()
            }],
            source: Some(Source {
                instance: "coinbase-1".into(),
                dataset: dataset.into(),
                ..Default::default()
            }),
            valid_from_ns: 100,
            valid_until_ns: 200,
            business_date: date.into(),
            ..Default::default()
        }),
        kind: 1,
        price: Some(Money {
            amount: Some(meridian_pb::v1::Decimal {
                high: 0,
                low: amount as u64,
                scale: 0,
            }),
            currency_code: "USD".into(),
            instrument_id: "LCL-USD".into(),
        }),
        basis: 1,
        confidence: None,
        ..Default::default()
    }))
}

#[test]
fn a_batch_is_numbered_restated_read_as_of_and_retention_recorded() {
    let store = store();
    let dataset = unique("coinbase-1:daily");
    let subject = unique("LCL-BTC");
    let first = store
        .record(
            &dataset,
            vec![
                close(&dataset, "a", &subject, "2026-10-07", 10),
                close(&dataset, "b", &subject, "2026-10-08", 11),
            ],
            1_000,
        )
        .unwrap();
    assert_eq!((first.recorded, first.head), (2, 2));
    assert_eq!(first.rows[1].meta().previous_sequence, 1);
    let same = store
        .record(
            &dataset,
            vec![close(&dataset, "a", &subject, "2026-10-07", 10)],
            2_000,
        )
        .unwrap();
    assert_eq!((same.unchanged, same.head), (1, 2), "nothing numbered");
    let restated = store
        .record(
            &dataset,
            vec![close(&dataset, "a", &subject, "2026-10-07", 12)],
            3_000,
        )
        .unwrap();
    assert_eq!(restated.restated, 1);
    assert_eq!(restated.rows[0].meta().version, 2);
    assert_eq!(restated.rows[0].meta().sequence, 3);

    let query = |as_of_ns: i64| Query {
        datasets: vec![dataset.clone()],
        subjects: vec![subject.clone()],
        data_type: DataType::Price,
        kinds: vec![],
        interval_ns: 0,
        when: When::BusinessDate(Date::parse("2026-10-07").unwrap()),
        as_of_ns,
        published_as_of_ns: 0,
        latest_only: false,
    };
    let now = store.read(&query(0)).unwrap();
    assert_eq!(now.len(), 1);
    assert_eq!(now[0].meta().version, 2);
    let then = store.read(&query(2_500)).unwrap();
    assert_eq!(then[0].meta().version, 1, "what the lake knew then");
    assert_eq!(store.heads().unwrap().get(&dataset), Some(&3));

    let removed = store
        .remove_before(
            &dataset,
            2_000,
            "retention under the dataset's licence: 1 days",
            5_000,
        )
        .unwrap();
    assert_eq!(removed, 2);
    assert!(store
        .removals()
        .unwrap()
        .iter()
        .any(|r| r.dataset == dataset && r.rows == 2));
}

#[test]
fn a_priority_carries_the_stale_guard_and_what_is_served_wants_and_misses_are_kept() {
    let store = store();
    let data_type = unique("meridian.v1.Price");
    let priority = |at: i64| SourcePriority {
        data_type: data_type.clone(),
        kind: 1,
        datasets: vec!["coinbase-1:daily".into()],
        updated_by: "local|ada".into(),
        updated_at_ns: at,
        ..Default::default()
    };
    assert!(store.set_priority(&priority(10), 0).unwrap().is_ok());
    assert!(
        store.set_priority(&priority(20), 0).unwrap().is_err(),
        "stale"
    );
    assert!(store.set_priority(&priority(20), 10).unwrap().is_ok());
    let latest: Vec<_> = store
        .priorities()
        .unwrap()
        .into_iter()
        .filter(|p| p.data_type == data_type)
        .collect();
    assert_eq!(latest, vec![priority(20)]);

    let dataset = unique("kraken-1:live");
    let served = store
        .serve_unkept(
            &dataset,
            vec![close(&dataset, "x", "LCL-BTC", "", 1)],
            &["reporting-1".to_string()],
            None,
            7,
        )
        .unwrap();
    assert_eq!(served.head, 1);
    assert!(store
        .served()
        .unwrap()
        .iter()
        .any(|s| s.dataset == dataset && s.readers == vec!["reporting-1"]));
    assert!(
        !store.counts().unwrap().contains_key(&dataset),
        "no value kept"
    );

    let instance = unique("coinbase");
    store.count_miss(&instance, 1).unwrap();
    store.count_miss(&instance, 2).unwrap();
    assert_eq!(store.miss_counts().unwrap().get(&instance), Some(&2));

    let replaced = unique("LCL-OLD");
    store.keep_alias(&replaced, "LCL-NEW", 3).unwrap();
    assert_eq!(
        store.aliases().unwrap().get(&replaced).map(String::as_str),
        Some("LCL-NEW")
    );

    let event = EntitlementsChangedEvent {
        changed_at_ns: 42,
        ..Default::default()
    };
    store.keep_configuration(&event).unwrap();
    assert!(store.configuration().unwrap().is_some());
}

/// A trade, as the 1b records it (contract v19): keyed by its valid time,
/// with no business date.
/// A row restated: its value changed, its valid time and first subject kept
/// (the lake's rule 5).
fn restate(mut row: Observation) -> Observation {
    row.meta_mut().valid_until_ns += 1;
    row
}

fn trade(dataset: &str, row_key: &str, subject: &str, valid_from_ns: i64) -> Observation {
    Observation::Trade(Box::new(meridian_domain::v1::Trade {
        meta: Some(ObservationMeta {
            row_key: row_key.into(),
            subjects: vec![SubjectRef {
                entity_id: subject.into(),
                ..Default::default()
            }],
            source: Some(Source {
                instance: "coinbase-1".into(),
                dataset: dataset.into(),
                ..Default::default()
            }),
            valid_from_ns,
            ..Default::default()
        }),
        price: Some(Money {
            amount: Some(meridian_pb::v1::Decimal {
                high: 0,
                low: 62_000,
                scale: 0,
            }),
            currency_code: "USD".into(),
            instrument_id: "LCL-USD".into(),
        }),
        quantity: Some(meridian_pb::v1::Decimal {
            high: 0,
            low: 5,
            scale: 1,
        }),
        ..Default::default()
    }))
}

#[test]
fn trades_recorded_after_a_watermark_are_read_whatever_their_valid_time() {
    let store = store();
    let dataset = unique("coinbase-1:live");
    let subject = unique("LCL-BTC");
    store
        .record(
            &dataset,
            vec![
                trade(&dataset, "t1", &subject, 500),
                trade(&dataset, "t2", &subject, 600),
            ],
            1_000,
        )
        .unwrap();
    // A late print, recorded after the reader's watermark of 2.
    store
        .record(
            &dataset,
            vec![trade(&dataset, "late", &subject, 100)],
            2_000,
        )
        .unwrap();
    let after = store
        .read(&Query {
            datasets: vec![dataset.clone()],
            subjects: vec![subject.clone()],
            data_type: DataType::Trade,
            kinds: vec![],
            interval_ns: 0,
            when: When::After([(dataset.clone(), 2)].into()),
            as_of_ns: 0,
            published_as_of_ns: 0,
            latest_only: false,
        })
        .unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].meta().row_key, "late");
    assert_eq!(after[0].meta().sequence, 3);
    assert_eq!(store.lowest_kept(&dataset).unwrap(), Some(1));
    assert_eq!(store.lowest_kept(&unique("none")).unwrap(), None);
}

/// Stage M (the store memory audit of 2026-11-09, item 4): a catch-up from
/// an old watermark -- the whole dataset -- is handed on a chunk at a time as
/// the database answers it, each row key at its version in force and in the
/// order recorded, never read whole first. Its heap holds a chunk: well under
/// a tenth of the set decoded whole.
#[test]
fn a_catch_up_from_an_old_watermark_holds_a_chunk_never_the_dataset() {
    let store = store();
    let dataset = unique("coinbase-1:live");
    let subject = unique("LCL-BTC");
    const ROWS: usize = 40_000;
    for batch in 0..ROWS / 500 {
        let rows = (0..500)
            .map(|n| {
                trade(
                    &dataset,
                    &format!("t{}", batch * 500 + n),
                    &subject,
                    1_000 + n as i64,
                )
            })
            .collect();
        store.record(&dataset, rows, 1_000 + batch as i64).unwrap();
    }
    // Ten row keys restated: each answered once, at its later version.
    let restated: Vec<Observation> = (0..10)
        .map(|n| restate(trade(&dataset, &format!("t{n}"), &subject, 1_000 + n)))
        .collect();
    store.record(&dataset, restated, 5_000).unwrap();
    let query = Query {
        datasets: vec![dataset.clone()],
        subjects: vec![subject.clone()],
        data_type: DataType::Trade,
        kinds: vec![],
        interval_ns: 0,
        when: When::After([(dataset.clone(), 0)].into()),
        as_of_ns: 0,
        published_as_of_ns: 0,
        latest_only: false,
    };
    let mut whole_bytes = 0usize;
    let mut answered = 0usize;
    let mut last_sequence = 0u64;
    let mut versions_two = 0usize;
    counted::take();
    store
        .read_each(&query, &mut |row| {
            whole_bytes += meridian_lake::pages::row_bytes(&row);
            answered += 1;
            assert!(row.meta().sequence > last_sequence, "in the order recorded");
            last_sequence = row.meta().sequence;
            if row.meta().version == 2 {
                versions_two += 1;
            }
            Ok(())
        })
        .unwrap();
    let (_, peak) = counted::take();
    assert_eq!(answered, ROWS, "each row key once");
    assert_eq!(versions_two, 10, "a restated key at its version in force");
    let whole_mib = whole_bytes as f64 / 1048576.0;
    println!("CATCH_UP heap_peak_mib={peak:.1} whole_set_mib={whole_mib:.1}");
    assert!(
        peak * 10.0 < whole_mib,
        "the catch-up's heap peaked at {peak:.1} MiB against the set's {whole_mib:.1} MiB decoded"
    );
}

/// Stage M (the store memory audit of 2026-11-09, item 12): a read loads the
/// closed-day re-checks of the rows it answers, beside each chunk, never
/// every re-check its dataset ever had. A day's read of a dataset re-checked
/// 20,000 times holds a few kilobytes of them, and still answers the flag
/// as re-checked.
#[test]
fn a_read_loads_the_closed_day_rechecks_of_its_rows_alone() {
    use meridian_domain::v1::{DateCheck, DateCheckReason};
    use meridian_lake::store::ClosedDayAmendment;
    let store = store();
    let dataset = unique("coinbase-1:daily");
    let subject = unique("LCL-BTC");
    let other = unique("LCL-ETH");
    const ROWS: usize = 20_000;
    let mut sequences = Vec::new();
    for batch in 0..ROWS / 500 {
        let rows = (0..500)
            .map(|n| {
                close(
                    &dataset,
                    &format!("k{}", batch * 500 + n),
                    &other,
                    "2026-10-12",
                    10,
                )
            })
            .collect();
        let done = store.record(&dataset, rows, 1_000).unwrap();
        sequences.extend(done.rows.iter().map(|row| row.meta().sequence));
    }
    let asked = store
        .record(
            &dataset,
            vec![close(&dataset, "asked", &subject, "2026-10-13", 10)],
            1_000,
        )
        .unwrap()
        .rows[0]
        .meta()
        .sequence;
    sequences.push(asked);
    let recheck = |sequence: u64| ClosedDayAmendment {
        dataset: dataset.clone(),
        sequence,
        row_key: String::new(),
        closed_day: Some(DateCheck {
            date: "2026-10-13".into(),
            reason: DateCheckReason::Holiday as i32,
            holiday_name: format!("A closure re-checked for row {sequence} of the dataset"),
            ..Default::default()
        }),
        why: "calendar CAL-USSS version 2 applied".into(),
        at_ns: 2_000,
    };
    for chunk in sequences.chunks(1_000) {
        let rechecks: Vec<ClosedDayAmendment> = chunk.iter().map(|s| recheck(*s)).collect();
        store.amend_closed_day(&rechecks).unwrap();
    }
    let query = Query {
        datasets: vec![dataset.clone()],
        subjects: vec![subject.clone()],
        data_type: DataType::Price,
        kinds: vec![],
        interval_ns: 0,
        when: When::BusinessDate(Date::parse("2026-10-13").unwrap()),
        as_of_ns: 0,
        published_as_of_ns: 0,
        latest_only: false,
    };
    // Once to warm the pool's connections, then measured.
    store.read(&query).unwrap();
    counted::take();
    let read = store.read(&query).unwrap();
    let (_, peak) = counted::take();
    assert_eq!(read.len(), 1);
    assert_eq!(
        read[0].meta().closed_day.as_ref().unwrap().reason,
        DateCheckReason::Holiday as i32,
        "the flag as re-checked"
    );
    println!(
        "RECHECKS heap_peak_mib={peak:.2} rechecks={}",
        sequences.len()
    );
    assert!(
        peak < 1.0,
        "a read of one row held {peak:.2} MiB; every re-check of the dataset is about 4 MiB"
    );
}

/// Stage M (the store memory audit of 2026-11-09, item 6): a whole dataset's
/// read -- how one account's execution costs and the saved scenarios are
/// read -- hands each row key on at its version in force, a chunk at a time,
/// in sequence order, the same rows `read_dataset` answers; the reader keeps
/// what it answers, and the heap holds a chunk, never the dataset.
#[test]
fn a_whole_datasets_read_hands_on_a_chunk_at_a_time() {
    let store = store();
    let dataset = unique("coinbase-1:live");
    let subject = unique("LCL-BTC");
    const ROWS: usize = 40_000;
    for batch in 0..ROWS / 500 {
        let rows = (0..500)
            .map(|n| {
                trade(
                    &dataset,
                    &format!("t{}", batch * 500 + n),
                    &subject,
                    1_000 + n as i64,
                )
            })
            .collect();
        store.record(&dataset, rows, 1_000 + batch as i64).unwrap();
    }
    let restated: Vec<Observation> = (0..10)
        .map(|n| restate(trade(&dataset, &format!("t{n}"), &subject, 1_000 + n)))
        .collect();
    store.record(&dataset, restated, 5_000).unwrap();
    let mut whole_bytes = 0usize;
    let mut kept = Vec::new();
    let mut answered = 0usize;
    let mut last = 0u64;
    counted::take();
    store
        .read_dataset_each(&dataset, DataType::Trade, 0, &mut |row| {
            whole_bytes += meridian_lake::pages::row_bytes(&row);
            answered += 1;
            assert!(row.meta().sequence > last, "in sequence order");
            last = row.meta().sequence;
            // What a reader keeps: one row in a thousand.
            if row.meta().row_key.ends_with("000") {
                kept.push(row);
            }
            Ok(())
        })
        .unwrap();
    let (_, peak) = counted::take();
    assert_eq!(answered, ROWS, "each row key once");
    let whole = store.read_dataset(&dataset, DataType::Trade, 0, 0).unwrap();
    assert_eq!(whole.len(), ROWS, "the rows read_dataset answers");
    assert_eq!(
        whole.iter().filter(|row| row.meta().version == 2).count(),
        10,
        "a restated key at its version in force"
    );
    let whole_mib = whole_bytes as f64 / 1048576.0;
    println!(
        "DATASET heap_peak_mib={peak:.1} whole_set_mib={whole_mib:.1} kept={}",
        kept.len()
    );
    assert!(
        peak * 10.0 < whole_mib,
        "the read's heap peaked at {peak:.1} MiB against the set's {whole_mib:.1} MiB decoded"
    );
}

fn outcome(dataset: &str, subject: &str, stage: i32, value: u64) -> Observation {
    Observation::Outcome(Box::new(meridian_domain::v1::Outcome {
        meta: Some(ObservationMeta {
            row_key: "KXFED-26OCT".into(),
            subjects: vec![SubjectRef {
                entity_id: subject.into(),
                ..Default::default()
            }],
            source: Some(Source {
                instance: "kalshi-1".into(),
                dataset: dataset.into(),
                ..Default::default()
            }),
            // Every stage under the first determination's valid time (the
            // lake's rule 5, ruled 2026-11-12).
            valid_from_ns: 100,
            ..Default::default()
        }),
        determination: 1,
        stage,
        settlement_value: Some(Money {
            amount: Some(meridian_pb::v1::Decimal {
                high: 0,
                low: value,
                scale: 2,
            }),
            currency_code: "USD".into(),
            instrument_id: "LCL-USD".into(),
        }),
        settled_at_ns: 0,
        determination_as_reported: Some(meridian_pb::v1::AsReported {
            scheme: "kalshi:result".into(),
            code: "yes".into(),
            text: String::new(),
        }),
        challenge_ends_at_ns: 0,
    }))
}

/// Contract v23: an outcome's stages are versions of one row key; the
/// latest read answers the version in force, a catch-up after a watermark
/// every version recorded since.
#[test]
fn an_outcomes_every_version_is_read_after_a_watermark() {
    let store = store();
    let dataset = unique("kalshi-1:outcomes");
    let subject = unique("LCL-KXFED");
    store
        .record(&dataset, vec![outcome(&dataset, &subject, 2, 100)], 1_000)
        .unwrap();
    store
        .record(&dataset, vec![outcome(&dataset, &subject, 3, 100)], 2_000)
        .unwrap();
    store
        .record(&dataset, vec![outcome(&dataset, &subject, 4, 50)], 3_000)
        .unwrap();
    let query = |when: When, as_of_ns: i64| Query {
        datasets: vec![dataset.clone()],
        subjects: vec![subject.clone()],
        data_type: DataType::Outcome,
        kinds: vec![],
        interval_ns: 0,
        when,
        as_of_ns,
        published_as_of_ns: 0,
        latest_only: false,
    };
    let versions = |rows: Vec<Observation>| -> Vec<u64> {
        rows.iter().map(|row| row.meta().version).collect()
    };
    assert_eq!(
        versions(store.read(&query(When::Latest { at_ns: 0 }, 0)).unwrap()),
        vec![3]
    );
    assert_eq!(
        versions(
            store
                .read(&query(When::Latest { at_ns: 0 }, 2_500))
                .unwrap()
        ),
        vec![2]
    );
    let after = When::After([(dataset.clone(), 1)].into_iter().collect());
    assert_eq!(versions(store.read(&query(after, 0)).unwrap()), vec![2, 3]);
}

/// Contract v23 (W10.4, W10.9): a row's closed-day flag kept with it and
/// counted; a re-check journalled beside it, read as of when it was known,
/// never a new version.
#[test]
fn a_closed_day_flag_is_counted_and_amended_beside_its_row() {
    use meridian_domain::v1::{DateCheck, DateCheckReason};
    use meridian_lake::store::ClosedDayAmendment;
    let store = store();
    let dataset = unique("coinbase-1:daily");
    let subject = unique("LCL-BTC");
    let flagged = |row_key: &str, date: &str, reason: Option<DateCheckReason>| {
        let mut row = close(&dataset, row_key, &subject, date, 10);
        row.meta_mut().closed_day = reason.map(|reason| DateCheck {
            date: date.into(),
            reason: reason as i32,
            ..Default::default()
        });
        row
    };
    let done = store
        .record(
            &dataset,
            vec![
                flagged("sat", "2026-10-10", Some(DateCheckReason::Weekend)),
                flagged("tue", "2026-10-13", None),
                flagged("late", "2027-03-01", Some(DateCheckReason::OutsideCoverage)),
            ],
            1_000,
        )
        .unwrap();
    assert_eq!(store.closed_day_counts().unwrap()[&dataset], (1, 1));
    assert_eq!(
        store.dated_range(&dataset).unwrap(),
        Some(("2026-10-10".to_string(), "2027-03-01".to_string()))
    );
    let tuesday = done.rows[1].meta().sequence;
    store
        .amend_closed_day(&[ClosedDayAmendment {
            dataset: dataset.clone(),
            sequence: tuesday,
            row_key: String::new(),
            closed_day: Some(DateCheck {
                date: "2026-10-13".into(),
                reason: DateCheckReason::Holiday as i32,
                holiday_name: "A closure".into(),
                ..Default::default()
            }),
            why: "calendar CAL-USSS version 2 applied".into(),
            at_ns: 2_000,
        }])
        .unwrap();
    assert_eq!(store.closed_day_counts().unwrap()[&dataset], (2, 1));
    let on = store
        .dated_on(&dataset, &["2026-10-13".to_string()], 0, usize::MAX)
        .unwrap();
    assert_eq!(on.len(), 1);
    // A chunk at a time, by sequence: nothing after the last.
    assert!(store
        .dated_on(&dataset, &["2026-10-13".to_string()], tuesday, 1)
        .unwrap()
        .is_empty());
    assert_eq!(
        on[0].meta().closed_day.as_ref().unwrap().holiday_name,
        "A closure"
    );
    let query = |as_of_ns: i64| Query {
        datasets: vec![dataset.clone()],
        subjects: vec![subject.clone()],
        data_type: DataType::Price,
        kinds: vec![],
        interval_ns: 0,
        when: When::BusinessDate(Date::parse("2026-10-13").unwrap()),
        as_of_ns,
        published_as_of_ns: 0,
        latest_only: false,
    };
    assert!(store.read(&query(1_500)).unwrap()[0]
        .meta()
        .closed_day
        .is_none());
    let now = store.read(&query(0)).unwrap();
    assert_eq!(
        now[0].meta().closed_day.as_ref().unwrap().reason,
        DateCheckReason::Holiday as i32
    );
    assert_eq!(now[0].meta().version, 1, "never a new version");
    assert_eq!(store.heads().unwrap()[&dataset], 3);
    // A day recorded closed and re-checked open is no longer counted: each
    // count reads only the rows recorded flagged or re-checked since
    // (migration 6, ruling 816), by the flag as known now.
    store
        .amend_closed_day(&[ClosedDayAmendment {
            dataset: dataset.clone(),
            sequence: done.rows[0].meta().sequence,
            row_key: String::new(),
            closed_day: None,
            why: "calendar CAL-USSS version 3 applied".into(),
            at_ns: 3_000,
        }])
        .unwrap();
    assert_eq!(store.closed_day_counts().unwrap()[&dataset], (1, 1));
}

/// The datasets' listing's count of rows carrying a value as reported, read
/// from those rows alone (migration 6, ruling 816), as every row's count
/// says it.
#[test]
fn the_rows_carrying_a_value_as_reported_are_counted_per_dataset() {
    let store = store();
    let dataset = unique("coinbase-1:daily");
    let subject = unique("LCL-BTC");
    let mut reported = close(&dataset, "as-reported", &subject, "2026-10-13", 10);
    reported.meta_mut().unconverted = vec![meridian_pb::v1::AsReported {
        scheme: "coinbase".into(),
        code: "X".into(),
        text: "a value as reported".into(),
    }];
    store
        .record(
            &dataset,
            vec![
                reported,
                close(&dataset, "plain", &subject, "2026-10-14", 11),
            ],
            1_000,
        )
        .unwrap();
    assert_eq!(store.unconverted_counts().unwrap().get(&dataset), Some(&1));
    assert_eq!(store.counts().unwrap()[&dataset], (2, 1));
    let empty = unique("coinbase-1:daily");
    store
        .record(
            &empty,
            vec![close(&empty, "plain", &subject, "2026-10-14", 11)],
            1_000,
        )
        .unwrap();
    assert_eq!(store.unconverted_counts().unwrap().get(&empty), None);
}

/// Ruling 1006 (c): the rows carrying a value as reported and those flagged
/// on a closed day are counted in force -- a row key once, at the version a
/// default read serves -- as the quality counts are; a re-check of a
/// replaced version, and a rate's late-arriving older publication, count
/// nothing.
#[test]
fn the_rows_as_reported_and_on_a_closed_day_are_counted_in_force() {
    use meridian_domain::v1::{DateCheck, DateCheckReason, QualityState};
    use meridian_lake::store::ClosedDayAmendment;
    let store = store();
    let dataset = unique("coinbase-1:daily");
    let subject = unique("LCL-BTC");
    let as_reported = |mut row: Observation| {
        row.meta_mut().unconverted = vec![meridian_pb::v1::AsReported {
            scheme: "coinbase".into(),
            code: "X".into(),
            text: "a value as reported".into(),
        }];
        row
    };
    let on = |reason: DateCheckReason, date: &str, mut row: Observation| {
        row.meta_mut().closed_day = Some(DateCheck {
            date: date.into(),
            reason: reason as i32,
            ..Default::default()
        });
        row
    };
    let first = store
        .record(
            &dataset,
            vec![
                as_reported(close(&dataset, "tue", &subject, "2026-10-13", 10)),
                on(
                    DateCheckReason::Weekend,
                    "2026-10-10",
                    close(&dataset, "sat", &subject, "2026-10-10", 11),
                ),
                on(
                    DateCheckReason::OutsideCoverage,
                    "2027-03-01",
                    close(&dataset, "late", &subject, "2027-03-01", 12),
                ),
                close(&dataset, "wed", &subject, "2026-10-14", 13),
            ],
            1_000,
        )
        .unwrap();
    let unconverted = |store: &PostgresStore| {
        store
            .unconverted_counts()
            .unwrap()
            .get(&dataset)
            .copied()
            .unwrap_or(0)
    };
    let closed = |store: &PostgresStore| {
        store
            .closed_day_counts()
            .unwrap()
            .get(&dataset)
            .copied()
            .unwrap_or_default()
    };
    assert_eq!((unconverted(&store), closed(&store)), (1, (1, 1)));

    // Each restated: converted, and corrected on the same days.
    store
        .record(
            &dataset,
            vec![
                close(&dataset, "tue", &subject, "2026-10-13", 20),
                on(
                    DateCheckReason::Weekend,
                    "2026-10-10",
                    close(&dataset, "sat", &subject, "2026-10-10", 21),
                ),
                on(
                    DateCheckReason::OutsideCoverage,
                    "2027-03-01",
                    close(&dataset, "late", &subject, "2027-03-01", 22),
                ),
            ],
            2_000,
        )
        .unwrap();
    assert_eq!((unconverted(&store), closed(&store)), (0, (1, 1)));
    assert_eq!(
        store.counts().unwrap()[&dataset],
        (7, 1),
        "every version kept"
    );

    // A re-check of a replaced version counts nothing; of the version in
    // force, it counts.
    let recheck = |row: &Observation| ClosedDayAmendment {
        dataset: dataset.clone(),
        sequence: row.meta().sequence,
        row_key: String::new(),
        closed_day: Some(DateCheck {
            date: row.meta().business_date.clone(),
            reason: DateCheckReason::Holiday as i32,
            holiday_name: "A closure".into(),
            ..Default::default()
        }),
        why: "calendar CAL-USSS version 2 applied".into(),
        at_ns: 3_000,
    };
    store.amend_closed_day(&[recheck(&first.rows[0])]).unwrap();
    assert_eq!(closed(&store), (1, 1), "a replaced version");
    store.amend_closed_day(&[recheck(&first.rows[3])]).unwrap();
    assert_eq!(closed(&store), (2, 1));

    // A rate's late-arriving older publication, as reported: never in
    // force, never counted.
    let rates = unique("ny-fed-1:rates");
    let rate = unique("INS-SOFR");
    store
        .record(
            &rates,
            vec![rate_row(
                &rates,
                "k",
                &rate,
                432,
                2_000,
                QualityState::Valid,
            )],
            1_000,
        )
        .unwrap();
    let mut late = rate_row(&rates, "k", &rate, 431, 1_000, QualityState::Valid);
    late.meta_mut().unconverted = vec![meridian_pb::v1::AsReported {
        scheme: "ny-fed".into(),
        code: "X".into(),
        text: "a value as reported".into(),
    }];
    store.record(&rates, vec![late], 2_000).unwrap();
    assert_eq!(store.unconverted_counts().unwrap().get(&rates), None);
}

/// Contract v28 (W10.12; ruling 423): a signal plugin's assessment kept
/// beside its row, its own record, raising the row's effective state as of
/// when it was recorded; the row's body never rewritten.
#[test]
fn an_assessment_is_kept_beside_its_row_and_raises_its_effective_state() {
    use meridian_domain::v1::{QualityReason, QualityState};
    use meridian_lake::store::Assessment;
    let store = store();
    let dataset = unique("coinbase-1:daily");
    let subject = unique("LCL-BTC");
    let mut suspicious = close(&dataset, "k", &subject, "2026-10-13", 10);
    suspicious.meta_mut().quality = QualityState::Suspicious as i32;
    suspicious.meta_mut().quality_reasons = vec![QualityReason::Duplicate as i32];
    let done = store.record(&dataset, vec![suspicious], 1_000).unwrap();
    let sequence = done.rows[0].meta().sequence;
    let held = store.row_version(&dataset, "k", 1).unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].meta().quality, QualityState::Suspicious as i32);
    assert!(store.row_version(&dataset, "k", 2).unwrap().is_empty());
    store
        .assess(&[Assessment {
            dataset: dataset.clone(),
            sequence,
            row_key: "k".into(),
            version: 1,
            state: QualityState::Invalid as i32,
            reasons: vec![QualityReason::PriceOutlier as i32],
            by_instance: "signal-1".into(),
            at_ns: 2_000,
        }])
        .unwrap();
    let query = |as_of_ns: i64| Query {
        datasets: vec![dataset.clone()],
        subjects: vec![subject.clone()],
        data_type: DataType::Price,
        kinds: vec![],
        interval_ns: 0,
        when: When::BusinessDate(Date::parse("2026-10-13").unwrap()),
        as_of_ns,
        published_as_of_ns: 0,
        latest_only: false,
    };
    let then = store.read(&query(1_500)).unwrap();
    assert_eq!(then[0].meta().quality, QualityState::Suspicious as i32);
    let now = store.read(&query(0)).unwrap();
    assert_eq!(now[0].meta().quality, QualityState::Invalid as i32);
    assert_eq!(
        now[0].meta().quality_reasons,
        vec![
            QualityReason::Duplicate as i32,
            QualityReason::PriceOutlier as i32
        ]
    );
    assert_eq!(now[0].meta().version, 1, "never a new version");
    let kept: Vec<Assessment> = store
        .assessments()
        .unwrap()
        .into_iter()
        .filter(|a| a.dataset == dataset)
        .collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].by_instance, "signal-1");
    assert_eq!(
        store.read_dataset(&dataset, DataType::Price, 0, 0).unwrap()[0]
            .meta()
            .quality,
        QualityState::Invalid as i32
    );
}

/// The quality counts of `dataset` (contract v29, W10.9; ruling 619 (2)):
/// valid, suspicious and invalid, of the rows in force.
fn counts_of(store: &PostgresStore, dataset: &str) -> (u64, u64, u64) {
    store
        .quality_counts()
        .unwrap()
        .get(dataset)
        .copied()
        .unwrap_or_default()
}

/// An escalation of one row version by a signal plugin (contract v28, W10.12).
fn escalation(
    dataset: &str,
    row: &Observation,
    state: meridian_domain::v1::QualityState,
    at_ns: i64,
) -> meridian_lake::store::Assessment {
    meridian_lake::store::Assessment {
        dataset: dataset.into(),
        sequence: row.meta().sequence,
        row_key: row.meta().row_key.clone(),
        version: row.meta().version,
        state: state as i32,
        reasons: vec![],
        by_instance: "signal-1".into(),
        at_ns,
    }
}

#[test]
fn the_rows_in_force_are_counted_by_their_effective_state_without_reading_every_row() {
    // Contract v29 (W10.9; ruling 619 (2)): valid, suspicious and invalid,
    // an escalation counted at its state; a row served unkept never in
    // force; retention moving the count.
    use meridian_domain::v1::QualityState;
    let store = store();
    let dataset = unique("coinbase-1:daily");
    let subject = unique("LCL-BTC");
    let mut flagged = close(&dataset, "a", &subject, "2026-10-13", 10);
    flagged.meta_mut().quality = QualityState::Suspicious as i32;
    let rows = vec![
        flagged,
        close(&dataset, "b", &subject, "2026-10-14", 11),
        close(&dataset, "c", &subject, "2026-10-15", 12),
    ];
    let done = store.record(&dataset, rows, 1_000).unwrap();
    assert_eq!(counts_of(&store, &dataset), (2, 1, 0));
    store
        .assess(&[escalation(
            &dataset,
            &done.rows[1],
            QualityState::Invalid,
            2_000,
        )])
        .unwrap();
    assert_eq!(counts_of(&store, &dataset), (1, 1, 1));
    store
        .serve_unkept(
            &dataset,
            vec![close(&dataset, "d", &subject, "2026-10-16", 13)],
            &["reporting-1".to_string()],
            None,
            2_500,
        )
        .unwrap();
    assert_eq!(counts_of(&store, &dataset), (1, 1, 1), "served, not kept");
    store
        .remove_before(&dataset, 2_000, "retention", 3_000)
        .unwrap();
    assert_eq!(counts_of(&store, &dataset), (0, 0, 0));
}

#[test]
fn a_new_version_moves_its_row_key_from_the_last_versions_state_to_its_own() {
    // Contract v29 (W10.9; ruling 619 (2), the counts of rows in force): a
    // row key counts once, at its latest version kept, so a correction
    // stops it counting under its last version's state.
    use meridian_domain::v1::QualityState;
    let store = store();
    let dataset = unique("coinbase-1:daily");
    let subject = unique("LCL-BTC");
    let at = |state: QualityState, mut row: Observation| {
        row.meta_mut().quality = state as i32;
        row
    };
    let first = store
        .record(
            &dataset,
            vec![
                close(&dataset, "a", &subject, "2026-10-13", 10),
                at(
                    QualityState::Invalid,
                    close(&dataset, "b", &subject, "2026-10-14", 11),
                ),
                close(&dataset, "c", &subject, "2026-10-15", 12),
            ],
            1_000,
        )
        .unwrap();
    assert_eq!(counts_of(&store, &dataset), (2, 0, 1));

    // A quarantined row republished valid: corrected, it counts valid.
    let fixed = store
        .record(
            &dataset,
            vec![close(&dataset, "b", &subject, "2026-10-14", 21)],
            2_000,
        )
        .unwrap();
    assert_eq!((fixed.recorded, fixed.restated), (0, 1));
    assert_eq!(counts_of(&store, &dataset), (3, 0, 0));

    // A valid row republished suspicious.
    store
        .record(
            &dataset,
            vec![at(
                QualityState::Suspicious,
                close(&dataset, "a", &subject, "2026-10-13", 20),
            )],
            2_000,
        )
        .unwrap();
    assert_eq!(counts_of(&store, &dataset), (2, 1, 0));

    // A row escalated invalid, then corrected valid by its source; an
    // escalation of a version since replaced moves nothing.
    store
        .assess(&[escalation(
            &dataset,
            &first.rows[2],
            QualityState::Invalid,
            2_500,
        )])
        .unwrap();
    assert_eq!(counts_of(&store, &dataset), (1, 1, 1));
    store
        .record(
            &dataset,
            vec![close(&dataset, "c", &subject, "2026-10-15", 22)],
            3_000,
        )
        .unwrap();
    assert_eq!(counts_of(&store, &dataset), (2, 1, 0));
    store
        .assess(&[escalation(
            &dataset,
            &first.rows[0],
            QualityState::Invalid,
            3_500,
        )])
        .unwrap();
    assert_eq!(counts_of(&store, &dataset), (2, 1, 0), "a replaced version");

    // A restart: the counts are the database's, read again the same.
    drop(store);
    let store = self::store();
    assert_eq!(counts_of(&store, &dataset), (2, 1, 0));

    // Retention of the replaced versions leaves every key in force; of
    // every version, none.
    store
        .remove_before(&dataset, 2_000, "retention", 4_000)
        .unwrap();
    assert_eq!(counts_of(&store, &dataset), (2, 1, 0));
    store
        .remove_before(&dataset, 3_001, "retention", 4_000)
        .unwrap();
    assert_eq!(counts_of(&store, &dataset), (0, 0, 0));
}

#[test]
fn a_quarantined_row_resent_valid_with_the_same_value_is_the_next_version() {
    // Ruling 1006 (a): a republish whose only change is its quality state
    // is a restatement, never dropped as unchanged; the same state again
    // changes nothing.
    use meridian_domain::v1::QualityState;
    let store = store();
    let dataset = unique("coinbase-1:daily");
    let subject = unique("LCL-BTC");
    let row = || close(&dataset, "a", &subject, "2026-10-13", 10);
    let mut quarantined = row();
    quarantined.meta_mut().quality = QualityState::Invalid as i32;
    store.record(&dataset, vec![quarantined], 1_000).unwrap();
    assert_eq!(counts_of(&store, &dataset), (0, 0, 1));

    let resent = store.record(&dataset, vec![row()], 2_000).unwrap();
    assert_eq!(
        (resent.recorded, resent.restated, resent.unchanged),
        (0, 1, 0)
    );
    assert_eq!(resent.rows[0].meta().version, 2);
    assert_eq!(counts_of(&store, &dataset), (1, 0, 0));
    let query = Query {
        datasets: vec![dataset.clone()],
        subjects: vec![subject.clone()],
        data_type: DataType::Price,
        kinds: vec![],
        interval_ns: 0,
        when: When::Latest { at_ns: 0 },
        as_of_ns: 0,
        published_as_of_ns: 0,
        latest_only: true,
    };
    let read = store.read(&query).unwrap();
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].meta().version, 2);
    assert_ne!(read[0].meta().quality, QualityState::Invalid as i32);

    let again = store.record(&dataset, vec![row()], 3_000).unwrap();
    assert_eq!((again.unchanged, again.head), (1, 2), "nothing numbered");
}

/// A rate of `subject` under `row_key` (contract v27, W10.4): `value`
/// hundredths of a percent, published at `published_ns`, in `state`.
fn rate_row(
    dataset: &str,
    row_key: &str,
    subject: &str,
    value: u64,
    published_ns: i64,
    state: meridian_domain::v1::QualityState,
) -> Observation {
    use meridian_domain::v1::{Rate, RateKind, SourceTime, SourceTimeKind, UnitValue, ValueUnit};
    Observation::Rate(Box::new(Rate {
        meta: Some(ObservationMeta {
            row_key: row_key.into(),
            subjects: vec![SubjectRef {
                entity_id: subject.into(),
                ..Default::default()
            }],
            source: Some(Source {
                instance: "ny-fed-1".into(),
                dataset: dataset.into(),
                ..Default::default()
            }),
            valid_from_ns: 100,
            business_date: "2026-10-07".into(),
            source_times: vec![SourceTime {
                kind: SourceTimeKind::Published as i32,
                at_ns: published_ns,
            }],
            quality: state as i32,
            ..Default::default()
        }),
        kind: RateKind::Fixing as i32,
        value: Some(UnitValue {
            value: Some(meridian_pb::v1::Decimal {
                high: 0,
                low: value,
                scale: 2,
            }),
            unit: ValueUnit::Percent as i32,
            ..Default::default()
        }),
        ..Default::default()
    }))
}

#[test]
fn a_rates_version_in_force_is_its_latest_by_published_time() {
    // Ruling 1006 (b): a rate's version in force is the one a default read
    // serves, its latest by published time, so a late-arriving older
    // publication is kept and never takes force -- read by the latest's
    // index and by every row alike -- and the counts follow it; `migrate`
    // gives a rate row recorded before migration 8 its published time.
    use meridian_domain::v1::QualityState;
    let store = store();
    let dataset = unique("ny-fed-1:rates");
    let subject = unique("INS-SOFR");
    let row = |value, published, state| rate_row(&dataset, "k", &subject, value, published, state);
    let first = store
        .record(&dataset, vec![row(432, 2_000, QualityState::Valid)], 10_000)
        .unwrap();
    let late = store
        .record(
            &dataset,
            vec![row(431, 1_000, QualityState::Invalid)],
            20_000,
        )
        .unwrap();
    assert_eq!((late.restated, late.rows[0].meta().version), (1, 2));
    let in_force = |store: &PostgresStore| {
        let mut seen = Vec::new();
        for latest_only in [true, false] {
            let read = store
                .read(&Query {
                    datasets: vec![dataset.clone()],
                    subjects: vec![subject.clone()],
                    data_type: DataType::Rate,
                    kinds: vec![],
                    interval_ns: 0,
                    when: When::Latest { at_ns: 0 },
                    as_of_ns: 0,
                    published_as_of_ns: 0,
                    latest_only,
                })
                .unwrap();
            assert_eq!(read.len(), 1, "latest_only {latest_only}");
            seen.push(read[0].meta().version);
        }
        assert_eq!(seen[0], seen[1], "the index and every row agree");
        seen[0]
    };
    assert_eq!(in_force(&store), 1);
    assert_eq!(
        counts_of(&store, &dataset),
        (1, 0, 0),
        "the late one counts nowhere"
    );

    // An escalation of the version in force counts; of the late one, not.
    store
        .assess(&[escalation(
            &dataset,
            &first.rows[0],
            QualityState::Suspicious,
            21_000,
        )])
        .unwrap();
    store
        .assess(&[escalation(
            &dataset,
            &late.rows[0],
            QualityState::Invalid,
            21_000,
        )])
        .unwrap();
    assert_eq!(counts_of(&store, &dataset), (0, 1, 0));

    // Recorded before migration 8: its published time read from its body.
    let url = std::env::var("MERIDIAN_TEST_DATABASE_URL").unwrap();
    let mut raw = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    raw.execute(
        "UPDATE lake_row SET published_at_ns = NULL WHERE dataset = $1",
        &[&dataset],
    )
    .unwrap();
    store.migrate(&SystemClock).unwrap();
    let kept: Vec<(i64, Option<i64>)> = raw
        .query(
            "SELECT version, published_at_ns FROM lake_row WHERE dataset = $1 ORDER BY version",
            &[&dataset],
        )
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    assert_eq!(kept, vec![(1, Some(2_000)), (2, Some(1_000))]);
    assert_eq!(in_force(&store), 1);

    // A newer publication takes force, its state counted in place of the
    // escalated one's.
    store
        .record(&dataset, vec![row(433, 3_000, QualityState::Valid)], 30_000)
        .unwrap();
    assert_eq!(in_force(&store), 3);
    assert_eq!(counts_of(&store, &dataset), (1, 0, 0));
}

#[test]
fn migrating_counts_the_rows_in_force_the_lake_already_keeps() {
    // Contract v29 (ruling 619 (2)): migration 8 fills each dataset's rows
    // in force from the rows kept, one per row key, so a lake upgraded with
    // corrected rows counts each key at its latest version.
    let seq = COUNTER.fetch_add(1, Ordering::SeqCst);
    let name = format!("lake_in_force_{}_{seq}", SystemClock.now_ns());
    let url = own_schema(&name);
    let mut client = postgres::Client::connect(&url, postgres::NoTls).expect("connects");

    // The lake as the release before this one left it.
    let before: Vec<_> = meridian_lake::migrations::MIGRATIONS
        .iter()
        .take_while(|m| m.name != "quality_counts")
        .collect();
    assert_eq!(before.len(), 7, "every migration before the counts");
    client
        .batch_execute(meridian_lake::migrations::HISTORY)
        .unwrap();
    for migration in before {
        let mut tx = client.transaction().unwrap();
        tx.batch_execute(migration.sql).unwrap();
        meridian_lake::migrations::record(&mut tx, migration, 1).unwrap();
        tx.commit().unwrap();
    }
    // Key a: invalid, then corrected valid. Key b: suspicious. Key c:
    // valid, escalated invalid. Key d: valid, then corrected suspicious.
    client
        .batch_execute(
            "INSERT INTO lake_partition (dataset, head) VALUES ('x-1:daily', 6);
             INSERT INTO lake_row (dataset, sequence, data_type, row_key, version, subject,
                                   subjects, kind, interval_ns, venue_id, valid_from_ns,
                                   valid_until_ns, recorded_at_ns, previous_sequence,
                                   unconverted, body, latest_key, quality)
             VALUES ('x-1:daily', 1, 1, 'a', 1, 'S', '{S}', 1, 0, '', 1, 2, 10, 0, false, '', '', 3),
                    ('x-1:daily', 2, 1, 'b', 1, 'S', '{S}', 1, 0, '', 1, 2, 10, 1, false, '', '', 2),
                    ('x-1:daily', 3, 1, 'c', 1, 'S', '{S}', 1, 0, '', 1, 2, 10, 2, false, '', '', 1),
                    ('x-1:daily', 4, 1, 'd', 1, 'S', '{S}', 1, 0, '', 1, 2, 10, 3, false, '', '', 1),
                    ('x-1:daily', 5, 1, 'a', 2, 'S', '{S}', 1, 0, '', 1, 2, 20, 4, false, '', '', 1),
                    ('x-1:daily', 6, 1, 'd', 2, 'S', '{S}', 1, 0, '', 1, 2, 20, 5, false, '', '', 2);
             INSERT INTO lake_quality_assessment
                    (dataset, sequence, row_key, version, state, reasons, by_instance, at_ns)
             VALUES ('x-1:daily', 3, 'c', 1, 3, '{}', 'signal-1', 15);",
        )
        .unwrap();

    let store = PostgresStore::connect(&url, 2).expect("connects");
    assert!(store.verify().is_err(), "migrations behind");
    store.migrate(&SystemClock).expect("migrates");
    store.verify().expect("recognised after migrating");
    assert_eq!(counts_of(&store, "x-1:daily"), (1, 2, 1));
}

/// A chain row: a trade of the underlying keyed by `series` (none for the
/// underlying's own), at `valid_from_ns`.
fn chain_trade(
    dataset: &str,
    row_key: &str,
    subject: &str,
    series: &str,
    valid_from_ns: i64,
) -> Observation {
    let mut row = trade(dataset, row_key, subject, valid_from_ns);
    if !series.is_empty() {
        row.meta_mut().series_key = vec![meridian_domain::v1::Identifier {
            scheme: "occ_osi".into(),
            value: series.into(),
            ..Default::default()
        }];
    }
    row
}

fn latest_query(dataset: &str, subject: &str, latest_only: bool) -> Query {
    Query {
        datasets: vec![dataset.to_string()],
        subjects: vec![subject.to_string()],
        data_type: DataType::Trade,
        kinds: vec![],
        interval_ns: 0,
        when: When::Latest { at_ns: 0 },
        as_of_ns: 0,
        published_as_of_ns: 0,
        latest_only,
    }
}

fn keys_of(rows: &[Observation]) -> Vec<String> {
    let mut keys: Vec<String> = rows.iter().map(|r| r.meta().row_key.clone()).collect();
    keys.sort();
    keys
}

/// Ruling 770: the latest read served by an index, and its choice the same
/// for a row naming the asked subject after its first (found by the later
/// subjects' index, a key of that first subject), and for a row key whose
/// later version moved it to another first subject (its earlier version no
/// longer in force under either).
#[test]
fn a_latest_read_by_the_index_keeps_a_later_subjects_rows_and_a_moved_row_key() {
    let store = store();
    let dataset = unique("tradier-1:options");
    let subject = unique("INS-UNDERLYING");
    let other = unique("INS-OTHER");
    let naming_other_first = |mut row: Observation| {
        row.meta_mut().subjects.insert(
            0,
            meridian_domain::v1::SubjectRef {
                entity_id: other.clone(),
                ..Default::default()
            },
        );
        row
    };
    let rows = vec![
        naming_other_first(chain_trade(&dataset, "x1", &subject, "A", 10)),
        naming_other_first(chain_trade(&dataset, "x2", &subject, "A", 20)),
        chain_trade(&dataset, "y1", &subject, "A", 15),
        chain_trade(&dataset, "m", &subject, "B", 30),
        chain_trade(&dataset, "n", &subject, "B", 25),
    ];
    assert_eq!(store.record(&dataset, rows, 1_000).unwrap().recorded, 5);
    // Its first subject replaced by the other record (W10.8): the key keeps
    // its range, followed by alias, and is no first-subject change (the
    // lake's rule 5, ruled 2026-11-12).
    store.keep_alias(&subject, &other, 1_050).unwrap();
    let moved = store
        .record(
            &dataset,
            vec![naming_other_first(chain_trade(
                &dataset, "m", &subject, "B", 30,
            ))],
            1_100,
        )
        .unwrap();
    assert_eq!(moved.restated, 1);
    let every = store
        .read(&latest_query(&dataset, &subject, false))
        .unwrap();
    assert_eq!(keys_of(&every), vec!["m", "n", "x1", "x2", "y1"]);
    let chosen = store.read(&latest_query(&dataset, &subject, true)).unwrap();
    assert_eq!(
        keys_of(&chosen),
        vec!["m", "n", "x2", "y1"],
        "per first subject and series the latest, m at its version in force"
    );
    let m = chosen.iter().find(|r| r.meta().row_key == "m").unwrap();
    assert_eq!(m.meta().version, 2);
    // As of before the move, m was the asked subject's own series B latest.
    let mut before = latest_query(&dataset, &subject, true);
    before.as_of_ns = 1_050;
    assert_eq!(
        keys_of(&store.read(&before).unwrap()),
        vec!["m", "x2", "y1"]
    );
}

/// Found on dev 2026-10-26: a read of the latest loaded every row its
/// datasets kept for the subject -- a chain's quotes, 81,720 rows in two
/// hours -- and chose the latest in memory. Chosen in SQL: each row key at
/// its version in force, then per series (and asset, venue, kind) the latest
/// and the latest not invalid, by its own state or an assessment, so the
/// read's choice over these is its choice over every row. A row without the
/// key it is chosen by is a key of its own, and `migrate` fills it in.
#[test]
fn a_read_of_the_latest_is_chosen_in_sql_per_series_keeping_the_last_valid_row() {
    use meridian_domain::v1::QualityState;
    use meridian_lake::store::Assessment;
    let store = store();
    let dataset = unique("tradier-1:options");
    let subject = unique("INS-UNDERLYING");
    let mut invalid = chain_trade(&dataset, "a3", &subject, "A", 30);
    invalid.meta_mut().quality = QualityState::Invalid as i32;
    let rows = vec![
        chain_trade(&dataset, "t1", &subject, "", 10),
        chain_trade(&dataset, "t2", &subject, "", 20),
        chain_trade(&dataset, "a1", &subject, "A", 10),
        chain_trade(&dataset, "a2", &subject, "A", 20),
        invalid,
        chain_trade(&dataset, "r", &subject, "A", 40),
        chain_trade(&dataset, "b1", &subject, "B", 10),
        chain_trade(&dataset, "b2", &subject, "B", 20),
    ];
    let done = store.record(&dataset, rows, 1_000).unwrap();
    assert_eq!(done.recorded, 8);
    // `r` restated invalid, its valid time kept (the lake's rule 5): series
    // A's latest is it, and its latest valid the one before both invalid.
    let mut r_invalid = chain_trade(&dataset, "r", &subject, "A", 40);
    r_invalid.meta_mut().quality = QualityState::Invalid as i32;
    let restated = store.record(&dataset, vec![r_invalid], 1_100).unwrap();
    assert_eq!(restated.restated, 1);
    let b2 = done
        .rows
        .iter()
        .find(|r| r.meta().row_key == "b2")
        .unwrap()
        .meta()
        .sequence;
    store
        .assess(&[Assessment {
            dataset: dataset.clone(),
            sequence: b2,
            row_key: "b2".into(),
            version: 1,
            state: QualityState::Invalid as i32,
            reasons: vec![],
            by_instance: "signal-1".into(),
            at_ns: 1_200,
        }])
        .unwrap();

    let every = store
        .read(&latest_query(&dataset, &subject, false))
        .unwrap();
    assert_eq!(
        keys_of(&every),
        vec!["a1", "a2", "a3", "b1", "b2", "r", "t1", "t2"],
        "every row key at its version in force"
    );
    let chosen = store.read(&latest_query(&dataset, &subject, true)).unwrap();
    assert_eq!(
        keys_of(&chosen),
        vec!["a2", "b1", "b2", "r", "t2"],
        "per series the latest, and the latest not invalid"
    );
    let state = |key: &str| {
        chosen
            .iter()
            .find(|r| r.meta().row_key == key)
            .unwrap()
            .meta()
            .quality
    };
    assert_eq!(state("b2"), QualityState::Invalid as i32, "as assessed");
    assert_eq!(state("r"), QualityState::Invalid as i32, "as restated");
    // As of before the restatement and the assessment: r at its first
    // version is series A's latest and valid, and b2 stood.
    let mut before = latest_query(&dataset, &subject, true);
    before.as_of_ns = 1_050;
    assert_eq!(
        keys_of(&store.read(&before).unwrap()),
        vec!["b2", "r", "t2"],
        "r as recorded first, b2 not yet assessed"
    );

    // Rows recorded without the key (before migration 4, or by the release
    // before during a rolling upgrade): each a key of its own, then filled
    // in by the next migrate.
    //
    // Under migrate's own lock until they are read: every other test's
    // store() migrates, and its backfill fills in every row with no key, so
    // one running between the keys taken away and the read had the read
    // choose as if they were never gone (a flake; ruling 1069, 448). Taking
    // the lock waits for a migrate under way and holds off the next.
    let url = std::env::var("MERIDIAN_TEST_DATABASE_URL").unwrap();
    let mut raw = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    raw.execute("SELECT pg_advisory_lock($1)", &[&SCHEMA_LOCK])
        .unwrap();
    raw.execute(
        "UPDATE lake_row SET latest_key = NULL, quality = NULL WHERE dataset = $1",
        &[&dataset],
    )
    .unwrap();
    let unkeyed = store.read(&latest_query(&dataset, &subject, true)).unwrap();
    raw.execute("SELECT pg_advisory_unlock($1)", &[&SCHEMA_LOCK])
        .unwrap();
    assert_eq!(keys_of(&unkeyed), keys_of(&every), "every row, as before");
    store.migrate(&SystemClock).unwrap();
    let missing: i64 = raw
        .query_one(
            "SELECT count(*) FROM lake_row WHERE dataset = $1 AND latest_key IS NULL",
            &[&dataset],
        )
        .unwrap()
        .get(0);
    assert_eq!(missing, 0, "filled in by migrate");
    let filled = store.read(&latest_query(&dataset, &subject, true)).unwrap();
    assert_eq!(keys_of(&filled), keys_of(&chosen));
}

/// A chain of `series` series quoted `times` times each, recorded a quote
/// round (one batch) at a time: how long the recording took.
fn record_chain(
    store: &PostgresStore,
    dataset: &str,
    subject: &str,
    series: usize,
    times: usize,
) -> std::time::Duration {
    let started = std::time::Instant::now();
    for time in 0..times {
        let batch: Vec<Observation> = (0..series)
            .map(|s| {
                chain_trade(
                    dataset,
                    &format!("S{s:03}:q:{time}"),
                    subject,
                    &format!("S{s:03}"),
                    1 + time as i64,
                )
            })
            .collect();
        store.record(dataset, batch, 1_000 + time as i64).unwrap();
    }
    started.elapsed()
}

/// What one size of chain measured: its record and read rates, and its
/// latest read's time.
struct LakeMeasure {
    rows: usize,
    record_rows_per_s: f64,
    read_rows_per_s: f64,
    latest: std::time::Duration,
    latest_rows: usize,
}

fn measure_chain(store: &PostgresStore, series: usize, times: usize) -> LakeMeasure {
    let dataset = unique("tradier-1:options");
    let subject = unique("INS-UNDERLYING");
    let recorded = record_chain(store, &dataset, &subject, series, times);
    let started = std::time::Instant::now();
    let every = store
        .read(&latest_query(&dataset, &subject, false))
        .unwrap();
    let read = started.elapsed();
    let started = std::time::Instant::now();
    let chosen = store.read(&latest_query(&dataset, &subject, true)).unwrap();
    let latest = started.elapsed();
    assert_eq!(every.len(), series * times);
    assert_eq!(chosen.len(), series);
    assert!(chosen
        .iter()
        .all(|row| row.meta().valid_from_ns == times as i64));
    LakeMeasure {
        rows: every.len(),
        record_rows_per_s: every.len() as f64 / recorded.as_secs_f64(),
        read_rows_per_s: every.len() as f64 / read.as_secs_f64(),
        latest,
        latest_rows: chosen.len(),
    }
}

fn said(m: &LakeMeasure) -> String {
    format!(
        "rows={} record_rows_per_s={:.0} read_rows_per_s={:.0} latest_ms={:.1} latest_rows={}",
        m.rows,
        m.record_rows_per_s,
        m.read_rows_per_s,
        m.latest.as_secs_f64() * 1e3,
        m.latest_rows
    )
}

/// The bench behind the fix, and the gate's regression guard on the lake's
/// stated capacity (H1, ruling 586): a chain of 200 series quoted 100 times
/// each, recorded a round at a time, read whole and read for the latest.
/// The floors are a quarter of what the gate's container measured
/// (INSTALL.md, "Capacity"), so a slower CI runner passes and a
/// regression of four times does not.
///
/// It runs alone (.config/nextest.toml, `lake-speed`), and says what it
/// measured, beside its floors, on every run, passing or not: test-store
/// prints the line, and its JUnit keeps it.
#[test]
fn a_chains_latest_loads_one_row_per_series() {
    let store = store();
    let measured = measure_chain(&store, 200, 100);
    println!(
        "SPEED lake {} floors: record_rows_per_s>={GUARD_RECORD_ROWS_PER_S:.0} \
         read_rows_per_s>={GUARD_READ_ROWS_PER_S:.0} latest_ms<={}",
        said(&measured),
        GUARD_LATEST.as_millis()
    );
    assert!(
        measured.record_rows_per_s >= GUARD_RECORD_ROWS_PER_S,
        "the lake records {:.0} rows a second; its guard is {GUARD_RECORD_ROWS_PER_S}",
        measured.record_rows_per_s
    );
    assert!(
        measured.read_rows_per_s >= GUARD_READ_ROWS_PER_S,
        "the lake reads {:.0} rows a second; its guard is {GUARD_READ_ROWS_PER_S}",
        measured.read_rows_per_s
    );
    assert!(
        measured.latest <= GUARD_LATEST,
        "the latest of a 200-series chain at 20,000 rows took {:?}; its guard is {GUARD_LATEST:?}",
        measured.latest
    );
}

/// The guard's floors, set from the capacity stated on 2026-10-26 at 20,000
/// rows (a quarter of 2,449 rows a second recorded and 102,429 read, and
/// four times its latest read of 222 ms), and kept as they were when the
/// record path was batched (2026-11-10; INSTALL.md now states about 15,000
/// recorded and the latest in 14 ms).
const GUARD_RECORD_ROWS_PER_S: f64 = 600.0;
const GUARD_READ_ROWS_PER_S: f64 = 25_000.0;
const GUARD_LATEST: std::time::Duration = std::time::Duration::from_millis(900);

/// The lake's stated capacity as a dataset grows (H1): `make bench` runs
/// it, and INSTALL.md's "Capacity" table is what it printed. A 200-series
/// chain at each size, a fresh dataset each.
#[test]
#[ignore = "the capacity bench: make bench"]
fn capacity_lake_as_a_dataset_grows() {
    let store = store();
    let sizes: Vec<usize> = std::env::var("MERIDIAN_BENCH_LAKE_ROWS")
        .unwrap_or_else(|_| "20000,100000,400000".into())
        .split(',')
        .filter_map(|n| n.trim().parse().ok())
        .collect();
    for rows in sizes {
        let measured = measure_chain(&store, 200, (rows / 200).max(1));
        println!("CAPACITY lake {}", said(&measured));
    }
}

// ── A chain page's reads, as a plugin's page makes them (rulings 798, 808) ──

/// The bytes this test binary holds allocated, now and at its peak since
/// the peak was last reset: what a read keeps, apart from what the
/// allocator holds back from the system (resident memory).
/// A listed series row under `subject` keyed by `key`, expiring on `date`:
/// a future where it names its product code, else an option.
fn listed_series(dataset: &str, subject: &str, key: &str, date: &str, future: bool) -> Observation {
    use meridian_domain::v1::{Identifier, ListedSeries, TemplateFieldValue};
    let field = |path: &str, value: &str| TemplateFieldValue {
        path: path.into(),
        value: value.into(),
        ..Default::default()
    };
    let mut fields = vec![field("underlying_instrument_id", subject)];
    if future {
        fields.push(field("product_code", "ES"));
        fields.push(field("last_trade_date", date));
    } else {
        fields.push(field("expiration_date", date));
    }
    Observation::Series(ListedSeries {
        meta: Some(ObservationMeta {
            row_key: format!("series:{key}"),
            subjects: vec![SubjectRef {
                entity_id: subject.into(),
                ..Default::default()
            }],
            source: Some(Source {
                instance: "tradier-1".into(),
                dataset: dataset.into(),
                ..Default::default()
            }),
            valid_from_ns: 100,
            series_key: vec![Identifier {
                scheme: "occ_osi".into(),
                value: key.into(),
                ..Default::default()
            }],
            ..Default::default()
        }),
        template_id: "TPL-LISTED".into(),
        template_version: "1".into(),
        fields,
        instrument_id: String::new(),
    })
}

/// Rulings 856 and 858: the listing an expirations read counts, read from
/// `lake_row_listing` alone, is what reading the rows whole answers -- each
/// series row key at its version in force by the cut-off, an invalid one
/// (its own state or an assessment's) left out, a series two datasets list
/// once each, a future told by its product code -- and a row recorded
/// before migration 7 is read whole until `migrate` fills it in.
#[test]
fn the_listing_is_read_from_its_index_as_the_rows_answer_it() {
    use meridian_domain::v1::{InstrumentType, QualityState};
    use meridian_lake::store::{listing_read, Assessment, Listed};
    let store = store();
    let first = unique("tradier-1:options");
    let second = unique("cboe-1:options");
    let subject = unique("INS-UNDERLYING");
    let other = unique("INS-OTHER");
    let mut invalid = listed_series(&first, &subject, "BAD", "2026-11-20", false);
    invalid.meta_mut().quality = QualityState::Invalid as i32;
    let done = store
        .record(
            &first,
            vec![
                listed_series(&first, &subject, "A", "2026-11-20", false),
                listed_series(&first, &subject, "B", "2026-11-20", false),
                listed_series(&first, &subject, "C", "2026-12-18", false),
                listed_series(&first, &subject, "F", "2026-12-18", true),
                listed_series(&first, &subject, "D", "2026-11-20", false),
                invalid,
                listed_series(&first, &other, "Z", "2026-11-20", false),
            ],
            1_000,
        )
        .unwrap();
    assert_eq!(done.recorded, 7);
    // The same series listed by a second dataset.
    store
        .record(
            &second,
            vec![listed_series(&second, &subject, "A", "2026-11-20", false)],
            1_000,
        )
        .unwrap();
    // C restated twice, to January and back (versions 2 and 3; dev's
    // chains are re-listed each pass): only its version in force counts.
    for (date, at) in [("2027-01-15", 2_500), ("2026-12-18", 2_600)] {
        let restated = store
            .record(
                &first,
                vec![listed_series(&first, &subject, "C", date, false)],
                at,
            )
            .unwrap();
        assert_eq!(restated.restated, 1);
    }
    // An adjustment: B moves to December, recorded after the first cut-off.
    let moved = store
        .record(
            &first,
            vec![listed_series(&first, &subject, "B", "2026-12-18", false)],
            3_000,
        )
        .unwrap();
    assert_eq!(moved.restated, 1);
    // D found invalid by an assessment after the first cut-off.
    let d = done
        .rows
        .iter()
        .find(|row| row.meta().row_key == "series:D")
        .unwrap()
        .meta()
        .clone();
    store
        .assess(&[Assessment {
            dataset: first.clone(),
            sequence: d.sequence,
            row_key: d.row_key.clone(),
            version: d.version,
            state: QualityState::Invalid as i32,
            reasons: vec![],
            by_instance: "signal-1".into(),
            at_ns: 3_000,
        }])
        .unwrap();
    let datasets = vec![first.clone(), second.clone()];
    let subjects = vec![subject.clone()];
    let sorted = |mut listed: Vec<Listed>| {
        listed.sort_by(|a, b| (&a.dataset, &a.series).cmp(&(&b.dataset, &b.series)));
        listed
    };
    let both = |as_of_ns: i64| {
        let indexed = sorted(store.listing(&datasets, &subjects, as_of_ns).unwrap());
        let whole = sorted(listing_read(&store, &datasets, &subjects, as_of_ns).unwrap());
        assert_eq!(indexed, whole, "as of {as_of_ns}");
        indexed
    };
    let at = |listed: &[Listed], date: &str| {
        let mut series: Vec<String> = listed
            .iter()
            .filter(|l| {
                l.expires.map(|d| d.to_string()).as_deref() == Some(date)
                    && l.listed_type == InstrumentType::ListedOption
            })
            .map(|l| l.series.clone())
            .collect();
        series.sort();
        series.dedup();
        series.len()
    };
    let then = both(2_000);
    assert_eq!(then.len(), 6, "A twice, B, C, D and F; BAD and Z left out");
    assert_eq!((at(&then, "2026-11-20"), at(&then, "2026-12-18")), (3, 1));
    let between = both(2_550);
    assert_eq!(
        (at(&between, "2026-11-20"), at(&between, "2026-12-18")),
        (3, 0)
    );
    assert_eq!(at(&between, "2027-01-15"), 1, "C's second version in force");
    let now = both(0);
    assert_eq!((at(&now, "2026-11-20"), at(&now, "2026-12-18")), (1, 2));
    assert!(now
        .iter()
        .any(|l| l.listed_type == InstrumentType::Future && l.dataset == first));

    // A row recorded before migration 7: read whole, then filled in.
    let url = std::env::var("MERIDIAN_TEST_DATABASE_URL").unwrap();
    let mut sql = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let cleared = sql
        .execute(
            "UPDATE lake_row SET expires = NULL, listed_type = NULL WHERE dataset = $1",
            &[&first],
        )
        .unwrap();
    assert!(cleared > 0);
    assert_eq!(both(0), now, "read whole before migrate fills it in");
    store.migrate(&SystemClock).unwrap();
    let unfilled: i64 = sql
        .query_one(
            "SELECT count(*) FROM lake_row WHERE dataset = $1 AND listed_type IS NULL",
            &[&first],
        )
        .unwrap()
        .get(0);
    assert_eq!(unfilled, 0);
    assert_eq!(both(0), now);
}

mod counted {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub struct Counting;
    static LIVE: AtomicUsize = AtomicUsize::new(0);
    static PEAK: AtomicUsize = AtomicUsize::new(0);

    std::thread_local! {
        /// This thread's allocations, as glibc's 64-bit malloc places
        /// them: a header and rounding to 16 bytes, 32 at the least.
        static PLACED: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    }

    fn placed(size: usize) -> isize {
        ((size + 8 + 15) & !15).max(32) as isize
    }

    fn place(by: isize) {
        let _ = PLACED.try_with(|placed| placed.set(placed.get() + by));
    }

    /// What this thread has allocated and not freed, in bytes as glibc
    /// places them.
    pub fn placed_here() -> isize {
        PLACED.with(std::cell::Cell::get)
    }

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let at = unsafe { System.alloc(layout) };
            if !at.is_null() {
                let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
                PEAK.fetch_max(live, Ordering::Relaxed);
                place(placed(layout.size()));
            }
            at
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) };
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            place(-placed(layout.size()));
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let at = unsafe { System.realloc(ptr, layout, new_size) };
            if !at.is_null() {
                if new_size > layout.size() {
                    let live = LIVE.fetch_add(new_size - layout.size(), Ordering::Relaxed)
                        + (new_size - layout.size());
                    PEAK.fetch_max(live, Ordering::Relaxed);
                } else {
                    LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
                }
                place(placed(new_size) - placed(layout.size()));
            }
            at
        }
    }

    /// Live and peak MiB; the peak starts again from now.
    pub fn take() -> (f64, f64) {
        let live = LIVE.load(Ordering::Relaxed);
        let peak = PEAK.swap(live, Ordering::Relaxed);
        (live as f64 / 1048576.0, peak as f64 / 1048576.0)
    }
}

#[global_allocator]
static COUNTING: counted::Counting = counted::Counting;

mod chain_page {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use meridian_bus::{Bus, MemoryBackend, Stamp};
    use meridian_clock::{Clock, SystemClock};
    use meridian_domain::v1::{
        AssetClass, DatasetEntitlement, DatasetRef, EntitlementsChangedEvent, Identifier,
        InstrumentRecord, ListExpirationsReply, ListExpirationsRequest, ListQuotesReply,
        ListQuotesRequest, ListSeriesReply, ListSeriesRequest, ListStatisticsReply,
        ListStatisticsRequest, ListedSeries, Money, ObservationMeta, Quote, ResolveIdentifierReply,
        ResolveInstrumentReply, Source, SourceChoice, Statistic, StatisticKind, SubjectRef,
        TemplateFieldValue, UnitValue, ValueUnit,
    };
    use meridian_lake::row::Observation;
    use meridian_lake::service;
    use meridian_lake::{PostgresStore, Store};
    use meridian_pb::v1::{DatasetDeclaration, Decimal, ObservationMode};
    use prost::Message;

    /// The reader: a plugin's instance, as its sidecar asks.
    pub const READER: &str = "sample-options";
    const DAY: i64 = 86_400 * 1_000_000_000;
    const MINUTE: i64 = 60 * 1_000_000_000;

    /// A TQQQ-shaped chain: `expiries` expirations of `strikes` strikes, a
    /// call and a put each; quoted `rounds` times, a pass a minute, half the
    /// series changing each pass (what an unchanged quote's dedup leaves).
    #[derive(Clone, Copy, Debug)]
    pub struct Shape {
        pub expiries: usize,
        pub strikes: usize,
        pub rounds: usize,
        pub days: usize,
    }

    impl Shape {
        pub fn series(&self) -> usize {
            self.expiries * self.strikes * 2
        }
    }

    pub struct Page {
        pub bus: Arc<Bus>,
        pub resolves: Arc<AtomicU64>,
        pub underlying: String,
        /// The lake served, and its hot layer where the bench asked for it.
        pub lake: Arc<service::Lake>,
        pub store: Arc<dyn Store>,
        pub hot: Option<Arc<meridian_lake::hot::HotStore>>,
    }

    fn decimal(units: u64, scale: u32) -> Option<Decimal> {
        Some(Decimal {
            high: 0,
            low: units,
            scale,
        })
    }

    fn osi(underlying: &str, e: usize, s: usize, right: char) -> String {
        format!(
            "{underlying:<6}27{:02}{:02}{right}{:08}",
            1 + e % 12,
            1 + e % 28,
            (10 + s) * 1000
        )
    }

    fn key(value: &str) -> Vec<Identifier> {
        vec![Identifier {
            scheme: "occ_osi".into(),
            value: value.into(),
            ..Default::default()
        }]
    }

    fn meta(
        dataset: &str,
        row_key: &str,
        subject: &str,
        series: &str,
        at: i64,
    ) -> Option<ObservationMeta> {
        Some(ObservationMeta {
            row_key: row_key.into(),
            subjects: vec![SubjectRef {
                entity_id: subject.into(),
                ..Default::default()
            }],
            source: Some(Source {
                instance: "tradier".into(),
                dataset: dataset.into(),
                ..Default::default()
            }),
            valid_from_ns: at,
            series_key: key(series),
            ..Default::default()
        })
    }

    fn field(path: &str, value: String) -> TemplateFieldValue {
        TemplateFieldValue {
            path: path.into(),
            value,
            ..Default::default()
        }
    }

    fn usd(cents: u64) -> Option<Money> {
        Some(Money {
            amount: decimal(cents, 2),
            currency_code: "USD".into(),
            instrument_id: String::new(),
        })
    }

    fn configuration(dataset: &str) -> EntitlementsChangedEvent {
        let (instance, key) = dataset.split_once(':').unwrap();
        EntitlementsChangedEvent {
            datasets: vec![DatasetRef {
                dataset: dataset.into(),
                instance: instance.into(),
                vendor: "Tradier".into(),
                declaration: Some(DatasetDeclaration {
                    key: key.into(),
                    vendor: "Tradier".into(),
                    data_types: vec![
                        "meridian.v1.ListedSeries".into(),
                        "meridian.v1.Quote".into(),
                        "meridian.v1.Statistic".into(),
                    ],
                    modes: vec![ObservationMode::Pull as i32],
                    cadence: 60,
                    ..Default::default()
                }),
                ..Default::default()
            }],
            licences: vec![],
            entitlements: vec![DatasetEntitlement {
                dataset: dataset.into(),
                instance: READER.into(),
                allowed: true,
                ..Default::default()
            }],
            changed_at_ns: SystemClock.now_ns(),
        }
    }

    /// Record the fixture once per database: a dataset already holding it
    /// is read as it is, so a change is measured against the same rows.
    pub fn record_fixture(store: &PostgresStore, dataset: &str, underlying: &str, shape: Shape) {
        let expected = (shape.series()
            + shape.rounds * shape.series() / 2
            + 2 * shape.days * shape.series()) as u64;
        let head = store.heads().unwrap().get(dataset).copied().unwrap_or(0);
        if head >= expected {
            eprintln!("bench: fixture held, {head} rows");
            return;
        }
        assert_eq!(
            head, 0,
            "a partial fixture: drop the bench's database and run again"
        );
        let started = Instant::now();
        let now = SystemClock.now_ns();
        let first = now - shape.days as i64 * DAY;
        let keys: Vec<String> = (0..shape.expiries)
            .flat_map(|e| {
                (0..shape.strikes).flat_map(move |s| ['C', 'P'].into_iter().map(move |r| (e, s, r)))
            })
            .map(|(e, s, r)| osi(underlying, e, s, r))
            .collect();
        let series: Vec<Observation> = keys
            .iter()
            .enumerate()
            .map(|(i, k)| {
                let e = i / (shape.strikes * 2);
                Observation::Series(ListedSeries {
                    meta: meta(dataset, &format!("series:{k}"), underlying, k, first),
                    template_id: "TPL-listed-option".into(),
                    template_version: "1".into(),
                    fields: vec![
                        field("underlying_instrument_id", underlying.into()),
                        field(
                            "put_or_call",
                            if k.contains('C') { "call" } else { "put" }.into(),
                        ),
                        field("strike_price", format!("{}", 10 + (i / 2) % shape.strikes)),
                        field(
                            "expiration_date",
                            format!("2027-{:02}-{:02}", 1 + e % 12, 1 + e % 28),
                        ),
                        field("exercise_style", "american".into()),
                        field("settlement_method", "physical".into()),
                        field("contract_multiplier", "100".into()),
                        field("option_root", underlying.chars().take(6).collect()),
                    ],
                    instrument_id: String::new(),
                })
            })
            .collect();
        for batch in series.chunks(2000) {
            store.record(dataset, batch.to_vec(), now).unwrap();
        }
        // Quotes: a pass a minute, half the series changing each pass, the
        // passes spread over the days.
        let spacing = (shape.days as i64 * DAY / shape.rounds.max(1) as i64).max(MINUTE);
        let mut batch = Vec::new();
        for round in 0..shape.rounds {
            let at = first + round as i64 * spacing;
            for (i, k) in keys.iter().enumerate() {
                if (i + round) % 2 != 0 {
                    continue;
                }
                let bid = 100 + ((i * 7 + round * 3) % 500) as u64;
                batch.push(Observation::Quote(Box::new(Quote {
                    meta: meta(dataset, &format!("{k}:q:{at}"), underlying, k, at),
                    bid: usd(bid),
                    ask: usd(bid + 5),
                    bid_quantity: decimal(10 + (round % 90) as u64, 0),
                    ask_quantity: decimal(12, 0),
                    ..Default::default()
                })));
            }
            if batch.len() >= 5000 {
                store
                    .record(dataset, std::mem::take(&mut batch), at)
                    .unwrap();
            }
        }
        if !batch.is_empty() {
            store.record(dataset, batch, now).unwrap();
        }
        // Open interest and volume, a row each series each day.
        for day in 0..shape.days {
            let date =
                meridian_domain::date::Date::of_utc_instant(first + day as i64 * DAY).to_string();
            let mut rows = Vec::new();
            for (i, k) in keys.iter().enumerate() {
                for (kind, name) in [
                    (StatisticKind::OpenInterest, "oi"),
                    (StatisticKind::Volume, "volume"),
                ] {
                    let mut m = meta(
                        dataset,
                        &format!("{name}:{k}:{date}"),
                        underlying,
                        k,
                        first + day as i64 * DAY,
                    );
                    m.as_mut().unwrap().business_date = date.clone();
                    rows.push(Observation::Statistic(Statistic {
                        meta: m,
                        kind: kind as i32,
                        value: Some(UnitValue {
                            value: decimal((100 + i * 3 + day) as u64, 0),
                            unit: ValueUnit::Count as i32,
                            ..Default::default()
                        }),
                    }));
                }
            }
            for chunk in rows.chunks(5000) {
                store.record(dataset, chunk.to_vec(), now).unwrap();
            }
        }
        eprintln!(
            "bench: fixture recorded, {} rows in {:.1} s",
            store.heads().unwrap().get(dataset).copied().unwrap_or(0),
            started.elapsed().as_secs_f64()
        );
    }

    /// The lake served on an in-process bus over the store, its instrument
    /// store stood in: the underlying held, no series record held, each
    /// series' resolve answered after `resolve_delay` (the broker's round
    /// trip and the store's lookup on a deployment) and counted.
    pub fn page(
        store: Arc<PostgresStore>,
        tag: &str,
        shape: Shape,
        resolve_delay: Duration,
    ) -> Page {
        let dataset = format!("tradier:{tag}");
        let underlying = format!("INS-{tag}").to_uppercase();
        record_fixture(&store, &dataset, &underlying, shape);
        serve(store, &dataset, underlying, resolve_delay)
    }

    /// A chain's listing alone, each series recorded `versions` times --
    /// a data plugin re-listing its chain and restating a term each pass,
    /// as dev's 41,148 series rows for about 4,500 series do (found
    /// 2026-10-30) -- served as [`page`] serves its fixture.
    pub fn versions_page(
        store: Arc<PostgresStore>,
        tag: &str,
        shape: Shape,
        versions: usize,
    ) -> Page {
        let dataset = format!("tradier:{tag}");
        let underlying = format!("INS-{tag}").to_uppercase();
        let expected = (shape.series() * versions) as u64;
        let head = store.heads().unwrap().get(&dataset).copied().unwrap_or(0);
        if head < expected {
            assert_eq!(
                head, 0,
                "a partial fixture: drop the bench's database and run again"
            );
            let now = SystemClock.now_ns();
            for pass in 0..versions {
                let rows: Vec<Observation> = (0..shape.expiries)
                    .flat_map(|e| {
                        (0..shape.strikes).flat_map(move |s| ['C', 'P'].map(|r| (e, s, r)))
                    })
                    .map(|(e, s, r)| {
                        let k = osi(&underlying, e, s, r);
                        Observation::Series(ListedSeries {
                            meta: meta(&dataset, &format!("series:{k}"), &underlying, &k, now),
                            template_id: "TPL-listed-option".into(),
                            template_version: "1".into(),
                            fields: vec![
                                field("underlying_instrument_id", underlying.clone()),
                                field("strike_price", format!("{}", 10 + s)),
                                field(
                                    "expiration_date",
                                    format!("2027-{:02}-{:02}", 1 + e % 12, 1 + e % 28),
                                ),
                                field("listing_pass", pass.to_string()),
                            ],
                            instrument_id: String::new(),
                        })
                    })
                    .collect();
                for batch in rows.chunks(2000) {
                    store
                        .record(&dataset, batch.to_vec(), now + pass as i64)
                        .unwrap();
                }
            }
        }
        serve(store, &dataset, underlying, Duration::from_millis(1))
    }

    /// The lake served on an in-process bus over the store, its instrument
    /// store stood in, as [`page`] describes.
    fn serve(
        store: Arc<PostgresStore>,
        dataset: &str,
        underlying: String,
        resolve_delay: Duration,
    ) -> Page {
        let hot = std::env::var("MERIDIAN_BENCH_HOT").as_deref() == Ok("1");
        serve_with(store, dataset, underlying, resolve_delay, hot)
    }

    /// [`serve`], its hot layer in front of the record where `hot` says, as
    /// the lake runs it.
    fn serve_with(
        store: Arc<PostgresStore>,
        dataset: &str,
        underlying: String,
        resolve_delay: Duration,
        hot: bool,
    ) -> Page {
        let bus = Arc::new(Bus::single(
            READER,
            Arc::new(MemoryBackend::new()),
            Arc::new(SystemClock),
        ));
        let held = underlying.clone();
        bus.serve(service::RESOLVE_INSTRUMENT, move |_| {
            Ok((
                "meridian.v1.ResolveInstrumentReply".into(),
                ResolveInstrumentReply {
                    found: true,
                    instrument: Some(InstrumentRecord {
                        instrument_id: held.clone(),
                        asset_class: AssetClass::Equity as i32,
                        ..Default::default()
                    }),
                }
                .encode_to_vec(),
            ))
        });
        let resolves = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&resolves);
        bus.serve(service::RESOLVE_IDENTIFIER, move |_| {
            counted.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(resolve_delay);
            Ok((
                "meridian.v1.ResolveIdentifierReply".into(),
                ResolveIdentifierReply::default().encode_to_vec(),
            ))
        });
        bus.serve(service::PLUGIN_CATALOGUE, |_| {
            Ok((
                "meridian.v1.PluginCatalogue".into(),
                meridian_domain::v1::PluginCatalogue::default().encode_to_vec(),
            ))
        });
        // Step 3a: the hot layer in front of the record, where the bench
        // asks for it (MERIDIAN_BENCH_HOT=1), as the lake runs it.
        let hot = hot.then(|| {
            meridian_lake::hot::HotStore::new(store.clone(), meridian_lake::hot::Bounds::DEFAULT)
        });
        let store: Arc<dyn Store> = match &hot {
            Some(hot) => hot.clone(),
            None => store,
        };
        let lake = service::serve(bus.clone(), store.clone(), Arc::new(SystemClock));
        lake.configured(&configuration(dataset));
        Page {
            bus,
            resolves,
            underlying,
            lake,
            store,
            hot,
        }
    }

    /// What one load of the page measured: each read's pages and time.
    #[derive(Default, Debug)]
    pub struct Load {
        pub series: (usize, usize, Duration),
        pub quotes: (usize, usize, Duration),
        pub open_interest: (usize, usize, Duration),
        pub volume: (usize, usize, Duration),
        pub bytes: usize,
        pub total: Duration,
    }

    impl Page {
        async fn ask<Q: Message, R: Message + Default>(
            &self,
            topic: &str,
            kind: &str,
            q: Q,
        ) -> (R, usize) {
            let stamp = Stamp {
                account_scope: Some(vec![]),
                ..Default::default()
            };
            let (_, payload) = self
                .bus
                .call_stamped(
                    topic,
                    kind,
                    q.encode_to_vec(),
                    None,
                    Some(Duration::from_secs(60)),
                    &stamp,
                )
                .await
                .unwrap();
            (R::decode(&payload[..]).unwrap(), payload.len())
        }

        fn subject(&self) -> Vec<SubjectRef> {
            vec![SubjectRef {
                entity_id: self.underlying.clone(),
                ..Default::default()
            }]
        }

        /// The chain page's reads before its first byte, as sample-options
        /// 0.3.2 makes them: every series listed within 400 days (the SDK's
        /// read_chain, its default page), every quote of the chain (pages
        /// of 500), its open interest and its volume (default pages).
        pub async fn load(&self) -> Load {
            let mut load = Load::default();
            let started = Instant::now();
            let (mut cursor, mut pages, mut rows) = (String::new(), 0, 0);
            let at = Instant::now();
            loop {
                let (reply, bytes): (ListSeriesReply, usize) = self
                    .ask(
                        service::LIST_SERIES,
                        "meridian.v1.ListSeriesRequest",
                        ListSeriesRequest {
                            subjects: self.subject(),
                            expiration_from: "2026-01-01".into(),
                            expiration_to: "2028-12-31".into(),
                            cursor: cursor.clone(),
                            ..Default::default()
                        },
                    )
                    .await;
                load.bytes += bytes;
                pages += 1;
                rows += reply.series.len();
                cursor = reply.next_cursor;
                if cursor.is_empty() {
                    break;
                }
            }
            load.series = (pages, rows, at.elapsed());
            let (mut cursor, mut pages, mut rows) = (String::new(), 0, 0);
            let at = Instant::now();
            loop {
                let (reply, bytes): (ListQuotesReply, usize) = self
                    .ask(
                        "platform.lake.query.list-quotes",
                        "meridian.v1.ListQuotesRequest",
                        ListQuotesRequest {
                            subjects: self.subject(),
                            sources: Some(SourceChoice {
                                default: true,
                                ..Default::default()
                            }),
                            all_series: true,
                            page_size: 500,
                            cursor: cursor.clone(),
                            ..Default::default()
                        },
                    )
                    .await;
                load.bytes += bytes;
                pages += 1;
                rows += reply.quotes.len();
                cursor = reply.next_cursor;
                if cursor.is_empty() {
                    break;
                }
            }
            load.quotes = (pages, rows, at.elapsed());
            for kind in [StatisticKind::OpenInterest, StatisticKind::Volume] {
                let (mut cursor, mut pages, mut rows) = (String::new(), 0, 0);
                let at = Instant::now();
                loop {
                    let (reply, bytes): (ListStatisticsReply, usize) = self
                        .ask(
                            service::LIST_STATISTICS,
                            "meridian.v1.ListStatisticsRequest",
                            ListStatisticsRequest {
                                subjects: self.subject(),
                                kinds: vec![kind as i32],
                                all_series: true,
                                cursor: cursor.clone(),
                                ..Default::default()
                            },
                        )
                        .await;
                    load.bytes += bytes;
                    pages += 1;
                    rows += reply.statistics.len();
                    cursor = reply.next_cursor;
                    if cursor.is_empty() {
                        break;
                    }
                }
                let measured = (pages, rows, at.elapsed());
                if kind == StatisticKind::OpenInterest {
                    load.open_interest = measured;
                } else {
                    load.volume = measured;
                }
            }
            load.total = started.elapsed();
            load
        }

        /// The datasets' listing, as a plugin's first request after a
        /// restart reads it (ruling 816: 1,068 ms on dev): its time.
        pub async fn datasets(&self) -> Duration {
            let at = Instant::now();
            let _: (meridian_domain::v1::ListDatasetsReply, usize) = self
                .ask(
                    service::LIST_DATASETS,
                    "meridian.v1.ListDatasetsRequest",
                    meridian_domain::v1::ListDatasetsRequest::default(),
                )
                .await;
            at.elapsed()
        }

        /// An options page's expirations as sample-options 0.3.4 lists
        /// them: every series in the window, `paths=expiration_date`, every
        /// page, each date kept once. Its calls, rows, dates, bytes and time.
        pub async fn expirations_by_series(&self) -> (usize, usize, usize, usize, Duration) {
            let at = Instant::now();
            let (mut cursor, mut calls, mut rows, mut bytes) = (String::new(), 0, 0, 0);
            let mut dates = std::collections::BTreeSet::new();
            loop {
                let (reply, size): (ListSeriesReply, usize) = self
                    .ask(
                        service::LIST_SERIES,
                        "meridian.v1.ListSeriesRequest",
                        ListSeriesRequest {
                            subjects: self.subject(),
                            expiration_from: "2026-01-01".into(),
                            expiration_to: "2028-12-31".into(),
                            paths: vec!["expiration_date".into()],
                            cursor: cursor.clone(),
                            ..Default::default()
                        },
                    )
                    .await;
                calls += 1;
                bytes += size;
                rows += reply.series.len();
                for series in &reply.series {
                    for field in &series.fields {
                        dates.insert(field.value.clone());
                    }
                }
                cursor = reply.next_cursor;
                if cursor.is_empty() {
                    break;
                }
            }
            (calls, rows, dates.len(), bytes, at.elapsed())
        }

        /// The same expirations by ListExpirations (rulings 856, 858 and
        /// 859): one call. Its dates, bytes and time.
        pub async fn expirations(&self) -> (usize, usize, Duration) {
            let at = Instant::now();
            let (reply, bytes): (ListExpirationsReply, usize) = self
                .ask(
                    service::LIST_EXPIRATIONS,
                    "meridian.v1.ListExpirationsRequest",
                    ListExpirationsRequest {
                        underlying_id: self.underlying.clone(),
                        expiration_from: "2026-01-01".into(),
                        expiration_to: "2028-12-31".into(),
                        ..Default::default()
                    },
                )
                .await;
            (reply.expirations.len(), bytes, at.elapsed())
        }

        /// The chain page's reads as a targeted read makes them (rulings
        /// 821 and 833, spec/one-data-access-model step 1): the shown
        /// expiration's series with the drawn columns' paths, and its quotes
        /// with all_series, the window and the fields the page shows. Each
        /// read's calls, rows and time.
        pub async fn targeted(&self, expiration: &str) -> Load {
            let mut load = Load::default();
            let started = Instant::now();
            let (mut cursor, mut pages, mut rows) = (String::new(), 0, 0);
            let at = Instant::now();
            loop {
                let (reply, bytes): (ListSeriesReply, usize) = self
                    .ask(
                        service::LIST_SERIES,
                        "meridian.v1.ListSeriesRequest",
                        ListSeriesRequest {
                            subjects: self.subject(),
                            expiration_from: expiration.into(),
                            expiration_to: expiration.into(),
                            paths: vec![
                                "put_or_call".into(),
                                "strike_price".into(),
                                "expiration_date".into(),
                            ],
                            cursor: cursor.clone(),
                            ..Default::default()
                        },
                    )
                    .await;
                load.bytes += bytes;
                pages += 1;
                rows += reply.series.len();
                cursor = reply.next_cursor;
                if cursor.is_empty() {
                    break;
                }
            }
            load.series = (pages, rows, at.elapsed());
            let (mut cursor, mut pages, mut rows) = (String::new(), 0, 0);
            let at = Instant::now();
            loop {
                let (reply, bytes): (ListQuotesReply, usize) = self
                    .ask(
                        "platform.lake.query.list-quotes",
                        "meridian.v1.ListQuotesRequest",
                        ListQuotesRequest {
                            subjects: self.subject(),
                            sources: Some(SourceChoice {
                                default: true,
                                ..Default::default()
                            }),
                            all_series: true,
                            expiration_from: expiration.into(),
                            expiration_to: expiration.into(),
                            fields: [
                                "meridian.v1.Quote.bid",
                                "meridian.v1.Quote.ask",
                                "meridian.v1.Quote.bid_quantity",
                                "meridian.v1.Quote.ask_quantity",
                            ]
                            .map(String::from)
                            .to_vec(),
                            page_size: 500,
                            cursor: cursor.clone(),
                            ..Default::default()
                        },
                    )
                    .await;
                load.bytes += bytes;
                pages += 1;
                rows += reply.quotes.len();
                cursor = reply.next_cursor;
                if cursor.is_empty() {
                    break;
                }
            }
            load.quotes = (pages, rows, at.elapsed());
            load.total = started.elapsed();
            load
        }
    }

    /// The process's resident memory and its high-water mark, in MiB (Linux).
    pub fn memory() -> (f64, f64) {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let kb = |name: &str| {
            status
                .lines()
                .find(|line| line.starts_with(name))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|n| n.parse::<f64>().ok())
                .unwrap_or(0.0)
                / 1024.0
        };
        (kb("VmRSS:"), kb("VmHWM:"))
    }

    pub fn said(load: &Load) -> String {
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        format!(
            "total_ms={:.0} series={}p/{}r/{:.0}ms quotes={}p/{}r/{:.0}ms oi={}p/{}r/{:.0}ms volume={}p/{}r/{:.0}ms bytes={}",
            ms(load.total),
            load.series.0, load.series.1, ms(load.series.2),
            load.quotes.0, load.quotes.1, ms(load.quotes.2),
            load.open_interest.0, load.open_interest.1, ms(load.open_interest.2),
            load.volume.0, load.volume.1, ms(load.volume.2),
            load.bytes,
        )
    }

    /// `MERIDIAN_BENCH_CHAIN`: expiries,strikes,rounds,days.
    pub fn shape_from_env(default: Shape) -> Shape {
        let said = std::env::var("MERIDIAN_BENCH_CHAIN").unwrap_or_default();
        let n: Vec<usize> = said
            .split(',')
            .filter_map(|n| n.trim().parse().ok())
            .collect();
        match n.as_slice() {
            [e, s, r, d] => Shape {
                expiries: *e,
                strikes: *s,
                rounds: *r,
                days: *d,
            },
            _ => default,
        }
    }

    impl Page {
        /// The lake's steady minute on dev between its bursts (2026-11-09, six
        /// hours up before its second OOMKill): a batch of quotes recorded, each
        /// its own row key, as an FX source records them; the datasets listed
        /// 17 times; four pairs' latest quotes read.
        pub async fn steady_minute(&self, dataset: &str, minute: usize, pairs: usize) {
            let now = SystemClock.now_ns();
            let rows: Vec<Observation> = (0..pairs)
                .flat_map(|p| {
                    (0..40).map(move |j| {
                        let subject = format!("INS-SOAK-PAIR{p}");
                        let at = now + j as i64;
                        let bid = 10_000 + ((minute * 7 + j) % 300) as u64;
                        Observation::Quote(Box::new(Quote {
                            meta: Some(ObservationMeta {
                                row_key: format!("{subject}:{minute}:{j}"),
                                subjects: vec![SubjectRef {
                                    entity_id: subject.clone(),
                                    ..Default::default()
                                }],
                                source: Some(Source {
                                    instance: "truefx".into(),
                                    dataset: dataset.into(),
                                    ..Default::default()
                                }),
                                valid_from_ns: at,
                                ..Default::default()
                            }),
                            bid: usd(bid),
                            ask: usd(bid + 3),
                            ..Default::default()
                        }))
                    })
                })
                .collect();
            let store = Arc::clone(&self.store);
            let dataset = dataset.to_string();
            tokio::task::spawn_blocking(move || store.record(&dataset, rows, now).unwrap())
                .await
                .unwrap();
            for _ in 0..17 {
                self.datasets().await;
            }
            for p in 0..4.min(pairs) {
                let _: (ListQuotesReply, usize) = self
                    .ask(
                        "platform.lake.query.list-quotes",
                        "meridian.v1.ListQuotesRequest",
                        ListQuotesRequest {
                            subjects: vec![SubjectRef {
                                entity_id: format!("INS-SOAK-PAIR{p}"),
                                ..Default::default()
                            }],
                            sources: Some(SourceChoice {
                                default: true,
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    )
                    .await;
            }
        }
    }

    /// Dev's chain when its lake was OOMKilled at 1 GiB, twice on 2026-11-09
    /// (`tradier:options_delayed`, read-only counts that day): one
    /// underlying of about 3,340 series; its listing restated pass after
    /// pass; its quotes a row a series each pass, each its own key; and its
    /// open interest and volume a row a series each business date, each
    /// date's rows restated as many times as `oi_versions` and
    /// `volume_versions` say -- dev held 358,688 statistics, 54,376 keys,
    /// one date's open interest at 99 versions a key.
    #[derive(Clone, Debug)]
    pub struct Relaunch {
        pub expiries: usize,
        pub strikes: usize,
        pub series_versions: usize,
        pub quote_rounds: usize,
        pub oi_versions: Vec<usize>,
        pub volume_versions: Vec<usize>,
    }

    impl Relaunch {
        /// Dev's, as counted on 2026-11-09.
        pub fn dev() -> Self {
            Relaunch {
                expiries: 25,
                strikes: 67,
                series_versions: 5,
                quote_rounds: 90,
                oi_versions: vec![2, 5, 9, 1, 70, 1, 1, 1],
                volume_versions: vec![4, 5, 2, 1, 1, 1, 1, 2],
            }
        }

        pub fn series(&self) -> usize {
            self.expiries * self.strikes * 2
        }

        pub fn days(&self) -> usize {
            self.oi_versions.len().max(self.volume_versions.len())
        }

        /// The statistics recorded, every version.
        pub fn statistics(&self) -> usize {
            self.series()
                * (self.oi_versions.iter().sum::<usize>()
                    + self.volume_versions.iter().sum::<usize>())
        }

        /// The statistics in force: a key each series, kind and date.
        pub fn statistic_keys(&self) -> usize {
            self.series() * (self.oi_versions.len() + self.volume_versions.len())
        }

        pub fn rows(&self) -> usize {
            self.series() * self.series_versions
                + self.quote_rounds * self.series() / 2
                + self.statistics()
        }

        /// `MERIDIAN_BENCH_RELAUNCH`: expiries,strikes,series_versions,
        /// quote_rounds, then the open interest's versions a date and the
        /// volume's, each `/`-separated.
        pub fn from_env(default: Relaunch) -> Relaunch {
            let said = std::env::var("MERIDIAN_BENCH_RELAUNCH").unwrap_or_default();
            let parts: Vec<&str> = said.split(',').map(str::trim).collect();
            let list = |s: &str| -> Vec<usize> {
                s.split('/').filter_map(|n| n.trim().parse().ok()).collect()
            };
            match parts.as_slice() {
                [e, s, v, q, oi, vol] => Relaunch {
                    expiries: e.parse().unwrap_or(default.expiries),
                    strikes: s.parse().unwrap_or(default.strikes),
                    series_versions: v.parse().unwrap_or(default.series_versions),
                    quote_rounds: q.parse().unwrap_or(default.quote_rounds),
                    oi_versions: list(oi),
                    volume_versions: list(vol),
                },
                _ => default,
            }
        }
    }

    /// The first business date [`record_relaunch`] records statistics on.
    pub fn relaunch_first_date(shape: &Relaunch) -> String {
        let now = SystemClock.now_ns();
        meridian_domain::date::Date::of_utc_instant(now - shape.days() as i64 * DAY).to_string()
    }

    /// Record [`Relaunch`] once per database, as [`record_fixture`] does.
    pub fn record_relaunch(
        store: &PostgresStore,
        dataset: &str,
        underlying: &str,
        shape: &Relaunch,
    ) {
        let expected = shape.rows() as u64;
        let head = store.heads().unwrap().get(dataset).copied().unwrap_or(0);
        if head >= expected {
            eprintln!("bench: relaunch fixture held, {head} rows");
            return;
        }
        assert_eq!(
            head, 0,
            "a partial fixture: drop the bench's database and run again"
        );
        let started = Instant::now();
        let now = SystemClock.now_ns();
        let first = now - shape.days() as i64 * DAY;
        let keys: Vec<(usize, char, String)> = (0..shape.expiries)
            .flat_map(|e| {
                (0..shape.strikes).flat_map(move |s| ['C', 'P'].into_iter().map(move |r| (e, s, r)))
            })
            .map(|(e, s, r)| (e, r, osi(underlying, e, s, r)))
            .collect();
        for pass in 0..shape.series_versions {
            let rows: Vec<Observation> = keys
                .iter()
                .enumerate()
                .map(|(i, (e, right, k))| {
                    Observation::Series(ListedSeries {
                        meta: meta(dataset, &format!("series:{k}"), underlying, k, first),
                        template_id: "TPL-listed-option".into(),
                        template_version: "1".into(),
                        fields: vec![
                            field("underlying_instrument_id", underlying.into()),
                            field(
                                "put_or_call",
                                if *right == 'C' { "call" } else { "put" }.into(),
                            ),
                            field("strike_price", format!("{}", 10 + (i / 2) % shape.strikes)),
                            field(
                                "expiration_date",
                                format!("2027-{:02}-{:02}", 1 + e % 12, 1 + e % 28),
                            ),
                            field("exercise_style", "american".into()),
                            field("settlement_method", "physical".into()),
                            field("contract_multiplier", "100".into()),
                            field("listing_pass", pass.to_string()),
                        ],
                        instrument_id: String::new(),
                    })
                })
                .collect();
            for batch in rows.chunks(5000) {
                store.record(dataset, batch.to_vec(), now).unwrap();
            }
        }
        let spacing = (shape.days() as i64 * DAY / shape.quote_rounds.max(1) as i64).max(MINUTE);
        let mut batch = Vec::new();
        for round in 0..shape.quote_rounds {
            let at = first + round as i64 * spacing;
            for (i, (_, _, k)) in keys.iter().enumerate() {
                if (i + round) % 2 != 0 {
                    continue;
                }
                let bid = 100 + ((i * 7 + round * 3) % 500) as u64;
                batch.push(Observation::Quote(Box::new(Quote {
                    meta: meta(dataset, &format!("{k}:q:{at}"), underlying, k, at),
                    bid: usd(bid),
                    ask: usd(bid + 5),
                    bid_quantity: decimal(10 + (round % 90) as u64, 0),
                    ask_quantity: decimal(12, 0),
                    ..Default::default()
                })));
            }
            if batch.len() >= 5000 {
                store
                    .record(dataset, std::mem::take(&mut batch), now)
                    .unwrap();
            }
        }
        if !batch.is_empty() {
            store.record(dataset, batch, now).unwrap();
        }
        for (kind, name, versions) in [
            (StatisticKind::OpenInterest, "OI", &shape.oi_versions),
            (StatisticKind::Volume, "VOL", &shape.volume_versions),
        ] {
            for (day, times) in versions.iter().enumerate() {
                let date = meridian_domain::date::Date::of_utc_instant(first + day as i64 * DAY)
                    .to_string();
                for pass in 0..*times {
                    let rows: Vec<Observation> = keys
                        .iter()
                        .enumerate()
                        .map(|(i, (_, _, k))| {
                            let mut m = meta(
                                dataset,
                                &format!("{k}:{date}:{name}"),
                                underlying,
                                k,
                                first + day as i64 * DAY,
                            );
                            m.as_mut().unwrap().business_date = date.clone();
                            Observation::Statistic(Statistic {
                                meta: m,
                                kind: kind as i32,
                                value: Some(UnitValue {
                                    value: decimal((100 + i * 3 + day * 7 + pass) as u64, 0),
                                    unit: ValueUnit::Count as i32,
                                    ..Default::default()
                                }),
                            })
                        })
                        .collect();
                    for chunk in rows.chunks(5000) {
                        store.record(dataset, chunk.to_vec(), now).unwrap();
                    }
                }
            }
        }
        eprintln!(
            "bench: relaunch fixture recorded, {} rows in {:.1} s",
            store.heads().unwrap().get(dataset).copied().unwrap_or(0),
            started.elapsed().as_secs_f64()
        );
    }

    /// [`Relaunch`] recorded and served, as [`page`] serves its fixture.
    pub fn relaunch_page(store: Arc<PostgresStore>, tag: &str, shape: &Relaunch) -> Page {
        let dataset = format!("tradier:{tag}");
        let underlying = format!("INS-{tag}").to_uppercase();
        record_relaunch(&store, &dataset, &underlying, shape);
        serve(store, &dataset, underlying, Duration::from_millis(1))
    }

    /// [`relaunch_page`], the hot layer in front of the record as the lake
    /// runs it.
    pub fn relaunch_page_hot(store: Arc<PostgresStore>, tag: &str, shape: &Relaunch) -> Page {
        let dataset = format!("tradier:{tag}");
        let underlying = format!("INS-{tag}").to_uppercase();
        record_relaunch(&store, &dataset, &underlying, shape);
        serve_with(store, &dataset, underlying, Duration::from_millis(1), true)
    }

    /// The process's resident high-water mark started again from what it
    /// holds now (Linux: `/proc/self/clear_refs`); false where it cannot be.
    pub fn restart_high_water() -> bool {
        std::fs::write("/proc/self/clear_refs", "5").is_ok()
    }

    /// Dev's relaunch burst, every read at once (2026-11-09: 21 list-datasets,
    /// 14 list-quotes, 10 list-statistics, 10 list-series and 8 list-prices in
    /// two minutes): two readers of the chain's statistics over its dates,
    /// two of its latest quotes and two of its listing, eight of prices and
    /// 21 of the datasets. Each read's slowest, by kind.
    pub async fn burst(
        page: &Arc<Page>,
        from: &str,
    ) -> std::collections::BTreeMap<&'static str, Paged> {
        let mut handles = Vec::new();
        for _ in 0..2 {
            let (p, f) = (Arc::clone(page), from.to_string());
            handles.push(tokio::spawn(async move {
                ("statistics", p.statistics_ranged(&f, usize::MAX).await)
            }));
            let p = Arc::clone(page);
            handles.push(tokio::spawn(
                async move { ("quotes", p.quotes_latest().await) },
            ));
            let p = Arc::clone(page);
            handles.push(tokio::spawn(
                async move { ("series", p.series_window().await) },
            ));
        }
        for _ in 0..8 {
            let p = Arc::clone(page);
            handles.push(tokio::spawn(async move {
                let took = p.prices_latest().await;
                (
                    "prices",
                    Paged {
                        pages: 1,
                        rows: 0,
                        took,
                    },
                )
            }));
        }
        for _ in 0..21 {
            let p = Arc::clone(page);
            handles.push(tokio::spawn(async move {
                let took = p.datasets().await;
                (
                    "datasets",
                    Paged {
                        pages: 1,
                        rows: 0,
                        took,
                    },
                )
            }));
        }
        let mut slowest: std::collections::BTreeMap<&'static str, Paged> = Default::default();
        for handle in handles {
            let (what, p) = handle.await.unwrap();
            let held = slowest.entry(what).or_default();
            if p.took >= held.took {
                *held = p;
            }
        }
        slowest
    }

    /// One paged read's pages, rows and time.
    #[derive(Debug, Default, Clone, Copy)]
    pub struct Paged {
        pub pages: usize,
        pub rows: usize,
        pub took: Duration,
    }

    impl Page {
        /// A chain's statistics over a range of business dates, every
        /// series in the window, a page of 500 at a time: dev's
        /// list-statistics, 500 rows of 49,412 a page, 1.6 to 4.0 s each.
        pub async fn statistics_ranged(&self, from_date: &str, most_pages: usize) -> Paged {
            let at = Instant::now();
            let mut out = Paged::default();
            let mut cursor = String::new();
            loop {
                let (reply, _): (ListStatisticsReply, usize) = self
                    .ask(
                        service::LIST_STATISTICS,
                        "meridian.v1.ListStatisticsRequest",
                        ListStatisticsRequest {
                            subjects: self.subject(),
                            all_series: true,
                            from_date: from_date.into(),
                            expiration_from: "2026-01-01".into(),
                            expiration_to: "2028-12-31".into(),
                            page_size: 500,
                            cursor: cursor.clone(),
                            ..Default::default()
                        },
                    )
                    .await;
                out.pages += 1;
                out.rows += reply.statistics.len();
                cursor = reply.next_cursor;
                if cursor.is_empty() || out.pages >= most_pages {
                    break;
                }
            }
            out.took = at.elapsed();
            out
        }

        /// The chain's latest quotes, a page of 500 at a time: dev's
        /// list-quotes, 500 of about 3,300 a page.
        pub async fn quotes_latest(&self) -> Paged {
            let at = Instant::now();
            let mut out = Paged::default();
            let mut cursor = String::new();
            loop {
                let (reply, _): (ListQuotesReply, usize) = self
                    .ask(
                        "platform.lake.query.list-quotes",
                        "meridian.v1.ListQuotesRequest",
                        ListQuotesRequest {
                            subjects: self.subject(),
                            sources: Some(SourceChoice {
                                default: true,
                                ..Default::default()
                            }),
                            all_series: true,
                            page_size: 500,
                            cursor: cursor.clone(),
                            ..Default::default()
                        },
                    )
                    .await;
                out.pages += 1;
                out.rows += reply.quotes.len();
                cursor = reply.next_cursor;
                if cursor.is_empty() {
                    break;
                }
            }
            out.took = at.elapsed();
            out
        }

        /// The chain's listing in a window, a page of 500 at a time: dev's
        /// list-series, 500 of about 2,000 a page.
        pub async fn series_window(&self) -> Paged {
            let at = Instant::now();
            let mut out = Paged::default();
            let mut cursor = String::new();
            loop {
                let (reply, _): (ListSeriesReply, usize) = self
                    .ask(
                        service::LIST_SERIES,
                        "meridian.v1.ListSeriesRequest",
                        ListSeriesRequest {
                            subjects: self.subject(),
                            expiration_from: "2027-01-01".into(),
                            expiration_to: "2027-06-30".into(),
                            page_size: 500,
                            cursor: cursor.clone(),
                            ..Default::default()
                        },
                    )
                    .await;
                out.pages += 1;
                out.rows += reply.series.len();
                cursor = reply.next_cursor;
                if cursor.is_empty() {
                    break;
                }
            }
            out.took = at.elapsed();
            out
        }

        /// The underlying's latest prices: one page.
        pub async fn prices_latest(&self) -> Duration {
            let at = Instant::now();
            let _: (meridian_domain::v1::ListPricesReply, usize) = self
                .ask(
                    "platform.lake.query.list-prices",
                    "meridian.v1.ListPricesRequest",
                    meridian_domain::v1::ListPricesRequest {
                        subjects: self.subject(),
                        ..Default::default()
                    },
                )
                .await;
            at.elapsed()
        }
    }
}

/// The chain page's reads on a TQQQ-shaped chain (rulings 798, 808): `make
/// bench` runs it. Sequential loads, then eight at once, with the time of
/// each read and the process's memory across them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "the capacity bench: make bench"]
async fn capacity_chain_page_reads() {
    use chain_page::*;
    let shape = shape_from_env(Shape {
        expiries: 12,
        strikes: 60,
        rounds: 600,
        days: 3,
    });
    let store = Arc::new(tokio::task::block_in_place(store));
    let page = tokio::task::block_in_place(|| {
        chain_page::page(
            Arc::clone(&store),
            &format!(
                "bench-{}x{}x{}x{}",
                shape.expiries, shape.strikes, shape.rounds, shape.days
            ),
            shape,
            std::time::Duration::from_millis(1),
        )
    });
    let (rss, _) = memory();
    let (live, _) = counted::take();
    println!(
        "CAPACITY chain shape={shape:?} series={} heap_live_mib={live:.1} rss_mib={rss:.0}",
        shape.series()
    );
    for i in 0..5 {
        let before = page.resolves.load(std::sync::atomic::Ordering::Relaxed);
        let load = page.load().await;
        let resolves = page.resolves.load(std::sync::atomic::Ordering::Relaxed) - before;
        let (rss, hwm) = memory();
        let (live, peak) = counted::take();
        println!("CAPACITY chain load={i} {} resolves={resolves} heap_live_mib={live:.1} heap_peak_mib={peak:.1} rss_mib={rss:.0} hwm_mib={hwm:.0}", said(&load));
    }
    // The datasets' listing (ruling 816), and the targeted reads of one
    // expiration (ruling 821): the first expiration the fixture lists.
    for i in 0..3 {
        let took = page.datasets().await;
        println!(
            "CAPACITY chain list_datasets={i} ms={:.1}",
            took.as_secs_f64() * 1e3
        );
    }
    // An options page's expirations (rulings 856 and 858): every series
    // in the window, as sample-options 0.3.4 lists them, against one
    // ListExpirations.
    for i in 0..5 {
        let (calls, rows, dates, bytes, took) = page.expirations_by_series().await;
        println!(
            "CAPACITY chain expirations_by_series={i} calls={calls} rows={rows} dates={dates} bytes={bytes} ms={:.1}",
            took.as_secs_f64() * 1e3
        );
    }
    for i in 0..5 {
        let (dates, bytes, took) = page.expirations().await;
        println!(
            "CAPACITY chain expirations={i} calls=1 dates={dates} bytes={bytes} ms={:.1}",
            took.as_secs_f64() * 1e3
        );
    }
    // The same listing restated: each series recorded ten times, as a data
    // plugin re-listing its chain leaves it (dev, 2026-10-30).
    let versions = tokio::task::block_in_place(|| {
        chain_page::versions_page(
            Arc::clone(&store),
            &format!("bench-versions-{}x{}x10", shape.expiries, shape.strikes),
            shape,
            10,
        )
    });
    for i in 0..5 {
        let (dates, bytes, took) = versions.expirations().await;
        println!(
            "CAPACITY chain expirations_versions=10 run={i} calls=1 dates={dates} bytes={bytes} ms={:.1}",
            took.as_secs_f64() * 1e3
        );
    }
    for i in 0..5 {
        let before = page.resolves.load(std::sync::atomic::Ordering::Relaxed);
        let load = page.targeted("2027-01-01").await;
        let resolves = page.resolves.load(std::sync::atomic::Ordering::Relaxed) - before;
        println!(
            "CAPACITY chain targeted={i} calls={} total_ms={:.0} series={}p/{}r/{:.0}ms quotes={}p/{}r/{:.0}ms bytes={} resolves={resolves}",
            load.series.0 + load.quotes.0,
            load.total.as_secs_f64() * 1e3,
            load.series.0,
            load.series.1,
            load.series.2.as_secs_f64() * 1e3,
            load.quotes.0,
            load.quotes.1,
            load.quotes.2.as_secs_f64() * 1e3,
            load.bytes,
        );
    }
    let page = Arc::new(page);
    for round in 0..3 {
        let started = std::time::Instant::now();
        let loads = futures_lite_join(8, Arc::clone(&page)).await;
        let slowest = loads.iter().map(|l| l.total).max().unwrap();
        let (rss, hwm) = memory();
        let (live, peak) = counted::take();
        println!(
            "CAPACITY chain concurrent=8 round={round} wall_ms={:.0} slowest_ms={:.0} heap_live_mib={live:.1} heap_peak_mib={peak:.1} rss_mib={rss:.0} hwm_mib={hwm:.0}",
            started.elapsed().as_secs_f64() * 1e3,
            slowest.as_secs_f64() * 1e3
        );
    }
}

/// Dev's relaunch burst against dev's chain (2026-11-09: the lake
/// OOMKilled at 1 GiB, at 15:37:40 and 21:38:15, each during one): each
/// read alone, then the burst's reads at once -- a chain's statistics over
/// its dates, its latest quotes twice, its listing twice, prices eight
/// times and the datasets 21 -- with the heap's peak and the process's
/// resident high-water mark across each. `make bench` runs it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "the capacity bench: make bench"]
async fn capacity_relaunch_read_mix() {
    use chain_page::*;
    let shape = Relaunch::from_env(Relaunch::dev());
    let store = Arc::new(tokio::task::block_in_place(store));
    let tag = format!(
        "relaunch-{}x{}x{}x{}-{}-{}",
        shape.expiries,
        shape.strikes,
        shape.series_versions,
        shape.quote_rounds,
        shape
            .oi_versions
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join("."),
        shape
            .volume_versions
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join("."),
    );
    let page = tokio::task::block_in_place(|| relaunch_page(Arc::clone(&store), &tag, &shape));
    let from = relaunch_first_date(&shape);
    let most_pages = std::env::var("MERIDIAN_BENCH_PAGES")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(usize::MAX);
    let (rss, hwm) = memory();
    let (live, _) = counted::take();
    println!(
        "CAPACITY relaunch shape={shape:?} rows={} statistics={} keys={} observation_bytes={} heap_live_mib={live:.1} rss_mib={rss:.0} hwm_mib={hwm:.0}",
        shape.rows(),
        shape.statistics(),
        shape.statistic_keys(),
        std::mem::size_of::<meridian_lake::row::Observation>(),
    );
    let said = |what: &str, p: Paged| {
        let (rss, hwm) = memory();
        let (live, peak) = counted::take();
        println!(
            "CAPACITY relaunch alone={what} pages={} rows={} ms={:.0} heap_live_mib={live:.1} heap_peak_mib={peak:.1} rss_mib={rss:.0} hwm_mib={hwm:.0}",
            p.pages,
            p.rows,
            p.took.as_secs_f64() * 1e3
        );
    };
    let quotes = page.quotes_latest().await;
    said("quotes", quotes);
    let series = page.series_window().await;
    said("series", series);
    let at = std::time::Instant::now();
    page.prices_latest().await;
    page.datasets().await;
    said(
        "prices+datasets",
        Paged {
            pages: 2,
            took: at.elapsed(),
            ..Default::default()
        },
    );
    let one = page.statistics_ranged(&from, 1).await;
    said("statistics-first-page", one);
    let statistics = page.statistics_ranged(&from, most_pages).await;
    said("statistics", statistics);

    // The burst: every read at once, as the relaunched plugins made them.
    let page = Arc::new(page);
    let started = std::time::Instant::now();
    let slowest = burst(&page, &from).await;
    let (rss, hwm) = memory();
    let (live, peak) = counted::take();
    println!(
        "CAPACITY relaunch burst wall_ms={:.0} heap_live_mib={live:.1} heap_peak_mib={peak:.1} rss_mib={rss:.0} hwm_mib={hwm:.0} slowest={}",
        started.elapsed().as_secs_f64() * 1e3,
        slowest
            .iter()
            .map(|(what, p)| format!("{what}:{}p/{}r/{:.0}ms", p.pages, p.rows, p.took.as_secs_f64() * 1e3))
            .collect::<Vec<_>>()
            .join(",")
    );
}

/// The lake's steady hours between bursts (dev 2026-11-09: OOMKilled again
/// at 21:38:15 after six hours up): `MERIDIAN_BENCH_MINUTES` minutes of
/// dev's steady traffic run back to back -- quotes recorded, the datasets
/// listed, pairs' latest read -- and what the lake holds every hour of
/// them: its heap, its resident memory, its held answers and its hot
/// layer. `make bench` runs it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "the capacity bench: make bench"]
async fn capacity_lake_steady_hours() {
    use chain_page::*;
    let minutes: usize = std::env::var("MERIDIAN_BENCH_MINUTES")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(360);
    let store = Arc::new(tokio::task::block_in_place(store));
    let tag = unique("soak");
    let dataset = format!("tradier:{tag}");
    let page = tokio::task::block_in_place(|| {
        relaunch_page(
            Arc::clone(&store),
            &tag,
            &Relaunch {
                expiries: 1,
                strikes: 1,
                series_versions: 1,
                quote_rounds: 1,
                oi_versions: vec![1],
                volume_versions: vec![1],
            },
        )
    });
    let said = |minute: usize, page: &Page| {
        let (rss, hwm) = memory();
        let (live, _) = counted::take();
        let hot = page
            .hot
            .as_ref()
            .map(|hot| format!("{:?}", hot.held()))
            .unwrap_or_default();
        println!(
            "CAPACITY steady minute={minute} heap_live_mib={live:.1} rss_mib={rss:.0} hwm_mib={hwm:.0} held_pages={} held_bytes={} hot={hot}",
            page.lake.held_pages(),
            page.lake.held_bytes(),
        );
    };
    said(0, &page);
    for minute in 1..=minutes {
        page.steady_minute(&dataset, minute, 7).await;
        if minute % 60 == 0 {
            let lake = Arc::clone(&page.lake);
            tokio::task::spawn_blocking(move || lake.apply_retention())
                .await
                .unwrap();
            said(minute, &page);
        }
    }
}

/// Found on dev 2026-11-09: the lake OOMKilled at 1 GiB twice, each time
/// during a relaunch's burst of reads, by a chain's statistics over its
/// dates read a page at a time -- each page read every version of every key
/// and decoded it to choose the 500 it answered (358,688 versions for 49,412
/// rows; core's bench, dev's shape: 536 MiB of heap a page, 1,089 MiB with
/// the burst's two readers, resident 1.9 GiB). A paged read's working memory
/// is now its page: the rows in force chosen by the database, read a chunk
/// at a time, each held while choosing as a few dozen bytes, and each page's
/// rows read by their places alone.
///
/// The burst against a chain whose 4,800 statistics in force are 66,600
/// versions (one date's open interest at 100 a key, as dev's at 99), the
/// hot layer in front as the lake runs it, under the counting allocator,
/// each test its own process (the gate's nextest): every read answers every
/// row; the heap's peak across the burst stays within
/// [`BURST_HEAP_MOST_MIB`], and resident memory's high-water mark within
/// [`BURST_RESIDENT_MOST_MIB`] of where it started; and the hot layer
/// within its bounds. Measured (core's bench, 2026-11-09): main 7e0950f's
/// heap peaked at 194 MiB and its resident memory rose 207 MiB; this
/// change's, 11 to 19 MiB and 14 to 46 MiB.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paged_read_holds_its_page_never_its_set() {
    use chain_page::*;
    let shape = Relaunch {
        expiries: 12,
        strikes: 25,
        series_versions: 2,
        quote_rounds: 4,
        oi_versions: vec![3, 100, 2, 1],
        volume_versions: vec![2, 1, 1, 1],
    };
    let store = Arc::new(tokio::task::block_in_place(store));
    let tag = unique("paged");
    let page = tokio::task::block_in_place(|| relaunch_page_hot(Arc::clone(&store), &tag, &shape));
    let from = relaunch_first_date(&shape);
    // What the set would take decoded whole, every version: what main
    // decoded for each page.
    let sample = tokio::task::block_in_place(|| {
        store.read(&Query {
            datasets: vec![format!("tradier:{tag}")],
            subjects: vec![page.underlying.clone()],
            data_type: DataType::Statistic,
            kinds: vec![],
            interval_ns: 0,
            when: When::Latest { at_ns: 0 },
            as_of_ns: 0,
            published_as_of_ns: 0,
            latest_only: true,
        })
    })
    .unwrap();
    let per_row = sample
        .into_iter()
        .next()
        .map(|row| {
            meridian_lake::pages::Whole {
                rows: vec![row],
                not_served: Vec::new(),
            }
            .held_bytes()
        })
        .expect("a statistic recorded");
    let whole_mib = (shape.statistics() * per_row) as f64 / 1048576.0;

    let (rss_before, _) = memory();
    let high_water = restart_high_water();
    counted::take();
    let page = Arc::new(page);
    let slowest = burst(&page, &from).await;
    let (_, peak) = counted::take();
    let (_, hwm) = memory();
    println!(
        "BURST heap_peak_mib={peak:.1} rss_before_mib={rss_before:.0} hwm_mib={hwm:.0} whole_set_mib={whole_mib:.0} slowest={slowest:?}"
    );
    assert!(
        whole_mib >= 2.0 * BURST_HEAP_MOST_MIB,
        "the set decoded whole, {whole_mib:.0} MiB, is what the bound tells apart"
    );
    assert_eq!(
        slowest["statistics"].rows,
        shape.statistic_keys(),
        "every statistic in force, once"
    );
    assert_eq!(
        slowest["quotes"].rows,
        shape.series(),
        "every series' latest quote"
    );
    assert!(slowest["series"].rows > 0);
    assert!(
        peak <= BURST_HEAP_MOST_MIB,
        "the burst's heap peaked at {peak:.1} MiB; a paged read holds its page, and the bound is {BURST_HEAP_MOST_MIB} MiB"
    );
    if high_water && hwm > 0.0 {
        assert!(
            hwm - rss_before <= BURST_RESIDENT_MOST_MIB,
            "resident memory rose {:.0} MiB across the burst; the bound is {BURST_RESIDENT_MOST_MIB} MiB",
            hwm - rss_before
        );
    }
    // The hot layer's bounds still hold (ruling 808; step 3a).
    let hot = page.hot.as_ref().expect("the hot layer in front");
    let ((_, latest), (_, listings)) = hot.held();
    let bounds = meridian_lake::hot::Bounds::DEFAULT;
    assert!(latest <= bounds.latest_bytes && listings <= bounds.listings_bytes);
}

/// The most the burst's heap may peak at, in MiB: two statistics readers
/// building at once (`lake.maxConcurrentAnswers`), each holding a chunk of
/// rows read, its candidates and at most `read::HELD_INLINE_MOST` of rows,
/// beside the other reads' pages.
const BURST_HEAP_MOST_MIB: f64 = 40.0;

/// The most resident memory may rise across the burst, in MiB.
const BURST_RESIDENT_MOST_MIB: f64 = 96.0;

async fn futures_lite_join(n: usize, page: Arc<chain_page::Page>) -> Vec<chain_page::Load> {
    let handles: Vec<_> = (0..n)
        .map(|_| {
            let page = Arc::clone(&page);
            tokio::spawn(async move { page.load().await })
        })
        .collect();
    let mut out = Vec::new();
    for handle in handles {
        out.push(handle.await.unwrap());
    }
    out
}

/// Ruling 957: a held answer is counted at what it holds -- every row's
/// place in the vector and each row's own strings and lists as the
/// allocator places them -- never at less, so the held answers' budget is
/// what they take; and at no more than half again, so an answer is not
/// turned away for what it does not hold. The rows as the store reads them
/// back, cloned into an answer as the read copies them.
#[test]
fn a_held_answer_is_counted_at_what_it_holds() {
    use meridian_lake::pages::Whole;
    let store = store();
    let tag = unique("held");
    let dataset = format!("tradier:{tag}");
    let underlying = format!("INS-{tag}").to_uppercase();
    let shape = chain_page::Shape {
        expiries: 2,
        strikes: 10,
        rounds: 4,
        days: 2,
    };
    chain_page::record_fixture(&store, &dataset, &underlying, shape);
    for data_type in [DataType::Series, DataType::Quote, DataType::Statistic] {
        let rows = store
            .read(&Query {
                datasets: vec![dataset.clone()],
                subjects: vec![underlying.clone()],
                data_type,
                kinds: vec![],
                interval_ns: 0,
                when: When::Range {
                    from_ns: 0,
                    until_ns: i64::MAX,
                },
                as_of_ns: SystemClock.now_ns(),
                published_as_of_ns: 0,
                latest_only: false,
            })
            .unwrap();
        assert!(!rows.is_empty(), "{data_type:?}");
        let before = counted::placed_here();
        let whole = Whole {
            rows: rows.to_vec(),
            not_served: Vec::new(),
        };
        let holds = (counted::placed_here() - before) as usize;
        let counted = whole.held_bytes();
        println!(
            "HELD {data_type:?}: {} rows hold {holds} bytes, {} a row; counted {counted}, {} a row",
            whole.rows.len(),
            holds / whole.rows.len(),
            counted / whole.rows.len()
        );
        assert!(
            counted >= holds,
            "{data_type:?}: counted {counted} of {holds}"
        );
        assert!(
            counted <= holds * 3 / 2,
            "{data_type:?}: counted {counted}, more than half again {holds}"
        );
    }
}

// ── The change log (step 3a) ──

/// A store over a schema of its own, so a takeover raises no other test's
/// epoch: the lake's tables made there, as `migrate` makes them anywhere.
/// The schema is dropped with the last clone of its URL.
fn isolated(tag: &str) -> (Scratch, PostgresStore) {
    let schema = unique(tag).replace(['-', ':'], "_").to_lowercase();
    let url = own_schema(&schema);
    let store = PostgresStore::connect(&url, 2).expect("could not reach the test database");
    store
        .migrate(&SystemClock)
        .expect("could not create the schema");
    (url, store)
}

fn every_change(
    store: &PostgresStore,
    dataset: &str,
    page: usize,
) -> Vec<meridian_changes::Change> {
    let mut out = Vec::new();
    meridian_changes::catch_up(
        |after| store.read_changes(dataset, after, page),
        0,
        |change| out.push(change.clone()),
    )
    .expect("the log reads");
    out
}

#[test]
fn a_follower_given_the_log_in_any_page_split_holds_the_writers_rows() {
    let store = store();
    let dataset = unique("coinbase-1:daily");
    let subject = unique("LCL-BTC");
    for day in 1..=30 {
        let date = format!("2026-09-{day:02}");
        store
            .record(
                &dataset,
                vec![
                    close(&dataset, &format!("a-{day}"), &subject, &date, day),
                    close(&dataset, &format!("b-{day}"), &subject, &date, day * 2),
                ],
                1_000 + day,
            )
            .unwrap();
    }
    // Restatements: a new version of keys held.
    store
        .record(
            &dataset,
            (1..=10)
                .map(|day| {
                    close(
                        &dataset,
                        &format!("a-{day}"),
                        &subject,
                        "2026-09-01",
                        day + 500,
                    )
                })
                .collect(),
            2_000,
        )
        .unwrap();
    let kept = store
        .read_dataset_versions(&dataset, DataType::Price)
        .unwrap();
    assert_eq!(kept.len(), 70);
    for page in [1usize, 200, 2_000, meridian_changes::PAGE_MOST] {
        let changes = every_change(&store, &dataset, page);
        let sequences: Vec<u64> = changes.iter().map(|c| c.sequence).collect();
        assert_eq!(
            sequences,
            (1..=70).collect::<Vec<_>>(),
            "no holes, page {page}"
        );
        let rows: Vec<Observation> = changes
            .iter()
            .map(|c| Observation::decode(DataType::Price, &c.body).unwrap())
            .collect();
        assert_eq!(rows, kept, "page {page}");
        assert!(changes
            .iter()
            .all(|c| c.epoch >= 1 && c.kind == "meridian.v1.Price"));
    }
    let reply = store
        .read_changes(&dataset, 70, meridian_changes::PAGE_MOST)
        .unwrap();
    assert_eq!((reply.head, reply.lowest_kept), (70, 1));
}

#[test]
fn a_deposed_writers_batch_is_refused_after_a_takeover_and_leaves_no_hole() {
    let (url, writer) = isolated("lake-fence");
    let dataset = unique("coinbase-1:daily");
    let subject = unique("LCL-BTC");
    assert_eq!(writer.adopt().unwrap(), 1);
    writer
        .record(
            &dataset,
            vec![close(&dataset, "a", &subject, "2026-10-01", 1)],
            10,
        )
        .unwrap();
    let standby = PostgresStore::connect(&url, 1).unwrap();
    assert_eq!(
        standby
            .take_over(&meridian_changes::TookOver::unnamed("a test"))
            .unwrap(),
        2
    );
    // The takeover is its own record, in its own transaction with the
    // epoch it raised (step 3b, `WriterTookOver`).
    let took = {
        let url = url.clone();
        let read = move || {
            let mut held = postgres::Client::connect(&url, postgres::NoTls).unwrap();
            let row = held.query_one("SELECT why, coalesce(array_length(partitions, 1), 0) FROM lake_writer_took_over WHERE epoch = 2", &[]).unwrap();
            (row.get::<_, String>(0), row.get::<_, i32>(1))
        };
        read()
    };
    assert_eq!(took.0, "a test");
    assert!(
        took.1 >= 2,
        "the partitions it took, control among them: {took:?}"
    );
    // The deposed writer's next batch, to a partition held and to a new one.
    let refused = writer
        .record(
            &dataset,
            vec![close(&dataset, "b", &subject, "2026-10-02", 2)],
            20,
        )
        .unwrap_err();
    assert!(
        refused.to_string().starts_with(meridian_changes::FENCED),
        "{refused}"
    );
    let fresh = unique("coinbase-1:other");
    assert!(writer
        .record(
            &fresh,
            vec![close(&fresh, "a", &subject, "2026-10-02", 2)],
            20
        )
        .is_err());
    assert!(writer
        .remove_before(&dataset, i64::MAX, "retention", 30)
        .is_err());
    // The writer that took over numbers on from the head: the refused batch
    // took nothing.
    standby
        .record(
            &dataset,
            vec![close(&dataset, "c", &subject, "2026-10-03", 3)],
            30,
        )
        .unwrap();
    let changes = every_change(&standby, &dataset, meridian_changes::PAGE_MOST);
    let numbered: Vec<(u64, u64)> = changes.iter().map(|c| (c.sequence, c.epoch)).collect();
    assert_eq!(numbered, vec![(1, 1), (2, 2)]);
    assert_eq!(
        standby
            .read_changes(&dataset, 0, meridian_changes::PAGE_MOST)
            .unwrap()
            .epoch,
        2
    );
    assert_eq!(
        standby
            .read_changes("control", 0, meridian_changes::PAGE_MOST)
            .unwrap()
            .epoch,
        2
    );
}

#[test]
fn a_follower_behind_retention_is_refused_naming_after_sequence_and_reloads() {
    let store = store();
    let dataset = unique("coinbase-1:daily");
    let subject = unique("LCL-BTC");
    for day in 1..=4 {
        store
            .record(
                &dataset,
                vec![close(
                    &dataset,
                    &format!("k-{day}"),
                    &subject,
                    "2026-10-01",
                    day,
                )],
                day * 100,
            )
            .unwrap();
    }
    store
        .remove_before(&dataset, 300, "retention", 500)
        .unwrap();
    let refused = store
        .read_changes(&dataset, 0, meridian_changes::PAGE_MOST)
        .unwrap_err();
    assert!(
        refused.to_string().contains("after_sequence 0"),
        "{refused}"
    );
    assert!(meridian_changes::is_behind(&refused.to_string()));
    // The state at the head, then the log from there, is the writer's.
    let reply = store
        .read_changes(&dataset, 2, meridian_changes::PAGE_MOST)
        .unwrap();
    assert_eq!(reply.lowest_kept, 3);
    assert_eq!(reply.changes.len(), 2);
    let state = store
        .read_dataset_versions(&dataset, DataType::Price)
        .unwrap();
    let followed: Vec<Observation> = reply
        .changes
        .iter()
        .map(|c| Observation::decode(DataType::Price, &c.body).unwrap())
        .collect();
    assert_eq!(state, followed);
}

/// Step 3b (ruling 1062's carried item): an assessment, a closed-day
/// amendment, a priority and an alias each a change of the `control`
/// partition, in the order committed, read back a page at a time; an alias
/// heard again is not logged again; and a deposed writer is refused them.
#[test]
fn the_control_partition_logs_what_is_not_a_row_and_is_fenced() {
    use meridian_lake::store::{
        Assessment, ClosedDayAmendment, CONTROL_ALIAS, CONTROL_ASSESSMENT, CONTROL_CLOSED_DAY,
        CONTROL_PRIORITY,
    };
    let (url, writer) = isolated("lake-control");
    assert_eq!(writer.adopt().unwrap(), 1);
    writer
        .assess(&[Assessment {
            dataset: "d-1".into(),
            sequence: 1,
            row_key: "k".into(),
            version: 1,
            state: 3,
            reasons: vec![1],
            by_instance: "signal-1".into(),
            at_ns: 1,
        }])
        .unwrap();
    writer
        .amend_closed_day(&[ClosedDayAmendment {
            dataset: "d-1".into(),
            sequence: 1,
            row_key: String::new(),
            closed_day: None,
            why: "calendar 2 applied".into(),
            at_ns: 2,
        }])
        .unwrap();
    let priority = SourcePriority {
        data_type: "quote".into(),
        updated_at_ns: 3,
        ..Default::default()
    };
    writer.set_priority(&priority, 0).unwrap().unwrap();
    writer.keep_alias("INS-OLD", "INS-NEW", 4).unwrap();
    writer.keep_alias("INS-OLD", "INS-NEW", 5).unwrap();
    let control = writer
        .read_changes(meridian_changes::CONTROL, 0, meridian_changes::PAGE_MOST)
        .unwrap();
    let kinds: Vec<&str> = control.changes.iter().map(|c| c.kind.as_str()).collect();
    assert_eq!(
        kinds,
        vec![
            CONTROL_ASSESSMENT,
            CONTROL_CLOSED_DAY,
            CONTROL_PRIORITY,
            CONTROL_ALIAS
        ]
    );
    assert_eq!((control.head, control.lowest_kept), (4, 1));
    let paged = writer
        .read_changes(meridian_changes::CONTROL, 1, 1)
        .unwrap();
    assert_eq!(
        paged.changes.len(),
        1,
        "a page of one byte holds one change"
    );
    assert_eq!(paged.changes[0].sequence, 2);
    let standby = PostgresStore::connect(&url, 1).unwrap();
    standby
        .take_over(&meridian_changes::TookOver::unnamed("a test"))
        .unwrap();
    let refused = writer.keep_alias("INS-A", "INS-B", 6).unwrap_err();
    assert!(
        refused.to_string().starts_with(meridian_changes::FENCED),
        "{refused}"
    );
    standby.keep_alias("INS-A", "INS-B", 6).unwrap();
    let after = standby
        .read_changes(meridian_changes::CONTROL, 4, meridian_changes::PAGE_MOST)
        .unwrap();
    // The takeover is a change in the control partition itself, under the
    // epoch it raised, before the new writer's first (contract v30, stage
    // D): the record the conductor reads.
    let kinds: Vec<(&str, u64, u64)> = after
        .changes
        .iter()
        .map(|c| (c.kind.as_str(), c.sequence, c.epoch))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (meridian_changes::records::TOOK_OVER, 5, 2),
            (CONTROL_ALIAS, 6, 2)
        ]
    );
    let took = meridian_changes::records::of_change(&after.changes[0]);
    let [meridian_changes::records::Record::TookOver(took)] = took.as_slice() else {
        panic!("the takeover's record: {took:?}")
    };
    assert_eq!(
        (took.component.as_str(), took.epoch, took.partitions.last()),
        ("lake", 2, Some(&"control".to_string()))
    );
    assert_eq!(
        meridian_changes::integrity::digest(&after.changes[0].body),
        after.changes[0].digest,
        "carried with its digest"
    );
}

// ── Integrity at rest (step 3b) ─────────────────────────────────────────────

#[test]
fn each_row_keeps_its_digest_and_the_scrub_names_one_changed_beneath_the_lake() {
    let store = store();
    let dataset = unique("integrity-1:daily");
    let subject = unique("LCL-ETH");
    store
        .record(
            &dataset,
            vec![
                close(&dataset, "a", &subject, "2026-10-07", 10),
                close(&dataset, "b", &subject, "2026-10-08", 11),
                close(&dataset, "c", &subject, "2026-10-09", 12),
            ],
            1_000,
        )
        .unwrap();
    // Each row's change carries its digest, which verifies.
    let mut changes = Vec::new();
    meridian_changes::catch_up(
        |after| store.read_changes(&dataset, after, 0),
        0,
        |change| changes.push(change.clone()),
    )
    .expect("the log reads");
    assert_eq!(changes.len(), 3);
    for change in &changes {
        assert!(!change.digest.is_empty());
        meridian_changes::integrity::verify(change).expect("verifies");
    }
    let url = std::env::var("MERIDIAN_TEST_DATABASE_URL").unwrap();
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    // A row kept before digests is given its own; a row changed beneath the
    // lake is named by its dataset's first bad sequence.
    client
        .execute(
            "UPDATE lake_row SET digest = NULL WHERE dataset = $1 AND sequence = 3",
            &[&dataset],
        )
        .unwrap();
    client
        .execute(
            "UPDATE lake_row SET body = body || '\\x00'::bytea WHERE dataset = $1 AND sequence = 2",
            &[&dataset],
        )
        .unwrap();
    let scrubbed = store.scrub().expect("scrubs");
    assert!(scrubbed.filled >= 1);
    let ours: Vec<_> = scrubbed
        .findings
        .iter()
        .filter(|finding| finding.partition == dataset)
        .collect();
    assert_eq!(ours.len(), 1, "{:?}", scrubbed.findings);
    assert_eq!(ours[0].sequence, 2);
    let filled: bool = client
        .query_one(
            "SELECT digest = sha256(body) FROM lake_row WHERE dataset = $1 AND sequence = 3",
            &[&dataset],
        )
        .unwrap()
        .get(0);
    assert!(filled, "the row kept before digests now has its own");
    // Each finding kept is also the lake's `IntegrityFinding` in its control
    // partition, once however often it is kept (contract v30, stage D).
    store.record_scrub(&scrubbed, "test", 1, 2).expect("kept");
    store
        .record_scrub(&scrubbed, "test", 3, 4)
        .expect("kept again, once");
    let mut found = Vec::new();
    meridian_changes::catch_up(
        |after| store.read_changes(meridian_changes::CONTROL, after, 0),
        0,
        |change| {
            for record in meridian_changes::records::of_change(change) {
                if let meridian_changes::records::Record::Finding(f) = record {
                    if f.partition == dataset {
                        found.push(f);
                    }
                }
            }
        },
    )
    .expect("the control partition reads");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        (
            found[0].component.as_str(),
            found[0].first_bad_sequence,
            found[0].check
        ),
        (
            "lake",
            2,
            meridian_domain::v1::IntegrityCheck::RowDigest as i32
        )
    );
}

/// The lake's whole-dataset reads, each bounded by a chunk (found on dev
/// 2026-11-09, core's store memory audit, items 1 and 5): a calendar's
/// re-check read a dataset following it whole, every version; a read as
/// published by a time read every version in its window to choose each
/// row key's in force. Each test here measures the read under the counting
/// allocator, each its own process (the gate's nextest), against what the
/// set would take decoded whole, and checks the answer row by row.
mod bounded {
    use std::sync::Arc;

    use meridian_bus::{Bus, MemoryBackend, Stamp};
    use meridian_clock::{Clock, SystemClock};
    use meridian_domain::v1::{
        AssetClass, CalendarAppliedEvent, CalendarCoverage, CalendarDay, CalendarDayKind,
        CalendarWeekend, DatasetEntitlement, DatasetRef, EntitlementsChangedEvent,
        InstrumentRecord, ObservationMeta, Rate, RateKind, ReadCalendarReply,
        ResolveIdentifierReply, ResolveInstrumentReply, Source, SourceChoice, SourceTime,
        SourceTimeKind, Statistic, StatisticKind, SubjectRef, Tenor, TenorUnit, UnitValue,
        ValueUnit, Weekday,
    };
    use meridian_lake::row::Observation;
    use meridian_lake::service;
    use meridian_lake::store::Store;
    use meridian_lake::PostgresStore;
    use meridian_pb::v1::{DatasetDeclaration, Decimal, ObservationMode};
    use prost::Message;

    pub const READER: &str = "sample-bounded";
    pub const DAY: i64 = 86_400 * 1_000_000_000;
    pub const HOUR: i64 = 3_600 * 1_000_000_000;

    /// A size from the environment, for measuring at another scale.
    pub fn sized(name: &str, default: usize) -> usize {
        std::env::var(name)
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(default)
    }

    pub fn decimal(units: u64, scale: u32) -> Option<Decimal> {
        Some(Decimal {
            high: 0,
            low: units,
            scale,
        })
    }

    /// The lake served on an in-process bus over the store for one dataset
    /// of these data types, following `calendar` where one is named; the
    /// instrument store stood in, and the calendar's every read answered
    /// `reply`.
    pub fn serve(
        store: Arc<PostgresStore>,
        dataset: &str,
        data_types: &[&str],
        calendar: Option<ReadCalendarReply>,
    ) -> (Arc<Bus>, Arc<service::Lake>) {
        let bus = Arc::new(Bus::single(
            READER,
            Arc::new(MemoryBackend::new()),
            Arc::new(SystemClock),
        ));
        bus.serve(service::RESOLVE_INSTRUMENT, |envelope| {
            let asked =
                meridian_domain::v1::ResolveInstrumentRequest::decode(&envelope.payload[..])
                    .unwrap_or_default();
            Ok((
                "meridian.v1.ResolveInstrumentReply".into(),
                ResolveInstrumentReply {
                    found: true,
                    instrument: Some(InstrumentRecord {
                        instrument_id: asked.instrument_id,
                        asset_class: AssetClass::Equity as i32,
                        ..Default::default()
                    }),
                }
                .encode_to_vec(),
            ))
        });
        bus.serve(service::RESOLVE_IDENTIFIER, |_| {
            Ok((
                "meridian.v1.ResolveIdentifierReply".into(),
                ResolveIdentifierReply::default().encode_to_vec(),
            ))
        });
        bus.serve(service::PLUGIN_CATALOGUE, |_| {
            Ok((
                "meridian.v1.PluginCatalogue".into(),
                meridian_domain::v1::PluginCatalogue::default().encode_to_vec(),
            ))
        });
        let calendar_id = calendar
            .as_ref()
            .map(|reply| reply.calendar_id.clone())
            .unwrap_or_default();
        if let Some(reply) = calendar {
            bus.serve(service::READ_CALENDAR, move |_| {
                Ok((
                    "meridian.v1.ReadCalendarReply".into(),
                    reply.encode_to_vec(),
                ))
            });
        }
        let store: Arc<dyn Store> = store;
        let lake = service::serve(bus.clone(), store, Arc::new(SystemClock));
        let (instance, key) = dataset.split_once(':').unwrap();
        lake.configured(&EntitlementsChangedEvent {
            datasets: vec![DatasetRef {
                dataset: dataset.into(),
                instance: instance.into(),
                vendor: "Bounded".into(),
                declaration: Some(DatasetDeclaration {
                    key: key.into(),
                    vendor: "Bounded".into(),
                    data_types: data_types.iter().map(|t| t.to_string()).collect(),
                    modes: vec![ObservationMode::Pull as i32],
                    cadence: 86_400,
                    calendar_id,
                    // Kept: no licence says otherwise (contract v30 retired
                    // a catalogue's licence_default, ruling 1032).
                    ..Default::default()
                }),
                ..Default::default()
            }],
            licences: vec![],
            entitlements: vec![DatasetEntitlement {
                dataset: dataset.into(),
                instance: READER.into(),
                allowed: true,
                ..Default::default()
            }],
            changed_at_ns: SystemClock.now_ns(),
        });
        (bus, lake)
    }

    /// Asked as the reader's sidecar asks.
    pub async fn ask<Q: Message, R: Message + Default>(
        bus: &Bus,
        topic: &str,
        kind: &str,
        q: Q,
    ) -> R {
        let stamp = Stamp {
            account_scope: Some(vec![]),
            ..Default::default()
        };
        let (_, payload) = bus
            .call_stamped(
                topic,
                kind,
                q.encode_to_vec(),
                None,
                Some(std::time::Duration::from_secs(120)),
                &stamp,
            )
            .await
            .unwrap_or_else(|failed| panic!("{topic}: {failed}"));
        R::decode(&payload[..]).unwrap()
    }

    /// A calendar closing the weekends and every `holiday_every`-th date
    /// from `first`, covering `first` to `last`.
    pub fn calendar(
        calendar_id: &str,
        first: &str,
        last: &str,
        holidays: &[String],
    ) -> ReadCalendarReply {
        ReadCalendarReply {
            found: true,
            calendar_id: calendar_id.into(),
            version: 2,
            days: holidays
                .iter()
                .map(|date| CalendarDay {
                    date: date.clone(),
                    kind: CalendarDayKind::Closed as i32,
                    name: "A closure".into(),
                    source: String::new(),
                })
                .collect(),
            weekends: vec![CalendarWeekend {
                weekdays: vec![Weekday::Saturday as i32, Weekday::Sunday as i32],
                valid_from_date: "2000-01-01".into(),
            }],
            coverage: Some(CalendarCoverage {
                from_date: first.into(),
                to_date: last.into(),
            }),
            sources: vec![],
            trading_sessions: None,
        }
    }

    pub fn statistic(
        dataset: &str,
        key: &str,
        subject: &str,
        date: &str,
        at: i64,
        value: u64,
    ) -> Observation {
        Observation::Statistic(Statistic {
            meta: Some(ObservationMeta {
                row_key: format!("{key}:{date}"),
                subjects: vec![SubjectRef {
                    entity_id: subject.into(),
                    ..Default::default()
                }],
                source: Some(Source {
                    instance: "bounded".into(),
                    dataset: dataset.into(),
                    ..Default::default()
                }),
                valid_from_ns: at,
                business_date: date.into(),
                ..Default::default()
            }),
            kind: StatisticKind::OpenInterest as i32,
            value: Some(UnitValue {
                value: decimal(value, 0),
                unit: ValueUnit::Count as i32,
                ..Default::default()
            }),
        })
    }

    /// A par yield of `subject` at a tenor on a date, `value` hundredths
    /// of a percent, published at `published_ns`.
    pub fn par(
        dataset: &str,
        subject: &str,
        months: i32,
        date: &str,
        at: i64,
        value: u64,
        published_ns: i64,
    ) -> Observation {
        Observation::Rate(Box::new(Rate {
            meta: Some(ObservationMeta {
                row_key: format!("{date}:{months}M"),
                subjects: vec![SubjectRef {
                    entity_id: subject.into(),
                    ..Default::default()
                }],
                source: Some(Source {
                    instance: "us-treasury".into(),
                    dataset: dataset.into(),
                    ..Default::default()
                }),
                valid_from_ns: at,
                business_date: date.into(),
                source_times: vec![SourceTime {
                    kind: SourceTimeKind::Published as i32,
                    at_ns: published_ns,
                }],
                ..Default::default()
            }),
            kind: RateKind::ParYield as i32,
            value: Some(UnitValue {
                value: decimal(value, 2),
                unit: ValueUnit::Percent as i32,
                ..Default::default()
            }),
            tenor: Some(Tenor {
                count: months,
                unit: TenorUnit::Month as i32,
            }),
            ..Default::default()
        }))
    }

    /// What `rows` take held decoded, as a held answer counts a row.
    pub fn whole_mib(rows: &[Observation], of: usize) -> f64 {
        let per_row = rows
            .iter()
            .map(|row| {
                meridian_lake::pages::Whole {
                    rows: vec![row.clone()],
                    not_served: Vec::new(),
                }
                .held_bytes()
            })
            .sum::<usize>()
            / rows.len().max(1);
        (of * per_row) as f64 / 1048576.0
    }

    pub fn applied(calendar_id: &str) -> CalendarAppliedEvent {
        CalendarAppliedEvent {
            calendar_id: calendar_id.into(),
            version: 2,
            rule: None,
            applied_at_ns: 0,
        }
    }

    pub fn now() -> i64 {
        SystemClock.now_ns()
    }

    pub fn default_sources() -> Option<SourceChoice> {
        Some(SourceChoice {
            default: true,
            ..Default::default()
        })
    }
}

/// Found on dev 2026-11-09 (core's store memory audit, item 1): a
/// calendar's new version re-checked every dataset following it by reading
/// the dataset's dated rows whole, every version and data type, decoded in
/// one query -- a chain's dataset following its venue's calendar kept about
/// 565,000 such rows, about 216 MB of bodies -- and held every amendment it
/// made until the last. Now a chunk of [`meridian_lake::store::READ_CHUNK`]
/// rows at a time, each chunk's re-checks journalled before the next.
///
/// A dataset of `MERIDIAN_BOUNDED_KEYS` keys over `MERIDIAN_BOUNDED_DAYS`
/// dates, each restated `MERIDIAN_BOUNDED_VERSIONS` times, recorded
/// unflagged; a calendar's version closing the weekends and every fifth
/// date: the heap's peak across the re-check stays within
/// [`RECHECK_HEAP_MOST_MIB`] and resident memory's high-water mark within
/// [`RECHECK_RESIDENT_MOST_MIB`] of where it started; every row on a closed
/// date, every version, is amended once, with the day's reason, and no
/// other; the same version again amends nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_calendars_recheck_holds_a_chunk_never_the_dataset() {
    use bounded::*;
    use chain_page::{memory, restart_high_water};
    let keys = sized("MERIDIAN_BOUNDED_KEYS", 400);
    let days = sized("MERIDIAN_BOUNDED_DAYS", 25);
    let versions = sized("MERIDIAN_BOUNDED_VERSIONS", 5);
    let store = Arc::new(tokio::task::block_in_place(store));
    let tag = unique("recheck");
    let dataset = format!("bounded:{tag}");
    let subject = format!("INS-{tag}").to_uppercase();
    let calendar_id = format!("CAL-{tag}").to_uppercase();
    let first = now() - (days as i64 + 1) * DAY;
    let dates: Vec<String> = (0..days)
        .map(|day| Date::of_utc_instant(first + day as i64 * DAY).to_string())
        .collect();
    tokio::task::block_in_place(|| {
        for version in 0..versions {
            let rows: Vec<Observation> = dates
                .iter()
                .enumerate()
                .flat_map(|(day, date)| {
                    let (dataset, subject) = (&dataset, &subject);
                    (0..keys).map(move |k| {
                        statistic(
                            dataset,
                            &format!("K{k}"),
                            subject,
                            date,
                            first + day as i64 * DAY,
                            (k + day + version) as u64,
                        )
                    })
                })
                .collect();
            for chunk in rows.chunks(5000) {
                store.record(&dataset, chunk.to_vec(), now()).unwrap();
            }
        }
    });
    let holidays: Vec<String> = dates.iter().step_by(5).cloned().collect();
    let reply = calendar(&calendar_id, &dates[0], &dates[days - 1], &holidays);
    let closed: std::collections::BTreeMap<String, Option<meridian_domain::v1::DateCheck>> = dates
        .iter()
        .map(|date| {
            (
                date.clone(),
                meridian_domain::lake::closed_day(&reply, Date::parse(date).unwrap()),
            )
        })
        .collect();
    let closed_dates = closed.values().filter(|check| check.is_some()).count();
    assert!(
        closed_dates > 0 && closed_dates < days,
        "some days closed, some open"
    );
    let (_, lake) = tokio::task::block_in_place(|| {
        serve(
            Arc::clone(&store),
            &dataset,
            &["meridian.v1.Statistic"],
            Some(reply),
        )
    });
    // What the dataset takes decoded whole: what a re-check held before.
    let sample = tokio::task::block_in_place(|| {
        store.read(&Query {
            datasets: vec![dataset.clone()],
            subjects: vec![subject.clone()],
            data_type: DataType::Statistic,
            kinds: vec![],
            interval_ns: 0,
            when: When::BusinessDate(Date::parse(&dates[0]).unwrap()),
            as_of_ns: 0,
            published_as_of_ns: 0,
            latest_only: false,
        })
    })
    .unwrap();
    let rows = keys * days * versions;
    let whole = whole_mib(&sample, rows);

    let (rss_before, _) = memory();
    let high_water = restart_high_water();
    counted::take();
    tokio::task::block_in_place(|| lake.recheck(&applied(&calendar_id)));
    let (_, peak) = counted::take();
    let (_, hwm) = memory();
    println!(
        "RECHECK rows={rows} heap_peak_mib={peak:.1} rss_before_mib={rss_before:.0} hwm_mib={hwm:.0} whole_set_mib={whole:.0}"
    );

    // Every row on a closed date, every version, amended once with its
    // day's check; none on an open date.
    let amendments: Vec<_> = tokio::task::block_in_place(|| store.closed_day_amendments())
        .unwrap()
        .into_iter()
        .filter(|a| a.dataset == dataset)
        .collect();
    assert_eq!(amendments.len(), closed_dates * keys * versions);
    let read = tokio::task::block_in_place(|| {
        store.read(&Query {
            datasets: vec![dataset.clone()],
            subjects: vec![subject.clone()],
            data_type: DataType::Statistic,
            kinds: vec![],
            interval_ns: 0,
            when: When::Range {
                from_ns: 0,
                until_ns: 0,
            },
            as_of_ns: 0,
            published_as_of_ns: 0,
            latest_only: false,
        })
    })
    .unwrap();
    assert_eq!(read.len(), keys * days, "each key on each date, in force");
    for row in &read {
        let expected = &closed[&row.meta().business_date];
        assert_eq!(
            row.meta()
                .closed_day
                .as_ref()
                .map(|c| (c.reason, c.holiday_name.clone())),
            expected
                .as_ref()
                .map(|c| (c.reason, c.holiday_name.clone())),
            "{}",
            row.meta().business_date
        );
    }
    // The same version again: nothing differs, nothing amended.
    tokio::task::block_in_place(|| lake.recheck(&applied(&calendar_id)));
    let again = tokio::task::block_in_place(|| store.closed_day_amendments())
        .unwrap()
        .into_iter()
        .filter(|a| a.dataset == dataset)
        .count();
    assert_eq!(again, amendments.len());

    assert!(
        whole >= 2.0 * RECHECK_HEAP_MOST_MIB,
        "the dataset decoded whole, {whole:.0} MiB, is what the bound tells apart"
    );
    assert!(
        peak <= RECHECK_HEAP_MOST_MIB,
        "the re-check's heap peaked at {peak:.1} MiB; it holds a chunk, and the bound is {RECHECK_HEAP_MOST_MIB} MiB"
    );
    if high_water && hwm > 0.0 {
        assert!(
            hwm - rss_before <= RECHECK_RESIDENT_MOST_MIB,
            "resident memory rose {:.0} MiB across the re-check; the bound is {RECHECK_RESIDENT_MOST_MIB} MiB",
            hwm - rss_before
        );
    }
}

/// The most a calendar's re-check's heap may peak at, in MiB: a chunk of
/// rows decoded and its amendments.
const RECHECK_HEAP_MOST_MIB: f64 = 8.0;

/// The most resident memory may rise across a re-check, in MiB.
const RECHECK_RESIDENT_MOST_MIB: f64 = 48.0;

/// Found on dev 2026-11-09 (core's store memory audit, item 5): a read as
/// published by a time collected every version in its window, decoded, to
/// choose each row key's version in force by then; Treasury's par curve
/// keeps 100,048 rows for its one subject, about 100 to 200 MB a reader.
/// Now a rate's rows in force are chosen by the database
/// (`lake_row.published_at_ns`) and streamed as every other read is.
///
/// A par curve of `MERIDIAN_BOUNDED_TENORS` tenors over
/// `MERIDIAN_BOUNDED_RATE_DAYS` dates, each restated
/// `MERIDIAN_BOUNDED_VERSIONS` times -- version v published v hours after
/// the first, the last a late-arriving older publication -- read as
/// published by a time between the versions, a page of 500 at a time: the
/// heap's peak across the read stays within [`PUBLISHED_HEAP_MOST_MIB`] and
/// resident memory within [`PUBLISHED_RESIDENT_MOST_MIB`]; every row key is
/// answered once, at the version in force by then -- the latest published
/// by the cut, never the late older one, never one published after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_as_published_holds_its_page_never_every_version() {
    use bounded::*;
    use chain_page::{memory, restart_high_water};
    use meridian_domain::v1::{ListRatesReply, ListRatesRequest};
    let tenors = sized("MERIDIAN_BOUNDED_TENORS", 12);
    let days = sized("MERIDIAN_BOUNDED_RATE_DAYS", 600);
    let versions = sized("MERIDIAN_BOUNDED_VERSIONS", 5).max(3);
    let store = Arc::new(tokio::task::block_in_place(store));
    let tag = unique("published");
    let dataset = format!("us-treasury:{tag}");
    let subject = format!("INS-{tag}").to_uppercase();
    let first = now() - (days as i64 + 1) * DAY;
    let base = first - DAY;
    // Version v (1-based) published v hours after `base`; the last
    // version a late-arriving older publication, half an hour after it.
    let published = |version: usize| {
        if version == versions {
            base + HOUR / 2
        } else {
            base + version as i64 * HOUR
        }
    };
    let value =
        |tenor: usize, day: usize, version: usize| (100 + tenor * 7 + day + version * 1000) as u64;
    tokio::task::block_in_place(|| {
        for version in 1..=versions {
            let rows: Vec<Observation> = (0..days)
                .flat_map(|day| {
                    let date = Date::of_utc_instant(first + day as i64 * DAY).to_string();
                    let (dataset, subject) = (&dataset, &subject);
                    (0..tenors).map(move |tenor| {
                        par(
                            dataset,
                            subject,
                            1 + tenor as i32,
                            &date,
                            first + day as i64 * DAY,
                            value(tenor, day, version),
                            published(version),
                        )
                    })
                })
                .collect();
            for chunk in rows.chunks(5000) {
                store.record(&dataset, chunk.to_vec(), now()).unwrap();
            }
        }
    });
    // Published by the cut: versions 1 to versions-2, and the late older
    // last; in force, versions-2, the latest by published time.
    let cut = published(versions - 2) + HOUR / 4;
    let in_force = versions - 2;
    let (bus, _lake) = tokio::task::block_in_place(|| {
        serve(Arc::clone(&store), &dataset, &["meridian.v1.Rate"], None)
    });
    let rows = tenors * days * versions;
    let sample = vec![par(&dataset, &subject, 1, "2026-01-01", first, 1, 1)];
    let whole = whole_mib(&sample, rows);

    let (rss_before, _) = memory();
    let high_water = restart_high_water();
    counted::take();
    let mut answered = Vec::new();
    let mut cursor = String::new();
    let mut pages = 0;
    loop {
        let reply: ListRatesReply = ask(
            &bus,
            meridian_lake::service::LIST_RATES,
            "meridian.v1.ListRatesRequest",
            ListRatesRequest {
                subjects: vec![meridian_domain::v1::SubjectRef {
                    entity_id: subject.clone(),
                    ..Default::default()
                }],
                kinds: vec![meridian_domain::v1::RateKind::ParYield as i32],
                sources: default_sources(),
                valid_from_ns: first,
                valid_until_ns: first + days as i64 * DAY,
                published_as_of_ns: cut,
                page_size: 500,
                cursor: cursor.clone(),
                ..Default::default()
            },
        )
        .await;
        pages += 1;
        answered.extend(reply.rates);
        cursor = reply.next_cursor;
        if cursor.is_empty() {
            break;
        }
    }
    let (_, peak) = counted::take();
    let (_, hwm) = memory();
    println!(
        "PUBLISHED rows={rows} pages={pages} answered={} heap_peak_mib={peak:.1} rss_before_mib={rss_before:.0} hwm_mib={hwm:.0} whole_set_mib={whole:.0}",
        answered.len()
    );

    assert_eq!(answered.len(), tenors * days, "every row key once");
    let mut seen = std::collections::BTreeSet::new();
    for rate in &answered {
        let meta = rate.meta.as_ref().unwrap();
        assert!(seen.insert(meta.row_key.clone()), "{} twice", meta.row_key);
        assert_eq!(meta.version as usize, in_force, "{}", meta.row_key);
        let tenor = rate.tenor.as_ref().unwrap().count as usize - 1;
        let day = ((meta.valid_from_ns - first) / DAY) as usize;
        assert_eq!(
            rate.value.as_ref().unwrap().value.as_ref().unwrap().low,
            value(tenor, day, in_force),
            "{}",
            meta.row_key
        );
    }

    assert!(
        whole >= 2.0 * PUBLISHED_HEAP_MOST_MIB,
        "the versions decoded whole, {whole:.0} MiB, are what the bound tells apart"
    );
    assert!(
        peak <= PUBLISHED_HEAP_MOST_MIB,
        "the read's heap peaked at {peak:.1} MiB; it holds its page, and the bound is {PUBLISHED_HEAP_MOST_MIB} MiB"
    );
    if high_water && hwm > 0.0 {
        assert!(
            hwm - rss_before <= PUBLISHED_RESIDENT_MOST_MIB,
            "resident memory rose {:.0} MiB across the read; the bound is {PUBLISHED_RESIDENT_MOST_MIB} MiB",
            hwm - rss_before
        );
    }
}

/// The most a read as published by a time may peak at, in MiB: a chunk of
/// rows read, its candidates and at most `read::HELD_INLINE_MOST` of rows.
const PUBLISHED_HEAP_MOST_MIB: f64 = 16.0;

/// The most resident memory may rise across it, in MiB.
const PUBLISHED_RESIDENT_MOST_MIB: f64 = 64.0;

/// Item 5's second half (core's store memory audit, 2026-11-09): the
/// holdings reports a read as published by a time chose from were every
/// version of every report in the dataset (`read_dataset_versions`), held
/// decoded to choose each report's in force. Now the store reads each
/// report's versions together, a chunk at a time, and keeps one row a
/// report as it reads.
///
/// `MERIDIAN_BOUNDED_REPORTS` reports each restated
/// `MERIDIAN_BOUNDED_REPORT_VERSIONS` times, version v published v hours in, the
/// last a late-arriving older publication: read as published between the
/// versions, the answer is the one every version chosen whole gave, row
/// for row, and the heap's peak is at most half what choosing from every
/// version took, measured beside it.
#[test]
fn a_dataset_read_as_published_keeps_a_row_a_key_never_every_version() {
    use bounded::*;
    use meridian_domain::v1::{HoldingsReport, HoldingsReportKind};
    let reports = sized("MERIDIAN_BOUNDED_REPORTS", 4000);
    let versions = sized("MERIDIAN_BOUNDED_REPORT_VERSIONS", 8).max(3);
    let store = store();
    let dataset = format!("sec:{}", unique("reports"));
    let base = now() - 30 * DAY;
    let published = |version: usize| {
        if version == versions {
            base + HOUR / 2
        } else {
            base + version as i64 * HOUR
        }
    };
    for version in 1..=versions {
        let rows: Vec<Observation> = (0..reports)
            .map(|r| {
                Observation::HoldingsReport(HoldingsReport {
                    meta: Some(ObservationMeta {
                        row_key: format!("0001067983-26-{r:06}"),
                        subjects: vec![SubjectRef {
                            entity_id: format!("ENT-HOLDER{}", r % 50),
                            ..Default::default()
                        }],
                        source: Some(Source {
                            instance: "sec".into(),
                            dataset: dataset.clone(),
                            ..Default::default()
                        }),
                        valid_from_ns: base,
                        source_times: vec![meridian_domain::v1::SourceTime {
                            kind: meridian_domain::v1::SourceTimeKind::Published as i32,
                            at_ns: published(version),
                        }],
                        ..Default::default()
                    }),
                    kind: HoldingsReportKind::Form13f as i32,
                    report_date: format!("2026-09-{:02}", 1 + (r + version) % 28),
                    complete: true,
                    filing: None,
                })
            })
            .collect();
        for chunk in rows.chunks(5000) {
            store.record(&dataset, chunk.to_vec(), now()).unwrap();
        }
    }
    let cut = published(versions - 2) + HOUR / 4;

    counted::take();
    let every = store
        .read_dataset_versions(&dataset, DataType::HoldingsReport)
        .unwrap();
    let before =
        meridian_lake::read::as_published(meridian_lake::store::recorded_by(every, 0), cut);
    let (_, whole_peak) = counted::take();
    let after = store
        .read_dataset(&dataset, DataType::HoldingsReport, 0, cut)
        .unwrap();
    let (_, peak) = counted::take();
    println!(
        "REPORTS rows={} in_force={} every_version_peak_mib={whole_peak:.1} heap_peak_mib={peak:.1}",
        reports * versions,
        after.len()
    );
    assert_eq!(after.len(), reports);
    assert!(after
        .iter()
        .all(|row| row.meta().version as usize == versions - 2));
    assert_eq!(after, before, "the answer, row for row");
    assert!(
        peak * 2.0 <= whole_peak,
        "the read's heap peaked at {peak:.1} MiB, choosing from every version {whole_peak:.1} MiB"
    );
}

/// Contract v30, stage E (the resilience spec's section 4): writes from the
/// write-ahead stream recorded as one -- each dataset's parts numbered in
/// turn, as one batch of it -- with the stream's position in the same
/// commit; a batch that does not follow the position held is refused and
/// records nothing, so a writer behind a failover goes back and takes the
/// writes again, each recorded once.
#[test]
fn writes_from_the_stream_commit_with_its_position_or_not_at_all() {
    use meridian_lake::store::{StoreError, WriteAheadBatch, WriteAheadPosition};
    let store = store();
    let stream = unique("STREAM");
    let a = unique("coinbase-1:a");
    let b = unique("coinbase-1:b");
    let subject = unique("LCL-BTC");
    assert_eq!(store.write_ahead_position(&stream).unwrap(), None);
    let at = |sequence: u64| WriteAheadPosition {
        sequence,
        at_ns: 1_000 + sequence as i64,
    };
    let done = store
        .record_write_ahead(
            WriteAheadBatch {
                stream: stream.clone(),
                after: None,
                through: at(3),
                parts: vec![
                    (a.clone(), vec![close(&a, "x", &subject, "2026-10-07", 10)]),
                    (b.clone(), vec![close(&b, "y", &subject, "2026-10-07", 20)]),
                    // The same key again, a later part of the same dataset:
                    // its next version, numbered after the first part.
                    (a.clone(), vec![close(&a, "x", &subject, "2026-10-07", 11)]),
                ],
                ..Default::default()
            },
            5_000,
        )
        .unwrap()
        .into_iter()
        .map(Result::unwrap)
        .collect::<Vec<_>>();
    assert_eq!(
        done.iter()
            .map(|part| (part.recorded, part.restated, part.unchanged, part.head))
            .collect::<Vec<_>>(),
        vec![(1, 0, 0, 1), (1, 0, 0, 1), (0, 1, 0, 2)]
    );
    let second = done[2].rows[0].meta();
    assert_eq!(
        (second.sequence, second.version, second.previous_sequence),
        (2, 2, 1)
    );
    assert_eq!(store.write_ahead_position(&stream).unwrap(), Some(at(3)));

    // A writer that believes the stream is elsewhere is refused, and
    // nothing of its batch is recorded.
    let refused = store.record_write_ahead(
        WriteAheadBatch {
            stream: stream.clone(),
            after: Some(at(2)),
            through: at(4),
            parts: vec![(a.clone(), vec![close(&a, "z", &subject, "2026-10-07", 30)])],
            ..Default::default()
        },
        6_000,
    );
    assert!(
        matches!(refused, Err(StoreError::PositionMoved(_))),
        "{refused:?}"
    );
    assert_eq!(store.heads().unwrap()[&a], 2);
    assert_eq!(store.write_ahead_position(&stream).unwrap(), Some(at(3)));

    // From the position held, a write taken again changes nothing.
    let again = store
        .record_write_ahead(
            WriteAheadBatch {
                stream: stream.clone(),
                after: Some(at(3)),
                through: at(4),
                parts: vec![(a.clone(), vec![close(&a, "x", &subject, "2026-10-07", 11)])],
                ..Default::default()
            },
            7_000,
        )
        .unwrap()
        .into_iter()
        .map(Result::unwrap)
        .collect::<Vec<_>>();
    assert_eq!(
        (again[0].recorded, again[0].restated, again[0].unchanged),
        (0, 0, 1)
    );
    assert_eq!(store.heads().unwrap()[&a], 2);
    assert_eq!(store.write_ahead_position(&stream).unwrap(), Some(at(4)));
}

/// Contract v30, stage E (W10.14): what a batch from the write-ahead stream
/// found -- the runs the stream dropped, each dataset's latest valid time,
/// each gap -- commits with it, and each gap's next state is its own
/// record, read back in order.
#[test]
fn a_gap_found_by_a_batch_commits_with_it_and_each_state_is_its_own_record() {
    use meridian_domain::v1::{DatasetGap, GapCause, GapState};
    use meridian_lake::store::{Dropped, WriteAheadBatch, WriteAheadPosition};
    let store = store();
    let stream = unique("STREAM");
    let dataset = unique("coinbase-1:gaps");
    let subject = unique("LCL-BTC");
    let gap = DatasetGap {
        gap_id: unique("GAP-TEST"),
        dataset: dataset.clone(),
        subjects: vec![subject.clone()],
        valid_from_ns: 100,
        valid_until_ns: 200,
        cause: GapCause::WriteAheadBound as i32,
        state: GapState::Refetching as i32,
        recorded_at_ns: 5_000,
        found_in_gap_id: String::new(),
    };
    // The run keeps the unattributed gap recorded for it (contract v30,
    // stage C2), so a writer that starts names it too.
    let dropped = Dropped {
        first: 2,
        last: 4,
        after_at_ns: 1_001,
        until_at_ns: 1_005,
        gap_id: unique("GAP-UNATTRIBUTED"),
    };
    store
        .record_write_ahead(
            WriteAheadBatch {
                stream: stream.clone(),
                after: None,
                through: WriteAheadPosition {
                    sequence: 5,
                    at_ns: 1_005,
                },
                parts: vec![(
                    dataset.clone(),
                    vec![close(&dataset, "x", &subject, "2026-10-07", 10)],
                )],
                dropped: vec![dropped.clone()],
                valid: vec![(dataset.clone(), 100)],
                gaps: vec![gap.clone()],
                ..Default::default()
            },
            5_000,
        )
        .unwrap();
    let (found, valid) = store.write_ahead_found(&stream).unwrap();
    assert_eq!(found, vec![dropped]);
    assert_eq!(valid.get(&dataset), Some(&100));
    let recovered = DatasetGap {
        state: GapState::Recovered as i32,
        recorded_at_ns: 6_000,
        ..gap.clone()
    };
    store.record_gap(&recovered).unwrap();
    let mine: Vec<DatasetGap> = store
        .dataset_gaps()
        .unwrap()
        .into_iter()
        .filter(|g| g.gap_id == gap.gap_id)
        .collect();
    assert_eq!(mine, vec![gap, recovered]);
}

/// Never blind (contract v30, W10.6; the resilience spec's section 8): a
/// default read's source switches, and switches back, each its own record in
/// the `control` partition, read back in the order recorded.
#[test]
fn a_source_switch_and_its_switch_back_are_kept_in_the_control_partition() {
    use meridian_domain::v1::{SourceSwitchReason, SourceSwitched};
    let store = store();
    store.adopt().unwrap();
    let from = unique("switch-a");
    let to = unique("switch-b");
    let before = store
        .read_changes(meridian_changes::CONTROL, i64::MAX as u64, 1)
        .unwrap()
        .head;
    let switch = |reason: SourceSwitchReason, a: &str, b: &str, at: i64| SourceSwitched {
        data_type: "meridian.v1.Quote".into(),
        kind: 0,
        from_dataset: a.into(),
        to_dataset: b.into(),
        subjects: Vec::new(),
        reason: reason as i32,
        switched_at_ns: at,
    };
    store
        .record_switches(&[switch(SourceSwitchReason::Stalled, &from, &to, 10)])
        .unwrap();
    store
        .record_switches(&[switch(SourceSwitchReason::Restored, &to, &from, 20)])
        .unwrap();
    let mine: Vec<SourceSwitched> = store
        .source_switches()
        .unwrap()
        .into_iter()
        .filter(|s| s.from_dataset == from || s.to_dataset == from)
        .collect();
    assert_eq!(
        mine.iter().map(|s| s.reason).collect::<Vec<_>>(),
        vec![
            SourceSwitchReason::Stalled as i32,
            SourceSwitchReason::Restored as i32
        ]
    );
    let control = store
        .read_changes(
            meridian_changes::CONTROL,
            before,
            meridian_changes::PAGE_MOST,
        )
        .unwrap();
    assert!(
        control
            .changes
            .iter()
            .filter(|c| c.kind == "meridian.v1.SourceSwitched")
            .count()
            >= 2
    );
}

/// A new database timeline raises the epoch (contract v30, stage C; the
/// resilience spec's section 3): the identity a writer first sees is kept,
/// nothing raised; one unlike it -- a failover or a restore -- raises the
/// epoch on every head row, recorded with that cause, and the writer's next
/// batch commits under it; a writer whose epoch another raised does nothing.
#[test]
fn a_new_database_timeline_raises_the_epoch_and_the_next_batch_commits_under_it() {
    use meridian_changes::timeline::Timeline;
    let (url, writer) = isolated("lake-timeline");
    let dataset = unique("coinbase-1:daily");
    let subject = unique("LCL-BTC");
    assert_eq!(writer.adopt().unwrap(), 1);
    writer
        .record(
            &dataset,
            vec![close(&dataset, "a", &subject, "2026-10-01", 1)],
            10,
        )
        .unwrap();
    assert_eq!(writer.follow_timeline().unwrap(), Timeline::First);
    assert_eq!(writer.follow_timeline().unwrap(), Timeline::Same);
    let mut admin = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    // As a failover or a restore leaves the record: another database's.
    admin
        .execute(
            "UPDATE lake_change_head SET database_identity = 'another/00000001'
              WHERE partition = 'control'",
            &[],
        )
        .unwrap();
    assert_eq!(writer.follow_timeline().unwrap(), Timeline::Moved(2));
    assert_eq!(writer.epoch(), 2);
    let why: String = admin
        .query_one("SELECT why FROM lake_writer_took_over WHERE epoch = 2", &[])
        .unwrap()
        .get(0);
    assert_eq!(why, meridian_changes::timeline::DATABASE_TIMELINE);
    writer
        .record(
            &dataset,
            vec![close(&dataset, "b", &subject, "2026-10-02", 2)],
            20,
        )
        .unwrap();
    let epochs: Vec<u64> = every_change(&writer, &dataset, meridian_changes::PAGE_MOST)
        .iter()
        .map(|c| c.epoch)
        .collect();
    assert_eq!(epochs, vec![1, 2]);
    assert_eq!(writer.follow_timeline().unwrap(), Timeline::Same);
    // Another took the lake over: a writer whose epoch is gone raises none.
    let standby = PostgresStore::connect(&url, 1).unwrap();
    assert_eq!(
        standby
            .take_over(&meridian_changes::TookOver::unnamed("a test"))
            .unwrap(),
        3
    );
    admin
        .execute(
            "UPDATE lake_change_head SET database_identity = 'yet another/00000001'
              WHERE partition = 'control'",
            &[],
        )
        .unwrap();
    assert_eq!(writer.follow_timeline().unwrap(), Timeline::NotMine(3));
}

/// A record read on a read replica is stamped from its own snapshot
/// (contract v30, stage C; the resilience spec's section 5): asked, a read
/// says the head row of each dataset it read from the same repeatable-read
/// transaction as its rows; not asked, it says nothing.
#[test]
fn a_record_read_says_the_heads_of_the_snapshot_it_answered_from() {
    let store = store();
    let dataset = unique("coinbase-1:daily");
    let subject = unique("LCL-BTC");
    store
        .record(
            &dataset,
            vec![
                close(&dataset, "a", &subject, "2026-10-01", 1),
                close(&dataset, "b", &subject, "2026-10-02", 2),
            ],
            10,
        )
        .unwrap();
    let query = Query {
        datasets: vec![dataset.clone()],
        subjects: vec![subject.clone()],
        data_type: DataType::Price,
        kinds: Vec::new(),
        interval_ns: 0,
        when: When::Latest { at_ns: 0 },
        as_of_ns: 0,
        published_as_of_ns: 0,
        latest_only: false,
    };
    meridian_lake::postgres::stamp_from_snapshot();
    let rows = store.read(&query).unwrap();
    let heads = meridian_lake::postgres::snapshot_heads().expect("the snapshot's heads");
    assert_eq!(rows.len(), 2);
    assert_eq!(heads.get(&dataset), Some(&2), "{heads:?}");
    // Not asked: nothing said.
    store.read(&query).unwrap();
    assert!(meridian_lake::postgres::snapshot_heads().is_none());
}

/// A new database timeline at two writer groups (contract v30, stage C;
/// step 3d; PR CI 43684440785, ha, the writer groups phase: "the lake's
/// group 1 owns drive-data:drive under its own epoch (group 1 19, the
/// dataset 20, control 19)" and 0 rows under 19). Group 0's writer raises
/// every group's epoch to one -- the control partition's, each dataset's
/// and group 1's row alike -- and group 1's writer is fenced: nothing is
/// ever committed under that epoch for group 1. Its takeover then raises
/// its row and its datasets together, in one transaction, past it, and it
/// commits under that. So the three epochs read in one statement are never
/// apart, and what the failover run read -- apart, each side of that
/// takeover's commit -- was two states, not one.
#[test]
fn a_new_database_timeline_at_two_writer_groups_fences_the_second_until_it_takes_over_again() {
    use meridian_changes::groups::Groups;
    use meridian_changes::timeline::Timeline;
    use meridian_lake::store::StoreError;
    let (url, migrated) = isolated("lake-groups-timeline");
    drop(migrated);
    let of_group = |group: u32| {
        (0..)
            .map(|n| unique(&format!("coinbase-1:t{n}")))
            .find(|dataset| meridian_changes::groups::owner(dataset, 2) == group)
            .unwrap()
    };
    let (zeros, ones) = (of_group(0), of_group(1));
    let member = |group: u32| {
        PostgresStore::connect(&url, 2)
            .unwrap()
            .with_groups(Groups::new(group, 2).unwrap())
    };
    let (zero, one) = (member(0), member(1));
    let subject = unique("LCL-BTC");
    let row = |dataset: &str, key: &str| vec![close(dataset, key, &subject, "2026-10-01", 1)];
    zero.adopt().unwrap();
    let one_epoch = one.adopt().unwrap();
    zero.record(&zeros, row(&zeros, "a"), 10).unwrap();
    one.record(&ones, row(&ones, "a"), 10).unwrap();
    let mut admin = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    // As the failover run reads them, in one statement: group 1's row, the
    // control partition, group 1's dataset, group 0's, and group 1's
    // dataset's rows under group 1's row's epoch.
    let mut read = || -> (i64, i64, i64, i64, i64) {
        let row = admin
            .query_one(
                "SELECT (SELECT epoch FROM lake_writer_group WHERE writer_group = 1),
                        (SELECT epoch FROM lake_change_head WHERE partition = 'control'),
                        (SELECT epoch FROM lake_partition WHERE dataset = $1),
                        (SELECT epoch FROM lake_partition WHERE dataset = $2),
                        (SELECT count(*) FROM lake_row WHERE dataset = $1 AND epoch =
                            (SELECT epoch FROM lake_writer_group WHERE writer_group = 1))",
                &[&ones, &zeros],
            )
            .unwrap();
        (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4))
    };
    let (own, control, theirs, _, under) = read();
    assert_eq!(
        (own, theirs, under),
        (one_epoch as i64, one_epoch as i64, 1)
    );
    assert_ne!(control, own, "each group its own epoch");
    assert_eq!(zero.follow_timeline().unwrap(), Timeline::First);
    // A failover or a restore: group 0's writer, which holds the control
    // partition, raises every group's. Group 1's own look raises nothing.
    postgres::Client::connect(&url, postgres::NoTls)
        .unwrap()
        .execute(
            "UPDATE lake_change_head SET database_identity = 'another/00000001'
              WHERE partition = 'control'",
            &[],
        )
        .unwrap();
    assert_eq!(one.follow_timeline().unwrap(), Timeline::Same);
    let Timeline::Moved(raised) = zero.follow_timeline().unwrap() else {
        panic!("the epoch raised on the new timeline");
    };
    let raised = raised as i64;
    assert_eq!(
        read(),
        (raised, raised, raised, raised, 0),
        "one epoch for every group, and nothing of group 1's under it"
    );
    // Group 1's writer is fenced, so nothing ever is.
    assert!(matches!(
        one.record(&ones, row(&ones, "b"), 20),
        Err(StoreError::Fenced(_))
    ));
    assert_eq!(read().4, 0);
    // It takes over again: its row and its dataset together, past it.
    let standby = member(1);
    let again = standby
        .take_over(&meridian_changes::TookOver::unnamed("a test"))
        .unwrap() as i64;
    assert!(again > raised);
    assert_eq!(read(), (again, raised, again, raised, 0));
    standby.record(&ones, row(&ones, "b"), 30).unwrap();
    zero.record(&zeros, row(&zeros, "b"), 30)
        .expect("group 0 writes on");
    assert_eq!(read(), (again, raised, again, raised, 1));
    // Once per timeline (CI 43804841979: in tolerant, the fenced writer
    // steps down and its pod starts it again): group 0's next looks raise
    // nothing more, and group 1's writer, taken over again, is never fenced
    // again until the database moves once more -- no step-down loop.
    for _ in 0..3 {
        assert_eq!(zero.follow_timeline().unwrap(), Timeline::Same);
    }
    assert_eq!(standby.head_epoch().unwrap(), again as u64);
    standby
        .record(&ones, row(&ones, "c"), 40)
        .expect("group 1's writer, taken over again, writes on");
    assert_eq!(read(), (again, raised, again, raised, 2));
}

/// Two writer groups of one lake (step 3d; the resilience spec's sections
/// 6.1 and 6.7): each commits to its own datasets alone, under its own
/// epoch -- group 0's the control partition's, a group past it its own row
/// in `lake_writer_group` -- and a takeover of one raises its datasets and
/// its row alone, so the other writes on; the deposed writer of the group
/// taken over is refused a batch, and a dataset it begins after it.
#[test]
fn writer_groups_commit_their_own_datasets_under_their_own_epochs() {
    use meridian_changes::groups::Groups;
    use meridian_lake::store::StoreError;
    let (url, migrated) = isolated("lake-groups");
    drop(migrated);
    let of_group = |group: u32| {
        (0..)
            .map(|n| unique(&format!("coinbase-1:g{n}")))
            .find(|dataset| meridian_changes::groups::owner(dataset, 2) == group)
            .unwrap()
    };
    let (zeros, ones) = (of_group(0), of_group(1));
    let member = |group: u32| {
        PostgresStore::connect(&url, 2)
            .unwrap()
            .with_groups(Groups::new(group, 2).unwrap())
    };
    let (zero, one) = (member(0), member(1));
    let subject = unique("LCL-BTC");
    let row = |dataset: &str, key: &str| vec![close(dataset, key, &subject, "2026-10-01", 1)];
    // Group 1 first takes its group's epoch: past every epoch the lake holds.
    let zero_epoch = zero.adopt().unwrap();
    let one_epoch = one.adopt().unwrap();
    assert!(one_epoch > zero_epoch, "{one_epoch} past {zero_epoch}");
    zero.record(&zeros, row(&zeros, "a"), 10).unwrap();
    one.record(&ones, row(&ones, "a"), 10).unwrap();
    // Each refused the other's dataset, nothing written.
    for (store, dataset, group) in [(&zero, &ones, 1), (&one, &zeros, 0)] {
        match store.record(dataset, row(dataset, "b"), 20) {
            Err(StoreError::NotOwned { group: said, .. }) => assert_eq!(said, group),
            other => panic!("another group's dataset written: {:?}", other.is_ok()),
        }
    }
    // Group 1 taken over: its datasets and its row raised, group 0's not.
    let standby = member(1);
    let raised = standby
        .take_over(&meridian_changes::TookOver::unnamed("a test"))
        .unwrap();
    assert!(raised > one_epoch);
    assert_eq!(one.head_epoch().unwrap(), raised, "its row raised");
    zero.record(&zeros, row(&zeros, "c"), 30)
        .expect("group 0 writes on");
    standby.record(&ones, row(&ones, "c"), 30).unwrap();
    // The deposed writer of group 1: refused a batch, and a dataset begun.
    assert!(matches!(
        one.record(&ones, row(&ones, "d"), 40),
        Err(StoreError::Fenced(_))
    ));
    let begun = of_group(1);
    assert!(matches!(
        one.record(&begun, row(&begun, "a"), 40),
        Err(StoreError::Fenced(_))
    ));
    standby.record(&begun, row(&begun, "a"), 40).unwrap();
    // Group 0's writer records each dataset's group where it changed: once.
    assert_eq!(
        zero.record_assignments(Groups::new(0, 2).unwrap(), 50)
            .unwrap(),
        2
    );
    assert_eq!(
        zero.record_assignments(Groups::new(0, 2).unwrap(), 60)
            .unwrap(),
        0
    );
}

/// Subjects whose hash falls in the lower and the upper half of the space.
fn halves_of(tag: &str) -> (String, String) {
    let mut low = None;
    let mut high = None;
    for n in 0.. {
        let subject = unique(&format!("{tag}-{n}"));
        if meridian_lake::range::hash(&subject) < 0x8000 {
            low.get_or_insert(subject);
        } else {
            high.get_or_insert(subject);
        }
        if low.is_some() && high.is_some() {
            break;
        }
    }
    (low.unwrap(), high.unwrap())
}

/// Step 3d (section 6.1): a split is a continuation. The parent closes at
/// its head N with its rows kept; each child opens naming it and numbers
/// from N + 1; a subject's chain runs on across it, each row in the child
/// holding its first subject's hash; a key restated after it keeps its
/// version and goes to its child; a read after a watermark naming the
/// parent reads on into both children; and the split is recorded in the
/// control partition.
#[test]
fn a_split_closes_the_range_and_its_children_number_on_from_its_close() {
    let store = store();
    let dataset = unique("coinbase-1:live");
    let (low, high) = halves_of("LCL-SPLIT");
    let first = store
        .record(
            &dataset,
            vec![
                trade(&dataset, "l1", &low, 10),
                trade(&dataset, "h1", &high, 11),
                trade(&dataset, "l2", &low, 12),
            ],
            1_000,
        )
        .unwrap();
    assert_eq!(first.head, 3);
    assert!(
        first.partitions.is_empty(),
        "one full range: no range named"
    );

    let split = store
        .split_partition(&dataset, "admin-1", "a busy dataset", 1_500)
        .unwrap();
    let lower = format!("{dataset}/0000-7fff");
    let upper = format!("{dataset}/8000-ffff");
    assert_eq!(split.parent, dataset);
    assert_eq!(split.children, vec![lower.clone(), upper.clone()]);
    assert_eq!(split.closing_sequence, 3);
    assert_eq!(split.split_by, "admin-1");
    assert_eq!(
        store
            .splits()
            .unwrap()
            .into_iter()
            .filter(|kept| kept.parent == dataset)
            .collect::<Vec<_>>(),
        vec![split.clone()]
    );
    let heads = store.heads().unwrap();
    assert_eq!(
        (heads[&dataset], heads[&lower], heads[&upper]),
        (3, 3, 3),
        "the parent closed at N, each child opened at it"
    );

    // Rows after it: each in the child holding its subject's hash, each
    // child numbering from N + 1, each subject's chain unbroken.
    let after = store
        .record(
            &dataset,
            vec![
                trade(&dataset, "l3", &low, 13),
                trade(&dataset, "h2", &high, 14),
                // A key from before the split, restated: its next version,
                // in the child of its anchor.
                restate(trade(&dataset, "l1", &low, 10)),
            ],
            2_000,
        )
        .unwrap();
    assert_eq!((after.recorded, after.restated), (2, 1));
    let placed: Vec<(String, u64, u64, u64)> = after
        .rows
        .iter()
        .enumerate()
        .map(|(at, row)| {
            (
                after.partition_of(&dataset, at).to_string(),
                row.meta().sequence,
                row.meta().previous_sequence,
                row.meta().version,
            )
        })
        .collect();
    assert!(placed.contains(&(lower.clone(), 4, 3, 1)), "{placed:?}");
    assert!(placed.contains(&(lower.clone(), 5, 4, 2)), "{placed:?}");
    assert!(placed.contains(&(upper.clone(), 4, 2, 1)), "{placed:?}");
    let heads = store.heads().unwrap();
    assert_eq!((heads[&dataset], heads[&lower], heads[&upper]), (3, 5, 4));

    // Each range's log is its own.
    let lower_log = store.read_changes(&lower, 3, 1 << 20).unwrap();
    assert_eq!(
        lower_log
            .changes
            .iter()
            .map(|c| (c.partition.clone(), c.sequence))
            .collect::<Vec<_>>(),
        vec![(lower.clone(), 4), (lower.clone(), 5)]
    );
    assert_eq!(store.read_changes(&dataset, 0, 1 << 20).unwrap().head, 3);

    // A catch-up after a watermark naming the parent reads on into both
    // children; one naming a child reads after its own sequence there.
    let catch_up = |watermark: std::collections::BTreeMap<String, u64>| {
        let mut keys: Vec<String> = store
            .read(&Query {
                datasets: vec![dataset.clone()],
                subjects: vec![low.clone(), high.clone()],
                data_type: DataType::Trade,
                kinds: vec![],
                interval_ns: 0,
                when: When::After(watermark),
                as_of_ns: 0,
                published_as_of_ns: 0,
                latest_only: false,
            })
            .unwrap()
            .iter()
            .map(|r| format!("{}@{}", r.meta().row_key, r.meta().version))
            .collect();
        keys.sort();
        keys
    };
    assert_eq!(
        catch_up([(dataset.clone(), 2)].into()),
        vec!["h2@1", "l1@2", "l2@1", "l3@1"]
    );
    assert_eq!(
        catch_up([(dataset.clone(), 3), (lower.clone(), 4), (upper.clone(), 3)].into()),
        vec!["h2@1", "l1@2"]
    );
    // A read of the dataset reads every range.
    let latest = store
        .read(&Query {
            datasets: vec![dataset.clone()],
            subjects: vec![low.clone(), high.clone()],
            data_type: DataType::Trade,
            kinds: vec![],
            interval_ns: 0,
            when: When::Latest { at_ns: 0 },
            as_of_ns: 0,
            published_as_of_ns: 0,
            latest_only: false,
        })
        .unwrap();
    assert_eq!(latest.len(), 5, "every key at its version in force");

    // A child halves in turn; the parent, closed, does not.
    let quarter = store
        .split_partition(&upper, "admin-1", "busier still", 2_500)
        .unwrap();
    assert_eq!(
        quarter.children,
        vec![
            format!("{dataset}/8000-bfff"),
            format!("{dataset}/c000-ffff")
        ]
    );
    assert_eq!(quarter.closing_sequence, 4);
    let refused = |partition: &str| match store.split_partition(partition, "admin-1", "why", 3_000)
    {
        Err(meridian_lake::StoreError::Invalid { field, words }) => {
            assert_eq!(field, "partition");
            words
        }
        other => panic!("{partition}: {other:?}"),
    };
    assert!(refused(&dataset).contains("not a range this lake has open"));
    assert!(refused(&upper).contains("not a range this lake has open"));
    assert!(refused("control").contains("control partition"));
    assert!(refused("P0").contains("book partition"));
    assert!(refused(&unique("none:none")).contains("not a range this lake has open"));
    assert!(refused(&format!("{dataset}/0005-0005")).contains("one hash value"));
}

/// The lake's rule 5 (step 3d; ruled 2026-11-12): a row key fixes its valid
/// time and its first subject. A restatement changing either is refused
/// naming the field, and nothing of its batch is kept; one naming the record
/// that replaced its first subject is no change, and keeps the key's range.
#[test]
fn a_restatement_keeps_its_keys_valid_time_and_first_subject() {
    let store = store();
    let dataset = unique("coinbase-1:live");
    let subject = unique("LCL-BTC");
    let stays = unique("INS-BTC");
    store
        .record(&dataset, vec![trade(&dataset, "k", &subject, 10)], 1_000)
        .unwrap();
    let field = |refused: meridian_lake::StoreError| match refused {
        meridian_lake::StoreError::KeyFixed { field, row_key, .. } => (field, row_key),
        other => panic!("{other:?}"),
    };
    let moved = store
        .record(
            &dataset,
            vec![
                trade(&dataset, "fresh", &subject, 30),
                trade(&dataset, "k", &subject, 20),
            ],
            1_100,
        )
        .unwrap_err();
    assert_eq!(field(moved), ("meta.valid_from_ns", "k".to_string()));
    let other = store
        .record(&dataset, vec![trade(&dataset, "k", &stays, 10)], 1_200)
        .unwrap_err();
    assert_eq!(
        field(other),
        ("meta.subjects[0].entity_id", "k".to_string())
    );
    assert_eq!(
        store.heads().unwrap()[&dataset],
        1,
        "nothing of either kept"
    );
    // Replaced by the record that stays: followed by alias, restated.
    store.keep_alias(&subject, &stays, 1_300).unwrap();
    let restated = store
        .record(
            &dataset,
            vec![restate(trade(&dataset, "k", &stays, 10))],
            1_400,
        )
        .unwrap();
    assert_eq!(restated.restated, 1);
}

/// Step 3d (section 6.2; section 15's online conversions): `lake_row`
/// partitioned on upgrade, online and under write load. The rows kept
/// before it stand as the new table's default partition and are copied
/// behind a page at a time while a writer records; every read of them
/// answers the same before, between every page, and after; nothing the
/// writer recorded meanwhile is missed; and once the default is empty it is
/// let go and `lake_latest` holds every key.
#[test]
fn lake_rows_are_partitioned_online_under_write_load_with_equal_answers() {
    let seq = COUNTER.fetch_add(1, Ordering::SeqCst);
    let name = format!("lake_convert_{}_{seq}", SystemClock.now_ns());
    let url = own_schema(&name);
    let migrated = PostgresStore::connect(&url, 4).unwrap();
    migrated.migrate(&SystemClock).unwrap();
    let dataset = unique("coinbase-1:live");
    let kept = unique("LCL-KEPT");
    let live = unique("LCL-LIVE");
    // Six months of a subject's rows, some restated, as the release before
    // kept them.
    const MONTH_NS: i64 = 31 * 86_400_000_000_000;
    let start = 1_780_000_000_000_000_000_i64;
    for batch in 0..12 {
        let rows: Vec<Observation> = (0..500)
            .map(|n| {
                let at = start + (batch / 2) * MONTH_NS + n;
                trade(&dataset, &format!("k{batch}-{n}"), &kept, at)
            })
            .collect();
        migrated.record(&dataset, rows, 1_000 + batch).unwrap();
    }
    let restated: Vec<Observation> = (0..50)
        .map(|n| {
            restate(trade(
                &dataset,
                &format!("k3-{n}"),
                &kept,
                start + MONTH_NS + n,
            ))
        })
        .collect();
    migrated.record(&dataset, restated, 2_000).unwrap();
    let mut sql = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    sql.batch_execute(
        "CREATE TABLE lake_row_legacy (LIKE lake_row INCLUDING DEFAULTS);
         WITH moved AS (DELETE FROM lake_row RETURNING *)
         INSERT INTO lake_row_legacy SELECT * FROM moved;
         UPDATE lake_row_legacy SET partition = NULL, anchor = NULL;
         ALTER TABLE lake_row_legacy
           ADD CONSTRAINT lake_row_unpartitioned CHECK (partition IS NULL);
         ALTER TABLE lake_row ATTACH PARTITION lake_row_legacy DEFAULT;
         DELETE FROM lake_latest;
         UPDATE lake_conversion SET latest_all = false;",
    )
    .unwrap();
    drop(migrated);
    let store = Arc::new(PostgresStore::connect(&url, 4).unwrap());
    let legacy_rows = |sql: &mut postgres::Client| -> i64 {
        let there: bool = sql
            .query_one("SELECT to_regclass('lake_row_legacy') IS NOT NULL", &[])
            .unwrap()
            .get(0);
        if !there {
            return -1;
        }
        sql.query_one("SELECT count(*) FROM lake_row_legacy", &[])
            .unwrap()
            .get(0)
    };
    assert_eq!(
        legacy_rows(&mut sql),
        6_050,
        "every row kept so far in the default"
    );

    let answers = |store: &PostgresStore| {
        let latest = keys_of(&store.read(&latest_query(&dataset, &kept, true)).unwrap());
        let every = store
            .read(&Query {
                when: When::After([(dataset.clone(), 0)].into()),
                ..latest_query(&dataset, &kept, false)
            })
            .unwrap()
            .iter()
            .map(|r| (r.meta().sequence, r.meta().version))
            .collect::<Vec<_>>();
        let in_force = store
            .read(&latest_query(&dataset, &kept, false))
            .unwrap()
            .len();
        (latest, every, in_force)
    };
    let before = answers(&store);
    assert_eq!(before.2, 6_000, "each key at its version in force");
    assert_eq!(before.1.iter().filter(|(_, v)| *v == 2).count(), 50);

    // A writer records throughout, another subject, a row at a time.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let (store, stop, dataset, live) = (
            Arc::clone(&store),
            Arc::clone(&stop),
            dataset.clone(),
            live.clone(),
        );
        std::thread::spawn(move || {
            let mut written = 0;
            while !stop.load(Ordering::Relaxed) {
                store
                    .record(
                        &dataset,
                        vec![trade(
                            &dataset,
                            &format!("w{written}"),
                            &live,
                            start + written,
                        )],
                        3_000 + written,
                    )
                    .unwrap();
                written += 1;
            }
            written
        })
    };
    let mut pages = 0;
    loop {
        let moved = store.convert_page().unwrap();
        if moved == 0 {
            break;
        }
        pages += 1;
        assert_eq!(
            answers(&store),
            before,
            "the same answer between pages ({pages})"
        );
    }
    stop.store(true, Ordering::Relaxed);
    let written = writer.join().unwrap();
    assert!(pages >= 2, "copied a page at a time: {pages}");
    assert_eq!(legacy_rows(&mut sql), -1, "the default let go once empty");
    let after = PostgresStore::connect(&url, 4).unwrap();
    assert_eq!(answers(&after), before, "the same answer after");
    let live_rows = after
        .read(&Query {
            when: When::After([(dataset.clone(), 0)].into()),
            ..latest_query(&dataset, &live, false)
        })
        .unwrap();
    assert_eq!(
        live_rows.len() as i64,
        written,
        "nothing written meanwhile missed"
    );
    let latest_all: bool = sql
        .query_one("SELECT latest_all FROM lake_conversion", &[])
        .unwrap()
        .get(0);
    assert!(latest_all, "lake_latest holds every key");
    let keys: i64 = sql
        .query_one(
            "SELECT count(*) FROM lake_latest WHERE dataset = $1",
            &[&dataset],
        )
        .unwrap()
        .get(0);
    assert_eq!(keys, 6_000 + written);
    // Each of the six months in its own table.
    let months: i64 = sql
        .query_one(
            "SELECT count(*) FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid
              WHERE i.inhparent = $1::text::regclass",
            &[&meridian_lake::postgres::range_table_of(&dataset)],
        )
        .unwrap()
        .get(0);
    assert_eq!(months, 6);
}

// ── A database per writer group (step 3d; the resilience spec's section 6.4) ──

/// Two fresh databases beside the test database, each migrated: a lake of
/// two writer groups on two databases, alone, so no other test's datasets
/// are taken over by its groups. Each is dropped with its URL's last clone,
/// a failed test's too, and one made before the other failed (m9 and PO 65:
/// runs left their `lake2_*` databases behind).
fn two_databases() -> Vec<Scratch> {
    databases(2)
}

fn databases(count: usize) -> Vec<Scratch> {
    let tag = unique("lake2").replace([':', '-'], "_");
    (0..count)
        .map(|d| {
            let made = Scratch::database(&test_database(), &format!("{tag}_{d}"));
            PostgresStore::connect(&made, 2)
                .unwrap()
                .migrate(&SystemClock)
                .unwrap();
            made
        })
        .collect()
}

/// A member of writer group `group` of two, over both databases.
fn spread(urls: &[Scratch], group: u32) -> meridian_lake::spread::Spread {
    let groups = meridian_changes::groups::Groups::new(group, 2).unwrap();
    let stores = urls
        .iter()
        .enumerate()
        .map(|(d, url)| {
            Arc::new(
                PostgresStore::connect(url, 4)
                    .unwrap()
                    .with_groups(groups)
                    .with_databases(urls.len() as u32)
                    .in_database(d as u32),
            )
        })
        .collect();
    meridian_lake::spread::Spread::new(stores, groups)
}

/// A dataset writer group `group` of two owns.
fn owned_by(group: u32) -> String {
    loop {
        let dataset = unique("coinbase-1:daily");
        if meridian_changes::groups::owner(&dataset, 2) == group {
            return dataset;
        }
    }
}

/// Section 6.4: group g writes to database g mod 2, alone; a read naming
/// both groups' datasets fans out and answers both, in dataset order, and
/// the heads of both; a write of another group's dataset is refused here
/// (it is handed on).
#[test]
fn two_groups_write_two_databases_and_a_read_spans_both() {
    let urls = two_databases();
    let (zero, one) = (spread(&urls, 0), spread(&urls, 1));
    let (mine, theirs) = (owned_by(0), owned_by(1));
    let subject = unique("LCL-BTC");
    zero.record(
        &mine,
        vec![close(&mine, "a", &subject, "2026-10-07", 10)],
        1_000,
    )
    .unwrap();
    one.record(
        &theirs,
        vec![close(&theirs, "a", &subject, "2026-10-07", 20)],
        1_000,
    )
    .unwrap();
    // Each in its own group's database alone.
    assert!(zero.database(0).heads().unwrap().contains_key(&mine));
    assert!(!zero.database(1).heads().unwrap().contains_key(&mine));
    assert!(zero.database(1).heads().unwrap().contains_key(&theirs));
    assert!(!zero.database(0).heads().unwrap().contains_key(&theirs));
    // Refused where another group owns it, never written here.
    assert!(matches!(
        one.record(
            &mine,
            vec![close(&mine, "b", &subject, "2026-10-07", 11)],
            1_000
        ),
        Err(meridian_lake::store::StoreError::NotOwned { group: 0, .. })
    ));
    let mut datasets = vec![mine.clone(), theirs.clone()];
    datasets.sort();
    let read = zero
        .read(&Query {
            datasets: datasets.clone(),
            subjects: vec![subject.clone()],
            data_type: DataType::Price,
            kinds: vec![],
            interval_ns: 0,
            when: When::BusinessDate(Date::parse("2026-10-07").unwrap()),
            as_of_ns: 0,
            published_as_of_ns: 0,
            latest_only: false,
        })
        .unwrap();
    let answered: Vec<String> = read.iter().map(|row| row.dataset().to_string()).collect();
    assert_eq!(answered, datasets, "both databases' rows, in dataset order");
    let heads = one.heads().unwrap();
    assert_eq!((heads.get(&mine), heads.get(&theirs)), (Some(&1), Some(&1)));
}

/// Approved 2026-11-14: an alias database 0 keeps is in every database,
/// where rule 5 reads it; one kept in database 0 alone -- a crash between
/// the databases -- is found and copied by the reconcile, and said; a
/// closed-day re-check of a row in database 1 likewise, and its read there
/// carries it.
#[test]
fn what_database_zero_keeps_reaches_every_database_and_a_gap_between_is_repaired() {
    let urls = two_databases();
    let zero = spread(&urls, 0);
    let theirs = owned_by(1);
    let subject = unique("LCL-BTC");
    let one = spread(&urls, 1);
    let recorded = one
        .record(
            &theirs,
            vec![close(&theirs, "a", &subject, "2026-10-07", 20)],
            1_000,
        )
        .unwrap();
    // Kept through the lake: in both at once.
    zero.keep_alias("LCL-OLD-1", "INS-NEW-1", 1_000).unwrap();
    assert!(zero
        .database(1)
        .alias_rows()
        .unwrap()
        .iter()
        .any(|(replaced, _, _)| replaced == "LCL-OLD-1"));
    // A crash between the databases: database 0 alone holds them.
    zero.database(0)
        .keep_alias("LCL-OLD-2", "INS-NEW-2", 1_001)
        .unwrap();
    let amendment = meridian_lake::store::ClosedDayAmendment {
        dataset: theirs.clone(),
        sequence: recorded.rows[0].meta().sequence,
        row_key: "a".into(),
        closed_day: Some(meridian_domain::v1::DateCheck {
            date: "2026-10-07".into(),
            reason: meridian_domain::v1::DateCheckReason::Holiday as i32,
            ..Default::default()
        }),
        why: "calendar CAL-USSS version 2 applied".into(),
        at_ns: 1_002,
    };
    zero.database(0)
        .amend_closed_day(std::slice::from_ref(&amendment))
        .unwrap();
    assert!(zero.database(1).closed_day_amendments().unwrap().is_empty());
    let repaired = zero.reconcile().unwrap();
    assert_eq!(
        (
            repaired[1].aliases,
            repaired[1].amendments,
            repaired[1].assessments
        ),
        (1, 1, 0),
        "the alias and the re-check database 1 was missing"
    );
    assert!(
        !zero.reconcile().unwrap()[1].any(),
        "a second reconcile finds nothing"
    );
    // The row read in its own database carries the re-check.
    let read = zero
        .read(&Query {
            datasets: vec![theirs.clone()],
            subjects: vec![subject.clone()],
            data_type: DataType::Price,
            kinds: vec![],
            interval_ns: 0,
            when: When::BusinessDate(Date::parse("2026-10-07").unwrap()),
            as_of_ns: 0,
            published_as_of_ns: 0,
            latest_only: false,
        })
        .unwrap();
    assert_eq!(
        read[0].meta().closed_day.as_ref().map(|day| day.reason),
        Some(meridian_domain::v1::DateCheckReason::Holiday as i32)
    );
    // Through the lake, a re-check reaches database 1 at once.
    let again = meridian_lake::store::ClosedDayAmendment {
        at_ns: 1_003,
        ..amendment
    };
    zero.amend_closed_day(std::slice::from_ref(&again)).unwrap();
    assert_eq!(zero.database(1).closed_day_amendments().unwrap().len(), 2);
    assert!(!zero.reconcile().unwrap()[1].any());
}

/// A database past the first records its writer group's takeover in its
/// own control journal, atomic with its epochs, and database 0's relays
/// it, once.
#[test]
fn a_database_past_the_firsts_own_records_are_relayed_into_database_zero_once() {
    let urls = two_databases();
    let (zero, one) = (spread(&urls, 0), spread(&urls, 1));
    zero.adopt().unwrap();
    one.take_over(&meridian_changes::TookOver::unnamed("a test's takeover"))
        .unwrap();
    let taken = |store: &PostgresStore| {
        store
            .control_entries()
            .unwrap()
            .iter()
            .filter(|(kind, _, _)| kind == "meridian.v1.WriterTookOver")
            .count()
    };
    let before = taken(zero.database(0));
    assert_eq!(taken(zero.database(1)), 1);
    assert_eq!(zero.relay().unwrap(), 1);
    assert_eq!(taken(zero.database(0)), before + 1);
    assert_eq!(zero.relay().unwrap(), 0, "each record relayed once");
    assert!(one.relay().is_err(), "database 0's writes are group 0's");
}

/// A month's table is analysed once the batch that first fills it commits
/// (step 3d; migration 24, `lake_analyze_month` approved 2026-11-14): until
/// then every estimate of it clamped to one row, and a lookup of one row
/// could take the subject index and scan the dataset's month whole.
#[test]
fn a_months_table_is_analysed_once_its_first_batch_commits() {
    let store = store();
    let dataset = unique("analysed");
    store
        .record(
            &dataset,
            (0..3)
                .map(|n| close(&dataset, &format!("k{n}"), "INS-1", "2026-09-08", n))
                .collect(),
            1,
        )
        .unwrap();
    let url = std::env::var("MERIDIAN_TEST_DATABASE_URL").unwrap();
    let mut raw = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let (leaf, reltuples): (String, f32) = {
        let row = raw
            .query_one(
                "SELECT c.relname::text, c.reltuples FROM pg_class c
                  WHERE c.oid = (SELECT tableoid FROM lake_row WHERE dataset = $1 LIMIT 1)",
                &[&dataset],
            )
            .unwrap();
        (row.get(0), row.get(1))
    };
    assert!(leaf.starts_with("lake_row_"), "{leaf}");
    assert_eq!(
        reltuples, 3.0,
        "{leaf} analysed with its first batch's rows"
    );
    let (analysed_at, again): (Option<std::time::SystemTime>, bool) = {
        let at = raw
            .query_one(
                "SELECT last_analyze::timestamptz FROM pg_stat_user_tables WHERE relname = $1",
                &[&leaf],
            )
            .unwrap()
            .get::<_, Option<std::time::SystemTime>>(0);
        store
            .record(
                &dataset,
                vec![close(&dataset, "k9", "INS-1", "2026-09-08", 9)],
                2,
            )
            .unwrap();
        let later = raw
            .query_one(
                "SELECT last_analyze::timestamptz FROM pg_stat_user_tables WHERE relname = $1",
                &[&leaf],
            )
            .unwrap()
            .get::<_, Option<std::time::SystemTime>>(0);
        (at, later != at)
    };
    assert!(
        analysed_at.is_some(),
        "analysed by the lake, not autovacuum"
    );
    assert!(
        !again,
        "analysed once: the month's next batch leaves it to autovacuum"
    );
}

// ── Range moves (step 3d; section 6.8; ruled 2026-11-14) ────────────────────

/// Two writer groups' members over one database (isolated), each at its own
/// epoch, and a dataset of group 0's split in two, each half pinned to
/// group 0: (url, group 0, group 1, dataset, lower, upper, a low subject,
/// a high subject).
#[allow(clippy::type_complexity)]
fn two_groups_and_a_split(
    tag: &str,
) -> (
    Scratch,
    Arc<PostgresStore>,
    Arc<PostgresStore>,
    String,
    String,
    String,
    String,
    String,
) {
    two_groups_and_a_split_of(tag, 0)
}

/// [`two_groups_and_a_split`], the dataset and both its halves writer group
/// `owner`'s.
#[allow(clippy::type_complexity)]
fn two_groups_and_a_split_of(
    tag: &str,
    owner: u32,
) -> (
    Scratch,
    Arc<PostgresStore>,
    Arc<PostgresStore>,
    String,
    String,
    String,
    String,
    String,
) {
    use meridian_changes::groups::Groups;
    let (url, migrated) = isolated(tag);
    drop(migrated);
    let member = |group: u32| {
        Arc::new(
            PostgresStore::connect(&url, 4)
                .unwrap()
                .with_groups(Groups::new(group, 2).unwrap()),
        )
    };
    let (zero, one) = (member(0), member(1));
    zero.adopt().unwrap();
    one.adopt().unwrap();
    let dataset = (0..)
        .map(|n| unique(&format!("coinbase-1:mv{n}")))
        .find(|dataset| meridian_changes::groups::owner(dataset, 2) == owner)
        .unwrap();
    let (low, high) = halves_of(tag);
    let owning = if owner == 0 { &zero } else { &one };
    owning
        .record(
            &dataset,
            vec![
                trade(&dataset, "l0", &low, 10),
                trade(&dataset, "h0", &high, 11),
            ],
            1_000,
        )
        .unwrap();
    owning
        .split_partition(&dataset, "admin-1", "two halves", 1_100)
        .unwrap();
    assert_eq!(
        zero.pin_owners().unwrap(),
        3,
        "the parent and both children"
    );
    zero.read_owners().unwrap();
    one.read_owners().unwrap();
    let lower = format!("{dataset}/0000-7fff");
    let upper = format!("{dataset}/8000-ffff");
    (url, zero, one, dataset, lower, upper, low, high)
}

/// Rows written to `dataset` by `subject` until `stop`, each sent to the
/// group that owns its range now: tried again while held, handed to the
/// group a NotOwned names. Says how many committed, each refusal, and the
/// longest wait between two commits.
fn writing_range(
    zero: Arc<dyn Store>,
    one: Arc<dyn Store>,
    dataset: String,
    subject: String,
    tag: &'static str,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<(u64, Vec<String>, std::time::Duration)> {
    use meridian_lake::store::StoreError;
    std::thread::spawn(move || {
        let (mut written, mut refused) = (0u64, Vec::new());
        let mut longest = std::time::Duration::ZERO;
        let mut last = std::time::Instant::now();
        let mut to = 0u32;
        let mut n = 0u64;
        while !stop.load(Ordering::SeqCst) {
            n += 1;
            let rows = vec![trade(
                &dataset,
                &format!("{tag}{n}"),
                &subject,
                100 + n as i64,
            )];
            let store = if to == 0 { &zero } else { &one };
            match store.record(&dataset, rows, 2_000 + n as i64) {
                Ok(_) => {
                    written += 1;
                    longest = longest.max(last.elapsed());
                    last = std::time::Instant::now();
                }
                Err(StoreError::Held(_)) => {
                    n -= 1;
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(StoreError::NotOwned { group, .. }) => {
                    n -= 1;
                    to = group;
                }
                Err(failed) => refused.push(failed.to_string()),
            }
        }
        (written, refused, longest)
    })
}

/// Each subject's rows of `dataset` in this database, by sequence across
/// its ranges' lineage: each names the one before as its previous, never
/// a hole. Answers (partition, sequence, epoch) of `subject`'s.
fn chain_of(url: &str, dataset: &str, subject: &str) -> Vec<(String, i64, i64)> {
    let mut admin = postgres::Client::connect(url, postgres::NoTls).unwrap();
    let rows: Vec<(String, i64, i64, i64)> = admin
        .query(
            "SELECT coalesce(partition, dataset), sequence, previous_sequence, epoch FROM lake_row
              WHERE dataset = $1 AND subject = $2 ORDER BY sequence",
            &[&dataset, &subject],
        )
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3)))
        .collect();
    for pair in rows.windows(2) {
        assert_eq!(
            pair[1].2, pair[0].1,
            "{subject}: its chain broken at {pair:?}"
        );
    }
    rows.into_iter().map(|(p, s, _, e)| (p, s, e)).collect()
}

/// The kinds the lake's control partition logged, in order.
fn control_kinds(url: &str) -> Vec<String> {
    let mut admin = postgres::Client::connect(url, postgres::NoTls).unwrap();
    admin
        .query(
            "SELECT kind FROM lake_control_change ORDER BY sequence",
            &[],
        )
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect()
}

/// A lake range moved to another writer group under write load, within one
/// database (step 3d; section 6.8; fixtures/lake/move-lake-partition.yaml,
/// "a split range"): the switch holds the range's row, so its writes wait
/// while its sibling's commit on; they then find the target named and go
/// to it, none refused; the subject's chain runs on unbroken across the
/// split and the switch, the target's epoch from it on; the rows a read
/// named before the switch are read after it; the record is logged in the
/// lake's control; and the range moved back is written by group 0 at its
/// own epoch again, never fenced.
#[test]
fn a_lake_range_moved_under_load_keeps_its_chain_and_its_sibling_writes_on() {
    use meridian_lake::partition_moves::{Asked, Ended, Mover, Settings};
    let (url, zero, one, dataset, lower, upper, low, high) = two_groups_and_a_split("lake-move");
    let before = zero.heads().unwrap()[&lower];
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let moving = writing_range(
        zero.clone() as Arc<dyn Store>,
        one.clone() as Arc<dyn Store>,
        dataset.clone(),
        low.clone(),
        "l",
        stop.clone(),
    );
    let sibling = writing_range(
        zero.clone() as Arc<dyn Store>,
        one.clone() as Arc<dyn Store>,
        dataset.clone(),
        high.clone(),
        "h",
        stop.clone(),
    );
    std::thread::sleep(std::time::Duration::from_millis(300));
    let mover = Mover::new(
        vec![zero.clone()],
        zero.groups(),
        Settings {
            switch: std::time::Duration::from_secs(2),
            ..Default::default()
        },
        Arc::new(SystemClock),
    );
    let request = meridian_domain::v1::MovePartitionRequest {
        partition: lower.clone(),
        target_group: 1,
        reason: "spread the dataset's halves over both writer groups".into(),
    };
    assert_eq!(mover.check(&request).unwrap(), 0);
    // The parent, split, is refused: each half moves on its own.
    let parent = mover
        .check(&meridian_domain::v1::MovePartitionRequest {
            partition: dataset.clone(),
            ..request.clone()
        })
        .unwrap_err();
    assert!(parent.on_the_bus().contains("partition"), "{parent:?}");
    assert!(mover.begin(&lower));
    let again = mover.check(&request).unwrap_err();
    assert!(again.on_the_bus().contains("partition"), "{again:?}");
    let asked = Asked {
        by_hand: true,
        moved_by: "https://directory.example.org|8812".into(),
        reason: request.reason.clone(),
    };
    let switched_at = std::time::Instant::now();
    let ended = mover.run(&lower, 1, &asked).unwrap();
    let switch_took = switched_at.elapsed();
    std::thread::sleep(std::time::Duration::from_millis(300));
    stop.store(true, Ordering::SeqCst);
    let (moved_rows, refused, moved_wait) = moving.join().unwrap();
    let (sibling_rows, sibling_refused, sibling_wait) = sibling.join().unwrap();
    assert!(
        refused.is_empty() && sibling_refused.is_empty(),
        "{refused:?} {sibling_refused:?}"
    );
    assert!(
        moved_rows > 5 && sibling_rows > 5,
        "{moved_rows} {sibling_rows}"
    );
    println!(
        "lake range move within one database: switch {switch_took:?}; the moved range's longest \
         wait {moved_wait:?}, its sibling's {sibling_wait:?}; {moved_rows} and {sibling_rows} rows"
    );
    let Ended::Moved(moved) = &ended else {
        panic!("{ended:?}");
    };
    assert_eq!((moved.from_group, moved.to_group), (0, 1));
    assert_eq!(moved.epoch, one.epoch(), "group 1's own epoch");
    mover.log(&ended);
    assert!(control_kinds(&url).contains(&"meridian.v1.PartitionMoved".to_string()));
    let tip = zero.range_tip(&lower).unwrap().unwrap();
    assert_eq!((tip.writer_group, tip.epoch), (Some(1), one.epoch()));
    // Unbroken: across the split and the switch, the target's epoch after.
    let chain = chain_of(&url, &dataset, &low);
    let switched = chain
        .iter()
        .position(|(_, _, epoch)| *epoch as u64 == moved.epoch)
        .expect("written by the target after the switch");
    assert!(
        switched > 1
            && chain[switched..]
                .iter()
                .all(|(p, _, e)| p == &lower && *e as u64 == moved.epoch)
    );
    assert!(chain_of(&url, &dataset, &high)
        .iter()
        .all(|(_, _, epoch)| *epoch as u64 == zero.epoch()));
    assert!(zero.scrub().unwrap().findings.is_empty());
    // The rows a read named before the switch, read after it from the
    // range's lineage: its head is past them.
    let heads = one.heads().unwrap();
    assert!(heads[&lower] > before && heads[&upper] > 1);
    let read = one
        .read(&Query {
            datasets: vec![dataset.clone()],
            data_type: DataType::Trade,
            subjects: vec![low.clone()],
            kinds: vec![],
            interval_ns: 0,
            when: When::Latest { at_ns: 0 },
            as_of_ns: 0,
            published_as_of_ns: 0,
            latest_only: false,
        })
        .unwrap();
    assert!(!read.is_empty());
    // Moved back: group 0's own epoch, written by its same writer.
    assert!(mover.begin(&lower));
    let back = mover
        .run(
            &lower,
            0,
            &Asked {
                reason: "back".into(),
                ..asked
            },
        )
        .unwrap();
    assert!(
        matches!(back, Ended::Moved(ref m) if m.epoch == zero.epoch()),
        "{back:?}"
    );
    zero.read_owners().unwrap();
    zero.record(&dataset, vec![trade(&dataset, "back", &low, 999)], 9_000)
        .expect("group 0 writes the range moved back, never fenced");
}

/// Every cause a range's move aborts for (section 6.8; MoveAbortCause): the
/// injected lag before anything is held, a switch that cannot finish
/// within moves.switchSeconds (a batch holds the range's row), and the
/// injected digest mismatch, with an IntegrityFinding. Each leaves the
/// source owning at its epoch, writing on with no gap, and its record in
/// the lake's control.
#[test]
fn a_lake_range_move_aborts_for_lag_timeout_and_digest_and_the_source_owns() {
    use meridian_domain::v1::MoveAbortCause;
    use meridian_lake::partition_moves::{Asked, Ended, Mover, Settings};
    let (url, zero, _one, dataset, lower, _upper, low, _high) =
        two_groups_and_a_split("lake-abort");
    let dir = std::env::temp_dir().join(unique("lake-abort"));
    std::fs::create_dir_all(&dir).unwrap();
    let (lag, digest) = (dir.join("lag"), dir.join("digest"));
    std::fs::write(&lag, "60000").unwrap();
    std::fs::write(&digest, "").unwrap();
    let mover = |lag_file: Option<std::path::PathBuf>, digest_file| {
        Mover::new(
            vec![zero.clone()],
            zero.groups(),
            Settings {
                switch: std::time::Duration::from_secs(1),
                lag_file,
                digest_file,
                ..Default::default()
            },
            Arc::new(SystemClock),
        )
    };
    let attempt = |mover: &Mover| {
        assert!(mover.begin(&lower));
        let ended = mover.run(&lower, 1, &Asked::default()).unwrap();
        mover.log(&ended);
        ended
    };
    let mut n = 0;
    let mut writes_on = || {
        n += 1;
        zero.record(
            &dataset,
            vec![trade(&dataset, &format!("a{n}"), &low, n)],
            5_000 + n,
        )
        .expect("the source writes on");
    };
    // Lag: the gate never opens.
    let ended = attempt(&mover(Some(lag.clone()), None));
    assert!(
        matches!(ended, Ended::Aborted(ref a, None) if a.cause == MoveAbortCause::Lag as i32),
        "{ended:?}"
    );
    writes_on();
    // Timeout: a batch holds the range's row past the bound.
    let mut holder = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let mut held = holder.transaction().unwrap();
    held.execute(
        "SELECT 1 FROM lake_partition WHERE dataset = $1 FOR UPDATE",
        &[&lower],
    )
    .unwrap();
    let ended = attempt(&mover(None, None));
    held.rollback().unwrap();
    assert!(
        matches!(ended, Ended::Aborted(ref a, None) if a.cause == MoveAbortCause::Timeout as i32),
        "{ended:?}"
    );
    writes_on();
    // Digest: the target's differs at N.
    let ended = attempt(&mover(None, Some(digest.clone())));
    let Ended::Aborted(aborted, Some(finding)) = &ended else {
        panic!("{ended:?}");
    };
    assert_eq!(aborted.cause, MoveAbortCause::Digest as i32);
    assert_eq!(
        (aborted.component.as_str(), aborted.target_group),
        ("lake", 1)
    );
    assert!(finding.detail.contains(&lower), "{finding:?}");
    writes_on();
    // The source owns at its epoch; no hole; each record logged.
    let tip = zero.range_tip(&lower).unwrap().unwrap();
    assert_eq!((tip.writer_group, tip.epoch), (Some(0), zero.epoch()));
    chain_of(&url, &dataset, &low);
    let kinds = control_kinds(&url);
    let count = |kind: &str| kinds.iter().filter(|k| *k == kind).count();
    assert_eq!(count("meridian.v1.PartitionMoveAborted"), 3);
    assert_eq!(count("meridian.v1.PartitionMoved"), 0);
    assert_eq!(count(meridian_changes::records::FINDING), 1);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// An aliased key restated after a merge, routed across two groups to its
/// anchor's range (step 3d; ruling 2 of 2026-11-14): a key first written
/// under a subject in the lower half, whose record then merged into one in
/// the upper half, is restated under the record that stays. Its range is
/// its anchor's -- the lower, moved to group 1 -- never its new subject's:
/// the router reads the anchor in one look and sends it to group 1, which
/// records it there as its next version; group 0 is refused it, and the
/// upper range holds nothing of it.
#[test]
fn an_aliased_key_restated_after_a_merge_is_routed_to_its_anchors_range() {
    use meridian_lake::partition_moves::{Asked, Ended, Mover, Settings};
    use meridian_lake::store::StoreError;
    let (url, zero, one, dataset, lower, upper, low, high) = two_groups_and_a_split("lake-alias");
    zero.record(&dataset, vec![trade(&dataset, "k", &low, 50)], 2_000)
        .unwrap();
    let mover = Mover::new(
        vec![zero.clone()],
        zero.groups(),
        Settings::default(),
        Arc::new(SystemClock),
    );
    assert!(mover.begin(&lower));
    let ended = mover.run(&lower, 1, &Asked::default()).unwrap();
    assert!(matches!(ended, Ended::Moved(_)), "{ended:?}");
    one.read_owners().unwrap();
    // The low subject's record merged into the high one's.
    zero.keep_alias(&low, &high, 2_500).unwrap();
    let mut restated = trade(&dataset, "k", &high, 50);
    restated.meta_mut().valid_until_ns += 1;
    let rows = vec![restated];
    // Its new subject's hash says group 0; its anchor says group 1.
    assert!(meridian_lake::range::hash(&high) >= 0x8000);
    for member in [&zero, &one] {
        assert_eq!(
            meridian_lake::service::owners_of_rows(
                member.groups(),
                member.as_ref(),
                &dataset,
                &rows
            ),
            vec![1],
            "routed to the anchor's range's group"
        );
    }
    assert!(matches!(
        zero.record(&dataset, rows.clone(), 3_000),
        Err(StoreError::NotOwned { group: 1, .. })
    ));
    let done = one.record(&dataset, rows, 3_000).unwrap();
    assert_eq!(done.restated, 1);
    assert_eq!(done.partition_of(&dataset, 0), lower);
    assert_eq!(done.rows[0].meta().version, 2);
    let mut admin = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let in_upper: i64 = admin
        .query_one(
            "SELECT count(*) FROM lake_row WHERE partition = $1 AND row_key = 'k'",
            &[&upper],
        )
        .unwrap()
        .get(0);
    assert_eq!(in_upper, 0, "nothing in the wrong range");
}

/// A group past the first leads only its own database (step 3d; section
/// 6.4): its takeover in database 0, where a range of its own is still
/// ahead of home, raises its own epoch there and never database 0's
/// control head, which group 0's writer holds. CI 43788516279 (ha, a
/// database added): group 1's new writer took over in database 1 and in
/// database 0 behind it; database 0's control head went to 22, and group
/// 0's writer, at 21, stepped down without its lease, which lapsed.
#[test]
fn t_a_later_groups_takeover_in_database_0_never_fences_group_0() {
    use meridian_changes::groups::Groups;
    let urls = two_databases();
    let stores = |group: u32| -> Vec<Arc<PostgresStore>> {
        let groups = Groups::new(group, 2).unwrap();
        urls.iter()
            .enumerate()
            .map(|(d, url)| {
                Arc::new(
                    PostgresStore::connect(url, 2)
                        .unwrap()
                        .with_groups(groups)
                        .with_databases(2)
                        .in_database(d as u32),
                )
            })
            .collect()
    };
    let (zeros, ones) = (stores(0), stores(1));
    let mine = zeros[0].adopt().unwrap();
    ones[1].adopt().unwrap();
    assert_eq!(
        zeros[0].head_epoch().unwrap(),
        mine,
        "group 0's writer reads its own epoch"
    );
    // Group 1's takeover where it writes, then in database 0 behind it.
    ones[1]
        .take_over(&meridian_changes::TookOver::unnamed("a test"))
        .unwrap();
    let theirs = ones[0]
        .take_over(&meridian_changes::TookOver::unnamed("a test"))
        .unwrap();
    assert!(
        theirs > mine,
        "group 1's takeover in database 0 raised its own epoch there"
    );
    assert_eq!(
        zeros[0].head_epoch().unwrap(),
        mine,
        "database 0's control head is group 0's: another group's takeover there never raises it"
    );
    assert_eq!(
        ones[0].head_epoch().unwrap(),
        theirs,
        "group 1 reads its own epoch in database 0"
    );
    assert_eq!(
        ones[1].head_epoch().unwrap(),
        ones[1].epoch(),
        "and leads its own database"
    );
}

/// A lake range moved to a writer group of another database under write
/// load (step 3d; sections 6.4 and 6.8): the target's database first
/// copies the range, its lineage's rows its anchors hold and its keys'
/// latest versions, then the rest up to the source's close under the
/// switch; the subject's chain reads whole in the target's database across
/// the split and the move; its sibling writes on in the source's; and a
/// read through either member answers each range from the database it is
/// written in, the moved range's latest from the target's alone.
#[test]
fn a_lake_range_moved_between_databases_is_copied_and_its_chain_reads_whole() {
    use meridian_changes::groups::Groups;
    use meridian_lake::partition_moves::{Asked, Ended, Mover, Settings};
    let urls = two_databases();
    let stores = |group: u32| -> Vec<Arc<PostgresStore>> {
        let groups = Groups::new(group, 2).unwrap();
        urls.iter()
            .enumerate()
            .map(|(d, url)| {
                Arc::new(
                    PostgresStore::connect(url, 4)
                        .unwrap()
                        .with_groups(groups)
                        .with_databases(2)
                        .in_database(d as u32),
                )
            })
            .collect()
    };
    let (zeros, ones) = (stores(0), stores(1));
    let zero = Arc::new(meridian_lake::spread::Spread::new(
        zeros.clone(),
        zeros[0].groups(),
    ));
    let one = Arc::new(meridian_lake::spread::Spread::new(
        ones.clone(),
        ones[0].groups(),
    ));
    zero.adopt().unwrap();
    one.adopt().unwrap();
    let dataset = (0..)
        .map(|n| unique(&format!("coinbase-1:x{n}")))
        .find(|dataset| meridian_changes::groups::owner(dataset, 2) == 0)
        .unwrap();
    let (low, high) = halves_of("lake-across");
    zero.record(
        &dataset,
        vec![
            trade(&dataset, "l0", &low, 10),
            trade(&dataset, "h0", &high, 11),
        ],
        1_000,
    )
    .unwrap();
    zero.split_partition(&dataset, "admin-1", "two halves", 1_100)
        .unwrap();
    zero.record(&dataset, vec![trade(&dataset, "split-l", &low, 12)], 1_200)
        .unwrap();
    zeros[0].pin_owners().unwrap();
    for store in zeros.iter().chain(&ones) {
        store.read_owners().unwrap();
    }
    let lower = format!("{dataset}/0000-7fff");
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let moving = writing_range(
        zero.clone() as Arc<dyn Store>,
        one.clone() as Arc<dyn Store>,
        dataset.clone(),
        low.clone(),
        "l",
        stop.clone(),
    );
    let sibling = writing_range(
        zero.clone() as Arc<dyn Store>,
        one.clone() as Arc<dyn Store>,
        dataset.clone(),
        high.clone(),
        "h",
        stop.clone(),
    );
    std::thread::sleep(std::time::Duration::from_millis(300));
    let mover = Mover::new(
        zeros.clone(),
        zeros[0].groups(),
        Settings {
            switch: std::time::Duration::from_secs(5),
            ..Default::default()
        },
        Arc::new(SystemClock),
    );
    assert!(mover.begin(&lower));
    let began = std::time::Instant::now();
    let ended = mover
        .run(
            &lower,
            1,
            &Asked {
                by_hand: true,
                moved_by: "https://directory.example.org|8812".into(),
                reason: "the lower half to database 1".into(),
            },
        )
        .unwrap();
    let took = began.elapsed();
    for store in ones.iter() {
        store.read_owners().unwrap();
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    stop.store(true, Ordering::SeqCst);
    let (moved_rows, refused, moved_wait) = moving.join().unwrap();
    let (sibling_rows, sibling_refused, sibling_wait) = sibling.join().unwrap();
    assert!(
        refused.is_empty() && sibling_refused.is_empty(),
        "{refused:?} {sibling_refused:?}"
    );
    println!(
        "lake range move across databases: warm and switch {took:?}; the moved range's longest \
         wait {moved_wait:?}, its sibling's {sibling_wait:?}; {moved_rows} and {sibling_rows} rows"
    );
    let Ended::Moved(moved) = &ended else {
        panic!("{ended:?}");
    };
    assert_eq!(
        moved.epoch,
        ones[1].epoch(),
        "group 1's own epoch in its database"
    );
    // The target's database reads the subject's chain whole: the parent's
    // rows before the split, the range's own, and the target's after.
    let chain = chain_of(&urls[1], &dataset, &low);
    assert!(
        chain.first().is_some_and(|(p, _, _)| p == &dataset),
        "{chain:?}"
    );
    assert!(chain.iter().any(|(_, _, e)| *e as u64 == moved.epoch));
    assert!(chain
        .last()
        .is_some_and(|(p, _, e)| p == &lower && *e as u64 == moved.epoch));
    // Its sibling stays and writes on in database 0.
    chain_of(&urls[0], &dataset, &high);
    // A read through either member: each range from its own database.
    for member in [&zero, &one] {
        let read = |subject: &str| {
            member
                .read(&Query {
                    datasets: vec![dataset.clone()],
                    data_type: DataType::Trade,
                    subjects: vec![subject.to_string()],
                    kinds: vec![],
                    interval_ns: 0,
                    when: When::Latest { at_ns: 0 },
                    as_of_ns: 0,
                    published_as_of_ns: 0,
                    latest_only: false,
                })
                .unwrap()
        };
        let lows = read(&low);
        let mut keys: Vec<&str> = lows.iter().map(|row| row.meta().row_key.as_str()).collect();
        let whole = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), whole, "each key once, from one database");
        assert_eq!(whole as u64, moved_rows + 2, "every key of the moved range");
        assert_eq!(read(&high).len() as u64, sibling_rows + 1);
    }
    // The moved range's head is the target's: past the source's close.
    assert!(one.heads().unwrap()[&lower] > zeros[0].heads().unwrap()[&lower]);
}

/// A read of a dataset, made through a member whose view of the writer
/// groups was read before one of its ranges moved to another group's
/// database and not since, answers the range from the database it is
/// written in now (step 3d; sections 5 and 6.4; the book's stale read of
/// merge-queue run 45104369464, the same shape here). A reader reads the
/// writer groups at its start and every few seconds; its `read_after` is
/// held to every database's heads, the target's among them, while the read
/// went by the view to the range's source, which holds it as it closed.
#[test]
fn a_read_follows_a_range_past_a_stale_view_of_its_owner() {
    use meridian_changes::groups::Groups;
    use meridian_lake::partition_moves::{Asked, Ended, Mover, Settings};
    let urls = two_databases();
    let stores = |group: u32| -> Vec<Arc<PostgresStore>> {
        let groups = Groups::new(group, 2).unwrap();
        urls.iter()
            .enumerate()
            .map(|(d, url)| {
                Arc::new(
                    PostgresStore::connect(url, 4)
                        .unwrap()
                        .with_groups(groups)
                        .with_databases(2)
                        .in_database(d as u32),
                )
            })
            .collect()
    };
    // A reader's stores are a member's of its own: its own view.
    let (zeros, ones, readers) = (stores(0), stores(1), stores(0));
    let member = |stores: &[Arc<PostgresStore>]| {
        meridian_lake::spread::Spread::new(stores.to_vec(), stores[0].groups())
    };
    let (zero, one, reader) = (member(&zeros), member(&ones), member(&readers));
    zero.adopt().unwrap();
    one.adopt().unwrap();
    let dataset = (0..)
        .map(|n| unique(&format!("coinbase-1:s{n}")))
        .find(|dataset| meridian_changes::groups::owner(dataset, 2) == 0)
        .unwrap();
    let (low, high) = halves_of("lake-stale");
    zero.record(
        &dataset,
        vec![
            trade(&dataset, "l0", &low, 10),
            trade(&dataset, "h0", &high, 11),
        ],
        1_000,
    )
    .unwrap();
    zero.split_partition(&dataset, "admin-1", "two halves", 1_100)
        .unwrap();
    zero.record(&dataset, vec![trade(&dataset, "l1", &low, 12)], 1_200)
        .unwrap();
    zeros[0].pin_owners().unwrap();
    for store in zeros.iter().chain(&ones) {
        store.read_owners().unwrap();
    }
    let lower = format!("{dataset}/0000-7fff");
    let keys = |member: &meridian_lake::spread::Spread| -> Vec<String> {
        let mut keys: Vec<String> = member
            .read(&Query {
                datasets: vec![dataset.clone()],
                data_type: DataType::Trade,
                subjects: vec![low.clone()],
                kinds: vec![],
                interval_ns: 0,
                when: When::Latest { at_ns: 0 },
                as_of_ns: 0,
                published_as_of_ns: 0,
                latest_only: false,
            })
            .unwrap()
            .iter()
            .map(|row| row.meta().row_key.clone())
            .collect();
        keys.sort();
        keys
    };
    // The reader's one look at the writer groups, and a read by it.
    reader.read_owners_again().unwrap();
    assert_eq!(keys(&reader), ["l0", "l1"]);
    assert_eq!(reader.of(&lower), 0);
    let mover = Mover::new(
        zeros.clone(),
        zeros[0].groups(),
        Settings {
            switch: std::time::Duration::from_secs(5),
            ..Default::default()
        },
        Arc::new(SystemClock),
    );
    assert!(mover.begin(&lower));
    let ended = mover.run(&lower, 1, &Asked::default()).unwrap();
    assert!(matches!(ended, Ended::Moved(..)), "{ended:?}");
    for store in ones.iter() {
        store.read_owners().unwrap();
    }
    one.record(&dataset, vec![trade(&dataset, "l2", &low, 13)], 1_300)
        .unwrap();
    // What the read names: the head its row was committed at, in database
    // 1; the reader's `read_after` passes on every database's heads.
    let written = one.heads().unwrap()[&lower];
    assert!(written > zeros[0].heads().unwrap()[&lower]);
    assert!(reader.heads().unwrap()[&lower] >= written);
    // Its view of the owner is not read again by any timer here.
    assert_eq!(
        keys(&reader),
        ["l0", "l1", "l2"],
        "a read after the row was acknowledged answered without it"
    );
    // For that read alone: a read writes no view of the owners (the review
    // of 2026-11-23, finding 3).
    assert_eq!(reader.of(&lower), 0);
}

/// Both writer groups' members over two databases, each with its own view
/// of the writer groups.
struct Members {
    zeros: Vec<Arc<PostgresStore>>,
    ones: Vec<Arc<PostgresStore>>,
    zero: meridian_lake::spread::Spread,
    one: meridian_lake::spread::Spread,
}

/// A member's stores of both databases, sharing `groups`.
fn stores_of(
    urls: &[Scratch],
    groups: meridian_changes::groups::Groups,
    pool: u32,
) -> Vec<Arc<PostgresStore>> {
    urls.iter()
        .enumerate()
        .map(|(d, url)| {
            Arc::new(
                PostgresStore::connect(url, pool)
                    .unwrap()
                    .with_groups(groups)
                    .with_databases(urls.len() as u32)
                    .in_database(d as u32),
            )
        })
        .collect()
}

fn members(urls: &[Scratch]) -> Members {
    use meridian_changes::groups::Groups;
    let count = urls.len() as u32;
    let zeros = stores_of(urls, Groups::new(0, count).unwrap(), 4);
    let ones = stores_of(urls, Groups::new(1, count).unwrap(), 4);
    let zero = meridian_lake::spread::Spread::new(zeros.clone(), zeros[0].groups());
    let one = meridian_lake::spread::Spread::new(ones.clone(), ones[0].groups());
    zero.adopt().unwrap();
    one.adopt().unwrap();
    Members {
        zeros,
        ones,
        zero,
        one,
    }
}

/// A dataset group 0 of two owns, named from `tag`.
fn owned_by_zero(tag: &str) -> String {
    owned_by_zero_of(tag, 2)
}

/// A dataset group 0 of `count` owns, named from `tag`.
fn owned_by_zero_of(tag: &str, count: u32) -> String {
    (0..)
        .map(|n| unique(&format!("{tag}{n}")))
        .find(|dataset| meridian_changes::groups::owner(dataset, count) == 0)
        .unwrap()
}

/// `dataset` written and split in two halves in database 0, `l0` and `l1`
/// in its lower: answers the lower range.
fn halved(m: &Members, dataset: &str, low: &str, high: &str) -> String {
    m.zero
        .record(
            dataset,
            vec![
                trade(dataset, "l0", low, 10),
                trade(dataset, "h0", high, 11),
            ],
            1_000,
        )
        .unwrap();
    m.zero
        .split_partition(dataset, "admin-1", "two halves", 1_100)
        .unwrap();
    m.zero
        .record(dataset, vec![trade(dataset, "l1", low, 12)], 1_200)
        .unwrap();
    m.zeros[0].pin_owners().unwrap();
    for store in m.zeros.iter().chain(&m.ones) {
        store.read_owners().unwrap();
    }
    format!("{dataset}/0000-7fff")
}

/// `range` moved from group 0 to group 1, so from database 0 to 1.
fn moved_to_one(m: &Members, range: &str) {
    use meridian_lake::partition_moves::{Asked, Ended, Mover, Settings};
    let mover = Mover::new(
        m.zeros.clone(),
        m.zeros[0].groups(),
        Settings {
            switch: std::time::Duration::from_secs(5),
            ..Default::default()
        },
        Arc::new(SystemClock),
    );
    assert!(mover.begin(range));
    let ended = mover.run(range, 1, &Asked::default()).unwrap();
    assert!(matches!(ended, Ended::Moved(..)), "{ended:?}");
    for store in m.ones.iter() {
        store.read_owners().unwrap();
    }
}

/// The keys a latest read of `dataset`'s `subject` answers through `member`.
fn latest_keys(
    member: &meridian_lake::spread::Spread,
    dataset: &str,
    subject: &str,
) -> meridian_lake::store::Result<Vec<String>> {
    let mut keys: Vec<String> = member
        .read(&Query {
            datasets: vec![dataset.to_string()],
            data_type: DataType::Trade,
            subjects: vec![subject.to_string()],
            kinds: vec![],
            interval_ns: 0,
            when: When::Latest { at_ns: 0 },
            as_of_ns: 0,
            published_as_of_ns: 0,
            latest_only: false,
        })?
        .iter()
        .map(|row| row.meta().row_key.clone())
        .collect();
    keys.sort();
    Ok(keys)
}

/// A read answered from a reader's replicas never writes the member's view
/// of the writer groups (the review of 2026-11-23, finding 3): the replicas
/// share that view with the member's own stores, and one behind a move
/// holds the ranges' rows as they were before it. Following a range there
/// by reading the owners again wrote the owner before the move into the
/// view every read and write of the member's goes by. A read is routed by
/// the rows it just read, for itself alone: the replica's answer is its own
/// state, and the view still names the owner the record does.
#[test]
fn a_read_from_a_replica_behind_a_move_never_writes_the_members_view_of_the_owner() {
    use meridian_changes::groups::Groups;
    // The record, and what its replicas hold: the state before the move.
    let (urls, behind) = (two_databases(), two_databases());
    let dataset = owned_by_zero("coinbase-1:b");
    let (low, high) = halves_of("lake-behind");
    let record = members(&urls);
    let lower = halved(&record, &dataset, &low, &high);
    halved(&members(&behind), &dataset, &low, &high);
    moved_to_one(&record, &lower);
    record
        .one
        .record(&dataset, vec![trade(&dataset, "l2", &low, 13)], 1_300)
        .unwrap();
    // A reader member: one view, its own stores' and its replicas'.
    let groups = Groups::new(0, 2).unwrap();
    let primary = meridian_lake::spread::Spread::new(stores_of(&urls, groups, 2), groups);
    let replica = meridian_lake::spread::Spread::new(stores_of(&behind, groups, 2), groups);
    primary.read_owners_again().unwrap();
    assert_eq!(groups.owner_by(&lower, &dataset), 1, "the view, read after");
    assert_eq!(
        latest_keys(&replica, &dataset, &low).unwrap(),
        ["l0", "l1"],
        "the replica's own state, the range as it was before the move"
    );
    assert_eq!(
        groups.owner_by(&lower, &dataset),
        1,
        "a read from a replica behind the move wrote the owner before it into the member's view"
    );
    // And the record's, where the range is written now.
    assert_eq!(
        latest_keys(&primary, &dataset, &low).unwrap(),
        ["l0", "l1", "l2"]
    );
}

/// A database that cannot be reached slows no read of a dataset written in
/// another, split or not, and a read of a dataset one of whose ranges it
/// may hold is refused UNAVAILABLE at once, never answered without the
/// range (the review of 2026-11-23, findings 4 and 7: every database's
/// rows were read before every read of a dataset, so one database's outage
/// held each for its pool's 30 s and then answered it by the view).
/// Unreachable here as a pool finds it: every connection held, the next
/// checkout waiting.
#[test]
fn a_database_out_of_reach_slows_no_read_of_a_dataset_written_in_another() {
    use meridian_changes::groups::Groups;
    use meridian_lake::store::StoreError;
    let urls = two_databases();
    let m = members(&urls);
    let (stays, moves) = (owned_by_zero("coinbase-1:r"), owned_by_zero("coinbase-1:m"));
    let (low, high) = halves_of("lake-reach");
    halved(&m, &stays, &low, &high);
    let gone = halved(&m, &moves, &low, &high);
    // A reader member: its view of the writer groups read once, before the
    // move.
    let groups = Groups::new(0, 2).unwrap();
    let stores = stores_of(&urls, groups, 2);
    let reader = meridian_lake::spread::Spread::new(stores.clone(), groups);
    reader.read_owners_again().unwrap();
    for dataset in [&stays, &moves] {
        assert_eq!(latest_keys(&reader, dataset, &low).unwrap(), ["l0", "l1"]);
    }
    // Another, which has read nothing yet: where each range is, it has not
    // found (the review of the book's fix, finding 3).
    let fresh_stores = stores_of(&urls, groups, 2);
    let fresh = meridian_lake::spread::Spread::new(fresh_stores.clone(), groups);
    moved_to_one(&m, &gone);
    // Database 1 out of this member's reach: both connections of its pool
    // taken, each waiting on a lock held here.
    let mut admin = postgres::Client::connect(&urls[1], postgres::NoTls).unwrap();
    admin
        .batch_execute("BEGIN; LOCK TABLE lake_partition IN ACCESS EXCLUSIVE MODE")
        .unwrap();
    let held: Vec<_> = [&stores[1], &stores[1], &fresh_stores[1], &fresh_stores[1]]
        .into_iter()
        .map(|store| {
            let (store, range) = (store.clone(), gone.clone());
            std::thread::spawn(move || drop(store.range_tip(&range)))
        })
        .collect();
    std::thread::sleep(std::time::Duration::from_millis(500));
    // With no read before it: its home, asked first, holds its ranges, and
    // database 1 is not asked at all.
    let started = std::time::Instant::now();
    let read = latest_keys(&fresh, &stays, &low);
    let took = started.elapsed();
    assert_eq!(read.ok(), Some(vec!["l0".to_string(), "l1".to_string()]));
    assert!(
        took < std::time::Duration::from_secs(1),
        "a member's first read of a dataset database 0 writes waited {took:?} on database 1"
    );
    let started = std::time::Instant::now();
    let read = latest_keys(&reader, &stays, &low);
    let took = started.elapsed();
    assert_eq!(read.ok(), Some(vec!["l0".to_string(), "l1".to_string()]));
    assert!(
        took < std::time::Duration::from_secs(1),
        "a read of a dataset database 0 writes waited {took:?} on database 1"
    );
    // Its range's row in database 0 a move's pointer, and the database it
    // may have gone to not read: refused, within the bound.
    let started = std::time::Instant::now();
    let refused = latest_keys(&reader, &moves, &low);
    let took = started.elapsed();
    assert!(
        matches!(refused, Err(StoreError::Unavailable(_))),
        "{refused:?}"
    );
    assert!(
        took < std::time::Duration::from_secs(3),
        "refused only after {took:?}"
    );
    admin.batch_execute("ROLLBACK").unwrap();
    for holder in held {
        holder.join().unwrap();
    }
    // In reach again: read where it is written, the view as it was.
    assert_eq!(latest_keys(&reader, &moves, &low).unwrap(), ["l0", "l1"]);
    assert_eq!(groups.owner_by(&gone, &moves), 0);
}

/// A dataset whose ranges sit in two groups and two databases (step 3d;
/// sections 6.4 and 6.8): its counts are each database's of the ranges
/// read there, summed -- never one database's overwriting the other's, and
/// a range a move copied counted once; retention removes each range's rows
/// by its own writer, in its own database, each range's kept-from its own,
/// and the copies a move left with them; and rows served unkept are
/// numbered in their own range by its own group, a request with another
/// group's rows refused whole to be routed again.
#[test]
fn a_datasets_ranges_in_two_databases_keep_their_own_retention_counts_and_unkept_rows() {
    use meridian_changes::groups::Groups;
    use meridian_lake::partition_moves::{Asked, Ended, Mover, Settings};
    let urls = two_databases();
    let stores = |group: u32| -> Vec<Arc<PostgresStore>> {
        let groups = Groups::new(group, 2).unwrap();
        urls.iter()
            .enumerate()
            .map(|(d, url)| {
                Arc::new(
                    PostgresStore::connect(url, 4)
                        .unwrap()
                        .with_groups(groups)
                        .with_databases(2)
                        .in_database(d as u32),
                )
            })
            .collect()
    };
    let (zeros, ones) = (stores(0), stores(1));
    let zero = meridian_lake::spread::Spread::new(zeros.clone(), zeros[0].groups());
    let one = meridian_lake::spread::Spread::new(ones.clone(), ones[0].groups());
    zero.adopt().unwrap();
    one.adopt().unwrap();
    let dataset = (0..)
        .map(|n| unique(&format!("coinbase-1:r{n}")))
        .find(|dataset| meridian_changes::groups::owner(dataset, 2) == 0)
        .unwrap();
    let (low, high) = halves_of("lake-retain");
    // Old rows: two before the split (the dataset's own range), one after
    // in the lower half; then the lower half moved to group 1's database.
    zero.record(
        &dataset,
        vec![
            trade(&dataset, "l0", &low, 10),
            trade(&dataset, "h0", &high, 11),
        ],
        1_000,
    )
    .unwrap();
    zero.split_partition(&dataset, "admin-1", "two halves", 1_100)
        .unwrap();
    zero.record(&dataset, vec![trade(&dataset, "l1", &low, 12)], 1_200)
        .unwrap();
    zeros[0].pin_owners().unwrap();
    for store in zeros.iter().chain(&ones) {
        store.read_owners().unwrap();
    }
    let (lower, upper) = (
        format!("{dataset}/0000-7fff"),
        format!("{dataset}/8000-ffff"),
    );
    let mover = Mover::new(
        zeros.clone(),
        zeros[0].groups(),
        Settings::default(),
        Arc::new(SystemClock),
    );
    assert!(mover.begin(&lower));
    let ended = mover.run(&lower, 1, &Asked::default()).unwrap();
    assert!(matches!(ended, Ended::Moved(_)), "{ended:?}");
    for store in zeros.iter().chain(&ones) {
        store.read_owners().unwrap();
    }
    // New rows, each half by its own writer.
    let l2 = one
        .record(&dataset, vec![trade(&dataset, "l2", &low, 13)], 5_000)
        .unwrap();
    assert_eq!(l2.partition_of(&dataset, 0), lower);
    let h1 = zero
        .record(&dataset, vec![trade(&dataset, "h1", &high, 14)], 5_000)
        .unwrap();
    assert_eq!(h1.partition_of(&dataset, 0), upper);
    // Counted once each, wherever read: l0 and h0 in the dataset's own
    // range (database 0), l1 and l2 in the lower half (database 1), h1 in
    // the upper (database 0). Database 1's count of its range alone would
    // have stood for the whole dataset before.
    for member in [&zero, &one] {
        assert_eq!(member.counts().unwrap()[&dataset].0, 5);
    }
    // Retention: rows recorded before 2,000 removed, each group its own.
    let raw = |d: usize, sql: &str| -> i64 {
        postgres::Client::connect(&urls[d], postgres::NoTls)
            .unwrap()
            .query_one(sql, &[&dataset])
            .unwrap()
            .get(0)
    };
    let old = "SELECT count(*) FROM lake_row WHERE dataset = $1 AND recorded_at_ns < 2000";
    assert!(raw(0, old) > 0 && raw(1, old) > 0);
    let why = "retention under the dataset's licence: a test";
    // Group 0: in database 0, the dataset's own range's l0 and h0, and the
    // copy of l1 the move left there.
    assert_eq!(zero.remove_before(&dataset, 2_000, why, 6_000).unwrap(), 3);
    assert_eq!(raw(0, old), 0);
    assert!(raw(1, old) > 0, "database 1's are group 1's to remove");
    // Group 1: in database 1, its l1 and the lineage's copy of l0.
    assert_eq!(one.remove_before(&dataset, 2_000, why, 6_000).unwrap(), 2);
    assert_eq!(raw(1, old), 0);
    for member in [&zero, &one] {
        assert_eq!(member.counts().unwrap()[&dataset].0, 2);
    }
    // Each range kept from its own first row kept, in its own database.
    let first = |done: &meridian_lake::store::Recorded| done.rows[0].meta().sequence;
    assert_eq!(zero.lowest_kept(&lower).unwrap(), Some(first(&l2)));
    assert_eq!(one.lowest_kept(&upper).unwrap(), Some(first(&h1)));
    let kept_from = |d: usize, partition: &str| -> i64 {
        postgres::Client::connect(&urls[d], postgres::NoTls)
            .unwrap()
            .query_one(
                "SELECT kept_from FROM lake_partition WHERE dataset = $1",
                &[&partition],
            )
            .unwrap()
            .get(0)
    };
    assert!(kept_from(1, &lower) > 1, "the lower half's, in database 1");
    assert!(
        kept_from(0, &dataset) > 2,
        "the dataset's own, in database 0"
    );
    // Rows served unkept: numbered in their own range by its own group.
    let heads = one.heads().unwrap();
    let mixed = vec![
        trade(&dataset, "u-l", &low, 20),
        trade(&dataset, "u-h", &high, 21),
    ];
    let refused = zero
        .serve_unkept(&dataset, mixed, &["reader".into()], None, 7_000)
        .unwrap_err();
    assert!(
        matches!(
            refused,
            meridian_lake::store::StoreError::NotOwned { group: 1, .. }
        ),
        "{refused:?}"
    );
    let served_high = zero
        .serve_unkept(
            &dataset,
            vec![trade(&dataset, "u-h", &high, 21)],
            &["reader".into()],
            None,
            7_000,
        )
        .unwrap();
    assert_eq!(served_high.partition_of(&dataset, 0), upper);
    let served_low = one
        .serve_unkept(
            &dataset,
            vec![trade(&dataset, "u-l", &low, 20)],
            &["reader".into()],
            None,
            7_000,
        )
        .unwrap();
    assert_eq!(served_low.partition_of(&dataset, 0), lower);
    let now = one.heads().unwrap();
    assert_eq!(now[&lower], heads[&lower] + 1);
    assert_eq!(now[&upper], heads[&upper] + 1);
    for member in [&zero, &one] {
        assert_eq!(member.counts().unwrap()[&dataset].0, 2, "nothing kept");
    }
}

/// A range split by a writer group past the first (step 3d; section 6.1;
/// CI 43615034712): its PartitionSplit is logged in the lake's control,
/// which group 0 writes, under the splitting group's own epoch -- never
/// refused as though group 1's writer had been fenced by group 0's.
#[test]
fn a_range_of_group_ones_is_split_by_its_own_writer() {
    use meridian_changes::groups::Groups;
    let (url, migrated) = isolated("lake-split-one");
    drop(migrated);
    let member = |group: u32| {
        Arc::new(
            PostgresStore::connect(&url, 4)
                .unwrap()
                .with_groups(Groups::new(group, 2).unwrap()),
        )
    };
    let (zero, one) = (member(0), member(1));
    zero.adopt().unwrap();
    one.adopt().unwrap();
    let dataset = (0..)
        .map(|n| unique(&format!("coinbase-1:g1s{n}")))
        .find(|dataset| meridian_changes::groups::owner(dataset, 2) == 1)
        .unwrap();
    let (low, high) = halves_of("lake-split-one");
    one.record(
        &dataset,
        vec![
            trade(&dataset, "l0", &low, 10),
            trade(&dataset, "h0", &high, 11),
        ],
        1_000,
    )
    .unwrap();
    let split = one
        .split_partition(&dataset, "admin-1", "two halves", 1_100)
        .unwrap();
    assert_eq!(split.children.len(), 2);
    assert!(zero
        .splits()
        .unwrap()
        .iter()
        .any(|kept| kept.parent == dataset));
}

/// A range a deployment admin moved by hand stays where she put it (step
/// 3d; section 6.8, ruled 2026-11-14): the sweep, every minute, moves a
/// range only by a changed count, so it never takes hers back to its
/// dataset's group.
#[test]
fn a_range_moved_by_hand_stays_through_the_sweep() {
    use meridian_lake::partition_moves::{Asked, Ended, Mover, Settings};
    let (_url, zero, one, dataset, lower, _upper, _low, _high) =
        two_groups_and_a_split("lake-by-hand");
    assert_eq!(meridian_changes::groups::owner(&dataset, 2), 0);
    let mover = Mover::new(
        vec![zero.clone()],
        zero.groups(),
        Settings::default(),
        Arc::new(SystemClock),
    );
    assert!(mover.begin(&lower));
    let ended = mover
        .run(
            &lower,
            1,
            &Asked {
                by_hand: true,
                moved_by: "https://directory.example.org|8812".into(),
                reason: "group 0 is busy".into(),
            },
        )
        .unwrap();
    assert!(matches!(&ended, Ended::Moved(m) if m.by_hand), "{ended:?}");
    mover.log(&ended);
    assert!(mover.rebalance().is_empty(), "the sweep leaves hers");
    one.read_owners().unwrap();
    assert_eq!(
        zero.range_tip(&lower).unwrap().unwrap().writer_group,
        Some(1)
    );
}

/// A writer group past the first leading its own database, taken over in
/// ha, follows that database's timeline under its own epoch: never found
/// another writer's on the control head its takeover does not raise (step
/// 3d; CI 43631481777, ha at two databases: group 1's members stepped down
/// in turn, "the store's epoch is 1, another writer's").
#[test]
fn a_group_leading_its_own_database_follows_its_timeline_under_its_own_epoch() {
    use meridian_changes::groups::Groups;
    use meridian_changes::timeline::Timeline;
    let (url, migrated) = isolated("lake-leads");
    drop(migrated);
    let leader = PostgresStore::connect(&url, 2)
        .unwrap()
        .with_groups(Groups::new(1, 2).unwrap())
        .with_databases(2);
    let epoch = leader
        .take_over(&meridian_changes::TookOver {
            member: "lake-group-1-b".into(),
            previous: "lake-group-1-a".into(),
            why: "the writer's lease lapsed".into(),
            at_ns: 1,
        })
        .unwrap();
    assert!(epoch > 1, "{epoch}");
    for _ in 0..2 {
        let found = leader.follow_timeline().unwrap();
        assert!(
            !matches!(found, Timeline::NotMine(_)),
            "the leader is not deposed by its database's control head: {found:?}"
        );
    }
}

/// A lake of `store`'s writer group as it runs in ha, `dataset` kept and
/// its trades' instruments and currency resolved.
fn ha_lake(
    store: &Arc<PostgresStore>,
    instance: &str,
    dataset: &str,
) -> Arc<meridian_lake::service::Lake> {
    ha_lake_over(
        store.clone() as Arc<dyn Store>,
        store.groups(),
        instance,
        dataset,
    )
}

/// [`ha_lake`], over any store of writer group `groups`.
fn ha_lake_over(
    store: Arc<dyn Store>,
    groups: meridian_changes::groups::Groups,
    instance: &str,
    dataset: &str,
) -> Arc<meridian_lake::service::Lake> {
    use meridian_domain::v1::{
        DatasetEntitlement, DatasetRef, EntitlementsChangedEvent, InstrumentRecord,
        ResolveIdentifierReply, ResolveInstrumentReply,
    };
    use meridian_lake::service;
    use meridian_pb::v1::{DatasetDeclaration, ObservationMode};
    use prost::Message;
    let dataset = dataset.to_string();
    let bus = Arc::new(meridian_bus::Bus::single(
        instance,
        Arc::new(meridian_bus::MemoryBackend::new()),
        Arc::new(SystemClock),
    ));
    bus.serve(service::RESOLVE_INSTRUMENT, |envelope| {
        let asked =
            meridian_domain::v1::ResolveInstrumentRequest::decode(&envelope.payload[..]).unwrap();
        Ok((
            "meridian.v1.ResolveInstrumentReply".into(),
            ResolveInstrumentReply {
                found: true,
                instrument: Some(InstrumentRecord {
                    instrument_id: asked.instrument_id,
                    ..Default::default()
                }),
            }
            .encode_to_vec(),
        ))
    });
    // USD, the trades' currency, by its code.
    bus.serve(service::RESOLVE_IDENTIFIER, |_| {
        Ok((
            "meridian.v1.ResolveIdentifierReply".into(),
            ResolveIdentifierReply {
                found: true,
                instrument_id: "LCL-USD".into(),
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    bus.serve(service::PLUGIN_CATALOGUE, |_| {
        Ok((
            "meridian.v1.PluginCatalogue".into(),
            meridian_domain::v1::PluginCatalogue {
                launches: vec![meridian_domain::v1::PluginLaunch {
                    instance_id: "coinbase-1".into(),
                    version: "0.3.1".into(),
                    state: meridian_domain::v1::PluginLaunchState::Launched as i32,
                    ..Default::default()
                }],
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    let lake = meridian_lake::service::serve_grouped(bus, store, Arc::new(SystemClock), groups);
    let (instance, key) = dataset.split_once(':').unwrap();
    lake.configured(&EntitlementsChangedEvent {
        datasets: vec![DatasetRef {
            dataset: dataset.clone(),
            instance: instance.into(),
            vendor: "Coinbase".into(),
            declaration: Some(DatasetDeclaration {
                key: key.into(),
                vendor: "Coinbase".into(),
                data_types: vec!["meridian.v1.Trade".into()],
                modes: vec![ObservationMode::Pull as i32],
                history: 3650,
                day_time_zone: "Etc/UTC".into(),
                ..Default::default()
            }),
            ..Default::default()
        }],
        licences: vec![],
        entitlements: vec![DatasetEntitlement {
            dataset: dataset.clone(),
            instance: "reporting-1".into(),
            allowed: true,
            ..Default::default()
        }],
        changed_at_ns: SystemClock.now_ns(),
    });
    lake
}

/// A lake range moved between writer groups in ha, under load (step 3d;
/// spec 6.1 and 6.8; ruled 2026-11-14 after CI 43621875619): two groups'
/// writers take one stream at their own paces, the target ahead of the
/// source, while the range moves from the source to the target. At the
/// switch the target takes the range's rows from the source's committed
/// position; the source, its view stale, never refuses a write the target
/// commits, its answer waiting for the target's commit. Every write
/// answered, none refused; every row committed once, none missed. The
/// siblings' longest pause while the switch held a batch is said.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_range_moved_between_groups_in_ha_under_load_commits_each_row_once_and_refuses_none() {
    use meridian_domain::v1::{RecordObservationsReply, RecordTradesRequest};
    use meridian_lake::partition_moves::{Asked, Ended, Mover, Settings};
    use meridian_lake::service;
    use meridian_lake::write_ahead::{settled, taken_past_a_switch, Taken, Writer};
    use prost::Message;
    const STREAM: &str = "LAKE_WRITE_AHEAD";
    const WRITES: u64 = 240;
    // The dataset and both its halves group 1's, which answers its writes,
    // as the driver's dataset in CI 43621875619.
    let (url, zero, one, dataset, _lower, upper, low, high) =
        tokio::task::block_in_place(|| two_groups_and_a_split_of("lake-ham", 1));
    let (zero, one) = (OffRuntime::new(zero), OffRuntime::new(one));
    let lake_of = |store: &Arc<PostgresStore>, instance: &str| ha_lake(store, instance, &dataset);
    let (lake0, lake1) =
        tokio::task::block_in_place(|| (lake_of(&zero, "lake-0"), lake_of(&one, "lake-1")));
    let (lake0, lake1) = (OffRuntime::new(lake0), OffRuntime::new(lake1));
    // The stream: each write a row in each half, each row its own key.
    let stream: Vec<Taken> = (1..=WRITES)
        .map(|n| {
            let row = |half: &str, subject: &str| match trade(
                &dataset,
                &format!("w{n}-{half}"),
                subject,
                1_000 + n as i64,
            ) {
                Observation::Trade(trade) => *trade,
                _ => unreachable!(),
            };
            Taken {
                sequence: n,
                at_ns: 1_000 + n as i64,
                topic: service::RECORD_TRADES.into(),
                reply_to: Some(format!("_INBOX.ham.{n}")),
                after: None,
                envelope: meridian_bus::Envelope {
                    meta: Some(meridian_bus::MessageMeta {
                        publisher_instance_id: "coinbase-1".into(),
                        account_scope_applies: true,
                        published_at_ns: SystemClock.now_ns(),
                        message_id: format!("ham-{n}"),
                        ..Default::default()
                    }),
                    payload_type: "meridian.v1.RecordTradesRequest".into(),
                    payload: RecordTradesRequest {
                        trades: vec![row("l", &low), row("h", &high)],
                        want_id: String::new(),
                    }
                    .encode_to_vec(),
                },
                dataset: dataset.clone(),
                timeout_ms: None,
            }
        })
        .collect();
    let mut writers = OffRuntime::new([
        tokio::task::block_in_place(|| Writer::start(lake0.clone(), STREAM, 0)).unwrap(),
        tokio::task::block_in_place(|| Writer::start(lake1.clone(), STREAM, 0)).unwrap(),
    ]);
    let lakes = OffRuntime::new([lake0.clone(), lake1.clone()]);
    // Group 0, the target, takes four writes a step; group 1, the source,
    // one: the target is ahead when the range moves.
    let paces = [4usize, 1];
    let mut settling = Vec::new();
    let mut refused: Vec<String> = Vec::new();
    let mut taken_back = 0;
    let mut owed = 0;
    let mut longest = [std::time::Duration::ZERO; 2];
    let mut moving: Option<tokio::task::JoinHandle<Ended>> = None;
    let mut moving_since = None;
    let mut step = 0;
    while writers.iter().any(|writer| writer.next() <= WRITES) {
        step += 1;
        if step == 20 {
            // The upper half moved to group 0 under load: group 1's view of
            // it stale, as its member's was in CI.
            let (zero, moved) = (zero.clone(), upper.clone());
            moving_since = Some(std::time::Instant::now());
            moving = Some(tokio::task::spawn_blocking(move || {
                let mover = Mover::new(
                    vec![zero.clone()],
                    zero.groups(),
                    Settings::default(),
                    Arc::new(SystemClock),
                );
                assert!(mover.begin(&moved));
                let began = std::time::Instant::now();
                let ended = mover
                    .run(
                        &moved,
                        0,
                        &Asked {
                            by_hand: true,
                            moved_by: "admin-1".into(),
                            reason: "moved under load in ha".into(),
                        },
                    )
                    .unwrap();
                eprintln!("ham: the move took {:?}", began.elapsed());
                ended
            }));
        }
        for (g, writer) in writers.iter_mut().enumerate() {
            let from = writer.next();
            if from > WRITES {
                continue;
            }
            let batch: Vec<Taken> = stream
                .iter()
                .filter(|write| write.sequence >= from)
                .take(paces[g])
                .cloned()
                .collect();
            // A take begun while the move is under way: its time is the
            // pause the switch gave every dataset of the group.
            let in_flight = moving.as_ref().is_some_and(|m| !m.is_finished());
            let began = std::time::Instant::now();
            let took = taken_past_a_switch(writer, batch).await;
            if in_flight {
                longest[g] = longest[g].max(began.elapsed());
            }
            match took {
                Ok(answers) => {
                    for (to, answer) in &answers {
                        if let (Some(to), Err(words)) = (to, answer) {
                            refused.push(format!("{to}: {words}"));
                        }
                    }
                    let waits = writer.take_waits();
                    owed += waits
                        .iter()
                        .filter(|wait| g == 1 && wait.groups.contains(&0))
                        .count();
                    let lake = lakes[g].clone();
                    settling.push(tokio::spawn(async move {
                        settled(&lake, STREAM, answers, waits).await
                    }));
                }
                Err(meridian_lake::store::StoreError::PositionMoved(_)) => {
                    taken_back += 1;
                    tokio::task::block_in_place(|| writer.reread()).unwrap();
                }
                Err(failed) => panic!("group {g}'s batch from {from}: {failed}"),
            }
        }
    }
    let ended = moving.expect("moved").await.unwrap();
    assert!(matches!(ended, Ended::Moved(_)), "{ended:?}");
    eprintln!(
        "ham: switch began {:?} before the end; longest take, group 0 {:?}, group 1 {:?}; \
         positions taken back {taken_back}; answers waiting for group 0's part {owed}",
        moving_since.map(|at| at.elapsed()),
        longest[0],
        longest[1]
    );
    // Each answer counts both its write's rows, whichever group committed
    // each and whichever take (spec 6.7; CI 43662032539: an answer counted
    // only its own group's).
    let mut miscounted: Vec<String> = Vec::new();
    let mut answers = 0;
    for answered in settling {
        for (to, answer) in answered.await.unwrap() {
            match (to, answer) {
                (Some(to), Err(words)) => refused.push(format!("{to}: {words}")),
                (Some(to), Ok((_, payload))) => {
                    answers += 1;
                    let reply = RecordObservationsReply::decode(payload.as_slice()).unwrap();
                    let counted = reply.recorded + reply.restated + reply.unchanged;
                    if counted != 2 {
                        miscounted.push(format!(
                            "{to}: recorded {} restated {} unchanged {}",
                            reply.recorded, reply.restated, reply.unchanged
                        ));
                    }
                }
                _ => {}
            }
        }
    }
    assert!(refused.is_empty(), "nothing refused: {refused:?}");
    assert!(answers as u64 >= WRITES, "every write answered: {answers}");
    assert!(
        miscounted.is_empty(),
        "each answer counts both its rows: {} of {answers} did not: {:?}",
        miscounted.len(),
        &miscounted[..miscounted.len().min(12)]
    );
    assert!(taken_back >= 1, "the target's position was taken back");
    assert!(
        owed >= 1,
        "group 1 answered writes whose moved part group 0 committed"
    );
    // Every row once: each key held by one row of the record, none missed.
    tokio::task::block_in_place(|| {
        let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
        let rows = client
            .query(
                "SELECT row_key, count(*) FROM lake_row WHERE dataset = $1 GROUP BY row_key",
                &[&dataset],
            )
            .unwrap();
        let twice: Vec<(String, i64)> = rows
            .iter()
            .map(|row| (row.get::<_, String>(0), row.get::<_, i64>(1)))
            .filter(|(_, count)| *count > 1)
            .collect();
        assert!(twice.is_empty(), "committed twice: {twice:?}");
        let held: std::collections::BTreeSet<String> =
            rows.iter().map(|row| row.get::<_, String>(0)).collect();
        let missed: Vec<String> = (1..=WRITES)
            .flat_map(|n| [format!("w{n}-l"), format!("w{n}-h")])
            .filter(|key| !held.contains(key))
            .collect();
        assert!(missed.is_empty(), "missed: {missed:?}");
    });
}

/// A write is answered a success only when its answer accounts for every
/// row it sent (ruled 2026-11-15; step 3d; spec 6.7; "lake loss is re-sent,
/// never silent"), in ha, two writer groups taking one stream. Counted are
/// the rows of datasets the lake keeps, whichever group committed each.
///
/// - A write on its first dataset's subject holding a row of another
///   dataset, whose group has no range of the first: that group passes the
///   write, and the row is committed by nobody. Answered OVERLOADED with a
///   retry-after (it was a success counting one row of two).
/// - A write whose rows every group commits: a success counting both.
/// - A part no group took, each taking it for the other's: OVERLOADED, and
///   sent again it is a success counting both.
/// - A row a group that does not answer the write refuses by the lake's
///   rule 5: the write is answered refused by name, as for the answering
///   group's own part (it was a success counting the rest), its other row
///   committed all the same.
/// - A write of a dataset served and not kept: a success, held to no count.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_is_a_success_only_when_its_answer_accounts_for_every_row_it_sent() {
    use meridian_bus::HandlerReply;
    use meridian_domain::v1::{
        DatasetEntitlement, DatasetRef, EntitlementsChangedEvent, RecordObservationsReply,
        RecordTradesRequest,
    };
    use meridian_lake::partition_moves::{Asked, Ended, Mover, Settings};
    use meridian_lake::service;
    use meridian_lake::store::StoreError;
    use meridian_lake::write_ahead::{settled, taken_past_a_switch, Taken, Writer};
    use meridian_pb::v1::{DatasetDeclaration, DatasetLicence, ObservationMode};
    use prost::Message;
    const STREAM: &str = "LAKE_WRITE_AHEAD";
    // Group 1's dataset, split, both halves its own; another kept dataset
    // of group 0's; and one served and not kept.
    let (url, zero, one, dataset, _lower, upper, low, high) =
        tokio::task::block_in_place(|| two_groups_and_a_split_of("lake-rule", 1));
    let (zero, one) = (OffRuntime::new(zero), OffRuntime::new(one));
    let named = |tag: &str, owner: u32| {
        (0..)
            .map(|n| unique(&format!("coinbase-1:{tag}{n}")))
            .find(|dataset| meridian_changes::groups::owner(dataset, 2) == owner)
            .unwrap()
    };
    let (other, served) = (named("oth", 0), named("srv", 1));
    let (lake0, lake1) = tokio::task::block_in_place(|| {
        (
            ha_lake(&zero, "lake-0", &dataset),
            ha_lake(&one, "lake-1", &dataset),
        )
    });
    let (lake0, lake1) = (OffRuntime::new(lake0), OffRuntime::new(lake1));
    let configured = EntitlementsChangedEvent {
        datasets: [&dataset, &other, &served]
            .iter()
            .map(|dataset| {
                let (instance, key) = dataset.split_once(':').unwrap();
                DatasetRef {
                    dataset: dataset.to_string(),
                    instance: instance.into(),
                    vendor: "Coinbase".into(),
                    declaration: Some(DatasetDeclaration {
                        key: key.into(),
                        vendor: "Coinbase".into(),
                        data_types: vec!["meridian.v1.Trade".into()],
                        modes: vec![ObservationMode::Pull as i32],
                        history: 3650,
                        day_time_zone: "Etc/UTC".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }
            })
            .collect(),
        licences: vec![DatasetLicence {
            dataset: served.clone(),
            kept: false,
            ..Default::default()
        }],
        entitlements: [&dataset, &other, &served]
            .iter()
            .map(|dataset| DatasetEntitlement {
                dataset: dataset.to_string(),
                instance: "reporting-1".into(),
                allowed: true,
                ..Default::default()
            })
            .collect(),
        changed_at_ns: SystemClock.now_ns(),
    };
    tokio::task::block_in_place(|| {
        lake0.configured(&configured);
        lake1.configured(&configured);
    });
    let lakes = OffRuntime::new([lake0.clone(), lake1.clone()]);
    let mut writers = OffRuntime::new([
        tokio::task::block_in_place(|| Writer::start(lake0.clone(), STREAM, 0)).unwrap(),
        tokio::task::block_in_place(|| Writer::start(lake1.clone(), STREAM, 0)).unwrap(),
    ]);
    let mut stream: Vec<Taken> = Vec::new();
    let row = |dataset: &str, key: &str, subject: &str, valid: i64| match trade(
        dataset, key, subject, valid,
    ) {
        Observation::Trade(trade) => *trade,
        _ => unreachable!(),
    };
    // A write on `subject`'s dataset's subject, taken by both groups, a
    // write a batch, and answered once both have: its answer.
    macro_rules! ask {
        ($subject:expr, $rows:expr) => {{
            let n = stream.len() as u64 + 1;
            stream.push(Taken {
                sequence: n,
                at_ns: 1_000 + n as i64,
                topic: service::RECORD_TRADES.into(),
                reply_to: Some(format!("_INBOX.rule.{n}")),
                after: None,
                envelope: meridian_bus::Envelope {
                    meta: Some(meridian_bus::MessageMeta {
                        publisher_instance_id: "coinbase-1".into(),
                        account_scope_applies: true,
                        published_at_ns: SystemClock.now_ns(),
                        message_id: format!("rule-{n}"),
                        ..Default::default()
                    }),
                    payload_type: "meridian.v1.RecordTradesRequest".into(),
                    payload: RecordTradesRequest {
                        trades: $rows,
                        want_id: String::new(),
                    }
                    .encode_to_vec(),
                },
                dataset: $subject.to_string(),
                timeout_ms: None,
            });
            let mut settling = Vec::new();
            for (g, writer) in writers.iter_mut().enumerate() {
                while writer.next() <= n {
                    let from = writer.next();
                    let batch: Vec<Taken> = stream
                        .iter()
                        .filter(|write| write.sequence == from)
                        .cloned()
                        .collect();
                    match taken_past_a_switch(writer, batch).await {
                        Ok(answers) => settling.push((g, answers, writer.take_waits())),
                        Err(StoreError::PositionMoved(_)) => {
                            tokio::task::block_in_place(|| writer.reread()).unwrap();
                        }
                        Err(failed) => panic!("group {g}'s batch from {from}: {failed}"),
                    }
                }
            }
            let mut answer: Option<HandlerReply> = None;
            for (g, answers, waits) in settling {
                for (to, said) in settled(&lakes[g], STREAM, answers, waits).await {
                    if to.as_deref() == Some(format!("_INBOX.rule.{n}").as_str()) {
                        answer = Some(said);
                    }
                }
            }
            answer.expect("answered")
        }};
    }
    let counts = |answer: HandlerReply| {
        let (_, payload) = answer.expect("a success");
        let reply = RecordObservationsReply::decode(payload.as_slice()).unwrap();
        (reply.recorded, reply.restated, reply.unchanged)
    };
    let overloaded = |answer: HandlerReply, what: &str| {
        let refused = answer.expect_err(what);
        let reason = format!(
            "refusal-reason {}",
            meridian_pb::v1::RefusalReason::Overloaded as i32
        );
        assert!(
            refused.contains(&reason) && refused.contains("retry="),
            "{what}: OVERLOADED with a retry-after: {refused}"
        );
    };
    let held = |key: &str| -> i64 {
        tokio::task::block_in_place(|| {
            postgres::Client::connect(&url, postgres::NoTls)
                .unwrap()
                .query_one("SELECT count(*) FROM lake_row WHERE row_key = $1", &[&key])
                .unwrap()
                .get(0)
        })
    };

    // A row of another dataset on this one's subject, its group holding no
    // range of this one: committed by nobody, so never a success.
    let answer = ask!(
        dataset,
        vec![row(&dataset, "m-d", &low, 10), row(&other, "m-o", &low, 10)]
    );
    assert_eq!(held("m-o"), 0, "the other dataset's row is nobody's");
    overloaded(answer, "a row committed by nobody");

    // The upper half to group 0: the dataset's ranges span both groups.
    tokio::task::block_in_place(|| {
        let mover = Mover::new(
            vec![zero.clone()],
            zero.groups(),
            Settings::default(),
            Arc::new(SystemClock),
        );
        assert!(mover.begin(&upper));
        let ended = mover
            .run(
                &upper,
                0,
                &Asked {
                    by_hand: true,
                    moved_by: "admin-1".into(),
                    reason: "its ranges across both groups".into(),
                },
            )
            .unwrap();
        assert!(matches!(ended, Ended::Moved(_)), "{ended:?}");
        zero.read_owners().unwrap();
        one.read_owners().unwrap();
    });

    // Each group commits its own row: a success counting both.
    let both = |tag: &str| {
        vec![
            row(&dataset, &format!("{tag}-l"), &low, 20),
            row(&dataset, &format!("{tag}-h"), &high, 20),
        ]
    };
    assert_eq!(counts(ask!(dataset, both("b"))), (2, 0, 0));

    // A part no group takes: group 0's member takes its half for group
    // 1's, group 1's for group 0's.
    zero.groups().assign(&upper, Some(1));
    let answer = ask!(dataset, both("s"));
    assert_eq!((held("s-l"), held("s-h")), (1, 0));
    overloaded(answer, "a part no group took");
    // Sent again, its route read: a success counting both.
    tokio::task::block_in_place(|| zero.read_owners()).unwrap();
    assert_eq!(counts(ask!(dataset, both("s"))), (1, 0, 1));

    // A row the group that does not answer refuses by rule 5: the write is
    // answered refused, naming it; its other row committed all the same.
    assert_eq!(
        counts(ask!(dataset, vec![row(&dataset, "k", &high, 30)])),
        (1, 0, 0)
    );
    let refused = ask!(
        dataset,
        vec![
            row(&dataset, "r-l", &low, 40),
            row(&dataset, "k", &high, 41)
        ]
    )
    .expect_err("a restated key's valid time: refused");
    assert!(
        refused.contains("trades[1].meta.valid_from_ns")
            && refused.contains("fixes its valid time"),
        "refused by name: {refused}"
    );
    assert_eq!(held("r-l"), 1, "its other row committed");

    // A dataset served and not kept: a success, its row served.
    assert_eq!(
        counts(ask!(served, vec![row(&served, "u", &low, 50)])),
        (1, 0, 0)
    );
}

/// A lake range moved to the group that answers its dataset, across
/// databases in ha, under writes of one row each into the moved half, its
/// sibling half that group's already -- and each member's store the hot
/// layer in front of the record, as the lake's binary builds it (step 3d;
/// spec 6.1, 6.7 and 6.8; PR CI 43684440785, ha: three prices answered
/// recorded 0, restated 0, unchanged 0 in the two seconds after the
/// driver's half moved to group 1 in database 1, "waited_on=[0]
/// own_part=false"). The switch takes the target's position back so that
/// its writer reads the ranges' owners again and takes the range's rows as
/// its own; behind the hot layer that read asked nobody, the target kept
/// taking the range for its source's until its sweep, the source had let
/// it go, and the rows were committed by neither: lost, not only
/// uncounted. Every write is answered counting its row, and every row is
/// held once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_range_moved_to_its_datasets_answering_group_behind_the_hot_layer_loses_no_row() {
    use meridian_domain::v1::{RecordObservationsReply, RecordTradesRequest};
    use meridian_lake::hot::{Bounds, HotStore};
    use meridian_lake::partition_moves::{Asked, Ended};
    use meridian_lake::service;
    use meridian_lake::write_ahead::{settled, taken_past_a_switch, Taken, Writer};
    use prost::Message;
    const STREAM: &str = "LAKE_WRITE_AHEAD";
    const WRITES: u64 = 120;
    // Group 1's dataset, both halves at its home (database 1).
    let a = OffRuntime::new(tokio::task::block_in_place(|| {
        across_owned("ham3", true, 1)
    }));
    let dataset = a.dataset.clone();
    let upper = format!("{dataset}/8000-ffff");
    let (_, high) = halves_of("ham3");
    let mover = OffRuntime::new(Arc::new(a.mover()));
    let asked = || Asked {
        by_hand: true,
        moved_by: "admin-1".into(),
        reason: "the upper half, out and home again".into(),
    };
    // The upper half to group 0 in database 0 first, nothing writing: the
    // shape CI moved from. Every member reads it, as its sweep does.
    tokio::task::block_in_place(|| {
        assert!(mover.begin(&upper));
        let ended = mover.run(&upper, 0, &asked()).unwrap();
        assert!(matches!(ended, Ended::Moved(_)), "{ended:?}");
        for store in a.zeros.iter().chain(&a.ones) {
            store.read_owners().unwrap();
        }
    });
    let behind_hot = |spread: &Arc<meridian_lake::spread::Spread>,
                      groups: meridian_changes::groups::Groups,
                      instance: &str| {
        let hot = HotStore::new(spread.clone() as Arc<dyn Store>, Bounds::DEFAULT);
        hot.for_group(groups);
        ha_lake_over(hot as Arc<dyn Store>, groups, instance, &dataset)
    };
    let (lake0, lake1) = tokio::task::block_in_place(|| {
        (
            behind_hot(&a.zero, a.zeros[0].groups(), "lake-0"),
            behind_hot(&a.one, a.ones[0].groups(), "lake-1"),
        )
    });
    let (lake0, lake1) = (OffRuntime::new(lake0), OffRuntime::new(lake1));
    let stream: Vec<Taken> = (1..=WRITES)
        .map(|n| {
            let row = match trade(&dataset, &format!("y{n}"), &high, 1_000 + n as i64) {
                Observation::Trade(trade) => *trade,
                _ => unreachable!(),
            };
            Taken {
                sequence: n,
                at_ns: 1_000 + n as i64,
                topic: service::RECORD_TRADES.into(),
                reply_to: Some(format!("_INBOX.ham3.{n}")),
                after: None,
                envelope: meridian_bus::Envelope {
                    meta: Some(meridian_bus::MessageMeta {
                        publisher_instance_id: "coinbase-1".into(),
                        account_scope_applies: true,
                        published_at_ns: SystemClock.now_ns(),
                        message_id: format!("ham3-{n}"),
                        ..Default::default()
                    }),
                    payload_type: "meridian.v1.RecordTradesRequest".into(),
                    payload: RecordTradesRequest {
                        trades: vec![row],
                        want_id: String::new(),
                    }
                    .encode_to_vec(),
                },
                dataset: dataset.clone(),
                timeout_ms: None,
            }
        })
        .collect();
    let mut writers = OffRuntime::new([
        tokio::task::block_in_place(|| Writer::start(lake0.clone(), STREAM, 0)).unwrap(),
        tokio::task::block_in_place(|| Writer::start(lake1.clone(), STREAM, 0)).unwrap(),
    ]);
    let lakes = OffRuntime::new([lake0.clone(), lake1.clone()]);
    let mut settling = Vec::new();
    let mut refused: Vec<String> = Vec::new();
    let mut taken_back = 0;
    let mut moving: Option<tokio::task::JoinHandle<Ended>> = None;
    let mut step = 0;
    while writers.iter().any(|writer| writer.next() <= WRITES) {
        step += 1;
        if step == 20 {
            // Home to group 1 in database 1, under the writes.
            let (mover, moved, asked) = (mover.clone(), upper.clone(), asked());
            moving = Some(tokio::task::spawn_blocking(move || {
                assert!(mover.begin(&moved));
                mover.run(&moved, 1, &asked).unwrap()
            }));
        }
        for (g, writer) in writers.iter_mut().enumerate() {
            let from = writer.next();
            if from > WRITES {
                continue;
            }
            // One write a batch, as the driver's half second apart.
            let batch: Vec<Taken> = stream
                .iter()
                .filter(|write| write.sequence >= from)
                .take(1)
                .cloned()
                .collect();
            match taken_past_a_switch(writer, batch).await {
                Ok(answers) => {
                    let waits = writer.take_waits();
                    let lake = lakes[g].clone();
                    settling.push(tokio::spawn(async move {
                        settled(&lake, STREAM, answers, waits).await
                    }));
                }
                Err(meridian_lake::store::StoreError::PositionMoved(_)) => {
                    taken_back += 1;
                    tokio::task::block_in_place(|| writer.reread()).unwrap();
                }
                Err(failed) => panic!("group {g}'s batch from {from}: {failed}"),
            }
        }
    }
    let ended = moving.expect("moved").await.unwrap();
    assert!(matches!(ended, Ended::Moved(_)), "{ended:?}");
    let mut uncounted: Vec<String> = Vec::new();
    let mut answers = 0;
    for answered in settling {
        for (to, answer) in answered.await.unwrap() {
            match (to, answer) {
                (Some(to), Err(words)) => refused.push(format!("{to}: {words}")),
                (Some(to), Ok((_, payload))) => {
                    answers += 1;
                    let reply = RecordObservationsReply::decode(payload.as_slice()).unwrap();
                    if reply.recorded + reply.restated + reply.unchanged != 1 {
                        uncounted.push(format!(
                            "{to}: recorded {} restated {} unchanged {}",
                            reply.recorded, reply.restated, reply.unchanged
                        ));
                    }
                }
                _ => {}
            }
        }
    }
    // Lost, or only uncounted: each key as the record holds it, in either
    // database (a moved range's rows stay where it was, beside its
    // pointer), and once in each.
    let (missed, twice) = tokio::task::block_in_place(|| {
        let mut held = std::collections::BTreeSet::new();
        let mut twice: Vec<String> = Vec::new();
        for (d, url) in a.urls.iter().enumerate() {
            let mut client = postgres::Client::connect(url, postgres::NoTls).unwrap();
            for row in client
                .query(
                    "SELECT row_key, count(*) FROM lake_row WHERE dataset = $1 GROUP BY row_key",
                    &[&dataset],
                )
                .unwrap()
            {
                let key: String = row.get(0);
                if row.get::<_, i64>(1) > 1 {
                    twice.push(format!("db{d} {key}"));
                }
                held.insert(key);
            }
        }
        let missed: Vec<String> = (1..=WRITES)
            .map(|n| format!("y{n}"))
            .filter(|key| !held.contains(key))
            .collect();
        (missed, twice)
    });
    eprintln!(
        "ham3: positions taken back {taken_back}; answers {answers}; uncounted {}; missed {}",
        uncounted.len(),
        missed.len()
    );
    // What each group kept of a miscounted write, and where the range was
    // taken from, said with the failure.
    let kept: Vec<String> = tokio::task::block_in_place(|| {
        let mut kept = Vec::new();
        for (d, url) in a.urls.iter().enumerate() {
            let mut client = postgres::Client::connect(url, postgres::NoTls).unwrap();
            for said in &uncounted {
                let sequence: i64 = said
                    .split([':', '.'])
                    .nth(2)
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                for row in client
                    .query(
                        "SELECT stream, recorded, restated, unchanged FROM lake_write_ahead_part
                          WHERE sequence = $1",
                        &[&sequence],
                    )
                    .unwrap()
                {
                    kept.push(format!(
                        "db{d} {} {sequence}: {} {} {}",
                        row.get::<_, String>(0),
                        row.get::<_, i32>(1),
                        row.get::<_, i32>(2),
                        row.get::<_, i32>(3)
                    ));
                }
            }
            if !uncounted.is_empty() {
                for row in client
                    .query(
                        "SELECT writer_group, taken_from, head FROM lake_partition WHERE dataset = $1",
                        &[&upper],
                    )
                    .unwrap()
                {
                    kept.push(format!(
                        "db{d} range: group {:?} taken_from {:?} head {}",
                        row.get::<_, Option<i32>>(0),
                        row.get::<_, Option<i64>>(1),
                        row.get::<_, i64>(2)
                    ));
                }
            }
        }
        kept
    });
    assert!(missed.is_empty(), "rows no group committed: {missed:?}");
    assert!(refused.is_empty(), "nothing refused: {refused:?}");
    assert!(taken_back >= 1, "the target's position was taken back");
    assert!(answers as u64 >= WRITES, "every write answered: {answers}");
    assert!(
        uncounted.is_empty(),
        "each answer counts its row: {} of {answers} did not: {:?}; kept {kept:?}",
        uncounted.len(),
        &uncounted[..uncounted.len().min(12)]
    );
    assert!(twice.is_empty(), "held twice: {twice:?}");
}

/// A lake range moved between writer groups across databases in ha, under
/// load, there and back (step 3d; spec 6.4, 6.7 and 6.8; CI 43662032539's
/// ha rebalance): two groups' writers, each over both databases, take one
/// stream while the lower half moves from group 0, which answers, in
/// database 0 to group 1 in database 1, and later home again. Every write
/// is answered, none refused, and every answer counts both its rows --
/// the part each group committed, whichever take committed it -- so none
/// is acknowledged with a row of it uncounted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_range_moved_across_databases_in_ha_under_load_is_answered_counting_every_part() {
    use meridian_domain::v1::{RecordObservationsReply, RecordTradesRequest};
    use meridian_lake::partition_moves::{Asked, Ended};
    use meridian_lake::service;
    use meridian_lake::write_ahead::{settled, taken_past_a_switch, Taken, Writer};
    use prost::Message;
    const STREAM: &str = "LAKE_WRITE_AHEAD";
    const WRITES: u64 = 360;
    let a = OffRuntime::new(tokio::task::block_in_place(|| across("ham2", true)));
    let (dataset, low, lower) = (a.dataset.clone(), a.low.clone(), a.lower());
    let (_, high) = halves_of("ham2");
    let (lake0, lake1) = tokio::task::block_in_place(|| {
        (
            ha_lake_over(
                a.zero.clone() as Arc<dyn Store>,
                a.zeros[0].groups(),
                "lake-0",
                &dataset,
            ),
            ha_lake_over(
                a.one.clone() as Arc<dyn Store>,
                a.ones[0].groups(),
                "lake-1",
                &dataset,
            ),
        )
    });
    let (lake0, lake1) = (OffRuntime::new(lake0), OffRuntime::new(lake1));
    let stream: Vec<Taken> = (1..=WRITES)
        .map(|n| {
            let row = |half: &str, subject: &str| match trade(
                &dataset,
                &format!("x{n}-{half}"),
                subject,
                1_000 + n as i64,
            ) {
                Observation::Trade(trade) => *trade,
                _ => unreachable!(),
            };
            Taken {
                sequence: n,
                at_ns: 1_000 + n as i64,
                topic: service::RECORD_TRADES.into(),
                reply_to: Some(format!("_INBOX.ham2.{n}")),
                after: None,
                envelope: meridian_bus::Envelope {
                    meta: Some(meridian_bus::MessageMeta {
                        publisher_instance_id: "coinbase-1".into(),
                        account_scope_applies: true,
                        published_at_ns: SystemClock.now_ns(),
                        message_id: format!("ham2-{n}"),
                        ..Default::default()
                    }),
                    payload_type: "meridian.v1.RecordTradesRequest".into(),
                    payload: RecordTradesRequest {
                        trades: vec![row("l", &low), row("h", &high)],
                        want_id: String::new(),
                    }
                    .encode_to_vec(),
                },
                dataset: dataset.clone(),
                timeout_ms: None,
            }
        })
        .collect();
    let mut writers = OffRuntime::new([
        tokio::task::block_in_place(|| Writer::start(lake0.clone(), STREAM, 0)).unwrap(),
        tokio::task::block_in_place(|| Writer::start(lake1.clone(), STREAM, 0)).unwrap(),
    ]);
    let lakes = OffRuntime::new([lake0.clone(), lake1.clone()]);
    let mover = OffRuntime::new(Arc::new(a.mover()));
    let mut settling = Vec::new();
    let mut refused: Vec<String> = Vec::new();
    let mut taken_back = 0;
    let mut moves: Vec<tokio::task::JoinHandle<Ended>> = Vec::new();
    let mut step = 0;
    while writers.iter().any(|writer| writer.next() <= WRITES) {
        step += 1;
        // Out at step 20, its target ahead of its source; home at step 60,
        // once the first has ended, its target ahead again.
        let target = match step {
            20 => Some(1),
            60 => Some(0),
            _ => None,
        };
        if let Some(target) = target {
            if let Some(first) = moves.pop() {
                let ended = first.await.unwrap();
                assert!(matches!(ended, Ended::Moved(_)), "{ended:?}");
            }
            let (mover, moved) = (mover.clone(), lower.clone());
            moves.push(tokio::task::spawn_blocking(move || {
                assert!(mover.begin(&moved));
                mover
                    .run(
                        &moved,
                        target,
                        &Asked {
                            by_hand: true,
                            moved_by: "admin-1".into(),
                            reason: "moved across databases under load in ha".into(),
                        },
                    )
                    .unwrap()
            }));
        }
        let paces = if step < 60 { [1usize, 4] } else { [4, 1] };
        for (g, writer) in writers.iter_mut().enumerate() {
            let from = writer.next();
            if from > WRITES {
                continue;
            }
            let batch: Vec<Taken> = stream
                .iter()
                .filter(|write| write.sequence >= from)
                .take(paces[g])
                .cloned()
                .collect();
            match taken_past_a_switch(writer, batch).await {
                Ok(answers) => {
                    let waits = writer.take_waits();
                    let lake = lakes[g].clone();
                    settling.push(tokio::spawn(async move {
                        settled(&lake, STREAM, answers, waits).await
                    }));
                }
                Err(meridian_lake::store::StoreError::PositionMoved(_)) => {
                    taken_back += 1;
                    tokio::task::block_in_place(|| writer.reread()).unwrap();
                }
                Err(failed) => panic!("group {g}'s batch from {from}: {failed}"),
            }
        }
    }
    for ended in moves {
        let ended = ended.await.unwrap();
        assert!(matches!(ended, Ended::Moved(_)), "{ended:?}");
    }
    let mut miscounted: Vec<String> = Vec::new();
    let mut answers = 0;
    for answered in settling {
        for (to, answer) in answered.await.unwrap() {
            match (to, answer) {
                (Some(to), Err(words)) => refused.push(format!("{to}: {words}")),
                (Some(to), Ok((_, payload))) => {
                    answers += 1;
                    let reply = RecordObservationsReply::decode(payload.as_slice()).unwrap();
                    if reply.recorded + reply.restated + reply.unchanged != 2 {
                        miscounted.push(format!(
                            "{to}: recorded {} restated {} unchanged {}",
                            reply.recorded, reply.restated, reply.unchanged
                        ));
                    }
                }
                _ => {}
            }
        }
    }
    eprintln!("ham2: positions taken back {taken_back}; answers {answers}");
    // What each group kept of a miscounted write, said with the failure.
    let kept: Vec<String> = tokio::task::block_in_place(|| {
        let mut kept = Vec::new();
        for (d, url) in a.urls.iter().enumerate() {
            let mut client = postgres::Client::connect(url, postgres::NoTls).unwrap();
            for said in &miscounted {
                let sequence: i64 = said
                    .split([':', '.'])
                    .nth(2)
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                for row in client
                    .query(
                        "SELECT stream, recorded, restated, unchanged FROM lake_write_ahead_part
                          WHERE sequence = $1",
                        &[&sequence],
                    )
                    .unwrap()
                {
                    kept.push(format!(
                        "db{d} {} {sequence}: {} {} {}",
                        row.get::<_, String>(0),
                        row.get::<_, i32>(1),
                        row.get::<_, i32>(2),
                        row.get::<_, i32>(3)
                    ));
                }
            }
        }
        kept
    });
    assert!(refused.is_empty(), "nothing refused: {refused:?}");
    assert!(taken_back >= 2, "each move took its target's position back");
    assert!(answers as u64 >= WRITES, "every write answered: {answers}");
    assert!(
        miscounted.is_empty(),
        "each answer counts both its rows: {} of {answers} did not: {:?}; kept {kept:?}",
        miscounted.len(),
        &miscounted[..miscounted.len().min(12)]
    );
    // None missed: each key held in one database or the other.
    tokio::task::block_in_place(|| {
        let mut held = std::collections::BTreeSet::new();
        for url in a.urls.iter() {
            let mut client = postgres::Client::connect(url, postgres::NoTls).unwrap();
            for row in client
                .query(
                    "SELECT DISTINCT row_key FROM lake_row WHERE dataset = $1",
                    &[&dataset],
                )
                .unwrap()
            {
                held.insert(row.get::<_, String>(0));
            }
        }
        let missed: Vec<String> = (1..=WRITES)
            .flat_map(|n| [format!("x{n}-l"), format!("x{n}-h")])
            .filter(|key| !held.contains(key))
            .collect();
        assert!(missed.is_empty(), "missed: {missed:?}");
    });
}

/// A write split across two writer groups, in ha, is answered counting
/// every group's part (step 3d; spec 6.7; `lake_write_ahead_part`, ruled
/// 2026-11-15 after CI 43656183991 and 43662032539): the group answering
/// adds what the other group's rows did, as that group kept it with its
/// rows and its position. A write none of whose rows are the answering
/// group's -- the failover driver's price, once its range had moved -- is
/// answered with its row counted and its range named in the watermark,
/// never OK with nothing counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_split_across_groups_is_answered_counting_every_groups_part() {
    use meridian_domain::v1::{RecordObservationsReply, RecordTradesRequest};
    use meridian_lake::partition_moves::{Asked, Mover, Settings};
    use meridian_lake::service;
    use meridian_lake::write_ahead::{settled, Taken, Writer};
    use prost::Message;
    const STREAM: &str = "LAKE_WRITE_AHEAD";
    // The dataset group 1's, which answers its writes; its upper half moved
    // to group 0.
    let (_url, zero, one, dataset, _lower, upper, low, high) =
        tokio::task::block_in_place(|| two_groups_and_a_split_of("lake-parts", 1));
    let (zero, one) = (OffRuntime::new(zero), OffRuntime::new(one));
    tokio::task::block_in_place(|| {
        let mover = Mover::new(
            vec![one.clone()],
            one.groups(),
            Settings::default(),
            Arc::new(SystemClock),
        );
        assert!(mover.begin(&upper));
        mover.run(&upper, 0, &Asked::default()).unwrap();
        zero.read_owners().unwrap();
        one.read_owners().unwrap();
    });
    let (lake0, lake1) = tokio::task::block_in_place(|| {
        (
            ha_lake(&zero, "lake-0", &dataset),
            ha_lake(&one, "lake-1", &dataset),
        )
    });
    let (lake0, lake1) = (OffRuntime::new(lake0), OffRuntime::new(lake1));
    let write = |n: u64, rows: Vec<(&str, &String)>| Taken {
        sequence: n,
        at_ns: 1_000 + n as i64,
        topic: service::RECORD_TRADES.into(),
        reply_to: Some(format!("_INBOX.parts.{n}")),
        after: None,
        envelope: meridian_bus::Envelope {
            meta: Some(meridian_bus::MessageMeta {
                publisher_instance_id: "coinbase-1".into(),
                account_scope_applies: true,
                published_at_ns: SystemClock.now_ns(),
                message_id: format!("parts-{n}"),
                ..Default::default()
            }),
            payload_type: "meridian.v1.RecordTradesRequest".into(),
            payload: RecordTradesRequest {
                trades: rows
                    .into_iter()
                    .map(|(key, subject)| {
                        match trade(&dataset, &format!("p{n}-{key}"), subject, 2_000 + n as i64) {
                            Observation::Trade(trade) => *trade,
                            _ => unreachable!(),
                        }
                    })
                    .collect(),
                want_id: String::new(),
            }
            .encode_to_vec(),
        },
        dataset: dataset.clone(),
        timeout_ms: None,
    };
    // A row in each half; then one in the upper half alone, group 0's.
    let stream = vec![
        write(1, vec![("l", &low), ("h", &high)]),
        write(2, vec![("h", &high)]),
    ];
    let mut writer0 = OffRuntime::new(
        tokio::task::block_in_place(|| Writer::start(lake0.clone(), STREAM, 0)).unwrap(),
    );
    let mut writer1 = OffRuntime::new(
        tokio::task::block_in_place(|| Writer::start(lake1.clone(), STREAM, 0)).unwrap(),
    );
    // Group 1, answering, takes both first; group 0 commits its parts after.
    let answers = tokio::task::block_in_place(|| writer1.take(stream.clone())).unwrap();
    let waits = writer1.take_waits();
    assert_eq!(waits.len(), 2, "both answers wait for group 0's part");
    let theirs = tokio::task::block_in_place(|| writer0.take(stream.clone())).unwrap();
    assert!(
        theirs.iter().all(|(to, _)| to.is_none()),
        "group 0 answers neither"
    );
    let answered = settled(&lake1, STREAM, answers, waits).await;
    let reply = |at: usize| {
        let (_, payload) = answered[at].1.clone().expect("answered");
        RecordObservationsReply::decode(payload.as_slice()).unwrap()
    };
    let (both, theirs_alone) = (reply(0), reply(1));
    assert_eq!(
        (both.recorded, both.restated, both.unchanged),
        (2, 0, 0),
        "a row in each group's half: both counted"
    );
    assert_eq!(
        (
            theirs_alone.recorded,
            theirs_alone.restated,
            theirs_alone.unchanged
        ),
        (1, 0, 0),
        "a row in group 0's half alone: counted by group 1's answer"
    );
    let named: Vec<String> = theirs_alone
        .watermark
        .unwrap_or_default()
        .partitions
        .into_iter()
        .filter(|at| at.sequence > 0)
        .map(|at| at.partition)
        .collect();
    assert!(
        named.contains(&upper),
        "its range named at its head: {named:?}"
    );
}

/// A row sent again unchanged in a write just after its range's switch is
/// answered unchanged, never with nothing counted (step 3d; spec 6.7 and
/// 6.8; `lake_partition.taken_from`, ruled 2026-11-15). The switch takes
/// its target's position back, and the target takes the stream again from
/// there: the range's rows the source committed it finds unchanged, as it
/// finds a row a plugin sent again. The range keeps the stream sequence
/// its source had committed through at the switch, so the target leaves
/// uncounted the first -- the source counted them -- and counts the second,
/// which nobody has. Before, the target could only bound the source's
/// place by the furthest any group had committed when it noticed, and a
/// row sent again in a write between the two was answered 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_row_sent_again_unchanged_just_after_a_switch_is_answered_unchanged() {
    use meridian_domain::v1::{RecordObservationsReply, RecordTradesRequest};
    use meridian_lake::partition_moves::{Asked, Ended, Mover, Settings};
    use meridian_lake::service;
    use meridian_lake::write_ahead::{settled, taken_past_a_switch, Taken, Writer};
    use prost::Message;
    const STREAM: &str = "LAKE_WRITE_AHEAD";
    // The dataset and both its halves group 1's, which answers its writes.
    let (url, zero, one, dataset, _lower, upper, low, high) =
        tokio::task::block_in_place(|| two_groups_and_a_split_of("lake-taken", 1));
    let (zero, one) = (OffRuntime::new(zero), OffRuntime::new(one));
    let (lake0, lake1) = tokio::task::block_in_place(|| {
        (
            ha_lake(&zero, "lake-0", &dataset),
            ha_lake(&one, "lake-1", &dataset),
        )
    });
    let (lake0, lake1) = (OffRuntime::new(lake0), OffRuntime::new(lake1));
    let sent_at = SystemClock.now_ns();
    let write = |n: u64, key: &str, subject: &String| Taken {
        sequence: n,
        at_ns: 1_000 + n as i64,
        topic: service::RECORD_TRADES.into(),
        reply_to: Some(format!("_INBOX.taken.{n}")),
        after: None,
        envelope: meridian_bus::Envelope {
            meta: Some(meridian_bus::MessageMeta {
                publisher_instance_id: "coinbase-1".into(),
                account_scope_applies: true,
                published_at_ns: sent_at,
                message_id: format!("taken-{n}"),
                ..Default::default()
            }),
            payload_type: "meridian.v1.RecordTradesRequest".into(),
            payload: RecordTradesRequest {
                trades: vec![match trade(&dataset, key, subject, 5_000) {
                    Observation::Trade(trade) => *trade,
                    _ => unreachable!(),
                }],
                want_id: String::new(),
            }
            .encode_to_vec(),
        },
        dataset: dataset.clone(),
        timeout_ms: None,
    };
    // A row in the upper half; the same row sent again; one in the lower.
    let stream = [
        write(1, "again", &high),
        write(2, "again", &high),
        write(3, "other", &low),
    ];
    let mut writer0 = OffRuntime::new(
        tokio::task::block_in_place(|| Writer::start(lake0.clone(), STREAM, 0)).unwrap(),
    );
    let mut writer1 = OffRuntime::new(
        tokio::task::block_in_place(|| Writer::start(lake1.clone(), STREAM, 0)).unwrap(),
    );
    // Both groups take the first write: group 1 commits and counts its row.
    let first = taken_past_a_switch(&mut writer1, vec![stream[0].clone()])
        .await
        .unwrap();
    let waits = writer1.take_waits();
    taken_past_a_switch(&mut writer0, vec![stream[0].clone()])
        .await
        .unwrap();
    let first = settled(&lake1, STREAM, first, waits).await;
    let count = |answered: &(Option<String>, meridian_bus::HandlerReply)| {
        let (_, payload) = answered.1.clone().expect("answered");
        let reply = RecordObservationsReply::decode(payload.as_slice()).unwrap();
        (reply.recorded, reply.restated, reply.unchanged)
    };
    assert_eq!(count(&first[0]), (1, 0, 0), "recorded by group 1");
    // The upper half moved to group 0, group 1 having committed through 1.
    let ended = tokio::task::block_in_place(|| {
        let mover = Mover::new(
            vec![zero.clone()],
            zero.groups(),
            Settings::default(),
            Arc::new(SystemClock),
        );
        assert!(mover.begin(&upper));
        mover.run(&upper, 0, &Asked::default()).unwrap()
    });
    assert!(matches!(ended, Ended::Moved(_)), "{ended:?}");
    let taken_from: Option<i64> = tokio::task::block_in_place(|| {
        postgres::Client::connect(&url, postgres::NoTls)
            .unwrap()
            .query_one(
                "SELECT taken_from FROM lake_partition WHERE dataset = $1",
                &[&upper],
            )
            .unwrap()
            .get(0)
    });
    assert_eq!(taken_from, Some(1), "where its source stood at the switch");
    // Group 1, the source, goes on past the row sent again before group 0,
    // the target, has noticed its position was taken back.
    let rest = vec![stream[1].clone(), stream[2].clone()];
    let answers = taken_past_a_switch(&mut writer1, rest.clone())
        .await
        .unwrap();
    let waits = writer1.take_waits();
    let took = taken_past_a_switch(&mut writer0, rest.clone()).await;
    assert!(
        matches!(
            took,
            Err(meridian_lake::store::StoreError::PositionMoved(_))
        ),
        "its position was taken back"
    );
    tokio::task::block_in_place(|| writer0.reread()).unwrap();
    // It takes the stream again from there: the first write's row, which
    // group 1 counted, and the row sent again, which nobody has.
    while writer0.next() <= 3 {
        let from = writer0.next();
        let batch: Vec<Taken> = stream
            .iter()
            .filter(|write| write.sequence >= from)
            .cloned()
            .collect();
        taken_past_a_switch(&mut writer0, batch).await.unwrap();
    }
    let answered = settled(&lake1, STREAM, answers, waits).await;
    assert_eq!(
        count(&answered[0]),
        (0, 0, 1),
        "the row sent again: unchanged, counted once"
    );
    assert_eq!(count(&answered[1]), (1, 0, 0), "the lower half's row");
    // The first write's part, as group 0 keeps it: none. Its row was the
    // source's to count.
    let kept: i64 = tokio::task::block_in_place(|| {
        postgres::Client::connect(&url, postgres::NoTls)
            .unwrap()
            .query_one(
                "SELECT count(*) FROM lake_write_ahead_part WHERE stream = $1 AND sequence = 1",
                &[&STREAM],
            )
            .unwrap()
            .get(0)
    });
    assert_eq!(
        kept, 0,
        "group 0 counts nothing of the write its source counted"
    );
}

/// No write is answered while a range of its dataset is between two
/// databases (step 3d; spec 6.4, 6.7 and 6.8; PR CI 43675491409, ha: two
/// prices answered recorded 0, restated 0, unchanged 0 just after the
/// driver's range switched from database 0 to database 1). Across
/// databases a switch is two commits: the source lets the range go, then
/// its target opens it, its position in the stream taken back with the
/// open. Between them the source passes a write whose row it no longer
/// commits, while the target's position still says it passed the write on
/// its first take, when the row was not its own. Every position past the
/// write, the answer was given with the row neither committed nor counted.
/// It now waits for the open, and for the target to take the write again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_is_not_answered_while_its_range_is_between_two_databases() {
    use meridian_changes::moves::points;
    use meridian_domain::v1::{RecordObservationsReply, RecordTradesRequest};
    use meridian_lake::partition_moves::Asked;
    use meridian_lake::service;
    use meridian_lake::write_ahead::{settled, taken_past_a_switch, Taken, Writer};
    use prost::Message;
    const STREAM: &str = "LAKE_WRITE_AHEAD";
    // The dataset group 0's, which answers its writes, in database 0; its
    // lower half moved to group 1, in database 1.
    let a = OffRuntime::new(tokio::task::block_in_place(|| across("between", true)));
    let (dataset, low, lower) = (a.dataset.clone(), a.low.clone(), a.lower());
    let mover = OffRuntime::new(Arc::new(a.mover()));
    tokio::task::block_in_place(|| {
        assert!(mover.begin(&lower));
        mover.run(&lower, 1, &Asked::default()).unwrap();
        for store in a.zeros.iter().chain(&a.ones) {
            store.read_owners().unwrap();
        }
    });
    let (lake0, lake1) = tokio::task::block_in_place(|| {
        (
            ha_lake_over(
                a.zero.clone() as Arc<dyn Store>,
                a.zeros[0].groups(),
                "lake-0",
                &dataset,
            ),
            ha_lake_over(
                a.one.clone() as Arc<dyn Store>,
                a.ones[0].groups(),
                "lake-1",
                &dataset,
            ),
        )
    });
    let (lake0, lake1) = (OffRuntime::new(lake0), OffRuntime::new(lake1));
    let write = |n: u64| Taken {
        sequence: n,
        at_ns: 1_000 + n as i64,
        topic: service::RECORD_TRADES.into(),
        reply_to: Some(format!("_INBOX.between.{n}")),
        after: None,
        envelope: meridian_bus::Envelope {
            meta: Some(meridian_bus::MessageMeta {
                publisher_instance_id: "coinbase-1".into(),
                account_scope_applies: true,
                published_at_ns: SystemClock.now_ns(),
                message_id: format!("between-{n}"),
                ..Default::default()
            }),
            payload_type: "meridian.v1.RecordTradesRequest".into(),
            payload: RecordTradesRequest {
                trades: vec![
                    match trade(&dataset, &format!("b{n}"), &low, 7_000 + n as i64) {
                        Observation::Trade(trade) => *trade,
                        _ => unreachable!(),
                    },
                ],
                want_id: String::new(),
            }
            .encode_to_vec(),
        },
        dataset: dataset.clone(),
        timeout_ms: None,
    };
    let count = |answered: &(Option<String>, meridian_bus::HandlerReply)| {
        let (_, payload) = answered.1.clone().expect("answered");
        let reply = RecordObservationsReply::decode(payload.as_slice()).unwrap();
        (reply.recorded, reply.restated, reply.unchanged)
    };
    let mut writer0 = OffRuntime::new(
        tokio::task::block_in_place(|| Writer::start(lake0.clone(), STREAM, 0)).unwrap(),
    );
    let mut writer1 = OffRuntime::new(
        tokio::task::block_in_place(|| Writer::start(lake1.clone(), STREAM, 0)).unwrap(),
    );
    // A first write, its row group 1's: committed there, answered by group 0.
    taken_past_a_switch(&mut writer1, vec![write(1)])
        .await
        .unwrap();
    let first = taken_past_a_switch(&mut writer0, vec![write(1)])
        .await
        .unwrap();
    let waits = writer0.take_waits();
    let first = settled(&lake0, STREAM, first, waits).await;
    assert_eq!(count(&first[0]), (1, 0, 0), "counted from group 1's part");
    // A second, taken by group 0 first: its row not group 0's, its answer
    // waiting for group 1.
    let answers = taken_past_a_switch(&mut writer0, vec![write(2)])
        .await
        .unwrap();
    let waits = writer0.take_waits();
    // The lower half moved home to group 0, stopped between its two
    // commits: let go in database 1, not yet opened in database 0.
    points::arm(
        "lake.switch.between",
        &lower,
        Arc::new(|_| Err("stopped between the two databases".into())),
    );
    tokio::task::block_in_place(|| {
        assert!(mover.begin(&lower));
        mover.run(&lower, 0, &Asked::default()).unwrap();
    });
    points::disarm("lake.switch.between", &lower);
    // Group 1, the source, passes the write: its row is no longer its own.
    taken_past_a_switch(&mut writer1, vec![write(2)])
        .await
        .unwrap();
    // Every position is past the write, and its row is committed nowhere.
    let settling = {
        let lake0 = lake0.clone();
        tokio::spawn(async move { settled(&lake0, STREAM, answers, waits).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert!(
        !settling.is_finished(),
        "answered while its row was neither committed nor counted"
    );
    // The sweep opens the half at home, group 0's place taken back with
    // it; group 0 takes the write again, and commits its row.
    tokio::task::block_in_place(|| {
        mover.rebalance();
    });
    let took = taken_past_a_switch(&mut writer0, vec![write(3)]).await;
    assert!(
        matches!(
            took,
            Err(meridian_lake::store::StoreError::PositionMoved(_))
        ),
        "its position was taken back: {took:?}"
    );
    tokio::task::block_in_place(|| writer0.reread()).unwrap();
    while writer0.next() <= 2 {
        let from = writer0.next();
        taken_past_a_switch(&mut writer0, vec![write(from)])
            .await
            .unwrap();
    }
    let answered = settling.await.unwrap();
    assert_eq!(
        count(&answered[0]),
        (1, 0, 0),
        "answered once its row was committed, and counted"
    );
}

/// A write the target group passed, its row then another group's, whose
/// range's switch between databases commits just after -- and no write
/// after it -- is taken again by the target from the place the switch took
/// back, and answered within the hold (step 3d; spec 6.1 and 6.8; PR CI
/// 43810170103, ha: "no write refused while it moved", a price refused
/// DEADLINE_EXCEEDED at 5 s). Group 1 had passed write 2302 100 ms before
/// the switch committed; its place was taken back to 2301; the driver
/// sends its next price only once the last is answered, so the stream was
/// quiet, and group 1's writer found its place taken back only when the
/// next write came, 6 s later.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_passed_just_before_its_range_switched_is_taken_again_with_no_write_after() {
    use meridian_domain::v1::{RecordObservationsReply, RecordTradesRequest};
    use meridian_lake::partition_moves::Asked;
    use meridian_lake::service;
    use meridian_lake::write_ahead::{
        read, settled, taken_past_a_switch, Read, Stall, Taken, Writer,
    };
    use prost::Message;
    const STREAM: &str = "LAKE_WRITE_AHEAD";
    /// The stream's leader, stood in: it holds the two writes.
    struct Two;
    impl meridian_lake::write_ahead::Leader for Two {
        async fn last(&mut self) -> Result<u64, String> {
            Ok(2)
        }
    }
    // The dataset group 0's, which answers its writes, split, both halves
    // in database 0; its lower half moves to group 1, in database 1.
    let a = OffRuntime::new(tokio::task::block_in_place(|| across("passed", true)));
    let (dataset, low, lower) = (a.dataset.clone(), a.low.clone(), a.lower());
    let mover = OffRuntime::new(Arc::new(a.mover()));
    let (lake0, lake1) = tokio::task::block_in_place(|| {
        (
            ha_lake_over(
                a.zero.clone() as Arc<dyn Store>,
                a.zeros[0].groups(),
                "lake-0",
                &dataset,
            ),
            ha_lake_over(
                a.one.clone() as Arc<dyn Store>,
                a.ones[0].groups(),
                "lake-1",
                &dataset,
            ),
        )
    });
    let (lake0, lake1) = (OffRuntime::new(lake0), OffRuntime::new(lake1));
    let write = |n: u64| Taken {
        sequence: n,
        at_ns: 1_000 + n as i64,
        topic: service::RECORD_TRADES.into(),
        reply_to: Some(format!("_INBOX.passed.{n}")),
        after: None,
        envelope: meridian_bus::Envelope {
            meta: Some(meridian_bus::MessageMeta {
                publisher_instance_id: "coinbase-1".into(),
                account_scope_applies: true,
                published_at_ns: SystemClock.now_ns(),
                message_id: format!("passed-{n}"),
                ..Default::default()
            }),
            payload_type: "meridian.v1.RecordTradesRequest".into(),
            payload: RecordTradesRequest {
                trades: vec![
                    match trade(&dataset, &format!("p{n}"), &low, 7_000 + n as i64) {
                        Observation::Trade(trade) => *trade,
                        _ => unreachable!(),
                    },
                ],
                want_id: String::new(),
            }
            .encode_to_vec(),
        },
        dataset: dataset.clone(),
        timeout_ms: None,
    };
    let count = |answered: &(Option<String>, meridian_bus::HandlerReply)| {
        let (_, payload) = answered.1.clone().expect("answered");
        let reply = RecordObservationsReply::decode(payload.as_slice()).unwrap();
        (reply.recorded, reply.restated, reply.unchanged)
    };
    let mut writer0 = OffRuntime::new(
        tokio::task::block_in_place(|| Writer::start(lake0.clone(), STREAM, 0)).unwrap(),
    );
    let mut writer1 = OffRuntime::new(
        tokio::task::block_in_place(|| Writer::start(lake1.clone(), STREAM, 0)).unwrap(),
    );
    // A first write, its row group 0's: committed there, passed by group 1.
    taken_past_a_switch(&mut writer0, vec![write(1)])
        .await
        .unwrap();
    writer0.take_waits();
    assert!(!writer1.may_be_taken_back(), "nothing passed yet");
    taken_past_a_switch(&mut writer1, vec![write(1)])
        .await
        .unwrap();
    // The second, passed by group 1 first: its row still group 0's.
    taken_past_a_switch(&mut writer1, vec![write(2)])
        .await
        .unwrap();
    assert!(writer1.may_be_taken_back(), "a write passed");
    // The lower half's switch to group 1 commits: group 1's place is
    // taken back to group 0's, 1.
    tokio::task::block_in_place(|| {
        assert!(mover.begin(&lower));
        mover.run(&lower, 1, &Asked::default()).unwrap();
    });
    // Group 0, its batch held by the switch till now, passes the write and
    // answers it once group 1 has committed the row.
    let began = tokio::time::Instant::now();
    let answers = taken_past_a_switch(&mut writer0, vec![write(2)])
        .await
        .unwrap();
    let waits = writer0.take_waits();
    let settling = {
        let lake0 = lake0.clone();
        tokio::spawn(async move { settled(&lake0, STREAM, answers, waits).await })
    };
    // No write comes after it. Group 1's reader, delivered nothing, finds
    // its place taken back and takes the stream again from there.
    let hold = meridian_changes::moves::hold();
    let mut quiet =
        futures_util::stream::pending::<Result<async_nats::jetstream::Message, String>>();
    let mut stall = Stall::new(meridian_lake::write_ahead::STALL_AFTER);
    let next = writer1.next();
    let said = tokio::time::timeout(
        hold,
        read(&mut quiet, &mut stall, next, &mut Two, &*writer1),
    )
    .await;
    assert!(
        matches!(said, Ok(Read::TakenBack)),
        "group 1 never found its place taken back within the hold: {said:?}"
    );
    tokio::task::block_in_place(|| writer1.reread()).unwrap();
    assert_eq!(writer1.next(), 2, "taken back to group 0's place");
    taken_past_a_switch(&mut writer1, vec![write(2)])
        .await
        .unwrap();
    let answered = settling.await.unwrap();
    assert!(
        began.elapsed() < hold,
        "answered {:?} after the switch, past the hold",
        began.elapsed()
    );
    assert_eq!(
        count(&answered[0]),
        (1, 0, 0),
        "answered once group 1 committed its row"
    );
}

/// Item 191 (ruled 2026-11-19): a move's switch takes back the target's
/// place in the write-ahead stream the lake's writers take from now, kept
/// by its identity, and never touches a place kept in a stream made before
/// it under the same name. In `ha`, until the writer has read the stream's
/// identity, no range switches.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_switch_takes_back_the_place_kept_by_the_streams_identity_never_an_older_streams() {
    use meridian_domain::v1::{RecordObservationsReply, RecordTradesRequest};
    use meridian_lake::partition_moves::Asked;
    use meridian_lake::service;
    use meridian_lake::store::{WriteAheadBatch, WriteAheadPosition};
    use meridian_lake::write_ahead::{
        group_key, read, settled, stream_key, taken_past_a_switch, Read, Stall, Taken, Writer,
    };
    use prost::Message;
    const STREAM: &str = "LAKE_WRITE_AHEAD";
    const IDENTITY: i64 = 1_790_000_000_000_000_000;
    struct Two;
    impl meridian_lake::write_ahead::Leader for Two {
        async fn last(&mut self) -> Result<u64, String> {
            Ok(2)
        }
    }
    let a = tokio::task::block_in_place(|| across("identity", true));
    let (dataset, low, lower) = (a.dataset.clone(), a.low.clone(), a.lower());
    let mover = Arc::new(a.mover());
    let (lake0, lake1) = tokio::task::block_in_place(|| {
        (
            ha_lake_over(
                a.zero.clone() as Arc<dyn Store>,
                a.zeros[0].groups(),
                "lake-0",
                &dataset,
            ),
            ha_lake_over(
                a.one.clone() as Arc<dyn Store>,
                a.ones[0].groups(),
                "lake-1",
                &dataset,
            ),
        )
    });
    // `ha`, the stream's identity not read yet: no switch.
    for store in a.zeros.iter().chain(&a.ones) {
        store.stream_not_read();
    }
    tokio::task::block_in_place(|| {
        assert!(mover.begin(&lower));
        let refused = mover.run(&lower, 1, &Asked::default());
        assert!(
            refused
                .as_ref()
                .is_err_and(|failed| failed.to_string().contains("identity is not read")),
            "a range switched before the stream's identity was read: {:?}",
            refused.map(|_| ())
        );
    });
    // A place group 1 kept in a stream made before this one under its name.
    let older = WriteAheadPosition {
        sequence: 50,
        at_ns: 500,
    };
    tokio::task::block_in_place(|| {
        a.one.record_write_ahead(
            WriteAheadBatch {
                stream: group_key(STREAM, 1),
                after: None,
                through: older,
                ..Default::default()
            },
            SystemClock.now_ns(),
        )
    })
    .unwrap();
    // The writers read the stream's identity.
    lake0.with_stream(IDENTITY);
    lake1.with_stream(IDENTITY);
    let write = |n: u64| Taken {
        sequence: n,
        at_ns: 1_000 + n as i64,
        topic: service::RECORD_TRADES.into(),
        reply_to: Some(format!("_INBOX.identity.{n}")),
        after: None,
        envelope: meridian_bus::Envelope {
            meta: Some(meridian_bus::MessageMeta {
                publisher_instance_id: "coinbase-1".into(),
                account_scope_applies: true,
                published_at_ns: SystemClock.now_ns(),
                message_id: format!("identity-{n}"),
                ..Default::default()
            }),
            payload_type: "meridian.v1.RecordTradesRequest".into(),
            payload: RecordTradesRequest {
                trades: vec![
                    match trade(&dataset, &format!("i{n}"), &low, 7_000 + n as i64) {
                        Observation::Trade(trade) => *trade,
                        _ => unreachable!(),
                    },
                ],
                want_id: String::new(),
            }
            .encode_to_vec(),
        },
        dataset: dataset.clone(),
        timeout_ms: None,
    };
    let mut writer0 =
        tokio::task::block_in_place(|| Writer::start(lake0.clone(), STREAM, IDENTITY)).unwrap();
    let mut writer1 =
        tokio::task::block_in_place(|| Writer::start(lake1.clone(), STREAM, IDENTITY)).unwrap();
    assert_eq!(writer1.position(), None, "no place in this stream");
    taken_past_a_switch(&mut writer0, vec![write(1)])
        .await
        .unwrap();
    writer0.take_waits();
    taken_past_a_switch(&mut writer1, vec![write(1)])
        .await
        .unwrap();
    taken_past_a_switch(&mut writer1, vec![write(2)])
        .await
        .unwrap();
    // The lower half's switch to group 1: its place in this stream taken
    // back to group 0's, 1.
    tokio::task::block_in_place(|| {
        assert!(mover.begin(&lower));
        mover.run(&lower, 1, &Asked::default()).unwrap();
    });
    let answers = taken_past_a_switch(&mut writer0, vec![write(2)])
        .await
        .unwrap();
    let waits = writer0.take_waits();
    let base = stream_key(STREAM, IDENTITY);
    let settling = {
        let (lake0, base) = (lake0.clone(), base.clone());
        tokio::spawn(async move { settled(&lake0, &base, answers, waits).await })
    };
    let mut quiet =
        futures_util::stream::pending::<Result<async_nats::jetstream::Message, String>>();
    let mut stall = Stall::new(meridian_lake::write_ahead::STALL_AFTER);
    let next = writer1.next();
    let said = tokio::time::timeout(
        meridian_changes::moves::hold(),
        read(&mut quiet, &mut stall, next, &mut Two, &writer1),
    )
    .await;
    assert!(
        matches!(said, Ok(Read::TakenBack)),
        "group 1's place in this stream was not taken back: {said:?}"
    );
    tokio::task::block_in_place(|| writer1.reread()).unwrap();
    assert_eq!(writer1.next(), 2, "taken back to group 0's place");
    taken_past_a_switch(&mut writer1, vec![write(2)])
        .await
        .unwrap();
    let answered = settling.await.unwrap();
    let (_, payload) = answered[0].1.clone().expect("answered");
    let reply = RecordObservationsReply::decode(payload.as_slice()).unwrap();
    assert_eq!((reply.recorded, reply.restated, reply.unchanged), (1, 0, 0));
    // The older stream's place is as it was.
    assert_eq!(
        tokio::task::block_in_place(|| a.one.write_ahead_position(&group_key(STREAM, 1))).unwrap(),
        Some(older)
    );
    assert!(base.ends_with(&IDENTITY.to_string()));
    tokio::task::block_in_place(move || drop((writer0, writer1, lake0, lake1, mover, a)));
}

/// The review of item 191's finding 1, in Postgres: what a lake before
/// item 191 kept of its write-ahead stream by its name alone -- the
/// position, a run it found dropped, a dataset's latest valid time and a
/// part of a write -- carried to the stream's key with its identity in one
/// transaction, once: never over what the identity's key holds, and never
/// into a stream made after the position was taken.
#[test]
fn places_kept_by_the_streams_name_are_carried_to_its_identity_once() {
    use meridian_lake::store::{Counts, Dropped, WriteAheadBatch, WriteAheadPosition};
    use meridian_lake::write_ahead::stream_key;
    const STREAM: &str = "LAKE_WRITE_AHEAD";
    let (url, store) = isolated("carried");
    let at = WriteAheadPosition {
        sequence: 10,
        at_ns: 10_000,
    };
    let run = Dropped {
        first: 5,
        last: 5,
        after_at_ns: 4_000,
        until_at_ns: 6_000,
        gap_id: "gap-carried".into(),
    };
    store
        .record_write_ahead(
            WriteAheadBatch {
                stream: STREAM.into(),
                after: None,
                through: at,
                dropped: vec![run.clone()],
                valid: vec![("coinbase-1:daily".to_string(), 7_000)],
                ..Default::default()
            },
            SystemClock.now_ns(),
        )
        .unwrap();
    postgres::Client::connect(&url, postgres::NoTls)
        .unwrap()
        .execute(
            "INSERT INTO lake_write_ahead_part
                    (stream, sequence, recorded, restated, unchanged, committed_at_ns)
             VALUES ($1, 9, 1, 0, 0, 1)",
            &[&STREAM],
        )
        .unwrap();
    // A stream made after the position was taken: nothing carried.
    let later = stream_key(STREAM, 20_000);
    assert!(!store.write_ahead_carried(STREAM, &later, 20_000).unwrap());
    assert_eq!(store.write_ahead_position(&later).unwrap(), None);
    // The stream it was taken in: everything carried, once.
    let key = stream_key(STREAM, 1_000);
    assert!(store.write_ahead_carried(STREAM, &key, 1_000).unwrap());
    assert_eq!(store.write_ahead_position(&key).unwrap(), Some(at));
    let (dropped, valid) = store.write_ahead_found(&key).unwrap();
    assert_eq!(dropped, vec![run]);
    assert_eq!(valid.get("coinbase-1:daily"), Some(&7_000));
    assert_eq!(
        store.write_ahead_part(&key, 0, 9).unwrap(),
        Some(Counts {
            recorded: 1,
            restated: 0,
            unchanged: 0
        })
    );
    // Once: the identity's key keeps its own from now on.
    store
        .record_write_ahead(
            WriteAheadBatch {
                stream: key.clone(),
                after: Some(at),
                through: WriteAheadPosition {
                    sequence: 11,
                    at_ns: 11_000,
                },
                ..Default::default()
            },
            SystemClock.now_ns(),
        )
        .unwrap();
    assert!(!store.write_ahead_carried(STREAM, &key, 1_000).unwrap());
    assert_eq!(
        store
            .write_ahead_position(&key)
            .unwrap()
            .map(|at| at.sequence),
        Some(11)
    );
}

/// The review of item 191's finding 2 (`lake_partition.taken_in`, item
/// 202), at finding 1's carry: where a range's source stood at its last
/// move, kept before `taken_in` (a bare `taken_from`), is a place in the
/// stream the position under the name was taken in. The carry into that
/// stream marks it taken there, so its group leaves uncounted what it left
/// uncounted before the upgrade. A range that never moved, or whose place
/// is kept with its stream already, is left as it is; a stream made after
/// the position was taken marks nothing.
#[test]
fn a_place_taken_from_before_taken_in_is_carried_into_the_stream_it_was_taken_in() {
    use meridian_lake::store::{WriteAheadBatch, WriteAheadPosition};
    use meridian_lake::write_ahead::stream_key;
    const STREAM: &str = "LAKE_WRITE_AHEAD";
    let (url, store) = isolated("taken-in-carried");
    store
        .record_write_ahead(
            WriteAheadBatch {
                stream: STREAM.into(),
                after: None,
                through: WriteAheadPosition {
                    sequence: 10,
                    at_ns: 10_000,
                },
                ..Default::default()
            },
            SystemClock.now_ns(),
        )
        .unwrap();
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    client
        .batch_execute(
            "INSERT INTO lake_partition (dataset, head, taken_from, taken_in) VALUES
                ('before-1:daily', 1, 7, NULL),
                ('never-1:daily', 1, NULL, NULL),
                ('kept-1:daily', 1, 9, 42)",
        )
        .unwrap();
    let mut taken_in = || -> Vec<(String, Option<i64>)> {
        client
            .query(
                "SELECT dataset, taken_in FROM lake_partition ORDER BY dataset",
                &[],
            )
            .unwrap()
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect()
    };
    let before = vec![
        ("before-1:daily".to_string(), None),
        ("kept-1:daily".to_string(), Some(42)),
        ("never-1:daily".to_string(), None),
    ];
    // A stream made after the position was taken: nothing marked.
    assert!(!store
        .write_ahead_carried(STREAM, &stream_key(STREAM, 20_000), 20_000)
        .unwrap());
    assert_eq!(taken_in(), before);
    // The stream it was taken in: the bare place marked taken there.
    assert!(store
        .write_ahead_carried(STREAM, &stream_key(STREAM, 1_000), 1_000)
        .unwrap());
    assert_eq!(
        taken_in(),
        vec![
            ("before-1:daily".to_string(), Some(1_000)),
            ("kept-1:daily".to_string(), Some(42)),
            ("never-1:daily".to_string(), None),
        ],
        "the bare taken_from carried into the stream it was taken in"
    );
    // Read as a place in that stream by the writer taking from it.
    store.in_stream(1_000);
    assert_eq!(
        store
            .taken_from(&[
                "before-1:daily".to_string(),
                "never-1:daily".to_string(),
                "kept-1:daily".to_string(),
            ])
            .unwrap(),
        [("before-1:daily".to_string(), 7)].into_iter().collect()
    );
}

/// The review of item 191's finding 2 (`lake_partition.taken_in`, item
/// 202): where a range's source stood at its last move (`taken_from`) is a
/// place in the stream the move was made in, and in no other. After `ha`
/// is left and taken again, the stream made again numbers from its own
/// first: a row its range's group finds unchanged in a write at a sequence
/// at or below the old `taken_from` was counted by nobody, and is counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn where_a_range_was_taken_from_in_an_older_stream_never_leaves_a_row_uncounted() {
    use meridian_domain::v1::{RecordObservationsReply, RecordTradesRequest};
    use meridian_lake::partition_moves::{Asked, Ended, Mover, Settings};
    use meridian_lake::service;
    use meridian_lake::store::{WriteAheadBatch, WriteAheadPosition};
    use meridian_lake::write_ahead::{
        group_key, settled, stream_key, taken_past_a_switch, Taken, Writer,
    };
    use prost::Message;
    const STREAM: &str = "LAKE_WRITE_AHEAD";
    const OLD: i64 = 1_790_000_000_000_000_000;
    const NEW: i64 = OLD + 600_000_000_000;
    // The dataset and both its halves group 1's, which answers its writes.
    let (url, zero, one, dataset, _lower, upper, low, high) =
        tokio::task::block_in_place(|| two_groups_and_a_split_of("taken-in", 1));
    let (lake0, lake1) = tokio::task::block_in_place(|| {
        (
            ha_lake(&zero, "lake-0", &dataset),
            ha_lake(&one, "lake-1", &dataset),
        )
    });
    // In the old stream: group 1 had committed through 500 when the upper
    // half moved to group 0.
    lake0.with_stream(OLD);
    lake1.with_stream(OLD);
    tokio::task::block_in_place(|| {
        one.record_write_ahead(
            WriteAheadBatch {
                stream: group_key(&stream_key(STREAM, OLD), 1),
                after: None,
                through: WriteAheadPosition {
                    sequence: 500,
                    at_ns: 500,
                },
                ..Default::default()
            },
            SystemClock.now_ns(),
        )
    })
    .unwrap();
    let ended = tokio::task::block_in_place(|| {
        let mover = Mover::new(
            vec![zero.clone()],
            zero.groups(),
            Settings::default(),
            Arc::new(SystemClock),
        );
        assert!(mover.begin(&upper));
        mover.run(&upper, 0, &Asked::default()).unwrap()
    });
    assert!(matches!(ended, Ended::Moved(_)), "{ended:?}");
    let taken_from: Option<i64> = tokio::task::block_in_place(|| {
        postgres::Client::connect(&url, postgres::NoTls)
            .unwrap()
            .query_one(
                "SELECT taken_from FROM lake_partition WHERE dataset = $1",
                &[&upper],
            )
            .unwrap()
            .get(0)
    });
    assert_eq!(
        taken_from,
        Some(500),
        "where its source stood in the old stream"
    );
    // `ha` left and taken again: the stream made again, the writers in it.
    lake0.with_stream(NEW);
    lake1.with_stream(NEW);
    let sent_at = SystemClock.now_ns();
    let write = |n: u64, key: &str, subject: &String| Taken {
        sequence: n,
        at_ns: 1_000 + n as i64,
        topic: service::RECORD_TRADES.into(),
        reply_to: Some(format!("_INBOX.taken-in.{n}")),
        after: None,
        envelope: meridian_bus::Envelope {
            meta: Some(meridian_bus::MessageMeta {
                publisher_instance_id: "coinbase-1".into(),
                account_scope_applies: true,
                published_at_ns: sent_at,
                message_id: format!("taken-in-{n}"),
                ..Default::default()
            }),
            payload_type: "meridian.v1.RecordTradesRequest".into(),
            payload: RecordTradesRequest {
                trades: vec![match trade(&dataset, key, subject, 5_000) {
                    Observation::Trade(trade) => *trade,
                    _ => unreachable!(),
                }],
                want_id: String::new(),
            }
            .encode_to_vec(),
        },
        dataset: dataset.clone(),
        timeout_ms: None,
    };
    // A row in the upper half, group 0's now; one in the lower; and at 3
    // the first sent again, unchanged.
    let stream = vec![
        write(1, "again", &high),
        write(2, "other", &low),
        write(3, "again", &high),
    ];
    let mut writer0 =
        tokio::task::block_in_place(|| Writer::start(lake0.clone(), STREAM, NEW)).unwrap();
    let mut writer1 =
        tokio::task::block_in_place(|| Writer::start(lake1.clone(), STREAM, NEW)).unwrap();
    let answers = taken_past_a_switch(&mut writer1, stream.clone())
        .await
        .unwrap();
    let waits = writer1.take_waits();
    taken_past_a_switch(&mut writer0, stream.clone())
        .await
        .unwrap();
    writer0.take_waits();
    let answered = settled(&lake1, writer1.stream_name(), answers, waits).await;
    let count = |at: usize| {
        let (_, payload) = answered[at]
            .1
            .clone()
            .unwrap_or_else(|failed| panic!("write {} refused: {failed:?}", at + 1));
        let reply = RecordObservationsReply::decode(payload.as_slice()).unwrap();
        (reply.recorded, reply.restated, reply.unchanged)
    };
    assert_eq!(count(0), (1, 0, 0));
    assert_eq!(count(1), (1, 0, 0));
    assert_eq!(
        count(2),
        (0, 0, 1),
        "the row sent again at 3, below the old stream's taken_from: counted unchanged"
    );
    tokio::task::block_in_place(move || drop((writer0, writer1, lake0, lake1, zero, one)));
}

/// The review of item 191's finding 2, in `tolerant`: a lake with no
/// write-ahead stream reads and writes no place in one at a move's switch.
/// Places a lake kept in `ha` before it left stay as they were, and the
/// range keeps no `taken_from` from them.
#[test]
fn a_switch_in_tolerant_reads_and_writes_no_place_in_a_stream() {
    use meridian_lake::partition_moves::{Asked, Ended, Mover, Settings};
    use meridian_lake::store::{WriteAheadBatch, WriteAheadPosition};
    use meridian_lake::write_ahead::group_key;
    const STREAM: &str = "LAKE_WRITE_AHEAD";
    // The dataset and both its halves group 1's.
    let (url, zero, one, _dataset, _lower, upper, _low, _high) =
        two_groups_and_a_split_of("tolerant-places", 1);
    for store in [&zero, &one] {
        store.in_stream(meridian_lake::postgres::NO_STREAM);
    }
    // A place group 0 kept in `ha`, frozen since.
    let frozen = WriteAheadPosition {
        sequence: 7,
        at_ns: 700,
    };
    zero.record_write_ahead(
        WriteAheadBatch {
            stream: group_key(STREAM, 0),
            after: None,
            through: frozen,
            ..Default::default()
        },
        SystemClock.now_ns(),
    )
    .unwrap();
    let mover = Mover::new(
        vec![zero.clone()],
        zero.groups(),
        Settings::default(),
        Arc::new(SystemClock),
    );
    assert!(mover.begin(&upper));
    let ended = mover.run(&upper, 0, &Asked::default()).unwrap();
    assert!(matches!(ended, Ended::Moved(_)), "{ended:?}");
    assert_eq!(
        zero.write_ahead_position(&group_key(STREAM, 0)).unwrap(),
        Some(frozen),
        "a place kept in ha taken back by a switch in tolerant"
    );
    let taken_from: Option<i64> = postgres::Client::connect(&url, postgres::NoTls)
        .unwrap()
        .query_one(
            "SELECT taken_from FROM lake_partition WHERE dataset = $1",
            &[&upper],
        )
        .unwrap()
        .get(0);
    assert_eq!(taken_from, None, "a taken_from kept from no stream");
}

/// The review of item 191's finding 3(b): a move's switch whose source has
/// no place in the stream -- it never committed in it -- takes its target
/// back to before the stream's first write, never one write back: the
/// range's writes the target passed while the source owned it are all
/// taken again, the first among them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_switch_from_a_source_with_no_place_takes_its_target_back_to_the_streams_first_write() {
    use meridian_domain::v1::RecordTradesRequest;
    use meridian_lake::partition_moves::Asked;
    use meridian_lake::service;
    use meridian_lake::write_ahead::{taken_past_a_switch, Taken, Writer};
    use prost::Message;
    const STREAM: &str = "LAKE_WRITE_AHEAD";
    // The dataset group 0's, split, both halves in database 0; its lower
    // half moves to group 1, in database 1.
    let a = tokio::task::block_in_place(|| across("no-source", true));
    let (dataset, low, lower) = (a.dataset.clone(), a.low.clone(), a.lower());
    let mover = Arc::new(a.mover());
    let lake1 = tokio::task::block_in_place(|| {
        ha_lake_over(
            a.one.clone() as Arc<dyn Store>,
            a.ones[0].groups(),
            "lake-1",
            &dataset,
        )
    });
    let write = |n: u64| Taken {
        sequence: n,
        at_ns: 1_000 + n as i64,
        topic: service::RECORD_TRADES.into(),
        reply_to: Some(format!("_INBOX.no-source.{n}")),
        after: None,
        envelope: meridian_bus::Envelope {
            meta: Some(meridian_bus::MessageMeta {
                publisher_instance_id: "coinbase-1".into(),
                account_scope_applies: true,
                published_at_ns: SystemClock.now_ns(),
                message_id: format!("no-source-{n}"),
                ..Default::default()
            }),
            payload_type: "meridian.v1.RecordTradesRequest".into(),
            payload: RecordTradesRequest {
                trades: vec![
                    match trade(&dataset, &format!("p{n}"), &low, 7_000 + n as i64) {
                        Observation::Trade(trade) => *trade,
                        _ => unreachable!(),
                    },
                ],
                want_id: String::new(),
            }
            .encode_to_vec(),
        },
        dataset: dataset.clone(),
        timeout_ms: None,
    };
    let mut writer1 =
        tokio::task::block_in_place(|| Writer::start(lake1.clone(), STREAM, 0)).unwrap();
    // Group 1 takes 1 and 2, passing both: the lower half is group 0's,
    // which has committed nothing in the stream.
    taken_past_a_switch(&mut writer1, vec![write(1), write(2)])
        .await
        .unwrap();
    assert_eq!(writer1.next(), 3);
    tokio::task::block_in_place(|| {
        assert!(mover.begin(&lower));
        mover.run(&lower, 1, &Asked::default()).unwrap();
    });
    tokio::task::block_in_place(|| writer1.reread()).unwrap();
    assert_eq!(
        writer1.next(),
        1,
        "taken back to before the stream's first write: write 1's row of the range is group 1's to take"
    );
    while writer1.next() <= 2 {
        let from = writer1.next();
        taken_past_a_switch(&mut writer1, (from..=2).map(write).collect())
            .await
            .unwrap();
    }
    let keys: Vec<String> = tokio::task::block_in_place(|| a.rows(1, &lower))
        .into_iter()
        .map(|(_, key)| key)
        .collect();
    assert!(
        keys.contains(&"p1".to_string()) && keys.contains(&"p2".to_string()),
        "both writes' rows committed where the range went: {keys:?}"
    );
    tokio::task::block_in_place(move || drop((writer1, lake1, mover, a)));
}

/// The review of item 191's finding 3(a): a move's switch whose target has
/// no place in the stream writes it one, before the stream's first write,
/// so a batch the target took on its view from before the switch -- its
/// `after` none -- is refused, never committed without the range's rows;
/// and the target, its view read again, takes the range's writes from the
/// stream's first with no gap counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_switch_to_a_target_with_no_place_refuses_its_batch_taken_on_the_view_before() {
    use meridian_domain::v1::RecordTradesRequest;
    use meridian_lake::partition_moves::Asked;
    use meridian_lake::service;
    use meridian_lake::store::StoreError;
    use meridian_lake::write_ahead::{group_key, taken_past_a_switch, Taken, Writer};
    use prost::Message;
    const STREAM: &str = "LAKE_WRITE_AHEAD";
    let a = tokio::task::block_in_place(|| across("no-target", true));
    let (dataset, low, lower) = (a.dataset.clone(), a.low.clone(), a.lower());
    let mover = Arc::new(a.mover());
    let (lake0, lake1) = tokio::task::block_in_place(|| {
        (
            ha_lake_over(
                a.zero.clone() as Arc<dyn Store>,
                a.zeros[0].groups(),
                "lake-0",
                &dataset,
            ),
            ha_lake_over(
                a.one.clone() as Arc<dyn Store>,
                a.ones[0].groups(),
                "lake-1",
                &dataset,
            ),
        )
    });
    let write = |n: u64| Taken {
        sequence: n,
        at_ns: 1_000 + n as i64,
        topic: service::RECORD_TRADES.into(),
        reply_to: Some(format!("_INBOX.no-target.{n}")),
        after: None,
        envelope: meridian_bus::Envelope {
            meta: Some(meridian_bus::MessageMeta {
                publisher_instance_id: "coinbase-1".into(),
                account_scope_applies: true,
                published_at_ns: SystemClock.now_ns(),
                message_id: format!("no-target-{n}"),
                ..Default::default()
            }),
            payload_type: "meridian.v1.RecordTradesRequest".into(),
            payload: RecordTradesRequest {
                trades: vec![
                    match trade(&dataset, &format!("p{n}"), &low, 7_000 + n as i64) {
                        Observation::Trade(trade) => *trade,
                        _ => unreachable!(),
                    },
                ],
                want_id: String::new(),
            }
            .encode_to_vec(),
        },
        dataset: dataset.clone(),
        timeout_ms: None,
    };
    let mut writer0 =
        tokio::task::block_in_place(|| Writer::start(lake0.clone(), STREAM, 0)).unwrap();
    let mut writer1 =
        tokio::task::block_in_place(|| Writer::start(lake1.clone(), STREAM, 0)).unwrap();
    // Group 0, the source, commits write 1's row: its place 1.
    taken_past_a_switch(&mut writer0, vec![write(1)])
        .await
        .unwrap();
    writer0.take_waits();
    // The lower half's switch to group 1, which has never committed.
    tokio::task::block_in_place(|| {
        assert!(mover.begin(&lower));
        mover.run(&lower, 1, &Asked::default()).unwrap();
    });
    // Group 1's batch of 1 and 2, taken on its view from before the switch,
    // which names the range group 0's: none of the range's rows in it.
    let took = tokio::task::block_in_place(|| writer1.take(vec![write(1), write(2)]));
    assert!(
        matches!(took, Err(StoreError::PositionMoved(_))),
        "a batch taken on the view from before the switch was committed: {:?}",
        took.map(|answers| answers.len())
    );
    tokio::task::block_in_place(|| writer1.reread()).unwrap();
    assert_eq!(writer1.next(), 1, "taken from the stream's first write");
    while writer1.next() <= 2 {
        let from = writer1.next();
        taken_past_a_switch(&mut writer1, (from..=2).map(write).collect())
            .await
            .unwrap();
    }
    let keys: Vec<String> = tokio::task::block_in_place(|| a.rows(1, &lower))
        .into_iter()
        .map(|(_, key)| key)
        .collect();
    assert!(
        keys.contains(&"p2".to_string()),
        "write 2's row committed where the range went: {keys:?}"
    );
    let (dropped, _) =
        tokio::task::block_in_place(|| a.one.write_ahead_found(&group_key(STREAM, 1))).unwrap();
    assert!(dropped.is_empty(), "no gap counted: {dropped:?}");
    tokio::task::block_in_place(move || drop((writer0, writer1, lake0, lake1, mover, a)));
}

/// The review of item 191's finding 11: a move's switch that takes its
/// target back from 1 to 0 -- the source at the same place -- takes it to
/// before the stream's first write, a place that counts nothing before the
/// first write it takes as dropped. Write 1, which the target committed,
/// aged out of the stream since, is never counted lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_target_taken_back_from_its_first_write_counts_no_run_before_the_streams_first() {
    use meridian_domain::v1::RecordTradesRequest;
    use meridian_lake::partition_moves::Asked;
    use meridian_lake::service;
    use meridian_lake::write_ahead::{group_key, taken_past_a_switch, Taken, Writer};
    use prost::Message;
    const STREAM: &str = "LAKE_WRITE_AHEAD";
    let a = tokio::task::block_in_place(|| across("back-to-first", true));
    let (dataset, low, lower) = (a.dataset.clone(), a.low.clone(), a.lower());
    let mover = Arc::new(a.mover());
    let (lake0, lake1) = tokio::task::block_in_place(|| {
        (
            ha_lake_over(
                a.zero.clone() as Arc<dyn Store>,
                a.zeros[0].groups(),
                "lake-0",
                &dataset,
            ),
            ha_lake_over(
                a.one.clone() as Arc<dyn Store>,
                a.ones[0].groups(),
                "lake-1",
                &dataset,
            ),
        )
    });
    let write = |n: u64| Taken {
        sequence: n,
        at_ns: 1_000 + n as i64,
        topic: service::RECORD_TRADES.into(),
        reply_to: Some(format!("_INBOX.back-to-first.{n}")),
        after: None,
        envelope: meridian_bus::Envelope {
            meta: Some(meridian_bus::MessageMeta {
                publisher_instance_id: "coinbase-1".into(),
                account_scope_applies: true,
                published_at_ns: SystemClock.now_ns(),
                message_id: format!("back-to-first-{n}"),
                ..Default::default()
            }),
            payload_type: "meridian.v1.RecordTradesRequest".into(),
            payload: RecordTradesRequest {
                trades: vec![
                    match trade(&dataset, &format!("p{n}"), &low, 7_000 + n as i64) {
                        Observation::Trade(trade) => *trade,
                        _ => unreachable!(),
                    },
                ],
                want_id: String::new(),
            }
            .encode_to_vec(),
        },
        dataset: dataset.clone(),
        timeout_ms: None,
    };
    let mut writer0 =
        tokio::task::block_in_place(|| Writer::start(lake0.clone(), STREAM, 0)).unwrap();
    let mut writer1 =
        tokio::task::block_in_place(|| Writer::start(lake1.clone(), STREAM, 0)).unwrap();
    // Both groups at 1: group 0 committed write 1's row, group 1 passed it.
    taken_past_a_switch(&mut writer0, vec![write(1)])
        .await
        .unwrap();
    writer0.take_waits();
    taken_past_a_switch(&mut writer1, vec![write(1)])
        .await
        .unwrap();
    // The switch steps group 1 back from 1, the source's place.
    tokio::task::block_in_place(|| {
        assert!(mover.begin(&lower));
        mover.run(&lower, 1, &Asked::default()).unwrap();
    });
    tokio::task::block_in_place(|| writer1.reread()).unwrap();
    assert_eq!(writer1.next(), 1);
    // Write 1 aged out of the stream since: its first is 2.
    taken_past_a_switch(&mut writer1, vec![write(2)])
        .await
        .unwrap();
    let (dropped, _) =
        tokio::task::block_in_place(|| a.one.write_ahead_found(&group_key(STREAM, 1))).unwrap();
    assert!(
        dropped.is_empty(),
        "write 1, committed, counted lost: {dropped:?}"
    );
    let keys: Vec<String> = tokio::task::block_in_place(|| a.rows(1, &lower))
        .into_iter()
        .map(|(_, key)| key)
        .collect();
    assert!(keys.contains(&"p2".to_string()), "{keys:?}");
    tokio::task::block_in_place(move || drop((writer0, writer1, lake0, lake1, mover, a)));
}

/// The review of item 191's finding 4: a range's switch between databases
/// stopped between its two rows keeps, beside the source's pointer, the
/// place its target takes the range's rows from -- the source's -- under
/// the target's key: group 0's place, with group 1 never committed.
fn a_switch_stopped_beside_a_pointer(tag: &str) -> Across {
    use meridian_changes::moves::points;
    use meridian_lake::partition_moves::Asked;
    use meridian_lake::store::{WriteAheadBatch, WriteAheadPosition};
    use meridian_lake::write_ahead::group_key;
    const STREAM: &str = "LAKE_WRITE_AHEAD";
    let a = across(tag, true);
    let lower = a.lower();
    let mover = a.mover();
    // Group 0, the source, has committed through 5, at home.
    let source = WriteAheadPosition {
        sequence: 5,
        at_ns: 500,
    };
    a.zero
        .record_write_ahead(
            WriteAheadBatch {
                stream: group_key(STREAM, 0),
                after: None,
                through: source,
                ..Default::default()
            },
            SystemClock.now_ns(),
        )
        .unwrap();
    points::arm(
        "lake.switch.between",
        &lower,
        Arc::new(|_| Err("stopped between the two databases".into())),
    );
    assert!(mover.begin(&lower));
    let _ = mover.run(&lower, 1, &Asked::default());
    points::disarm("lake.switch.between", &lower);
    // The point kept beside the source's pointer, in database 0.
    assert_eq!(
        a.zeros[0]
            .write_ahead_position(&group_key(STREAM, 1))
            .unwrap(),
        Some(source),
        "the point kept beside the pointer"
    );
    a
}

/// Finding 4, the group's own read: a target group that has never
/// committed is not at the place kept beside a pointer. Its own position is
/// none -- its home's first, never a point kept beside a pointer -- so it
/// skips none of its own ranges' writes.
#[test]
fn a_point_kept_beside_a_pointer_is_never_a_fresh_groups_own_position() {
    use meridian_lake::write_ahead::group_key;
    let a = a_switch_stopped_beside_a_pointer("kept-point");
    assert_eq!(
        a.one
            .write_ahead_position(&group_key("LAKE_WRITE_AHEAD", 1))
            .unwrap(),
        None,
        "a fresh group adopted the point kept beside a pointer"
    );
}

/// Finding 4, another group's read (`group_position`, as a group waiting
/// for the target reads it): the place kept beside a pointer is not the
/// fresh target's position either.
#[test]
fn a_point_kept_beside_a_pointer_is_never_read_as_a_fresh_groups_position() {
    use meridian_lake::write_ahead::group_key;
    let a = a_switch_stopped_beside_a_pointer("kept-point-read");
    assert_eq!(
        a.zero
            .group_position(&group_key("LAKE_WRITE_AHEAD", 1), 1)
            .unwrap(),
        None,
        "another group read the point kept beside a pointer as group 1's"
    );
}

/// The same, where the switch's target is the group answering the dataset
/// (failover-repeat 43993402238, ha run 3: "lake range
/// drive-data:drive/8000-ffff to group 1, from database 0 to database 1: no
/// write refused while it moved", a price refused DEADLINE_EXCEEDED at 5 s).
/// Group 1 answers the driver's dataset -- it owns the dataset's own
/// partition -- so it took the price, its range then group 0's, rather than
/// pass it: it committed its place past the price with none of its rows and
/// waited to answer on group 0's part. The range's switch to it committed
/// 16 ms after the price reached the stream and took its place back; with
/// no write after, its writer found that only when the next write came, 6 s
/// on, and the answer waited past its caller. The target takes the write
/// again from the place the switch took back, with no write after it, and
/// the answer is given within the hold, counting its row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_its_answering_group_took_just_before_its_range_switched_to_it_is_taken_again() {
    use meridian_domain::v1::{RecordObservationsReply, RecordTradesRequest};
    use meridian_lake::partition_moves::{Asked, Ended};
    use meridian_lake::service;
    use meridian_lake::write_ahead::{
        read, settled, taken_past_a_switch, Read, Stall, Taken, Writer,
    };
    use prost::Message;
    const STREAM: &str = "LAKE_WRITE_AHEAD";
    /// The stream's leader, stood in: it holds the two writes.
    struct Two;
    impl meridian_lake::write_ahead::Leader for Two {
        async fn last(&mut self) -> Result<u64, String> {
            Ok(2)
        }
    }
    // Group 1's dataset, which it answers, split at its home (database 1);
    // the upper half moved to group 0 in database 0 first, nothing writing:
    // the shape CI moved it back from. Every member reads it, as its sweep
    // does.
    let a = OffRuntime::new(tokio::task::block_in_place(|| {
        across_owned("answering", true, 1)
    }));
    let dataset = a.dataset.clone();
    let upper = format!("{dataset}/8000-ffff");
    let (_, high) = halves_of("answering");
    let mover = OffRuntime::new(Arc::new(a.mover()));
    let asked = || Asked {
        by_hand: true,
        moved_by: "admin-1".into(),
        reason: "the upper half, out and back to the group answering its dataset".into(),
    };
    tokio::task::block_in_place(|| {
        assert!(mover.begin(&upper));
        let ended = mover.run(&upper, 0, &asked()).unwrap();
        assert!(matches!(ended, Ended::Moved(_)), "{ended:?}");
        for store in a.zeros.iter().chain(&a.ones) {
            store.read_owners().unwrap();
        }
    });
    let (lake0, lake1) = tokio::task::block_in_place(|| {
        (
            ha_lake_over(
                a.zero.clone() as Arc<dyn Store>,
                a.zeros[0].groups(),
                "lake-0",
                &dataset,
            ),
            ha_lake_over(
                a.one.clone() as Arc<dyn Store>,
                a.ones[0].groups(),
                "lake-1",
                &dataset,
            ),
        )
    });
    let (lake0, lake1) = (OffRuntime::new(lake0), OffRuntime::new(lake1));
    let write = |n: u64| Taken {
        sequence: n,
        at_ns: 1_000 + n as i64,
        topic: service::RECORD_TRADES.into(),
        reply_to: Some(format!("_INBOX.answering.{n}")),
        after: None,
        envelope: meridian_bus::Envelope {
            meta: Some(meridian_bus::MessageMeta {
                publisher_instance_id: "coinbase-1".into(),
                account_scope_applies: true,
                published_at_ns: SystemClock.now_ns(),
                message_id: format!("answering-{n}"),
                ..Default::default()
            }),
            payload_type: "meridian.v1.RecordTradesRequest".into(),
            payload: RecordTradesRequest {
                trades: vec![
                    match trade(&dataset, &format!("q{n}"), &high, 7_000 + n as i64) {
                        Observation::Trade(trade) => *trade,
                        _ => unreachable!(),
                    },
                ],
                want_id: String::new(),
            }
            .encode_to_vec(),
        },
        dataset: dataset.clone(),
        timeout_ms: None,
    };
    let count = |answered: &(Option<String>, meridian_bus::HandlerReply)| {
        let (_, payload) = answered.1.clone().expect("answered");
        let reply = RecordObservationsReply::decode(payload.as_slice()).unwrap();
        (reply.recorded, reply.restated, reply.unchanged)
    };
    let mut writer0 = OffRuntime::new(
        tokio::task::block_in_place(|| Writer::start(lake0.clone(), STREAM, 0)).unwrap(),
    );
    let mut writer1 = OffRuntime::new(
        tokio::task::block_in_place(|| Writer::start(lake1.clone(), STREAM, 0)).unwrap(),
    );
    // A first write, its row group 0's: committed there, taken by group 1
    // with none of its rows.
    taken_past_a_switch(&mut writer0, vec![write(1)])
        .await
        .unwrap();
    writer0.take_waits();
    taken_past_a_switch(&mut writer1, vec![write(1)])
        .await
        .unwrap();
    writer1.take_waits();
    // The second, taken by group 1 first, its row still group 0's: its
    // answer waits on group 0's part, on a task of its own as the writer's.
    let answers = taken_past_a_switch(&mut writer1, vec![write(2)])
        .await
        .unwrap();
    let waits = writer1.take_waits();
    assert_eq!(waits.len(), 1, "group 1 answers its dataset's write");
    // The upper half's switch back to group 1 commits: group 1's place is
    // taken back to group 0's, 1.
    tokio::task::block_in_place(|| {
        assert!(mover.begin(&upper));
        let ended = mover.run(&upper, 1, &asked()).unwrap();
        assert!(matches!(ended, Ended::Moved(_)), "{ended:?}");
    });
    let began = tokio::time::Instant::now();
    let settling = {
        let lake1 = lake1.clone();
        tokio::spawn(async move { settled(&lake1, STREAM, answers, waits).await })
    };
    // Group 0, its batch held by the switch till now, passes the write.
    taken_past_a_switch(&mut writer0, vec![write(2)])
        .await
        .unwrap();
    writer0.take_waits();
    // No write comes after it. Group 1's reader, delivered nothing, finds
    // its place taken back and takes the stream again from there.
    let hold = meridian_changes::moves::hold();
    let mut quiet =
        futures_util::stream::pending::<Result<async_nats::jetstream::Message, String>>();
    let mut stall = Stall::new(meridian_lake::write_ahead::STALL_AFTER);
    let next = writer1.next();
    let said = tokio::time::timeout(
        hold,
        read(&mut quiet, &mut stall, next, &mut Two, &*writer1),
    )
    .await;
    assert!(
        matches!(said, Ok(Read::TakenBack)),
        "group 1, which answers the dataset, never found its place taken back within the hold: \
         {said:?}"
    );
    tokio::task::block_in_place(|| writer1.reread()).unwrap();
    assert_eq!(writer1.next(), 2, "taken back to group 0's place");
    let again = taken_past_a_switch(&mut writer1, vec![write(2)])
        .await
        .unwrap();
    let waits = writer1.take_waits();
    let again = settled(&lake1, STREAM, again, waits).await;
    assert_eq!(
        count(&again[0]),
        (1, 0, 0),
        "taken again, its row committed"
    );
    let answered = settling.await.unwrap();
    assert!(
        began.elapsed() < hold,
        "answered {:?} after the switch, past the hold",
        began.elapsed()
    );
    assert_eq!(
        count(&answered[0]),
        (1, 0, 0),
        "the first take's answer, given once group 1 committed its row"
    );
    assert_eq!(
        tokio::task::block_in_place(|| a.rows(1, &upper))
            .iter()
            .filter(|(_, key)| key == "q2")
            .count(),
        1,
        "the row held once, where its range is now"
    );
}

/// An answer that waits on another group's part is given before its caller
/// stops waiting: past the caller's wait less the answer's way back, where
/// that comes before the hold, it is refused OVERLOADED with a retry-after,
/// never left to the caller's timeout (step 3d; spec 6.8, the hold;
/// failover-repeat 43993402238, ha run 3: a price's answer could wait the
/// hold from its take, past its caller's 5 s, and its caller heard
/// DEADLINE_EXCEEDED). The caller's wait is the one its sidecar sent with
/// the write (`Meridian-Timeout-Ms`), counted from when it was sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_answer_waiting_on_another_groups_part_is_refused_before_its_caller_stops_waiting() {
    use meridian_domain::v1::RecordTradesRequest;
    use meridian_lake::partition_moves::{Asked, Ended};
    use meridian_lake::service;
    use meridian_lake::write_ahead::{settled, taken_past_a_switch, Taken, Writer};
    use prost::Message;
    const STREAM: &str = "LAKE_WRITE_AHEAD";
    /// The caller's wait: shorter than the hold.
    const WAIT_MS: u64 = 2_000;
    let hold = meridian_changes::moves::hold();
    assert!(std::time::Duration::from_millis(WAIT_MS) < hold);
    // Group 1's dataset, which it answers, its upper half group 0's.
    let a = OffRuntime::new(tokio::task::block_in_place(|| {
        across_owned("caller", true, 1)
    }));
    let dataset = a.dataset.clone();
    let upper = format!("{dataset}/8000-ffff");
    let (_, high) = halves_of("caller");
    let mover = OffRuntime::new(a.mover());
    tokio::task::block_in_place(|| {
        assert!(mover.begin(&upper));
        let asked = Asked {
            by_hand: true,
            moved_by: "admin-1".into(),
            reason: "the upper half to the group not answering its dataset".into(),
        };
        let ended = mover.run(&upper, 0, &asked).unwrap();
        assert!(matches!(ended, Ended::Moved(_)), "{ended:?}");
        for store in a.zeros.iter().chain(&a.ones) {
            store.read_owners().unwrap();
        }
    });
    let lake1 = OffRuntime::new(tokio::task::block_in_place(|| {
        ha_lake_over(
            a.one.clone() as Arc<dyn Store>,
            a.ones[0].groups(),
            "lake-1",
            &dataset,
        )
    }));
    // A write sent now, its row in group 0's half, its caller waiting 2 s.
    // Group 0 never takes it.
    let sent = SystemClock.now_ns();
    let write = Taken {
        sequence: 1,
        at_ns: sent,
        topic: service::RECORD_TRADES.into(),
        reply_to: Some("_INBOX.caller.1".into()),
        after: None,
        envelope: meridian_bus::Envelope {
            meta: Some(meridian_bus::MessageMeta {
                publisher_instance_id: "coinbase-1".into(),
                account_scope_applies: true,
                published_at_ns: sent,
                message_id: "caller-1".into(),
                ..Default::default()
            }),
            payload_type: "meridian.v1.RecordTradesRequest".into(),
            payload: RecordTradesRequest {
                trades: vec![match trade(&dataset, "c1", &high, 7_001) {
                    Observation::Trade(trade) => *trade,
                    _ => unreachable!(),
                }],
                want_id: String::new(),
            }
            .encode_to_vec(),
        },
        dataset: dataset.clone(),
        timeout_ms: Some(WAIT_MS),
    };
    let began = std::time::Instant::now();
    let mut writer1 = OffRuntime::new(
        tokio::task::block_in_place(|| Writer::start(lake1.clone(), STREAM, 0)).unwrap(),
    );
    let answers = taken_past_a_switch(&mut writer1, vec![write])
        .await
        .unwrap();
    let waits = writer1.take_waits();
    assert_eq!(
        waits.len(),
        1,
        "group 1 answers once group 0's part commits"
    );
    let answered = settled(&lake1, STREAM, answers, waits).await;
    let took = began.elapsed();
    let refused = answered[0]
        .1
        .clone()
        .expect_err("group 0 never committed its part");
    assert!(
        took < std::time::Duration::from_millis(WAIT_MS),
        "answered {took:?} after it was sent, past its caller's {WAIT_MS} ms: {refused}"
    );
    assert!(
        took >= std::time::Duration::from_millis(WAIT_MS)
            - meridian_changes::moves::ANSWERED_BEFORE
            - std::time::Duration::from_millis(100),
        "refused {took:?} after it was sent, well before its caller's wait less the way back"
    );
    let overloaded = format!(
        "refusal-reason {}",
        meridian_pb::v1::RefusalReason::Overloaded as i32
    );
    assert!(
        refused.contains(&overloaded) && meridian_bus::refusal_retry_after(&refused).is_some(),
        "OVERLOADED with a retry-after: {refused}"
    );
}

/// A write of `dataset`'s row `key` on `subject`, sent now, its caller
/// waiting `wait_ms`, as the stream holds it at `sequence`.
fn a_callers_write(
    dataset: &str,
    key: &str,
    subject: &str,
    sequence: u64,
    wait_ms: u64,
) -> meridian_lake::write_ahead::Taken {
    use meridian_domain::v1::RecordTradesRequest;
    use prost::Message;
    let sent = SystemClock.now_ns();
    meridian_lake::write_ahead::Taken {
        sequence,
        at_ns: sent,
        topic: meridian_lake::service::RECORD_TRADES.into(),
        reply_to: Some(format!("_INBOX.{key}")),
        after: None,
        envelope: meridian_bus::Envelope {
            meta: Some(meridian_bus::MessageMeta {
                publisher_instance_id: "coinbase-1".into(),
                account_scope_applies: true,
                published_at_ns: sent,
                message_id: key.into(),
                ..Default::default()
            }),
            payload_type: "meridian.v1.RecordTradesRequest".into(),
            payload: RecordTradesRequest {
                trades: vec![
                    match trade(dataset, key, subject, 7_000 + sequence as i64) {
                        Observation::Trade(trade) => *trade,
                        _ => unreachable!(),
                    },
                ],
                want_id: String::new(),
            }
            .encode_to_vec(),
        },
        dataset: dataset.to_string(),
        timeout_ms: Some(wait_ms),
    }
}

/// Group 1's dataset, which it answers, its upper half moved to group 0 in
/// database 0 (the shape ha run 3 moved it back from): the across, the
/// upper half's name and a subject in it, and the mover.
fn answering_with_its_upper_half_out(
    tag: &str,
) -> (
    Across,
    String,
    String,
    meridian_lake::partition_moves::Mover,
) {
    use meridian_lake::partition_moves::{Asked, Ended};
    let a = across_owned(tag, true, 1);
    let upper = format!("{}/8000-ffff", a.dataset);
    let (_, high) = halves_of(tag);
    let mover = a.mover();
    assert!(mover.begin(&upper));
    let asked = Asked {
        by_hand: true,
        moved_by: "admin-1".into(),
        reason: "the upper half to the group not answering its dataset".into(),
    };
    let ended = mover.run(&upper, 0, &asked).unwrap();
    assert!(matches!(ended, Ended::Moved(_)), "{ended:?}");
    for store in a.zeros.iter().chain(&a.ones) {
        store.read_owners().unwrap();
    }
    (a, upper, high, mover)
}

/// An async test that fails drops its databases as it unwinds (merge-queue
/// run 44594070565): what it holds over its stores is dropped off the
/// runtime ([`OffRuntime`]), so no sync client closes on the runtime's
/// thread -- a panic in a destructor, which ended the process with its two
/// `lake2_*` databases left for `make test-store` to fail on.
#[test]
fn what_a_failed_async_test_made_is_dropped_as_it_unwinds() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let there = |names: &[String]| -> i64 {
        postgres::Client::connect(&test_database(), postgres::NoTls)
            .unwrap()
            .query_one(
                "SELECT count(*) FROM pg_database WHERE datname = ANY($1)",
                &[&names],
            )
            .unwrap()
            .get(0)
    };
    let made = std::sync::Mutex::new(Vec::new());
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(async {
            let (a, _upper, _high, mover) =
                tokio::task::block_in_place(|| answering_with_its_upper_half_out("unwound"));
            let (a, mover) = (OffRuntime::new(a), OffRuntime::new(mover));
            let lake1 = OffRuntime::new(tokio::task::block_in_place(|| {
                ha_lake_over(
                    a.one.clone() as Arc<dyn Store>,
                    a.ones[0].groups(),
                    "lake-1",
                    &a.dataset,
                )
            }));
            let writer1 = OffRuntime::new(
                tokio::task::block_in_place(|| {
                    meridian_lake::write_ahead::Writer::start(lake1.clone(), "LAKE_WRITE_AHEAD", 0)
                })
                .unwrap(),
            );
            let names: Vec<String> = a.urls.iter().map(|url| url.name().to_string()).collect();
            assert_eq!(tokio::task::block_in_place(|| there(&names)), 2);
            *made.lock().unwrap() = names;
            tokio::task::yield_now().await;
            let _held = (&writer1, &mover);
            panic!("an async store test that fails");
        })
    }));
    assert!(failed.is_err());
    let names = made.lock().unwrap().clone();
    assert_eq!(names.len(), 2, "it failed after making its databases");
    assert_eq!(
        there(&names),
        0,
        "dropped as the failed test's stack unwound: {names:?}"
    );
}

/// Each answer of a batch is sent the moment it is decided, by its own
/// caller's wait (review c5r M1): one that waits on no other group at once,
/// and of two waiting on a part group 0 never commits, the one whose caller
/// waits 2 s refused within those 2 s, never held for its batch-mate's 5 s.
/// While either waits, the writer still looks at its place (review c5r L3).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_answer_of_a_batch_is_sent_by_its_own_callers_wait() {
    use meridian_lake::write_ahead::{settle_each, taken_past_a_switch, Writer};
    use std::sync::atomic::Ordering;
    const STREAM: &str = "LAKE_WRITE_AHEAD";
    let (a, _upper, high, mover) =
        tokio::task::block_in_place(|| answering_with_its_upper_half_out("each"));
    let dataset = a.dataset.clone();
    let (a, _mover) = (OffRuntime::new(a), OffRuntime::new(mover));
    let lake1 = OffRuntime::new(tokio::task::block_in_place(|| {
        ha_lake_over(
            a.one.clone() as Arc<dyn Store>,
            a.ones[0].groups(),
            "lake-1",
            &dataset,
        )
    }));
    // One batch: a 5 s caller's write first, then a 2 s caller's, each a
    // row of group 0's half, which group 0 never takes.
    let began = std::time::Instant::now();
    let writes = vec![
        a_callers_write(&dataset, "each-5s", &high, 1, 5_000),
        a_callers_write(&dataset, "each-2s", &high, 2, 2_000),
    ];
    let mut writer1 = OffRuntime::new(
        tokio::task::block_in_place(|| Writer::start(lake1.clone(), STREAM, 0)).unwrap(),
    );
    let mut answers = taken_past_a_switch(&mut writer1, writes).await.unwrap();
    let mut waits = writer1.take_waits();
    assert_eq!(
        waits.len(),
        2,
        "group 1 answers both once group 0's parts commit"
    );
    // A batch-mate that waits on nothing, first in the batch.
    answers.insert(
        0,
        (
            Some("_INBOX.each-now".into()),
            Ok(("meridian.v1.RecordObservationsReply".into(), Vec::new())),
        ),
    );
    for wait in &mut waits {
        wait.at += 1;
    }
    let settling = writer1.settling();
    let mut each = settle_each(
        lake1.clone(),
        STREAM.to_string(),
        answers,
        waits,
        Some(settling.clone()),
    );
    let mut heard = Vec::new();
    while let Some((at, to, answered)) = each.recv().await {
        if heard.is_empty() {
            assert_eq!(settling.load(Ordering::SeqCst), 2, "both wait");
            assert!(
                writer1.may_be_taken_back(),
                "it looks while an answer waits"
            );
        }
        heard.push((at, to, answered.is_ok(), began.elapsed()));
    }
    let when = |to: &str| {
        heard
            .iter()
            .find(|(_, said, _, _)| said.as_deref() == Some(to))
            .unwrap_or_else(|| panic!("{to} never answered: {heard:?}"))
            .clone()
    };
    let now = when("_INBOX.each-now");
    assert!(
        now.3 < std::time::Duration::from_millis(500),
        "the answer waiting on nothing was held {:?}: {heard:?}",
        now.3
    );
    let two = when("_INBOX.each-2s");
    assert!(
        !two.2 && two.3 < std::time::Duration::from_millis(2_000),
        "the 2 s caller's refusal came {:?} after it was sent, past its wait: {heard:?}",
        two.3
    );
    let five = when("_INBOX.each-5s");
    assert!(
        !five.2 && five.3 < std::time::Duration::from_millis(5_000),
        "the 5 s caller's refusal came {:?}: {heard:?}",
        five.3
    );
    assert_eq!(settling.load(Ordering::SeqCst), 0, "no answer waits");
}

/// A batch held by a move's switch is held no longer than its caller waits
/// (review c5r M2; spec 6.8): its switch stopped between the two databases,
/// the range a pointer where it goes, a write into it whose caller waits 2 s
/// is refused Held within those 2 s -- never the hold's 5 s -- and the
/// group answering it answers OVERLOADED with a retry-after.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_batch_held_by_a_switch_is_refused_before_its_caller_stops_waiting() {
    use meridian_changes::moves::points;
    use meridian_lake::partition_moves::Asked;
    use meridian_lake::store::StoreError;
    use meridian_lake::write_ahead::{taken_past_a_switch, Writer};
    const STREAM: &str = "LAKE_WRITE_AHEAD";
    const WAIT_MS: u64 = 2_000;
    let (a, upper, high, mover) =
        tokio::task::block_in_place(|| answering_with_its_upper_half_out("held"));
    let dataset = a.dataset.clone();
    let (a, mover) = (OffRuntime::new(a), OffRuntime::new(mover));
    // Back to group 1, stopped between the two databases: the target's row
    // a pointer, so its writes there are held until the sweep opens it.
    tokio::task::block_in_place(|| {
        points::arm(
            "lake.switch.between",
            &upper,
            Arc::new(|_| Err("stopped between the two databases".into())),
        );
        assert!(mover.begin(&upper));
        let asked = Asked {
            by_hand: true,
            moved_by: "admin-1".into(),
            reason: "the upper half home, stopped between".into(),
        };
        let ended = mover.run(&upper, 1, &asked);
        points::disarm("lake.switch.between", &upper);
        assert!(ended.is_ok(), "{ended:?}");
        for store in a.zeros.iter().chain(&a.ones) {
            store.read_owners().unwrap();
        }
    });
    let lake1 = OffRuntime::new(tokio::task::block_in_place(|| {
        ha_lake_over(
            a.one.clone() as Arc<dyn Store>,
            a.ones[0].groups(),
            "lake-1",
            &dataset,
        )
    }));
    // Its caller's wait counts from when the write was sent, so the time is
    // read before the write is stamped. Read after the writer started, it
    // left out what the start took, and beside other tests' load a refusal
    // on time read as one come too early.
    let began = std::time::Instant::now();
    let write = a_callers_write(&dataset, "held-1", &high, 1, WAIT_MS);
    let mut writer1 = OffRuntime::new(
        tokio::task::block_in_place(|| Writer::start(lake1.clone(), STREAM, 0)).unwrap(),
    );
    let took = taken_past_a_switch(&mut writer1, vec![write.clone()]).await;
    let held = began.elapsed();
    assert!(
        matches!(took, Err(StoreError::Held(_))),
        "held by the switch: {:?}",
        took.map(|answers| answers.len())
    );
    assert!(
        held < std::time::Duration::from_millis(WAIT_MS),
        "held {held:?}, past its caller's {WAIT_MS} ms"
    );
    assert!(
        held >= std::time::Duration::from_millis(WAIT_MS)
            - meridian_changes::moves::ANSWERED_BEFORE
            - std::time::Duration::from_millis(100),
        "held only {held:?}: refused well before its caller's wait less the way back"
    );
    let refused = writer1.held_refusals(&[write]);
    assert_eq!(refused.len(), 1, "the write group 1 answers");
    assert_eq!(refused[0].0.as_deref(), Some("_INBOX.held-1"));
    let refusal = refused[0].1.clone().unwrap_err();
    let overloaded = format!(
        "refusal-reason {}",
        meridian_pb::v1::RefusalReason::Overloaded as i32
    );
    assert!(
        refusal.contains(&overloaded) && meridian_bus::refusal_retry_after(&refusal).is_some(),
        "OVERLOADED with a retry-after: {refusal}"
    );
}

/// A write split across two writer groups, in ha (step 3d; spec 6.7): the
/// group answering it waits until the other group's consumer has committed
/// its part -- its position in the stream past the write -- and answers
/// with a watermark naming each part's range at its head, so a read after
/// that watermark finds both parts at once. Never within the hold, it is
/// refused OVERLOADED with a retry-after.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_split_across_groups_is_answered_once_every_part_has_committed() {
    use meridian_domain::v1::{RecordObservationsReply, Watermark};
    use meridian_lake::partition_moves::{Asked, Mover, Settings};
    use meridian_lake::store::{WriteAheadBatch, WriteAheadPosition};
    use meridian_lake::write_ahead::{settled, Wait};
    use prost::Message;
    let (_url, zero, one, dataset, lower, upper, low, high) =
        tokio::task::block_in_place(|| two_groups_and_a_split("lake-wait"));
    let (zero, one) = (OffRuntime::new(zero), OffRuntime::new(one));
    tokio::task::block_in_place(|| {
        let mover = Mover::new(
            vec![zero.clone()],
            zero.groups(),
            Settings::default(),
            Arc::new(SystemClock),
        );
        assert!(mover.begin(&lower));
        mover.run(&lower, 1, &Asked::default()).unwrap();
        one.read_owners().unwrap();
    });
    let bus = Arc::new(meridian_bus::Bus::single(
        "lake-1",
        Arc::new(meridian_bus::MemoryBackend::new()),
        Arc::new(SystemClock),
    ));
    let lake = OffRuntime::new(meridian_lake::service::serve_grouped(
        bus,
        zero.clone() as Arc<dyn Store>,
        Arc::new(SystemClock),
        zero.groups(),
    ));
    let stream = "lake-test-stream";
    // Group 0's part committed with its position, and its answer made.
    let part = |store: &PostgresStore, key: &str, row: Observation, sequence: u64| {
        store
            .record_write_ahead(
                WriteAheadBatch {
                    stream: key.to_string(),
                    after: None,
                    through: WriteAheadPosition { sequence, at_ns: 1 },
                    parts: vec![(dataset.clone(), vec![row])],
                    ..Default::default()
                },
                5_000,
            )
            .unwrap()
    };
    tokio::task::block_in_place(|| part(&zero, stream, trade(&dataset, "w-high", &high, 70), 7));
    let answer = || {
        vec![(
            Some("_INBOX.w".to_string()),
            Ok((
                "meridian.v1.RecordObservationsReply".to_string(),
                RecordObservationsReply {
                    recorded: 1,
                    watermark: Some(Watermark {
                        partitions: vec![meridian_pb::v1::PartitionSequence {
                            partition: dataset.clone(),
                            sequence: 0,
                        }],
                    }),
                    ..Default::default()
                }
                .encode_to_vec(),
            )),
        )]
    };
    let wait = || Wait {
        at: 0,
        sequence: 7,
        groups: vec![1],
        dataset: String::new(),
        ..Default::default()
    };
    // Group 1's part commits a moment later: the answer waits for it.
    let committing = {
        let (one, dataset, low) = (one.clone(), dataset.clone(), low.clone());
        let key = format!("{stream}.group-1");
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            tokio::task::block_in_place(|| {
                one.record_write_ahead(
                    WriteAheadBatch {
                        stream: key,
                        after: None,
                        through: WriteAheadPosition {
                            sequence: 7,
                            at_ns: 1,
                        },
                        parts: vec![(dataset.clone(), vec![trade(&dataset, "w-low", &low, 71)])],
                        ..Default::default()
                    },
                    5_000,
                )
                .unwrap()
            })
        })
    };
    let began = std::time::Instant::now();
    let answered = settled(&lake, stream, answer(), vec![wait()]).await;
    assert!(
        began.elapsed() >= std::time::Duration::from_millis(250),
        "it waited"
    );
    committing.await.unwrap();
    let (_, payload) = answered[0].1.clone().expect("answered once both committed");
    let reply = RecordObservationsReply::decode(payload.as_slice()).unwrap();
    let named: std::collections::BTreeMap<String, u64> = reply
        .watermark
        .unwrap()
        .partitions
        .into_iter()
        .map(|at| (at.partition, at.sequence))
        .collect();
    // Read after the watermark: both parts there at once.
    tokio::task::block_in_place(|| {
        let heads = zero.heads().unwrap();
        for range in [&lower, &upper] {
            assert!(
                named[range] >= 1 && heads[range] >= named[range],
                "{range}: {named:?}"
            );
        }
        for (subject, key) in [(&low, "w-low"), (&high, "w-high")] {
            let read = zero
                .read(&Query {
                    datasets: vec![dataset.clone()],
                    data_type: DataType::Trade,
                    subjects: vec![subject.to_string()],
                    kinds: vec![],
                    interval_ns: 0,
                    when: When::Latest { at_ns: 0 },
                    as_of_ns: 0,
                    published_as_of_ns: 0,
                    latest_only: false,
                })
                .unwrap();
            assert!(
                read.iter().any(|row| row.meta().row_key == key),
                "{key} read at once"
            );
        }
    });
    // A part never committed: refused OVERLOADED within the hold.
    let never = settled(
        &lake,
        stream,
        answer(),
        vec![Wait {
            sequence: 99,
            ..wait()
        }],
    )
    .await;
    let refused = never[0].1.clone().unwrap_err();
    let overloaded = format!(
        "refusal-reason {}",
        meridian_pb::v1::RefusalReason::Overloaded as i32
    );
    assert!(refused.contains(&overloaded), "{refused}");
    // Their connections closed off the runtime's threads.
}

/// The lake's database count raised under load (step 3d; sections 6.4 and
/// 6.8, ruled 2026-11-14: each range is written and read where its head
/// is). Two writer groups wrote one database; with a second, writer group
/// 1's home is the new one, but its datasets' heads are in the first: their
/// writes go on there, never held past a switch or refused, until the
/// sweep moves each home -- once, from group 1 to group 1, by_hand false --
/// after which they are written at home. A second sweep moves nothing, and
/// every key written is read back through either member.
#[test]
fn a_raised_lake_database_count_writes_where_each_head_is_and_moves_each_range_home_once() {
    use meridian_changes::groups::Groups;
    use meridian_lake::partition_moves::{Ended, Mover, Settings};
    let urls = two_databases();
    let datasets: Vec<String> = (0..)
        .map(|n| unique(&format!("coinbase-1:raise{n}")))
        .filter(|dataset| meridian_changes::groups::owner(dataset, 2) == 1)
        .take(2)
        .collect();
    let subject = unique("LCL-RAISE");
    // One database: group 1 writes its datasets there.
    {
        let member = |group: u32| {
            let store = PostgresStore::connect(&urls[0], 4)
                .unwrap()
                .with_groups(Groups::new(group, 2).unwrap());
            store.adopt().unwrap();
            store
        };
        let (zero, one) = (member(0), member(1));
        zero.pin_owners().unwrap();
        for dataset in &datasets {
            one.record(dataset, vec![trade(dataset, "before", &subject, 10)], 1_000)
                .unwrap();
        }
    }
    // Two databases: group 1's home is the second.
    let stores = |group: u32| -> Vec<Arc<PostgresStore>> {
        let groups = Groups::new(group, 2).unwrap();
        urls.iter()
            .enumerate()
            .map(|(d, url)| {
                Arc::new(
                    PostgresStore::connect(url, 4)
                        .unwrap()
                        .with_groups(groups)
                        .with_databases(2)
                        .in_database(d as u32),
                )
            })
            .collect()
    };
    let (zeros, ones) = (stores(0), stores(1));
    let zero = Arc::new(meridian_lake::spread::Spread::new(
        zeros.clone(),
        zeros[0].groups(),
    ));
    let one = Arc::new(meridian_lake::spread::Spread::new(
        ones.clone(),
        ones[0].groups(),
    ));
    zero.adopt().unwrap();
    one.adopt().unwrap();
    for store in zeros.iter().chain(&ones) {
        store.read_owners().unwrap();
    }
    let fresh = |d: usize, range: &str| {
        PostgresStore::connect(&urls[d], 1)
            .unwrap()
            .range_tip(range)
            .unwrap()
    };
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let load: Vec<_> = datasets
        .iter()
        .map(|dataset| {
            writing_range(
                one.clone() as Arc<dyn Store>,
                zero.clone() as Arc<dyn Store>,
                dataset.clone(),
                subject.clone(),
                "w",
                stop.clone(),
            )
        })
        .collect();
    std::thread::sleep(std::time::Duration::from_millis(300));
    // Written on where the heads are: the new database not begun.
    for dataset in &datasets {
        assert!(fresh(0, dataset).unwrap().head > 1, "{dataset}");
        assert_eq!(fresh(1, dataset), None, "{dataset}");
    }
    let mover = Mover::new(
        zeros.clone(),
        zeros[0].groups(),
        Settings {
            switch: std::time::Duration::from_secs(10),
            ..Default::default()
        },
        Arc::new(SystemClock),
    );
    let ended = mover.rebalance();
    std::thread::sleep(std::time::Duration::from_millis(300));
    stop.store(true, Ordering::SeqCst);
    let mut written = 0;
    for task in load {
        let (count, refused, _) = task.join().unwrap();
        assert!(refused.is_empty(), "refused: {refused:?}");
        written += count;
    }
    assert!(written > 20, "{written} written across the raise");
    let mut moved: Vec<String> = ended
        .iter()
        .map(|e| match e {
            Ended::Moved(m) => {
                assert!(!m.by_hand && m.moved_by.is_empty(), "{m:?}");
                assert_eq!((m.from_group, m.to_group), (1, 1));
                m.partition.clone()
            }
            other => panic!("{other:?}"),
        })
        .collect();
    moved.sort();
    let mut wanted = datasets.clone();
    wanted.sort();
    assert_eq!(moved, wanted, "each of group 1's datasets moved home once");
    assert!(mover.rebalance().is_empty(), "a second sweep moves nothing");
    for dataset in &datasets {
        let (away, home) = (fresh(0, dataset).unwrap(), fresh(1, dataset).unwrap());
        assert!(home.head > away.head, "{dataset}: written on at home");
        // Every key written, read back through either member.
        for member in [&zero, &one] {
            let read = member
                .read(&Query {
                    datasets: vec![dataset.clone()],
                    data_type: DataType::Trade,
                    subjects: vec![subject.clone()],
                    kinds: vec![],
                    interval_ns: 0,
                    when: When::Latest { at_ns: 0 },
                    as_of_ns: 0,
                    published_as_of_ns: 0,
                    latest_only: false,
                })
                .unwrap();
            assert_eq!(
                read.len() as u64,
                home.head,
                "{dataset}: {} read",
                read.len()
            );
        }
    }
}

/// A range is moved home by a raised database count only once the lake's
/// rollout is complete (step 3d; sections 6.4 and 6.8; `lake_member`, ruled
/// 2026-11-15, item 9; PR CI 43656183991). The count alone was raised, the
/// release the same: a member started before it, reading one database, is
/// still seen. While it is, the control group's sweep moves nothing home --
/// that member would answer for a range that left its database as though
/// it held none of it. Once it is no longer seen within the interval, the
/// sweep moves each range home.
#[test]
fn m11_a_range_is_moved_home_only_once_every_member_seen_runs_the_sweeps_revision() {
    use meridian_changes::groups::Groups;
    use meridian_changes::members::revision;
    use meridian_lake::partition_moves::{Ended, Mover, Settings};
    let urls = two_databases();
    let dataset = (0..)
        .map(|n| unique(&format!("coinbase-1:rollout{n}")))
        .find(|dataset| meridian_changes::groups::owner(dataset, 2) == 1)
        .unwrap();
    let subject = unique("LCL-ROLLOUT");
    // One database: group 1 writes its dataset there.
    {
        let member = |group: u32| {
            let store = PostgresStore::connect(&urls[0], 4)
                .unwrap()
                .with_groups(Groups::new(group, 2).unwrap());
            store.adopt().unwrap();
            store
        };
        let (zero, one) = (member(0), member(1));
        zero.pin_owners().unwrap();
        one.record(
            &dataset,
            vec![trade(&dataset, "before", &subject, 10)],
            1_000,
        )
        .unwrap();
    }
    // Two databases: group 1's home is the second.
    let groups = Groups::new(0, 2).unwrap();
    let zeros: Vec<Arc<PostgresStore>> = urls
        .iter()
        .enumerate()
        .map(|(d, url)| {
            Arc::new(
                PostgresStore::connect(url, 4)
                    .unwrap()
                    .with_groups(groups)
                    .with_databases(2)
                    .in_database(d as u32),
            )
        })
        .collect();
    zeros[0].adopt().unwrap();
    for store in &zeros {
        store.read_owners().unwrap();
    }
    // Group 1's writer started at two databases, as the runtime starts it:
    // its epoch taken at its home, so a range moved home is moved under it.
    PostgresStore::connect(&urls[1], 2)
        .unwrap()
        .with_groups(Groups::new(1, 2).unwrap())
        .with_databases(2)
        .in_database(1)
        .adopt()
        .unwrap();
    // The same release, the count alone raised: one database against two.
    let (before, after) = (
        revision("0.1.540", 1, 2, None),
        revision("0.1.540", 2, 2, None),
    );
    let within = std::time::Duration::from_secs(6);
    let now = SystemClock.now_ns();
    // The sweep's own member, and one started before the raise, both seen.
    zeros[0]
        .member_seen("lake-0-new", &after, now, now)
        .unwrap();
    zeros[0]
        .member_seen("lake-group-1-old", &before, now - 60_000_000_000, now)
        .unwrap();
    let mover = Mover::new(
        zeros.clone(),
        groups,
        Settings {
            switch: std::time::Duration::from_secs(10),
            ..Default::default()
        },
        Arc::new(SystemClock),
    )
    .waiting_for_rollout(after.clone(), within);
    let tip = |d: usize| {
        PostgresStore::connect(&urls[d], 1)
            .unwrap()
            .range_tip(&dataset)
            .unwrap()
    };
    assert!(
        mover.rebalance().is_empty(),
        "held: a member seen lately reads one database"
    );
    assert_eq!(tip(1), None, "nothing of it moved to its home");
    // That member gone: its row last seen before the interval.
    zeros[0]
        .member_seen(
            "lake-group-1-old",
            &before,
            now - 60_000_000_000,
            SystemClock.now_ns() - within.as_nanos() as i64 - 1_000_000_000,
        )
        .unwrap();
    let ended = mover.rebalance();
    let moved: Vec<String> = ended
        .iter()
        .map(|e| match e {
            Ended::Moved(m) => m.partition.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        moved,
        std::slice::from_ref(&dataset),
        "moved home once it was gone"
    );
    assert!(tip(1).is_some_and(|home| home.head >= 1));
}

/// A range's move by hand to another database waits for the lake's rollout
/// too (step 3d; sections 6.4 and 6.8; ruled 2026-11-15): its reply said it
/// had begun, and it runs in the background once every member seen runs
/// the mover's revision. The count alone raised, a member reading one
/// database still seen: the move waits; that member gone, it runs.
#[test]
fn m11_a_ranges_move_by_hand_to_another_database_waits_for_the_rollout() {
    use meridian_changes::members::revision;
    use meridian_lake::partition_moves::{Asked, Ended};
    let a = across("m11-hand", true);
    let lower = a.lower();
    let (before, after) = (
        revision("0.1.540", 1, 2, None),
        revision("0.1.540", 2, 2, None),
    );
    let within = std::time::Duration::from_secs(6);
    let old_member = |seen_at: i64| {
        a.zeros[0]
            .member_seen("lake-group-1-old", &before, 1, seen_at)
            .unwrap()
    };
    old_member(SystemClock.now_ns());
    let mover = Arc::new(a.mover().waiting_for_rollout(after, within));
    let moving = {
        let (mover, lower) = (mover.clone(), lower.clone());
        std::thread::spawn(move || {
            assert!(mover.begin(&lower));
            mover.run(
                &lower,
                1,
                &Asked {
                    by_hand: true,
                    moved_by: "admin-1".into(),
                    reason: "moved by hand during a rollout".into(),
                },
            )
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert!(
        !moving.is_finished(),
        "the range's move waits while a member reads one database"
    );
    assert!(
        a.heads(&lower)[1].is_none(),
        "nothing of it in the other database yet"
    );
    // That member gone: its row last seen before the interval.
    old_member(SystemClock.now_ns() - within.as_nanos() as i64 - 1_000_000_000);
    let ended = moving.join().unwrap().unwrap();
    assert!(matches!(ended, Ended::Moved(_)), "{ended:?}");
    assert!(a.heads(&lower)[1].is_some(), "moved to database 1");
}

/// A writer group's place in the write-ahead stream, committed in the
/// first database before the lake's database count was raised (step 3d;
/// "lake loss is re-sent, never silent"): its member over two databases
/// reads it from where it was, carries it to its group's home, and so goes
/// on from it -- a run the stream dropped past it is a gap, never taken for
/// a group's first start -- with the runs it found there.
#[test]
fn a_groups_stream_position_is_carried_home_when_the_database_count_is_raised() {
    use meridian_changes::groups::Groups;
    use meridian_lake::store::{Dropped, WriteAheadBatch, WriteAheadPosition};
    let urls = two_databases();
    let key = unique("LAKE_WRITE_AHEAD.group-1");
    let run = Dropped {
        first: 3,
        last: 4,
        after_at_ns: 2,
        until_at_ns: 5,
        gap_id: unique("GAP-CARRIED"),
    };
    {
        let one = PostgresStore::connect(&urls[0], 2)
            .unwrap()
            .with_groups(Groups::new(1, 2).unwrap());
        one.adopt().unwrap();
        one.record_write_ahead(
            WriteAheadBatch {
                stream: key.clone(),
                after: None,
                through: WriteAheadPosition {
                    sequence: 7,
                    at_ns: 70,
                },
                dropped: vec![run.clone()],
                ..Default::default()
            },
            1_000,
        )
        .unwrap();
    }
    let groups = Groups::new(1, 2).unwrap();
    let stores: Vec<Arc<PostgresStore>> = urls
        .iter()
        .enumerate()
        .map(|(d, url)| {
            Arc::new(
                PostgresStore::connect(url, 2)
                    .unwrap()
                    .with_groups(groups)
                    .with_databases(2)
                    .in_database(d as u32),
            )
        })
        .collect();
    let one = meridian_lake::spread::Spread::new(stores.clone(), groups);
    one.adopt().unwrap();
    assert_eq!(stores[1].write_ahead_position(&key).unwrap(), None);
    let at = one
        .write_ahead_position(&key)
        .unwrap()
        .map(|at| at.sequence);
    assert_eq!(at, Some(7), "read from where it was");
    assert_eq!(
        stores[1]
            .write_ahead_position(&key)
            .unwrap()
            .map(|at| at.sequence),
        Some(7),
        "carried home"
    );
    let (found, _) = one.write_ahead_found(&key).unwrap();
    assert_eq!(found, vec![run]);
}

// ---- The correctness review of the moves (2026-11-14, c30d-m7) ----------

/// Two databases, a store of each of two writer groups over each, a
/// member (Spread) of each group, and a dataset of group 0's in database
/// 0 -- split into two halves where `split` -- with a row in each half.
struct Across {
    zeros: Vec<Arc<PostgresStore>>,
    ones: Vec<Arc<PostgresStore>>,
    zero: Arc<meridian_lake::spread::Spread>,
    one: Arc<meridian_lake::spread::Spread>,
    dataset: String,
    low: String,
    // Last, so its databases are dropped after the stores over them.
    urls: Vec<Scratch>,
}

fn across(tag: &str, split: bool) -> Across {
    across_owned(tag, split, 0)
}

/// [`across`], its dataset writer group `owner`'s by rendezvous, in that
/// group's own database.
fn across_owned(tag: &str, split: bool, owner: u32) -> Across {
    use meridian_changes::groups::Groups;
    let urls = two_databases();
    let stores = |group: u32| -> Vec<Arc<PostgresStore>> {
        let groups = Groups::new(group, 2).unwrap();
        urls.iter()
            .enumerate()
            .map(|(d, url)| {
                Arc::new(
                    PostgresStore::connect(url, 4)
                        .unwrap()
                        .with_groups(groups)
                        .with_databases(2)
                        .in_database(d as u32),
                )
            })
            .collect()
    };
    let (zeros, ones) = (stores(0), stores(1));
    let zero = Arc::new(meridian_lake::spread::Spread::new(
        zeros.clone(),
        zeros[0].groups(),
    ));
    let one = Arc::new(meridian_lake::spread::Spread::new(
        ones.clone(),
        ones[0].groups(),
    ));
    zero.adopt().unwrap();
    one.adopt().unwrap();
    let dataset = (0..)
        .map(|n| unique(&format!("coinbase-1:{tag}{n}")))
        .find(|dataset| meridian_changes::groups::owner(dataset, 2) == owner)
        .unwrap();
    let (low, high) = halves_of(tag);
    let owning = if owner == 0 { &zero } else { &one };
    owning
        .record(
            &dataset,
            vec![
                trade(&dataset, "l0", &low, 10),
                trade(&dataset, "h0", &high, 11),
            ],
            1_000,
        )
        .unwrap();
    if split {
        owning
            .split_partition(&dataset, "admin-1", "two halves", 1_100)
            .unwrap();
        owning
            .record(&dataset, vec![trade(&dataset, "split-l", &low, 12)], 1_200)
            .unwrap();
    }
    // Each database's ranges pinned, as the control partition's group does.
    for store in &zeros {
        store.pin_owners().unwrap();
    }
    for store in zeros.iter().chain(&ones) {
        store.read_owners().unwrap();
    }
    Across {
        urls,
        zeros,
        ones,
        zero,
        one,
        dataset,
        low,
    }
}

impl Across {
    fn lower(&self) -> String {
        format!("{}/0000-7fff", self.dataset)
    }

    fn mover(&self) -> meridian_lake::partition_moves::Mover {
        meridian_lake::partition_moves::Mover::new(
            self.zeros.clone(),
            self.zeros[0].groups(),
            meridian_lake::partition_moves::Settings {
                switch: std::time::Duration::from_secs(5),
                ..Default::default()
            },
            Arc::new(SystemClock),
        )
    }

    /// The range's rows in database `d`: (sequence, row key) each.
    fn rows(&self, d: usize, partition: &str) -> Vec<(i64, String)> {
        postgres::Client::connect(&self.urls[d], postgres::NoTls)
            .unwrap()
            .query(
                "SELECT sequence, row_key FROM lake_row WHERE partition = $1
                  ORDER BY sequence, row_key",
                &[&partition],
            )
            .unwrap()
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect()
    }

    /// The range's row in each database: (head, epoch, writer group).
    fn heads(&self, partition: &str) -> Vec<Option<(i64, i64, Option<i32>)>> {
        self.urls
            .iter()
            .map(|url| {
                postgres::Client::connect(url, postgres::NoTls)
                    .unwrap()
                    .query_opt(
                        "SELECT head, epoch, writer_group FROM lake_partition WHERE dataset = $1",
                        &[&partition],
                    )
                    .unwrap()
                    .map(|row| (row.get(0), row.get(1), row.get(2)))
            })
            .collect()
    }
}

/// C3: a range's switch between databases stopped between its two rows
/// -- the source let go, the target not opened -- never leaves two owners;
/// the control group's sweep opens the target, written on there alone.
#[test]
fn review_c3_a_range_switch_stopped_between_its_two_databases_leaves_one_owner() {
    use meridian_changes::moves::points;
    use meridian_lake::partition_moves::{Asked, Ended};
    let a = across("review-c3", true);
    let lower = a.lower();
    let mover = a.mover();
    points::arm(
        "lake.switch.between",
        &lower,
        Arc::new(|_| Err("stopped between the two databases".into())),
    );
    assert!(mover.begin(&lower));
    let ended = mover.run(&lower, 1, &Asked::default());
    points::disarm("lake.switch.between", &lower);
    let heads = a.heads(&lower);
    let top = heads.iter().flatten().map(|h| h.0).max().unwrap();
    let written: std::collections::BTreeSet<Option<i32>> = heads
        .iter()
        .flatten()
        .filter(|h| h.0 == top && h.1 != meridian_changes::moves::POINTER_EPOCH)
        .map(|h| h.2)
        .collect();
    assert!(written.len() <= 1, "two owners at {top}: {heads:?}");
    assert!(matches!(ended, Ok(Ended::Moved(_))), "{ended:?}");
    // The sweep opens the target; each member's write commits there once.
    mover.rebalance();
    for store in a.zeros.iter().chain(&a.ones) {
        store.read_owners().unwrap();
    }
    // Group 0's member hands it on, never commits it; group 1's commits.
    let handed = a.zero.record(
        &a.dataset,
        vec![trade(&a.dataset, "after-0", &a.low, 20)],
        2_000,
    );
    assert!(
        matches!(
            handed,
            Err(meridian_lake::store::StoreError::NotOwned { group: 1, .. })
        ),
        "{handed:?}"
    );
    for (n, at) in [(0, 2_100), (1, 2_200)] {
        a.one
            .record(
                &a.dataset,
                vec![trade(&a.dataset, &format!("after-{n}"), &a.low, 21 + n)],
                at,
            )
            .unwrap();
    }
    let heads = a.heads(&lower);
    let target = heads[1].unwrap();
    assert_eq!(target.2, Some(1), "{heads:?}");
    assert_ne!(
        target.1,
        meridian_changes::moves::POINTER_EPOCH,
        "opened: {heads:?}"
    );
    assert_eq!(target.0, top + 2, "{heads:?}");
    let (ours, theirs) = (a.rows(1, &lower), a.rows(0, &lower));
    assert!(
        theirs.iter().all(|(s, _)| *s <= top),
        "written in both: {theirs:?}"
    );
    assert_eq!(ours.last().map(|r| r.0), Some(top + 2), "{ours:?}");
}

/// A1: the source's commit made but its answer lost: the switch reads the
/// source's row again and goes on, never naming the source again on the
/// target -- one owner, the target.
#[test]
fn review_a1_a_lost_answer_to_a_ranges_source_commit_leaves_one_owner() {
    use meridian_changes::moves::points;
    use meridian_lake::partition_moves::Asked;
    let a = across("review-a1", true);
    let lower = a.lower();
    let mover = a.mover();
    points::arm(
        "lake.switch.committed",
        &lower,
        Arc::new(|_| Err("the commit's answer was lost".into())),
    );
    assert!(mover.begin(&lower));
    let ended = mover.run(&lower, 1, &Asked::default());
    points::disarm("lake.switch.committed", &lower);
    let heads = a.heads(&lower);
    assert_eq!(
        (heads[0].unwrap().2, heads[1].unwrap().2),
        (Some(1), Some(1)),
        "{heads:?} {ended:?}"
    );
}

/// C2: rows committed while a warm round copies leave no hole: each round
/// copies up to the sequence it read first, in one snapshot, and the switch
/// copies on from there.
#[test]
fn review_c2_rows_committed_during_a_warm_round_leave_no_hole() {
    use meridian_changes::moves::points;
    let a = across("review-c2", true);
    let lower = a.lower();
    let (source, target) = (a.zeros[0].clone(), a.zeros[1].clone());
    let warmed = target.copy_range_from(&source, &lower, 0).unwrap();
    let (zero, dataset, low) = (a.zero.clone(), a.dataset.clone(), a.low.clone());
    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    points::arm(
        "lake.copy.read",
        &lower,
        Arc::new(move |_| {
            if !fired.swap(true, Ordering::SeqCst) {
                zero.record(
                    &dataset,
                    vec![
                        trade(&dataset, "during-1", &low, 30),
                        trade(&dataset, "during-2", &low, 31),
                    ],
                    3_000,
                )
                .unwrap();
            }
            Ok(())
        }),
    );
    let warmed = target.copy_range_from(&source, &lower, warmed).unwrap();
    points::disarm("lake.copy.read", &lower);
    a.zero
        .record(
            &a.dataset,
            vec![trade(&a.dataset, "later", &a.low, 32)],
            3_100,
        )
        .unwrap();
    let warmed = target.copy_range_from(&source, &lower, warmed).unwrap();
    let switched = source
        .switch_range(
            &target,
            &lower,
            1,
            std::time::Duration::from_secs(5),
            false,
            warmed,
        )
        .unwrap();
    assert!(
        matches!(switched, meridian_lake::postgres::Switched::Moved { .. }),
        "{switched:?}"
    );
    assert_eq!(
        a.rows(1, &lower),
        a.rows(0, &lower),
        "the target's rows differ"
    );
}

/// C9: the switch's digest at N is each side's rows taken again, not a
/// copied value: a row changed in the target after its copy aborts the
/// move, cause digest, and the source still owns.
#[test]
fn review_c9_a_range_copy_changed_after_it_was_made_is_found_at_the_switch() {
    let a = across("review-c9", true);
    let lower = a.lower();
    a.zero
        .record(
            &a.dataset,
            vec![trade(&a.dataset, "more", &a.low, 40)],
            4_000,
        )
        .unwrap();
    let (source, target) = (a.zeros[0].clone(), a.zeros[1].clone());
    let warmed = target.copy_range_from(&source, &lower, 0).unwrap();
    let changed = postgres::Client::connect(&a.urls[1], postgres::NoTls)
        .unwrap()
        .execute(
            "UPDATE lake_row SET body = body || '\\x00'::bytea
              WHERE partition = $1 AND sequence = (SELECT min(sequence) FROM lake_row
                                                    WHERE partition = $1)",
            &[&lower],
        )
        .unwrap();
    assert!(changed >= 1);
    let switched = source
        .switch_range(
            &target,
            &lower,
            1,
            std::time::Duration::from_secs(5),
            false,
            warmed,
        )
        .unwrap();
    assert!(
        matches!(
            switched,
            meridian_lake::postgres::Switched::DigestDiffers { .. }
        ),
        "{switched:?}"
    );
    assert_eq!(
        a.heads(&lower)[0].unwrap().2,
        Some(0),
        "the source still owns"
    );
}

/// C8: a dataset moved by hand, then split: the ranges split from it stay
/// with the group it was moved to through the sweep.
#[test]
fn review_c8_a_dataset_moved_by_hand_then_split_stays_through_the_sweep() {
    use meridian_lake::partition_moves::{Asked, Ended};
    let a = across("review-c8", false);
    let mover = a.mover();
    assert!(mover.begin(&a.dataset));
    let ended = mover
        .run(
            &a.dataset,
            1,
            &Asked {
                by_hand: true,
                moved_by: "https://directory.example.org|8812".into(),
                reason: "to database 1".into(),
            },
        )
        .unwrap();
    assert!(matches!(&ended, Ended::Moved(_)), "{ended:?}");
    mover.log(&ended);
    for store in a.zeros.iter().chain(&a.ones) {
        store.read_owners().unwrap();
    }
    // Its database's timeline followed first, so its control's epoch is
    // its leader's whatever M1 does.
    a.one.follow_timeline().unwrap();
    a.one
        .split_partition(&a.dataset, "admin-1", "two halves", 5_000)
        .unwrap();
    let moved = mover.rebalance();
    assert!(moved.is_empty(), "the sweep moved {moved:?}");
    for child in [a.lower(), format!("{}/8000-ffff", a.dataset)] {
        assert_eq!(a.heads(&child)[1].unwrap().2, Some(1), "{child}");
    }
}

/// M1: a writer group past the first that leads its own database logs in
/// that database's control under its own epoch right after its takeover,
/// before any look at the timeline -- its control head raised with it.
#[test]
fn review_m1_a_group_leading_its_database_logs_control_right_after_its_takeover() {
    let a = across("review-m1", false);
    let dataset = (0..)
        .map(|n| unique(&format!("coinbase-1:m1x{n}")))
        .find(|dataset| meridian_changes::groups::owner(dataset, 2) == 1)
        .unwrap();
    a.one
        .record(&dataset, vec![trade(&dataset, "m", &a.low, 10)], 1_000)
        .unwrap();
    a.one
        .split_partition(&dataset, "admin-1", "two halves", 1_100)
        .unwrap();
}

/// C4: an answer of a write split across groups waits on the groups that
/// own its dataset's ranges as they are while it waits, never only those
/// its taker saw: a range moved to group 1 since the write was taken (its
/// taker saw no other group) holds the answer until group 1 commits -- here
/// it never does, so the answer is refused for load, never answered OK.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn review_c4_an_answer_waits_on_the_ranges_owners_as_they_are_now() {
    use meridian_domain::v1::{RecordObservationsReply, Watermark};
    use meridian_lake::partition_moves::{Asked, Mover, Settings};
    use meridian_lake::write_ahead::{settled, Wait};
    use prost::Message;
    let (_url, zero, one, dataset, lower, _upper, _low, _high) =
        tokio::task::block_in_place(|| two_groups_and_a_split("lake-c4"));
    let (zero, one) = (OffRuntime::new(zero), OffRuntime::new(one));
    tokio::task::block_in_place(|| {
        let mover = Mover::new(
            vec![zero.clone()],
            zero.groups(),
            Settings::default(),
            Arc::new(SystemClock),
        );
        assert!(mover.begin(&lower));
        mover.run(&lower, 1, &Asked::default()).unwrap();
        one.read_owners().unwrap();
    });
    let bus = Arc::new(meridian_bus::Bus::single(
        "lake-c4",
        Arc::new(meridian_bus::MemoryBackend::new()),
        Arc::new(SystemClock),
    ));
    let lake = OffRuntime::new(meridian_lake::service::serve_grouped(
        bus,
        zero.clone() as Arc<dyn Store>,
        Arc::new(SystemClock),
        zero.groups(),
    ));
    let answer = vec![(
        Some("_INBOX.c4".to_string()),
        Ok((
            "meridian.v1.RecordObservationsReply".to_string(),
            RecordObservationsReply {
                recorded: 1,
                watermark: Some(Watermark {
                    partitions: vec![meridian_pb::v1::PartitionSequence {
                        partition: dataset.clone(),
                        sequence: 0,
                    }],
                }),
                ..Default::default()
            }
            .encode_to_vec(),
        )),
    )];
    let answered = settled(
        &lake,
        "lake-c4-stream",
        answer,
        vec![Wait {
            at: 0,
            sequence: 7,
            groups: Vec::new(),
            dataset: dataset.clone(),
            ..Default::default()
        }],
    )
    .await;
    assert!(
        answered[0].1.is_err(),
        "answered before group 1, which owns a range of it now, committed its part"
    );
    // Their connections closed off the runtime's threads.
}

// ---- Part 12 (c30d-m12): the hold of a range's switch between databases --

/// A range holding 20,000 keys switched between databases under write load
/// (step 3d; section 6.8, "the hold is bounded, and nothing is refused
/// inside it"): its hold -- from the range's row taken to the switch's
/// answer -- stays under a second, no write is refused, and each part of
/// it is said. What was warmed is compared before the row is taken, so
/// under it only the rows since are copied and compared; the source's
/// latest versions of the range's keys are let go under it, with the range.
#[test]
fn m12_a_large_range_switches_between_databases_inside_a_short_hold() {
    use meridian_changes::groups::Groups;
    use meridian_lake::partition_moves::{Asked, Ended, Mover, Settings};
    const KEYS: usize = 20_000;
    let urls = two_databases();
    let stores = |group: u32| -> Vec<Arc<PostgresStore>> {
        let groups = Groups::new(group, 2).unwrap();
        urls.iter()
            .enumerate()
            .map(|(d, url)| {
                Arc::new(
                    PostgresStore::connect(url, 4)
                        .unwrap()
                        .with_groups(groups)
                        .with_databases(2)
                        .in_database(d as u32),
                )
            })
            .collect()
    };
    let (zeros, ones) = (stores(0), stores(1));
    let zero = Arc::new(meridian_lake::spread::Spread::new(
        zeros.clone(),
        zeros[0].groups(),
    ));
    let one = Arc::new(meridian_lake::spread::Spread::new(
        ones.clone(),
        ones[0].groups(),
    ));
    zero.adopt().unwrap();
    one.adopt().unwrap();
    let dataset = (0..)
        .map(|n| unique(&format!("coinbase-1:m{n}")))
        .find(|dataset| meridian_changes::groups::owner(dataset, 2) == 0)
        .unwrap();
    let (low, high) = halves_of("lake-m12");
    zero.record(
        &dataset,
        vec![
            trade(&dataset, "l0", &low, 10),
            trade(&dataset, "h0", &high, 11),
        ],
        1_000,
    )
    .unwrap();
    zero.split_partition(&dataset, "admin-1", "two halves", 1_100)
        .unwrap();
    let built = std::time::Instant::now();
    for batch in 0..KEYS / 1_000 {
        let rows = (0..1_000)
            .map(|n| {
                trade(
                    &dataset,
                    &format!("bulk-{batch}-{n}"),
                    &low,
                    20 + (batch * 1_000 + n) as i64,
                )
            })
            .collect();
        zero.record(&dataset, rows, 1_200 + batch as i64).unwrap();
    }
    println!("m12: {KEYS} keys recorded in {:?}", built.elapsed());
    zeros[0].pin_owners().unwrap();
    for store in zeros.iter().chain(&ones) {
        store.read_owners().unwrap();
    }
    let lower = format!("{dataset}/0000-7fff");
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let moving = writing_range(
        zero.clone() as Arc<dyn Store>,
        one.clone() as Arc<dyn Store>,
        dataset.clone(),
        low.clone(),
        "l",
        stop.clone(),
    );
    std::thread::sleep(std::time::Duration::from_millis(300));
    let mover = Mover::new(
        zeros.clone(),
        zeros[0].groups(),
        Settings {
            switch: std::time::Duration::from_secs(10),
            ..Default::default()
        },
        Arc::new(SystemClock),
    );
    assert!(mover.begin(&lower));
    let began = std::time::Instant::now();
    let ended = mover
        .run(
            &lower,
            1,
            &Asked {
                by_hand: true,
                moved_by: "https://directory.example.org|8812".into(),
                reason: "the lower half to database 1".into(),
            },
        )
        .unwrap();
    let took = began.elapsed();
    let times = mover.last_switch().expect("a switch was made");
    for store in ones.iter() {
        store.read_owners().unwrap();
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    stop.store(true, Ordering::SeqCst);
    let (moved_rows, refused, moved_wait) = moving.join().unwrap();
    println!(
        "m12: warm and switch {took:?}; its switch {times:?}; the moved range's longest wait \
         {moved_wait:?}; {moved_rows} rows written under it"
    );
    assert!(refused.is_empty(), "{refused:?}");
    assert!(matches!(ended, Ended::Moved(_)), "{ended:?}");
    assert!(moved_rows > 0);
    // Every key of the range is the target's, each once.
    let keys: i64 = postgres::Client::connect(&urls[1], postgres::NoTls)
        .unwrap()
        .query_one(
            "SELECT count(*) FROM lake_latest WHERE dataset = $1 AND partition = $2",
            &[&dataset, &lower],
        )
        .unwrap()
        .get(0);
    assert!(keys as usize >= KEYS, "{keys} keys in the target");
    assert!(
        times.held < std::time::Duration::from_secs(1),
        "the switch held the range {:?}: {times:?}",
        times.held
    );
}

/// Step 3d, stage T (section 15; `writer_group` and `mode` approved
/// 2026-11-15): which of the store's members a writer told to stop may stop
/// for, read from the members' own rows look after look. Only another
/// group's writer that serves, said again since the look before, and that
/// will not stop with it: not there at its first look, or running another
/// revision. Never the writer its own upgrade is about to stop too, a
/// standby, a reader, its own group's, one holding its lease that does not
/// serve yet, one no longer said, or a member of the release before, which
/// says neither its group nor its mode. No time is compared with another
/// member's clock (the review's finding 10): every member here stamps its
/// rows by a clock of its own, hours apart.
#[test]
fn t_a_writer_told_to_stop_stops_only_for_another_groups_writer_serving_anew() {
    use meridian_changes::members::Watch;
    let (_schema, store) = isolated("t_member");
    let revision = meridian_changes::members::revision("0.1.900", 1, 2, None);
    let other = meridian_changes::members::revision("0.1.901", 1, 2, None);
    let hour = 3_600_000_000_000_i64;
    let mut tick = 0_i64;
    // Each member says its row by its own clock: `skew` hours from the rest.
    let mut says =
        |member: &str, revision: &str, skew: i64, started: i64, group: u32, mode: &str| {
            tick += 1;
            store
                .member_says(
                    member,
                    revision,
                    skew * hour + started,
                    skew * hour + 1_000 + tick,
                    group,
                    mode,
                )
                .unwrap();
        };
    let mut watch = Watch::new(1, &revision, false);
    let look = |watch: &mut Watch| watch.serves(&store.members_but("one-old").unwrap());
    // The first group's writer of before the upgrade, about to stop too,
    // its clock five hours ahead: "started" long after any tell.
    says("zero-old", &revision, 5, 1, 0, "writer");
    says("one-old", &revision, 0, 1, 1, "writer");
    // A member of the release before: neither group nor mode said.
    store.member_seen("zero-older", &other, 1, 1_000).unwrap();
    assert!(!look(&mut watch), "the first look notes who is there");
    says("zero-old", &revision, 5, 1, 0, "writer");
    store.member_seen("zero-older", &other, 1, 1_001).unwrap();
    assert!(
        !look(&mut watch),
        "the writer about to stop too, and the release before's member"
    );
    // Its own group's new writer, a standby and a reader of the first group.
    says("one-new", &revision, -3, 9, 1, "writer");
    says("zero-standby", &revision, 0, 9, 0, "standby");
    says("zero-reader", &revision, 0, 9, 0, "reader");
    assert!(
        !look(&mut watch),
        "none is another group's writer serving anew"
    );
    // The first group's new member: its lease held, its topics not served.
    says("zero-new", &revision, -7, 9, 0, "");
    assert!(!look(&mut watch), "a writer that does not serve yet");
    says("zero-new", &revision, -7, 9, 0, "writer");
    assert!(
        look(&mut watch),
        "the first group's writer serves anew, its clock seven hours behind"
    );
    // Not said again: stopped, or cut off.
    assert!(
        !look(&mut watch),
        "a writer not said again since the look before"
    );
    // Running another revision, there from the first look: the roll reached
    // that group first.
    let mut rolled = Watch::new(1, &revision, false);
    says("zero-rolled", &other, 2, 1, 0, "writer");
    assert!(!look(&mut rolled));
    assert!(!look(&mut rolled), "not said again");
    says("zero-rolled", &other, 2, 1, 0, "writer");
    assert!(
        look(&mut rolled),
        "another group's writer at another revision, said again"
    );
    // Where no lease fences its group, its own group's new writer too.
    let mut unleased = Watch::new(1, &revision, true);
    assert!(!look(&mut unleased));
    says("one-newer", &revision, 0, 20, 1, "writer");
    assert!(look(&mut unleased), "its own group's replacement serves");
}

/// Stage T's review, finding 9: a member's row never says one process's
/// start beside another's mode. Its container restarted under the same
/// name, the new process says nothing of what it serves as until it
/// serves; before, its row kept the old process's `writer` beside the new
/// start, and a writer told to stop stopped for a member serving nothing.
#[test]
fn t_a_member_started_again_says_nothing_of_the_process_before() {
    let (_schema, store) = isolated("t_member");
    let revision = meridian_changes::members::revision("0.1.900", 1, 2, None);
    let mode = |member: &str| {
        store
            .members_but("nobody")
            .unwrap()
            .into_iter()
            .find(|said| said.member == member)
            .map(|said| (said.started_at_ns, said.mode))
            .unwrap()
    };
    store
        .member_says("zero", &revision, 10, 100, 0, "writer")
        .unwrap();
    // The same process, seen again: what it serves as is kept.
    store.member_seen("zero", &revision, 10, 102).unwrap();
    assert_eq!(mode("zero"), (10, "writer".to_string()));
    // Another process under its name: nothing said of what it serves as.
    store.member_seen("zero", &revision, 20, 104).unwrap();
    assert_eq!(mode("zero"), (20, String::new()));
    // And the one statement says both: never the old mode beside the new start.
    store
        .member_says("zero", &revision, 10, 105, 0, "writer")
        .unwrap();
    store
        .member_says("zero", &revision, 30, 106, 0, "")
        .unwrap();
    assert_eq!(mode("zero"), (30, String::new()));
}

/// Stage T's review, finding 3 (ruled 2026-11-15): two writers of one
/// group, the second started while the first serves on, told to stop. No
/// lease fences the group -- the tolerant default -- so the second adopts
/// the epoch the first writes under and neither is fenced: both committed,
/// side by side. Told to stop, the first commits nothing more; only the
/// second commits. No epoch is raised and no takeover recorded.
#[test]
fn t_a_writer_told_to_stop_commits_nothing_beside_its_replacement() {
    use meridian_changes::groups::Groups;
    use meridian_lake::store::StoreError;
    let (url, migrated) = isolated("t_two_writers");
    drop(migrated);
    let dataset = (0..)
        .map(|n| unique(&format!("coinbase-1:w{n}")))
        .find(|dataset| meridian_changes::groups::owner(dataset, 2) == 1)
        .unwrap();
    let member = || {
        PostgresStore::connect(&url, 2)
            .unwrap()
            .with_groups(Groups::new(1, 2).unwrap())
    };
    let subject = unique("LCL-BTC");
    let row = |key: &str| vec![close(&dataset, key, &subject, "2026-10-01", 1)];
    let first = member();
    let epoch = first.adopt().unwrap();
    first.record(&dataset, row("a"), 10).unwrap();
    let mut admin = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let mut read = || -> (i64, i64) {
        let row = admin
            .query_one(
                "SELECT (SELECT count(*) FROM lake_row WHERE dataset = $1),
                        (SELECT count(*) FROM lake_writer_took_over)",
                &[&dataset],
            )
            .unwrap();
        (row.get(0), row.get(1))
    };
    let (_, took) = read();
    // Told to stop, it serves on; its replacement starts beside it.
    first.stop_committing();
    let second = member();
    assert_eq!(
        second.adopt().unwrap(),
        epoch,
        "the same epoch: no lease, nobody fenced"
    );
    second
        .record(&dataset, row("b"), 20)
        .expect("the second commits");
    let refused = first.record(&dataset, row("c"), 30);
    assert!(
        matches!(&refused, Err(StoreError::Fenced(fenced)) if fenced.mine == meridian_changes::Fenced::STOPPED),
        "the first commits nothing more: {refused:?}"
    );
    second
        .record(&dataset, row("d"), 40)
        .expect("the second commits on");
    assert!(first.record(&dataset, row("e"), 50).is_err());
    assert_eq!(
        read(),
        (3, took),
        "a, b and d: only the second committed, and no takeover was recorded"
    );
}

/// What the lake's record breaks of "every epoch above the first raised by
/// a recorded takeover, the takeovers one per epoch with none skipped"
/// (ruling 1105; e2e/failover/run.py `properties`), in the database at
/// `url`: each row recorded, and each range held, at an epoch no takeover
/// recorded, and a takeover count short of the epochs it spans.
fn unrecorded_lake_epochs(url: &str) -> Vec<String> {
    let mut admin = postgres::Client::connect(url, postgres::NoTls).unwrap();
    let mut broken = Vec::new();
    for (what, table) in [
        ("rows recorded", "lake_row"),
        ("ranges held", "lake_partition"),
    ] {
        for row in admin
            .query(
                &format!(
                    "SELECT epoch, count(*) FROM {table}
                      WHERE epoch > 1 AND epoch NOT IN (SELECT epoch FROM lake_writer_took_over)
                      GROUP BY epoch ORDER BY epoch"
                ),
                &[],
            )
            .unwrap()
        {
            broken.push(format!(
                "{} {what} at epoch {}, which no takeover recorded",
                row.get::<_, i64>(1),
                row.get::<_, i64>(0)
            ));
        }
    }
    let row = admin
        .query_one(
            "SELECT count(*), coalesce(max(epoch) - min(epoch) + 1, 0) FROM lake_writer_took_over",
            &[],
        )
        .unwrap();
    let (recorded, spanned): (i64, i64) = (row.get(0), row.get(1));
    if recorded != spanned {
        broken.push(format!(
            "{recorded} takeovers recorded over {spanned} epochs"
        ));
    }
    broken
}

/// A range moved to a writer group whose writer has not taken over in its
/// database records nothing at an epoch no takeover recorded (the book's
/// CI 43820357549, reconfigure, "groups raised"; the lake's moves made an
/// epoch the same way). The count raised from one group to two: group 0's
/// writer starts again and rebalances before group 1's writer has started.
/// A move never makes an epoch: group 1 is not ready, so the range stays
/// group 0's; once its writer has taken over -- recorded -- the next sweep
/// moves it under that epoch, and group 1 records there.
#[test]
fn a_range_moved_to_a_group_whose_writer_has_not_taken_over_records_nothing_at_an_unrecorded_epoch()
{
    use meridian_changes::groups::Groups;
    use meridian_lake::partition_moves::Ended;
    let (url, dataset, subject, zero, mover) = lake_raised_to_two("lake-unrecorded");
    // The sweep at group 0's start, before group 1's writer has started.
    let early = mover.rebalance();
    let unready = zero.range_tip(&dataset).unwrap().unwrap().writer_group;
    // Group 1's writer starts, and the next sweep runs.
    let one = PostgresStore::connect(&url, 4)
        .unwrap()
        .with_groups(Groups::new(1, 2).unwrap());
    let epoch = one.adopt().unwrap();
    let ended = mover.rebalance();
    // Group 1 records into its range.
    one.read_owners().unwrap();
    one.record(
        &dataset,
        vec![trade(&dataset, "after", &subject, 20)],
        2_000,
    )
    .unwrap();
    // Nothing recorded or held at an epoch no takeover recorded.
    assert_eq!(unrecorded_lake_epochs(&url), Vec::<String>::new());
    // Nothing moved before group 1's writer took over.
    assert!(
        early.is_empty(),
        "moved before group 1's writer took over: {early:?}"
    );
    assert_eq!(unready, Some(0));
    // Then moved, under the epoch its takeover recorded, and recorded.
    match &ended[..] {
        [Ended::Moved(m)] => {
            assert_eq!((m.from_group, m.to_group, m.epoch), (0, 1, epoch), "{m:?}")
        }
        other => panic!("{other:?}"),
    }
    let tip = zero.range_tip(&dataset).unwrap().unwrap();
    assert_eq!((tip.writer_group, tip.epoch), (Some(1), epoch));
}

/// A lake at a schema of its own, started at one writer group -- its
/// writer's takeover recorded, a dataset of group 1's at two groups
/// recorded and its range pinned to group 0 -- then raised to two: group
/// 0's writer started again, its epoch adopted, and its mover, whose switch
/// is bound by 2 s; group 1's writer not started. Says the database's URL,
/// the dataset, its subject, group 0's store and the mover.
fn lake_raised_to_two(
    tag: &str,
) -> (
    Scratch,
    String,
    String,
    Arc<PostgresStore>,
    meridian_lake::partition_moves::Mover,
) {
    use meridian_changes::groups::{owner, Groups};
    use meridian_lake::partition_moves::{Mover, Settings};
    let (url, _migrated) = isolated(tag);
    let dataset = (0..)
        .map(|n| unique(&format!("coinbase-1:{tag}{n}")))
        .find(|dataset| owner(dataset, 2) == 1)
        .unwrap();
    let subject = unique("LCL-UNREC");
    let alone = PostgresStore::connect(&url, 2)
        .unwrap()
        .with_groups(Groups::new(0, 1).unwrap());
    alone
        .take_over(&meridian_changes::TookOver::unnamed(
            meridian_changes::LEASE_FREE,
        ))
        .unwrap();
    alone
        .record(
            &dataset,
            vec![trade(&dataset, "before", &subject, 10)],
            1_000,
        )
        .unwrap();
    alone.pin_owners().unwrap();
    let zero = Arc::new(
        PostgresStore::connect(&url, 4)
            .unwrap()
            .with_groups(Groups::new(0, 2).unwrap()),
    );
    zero.adopt().unwrap();
    zero.read_owners().unwrap();
    let mover = Mover::new(
        vec![zero.clone()],
        zero.groups(),
        Settings {
            switch: std::time::Duration::from_secs(2),
            ..Default::default()
        },
        Arc::new(SystemClock),
    );
    (url, dataset, subject, zero, mover)
}

/// An epoch an earlier release's move made for a writer group with no
/// takeover recorded (7bbbf608 and before) is recorded as its writer's own
/// when that writer adopts it, before anything is recorded under it (review
/// t7, M1), named by the member that adopted it, and logged in the control
/// partition as a takeover is.
#[test]
fn an_epoch_an_earlier_range_move_made_with_no_takeover_is_recorded_before_anything_under_it() {
    use meridian_changes::groups::Groups;
    use meridian_lake::partition_moves::{Asked, Ended};
    meridian_changes::set_member("lake-t7-member");
    let (url, dataset, subject, zero, mover) = lake_raised_to_two("lake-minted");
    // Group 1's row as an earlier release's move made it: one past every
    // epoch, no takeover recorded.
    let mut admin = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let made: i64 = admin
        .query_one(
            "INSERT INTO lake_writer_group (writer_group, epoch, origin)
             SELECT 1, greatest((SELECT max(epoch) FROM lake_partition),
                                (SELECT max(epoch) FROM lake_change_head)) + 1, ''
             RETURNING epoch",
            &[],
        )
        .unwrap()
        .get(0);
    // The range moved to group 1 under it, then group 1's writer starts and
    // records.
    assert!(mover.begin(&dataset));
    let ended = mover.run(&dataset, 1, &Asked::default()).unwrap();
    assert!(
        matches!(&ended, Ended::Moved(m) if m.epoch == made as u64),
        "{ended:?}"
    );
    let one = PostgresStore::connect(&url, 4)
        .unwrap()
        .with_groups(Groups::new(1, 2).unwrap());
    let epoch = one.adopt().unwrap();
    one.read_owners().unwrap();
    one.record(
        &dataset,
        vec![trade(&dataset, "after", &subject, 20)],
        2_000,
    )
    .unwrap();
    assert_eq!(unrecorded_lake_epochs(&url), Vec::<String>::new());
    assert_eq!(epoch, made as u64, "adopted, not raised again");
    let row = admin
        .query_one(
            "SELECT member, partitions FROM lake_writer_took_over WHERE epoch = $1",
            &[&made],
        )
        .unwrap();
    let (member, partitions): (String, Vec<String>) = (row.get(0), row.get(1));
    assert_eq!(
        member, "lake-t7-member",
        "named by the member that adopted it"
    );
    assert!(partitions.contains(&dataset), "{partitions:?}");
    let logged: i64 = admin
        .query_one(
            "SELECT count(*) FROM lake_control_change WHERE kind = $1",
            &[&meridian_changes::records::TOOK_OVER],
        )
        .unwrap()
        .get(0);
    assert_eq!(
        logged, 2,
        "the first takeover and this one, each logged in control"
    );
    drop(zero);
}

/// Writer groups lowered from two to one in tolerant -- no lease taken, no
/// new database timeline raising every epoch -- and the one writer left
/// writes what group 1 held (the reconfigure check's "groups lowered";
/// c5r2's second finding): at its start it takes over every range held at
/// an epoch not the control partition's, as several groups' adopt does,
/// recorded with why, and started again it adopts that epoch. A store a
/// tool opens at one group, its epoch adopted at its first batch, takes
/// nothing over: group 1's writer, still writing, is not fenced by it
/// (ruled 2026-11-18). The one writer's takeover is: group 1's writer, not
/// stopped yet, begins no dataset under its old epoch and sees the takeover
/// in its own group's row (review c5r3, M1).
#[test]
fn a_count_lowered_to_one_writes_what_group_one_held_on_the_same_timeline() {
    use meridian_changes::groups::{owner, Groups};
    let (url, dataset, subject, zero, mover) = lake_raised_to_two("lake-lowered-same");
    let one = PostgresStore::connect(&url, 4)
        .unwrap()
        .with_groups(Groups::new(1, 2).unwrap());
    let held = one.adopt().unwrap();
    mover.rebalance();
    one.read_owners().unwrap();
    one.record(&dataset, vec![trade(&dataset, "one", &subject, 20)], 2_000)
        .unwrap();
    assert_eq!(
        zero.range_tip(&dataset).unwrap().unwrap().writer_group,
        Some(1)
    );
    // A tool's store at one group, adopting at its first batch: group 1's
    // writer goes on writing.
    let tool = PostgresStore::connect(&url, 2).unwrap();
    let elsewhere = (0..)
        .map(|n| unique(&format!("coinbase-1:lake-lowered-tool{n}")))
        .find(|dataset| owner(dataset, 2) == 0)
        .unwrap();
    tool.record(
        &elsewhere,
        vec![trade(&elsewhere, "tool", &subject, 25)],
        2_500,
    )
    .unwrap();
    let still = one.record(
        &dataset,
        vec![trade(&dataset, "still", &subject, 26)],
        2_600,
    );
    assert!(
        still.is_ok(),
        "a tool's store fenced group 1's writer: {still:?}"
    );
    drop((zero, mover, tool));
    // Lowered: the first group's writer alone, started again, no lease;
    // group 1's writer not stopped yet.
    let alone = PostgresStore::connect(&url, 2)
        .unwrap()
        .with_groups(Groups::new(0, 1).unwrap());
    let epoch = alone.adopt().unwrap();
    let recorded = alone.record(
        &dataset,
        vec![trade(&dataset, "alone", &subject, 30)],
        3_000,
    );
    assert!(
        recorded.is_ok(),
        "the one writer left was refused what group 1 held: {recorded:?}"
    );
    // Group 1's writer, still running, is fenced by the takeover in its own
    // group's row too (review c5r3, M1): it begins no dataset under its old
    // epoch, its epoch's watch sees the takeover, and the one writer writes
    // the dataset it would have begun.
    let begun = (0..)
        .map(|n| unique(&format!("coinbase-1:lake-lowered-new{n}")))
        .find(|dataset| owner(dataset, 2) == 1)
        .unwrap();
    let stale = one.record(&begun, vec![trade(&begun, "stale", &subject, 31)], 3_100);
    assert!(
        matches!(stale, Err(meridian_lake::store::StoreError::Fenced(_))),
        "group 1's writer began a dataset under the epoch taken over: {stale:?}"
    );
    assert!(
        one.head_epoch().unwrap() > one.epoch(),
        "group 1's writer does not see the takeover: its row {} and its epoch {}",
        one.head_epoch().unwrap(),
        one.epoch()
    );
    let taken = alone.record(&begun, vec![trade(&begun, "alone", &subject, 32)], 3_200);
    assert!(
        taken.is_ok(),
        "the one writer was refused a dataset group 1's writer began: {taken:?}"
    );
    drop(one);
    assert!(epoch > held, "taken over past group 1's {held}: {epoch}");
    assert_eq!(unrecorded_lake_epochs(&url), Vec::<String>::new());
    let mut admin = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let why: String = admin
        .query_one(
            "SELECT why FROM lake_writer_took_over WHERE epoch = $1",
            &[&(epoch as i64)],
        )
        .unwrap()
        .get(0);
    assert_eq!(why, meridian_changes::groups::ASSIGNED);
    // Started again with every range its own: adopted, not raised again.
    drop(alone);
    let again = PostgresStore::connect(&url, 2)
        .unwrap()
        .with_groups(Groups::new(0, 1).unwrap());
    assert_eq!(again.adopt().unwrap(), epoch);
}

/// A range's move by hand to a writer group whose writer has not taken
/// over in its database waits at the gate, and aborts for lag past
/// moves.switchSeconds with nothing held: the source still owns it, and
/// nothing is at an epoch no takeover recorded (review t7, L1). Its writer
/// taking over while the gate waits, the move switches under that
/// takeover's epoch.
#[test]
fn a_range_move_by_hand_to_a_group_not_ready_waits_then_aborts_for_lag_or_moves_once_it_takes_over()
{
    use meridian_changes::groups::Groups;
    use meridian_lake::partition_moves::{Asked, Ended};
    let (url, dataset, _subject, zero, mover) = lake_raised_to_two("lake-gate");
    let asked = Asked {
        by_hand: true,
        moved_by: "https://directory.example.org|8812".into(),
        reason: "group 1 is to write it".into(),
    };
    let began = std::time::Instant::now();
    assert!(mover.begin(&dataset));
    let ended = mover.run(&dataset, 1, &asked).unwrap();
    let waited = began.elapsed();
    match &ended {
        Ended::Aborted(aborted, None) => assert_eq!(
            aborted.cause,
            meridian_domain::v1::MoveAbortCause::Lag as i32,
            "{aborted:?}"
        ),
        other => panic!("{other:?}"),
    }
    assert!(waited >= std::time::Duration::from_secs(2), "{waited:?}");
    assert_eq!(
        zero.range_tip(&dataset).unwrap().unwrap().writer_group,
        Some(0)
    );
    assert_eq!(unrecorded_lake_epochs(&url), Vec::<String>::new());
    // Group 1's writer takes over while the gate waits: moved under its
    // epoch.
    let taking = {
        // Its text: a thread a failed test leaves running never holds the
        // Scratch past the test (review c5p L2).
        let url = url.to_string();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(500));
            PostgresStore::connect(&url, 2)
                .unwrap()
                .with_groups(Groups::new(1, 2).unwrap())
                .adopt()
                .unwrap()
        })
    };
    assert!(mover.begin(&dataset));
    let ended = mover.run(&dataset, 1, &asked).unwrap();
    let epoch = taking.join().unwrap();
    assert!(
        matches!(&ended, Ended::Moved(m) if m.epoch == epoch && m.to_group == 1),
        "{ended:?}"
    );
    assert_eq!(
        zero.range_tip(&dataset).unwrap().unwrap().writer_group,
        Some(1)
    );
    assert_eq!(unrecorded_lake_epochs(&url), Vec::<String>::new());
}

// ── A plugin shard's epoch fence (step 3d, checkpoint 5, item 32) ───────────

/// A take of the Lease `uid-1` at `transitions`, with no stream.
fn lease(transitions: u64) -> meridian_changes::shard_fence::Taking {
    meridian_changes::shard_fence::Taking {
        lease_transitions: transitions,
        lease_uid: "uid-1".into(),
        admit: None,
    }
}

fn took(instance: &str, shard: u32) -> meridian_domain::v1::ShardTookOver {
    meridian_domain::v1::ShardTookOver {
        instance: instance.to_string(),
        shard,
        holder: "feed-pod-b_1".into(),
        previous_holder: "feed-pod-a_1".into(),
        reason: meridian_domain::v1::TakeoverReason::LeaseLapsed as i32,
        took_over_at_ns: 9_000,
        ..Default::default()
    }
}

/// A write stamped with a shard's epoch, checked in its own transaction
/// against `lake_shard_fence`: lower is refused and records nothing, equal
/// passes, higher raises -- by `record_parts`, by a write-ahead batch, and
/// for a dataset whose licence keeps nothing. A take raises past the epoch
/// held and the Lease's transitions, and is recorded in the control
/// partition.
#[test]
fn a_write_under_a_lower_shard_epoch_is_refused_an_equal_passes_a_higher_raises() {
    use meridian_lake::store::{ShardStamp, StoreError, WriteAheadBatch, WriteAheadPosition};
    let store = store();
    let instance = unique("feed");
    let dataset = unique("feed:prices");
    let subject = unique("LCL-BTC");
    let stamp = |epoch: u64| ShardStamp::of(&instance, 1, epoch);
    let fenced = |done: &Result<meridian_lake::store::Recorded, StoreError>| {
        matches!(done, Err(StoreError::ShardFenced(_)))
    };

    let first = store.take_shard(took(&instance, 1), &lease(0)).unwrap();
    assert_eq!(first.epoch, 1, "a shard never taken: epoch 1");
    store.record_shard_take(&first).unwrap();

    // Equal passes.
    let done = store.record_parts(
        &dataset,
        vec![vec![close(&dataset, "a", &subject, "2026-11-16", 1)]],
        &[stamp(1)],
        1_000,
    );
    assert_eq!(done[0].as_ref().unwrap().recorded, 1);
    // Higher raises, and passes.
    let done = store.record_parts(
        &dataset,
        vec![vec![close(&dataset, "b", &subject, "2026-11-16", 2)]],
        &[stamp(3)],
        2_000,
    );
    assert_eq!(done[0].as_ref().unwrap().recorded, 1);
    // Lower is refused, with the words, and records nothing; a part beside
    // it with no stamp passes.
    let head = store.heads().unwrap()[&dataset];
    let done = store.record_parts(
        &dataset,
        vec![
            vec![close(&dataset, "c", &subject, "2026-11-16", 3)],
            vec![close(&dataset, "d", &subject, "2026-11-16", 4)],
        ],
        &[stamp(2), None],
        3_000,
    );
    assert!(fenced(&done[0]), "{:?}", done[0]);
    assert_eq!(
        done[0].as_ref().unwrap_err().to_string(),
        meridian_changes::shard_fence::WORDS
    );
    assert_eq!(done[1].as_ref().unwrap().recorded, 1);
    assert_eq!(
        store.heads().unwrap()[&dataset],
        head + 1,
        "only the unstamped part"
    );

    // A write-ahead batch: the lower part refused alone, the rest and the
    // position committed.
    let stream = unique("STREAM");
    let at = WriteAheadPosition {
        sequence: 2,
        at_ns: 4_000,
    };
    let done = store
        .record_write_ahead(
            WriteAheadBatch {
                stream: stream.clone(),
                after: None,
                through: at,
                parts: vec![
                    (
                        dataset.clone(),
                        vec![close(&dataset, "e", &subject, "2026-11-16", 5)],
                    ),
                    (
                        dataset.clone(),
                        vec![close(&dataset, "f", &subject, "2026-11-16", 6)],
                    ),
                ],
                shards: vec![stamp(2), stamp(3)],
                ..Default::default()
            },
            4_000,
        )
        .unwrap();
    assert!(fenced(&done[0]), "{:?}", done[0]);
    assert_eq!(done[1].as_ref().unwrap().recorded, 1);
    assert_eq!(store.write_ahead_position(&stream).unwrap(), Some(at));

    // A dataset whose licence keeps nothing: served, fenced the same.
    let served = store.serve_unkept(
        &dataset,
        vec![close(&dataset, "g", &subject, "2026-11-16", 7)],
        &["reporting-1".to_string()],
        stamp(2).as_ref(),
        5_000,
    );
    assert!(fenced(&served), "{served:?}");

    // A take raises past the epoch held, and past the Lease's transitions;
    // each is recorded once every group raised it (review c5a M4).
    let take = |shard, transitions| {
        let taken = store
            .take_shard(took(&instance, shard), &lease(transitions))
            .unwrap();
        store.record_shard_take(&taken).unwrap();
        taken.epoch
    };
    assert_eq!(take(1, 0), 4);
    assert_eq!(take(1, 9), 9);
    // Another shard of the instance is its own.
    assert_eq!(take(0, 0), 1);
    // Each take is its own record in the control partition, with its epoch.
    let recorded: Vec<u64> = store
        .control_entries()
        .unwrap()
        .into_iter()
        .filter(|(kind, _, _)| kind == "meridian.v1.ShardTookOver")
        .filter_map(|(_, body, _)| {
            use prost::Message as _;
            meridian_domain::v1::ShardTookOver::decode(body.as_slice()).ok()
        })
        .filter(|took| took.instance == instance)
        .map(|took| took.epoch)
        .collect();
    assert_eq!(recorded, vec![1, 4, 9, 1]);
    // A raise heard from the control writer: never down.
    assert_eq!(store.raise_shard(&instance, 1, 5, None).unwrap(), 9);
    assert_eq!(store.raise_shard(&instance, 1, 12, None).unwrap(), 12);
    let done = store.record_parts(
        &dataset,
        vec![vec![close(&dataset, "h", &subject, "2026-11-16", 8)]],
        &[stamp(9)],
        6_000,
    );
    assert!(fenced(&done[0]), "{:?}", done[0]);
}

/// Review c5a2 N1, N2: every fence transaction commits on the synchronous
/// standby, whatever the lake's connections default to (they commit
/// locally): the take's numbering, a group's raise and its stream's place,
/// and the raise to database 0's epochs.
#[test]
fn a_shard_s_fence_commits_on_the_synchronous_standby() {
    use meridian_changes::shard_fence::{raise_to, seat, take, Admit, Held, LAKE};
    let url = std::env::var("MERIDIAN_TEST_DATABASE_URL").expect("MERIDIAN_TEST_DATABASE_URL");
    let joined = if url.contains('?') { "&" } else { "?" };
    let local = format!("{url}{joined}options=-c%20synchronous_commit%3Dlocal");
    let mut client = postgres::Client::connect(&local, postgres::NoTls).unwrap();
    let instance = unique("feed");
    let setting = |tx: &mut postgres::Transaction<'_>| -> String {
        tx.query_one("SHOW synchronous_commit", &[]).unwrap().get(0)
    };
    let mut tx = client.transaction().unwrap();
    assert_eq!(setting(&mut tx), "local", "the connection commits locally");
    take(&mut tx, LAKE, &instance, 1, &lease(0))
        .unwrap()
        .unwrap();
    assert_eq!(setting(&mut tx), "on", "a take's numbering");
    tx.commit().unwrap();
    let mut tx = client.transaction().unwrap();
    let admit = Admit {
        through: 9,
        stream: 1,
    };
    raise_to(&mut tx, LAKE, &instance, 1, 2, Some(admit)).unwrap();
    assert_eq!(setting(&mut tx), "on", "a group's raise");
    tx.commit().unwrap();
    let mut tx = client.transaction().unwrap();
    let numbered = [(
        (instance.clone(), 1),
        Held {
            epoch: 3,
            admit: Admit::default(),
        },
    )]
    .into_iter()
    .collect();
    seat(&mut tx, LAKE, &numbered).unwrap();
    assert_eq!(setting(&mut tx), "on", "the rows made from database 0's");
    tx.commit().unwrap();
    // A row held is never raised by it, nor its place lowered (c5a3 H1,
    // c5a2 N2).
    let (epoch, through): (i64, i64) = {
        let row = client
            .query_one(
                "SELECT epoch, admit_through FROM lake_shard_fence WHERE instance = $1",
                &[&instance],
            )
            .unwrap();
        (row.get(0), row.get(1))
    };
    assert_eq!((epoch, through), (2, 9));
}

/// Review c5a2 N6: a take is numbered by the shard's row, which holds the
/// highest epoch, never by reading the control journal: a record naming a
/// higher epoch than the row changes nothing.
#[test]
fn a_take_is_numbered_by_its_row_not_by_the_control_journal() {
    let (url, writer) = isolated("lake-shard-row");
    let instance = unique("feed");
    assert_eq!(writer.adopt().unwrap(), 1);
    let taken = writer.take_shard(took(&instance, 1), &lease(0)).unwrap();
    assert_eq!(taken.epoch, 1);
    writer.record_shard_take(&taken).unwrap();
    // A record the row never saw, as a journal read would find it.
    let mut admin = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let body = {
        use prost::Message as _;
        meridian_domain::v1::ShardTookOver {
            epoch: 50,
            ..took(&instance, 1)
        }
        .encode_to_vec()
    };
    admin
        .execute(
            "INSERT INTO lake_control_change (sequence, epoch, committed_at_ns, kind, body)
             SELECT max(sequence) + 1, 1, 1, 'meridian.v1.ShardTookOver', $1 FROM lake_control_change",
            &[&body],
        )
        .unwrap();
    assert_eq!(
        writer
            .take_shard(took(&instance, 1), &lease(0))
            .unwrap()
            .epoch,
        2
    );
}

/// Review c5a2 L2: the first takes of a shard racing each other: the row is
/// made before it is locked, so the take under fewer Lease transitions,
/// committed after the other's, is refused, never numbered past it.
#[test]
fn a_first_take_racing_another_under_fewer_transitions_is_refused() {
    use meridian_changes::shard_fence::{take, LAKE};
    let url = std::env::var("MERIDIAN_TEST_DATABASE_URL").expect("MERIDIAN_TEST_DATABASE_URL");
    let instance = unique("feed");
    let mut first = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let mut tx = first.transaction().unwrap();
    assert_eq!(take(&mut tx, LAKE, &instance, 1, &lease(1)).unwrap(), Ok(1));
    let racing = {
        let (url, instance) = (url.clone(), instance.clone());
        std::thread::spawn(move || {
            let mut second = postgres::Client::connect(&url, postgres::NoTls).unwrap();
            let mut tx = second.transaction().unwrap();
            let taken = take(&mut tx, LAKE, &instance, 1, &lease(0)).unwrap();
            tx.commit().unwrap();
            taken
        })
    };
    // The second waits on the first's row.
    std::thread::sleep(std::time::Duration::from_millis(300));
    tx.commit().unwrap();
    let late = racing.join().unwrap();
    assert!(
        late.is_err(),
        "the take under fewer transitions was numbered: {late:?}"
    );
}

/// Review c5a H2: a write the write-ahead stream held before the take
/// reached the group -- one the lake answered as recorded, taken again
/// after a database failover lost its commit -- is admitted whatever its
/// epoch; one past the stream's place at the raise is refused.
#[test]
fn a_write_ahead_replay_the_stream_held_before_the_take_is_recorded_not_refused() {
    use meridian_changes::shard_fence::Admit;
    use meridian_lake::store::{ShardStamp, StoreError, WriteAheadBatch, WriteAheadPosition};
    const STREAM: i64 = 1_790_000_000_000_000_000;
    let store = store();
    let instance = unique("feed");
    let dataset = unique("feed:prices");
    let subject = unique("LCL-BTC");
    let stream = unique("STREAM");
    let old = |sequence| ShardStamp::of(&instance, 1, 1).map(|s| s.in_stream(sequence, STREAM));
    let first = store.take_shard(took(&instance, 1), &lease(0)).unwrap();
    assert_eq!(first.epoch, 1);
    // The take that fenced the old holder reached this group with the
    // stream at 7: sequences 1 to 7 were sent before it.
    assert_eq!(
        store
            .raise_shard(
                &instance,
                1,
                2,
                Some(Admit {
                    through: 7,
                    stream: STREAM
                })
            )
            .unwrap(),
        2
    );
    let batch = |sequence: u64, key: &str, stamp| WriteAheadBatch {
        stream: stream.clone(),
        after: None,
        through: WriteAheadPosition {
            sequence,
            at_ns: sequence as i64,
        },
        parts: vec![(
            dataset.clone(),
            vec![close(
                &dataset,
                key,
                &subject,
                "2026-11-16",
                sequence as i64,
            )],
        )],
        shards: vec![stamp],
        ..Default::default()
    };
    // The replay of a write the lake had answered: recorded.
    let done = store
        .record_write_ahead(batch(5, "a", old(5)), 5_000)
        .unwrap();
    assert_eq!(
        done[0].as_ref().map(|d| d.recorded).ok(),
        Some(1),
        "a write the stream held before the take was refused on its replay: {:?}",
        done[0]
    );
    // One the old holder sent after the take reached the group: refused.
    let mut late = batch(8, "b", old(8));
    late.after = store.write_ahead_position(&stream).unwrap();
    let done = store.record_write_ahead(late, 8_000).unwrap();
    assert!(
        matches!(done[0], Err(StoreError::ShardFenced(_))),
        "{:?}",
        done[0]
    );
}

/// Review c5a M2: a take whose Lease's transitions are below the highest a
/// take of the shard was numbered with is refused, SHARD_FENCED, and the
/// shard's epoch is left as the later take numbered it.
#[test]
fn a_late_take_under_fewer_lease_transitions_is_refused() {
    use meridian_lake::store::StoreError;
    let store = store();
    let instance = unique("feed");
    assert_eq!(
        store
            .take_shard(took(&instance, 1), &lease(5))
            .unwrap()
            .epoch,
        5
    );
    let late = store.take_shard(took(&instance, 1), &lease(4));
    assert!(matches!(late, Err(StoreError::ShardFenced(_))), "{late:?}");
    // The same transitions take again: a sidecar restarted in its pod.
    assert_eq!(
        store
            .take_shard(took(&instance, 1), &lease(5))
            .unwrap()
            .epoch,
        6
    );
}

/// Review c5a L1: a database added later holds every plugin shard's epoch
/// before it serves. A member's start raises every database it writes to
/// the highest epoch database 0's records name, so a stale holder's write
/// routed there is refused, never taken as the first epoch seen.
#[test]
fn a_database_added_later_holds_each_shard_s_recorded_epoch_once_a_member_starts() {
    use meridian_lake::store::{ShardStamp, StoreError};
    let urls = two_databases();
    let zero = spread(&urls, 0);
    let instance = unique("feed");
    let dataset = unique("feed:prices");
    let subject = unique("LCL-BTC");
    for want in [1, 2] {
        let taken = zero.take_shard(took(&instance, 1), &lease(0)).unwrap();
        assert_eq!(taken.epoch, want);
        zero.record_shard_take(&taken).unwrap();
    }
    // Database 1 never heard a raise, as one added after the takes.
    assert_eq!(zero.reseat_shards().unwrap(), 1, "database 1 raised");
    let added = PostgresStore::connect(&urls[1], 2).unwrap();
    let done = added.record_parts(
        &dataset,
        vec![vec![close(&dataset, "a", &subject, "2026-11-16", 1)]],
        &[ShardStamp::of(&instance, 1, 1)],
        1_000,
    );
    assert!(
        matches!(done[0], Err(StoreError::ShardFenced(_))),
        "a stale stamp passed in a database added later: {:?}",
        done[0]
    );
}

/// A write-ahead batch of one part of `dataset`, at `sequence` of its
/// stream, under `stamp`.
fn fenced_batch(
    stream: &str,
    dataset: &str,
    subject: &str,
    sequence: u64,
    key: &str,
    stamp: Option<meridian_lake::store::ShardStamp>,
) -> meridian_lake::store::WriteAheadBatch {
    meridian_lake::store::WriteAheadBatch {
        stream: stream.to_string(),
        after: None,
        through: meridian_lake::store::WriteAheadPosition {
            sequence,
            at_ns: sequence as i64,
        },
        parts: vec![(
            dataset.to_string(),
            vec![close(dataset, key, subject, "2026-11-16", sequence as i64)],
        )],
        shards: vec![stamp],
        ..Default::default()
    }
}

/// Review c5a3 H1 (ruled 2026-11-16): a database on a new timeline -- or a
/// member starting -- while a take is still being raised is never raised
/// past it from database 0's row. An honest write the holder sent before
/// the take (answered, then lost with its local commit) is replayed after
/// the follow and before the take's own raise reaches it, and passes at its
/// epoch; the take's raise then keeps its own place with its epoch.
#[test]
fn a_replay_after_a_follow_and_before_the_take_s_raise_is_recorded() {
    use meridian_changes::shard_fence::Admit;
    use meridian_lake::store::ShardStamp;
    const STREAM: i64 = 1_790_000_000_000_000_000;
    let urls = two_databases();
    let zero = spread(&urls, 0);
    let instance = unique("feed");
    let dataset = unique("feed:prices");
    let subject = unique("LCL-BTC");
    let stream = unique("STREAM");
    let place = |through| Admit {
        through,
        stream: STREAM,
    };
    // Take 1, raised everywhere with the stream at 10, and recorded.
    let first = zero.take_shard(took(&instance, 1), &lease(0)).unwrap();
    assert_eq!(first.epoch, 1);
    assert_eq!(
        zero.raise_shard(&instance, 1, 1, Some(place(10))).unwrap(),
        1
    );
    zero.record_shard_take(&first).unwrap();
    // Take 2 numbered in database 0; its raise has not reached database 1.
    assert_eq!(
        zero.take_shard(took(&instance, 1), &lease(0))
            .unwrap()
            .epoch,
        2
    );
    // Database 1 follows a new timeline (or a member starts) meanwhile.
    zero.reseat_shards().unwrap();
    // The holder's write at 150, stamped 1 -- answered, then lost with the
    // failover -- replayed in database 1 before take 2's raise.
    let one = PostgresStore::connect(&urls[1], 2).unwrap();
    let w = ShardStamp::of(&instance, 1, 1).map(|s| s.in_stream(150, STREAM));
    let done = one
        .record_write_ahead(fenced_batch(&stream, &dataset, &subject, 150, "w", w), 150)
        .unwrap();
    assert_eq!(
        done[0].as_ref().map(|d| d.recorded).ok(),
        Some(1),
        "an answered write's replay before the take's raise was refused: {:?}",
        done[0]
    );
    // Take 2's raise keeps its own place with its epoch.
    assert_eq!(
        zero.raise_shard(&instance, 1, 2, Some(place(200))).unwrap(),
        2
    );
    let mut admin = postgres::Client::connect(&urls[1], postgres::NoTls).unwrap();
    let row = admin
        .query_one(
            "SELECT epoch, admit_through FROM lake_shard_fence WHERE instance = $1",
            &[&instance],
        )
        .unwrap();
    assert_eq!((row.get::<_, i64>(0), row.get::<_, i64>(1)), (2, 200));
}

/// Review c5a3 M1 (ruled 2026-11-16): the take's numbering keeps the
/// stream's place the control writer found before it, in database 0's row,
/// so group 0 -- whose row the numbering raised -- admits the backlog the
/// stream held before the take as every other group does, and its raise
/// at that epoch keeps that place.
#[test]
fn group_0_keeps_the_take_s_place_with_its_numbering() {
    use meridian_changes::shard_fence::{Admit, Taking};
    use meridian_lake::store::{ShardStamp, StoreError};
    const STREAM: i64 = 1_790_000_000_000_000_000;
    let store = store();
    let instance = unique("feed");
    let dataset = unique("feed:prices");
    let subject = unique("LCL-BTC");
    let stream = unique("STREAM");
    assert_eq!(
        store
            .take_shard(took(&instance, 1), &lease(0))
            .unwrap()
            .epoch,
        1
    );
    let taking = Taking {
        admit: Some(Admit {
            through: 105,
            stream: STREAM,
        }),
        ..lease(0)
    };
    assert_eq!(
        store.take_shard(took(&instance, 1), &taking).unwrap().epoch,
        2
    );
    // Group 0's raise at the epoch its numbering holds keeps the place.
    let later = Admit {
        through: 200,
        stream: STREAM,
    };
    assert_eq!(store.raise_shard(&instance, 1, 2, Some(later)).unwrap(), 2);
    let old = |sequence| ShardStamp::of(&instance, 1, 1).map(|s| s.in_stream(sequence, STREAM));
    let done = store
        .record_write_ahead(
            fenced_batch(&stream, &dataset, &subject, 100, "a", old(100)),
            100,
        )
        .unwrap();
    assert_eq!(
        done[0].as_ref().map(|d| d.recorded).ok(),
        Some(1),
        "group 0 refused the backlog the stream held before the take: {:?}",
        done[0]
    );
    let mut sent_after = fenced_batch(&stream, &dataset, &subject, 150, "b", old(150));
    sent_after.after = store.write_ahead_position(&stream).unwrap();
    let done = store.record_write_ahead(sent_after, 150).unwrap();
    assert!(
        matches!(done[0], Err(StoreError::ShardFenced(_))),
        "a write sent after the take's place was admitted: {:?}",
        done[0]
    );
}

/// Review c5a2 N5 (approved 2026-11-16): a Lease made again -- another
/// uid, its transitions from 0 -- is taken, never refused by the floor the
/// old Lease's takes raised; on it, its own floor holds.
#[test]
fn a_lease_made_again_is_taken_past_the_old_lease_s_floor() {
    use meridian_changes::shard_fence::Taking;
    use meridian_lake::store::StoreError;
    let store = store();
    let instance = unique("feed");
    assert_eq!(
        store
            .take_shard(took(&instance, 1), &lease(7))
            .unwrap()
            .epoch,
        7
    );
    let again = |transitions| Taking {
        lease_transitions: transitions,
        lease_uid: "uid-2".into(),
        admit: None,
    };
    let taken = store.take_shard(took(&instance, 1), &again(0));
    assert_eq!(
        taken.map(|t| t.epoch).ok(),
        Some(8),
        "a Lease made again was refused"
    );
    assert_eq!(
        store
            .take_shard(took(&instance, 1), &again(2))
            .unwrap()
            .epoch,
        9
    );
    let late = store.take_shard(took(&instance, 1), &again(1));
    assert!(matches!(late, Err(StoreError::ShardFenced(_))), "{late:?}");
}

/// The review of v31a-core-2, F1(b): a Lease's transitions are an int32, and
/// the fence keeps them as a bigint. A take sent with more -- 2^63 -- was
/// kept as i64::MIN and read back as 2^63, so every later take of the Lease
/// was refused below it: the shard locked out. Refused now, before anything
/// is numbered or kept, and the Lease's next take is numbered.
#[test]
fn a_take_past_an_int32_of_lease_transitions_is_refused_and_locks_no_take_out() {
    let store = store();
    let instance = unique("feed");
    let extreme = store.take_shard(took(&instance, 1), &lease(1 << 63));
    let next = store.take_shard(took(&instance, 1), &lease(5));
    assert_eq!(
        next.map(|t| t.epoch).ok(),
        Some(5),
        "the take past an int32 locked the Lease's next take out: {extreme:?}"
    );
    assert!(extreme.is_err(), "numbered past an int32: {extreme:?}");
    let past = store.take_shard(took(&instance, 1), &lease(i32::MAX as u64 + 1));
    assert!(past.is_err(), "numbered past an int32: {past:?}");
}

/// The review of v31a-core-2, F1(b): a write's shard epoch, its sidecar's to
/// stamp, raised the fence whatever it was. u64::MAX was kept as -1 and read
/// back as u64::MAX, so the shard's next take could number nothing past it;
/// an epoch past i64 compared below every epoch in SQL, and one past an
/// int32 left the takes nothing to number within the Lease's transitions.
/// Each is refused now and raises nothing, and the shard's holder writes on.
#[test]
fn a_write_stamped_past_an_int32_epoch_is_refused_and_raises_nothing() {
    use meridian_lake::store::{ShardStamp, StoreError};
    let store = store();
    let instance = unique("feed");
    let dataset = unique("feed:prices");
    let subject = unique("LCL-BTC");
    let stamp = |epoch: u64| ShardStamp::of(&instance, 1, epoch);
    let fenced = |done: &Result<meridian_lake::store::Recorded, StoreError>| {
        matches!(done, Err(StoreError::ShardFenced(_)))
    };

    let forged = store.record_parts(
        &dataset,
        vec![vec![close(&dataset, "a", &subject, "2026-11-20", 1)]],
        &[stamp(u64::MAX)],
        1_000,
    );
    let taken = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store.take_shard(took(&instance, 1), &lease(0))
    }));
    assert!(
        matches!(&taken, Ok(Ok(t)) if t.epoch == 1),
        "the shard's take after a write stamped u64::MAX: {:?}, the write {:?}",
        taken.as_ref().map(|t| t.as_ref().map(|t| t.epoch)),
        forged[0]
    );
    assert!(fenced(&forged[0]), "{:?}", forged[0]);

    for epoch in [i32::MAX as u64 + 1, i64::MAX as u64, 1 << 63, u64::MAX] {
        let done = store.record_parts(
            &dataset,
            vec![vec![close(&dataset, "b", &subject, "2026-11-20", 2)]],
            &[stamp(epoch)],
            2_000,
        );
        assert!(fenced(&done[0]), "{epoch}: {:?}", done[0]);
    }
    let done = store.record_parts(
        &dataset,
        vec![vec![close(&dataset, "c", &subject, "2026-11-20", 3)]],
        &[stamp(1)],
        3_000,
    );
    assert_eq!(
        done[0].as_ref().map(|recorded| recorded.recorded).ok(),
        Some(1),
        "the holder's write at its epoch: {:?}",
        done[0]
    );
    assert_eq!(
        store
            .take_shard(took(&instance, 1), &lease(0))
            .unwrap()
            .epoch,
        2,
        "nothing raised it"
    );
}

/// The final review of v31a-core-2 (review-core-2c), finding 4: a write
/// stamped past `EPOCH_MOST` raises nothing, so its shard's row was never
/// read, and its refusal said the epoch held was 0. It names the epoch the
/// shard holds.
#[test]
fn a_write_stamped_past_the_most_epoch_names_the_epoch_held() {
    use meridian_lake::store::{ShardStamp, StoreError};
    let store = store();
    let instance = unique("feed");
    let dataset = unique("feed:prices");
    let subject = unique("LCL-BTC");
    assert_eq!(
        store
            .take_shard(took(&instance, 1), &lease(5))
            .unwrap()
            .epoch,
        5
    );
    let done = store.record_parts(
        &dataset,
        vec![vec![close(&dataset, "a", &subject, "2026-11-20", 1)]],
        &[ShardStamp::of(&instance, 1, u64::MAX)],
        1_000,
    );
    match &done[0] {
        Err(StoreError::ShardFenced(fenced)) => assert_eq!(fenced.held, 5, "{fenced}"),
        other => panic!("a write stamped u64::MAX: {other:?}"),
    }
}

/// The review of v31a-core-2, F1(b): a shard whose row a forged write raised
/// past an int32 before the check above -- kept as -1, or as i64::MAX -- is
/// refused its take, never numbered an epoch that wraps: u64::MAX + 1
/// overflowed, and 2^63 was kept as i64::MIN, below every epoch in SQL.
/// Refused as a shard held at the most (the second review, finding 3).
#[test]
fn a_shard_held_past_an_int32_epoch_refuses_a_take_rather_than_wrap() {
    let store = store();
    let instance = unique("feed");
    let url = std::env::var("MERIDIAN_TEST_DATABASE_URL").expect("MERIDIAN_TEST_DATABASE_URL");
    let mut admin = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    for (shard, held) in [(1u32, -1i64), (2, i64::MAX)] {
        admin
            .execute(
                "INSERT INTO lake_shard_fence (instance, shard, epoch) VALUES ($1, $2, $3)",
                &[&instance, &i64::from(shard), &held],
            )
            .unwrap();
        let taken = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            store.take_shard(took(&instance, shard), &lease(0))
        }));
        assert!(
            matches!(
                &taken,
                Ok(Err(meridian_lake::store::StoreError::FailedPrecondition {
                    field: "shard",
                    ..
                }))
            ),
            "held {held}: {:?}",
            taken.as_ref().map(|t| t.as_ref().map(|t| t.epoch))
        );
    }
}

/// The second review of v31a-core-2, finding 3: a shard whose row holds
/// exactly the most an epoch is -- as a write stamped there leaves it -- is
/// refused its take FAILED_PRECONDITION naming the shard, the instance, the
/// shard's number and the epoch held, and saying a person must reset its
/// fence; a take whose transitions are past an int32 is still refused
/// naming `lease_transitions`.
#[test]
fn a_take_of_a_shard_held_at_the_most_epoch_names_the_epoch_and_the_shard() {
    use meridian_changes::shard_fence::EPOCH_MOST;
    use meridian_lake::store::StoreError;
    let store = store();
    let instance = unique("feed");
    let url = std::env::var("MERIDIAN_TEST_DATABASE_URL").expect("MERIDIAN_TEST_DATABASE_URL");
    let mut admin = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    admin
        .execute(
            "INSERT INTO lake_shard_fence (instance, shard, epoch) VALUES ($1, 1, $2)",
            &[&instance, &(EPOCH_MOST as i64)],
        )
        .unwrap();
    let refused = store
        .take_shard(took(&instance, 1), &lease(5))
        .expect_err("no take numbers past the most");
    assert!(
        matches!(
            &refused,
            StoreError::FailedPrecondition { field: "shard", .. }
        ),
        "{refused:?}"
    );
    let detail = refused.on_the_bus();
    let at = detail.find("[refusal-status").expect("a refusal's status");
    let (said, fields, words) = meridian_bus::read_refusal_status(&detail[at..]).expect("readable");
    assert_eq!(
        said,
        meridian_bus::RefusalStatus::FailedPrecondition,
        "{detail}"
    );
    assert_eq!(fields, ["shard"], "{detail}");
    for named in [
        format!("instance {instance} shard 1"),
        format!("epoch {EPOCH_MOST}"),
        "a person must reset the shard's fence".to_string(),
    ] {
        assert!(words.contains(&named), "{named} in {words}");
    }
    let past = store.take_shard(took(&instance, 1), &lease(EPOCH_MOST + 1));
    assert!(
        matches!(
            &past,
            Err(StoreError::Invalid {
                field: "lease_transitions",
                ..
            })
        ),
        "{past:?}"
    );
}

/// PR CI 44283011761 (reconfigure, groups lowered, tolerant): the store's
/// own raise on a new database timeline is its epoch from the moment the
/// raise can be read. Its writer's start-up follow raised the epoch while
/// another of its commits waited behind the raise's locks; that commit read
/// the raised head before the store had adopted it, was refused as fenced
/// by a newer writer, and the member stepped down two seconds after it said
/// it served -- the second group's writer, serving on until it did, had
/// already stopped, and a price write was told "nothing serves".
///
/// Held here at the raise's commit, by a deferred trigger waiting on an
/// advisory lock this test holds: the writer's epoch is the raised one
/// before the raise commits; a batch waiting behind it commits under it
/// once it does; and a raise whose commit fails leaves the epoch the
/// writer had.
#[test]
fn a_raise_on_a_new_timeline_is_the_writers_epoch_before_its_commit_can_be_read() {
    use meridian_changes::timeline::Timeline;
    let (url, writer) = isolated("lake-raise-held");
    let writer = Arc::new(writer);
    let dataset = unique("coinbase-1:daily");
    let subject = unique("LCL-BTC");
    assert_eq!(writer.adopt().unwrap(), 1);
    writer
        .record(
            &dataset,
            vec![close(&dataset, "a", &subject, "2026-10-01", 1)],
            10,
        )
        .unwrap();
    assert_eq!(writer.follow_timeline().unwrap(), Timeline::First);
    let mut admin = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let hold = 0x6335_7232_i64;
    // A raise's commit waits on `hold`; a commit that changes no epoch does
    // not. And one marked to fail, fails.
    admin
        .batch_execute(&format!(
            "CREATE TABLE raise_fails (fails boolean NOT NULL);
             INSERT INTO raise_fails VALUES (false);
             CREATE FUNCTION held_raise() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
               PERFORM pg_advisory_xact_lock({hold});
               IF (SELECT fails FROM raise_fails) THEN
                 RAISE EXCEPTION 'the raise''s commit fails';
               END IF;
               RETURN NULL;
             END $$;
             CREATE CONSTRAINT TRIGGER held_raise AFTER UPDATE ON lake_change_head
               DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
               WHEN (NEW.epoch <> OLD.epoch) EXECUTE FUNCTION held_raise();"
        ))
        .unwrap();
    let moved = |admin: &mut postgres::Client, to: &str| {
        admin
            .execute(
                "UPDATE lake_change_head SET database_identity = $1 WHERE partition = 'control'",
                &[&to],
            )
            .unwrap();
    };
    let waiting_on_hold = |admin: &mut postgres::Client| -> i64 {
        admin
            .query_one(
                "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND NOT granted
                   AND classid = 0 AND objid = $1::bigint::oid AND objsubid = 1",
                &[&hold],
            )
            .unwrap()
            .get(0)
    };
    // A failover's new timeline, the raise held at its commit.
    moved(&mut admin, "another/00000002");
    admin
        .execute("SELECT pg_advisory_lock($1)", &[&hold])
        .unwrap();
    let following = Arc::clone(&writer);
    let raise = std::thread::spawn(move || following.follow_timeline());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while waiting_on_hold(&mut admin) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the raise never reached its commit"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(
        writer.epoch(),
        2,
        "the raise can be read the moment it commits, and the writer's epoch was not yet its own"
    );
    // A batch of the writer's waits behind the raise's locks.
    let recording = Arc::clone(&writer);
    let (recorded_dataset, recorded_subject) = (dataset.clone(), subject.clone());
    let batch = std::thread::spawn(move || {
        recording.record(
            &recorded_dataset,
            vec![close(
                &recorded_dataset,
                "b",
                &recorded_subject,
                "2026-10-02",
                2,
            )],
            20,
        )
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    admin
        .execute("SELECT pg_advisory_unlock($1)", &[&hold])
        .unwrap();
    assert_eq!(raise.join().unwrap().unwrap(), Timeline::Moved(2));
    let committed = batch.join().unwrap();
    assert!(
        committed.is_ok(),
        "a batch behind the writer's own raise was refused: {committed:?}"
    );
    let epochs: Vec<u64> = every_change(&writer, &dataset, meridian_changes::PAGE_MOST)
        .iter()
        .map(|c| c.epoch)
        .collect();
    assert_eq!(epochs, vec![1, 2]);
    // Another new timeline, its raise's commit failing: the writer keeps
    // the epoch it had, and its next batch commits under it.
    admin
        .execute("UPDATE raise_fails SET fails = true", &[])
        .unwrap();
    moved(&mut admin, "yet another/00000003");
    assert!(
        writer.follow_timeline().is_err(),
        "the raise's commit failed"
    );
    assert_eq!(writer.epoch(), 2);
    admin
        .execute("UPDATE raise_fails SET fails = false", &[])
        .unwrap();
    admin
        .execute("UPDATE lake_change_head SET database_identity = 'another/00000002' WHERE partition = 'control'", &[])
        .unwrap();
    writer
        .record(
            &dataset,
            vec![close(&dataset, "c", &subject, "2026-10-03", 3)],
            30,
        )
        .unwrap();
}

/// A dataset a group past the first begins while its group's takeover is
/// raising (review-delta B1): the raise has read the ranges it raises and
/// waits before it raises the group's own row, and the old writer, its row
/// not raised yet, begins a dataset under its old epoch and commits. The
/// takeover raises that dataset too, so the new writer can write it. The
/// raise is held before its group's row by a statement trigger waiting on
/// an advisory lock this test holds.
#[test]
fn a_dataset_begun_while_a_takeover_raises_is_raised_with_the_rest() {
    use meridian_changes::groups::{owner, Groups};
    use meridian_changes::{TookOver, LEASE_FREE};
    let (url, _migrated) = isolated("lake-raise-begun");
    let store = |group: u32| {
        Arc::new(
            PostgresStore::connect(&url, 4)
                .unwrap()
                .with_groups(Groups::new(group, 2).unwrap()),
        )
    };
    let (zero, old) = (store(0), store(1));
    zero.take_over(&TookOver::unnamed(LEASE_FREE)).unwrap();
    let was = old.take_over(&TookOver::unnamed(LEASE_FREE)).unwrap();
    let begun = (0..)
        .map(|n| unique(&format!("coinbase-1:begun{n}")))
        .find(|dataset| owner(dataset, 2) == 1)
        .unwrap();
    let subject = unique("LCL-BTC");
    let mut admin = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let hold = 0x6335_7742_i64;
    admin
        .batch_execute(&format!(
            "CREATE FUNCTION held_raise() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
               PERFORM pg_advisory_xact_lock({hold});
               RETURN NULL;
             END $$;
             CREATE TRIGGER held_raise BEFORE INSERT OR UPDATE ON lake_writer_group
               FOR EACH STATEMENT EXECUTE FUNCTION held_raise();"
        ))
        .unwrap();
    admin
        .execute("SELECT pg_advisory_lock($1)", &[&hold])
        .unwrap();
    let new = store(1);
    let taking = Arc::clone(&new);
    let takeover = std::thread::spawn(move || taking.take_over(&TookOver::unnamed(LEASE_FREE)));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while admin
        .query_one(
            "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND NOT granted
               AND classid = 0 AND objid = $1::bigint::oid AND objsubid = 1",
            &[&hold],
        )
        .unwrap()
        .get::<_, i64>(0)
        == 0
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the raise never reached its group's row"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // The old writer begins a dataset meanwhile, under its row's epoch,
    // and commits: nothing the raise holds yet waits it.
    let beginning = {
        let (old, begun, subject) = (Arc::clone(&old), begun.clone(), subject.clone());
        std::thread::spawn(move || {
            old.record(
                &begun,
                vec![close(&begun, "a", &subject, "2026-10-01", 1)],
                10,
            )
        })
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !beginning.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let committed_first = beginning.is_finished();
    admin
        .execute("SELECT pg_advisory_unlock($1)", &[&hold])
        .unwrap();
    assert!(committed_first, "the begin waited on the raise");
    beginning.join().unwrap().unwrap();
    let raised = takeover.join().unwrap().unwrap();
    assert!(raised > was);
    assert_eq!(
        new.range_tip(&begun).unwrap().unwrap().epoch,
        raised,
        "the dataset begun while the takeover raised is left under the fenced epoch"
    );
    new.record(
        &begun,
        vec![close(&begun, "b", &subject, "2026-10-02", 2)],
        20,
    )
    .unwrap();
}

/// A dataset begun while the writer's own raise on a new timeline waits at
/// its commit, and the raise's commit fails (review c5r3, L2): the dataset
/// is begun under the epoch the writer kept, never the raise's, so the
/// writer does not step down from its own failed raise. The raise is held
/// at its COMMIT by a deferred constraint trigger waiting on an advisory
/// lock this test holds, then fails.
#[test]
fn a_dataset_begun_behind_a_raise_whose_commit_fails_never_steps_the_writer_down() {
    use meridian_changes::timeline::Timeline;
    let (url, writer) = isolated("lake-raise-fails-begun");
    let writer = Arc::new(writer);
    let dataset = unique("coinbase-1:daily");
    let subject = unique("LCL-BTC");
    assert_eq!(writer.adopt().unwrap(), 1);
    writer
        .record(
            &dataset,
            vec![close(&dataset, "a", &subject, "2026-10-01", 1)],
            10,
        )
        .unwrap();
    assert_eq!(writer.follow_timeline().unwrap(), Timeline::First);
    let mut admin = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let hold = 0x6335_7234_i64;
    // A raise's commit waits on `hold`, then fails.
    admin
        .batch_execute(&format!(
            "CREATE FUNCTION failed_raise() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
               PERFORM pg_advisory_xact_lock({hold});
               RAISE EXCEPTION 'the raise''s commit fails';
             END $$;
             CREATE CONSTRAINT TRIGGER failed_raise AFTER UPDATE ON lake_change_head
               DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
               WHEN (NEW.epoch <> OLD.epoch) EXECUTE FUNCTION failed_raise();"
        ))
        .unwrap();
    let waiting_on_hold = |admin: &mut postgres::Client| -> i64 {
        admin
            .query_one(
                "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND NOT granted
                   AND classid = 0 AND objid = $1::bigint::oid AND objsubid = 1",
                &[&hold],
            )
            .unwrap()
            .get(0)
    };
    // A failover's new timeline, the raise held at its commit.
    admin
        .execute(
            "UPDATE lake_change_head SET database_identity = 'another/00000002'
              WHERE partition = 'control'",
            &[],
        )
        .unwrap();
    admin
        .execute("SELECT pg_advisory_lock($1)", &[&hold])
        .unwrap();
    let following = Arc::clone(&writer);
    let raise = std::thread::spawn(move || following.follow_timeline());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while waiting_on_hold(&mut admin) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the raise never reached its commit"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(
        writer.epoch(),
        2,
        "the raise is the writer's epoch while it commits"
    );
    // A dataset the writer begins meanwhile waits behind the raise.
    let begun = unique("coinbase-1:daily");
    let begin = move |writer: &PostgresStore, dataset: &str, subject: &str| {
        writer.record(
            dataset,
            vec![close(dataset, "b", subject, "2026-10-02", 2)],
            20,
        )
    };
    let first = {
        let (beginning, dataset, subject) = (Arc::clone(&writer), begun.clone(), subject.clone());
        std::thread::spawn(move || begin(&beginning, &dataset, &subject))
    };
    std::thread::sleep(std::time::Duration::from_millis(300));
    admin
        .execute("SELECT pg_advisory_unlock($1)", &[&hold])
        .unwrap();
    assert!(raise.join().unwrap().is_err(), "the raise's commit failed");
    assert_eq!(writer.epoch(), 1, "the epoch the writer had is kept");
    let first = first.join().unwrap();
    assert!(
        !meridian_changes::fenced_seen(),
        "the writer was told it was fenced by its own raise, which failed: {first:?}"
    );
    // Refused at most while the epoch was being put back: taken again, it
    // is begun under the epoch kept.
    if first.is_err() {
        begin(&writer, &begun, &subject).unwrap();
    }
    assert_eq!(writer.range_tip(&begun).unwrap().unwrap().epoch, 1);
}

/// Review c5w's residual, 3 -> 2: group 0's takeover at the lowered count
/// raises group 2's own row too, so group 2's writer, not stopped yet,
/// begins no dataset under its old epoch that a group left now owns.
#[test]
fn a_count_lowered_from_three_to_two_fences_group_twos_own_row() {
    use meridian_changes::groups::{owner, Groups};
    use meridian_changes::{TookOver, LEASE_FREE};
    let (url, _migrated) = isolated("lake-lowered-row");
    let store = |group: u32, count: u32| {
        PostgresStore::connect(&url, 2)
            .unwrap()
            .with_groups(Groups::new(group, count).unwrap())
    };
    let (zero, one, two) = (store(0, 3), store(1, 3), store(2, 3));
    for each in [&zero, &one, &two] {
        each.take_over(&TookOver::unnamed(LEASE_FREE)).unwrap();
    }
    let subject = unique("LCL-UNREC");
    let begun = (0..)
        .map(|n| unique(&format!("coinbase-1:lowrow{n}")))
        .find(|dataset| owner(dataset, 3) == 2 && owner(dataset, 2) == 0)
        .unwrap();
    let lowered = store(0, 2);
    lowered.take_over(&TookOver::unnamed(LEASE_FREE)).unwrap();
    let stale = two.record(&begun, vec![trade(&begun, "stale", &subject, 1)], 1_000);
    assert!(
        matches!(stale, Err(meridian_lake::store::StoreError::Fenced(_))),
        "group 2's writer began a dataset group 0 now owns under its old epoch: {stale:?}"
    );
    assert!(two.head_epoch().unwrap() > two.epoch());
    drop((zero, one));
}

/// Review c5w3 L-c: a migration cut off by the network alone leaves its
/// server session holding the schema lock until its keepalive ends it. The
/// migration applied again waits for that lock a bounded while, then is
/// refused, so its wait never runs on past the bound; once the lock is let
/// go it is taken, and the session's lock_timeout is as it was.
#[test]
fn a_schema_lock_a_cut_off_session_holds_is_waited_for_a_bounded_while() {
    // A key of this test's own: every other test's store() migrates under
    // the lake's.
    const KEY: i64 = 0x6c61_6b65_6c6f_636b_u64 as i64;
    let url = std::env::var("MERIDIAN_TEST_DATABASE_URL").unwrap();
    let mut cut_off = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    cut_off
        .execute("SELECT pg_advisory_lock($1)", &[&KEY])
        .unwrap();
    let mut again = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let (said, heard) = std::sync::mpsc::channel();
    let waiting = std::thread::spawn(move || {
        let taken =
            meridian_changes::take_schema_lock(&mut again, KEY, std::time::Duration::from_secs(1));
        let _ = said.send(taken.map_err(|failed| failed.code().cloned()));
        again
    });
    let taken = heard
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("it waited on past its bound for a lock a cut-off session holds");
    let refused = taken.expect_err("it took a lock another session holds");
    assert_eq!(
        refused,
        Some(postgres::error::SqlState::LOCK_NOT_AVAILABLE),
        "refused for another reason"
    );
    cut_off
        .execute("SELECT pg_advisory_unlock($1)", &[&KEY])
        .unwrap();
    let mut again = waiting.join().unwrap();
    meridian_changes::take_schema_lock(&mut again, KEY, std::time::Duration::from_secs(1))
        .expect("not taken once let go");
    let timeout: String = again.query_one("SHOW lock_timeout", &[]).unwrap().get(0);
    assert_eq!(timeout, "0", "the session's lock_timeout left changed");
    again
        .execute("SELECT pg_advisory_unlock($1)", &[&KEY])
        .unwrap();
}

/// Review c5s L1, a tolerant lowering 3 -> 2: group 1's takeover raised the
/// row of group 2, now past the count, to group 1's own epoch. Group 1's
/// writer restarted at the unchanged count adopts its epoch: a row past the
/// count is no live group's, so it takes nothing over and records no
/// "assigned" takeover at every restart.
#[test]
fn a_writer_restarted_after_a_lowering_adopts_its_epoch_past_a_stale_row() {
    use meridian_changes::groups::Groups;
    use meridian_changes::{TookOver, LEASE_FREE};
    let (url, _migrated) = isolated("lake-lowered-restart");
    let store = |group: u32, count: u32| {
        PostgresStore::connect(&url, 2)
            .unwrap()
            .with_groups(Groups::new(group, count).unwrap())
    };
    for group in 0..3 {
        store(group, 3)
            .take_over(&TookOver::unnamed(LEASE_FREE))
            .unwrap();
    }
    store(0, 2)
        .take_over(&TookOver::unnamed(LEASE_FREE))
        .unwrap();
    let one = store(1, 2);
    let epoch = one.take_over(&TookOver::unnamed(LEASE_FREE)).unwrap();
    let mut sql = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let took_over = |sql: &mut postgres::Client| -> i64 {
        sql.query_one("SELECT count(*) FROM lake_writer_took_over", &[])
            .unwrap()
            .get(0)
    };
    let before = took_over(&mut sql);
    drop(one);
    let restarted = store(1, 2);
    assert_eq!(
        restarted.adopt().unwrap(),
        epoch,
        "group 1's writer took over again at an unchanged count"
    );
    assert_eq!(took_over(&mut sql), before, "a takeover was recorded");
}

/// Review c5s L8: a takeover chosen as a deadlock's victim (SQLSTATE 40P01)
/// is tried again, not given up on. Group 0's takeover at a count lowered
/// from three to two holds the control partition and waits for group 2's
/// row, which a batch of group 2's writer, not stopped yet, holds shared
/// while it waits for the control partition: Postgres ends the takeover,
/// the one that waited first.
#[test]
fn a_takeover_ended_by_a_deadlock_is_tried_again() {
    use meridian_changes::groups::Groups;
    use meridian_changes::{TookOver, LEASE_FREE};
    let (url, _migrated) = isolated("lake-takeover-deadlock");
    let store = |group: u32, count: u32| {
        PostgresStore::connect(&url, 2)
            .unwrap()
            .with_groups(Groups::new(group, count).unwrap())
    };
    for group in 0..3 {
        store(group, 3)
            .take_over(&TookOver::unnamed(LEASE_FREE))
            .unwrap();
    }
    let mut batch = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let mut watch = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let mut held = batch.transaction().unwrap();
    held.query_one(
        "SELECT epoch FROM lake_writer_group WHERE writer_group = 2 FOR SHARE",
        &[],
    )
    .unwrap();
    let lowered = store(0, 2);
    let taking = std::thread::spawn(move || lowered.take_over(&TookOver::unnamed(LEASE_FREE)));
    // The takeover waits for group 2's row, the control partition held.
    let waiting = (0..500).any(|_| {
        let waits: i64 = watch
            .query_one(
                "SELECT count(*) FROM pg_stat_activity
                  WHERE datname = current_database() AND wait_event_type = 'Lock'",
                &[],
            )
            .unwrap()
            .get(0);
        waits > 0 || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            false
        }
    });
    assert!(waiting, "the takeover never waited for group 2's row");
    held.query_one(
        "SELECT epoch FROM lake_change_head WHERE partition = 'control' FOR SHARE",
        &[],
    )
    .unwrap();
    held.commit().unwrap();
    let took = taking.join().unwrap();
    assert!(
        took.is_ok(),
        "the takeover ended by a deadlock was given up on: {took:?}"
    );
}

/// A range moved to another database between a read's routing and its
/// rows is never answered without the range's rows (the review of
/// 2026-11-23, finding 1): the source, read after it let the range go,
/// answers nothing of it, and the read was sent nowhere else -- rows
/// committed long before the move lost, whatever the member's view. The
/// read is refused UNAVAILABLE, and asked again answers whole.
#[test]
fn a_range_moved_under_a_read_is_never_answered_without_its_rows() {
    use meridian_changes::groups::Groups;
    use meridian_changes::moves::points;
    use meridian_lake::store::StoreError;
    let urls = two_databases();
    let m = Arc::new(members(&urls));
    let dataset = owned_by_zero("coinbase-1:u");
    let (low, high) = halves_of("lake-under");
    let lower = halved(&m, &dataset, &low, &high);
    let groups = Groups::new(0, 2).unwrap();
    let reader = meridian_lake::spread::Spread::new(stores_of(&urls, groups, 2), groups);
    reader.read_owners_again().unwrap();
    assert_eq!(latest_keys(&reader, &dataset, &low).unwrap(), ["l0", "l1"]);
    // Between the read's routing and its rows, once: the range moves.
    let moved = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (mover, range, once) = (m.clone(), lower.clone(), moved.clone());
    points::arm(
        "lake.read.routed",
        &dataset,
        Arc::new(move |_| {
            if !once.swap(true, Ordering::SeqCst) {
                moved_to_one(&mover, &range);
            }
            Ok(())
        }),
    );
    let read = latest_keys(&reader, &dataset, &low);
    points::disarm("lake.read.routed", &dataset);
    assert!(
        moved.load(Ordering::SeqCst),
        "the range moved under the read"
    );
    match read {
        Err(StoreError::Unavailable(_)) => {}
        Ok(keys) => assert_eq!(
            keys,
            ["l0", "l1"],
            "a read a move went by answered without the range's rows"
        ),
        Err(failed) => panic!("{failed:?}"),
    }
    assert_eq!(latest_keys(&reader, &dataset, &low).unwrap(), ["l0", "l1"]);
    assert_eq!(groups.owner_by(&lower, &dataset), 0, "the view, as it was");
}

/// A written row a failed move left behind never stands for a range past
/// a move's pointer of it (the review of the book's fix, finding 1): the
/// databases a read's view names hold a pointer of the range at its close
/// and, in another, a row written long before and never taken away. The
/// leftover is behind the pointer, so the range is looked for in every
/// database, and read where it went.
#[test]
fn a_leftover_written_row_behind_a_moves_pointer_sends_the_read_on() {
    use meridian_changes::groups::Groups;
    let urls = databases(3);
    let m = members(&urls);
    let dataset = owned_by_zero_of("coinbase-1:o", 3);
    let (low, high) = halves_of("lake-leftover");
    let lower = halved(&m, &dataset, &low, &high);
    moved_to_one(&m, &lower);
    // Left in database 2 by a move that failed, long ago: written, behind.
    postgres::Client::connect(&urls[2], postgres::NoTls)
        .unwrap()
        .execute(
            "INSERT INTO lake_partition (dataset, head, epoch, lo, hi, writer_group)
             VALUES ($1, 1, 1, 0, 32767, 2)",
            &[&lower],
        )
        .unwrap();
    // A member whose view names database 2's group for the range.
    let groups = Groups::new(0, 3).unwrap();
    let reader = meridian_lake::spread::Spread::new(stores_of(&urls, groups, 2), groups);
    reader.read_owners_again().unwrap();
    groups.assign(&lower, Some(2));
    assert_eq!(
        latest_keys(&reader, &dataset, &low).unwrap(),
        ["l0", "l1"],
        "a leftover row behind the move's pointer stood for the range"
    );
}

/// A read waits for its own database's pool as it always has (the review
/// of the book's fix, finding 2): the short bound is for a database the
/// read may not need, so its own under load refuses nothing it answered
/// before.
#[test]
fn a_read_waits_for_its_own_databases_pool_as_before() {
    use meridian_changes::groups::Groups;
    let urls = two_databases();
    let m = members(&urls);
    let dataset = owned_by_zero("coinbase-1:p");
    let (low, high) = halves_of("lake-pool");
    let lower = halved(&m, &dataset, &low, &high);
    let groups = Groups::new(0, 2).unwrap();
    let stores = stores_of(&urls, groups, 2);
    let reader = meridian_lake::spread::Spread::new(stores.clone(), groups);
    reader.read_owners_again().unwrap();
    assert_eq!(latest_keys(&reader, &dataset, &low).unwrap(), ["l0", "l1"]);
    // Database 0's pool taken whole for a second and a half.
    let mut admin = postgres::Client::connect(&urls[0], postgres::NoTls).unwrap();
    admin
        .batch_execute("BEGIN; LOCK TABLE lake_partition IN ACCESS EXCLUSIVE MODE")
        .unwrap();
    let held: Vec<_> = (0..2)
        .map(|_| {
            let (store, range) = (stores[0].clone(), lower.clone());
            std::thread::spawn(move || drop(store.range_tip(&range)))
        })
        .collect();
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1_500));
        admin.batch_execute("ROLLBACK").unwrap();
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(
        latest_keys(&reader, &dataset, &low).unwrap(),
        ["l0", "l1"],
        "a read its own database's pool held for a moment was refused"
    );
    release.join().unwrap();
    for holder in held {
        holder.join().unwrap();
    }
}

/// The keys a read of every row of `dataset`'s `subjects` answers through
/// `member`.
fn range_keys(
    member: &meridian_lake::spread::Spread,
    dataset: &str,
    subjects: &[&str],
) -> meridian_lake::store::Result<Vec<String>> {
    let mut keys: Vec<String> = member
        .read(&Query {
            datasets: vec![dataset.to_string()],
            data_type: DataType::Trade,
            subjects: subjects.iter().map(|s| s.to_string()).collect(),
            kinds: vec![],
            interval_ns: 0,
            when: When::Range {
                from_ns: 0,
                until_ns: i64::MAX,
            },
            as_of_ns: 0,
            published_as_of_ns: 0,
            latest_only: false,
        })?
        .iter()
        .map(|row| row.meta().row_key.clone())
        .collect();
    keys.sort();
    Ok(keys)
}

/// Every connection of `stores`' pools taken, each waiting on a lock held
/// by what is answered: the database out of those members' reach until it
/// is rolled back and the holders joined.
fn out_of_reach(
    url: &Scratch,
    stores: &[&Arc<PostgresStore>],
    partition: &str,
) -> (postgres::Client, Vec<std::thread::JoinHandle<()>>) {
    let mut admin = postgres::Client::connect(url, postgres::NoTls).unwrap();
    admin
        .batch_execute("BEGIN; LOCK TABLE lake_partition IN ACCESS EXCLUSIVE MODE")
        .unwrap();
    let held = stores
        .iter()
        .flat_map(|store| [*store, *store])
        .map(|store| {
            let (store, partition) = (store.clone(), partition.to_string());
            std::thread::spawn(move || drop(store.range_tip(&partition)))
        })
        .collect();
    std::thread::sleep(std::time::Duration::from_millis(300));
    (admin, held)
}

/// A range split in the database it moved to sends no read on to every
/// database (the review of 2026-11-23 of the lake's routing, finding 4):
/// its row there is closed and stands at or past the pointer its source
/// keeps, which it satisfies. Counted as held nowhere, the pointer sent
/// every read of the dataset to every database for good, and refused it
/// while any was out of reach -- here a third, which holds nothing of it.
#[test]
fn a_range_split_where_it_moved_to_sends_no_read_to_every_database() {
    use meridian_changes::groups::Groups;
    let urls = databases(3);
    let m = members(&urls);
    let dataset = owned_by_zero_of("coinbase-1:t", 3);
    let (low, high) = halves_of("lake-split-after");
    let lower = halved(&m, &dataset, &low, &high);
    moved_to_one(&m, &lower);
    m.one
        .split_partition(&lower, "admin-1", "busier where it went", 1_300)
        .unwrap();
    m.one
        .record(&dataset, vec![trade(&dataset, "l2", &low, 13)], 1_400)
        .unwrap();
    m.zeros[0].pin_owners().unwrap();
    // A member whose view was read after all of it.
    let groups = Groups::new(0, 3).unwrap();
    let stores = stores_of(&urls, groups, 2);
    let reader = meridian_lake::spread::Spread::new(stores.clone(), groups);
    reader.read_owners_again().unwrap();
    assert_eq!(
        range_keys(&reader, &dataset, &[&low, &high]).unwrap(),
        ["h0", "l0", "l1", "l2"]
    );
    let (mut admin, held) = out_of_reach(&urls[2], &[&stores[2]], &lower);
    let started = std::time::Instant::now();
    let read = range_keys(&reader, &dataset, &[&low, &high]);
    let took = started.elapsed();
    admin.batch_execute("ROLLBACK").unwrap();
    for holder in held {
        holder.join().unwrap();
    }
    assert_eq!(
        read.ok(),
        Some(
            ["h0", "l0", "l1", "l2"]
                .iter()
                .map(|key| key.to_string())
                .collect()
        ),
        "a range split where it moved to sent the read to a database holding nothing of it"
    );
    assert!(
        took < std::time::Duration::from_millis(500),
        "it waited {took:?} on that database"
    );
}

/// A dataset held away from its group's home -- the database count raised,
/// the dataset not moved home yet -- is read where it is held on that
/// pool's own wait, as on main (the review of 2026-11-23 of the lake's
/// routing, finding 1; its probe P1): its home, asked first, has begun
/// nothing of it, so the database holding it is one the read needs, never
/// one asked for 1 s at most and then gone without.
#[test]
fn a_dataset_held_away_from_its_home_is_read_while_its_databases_pool_is_busy() {
    use meridian_changes::groups::Groups;
    let urls = two_databases();
    let dataset = (0..)
        .map(|n| unique(&format!("coinbase-1:pa{n}")))
        .find(|d| meridian_changes::groups::owner(d, 2) == 1)
        .unwrap();
    let subject = unique("LCL-PA");
    {
        let member = |group: u32| {
            let store = PostgresStore::connect(&urls[0], 4)
                .unwrap()
                .with_groups(Groups::new(group, 2).unwrap());
            store.adopt().unwrap();
            store
        };
        let (zero, one) = (member(0), member(1));
        zero.pin_owners().unwrap();
        one.record(
            &dataset,
            vec![trade(&dataset, "before", &subject, 10)],
            1_000,
        )
        .unwrap();
    }
    let groups = Groups::new(0, 2).unwrap();
    let stores: Vec<Arc<PostgresStore>> = urls
        .iter()
        .enumerate()
        .map(|(d, url)| {
            Arc::new(
                PostgresStore::connect(url, 2)
                    .unwrap()
                    .with_groups(groups)
                    .with_databases(2)
                    .in_database(d as u32),
            )
        })
        .collect();
    let reader = meridian_lake::spread::Spread::new(stores.clone(), groups);
    reader.read_owners_again().unwrap();
    assert_eq!(
        range_keys(&reader, &dataset, &[&subject]).ok(),
        Some(vec!["before".to_string()])
    );
    let mut admin = postgres::Client::connect(&urls[0], postgres::NoTls).unwrap();
    admin
        .batch_execute("BEGIN; LOCK TABLE lake_partition IN ACCESS EXCLUSIVE MODE")
        .unwrap();
    let held: Vec<_> = (0..2)
        .map(|_| {
            let (store, d) = (stores[0].clone(), dataset.clone());
            std::thread::spawn(move || drop(store.range_tip(&d)))
        })
        .collect();
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(3_000));
        admin.batch_execute("ROLLBACK").unwrap();
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    let busy = range_keys(&reader, &dataset, &[&subject]);
    release.join().unwrap();
    for h in held {
        h.join().unwrap();
    }
    assert_eq!(
        busy.ok(),
        Some(vec!["before".to_string()]),
        "a dataset held away from its home was not read while its database's pool was busy"
    );
}

/// A member that has read nothing, another database out of reach for as
/// long, waits on it in no read of a dataset its home writes (the review of
/// 2026-11-23 of the lake's routing, finding 2; its probe P3): the home is
/// asked first and holds the dataset's ranges, so no other is asked -- not
/// for 1 s in each of a read's two looks, on every read, as nothing was
/// settled by a look that could not ask every database.
#[test]
fn a_member_that_has_read_nothing_waits_on_no_database_out_of_reach() {
    use meridian_changes::groups::Groups;
    let urls = two_databases();
    let m = members(&urls);
    let dataset = owned_by_zero("coinbase-1:c");
    let (low, high) = halves_of("lake-cold");
    halved(&m, &dataset, &low, &high);
    let groups = Groups::new(0, 2).unwrap();
    let stores = stores_of(&urls, groups, 2);
    let fresh = meridian_lake::spread::Spread::new(stores.clone(), groups);
    let mut admin = postgres::Client::connect(&urls[1], postgres::NoTls).unwrap();
    admin
        .batch_execute("BEGIN; LOCK TABLE lake_partition IN ACCESS EXCLUSIVE MODE")
        .unwrap();
    let held: Vec<_> = (0..2)
        .map(|_| {
            let (store, d) = (stores[1].clone(), dataset.clone());
            std::thread::spawn(move || drop(store.range_tip(&d)))
        })
        .collect();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let mut took = Vec::new();
    for _ in 0..3 {
        let t = std::time::Instant::now();
        let read = latest_keys(&fresh, &dataset, &low);
        took.push(t.elapsed());
        assert_eq!(read.ok(), Some(vec!["l0".to_string(), "l1".to_string()]));
    }
    admin.batch_execute("ROLLBACK").unwrap();
    for h in held {
        h.join().unwrap();
    }
    assert!(
        took[2] < std::time::Duration::from_millis(500),
        "each read of a dataset database 0 writes waited on database 1: {took:?}"
    );
}

/// A dataset no database has begun is answered by its home, empty, and
/// once every database has been asked and holds nothing of it, by its home
/// alone (the review of 2026-11-23 of the lake's routing, finding 6; main
/// answered so once it had looked). Its home having begun nothing of it,
/// every other database is asked once, on its pool's own wait, for a
/// dataset held there away from home; none holding any, the member keeps
/// that, as a write's routing does, and another database out of reach
/// afterwards neither slows nor refuses the read.
#[test]
fn a_dataset_never_written_is_answered_empty_by_its_home() {
    use meridian_changes::groups::Groups;
    let urls = two_databases();
    let m = members(&urls);
    let (written, never) = (owned_by_zero("coinbase-1:n"), owned_by_zero("coinbase-1:e"));
    let (low, high) = halves_of("lake-never");
    halved(&m, &written, &low, &high);
    let groups = Groups::new(0, 2).unwrap();
    let stores = stores_of(&urls, groups, 2);
    let reader = meridian_lake::spread::Spread::new(stores.clone(), groups);
    reader.read_owners_again().unwrap();
    assert_eq!(latest_keys(&reader, &never, &low).unwrap(), [""; 0]);
    let (mut admin, held) = out_of_reach(&urls[1], &[&stores[1]], &never);
    let started = std::time::Instant::now();
    let read = latest_keys(&reader, &never, &low);
    let took = started.elapsed();
    admin.batch_execute("ROLLBACK").unwrap();
    for holder in held {
        holder.join().unwrap();
    }
    assert_eq!(
        read.ok(),
        Some(Vec::new()),
        "a dataset no database has begun was not answered by its home"
    );
    assert!(
        took < std::time::Duration::from_millis(500),
        "it waited {took:?} on another database"
    );
}

/// A range two of a read's databases would both answer is refused
/// UNAVAILABLE, never answered twice (the review of 2026-11-23 of the
/// lake's routing, finding 3; its probe P2; on main too). Between a
/// switch's pointing the range in at its target and its source's letting it
/// go, the source's row is written and the target's a pointer to its own
/// group: each database answers the range, and a mover that dies there
/// leaves it so. Planted here as that window stands.
#[test]
fn a_range_two_databases_would_both_answer_is_refused_never_doubled() {
    use meridian_changes::groups::Groups;
    let urls = two_databases();
    let m = members(&urls);
    let dataset = owned_by_zero("coinbase-1:w");
    let (low, high) = halves_of("lake-window");
    let lower = halved(&m, &dataset, &low, &high);
    let upper = format!("{dataset}/8000-ffff");
    moved_to_one(&m, &upper);
    moved_to_one(&m, &lower);
    let mut c0 = postgres::Client::connect(&urls[0], postgres::NoTls).unwrap();
    let mut c1 = postgres::Client::connect(&urls[1], postgres::NoTls).unwrap();
    let g0_epoch: i64 = c0
        .query_one(
            "SELECT max(epoch) FROM lake_partition WHERE epoch >= 0",
            &[],
        )
        .unwrap()
        .get(0);
    c0.execute(
        "UPDATE lake_partition SET epoch = $2, writer_group = 0 WHERE dataset = $1",
        &[&lower, &g0_epoch],
    )
    .unwrap();
    c1.execute(
        "UPDATE lake_partition SET epoch = -1 WHERE dataset = $1",
        &[&lower],
    )
    .unwrap();
    let groups = Groups::new(0, 2).unwrap();
    let reader = meridian_lake::spread::Spread::new(stores_of(&urls, groups, 2), groups);
    reader.read_owners_again().unwrap();
    let read = range_keys(&reader, &dataset, &[&low, &high]);
    assert!(
        matches!(read, Err(meridian_lake::store::StoreError::Unavailable(_))),
        "a range read in two databases was answered: {read:?}"
    );
}

/// A member's view of the writer groups read again under a read refuses
/// nothing (the review of 2026-11-23 of the lake's routing, finding 5). A
/// range gone on from the database it first moved to leaves a pointer
/// there; a read by a view from before either move finds that database
/// only by following the first pointer, and reads nothing from it. Looked
/// at again after its rows by the view read since, the read was not sent
/// there, so the ranges let go differed and it was refused though nothing
/// moved. What a read is held to is what the databases it read answered.
#[test]
fn a_view_read_again_under_a_read_refuses_nothing() {
    use meridian_changes::groups::Groups;
    use meridian_changes::moves::points;
    let urls = databases(3);
    let m = members(&urls);
    let dataset = owned_by_zero_of("coinbase-1:v", 3);
    let (low, high) = halves_of("lake-view-again");
    let lower = halved(&m, &dataset, &low, &high);
    // A member whose view was read before the range moved.
    let groups = Groups::new(0, 3).unwrap();
    let reader = Arc::new(meridian_lake::spread::Spread::new(
        stores_of(&urls, groups, 2),
        groups,
    ));
    reader.read_owners_again().unwrap();
    moved_to_one(&m, &lower);
    // What a move on from database 2, long ago, left there: its pointer.
    postgres::Client::connect(&urls[2], postgres::NoTls)
        .unwrap()
        .execute(
            "INSERT INTO lake_partition (dataset, head, epoch, lo, hi, writer_group)
             VALUES ($1, 1, -1, 0, 32767, 1)",
            &[&lower],
        )
        .unwrap();
    assert_eq!(
        groups.owner_by(&lower, &dataset),
        0,
        "the view, from before"
    );
    let again = reader.clone();
    points::arm(
        "lake.read.routed",
        &dataset,
        Arc::new(move |_| {
            again
                .read_owners_again()
                .map_err(|failed| failed.to_string())?;
            Ok(())
        }),
    );
    let read = latest_keys(&reader, &dataset, &low);
    points::disarm("lake.read.routed", &dataset);
    assert_eq!(groups.owner_by(&lower, &dataset), 1, "the view, read again");
    assert_eq!(
        read.ok(),
        Some(vec!["l0".to_string(), "l1".to_string()]),
        "a read whose member's view was read again under it was refused"
    );
}

/// The lake's lock budget (v31a.17; the proposal of 2026-11-23): Postgres
/// takes a lock a relation a statement touches and holds it to commit, a
/// month's table is 14 relations, and the lock table is the whole
/// cluster's, 64 a connection unless the database is told otherwise. So a
/// statement whose locks grow with the months a dataset keeps fails every
/// other statement beside it once the history is long enough: dev's lake
/// answered 53200, out of shared memory, at a fund's 29 years.
///
/// A history of 32 years of months is written here as a backfill sends it,
/// a few months a batch, while the lake's own month-less reads run beside
/// it: its rows per dataset, read on retention's timer, and the subject's
/// latest. Nothing is refused, and the most locks a backend of each
/// statement class held is read from `pg_locks`:
///
/// - the rows per dataset hold the catalogue's alone, whatever the
///   history's length (they read every month of every dataset before, and
///   were the first refused on dev);
/// - the write and the latest read are KNOWN GAPS, held here to what they
///   take today so neither grows. A write reads its subject's last
///   sequence across its range's every month (`previous_of_each`; M4's
///   subject head, v32). The latest read looks in the rows of every month
///   for a row naming the subject after its first: `lake_latest` keeps no
///   later subject, so a split into places and bodies bounds nothing yet.
///   Each is 14 relations a month of history, where its budget is the
///   leaves it touches.
///
/// On a database of its own: the budget is read from `pg_locks` by
/// database, and the lock table it would exhaust is the cluster's.
///
/// Ignored until the two gaps close (run it alone with `-- --ignored`):
/// beside the suite's other tests, on one cluster, the write's and the
/// latest read's locks together fill the lock table, and it -- or a test
/// beside it -- is answered 53200. That is the failure it is here to show.
/// What holds today whatever the history's length is asserted, not
/// ignored, by `a_statement_holds_its_own_months_locks_beside_a_long_history`.
#[test]
#[ignore = "known gaps (v31a.17): the write's subject read and the latest read lock every month"]
fn a_long_history_written_under_reads_stays_within_its_lock_budget() {
    use std::sync::atomic::AtomicBool;
    // A month's table, its TOAST table and index, and its 13 indexes, each
    // under the two modes a write that makes it takes.
    const A_LEAF: i64 = 2 * 16;
    // The catalogue's tables and their indexes a statement reads beside.
    const BESIDE: i64 = 80;
    // A write makes and writes at most eight months a transaction.
    const A_WRITE: i64 = 8 * A_LEAF + BESIDE;
    // The rows per dataset are the catalogue's figures: no leaf.
    const ROWS: i64 = BESIDE;
    // A subject's latest is one row of one month.
    const LATEST: i64 = A_LEAF + BESIDE;
    // A known gap's read: a month's table and its 13 indexes, each month.
    const A_MONTH_READ: i64 = 14;
    const MONTHS: i64 = 384;

    let made = databases(1);
    let named = |name: &str| {
        let joined = if made[0].contains('?') { "&" } else { "?" };
        PostgresStore::connect(&format!("{}{joined}application_name={name}", &*made[0]), 2).unwrap()
    };
    let (writer, rows, latest) = (named("llb_write"), named("llb_rows"), named("llb_latest"));
    let dataset = unique("lockbudget");
    let done = Arc::new(AtomicBool::new(false));

    // Each class's backends' most locks held at once, sampled while the
    // history is written.
    let sampler = {
        let (url, done) = (made[0].to_string(), done.clone());
        std::thread::spawn(move || {
            let mut raw = postgres::Client::connect(&url, postgres::NoTls).unwrap();
            let mut most: std::collections::BTreeMap<String, i64> = Default::default();
            while !done.load(Ordering::SeqCst) {
                for row in raw
                    .query(
                        "SELECT a.application_name, max(l.held)
                           FROM (SELECT pid, count(*) AS held FROM pg_locks
                                  WHERE locktype = 'relation'
                                    AND database = (SELECT oid FROM pg_database
                                                     WHERE datname = current_database())
                                  GROUP BY pid) l
                           JOIN pg_stat_activity a USING (pid)
                          WHERE a.application_name LIKE 'llb_%'
                          GROUP BY 1",
                        &[],
                    )
                    .unwrap()
                {
                    let at = most.entry(row.get(0)).or_default();
                    *at = (*at).max(row.get::<_, i64>(1));
                }
            }
            most
        })
    };
    let reading = |read: Box<dyn Fn() -> Option<String> + Send>| {
        let done = done.clone();
        std::thread::spawn(move || {
            let mut refused = Vec::new();
            while !done.load(Ordering::SeqCst) {
                refused.extend(read());
            }
            refused
        })
    };
    let readers = vec![
        reading(Box::new(move || {
            rows.rows_in_force()
                .err()
                .map(|failed| format!("{failed:?}"))
        })),
        reading({
            let query = latest_query(&dataset, "INS-1", true);
            Box::new(move || {
                latest
                    .read(&query)
                    .err()
                    .map(|failed| format!("{failed:?}"))
            })
        }),
    ];

    // One row a month, six months a batch, oldest first: 2_629_800 s is a
    // mean month, from 1990-01-15.
    let month_ns = 2_629_800_i64 * 1_000_000_000;
    let from_ns = 631_152_000_i64 * 1_000_000_000 + 14 * 86_400 * 1_000_000_000;
    let mut refused = Vec::new();
    for batch in 0..MONTHS / 6 {
        let rows = (batch * 6..batch * 6 + 6)
            .map(|m| trade(&dataset, &format!("m{m}"), "INS-1", from_ns + m * month_ns))
            .collect();
        if let Err(failed) = writer.record(&dataset, rows, 1) {
            refused.push(format!("batch {batch}: {failed:?}"));
        }
    }
    done.store(true, Ordering::SeqCst);
    for reader in readers {
        refused.extend(reader.join().unwrap());
    }
    let most = sampler.join().unwrap();
    eprintln!("LOCKS lake: the most a backend held, by statement class: {most:?}");

    let mut raw = postgres::Client::connect(&made[0], postgres::NoTls).unwrap();
    let leaves: i64 = raw
        .query_one(
            "SELECT count(*) FROM pg_class WHERE relkind = 'r' AND relname ~ '^lake_row_[0-9a-f]{16}_'",
            &[],
        )
        .unwrap()
        .get(0);
    assert!(
        leaves >= 360,
        "the history is {leaves} months, short of 30 years"
    );
    refused.truncate(3);
    assert!(
        refused.is_empty(),
        "refused under its own locks: {refused:?}"
    );
    assert_eq!(
        writer
            .read(&latest_query(&dataset, "INS-1", false))
            .unwrap()
            .len() as i64,
        MONTHS,
        "every month's row is kept"
    );
    let gap = leaves * A_MONTH_READ;
    for (class, budget) in [
        ("llb_rows", ROWS),
        ("llb_write", A_WRITE + gap),
        ("llb_latest", LATEST + gap),
    ] {
        let held = most.get(class).copied().unwrap_or(0);
        assert!(
            held <= budget,
            "{class} held {held} locks at once over {leaves} months; it is held to {budget} \
             ({most:?})"
        );
    }
}

/// The relation locks the backend of `application` holds while it waits
/// on a lock, by mode: read from `pg_locks` once it is seen waiting.
fn locks_held_waiting(
    raw: &mut postgres::Client,
    application: &str,
) -> std::collections::BTreeMap<String, i64> {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let pid: i32 = loop {
        let waiting = raw
            .query_opt(
                "SELECT pid FROM pg_stat_activity
                  WHERE application_name = $1 AND wait_event_type = 'Lock' LIMIT 1",
                &[&application],
            )
            .unwrap();
        if let Some(row) = waiting {
            break row.get(0);
        }
        assert!(
            std::time::Instant::now() < until,
            "{application} was never seen waiting on a lock"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    raw.query(
        "SELECT mode, count(*) FROM pg_locks
          WHERE pid = $1 AND locktype = 'relation' AND granted GROUP BY mode",
        &[&pid],
    )
    .unwrap()
    .iter()
    .map(|row| (row.get(0), row.get(1)))
    .collect()
}

/// `url` with its sessions named `application`.
fn named_store(url: &str, application: &str) -> PostgresStore {
    let joined = if url.contains('?') { "&" } else { "?" };
    PostgresStore::connect(&format!("{url}{joined}application_name={application}"), 2).unwrap()
}

/// The locks only a month's making takes, of those `held` by mode: on its
/// table, its TOAST table and index and its 13 indexes, ACCESS EXCLUSIVE.
fn making(held: &std::collections::BTreeMap<String, i64>) -> i64 {
    held.get("AccessExclusiveLock").copied().unwrap_or(0)
}

/// A batch spanning more months than one transaction makes has them made
/// before it, a few a transaction (v31a.17), and is recorded whole: its
/// own transaction makes none, and holds no lock of a month's making.
///
/// The batch is held at its end -- one of its keys is held already, and
/// its row of `lake_latest` locked here -- and its backend's locks read
/// from `pg_locks`. Its 40 months' rows are still written, and its
/// subject's last sequence read, in the one transaction: 14 relations a
/// month each, the KNOWN GAPS `insert_all` and `previous_of_each` of the
/// inventory (`leaves.rs`), so the whole is said and not held to a budget.
#[test]
fn a_batch_of_many_months_is_recorded_its_months_made_before_it() {
    // A month's table, its TOAST table and index, and its 13 indexes.
    const A_MONTH_MADE: i64 = 16;
    let (url, setup) = isolated("manymonths");
    let store = named_store(&url, "llb_manymonths");
    let dataset = unique("manymonths");
    let month_ns = 2_629_800_i64 * 1_000_000_000;
    let row = |m: i64| {
        trade(
            &dataset,
            &format!("m{m}"),
            "INS-1",
            (240 + m) * month_ns + 14 * 86_400 * 1_000_000_000,
        )
    };
    setup.record(&dataset, vec![row(0)], 1).unwrap();
    let mut raw = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let mut held = raw.transaction().unwrap();
    held.execute(
        "SELECT 1 FROM lake_latest WHERE dataset = $1 AND row_key = 'm0' FOR UPDATE",
        &[&dataset],
    )
    .unwrap();
    let rows: Vec<Observation> = std::iter::once(restate(row(0)))
        .chain((1..40).map(row))
        .collect();
    let writing = {
        let dataset = dataset.clone();
        std::thread::spawn(move || {
            let done = store.record(&dataset, rows, 2);
            (store, done)
        })
    };
    let mut beside = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let locks = locks_held_waiting(&mut beside, "llb_manymonths");
    held.commit().unwrap();
    let (store, done) = writing.join().unwrap();
    let done = done.unwrap();
    assert_eq!((done.recorded, done.restated), (39, 1));
    eprintln!(
        "LOCKS lake: a batch of 40 months held {} at its end: {locks:?}",
        locks.values().sum::<i64>()
    );
    assert!(
        making(&locks) <= 8 * A_MONTH_MADE,
        "the batch's own transaction held {} locks of months' making, past the eight months \
         one makes: {locks:?}",
        making(&locks)
    );
    let leaves: i64 = raw
        .query_one(
            "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = current_schema() AND c.relkind = 'r'
                AND c.relname ~ '^lake_row_[0-9a-f]{16}_'",
            &[],
        )
        .unwrap()
        .get(0);
    assert!(
        leaves >= 39,
        "{leaves} months made for 40 rows a month apart"
    );
    assert_eq!(
        store
            .read(&latest_query(&dataset, "INS-1", false))
            .unwrap()
            .len(),
        40
    );
}

/// The most relation locks a backend of each `llb_*` application held at
/// once in `url`'s database, sampled from `pg_locks` until `done`.
fn most_locks_held(
    url: &str,
    done: &Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<std::collections::BTreeMap<String, i64>> {
    let (url, done) = (url.to_string(), done.clone());
    std::thread::spawn(move || {
        let mut raw = postgres::Client::connect(&url, postgres::NoTls).unwrap();
        let mut most: std::collections::BTreeMap<String, i64> = Default::default();
        while !done.load(Ordering::SeqCst) {
            for row in raw
                .query(
                    "SELECT a.application_name, max(l.held)
                       FROM (SELECT pid, count(*) AS held FROM pg_locks
                              WHERE locktype = 'relation'
                                AND database = (SELECT oid FROM pg_database
                                                 WHERE datname = current_database())
                              GROUP BY pid) l
                       JOIN pg_stat_activity a USING (pid)
                      WHERE a.application_name LIKE 'llb_%'
                      GROUP BY 1",
                    &[],
                )
                .unwrap()
            {
                let at = most.entry(row.get(0)).or_default();
                *at = (*at).max(row.get::<_, i64>(1));
            }
        }
        most
    })
}

/// The lake's lock budget (v31a.17), where it holds whatever the history's
/// length: a statement's locks are its own months', never those of a long
/// history beside it, nor of the months its window leaves out.
///
/// One dataset keeps 32 years of months. Beside it, on a database of its
/// own, each statement class runs again and again while the most locks its
/// backend held is read from `pg_locks`:
///
/// - the rows per dataset, on retention's timer: the catalogue's alone;
/// - a write to another dataset, a year of months: its own months' (its
///   subject's last sequence is read across its own range's months, the
///   known gap M4's subject head closes in v32, and no other range's);
/// - the latest read of that other dataset: its own months' (it looks in
///   each for a row naming the subject after its first, the known gap
///   `lake_latest` closes once it keeps later subjects), and no other's;
/// - a read of one month of the long history, by its window: that month's.
///   Its rows in force were chosen within the window and then fetched from
///   every month the dataset keeps, 14 relations each.
#[test]
fn a_statement_holds_its_own_months_locks_beside_a_long_history() {
    use std::sync::atomic::AtomicBool;
    // A month's table, its TOAST table and index, and its 13 indexes, each
    // under the two modes a write that makes it takes.
    const A_LEAF: i64 = 2 * 16;
    // The catalogue's tables and their indexes a statement reads beside.
    const BESIDE: i64 = 80;
    // A month's table and its 13 indexes, read.
    const A_MONTH_READ: i64 = 14;
    // A write makes and writes at most eight months a transaction, and
    // reads its subject's last sequence across its own range's months
    // (KNOWN GAP: M4's subject head, v32).
    const OWN_MONTHS: i64 = 12;
    const A_WRITE: i64 = 8 * A_LEAF + BESIDE + A_MONTH_READ * OWN_MONTHS;
    const ROWS: i64 = BESIDE;
    // One row of one month.
    const ONE_MONTH: i64 = A_LEAF + BESIDE;
    // The latest read finds its row by `lake_latest`, and looks in its own
    // datasets' every month for a row naming the subject after its first
    // (KNOWN GAP: `lake_latest` keeps no later subject).
    const A_LATEST: i64 = ONE_MONTH + A_MONTH_READ * OWN_MONTHS;
    const MONTHS: i64 = 384;

    let made = databases(1);
    let named = |name: &str| {
        let joined = if made[0].contains('?') { "&" } else { "?" };
        PostgresStore::connect(&format!("{}{joined}application_name={name}", &*made[0]), 2).unwrap()
    };
    let (long, short) = (unique("lockbudget-long"), unique("lockbudget-short"));
    // 2_629_800 s is a mean month, from 1990-01-15.
    let month_ns = 2_629_800_i64 * 1_000_000_000;
    let from_ns = 631_152_000_i64 * 1_000_000_000 + 14 * 86_400 * 1_000_000_000;

    // The long history: its range and first month made by its first row,
    // every later month by the function a write makes one with.
    named("lake_setup")
        .record(&long, vec![trade(&long, "m0", "INS-1", from_ns)], 1)
        .unwrap();
    let mut raw = postgres::Client::connect(&made[0], postgres::NoTls).unwrap();
    for m in 1..MONTHS {
        let month = format!("{:04}-{:02}", 1990 + m / 12, m % 12 + 1);
        raw.execute("SELECT lake_make_month($1, $2)", &[&long, &month])
            .unwrap();
    }
    let leaves: i64 = raw
        .query_one(
            "SELECT count(*) FROM pg_class WHERE relkind = 'r' AND relname ~ '^lake_row_[0-9a-f]{16}_'",
            &[],
        )
        .unwrap()
        .get(0);
    assert!(
        leaves >= 360,
        "the history is {leaves} months, short of 30 years"
    );

    let done = Arc::new(AtomicBool::new(false));
    let sampler = most_locks_held(&made[0], &done);
    let reading = |read: Box<dyn Fn() -> Option<String> + Send>| {
        let done = done.clone();
        std::thread::spawn(move || {
            let mut refused = Vec::new();
            while !done.load(Ordering::SeqCst) {
                refused.extend(read());
            }
            refused
        })
    };
    let (writer, rows, latest, window) = (
        named("llb_write"),
        named("llb_rows"),
        named("llb_latest"),
        named("llb_window"),
    );
    let readers = vec![
        reading(Box::new(move || {
            rows.rows_in_force()
                .err()
                .map(|failed| format!("{failed:?}"))
        })),
        reading({
            let query = latest_query(&short, "INS-1", true);
            Box::new(move || {
                latest
                    .read(&query)
                    .err()
                    .map(|failed| format!("{failed:?}"))
            })
        }),
        reading({
            // The long history's first month, and no other.
            let mut query = latest_query(&long, "INS-1", false);
            query.when = When::Range {
                from_ns: from_ns - 86_400 * 1_000_000_000,
                until_ns: from_ns + 86_400 * 1_000_000_000,
            };
            Box::new(move || match window.read(&query) {
                Ok(read) if read.len() == 1 => None,
                other => Some(format!("the window's read: {other:?}")),
            })
        }),
    ];
    let mut refused = Vec::new();
    // The short dataset's keys written, then sent again and again with
    // their values changed: each a key held, whose latest version is read
    // from the month it is in (`latest_of_each`'s second statement).
    for pass in 0..12_i64 {
        for batch in 0..OWN_MONTHS / 6 {
            let rows = (batch * 6..batch * 6 + 6)
                .map(|m| {
                    let mut row = trade(&short, &format!("s{m}"), "INS-1", from_ns + m * month_ns);
                    row.meta_mut().valid_until_ns += pass;
                    row
                })
                .collect();
            match writer.record(&short, rows, 1 + pass) {
                Ok(done) if pass > 0 && done.restated != 6 => {
                    refused.push(format!("pass {pass}, batch {batch}: {done:?}"))
                }
                Ok(_) => {}
                Err(failed) => refused.push(format!("pass {pass}, batch {batch}: {failed:?}")),
            }
        }
    }
    // Each read sampled many times over.
    std::thread::sleep(std::time::Duration::from_secs(2));
    done.store(true, Ordering::SeqCst);
    for reader in readers {
        refused.extend(reader.join().unwrap());
    }
    let most = sampler.join().unwrap();
    eprintln!("LOCKS lake: the most a backend held beside {leaves} months: {most:?}");
    refused.truncate(3);
    assert!(refused.is_empty(), "refused: {refused:?}");
    for (class, budget) in [
        ("llb_rows", ROWS),
        ("llb_write", A_WRITE),
        ("llb_latest", A_LATEST),
        ("llb_window", ONE_MONTH),
    ] {
        let held = most.get(class).copied().unwrap_or(0);
        assert!(held > 0, "{class} was never seen holding a lock ({most:?})");
        assert!(
            held <= budget,
            "{class} held {held} locks at once beside {leaves} months; its budget is {budget} \
             ({most:?})"
        );
    }
}

/// A range's months' lock, as `leaves.rs` takes it to make one.
fn months_lock(partition: &str) -> i64 {
    let digest = meridian_changes::integrity::digest(partition.as_bytes());
    i64::from_be_bytes([
        0x6c, 0x61, 0x6b, 0x66, digest[0], digest[1], digest[2], digest[3],
    ])
}

/// The lake's lock budget (v31a.17; the review's F1): a transaction makes
/// at most eight months whatever its datasets and ranges, not eight a
/// range. One write-ahead batch of 20 datasets, each with one row in a
/// month not made yet, has every one made before it, and its own
/// transaction makes none.
///
/// The batch is held at its end -- on the head of a 21st dataset, the last
/// by name, locked here -- and its backend's locks read from `pg_locks`.
/// Its 20 months' rows are still written in the one transaction, 14
/// relations a month and as many again read (the KNOWN GAPS `insert_all`
/// and `previous_of_each`), so the whole is said and not held to a budget.
#[test]
fn a_write_ahead_batch_of_many_datasets_has_its_months_made_before_it() {
    use meridian_lake::store::{WriteAheadBatch, WriteAheadPosition};
    // A month's table, its TOAST table and index, and its 13 indexes.
    const A_MONTH_MADE: i64 = 16;
    let made = databases(1);
    let store = named_store(&made[0], "llb_stream");
    let stream = unique("STREAM");
    let last = unique("lockbudget-z");
    store
        .record(&last, vec![trade(&last, "z", "INS-1", 10)], 1)
        .unwrap();
    let mut raw = postgres::Client::connect(&made[0], postgres::NoTls).unwrap();
    let mut held = raw.transaction().unwrap();
    held.execute(
        "SELECT 1 FROM lake_partition WHERE dataset = $1 FOR UPDATE",
        &[&last],
    )
    .unwrap();
    let mut parts: Vec<(String, Vec<Observation>)> = (0..20)
        .map(|n| {
            let dataset = unique(&format!("lockbudget-a{n:02}"));
            let rows = vec![trade(&dataset, "k", "INS-1", 10)];
            (dataset, rows)
        })
        .collect();
    parts.push((last.clone(), vec![restate(trade(&last, "z", "INS-1", 10))]));
    let writing = std::thread::spawn(move || {
        store.record_write_ahead(
            WriteAheadBatch {
                stream,
                after: None,
                through: WriteAheadPosition {
                    sequence: 1,
                    at_ns: 1_000,
                },
                parts,
                ..Default::default()
            },
            5_000,
        )
    });
    let mut beside = postgres::Client::connect(&made[0], postgres::NoTls).unwrap();
    let locks = locks_held_waiting(&mut beside, "llb_stream");
    held.commit().unwrap();
    let done = writing.join().unwrap().unwrap();
    assert_eq!(
        done.iter()
            .map(|part| part.as_ref().map(|part| part.recorded + part.restated).ok())
            .collect::<Vec<_>>(),
        vec![Some(1); 21],
        "every part is recorded"
    );
    eprintln!(
        "LOCKS lake: a write-ahead batch of 20 datasets held {} at its end: {locks:?}",
        locks.values().sum::<i64>()
    );
    assert!(
        making(&locks) <= 8 * A_MONTH_MADE,
        "the batch's own transaction held {} locks of months' making, past the eight months \
         one makes: {locks:?}",
        making(&locks)
    );
}

/// `months` keys of a new dataset recorded under a subject across as many
/// months, the dataset then split so that subject's hash and another's
/// fall in different children, and the first subject merged into the
/// other: the dataset, and the keys restated under the subject that
/// stays. Each is anchored by the first subject, so it is written in that
/// subject's child, where no month is made yet -- and placed by the hash
/// of the subject it now names, in the other, before its batch.
fn keys_anchored_by_a_merged_subject(
    store: &PostgresStore,
    tag: &str,
    months: i64,
) -> (String, Vec<Observation>) {
    let dataset = unique(tag);
    let (merged, stays) = halves_of(&format!("LCL-{tag}"));
    let month_ns = 2_629_800_i64 * 1_000_000_000;
    let rows = |subject: &str| -> Vec<Observation> {
        (0..months)
            .map(|m| trade(&dataset, &format!("k{m}"), subject, (300 + m) * month_ns))
            .collect()
    };
    assert_eq!(
        store
            .record(&dataset, rows(&merged), 1_000)
            .unwrap()
            .recorded as i64,
        months
    );
    store
        .split_partition(&dataset, "admin-1", "two halves", 1_100)
        .unwrap();
    store.keep_alias(&merged, &stays, 1_200).unwrap();
    let again = rows(&stays).into_iter().map(restate).collect();
    (dataset, again)
}

/// The review's F2 (v31a.17): a batch refused for more month tables than
/// its transaction makes -- its keys anchored by a subject merged into the
/// one they now name, so their months were not those made before it -- is
/// recorded all the same: the months it was refused for are made and it is
/// recorded again, once, inside the store. Its caller is not answered the
/// refusal as words with no retry-after.
#[test]
fn a_batch_whose_keys_a_merged_subject_anchors_is_recorded_its_months_owed_made() {
    let (_url, store) = isolated("owedmonths");
    let (dataset, again) = keys_anchored_by_a_merged_subject(&store, "owed", 10);
    let done = store.record(&dataset, again, 1_300).unwrap();
    assert_eq!((done.recorded, done.restated), (0, 10));
}

/// The review's F1 (v31a.17), the bound itself: the months a transaction
/// makes are counted across its datasets. A write-ahead batch of two
/// datasets, five months of each not made where its keys are anchored (as
/// [`keys_anchored_by_a_merged_subject`]: six keys' months, the first two
/// in one), is refused whole -- ten in one transaction, five a range --
/// and recorded when taken again, the months it was refused for made
/// before it.
#[test]
fn a_write_ahead_batch_is_refused_past_eight_months_made_across_its_datasets() {
    use meridian_lake::store::{WriteAheadBatch, WriteAheadPosition};
    let (_url, store) = isolated("acrossdatasets");
    let stream = unique("STREAM");
    let parts: Vec<(String, Vec<Observation>)> = ["across-a", "across-b"]
        .into_iter()
        .map(|tag| keys_anchored_by_a_merged_subject(&store, tag, 6))
        .collect();
    let batch = || WriteAheadBatch {
        stream: stream.clone(),
        after: None,
        through: WriteAheadPosition {
            sequence: 1,
            at_ns: 1_000,
        },
        parts: parts.clone(),
        ..Default::default()
    };
    let refused = store.record_write_ahead(batch(), 5_000);
    assert!(
        matches!(&refused, Err(meridian_lake::StoreError::Unavailable(said)) if said.contains("month tables")),
        "ten months made in one transaction: {refused:?}"
    );
    assert_eq!(store.write_ahead_position(&stream).unwrap(), None);
    let done = store.record_write_ahead(batch(), 5_001).unwrap();
    assert_eq!(
        done.iter()
            .map(|part| part.as_ref().map(|part| part.restated).ok())
            .collect::<Vec<_>>(),
        vec![Some(6), Some(6)]
    );
}

/// The review's F4 (v31a.17): months made before a batch are made a range
/// a transaction, so their making holds one range's months' lock at a
/// time. Holding two, taken in the locks' own order, it waited on the
/// second while a session holding that one -- a batch making its ranges'
/// months in its ranges' order -- waited on the first: a deadlock, one of
/// the two ended by the database.
///
/// Here another session holds the greater of two ranges' locks; a batch of
/// ten months across both ranges makes them first and waits on it; the
/// session then takes the lesser. Both are given, and the batch recorded.
#[test]
fn months_made_before_a_batch_hold_one_ranges_lock_at_a_time() {
    let (url, setup) = isolated("onerange");
    let store = named_store(&url, "llb_onerange");
    let dataset = unique("onerange");
    let (low, high) = halves_of("LCL-ONERANGE");
    setup
        .record(&dataset, vec![trade(&dataset, "first", &low, 10)], 1_000)
        .unwrap();
    setup
        .split_partition(&dataset, "admin-1", "two halves", 1_100)
        .unwrap();
    let mut locks = [
        months_lock(&format!("{dataset}/0000-7fff")),
        months_lock(&format!("{dataset}/8000-ffff")),
    ];
    locks.sort_unstable();
    let [lesser, greater] = locks;
    let mut raw = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let mut other = raw.transaction().unwrap();
    other
        .execute("SELECT pg_advisory_xact_lock($1)", &[&greater])
        .unwrap();
    let month_ns = 2_629_800_i64 * 1_000_000_000;
    let rows: Vec<Observation> = (0..10)
        .map(|m| {
            let subject = if m % 2 == 0 { &low } else { &high };
            trade(&dataset, &format!("k{m}"), subject, (300 + m) * month_ns)
        })
        .collect();
    let writing = {
        let dataset = dataset.clone();
        std::thread::spawn(move || store.record(&dataset, rows, 1_200))
    };
    let mut beside = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    locks_held_waiting(&mut beside, "llb_onerange");
    let taken = other.execute("SELECT pg_advisory_xact_lock($1)", &[&lesser]);
    assert!(
        taken.is_ok(),
        "the session holding one range's lock was not given the other's: {taken:?}"
    );
    other.commit().unwrap();
    let done = writing.join().unwrap();
    assert_eq!(
        done.as_ref().map(|done| done.recorded).ok(),
        Some(10),
        "{done:?}"
    );
}
