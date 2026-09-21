//! The access records the dashboard evaluates from, and how fresh they are.
//!
//! Read from the conductor's configuration store at least every 30 seconds,
//! so a change to a permission reaches every live session within that. A
//! dashboard that has not read them for 10 minutes refuses every request
//! rather than serving what it last knew. Both are ADR 006's bounds, reused
//! for people (decisions/015), and neither is configuration.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use meridian_bus::Bus;
use meridian_domain::v1::{AccessRecords, AccessRecordsRequest};
use prost::Message;

use crate::clock::{Clock, MINUTE_NS, SECOND_NS};

pub const REFRESH_NS: i64 = 30 * SECOND_NS;
pub const CEILING_NS: i64 = 10 * MINUTE_NS;

pub const ACCESS_RECORDS: &str = "platform.config.query.access-records";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Stale {
    #[error("the dashboard has not yet read who may do what, and serves nothing until it has")]
    NeverRead,
    #[error(
        "the dashboard has not been able to read who may do what for over 10 minutes, and \
         refuses rather than act on what it last knew"
    )]
    PastCeiling,
}

#[derive(Default)]
pub struct RecordsCache {
    held: Mutex<Option<(AccessRecords, i64)>>,
}

impl RecordsCache {
    pub fn store(&self, records: AccessRecords, read_at_ns: i64) {
        *self.held.lock().expect("records lock poisoned") = Some((records, read_at_ns));
    }

    /// What access is evaluated from now, or a refusal naming why not.
    pub fn current(&self, now_ns: i64) -> Result<AccessRecords, Stale> {
        match &*self.held.lock().expect("records lock poisoned") {
            None => Err(Stale::NeverRead),
            Some((_, read_at)) if now_ns - read_at > CEILING_NS => Err(Stale::PastCeiling),
            Some((records, _)) => Ok(records.clone()),
        }
    }
}

/// Ask the conductor once, and keep the answer when there is one.
pub async fn refresh(bus: &Bus, cache: &RecordsCache, clock: &dyn Clock) -> Result<(), String> {
    let (_, bytes) = bus
        .call(
            ACCESS_RECORDS,
            "meridian.v1.AccessRecordsRequest",
            AccessRecordsRequest {}.encode_to_vec(),
            None,
            Some(Duration::from_secs(5)),
        )
        .await
        .map_err(|failed| failed.to_string())?;
    let records = AccessRecords::decode(&bytes[..])
        .map_err(|failed| format!("undecodable records: {failed}"))?;
    // Timed by this process's clock at receipt, not the store's reply: the
    // ceiling is about how long this dashboard has gone without hearing, and
    // two clocks would make it about their difference.
    cache.store(records, clock.now_ns());
    Ok(())
}

/// Every 30 seconds, for as long as the process runs. A failed read is logged
/// and retried; the ceiling, not this loop, decides when that becomes a
/// refusal.
pub async fn refresh_forever(bus: Arc<Bus>, cache: Arc<RecordsCache>, clock: Arc<dyn Clock>) {
    let mut every = tokio::time::interval(Duration::from_nanos(REFRESH_NS as u64));
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        if let Err(failed) = refresh(&bus, &cache, clock.as_ref()).await {
            tracing::warn!("the access records could not be read: {failed}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_790_380_800_000_000_000;

    #[test]
    fn nothing_is_served_before_the_first_read() {
        assert_eq!(RecordsCache::default().current(T0), Err(Stale::NeverRead));
    }

    #[test]
    fn records_serve_up_to_the_ceiling_and_are_refused_past_it() {
        let cache = RecordsCache::default();
        cache.store(AccessRecords::default(), T0);
        assert!(cache.current(T0 + CEILING_NS).is_ok());
        assert_eq!(cache.current(T0 + CEILING_NS + 1), Err(Stale::PastCeiling));
        cache.store(AccessRecords::default(), T0 + CEILING_NS + 1);
        assert!(
            cache.current(T0 + CEILING_NS + 1).is_ok(),
            "a fresh read restores it"
        );
    }

    #[test]
    fn the_bounds_are_the_ones_decision_015_states() {
        assert_eq!(REFRESH_NS, 30 * SECOND_NS);
        assert_eq!(CEILING_NS, 10 * MINUTE_NS);
    }
}
