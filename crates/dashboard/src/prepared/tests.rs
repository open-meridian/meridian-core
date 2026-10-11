//! The prepared change's own rules, without a deployment.

use serde_json::json;

use super::*;

#[test]
fn a_prepared_calls_id_is_pc_and_a_ulid() {
    assert!(is_prepared_call_id("PC-01JAC2Q4M7K00000000000001"));
    for not in [
        "",
        "PC-",
        "TK-01JAC2Q4M7K00000000000001",
        "PC-01jac2q4m7k00000000000001",
    ] {
        assert!(!is_prepared_call_id(not), "{not:?}");
    }
    let prepared = Prepared::default();
    let one = prepared.ulid(1_791_504_000_000_000_000);
    let two = prepared.ulid(1_791_504_000_000_000_000);
    assert!(one < two, "minted in order within a millisecond");
    assert!(is_prepared_call_id(&format!("PC-{one}")));
}

#[test]
fn the_accounts_an_input_names_are_read_at_their_paths() {
    let arguments = json!({
        "account_id": "ACC-1",
        "positions": [{"account_id": "ACC-2"}, {"account_id": "ACC-1"}, {"other": 1}],
    });
    assert_eq!(
        values_at(
            &arguments,
            &["account_id".into(), "positions[].account_id".into()]
        ),
        vec!["ACC-1".to_string(), "ACC-2".to_string()]
    );
    assert!(values_at(&arguments, &["missing".into()]).is_empty());
}

#[test]
fn a_made_record_is_the_prepared_one_when_every_field_it_names_says_the_same() {
    let prepared = json!({"days": 30, "role": "custody", "positions": [{"q": "1.5"}]});
    assert!(same_record(
        &prepared,
        &json!({"days": "30", "role": "custody", "positions": [{"q": "1.5"}], "csrf": "x"})
    ));
    assert!(!same_record(
        &prepared,
        &json!({"days": 31, "role": "custody", "positions": [{"q": "1.5"}]})
    ));
    assert!(!same_record(
        &prepared,
        &json!({"days": 30, "role": "custody", "positions": []})
    ));
}

#[test]
fn a_store_settles_once_and_keeps_each_step() {
    let store = store::InMemory::default();
    let kept = Kept {
        prepared_call_id: "PC-01JAC2Q4M7K000000000000001".into(),
        tool_name: "dashboard__set_hold".into(),
        state: PreparedCallState::Prepared as i32,
        prepared_at_ns: 1,
        ..Kept::default()
    };
    let step = |kind| Step {
        prepared_call_id: kept.prepared_call_id.clone(),
        kind,
        by_subject: "ada".into(),
        through_delegation: String::new(),
        client_name: String::new(),
        at_ns: 2,
        note: String::new(),
    };
    store.insert(&kept, &step(StepKind::Prepared), &[]).unwrap();
    assert!(store
        .transition(
            &kept.prepared_call_id,
            &store::PENDING,
            PreparedCallState::Made as i32,
            &step(StepKind::Made),
            &[]
        )
        .unwrap());
    // The first who settles it settles it for all.
    assert!(!store
        .transition(
            &kept.prepared_call_id,
            &store::PENDING,
            PreparedCallState::Discarded as i32,
            &step(StepKind::Discarded),
            &[]
        )
        .unwrap());
    let steps = store.steps(&kept.prepared_call_id).unwrap();
    assert_eq!(
        steps.iter().map(|s| s.kind).collect::<Vec<_>>(),
        vec![StepKind::Prepared, StepKind::Made]
    );
    assert_eq!(
        store
            .get(&kept.prepared_call_id)
            .unwrap()
            .unwrap()
            .settled_at_ns,
        Some(2)
    );
    assert!(store.pending().unwrap().is_empty());
    // Swept 90 days after it settles.
    assert_eq!(store.sweep(3).unwrap(), 1);
    assert!(store.get(&kept.prepared_call_id).unwrap().is_none());
}

/// The prepared change's pages and the Agents tab, each against the tool
/// that reaches it or its ruled exception (the plan's "parity check", grown
/// at v20): nothing lands page-only.
pub const PARITY: &[(&str, &str)] = &[
    ("/inbox/prepared", "read_inbox list_prepared read_prepared"),
    ("/prepared/{id}", "read_prepared"),
    ("/prepared/{id}/make", "make_prepared"),
    ("/prepared/{id}/discard", "discard_prepared"),
    ("/prepared/{id}/share", "share_prepared"),
    ("/prepared/{id}/withdraw", "withdraw_prepared"),
    (
        "/admin/agents",
        "list_prepared read_plugin_access; exception: a level and a gate are set at the page only (ruled 2026-10-10)",
    ),
    (
        "/admin/agents/lapse",
        "list_prepared; exception: the lapse period is set at the page only (the plan's Q18)",
    ),
];

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
fn every_prepared_page_and_the_agents_tab_names_its_tool_or_its_exception() {
    let tools: Vec<&str> = crate::mcp::prepared::SPECS
        .iter()
        .chain(crate::mcp::plugin_area::SPECS)
        .chain(crate::mcp::tickets::SPECS)
        .map(|spec| spec.name)
        .collect();
    let mut drawn = routes(include_str!("pages.rs"));
    drawn.extend(
        routes(include_str!("../admin.rs"))
            .into_iter()
            .filter(|path| path.starts_with("/admin/agents")),
    );
    assert!(drawn.len() >= 8, "{drawn:?}");
    for path in drawn {
        let (_, by) = PARITY
            .iter()
            .find(|(key, _)| *key == path)
            .unwrap_or_else(|| panic!("{path} is drawn and names no tool, nor a ruled exception"));
        let named = by.split([' ', ';']).any(|word| tools.contains(&word));
        assert!(named, "{path}: {by}");
    }
    for spec in crate::mcp::prepared::SPECS {
        assert!(
            PARITY
                .iter()
                .any(|(_, by)| by.split([' ', ';']).any(|w| w == spec.name))
                || spec.name == "prepare_call",
            "{} reaches no page PARITY names",
            spec.name
        );
    }
}
