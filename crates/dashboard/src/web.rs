//! The dashboard's HTTP surface.
//!
//! Every page that shows anything about the deployment first asks
//! [`RecordsCache::current`], and refuses with its sentence when the records
//! are stale: a dashboard past the ceiling serves nothing but that refusal
//! and its own health. That includes signing in.
//!
//! Cookies are HttpOnly and SameSite=Lax, and Secure whenever the dashboard's
//! own address is HTTPS. Lax rather than Strict because the directory sends
//! the person back with a top-level redirect, which Strict would strip the
//! sign-in cookie from. Every form posts a token bound to its session, so Lax
//! costs nothing a form could be tricked into.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Form, Query, State};
use axum::http::header::{COOKIE, LOCATION, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use meridian_bus::Bus;
use meridian_domain::v1::SignInRecord;
use prost::Message;

use crate::clock::Clock;
use crate::directory::Directory;
use crate::html::{escape, page};
use crate::oidc::Oidc;
use crate::records::RecordsCache;
use crate::session::{Session, Sessions, ABSOLUTE_NS};

pub const SESSION_COOKIE: &str = "meridian_session";
pub const SIGN_IN_COOKIE: &str = "meridian_signin";
pub const PERSON_SIGNED_IN: &str = "platform.config.event.person-signed-in";

pub struct App {
    /// Whether this deployment has been configured yet. False is the wizard;
    /// true is a directory and everything else. Read from whether a directory
    /// is configured at all, so ending first run is the configuration landing
    /// rather than a flag somebody could flip back.
    pub first_run: bool,
    pub wizard: Arc<crate::first_run::WizardSession>,
    pub records: Arc<RecordsCache>,
    pub sessions: Arc<Sessions>,
    pub clock: Arc<dyn Clock>,
    pub bus: Arc<Bus>,
    /// None when no directory is configured, which the sign-in page says.
    pub oidc: Option<Arc<Oidc>>,
    /// The firm's LDAP, bound directly rather than brokered by an identity
    /// server of ours (decisions/018). At most one of this and `oidc` is set:
    /// two ways in would mean a person's groups depending on which they used.
    pub directory: Option<Arc<Directory>>,
    /// Whether cookies carry `Secure`: true whenever this dashboard is served
    /// over HTTPS, which is always outside a developer's machine.
    pub secure_cookies: bool,
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/", get(home))
        .route("/sign-in", get(sign_in).post(sign_in_with_password))
        .route("/callback", get(callback))
        .route("/sign-out", post(sign_out))
        .merge(crate::admin::routes())
        .merge(crate::first_run::routes())
        .with_state(app)
}

/// Alive, and whether the records are fresh enough to serve. A load balancer
/// takes a stale dashboard out rather than sending people to a refusal.
async fn healthz(State(app): State<Arc<App>>) -> Response {
    match app.records.current(app.clock.now_ns()) {
        Ok(_) => (StatusCode::OK, "serving\n").into_response(),
        Err(stale) => (StatusCode::SERVICE_UNAVAILABLE, format!("{stale}\n")).into_response(),
    }
}

pub(crate) fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.to_string())
}

pub(crate) fn set_cookie(
    app: &App,
    name: &str,
    value: &str,
    path: &str,
    max_age_s: i64,
) -> HeaderValue {
    let secure = if app.secure_cookies { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{name}={value}; Path={path}; Max-Age={max_age_s}; HttpOnly; SameSite=Lax{secure}"
    ))
    .expect("cookie values are url-safe")
}

/// The session a request carries, if it carries a live one.
pub fn session_of(app: &App, headers: &HeaderMap) -> Option<Session> {
    let key = cookie(headers, SESSION_COOKIE)?;
    app.sessions.find(&key, app.clock.now_ns())
}

fn redirect(to: &str) -> Response {
    (StatusCode::SEE_OTHER, [(LOCATION, to.to_string())]).into_response()
}

async fn home(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let records = match app.records.current(app.clock.now_ns()) {
        Ok(records) => records,
        Err(stale) => return refused(&stale.to_string()),
    };
    let Some(session) = session_of(&app, &headers) else {
        return Html(page(
            "Sign in",
            "<h1>Meridian</h1><p><a href=\"/sign-in\">Sign in</a> with your firm's directory.</p>",
        ))
        .into_response();
    };

    let access =
        meridian_access::person_access(&records, &session.subject, &session.directory_groups);
    let mut body = format!(
        "<h1>Meridian</h1><p>Signed in as <strong>{}</strong>.</p>",
        escape(&session.display_name)
    );
    if access.deployment_admin {
        body.push_str(
            "<p>You are a deployment admin: <a href=\"/admin\">administer this deployment</a>.</p>",
        );
    } else if !records
        .permissions
        .iter()
        .any(|p| p.access_group_id == meridian_access::DEPLOYMENT_ADMIN)
    {
        body.push_str("<p>Nobody administers this deployment yet. <a href=\"/claim\">Claim it</a> with a code from open-meridian.com.</p>");
    }
    if access.plugins.is_empty() {
        body.push_str("<p>You hold no access to any plugin.</p>");
    } else {
        body.push_str("<h2>Plugins</h2><ul>");
        for plugin in access.plugins.keys() {
            body.push_str(&format!("<li>{}</li>", escape(plugin)));
        }
        body.push_str("</ul>");
    }
    body.push_str(&format!(
        "<form method=\"post\" action=\"/sign-out\"><input type=\"hidden\" name=\"form_token\" \
         value=\"{}\"><button>Sign out</button></form>",
        escape(&session.form_token)
    ));
    Html(page("Home", &body)).into_response()
}

async fn sign_in(State(app): State<Arc<App>>) -> Response {
    let now = app.clock.now_ns();
    if let Err(stale) = app.records.current(now) {
        return refused(&stale.to_string());
    }
    if app.first_run {
        // Somebody trying to sign in to a deployment nobody has set up yet is
        // looking for the wizard, and the wizard is where a directory comes
        // from. Refusing them with a sentence about configuration would be
        // true and useless.
        return redirect("/first-run");
    }
    // A directory we bind to ourselves has nowhere to send the browser, so
    // the form is here rather than at somebody else's address. That absence
    // is the point of 018: no redirect means no second address, and no issuer
    // whose URL has to resolve from a pod and a browser at once.
    if app.directory.is_some() {
        return Html(password_page("")).into_response();
    }
    let Some(oidc) = &app.oidc else {
        return refused("no directory is configured for this deployment's dashboard");
    };
    let (url, state) = oidc.begin(now);
    let mut response = redirect(&url);
    response.headers_mut().insert(
        SET_COOKIE,
        set_cookie(&app, SIGN_IN_COOKIE, &state, "/callback", 600),
    );
    response
}

/// The sign-in form, for a directory this deployment binds to itself.
///
/// Deliberately plain about failure: one sentence, and the same one whether
/// the name is unknown or the password is wrong. Telling them apart would let
/// anybody who can reach this page enumerate a firm's staff.
fn password_page(refusal: &str) -> String {
    let told = if refusal.is_empty() {
        String::new()
    } else {
        format!("<p class=\"refusal\">{}</p>", escape(refusal))
    };
    page(
        "Sign in",
        &format!(
            "<h1>Sign in</h1>{told}\
             <form method=\"post\" action=\"/sign-in\">\
             <label for=\"name\">Username</label>\
             <input id=\"name\" name=\"name\" autocomplete=\"username\" required>\
             <label for=\"password\">Password</label>\
             <input id=\"password\" name=\"password\" type=\"password\" \
             autocomplete=\"current-password\" required>\
             <button type=\"submit\">Sign in</button>\
             </form>"
        ),
    )
}

#[derive(serde::Deserialize)]
pub struct Credentials {
    name: String,
    password: String,
}

/// W7.7 by the other route. The directory checks the password; we never do.
async fn sign_in_with_password(
    State(app): State<Arc<App>>,
    Form(credentials): Form<Credentials>,
) -> Response {
    let now = app.clock.now_ns();
    if let Err(stale) = app.records.current(now) {
        return refused(&stale.to_string());
    }
    let Some(directory) = &app.directory else {
        return refused("this deployment does not sign people in with a password");
    };

    let person = match directory
        .authenticate(&credentials.name, &credentials.password)
        .await
    {
        Ok(person) => person,
        Err(crate::directory::Failure::Refused) => {
            // 401 and the same sentence for both halves.
            let mut response = Html(password_page(
                "That username and password were not accepted.",
            ))
            .into_response();
            *response.status_mut() = StatusCode::UNAUTHORIZED;
            return response;
        }
        Err(ours) => {
            // Not the person's fault and not something they can act on, so it
            // is said plainly here and loudly in the log.
            tracing::warn!(%ours, "a sign-in could not reach the directory");
            let mut response = Html(password_page(&ours.to_string())).into_response();
            *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
            return response;
        }
    };

    let subject = format!("{}|{}", directory.issuer(), person.subject);
    began(&app, &subject, &person.name, person.groups, now)
}

/// Start the session, record who it was, and hand back the cookie.
///
/// One place, because both ways in owe the same things afterwards and a
/// second copy is where they would drift: a person signed in through one
/// route appearing in the access table and not the other.
fn began(
    app: &Arc<App>,
    subject: &str,
    display_name: &str,
    groups: Vec<String>,
    now: i64,
) -> Response {
    let key = app
        .sessions
        .start(subject, display_name, groups.clone(), now);
    let record = SignInRecord {
        subject: subject.to_string(),
        display_name: display_name.to_string(),
        directory_groups: groups,
        signed_in_at_ns: now,
    };
    if let Err(failed) = app.bus.publish(
        PERSON_SIGNED_IN,
        "meridian.v1.SignInRecord",
        record.encode_to_vec(),
        None,
        None,
    ) {
        // The session stands: recording who signed in is for the access table
        // and the count, and a person should not be locked out because a
        // broker hiccupped.
        tracing::warn!("a sign-in was not recorded: {failed}");
    }
    tracing::info!(subject, "signed in");

    let mut response = redirect("/");
    response.headers_mut().append(
        SET_COOKIE,
        set_cookie(app, SESSION_COOKIE, &key, "/", ABSOLUTE_NS / 1_000_000_000),
    );
    response
}

async fn callback(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let now = app.clock.now_ns();
    if let Err(stale) = app.records.current(now) {
        return refused(&stale.to_string());
    }
    let Some(oidc) = &app.oidc else {
        return refused("no directory is configured for this deployment's dashboard");
    };
    if let Some(error) = query.get("error") {
        return bad_request(&format!("the directory declined the sign-in: {error}"));
    }
    let (Some(state), Some(code)) = (query.get("state"), query.get("code")) else {
        return bad_request("the directory's reply is missing its state or code");
    };
    // The state must come back to the browser that started it. Without this,
    // someone could send a person a link that signs them in as someone else.
    if cookie(&headers, SIGN_IN_COOKIE).as_deref() != Some(state.as_str()) {
        return bad_request("this sign-in was started in another browser; start again here");
    }

    let identity = match oidc.finish(state, code, now).await {
        Ok(identity) => identity,
        Err(failed) => return bad_request(&failed),
    };

    let mut response = began(
        &app,
        &identity.subject,
        &identity.display_name,
        identity.groups,
        now,
    );
    // And the one cookie only this route sets: the state it was matched
    // against has done its work and should not outlive it.
    response.headers_mut().append(
        SET_COOKIE,
        set_cookie(&app, SIGN_IN_COOKIE, "", "/callback", 0),
    );
    response
}

#[derive(serde::Deserialize)]
struct Posted {
    form_token: String,
}

async fn sign_out(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(posted): Form<Posted>,
) -> Response {
    if let (Some(key), Some(session)) =
        (cookie(&headers, SESSION_COOKIE), session_of(&app, &headers))
    {
        if session.form_token != posted.form_token {
            return bad_request("this form did not come from your session");
        }
        app.sessions.end(&key);
    }
    let mut response = redirect("/");
    response
        .headers_mut()
        .insert(SET_COOKIE, set_cookie(&app, SESSION_COOKIE, "", "/", 0));
    response
}

/// A refusal a person can read, with the status that says it is not their doing.
pub fn refused(sentence: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Html(page(
            "Unavailable",
            &format!(
                "<h1>Unavailable</h1><p class=\"refused\">{}</p>",
                escape(sentence)
            ),
        )),
    )
        .into_response()
}

fn bad_request(sentence: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Html(page(
            "Not signed in",
            &format!(
                "<h1>Not signed in</h1><p class=\"refused\">{}</p><p><a href=\"/sign-in\">Start again</a></p>",
                escape(sentence)
            ),
        )),
    )
        .into_response()
}

#[cfg(test)]
mod tests;
