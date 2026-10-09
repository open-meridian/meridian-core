//! The parity check (W6.9, W6.20, contract v17; the plan's "The parity
//! check, core's own"): every tab core draws for a plugin at `admin`, every
//! part of its Summary, every tab of the admin portal's view of it, and every
//! route under `/plugins/{instance}`, `/admin/plugins/{instance}`,
//! `/admin/holds` and `/terminal/plugins` names a plugin-area tool reaching
//! it, or its ruled exception. A tab or an action drawn later with neither
//! fails the build: restarting and moving a version, when they are built,
//! come with theirs. And no `/mcp` code can send what changes who holds
//! access.

use meridian_access::AccessLevel;
use meridian_domain::v1::{PluginReport, PluginSettingsRecord};
use meridian_pb::v1::{SettingDeclaration, SettingType};

use super::{PARITY, SPECS};

/// What PARITY says of `key`, as a tool's name or an exception; a key
/// ending `-*` stands for any of its kind.
fn reached(key: &str) -> Option<&'static str> {
    PARITY.iter().find_map(|(drawn, by)| {
        let matches = match drawn.strip_suffix('*') {
            Some(prefix) => key.starts_with(prefix),
            None => *drawn == key,
        };
        matches.then_some(*by)
    })
}

/// Each name PARITY gives is a tool of this module, or the entry says why
/// none: an exception.
fn names_a_tool_or_an_exception(by: &str) -> bool {
    let tools: Vec<&str> = SPECS.iter().map(|spec| spec.name).collect();
    let (named, exception) = match by.split_once("exception:") {
        Some((named, _)) => (named, true),
        None => (by, false),
    };
    let named: Vec<&str> = named
        .split([' ', ';'])
        .filter(|word| !word.is_empty())
        .collect();
    named.iter().all(|name| tools.contains(name)) && (exception || !named.is_empty())
}

/// A plugin with a table setting, at the edge, as the area draws it.
fn drawn() -> (PluginReport, PluginSettingsRecord) {
    let table = SettingDeclaration {
        name: "plan_code_links".into(),
        r#type: SettingType::Table as i32,
        label: "Plan-code links".into(),
        ..Default::default()
    };
    let record = PluginSettingsRecord {
        plugin_instance_id: "ops-1".into(),
        declared_settings: vec![table],
        ..Default::default()
    };
    let report = PluginReport {
        plugin_instance_id: "ops-1".into(),
        registered: true,
        ..Default::default()
    };
    (report, record)
}

#[test]
fn every_tab_and_part_core_draws_at_admin_names_its_tool_or_its_exception() {
    let (report, record) = drawn();
    let tables: Vec<(String, String)> = crate::admin::settings::tables(&record, false)
        .into_iter()
        .map(|t| (crate::admin::settings::table_key(t), t.label.clone()))
        .collect();
    assert!(!tables.is_empty(), "the check draws a table setting's tab");
    let mut drawn: Vec<String> = crate::area::tabs(Some(&report), AccessLevel::Admin, &tables)
        .into_iter()
        .filter(|tab| tab.drawn)
        .map(|tab| format!("tab {}", tab.key))
        .collect();
    drawn.extend(
        crate::admin::SUMMARY_PART_KEYS
            .iter()
            .map(|(key, _)| format!("part {key}")),
    );
    drawn.extend(
        crate::admin::view::tabs(true, Some(&record), false)
            .into_iter()
            .map(|tab| format!("view {}", tab.key)),
    );
    for key in &drawn {
        let by = reached(key).unwrap_or_else(|| {
            panic!(
                "{key} is drawn for a plugin and names no tool, nor a ruled exception, in PARITY"
            )
        });
        assert!(names_a_tool_or_an_exception(by), "{key}: {by:?}");
    }
}

/// The paths a source file routes, as `.route("...")` gives each.
fn routes(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = source;
    while let Some(at) = rest.find(".route(") {
        rest = &rest[at + ".route(".len()..];
        let trimmed = rest.trim_start();
        if let Some(quoted) = trimmed.strip_prefix('"') {
            if let Some(end) = quoted.find('"') {
                found.push(quoted[..end].to_string());
            }
        }
    }
    found
}

#[test]
fn every_route_of_a_plugins_area_its_holds_and_its_lifecycle_names_its_tool_or_its_exception() {
    let sources = [
        include_str!("../../web.rs"),
        include_str!("../../admin.rs"),
        include_str!("../../catalogue.rs"),
        include_str!("../../archive.rs"),
        include_str!("../../area.rs"),
        include_str!("../../plugins.rs"),
    ];
    let mut checked = 0;
    for path in sources.iter().flat_map(|source| routes(source)) {
        let ours = path.starts_with("/plugins/{instance}")
            || path.starts_with("/admin/plugins/")
            || path == "/admin/holds"
            || path.starts_with("/terminal/plugins");
        if !ours {
            continue;
        }
        checked += 1;
        let by = reached(&path).unwrap_or_else(|| {
            panic!("{path} is routed and names no tool, nor a ruled exception, in PARITY")
        });
        assert!(names_a_tool_or_an_exception(by), "{path}: {by:?}");
    }
    assert!(checked >= 10, "the routes were not found: {checked}");
}

#[test]
fn every_tool_parity_names_is_one_and_every_tool_is_named() {
    for (key, by) in PARITY {
        assert!(names_a_tool_or_an_exception(by), "{key}: {by:?}");
    }
    for spec in SPECS {
        assert!(
            PARITY
                .iter()
                .any(|(_, by)| by.split([' ', ';']).any(|word| word == spec.name)),
            "{} reaches nothing PARITY names",
            spec.name
        );
    }
    assert_eq!(SPECS.len(), 13, "the thirteen approved tools");
}

/// What changes who holds access: no tool's (the ruling of 2026-10-01).
const ACCESS_CHANGES: [&str; 10] = [
    "define-user-group",
    "define-account-group",
    "define-access-group",
    "grant-permission",
    "withdraw-permission",
    "DefineUserGroupRequest",
    "DefineAccountGroupRequest",
    "DefineAccessGroupRequest",
    "GrantPermissionRequest",
    "WithdrawPermissionRequest",
];

#[test]
fn no_tool_in_any_catalogue_sends_what_changes_who_holds_access() {
    // Every module /mcp calls through: core's tools, and the transport to a
    // plugin's.
    for (file, source) in [
        ("mcp.rs", include_str!("../../mcp.rs")),
        ("mcp/plugin_area.rs", include_str!("../plugin_area.rs")),
        ("mcp/instruments.rs", include_str!("../instruments.rs")),
        ("mcp/tickets.rs", include_str!("../tickets.rs")),
        ("mcp/bounds.rs", include_str!("../bounds.rs")),
    ] {
        for named in ACCESS_CHANGES {
            assert!(!source.contains(named), "{file} names {named}");
        }
    }
    for spec in super::super::instruments::SPECS
        .iter()
        .chain(super::super::tickets::SPECS)
        .chain(SPECS)
    {
        for word in [
            "grant",
            "withdraw_permission",
            "access_group",
            "user_group",
            "permission",
        ] {
            assert!(!spec.name.contains(word), "{} names {word}", spec.name);
        }
    }
}

#[test]
fn a_secret_is_refused_by_name_and_the_instructions_say_both_exceptions() {
    let instructions = crate::mcp::INSTRUCTIONS;
    assert!(instructions.contains("no tool reads or takes a secret setting's value"));
    assert!(instructions.contains("no tool changes who holds access"));
    for spec in SPECS {
        let said = super::described(spec);
        assert!(
            said.contains("never") || said.contains("No tool"),
            "{}",
            spec.name
        );
    }
}
