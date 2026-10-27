//! The data terms' tools: each names something the Use tab or the test
//! draws, and everything they draw names a tool (W6.20).

use serde_json::json;

use super::{call, PARITY, SPECS};
use crate::mcp::Area;
use crate::tickets::tests::{app, records, ADA, BEN, DEE};
use meridian_domain::v1::{DatasetEntitlement, DatasetRef, ListDatasetsReply};
use meridian_pb::v1::{DatasetLicence, DisplayTerms, ProfessionalDefinition};

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

fn names_tools(by: &str) -> bool {
    let tools: Vec<&str> = SPECS.iter().map(|spec| spec.name).collect();
    let named: Vec<&str> = by.split(' ').filter(|w| !w.is_empty()).collect();
    !named.is_empty() && named.iter().all(|name| tools.contains(name))
}

#[test]
fn the_use_tab_and_every_route_name_their_tools() {
    assert!(crate::admin::data_sources::TABS
        .iter()
        .any(|(id, _)| *id == "use"));
    let tab = PARITY.iter().find(|(k, _)| *k == "tab use").unwrap();
    assert!(names_tools(tab.1));
    let paths = routes(include_str!("../../data_use/pages.rs"));
    assert_eq!(paths.len(), 3, "{paths:?}");
    for path in paths {
        let (_, by) = PARITY
            .iter()
            .find(|(k, _)| *k == path)
            .unwrap_or_else(|| panic!("{path} is routed and names no tool"));
        assert!(names_tools(by), "{path}");
    }
    for spec in SPECS {
        assert!(
            PARITY
                .iter()
                .any(|(_, by)| by.split(' ').any(|w| w == spec.name)),
            "{} reaches nothing",
            spec.name
        );
    }
}

#[test]
fn the_three_approved_names_at_their_levels() {
    let names: Vec<&str> = SPECS.iter().map(|s| s.name).collect();
    assert_eq!(
        names,
        vec![
            "read_data_use",
            "export_data_use",
            "answer_professional_test"
        ]
    );
    for spec in SPECS {
        match spec.name {
            "answer_professional_test" => {
                assert_eq!(spec.area, Area::ProfessionalTest);
                assert!(!spec.reads);
                let schema = (spec.input_schema)();
                let required = schema["required"].as_array().unwrap();
                assert!(required.contains(&json!("note")));
                assert!(required.contains(&json!("answer")));
            }
            _ => {
                assert_eq!(spec.area, Area::DataUse);
                assert!(spec.reads);
            }
        }
    }
}

fn split_listing() -> ListDatasetsReply {
    ListDatasetsReply {
        datasets: vec![DatasetRef {
            dataset: "feed-1:daily".into(),
            instance: "feed-1".into(),
            vendor: "Feed Co".into(),
            ..Default::default()
        }],
        licences: vec![DatasetLicence {
            dataset: "feed-1:daily".into(),
            display_terms: Some(DisplayTerms {
                professional_split: true,
                professional_definition: ProfessionalDefinition::Standard as i32,
                ..Default::default()
            }),
            updated_at_ns: 1,
            ..Default::default()
        }],
        entitlements: vec![DatasetEntitlement {
            dataset: "feed-1:daily".into(),
            instance: crate::tickets::tests::OPS.into(),
            allowed: true,
            ..Default::default()
        }],
    }
}

#[tokio::test]
async fn a_person_answers_once_and_an_admin_changes_it_with_a_note() {
    let app = app();
    let records = records();
    // What the page's read notes: ops-1 is entitled to a split dataset.
    crate::data_use::read(&app, &split_listing(), &records, None)
        .await
        .unwrap();
    let ben = crate::prepared::person_caller(BEN, "Ben Ito", &[]);
    let spec = &SPECS[2];
    // Ben reads ops-1: he owes the standard test, and answers it.
    assert_eq!(
        crate::data_use::owed(&records, BEN, &[], crate::tickets::tests::OPS).await,
        vec!["standard".to_string()]
    );
    let refused = call(&app, &ben, spec, json!({"answer": "non_professional"})).await;
    assert_eq!(refused["outcome"], "refused", "{refused}");
    assert!(
        refused["detail"].as_str().unwrap().contains("note"),
        "{refused}"
    );
    let made = call(
        &app,
        &ben,
        spec,
        json!({"answer": "non_professional", "note": "my own investing"}),
    )
    .await;
    assert_eq!(made["outcome"], "made", "{made}");
    assert_eq!(made["data"]["record"]["kind"], "professional_status_set");
    assert_eq!(made["data"]["record"]["to"], "non_professional");
    assert!(
        crate::data_use::owed(&records, BEN, &[], crate::tickets::tests::OPS)
            .await
            .is_empty()
    );
    // Once: a second answer is refused.
    let again = call(
        &app,
        &ben,
        spec,
        json!({"answer": "professional", "note": "changed my mind"}),
    )
    .await;
    assert_eq!(again["reason"], "already_answered", "{again}");
    // Ada may not change Ben's; Dee, a deployment admin, may, with a note.
    let ada = crate::prepared::person_caller(ADA, "Ada Park", &[]);
    let not = call(
        &app,
        &ada,
        spec,
        json!({"subject": BEN, "answer": "professional", "note": "x"}),
    )
    .await;
    assert_eq!(not["reason"], "not_permitted", "{not}");
    let dee = crate::prepared::person_caller(DEE, "Dee Admin", &[]);
    let changed = call(
        &app,
        &dee,
        spec,
        json!({"subject": BEN, "answer": "professional", "note": "registered as an adviser"}),
    )
    .await;
    assert_eq!(changed["outcome"], "made", "{changed}");
    assert_eq!(changed["data"]["record"]["from"], "non_professional");
    assert_eq!(changed["data"]["record"]["by"], DEE);
    let kept = crate::data_use::changes_of(BEN).await.unwrap();
    assert_eq!(kept.len(), 2);
    // A test no data asks is refused naming it.
    let unknown = call(
        &app,
        &ada,
        spec,
        json!({"test": "nobody-1:daily", "answer": "professional", "note": "x"}),
    )
    .await;
    assert_eq!(unknown["fields"][0]["path"], "test", "{unknown}");
}
