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
//! as in memory. What changes is that a message can also be lost between
//! processes, which is the same guarantee said out loud rather than a weaker
//! one.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use meridian_pb::v1::Envelope;
use prost::Message as _;
use tokio::sync::mpsc;
use tokio::sync::Notify;

use crate::backend::{Answer, Backend, BusError, Delivery, Handler, Subscription};
use crate::topic;

/// Same bound as the in-memory backend, and for the same reason: a subscriber
/// that has stopped consuming is noticed rather than hidden behind a backlog.
const SUBSCRIBER_QUEUE: usize = 1024;

/// Where a refusal travels. Not in the payload, because the payload is the
/// answer's shape and a refusal is not an answer of that shape.
const REFUSAL_HEADER: &str = "Meridian-Refusal";

pub struct NatsBackend {
    client: async_nats::Client,

    /// The runtime this was built on, so a synchronous `publish` can hand work
    /// to it. Captured rather than created: two runtimes in one process is a
    /// deadlock waiting for an afternoon nobody has.
    handle: tokio::runtime::Handle,

    sequences: Mutex<HashMap<String, u64>>,
    dropped: AtomicU64,
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

        Ok(Self {
            client,
            handle: tokio::runtime::Handle::current(),
            sequences: Mutex::new(HashMap::new()),
            dropped: AtomicU64::new(0),
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
        let client = self.client.clone();
        let subject = topic.to_string();

        // Spawned rather than awaited: publish is synchronous above this
        // trait, and blocking a caller on a broker round trip would make every
        // publisher wait for the slowest link in the deployment.
        self.handle.spawn(async move {
            if let Err(failed) = client.publish(subject.clone(), bytes.into()).await {
                tracing::warn!(topic = subject, %failed, "the broker did not take a message");
            }
        });

        Ok(sequence)
    }

    fn subscribe(&self, pattern: &str) -> Subscription {
        let (tx, rx) = mpsc::channel(SUBSCRIBER_QUEUE);
        let subject = Self::subject(pattern);
        let client = self.client.clone();
        let owned = pattern.to_string();

        self.handle.spawn(async move {
            let mut messages = match client.subscribe(subject.clone()).await {
                Ok(messages) => messages,
                Err(failed) => {
                    tracing::error!(pattern = owned, %failed, "could not subscribe at the broker");
                    return;
                }
            };

            let mut sequence = 0u64;
            while let Some(message) = futures_util::StreamExt::next(&mut messages).await {
                let envelope = match Envelope::decode(message.payload) {
                    Ok(envelope) => envelope,
                    Err(failed) => {
                        // Something on our subjects that is not ours. Dropped
                        // and counted rather than passed up: a consumer cannot
                        // do anything with bytes that are not an envelope.
                        tracing::warn!(pattern = owned, %failed, "undecodable message");
                        continue;
                    }
                };

                sequence += 1;
                if tx.send(Delivery { envelope, sequence }).await.is_err() {
                    // The subscription was dropped. Ending the task is what
                    // unsubscribes at the broker, since the client goes with it.
                    break;
                }
            }
        });

        Subscription {
            rx,
            pattern: pattern.to_string(),
        }
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

                // Each request on its own task, so one slow handler does not
                // hold up the next, and on a blocking pool, because a handler
                // that blocks would otherwise stall unrelated work.
                tokio::spawn(async move {
                    let answered = tokio::task::spawn_blocking(move || handler(asked)).await;

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
