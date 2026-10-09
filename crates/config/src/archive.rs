//! An edge plugin's older records and the archive (contract v16;
//! spec/an-edge-plugins-older-records-move-to-the-archive, decisions/028 as
//! amended 2026-10-05, decisions/032).
//!
//! What the conductor keeps and decides of it, each change its own record
//! (decisions/031), never a record's content:
//!
//! - **The holds** (W6.25), a deployment admin's from the deployment's
//!   Settings: the least days a record is kept anywhere, for one edge role or
//!   for every one. The hold over an instance is the longest of its edge
//!   roles' and the one for every role, delivered to its sidecar on the
//!   plugin's configuration; write-once only where the deployment's archive
//!   can lock, and refused, saying why, where it cannot.
//! - **The archives** (W8.7), a deployment admin's from a plugin's Manage
//!   page: allowed with a bound or none, changed, withdrawn
//!   ([`crate::plugins`], which restarts the instance through CreatePlugin).
//! - **The moves** (W4.13), each reported by a plugin's sidecar as itself:
//!   the hold checked again, a unit the archive holds deleted only for a
//!   person (its sidecar admits that only for an admin), and `archived` only
//!   for an instance allowed an archive; a retry recorded once.
//! - **The window settings** (W6.11): `<kind>_window_days` never below the
//!   hold over the instance, and `<kind>_past_window` archived only where an
//!   archive is allowed and the kind is archivable, refused naming the
//!   setting, with no code of its own.

use std::collections::BTreeMap;

use meridian_domain::v1::{
    Hold, MoveRecord, PluginArchive, ReadMovesReply, ReadMovesRequest, SetHoldRequest,
    SetPluginSettingsRequest,
};
use meridian_domain::{thousands, EDGE_ROLES};
use meridian_pb::v1::{
    MoveOutcome, RawRecordKind, RecordMoveReply, RecordMoveRequest, RefusalReason,
};

use crate::store::{in_the_archive, note_refused, Author, Snapshot, Store};

pub const SET_HOLD: &str = "platform.config.command.set-hold";
pub const RECORD_MOVE: &str = "platform.config.command.record-move";
pub const READ_MOVES: &str = "platform.config.query.moves";
pub const ALLOW_ARCHIVE: &str = "platform.config.command.allow-archive";
pub const WITHDRAW_ARCHIVE: &str = "platform.config.command.withdraw-archive";

/// The most days a hold reaches, as a window does (the dictionary's).
pub const MOST_DAYS: u32 = 36_500;

/// How many moves a page of the Summary's holds.
pub const MOVES_A_PAGE: usize = 50;

const DAY_NS: i64 = 86_400 * 1_000_000_000;

/// Where the deployment keeps archives, as its install named it (W7.1,
/// W8.7): nowhere, a volume or path on a local or on-premises deployment, or
/// a bucket per instance in a cloud's cold class.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ArchiveKind {
    #[default]
    None,
    Path,
    Bucket,
}

/// The deployment's archive, as the chart's `pluginArchive` gives the
/// conductor it: where, and whether it can lock what it holds for a hold
/// that needs records that cannot be altered (object lock, a cloud bucket's
/// alone).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArchiveGrant {
    pub kind: ArchiveKind,
    pub locks: bool,
}

impl ArchiveGrant {
    /// As the chart says it: `MERIDIAN_ARCHIVE` (`path`, `bucket`, or empty
    /// for none) and `MERIDIAN_ARCHIVE_LOCKS` (`true` only where the bucket
    /// was made with object lock). A local archive never locks.
    pub fn named(kind: &str, locks: &str) -> Result<ArchiveGrant, String> {
        let kind = match kind.trim() {
            "" | "none" => ArchiveKind::None,
            "path" => ArchiveKind::Path,
            "bucket" => ArchiveKind::Bucket,
            other => {
                return Err(format!(
                    "MERIDIAN_ARCHIVE is {other:?}; it is path, bucket, or empty for none"
                ))
            }
        };
        let locks = locks.trim() == "true";
        if locks && kind != ArchiveKind::Bucket {
            return Err(
                "MERIDIAN_ARCHIVE_LOCKS is true, and only a cloud bucket made with object lock \
                 can hold records that cannot be altered"
                    .into(),
            );
        }
        Ok(ArchiveGrant { kind, locks })
    }
}

/// The roles an instance holds, as its sidecar last reported them or, where
/// none has, as its latest launch said.
pub fn roles_of<'a>(snapshot: &'a Snapshot, instance: &str) -> Option<&'a [String]> {
    snapshot
        .plugins
        .iter()
        .find(|plugin| plugin.plugin_instance_id == instance)
        .map(|plugin| plugin.roles.as_slice())
        .or_else(|| {
            snapshot
                .catalogue
                .launches
                .iter()
                .rev()
                .find(|launch| launch.instance_id == instance)
                .map(|launch| launch.roles.as_slice())
        })
}

/// The edge roles of `roles` (decisions/028).
pub fn edge_roles(roles: &[String]) -> Vec<String> {
    roles
        .iter()
        .filter(|role| EDGE_ROLES.contains(&role.as_str()))
        .cloned()
        .collect()
}

/// The hold over an instance (W6.25): the longest of its edge roles' holds
/// and the one for every role, and whether any hold that long or shorter
/// standing over it needs records that cannot be altered. Nothing over an
/// instance holding no edge role.
pub fn hold_over(snapshot: &Snapshot, instance: &str) -> (u32, bool) {
    let edge = edge_roles(roles_of(snapshot, instance).unwrap_or_default());
    if edge.is_empty() {
        return (0, false);
    }
    snapshot
        .holds
        .iter()
        .filter(|hold| hold.role.is_empty() || edge.contains(&hold.role))
        .fold((0, false), |(days, once), hold| {
            (days.max(hold.days), once || hold.write_once)
        })
}

/// An instance's archive, where a deployment admin allows it one now.
pub fn archive_allowed<'a>(snapshot: &'a Snapshot, instance: &str) -> Option<&'a PluginArchive> {
    snapshot
        .archives
        .iter()
        .find(|archive| archive.instance_id == instance && archive.allowed)
}

/// A hold as asked, checked (W6.25): an edge role or none, within the
/// dictionary's days, and write-once only where the deployment's archive
/// can lock.
pub fn hold_refused(request: &SetHoldRequest, grant: ArchiveGrant) -> Option<String> {
    if !request.role.is_empty() && !EDGE_ROLES.contains(&request.role.as_str()) {
        return Some(format!(
            "{} is not an edge role; a hold names one of {}, or none for every one",
            request.role,
            EDGE_ROLES.join(", ")
        ));
    }
    if request.days > MOST_DAYS {
        return Some(format!(
            "a hold of {} days is past the most, {MOST_DAYS}",
            request.days
        ));
    }
    if request.write_once && request.days > 0 && !grant.locks {
        return Some(match grant.kind {
            ArchiveKind::Bucket => "this deployment's archive bucket was not made with object \
                 lock, so a hold needing records that cannot be altered would be claimed and not \
                 kept; it is refused rather than claimed"
                .into(),
            ArchiveKind::Path => "this deployment's archive is a local volume, which cannot \
                 lock what it holds, so a hold needing records that cannot be altered would be \
                 claimed and not kept; it is refused rather than claimed"
                .into(),
            ArchiveKind::None => "this deployment has no archive, so nothing can hold records \
                 that cannot be altered; a write-once hold is refused rather than claimed"
                .into(),
        });
    }
    None
}

/// The kinds whose window settings a plugin declared (W6.11): a kind is one
/// whose `<kind>_window_days` and `<kind>_past_window` are both declared, or
/// one its declaration names.
fn window_kind<'a>(
    name: &'a str,
    suffix: &str,
    declared: &[meridian_pb::v1::SettingDeclaration],
    kinds: Option<&[RawRecordKind]>,
) -> Option<&'a str> {
    let kind = name.strip_suffix(suffix)?;
    let named = kinds.is_some_and(|kinds| kinds.iter().any(|k| k.name == kind));
    let both = ["_window_days", "_past_window"].iter().all(|other| {
        let wanted = format!("{kind}{other}");
        declared.iter().any(|d| d.name == wanted)
    });
    (named || both).then_some(kind)
}

/// Why a settings change a plugin's admin asked cannot stand by the windows'
/// rules (W6.11, contract v16), naming the setting; or nothing. `kinds` is
/// what the plugin last declared, where it is known.
pub fn windows_refused(
    snapshot: &Snapshot,
    request: &SetPluginSettingsRequest,
    kinds: Option<&[RawRecordKind]>,
) -> Option<String> {
    let instance = request.plugin_instance_id.as_str();
    let declared = snapshot
        .declared_settings
        .get(instance)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let (hold, _) = hold_over(snapshot, instance);
    for given in &request.values {
        if window_kind(&given.name, "_window_days", declared, kinds).is_some() && hold > 0 {
            if let Ok(days) = given.value.trim().parse::<u64>() {
                if days < u64::from(hold) {
                    return Some(format!(
                        "{}: {} is below the hold of {} days",
                        given.name,
                        thousands(days),
                        thousands(u64::from(hold))
                    ));
                }
            }
        }
        if let Some(kind) = window_kind(&given.name, "_past_window", declared, kinds) {
            if given.value.trim() != "archived" {
                continue;
            }
            if archive_allowed(snapshot, instance).is_none() {
                return Some(format!(
                    "{}: archived needs an archive, and a deployment admin has allowed {instance} \
                     none; its records past their window are kept until one is",
                    given.name
                ));
            }
            let archivable = kinds
                .and_then(|kinds| kinds.iter().find(|k| k.name == kind))
                .map(|k| k.archivable);
            if archivable == Some(false) {
                return Some(format!(
                    "{}: archived, and {kind} is declared not archivable",
                    given.name
                ));
            }
        }
    }
    None
}

/// The hold's refusal of a deletion, with its code, or nothing (W6.25).
pub fn deletion_within_hold(
    moved: &RecordMoveRequest,
    hold_days: u32,
    now_ns: i64,
) -> Option<String> {
    if moved.outcome != MoveOutcome::Deleted as i32 || hold_days == 0 {
        return None;
    }
    let ends = moved
        .last_received_ns
        .saturating_add(i64::from(hold_days).saturating_mul(DAY_NS));
    (ends > now_ns).then(|| {
        meridian_bus::refusal(
            RefusalReason::WithinHold as i32,
            format!(
                "the unit's last record is inside the hold of {} days over this instance; \
                 nothing is recorded, and the unit is kept",
                thousands(u64::from(hold_days))
            ),
        )
    })
}

/// W4.13: a move a sidecar reports for its own instance, as the conductor
/// records it, or the refusal. A retry -- the unit's latest move the same
/// outcome -- is answered as recorded, and recorded once.
pub fn record_move(
    store: &dyn Store,
    snapshot: &Snapshot,
    instance: &str,
    moved: RecordMoveRequest,
    author: &Author,
    now_ns: i64,
) -> Result<RecordMoveReply, String> {
    let person = author.by.as_str();
    if instance.is_empty() {
        return Err("a move is reported by a plugin's sidecar, for its own instance".into());
    }
    if let Some(roles) = roles_of(snapshot, instance) {
        if edge_roles(roles).is_empty() {
            return Err(format!(
                "permission_denied: {instance} holds no edge role, and only a plugin at the \
                 edge keeps raw records (decisions/028)"
            ));
        }
    }
    if moved.outcome == MoveOutcome::Archived as i32
        && archive_allowed(snapshot, instance).is_none()
    {
        return Err(format!(
            "invalid_argument: record_kind: {instance} is allowed no archive, so nothing of {} \
             is archived; records past their window are kept",
            moved.record_kind
        ));
    }
    let (hold, _) = hold_over(snapshot, instance);
    if let Some(refusal) = deletion_within_hold(&moved, hold, now_ns) {
        return Err(refusal);
    }
    if moved.outcome == MoveOutcome::Deleted as i32 && person.is_empty() {
        let latest = store
            .latest_move(instance, &moved.record_kind, &moved.unit)
            .map_err(|failed| failed.to_string())?;
        let archived = latest
            .and_then(|record| record.r#move)
            .is_some_and(|latest| in_the_archive(latest.outcome));
        if archived {
            return Err(format!(
                "permission_denied: {} is in the archive, and deleting an archived unit is an \
                 admin's act after its hold (requirement 9), never a window's",
                moved.unit
            ));
        }
    }
    // The delegation and client the person acted through, beside them
    // (contract v17); never alone.
    let through = !person.is_empty();
    let record = MoveRecord {
        r#move: Some(moved),
        person: person.to_string(),
        at_ns: now_ns,
        acting_through_delegation: if through {
            author.delegation.clone()
        } else {
            String::new()
        },
        client_name: if through && !author.delegation.is_empty() {
            author.client.clone()
        } else {
            String::new()
        },
    };
    let recorded = store
        .record_move(instance, &record)
        .map_err(|failed| failed.to_string())?;
    let moved = record.r#move.unwrap_or_default();
    tracing::info!(
        instance,
        kind = moved.record_kind,
        unit = moved.unit,
        records = moved.record_count,
        outcome = moved.outcome,
        rule = moved.rule,
        person,
        again = !recorded,
        "a move of raw records recorded"
    );
    Ok(RecordMoveReply {})
}

/// W6.9: a page of an instance's moves, newest first, what the archive
/// holds of each kind, and its archive as allowed.
pub fn read_moves(
    store: &dyn Store,
    snapshot: &Snapshot,
    request: &ReadMovesRequest,
) -> Result<ReadMovesReply, String> {
    let instance = request.plugin_instance_id.as_str();
    let before = if request.cursor.is_empty() {
        0
    } else {
        request
            .cursor
            .strip_prefix("before-")
            .and_then(|n| n.parse::<i64>().ok())
            .filter(|n| *n > 0)
            .ok_or_else(|| format!("{:?} is not a cursor this answered", request.cursor))?
    };
    let mut page = store
        .moves(instance, before, MOVES_A_PAGE + 1)
        .map_err(|failed| failed.to_string())?;
    let more = page.len() > MOVES_A_PAGE;
    page.truncate(MOVES_A_PAGE);
    let next_cursor = match (more, page.last()) {
        (true, Some(last)) => format!("before-{}", last.move_id),
        _ => String::new(),
    };
    Ok(ReadMovesReply {
        moves: page.into_iter().map(|held| held.record).collect(),
        next_cursor,
        archived: store
            .archived(instance)
            .map_err(|failed| failed.to_string())?,
        archive: snapshot
            .archives
            .iter()
            .find(|archive| archive.instance_id == instance)
            .cloned(),
    })
}

/// W6.25: a hold as the conductor records it, or the refusal.
pub fn set_hold(
    store: &dyn Store,
    grant: ArchiveGrant,
    request: &SetHoldRequest,
    author: &Author,
    now_ns: i64,
) -> Result<Hold, String> {
    let by = author.by.as_str();
    if by.is_empty() {
        return Err("a hold is a deployment admin's to set, and this is sent for nobody".into());
    }
    if let Some(refusal) = hold_refused(request, grant) {
        return Err(refusal);
    }
    if let Some(refusal) = note_refused(&request.note) {
        return Err(refusal);
    }
    let hold = Hold {
        role: request.role.clone(),
        days: request.days,
        write_once: request.write_once && request.days > 0,
        updated_by: by.to_string(),
        updated_at_ns: now_ns,
        acting_through_delegation: author.delegation.clone(),
        client_name: author.client.clone(),
    };
    store
        .set_hold(&hold, &request.note)
        .map_err(|failed| failed.to_string())?;
    tracing::info!(
        role = if hold.role.is_empty() {
            "every edge role"
        } else {
            hold.role.as_str()
        },
        days = hold.days,
        write_once = hold.write_once,
        by,
        "a hold on raw records set"
    );
    Ok(hold)
}

/// The record kinds each plugin last declared, by instance, as its sidecar's
/// report carried them (W4.8): what a past-the-window setting is checked
/// against. Kept in memory, as the conductor keeps the accounts each
/// connection reported, since each report carries them whole.
pub type Kinds = std::sync::Mutex<BTreeMap<String, Vec<RawRecordKind>>>;

#[cfg(test)]
mod tests;
