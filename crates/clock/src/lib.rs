//! The deployment's one clock. decisions/024: time is the deployment's.
//!
//! Every component reads the time through [`Clock`], handed to it by the
//! process that wires it up, and nothing reads the wall clock itself. Five
//! components once each defined a `Clock` of their own and the bus and the
//! sidecar read the wall clock directly, so the times in one journal came from
//! as many sources as there were components, and none of it could be replayed.
//!
//! [`SystemClock`] is the only place in the workspace that asks the operating
//! system the time; `make check-one-clock` refuses a second, and refuses a
//! component that constructs one rather than taking the clock it is given.
//! [`ManualClock`] is a test's, and [`ReplayClock`] answers with times that
//! were recorded, so what happened can be run again at the times it happened.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;

/// Where the time comes from: nanoseconds since the Unix epoch.
pub trait Clock: Send + Sync {
    fn now_ns(&self) -> i64;
}

/// The wall clock.
///
/// Constructed once per process, by the process's composition root, and
/// handed to everything in it.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ns(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos() as i64)
            .unwrap_or_default()
    }
}

/// A clock that moves only when told to, so a test of a bound does not wait
/// for it.
#[derive(Debug, Default)]
pub struct ManualClock(AtomicI64);

impl ManualClock {
    pub fn at(now_ns: i64) -> Self {
        Self(AtomicI64::new(now_ns))
    }

    pub fn set(&self, now_ns: i64) {
        self.0.store(now_ns, Ordering::SeqCst);
    }

    pub fn advance(&self, by_ns: i64) {
        self.0.fetch_add(by_ns, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ns(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// A clock that answers with recorded times, one per reading, in order.
///
/// For running a recording again: each reading the recording made is answered
/// with the time it got then. Once the recording is used up it holds at its
/// last time rather than inventing one, and before any time was recorded it
/// answers zero, which no real reading ever is.
#[derive(Debug, Default)]
pub struct ReplayClock {
    recorded: Mutex<VecDeque<i64>>,
    last: AtomicI64,
}

impl ReplayClock {
    pub fn new(recorded: impl IntoIterator<Item = i64>) -> Self {
        Self {
            recorded: Mutex::new(recorded.into_iter().collect()),
            last: AtomicI64::new(0),
        }
    }

    /// Recorded times not yet read. A replay that ends with some left read
    /// the clock fewer times than the recording did, which is a divergence.
    pub fn remaining(&self) -> usize {
        self.recorded.lock().expect("replay clock poisoned").len()
    }
}

impl Clock for ReplayClock {
    fn now_ns(&self) -> i64 {
        let mut recorded = self.recorded.lock().expect("replay clock poisoned");
        match recorded.pop_front() {
            Some(at) => {
                self.last.store(at, Ordering::SeqCst);
                at
            }
            None => self.last.load(Ordering::SeqCst),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn the_system_clock_reads_the_wall_clock() {
        let read = SystemClock.now_ns();
        // 2020-01-01T00:00:00Z: anything earlier is not a reading of today.
        assert!(read > 1_577_836_800_000_000_000, "{read}");
        assert!(SystemClock.now_ns() >= read);
    }

    #[test]
    fn a_manual_clock_moves_only_when_told() {
        let clock = ManualClock::at(1_000);
        assert_eq!(clock.now_ns(), 1_000);
        assert_eq!(clock.now_ns(), 1_000);
        clock.advance(500);
        assert_eq!(clock.now_ns(), 1_500);
        clock.set(7);
        assert_eq!(clock.now_ns(), 7);
    }

    #[test]
    fn a_replay_clock_answers_with_what_was_recorded_then_holds() {
        let clock = ReplayClock::new([10, 20, 30]);
        assert_eq!(clock.now_ns(), 10);
        assert_eq!(clock.remaining(), 2);
        assert_eq!(clock.now_ns(), 20);
        assert_eq!(clock.now_ns(), 30);
        assert_eq!(clock.remaining(), 0);
        assert_eq!(
            clock.now_ns(),
            30,
            "a used-up recording holds its last time"
        );
    }

    #[test]
    fn an_empty_replay_answers_zero() {
        assert_eq!(ReplayClock::new([]).now_ns(), 0);
    }

    #[test]
    fn one_clock_serves_many_holders() {
        // What a composition root does: one clock, shared. Every holder sees
        // the same time because there is only one.
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::at(42));
        let held = [Arc::clone(&clock), Arc::clone(&clock)];
        assert!(held.iter().all(|c| c.now_ns() == 42));
    }
}
