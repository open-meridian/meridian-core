//! A person's way to a plugin's page (W6.9, decisions/014 and 021).
//!
//! Each plugin instance's page has an origin of its own,
//! `{instance}.plugins.{dashboard-host}`, which this process also serves, so a
//! plugin's script runs with nothing of the dashboard's. A person gets there
//! from the dashboard: `/plugins/{instance}` checks they hold access on the
//! plugin and hands the browser a one-time code, which the plugin's host
//! redeems for a session of its own, bound to the dashboard session it came
//! from and ending with it.
//!
//! On the plugin's host, every request is checked again -- the session, and
//! the person's access from the records as they are now -- and then signed:
//! who they are and what they hold on this plugin, for this instance alone,
//! for 60 seconds, once. The request goes to that instance's sidecar, which
//! verifies the signature before anything reaches the plugin; this streams it
//! there and the answer back without reading either.
//!
//! What the request arrived carrying about who it is -- cookies, credentials,
//! a `Meridian-Caller` of its own -- goes no further than here, and the
//! sidecar removes the same again. So a plugin sets no cookies either: one it
//! set could never come back to it, and could only be somebody's attempt to
//! plant one. Who is asking is the assertion, every time.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::header::{ACCEPT, CACHE_CONTROL, CONTENT_TYPE, HOST, SET_COOKIE};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use meridian_pb::v1::{CallerClaims, TagAccess};
use prost::Message;

use crate::clock::SECOND_NS;
use crate::html::{escape, page};
use crate::session::{token, Sessions, ABSOLUTE_NS};
use crate::signing::Signer;
use crate::terminal::Terminals;
use crate::web::{cookie, redirect, refused, set_cookie, App, SESSION_COOKIE};

/// The plugin host's own session.
pub const PLUGIN_COOKIE: &str = "meridian_plugin_session";
/// Where a plugin's host redeems a code. Under a path no plugin would choose,
/// since everything else on the host is the plugin's.
pub const ENTER_PATH: &str = "/.meridian/enter";
/// The header the sidecar verifies (plans/a-person-reaches-a-plugin, ruling 2).
pub const CALLER: &str = "meridian-caller";

/// The most of a page `--print` hands back: a page, not a download.
const PAGE_MOST: usize = 8 << 20;

/// How long a code and an assertion live.
const CODE_NS: i64 = 60 * SECOND_NS;
const ASSERTION_NS: i64 = 60 * SECOND_NS;

/// The session a plugin host's session came from, which it ends with: a
/// browser's on the dashboard, or a terminal's (W6.15), held by the hash its
/// own store keys it by, so no terminal's token is kept anywhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Came {
    Browser(String),
    Terminal(String),
}

/// Who a session names, whichever kind it is.
pub(crate) struct Who {
    pub subject: String,
    pub display_name: String,
    pub directory_groups: Vec<String>,
}

impl Came {
    /// The person, if the session is still live; touched, since this is
    /// them using it.
    fn who(&self, app: &App, now_ns: i64) -> Option<Who> {
        match self {
            Came::Browser(key) => app.sessions.find(key, now_ns).map(|s| Who {
                subject: s.subject,
                display_name: s.display_name,
                directory_groups: s.directory_groups,
            }),
            Came::Terminal(hash) => app.terminals.find_hashed(hash, now_ns).ok().map(|p| Who {
                subject: p.subject,
                display_name: p.display_name,
                directory_groups: p.directory_groups,
            }),
        }
    }

    fn is_live(&self, sessions: &Sessions, terminals: &Terminals, now_ns: i64) -> bool {
        match self {
            Came::Browser(key) => sessions.is_live(key, now_ns),
            Came::Terminal(hash) => terminals.is_live_hashed(hash, now_ns),
        }
    }
}

/// What the plugin's host needs to know about a person who came from the
/// dashboard or a terminal.
struct Entered {
    came: Came,
    instance: String,
}

struct Code {
    came: Came,
    instance: String,
    expires_at_ns: i64,
}

pub struct Plugins {
    scheme: String,
    /// The dashboard's own host, without a port. Plugin hosts sit below it.
    host: String,
    /// The port a browser names, when the dashboard's address names one.
    port: Option<u16>,
    /// An instance's sidecar front door, with `{instance}` in it.
    front_door: String,
    signer: Signer,
    client: reqwest::Client,
    /// Names answered without asking DNS: a test's sidecars, on loopback.
    pinned: HashMap<String, SocketAddr>,
    codes: Mutex<HashMap<String, Code>>,
    entered: Mutex<HashMap<String, Entered>>,
}

/// A plugin's redirect goes back to the browser, which follows it on the
/// plugin's host with a new assertion; followed here, it would be a second
/// request under the first one's, to wherever the plugin said. No overall
/// timeout, since a page may stream for as long as it is open.
fn client(pinned: &HashMap<String, SocketAddr>) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(std::time::Duration::from_secs(5));
    for (name, at) in pinned {
        builder = builder.resolve(name, *at);
    }
    builder
        .build()
        .map_err(|failed| format!("the plugin client could not be built: {failed}"))
}

/// What a request's host is to this process.
#[derive(Debug, PartialEq, Eq)]
pub enum Host {
    /// Anything that is not a plugin's: the dashboard's own, or an address
    /// used from inside the cluster.
    Dashboard,
    Plugin(String),
    /// Under the plugins' domain, and not an instance's name. Never the
    /// dashboard's pages, whatever the path.
    NoSuchPlugin,
}

/// An instance's name, as a host label and a Service name both have it.
pub fn is_instance(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

impl Plugins {
    /// From the dashboard's public address, the front door's template and the
    /// key it signs with.
    pub fn new(public_url: &str, front_door: &str, signer: Signer) -> Result<Plugins, String> {
        pages_possible(public_url)?;
        let url = reqwest::Url::parse(public_url)
            .map_err(|failed| format!("{public_url} is not an address: {failed}"))?;
        let host = url
            .host_str()
            .ok_or_else(|| format!("{public_url} names no host"))?
            .to_ascii_lowercase();
        if !front_door.contains("{instance}") {
            return Err(format!(
                "the plugin front door {front_door} does not say where {{instance}} goes"
            ));
        }
        reqwest::Url::parse(&front_door.replace("{instance}", "x"))
            .map_err(|failed| format!("{front_door} is not an address: {failed}"))?;
        Ok(Plugins {
            scheme: url.scheme().to_string(),
            host,
            port: url.port(),
            front_door: front_door.to_string(),
            signer,
            client: client(&HashMap::new())?,
            pinned: HashMap::new(),
            codes: Mutex::new(HashMap::new()),
            entered: Mutex::new(HashMap::new()),
        })
    }

    /// Answer `name` with `at` rather than asking DNS. The port is still the
    /// front door's own.
    #[cfg(test)]
    pub(crate) fn resolving(mut self, name: &str, at: SocketAddr) -> Plugins {
        self.pinned.insert(name.to_string(), at);
        self.client = client(&self.pinned).expect("a client");
        self
    }

    fn port_suffix(&self) -> String {
        self.port.map(|port| format!(":{port}")).unwrap_or_default()
    }

    /// Where an instance's page is.
    pub fn origin(&self, instance: &str) -> String {
        format!(
            "{}://{instance}.plugins.{}{}",
            self.scheme,
            self.host,
            self.port_suffix()
        )
    }

    fn dashboard_origin(&self) -> String {
        format!("{}://{}{}", self.scheme, self.host, self.port_suffix())
    }

    /// What a `Host` header names.
    pub fn host(&self, header: &str) -> Host {
        let name = header.rsplit_once(':').map_or(header, |(name, port)| {
            if port.bytes().all(|b| b.is_ascii_digit()) {
                name
            } else {
                header
            }
        });
        let name = name.trim_end_matches('.').to_ascii_lowercase();
        let Some(below) = name.strip_suffix(&format!(".plugins.{}", self.host)) else {
            return if name == format!("plugins.{}", self.host) {
                Host::NoSuchPlugin
            } else {
                Host::Dashboard
            };
        };
        if is_instance(below) {
            Host::Plugin(below.to_string())
        } else {
            Host::NoSuchPlugin
        }
    }

    fn front_door(&self, instance: &str) -> String {
        self.front_door.replace("{instance}", instance)
    }

    /// A live instance's development path (W8.5, W8.6), for a person who may
    /// launch plugins, relayed from their terminal: signed as them, for this
    /// instance alone, as a page request is, and passed to the sidecar's
    /// development endpoint. The sidecar decides whether there is one.
    pub(crate) async fn develop(
        &self,
        instance: &str,
        person: &crate::terminal::Person,
        asked: Development,
        now: i64,
    ) -> Response {
        let Development {
            method,
            what,
            query,
            body,
        } = asked;
        let claims = CallerClaims {
            subject: person.subject.clone(),
            display_name: person.display_name.clone(),
            audience_instance_id: instance.to_string(),
            access: Vec::new(),
            issued_at_ns: now,
            expires_at_ns: now + ASSERTION_NS,
            assertion_id: token(),
        };
        let assertion = match self.signer.sign(&claims) {
            Ok(assertion) => URL_SAFE_NO_PAD.encode(assertion.encode_to_vec()),
            Err(failed) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("the dashboard cannot vouch for anybody yet: {failed}"),
                )
                    .into_response()
            }
        };
        let url = match query.as_deref() {
            Some(query) => format!("{}/.meridian/dev/{what}?{query}", self.front_door(instance)),
            None => format!("{}/.meridian/dev/{what}", self.front_door(instance)),
        };
        let answer = self
            .client
            .request(method, url)
            .header(CALLER, assertion)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await;
        match answer {
            Ok(answer) => {
                let status = answer.status();
                let body = answer.bytes().await.unwrap_or_default();
                (status, [("content-type", "application/json")], body).into_response()
            }
            Err(failed) => (
                StatusCode::BAD_GATEWAY,
                format!("{instance}'s sidecar did not answer: {failed}"),
            )
                .into_response(),
        }
    }

    /// Whether an instance runs here: whether its front door has an address.
    /// A Service exists for every sidecar the deployment runs and for nothing
    /// else, so this needs no list of its own to fall behind.
    async fn runs(&self, instance: &str) -> bool {
        let Ok(url) = reqwest::Url::parse(&self.front_door(instance)) else {
            return false;
        };
        let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
            return false;
        };
        if self.pinned.contains_key(host) {
            return true;
        }
        let found = tokio::net::lookup_host((host, port)).await;
        found.is_ok_and(|mut addresses| addresses.next().is_some())
    }

    fn mint(&self, came: Came, instance: &str, now_ns: i64) -> String {
        let code = token();
        self.codes.lock().expect("code lock poisoned").insert(
            code.clone(),
            Code {
                came,
                instance: instance.to_string(),
                expires_at_ns: now_ns + CODE_NS,
            },
        );
        code
    }

    /// The session a code was minted for, if it is being presented on its
    /// own instance's host, in time. Gone once presented, whatever the
    /// answer: a code is tried once.
    fn redeem(&self, code: &str, instance: &str, now_ns: i64) -> Option<Came> {
        let held = self
            .codes
            .lock()
            .expect("code lock poisoned")
            .remove(code)?;
        (held.instance == instance && now_ns <= held.expires_at_ns).then_some(held.came)
    }

    fn enter(&self, came: Came, instance: &str) -> String {
        let key = token();
        self.entered.lock().expect("entered lock poisoned").insert(
            key.clone(),
            Entered {
                came,
                instance: instance.to_string(),
            },
        );
        key
    }

    fn entered(&self, key: &str, instance: &str) -> Option<Came> {
        let entered = self.entered.lock().expect("entered lock poisoned");
        let held = entered.get(key)?;
        (held.instance == instance).then(|| held.came.clone())
    }

    fn leave(&self, key: &str) {
        self.entered
            .lock()
            .expect("entered lock poisoned")
            .remove(key);
    }

    /// Forget codes past their minute and plugin sessions whose dashboard or
    /// terminal session has ended.
    pub fn sweep(&self, sessions: &Sessions, terminals: &Terminals, now_ns: i64) {
        self.codes
            .lock()
            .expect("code lock poisoned")
            .retain(|_, code| code.expires_at_ns >= now_ns);
        self.entered
            .lock()
            .expect("entered lock poisoned")
            .retain(|_, held| held.came.is_live(sessions, terminals, now_ns));
    }
}

fn said(status: StatusCode, title: &str, sentence: &str) -> Response {
    (
        status,
        Html(page(
            title,
            &format!("<h1>{title}</h1><p>{}</p>", escape(sentence)),
        )),
    )
        .into_response()
}

/// `GET /plugins/{instance}` on the dashboard: EnterPlugin.
pub(crate) async fn open(
    State(app): State<Arc<App>>,
    Path(instance): Path<String>,
    headers: HeaderMap,
) -> Response {
    let now = app.clock.now_ns();
    let records = match app.records.current(now) {
        Ok(records) => records,
        Err(stale) => return refused(&stale.to_string()),
    };
    let (Some(key), Some(session)) = (
        cookie(&app, &headers, SESSION_COOKIE),
        crate::web::session_of(&app, &headers),
    ) else {
        return redirect("/sign-in");
    };
    let Some(plugins) = &app.plugins else {
        return refused(
            "this dashboard serves no plugin pages: it needs its own address \
             (MERIDIAN_DASHBOARD_URL) and where plugins' sidecars are",
        );
    };
    if !is_instance(&instance) {
        return said(
            StatusCode::NOT_FOUND,
            "No such plugin",
            "That is not a plugin's name.",
        );
    }
    let access =
        meridian_access::person_access(&records, &session.subject, &session.directory_groups);
    if opening(&access, &instance).is_none() {
        return said(
            StatusCode::FORBIDDEN,
            "No access",
            &format!("You hold no access on {instance}."),
        );
    }
    if !plugins.runs(&instance).await {
        return said(
            StatusCode::NOT_FOUND,
            "No such plugin",
            &format!("No plugin {instance} runs in this deployment."),
        );
    }
    let code = plugins.mint(Came::Browser(key), &instance, now);
    redirect(&format!(
        "{}{ENTER_PATH}?code={code}",
        plugins.origin(&instance)
    ))
}

/// Whether plugins' pages can be served below this dashboard's address: not
/// below an IP address, which has no names under it -- a browser reads
/// `snaptrade-1.plugins.127.0.0.1` as no address at all.
pub fn pages_possible(public_url: &str) -> Result<(), String> {
    let url = reqwest::Url::parse(public_url)
        .map_err(|failed| format!("{public_url} is not an address: {failed}"))?;
    // A domain, as against an address: `domain()` is none for either kind
    // of IP address.
    match (url.domain(), url.host_str()) {
        (Some(_), _) => Ok(()),
        (None, Some(_)) => Err(format!(
            "{public_url} is an IP address, and each plugin's page is served on a name \
             below the dashboard's ({{instance}}.plugins.<host>), which an address has none \
             of; give the dashboard a name -- http://localhost:<port> on one machine"
        )),
        (None, None) => Err(format!("{public_url} names no host")),
    }
}

/// What a person carries onto a plugin's page, if they may open it: their
/// access on it; or, for a deployment admin, who opens any plugin's page,
/// whatever they hold there, which may be nothing. Opening is not access, so
/// an admin is asserted with nothing they do not hold
/// (spec/deployment-dashboard-and-access, ruling 19).
pub(crate) fn opening(access: &meridian_access::Access, instance: &str) -> Option<Vec<TagAccess>> {
    let held = access.on_plugin(instance);
    (access.deployment_admin || !held.is_empty()).then_some(held)
}

// ── From a terminal (W6.15) ─────────────────────────────────────────────

fn answered(status: StatusCode, body: serde_json::Value) -> Response {
    (status, [(CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

fn declined(status: StatusCode, reason: impl Into<String>) -> Response {
    answered(status, serde_json::json!({ "error": reason.into() }))
}

/// The person on a terminal session, what they hold on the plugin as opening
/// it from the dashboard would find it (W6.9's checks, ruling 19 included),
/// and the session a plugin-host session opened for them would end with.
fn from_terminal(
    app: &App,
    headers: &HeaderMap,
    instance: &str,
    now: i64,
) -> Result<(Who, Vec<TagAccess>, Came), Box<Response>> {
    let person = crate::web::terminal_session_of(app, headers)?;
    let came = Came::Terminal(
        crate::web::bearer(headers)
            .map(|token| crate::terminal::hashed(&token))
            .unwrap_or_default(),
    );
    if !is_instance(instance) {
        return Err(Box::new(declined(
            StatusCode::NOT_FOUND,
            "that is not a plugin's name",
        )));
    }
    let records = app
        .records
        .current(now)
        .map_err(|stale| Box::new(declined(StatusCode::SERVICE_UNAVAILABLE, stale.to_string())))?;
    let access =
        meridian_access::person_access(&records, &person.subject, &person.directory_groups);
    let Some(held) = opening(&access, instance) else {
        return Err(Box::new(declined(
            StatusCode::FORBIDDEN,
            format!("you hold no access on {instance}"),
        )));
    };
    let who = Who {
        subject: person.subject,
        display_name: person.display_name,
        directory_groups: person.directory_groups,
    };
    Ok((who, held, came))
}

/// The plugins a terminal can reach, and that this one runs.
async fn running<'a>(app: &'a App, instance: &str) -> Result<&'a Plugins, Box<Response>> {
    let Some(plugins) = app.plugins.as_deref() else {
        return Err(Box::new(declined(
            StatusCode::SERVICE_UNAVAILABLE,
            "this dashboard serves no plugin pages: it needs its own address",
        )));
    };
    if !plugins.runs(instance).await {
        return Err(Box::new(declined(
            StatusCode::NOT_FOUND,
            format!("no plugin {instance} runs in this deployment"),
        )));
    }
    Ok(plugins)
}

/// `POST /terminal/plugins/{instance}/open`: OpenPluginFromTerminal. The
/// code `/plugins/{instance}` would mint, bound to the terminal session:
/// whichever browser opens the link first enters that plugin's host alone,
/// until the terminal session ends.
pub(crate) async fn open_from_terminal(
    State(app): State<Arc<App>>,
    Path(instance): Path<String>,
    headers: HeaderMap,
) -> Response {
    let now = app.clock.now_ns();
    let came = match from_terminal(&app, &headers, &instance, now) {
        Ok((_, _, came)) => came,
        Err(refusal) => return *refusal,
    };
    let plugins = match running(&app, &instance).await {
        Ok(plugins) => plugins,
        Err(refusal) => return *refusal,
    };
    let code = plugins.mint(came, &instance, now);
    answered(
        StatusCode::OK,
        serde_json::json!({
            "instance_id": instance,
            "url": format!("{}{ENTER_PATH}?code={code}", plugins.origin(&instance)),
        }),
    )
}

/// A path on a plugin's host and nothing else: one leading `/`, no
/// authority, and not under `/.meridian`, which on a plugin's host is the
/// dashboard's and the sidecar's.
pub(crate) fn page_path(path: &str) -> Result<(), String> {
    if !path.starts_with('/') || path.starts_with("//") {
        return Err(format!(
            "`{path}` is not a path on the plugin's host: it starts with one /"
        ));
    }
    let first = path[1..].split(['/', '?']).next().unwrap_or_default();
    if first == ".meridian" {
        return Err(format!(
            "`{path}` is under /.meridian, which is not the plugin's"
        ));
    }
    if path
        .chars()
        .any(|c| c.is_control() || matches!(c, '\\' | '#' | ' '))
    {
        return Err(format!("`{path}` is not a path"));
    }
    Ok(())
}

/// `GET /terminal/plugins/{instance}/page?path=`: ReadPluginPageFromTerminal.
/// The page as the person would be served it: asserted as them, forwarded
/// as a browser's request is, no redirect followed, and handed back with
/// the plugin's own status.
pub(crate) async fn page_from_terminal(
    State(app): State<Arc<App>>,
    Path(instance): Path<String>,
    Query(asked): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let now = app.clock.now_ns();
    let (who, access) = match from_terminal(&app, &headers, &instance, now) {
        Ok((who, access, _)) => (who, access),
        Err(refusal) => return *refusal,
    };
    let path = asked.get("path").map(String::as_str).unwrap_or("/");
    if let Err(why) = page_path(path) {
        return declined(StatusCode::UNPROCESSABLE_ENTITY, why);
    }
    let plugins = match running(&app, &instance).await {
        Ok(plugins) => plugins,
        Err(refusal) => return *refusal,
    };
    let claims = CallerClaims {
        subject: who.subject,
        display_name: who.display_name,
        audience_instance_id: instance.clone(),
        access,
        issued_at_ns: now,
        expires_at_ns: now + ASSERTION_NS,
        assertion_id: token(),
    };
    let assertion = match plugins.signer.sign(&claims) {
        Ok(assertion) => URL_SAFE_NO_PAD.encode(assertion.encode_to_vec()),
        Err(failed) => {
            return declined(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("the dashboard cannot vouch for anybody yet: {failed}"),
            )
        }
    };
    let answer = plugins
        .client
        .get(format!("{}{path}", plugins.front_door(&instance)))
        .header(CALLER, assertion)
        .header(ACCEPT, "text/html, */*")
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await;
    let mut answer = match answer {
        Ok(answer) => answer,
        Err(failed) => {
            return declined(
                StatusCode::BAD_GATEWAY,
                format!("{instance} did not answer: {failed}"),
            )
        }
    };
    let status = answer.status().as_u16();
    let content_type = answer
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let mut bytes = Vec::new();
    loop {
        match answer.chunk().await {
            Ok(Some(chunk)) if bytes.len() + chunk.len() <= PAGE_MOST => {
                bytes.extend_from_slice(&chunk)
            }
            Ok(Some(_)) => {
                return declined(
                    StatusCode::BAD_GATEWAY,
                    format!("{path} is more than {PAGE_MOST} bytes: a page, not a download"),
                )
            }
            Ok(None) => break,
            Err(failed) => {
                return declined(
                    StatusCode::BAD_GATEWAY,
                    format!("{instance} stopped answering: {failed}"),
                )
            }
        }
    }
    let mut said = serde_json::json!({
        "instance_id": instance, "status": status, "content_type": content_type,
    });
    match String::from_utf8(bytes) {
        Ok(text) => said["body"] = text.into(),
        Err(binary) => said["body_base64"] = STANDARD.encode(binary.into_bytes()).into(),
    }
    answered(StatusCode::OK, said)
}

/// A development request as the terminal made it: which path, how, and what
/// it carried.
pub(crate) struct Development {
    pub method: Method,
    pub what: String,
    pub query: Option<String>,
    pub body: axum::body::Bytes,
}

/// Every request, before the dashboard's routes: one for a plugin's host is
/// answered here and never reaches them.
pub(crate) async fn on_plugin_host(
    State(app): State<Arc<App>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(plugins) = app.plugins.clone() else {
        return next.run(request).await;
    };
    let named = request
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .or_else(|| request.uri().authority().map(|a| a.to_string()));
    match named.map(|name| plugins.host(&name)) {
        None | Some(Host::Dashboard) => next.run(request).await,
        Some(Host::NoSuchPlugin) => said(
            StatusCode::NOT_FOUND,
            "No such plugin",
            "That is not a plugin's name.",
        ),
        Some(Host::Plugin(instance)) => serve(&app, &plugins, &instance, request).await,
    }
}

async fn serve(app: &App, plugins: &Plugins, instance: &str, request: Request) -> Response {
    let now = app.clock.now_ns();
    let records = match app.records.current(now) {
        Ok(records) => records,
        Err(stale) => return refused(&stale.to_string()),
    };

    if request.uri().path() == ENTER_PATH {
        return enter(app, plugins, instance, &request, now);
    }

    // Whose request this is: the plugin host's session, and the dashboard or
    // terminal session it came from, still live.
    let key = cookie(app, request.headers(), PLUGIN_COOKIE);
    let session = key
        .as_deref()
        .and_then(|key| plugins.entered(key, instance))
        .and_then(|came| came.who(app, now));
    let Some(session) = session else {
        if let Some(key) = &key {
            plugins.leave(key);
        }
        return again(plugins, instance, request.method());
    };

    // Evaluated now, from the records as they are now: access withdrawn a
    // moment ago is withdrawn here.
    let access =
        meridian_access::person_access(&records, &session.subject, &session.directory_groups);
    let Some(access) = opening(&access, instance) else {
        return said(
            StatusCode::FORBIDDEN,
            "No access",
            &format!("You hold no access on {instance}."),
        );
    };
    let claims = CallerClaims {
        subject: session.subject.clone(),
        display_name: session.display_name.clone(),
        audience_instance_id: instance.to_string(),
        access,
        issued_at_ns: now,
        expires_at_ns: now + ASSERTION_NS,
        assertion_id: token(),
    };
    let assertion = match plugins.signer.sign(&claims) {
        Ok(assertion) => URL_SAFE_NO_PAD.encode(assertion.encode_to_vec()),
        Err(failed) => {
            tracing::warn!(instance, "no assertion could be signed: {failed}");
            return refused(&format!(
                "the dashboard cannot vouch for anybody yet: {failed}"
            ));
        }
    };
    forward(plugins, instance, request, assertion).await
}

/// Somebody on a plugin's host with no session there, or one whose dashboard
/// session ended: a page is sent back through the dashboard, which asks them
/// to sign in if they must; anything else is refused, since a script's
/// request cannot follow a sign-in.
fn again(plugins: &Plugins, instance: &str, method: &Method) -> Response {
    if method == Method::GET || method == Method::HEAD {
        return redirect(&format!(
            "{}/plugins/{instance}",
            plugins.dashboard_origin()
        ));
    }
    said(
        StatusCode::UNAUTHORIZED,
        "Signed out",
        "Open this plugin from the dashboard again.",
    )
}

fn enter(app: &App, plugins: &Plugins, instance: &str, request: &Request, now: i64) -> Response {
    let code = request
        .uri()
        .query()
        .unwrap_or_default()
        .split('&')
        .find_map(|pair| pair.strip_prefix("code="))
        .unwrap_or_default();
    let came = plugins
        .redeem(code, instance, now)
        .filter(|came| came.is_live(&app.sessions, &app.terminals, now));
    let Some(came) = came else {
        return said(
            StatusCode::UNAUTHORIZED,
            "This link has been used",
            "It has been used already, or it is over a minute old, or it is for another \
             plugin. Open the plugin from the dashboard again.",
        );
    };
    let key = plugins.enter(came, instance);
    let mut response = redirect("/");
    response.headers_mut().insert(
        SET_COOKIE,
        set_cookie(app, PLUGIN_COOKIE, &key, "/", ABSOLUTE_NS / 1_000_000_000),
    );
    response
}

/// Headers that describe one connection rather than the request.
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// What a request may arrive claiming about who sent it, which the plugin is
/// told only by the assertion; and the host, which the sidecar's is.
const CLAIMED: [&str; 4] = ["cookie", "authorization", CALLER, "host"];

async fn forward(
    plugins: &Plugins,
    instance: &str,
    request: Request,
    assertion: String,
) -> Response {
    let (parts, body) = request.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let mut headers = HeaderMap::new();
    for (name, value) in &parts.headers {
        let name_str = name.as_str();
        if !HOP_BY_HOP.contains(&name_str) && !CLAIMED.contains(&name_str) {
            headers.append(name.clone(), value.clone());
        }
    }
    headers.insert(
        HeaderName::from_static(CALLER),
        HeaderValue::from_str(&assertion).expect("base64url is a header value"),
    );
    let answer = plugins
        .client
        .request(
            parts.method,
            format!("{}{path}", plugins.front_door(instance)),
        )
        .headers(headers)
        .body(reqwest::Body::wrap_stream(body.into_data_stream()))
        .send()
        .await;
    let answer = match answer {
        Ok(answer) => answer,
        Err(failed) => {
            tracing::warn!(instance, "the plugin's sidecar did not answer: {failed}");
            return said(
                StatusCode::BAD_GATEWAY,
                "The plugin did not answer",
                &format!("{instance} did not answer. It may be starting; try again shortly."),
            );
        }
    };
    let mut response = Response::builder().status(answer.status());
    for (name, value) in answer.headers() {
        if HOP_BY_HOP.contains(&name.as_str()) {
            continue;
        }
        if name == SET_COOKIE {
            continue;
        }
        response = response.header(name, value);
    }
    response
        .body(Body::from_stream(answer.bytes_stream()))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

#[cfg(test)]
mod tests;
