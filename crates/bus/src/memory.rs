//! In-process backend.
//!
//! Not a toy. Every message in a single-host deployment stays in this process,
//! so this is the production path for that shape of deployment and is written
//! accordingly: per-topic sequencing, bounded queues, observable drops.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

use meridian_pb::v1::Envelope;
use tokio::sync::mpsc;

use crate::backend::{Backend, BusError, Delivery, Subscription};
use crate::topic;

/// How many messages a subscriber may fall behind before it starts losing them.
///
/// Sized so a subscriber doing ordinary work survives a burst, and a subscriber
/// that has stopped consuming is noticed quickly rather than accumulating an
/// unbounded backlog that hides the failure.
const SUBSCRIBER_QUEUE: usize = 1024;

struct Subscriber {
    pattern: String,
    tx: mpsc::Sender<Delivery>,
}

pub struct MemoryBackend {
    subscribers: RwLock<Vec<Subscriber>>,
    sequences: Mutex<HashMap<String, u64>>,
    dropped: AtomicU64,
}

impl MemoryBackend {
    pub fn new() -> Self {
        Self {
            subscribers: RwLock::new(Vec::new()),
            sequences: Mutex::new(HashMap::new()),
            dropped: AtomicU64::new(0),
        }
    }

    fn next_sequence(&self, topic: &str) -> u64 {
        let mut sequences = self.sequences.lock().expect("sequence lock poisoned");
        let entry = sequences.entry(topic.to_string()).or_insert(0);
        *entry += 1;
        *entry
    }
}

impl Default for MemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for MemoryBackend {
    fn publish(&self, topic: &str, envelope: Envelope) -> Result<u64, BusError> {
        if !topic::is_publishable(topic) {
            return Err(BusError::NotPublishable(topic.to_string()));
        }

        let sequence = self.next_sequence(topic);
        let delivery = Delivery { envelope, sequence };

        // Closed senders accumulate as subscribers are dropped. Sweeping them
        // here rather than on drop keeps `Subscription` free of a back-pointer
        // to the backend, which would otherwise make its lifetime the awkward
        // part of every consumer.
        let mut closed = false;

        {
            let subscribers = self.subscribers.read().expect("subscriber lock poisoned");
            for subscriber in subscribers.iter() {
                if !topic::matches(&subscriber.pattern, topic) {
                    continue;
                }
                match subscriber.tx.try_send(delivery.clone()) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        // At-most-once: this subscriber loses the message. The
                        // publisher and every other subscriber are unaffected,
                        // which is the whole point of not blocking here.
                        self.dropped.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(
                            topic = topic,
                            pattern = subscriber.pattern,
                            "subscriber queue full; message dropped"
                        );
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => closed = true,
                }
            }
        }

        if closed {
            let mut subscribers = self.subscribers.write().expect("subscriber lock poisoned");
            subscribers.retain(|s| !s.tx.is_closed());
        }

        Ok(sequence)
    }

    fn subscribe(&self, pattern: &str) -> Subscription {
        let (tx, rx) = mpsc::channel(SUBSCRIBER_QUEUE);
        self.subscribers
            .write()
            .expect("subscriber lock poisoned")
            .push(Subscriber {
                pattern: pattern.to_string(),
                tx,
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

#[cfg(test)]
mod tests {
    use super::*;
    use meridian_pb::v1::MessageMeta;

    fn envelope(topic: &str) -> Envelope {
        Envelope {
            meta: Some(MessageMeta {
                topic: topic.to_string(),
                ..Default::default()
            }),
            payload_type: "meridian.v1.PositionUpdatedEvent".into(),
            payload: Vec::new(),
        }
    }

    #[tokio::test]
    async fn delivers_to_a_matching_subscriber() {
        let bus = MemoryBackend::new();
        let mut sub = bus.subscribe("platform.kernel.event.position-updated");
        bus.publish("platform.kernel.event.position-updated", envelope("x"))
            .unwrap();

        let got = sub.recv().await.expect("a delivery");
        assert_eq!(got.sequence, 1);
    }

    #[tokio::test]
    async fn does_not_deliver_to_a_non_matching_subscriber() {
        let bus = MemoryBackend::new();
        let mut other = bus.subscribe("platform.reference.**");
        bus.publish("platform.kernel.event.position-updated", envelope("x"))
            .unwrap();

        // Nothing queued: try_recv would block forever on recv, so check the
        // channel is empty rather than waiting on it.
        assert!(other.rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn every_matching_subscriber_gets_its_own_copy() {
        let bus = MemoryBackend::new();
        let mut a = bus.subscribe("platform.kernel.**");
        let mut b = bus.subscribe("platform.kernel.event.position-updated");
        bus.publish("platform.kernel.event.position-updated", envelope("x"))
            .unwrap();

        assert!(a.recv().await.is_some());
        assert!(b.recv().await.is_some());
    }

    #[tokio::test]
    async fn sequence_is_per_topic() {
        let bus = MemoryBackend::new();
        assert_eq!(
            bus.publish("platform.kernel.event.position-updated", envelope("x"))
                .unwrap(),
            1
        );
        assert_eq!(
            bus.publish("platform.kernel.event.position-updated", envelope("x"))
                .unwrap(),
            2
        );
        // A different topic starts its own count rather than continuing a
        // global one.
        assert_eq!(
            bus.publish("platform.kernel.event.statement-recorded", envelope("x"))
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn a_pattern_cannot_be_published_on() {
        let bus = MemoryBackend::new();
        let err = bus
            .publish("platform.custody.*.event.sync-status", envelope("x"))
            .unwrap_err();
        assert!(matches!(err, BusError::NotPublishable(_)));
    }

    #[tokio::test]
    async fn a_full_subscriber_loses_messages_and_the_loss_is_counted() {
        let bus = MemoryBackend::new();
        let _sub = bus.subscribe("platform.kernel.**");

        // Never read from _sub, so the queue fills and then overflows.
        for _ in 0..(SUBSCRIBER_QUEUE + 10) {
            bus.publish("platform.kernel.event.position-updated", envelope("x"))
                .unwrap();
        }

        assert_eq!(
            bus.dropped(),
            10,
            "overflow beyond the queue should be dropped, not queued"
        );
    }

    #[tokio::test]
    async fn a_slow_subscriber_does_not_stall_a_healthy_one() {
        let bus = MemoryBackend::new();
        let _slow = bus.subscribe("platform.kernel.**");
        let mut healthy = bus.subscribe("platform.kernel.**");

        for _ in 0..(SUBSCRIBER_QUEUE + 5) {
            bus.publish("platform.kernel.event.position-updated", envelope("x"))
                .unwrap();
        }

        // The healthy subscriber is equally full here, but the publisher never
        // blocked: every publish returned.
        assert!(healthy.recv().await.is_some());
        assert!(bus.dropped() > 0);
    }

    #[tokio::test]
    async fn dropping_a_subscription_stops_delivery_to_it() {
        let bus = MemoryBackend::new();
        {
            let _sub = bus.subscribe("platform.kernel.**");
        }
        bus.publish("platform.kernel.event.position-updated", envelope("x"))
            .unwrap();
        // The closed subscriber is swept, not counted as a drop.
        assert_eq!(bus.dropped(), 0);
        assert_eq!(bus.subscribers.read().unwrap().len(), 0);
    }
}
