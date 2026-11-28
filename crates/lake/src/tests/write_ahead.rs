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
        dataset: String::new(),
        timeout_ms: None,
    }
}

fn reply(answer: &meridian_bus::HandlerReply) -> RecordObservationsReply {
    let (kind, bytes) = answer.as_ref().expect("answered");
    assert_eq!(kind, "meridian.v1.RecordObservationsReply");
    RecordObservationsReply::decode(&bytes[..]).unwrap()
}

/// v31a, item 2 (the access audit's S1, point 10): a write's subject names
/// the instance whose credential alone may publish it, and the lake answers
/// it on that credential's own inboxes alone. A reply-to outside them --
/// another plugin's inbox, a component's, or a client's default -- is
/// refused: the write is recorded and answered to nobody, so a plugin cannot
/// have the lake publish into another's inbox as the runtime.
#[test]
fn a_reply_to_outside_its_senders_inboxes_is_refused() {
    use crate::write_ahead::answered_to;
    use meridian_bus::write_ahead::subject;
    let truefx = subject("truefx", "eurusd");
    assert_eq!(
        answered_to(&truefx, Some("_INBOX.truefx.Ab12.7".into())).as_deref(),
        Some("_INBOX.truefx.Ab12.7")
    );
    for elsewhere in [
        "_INBOX.kraken-1.Ab12.7",
        "_INBOX.runtime.Ab12",
        "_INBOX.dashboard-1.x",
        "_INBOX.Ab12",
        "_INBOX.truefx",
        "platform.config.query.plugin-configuration",
        "_INBOX.truefxx.Ab12",
    ] {
        assert_eq!(
            answered_to(&truefx, Some(elsewhere.into())),
            None,
            "{elsewhere} answered for truefx"
        );
    }
    assert_eq!(answered_to(&truefx, Some(String::new())), None);
    assert_eq!(answered_to(&truefx, None), None);
    // A subject of no dataset's names no sender: nobody is answered.
    assert_eq!(
        answered_to(
            "platform.lake.internal.write-ahead",
            Some("_INBOX.truefx.1".into())
        ),
        None
    );
}

/// The review of v31a-core-2, F4, against a real JetStream (by hand, as the
/// test after this one): a write on coinbase-1's subject that names another
/// instance's inbox, kraken-1's, as its reply-to is recorded and nothing is
/// delivered to that inbox; the write after it, naming coinbase-1's own,
/// is answered there -- so the writer ran past the first, and its silence is
/// a refusal, not a writer that never took it.
///
/// Run by `make test-jetstream`, on its three servers under the tests
/// credential (the second review of v31a-core-2, finding 1); by hand, with
/// JetStream on: `docker run --rm -d --name js -p 4222:4222
/// nats:2.10.29-alpine -js`, then `MERIDIAN_TEST_JETSTREAM_URL=
/// nats://localhost:4222 cargo test -p meridian-lake --lib
/// a_write_naming_another_instances_inbox_is_recorded_and_answered_to_nobody --
/// --ignored`. Connected as the runtime connects, so a credential in the URL
/// is presented.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a JetStream broker: MERIDIAN_TEST_JETSTREAM_URL"]
async fn a_write_naming_another_instances_inbox_is_recorded_and_answered_to_nobody() {
    use crate::write_ahead::{stream_config, write_forever, Bounds};
    use async_nats::jetstream;
    use futures_util::StreamExt as _;
    use meridian_bus::write_ahead::{subject, REPLY_HEADER, STREAM, TOPIC_HEADER};
    use std::time::Duration;
    let url = std::env::var("MERIDIAN_TEST_JETSTREAM_URL").unwrap();
    let client = meridian_bus::NatsBackend::client(&url).await.unwrap();
    let context = jetstream::new(client.clone());
    let _ = context.delete_stream(STREAM).await;
    let bounds = Bounds {
        max_age: Duration::from_secs(600),
        max_bytes: 64 << 20,
        replicas: 1,
    };
    let h = harness();
    tokio::task::block_in_place(|| gap_configured(&h));
    let mut recorded = h
        .bus
        .subscribe_every("platform.lake.coinbase-1:daily.event.prices-recorded");
    let tag = std::process::id();
    let foreign = format!("_INBOX.kraken-1.f4-{tag}");
    let own = format!("_INBOX.coinbase-1.f4-{tag}");
    let mut on_foreign = client.subscribe(format!("{foreign}.>")).await.unwrap();
    let mut on_own = client.subscribe(format!("{own}.>")).await.unwrap();
    client.flush().await.unwrap();
    context.create_stream(stream_config(&bounds)).await.unwrap();
    for (n, reply_to) in [(1u64, format!("{foreign}.1")), (2, format!("{own}.2"))] {
        let price = at_valid(
            close("coinbase-1", &format!("F4:{n}"), BTC, "2026-10-07", "1"),
            NOW - 100_000 + n as i64,
        );
        let mut write = written(n, "coinbase-1", vec![price]);
        write.envelope.meta.as_mut().unwrap().published_at_ns = NOW;
        let mut headers = async_nats::HeaderMap::new();
        headers.insert(TOPIC_HEADER, write.topic.as_str());
        headers.insert(REPLY_HEADER, reply_to.as_str());
        context
            .publish_with_headers(
                subject("coinbase-1", "daily"),
                headers,
                write.envelope.encode_to_vec().into(),
            )
            .await
            .unwrap()
            .await
            .unwrap();
    }
    let writer = {
        let (lake, client) = (h.lake.clone(), client.clone());
        tokio::spawn(async move { write_forever(lake, client, bounds, |_| {}).await })
    };
    let answered = tokio::time::timeout(Duration::from_secs(30), on_own.next())
        .await
        .expect("the write naming its own inbox is answered")
        .unwrap();
    assert_eq!(answered.subject.as_str(), format!("{own}.2"));
    let elsewhere = tokio::time::timeout(Duration::from_secs(3), on_foreign.next()).await;
    writer.abort();
    let _ = writer.await;
    assert!(
        elsewhere.is_err(),
        "the lake answered into kraken-1's inbox: {:?}",
        elsewhere.ok().flatten().map(|message| message.subject)
    );
    let mut keys = std::collections::BTreeSet::new();
    while keys.len() < 2 {
        let event = tokio::time::timeout(Duration::from_secs(10), recorded.recv())
            .await
            .expect("both writes' rows are recorded")
            .unwrap();
        let price = meridian_domain::v1::PricesRecordedEvent::decode(&event.envelope.payload[..])
            .unwrap()
            .price
            .unwrap();
        keys.insert(price.meta.unwrap().row_key);
    }
    assert_eq!(keys, ["F4:1".to_string(), "F4:2".to_string()].into());
    context.delete_stream(STREAM).await.unwrap();
}

/// v31a, item 7.4 (the fault runs of 2026-11-20, finding 2): a write the
/// writer refuses for validation -- a dataset its instance's catalogue does
/// not declare yet -- is refused for good. Its caller is answered the
/// refusal, the lake's place passes it with the batch's commit, and neither
/// the catalogue declaring the dataset after nor the writer starting again
/// has it recorded: the stream still holds it, and the lake never takes it
/// again. Its own process against a real broker, as
/// `a_write_naming_another_instances_inbox_is_recorded_and_answered_to_nobody`
/// is run.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a JetStream broker: MERIDIAN_TEST_JETSTREAM_URL"]
async fn a_write_refused_at_the_writer_is_never_recorded_once_its_dataset_is_declared_or_the_writer_starts_again(
) {
    use crate::write_ahead::{stream_config, write_forever, Bounds};
    use async_nats::jetstream;
    use futures_util::StreamExt as _;
    use meridian_bus::write_ahead::{subject, REFUSAL_HEADER, REPLY_HEADER, STREAM, TOPIC_HEADER};
    use std::time::Duration;
    let url = std::env::var("MERIDIAN_TEST_JETSTREAM_URL").unwrap();
    let client = meridian_bus::NatsBackend::client(&url).await.unwrap();
    let context = jetstream::new(client.clone());
    let _ = context.delete_stream(STREAM).await;
    let bounds = || Bounds {
        max_age: Duration::from_secs(600),
        max_bytes: 64 << 20,
        replicas: 1,
    };
    let h = harness();
    // coinbase-1's catalogue declares no dataset yet: kraken-1's alone.
    let mut undeclared = configuration(true, &[]);
    undeclared
        .datasets
        .retain(|dataset| dataset.dataset != "coinbase-1:daily");
    tokio::task::block_in_place(|| h.lake.configured(&undeclared));
    let mut recorded = h
        .bus
        .subscribe_every("platform.lake.coinbase-1:daily.event.prices-recorded");
    let own = format!("_INBOX.coinbase-1.i74-{}", std::process::id());
    let mut on_own = client.subscribe(format!("{own}.>")).await.unwrap();
    client.flush().await.unwrap();
    context
        .create_stream(stream_config(&bounds()))
        .await
        .unwrap();
    let put = |n: u64| {
        let (context, own) = (context.clone(), own.clone());
        async move {
            let price = at_valid(
                close("coinbase-1", &format!("I74:{n}"), BTC, "2026-10-07", "1"),
                NOW - 100_000 + n as i64,
            );
            let mut write = written(n, "coinbase-1", vec![price]);
            write.envelope.meta.as_mut().unwrap().published_at_ns = NOW;
            let mut headers = async_nats::HeaderMap::new();
            headers.insert(TOPIC_HEADER, write.topic.as_str());
            headers.insert(REPLY_HEADER, format!("{own}.{n}").as_str());
            context
                .publish_with_headers(
                    subject("coinbase-1", "daily"),
                    headers,
                    write.envelope.encode_to_vec().into(),
                )
                .await
                .unwrap()
                .await
                .unwrap()
                .sequence
        }
    };
    let start = || {
        let (lake, client) = (h.lake.clone(), client.clone());
        tokio::spawn(async move { write_forever(lake, client, bounds(), |_| {}).await })
    };
    // Refused: its dataset is none its instance's catalogue declares.
    let refused_at = put(1).await;
    let writer = start();
    let refused = tokio::time::timeout(Duration::from_secs(30), on_own.next())
        .await
        .expect("the refused write is answered")
        .unwrap();
    assert_eq!(refused.subject.as_str(), format!("{own}.1"));
    let why = refused
        .headers
        .as_ref()
        .and_then(|headers| headers.get(REFUSAL_HEADER))
        .map(|said| said.to_string())
        .expect("answered a refusal");
    assert!(why.contains("catalogue declares"), "{why}");
    // The lake's place passed it, committed with nothing recorded.
    let key = crate::write_ahead::stream_key(
        STREAM,
        crate::write_ahead::place(&client).await.unwrap().stream,
    );
    let placed = |h: &Harness| {
        h.store
            .write_ahead_position(&key)
            .unwrap()
            .map(|at| at.sequence)
    };
    assert_eq!(placed(&h), Some(refused_at));
    // The catalogue declares the dataset, and the writer starts again: the
    // stream still holds the refused write.
    writer.abort();
    let _ = writer.await;
    tokio::task::block_in_place(|| gap_configured(&h));
    assert_eq!(
        context
            .get_stream(STREAM)
            .await
            .unwrap()
            .info()
            .await
            .unwrap()
            .state
            .first_sequence,
        refused_at,
        "the stream still holds the refused write"
    );
    let writer = start();
    // A write after it, declared now, is recorded and answered.
    let after_at = put(2).await;
    let answered = tokio::time::timeout(Duration::from_secs(30), on_own.next())
        .await
        .expect("the write after it is answered")
        .unwrap();
    assert_eq!(answered.subject.as_str(), format!("{own}.2"));
    assert!(
        answered
            .headers
            .as_ref()
            .and_then(|headers| headers.get(REFUSAL_HEADER))
            .is_none(),
        "the write after the refused one was refused"
    );
    assert_eq!(placed(&h), Some(after_at));
    // Nothing more is answered or recorded: the refused write is not taken
    // again, so it has no second answer and no row.
    let again = tokio::time::timeout(Duration::from_secs(3), on_own.next()).await;
    writer.abort();
    let _ = writer.await;
    assert!(
        again.is_err(),
        "a write was answered again: {:?}",
        again.ok().flatten().map(|message| message.subject)
    );
    let mut keys = Vec::new();
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(2), recorded.recv()).await
    {
        let price = meridian_domain::v1::PricesRecordedEvent::decode(&event.envelope.payload[..])
            .unwrap()
            .price
            .unwrap();
        keys.push(price.meta.unwrap().row_key);
    }
    assert_eq!(
        keys,
        ["I74:2".to_string()],
        "the refused write's row was recorded after its caller was answered a refusal"
    );
    context.delete_stream(STREAM).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_from_the_stream_are_recorded_with_its_position_and_answered_as_the_lake_answers() {
    let h = harness();
    let mut heard = h
        .bus
        .subscribe_every("platform.lake.coinbase-1:daily.event.prices-recorded");
    let mut writer =
        tokio::task::block_in_place(|| Writer::start(h.lake.clone(), STREAM, 0)).unwrap();
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
    let mut writer =
        tokio::task::block_in_place(|| Writer::start(h.lake.clone(), STREAM, 0)).unwrap();
    // A second writer that read the position before the first moved it: as
    // a writer whose last commits a database failover lost believes it is
    // further on than the store, this one believes it is behind.
    let mut stale =
        tokio::task::block_in_place(|| Writer::start(h.lake.clone(), STREAM, 0)).unwrap();
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
    let mut writer =
        tokio::task::block_in_place(|| Writer::start(h.lake.clone(), STREAM, 0)).unwrap();
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
    let open = gaps(&h, "coinbase-1:daily", GapState::Unspecified);
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
    // Each state its own record, in the control partition too: the
    // unattributed gap first, recorded as the stream was found to skip.
    let every = h.store.dataset_gaps().unwrap();
    assert_eq!(
        every
            .iter()
            .map(|g| (g.dataset.as_str(), g.state))
            .collect::<Vec<_>>(),
        vec![
            ("", GapState::Unrecovered as i32),
            ("coinbase-1:daily", GapState::Refetching as i32),
            ("coinbase-1:daily", GapState::Recovered as i32)
        ]
    );
    assert_eq!(gap.found_in_gap_id, every[0].gap_id);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gap_in_a_dataset_declaring_none_is_recorded_unrecovered_and_read_never_asked() {
    let h = harness();
    tokio::task::block_in_place(|| gap_configured(&h));
    let mut wanted = h.bus.subscribe_every(service::OBSERVATIONS_WANTED);
    let mut writer =
        tokio::task::block_in_place(|| Writer::start(h.lake.clone(), STREAM, 0)).unwrap();
    let price =
        |key: &str, valid: i64| at_valid(close("kraken-1", key, BTC, "2026-10-07", "1"), valid);
    tokio::task::block_in_place(|| {
        writer.take(vec![written(1, "kraken-1", vec![price("a", NOW - 30_000)])])
    })
    .unwrap();
    // A writer started again after the gap: what it found is the store's.
    let mut writer =
        tokio::task::block_in_place(|| Writer::start(h.lake.clone(), STREAM, 0)).unwrap();
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
    // The run's unattributed gap and the dataset's, nothing more.
    assert_eq!(h.store.dataset_gaps().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gap_whose_want_is_declined_is_unrecovered() {
    let h = harness();
    tokio::task::block_in_place(|| gap_configured(&h));
    let mut wanted = h.bus.subscribe_every(service::OBSERVATIONS_WANTED);
    let mut writer =
        tokio::task::block_in_place(|| Writer::start(h.lake.clone(), STREAM, 0)).unwrap();
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
    let shown = gaps(&h, "coinbase-1:daily", GapState::Unrecovered);
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].gap_id, want.gap_id);
}

/// The unattributed gap (contract v30, stage C2; the resilience spec's
/// section 4, "A gap is found when the stream skips", approved 2026-11-11):
/// the writer records a gap the moment it finds the stream's sequence past
/// rows it never committed, its datasets not yet known, unrecovered and so
/// shown at once, before any dataset writes again. Each dataset's next write
/// names its own gap, found from it, and asks its own `backfill`; a writer
/// started again names it too; a dataset that never writes again leaves the
/// unattributed gap shown, never silent.
#[tokio::test(flavor = "multi_thread")]
async fn a_gap_is_recorded_the_moment_the_stream_skips_and_each_dataset_names_it_as_it_writes_next()
{
    let h = harness();
    tokio::task::block_in_place(|| gap_configured(&h));
    let mut wanted = h.bus.subscribe_every(service::OBSERVATIONS_WANTED);
    let mut writer =
        tokio::task::block_in_place(|| Writer::start(h.lake.clone(), STREAM, 0)).unwrap();
    let coinbase =
        |key: &str, valid: i64| at_valid(close("coinbase-1", key, BTC, "2026-10-07", "1"), valid);
    let kraken =
        |key: &str, valid: i64| at_valid(close("kraken-1", key, BTC, "2026-10-07", "1"), valid);
    tokio::task::block_in_place(|| {
        writer.take(vec![
            written(1, "coinbase-1", vec![coinbase("a", NOW - 30_000)]),
            written(2, "kraken-1", vec![kraken("a", NOW - 30_000)]),
        ])
    })
    .unwrap();
    // 3 to 6 dropped. The first write taken after them is neither dataset's
    // next after the gap -- a write of another instance, refused -- so no
    // dataset is named yet; the gap is recorded all the same, at once.
    let stray = written(
        7,
        "kraken-1",
        vec![close("coinbase-1", "z", BTC, "2026-10-07", "1")],
    );
    tokio::task::block_in_place(|| writer.take(vec![stray])).unwrap();
    let found = gaps(&h, "", GapState::Unspecified);
    assert_eq!(found.len(), 1, "{found:?}");
    let unattributed = found[0].clone();
    assert_eq!(unattributed.dataset, "", "its datasets not yet known");
    assert!(unattributed.found_in_gap_id.is_empty());
    assert_eq!(
        unattributed.state,
        GapState::Unrecovered as i32,
        "shown at once"
    );
    assert_eq!(unattributed.cause, GapCause::WriteAheadBound as i32);
    assert_eq!(
        (unattributed.valid_from_ns, unattributed.valid_until_ns),
        (NOW + 2, NOW + 7),
        "the stream's times either side of the run it skipped"
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), wanted.recv())
            .await
            .is_err(),
        "nothing asked of no dataset"
    );
    // coinbase-1 writes next, its sidecar's last write one of those dropped:
    // its own gap, naming the unattributed one, and its backfill asked.
    let mut next = written(8, "coinbase-1", vec![coinbase("e", NOW - 10_000)]);
    next.after = Some(4);
    tokio::task::block_in_place(|| writer.take(vec![next])).unwrap();
    let named = gaps(&h, "coinbase-1:daily", GapState::Refetching);
    assert_eq!(named.len(), 1, "{named:?}");
    assert_eq!(named[0].found_in_gap_id, unattributed.gap_id);
    assert_eq!(
        (named[0].valid_from_ns, named[0].valid_until_ns),
        (NOW - 30_000, NOW - 10_000)
    );
    let want = ObservationsWantedEvent::decode(&wanted.recv().await.unwrap().envelope.payload[..])
        .unwrap();
    assert_eq!(
        (want.gap_id.as_str(), want.dataset.as_str()),
        (named[0].gap_id.as_str(), "coinbase-1:daily")
    );
    // A writer started again -- a takeover, a restart -- names it too: the
    // run keeps its gap's ID in the store.
    let mut writer =
        tokio::task::block_in_place(|| Writer::start(h.lake.clone(), STREAM, 0)).unwrap();
    let mut later = written(9, "kraken-1", vec![kraken("i", NOW - 1_000)]);
    later.after = Some(5);
    tokio::task::block_in_place(|| writer.take(vec![later])).unwrap();
    let kraken_gap = gaps(&h, "kraken-1:daily", GapState::Unrecovered);
    assert_eq!(kraken_gap.len(), 1);
    assert_eq!(kraken_gap[0].found_in_gap_id, unattributed.gap_id);
    // The unattributed gap stays shown: a dataset that never writes again
    // is still in it.
    let still = gaps(&h, "", GapState::Unrecovered)
        .into_iter()
        .filter(|g| g.dataset.is_empty())
        .collect::<Vec<_>>();
    assert_eq!(still, vec![unattributed]);
}

/// Step 3d (section 6.1): one reader of the stream per writer group, each
/// with its own position. Every group reads the whole stream; each commits
/// its own datasets' writes and answers them, and moves its position past
/// the rest, which their owners answer.
#[tokio::test(flavor = "multi_thread")]
async fn each_writer_group_takes_its_own_datasets_writes_from_the_stream_with_its_own_position() {
    use meridian_changes::groups::{owner, Groups};
    let h = harness();
    let dataset = "coinbase-1:daily";
    let owning = owner(dataset, 2);
    let lake_of = |group: u32| {
        let lake = service::Lake::grouped(
            h.bus.clone(),
            h.store.clone(),
            h.clock.clone(),
            Groups::new(group, 2).unwrap(),
        );
        lake.configured(&configuration(true, &[]));
        lake
    };
    let write = |sequence: u64| {
        let mut taken = written(
            sequence,
            "coinbase-1",
            vec![close(
                "coinbase-1",
                &format!("BTC:{sequence}"),
                BTC,
                "2026-10-07",
                "62000",
            )],
        );
        taken.dataset = dataset.to_string();
        taken
    };
    let batch = vec![write(1), write(2)];
    let other = lake_of(1 - owning);
    let mut skipping = tokio::task::block_in_place(|| Writer::start(other, STREAM, 0)).unwrap();
    let skipped = tokio::task::block_in_place(|| skipping.take(batch.clone())).unwrap();
    assert!(
        skipped.is_empty(),
        "another group's writes are its owner's to answer"
    );
    let key = |group: u32| crate::write_ahead::position_key(STREAM, Groups::new(group, 2).unwrap());
    assert_eq!(
        h.store
            .write_ahead_position(&key(1 - owning))
            .unwrap()
            .map(|at| at.sequence),
        Some(2),
        "its own position moved past them"
    );
    assert_eq!(
        h.store.heads().unwrap().get(dataset),
        None,
        "nothing of them kept"
    );
    let mut taking =
        tokio::task::block_in_place(|| Writer::start(lake_of(owning), STREAM, 0)).unwrap();
    let answers = tokio::task::block_in_place(|| taking.take(batch)).unwrap();
    assert_eq!(answers.len(), 2);
    assert_eq!(reply(&answers[1].1).recorded, 1);
    assert_eq!(h.store.heads().unwrap()[dataset], 2);
    assert_eq!(
        h.store
            .write_ahead_position(&key(owning))
            .unwrap()
            .map(|at| at.sequence),
        Some(2)
    );
    assert_eq!(key(0), STREAM, "group 0 keeps the position every lake kept");
}

/// Step 3d (section 6.1; CI 43615034712): a writer group past the first,
/// started for the first time after `writerGroups` was raised, has no
/// position of its own. The stream's earlier writes were group 0's to take,
/// and were taken, so the group begins at the first write the stream still
/// holds and counts none before it dropped: no run, no gap, nothing logged
/// in the control partition, which group 0 writes. Its own dataset's write
/// is committed and answered. Group 0 with no position still finds the run
/// it never took.
#[tokio::test(flavor = "multi_thread")]
async fn a_writer_group_started_after_the_count_was_raised_takes_from_the_streams_first_with_no_gap(
) {
    use meridian_changes::groups::{owner, Groups};
    let h = harness();
    let dataset = "coinbase-1:daily";
    // A count of groups at which the dataset is a group's past the first.
    let count = (2..16)
        .find(|count| owner(dataset, *count) > 0)
        .expect("a count where a group past the first owns it");
    let owning = owner(dataset, count);
    let lake_of = |group: u32| {
        let lake = service::Lake::grouped(
            h.bus.clone(),
            h.store.clone(),
            h.clock.clone(),
            Groups::new(group, count).unwrap(),
        );
        lake.configured(&configuration(true, &[]));
        lake
    };
    let key =
        |group: u32| crate::write_ahead::position_key(STREAM, Groups::new(group, count).unwrap());
    let write = |sequence: u64| {
        let mut taken = written(
            sequence,
            "coinbase-1",
            vec![close("coinbase-1", "first", BTC, "2026-10-07", "62000")],
        );
        taken.dataset = dataset.to_string();
        taken
    };
    // The stream holds 1,810 on: what came before aged out after group 0
    // took it.
    let mut fresh =
        tokio::task::block_in_place(|| Writer::start(lake_of(owning), STREAM, 0)).unwrap();
    assert_eq!(fresh.position(), None);
    let answers = tokio::task::block_in_place(|| fresh.take(vec![write(1_810)])).unwrap();
    assert_eq!(answers.len(), 1);
    assert_eq!(reply(&answers[0].1).recorded, 1, "its own write committed");
    assert_eq!(
        h.store
            .write_ahead_position(&key(owning))
            .unwrap()
            .map(|at| at.sequence),
        Some(1_810)
    );
    let (dropped, _) = h.store.write_ahead_found(&key(owning)).unwrap();
    assert!(dropped.is_empty(), "no run counted dropped: {dropped:?}");
    assert!(
        gaps(&h, "", GapState::Unspecified).is_empty(),
        "no gap recorded"
    );
    // Committed since: the stream aging past its position later is a gap
    // it marks, as group 0 would ("lake loss is re-sent, never silent").
    let mut again =
        tokio::task::block_in_place(|| Writer::start(lake_of(owning), STREAM, 0)).unwrap();
    tokio::task::block_in_place(|| again.take(vec![write(2_500)])).unwrap();
    let (dropped, _) = h.store.write_ahead_found(&key(owning)).unwrap();
    assert_eq!(
        dropped
            .iter()
            .map(|run| (run.first, run.last))
            .collect::<Vec<_>>(),
        vec![(1_811, 2_499)],
        "a group that has committed marks the run past its position"
    );
    assert_eq!(
        gaps(&h, "", GapState::Unspecified).len(),
        1,
        "its gap recorded"
    );
    // Group 0, with no position, never took 1 to 1,809: those it finds.
    let mut zero = tokio::task::block_in_place(|| Writer::start(lake_of(0), STREAM, 0)).unwrap();
    tokio::task::block_in_place(|| zero.take(vec![write(1_811)])).unwrap();
    let (dropped, _) = h.store.write_ahead_found(&key(0)).unwrap();
    assert_eq!(
        dropped
            .iter()
            .map(|run| (run.first, run.last))
            .collect::<Vec<_>>(),
        vec![(1, 1_810)]
    );
}

/// Item 191 (ruled 2026-11-19): leaving `ha` deletes JetStream's volumes,
/// and taking `ha` again makes the stream again under the same name,
/// numbered from its own first. Everything the lake keeps of a stream is
/// kept by its identity: in the new one the writer has no position, takes
/// from its first write and counts nothing before it lost; the old one's
/// position above the new one's sequences skips none of them, and a run
/// the old one dropped never marks a write of the new one as following a
/// gap. The old stream's keeping is left as it was.
#[tokio::test(flavor = "multi_thread")]
async fn a_stream_made_again_under_its_name_is_taken_from_its_first_write_with_nothing_of_the_old()
{
    const OLD: i64 = 1_790_000_000_000_000_000;
    const NEW: i64 = OLD + 600_000_000_000;
    let h = harness();
    tokio::task::block_in_place(|| gap_configured(&h));
    let coinbase =
        |key: &str, valid: i64| at_valid(close("coinbase-1", key, BTC, "2026-10-07", "1"), valid);
    let key = |identity: i64| crate::write_ahead::stream_key(STREAM, identity);
    // The old stream: 1 and 2 taken, 3 to 9 dropped, 10 taken -- a run and
    // its gap kept, and coinbase-1's latest valid time.
    let mut old =
        tokio::task::block_in_place(|| Writer::start(h.lake.clone(), STREAM, OLD)).unwrap();
    tokio::task::block_in_place(|| {
        old.take(vec![
            written(1, "coinbase-1", vec![coinbase("a", NOW - 50_000)]),
            written(2, "coinbase-1", vec![coinbase("b", NOW - 40_000)]),
        ])
    })
    .unwrap();
    tokio::task::block_in_place(|| {
        old.take(vec![written(
            10,
            "coinbase-1",
            vec![coinbase("c", NOW - 30_000)],
        )])
    })
    .unwrap();
    let held = h.store.write_ahead_position(&key(OLD)).unwrap();
    assert_eq!(held.map(|at| at.sequence), Some(10));
    let (dropped, _) = h.store.write_ahead_found(&key(OLD)).unwrap();
    assert_eq!(
        dropped
            .iter()
            .map(|run| (run.first, run.last))
            .collect::<Vec<_>>(),
        vec![(3, 9)]
    );
    let before = gaps(&h, "", GapState::Unspecified);
    assert_eq!(before.len(), 1, "the old stream's run, its gap: {before:?}");
    // The stream made again: 1 to 12, write 5 its sidecar's next after 4 --
    // a sequence in the old stream's dropped run.
    let mut new =
        tokio::task::block_in_place(|| Writer::start(h.lake.clone(), STREAM, NEW)).unwrap();
    assert_eq!(new.position(), None, "no place in the new stream");
    assert_eq!(new.next(), 1, "taken from its first");
    let writes: Vec<Taken> = (1..=12)
        .map(|n| {
            let mut write = written(
                n,
                "coinbase-1",
                vec![coinbase(&format!("new-{n}"), NOW - 20_000 + n as i64)],
            );
            if n == 5 {
                write.after = Some(4);
            }
            write
        })
        .collect();
    let answers = tokio::task::block_in_place(|| new.take(writes)).unwrap();
    assert_eq!(answers.len(), 12, "every write of the new stream taken");
    for answered in &answers {
        assert_eq!(reply(&answered.1).recorded, 1, "{:?}", answered.0);
    }
    assert_eq!(
        h.store
            .write_ahead_position(&key(NEW))
            .unwrap()
            .map(|at| at.sequence),
        Some(12)
    );
    let (dropped, _) = h.store.write_ahead_found(&key(NEW)).unwrap();
    assert!(dropped.is_empty(), "nothing counted lost: {dropped:?}");
    assert_eq!(
        gaps(&h, "", GapState::Unspecified),
        before,
        "no gap of the new stream's, and no write of it after the old one's run"
    );
    // The old stream's keeping is as it was.
    assert_eq!(h.store.write_ahead_position(&key(OLD)).unwrap(), held);
    let restarted =
        tokio::task::block_in_place(|| Writer::start(h.lake.clone(), STREAM, NEW)).unwrap();
    assert_eq!(
        restarted.next(),
        13,
        "a writer started again goes on in the new stream"
    );
}

/// The review of item 191's finding 1: a lake upgraded from one that kept
/// its write-ahead places by the stream's name alone carries them -- the
/// position, the runs it found dropped and each dataset's latest valid time
/// -- to the stream's identity at its writer's start, and goes on from its
/// position. The stream is the one it was: a write it took before is never
/// taken again, so a row restated by a later write is never restated back
/// (an older write taken again records a new version of the old value, and
/// the newer then another).
#[tokio::test(flavor = "multi_thread")]
async fn an_upgrade_from_places_kept_by_the_streams_name_goes_on_from_its_position() {
    // The stream made before its first write.
    const IDENTITY: i64 = NOW - 1_000;
    let h = harness();
    tokio::task::block_in_place(|| gap_configured(&h));
    let valid = NOW - 50_000;
    let write = |n: u64| {
        let price = match n {
            3 => close("coinbase-1", "K", BTC, "2026-10-07", "1"),
            7 => close("coinbase-1", "K", BTC, "2026-10-07", "2"),
            _ => close("coinbase-1", &format!("other-{n}"), BTC, "2026-10-07", "1"),
        };
        written(n, "coinbase-1", vec![at_valid(price, valid)])
    };
    let versions_of_k = || {
        h.store
            .read_dataset_versions("coinbase-1:daily", DataType::Price)
            .unwrap()
            .into_iter()
            .filter(|row| row.meta().row_key == "K")
            .count()
    };
    // The lake before item 191: its places under the stream's name. Write
    // 3 sets K; write 7 restates it; 5 was dropped before it was taken.
    let mut before =
        tokio::task::block_in_place(|| Writer::start(h.lake.clone(), STREAM, 0)).unwrap();
    tokio::task::block_in_place(|| before.take((1..=4).map(write).collect())).unwrap();
    tokio::task::block_in_place(|| before.take((6..=10).map(write).collect())).unwrap();
    assert_eq!(
        versions_of_k(),
        2,
        "K set by write 3 and restated by write 7"
    );
    let (dropped, valid_times) = h.store.write_ahead_found(STREAM).unwrap();
    assert_eq!(
        dropped
            .iter()
            .map(|run| (run.first, run.last))
            .collect::<Vec<_>>(),
        vec![(5, 5)]
    );
    // Upgraded: the writer starts under the stream's identity, and goes on
    // from its position there.
    let mut after =
        tokio::task::block_in_place(|| Writer::start(h.lake.clone(), STREAM, IDENTITY)).unwrap();
    assert_eq!(after.next(), 11, "goes on from its position");
    // The stream sends what the writer asks for, from its next.
    let rest: Vec<Taken> = (after.next()..=12).map(write).collect();
    tokio::task::block_in_place(|| after.take(rest)).unwrap();
    assert_eq!(
        versions_of_k(),
        2,
        "no write taken again: K never restated back"
    );
    let key = crate::write_ahead::stream_key(STREAM, IDENTITY);
    assert_eq!(
        h.store
            .write_ahead_position(&key)
            .unwrap()
            .map(|at| at.sequence),
        Some(12)
    );
    let (carried, carried_times) = h.store.write_ahead_found(&key).unwrap();
    assert_eq!(
        carried
            .iter()
            .map(|run| (run.first, run.last))
            .collect::<Vec<_>>(),
        vec![(5, 5)],
        "the run found dropped carried with the position"
    );
    assert_eq!(
        carried_times, valid_times,
        "each dataset's latest valid time carried"
    );
}

/// Finding 1's carry, never into a stream made after the places were
/// taken (item 191): places kept by the stream's name from before `ha` was
/// left -- a lake that switched before the fix -- belong to a stream older
/// than the one made again, whose writes are all later than its making.
/// The writer starts in the new stream with no place, from its first write.
#[tokio::test(flavor = "multi_thread")]
async fn places_kept_by_the_streams_name_are_never_carried_into_a_stream_made_after_them() {
    let h = harness();
    let mut before =
        tokio::task::block_in_place(|| Writer::start(h.lake.clone(), STREAM, 0)).unwrap();
    let price = || close("coinbase-1", "a", BTC, "2026-10-07", "1");
    tokio::task::block_in_place(|| before.take(vec![written(10, "coinbase-1", vec![price()])]))
        .unwrap();
    // Made again after write 10 was taken.
    let made_again = NOW + 1_000;
    let after =
        tokio::task::block_in_place(|| Writer::start(h.lake.clone(), STREAM, made_again)).unwrap();
    assert_eq!(after.position(), None, "no place in the stream made again");
    assert_eq!(after.next(), 1, "taken from its first");
    assert_eq!(
        h.store
            .write_ahead_position(&crate::write_ahead::stream_key(STREAM, made_again))
            .unwrap(),
        None
    );
}

/// The review of item 191's finding 5 (the coordinator's reading, item
/// 203): group 0, in a stream it has no place in, counts the stream's own
/// sequences from 1. A run that aged out of the stream before its first
/// take -- a new stream whose writer was down longer than its bound -- is
/// a gap (spec section 4, GAP_CAUSE_WRITE_AHEAD_BOUND). The old stream's
/// writes are never in the new one's numbering: nothing is counted lost of
/// them.
#[tokio::test(flavor = "multi_thread")]
async fn group_zero_with_no_place_in_a_stream_counts_a_run_aged_out_before_its_first_take() {
    const IDENTITY: i64 = NOW - 1_000;
    let h = harness();
    tokio::task::block_in_place(|| gap_configured(&h));
    let mut writer =
        tokio::task::block_in_place(|| Writer::start(h.lake.clone(), STREAM, IDENTITY)).unwrap();
    assert_eq!(writer.position(), None);
    let first = written(
        5,
        "coinbase-1",
        vec![close("coinbase-1", "a", BTC, "2026-10-07", "1")],
    );
    tokio::task::block_in_place(|| writer.take(vec![first])).unwrap();
    let (dropped, _) = h
        .store
        .write_ahead_found(&crate::write_ahead::stream_key(STREAM, IDENTITY))
        .unwrap();
    assert_eq!(
        dropped
            .iter()
            .map(|run| (run.first, run.last))
            .collect::<Vec<_>>(),
        vec![(1, 4)],
        "1 to 4 aged out before group 0's first take: a gap"
    );
}

/// Item 191 against a real JetStream (by hand: a broker with JetStream at
/// MERIDIAN_TEST_JETSTREAM_URL, which this test empties of the lake's
/// stream): the lake's writer takes ten writes from the stream, three of
/// them dropped before it took them; the stream is deleted -- `ha` left --
/// and made again under its name -- `ha` taken again -- holding twelve
/// writes, more than the old position, one of them its sidecar's next after
/// a sequence in the old stream's dropped run. Every write of the new stream
/// is taken and answered, and none is found to follow a gap. Then the
/// stream is deleted and made again once more under the writer while it
/// runs (the review of item 191's finding 10): the running writer finds the
/// stream's identity another, starts again in it from its first write, and
/// takes and answers every write of it, none found to follow a gap.
///
/// Run by hand, with JetStream on: `docker run --rm -d --name js -p
/// 4222:4222 nats:2.10.29-alpine -js`, then `MERIDIAN_TEST_JETSTREAM_URL=
/// nats://localhost:4222 cargo test -p meridian-lake --lib
/// a_stream_made_again_under_its_name_is_taken_whole_from_a_real_broker --
/// --ignored`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a JetStream broker: MERIDIAN_TEST_JETSTREAM_URL"]
async fn a_stream_made_again_under_its_name_is_taken_whole_from_a_real_broker() {
    use crate::write_ahead::{stream_config, write_forever, Bounds};
    use async_nats::jetstream;
    use futures_util::StreamExt as _;
    use meridian_bus::write_ahead::{subject, AFTER_HEADER, REPLY_HEADER, STREAM, TOPIC_HEADER};
    use std::time::Duration;
    let url = std::env::var("MERIDIAN_TEST_JETSTREAM_URL").unwrap();
    let client = async_nats::connect(&url).await.unwrap();
    let context = jetstream::new(client.clone());
    let _ = context.delete_stream(STREAM).await;
    let bounds = Bounds {
        max_age: Duration::from_secs(600),
        max_bytes: 64 << 20,
        replicas: 1,
    };
    let h = harness();
    tokio::task::block_in_place(|| gap_configured(&h));
    let tag = std::process::id();
    let mut answered = client
        .subscribe(format!("_INBOX.coinbase-1.c5o-{tag}.>"))
        .await
        .unwrap();
    let put = |round: &'static str, n: u64, after: Option<u64>| {
        let context = context.clone();
        async move {
            let price = at_valid(
                close(
                    "coinbase-1",
                    &format!("{round}-{n}"),
                    BTC,
                    "2026-10-07",
                    "1",
                ),
                NOW - 100_000 + n as i64,
            );
            let mut write = written(n, "coinbase-1", vec![price]);
            let mut headers = async_nats::HeaderMap::new();
            headers.insert(TOPIC_HEADER, write.topic.as_str());
            headers.insert(
                REPLY_HEADER,
                format!("_INBOX.coinbase-1.c5o-{tag}.{round}.{n}").as_str(),
            );
            if let Some(after) = after {
                headers.insert(AFTER_HEADER, after.to_string().as_str());
            }
            write.envelope.meta.as_mut().unwrap().published_at_ns = NOW;
            context
                .publish_with_headers(
                    subject("coinbase-1", "daily"),
                    headers,
                    write.envelope.encode_to_vec().into(),
                )
                .await
                .unwrap()
                .await
                .unwrap();
        }
    };
    let run = || {
        let (lake, client) = (h.lake.clone(), client.clone());
        tokio::spawn(async move { write_forever(lake, client, bounds, |_| {}).await })
    };
    // Answers heard on `round`'s inbox within `within`, by write.
    async fn heard(
        answered: &mut async_nats::Subscriber,
        round: &str,
        want: usize,
        within: Duration,
    ) -> std::collections::BTreeSet<u64> {
        let mut seen = std::collections::BTreeSet::new();
        let until = tokio::time::Instant::now() + within;
        while seen.len() < want {
            match tokio::time::timeout_at(until, answered.next()).await {
                Ok(Some(message)) => {
                    let mut words = message.subject.as_str().rsplit('.');
                    let n: u64 = words.next().unwrap().parse().unwrap();
                    if words.next() == Some(round) && message.headers.is_none() {
                        seen.insert(n);
                    }
                }
                _ => break,
            }
        }
        seen
    }
    // The old stream: 1 to 5 taken.
    context.create_stream(stream_config(&bounds)).await.unwrap();
    let writer = run();
    for n in 1..=5 {
        put("old", n, None).await;
    }
    let first = heard(&mut answered, "old", 5, Duration::from_secs(30)).await;
    assert_eq!(first.len(), 5, "the old stream's first five: {first:?}");
    writer.abort();
    let _ = writer.await;
    // 6 to 10 written while no writer runs, 6 to 8 dropped before it starts.
    for n in 6..=10 {
        put("old", n, None).await;
    }
    context
        .get_stream(STREAM)
        .await
        .unwrap()
        .purge()
        .sequence(9)
        .await
        .unwrap();
    let writer = run();
    let rest = heard(&mut answered, "old", 2, Duration::from_secs(30)).await;
    assert_eq!(rest, [9, 10].into(), "the old stream's last two");
    writer.abort();
    let _ = writer.await;
    let old_gaps = gaps(&h, "", GapState::Unspecified);
    assert_eq!(old_gaps.len(), 1, "the old stream's run: {old_gaps:?}");
    // `ha` left and taken again: the stream deleted, made again, twelve
    // writes in it before the writer starts; write 11 its sidecar's next
    // after 7.
    context.delete_stream(STREAM).await.unwrap();
    context.create_stream(stream_config(&bounds)).await.unwrap();
    for n in 1..=12 {
        put("new", n, (n == 11).then_some(7)).await;
    }
    let writer = run();
    let new = heard(&mut answered, "new", 12, Duration::from_secs(30)).await;
    assert_eq!(
        new,
        (1..=12).collect(),
        "every write of the stream made again taken and answered"
    );
    assert_eq!(
        gaps(&h, "", GapState::Unspecified),
        old_gaps,
        "no write of the new stream found to follow the old one's run"
    );
    // `ha` left and taken again under the running writer: the stream
    // deleted and made again once more, four writes in it; write 3 its
    // sidecar's next after 7, a sequence in the first stream's dropped run.
    context.delete_stream(STREAM).await.unwrap();
    context.create_stream(stream_config(&bounds)).await.unwrap();
    for n in 1..=4 {
        put("again", n, (n == 3).then_some(7)).await;
    }
    let again = heard(&mut answered, "again", 4, Duration::from_secs(30)).await;
    writer.abort();
    let _ = writer.await;
    assert_eq!(
        again,
        (1..=4).collect(),
        "every write of the stream made again under the running writer taken and answered"
    );
    // No write of it follows a gap; the stream the writer held a place in,
    // gone under it, is marked once, for the dataset the lake keeps as
    // written through it, found from no run (the consensus of 2026-11-28).
    let now_gaps = gaps(&h, "", GapState::Unspecified);
    let lost: Vec<&DatasetGap> = now_gaps
        .iter()
        .filter(|gap| !old_gaps.contains(gap))
        .collect();
    assert_eq!(lost.len(), 1, "the lost stream's gap alone: {now_gaps:?}");
    assert_eq!(lost[0].dataset, "coinbase-1:daily");
    assert!(lost[0].found_in_gap_id.is_empty());
    assert_eq!(lost[0].state, GapState::Unrecovered as i32);
    context.delete_stream(STREAM).await.unwrap();
}

/// The consensus of 2026-11-28 (run 46090125342), option 2, and the review
/// of it (findings 2, 7 and 9): a stream gone under a writer that held a
/// place in it is marked with the existing gap record -- a gap for each
/// dataset the lake keeps as written through it, unrecovered so it is shown
/// at once and stays shown, and asked of nobody though the dataset declares
/// `backfill` (no subject is known to ask for); from the earlier of the
/// stream's time of the last write recorded and the dataset's latest valid
/// time, to now. The writer's place stays where it was. A stream it had
/// recorded nothing from is marked by the one gap that names no dataset,
/// from when the stream was made, its identity, and no place is committed
/// in it.
#[tokio::test(flavor = "multi_thread")]
async fn a_stream_lost_under_its_writer_is_marked_for_each_dataset_it_kept_by_time() {
    let h = harness();
    tokio::task::block_in_place(|| gap_configured(&h));
    let mut wanted = h.bus.subscribe_every(service::OBSERVATIONS_WANTED);
    let made_at = NOW - 5_000;
    let mut writer = Writer::start(h.lake.clone(), STREAM, made_at).unwrap();
    let taken = (1..=3)
        .map(|n| {
            let price = close("coinbase-1", &format!("L:{n}"), BTC, "2026-10-07", "1");
            written(
                n,
                "coinbase-1",
                vec![at_valid(price, NOW - 100_000 + n as i64)],
            )
        })
        .collect();
    tokio::task::block_in_place(|| writer.take(taken)).unwrap();
    assert!(gaps(&h, "", GapState::Unspecified).is_empty());
    h.clock.set(NOW + 9_000);
    let lost = tokio::task::block_in_place(|| writer.lost(None)).unwrap();
    let marked = gaps(&h, "", GapState::Unspecified);
    assert_eq!(marked, lost, "what was recorded is what was marked");
    assert_eq!(marked.len(), 1, "{marked:?}");
    let gap = &marked[0];
    assert_eq!(gap.dataset, "coinbase-1:daily");
    assert!(gap.subjects.is_empty() && gap.found_in_gap_id.is_empty());
    assert_eq!(gap.cause, GapCause::WriteAheadBound as i32);
    assert_eq!(gap.state, GapState::Unrecovered as i32);
    // From the dataset's latest valid time recorded, earlier here than the
    // stream's time of write 3, the last recorded, to now.
    assert_eq!(
        (gap.valid_from_ns, gap.valid_until_ns),
        (NOW - 100_000 + 3, NOW + 9_000)
    );
    assert_eq!(gaps(&h, "coinbase-1:daily", GapState::Unrecovered), marked);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), wanted.recv())
            .await
            .is_err(),
        "nothing is asked again: no subject is known to ask for"
    );
    // Its place is where it was, kept: a writer started in the same stream
    // again holds it.
    assert_eq!(writer.next(), 4);
    let again = Writer::start(h.lake.clone(), STREAM, made_at).unwrap();
    assert_eq!(again.next(), 4);
    // A stream the lake recorded nothing from: marked from when it was
    // made, naming no dataset, and no place committed in it (finding 7) --
    // found still there, its own sequences are counted from 1.
    let other = NOW - 2_000;
    let mut unplaced = Writer::start(h.lake.clone(), STREAM, other).unwrap();
    let lost = tokio::task::block_in_place(|| unplaced.lost(None)).unwrap();
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0].dataset, "");
    assert_eq!(
        (lost[0].valid_from_ns, lost[0].valid_until_ns),
        (other, NOW + 9_000)
    );
    assert_eq!(unplaced.position(), None);
    let unplaced = Writer::start(h.lake.clone(), STREAM, other).unwrap();
    assert_eq!(unplaced.position(), None, "no place was committed");
    assert_eq!(gaps(&h, "", GapState::Unspecified).len(), 2);
}

/// The silent loss of 2026-11-28 (`make test-jetstream` at 7f80a4d3: a
/// write the deleted stream held was neither recorded nor inside the gap,
/// whose window was empty and ended before it): the window's end was the
/// lake's clock alone, and a lake's clock behind the stream's time of the
/// last write recorded moved the whole window back before that write. The
/// window never ends before it starts, and where the stream was made again
/// never before the new one's making, by the broker's clock, which every
/// write of the old one precedes.
#[tokio::test(flavor = "multi_thread")]
async fn a_lost_streams_gap_never_ends_before_its_start_or_before_the_stream_made_again() {
    let h = harness();
    tokio::task::block_in_place(|| gap_configured(&h));
    let made_at = NOW - 5_000;
    // A place, and no dataset kept under it: the stream's own window.
    let place = |writer: &mut Writer| {
        let mut write = written(1, "coinbase-1", Vec::new());
        write.envelope.payload_type = "meridian.v1.Nothing".into();
        tokio::task::block_in_place(|| writer.take(vec![write])).unwrap();
        assert_eq!(writer.next(), 2);
    };
    // The lake's clock behind the stream's: write 1 was taken at NOW + 1.
    h.clock.set(NOW - 50_000);
    let mut writer = Writer::start(h.lake.clone(), STREAM, made_at).unwrap();
    place(&mut writer);
    let lost = tokio::task::block_in_place(|| writer.lost(None)).unwrap();
    assert_eq!(
        (lost[0].valid_from_ns, lost[0].valid_until_ns),
        (NOW + 1, NOW + 1),
        "from the last write recorded, never moved back before it"
    );
    // Made again at NOW + 77 by the broker's clock: a write the old stream
    // took at NOW + 40 is inside the window, whatever the lake's clock says.
    let mut writer = Writer::start(h.lake.clone(), STREAM, made_at + 1).unwrap();
    place(&mut writer);
    let lost = tokio::task::block_in_place(|| writer.lost(Some(NOW + 77))).unwrap();
    assert_eq!(
        (lost[0].valid_from_ns, lost[0].valid_until_ns),
        (NOW + 1, NOW + 77)
    );
    assert!(lost[0].valid_from_ns <= NOW + 40 && NOW + 40 <= lost[0].valid_until_ns);
}

/// The review's finding 6: a fenced writer's mark of a lost stream is not
/// tried again for ever; it ends as a fenced batch ends the writer.
#[tokio::test(flavor = "multi_thread")]
async fn a_fenced_writer_marking_a_lost_stream_ends_saying_so() {
    let h = harness();
    tokio::task::block_in_place(|| gap_configured(&h));
    let mut writer = Writer::start(h.lake.clone(), STREAM, NOW - 5_000).unwrap();
    let price = close("coinbase-1", "F:1", BTC, "2026-10-07", "1");
    let taken = vec![written(1, "coinbase-1", vec![price])];
    tokio::task::block_in_place(|| writer.take(taken)).unwrap();
    // Taken over by another writer: this store acts as the deposed one,
    // at the epoch it had.
    h.store
        .take_over(&meridian_changes::TookOver::unnamed("a test"))
        .unwrap();
    h.store.fence_for_test(1);
    let ended = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        crate::write_ahead::lost_marked(&mut writer, None),
    )
    .await
    .expect("a fenced writer's mark was tried again and again");
    assert!(ended.unwrap_err().contains("fenced"));
    assert!(gaps(&h, "", GapState::Unspecified).is_empty());
}

/// The consensus of 2026-11-28, option 1: the stream is said gone only once
/// the broker has said "stream not found" at every look for the whole bound,
/// sixty seconds -- past the thirty its servers take to clean up a stream
/// whose files they still hold. A look answered any other way, a timeout or
/// no servers, starts the count again.
#[test]
fn a_stream_is_gone_only_once_the_broker_has_said_so_for_the_whole_bound() {
    use crate::write_ahead::{Gone, GONE_AFTER};
    use std::time::Duration;
    assert_eq!(GONE_AFTER, Duration::from_secs(60));
    let began = tokio::time::Instant::now();
    let at = |s: u64| began + Duration::from_secs(s);
    let mut gone = Gone::default();
    assert!(!gone.not_found(at(0), GONE_AFTER));
    assert!(!gone.not_found(at(59), GONE_AFTER));
    assert!(gone.not_found(at(60), GONE_AFTER));
    // A timeout between two not-founds: counted from the next not-found.
    let mut gone = Gone::default();
    assert!(!gone.not_found(at(0), GONE_AFTER));
    gone.other();
    assert!(!gone.not_found(at(59), GONE_AFTER));
    assert!(!gone.not_found(at(118), GONE_AFTER));
    assert!(gone.not_found(at(119), GONE_AFTER));
}

/// The consensus of 2026-11-28 (run 46090125342), options 1 and 2 as one,
/// against a real JetStream: the stream is deleted under a writer following
/// it, as the broker's orphan cleanup deleted it in that run. The writer
/// makes none for as long as the bound -- still none 45 s on -- then makes
/// it again and takes and answers its writes.
///
/// A write inside the gap, each time (the review's finding 3): the lake's
/// store refuses every batch while the stream takes and acknowledges
/// writes 4 and 5, and takes batches again only once the stream is
/// deleted, so the lake never records them. Their times at the broker are
/// read from the stream before it goes. The lake's place is still write 3;
/// the gap -- its dataset's, the one the lake keeps as written through the
/// stream -- is there when the stream made again is first seen; and both
/// writes' times are inside its window: neither is silently missing. The
/// lake's clock is the system's, as a deployment's is: at 7f80a4d3 this
/// test's clock stood still from before the stream was made, and the gap's
/// window ended before the writes it was to hold.
///
/// Run by `make test-jetstream`; by hand with `MERIDIAN_TEST_JETSTREAM_URL`
/// set and `--ignored --exact
/// tests::write_ahead::a_stream_deleted_under_a_following_writer_is_made_again_after_the_bound_and_its_loss_marked`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a JetStream broker: MERIDIAN_TEST_JETSTREAM_URL"]
async fn a_stream_deleted_under_a_following_writer_is_made_again_after_the_bound_and_its_loss_marked(
) {
    use crate::write_ahead::{identity, stream_config, write_forever, Bounds};
    use async_nats::jetstream;
    use futures_util::StreamExt as _;
    use meridian_bus::write_ahead::{subject, REPLY_HEADER, STREAM, TOPIC_HEADER};
    use std::time::Duration;
    let url = std::env::var("MERIDIAN_TEST_JETSTREAM_URL").unwrap();
    let client = meridian_bus::NatsBackend::client(&url).await.unwrap();
    let context = jetstream::new(client.clone());
    let _ = context.delete_stream(STREAM).await;
    let bounds = Bounds {
        max_age: Duration::from_secs(600),
        max_bytes: 64 << 20,
        replicas: 1,
    };
    // The lake on the system's clock, as a deployment's is: the gap's
    // window ends at the lake's time when the loss is marked.
    let h = {
        let clock = Arc::new(meridian_clock::SystemClock);
        let bus = Arc::new(Bus::single(
            "lake-1",
            Arc::new(MemoryBackend::new()),
            clock.clone(),
        ));
        stand_ins(&bus);
        let store = Arc::new(MemoryStore::new());
        let lake = service::serve(bus.clone(), store.clone(), clock);
        lake.configured(&configuration(true, &[]));
        Harness {
            bus,
            store,
            clock: Arc::new(ManualClock::at(NOW)),
            lake,
        }
    };
    tokio::task::block_in_place(|| gap_configured(&h));
    let tag = std::process::id();
    let mut answered = client
        .subscribe(format!("_INBOX.coinbase-1.sls-{tag}.>"))
        .await
        .unwrap();
    client.flush().await.unwrap();
    let put = |round: &'static str, n: u64| {
        let context = context.clone();
        async move {
            let price = at_valid(
                close(
                    "coinbase-1",
                    &format!("{round}-{n}"),
                    BTC,
                    "2026-10-07",
                    "1",
                ),
                NOW - 100_000 + n as i64,
            );
            let mut write = written(n, "coinbase-1", vec![price]);
            let mut headers = async_nats::HeaderMap::new();
            headers.insert(TOPIC_HEADER, write.topic.as_str());
            headers.insert(
                REPLY_HEADER,
                format!("_INBOX.coinbase-1.sls-{tag}.{round}.{n}").as_str(),
            );
            write.envelope.meta.as_mut().unwrap().published_at_ns = NOW;
            // Acknowledged by the stream: its sequence there.
            context
                .publish_with_headers(
                    subject("coinbase-1", "daily"),
                    headers,
                    write.envelope.encode_to_vec().into(),
                )
                .await
                .unwrap()
                .await
                .unwrap()
                .sequence
        }
    };
    async fn heard(
        answered: &mut async_nats::Subscriber,
        round: &str,
        want: usize,
        within: Duration,
    ) -> std::collections::BTreeSet<u64> {
        let mut seen = std::collections::BTreeSet::new();
        let until = tokio::time::Instant::now() + within;
        while seen.len() < want {
            match tokio::time::timeout_at(until, answered.next()).await {
                Ok(Some(message)) => {
                    let mut words = message.subject.as_str().rsplit('.');
                    let n: u64 = words.next().unwrap().parse().unwrap();
                    if words.next() == Some(round) && message.headers.is_none() {
                        seen.insert(n);
                    }
                }
                _ => break,
            }
        }
        seen
    }
    let mut stream = context.create_stream(stream_config(&bounds)).await.unwrap();
    let old = identity(stream.info().await.unwrap());
    let writer = {
        let (lake, client) = (h.lake.clone(), client.clone());
        tokio::spawn(async move { write_forever(lake, client, bounds, |_| {}).await })
    };
    for n in 1..=3 {
        put("old", n).await;
    }
    let first = heard(&mut answered, "old", 3, Duration::from_secs(30)).await;
    assert_eq!(first, [1, 2, 3].into(), "the stream's first three");
    let key = crate::write_ahead::stream_key(STREAM, old);
    let place = || h.lake.write_ahead_position(&key).unwrap().unwrap().sequence;
    assert_eq!(place(), 3);
    // The store refuses every batch; the stream takes and acknowledges two
    // more, whose times at the broker are read from it; then it is deleted
    // under the writer, and only then does the store take batches again.
    h.store.refuse_write_ahead_for_test(true);
    let mut held = Vec::new();
    for n in 4..=5 {
        let sequence = put("old", n).await;
        assert_eq!(sequence, n, "acknowledged by the stream");
        let at = stream.direct_get(sequence).await.unwrap().time;
        held.push((n, at.unix_timestamp_nanos() as i64));
    }
    context.delete_stream(STREAM).await.unwrap();
    let deleted = tokio::time::Instant::now();
    // Nothing the stream sent before it went is still on its way.
    tokio::time::sleep(Duration::from_secs(2)).await;
    h.store.refuse_write_ahead_for_test(false);
    let taken = heard(&mut answered, "old", 2, Duration::from_secs(5)).await;
    assert!(
        taken.is_empty(),
        "never recorded, never answered: {taken:?}"
    );
    // Not made again before the bound: 45 s on, the broker holds none.
    tokio::time::sleep_until(deleted + Duration::from_secs(45)).await;
    assert!(
        context.get_stream(STREAM).await.is_err(),
        "the stream was made again before its loss had stood for the bound"
    );
    assert!(
        gaps(&h, "", GapState::Unspecified).is_empty(),
        "a gap marked before the broker's word had stood for the bound"
    );
    // Made again once it has, by the writer, as another stream; its gap is
    // there when the stream made again is first seen.
    let mut made = None;
    while made.is_none() && deleted.elapsed() < Duration::from_secs(120) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(mut stream) = context.get_stream(STREAM).await {
            made = stream.info().await.ok().map(|info| identity(info));
        }
    }
    let marked = gaps(&h, "", GapState::Unspecified);
    let made = made.expect(
        "the stream deleted under the following writer was never made again: the lake takes \
         no write until its writer is restarted",
    );
    assert_ne!(made, old, "another stream");
    assert!(deleted.elapsed() >= Duration::from_secs(60));
    assert_eq!(marked.len(), 1, "the lost stream's gap alone: {marked:?}");
    assert_eq!(
        place(),
        3,
        "the lake's place in the lost stream is where it was"
    );
    // It records again.
    for n in 1..=2 {
        put("new", n).await;
    }
    let new = heard(&mut answered, "new", 2, Duration::from_secs(30)).await;
    writer.abort();
    let _ = writer.await;
    assert_eq!(
        new,
        [1, 2].into(),
        "the stream made again is taken and answered"
    );
    // One gap, its dataset's, unrecovered; writes 4 and 5, never recorded,
    // each inside its window.
    assert_eq!(gaps(&h, "", GapState::Unspecified), marked, "and no other");
    let gap = &marked[0];
    assert_eq!(gap.dataset, "coinbase-1:daily");
    assert!(gap.found_in_gap_id.is_empty());
    assert_eq!(gap.state, GapState::Unrecovered as i32);
    assert_eq!(gap.cause, GapCause::WriteAheadBound as i32);
    for (n, at_ns) in held {
        assert!(
            gap.valid_from_ns <= at_ns && at_ns <= gap.valid_until_ns,
            "write {n}, acknowledged by the deleted stream at {at_ns} and never recorded, is \
             outside the gap marked ({} to {})",
            gap.valid_from_ns,
            gap.valid_until_ns
        );
    }
    let rows = tokio::task::block_in_place(|| h.store.dataset_gaps()).unwrap();
    assert_eq!(rows.len(), 1, "marked once");
    context.delete_stream(STREAM).await.unwrap();
}

/// v31a item 5, the review of the stream guard, finding 1: the guard judges
/// the stream by the stream's own bounds, not the chart's. A stream made at
/// 256 MiB holds more than 128 MiB, every write of it past the lake's
/// place, when a writer starts whose chart says 128 MiB. After 40 s -- past
/// the 30 s a stream stands at a bound before it is said -- its member is
/// said stuck full only where the stream stands at the bound the stream
/// itself has by then, never for the chart's while the stream has room; and
/// none of its writes is dropped. (The chart's lower bound is not applied
/// while the stream holds more: the test after this one.)
///
/// By hand, as `a_write_naming_another_instances_inbox_is_recorded_and_
/// answered_to_nobody` above; this test empties the broker of the lake's
/// stream.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a JetStream broker: MERIDIAN_TEST_JETSTREAM_URL"]
async fn a_stream_with_room_under_its_own_bound_is_not_said_stuck_full_by_the_charts() {
    use crate::write_ahead::{guard::STUCK_FULL, stream_config, write_forever, Bounds};
    use async_nats::jetstream;
    use meridian_bus::write_ahead::{subject, STREAM};
    use std::time::Duration;
    let url = std::env::var("MERIDIAN_TEST_JETSTREAM_URL").unwrap();
    let client = meridian_bus::NatsBackend::client(&url).await.unwrap();
    let context = jetstream::new(client.clone());
    let _ = context.delete_stream(STREAM).await;
    let made = Bounds {
        max_age: Duration::from_secs(600),
        max_bytes: 256 << 20,
        replicas: 1,
    };
    let mut stream = context.create_stream(stream_config(&made)).await.unwrap();
    // 160 writes of 900 KB: 137 MiB, past 128.
    let body = vec![7u8; 900_000];
    for _ in 0..160 {
        context
            .publish(subject("coinbase-1", "daily"), body.clone().into())
            .await
            .unwrap()
            .await
            .unwrap();
    }
    let held = stream.info().await.unwrap().state.bytes;
    assert!(held > 128 << 20 && held < 200 << 20, "{held} bytes held");
    let h = harness();
    let said = Arc::new(std::sync::Mutex::new(Vec::<Option<String>>::new()));
    let writer = {
        let (lake, client, said) = (h.lake.clone(), client.clone(), Arc::clone(&said));
        let chart = Bounds {
            max_bytes: 128 << 20,
            ..made
        };
        tokio::spawn(async move {
            write_forever(lake, client, chart, move |why| {
                said.lock().unwrap().push(why)
            })
            .await
        })
    };
    tokio::time::sleep(Duration::from_secs(40)).await;
    writer.abort();
    let _ = writer.await;
    let info = stream.info().await.unwrap().clone();
    context.delete_stream(STREAM).await.unwrap();
    assert_eq!(info.state.messages, 160, "no write dropped by the bound");
    let said = said.lock().unwrap().clone();
    let stuck: Vec<_> = said
        .iter()
        .flatten()
        .filter(|why| why.starts_with(STUCK_FULL))
        .collect();
    assert!(
        stuck.is_empty() || info.state.bytes + (1 << 20) > info.config.max_bytes as u64,
        "said stuck full with {} bytes held of the stream's own {} bytes bound: {stuck:?}",
        info.state.bytes,
        info.config.max_bytes
    );
}

/// v31a item 5, the lowered byte bound: the lake never refuses its own
/// writes over a setting. A stream made at 256 MiB holds 137 MiB when a
/// writer starts whose chart says 128 MiB. Lowered, the stream would refuse
/// every write until the writes it holds aged out; so it keeps its 256 MiB
/// bound, takes the next write, and its member says why. The next writer
/// start applies the bound once what the stream holds fits it.
///
/// By hand, as `a_write_naming_another_instances_inbox_is_recorded_and_
/// answered_to_nobody` above; this test empties the broker of the lake's
/// stream.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a JetStream broker: MERIDIAN_TEST_JETSTREAM_URL"]
async fn a_byte_bound_under_what_the_stream_holds_is_not_applied_and_the_stream_takes_writes() {
    use crate::write_ahead::{stream_config, write_forever, Bounds};
    use async_nats::jetstream;
    use meridian_bus::write_ahead::{subject, STREAM};
    use std::time::Duration;
    let url = std::env::var("MERIDIAN_TEST_JETSTREAM_URL").unwrap();
    let client = meridian_bus::NatsBackend::client(&url).await.unwrap();
    let context = jetstream::new(client.clone());
    let _ = context.delete_stream(STREAM).await;
    let made = Bounds {
        max_age: Duration::from_secs(600),
        max_bytes: 256 << 20,
        replicas: 1,
    };
    let mut stream = context.create_stream(stream_config(&made)).await.unwrap();
    // 160 writes of 900 KB: 137 MiB, past 128.
    let body = vec![7u8; 900_000];
    for _ in 0..160 {
        context
            .publish(subject("coinbase-1", "daily"), body.clone().into())
            .await
            .unwrap()
            .await
            .unwrap();
    }
    let h = harness();
    let said = Arc::new(std::sync::Mutex::new(Vec::<Option<String>>::new()));
    let writer = {
        let (lake, client, said) = (h.lake.clone(), client.clone(), Arc::clone(&said));
        let chart = Bounds {
            max_bytes: 128 << 20,
            ..made
        };
        tokio::spawn(async move {
            write_forever(lake, client, chart, move |why| {
                said.lock().unwrap().push(why)
            })
            .await
        })
    };
    tokio::time::sleep(Duration::from_secs(15)).await;
    let taken = match context
        .publish(subject("coinbase-1", "daily"), body.clone().into())
        .await
    {
        Ok(ack) => ack.await.map(|_| ()).map_err(|f| f.to_string()),
        Err(failed) => Err(failed.to_string()),
    };
    writer.abort();
    let _ = writer.await;
    let info = stream.info().await.unwrap().clone();
    context.delete_stream(STREAM).await.unwrap();
    assert_eq!(
        info.config.max_bytes,
        256 << 20,
        "the stream keeps its own bound while it holds more than the chart's"
    );
    assert_eq!(taken, Ok(()), "the stream takes the next write");
    assert_eq!(info.state.messages, 161);
    let said = said.lock().unwrap().clone();
    let why = said
        .iter()
        .flatten()
        .find(|why| why.contains("keeps its 256.0 MiB bound"))
        .unwrap_or_else(|| panic!("the member says why its stream keeps its bound: {said:?}"));
    for part in [STREAM, "128.0 MiB", "holds 137.3 MiB"] {
        assert!(why.contains(part), "{part} in {why}");
    }
}

/// The review of 4c, fix B item 2: the place the stream's age bound is
/// held to is the lowest among the writer groups'. A group with no place
/// takes the stream from its first; one group alone, its own place.
#[tokio::test(flavor = "multi_thread")]
async fn the_lowest_place_among_the_writer_groups_is_the_one_the_age_bound_is_held_to() {
    use crate::write_ahead::lowest_place;
    use meridian_changes::groups::{owner, Groups};
    let h = harness();
    let dataset = "coinbase-1:daily";
    let (b, a) = (owner(dataset, 2), 1 - owner(dataset, 2));
    let lake_of = |group: u32| {
        service::Lake::grouped(
            h.bus.clone(),
            h.store.clone(),
            h.clock.clone(),
            Groups::new(group, 2).unwrap(),
        )
    };
    let pass = |group: u32, through: u64| {
        let batch: Vec<Taken> = (1..=through)
            .map(|sequence| {
                let mut taken = written(sequence, "coinbase-1", Vec::new());
                taken.dataset = dataset.to_string();
                taken
            })
            .collect();
        tokio::task::block_in_place(|| {
            let mut writer = Writer::start(lake_of(group), STREAM, 0).unwrap();
            writer.take(batch).unwrap();
            writer.next()
        })
    };
    // A at the stream's last + 1, B with no place: the stream's first.
    assert_eq!(pass(a, 40), 41);
    assert_eq!(lowest_place(&lake_of(a), STREAM, 41).unwrap(), 1);
    assert_eq!(lowest_place(&h.lake, STREAM, 41).unwrap(), 41, "one group");
    // B at 10: its next, from either group.
    assert_eq!(pass(b, 10), 11);
    assert_eq!(lowest_place(&lake_of(a), STREAM, 41).unwrap(), 11);
    assert_eq!(lowest_place(&lake_of(b), STREAM, 11).unwrap(), 11);
    // A lower age bound than the oldest write B has not recorded is old
    // is refused at B's place, and would not be at A's own.
    let now = h.clock.now_ns();
    let standing = crate::write_ahead::guard::Standing {
        messages: 40,
        bytes: 4_000,
        first: 11,
        last: 40,
        oldest_ns: now - 300_000_000_000,
    };
    let lower = crate::write_ahead::Bounds {
        max_age: std::time::Duration::from_secs(120),
        max_bytes: 256 << 20,
        replicas: 1,
    };
    let drop = |next| crate::write_ahead::guard::bounds_drop(STREAM, &standing, next, &lower, now);
    assert_eq!(drop(41), None);
    assert!(drop(11).is_some_and(|why| why.contains("30 write(s)")));
}

/// The review of 4c, fix B item 2: with several writer groups, a lower
/// age bound is held to the lowest place among them, never this group's
/// alone. Group A, at the stream's last write, starts with a lower age
/// bound than the stream's oldest write's age, while group B has recorded
/// none of the stream: the bound would drop writes B has not recorded,
/// so the stream is kept as it is, and A's member says why.
///
/// By hand, as the tests above; this test empties the broker of the
/// lake's stream.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a JetStream broker: MERIDIAN_TEST_JETSTREAM_URL"]
async fn a_lower_age_bound_is_refused_while_another_writer_group_has_not_recorded_the_streams_oldest(
) {
    use crate::write_ahead::{group_key, place, stream_config, stream_key, write_forever, Bounds};
    use async_nats::jetstream;
    use meridian_bus::write_ahead::{subject, STREAM};
    use meridian_changes::groups::{owner, Groups};
    use std::time::Duration;
    let url = std::env::var("MERIDIAN_TEST_JETSTREAM_URL").unwrap();
    let client = meridian_bus::NatsBackend::client(&url).await.unwrap();
    let context = jetstream::new(client.clone());
    let _ = context.delete_stream(STREAM).await;
    let made = Bounds {
        max_age: Duration::from_secs(600),
        max_bytes: 256 << 20,
        replicas: 1,
    };
    let mut stream = context.create_stream(stream_config(&made)).await.unwrap();
    for _ in 0..3 {
        context
            .publish(subject("coinbase-1", "daily"), vec![7u8; 100].into())
            .await
            .unwrap()
            .await
            .unwrap();
    }
    // The stream's writes older than the lower bound.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let h = harness();
    let wall = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    h.clock.set(i64::try_from(wall.as_nanos()).unwrap());
    let identity = place(&client).await.unwrap().stream;
    // Group A passes the stream's three writes, their dataset group B's.
    let dataset = "coinbase-1:daily";
    let (b, a) = (owner(dataset, 2), 1 - owner(dataset, 2));
    let lake = service::Lake::grouped(
        h.bus.clone(),
        h.store.clone(),
        h.clock.clone(),
        Groups::new(a, 2).unwrap(),
    );
    lake.configured(&configuration(true, &[]));
    let batch: Vec<Taken> = (1..=3)
        .map(|sequence| {
            let mut taken = written(sequence, "coinbase-1", Vec::new());
            taken.dataset = dataset.to_string();
            taken
        })
        .collect();
    let base = stream_key(STREAM, identity);
    tokio::task::block_in_place(|| {
        let mut writer = Writer::start(lake.clone(), STREAM, identity).unwrap();
        assert!(writer.take(batch).unwrap().is_empty());
    });
    let at = |group: u32| {
        h.store
            .write_ahead_position(&group_key(&base, group))
            .unwrap()
            .map(|at| at.sequence)
    };
    assert_eq!(
        (at(a), at(b)),
        (Some(3), None),
        "A at the last, B at the first"
    );
    let said = Arc::new(std::sync::Mutex::new(Vec::<Option<String>>::new()));
    let writer = {
        let (client, said) = (client.clone(), Arc::clone(&said));
        let chart = Bounds {
            max_age: Duration::from_secs(2),
            ..made
        };
        tokio::spawn(async move {
            write_forever(lake, client, chart, move |why| {
                said.lock().unwrap().push(why)
            })
            .await
        })
    };
    tokio::time::sleep(Duration::from_secs(4)).await;
    writer.abort();
    let _ = writer.await;
    let info = stream.info().await.unwrap().clone();
    context.delete_stream(STREAM).await.unwrap();
    let said = said.lock().unwrap().clone();
    assert_eq!(
        (info.config.max_age, info.state.messages),
        (Duration::from_secs(600), 3),
        "the stream keeps its age bound and group B's writes: {said:?}"
    );
    let why = said
        .iter()
        .flatten()
        .find(|why| why.contains("not yet recorded"))
        .unwrap_or_else(|| panic!("the member says why its stream is kept: {said:?}"));
    for part in [STREAM, "past the 2s bound", "3 write(s)"] {
        assert!(why.contains(part), "{part} in {why}");
    }
}
