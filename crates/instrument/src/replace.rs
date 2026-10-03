//! A record replaced by another, and following what it became. W3.8.
//!
//! From contract v10 a record is replaced only when a person merges it into
//! another they say is the same security (W3.13): one local record by another,
//! which decisions/030's "added, never substituted" does not touch, since that
//! is about a global ID. Before v10 a placeholder was replaced by the INS- ID
//! the platform paired it with; those replacements stand and are followed the
//! same way.
//!
//! # Replaced, never deleted
//!
//! Holding rows and book entries keep the ID they were recorded with, because
//! they record what was reported. A reader holding one must still be able to
//! learn what it became, so a replaced ID answers its replacement from then on:
//! a resolve that would have met it meets the one that stays, and resolving it
//! answers that record.
//!
//! Replacements are followed, not looked up once: a placeholder replaced by a
//! stub an older platform minted, which the platform later moved to `INS-`, and
//! that record merged into another, is three hops, and a reader of the first
//! wants the last.

use meridian_domain::v1::InstrumentReplacedEvent;

use crate::record::to_wire;
use crate::store::{Instrument, Result, Store};

/// How many replacements are followed from one ID before stopping.
///
/// A correct history makes a few; the rest is room, and the bound is there so
/// a cycle written by a defect somewhere else ends a resolve rather than
/// hanging it.
const LONGEST_CHAIN: usize = 8;

/// What `instrument_id` has become: itself, unless it was replaced.
pub fn current(store: &dyn Store, instrument_id: &str) -> Result<String> {
    let mut current = instrument_id.to_string();
    let mut seen = vec![current.clone()];

    for _ in 0..LONGEST_CHAIN {
        match store.replacement_of(&current)? {
            Some(next) if !seen.contains(&next) => {
                seen.push(next.clone());
                current = next;
            }
            _ => break,
        }
    }
    Ok(current)
}

/// W3.8. The event saying `replaced_id` is now `stays`, for the stores keyed
/// by instrument to move what they hold (W3.9, W9.9).
pub fn replaced(replaced_id: &str, stays: &Instrument, now_ns: i64) -> InstrumentReplacedEvent {
    InstrumentReplacedEvent {
        replaced_instrument_id: replaced_id.to_string(),
        instrument: Some(to_wire(stays)),
        replaced_at_ns: now_ns,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryStore;

    const NOW: i64 = 1_757_376_000_000_000_000;

    #[test]
    fn a_chain_of_replacements_is_followed_to_its_end() {
        // A placeholder replaced by an older platform's LCL- stub, which the
        // platform later moved to INS-, which a person then merged.
        let store = MemoryStore::new();
        store.replace("LCL-PLACEHOLDER", "LCL-STUB", NOW).unwrap();
        store.replace("LCL-STUB", "INS-FINAL", NOW).unwrap();
        store.replace("INS-FINAL", "LCL-KEPT", NOW).unwrap();

        assert_eq!(current(&store, "LCL-PLACEHOLDER").unwrap(), "LCL-KEPT");
        assert_eq!(current(&store, "LCL-KEPT").unwrap(), "LCL-KEPT");
    }

    #[test]
    fn a_cycle_ends_rather_than_hangs() {
        let store = MemoryStore::new();
        store.replace("LCL-A", "LCL-B", NOW).unwrap();
        store.replace("LCL-B", "LCL-A", NOW).unwrap();

        assert_eq!(current(&store, "LCL-A").unwrap(), "LCL-B");
    }
}
