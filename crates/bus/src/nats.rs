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
const REFUSAL_HEADER: &str = "Meridian-Refusal";

/// How long a request's caller waits for its answer, in milliseconds, carried
/// beside the request as gRPC's own `grpc-timeout` is: relative, so no two
/// clocks are compared. The answering side holds a read to its `read_after`
/// no longer than its caller waits (contract v30, ruled 2026-11-10).
const TIMEOUT_HEADER: &str = "Meridian-Timeout-Ms";

/// How often a client asks its server to hand a message back to it, and how
/// long it waits for it before moving to another server (step 3b): the
/// servers' 2 s rule for a client, said from the client's side. A server
/// cut off from its clients, or on a node that stopped, sends no reset and
/// no error, and the library's own ping waits a minute -- and is put off by
/// every message the client sends, so a client that publishes often never
/// pings at all. Meanwhile its subscriptions, a writer's change log among
/// them, are served by nobody. The round trip is on an inbox of its own,
/// which every credential holds (`_INBOX.>`).
const HEARTBEAT: Duration = Duration::from_secs(1);
const UNANSWERED: Duration = Duration::from_secs(2);

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
        let client = options
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
            dropped: Arc::new(AtomicU64::new(0)),
            outbox,
            heartbeat,
        })
    }

    /// A client of its own on the broker the URL names, with its credential,
    /// for a component that speaks to JetStream itself (step 3b: the
    /// analytics units' work queue). Everything else goes by the bus.
    /// Its heartbeat runs as long as the process does.
    pub async fn client(url: &str) -> Result<async_nats::Client, BusError> {
        let (options, address) = credential_in(url);
        let client = options
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
                ),
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

    /// Offer a local handler's answer to callers in other processes.
    fn serve(&self, topic: &str, group: &str, handler: Handler) {
        self.offer(topic, group, handler, None);
    }

    /// The same, signalling once each answer is on the wire.
    fn serve_delivered(&self, topic: &str, group: &str, handler: Handler, delivered: Arc<Notify>) {
        self.offer(topic, group, handler, Some(delivered));
    }

    /// One of a queue group at the broker (contract v25): each message to one
    /// member of the group.
    fn subscribe_shared(&self, pattern: &str, group: &str) -> Subscription {
        self.listen(pattern, Some(group.to_string()))
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
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
        let counted = dropped.clone();
        let total = Arc::clone(&self.dropped);

        self.handle.spawn(async move {
            let subscribed = match group {
                None => client.subscribe(subject.clone()).await,
                Some(group) => client.queue_subscribe(subject.clone(), group).await,
            };
            let mut messages = match subscribed {
                Ok(messages) => messages,
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
            while let Some(message) = futures_util::StreamExt::next(&mut messages).await {
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
    fn offer(&self, topic: &str, group: &str, handler: Handler, delivered: Option<Arc<Notify>>) {
        let client = self.client.clone();
        let subject = topic.to_string();
        let group = group.to_string();

        self.handle.spawn(async move {
            // One of the component's queue group (contract v25): each request
            // reaches one copy of the component, which answers it.
            let mut requests = match client.queue_subscribe(subject.clone(), group).await {
                Ok(requests) => requests,
                Err(failed) => {
                    tracing::error!(topic = subject, %failed, "could not offer a handler");
                    return;
                }
            };

            while let Some(message) = futures_util::StreamExt::next(&mut requests).await {
                // A publish on a topic somebody also serves: there is nobody
                // to answer, so there is nothing to do with it here.
                let Some(reply_to) = message.reply.clone() else {
                    continue;
                };

                // When its caller stops waiting, from what it said as it
                // asked, counted from now (contract v30, ruled 2026-11-10).
                let deadline = message
                    .headers
                    .as_ref()
                    .and_then(|headers| headers.get(TIMEOUT_HEADER))
                    .and_then(|said| said.to_string().parse::<u64>().ok())
                    .map(|ms| std::time::Instant::now() + Duration::from_millis(ms));

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

                // Each request on its own task, so one slow handler does not
                // hold up the next, and on a blocking pool, because a handler
                // that blocks would otherwise stall unrelated work. Admitted
                // first (the resilience spec's section 10a): its turn among
                // its class's callers at this process's gate -- at most
                // HANDLERS_AT_ONCE answering at once -- and its working memory
                // reserved; or refused for load with a retry-after.
                tokio::spawn(async move {
                    let answered = crate::router::admitted(&topic, asked, handler, deadline).await;

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
                            headers.insert(REFUSAL_HEADER, detail.as_str());
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
                crate::router::admitted("platform.lake.query.list-quotes", asked, handler, None)
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
