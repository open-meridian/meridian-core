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
    AlgorithmDeclaration, Catalogue, DatasetDeclaration, DatasetLicence, DerivedUse, NotCarried,
    NotCarriedReason, ObservationMode, PluginDeclaration, PricingModelDeclaration, RawRecordKind,
    StorageDeclaration,
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
    #[serde(default)]
    pub licence_default: Option<DeclaredLicence>,
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
}

#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
pub struct DeclaredLicence {
    #[serde(default)]
    pub kept: bool,
    #[serde(default)]
    pub retention_days: u32,
    #[serde(default)]
    pub derived_use: bool,
    /// The terms' derived-data answer (contract v27, ruling 479), by its
    /// word (`internal_only`) or the enum's name; empty reads `derived_use`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub derived_use_terms: String,
    #[serde(default)]
    pub display: bool,
    #[serde(default)]
    pub default_fields: Vec<String>,
    #[serde(default)]
    pub personal_use: bool,
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
        let licence_default = match &dataset.licence_default {
            None => None,
            Some(l) => {
                let terms = derived_use_of(&l.derived_use_terms).ok_or_else(|| {
                    format!(
                        "declaration.catalogue.datasets[{i}].licence_default.derived_use_terms \
                         {:?} is none of not_restricted, internal_only and \
                         redistribution_needs_licence",
                        l.derived_use_terms
                    )
                })?;
                #[allow(deprecated)]
                let mut licence = DatasetLicence {
                    kept: l.kept,
                    retention_days: l.retention_days,
                    derived_use: l.derived_use,
                    derived_use_terms: terms as i32,
                    display: l.display,
                    default_fields: l.default_fields.clone(),
                    personal_use: l.personal_use,
                    ..Default::default()
                };
                crate::lake::derived_use_both(&mut licence);
                Some(licence)
            }
        };
        datasets.push(DatasetDeclaration {
            key: dataset.key.clone(),
            vendor: dataset.vendor.clone(),
            aggregator: dataset.aggregator.clone(),
            data_types: dataset.data_types.clone(),
            modes,
            cadence: dataset.cadence,
            history: dataset.history,
            licence_default,
            day_time_zone: dataset.day_time_zone.clone(),
            day_end_minute: dataset.day_end_minute,
            venue_id: dataset.venue_id.clone(),
            calendar_id: dataset.calendar_id.clone(),
            declared_delay_ns: dataset.declared_delay_ns,
        });
    }
    Ok(Catalogue { datasets })
}

/// A dataset's catalogue entry as JSON, its modes in their words.
pub fn dataset_json(dataset: &DatasetDeclaration) -> serde_json::Value {
    serde_json::json!({
        "key": dataset.key,
        "vendor": dataset.vendor,
        "aggregator": dataset.aggregator,
        "data_types": dataset.data_types,
        "modes": dataset.modes.iter().map(|m| mode_word(*m)).collect::<Vec<_>>(),
        "cadence": dataset.cadence,
        "history": dataset.history,
        "licence_default": dataset.licence_default.as_ref().map(|l| serde_json::json!({
            "kept": l.kept,
            "retention_days": l.retention_days,
            "derived_use": crate::lake::derived_use_bool(l),
            "derived_use_terms": derived_use_word(crate::lake::derived_use_terms(l) as i32),
            "display": l.display,
            "default_fields": l.default_fields,
            "personal_use": l.personal_use,
        })),
        "day_time_zone": dataset.day_time_zone,
        "day_end_minute": dataset.day_end_minute,
        "venue_id": dataset.venue_id,
        "calendar_id": dataset.calendar_id,
        "declared_delay_ns": dataset.declared_delay_ns,
    })
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
        assert!(dataset.licence_default.as_ref().unwrap().personal_use);
        let back = to_json(&declaration);
        assert_eq!(back["catalogue"]["datasets"][0]["modes"][1], "push");
        let mut bad = declared.clone();
        bad.catalogue.as_mut().unwrap().datasets[0].modes[0] = "poll".into();
        assert!(from_json(&bad)
            .unwrap_err()
            .contains("catalogue.datasets[0].modes[0]"));
    }
}
