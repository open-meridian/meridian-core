//! Standing in for identity, and being replaced by it. W3.7 and W3.8.
//!
//! Only the platform mints identity, and every instrument ID is `INS-`. What
//! this store mints is a placeholder, `LCL-`, one per identifier set that
//! matched nothing, so a holding has a name at once, platform or no platform,
//! and every book can use it until the `INS-` ID arrives. The placeholder is
//! announced on the same event a connector's miss travels on; the conductor
//! escalates it; the platform pairs it with an instrument; the record comes
//! back on the pulled event naming the placeholder it replaces; and this store
//! records the replacement and says so, for the stores keyed by instrument to
//! move onto the `INS-` ID (W3.9).
//!
//! # Announced again until it is replaced
//!
//! Announcing once would make every outage permanent: a placeholder minted
//! while the conductor or the platform was away would never be asked about
//! again. So what is outstanding is announced at start and on an interval, and
//! the set of outstanding placeholders is the store's own, which is what makes
//! a restart, a restore from a backup, and an outage all the same harmless
//! case. The conductor's throttle keeps the repetition from becoming a burst,
//! and the platform answers the same placeholder with the same pairing.
//!
//! # Replaced, never deleted
//!
//! Holding rows keep the placeholder they were recorded with, because they
//! record what was reported. A reader holding one must still be able to learn
//! what it became, so a replaced ID answers its replacement from then on: a
//! resolve that would have answered the placeholder answers the `INS-` ID, and
//! resolving the placeholder itself answers the replacement's record.
//!
//! Replacements are followed, not looked up once. A placeholder replaced while
//! the platform still minted `LCL-` stubs is replaced by one of those, which is
//! itself replaced when the platform moves it to `INS-` (below), and a reader
//! of the first wants the last.
//!
//! # The legacy path
//!
//! Before 2026-09-28 the platform minted its stubs as `LCL-` and this store
//! applied them as ordinary instruments. The platform moves each of those to
//! a new `INS-` ID and keeps a pairing from the old one. So a held instrument
//! whose ID is `LCL-` and has not been replaced is announced exactly as an
//! outstanding placeholder is, naming itself as the placeholder, with its own
//! identifiers, asset class and date; the pairing comes back the same way,
//! and from then on it answers as a replaced placeholder does. Its row stays
//! in the instrument table, and a resolve that matches both it and its
//! replacement meets one instrument rather than two, because each candidate
//! answers what it became before candidates are counted.
//!
//! While the platform still mints `LCL-` stubs, an escalation of one of these
//! is answered with another stub rather than a pairing, so until the platform
//! moves to `INS-` the legacy path can add stubs rather than retire them. It
//! is announced anyway, because a deployment cannot tell which platform it is
//! talking to and the other failure is worse: one that never announced them
//! would never learn their `INS-` IDs.

use meridian_domain::v1::{
    Identifier as PbIdentifier, InstrumentLifecycleState, InstrumentRecord as PbInstrument,
    InstrumentReplacedEvent, MissReason, MissingInstrumentDetectedEvent,
};

use crate::apply::{asset_class_value, to_wire};
use crate::store::{Instrument, Placeholder, Result, Store};

/// How many replacements are followed from one ID before stopping.
///
/// Two is the longest a correct history makes: a placeholder, the stub an
/// older platform answered it with, and that stub's `INS-` ID. The rest is
/// room, and the bound is there so a cycle written by a defect somewhere else
/// ends a resolve rather than hanging it.
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

/// W3.7. The miss event announcing a placeholder, minted now or outstanding.
///
/// Not-found, because that is what minted it: an ambiguous resolve never has a
/// placeholder. Carries every identifier asked with, so the conductor can pull
/// on a global scheme before it escalates.
pub fn announcement(
    placeholder: &Placeholder,
    publisher_instance_id: &str,
    observed_at_ns: i64,
) -> MissingInstrumentDetectedEvent {
    MissingInstrumentDetectedEvent {
        source: placeholder.source.clone(),
        asset_class: asset_class_value(&placeholder.asset_class),
        identifiers: placeholder
            .identifiers
            .members()
            .iter()
            .map(|member| PbIdentifier {
                scheme: member.scheme.clone(),
                value: member.value.clone(),
                source: member.source.clone(),
            })
            .collect(),
        as_of_ns: placeholder.as_of_ns,
        publisher_instance_id: publisher_instance_id.to_string(),
        reason: MissReason::NotFound as i32,
        observed_at_ns,
        placeholder_instrument_id: placeholder.placeholder_id.clone(),
    }
}

/// The legacy path's announcement: an `LCL-` instrument the platform minted,
/// naming itself as the placeholder.
///
/// Dated from the record, whose `valid_from_ns` is the as-of the escalation
/// that minted it carried.
fn legacy_announcement(
    instrument: &Instrument,
    publisher_instance_id: &str,
    observed_at_ns: i64,
) -> MissingInstrumentDetectedEvent {
    MissingInstrumentDetectedEvent {
        source: instrument
            .identifiers
            .iter()
            .map(|identifier| identifier.source.as_str())
            .find(|source| !source.is_empty())
            .unwrap_or_default()
            .to_string(),
        asset_class: asset_class_value(&instrument.asset_class),
        identifiers: to_wire(instrument).identifiers,
        as_of_ns: instrument.valid_from_ns,
        publisher_instance_id: publisher_instance_id.to_string(),
        reason: MissReason::NotFound as i32,
        observed_at_ns,
        placeholder_instrument_id: instrument.instrument_id.clone(),
    }
}

/// W3.7, again. Everything not yet replaced, as the events to publish.
///
/// Placeholders minted here first, then the legacy path's, each in ID order,
/// so a repeated announcement is a repeated sequence.
pub fn outstanding(
    store: &dyn Store,
    publisher_instance_id: &str,
    observed_at_ns: i64,
) -> Result<Vec<MissingInstrumentDetectedEvent>> {
    let minted = store
        .outstanding()?
        .into_iter()
        .map(|placeholder| announcement(&placeholder, publisher_instance_id, observed_at_ns));

    let legacy = store
        .legacy_outstanding()?
        .into_iter()
        .map(|instrument| legacy_announcement(&instrument, publisher_instance_id, observed_at_ns));

    Ok(minted.chain(legacy).collect())
}

/// W3.6 for a placeholder not yet replaced: a record for the placeholder
/// itself.
///
/// So resolving any ID a holding can carry answers something, and a display
/// can show what the placeholder stands for rather than an opaque key. It is
/// built on demand and never written to the instrument table, and it says
/// what it is: DEFINE, the state of an instrument awaiting its definition, and
/// version 0, which no record from the platform carries, so nothing reading it
/// can take it for the authority's word.
pub fn record_of(placeholder: &Placeholder) -> PbInstrument {
    PbInstrument {
        instrument_id: placeholder.placeholder_id.clone(),
        identifiers: placeholder
            .identifiers
            .members()
            .iter()
            .map(|member| PbIdentifier {
                scheme: member.scheme.clone(),
                value: member.value.clone(),
                source: member.source.clone(),
            })
            .collect(),
        asset_class: asset_class_value(&placeholder.asset_class),
        lifecycle_state: InstrumentLifecycleState::Define as i32,
        version: 0,
        valid_from_ns: placeholder.as_of_ns,
        record_time_ns: placeholder.minted_at_ns,
        ..Default::default()
    }
}

/// W3.8. The event announcing that `replaced_id` is now `record`'s
/// instrument, when that is news. Writes nothing: the caller records the
/// replacement with [`Store::replace`] once the event is published.
///
/// Recorded after it is announced rather than before, because the other order
/// loses the announcement for good when the publish fails: the replacement
/// would be recorded, the placeholder no longer outstanding, and nothing would
/// ever say it again, leaving the street store's positions under a
/// placeholder nobody is replacing. This order at worst announces it twice,
/// and the second moves nothing.
///
/// `None` when there is nothing to announce: no placeholder named, a record
/// naming itself, or a replacement already recorded. The last is the
/// re-announced placeholder's second answer, and a redelivery; announcing it
/// again would make a subscriber's work depend on how many times it heard.
///
/// The event carries the record the store now holds rather than the one that
/// arrived, which may be older than it.
pub fn replacement(
    store: &dyn Store,
    replaced_id: &str,
    record: &PbInstrument,
    now_ns: i64,
) -> Result<Option<InstrumentReplacedEvent>> {
    // A record naming itself is what an older platform answers when a legacy
    // `LCL-` instrument is announced and it is pulled by a global identifier.
    // Recording it would make the ID its own replacement.
    if replaced_id.is_empty() || replaced_id == record.instrument_id {
        return Ok(None);
    }

    if let Some(replaced_by) = store.replacement_of(replaced_id)? {
        if replaced_by != record.instrument_id {
            // The first pairing stands. A second, different one is the
            // platform changing its mind, which staff mapping one instrument
            // onto another (W1.11) will say with its own event.
            tracing::warn!(
                replaced_id,
                replaced_by,
                offered = record.instrument_id,
                "a placeholder already replaced was offered a different replacement"
            );
        }
        return Ok(None);
    }

    let held = store
        .by_id(&record.instrument_id)?
        .map(|instrument| to_wire(&instrument))
        .unwrap_or_else(|| record.clone());

    Ok(Some(InstrumentReplacedEvent {
        replaced_instrument_id: replaced_id.to_string(),
        instrument: Some(held),
        replaced_at_ns: now_ns,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Asked, Identifier, IdentifierSet, Stood};
    use crate::MemoryStore;
    use meridian_domain::v1::AssetClass;

    const AS_OF: i64 = 1_757_289_600_000_000_000;
    const NOW: i64 = 1_757_376_000_000_000_000;

    fn placeholder(placeholder_id: &str) -> Placeholder {
        Placeholder {
            placeholder_id: placeholder_id.into(),
            identifiers: IdentifierSet::new([
                Asked {
                    scheme: "symbol".into(),
                    value: "ZZTOP".into(),
                    source: "snaptrade".into(),
                },
                Asked {
                    scheme: "figi".into(),
                    value: "BBG000ZZTOP1".into(),
                    source: String::new(),
                },
            ]),
            source: "snaptrade".into(),
            asset_class: String::new(),
            as_of_ns: AS_OF,
            minted_at_ns: NOW,
        }
    }

    fn instrument(instrument_id: &str, version: i64) -> Instrument {
        Instrument {
            instrument_id: instrument_id.into(),
            identifiers: vec![Identifier {
                scheme: "figi".into(),
                value: "BBG000ZZTOP1".into(),
                source: String::new(),
                valid_from_ns: AS_OF,
                valid_to_ns: None,
            }],
            asset_class: "ASSET_CLASS_EQUITY".into(),
            currency: "USD".into(),
            exchange_mic: "XNAS".into(),
            description: "ZZ Top Holdings".into(),
            lifecycle_state: "INSTRUMENT_LIFECYCLE_STATE_ACTIVE".into(),
            version,
            valid_from_ns: AS_OF,
            record_time_ns: AS_OF,
        }
    }

    #[test]
    fn the_announcement_is_the_fixtures_event() {
        // placeholder-announced.yaml, field for field.
        let event = announcement(
            &placeholder("LCL-01J8XQ4M7K0000000000ZZTP"),
            "instrument-1",
            NOW,
        );

        assert_eq!(event.source, "snaptrade");
        assert_eq!(event.identifiers.len(), 2);
        assert_eq!(event.as_of_ns, AS_OF);
        assert_eq!(event.publisher_instance_id, "instrument-1");
        assert_eq!(event.reason, MissReason::NotFound as i32);
        assert_eq!(event.observed_at_ns, NOW);
        assert_eq!(
            event.placeholder_instrument_id,
            "LCL-01J8XQ4M7K0000000000ZZTP"
        );
    }

    #[test]
    fn only_what_is_not_replaced_is_announced_again() {
        let store = MemoryStore::new();
        let (kept, _) = store.stand_in(placeholder("LCL-KEPT")).unwrap();
        let mut other = placeholder("LCL-GONE");
        other.identifiers = IdentifierSet::new([Asked {
            scheme: "symbol".into(),
            value: "GONE".into(),
            source: "snaptrade".into(),
        }]);
        let (gone, stood) = store.stand_in(other).unwrap();
        assert_eq!(stood, Stood::Minted);
        store
            .replace(&gone.placeholder_id, "INS-GONE", NOW)
            .unwrap();

        let announced = outstanding(&store, "instrument-1", NOW).unwrap();
        assert_eq!(announced.len(), 1);
        assert_eq!(announced[0].placeholder_instrument_id, kept.placeholder_id);
    }

    #[test]
    fn a_legacy_lcl_instrument_is_announced_as_its_own_placeholder() {
        // The legacy path: minted by the platform as LCL- before it minted
        // only INS-, applied here as an instrument, and announced so the
        // platform's pairing for it comes back.
        let store = MemoryStore::new();
        store.apply(instrument("LCL-LEGACY", 1)).unwrap();
        store.apply(instrument("INS-HELD", 1)).unwrap();

        let announced = outstanding(&store, "instrument-1", NOW).unwrap();
        assert_eq!(announced.len(), 1, "{announced:?}");

        let event = &announced[0];
        assert_eq!(event.placeholder_instrument_id, "LCL-LEGACY");
        assert_eq!(event.asset_class, AssetClass::Equity as i32);
        assert_eq!(event.identifiers[0].value, "BBG000ZZTOP1");
        assert_eq!(event.as_of_ns, AS_OF);
        assert_eq!(event.reason, MissReason::NotFound as i32);

        store.replace("LCL-LEGACY", "INS-NEW", NOW).unwrap();
        assert!(outstanding(&store, "instrument-1", NOW).unwrap().is_empty());
    }

    #[test]
    fn a_placeholder_record_says_it_awaits_definition() {
        let record = record_of(&placeholder("LCL-1"));

        assert_eq!(record.instrument_id, "LCL-1");
        assert_eq!(record.identifiers.len(), 2);
        assert_eq!(
            record.lifecycle_state,
            InstrumentLifecycleState::Define as i32
        );
        assert_eq!(
            record.version, 0,
            "no record from the platform is version 0"
        );
    }

    #[test]
    fn a_replacement_is_announced_until_it_is_recorded_and_not_after() {
        let store = MemoryStore::new();
        store.apply(instrument("INS-ZZTOP", 1)).unwrap();
        let record = to_wire(&instrument("INS-ZZTOP", 1));

        let event = replacement(&store, "LCL-1", &record, NOW).unwrap().unwrap();
        assert_eq!(event.replaced_instrument_id, "LCL-1");
        assert_eq!(event.instrument.unwrap().instrument_id, "INS-ZZTOP");
        assert_eq!(event.replaced_at_ns, NOW);

        // Nothing written yet, so a publish that failed is tried again.
        assert_eq!(store.replacement_of("LCL-1").unwrap(), None);

        store.replace("LCL-1", "INS-ZZTOP", NOW).unwrap();
        assert!(replacement(&store, "LCL-1", &record, NOW + 1)
            .unwrap()
            .is_none());
    }

    #[test]
    fn the_replacement_event_carries_the_record_held_rather_than_an_older_one() {
        let store = MemoryStore::new();
        store.apply(instrument("INS-ZZTOP", 4)).unwrap();

        let late = to_wire(&instrument("INS-ZZTOP", 2));
        let event = replacement(&store, "LCL-1", &late, NOW).unwrap().unwrap();
        assert_eq!(event.instrument.unwrap().version, 4);
    }

    #[test]
    fn a_record_naming_itself_replaces_nothing() {
        // An older platform, asked about a legacy LCL- instrument by its FIGI,
        // answers with that same instrument.
        let store = MemoryStore::new();
        let record = to_wire(&instrument("LCL-LEGACY", 1));

        assert!(replacement(&store, "LCL-LEGACY", &record, NOW)
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_chain_of_replacements_is_followed_to_its_end() {
        // A placeholder replaced by an older platform's LCL- stub, which the
        // platform later moved to INS-.
        let store = MemoryStore::new();
        store.replace("LCL-PLACEHOLDER", "LCL-STUB", NOW).unwrap();
        store.replace("LCL-STUB", "INS-FINAL", NOW).unwrap();

        assert_eq!(current(&store, "LCL-PLACEHOLDER").unwrap(), "INS-FINAL");
        assert_eq!(current(&store, "INS-FINAL").unwrap(), "INS-FINAL");
    }

    #[test]
    fn a_cycle_ends_rather_than_hangs() {
        let store = MemoryStore::new();
        store.replace("LCL-A", "LCL-B", NOW).unwrap();
        store.replace("LCL-B", "LCL-A", NOW).unwrap();

        assert_eq!(current(&store, "LCL-A").unwrap(), "LCL-B");
    }
}
