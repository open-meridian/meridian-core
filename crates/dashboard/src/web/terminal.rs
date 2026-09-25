//! W6.13 and W6.14 over HTTP: a terminal's sign-in, its code, and its session.
//!
//! The sign-in itself is the deployment's own -- the same form, the same
//! provider -- and this module is only what differs: where it starts, what it
//! ends in, and what a terminal presents afterwards. The state it keeps is
//! [`crate::terminal`]'s.
//!
//! Two kinds of client, two kinds of credential, and neither accepted where
//! the other belongs: a bearer session only on `/terminal/` paths, which read
//! no cookie, and a browser's cookie everywhere else, which read no bearer.
//! So the form tokens protecting a browser's forms never have to reason about
//! a client that sends no cookie, and a terminal's session cannot be made to
//! drive a browser page.

use std::sync::Arc;

use axum::extract::{Form, Query, State};
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, LOCATION, SET_COOKIE, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};

use super::{password_page, record_sign_in, redirect, refused, set_cookie, App, SIGN_IN_COOKIE};
use crate::html::{escape, page};
use crate::session::IDLE_NS;
use crate::terminal::{check, rfc3339, Person, Refusal};

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/terminal/authorize", get(authorize).post(decide))
        .route("/terminal/token", post(exchange))
        .route("/terminal/sign-out", post(sign_out))
}

#[derive(serde::Deserialize)]
struct Asked {
    #[serde(default)]
    redirect_uri: String,
    #[serde(default)]
    code_challenge: String,
    #[serde(default)]
    code_challenge_method: String,
    #[serde(default)]
    state: String,
}

/// Start a terminal's sign-in: check what it asked for, then the
/// deployment's own sign-in, always afresh.
async fn authorize(State(app): State<Arc<App>>, Query(asked): Query<Asked>) -> Response {
    let now = app.clock.now_ns();
    if let Err(stale) = app.records.current(now) {
        return refused(&stale.to_string());
    }
    if app.first_run {
        return refused("this deployment is not set up yet, so nobody can sign in to it");
    }
    // Refused here, before anybody is asked for a password, and never by
    // sending the browser to the address it gave: a request that fails these
    // checks has not earned a redirect anywhere.
    let request = match check(
        &asked.redirect_uri,
        &asked.code_challenge,
        &asked.code_challenge_method,
        &asked.state,
    ) {
        Ok(request) => request,
        Err(why) => return not_accepted(&why),
    };
    let id = app.terminals.open(request, now);

    if app.directory.is_some() || app.accounts.is_some() {
        return Html(password_page("", Some(&id))).into_response();
    }
    let Some(oidc) = &app.oidc else {
        return refused("no directory is configured for this deployment's dashboard");
    };
    // The provider sends everybody back to /callback. This is how it will
    // know this one was a terminal's.
    let (url, provider_state) = oidc.begin(now);
    app.terminals.through_provider(&provider_state, &id);
    let mut response = redirect(&url);
    response.headers_mut().insert(
        SET_COOKIE,
        set_cookie(&app, SIGN_IN_COOKIE, &provider_state, "/callback", 600),
    );
    response
}

/// Somebody signed in to a terminal's request: record it, and ask them to
/// confirm. No browser session is made, by this or by anything after it.
pub(super) fn signed_in(
    app: &Arc<App>,
    id: &str,
    subject: &str,
    display_name: &str,
    groups: Vec<String>,
    now: i64,
) -> Response {
    let person = Person {
        subject: subject.to_string(),
        display_name: display_name.to_string(),
        directory_groups: groups.clone(),
        signed_in_at_ns: now,
    };
    let Some(confirm) = app.terminals.signed_in(id, person, now) else {
        return not_accepted(
            "this terminal sign-in has expired or was already used; run `meridian connect` again",
        );
    };
    record_sign_in(app, subject, display_name, groups, now);
    tracing::info!(subject, "signed in to connect a terminal");
    Html(page(
        "Connect a terminal",
        &format!(
            "<h1>Connect a terminal</h1>\
             <p>A <code>meridian</code> command on this computer asked to act as \
             <strong>{name}</strong> on this deployment. It will be able to do what you \
             can do here until it is signed out, goes unused for 30 minutes, or 12 hours \
             pass.</p>\
             <p><strong>Only connect it if you ran <code>meridian connect</code> yourself, \
             just now.</strong> If you did not, somebody is asking you to let them in.</p>\
             <form method=\"post\" action=\"/terminal/authorize\">\
             <input type=\"hidden\" name=\"request\" value=\"{id}\">\
             <input type=\"hidden\" name=\"confirm\" value=\"{confirm}\">\
             <button name=\"decision\" value=\"connect\">Connect</button> \
             <button name=\"decision\" value=\"decline\">Don't connect</button>\
             </form>",
            name = escape(display_name),
            id = escape(id),
            confirm = escape(&confirm),
        ),
    ))
    .into_response()
}

#[derive(serde::Deserialize)]
struct Decided {
    #[serde(default)]
    request: String,
    #[serde(default)]
    confirm: String,
    #[serde(default)]
    decision: String,
}

/// The person's answer, sent back to the terminal's loopback address: a
/// code, or that they declined.
async fn decide(State(app): State<Arc<App>>, Form(decided): Form<Decided>) -> Response {
    let now = app.clock.now_ns();
    let allow = decided.decision == "connect";
    match app
        .terminals
        .decide(&decided.request, &decided.confirm, allow, now)
    {
        Err(why) => not_accepted(why),
        // The state and the code need no escaping: the state was held to
        // unreserved characters when it arrived, and the code is base64url.
        Ok((back, Some(code))) => found(&format!(
            "{}?code={code}&state={}",
            back.redirect_uri, back.state
        )),
        Ok((back, None)) => found(&format!(
            "{}?error=access_denied&state={}",
            back.redirect_uri, back.state
        )),
    }
}

#[derive(serde::Deserialize)]
struct Exchanged {
    #[serde(default)]
    code: String,
    #[serde(default)]
    code_verifier: String,
    #[serde(default)]
    redirect_uri: String,
}

/// The CLI's code and verifier, for a session of its own.
async fn exchange(State(app): State<Arc<App>>, Form(asked): Form<Exchanged>) -> Response {
    let now = app.clock.now_ns();
    if let Err(stale) = app.records.current(now) {
        return json(
            StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({"error": "temporarily_unavailable", "error_description": stale.to_string()}),
        );
    }
    match app
        .terminals
        .exchange(&asked.code, &asked.code_verifier, &asked.redirect_uri, now)
    {
        Ok(issued) => {
            tracing::info!(subject = %issued.subject, "a terminal session began");
            json(
                StatusCode::OK,
                serde_json::json!({
                    "session": issued.session,
                    "subject": issued.subject,
                    "idle_seconds": IDLE_NS / 1_000_000_000,
                    "expires_at": rfc3339(issued.expires_at_ns),
                }),
            )
        }
        // One refusal whatever the reason, as RFC 6749 has it; the reason
        // is for whoever reads the log.
        Err(why) => {
            tracing::info!(why, "a terminal code was refused");
            json(
                StatusCode::BAD_REQUEST,
                serde_json::json!({"error": "invalid_grant"}),
            )
        }
    }
}

/// `meridian sign-out`. The same answer whether or not the session was
/// still live, because the CLI forgets it either way.
async fn sign_out(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if let Some(session) = bearer(&headers) {
        app.terminals.end(&session);
    }
    StatusCode::NO_CONTENT.into_response()
}

/// The terminal session a request presents, or the refusal to send back.
/// Only for `/terminal/` paths, and it reads no cookie.
pub fn terminal_session_of(app: &App, headers: &HeaderMap) -> Result<Person, Box<Response>> {
    let found = match bearer(headers) {
        Some(session) => app.terminals.find(&session, app.clock.now_ns()),
        None => Err(Refusal::Unknown),
    };
    found.map_err(|refusal| {
        let mut response = json(
            StatusCode::UNAUTHORIZED,
            serde_json::json!({"error": "invalid_token", "reason": refusal.reason()}),
        );
        response.headers_mut().insert(
            WWW_AUTHENTICATE,
            axum::http::HeaderValue::from_static("Bearer error=\"invalid_token\""),
        );
        Box::new(response)
    })
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then(|| token.to_string())
}

fn json(status: StatusCode, body: serde_json::Value) -> Response {
    // Never cached: a session is in one of these, and a refusal is about now.
    (status, [(CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

fn found(to: &str) -> Response {
    (StatusCode::FOUND, [(LOCATION, to.to_string())]).into_response()
}

fn not_accepted(sentence: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Html(page(
            "Not connected",
            &format!(
                "<h1>Not connected</h1><p class=\"refused\">{}</p>",
                escape(sentence)
            ),
        )),
    )
        .into_response()
}

#[cfg(test)]
mod tests;
