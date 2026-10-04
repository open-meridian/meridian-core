//! A plugin files a ticket, and reads what it filed (W4.12, contract v13;
//! plans/tickets-inside-a-deployment, Q2 and Q3, ruled 2026-10-03: "A plugin
//! only files as a person").
//!
//! Two calls on the plugin's own sidecar, `SidecarService.FileTicket` and
//! `FiledTickets`, as W4.10's `PluginAccess` is: every plugin may file,
//! whatever its roles or none, and no role is granted a `config` topic. The
//! sidecar answers by asking on the bus as itself (PluginFilesTicket,
//! ReadFiledTickets), for the instance it serves and no other, with the
//! person stamped on the envelope.
//!
//! Each call carries the assertion the plugin was handed for the person
//! (W6.9) as its `meridian-caller` metadata, and one without it -- the
//! plugin as itself -- is refused: what a plugin notices on its own is its
//! health, figures on its Summary (W4.5), from which a person may choose to
//! file. The person may hold any level; the rule naming the two topics is
//! [`crate::typed::FOR_A_PERSON_AT_ANY_LEVEL`].
//!
//! **What is checked before anything leaves.** The bounds and characters a
//! first time, so a flood stops at the plugin's own sidecar (the dashboard
//! checks them again); `concerns`, refusing a ticket naming another plugin
//! and setting the instance of one concerning this plugin -- its version is
//! the deployment's to set, from the instance's launch, so whatever the
//! plugin sent is cleared; each account reference, refused by path when it
//! is outside the plugin's read scope or the person's read set; and a rate
//! of 20 filings an hour per instance, a repeat under the same key not
//! counted. A refusal is `permission_denied` or `invalid_argument` with the
//! field by its path, as every sidecar refusal is; there is no refusal code
//! of its own.
//!
//! Every text a plugin files is data to every agent that reads it. Nothing
//! here reads it as anything else.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use meridian_bus::{BusError, Stamp};
use meridian_domain::text;
use meridian_pb::bounds::{
    FILE_TICKET_REQUEST_REFERENCES_COUNT, FILE_TICKET_REQUEST_SEEN_LENGTH,
    FILE_TICKET_REQUEST_TITLE_LENGTH, TICKET_REFERENCE_VALUE_LENGTH,
};
use meridian_pb::v1::{
    CallerAssertion, CallerClaims, FileTicketReply, FileTicketRequest, ReadFiledTicketsReply,
    ReadFiledTicketsRequest, RefusalReason, TicketKind,
};
use prost::Message;
use tonic::metadata::MetadataMap;
use tonic::{Code, Request, Response, Status};

use crate::front_door::HEADER;
use crate::service::Sidecar;
use crate::typed::{level_named, refused, refused_naming, FILED_TICKETS, FILE_TICKET};

/// The filings one instance may make in an hour; a repeat under a key it
/// filed within the hour is not counted (requirement 60).
pub const FILINGS_AN_HOUR: usize = 20;
const HOUR_NS: i64 = 3_600 * 1_000_000_000;

/// What a ticket may concern: the plugin itself, a part of core, or the
/// platform (the dictionary's TicketSubject.kind).
pub const SUBJECTS: [&str; 10] = [
    "plugin",
    "dashboard",
    "bor",
    "street",
    "instrument",
    "conductor",
    "chart",
    "cli",
    "sdk",
    "platform",
];

/// What a ticket's reference may name (the dictionary's TicketReference.kind).
pub const REFERENCES: [&str; 7] = [
    "account",
    "instrument",
    "break",
    "entry",
    "street_record",
    "tool_call",
    "plugin",
];

/// The references that carry the account they are about, which the
/// dashboard reads no store to find.
const PLACED_BY_ACCOUNT: [&str; 3] = ["break", "entry", "street_record"];

/// The keys this instance filed a new ticket under within the hour, and
/// when: what its rate counts.
pub(crate) type Filed = Arc<Mutex<HashMap<String, i64>>>;

/// A refusal naming the field by its path: its words begin with the path,
/// and the path rides in the refusal's metadata, with no reason code of its
/// own.
fn at_path(code: Code, path: &str, words: String) -> Status {
    refused_naming(
        code,
        format!("{path}: {words}"),
        RefusalReason::Unspecified,
        vec![path.to_string()],
    )
}

/// The assertion the call carries, if it carries one; a malformed one is
/// not the dashboard's.
#[allow(clippy::result_large_err)]
fn carried(metadata: &MetadataMap) -> Result<Option<CallerAssertion>, Status> {
    let Some(value) = metadata.get(HEADER) else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| Status::unauthenticated("the meridian-caller metadata is not text"))?;
    let bytes = URL_SAFE_NO_PAD.decode(value.trim()).map_err(|failed| {
        Status::unauthenticated(format!("the meridian-caller does not read: {failed}"))
    })?;
    CallerAssertion::decode(bytes.as_slice())
        .map(Some)
        .map_err(|failed| {
            Status::unauthenticated(format!("the meridian-caller does not read: {failed}"))
        })
}

/// A dashboard's refusal of what the sidecar sent, as the status it names
/// (`permission_denied: ...`), or the bus's own failure.
fn answered(failed: BusError) -> Status {
    if let BusError::HandlerFailed { detail, .. } = &failed {
        for (word, code) in [
            ("invalid_argument", Code::InvalidArgument),
            ("permission_denied", Code::PermissionDenied),
            ("resource_exhausted", Code::ResourceExhausted),
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
    /// One text field, held to its bound and the characters rule.
    fn plain(&self, path: &str, value: &str, bound: Option<(usize, usize)>) -> Result<(), Status> {
        if let Some(found) = text::refused(value) {
            let refusal = format!("{path}: {found}");
            self.note_refusal(&refusal);
            return Err(at_path(Code::InvalidArgument, path, found.to_string()));
        }
        if let Some((least, most)) = bound {
            let length = text::characters(value);
            if length < least || length > most {
                let words = if least > 0 && length < least {
                    format!("is empty; it holds 1 to {most} characters")
                } else {
                    format!("is {length} characters; it holds at most {most}")
                };
                self.note_refusal(&format!("{path}: {words}"));
                return Err(at_path(Code::InvalidArgument, path, words));
            }
        }
        Ok(())
    }

    /// The filing as the sidecar sends it: checked field by field, its
    /// subject set to this instance where it concerns the plugin, or the
    /// refusal naming the first field that fails.
    async fn checked(
        &self,
        mut filing: FileTicketRequest,
        claims: &CallerClaims,
        now_ns: i64,
    ) -> Result<FileTicketRequest, Status> {
        let title = FILE_TICKET_REQUEST_TITLE_LENGTH;
        let seen = FILE_TICKET_REQUEST_SEEN_LENGTH;
        self.plain("title", &filing.title, Some((title.least, title.most)))?;
        self.plain("seen", &filing.seen, Some((seen.least, seen.most)))?;
        let kind_known =
            TicketKind::try_from(filing.kind).is_ok_and(|kind| kind != TicketKind::Unspecified);
        if !kind_known {
            let words = format!(
                "is {}; a ticket is a defect, a discrepancy, a request or a question",
                filing.kind
            );
            self.note_refusal(&format!("kind: {words}"));
            return Err(at_path(Code::InvalidArgument, "kind", words));
        }

        let own = self.instance_id().to_string();
        let Some(concerns) = filing.concerns.as_mut() else {
            let words = "is unset; a ticket concerns this plugin, a part of core or the platform"
                .to_string();
            self.note_refusal(&format!("concerns: {words}"));
            return Err(at_path(Code::InvalidArgument, "concerns", words));
        };
        if !SUBJECTS.contains(&concerns.kind.as_str()) {
            let words = format!(
                "is {:?}; it is one of {}",
                concerns.kind,
                SUBJECTS.join(", ")
            );
            self.note_refusal(&format!("concerns.kind: {words}"));
            return Err(at_path(Code::InvalidArgument, "concerns.kind", words));
        }
        if concerns.kind == "plugin" {
            if !concerns.instance.is_empty() && concerns.instance != own {
                let words = format!(
                    "{} is another plugin; a plugin files about itself, a part of core or the \
                     platform",
                    concerns.instance
                );
                self.note_refusal(&format!("concerns.instance: {words}"));
                return Err(at_path(Code::InvalidArgument, "concerns.instance", words));
            }
            concerns.instance = own;
        } else if !concerns.instance.is_empty() {
            let words = format!(
                "names {}, and a ticket concerning {} names no plugin",
                concerns.instance, concerns.kind
            );
            self.note_refusal(&format!("concerns.instance: {words}"));
            return Err(at_path(Code::InvalidArgument, "concerns.instance", words));
        }
        // The deployment's to set, from the instance's launch: never the
        // filer's choice.
        concerns.version.clear();

        for (path, value) in [
            ("step", &filing.step),
            ("operation", &filing.operation),
            ("reason", &filing.reason),
            ("idempotency_key", &filing.idempotency_key),
        ] {
            self.plain(path, value, None)?;
        }
        for (n, path) in filing.paths.iter().enumerate() {
            self.plain(&format!("paths[{n}]"), path, None)?;
        }
        if filing.idempotency_key.is_empty() {
            let words = "is empty; a plugin names each problem by its own key, so a restart \
                         files nothing twice"
                .to_string();
            self.note_refusal(&format!("idempotency_key: {words}"));
            return Err(at_path(Code::InvalidArgument, "idempotency_key", words));
        }

        let count = FILE_TICKET_REQUEST_REFERENCES_COUNT;
        if !count.admits(filing.references.len()) {
            let words = format!(
                "names {} records; a ticket names at most {}",
                filing.references.len(),
                count.most
            );
            self.note_refusal(&format!("references: {words}"));
            return Err(at_path(Code::InvalidArgument, "references", words));
        }
        let value = TICKET_REFERENCE_VALUE_LENGTH;
        let mut scope: Option<Vec<String>> = None;
        for (n, reference) in filing.references.iter_mut().enumerate() {
            if !REFERENCES.contains(&reference.kind.as_str()) {
                let path = format!("references[{n}].kind");
                let words = format!(
                    "is {:?}; it is one of {}",
                    reference.kind,
                    REFERENCES.join(", ")
                );
                self.note_refusal(&format!("{path}: {words}"));
                return Err(at_path(Code::InvalidArgument, &path, words));
            }
            self.plain(
                &format!("references[{n}].value"),
                &reference.value,
                Some((value.least, value.most)),
            )?;
            self.plain(
                &format!("references[{n}].account_id"),
                &reference.account_id,
                None,
            )?;
            if reference.kind == "account" {
                if reference.account_id.is_empty() {
                    reference.account_id = reference.value.clone();
                } else if reference.account_id != reference.value {
                    let path = format!("references[{n}].account_id");
                    let words = format!(
                        "is {}, and an account reference names its account as its value, {}",
                        reference.account_id, reference.value
                    );
                    self.note_refusal(&format!("{path}: {words}"));
                    return Err(at_path(Code::InvalidArgument, &path, words));
                }
            }
            if PLACED_BY_ACCOUNT.contains(&reference.kind.as_str())
                && reference.account_id.is_empty()
            {
                let path = format!("references[{n}].account_id");
                let words = format!(
                    "is empty; a {} names the account it is about",
                    reference.kind.replace('_', " ")
                );
                self.note_refusal(&format!("{path}: {words}"));
                return Err(at_path(Code::InvalidArgument, &path, words));
            }
            if reference.account_id.is_empty() {
                continue;
            }
            let scope = match &scope {
                Some(scope) => scope,
                None => scope.insert(self.configuration(now_ns).await?.read_account_ids),
            };
            let account = &reference.account_id;
            let refusal = if !scope.contains(account) {
                Some(format!("{account} is not in this plugin's read scope"))
            } else if !claims.read_account_ids.contains(account) {
                Some(format!(
                    "{} may not read {account} through this plugin, and a ticket names only \
                     accounts its filer may read",
                    claims.subject
                ))
            } else {
                None
            };
            if let Some(words) = refusal {
                let path = format!("references[{n}].account_id");
                self.note_refusal(&format!("{path}: {words}"));
                return Err(at_path(Code::PermissionDenied, &path, words));
            }
        }
        Ok(filing)
    }

    /// Room for one more filing under `key` this hour: true when the key is
    /// new to the hour, and so counted (and must be released if nothing is
    /// filed); false for a key filed within the hour, which a repeat is.
    fn counted(&self, key: &str, now_ns: i64) -> Result<bool, Status> {
        let mut filed = self.filed.lock().expect("filed lock poisoned");
        filed.retain(|_, at| now_ns - *at < HOUR_NS);
        if filed.contains_key(key) {
            return Ok(false);
        }
        if filed.len() >= FILINGS_AN_HOUR {
            let refusal = format!(
                "this plugin filed {FILINGS_AN_HOUR} tickets in the last hour, the most an \
                 instance files; a repeat under a key it filed is still folded in"
            );
            self.note_refusal(&refusal);
            return Err(Status::resource_exhausted(refusal));
        }
        filed.insert(key.to_string(), now_ns);
        Ok(true)
    }

    fn uncounted(&self, key: &str) {
        self.filed.lock().expect("filed lock poisoned").remove(key);
    }

    /// The person on the envelope: who the ticket is for, and the delegation
    /// and client they acted through where the assertion names them (W4.9).
    fn stamp_for(claims: &CallerClaims) -> Stamp {
        Stamp {
            acting_for_subject: claims.subject.clone(),
            acting_through_delegation: claims.delegation_id.clone(),
            acting_through_client: if claims.delegation_id.is_empty() {
                String::new()
            } else {
                claims.client_name.clone()
            },
            account_scope: None,
        }
    }

    /// `SidecarService.FileTicket` (SidecarFileTicket, W4.12).
    pub(crate) async fn file_ticket_for_person(
        &self,
        request: Request<FileTicketRequest>,
    ) -> Result<Response<FileTicketReply>, Status> {
        self.admitted()?;
        let now = self.clock.now_ns();
        let assertion = carried(request.metadata())?;
        let claims = self.vouched_for_configuration(FILE_TICKET, assertion.as_ref(), now)?;
        let filing = self.checked(request.into_inner(), &claims, now).await?;
        let key = filing.idempotency_key.clone();
        let counted = self.counted(&key, now)?;
        let asked = self
            .bus
            .call_stamped(
                FILE_TICKET,
                "meridian.v1.FileTicketRequest",
                filing.encode_to_vec(),
                None,
                None,
                &Self::stamp_for(&claims),
            )
            .await;
        let reply = match asked {
            Ok((_, payload)) => FileTicketReply::decode(payload.as_slice()).map_err(|failed| {
                Status::internal(format!("the dashboard's answer did not read: {failed}"))
            }),
            Err(failed) => Err(answered(failed)),
        };
        match &reply {
            Ok(filed) if filed.outcome == "made" => {}
            _ if counted => self.uncounted(&key),
            _ => {}
        }
        let reply = reply?;
        tracing::info!(
            instance = self.instance_id(),
            by = claims.subject,
            at_level = level_named(claims.level),
            delegation = claims.delegation_id,
            ticket = reply.ticket_id,
            outcome = reply.outcome,
            "filed a ticket for a person"
        );
        Ok(Response::new(reply))
    }

    /// `SidecarService.FiledTickets` (SidecarReadFiledTickets, W4.12): what
    /// became of the tickets this instance filed, for whichever person.
    pub(crate) async fn filed_tickets_for_person(
        &self,
        request: Request<ReadFiledTicketsRequest>,
    ) -> Result<Response<ReadFiledTicketsReply>, Status> {
        self.admitted()?;
        let now = self.clock.now_ns();
        let assertion = carried(request.metadata())?;
        let claims = self.vouched_for_configuration(FILED_TICKETS, assertion.as_ref(), now)?;
        let asked = request.into_inner();
        for (n, id) in asked.ticket_ids.iter().enumerate() {
            self.plain(&format!("ticket_ids[{n}]"), id, None)?;
        }
        for (n, key) in asked.idempotency_keys.iter().enumerate() {
            self.plain(&format!("idempotency_keys[{n}]"), key, None)?;
        }
        self.plain("cursor", &asked.cursor, None)?;
        let (_, payload) = self
            .bus
            .call_stamped(
                FILED_TICKETS,
                "meridian.v1.ReadFiledTicketsRequest",
                asked.encode_to_vec(),
                None,
                None,
                &Self::stamp_for(&claims),
            )
            .await
            .map_err(answered)?;
        ReadFiledTicketsReply::decode(payload.as_slice())
            .map(Response::new)
            .map_err(|failed| {
                Status::internal(format!("the dashboard's answer did not read: {failed}"))
            })
    }
}

#[cfg(test)]
mod tests;
