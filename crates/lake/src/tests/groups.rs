//! The lake at two writer groups (step 3d; the resilience spec's sections
//! 6.1 and 6.7): every member serves the public write in the lake's queue
//! group; a write's datasets of another group are handed to that group's
//! writer, addressed to it, and the write answered once every part has
//! committed, with a watermark naming each dataset. A write answering a want
//! is group 0's to answer, and handed there whole.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use meridian_bus::{Answer, Backend, BusError, Handler, Stamp, Subscription};
use meridian_changes::groups::{owner, Groups};
use meridian_domain::v1::{RecordObservationsReply, RecordPricesRequest};

use super::*;

/// The broker, stood in: publishes and subscriptions the in-process
/// backend's; a public request taken by the member `to` names, as the
/// lake's queue group gives it to one; a hand-off answered by the member
/// of the writer group it is addressed to, as each group's own queue group
/// on the internal topic gives it.
struct GroupsBroker {
    inner: MemoryBackend,
    offered: Mutex<BTreeMap<String, Vec<Handler>>>,
    groups: Mutex<BTreeMap<(String, u32), Handler>>,
    to: AtomicUsize,
    handed: Mutex<Vec<u32>>,
}

fn answered(topic: &str, handler: Option<Handler>, envelope: Envelope) -> Answer<'_> {
    Box::pin(async move {
        let Some(handler) = handler else {
            return Err(BusError::NoHandler(topic.to_string()));
        };
        match tokio::task::spawn_blocking(move || handler(envelope))
            .await
            .unwrap()
        {
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

impl Backend for GroupsBroker {
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

    fn serve_group(&self, topic: &str, _: &str, group: u32, handler: Handler) {
        self.groups
            .lock()
            .unwrap()
            .insert((topic.to_string(), group), handler);
    }

    fn request<'a>(&'a self, topic: &'a str, envelope: Envelope, _: Duration) -> Answer<'a> {
        let handler = self.offered.lock().unwrap().get(topic).map(|offered| {
            let to = self.to.load(Ordering::SeqCst).min(offered.len() - 1);
            Arc::clone(&offered[to])
        });
        answered(topic, handler, envelope)
    }

    fn request_to_group<'a>(
        &'a self,
        topic: &'a str,
        envelope: Envelope,
        _: Duration,
        group: u32,
    ) -> Answer<'a> {
        self.handed.lock().unwrap().push(group);
        let handler = self
            .groups
            .lock()
            .unwrap()
            .get(&(topic.to_string(), group))
            .cloned();
        answered(topic, handler, envelope)
    }
}

/// coinbase-1's daily and hourly datasets, reporting-1 entitled to both.
fn two_datasets() -> EntitlementsChangedEvent {
    let mut configured = configuration(true, &[]);
    configured.datasets[1] = DatasetRef {
        dataset: "coinbase-1:hourly".into(),
        instance: "coinbase-1".into(),
        vendor: "Coinbase".into(),
        declaration: Some(declaration("hourly", 0)),
        ..Default::default()
    };
    configured.entitlements[1].dataset = "coinbase-1:hourly".into();
    configured
}

/// The lake's writers of groups 0 and 1 over one record, each serving the
/// public writes in the lake's queue group (`lake-1`), and coinbase-1's
/// sidecar asking through the broker.
struct Groups2 {
    broker: Arc<GroupsBroker>,
    store: Arc<MemoryStore>,
    client: Arc<Bus>,
}

fn two_groups() -> Groups2 {
    let clock = Arc::new(ManualClock::at(NOW));
    let broker = Arc::new(GroupsBroker {
        inner: MemoryBackend::new(),
        offered: Mutex::new(BTreeMap::new()),
        groups: Mutex::new(BTreeMap::new()),
        to: AtomicUsize::new(0),
        handed: Mutex::new(Vec::new()),
    });
    let store = Arc::new(MemoryStore::new());
    for group in 0..2 {
        let bus = Arc::new(Bus::single("lake-1", broker.clone(), clock.clone()));
        stand_ins(&bus);
        bus.hand_writes_on(service::INTERNAL_RECORD, group, service::is_record_write);
        if group > 0 {
            bus.serve_only(service::is_record_write);
        }
        let lake = service::serve_grouped(
            bus,
            store.clone(),
            clock.clone(),
            Groups::new(group, 2).unwrap(),
        );
        lake.configured(&two_datasets());
    }
    let client = Arc::new(Bus::single("coinbase-1", broker.clone(), clock));
    Groups2 {
        broker,
        store,
        client,
    }
}

fn price(dataset: &str, row_key: &str, amount: &str) -> Price {
    let mut price = close("coinbase-1", row_key, BTC, "2026-10-07", amount);
    price
        .meta
        .as_mut()
        .unwrap()
        .source
        .as_mut()
        .unwrap()
        .dataset = dataset.into();
    price
}

/// A write of `prices`, asked of the member of group `to`.
async fn write(
    g: &Groups2,
    to: usize,
    prices: Vec<Price>,
    want_id: &str,
) -> Result<RecordObservationsReply, String> {
    g.broker.to.store(to, Ordering::SeqCst);
    g.client
        .call_stamped(
            service::RECORD_PRICES,
            "meridian.v1.RecordPricesRequest",
            RecordPricesRequest {
                prices,
                want_id: want_id.into(),
            }
            .encode_to_vec(),
            None,
            Some(Duration::from_secs(10)),
            &Stamp::default(),
        )
        .await
        .map(|(_, reply)| RecordObservationsReply::decode(&reply[..]).unwrap())
        .map_err(|failed| failed.to_string())
}

fn named(reply: &RecordObservationsReply) -> Vec<String> {
    reply
        .watermark
        .iter()
        .flat_map(|w| w.partitions.iter().map(|p| p.partition.clone()))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_spanning_two_groups_is_split_one_hand_off_per_owner_and_answered_whole() {
    // A constant of the product: at two groups, daily is group 0's, hourly
    // group 1's.
    assert_eq!(
        (owner("coinbase-1:daily", 2), owner("coinbase-1:hourly", 2)),
        (0, 1)
    );
    let g = two_groups();
    for (to, other) in [(1, 0), (0, 1)] {
        g.broker.handed.lock().unwrap().clear();
        let key = format!("k{to}");
        let reply = write(
            &g,
            to,
            vec![
                price("coinbase-1:daily", &key, "100"),
                price("coinbase-1:hourly", &key, "101"),
            ],
            "",
        )
        .await
        .unwrap();
        assert_eq!(reply.recorded, 2, "each part counted once it committed");
        let mut named = named(&reply);
        named.sort();
        assert_eq!(named, ["coinbase-1:daily", "coinbase-1:hourly"]);
        assert_eq!(
            *g.broker.handed.lock().unwrap(),
            vec![other],
            "one hand-off, to the other group"
        );
    }
    // Each row recorded once, in its dataset.
    let heads = g.store.heads().unwrap();
    assert_eq!(heads.get("coinbase-1:daily"), Some(&2));
    assert_eq!(heads.get("coinbase-1:hourly"), Some(&2));
    // A write of one group's dataset alone, asked of that group: no
    // hand-off.
    g.broker.handed.lock().unwrap().clear();
    let reply = write(&g, 1, vec![price("coinbase-1:hourly", "solo", "7")], "")
        .await
        .unwrap();
    assert_eq!(reply.recorded, 1);
    assert!(g.broker.handed.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_answering_a_want_is_group_zeros_to_answer() {
    let g = two_groups();
    let reply = write(
        &g,
        1,
        vec![
            price("coinbase-1:daily", "w", "100"),
            price("coinbase-1:hourly", "w", "101"),
        ],
        "WANT-1",
    )
    .await
    .unwrap();
    assert_eq!(reply.recorded, 2);
    // Whole to group 0, which hands group 1 its part.
    assert_eq!(*g.broker.handed.lock().unwrap(), vec![0, 1]);
}
