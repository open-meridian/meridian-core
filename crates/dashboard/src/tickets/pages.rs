//! The ticket rows over HTTP, and the pages a person works tickets on
//! (W6.21 to W6.24): `POST /tickets` (FileTicket), `GET /tickets`
//! (ListTickets: the Tickets page), `GET /tickets/{ticket_id}` (ReadTicket:
//! the ticket's page), `POST /tickets/{ticket_id}/notes` (AddTicketNote),
//! `GET /tickets/counts` (CountTickets), `POST /tickets/{ticket_id}/work`
//! (WorkTicket), `GET /inbox` (ReadInbox: the Inbox page), `POST
//! /inbox/read` (MarkNoticesRead); and "Report a problem" (`GET
//! /tickets/new`) and the header's count (`GET /inbox/count`).
//!
//! Every row is a person's, in their own session at the dashboard: a
//! request asking for JSON is answered with the row's JSON, any other with
//! the page. A client reaches tickets through `/mcp`'s tools, never here: a
//! bearer token is refused, and above all on WorkTicket, which no delegation
//! takes. Every form, and every JSON body, carries the session's form token,
//! since a plugin's page on a host below this one could otherwise post as
//! the person.
//!
//! **Plain text, always.** A ticket's title, text and notes are escaped,
//! never rendered as HTML or Markdown; a URL in them is shown unlinked, with
//! its host beside it. Each text has its provenance beside it. A suspect
//! text is shown here -- this is where a person reads it -- with the matches
//! marked, and the JSON answers withhold it as tools do.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use meridian_access::{person_access, Access};
use meridian_domain::v1::AccessRecords;
use meridian_pb::v1::{FileTicketRequest, TicketKind, TicketSubject};
use serde_json::{json, Value};

use super::{
    quarantine, visibility, Act, Actor, Author, Filter, Notice, Provenance, Reader, Refused,
    State as TicketState, Ticket,
};
use crate::html::{crumb_here, crumb_link, escape, page_with, Chrome, Report, Viewer};
use crate::session::Session;
use crate::web::App;

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/tickets", get(tickets_page).post(file_posted))
        .route("/tickets/new", get(report_page))
        .route("/tickets/counts", get(counts))
        .route("/tickets/{ticket_id}", get(ticket_page))
        .route("/tickets/{ticket_id}/notes", post(note_posted))
        .route("/tickets/{ticket_id}/work", post(work_posted))
        .route("/inbox", get(inbox_page))
        .route("/inbox/read", post(read_posted))
        .route("/inbox/count", get(inbox_count))
}

/// The parts of core a person may file about, as the form names them.
pub const CORE_PARTS: [(&str, &str); 9] = [
    ("dashboard", "The dashboard"),
    ("bor", "The book of record"),
    ("street", "The street store"),
    ("instrument", "The instrument store"),
    ("conductor", "The conductor"),
    ("chart", "The chart"),
    ("cli", "The meridian command"),
    ("sdk", "A plugin SDK"),
    ("platform", "Open Meridian's platform"),
];

const KINDS: [(TicketKind, &str); 4] = [
    (TicketKind::Defect, "Something did not do what it should"),
    (TicketKind::Discrepancy, "Two records disagree"),
    (
        TicketKind::Request,
        "Something is missing, or should change",
    ),
    (TicketKind::Question, "How do I do something?"),
];

// ── Who is asking ───────────────────────────────────────────────────────

fn wants_json(headers: &HeaderMap) -> bool {
    let says = |name| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("application/json"))
    };
    says(ACCEPT) || says(CONTENT_TYPE)
}

fn json_refused(refused: &Refused) -> Response {
    (
        StatusCode::from_u16(refused.status).unwrap_or(StatusCode::BAD_REQUEST),
        Json(refused.tool_answer()),
    )
        .into_response()
}

fn page_refused(refused: &Refused) -> Response {
    crate::admin::status_page(
        StatusCode::from_u16(refused.status).unwrap_or(StatusCode::BAD_REQUEST),
        if refused.status == 404 {
            "No such ticket"
        } else {
            "Not done"
        },
        &refused.detail,
    )
}

fn refused(headers: &HeaderMap, refused: &Refused) -> Response {
    if wants_json(headers) {
        json_refused(refused)
    } else {
        page_refused(refused)
    }
}

/// The person in their own session, what the records say now, and what they
/// hold; or the answer refusing the request. A bearer token is a client's,
/// which reaches tickets through `/mcp`: refused here.
fn person(
    app: &App,
    headers: &HeaderMap,
    work: bool,
) -> Result<(Session, AccessRecords, Access), Box<Response>> {
    if headers.contains_key(AUTHORIZATION) {
        let said = if work {
            "no delegation works a ticket: assigning, resolving, closing, reopening and \
             releasing are a person's, on the ticket's page"
        } else {
            "a client reaches tickets through the deployment's MCP surface, /mcp, not here"
        };
        return Err(Box::new(
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"outcome": "refused", "reason": "unauthorised", "detail": said})),
            )
                .into_response(),
        ));
    }
    let (session, records) = match crate::admin::gate(app, headers, false) {
        Ok(gated) => gated,
        Err(response) if wants_json(headers) => {
            let status = response.status();
            return Err(Box::new(
                (
                    status,
                    Json(json!({"outcome": "refused", "reason": "unauthorised", "detail": "sign in to the dashboard first"})),
                )
                    .into_response(),
            ));
        }
        Err(response) => return Err(response),
    };
    let access = person_access(&records, &session.subject, &session.directory_groups);
    Ok((session, records, access))
}

fn actor_of(session: &Session, access: &Access) -> Actor {
    Actor {
        author: Author {
            provenance: Provenance::Person,
            subject: session.subject.clone(),
            person: session.display_name.clone(),
            ..Author::default()
        },
        access: access.clone(),
        through_delegation: false,
    }
}

fn reader_of(session: &Session, access: &Access) -> Reader {
    actor_of(session, access).reader()
}

/// A POST's fields: urlencoded from a page's form, or a JSON object, each
/// carrying the session's form token.
async fn posted(
    headers: &HeaderMap,
    body: &Bytes,
    session: &Session,
) -> Result<(Value, bool), Box<Response>> {
    let json = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("application/json"));
    let fields: Value = if json {
        serde_json::from_slice(body)
            .map_err(|_| Box::new(json_refused(&Refused::invalid("", "a JSON object"))))?
    } else {
        let mut object = serde_json::Map::new();
        for (key, value) in form_pairs(body) {
            match object.get_mut(&key) {
                Some(Value::Array(listed)) => listed.push(Value::String(value)),
                Some(one) => {
                    let first = one.take();
                    *one = Value::Array(vec![first, Value::String(value)]);
                }
                None => {
                    object.insert(key, Value::String(value));
                }
            }
        }
        Value::Object(object)
    };
    if fields.get("form_token").and_then(Value::as_str) != Some(session.form_token.as_str()) {
        let refused = Refused::forbidden("form_token", "this form did not come from your session");
        return Err(Box::new(if json {
            json_refused(&refused)
        } else {
            page_refused(&refused)
        }));
    }
    Ok((fields, json))
}

/// `application/x-www-form-urlencoded`, read as a browser writes it.
fn form_pairs(body: &[u8]) -> Vec<(String, String)> {
    fn decoded(said: &str) -> String {
        let bytes = said.as_bytes();
        let hex = |b: u8| (b as char).to_digit(16);
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'+' => out.push(b' '),
                b'%' if i + 2 < bytes.len() => match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    (Some(high), Some(low)) => {
                        out.push((high * 16 + low) as u8);
                        i += 2;
                    }
                    _ => out.push(b'%'),
                },
                byte => out.push(byte),
            }
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }
    String::from_utf8_lossy(body)
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decoded(key), decoded(value))
        })
        .collect()
}

fn field<'a>(fields: &'a Value, name: &str) -> &'a str {
    fields.get(name).and_then(Value::as_str).unwrap_or_default()
}

fn chrome<'a>(session: &'a Session, access: &Access, crumbs: String) -> Chrome<'a> {
    Chrome {
        viewer: Some(Viewer {
            display_name: &session.display_name,
            form_token: &session.form_token,
            admin: access.deployment_admin,
        }),
        crumbs,
        main: "page",
        in_admin: false,
        report: Report::Dashboard,
    }
}

fn token_input(session: &Session) -> String {
    format!(
        "<input type=\"hidden\" name=\"form_token\" value=\"{}\">",
        escape(&session.form_token)
    )
}

// ── Plain text, marked ──────────────────────────────────────────────────

/// A text as a page shows it: escaped, a URL unlinked with its host beside
/// it, and each span `marks` names wrapped in a mark naming its rule.
pub fn shown(text: &str, marks: &[quarantine::Matched]) -> String {
    let chars: Vec<char> = text.chars().collect();
    // Where each URL ends, and its host.
    let mut url_ends: HashMap<usize, String> = HashMap::new();
    let lower: String = chars.iter().collect::<String>().to_lowercase();
    let lower_chars: Vec<char> = lower.chars().collect();
    let mut i = 0;
    while i < lower_chars.len() {
        let rest: String = lower_chars[i..lower_chars.len().min(i + 8)]
            .iter()
            .collect();
        let scheme = if rest.starts_with("https://") {
            8
        } else if rest.starts_with("http://") {
            7
        } else {
            0
        };
        if scheme > 0 && lower_chars.len() == chars.len() {
            let mut end = i + scheme;
            while end < chars.len()
                && !chars[end].is_whitespace()
                && !matches!(chars[end], '"' | '\'' | '<' | '>')
            {
                end += 1;
            }
            let host: String = chars[i + scheme..end]
                .iter()
                .take_while(|c| !matches!(c, '/' | '?' | '#'))
                .collect();
            url_ends.insert(end, host);
            i = end;
        } else {
            i += 1;
        }
    }
    let mut out = String::new();
    let mut open: Option<&quarantine::Matched> = None;
    for (at, c) in chars.iter().enumerate() {
        if let Some(host) = url_ends.get(&at) {
            out.push_str(&format!(
                "<span class=\"url-host\"> (link to {}, not followed)</span>",
                escape(host)
            ));
        }
        if open.is_some_and(|m| m.end == at) {
            out.push_str("</mark>");
            open = None;
        }
        if open.is_none() {
            if let Some(m) = marks.iter().find(|m| m.start == at) {
                out.push_str(&format!(
                    "<mark data-rule=\"{rule}\" title=\"{rule}\">",
                    rule = escape(m.rule)
                ));
                open = Some(m);
            }
        }
        out.push_str(&escape(&c.to_string()));
    }
    if open.is_some() {
        out.push_str("</mark>");
    }
    if let Some(host) = url_ends.get(&chars.len()) {
        out.push_str(&format!(
            "<span class=\"url-host\"> (link to {}, not followed)</span>",
            escape(host)
        ));
    }
    out
}

/// A text shown with its marks where it is held, plainly where it is not.
fn shown_held(text: &str, held: bool) -> String {
    if held {
        shown(text, &quarantine::matched(text))
    } else {
        shown(text, &[])
    }
}

fn author_badge(author: &Author) -> String {
    let said = match author.provenance {
        Provenance::Person => "at the dashboard",
        Provenance::Client => "through a client",
        Provenance::Plugin => "a plugin, for a person",
        Provenance::Rules => "the dashboard's rules",
    };
    format!(
        "<span class=\"author\">{}</span> <span class=\"badge\" data-provenance=\"{}\">{}</span>",
        escape(&author.said()),
        author.provenance.as_str(),
        said
    )
}

fn state_badge(state: TicketState) -> String {
    let tone = match state {
        TicketState::Open => "accent",
        TicketState::Resolved => "good",
        TicketState::Closed => "",
    };
    format!(
        "<span class=\"badge {tone}\" data-state=\"{s}\">{s}</span>",
        s = state.as_str()
    )
}

fn concerns_said(ticket: &Ticket) -> String {
    let c = &ticket.concerns;
    if c.kind == "plugin" {
        let name = if c.plugin.is_empty() {
            c.instance.clone()
        } else {
            format!("{} ({})", c.plugin, c.instance)
        };
        if c.version.is_empty() {
            name
        } else {
            format!("{name} at {}", c.version)
        }
    } else {
        CORE_PARTS
            .iter()
            .find(|(k, _)| *k == c.kind)
            .map(|(_, said)| said.to_string())
            .unwrap_or_else(|| c.kind.clone())
    }
}

// ── The Tickets page and ListTickets ────────────────────────────────────

async fn tickets_page(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let (session, _, access) = match person(&app, &headers, false) {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let asked = |name: &str| query.get(name).map(String::as_str).unwrap_or_default();
    let filter = match Filter::parse(
        asked("concerns"),
        asked("state"),
        matches!(asked("mine"), "1" | "true" | "on"),
        matches!(asked("suspect"), "1" | "true" | "on"),
    ) {
        Ok(filter) => filter,
        Err(why) => return refused(&headers, &why),
    };
    let reader = reader_of(&session, &access);
    let listed = match super::list(&app, &reader, &filter).await {
        Ok(listed) => listed,
        Err(why) => return refused(&headers, &why),
    };
    if wants_json(&headers) {
        return Json(json!({
            "tickets": listed.iter().map(|t| super::ticket_json(t, true, false)).collect::<Vec<_>>(),
        }))
        .into_response();
    }
    let all = super::list(&app, &reader, &Filter::default())
        .await
        .unwrap_or_default();
    let mut concerned: Vec<String> = all.iter().map(|t| t.concerns.named().to_string()).collect();
    concerned.sort();
    concerned.dedup();
    let open = all.iter().filter(|t| t.state == TicketState::Open).count();
    let held = all.iter().filter(|t| t.suspect).count();
    let options = |chosen: &str, values: &[(String, String)]| -> String {
        values
            .iter()
            .map(|(value, said)| {
                format!(
                    "<option value=\"{v}\"{s}>{t}</option>",
                    v = escape(value),
                    s = if value == chosen { " selected" } else { "" },
                    t = escape(said)
                )
            })
            .collect()
    };
    let mut concern_options = vec![(String::new(), "Anything".to_string())];
    concern_options.extend(concerned.iter().map(|c| (c.clone(), c.clone())));
    let state_options: Vec<(String, String)> = [
        ("", "Any state"),
        ("open", "Open"),
        ("resolved", "Resolved"),
        ("closed", "Closed"),
    ]
    .iter()
    .map(|(v, s)| (v.to_string(), s.to_string()))
    .collect();
    let rows: String = listed
        .iter()
        .map(|t| {
            format!(
                "<tr data-id=\"{id}\" data-state=\"{state}\"><td class=\"name\"><a href=\"/tickets/{id}\">{title}</a>\
                 <span class=\"hint\"><code>{id}</code></span>{held}</td><td>{concerns}</td><td>{kind}</td>\
                 <td>{badge}</td><td>{by}</td><td>{seen}</td></tr>",
                id = escape(&t.ticket_id),
                state = t.state.as_str(),
                title = escape(&t.title),
                held = if t.suspect {
                    " <span class=\"badge warn\" data-held>held from tools</span>"
                } else {
                    ""
                },
                concerns = escape(&concerns_said(t)),
                kind = super::kind_name(t.kind),
                badge = state_badge(t.state),
                by = author_badge(&t.filed_by),
                seen = t.seen_count,
            )
        })
        .collect();
    let table = if listed.is_empty() {
        "<div class=\"panel empty-state\"><strong>No tickets</strong>\
         <p>None you may see matches. A ticket is seen by the people who could act on what it \
         concerns.</p></div>"
            .to_string()
    } else {
        format!(
            "<div class=\"scroll\"><table class=\"list\" id=\"tickets-table\"><thead><tr><th>Ticket</th>\
             <th>Concerns</th><th>Kind</th><th>State</th><th>Filed by</th><th>Seen</th></tr></thead>\
             <tbody>{rows}</tbody></table></div>"
        )
    };
    let body = format!(
        "<div class=\"tickets\"><div class=\"page-head\"><div><h1>Tickets</h1>\
         <p>Problems seen in this deployment that you may see: {count} in all, {open} open, {held} held \
         from tools. Advice changes nothing; only a person, on a ticket's page, works it.</p></div>\
         <div class=\"actions\"><a class=\"button primary\" href=\"/tickets/new\">Report a problem</a></div></div>\
         <form class=\"filter-row ticket-filter\" method=\"get\" action=\"/tickets\">\
         <label>Concerns<select name=\"concerns\">{concern_options}</select></label>\
         <label>State<select name=\"state\">{state_options}</select></label>\
         <label><input type=\"checkbox\" name=\"mine\" value=\"1\"{mine}> Mine</label>\
         <label><input type=\"checkbox\" name=\"suspect\" value=\"1\"{suspect}> Held</label>\
         <button type=\"submit\">Show</button></form>{table}</div>",
        count = all.len(),
        concern_options = options(asked("concerns"), &concern_options),
        state_options = options(asked("state"), &state_options),
        mine = if filter.mine { " checked" } else { "" },
        suspect = if filter.suspect { " checked" } else { "" },
    );
    Html(page_with(
        "Tickets",
        &body,
        &chrome(
            &session,
            &access,
            crumb_link("/", "Home") + &crumb_here("Tickets", None),
        ),
    ))
    .into_response()
}

// ── Report a problem, and FileTicket ────────────────────────────────────

/// What "Report a problem" fills in from where the person was.
#[derive(Debug, Default, Clone)]
pub struct Prefill {
    pub concerns: String,
    pub instance: String,
    pub operation: String,
    pub reason: String,
    pub paths: Vec<String>,
    /// The page's last refusal, in its words (Q12): on the dashboard's own
    /// pages only.
    pub seen: String,
}

/// The form itself, for its own page and inline beside a refusal.
pub fn report_form(session: &Session, access: &Access, prefill: &Prefill) -> String {
    let concerns = if !prefill.instance.is_empty() {
        format!(
            "<input type=\"hidden\" name=\"concerns\" value=\"plugin\">\
             <input type=\"hidden\" name=\"instance\" value=\"{i}\">\
             <p class=\"hint\">About the plugin <code>{i}</code>, at the version it runs.</p>",
            i = escape(&prefill.instance)
        )
    } else {
        let mut options: String = CORE_PARTS
            .iter()
            .map(|(value, said)| {
                format!(
                    "<option value=\"{value}\"{s}>{said}</option>",
                    s = if *value == prefill.concerns {
                        " selected"
                    } else {
                        ""
                    }
                )
            })
            .collect();
        for instance in access
            .plugins
            .iter()
            .filter(|(_, held)| held.holds_any())
            .map(|(i, _)| i)
        {
            options.push_str(&format!(
                "<option value=\"plugin:{i}\">The plugin {i}</option>",
                i = escape(instance)
            ));
        }
        format!("<label>It concerns<select name=\"concerns\">{options}</select></label>")
    };
    let kinds: String = KINDS
        .iter()
        .map(|(kind, said)| {
            format!(
                "<option value=\"{}\">{said}</option>",
                super::kind_name(*kind as i32)
            )
        })
        .collect();
    let mut hidden = String::new();
    for (name, value) in [
        ("operation", &prefill.operation),
        ("reason", &prefill.reason),
    ] {
        if !value.is_empty() {
            hidden.push_str(&format!(
                "<input type=\"hidden\" name=\"{name}\" value=\"{}\">",
                escape(value)
            ));
        }
    }
    for path in &prefill.paths {
        hidden.push_str(&format!(
            "<input type=\"hidden\" name=\"paths\" value=\"{}\">",
            escape(path)
        ));
    }
    format!(
        "<form class=\"report\" method=\"post\" action=\"/tickets\">{token}{hidden}{concerns}\
         <label>What is wrong, in a line<input name=\"title\" maxlength=\"120\" required></label>\
         <label>What you saw<textarea name=\"seen\" rows=\"6\" maxlength=\"8000\">{seen}</textarea></label>\
         <p class=\"hint\">Plain text. An account named by its identifier here narrows who sees \
         the ticket to those who may read it; an account named only in words does not. Nothing \
         you write leaves the deployment.</p>\
         <label>Kind<select name=\"kind\">{kinds}</select></label>\
         <div class=\"form-foot\"><button type=\"submit\" class=\"primary\">File the ticket</button></div></form>",
        token = token_input(session),
        seen = escape(&prefill.seen),
    )
}

async fn report_page(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let (session, _, access) = match person(&app, &headers, false) {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let asked = |name: &str| query.get(name).cloned().unwrap_or_default();
    let instance = asked("instance");
    let prefill = Prefill {
        concerns: asked("concerns"),
        instance: if crate::plugins::is_instance(&instance) && access.held(&instance).holds_any() {
            instance
        } else {
            String::new()
        },
        operation: asked("operation"),
        reason: asked("reason"),
        paths: asked("paths")
            .split(',')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect(),
        seen: String::new(),
    };
    let body = format!(
        "<div class=\"report-page\"><h1>Report a problem</h1>\
         <p>It is seen by the people who could act on what it concerns, and the dashboard adds \
         advice at once; nothing changes until a person acts on its page.</p>{}</div>",
        report_form(&session, &access, &prefill)
    );
    let mut chrome = chrome(
        &session,
        &access,
        crumb_link("/", "Home")
            + &crumb_link("/tickets", "Tickets")
            + &crumb_here("Report a problem", None),
    );
    chrome.main = "sheet";
    Html(page_with("Report a problem", &body, &chrome)).into_response()
}

async fn file_posted(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    let (session, _, access) = match person(&app, &headers, false) {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let (fields, json) = match posted(&headers, &body, &session).await {
        Ok(posted) => posted,
        Err(response) => return *response,
    };
    let filing = if json {
        super::filing_from_json(&fields, &["form_token"])
    } else {
        filing_from_form(&fields)
    };
    let filing = match filing {
        Ok(filing) => filing,
        Err(why) => return refused(&headers, &why),
    };
    match super::file(&app, &actor_of(&session, &access), filing).await {
        Ok(filed) if json => Json(json!({
            "ticket_id": filed.ticket_id,
            "outcome": filed.outcome,
            "seen_count": filed.seen_count,
        }))
        .into_response(),
        Ok(filed) => crate::web::redirect(&format!("/tickets/{}?filed=1", filed.ticket_id)),
        Err(why) => refused(&headers, &why),
    }
}

/// A page's form: what it concerns as `plugin:INSTANCE`, a part of core, or
/// `plugin` beside an `instance` field.
fn filing_from_form(fields: &Value) -> Result<FileTicketRequest, Refused> {
    let concerns = field(fields, "concerns");
    let (kind, instance) = match concerns.split_once(':') {
        Some(("plugin", instance)) => ("plugin".to_string(), instance.to_string()),
        _ if concerns == "plugin" => ("plugin".to_string(), field(fields, "instance").to_string()),
        _ => (concerns.to_string(), String::new()),
    };
    let paths = match fields.get("paths") {
        Some(Value::String(one)) if !one.is_empty() => vec![one.clone()],
        Some(Value::Array(listed)) => listed
            .iter()
            .filter_map(|p| p.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    Ok(FileTicketRequest {
        title: field(fields, "title").to_string(),
        seen: field(fields, "seen").to_string(),
        kind: super::parse_kind(field(fields, "kind"))
            .ok_or_else(|| Refused::invalid("kind", "defect, discrepancy, request or question"))?,
        concerns: Some(TicketSubject {
            kind,
            instance,
            version: String::new(),
        }),
        step: field(fields, "step").to_string(),
        operation: field(fields, "operation").to_string(),
        reason: field(fields, "reason").to_string(),
        paths,
        references: Vec::new(),
        idempotency_key: String::new(),
    })
}

// ── The ticket's page and ReadTicket ────────────────────────────────────

async fn ticket_page(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(ticket_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let (session, records, access) = match person(&app, &headers, false) {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let reader = reader_of(&session, &access);
    let ticket = match super::read(&app, &reader, &ticket_id).await {
        Ok(ticket) => ticket,
        Err(why) => return refused(&headers, &why),
    };
    if wants_json(&headers) {
        return Json(super::ticket_json(&ticket, true, true)).into_response();
    }
    let done = query.get("done").map(String::as_str).unwrap_or_default();
    let filed = query.contains_key("filed");
    let body = ticket_body(&session, &records, &access, &ticket, done, filed);
    Html(page_with(
        &ticket.ticket_id,
        &body,
        &chrome(
            &session,
            &access,
            crumb_link("/", "Home")
                + &crumb_link("/tickets", "Tickets")
                + &crumb_here(&ticket.ticket_id, None),
        ),
    ))
    .into_response()
}

fn ticket_body(
    session: &Session,
    records: &AccessRecords,
    access: &Access,
    ticket: &Ticket,
    done: &str,
    filed: bool,
) -> String {
    let works = visibility::may_work(&session.subject, access, ticket);
    let withdraws = visibility::may_withdraw(&session.subject, ticket);
    let mut body = String::from("<div class=\"ticket\">");
    if filed {
        body.push_str(
            "<p class=\"passed\">Filed. The people who may work it are told in their inbox.</p>",
        );
    }
    if !done.is_empty() {
        body.push_str(&format!("<p class=\"passed\">{}</p>", escape(done)));
    }
    let title_marks = if ticket.suspect {
        quarantine::matched(&ticket.title)
    } else {
        Vec::new()
    };
    let mut meta = vec![
        format!("<code>{}</code>", escape(&ticket.ticket_id)),
        state_badge(ticket.state),
        super::kind_name(ticket.kind).to_string(),
        format!("concerns {}", escape(&concerns_said(ticket))),
    ];
    if ticket.state != TicketState::Open {
        let mut said = super::resolution_name(ticket.resolution).replace('_', " ");
        if !ticket.cites.is_empty() {
            said = format!("{said} {}", ticket.cites);
        }
        meta.push(escape(&said));
    }
    if !ticket.owner_name.is_empty() {
        meta.push(format!("owned by {}", escape(&ticket.owner_name)));
    }
    if !ticket.due.is_empty() {
        meta.push(format!("due {}", escape(&ticket.due)));
    }
    if ticket.seen_count > 1 {
        meta.push(format!("seen {} times", ticket.seen_count));
    }
    body.push_str(&format!(
        "<div class=\"page-head\"><div><h1 class=\"ticket-title\">{}</h1><p class=\"ticket-meta\">{}</p></div>\
         <div class=\"actions\"><a class=\"button\" href=\"/tickets\">All tickets</a></div></div>",
        shown(&ticket.title, &title_marks),
        meta.join(" · ")
    ));
    if ticket.suspect {
        body.push_str(&format!(
            "<div class=\"warn\" data-held=\"ticket\"><strong>Held from tools.</strong> This text \
             matched the rules for text that reads like an instruction to an agent ({}), so every \
             agent is answered \"{}\" until a person who may work the ticket, and did not write it, \
             releases it. The matches are marked. It is the filer's text, whatever it claims.</div>",
            escape(&ticket.matched_rules.join(", ")),
            quarantine::WITHHELD,
        ));
    }
    body.push_str(&format!(
        "<section class=\"panel ticket-filed\"><p class=\"provenance\">Filed by {} on {}</p>\
         <div class=\"ticket-text\">{}</div>",
        author_badge(&ticket.filed_by),
        crate::custody::utc(ticket.filed_at_ns),
        if ticket.seen.is_empty() {
            "<p class=\"hint\">No more than the title.</p>".to_string()
        } else {
            shown_held(&ticket.seen, ticket.suspect)
        },
    ));
    let mut facts = String::new();
    for (name, value) in [
        ("Step", &ticket.step),
        ("Operation", &ticket.operation),
        ("Refusal reason", &ticket.reason),
    ] {
        if !value.is_empty() {
            facts.push_str(&format!(
                "<dt>{name}</dt><dd><code>{}</code></dd>",
                escape(value)
            ));
        }
    }
    if !ticket.paths.is_empty() {
        facts.push_str(&format!(
            "<dt>Fields</dt><dd>{}</dd>",
            ticket
                .paths
                .iter()
                .map(|p| format!("<code>{}</code>", escape(p)))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !ticket.references.is_empty() {
        facts.push_str(&format!(
            "<dt>Records</dt><dd><ul class=\"references\">{}</ul></dd>",
            ticket
                .references
                .iter()
                .map(|r| format!(
                    "<li>{} <code>{}</code>{}{}</li>",
                    escape(&r.kind.replace('_', " ")),
                    escape(&r.value),
                    if !r.account_id.is_empty() && r.account_id != r.value {
                        format!(" on <code>{}</code>", escape(&r.account_id))
                    } else {
                        String::new()
                    },
                    if r.found {
                        " <span class=\"hint\">(found in the text)</span>"
                    } else {
                        ""
                    }
                ))
                .collect::<String>()
        ));
    }
    if !facts.is_empty() {
        body.push_str(&format!("<dl class=\"ticket-facts\">{facts}</dl>"));
    }
    body.push_str("</section>");

    // The notes, oldest first, each beside its author.
    let notes: String = ticket
        .notes
        .iter()
        .map(|note| {
            let release = if note.suspect && works && note.author.subject != session.subject {
                format!(
                    "<form method=\"post\" action=\"/tickets/{id}/work\" class=\"inline\">{token}\
                     <input type=\"hidden\" name=\"act\" value=\"release\">\
                     <input type=\"hidden\" name=\"release\" value=\"{n}\">\
                     <input type=\"hidden\" name=\"against_notes\" value=\"{count}\">\
                     <button type=\"submit\">Release</button></form>",
                    id = escape(&ticket.ticket_id),
                    token = token_input(session),
                    n = note.number,
                    count = ticket.notes.len(),
                )
            } else {
                String::new()
            };
            format!(
                "<li data-number=\"{n}\" data-kind=\"{kind}\"{held}><p class=\"provenance\"><span class=\"badge\">{kind}</span> \
                 {n}. {author}, {when}{held_badge}</p><div class=\"ticket-text\">{text}</div>{release}</li>",
                n = note.number,
                kind = super::note_kind_name(note.kind),
                held = if note.suspect { " data-held" } else { "" },
                author = author_badge(&note.author),
                when = crate::custody::utc(note.noted_ns),
                held_badge = if note.suspect {
                    format!(
                        " <span class=\"badge warn\">held from tools: {}</span>",
                        escape(&note.matched_rules.join(", "))
                    )
                } else {
                    String::new()
                },
                text = shown_held(&note.note, note.suspect),
            )
        })
        .collect();
    body.push_str(&format!(
        "<section class=\"panel\"><h2>Notes and advice</h2><ol class=\"notes\">{notes}</ol>\
         <form method=\"post\" action=\"/tickets/{id}/notes\" class=\"note-form\">{token}\
         <input type=\"hidden\" name=\"kind\" value=\"note\">\
         <label>Add a note<textarea name=\"note\" rows=\"3\" maxlength=\"4000\" required></textarea></label>\
         <button type=\"submit\">Add the note</button></form></section>",
        id = escape(&ticket.ticket_id),
        token = token_input(session),
    ));

    // The acts, for a person who may take them.
    if works || withdraws {
        body.push_str(&acts(session, records, access, ticket, works));
    } else {
        body.push_str(
            "<p class=\"hint\">You may read this ticket and add a note. It is worked by a person \
             holding write on what it concerns and every account it names, or, naming none, by \
             its admins.</p>",
        );
    }
    body.push_str("</div>");
    body
}

fn acts(
    session: &Session,
    records: &AccessRecords,
    access: &Access,
    ticket: &Ticket,
    works: bool,
) -> String {
    let id = escape(&ticket.ticket_id);
    let token = token_input(session);
    let against = format!(
        "<input type=\"hidden\" name=\"against_notes\" value=\"{}\">",
        ticket.notes.len()
    );
    let form = |act: &str, inner: &str, button: &str| {
        format!(
            "<form method=\"post\" action=\"/tickets/{id}/work\" class=\"act\" data-act=\"{act}\">{token}{against}\
             <input type=\"hidden\" name=\"act\" value=\"{act}\">{inner}<button type=\"submit\">{button}</button></form>"
        )
    };
    let mut forms = String::new();
    if works {
        // An owner is someone who may see it.
        let people: String = records
            .people
            .iter()
            .filter(|p| {
                visibility::may_see(
                    &Reader {
                        subject: p.subject.clone(),
                        access: super::access_of(records, &p.subject),
                        through_delegation: false,
                    },
                    ticket,
                )
            })
            .map(|p| {
                format!(
                    "<option value=\"{s}\"{chosen}>{n}</option>",
                    s = escape(&p.subject),
                    chosen = if p.subject == ticket.owner {
                        " selected"
                    } else {
                        ""
                    },
                    n = escape(if p.display_name.is_empty() {
                        &p.subject
                    } else {
                        &p.display_name
                    })
                )
            })
            .collect();
        forms.push_str(&form(
            "assign",
            &format!("<label>Owner<select name=\"owner\"><option value=\"\">Nobody</option>{people}</select></label>"),
            "Assign",
        ));
        forms.push_str(&form(
            "due",
            &format!(
                "<label>Due<input type=\"date\" name=\"due\" value=\"{}\"></label>",
                escape(&ticket.due)
            ),
            "Set the due date",
        ));
        if ticket.state == TicketState::Open {
            forms.push_str(&form(
                "resolve",
                "<label>Resolved by<select name=\"resolution\"><option value=\"note\">A note, by its number</option>\
                 <option value=\"version\">A version</option></select></label>\
                 <label>Which<input name=\"cites\" required placeholder=\"the note's number, or the version\"></label>",
                "Resolve",
            ));
        } else {
            forms.push_str(&form("reopen", "", "Reopen"));
        }
        if ticket.suspect && ticket.filed_by.subject != session.subject {
            forms.push_str(&form(
                "release",
                "<input type=\"hidden\" name=\"release\" value=\"ticket\">\
                 <p class=\"hint\">Release the ticket's text to agents, having read it.</p>",
                "Release",
            ));
        }
    }
    if ticket.state == TicketState::Open {
        let reasons = if works {
            "<option value=\"not_a_problem\">Not a problem</option>\
             <option value=\"duplicate\">A duplicate of another ticket</option>\
             <option value=\"withdrawn\">Withdrawn</option>"
        } else {
            "<option value=\"withdrawn\">Withdrawn</option>"
        };
        forms.push_str(&form(
            "close",
            &format!(
                "<label>Closed as<select name=\"resolution\">{reasons}</select></label>\
                 <label>The other ticket, for a duplicate<input name=\"cites\" placeholder=\"TKT-...\"></label>"
            ),
            "Close",
        ));
    }
    let _ = access;
    format!(
        "<section class=\"panel ticket-acts\"><h2>Work it</h2>\
         <p class=\"hint\">Each act is recorded as a note with your name. No agent can take one.</p>\
         <div class=\"acts\">{forms}</div></section>"
    )
}

// ── AddTicketNote and WorkTicket ────────────────────────────────────────

async fn note_posted(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(ticket_id): Path<String>,
    body: Bytes,
) -> Response {
    let (session, _, access) = match person(&app, &headers, false) {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let (fields, json) = match posted(&headers, &body, &session).await {
        Ok(posted) => posted,
        Err(response) => return *response,
    };
    let kind = match field(&fields, "kind") {
        "" => "note",
        said => said,
    };
    match super::add_note(
        &app,
        &actor_of(&session, &access),
        &ticket_id,
        kind,
        field(&fields, "note"),
    )
    .await
    {
        Ok(number) if json => Json(json!({"outcome": "made", "number": number})).into_response(),
        Ok(_) => crate::web::redirect(&format!("/tickets/{ticket_id}?done=Noted.")),
        Err(why) => refused(&headers, &why),
    }
}

async fn work_posted(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(ticket_id): Path<String>,
    body: Bytes,
) -> Response {
    let (session, _, access) = match person(&app, &headers, true) {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let (fields, json) = match posted(&headers, &body, &session).await {
        Ok(posted) => posted,
        Err(response) => return *response,
    };
    let act = Act {
        ticket_id: ticket_id.clone(),
        act: field(&fields, "act").to_string(),
        owner: field(&fields, "owner").to_string(),
        due: field(&fields, "due").to_string(),
        resolution: field(&fields, "resolution").to_string(),
        cites: field(&fields, "cites").to_string(),
        release: field(&fields, "release").to_string(),
        against_notes: field(&fields, "against_notes").parse().ok(),
    };
    match super::work(&app, &session.subject, &session.display_name, &access, &act).await {
        Ok(ticket) if json => Json(json!({"state": ticket.state.as_str()})).into_response(),
        Ok(ticket) => {
            let said = ticket
                .notes
                .iter()
                .rev()
                .find(|n| n.kind == meridian_pb::v1::TicketNoteKind::Change as i32)
                .map(|n| n.note.clone())
                .unwrap_or_default();
            crate::web::redirect(&format!(
                "/tickets/{ticket_id}?done={}",
                crate::admin::query_text(&said)
            ))
        }
        Err(why) => refused(&headers, &why),
    }
}

// ── CountTickets ────────────────────────────────────────────────────────

async fn counts(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let (session, _, access) = match person(&app, &headers, false) {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let by: Vec<String> = query
        .get("by")
        .map(|b| {
            b.split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    match super::count(&app, &reader_of(&session, &access), &by).await {
        Ok(counts) => Json(counts).into_response(),
        Err(why) => json_refused(&why),
    }
}

// ── The Inbox, ReadInbox and MarkNoticesRead ────────────────────────────

fn notice_said(notice: &Notice) -> &'static str {
    match notice.kind.as_str() {
        "filed" => "Filed",
        "noted" => "A note",
        "advised" => "Advice",
        "assigned" => "Assigned",
        "due" => "Due date",
        "resolved" => "Resolved",
        "closed" => "Closed",
        "reopened" => "Reopened",
        "released" => "Released",
        _ => "Changed",
    }
}

async fn inbox_page(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let (session, _, access) = match person(&app, &headers, false) {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let reader = reader_of(&session, &access);
    if wants_json(&headers) {
        return match super::read_inbox(&app, &reader, "").await {
            Ok(notices) => Json(json!({
                "notices": notices.iter().map(|(n, t)| super::notice_json(n, Some(t), true)).collect::<Vec<_>>(),
            }))
            .into_response(),
            Err(why) => json_refused(&why),
        };
    }
    let notices = match super::latest(&app, &reader).await {
        Ok(notices) => notices,
        Err(why) => return page_refused(&why),
    };
    let unread: Vec<String> = notices
        .iter()
        .filter(|(n, _)| !n.read)
        .map(|(n, _)| n.ticket_id.clone())
        .collect();
    let rows: String = notices
        .iter()
        .map(|(notice, ticket)| {
            format!(
                "<li data-kind=\"{kind}\" data-ticket=\"{id}\"{unread}><span class=\"badge{tone}\">{said}</span> \
                 <a href=\"/tickets/{id}\">{title}</a> <span class=\"hint\"><code>{id}</code></span>\
                 <p class=\"provenance\">{author}, {when}</p></li>",
                kind = escape(&notice.kind),
                id = escape(&notice.ticket_id),
                unread = if notice.read { "" } else { " data-unread" },
                tone = if notice.read { "" } else { " accent" },
                said = notice_said(notice),
                title = escape(&ticket.title),
                author = author_badge(&notice.author),
                when = crate::custody::utc(notice.changed_ns),
            )
        })
        .collect();
    let mark_all = if unread.is_empty() {
        String::new()
    } else {
        let mut ids = unread.clone();
        ids.sort();
        ids.dedup();
        format!(
            "<form method=\"post\" action=\"/inbox/read\">{}{}<button type=\"submit\">Mark all read</button></form>",
            token_input(&session),
            ids.iter()
                .map(|id| format!("<input type=\"hidden\" name=\"ticket_ids\" value=\"{}\">", escape(id)))
                .collect::<String>()
        )
    };
    let body = format!(
        "<div class=\"inbox\"><div class=\"page-head\"><div><h1>Inbox</h1>\
         <p>What changed on the tickets you filed, own, noted or may work, for 90 days. A notice \
         names a ticket and never quotes it.</p></div><div class=\"actions\">{mark_all}</div></div>{list}</div>",
        list = if notices.is_empty() {
            "<div class=\"panel empty-state\"><strong>Nothing yet</strong><p>When a ticket you may \
             act on changes, it is listed here.</p></div>"
                .to_string()
        } else {
            format!("<ol class=\"notices\">{rows}</ol>")
        }
    );
    Html(page_with(
        "Inbox",
        &body,
        &chrome(
            &session,
            &access,
            crumb_link("/", "Home") + &crumb_here("Inbox", None),
        ),
    ))
    .into_response()
}

async fn read_posted(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    let (session, _, _) = match person(&app, &headers, false) {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let (fields, json) = match posted(&headers, &body, &session).await {
        Ok(posted) => posted,
        Err(response) => return *response,
    };
    let ids: Vec<String> = match fields.get("ticket_ids") {
        Some(Value::String(one)) => vec![one.clone()],
        Some(Value::Array(listed)) => listed
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    match super::mark_read(&app, &session.subject, &ids).await {
        Ok(marked) if json => Json(json!({"outcome": "made", "marked": marked})).into_response(),
        Ok(_) => crate::web::redirect("/inbox"),
        Err(why) => refused(&headers, &why),
    }
}

/// The header's count, polled every 30 seconds while a page is open
/// (requirement 36): no stream.
async fn inbox_count(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let (session, _, access) = match person(&app, &headers, false) {
        Ok(found) => found,
        Err(response) => return *response,
    };
    match app.tickets.unread(&reader_of(&session, &access)).await {
        Ok(unread) => (
            [(axum::http::header::CACHE_CONTROL, "no-store")],
            Json(json!({"unread": unread})),
        )
            .into_response(),
        Err(why) => json_refused(&why),
    }
}

#[cfg(test)]
mod tests;
