//! Never blind (contract v30, W10.6; the resilience spec's section 8; rule
//! 1050): a default read of the latest now passes over a stalled or a
//! quarantined source for the next by priority, as it passes over a silent
//! one; each switch, and each switch back, is its own `SourceSwitched`
//! record in the lake's `control` partition, recorded once and read by
//! `ListSourceSwitches`; a reader member, which holds no liveness, hands a
//! read only liveness can settle to the writer.

use meridian_domain::v1::{
    DatasetLiveness, ListSourceSwitchesRequest, QualityReason, QualityState, RecordLivenessRequest,
    SourceSwitchReason, SourceSwitched,
};

use super::*;

const COINBASE: &str = "coinbase-1:daily";
const KRAKEN: &str = "kraken-1:daily";
const SECOND: i64 = 1_000_000_000;
/// coinbase's stale ceiling, its data plugin admin's `dataset_thresholds`.
const CEILING: i64 = 10 * SECOND;

/// coinbase's stale ceiling set.
fn configured(h: &Harness) {
    let mut config = configuration(true, &[]);
    for dataset in &mut config.datasets {
        if dataset.dataset == COINBASE {
            dataset.stale_after_ns = CEILING;
        }
    }
    h.lake.configured(&config);
}

/// coinbase first, kraken second, for closes; coinbase's stale ceiling set.
fn prioritised(h: &Harness) {
    configured(h);
    tokio::task::block_in_place(|| {
        h.lake.set_priority(
            SetSourcePriorityRequest {
                data_type: "meridian.v1.Price".into(),
                kind: PriceKind::Close as i32,
                datasets: vec![COINBASE.into(), KRAKEN.into()],
                note: "Coinbase first.".into(),
                against_updated_at_ns: 0,
            },
            &envelope("dashboard-1", false, "local|ada"),
        )
    })
    .unwrap();
}

/// A default read of BTC's latest close now: where its row came from.
fn served_from(h: &Harness) -> (Vec<String>, Vec<(String, i32)>) {
    let answer = read(
        h,
        "reporting-1",
        &[BTC],
        "",
        SourceChoice {
            default: true,
            ..Default::default()
        },
        0,
    );
    (
        answer
            .rows
            .iter()
            .map(|row| row.dataset().to_string())
            .collect(),
        answer
            .unanswered
            .iter()
            .map(|n| (n.dataset.clone(), n.reason))
            .collect(),
    )
}

fn switches(h: &Harness) -> Vec<SourceSwitched> {
    tokio::task::block_in_place(|| {
        h.lake
            .list_source_switches(ListSourceSwitchesRequest::default())
    })
    .unwrap()
    .switches
}

fn close_now(h: &Harness, instance: &str, row_key: &str, amount: &str) {
    let mut price = close(instance, row_key, BTC, "", amount);
    let meta = price.meta.as_mut().unwrap();
    meta.business_date = String::new();
    meta.valid_from_ns = h.clock.now_ns() - SECOND;
    meta.valid_until_ns = 0;
    record(h, instance, vec![price], "").unwrap();
}

fn alive(h: &Harness, at: i64) {
    tokio::task::block_in_place(|| {
        h.lake.record_liveness(
            RecordLivenessRequest {
                liveness: vec![DatasetLiveness {
                    dataset: COINBASE.into(),
                    venue_id: String::new(),
                    alive_at_ns: at,
                    instance: String::new(),
                }],
            },
            &envelope("coinbase-1", true, ""),
        )
    })
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_source_is_passed_over_and_its_switch_and_switch_back_recorded_once() {
    let h = harness();
    prioritised(&h);
    close_now(&h, "coinbase-1", "c1", "61000");
    close_now(&h, "kraken-1", "k1", "61010");
    assert_eq!(served_from(&h).0, vec![COINBASE.to_string()]);
    assert!(switches(&h).is_empty(), "nothing passed over yet");

    // Past coinbase's ceiling with no sign of life: stalled.
    h.clock.advance(CEILING + SECOND);
    close_now(&h, "kraken-1", "k2", "61020");
    let (rows, unanswered) = served_from(&h);
    assert_eq!(
        rows,
        vec![KRAKEN.to_string()],
        "the next source by priority"
    );
    assert!(
        unanswered.contains(&(COINBASE.to_string(), UnansweredReason::SourceSilent as i32)),
        "{unanswered:?}"
    );
    let recorded = switches(&h);
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    let switch = &recorded[0];
    assert_eq!(
        (
            switch.from_dataset.as_str(),
            switch.to_dataset.as_str(),
            switch.reason
        ),
        (COINBASE, KRAKEN, SourceSwitchReason::Stalled as i32)
    );
    assert!(switch.subjects.is_empty(), "a stall is the whole source's");
    assert_eq!(switch.data_type, "meridian.v1.Price");
    assert_eq!(switch.kind, PriceKind::Close as i32);
    assert_eq!(switch.switched_at_ns, h.clock.now_ns());
    // Read again: switched already, nothing more recorded.
    served_from(&h);
    assert_eq!(switches(&h).len(), 1);

    // Its source alive again: served from again, and switched back once.
    alive(&h, h.clock.now_ns());
    assert_eq!(served_from(&h).0, vec![COINBASE.to_string()]);
    served_from(&h);
    let recorded = switches(&h);
    assert_eq!(recorded.len(), 2, "{recorded:?}");
    let back = &recorded[0];
    assert_eq!(
        back.reason,
        SourceSwitchReason::Restored as i32,
        "newest first"
    );
    assert_eq!(
        (back.from_dataset.as_str(), back.to_dataset.as_str()),
        (KRAKEN, COINBASE)
    );

    // Each is a change of the control partition, as a follower reads it.
    let control = h
        .store
        .read_changes(meridian_changes::CONTROL, 0, meridian_changes::PAGE_MOST)
        .unwrap();
    let kinds: Vec<&str> = control
        .changes
        .iter()
        .filter(|c| c.kind == crate::store::CONTROL_SWITCH)
        .map(|c| c.kind.as_str())
        .collect();
    assert_eq!(kinds.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_source_whose_latest_is_quarantined_is_passed_over_for_the_subject_alone() {
    let h = harness();
    prioritised(&h);
    close_now(&h, "coinbase-1", "c1", "61000");
    close_now(&h, "kraken-1", "k1", "61010");
    // coinbase's latest held invalid: its last good row is older, and the
    // next source answers instead, never the quarantined one as live.
    h.clock.advance(SECOND);
    let mut bad = close("coinbase-1", "c2", BTC, "", "1");
    let meta = bad.meta.as_mut().unwrap();
    meta.business_date = String::new();
    meta.valid_from_ns = h.clock.now_ns();
    meta.valid_until_ns = 0;
    meta.quality = QualityState::Invalid as i32;
    meta.quality_reasons = vec![QualityReason::CrossedQuote as i32];
    record(&h, "coinbase-1", vec![bad], "").unwrap();
    let (rows, unanswered) = served_from(&h);
    assert_eq!(rows, vec![KRAKEN.to_string()]);
    assert!(
        unanswered.contains(&(COINBASE.to_string(), UnansweredReason::Quarantined as i32)),
        "{unanswered:?}"
    );
    let recorded = switches(&h);
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0].reason, SourceSwitchReason::Quarantined as i32);
    assert_eq!(recorded[0].subjects, vec![BTC.to_string()]);

    // A good row after it: back to coinbase, one switch back for BTC.
    h.clock.advance(SECOND);
    close_now(&h, "coinbase-1", "c3", "61030");
    assert_eq!(served_from(&h).0, vec![COINBASE.to_string()]);
    let recorded = switches(&h);
    assert_eq!(recorded.len(), 2);
    assert_eq!(recorded[0].reason, SourceSwitchReason::Restored as i32);
    assert_eq!(recorded[0].subjects, vec![BTC.to_string()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn with_no_source_left_the_last_good_row_stands_named_and_the_switch_says_none_serves() {
    let h = harness();
    prioritised(&h);
    close_now(&h, "coinbase-1", "c1", "61000");
    h.clock.advance(CEILING + SECOND);
    let (rows, unanswered) = served_from(&h);
    // Never blank: the last good row, its age its own; never said live.
    assert_eq!(rows, vec![COINBASE.to_string()]);
    assert!(
        unanswered.contains(&(COINBASE.to_string(), UnansweredReason::SourceSilent as i32)),
        "{unanswered:?}"
    );
    let recorded = switches(&h);
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].to_dataset, "", "none left to serve");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_writer_taking_over_holds_what_was_recorded_and_records_none_of_it_again() {
    let h = harness();
    prioritised(&h);
    close_now(&h, "coinbase-1", "c1", "61000");
    h.clock.advance(CEILING + SECOND);
    close_now(&h, "kraken-1", "k1", "61010");
    served_from(&h);
    assert_eq!(switches(&h).len(), 1);
    // A new writer over the same record.
    let bus = Arc::new(Bus::single(
        "lake-2",
        Arc::new(MemoryBackend::new()),
        h.clock.clone(),
    ));
    stand_ins(&bus);
    let next = service::serve(bus, h.store.clone(), h.clock.clone());
    let again = Harness {
        bus: h.bus.clone(),
        store: h.store.clone(),
        clock: h.clock.clone(),
        lake: next,
    };
    configured(&again);
    served_from(&again);
    assert_eq!(switches(&again).len(), 1, "recorded once, whoever writes");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_as_of_a_time_or_naming_its_sources_switches_nothing() {
    let h = harness();
    prioritised(&h);
    close_now(&h, "coinbase-1", "c1", "61000");
    close_now(&h, "kraken-1", "k1", "61010");
    h.clock.advance(CEILING + SECOND);
    // Named: coinbase answers, as named.
    let named = read(
        &h,
        "reporting-1",
        &[BTC],
        "",
        SourceChoice {
            named: vec![COINBASE.into()],
            ..Default::default()
        },
        0,
    );
    assert_eq!(named.rows.len(), 1);
    assert_eq!(named.rows[0].dataset(), COINBASE);
    // As of a time: the record's past, no switch.
    let past = read(
        &h,
        "reporting-1",
        &[BTC],
        "",
        SourceChoice {
            default: true,
            ..Default::default()
        },
        h.clock.now_ns(),
    );
    assert_eq!(past.rows[0].dataset(), COINBASE);
    assert!(switches(&h).is_empty());
}
