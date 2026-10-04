//! The dashboard's rules (the spec's Q1, requirements 12 and 13; W6.22):
//! deterministic advice at filing and on each change, attributed to "the
//! dashboard's rules", at most one advice note per kind per change. Advice
//! changes nothing: no rule assigns, sets a due date, resolves, closes,
//! reopens or sends, and none reads a store a person could not.
//!
//! - **The route** the concern implies (the spec's "Routing"): the book's for
//!   a break, the deployment admin's for an instrument record, the firm's for
//!   a plugin, Open Meridian's for core or the platform -- saying that sending
//!   a public issue from the ticket arrives in a later release, with no link
//!   out (Q9).
//! - **A likely duplicate**: another open ticket the filer may see with the
//!   same fingerprint; or, for a plugin's filing under a key whose ticket was
//!   resolved or closed, a recurrence of it, which nothing reopens by itself.
//! - **The firm's own**: the plugin's report (W4.8) names a required setting
//!   with no value, or an external account nobody has linked.
//! - **A credential's shape** in the text, advising the filer to remove it.
//!
//! Every word here is the dashboard's; a ticket's own text is never spliced
//! into advice, only its identifiers.

use super::{quarantine, Ticket};

/// What a rule may read: what the dashboard already holds, cut to the filer.
#[derive(Debug, Default)]
pub struct Context<'a> {
    /// Open tickets the filer may see, the new one not among them.
    pub open_seen: Vec<&'a Ticket>,
    /// The resolved or closed ticket a plugin's repeat under the same key
    /// follows, if one does.
    pub recurrence_of: Option<&'a Ticket>,
    /// The plugin's required settings with no value, by name.
    pub missing_settings: Vec<String>,
    /// The plugin's external accounts nobody has linked.
    pub unlinked: Vec<String>,
}

/// One advice note, with the kind of rule that gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advice {
    pub rule: &'static str,
    pub note: String,
}

/// The route the concern implies.
pub fn route(ticket: &Ticket) -> Advice {
    let names = |kind: &str| ticket.references.iter().any(|r| r.kind == kind);
    let note = if names("break") {
        "Route: the book's. A break stays with the operations plugin's people; see its \
         reconciliation page."
            .to_string()
    } else if names("instrument") {
        "Route: the deployment admin's. An instrument record is the deployment's own: the \
         Instruments page, or dashboard__complete_instruments through the deployment's MCP."
            .to_string()
    } else if ticket.concerns.kind == "plugin" {
        format!(
            "Route: the firm's. This concerns {}, which the firm runs, so its people work it: a \
             setting, a link or a grant to look at.",
            ticket.concerns.instance
        )
    } else {
        let part = match ticket.concerns.kind.as_str() {
            "platform" => "Open Meridian's platform",
            "chart" => "the chart",
            "cli" => "the meridian command",
            "sdk" => "a plugin SDK",
            "bor" => "core's book of record",
            "street" => "core's street store",
            "instrument" => "core's instrument store",
            "conductor" => "core's conductor",
            _ => "core's dashboard",
        };
        format!(
            "Route: Open Meridian's. This concerns {part}, which Open Meridian maintains. \
             Sending a public issue from this ticket arrives in a later release, and this \
             ticket will be ready to send then; nothing leaves the deployment until a person \
             sends it."
        )
    };
    Advice {
        rule: "route",
        note,
    }
}

/// A likely duplicate, or a recurrence.
pub fn duplicate(ticket: &Ticket, context: &Context) -> Option<Advice> {
    if let Some(earlier) = context.recurrence_of {
        return Some(Advice {
            rule: "duplicate",
            note: format!(
                "A recurrence of {}, which is {}: filed again under the same key. Nothing \
                 reopens by itself; a person decides whether this is the same problem.",
                earlier.ticket_id,
                earlier.state.as_str()
            ),
        });
    }
    context
        .open_seen
        .iter()
        .find(|other| {
            other.ticket_id != ticket.ticket_id && other.fingerprint == ticket.fingerprint
        })
        .map(|other| Advice {
            rule: "duplicate",
            note: format!(
                "Likely a duplicate of {}, open, with the same concerns, version, step, \
                 operation, reason and paths.",
                other.ticket_id
            ),
        })
}

/// The firm's own: what the plugin's report already names.
pub fn firms_own(ticket: &Ticket, context: &Context) -> Option<Advice> {
    if ticket.concerns.kind != "plugin" {
        return None;
    }
    let instance = &ticket.concerns.instance;
    if !context.missing_settings.is_empty() {
        return Some(Advice {
            rule: "firm's own",
            note: format!(
                "This may be the firm's own: {instance} holds no value for the required \
                 setting {}. Its admin sets it on the plugin's Settings, under Manage.",
                context.missing_settings.join(", ")
            ),
        });
    }
    if !context.unlinked.is_empty() {
        return Some(Advice {
            rule: "firm's own",
            note: format!(
                "This may be the firm's own: {instance} reaches an external account nobody \
                 has linked ({}). Its admin links it on the plugin's page, under Manage.",
                context.unlinked.join(", ")
            ),
        });
    }
    None
}

/// A credential's shape in a text the filer wrote.
pub fn credential(text: &str) -> Option<Advice> {
    quarantine::credential_shape(text).map(|shape| Advice {
        rule: "credential",
        note: format!(
            "This text has the shape of {shape}. If it is a real one, treat it as exposed: \
             revoke it where it was issued. A ticket's text cannot be edited, so the filer may \
             close this ticket as withdrawn and file it again without it."
        ),
    })
}

/// Everything the rules advise at filing, in order: the route, a duplicate,
/// the firm's own, a credential's shape.
pub fn at_filing(ticket: &Ticket, context: &Context) -> Vec<Advice> {
    let mut advice = vec![route(ticket)];
    advice.extend(duplicate(ticket, context));
    advice.extend(firms_own(ticket, context));
    advice.extend(credential(&format!("{}\n{}", ticket.title, ticket.seen)));
    advice
}
