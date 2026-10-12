//! A plugin's autonomous work calls one of its own acts as its role's
//! service account (W4.15, contract v21; plans/a-service-account-runs-a-
//! plugins-autonomous-work, "Refreshed 2026-10-10", Q4 and Q14, ruled
//! 2026-10-11): `SidecarService.CallAsServiceAccount`, the row
//! SidecarCallAsServiceAccount.
//!
//! Sent as the plugin itself, with no person behind it: a call carrying a
//! `meridian-caller` is refused, since there is nobody to act for. The
//! sidecar asks the dashboard on the bus as itself, for the instance it
//! serves and no other, stamping no one (PluginCallsAsServiceAccount on
//! `platform.config.command.call-as-service-account`), and answers what the
//! dashboard answers: made, with the act's own answer; prepared, with the
//! prepared change's `PC-` ID; or the dashboard's refusal by the same fields,
//! unchanged. The plugin is handed no assertion by this call: the act's route
//! receives one for that call alone, at that tool's method and path, minted
//! by the dashboard.
//!
//! **What is checked here first**, so a malformed call stops at the plugin's
//! own sidecar and the dashboard is never asked: the role is one the
//! instance was launched with, the tool's name is 1 to 61 characters and no
//! core tool's (`dashboard__...`), the arguments are a JSON object of at most
//! 1 MiB, and the reason is 1 to 4,000 characters of plain text. A refusal is
//! `invalid_argument`, naming the field (`role`, `tool_name`,
//! `arguments_json`, `reason`), with no refusal code of its own.
//!
//! The dashboard then checks what admits the call (its
//! `service_accounts::admission`): the role, and the tool one of its acts;
//! the service account the role runs as, its state and its grant; the level
//! that applies and the gate; the reason; and that the arguments are a JSON
//! object. The 1 MiB bound and the tool name's length are this sidecar's
//! alone.

use std::time::Duration;

use meridian_bus::{BusError, Stamp};
use meridian_domain::text;
use meridian_pb::v1::{CallAsServiceAccountReply, CallAsServiceAccountRequest, RefusalReason};
use prost::Message;
use tonic::{Code, Request, Response, Status};

use crate::service::Sidecar;
use crate::typed::{refused, refused_naming};

/// PluginCallsAsServiceAccount on the bus (W4.15): the sidecar asks, the
/// dashboard answers. The sixth named exception to a `config` command being
/// sent only for a plugin's admin (W4.9).
pub const CALL_AS_SERVICE_ACCOUNT: &str = "platform.config.command.call-as-service-account";

/// The bounds the request's fields are held to (the dictionary's).
pub const TOOL_NAME_MOST: usize = 61;
pub const ARGUMENTS_MOST: usize = 1 << 20;
pub const REASON_MOST: usize = 4_000;

/// How long the sidecar waits for the dashboard: the act's route has 30
/// seconds to answer the dashboard (W6.20), and the dashboard its checks.
pub const ANSWERED_WITHIN: Duration = Duration::from_secs(45);

fn at_path(code: Code, path: &str, words: String) -> Status {
    refused_naming(
        code,
        format!("{path}: {words}"),
        RefusalReason::Unspecified,
        vec![path.to_string()],
    )
}

/// The dashboard's refusal as the status it names, its fields with it: its
/// words are `<status>: [fields=a;b] <words>` (the dashboard's
/// `service_accounts::admission`), the fields riding in the refusal's
/// metadata as every sidecar refusal's do. Anything else is the bus's own.
pub(crate) fn answered(failed: BusError) -> Status {
    if let BusError::HandlerFailed { detail, .. } = &failed {
        for (word, code) in [
            ("invalid_argument", Code::InvalidArgument),
            ("permission_denied", Code::PermissionDenied),
            ("failed_precondition", Code::FailedPrecondition),
            ("resource_exhausted", Code::ResourceExhausted),
            ("unavailable", Code::Unavailable),
        ] {
            let Some(words) = detail.strip_prefix(word).and_then(|r| r.strip_prefix(": ")) else {
                continue;
            };
            let (fields, words) = match words
                .strip_prefix("[fields=")
                .and_then(|rest| rest.split_once("] "))
            {
                Some((named, words)) => (
                    named
                        .split(';')
                        .filter(|field| !field.is_empty())
                        .map(str::to_string)
                        .collect(),
                    words,
                ),
                None => (Vec::new(), words),
            };
            return refused_naming(code, words.to_string(), RefusalReason::Unspecified, fields);
        }
    }
    refused(failed)
}

// The tonic surface returns `Result<_, Status>` everywhere.
#[allow(clippy::result_large_err)]
impl Sidecar {
    /// The request as the sidecar sends it, or the refusal naming the first
    /// field that fails.
    fn shaped(&self, asked: &CallAsServiceAccountRequest) -> Result<(), Status> {
        let refuse = |path: &str, words: String| {
            self.note_refusal(&format!("{path}: {words}"));
            Err(at_path(Code::InvalidArgument, path, words))
        };
        if !self.identity.roles.iter().any(|role| role == &asked.role) {
            let held = if self.identity.roles.is_empty() {
                "no role".to_string()
            } else {
                self.identity.roles.join(", ")
            };
            return refuse(
                "role",
                format!(
                    "{:?} is not a role {} was launched with; it holds {held}",
                    asked.role,
                    self.instance_id()
                ),
            );
        }
        let length = text::characters(&asked.tool_name);
        if length == 0 || length > TOOL_NAME_MOST || text::refused(&asked.tool_name).is_some() {
            return refuse(
                "tool_name",
                format!("names one of this plugin's acts by its tool's name, 1 to {TOOL_NAME_MOST} characters"),
            );
        }
        if asked.tool_name.starts_with("dashboard__") {
            return refuse(
                "tool_name",
                format!(
                    "{:?} is not one of this plugin's acts: a service account reaches its plugin's \
                     own acts on the role it runs for, and no core act",
                    asked.tool_name
                ),
            );
        }
        if asked.arguments_json.len() > ARGUMENTS_MOST {
            return refuse(
                "arguments_json",
                format!(
                    "is {} bytes; at most {ARGUMENTS_MOST}",
                    asked.arguments_json.len()
                ),
            );
        }
        let object = serde_json::from_str::<serde_json::Value>(&asked.arguments_json)
            .map(|value| value.is_object())
            .unwrap_or(false);
        if !object {
            return refuse(
                "arguments_json",
                "is not a JSON object: the act's typed record of inputs, an empty object when \
                 it takes none"
                    .into(),
            );
        }
        let reason = text::characters(&asked.reason);
        if reason == 0 {
            return refuse(
                "reason",
                "is empty; a call says why, in 1 to 4,000 characters".into(),
            );
        }
        if reason > REASON_MOST {
            return refuse(
                "reason",
                format!("is {reason} characters; it holds at most {REASON_MOST}"),
            );
        }
        if let Some(found) = text::refused(&asked.reason) {
            return refuse("reason", found.to_string());
        }
        // The algorithm the work runs (contract v22, W4.9, W4.15): one the
        // version declares, or none.
        if let Err(words) = self.declared_algorithm(&asked.algorithm_id) {
            return refuse("algorithm_id", words);
        }
        Ok(())
    }

    /// Whether `algorithm_id` is one this plugin's version declares
    /// (PluginDeclaration.algorithms, contract v22), or empty, naming none:
    /// what a call as a service account may name, checked again on every
    /// command the call's claims carry it on. The words of the refusal
    /// otherwise.
    pub(crate) fn declared_algorithm(&self, algorithm_id: &str) -> Result<(), String> {
        if algorithm_id.is_empty() {
            return Ok(());
        }
        let declared: Vec<String> = self
            .registration()
            .and_then(|held| held.declaration)
            .map(|declaration| {
                declaration
                    .algorithms
                    .into_iter()
                    .map(|algorithm| algorithm.algorithm_id)
                    .collect()
            })
            .unwrap_or_default();
        if declared.iter().any(|held| held == algorithm_id) {
            return Ok(());
        }
        Err(if declared.is_empty() {
            format!("{algorithm_id:?} is not an algorithm this version declares; it declares none")
        } else {
            format!(
                "{algorithm_id:?} is not an algorithm this version declares; it declares {}",
                declared.join(", ")
            )
        })
    }

    /// `SidecarService.CallAsServiceAccount` (SidecarCallAsServiceAccount,
    /// W4.15).
    pub(crate) async fn call_act_as_service_account(
        &self,
        request: Request<CallAsServiceAccountRequest>,
    ) -> Result<Response<CallAsServiceAccountReply>, Status> {
        self.admitted()?;
        if crate::tickets::carried(request.metadata())?.is_some() {
            let refusal = "a plugin calls its act as its role's service account with no person \
                           behind it, and this call carries a person's assertion: an act for a \
                           person is sent from the request it serves"
                .to_string();
            self.note_refusal(&refusal);
            return Err(Status::invalid_argument(refusal));
        }
        let asked = request.into_inner();
        self.shaped(&asked)?;
        let (_, payload) = self
            .bus
            .call_stamped(
                CALL_AS_SERVICE_ACCOUNT,
                "meridian.v1.CallAsServiceAccountRequest",
                asked.encode_to_vec(),
                None,
                Some(ANSWERED_WITHIN),
                // As itself: nothing a plugin sends here names a person.
                &Stamp::default(),
            )
            .await
            .map_err(|failed| {
                let status = answered(failed);
                self.note_refusal(status.message());
                status
            })?;
        let reply = CallAsServiceAccountReply::decode(payload.as_slice()).map_err(|failed| {
            Status::internal(format!("the dashboard's answer did not read: {failed}"))
        })?;
        tracing::info!(
            instance = self.instance_id(),
            role = asked.role,
            tool = asked.tool_name,
            outcome = reply.outcome,
            prepared_call = reply.prepared_call_id,
            "an act called as the role's service account"
        );
        Ok(Response::new(reply))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dashboards_refusal_keeps_its_status_and_its_fields() {
        let status = answered(BusError::HandlerFailed {
            topic: CALL_AS_SERVICE_ACCOUNT.into(),
            detail:
                "failed_precondition: [fields=role] role: operations runs as no service account"
                    .into(),
        });
        assert_eq!(status.code(), Code::FailedPrecondition);
        assert_eq!(
            status.message(),
            "role: operations runs as no service account"
        );
        let refusal = status
            .metadata()
            .get_bin(crate::typed::REFUSAL_METADATA)
            .expect("the refusal rides beside the status")
            .to_bytes()
            .unwrap();
        let refusal = meridian_pb::v1::Refusal::decode(refusal.as_ref()).unwrap();
        assert_eq!(refusal.fields, vec!["role".to_string()]);
        assert_eq!(refusal.reason, RefusalReason::Unspecified as i32);

        let plain = answered(BusError::HandlerFailed {
            topic: CALL_AS_SERVICE_ACCOUNT.into(),
            detail: "permission_denied: tool_name: may not".into(),
        });
        assert_eq!(plain.code(), Code::PermissionDenied);
        assert_eq!(plain.message(), "tool_name: may not");
    }
}
