//! What the sidecar carried for its instance, per UTC day (W5.19, contract
//! v25; amendment 4): the operations it carried on the bus and the page and
//! tool requests its front door served, reported as seven counts each on
//! its usual report, the seven UTC days ending on the report's day, the
//! earliest first. Kept by the sidecar, never by the plugin; the conductor
//! sums them per installed registry version for the weekly plugin usage
//! report. Counts only: no operation, page or person is kept.

use std::collections::BTreeMap;
use std::sync::Mutex;

const DAY_NS: i64 = 86_400_000_000_000;

/// The days kept: the seven a report carries.
const DAYS: i64 = 7;

#[derive(Default)]
pub struct Daily {
    /// Per UTC day since the epoch, the operations and the requests.
    days: Mutex<BTreeMap<i64, (u64, u64)>>,
}

impl Daily {
    fn count(&self, now_ns: i64, operation: bool) {
        let day = now_ns.div_euclid(DAY_NS);
        let mut days = self.days.lock().expect("daily lock poisoned");
        let counts = days.entry(day).or_default();
        if operation {
            counts.0 += 1;
        } else {
            counts.1 += 1;
        }
        // Nothing older than the seven days a report carries.
        days.retain(|held, _| *held > day - DAYS);
    }

    /// One operation carried on the bus.
    pub fn operation(&self, now_ns: i64) {
        self.count(now_ns, true);
    }

    /// One page or tool request the front door served.
    pub fn request(&self, now_ns: i64) {
        self.count(now_ns, false);
    }

    /// The seven days ending on `now_ns`'s, the earliest first: the
    /// operations and the requests; both empty while nothing is counted.
    pub fn seven(&self, now_ns: i64) -> (Vec<u64>, Vec<u64>) {
        let days = self.days.lock().expect("daily lock poisoned");
        if days.is_empty() {
            return (Vec::new(), Vec::new());
        }
        let today = now_ns.div_euclid(DAY_NS);
        (today - DAYS + 1..=today)
            .map(|day| days.get(&day).copied().unwrap_or_default())
            .unzip()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seven_days_are_reported_the_earliest_first_and_nothing_older_kept() {
        let daily = Daily::default();
        assert_eq!(
            daily.seven(0),
            (vec![], vec![]),
            "nothing counted, nothing said"
        );
        let day = |n: i64| n * DAY_NS + 5;
        daily.operation(day(10));
        daily.operation(day(10));
        daily.request(day(12));
        daily.operation(day(16));
        let (operations, requests) = daily.seven(day(16));
        assert_eq!(operations, [2, 0, 0, 0, 0, 0, 1]);
        assert_eq!(requests, [0, 0, 1, 0, 0, 0, 0]);
        // A week on, day 10 is gone and day 12 is the first kept.
        daily.request(day(18));
        let (operations, requests) = daily.seven(day(18));
        assert_eq!(operations, [0, 0, 0, 0, 1, 0, 0]);
        assert_eq!(requests, [1, 0, 0, 0, 0, 0, 1]);
    }
}
