//! A person completes the deployment's records; the platform's answer is kept
//! beside them. W3.10, W3.13 and W3.5 (contract v10).
//!
//! Every value a person sets carries its source in words and the person core
//! stamped from the command's envelope; a change to a value already held also
//! carries a note saying why (no second person on day one, the spec's Q9).
//! Each record is set against the version it was read at, so an identifier a
//! plugin joined while the page was open is never written over: the record is
//! refused alone, and the person sees it again.
//!
//! The platform's answer to a person's ask (W3.3) is never applied as a record
//! of its own: its INS- ID joins the record as an identifier -- added, never
//! substituted (decisions/030, choice 2) -- and its values are offers.

use meridian_domain::v1::{
    instrument_value, AssetClass, CompleteInstrumentsRequest, InstrumentCompletion,
    InstrumentCompletionResult, InstrumentRecord as PbInstrument, MergeInstrumentsRequest,
};
use meridian_pb::v1::{Refusal, RefusalReason};
use meridian_symbology::{GLOBAL_ID, OPEN};

use crate::record::{
    asked_from_wire, asset_class_name, class_words, dated, field_from_wire, is_currency, to_wire,
    CURRENCY_SCHEME,
};
use crate::replace::current;
use crate::resolve::{identifier_change, keep_offers};
use crate::store::{
    Asked, Change, Conflict, Field, Instrument, Offer, Result, Source, Store, Version, Written,
};

/// The most records one completion names (the spec's requirement 5).
pub const MOST_RECORDS: usize = 500;

/// A whole command refused, with its code and words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub reason: RefusalReason,
    pub words: String,
}

impl Refused {
    fn new(reason: RefusalReason, words: impl Into<String>) -> Self {
        Self {
            reason,
            words: words.into(),
        }
    }
}

/// What a completion did: each record's result, and the records at their new
/// versions, which the caller announces (W3.5).
#[derive(Debug, Clone, Default)]
pub struct Completed {
    pub results: Vec<InstrumentCompletionResult>,
    pub changed: Vec<Instrument>,
}

/// W3.10. Set the values of up to 500 records, each against its version,
/// each refused alone.
pub fn complete(
    store: &dyn Store,
    request: &CompleteInstrumentsRequest,
    person: &str,
    now_ns: i64,
) -> Result<std::result::Result<Completed, Refused>> {
    if person.is_empty() {
        return Ok(Err(Refused::new(
            RefusalReason::ActorRequired,
            "a person completes an instrument record, and none was named",
        )));
    }
    if request.completions.len() > MOST_RECORDS {
        return Ok(Err(Refused::new(
            RefusalReason::Unspecified,
            format!("a completion names at most {MOST_RECORDS} records"),
        )));
    }
    let mut completed = Completed::default();
    for completion in &request.completions {
        let (result, changed) = complete_one(store, completion, person, now_ns)?;
        completed.results.push(result);
        completed.changed.extend(changed);
    }
    Ok(Ok(completed))
}

/// One record's completion: its result, and the record when it changed.
fn complete_one(
    store: &dyn Store,
    completion: &InstrumentCompletion,
    person: &str,
    now_ns: i64,
) -> Result<(InstrumentCompletionResult, Option<Instrument>)> {
    let refuse =
        |reason: RefusalReason, fields: Vec<String>, detail: String| InstrumentCompletionResult {
            instrument_id: completion.instrument_id.clone(),
            instrument: None,
            refusal: Some(Refusal {
                reason: reason as i32,
                fields,
            }),
            detail,
        };

    let became = current(store, &completion.instrument_id)?;
    let held = if became == completion.instrument_id {
        store.by_id(&became)?
    } else {
        None
    };
    let Some(held) = held else {
        let detail = if became != completion.instrument_id {
            format!(
                "{} was merged into {became}; complete that record",
                completion.instrument_id
            )
        } else {
            format!(
                "the deployment holds no record {}",
                completion.instrument_id
            )
        };
        return Ok((
            refuse(
                RefusalReason::RecordChanged,
                vec!["instrument_id".into()],
                detail,
            ),
            None,
        ));
    };
    if held.version != completion.against_version {
        return Ok((
            refuse(
                RefusalReason::RecordChanged,
                vec!["against_version".into()],
                format!(
                    "{} changed since version {}; it is at version {} now",
                    held.instrument_id, completion.against_version, held.version
                ),
            ),
            None,
        ));
    }

    let mut record = held.clone();
    let mut changes: Vec<Change> = Vec::new();
    let mut problems: Vec<(RefusalReason, String, String)> = Vec::new();
    let mut changes_held = false;

    for (at, value) in completion.values.iter().enumerate() {
        let path = |name: &str| format!("values[{at}].{name}");
        let source = value.source.trim();
        if source.is_empty() {
            problems.push((
                RefusalReason::Incomplete,
                path("source"),
                "every value says where it came from".into(),
            ));
            continue;
        }
        let Some(set) = value.value.as_ref() else {
            problems.push((
                RefusalReason::Incomplete,
                path("value"),
                "a value names what it sets".into(),
            ));
            continue;
        };
        let (field, text, identifier) = match set {
            instrument_value::Value::AssetClass(class) => match AssetClass::try_from(*class) {
                Ok(class) if class != AssetClass::Unspecified => {
                    (Field::AssetClass, asset_class_name(class as i32), None)
                }
                _ => {
                    problems.push((
                        RefusalReason::Incomplete,
                        path("asset_class"),
                        "an asset class is one of the list: equity, debt, fund, derivative, \
                             crypto_asset, event_contract, cash"
                            .into(),
                    ));
                    continue;
                }
            },
            instrument_value::Value::Currency(code) => {
                let code = code.trim();
                if !is_currency(code) {
                    problems.push((
                        RefusalReason::Incomplete,
                        path("currency"),
                        format!("{code:?} is no ISO 4217 code"),
                    ));
                    continue;
                }
                (Field::Currency, code.to_string(), None)
            }
            instrument_value::Value::Description(text) => {
                let text = text.trim();
                if text.is_empty() {
                    problems.push((
                        RefusalReason::Incomplete,
                        path("description"),
                        "a description says something".into(),
                    ));
                    continue;
                }
                (Field::Description, text.to_string(), None)
            }
            instrument_value::Value::Identifier(identifier) => {
                let asked = asked_from_wire(identifier);
                if asked.scheme.is_empty() || asked.value.is_empty() {
                    problems.push((
                        RefusalReason::Incomplete,
                        path("identifier"),
                        "an identifier names its scheme and its value".into(),
                    ));
                    continue;
                }
                (Field::Identifier, asked.value.clone(), Some(asked))
            }
        };

        if let Some(asked) = identifier {
            if record.carries(&asked) {
                continue;
            }
            let others = held_elsewhere(store, &asked, &record.instrument_id)?;
            if !others.is_empty() {
                let mut ids = vec![record.instrument_id.clone()];
                ids.extend(others.iter().cloned());
                note_conflict(store, vec![asked.clone()], ids, now_ns)?;
                problems.push((
                    RefusalReason::IdentifierHeld,
                    path("identifier"),
                    format!(
                        "{}:{} is on {} already; merge the records if they are one security",
                        asked.scheme,
                        asked.value,
                        others.join(", ")
                    ),
                ));
                continue;
            }
            record.identifiers.push(dated(&asked));
            record.set_source(Source {
                field: Field::Identifier,
                identifier: Some(asked.clone()),
                source: source.to_string(),
                person: person.to_string(),
                instance_id: String::new(),
                recorded_at_ns: now_ns,
                note: completion.note.trim().to_string(),
            });
            changes.push(identifier_change(&asked, source));
            continue;
        }

        let before = record.value(field).to_string();
        if before == text {
            continue;
        }
        if !before.is_empty() {
            changes_held = true;
        }
        record.set_value(field, text.clone());
        record.set_source(Source {
            field,
            identifier: None,
            source: source.to_string(),
            person: person.to_string(),
            instance_id: String::new(),
            recorded_at_ns: now_ns,
            note: completion.note.trim().to_string(),
        });
        let words = |value: &str| {
            if field == Field::AssetClass {
                class_words(value)
            } else {
                value.to_string()
            }
        };
        changes.push(Change {
            field: field.name().into(),
            scheme: String::new(),
            namespace: String::new(),
            before: words(&before),
            after: words(&text),
            source: source.to_string(),
        });
    }

    if changes_held && completion.note.trim().is_empty() {
        problems.push((
            RefusalReason::ReasonRequired,
            "note".into(),
            "a change to a value already held says why".into(),
        ));
    }

    // Cash is a currency's instrument: its currency is its ISO 4217 code.
    if record.asset_class == asset_class_name(AssetClass::Cash as i32)
        && !record.currency.is_empty()
    {
        if let Some(code) = record
            .identifiers
            .iter()
            .find(|identifier| identifier.scheme == CURRENCY_SCHEME)
            .map(|identifier| identifier.value.clone())
        {
            if code != record.currency {
                let at = completion
                    .values
                    .iter()
                    .position(|value| {
                        matches!(value.value, Some(instrument_value::Value::Currency(_)))
                    })
                    .map(|at| format!("values[{at}].currency"))
                    .unwrap_or_else(|| "currency".into());
                problems.push((
                    RefusalReason::Incomplete,
                    at,
                    format!(
                        "a cash record's currency is its ISO 4217 code, {code}, not {}",
                        record.currency
                    ),
                ));
            }
        }
    }

    if !problems.is_empty() {
        let reason = [
            RefusalReason::IdentifierHeld,
            RefusalReason::ReasonRequired,
            RefusalReason::Incomplete,
        ]
        .into_iter()
        .find(|wanted| problems.iter().any(|(reason, _, _)| reason == wanted))
        .unwrap_or(RefusalReason::Incomplete);
        let fields = problems.iter().map(|(_, field, _)| field.clone()).collect();
        let detail = problems
            .iter()
            .map(|(_, _, words)| words.clone())
            .collect::<Vec<_>>()
            .join("; ");
        return Ok((refuse(reason, fields, detail), None));
    }

    if changes.is_empty() {
        return Ok((accepted(&completion.instrument_id, to_wire(&held)), None));
    }

    // An offer of what is now in force is no longer an offer.
    let in_force = record.clone();
    record.offers.retain(|offer| match offer.field {
        Field::Identifier => !offer
            .identifier
            .as_ref()
            .is_some_and(|identifier| in_force.carries(identifier)),
        field => in_force.value(field) != offer.value,
    });
    record.version = held.version + 1;
    record.record_time_ns = now_ns;
    let entry = Version {
        instrument_id: record.instrument_id.clone(),
        version: record.version,
        operation: "complete".into(),
        changes,
        person: person.to_string(),
        instance_id: String::new(),
        note: completion.note.trim().to_string(),
        merged_instrument_id: String::new(),
        record_time_ns: now_ns,
    };
    match store.write(record.clone(), held.version, entry)? {
        Written::Stored => Ok((
            accepted(&record.instrument_id, to_wire(&record)),
            Some(record),
        )),
        Written::Stale { held } => Ok((
            refuse(
                RefusalReason::RecordChanged,
                vec!["against_version".into()],
                format!(
                    "{} changed while it was being completed; it is at version {held} now",
                    record.instrument_id
                ),
            ),
            None,
        )),
        Written::Missing => Ok((
            refuse(
                RefusalReason::RecordChanged,
                vec!["instrument_id".into()],
                format!("the deployment holds no record {}", record.instrument_id),
            ),
            None,
        )),
    }
}

fn accepted(instrument_id: &str, record: PbInstrument) -> InstrumentCompletionResult {
    InstrumentCompletionResult {
        instrument_id: instrument_id.to_string(),
        instrument: Some(record),
        refusal: None,
        detail: String::new(),
    }
}

/// What a merge did: the record that stays, and the one replaced by it, both
/// at their new versions.
#[derive(Debug, Clone)]
pub struct Merged {
    pub stays: Instrument,
    pub merged: Instrument,
}

/// W3.13. Merge one record into another a person says is the same security.
/// The caller records the replacement and announces it (W3.8).
pub fn merge(
    store: &dyn Store,
    request: &MergeInstrumentsRequest,
    person: &str,
    now_ns: i64,
) -> Result<std::result::Result<Merged, Refused>> {
    if person.is_empty() {
        return Ok(Err(Refused::new(
            RefusalReason::ActorRequired,
            "a person merges instrument records, and none was named",
        )));
    }
    let note = request.note.trim();
    if note.is_empty() {
        return Ok(Err(Refused::new(
            RefusalReason::ReasonRequired,
            "a merge says why the two records are one security",
        )));
    }
    let (kept_id, merged_id) = (&request.kept_instrument_id, &request.merged_instrument_id);
    let refused_unspecified = || {
        Refused::new(
            RefusalReason::Unspecified,
            "a record merges only into another record that has not been replaced",
        )
    };
    if kept_id == merged_id
        || store.replacement_of(kept_id)?.is_some()
        || store.replacement_of(merged_id)?.is_some()
    {
        return Ok(Err(refused_unspecified()));
    }
    let (Some(kept), Some(merged)) = (store.by_id(kept_id)?, store.by_id(merged_id)?) else {
        return Ok(Err(Refused::new(
            RefusalReason::Unspecified,
            "the deployment holds no such record",
        )));
    };
    if kept.version != request.kept_version || merged.version != request.merged_version {
        return Ok(Err(Refused::new(
            RefusalReason::RecordChanged,
            format!(
                "a record changed since it was read: {kept_id} is at version {}, {merged_id} at {}",
                kept.version, merged.version
            ),
        )));
    }

    let take: Vec<Field> = request
        .take_from_merged
        .iter()
        .filter_map(|field| field_from_wire(*field))
        .collect();
    let mut stays = kept.clone();
    let mut changes: Vec<Change> = Vec::new();

    for identifier in &merged.identifiers {
        let asked = identifier.asked();
        if stays.carries(&asked) {
            continue;
        }
        stays.identifiers.push(identifier.clone());
        let carried = merged
            .source_of(Field::Identifier, Some(&asked))
            .cloned()
            .unwrap_or(Source {
                field: Field::Identifier,
                identifier: Some(asked.clone()),
                source: format!("merged from {merged_id}"),
                person: person.to_string(),
                instance_id: String::new(),
                recorded_at_ns: now_ns,
                note: String::new(),
            });
        changes.push(identifier_change(&asked, &carried.source));
        stays.set_source(carried);
    }
    for field in [Field::AssetClass, Field::Currency, Field::Description] {
        let (ours, theirs) = (kept.value(field), merged.value(field));
        if theirs.is_empty() || ours == theirs {
            continue;
        }
        if !ours.is_empty() && !take.contains(&field) {
            continue;
        }
        stays.set_value(field, theirs.to_string());
        let carried = merged.source_of(field, None).cloned().unwrap_or(Source {
            field,
            identifier: None,
            source: format!("merged from {merged_id}"),
            person: person.to_string(),
            instance_id: String::new(),
            recorded_at_ns: now_ns,
            note: note.to_string(),
        });
        let words = |value: &str| {
            if field == Field::AssetClass {
                class_words(value)
            } else {
                value.to_string()
            }
        };
        changes.push(Change {
            field: field.name().into(),
            scheme: String::new(),
            namespace: String::new(),
            before: words(ours),
            after: words(theirs),
            source: carried.source.clone(),
        });
        stays.set_source(carried);
    }
    keep_offers(&mut stays, merged.offers.clone());

    stays.version = kept.version + 1;
    stays.record_time_ns = now_ns;
    let stays_entry = Version {
        instrument_id: stays.instrument_id.clone(),
        version: stays.version,
        operation: "merge".into(),
        changes,
        person: person.to_string(),
        instance_id: String::new(),
        note: note.to_string(),
        merged_instrument_id: merged_id.clone(),
        record_time_ns: now_ns,
    };
    match store.write(stays.clone(), kept.version, stays_entry)? {
        Written::Stored => {}
        Written::Stale { .. } | Written::Missing => {
            return Ok(Err(Refused::new(
                RefusalReason::RecordChanged,
                format!("{kept_id} changed while it was being merged"),
            )))
        }
    }

    let mut gone = merged.clone();
    gone.version = merged.version + 1;
    gone.record_time_ns = now_ns;
    let gone_entry = Version {
        instrument_id: gone.instrument_id.clone(),
        version: gone.version,
        operation: "merged-into".into(),
        changes: Vec::new(),
        person: person.to_string(),
        instance_id: String::new(),
        note: note.to_string(),
        merged_instrument_id: kept_id.clone(),
        record_time_ns: now_ns,
    };
    // The record that stays is written: the merge has happened whatever this
    // says, and the replacement recorded next is what every reader follows.
    if store.write(gone.clone(), merged.version, gone_entry)? != Written::Stored {
        tracing::warn!(
            merged = merged_id,
            "a merged record changed as it was merged; its history lacks the merge"
        );
    }
    Ok(Ok(Merged {
        stays,
        merged: gone,
    }))
}

/// W3.5. Keep the platform's answer to a person's ask on the record it names:
/// its INS- ID joins as an identifier, and its values are offers. The record
/// at its new version, when anything changed.
pub fn keep_platform_answer(
    store: &dyn Store,
    for_instrument_id: &str,
    answer: &PbInstrument,
    now_ns: i64,
) -> Result<Option<Instrument>> {
    let became = current(store, for_instrument_id)?;
    let Some(held) = store.by_id(&became)? else {
        return Ok(None);
    };
    let words = format!(
        "the platform, record {} version {}",
        answer.instrument_id, answer.version
    );
    let mut record = held.clone();
    let mut changes = Vec::new();

    if !answer.instrument_id.is_empty() {
        let global = Asked {
            scheme: GLOBAL_ID.into(),
            value: answer.instrument_id.clone(),
            source: String::new(),
        };
        let others = held_elsewhere(store, &global, &record.instrument_id)?;
        let keyed_elsewhere = store.by_id(&answer.instrument_id)?.is_some()
            && current(store, &answer.instrument_id)? != record.instrument_id;
        if !others.is_empty() || keyed_elsewhere {
            let mut ids = vec![record.instrument_id.clone()];
            ids.extend(others);
            if keyed_elsewhere {
                ids.push(current(store, &answer.instrument_id)?);
            }
            note_conflict(store, vec![global], ids, now_ns)?;
        } else if !record.carries(&global) {
            record.identifiers.push(dated(&global));
            record.set_source(Source {
                field: Field::Identifier,
                identifier: Some(global.clone()),
                source: words.clone(),
                person: String::new(),
                instance_id: String::new(),
                recorded_at_ns: now_ns,
                note: String::new(),
            });
            changes.push(identifier_change(&global, &words));
        }
    }

    let offer = |field: Field, value: String, identifier: Option<Asked>| Offer {
        field,
        value,
        identifier,
        source: words.clone(),
        instance_id: String::new(),
        offered_at_ns: now_ns,
    };
    let mut offers = Vec::new();
    let class = asset_class_name(answer.asset_class);
    if !class.is_empty() {
        offers.push(offer(Field::AssetClass, class, None));
    }
    if is_currency(&answer.currency) {
        offers.push(offer(Field::Currency, answer.currency.clone(), None));
    }
    if !answer.description.trim().is_empty() {
        offers.push(offer(
            Field::Description,
            answer.description.trim().to_string(),
            None,
        ));
    }
    for identifier in &answer.identifiers {
        let asked = asked_from_wire(identifier);
        if asked.source.is_empty()
            && OPEN.contains(&asked.scheme.as_str())
            && asked.scheme != GLOBAL_ID
        {
            offers.push(offer(Field::Identifier, asked.value.clone(), Some(asked)));
        }
    }
    let offered = keep_offers(&mut record, offers);
    if changes.is_empty() && !offered {
        return Ok(None);
    }

    record.version = held.version + 1;
    record.record_time_ns = now_ns;
    let entry = Version {
        instrument_id: record.instrument_id.clone(),
        version: record.version,
        operation: "platform".into(),
        changes,
        person: String::new(),
        instance_id: String::new(),
        note: String::new(),
        merged_instrument_id: String::new(),
        record_time_ns: now_ns,
    };
    match store.write(record.clone(), held.version, entry)? {
        Written::Stored => Ok(Some(record)),
        Written::Stale { .. } | Written::Missing => Ok(None),
    }
}

/// The records other than `instrument_id`, as they are now, carrying the
/// identifier whatever its dates.
fn held_elsewhere(store: &dyn Store, asked: &Asked, instrument_id: &str) -> Result<Vec<String>> {
    let mut others = Vec::new();
    for record in store.matching(&asked.scheme, &asked.value, &asked.source, i64::MAX)? {
        let became = current(store, &record.instrument_id)?;
        if became != instrument_id && !others.contains(&became) {
            others.push(became);
        }
    }
    Ok(others)
}

fn note_conflict(
    store: &dyn Store,
    identifiers: Vec<Asked>,
    mut instrument_ids: Vec<String>,
    now_ns: i64,
) -> Result<()> {
    instrument_ids.sort();
    instrument_ids.dedup();
    store.note_conflict(Conflict {
        identifiers,
        instrument_ids,
        reported_by: String::new(),
        first_seen_ns: now_ns,
        last_seen_ns: now_ns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve::resolve_identifier;
    use crate::MemoryStore;
    use meridian_domain::v1::{
        Identifier as PbIdentifier, InstrumentField, InstrumentValue, ResolveIdentifierRequest,
    };

    const NOW: i64 = 1_757_376_000_000_000_000;
    const ADA: &str = "local|ada";

    fn minted(store: &MemoryStore, scheme: &str, value: &str, source: &str) -> String {
        resolve_identifier(
            store,
            &ResolveIdentifierRequest {
                identifiers: vec![PbIdentifier {
                    scheme: scheme.into(),
                    value: value.into(),
                    source: source.into(),
                }],
                as_of_ns: NOW,
                ..Default::default()
            },
            "custody-snaptrade-1",
            NOW,
        )
        .unwrap()
        .reply
        .instrument_id
    }

    fn value(value: instrument_value::Value, source: &str) -> InstrumentValue {
        InstrumentValue {
            value: Some(value),
            source: source.into(),
        }
    }

    fn completing(
        instrument_id: &str,
        against: i64,
        values: Vec<InstrumentValue>,
        note: &str,
    ) -> CompleteInstrumentsRequest {
        CompleteInstrumentsRequest {
            completions: vec![InstrumentCompletion {
                instrument_id: instrument_id.into(),
                against_version: against,
                values,
                note: note.into(),
            }],
        }
    }

    fn equity_in_usd() -> Vec<InstrumentValue> {
        vec![
            value(
                instrument_value::Value::AssetClass(AssetClass::Equity as i32),
                "Fidelity statement of 30 September",
            ),
            value(
                instrument_value::Value::Currency("USD".into()),
                "Fidelity statement of 30 September",
            ),
        ]
    }

    fn one(
        store: &MemoryStore,
        request: &CompleteInstrumentsRequest,
    ) -> InstrumentCompletionResult {
        complete(store, request, ADA, NOW + 1)
            .unwrap()
            .unwrap()
            .results
            .remove(0)
    }

    #[test]
    fn a_person_completes_a_record_each_value_with_its_source_and_the_person() {
        let store = MemoryStore::new();
        let id = minted(&store, "symbol", "SNAP1", "snaptrade");
        let done = complete(
            &store,
            &completing(&id, 1, equity_in_usd(), ""),
            ADA,
            NOW + 1,
        )
        .unwrap()
        .unwrap();
        assert!(
            done.results[0].refusal.is_none(),
            "{}",
            done.results[0].detail
        );
        let record = &done.changed[0];
        assert_eq!(record.version, 2);
        assert_eq!(record.asset_class, "ASSET_CLASS_EQUITY");
        let source = record.source_of(Field::AssetClass, None).unwrap();
        assert_eq!(source.person, ADA);
        assert_eq!(source.source, "Fidelity statement of 30 September");

        let history = store.history(&id).unwrap();
        assert_eq!(history[0].operation, "complete");
        assert_eq!(history[0].person, ADA);
        assert_eq!(history[0].changes[0].after, "equity");
    }

    #[test]
    fn a_record_changed_since_the_version_named_is_refused_alone() {
        let store = MemoryStore::new();
        let first = minted(&store, "symbol", "SNAP1", "snaptrade");
        let second = minted(&store, "symbol", "SNAP2", "snaptrade");
        let request = CompleteInstrumentsRequest {
            completions: vec![
                InstrumentCompletion {
                    instrument_id: first.clone(),
                    against_version: 7,
                    values: equity_in_usd(),
                    note: String::new(),
                },
                InstrumentCompletion {
                    instrument_id: second.clone(),
                    against_version: 1,
                    values: equity_in_usd(),
                    note: String::new(),
                },
            ],
        };
        let done = complete(&store, &request, ADA, NOW + 1).unwrap().unwrap();
        let refusal = done.results[0].refusal.as_ref().unwrap();
        assert_eq!(refusal.reason, RefusalReason::RecordChanged as i32);
        assert_eq!(refusal.fields, vec!["against_version"]);
        assert!(done.results[1].refusal.is_none());
        assert_eq!(done.changed.len(), 1);
    }

    #[test]
    fn each_bad_value_is_refused_naming_its_field() {
        let store = MemoryStore::new();
        let id = minted(&store, "symbol", "SNAP1", "snaptrade");
        let result = one(
            &store,
            &completing(
                &id,
                1,
                vec![
                    value(instrument_value::Value::AssetClass(0), "a statement"),
                    value(
                        instrument_value::Value::Currency("BASE".into()),
                        "a statement",
                    ),
                    value(instrument_value::Value::Description("Snap One".into()), ""),
                ],
                "",
            ),
        );
        let refusal = result.refusal.unwrap();
        assert_eq!(refusal.reason, RefusalReason::Incomplete as i32);
        assert_eq!(
            refusal.fields,
            vec![
                "values[0].asset_class",
                "values[1].currency",
                "values[2].source"
            ]
        );
        assert_eq!(
            store.by_id(&id).unwrap().unwrap().version,
            1,
            "nothing written"
        );
    }

    #[test]
    fn changing_a_value_held_needs_a_note_and_keeps_it() {
        let store = MemoryStore::new();
        let id = minted(&store, "symbol", "SNAP1", "snaptrade");
        one(&store, &completing(&id, 1, equity_in_usd(), ""));
        let to_fund = vec![value(
            instrument_value::Value::AssetClass(AssetClass::Fund as i32),
            "the prospectus",
        )];
        let refused = one(&store, &completing(&id, 2, to_fund.clone(), ""));
        assert_eq!(
            refused.refusal.unwrap().reason,
            RefusalReason::ReasonRequired as i32
        );
        let done = one(&store, &completing(&id, 2, to_fund, "an ETF, so a fund"));
        assert!(done.refusal.is_none());
        let history = store.history(&id).unwrap();
        assert_eq!(history[0].note, "an ETF, so a fund");
        assert_eq!(history[0].changes[0].before, "equity");
        assert_eq!(history[0].changes[0].after, "fund");
    }

    #[test]
    fn a_cash_records_currency_is_its_iso_code() {
        let store = MemoryStore::new();
        let id = minted(&store, "iso4217", "USD", "");
        let refused = one(
            &store,
            &completing(
                &id,
                1,
                vec![
                    value(
                        instrument_value::Value::AssetClass(AssetClass::Cash as i32),
                        "ISO 4217",
                    ),
                    value(instrument_value::Value::Currency("CAD".into()), "ISO 4217"),
                ],
                "",
            ),
        );
        let refusal = refused.refusal.unwrap();
        assert_eq!(refusal.fields, vec!["values[1].currency"]);
        let accepted = one(
            &store,
            &completing(
                &id,
                1,
                vec![
                    value(
                        instrument_value::Value::AssetClass(AssetClass::Cash as i32),
                        "ISO 4217",
                    ),
                    value(instrument_value::Value::Currency("USD".into()), "ISO 4217"),
                ],
                "",
            ),
        );
        assert!(accepted.refusal.is_none());
        assert!(
            accepted.instrument.unwrap().offers.is_empty(),
            "accepted offers are offers no longer"
        );
    }

    #[test]
    fn an_identifier_another_record_carries_is_refused_as_held_and_listed() {
        let store = MemoryStore::new();
        let l1 = minted(&store, "symbol", "SPAXX", "snaptrade");
        let l2 = minted(&store, "figi", "BBG000SPAXX1", "");
        let refused = one(
            &store,
            &completing(
                &l1,
                1,
                vec![value(
                    instrument_value::Value::Identifier(PbIdentifier {
                        scheme: "figi".into(),
                        value: "BBG000SPAXX1".into(),
                        source: String::new(),
                    }),
                    "OpenFIGI",
                )],
                "",
            ),
        );
        let refusal = refused.refusal.unwrap();
        assert_eq!(refusal.reason, RefusalReason::IdentifierHeld as i32);
        assert_eq!(refusal.fields, vec!["values[0].identifier"]);
        let conflicts = store.conflicts().unwrap();
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].instrument_ids.contains(&l2));
    }

    #[test]
    fn a_completion_needs_a_person_and_names_at_most_five_hundred_records() {
        let store = MemoryStore::new();
        let refused = complete(&store, &CompleteInstrumentsRequest::default(), "", NOW)
            .unwrap()
            .unwrap_err();
        assert_eq!(refused.reason, RefusalReason::ActorRequired);
        let many = CompleteInstrumentsRequest {
            completions: vec![InstrumentCompletion::default(); MOST_RECORDS + 1],
        };
        assert!(complete(&store, &many, ADA, NOW).unwrap().is_err());
    }

    #[test]
    fn a_merge_joins_the_identifiers_and_names_which_value_stands() {
        let store = MemoryStore::new();
        let l1 = minted(&store, "symbol", "SPAXX", "snaptrade");
        let l2 = minted(&store, "figi", "BBG000SPAXX1", "");
        one(&store, &completing(&l1, 1, equity_in_usd(), ""));
        one(
            &store,
            &completing(
                &l2,
                1,
                vec![
                    value(
                        instrument_value::Value::Currency("CAD".into()),
                        "a statement",
                    ),
                    value(
                        instrument_value::Value::Description("Spaxx fund".into()),
                        "a statement",
                    ),
                ],
                "",
            ),
        );
        let request = MergeInstrumentsRequest {
            kept_instrument_id: l1.clone(),
            kept_version: 2,
            merged_instrument_id: l2.clone(),
            merged_version: 2,
            take_from_merged: vec![],
            note: "one security".into(),
        };
        let merged = merge(&store, &request, ADA, NOW + 2).unwrap().unwrap();
        assert_eq!(merged.stays.version, 3);
        assert_eq!(
            merged.stays.currency, "USD",
            "the kept record's value stands by default"
        );
        assert_eq!(
            merged.stays.description, "Spaxx fund",
            "a blank is filled from the merged"
        );
        assert!(merged.stays.carries(&Asked {
            scheme: "figi".into(),
            value: "BBG000SPAXX1".into(),
            source: String::new(),
        }));
        assert_eq!(store.history(&l2).unwrap()[0].operation, "merged-into");

        let refused = merge(
            &store,
            &MergeInstrumentsRequest {
                take_from_merged: vec![InstrumentField::Currency as i32],
                ..request.clone()
            },
            ADA,
            NOW + 3,
        )
        .unwrap()
        .unwrap_err();
        assert_eq!(refused.reason, RefusalReason::RecordChanged);
        assert!(merge(
            &store,
            &MergeInstrumentsRequest {
                note: String::new(),
                ..request.clone()
            },
            ADA,
            NOW
        )
        .unwrap()
        .is_err());
        assert!(merge(&store, &request, "", NOW).unwrap().is_err());
    }

    #[test]
    fn the_platforms_answer_adds_its_id_and_offers_the_rest_without_changing_the_key() {
        // decisions/030, choice 2: the backfill adds the INS- ID and changes
        // no key; a person's value stands beside a differing platform value.
        let store = MemoryStore::new();
        let id = minted(&store, "iso4217", "USD", "");
        one(
            &store,
            &completing(
                &id,
                1,
                vec![value(
                    instrument_value::Value::Description("Dollars".into()),
                    "our own words",
                )],
                "",
            ),
        );
        let answer = PbInstrument {
            instrument_id: "INS-01J8XQ4M7K00000000CASHUSD".into(),
            identifiers: vec![PbIdentifier {
                scheme: "iso4217".into(),
                value: "USD".into(),
                source: String::new(),
            }],
            asset_class: AssetClass::Cash as i32,
            currency: "USD".into(),
            description: "US dollar".into(),
            version: 2,
            ..Default::default()
        };
        let kept = keep_platform_answer(&store, &id, &answer, NOW + 5)
            .unwrap()
            .expect("a new version");
        assert_eq!(
            kept.instrument_id, id,
            "the key is the deployment's for life"
        );
        assert!(kept.carries(&Asked {
            scheme: GLOBAL_ID.into(),
            value: answer.instrument_id.clone(),
            source: String::new(),
        }));
        assert_eq!(
            kept.description, "Dollars",
            "a person's value is never overwritten"
        );
        assert!(kept
            .offers
            .iter()
            .any(|offer| offer.field == Field::Description && offer.value == "US dollar"));
        assert!(kept.asset_class.is_empty(), "offered, not applied");

        assert!(
            keep_platform_answer(&store, &id, &answer, NOW + 6)
                .unwrap()
                .is_none(),
            "the same answer again changes nothing"
        );
    }
}
