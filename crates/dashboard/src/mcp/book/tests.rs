//! The parity check for the order path's core tools (W6.20, contract v22;
//! the refresh's Q44): each route the Accounts tab's book part and the
//! trading instructions' page change names one of the four tools, and each
//! tool reaches something drawn.

use super::{PARITY, SPECS};
use crate::mcp::Area;

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
fn every_route_of_the_book_and_the_instructions_names_its_tool() {
    let mut checked = 0;
    for path in routes(include_str!("../../admin.rs")) {
        if path != "/admin/accounts/book" && !path.starts_with("/admin/trading-instructions") {
            continue;
        }
        checked += 1;
        let by = PARITY
            .iter()
            .find_map(|(drawn, by)| (*drawn == path).then_some(*by))
            .unwrap_or_else(|| panic!("{path} is routed and names no tool"));
        assert!(names_tools(by), "{path}: {by:?}");
    }
    assert_eq!(checked, 2, "the routes were not found");
}

#[test]
fn the_four_tools_are_named_preparable_where_they_change_and_may_not_for_agents() {
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
        assert!(matches!(spec.area, Area::Accounts | Area::Trading));
        assert_eq!(spec.reads, spec.name.starts_with("list_"), "{}", spec.name);
        let name = format!("dashboard__{}", spec.name);
        assert_eq!(
            crate::prepared::CORE_PREPARABLE.contains(&name.as_str()),
            !spec.reads,
            "{name}"
        );
        if !spec.reads {
            assert_eq!(
                crate::prepared::levels::default_for(&name),
                crate::prepared::levels::Level::MayNot,
                "{name} is at may not for agents until loosened (Q44)"
            );
        }
    }
    // The four approved in v22, and from contract v24 the escape hatch's
    // setting (W11.36, approved 2026-10-14) and closing an account (W6.3,
    // approved 2026-10-15).
    assert_eq!(SPECS.len(), 6, "the six approved tools");
}

#[test]
fn a_deployment_admin_reaches_all_four_and_a_plugins_admin_the_instructions_alone() {
    let admin = meridian_access::Access {
        deployment_admin: true,
        ..Default::default()
    };
    let plugin_admin = meridian_access::Access {
        all_plugins_admin: true,
        ..Default::default()
    };
    for spec in SPECS {
        assert!(spec.area.open_to(&admin), "{}", spec.name);
        assert_eq!(
            spec.area.open_to(&plugin_admin),
            spec.area == Area::Trading,
            "{}",
            spec.name
        );
        assert!(!spec.area.open_to(&meridian_access::Access::default()));
    }
}

#[test]
fn a_change_takes_a_note_and_a_read_answers_unchanged() {
    // The core-tools page's two rules (the product owner, 2026-10-12): a
    // change's why is `note`, kept as the book entry's reason; a read's
    // outcome is "unchanged", as every other core read's.
    for spec in SPECS {
        let schema = (spec.input_schema)();
        let properties = schema["properties"].as_object().unwrap();
        assert!(!properties.contains_key("reason"), "{}", spec.name);
        if spec.reads {
            assert!(properties.is_empty(), "{}", spec.name);
        } else {
            assert!(properties.contains_key("note"), "{}", spec.name);
            assert!(
                schema["required"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|name| name == "note"),
                "{}",
                spec.name
            );
        }
    }
    let source = include_str!("../book.rs");
    assert!(!source.contains("\"outcome\": \"read\""));
    assert_eq!(source.matches("\"outcome\": \"unchanged\"").count(), 2);
}

/// Contract v24 (W6.3, W6.30): "Unlink all and close" through the surface is
/// refused while a grant stands, since no tool sets a grant; nothing is sent.
#[test]
fn close_account_takes_a_note_and_asks_nothing_of_grants() {
    let spec = SPECS
        .iter()
        .find(|spec| spec.name == "close_account")
        .expect("close_account is listed");
    let schema = (spec.input_schema)();
    let required: Vec<&str> = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(required, vec!["account_id", "note"]);
    assert!(spec.description.contains("no tool sets a grant"));
}
