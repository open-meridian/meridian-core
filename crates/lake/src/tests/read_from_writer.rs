//! A lake reader member's reads that depend on the writer's own acts or
//! memory, handed to the writer on `platform.lake.internal.read-from-writer`,
//! so the answer never depends on which member takes the read (contract
//! v30, stage B; the resilience spec's section 5, approved 2026-11-10).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use meridian_bus::{Answer, Backend, BusError, Handler, Stamp, Subscription};
use meridian_domain::v1::{
    DatasetLiveness, ListDatasetsReply, ListDatasetsRequest, ListPricesReply, ListPricesRequest,
    RecordLivenessRequest,
};

use super::*;
use crate::hot::{Bounds, HotStore};
use crate::reader::ReaderStore;

/// The broker, stood in: its publishes and subscriptions the in-process
/// backend's, every bus's handlers offered on it, and a request taken by
/// one of its topic's offerers -- the member `to` names where two offer it,
/// as a queue group gives each request to one.
struct Broker {
    inner: MemoryBackend,
    offered: Mutex<BTreeMap<String, Vec<Handler>>>,
    to: AtomicUsize,
}

impl Backend for Broker {
    fn publish(&self, topic: &str, envelope: Envelope) -> Result<u64, BusError> {
        self.inner.publish(topic, envelope)
    }

    fn subscribe(&self, pattern: &str) -> Subscription {
        self.inner.subscribe(pattern)
    }

    fn subscribe_shared(&self, pattern: &str, group: &str) -> Subscription {
        self.inner.subscribe_shared(pattern, group)
    }

    fn dropped(&self) -> u64 {
        self.inner.dropped()
    }

    fn serve(&self, topic: &str, _: &str, handler: Handler) {
        self.offered
            .lock()
            .unwrap()
            .entry(topic.to_string())
            .or_default()
            .push(handler);
    }

    fn request<'a>(&'a self, topic: &'a str, envelope: Envelope, timeout: Duration) -> Answer<'a> {
        let handler = self.offered.lock().unwrap().get(topic).map(|offered| {
            let to = self.to.load(Ordering::SeqCst).min(offered.len() - 1);
            Arc::clone(&offered[to])
        });
        Box::pin(async move {
            let Some(handler) = handler else {
                return Err(BusError::NoHandler(topic.to_string()));
            };
            let deadline = Instant::now() + timeout;
            let answered = tokio::task::spawn_blocking(move || {
                meridian_bus::read_after::within_deadline(Some(deadline), || handler(envelope))
            })
            .await
            .map_err(|failed| BusError::HandlerFailed {
                topic: topic.to_string(),
                detail: failed.to_string(),
            })?;
            match answered {
                Ok((payload_type, payload)) => Ok(Envelope {
                    meta: None,
                    payload_type,
                    payload,
                }),
                Err(detail) => Err(BusError::HandlerFailed {
                    topic: topic.to_string(),
                    detail,
                }),
            }
        })
    }
}

/// Where the broker sends a read: the lake's writer, offered first, or its
/// reader.
const WRITER: usize = 0;
const READER: usize = 1;

/// The lake's writer and a reader member over the same record, as a
/// deployment runs them, and a plugin asking through the broker.
struct Members {
    broker: Arc<Broker>,
    writer: Arc<Lake>,
    writer_bus: Arc<Bus>,
    client: Arc<Bus>,
    store: Arc<MemoryStore>,
    clock: Arc<ManualClock>,
}

fn members(configured: &EntitlementsChangedEvent) -> Members {
    let clock = Arc::new(ManualClock::at(NOW));
    let broker = Arc::new(Broker {
        inner: MemoryBackend::new(),
        offered: Mutex::new(BTreeMap::new()),
        to: AtomicUsize::new(WRITER),
    });
    let writer_bus = Arc::new(Bus::single("lake-1", broker.clone(), clock.clone()));
    stand_ins(&writer_bus);
    let store = Arc::new(MemoryStore::new());
    let writer = service::serve(writer_bus.clone(), store.clone(), clock.clone());
    writer.configured(configured);
    // The reader: the same instance, so the same queue group, serving reads
    // alone, its hot layer over the writer's record.
    let reader_bus = Arc::new(Bus::single("lake-1", broker.clone(), clock.clone()));
    reader_bus.serve_only(meridian_bus::read_after::is_read);
    let primary: Arc<dyn Store> = store.clone();
    let heads = Arc::clone(&primary);
    let reading = ReaderStore::new(
        HotStore::new(Arc::clone(&primary), Bounds::DEFAULT),
        primary,
        None,
        Arc::new(move || heads.heads().unwrap()),
    );
    let reader = service::serve(reader_bus, reading, clock.clone());
    reader.configured(configured);
    let client = Arc::new(Bus::single("reporting-1", broker.clone(), clock.clone()));
    Members {
        broker,
        writer,
        writer_bus,
        client,
        store,
        clock,
    }
}

/// `request` asked on `topic` by reporting-1's sidecar, the broker taking
/// it to `member`.
async fn ask<R: Message + Default>(
    m: &Members,
    member: usize,
    topic: &str,
    payload_type: &str,
    request: impl Message,
) -> R {
    m.broker.to.store(member, Ordering::SeqCst);
    let stamp = Stamp {
        account_scope: Some(Vec::new()),
        ..Default::default()
    };
    let (_, reply) = m
        .client
        .call_stamped(
            topic,
            payload_type,
            request.encode_to_vec(),
            None,
            Some(Duration::from_secs(10)),
            &stamp,
        )
        .await
        .unwrap();
    R::decode(&reply[..]).unwrap()
}

fn btc_on(date: &str) -> ListPricesRequest {
    ListPricesRequest {
        subjects: vec![SubjectRef {
            entity_id: BTC.into(),
            ..Default::default()
        }],
        business_date: date.into(),
        ..Default::default()
    }
}

/// Each row by its key and amount, and each dataset not answered by why.
type Said = (Vec<(String, String)>, Vec<(String, i32)>);

/// What a reply answers: its rows and what it could not, but for when the
/// rows were recorded and the watermark it was answered at.
fn said(reply: &ListPricesReply) -> Said {
    (
        reply
            .prices
            .iter()
            .map(|price| (price.meta.as_ref().unwrap().row_key.clone(), amount(price)))
            .collect(),
        reply
            .unanswered
            .iter()
            .map(|u| (u.dataset.clone(), u.reason))
            .collect(),
    )
}

/// A read of a dataset served, not kept, raises a want -- the writer's
/// recorded act -- and waits for what is served: a reader member that takes
/// it hands it to the writer, and the plugin hears the want and is answered
/// the rows served, as it is when the writer takes it. A dataset's liveness,
/// which only the writer's commands set, is the writer's to list too.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_that_depends_on_the_writer_is_answered_alike_whichever_member_takes_it() {
    let m = members(&configuration(false, &[]));
    // The data plugin: each want of coinbase-1:daily served, and counted.
    let wants = Arc::new(AtomicUsize::new(0));
    let served = Arc::new(AtomicUsize::new(0));
    let mut wanted = m.writer_bus.subscribe_every(service::OBSERVATIONS_WANTED);
    let (writer, heard, done) = (m.writer.clone(), wants.clone(), served.clone());
    tokio::spawn(async move {
        while let Some(delivery) = wanted.recv().await {
            let want = ObservationsWantedEvent::decode(&delivery.envelope.payload[..]).unwrap();
            if want.dataset != "coinbase-1:daily" {
                continue;
            }
            heard.fetch_add(1, Ordering::SeqCst);
            tokio::task::block_in_place(|| {
                writer.record(
                    "prices",
                    vec![Observation::Price(Box::new(close(
                        "coinbase-1",
                        "BTC-USD:2026-10-07",
                        BTC,
                        "2026-10-07",
                        "62000.5",
                    )))],
                    &want.want_id,
                    &envelope("coinbase-1", true, ""),
                )
            })
            .unwrap();
            done.fetch_add(1, Ordering::SeqCst);
        }
    });
    let at_writer: ListPricesReply = ask(
        &m,
        WRITER,
        service::LIST_PRICES,
        "meridian.v1.ListPricesRequest",
        btc_on("2026-10-07"),
    )
    .await;
    assert_eq!(at_writer.prices.len(), 1, "served to the read that asked");
    assert_eq!(wants.load(Ordering::SeqCst), 1);
    // The want settled before the next read asks again.
    for _ in 0..200 {
        if served.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let at_reader: ListPricesReply = ask(
        &m,
        READER,
        service::LIST_PRICES,
        "meridian.v1.ListPricesRequest",
        btc_on("2026-10-07"),
    )
    .await;
    assert_eq!(
        wants.load(Ordering::SeqCst),
        2,
        "the read the reader took raised its want, at the writer"
    );
    assert_eq!(said(&at_reader), said(&at_writer), "answered alike");
    // Answered by the writer: its record's position, which serving the second
    // want moved on.
    let head = m.store.heads().unwrap()["coinbase-1:daily"];
    let answered_at = |reply: &ListPricesReply| {
        reply
            .watermark
            .as_ref()
            .unwrap()
            .partitions
            .iter()
            .find(|p| p.partition == "coinbase-1:daily")
            .unwrap()
            .sequence
    };
    assert!(answered_at(&at_writer) < head);
    assert_eq!(answered_at(&at_reader), head, "the writer's position");

    // A dataset's liveness, set by the writer's command, listed alike.
    tokio::task::block_in_place(|| {
        m.writer.record_liveness(
            RecordLivenessRequest {
                liveness: vec![DatasetLiveness {
                    dataset: "coinbase-1:daily".into(),
                    venue_id: "VEN-01JA0000000000000CBEXC".into(),
                    alive_at_ns: NOW - 5,
                    instance: String::new(),
                }],
            },
            &envelope("coinbase-1", true, ""),
        )
    })
    .unwrap();
    let listed = |member| {
        ask::<ListDatasetsReply>(
            &m,
            member,
            service::LIST_DATASETS,
            "meridian.v1.ListDatasetsRequest",
            ListDatasetsRequest::default(),
        )
    };
    let (at_writer, at_reader) = (listed(WRITER).await, listed(READER).await);
    let daily = at_reader
        .datasets
        .iter()
        .find(|d| d.dataset == "coinbase-1:daily")
        .unwrap();
    assert_eq!(
        daily.liveness.len(),
        1,
        "the writer's liveness, from the reader"
    );
    assert_eq!(at_reader, at_writer);
}

/// A read of a kept dataset the record holds nothing of, from a source
/// that is asked: the reader member asks nothing itself, the writer asks
/// the source once for each read, and both say so alike.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_that_asks_a_source_is_wanted_by_the_writer_whichever_member_takes_it() {
    let m = members(&configuration(true, &[]));
    let mut wanted = m.writer_bus.subscribe_every(service::OBSERVATIONS_WANTED);
    let at_writer: ListPricesReply = ask(
        &m,
        WRITER,
        service::LIST_PRICES,
        "meridian.v1.ListPricesRequest",
        btc_on("2026-10-07"),
    )
    .await;
    let first = futures_lite_recv(&mut wanted);
    let at_reader: ListPricesReply = ask(
        &m,
        READER,
        service::LIST_PRICES,
        "meridian.v1.ListPricesRequest",
        btc_on("2026-10-06"),
    )
    .await;
    let second = futures_lite_recv(&mut wanted);
    assert_eq!(first.business_date, "2026-10-07");
    assert_eq!(
        second.business_date, "2026-10-06",
        "the reader's read wanted"
    );
    let reasons = |reply: &ListPricesReply| -> Vec<i32> {
        reply.unanswered.iter().map(|u| u.reason).collect()
    };
    assert!(reasons(&at_writer).contains(&(UnansweredReason::AskedSource as i32)));
    assert_eq!(reasons(&at_reader), reasons(&at_writer));
}

/// Never blind (the resilience spec's section 8): a default read of the
/// latest now whose first source's rows are past its stale ceiling -- which
/// only its liveness, the writer's, can say is alive -- is handed to the
/// writer by a reader member, so a stalled source is passed over, and a live
/// one kept, alike whichever member takes it.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_only_liveness_can_settle_is_the_writers_whichever_member_takes_it() {
    let mut config = configuration(true, &[]);
    for dataset in &mut config.datasets {
        if dataset.dataset == "coinbase-1:daily" {
            dataset.stale_after_ns = 10;
        }
    }
    let m = members(&config);
    let latest = |instance: &str, row_key: &str, amount: &str| {
        let mut price = close(instance, row_key, BTC, "", amount);
        let meta = price.meta.as_mut().unwrap();
        meta.business_date = String::new();
        meta.valid_until_ns = 0;
        tokio::task::block_in_place(|| {
            m.writer.record(
                "prices",
                vec![Observation::Price(Box::new(price))],
                "",
                &envelope(instance, true, ""),
            )
        })
        .unwrap();
    };
    latest("coinbase-1", "c", "61000");
    latest("kraken-1", "k", "61010");
    // Past the ceiling since the rows were recorded, and alive by the
    // source's liveness, which the writer alone holds: coinbase answers.
    m.clock.advance(100);
    tokio::task::block_in_place(|| {
        m.writer.record_liveness(
            RecordLivenessRequest {
                liveness: vec![DatasetLiveness {
                    dataset: "coinbase-1:daily".into(),
                    venue_id: String::new(),
                    alive_at_ns: NOW + 100,
                    instance: String::new(),
                }],
            },
            &envelope("coinbase-1", true, ""),
        )
    })
    .unwrap();
    let at_writer: ListPricesReply = ask(
        &m,
        WRITER,
        service::LIST_PRICES,
        "meridian.v1.ListPricesRequest",
        btc_on(""),
    )
    .await;
    let at_reader: ListPricesReply = ask(
        &m,
        READER,
        service::LIST_PRICES,
        "meridian.v1.ListPricesRequest",
        btc_on(""),
    )
    .await;
    assert_eq!(
        said(&at_writer).0,
        vec![("c".to_string(), "61000".to_string())]
    );
    assert_eq!(said(&at_reader), said(&at_writer), "answered alike");
    // No sign of life within the ceiling: kraken answers, from either.
    m.clock.advance(100);
    let at_writer: ListPricesReply = ask(
        &m,
        WRITER,
        service::LIST_PRICES,
        "meridian.v1.ListPricesRequest",
        btc_on(""),
    )
    .await;
    let at_reader: ListPricesReply = ask(
        &m,
        READER,
        service::LIST_PRICES,
        "meridian.v1.ListPricesRequest",
        btc_on(""),
    )
    .await;
    assert_eq!(
        said(&at_writer).0,
        vec![("k".to_string(), "61010".to_string())]
    );
    assert_eq!(said(&at_reader), said(&at_writer), "answered alike");
}
