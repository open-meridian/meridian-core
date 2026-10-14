//! What each instrument ID became (contract v24; W11's invariant, "a
//! replaced instrument is the record it became"; the one-ID spec's
//! requirement 8).
//!
//! The book hears every replacement (W9.9) and sweeps what it holds and
//! what its open orders name; each one is kept here, in this process, so
//! that every check of "the same instrument" on the order path compares the
//! record each ID became, and a write naming a replaced record lands on the
//! record it became. Nothing is rewritten: orders, placements and
//! executions keep the ID they were made with. Rebuilt by the sweep at each
//! start.

use std::collections::BTreeMap;
use std::sync::{OnceLock, RwLock};

fn held() -> &'static RwLock<BTreeMap<String, String>> {
    static HELD: OnceLock<RwLock<BTreeMap<String, String>>> = OnceLock::new();
    HELD.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// Keep that `replaced` is now `by`.
pub fn record(replaced: &str, by: &str) {
    if replaced.is_empty() || by.is_empty() || replaced == by {
        return;
    }
    if let Ok(mut map) = held().write() {
        map.insert(replaced.to_string(), by.to_string());
    }
}

/// What `instrument` became: itself, unless it was replaced; replacements
/// followed to their end, a few hops at most.
pub fn current(instrument: &str) -> String {
    let mut now = instrument.to_string();
    let Ok(map) = held().read() else {
        return now;
    };
    for _ in 0..8 {
        match map.get(&now) {
            Some(next) if *next != now => now = next.clone(),
            _ => break,
        }
    }
    now
}

/// Whether two IDs name one instrument, through the replacement.
pub fn same(a: &str, b: &str) -> bool {
    a == b || current(a) == current(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resolved_record_and_the_one_it_became_are_one_instrument() {
        record("LCL-BECAME-1", "INS-BECAME-1");
        record("INS-BECAME-1", "INS-BECAME-2");
        assert_eq!(current("LCL-BECAME-1"), "INS-BECAME-2");
        assert!(same("LCL-BECAME-1", "INS-BECAME-2"));
        assert!(same("INS-BECAME-1", "LCL-BECAME-1"));
        assert!(!same("LCL-BECAME-1", "INS-OTHER"));
    }
}
