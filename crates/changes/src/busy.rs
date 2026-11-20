//! How busy a writer's commit lane is (step 3d; the resilience spec's
//! sections 6.6 and 12; `ComponentReport.peak_write_busy_percent`, approved
//! 2026-11-14): the share of each minute a partition's lane had a
//! transaction in flight, the busiest partition's, and the highest such
//! share over the profile's window. The profile advises a writer group
//! more where it passes its bound.
//!
//! A share of time, never a record: what is kept is a few counters a
//! minute, per partition written in that minute, for the window alone.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

/// A bucket's length: one minute.
pub const BUCKET_NS: i64 = 60_000_000_000;

/// The profile's window: the last fifteen minutes.
pub const WINDOW_BUCKETS: usize = 15;

/// Each minute's busy time by partition, the newest last.
#[derive(Debug, Default)]
struct Buckets {
    minutes: VecDeque<(i64, BTreeMap<String, i64>)>,
}

static BUSY: Mutex<Buckets> = Mutex::new(Buckets {
    minutes: VecDeque::new(),
});

/// A transaction of `partition`'s committed (or failed) after `took`,
/// ending at `at_ns`: counted in the minute it ended in.
pub fn record(partition: &str, took: Duration, at_ns: i64) {
    let minute = at_ns.div_euclid(BUCKET_NS);
    let mut held = BUSY.lock().expect("busy poisoned");
    match held.minutes.back_mut() {
        Some((at, _)) if *at == minute => {}
        _ => {
            held.minutes.push_back((minute, BTreeMap::new()));
            while held.minutes.len() > WINDOW_BUCKETS + 1 {
                held.minutes.pop_front();
            }
        }
    }
    if let Some((_, by)) = held.minutes.back_mut() {
        *by.entry(partition.to_string()).or_default() +=
            took.as_nanos().min(i64::MAX as u128) as i64;
    }
}

/// The highest share, in whole percent, any partition's lane was busy in a
/// minute of the window ending at `now_ns`: 0 where nothing was written.
/// The minute under way counts against the whole minute, never what of it
/// has passed, so its first moments never read as saturated.
pub fn peak_percent(now_ns: i64) -> u32 {
    let current = now_ns.div_euclid(BUCKET_NS);
    let held = BUSY.lock().expect("busy poisoned");
    held.minutes
        .iter()
        .filter(|(minute, _)| current - *minute < WINDOW_BUCKETS as i64)
        .map(|(_, by)| {
            let busiest = by.values().copied().max().unwrap_or(0);
            ((busiest as f64 / BUCKET_NS as f64) * 100.0)
                .round()
                .min(100.0) as u32
        })
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
pub(crate) fn forget() {
    BUSY.lock().expect("busy poisoned").minutes.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_busiest_lane_of_a_minute_is_its_share_and_the_window_keeps_fifteen() {
        forget();
        let t0 = 10 * BUCKET_NS;
        // P0 busy 45 s of the minute, P1 15 s: P0's lane is the share.
        record("P0", Duration::from_secs(45), t0 + 1);
        record("P1", Duration::from_secs(15), t0 + 2);
        assert_eq!(peak_percent(t0 + BUCKET_NS + 5), 75);
        // A quieter minute after it leaves the peak where it was.
        record("P0", Duration::from_secs(6), t0 + BUCKET_NS + 1);
        assert_eq!(peak_percent(t0 + 2 * BUCKET_NS), 75);
        // Out of the window: forgotten.
        assert_eq!(peak_percent(t0 + 20 * BUCKET_NS), 0);
        forget();
    }
}
