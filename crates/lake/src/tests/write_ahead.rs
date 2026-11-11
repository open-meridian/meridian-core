//! The write-ahead stream at the lake's writer (contract v30, stage E; the
//! resilience spec's section 4): writes taken in the stream's order,
//! recorded with its position in one commit, answered as the lake answers a
//! write; and a writer behind the position the store holds refused and sent
//! back to it, so nothing the stream held is lost or recorded twice.

use super::*;
use crate::store::{StoreError, WriteAheadPosition};
use crate::write_ahead::{Taken, Writer};
use meridian_domain::v1::{RecordObservationsReply, RecordPricesRequest};

const STREAM: &str = "LAKE_WRITE_AHEAD";

fn written(sequence: u64, instance: &str, prices: Vec<Price>) -> Taken {
    let mut envelope = envelope(instance, true, "");
    envelope.payload_type = "meridian.v1.RecordPricesRequest".into();
    envelope.payload = RecordPricesRequest {
        prices,
        want_id: String::new(),
    }
    .encode_to_vec();
    Taken {
        sequence,
        at_ns: NOW + sequence as i64,
        topic: service::RECORD_PRICES.into(),
        reply_to: Some(format!("_INBOX.test.{sequence}")),
        after: None,
        envelope,
    }
}

fn reply(answer: &meridian_bus::HandlerReply) -> RecordObservationsReply {
    let (kind, bytes) = answer.as_ref().expect("answered");
    assert_eq!(kind, "meridian.v1.RecordObservationsReply");
    RecordObservationsReply::decode(&bytes[..]).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_from_the_stream_are_recorded_with_its_position_and_answered_as_the_lake_answers() {
    let h = harness();
    let mut heard = h
        .bus
        .subscribe_every("platform.lake.coinbase-1:daily.event.prices-recorded");
    let mut writer = tokio::task::block_in_place(|| Writer::start(h.lake.clone(), STREAM)).unwrap();
    assert_eq!(writer.position(), None);
    assert_eq!(
        writer.next(),
        1,
        "a writer with no position takes from the first"
    );
    let batch = vec![
        written(
            1,
            "coinbase-1",
            vec![close("coinbase-1", "BTC:07", BTC, "2026-10-07", "62000.5")],
        ),
        // A write the lake refuses: its row names another instance.
        written(
            2,
            "kraken-1",
            vec![close("coinbase-1", "BTC:07", BTC, "2026-10-07", "1")],
        ),
        // The same row key again in the same batch: the next version.
        written(
            3,
            "coinbase-1",
            vec![
                close("coinbase-1", "BTC:07", BTC, "2026-10-07", "62001"),
                close("coinbase-1", "USDC:07", USDC, "2026-10-07", "1"),
            ],
        ),
    ];
    let answers = tokio::task::block_in_place(|| writer.take(batch)).unwrap();
    assert_eq!(answers.len(), 3);
    assert_eq!(answers[0].0.as_deref(), Some("_INBOX.test.1"));
    let first = reply(&answers[0].1);
    assert_eq!((first.recorded, first.restated, first.unchanged), (1, 0, 0));
    assert!(answers[1]
        .1
        .as_ref()
        .unwrap_err()
        .contains("not the instance recording it"));
    let third = reply(&answers[2].1);
    assert_eq!((third.recorded, third.restated, third.unchanged), (1, 1, 0));
    assert_eq!(third.watermark.unwrap().partitions[0].sequence, 3);
    // The position the batch reached, committed with its rows.
    let held = WriteAheadPosition {
        sequence: 3,
        at_ns: NOW + 3,
    };
    assert_eq!(writer.position(), Some(held));
    assert_eq!(h.store.write_ahead_position(STREAM).unwrap(), Some(held));
    // Each row published once it committed, in order.
    for amount_said in ["62000.5", "62001"] {
        let event = meridian_domain::v1::PricesRecordedEvent::decode(
            &heard.recv().await.unwrap().envelope.payload[..],
        )
        .unwrap();
        let price = event.price.unwrap();
        if price.meta.as_ref().unwrap().row_key == "BTC:07" {
            assert_eq!(amount(&price), amount_said);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_writer_behind_the_held_position_is_refused_and_a_write_taken_again_is_recorded_once() {
    let h = harness();
    let mut writer = tokio::task::block_in_place(|| Writer::start(h.lake.clone(), STREAM)).unwrap();
    // A second writer that read the position before the first moved it: as
    // a writer whose last commits a database failover lost believes it is
    // further on than the store, this one believes it is behind.
    let mut stale = tokio::task::block_in_place(|| Writer::start(h.lake.clone(), STREAM)).unwrap();
    let btc = || close("coinbase-1", "BTC:07", BTC, "2026-10-07", "62000.5");
    tokio::task::block_in_place(|| writer.take(vec![written(1, "coinbase-1", vec![btc()])]))
        .unwrap();
    let refused =
        tokio::task::block_in_place(|| stale.take(vec![written(2, "coinbase-1", vec![btc()])]));
    assert!(
        matches!(refused, Err(StoreError::PositionMoved(_))),
        "{refused:?}"
    );
    assert_eq!(
        h.store
            .write_ahead_position(STREAM)
            .unwrap()
            .unwrap()
            .sequence,
        1
    );
    // Read again, it goes on from the position held, and the write it is
    // given again -- the same row key, the same value -- changes nothing.
    tokio::task::block_in_place(|| stale.reread()).unwrap();
    assert_eq!(stale.next(), 2);
    let answers =
        tokio::task::block_in_place(|| stale.take(vec![written(2, "coinbase-1", vec![btc()])]))
            .unwrap();
    let again = reply(&answers[0].1);
    assert_eq!((again.recorded, again.restated, again.unchanged), (0, 0, 1));
    assert_eq!(
        h.store
            .write_ahead_position(STREAM)
            .unwrap()
            .unwrap()
            .sequence,
        2
    );
}

#[test]
fn every_topic_the_stream_carries_is_one_the_lake_takes_from_it() {
    for topic in meridian_bus::write_ahead::TOPICS {
        let refused = service::rows_of(topic, &Envelope::default()).err();
        // An empty request decodes, as a request with no rows, or is refused
        // for its type: never as a topic the lake does not take.
        assert!(
            refused.is_none_or(|said| !said.contains("no write the lake takes")),
            "{topic}"
        );
    }
    assert!(service::rows_of(
        "platform.lake.command.record-liveness",
        &Envelope::default()
    )
    .unwrap_err()
    .contains("no write the lake takes"));
}

/// Each dataset's catalogue as `configuration` makes it, coinbase-1's
/// prices alone and declaring `backfill`, kraken-1's declaring nothing --
/// read as `none` (W10.14).
fn gap_configured(h: &Harness) {
    let mut event = configuration(true, &[]);
    for dataset in &mut event.datasets {
        let declared = dataset.declaration.as_mut().unwrap();
        declared.data_types = vec!["meridian.v1.Price".into()];
        if dataset.dataset == "coinbase-1:daily" {
            declared.gap_recovery = meridian_pb::v1::GapRecovery::Backfill as i32;
        }
    }
    h.lake.configured(&event);
}

fn at_valid(mut price: Price, valid_from_ns: i64) -> Price {
    price.meta.as_mut().unwrap().valid_from_ns = valid_from_ns;
    price
}

fn gaps(h: &Harness, dataset: &str, state: GapState) -> Vec<DatasetGap> {
    let (_, bytes) = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(
            h.bus.call(
                service::LIST_DATASET_GAPS,
                "meridian.v1.ListDatasetGapsRequest",
                ListDatasetGapsRequest {
                    dataset: dataset.into(),
                    state: state as i32,
                    ..Default::default()
                }
                .encode_to_vec(),
                None,
                None,
            ),
        )
    })
    .unwrap();
    ListDatasetGapsReply::decode(&bytes[..]).unwrap().gaps
}

use meridian_domain::v1::{
    DatasetGap, GapCause, GapState, ListDatasetGapsReply, ListDatasetGapsRequest,
};

#[tokio::test(flavor = "multi_thread")]
async fn a_writer_gone_past_the_bound_leaves_a_backfill_gap_asked_again_and_recovered() {
    let h = harness();
    tokio::task::block_in_place(|| gap_configured(&h));
    let mut wanted = h.bus.subscribe_every(service::OBSERVATIONS_WANTED);
    let mut writer = tokio::task::block_in_place(|| Writer::start(h.lake.clone(), STREAM)).unwrap();
    let price =
        |key: &str, valid: i64| at_valid(close("coinbase-1", key, BTC, "2026-10-07", "1"), valid);
    tokio::task::block_in_place(|| {
        writer.take(vec![written(
            1,
            "coinbase-1",
            vec![price("a", NOW - 30_000)],
        )])
    })
    .unwrap();
    // The writer gone past the stream's bound: 2 to 4 dropped. The next
    // write the sidecar sent says its last was 3, one of them.
    let mut next = written(5, "coinbase-1", vec![price("e", NOW - 10_000)]);
    next.after = Some(3);
    let answers = tokio::task::block_in_place(|| writer.take(vec![next])).unwrap();
    assert_eq!(
        reply(&answers[0].1).recorded,
        1,
        "the write after a gap is recorded"
    );
    // The gap, recorded and asked again of its data plugin, naming it.
    let open = gaps(&h, "", GapState::Unspecified);
    assert_eq!(open.len(), 1, "{open:?}");
    let gap = &open[0];
    assert!(
        gap.gap_id.starts_with("GAP-") && gap.gap_id.len() == 30,
        "{}",
        gap.gap_id
    );
    assert_eq!(gap.dataset, "coinbase-1:daily");
    assert_eq!(gap.subjects, vec![BTC.to_string()]);
    assert_eq!(
        (gap.valid_from_ns, gap.valid_until_ns),
        (NOW - 30_000, NOW - 10_000),
        "from the dataset's latest valid time before the gap to the write after it"
    );
    assert_eq!(gap.cause, GapCause::WriteAheadBound as i32);
    assert_eq!(gap.state, GapState::Refetching as i32);
    let want = ObservationsWantedEvent::decode(&wanted.recv().await.unwrap().envelope.payload[..])
        .unwrap();
    assert_eq!(want.gap_id, gap.gap_id);
    assert_eq!(want.dataset, "coinbase-1:daily");
    assert_eq!(want.data_type, "meridian.v1.Price");
    assert_eq!(
        (want.valid_from_ns, want.valid_until_ns),
        (NOW - 30_000, NOW - 10_000)
    );
    assert_eq!(want.subjects[0].entity_id, BTC);
    // The plugin answers it from its source's history: the gap recovered,
    // a row it already held recorded once.
    let answered = record(
        &h,
        "coinbase-1",
        vec![price("b", NOW - 25_000), price("e", NOW - 10_000)],
        &want.want_id,
    )
    .unwrap();
    assert_eq!((answered.recorded, answered.unchanged), (1, 1));
    let recovered = gaps(&h, "coinbase-1:daily", GapState::Recovered);
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].gap_id, gap.gap_id);
    assert!(gaps(&h, "", GapState::Refetching).is_empty());
    // Each state its own record, in the control partition too.
    let every = h.store.dataset_gaps().unwrap();
    assert_eq!(
        every.iter().map(|g| g.state).collect::<Vec<_>>(),
        vec![GapState::Refetching as i32, GapState::Recovered as i32]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gap_in_a_dataset_declaring_none_is_recorded_unrecovered_and_read_never_asked() {
    let h = harness();
    tokio::task::block_in_place(|| gap_configured(&h));
    let mut wanted = h.bus.subscribe_every(service::OBSERVATIONS_WANTED);
    let mut writer = tokio::task::block_in_place(|| Writer::start(h.lake.clone(), STREAM)).unwrap();
    let price =
        |key: &str, valid: i64| at_valid(close("kraken-1", key, BTC, "2026-10-07", "1"), valid);
    tokio::task::block_in_place(|| {
        writer.take(vec![written(1, "kraken-1", vec![price("a", NOW - 30_000)])])
    })
    .unwrap();
    // A writer started again after the gap: what it found is the store's.
    let mut writer = tokio::task::block_in_place(|| Writer::start(h.lake.clone(), STREAM)).unwrap();
    let mut next = written(9, "kraken-1", vec![price("i", NOW - 1_000)]);
    next.after = Some(8);
    tokio::task::block_in_place(|| writer.take(vec![next])).unwrap();
    let shown = gaps(&h, "kraken-1:daily", GapState::Unrecovered);
    assert_eq!(shown.len(), 1, "a gap is never silent");
    assert_eq!(
        (shown[0].valid_from_ns, shown[0].valid_until_ns),
        (NOW - 30_000, NOW - 1_000)
    );
    assert!(gaps(&h, "coinbase-1:daily", GapState::Unspecified).is_empty());
    // Never asked again: the source cannot be.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), wanted.recv())
            .await
            .is_err(),
        "no want for a gap its dataset cannot recover"
    );
    // Rows inside it are never invented: what was recorded is what is read.
    assert_eq!(h.store.heads().unwrap()["kraken-1:daily"], 2);
    // A later write of the dataset, not after a gap, finds none.
    let mut after = written(10, "kraken-1", vec![price("j", NOW)]);
    after.after = Some(9);
    tokio::task::block_in_place(|| writer.take(vec![after])).unwrap();
    assert_eq!(h.store.dataset_gaps().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gap_whose_want_is_declined_is_unrecovered() {
    let h = harness();
    tokio::task::block_in_place(|| gap_configured(&h));
    let mut wanted = h.bus.subscribe_every(service::OBSERVATIONS_WANTED);
    let mut writer = tokio::task::block_in_place(|| Writer::start(h.lake.clone(), STREAM)).unwrap();
    let mut first = written(
        2,
        "coinbase-1",
        vec![close("coinbase-1", "a", BTC, "2026-10-07", "1")],
    );
    first.after = Some(1);
    // No position yet and the stream's first is 2: 1 was dropped before any
    // writer took it.
    tokio::task::block_in_place(|| writer.take(vec![first])).unwrap();
    let want = ObservationsWantedEvent::decode(&wanted.recv().await.unwrap().envelope.payload[..])
        .unwrap();
    let mut asked = envelope("coinbase-1", true, "");
    asked.payload_type = "meridian.v1.DeclineWantRequest".into();
    let declined = DeclineWantRequest {
        want_id: want.want_id.clone(),
        subjects: want.subjects.clone(),
        reason: UnansweredReason::BeyondHistory as i32,
    };
    tokio::task::block_in_place(|| h.lake.decline(declined, &asked)).unwrap();
    let shown = gaps(&h, "", GapState::Unrecovered);
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].gap_id, want.gap_id);
}
