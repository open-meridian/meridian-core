use super::*;

fn change(sequence: u64, size: usize) -> Change {
    Change {
        partition: "P0".into(),
        sequence,
        epoch: 1,
        origin: "DEP-1".into(),
        committed_at_ns: sequence as i64,
        kind: "rows".into(),
        body: vec![7; size],
    }
}

#[test]
fn a_page_holds_at_most_its_bytes_and_always_one_change() {
    let mut page = Page::new(1000);
    assert!(
        page.take(change(1, 5000)),
        "the first change is taken whatever its size"
    );
    assert!(!page.take(change(2, 10)), "and nothing after it once full");
    let mut page = Page::new(1000);
    let mut taken = 0;
    for sequence in 1..100 {
        if !page.take(change(sequence, 90)) {
            break;
        }
        taken += 1;
    }
    let bytes: usize = page.changes.iter().map(|c| c.encoded_len() + 4).sum();
    assert!(
        bytes <= 1000 && taken > 1,
        "{taken} changes in {bytes} bytes"
    );
}

#[test]
fn a_page_is_512_kib_at_most() {
    assert_eq!(page_most(0), PAGE_MOST);
    assert_eq!(page_most(10 * 1024 * 1024), PAGE_MOST);
    assert_eq!(page_most(4096), 4096);
    assert_eq!(PAGE_MOST, 524_288);
}

#[test]
fn behind_the_lowest_kept_is_refused_naming_after_sequence() {
    assert!(check_kept(0, 1, 10).is_ok());
    assert!(check_kept(4, 5, 10).is_ok());
    let refused = check_kept(3, 5, 10).unwrap_err();
    assert_eq!(
        refused,
        ReadError::Behind {
            after_sequence: 3,
            lowest_kept: 5
        }
    );
    assert!(is_behind(&refused.to_string()));
    assert!(refused.to_string().contains("after_sequence 3"));
    // Nothing kept and the follower at the head: nothing to miss.
    assert!(check_kept(10, 0, 10).is_ok());
    // Nothing kept and the follower behind the head: everything it lacks is gone.
    assert!(check_kept(3, 0, 10).is_err());
}

/// The log in any page split builds the same state: a follower reading a
/// partition page by page, whatever the page size, applies every change once
/// and in order.
#[test]
fn a_follower_given_the_log_in_any_page_split_applies_every_change_in_order() {
    let log: Vec<Change> = (1..=200)
        .map(|s| change(s, (s as usize * 37) % 900))
        .collect();
    for most in [1usize, 64, 700, 4096, 100_000, PAGE_MOST] {
        for start in [0u64, 1, 57, 199, 200] {
            let read = |after: u64| -> Result<ReadChangesReply, ReadError> {
                let mut page = Page::new(most);
                for change in log.iter().filter(|c| c.sequence > after) {
                    if !page.take(change.clone()) {
                        break;
                    }
                }
                Ok(ReadChangesReply {
                    changes: page.changes,
                    head: 200,
                    lowest_kept: 1,
                    epoch: 1,
                })
            };
            let mut applied = Vec::new();
            let reached = catch_up(read, start, |c| applied.push(c.sequence)).unwrap();
            assert_eq!(reached, 200);
            assert_eq!(
                applied,
                ((start + 1)..=200).collect::<Vec<_>>(),
                "page {most} from {start}"
            );
        }
    }
}

#[test]
fn a_log_out_of_order_is_refused_never_inferred_over() {
    let read = |_after: u64| -> Result<ReadChangesReply, ReadError> {
        Ok(ReadChangesReply {
            changes: vec![change(3, 1), change(2, 1)],
            head: 3,
            lowest_kept: 1,
            epoch: 1,
        })
    };
    assert!(catch_up(read, 0, |_| {}).is_err());
}

#[test]
fn a_fence_admits_its_own_epoch_alone() {
    let fence = Fence::new();
    assert_eq!(fence.epoch(), 0);
    assert_eq!(fence.adopt_once(3), 3);
    assert_eq!(fence.adopt_once(4), 3, "adopted once, not twice");
    assert!(fence.admits(3).is_ok());
    let refused = fence.admits(4).unwrap_err();
    assert_eq!(refused, Fenced { mine: 3, head: 4 });
    assert!(refused.to_string().starts_with(FENCED));
    fence.adopt(4);
    assert!(fence.admits(4).is_ok());
}

#[test]
fn topics_are_the_component_topics_approved() {
    assert_eq!(
        read_changes_topic("lake"),
        "platform.lake.internal.read-changes"
    );
    assert_eq!(
        changes_topic("book", "P0"),
        "platform.book.internal.changes.P0"
    );
    assert_eq!(
        every_changes_topic("reference"),
        "platform.reference.internal.changes.*"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn read_changes_is_served_on_its_component_topic() {
    struct Two;
    impl ChangeLog for Two {
        fn read_changes(
            &self,
            partition: &str,
            after: u64,
            most: usize,
        ) -> Result<ReadChangesReply, ReadError> {
            if partition != "P0" {
                return Err(ReadError::UnknownPartition(partition.into()));
            }
            check_kept(after, 1, 2)?;
            let mut page = Page::new(most);
            for s in (after + 1)..=2 {
                page.take(change(s, 10));
            }
            Ok(ReadChangesReply {
                changes: page.changes,
                head: 2,
                lowest_kept: 1,
                epoch: 1,
            })
        }
    }
    let bus = Arc::new(Bus::single(
        "bor",
        Arc::new(meridian_bus::MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    serve(&bus, "book", Arc::new(Two));
    let asked = ReadChangesRequest {
        partition: "P0".into(),
        after_sequence: 0,
        page_size: 0,
    };
    let (kind, body) = bus
        .call(
            "platform.book.internal.read-changes",
            "meridian.v1.ReadChangesRequest",
            asked.encode_to_vec(),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(kind, "meridian.v1.ReadChangesReply");
    let reply = ReadChangesReply::decode(body.as_slice()).unwrap();
    assert_eq!(reply.changes.len(), 2);
    assert_eq!(reply.head, 2);
    let unknown = ReadChangesRequest {
        partition: "P9".into(),
        ..asked
    };
    let refused = bus
        .call(
            "platform.book.internal.read-changes",
            "meridian.v1.ReadChangesRequest",
            unknown.encode_to_vec(),
            None,
            None,
        )
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("no partition P9"), "{refused}");
}
