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
use std::sync::Mutex;

use meridian_pb::v1::Envelope;
use prost::Message as _;
use tokio::sync::mpsc;

use crate::backend::{Backend, BusError, Delivery, Subscription};
use crate::topic;

/// Same bound as the in-memory backend, and for the same reason: a subscriber
/// that has stopped consuming is noticed rather than hidden behind a backlog.
const SUBSCRIBER_QUEUE: usize = 1024;

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
    /// Must be called from a Tokio runtime: the handle it captures is what
    /// lets the synchronous half of [`Backend`] reach an async client.
    pub async fn connect(url: &str) -> Result<Self, BusError> {
        let client = async_nats::connect(url)
            .await
            .map_err(|failed| BusError::HandlerFailed {
                topic: url.to_string(),
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

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}
