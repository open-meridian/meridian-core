//! A shard's servings, each on a runtime of its own, driven whole against
//! a Lease API in memory and a bus in memory whose lake answers each take
//! (review c5a6 L2, L3, L4, M3): real ports, real time, short Leases.

use super::*;
use crate::lease::Wrote;
use crate::shards::tests::Memory;
use meridian_bus::MemoryBackend;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};
use std::sync::Mutex;
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::watch;

/// The process's one FENCED word is shared by these tests: one at a time.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

const RENEW: Duration = Duration::from_millis(200);
/// Below a second, so a Lease's record says 1 s: a lapse is 1 s.
const LASTS: Duration = Duration::from_millis(600);
const LEASE: &str = "feed-1-shard-0";
/// A topic every gate before c5a6 let through: a read.
const READ: &str = "platform.lake.query.read";

/// Whether `$ready` comes true within `$secs` seconds, asked every 25 ms.
macro_rules! within {
    ($secs:expr, $ready:expr) => {{
        let until = tokio::time::Instant::now() + Duration::from_secs_f64($secs);
        loop {
            if $ready {
                break true;
            }
            if tokio::time::Instant::now() >= until {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }};
}

fn free_port() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

async fn up(at: SocketAddr) -> bool {
    tokio::net::TcpStream::connect(at).await.is_ok()
}

/// A plugin's connection to the surface, as HTTP/2 opens one.
async fn plugin(at: SocketAddr) -> tokio::net::TcpStream {
    let mut stream = tokio::net::TcpStream::connect(at).await.unwrap();
    stream
        .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\x00\x00\x00\x04\x00\x00\x00\x00\x00")
        .await
        .unwrap();
    stream
}

/// Whether the other end closed `stream`: what it sent read past.
async fn closed(stream: &mut tokio::net::TcpStream) -> bool {
    let mut read = [0u8; 1024];
    loop {
        match tokio::time::timeout(Duration::from_millis(50), stream.read(&mut read)).await {
            Err(_) => return false,
            Ok(Ok(0)) | Ok(Err(_)) => return true,
            Ok(Ok(_)) => {}
        }
    }
}

/// Each serving's bus as the test makes it, each kept for a look after:
/// the lake answers each take with the next epoch, and says how it was.
#[derive(Default)]
struct Buses {
    made: Mutex<Vec<Arc<Bus>>>,
    /// Each take's holder and reason (`TakeoverReason`).
    takes: Arc<Mutex<Vec<(String, i32)>>>,
    /// The next this many makings never end, as a broker that does not
    /// answer.
    hang: AtomicU32,
    /// The next making fails, as a bus set up wrongly does.
    fail: AtomicBool,
}

impl Buses {
    fn made(&self, n: usize) -> Arc<Bus> {
        Arc::clone(&self.made.lock().unwrap()[n])
    }
    fn takes(&self) -> Vec<(String, i32)> {
        self.takes.lock().unwrap().clone()
    }
}

fn make(buses: &Arc<Buses>) -> MakeBus {
    let buses = Arc::clone(buses);
    Arc::new(move || {
        let buses = Arc::clone(&buses);
        Box::pin(async move {
            use prost::Message as _;
            if buses.fail.swap(false, SeqCst) {
                return Err("the broker's address is not one".to_string());
            }
            if buses
                .hang
                .fetch_update(SeqCst, SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                std::future::pending::<()>().await;
            }
            let bus = Arc::new(Bus::single(
                "feed-1",
                Arc::new(MemoryBackend::new()),
                Arc::new(meridian_clock::ManualClock::at(1)),
            ));
            let takes = Arc::clone(&buses.takes);
            bus.serve(meridian_lake::service::TAKE_SHARD, move |envelope| {
                let request =
                    meridian_domain::v1::TakeShardRequest::decode(&envelope.payload[..]).unwrap();
                let mut takes = takes.lock().unwrap();
                takes.push((request.holder, request.reason));
                Ok((
                    "meridian.v1.TakeShardReply".into(),
                    meridian_domain::v1::TakeShardReply {
                        epoch: takes.len() as u64,
                    }
                    .encode_to_vec(),
                ))
            });
            buses.made.lock().unwrap().push(Arc::clone(&bus));
            Ok(bus)
        })
    })
}

/// A data plugin's sidecar at `surface`, its front door at `door`.
fn serving(surface: SocketAddr, door: SocketAddr, buses: &Arc<Buses>) -> Arc<Serving> {
    let verifier = Arc::new(Verifier::new("feed-1", "/nonexistent/dashboard-keys"));
    Arc::new(Serving {
        deployment_id: "d-1".into(),
        identity: Identity::new("feed-1", vec!["data".to_string()]),
        verifier: Some(Arc::clone(&verifier)),
        front: Some((door, verifier, None)),
        listening: surface,
        instance_id: "feed-1".into(),
        limits: Limits::DEFAULT,
        write_share: None,
        declaration: None,
        bus: make(buses),
    })
}

/// One pod of the instance, its servings run until it ends.
struct Pod {
    store: Arc<Memory>,
    buses: Arc<Buses>,
    surface: SocketAddr,
    door: SocketAddr,
    ended: tokio::task::JoinHandle<Result<(), String>>,
    _stopper: watch::Sender<bool>,
}

fn pod(buses: Arc<Buses>) -> Pod {
    let store = Arc::new(Memory::default());
    // Made before the pod starts, as the launcher or the chart does (item 31).
    store.make(LEASE);
    let (surface, door) = (free_port(), free_port());
    let (stopper, stop) = watch::channel(false);
    let api = Arc::clone(&store);
    let ended = tokio::task::spawn_local(serve_as_shards(
        serving(surface, door, &buses),
        1,
        move || Ok(Arc::clone(&api)),
        (RENEW, LASTS),
        stop,
    ));
    Pod {
        store,
        buses,
        surface,
        door,
        ended,
        _stopper: stopper,
    }
}

impl Pod {
    async fn serving(&self) -> bool {
        up(self.surface).await && up(self.door).await
    }
    /// Its Lease written to name `holder`, as another pod's take or renewal
    /// does.
    async fn held_by(&self, holder: &str) {
        loop {
            let mut record = self.store.get(LEASE).await.unwrap().unwrap();
            if record.holder != holder {
                record.transitions += 1;
            }
            record.holder = holder.to_string();
            record.renew_time = crate::lease::micro_time(crate::now_ns());
            if matches!(self.store.update(LEASE, &record).await, Ok(Wrote::Done(_))) {
                return;
            }
        }
    }
    /// Another pod, `b`, holding the Lease and renewing it, until dropped.
    fn b_holds(self: &Arc<Self>) -> Renewing {
        let pod = Arc::clone(self);
        Renewing(tokio::task::spawn_local(async move {
            loop {
                pod.held_by("b").await;
                tokio::time::sleep(RENEW).await;
            }
        }))
    }
}

struct Renewing(tokio::task::JoinHandle<()>);

impl Drop for Renewing {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn local<F: Future>(test: F) -> F::Output {
    let _one = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|held| held.into_inner());
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(tokio::task::LocalSet::new().run_until(test))
}

/// Review c5a6 L2 and L3: each serving ends whole -- its plugin's
/// connection closed, its ports shut and bound again by the next, no task
/// left holding its bus, nothing more sent on it -- a refusal heard as
/// fenced ends that serving alone, and a surface that cannot be served
/// still ends the pod.
#[test]
fn a_shard_s_serving_ends_whole_and_the_next_serves_afresh() {
    local(async {
        let pod = Arc::new(pod(Arc::new(Buses::default())));
        assert!(
            within!(10.0, pod.serving().await),
            "the first serving never served"
        );
        let first = pod.buses.made(0);
        assert!(first.publish(READ, "t", Vec::new(), None, None).is_ok());
        let mut connection = plugin(pod.surface).await;
        assert!(!closed(&mut connection).await);

        // A store refuses a write as fenced: that serving ends.
        meridian_sidecar::shards::say_fenced("a test's refusal");
        assert!(
            within!(5.0, closed(&mut connection).await),
            "the plugin's connection outlived its serving"
        );
        assert!(
            first.publish(READ, "t", Vec::new(), None, None).is_err(),
            "a serving that ended still sends (review c5a6 L3)"
        );
        // The next serves afresh, on the same ports, unfenced.
        assert!(
            within!(
                10.0,
                pod.buses.made.lock().unwrap().len() == 2 && pod.serving().await
            ),
            "no second serving"
        );
        assert!(
            within!(5.0, Arc::strong_count(&first) == 2),
            "a task of the first serving still holds its bus ({} holders)",
            Arc::strong_count(&first)
        );
        tokio::time::sleep(LASTS * 2).await;
        assert_eq!(
            pod.buses.takes().len(),
            2,
            "the fence heard before ended the next serving"
        );
        assert!(!pod.ended.is_finished());
        let second = pod.buses.made(1);
        assert!(second.publish(READ, "t", Vec::new(), None, None).is_ok());

        // Another pod takes its Lease: that serving ends, its ports shut.
        let mut connection = plugin(pod.surface).await;
        let b = pod.b_holds();
        assert!(
            within!(
                5.0,
                closed(&mut connection).await && !up(pod.surface).await && !up(pod.door).await
            ),
            "the lost serving's ports are open"
        );
        assert!(second.publish(READ, "t", Vec::new(), None, None).is_err());
        assert!(within!(5.0, Arc::strong_count(&second) == 2));

        // Its surface's port taken as it takes the shard again: the pod ends.
        let taken = std::net::TcpListener::bind(pod.surface).unwrap();
        drop(b);
        pod.held_by("").await;
        assert!(
            within!(10.0, pod.ended.is_finished()),
            "the pod served on with no surface"
        );
        drop(taken);
    });
}

/// Review c5a6 L4: a serving that ends other than by a planned stop -- a
/// write refused as fenced, a surface that stopped -- gives its Lease up
/// where it still names it, so the next take need not wait out a lapse.
#[test]
fn a_serving_that_ends_unplanned_gives_its_lease_up() {
    local(async {
        let pod = Arc::new(pod(Arc::new(Buses::default())));
        assert!(
            within!(10.0, pod.serving().await),
            "the first serving never served"
        );
        meridian_sidecar::shards::say_fenced("a test's refusal");
        assert!(
            within!(10.0, pod.buses.takes().len() == 2),
            "no second take"
        );
        assert_eq!(
            pod.buses.takes()[1].1,
            meridian_domain::v1::TakeoverReason::Released as i32,
            "the fenced serving left its Lease to lapse: {:?}",
            pod.buses.takes()
        );
        // A surface that cannot be served: the pod ends, its Lease given up.
        let b = pod.b_holds();
        assert!(within!(5.0, !up(pod.surface).await));
        let taken = std::net::TcpListener::bind(pod.surface).unwrap();
        drop(b);
        pod.held_by("").await;
        assert!(
            within!(10.0, pod.ended.is_finished()),
            "the pod served on with no surface"
        );
        assert_eq!(pod.store.holder(LEASE), "", "its Lease was left to lapse");
        drop(taken);
    });
}

/// Review c5a6 M3: a broker that does not answer is an outage, never the
/// pod's own fault: however many takes in a row find no bus, the pod
/// stands by again each time and serves once the broker answers. A bus
/// set up wrongly -- an error from its setup -- ends it, its Lease given
/// up.
#[test]
fn a_broker_not_answering_never_ends_the_pod_and_a_bus_set_up_wrongly_does() {
    local(async {
        let buses = Arc::new(Buses::default());
        buses.hang.store(6, SeqCst);
        let pod = Arc::new(pod(buses));
        assert!(
            within!(20.0, pod.ended.is_finished() || pod.serving().await),
            "never served"
        );
        assert!(
            !pod.ended.is_finished(),
            "six takes with no broker ended the pod"
        );
        assert_eq!(pod.buses.hang.load(SeqCst), 0);
        // Lost, then a bus set up wrongly as it takes again.
        let b = pod.b_holds();
        assert!(within!(5.0, !up(pod.surface).await));
        pod.buses.fail.store(true, SeqCst);
        drop(b);
        pod.held_by("").await;
        assert!(
            within!(10.0, pod.ended.is_finished()),
            "a bus set up wrongly did not end the pod"
        );
        assert_eq!(pod.store.holder(LEASE), "", "its Lease was left to lapse");
    });
}
