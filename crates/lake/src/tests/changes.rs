//! The lake's change log (step 3a): each batch's rows on its partition's
//! tail after commit -- one compressed page a batch, and only while a
//! follower holds interest (step 3b, ruling 1062) -- the same rows read back
//! a page at a time, and what the tail costs beside the row events every
//! batch already publishes.

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
    // A follower asking is the interest that has the tail published.
    ask(&h, "control", i64::MAX as u64, 1).unwrap();
    for at in 0..40 {
        let quotes: Vec<Observation> = (0..5)
            .map(|i| {
                Observation::Quote(Box::new(quote(
                    &format!("q-{at}-{i}"),
                    NOW + at * 1_000 + i,
                    money("100.5", "USD", ""),
                    money("100.7", "USD", ""),
                )))
            })
            .collect();
        record_rows(&h, "quotes", quotes).unwrap();
    }
    // Every row on the tail, in order, under the writer's epoch: one page a
    // batch, following the last.
    let mut sequence = 0u64;
    for _batch in 0..40 {
        let heard = tail.recv().await.unwrap();
        assert_eq!(heard.envelope.payload_type, meridian_changes::TAIL_PAYLOAD);
        let page =
            meridian_changes::read_tail(&heard.envelope.payload_type, &heard.envelope.payload)
                .unwrap();
        assert_eq!(page.after_sequence, sequence);
        assert_eq!(page.changes.len(), 5);
        for change in &page.changes {
            sequence += 1;
            assert_eq!(change.partition, "coinbase-1:live");
            assert_eq!(change.sequence, sequence);
            assert_eq!(change.epoch, 1);
            assert_eq!(change.kind, "meridian.v1.Quote");
            let row = Observation::decode(DataType::Quote, &change.body).unwrap();
            assert_eq!(row.meta().sequence, sequence);
        }
        assert_eq!(page.head, sequence);
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
    // The control partition is kept beside the datasets: nothing yet.
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
            vec![Observation::Quote(Box::new(quote(
                &format!("q-{at}"),
                NOW + at * DAY,
                money("1", "USD", ""),
                None,
            )))],
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

/// The tail's traffic beside the row events every batch already publishes,
/// for a ticking quote stream, a row a batch and ten a batch (step 3a's
/// measurement, again after ruling 1062): with a follower, one compressed
/// page a batch; with none, as on a desktop, nothing. Printed for the
/// report; held to one page a batch, and to none unfollowed.
#[tokio::test(flavor = "multi_thread")]
async fn the_tail_beside_the_row_events_is_measured() {
    for (per_batch, followed) in [(1i64, true), (10, true), (1, false)] {
        let h = harness();
        h.lake.configured(&live_configuration());
        if followed {
            ask(&h, "control", i64::MAX as u64, 1).unwrap();
        }
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
                    Observation::Quote(Box::new(quote(
                        &format!("m-{at}-{i}"),
                        NOW + at * 1_000 + i,
                        money("100.51", "USD", ""),
                        money("100.73", "USD", ""),
                    )))
                })
                .collect();
            record_rows(&h, "quotes", quotes).unwrap();
        }
        let rows = (batches * per_batch) as u64;
        let (event_messages, event_bytes) = events.await.unwrap();
        let (tail_messages, tail_bytes) = tail.await.unwrap();
        println!(
            "tail traffic, {} follower, {per_batch} quote(s) a batch, {batches} batches: row events \
             {event_messages} messages, {event_bytes} bytes enveloped ({:.0} a row); change-log tail \
             {tail_messages} messages, {tail_bytes} bytes enveloped ({:.0} a row); the tail adds \
             {:.0}% to the bytes and {:.0}% to the messages the lake publishes",
            if followed { "a" } else { "no" },
            event_bytes as f64 / rows as f64,
            tail_bytes as f64 / rows as f64,
            100.0 * tail_bytes as f64 / event_bytes.max(1) as f64,
            100.0 * tail_messages as f64 / event_messages.max(1) as f64,
        );
        let _ = rows;
        assert_eq!(tail_messages, if followed { batches as u64 } else { 0 });
    }
}

/// Step 3b (ruling 1062's carried item): what is not a dataset's row -- an
/// assessment, a closed-day amendment, a priority, an alias -- is in the
/// `control` partition's log in the order committed, so a follower's quality
/// states, flags, priorities and aliases match the writer's; and a writer
/// another took over is refused it, as its batch is.
#[tokio::test(flavor = "multi_thread")]
async fn the_control_partition_carries_assessments_amendments_priorities_and_aliases() {
    use meridian_domain::v1::SourcePriority;
    let h = harness();
    h.lake.configured(&live_configuration());
    record_rows(
        &h,
        "quotes",
        vec![Observation::Quote(Box::new(quote(
            "c-1",
            NOW,
            money("1", "USD", ""),
            None,
        )))],
    )
    .unwrap();
    let store: &dyn Store = h.store.as_ref();
    store
        .assess(&[crate::store::Assessment {
            dataset: "coinbase-1:live".into(),
            sequence: 1,
            row_key: "k".into(),
            version: 1,
            state: 3,
            reasons: vec![1],
            by_instance: "signal-1".into(),
            at_ns: NOW + 1,
        }])
        .unwrap();
    store
        .amend_closed_day(&[crate::store::ClosedDayAmendment {
            dataset: "coinbase-1:live".into(),
            sequence: 1,
            row_key: String::new(),
            closed_day: None,
            why: "calendar 2 applied".into(),
            at_ns: NOW + 2,
        }])
        .unwrap();
    let priority = SourcePriority {
        data_type: "quote".into(),
        updated_at_ns: NOW + 3,
        ..Default::default()
    };
    store.set_priority(&priority, 0).unwrap().unwrap();
    store.keep_alias("INS-OLD", "INS-NEW", NOW + 4).unwrap();
    // Heard again, an alias changes nothing and is logged once.
    store.keep_alias("INS-OLD", "INS-NEW", NOW + 5).unwrap();
    let control = ask(&h, "control", 0, 0).unwrap();
    let kinds: Vec<&str> = control.changes.iter().map(|c| c.kind.as_str()).collect();
    assert_eq!(
        kinds,
        vec![
            crate::store::CONTROL_ASSESSMENT,
            crate::store::CONTROL_CLOSED_DAY,
            crate::store::CONTROL_PRIORITY,
            crate::store::CONTROL_ALIAS
        ]
    );
    assert_eq!(control.head, 4);
    assert_eq!(
        control
            .changes
            .iter()
            .map(|c| c.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    let set = SourcePriority::decode(control.changes[2].body.as_slice()).unwrap();
    assert_eq!(set, priority);
    // A page after a sequence starts after it.
    assert_eq!(ask(&h, "control", 2, 0).unwrap().changes.len(), 2);
    // Taken over by another writer: this one's next is refused.
    store
        .take_over(&meridian_changes::TookOver::unnamed("a test"))
        .unwrap();
    // This store now acts as the deposed writer, at the epoch it had.
    h.store.fence_for_test(1);
    assert!(store.keep_alias("INS-A", "INS-B", NOW + 6).is_err());
}
