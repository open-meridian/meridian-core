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
//!
//! **A session carries the level chosen** (W6.9; the product owner,
//! 2026-09-30: "manage for admin, open for write, view for read"). The home
//! offers a button per level the person holds; `/plugins/{instance}?level=`
//! opens the plugin's area at that level, refused for a level not held, and
//! the plugin host's session carries it: every request there is asserted at
//! that level alone, its accounts cut to it -- none under `admin`, the read
//! and write sets under `write`, the read set under `read` -- after checking
//! the person still holds it.
//!
//! **The area** ([`crate::area`]; spec/plugin-pages-share-one-kit.md, Q3 and
//! Q5). `/plugins/{instance}` is a dashboard page drawing the one header --
//! the plugin's name, the way back, the person -- and one tab row, the pages
//! the plugin declared at the session's level, around the page in a seamless
//! frame, which enters the plugin's host through `/plugins/{instance}/enter`:
//! `om-framed=1` on the address, `framed: true` in a version-3 message, and
//! the frame the viewport's height under the dashboard's chrome (every page
//! fits one screen: the frame is the page's height budget), so the
//! dashboard's heading and tab row are the only ones; the page's header
//! actions (`meridian:actions`) are drawn in the area's head, and its status
//! dot (`meridian:status`, kit 0.7.0) right after the plugin's name title.
//! Under Manage, the dashboard draws two tabs itself, first, not framed:
//! Summary, the plugin's status (`crate::admin::summary_tab`), where Manage
//! opens, and Settings, its settings form (`crate::admin::settings_tab`).
//! The person's theme reaches the page as meridian-ui reads it: on first load
//! as `om-scheme`, `om-mode` and `om-direction` on the page's address, and on
//! change, and on every load of the frame, as the `meridian:theme` message
//! the header's script sends to the plugin's origin alone. A plugin's page
//! may be framed by the dashboard and by nothing else, and the dashboard's
//! own pages by nobody but the dashboard. In a window of its own
//! (`om-framed=0`), a page draws its own heading.
//!
//! **The kit** is served at `/.meridian/ui/<version>/` on every plugin host
//! (Q2), on the plugin's own origin, to anybody: it is the same static files
//! for everyone, and a page links it before anything else is asked. A page
//! asking for any `0.x` is answered with the newest `0.x` the image carries
//! ([`crate::kit`]).

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
use meridian_access::{button, level_name, parse_level, AccessLevel};
use meridian_pb::v1::CallerClaims;
use prost::Message;

use crate::clock::SECOND_NS;
use crate::delegation::Delegations;
use crate::html::{escape, page};
use crate::session::{token, Sessions, ABSOLUTE_NS};
use crate::signing::Signer;
use crate::terminal::Unavailable;
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
/// browser's on the dashboard, or a delegation's, by its id -- so no
/// client's token is kept anywhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Came {
    Browser(String),
    Delegation(String),
}

/// Who a session names, whichever kind it is, and what it covers when it
/// came through a delegation.
pub(crate) struct Who {
    pub subject: String,
    pub display_name: String,
    pub directory_groups: Vec<String>,
    pub covers: Option<crate::delegation::Covers>,
    /// The delegation and its client, when they came through one: named in
    /// the assertion, so the sidecar stamps it beside the person (W4.9,
    /// W6.18, contract v9).
    pub delegation: Option<Delegated>,
}

/// The delegation a person acted through, as the assertion names it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Delegated {
    pub id: String,
    pub client_name: String,
}

impl Who {
    /// What they may reach now, from the records as they are, cut to what
    /// the delegation covers when there is one.
    fn access(&self, records: &meridian_domain::v1::AccessRecords) -> meridian_access::Access {
        let access = meridian_access::person_access(records, &self.subject, &self.directory_groups);
        match &self.covers {
            Some(covers) => crate::delegation::narrow(access, covers, records),
            None => access,
        }
    }
}

impl Came {
    /// The person, if the session is still live; touched, since this is
    /// them using it. An error only when a terminal's could not be asked
    /// about, which is not the same as its having ended.
    async fn who(&self, app: &App, now_ns: i64) -> Result<Option<Who>, Unavailable> {
        Ok(match self {
            Came::Browser(key) => app.sessions.find(key, now_ns).map(|s| Who {
                subject: s.subject,
                display_name: s.display_name,
                directory_groups: s.directory_groups,
                covers: None,
                delegation: None,
            }),
            Came::Delegation(id) => app
                .delegations
                .delegation(id)
                .await?
                .filter(|d| {
                    d.refusal(now_ns, crate::web::oauth::groups_bound(app))
                        .is_none()
                })
                .map(|d| Who {
                    subject: d.subject,
                    display_name: d.display_name,
                    directory_groups: d.directory_groups,
                    covers: Some(d.covers),
                    delegation: Some(Delegated {
                        id: d.id,
                        client_name: d.client_name,
                    }),
                }),
        })
    }

    async fn is_live(
        &self,
        sessions: &Sessions,
        delegations: &Delegations,
        now_ns: i64,
    ) -> Result<bool, Unavailable> {
        match self {
            Came::Browser(key) => Ok(sessions.is_live(key, now_ns)),
            Came::Delegation(id) => Ok(delegations
                .delegation(id)
                .await?
                .is_some_and(|d| d.live(now_ns))),
        }
    }
}

/// What the plugin's host needs to know about a person who came from the
/// dashboard or a terminal, and the level their session was opened at.
struct Entered {
    came: Came,
    instance: String,
    level: AccessLevel,
}

struct Code {
    came: Came,
    instance: String,
    level: AccessLevel,
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

    /// Whether a browser keeps a framed page's session: only where the
    /// dashboard's host has a domain, so its plugins' hosts are the same site
    /// as it. `x.plugins.localhost` is another site from `localhost`, and a
    /// cookie set inside a frame from another site is refused; below
    /// `meridian.localhost`, or a firm's own name, it is kept.
    pub fn frames(&self) -> bool {
        self.host.contains('.')
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
            // Nothing on the plugin's accounts: developing it is not access.
            read_account_ids: Vec::new(),
            write_account_ids: Vec::new(),
            issued_at_ns: now,
            expires_at_ns: now + ASSERTION_NS,
            assertion_id: token(),
            // Only a deployment admin's terminal reaches a development path
            // (catalogue::admin), so this is said of every caller here.
            deployment_admin: true,
            // And no level: developing a plugin opens none of its pages.
            level: AccessLevel::Unspecified as i32,
            ..CallerClaims::default()
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

    fn mint(&self, came: Came, instance: &str, level: AccessLevel, now_ns: i64) -> String {
        let code = token();
        self.codes.lock().expect("code lock poisoned").insert(
            code.clone(),
            Code {
                came,
                instance: instance.to_string(),
                level,
                expires_at_ns: now_ns + CODE_NS,
            },
        );
        code
    }

    /// The session a code was minted for, and its level, if it is being
    /// presented on its own instance's host, in time. Gone once presented,
    /// whatever the answer: a code is tried once.
    fn redeem(&self, code: &str, instance: &str, now_ns: i64) -> Option<(Came, AccessLevel)> {
        let held = self
            .codes
            .lock()
            .expect("code lock poisoned")
            .remove(code)?;
        (held.instance == instance && now_ns <= held.expires_at_ns)
            .then_some((held.came, held.level))
    }

    fn enter(&self, came: Came, instance: &str, level: AccessLevel) -> String {
        let key = token();
        self.entered.lock().expect("entered lock poisoned").insert(
            key.clone(),
            Entered {
                came,
                instance: instance.to_string(),
                level,
            },
        );
        key
    }

    fn entered(&self, key: &str, instance: &str) -> Option<(Came, AccessLevel)> {
        let entered = self.entered.lock().expect("entered lock poisoned");
        let held = entered.get(key)?;
        (held.instance == instance).then(|| (held.came.clone(), held.level))
    }

    fn leave(&self, key: &str) {
        self.entered
            .lock()
            .expect("entered lock poisoned")
            .remove(key);
    }

    /// Forget codes past their minute and plugin sessions whose dashboard
    /// session or delegation has ended. One whose delegation could not be
    /// asked about is kept for the next sweep: a database briefly away has
    /// ended nobody's session.
    pub async fn sweep(&self, sessions: &Sessions, delegations: &Delegations, now_ns: i64) {
        self.codes
            .lock()
            .expect("code lock poisoned")
            .retain(|_, code| code.expires_at_ns >= now_ns);
        let held: Vec<(String, Came)> = self
            .entered
            .lock()
            .expect("entered lock poisoned")
            .iter()
            .map(|(key, held)| (key.clone(), held.came.clone()))
            .collect();
        let mut ended = Vec::new();
        for (key, came) in held {
            match came.is_live(sessions, delegations, now_ns).await {
                Ok(true) => {}
                Ok(false) => ended.push(key),
                Err(unavailable) => {
                    tracing::warn!(%unavailable, "a plugin page's session was not swept");
                }
            }
        }
        let mut entered = self.entered.lock().expect("entered lock poisoned");
        for key in ended {
            entered.remove(&key);
        }
    }
}

/// The person's theme, handed to a framed page on first load as meridian-ui
/// reads it, and whether the frame is seamless (`om-framed`). Anything the
/// kit would not take is dropped for its default, so nothing arbitrary is
/// carried into a plugin's address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Theme {
    scheme: String,
    mode: String,
    direction: String,
    /// Said either way, never left out: the kit keeps the last word for the
    /// tab, so a page on its own after a seamless one in the same tab would
    /// otherwise be drawn without its heading.
    framed: bool,
}

impl Theme {
    /// The brand's default scheme (schemes of an admin's are
    /// kernel/colour-schemes), the person's mode, green-up.
    pub(crate) fn of_mode(mode: &str) -> Theme {
        Theme::from_pairs(&HashMap::from([("om-mode".to_string(), mode.to_string())]))
    }

    /// The same, for a frame the dashboard draws the page's heading and tabs
    /// around (the plugin area's), which the kit then leaves out.
    pub(crate) fn seamless(self) -> Theme {
        Theme {
            framed: true,
            ..self
        }
    }

    fn from_pairs(asked: &HashMap<String, String>) -> Theme {
        let given = |name: &str| asked.get(name).map(String::as_str).unwrap_or_default();
        let scheme = given("om-scheme");
        let scheme_ok = (1..=64).contains(&scheme.len())
            && scheme
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        Theme {
            scheme: if scheme_ok { scheme } else { "default" }.to_string(),
            mode: match given("om-mode") {
                mode @ ("light" | "dark") => mode,
                _ => "system",
            }
            .to_string(),
            direction: match given("om-direction") {
                "red-up" => "red-up",
                _ => "green-up",
            }
            .to_string(),
            framed: given("om-framed") == "1",
        }
    }

    fn append_to(&self, url: &mut reqwest::Url) {
        url.query_pairs_mut()
            .append_pair("om-scheme", &self.scheme)
            .append_pair("om-mode", &self.mode)
            .append_pair("om-direction", &self.direction)
            .append_pair("om-framed", if self.framed { "1" } else { "0" });
    }
}

/// Where the frame enters an instance's page, at `path` on its host, at a
/// level, with the person's theme: the dashboard's own route, which mints the
/// code only once the frame asks for it.
pub(crate) fn entrance(instance: &str, path: &str, level: AccessLevel, theme: &Theme) -> String {
    let mut url = reqwest::Url::parse("http://dashboard.invalid/").expect("a fixed address");
    url.set_path(&format!("/plugins/{instance}/enter"));
    url.query_pairs_mut()
        .append_pair("path", path)
        .append_pair("level", level_name(level));
    theme.append_to(&mut url);
    match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
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

/// The level a session is opened at: the one named, if the person holds it,
/// or when none is named the first they hold, Manage before Open before View;
/// or the sentence refusing them. Nothing is minted for a level not held
/// (W6.9).
pub(crate) fn level_to_open(
    held: &meridian_access::Held,
    named: Option<&str>,
    instance: &str,
) -> Result<AccessLevel, String> {
    let named = named.map(str::trim).filter(|named| !named.is_empty());
    match named {
        None => held
            .levels()
            .first()
            .copied()
            .ok_or_else(|| format!("You hold no access on {instance}.")),
        Some(named) => match parse_level(named) {
            None => Err(format!(
                "`{named}` is not a level: it is admin, write or read (Manage, Open or View)."
            )),
            Some(level) if held.holds(level) => Ok(level),
            Some(level) => Err(if held.holds_any() {
                format!(
                    "You do not hold {} on {instance}: you hold {}.",
                    level_name(level),
                    held.levels()
                        .iter()
                        .map(|l| format!("{} ({})", button(*l), level_name(*l)))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            } else {
                format!("You hold no access on {instance}.")
            }),
        },
    }
}

/// Who may open an instance's page from the dashboard, at which level, and
/// the plugins that serve it; or the answer that refuses them. W6.9's
/// checks, in its order.
async fn may_open<'a>(
    app: &'a App,
    instance: &str,
    headers: &HeaderMap,
    named: Option<&str>,
) -> Result<
    (
        String,
        crate::session::Session,
        meridian_access::Access,
        AccessLevel,
        &'a Plugins,
    ),
    Box<Response>,
> {
    let now = app.clock.now_ns();
    let records = app
        .records
        .current(now)
        .map_err(|stale| Box::new(refused(&stale.to_string())))?;
    let (Some(key), Some(session)) = (
        cookie(app, headers, SESSION_COOKIE),
        crate::web::session_of(app, headers),
    ) else {
        return Err(Box::new(redirect("/sign-in")));
    };
    let Some(plugins) = app.plugins.as_deref() else {
        return Err(Box::new(refused(
            "this dashboard serves no plugin pages: it needs its own address \
             (MERIDIAN_DASHBOARD_URL) and where plugins' sidecars are",
        )));
    };
    if !is_instance(instance) {
        return Err(Box::new(said(
            StatusCode::NOT_FOUND,
            "No such plugin",
            "That is not a plugin's name.",
        )));
    }
    let access =
        meridian_access::person_access(&records, &session.subject, &session.directory_groups);
    let level = level_to_open(&access.held(instance), named, instance)
        .map_err(|why| Box::new(said(StatusCode::FORBIDDEN, "No access", &why)))?;
    if !plugins.runs(instance).await {
        return Err(Box::new(said(
            StatusCode::NOT_FOUND,
            "No such plugin",
            &format!("No plugin {instance} runs in this deployment."),
        )));
    }
    Ok((key, session, access, level, plugins))
}

/// `GET /plugins/{instance}?level=&tab=` on the dashboard: the plugin's
/// area at the level chosen ([`crate::area`]). The one header and one tab
/// row, the pages the plugin declared at that level, and the page asked for
/// below them in a seamless frame.
pub(crate) async fn frame(
    State(app): State<Arc<App>>,
    Path(instance): Path<String>,
    Query(asked): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let named = asked.get("level").map(String::as_str);
    let (_, session, access, level, plugins) =
        match may_open(&app, &instance, &headers, named).await {
            Ok(allowed) => allowed,
            Err(refusal) => return *refusal,
        };
    let theme = Theme::of_mode(crate::web::mode_of(&app, &headers));
    let reports = app.health.view();
    let held_roles: Vec<(String, AccessLevel)> = access
        .plugin(&instance)
        .session(level)
        .map(|session| {
            session
                .roles
                .into_iter()
                .filter(|role| !role.role.is_empty())
                .map(|role| (role.role, role.level))
                .collect()
        })
        .unwrap_or_default();
    let tabs = crate::area::tabs_by_role(
        reports.get(&instance),
        level,
        &held_roles,
        &table_tabs(&app, &instance),
    );
    // A path asked for directly is its tab's, or one of its own under the
    // level's pages; otherwise the tab asked for, or the first: under Manage,
    // the dashboard's own Summary, drawn here rather than framed. A path
    // asked for is always framed, under the first of the plugin's tabs when
    // it is none of theirs.
    let asked_path = asked
        .get("path")
        .map(String::as_str)
        .filter(|path| page_path(path).is_ok());
    let frameable = |tab: &&crate::area::Tab| !(tab.drawn && asked_path.is_some());
    let current = asked
        .get("tab")
        .and_then(|key| tabs.iter().find(|tab| &tab.key == key))
        .filter(frameable)
        .or_else(|| asked_path.and_then(|path| tabs.iter().find(|tab| tab.path == path)))
        .or_else(|| tabs.iter().find(frameable))
        .or_else(|| tabs.first());
    let drawn = asked_path.is_none() && current.is_some_and(|tab| tab.drawn);
    let path = if drawn {
        None
    } else {
        asked_path
            .or(current.map(|tab| tab.path.as_str()))
            .map(str::to_string)
    };
    // Where no frame can hold the page's session, its window is its own.
    if !plugins.frames() {
        if let Some(path) = &path {
            return redirect(&entrance(&instance, path, level, &theme));
        }
    }
    let launches = crate::catalogue::launches(&app).await;
    let launch = launches
        .iter()
        .find(|launch| launch.instance_id == instance);
    let name = launch.map_or(instance.as_str(), |launch| launch.name.as_str());
    let shown = match &path {
        Some(path) => crate::area::Shown::Framed {
            src: entrance(&instance, path, level, &theme.clone().seamless()),
            origin: plugins.origin(&instance),
        },
        // The dashboard's Summary or Settings, under Manage alone: may_open
        // has held the session to a level the person holds, and only `admin`
        // has the tabs.
        None => {
            let now = app.clock.now_ns();
            let records = match app.records.current(now) {
                Ok(records) => records,
                Err(stale) => return refused(&stale.to_string()),
            };
            let notice = match asked.get("saved").map(String::as_str) {
                Some("1") => "Saved.",
                Some("none") => "Nothing was changed.",
                _ => "",
            };
            let choices = match records
                .plugin_settings
                .iter()
                .find(|record| record.plugin_instance_id == instance)
            {
                Some(record) => crate::admin::choices(&app, &instance, record, &records).await,
                None => crate::admin::settings::Choices::default(),
            };
            let manage = crate::admin::Manage {
                instance: &instance,
                choices: &choices,
                records: &records,
                report: reports.get(&instance),
                version: launch.map(|launch| launch.version.as_str()),
                session: &session,
                notice,
                now,
            };
            crate::area::Shown::Drawn(match current.map(|tab| tab.key.as_str()) {
                Some(crate::area::SETTINGS) => crate::admin::settings_tab(&manage),
                Some(crate::area::SUMMARY) | None => crate::admin::summary_tab(&manage),
                // A table setting's tab, the only other the dashboard draws.
                Some(key) => crate::admin::table_tab(&manage, key),
            })
        }
    };
    let held = access.held(&instance);
    // Under Manage, the plugin's health is the dot on every tab where no page
    // tells its own (the product owner, 2026-10-01: "yes, dot on every tab").
    let health = crate::health::state(reports.get(&instance), app.clock.now_ns());
    let body = crate::area::render(&crate::area::Area {
        instance: &instance,
        name,
        held: &held,
        level,
        tabs: &tabs,
        current,
        shown,
        health: (level == AccessLevel::Admin).then_some(&health),
    });
    let crumbs = format!(
        "{}{}",
        crate::html::crumb_link("/", "Home"),
        crate::html::crumb_here(name, Some(&instance)),
    );
    Html(crate::html::page_with(
        &format!("{name} · {}", button(level)),
        &body,
        &crate::html::Chrome {
            viewer: Some(crate::html::Viewer {
                display_name: &session.display_name,
                form_token: &session.form_token,
                admin: access.deployment_admin,
            }),
            crumbs,
            main: "page",
            in_admin: false,
            // The area's head carries it, with the plugin filled in.
            report: crate::html::Report::Omitted,
        },
    ))
    .into_response()
}

/// The plugin's table settings as tabs of its area under Manage, each its
/// key and label; none while its settings are not known.
fn table_tabs(app: &App, instance: &str) -> Vec<(String, String)> {
    let Ok(records) = app.records.current(app.clock.now_ns()) else {
        return Vec::new();
    };
    records
        .plugin_settings
        .iter()
        .find(|record| record.plugin_instance_id == instance)
        .map(|record| {
            crate::admin::settings::tables(record, crate::html::is_development())
                .into_iter()
                .map(|table| {
                    (
                        crate::admin::settings::table_key(table),
                        crate::admin::settings::label(table).to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Where a person entering at `level` with no page named lands: the first
/// page the plugin declares at that level, the area's first framed tab
/// there (under Manage, the one after the dashboard's own Summary and
/// Settings), and its `/` only where it declares none. A `/` serving Open
/// and View alone would refuse Manage with the plugin's 403.
fn first_page(app: &App, instance: &str, level: AccessLevel) -> String {
    // The dashboard's own tabs, its table settings' among them, frame nothing.
    crate::area::tabs(app.health.view().get(instance), level, &[])
        .into_iter()
        .find(|tab| !tab.drawn)
        .map(|tab| tab.path)
        .unwrap_or_else(|| "/".into())
}

/// The plugin's host's way in for a code, carrying on to `path` there.
fn enter_url(
    plugins: &Plugins,
    instance: &str,
    code: &str,
    path: &str,
) -> Result<reqwest::Url, String> {
    let mut url = reqwest::Url::parse(&format!("{}{ENTER_PATH}", plugins.origin(instance)))
        .map_err(|failed| format!("{instance}'s address is not one: {failed}"))?;
    url.query_pairs_mut().append_pair("code", code);
    if path != "/" {
        url.query_pairs_mut().append_pair("path", path);
    }
    Ok(url)
}

/// `GET /plugins/{instance}/enter?path=&level=` on the dashboard:
/// EnterPlugin. A one-time code for the instance's host, at the level named,
/// carrying on to `path` there -- with none named, the first page at that
/// level ([`first_page`]) -- with the theme, which is what the frame loads;
/// opened on its own, the page unframed.
pub(crate) async fn open(
    State(app): State<Arc<App>>,
    Path(instance): Path<String>,
    Query(asked): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let named = asked.get("level").map(String::as_str);
    let (key, _, _, level, plugins) = match may_open(&app, &instance, &headers, named).await {
        Ok(allowed) => allowed,
        Err(refusal) => return *refusal,
    };
    let path = match asked.get("path") {
        Some(path) => path.clone(),
        None => first_page(&app, &instance, level),
    };
    if let Err(why) = page_path(&path) {
        return said(StatusCode::BAD_REQUEST, "Not a page", &why);
    }
    let code = plugins.mint(Came::Browser(key), &instance, level, app.clock.now_ns());
    let mut url = match enter_url(plugins, &instance, &code, &path) {
        Ok(url) => url,
        Err(why) => return refused(&why),
    };
    if asked.keys().any(|name| name.starts_with("om-")) {
        Theme::from_pairs(&asked).append_to(&mut url);
    }
    redirect(url.as_str())
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

/// What a person carries onto a plugin's page: the level the session was
/// opened at, and what that level reaches.
pub(crate) struct Opening {
    /// The accounts the level reaches, cut to it: none under `admin`, the
    /// read and write sets under `write`, the read set under `read`; the
    /// union over the plugin's roles.
    pub access: meridian_access::Levels,
    /// Their level and accounts on each of the plugin's roles within the
    /// button (W6.9, contract v15): one entry per role held, none for a
    /// plugin holding no role.
    pub roles: Vec<meridian_access::RoleSession>,
    /// Whether they are a deployment admin, which a plugin's page at `admin`
    /// asks when linking, a new account being theirs to name (W6.4).
    pub deployment_admin: bool,
    pub level: AccessLevel,
}

impl Opening {
    /// The assertion's claims, for this instance alone, at the session's
    /// level, for 60 seconds, once.
    fn claims(self, who: &Who, instance: &str, now: i64) -> CallerClaims {
        CallerClaims {
            subject: who.subject.clone(),
            display_name: who.display_name.clone(),
            audience_instance_id: instance.to_string(),
            read_account_ids: self.access.read_account_ids(),
            write_account_ids: self.access.write_account_ids(),
            issued_at_ns: now,
            expires_at_ns: now + ASSERTION_NS,
            assertion_id: token(),
            deployment_admin: self.deployment_admin,
            level: self.level as i32,
            // Through a client, the delegation and its name; a browser's
            // session names neither (W6.18, decisions/029).
            delegation_id: who
                .delegation
                .as_ref()
                .map(|d| d.id.clone())
                .unwrap_or_default(),
            client_name: who
                .delegation
                .as_ref()
                .map(|d| d.client_name.clone())
                .unwrap_or_default(),
            // A page's request names no tool: only `/mcp` sets one (W6.20).
            tool_name: String::new(),
            // Each role's level and accounts within the button, as
            // positions in the read set (contract v15, the plan's Q2), so
            // the sidecar admits a command by the role holding it whether or
            // not the plugin reads roles. None for a role-less plugin.
            roles: self
                .roles
                .iter()
                .filter(|role| !role.role.is_empty())
                .map(|role| {
                    meridian_access::role_access(
                        &role.role,
                        role.level,
                        &role.accounts,
                        &self.access,
                    )
                })
                .collect(),
        }
    }
}

/// What a person carries onto a plugin's page at `level`, if they hold it
/// there: the accounts it reaches and nothing more. A deployment admin holds
/// on a plugin what their grants give them, as anybody (the product owner,
/// 2026-09-30, superseding the spec's ruling 19).
pub(crate) fn opening(
    access: &meridian_access::Access,
    instance: &str,
    level: AccessLevel,
) -> Option<Opening> {
    access
        .plugin(instance)
        .session(level)
        .map(|session| Opening {
            access: session.accounts,
            roles: session.roles,
            deployment_admin: access.deployment_admin,
            level,
        })
}

// ── A tool's call through the deployment's MCP surface (W6.20) ─────────

/// The most of a plugin's answer to a tool's call the dashboard reads: a
/// larger read is paged (W6.20, Q7).
pub const TOOL_ANSWER_MOST: usize = 1 << 20;

/// How long a plugin has to answer a tool's call.
pub const TOOL_ANSWER_WITHIN: std::time::Duration = std::time::Duration::from_secs(30);

/// Who a tool's call acts for: the person, the delegation and its client.
pub(crate) struct ToolCaller<'a> {
    pub subject: &'a str,
    pub display_name: &'a str,
    pub delegation_id: &'a str,
    pub client_name: &'a str,
}

impl Plugins {
    /// CallPluginTool: the request the tool's route would receive from a
    /// page, at the level opened, with the assertion naming the delegation,
    /// its client and the tool, and the arguments as JSON; the plugin's typed
    /// answer back, or a refusal saying why there is none. No bearer token
    /// crosses: the sidecar sees the assertion alone.
    pub(crate) async fn call_tool(
        &self,
        instance: &str,
        caller: ToolCaller<'_>,
        opened: Opening,
        tool: &meridian_pb::v1::ToolDeclaration,
        arguments: serde_json::Value,
        now: i64,
    ) -> serde_json::Value {
        let refused = |reason: &str, detail: String| serde_json::json!({"outcome": "refused", "reason": reason, "fields": [], "detail": detail});
        if !self.runs(instance).await {
            return refused(
                "not_running",
                format!("no plugin {instance} runs in this deployment"),
            );
        }
        let who = Who {
            subject: caller.subject.to_string(),
            display_name: caller.display_name.to_string(),
            directory_groups: Vec::new(),
            covers: None,
            delegation: Some(Delegated {
                id: caller.delegation_id.to_string(),
                client_name: caller.client_name.to_string(),
            }),
        };
        let mut claims = opened.claims(&who, instance, now);
        claims.tool_name = tool.name.clone();
        let assertion = match self.signer.sign(&claims) {
            Ok(assertion) => URL_SAFE_NO_PAD.encode(assertion.encode_to_vec()),
            Err(failed) => {
                return refused(
                    "unavailable",
                    format!("the dashboard cannot vouch for anybody yet: {failed}"),
                )
            }
        };
        let Ok(method) = Method::from_bytes(tool.method.as_bytes()) else {
            return refused("unavailable", format!("{} is not a method", tool.method));
        };
        let answer = self
            .client
            .request(
                method,
                format!("{}{}", self.front_door(instance), tool.path),
            )
            .header(CALLER, assertion)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .timeout(TOOL_ANSWER_WITHIN)
            .body(arguments.to_string())
            .send()
            .await;
        let mut answer = match answer {
            Ok(answer) => answer,
            Err(failed) if failed.is_timeout() => {
                return refused(
                    "unanswered",
                    format!(
                        "{instance} did not answer within {} seconds",
                        TOOL_ANSWER_WITHIN.as_secs()
                    ),
                )
            }
            Err(failed) => {
                return refused("unanswered", format!("{instance} did not answer: {failed}"))
            }
        };
        let status = answer.status();
        let typed = answer
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|kind| kind.starts_with("application/json"));
        let mut bytes = Vec::new();
        loop {
            match answer.chunk().await {
                Ok(Some(chunk)) if bytes.len() + chunk.len() <= TOOL_ANSWER_MOST => {
                    bytes.extend_from_slice(&chunk)
                }
                Ok(Some(_)) => {
                    return refused(
                        "too_large",
                        format!("{instance} answered more than {TOOL_ANSWER_MOST} bytes: a larger read is paged"),
                    )
                }
                Ok(None) => break,
                Err(failed) => return refused("unanswered", format!("{instance} stopped answering: {failed}")),
            }
        }
        if typed {
            if let Ok(said) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                if said.get("outcome").and_then(|o| o.as_str()).is_some() {
                    return said;
                }
            }
        }
        // Not typed: the sidecar's own refusal, or a plugin answering a page.
        let text = String::from_utf8_lossy(&bytes);
        let said: String = text.chars().take(1000).collect();
        if status.is_success() {
            refused(
                "untyped_answer",
                format!("{instance} answered no typed data: {said}"),
            )
        } else {
            refused(
                "refused",
                format!("{instance} answered {}: {said}", status.as_u16()),
            )
        }
    }
}

// ── From a terminal (W6.15) ─────────────────────────────────────────────

fn answered(status: StatusCode, body: serde_json::Value) -> Response {
    (status, [(CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

fn declined(status: StatusCode, reason: impl Into<String>) -> Response {
    answered(status, serde_json::json!({ "error": reason.into() }))
}

/// The person acting from a terminal, what they hold on the plugin at the
/// level the request names as opening it from the dashboard would find it
/// (W6.9's checks; W6.15) -- cut to what their delegation covers, when they
/// act on one -- and the session a plugin-host session opened for them would
/// end with. A request naming no level opens at the first held, as the
/// home's first button does.
async fn from_terminal(
    app: &App,
    headers: &HeaderMap,
    instance: &str,
    named: Option<&str>,
    now: i64,
) -> Result<(Who, Opening, Came), Box<Response>> {
    let caller = crate::web::caller_of(app, headers).await?;
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
    let access = caller.access(&records);
    let level = match level_to_open(&access.held(instance), named, instance) {
        Ok(level) => level,
        Err(why) => {
            caller.refused(app, &why).await;
            return Err(Box::new(declined(StatusCode::FORBIDDEN, why)));
        }
    };
    let Some(opened) = opening(&access, instance, level) else {
        return Err(Box::new(declined(
            StatusCode::FORBIDDEN,
            format!("you hold no access on {instance}"),
        )));
    };
    let (came, covers, delegation) = match caller.through {
        crate::web::Through::Delegation {
            id,
            client_name,
            covers,
        } => (
            Came::Delegation(id.clone()),
            Some(covers),
            Some(Delegated { id, client_name }),
        ),
    };
    let who = Who {
        subject: caller.person.subject,
        display_name: caller.person.display_name,
        directory_groups: caller.person.directory_groups,
        covers,
        delegation,
    };
    Ok((who, opened, came))
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

/// `POST /terminal/plugins/{instance}/open?level=`: OpenPluginFromTerminal.
/// The code `/plugins/{instance}` would mint at the level named, bound to the
/// delegation: whichever browser opens the link first enters that plugin's
/// host alone, at that level, until the delegation ends,
/// landing where the area would at that level ([`first_page`]).
pub(crate) async fn open_from_terminal(
    State(app): State<Arc<App>>,
    Path(instance): Path<String>,
    Query(asked): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let now = app.clock.now_ns();
    let named = asked.get("level").map(String::as_str);
    let (came, level) = match from_terminal(&app, &headers, &instance, named, now).await {
        Ok((_, opened, came)) => (came, opened.level),
        Err(refusal) => return *refusal,
    };
    let plugins = match running(&app, &instance).await {
        Ok(plugins) => plugins,
        Err(refusal) => return *refusal,
    };
    let code = plugins.mint(came, &instance, level, now);
    let landing = first_page(&app, &instance, level);
    let url = match enter_url(plugins, &instance, &code, &landing) {
        Ok(url) => url,
        Err(why) => return declined(StatusCode::SERVICE_UNAVAILABLE, why),
    };
    answered(
        StatusCode::OK,
        serde_json::json!({
            "instance_id": instance,
            "level": level_name(level),
            "url": url.as_str(),
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

/// `GET /terminal/plugins/{instance}/page?path=&level=`:
/// ReadPluginPageFromTerminal. The page as the person would be served it at
/// the level named: asserted as them, at that level, forwarded as a
/// browser's request is, no redirect followed, and handed back with the
/// plugin's own status.
pub(crate) async fn page_from_terminal(
    State(app): State<Arc<App>>,
    Path(instance): Path<String>,
    Query(asked): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let now = app.clock.now_ns();
    let named = asked.get("level").map(String::as_str);
    let (who, opened) = match from_terminal(&app, &headers, &instance, named, now).await {
        Ok((who, opened, _)) => (who, opened),
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
    let level = opened.level;
    let claims = opened.claims(&who, &instance, now);
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
        "level": level_name(level),
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
    // The icon the dashboard's pages here link, before anything about who
    // asks, as the kit is.
    if crate::brand::is_icon(request.uri().path()) {
        return crate::brand::serve(request.method(), request.uri().path());
    }
    // The kit, on the plugin's own origin, before anything about who asks.
    if request.uri().path().starts_with(crate::kit::PATH) {
        return match &app.kit {
            Some(kit) => kit.serve(request.method(), request.uri().path()).await,
            None => said(
                StatusCode::NOT_FOUND,
                "No kit",
                "This dashboard carries no UI kit.",
            ),
        };
    }
    let now = app.clock.now_ns();
    let records = match app.records.current(now) {
        Ok(records) => records,
        Err(stale) => return refused(&stale.to_string()),
    };

    if request.uri().path() == ENTER_PATH {
        // By its address alone: a request's body is not Sync, so a borrow
        // of the whole request cannot be held across the store's answer.
        let uri = request.uri().clone();
        return enter(app, plugins, instance, &uri, now).await;
    }

    // Whose request this is: the plugin host's session, and the dashboard
    // session or delegation it came from, still live.
    let key = cookie(app, request.headers(), PLUGIN_COOKIE);
    let came = key
        .as_deref()
        .and_then(|key| plugins.entered(key, instance));
    let level = came.as_ref().map(|(_, level)| *level);
    let session = match came {
        Some((came, _)) => match came.who(app, now).await {
            Ok(who) => who,
            Err(unavailable) => return refused(&unavailable.to_string()),
        },
        None => None,
    };
    let (Some(session), Some(level)) = (session, level) else {
        if let Some(key) = &key {
            plugins.leave(key);
        }
        return again(plugins, instance, level, &request);
    };

    // Evaluated now, from the records as they are now, at the session's
    // level, cut to what a delegation covers when it came through one:
    // access withdrawn a moment ago is withdrawn here, and a level no longer
    // held opens nothing.
    let access = session.access(&records);
    let Some(opened) = opening(&access, instance, level) else {
        return said(
            StatusCode::FORBIDDEN,
            "No access",
            &format!(
                "You no longer hold {} on {instance}. Open it from the dashboard again.",
                level_name(level)
            ),
        );
    };
    let claims = opened.claims(&session, instance, now);
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
/// request cannot follow a sign-in. A page loaded in a window of its own goes
/// back to the frame; one in the frame, to the frame's way in, so it comes
/// back at the page it was on rather than the frame inside itself.
fn again(
    plugins: &Plugins,
    instance: &str,
    level: Option<AccessLevel>,
    request: &Request,
) -> Response {
    let method = request.method();
    if method != Method::GET && method != Method::HEAD {
        return said(
            StatusCode::UNAUTHORIZED,
            "Signed out",
            "Open this plugin from the dashboard again.",
        );
    }
    let in_a_window = request
        .headers()
        .get("sec-fetch-dest")
        .is_some_and(|dest| dest == "document");
    let path = request
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .filter(|path| page_path(path).is_ok())
        .unwrap_or("/");
    let back = if in_a_window {
        format!("{}/plugins/{instance}", plugins.dashboard_origin())
    } else {
        format!("{}/plugins/{instance}/enter", plugins.dashboard_origin())
    };
    let mut url = match reqwest::Url::parse(&back) {
        Ok(url) => url,
        Err(_) => return redirect(&format!("/plugins/{instance}")),
    };
    if path != "/" {
        url.query_pairs_mut().append_pair("path", path);
    }
    if let Some(level) = level {
        url.query_pairs_mut()
            .append_pair("level", level_name(level));
    }
    redirect(url.as_str())
}

async fn enter(
    app: &App,
    plugins: &Plugins,
    instance: &str,
    uri: &axum::http::Uri,
    now: i64,
) -> Response {
    let asked = Query::<HashMap<String, String>>::try_from_uri(uri)
        .map(|Query(asked)| asked)
        .unwrap_or_default();
    let code = asked.get("code").map(String::as_str).unwrap_or_default();
    let came = match plugins.redeem(code, instance, now) {
        Some((came, level)) => match came.is_live(&app.sessions, &app.delegations, now).await {
            Ok(true) => Some((came, level)),
            Ok(false) => None,
            Err(unavailable) => return refused(&unavailable.to_string()),
        },
        None => None,
    };
    let Some((came, level)) = came else {
        return said(
            StatusCode::UNAUTHORIZED,
            "This link has been used",
            "It has been used already, or it is over a minute old, or it is for another \
             plugin. Open the plugin from the dashboard again.",
        );
    };
    // One session per plugin host in a browser: entering again, at any
    // level, replaces the last.
    let key = plugins.enter(came, instance, level);
    let mut response = redirect(&landing(&asked));
    response.headers_mut().insert(
        SET_COOKIE,
        set_cookie(app, PLUGIN_COOKIE, &key, "/", ABSOLUTE_NS / 1_000_000_000),
    );
    response
}

/// Where a redeemed code lands on the plugin's host: the page asked for, a
/// path there and nowhere else, carrying the theme when the frame gave one.
fn landing(asked: &HashMap<String, String>) -> String {
    let path = asked
        .get("path")
        .map(String::as_str)
        .filter(|path| page_path(path).is_ok())
        .unwrap_or("/");
    if !asked.keys().any(|name| name.starts_with("om-")) {
        return path.to_string();
    }
    let mut url = reqwest::Url::parse("http://plugin.invalid/").expect("a fixed address");
    let (only, query) = path.split_once('?').unwrap_or((path, ""));
    url.set_path(only);
    url.set_query((!query.is_empty()).then_some(query));
    Theme::from_pairs(asked).append_to(&mut url);
    match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    }
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
    // Framed by the dashboard, and by nothing else.
    if let Ok(ancestors) =
        HeaderValue::from_str(&format!("frame-ancestors {}", plugins.dashboard_origin()))
    {
        response = response.header(axum::http::header::CONTENT_SECURITY_POLICY, ancestors);
    }
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
