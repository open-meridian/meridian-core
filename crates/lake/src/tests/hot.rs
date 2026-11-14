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
