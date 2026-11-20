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
            lease_uid: "uid-1".into(),
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

/// v31a, item 3 (the access audit's mismatch 1): a take names its own
/// instance, and its sender is the credential the request's reply inbox is
/// under; a take sent by another instance's credential, naming this one, is
/// refused before anything is numbered or raised.
/// INTERIM(v32): deleted with the reply-inbox check.
#[tokio::test(flavor = "multi_thread")]
async fn a_take_asked_as_another_instance_is_refused() {
    let (_bus, _zero, _one, lake) = two_groups();
    let envelope = meridian_bus::Envelope {
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
        lease_uid: "uid-1".into(),
        ..Default::default()
    };
    let forged = tokio::task::spawn_blocking(move || {
        meridian_bus::inbox::within_asker(Some("feed-2".into()), || {
            lake.take_shard(request, &envelope)
        })
    })
    .await
    .unwrap();
    let refused = forged.expect_err("a take asked as another instance is refused");
    assert!(refused.contains("sent by"), "{refused}");
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
                lease_uid: "uid-1".into(),
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
        lease_uid: "uid-1".into(),
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

/// Review c5a3 L2: a write is stamped with its place in the write-ahead
/// stream only once the lake knows the stream's identity, read with the
/// reader that took it; never in a stream 0, which a place could admit.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_s_place_is_never_stamped_in_stream_0() {
    let (_bus, _zero, _one, lake) = two_groups();
    let stamp = ShardStamp::of(INSTANCE, 0, 1).unwrap();
    let unknown = lake.in_stream(stamp.clone(), 100);
    assert_eq!(
        (unknown.in_stream, unknown.stream),
        (0, 0),
        "a place stamped with no stream's identity"
    );
    lake.with_stream(1_790_000_000_000_000_000);
    let known = lake.in_stream(stamp, 100);
    assert_eq!(
        (known.in_stream, known.stream),
        (100, 1_790_000_000_000_000_000)
    );
}

/// Review c5a3 L1: a take's deadline counts from when its sidecar sent it,
/// not from when the lake received it; a sender's clock ahead never
/// stretches it past the wait counted from receipt.
#[test]
fn a_take_s_deadline_counts_from_its_sending() {
    use meridian_changes::shard_fence::TAKE_WITHIN;
    let now = std::time::Instant::now();
    let wait = |sent_at_ns: i64| service::take_deadline(now, 10_000_000_000, sent_at_ns) - now;
    let from_receipt = wait(0);
    assert!(from_receipt < TAKE_WITHIN, "{from_receipt:?}");
    // Sent 3.4 s before it was received: past the sidecar's wait already.
    assert_eq!(wait(10_000_000_000 - 3_400_000_000), Duration::ZERO);
    // Sent 1 s before: a second less.
    assert_eq!(wait(9_000_000_000), from_receipt - Duration::from_secs(1));
    // A sender's clock ahead of the lake's: as from receipt.
    assert_eq!(wait(12_000_000_000), from_receipt);
}

/// Review c5a3 L1: a take that reached the lake after its sidecar's wait
/// had passed is refused before it is numbered, and recorded nowhere.
#[tokio::test(flavor = "multi_thread")]
async fn a_take_sent_past_its_sidecar_s_wait_is_not_numbered_or_recorded() {
    let (bus, zero, _one, lake) = two_groups();
    lake.with_book_groups(1);
    let book = book_group(&bus, 0);
    let sent_at = NOW - 3_400_000_000;
    let envelope = Envelope {
        meta: Some(meridian_bus::MessageMeta {
            publisher_instance_id: INSTANCE.into(),
            published_at_ns: sent_at,
            ..Default::default()
        }),
        ..Default::default()
    };
    let request = TakeShardRequest {
        instance: INSTANCE.into(),
        shard: 0,
        holder: "feed-pod-b_1".into(),
        lease_uid: "uid-1".into(),
        ..Default::default()
    };
    let late = tokio::task::spawn_blocking(move || lake.take_shard(request, &envelope))
        .await
        .unwrap();
    assert!(late.is_err(), "{late:?}");
    assert_eq!(book.epoch(INSTANCE, 0), 0, "a late take was raised");
    let recorded = zero
        .read_changes(meridian_changes::CONTROL, 0, meridian_changes::PAGE_MOST)
        .unwrap()
        .changes
        .into_iter()
        .filter(|change| change.kind == crate::store::CONTROL_SHARD_TOOK_OVER)
        .count();
    assert_eq!(
        recorded, 0,
        "a take sent past its sidecar's wait was recorded"
    );
}

/// Ruling 184 (option B; item 33): a take of one of an instance's shards
/// delivers each of that instance's standing wants still asked for again
/// at once, as the sweep would -- the same want under the same ID, nothing
/// recorded -- and nothing else: no other instance's, no one-off want, and
/// nothing for a take refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_take_delivers_its_instance_s_standing_wants_again_and_nothing_else() {
    let h = harness();
    let dataset = |instance: &str| DatasetRef {
        dataset: format!("{instance}:daily"),
        instance: instance.into(),
        vendor: "Coinbase".into(),
        declaration: Some(declaration("daily", 60)),
        ..Default::default()
    };
    let entitled = |dataset: &str, reader: &str| DatasetEntitlement {
        dataset: dataset.into(),
        instance: reader.into(),
        allowed: true,
        ..Default::default()
    };
    h.lake.configured(&EntitlementsChangedEvent {
        datasets: vec![dataset(INSTANCE), dataset("kraken-1")],
        licences: vec![],
        entitlements: vec![
            entitled("lake-1:daily", "reporting-1"),
            entitled("kraken-1:daily", "reporting-2"),
        ],
        changed_at_ns: NOW,
    });
    book_group(&h.bus, 0);
    let mut wanted = h.bus.subscribe_every(service::OBSERVATIONS_WANTED);
    let renew = |reader: &str| {
        h.lake
            .renew_wants(
                meridian_domain::v1::RenewWantsRequest {
                    wants: vec![meridian_domain::v1::RenewedWant {
                        data_type: "meridian.v1.Price".into(),
                        subjects: vec![SubjectRef {
                            entity_id: BTC.into(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }],
                },
                &envelope(reader, true, ""),
            )
            .unwrap()
    };
    renew("reporting-1");
    renew("reporting-2");
    // A one-off want of the instance's: a day's prices it has none of.
    read(
        &h,
        "reporting-1",
        &[BTC],
        "2026-10-07",
        SourceChoice::default(),
        0,
    );
    let mut heard = Vec::new();
    while let Ok(Some(delivery)) =
        tokio::time::timeout(Duration::from_millis(200), wanted.recv()).await
    {
        heard.push(ObservationsWantedEvent::decode(&delivery.envelope.payload[..]).unwrap());
    }
    let standing: Vec<&ObservationsWantedEvent> = heard
        .iter()
        .filter(|want| want.standing && want.dataset == "lake-1:daily")
        .collect();
    assert_eq!(standing.len(), 1, "{heard:?}");
    let standing = standing[0].clone();
    assert!(
        heard
            .iter()
            .any(|want| !want.standing && want.dataset == "lake-1:daily"),
        "no one-off want of the instance's: {heard:?}"
    );
    assert!(heard.iter().any(|want| want.dataset == "kraken-1:daily"));
    let recorded = h.store.want_changes().unwrap().len();

    take(&h.bus, 1, 0).await.unwrap();
    let mut again = Vec::new();
    while let Ok(Some(delivery)) =
        tokio::time::timeout(Duration::from_millis(200), wanted.recv()).await
    {
        again.push(ObservationsWantedEvent::decode(&delivery.envelope.payload[..]).unwrap());
    }
    assert_eq!(
        again,
        vec![standing.clone()],
        "the take delivered other than the instance's standing want, as it was"
    );
    assert_eq!(
        h.store.want_changes().unwrap().len(),
        recorded,
        "a delivery again was recorded as a change"
    );

    // A take refused -- its Lease's transitions below one a take was
    // numbered with -- delivers nothing.
    take(&h.bus, 1, 7).await.unwrap();
    while tokio::time::timeout(Duration::from_millis(200), wanted.recv())
        .await
        .is_ok_and(|heard| heard.is_some())
    {}
    assert!(take(&h.bus, 1, 3).await.is_err(), "a take behind was taken");
    assert!(
        tokio::time::timeout(Duration::from_millis(300), wanted.recv())
            .await
            .is_err(),
        "a refused take delivered a want again"
    );
}

/// `lake-1`'s daily dataset served, entitled to `reporting-1`, and a
/// standing want of it for each of `subjects`: the wants it delivered,
/// heard on `wanted`.
async fn standing_wants(
    h: &Harness,
    subjects: &[&str],
    wanted: &mut meridian_bus::Subscription,
) -> Vec<ObservationsWantedEvent> {
    h.lake.configured(&EntitlementsChangedEvent {
        datasets: vec![DatasetRef {
            dataset: format!("{INSTANCE}:daily"),
            instance: INSTANCE.into(),
            vendor: "Coinbase".into(),
            declaration: Some(declaration("daily", 60)),
            ..Default::default()
        }],
        licences: vec![],
        entitlements: vec![DatasetEntitlement {
            dataset: format!("{INSTANCE}:daily"),
            instance: "reporting-1".into(),
            allowed: true,
            ..Default::default()
        }],
        changed_at_ns: NOW,
    });
    h.lake
        .renew_wants(
            meridian_domain::v1::RenewWantsRequest {
                wants: subjects
                    .iter()
                    .map(|subject| meridian_domain::v1::RenewedWant {
                        data_type: "meridian.v1.Price".into(),
                        subjects: vec![SubjectRef {
                            entity_id: (*subject).into(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    })
                    .collect(),
            },
            &envelope("reporting-1", true, ""),
        )
        .unwrap();
    let mut heard = Vec::new();
    while let Ok(Some(delivery)) =
        tokio::time::timeout(Duration::from_millis(200), wanted.recv()).await
    {
        if let Ok(want) = ObservationsWantedEvent::decode(&delivery.envelope.payload[..]) {
            if want.standing {
                heard.push(want);
            }
        }
    }
    heard
}

/// Review c5z F2 (a): the sweep withdraws a want between a take's choosing
/// of the wants it delivers again and their delivery. The want is never
/// heard again after its withdrawal: the plugin would serve it for good.
#[tokio::test(flavor = "multi_thread")]
async fn a_take_s_delivery_again_never_follows_its_want_s_withdrawal() {
    let h = harness();
    book_group(&h.bus, 0);
    let mut lake_events = h.bus.subscribe_every("platform.lake.event.*");
    let standing = standing_wants(&h, &[BTC], &mut lake_events).await;
    assert_eq!(standing.len(), 1, "{standing:?}");
    let id = standing[0].want_id.clone();
    // The sweep, past the want's lapse, as the take delivers it again.
    let (lake, clock) = (Arc::downgrade(&h.lake), h.clock.clone());
    let once = std::sync::atomic::AtomicBool::new(false);
    *h.lake.resending.lock().unwrap() = Some(Box::new(move || {
        if once.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let (lake, clock) = (lake.clone(), clock.clone());
        let (swept, sweeping) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            clock.advance(DAY);
            if let Some(lake) = lake.upgrade() {
                lake.sweep_wants();
            }
            let _ = swept.send(());
        });
        let _ = sweeping.recv_timeout(Duration::from_millis(500));
    }));
    take(&h.bus, 1, 0).await.unwrap();
    let mut order = Vec::new();
    while let Ok(Some(delivery)) =
        tokio::time::timeout(Duration::from_millis(1500), lake_events.recv()).await
    {
        let payload = &delivery.envelope.payload[..];
        match delivery.envelope.payload_type.as_str() {
            "meridian.v1.ObservationsWantedEvent" => order.push((
                "wanted",
                ObservationsWantedEvent::decode(payload).unwrap().want_id,
            )),
            "meridian.v1.WantWithdrawnEvent" => order.push((
                "withdrawn",
                meridian_domain::v1::WantWithdrawnEvent::decode(payload)
                    .unwrap()
                    .want_id,
            )),
            _ => {}
        }
    }
    let withdrawn = order
        .iter()
        .position(|heard| *heard == ("withdrawn", id.clone()))
        .unwrap_or_else(|| panic!("the sweep did not withdraw the want: {order:?}"));
    assert!(
        !order[withdrawn..].contains(&("wanted", id.clone())),
        "the take delivered the want again after its withdrawal: {order:?}"
    );
}

/// Review c5z F3: a take is answered before its instance's standing wants
/// are delivered again, so however many there are, and however slowly
/// they go, they never hold up a take already recorded; and each is still
/// delivered.
#[tokio::test(flavor = "multi_thread")]
async fn a_take_is_answered_before_its_wants_are_delivered_again() {
    const EACH: Duration = Duration::from_millis(400);
    let h = harness();
    book_group(&h.bus, 0);
    let mut wanted = h.bus.subscribe_every(service::OBSERVATIONS_WANTED);
    let standing = standing_wants(&h, &[BTC, USD, USDC, AAPL], &mut wanted).await;
    assert!(standing.len() >= 2, "{standing:?}");
    // Each delivery again slow.
    *h.lake.resending.lock().unwrap() = Some(Box::new(|| std::thread::sleep(EACH)));
    let sent = std::time::Instant::now();
    take(&h.bus, 1, 0).await.unwrap();
    let answered = sent.elapsed();
    assert!(
        answered < EACH,
        "the take was answered in {answered:?}, behind {} wants delivered again",
        standing.len()
    );
    let mut again = Vec::new();
    while let Ok(Some(delivery)) = tokio::time::timeout(EACH * 3, wanted.recv()).await {
        again.push(ObservationsWantedEvent::decode(&delivery.envelope.payload[..]).unwrap());
    }
    let ids = |wants: &[ObservationsWantedEvent]| {
        wants
            .iter()
            .map(|want| want.want_id.clone())
            .collect::<std::collections::BTreeSet<_>>()
    };
    assert_eq!(ids(&again), ids(&standing), "not each delivered again");
}
