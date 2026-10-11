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
//!
//! An identifier leaving a record has its window closed at that date, never
//! deleted (contract v19, W3.5): moved by a merge, or ended by the platform's
//! answer once a person accepts the end, which a completion carries as the
//! identifier with its `valid_until_ns`.

use meridian_domain::instrument_type;
use meridian_domain::v1::{
    instrument_value, AssetClass, BinaryEventContract, CompleteInstrumentsRequest,
    InstrumentCompletion, InstrumentCompletionResult, InstrumentRecord as PbInstrument,
    InstrumentType, MergeInstrumentsRequest, MissReason,
};
use meridian_pb::v1::{Refusal, RefusalReason};
use meridian_symbology::{GLOBAL_ID, OPEN};

use crate::record::{
    asked_from_wire, asset_class_name, field_from_wire, is_currency, to_wire, value_words,
    CURRENCY_SCHEME,
};
use crate::replace::current;
use crate::resolve::{identifier_change, keep_offers, window_for};
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

/// Who made a change: the person core stamped, and the delegation and its
/// client's name when they acted through a client on the deployment's MCP
/// surface (contract v12, Q9), each from the command's envelope.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Actor {
    pub person: String,
    pub delegation: String,
    pub client: String,
    /// The prepared change the person made it from (W6.27, contract v20),
    /// as the envelope carried it beside them; empty otherwise.
    pub prepared_call_id: String,
}

impl Actor {
    /// A person at the dashboard in a browser: no delegation.
    pub fn person(person: &str) -> Actor {
        Actor {
            person: person.to_string(),
            ..Actor::default()
        }
    }
}

/// W3.10. Set the values of up to 500 records, each against its version,
/// each refused alone, for a person at the dashboard.
pub fn complete(
    store: &dyn Store,
    request: &CompleteInstrumentsRequest,
    person: &str,
    now_ns: i64,
) -> Result<std::result::Result<Completed, Refused>> {
    complete_as(store, request, &Actor::person(person), now_ns)
}

/// W3.10, for whoever the envelope names: the person, and the delegation
/// and client they acted through, recorded on each version and value.
pub fn complete_as(
    store: &dyn Store,
    request: &CompleteInstrumentsRequest,
    actor: &Actor,
    now_ns: i64,
) -> Result<std::result::Result<Completed, Refused>> {
    if actor.person.is_empty() {
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
        let (result, changed) = complete_one(store, completion, actor, now_ns)?;
        completed.results.push(result);
        completed.changed.extend(changed);
    }
    Ok(Ok(completed))
}

/// One record's completion: its result, and the record when it changed.
fn complete_one(
    store: &dyn Store,
    completion: &InstrumentCompletion,
    actor: &Actor,
    now_ns: i64,
) -> Result<(InstrumentCompletionResult, Option<Instrument>)> {
    let prepared = actor.prepared_call_id.as_str();
    let (person, delegation, client) = (
        actor.person.as_str(),
        actor.delegation.as_str(),
        actor.client.as_str(),
    );
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
            instrument_value::Value::InstrumentType(kind) => {
                match InstrumentType::try_from(*kind) {
                    Ok(kind) if kind != InstrumentType::Unspecified => (
                        Field::InstrumentType,
                        instrument_type::name(kind).to_string(),
                        None,
                    ),
                    _ => {
                        problems.push((
                            RefusalReason::Incomplete,
                            path("instrument_type"),
                            format!(
                                "an instrument type is one of the list: {}",
                                instrument_type::TYPES
                                    .iter()
                                    .map(|kind| instrument_type::short(*kind))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                        ));
                        continue;
                    }
                }
            }
            instrument_value::Value::MoneyMarketFund(fund) => {
                let missing = instrument_type::unstated(fund);
                if !missing.is_empty() {
                    for attribute in &missing {
                        problems.push((
                            RefusalReason::Incomplete,
                            path(&format!("money_market_fund.{attribute}")),
                            format!(
                                "a money market fund's attributes are stated together: its {}",
                                missing.join(", ")
                            ),
                        ));
                    }
                    continue;
                }
                (
                    Field::MoneyMarketFund,
                    instrument_type::fund_to_text(fund),
                    None,
                )
            }
            instrument_value::Value::BinaryEventContract(contract) => {
                match payout_named(store, contract, now_ns)? {
                    Ok(named) => (
                        Field::BinaryEventContract,
                        instrument_type::contract_to_text(&named),
                        None,
                    ),
                    Err(wrong) => {
                        for (field, words) in wrong {
                            problems.push((
                                RefusalReason::Incomplete,
                                path(&format!("binary_event_contract.{field}")),
                                words,
                            ));
                        }
                        continue;
                    }
                }
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
                // An end stated (contract v19, W3.5): the identifier left the
                // record then, as the platform's offer said; its window closes.
                if identifier.valid_until_ns > 0 {
                    if !record.holds(&asked) {
                        if !record.carries(&asked) {
                            problems.push((
                                RefusalReason::Incomplete,
                                path("identifier"),
                                format!(
                                    "{}:{} is not on {}, so it cannot leave it",
                                    asked.scheme, asked.value, record.instrument_id
                                ),
                            ));
                        }
                        continue;
                    }
                    record.close(&asked, identifier.valid_until_ns);
                    changes_held = true;
                    record.set_source(Source {
                        acting_through_delegation: delegation.to_string(),
                        client_name: client.to_string(),
                        prepared_call_id: prepared.to_string(),
                        field: Field::Identifier,
                        identifier: Some(asked.clone()),
                        source: source.to_string(),
                        person: person.to_string(),
                        instance_id: String::new(),
                        recorded_at_ns: now_ns,
                        note: completion.note.trim().to_string(),
                    });
                    changes.push(Change {
                        field: Field::Identifier.name().into(),
                        scheme: asked.scheme.clone(),
                        namespace: asked.source.clone(),
                        before: asked.value.clone(),
                        after: String::new(),
                        source: source.to_string(),
                    });
                    continue;
                }
                (Field::Identifier, asked.value.clone(), Some(asked))
            }
        };

        if let Some(asked) = identifier {
            if record.holds(&asked) {
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
            record.identifiers.push(window_for(store, &asked)?);
            record.set_source(Source {
                acting_through_delegation: delegation.to_string(),
                client_name: client.to_string(),
                prepared_call_id: prepared.to_string(),
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
            acting_through_delegation: delegation.to_string(),
            client_name: client.to_string(),
            prepared_call_id: prepared.to_string(),
            field,
            identifier: None,
            source: source.to_string(),
            person: person.to_string(),
            instance_id: String::new(),
            recorded_at_ns: now_ns,
            note: completion.note.trim().to_string(),
        });
        changes.push(Change {
            field: field.name().into(),
            scheme: String::new(),
            namespace: String::new(),
            before: value_words(field, &before),
            after: value_words(field, &text),
            source: source.to_string(),
        });
    }

    // A type is under one asset class, and a fund's attributes belong to a
    // money market fund (contract v11): refused naming the value that set
    // what does not fit, whatever order the values came in.
    let kind =
        instrument_type::read(&record.instrument_type).unwrap_or(InstrumentType::Unspecified);
    let at_value = |wanted: fn(&instrument_value::Value) -> bool, name: &str| {
        completion
            .values
            .iter()
            .position(|value| value.value.as_ref().is_some_and(wanted))
            .map(|at| format!("values[{at}].{name}"))
            .unwrap_or_else(|| name.to_string())
    };
    if let Some(class) = instrument_type::class_of(kind) {
        if record.asset_class != asset_class_name(class as i32) {
            problems.push((
                RefusalReason::Incomplete,
                at_value(
                    |value| matches!(value, instrument_value::Value::InstrumentType(_)),
                    "instrument_type",
                ),
                format!(
                    "a {} is under {}, and the record's asset class is {}",
                    instrument_type::words(kind),
                    value_words(Field::AssetClass, &asset_class_name(class as i32)),
                    if record.asset_class.is_empty() {
                        "not set".to_string()
                    } else {
                        value_words(Field::AssetClass, &record.asset_class)
                    }
                ),
            ));
        }
    }
    if !record.money_market_fund.is_empty() && kind != InstrumentType::MoneyMarketFund {
        problems.push((
            RefusalReason::Incomplete,
            at_value(
                |value| matches!(value, instrument_value::Value::MoneyMarketFund(_)),
                "money_market_fund",
            ),
            "a money market fund's attributes are set only on a record whose type is money \
             market fund"
                .into(),
        ));
    }
    // And a binary event contract's to one (contract v19, W3.10).
    if !record.binary_event_contract.is_empty() && kind != InstrumentType::BinaryEventContract {
        problems.push((
            RefusalReason::Incomplete,
            at_value(
                |value| matches!(value, instrument_value::Value::BinaryEventContract(_)),
                "binary_event_contract",
            ),
            "a binary event contract's attributes are set only on a record whose type is \
             binary event contract"
                .into(),
        ));
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
        acting_through_delegation: delegation.to_string(),
        client_name: client.to_string(),
        prepared_call_id: prepared.to_string(),
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
    merge_as(store, request, &Actor::person(person), now_ns)
}

/// W3.13, for whoever the envelope names, the delegation and client beside
/// the person on both records' versions.
pub fn merge_as(
    store: &dyn Store,
    request: &MergeInstrumentsRequest,
    actor: &Actor,
    now_ns: i64,
) -> Result<std::result::Result<Merged, Refused>> {
    let prepared = actor.prepared_call_id.as_str();
    let (person, delegation, client) = (
        actor.person.as_str(),
        actor.delegation.as_str(),
        actor.client.as_str(),
    );
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

    // The merged record's identifiers move to the one that stays, each with
    // the window it had; on the merged record each window closes now, never
    // deleted (contract v19, W3.5), so a resolve as of a date before still
    // meets it, and follows its replacement to the record that stays.
    for identifier in merged.held_now() {
        let asked = identifier.asked();
        if stays.holds(&asked) {
            continue;
        }
        stays.identifiers.push(identifier.clone());
        let carried = merged
            .source_of(Field::Identifier, Some(&asked))
            .cloned()
            .unwrap_or(Source {
                acting_through_delegation: delegation.to_string(),
                client_name: client.to_string(),
                prepared_call_id: prepared.to_string(),
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
    for field in [
        Field::AssetClass,
        Field::Currency,
        Field::Description,
        Field::InstrumentType,
        Field::MoneyMarketFund,
        Field::BinaryEventContract,
    ] {
        let (ours, theirs) = (kept.value(field), merged.value(field));
        if theirs.is_empty() || ours == theirs {
            continue;
        }
        if !ours.is_empty() && !take.contains(&field) {
            continue;
        }
        stays.set_value(field, theirs.to_string());
        let carried = merged.source_of(field, None).cloned().unwrap_or(Source {
            acting_through_delegation: delegation.to_string(),
            client_name: client.to_string(),
            prepared_call_id: prepared.to_string(),
            field,
            identifier: None,
            source: format!("merged from {merged_id}"),
            person: person.to_string(),
            instance_id: String::new(),
            recorded_at_ns: now_ns,
            note: note.to_string(),
        });
        changes.push(Change {
            field: field.name().into(),
            scheme: String::new(),
            namespace: String::new(),
            before: value_words(field, ours),
            after: value_words(field, theirs),
            source: carried.source.clone(),
        });
        stays.set_source(carried);
    }
    keep_offers(&mut stays, merged.offers.clone());

    stays.version = kept.version + 1;
    stays.record_time_ns = now_ns;
    let stays_entry = Version {
        acting_through_delegation: delegation.to_string(),
        client_name: client.to_string(),
        prepared_call_id: prepared.to_string(),
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
    let mut left = Vec::new();
    for identifier in merged.held_now() {
        let asked = identifier.asked();
        gone.close(&asked, now_ns);
        left.push(Change {
            field: Field::Identifier.name().into(),
            scheme: asked.scheme.clone(),
            namespace: asked.source.clone(),
            before: asked.value.clone(),
            after: String::new(),
            source: format!("merged into {kept_id}"),
        });
    }
    gone.version = merged.version + 1;
    gone.record_time_ns = now_ns;
    let gone_entry = Version {
        acting_through_delegation: delegation.to_string(),
        client_name: client.to_string(),
        prepared_call_id: prepared.to_string(),
        instrument_id: gone.instrument_id.clone(),
        version: gone.version,
        operation: "merged-into".into(),
        changes: left,
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
        } else if !record.holds(&global) {
            record.identifiers.push(window_for(store, &global)?);
            record.set_source(Source {
                acting_through_delegation: String::new(),
                client_name: String::new(),
                prepared_call_id: String::new(),
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
        valid_until_ns: 0,
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
    // Its type and a binary event contract's attributes (contract v19): the
    // type only under the class the answer gives beside it, as a plugin's.
    if let Ok(kind) = InstrumentType::try_from(answer.instrument_type) {
        if instrument_type::class_of(kind).is_some_and(|class| class as i32 == answer.asset_class) {
            offers.push(offer(
                Field::InstrumentType,
                instrument_type::name(kind).to_string(),
                None,
            ));
            if kind == InstrumentType::BinaryEventContract {
                if let Some(contract) = answer.binary_event_contract.as_ref() {
                    if let Ok(Ok(named)) = payout_named(store, contract, now_ns) {
                        offers.push(offer(
                            Field::BinaryEventContract,
                            instrument_type::contract_to_text(&named),
                            None,
                        ));
                    }
                }
            }
        }
    }
    for identifier in &answer.identifiers {
        let asked = asked_from_wire(identifier);
        if asked.source.is_empty()
            && OPEN.contains(&asked.scheme.as_str())
            && asked.scheme != GLOBAL_ID
        {
            if identifier.valid_until_ns > 0 {
                // The master ended it (contract v19, the plan's Q34): an offer
                // to close its window here, where the record holds it, in
                // force once a person accepts it (decisions/030, choice 2).
                if record.holds(&asked) {
                    offers.push(Offer {
                        valid_until_ns: identifier.valid_until_ns,
                        ..offer(Field::Identifier, asked.value.clone(), Some(asked))
                    });
                }
            } else {
                offers.push(offer(Field::Identifier, asked.value.clone(), Some(asked)));
            }
        }
    }
    let offered = keep_offers(&mut record, offers);
    // The venue it is listed on (contract v18, W3.5): the platform's ID for a
    // venue the answer carried, kept where the record names none yet; never
    // over one it names.
    if record.listing_venue_id.is_empty() && answer.listing_venue_id.starts_with("VEN-") {
        record.listing_venue_id = answer.listing_venue_id.clone();
        changes.push(Change {
            field: "listing_venue_id".into(),
            scheme: String::new(),
            namespace: String::new(),
            before: String::new(),
            after: answer.listing_venue_id.clone(),
            source: words.clone(),
        });
    }
    if changes.is_empty() && !offered {
        return Ok(None);
    }

    record.version = held.version + 1;
    record.record_time_ns = now_ns;
    let entry = Version {
        acting_through_delegation: String::new(),
        client_name: String::new(),
        prepared_call_id: String::new(),
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
    // A completion or the platform's ID meeting another record's identifier:
    // no reason (contract v19, the plan's Q35).
    store.note_conflict(Conflict {
        identifiers,
        instrument_ids,
        reported_by: String::new(),
        first_seen_ns: now_ns,
        last_seen_ns: now_ns,
        reason: MissReason::Unspecified as i32,
    })
}

/// A binary event contract's attributes held to W3.10, its payout naming
/// its cash instrument where the store holds one record for its ISO 4217
/// code (contract v19): the attributes as kept, or what is wrong, each by
/// its field. A payout naming an instrument the deployment does not hold, or
/// a code and an instrument naming two assets, is refused.
pub(crate) fn payout_named(
    store: &dyn Store,
    contract: &BinaryEventContract,
    now_ns: i64,
) -> Result<std::result::Result<BinaryEventContract, Vec<(&'static str, String)>>> {
    let wrong = instrument_type::contract_refused(contract);
    if !wrong.is_empty() {
        return Ok(Err(wrong));
    }
    let mut named = contract.clone();
    let payout = named.payout.get_or_insert_with(Default::default);
    payout.currency_code = payout.currency_code.trim().to_string();
    payout.instrument_id = payout.instrument_id.trim().to_string();
    if !payout.instrument_id.is_empty() && store.by_id(&payout.instrument_id)?.is_none() {
        return Ok(Err(vec![(
            "payout.instrument_id",
            format!("{} is no record the deployment holds", payout.instrument_id),
        )]));
    }
    if !payout.currency_code.is_empty() {
        let mut cash: Vec<String> = Vec::new();
        for record in store.matching(CURRENCY_SCHEME, &payout.currency_code, "", now_ns)? {
            let became = current(store, &record.instrument_id)?;
            if !cash.contains(&became) {
                cash.push(became);
            }
        }
        match (cash.as_slice(), payout.instrument_id.is_empty()) {
            ([one], true) => payout.instrument_id = one.clone(),
            ([one], false) if *one != payout.instrument_id => {
                return Ok(Err(vec![(
                    "payout",
                    format!(
                        "currency_code {} names {one}, and instrument_id names {}; both set \
                         name the same asset",
                        payout.currency_code, payout.instrument_id
                    ),
                )]))
            }
            _ => {}
        }
    }
    Ok(Ok(named))
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
                    ..Default::default()
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
    fn a_completion_through_a_client_records_the_delegation_and_client_beside_the_person() {
        // Contract v12, Q9: "set by the person, through that client".
        let store = MemoryStore::new();
        let id = minted(&store, "symbol", "SNAP1", "snaptrade");
        let actor = Actor {
            prepared_call_id: Default::default(),
            person: ADA.into(),
            delegation: "del-01J9AAAA".into(),
            client: "Claude".into(),
        };
        let done = complete_as(
            &store,
            &completing(&id, 1, equity_in_usd(), ""),
            &actor,
            NOW + 1,
        )
        .unwrap()
        .unwrap();
        let source = done.changed[0].source_of(Field::AssetClass, None).unwrap();
        assert_eq!(
            (
                source.person.as_str(),
                source.acting_through_delegation.as_str(),
                source.client_name.as_str()
            ),
            (ADA, "del-01J9AAAA", "Claude")
        );
        let history = store.history(&id).unwrap();
        assert_eq!(history[0].acting_through_delegation, "del-01J9AAAA");
        assert_eq!(history[0].client_name, "Claude");
        // A person at the dashboard names none.
        let other = minted(&store, "symbol", "SNAP2", "snaptrade");
        let done = complete(
            &store,
            &completing(&other, 1, equity_in_usd(), ""),
            ADA,
            NOW + 2,
        )
        .unwrap()
        .unwrap();
        let source = done.changed[0].source_of(Field::AssetClass, None).unwrap();
        assert!(source.acting_through_delegation.is_empty() && source.client_name.is_empty());
    }

    fn stable_government_fund() -> meridian_domain::v1::MoneyMarketFund {
        use meridian_domain::v1::{
            LiquidityFeeRegime, MoneyMarketFundCategory, MoneyMarketFundInvestors,
            MoneyMarketFundNav,
        };
        meridian_domain::v1::MoneyMarketFund {
            category: MoneyMarketFundCategory::Government as i32,
            investors: MoneyMarketFundInvestors::Retail as i32,
            nav: MoneyMarketFundNav::Stable as i32,
            liquidity_fee: LiquidityFeeRegime::Discretionary as i32,
        }
    }

    #[test]
    fn a_money_market_fund_is_completed_with_its_type_and_attributes() {
        // Contract v11: a type under its class, and a fund's four attributes,
        // each a value with its source and the person.
        let store = MemoryStore::new();
        let id = minted(&store, "symbol", "SPAXX", "snaptrade");
        let result = one(
            &store,
            &completing(
                &id,
                1,
                vec![
                    value(
                        instrument_value::Value::AssetClass(AssetClass::Fund as i32),
                        "stated by custody-snaptrade-1",
                    ),
                    value(
                        instrument_value::Value::InstrumentType(
                            InstrumentType::MoneyMarketFund as i32,
                        ),
                        "stated by custody-snaptrade-1",
                    ),
                    value(
                        instrument_value::Value::Currency("USD".into()),
                        "the prospectus",
                    ),
                    value(
                        instrument_value::Value::MoneyMarketFund(stable_government_fund()),
                        "the prospectus",
                    ),
                ],
                "",
            ),
        );
        assert!(result.refusal.is_none(), "{}", result.detail);
        let record = result.instrument.unwrap();
        assert_eq!(
            record.instrument_type,
            InstrumentType::MoneyMarketFund as i32
        );
        assert_eq!(record.money_market_fund, Some(stable_government_fund()));
        let held = store.by_id(&id).unwrap().unwrap();
        assert_eq!(
            held.source_of(Field::MoneyMarketFund, None).unwrap().person,
            ADA
        );
        let history = store.history(&id).unwrap();
        assert!(history[0]
            .changes
            .iter()
            .any(|change| change.after
                == "government, retail, stable NAV, discretionary liquidity fee"));
        assert!(history[0]
            .changes
            .iter()
            .any(|change| change.after == "money market fund"));
    }

    #[test]
    fn a_type_off_its_class_and_a_funds_attributes_off_a_fund_are_refused_naming_them() {
        let store = MemoryStore::new();
        let id = minted(&store, "symbol", "SNAP1", "snaptrade");
        let mut values = equity_in_usd();
        values.push(value(
            instrument_value::Value::InstrumentType(InstrumentType::MoneyMarketFund as i32),
            "a statement",
        ));
        let refusal = one(&store, &completing(&id, 1, values, ""))
            .refusal
            .unwrap();
        assert_eq!(refusal.reason, RefusalReason::Incomplete as i32);
        assert_eq!(refusal.fields, vec!["values[2].instrument_type"]);

        let mut values = equity_in_usd();
        values.push(value(
            instrument_value::Value::MoneyMarketFund(stable_government_fund()),
            "a statement",
        ));
        let refusal = one(&store, &completing(&id, 1, values, ""))
            .refusal
            .unwrap();
        assert_eq!(refusal.fields, vec!["values[2].money_market_fund"]);

        let partial = meridian_domain::v1::MoneyMarketFund {
            category: stable_government_fund().category,
            nav: stable_government_fund().nav,
            ..Default::default()
        };
        let refusal = one(
            &store,
            &completing(
                &id,
                1,
                vec![value(
                    instrument_value::Value::MoneyMarketFund(partial),
                    "a statement",
                )],
                "",
            ),
        )
        .refusal
        .unwrap();
        assert_eq!(
            refusal.fields,
            vec![
                "values[0].money_market_fund.investors",
                "values[0].money_market_fund.liquidity_fee"
            ]
        );
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
                        ..Default::default()
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
                ..Default::default()
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

    fn ticker() -> Asked {
        Asked {
            scheme: "symbol".into(),
            value: "ZZ".into(),
            source: "snaptrade".into(),
        }
    }

    fn resolve_ticker(store: &MemoryStore, as_of_ns: i64, now_ns: i64) -> String {
        let reply = resolve_identifier(
            store,
            &ResolveIdentifierRequest {
                identifiers: vec![crate::record::asked_to_wire(&ticker())],
                as_of_ns,
                ..Default::default()
            },
            "custody-snaptrade-1",
            now_ns,
        )
        .unwrap()
        .reply;
        assert!(reply.found, "{reply:?}");
        reply.instrument_id
    }

    #[test]
    fn a_ticker_reassigned_answers_each_record_for_its_own_dates() {
        // decisions/034, W3.5: the platform ends the ticker on the record
        // that held it, a person accepts the end, and another security is
        // given it later. Its window on the first record is closed, never
        // deleted: as of a date before, the first answers; after, the second.
        let store = MemoryStore::new();
        let first = resolve_ticker(&store, NOW, NOW);
        let left = NOW + 10;
        let answer = PbInstrument {
            instrument_id: "INS-01J8XQ4M7K000000000000ZZ".into(),
            identifiers: vec![PbIdentifier {
                valid_until_ns: left,
                ..crate::record::asked_to_wire(&Asked {
                    scheme: "ticker".into(),
                    value: "ZZ".into(),
                    source: String::new(),
                })
            }],
            version: 3,
            ..Default::default()
        };
        // The platform's ended identifier is offered only where the record
        // holds it: this record holds the brokerage symbol, not the ticker.
        let kept = keep_platform_answer(&store, &first, &answer, NOW + 11)
            .unwrap()
            .unwrap();
        assert!(!kept.offers.iter().any(|offer| offer.valid_until_ns > 0));

        // A person ends the symbol, as an accepted end would.
        let ended = one(
            &store,
            &completing(
                &first,
                kept.version,
                vec![value(
                    instrument_value::Value::Identifier(PbIdentifier {
                        valid_until_ns: left,
                        ..crate::record::asked_to_wire(&ticker())
                    }),
                    "the platform, record INS-01J8XQ4M7K000000000000ZZ version 3",
                )],
                "delisted; the platform ended the symbol",
            ),
        );
        assert!(ended.refusal.is_none(), "{ended:?}");
        let record = ended.instrument.unwrap();
        let window = record
            .identifiers
            .iter()
            .find(|identifier| identifier.value == "ZZ")
            .expect("kept, never deleted");
        assert_eq!(window.valid_until_ns, left);

        // Another security is given the symbol after it left.
        let second = resolve_ticker(&store, NOW + 20, NOW + 20);
        assert_ne!(second, first, "a new record for the new security");
        let held = store.by_id(&second).unwrap().unwrap();
        assert_eq!(
            held.identifiers[0].valid_from_ns, left,
            "its window opens where the last closed"
        );
        assert_eq!(resolve_ticker(&store, NOW + 5, NOW + 21), first);
        assert_eq!(resolve_ticker(&store, NOW + 30, NOW + 22), second);
        assert!(store.conflicts().unwrap().is_empty(), "no date answers two");
    }

    #[test]
    fn a_merge_closes_the_merged_records_windows_and_keeps_them() {
        let store = MemoryStore::new();
        let a = minted(&store, "symbol", "AAA", "snaptrade");
        let b = minted(&store, "figi", "BBG000AAA001", "");
        let merged = merge(
            &store,
            &MergeInstrumentsRequest {
                kept_instrument_id: a.clone(),
                kept_version: 1,
                merged_instrument_id: b.clone(),
                merged_version: 1,
                note: "one security".into(),
                ..Default::default()
            },
            ADA,
            NOW + 1,
        )
        .unwrap()
        .unwrap();
        let figi = Asked {
            scheme: "figi".into(),
            value: "BBG000AAA001".into(),
            source: String::new(),
        };
        assert!(merged.stays.holds(&figi));
        assert!(merged.merged.carries(&figi) && !merged.merged.holds(&figi));
        assert_eq!(
            merged.merged.identifiers[0].valid_to_ns,
            Some(NOW + 1),
            "closed at the merge"
        );
    }

    #[test]
    fn a_binary_event_contract_is_completed_with_its_payout_named() {
        let store = MemoryStore::new();
        let usd = minted(&store, "iso4217", "USD", "");
        let id = minted(&store, "symbol", "KXFED-26NOV", "kalshi");
        let contract = BinaryEventContract {
            payout: Some(meridian_domain::v1::Money {
                amount: Some(meridian_domain::exact::Exact::new(1, 0).unwrap().to_wire()),
                currency_code: "USD".into(),
                instrument_id: String::new(),
            }),
            closes_at_ns: 1_793_548_800_000_000_000,
        };
        let done = one(
            &store,
            &completing(
                &id,
                1,
                vec![
                    value(
                        instrument_value::Value::AssetClass(AssetClass::EventContract as i32),
                        "stated by kalshi-1",
                    ),
                    value(
                        instrument_value::Value::InstrumentType(
                            InstrumentType::BinaryEventContract as i32,
                        ),
                        "stated by kalshi-1",
                    ),
                    value(
                        instrument_value::Value::BinaryEventContract(contract.clone()),
                        "the venue's contract terms",
                    ),
                ],
                "",
            ),
        );
        assert!(done.refusal.is_none(), "{done:?}");
        let record = done.instrument.unwrap();
        let kept = record.binary_event_contract.unwrap();
        assert_eq!(
            kept.payout.as_ref().unwrap().instrument_id,
            usd,
            "its cash instrument"
        );
        assert_eq!(kept.closes_at_ns, contract.closes_at_ns);

        // On a record that is not one, refused naming the value.
        let other = minted(&store, "symbol", "SPY", "snaptrade");
        let refused = one(
            &store,
            &completing(
                &other,
                1,
                vec![value(
                    instrument_value::Value::BinaryEventContract(contract),
                    "the venue's contract terms",
                )],
                "",
            ),
        );
        let refusal = refused.refusal.unwrap();
        assert_eq!(refusal.reason, RefusalReason::Incomplete as i32);
        assert_eq!(refusal.fields, vec!["values[0].binary_event_contract"]);
    }

    #[test]
    fn a_completions_conflict_has_no_reason_and_a_resolves_is_ambiguous() {
        let store = MemoryStore::new();
        let a = minted(&store, "symbol", "AAA", "snaptrade");
        let b = minted(&store, "figi", "BBG000AAA002", "");
        let refused = one(
            &store,
            &completing(
                &a,
                1,
                vec![value(
                    instrument_value::Value::Identifier(PbIdentifier {
                        scheme: "figi".into(),
                        value: "BBG000AAA002".into(),
                        ..Default::default()
                    }),
                    "a statement",
                )],
                "",
            ),
        );
        assert!(refused.refusal.is_some());
        let listed = store.conflicts().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].reason, MissReason::Unspecified as i32);
        let _ = b;

        // A resolve meeting both records lists them ambiguous.
        let reply = resolve_identifier(
            &store,
            &ResolveIdentifierRequest {
                identifiers: vec![
                    PbIdentifier {
                        scheme: "figi".into(),
                        value: "BBG000AAA002".into(),
                        ..Default::default()
                    },
                    PbIdentifier {
                        scheme: "symbol".into(),
                        value: "AAA".into(),
                        source: "snaptrade".into(),
                        ..Default::default()
                    },
                ],
                as_of_ns: NOW,
                ..Default::default()
            },
            "custody-snaptrade-1",
            NOW + 2,
        )
        .unwrap()
        .reply;
        assert_eq!(reply.miss_reason, MissReason::Ambiguous as i32);
        let flagged = store.conflicts().unwrap();
        assert!(flagged
            .iter()
            .any(|conflict| conflict.reason == MissReason::Ambiguous as i32));
    }
}
