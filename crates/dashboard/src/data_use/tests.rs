use std::collections::BTreeMap;

use meridian_access::AccessLevel;
use meridian_domain::v1::{
    AccessEntry, AccessGroup, AccessRecords, DatasetEntitlement, DatasetRef, KnownPluginRoles,
    ListDatasetsReply, Permission, UserGroup,
};
use meridian_pb::v1::{
    DatasetDeclaration, DatasetLicence, DisplayTerms, LicenceMeter, ProfessionalDefinition,
};

use super::store::{DataUseStore, InMemory, Seen};
use super::*;
use crate::tickets::tests::{records as base_records, ADA, BEN, OPS};

const SA: &str = "SA-01JBX000000000000000000AB";
const EMS: &str = "ems-1";

fn licence(dataset: &str, split: Option<ProfessionalDefinition>, source: &str) -> DatasetLicence {
    DatasetLicence {
        dataset: dataset.into(),
        kept: true,
        meters: vec![
            LicenceMeter::Display as i32,
            LicenceMeter::NonDisplay as i32,
        ],
        display_terms: split.map(|definition| DisplayTerms {
            professional_split: true,
            professional_definition: definition as i32,
            ..Default::default()
        }),
        underlying_source: source.into(),
        delay_label: "Delayed 15 minutes".into(),
        updated_at_ns: 1,
        ..Default::default()
    }
}

fn listed(datasets: &[(&str, Option<ProfessionalDefinition>, &str, &[&str])]) -> ListDatasetsReply {
    ListDatasetsReply {
        datasets: datasets
            .iter()
            .map(|(dataset, _, _, _)| DatasetRef {
                dataset: (*dataset).into(),
                instance: "feed-1".into(),
                vendor: "Feed Co".into(),
                declaration: Some(DatasetDeclaration {
                    key: "daily".into(),
                    vendor: "Feed Co".into(),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .collect(),
        licences: datasets
            .iter()
            .map(|(dataset, split, source, _)| licence(dataset, *split, source))
            .collect(),
        entitlements: datasets
            .iter()
            .flat_map(|(dataset, _, _, instances)| {
                instances.iter().map(move |instance| DatasetEntitlement {
                    dataset: (*dataset).into(),
                    instance: (*instance).into(),
                    allowed: true,
                    ..Default::default()
                })
            })
            .collect(),
    }
}

/// Ada writes and Ben reads on ops-1 (an `operations` plugin); a service
/// account reads it too; ems-1 is an `ems`.
fn records() -> AccessRecords {
    let mut records = base_records();
    records.user_groups.push(UserGroup {
        user_group_id: "UG-sa".into(),
        name: "Robots".into(),
        directory_groups: vec![],
        logins: vec![SA.into()],
    });
    records.access_groups.push(AccessGroup {
        access_group_id: "AG-ops-read-sa".into(),
        name: "ops read".into(),
        entries: vec![AccessEntry {
            plugin_instance_id: OPS.into(),
            level: AccessLevel::Read as i32,
            role: "operations".into(),
        }],
        built_in: false,
    });
    records.permissions.push(Permission {
        permission_id: "P-sa".into(),
        user_group_id: "UG-sa".into(),
        account_group_id: "AcG-A".into(),
        access_group_id: "AG-ops-read-sa".into(),
    });
    // Grants name the role they reach once the plugin's roles are known.
    for group in &mut records.access_groups {
        for entry in &mut group.entries {
            if entry.plugin_instance_id == OPS {
                entry.role = "operations".into();
            }
        }
    }
    records.known_plugins = vec![
        KnownPluginRoles {
            plugin_instance_id: OPS.into(),
            roles: vec!["operations".into()],
        },
        KnownPluginRoles {
            plugin_instance_id: EMS.into(),
            roles: vec!["ems".into()],
        },
    ];
    records
}

#[test]
fn a_split_dataset_asks_the_standard_test_or_its_vendors() {
    let standard = licence("a", Some(ProfessionalDefinition::Standard), "");
    assert_eq!(test_of("a", &standard).as_deref(), Some(STANDARD));
    let vendor = licence("b", Some(ProfessionalDefinition::Vendor), "");
    assert_eq!(test_of("b", &vendor).as_deref(), Some("b"));
    assert_eq!(test_of("c", &licence("c", None, "")), None);
}

#[test]
fn an_ems_trades_automatically_and_every_other_role_is_internal() {
    assert_eq!(category_of(&["ems".into()]), "automated_trading");
    assert_eq!(category_of(&["operations".into()]), "internal");
    assert_eq!(category_of(&[]), "internal");
}

#[test]
fn who_could_use_a_dataset_is_read_from_the_records_and_entitlements() {
    let records = records();
    let reply = listed(&[(
        "feed-1:daily",
        Some(ProfessionalDefinition::Standard),
        "SIP",
        &[OPS, EMS],
    )]);
    let rows = crate::admin::data_sources::rows(&reply, &records);
    let mut answers = BTreeMap::new();
    answers.insert(
        (BEN.to_string(), STANDARD.to_string()),
        Status::NonProfessional,
    );
    let seen = seen_now(&rows, &records, &answers);
    let of = |kind: Kind| -> Vec<(&str, &str)> {
        seen.iter()
            .filter(|s| s.kind == kind)
            .map(|s| (s.who.as_str(), s.classification.as_str()))
            .collect()
    };
    // Dee administers ops-1, which reads nothing; Cy writes custody-1.
    assert_eq!(
        of(Kind::Person),
        vec![(ADA, "professional"), (BEN, "non_professional")]
    );
    assert_eq!(
        of(Kind::Plugin),
        vec![(OPS, "internal"), (EMS, "automated_trading")]
    );
    assert_eq!(of(Kind::ServiceAccount), vec![(SA, "")]);
    assert!(seen.iter().all(|s| s.underlying_source == "SIP"));

    let rows_seen = merged(Vec::new(), &seen, 5);
    let counts = by_dataset(&rows_seen, &rows);
    assert_eq!(counts.len(), 1);
    let c = &counts[0];
    assert_eq!((c.persons, c.professional, c.non_professional), (2, 1, 1));
    assert!(c.split);
    assert_eq!(c.plugins_total(), 2);
    assert_eq!(c.plugins_said(), "1 automated trading, 1 internal");
    assert_eq!(c.service_accounts, 1);
    assert_eq!(c.meters, vec!["display", "non_display"]);
    assert_eq!(c.timeliness, "Delayed 15 minutes");
    let json = counts_json(c, false);
    assert!(json["display"]["persons_served"].is_null(), "{json}");
    assert_eq!(json["display"]["persons_entitled"], 2);
    assert_eq!(json["display"]["non_professional"], 1);
}

#[test]
fn a_source_counts_each_person_once_across_its_datasets() {
    let records = records();
    let reply = listed(&[
        ("feed-1:daily", None, "SIP", &[OPS]),
        ("feed-1:intraday", None, "SIP", &[OPS]),
        ("other-1:daily", None, "Other", &[]),
    ]);
    let rows = crate::admin::data_sources::rows(&reply, &records);
    let seen = seen_now(&rows, &records, &BTreeMap::new());
    let rows_seen = merged(Vec::new(), &seen, 5);
    let sources = by_source(&rows_seen, &rows);
    let sip = sources.iter().find(|c| c.key == "SIP").unwrap();
    assert_eq!(sip.persons, 2);
    assert_eq!(sip.plugins_total(), 1);
    assert_eq!(sip.beside, "feed-1:daily, feed-1:intraday");
    assert!(!sip.split);
    let other = sources.iter().find(|c| c.key == "Other").unwrap();
    assert_eq!((other.persons, other.plugins_total()), (0, 0));
    // Not split: no professional figures are said.
    let json = counts_json(sip, true);
    assert!(json["display"].get("professional").is_none(), "{json}");
    assert_eq!(json["datasets"].as_array().unwrap().len(), 2);
}

#[test]
fn a_month_keeps_who_was_first_seen_when() {
    let seen = vec![Seen {
        dataset: "d".into(),
        kind: Kind::Person,
        who: ADA.into(),
        classification: String::new(),
        underlying_source: "S".into(),
    }];
    let store = InMemory::default();
    store.note("2026-10", &seen, 10).unwrap();
    store.note("2026-10", &seen, 20).unwrap();
    store.note("2026-11", &seen, 30).unwrap();
    let october = store.month("2026-10").unwrap();
    assert_eq!(october.len(), 1);
    assert_eq!(
        (october[0].first_seen_ns, october[0].last_seen_ns),
        (10, 20)
    );
    assert_eq!(store.months().unwrap(), vec!["2026-10", "2026-11"]);
    // Laid over now: last seen moves, first stays, nobody twice.
    let now = merged(october, &seen, 25);
    assert_eq!(now.len(), 1);
    assert_eq!((now[0].first_seen_ns, now[0].last_seen_ns), (10, 25));
}

#[test]
fn an_answer_is_its_own_record_and_stands() {
    let store = InMemory::default();
    let change = store::StatusChange {
        subject: BEN.into(),
        test: STANDARD.into(),
        from: None,
        to: Status::NonProfessional,
        by: BEN.into(),
        through_delegation: String::new(),
        client_name: String::new(),
        at_ns: 7,
        note: String::new(),
    };
    store.set_status(&change).unwrap();
    store
        .set_status(&store::StatusChange {
            from: Some(Status::NonProfessional),
            to: Status::Professional,
            by: "local|dee".into(),
            note: "registered as an adviser".into(),
            at_ns: 8,
            ..change.clone()
        })
        .unwrap();
    let standing = store.statuses().unwrap();
    assert_eq!(standing.len(), 1);
    assert_eq!(standing[0].status, Status::Professional);
    let records = store.status_changes(BEN).unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].note, "registered as an adviser");
    assert_eq!(records[1].from, None);
}

#[test]
fn the_csv_carries_the_reports_fields_and_never_a_formula() {
    let records = records();
    let reply = listed(&[(
        "feed-1:daily",
        Some(ProfessionalDefinition::Standard),
        "=SIP",
        &[OPS],
    )]);
    let rows = crate::admin::data_sources::rows(&reply, &records);
    let seen = seen_now(&rows, &records, &BTreeMap::new());
    let rows_seen = merged(Vec::new(), &seen, 1_792_800_000_000_000_000);
    let out = csv("2026-10", &rows_seen, &rows, |subject| {
        crate::tickets::display_name(&records, subject)
    });
    let mut lines = out.lines();
    assert_eq!(
        lines.next().unwrap(),
        "month,dataset,underlying_source,meter,kind,who,name,classification,activated,deactivated,quantity,timeliness"
    );
    let body: Vec<&str> = lines.collect();
    assert_eq!(body.len(), 4, "{out}");
    assert!(
        body.iter()
            .any(|l| l.contains(",display,person,local|ada,Ada Park,professional,")),
        "{out}"
    );
    assert!(
        body.iter()
            .any(|l| l.contains(",non_display,plugin,ops-1,,internal,")),
        "{out}"
    );
    assert!(body.iter().all(|l| l.contains(",'=SIP,")), "{out}");
    assert!(
        body.iter().all(|l| l.ends_with(",1,Delayed 15 minutes")),
        "{out}"
    );
}

#[test]
fn a_month_is_four_digits_a_dash_and_a_month() {
    assert!(is_month("2026-10"));
    assert!(!is_month("2026-13"));
    assert!(!is_month("2026-1"));
    assert!(!is_month("26-10-01"));
    assert_eq!(month_of(1_792_800_000_000_000_000), "2026-10");
}

#[test]
fn then_goes_only_to_a_path_on_this_dashboard() {
    assert_eq!(
        pages::then_for_tests("/plugins/ops-1?level=read"),
        "/plugins/ops-1?level=read"
    );
    assert_eq!(pages::then_for_tests("//evil.example"), "/");
    assert_eq!(pages::then_for_tests("https://evil.example"), "/");
    assert_eq!(pages::then_for_tests(""), "/");
}

#[test]
fn the_use_tab_says_served_is_not_counted_yet() {
    let records = records();
    let reply = listed(&[(
        "feed-1:daily",
        Some(ProfessionalDefinition::Standard),
        "SIP",
        &[OPS],
    )]);
    let rows = crate::admin::data_sources::rows(&reply, &records);
    let seen = seen_now(&rows, &records, &BTreeMap::new());
    let read = Read {
        month: "2026-10".into(),
        rows: merged(Vec::new(), &seen, 5),
        listed: rows,
        months: vec!["2026-10".into()],
    };
    let (body, dialog) = pages::tab(&Ok(read), false, &records, "<t>");
    assert!(body.contains("not counted yet"), "{body}");
    assert!(body.contains("data-use=\"1\""), "{body}");
    assert!(
        body.contains("<td class=\"num\" data-persons>2</td>"),
        "{body}"
    );
    assert!(body.contains("2 / 0"), "{body}");
    assert!(body.contains("use.csv?month=2026-10"), "{body}");
    assert!(dialog.contains("action=\"/admin/data-sources/professional-status\""));
    assert!(dialog.contains("name=\"note\""));
}
