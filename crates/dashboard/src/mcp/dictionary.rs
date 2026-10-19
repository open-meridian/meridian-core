//! The data dictionary, as `dashboard__read_dictionary_entry` answers it
//! (contract v26, W6.20; sdk-contract/the-data-dictionary-is-an-mcp-tool):
//! one entry by its name, as the published boundaries' `fields.json` holds
//! it at this release. `fields.json` beside this file is that copy, written
//! by `make codegen` from meridian-schema at the revision the workspace
//! links (never edited by hand; `make check-codegen` compares it).

use std::collections::HashMap;
use std::sync::OnceLock;

use serde_json::Value;

const FIELDS_JSON: &str = include_str!("fields.json");

fn entries() -> &'static HashMap<String, Value> {
    static HELD: OnceLock<HashMap<String, Value>> = OnceLock::new();
    HELD.get_or_init(|| {
        let whole: Value = serde_json::from_str(FIELDS_JSON).unwrap_or(Value::Null);
        whole["entries"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| {
                        entry["name"]
                            .as_str()
                            .map(|name| (name.to_string(), entry.clone()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// The entry named, as fields.json holds it; `None` for a name it has not.
pub fn entry(name: &str) -> Option<Value> {
    entries().get(name).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_is_answered_as_the_boundaries_hold_it() {
        let held = entry("meridian.v1.RunJobRequest.job").expect("a v26 entry");
        assert_eq!(held["since"], "v26");
        assert!(held["meaning"].as_str().is_some_and(|m| !m.is_empty()));
        assert!(entries().len() > 100, "the whole dictionary is carried");
        assert!(entry("meridian.v1.Nothing.here").is_none());
    }
}
