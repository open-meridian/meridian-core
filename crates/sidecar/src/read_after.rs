//! The sidecar's session for `read_after` (contract v30, step 3c stage B;
//! W4.4; spec/a-deployment-scales-out-and-fails-over-by-configuration
//! section 5; approved 2026-11-09).
//!
//! The highest sequence this instance has seen on each of a store's
//! partitions -- from its acknowledged writes and the deliveries passed on to
//! its plugin -- named on each read it asks of that store as
//! `MessageMeta.read_after`. The plugin sets nothing: read-your-writes and
//! monotonic reads hold for the instance, as MongoDB's causally consistent
//! sessions and Cosmos DB's session tokens give them. A read's own answer is
//! not counted (the ruling names writes and deliveries).
//!
//! Bounded: at most [`MOST_BYTES`] of partitions are held, a fixed share
//! within the sidecar's own (its per-plugin caps are stage M2's
//! `sidecar.limits`); a partition past it is not held, said once, and reads
//! of it name no more than was held.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use meridian_bus::read_after::{domain_of, is_read, raise, watermark_of, Session};
use meridian_bus::Bus;
use meridian_domain::v1 as domain;
use meridian_pb::v1::Watermark;
use prost::Message;

/// The most a session holds: its partitions' names and sequences, a few
/// thousand partitions.
pub const MOST_BYTES: usize = 256 * 1024;

/// What one partition held costs, beyond its name.
const PER_PARTITION: usize = 64;

/// A sidecar's session: what its instance has seen of each store.
#[derive(Default)]
pub struct Seen {
    at: Mutex<Held>,
    said_full: AtomicBool,
}

#[derive(Default)]
struct Held {
    stores: HashMap<String, HashMap<String, u64>>,
    bytes: usize,
}

impl Seen {
    /// A session kept on `bus`: its reads stamped, its answers seen.
    pub fn keep_on(bus: &Bus) -> Arc<Self> {
        let seen = Arc::new(Self::default());
        bus.reads_after()
            .set_session(Arc::clone(&seen) as Arc<dyn Session>);
        seen
    }

    fn saw(&self, domain: &str, seen: Vec<(String, u64)>) {
        if seen.is_empty() {
            return;
        }
        let mut held = self.at.lock().expect("session poisoned");
        let Held { stores, bytes } = &mut *held;
        let store = stores.entry(domain.to_string()).or_default();
        for (partition, sequence) in seen {
            if !store.contains_key(&partition) {
                let cost = partition.len() + PER_PARTITION;
                if *bytes + cost > MOST_BYTES {
                    if !self.said_full.swap(true, Ordering::Relaxed) {
                        tracing::warn!(
                            domain,
                            most_bytes = MOST_BYTES,
                            "the read_after session is full: a partition past it is not held"
                        );
                    }
                    continue;
                }
                *bytes += cost;
            }
            raise(store, [(partition, sequence)]);
        }
    }
}

impl Session for Seen {
    fn read_after(&self, domain: &str) -> Option<Watermark> {
        let held = self.at.lock().expect("session poisoned");
        held.stores
            .get(domain)
            .filter(|at| !at.is_empty())
            .map(watermark_of)
    }

    fn answered(&self, topic: &str, payload_type: &str, payload: &[u8]) {
        if is_read(topic) {
            return;
        }
        let Some(domain) = domain_of(topic) else {
            return;
        };
        self.saw(domain, acknowledged(payload_type, payload));
    }

    fn delivered(&self, topic: &str, partition: &str, sequence: u64) {
        if let Some(domain) = domain_of(topic) {
            self.saw(domain, vec![(partition.to_string(), sequence)]);
        }
    }
}

/// Where an acknowledged write's answer says it was committed: the lake's
/// watermark over the batch's datasets, or a book or street change's
/// journal reference. Nothing for an answer that names neither.
pub fn acknowledged(payload_type: &str, payload: &[u8]) -> Vec<(String, u64)> {
    macro_rules! journal_of {
        ($($reply:ident),* $(,)?) => {
            match payload_type {
                $(concat!("meridian.v1.", stringify!($reply)) => domain::$reply::decode(payload)
                    .ok()
                    .and_then(|reply| reply.journal)
                    .map(|journal| vec![(journal.partition, journal.sequence)])
                    .unwrap_or_default(),)*
                "meridian.v1.RecordObservationsReply" => domain::RecordObservationsReply::decode(payload)
                    .ok()
                    .and_then(|reply| reply.watermark)
                    .map(|mark| {
                        mark.partitions
                            .into_iter()
                            .map(|at| (at.partition, at.sequence))
                            .collect()
                    })
                    .unwrap_or_default(),
                _ => Vec::new(),
            }
        };
    }
    journal_of!(
        BookEntryReply,
        AccountAttributeReply,
        OrderReply,
        PlacementReply,
        FactReply,
        AcceptExecutionReply,
        DontKnowExecutionReply,
        TradingInstructionReply,
        LocateReply,
        BookingReply,
        SleeveCrossReply,
        AlertReply,
    )
}

#[cfg(test)]
mod tests;
