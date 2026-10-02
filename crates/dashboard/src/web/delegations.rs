//! W6.14 and W6.17 in a browser: a person's Connected clients, and a
//! deployment admin's view of anybody's (spec/clients-act-on-a-persons-delegation,
//! requirements 16 and 17).
//!
//! Browser pages: they read the session's cookie and no bearer token, and
//! every form carries the session's form token. Revoking ends a client's
//! access at its next request; a person's browser sessions are untouched.
//! Nothing goes to the conductor: delegations are this dashboard's, and so is
//! ending them.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Form, Path, Query, State};
use axum::http::header::LOCATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use meridian_domain::v1::AccessRecords;

use super::{redirect, refused, session_of, App};
use crate::admin::{admin_chrome, form_token_matches, gate, status_page, token_input, Fields};
use crate::delegation::Delegation;
use crate::html::{crumb_here, crumb_link, escape, page_with, Chrome, Viewer};
use crate::terminal::rfc3339;

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/delegations", get(own))
        .route("/delegations/{id}/revoke", post(revoke_own))
        .route("/admin/people/{subject}/delegations", get(persons))
        .route(
            "/admin/people/{subject}/delegations/revoke",
            post(revoke_persons),
        )
}

/// Account group names, for saying what a delegation covers.
fn group_names(records: &AccessRecords) -> BTreeMap<String, String> {
    records
        .account_groups
        .iter()
        .map(|g| (g.account_group_id.clone(), g.name.clone()))
        .collect()
}

fn day(at_ns: i64) -> String {
    rfc3339(at_ns)[..10].to_string()
}

fn minute(at_ns: i64) -> String {
    let at = rfc3339(at_ns);
    format!("{} {} UTC", &at[..10], &at[11..16])
}

/// Live, revoked or lapsed, in a word and why.
fn standing(delegation: &Delegation, now: i64) -> (&'static str, String) {
    match &delegation.revoked {
        Some(revoked) => (
            "revoked",
            format!(
                "Revoked {} by {}: {}",
                minute(revoked.at_ns),
                match revoked.by.as_str() {
                    "client" => "the client",
                    "dashboard" => "this deployment",
                    by => by,
                },
                revoked.why
            ),
        ),
        None if now > delegation.expires_at_ns => (
            "lapsed",
            format!("Lapsed on {}", day(delegation.expires_at_ns)),
        ),
        None => ("live", super::oauth::lapses(delegation, now)),
    }
}

/// The table both pages draw. `revoke` is where a live one's form posts,
/// given its id, or none for no form.
fn table(
    delegations: &[Delegation],
    names: &BTreeMap<String, String>,
    now: i64,
    token: &str,
    revoke: &dyn Fn(&Delegation) -> Option<(String, String)>,
) -> String {
    if delegations.is_empty() {
        return "<p class=\"empty\">No client acts on a delegation here.</p>".into();
    }
    let rows: String = delegations
        .iter()
        .map(|d| {
            let (state, said) = standing(d, now);
            let used = d
                .last_used_at_ns
                .map(minute)
                .unwrap_or_else(|| "never".into());
            let refusal = d
                .last_refusal
                .as_ref()
                .map(|(at, why)| format!("{}: {}", minute(*at), escape(why)))
                .unwrap_or_else(|| "none".into());
            let action = match (state, revoke(d)) {
                ("live", Some((action, field))) => format!(
                    "<form method=\"post\" action=\"{action}\" data-confirm=\"Revoke {name}? \
                     It stops at its next request.\">{token}{field}\
                     <button type=\"submit\">Revoke</button></form>",
                    action = escape(&action),
                    name = escape(&d.client_name),
                ),
                _ => String::new(),
            };
            format!(
                "<tr data-id=\"{id}\" data-state=\"{state}\"><td><span class=\"name\">{client}</span></td>\
                 <td>{covers}</td><td>{made}</td><td>{said}</td><td>{used}</td><td>{refusal}</td>\
                 <td class=\"actions\">{action}</td></tr>",
                id = escape(&d.id),
                client = escape(&d.client_name),
                covers = escape(&d.covers.said(names)),
                made = day(d.made_at_ns),
                said = escape(&said),
            )
        })
        .collect();
    format!(
        "<table class=\"delegations\"><thead><tr><th>Client</th><th>Covers</th><th>Made</th>\
         <th>Until</th><th>Last used</th><th>Last refused</th><th></th></tr></thead>\
         <tbody>{rows}</tbody></table>"
    )
}

/// GET /delegations: ListOwnDelegations, the person's Connected clients.
async fn own(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(query): Query<Fields>,
) -> Response {
    let now = app.clock.now_ns();
    let records = match app.records.current(now) {
        Ok(records) => records,
        Err(stale) => return refused(&stale.to_string()),
    };
    let Some(session) = session_of(&app, &headers) else {
        return redirect("/sign-in");
    };
    let theirs = match app.delegations.of_person(&session.subject, now).await {
        Ok(theirs) => theirs,
        Err(failed) => return refused(&failed.to_string()),
    };
    let admin =
        meridian_access::person_access(&records, &session.subject, &session.directory_groups)
            .deployment_admin;
    let token = token_input(&session);
    let notice = match query.get("revoked").map(String::as_str) {
        Some("1") => "<p class=\"passed\">Revoked. That client stops at its next request.</p>",
        _ => "",
    };
    let body = format!(
        "<div class=\"page-head\"><div><h1>Connected clients</h1>\
         <p>Clients acting as you on this deployment: the <code>meridian</code> command on each \
         computer you connected, and anything else you allowed. What each does is recorded as \
         yours, through it.</p></div></div>{notice}{table}",
        table = table(&theirs, &group_names(&records), now, &token, &|d| Some((
            format!("/delegations/{}/revoke", d.id),
            String::new()
        ))),
    );
    Html(page_with(
        "Connected clients",
        &body,
        &Chrome {
            viewer: Some(Viewer {
                display_name: &session.display_name,
                form_token: &session.form_token,
                admin,
            }),
            crumbs: crumb_here("Connected clients", None),
            main: "page",
            ..Default::default()
        },
    ))
    .into_response()
}

/// POST /delegations/{id}/revoke: RevokeOwnDelegation. Only a delegation of
/// the person's own.
async fn revoke_own(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(fields): Form<Fields>,
) -> Response {
    let now = app.clock.now_ns();
    let Some(session) = session_of(&app, &headers) else {
        return redirect("/sign-in");
    };
    if let Err(response) = form_token_matches(&session, &fields) {
        return *response;
    }
    let theirs = match app.delegations.delegation(&id).await {
        Ok(found) => found.filter(|d| d.subject == session.subject),
        Err(failed) => return refused(&failed.to_string()),
    };
    if theirs.is_none() {
        return status_page(
            StatusCode::NOT_FOUND,
            "No such delegation",
            "you hold no delegation by that name",
        );
    }
    match app
        .delegations
        .revoke(&id, &session.subject, "revoked by the person", now)
        .await
    {
        Ok(_) => {
            tracing::info!(subject = %session.subject, delegation_id = %id, "a person revoked a delegation");
            redirect("/delegations?revoked=1")
        }
        Err(failed) => refused(&failed.to_string()),
    }
}

/// GET /admin/people/{subject}/delegations: ListPersonsDelegations.
async fn persons(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(subject): Path<String>,
    Query(query): Query<Fields>,
) -> Response {
    let (session, records) = match gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let now = app.clock.now_ns();
    let theirs = match app.delegations.of_person(&subject, now).await {
        Ok(theirs) => theirs,
        Err(failed) => return refused(&failed.to_string()),
    };
    let token = token_input(&session);
    let called = theirs
        .first()
        .map(|d| d.display_name.clone())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| crate::admin::people::user_id(&subject));
    let action = format!(
        "/admin/people/{}/delegations/revoke",
        crate::html::escape(&path_segment(&subject))
    );
    let notice = match query.get("revoked").and_then(|n| n.parse::<usize>().ok()) {
        Some(n) => format!(
            "<p class=\"passed\">Revoked {n} delegation{}.</p>",
            if n == 1 { "" } else { "s" }
        ),
        None => String::new(),
    };
    let all = if theirs.iter().any(|d| d.live(now)) {
        format!(
            "<form method=\"post\" action=\"{action}\" data-confirm=\"Revoke every delegation \
             {called} holds? Each client stops at its next request; their browser sessions are \
             untouched.\">{token}<input type=\"hidden\" name=\"all\" value=\"1\">\
             <button type=\"submit\">Revoke them all</button></form>",
            called = escape(&called),
        )
    } else {
        String::new()
    };
    let rows = table(&theirs, &group_names(&records), now, &token, &|d| {
        Some((
            action.clone(),
            format!(
                "<input type=\"hidden\" name=\"delegation_id\" value=\"{}\">",
                escape(&d.id)
            ),
        ))
    });
    let body = format!(
        "<div class=\"page-head\"><div><h1>{name}'s connected clients</h1>\
         <p class=\"hint\">{login}</p></div>{all}</div>{notice}{rows}",
        name = escape(&called),
        login = escape(&subject),
    );
    let mut chrome = admin_chrome(&session);
    chrome.crumbs = format!(
        "{}{}",
        crumb_link("/admin#connected-clients", "Connected clients"),
        crumb_here(&called, None)
    );
    Html(page_with("Connected clients", &body, &chrome)).into_response()
}

/// A subject as one path segment: `local|ada` and a provider's
/// `https://idp|8812` both, every byte that is not plainly safe escaped.
pub(crate) fn path_segment(subject: &str) -> String {
    subject
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// POST /admin/people/{subject}/delegations/revoke: RevokePersonsDelegations.
/// One delegation, by `delegation_id`, or every one the person holds, by
/// `all`.
async fn revoke_persons(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(subject): Path<String>,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, _) = match gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if let Err(response) = form_token_matches(&session, &fields) {
        return *response;
    }
    let now = app.clock.now_ns();
    let why = "revoked by a deployment admin";
    let revoked = if fields.get("all").map(String::as_str) == Some("1") {
        app.delegations
            .revoke_person(&subject, &session.subject, why, now)
            .await
    } else {
        let id = fields.get("delegation_id").cloned().unwrap_or_default();
        match app.delegations.delegation(&id).await {
            Ok(Some(d)) if d.subject == subject => app
                .delegations
                .revoke(&id, &session.subject, why, now)
                .await
                .map(usize::from),
            Ok(_) => {
                return status_page(
                    StatusCode::NOT_FOUND,
                    "No such delegation",
                    "that person holds no delegation by that name",
                )
            }
            Err(failed) => Err(failed),
        }
    };
    match revoked {
        Ok(n) => {
            tracing::info!(%subject, revoked = n, by = %session.subject, "delegations revoked by a deployment admin");
            (
                StatusCode::SEE_OTHER,
                [(
                    LOCATION,
                    format!(
                        "/admin/people/{}/delegations?revoked={n}",
                        path_segment(&subject)
                    ),
                )],
            )
                .into_response()
        }
        Err(failed) => refused(&failed.to_string()),
    }
}

/// On the person's home: a delegation that lapses within a week, or did in
/// the last one (requirement 6). Nothing when there is none, or when they
/// cannot be read, which is not worth refusing the home over.
pub(crate) async fn notices(app: &App, subject: &str) -> String {
    let now = app.clock.now_ns();
    let theirs = match app.delegations.of_person(subject, now).await {
        Ok(theirs) => theirs,
        Err(failed) => {
            tracing::warn!(%failed, "a person's delegations could not be read for their home");
            return String::new();
        }
    };
    theirs
        .iter()
        .filter(|d| d.noticed(now))
        .map(|d| {
            format!(
                "<p class=\"notice warn\" data-delegation=\"{id}\">Your delegation to \
                 <strong>{client}</strong> {when}. Connect it again to renew it; \
                 <a href=\"/delegations\">Connected clients</a> lists them.</p>",
                id = escape(&d.id),
                client = escape(&d.client_name),
                when = escape(&super::oauth::lapses(d, now)),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests;
