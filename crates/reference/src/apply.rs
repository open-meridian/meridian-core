//! Writing a record into the replica, gated on its version.
//!
//! Everything inbound lands here: a record pulled because a resolve missed, one
//! returned by an escalation, one redelivered because a retry could not tell
//! whether the first attempt landed. One path, because a second write path is a
//! second place for the version gate to be forgotten.
//!
//! W3.5 puts it plainly: a pulled record is indistinguishable from a locally
//! defined one afterwards. That is what makes the resume cursor trustworthy —
//! the highest version held describes everything the replica knows, however it
//! arrived.
//!
//! # An unapplied record is still an event
//!
//! An apply that changes nothing still announces itself, with `applied` false.
//! Suppressing it would make the event stream depend on what the replica
//! happened to be holding, so a subscriber could not tell "we checked and it was
//! current" from "nobody checked". The first is a healthy replica confirming
//! itself; the second is a connection that stopped working.

use meridian_pb::v1::{
    Identifier as PbIdentifier, InstrumentAppliedEvent, InstrumentRecord as PbInstrument,
};

use crate::store::{Applied, Identifier, Instrument, Result, Store};

/// What an apply did, and what to announce about it.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub applied: Applied,
    pub event: InstrumentAppliedEvent,
}

impl Outcome {
    /// Whether the replica changed. False for a redelivery or a late arrival.
    pub fn changed(&self) -> bool {
        matches!(self.applied, Applied::Stored)
    }
}

/// Apply a record from the platform, and produce the event announcing it.
///
/// `now_ns` is passed in rather than read, so a test controls time.
pub fn apply(store: &dyn Store, record: PbInstrument, now_ns: i64) -> Result<Outcome> {
    let instrument = from_wire(&record);
    let applied = store.apply(instrument)?;

    Ok(Outcome {
        applied,
        event: InstrumentAppliedEvent {
            instrument: Some(record),
            applied: matches!(applied, Applied::Stored),
            applied_at_ns: now_ns,
        },
    })
}

/// The wire record as the replica holds it.
///
/// A deliberate translation rather than storing the generated type directly.
/// The store's shape is the replica's business and the wire's shape is the
/// contract's, and letting one be the other means a schema change reaches into
/// the store without passing anything that could object.
fn from_wire(record: &PbInstrument) -> Instrument {
    Instrument {
        instrument_id: record.instrument_id.clone(),
        identifiers: record
            .identifiers
            .iter()
            .map(|identifier| from_wire_identifier(identifier, record.valid_from_ns))
            .collect(),
        asset_class: record.asset_class.clone(),
        currency: record.currency.clone(),
        exchange_mic: record.exchange_mic.clone(),
        description: record.description.clone(),
        lifecycle_state: lifecycle_name(record.lifecycle_state),
        version: record.version,
        valid_from_ns: record.valid_from_ns,
        record_time_ns: record.record_time_ns,
    }
}

fn from_wire_identifier(identifier: &PbIdentifier, valid_from_ns: i64) -> Identifier {
    Identifier {
        scheme: identifier.scheme.clone(),
        value: identifier.value.clone(),
        source: identifier.source.clone(),

        // The wire carries no dates on an identifier: the record carries one
        // `valid_from_ns` for the whole set, being the moment this version's
        // mapping became true. So each identifier inherits it.
        //
        // Nothing invents an end. A mapping that has closed is expressed by the
        // identifier's absence from a later version, and the consequence of that
        // is recorded in design/replica-holds-one-version.
        valid_from_ns,
        valid_to_ns: None,
    }
}

fn lifecycle_name(value: i32) -> String {
    match meridian_pb::v1::InstrumentLifecycleState::try_from(value) {
        Ok(state) => state.as_str_name().to_string(),
        Err(_) => meridian_pb::v1::InstrumentLifecycleState::Unspecified
            .as_str_name()
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryStore;

    fn record(version: i64) -> PbInstrument {
        PbInstrument {
            instrument_id: "INS-01J8XQ4M7K0000000000AAPL".into(),
            identifiers: vec![PbIdentifier {
                scheme: "figi".into(),
                value: "BBG000B9XRY4".into(),
                source: String::new(),
            }],
            asset_class: "EQUITY".into(),
            currency: "USD".into(),
            exchange_mic: "XNAS".into(),
            description: "Apple Inc. common stock".into(),
            lifecycle_state: meridian_pb::v1::InstrumentLifecycleState::Active as i32,
            version,
            valid_from_ns: 1_757_376_000_000_000_000,
            record_time_ns: 1_757_376_000_000_000_000,
        }
    }

    #[test]
    fn a_new_record_is_stored_and_announced_as_applied() {
        let store = MemoryStore::new();
        let outcome = apply(&store, record(4), 1_000).unwrap();

        assert!(outcome.changed());
        assert!(outcome.event.applied);
        assert_eq!(outcome.event.applied_at_ns, 1_000);
        assert_eq!(
            store.version_of("INS-01J8XQ4M7K0000000000AAPL").unwrap(),
            Some(4)
        );
    }

    #[test]
    fn replaying_the_same_version_changes_nothing_and_says_so() {
        // The fixture's named case. Overlapping pulls are harmless, which is
        // what lets the retry policy stay simple.
        let store = MemoryStore::new();
        apply(&store, record(4), 1_000).unwrap();
        let again = apply(&store, record(4), 2_000).unwrap();

        assert!(!again.changed());
        assert!(!again.event.applied);
        assert_eq!(
            store.version_of("INS-01J8XQ4M7K0000000000AAPL").unwrap(),
            Some(4)
        );
    }

    #[test]
    fn a_late_arrival_does_not_undo_a_newer_record() {
        let store = MemoryStore::new();
        apply(&store, record(9), 1_000).unwrap();
        let late = apply(&store, record(3), 2_000).unwrap();

        assert!(!late.changed());
        assert_eq!(
            store.version_of("INS-01J8XQ4M7K0000000000AAPL").unwrap(),
            Some(9)
        );
    }

    #[test]
    fn an_unapplied_record_is_still_announced() {
        // So a subscriber can tell "we checked and it was current" from "nobody
        // checked". The first is a healthy replica; the second is a connection
        // that stopped working.
        let store = MemoryStore::new();
        apply(&store, record(4), 1_000).unwrap();
        let again = apply(&store, record(4), 2_000).unwrap();

        assert_eq!(again.event.applied_at_ns, 2_000);
        assert_eq!(
            again.event.instrument.as_ref().unwrap().instrument_id,
            "INS-01J8XQ4M7K0000000000AAPL"
        );
    }

    #[test]
    fn the_record_survives_the_journey_intact() {
        let store = MemoryStore::new();
        apply(&store, record(4), 1_000).unwrap();

        let held = store
            .by_id("INS-01J8XQ4M7K0000000000AAPL")
            .unwrap()
            .unwrap();
        assert_eq!(held.asset_class, "EQUITY");
        assert_eq!(held.currency, "USD");
        assert_eq!(held.exchange_mic, "XNAS");
        assert_eq!(held.description, "Apple Inc. common stock");
        assert_eq!(held.identifiers.len(), 1);
        assert_eq!(held.identifiers[0].value, "BBG000B9XRY4");
    }

    #[test]
    fn a_lifecycle_state_arrives_as_a_name_rather_than_a_number() {
        // A replica read by a person or a dashboard should not require a lookup
        // table to say what state something is in.
        let store = MemoryStore::new();
        apply(&store, record(4), 1_000).unwrap();
        let held = store
            .by_id("INS-01J8XQ4M7K0000000000AAPL")
            .unwrap()
            .unwrap();
        assert!(
            held.lifecycle_state.contains("ACTIVE"),
            "{}",
            held.lifecycle_state
        );
    }

    #[test]
    fn an_unknown_lifecycle_state_does_not_panic() {
        // A replica older than the platform will meet a state it has never
        // heard of. Refusing to hold the record would be worse than holding it
        // with a state nobody recognises.
        let store = MemoryStore::new();
        let mut future = record(4);
        future.lifecycle_state = 9_999;
        apply(&store, future, 1_000).unwrap();

        assert!(store
            .by_id("INS-01J8XQ4M7K0000000000AAPL")
            .unwrap()
            .is_some());
    }

    #[test]
    fn an_identifier_is_dated_from_the_record_that_carried_it() {
        // The wire dates the set, not each member. Leaving these at zero would
        // make every mapping look true since the beginning of time, which
        // quietly disables the dated resolution the whole design turns on.
        let store = MemoryStore::new();
        apply(&store, record(4), 1_000).unwrap();
        let held = store
            .by_id("INS-01J8XQ4M7K0000000000AAPL")
            .unwrap()
            .unwrap();
        assert_eq!(held.identifiers[0].valid_from_ns, 1_757_376_000_000_000_000);
    }

    #[test]
    fn an_identifier_does_not_resolve_before_the_version_that_introduced_it() {
        let store = MemoryStore::new();
        apply(&store, record(4), 1_000).unwrap();

        assert!(store
            .by_identifier("figi", "BBG000B9XRY4", "", 1_757_376_000_000_000_000 + 1)
            .unwrap()
            .is_some());
        assert!(store
            .by_identifier("figi", "BBG000B9XRY4", "", 1_000)
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_pulled_record_is_indistinguishable_from_any_other_afterwards() {
        // The fixture's postcondition, and what makes the resume cursor
        // trustworthy: the highest version held describes everything the replica
        // knows, however it arrived.
        let store = MemoryStore::new();
        apply(&store, record(4), 1_000).unwrap();
        let held = store
            .by_id("INS-01J8XQ4M7K0000000000AAPL")
            .unwrap()
            .unwrap();
        assert_eq!(held.version, 4);
        assert_eq!(store.count().unwrap(), 1);
    }
}
