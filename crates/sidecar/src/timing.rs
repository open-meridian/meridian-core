//! The front door's timing line (ruling 798): measure before tuning.
//!
//! One line at info per request the front door answers: its method, its
//! route (the path, never its query), its status, the time to the plugin's
//! first byte and to the answer's end, and each bus call asked while it was
//! being answered, by topic and time. No payload and no person: the
//! assertion's subject is not logged.
//!
//! A plugin's reads reach the bus through this sidecar as calls of their
//! own, not inside the page's request, so a call is named by when it was
//! asked: every call asked between the request's arrival and its end. Where
//! other requests were in flight at once, `overlapping` says how many, and
//! the calls named may be theirs.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use meridian_bus::TimedCall;

/// The most recent calls kept to name; a request outlasting them says so.
const KEPT: usize = 512;

/// The most calls a line names; past it, how many more.
pub const CALLS_NAMED: usize = 24;

struct Heard {
    at: Instant,
    topic: String,
    took: Duration,
}

/// The calls this sidecar asked lately, and the front door's requests in
/// flight.
#[derive(Default)]
pub struct Calls {
    recent: Mutex<VecDeque<Heard>>,
    in_flight: AtomicUsize,
}

impl Calls {
    /// A call asked at `at`, answered after `took`.
    pub fn heard(&self, topic: &str, at: Instant, took: Duration) {
        let mut recent = self.recent.lock().expect("calls poisoned");
        if recent.len() == KEPT {
            recent.pop_front();
        }
        recent.push_back(Heard {
            at,
            topic: topic.to_string(),
            took,
        });
    }

    /// Every call asked since `from`, in the order asked; and whether some
    /// asked since were no longer kept.
    pub fn since(&self, from: Instant) -> (Vec<TimedCall>, bool) {
        let recent = self.recent.lock().expect("calls poisoned");
        let lost = recent.len() == KEPT && recent.front().is_some_and(|oldest| oldest.at > from);
        let mut calls: Vec<&Heard> = recent.iter().filter(|heard| heard.at >= from).collect();
        calls.sort_by_key(|heard| heard.at);
        (
            calls
                .into_iter()
                .map(|heard| TimedCall {
                    topic: heard.topic.clone(),
                    took: heard.took,
                })
                .collect(),
            lost,
        )
    }

    /// A request arrived: how many others were in flight.
    pub fn arrived(&self) -> usize {
        self.in_flight.fetch_add(1, Ordering::SeqCst)
    }

    pub fn left(&self) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One request's line, said once, when it is dropped: when the answer's
/// last byte has gone, or the person went away before it did.
pub struct Line {
    pub calls: std::sync::Arc<Calls>,
    pub method: String,
    pub route: String,
    pub status: u16,
    pub started: Instant,
    pub first_byte: Option<Duration>,
    pub overlapping: usize,
}

impl Drop for Line {
    fn drop(&mut self) {
        let total = self.started.elapsed();
        let (calls, lost) = self.calls.since(self.started);
        self.calls.left();
        let ms = |d: Duration| format!("{:.1}", d.as_secs_f64() * 1e3);
        tracing::info!(
            method = self.method,
            route = self.route,
            status = self.status,
            first_byte_ms = self.first_byte.map(ms).unwrap_or_default(),
            total_ms = ms(total),
            overlapping = self.overlapping,
            calls = calls.len(),
            calls_lost = lost,
            bus = meridian_bus::calls_said(&calls, CALLS_NAMED),
            "a request answered"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_calls_since_a_request_arrived_in_the_order_asked_and_none_before() {
        let calls = Calls::default();
        let t0 = Instant::now();
        calls.heard(
            "platform.lake.query.list-series",
            t0,
            Duration::from_millis(5),
        );
        let arrived = t0 + Duration::from_millis(10);
        calls.heard(
            "platform.lake.query.list-statistics",
            arrived + Duration::from_millis(30),
            Duration::from_millis(7),
        );
        calls.heard(
            "platform.lake.query.list-quotes",
            arrived + Duration::from_millis(20),
            Duration::from_millis(9),
        );
        let (since, lost) = calls.since(arrived);
        assert!(!lost);
        assert_eq!(
            since.iter().map(|c| c.topic.as_str()).collect::<Vec<_>>(),
            [
                "platform.lake.query.list-quotes",
                "platform.lake.query.list-statistics"
            ]
        );
        // A request outlasting what is kept says so.
        for i in 0..KEPT {
            calls.heard(
                "platform.lake.query.list-quotes",
                arrived + Duration::from_millis(100 + i as u64),
                Duration::ZERO,
            );
        }
        let (since, lost) = calls.since(arrived);
        assert_eq!(since.len(), KEPT);
        assert!(lost);
        assert_eq!(calls.arrived(), 0);
        assert_eq!(calls.arrived(), 1);
    }
}
