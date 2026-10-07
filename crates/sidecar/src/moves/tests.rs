//! A plugin at the edge reports a move of its raw records through its
//! sidecar (W4.13, fixtures/sidecar/record-move.yaml), against an in-memory
//! bus with a stand-in conductor that answers the plugin's configuration --
//! a hold of 2,190 days over the instance -- and keeps each move that
//! reached it, refusing a deletion of the unit it holds as archived that
//! names no person, as the real one does.

use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ed25519_dalek::{Signer as _, SigningKey};
use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::v1::{Envelope, PluginConfiguration};
use meridian_pb::v1::sidecar_service_server::SidecarService;
use meridian_pb::v1::{
    AccessLevel, CallerAssertion, CallerClaims, MoveOutcome, PluginDeclaration, RawRecordKind,
    RecordMoveRequest, Refusal, RefusalReason, RegisterRequest, RoleAccess, StorageDeclaration,
};
use prost::Message;
use tonic::{Code, Request, Status};

use super::{within_hold, RECORD_MOVE};
use crate::front_door::{Verifier, HEADER};
use crate::grants::Contract;
use crate::service::{Identity, Sidecar};
use crate::typed::REFUSAL_METADATA;

const KEY_ID: &str = "k-test";
const INSTANCE: &str = "custody-snaptrade-1";
const DAY_NS: i64 = 86_400 * 1_000_000_000;
const HOLD_DAYS: u32 = 2190;

type Heard = Arc<Mutex<Vec<(RecordMoveRequest, Envelope)>>>;

fn key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

fn contract() -> Contract {
    Contract::parse(
        "topic\tkind\tpublisher\tsubscriber\n\
         platform.config.query.plugin-configuration\tquery\tsidecar\tconductor\n\
         platform.config.command.record-move\tcommand\tsidecar\tconductor\n",
        "name\tkind\ncustody\trole\noperations\trole\nsidecar\tcomponent\n\
         conductor\tcomponent\n",
    )
    .unwrap()
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64
}

/// The fixture's declaration: activity, archivable, and a FIX session's
/// state, which is not.
fn declaration() -> PluginDeclaration {
    PluginDeclaration {
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
                    name: "session".into(),
                    label: "Session state".into(),
                    window_days: 30,
                    archivable: false,
                },
            ],
        }),
        ..Default::default()
    }
}

async fn registered(roles: &[&str]) -> (Sidecar, Heard) {
    let bus = Arc::new(Bus::single(
        INSTANCE,
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    bus.serve(crate::configuration::PLUGIN_CONFIGURATION, |_| {
        Ok((
            "meridian.v1.PluginConfiguration".into(),
            PluginConfiguration {
                plugin_instance_id: INSTANCE.into(),
                hold_days: HOLD_DAYS,
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    let heard: Heard = Arc::default();
    let keeping = Arc::clone(&heard);
    bus.serve(RECORD_MOVE, move |envelope| {
        let asked = RecordMoveRequest::decode(&envelope.payload[..]).map_err(|e| e.to_string())?;
        let person = envelope
            .meta
            .as_ref()
            .map(|m| m.acting_for_subject.clone())
            .unwrap_or_default();
        if asked.outcome == MoveOutcome::Deleted as i32
            && asked.unit == "activity/ACC-1/2010-01"
            && person.is_empty()
        {
            return Err("permission_denied: deleting a unit the archive holds is an admin's act, after its hold".into());
        }
        keeping.lock().unwrap().push((asked, envelope));
        Ok(("meridian.v1.RecordMoveReply".into(), Vec::new()))
    });
    let roles: Vec<String> = roles.iter().map(|r| r.to_string()).collect();
    let sidecar = Sidecar::under(
        &contract(),
        bus,
        "DEP-test",
        Identity::new(INSTANCE, roles.clone()),
    )
    .with_verifier(Arc::new(Verifier::holding(
        INSTANCE,
        KEY_ID,
        key().verifying_key(),
    )));
    let reply = sidecar
        .register(Request::new(RegisterRequest {
            schema_version: "v16".into(),
            // Storage is an edge plugin's alone (decisions/028).
            declaration: roles
                .iter()
                .any(|role: &String| role == "custody")
                .then(declaration),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(reply.admitted, "{}", reply.refusal_reason);
    (sidecar, heard)
}

/// The header a page at `level` hands the plugin for Ben, holding `level`
/// on custody.
fn header_for(level: AccessLevel) -> String {
    let issued = now();
    let claims = CallerClaims {
        subject: "local|ben".into(),
        display_name: "Ben Ito".into(),
        audience_instance_id: INSTANCE.into(),
        issued_at_ns: issued,
        expires_at_ns: issued + 60_000_000_000,
        assertion_id: "a-1".into(),
        level: level as i32,
        roles: vec![RoleAccess {
            role: "custody".into(),
            level: level as i32,
            ..Default::default()
        }],
        ..CallerClaims::default()
    }
    .encode_to_vec();
    URL_SAFE_NO_PAD.encode(
        CallerAssertion {
            signature: key().sign(&claims).to_bytes().to_vec(),
            claims,
            key_id: KEY_ID.into(),
        }
        .encode_to_vec(),
    )
}

/// The fixture's move: a month of an account's activity, archived by its
/// window, from long before any hold.
fn archived() -> RecordMoveRequest {
    RecordMoveRequest {
        record_kind: "activity".into(),
        unit: "activity/ACC-1/2019-03".into(),
        record_count: 214,
        first_received_ns: 1_551_398_400_000_000_000,
        last_received_ns: 1_554_076_799_000_000_000,
        outcome: MoveOutcome::Archived as i32,
        rule: "activity_window_days 2555".into(),
    }
}

fn carrying(header: Option<&str>, asked: RecordMoveRequest) -> Request<RecordMoveRequest> {
    let mut request = Request::new(asked);
    if let Some(header) = header {
        request
            .metadata_mut()
            .insert(HEADER, header.parse().unwrap());
    }
    request
}

fn refusal_of(status: &Status) -> Refusal {
    status
        .metadata()
        .get_bin(REFUSAL_METADATA)
        .map(|value| Refusal::decode(value.to_bytes().unwrap().as_ref()).unwrap())
        .unwrap_or_default()
}

#[tokio::test]
async fn a_windows_move_as_the_plugin_itself_is_recorded_naming_no_person() {
    let (sidecar, heard) = registered(&["custody"]).await;
    sidecar
        .record_move(carrying(None, archived()))
        .await
        .unwrap();
    let heard = heard.lock().unwrap();
    assert_eq!(heard.len(), 1);
    assert_eq!(heard[0].0, archived());
    let meta = heard[0].1.meta.clone().unwrap_or_default();
    assert_eq!(meta.acting_for_subject, "");
    assert_eq!(meta.publisher_instance_id, INSTANCE);
}

#[tokio::test]
async fn a_restore_for_a_person_with_write_names_them_and_no_rule() {
    let (sidecar, heard) = registered(&["custody"]).await;
    let header = header_for(AccessLevel::Write);
    let restored = RecordMoveRequest {
        outcome: MoveOutcome::Restored as i32,
        rule: String::new(),
        ..archived()
    };
    sidecar
        .record_move(carrying(Some(&header), restored.clone()))
        .await
        .unwrap();
    let (moved, envelope) = heard.lock().unwrap()[0].clone();
    assert_eq!(moved, restored);
    assert_eq!(
        envelope.meta.unwrap_or_default().acting_for_subject,
        "local|ben"
    );

    // A rule beside the person is refused: the record names one or the other.
    let both = sidecar
        .record_move(carrying(Some(&header), archived()))
        .await
        .unwrap_err();
    assert_eq!(both.code(), Code::InvalidArgument);
    assert_eq!(refusal_of(&both).fields, ["rule"]);
}

#[tokio::test]
async fn a_restore_for_a_person_holding_only_read_is_refused() {
    let (sidecar, heard) = registered(&["custody"]).await;
    let refused = sidecar
        .record_move(carrying(
            Some(&header_for(AccessLevel::Read)),
            RecordMoveRequest {
                outcome: MoveOutcome::Restored as i32,
                rule: String::new(),
                ..archived()
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert!(
        refused.message().contains("for write"),
        "{}",
        refused.message()
    );
    assert!(heard.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_deletion_inside_the_hold_is_refused_with_its_code_and_nothing_leaves() {
    let (sidecar, heard) = registered(&["custody"]).await;
    let recent = now() - 30 * DAY_NS;
    let refused = sidecar
        .record_move(carrying(
            None,
            RecordMoveRequest {
                first_received_ns: recent - DAY_NS,
                last_received_ns: recent,
                outcome: MoveOutcome::Deleted as i32,
                rule: "activity_past_window deleted".into(),
                ..archived()
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::FailedPrecondition);
    assert_eq!(
        refusal_of(&refused).reason,
        RefusalReason::WithinHold as i32
    );
    assert!(
        refused.message().contains("inside the hold of 2,190 days"),
        "{}",
        refused.message()
    );
    assert!(heard.lock().unwrap().is_empty());

    // Past the hold, a window's deletion of a unit in storage is recorded.
    sidecar
        .record_move(carrying(
            None,
            RecordMoveRequest {
                unit: "activity/ACC-1/2011-01".into(),
                first_received_ns: 1_293_840_000_000_000_000,
                last_received_ns: 1_296_000_000_000_000_000,
                outcome: MoveOutcome::Deleted as i32,
                rule: "activity_past_window deleted".into(),
                ..archived()
            },
        ))
        .await
        .unwrap();
    assert_eq!(heard.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_deletion_for_a_person_without_admin_or_of_an_archived_unit_as_itself_is_refused() {
    let (sidecar, heard) = registered(&["custody"]).await;
    let old = RecordMoveRequest {
        unit: "activity/ACC-1/2010-01".into(),
        first_received_ns: 1_262_304_000_000_000_000,
        last_received_ns: 1_264_982_399_000_000_000,
        outcome: MoveOutcome::Deleted as i32,
        rule: String::new(),
        ..archived()
    };
    let by_a_writer = sidecar
        .record_move(carrying(Some(&header_for(AccessLevel::Write)), old.clone()))
        .await
        .unwrap_err();
    assert_eq!(by_a_writer.code(), Code::PermissionDenied);
    assert!(by_a_writer.message().contains("an admin's act"));

    // As itself, the conductor refuses a deletion of a unit it holds as
    // archived, and the sidecar says it as the conductor did.
    let as_itself = sidecar
        .record_move(carrying(
            None,
            RecordMoveRequest {
                rule: "activity_past_window deleted".into(),
                ..old.clone()
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(as_itself.code(), Code::PermissionDenied);
    assert!(heard.lock().unwrap().is_empty());

    // An admin's, after its hold, is recorded for them.
    sidecar
        .record_move(carrying(Some(&header_for(AccessLevel::Admin)), old))
        .await
        .unwrap();
    assert_eq!(
        heard.lock().unwrap()[0]
            .1
            .meta
            .clone()
            .unwrap_or_default()
            .acting_for_subject,
        "local|ben"
    );
}

#[tokio::test]
async fn each_field_is_refused_by_its_path() {
    let (sidecar, heard) = registered(&["custody"]).await;
    let cases: Vec<(RecordMoveRequest, &str)> = vec![
        (
            RecordMoveRequest {
                record_kind: "statements".into(),
                ..archived()
            },
            "record_kind",
        ),
        (
            RecordMoveRequest {
                record_kind: "session".into(),
                ..archived()
            },
            "record_kind",
        ),
        (
            RecordMoveRequest {
                unit: String::new(),
                ..archived()
            },
            "unit",
        ),
        (
            RecordMoveRequest {
                unit: "u".repeat(513),
                ..archived()
            },
            "unit",
        ),
        (
            RecordMoveRequest {
                rule: "r".repeat(201),
                ..archived()
            },
            "rule",
        ),
        (
            RecordMoveRequest {
                record_count: 0,
                ..archived()
            },
            "record_count",
        ),
        (
            RecordMoveRequest {
                last_received_ns: 1_551_398_300_000_000_000,
                ..archived()
            },
            "last_received_ns",
        ),
        (
            RecordMoveRequest {
                first_received_ns: 0,
                ..archived()
            },
            "first_received_ns",
        ),
        (
            RecordMoveRequest {
                outcome: MoveOutcome::Unspecified as i32,
                ..archived()
            },
            "outcome",
        ),
        (
            RecordMoveRequest {
                outcome: 9,
                ..archived()
            },
            "outcome",
        ),
        (
            RecordMoveRequest {
                rule: String::new(),
                ..archived()
            },
            "rule",
        ),
    ];
    for (asked, path) in cases {
        let refused = sidecar
            .record_move(carrying(None, asked.clone()))
            .await
            .unwrap_err();
        assert_eq!(refused.code(), Code::InvalidArgument, "{asked:?}");
        assert_eq!(refusal_of(&refused).fields, [path], "{}", refused.message());
        assert!(refused.message().starts_with(path), "{}", refused.message());
    }
    assert!(heard.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_plugin_holding_no_edge_role_is_refused() {
    let (sidecar, heard) = registered(&["operations"]).await;
    let refused = sidecar
        .record_move(carrying(None, archived()))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert!(refused.message().contains("no edge role"));
    assert!(heard.lock().unwrap().is_empty());
}

#[test]
fn a_hold_is_counted_from_when_the_last_record_was_received() {
    let now = 1_791_417_600_000_000_000;
    assert_eq!(within_hold(now - 10 * DAY_NS, 0, now), None);
    assert!(within_hold(now - 10 * DAY_NS, 30, now).is_some());
    assert_eq!(within_hold(now - 31 * DAY_NS, 30, now), None);
    assert!(within_hold(now - 10 * DAY_NS, 30, now)
        .unwrap()
        .contains("nothing of it is deleted before"));
}
