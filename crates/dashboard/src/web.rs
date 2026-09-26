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

use crate::accounts::{self, Accounts};
use crate::clock::Clock;
use crate::directory::Directory;
use crate::html::{escape, page};
use crate::oidc::Oidc;
use crate::records::{refresh, RecordsCache};
use crate::session::{Session, Sessions, ABSOLUTE_NS};
use crate::terminal::Terminals;

mod terminal;
pub use terminal::terminal_session_of;

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
    /// Terminals' requests, codes and sessions (W6.13, W6.14). Apart from
    /// `sessions` because a terminal's session is never a browser's.
    pub terminals: Arc<Terminals>,
    pub clock: Arc<dyn Clock>,
    pub bus: Arc<Bus>,
    /// None when no directory is configured, which the sign-in page says.
    pub oidc: Option<Arc<Oidc>>,
    /// The firm's LDAP, bound directly rather than brokered by an identity
    /// server of ours (decisions/018). At most one of this and `oidc` is set:
    /// two ways in would mean a person's groups depending on which they used.
    pub directory: Option<Arc<Directory>>,
    /// Accounts this deployment holds, where the firm has no directory at
    /// all. None on the other two branches, which is what keeps a dashboard
    /// signing people in through a provider or through LDAP holding nothing
    /// but its sessions.
    pub accounts: Option<Arc<dyn Accounts>>,
    /// Whether cookies carry `Secure`: true whenever this dashboard is served
    /// over HTTPS, which is always outside a developer's machine.
    pub secure_cookies: bool,
    /// Plugins' pages, each on its own host (decisions/021). None when this
    /// dashboard has no public address to put them under, or no front door
    /// to send them to; it says so rather than serving them from its own.
    pub plugins: Option<Arc<crate::plugins::Plugins>>,
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/", get(home))
        .route("/sign-in", get(sign_in).post(sign_in_with_password))
        .route("/callback", get(callback))
        .route("/sign-out", post(sign_out))
        .merge(terminal::routes())
        .merge(crate::admin::routes())
        .merge(crate::first_run::routes())
        .route("/plugins/{instance}", get(crate::plugins::open))
        .with_state(Arc::clone(&app))
        // Outermost, so a request for a plugin's host meets none of the
        // dashboard's pages, whatever its path.
        .layer(axum::middleware::from_fn_with_state(
            app,
            crate::plugins::on_plugin_host,
        ))
}

/// Alive, and whether the records are fresh enough to serve. A load balancer
/// takes a stale dashboard out rather than sending people to a refusal.
async fn healthz(State(app): State<Arc<App>>) -> Response {
    match app.records.current(app.clock.now_ns()) {
        Ok(_) => (StatusCode::OK, "serving\n").into_response(),
        Err(stale) => (StatusCode::SERVICE_UNAVAILABLE, format!("{stale}\n")).into_response(),
    }
}

/// A cookie's name as the browser holds it. Over HTTPS every cookie of ours
/// is `__Host-`: the browser then refuses one set with a `Domain`, or from
/// anywhere but this host. That matters since plugin pages are served from
/// subdomains of this host (decisions/021): without it, a plugin's script
/// could set `meridian_session` for the parent domain and have the dashboard
/// read it -- somebody else's session, or one it chose. Over plain HTTP, on a
/// developer's machine, no prefix is possible and none is used.
pub fn cookie_name(secure: bool, name: &str) -> String {
    if secure {
        format!("__Host-{name}")
    } else {
        name.to_string()
    }
}

/// The value of one of our cookies, by its unprefixed name.
pub(crate) fn cookie(app: &App, headers: &HeaderMap, name: &str) -> Option<String> {
    named(headers, &cookie_name(app.secure_cookies, name))
}

pub(crate) fn named(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.to_string())
}

/// `__Host-` requires the whole host, so a prefixed cookie's path is `/`
/// whatever the caller asked for; the narrower path was only ever tidiness.
pub(crate) fn set_cookie(
    app: &App,
    name: &str,
    value: &str,
    path: &str,
    max_age_s: i64,
) -> HeaderValue {
    let (secure, path) = if app.secure_cookies {
        ("; Secure", "/")
    } else {
        ("", path)
    };
    let name = cookie_name(app.secure_cookies, name);
    HeaderValue::from_str(&format!(
        "{name}={value}; Path={path}; Max-Age={max_age_s}; HttpOnly; SameSite=Lax{secure}"
    ))
    .expect("cookie values are url-safe")
}

/// The session a request carries, if it carries a live one.
pub fn session_of(app: &App, headers: &HeaderMap) -> Option<Session> {
    let key = cookie(app, headers, SESSION_COOKIE)?;
    app.sessions.find(&key, app.clock.now_ns())
}

pub(crate) fn redirect(to: &str) -> Response {
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
            // Linked when it can be opened: access to no account yet is
            // listed, and would be refused at the door.
            let openable = !access.on_plugin(plugin).is_empty();
            if app.plugins.is_some() && openable && crate::plugins::is_instance(plugin) {
                body.push_str(&format!(
                    "<li><a href=\"/plugins/{0}\">{0}</a></li>",
                    escape(plugin)
                ));
            } else {
                body.push_str(&format!("<li>{}</li>", escape(plugin)));
            }
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
    if app.directory.is_some() || app.accounts.is_some() {
        return Html(password_page("", None)).into_response();
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
///
/// The same form signs somebody in for a terminal (W6.13), carrying the
/// terminal's request so the sign-in ends in a confirmation rather than a
/// browser session.
fn password_page(refusal: &str, terminal: Option<&str>) -> String {
    let told = if refusal.is_empty() {
        String::new()
    } else {
        format!("<p class=\"refusal\">{}</p>", escape(refusal))
    };
    let (heading, carried) = match terminal {
        Some(id) => (
            "Sign in to connect a terminal",
            format!(
                "<input type=\"hidden\" name=\"terminal\" value=\"{}\">",
                escape(id)
            ),
        ),
        None => ("Sign in", String::new()),
    };
    page(
        "Sign in",
        &format!(
            "<h1>{heading}</h1>{told}\
             <form method=\"post\" action=\"/sign-in\">{carried}\
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
    /// A terminal's request, when this sign-in is for one.
    #[serde(default)]
    terminal: String,
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
    let terminal = Some(credentials.terminal.as_str()).filter(|id| !id.is_empty());
    // The same sentence for both halves, wherever the refusal came from.
    let no = |reason: &str, status: StatusCode| {
        let mut response = Html(password_page(reason, terminal)).into_response();
        *response.status_mut() = status;
        response
    };
    let refused_them = || {
        no(
            "That username and password were not accepted.",
            StatusCode::UNAUTHORIZED,
        )
    };

    if let Some(directory) = &app.directory {
        return match directory
            .authenticate(&credentials.name, &credentials.password)
            .await
        {
            Ok(person) => {
                let subject = format!("{}|{}", directory.issuer(), person.subject);
                match terminal {
                    Some(id) => {
                        terminal::signed_in(&app, id, &subject, &person.name, person.groups, now)
                    }
                    None => began(&app, &subject, &person.name, person.groups, now).await,
                }
            }
            Err(crate::directory::Failure::Refused) => refused_them(),
            Err(ours) => {
                // Not the person's fault and not something they can act on,
                // so it is said plainly here and loudly in the log.
                tracing::warn!(%ours, "a sign-in could not reach the directory");
                no(&ours.to_string(), StatusCode::SERVICE_UNAVAILABLE)
            }
        };
    }

    let Some(accounts) = &app.accounts else {
        return refused("this deployment does not sign people in with a password");
    };

    // On the blocking pool: this is the synchronous Postgres client, as every
    // store in this deployment is, and it makes a runtime of its own. Called
    // straight from an async handler it panics in a destructor, which arrives
    // at the browser as a connection closed with no response and in the log
    // as a backtrace with no cause.
    //
    // Argon2 belongs off the async threads anyway. It is deliberately slow,
    // and a handful of sign-ins would otherwise stall every other request.
    let asked = Arc::clone(accounts);
    let name = credentials.name.clone();
    let password = credentials.password.clone();
    let outcome = match tokio::task::spawn_blocking(move || {
        accounts::authenticate(asked.as_ref(), &name, &password, now)
    })
    .await
    {
        Ok(outcome) => outcome,
        Err(joined) => {
            tracing::error!(%joined, "a sign-in did not finish");
            return no(
                "This deployment could not check that sign-in. Try again shortly.",
                StatusCode::SERVICE_UNAVAILABLE,
            );
        }
    };

    match outcome {
        accounts::Outcome::SignedIn {
            subject,
            display_name,
            groups,
        } => match terminal {
            Some(id) => terminal::signed_in(&app, id, &subject, &display_name, groups, now),
            None => began(&app, &subject, &display_name, groups, now).await,
        },
        accounts::Outcome::Refused => refused_them(),
        // Said plainly, and not as a refusal: somebody locked out and not
        // told keeps trying and cannot tell it from a wrong password.
        accounts::Outcome::Locked => no(
            "Too many attempts. Try again in a little while.",
            StatusCode::TOO_MANY_REQUESTS,
        ),
        accounts::Outcome::Unavailable(ours) => {
            tracing::warn!(%ours, "a sign-in could not be checked");
            no(
                "This deployment could not check that sign-in. Try again shortly.",
                StatusCode::SERVICE_UNAVAILABLE,
            )
        }
    }
}

/// Start the session, record who it was, and hand back the cookie.
///
/// One place, because both ways in owe the same things afterwards and a
/// second copy is where they would drift: a person signed in through one
/// route appearing in the access table and not the other.
async fn began(
    app: &Arc<App>,
    subject: &str,
    display_name: &str,
    groups: Vec<String>,
    now: i64,
) -> Response {
    // The access records read again, so a person lands holding what they
    // hold now rather than what they held at the last 30-second read. First
    // run is where that differs: the dashboard restarts and reads, then the
    // conductor writes the administrator the wizard named, and the page that
    // told them to sign in sent them to a dashboard that did not know yet.
    // Best effort: records that cannot be read now are the ones already held,
    // and the ceiling, not this, decides when they are too old to serve.
    if let Err(failed) = refresh(&app.bus, &app.records, app.clock.as_ref()).await {
        tracing::warn!("the access records could not be read at sign-in: {failed}");
    }
    let key = app
        .sessions
        .start(subject, display_name, groups.clone(), now);
    record_sign_in(app, subject, display_name, groups, now);
    tracing::info!(subject, "signed in");

    let mut response = redirect("/");
    response.headers_mut().append(
        SET_COOKIE,
        set_cookie(app, SESSION_COOKIE, &key, "/", ABSOLUTE_NS / 1_000_000_000),
    );
    response
}

/// Say who signed in, for the access table and the count. Every sign-in,
/// a browser's or a terminal's, because W6.1 records them all.
pub(crate) fn record_sign_in(
    app: &App,
    subject: &str,
    display_name: &str,
    groups: Vec<String>,
    now: i64,
) {
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
    if cookie(&app, &headers, SIGN_IN_COOKIE).as_deref() != Some(state.as_str()) {
        return bad_request("this sign-in was started in another browser; start again here");
    }

    // Whether this sign-in was a terminal's, asked before it is finished so
    // a state is matched to at most one request, whatever happens next.
    let terminal = app.terminals.for_provider_state(state);
    let identity = match oidc.finish(state, code, now).await {
        Ok(identity) => identity,
        Err(failed) => return bad_request(&failed),
    };

    let mut response = match terminal {
        Some(id) => terminal::signed_in(
            &app,
            &id,
            &identity.subject,
            &identity.display_name,
            identity.groups,
            now,
        ),
        None => {
            began(
                &app,
                &identity.subject,
                &identity.display_name,
                identity.groups,
                now,
            )
            .await
        }
    };
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
    if let (Some(key), Some(session)) = (
        cookie(&app, &headers, SESSION_COOKIE),
        session_of(&app, &headers),
    ) {
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
