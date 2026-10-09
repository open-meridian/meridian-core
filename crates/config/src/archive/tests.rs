//! The holds, the archives and the moves, against the in-memory store
//! (fixtures/config/set-hold.yaml, allow-archive.yaml, plugin-records-move.yaml,
//! read-moves.yaml, set-plugin-settings.yaml's v16 cases).

use meridian_domain::v1::{
    PluginArchive, PluginSettingValue, ReadMovesRequest, SetHoldRequest, SetPluginSettingsRequest,
};
use meridian_pb::v1::{
    MoveOutcome, RawRecordKind, RecordMoveRequest, RefusalReason, SettingDeclaration, SettingType,
};

use super::*;
use crate::store::{KnownPlugin, Store};
use crate::MemoryStore;

const DAY_NS: i64 = 86_400 * 1_000_000_000;
const NOW: i64 = 1_791_417_600_000_000_000;
const INSTANCE: &str = "snaptrade-1";

fn store_with(roles: &[&str]) -> MemoryStore {
    let store = MemoryStore::new();
    store
        .record_plugin(&KnownPlugin {
            plugin_instance_id: INSTANCE.into(),
            roles: roles.iter().map(|r| r.to_string()).collect(),
            last_reported_at_ns: NOW,
        })
        .unwrap();
    store
}

fn hold(role: &str, days: u32) -> SetHoldRequest {
    SetHoldRequest {
        role: role.into(),
        days,
        write_once: false,
        note: String::new(),
    }
}

/// Who made a change: the person, and the delegation where one was used.
fn by(person: &str, delegation: &str) -> crate::store::Author {
    crate::store::Author {
        by: person.into(),
        delegation: delegation.into(),
        client: if delegation.is_empty() {
            String::new()
        } else {
            "Claude".into()
        },
        note: String::new(),
    }
}

fn local() -> ArchiveGrant {
    ArchiveGrant {
        kind: ArchiveKind::Path,
        locks: false,
    }
}

fn allow(store: &MemoryStore) {
    store
        .put_archive(
            &PluginArchive {
                instance_id: INSTANCE.into(),
                allowed: true,
                most_bytes: 53_687_091_200,
                updated_by: "ada@example.com".into(),
                updated_at_ns: NOW,
                acting_through_delegation: String::new(),
                client_name: String::new(),
            },
            "",
        )
        .unwrap();
}

fn moved(unit: &str, outcome: MoveOutcome, rule: &str) -> RecordMoveRequest {
    RecordMoveRequest {
        record_kind: "activity".into(),
        unit: unit.into(),
        record_count: 214,
        first_received_ns: 1_551_398_400_000_000_000,
        last_received_ns: 1_554_076_799_000_000_000,
        outcome: outcome as i32,
        rule: rule.into(),
    }
}

#[test]
fn a_hold_names_an_edge_role_or_none_and_write_once_only_where_it_locks() {
    assert_eq!(hold_refused(&hold("custody", 2190), local()), None);
    assert_eq!(hold_refused(&hold("", 2190), local()), None);
    let oms = hold_refused(&hold("oms", 30), local()).unwrap();
    assert!(oms.starts_with("oms is not an edge role"), "{oms}");
    assert!(hold_refused(&hold("custody", 36_501), local()).is_some());
    let once = SetHoldRequest {
        write_once: true,
        ..hold("custody", 2190)
    };
    let refused = hold_refused(&once, local()).unwrap();
    assert!(refused.contains("cannot lock"), "{refused}");
    assert!(hold_refused(&once, ArchiveGrant::default()).is_some());
    let locking = ArchiveGrant {
        kind: ArchiveKind::Bucket,
        locks: true,
    };
    assert_eq!(hold_refused(&once, locking), None);
    // Clearing a write-once hold is never refused.
    assert_eq!(
        hold_refused(
            &SetHoldRequest {
                days: 0,
                ..once.clone()
            },
            local()
        ),
        None
    );
}

#[test]
fn the_archives_grant_is_read_as_the_chart_says_it() {
    assert_eq!(
        ArchiveGrant::named("", "").unwrap(),
        ArchiveGrant::default()
    );
    assert_eq!(ArchiveGrant::named("path", "").unwrap(), local());
    assert!(ArchiveGrant::named("bucket", "true").unwrap().locks);
    assert!(ArchiveGrant::named("path", "true").is_err());
    assert!(ArchiveGrant::named("nas", "").is_err());
}

#[test]
fn the_hold_over_an_instance_is_its_edge_roles_longest_and_every_roles() {
    let store = store_with(&["custody", "operations"]);
    for (role, days) in [("custody", 2190), ("", 400), ("settlement", 3650)] {
        set_hold(
            &store,
            local(),
            &hold(role, days),
            &by("ada@example.com", ""),
            NOW,
        )
        .unwrap();
    }
    let snapshot = store.snapshot().unwrap();
    assert_eq!(hold_over(&snapshot, INSTANCE), (2190, false));
    // Cleared, its own record: the one for every role stands.
    set_hold(
        &store,
        local(),
        &hold("custody", 0),
        &by("ada@example.com", ""),
        NOW,
    )
    .unwrap();
    let snapshot = store.snapshot().unwrap();
    assert_eq!(hold_over(&snapshot, INSTANCE), (400, false));
    assert_eq!(snapshot.holds.len(), 2);
    // An instance holding no edge role is under none.
    let other = store_with(&["operations"]);
    set_hold(
        &other,
        local(),
        &hold("", 400),
        &by("ada@example.com", ""),
        NOW,
    )
    .unwrap();
    assert_eq!(hold_over(&other.snapshot().unwrap(), INSTANCE), (0, false));
    // A hold sent for nobody is refused.
    assert!(set_hold(&store, local(), &hold("custody", 1), &by("", ""), NOW).is_err());
}

fn declared() -> Vec<SettingDeclaration> {
    ["activity", "responses"]
        .iter()
        .flat_map(|kind| {
            [
                SettingDeclaration {
                    name: format!("{kind}_window_days"),
                    r#type: SettingType::Integer as i32,
                    ..Default::default()
                },
                SettingDeclaration {
                    name: format!("{kind}_past_window"),
                    r#type: SettingType::Choice as i32,
                    ..Default::default()
                },
            ]
        })
        .collect()
}

fn kinds() -> Vec<RawRecordKind> {
    vec![
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
            archivable: false,
        },
    ]
}

fn setting(name: &str, value: &str) -> SetPluginSettingsRequest {
    SetPluginSettingsRequest {
        plugin_instance_id: INSTANCE.into(),
        values: vec![PluginSettingValue {
            name: name.into(),
            value: value.into(),
        }],
        ..Default::default()
    }
}

#[test]
fn a_window_below_the_hold_is_refused_naming_the_setting() {
    let store = store_with(&["custody"]);
    store
        .record_declared_settings(INSTANCE, &declared(), NOW)
        .unwrap();
    set_hold(
        &store,
        local(),
        &hold("custody", 2190),
        &by("ada@example.com", ""),
        NOW,
    )
    .unwrap();
    let snapshot = store.snapshot().unwrap();
    assert_eq!(
        windows_refused(&snapshot, &setting("activity_window_days", "30"), None).as_deref(),
        Some("activity_window_days: 30 is below the hold of 2,190 days")
    );
    assert_eq!(
        windows_refused(&snapshot, &setting("activity_window_days", "3650"), None),
        None
    );
    // Archived, with no archive allowed, and then of a kind not archivable.
    let none = windows_refused(
        &snapshot,
        &setting("activity_past_window", "archived"),
        None,
    )
    .unwrap();
    assert!(
        none.starts_with("activity_past_window: archived needs an archive"),
        "{none}"
    );
    allow(&store);
    let snapshot = store.snapshot().unwrap();
    assert_eq!(
        windows_refused(
            &snapshot,
            &setting("activity_past_window", "archived"),
            Some(&kinds())
        ),
        None
    );
    assert_eq!(
        windows_refused(
            &snapshot,
            &setting("responses_past_window", "archived"),
            Some(&kinds())
        )
        .as_deref(),
        Some("responses_past_window: archived, and responses is declared not archivable")
    );
    // Deleted is an admin's explicit choice, and accepted.
    assert_eq!(
        windows_refused(
            &snapshot,
            &setting("responses_past_window", "deleted"),
            None
        ),
        None
    );
    // A setting that is no window is not the windows' business.
    assert_eq!(
        windows_refused(&snapshot, &setting("poll_window_days", "1"), None),
        None
    );
}

#[test]
fn a_move_is_recorded_once_and_the_archive_holds_its_unit() {
    let store = store_with(&["custody"]);
    allow(&store);
    let snapshot = store.snapshot().unwrap();
    let archived = moved(
        "activity/ACC-1/2019-03",
        MoveOutcome::Archived,
        "activity_window_days 2555",
    );
    record_move(
        &store,
        &snapshot,
        INSTANCE,
        archived.clone(),
        &by("", ""),
        NOW,
    )
    .unwrap();
    // A retry: answered as recorded, and recorded once.
    record_move(&store, &snapshot, INSTANCE, archived, &by("", ""), NOW + 1).unwrap();
    let restored = moved("activity/ACC-1/2019-03", MoveOutcome::Restored, "");
    record_move(
        &store,
        &snapshot,
        INSTANCE,
        restored,
        &by("ben@example.com", "DLG-1"),
        NOW + 2,
    )
    .unwrap();
    let page = read_moves(
        &store,
        &snapshot,
        &ReadMovesRequest {
            plugin_instance_id: INSTANCE.into(),
            cursor: String::new(),
        },
    )
    .unwrap();
    assert_eq!(page.moves.len(), 2);
    assert_eq!(page.moves[0].person, "ben@example.com");
    // The delegation and client beside the person (contract v17); a
    // window's move names neither.
    assert_eq!(page.moves[0].acting_through_delegation, "DLG-1");
    assert_eq!(page.moves[0].client_name, "Claude");
    assert!(
        page.moves[1].acting_through_delegation.is_empty() && page.moves[1].client_name.is_empty()
    );
    assert_eq!(
        page.moves[0].r#move.as_ref().unwrap().outcome,
        MoveOutcome::Restored as i32
    );
    assert_eq!(page.moves[1].person, "");
    assert_eq!(page.archived.len(), 1);
    assert_eq!(page.archived[0].record_count, 214);
    assert_eq!(page.archive.as_ref().unwrap().most_bytes, 53_687_091_200);
    assert_eq!(page.next_cursor, "");
}

#[test]
fn a_deletion_inside_the_hold_or_of_an_archived_unit_by_a_window_is_refused() {
    let store = store_with(&["custody"]);
    allow(&store);
    set_hold(
        &store,
        local(),
        &hold("custody", 2190),
        &by("ada@example.com", ""),
        NOW,
    )
    .unwrap();
    let snapshot = store.snapshot().unwrap();
    let recent = RecordMoveRequest {
        first_received_ns: NOW - 40 * DAY_NS,
        last_received_ns: NOW - 30 * DAY_NS,
        ..moved(
            "activity/ACC-1/2026-09",
            MoveOutcome::Deleted,
            "activity_past_window deleted",
        )
    };
    let refused = record_move(&store, &snapshot, INSTANCE, recent, &by("", ""), NOW).unwrap_err();
    assert_eq!(
        meridian_bus::read_refusal(&refused).map(|(reason, _)| reason),
        Some(RefusalReason::WithinHold as i32)
    );
    // An archived unit, past its hold: a window may not delete it; an admin,
    // whose sidecar vouched for them, may.
    let unit = "activity/ACC-1/2019-03";
    record_move(
        &store,
        &snapshot,
        INSTANCE,
        moved(unit, MoveOutcome::Archived, "activity_window_days 2555"),
        &by("", ""),
        NOW,
    )
    .unwrap();
    let by_window = record_move(
        &store,
        &snapshot,
        INSTANCE,
        moved(unit, MoveOutcome::Deleted, "activity_past_window deleted"),
        &by("", ""),
        NOW,
    )
    .unwrap_err();
    assert!(by_window.starts_with("permission_denied: "), "{by_window}");
    record_move(
        &store,
        &snapshot,
        INSTANCE,
        moved(unit, MoveOutcome::Deleted, ""),
        &by("ada@example.com", ""),
        NOW,
    )
    .unwrap();
    let page = store.archived(INSTANCE).unwrap();
    assert!(
        page.is_empty(),
        "a deleted unit is no longer in the archive: {page:?}"
    );
}

#[test]
fn archived_with_no_archive_allowed_is_refused_naming_record_kind() {
    let store = store_with(&["custody"]);
    let snapshot = store.snapshot().unwrap();
    let refused = record_move(
        &store,
        &snapshot,
        INSTANCE,
        moved("u", MoveOutcome::Archived, "activity_window_days 2555"),
        &by("", ""),
        NOW,
    )
    .unwrap_err();
    assert!(
        refused.starts_with("invalid_argument: record_kind: "),
        "{refused}"
    );
    // A plugin holding no edge role is refused whatever it moves.
    let other = store_with(&["operations"]);
    let snapshot = other.snapshot().unwrap();
    assert!(record_move(
        &other,
        &snapshot,
        INSTANCE,
        moved("u", MoveOutcome::Returned, "restore period 7 days"),
        &by("", ""),
        NOW
    )
    .unwrap_err()
    .starts_with("permission_denied: "));
}

#[test]
fn the_moves_page_newest_first() {
    let store = store_with(&["custody"]);
    allow(&store);
    let snapshot = store.snapshot().unwrap();
    for n in 0..(MOVES_A_PAGE + 5) {
        record_move(
            &store,
            &snapshot,
            INSTANCE,
            moved(
                &format!("activity/ACC-1/{n:04}"),
                MoveOutcome::Archived,
                "activity_window_days 2555",
            ),
            &by("", ""),
            NOW + n as i64,
        )
        .unwrap();
    }
    let ask = |cursor: &str| {
        read_moves(
            &store,
            &snapshot,
            &ReadMovesRequest {
                plugin_instance_id: INSTANCE.into(),
                cursor: cursor.into(),
            },
        )
        .unwrap()
    };
    let first = ask("");
    assert_eq!(first.moves.len(), MOVES_A_PAGE);
    assert_eq!(
        first.moves[0].r#move.as_ref().unwrap().unit,
        format!("activity/ACC-1/{:04}", MOVES_A_PAGE + 4)
    );
    assert!(!first.next_cursor.is_empty());
    let second = ask(&first.next_cursor);
    assert_eq!(second.moves.len(), 5);
    assert_eq!(second.next_cursor, "");
    assert_eq!(
        first.archived[0].record_count,
        214 * (MOVES_A_PAGE as u64 + 5)
    );
    assert!(read_moves(
        &store,
        &snapshot,
        &ReadMovesRequest {
            plugin_instance_id: INSTANCE.into(),
            cursor: "nonsense".into(),
        },
    )
    .is_err());
}
