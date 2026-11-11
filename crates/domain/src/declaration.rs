//! A version's declaration (W8.1, W4.8, contract v11): as the SDK prints it
//! and the CLI sends it with an upload, as the catalogue lists it, and as a
//! harness's sidecar reads it from a file to hold registration to it (W4.1,
//! contract v25). The dashboard draws it on the Documentation tab's
//! Reference.
//!
//! What a version still declares beside its roles
//! (spec/vendor-differences-have-a-place-in-the-contract, requirements 16, 19
//! and 30, Q6 and Q15): its secret settings' names, what it receives and does
//! not carry, by name only, and the storage it asks for with its retention
//! (decisions/028). Never a value, an account or an identifier.

use meridian_pb::v1::{
    AlgorithmDeclaration, Catalogue, CopyrightClass, DatasetDeclaration, DerivedUse, GapRecovery,
    NotCarried, NotCarriedReason, ObservationMode, PluginDeclaration, PriceSourceClass,
    PricingModelDeclaration, RawRecordKind, Redistribution, StorageDeclaration,
};

/// The declaration as the CLI sends it, in JSON.
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
pub struct Declared {
    #[serde(default)]
    pub secret_settings: Vec<String>,
    #[serde(default)]
    pub not_carried: Vec<DeclaredNotCarried>,
    #[serde(default)]
    pub storage: Option<DeclaredStorage>,
    /// A data plugin's catalogue (W8.1, contract v18): absent from a version
    /// declaring none, as the SDK uploads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalogue: Option<DeclaredCatalogue>,
    /// The algorithm identifiers its autonomous work may name (W4.1, W4.9,
    /// contract v22): absent from a version declaring none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub algorithms: Vec<DeclaredAlgorithm>,
    /// An operations plugin's affirmation deadline, minutes after midnight
    /// in the deployment's time zone (W11.34, contract v25): absent or 0
    /// from a version declaring none.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub affirmation_deadline: u32,
    /// From contract v25 (W8.1) what registration sends, as protobuf's JSON
    /// of its messages ([`crate::protojson`]): every setting, the interface
    /// and its tools, and whether it reads external accounts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub settings: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reads_external_accounts: bool,
    /// Each section of the vendor's documentation, by its fixed name, its
    /// digest and its size (contract v25); the texts travel beside it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub documentation: Vec<DeclaredSection>,
    /// A pricing plugin's model ids and versions (W12.3, contract v27,
    /// ruling 324): absent from a version declaring none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pricing_models: Vec<DeclaredPricingModel>,
}

/// One pricing model as an upload's declaration names it (contract v27).
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
pub struct DeclaredPricingModel {
    pub model_id: String,
    pub model_version: i32,
}

/// One section of the vendor's documentation as an upload's declaration
/// names it (W8.1, contract v25).
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
pub struct DeclaredSection {
    pub name: String,
    pub digest: String,
    pub bytes: u64,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// One algorithm identifier as an upload carries it (contract v22).
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
pub struct DeclaredAlgorithm {
    pub algorithm_id: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
pub struct DeclaredCatalogue {
    #[serde(default)]
    pub datasets: Vec<DeclaredDataset>,
}

/// One dataset as an upload carries it: each mode in its word (`pull`,
/// `push`, `stream`), as a reason not carried is.
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
pub struct DeclaredDataset {
    pub key: String,
    #[serde(default)]
    pub vendor: String,
    #[serde(default)]
    pub aggregator: String,
    #[serde(default)]
    pub data_types: Vec<String>,
    #[serde(default)]
    pub modes: Vec<String>,
    #[serde(default)]
    pub cadence: u32,
    #[serde(default)]
    pub history: u32,
    // licence_default, retired at contract v30 (ruling 1032): an upload
    // from an SDK before it still carries it, read and dropped as any key
    // the declaration does not name.
    #[serde(default)]
    pub day_time_zone: String,
    #[serde(default)]
    pub day_end_minute: u32,
    #[serde(default)]
    pub venue_id: String,
    /// The settlement calendar its business dates follow (W10.1, contract
    /// v22): empty names none.
    #[serde(default)]
    pub calendar_id: String,
    /// Its declared delay (W10.1, contract v27): 0 or absent for real time.
    #[serde(default)]
    pub declared_delay_ns: i64,
    /// What class of source its prices come from (W10.1; contract 28,
    /// amended 2026-10-31, ruling 906), by its word (`composite`) or the
    /// enum's name; empty or absent for not stated.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub price_source_class: String,
    /// How a gap in it is recovered (W10.1, W10.14; contract v30): its word
    /// (`backfill`, `stream_only`, `none`) or the enum's name; empty or
    /// absent for none declared, read as `none`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub gap_recovery: String,
}

/// A mode as the wire numbers it, from its word or the enum's name.
fn mode_of(text: &str) -> Option<ObservationMode> {
    ObservationMode::from_str_name(text)
        .or_else(|| {
            ObservationMode::from_str_name(&format!(
                "OBSERVATION_MODE_{}",
                text.to_ascii_uppercase()
            ))
        })
        .filter(|mode| *mode != ObservationMode::Unspecified)
}

/// A mode's word, as an upload carries it and the catalogue lists it.
pub fn mode_word(mode: i32) -> &'static str {
    match ObservationMode::try_from(mode) {
        Ok(ObservationMode::Pull) => "pull",
        Ok(ObservationMode::Push) => "push",
        Ok(ObservationMode::Stream) => "stream",
        _ => "",
    }
}

#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
pub struct DeclaredNotCarried {
    pub role: String,
    pub scheme: String,
    pub name: String,
    /// `no_contract_meaning` or `not_converted`, or the enum's name.
    pub reason: String,
}

#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
pub struct DeclaredStorage {
    pub retention_days: u32,
    /// The kinds of raw record it keeps (W8.1, contract v16), each with its
    /// default window and whether it can be archived; none from a version
    /// built before.
    #[serde(default)]
    pub record_kinds: Vec<DeclaredKind>,
}

#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
pub struct DeclaredKind {
    pub name: String,
    #[serde(default)]
    pub label: String,
    pub window_days: u32,
    #[serde(default)]
    pub archivable: bool,
}

/// A reason as the wire numbers it, or none for one it does not define.
fn reason_of(text: &str) -> Option<NotCarriedReason> {
    NotCarriedReason::from_str_name(text).or_else(|| {
        NotCarriedReason::from_str_name(&format!(
            "NOT_CARRIED_REASON_{}",
            text.to_ascii_uppercase()
        ))
    })
}

/// The JSON as the wire's message, or the refusal naming the field.
pub fn from_json(declared: &Declared) -> Result<PluginDeclaration, String> {
    let mut not_carried = Vec::new();
    for (i, held) in declared.not_carried.iter().enumerate() {
        let reason = reason_of(&held.reason)
            .filter(|reason| *reason != NotCarriedReason::Unspecified)
            .ok_or_else(|| {
                format!(
                    "declaration.not_carried[{i}].reason {:?} is neither no_contract_meaning nor \
                     not_converted",
                    held.reason
                )
            })?;
        not_carried.push(NotCarried {
            role: held.role.clone(),
            scheme: held.scheme.clone(),
            name: held.name.clone(),
            reason: reason as i32,
        });
    }
    Ok(PluginDeclaration {
        secret_settings: declared.secret_settings.clone(),
        not_carried,
        storage: declared.storage.as_ref().map(|storage| StorageDeclaration {
            retention_days: storage.retention_days,
            // The kinds of raw record (W8.1, contract v16), which the
            // conductor holds to the dictionary's bounds when it records the
            // version.
            record_kinds: storage
                .record_kinds
                .iter()
                .map(|kind| RawRecordKind {
                    name: kind.name.clone(),
                    label: kind.label.clone(),
                    window_days: kind.window_days,
                    archivable: kind.archivable,
                })
                .collect(),
        }),
        // A data plugin's catalogue (W8.1, contract v18), which the conductor holds
        // to the dictionary's bounds when it records the version.
        catalogue: match &declared.catalogue {
            None => None,
            Some(catalogue) => Some(catalogue_from_json(catalogue)?),
        },
        algorithms: declared
            .algorithms
            .iter()
            .map(|algorithm| AlgorithmDeclaration {
                algorithm_id: algorithm.algorithm_id.clone(),
                description: algorithm.description.clone(),
            })
            .collect(),
        affirmation_deadline: declared.affirmation_deadline,
        // Contract v25 (W8.1): the whole of what registration sends, and the
        // documentation's sections by digest.
        settings: declared
            .settings
            .iter()
            .enumerate()
            .map(|(n, setting)| {
                crate::protojson::setting_from(setting, &format!("declaration.settings[{n}]"))
            })
            .collect::<Result<_, _>>()?,
        interface: declared
            .interface
            .as_ref()
            .map(|interface| crate::protojson::interface_from(interface, "declaration.interface"))
            .transpose()?,
        tools: declared
            .tools
            .iter()
            .enumerate()
            .map(|(n, tool)| crate::protojson::tool_from(tool, &format!("declaration.tools[{n}]")))
            .collect::<Result<_, _>>()?,
        reads_external_accounts: declared.reads_external_accounts,
        documentation: declared
            .documentation
            .iter()
            .map(|section| meridian_pb::v1::DocumentationSection {
                name: section.name.clone(),
                digest: section.digest.clone(),
                bytes: section.bytes,
            })
            .collect(),
        pricing_models: declared
            .pricing_models
            .iter()
            .map(|model| PricingModelDeclaration {
                model_id: model.model_id.clone(),
                model_version: model.model_version,
            })
            .collect(),
        // Contract v30's most_shards (W8.1): not yet read from an upload;
        // core's stage commits read it (step 3c).
        most_shards: 0,
    })
}

/// A derived-data answer from its word (`not_restricted`) or the enum's
/// name; empty is unspecified, anything else `None`.
pub fn derived_use_of(text: &str) -> Option<DerivedUse> {
    let text = text.trim();
    if text.is_empty() {
        return Some(DerivedUse::Unspecified);
    }
    DerivedUse::from_str_name(text).or_else(|| {
        DerivedUse::from_str_name(&format!("DERIVED_USE_{}", text.to_ascii_uppercase()))
    })
}

/// A redistribution answer from its word (`not_allowed`) or the enum's name;
/// empty is not stated, anything else `None`.
pub fn redistribution_of(text: &str) -> Option<Redistribution> {
    let text = text.trim();
    if text.is_empty() {
        return Some(Redistribution::Unspecified);
    }
    Redistribution::from_str_name(text).or_else(|| {
        Redistribution::from_str_name(&format!("REDISTRIBUTION_{}", text.to_ascii_uppercase()))
    })
}

/// A redistribution answer's word, as an upload carries it; empty for none.
pub fn redistribution_word(answer: i32) -> &'static str {
    match Redistribution::try_from(answer) {
        Ok(Redistribution::NotAllowed) => "not_allowed",
        Ok(Redistribution::LicenceRequired) => "licence_required",
        Ok(Redistribution::AllowedWithAttribution) => "allowed_with_attribution",
        _ => "",
    }
}

/// A copyright class from its word (`public_domain`) or the enum's name;
/// empty is not stated, anything else `None`.
pub fn copyright_class_of(text: &str) -> Option<CopyrightClass> {
    let text = text.trim();
    if text.is_empty() {
        return Some(CopyrightClass::Unspecified);
    }
    CopyrightClass::from_str_name(text).or_else(|| {
        CopyrightClass::from_str_name(&format!("COPYRIGHT_CLASS_{}", text.to_ascii_uppercase()))
    })
}

/// A copyright class's word, as an upload carries it; empty for none.
pub fn copyright_class_word(class: i32) -> &'static str {
    match CopyrightClass::try_from(class) {
        Ok(CopyrightClass::PublicDomain) => "public_domain",
        Ok(CopyrightClass::CopyrightCitationRequired) => "copyright_citation_required",
        Ok(CopyrightClass::CopyrightPreapprovalRequired) => "copyright_preapproval_required",
        _ => "",
    }
}

/// A class of source from its word (`firm_quote`) or the enum's name;
/// empty is not stated, anything else `None` (contract 28, amended
/// 2026-10-31, ruling 906).
pub fn price_source_class_of(text: &str) -> Option<PriceSourceClass> {
    let text = text.trim();
    if text.is_empty() {
        return Some(PriceSourceClass::Unspecified);
    }
    PriceSourceClass::from_str_name(text).or_else(|| {
        PriceSourceClass::from_str_name(&format!(
            "PRICE_SOURCE_CLASS_{}",
            text.to_ascii_uppercase()
        ))
    })
}

/// A class of source's word, as an upload carries it, the catalogue lists
/// it and core's pages and tools say it; empty for not stated.
pub fn price_source_class_word(class: i32) -> &'static str {
    match PriceSourceClass::try_from(class) {
        Ok(PriceSourceClass::Traded) => "traded",
        Ok(PriceSourceClass::FirmQuote) => "firm_quote",
        Ok(PriceSourceClass::Composite) => "composite",
        Ok(PriceSourceClass::Evaluated) => "evaluated",
        _ => "",
    }
}

/// A derived-data answer's word, as an upload carries it.
pub fn derived_use_word(terms: i32) -> &'static str {
    match DerivedUse::try_from(terms) {
        Ok(DerivedUse::NotRestricted) => "not_restricted",
        Ok(DerivedUse::InternalOnly) => "internal_only",
        Ok(DerivedUse::RedistributionNeedsLicence) => "redistribution_needs_licence",
        _ => "",
    }
}

fn catalogue_from_json(catalogue: &DeclaredCatalogue) -> Result<Catalogue, String> {
    let mut datasets = Vec::new();
    for (i, dataset) in catalogue.datasets.iter().enumerate() {
        let mut modes = Vec::new();
        for (j, word) in dataset.modes.iter().enumerate() {
            let mode = mode_of(word).ok_or_else(|| {
                format!(
                    "declaration.catalogue.datasets[{i}].modes[{j}] {word:?} is none of pull, push \
                     and stream"
                )
            })?;
            modes.push(mode as i32);
        }
        let price_source_class =
            price_source_class_of(&dataset.price_source_class).ok_or_else(|| {
                format!(
                    "declaration.catalogue.datasets[{i}].price_source_class {:?} is none of \
                     traded, firm_quote, composite and evaluated",
                    dataset.price_source_class
                )
            })?;
        let gap_recovery = gap_recovery_of(&dataset.gap_recovery).ok_or_else(|| {
            format!(
                "declaration.catalogue.datasets[{i}].gap_recovery {:?} is none of backfill, \
                 stream_only and none",
                dataset.gap_recovery
            )
        })?;
        datasets.push(DatasetDeclaration {
            key: dataset.key.clone(),
            vendor: dataset.vendor.clone(),
            aggregator: dataset.aggregator.clone(),
            data_types: dataset.data_types.clone(),
            modes,
            cadence: dataset.cadence,
            history: dataset.history,
            day_time_zone: dataset.day_time_zone.clone(),
            day_end_minute: dataset.day_end_minute,
            venue_id: dataset.venue_id.clone(),
            calendar_id: dataset.calendar_id.clone(),
            declared_delay_ns: dataset.declared_delay_ns,
            price_source_class: price_source_class as i32,
            // Contract v30's terms and pricing and sessions calendar
            // (W10.1): not yet read from an upload; Track 6's core part
            // reads them (step 3c).
            terms_and_pricing: String::new(),
            sessions_calendar_id: String::new(),
            // How a gap in it is recovered (W10.14; core's stage E).
            gap_recovery: gap_recovery as i32,
        });
    }
    Ok(Catalogue { datasets })
}

/// How a gap is recovered, from its word or the enum's name; unspecified
/// for none said, none for a word that names nothing.
pub fn gap_recovery_of(text: &str) -> Option<GapRecovery> {
    let text = text.trim();
    if text.is_empty() {
        return Some(GapRecovery::Unspecified);
    }
    GapRecovery::from_str_name(text).or_else(|| {
        GapRecovery::from_str_name(&format!("GAP_RECOVERY_{}", text.to_ascii_uppercase()))
    })
}

/// How a gap is recovered, in its word: empty for none declared.
pub fn gap_recovery_word(recovery: i32) -> &'static str {
    match GapRecovery::try_from(recovery) {
        Ok(GapRecovery::Backfill) => "backfill",
        Ok(GapRecovery::StreamOnly) => "stream_only",
        Ok(GapRecovery::None) => "none",
        _ => "",
    }
}

/// A dataset's catalogue entry as JSON, its modes in their words, and how a
/// gap in it is recovered where it declares that (contract v30).
pub fn dataset_json(dataset: &DatasetDeclaration) -> serde_json::Value {
    let mut said = serde_json::json!({
        "key": dataset.key,
        "vendor": dataset.vendor,
        "aggregator": dataset.aggregator,
        "data_types": dataset.data_types,
        "modes": dataset.modes.iter().map(|m| mode_word(*m)).collect::<Vec<_>>(),
        "cadence": dataset.cadence,
        "history": dataset.history,
        "day_time_zone": dataset.day_time_zone,
        "day_end_minute": dataset.day_end_minute,
        "venue_id": dataset.venue_id,
        "calendar_id": dataset.calendar_id,
        "declared_delay_ns": dataset.declared_delay_ns,
        "price_source_class": price_source_class_word(dataset.price_source_class),
    });
    let recovery = gap_recovery_word(dataset.gap_recovery);
    if !recovery.is_empty() {
        said["gap_recovery"] = serde_json::Value::from(recovery);
    }
    said
}

/// The wire's message as JSON, for the catalogue's listing.
pub fn to_json(declaration: &PluginDeclaration) -> serde_json::Value {
    serde_json::json!({
        "secret_settings": declaration.secret_settings,
        "not_carried": declaration.not_carried.iter().map(|held| serde_json::json!({
            "role": held.role,
            "scheme": held.scheme,
            "name": held.name,
            "reason": reason_words(held.reason),
        })).collect::<Vec<_>>(),
        "storage": declaration.storage.as_ref().map(|storage| serde_json::json!({
            "retention_days": storage.retention_days,
            "record_kinds": storage.record_kinds.iter().map(|kind| serde_json::json!({
                "name": kind.name,
                "label": kind.label,
                "window_days": kind.window_days,
                "archivable": kind.archivable,
            })).collect::<Vec<_>>(),
        })),
        "catalogue": declaration.catalogue.as_ref().map(|catalogue| serde_json::json!({
            "datasets": catalogue.datasets.iter().map(dataset_json).collect::<Vec<_>>(),
        })),
        "algorithms": declaration.algorithms.iter().map(|algorithm| serde_json::json!({
            "algorithm_id": algorithm.algorithm_id,
            "description": algorithm.description,
        })).collect::<Vec<_>>(),
        // Contract v25 (W11.34): minutes after midnight, 0 for none.
        "affirmation_deadline": declaration.affirmation_deadline,
        // Contract v25 (W8.1): what registration sends, as protobuf's JSON,
        // and the documentation's sections by digest.
        "settings": declaration.settings.iter().map(crate::protojson::setting_json).collect::<Vec<_>>(),
        "interface": declaration.interface.as_ref().map(crate::protojson::interface_json),
        "tools": declaration.tools.iter().map(crate::protojson::tool_json).collect::<Vec<_>>(),
        "reads_external_accounts": declaration.reads_external_accounts,
        "documentation": declaration.documentation.iter().map(|section| serde_json::json!({
            "name": section.name,
            "digest": section.digest,
            "bytes": section.bytes,
        })).collect::<Vec<_>>(),
        // Contract v27 (ruling 324): a pricing plugin's models.
        "pricing_models": declaration.pricing_models.iter().map(|model| serde_json::json!({
            "model_id": model.model_id,
            "model_version": model.model_version,
        })).collect::<Vec<_>>(),
    })
}

fn reason_words(reason: i32) -> &'static str {
    match NotCarriedReason::try_from(reason) {
        Ok(NotCarriedReason::NoContractMeaning) => "no_contract_meaning",
        Ok(NotCarriedReason::NotConverted) => "not_converted",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cli_s_json_becomes_the_declaration_and_back() {
        let declared: Declared = serde_json::from_value(serde_json::json!({
            "secret_settings": ["consumer_key"],
            "not_carried": [{"role": "custody", "scheme": "snaptrade:position",
                             "name": "open_pnl", "reason": "no_contract_meaning"}],
            "storage": {"retention_days": 2555}
        }))
        .unwrap();
        let declaration = from_json(&declared).unwrap();
        assert_eq!(
            declaration.not_carried[0].reason,
            NotCarriedReason::NoContractMeaning as i32
        );
        assert_eq!(declaration.storage.unwrap().retention_days, 2555);
        assert_eq!(
            to_json(&from_json(&declared).unwrap())["not_carried"][0]["reason"],
            "no_contract_meaning"
        );
        let mut bad = declared.clone();
        bad.not_carried[0].reason = "lost".into();
        assert!(from_json(&bad)
            .unwrap_err()
            .contains("not_carried[0].reason"));
    }

    #[test]
    fn a_catalogue_s_modes_travel_as_words_as_the_sdk_uploads_them() {
        let declared: Declared = serde_json::from_value(serde_json::json!({
            "secret_settings": [],
            "not_carried": [],
            "storage": null,
            "catalogue": {"datasets": [{
                "key": "daily", "vendor": "Coinbase", "aggregator": "",
                "data_types": ["meridian.v1.Price"], "modes": ["pull", "push"],
                "cadence": 86400, "history": 365,
                "licence_default": {"kept": true, "retention_days": 0, "derived_use": true,
                                    "display": true, "default_fields": [], "personal_use": true},
                "day_time_zone": "UTC", "day_end_minute": 0, "venue_id": ""
            }]}
        }))
        .unwrap();
        let declaration = from_json(&declared).unwrap();
        let dataset = &declaration.catalogue.as_ref().unwrap().datasets[0];
        assert_eq!(
            dataset.modes,
            vec![ObservationMode::Pull as i32, ObservationMode::Push as i32]
        );
        // An upload from an SDK before contract v30 still carries
        // licence_default (ruling 1032 retired it): read and dropped.
        let back = to_json(&declaration);
        assert_eq!(back["catalogue"]["datasets"][0]["modes"][1], "push");
        assert!(back["catalogue"]["datasets"][0]
            .get("licence_default")
            .is_none());
        let mut bad = declared.clone();
        bad.catalogue.as_mut().unwrap().datasets[0].modes[0] = "poll".into();
        assert!(from_json(&bad)
            .unwrap_err()
            .contains("catalogue.datasets[0].modes[0]"));
    }

    /// Contract 28, amended 2026-10-31 (ruling 906): a dataset's class of
    /// source travels as the SDK uploads it, so a registration carrying it
    /// matches its upload. Its terms' underlying source went with
    /// licence_default (contract v30, ruling 1032).
    #[test]
    fn a_datasets_class_of_source_and_underlying_source_travel_as_the_sdk_uploads_them() {
        let declared: Declared = serde_json::from_value(serde_json::json!({
            "catalogue": {"datasets": [{
                "key": "crypto", "vendor": "Pyth", "modes": ["stream"],
                "data_types": ["meridian.v1.Price", "meridian.v1.Price.confidence"],
                "price_source_class": "composite",
                "licence_default": {
                    "kept": true, "retention_days": 0, "derived_use": true,
                    "derived_use_terms": "not_restricted", "display": true,
                    "default_fields": [], "personal_use": false,
                    "underlying_source": "Pyth's aggregate of its publishers"
                }
            }]}
        }))
        .unwrap();
        let declaration = from_json(&declared).unwrap();
        let dataset = &declaration.catalogue.as_ref().unwrap().datasets[0];
        assert_eq!(
            dataset.price_source_class,
            PriceSourceClass::Composite as i32
        );
        // Printed back by the same words, so a second read is the same.
        let back = to_json(&declaration);
        let printed = &back["catalogue"]["datasets"][0];
        assert_eq!(printed["price_source_class"], "composite");
        let again: Declared = serde_json::from_value(back).unwrap();
        assert_eq!(from_json(&again).unwrap(), declaration);
        // Each word and the enum's name; absent or empty is not stated.
        for (word, class) in [
            ("traded", PriceSourceClass::Traded),
            ("firm_quote", PriceSourceClass::FirmQuote),
            ("PRICE_SOURCE_CLASS_EVALUATED", PriceSourceClass::Evaluated),
            ("", PriceSourceClass::Unspecified),
        ] {
            let mut said = declared.clone();
            said.catalogue.as_mut().unwrap().datasets[0].price_source_class = word.into();
            assert_eq!(
                from_json(&said).unwrap().catalogue.unwrap().datasets[0].price_source_class,
                class as i32,
                "{word}"
            );
        }
        let mut bad = declared.clone();
        bad.catalogue.as_mut().unwrap().datasets[0].price_source_class = "indicative".into();
        assert!(from_json(&bad)
            .unwrap_err()
            .contains("catalogue.datasets[0].price_source_class"));
    }

    /// Contract v30 (W10.1, W10.14; core's stage E): how a gap in a dataset
    /// is recovered travels as the SDK uploads it, by its word or the enum's
    /// name, and is printed back by its word; none declared is unspecified,
    /// which the lake reads as `none`.
    #[test]
    fn a_datasets_gap_recovery_travels_as_the_sdk_uploads_it() {
        let declared: Declared = serde_json::from_value(serde_json::json!({
            "catalogue": {"datasets": [{
                "key": "live", "vendor": "TrueFX", "modes": ["stream"],
                "data_types": ["meridian.v1.Quote"],
                "gap_recovery": "backfill"
            }]}
        }))
        .unwrap();
        let declaration = from_json(&declared).unwrap();
        let dataset = &declaration.catalogue.as_ref().unwrap().datasets[0];
        assert_eq!(dataset.gap_recovery, GapRecovery::Backfill as i32);
        let back = to_json(&declaration);
        assert_eq!(back["catalogue"]["datasets"][0]["gap_recovery"], "backfill");
        let again: Declared = serde_json::from_value(back).unwrap();
        assert_eq!(from_json(&again).unwrap(), declaration);
        for (word, recovery) in [
            ("stream_only", GapRecovery::StreamOnly),
            ("none", GapRecovery::None),
            ("GAP_RECOVERY_BACKFILL", GapRecovery::Backfill),
            ("", GapRecovery::Unspecified),
        ] {
            let mut said = declared.clone();
            said.catalogue.as_mut().unwrap().datasets[0].gap_recovery = word.into();
            assert_eq!(
                from_json(&said).unwrap().catalogue.unwrap().datasets[0].gap_recovery,
                recovery as i32,
                "{word}"
            );
        }
        // None declared prints nothing, as an SDK before v30 uploads it.
        let mut none = declared.clone();
        none.catalogue.as_mut().unwrap().datasets[0].gap_recovery = String::new();
        let printed = to_json(&from_json(&none).unwrap());
        assert!(printed["catalogue"]["datasets"][0]
            .get("gap_recovery")
            .is_none());
        let mut bad = declared.clone();
        bad.catalogue.as_mut().unwrap().datasets[0].gap_recovery = "sometimes".into();
        assert!(from_json(&bad)
            .unwrap_err()
            .contains("catalogue.datasets[0].gap_recovery"));
    }
}
