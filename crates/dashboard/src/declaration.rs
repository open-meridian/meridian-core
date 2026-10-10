//! A version's declaration (W8.1, W4.8, contract v11): as the CLI sends it
//! with an upload, as the catalogue lists it, and as a plugin's Summary
//! draws it beside the names not carried it saw.
//!
//! What a version still declares beside its roles
//! (spec/vendor-differences-have-a-place-in-the-contract, requirements 16, 19
//! and 30, Q6 and Q15): its secret settings' names, what it receives and does
//! not carry, by name only, and the storage it asks for with its retention
//! (decisions/028). Never a value, an account or an identifier.

use meridian_pb::v1::{
    NotCarried, NotCarriedReason, NotCarriedSeen, PluginDeclaration, RawRecordKind,
    StorageDeclaration,
};

use crate::html::escape;

/// The declaration as the CLI sends it, in JSON.
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
pub struct Declared {
    #[serde(default)]
    pub secret_settings: Vec<String>,
    #[serde(default)]
    pub not_carried: Vec<DeclaredNotCarried>,
    #[serde(default)]
    pub storage: Option<DeclaredStorage>,
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
        // A dgm's catalogue (W8.1, contract v18): read from the version's
        // metadata once the lake is built (row 4).
        catalogue: None,
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
    })
}

fn reason_words(reason: i32) -> &'static str {
    match NotCarriedReason::try_from(reason) {
        Ok(NotCarriedReason::NoContractMeaning) => "no_contract_meaning",
        Ok(NotCarriedReason::NotConverted) => "not_converted",
        _ => "",
    }
}

/// The declaration on a plugin's Summary, with how often each name not
/// carried was seen: a section of its own, or nothing from a plugin before
/// v11.
pub fn summary(declaration: Option<&PluginDeclaration>, seen: &[NotCarriedSeen]) -> String {
    let Some(declaration) = declaration else {
        return String::new();
    };
    let secrets = if declaration.secret_settings.is_empty() {
        "none".to_string()
    } else {
        declaration
            .secret_settings
            .iter()
            .map(|name| escape(name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let storage = match &declaration.storage {
        None => "none".to_string(),
        Some(storage) if storage.record_kinds.is_empty() => format!(
            "its own, for its raw records, kept {} days",
            storage.retention_days
        ),
        // Each kind's window and what is kept of it are drawn in the
        // records panel (contract v16).
        Some(storage) => format!(
            "its own, for {} kinds of raw record, each kept for its window",
            storage.record_kinds.len()
        ),
    };
    let rows: String = declaration
        .not_carried
        .iter()
        .map(|held| {
            let count = seen
                .iter()
                .find(|seen| seen.scheme == held.scheme && seen.name == held.name)
                .map(|seen| seen.count)
                .unwrap_or(0);
            let why = match NotCarriedReason::try_from(held.reason) {
                Ok(NotCarriedReason::NoContractMeaning) => "no meaning in the contract yet",
                Ok(NotCarriedReason::NotConverted) => "not yet converted",
                _ => "",
            };
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{why}</td><td>{count}</td></tr>",
                escape(&held.role),
                escape(&held.scheme),
                escape(&held.name),
            )
        })
        .collect();
    let table = if rows.is_empty() {
        "<p class=\"hint\">It declares nothing it receives and does not carry.</p>".to_string()
    } else {
        format!(
            "<table class=\"list one-line\"><thead><tr><th>Role</th><th>Scheme</th><th>Name</th>\
             <th>Why</th><th>Seen</th></tr></thead><tbody>{rows}</tbody></table>"
        )
    };
    format!(
        "<section class=\"admin-section\"><div class=\"section-head\"><div><h2>What it declares</h2>\
         <p class=\"hint\">Its secret settings: {secrets}. Its storage: {storage}. What it receives \
         and does not carry, by name only, with how often it saw each since it started; the \
         counts stay in this deployment.</p></div></div>{table}</section>"
    )
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
    fn the_summary_draws_names_and_counts_never_a_value() {
        let declaration = PluginDeclaration {
            secret_settings: vec!["consumer_key".into()],
            not_carried: vec![NotCarried {
                role: "custody".into(),
                scheme: "snaptrade:position".into(),
                name: "open_pnl".into(),
                reason: NotCarriedReason::NoContractMeaning as i32,
            }],
            storage: Some(StorageDeclaration {
                retention_days: 30,
                ..Default::default()
            }),
            catalogue: None,
        };
        let seen = [NotCarriedSeen {
            scheme: "snaptrade:position".into(),
            name: "open_pnl".into(),
            count: 42,
        }];
        let page = summary(Some(&declaration), &seen);
        assert!(
            page.contains("consumer_key") && page.contains("open_pnl") && page.contains(">42<")
        );
        assert!(page.contains("kept 30 days"));
        assert_eq!(summary(None, &seen), "");
    }
}
