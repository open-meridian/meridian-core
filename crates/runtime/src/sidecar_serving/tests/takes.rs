//! Ruling 184 (option B; item 33): a shard's taker serves its share of the
//! instance's standing wants within 10 s of the lapse. Driven whole: the
//! lake itself -- its take, its wants, its sweep at its own period, never
//! shortened -- a pod of `feed-1`'s two shards, and its plugin, on one
//! broker in memory that answers each bus's calls wherever they are served.

use super::*;
use meridian_bus::{Answer, Backend, BusError, Handler, Subscription};
use prost::Message as _;
use std::collections::BTreeSet;

const WANTED: &str = "platform.lake.event.observations-wanted";
/// The resilience spec's sections 3 and 7, item 33: a taker serves within
/// 10 s of the Lease lapsing.
const TAKES_WITHIN: f64 = 10.0;

/// The deployment's broker in memory: every bus's messages reach every
/// subscriber, and a call is answered by the bus that serves its topic.
#[derive(Default)]
struct Broker {
    memory: MemoryBackend,
    served: Mutex<std::collections::HashMap<String, Handler>>,
    /// Every pattern subscribed to, in order.
    subscribed: Mutex<Vec<String>>,
}

impl Backend for Broker {
    fn publish(&self, topic: &str, envelope: meridian_bus::Envelope) -> Result<u64, BusError> {
        self.memory.publish(topic, envelope)
    }
    fn subscribe(&self, pattern: &str) -> Subscription {
        self.subscribed.lock().unwrap().push(pattern.to_string());
        self.memory.subscribe(pattern)
    }
    fn subscribe_shared(&self, pattern: &str, group: &str) -> Subscription {
        self.subscribed.lock().unwrap().push(pattern.to_string());
        self.memory.subscribe_shared(pattern, group)
    }
    fn serve(&self, topic: &str, _group: &str, handler: Handler) {
        self.served
            .lock()
            .unwrap()
            .entry(topic.to_string())
            .or_insert(handler);
    }
    fn request<'a>(
        &'a self,
        topic: &'a str,
        envelope: meridian_bus::Envelope,
        timeout: Duration,
    ) -> Answer<'a> {
        let handler = self.served.lock().unwrap().get(topic).cloned();
        Box::pin(async move {
            let handler = handler.ok_or_else(|| BusError::NoHandler(topic.to_string()))?;
            let answered = tokio::time::timeout(
                timeout,
                tokio::task::spawn_blocking(move || handler(envelope)),
            )
            .await
            .map_err(|_| BusError::Timeout {
                topic: topic.to_string(),
                timeout_ms: timeout.as_millis() as u64,
            })?
            .map_err(|failed| BusError::HandlerFailed {
                topic: topic.to_string(),
                detail: failed.to_string(),
            })?
            .map_err(|detail| BusError::HandlerFailed {
                topic: topic.to_string(),
                detail,
            })?;
            Ok(meridian_bus::Envelope {
                meta: None,
                payload_type: answered.0,
                payload: answered.1,
            })
        })
    }
    fn dropped(&self) -> u64 {
        self.memory.dropped()
    }
}

/// The lake, serving on `broker` with its own sweep, holding `feed-1`'s
/// prices (a cadence of a minute, kept) that `reporting-1` reads; the
/// conductor and the book's writer group stood in.
struct Deployment {
    broker: Arc<Broker>,
    /// `reporting-1`'s bus: it renews its standing want, and hears it.
    reporting: Arc<Bus>,
    _lake: Arc<meridian_lake::service::Lake>,
}

fn bus_as(broker: &Arc<Broker>, instance: &str) -> Arc<Bus> {
    Arc::new(Bus::single(
        instance,
        Arc::clone(broker) as Arc<dyn Backend>,
        Arc::new(meridian_clock::SystemClock),
    ))
}

async fn deployment() -> Deployment {
    use meridian_domain::v1::{DatasetEntitlement, DatasetRef, EntitlementsChangedEvent};
    use meridian_pb::v1::{DatasetDeclaration, ObservationMode};
    let broker = Arc::new(Broker::default());
    let bus = bus_as(&broker, "lake");
    // The book's one writer group, raising a shard's epoch when asked.
    bus.serve(
        &meridian_changes::shard_fence::raise_topic("book", 0),
        |envelope| {
            let took = meridian_domain::v1::ShardTookOver::decode(&envelope.payload[..]).unwrap();
            Ok((
                "meridian.v1.TakeShardReply".into(),
                meridian_domain::v1::TakeShardReply { epoch: took.epoch }.encode_to_vec(),
            ))
        },
    );
    // The conductor's answer to a plugin's configuration.
    bus.serve("platform.config.query.plugin-configuration", |_| {
        Ok((
            "meridian.v1.PluginConfiguration".into(),
            meridian_domain::v1::PluginConfiguration::default().encode_to_vec(),
        ))
    });
    let lake = {
        let bus = Arc::clone(&bus);
        tokio::spawn(async move {
            meridian_lake::service::serve(
                bus,
                Arc::new(meridian_lake::MemoryStore::new()),
                Arc::new(meridian_clock::SystemClock),
            )
        })
        .await
        .unwrap()
    };
    lake.configured(&EntitlementsChangedEvent {
        datasets: vec![DatasetRef {
            dataset: "feed-1:prices".into(),
            instance: "feed-1".into(),
            vendor: "Vendor".into(),
            declaration: Some(DatasetDeclaration {
                key: "prices".into(),
                vendor: "Vendor".into(),
                data_types: vec!["meridian.v1.Price".into()],
                modes: vec![ObservationMode::Pull as i32],
                cadence: 60,
                history: 30,
                day_time_zone: "Etc/UTC".into(),
                ..Default::default()
            }),
            ..Default::default()
        }],
        entitlements: vec![DatasetEntitlement {
            dataset: "feed-1:prices".into(),
            instance: "reporting-1".into(),
            allowed: true,
            ..Default::default()
        }],
        licences: Vec::new(),
        changed_at_ns: crate::now_ns(),
    });
    Deployment {
        reporting: bus_as(&broker, "reporting-1"),
        broker,
        _lake: lake,
    }
}

impl Deployment {
    /// `reporting-1`'s subscription keeps `names` current: the lake opens a
    /// standing want of `feed-1`'s prices and publishes it, and its sidecar
    /// renews it every 10 s, as within its 30 s lease, until dropped.
    /// Answers the want.
    async fn standing_want(
        &self,
        names: &[String],
    ) -> (meridian_domain::v1::ObservationsWantedEvent, Renewing) {
        let mut heard = self.reporting.subscribe_every(WANTED);
        renew(&self.reporting, names).await;
        let want = tokio::time::timeout(Duration::from_secs(2), heard.recv())
            .await
            .expect("the lake published no want")
            .unwrap();
        let want = meridian_domain::v1::ObservationsWantedEvent::decode(&want.envelope.payload[..])
            .unwrap();
        assert!(want.standing && want.dataset == "feed-1:prices", "{want:?}");
        let (bus, names) = (Arc::clone(&self.reporting), names.to_vec());
        let renewing = Renewing(tokio::task::spawn_local(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                renew(&bus, &names).await;
            }
        }));
        (want, renewing)
    }

    /// A data plugin's pod of `feed-1`'s two shards on the broker, its
    /// Leases in `store`.
    fn pod(&self, store: &Arc<super::Memory>) -> Taker {
        self.pod_as(store, "data")
    }

    /// How many subscriptions to the want rows were made on the broker.
    fn hearing_wants(&self) -> usize {
        self.broker
            .subscribed
            .lock()
            .unwrap()
            .iter()
            .filter(|pattern| *pattern == WANTED || pattern.ends_with("want-withdrawn"))
            .count()
    }

    /// A pod of `feed-1`'s two shards whose plugin is of `role`.
    fn pod_as(&self, store: &Arc<super::Memory>, role: &str) -> Taker {
        store.make_shards(2);
        let (surface, door) = (free_port(), free_port());
        let verifier = Arc::new(Verifier::new("feed-1", "/nonexistent/dashboard-keys"));
        let broker = Arc::clone(&self.broker);
        let serving = Arc::new(Serving {
            deployment_id: "d-1".into(),
            identity: Identity::new("feed-1", vec![role.to_string()]),
            verifier: Some(Arc::clone(&verifier)),
            front: Some((door, verifier, None)),
            listening: surface,
            instance_id: "feed-1".into(),
            limits: Limits::DEFAULT,
            write_share: None,
            declaration: None,
            bus: Arc::new(move || {
                let bus = bus_as(&broker, "feed-1");
                Box::pin(async move { Ok(bus) })
            }),
        });
        let (stopper, stop) = watch::channel(false);
        let api = Arc::clone(store);
        let ended = tokio::task::spawn_local(serve_as_shards(
            serving,
            2,
            move || Ok(Arc::clone(&api)),
            (RENEW, LASTS),
            stop,
        ));
        Taker {
            surface,
            _ended: ended,
            _stopper: stopper,
        }
    }
}

/// `bus`'s renewal of its standing want of `names`, as a sidecar renews
/// its plugin's subscriptions' wants (ruling 885).
async fn renew(bus: &Bus, names: &[String]) {
    bus.call_stamped(
        meridian_lake::service::RENEW_WANTS,
        "meridian.v1.RenewWantsRequest",
        meridian_domain::v1::RenewWantsRequest {
            wants: vec![meridian_domain::v1::RenewedWant {
                data_type: "meridian.v1.Price".into(),
                subjects: names
                    .iter()
                    .map(|entity| meridian_domain::v1::SubjectRef {
                        entity_id: entity.clone(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }],
        }
        .encode_to_vec(),
        None,
        Some(Duration::from_secs(5)),
        &meridian_bus::Stamp {
            account_scope: Some(Vec::new()),
            ..Default::default()
        },
    )
    .await
    .expect("the lake took the renewal");
}

struct Taker {
    surface: SocketAddr,
    _ended: tokio::task::JoinHandle<Result<(), String>>,
    _stopper: watch::Sender<bool>,
}

/// Shard `shard`'s Lease held by `holder` and renewed until dropped, as
/// another pod's is; written before this returns.
async fn holds(store: &Arc<super::Memory>, shard: u32, holder: &str) -> Renewing {
    store.make_shards(2);
    let lease = crate::shards::lease_name("feed-1", shard);
    let renewing = {
        let (store, lease, holder) = (Arc::clone(store), lease.clone(), holder.to_string());
        Renewing(tokio::task::spawn_local(async move {
            loop {
                if let Ok(Some(mut record)) = store.get(&lease).await {
                    if record.holder != holder {
                        record.transitions += 1;
                    }
                    record.holder = holder.clone();
                    record.renew_time = crate::lease::micro_time(crate::now_ns());
                    let _ = store.update(&lease, &record).await;
                }
                tokio::time::sleep(RENEW).await;
            }
        }))
    };
    assert!(within!(1.0, store.holder(&lease) == holder));
    renewing
}

/// The plugin at `at`: registered, and its one stream of what it hears.
async fn plugin_stream(at: SocketAddr) -> tonic::Streaming<meridian_pb::plugin::v1::Delivery> {
    use meridian_pb::plugin::v1::plugin_operations_client::PluginOperationsClient;
    use meridian_pb::v1::sidecar_service_client::SidecarServiceClient;
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{at}"))
        .unwrap()
        .connect()
        .await
        .expect("the plugin reached its sidecar");
    let reply = SidecarServiceClient::new(channel.clone())
        .register(meridian_pb::v1::RegisterRequest {
            schema_version: "v7".into(),
            ..Default::default()
        })
        .await
        .expect("registered")
        .into_inner();
    assert!(reply.admitted, "{}", reply.refusal_reason);
    PluginOperationsClient::new(channel)
        .receive(meridian_pb::plugin::v1::ReceiveRequest::default())
        .await
        .expect("its stream")
        .into_inner()
}

/// The next want the plugin is handed before `until`: its ID and its
/// subjects, sorted; `None` where none is.
async fn next_want(
    stream: &mut tonic::Streaming<meridian_pb::plugin::v1::Delivery>,
    until: tokio::time::Instant,
) -> Option<(String, Vec<String>)> {
    use meridian_pb::plugin::v1::delivery::Item;
    loop {
        let delivery = tokio::time::timeout_at(until, stream.message())
            .await
            .ok()?
            .expect("a delivery, not a refusal")
            .expect("the stream is open");
        if let Some(Item::ObservationsWanted(want)) = delivery.item {
            let mut subjects: Vec<String> =
                want.subjects.into_iter().map(|s| s.entity_id).collect();
            subjects.sort();
            return Some((want.want_id, subjects));
        }
    }
}

fn subjects(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("INS-{i:03}")).collect()
}

/// Of `names`, shard `mine`'s while shards 0 and 1 live.
fn share_of(mine: u32, names: &[String]) -> Vec<String> {
    let live = BTreeSet::from([0, 1]);
    names
        .iter()
        .filter(|name| meridian_sidecar::shards::keeper(name, &live) == Some(mine))
        .cloned()
        .collect()
}

/// A standby pod: the lake's standing want was published before it took
/// shard 1, and the lake will not deliver it again for a minute. Its plugin
/// is handed shard 1's share within 10 s of the lapse.
#[test]
fn a_standby_taking_a_shard_serves_its_share_of_a_standing_want_within_ten_seconds() {
    local(async {
        let deployment = deployment().await;
        let store = Arc::new(super::Memory::default());
        let _a = holds(&store, 0, "a").await;
        let c = holds(&store, 1, "c").await;
        let pod = deployment.pod(&store);
        let names = subjects(40);
        let (want, _renewing) = deployment.standing_want(&names).await;
        tokio::time::sleep(LASTS).await;
        assert!(!up(pod.surface).await, "a standby serves its plugin");

        // Shard 1's pod is gone: its Lease lapses, and the standby takes it.
        drop(c);
        let lapsed = tokio::time::Instant::now();
        let until = lapsed + Duration::from_secs_f64(TAKES_WITHIN);
        assert!(
            within!(TAKES_WITHIN, up(pod.surface).await),
            "the standby never took shard 1"
        );
        let mut stream = plugin_stream(pod.surface).await;
        let mine = share_of(1, &names);
        assert!(!mine.is_empty() && mine.len() < names.len());
        assert_eq!(
            next_want(&mut stream, until).await,
            Some((want.want_id.clone(), mine)),
            "shard 1's share was not served within {TAKES_WITHIN} s of the lapse"
        );
    });
}

/// A pod new to the instance, started once shard 1's Lease lapsed, whose
/// plugin registers two seconds after its sidecar serves: the want was
/// published before either. Its plugin is handed shard 1's share within
/// 10 s of the lapse.
#[test]
fn a_new_pod_whose_plugin_registers_late_serves_its_share_within_ten_seconds() {
    local(async {
        let deployment = deployment().await;
        let store = Arc::new(super::Memory::default());
        let _a = holds(&store, 0, "a").await;
        let c = holds(&store, 1, "c").await;
        let names = subjects(40);
        let (want, _renewing) = deployment.standing_want(&names).await;

        drop(c);
        let lapsed = tokio::time::Instant::now();
        let until = lapsed + Duration::from_secs_f64(TAKES_WITHIN);
        let pod = deployment.pod(&store);
        assert!(
            within!(TAKES_WITHIN, up(pod.surface).await),
            "the new pod never took shard 1"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
        let mut stream = plugin_stream(pod.surface).await;
        assert_eq!(
            next_want(&mut stream, until).await,
            Some((want.want_id.clone(), share_of(1, &names))),
            "shard 1's share was not served within {TAKES_WITHIN} s of the lapse"
        );
    });
}

/// Only a pod whose plugin hears wants listens for them at its take: a
/// data plugin's taker subscribes to the two want rows as it takes, before
/// its plugin opens any stream; a signal plugin's subscribes to neither.
#[test]
fn only_a_taker_whose_plugin_hears_wants_listens_for_them_at_its_take() {
    local(async {
        let deployment = deployment().await;
        for (role, listens) in [("signal", 0), ("data", 2)] {
            let store = Arc::new(super::Memory::default());
            let _a = holds(&store, 0, "a").await;
            let before = deployment.hearing_wants();
            let pod = deployment.pod_as(&store, role);
            assert!(
                within!(TAKES_WITHIN, up(pod.surface).await),
                "the {role} pod never took shard 1"
            );
            assert_eq!(
                deployment.hearing_wants() - before,
                listens,
                "a {role} plugin's taker"
            );
        }
    });
}
