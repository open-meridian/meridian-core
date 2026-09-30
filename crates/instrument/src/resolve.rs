//! Answering instrument questions from what the instrument store holds.
//!
//! Three steps live here. W3.1 turns a set of identifiers into one instrument,
//! into the deployment's placeholder for the set, or into a miss. W3.6 turns an
//! instrument identifier into its record, so a holding can be shown with a
//! name. W3.2 turns a miss into a fact published on the bus, which is where
//! this crate's obligation ends.
//!
//! # Nothing matched is answered; more than one is not
//!
//! A set that matched nothing is answered with its placeholder (W3.7), minted
//! the first time the set is asked about and the same one every time after, so
//! the holding can be recorded and counted at once. An ambiguous set is not: a
//! placeholder would have to stand for one of the matches, which is the pick
//! ambiguity refuses. See [`crate::placeholder`].
//!
//! Every answer is what the ID has become. A placeholder since replaced
//! answers its `INS-` ID, and so does a legacy `LCL-` instrument, which is
//! also why a resolve that matches both a replaced ID and its replacement
//! meets one instrument and not two.
//!
//! # Identity is the answer; a ticker is an attribute
//!
//! A position and an order carry the canonical instrument identifier and
//! nothing else about identity. Tickers, symbols and venue codes are dated
//! attributes hanging off that identifier, true over a window and reassigned to
//! somebody else afterwards. So resolution runs once, at the boundary where
//! external data arrives, and everything downstream of it holds a key that
//! cannot go stale.
//!
//! The identifier is also never reused. A retired instrument does not free its
//! identifier for the next one, which is why forward resolution needs no as-of
//! to decide *which* instrument it is being asked about: there has only ever
//! been one.
//!
//! # Strongest first, and ambiguity is not a tiebreak
//!
//! A global scheme is tried before a source-scoped symbol, because a brokerage
//! symbol means nothing outside its own namespace. Within a tier, more than one
//! candidate is a miss rather than a choice. Picking would be wrong about half
//! the time, and it would be wrong silently, which is worse than being unable
//! to answer.
//!
//! An ambiguous tier does not fall through to a weaker one either. Resolving by
//! symbol what the FIGIs said was ambiguous would answer a question nobody
//! asked, using the evidence the caller trusted least.
//!
//! # What the instrument store cannot answer yet
//!
//! It holds one version per instrument, so an identifier dropped by a later
//! version is simply gone from it, and resolving as of a date when that mapping
//! was still true returns nothing. Nothing, not the wrong instrument: the
//! identifier's window is what gates the match, so a ticker since reassigned
//! cannot answer for its previous holder. A safe failure, and still a gap.
//! `design/replica-holds-one-version` owns closing it.

use meridian_domain::v1::{
    AssetClass, Identifier as PbIdentifier, MissReason, MissingInstrumentDetectedEvent,
    ResolveIdentifierReply, ResolveIdentifierRequest, ResolveInstrumentReply,
    ResolveInstrumentRequest,
};

use meridian_symbology::rank;

use crate::apply::to_wire;
use crate::ids;
use crate::placeholder::{current, record_of};
use crate::store::{Asked, IdentifierSet, Instrument, Placeholder, Result, Stood, Store};

/// What a resolve answered, and what it minted to answer it.
#[derive(Debug, Clone)]
pub struct Resolution {
    pub reply: ResolveIdentifierReply,

    /// The placeholder this resolve minted, which the caller announces (W3.7).
    /// `None` when it answered an instrument, a placeholder already held, or a
    /// miss, so a placeholder is announced as minted once and only once.
    pub minted: Option<Placeholder>,
}

/// W3.1 — which instrument this identifier set meant, on that date; or, when
/// nothing did, the deployment's placeholder for the set (W3.7).
///
/// `now_ns` is when a placeholder minted here is minted, passed in so a test
/// controls time.
pub fn resolve_identifier(
    store: &dyn Store,
    request: &ResolveIdentifierRequest,
    now_ns: i64,
) -> Result<Resolution> {
    let answered = |reply| Resolution {
        reply,
        minted: None,
    };

    let mut tiers: Vec<usize> = request.identifiers.iter().map(rank).collect();
    tiers.sort_unstable();
    tiers.dedup();

    for tier in tiers {
        let mut candidates: Vec<String> = Vec::new();

        for identifier in request.identifiers.iter().filter(|held| rank(held) == tier) {
            let matches = store.matching(
                &identifier.scheme,
                &identifier.value,
                &identifier.source,
                request.as_of_ns,
            )?;

            for instrument in matches {
                if !qualifies(&instrument, identifier, request) {
                    continue;
                }
                // What it has become, before counting. A legacy LCL- record and
                // the INS- record that replaced it both carry the identifiers,
                // and they are one instrument.
                let instrument_id = current(store, &instrument.instrument_id)?;

                // Two identifiers reaching the same instrument agree; they do
                // not compete. That holds only because an instrument identifier
                // is never reused, so equality of the key is equality of the
                // thing.
                if !candidates.contains(&instrument_id) {
                    candidates.push(instrument_id);
                }
            }
        }

        match candidates.len() {
            0 => continue,
            1 => return Ok(answered(resolved(candidates.remove(0)))),
            _ => return Ok(answered(missed(MissReason::Ambiguous))),
        }
    }

    stand_in(store, request, now_ns)
}

/// W3.7 — nothing matched, so answer the set's placeholder, minting it if this
/// is the first time the set has been asked about.
///
/// An empty request is still a miss. A placeholder for no identifiers would
/// stand for nothing, and every empty request would meet it.
fn stand_in(
    store: &dyn Store,
    request: &ResolveIdentifierRequest,
    now_ns: i64,
) -> Result<Resolution> {
    let identifiers = IdentifierSet::new(request.identifiers.iter().map(|identifier| Asked {
        scheme: identifier.scheme.clone(),
        value: identifier.value.clone(),
        source: identifier.source.clone(),
    }));
    if identifiers.is_empty() {
        return Ok(Resolution {
            reply: missed(MissReason::NotFound),
            minted: None,
        });
    }

    let (placeholder, stood) = store.stand_in(Placeholder {
        placeholder_id: ids::placeholder(now_ns),
        identifiers,
        source: source_of(request).to_string(),
        asset_class: String::new(),
        as_of_ns: request.as_of_ns,
        minted_at_ns: now_ns,
    })?;

    // Replaced, so the set has an identity now even though nothing held
    // carries these identifiers: the platform may have paired the placeholder
    // with an instrument it knows by others.
    if stood == Stood::AlreadyHeld {
        let became = current(store, &placeholder.placeholder_id)?;
        if became != placeholder.placeholder_id {
            return Ok(Resolution {
                reply: resolved(became),
                minted: None,
            });
        }
    }

    Ok(Resolution {
        reply: ResolveIdentifierReply {
            found: true,
            instrument_id: placeholder.placeholder_id.clone(),
            miss_reason: MissReason::Unspecified as i32,
            placeholder: true,
        },
        minted: match stood {
            Stood::Minted => Some(placeholder),
            Stood::AlreadyHeld => None,
        },
    })
}

/// W3.6 — the record behind an instrument identifier.
///
/// `as_of_ns` does not select which instrument. Identity is never reused, so
/// the key answers that on its own. It selects which version's attributes were
/// true then, and the instrument store holds one version, so the answer here is the
/// version held whatever the as-of. Stale attributes on a stable identity, and
/// the same gap `design/replica-holds-one-version` covers.
///
/// A replaced ID answers its replacement's record, whose `instrument_id` is
/// not the one asked about: that difference is how a reader holding a
/// placeholder learns what it became. A placeholder not yet replaced answers a
/// record for itself, in DEFINE at version 0 (see [`record_of`]).
pub fn resolve_instrument(
    store: &dyn Store,
    request: &ResolveInstrumentRequest,
) -> Result<ResolveInstrumentReply> {
    let became = current(store, &request.instrument_id)?;

    let record = match store.by_id(&became)? {
        Some(held) => Some(to_wire(&held)),
        None => store.placeholder(&became)?.as_ref().map(record_of),
    };

    Ok(ResolveInstrumentReply {
        found: record.is_some(),
        instrument: record,
    })
}

/// W3.2 — the miss, as a fact to publish.
///
/// `None` when the resolution found something, so a caller cannot announce a
/// miss that did not happen.
///
/// A fact and not a request: it reports what was held and stops. This crate has
/// no authority to mint an instrument, and keeping that authority on the far
/// side of an event boundary is what stops a misbehaving feed from filling the
/// master with junk.
pub fn missing_instrument(
    request: &ResolveIdentifierRequest,
    reply: &ResolveIdentifierReply,
    asset_class: AssetClass,
    publisher_instance_id: &str,
    observed_at_ns: i64,
) -> Option<MissingInstrumentDetectedEvent> {
    if reply.found {
        return None;
    }

    Some(MissingInstrumentDetectedEvent {
        source: source_of(request).to_string(),
        asset_class: asset_class as i32,

        // Everything held, not just what was tried. A reader with access to the
        // platform may be able to pull on a scheme this store could not.
        identifiers: request.identifiers.clone(),
        as_of_ns: request.as_of_ns,
        publisher_instance_id: publisher_instance_id.to_string(),
        reason: reply.miss_reason,
        observed_at_ns,
        placeholder_instrument_id: String::new(),
    })
}

/// Whether the request's venue and currency admit this match.
///
/// They narrow a source-scoped symbol, which is the identifier that needs
/// narrowing: the same ticker trades in several places. They do not narrow a
/// global identifier, which is unique already, and where a stale venue on the
/// request would only suppress a correct answer.
fn qualifies(
    instrument: &Instrument,
    identifier: &PbIdentifier,
    request: &ResolveIdentifierRequest,
) -> bool {
    if identifier.source.is_empty() {
        return true;
    }

    let venue_ok =
        request.exchange_mic.is_empty() || request.exchange_mic == instrument.exchange_mic;
    let currency_ok = request.currency.is_empty() || request.currency == instrument.currency;

    venue_ok && currency_ok
}

/// The namespace the miss happened in, for a reader deciding who to ask.
fn source_of(request: &ResolveIdentifierRequest) -> &str {
    request
        .identifiers
        .iter()
        .map(|identifier| identifier.source.as_str())
        .find(|source| !source.is_empty())
        .unwrap_or_default()
}

fn resolved(instrument_id: String) -> ResolveIdentifierReply {
    ResolveIdentifierReply {
        found: true,
        instrument_id,
        miss_reason: MissReason::Unspecified as i32,
        placeholder: false,
    }
}

fn missed(reason: MissReason) -> ResolveIdentifierReply {
    ResolveIdentifierReply {
        found: false,
        instrument_id: String::new(),
        miss_reason: reason as i32,
        placeholder: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Identifier;
    use crate::MemoryStore;

    /// The fixture's as-of.
    const AS_OF: i64 = 1_757_289_600_000_000_000;

    /// Long before it, so a mapping is comfortably in force.
    const EFFECTIVE: i64 = 1_700_000_000_000_000_000;

    /// When a placeholder minted by these tests is minted.
    const NOW: i64 = 1_757_376_000_000_000_000;

    /// W3.1's answer alone, for the tests that are about the answer.
    fn resolve(
        store: &dyn Store,
        request: &ResolveIdentifierRequest,
    ) -> Result<ResolveIdentifierReply> {
        resolve_identifier(store, request, NOW).map(|resolution| resolution.reply)
    }

    fn held(scheme: &str, value: &str, source: &str, valid_from_ns: i64) -> Identifier {
        Identifier {
            scheme: scheme.into(),
            value: value.into(),
            source: source.into(),
            valid_from_ns,
            valid_to_ns: None,
        }
    }

    fn instrument(instrument_id: &str, identifiers: Vec<Identifier>) -> Instrument {
        Instrument {
            instrument_id: instrument_id.into(),
            identifiers,
            asset_class: "ASSET_CLASS_EQUITY".into(),
            currency: "USD".into(),
            exchange_mic: "XNAS".into(),
            description: "Apple Inc. common stock".into(),
            lifecycle_state: "INSTRUMENT_LIFECYCLE_STATE_ACTIVE".into(),
            version: 1,
            valid_from_ns: EFFECTIVE,
            record_time_ns: EFFECTIVE,
        }
    }

    fn asked(scheme: &str, value: &str, source: &str) -> PbIdentifier {
        PbIdentifier {
            scheme: scheme.into(),
            value: value.into(),
            source: source.into(),
        }
    }

    /// The fixture's request: a FIGI and a brokerage symbol, narrowed by venue
    /// and currency.
    fn request(identifiers: Vec<PbIdentifier>) -> ResolveIdentifierRequest {
        ResolveIdentifierRequest {
            identifiers,
            as_of_ns: AS_OF,
            exchange_mic: "XNAS".into(),
            currency: "USD".into(),
        }
    }

    #[test]
    fn the_fixture_request_resolves_to_the_fixture_instrument() {
        let store = MemoryStore::new();
        store
            .apply(instrument(
                "INS-01J8XQ4M7K0000000000AAPL",
                vec![
                    held("figi", "BBG000B9XRY4", "", EFFECTIVE),
                    held("symbol", "AAPL", "snaptrade", EFFECTIVE),
                ],
            ))
            .unwrap();

        let reply = resolve(
            &store,
            &request(vec![
                asked("figi", "BBG000B9XRY4", ""),
                asked("symbol", "AAPL", "snaptrade"),
            ]),
        )
        .unwrap();

        assert!(reply.found);
        assert_eq!(reply.instrument_id, "INS-01J8XQ4M7K0000000000AAPL");
    }

    #[test]
    fn a_global_scheme_answers_before_a_brokerage_symbol() {
        // The two identifiers disagree, which is exactly when the order matters.
        // A symbol is meaningful only inside its namespace, so it loses.
        let store = MemoryStore::new();
        store
            .apply(instrument(
                "INS-GLOBAL",
                vec![held("figi", "BBG000B9XRY4", "", EFFECTIVE)],
            ))
            .unwrap();
        store
            .apply(instrument(
                "INS-SCOPED",
                vec![held("symbol", "AAPL", "snaptrade", EFFECTIVE)],
            ))
            .unwrap();

        let reply = resolve(
            &store,
            &request(vec![
                asked("symbol", "AAPL", "snaptrade"),
                asked("figi", "BBG000B9XRY4", ""),
            ]),
        )
        .unwrap();

        assert_eq!(reply.instrument_id, "INS-GLOBAL");
    }

    #[test]
    fn a_weaker_global_scheme_answers_when_the_stronger_one_is_not_held() {
        let store = MemoryStore::new();
        store
            .apply(instrument(
                "INS-BY-ISIN",
                vec![held("isin", "US0378331005", "", EFFECTIVE)],
            ))
            .unwrap();

        let reply = resolve(
            &store,
            &request(vec![
                asked("figi", "BBG000B9XRY4", ""),
                asked("isin", "US0378331005", ""),
            ]),
        )
        .unwrap();

        assert_eq!(reply.instrument_id, "INS-BY-ISIN");
    }

    #[test]
    fn nothing_matched_is_answered_with_a_placeholder_minted_once() {
        // The fixture's first case. The holding has a name at once, and the
        // same name every time the set is asked about.
        let store = MemoryStore::new();
        let asking = request(vec![asked("symbol", "ZZTOP", "snaptrade")]);

        let first = resolve_identifier(&store, &asking, NOW).unwrap();
        assert!(first.reply.found);
        assert!(first.reply.placeholder);
        assert!(first.reply.instrument_id.starts_with("LCL-"));
        let minted = first.minted.expect("the first resolve mints");
        assert_eq!(minted.placeholder_id, first.reply.instrument_id);
        assert_eq!(minted.source, "snaptrade");
        assert_eq!(minted.as_of_ns, AS_OF);

        let again = resolve_identifier(&store, &asking, NOW + 1).unwrap();
        assert_eq!(again.reply.instrument_id, first.reply.instrument_id);
        assert!(again.reply.placeholder);
        assert!(again.minted.is_none(), "announced as minted twice");
    }

    #[test]
    fn the_same_set_in_another_order_meets_the_same_placeholder() {
        let store = MemoryStore::new();

        let first = resolve(
            &store,
            &request(vec![
                asked("symbol", "ZZTOP", "snaptrade"),
                asked("figi", "BBG000ZZTOP1", ""),
            ]),
        )
        .unwrap();
        let reordered = resolve(
            &store,
            &request(vec![
                asked("figi", "BBG000ZZTOP1", ""),
                asked("symbol", "ZZTOP", "snaptrade"),
            ]),
        )
        .unwrap();

        assert_eq!(reordered.instrument_id, first.instrument_id);
        assert_eq!(store.outstanding().unwrap().len(), 1);
    }

    #[test]
    fn a_different_set_is_a_different_placeholder() {
        let store = MemoryStore::new();

        let one = resolve(
            &store,
            &request(vec![asked("symbol", "ZZTOP", "snaptrade")]),
        )
        .unwrap();
        let other = resolve(
            &store,
            &request(vec![
                asked("symbol", "ZZTOP", "snaptrade"),
                asked("figi", "BBG000ZZTOP1", ""),
            ]),
        )
        .unwrap();

        assert_ne!(one.instrument_id, other.instrument_id);
    }

    #[test]
    fn ambiguity_mints_no_placeholder() {
        // A placeholder would have to stand for one of the matches, which is
        // the pick ambiguity refuses.
        let store = MemoryStore::new();
        for instrument_id in ["INS-ONE", "INS-TWO"] {
            store
                .apply(instrument(
                    instrument_id,
                    vec![held("symbol", "AAPL", "snaptrade", EFFECTIVE)],
                ))
                .unwrap();
        }

        let resolution = resolve_identifier(
            &store,
            &request(vec![asked("symbol", "AAPL", "snaptrade")]),
            NOW,
        )
        .unwrap();

        assert!(!resolution.reply.found);
        assert!(!resolution.reply.placeholder);
        assert!(resolution.minted.is_none());
        assert!(store.outstanding().unwrap().is_empty());
    }

    #[test]
    fn a_replaced_placeholder_answers_its_instrument() {
        // The platform paired it with an instrument it knows by other
        // identifiers, so nothing held carries these. The set answers the INS-
        // ID all the same, and says it is not a placeholder.
        let store = MemoryStore::new();
        let asking = request(vec![asked("symbol", "ZZTOP", "snaptrade")]);
        let placeholder = resolve(&store, &asking).unwrap().instrument_id;

        store
            .apply(instrument(
                "INS-ZZTOP",
                vec![held("figi", "BBG000ZZTOP1", "", EFFECTIVE)],
            ))
            .unwrap();
        store.replace(&placeholder, "INS-ZZTOP", NOW).unwrap();

        let reply = resolve(&store, &asking).unwrap();
        assert!(reply.found);
        assert!(!reply.placeholder);
        assert_eq!(reply.instrument_id, "INS-ZZTOP");
    }

    #[test]
    fn a_legacy_record_and_its_replacement_are_one_match_and_not_two() {
        // The legacy path. The platform minted LCL- before it minted only INS-,
        // and moved it to INS- keeping the identifiers, so both rows carry
        // them. Counted as two, the set would turn ambiguous the moment its
        // identity arrived.
        let store = MemoryStore::new();
        for instrument_id in ["LCL-LEGACY", "INS-MOVED"] {
            store
                .apply(instrument(
                    instrument_id,
                    vec![held("figi", "BBG000B9XRY4", "", EFFECTIVE)],
                ))
                .unwrap();
        }
        store.replace("LCL-LEGACY", "INS-MOVED", NOW).unwrap();

        let reply = resolve(&store, &request(vec![asked("figi", "BBG000B9XRY4", "")])).unwrap();
        assert!(reply.found);
        assert_eq!(reply.instrument_id, "INS-MOVED");
    }

    #[test]
    fn more_than_one_match_is_a_miss_rather_than_a_guess() {
        // The fixture's second case. A silent pick would be wrong half the time.
        let store = MemoryStore::new();
        for instrument_id in ["INS-ONE", "INS-TWO"] {
            store
                .apply(instrument(
                    instrument_id,
                    vec![held("symbol", "AAPL", "snaptrade", EFFECTIVE)],
                ))
                .unwrap();
        }

        let reply = resolve(&store, &request(vec![asked("symbol", "AAPL", "snaptrade")])).unwrap();

        assert!(!reply.found);
        assert!(reply.instrument_id.is_empty());
        assert_eq!(reply.miss_reason, MissReason::Ambiguous as i32);
    }

    #[test]
    fn an_ambiguous_tier_does_not_fall_through_to_a_weaker_one() {
        // Two instruments claim the FIGI, one claims the symbol. Answering from
        // the symbol would resolve by the evidence the caller trusted least,
        // and would hide a contradiction in the evidence it trusted most.
        let store = MemoryStore::new();
        for instrument_id in ["INS-ONE", "INS-TWO"] {
            store
                .apply(instrument(
                    instrument_id,
                    vec![held("figi", "BBG000B9XRY4", "", EFFECTIVE)],
                ))
                .unwrap();
        }
        store
            .apply(instrument(
                "INS-THREE",
                vec![held("symbol", "AAPL", "snaptrade", EFFECTIVE)],
            ))
            .unwrap();

        let reply = resolve(
            &store,
            &request(vec![
                asked("figi", "BBG000B9XRY4", ""),
                asked("symbol", "AAPL", "snaptrade"),
            ]),
        )
        .unwrap();

        assert_eq!(reply.miss_reason, MissReason::Ambiguous as i32);
    }

    #[test]
    fn two_identifiers_reaching_one_instrument_are_not_ambiguous() {
        // They agree. That reads as agreement only because an instrument
        // identifier is never reused, so equality of the key is equality of the
        // instrument.
        let store = MemoryStore::new();
        store
            .apply(instrument(
                "INS-ONE",
                vec![
                    held("figi", "BBG000B9XRY4", "", EFFECTIVE),
                    held("isin", "US0378331005", "", EFFECTIVE),
                ],
            ))
            .unwrap();

        let reply = resolve(
            &store,
            &request(vec![
                asked("figi", "BBG000B9XRY4", ""),
                asked("isin", "US0378331005", ""),
            ]),
        )
        .unwrap();

        assert!(reply.found);
        assert_eq!(reply.instrument_id, "INS-ONE");
    }

    #[test]
    fn venue_and_currency_narrow_a_symbol_match() {
        let store = MemoryStore::new();
        store
            .apply(instrument(
                "INS-NASDAQ",
                vec![held("symbol", "AAPL", "snaptrade", EFFECTIVE)],
            ))
            .unwrap();

        let mut elsewhere = instrument(
            "INS-XETRA",
            vec![held("symbol", "AAPL", "snaptrade", EFFECTIVE)],
        );
        elsewhere.exchange_mic = "XETR".into();
        elsewhere.currency = "EUR".into();
        store.apply(elsewhere).unwrap();

        let reply = resolve(&store, &request(vec![asked("symbol", "AAPL", "snaptrade")])).unwrap();

        assert_eq!(reply.instrument_id, "INS-NASDAQ");
    }

    #[test]
    fn a_venue_qualifier_does_not_suppress_a_global_match() {
        // A FIGI is unique on its own. Narrowing it by a venue the caller
        // happened to send would turn a correct answer into a miss.
        let store = MemoryStore::new();
        let mut listed_elsewhere = instrument(
            "INS-XETRA",
            vec![held("figi", "BBG000B9XRY4", "", EFFECTIVE)],
        );
        listed_elsewhere.exchange_mic = "XETR".into();
        store.apply(listed_elsewhere).unwrap();

        let reply = resolve(&store, &request(vec![asked("figi", "BBG000B9XRY4", "")])).unwrap();

        assert_eq!(reply.instrument_id, "INS-XETRA");
    }

    #[test]
    fn an_identifier_does_not_resolve_before_the_mapping_existed() {
        let store = MemoryStore::new();
        store
            .apply(instrument(
                "INS-ONE",
                vec![held("figi", "BBG000B9XRY4", "", AS_OF + 1)],
            ))
            .unwrap();

        let reply = resolve(&store, &request(vec![asked("figi", "BBG000B9XRY4", "")])).unwrap();

        // Nothing matched then, so the set's placeholder, and not INS-ONE.
        assert!(reply.placeholder, "{reply:?}");
        assert_ne!(reply.instrument_id, "INS-ONE");
    }

    #[test]
    fn a_reassigned_ticker_misses_rather_than_answering_for_its_previous_holder() {
        // ZZTOP belonged to one instrument and was later given to another. A
        // statement from before the reassignment must not resolve to whoever
        // holds the ticker now.
        //
        // The instrument store keeps one version, so the earlier mapping is simply gone
        // and the honest answer is that nothing matched: the set's placeholder,
        // which the platform can pair with the right instrument. Recorded, with
        // the alternatives, in design/replica-holds-one-version.
        let store = MemoryStore::new();
        store
            .apply(instrument(
                "INS-NEW-HOLDER",
                vec![held("symbol", "ZZTOP", "snaptrade", AS_OF + 1)],
            ))
            .unwrap();

        let reply = resolve(
            &store,
            &request(vec![asked("symbol", "ZZTOP", "snaptrade")]),
        )
        .unwrap();

        assert!(reply.placeholder, "{reply:?}");
        assert_ne!(reply.instrument_id, "INS-NEW-HOLDER");
    }

    #[test]
    fn a_request_carrying_no_identifiers_is_a_miss() {
        let store = MemoryStore::new();
        let reply = resolve(&store, &request(vec![])).unwrap();

        assert_eq!(reply.miss_reason, MissReason::NotFound as i32);
    }

    #[test]
    fn an_instrument_resolves_by_its_canonical_identifier() {
        let store = MemoryStore::new();
        store
            .apply(instrument(
                "INS-01J8XQ4M7K0000000000AAPL",
                vec![held("figi", "BBG000B9XRY4", "", EFFECTIVE)],
            ))
            .unwrap();

        let reply = resolve_instrument(
            &store,
            &ResolveInstrumentRequest {
                instrument_id: "INS-01J8XQ4M7K0000000000AAPL".into(),
                as_of_ns: AS_OF,
            },
        )
        .unwrap();

        assert!(reply.found);
        let record = reply.instrument.unwrap();
        assert_eq!(record.instrument_id, "INS-01J8XQ4M7K0000000000AAPL");
        assert_eq!(record.description, "Apple Inc. common stock");
        assert_eq!(
            record.lifecycle_state,
            meridian_domain::v1::InstrumentLifecycleState::Active as i32
        );
    }

    #[test]
    fn forward_resolution_needs_no_as_of_to_know_which_instrument() {
        // Identity is never reused, so the key answers that by itself. The
        // as-of selects which version's attributes were true, and the instrument store
        // holds one.
        let store = MemoryStore::new();
        store
            .apply(instrument(
                "INS-ONE",
                vec![held("figi", "BBG000B9XRY4", "", EFFECTIVE)],
            ))
            .unwrap();

        let reply = resolve_instrument(
            &store,
            &ResolveInstrumentRequest {
                instrument_id: "INS-ONE".into(),
                as_of_ns: EFFECTIVE - 1,
            },
        )
        .unwrap();

        assert!(reply.found);
        assert_eq!(reply.instrument.unwrap().instrument_id, "INS-ONE");
    }

    #[test]
    fn a_placeholder_resolves_to_a_record_for_itself_until_it_is_replaced() {
        let store = MemoryStore::new();
        let placeholder = resolve(
            &store,
            &request(vec![
                asked("symbol", "ZZTOP", "snaptrade"),
                asked("figi", "BBG000ZZTOP1", ""),
            ]),
        )
        .unwrap()
        .instrument_id;

        let asking = ResolveInstrumentRequest {
            instrument_id: placeholder.clone(),
            as_of_ns: AS_OF,
        };

        let before = resolve_instrument(&store, &asking).unwrap();
        assert!(before.found);
        let record = before.instrument.unwrap();
        assert_eq!(record.instrument_id, placeholder);
        assert_eq!(record.identifiers.len(), 2);
        assert_eq!(
            record.lifecycle_state,
            meridian_domain::v1::InstrumentLifecycleState::Define as i32
        );
        assert_eq!(record.version, 0);

        // Then its identity arrives. Resolving the placeholder answers the
        // replacement's record, which is how a reader holding it learns what
        // it became.
        store
            .apply(instrument(
                "INS-ZZTOP",
                vec![held("figi", "BBG000ZZTOP1", "", EFFECTIVE)],
            ))
            .unwrap();
        store.replace(&placeholder, "INS-ZZTOP", NOW).unwrap();

        let after = resolve_instrument(&store, &asking).unwrap();
        assert!(after.found);
        assert_eq!(after.instrument.unwrap().instrument_id, "INS-ZZTOP");
    }

    #[test]
    fn a_replaced_legacy_record_resolves_to_its_replacement() {
        let store = MemoryStore::new();
        for instrument_id in ["LCL-LEGACY", "INS-MOVED"] {
            store
                .apply(instrument(
                    instrument_id,
                    vec![held("figi", "BBG000B9XRY4", "", EFFECTIVE)],
                ))
                .unwrap();
        }
        store.replace("LCL-LEGACY", "INS-MOVED", NOW).unwrap();

        let reply = resolve_instrument(
            &store,
            &ResolveInstrumentRequest {
                instrument_id: "LCL-LEGACY".into(),
                as_of_ns: AS_OF,
            },
        )
        .unwrap();
        assert_eq!(reply.instrument.unwrap().instrument_id, "INS-MOVED");
    }

    #[test]
    fn an_unheld_instrument_is_reported_as_not_found() {
        let store = MemoryStore::new();

        let reply = resolve_instrument(
            &store,
            &ResolveInstrumentRequest {
                instrument_id: "INS-NOBODY".into(),
                as_of_ns: AS_OF,
            },
        )
        .unwrap();

        assert!(!reply.found);
        assert!(reply.instrument.is_none());
    }

    #[test]
    fn a_resolution_that_found_something_produces_no_miss_event() {
        let request = request(vec![asked("figi", "BBG000B9XRY4", "")]);
        let reply = resolved("INS-ONE".into());

        assert!(missing_instrument(
            &request,
            &reply,
            AssetClass::Equity,
            "custody-snaptrade-1",
            1
        )
        .is_none());
    }

    #[test]
    fn a_miss_carries_everything_the_publisher_held() {
        // The fixture's event. Both identifiers travel, not just the one that
        // was tried: a reader with platform access may be able to pull on a
        // scheme this store could not.
        let request = ResolveIdentifierRequest {
            identifiers: vec![
                asked("symbol", "ZZTOP", "snaptrade"),
                asked("figi", "BBG000ZZTOP1", ""),
            ],
            as_of_ns: AS_OF,
            exchange_mic: String::new(),
            currency: String::new(),
        };

        let event = missing_instrument(
            &request,
            &missed(MissReason::NotFound),
            AssetClass::Equity,
            "custody-snaptrade-1",
            1_757_376_000_000_000_000,
        )
        .unwrap();

        assert_eq!(event.source, "snaptrade");
        assert_eq!(event.asset_class, AssetClass::Equity as i32);
        assert_eq!(event.identifiers.len(), 2);
        assert_eq!(event.as_of_ns, AS_OF);
        assert_eq!(event.publisher_instance_id, "custody-snaptrade-1");
        assert_eq!(event.reason, MissReason::NotFound as i32);
        assert_eq!(event.observed_at_ns, 1_757_376_000_000_000_000);
    }

    #[test]
    fn an_ambiguous_miss_reports_ambiguity_and_not_absence() {
        // The two are acted on differently downstream: absence may warrant a
        // pull, ambiguity warrants a human.
        let request = request(vec![asked("symbol", "AAPL", "snaptrade")]);

        let event = missing_instrument(
            &request,
            &missed(MissReason::Ambiguous),
            AssetClass::Equity,
            "custody-snaptrade-1",
            1,
        )
        .unwrap();

        assert_eq!(event.reason, MissReason::Ambiguous as i32);
    }
}
