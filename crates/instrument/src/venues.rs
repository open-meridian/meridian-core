//! The venues the deployment holds (contract v18, W3.14, W3.15;
//! plans/the-lake-prices-the-book, ruling 2 of 2026-10-09).
//!
//! Each is the platform's venue master record, as the platform's answer to a
//! pull carried it (W3.5): kept at its version, never minted here. A plugin
//! asks which venue a set of identifiers names on a date -- a MIC under
//! `iso10383`, operating or segment, or a vendor's code as a `symbol` with its
//! source -- among the venues held; none, or more than one, is a miss. A
//! decommissioned venue still answers for a date it was in force: its
//! identifiers' dates say when, and an expired MIC is a decommissioned venue.

use meridian_domain::v1::{
    Identifier, MissReason, ResolveVenueReply, ResolveVenueRequest, VenueRecord,
};

use crate::store::{Result, Store};

/// The scheme an ISO 10383 MIC is an identifier under.
pub const ISO10383: &str = "iso10383";

fn names(venue: &VenueRecord, asked: &Identifier, as_of_ns: i64) -> bool {
    venue.valid_from_ns <= as_of_ns
        && venue.identifiers.iter().any(|held| {
            held.scheme == asked.scheme
                && held.value == asked.value
                && (asked.scheme == ISO10383 || held.source == asked.source)
        })
}

/// W3.14: the one venue held that the identifiers name on the date, or the
/// miss.
pub fn resolve_venue(
    store: &dyn Store,
    request: &ResolveVenueRequest,
    now_ns: i64,
) -> Result<ResolveVenueReply> {
    let as_of = if request.as_of_ns > 0 {
        request.as_of_ns
    } else {
        now_ns
    };
    let held = store.venues()?;
    // By its `VEN-` ID (contract v25; D9): the held record, as
    // ResolveInstrument answers by `instrument_id` (W3.6), the latest
    // version in force on the date.
    if !request.venue_id.is_empty() {
        let one = held
            .iter()
            .filter(|venue| venue.venue_id == request.venue_id && venue.valid_from_ns <= as_of)
            .max_by_key(|venue| venue.version);
        return Ok(match one {
            Some(venue) => ResolveVenueReply {
                found: true,
                venue: Some(venue.clone()),
                miss_reason: MissReason::Unspecified as i32,
            },
            None => ResolveVenueReply {
                found: false,
                venue: None,
                miss_reason: MissReason::NotFound as i32,
            },
        });
    }
    let mut found: Vec<&VenueRecord> = held
        .iter()
        .filter(|venue| {
            request
                .identifiers
                .iter()
                .any(|asked| names(venue, asked, as_of))
        })
        .collect();
    found.dedup_by(|a, b| a.venue_id == b.venue_id);
    Ok(match found.as_slice() {
        [one] => ResolveVenueReply {
            found: true,
            venue: Some((*one).clone()),
            miss_reason: MissReason::Unspecified as i32,
        },
        [] => ResolveVenueReply {
            found: false,
            venue: None,
            miss_reason: MissReason::NotFound as i32,
        },
        _ => ResolveVenueReply {
            found: false,
            venue: None,
            miss_reason: MissReason::Ambiguous as i32,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryStore;

    fn venue(id: &str, version: i64, mic: &str) -> VenueRecord {
        VenueRecord {
            venue_id: id.into(),
            name: format!("{id} name"),
            version,
            valid_from_ns: 10,
            identifiers: vec![
                Identifier {
                    scheme: ISO10383.into(),
                    value: mic.into(),
                    source: String::new(),
                    ..Default::default()
                },
                Identifier {
                    scheme: "symbol".into(),
                    value: "NYSE".into(),
                    source: "alpaca".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    fn ask(scheme: &str, value: &str, source: &str, as_of_ns: i64) -> ResolveVenueRequest {
        ResolveVenueRequest {
            identifiers: vec![Identifier {
                scheme: scheme.into(),
                value: value.into(),
                source: source.into(),
                ..Default::default()
            }],
            as_of_ns,
            venue_id: String::new(),
        }
    }

    #[test]
    fn a_venue_held_is_named_by_its_mic_or_a_vendors_code_on_a_date() {
        let store = MemoryStore::new();
        assert!(store.keep_venue(&venue("VEN-A", 2, "XNYS"), 1).unwrap());
        assert!(
            !store.keep_venue(&venue("VEN-A", 1, "XOLD"), 1).unwrap(),
            "an older version is not kept"
        );
        let found = resolve_venue(&store, &ask(ISO10383, "XNYS", "", 20), 30).unwrap();
        assert_eq!(found.venue.unwrap().venue_id, "VEN-A");
        let vendor = resolve_venue(&store, &ask("symbol", "NYSE", "alpaca", 20), 30).unwrap();
        assert!(vendor.found);
        let other_vendor =
            resolve_venue(&store, &ask("symbol", "NYSE", "tradier", 20), 30).unwrap();
        assert_eq!(other_vendor.miss_reason, MissReason::NotFound as i32);
        let before = resolve_venue(&store, &ask(ISO10383, "XNYS", "", 5), 30).unwrap();
        assert!(!before.found, "not in force yet");
        store.keep_venue(&venue("VEN-B", 1, "XNYS"), 1).unwrap();
        let both = resolve_venue(&store, &ask(ISO10383, "XNYS", "", 20), 30).unwrap();
        assert_eq!(both.miss_reason, MissReason::Ambiguous as i32);
    }

    #[test]
    fn a_venue_held_is_answered_by_its_id() {
        // Contract v25 (D9): the dashboard reads a calendar's venue by its ID.
        let store = MemoryStore::new();
        store.keep_venue(&venue("VEN-A", 2, "XNYS"), 1).unwrap();
        let by_id = |id: &str, as_of_ns: i64| ResolveVenueRequest {
            venue_id: id.into(),
            as_of_ns,
            ..Default::default()
        };
        let found = resolve_venue(&store, &by_id("VEN-A", 20), 30).unwrap();
        let venue = found.venue.unwrap();
        assert_eq!((venue.venue_id.as_str(), venue.version), ("VEN-A", 2));
        assert!(venue.identifiers.iter().any(|i| i.value == "XNYS"));
        let unheld = resolve_venue(&store, &by_id("VEN-Z", 20), 30).unwrap();
        assert_eq!(unheld.miss_reason, MissReason::NotFound as i32);
        let before = resolve_venue(&store, &by_id("VEN-A", 5), 30).unwrap();
        assert!(!before.found, "not in force yet");
    }
}
