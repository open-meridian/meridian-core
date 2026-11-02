//! The lake's change log (step 3a): each batch's rows on its partition's
//! tail after commit, the same rows read back a page at a time, and what the
//! tail costs beside the row events every batch already publishes.

use meridian_changes::{catch_up, ReadChangesReply, ReadChangesRequest, ReadError};
use prost::Message;

use super::*;

fn ask(
    h: &Harness,
    partition: &str,
    after: u64,
    page_size: u64,
) -> Result<ReadChangesReply, ReadError> {
    let asked = ReadChangesRequest {
        partition: partition.into(),
        after_sequence: after,
        page_size,
    };
    let bus = h.bus.clone();
    let answered = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(bus.call(
            "platform.lake.internal.read-changes",
            "meridian.v1.ReadChangesRequest",
            asked.encode_to_vec(),
            None,
            None,
        ))
    });
    match answered {
        Ok((_, body)) => Ok(ReadChangesReply::decode(body.as_slice()).unwrap()),
        Err(failed) if meridian_changes::is_behind(&failed.to_string()) => Err(ReadError::Behind {
            after_sequence: after,
            lowest_kept: 0,
        }),
        Err(failed) => Err(ReadError::Unavailable(failed.to_string())),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn each_batch_is_on_its_partitions_tail_and_read_back_in_any_page_split() {
    let h = harness();
    h.lake.configured(&live_configuration());
    let mut tail = h.bus.subscribe_every("platform.lake.internal.changes.*");
    for at in 0..40 {
        let quotes: Vec<Observation> = (0..5)
            .map(|i| {
                Observation::Quote(quote(
                    &format!("q-{at}-{i}"),
                    NOW + at * 1_000 + i,
                    money("100.5", "USD", ""),
                    money("100.7", "USD", ""),
                ))
            })
            .collect();
        record_rows(&h, "quotes", quotes).unwrap();
    }
    // Every row on the tail, in order, under the writer's epoch.
    for sequence in 1..=200u64 {
        let heard = tail.recv().await.unwrap();
        assert_eq!(heard.envelope.payload_type, "meridian.v1.Change");
        let change = meridian_changes::Change::decode(heard.envelope.payload.as_slice()).unwrap();
        assert_eq!(change.partition, "coinbase-1:live");
        assert_eq!(change.sequence, sequence);
        assert_eq!(change.epoch, 1);
        assert_eq!(change.kind, "meridian.v1.Quote");
        let row = Observation::decode(DataType::Quote, &change.body).unwrap();
        assert_eq!(row.meta().sequence, sequence);
    }
    // The same rows read back, whatever the page.
    let kept = h
        .store
        .read_dataset_versions("coinbase-1:live", DataType::Quote)
        .unwrap();
    for page_size in [1u64, 300, 4096, 0] {
        let mut followed = Vec::new();
        let reached = catch_up(
            |after| ask(&h, "coinbase-1:live", after, page_size),
            0,
            |change| followed.push(Observation::decode(DataType::Quote, &change.body).unwrap()),
        )
        .unwrap();
        assert_eq!(reached, 200);
        assert_eq!(followed, kept, "page size {page_size}");
    }
    // The control partition is kept beside the datasets, empty until 3b.
    let control = ask(&h, "control", 0, 0).unwrap();
    assert!(control.changes.is_empty());
    assert_eq!(control.epoch, 1);
    assert!(ask(&h, "no-such:dataset", 0, 0).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_follower_behind_retention_is_refused_and_reloads_at_the_head() {
    let h = harness();
    h.lake.configured(&live_configuration());
    for at in 0..3 {
        h.clock.set(NOW + at * DAY);
        record_rows(
            &h,
            "quotes",
            vec![Observation::Quote(quote(
                &format!("q-{at}"),
                NOW + at * DAY,
                money("1", "USD", ""),
                None,
            ))],
        )
        .unwrap();
    }
    h.store
        .remove_before("coinbase-1:live", NOW + DAY, "retention", NOW + 3 * DAY)
        .unwrap();
    let refused = ask(&h, "coinbase-1:live", 0, 0).unwrap_err();
    assert!(matches!(refused, ReadError::Behind { .. }), "{refused:?}");
    // Reload: the partition's state at its head, then follow from there.
    let head = h.store.heads().unwrap()["coinbase-1:live"];
    let state = h
        .store
        .read_dataset_versions("coinbase-1:live", DataType::Quote)
        .unwrap();
    assert_eq!(state.len(), 2);
    let after = ask(&h, "coinbase-1:live", head, 0).unwrap();
    assert!(after.changes.is_empty());
    assert_eq!(after.lowest_kept, 2);
    // Just behind the floor is still followable.
    assert_eq!(ask(&h, "coinbase-1:live", 1, 0).unwrap().changes.len(), 2);
}

/// Step 3a's duplicate-traffic measurement (the brief's item 3): what the
/// change log's tail publishes beside the row events every batch already
/// does, for a ticking quote stream, a row a batch and ten a batch (the row
/// events conflate a batch's rows per key; the tail carries every row).
/// Printed for the report; held only to the tail carrying each row once.
#[tokio::test(flavor = "multi_thread")]
async fn the_tail_beside_the_row_events_is_measured() {
    for per_batch in [1i64, 10] {
        let h = harness();
        h.lake.configured(&live_configuration());
        let drain = |mut heard: meridian_bus::Subscription| {
            tokio::spawn(async move {
                let (mut messages, mut bytes) = (0u64, 0u64);
                while let Ok(Some(delivery)) =
                    tokio::time::timeout(std::time::Duration::from_millis(500), heard.recv()).await
                {
                    messages += 1;
                    bytes += delivery.envelope.encoded_len() as u64;
                }
                (messages, bytes)
            })
        };
        let events = drain(
            h.bus
                .subscribe_every("platform.lake.coinbase-1:live.event.quotes-recorded"),
        );
        let tail = drain(h.bus.subscribe_every("platform.lake.internal.changes.*"));
        let batches = 300i64;
        for at in 0..batches {
            let quotes: Vec<Observation> = (0..per_batch)
                .map(|i| {
                    Observation::Quote(quote(
                        &format!("m-{at}-{i}"),
                        NOW + at * 1_000 + i,
                        money("100.51", "USD", ""),
                        money("100.73", "USD", ""),
                    ))
                })
                .collect();
            record_rows(&h, "quotes", quotes).unwrap();
        }
        let rows = (batches * per_batch) as u64;
        let (event_messages, event_bytes) = events.await.unwrap();
        let (tail_messages, tail_bytes) = tail.await.unwrap();
        println!(
            "duplicate traffic, {per_batch} quote(s) a batch, {batches} batches: row events {event_messages} \
             messages, {event_bytes} bytes enveloped ({:.0} a row); change-log tail {tail_messages} \
             messages, {tail_bytes} bytes enveloped ({:.0} a row); the tail adds {:.0}% to the bytes \
             and {:.0}% to the messages the lake publishes",
            event_bytes as f64 / rows as f64,
            tail_bytes as f64 / rows as f64,
            100.0 * tail_bytes as f64 / event_bytes.max(1) as f64,
            100.0 * tail_messages as f64 / event_messages.max(1) as f64,
        );
        assert_eq!(tail_messages, rows);
    }
}
