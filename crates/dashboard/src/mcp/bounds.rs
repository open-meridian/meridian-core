//! The surface's bounds (W6.20, Q7), in the binary as decisions/015's are:
//! per delegation 60 calls a minute with a burst of 120 and 4 at once; per
//! plugin instance 8 calls at once from `/mcp`. An agent should not be able
//! to make a plugin's pages slow for the people using them.
//!
//! Over a bound the call is refused as a tool error saying when to try again
//! (`retry_after_seconds`), never an HTTP 429, so an unsupervised agent reads
//! it as it reads any refusal. Held in this process's memory: a restart
//! forgets a minute's count, which costs nothing a minute does not.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Calls a minute a delegation earns, and the most it may have saved.
pub const PER_MINUTE: f64 = 60.0;
pub const BURST: f64 = 120.0;
/// Calls a delegation may have in flight at once.
pub const AT_ONCE: usize = 4;
/// Calls from `/mcp` a plugin instance may have in flight at once.
pub const INSTANCE_AT_ONCE: usize = 8;
/// How long a call waiting on a full instance or delegation is told to wait.
pub const BUSY_SECONDS: u64 = 1;

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    at_ns: i64,
    in_flight: usize,
}

#[derive(Debug, Default)]
struct Held {
    delegations: HashMap<String, Bucket>,
    instances: HashMap<String, usize>,
}

/// The bounds, shared by every request this dashboard serves.
#[derive(Default)]
pub struct Bounds {
    held: Arc<Mutex<Held>>,
}

/// A call admitted: its place in flight, given back when it is dropped.
pub struct Admitted {
    held: Arc<Mutex<Held>>,
    delegation: String,
    instance: Option<String>,
}

impl Drop for Admitted {
    fn drop(&mut self) {
        let mut held = self.held.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(bucket) = held.delegations.get_mut(&self.delegation) {
            bucket.in_flight = bucket.in_flight.saturating_sub(1);
        }
        if let Some(instance) = &self.instance {
            if let Some(count) = held.instances.get_mut(instance) {
                *count = count.saturating_sub(1);
            }
        }
    }
}

impl Bounds {
    /// Admit a call on `delegation` at `now`, to `instance` when it is a
    /// plugin's; or the seconds to wait before trying again.
    pub fn admit(
        &self,
        now: i64,
        delegation: &str,
        instance: Option<&str>,
    ) -> Result<Admitted, u64> {
        let mut held = self.held.lock().unwrap_or_else(|p| p.into_inner());
        if held.delegations.len() > 1_000 {
            // Those idle long enough to have refilled hold nothing worth
            // keeping.
            held.delegations.retain(|_, bucket| {
                bucket.in_flight > 0 || (now - bucket.at_ns) < 2 * 60 * 1_000_000_000
            });
        }
        if let Some(instance) = instance {
            if held.instances.get(instance).copied().unwrap_or(0) >= INSTANCE_AT_ONCE {
                return Err(BUSY_SECONDS);
            }
        }
        let bucket = held
            .delegations
            .entry(delegation.to_string())
            .or_insert(Bucket {
                tokens: BURST,
                at_ns: now,
                in_flight: 0,
            });
        let earned = (now - bucket.at_ns).max(0) as f64 / 60e9 * PER_MINUTE;
        bucket.tokens = (bucket.tokens + earned).min(BURST);
        bucket.at_ns = now;
        if bucket.in_flight >= AT_ONCE {
            return Err(BUSY_SECONDS);
        }
        if bucket.tokens < 1.0 {
            let wait = ((1.0 - bucket.tokens) / PER_MINUTE * 60.0).ceil() as u64;
            return Err(wait.max(1));
        }
        bucket.tokens -= 1.0;
        bucket.in_flight += 1;
        if let Some(instance) = instance {
            *held.instances.entry(instance.to_string()).or_insert(0) += 1;
        }
        Ok(Admitted {
            held: Arc::clone(&self.held),
            delegation: delegation.to_string(),
            instance: instance.map(str::to_string),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delegation_bursts_to_120_then_earns_one_a_second() {
        let now = 1_000_000_000_000;
        let bounds = Bounds::default();
        for _ in 0..120 {
            drop(bounds.admit(now, "del-1", None).expect("within the burst"));
        }
        assert_eq!(bounds.admit(now, "del-1", None).err(), Some(1));
        let later = now + 1_000_000_000;
        drop(
            bounds
                .admit(later, "del-1", None)
                .expect("a second earns one"),
        );
        assert!(bounds.admit(later, "del-1", None).is_err());
        // Another delegation has its own.
        drop(bounds.admit(later, "del-2", None).expect("its own bucket"));
    }

    #[test]
    fn four_at_once_on_a_delegation_and_eight_on_an_instance() {
        let now = 1_000_000_000_000;
        let bounds = Bounds::default();
        let held: Vec<_> = (0..4)
            .map(|_| {
                bounds
                    .admit(now, "del-1", Some("ops-1"))
                    .expect("four at once")
            })
            .collect();
        assert_eq!(
            bounds.admit(now, "del-1", Some("ops-1")).err(),
            Some(BUSY_SECONDS)
        );
        let others: Vec<_> = (0..4)
            .map(|n| {
                bounds
                    .admit(now, &format!("del-{}", n + 2), Some("ops-1"))
                    .expect("eight on the instance")
            })
            .collect();
        assert_eq!(
            bounds.admit(now, "del-9", Some("ops-1")).err(),
            Some(BUSY_SECONDS)
        );
        // Another instance is not held by this one's.
        drop(
            bounds
                .admit(now, "del-9", Some("ops-2"))
                .expect("another instance"),
        );
        drop(held);
        drop(others);
        drop(
            bounds
                .admit(now, "del-1", Some("ops-1"))
                .expect("given back when done"),
        );
    }
}
