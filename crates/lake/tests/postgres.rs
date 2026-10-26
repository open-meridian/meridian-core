//! The lake's store against Postgres: the same rules the in-memory store
//! answers, as the database keeps them. Run by `make test-store`.

use std::sync::atomic::{AtomicU64, Ordering};

use meridian_clock::{Clock, SystemClock};
use meridian_domain::date::Date;
use meridian_domain::v1::{
    EntitlementsChangedEvent, Money, ObservationMeta, Price, Source, SourcePriority, SubjectRef,
};
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
    Observation::Price(Price {
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
    })
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
fn trade(dataset: &str, row_key: &str, subject: &str, valid_from_ns: i64) -> Observation {
    Observation::Trade(meridian_domain::v1::Trade {
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
    })
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

fn outcome(dataset: &str, subject: &str, stage: i32, value: u64) -> Observation {
    Observation::Outcome(meridian_domain::v1::Outcome {
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
            valid_from_ns: 100 + stage as i64,
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
    })
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
        .dated_on(&dataset, &["2026-10-13".to_string()])
        .unwrap();
    assert_eq!(on.len(), 1);
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
        store.read_dataset(&dataset, DataType::Price, 0).unwrap()[0]
            .meta()
            .quality,
        QualityState::Invalid as i32
    );
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
    // `r` restated to an earlier time: its version in force is no latest.
    let restated = store
        .record(
            &dataset,
            vec![chain_trade(&dataset, "r", &subject, "A", 5)],
            1_100,
        )
        .unwrap();
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
        vec!["a2", "a3", "b1", "b2", "t2"],
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
    assert_eq!(state("a3"), QualityState::Invalid as i32, "as recorded");
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
    let url = std::env::var("MERIDIAN_TEST_DATABASE_URL").unwrap();
    let mut raw = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    raw.execute(
        "UPDATE lake_row SET latest_key = NULL, quality = NULL WHERE dataset = $1",
        &[&dataset],
    )
    .unwrap();
    let unkeyed = store.read(&latest_query(&dataset, &subject, true)).unwrap();
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
#[test]
fn a_chains_latest_loads_one_row_per_series() {
    let store = store();
    let measured = measure_chain(&store, 200, 100);
    eprintln!("bench: {}", said(&measured));
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

/// The guard's floors: a quarter of the stated capacity at 20,000 rows
/// (2,449 rows a second recorded, 102,429 read, the latest in 222 ms), and
/// four times its latest read.
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
