//! Answering instrument questions from the deployment's own records.
//!
//! W3.1 turns a set of identifiers into one record, a record minted for the
//! set, or a miss. W3.6 turns a record's ID into the record, so a holding can
//! be shown with a name and the book can read its asset class and currency.
//! W3.2 lists the conflict a connector reports.
//!
//! # One identifier in common is one security (contract v10)
//!
//! Every record the deployment holds is searched, the records it minted
//! included: a custody plugin's symbol and a market-data plugin's FIGI meet
//! one record once either record carries both. On a match, the set's other
//! identifiers join the record, each with the reporting instance as its
//! source, where no other record carries them. A contradiction -- the set's
//! identifiers meeting two records, by one tier or across tiers -- is not a
//! match: nothing joins, the resolve is a miss, and the conflict is listed for
//! a person, who merges what they say is one security (W3.13). That is the
//! platform's ruled rule brought inside the deployment (the spec's Q4).
//!
//! A set nothing matched is answered with a record minted for it (W3.7), the
//! same one for the same set however often and however concurrently it is
//! asked, so the holding can be recorded and counted at once.
//!
//! # Strongest first, and ambiguity is not a tiebreak
//!
//! A global scheme is tried before a source-scoped symbol, because a brokerage
//! symbol means nothing outside its own namespace. Within a tier, more than one
//! candidate is a miss rather than a choice; an ambiguous tier does not fall
//! through to a weaker one either.
//!
//! # What a source states is offered, never in force
//!
//! An asset class, a currency or a description a plugin sends with its resolve
//! is kept on the record as an offer, with the instance as its source, in force
//! only when a person accepts it (W3.10; the spec's Q6).

use meridian_domain::v1::{
    AssetClass, MissReason, MissingInstrumentDetectedEvent, ResolveIdentifierReply,
    ResolveIdentifierRequest, ResolveInstrumentReply, ResolveInstrumentRequest,
};

use meridian_symbology::rank;

use crate::ids;
use crate::record::{asked_from_wire, asset_class_name, dated, is_currency, to_wire};
use crate::replace::current;
use crate::store::{
    Asked, Change, Conflict, Field, IdentifierSet, Instrument, Offer, Result, Source, Stood, Store,
    Version, Written,
};

/// What a resolve answered, and the record it changed to answer it.
#[derive(Debug, Clone)]
pub struct Resolution {
    pub reply: ResolveIdentifierReply,

    /// The record at its new version, when this resolve minted it, joined an
    /// identifier to it, or kept a new offer on it: the caller announces it
    /// (W3.5). `None` when nothing changed.
    pub changed: Option<Instrument>,
}

/// W3.1 — which record this identifier set meant, on that date; or, when
/// nothing did, a record minted for it (W3.7).
///
/// `instance_id` is the plugin instance asking, the source of every
/// identifier it joins and every value it offers. `now_ns` is when, passed in
/// so a test controls time.
pub fn resolve_identifier(
    store: &dyn Store,
    request: &ResolveIdentifierRequest,
    instance_id: &str,
    now_ns: i64,
) -> Result<Resolution> {
    let asked: Vec<Asked> = IdentifierSet::new(
        request
            .identifiers
            .iter()
            .map(asked_from_wire)
            .filter(|asked| !asked.scheme.is_empty() && !asked.value.is_empty()),
    )
    .members()
    .to_vec();
    if asked.is_empty() {
        return Ok(unchanged(missed(MissReason::NotFound)));
    }

    let mut tiers: Vec<usize> = asked.iter().map(rank_of).collect();
    tiers.sort_unstable();
    tiers.dedup();

    for tier in tiers {
        let in_tier: Vec<&Asked> = asked.iter().filter(|each| rank_of(each) == tier).collect();
        let mut candidates: Vec<String> = Vec::new();
        let mut meeting: Vec<Asked> = Vec::new();

        for identifier in &in_tier {
            for record in matches(store, identifier, request)? {
                // What it has become, before counting: a record merged into
                // another and the one that stays both carry the identifiers,
                // and they are one record. That holds only because an ID is
                // never reused, so equality of the key is equality of the
                // thing.
                let became = current(store, &record.instrument_id)?;
                if !candidates.contains(&became) {
                    candidates.push(became);
                }
                if !meeting.contains(identifier) {
                    meeting.push((*identifier).clone());
                }
            }
        }

        match candidates.len() {
            0 => continue,
            1 => {
                return joined(
                    store,
                    request,
                    &asked,
                    candidates.remove(0),
                    meeting,
                    instance_id,
                    now_ns,
                )
            }
            _ => {
                note(store, meeting, candidates, instance_id, now_ns)?;
                return Ok(unchanged(missed(MissReason::Ambiguous)));
            }
        }
    }

    mint(store, request, asked, instance_id, now_ns)
}

/// One record matched: join the set's other identifiers to it, unless one of
/// them is another record's, which is a contradiction and a miss.
fn joined(
    store: &dyn Store,
    request: &ResolveIdentifierRequest,
    asked: &[Asked],
    found: String,
    meeting: Vec<Asked>,
    instance_id: &str,
    now_ns: i64,
) -> Result<Resolution> {
    let Some(mut record) = store.by_id(&found)? else {
        // Replaced into something this store does not hold: answer it as it
        // was answered before v10, the ID alone.
        return Ok(unchanged(resolved(found, false)));
    };

    let mut joining = Vec::new();
    for identifier in asked.iter().filter(|each| !record.carries(each)) {
        let others = holders(store, identifier, request.as_of_ns, &found)?;
        if !others.is_empty() {
            let mut ids = vec![found.clone()];
            ids.extend(others);
            let mut identifiers = meeting.clone();
            identifiers.push(identifier.clone());
            note(store, identifiers, ids, instance_id, now_ns)?;
            return Ok(unchanged(missed(MissReason::Ambiguous)));
        }
        joining.push(identifier.clone());
    }

    let expected = record.version;
    let mut changes = Vec::new();
    for identifier in &joining {
        record.identifiers.push(dated(identifier));
        record.set_source(Source {
            acting_through_delegation: String::new(),
            client_name: String::new(),
            field: Field::Identifier,
            identifier: Some(identifier.clone()),
            source: reported_by(instance_id),
            person: String::new(),
            instance_id: instance_id.to_string(),
            recorded_at_ns: now_ns,
            note: String::new(),
        });
        changes.push(identifier_change(identifier, &reported_by(instance_id)));
    }
    let offered = keep_offers(&mut record, stated(request, instance_id, now_ns));
    if joining.is_empty() && !offered {
        return Ok(unchanged(resolved(found, false)));
    }

    record.version = expected + 1;
    record.record_time_ns = now_ns;
    let entry = Version {
        acting_through_delegation: String::new(),
        client_name: String::new(),
        instrument_id: record.instrument_id.clone(),
        version: record.version,
        operation: if joining.is_empty() { "offer" } else { "join" }.into(),
        changes,
        person: String::new(),
        instance_id: instance_id.to_string(),
        note: String::new(),
        merged_instrument_id: String::new(),
        record_time_ns: now_ns,
    };
    match store.write(record.clone(), expected, entry)? {
        Written::Stored => Ok(Resolution {
            reply: resolved(found, false),
            changed: Some(record),
        }),
        // Somebody wrote it in between: the match stands, and the identifiers
        // join on the next report rather than over a version not read.
        Written::Stale { .. } | Written::Missing => Ok(unchanged(resolved(found, false))),
    }
}

/// W3.7 — nothing matched: mint a record for the set, or answer the one
/// minted for it already.
fn mint(
    store: &dyn Store,
    request: &ResolveIdentifierRequest,
    asked: Vec<Asked>,
    instance_id: &str,
    now_ns: i64,
) -> Result<Resolution> {
    let set = IdentifierSet::new(asked.clone());
    let instrument_id = ids::local(now_ns);
    let mut candidate = Instrument {
        instrument_id: instrument_id.clone(),
        identifiers: asked.iter().map(dated).collect(),
        asset_class: String::new(),
        currency: String::new(),
        exchange_mic: String::new(),
        description: String::new(),
        lifecycle_state: "INSTRUMENT_LIFECYCLE_STATE_ACTIVE".into(),
        version: 1,
        valid_from_ns: 0,
        record_time_ns: now_ns,
        instrument_type: String::new(),
        money_market_fund: String::new(),
        listing_venue_id: String::new(),
        sources: asked
            .iter()
            .map(|identifier| Source {
                acting_through_delegation: String::new(),
                client_name: String::new(),
                field: Field::Identifier,
                identifier: Some(identifier.clone()),
                source: reported_by(instance_id),
                person: String::new(),
                instance_id: instance_id.to_string(),
                recorded_at_ns: now_ns,
                note: String::new(),
            })
            .collect(),
        offers: Vec::new(),
    };
    keep_offers(&mut candidate, stated(request, instance_id, now_ns));
    let first = Version {
        acting_through_delegation: String::new(),
        client_name: String::new(),
        instrument_id: instrument_id.clone(),
        version: 1,
        operation: "mint".into(),
        changes: asked
            .iter()
            .map(|identifier| identifier_change(identifier, &reported_by(instance_id)))
            .collect(),
        person: String::new(),
        instance_id: instance_id.to_string(),
        note: String::new(),
        merged_instrument_id: String::new(),
        record_time_ns: now_ns,
    };

    let (record, stood) = store.mint(candidate, &set.key(), first)?;
    match stood {
        Stood::Minted => Ok(Resolution {
            reply: resolved(record.instrument_id.clone(), true),
            changed: Some(record),
        }),
        Stood::AlreadyHeld => {
            let became = current(store, &record.instrument_id)?;
            Ok(unchanged(resolved(became, false)))
        }
    }
}

/// W3.2 — a connector reports an ambiguous resolve: list the conflict if its
/// identifiers meet more than one record, and say whether they did.
pub fn conflict_reported(
    store: &dyn Store,
    event: &MissingInstrumentDetectedEvent,
    now_ns: i64,
) -> Result<bool> {
    let mut ids: Vec<String> = Vec::new();
    let mut meeting: Vec<Asked> = Vec::new();
    for identifier in &event.identifiers {
        let asked = asked_from_wire(identifier);
        for record in store.matching(&asked.scheme, &asked.value, &asked.source, event.as_of_ns)? {
            let became = current(store, &record.instrument_id)?;
            if !ids.contains(&became) {
                ids.push(became);
            }
            if !meeting.contains(&asked) {
                meeting.push(asked.clone());
            }
        }
    }
    if ids.len() < 2 {
        return Ok(false);
    }
    note(
        store,
        meeting,
        ids,
        &event.publisher_instance_id,
        if event.observed_at_ns > 0 {
            event.observed_at_ns
        } else {
            now_ns
        },
    )?;
    Ok(true)
}

/// W3.6 — the record behind an ID: its values with their sources, and the
/// offers beside them.
///
/// `as_of_ns` does not select which record: an ID is never reused. A record
/// merged into another answers the one that stays, whose `instrument_id` is
/// not the one asked about: that difference is how a reader holding the
/// merged ID learns what it became.
pub fn resolve_instrument(
    store: &dyn Store,
    request: &ResolveInstrumentRequest,
) -> Result<ResolveInstrumentReply> {
    let became = current(store, &request.instrument_id)?;
    let record = store.by_id(&became)?.as_ref().map(to_wire);
    Ok(ResolveInstrumentReply {
        found: record.is_some(),
        instrument: record,
    })
}

/// The records this identifier meets on the date, the request's venue and
/// currency admitting them.
fn matches(
    store: &dyn Store,
    identifier: &Asked,
    request: &ResolveIdentifierRequest,
) -> Result<Vec<Instrument>> {
    Ok(store
        .matching(
            &identifier.scheme,
            &identifier.value,
            &identifier.source,
            request.as_of_ns,
        )?
        .into_iter()
        .filter(|record| qualifies(record, identifier, request))
        .collect())
}

/// The records other than `found`, as they are now, carrying `identifier` on
/// the date.
fn holders(
    store: &dyn Store,
    identifier: &Asked,
    as_of_ns: i64,
    found: &str,
) -> Result<Vec<String>> {
    let mut others = Vec::new();
    for record in store.matching(
        &identifier.scheme,
        &identifier.value,
        &identifier.source,
        as_of_ns,
    )? {
        let became = current(store, &record.instrument_id)?;
        if became != found && !others.contains(&became) {
            others.push(became);
        }
    }
    Ok(others)
}

/// Whether the request's venue and currency admit this match.
///
/// They narrow a source-scoped symbol, the identifier that needs narrowing: the
/// same ticker trades in several places. They do not narrow a global
/// identifier, which is unique already. A record that names no venue or no
/// currency yet -- one this deployment minted, until a person completes it --
/// is not narrowed by what it does not say.
fn qualifies(record: &Instrument, identifier: &Asked, request: &ResolveIdentifierRequest) -> bool {
    if identifier.source.is_empty() {
        return true;
    }
    let venue_ok = request.exchange_mic.is_empty()
        || record.exchange_mic.is_empty()
        || request.exchange_mic == record.exchange_mic;
    let currency_ok = request.currency.is_empty()
        || record.currency.is_empty()
        || request.currency == record.currency;
    venue_ok && currency_ok
}

/// What the plugin's source stated, as offers (the spec's Q6). Only what it
/// can be: a class the enum defines, a currency that is a code.
fn stated(request: &ResolveIdentifierRequest, instance_id: &str, now_ns: i64) -> Vec<Offer> {
    let words = format!("stated by {instance_id}");
    let offer = |field: Field, value: String| Offer {
        field,
        value,
        identifier: None,
        source: words.clone(),
        instance_id: instance_id.to_string(),
        offered_at_ns: now_ns,
    };
    let mut offers = Vec::new();
    if let Ok(class) = AssetClass::try_from(request.stated_asset_class) {
        if class != AssetClass::Unspecified {
            offers.push(offer(Field::AssetClass, asset_class_name(class as i32)));
        }
    }
    let currency = request.stated_currency.trim();
    if is_currency(currency) {
        offers.push(offer(Field::Currency, currency.to_string()));
    }
    let description = request.stated_description.trim();
    if !description.is_empty() {
        offers.push(offer(Field::Description, description.to_string()));
    }
    // A type, only under the class the source stated beside it (contract
    // v11): a statement that contradicts itself offers nothing a person could
    // accept.
    if let Ok(kind) = meridian_domain::v1::InstrumentType::try_from(request.stated_instrument_type)
    {
        let under = meridian_domain::instrument_type::class_of(kind);
        if under.is_some_and(|class| class as i32 == request.stated_asset_class) {
            offers.push(offer(
                Field::InstrumentType,
                meridian_domain::instrument_type::name(kind).to_string(),
            ));
        }
    }
    offers
}

/// Keep each offer that says something the record does not, in place of what
/// the same source offered for the field before. `true` when any changed.
///
/// An offer of the value already in force is no offer, and a later offer
/// never touches a value in force (the spec's requirement 11).
pub(crate) fn keep_offers(record: &mut Instrument, offers: Vec<Offer>) -> bool {
    let mut changed = false;
    for offer in offers {
        let in_force = match offer.field {
            Field::Identifier => offer
                .identifier
                .as_ref()
                .is_some_and(|identifier| record.carries(identifier)),
            field => record.value(field) == offer.value,
        };
        if in_force || record.offers.iter().any(|held| held.same_as(&offer)) {
            continue;
        }
        if offer.field != Field::Identifier {
            record.offers.retain(|held| {
                !(held.field == offer.field
                    && held.instance_id == offer.instance_id
                    && held.source == offer.source)
            });
        }
        record.offers.push(offer);
        changed = true;
    }
    changed
}

/// List a conflict for a person, or bring it up to date.
fn note(
    store: &dyn Store,
    identifiers: Vec<Asked>,
    mut instrument_ids: Vec<String>,
    reported_by: &str,
    now_ns: i64,
) -> Result<()> {
    instrument_ids.sort();
    instrument_ids.dedup();
    tracing::info!(
        records = ?instrument_ids,
        reported_by,
        "identifiers meet more than one record: listed for a person to merge"
    );
    store.note_conflict(Conflict {
        identifiers,
        instrument_ids,
        reported_by: reported_by.to_string(),
        first_seen_ns: now_ns,
        last_seen_ns: now_ns,
    })
}

pub(crate) fn reported_by(instance_id: &str) -> String {
    if instance_id.is_empty() {
        "reported by a plugin".into()
    } else {
        format!("reported by {instance_id}")
    }
}

pub(crate) fn identifier_change(identifier: &Asked, source: &str) -> Change {
    Change {
        field: Field::Identifier.name().into(),
        scheme: identifier.scheme.clone(),
        namespace: identifier.source.clone(),
        before: String::new(),
        after: identifier.value.clone(),
        source: source.to_string(),
    }
}

fn rank_of(asked: &Asked) -> usize {
    rank(&crate::record::asked_to_wire(asked))
}

fn unchanged(reply: ResolveIdentifierReply) -> Resolution {
    Resolution {
        reply,
        changed: None,
    }
}

fn resolved(instrument_id: String, minted: bool) -> ResolveIdentifierReply {
    ResolveIdentifierReply {
        found: true,
        instrument_id,
        miss_reason: MissReason::Unspecified as i32,
        minted,
    }
}

fn missed(reason: MissReason) -> ResolveIdentifierReply {
    ResolveIdentifierReply {
        found: false,
        instrument_id: String::new(),
        miss_reason: reason as i32,
        minted: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Identifier;
    use crate::MemoryStore;
    use meridian_domain::v1::Identifier as PbIdentifier;

    /// The fixture's as-of.
    const AS_OF: i64 = 1_757_289_600_000_000_000;

    /// When a record minted by these tests is minted.
    const NOW: i64 = 1_757_376_000_000_000_000;

    fn resolve(store: &dyn Store, request: &ResolveIdentifierRequest) -> ResolveIdentifierReply {
        resolve_identifier(store, request, "custody-snaptrade-1", NOW)
            .unwrap()
            .reply
    }

    fn asked(scheme: &str, value: &str, source: &str) -> PbIdentifier {
        PbIdentifier {
            scheme: scheme.into(),
            value: value.into(),
            source: source.into(),
        }
    }

    fn request(identifiers: Vec<PbIdentifier>) -> ResolveIdentifierRequest {
        ResolveIdentifierRequest {
            identifiers,
            as_of_ns: AS_OF,
            exchange_mic: "XNAS".into(),
            currency: "USD".into(),
            ..Default::default()
        }
    }

    /// A record applied from the platform before v10, as the migration left
    /// it.
    fn platform_record(
        instrument_id: &str,
        identifiers: Vec<(&str, &str, &str, i64)>,
    ) -> Instrument {
        Instrument {
            instrument_id: instrument_id.into(),
            identifiers: identifiers
                .into_iter()
                .map(|(scheme, value, source, from)| Identifier {
                    scheme: scheme.into(),
                    value: value.into(),
                    source: source.into(),
                    valid_from_ns: from,
                    valid_to_ns: None,
                })
                .collect(),
            asset_class: "ASSET_CLASS_EQUITY".into(),
            currency: "USD".into(),
            exchange_mic: "XNAS".into(),
            description: "Apple Inc. common stock".into(),
            lifecycle_state: "INSTRUMENT_LIFECYCLE_STATE_ACTIVE".into(),
            version: 4,
            valid_from_ns: 0,
            record_time_ns: 0,
            instrument_type: String::new(),
            money_market_fund: String::new(),
            listing_venue_id: String::new(),
            sources: Vec::new(),
            offers: Vec::new(),
        }
    }

    fn hold(store: &MemoryStore, record: Instrument) {
        let key = format!("held:{}", record.instrument_id);
        let first = Version {
            acting_through_delegation: String::new(),
            client_name: String::new(),
            instrument_id: record.instrument_id.clone(),
            version: record.version,
            operation: "migrate".into(),
            changes: Vec::new(),
            person: String::new(),
            instance_id: String::new(),
            note: String::new(),
            merged_instrument_id: String::new(),
            record_time_ns: 0,
        };
        store.mint(record, &key, first).unwrap();
    }

    #[test]
    fn the_fixture_request_resolves_to_the_fixture_record() {
        let store = MemoryStore::new();
        hold(
            &store,
            platform_record(
                "INS-01J8XQ4M7K0000000000AAPL",
                vec![
                    ("figi", "BBG000B9XRY4", "", 0),
                    ("symbol", "AAPL", "snaptrade", 0),
                ],
            ),
        );
        let reply = resolve(
            &store,
            &request(vec![
                asked("figi", "BBG000B9XRY4", ""),
                asked("symbol", "AAPL", "snaptrade"),
            ]),
        );
        assert!(reply.found);
        assert!(!reply.minted);
        assert_eq!(reply.instrument_id, "INS-01J8XQ4M7K0000000000AAPL");
    }

    #[test]
    fn nothing_matched_mints_a_record_once_and_matches_it_after() {
        let store = MemoryStore::new();
        let set = request(vec![asked("symbol", "ZZTOP", "snaptrade")]);
        let first = resolve_identifier(&store, &set, "custody-snaptrade-1", NOW).unwrap();
        assert!(first.reply.found && first.reply.minted);
        assert!(first.reply.instrument_id.starts_with(ids::LOCAL_PREFIX));
        let minted = first.changed.expect("a minted record is announced");
        assert_eq!(minted.version, 1);
        assert_eq!(minted.sources[0].instance_id, "custody-snaptrade-1");
        assert!(
            minted.asset_class.is_empty(),
            "nothing in force from a resolve"
        );

        let again = resolve(&store, &set);
        assert_eq!(again.instrument_id, first.reply.instrument_id);
        assert!(!again.minted, "matched, not minted, the second time");
        assert_eq!(store.count().unwrap(), 1);
    }

    #[test]
    fn a_second_identifier_joins_the_record_one_in_common_meets() {
        // The spec's Q4: SnapTrade's symbol, then a report carrying the symbol
        // and a FIGI: one record, and the FIGI joins it from the instance.
        let store = MemoryStore::new();
        let first = resolve(
            &store,
            &request(vec![asked("symbol", "SPAXX", "snaptrade")]),
        );
        let joined = resolve_identifier(
            &store,
            &request(vec![
                asked("symbol", "SPAXX", "snaptrade"),
                asked("figi", "BBG000SPAXX1", ""),
            ]),
            "market-data-1",
            NOW + 1,
        )
        .unwrap();
        assert_eq!(joined.reply.instrument_id, first.instrument_id);
        let record = joined.changed.expect("a join is a new version");
        assert_eq!(record.version, 2);
        let figi = record
            .source_of(
                Field::Identifier,
                Some(&Asked {
                    scheme: "figi".into(),
                    value: "BBG000SPAXX1".into(),
                    source: String::new(),
                }),
            )
            .unwrap();
        assert_eq!(figi.instance_id, "market-data-1");
        assert!(figi.person.is_empty());

        // And the FIGI alone now meets it.
        let by_figi = resolve(&store, &request(vec![asked("figi", "BBG000SPAXX1", "")]));
        assert_eq!(by_figi.instrument_id, first.instrument_id);
        assert_eq!(
            store.history(&first.instrument_id).unwrap()[0].operation,
            "join"
        );
    }

    #[test]
    fn identifiers_meeting_two_records_are_a_miss_and_a_conflict_and_join_nothing() {
        let store = MemoryStore::new();
        let by_symbol = resolve(
            &store,
            &request(vec![asked("symbol", "ZZTOP", "snaptrade")]),
        );
        let by_figi = resolve(&store, &request(vec![asked("figi", "BBG000ZZTOP1", "")]));
        assert_ne!(by_symbol.instrument_id, by_figi.instrument_id);

        let both = resolve(
            &store,
            &request(vec![
                asked("symbol", "ZZTOP", "snaptrade"),
                asked("figi", "BBG000ZZTOP1", ""),
            ]),
        );
        assert!(!both.found);
        assert_eq!(both.miss_reason, MissReason::Ambiguous as i32);
        let conflicts = store.conflicts().unwrap();
        assert_eq!(conflicts.len(), 1);
        let mut ids = vec![
            by_symbol.instrument_id.clone(),
            by_figi.instrument_id.clone(),
        ];
        ids.sort();
        assert_eq!(conflicts[0].instrument_ids, ids);
        assert_eq!(conflicts[0].reported_by, "custody-snaptrade-1");
        assert_eq!(
            store
                .by_id(&by_figi.instrument_id)
                .unwrap()
                .unwrap()
                .identifiers
                .len(),
            1,
            "nothing joined either record"
        );
    }

    #[test]
    fn more_than_one_match_in_a_tier_is_a_miss_rather_than_a_guess() {
        let store = MemoryStore::new();
        hold(
            &store,
            platform_record("INS-A", vec![("figi", "BBG000B9XRY4", "", 0)]),
        );
        hold(
            &store,
            platform_record("INS-B", vec![("figi", "BBG000B9XRY4", "", 0)]),
        );
        let reply = resolve(&store, &request(vec![asked("figi", "BBG000B9XRY4", "")]));
        assert!(!reply.found);
        assert_eq!(reply.miss_reason, MissReason::Ambiguous as i32);
        assert_eq!(store.count().unwrap(), 2, "ambiguity mints nothing");
    }

    #[test]
    fn a_global_scheme_answers_before_a_brokerage_symbol() {
        let store = MemoryStore::new();
        hold(
            &store,
            platform_record("INS-GLOBAL", vec![("figi", "BBG000B9XRY4", "", 0)]),
        );
        hold(
            &store,
            platform_record("INS-SCOPED", vec![("symbol", "AAPL", "snaptrade", 0)]),
        );
        // The two disagree; the FIGI decides, and the symbol, carried by
        // another record, is a contradiction rather than a join.
        let reply = resolve(
            &store,
            &request(vec![
                asked("figi", "BBG000B9XRY4", ""),
                asked("symbol", "AAPL", "snaptrade"),
            ]),
        );
        assert!(
            !reply.found,
            "a symbol on another record contradicts the FIGI's"
        );
        let by_figi_alone = resolve(&store, &request(vec![asked("figi", "BBG000B9XRY4", "")]));
        assert_eq!(by_figi_alone.instrument_id, "INS-GLOBAL");
    }

    #[test]
    fn venue_and_currency_narrow_a_symbol_match_once_the_record_says_them() {
        let store = MemoryStore::new();
        let mut cad = platform_record("INS-CAD", vec![("symbol", "AAPL", "snaptrade", 0)]);
        cad.currency = "CAD".into();
        cad.exchange_mic = "XTSE".into();
        hold(&store, cad);
        let reply = resolve(&store, &request(vec![asked("symbol", "AAPL", "snaptrade")]));
        assert!(
            reply.minted,
            "a USD listing in XNAS is not the CAD one in XTSE"
        );
        assert_ne!(reply.instrument_id, "INS-CAD");

        // A minted record says no venue or currency, and is not narrowed by
        // what it does not say.
        let again = resolve(&store, &request(vec![asked("symbol", "AAPL", "snaptrade")]));
        assert_eq!(again.instrument_id, reply.instrument_id);
    }

    #[test]
    fn an_identifier_does_not_resolve_before_the_mapping_existed() {
        let store = MemoryStore::new();
        hold(
            &store,
            platform_record("INS-LATE", vec![("figi", "BBG000LATE01", "", AS_OF + 1)]),
        );
        let reply = resolve(&store, &request(vec![asked("figi", "BBG000LATE01", "")]));
        assert!(reply.minted, "not the record whose mapping began later");
        assert_ne!(reply.instrument_id, "INS-LATE");
    }

    #[test]
    fn what_the_source_states_is_offered_and_never_in_force() {
        let store = MemoryStore::new();
        let mut stating = request(vec![asked("symbol", "SNAP1", "snaptrade")]);
        stating.stated_asset_class = AssetClass::Equity as i32;
        stating.stated_currency = "USD".into();
        stating.stated_description = "Snap One Holdings".into();
        let minted = resolve_identifier(&store, &stating, "custody-snaptrade-1", NOW)
            .unwrap()
            .changed
            .unwrap();
        assert!(minted.asset_class.is_empty() && minted.currency.is_empty());
        assert_eq!(minted.offers.len(), 3);
        assert!(minted
            .offers
            .iter()
            .all(|offer| offer.instance_id == "custody-snaptrade-1"));

        // Stated again, the same: nothing changes.
        let again = resolve_identifier(&store, &stating, "custody-snaptrade-1", NOW + 1).unwrap();
        assert!(again.changed.is_none());

        // A pseudo-currency is no offer.
        let mut base = request(vec![asked("symbol", "SNAP2", "snaptrade")]);
        base.stated_currency = "BASE".into();
        let minted = resolve_identifier(&store, &base, "custody-snaptrade-1", NOW)
            .unwrap()
            .changed
            .unwrap();
        assert!(minted.offers.is_empty());
    }

    #[test]
    fn a_merged_record_and_the_one_that_stays_are_one_match_and_not_two() {
        let store = MemoryStore::new();
        hold(
            &store,
            platform_record("LCL-MERGED", vec![("figi", "BBG000B9XRY4", "", 0)]),
        );
        hold(
            &store,
            platform_record("LCL-KEPT", vec![("figi", "BBG000B9XRY4", "", 0)]),
        );
        store.replace("LCL-MERGED", "LCL-KEPT", NOW).unwrap();
        let reply = resolve(&store, &request(vec![asked("figi", "BBG000B9XRY4", "")]));
        assert_eq!(reply.instrument_id, "LCL-KEPT");
    }

    #[test]
    fn a_request_carrying_no_identifiers_is_a_miss() {
        let store = MemoryStore::new();
        let reply = resolve(&store, &request(vec![]));
        assert!(!reply.found);
        assert_eq!(reply.miss_reason, MissReason::NotFound as i32);
        assert_eq!(store.count().unwrap(), 0);
    }

    #[test]
    fn a_record_resolves_with_its_sources_and_a_merged_one_answers_what_stays() {
        let store = MemoryStore::new();
        let minted = resolve(&store, &request(vec![asked("iso4217", "USD", "")]));
        let reply = resolve_instrument(
            &store,
            &ResolveInstrumentRequest {
                instrument_id: minted.instrument_id.clone(),
                as_of_ns: AS_OF,
            },
        )
        .unwrap();
        let record = reply.instrument.unwrap();
        assert_eq!(record.version, 1);
        assert_eq!(record.sources.len(), 1);
        assert_eq!(record.offers.len(), 2, "ISO 4217 offers cash in USD");

        hold(&store, platform_record("LCL-KEPT", vec![]));
        store
            .replace(&minted.instrument_id, "LCL-KEPT", NOW)
            .unwrap();
        let after = resolve_instrument(
            &store,
            &ResolveInstrumentRequest {
                instrument_id: minted.instrument_id,
                as_of_ns: AS_OF,
            },
        )
        .unwrap();
        assert_eq!(after.instrument.unwrap().instrument_id, "LCL-KEPT");

        let unheld = resolve_instrument(
            &store,
            &ResolveInstrumentRequest {
                instrument_id: "LCL-NONE".into(),
                as_of_ns: AS_OF,
            },
        )
        .unwrap();
        assert!(!unheld.found);
    }

    #[test]
    fn a_reported_conflict_is_listed_only_where_the_identifiers_meet_two_records() {
        let store = MemoryStore::new();
        let a = resolve(
            &store,
            &request(vec![asked("symbol", "ZZTOP", "snaptrade")]),
        );
        let event = MissingInstrumentDetectedEvent {
            source: "snaptrade".into(),
            identifiers: vec![
                asked("symbol", "ZZTOP", "snaptrade"),
                asked("figi", "BBG000ZZTOP1", ""),
            ],
            as_of_ns: AS_OF,
            publisher_instance_id: "custody-snaptrade-1".into(),
            reason: MissReason::Ambiguous as i32,
            observed_at_ns: NOW,
            ..Default::default()
        };
        assert!(!conflict_reported(&store, &event, NOW).unwrap());
        let b = resolve(&store, &request(vec![asked("figi", "BBG000ZZTOP1", "")]));
        assert_ne!(a.instrument_id, b.instrument_id);
        assert!(conflict_reported(&store, &event, NOW).unwrap());
        assert_eq!(store.conflicts().unwrap().len(), 1);
    }

    #[test]
    fn a_stated_type_is_offered_only_under_its_stated_class() {
        // Contract v11: a sweep fund stated as a money market fund under fund
        // is offered; a type off the class stated beside it offers nothing.
        let store = MemoryStore::new();
        let mut stating = ResolveIdentifierRequest {
            identifiers: vec![meridian_domain::v1::Identifier {
                scheme: "symbol".into(),
                value: "SPAXX".into(),
                source: "snaptrade".into(),
            }],
            as_of_ns: 1,
            stated_asset_class: AssetClass::Fund as i32,
            stated_instrument_type: meridian_domain::v1::InstrumentType::MoneyMarketFund as i32,
            ..Default::default()
        };
        let minted = resolve_identifier(&store, &stating, "custody-snaptrade-1", 1)
            .unwrap()
            .reply
            .instrument_id;
        let held = store.by_id(&minted).unwrap().unwrap();
        assert!(held
            .offers
            .iter()
            .any(|offer| offer.field == Field::InstrumentType
                && offer.value == "INSTRUMENT_TYPE_MONEY_MARKET_FUND"));
        assert!(held.instrument_type.is_empty(), "offered, not in force");

        stating.identifiers[0].value = "OTHER".into();
        stating.stated_asset_class = AssetClass::Equity as i32;
        let minted = resolve_identifier(&store, &stating, "custody-snaptrade-1", 2)
            .unwrap()
            .reply
            .instrument_id;
        let held = store.by_id(&minted).unwrap().unwrap();
        assert!(!held
            .offers
            .iter()
            .any(|offer| offer.field == Field::InstrumentType));
    }
}
