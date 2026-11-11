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
