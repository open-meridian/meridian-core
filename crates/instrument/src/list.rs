//! The records to complete, the conflicts, and a record's history. W3.11 and
//! W3.12, for a deployment admin at the dashboard.
//!
//! The list names no account and no quantity: this store holds neither and
//! joins no other store (decisions/012). It counts the licensed identifiers
//! the deployment holds, per scheme, and never sends one anywhere.

use std::collections::{BTreeMap, BTreeSet};

use meridian_domain::v1::{
    InstrumentChange, InstrumentConflict, InstrumentToComplete, InstrumentVersion,
    LicensedIdentifierCount, ListInstrumentsToCompleteReply, ListInstrumentsToCompleteRequest,
    ReadInstrumentHistoryReply, ReadInstrumentHistoryRequest,
};
use meridian_symbology::LICENSED;

use crate::record::{asked_to_wire, complete, complete_for_book, field_to_wire, lacks, to_wire};
use crate::replace::current;
use crate::store::{Asked, Field, Instrument, Result, Store};

/// A page when the caller names none, and the most one may.
const PAGE: usize = 200;
const MOST: usize = 1000;

/// The longest search the list takes (W3.11, contract v20).
pub const MOST_SEARCH: usize = 100;

/// Whether a record answers a search: its ID, an identifier's value held
/// now (a symbol is one), or its description containing it.
fn matches(record: &Instrument, search: &str) -> bool {
    record.instrument_id.to_lowercase().contains(search)
        || record.description.to_lowercase().contains(search)
        || record
            .held_now()
            .any(|identifier| identifier.value.to_lowercase().contains(search))
}

/// W3.11. Those the book cannot use first, then the rest lacking something,
/// then (when asked) the complete; each by ID within its group.
pub fn list_to_complete(
    store: &dyn Store,
    request: &ListInstrumentsToCompleteRequest,
) -> Result<ListInstrumentsToCompleteReply> {
    // Contract v20 (W3.11): the picker's search, complete records included,
    // by an identifier's value, a symbol or the description, case aside.
    let search: String = request
        .search
        .trim()
        .chars()
        .take(MOST_SEARCH)
        .collect::<String>()
        .to_lowercase();
    // A replaced record answers the one it became (W3.6, W3.8): a resolved
    // LCL- ID reads its INS- record, the link on it.
    let became = if search.is_empty() && !request.provisional && !request.instrument_id.is_empty() {
        Some(current(store, &request.instrument_id)?)
    } else {
        None
    };
    let start: usize = request.cursor.parse().unwrap_or(0);
    let size = match request.page_size {
        n if n <= 0 => PAGE,
        n => (n as usize).min(MOST),
    };

    // The live records a chunk at a time (stage M; the store memory audit of
    // 2026-11-09, item 3): each counted as it is read, and only those the
    // page reaches kept -- the first `start + size` by the list's order --
    // never every record.
    let keep = start.saturating_add(size);
    let mut incomplete_for_book = 0i64;
    let mut incomplete = 0i64;
    let mut licensed: BTreeMap<&str, BTreeSet<String>> = LICENSED
        .iter()
        .map(|scheme| (*scheme, BTreeSet::new()))
        .collect();
    let mut tallies = crate::reask::Tallies::new(store);
    let mut chosen_count = 0usize;
    // Ordered as the list is: the exact first, then by description and ID,
    // for a search; otherwise those the book cannot use first, then the
    // rest lacking something, then the complete, each by ID.
    let mut best: BTreeMap<(u8, String, String), Instrument> = BTreeMap::new();
    store.each(&mut |record| {
        if store.replacement_of(&record.instrument_id)?.is_some() {
            return Ok(true);
        }
        if !complete_for_book(&record) {
            incomplete_for_book += 1;
        }
        if !complete(&record) {
            incomplete += 1;
        }
        for identifier in record.held_now() {
            if let Some(held) = licensed.get_mut(identifier.scheme.as_str()) {
                held.insert(identifier.value.clone());
            }
        }
        tallies.add(store, &record)?;
        let rank = 2 * u8::from(complete_for_book(&record)) + u8::from(complete(&record));
        let key = if !search.is_empty() {
            if !matches(&record, &search) {
                return Ok(true);
            }
            let exact = record.instrument_id.to_lowercase() == search
                || record
                    .held_now()
                    .any(|id| id.value.to_lowercase() == search);
            (
                u8::from(!exact),
                record.description.to_lowercase(),
                record.instrument_id.clone(),
            )
        } else if request.provisional {
            // Contract v24 (W3.11): the provisional records, complete or not.
            if !record.local.provisional {
                return Ok(true);
            }
            (rank, String::new(), record.instrument_id.clone())
        } else if let Some(became) = &became {
            if &record.instrument_id != became {
                return Ok(true);
            }
            (rank, String::new(), record.instrument_id.clone())
        } else {
            if !request.include_complete && complete(&record) {
                return Ok(true);
            }
            (rank, String::new(), record.instrument_id.clone())
        };
        chosen_count += 1;
        best.insert(key, record);
        if best.len() > keep {
            best.pop_last();
        }
        Ok(true)
    })?;
    let licensed_identifiers = LICENSED
        .iter()
        .filter_map(|scheme| {
            let held = licensed.get(scheme).map_or(0, BTreeSet::len);
            (held > 0).then(|| LicensedIdentifierCount {
                scheme: scheme.to_string(),
                count: held as i64,
            })
        })
        .collect();
    let chosen: Vec<&Instrument> = best.values().collect();

    let page: Vec<InstrumentToComplete> = chosen
        .iter()
        .skip(start)
        .take(size)
        .map(|record| InstrumentToComplete {
            instrument: Some(to_wire(record)),
            lacks: lacks(record)
                .into_iter()
                .map(|field| field_to_wire(field) as i32)
                .collect(),
            complete_for_book: complete_for_book(record),
            complete: complete(record),
            provisional_reason: if record.local.provisional {
                record.local.reason
            } else {
                0
            },
            // The migration's line (W3.26): its section, and the record the
            // platform answered it with, while the line is still to make.
            migration_section: if record.local.provisional {
                crate::reask::section_to_wire(&record.local.migration) as i32
            } else {
                0
            },
            answered_instrument_id: if record.local.provisional {
                record.local.migration_answer.clone()
            } else {
                String::new()
            },
            // Contract v25 (W3.26): what the platform's answer contradicts
            // of a person's, while the line is still to make.
            contradicted_fields: if record.local.provisional {
                record
                    .local
                    .contradicted_fields
                    .iter()
                    .filter_map(|name| crate::store::Field::parse(name))
                    .map(|field| field_to_wire(field) as i32)
                    .collect()
            } else {
                Vec::new()
            },
        })
        .collect();
    let next_cursor = if start + page.len() < chosen_count {
        (start + page.len()).to_string()
    } else {
        String::new()
    };

    let mut conflicts = Vec::new();
    for conflict in store.conflicts()? {
        // Settled once a merge left the records it named one.
        let mut now: Vec<String> = Vec::new();
        for id in &conflict.instrument_ids {
            let became = current(store, id)?;
            if !now.contains(&became) {
                now.push(became);
            }
        }
        if now.len() < 2 {
            continue;
        }
        if !request.instrument_id.is_empty() && !now.contains(&request.instrument_id) {
            continue;
        }
        now.sort();
        conflicts.push(InstrumentConflict {
            identifiers: conflict.identifiers.iter().map(asked_to_wire).collect(),
            instrument_ids: now,
            reported_by: conflict.reported_by,
            first_seen_ns: conflict.first_seen_ns,
            last_seen_ns: conflict.last_seen_ns,
            // Contract v19: ambiguous where a resolve met it.
            reason: conflict.reason,
        });
    }

    Ok(ListInstrumentsToCompleteReply {
        instruments: page,
        next_cursor,
        conflicts,
        incomplete_for_book,
        incomplete,
        licensed_identifiers,
        migration_counts: tallies.migration(),
        backfill_counts: tallies.backfill(),
    })
}

/// W3.12. A record's versions, newest first.
pub fn history(
    store: &dyn Store,
    request: &ReadInstrumentHistoryRequest,
) -> Result<ReadInstrumentHistoryReply> {
    let versions = store.history(&request.instrument_id)?;
    let start: usize = request.cursor.parse().unwrap_or(0);
    let size = match request.page_size {
        n if n <= 0 => PAGE,
        n => (n as usize).min(MOST),
    };
    let page: Vec<InstrumentVersion> = versions
        .iter()
        .skip(start)
        .take(size)
        .map(|version| InstrumentVersion {
            instrument_id: version.instrument_id.clone(),
            version: version.version,
            operation: version.operation.clone(),
            changes: version
                .changes
                .iter()
                .map(|change| {
                    let field = Field::parse(&change.field).unwrap_or(Field::Identifier);
                    InstrumentChange {
                        field: field_to_wire(field) as i32,
                        identifier: (field == Field::Identifier).then(|| {
                            asked_to_wire(&Asked {
                                scheme: change.scheme.clone(),
                                value: change.after.clone(),
                                source: change.namespace.clone(),
                            })
                        }),
                        before: change.before.clone(),
                        after: change.after.clone(),
                        source: change.source.clone(),
                    }
                })
                .collect(),
            person: version.person.clone(),
            instance_id: version.instance_id.clone(),
            // Contract v12: the delegation and client the person acted
            // through.
            acting_through_delegation: version.acting_through_delegation.clone(),
            client_name: version.client_name.clone(),
            prepared_call_id: version.prepared_call_id.clone(),
            note: version.note.clone(),
            record_time_ns: version.record_time_ns,
            merged_instrument_id: version.merged_instrument_id.clone(),
        })
        .collect();
    let next_cursor = if start + page.len() < versions.len() {
        (start + page.len()).to_string()
    } else {
        String::new()
    };
    Ok(ReadInstrumentHistoryReply {
        versions: page,
        next_cursor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve::resolve_identifier;
    use crate::MemoryStore;
    use meridian_domain::v1::{
        Identifier as PbIdentifier, InstrumentField, ResolveIdentifierRequest,
    };

    const NOW: i64 = 1_757_376_000_000_000_000;

    fn mint(store: &MemoryStore, scheme: &str, value: &str) -> String {
        resolve_identifier(
            store,
            &ResolveIdentifierRequest {
                identifiers: vec![PbIdentifier {
                    scheme: scheme.into(),
                    value: value.into(),
                    source: String::new(),
                    ..Default::default()
                }],
                as_of_ns: NOW,
                // The minimum a record needs (contract v24, W3.7).
                stated_description: "a security reported".into(),
                currency: "USD".into(),
                ..Default::default()
            },
            "custody-1",
            NOW,
        )
        .unwrap()
        .reply
        .instrument_id
    }

    #[test]
    fn a_search_finds_records_by_an_identifier_or_the_description_complete_ones_included() {
        let store = MemoryStore::new();
        let apple = mint(&store, "ticker", "AAPL");
        let _other = mint(&store, "ticker", "MSFT");
        let asked = |search: &str| {
            list_to_complete(
                &store,
                &ListInstrumentsToCompleteRequest {
                    search: search.into(),
                    ..Default::default()
                },
            )
            .unwrap()
            .instruments
            .iter()
            .map(|found| found.instrument.as_ref().unwrap().instrument_id.clone())
            .collect::<Vec<_>>()
        };
        assert_eq!(asked("aap"), vec![apple.clone()]);
        assert_eq!(asked(&apple), vec![apple.clone()], "an exact ID answers");
        assert!(asked("nothing-like-it").is_empty());
    }

    #[test]
    fn those_the_book_cannot_use_come_first_and_licensed_identifiers_are_counted() {
        let store = MemoryStore::new();
        let cusip = mint(&store, "cusip", "037833100");
        mint(&store, "isin", "US0378331005");
        let mut complete_one = store.by_id(&cusip).unwrap().unwrap();
        complete_one.asset_class = "ASSET_CLASS_EQUITY".into();
        complete_one.currency = "USD".into();
        complete_one.version = 2;
        store
            .write(
                complete_one,
                1,
                crate::store::Version {
                    acting_through_delegation: String::new(),
                    client_name: String::new(),
                    prepared_call_id: String::new(),
                    instrument_id: cusip.clone(),
                    version: 2,
                    operation: "complete".into(),
                    changes: Vec::new(),
                    person: "local|ada".into(),
                    instance_id: String::new(),
                    note: String::new(),
                    merged_instrument_id: String::new(),
                    record_time_ns: NOW,
                },
            )
            .unwrap();

        let listed =
            list_to_complete(&store, &ListInstrumentsToCompleteRequest::default()).unwrap();
        assert_eq!(listed.incomplete_for_book, 1);
        assert_eq!(listed.incomplete, 2);
        assert_eq!(listed.instruments.len(), 2);
        assert!(!listed.instruments[0].complete_for_book);
        assert!(listed.instruments[1].complete_for_book);
        assert_eq!(
            listed.instruments[1].lacks,
            vec![InstrumentField::Description as i32]
        );
        let counts: Vec<(String, i64)> = listed
            .licensed_identifiers
            .iter()
            .map(|count| (count.scheme.clone(), count.count))
            .collect();
        assert_eq!(counts, vec![("cusip".into(), 1), ("isin".into(), 1)]);

        let history = history(
            &store,
            &ReadInstrumentHistoryRequest {
                instrument_id: cusip,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            history
                .versions
                .iter()
                .map(|v| v.operation.as_str())
                .collect::<Vec<_>>(),
            vec!["complete", "mint"]
        );
    }
}
