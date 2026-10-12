//! Two calls a plugin makes for the person whose page it serves, contract v20:
//! the prepared change a page was opened from (`SidecarService.
//! ReadPreparedCall`, W4.14) and the person's own settings
//! (`SidecarService.SetOwnSettings`, W6.31).
//!
//! Each carries the person's assertion as its `meridian-caller` metadata, and
//! one without it -- the plugin as itself -- is refused: a plugin never reads
//! a prepared change, nor sets a person's own values, for nobody. The sidecar
//! asks the dashboard on the bus as itself, for the instance it serves and no
//! other, with the person stamped on the envelope; the dashboard decides
//! whether the person may see the change (shown to them and theirs to make
//! now) and keeps their values under them. Both rows are the sidecar's own
//! (`platform.config.query.prepared-call`,
//! `platform.config.command.set-own-settings`): no role is granted them.
//!
//! **What is checked here first.** A prepared change's ID is `PC-` and a
//! ULID. A person's own values are set only under Open (`write`), only for a
//! setting this plugin declared `per_person`, only where the person holds
//! `write` on a role the setting serves, and within the bounds the claims
//! carry them under: 32 KiB in all, 64 settings. The dashboard checks each
//! value against its declaration again, as the conductor checks a setting.

use meridian_bus::Stamp;
use meridian_pb::v1::{
    AccessLevel, CallerClaims, ReadPreparedCallReply, ReadPreparedCallRequest, RefusalReason,
    SetOwnSettingsReply, SetOwnSettingsRequest,
};
use prost::Message;
use tonic::{Code, Request, Response, Status};

use crate::service::Sidecar;
use crate::typed::{level_named, refused_naming};

/// ReadPreparedCall on the bus (W4.14): the sidecar asks, the dashboard
/// answers.
pub const PREPARED_CALL: &str = "platform.config.query.prepared-call";
/// PluginSetsOwnSettings on the bus (W6.31).
pub const SET_OWN_SETTINGS: &str = "platform.config.command.set-own-settings";

/// The most of a person's own values the claims carry, in bytes, and how
/// many settings (CallerClaims.own_settings).
pub const OWN_SETTINGS_MOST_BYTES: usize = 32 * 1024;
pub const OWN_SETTINGS_MOST: usize = 64;

/// A prepared change's ID as the dashboard mints it: `PC-` and a ULID, read
/// as Crockford's base 32 (a short one, as the fixtures write, read too).
pub fn is_prepared_call_id(id: &str) -> bool {
    id.strip_prefix("PC-").is_some_and(|ulid| {
        (20..=26).contains(&ulid.len())
            && ulid
                .bytes()
                .all(|b| b.is_ascii_digit() || (b.is_ascii_uppercase() && !b"ILOU".contains(&b)))
    })
}

fn at_path(code: Code, path: &str, words: String) -> Status {
    refused_naming(
        code,
        format!("{path}: {words}"),
        RefusalReason::Unspecified,
        vec![path.to_string()],
    )
}

/// The person on the envelope, and the delegation and client beside them.
fn stamp_for(claims: &CallerClaims) -> Stamp {
    crate::typed::stamp_of(claims, false)
}

impl Sidecar {
    /// The claims of the person the call carries an assertion for, at any
    /// level; refused for the plugin as itself.
    #[allow(clippy::result_large_err)]
    fn vouched_for_own(
        &self,
        request_metadata: &tonic::metadata::MetadataMap,
        topic: &str,
        what: &str,
        now_ns: i64,
    ) -> Result<CallerClaims, Status> {
        let assertion = crate::tickets::carried(request_metadata)?;
        let Some(assertion) = assertion else {
            let refusal = format!("a plugin {what} only for a person it acts for");
            self.note_refusal(&refusal);
            return Err(Status::permission_denied(refusal));
        };
        let verifier = self.verifier.as_ref().ok_or_else(|| {
            Status::unauthenticated(
                "this sidecar holds none of the dashboard's keys, so it can vouch for nobody",
            )
        })?;
        let claims = verifier
            .vouched(&assertion, now_ns)
            .map_err(|refusal| Status::unauthenticated(refusal.said()))?;
        // Never administering (W4.9, contract v21): a prepared change's read
        // and a person's own settings are the deployment's configuration.
        if crate::typed::is_service_account(&claims) {
            let refusal = crate::typed::never_administering(topic, &claims);
            self.note_refusal(&refusal);
            return Err(Status::permission_denied(refusal));
        }
        if !matches!(
            AccessLevel::try_from(claims.level),
            Ok(AccessLevel::Admin | AccessLevel::Write | AccessLevel::Read)
        ) {
            let refusal = format!(
                "{} holds no level on this plugin in this session, so nothing is asked for them",
                claims.subject
            );
            self.note_refusal(&refusal);
            return Err(Status::permission_denied(refusal));
        }
        Ok(claims)
    }

    /// `SidecarService.ReadPreparedCall` (SidecarReadPreparedCall, W4.14).
    pub(crate) async fn read_prepared_for_person(
        &self,
        request: Request<ReadPreparedCallRequest>,
    ) -> Result<Response<ReadPreparedCallReply>, Status> {
        self.admitted()?;
        let now = self.clock.now_ns();
        let claims = self.vouched_for_own(
            request.metadata(),
            PREPARED_CALL,
            "reads a prepared change",
            now,
        )?;
        let asked = request.into_inner();
        if !is_prepared_call_id(&asked.prepared_call_id) {
            return Err(at_path(
                Code::InvalidArgument,
                "prepared_call_id",
                format!(
                    "{:?} is not a prepared change's ID, which is PC- and a ULID",
                    asked.prepared_call_id
                ),
            ));
        }
        let (_, payload) = self
            .bus
            .call_stamped(
                PREPARED_CALL,
                "meridian.v1.ReadPreparedCallRequest",
                asked.encode_to_vec(),
                None,
                None,
                &stamp_for(&claims),
            )
            .await
            .map_err(crate::tickets::answered)?;
        let reply = ReadPreparedCallReply::decode(payload.as_slice()).map_err(|failed| {
            Status::internal(format!("the dashboard's answer did not read: {failed}"))
        })?;
        tracing::info!(
            instance = self.instance_id(),
            by = claims.subject,
            at_level = level_named(claims.level),
            prepared_call = asked.prepared_call_id,
            found = reply.prepared_call.is_some(),
            "a prepared change read for a person"
        );
        Ok(Response::new(reply))
    }

    /// `SidecarService.SetOwnSettings` (SidecarSetOwnSettings, W6.31).
    pub(crate) async fn set_own_settings_for_person(
        &self,
        request: Request<SetOwnSettingsRequest>,
    ) -> Result<Response<SetOwnSettingsReply>, Status> {
        let registration = self.admitted()?;
        let now = self.clock.now_ns();
        let claims = self.vouched_for_own(
            request.metadata(),
            SET_OWN_SETTINGS,
            "saves a person's own settings",
            now,
        )?;
        if claims.level != AccessLevel::Write as i32 {
            let refusal = format!(
                "a person's own settings are saved under Open, and {} opened this session at {}",
                claims.subject,
                level_named(claims.level)
            );
            self.note_refusal(&refusal);
            return Err(Status::permission_denied(refusal));
        }
        let asked = request.into_inner();
        if asked.values.len() > OWN_SETTINGS_MOST {
            return Err(at_path(
                Code::InvalidArgument,
                "values",
                format!(
                    "names {} settings; a person keeps at most {OWN_SETTINGS_MOST} of their own",
                    asked.values.len()
                ),
            ));
        }
        let bytes: usize = asked
            .values
            .iter()
            .map(|v| v.name.len() + v.value.len())
            .sum();
        if bytes > OWN_SETTINGS_MOST_BYTES {
            return Err(at_path(
                Code::InvalidArgument,
                "values",
                format!(
                    "come to {bytes} bytes; a person's own values are at most \
                     {OWN_SETTINGS_MOST_BYTES} bytes in all"
                ),
            ));
        }
        for (i, value) in asked.values.iter().enumerate() {
            let path = format!("values[{i}].name");
            let Some(declared) = registration
                .settings
                .iter()
                .find(|setting| setting.name == value.name)
            else {
                return Err(at_path(
                    Code::InvalidArgument,
                    &path,
                    format!("{:?} is no setting this plugin declares", value.name),
                ));
            };
            if !declared.per_person {
                return Err(at_path(
                    Code::InvalidArgument,
                    &path,
                    format!(
                        "{:?} is not declared per_person: it is set for everyone on the \
                         Settings form, never by a person for themselves",
                        value.name
                    ),
                ));
            }
            // Write on a role the setting serves: an entry per role from v15,
            // or the session's level on a plugin holding one role.
            let writes = |role: &str| {
                claims
                    .roles
                    .iter()
                    .any(|held| held.role == role && held.level == AccessLevel::Write as i32)
                    || (claims.roles.is_empty() && self.identity.roles.len() <= 1)
            };
            let serves: Vec<String> = if declared.roles.is_empty() {
                self.identity.roles.clone()
            } else {
                declared.roles.clone()
            };
            let open_alone = serves.is_empty() && claims.level == AccessLevel::Write as i32;
            if !(open_alone || serves.iter().any(|role| writes(role))) {
                let refusal = format!(
                    "{} holds no write on {} in this session, and {:?} serves only it",
                    claims.subject,
                    serves.join(" or "),
                    value.name
                );
                self.note_refusal(&refusal);
                return Err(at_path(Code::PermissionDenied, &path, refusal));
            }
        }
        let (_, payload) = self
            .bus
            .call_stamped(
                SET_OWN_SETTINGS,
                "meridian.v1.SetOwnSettingsRequest",
                asked.encode_to_vec(),
                None,
                None,
                &stamp_for(&claims),
            )
            .await
            .map_err(crate::tickets::answered)?;
        let reply = SetOwnSettingsReply::decode(payload.as_slice()).map_err(|failed| {
            Status::internal(format!("the dashboard's answer did not read: {failed}"))
        })?;
        tracing::info!(
            instance = self.instance_id(),
            by = claims.subject,
            settings = asked.values.len(),
            "a person's own settings saved"
        );
        Ok(Response::new(reply))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prepared_calls_id_is_pc_and_a_ulid() {
        assert!(is_prepared_call_id("PC-01JAC2Q4M7K00000000000001"));
        for not in [
            "",
            "PC-",
            "TK-01JAC2Q4M7K00000000000001",
            "PC-01JAC2Q4M7K0000000000000I",
            "PC-01jac2q4m7k00000000000001",
            "PC-01JAC2Q4M7K00000000000000123",
        ] {
            assert!(!is_prepared_call_id(not), "{not:?}");
        }
    }
}
