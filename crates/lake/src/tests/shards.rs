//! A plugin shard's take and its epoch fence (step 3d, checkpoint 5, item
//! 32): the lake's control writer raises the shard's epoch, records
//! `ShardTookOver`, and answers only once every lake and book writer group
//! has raised it; a group then refuses a write under the old epoch.

use std::time::Duration;

use meridian_changes::groups::Groups;
use meridian_changes::shard_fence::{raise_topic, ShardStamp, WORDS};
use meridian_domain::v1::{ShardTookOver, TakeShardReply, TakeShardRequest, TakeoverReason};

use super::*;
use crate::store::StoreError;

/// The plugin instance: the client's bus publishes as it.
const INSTANCE: &str = "lake-1";

async fn take(bus: &Bus, shard: u32, transitions: u64) -> Result<u64, String> {
    bus.call(
        service::TAKE_SHARD,
        "meridian.v1.TakeShardRequest",
        TakeShardRequest {
            instance: INSTANCE.into(),
            shard,
            lease_transitions: transitions,
            holder: "feed-pod-b_1".into(),
            previous_holder: "feed-pod-a_1".into(),
            reason: TakeoverReason::LeaseLapsed as i32,
        }
        .encode_to_vec(),
        None,
        Some(Duration::from_secs(10)),
    )
    .await
    .map(|(_, reply)| TakeShardReply::decode(&reply[..]).unwrap().epoch)
    .map_err(|failed| failed.to_string())
}

/// The book's writer group `group`, stood in: its fence, raised when asked.
fn book_group(bus: &Bus, group: u32) -> Arc<meridian_changes::shard_fence::Memory> {
    let fence = Arc::new(meridian_changes::shard_fence::Memory::default());
    let raising = Arc::clone(&fence);
    bus.serve(&raise_topic("book", group), move |envelope| {
        let took = ShardTookOver::decode(&envelope.payload[..]).unwrap();
        let epoch = raising.raise_to(&took.instance, took.shard, took.epoch);
        Ok((
            "meridian.v1.TakeShardReply".into(),
            TakeShardReply { epoch }.encode_to_vec(),
        ))
    });
    fence
}

/// Two lake writer groups, each its own store, on one bus.
fn two_groups() -> (Arc<Bus>, Arc<MemoryStore>, Arc<MemoryStore>, Arc<Lake>) {
    let clock = Arc::new(ManualClock::at(NOW));
    let bus = Arc::new(Bus::single(
        INSTANCE,
        Arc::new(MemoryBackend::new()),
        clock.clone(),
    ));
    stand_ins(&bus);
    let (zero, one) = (Arc::new(MemoryStore::new()), Arc::new(MemoryStore::new()));
    // Group 1 first: group 0's handlers of the public topics are the ones
    // left, as the lake's queue group gives a take to group 0's writer.
    service::serve_grouped(
        bus.clone(),
        one.clone(),
        clock.clone(),
        Groups::new(1, 2).unwrap(),
    );
    let lake = service::serve_grouped(bus.clone(), zero.clone(), clock, Groups::new(0, 2).unwrap());
    (bus, zero, one, lake)
}

fn row() -> Observation {
    Observation::Price(Box::new(close(INSTANCE, "r-1", BTC, "2026-10-07", "100")))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_take_is_answered_once_every_group_raised_it_and_a_second_group_refuses_the_old_epoch() {
    let (bus, zero, one, lake) = two_groups();
    lake.with_book_groups(1);
    let book = book_group(&bus, 0);
    let old = take(&bus, 1, 0).await.unwrap();
    let new = take(&bus, 1, 0).await.unwrap();
    assert_eq!((old, new), (1, 2), "the held epoch plus one each take");
    // Every group raised it before the answer: group 1's own store, which
    // group 0 never writes, refuses the old holder's write.
    for store in [&zero, &one] {
        let done = store.record_parts(
            "lake-1:prices",
            vec![vec![row()]],
            &[ShardStamp::of(INSTANCE, 1, old)],
            NOW,
        );
        assert!(
            matches!(&done[0], Err(StoreError::ShardFenced(fenced)) if fenced.held == new),
            "{:?}",
            done[0]
        );
        assert_eq!(done[0].as_ref().unwrap_err().to_string(), WORDS);
        // The new holder's passes.
        let done = store.record_parts(
            "lake-1:prices",
            vec![vec![row()]],
            &[ShardStamp::of(INSTANCE, 1, new)],
            NOW,
        );
        assert!(done[0].is_ok(), "{:?}", done[0]);
    }
    assert_eq!(
        book.epoch(INSTANCE, 1),
        new,
        "the book's group raised it too"
    );
    // A take past the Lease's transitions.
    assert_eq!(take(&bus, 1, 7).await.unwrap(), 7);
    // Each take its own record, in group 0's control partition.
    let recorded: Vec<u64> = zero
        .read_changes(meridian_changes::CONTROL, 0, meridian_changes::PAGE_MOST)
        .unwrap()
        .changes
        .into_iter()
        .filter(|change| change.kind == crate::store::CONTROL_SHARD_TOOK_OVER)
        .map(|change| ShardTookOver::decode(&change.body[..]).unwrap().epoch)
        .collect();
    assert_eq!(recorded, vec![1, 2, 7]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_take_a_writer_group_does_not_confirm_is_refused_and_another_s_shard_is_never_taken() {
    let (bus, zero, _one, lake) = two_groups();
    // Two book groups, one of them not answering.
    lake.with_book_groups(2);
    book_group(&bus, 0);
    let started = std::time::Instant::now();
    let refused = take(&bus, 0, 0).await.unwrap_err();
    assert!(
        refused.contains("was not confirmed") && refused.contains("raise-shard.1"),
        "{refused}"
    );
    assert!(
        started.elapsed()
            >= meridian_changes::shard_fence::CONFIRM_WITHIN
                - meridian_changes::shard_fence::ASK_EACH,
        "asked again until its time ran out"
    );
    // Nothing recorded of a take that was not confirmed (review c5a M4).
    let recorded = zero
        .read_changes(meridian_changes::CONTROL, 0, meridian_changes::PAGE_MOST)
        .unwrap()
        .changes
        .into_iter()
        .filter(|change| change.kind == crate::store::CONTROL_SHARD_TOOK_OVER)
        .count();
    assert_eq!(recorded, 0, "a take not confirmed was recorded as taken");
    // Another instance's shard: refused, nothing raised.
    let other = bus
        .call(
            service::TAKE_SHARD,
            "meridian.v1.TakeShardRequest",
            TakeShardRequest {
                instance: "feed-2".into(),
                shard: 0,
                holder: "x".into(),
                ..Default::default()
            }
            .encode_to_vec(),
            None,
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(other.contains("its own instance's shards"), "{other}");
}

/// Review c5a M3: the lake answers a take -- here refused, a book group
/// confirming nothing, each ask taking all but its whole window -- inside
/// the sidecar's wait, one constant shared: it starts no round it cannot
/// end inside its confirm window.
#[tokio::test(flavor = "multi_thread")]
async fn a_take_is_answered_inside_the_sidecar_s_wait_however_slow_a_group_is() {
    let (bus, _zero, _one, lake) = two_groups();
    lake.with_book_groups(1);
    // A book group that answers each ask just inside it, never raised.
    bus.serve(&raise_topic("book", 0), move |_| {
        std::thread::sleep(meridian_changes::shard_fence::ASK_EACH - Duration::from_millis(10));
        Ok((
            "meridian.v1.TakeShardReply".into(),
            TakeShardReply { epoch: 0 }.encode_to_vec(),
        ))
    });
    let answered = bus
        .call(
            service::TAKE_SHARD,
            "meridian.v1.TakeShardRequest",
            TakeShardRequest {
                instance: INSTANCE.into(),
                shard: 0,
                holder: "feed-pod-b_1".into(),
                ..Default::default()
            }
            .encode_to_vec(),
            None,
            Some(meridian_changes::shard_fence::TAKE_WITHIN),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(
        answered.contains("was not confirmed"),
        "the sidecar gave up before the lake answered: {answered}"
    );
}

/// Review c5a2 L1: a take raised in every group after its sidecar stopped
/// waiting records nothing, so no record names a holder that gave up.
#[tokio::test(flavor = "multi_thread")]
async fn a_take_raised_after_its_sidecar_gave_up_is_not_recorded() {
    let (bus, zero, _one, lake) = two_groups();
    lake.with_book_groups(1);
    book_group(&bus, 0);
    let envelope = Envelope {
        meta: Some(meridian_bus::MessageMeta {
            publisher_instance_id: INSTANCE.into(),
            ..Default::default()
        }),
        ..Default::default()
    };
    let request = TakeShardRequest {
        instance: INSTANCE.into(),
        shard: 0,
        holder: "feed-pod-b_1".into(),
        ..Default::default()
    };
    let late = tokio::task::spawn_blocking(move || {
        lake.take_shard_until(request, &envelope, std::time::Instant::now())
    })
    .await
    .unwrap();
    assert!(late.is_err(), "{late:?}");
    let recorded = zero
        .read_changes(meridian_changes::CONTROL, 0, meridian_changes::PAGE_MOST)
        .unwrap()
        .changes
        .into_iter()
        .filter(|change| change.kind == crate::store::CONTROL_SHARD_TOOK_OVER)
        .count();
    assert_eq!(recorded, 0, "a take its sidecar gave up on was recorded");
}
