//! The parity check for the Data sources page (W6.20, contract v18; the
//! plan's Q29): every tab it draws and every route under
//! `/admin/data-sources` names one of its five tools, and each tool reaches
//! something the page draws. A tab or an action drawn later with none fails
//! the build.

use super::{PARITY, SPECS};
use crate::mcp::Area;

fn reached(key: &str) -> Option<&'static str> {
    PARITY
        .iter()
        .find_map(|(drawn, by)| (*drawn == key).then_some(*by))
}

fn names_tools(by: &str) -> bool {
    let tools: Vec<&str> = SPECS.iter().map(|spec| spec.name).collect();
    let named: Vec<&str> = by.split(' ').filter(|w| !w.is_empty()).collect();
    !named.is_empty() && named.iter().all(|name| tools.contains(name))
}

/// The paths a source file routes, as `.route("...")` gives each.
fn routes(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = source;
    while let Some(at) = rest.find(".route(") {
        rest = &rest[at + ".route(".len()..];
        if let Some(quoted) = rest.trim_start().strip_prefix('"') {
            if let Some(end) = quoted.find('"') {
                found.push(quoted[..end].to_string());
            }
        }
    }
    found
}

#[test]
fn every_tab_the_page_draws_names_its_tool() {
    for (id, _) in crate::admin::data_sources::TABS {
        let key = format!("tab {id}");
        let by = reached(&key).unwrap_or_else(|| panic!("{key} is drawn and names no tool"));
        assert!(names_tools(by), "{key}: {by:?}");
    }
}

#[test]
fn every_route_of_the_page_names_its_tool() {
    let mut checked = 0;
    for path in routes(include_str!("../../admin.rs")) {
        if !path.starts_with("/admin/data-sources") {
            continue;
        }
        checked += 1;
        let by = reached(&path).unwrap_or_else(|| panic!("{path} is routed and names no tool"));
        assert!(names_tools(by), "{path}: {by:?}");
    }
    assert_eq!(checked, 4, "the page's routes were not found");
}

#[test]
fn every_tool_parity_names_is_one_and_every_tool_is_named() {
    for (key, by) in PARITY {
        assert!(names_tools(by), "{key}: {by:?}");
    }
    for spec in SPECS {
        assert!(
            PARITY
                .iter()
                .any(|(_, by)| by.split(' ').any(|word| word == spec.name)),
            "{} reaches nothing PARITY names",
            spec.name
        );
        assert_eq!(spec.area, Area::DataSources, "{}", spec.name);
        assert!(
            spec.reads == spec.name.starts_with("list_"),
            "{} reads only when it lists",
            spec.name
        );
    }
    assert_eq!(SPECS.len(), 5, "the five approved tools");
}

#[test]
fn every_change_requires_a_note_and_a_priority_the_stale_guard() {
    for spec in SPECS.iter().filter(|spec| !spec.reads) {
        let schema = (spec.input_schema)();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(required.contains(&"note"), "{}", spec.name);
        if spec.name == "set_source_priority" {
            assert!(required.contains(&"against_updated_at_ns"));
        }
    }
}
