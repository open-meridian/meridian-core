//! A record as the wire carries it, and what it lacks.
//!
//! A deliberate translation rather than storing the generated type directly.
//! The store's shape is the store's business and the wire's shape is the
//! contract's, and letting one be the other means a schema change reaches into
//! the store without passing anything that could object.

use meridian_domain::v1::{
    instrument_value, AssetClass, Identifier as PbIdentifier, InstrumentField,
    InstrumentRecord as PbInstrument, InstrumentType, InstrumentValue, InstrumentValueSource,
    OfferedValue,
};
use meridian_domain::{asset_class, instrument_type};

use crate::store::{Asked, Field, Identifier, Instrument, Offer, Source};

/// The scheme a currency's identifier is in ({scheme: iso4217, value: USD}).
pub const CURRENCY_SCHEME: &str = "iso4217";

/// The words a store-derived offer carries for a currency's record.
pub const ISO_4217: &str = "ISO 4217";

/// The held record as the wire carries it: its values with their sources, and
/// beside them every offer, those this store derives itself included (W3.6).
pub fn to_wire(instrument: &Instrument) -> PbInstrument {
    let mut offers: Vec<OfferedValue> = instrument.offers.iter().map(offer_to_wire).collect();
    offers.extend(derived_offers(instrument).iter().map(offer_to_wire));
    PbInstrument {
        instrument_id: instrument.instrument_id.clone(),
        identifiers: instrument
            .identifiers
            .iter()
            .map(|identifier| PbIdentifier {
                scheme: identifier.scheme.clone(),
                value: identifier.value.clone(),
                source: identifier.source.clone(),
            })
            .collect(),
        asset_class: asset_class_value(&instrument.asset_class),
        currency: instrument.currency.clone(),
        exchange_mic: instrument.exchange_mic.clone(),
        description: instrument.description.clone(),
        lifecycle_state: lifecycle_value(&instrument.lifecycle_state),
        version: instrument.version,
        valid_from_ns: instrument.valid_from_ns,
        record_time_ns: instrument.record_time_ns,
        sources: instrument.sources.iter().map(source_to_wire).collect(),
        offers,
        instrument_type: type_value(&instrument.instrument_type),
        money_market_fund: instrument_type::fund_from_text(&instrument.money_market_fund),
        // The venue master's ID (contract v18).
        listing_venue_id: instrument.listing_venue_id.clone(),
    }
}

/// A type as the store holds it, as the wire numbers it.
pub fn type_value(name: &str) -> i32 {
    instrument_type::read(name).unwrap_or(InstrumentType::Unspecified) as i32
}

/// A stored value as a person reads it in the history: a class by its word,
/// a type by its words, a fund's attributes in words.
pub fn value_words(field: Field, value: &str) -> String {
    match field {
        Field::AssetClass => class_words(value),
        Field::InstrumentType => instrument_type::read(value)
            .map(instrument_type::words)
            .unwrap_or(value)
            .to_string(),
        Field::MoneyMarketFund => instrument_type::fund_words(value),
        _ => value.to_string(),
    }
}

fn source_to_wire(source: &Source) -> InstrumentValueSource {
    InstrumentValueSource {
        field: field_to_wire(source.field) as i32,
        identifier: source.identifier.as_ref().map(asked_to_wire),
        source: source.source.clone(),
        person: source.person.clone(),
        instance_id: source.instance_id.clone(),
        recorded_at_ns: source.recorded_at_ns,
        note: source.note.clone(),
        // Contract v12: the delegation and client the person acted through.
        acting_through_delegation: source.acting_through_delegation.clone(),
        client_name: source.client_name.clone(),
    }
}

fn offer_to_wire(offer: &Offer) -> OfferedValue {
    OfferedValue {
        value: Some(InstrumentValue {
            value: Some(match offer.field {
                Field::AssetClass => {
                    instrument_value::Value::AssetClass(asset_class_value(&offer.value))
                }
                Field::Currency => instrument_value::Value::Currency(offer.value.clone()),
                Field::Description => instrument_value::Value::Description(offer.value.clone()),
                Field::InstrumentType => {
                    instrument_value::Value::InstrumentType(type_value(&offer.value))
                }
                Field::MoneyMarketFund => instrument_value::Value::MoneyMarketFund(
                    instrument_type::fund_from_text(&offer.value).unwrap_or_default(),
                ),
                Field::Identifier => instrument_value::Value::Identifier(
                    offer
                        .identifier
                        .as_ref()
                        .map(asked_to_wire)
                        .unwrap_or_default(),
                ),
            }),
            source: offer.source.clone(),
        }),
        instance_id: offer.instance_id.clone(),
        offered_at_ns: offer.offered_at_ns,
    }
}

pub fn asked_to_wire(asked: &Asked) -> PbIdentifier {
    PbIdentifier {
        scheme: asked.scheme.clone(),
        value: asked.value.clone(),
        source: asked.source.clone(),
    }
}

pub fn asked_from_wire(identifier: &PbIdentifier) -> Asked {
    Asked {
        scheme: identifier.scheme.trim().to_string(),
        value: identifier.value.trim().to_string(),
        source: identifier.source.trim().to_string(),
    }
}

/// A deployment's own identifier: in force from the start, since a deployment
/// keeps no effective dating of its own on day one, until a person ends it.
pub fn dated(asked: &Asked) -> Identifier {
    Identifier {
        scheme: asked.scheme.clone(),
        value: asked.value.clone(),
        source: asked.source.clone(),
        valid_from_ns: 0,
        valid_to_ns: None,
    }
}

pub fn field_to_wire(field: Field) -> InstrumentField {
    match field {
        Field::AssetClass => InstrumentField::AssetClass,
        Field::Currency => InstrumentField::Currency,
        Field::Description => InstrumentField::Description,
        Field::Identifier => InstrumentField::Identifier,
        Field::InstrumentType => InstrumentField::InstrumentType,
        Field::MoneyMarketFund => InstrumentField::MoneyMarketFund,
    }
}

pub fn field_from_wire(value: i32) -> Option<Field> {
    match InstrumentField::try_from(value).ok()? {
        InstrumentField::AssetClass => Some(Field::AssetClass),
        InstrumentField::Currency => Some(Field::Currency),
        InstrumentField::Description => Some(Field::Description),
        InstrumentField::Identifier => Some(Field::Identifier),
        InstrumentField::InstrumentType => Some(Field::InstrumentType),
        InstrumentField::MoneyMarketFund => Some(Field::MoneyMarketFund),
        InstrumentField::Unspecified => None,
    }
}

/// What a record lacks: an asset class or a currency, which the book requires
/// (W9.1), a description (W3.11), and for a money market fund its attributes
/// (contract v11), which the book's one lot at stable value waits on.
/// Computed from the fields; nobody sets it.
pub fn lacks(instrument: &Instrument) -> Vec<Field> {
    let mut lacking: Vec<Field> = [Field::AssetClass, Field::Currency, Field::Description]
        .into_iter()
        .filter(|field| instrument.value(*field).is_empty())
        .collect();
    if instrument_type::read(&instrument.instrument_type) == Some(InstrumentType::MoneyMarketFund)
        && instrument_type::fund_from_text(&instrument.money_market_fund).is_none()
    {
        lacking.push(Field::MoneyMarketFund);
    }
    lacking
}

/// An asset class and a currency in force: what the book needs.
pub fn complete_for_book(instrument: &Instrument) -> bool {
    !instrument.asset_class.is_empty() && !instrument.currency.is_empty()
}

/// Complete for the book, and a description. A symbol "where one exists" is
/// the platform's to know; a deployment joins every symbol its plugins report.
pub fn complete(instrument: &Instrument) -> bool {
    lacks(instrument).is_empty()
}

/// What this store offers of its own: a record carrying a currency's
/// identifier is cash in that currency, by ISO 4217, offered for a person to
/// accept where it is not in force already (the platform's rule, ruled
/// 2026-10-02: a record identified by `iso4217` with no class counts as cash).
pub fn derived_offers(instrument: &Instrument) -> Vec<Offer> {
    let Some(code) = instrument
        .identifiers
        .iter()
        .find(|identifier| identifier.scheme == CURRENCY_SCHEME && is_currency(&identifier.value))
        .map(|identifier| identifier.value.clone())
    else {
        return Vec::new();
    };
    let mut offers = Vec::new();
    let cash = asset_class::name(AssetClass::Cash).to_string();
    if instrument.asset_class.is_empty() {
        offers.push(derived(Field::AssetClass, cash, instrument));
    }
    if instrument.currency.is_empty() {
        offers.push(derived(Field::Currency, code, instrument));
    }
    offers
}

fn derived(field: Field, value: String, instrument: &Instrument) -> Offer {
    Offer {
        field,
        value,
        identifier: None,
        source: ISO_4217.into(),
        instance_id: String::new(),
        offered_at_ns: instrument.record_time_ns,
    }
}

/// An ISO 4217 code a record may carry: three capital letters, and neither
/// "no currency" (XXX) nor the testing code (XTS). A pseudo-currency such as
/// BASE is no code at all.
pub fn is_currency(code: &str) -> bool {
    code.len() == 3
        && code.bytes().all(|b| b.is_ascii_uppercase())
        && code != "XXX"
        && code != "XTS"
}

/// An asset class as the store holds it: the enum's name, or empty for none.
/// Text the enum does not define reads as none; nothing writes it, and
/// [`crate::PostgresStore::migrate`] mapped what the free-text column held.
pub fn asset_class_value(name: &str) -> i32 {
    asset_class::read(name).unwrap_or(AssetClass::Unspecified) as i32
}

/// The inverse of [`asset_class_value`]. A number the enum does not define is
/// none, as it is on the wire.
pub fn asset_class_name(value: i32) -> String {
    asset_class::name(AssetClass::try_from(value).unwrap_or(AssetClass::Unspecified)).to_string()
}

/// A class as a person reads it in the history: `cash`, `crypto_asset`.
pub fn class_words(name: &str) -> String {
    name.strip_prefix("ASSET_CLASS_")
        .unwrap_or(name)
        .to_ascii_lowercase()
}

pub fn lifecycle_value(name: &str) -> i32 {
    meridian_domain::v1::InstrumentLifecycleState::from_str_name(name)
        .unwrap_or(meridian_domain::v1::InstrumentLifecycleState::Unspecified) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(identifiers: Vec<Asked>) -> Instrument {
        Instrument {
            instrument_id: "LCL-1".into(),
            identifiers: identifiers.iter().map(dated).collect(),
            asset_class: String::new(),
            currency: String::new(),
            exchange_mic: String::new(),
            description: String::new(),
            lifecycle_state: "INSTRUMENT_LIFECYCLE_STATE_ACTIVE".into(),
            version: 1,
            valid_from_ns: 0,
            record_time_ns: 5,
            instrument_type: String::new(),
            money_market_fund: String::new(),
            listing_venue_id: String::new(),
            sources: Vec::new(),
            offers: Vec::new(),
        }
    }

    fn asked(scheme: &str, value: &str) -> Asked {
        Asked {
            scheme: scheme.into(),
            value: value.into(),
            source: String::new(),
        }
    }

    #[test]
    fn a_record_lacking_a_class_or_a_currency_is_not_complete_for_the_book() {
        let mut held = record(vec![]);
        assert_eq!(
            lacks(&held),
            vec![Field::AssetClass, Field::Currency, Field::Description]
        );
        assert!(!complete_for_book(&held));
        held.asset_class = "ASSET_CLASS_EQUITY".into();
        held.currency = "USD".into();
        assert!(complete_for_book(&held));
        assert!(!complete(&held), "a description is still wanted");
        held.description = "Snap One".into();
        assert!(complete(&held));
    }

    #[test]
    fn a_currency_record_is_offered_cash_in_its_code_and_nothing_once_in_force() {
        let mut held = record(vec![asked("iso4217", "USD")]);
        let offered = derived_offers(&held);
        assert_eq!(offered.len(), 2);
        assert_eq!(offered[0].value, "ASSET_CLASS_CASH");
        assert_eq!(offered[1].value, "USD");
        assert!(offered.iter().all(|offer| offer.source == ISO_4217));
        held.asset_class = "ASSET_CLASS_CASH".into();
        held.currency = "USD".into();
        assert!(derived_offers(&held).is_empty());
        let wire = to_wire(&held);
        assert!(wire.offers.is_empty());
    }

    #[test]
    fn a_pseudo_currency_is_no_currency() {
        assert!(is_currency("USD"));
        for code in ["BASE", "usd", "US", "XXX", "XTS", ""] {
            assert!(!is_currency(code), "{code}");
        }
    }
}
