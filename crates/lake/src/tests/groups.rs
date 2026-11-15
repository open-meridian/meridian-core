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

fn answered(
    topic: &str,
    handler: Option<Handler>,
    envelope: Envelope,
    timeout: Duration,
) -> Answer<'_> {
    Box::pin(async move {
        let Some(handler) = handler else {
            return Err(BusError::NoHandler(topic.to_string()));
        };
        // Its caller's deadline known to the handler, as the broker's path
        // makes it.
        let deadline = std::time::Instant::now() + timeout;
        match tokio::task::spawn_blocking(move || {
            meridian_bus::read_after::within_deadline(Some(deadline), || handler(envelope))
        })
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

    fn request<'a>(&'a self, topic: &'a str, envelope: Envelope, timeout: Duration) -> Answer<'a> {
        let handler = self.offered.lock().unwrap().get(topic).map(|offered| {
            let to = self.to.load(Ordering::SeqCst).min(offered.len() - 1);
            Arc::clone(&offered[to])
        });
        answered(topic, handler, envelope, timeout)
    }

    fn request_to_group<'a>(
        &'a self,
        topic: &'a str,
        envelope: Envelope,
        timeout: Duration,
        group: u32,
    ) -> Answer<'a> {
        self.handed.lock().unwrap().push(group);
        let handler = self
            .groups
            .lock()
            .unwrap()
            .get(&(topic.to_string(), group))
            .cloned();
        answered(topic, handler, envelope, timeout)
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

/// A write whose rows its store said another group's while it was written
/// -- its range was moving -- and that the lake then routed again and
/// committed, is answered with what it committed (step 3d; section 6.7; CI
/// 43662032539, tolerant: "answered recorded 0, restated 0, unchanged 0 for
/// one row"). It was handed on whole all the same, to the group the store
/// had said, whose answer of none of its rows replaced the write's own.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_routed_again_while_its_range_moved_is_answered_with_its_own_count() {
    assert_eq!(owner("coinbase-1:hourly", 2), 1);
    let g = two_groups();
    // Group 1's store, its view stale for one write, says the dataset group
    // 0's; the lake routes the rows again, and they are group 1's.
    g.store.say_moved_once("coinbase-1:hourly", 0);
    let reply = write(&g, 1, vec![price("coinbase-1:hourly", "moving", "7")], "")
        .await
        .unwrap();
    assert_eq!(
        (reply.recorded, reply.restated, reply.unchanged),
        (1, 0, 0),
        "its row counted"
    );
    assert_eq!(named(&reply), ["coinbase-1:hourly"]);
    assert!(
        g.broker.handed.lock().unwrap().is_empty(),
        "never handed on after it was answered: {:?}",
        g.broker.handed.lock().unwrap()
    );
}

/// A write whose range a move's switch holds longer than its caller waits
/// is answered OVERLOADED with a retry-after while the caller still waits
/// (step 3d; section 6.8, the hold: "at most moves.holdSeconds, never past
/// the caller's Meridian-Timeout-Ms"). The record loop held it for the
/// whole of moves.holdSeconds whatever its caller's deadline, as the book
/// did (PR CI 43675491409), and the caller timed out, answered nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_held_by_a_switch_is_answered_before_its_callers_deadline() {
    let g = two_groups();
    // Group 1's dataset, held as a switch holds it, for three seconds.
    g.store
        .hold_for("coinbase-1:hourly", Duration::from_secs(3));
    g.broker.to.store(1, Ordering::SeqCst);
    // A caller that waits a second and a half.
    let began = std::time::Instant::now();
    let said = g
        .client
        .call_stamped(
            service::RECORD_PRICES,
            "meridian.v1.RecordPricesRequest",
            RecordPricesRequest {
                prices: vec![price("coinbase-1:hourly", "held", "7")],
                want_id: String::new(),
            }
            .encode_to_vec(),
            None,
            Some(Duration::from_millis(1_500)),
            &Stamp::default(),
        )
        .await
        .map(|_| ())
        .map_err(|failed| failed.to_string());
    let took = began.elapsed();
    let refused = said.expect_err("held past its caller's wait: refused");
    let overloaded = format!(
        "refusal-reason {}",
        meridian_pb::v1::RefusalReason::Overloaded as i32
    );
    assert!(
        refused.contains(&overloaded) && refused.contains("retry="),
        "answered OVERLOADED with a retry-after, never left to time out: {refused}"
    );
    assert!(
        took < Duration::from_millis(1_500),
        "answered while its caller still waited: {took:?}"
    );
    // A caller that waits longer than the switch is answered its row.
    let reply = write(&g, 1, vec![price("coinbase-1:hourly", "held", "7")], "")
        .await
        .unwrap();
    assert_eq!(reply.recorded, 1);
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

/// The ownership pre-check (step 3d, decided 2026-11-13): a write's group
/// read from its request before any row is checked -- one wholly another
/// group's datasets', that group; a want's answer, group 0; a write
/// spanning groups, none, which its handler splits.
#[test]
fn a_writes_owner_is_read_from_its_request_first() {
    let groups = Groups::new(0, 2).unwrap();
    let datasets: Vec<String> = (0..40).map(|n| format!("coin-{n}:daily")).collect();
    let of = |group: u32| {
        datasets
            .iter()
            .find(|d| owner(d, 2) == group)
            .unwrap()
            .clone()
    };
    let (zero, one) = (of(0), of(1));
    let asked = |datasets: &[&str], want: &str| {
        let prices = datasets
            .iter()
            .map(|dataset| price(dataset, "BTC:07", "1"))
            .collect();
        Envelope {
            meta: None,
            payload_type: "meridian.v1.RecordPricesRequest".into(),
            payload: RecordPricesRequest {
                prices,
                want_id: want.into(),
            }
            .encode_to_vec(),
        }
    };
    let owner_of =
        |envelope: &Envelope| service::owner_of_write(groups, service::RECORD_PRICES, envelope);
    assert_eq!(owner_of(&asked(&[&one, &one], "")), Some(1));
    assert_eq!(owner_of(&asked(&[&zero], "")), Some(0));
    assert_eq!(
        owner_of(&asked(&[&zero, &one], "")),
        None,
        "spanning groups: split"
    );
    assert_eq!(
        owner_of(&asked(&[&one], "WANT-1")),
        Some(0),
        "a want's answer"
    );
    let split = Envelope {
        meta: None,
        payload_type: "meridian.v1.SplitPartitionRequest".into(),
        payload: meridian_domain::v1::SplitPartitionRequest {
            partition: format!("{one}/0000-7fff"),
            reason: "why".into(),
        }
        .encode_to_vec(),
    };
    assert_eq!(
        service::owner_of_write(groups, service::SPLIT_PARTITION, &split),
        Some(1),
        "a range is its dataset's group's"
    );
}
