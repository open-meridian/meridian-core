//! The two bus rows a plugin's sidecar asks (W4.12): PluginFilesTicket on
//! `platform.config.command.file-ticket` and ReadFiledTickets on
//! `platform.config.query.filed-tickets`. The first topics the dashboard
//! answers.
//!
//! Answered for the instance the envelope names and no other, and only for
//! the person the sidecar stamped on it, with the delegation and client
//! beside them where it did: a filing with no person is the plugin as
//! itself, which files nothing (ruled 2026-10-03). The sidecar checks first;
//! everything is checked again here, as a page's and a client's filing is.
//! A refusal's words begin with the status the sidecar answers its plugin
//! with -- `invalid_argument: `, `permission_denied: ` -- and name the field
//! by its path.
//!
//! One dashboard replica answers (the chart keeps it at one): a second would
//! hear each filing twice, which the unique index on an open ticket's key
//! makes harmless (kernel/the-bus-has-no-queue-groups).

use std::sync::Arc;

use meridian_domain::v1::Envelope;
use meridian_pb::v1::{FileTicketRequest, ReadFiledTicketsRequest};
use prost::Message;

use super::{Actor, Author, Provenance, Refused};
use crate::web::App;

/// The words a sidecar reads its status from.
fn refusal(refused: &Refused) -> String {
    let status = match refused.status {
        403 | 404 => "permission_denied",
        429 => "resource_exhausted",
        400 => "invalid_argument",
        _ => "unavailable",
    };
    format!("{status}: {}", refused.detail)
}

/// The person a plugin's request is for, and the instance it came from;
/// refused when no person is stamped.
fn acting(app: &App, envelope: &Envelope) -> Result<(Actor, String), String> {
    let meta = envelope.meta.clone().unwrap_or_default();
    let instance = meta.publisher_instance_id;
    if !crate::plugins::is_instance(&instance) {
        return Err("permission_denied: this is answered for a plugin instance's sidecar".into());
    }
    if meta.acting_for_subject.is_empty() {
        return Err(
            "permission_denied: a plugin files a ticket, and reads what it filed, only for a \
             person it acts for"
                .into(),
        );
    }
    let records = app
        .records
        .current(app.clock.now_ns())
        .map_err(|stale| format!("unavailable: {stale}"))?;
    let subject = meta.acting_for_subject;
    let author = Author {
        provenance: Provenance::Plugin,
        person: super::display_name(&records, &subject),
        subject: subject.clone(),
        delegation_id: meta.acting_through_delegation,
        client_name: meta.acting_through_client,
        instance: instance.clone(),
    };
    Ok((
        Actor {
            access: super::access_of(&records, &subject),
            author,
            through_delegation: false,
        },
        instance,
    ))
}

/// Answer both topics on the bus, from now on.
pub fn serve(app: Arc<App>) {
    let filing = Arc::clone(&app);
    app.bus.serve(super::FILE_TICKET, move |envelope| {
        let (actor, _) = acting(&filing, &envelope)?;
        let asked = FileTicketRequest::decode(&envelope.payload[..])
            .map_err(|failed| format!("invalid_argument: the filing did not read: {failed}"))?;
        // Off the async runtime already: the bus runs each handler on the
        // blocking pool, where waiting on the rows is allowed.
        let reply = tokio::runtime::Handle::current()
            .block_on(super::file(&filing, &actor, asked))
            .map_err(|refused| refusal(&refused))?;
        Ok((
            "meridian.v1.FileTicketReply".to_string(),
            reply.encode_to_vec(),
        ))
    });
    let reading = Arc::clone(&app);
    app.bus.serve(super::FILED_TICKETS, move |envelope| {
        let (_, instance) = acting(&reading, &envelope)?;
        let asked = ReadFiledTicketsRequest::decode(&envelope.payload[..])
            .map_err(|failed| format!("invalid_argument: the read did not read: {failed}"))?;
        let reply = tokio::runtime::Handle::current()
            .block_on(super::filed(&reading, &instance, &asked))
            .map_err(|refused| refusal(&refused))?;
        Ok((
            "meridian.v1.ReadFiledTicketsReply".to_string(),
            reply.encode_to_vec(),
        ))
    });
}
