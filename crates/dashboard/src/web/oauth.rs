//! W6.17 over HTTP: the dashboard's own OAuth endpoints (decisions/029;
//! spec/clients-act-on-a-persons-delegation, "The endpoints").
//!
//! Registration (RFC 7591), authorisation by the code flow with PKCE, S256
//! only, a token endpoint taking a code or a refresh token, revocation
//! (RFC 7009), and the two metadata documents a client discovers them by
//! (RFC 8414, RFC 9728). No implicit flow, no password grant, no client
//! credentials, no ID token and no OpenID discovery: this issues tokens for
//! the deployment's own resources and signs nobody in to anything, so it is
//! not the identity server decisions/018 keeps out. The sign-in before
//! consent is W6.1's, unchanged.
//!
//! `/oauth/register`, `/oauth/token` and `/oauth/revoke` read no cookie;
//! `/oauth/authorize` is a browser's page, the sign-in and the consent, and
//! reads no bearer token. A token is accepted only on `/terminal/` paths
//! ([`super::terminal::caller_of`]).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Form, Query, State};
use axum::http::header::{CACHE_CONTROL, HOST, LOCATION, PRAGMA, SET_COOKIE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use meridian_access::{button, level_name, person_access, AccessLevel};

use super::{
    password_page, record_sign_in, redirect, refused, set_cookie, App, For, SIGN_IN_COOKIE,
};
use crate::admin::picker::{self, Choice};
use crate::delegation::{
    check_asked, listed, Asked, Consent, Covers, Delegation, Refusal, Registration, Resource, DAYS,
    DAY_NS, GROUPS_BOUND_NS, LEVELS_DOWN, NOTICE_NS,
};
use crate::html::{escape, page};
use crate::terminal::{rfc3339, Person};

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/.well-known/oauth-authorization-server", get(metadata))
        .route(
            "/.well-known/oauth-protected-resource/terminal",
            get(terminal_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(mcp_metadata),
        )
        .route("/oauth/register", post(register))
        .route("/oauth/authorize", get(authorize).post(decide))
        .route("/oauth/token", post(token))
        .route("/oauth/revoke", post(revoke))
}

/// This dashboard's own address: its configured public address, or where a
/// development one was reached.
pub(crate) fn issuer(app: &App, headers: &HeaderMap) -> String {
    let given = if app.public_url.is_empty() {
        let host = headers
            .get(HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("localhost");
        let scheme = if app.secure_cookies { "https" } else { "http" };
        format!("{scheme}://{host}")
    } else {
        app.public_url.clone()
    };
    // As an origin, so it compares with a resource indicator's however
    // either was written: case, a default port, a trailing slash.
    match reqwest::Url::parse(&given) {
        Ok(url) if url.has_host() => url.origin().ascii_serialization(),
        _ => given.trim_end_matches('/').to_string(),
    }
}

/// The provider branch's bound on a delegation's directory groups
/// (requirement 7); none on the other two, whose groups the dashboard reads
/// for itself.
pub(crate) fn groups_bound(app: &App) -> Option<i64> {
    app.oidc.is_some().then_some(GROUPS_BOUND_NS)
}

fn json(status: StatusCode, body: serde_json::Value) -> Response {
    // Never cached: a token is in one of these, and a refusal is about now.
    (
        status,
        [(CACHE_CONTROL, "no-store"), (PRAGMA, "no-cache")],
        Json(body),
    )
        .into_response()
}

fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    json(
        status,
        serde_json::json!({ "error": error, "error_description": description }),
    )
}

fn unavailable(unavailable: crate::terminal::Unavailable) -> Response {
    tracing::error!(%unavailable, "delegations could not be read");
    oauth_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "temporarily_unavailable",
        &unavailable.to_string(),
    )
}

/// A refusal of a grant, naming why so an unsupervised client can report
/// what stopped it.
fn refused_grant(refusal: Refusal) -> Response {
    json(
        StatusCode::BAD_REQUEST,
        serde_json::json!({
            "error": "invalid_grant",
            "error_description": refusal.sentence(),
            "reason": refusal.reason(),
        }),
    )
}

// ── Metadata ──────────────────────────────────────────────────────────────

/// RFC 8414: a transport over W6.17's rows.
async fn metadata(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let issuer = issuer(&app, &headers);
    json(
        StatusCode::OK,
        serde_json::json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/oauth/authorize"),
            "token_endpoint": format!("{issuer}/oauth/token"),
            "registration_endpoint": format!("{issuer}/oauth/register"),
            "revocation_endpoint": format!("{issuer}/oauth/revoke"),
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["none"],
            "revocation_endpoint_auth_methods_supported": ["none"],
            "authorization_response_iss_parameter_supported": true,
        }),
    )
}

/// RFC 9728, for the CLI's surface: which authorisation server issues its
/// tokens.
async fn terminal_metadata(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    resource_metadata(&app, &headers, Resource::Terminal)
}

/// RFC 9728, for the deployment's MCP surface (W6.20, contract v12): the
/// dashboard issues its tokens, as it does the CLI's.
async fn mcp_metadata(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    resource_metadata(&app, &headers, Resource::Mcp)
}

fn resource_metadata(app: &App, headers: &HeaderMap, resource: Resource) -> Response {
    let issuer = issuer(app, headers);
    json(
        StatusCode::OK,
        serde_json::json!({
            "resource": format!("{issuer}{}", resource.path()),
            "authorization_servers": [issuer],
            "bearer_methods_supported": ["header"],
        }),
    )
}

// ── Registration ──────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct Registering {
    #[serde(default)]
    client_name: String,
    #[serde(default)]
    redirect_uris: Vec<String>,
    #[serde(default)]
    software_id: String,
    #[serde(default)]
    grant_types: Option<Vec<String>>,
    #[serde(default)]
    response_types: Option<Vec<String>>,
    #[serde(default)]
    token_endpoint_auth_method: Option<String>,
}

/// RFC 7591. Registration grants nothing: a client can do nothing until a
/// person consents to it.
async fn register(State(app): State<Arc<App>>, body: Bytes) -> Response {
    if app.first_run {
        return oauth_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "temporarily_unavailable",
            "this deployment is not set up yet, so nobody can consent to a client",
        );
    }
    let asked: Registering = match serde_json::from_slice(&body) {
        Ok(asked) => asked,
        Err(failed) => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_client_metadata",
                &format!("the registration does not read as JSON: {failed}"),
            )
        }
    };
    if asked
        .token_endpoint_auth_method
        .as_deref()
        .is_some_and(|method| method != "none")
    {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_client_metadata",
            "every client here is public: token_endpoint_auth_method is none",
        );
    }
    if asked.grant_types.as_ref().is_some_and(|grants| {
        grants
            .iter()
            .any(|g| g != "authorization_code" && g != "refresh_token")
    }) {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_client_metadata",
            "grant_types are authorization_code and refresh_token, and nothing else",
        );
    }
    if asked
        .response_types
        .as_ref()
        .is_some_and(|types| types.iter().any(|t| t != "code"))
    {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_client_metadata",
            "response_types is code, and nothing else",
        );
    }
    let now = app.clock.now_ns();
    let registration = Registration {
        name: asked.client_name,
        redirect_uris: asked.redirect_uris,
        software_id: asked.software_id,
    };
    match app.delegations.register(registration, now).await {
        Err(failed) => unavailable(failed),
        Ok(Err(why)) => oauth_error(StatusCode::BAD_REQUEST, "invalid_client_metadata", &why),
        Ok(Ok(client)) => {
            tracing::info!(client_id = %client.client_id, name = %client.name, "a client registered");
            json(
                StatusCode::CREATED,
                serde_json::json!({
                    "client_id": client.client_id,
                    "client_id_issued_at": now / 1_000_000_000,
                    "client_name": client.name,
                    "redirect_uris": client.redirect_uris,
                    "software_id": client.software_id,
                    "grant_types": ["authorization_code", "refresh_token"],
                    "response_types": ["code"],
                    "token_endpoint_auth_method": "none",
                }),
            )
        }
    }
}

// ── Authorisation and consent ─────────────────────────────────────────────

/// A refusal on a page, sending nothing anywhere: what an unknown client or
/// redirect address gets, since that is exactly where a code must not go.
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

/// Back to a client that is known and asked to be sent there, with what
/// went wrong or what it was given.
fn back_to(redirect_uri: &str, pairs: &[(&str, &str)]) -> Response {
    match reqwest::Url::parse(redirect_uri) {
        Ok(mut url) => {
            {
                let mut query = url.query_pairs_mut();
                for (key, value) in pairs {
                    if !value.is_empty() {
                        query.append_pair(key, value);
                    }
                }
            }
            (StatusCode::FOUND, [(LOCATION, url.to_string())]).into_response()
        }
        Err(_) => not_accepted("the client's redirect address does not read"),
    }
}

/// Start an authorisation: the client and where its codes go first, then
/// what it asked for, then the deployment's own sign-in, always afresh.
async fn authorize(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(asked): Query<HashMap<String, String>>,
) -> Response {
    let now = app.clock.now_ns();
    if let Err(stale) = app.records.current(now) {
        return refused(&stale.to_string());
    }
    if app.first_run {
        return refused("this deployment is not set up yet, so nobody can sign in to it");
    }
    let field = |name: &str| asked.get(name).map(String::as_str).unwrap_or_default();
    let client = match app.delegations.client(field("client_id")).await {
        Ok(Some(client)) => client,
        Ok(None) => {
            return not_accepted(
                "this deployment does not know that client; it registers before it asks",
            )
        }
        Err(failed) => return refused(&failed.to_string()),
    };
    let redirect_uri =
        match field("redirect_uri") {
            "" if client.redirect_uris.len() == 1 => client.redirect_uris[0].clone(),
            given if client.redirects_to(given) => given.to_string(),
            _ => return not_accepted(
                "that redirect address is not one the client registered, so nothing is sent there",
            ),
        };
    let state = field("state");
    let issuer = issuer(&app, &headers);
    let resource = match Resource::indicated(field("resource")) {
        Some((resource, origin)) if origin == issuer => resource,
        _ => {
            return back_to(
                &redirect_uri,
                &[
                    ("error", "invalid_target"),
                    (
                        "error_description",
                        &format!("resource must name {issuer}/terminal or {issuer}/mcp"),
                    ),
                    ("state", state),
                ],
            )
        }
    };
    let asked = match check_asked(
        client,
        &redirect_uri,
        field("response_type"),
        field("code_challenge"),
        field("code_challenge_method"),
        state,
        resource,
    ) {
        Ok(asked) => asked,
        Err((error, why)) => {
            return back_to(
                &redirect_uri,
                &[
                    ("error", error),
                    ("error_description", &why),
                    ("state", state),
                ],
            )
        }
    };
    let id = app.delegations.open(asked, now);

    if app.directory.is_some() || app.accounts.is_some() {
        return Html(password_page(&app, "", For::Client(&id), "")).into_response();
    }
    let Some(oidc) = &app.oidc else {
        return refused("no directory is configured for this deployment's dashboard");
    };
    // The provider sends everybody back to /callback; this is how it will
    // know this one was a client's.
    let (url, provider_state) = oidc.begin(now);
    app.delegations.through_provider(&provider_state, &id);
    let mut response = redirect(&url);
    response.headers_mut().insert(
        SET_COOKIE,
        set_cookie(&app, SIGN_IN_COOKIE, &provider_state, "/callback", 600),
    );
    response
}

/// What a person may name a delegation to: the plugin levels they hold, the
/// account groups their permissions name, and whether they hold the
/// deployment admin's capabilities.
struct Holdable {
    /// Each plugin instance and the levels held on it, Manage first.
    plugins: BTreeMap<String, Vec<AccessLevel>>,
    /// Each account group, its name and how many accounts it holds.
    account_groups: BTreeMap<String, (String, usize)>,
    deployment_admin: bool,
}

impl Holdable {
    fn group_names(&self) -> BTreeMap<String, String> {
        self.account_groups
            .iter()
            .map(|(id, (name, _))| (id.clone(), name.clone()))
            .collect()
    }

    /// What `covers` comes to on the page: on each plugin the one level
    /// picked, the highest it covers that the person holds, with the levels
    /// held below it; the account groups and the deployment admin's
    /// capabilities only where held. A starting point, never a grant: what
    /// is allowed is what the form sends, read by [`consent_of`].
    fn picked(&self, covers: &Covers) -> Covers {
        let mut picked = Covers {
            everything: covers.everything,
            deployment_admin: covers.deployment_admin && self.deployment_admin,
            ..Covers::default()
        };
        for (instance, held) in &self.plugins {
            let top = held.iter().copied().find(|level| {
                covers
                    .plugins
                    .contains(&(instance.clone(), level_name(*level).into()))
            });
            if let Some(top) = top {
                for level in with_below(held, top) {
                    picked
                        .plugins
                        .insert((instance.clone(), level_name(level).to_string()));
                }
            }
        }
        picked.account_groups = covers
            .account_groups
            .iter()
            .filter(|group| self.account_groups.contains_key(*group))
            .cloned()
            .collect();
        picked
    }
}

/// A level picked on a plugin, with every level held below it: Manage
/// includes Open and View, and Open includes View, as holding them does
/// (kernel/the-consent-page-at-scale, ruling 1).
fn with_below(held: &[AccessLevel], picked: AccessLevel) -> Vec<AccessLevel> {
    let rank = |level: AccessLevel| LEVELS_DOWN.iter().position(|l| *l == level);
    held.iter()
        .copied()
        .filter(|level| rank(*level) >= rank(picked))
        .collect()
}

async fn holdable(app: &App, person: &Person, now: i64) -> Result<Holdable, String> {
    let records = app
        .records
        .current(now)
        .map_err(|stale| stale.to_string())?;
    let access = person_access(&records, &person.subject, &person.directory_groups);
    let mut instances: BTreeSet<String> = access
        .plugins
        .iter()
        .filter(|(_, held)| held.holds_any())
        .map(|(instance, _)| instance.clone())
        .collect();
    if access.all_plugins_admin {
        instances.extend(
            crate::catalogue::launches(app)
                .await
                .into_iter()
                .map(|launch| launch.instance_id),
        );
        instances.extend(app.health.view().into_keys());
    }
    let plugins = instances
        .into_iter()
        .map(|instance| {
            let levels = access.held(&instance).levels();
            (instance, levels)
        })
        .filter(|(_, levels)| !levels.is_empty())
        .collect();
    let named: HashMap<&str, (&str, usize)> = records
        .account_groups
        .iter()
        .map(|g| {
            (
                g.account_group_id.as_str(),
                (g.name.as_str(), g.account_ids.len()),
            )
        })
        .collect();
    let account_groups = meridian_access::account_groups_named(&records, &access)
        .into_iter()
        .map(|id| {
            let (name, accounts) = named
                .get(id.as_str())
                .map(|(n, size)| (n.to_string(), *size))
                .unwrap_or_else(|| (id.clone(), 0));
            (id, (name, accounts))
        })
        .collect();
    Ok(Holdable {
        plugins,
        account_groups,
        deployment_admin: access.deployment_admin,
    })
}

/// Somebody signed in to a client's authorisation: record it as W6.1's
/// are, and ask them to consent. No browser session is made.
pub(super) async fn signed_in(
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
    let Some((asked, confirm)) = app.delegations.signed_in(id, person.clone(), now) else {
        return not_accepted(
            "this authorisation has expired or was already used; start it again from the client",
        );
    };
    record_sign_in(app, subject, display_name, groups.clone(), now);
    super::afresh(app, subject, groups, now).await;
    tracing::info!(subject, client_id = %asked.client.client_id, "signed in to consent to a client");
    consent_shown(app, id, &confirm, &asked, &person, false, now).await
}

/// The consent page for a signed-in authorisation: its choices starting from
/// the standing delegation to this client when renewing one, from the
/// person's last client's when they asked for that (ruling 3), else from the
/// defaults (ruling 2).
async fn consent_shown(
    app: &App,
    id: &str,
    confirm: &str,
    asked: &Asked,
    person: &Person,
    from_last: bool,
    now: i64,
) -> Response {
    let holdable = match holdable(app, person, now).await {
        Ok(holdable) => holdable,
        Err(why) => return refused(&why),
    };
    let theirs = match app.delegations.of_person(&person.subject, now).await {
        Ok(theirs) => theirs,
        Err(failed) => return refused(&failed.to_string()),
    };
    let client_id = &asked.client.client_id;
    let standing = theirs
        .iter()
        .find(|d| &d.client_id == client_id && d.live(now));
    // The person's most recent consent to any other client, made or renewed.
    let last = theirs
        .iter()
        .filter(|d| &d.client_id != client_id)
        .max_by_key(|d| d.renewed_at_ns.max(d.made_at_ns));
    let tools = match asked.resource {
        Resource::Mcp => Some(tool_rows(app)),
        Resource::Terminal => None,
    };
    Html(consent_page(&Consenting {
        id,
        confirm,
        asked,
        person,
        holdable: &holdable,
        standing,
        last,
        from_last: from_last && last.is_some(),
        tools: tools.as_ref(),
    }))
    .into_response()
}

/// The tools each row of access reaches, for a client asking for `/mcp`
/// (W6.17, W6.20, Q5): under each plugin and level, the plugin's tools that
/// level serves, by title, reads and acts apart; under the deployment
/// admin's capabilities, core's own.
pub(crate) struct ToolRows {
    pub plugins: BTreeMap<(String, String), Vec<(String, bool)>>,
    pub deployment_admin: Vec<(String, bool)>,
}

pub(crate) fn tool_rows(app: &App) -> ToolRows {
    let mut plugins: BTreeMap<(String, String), Vec<(String, bool)>> = BTreeMap::new();
    for (instance, report) in app.health.view() {
        for tool in &report.declared_tools {
            for level in &tool.levels {
                if let Ok(level) = AccessLevel::try_from(*level) {
                    plugins
                        .entry((instance.clone(), level_name(level).to_string()))
                        .or_default()
                        .push((tool.title.clone(), tool.reads));
                }
            }
        }
    }
    ToolRows {
        plugins,
        deployment_admin: crate::mcp::instruments::SPECS
            .iter()
            .map(|spec| (spec.title.to_string(), spec.reads))
            .collect(),
    }
}

fn tools_said(tools: &[(String, bool)]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    let said = |reads: bool| {
        tools
            .iter()
            .filter(|(_, r)| *r == reads)
            .map(|(title, _)| escape(title))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let (reads, acts) = (said(true), said(false));
    let mut out = String::from("<span class=\"hint tools\">");
    if !reads.is_empty() {
        out.push_str(&format!("Reads: {reads}. "));
    }
    if !acts.is_empty() {
        out.push_str(&format!("Acts: {acts}."));
    }
    out.push_str("</span>");
    out
}

/// Everything the consent page is drawn from.
struct Consenting<'a> {
    id: &'a str,
    confirm: &'a str,
    asked: &'a Asked,
    person: &'a Person,
    holdable: &'a Holdable,
    /// The person's live delegation to this client, which allowing renews.
    standing: Option<&'a Delegation>,
    /// Their most recent delegation to another client: "same as my last
    /// client" (ruling 3).
    last: Option<&'a Delegation>,
    /// The choices start from `last`, as the person asked.
    from_last: bool,
    /// For a client asking for `/mcp`: what each row's tools are.
    tools: Option<&'a ToolRows>,
}

/// One plugin instance's level, a select (ruling 1): Nothing, then each
/// level held from View up, each naming the levels it includes. Its options'
/// values are what [`consent_of`] reads as `level`.
fn level_select(instance: &str, held: &[AccessLevel], picked: Option<AccessLevel>) -> String {
    let selected = |on: bool| if on { " selected" } else { "" };
    let mut options = format!(
        "<option value=\"\"{}>Nothing</option>",
        selected(picked.is_none())
    );
    for level in held.iter().rev().copied() {
        let below: Vec<&str> = with_below(held, level)
            .into_iter()
            .filter(|l| *l != level)
            .map(button)
            .collect();
        let words = if below.is_empty() {
            button(level).to_string()
        } else {
            format!("{}, with {}", button(level), listed(&below))
        };
        // Whether the level reaches accounts, as Open and View do and
        // Manage alone does not: the summary says when none is ticked.
        let data = with_below(held, level)
            .iter()
            .any(|l| *l != AccessLevel::Admin);
        options.push_str(&format!(
            "<option value=\"{instance}:{name}\" data-short=\"{short}\"{data}{on}>{words}</option>",
            instance = escape(instance),
            name = level_name(level),
            short = button(level),
            data = if data { " data-accounts" } else { "" },
            on = selected(picked == Some(level)),
        ));
    }
    format!(
        "<select name=\"level\" data-instance=\"{id}\" aria-label=\"Level on {id}\">{options}</select>",
        id = escape(instance)
    )
}

/// What a plugin's levels reach on the MCP surface, folded away under it:
/// each level held and its tools, reads and acts apart. Empty when no level
/// reaches a tool. With the titles, for the search to find the plugin by.
fn reached(instance: &str, held: &[AccessLevel], tools: &ToolRows) -> (String, String) {
    let mut lines = String::new();
    let mut titles: BTreeSet<&str> = BTreeSet::new();
    for level in held {
        let Some(reached) = tools
            .plugins
            .get(&(instance.to_string(), level_name(*level).to_string()))
        else {
            continue;
        };
        titles.extend(reached.iter().map(|(title, _)| title.as_str()));
        lines.push_str(&format!(
            "<p><strong>{}</strong> {}</p>",
            button(*level),
            tools_said(reached)
        ));
    }
    if titles.is_empty() {
        return (String::new(), String::new());
    }
    (
        format!(
            "<details class=\"reach\"><summary>{n} tool{s}</summary>{lines}</details>",
            n = titles.len(),
            s = if titles.len() == 1 { "" } else { "s" },
        ),
        titles.into_iter().collect::<Vec<_>>().join(" "),
    )
}

/// What a narrowed choice lets the client do, in a short paragraph, from
/// what is picked. The page's script says the same from the form as it
/// changes ([`CONSENT_SCRIPT`]); keep the two alike.
fn may_do(picked: &Covers, names: &BTreeMap<String, String>) -> String {
    let mut clauses: Vec<String> = picked
        .by_level()
        .into_iter()
        .map(|(level, instances)| format!("use {} on {}", button(level), listed(&instances)))
        .collect();
    let mut groups: Vec<&str> = picked
        .account_groups
        .iter()
        .map(|id| names.get(id).map(String::as_str).unwrap_or(id))
        .collect();
    groups.sort_by_key(|name| name.to_lowercase());
    if !groups.is_empty() {
        clauses.push(format!("reach the accounts in {}", listed(&groups)));
    }
    if picked.deployment_admin {
        clauses.push("use the deployment admin's capabilities".into());
    }
    if clauses.is_empty() {
        return "Nothing is ticked, so it could reach nothing. Tick what it needs.".into();
    }
    let mut said = format!("It may {}.", clauses.join("; "));
    if groups.is_empty() && picked.plugins.iter().any(|(_, level)| level != "admin") {
        said.push_str(" It reaches no account: tick an account group for that.");
    }
    said.push_str(" Nothing else, and never more than you hold.");
    said
}

fn consent_page(consenting: &Consenting) -> String {
    let Consenting {
        id,
        confirm,
        asked,
        person,
        holdable,
        standing,
        last,
        from_last,
        tools,
    } = consenting;
    let client = &asked.client;
    let host = reqwest::Url::parse(&asked.redirect_uri)
        .ok()
        .and_then(|url| url.host_str().map(String::from))
        .unwrap_or_default();
    // Where the choices start (ruling 2): the last client's, when asked
    // for; the standing delegation's, when renewing one; else the CLI's
    // default is everything, any other client's nothing ticked.
    let start = match (from_last, last, standing) {
        (true, Some(last), _) => last.covers.clone(),
        (_, _, Some(standing)) => standing.covers.clone(),
        _ if client.is_cli() => Covers::everything(),
        _ => Covers::default(),
    };
    let picked = holdable.picked(&start);
    let names = holdable.group_names();
    let days = if client.is_cli() { 90 } else { 30 };
    let checked = |on: bool| if on { " checked" } else { "" };
    let who = if client.is_cli() {
        format!(
            "<p>A <code>meridian</code> command, calling itself <strong>{name}</strong>, asked \
             to act as <strong>{person}</strong> on this deployment. Its codes go to \
             <code>{host}</code>, this computer.</p>",
            name = escape(&client.name),
            person = escape(&person.display_name),
            host = escape(&host),
        )
    } else {
        format!(
            "<p>A client calling itself <strong>{name}</strong> (the name is its own choice) \
             asked to act as <strong>{person}</strong> on this deployment. Its codes go to \
             <code>{host}</code>.</p>",
            name = escape(&client.name),
            person = escape(&person.display_name),
            host = escape(&host),
        )
    };
    let renewing = match standing {
        Some(standing) => format!(
            "<p class=\"hint\">You delegated to it before; allowing renews that delegation, \
             which lapses on {}.</p>",
            escape(&rfc3339(standing.expires_at_ns)[..10])
        ),
        None => String::new(),
    };
    let started = match (from_last, last) {
        (true, Some(last)) => format!(
            "<p class=\"warn\" data-started=\"last\">These choices start from what you allowed \
             <strong>{}</strong>. Review them, then Allow.</p>",
            escape(&last.client_name)
        ),
        _ => String::new(),
    };
    // Ruling 3: a starting point, never an answer; it fills the choices in
    // again, for the person to read before Allow.
    let from = match last {
        Some(last) if !from_last => format!(
            "<p class=\"hint from-last\">Or start from what you allowed \
             <strong>{client}</strong> on {day}: {said}. \
             <button type=\"submit\" name=\"decision\" value=\"last\" formnovalidate>\
             Same as my last client</button></p>",
            client = escape(&last.client_name),
            day = escape(&rfc3339(last.renewed_at_ns.max(last.made_at_ns))[..10]),
            said = escape(&last.covers.said(&names)),
        ),
        _ => String::new(),
    };

    let mut choices = String::new();
    if holdable.deployment_admin {
        let reach = tools
            .filter(|t| !t.deployment_admin.is_empty())
            .map(|t| {
                format!(
                    "<details class=\"reach\"><summary>{} tools</summary><p>{}</p></details>",
                    t.deployment_admin.len(),
                    tools_said(&t.deployment_admin)
                )
            })
            .unwrap_or_default();
        choices.push_str(&format!(
            "<fieldset class=\"checks deployment\"><legend>This deployment</legend>\
             <div class=\"picker-option\"><label class=\"check\"><input type=\"checkbox\" \
             name=\"deployment_admin\" value=\"1\"{on}> <span class=\"option-label\">Deployment \
             admin: its own settings, accounts and plugins, but never who holds access</span>\
             </label>{reach}</div></fieldset>",
            on = checked(picked.deployment_admin),
        ));
    }
    if !holdable.plugins.is_empty() {
        let rows: Vec<Choice> = holdable
            .plugins
            .iter()
            .map(|(instance, held)| {
                let (after, also) = tools
                    .map(|t| reached(instance, held, t))
                    .unwrap_or_default();
                Choice {
                    value: instance.clone(),
                    label: instance.clone(),
                    also,
                    after,
                    control: level_select(instance, held, picked.level_on(instance)),
                    ..Default::default()
                }
            })
            .collect();
        choices.push_str(&picker::each(
            "consent-plugins",
            "Plugins: one level on each",
            "plugins",
            &rows,
        ));
    }
    if !holdable.account_groups.is_empty() {
        let mut groups: Vec<(&String, &(String, usize))> = holdable.account_groups.iter().collect();
        groups.sort_by(|(a, (an, _)), (b, (bn, _))| {
            (an.to_lowercase(), *a).cmp(&(bn.to_lowercase(), *b))
        });
        let rows: Vec<Choice> = groups
            .into_iter()
            .map(|(group, (name, accounts))| Choice {
                value: group.clone(),
                label: name.clone(),
                detail: format!(
                    "{accounts} account{}",
                    if *accounts == 1 { "" } else { "s" }
                ),
                also: group.clone(),
                ..Default::default()
            })
            .collect();
        let chosen: HashSet<&str> = picked.account_groups.iter().map(String::as_str).collect();
        choices.push_str(&picker::many(
            "consent-groups",
            "account_group",
            "Account groups: the accounts it reaches",
            "account groups",
            &rows,
            &chosen,
        ));
    }
    if choices.is_empty() {
        choices.push_str("<p class=\"empty\">You hold nothing on this deployment yet.</p>");
    }
    let lasts: String = DAYS
        .iter()
        .map(|d| {
            format!(
                "<option value=\"{d}\"{}>{d} days</option>",
                if *d == days { " selected" } else { "" }
            )
        })
        .collect();
    let everything = format!(
        "It may do anything you may do on this deployment, as that changes: every plugin at \
         every level you hold{tools}, and every account you reach{admin}.",
        tools = if tools.is_some() {
            ", with its tools on this deployment's MCP surface, those added later included"
        } else {
            ""
        },
        admin = if holdable.deployment_admin {
            ", and the deployment admin's capabilities"
        } else {
            ""
        },
    );
    let for_days =
        format!(" For <span data-days>{days}</span> days, recorded as yours, through it.");
    page(
        "Allow a client",
        &format!(
            "<h1>Allow a client to act as you</h1>{who}{renewing}{started}\
             <p><strong>Only allow it if you started this yourself, just now.</strong> If you \
             did not, somebody is asking you to let them in.</p>\
             <form method=\"post\" action=\"/oauth/authorize\" class=\"consent\">\
             <input type=\"hidden\" name=\"request\" value=\"{id}\">\
             <input type=\"hidden\" name=\"confirm\" value=\"{confirm}\">\
             <fieldset class=\"choice covers\"><legend>What it may do</legend>\
             <div class=\"options\">\
             <label class=\"option\"><input type=\"radio\" name=\"covers\" value=\"everything\"{all}> \
             <span class=\"option-label\">Everything you hold, as that changes</span></label>\
             <label class=\"option\"><input type=\"radio\" name=\"covers\" value=\"some\"{some}> \
             <span class=\"option-label\">Only what is ticked</span></label></div>{from}\
             <div class=\"choices\">{choices}</div></fieldset>\
             <label>Until<select name=\"days\">{lasts}</select></label>\
             <section class=\"summary\" aria-live=\"polite\"><h2>What it will be able to do</h2>\
             <p class=\"summary-everything\">{everything}{for_days}</p>\
             <p class=\"summary-some\"><span data-summary>{some_said}</span>{for_days}</p>\
             <p class=\"hint\">You are told a week before it lapses. You can revoke it at any \
             time from Connected clients.</p></section>\
             <div class=\"consent-foot\">\
             <button name=\"decision\" value=\"allow\" class=\"primary\">Allow</button> \
             <button name=\"decision\" value=\"deny\">Don't allow</button></div>\
             </form><script>(function () {{{picker_script}{consent_script}}})();</script>",
            id = escape(id),
            confirm = escape(confirm),
            all = checked(start.everything),
            some = checked(!start.everything),
            everything = escape(&everything),
            some_said = escape(&may_do(&picked, &names)),
            picker_script = picker::SCRIPT,
            consent_script = CONSENT_SCRIPT,
        ),
    )
}

/// The summary, said again from the form as it changes, as [`may_do`] says
/// it; and the days chosen. Without it the summary is what the page opened
/// with, and the form posts as it always did.
const CONSENT_SCRIPT: &str = r#"
  var form = document.querySelector("form.consent");
  if (!form) return;
  var summary = form.querySelector("[data-summary]");
  var LEVELS = [["admin", "Manage"], ["write", "Open"], ["read", "View"]];
  function listed(names) {
    if (names.length < 2) return names.join("");
    if (names.length <= 3) return names.slice(0, -1).join(", ") + " and " + names[names.length - 1];
    return names.slice(0, 3).join(", ") + " and " + (names.length - 3) + " more";
  }
  function label(input) { return input.closest(".picker-option").querySelector(".option-label").textContent; }
  function update() {
    var at = { admin: [], write: [], read: [] }, clauses = [], accounts = false;
    Array.prototype.forEach.call(form.querySelectorAll("select[name=level]"), function (select) {
      if (!select.value) return;
      at[select.value.slice(select.value.lastIndexOf(":") + 1)].push(select.getAttribute("data-instance"));
      if (select.options[select.selectedIndex].hasAttribute("data-accounts")) accounts = true;
    });
    LEVELS.forEach(function (level) {
      if (at[level[0]].length) clauses.push("use " + level[1] + " on " + listed(at[level[0]]));
    });
    var groups = Array.prototype.map.call(form.querySelectorAll("input[name=account_group]:checked"), label);
    if (groups.length) clauses.push("reach the accounts in " + listed(groups));
    var admin = form.querySelector("input[name=deployment_admin]");
    if (admin && admin.checked) clauses.push("use the deployment admin's capabilities");
    var said;
    if (!clauses.length) {
      said = "Nothing is ticked, so it could reach nothing. Tick what it needs.";
    } else {
      said = "It may " + clauses.join("; ") + ".";
      if (!groups.length && accounts) said += " It reaches no account: tick an account group for that.";
      said += " Nothing else, and never more than you hold.";
    }
    summary.textContent = said;
    var days = form.querySelector("select[name=days]").value;
    Array.prototype.forEach.call(form.querySelectorAll("[data-days]"), function (span) { span.textContent = days; });
  }
  form.addEventListener("change", update);
  update();
"#;

/// What the person ticked, held to what they hold now.
fn consent_of(fields: &[(String, String)], holdable: &Holdable) -> Result<Consent, String> {
    let one = |name: &str| {
        fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .unwrap_or_default()
    };
    let days: i64 = one("days").parse().unwrap_or(0);
    if !DAYS.contains(&days) {
        return Err("a delegation lasts 7, 30 or 90 days".into());
    }
    if one("covers") == "everything" {
        return Ok(Consent {
            covers: Covers::everything(),
            days,
        });
    }
    let mut covers = Covers::default();
    if one("deployment_admin") == "1" {
        if !holdable.deployment_admin {
            return Err("you do not hold the deployment admin's capabilities".into());
        }
        covers.deployment_admin = true;
    }
    for (key, value) in fields {
        match key.as_str() {
            // A plugin's select left at Nothing.
            "level" if value.is_empty() => {}
            "level" => {
                let Some((instance, level)) = value.rsplit_once(':') else {
                    return Err(format!("`{value}` is not a plugin and a level"));
                };
                let Some((held, picked)) = holdable.plugins.get(instance).and_then(|held| {
                    let picked = held.iter().copied().find(|l| level_name(*l) == level)?;
                    Some((held, picked))
                }) else {
                    return Err(format!("you do not hold {level} on {instance}"));
                };
                // One level per plugin, including those held below it
                // (ruling 1): what holding it gives.
                for level in with_below(held, picked) {
                    covers
                        .plugins
                        .insert((instance.to_string(), level_name(level).to_string()));
                }
            }
            "account_group" => {
                if !holdable.account_groups.contains_key(value) {
                    return Err(format!(
                        "no permission of yours names the account group {value}"
                    ));
                }
                covers.account_groups.insert(value.clone());
            }
            _ => {}
        }
    }
    Ok(Consent { covers, days })
}

/// The person's answer, sent back to the client's redirect address: a code,
/// or that they declined.
async fn decide(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Vec<(String, String)>>,
) -> Response {
    let now = app.clock.now_ns();
    let one = |name: &str| {
        fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    };
    let (id, confirm) = (one("request"), one("confirm"));
    let Some((asked, person)) = app.delegations.consenting(&id, &confirm, now) else {
        return not_accepted("this authorisation has expired, was already used, or is not yours");
    };
    // "Same as my last client" (ruling 3): the page again, its choices
    // filled from that delegation, and nothing decided or spent.
    if one("decision") == "last" {
        return consent_shown(&app, &id, &confirm, &asked, &person, true, now).await;
    }
    let consent = if one("decision") == "allow" {
        let holdable = match holdable(&app, &person, now).await {
            Ok(holdable) => holdable,
            Err(why) => return refused(&why),
        };
        match consent_of(&fields, &holdable) {
            Ok(consent) => Some(consent),
            Err(why) => return not_accepted(&why),
        }
    } else {
        None
    };
    let decided = match app.delegations.decide(&id, &confirm, consent, now).await {
        Ok(decided) => decided,
        Err(failed) => return refused(&failed.to_string()),
    };
    let issuer = issuer(&app, &headers);
    match decided {
        Err(why) => not_accepted(why),
        Ok((asked, Some(code))) => {
            tracing::info!(subject = %person.subject, client_id = %asked.client.client_id, "a delegation was made");
            back_to(
                &asked.redirect_uri,
                &[("code", &code), ("state", &asked.state), ("iss", &issuer)],
            )
        }
        Ok((back, None)) => back_to(
            &back.redirect_uri,
            &[
                ("error", "access_denied"),
                ("state", &back.state),
                ("iss", &issuer),
            ],
        ),
    }
}

// ── Tokens ────────────────────────────────────────────────────────────────

/// What the directory says of the person now, where the dashboard can ask
/// without them (requirement 7): LDAP's groups read again by the dashboard's
/// own bind, a local account looked for. A person the directory no longer
/// finds, or an account gone, revokes the delegation. Asked whenever a token
/// is issued on it, so at most ten minutes apart while the client works.
async fn fresh(
    app: &App,
    mut delegation: Delegation,
    now: i64,
) -> Result<Result<Delegation, Refusal>, Response> {
    let id = delegation.id.clone();
    let gone = |why: &'static str| {
        let id = id.clone();
        async move {
            app.delegations
                .revoke(&id, "dashboard", why, now)
                .await
                .map_err(unavailable)?;
            Ok::<_, Response>(Err(Refusal::Revoked))
        }
    };
    if let Some(directory) = &app.directory {
        let prefix = format!("{}|", directory.issuer());
        let Some(dn) = delegation.subject.strip_prefix(&prefix).map(str::to_string) else {
            return gone("the person is not one this directory knows").await;
        };
        match directory.groups_of(&dn).await {
            Ok(Some(groups)) => {
                app.delegations
                    .groups_read(&id, groups.clone(), now)
                    .await
                    .map_err(unavailable)?;
                delegation.directory_groups = groups;
                delegation.groups_read_at_ns = now;
            }
            Ok(None) => return gone("the directory no longer finds this person").await,
            Err(failed) => {
                tracing::warn!(%failed, "a delegation's groups could not be read again");
                return Err(oauth_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "temporarily_unavailable",
                    &format!("the directory could not be asked about the person: {failed}"),
                ));
            }
        }
    } else if let Some(accounts) = &app.accounts {
        let Some(name) = delegation
            .subject
            .strip_prefix("local|")
            .map(str::to_string)
        else {
            return gone("the person is not an account this deployment holds").await;
        };
        let store = Arc::clone(accounts);
        let found = tokio::task::spawn_blocking(move || store.by_name(&name))
            .await
            .map_err(|failed| failed.to_string())
            .and_then(|found| found);
        match found {
            Ok(Some(account)) => {
                if account.groups != delegation.directory_groups {
                    app.delegations
                        .groups_read(&id, account.groups.clone(), now)
                        .await
                        .map_err(unavailable)?;
                    delegation.directory_groups = account.groups;
                }
            }
            Ok(None) => return gone("the account was removed").await,
            Err(failed) => {
                return Err(oauth_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "temporarily_unavailable",
                    &failed,
                ))
            }
        }
    }
    Ok(match delegation.refusal(now, groups_bound(app)) {
        Some(refusal) => Err(refusal),
        None => Ok(delegation),
    })
}

/// RFC 6749's token endpoint, for a code or a refresh token. Every answer
/// carries the delegation's expiry, which the CLI says aloud within its last
/// week (requirement 6).
async fn token(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(asked): Form<HashMap<String, String>>,
) -> Response {
    let now = app.clock.now_ns();
    if let Err(stale) = app.records.current(now) {
        return oauth_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "temporarily_unavailable",
            &stale.to_string(),
        );
    }
    let field = |name: &str| asked.get(name).map(String::as_str).unwrap_or_default();
    let client_id = field("client_id");
    if client_id.is_empty() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "client_id is required: every client here is public",
        );
    }
    let resource = match field("resource") {
        "" => None,
        named => match Resource::indicated(named) {
            Some((resource, origin)) if origin == issuer(&app, &headers) => Some(resource),
            _ => {
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_target",
                    "that resource is not one this deployment issues tokens for",
                )
            }
        },
    };
    let (delegation, resource, spending) = match field("grant_type") {
        "authorization_code" => {
            let redeemed = app
                .delegations
                .redeem(
                    field("code"),
                    field("code_verifier"),
                    field("redirect_uri"),
                    client_id,
                    resource,
                    now,
                )
                .await;
            match redeemed {
                Err(failed) => return unavailable(failed),
                Ok(Err((refusal, why))) => {
                    tracing::info!(why, "an authorisation code was refused");
                    return refused_grant(refusal);
                }
                Ok(Ok((delegation, resource))) => (delegation, resource, None),
            }
        }
        "refresh_token" => {
            match app
                .delegations
                .refreshing(field("refresh_token"), client_id, now)
                .await
            {
                Err(failed) => return unavailable(failed),
                Ok(Err(refusal)) => return refused_grant(refusal),
                Ok(Ok((delegation, token))) => {
                    let named = Resource::named(&token.resource).unwrap_or(Resource::Terminal);
                    if resource.is_some_and(|asked| asked != named) {
                        return oauth_error(
                            StatusCode::BAD_REQUEST,
                            "invalid_target",
                            "a refresh token issues tokens for its own resource",
                        );
                    }
                    (delegation, named, Some(token))
                }
            }
        }
        "" => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "grant_type is required",
            )
        }
        _ => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "unsupported_grant_type",
                "this deployment takes an authorization_code or a refresh_token",
            )
        }
    };
    // How fresh the person is, before anything is spent: a refusal asking
    // them to sign in again leaves the refresh token usable after they have.
    let id = delegation.id.clone();
    let delegation = match fresh(&app, delegation, now).await {
        Err(response) => return response,
        Ok(Err(refusal)) => {
            if let Err(failed) = app.delegations.refused(&id, refusal.sentence(), now).await {
                tracing::warn!(%failed, "a refusal was not recorded");
            }
            return refused_grant(refusal);
        }
        Ok(Ok(delegation)) => delegation,
    };
    if let Some(token) = spending {
        match app.delegations.spend(&token, now).await {
            Err(failed) => return unavailable(failed),
            Ok(false) => return refused_grant(Refusal::Reused),
            Ok(true) => {}
        }
    }
    let issued = match app.delegations.issue(delegation, resource, now).await {
        Ok(issued) => issued,
        Err(failed) => return unavailable(failed),
    };
    let delegation = &issued.delegation;
    tracing::info!(
        subject = %delegation.subject,
        delegation_id = %delegation.id,
        client_id = %delegation.client_id,
        "tokens issued on a delegation"
    );
    let left_ns = delegation.expires_at_ns - now;
    json(
        StatusCode::OK,
        serde_json::json!({
            "access_token": issued.access_token,
            "token_type": "Bearer",
            "expires_in": (crate::delegation::ACCESS_NS.min(left_ns)) / 1_000_000_000,
            "refresh_token": issued.refresh_token,
            "resource": format!("{}{}", issuer(&app, &headers), resource.path()),
            "subject": delegation.subject,
            "delegation_id": delegation.id,
            "delegation_expires_at": rfc3339(delegation.expires_at_ns),
            "delegation_expires_in": left_ns / 1_000_000_000,
            "delegation_lapses_soon": left_ns <= NOTICE_NS,
        }),
    )
}

/// RFC 7009: a client revokes the delegation its token is on (`meridian
/// sign-out`). The same answer whether or not there was anything to revoke,
/// as the RFC has it -- unless it could not be revoked at all, which is said.
async fn revoke(
    State(app): State<Arc<App>>,
    Form(asked): Form<HashMap<String, String>>,
) -> Response {
    let field = |name: &str| asked.get(name).map(String::as_str).unwrap_or_default();
    let (token, client_id) = (field("token"), field("client_id"));
    if token.is_empty() || client_id.is_empty() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "token and client_id are both required",
        );
    }
    match app
        .delegations
        .revoke_by_token(token, client_id, app.clock.now_ns())
        .await
    {
        Ok(()) => json(StatusCode::OK, serde_json::json!({})),
        Err(failed) => unavailable(failed),
    }
}

/// How long until a delegation lapses, for a person: "in 3 days".
pub(crate) fn lapses(delegation: &Delegation, now: i64) -> String {
    let left = delegation.expires_at_ns - now;
    if left < 0 {
        format!("lapsed on {}", &rfc3339(delegation.expires_at_ns)[..10])
    } else if left < DAY_NS {
        "lapses today".into()
    } else {
        format!(
            "lapses in {} days, on {}",
            left / DAY_NS,
            &rfc3339(delegation.expires_at_ns)[..10]
        )
    }
}

#[cfg(test)]
pub(in crate::web) mod tests;
