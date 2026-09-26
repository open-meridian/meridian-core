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
use axum::extract::{Path, Request, State};
use axum::http::header::{HOST, SET_COOKIE};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use meridian_pb::v1::CallerClaims;
use prost::Message;

use crate::clock::SECOND_NS;
use crate::html::{escape, page};
use crate::session::{token, Sessions, ABSOLUTE_NS};
use crate::signing::Signer;
use crate::web::{cookie, redirect, refused, set_cookie, App, SESSION_COOKIE};

/// The plugin host's own session.
pub const PLUGIN_COOKIE: &str = "meridian_plugin_session";
/// Where a plugin's host redeems a code. Under a path no plugin would choose,
/// since everything else on the host is the plugin's.
pub const ENTER_PATH: &str = "/.meridian/enter";
/// The header the sidecar verifies (plans/a-person-reaches-a-plugin, ruling 2).
pub const CALLER: &str = "meridian-caller";

/// How long a code and an assertion live.
const CODE_NS: i64 = 60 * SECOND_NS;
const ASSERTION_NS: i64 = 60 * SECOND_NS;

/// What the plugin's host needs to know about a person who came from the
/// dashboard.
struct Entered {
    /// The dashboard session this one came from, which it ends with.
    session_key: String,
    instance: String,
}

struct Code {
    session_key: String,
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

    fn mint(&self, session_key: &str, instance: &str, now_ns: i64) -> String {
        let code = token();
        self.codes.lock().expect("code lock poisoned").insert(
            code.clone(),
            Code {
                session_key: session_key.to_string(),
                instance: instance.to_string(),
                expires_at_ns: now_ns + CODE_NS,
            },
        );
        code
    }

    /// The dashboard session a code was minted for, if it is being presented
    /// on its own instance's host, in time. Gone once presented, whatever the
    /// answer: a code is tried once.
    fn redeem(&self, code: &str, instance: &str, now_ns: i64) -> Option<String> {
        let held = self
            .codes
            .lock()
            .expect("code lock poisoned")
            .remove(code)?;
        (held.instance == instance && now_ns <= held.expires_at_ns).then_some(held.session_key)
    }

    fn enter(&self, session_key: String, instance: &str) -> String {
        let key = token();
        self.entered.lock().expect("entered lock poisoned").insert(
            key.clone(),
            Entered {
                session_key,
                instance: instance.to_string(),
            },
        );
        key
    }

    fn entered(&self, key: &str, instance: &str) -> Option<String> {
        let entered = self.entered.lock().expect("entered lock poisoned");
        let held = entered.get(key)?;
        (held.instance == instance).then(|| held.session_key.clone())
    }

    fn leave(&self, key: &str) {
        self.entered
            .lock()
            .expect("entered lock poisoned")
            .remove(key);
    }

    /// Forget codes past their minute and plugin sessions whose dashboard
    /// session has ended.
    pub fn sweep(&self, sessions: &Sessions, now_ns: i64) {
        self.codes
            .lock()
            .expect("code lock poisoned")
            .retain(|_, code| code.expires_at_ns >= now_ns);
        self.entered
            .lock()
            .expect("entered lock poisoned")
            .retain(|_, held| sessions.is_live(&held.session_key, now_ns));
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
        meridian_access::person_access(&records, &session.subject, &session.directory_groups)
            .on_plugin(&instance);
    if access.is_empty() {
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
    let code = plugins.mint(&key, &instance, now);
    redirect(&format!(
        "{}{ENTER_PATH}?code={code}",
        plugins.origin(&instance)
    ))
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

    // Whose request this is: the plugin host's session, and the dashboard
    // session it came from, still live.
    let key = cookie(app, request.headers(), PLUGIN_COOKIE);
    let dashboard_key = key
        .as_deref()
        .and_then(|key| plugins.entered(key, instance));
    let session = dashboard_key.and_then(|dashboard| app.sessions.find(&dashboard, now));
    let Some(session) = session else {
        if let Some(key) = &key {
            plugins.leave(key);
        }
        return again(plugins, instance, request.method());
    };

    // Evaluated now, from the records as they are now: access withdrawn a
    // moment ago is withdrawn here.
    let access =
        meridian_access::person_access(&records, &session.subject, &session.directory_groups)
            .on_plugin(instance);
    if access.is_empty() {
        return said(
            StatusCode::FORBIDDEN,
            "No access",
            &format!("You hold no access on {instance}."),
        );
    }
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
    let session_key = plugins
        .redeem(code, instance, now)
        .filter(|key| app.sessions.is_live(key, now));
    let Some(session_key) = session_key else {
        return said(
            StatusCode::UNAUTHORIZED,
            "This link has been used",
            "It has been used already, or it is over a minute old, or it is for another \
             plugin. Open the plugin from the dashboard again.",
        );
    };
    let key = plugins.enter(session_key, instance);
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
