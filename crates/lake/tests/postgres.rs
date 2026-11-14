//! The lake's store against Postgres: the same rules the in-memory store
//! answers, as the database keeps them. Run by `make test-store`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

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
    let base = std::env::var("MERIDIAN_TEST_DATABASE_URL").expect("the test database");
    postgres::Client::connect(&base, postgres::NoTls)
        .expect("could not reach the test database")
        .batch_execute(&format!("CREATE SCHEMA {name}"))
        .expect("could not create a schema");
    let url = format!("{base}?options=-c%20search_path%3D{name}");
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
fn isolated(tag: &str) -> (String, PostgresStore) {
    let url = std::env::var("MERIDIAN_TEST_DATABASE_URL").expect("MERIDIAN_TEST_DATABASE_URL");
    let schema = unique(tag).replace(['-', ':'], "_").to_lowercase();
    let mut admin = postgres::Client::connect(&url, postgres::NoTls).expect("the test database");
    admin
        .batch_execute(&format!("CREATE SCHEMA {schema}"))
        .expect("a schema of its own");
    let joined = if url.contains('?') { "&" } else { "?" };
    let url = format!("{url}{joined}options=-csearch_path%3D{schema}");
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
    let base = std::env::var("MERIDIAN_TEST_DATABASE_URL").expect("the test database");
    postgres::Client::connect(&base, postgres::NoTls)
        .expect("could not reach the test database")
        .batch_execute(&format!("CREATE SCHEMA {name}"))
        .expect("could not create a schema");
    let url = format!("{base}?options=-c%20search_path%3D{name}");
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
/// are taken over by its groups.
fn two_databases() -> Vec<String> {
    let url = std::env::var("MERIDIAN_TEST_DATABASE_URL").expect("MERIDIAN_TEST_DATABASE_URL");
    let (server, _) = url
        .rsplit_once('/')
        .expect("a database URL names its database");
    let tag = unique("lake2").replace([':', '-'], "_");
    let mut admin = postgres::Client::connect(&url, postgres::NoTls).expect("the test database");
    (0..2)
        .map(|d| {
            let name = format!("{tag}_{d}");
            admin
                .batch_execute(&format!("CREATE DATABASE {name}"))
                .expect("a database made");
            let made = format!("{server}/{name}");
            PostgresStore::connect(&made, 2)
                .unwrap()
                .migrate(&SystemClock)
                .unwrap();
            made
        })
        .collect()
}

/// A member of writer group `group` of two, over both databases.
fn spread(urls: &[String], group: u32) -> meridian_lake::spread::Spread {
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
    String,
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
        .find(|dataset| meridian_changes::groups::owner(dataset, 2) == 0)
        .unwrap();
    let (low, high) = halves_of(tag);
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
    assert_eq!(
        zero.pin_owners().unwrap(),
        3,
        "the parent and both children"
    );
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
    let lake = meridian_lake::service::serve_grouped(
        bus,
        zero.clone() as Arc<dyn Store>,
        Arc::new(SystemClock),
        zero.groups(),
    );
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
    tokio::task::block_in_place(move || drop((lake, zero, one)));
}
