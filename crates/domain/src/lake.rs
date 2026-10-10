//! The lake's vocabulary, read alike by the sidecar, the conductor, the lake
//! and the dashboard (contract v18, W10; spec/the-lake).
//!
//! The data types of the lake's 1a and the dictionary entries of their
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
use meridian_pb::v1::{Catalogue, DatasetDeclaration, DatasetLicence, ObservationMode};

/// A price of one kind for one subject.
pub const PRICE: &str = "meridian.v1.Price";
/// Open, high, low and close over an interval.
pub const BAR: &str = "meridian.v1.Bar";

/// The lake's data types in the 1a.
pub const DATA_TYPES: [&str; 2] = [PRICE, BAR];

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

/// The dictionary entries of a data type's fields; none for one the lake
/// does not have.
pub fn fields_of(data_type: &str) -> &'static [&'static str] {
    match data_type {
        PRICE => &PRICE_FIELDS,
        BAR => &BAR_FIELDS,
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
        .unwrap_or(DatasetLicence {
            dataset: dataset.to_string(),
            kept: true,
            derived_use: true,
            display: true,
            ..Default::default()
        })
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

/// A catalogue held to W4.1's rules: refused on a version not holding `dgm`,
/// a key repeated or not of its form, a data type the lake does not have, a
/// field not an entry of its data type, an invalid time zone, or a value past
/// the dictionary's bounds; each naming where.
pub fn catalogue_refused(catalogue: &Catalogue, roles: &[String]) -> Option<String> {
    if catalogue.datasets.is_empty() {
        return None;
    }
    if !roles.iter().any(|role| role == "dgm") {
        return Some(format!(
            "declaration.catalogue declares datasets on a version holding {}, not dgm: the \
             catalogue is a dgm's",
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
    fn a_catalogue_is_a_dgms_and_held_to_its_rules() {
        assert_eq!(refused(daily(), &["dgm"]), None);
        assert!(refused(daily(), &["reporting"])
            .unwrap()
            .contains("not dgm"));
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
                    data_types: vec!["meridian.v1.Trade".into()],
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
        ] {
            let said = refused(wrong, &["dgm"]).expect(words);
            assert!(said.contains(words), "{words}: {said}");
        }
        let twice = catalogue_refused(
            &Catalogue {
                datasets: vec![daily(), daily()],
            },
            &["dgm".to_string()],
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
        assert_eq!(instance_of("coinbase-1:daily"), Some("coinbase-1"));
        assert_eq!(dataset_id("coinbase-1", "daily"), "coinbase-1:daily");
    }
}
