//! Quality states in the lake (contract v28, W10.4, W10.6, W10.12; rulings
//! 421 to 424 and 619): a row recorded with the edge's state; an invalid row
//! left out of a default and a side-by-side read before the latest is
//! chosen, the dataset left with nothing named source silent and asked
//! nothing; answered, with its state and reasons, to a read naming its
//! dataset with `include_quarantined`, which a default read is refused; and
//! a `signal` plugin's assessment its own record, raising the row's
//! effective state, a lower one refused.

use meridian_bus::read_refusal_status;
use meridian_domain::v1::{
    DatasetEntitlement, Price, PricesRecordedEvent, QualityAssessment, QualityReason, QualityState,
    RecordQualityAssessmentsRequest, SourceChoice, SubjectRef, UnansweredReason,
};
use prost::Message;

use super::*;
use crate::service::ReadRequest;

const SIGNAL: &str = "signal-1";

fn judged(mut price: Price, state: QualityState, reasons: &[QualityReason]) -> Price {
    let meta = price.meta.as_mut().unwrap();
    meta.quality = state as i32;
    meta.quality_reasons = reasons.iter().map(|r| *r as i32).collect();
    price
}

fn named(dataset: &str, include_quarantined: bool) -> SourceChoice {
    SourceChoice {
        named: vec![dataset.into()],
        include_quarantined,
        ..Default::default()
    }
}

fn fields_of(refusal: &str) -> Vec<String> {
    read_refusal_status(refusal)
        .map(|(_, fields, _)| fields)
        .unwrap_or_default()
}

/// The read, or its refusal.
fn try_read(
    h: &Harness,
    sources: SourceChoice,
    at_ns: i64,
    date: &str,
) -> Result<service::ReadAnswer, String> {
    tokio::task::block_in_place(|| {
        h.lake.read(
            ReadRequest {
                data_type: DataType::Price,
                subjects: vec![SubjectRef {
                    entity_id: BTC.into(),
                    ..Default::default()
                }],
                kinds: vec![],
                interval_ns: 0,
                sources: Some(sources),
                at_ns,
                business_date: date.into(),
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
            &envelope("reporting-1", true, ""),
        )
    })
}

fn assess(h: &Harness, by: &str, items: Vec<QualityAssessment>) -> Result<u32, String> {
    tokio::task::block_in_place(|| {
        h.lake
            .record_quality_assessments(
                RecordQualityAssessmentsRequest { assessments: items },
                &envelope(by, true, ""),
            )
            .map(|reply| reply.recorded)
    })
}

fn assessment(row_key: &str, version: u64, state: QualityState) -> QualityAssessment {
    QualityAssessment {
        dataset: "coinbase-1:daily".into(),
        row_key: row_key.into(),
        version,
        state: state as i32,
        reasons: vec![QualityReason::PriceOutlier as i32],
        assessed_by_instance: SIGNAL.into(),
        ..Default::default()
    }
}

/// The harness, with a signal plugin entitled to coinbase's daily.
fn with_signal() -> Harness {
    let h = harness();
    let mut config = configuration(true, &[]);
    config.entitlements.push(DatasetEntitlement {
        dataset: "coinbase-1:daily".into(),
        instance: SIGNAL.into(),
        allowed: true,
        ..Default::default()
    });
    h.lake.configured(&config);
    h
}

fn quality_of(price: &Observation) -> (i32, Vec<i32>) {
    (price.meta().quality, price.meta().quality_reasons.clone())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_row_is_recorded_heard_with_its_state_and_left_out_of_a_default_read() {
    let h = harness();
    let mut heard = h
        .bus
        .subscribe_every("platform.lake.coinbase-1:daily.event.prices-recorded");
    record(
        &h,
        "coinbase-1",
        vec![judged(
            close("coinbase-1", "BTC:2026-10-07", BTC, "2026-10-07", "1"),
            QualityState::Invalid,
            &[QualityReason::ImplausibleTimestamp],
        )],
        "",
    )
    .unwrap();
    // Heard as recorded, flagged, not dropped (619 (3)).
    let event =
        PricesRecordedEvent::decode(&heard.recv().await.unwrap().envelope.payload[..]).unwrap();
    assert_eq!(
        event.price.unwrap().meta.unwrap().quality,
        QualityState::Invalid as i32
    );

    // A default read leaves it out: coinbase named quarantined (contract
    // v29, 619 (1)), asked nothing, kraken asked in its place.
    let default = try_read(&h, SourceChoice::default(), 0, "2026-10-07").unwrap();
    assert!(default.rows.is_empty());
    assert!(default.unanswered.iter().any(
        |u| u.dataset == "coinbase-1:daily" && u.reason == UnansweredReason::Quarantined as i32
    ));
    // Side by side, the same.
    let side = try_read(
        &h,
        SourceChoice {
            side_by_side: true,
            ..Default::default()
        },
        0,
        "2026-10-07",
    )
    .unwrap();
    assert!(side.rows.is_empty());

    // Named without include_quarantined: left out, quarantined.
    let plain = try_read(&h, named("coinbase-1:daily", false), 0, "2026-10-07").unwrap();
    assert!(plain.rows.is_empty());
    assert_eq!(
        plain.unanswered[0].reason,
        UnansweredReason::Quarantined as i32
    );
    // Named with it: answered, its state and reasons (W10.6).
    let quarantined = try_read(&h, named("coinbase-1:daily", true), 0, "2026-10-07").unwrap();
    assert_eq!(quarantined.rows.len(), 1);
    assert_eq!(
        quality_of(&quarantined.rows[0]),
        (
            QualityState::Invalid as i32,
            vec![QualityReason::ImplausibleTimestamp as i32]
        )
    );

    // include_quarantined on a default or side-by-side read: refused naming it.
    for sources in [
        SourceChoice {
            include_quarantined: true,
            ..Default::default()
        },
        SourceChoice {
            side_by_side: true,
            include_quarantined: true,
            ..Default::default()
        },
    ] {
        let refused = try_read(&h, sources, 0, "2026-10-07").unwrap_err();
        assert_eq!(fields_of(&refused), vec!["sources.include_quarantined"]);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_default_read_fails_over_past_a_quarantined_dataset_and_says_why() {
    let h = harness();
    record(
        &h,
        "coinbase-1",
        vec![judged(
            close("coinbase-1", "BTC:2026-10-07", BTC, "2026-10-07", "1"),
            QualityState::Invalid,
            &[QualityReason::CrossedQuote],
        )],
        "",
    )
    .unwrap();
    record(
        &h,
        "kraken-1",
        vec![close(
            "kraken-1",
            "BTC:2026-10-07",
            BTC,
            "2026-10-07",
            "62000",
        )],
        "",
    )
    .unwrap();
    let answer = try_read(&h, SourceChoice::default(), 0, "2026-10-07").unwrap();
    assert_eq!(answer.rows.len(), 1);
    assert_eq!(answer.rows[0].dataset(), "kraken-1:daily");
    // No read silently mixes sources: the dataset above it named.
    assert!(answer.unanswered.iter().any(
        |u| u.dataset == "coinbase-1:daily" && u.reason == UnansweredReason::Quarantined as i32
    ));
    assert!(
        h.store.want_changes().unwrap().is_empty(),
        "a quarantined dataset is asked nothing"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_suspicious_row_is_served_flagged_and_the_last_valid_row_stands_for_an_invalid_latest() {
    let h = harness();
    let mut earlier = close("coinbase-1", "BTC:1", BTC, "", "100");
    earlier.meta.as_mut().unwrap().valid_from_ns = NOW - 2 * DAY;
    let earlier = judged(
        earlier,
        QualityState::Suspicious,
        &[QualityReason::Duplicate],
    );
    let later = judged(
        close("coinbase-1", "BTC:2", BTC, "", "999999"),
        QualityState::Invalid,
        &[QualityReason::CrossedQuote],
    );
    record(&h, "coinbase-1", vec![earlier, later], "").unwrap();
    // The latest in force, the invalid row left out before it is chosen
    // (619 (4)): the earlier, suspicious row, served flagged (422).
    let answer = try_read(&h, SourceChoice::default(), 0, "").unwrap();
    assert_eq!(answer.rows.len(), 1);
    let Observation::Price(price) = &answer.rows[0] else {
        panic!()
    };
    assert_eq!(amount(price), "100");
    assert_eq!(
        quality_of(&answer.rows[0]),
        (
            QualityState::Suspicious as i32,
            vec![QualityReason::Duplicate as i32]
        )
    );
    // Named with include_quarantined, the invalid row is the one in force.
    let named = try_read(&h, named("coinbase-1:daily", true), 0, "").unwrap();
    let Observation::Price(price) = &named.rows[0] else {
        panic!()
    };
    assert_eq!(amount(price), "999999");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_row_not_valid_names_each_reason() {
    let h = harness();
    let refused = record(
        &h,
        "coinbase-1",
        vec![judged(
            close("coinbase-1", "k", BTC, "2026-10-07", "1"),
            QualityState::Suspicious,
            &[QualityReason::Unspecified],
        )],
        "",
    )
    .unwrap_err();
    assert_eq!(
        fields_of(&refused),
        vec!["prices[0].meta.quality_reasons[0]"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signals_assessment_is_its_own_record_raising_the_rows_effective_state() {
    let h = with_signal();
    record(
        &h,
        "coinbase-1",
        vec![close(
            "coinbase-1",
            "BTC:2026-10-07",
            BTC,
            "2026-10-07",
            "62000",
        )],
        "",
    )
    .unwrap();
    let before = h.clock.now_ns();
    h.clock.advance(1_000);
    assert_eq!(
        assess(
            &h,
            SIGNAL,
            vec![assessment("BTC:2026-10-07", 1, QualityState::Invalid)]
        )
        .unwrap(),
        1
    );
    // Never a rewrite: the row's body is as recorded; the assessment beside it.
    let kept = h.store.assessments().unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(
        (
            kept[0].sequence,
            kept[0].by_instance.as_str(),
            kept[0].state
        ),
        (1, SIGNAL, QualityState::Invalid as i32)
    );

    // A default read leaves it out from now on.
    let default = try_read(&h, SourceChoice::default(), 0, "2026-10-07").unwrap();
    assert!(default.rows.is_empty());
    // Named with include_quarantined: its effective state and reasons.
    let quarantined = try_read(&h, named("coinbase-1:daily", true), 0, "2026-10-07").unwrap();
    assert_eq!(
        quality_of(&quarantined.rows[0]),
        (
            QualityState::Invalid as i32,
            vec![QualityReason::PriceOutlier as i32]
        )
    );
    // As of before the assessment, the row read as it was then.
    let then = tokio::task::block_in_place(|| {
        h.lake.read(
            ReadRequest {
                data_type: DataType::Price,
                subjects: vec![SubjectRef {
                    entity_id: BTC.into(),
                    ..Default::default()
                }],
                kinds: vec![],
                interval_ns: 0,
                sources: Some(SourceChoice::default()),
                at_ns: 0,
                business_date: "2026-10-07".into(),
                valid_from_ns: 0,
                valid_until_ns: 0,
                as_of_ns: before,
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
            &envelope("reporting-1", true, ""),
        )
    })
    .unwrap();
    assert_eq!(then.rows.len(), 1);

    // States only escalate: a lower one refused naming its state.
    let lower = assess(
        &h,
        SIGNAL,
        vec![assessment("BTC:2026-10-07", 1, QualityState::Suspicious)],
    )
    .unwrap_err();
    assert_eq!(fields_of(&lower), vec!["assessments[0].state"]);
    // Within a batch too: an earlier item's state counts.
    record(
        &h,
        "coinbase-1",
        vec![close(
            "coinbase-1",
            "BTC:2026-10-06",
            BTC,
            "2026-10-06",
            "61000",
        )],
        "",
    )
    .unwrap();
    let batch = assess(
        &h,
        SIGNAL,
        vec![
            assessment("BTC:2026-10-06", 1, QualityState::Invalid),
            assessment("BTC:2026-10-06", 1, QualityState::Suspicious),
        ],
    )
    .unwrap_err();
    assert_eq!(fields_of(&batch), vec!["assessments[1].state"]);
    assert_eq!(h.store.assessments().unwrap().len(), 1, "refused whole");

    // A version the lake does not hold, and a row the plugin may not read:
    // refused naming the row key.
    let missing = assess(
        &h,
        SIGNAL,
        vec![assessment("BTC:2026-10-07", 2, QualityState::Invalid)],
    )
    .unwrap_err();
    assert_eq!(fields_of(&missing), vec!["assessments[0].row_key"]);
    let stranger = assess(
        &h,
        "reporting-2",
        vec![assessment("BTC:2026-10-07", 1, QualityState::Invalid)],
    )
    .unwrap_err();
    assert_eq!(fields_of(&stranger), vec!["assessments[0].row_key"]);
    // An unspecified state and no reasons: refused naming each.
    let mut unspecified = assessment("BTC:2026-10-07", 1, QualityState::Unspecified);
    let refused = assess(&h, SIGNAL, vec![unspecified.clone()]).unwrap_err();
    assert_eq!(fields_of(&refused), vec!["assessments[0].state"]);
    unspecified.state = QualityState::Invalid as i32;
    unspecified.reasons.clear();
    let refused = assess(&h, SIGNAL, vec![unspecified]).unwrap_err();
    assert_eq!(fields_of(&refused), vec!["assessments[0].reasons"]);
}

