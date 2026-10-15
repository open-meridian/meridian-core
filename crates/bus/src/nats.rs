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

/// The most requests a process answers at once (approved 2026-10-15, the
/// product owner, after the book's pod was OOMKilled): each is answered on
/// the blocking pool, and an unbounded burst there is as many threads, each
/// with an allocator arena of its own, as there are requests in flight. The
/// rest wait their turn on a cheap task.
pub const HANDLERS_AT_ONCE: usize = 16;

/// The bound a process runs with: `MERIDIAN_HANDLERS_AT_ONCE` where it is a
/// positive number, so a deployment lowers one component's without a
/// release; otherwise [`HANDLERS_AT_ONCE`].
pub fn handlers_at_once(said: Option<&str>) -> usize {
    said.and_then(|text| text.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(HANDLERS_AT_ONCE)
}

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

    /// Every served topic's handlers share it: at most [`HANDLERS_AT_ONCE`]
    /// answering at once.
    answering: Arc<tokio::sync::Semaphore>,
}

impl NatsBackend {
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
            answering: Arc::new(tokio::sync::Semaphore::new(handlers_at_once(
                std::env::var("MERIDIAN_HANDLERS_AT_ONCE").ok().as_deref(),
            ))),
        })
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

/// Split `nats://user:password@host` into what to present and where.
///
/// Taken from the URL rather than from a second setting, because that is where
/// a broker's client expects it and a second way to pass a credential is a
/// second place for it to be wrong. The client library does not read it from
/// there itself, which is the kind of thing that fails as an authentication
/// error naming no user at all.
fn credential_in(url: &str) -> (async_nats::ConnectOptions, String) {
    let (scheme, rest) = url.split_once("://").unwrap_or(("nats", url));

    let Some((credential, host)) = rest.rsplit_once('@') else {
        return (async_nats::ConnectOptions::new(), url.to_string());
    };

    let (user, password) = credential.split_once(':').unwrap_or((credential, ""));
    (
        async_nats::ConnectOptions::with_user_and_password(user.to_string(), password.to_string()),
        format!("{scheme}://{host}"),
    )
}

/// The address without its credential, for anything anybody will read.
fn redacted(url: &str) -> String {
    let (_, address) = credential_in(url);
    address
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
        let (tx, rx) = mpsc::channel(SUBSCRIBER_QUEUE);
        let subject = Self::subject(pattern);
        let client = self.client.clone();
        let owned = pattern.to_string();
        let dropped = DropCount::default();
        let counted = dropped.clone();
        let total = Arc::clone(&self.dropped);

        self.handle.spawn(async move {
            let mut messages = match client.subscribe(subject.clone()).await {
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
                        .payload(envelope.encode_to_vec().into())
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
    fn serve(&self, topic: &str, handler: Handler) {
        self.offer(topic, handler, None);
    }

    /// The same, signalling once each answer is on the wire.
    fn serve_delivered(&self, topic: &str, handler: Handler, delivered: Arc<Notify>) {
        self.offer(topic, handler, Some(delivered));
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl NatsBackend {
    fn offer(&self, topic: &str, handler: Handler, delivered: Option<Arc<Notify>>) {
        let client = self.client.clone();
        let subject = topic.to_string();
        let answering = Arc::clone(&self.answering);

        self.handle.spawn(async move {
            let mut requests = match client.subscribe(subject.clone()).await {
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
                let answering = Arc::clone(&answering);

                // Each request on its own task, so one slow handler does not
                // hold up the next, and on a blocking pool, because a handler
                // that blocks would otherwise stall unrelated work; at most
                // HANDLERS_AT_ONCE of them answering at once.
                tokio::spawn(async move {
                    let answered = answer_bounded(&answering, move || handler(asked)).await;

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

                    let sent = match refusal {
                        None => client.publish(reply_to, reply.encode_to_vec().into()).await,
                        Some(detail) => {
                            let mut headers = async_nats::HeaderMap::new();
                            headers.insert(REFUSAL_HEADER, detail.as_str());
                            client
                                .publish_with_headers(
                                    reply_to,
                                    headers,
                                    reply.encode_to_vec().into(),
                                )
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

/// `work` on the blocking pool once a permit is free, the permit held until
/// it is done: no more than the semaphore's permits answer at once.
async fn answer_bounded<T: Send + 'static>(
    answering: &tokio::sync::Semaphore,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    let _permit = answering.acquire().await;
    tokio::task::spawn_blocking(work).await
}

#[cfg(test)]
mod bounded_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn the_bound_is_sixteen_unless_a_positive_number_is_set() {
        assert_eq!(handlers_at_once(None), 16);
        assert_eq!(handlers_at_once(Some("4")), 4);
        assert_eq!(handlers_at_once(Some("0")), 16);
        assert_eq!(handlers_at_once(Some("many")), 16);
    }

    /// A burst of requests: never more than the permits answering at once,
    /// and every one answered.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_burst_is_answered_at_most_handlers_at_once_at_a_time() {
        let answering = Arc::new(tokio::sync::Semaphore::new(HANDLERS_AT_ONCE));
        let now = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for n in 0..(HANDLERS_AT_ONCE * 6) {
            let (answering, now, most) = (answering.clone(), now.clone(), most.clone());
            tasks.push(tokio::spawn(async move {
                answer_bounded(&answering, move || {
                    let at = now.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(at, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(20));
                    now.fetch_sub(1, Ordering::SeqCst);
                    n
                })
                .await
                .unwrap()
            }));
        }
        let mut answered = Vec::new();
        for task in tasks {
            answered.push(task.await.unwrap());
        }
        answered.sort_unstable();
        assert_eq!(answered, (0..HANDLERS_AT_ONCE * 6).collect::<Vec<_>>());
        let most = most.load(Ordering::SeqCst);
        assert!(most <= HANDLERS_AT_ONCE, "{most} answered at once");
        assert!(most > 1, "answered concurrently all the same: {most}");
    }
}
