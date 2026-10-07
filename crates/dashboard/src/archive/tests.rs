//! The raw records panel on an edge plugin's Summary (W6.9, contract v16),
//! drawn from a report and the conductor's moves as fixtures/config/
//! read-moves.yaml and plugin-report.yaml give them.

use meridian_domain::v1::{
    AccessRecords, Hold, MoveRecord, PluginArchive, PluginReport, PluginSettingValue,
    PluginSettingsRecord, ReadMovesReply,
};
use meridian_pb::v1::{
    MoveOutcome, PluginDeclaration, RawRecordKind, RecordMoveRequest, StorageDeclaration,
    StoredSpan,
};

use super::*;

const INSTANCE: &str = "snaptrade-1";

fn report() -> PluginReport {
    PluginReport {
        plugin_instance_id: INSTANCE.into(),
        roles: vec!["custody".into()],
        registered: true,
        declaration: Some(PluginDeclaration {
            storage: Some(StorageDeclaration {
                retention_days: 2555,
                record_kinds: vec![
                    RawRecordKind {
                        name: "activity".into(),
                        label: "Reported activity".into(),
                        window_days: 2555,
                        archivable: true,
                    },
                    RawRecordKind {
                        name: "responses".into(),
                        label: "Raw responses".into(),
                        window_days: 30,
                        archivable: true,
                    },
                ],
            }),
            ..Default::default()
        }),
        stored: vec![StoredSpan {
            record_kind: "activity".into(),
            record_count: 48_210,
            first_received_ns: 1_554_076_800_000_000_000,
            last_received_ns: 1_790_380_500_000_000_000,
        }],
        ..Default::default()
    }
}

fn moved(outcome: MoveOutcome, rule: &str) -> RecordMoveRequest {
    RecordMoveRequest {
        record_kind: "activity".into(),
        unit: "activity/ACC-1/2019-03".into(),
        record_count: 214,
        first_received_ns: 1_551_398_400_000_000_000,
        last_received_ns: 1_554_076_799_000_000_000,
        outcome: outcome as i32,
        rule: rule.into(),
    }
}

fn reply(allowed: bool) -> ReadMovesReply {
    ReadMovesReply {
        moves: vec![
            MoveRecord {
                r#move: Some(moved(MoveOutcome::Restored, "")),
                person: "ben@example.com".into(),
                at_ns: 1_791_590_400_000_000_000,
            },
            MoveRecord {
                r#move: Some(moved(MoveOutcome::Archived, "activity_window_days 2555")),
                person: String::new(),
                at_ns: 1_791_504_000_000_000_000,
            },
        ],
        next_cursor: "before-2".into(),
        archived: vec![StoredSpan {
            record_kind: "activity".into(),
            record_count: 214,
            first_received_ns: 1_551_398_400_000_000_000,
            last_received_ns: 1_554_076_799_000_000_000,
        }],
        archive: allowed.then(|| PluginArchive {
            instance_id: INSTANCE.into(),
            allowed: true,
            most_bytes: 53_687_091_200,
            updated_by: "ada@example.com".into(),
            updated_at_ns: 1_791_417_600_000_000_000,
        }),
    }
}

fn drawn(records: &AccessRecords, moves: &Result<ReadMovesReply, String>, admin: bool) -> String {
    let report = report();
    let (records, moves) = sections(&Panel {
        instance: INSTANCE,
        report: Some(&report),
        records,
        moves,
        deployment_admin: admin,
        token: "<input type=\"hidden\" name=\"form_token\" value=\"t\">",
        cursor: "",
    });
    format!("{records}{moves}")
}

#[test]
fn each_kind_is_one_line_of_storage_and_archive_and_each_move_one_line() {
    let page = drawn(&AccessRecords::default(), &Ok(reply(true)), true);
    assert!(page.contains("id=\"records\""));
    assert!(page.contains(">Reported activity</td>"), "{page}");
    assert!(
        page.contains("<td class=\"wide\" title=\"2,555 days (default)\">"),
        "{page}"
    );
    assert!(
        page.contains("title=\"48,210, 2019-04-01 to 2026-09-25\">48,210<span class=\"dates\">"),
        "{page}"
    );
    assert!(
        page.contains("title=\"214, 2019-03-01 to 2019-03-31\""),
        "{page}"
    );
    assert!(page.contains("data-kind=\"responses\""));
    assert!(page.contains("Archive allowed, at most 50 GiB."));
    assert!(page.contains("<om-pager><table class=\"list one-line moves\" data-moves=\"2\">"));
    assert!(page.contains("data-move=\"Restored\""));
    assert!(page.contains("activity_window_days 2555"));
    assert!(
        page.contains("moves=before-2"),
        "an older page is a link away"
    );
    assert!(page.contains("Change bound") && page.contains("data-withdraw-archive"));
}

#[test]
fn no_archive_says_records_past_their_window_are_kept_and_a_plugin_admin_sees_no_button() {
    let page = drawn(&AccessRecords::default(), &Ok(reply(false)), false);
    assert!(page.contains("No archive allowed: records past their window are kept."));
    assert!(!page.contains("data-allow-archive"));
    let admin = drawn(&AccessRecords::default(), &Ok(reply(false)), true);
    assert!(admin.contains("Allow archive") && !admin.contains("data-withdraw-archive"));
    let unread = drawn(&AccessRecords::default(), &Err("timed out".into()), true);
    assert!(unread.contains("could not be read just now"));
}

#[test]
fn a_window_the_hold_overrides_is_named() {
    let records = AccessRecords {
        holds: vec![Hold {
            role: "custody".into(),
            days: 2190,
            ..Default::default()
        }],
        plugin_settings: vec![PluginSettingsRecord {
            plugin_instance_id: INSTANCE.into(),
            values: vec![PluginSettingValue {
                name: "activity_window_days".into(),
                value: "3650".into(),
            }],
            ..Default::default()
        }],
        ..Default::default()
    };
    let page = drawn(&records, &Ok(reply(true)), true);
    assert!(page.contains("title=\"3,650 days\""));
    assert!(page.contains("30 days (default), held longer"));
    assert!(page.contains("A hold of 2,190 days is over it, longer than responses_window_days"));
}

#[test]
fn a_plugin_keeping_no_raw_records_has_no_panel() {
    let mut inner = report();
    inner.roles = vec!["operations".into()];
    assert!(!keeps_records(Some(&inner)));
    let mut none = report();
    none.declaration = None;
    assert!(!keeps_records(Some(&none)));
    assert!(keeps_records(Some(&report())));
}

#[test]
fn a_bound_is_whole_gib_or_none() {
    let fields = |v: &str| Fields::from([("most_gib".to_string(), v.to_string())]);
    assert_eq!(bound_posted(&fields("")), Ok(0));
    assert_eq!(bound_posted(&fields("50")), Ok(53_687_091_200));
    assert!(bound_posted(&fields("0")).is_err());
    assert!(bound_posted(&fields("1.5")).is_err());
    assert_eq!(bound_said(0), "no bound");
    assert_eq!(bound_said(53_687_091_200), "at most 50 GiB");
}
