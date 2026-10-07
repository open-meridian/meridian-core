//! A plugin at the edge reports a move of its raw records (W4.13, contract
//! v16; spec/an-edge-plugins-older-records-move-to-the-archive, requirements
//! 4, 8, 9 and 10).
//!
//! One call on the plugin's own sidecar, `SidecarService.RecordMove`, as
//! filing a ticket is (W4.12): a plugin holding an edge role reports one
//! unit of a kind of raw record archived, restored, returned to the archive
//! or deleted, before it removes anything, and the sidecar asks the
//! conductor on the bus as itself (PluginRecordsMove), for the instance it
//! serves and no other. Nothing of a record's content is ever in a move: a
//! count, two times and the plugin's own key.
//!
//! **Who moved it.** As the plugin itself, for a window's move, the move
//! names the rule that made it ("activity_window_days 2555"); acting for a
//! person -- the call carrying the assertion it was handed as its
//! `meridian-caller` metadata -- the person is read from the assertion and
//! stamped on the envelope, never a field the plugin writes, and the move
//! names no rule. A restore, its return and an archiving done for a person
//! are for `write` on one of the plugin's edge roles; a deletion only for
//! `admin` on one (the names' choice f; W4.9's third named exception).
//!
//! **What is checked before anything leaves.** The kind is one the version
//! declared, and `archived` only of one declared archivable; the unit's
//! key, count, span and rule within the dictionary's bounds; the outcome one
//! the contract defines. Each refused `invalid_argument` naming the field
//! by its path. And **the hold** (W6.25): a deletion whose last record was
//! received inside the hold over the instance, which the plugin's
//! configuration carries and the plugin is never told, is refused
//! `failed_precondition` with `REFUSAL_REASON_WITHIN_HOLD`; the conductor
//! refuses it again, and also a deletion of a unit the archive holds that
//! names no person (requirement 9).

use meridian_bus::{BusError, Stamp};
use meridian_domain::{text, thousands, EDGE_ROLES};
use meridian_pb::bounds::{
    RECORD_MOVE_REQUEST_RECORD_COUNT_RANGE, RECORD_MOVE_REQUEST_RECORD_KIND_LENGTH,
    RECORD_MOVE_REQUEST_RULE_LENGTH, RECORD_MOVE_REQUEST_UNIT_LENGTH,
};
use meridian_pb::v1::{
    AccessLevel, CallerClaims, MoveOutcome, RecordMoveReply, RecordMoveRequest, RefusalReason,
};
use prost::Message;
use tonic::{Code, Request, Response, Status};

use crate::service::{Registration, Sidecar};
use crate::typed::{level_named, refused, refused_for, refused_naming};

/// A move recorded with the conductor (PluginRecordsMove, W4.13).
pub(crate) const RECORD_MOVE: &str = "platform.config.command.record-move";

const DAY_NS: i64 = 86_400 * 1_000_000_000;

/// A refusal naming the field by its path, its words beginning with it.
fn at_path(path: &str, words: impl std::fmt::Display) -> Status {
    refused_naming(
        Code::InvalidArgument,
        format!("{path} {words}"),
        RefusalReason::Unspecified,
        vec![path.to_string()],
    )
}

/// What a move is, as a log and a refusal name it.
pub fn outcome_named(outcome: i32) -> &'static str {
    match MoveOutcome::try_from(outcome) {
        Ok(MoveOutcome::Archived) => "archived",
        Ok(MoveOutcome::Restored) => "restored",
        Ok(MoveOutcome::Returned) => "returned",
        Ok(MoveOutcome::Deleted) => "deleted",
        _ => "unspecified",
    }
}

/// The day a time falls on, as a refusal names it.
fn day_of(at_ns: i64) -> String {
    let days = at_ns.div_euclid(DAY_NS);
    // Civil from days since the epoch (Howard Hinnant's algorithm), so the
    // sidecar takes no calendar crate for one sentence.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Why a unit's last record is inside a hold of `hold_days` at `now_ns`, or
/// nothing: the least time a record is kept anywhere, from when it was
/// received (W6.25).
pub fn within_hold(last_received_ns: i64, hold_days: u32, now_ns: i64) -> Option<String> {
    if hold_days == 0 {
        return None;
    }
    let ends = last_received_ns.saturating_add(i64::from(hold_days).saturating_mul(DAY_NS));
    (ends > now_ns).then(|| {
        format!(
            "the unit's last record was received {}, inside the hold of {} days over this \
             instance; nothing of it is deleted before {}",
            day_of(last_received_ns),
            thousands(u64::from(hold_days)),
            day_of(ends)
        )
    })
}

/// The move's fields, each within its bounds and the characters rule, its
/// kind one the version declared and archivable where it is archived; or
/// the refusal naming the first field that fails.
#[allow(clippy::result_large_err)]
pub fn checked(asked: &RecordMoveRequest, registration: &Registration) -> Result<(), Status> {
    let kinds = registration
        .declaration
        .as_ref()
        .and_then(|declaration| declaration.storage.as_ref())
        .map(|storage| storage.record_kinds.as_slice())
        .unwrap_or_default();
    for (path, value, bound) in [
        (
            "record_kind",
            &asked.record_kind,
            RECORD_MOVE_REQUEST_RECORD_KIND_LENGTH,
        ),
        ("unit", &asked.unit, RECORD_MOVE_REQUEST_UNIT_LENGTH),
        ("rule", &asked.rule, RECORD_MOVE_REQUEST_RULE_LENGTH),
    ] {
        if let Some(found) = text::refused(value) {
            return Err(at_path(path, found));
        }
        let length = text::characters(value);
        if !bound.admits(length) {
            return Err(at_path(
                path,
                if length == 0 {
                    format!("is empty; it holds 1 to {} characters", bound.most)
                } else {
                    format!("is {length} characters; it holds at most {}", bound.most)
                },
            ));
        }
    }
    let Some(kind) = kinds.iter().find(|kind| kind.name == asked.record_kind) else {
        return Err(at_path(
            "record_kind",
            format!(
                "is {:?}, a kind this version's declaration does not name",
                asked.record_kind
            ),
        ));
    };
    let count = RECORD_MOVE_REQUEST_RECORD_COUNT_RANGE;
    if !count.admits(i64::try_from(asked.record_count).unwrap_or(i64::MAX)) {
        return Err(at_path(
            "record_count",
            format!(
                "is {}; a unit holds {} to {} records",
                asked.record_count,
                count.least,
                thousands(count.most as u64)
            ),
        ));
    }
    if asked.first_received_ns <= 0 {
        return Err(at_path(
            "first_received_ns",
            "is not set; a unit's span runs from when its first record was received",
        ));
    }
    if asked.last_received_ns <= 0 || asked.last_received_ns < asked.first_received_ns {
        return Err(at_path(
            "last_received_ns",
            format!(
                "is {}; a unit's span ends when its last record was received, never before \
                 its first, {}",
                asked.last_received_ns, asked.first_received_ns
            ),
        ));
    }
    match MoveOutcome::try_from(asked.outcome) {
        Ok(MoveOutcome::Unspecified) | Err(_) => {
            return Err(at_path(
                "outcome",
                format!(
                    "is {}; a move is archived, restored, returned or deleted",
                    asked.outcome
                ),
            ))
        }
        Ok(MoveOutcome::Archived) if !kind.archivable => {
            return Err(at_path(
                "record_kind",
                format!(
                    "is {}, which this version declares not archivable",
                    kind.name
                ),
            ))
        }
        Ok(_) => {}
    }
    Ok(())
}

/// The level a person needs on one of the plugin's edge roles to have a
/// move done for them: `admin` for a deletion, `write` for any other
/// (W4.9's third named exception).
pub fn level_needed(outcome: i32) -> AccessLevel {
    if outcome == MoveOutcome::Deleted as i32 {
        AccessLevel::Admin
    } else {
        AccessLevel::Write
    }
}

/// Whether claims hold `level` on one of `edge` -- the plugin's edge roles
/// -- by their per-role entries; claims with none, from a dashboard before
/// v15, are read as the session's level on a plugin holding one role.
pub fn holds_on_an_edge_role(
    claims: &CallerClaims,
    edge: &[String],
    plugin_roles: usize,
    level: AccessLevel,
) -> bool {
    if claims.roles.is_empty() {
        return plugin_roles <= 1 && claims.level == level as i32;
    }
    claims.level == level as i32
        && claims
            .roles
            .iter()
            .any(|held| edge.contains(&held.role) && held.level == level as i32)
}

/// A refusal of the conductor's, as the status it names: the hold's with
/// its code, or `permission_denied: ...` and its kind; the bus's own failure
/// otherwise.
fn answered(failed: BusError) -> Status {
    if let BusError::HandlerFailed { detail, .. } = &failed {
        if let Some((reason, words)) = meridian_bus::read_refusal(detail) {
            if reason == RefusalReason::WithinHold as i32 {
                return refused_for(
                    Code::FailedPrecondition,
                    words.to_string(),
                    RefusalReason::WithinHold,
                );
            }
        }
        for (word, code) in [
            ("invalid_argument", Code::InvalidArgument),
            ("permission_denied", Code::PermissionDenied),
            ("failed_precondition", Code::FailedPrecondition),
        ] {
            if let Some(words) = detail.strip_prefix(word).and_then(|r| r.strip_prefix(": ")) {
                return Status::new(code, words.to_string());
            }
        }
    }
    refused(failed)
}

// The tonic surface returns `Result<_, Status>` everywhere.
#[allow(clippy::result_large_err)]
impl Sidecar {
    fn edge_roles(&self) -> Vec<String> {
        self.identity
            .roles
            .iter()
            .filter(|role| EDGE_ROLES.contains(&role.as_str()))
            .cloned()
            .collect()
    }

    fn refuse_move(&self, refusal: Status) -> Status {
        self.note_refusal(refusal.message());
        if let Some(live) = self.live.get() {
            live.refused(&format!("a move of raw records: {}", refusal.message()));
        }
        refusal
    }

    /// `SidecarService.RecordMove` (SidecarRecordMove, W4.13).
    pub(crate) async fn record_move_reported(
        &self,
        request: Request<RecordMoveRequest>,
    ) -> Result<Response<RecordMoveReply>, Status> {
        let registration = self.admitted()?;
        let now = self.clock.now_ns();
        let edge = self.edge_roles();
        if edge.is_empty() {
            return Err(self.refuse_move(Status::permission_denied(format!(
                "{} holds no edge role, and only a plugin at the edge keeps raw records to move \
                 (decisions/028)",
                self.instance_id()
            ))));
        }
        let assertion = crate::tickets::carried(request.metadata())?;
        let asked = request.into_inner();
        checked(&asked, &registration).map_err(|refusal| self.refuse_move(refusal))?;

        let claims = match &assertion {
            None => {
                // As itself: a window's move, which names the rule that made
                // it, so the record says what moved it.
                if asked.rule.trim().is_empty() {
                    return Err(self.refuse_move(at_path(
                        "rule",
                        "is empty; a move the plugin makes as itself names the window that \
                         made it, and one made for a person carries their assertion",
                    )));
                }
                None
            }
            Some(assertion) => {
                let verifier = self.verifier.as_ref().ok_or_else(|| {
                    Status::unauthenticated(
                        "this sidecar holds none of the dashboard's keys, so it can vouch for \
                         nobody",
                    )
                })?;
                let claims = verifier
                    .vouched(assertion, now)
                    .map_err(|refusal| Status::unauthenticated(refusal.said()))?;
                if !asked.rule.is_empty() {
                    return Err(self.refuse_move(at_path(
                        "rule",
                        "is set, and a move made for a person names them, read from their \
                         assertion, and no rule",
                    )));
                }
                let needed = level_needed(asked.outcome);
                if !holds_on_an_edge_role(&claims, &edge, self.identity.roles.len(), needed) {
                    let what = if needed == AccessLevel::Admin {
                        "deleting raw records is an admin's act, after the hold (requirement 9)"
                    } else {
                        "a restore, its return and an archiving are done for write"
                    };
                    return Err(self.refuse_move(Status::permission_denied(format!(
                        "{what}, on one of {}; and {} holds {} in this session",
                        edge.join(", "),
                        claims.subject,
                        level_named(claims.level)
                    ))));
                }
                Some(claims)
            }
        };

        // The hold over the instance, which the plugin is never told: no
        // deletion inside it, by a window or by an admin (W6.25).
        if asked.outcome == MoveOutcome::Deleted as i32 {
            let configuration = self.configuration(now).await?;
            if let Some(words) = within_hold(asked.last_received_ns, configuration.hold_days, now) {
                return Err(self.refuse_move(refused_for(
                    Code::FailedPrecondition,
                    words,
                    RefusalReason::WithinHold,
                )));
            }
        }

        let stamp = match &claims {
            None => Stamp::default(),
            Some(claims) => Stamp {
                acting_for_subject: claims.subject.clone(),
                acting_through_delegation: claims.delegation_id.clone(),
                acting_through_client: if claims.delegation_id.is_empty() {
                    String::new()
                } else {
                    claims.client_name.clone()
                },
                account_scope: None,
            },
        };
        self.bus
            .call_stamped(
                RECORD_MOVE,
                "meridian.v1.RecordMoveRequest",
                asked.encode_to_vec(),
                None,
                None,
                &stamp,
            )
            .await
            .map_err(|failed| self.refuse_move(answered(failed)))?;
        tracing::info!(
            instance = self.instance_id(),
            kind = asked.record_kind,
            unit = asked.unit,
            records = asked.record_count,
            outcome = outcome_named(asked.outcome),
            rule = asked.rule,
            by = claims.as_ref().map(|c| c.subject.as_str()).unwrap_or(""),
            "a move of raw records recorded"
        );
        Ok(Response::new(RecordMoveReply {}))
    }
}

#[cfg(test)]
mod tests;
