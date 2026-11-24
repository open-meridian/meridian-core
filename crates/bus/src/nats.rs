//! The bus over a broker. Decision 010.
//!
//! Everything above [`Backend`] is unchanged: routing rules, correlation and
//! the topic grammar are the bus's, and this carries bytes between processes.
//! That is the bargain 010 struck — the contract stays ours and the transport
//! is somebody else's problem, solved by people who have clustered one for a
//! decade.
//!
//! # The grammar maps exactly
//!
//! Our `*` is one segment and NATS' `*` is one token; our `**` is a tail of
//! one or more and NATS' `>` is the same. Nothing here has to approximate a
//! pattern, which is the kind of translation that quietly delivers a message
//! nobody subscribed to.
//!
//! # At-most-once, still
//!
//! A publish hands the bytes to the client and does not wait for them to
//! leave. A subscriber whose queue is full loses the message, alone, exactly
//! as in memory: the newest is dropped and counted, on the subscription and
//! on the backend, and the broker's stream for that subscription keeps being
//! drained so nothing backs up behind it. What changes is that a message can also be lost between
//! processes, which is the same guarantee said out loud rather than a weaker
//! one.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use meridian_domain::v1::Envelope;
use prost::Message as _;
use tokio::sync::mpsc;
use tokio::sync::Notify;

use crate::backend::{Answer, Backend, BusError, Delivery, DropCount, Handler, Subscription};
use crate::topic;

/// Same bound as the in-memory backend, and for the same reason: a subscriber
/// that has stopped consuming is noticed rather than hidden behind a backlog.
const SUBSCRIBER_QUEUE: usize = 1024;

/// Where a refusal travels. Not in the payload, because the payload is the
/// answer's shape and a refusal is not an answer of that shape.
pub(crate) const REFUSAL_HEADER: &str = "Meridian-Refusal";

/// Words carried in a header, on one line: a header's value may hold no
/// line break, and the client panics on one, so the answer is never sent and
/// its caller waits out its whole timeout (step 3d; CI 43621875619, a
/// database's error with its DETAIL line). Each line break read as a space.
pub fn header_words(words: &str) -> String {
    words.replace("\r\n", " ").replace(['\r', '\n'], " ")
}

/// How long a request's caller waits for its answer, in milliseconds, carried
/// beside the request as gRPC's own `grpc-timeout` is: relative, so no two
/// clocks are compared. The answering side holds a read to its `read_after`
/// no longer than its caller waits (contract v30, ruled 2026-11-10).
pub(crate) const TIMEOUT_HEADER: &str = "Meridian-Timeout-Ms";

/// How often a client asks its server to hand a message back to it, and how
/// long it waits for it before moving to another server (step 3b): the
/// servers' 2 s rule for a client, said from the client's side. A server
/// cut off from its clients, or on a node that stopped, sends no reset and
/// no error, and the library's own ping waits a minute -- and is put off by
/// every message the client sends, so a client that publishes often never
/// pings at all. Meanwhile its subscriptions, a writer's change log among
/// them, are served by nobody. The round trip is on an inbox of its own,
/// under its credential's prefix ([`crate::inbox`]).
const HEARTBEAT: Duration = Duration::from_secs(1);
const UNANSWERED: Duration = Duration::from_secs(2);

/// How long [`Backend::subscribed`] waits for the subscriptions to be made,
/// and then for the broker's answer: a caller asks again.
const SUBSCRIBED_WITHIN: Duration = Duration::from_secs(5);

pub struct NatsBackend {
    client: async_nats::Client,

    /// The runtime this was built on, so a synchronous `publish` can hand work
    /// to it. Captured rather than created: two runtimes in one process is a
    /// deadlock waiting for an afternoon nobody has.
    handle: tokio::runtime::Handle,

    sequences: Mutex<HashMap<String, u64>>,

    /// Shared with every subscription's forwarding task, which is where a
    /// drop happens.
    dropped: Arc<AtomicU64>,

    /// What `publish` hands the broker, in the order it was published: one
    /// task takes each in turn, so a publisher's messages reach the broker,
    /// and so each subscriber, in the order they were sent. A batch of the
    /// lake's trades is heard in its sequence (contract v19), which a task
    /// per message did not keep.
    outbox: mpsc::UnboundedSender<(String, Vec<u8>)>,

    /// The client's heartbeat, stopped with it.
    heartbeat: tokio::task::AbortHandle,

    /// Every live subscription's drop count, each counted a loss when the
    /// connection is lost (the resilience spec's section 10a).
    listening: Arc<Listening>,

    /// The subscriptions asked for and not yet made at the broker: each is
    /// made on a task of its own, so `serve` and `subscribe` return before
    /// the broker has them ([`Backend::subscribed`]).
    pending: Arc<tokio::sync::watch::Sender<usize>>,

    /// The subscriptions asked for that were not made: the broker refused
    /// one, or its task never ran. None is ever made later, so a backend
    /// with any is never [`Backend::subscribed`] (stage T's review,
    /// finding 5).
    unmade: Arc<AtomicU64>,

    /// What this member has left to its group's other members
    /// ([`Backend::withdraw`]); every offer and shared subscription
    /// watches it.
    withdrawn: Arc<tokio::sync::watch::Sender<Option<crate::backend::Withdrawn>>>,

    /// The URL it connected by, for a connection of its own to each other
    /// server of the broker's cluster ([`Backend::subscribed`]).
    url: String,
}

/// One subscription being made at the broker, counted until it is made --
/// or counted unmade, where the broker refused it or its task was dropped
/// unrun.
struct Subscribing {
    pending: Arc<tokio::sync::watch::Sender<usize>>,
    unmade: Arc<AtomicU64>,
    made: bool,
}

impl Subscribing {
    fn begun(pending: &Arc<tokio::sync::watch::Sender<usize>>, unmade: &Arc<AtomicU64>) -> Self {
        pending.send_modify(|count| *count += 1);
        Self {
            pending: Arc::clone(pending),
            unmade: Arc::clone(unmade),
            made: false,
        }
    }

    /// The broker took it.
    fn made(mut self) {
        self.made = true;
    }
}

impl Drop for Subscribing {
    fn drop(&mut self) {
        // Counted unmade before it stops being pending: whoever waits for
        // none pending then reads the count.
        if !self.made {
            self.unmade.fetch_add(1, Ordering::SeqCst);
        }
        self.pending
            .send_modify(|count| *count = count.saturating_sub(1));
    }
}

/// Every live subscription's drop count, so a connection lost counts a loss
/// on each (the resilience spec's section 10a; decisions/024's at-most-once
/// deliveries). Whatever the broker held for them while the connection was
/// down -- a slow consumer cut off at `broker.limits.maxPending`, a server
/// lost -- is gone, and the library's reconnection and resubscription say
/// nothing of it. Counted, a sidecar puts a `Lost` in its plugin's stream
/// and the plugin reads again from the store, as after any loss.
#[derive(Default)]
struct Listening(Mutex<Vec<std::sync::Weak<AtomicU64>>>);

impl Listening {
    fn hold(&self, count: &DropCount) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(count.weak());
    }

    /// A loss counted on every subscription still live, those gone let go:
    /// how many it was counted on.
    fn lost(&self) -> u64 {
        let mut held = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        held.retain(|weak| match weak.upgrade() {
            Some(count) => {
                count.fetch_add(1, Ordering::Relaxed);
                true
            }
            None => false,
        });
        held.len() as u64
    }
}

impl Drop for NatsBackend {
    fn drop(&mut self) {
        self.heartbeat.abort();
    }
}

impl NatsBackend {
    /// How often its client hands itself a message by its server.
    pub const HEARTBEAT: Duration = HEARTBEAT;
    /// How long it waits for one before moving to another server.
    pub const UNANSWERED: Duration = UNANSWERED;

    /// Connect, or say why not.
    ///
    /// The credential is in the URL, because that is where a broker's client
    /// expects it and inventing a second way to pass one would be a second
    /// place for it to be wrong. What matters is where the URL comes from: a
    /// file mounted into this container alone, which the plugin beside it has
    /// no mount for. Containers in a pod share a network namespace and not a
    /// filesystem, and that is the whole of the boundary decision 010 rests
    /// on.
    ///
    /// Must be called from a Tokio runtime: the handle it captures is what
    /// lets the synchronous half of [`Backend`] reach an async client.
    pub async fn connect(url: &str) -> Result<Self, BusError> {
        let (options, address) = credential_in(url);
        let listening = Arc::new(Listening::default());
        let dropped = Arc::new(AtomicU64::new(0));
        let unmade = Arc::new(AtomicU64::new(0));
        let options = options.event_callback({
            let (listening, dropped) = (Arc::clone(&listening), Arc::clone(&dropped));
            let unmade = Arc::clone(&unmade);
            move |event| {
                let (listening, dropped) = (Arc::clone(&listening), Arc::clone(&dropped));
                let unmade = Arc::clone(&unmade);
                async move {
                    // A subscription the server refused -- this member's
                    // login may not subscribe there -- is said so here, and
                    // nowhere else: counted, so the member never says it
                    // serves a topic the broker does not hold for it.
                    if let async_nats::Event::ServerError(error) = &event {
                        if refused_a_subscription(error) {
                            unmade.fetch_add(1, Ordering::SeqCst);
                            tracing::error!(%error, "the broker refused a subscription");
                        }
                    }
                    if matches!(event, async_nats::Event::Disconnected) {
                        let lost = listening.lost();
                        dropped.fetch_add(lost, Ordering::Relaxed);
                        tracing::warn!(
                            subscriptions = lost,
                            "the connection to the broker was lost: what it held for each \
                             subscription meanwhile is counted lost, and read again"
                        );
                    }
                }
            }
        });
        let address = answering_first(url, address).await?;
        let client = options
            .retain_servers_order()
            .connect(&address)
            .await
            .map_err(|failed| BusError::HandlerFailed {
                topic: redacted(url),
                detail: failed.to_string(),
            })?;
        let heartbeat = tokio::spawn(heartbeat(client.clone())).abort_handle();

        let (outbox, mut sending) = mpsc::unbounded_channel::<(String, Vec<u8>)>();
        let publishing = client.clone();
        tokio::spawn(async move {
            while let Some((subject, bytes)) = sending.recv().await {
                if let Err(failed) = publishing.publish(subject.clone(), bytes.into()).await {
                    tracing::warn!(topic = subject, %failed, "the broker did not take a message");
                }
            }
        });
        Ok(Self {
            client,
            handle: tokio::runtime::Handle::current(),
            sequences: Mutex::new(HashMap::new()),
            dropped,
            outbox,
            heartbeat,
            listening,
            pending: Arc::new(tokio::sync::watch::channel(0).0),
            unmade,
            withdrawn: Arc::new(tokio::sync::watch::channel(None).0),
            url: url.to_string(),
        })
    }

    /// Give up this connection and make it again, as the heartbeat does on
    /// a server that stopped answering: every subscription is counted a
    /// loss, and resubscribed.
    pub async fn reconnect(&self) -> Result<(), BusError> {
        self.client
            .force_reconnect()
            .await
            .map_err(|failed| BusError::HandlerFailed {
                topic: "the broker".into(),
                detail: failed.to_string(),
            })
    }

    /// A client of its own on the broker the URL names, with its credential,
    /// for a component that speaks to JetStream itself (step 3b: the
    /// analytics units' work queue). Everything else goes by the bus.
    /// Its heartbeat runs as long as the process does.
    pub async fn client(url: &str) -> Result<async_nats::Client, BusError> {
        let (options, address) = credential_in(url);
        let address = answering_first(url, address).await?;
        let client = options
            .retain_servers_order()
            .connect(&address)
            .await
            .map_err(|failed| BusError::HandlerFailed {
                topic: redacted(url),
                detail: failed.to_string(),
            })?;
        tokio::spawn(heartbeat(client.clone()));
        Ok(client)
    }

    /// Our pattern as a NATS subject.
    ///
    /// Only the tail wildcard differs, and the grammar already restricts `**`
    /// to the tail, so this cannot produce a subject that means something else.
    pub fn subject(pattern: &str) -> String {
        if let Some(head) = pattern.strip_suffix(".**") {
            format!("{head}.>")
        } else if pattern == "**" {
            ">".to_string()
        } else {
            pattern.to_string()
        }
    }

    fn next_sequence(&self, topic: &str) -> u64 {
        let mut sequences = self.sequences.lock().expect("sequence lock poisoned");
        let entry = sequences.entry(topic.to_string()).or_insert(0);
        *entry += 1;
        *entry
    }
}

/// Hand a message to this client by its server every [`HEARTBEAT`]; where
/// none comes back within [`UNANSWERED`], move to another server. Ends when
/// the client does.
async fn heartbeat(client: async_nats::Client) {
    use async_nats::connection::State;
    use futures_util::StreamExt as _;

    let inbox = client.new_inbox();
    let Ok(mut back) = client.subscribe(inbox.clone()).await else {
        return;
    };
    let mut sent: u64 = 0;
    loop {
        tokio::time::sleep(HEARTBEAT).await;
        // Already moving: the library reconnects, and resubscribes this.
        if client.connection_state() != State::Connected {
            continue;
        }
        sent += 1;
        if client
            .publish(inbox.clone(), sent.to_be_bytes().to_vec().into())
            .await
            .is_err()
        {
            continue;
        }
        // This one back, past any a slow server handed back late.
        let heard = tokio::time::timeout(UNANSWERED, async {
            while let Some(message) = back.next().await {
                if message.payload.as_ref() == sent.to_be_bytes() {
                    return true;
                }
            }
            false
        })
        .await;
        match heard {
            Ok(true) => {}
            // The client is gone, and with it what this watched.
            Ok(false) => return,
            Err(_) if client.connection_state() == State::Connected => {
                tracing::warn!(
                    server = %client.server_info().server_name,
                    "the broker's server did not answer within {} s: moving to another",
                    UNANSWERED.as_secs()
                );
                if client.force_reconnect().await.is_err() {
                    return;
                }
            }
            Err(_) => {}
        }
    }
}

/// The internal CA's certificate, a PEM file, where the deployment's
/// internal TLS is on (the chart's `tls.internal.enabled`; ruling 1084).
/// Every client in the runtime reads it: the broker's here, the database's
/// and a front door's through meridian-changes' `tls`.
pub const INTERNAL_TLS_CA: &str = "MERIDIAN_INTERNAL_TLS_CA";

/// Split `nats://user:password@host` into what to present and where.
///
/// Taken from the URL rather than from a second setting, because that is where
/// a broker's client expects it and a second way to pass a credential is a
/// second place for it to be wrong. The client library does not read it from
/// there itself, which is the kind of thing that fails as an authentication
/// error naming no user at all.
///
/// A cluster of servers (step 3b, `broker.replicas: 3`) is every server, by
/// its name, after the one credential: `nats://user:password@a:4222,b:4222`,
/// each tried in turn and reconnected to when one is lost.
///
/// Its inboxes are made under its user's prefix, the only inboxes the broker
/// grants that user (v31a, item 2; [`crate::inbox`]).
fn credential_in(url: &str) -> (async_nats::ConnectOptions, Vec<String>) {
    let (scheme, rest) = url.split_once("://").unwrap_or(("nats", url));

    let (options, hosts): (async_nats::ConnectOptions, &str) = match rest.rsplit_once('@') {
        None => (async_nats::ConnectOptions::new(), rest),
        Some((credential, hosts)) => {
            let (user, password) = credential.split_once(':').unwrap_or((credential, ""));
            (
                async_nats::ConnectOptions::with_user_and_password(
                    user.to_string(),
                    password.to_string(),
                )
                .custom_inbox_prefix(crate::inbox::prefix(user)),
                hosts,
            )
        }
    };
    let servers = hosts
        .split(',')
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .map(|host| match host.split_once("://") {
            Some(_) => host.to_string(),
            None => format!("{scheme}://{host}"),
        })
        .collect();
    // A server that does not answer is given as long as the heartbeat
    // gives one, before the next is tried.
    let options = options.connection_timeout(UNANSWERED);
    (
        with_internal_tls(options, std::env::var(INTERNAL_TLS_CA).ok()),
        servers,
    )
}

/// The listed servers, those that answer first, so a connection is made to
/// a server that answers without first waiting [`UNANSWERED`] on one that
/// does not (the #147 known gap 2: a shard takeover with one server
/// unreachable waited on it once for each connection it made, in sequence,
/// past its 10 s).
///
/// Every listed server is asked at once, and one answers when it sends the
/// INFO line a broker's server sends every client first. Those that answer
/// together with the first, in any order, so clients spread across them;
/// then the rest, as listed, for the library to move to when one is lost.
/// A connection is made in this order ([`async_nats::ConnectOptions::
/// retain_servers_order`]), so a dead server costs it no wait at all, and a
/// server lost later costs a reconnection at most one. Where none answers
/// within [`UNANSWERED`], the connection is refused then, naming each.
/// One server is left to the library: there is nothing to put first.
async fn answering_first(url: &str, servers: Vec<String>) -> Result<Vec<String>, BusError> {
    use futures_util::stream::{FuturesUnordered, StreamExt as _};

    if servers.len() < 2 {
        return Ok(servers);
    }
    let mut asking: FuturesUnordered<_> = servers
        .iter()
        .enumerate()
        .map(|(at, server)| async move { (at, answers(server).await) })
        .collect();
    let mut until = tokio::time::Instant::now() + UNANSWERED;
    let mut answered = Vec::new();
    while let Ok(Some((at, said))) = tokio::time::timeout_at(until, asking.next()).await {
        if said {
            if answered.is_empty() {
                until = until.min(tokio::time::Instant::now() + ANSWERED_TOGETHER);
            }
            answered.push(at);
        }
    }
    if answered.is_empty() {
        return Err(BusError::HandlerFailed {
            topic: redacted(url),
            detail: format!(
                "no server of the broker answered within {} s",
                UNANSWERED.as_secs()
            ),
        });
    }
    shuffle(&mut answered);
    let rest = (0..servers.len()).filter(|at| !answered.contains(at));
    let order: Vec<usize> = answered.iter().copied().chain(rest).collect();
    Ok(order.into_iter().map(|at| servers[at].clone()).collect())
}

/// How long after the first server answers the others are taken as
/// answering with it: the servers of one cluster answer within a few
/// milliseconds of each other, and one that does not answer is waited on
/// no longer than this.
const ANSWERED_TOGETHER: Duration = Duration::from_millis(100);

/// Whether the server at `address` (`nats://host:port`) sends its INFO line
/// within [`UNANSWERED`]. Nothing is sent to it: the connection is closed
/// once it has said it.
async fn answers(address: &str) -> bool {
    use tokio::io::AsyncReadExt as _;

    let (_, rest) = address.split_once("://").unwrap_or(("nats", address));
    let host = rest.split('/').next().unwrap_or(rest);
    let host = if host
        .rsplit_once(':')
        .is_some_and(|(_, port)| port.parse::<u16>().is_ok())
    {
        host.to_string()
    } else {
        format!("{host}:4222")
    };
    let said = tokio::time::timeout(UNANSWERED, async {
        let mut socket = tokio::net::TcpStream::connect(host).await.ok()?;
        let mut first = [0u8; 4];
        socket.read_exact(&mut first).await.ok()?;
        Some(&first == b"INFO")
    })
    .await;
    matches!(said, Ok(Some(true)))
}

/// In place, in an order of the process's own: [`std::collections::hash_map::RandomState`]
/// is seeded anew each time it is made.
fn shuffle(items: &mut [usize]) {
    use std::hash::{BuildHasher as _, Hasher as _};

    for last in (1..items.len()).rev() {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_usize(last);
        let pick = (hasher.finish() % (last as u64 + 1)) as usize;
        items.swap(last, pick);
    }
}

/// Over TLS, and only over TLS, where the deployment's internal TLS is on:
/// each server verified against the internal CA by the name it was given.
/// Only those names: a cluster's servers also tell a client each other's
/// addresses, by IP, which no certificate names, and every server is named
/// in the URL already.
fn with_internal_tls(
    options: async_nats::ConnectOptions,
    ca: Option<String>,
) -> async_nats::ConnectOptions {
    match ca.filter(|path| !path.trim().is_empty()) {
        None => options,
        Some(ca) => options
            .require_tls(true)
            .add_root_certificates(std::path::PathBuf::from(ca))
            .ignore_discovered_servers(),
    }
}

/// The address without its credential, for anything anybody will read.
fn redacted(url: &str) -> String {
    let (_, servers) = credential_in(url);
    servers.join(",")
}

impl Backend for NatsBackend {
    /// Publish, without waiting for the broker.
    ///
    /// The sequence returned is this publisher's own count for the topic, not
    /// a position in a broker-wide order: core NATS has no such number, and
    /// inventing one here would be a lie a reader could act on. A total order
    /// is a JetStream property and arrives with the workflow that needs it.
    fn publish(&self, topic: &str, envelope: Envelope) -> Result<u64, BusError> {
        if !topic::is_publishable(topic) {
            return Err(BusError::NotPublishable(topic.to_string()));
        }

        let sequence = self.next_sequence(topic);
        let bytes = envelope.encode_to_vec();

        // Queued rather than awaited: publish is synchronous above this
        // trait, and blocking a caller on a broker round trip would make every
        // publisher wait for the slowest link in the deployment. One queue,
        // taken in order, so what is published in order arrives in order.
        if self.outbox.send((topic.to_string(), bytes)).is_err() {
            tracing::warn!(
                topic,
                "the broker's publishing task has stopped; a message was not sent"
            );
        }

        Ok(sequence)
    }

    fn subscribe(&self, pattern: &str) -> Subscription {
        self.listen(pattern, None)
    }

    /// Ask whoever serves this topic, wherever they are.
    ///
    /// The broker knows whether anybody is listening, which is what keeps
    /// "nobody serves this" apart from "nobody answered in time". Those are
    /// different problems to whoever is holding a pager: one is a deployment
    /// missing a component, the other is a component that is too slow or gone.
    fn request<'a>(&'a self, topic: &'a str, envelope: Envelope, timeout: Duration) -> Answer<'a> {
        self.ask(topic, envelope, timeout, None)
    }

    /// The same, addressed to writer group `group` (step 3d, approved
    /// 2026-11-12): [`crate::owners::WRITER_GROUP_HEADER`] beside the
    /// timeout, so every other group's member leaves it unanswered.
    fn request_to_group<'a>(
        &'a self,
        topic: &'a str,
        envelope: Envelope,
        timeout: Duration,
        group: u32,
    ) -> Answer<'a> {
        self.ask(topic, envelope, timeout, Some(group))
    }

    /// Offer a local handler's answer to writer group `group` alone.
    fn serve_group(&self, topic: &str, queue: &str, group: u32, handler: Handler) {
        self.offer(topic, queue, handler, None, Some(group), None);
    }
    /// Offer a local handler's answer to callers in other processes.
    fn serve(&self, topic: &str, group: &str, handler: Handler) {
        self.offer(topic, group, handler, None, None, None);
    }

    /// The same, each request another writer group owns passed to it first.
    fn serve_routed(
        &self,
        topic: &str,
        group: &str,
        handler: Handler,
        route: crate::backend::PreRoute,
    ) {
        self.offer(topic, group, handler, None, None, Some(route));
    }

    /// The same, signalling once each answer is on the wire.
    fn serve_delivered(&self, topic: &str, group: &str, handler: Handler, delivered: Arc<Notify>) {
        self.offer(topic, group, handler, Some(delivered), None, None);
    }

    /// One of a queue group at the broker (contract v25): each message to one
    /// member of the group.
    fn subscribe_shared(&self, pattern: &str, group: &str) -> Subscription {
        self.listen(pattern, Some(group.to_string()))
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn withdraw(&self, what: crate::backend::Withdrawn) {
        self.withdrawn.send_replace(Some(what));
    }

    /// Every subscription asked for so far made at the broker, and every
    /// server of the broker shown to hold them.
    ///
    /// **Made.** Each is sent on this connection in order. One the broker
    /// refused, or whose task never ran, is never made later: with any, this
    /// is refused for good, and the member never says it serves (stage T's
    /// review, finding 5).
    ///
    /// **Held by the server it is connected to.** A message of this
    /// client's own, published after them to a subject subscribed after
    /// them and heard back, went through that server after it took every
    /// one.
    ///
    /// **Held by every other server** (finding 5: a caller connected to
    /// another server of a three-server broker was told nothing serves
    /// while its server had not yet heard of the subscriptions). NATS
    /// gives a subscriber no acknowledgement that interest has reached the
    /// other servers of a cluster, so it is shown by a round trip through
    /// each: on a connection of its own to each server the broker
    /// advertises (`connect_urls` in its INFO), the same subject is asked,
    /// and this client's subscription answers. A server routes a request
    /// only once it holds the interest, and answers "no responders" at
    /// once where it does not; and a server sends its clients' interest to
    /// each other server on one route, in the order it took it, so the
    /// last subscription heard there says every one before it is. A server
    /// that cannot be reached routes nobody's call either, and is passed
    /// over; one that is reached and does not hold the subscription yet
    /// refuses this, and the member asks again.
    fn subscribed(&self) -> crate::backend::Settled<'_> {
        Box::pin(async move {
            let unsettled = |detail: String| BusError::HandlerFailed {
                topic: "the broker".into(),
                detail,
            };
            let mut pending = self.pending.subscribe();
            tokio::time::timeout(SUBSCRIBED_WITHIN, pending.wait_for(|count| *count == 0))
                .await
                .map_err(|_| unsettled("its subscriptions were not made in time".into()))?
                .map_err(|_| unsettled("the connection is gone".into()))?;
            let inbox = self.client.new_inbox();
            let mut back = self
                .client
                .subscribe(inbox.clone())
                .await
                .map_err(|failed| unsettled(failed.to_string()))?;
            self.client
                .publish(inbox.clone(), Vec::new().into())
                .await
                .map_err(|failed| unsettled(failed.to_string()))?;
            self.client
                .flush()
                .await
                .map_err(|failed| unsettled(failed.to_string()))?;
            match tokio::time::timeout(SUBSCRIBED_WITHIN, futures_util::StreamExt::next(&mut back))
                .await
            {
                Ok(Some(_)) => {}
                Ok(None) => return Err(unsettled("the connection is gone".into())),
                Err(_) => {
                    return Err(unsettled(
                        "the broker did not answer once the subscriptions were sent".into(),
                    ))
                }
            }
            // A subscription the server refused is said after the fact, on
            // the client's events, which a task of its own reads: given a
            // moment to be counted. The server said it before it sent the
            // message heard above.
            tokio::time::sleep(REFUSALS_COUNTED_WITHIN).await;
            let unmade = self.unmade.load(Ordering::SeqCst);
            if unmade > 0 {
                return Err(unsettled(format!(
                    "{unmade} of its subscriptions could not be made at the broker, and are \
                     never made later: this member does not serve"
                )));
            }
            self.held_by_every_server(&inbox, &mut back).await
        })
    }
}

/// How long a refused subscription is given to be counted, once the broker
/// has answered past it.
const REFUSALS_COUNTED_WITHIN: Duration = Duration::from_millis(50);

/// Whether the broker's server refused a subscription, by what it said: its
/// `-ERR 'Permissions Violation for Subscription to ...'`.
fn refused_a_subscription(error: &async_nats::ServerError) -> bool {
    let said = error.to_string().to_lowercase();
    said.contains("permissions violation") && said.contains("subscription")
}

impl NatsBackend {
    /// `probe`, which this client is subscribed to as `back`, asked through
    /// every server the broker advertises, each on a connection of its own,
    /// and answered here ([`Backend::subscribed`]).
    async fn held_by_every_server(
        &self,
        probe: &str,
        back: &mut async_nats::Subscriber,
    ) -> Result<(), BusError> {
        let info = self.client.server_info();
        let (_, first) = credential_in(&self.url);
        let scheme = first
            .first()
            .and_then(|address| address.split_once("://"))
            .map_or("nats", |(scheme, _)| scheme)
            .to_string();
        for advertised in &info.connect_urls {
            let address = match advertised.split_once("://") {
                Some(_) => advertised.clone(),
                None => format!("{scheme}://{advertised}"),
            };
            let (options, _) = credential_in(&self.url);
            let other = match options
                .ignore_discovered_servers()
                .connection_timeout(OTHER_SERVER_WITHIN)
                .connect(&address)
                .await
            {
                Ok(other) => other,
                Err(failed) => {
                    tracing::debug!(%failed, "a server of the broker was not reached: passed over");
                    continue;
                }
            };
            let server = other.server_info().server_name;
            if other.server_info().server_id == info.server_id {
                continue;
            }
            SERVERS_ASKED.fetch_add(1, Ordering::Relaxed);
            let asked = other.send_request(
                probe.to_string(),
                async_nats::Request::new()
                    .payload(Vec::new().into())
                    .timeout(Some(SUBSCRIBED_WITHIN)),
            );
            tokio::pin!(asked);
            let answered = loop {
                tokio::select! {
                    answered = &mut asked => break answered,
                    Some(message) = futures_util::StreamExt::next(back) => {
                        if let Some(reply) = message.reply {
                            let _ = self.client.publish(reply, Vec::new().into()).await;
                        }
                    }
                }
            };
            if let Err(failed) = answered {
                return Err(BusError::HandlerFailed {
                    topic: "the broker".into(),
                    detail: format!(
                        "its server {server} does not hold this member's subscriptions yet ({failed})"
                    ),
                });
            }
        }
        Ok(())
    }
}

/// The other servers asked so far, by every backend of this process.
static SERVERS_ASKED: AtomicU64 = AtomicU64::new(0);

/// How many times another server of the broker's cluster was asked whether
/// it holds a member's subscriptions ([`Backend::subscribed`]): what a test
/// reads to see the round trip went through each.
pub fn servers_asked() -> u64 {
    SERVERS_ASKED.load(Ordering::Relaxed)
}

/// How long another server of the broker is given to take a connection and
/// answer, before it is passed over as one that routes nobody's call.
const OTHER_SERVER_WITHIN: Duration = Duration::from_secs(2);

/// End a subscription at the broker, and have the broker know it before
/// anything more is done: what it had already sent is still read.
async fn leave(client: &async_nats::Client, subscription: &mut async_nats::Subscriber, what: &str) {
    if let Err(failed) = subscription.unsubscribe().await {
        tracing::warn!(topic = what, %failed, "could not leave a subscription at the broker");
        return;
    }
    if let Err(failed) = client.flush().await {
        tracing::warn!(topic = what, %failed, "the broker was not told a subscription was left");
    }
    tracing::info!(
        topic = what,
        "left to this member's group: no longer offered here"
    );
}

impl NatsBackend {
    /// A subscription at the broker: plain, every subscriber hearing every
    /// message, or one of a queue group (contract v25).
    fn listen(&self, pattern: &str, group: Option<String>) -> Subscription {
        let (tx, rx) = mpsc::channel(SUBSCRIBER_QUEUE);
        let subject = Self::subject(pattern);
        let client = self.client.clone();
        let owned = pattern.to_string();
        let dropped = DropCount::default();
        self.listening.hold(&dropped);
        let counted = dropped.clone();
        let total = Arc::clone(&self.dropped);
        let subscribing = Subscribing::begun(&self.pending, &self.unmade);
        let shared = group.is_some();
        let mut withdrawing = self.withdrawn.subscribe();

        self.handle.spawn(async move {
            let subscribed = match group {
                None => client.subscribe(subject.clone()).await,
                Some(group) => client.queue_subscribe(subject.clone(), group).await,
            };
            let mut messages = match subscribed {
                Ok(messages) => {
                    subscribing.made();
                    messages
                }
                Err(failed) => {
                    tracing::error!(pattern = owned, %failed, "could not subscribe at the broker");
                    return;
                }
            };

            let lose = || {
                counted.add();
                total.fetch_add(1, Ordering::Relaxed);
            };

            let mut sequence = 0u64;
            // A shared subscription ends where this member leaves its
            // shared work to its group's others ([`Backend::withdraw`]).
            let mut watching = shared;
            loop {
                let message = tokio::select! {
                    biased;
                    told = withdrawing.changed(), if watching => {
                        let leaves = withdrawing
                            .borrow_and_update()
                            .as_ref()
                            .is_some_and(|what| what.shared_work);
                        if told.is_err() {
                            watching = false;
                        } else if leaves {
                            watching = false;
                            leave(&client, &mut messages, &owned).await;
                        }
                        continue;
                    }
                    message = futures_util::StreamExt::next(&mut messages) => message,
                };
                let Some(message) = message else {
                    break;
                };
                let envelope = match Envelope::decode(message.payload) {
                    Ok(envelope) => envelope,
                    Err(failed) => {
                        // Something on our subjects that is not ours. Dropped
                        // and counted rather than passed up: a consumer cannot
                        // do anything with bytes that are not an envelope.
                        lose();
                        tracing::warn!(pattern = owned, %failed, "undecodable message");
                        continue;
                    }
                };

                sequence += 1;
                // Never waits for room. Waiting here backed the broker's
                // stream up behind one slow consumer and lost nothing that
                // anyone could count; dropping loses this message for this
                // subscriber alone, and says so.
                match tx.try_send(Delivery { envelope, sequence }) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        lose();
                        tracing::warn!(pattern = owned, "subscriber queue full; message dropped");
                    }
                    // The subscription was dropped. Ending the task is what
                    // unsubscribes at the broker, since the client goes with it.
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                }
            }
        });

        Subscription::new(rx, pattern, dropped)
    }
}

impl NatsBackend {
    /// Ask whoever serves `topic`, addressed to a writer group where
    /// `group` names one.
    fn ask<'a>(
        &'a self,
        topic: &'a str,
        envelope: Envelope,
        timeout: Duration,
        group: Option<u32>,
    ) -> Answer<'a> {
        let client = self.client.clone();
        let subject = topic.to_string();

        Box::pin(async move {
            if !topic::is_publishable(&subject) {
                return Err(BusError::NotPublishable(subject));
            }

            // A question past what the broker carries is refused here,
            // naming its size, rather than by the client's own words.
            let question = envelope.encode_to_vec();
            let most = client.server_info().max_payload;
            if most > 0 && question.len() > most {
                return Err(BusError::TooLarge {
                    topic: subject,
                    size: question.len(),
                    most,
                });
            }

            // The caller's timeout, on the request itself. The client has one
            // of its own, ten seconds unless told otherwise, and it fired
            // first and was reported as this one: every call on this bus was
            // ten seconds at most whatever it asked for, which a first-run
            // Apply on a fresh cluster outlasts.
            let asked = tokio::time::timeout(
                timeout,
                client.send_request(
                    subject.clone(),
                    async_nats::Request::new()
                        .payload(question.into())
                        .headers({
                            let mut headers = async_nats::HeaderMap::new();
                            headers
                                .insert(TIMEOUT_HEADER, timeout.as_millis().to_string().as_str());
                            if let Some(group) = group {
                                headers.insert(
                                    crate::owners::WRITER_GROUP_HEADER,
                                    group.to_string().as_str(),
                                );
                            }
                            headers
                        })
                        .timeout(Some(timeout)),
                ),
            )
            .await;

            let message = match asked {
                Err(_) => {
                    return Err(BusError::Timeout {
                        topic: subject,
                        timeout_ms: timeout.as_millis() as u64,
                    })
                }
                Ok(Err(failed)) => {
                    return Err(match failed.kind() {
                        async_nats::RequestErrorKind::NoResponders => BusError::NoHandler(subject),
                        async_nats::RequestErrorKind::TimedOut => BusError::Timeout {
                            topic: subject,
                            timeout_ms: timeout.as_millis() as u64,
                        },
                        _ => BusError::HandlerFailed {
                            topic: subject,
                            detail: failed.to_string(),
                        },
                    })
                }
                Ok(Ok(message)) => message,
            };

            // A handler that refused says so in a header rather than in the
            // payload: the payload is the answer's shape, and an error is not
            // an answer of that shape.
            if let Some(headers) = &message.headers {
                if let Some(detail) = headers.get(REFUSAL_HEADER) {
                    return Err(BusError::HandlerFailed {
                        topic: subject,
                        detail: detail.to_string(),
                    });
                }
            }

            Envelope::decode(message.payload).map_err(|failed| BusError::HandlerFailed {
                topic: subject,
                detail: format!("the answer was not an envelope: {failed}"),
            })
        })
    }

    fn offer(
        &self,
        topic: &str,
        group: &str,
        handler: Handler,
        delivered: Option<Arc<Notify>>,
        writer_group: Option<u32>,
        route: Option<crate::backend::PreRoute>,
    ) {
        let client = self.client.clone();
        let subject = topic.to_string();
        let group = group.to_string();
        let subscribing = Subscribing::begun(&self.pending, &self.unmade);
        let mut withdrawing = self.withdrawn.subscribe();

        self.handle.spawn(async move {
            // One of the component's queue group (contract v25): each request
            // reaches one copy of the component, which answers it.
            let subscribed = client.queue_subscribe(subject.clone(), group).await;
            let mut requests = match subscribed {
                Ok(requests) => {
                    subscribing.made();
                    requests
                }
                Err(failed) => {
                    tracing::error!(topic = subject, %failed, "could not offer a handler");
                    return;
                }
            };

            // No longer offered once this member leaves the topic to its
            // group's others ([`Backend::withdraw`]): what had already
            // arrived is still answered.
            let mut watching = true;
            loop {
                let message = tokio::select! {
                    biased;
                    told = withdrawing.changed(), if watching => {
                        let leaves = withdrawing
                            .borrow_and_update()
                            .as_ref()
                            .is_some_and(|what| (what.topics)(&subject));
                        if told.is_err() {
                            watching = false;
                        } else if leaves {
                            watching = false;
                            leave(&client, &mut requests, &subject).await;
                        }
                        continue;
                    }
                    message = futures_util::StreamExt::next(&mut requests) => message,
                };
                let Some(message) = message else {
                    break;
                };
                // A publish on a topic somebody also serves: there is nobody
                // to answer, so there is nothing to do with it here.
                let Some(reply_to) = message.reply.clone() else {
                    continue;
                };

                // A hand-off addressed to another writer group (step 3d):
                // its own member answers it, so this one leaves it be.
                if let Some(mine) = writer_group {
                    let addressed = message
                        .headers
                        .as_ref()
                        .and_then(|headers| headers.get(crate::owners::WRITER_GROUP_HEADER))
                        .and_then(|said| said.to_string().parse::<u32>().ok());
                    if addressed != Some(mine) {
                        continue;
                    }
                }

                // When its caller stops waiting, from what it said as it
                // asked, counted from now (contract v30, ruled 2026-11-10).
                let deadline = message
                    .headers
                    .as_ref()
                    .and_then(|headers| headers.get(TIMEOUT_HEADER))
                    .and_then(|said| said.to_string().parse::<u64>().ok())
                    .map(|ms| std::time::Instant::now() + Duration::from_millis(ms));

                let passed = message.payload.clone();
                let asked = match Envelope::decode(message.payload) {
                    Ok(envelope) => envelope,
                    Err(failed) => {
                        tracing::warn!(topic = subject, %failed, "undecodable request");
                        continue;
                    }
                };

                let handler = Arc::clone(&handler);
                let client = client.clone();
                let topic = subject.clone();
                let delivered = delivered.clone();
                let route = route.clone();

                // Each request on its own task, so one slow handler does not
                // hold up the next, and on a blocking pool, because a handler
                // that blocks would otherwise stall unrelated work. Admitted
                // first (the resilience spec's section 10a): its turn among
                // its class's callers at this process's gate -- at most
                // HANDLERS_AT_ONCE answering at once -- and its working memory
                // reserved; or refused for load with a retry-after.
                tokio::spawn(async move {
                    // Another writer group's write, as the request says (step
                    // 3d; section 6.7): passed to its owner as it came, its
                    // caller's reply address kept and what is left of its
                    // wait, so the owner answers the caller; nothing held or
                    // waited on here. Decided off the runtime: the owner's
                    // record may be read.
                    if let Some(route) = route {
                        let started = std::time::Instant::now();
                        let looked = asked.clone();
                        let deciding = route.clone();
                        let owner = tokio::task::spawn_blocking(move || deciding.owner(&looked))
                            .await
                            .ok()
                            .flatten();
                        // A wait spent while it was decided: dropped here,
                        // below, as any request whose caller stopped waiting
                        // -- never passed on to be run late. A fence that
                        // landed meanwhile: refused here, below, by its
                        // handler, never passed on (review-c5n5 F2).
                        let spent = deadline
                            .is_some_and(|deadline| std::time::Instant::now() >= deadline);
                        if let Some((internal, group)) =
                            owner.filter(|_| !spent && !route.refuses())
                        {
                            let mut headers = async_nats::HeaderMap::new();
                            headers.insert(
                                crate::owners::WRITER_GROUP_HEADER,
                                group.to_string().as_str(),
                            );
                            if let Some(deadline) = deadline {
                                let left = deadline
                                    .saturating_duration_since(std::time::Instant::now())
                                    .as_millis()
                                    .max(1);
                                // What is left of it, the owner's to keep.
                                headers.insert(TIMEOUT_HEADER, left.to_string().as_str());
                            }
                            match client
                                .publish_with_reply_and_headers(
                                    internal,
                                    reply_to.clone(),
                                    headers,
                                    passed,
                                )
                                .await
                            {
                                Ok(()) => {
                                    crate::owners::note_passed(started.elapsed());
                                    return;
                                }
                                Err(failed) => {
                                    // Answered here instead: its handler
                                    // hands it on as before.
                                    tracing::warn!(topic, %failed, "a write could not be passed to its owner");
                                }
                            }
                        }
                    }
                    // INTERIM(v32): who asked, by the inbox its answer goes
                    // to, which only its credential hears; empty where that
                    // inbox is no user's (v31a, item 3).
                    let asker = Some(
                        crate::inbox::owner(&reply_to)
                            .unwrap_or_default()
                            .to_string(),
                    );
                    let answered =
                        crate::router::admitted(&topic, asked, handler, deadline, asker).await;

                    let (payload_type, payload, refusal) = match answered {
                        Ok(Ok((payload_type, payload))) => (payload_type, payload, None),
                        Ok(Err(detail)) => (String::new(), Vec::new(), Some(detail)),
                        Err(joined) => (
                            String::new(),
                            Vec::new(),
                            Some(format!("handler panicked: {joined}")),
                        ),
                    };

                    let reply = Envelope {
                        meta: None,
                        payload_type,
                        payload,
                    };
                    let encoded = reply.encode_to_vec();

                    // An answer past what the broker carries is refused here,
                    // naming the topic and the size: sent, the broker drops
                    // it and the caller waits out its whole timeout for an
                    // answer that never comes (found on dev, 2026-10-26, the
                    // plugin catalogue at 1,050,251 bytes).
                    let (encoded, refusal) = match refusal {
                        None => match past_the_broker(
                            &topic,
                            encoded.len(),
                            client.server_info().max_payload,
                        ) {
                            Some(detail) => {
                                tracing::error!(topic, "{detail}");
                                let empty = Envelope {
                                    meta: None,
                                    payload_type: String::new(),
                                    payload: Vec::new(),
                                };
                                (empty.encode_to_vec(), Some(detail))
                            }
                            None => (encoded, None),
                        },
                        refused => (encoded, refused),
                    };

                    let sent = match refusal {
                        None => client.publish(reply_to, encoded.into()).await,
                        Some(detail) => {
                            let mut headers = async_nats::HeaderMap::new();
                            headers.insert(REFUSAL_HEADER, header_words(&detail).as_str());
                            client
                                .publish_with_headers(reply_to, headers, encoded.into())
                                .await
                        }
                    };

                    if let Err(failed) = sent {
                        // The caller will time out, which is the honest
                        // outcome: we have no way to tell them from here.
                        tracing::warn!(topic, %failed, "an answer did not reach the broker");
                        return;
                    }

                    // Queued is not sent. `publish` returns once the answer is
                    // in this client's write buffer, and a process that exits
                    // promptly afterwards takes the buffer with it. A
                    // long-lived component never notices, because the client
                    // drains in the background; a one-shot Job that replies
                    // and stops does, and its caller then waits out the whole
                    // timeout for an answer that was never written. Seen under
                    // load on 2026-09-23, reported to the operator as a first
                    // run that failed after it had entirely succeeded.
                    if let Err(failed) = client.flush().await {
                        tracing::warn!(topic, %failed, "an answer was not flushed to the broker");
                        return;
                    }

                    if let Some(delivered) = delivered {
                        delivered.notify_one();
                    }
                });
            }
        });
    }
}

/// Why an answer of `size` bytes is not sent, where the broker carries at
/// most `most` (its `max_payload`, 1 MiB unless its configuration says
/// otherwise): the topic, the size and the limit, for the caller and the
/// log. None where it fits, and where the broker states no limit.
///
/// NATS keeps messages small by design, and the limit is not raised to make
/// an answer fit: an answer that grows with a deployment's data pages, or
/// answers only what its caller needs.
pub(crate) fn past_the_broker(topic: &str, size: usize, most: usize) -> Option<String> {
    (most > 0 && size > most).then(|| {
        format!(
            "the answer to `{topic}` is {size} bytes, past the broker's limit of {most} bytes, \
             and was not sent"
        )
    })
}

#[cfg(test)]
mod size_tests {
    use super::*;

    #[test]
    fn an_answer_past_the_limit_is_named_by_topic_and_size() {
        let most = 1_048_576;
        assert_eq!(past_the_broker("platform.x.query.y", most, most), None);
        let said = past_the_broker("platform.config.query.plugin-catalogue", 1_050_251, most)
            .expect("past the limit");
        assert!(
            said.contains("`platform.config.query.plugin-catalogue`"),
            "{said}"
        );
        assert!(said.contains("1050251 bytes"), "{said}");
        assert!(said.contains("limit of 1048576 bytes"), "{said}");
        // A broker that states no limit is not second-guessed.
        assert_eq!(past_the_broker("platform.x.query.y", 5, 0), None);
    }
}

#[cfg(test)]
mod bounded_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A burst of requests: never more than the permits answering at once,
    /// and every one answered, admitted at the gate as the broker's are
    /// (the resilience spec's section 10a): one caller's burst within its
    /// share waits its turn.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_burst_is_answered_at_most_handlers_at_once_at_a_time() {
        let now = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for n in 0..(meridian_budget::HANDLERS_AT_ONCE * 5) {
            let (now, most) = (now.clone(), most.clone());
            let handler: Handler = Arc::new(move |_| {
                let at = now.fetch_add(1, Ordering::SeqCst) + 1;
                most.fetch_max(at, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(20));
                now.fetch_sub(1, Ordering::SeqCst);
                Ok((n.to_string(), Vec::new()))
            });
            let asked = Envelope {
                meta: None,
                payload_type: String::new(),
                payload: Vec::new(),
            };
            tasks.push(tokio::spawn(async move {
                crate::router::admitted(
                    "platform.lake.query.list-quotes",
                    asked,
                    handler,
                    None,
                    None,
                )
                .await
                .unwrap()
                .unwrap()
                .0
                .parse::<usize>()
                .unwrap()
            }));
        }
        let mut answered = Vec::new();
        for task in tasks {
            answered.push(task.await.unwrap());
        }
        answered.sort_unstable();
        assert_eq!(
            answered,
            (0..meridian_budget::HANDLERS_AT_ONCE * 5).collect::<Vec<_>>()
        );
        let most = most.load(Ordering::SeqCst);
        assert!(
            most <= meridian_budget::HANDLERS_AT_ONCE,
            "{most} answered at once"
        );
        assert!(most > 1, "answered concurrently all the same: {most}");
    }
}

#[cfg(test)]
mod address_tests {
    use super::*;

    #[test]
    fn one_server_or_every_server_of_a_cluster_with_one_credential() {
        assert_eq!(
            credential_in("nats://lake:pw@broker.ns.svc:4222").1,
            vec!["nats://broker.ns.svc:4222".to_string()]
        );
        assert_eq!(
            credential_in("nats://lake:pw@b-0.m:4222,b-1.m:4222,b-2.m:4222").1,
            vec![
                "nats://b-0.m:4222".to_string(),
                "nats://b-1.m:4222".to_string(),
                "nats://b-2.m:4222".to_string()
            ]
        );
        assert_eq!(
            credential_in("nats://127.0.0.1:4222").1,
            vec!["nats://127.0.0.1:4222".to_string()]
        );
        // Never the credential, in anything said.
        assert_eq!(
            redacted("nats://lake:pw@b-0.m:4222,b-1.m:4222"),
            "nats://b-0.m:4222,nats://b-1.m:4222"
        );
    }
}

#[cfg(test)]
mod dead_server_tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

    /// A server that takes a connection and never says a word: a broker
    /// server on a node that stopped, or a process stopped where its port
    /// is still open. Every connection is held, never answered.
    async fn black_holed() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        port
    }

    /// Enough of a broker server for a client to connect: its INFO, then a
    /// PONG for every PING.
    async fn answering() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let (read, mut write) = socket.into_split();
                    let info = format!(
                        "INFO {{\"server_id\":\"live\",\"server_name\":\"live\",\"version\":\"2.10.29\",\
                         \"host\":\"127.0.0.1\",\"port\":{port},\"headers\":true,\"max_payload\":1048576,\
                         \"proto\":1}}\r\n"
                    );
                    if write.write_all(info.as_bytes()).await.is_err() {
                        return;
                    }
                    let mut lines = tokio::io::BufReader::new(read).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        if line.starts_with("PING") && write.write_all(b"PONG\r\n").await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        port
    }

    /// The #147 known gap 2: a shard takeover with one broker server
    /// unreachable waited [`UNANSWERED`] on the dead address once for each
    /// connection it made, in sequence. Listed first, the dead server costs
    /// no connection a wait: every one connects well inside it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dead_server_listed_first_costs_no_connection_a_wait() {
        let (dead, live) = (black_holed().await, answering().await);
        let url = format!("nats://b4r:pw@127.0.0.1:{dead},127.0.0.1:{live}");
        let within = UNANSWERED / 2;
        for attempt in 0..8 {
            let started = std::time::Instant::now();
            let client = tokio::time::timeout(UNANSWERED * 3, NatsBackend::client(&url))
                .await
                .expect("connected within three waits")
                .expect("connected");
            let took = started.elapsed();
            assert!(
                took < within,
                "connection {attempt} took {took:?}, waiting on the dead server; the bound is {within:?}"
            );
            assert_eq!(client.server_info().server_id, "live");
            let started = std::time::Instant::now();
            let backend = NatsBackend::connect(&url).await.expect("connected");
            let took = started.elapsed();
            assert!(
                took < within,
                "backend {attempt} took {took:?}, waiting on the dead server; the bound is {within:?}"
            );
            drop(backend);
        }
    }

    /// Every server dead: refused within one wait, naming each address and
    /// never the credential.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_server_dead_is_refused_within_one_wait() {
        let (a, b) = (black_holed().await, black_holed().await);
        let url = format!("nats://b4r:pw@127.0.0.1:{a},127.0.0.1:{b}");
        let started = std::time::Instant::now();
        let refused = match NatsBackend::client(&url).await {
            Ok(_) => panic!("connected to no server"),
            Err(refused) => refused.to_string(),
        };
        let took = started.elapsed();
        assert!(took < UNANSWERED + UNANSWERED / 2, "refused after {took:?}");
        assert!(refused.contains(&format!("127.0.0.1:{a}")), "{refused}");
        assert!(!refused.contains("pw"), "{refused}");
    }
}
