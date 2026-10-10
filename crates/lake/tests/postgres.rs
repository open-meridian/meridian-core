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
        store.counts().unwrap().get(&dataset).is_none(),
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
