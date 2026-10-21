//! The lake's vocabulary, read alike by the sidecar, the conductor, the lake
//! and the dashboard (contract v18, W10; spec/the-lake).
//!
//! The data types of the lake's 1a and 1b (contract v19: trades and quotes)
//! and the dictionary entries of their
//! fields (matrix/boundaries/fields/lake.yaml), by which a catalogue names the
//! optional fields a dataset fills and an entitlement the fields a plugin may
//! read; a dataset's ID in a deployment; and a catalogue held to the rules
//! W4.1 states, refused naming what is wrong, at a registration and at an
//! upload alike.

use meridian_pb::bounds::{
    CATALOGUE_DATASETS_COUNT, DATASET_DECLARATION_AGGREGATOR_LENGTH,
    DATASET_DECLARATION_DATA_TYPES_COUNT, DATASET_DECLARATION_DAY_END_MINUTE_RANGE,
    DATASET_DECLARATION_DAY_TIME_ZONE_LENGTH, DATASET_DECLARATION_KEY_LENGTH,
    DATASET_DECLARATION_MODES_COUNT, DATASET_DECLARATION_VENDOR_LENGTH,
    DATASET_LICENCE_DEFAULT_FIELDS_COUNT, DATASET_LICENCE_RETENTION_DAYS_RANGE,
};
use meridian_pb::v1::{Catalogue, DatasetDeclaration, DatasetLicence, DerivedUse, ObservationMode};

/// A price of one kind for one subject.
pub const PRICE: &str = "meridian.v1.Price";
/// Open, high, low and close over an interval.
pub const BAR: &str = "meridian.v1.Bar";

/// One trade as its source reported it (contract v19).
pub const TRADE: &str = "meridian.v1.Trade";
/// The best bid and offer (contract v19).
pub const QUOTE: &str = "meridian.v1.Quote";

/// An event contract's outcome at one stage (contract v23, the lake's 1c).
pub const OUTCOME: &str = "meridian.v1.Outcome";

/// The lake's data types: the 1a's, the 1b's from contract v19, and the
/// 1c's outcome from contract v23.
pub const DATA_TYPES: [&str; 5] = [PRICE, BAR, TRADE, QUOTE, OUTCOME];

/// The data types a default read takes a priority among (W10.3): an outcome
/// is read from every dataset that holds it, with no default (contract v23).
pub const PRIORITISED: [&str; 4] = [PRICE, BAR, TRADE, QUOTE];

const PRICE_FIELDS: [&str; 4] = [
    "meridian.v1.Price.meta",
    "meridian.v1.Price.kind",
    "meridian.v1.Price.price",
    "meridian.v1.Price.basis",
];
const BAR_FIELDS: [&str; 8] = [
    "meridian.v1.Bar.meta",
    "meridian.v1.Bar.open",
    "meridian.v1.Bar.high",
    "meridian.v1.Bar.low",
    "meridian.v1.Bar.close",
    "meridian.v1.Bar.volume",
    "meridian.v1.Bar.vwap",
    "meridian.v1.Bar.trade_count",
];

const TRADE_FIELDS: [&str; 7] = [
    "meridian.v1.Trade.meta",
    "meridian.v1.Trade.price",
    "meridian.v1.Trade.quantity",
    "meridian.v1.Trade.attributes",
    "meridian.v1.Trade.aggressor",
    "meridian.v1.Trade.source_sequence",
    "meridian.v1.Trade.cancelled",
];
const QUOTE_FIELDS: [&str; 6] = [
    "meridian.v1.Quote.meta",
    "meridian.v1.Quote.bid",
    "meridian.v1.Quote.ask",
    "meridian.v1.Quote.bid_quantity",
    "meridian.v1.Quote.ask_quantity",
    "meridian.v1.Quote.characteristics",
];

const OUTCOME_FIELDS: [&str; 6] = [
    "meridian.v1.Outcome.meta",
    "meridian.v1.Outcome.determination",
    "meridian.v1.Outcome.stage",
    "meridian.v1.Outcome.settlement_value",
    "meridian.v1.Outcome.settled_at_ns",
    "meridian.v1.Outcome.determination_as_reported",
];

/// The dictionary entries of a data type's fields; none for one the lake
/// does not have.
pub fn fields_of(data_type: &str) -> &'static [&'static str] {
    match data_type {
        PRICE => &PRICE_FIELDS,
        BAR => &BAR_FIELDS,
        TRADE => &TRADE_FIELDS,
        QUOTE => &QUOTE_FIELDS,
        OUTCOME => &OUTCOME_FIELDS,
        _ => &[],
    }
}

/// The data type an entry is a field of, where it is one.
pub fn type_of_entry(entry: &str) -> Option<&'static str> {
    DATA_TYPES
        .into_iter()
        .find(|data_type| fields_of(data_type).contains(&entry))
}

/// The data types a declaration's `data_types` names, entries aside.
pub fn types_served(declaration: &DatasetDeclaration) -> Vec<&'static str> {
    DATA_TYPES
        .into_iter()
        .filter(|data_type| declaration.data_types.iter().any(|d| d == data_type))
        .collect()
}

/// Whether `entry` is a field of a data type the declaration serves.
pub fn is_field_of(declaration: &DatasetDeclaration, entry: &str) -> bool {
    type_of_entry(entry).is_some_and(|data_type| types_served(declaration).contains(&data_type))
}

/// The fields a reader may not read of a price, removed (W10.1: the
/// receiving sidecar strips its plugin's deliveries, the lake its replies):
/// each field entry not in `allowed`, an empty set allowing every field. The
/// envelope is never removed. Answers the entries removed.
pub fn strip_price(
    price: &mut crate::v1::Price,
    allowed: &std::collections::BTreeSet<String>,
) -> Vec<&'static str> {
    let mut removed = Vec::new();
    if allowed.is_empty() {
        return removed;
    }
    let mut gone = |entry: &'static str| {
        let gone = !allowed.contains(entry);
        if gone {
            removed.push(entry);
        }
        gone
    };
    if gone(PRICE_FIELDS[1]) {
        price.kind = 0;
    }
    if gone(PRICE_FIELDS[2]) {
        price.price = None;
    }
    if gone(PRICE_FIELDS[3]) {
        price.basis = 0;
    }
    removed
}

/// [`strip_price`], for a bar.
pub fn strip_bar(
    bar: &mut crate::v1::Bar,
    allowed: &std::collections::BTreeSet<String>,
) -> Vec<&'static str> {
    let mut removed = Vec::new();
    if allowed.is_empty() {
        return removed;
    }
    let mut gone = |entry: &'static str| {
        let gone = !allowed.contains(entry);
        if gone {
            removed.push(entry);
        }
        gone
    };
    if gone(BAR_FIELDS[1]) {
        bar.open = None;
    }
    if gone(BAR_FIELDS[2]) {
        bar.high = None;
    }
    if gone(BAR_FIELDS[3]) {
        bar.low = None;
    }
    if gone(BAR_FIELDS[4]) {
        bar.close = None;
    }
    if gone(BAR_FIELDS[5]) {
        bar.volume = None;
    }
    if gone(BAR_FIELDS[6]) {
        bar.vwap = None;
    }
    if gone(BAR_FIELDS[7]) {
        bar.trade_count = None;
    }
    removed
}

/// [`strip_price`], for a trade (contract v19).
pub fn strip_trade(
    trade: &mut crate::v1::Trade,
    allowed: &std::collections::BTreeSet<String>,
) -> Vec<&'static str> {
    let mut removed = Vec::new();
    if allowed.is_empty() {
        return removed;
    }
    let mut gone = |entry: &'static str| {
        let gone = !allowed.contains(entry);
        if gone {
            removed.push(entry);
        }
        gone
    };
    if gone(TRADE_FIELDS[1]) {
        trade.price = None;
    }
    if gone(TRADE_FIELDS[2]) {
        trade.quantity = None;
    }
    if gone(TRADE_FIELDS[3]) {
        trade.attributes = None;
    }
    if gone(TRADE_FIELDS[4]) {
        trade.aggressor = 0;
    }
    if gone(TRADE_FIELDS[5]) {
        trade.source_sequence = 0;
    }
    if gone(TRADE_FIELDS[6]) {
        trade.cancelled = false;
    }
    removed
}

/// [`strip_price`], for a quote (contract v19).
pub fn strip_quote(
    quote: &mut crate::v1::Quote,
    allowed: &std::collections::BTreeSet<String>,
) -> Vec<&'static str> {
    let mut removed = Vec::new();
    if allowed.is_empty() {
        return removed;
    }
    let mut gone = |entry: &'static str| {
        let gone = !allowed.contains(entry);
        if gone {
            removed.push(entry);
        }
        gone
    };
    if gone(QUOTE_FIELDS[1]) {
        quote.bid = None;
    }
    if gone(QUOTE_FIELDS[2]) {
        quote.ask = None;
    }
    if gone(QUOTE_FIELDS[3]) {
        quote.bid_quantity = None;
    }
    if gone(QUOTE_FIELDS[4]) {
        quote.ask_quantity = None;
    }
    if gone(QUOTE_FIELDS[5]) {
        quote.characteristics.clear();
    }
    removed
}

/// [`strip_price`], for an outcome (contract v23).
pub fn strip_outcome(
    outcome: &mut crate::v1::Outcome,
    allowed: &std::collections::BTreeSet<String>,
) -> Vec<&'static str> {
    let mut removed = Vec::new();
    if allowed.is_empty() {
        return removed;
    }
    let mut gone = |entry: &'static str| {
        let gone = !allowed.contains(entry);
        if gone {
            removed.push(entry);
        }
        gone
    };
    if gone(OUTCOME_FIELDS[1]) {
        outcome.determination = 0;
    }
    if gone(OUTCOME_FIELDS[2]) {
        outcome.stage = 0;
    }
    if gone(OUTCOME_FIELDS[3]) {
        outcome.settlement_value = None;
    }
    if gone(OUTCOME_FIELDS[4]) {
        outcome.settled_at_ns = 0;
    }
    if gone(OUTCOME_FIELDS[5]) {
        outcome.determination_as_reported = None;
    }
    removed
}

/// The asset a row is priced in, by its Money's cash instrument (or its
/// code, before the lake resolves it): what a price's and a quote's latest
/// is chosen per, and their deliveries conflated per (contract v19, W10.5,
/// W10.6). Empty for a row with no amount.
pub fn asset_key(money: Option<&crate::v1::Money>) -> String {
    money
        .map(|m| {
            if m.instrument_id.is_empty() {
                m.currency_code.clone()
            } else {
                m.instrument_id.clone()
            }
        })
        .unwrap_or_default()
}

/// A quote's asset: its bid's, or its ask's where the bid side is empty.
pub fn quote_asset(quote: &crate::v1::Quote) -> String {
    let bid = asset_key(quote.bid.as_ref());
    if bid.is_empty() {
        asset_key(quote.ask.as_ref())
    } else {
        bid
    }
}

/// The fields a plugin instance may read of each dataset it is entitled to,
/// from the data configuration whole: its entitlement's fields, or, where
/// those name none, the licence's default fields -- the deployment's licence,
/// or its catalogue's default until one is set; an empty set for every field.
pub fn entitled_fields(
    event: &crate::v1::EntitlementsChangedEvent,
    instance: &str,
) -> std::collections::BTreeMap<String, std::collections::BTreeSet<String>> {
    let mut out = std::collections::BTreeMap::new();
    for entitlement in event
        .entitlements
        .iter()
        .filter(|e| e.instance == instance && e.allowed)
    {
        let Some(dataset) = event
            .datasets
            .iter()
            .find(|d| d.dataset == entitlement.dataset)
        else {
            continue;
        };
        let fields = if entitlement.fields.is_empty() {
            event
                .licences
                .iter()
                .find(|l| l.dataset == entitlement.dataset)
                .map(|l| l.default_fields.clone())
                .or_else(|| {
                    dataset
                        .declaration
                        .as_ref()
                        .and_then(|d| d.licence_default.as_ref())
                        .map(|l| l.default_fields.clone())
                })
                .unwrap_or_default()
        } else {
            entitlement.fields.clone()
        };
        out.insert(entitlement.dataset.clone(), fields.into_iter().collect());
    }
    out
}

/// The licence enforced (W10.1; spec/the-lake, Q8): the deployment's, or
/// the catalogue's default until one is set; a dataset declaring none is
/// kept, with no retention set. The lake keeps by it, and the Data sources
/// page draws it.
pub fn effective_licence(
    dataset: &str,
    set: Option<&DatasetLicence>,
    declaration: Option<&DatasetDeclaration>,
) -> DatasetLicence {
    if let Some(licence) = set {
        return licence.clone();
    }
    declaration
        .and_then(|d| d.licence_default.clone())
        .map(|terms| DatasetLicence {
            dataset: dataset.to_string(),
            ..terms
        })
        .unwrap_or_else(|| {
            let mut licence = DatasetLicence {
                dataset: dataset.to_string(),
                kept: true,
                display: true,
                derived_use_terms: DerivedUse::NotRestricted as i32,
                ..Default::default()
            };
            derived_use_both(&mut licence);
            licence
        })
}

/// A licence's derived-data answer as read (contract v27, ruling 479): its
/// `derived_use_terms`, or where that is unspecified the deprecated boolean,
/// false as internal_only and true as not_restricted (ruling 180).
#[allow(deprecated)]
pub fn derived_use_terms(licence: &DatasetLicence) -> DerivedUse {
    match DerivedUse::try_from(licence.derived_use_terms) {
        Ok(DerivedUse::Unspecified) | Err(_) => {
            if licence.derived_use {
                DerivedUse::NotRestricted
            } else {
                DerivedUse::InternalOnly
            }
        }
        Ok(terms) => terms,
    }
}

/// The deprecated boolean as a v27 writer sets it beside the answer: false
/// for internal_only, true for either other answer.
pub fn derived_use_bool(licence: &DatasetLicence) -> bool {
    derived_use_terms(licence) != DerivedUse::InternalOnly
}

/// A licence written from the dashboard's one box, "Derived data may be
/// made", with both fields set (ruling 479): ticked is not_restricted,
/// unticked internal_only.
pub fn licence_with_derived_use(licence: DatasetLicence, derived: bool) -> DatasetLicence {
    let mut licence = DatasetLicence {
        derived_use_terms: if derived {
            DerivedUse::NotRestricted
        } else {
            DerivedUse::InternalOnly
        } as i32,
        ..licence
    };
    derived_use_both(&mut licence);
    licence
}

/// Sets both of a licence's derived-data fields from whichever it states
/// (ruling 479: a v27 writer sets both): the answer where stated, else the
/// boolean read as the answer.
#[allow(deprecated)]
pub fn derived_use_both(licence: &mut DatasetLicence) {
    let terms = derived_use_terms(licence);
    licence.derived_use_terms = terms as i32;
    licence.derived_use = terms != DerivedUse::InternalOnly;
}

/// A dataset's ID in a deployment: its instance, a colon, and its key.
pub fn dataset_id(instance: &str, key: &str) -> String {
    format!("{instance}:{key}")
}

/// The instance a dataset's ID names, where it is one.
pub fn instance_of(dataset: &str) -> Option<&str> {
    dataset.split_once(':').map(|(instance, _)| instance)
}

fn key_reads(key: &str) -> bool {
    let bytes = key.as_bytes();
    DATASET_DECLARATION_KEY_LENGTH.admits(key.chars().count())
        && bytes.first().is_some_and(u8::is_ascii_lowercase)
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
}

/// A licence's terms held to their bounds, at `at`, with its fields entries
/// of the dataset's types.
pub fn licence_refused(
    licence: &DatasetLicence,
    declaration: Option<&DatasetDeclaration>,
    at: &str,
) -> Option<String> {
    if !DATASET_LICENCE_RETENTION_DAYS_RANGE.admits(i64::from(licence.retention_days)) {
        return Some(format!(
            "{at}.retention_days is {}; at most {}",
            licence.retention_days, DATASET_LICENCE_RETENTION_DAYS_RANGE.most
        ));
    }
    if !DATASET_LICENCE_DEFAULT_FIELDS_COUNT.admits(licence.default_fields.len()) {
        return Some(format!(
            "{at}.default_fields names {}; at most {}",
            licence.default_fields.len(),
            DATASET_LICENCE_DEFAULT_FIELDS_COUNT.most
        ));
    }
    if let Some(declaration) = declaration {
        for (i, field) in licence.default_fields.iter().enumerate() {
            if !is_field_of(declaration, field) {
                return Some(format!(
                    "{at}.default_fields[{i}] {field:?} is not an entry of the dataset's data types"
                ));
            }
        }
    }
    None
}

/// A catalogue held to W4.1's rules: refused on a version not holding `data`,
/// a key repeated or not of its form, a data type the lake does not have, a
/// field not an entry of its data type, an invalid time zone, or a value past
/// the dictionary's bounds; each naming where.
pub fn catalogue_refused(catalogue: &Catalogue, roles: &[String]) -> Option<String> {
    if catalogue.datasets.is_empty() {
        return None;
    }
    if !roles.iter().any(|role| role == "data") {
        return Some(format!(
            "declaration.catalogue declares datasets on a version holding {}, not data: the \
             catalogue is a data plugin's",
            if roles.is_empty() {
                "no role".to_string()
            } else {
                roles.join(", ")
            }
        ));
    }
    if !CATALOGUE_DATASETS_COUNT.admits(catalogue.datasets.len()) {
        return Some(format!(
            "declaration.catalogue.datasets names {}; at most {}",
            catalogue.datasets.len(),
            CATALOGUE_DATASETS_COUNT.most
        ));
    }
    let mut keys = std::collections::BTreeSet::new();
    for (i, dataset) in catalogue.datasets.iter().enumerate() {
        let at = format!("declaration.catalogue.datasets[{i}]");
        if !key_reads(&dataset.key) {
            return Some(format!(
                "{at}.key {:?} is not a dataset's key: 1 to 40 lowercase letters, digits and \
                 underscores, a letter first",
                dataset.key
            ));
        }
        if !keys.insert(dataset.key.as_str()) {
            return Some(format!("{at}.key {:?} is declared twice", dataset.key));
        }
        if !DATASET_DECLARATION_VENDOR_LENGTH.admits(dataset.vendor.chars().count()) {
            return Some(format!("{at}.vendor is 1 to 64 characters"));
        }
        if !DATASET_DECLARATION_AGGREGATOR_LENGTH.admits(dataset.aggregator.chars().count()) {
            return Some(format!("{at}.aggregator is at most 64 characters"));
        }
        if !DATASET_DECLARATION_DATA_TYPES_COUNT.admits(dataset.data_types.len()) {
            return Some(format!(
                "{at}.data_types names {}; 1 to {}",
                dataset.data_types.len(),
                DATASET_DECLARATION_DATA_TYPES_COUNT.most
            ));
        }
        for (j, named) in dataset.data_types.iter().enumerate() {
            let known = DATA_TYPES.contains(&named.as_str());
            match type_of_entry(named) {
                _ if known => {}
                Some(data_type) if dataset.data_types.iter().any(|d| d == data_type) => {}
                Some(data_type) => {
                    return Some(format!(
                        "{at}.data_types[{j}] {named:?} is a field of {data_type}, which the \
                         dataset does not serve"
                    ))
                }
                None => {
                    return Some(format!(
                        "{at}.data_types[{j}] {named:?} is not a data type the lake has, nor a \
                         field of one"
                    ))
                }
            }
        }
        if types_served(dataset).is_empty() {
            return Some(format!(
                "{at}.data_types names no data type the lake has ({})",
                DATA_TYPES.join(", ")
            ));
        }
        if !DATASET_DECLARATION_MODES_COUNT.admits(dataset.modes.len()) {
            return Some(format!("{at}.modes names {}; 1 to 3", dataset.modes.len()));
        }
        let mut modes = std::collections::BTreeSet::new();
        for (j, mode) in dataset.modes.iter().enumerate() {
            let known = ObservationMode::try_from(*mode)
                .ok()
                .filter(|m| *m != ObservationMode::Unspecified);
            if known.is_none() {
                return Some(format!("{at}.modes[{j}] is {mode}: pull, push or stream"));
            }
            if !modes.insert(*mode) {
                return Some(format!("{at}.modes[{j}] is named twice"));
            }
        }
        if !DATASET_DECLARATION_DAY_TIME_ZONE_LENGTH.admits(dataset.day_time_zone.chars().count())
            || (!dataset.day_time_zone.is_empty() && !crate::zones::is_zone(&dataset.day_time_zone))
        {
            return Some(format!(
                "{at}.day_time_zone {:?} is not an IANA time zone",
                dataset.day_time_zone
            ));
        }
        if !DATASET_DECLARATION_DAY_END_MINUTE_RANGE.admits(i64::from(dataset.day_end_minute)) {
            return Some(format!(
                "{at}.day_end_minute is {}; 0 to 1439",
                dataset.day_end_minute
            ));
        }
        // The settlement calendar its business dates follow (contract v22,
        // W10.1): empty names none, as before.
        if !dataset.calendar_id.is_empty() && !dataset.calendar_id.starts_with("CAL-") {
            return Some(format!(
                "{at}.calendar_id {:?} is not a platform calendar's ID (CAL-...)",
                dataset.calendar_id
            ));
        }
        if !dataset.venue_id.is_empty() && !dataset.venue_id.starts_with("VEN-") {
            return Some(format!(
                "{at}.venue_id {:?} is not a venue master ID (VEN-...)",
                dataset.venue_id
            ));
        }
        if let Some(licence) = &dataset.licence_default {
            if let Some(refused) =
                licence_refused(licence, Some(dataset), &format!("{at}.licence_default"))
            {
                return Some(refused);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn daily() -> DatasetDeclaration {
        DatasetDeclaration {
            key: "daily".into(),
            vendor: "Coinbase".into(),
            data_types: vec![PRICE.into(), BAR.into(), "meridian.v1.Bar.vwap".into()],
            modes: vec![ObservationMode::Pull as i32],
            cadence: 86_400,
            day_time_zone: "Etc/UTC".into(),
            ..Default::default()
        }
    }

    fn refused(dataset: DatasetDeclaration, roles: &[&str]) -> Option<String> {
        let roles: Vec<String> = roles.iter().map(|r| r.to_string()).collect();
        catalogue_refused(
            &Catalogue {
                datasets: vec![dataset],
            },
            &roles,
        )
    }

    #[test]
    fn a_catalogue_is_a_data_plugins_and_held_to_its_rules() {
        assert_eq!(refused(daily(), &["data"]), None);
        assert!(refused(daily(), &["reporting"])
            .unwrap()
            .contains("not data"));
        for (wrong, words) in [
            (
                DatasetDeclaration {
                    key: "Daily".into(),
                    ..daily()
                },
                ".key",
            ),
            (
                DatasetDeclaration {
                    data_types: vec!["meridian.v1.Order".into()],
                    ..daily()
                },
                "data_types[0]",
            ),
            (
                DatasetDeclaration {
                    data_types: vec![PRICE.into(), "meridian.v1.Bar.vwap".into()],
                    ..daily()
                },
                "does not serve",
            ),
            (
                DatasetDeclaration {
                    modes: vec![],
                    ..daily()
                },
                ".modes",
            ),
            (
                DatasetDeclaration {
                    modes: vec![0],
                    ..daily()
                },
                ".modes[0]",
            ),
            (
                DatasetDeclaration {
                    day_time_zone: "America/New York".into(),
                    ..daily()
                },
                "day_time_zone",
            ),
            (
                DatasetDeclaration {
                    day_end_minute: 1440,
                    ..daily()
                },
                "day_end_minute",
            ),
            (
                DatasetDeclaration {
                    venue_id: "XNYS".into(),
                    ..daily()
                },
                "venue_id",
            ),
            (
                DatasetDeclaration {
                    calendar_id: "NYSE".into(),
                    ..daily()
                },
                "calendar_id",
            ),
        ] {
            let said = refused(wrong, &["data"]).expect(words);
            assert!(said.contains(words), "{words}: {said}");
        }
        let twice = catalogue_refused(
            &Catalogue {
                datasets: vec![daily(), daily()],
            },
            &["data".to_string()],
        )
        .unwrap();
        assert!(twice.contains("twice"), "{twice}");
    }

    #[test]
    fn an_entry_is_a_field_of_a_type_the_dataset_serves() {
        let declared = daily();
        assert!(is_field_of(&declared, "meridian.v1.Bar.vwap"));
        assert!(is_field_of(&declared, "meridian.v1.Price.price"));
        assert!(!is_field_of(&declared, "meridian.v1.Trade.price"));
        let streamed = DatasetDeclaration {
            data_types: vec![TRADE.into(), QUOTE.into()],
            ..daily()
        };
        assert!(is_field_of(&streamed, "meridian.v1.Trade.price"));
        assert!(is_field_of(&streamed, "meridian.v1.Quote.bid"));
        assert_eq!(refused(streamed, &["data"]), None);
        assert_eq!(instance_of("coinbase-1:daily"), Some("coinbase-1"));
        assert_eq!(dataset_id("coinbase-1", "daily"), "coinbase-1:daily");
    }
}

/// A daily value's business date checked on its dataset's calendar as a
/// read of it answered (contract v23, W10.4; the lake plan's vL1c, items 8
/// and 9): None for a business day; else the date, the calendar at its
/// version, and why it is closed -- a holiday listed with its name, a
/// weekend day not listed open, or outside the calendar's coverage, which
/// is never a business day by default. A calendar not held checks nothing.
pub fn closed_day(
    reply: &crate::v1::ReadCalendarReply,
    date: crate::date::Date,
) -> Option<crate::v1::DateCheck> {
    use crate::date::Date;
    use crate::v1::{CalendarDayKind, DateCheck, DateCheckReason};
    use meridian_pb::v1::CalendarRef;
    if !reply.found {
        return None;
    }
    let check = |reason: DateCheckReason, holiday_name: &str| DateCheck {
        date: date.to_string(),
        calendars: vec![CalendarRef {
            calendar_id: reply.calendar_id.clone(),
            version: reply.version,
        }],
        reason: reason as i32,
        holiday_name: holiday_name.to_string(),
    };
    let covered = reply.coverage.as_ref().is_some_and(|coverage| {
        match (
            Date::parse(&coverage.from_date),
            Date::parse(&coverage.to_date),
        ) {
            (Ok(from), Ok(to)) => date >= from && date <= to,
            _ => false,
        }
    });
    if !covered {
        return Some(check(DateCheckReason::OutsideCoverage, ""));
    }
    let text = date.to_string();
    if let Some(day) = reply.days.iter().find(|day| day.date == text) {
        if day.kind == CalendarDayKind::Closed as i32 {
            return Some(check(DateCheckReason::Holiday, &day.name));
        }
        if day.kind == CalendarDayKind::Open as i32 {
            return None;
        }
    }
    let weekend = reply
        .weekends
        .iter()
        .filter_map(|weekend| {
            Date::parse(&weekend.valid_from_date)
                .ok()
                .filter(|from| *from <= date)
                .map(|from| (from, weekend))
        })
        .max_by_key(|(from, _)| *from)
        .map(|(_, weekend)| weekend);
    let Some(weekend) = weekend else {
        // Before any weekend it dates: not known, never a business day.
        return Some(check(DateCheckReason::OutsideCoverage, ""));
    };
    // 1970-01-01 was a Thursday; Weekday counts Monday as 1.
    let weekday = (date.days_since_epoch() + 3).rem_euclid(7) as i32 + 1;
    weekend
        .weekdays
        .contains(&weekday)
        .then(|| check(DateCheckReason::Weekend, ""))
}

#[cfg(test)]
mod closed_day_tests {
    use super::closed_day;
    use crate::date::Date;
    use crate::v1::{
        CalendarCoverage, CalendarDay, CalendarDayKind, CalendarWeekend, DateCheckReason,
        ReadCalendarReply, Weekday,
    };

    fn reply() -> ReadCalendarReply {
        ReadCalendarReply {
            found: true,
            calendar_id: "CAL-USSS".into(),
            version: 4,
            days: vec![
                CalendarDay {
                    date: "2026-10-12".into(),
                    kind: CalendarDayKind::Closed as i32,
                    name: "Columbus Day".into(),
                    source: String::new(),
                },
                CalendarDay {
                    date: "2026-10-17".into(),
                    kind: CalendarDayKind::Open as i32,
                    name: String::new(),
                    source: String::new(),
                },
            ],
            weekends: vec![CalendarWeekend {
                weekdays: vec![Weekday::Saturday as i32, Weekday::Sunday as i32],
                valid_from_date: "2000-01-01".into(),
            }],
            coverage: Some(CalendarCoverage {
                from_date: "2026-01-01".into(),
                to_date: "2027-02-01".into(),
            }),
            sources: vec![],
            trading_sessions: None,
        }
    }

    fn reason(date: &str) -> Option<(i32, String, String)> {
        closed_day(&reply(), Date::parse(date).unwrap()).map(|check| {
            (
                check.reason,
                check.holiday_name,
                format!(
                    "{}@{}",
                    check.calendars[0].calendar_id, check.calendars[0].version
                ),
            )
        })
    }

    #[test]
    fn a_date_is_checked_on_the_calendar_as_read() {
        assert_eq!(reason("2026-10-13"), None, "a Tuesday");
        assert_eq!(
            reason("2026-10-12"),
            Some((
                DateCheckReason::Holiday as i32,
                "Columbus Day".into(),
                "CAL-USSS@4".into()
            ))
        );
        assert_eq!(
            reason("2026-10-18").map(|r| r.0),
            Some(DateCheckReason::Weekend as i32),
            "a Sunday"
        );
        assert_eq!(reason("2026-10-17"), None, "a Saturday listed open");
        assert_eq!(
            reason("2027-02-02").map(|r| r.0),
            Some(DateCheckReason::OutsideCoverage as i32)
        );
        let missing = ReadCalendarReply {
            found: false,
            ..reply()
        };
        assert!(closed_day(&missing, Date::parse("2026-10-18").unwrap()).is_none());
    }
}

/// W10.10 (contract v26): the kinds QueryLake answers, by the name its
/// request spells.
pub const QUERY_KINDS: [&str; 5] = ["prices", "bars", "trades", "quotes", "outcomes"];

/// The lake kinds a role reads, as the reads' rows grant them (W10.10; the
/// lake spec's Q6): prices, bars and outcomes for `reporting`, `portfolio`,
/// `compliance` and `signal`; trades and quotes for `signal` and `ems`;
/// trades for `data`; outcomes for `operations`. `reporting` reads no
/// trades or quotes.
pub fn kinds_read_by(role: &str) -> &'static [&'static str] {
    match role {
        "signal" => &["prices", "bars", "outcomes", "trades", "quotes"],
        "reporting" | "portfolio" | "compliance" => &["prices", "bars", "outcomes"],
        "ems" => &["trades", "quotes"],
        "data" => &["trades"],
        "operations" => &["outcomes"],
        _ => &[],
    }
}

/// Whether any of `roles` reads `kind` (W10.10).
pub fn roles_read_kind(roles: &[String], kind: &str) -> bool {
    roles.iter().any(|role| kinds_read_by(role).contains(&kind))
}

#[cfg(test)]
mod query_kind_tests {
    use super::*;

    #[test]
    fn reporting_reads_no_trades_or_quotes_and_signal_reads_every_kind() {
        let reporting = vec!["reporting".to_string()];
        assert!(roles_read_kind(&reporting, "prices"));
        assert!(!roles_read_kind(&reporting, "trades"));
        assert!(!roles_read_kind(&reporting, "quotes"));
        for kind in QUERY_KINDS {
            assert!(roles_read_kind(&["signal".to_string()], kind), "{kind}");
        }
        assert!(roles_read_kind(
            &["custody".to_string(), "data".to_string()],
            "trades"
        ));
        assert!(!roles_read_kind(&["custody".to_string()], "prices"));
    }
}
