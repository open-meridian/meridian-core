//! In-process backend.
//!
//! Not a toy. Every message in a single-host deployment stays in this process,
//! so this is the production path for that shape of deployment and is written
//! accordingly: per-topic sequencing, bounded queues, observable drops.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

use meridian_domain::v1::Envelope;
use tokio::sync::mpsc;

use crate::backend::{Backend, BusError, Delivery, DropCount, Subscription};
use crate::topic;

/// How many messages a subscriber may fall behind before it starts losing them.
///
/// Sized so a subscriber doing ordinary work survives a burst, and a subscriber
/// that has stopped consuming is noticed quickly rather than accumulating an
/// unbounded backlog that hides the failure.
const SUBSCRIBER_QUEUE: usize = 1024;

struct Subscriber {
    pattern: String,
    /// The group it shares messages with (contract v25), or none: every
    /// subscriber hears every message.
    group: Option<String>,
    tx: mpsc::Sender<Delivery>,
    dropped: DropCount,
}

pub struct MemoryBackend {
    subscribers: RwLock<Vec<Subscriber>>,
    /// Each group's turn, by pattern and group: which of its open members
    /// takes the next message.
    turns: Mutex<HashMap<(String, String), usize>>,
    sequences: Mutex<HashMap<String, u64>>,
    dropped: AtomicU64,
}

impl MemoryBackend {
    pub fn new() -> Self {
        Self {
            subscribers: RwLock::new(Vec::new()),
            turns: Mutex::new(HashMap::new()),
            sequences: Mutex::new(HashMap::new()),
            dropped: AtomicU64::new(0),
        }
    }

    fn join(&self, pattern: &str, group: Option<String>) -> Subscription {
        let (tx, rx) = mpsc::channel(SUBSCRIBER_QUEUE);
        let dropped = DropCount::default();
        self.subscribers
            .write()
            .expect("subscriber lock poisoned")
            .push(Subscriber {
                pattern: pattern.to_string(),
                group,
                tx,
                dropped: dropped.clone(),
            });
        Subscription::new(rx, pattern, dropped)
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
            // One member of each group takes the message, in turn among its
            // open members (contract v25), as a broker's queue group does.
            let mut chosen: HashMap<(String, String), usize> = HashMap::new();
            {
                let mut members: HashMap<(String, String), Vec<usize>> = HashMap::new();
                for (at, subscriber) in subscribers.iter().enumerate() {
                    if let Some(group) = &subscriber.group {
                        if topic::matches(&subscriber.pattern, topic) && !subscriber.tx.is_closed()
                        {
                            members
                                .entry((subscriber.pattern.clone(), group.clone()))
                                .or_default()
                                .push(at);
                        }
                    }
                }
                let mut turns = self.turns.lock().expect("turn lock poisoned");
                for (key, held) in members {
                    let turn = turns.entry(key.clone()).or_insert(0);
                    chosen.insert(key, held[*turn % held.len()]);
                    *turn = turn.wrapping_add(1);
                }
            }
            for (at, subscriber) in subscribers.iter().enumerate() {
                if !topic::matches(&subscriber.pattern, topic) {
                    continue;
                }
                if let Some(group) = &subscriber.group {
                    if chosen.get(&(subscriber.pattern.clone(), group.clone())) != Some(&at) {
                        continue;
                    }
                }
                match subscriber.tx.try_send(delivery.clone()) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        // At-most-once: this subscriber loses the message. The
                        // publisher and every other subscriber are unaffected,
                        // which is the whole point of not blocking here. The
                        // newest message is the one lost: what is queued stays
                        // queued, in order.
                        self.dropped.fetch_add(1, Ordering::Relaxed);
                        subscriber.dropped.add();
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
        self.join(pattern, None)
    }

    fn subscribe_shared(&self, pattern: &str, group: &str) -> Subscription {
        self.join(pattern, Some(group.to_string()))
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_member_of_a_group_takes_each_message_and_plain_subscribers_every_one() {
        // Contract v25 (Q15): as a broker's queue group does.
        let backend = MemoryBackend::new();
        let topic = "platform.street.event.work";
        let mut a = backend.subscribe_shared(topic, "street.keeping");
        let mut b = backend.subscribe_shared(topic, "street.keeping");
        let mut other = backend.subscribe_shared(topic, "lake.keeping");
        let mut every = backend.subscribe(topic);
        for _ in 0..6 {
            backend.publish(topic, envelope(topic)).unwrap();
        }
        let count = |s: &mut Subscription| {
            let mut n = 0;
            while s.try_recv().is_some() {
                n += 1;
            }
            n
        };
        assert_eq!((count(&mut a), count(&mut b)), (3, 3), "in turn, once each");
        assert_eq!(count(&mut other), 6, "another group hears each");
        assert_eq!(count(&mut every), 6, "a plain subscriber hears each");
        // A member gone, the other takes all.
        drop(a);
        for _ in 0..4 {
            backend.publish(topic, envelope(topic)).unwrap();
        }
        assert_eq!(count(&mut b), 4);
    }
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
        let mut sub = bus.subscribe("platform.street.event.position-updated");
        bus.publish("platform.street.event.position-updated", envelope("x"))
            .unwrap();

        let got = sub.recv().await.expect("a delivery");
        assert_eq!(got.sequence, 1);
    }

    #[tokio::test]
    async fn does_not_deliver_to_a_non_matching_subscriber() {
        let bus = MemoryBackend::new();
        let mut other = bus.subscribe("platform.reference.**");
        bus.publish("platform.street.event.position-updated", envelope("x"))
            .unwrap();

        // Nothing queued: try_recv would block forever on recv, so check the
        // channel is empty rather than waiting on it.
        assert!(other.rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn every_matching_subscriber_gets_its_own_copy() {
        let bus = MemoryBackend::new();
        let mut a = bus.subscribe("platform.street.**");
        let mut b = bus.subscribe("platform.street.event.position-updated");
        bus.publish("platform.street.event.position-updated", envelope("x"))
            .unwrap();

        assert!(a.recv().await.is_some());
        assert!(b.recv().await.is_some());
    }

    #[tokio::test]
    async fn sequence_is_per_topic() {
        let bus = MemoryBackend::new();
        assert_eq!(
            bus.publish("platform.street.event.position-updated", envelope("x"))
                .unwrap(),
            1
        );
        assert_eq!(
            bus.publish("platform.street.event.position-updated", envelope("x"))
                .unwrap(),
            2
        );
        // A different topic starts its own count rather than continuing a
        // global one.
        assert_eq!(
            bus.publish("platform.street.event.statement-recorded", envelope("x"))
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
        let _sub = bus.subscribe("platform.street.**");

        // Never read from _sub, so the queue fills and then overflows.
        for _ in 0..(SUBSCRIBER_QUEUE + 10) {
            bus.publish("platform.street.event.position-updated", envelope("x"))
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
        let _slow = bus.subscribe("platform.street.**");
        let mut healthy = bus.subscribe("platform.street.**");

        for _ in 0..(SUBSCRIBER_QUEUE + 5) {
            bus.publish("platform.street.event.position-updated", envelope("x"))
                .unwrap();
        }

        // The healthy subscriber is equally full here, but the publisher never
        // blocked: every publish returned.
        assert!(healthy.recv().await.is_some());
        assert!(bus.dropped() > 0);
    }

    #[tokio::test]
    async fn each_subscription_counts_its_own_losses_and_keeps_its_oldest() {
        let bus = MemoryBackend::new();
        let slow = bus.subscribe("platform.street.**");
        let mut healthy = bus.subscribe("platform.street.**");

        // The healthy subscriber keeps up; the slow one never reads.
        let mut heard = 0;
        for _ in 0..(SUBSCRIBER_QUEUE + 7) {
            bus.publish("platform.street.event.position-updated", envelope("x"))
                .unwrap();
            while healthy.rx.try_recv().is_ok() {
                heard += 1;
            }
        }

        assert_eq!(
            heard,
            SUBSCRIBER_QUEUE + 7,
            "the healthy subscriber lost nothing"
        );
        assert_eq!(healthy.dropped(), 0);
        assert_eq!(
            slow.dropped(),
            7,
            "the slow subscriber lost the overflow alone"
        );
        assert_eq!(bus.dropped(), 7);

        // The count is readable after the subscription has become a stream,
        // and what was queued is the oldest of what was sent.
        let count = slow.drop_count();
        let mut stream = slow.into_stream();
        let first = tokio_stream::StreamExt::next(&mut stream).await.unwrap();
        assert_eq!(first.sequence, 1, "the newest is dropped, not the oldest");
        assert_eq!(count.get(), 7);
    }

    #[tokio::test]
    async fn dropping_a_subscription_stops_delivery_to_it() {
        let bus = MemoryBackend::new();
        {
            let _sub = bus.subscribe("platform.street.**");
        }
        bus.publish("platform.street.event.position-updated", envelope("x"))
            .unwrap();
        // The closed subscriber is swept, not counted as a drop.
        assert_eq!(bus.dropped(), 0);
        assert_eq!(bus.subscribers.read().unwrap().len(), 0);
    }
}
