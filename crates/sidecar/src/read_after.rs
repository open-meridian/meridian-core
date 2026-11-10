//! The sidecar's session for `read_after` (contract v30, step 3c stage B;
//! W4.4; spec/a-deployment-scales-out-and-fails-over-by-configuration
//! section 5; approved 2026-11-09 and 2026-11-10).
//!
//! The highest sequence this instance has seen on each of a store's
//! partitions -- from its acknowledged writes, the deliveries passed on to
//! its plugin and the watermark each of its reads was answered at -- named
//! on each read it asks of that store as `MessageMeta.read_after`. The
//! plugin sets nothing: read-your-writes and monotonic reads hold for the
//! instance, as MongoDB's causally consistent sessions and Cosmos DB's
//! session tokens give them, both of which advance a session on every reply.
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
// gap until M2: these 256 KiB are reserved against the sidecar's budget once
// stage M2 installs it (tasks/sdk-contract/standing-analytics-and-resilience-
// contract, ruled 2026-11-10); until then they are a fixed share outside it.
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
        let Some(domain) = domain_of(topic) else {
            return;
        };
        // A read's own watermark counted too (ruled 2026-11-10): the
        // plugin's next read is never answered behind one it was answered.
        let seen = if is_read(topic) {
            answered_at(payload_type, payload)
        } else {
            acknowledged(payload_type, payload)
        };
        self.saw(domain, seen);
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

/// The field each store's read reply carries the watermark it was answered
/// at in: the lake's `watermark`, the book's `as_of`. Checked against the
/// generated messages by this module's tests.
const ANSWERED_AT: &[(&str, u32)] = &[
    ("ListBarsReply", 4),
    ("ListCorporateActionsReply", 4),
    ("ListExpirationsReply", 4),
    ("ListFactorsReply", 4),
    ("ListFilingsReply", 4),
    ("ListHoldingsReply", 4),
    ("ListOutcomesReply", 2),
    ("ListPricesReply", 4),
    ("ListQuotesReply", 4),
    ("ListRatesReply", 4),
    ("ListRatingActionsReply", 4),
    ("ListReportedFactsReply", 4),
    ("ListSeriesReply", 4),
    ("ListStatisticsReply", 4),
    ("ListTradesReply", 4),
    ("QueryLakeReply", 8),
    ("ListAccountAttributesReply", 3),
    ("ListAccountFiguresReply", 3),
    ("ListActivitiesReply", 3),
    ("ListAlertsReply", 2),
    ("ListBookingsReply", 2),
    ("ListBreaksReply", 3),
    ("ListCustodialPositionsReply", 4),
    ("ListLifecycleEventsReply", 2),
    ("ListLocatesReply", 2),
    ("ListOrdersReply", 2),
    ("ListPositionsReply", 3),
    ("ListStatementsReply", 3),
    ("ListSyncStatusesReply", 3),
    ("ListTradingInstructionsReply", 3),
];

/// The watermark a read's reply says it was answered at, read from its one
/// field and every other field skipped, so a page of rows is not decoded
/// twice. Nothing for a reply that carries none.
pub fn answered_at(payload_type: &str, payload: &[u8]) -> Vec<(String, u64)> {
    let Some(tag) = payload_type
        .strip_prefix("meridian.v1.")
        .and_then(|name| ANSWERED_AT.iter().find(|(reply, _)| *reply == name))
        .map(|(_, tag)| *tag)
    else {
        return Vec::new();
    };
    watermark_in(payload, tag)
        .map(|mark| {
            mark.partitions
                .into_iter()
                .map(|at| (at.partition, at.sequence))
                .collect()
        })
        .unwrap_or_default()
}

fn watermark_in(mut payload: &[u8], tag: u32) -> Option<Watermark> {
    use prost::encoding::{decode_key, decode_varint, skip_field, DecodeContext, WireType};
    let mut found = None;
    while !payload.is_empty() {
        let (number, wire) = decode_key(&mut payload).ok()?;
        if number == tag && wire == WireType::LengthDelimited {
            let length = decode_varint(&mut payload).ok()? as usize;
            if length > payload.len() {
                return None;
            }
            let (field, rest) = payload.split_at(length);
            // A field repeated on the wire merges, as prost's own decode does.
            let mut mark: Watermark = found.take().unwrap_or_default();
            mark.merge(field).ok()?;
            found = Some(mark);
            payload = rest;
        } else {
            skip_field(wire, number, &mut payload, DecodeContext::default()).ok()?;
        }
    }
    found
}

#[cfg(test)]
mod tests;
