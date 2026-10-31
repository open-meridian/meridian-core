//! The lake's store against Postgres: the same rules the in-memory store
//! answers, as the database keeps them. Run by `make test-store`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

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
    // A day recorded closed and re-checked open is no longer counted: each
    // count reads only the rows recorded flagged or re-checked since
    // (migration 6, ruling 816), by the flag as known now.
    store
        .amend_closed_day(&[ClosedDayAmendment {
            dataset: dataset.clone(),
            sequence: done.rows[0].meta().sequence,
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

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let at = unsafe { System.alloc(layout) };
            if !at.is_null() {
                let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
                PEAK.fetch_max(live, Ordering::Relaxed);
            }
            at
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) };
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
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
    use meridian_pb::v1::{DatasetDeclaration, DatasetLicence, Decimal, ObservationMode};
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
                    licence_default: Some(DatasetLicence {
                        kept: true,
                        ..Default::default()
                    }),
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
    fn record_fixture(store: &PostgresStore, dataset: &str, underlying: &str, shape: Shape) {
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
                batch.push(Observation::Quote(Quote {
                    meta: meta(dataset, &format!("{k}:q:{at}"), underlying, k, at),
                    bid: usd(bid),
                    ask: usd(bid + 5),
                    bid_quantity: decimal(10 + (round % 90) as u64, 0),
                    ask_quantity: decimal(12, 0),
                    ..Default::default()
                }));
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
        let lake = service::serve(bus.clone(), store as Arc<dyn Store>, Arc::new(SystemClock));
        lake.configured(&configuration(dataset));
        Page {
            bus,
            resolves,
            underlying,
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
