//! The pages that administer a deployment, and the claim page that makes its
//! first administrator.
//!
//! Every change is a command to the conductor's configuration store, sent on
//! the signed-in person's behalf (`Bus::call_for`), so the store records who
//! made it. The rules live there; a refusal comes back as the store's own
//! sentence and is shown as it is. After a change the records are read again
//! at once, so the page shows what was just done rather than waiting for the
//! next refresh.
//!
//! Every form carries the session's form token and is refused without it.
//! Every page but the claim page and a plugin's own tabs is refused to
//! anybody not holding deployment admin, checked against the records on each
//! request. A plugin's tabs, `/admin/plugins/{instance}`, are its admins' --
//! a deployment admin being one through All plugins (admin) -- and a
//! deployment admin's for what is theirs on it; its settings are its admins'
//! alone (W6.9 to W6.11, decisions/027). The plugin's status is drawn in the
//! plugin's area under Manage, on its Summary ([`summary_tab`]), and the same
//! settings form on its Settings ([`settings_tab`]), posted to
//! `/plugins/{instance}/settings` under the same guard.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use meridian_access::{person_access, Access, AccessLevel, DEPLOYMENT_ADMIN};
use meridian_bus::BusError;
use meridian_domain::v1::{
    AccessEntry, AccessGroup, AccessRecords, AccountGroup, ClaimCodePurpose, CloseAccountRequest,
    DefineAccessGroupRequest, DefineAccountGroupRequest, DefineAccountRequest,
    DefineUserGroupRequest, GrantPermissionRequest, PluginSettingsRecord, RedeemClaimCodeReply,
    RedeemClaimCodeRequest, UserGroup, WithdrawPermissionReply, WithdrawPermissionRequest,
};
use prost::Message;

use crate::html::{escape, page, page_with, Chrome, Viewer};
use crate::records;
use crate::session::Session;
use crate::web::{refused, session_of, App};

pub(crate) type Fields = HashMap<String, String>;

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/claim", get(claim_page).post(claim))
        .route("/admin", get(admin_page))
        .route("/admin/accounts", post(define_account))
        .route("/admin/accounts/close", post(close_account))
        .route("/admin/accounts/book", post(set_book))
        .route("/admin/user-groups", post(define_user_group))
        .route("/admin/account-groups", post(define_account_group))
        .route("/admin/access-groups", post(define_access_group))
        .route("/admin/permissions", post(grant))
        .route("/admin/permissions/withdraw", post(withdraw))
        .route("/admin/holds", post(set_hold))
        .route("/admin/instruments", get(instruments_page))
        .route("/admin/instruments/complete", post(complete_instruments))
        .route("/admin/instruments/accept", post(accept_offers))
        .route("/admin/instruments/merge", post(merge_instruments))
        .route("/admin/instruments/{instrument}", get(instrument_page))
        .route(
            "/admin/instruments/{instrument}/ask",
            post(ask_the_platform),
        )
        .route("/admin/plugins/{instance}", get(plugin_view))
        .route(
            "/admin/plugins/{instance}/settings",
            get(settings_page).post(set_settings),
        )
        .route("/plugins/{instance}/settings", post(set_settings_in_area))
}

fn field<'a>(fields: &'a Fields, name: &str) -> &'a str {
    fields.get(name).map(|v| v.trim()).unwrap_or_default()
}

/// One item per line, blanks dropped. For directory groups, because an LDAP
/// group is a distinguished name, and a distinguished name is full of commas:
/// split on them, `cn=traders,ou=groups,dc=firm` became three groups, none of
/// which any sign-in presents.
fn lines(fields: &Fields, name: &str) -> Vec<String> {
    field(fields, name)
        .lines()
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(String::from)
        .collect()
}

/// A comma- or line-separated list, blanks dropped.
fn list(fields: &Fields, name: &str) -> Vec<String> {
    field(fields, name)
        .split([',', '\n'])
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(String::from)
        .collect()
}

fn has_admin(records: &AccessRecords) -> bool {
    records
        .permissions
        .iter()
        .any(|p| p.access_group_id == DEPLOYMENT_ADMIN)
}

pub(crate) fn status_page(status: StatusCode, title: &str, sentence: &str) -> Response {
    (
        status,
        Html(page(
            title,
            &format!(
                "<h1>{}</h1><p class=\"refused\">{}</p><p><a href=\"/\">Home</a></p>",
                escape(title),
                escape(sentence)
            ),
        )),
    )
        .into_response()
}

/// Who is asking, what the records say now, and whether they hold
/// deployment admin; or the response that refuses them.
pub(crate) fn gate(
    app: &App,
    headers: &HeaderMap,
    need_admin: bool,
) -> Result<(Session, AccessRecords), Box<Response>> {
    let records = app
        .records
        .current(app.clock.now_ns())
        .map_err(|stale| Box::new(refused(&stale.to_string())))?;
    let session = session_of(app, headers).ok_or_else(|| {
        status_page(
            StatusCode::UNAUTHORIZED,
            "Sign in first",
            "this page needs you signed in",
        )
    })?;
    if need_admin
        && !person_access(&records, &session.subject, &session.directory_groups).deployment_admin
    {
        return Err(Box::new(status_page(
            StatusCode::FORBIDDEN,
            "Not permitted",
            "this page is for deployment admins",
        )));
    }
    Ok((session, records))
}

/// Who is asking about one plugin's tabs, what the records say now, and what
/// they hold; or the response refusing anybody who neither administers the
/// plugin nor the deployment. `settings`: only its admins.
fn gate_plugin(
    app: &App,
    headers: &HeaderMap,
    instance: &str,
    settings: bool,
) -> Result<(Session, AccessRecords, Access), Box<Response>> {
    let (session, records) = gate(app, headers, false)?;
    let access = person_access(&records, &session.subject, &session.directory_groups);
    let administers = access.administers(instance);
    if !(administers || (!settings && access.deployment_admin)) {
        return Err(Box::new(status_page(
            StatusCode::FORBIDDEN,
            "Not permitted",
            if settings {
                "a plugin's settings are for its admins"
            } else {
                "this page is for the plugin's admins and deployment admins"
            },
        )));
    }
    Ok((session, records, access))
}

pub(crate) fn form_token_matches(session: &Session, fields: &Fields) -> Result<(), Box<Response>> {
    if field(fields, "form_token") != session.form_token {
        return Err(Box::new(status_page(
            StatusCode::BAD_REQUEST,
            "Not accepted",
            "this form did not come from your session",
        )));
    }
    Ok(())
}

/// One command to the configuration store on the person's behalf. The
/// store's refusal sentence is returned as it is.
async fn command<Rep: Message + Default>(
    app: &App,
    session: &Session,
    topic: &str,
    request_type: &str,
    request: impl Message,
) -> Result<Rep, String> {
    let answered = app
        .bus
        .call_for(
            topic,
            request_type,
            request.encode_to_vec(),
            None,
            Some(Duration::from_secs(10)),
            &session.subject,
        )
        .await;
    let (_, bytes) = answered.map_err(|failed| match failed {
        BusError::HandlerFailed { detail, .. } => detail,
        other => other.to_string(),
    })?;
    // Read again now, so the next page shows this change.
    if let Err(failed) = records::refresh(&app.bus, &app.records, app.clock.as_ref()).await {
        tracing::warn!("the records could not be re-read after a change: {failed}");
    }
    Rep::decode(&bytes[..]).map_err(|failed| format!("an undecodable reply: {failed}"))
}

fn after(outcome: Result<(), String>) -> Response {
    after_to(outcome, "/admin", "/admin")
}

/// The page saying a change was not done, in the store's sentence, with a
/// way back to `back`.
pub(crate) fn not_done(sentence: &str, back: &str) -> Response {
    after_to(Err(sentence.to_string()), back, back)
}

/// To `done` when it was, and otherwise the store's sentence, with a way
/// back to `back`.
fn after_to(outcome: Result<(), String>, done: &str, back: &str) -> Response {
    match outcome {
        Ok(()) => (
            StatusCode::SEE_OTHER,
            [(axum::http::header::LOCATION, done.to_string())],
        )
            .into_response(),
        // Back, not to the page: the browser's back returns to the tab the
        // form was on, with what was typed into it.
        Err(sentence) => (
            StatusCode::BAD_REQUEST,
            Html(page(
                "Not done",
                &format!(
                    "<h1>Not done</h1><p class=\"refused\">{}</p>\
                     <p><a href=\"{}\" onclick=\"history.back();return false\">Back</a></p>",
                    escape(&sentence),
                    escape(back)
                ),
            )),
        )
            .into_response(),
    }
}

// ── The first deployment admin ──────────────────────────────────────────────

async fn claim_page(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let (session, records) = match gate(&app, &headers, false) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if has_admin(&records) {
        return status_page(
            StatusCode::CONFLICT,
            "Already claimed",
            "this deployment already has a deployment admin",
        );
    }
    Html(page(
        "Claim this deployment",
        &format!(
            "<h1>Claim this deployment</h1>\
             <p>Enter the claim code issued on open-meridian.com. Redeeming it makes you this \
             deployment's first deployment admin. The platform learns that the code was used, \
             and not by whom.</p>\
             <form method=\"post\" action=\"/claim\">{}\
             <label>Claim code <input name=\"code\" autocomplete=\"off\" required></label> \
             <button>Redeem</button></form>",
            token_input(&session)
        ),
    ))
    .into_response()
}

async fn claim(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, _) = match gate(&app, &headers, false) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if let Err(response) = form_token_matches(&session, &fields) {
        return *response;
    }
    let request = RedeemClaimCodeRequest {
        code: field(&fields, "code").to_string(),
        purpose: ClaimCodePurpose::FirstAdmin as i32,
    };
    let reply: Result<RedeemClaimCodeReply, String> = command(
        &app,
        &session,
        "platform.config.command.redeem-claim-code",
        "meridian.v1.RedeemClaimCodeRequest",
        request,
    )
    .await;
    match reply {
        Ok(reply) if reply.redeemed => (
            StatusCode::SEE_OTHER,
            [(axum::http::header::LOCATION, "/admin")],
        )
            .into_response(),
        Ok(reply) => status_page(
            StatusCode::BAD_REQUEST,
            "Not redeemed",
            &reply.refusal_reason,
        ),
        Err(sentence) => status_page(StatusCode::BAD_REQUEST, "Not redeemed", &sentence),
    }
}

/// The header of a page in the admin portal, for a deployment admin.
pub(crate) fn admin_chrome(session: &Session) -> Chrome<'_> {
    Chrome {
        viewer: Some(Viewer {
            display_name: &session.display_name,
            form_token: &session.form_token,
            admin: true,
        }),
        // The settings home, where this chrome is drawn as it is.
        crumbs: crate::html::crumb_here("Settings", None),
        main: "page",
        in_admin: true,
        report: crate::html::Report::Dashboard,
    }
}

pub(crate) fn token_input(session: &Session) -> String {
    format!(
        "<input type=\"hidden\" name=\"form_token\" value=\"{}\">",
        escape(&session.form_token)
    )
}

// ── The overview ────────────────────────────────────────────────────────────

mod books;
pub mod holds;
pub mod instruments;
mod overview;
pub mod people;
pub(crate) mod picker;

/// Everybody this dashboard can name for a user group (people.rs): those the
/// groups name, those holding a delegation, and the accounts this
/// deployment holds itself. The local accounts are read off the async
/// threads, as a sign-in reads them; if they cannot be read, the page still
/// lists the rest and says so in the log.
async fn people_known(
    app: &App,
    records: &AccessRecords,
    holders: &[(String, String, usize)],
) -> Vec<people::Person> {
    let local = match &app.accounts {
        None => Vec::new(),
        Some(accounts) => {
            let accounts = Arc::clone(accounts);
            match tokio::task::spawn_blocking(move || accounts.people()).await {
                Ok(Ok(held)) => held,
                Ok(Err(unread)) => {
                    tracing::warn!(%unread, "the local accounts could not be listed");
                    Vec::new()
                }
                Err(joined) => {
                    tracing::warn!(%joined, "listing the local accounts did not finish");
                    Vec::new()
                }
            }
        }
    };
    let local: Vec<(String, String)> = local
        .into_iter()
        .map(|(name, display)| (format!("local|{name}"), display))
        .collect();
    people::gather(
        holders
            .iter()
            .map(|(login, name, _)| (login.as_str(), name.as_str()))
            .chain(
                local
                    .iter()
                    .map(|(login, name)| (login.as_str(), name.as_str())),
            )
            .chain(
                records
                    .user_groups
                    .iter()
                    .flat_map(|g| g.logins.iter().map(|login| (login.as_str(), ""))),
            ),
    )
}

async fn admin_page(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let (session, records) = match gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let delegating = match app.delegations.holders(app.clock.now_ns()).await {
        Ok(delegating) => delegating,
        Err(unavailable) => {
            tracing::error!(%unavailable, "delegations could not be listed");
            return crate::web::refused(&unavailable.to_string());
        }
    };
    let custody = app.custody.view();
    let lines = plugin_lines(&app, &records, &custody).await;
    let people = people_known(&app, &records, &delegating).await;
    let books = books::read(&app.bus).await;
    let body = overview::render(
        &records,
        &delegating,
        &lines,
        &people,
        &books,
        &token_input(&session),
        "",
    );
    Html(page_with("Settings", &body, &admin_chrome(&session))).into_response()
}

/// Every plugin instance known anywhere, with its health and what it needs.
async fn plugin_lines(
    app: &App,
    records: &AccessRecords,
    custody: &crate::custody::Heard,
) -> Vec<view::Line> {
    let names: HashMap<String, String> = crate::catalogue::launches(app)
        .await
        .into_iter()
        .map(|launch| (launch.instance_id, launch.name))
        .collect();
    view::lines(
        records,
        &app.health.view(),
        &names,
        custody,
        crate::html::is_development(),
        app.clock.now_ns(),
    )
}

// ── A plugin instance's admin view ──────────────────────────────────────────

pub mod view;

async fn plugin_view(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(instance): Path<String>,
    Query(query): Query<Fields>,
) -> Response {
    let (session, records, access) = match gate_plugin(&app, &headers, &instance, false) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let custody = app.custody.view();
    let lines = plugin_lines(&app, &records, &custody).await;
    let Some(line) = lines.iter().find(|line| line.instance == instance) else {
        return no_such_plugin(&instance);
    };
    let notice = match field(&query, "saved") {
        "1" => "Saved.",
        "none" => "Nothing was changed.",
        _ => "",
    };
    // The tabs every plugin has; Settings for its admins (W6.11). Its own
    // pages are in its area, which the view links to (W6.9).
    let administers = access.administers(&instance);
    let reports = app.health.view();
    let report = reports.get(&instance);
    let tabs = view::tabs(
        administers,
        settings_of(&records, &instance),
        crate::html::is_development(),
    );
    let current = view::chosen(&tabs, field(&query, "tab"));
    let choices = match settings_of(&records, &instance) {
        Some(record) => choices(&app, &instance, record, &records).await,
        None => settings::Choices::default(),
    };
    let body = view::render(&view::View {
        line,
        choices: &choices,
        record: settings_of(&records, &instance),
        report,
        records: &records,
        custody: &custody,
        token: &token_input(&session),
        notice,
        development: crate::html::is_development(),
        tabs: &tabs,
        current,
        area: administers.then(|| view::area_at_admin(&instance)),
        may_grant: access.deployment_admin,
        held: &access.plugin(&instance),
    });
    // The way back is the breadcrumb: for a deployment admin the settings
    // home, its plugins, then this one by its name, its instance ID on
    // hover; for a plugin's admin, Home, then this one.
    let mut chrome = admin_chrome(&session);
    let name = line.name.as_deref().unwrap_or(&instance);
    if access.deployment_admin {
        chrome.crumbs = format!(
            "{}{}{}",
            crate::html::crumb_link("/admin", "Settings"),
            crate::html::crumb_link("/admin#plugins", "Plugins"),
            crate::html::crumb_here(name, Some(&instance)),
        );
    } else {
        chrome.crumbs = format!(
            "{}{}",
            crate::html::crumb_link("/", "Home"),
            crate::html::crumb_here(name, Some(&instance)),
        );
        if let Some(viewer) = chrome.viewer.as_mut() {
            viewer.admin = false;
        }
    }
    Html(page_with(&format!("{name} admin"), &body, &chrome)).into_response()
}

// ── A plugin instance's settings (W6.11) ────────────────────────────────────

pub(crate) mod settings;

fn settings_of<'a>(records: &'a AccessRecords, instance: &str) -> Option<&'a PluginSettingsRecord> {
    records
        .plugin_settings
        .iter()
        .find(|record| record.plugin_instance_id == instance)
}

fn no_such_plugin(instance: &str) -> Response {
    status_page(
        StatusCode::NOT_FOUND,
        "No such plugin",
        &format!("no plugin {instance} has reported, so what it needs is not known yet"),
    )
}

/// The settings form is in the plugin's admin view now; its old address
/// goes there.
async fn settings_page(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(instance): Path<String>,
) -> Response {
    if let Err(response) = gate_plugin(&app, &headers, &instance, true) {
        return *response;
    }
    (
        StatusCode::SEE_OTHER,
        [(
            axum::http::header::LOCATION,
            view::tab_href(&instance, view::SETTINGS),
        )],
    )
        .into_response()
}

/// The form, from the admin portal's Settings tab or a table's, back to it.
async fn set_settings(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(instance): Path<String>,
    Query(asked): Query<HashMap<String, String>>,
    Form(fields): Form<Fields>,
) -> Response {
    let tab = table_tab_posted(&app, &instance, &asked);
    let back = view::tab_href(&instance, tab.as_deref().unwrap_or(view::SETTINGS));
    save_settings(&app, &headers, &instance, &fields, &back).await
}

/// `POST /plugins/{instance}/settings`: the form, from the plugin's area
/// under Manage ([`settings_tab`], [`table_tab`]), back to the tab it came
/// from there.
async fn set_settings_in_area(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(instance): Path<String>,
    Query(asked): Query<HashMap<String, String>>,
    Form(fields): Form<Fields>,
) -> Response {
    let tab = table_tab_posted(&app, &instance, &asked);
    let back = crate::area::href(
        &instance,
        AccessLevel::Admin,
        Some(tab.as_deref().unwrap_or(crate::area::SETTINGS)),
    );
    save_settings(&app, &headers, &instance, &fields, &back).await
}

/// The form, as one command to the conductor, from an admin of the plugin
/// (W6.11), whichever page it came from: its admins alone, a deployment
/// admin being one through All plugins (admin). What was typed into a
/// secret's field goes there and nowhere else: not into a log line, and not
/// back into a page, including the one saying it was refused.
async fn save_settings(
    app: &App,
    headers: &HeaderMap,
    instance: &str,
    fields: &Fields,
    back: &str,
) -> Response {
    let (session, records, access) = match gate_plugin(app, headers, instance, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if let Err(response) = form_token_matches(&session, fields) {
        return *response;
    }
    let Some(record) = settings_of(&records, instance) else {
        return no_such_plugin(instance);
    };
    let held = access.plugin(instance);
    // A table's cells, each checked as the conductor checks it, an external
    // account one the plugin reported and an instrument one the deployment
    // holds; nothing is sent while one does not read (W6.11, contract v14).
    let mut offered = choices(app, instance, record, &records).await;
    // An instrument typed is checked by its ID, not against the list the
    // grid offers, which holds the first thousand records at most.
    for id in settings::posted_instruments(record, fields) {
        if offered.instruments.iter().any(|(held, _)| *held == id) {
            continue;
        }
        match instruments::record_held(&app.bus, &id).await {
            Ok(true) => offered.instruments.push((id.clone(), id)),
            Ok(false) => {}
            Err(why) => offered.unread = why,
        }
    }
    let problems = settings::table_problems(record, fields, &offered);
    if !problems.is_empty() {
        return after_to(Err(settings::said(record, &problems)), back, back);
    }
    let Some(request) = settings::request(record, fields, crate::html::is_development()) else {
        return after_to(Ok(()), &format!("{back}&saved=none"), back);
    };
    // A setting serving several roles is set by an admin of every one
    // (W6.11, decisions/033 point 2): the dashboard, which evaluates access,
    // checks it before sending.
    let plugin_roles = access
        .known_roles
        .get(instance)
        .cloned()
        .unwrap_or_default();
    if let Some(refusal) = settings::not_administered(record, &request, &held, &plugin_roles) {
        return after_to(Err(refusal), back, back);
    }
    let outcome = command::<PluginSettingsRecord>(
        app,
        &session,
        "platform.config.command.set-plugin-settings",
        "meridian.v1.SetPluginSettingsRequest",
        request,
    )
    .await
    .map(|_| ());
    after_to(outcome, &format!("{back}&saved=1"), back)
}

/// What the dashboard's own tabs in a plugin's area under Manage show.
pub(crate) struct Manage<'a> {
    pub instance: &'a str,
    /// What its table settings' columns offer (W6.11, contract v14).
    pub choices: &'a settings::Choices,
    pub records: &'a AccessRecords,
    pub report: Option<&'a meridian_domain::v1::PluginReport>,
    /// The version the catalogue launched, where it launched the plugin.
    pub version: Option<&'a str>,
    pub session: &'a Session,
    pub notice: &'a str,
    pub now: i64,
    /// Its moves and archive as the conductor answers them, for a plugin
    /// keeping raw records (W6.9, contract v16); and the page of moves asked.
    pub moves: Option<&'a Result<meridian_domain::v1::ReadMovesReply, String>>,
    pub moves_cursor: &'a str,
}

/// The dashboard's Summary tab in a plugin's area under Manage, where Manage
/// opens (the product owner, 2026-10-01: "think Status, Connections, Account
/// Reached, and Last Read can be their own Summary page", core drawing it):
/// its status -- health, its why the badge's note, the version running and
/// the contract it registered with, and the place kept for what will change
/// them -- then the figures it reports, as tiles ([`crate::figures`]). The
/// status is core's to say, so a plugin cannot misreport it, and the figures
/// are only ever drawn as the plugin's, below it.
pub(crate) fn summary_tab(manage: &Manage) -> String {
    let state = crate::health::state(manage.report, manage.now);
    let (badge, why) = view::state_badge(&state, "status-note");
    let said = |value: Option<&str>, otherwise: &str| {
        escape(
            value
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .unwrap_or(otherwise),
        )
    };
    let version = said(
        manage.version,
        "not known: it was not launched from the catalogue",
    );
    let contract = said(
        manage.report.map(|report| report.contract_version.as_str()),
        "not said",
    );
    let figures = crate::figures::section(manage.report, manage.now);
    let declared = crate::declaration::summary(
        manage.report.and_then(|report| report.declaration.as_ref()),
        manage
            .report
            .map(|report| report.not_carried_seen.as_slice())
            .unwrap_or_default(),
    );
    // Its raw records, storage's and the archive's, and its moves (W6.9,
    // contract v16), for a plugin at the edge keeping them.
    let (records, moves) = match manage.moves {
        Some(moves) => crate::archive::sections(&crate::archive::Panel {
            instance: manage.instance,
            report: manage.report,
            records: manage.records,
            moves,
            deployment_admin: person_access(
                manage.records,
                &manage.session.subject,
                &manage.session.directory_groups,
            )
            .deployment_admin,
            token: &token_input(manage.session),
            cursor: manage.moves_cursor,
        }),
        None => (String::new(), String::new()),
    };
    let status = format!(
        "<section class=\"panel padded\" id=\"status\"><div class=\"row\"><h2>Status</h2>{badge}</div>{why}\
         <dl class=\"facts\"><dt>Version</dt><dd data-version>{version}</dd>\
         <dt>Contract</dt><dd data-contract>{contract}</dd></dl>\
         <p class=\"reserved\" data-reserved=\"lifecycle\">Restarting it, moving it to another version and \
         holding it at one will be here. They are not built yet.</p></section>{figures}"
    );
    format!(
        "{notice}{parts}",
        notice = notice_line(manage.notice),
        parts = summary_parts(&[
            ("status", "Status", status),
            ("records", "Raw records", records),
            ("moves", "Moves", moves),
            ("declared", "What it declares", declared),
            ("tools", "Tools for agents", tools_section(manage.report)),
        ]),
    )
}

/// The Summary's parts -- its status and figures, an edge plugin's raw
/// records and their moves (contract v16), what it declares, its tools -- as
/// tabs of their own where there are two or more, so the Summary fits one
/// screen (the product owner, 2026-10-04): one shown at a time, the one the
/// address names or else the first, its status. Without script every part
/// shows, under its own heading.
fn summary_parts(parts: &[(&str, &str, String)]) -> String {
    let shown: Vec<&(&str, &str, String)> = parts
        .iter()
        .filter(|(_, _, html)| !html.is_empty())
        .collect();
    if shown.len() < 2 {
        return shown.iter().map(|(_, _, html)| html.as_str()).collect();
    }
    let links: String = shown
        .iter()
        .map(|(id, title, _)| format!("<a href=\"#part-{id}\">{title}</a>"))
        .collect();
    let sections: String = shown
        .iter()
        .map(|(id, _, html)| format!("<div class=\"summary-part\" id=\"part-{id}\">{html}</div>"))
        .collect();
    format!(
        "<nav class=\"tabs summary-parts\" data-summary-parts aria-label=\"The Summary's parts\">{links}</nav>\
         {sections}<script>{SUMMARY_PARTS}</script>"
    )
}

/// One part of the Summary at a time, as Settings shows one group; the kit's
/// om-pager in a part shown is told to measure again, as on a resize.
const SUMMARY_PARTS: &str = r#"(function () {
  var nav = document.querySelector("nav[data-summary-parts]");
  if (!nav) return;
  var tabs = [].slice.call(nav.querySelectorAll("a[href^='#']"));
  var ids = tabs.map(function (a) { return a.getAttribute("href").slice(1); });
  function show() {
    var asked = decodeURIComponent(location.hash.slice(1));
    var want = ids.indexOf(asked) >= 0 ? asked : ids[0];
    tabs.forEach(function (a) {
      var on = a.getAttribute("href").slice(1) === want;
      a.classList.toggle("on", on);
      if (on) a.setAttribute("aria-current", "page"); else a.removeAttribute("aria-current");
    });
    ids.forEach(function (id) {
      var part = document.getElementById(id);
      if (part) part.hidden = id !== want;
    });
    // A pager in the part now shown measures the space it is given again.
    window.dispatchEvent(new Event("resize"));
  }
  window.addEventListener("hashchange", show);
  show();
})();"#;

/// The plugin's tools on the deployment's MCP surface (W4.8, W6.20, contract
/// v12): those its sidecar admitted, reads and acts apart, and each refused
/// with why (requirement 10). Nothing for a plugin declaring none.
fn tools_section(report: Option<&meridian_domain::v1::PluginReport>) -> String {
    let Some(report) = report else {
        return String::new();
    };
    if report.declared_tools.is_empty() && report.tool_refusals.is_empty() {
        return String::new();
    }
    let rows: String = report
        .declared_tools
        .iter()
        .map(|tool| {
            format!(
                "<li data-tool=\"{name}\"><code>{name}</code> {title} ({kind}, {method} {path})</li>",
                name = escape(&tool.name),
                title = escape(&tool.title),
                kind = if tool.reads { "reads" } else { "acts" },
                method = escape(&tool.method),
                path = escape(&tool.path),
            )
        })
        .collect();
    let refused: String = report
        .tool_refusals
        .iter()
        .map(|why| {
            format!(
                "<li class=\"refused\" data-tool-refused>{}</li>",
                escape(why)
            )
        })
        .collect();
    format!(
        "<section class=\"panel padded\" id=\"tools\"><h2>Tools for agents</h2>\
         <p class=\"hint\">What an agent a person delegated to may call on this deployment's \
         MCP surface, each at its route's levels.</p><ul>{rows}{refused}</ul></section>"
    )
}

/// The dashboard's Settings tab in a plugin's area under Manage (the product
/// owner, 2026-10-01: "build Settings and the status panel under Manage"):
/// the admin portal's settings form alone, posted to the area's own address.
/// Nothing of the deployment's and no account's data: a plugin admin's, of
/// this plugin alone.
pub(crate) fn settings_tab(manage: &Manage) -> String {
    let held = person_access(
        manage.records,
        &manage.session.subject,
        &manage.session.directory_groups,
    )
    .plugin(manage.instance);
    settings_section(
        manage.records,
        settings_of(manage.records, manage.instance),
        &token_input(manage.session),
        crate::html::is_development(),
        &crate::area::settings_path(manage.instance),
        manage.notice,
        Some(&held),
    )
}

/// The dashboard's tab for one of the plugin's table settings, in its area
/// under Manage (the product owner, 2026-10-05: each table setting a tab of
/// its own beside Settings, titled with its label): the table's page alone,
/// posted to the area's own address and back to this tab.
pub(crate) fn table_tab(manage: &Manage, key: &str) -> String {
    let development = crate::html::is_development();
    match settings_of(manage.records, manage.instance)
        .and_then(|record| Some((record, settings::table_named(record, key, development)?)))
    {
        Some((record, table)) => table_section(
            manage.records,
            record,
            table,
            &token_input(manage.session),
            &with_tab(&crate::area::settings_path(manage.instance), key),
            manage.choices,
            manage.notice,
        ),
        None => not_known_yet(manage.notice),
    }
}

fn not_known_yet(notice: &str) -> String {
    format!(
        "{}<section class=\"panel padded\" id=\"settings\"><p class=\"empty\">Its settings are not \
         known yet: the plugin has not reported what it needs.</p></section>",
        notice_line(notice)
    )
}

fn notice_line(notice: &str) -> String {
    if notice.is_empty() {
        String::new()
    } else {
        format!("<p class=\"notice good\">{}</p>", escape(notice))
    }
}

/// `path?tab=<key>`: where a table's page posts, so the save comes back to
/// its tab.
pub(crate) fn with_tab(path: &str, key: &str) -> String {
    let mut url = reqwest::Url::parse("http://dashboard.invalid/").expect("a fixed address");
    url.query_pairs_mut().append_pair("tab", key);
    format!("{path}?{}", url.query().unwrap_or_default())
}

/// A plugin's Settings tab, in its area and in the admin portal alike, to
/// fit one screen (the product owner, 2026-10-04): its head on one line --
/// "Settings" and who last changed them -- then the form, its groups as
/// tabs ([`settings::form_with`]); its tables are tabs of their own.
pub(crate) fn settings_section(
    records: &AccessRecords,
    record: Option<&PluginSettingsRecord>,
    token: &str,
    development: bool,
    action: &str,
    notice: &str,
    held: Option<&meridian_access::PluginHeld>,
) -> String {
    let Some(record) = record else {
        return not_known_yet(notice);
    };
    let plugin_roles = meridian_access::known_roles(records, &record.plugin_instance_id)
        .map(<[String]>::to_vec)
        .unwrap_or_default();
    format!(
        "{notice}<section class=\"panel padded settings-page\" id=\"settings\"><div class=\"settings-head\">\
         <h2>Settings</h2>{changed}</div>{form}</section>",
        notice = notice_line(notice),
        changed = last_changed(records, record),
        form = settings::form_with(
            record,
            token,
            development,
            action,
            held.map(|held| (held, plugin_roles.as_slice())),
        ),
    )
}

/// One of a plugin's table settings on its own tab, in its area and in the
/// admin portal alike ([`settings::table_form`]).
pub(crate) fn table_section(
    records: &AccessRecords,
    record: &PluginSettingsRecord,
    table: &meridian_pb::v1::SettingDeclaration,
    token: &str,
    action: &str,
    choices: &settings::Choices,
    notice: &str,
) -> String {
    format!(
        "{notice}<section class=\"panel padded settings-page\" id=\"table-setting\" data-table=\"{name}\">\
         {form}</section>",
        notice = notice_line(notice),
        name = escape(&table.name),
        form = settings::table_form(record, table, token, action, choices, &|subject| {
            crate::tickets::display_name(records, subject)
        }),
    )
}

/// The tab a table's page posted from, `?tab=<key>`, where it names one of
/// the plugin's table settings: where the save comes back to.
fn table_tab_posted(app: &App, instance: &str, asked: &HashMap<String, String>) -> Option<String> {
    let key = asked.get("tab")?;
    let records = app.records.current(app.clock.now_ns()).ok()?;
    let record = settings_of(&records, instance)?;
    settings::table_named(record, key, crate::html::is_development()).map(settings::table_key)
}

/// What a plugin's table settings offer (W6.11, contract v14): the external
/// accounts it reported or links, and the deployment's instrument records,
/// read only where a table has such a column.
pub(crate) async fn choices(
    app: &App,
    instance: &str,
    record: &PluginSettingsRecord,
    records: &AccessRecords,
) -> settings::Choices {
    use meridian_pb::v1::SettingColumnType;
    let mut offered = settings::Choices::default();
    if settings::wants(record, SettingColumnType::ExternalAccount) {
        let heard = app.custody.view();
        let mut seen = std::collections::BTreeSet::new();
        for account in heard.reported.get(instance).into_iter().flatten() {
            if seen.insert(account.external_account_id.clone()) {
                let shown = if account.name.is_empty() {
                    account.external_account_id.clone()
                } else {
                    format!("{} ({})", account.name, account.external_account_id)
                };
                offered
                    .external_accounts
                    .push((account.external_account_id.clone(), shown));
            }
        }
        for link in records
            .links
            .iter()
            .filter(|l| l.plugin_instance_id == instance)
        {
            if seen.insert(link.external_account_id.clone()) {
                offered.external_accounts.push((
                    link.external_account_id.clone(),
                    link.external_account_id.clone(),
                ));
            }
        }
    }
    if settings::wants(record, SettingColumnType::Instrument) {
        match instruments::every_record(&app.bus).await {
            Ok(found) => offered.instruments = found,
            Err(why) => offered.unread = why,
        }
    }
    offered
}

/// Who last changed a plugin's settings, and when (W6.11), as the conductor
/// recorded it: on the dashboard's form, the one way a person sets one, or
/// the plugin's re-declaring one with another type, or as secret or not,
/// which cleared it and names no person.
pub(crate) fn last_changed(records: &AccessRecords, record: &PluginSettingsRecord) -> String {
    if record.updated_by.is_empty() {
        if record.updated_at_ns == 0 {
            return String::new();
        }
        return changed_line(&format!(
            "Last changed by the plugin's re-declaring a setting, which cleared it, {}.",
            crate::custody::utc(record.updated_at_ns)
        ));
    }
    changed_line(&format!(
        "Last changed by {}, {}.",
        crate::tickets::display_name(records, &record.updated_by),
        crate::custody::utc(record.updated_at_ns)
    ))
}

/// A head's "last changed" on one line, cut short where it must be, the
/// whole of it on hover.
fn changed_line(said: &str) -> String {
    format!(
        "<span class=\"changed\" title=\"{said}\" data-last-changed>{said}</span>",
        said = escape(said)
    )
}

// ── The commands ────────────────────────────────────────────────────────────

/// Gate, check the form, and hand the fields to `act`.
macro_rules! admin_form {
    ($app:ident, $headers:ident, $fields:ident, $session:ident, $body:block) => {{
        let ($session, _) = match gate(&$app, &$headers, true) {
            Ok(gated) => gated,
            Err(response) => return *response,
        };
        if let Err(response) = form_token_matches(&$session, &$fields) {
            return *response;
        }
        after($body)
    }};
}

async fn define_account(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    admin_form!(app, headers, fields, session, {
        let request = DefineAccountRequest {
            account_id: field(&fields, "account_id").into(),
            name: field(&fields, "name").into(),
            custodian: field(&fields, "custodian").into(),
            account_type: field(&fields, "account_type").into(),
            owner: field(&fields, "owner").into(),
            note: field(&fields, "note").into(),
        };
        command::<meridian_domain::v1::AccountRecord>(
            &app,
            &session,
            "platform.config.command.define-account",
            "meridian.v1.DefineAccountRequest",
            request,
        )
        .await
        .map(|_| ())
    })
}

/// W9.13: an account's attributes in the book, for a deployment admin, each
/// one that changed its own act with the reason given.
async fn set_book(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    admin_form!(app, headers, fields, session, {
        let account_id = field(&fields, "account_id");
        match books::read(&app.bus).await {
            books::Books::NotAnswering(why) => {
                Err(format!("the book of record is not answering: {why}"))
            }
            books::Books::Read(held) => {
                let held = held.iter().find(|a| a.account_id == account_id);
                match books::requests(
                    account_id,
                    field(&fields, "base_currency_code"),
                    field(&fields, "lot_relief_default"),
                    field(&fields, "reason"),
                    held,
                ) {
                    Err(sentence) => Err(sentence),
                    Ok(asked) => {
                        let mut outcome = Ok(());
                        for request in asked {
                            if let Err(refused) =
                                books::set(&app.bus, &session.subject, request).await
                            {
                                outcome = Err(refused);
                                break;
                            }
                        }
                        outcome
                    }
                }
            }
        }
    })
}

async fn close_account(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    admin_form!(app, headers, fields, session, {
        let request = CloseAccountRequest {
            account_id: field(&fields, "account_id").into(),
        };
        command::<meridian_domain::v1::AccountRecord>(
            &app,
            &session,
            "platform.config.command.close-account",
            "meridian.v1.CloseAccountRequest",
            request,
        )
        .await
        .map(|_| ())
    })
}

/// Each `name` a form sent, whole, in order: one per box ticked.
fn every(pairs: &[(String, String)], name: &str) -> Vec<String> {
    pairs
        .iter()
        .filter(|(n, _)| n == name)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

/// A form's fields where each name is sent once; a repeated one keeps its
/// last, as `Form<Fields>` would.
fn once(pairs: &[(String, String)]) -> Fields {
    pairs.iter().cloned().collect()
}

/// Logins typed in: one per line, and on a line several split by commas
/// only where every part is a login (`issuer|subject`). A distinguished
/// name's commas are its own, so `ldap:dc=firm|uid=ada,ou=people` stays one.
fn typed_logins(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .flat_map(|line| {
            let parts: Vec<&str> = line.split(',').map(str::trim).collect();
            if parts.len() > 1 && parts.iter().all(|part| part.contains('|')) {
                parts.into_iter().map(String::from).collect()
            } else {
                vec![line.to_string()]
            }
        })
        .collect()
}

/// The people chosen (`login`, one per box, whole) and those typed in
/// (`logins`), once each, in that order.
fn logins_of(pairs: &[(String, String)]) -> Vec<String> {
    let mut logins = every(pairs, "login");
    for typed in every(pairs, "logins") {
        logins.extend(typed_logins(&typed));
    }
    let mut seen = HashSet::new();
    logins.retain(|login| seen.insert(login.clone()));
    logins
}

async fn define_user_group(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let fields = once(&pairs);
    admin_form!(app, headers, fields, session, {
        let request = DefineUserGroupRequest {
            user_group: Some(UserGroup {
                user_group_id: field(&fields, "user_group_id").into(),
                name: field(&fields, "name").into(),
                directory_groups: lines(&fields, "directory_groups"),
                logins: logins_of(&pairs),
            }),
        };
        command::<UserGroup>(
            &app,
            &session,
            "platform.config.command.define-user-group",
            "meridian.v1.DefineUserGroupRequest",
            request,
        )
        .await
        .map(|_| ())
    })
}

/// The page sends one `account_ids` per box ticked, and a `Fields` keeps only
/// the last of a repeated name, so they are gathered here into the one list
/// the older form typed.
async fn define_account_group(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let mut fields = Fields::new();
    for (name, value) in pairs {
        match fields.get_mut(&name) {
            Some(held) if name == "account_ids" => {
                held.push(',');
                held.push_str(&value);
            }
            _ => {
                fields.insert(name, value);
            }
        }
    }
    admin_form!(app, headers, fields, session, {
        let request = DefineAccountGroupRequest {
            account_group: Some(AccountGroup {
                account_group_id: field(&fields, "account_group_id").into(),
                name: field(&fields, "name").into(),
                account_ids: list(&fields, "account_ids"),
                built_in: false,
            }),
        };
        command::<AccountGroup>(
            &app,
            &session,
            "platform.config.command.define-account-group",
            "meridian.v1.DefineAccountGroupRequest",
            request,
        )
        .await
        .map(|_| ())
    })
}

/// A level as a typed entry or the form names it.
fn level_named(level: &str) -> Option<AccessLevel> {
    match level {
        "read" => Some(AccessLevel::Read),
        "write" => Some(AccessLevel::Write),
        "admin" => Some(AccessLevel::Admin),
        _ => None,
    }
}

/// "plugin read|write|admin", or from contract v15 "plugin role
/// read|write|admin", one per line: a plugin, the role of it the entry is on,
/// and a level, the same three levels for every plugin and role (decisions/026,
/// 027, 033). A line naming no role is on the plugin's one role where it holds
/// exactly one ([`entries_of`] fills it), or the plugin as a whole.
pub fn parse_entries(text: &str) -> Result<Vec<AccessEntry>, String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            let (plugin, role, level) = match parts[..] {
                [plugin, level] => (plugin, "", level),
                [plugin, role, level] => (plugin, role, level),
                _ => return Err(format!("`{line}` is not `plugin [role] read|write|admin`")),
            };
            let level = level_named(level)
                .ok_or_else(|| format!("`{level}` is not read, write or admin"))?;
            Ok(AccessEntry {
                plugin_instance_id: plugin.into(),
                level: level as i32,
                role: role.into(),
            })
        })
        .collect()
}

/// A row of the Access editor as the form names it: `instance:role`, the
/// role empty for a plugin holding none; or, as the form was before v15,
/// the instance alone.
fn row_of(named: &str) -> (&str, &str) {
    named.split_once(':').unwrap_or((named, ""))
}

/// The levels one row's choice gives: `read`, `write`, `admin`, or
/// `admin-read` or `admin-write` for admin beside one data level.
fn chosen_levels(row: &str, chosen: &str) -> Result<Vec<AccessLevel>, String> {
    Ok(match chosen {
        "admin-read" => vec![AccessLevel::Admin, AccessLevel::Read],
        "admin-write" => vec![AccessLevel::Admin, AccessLevel::Write],
        "" => return Err(format!("`{row}` has no level: choose admin, read or write")),
        other => {
            vec![level_named(other)
                .ok_or_else(|| format!("`{other}` is not admin, read or write"))?]
        }
    })
}

/// An access group's entries as the form sends them (W6.7, contract v15):
/// the Access editor's rows, one per plugin and role, each `level.{instance}:{role}`
/// at its one choice -- `read`, `write`, `admin`, `admin-read` or
/// `admin-write` -- a row left at nothing not in the group; or, as the form
/// was before, each plugin ticked (`plugin`) at `level.{plugin}`; and any typed
/// as `plugin [role] read|write|admin` lines (`entries`). An entry naming no
/// role on a plugin the records say holds exactly one is on that role, so a
/// form or a client built before v15 grants what it did. A plugin and role at
/// both read and write, or twice at one level, is refused: write includes
/// read.
pub fn entries_of(
    pairs: &[(String, String)],
    records: &AccessRecords,
) -> Result<Vec<AccessEntry>, String> {
    let fields = once(pairs);
    let mut entries = Vec::new();
    let ticked = every(pairs, "plugin");
    let rows: Vec<(String, String)> = if ticked.is_empty() {
        pairs
            .iter()
            .filter_map(|(name, value)| {
                let row = name.strip_prefix("level.")?;
                (!value.is_empty()).then(|| (row.to_string(), value.clone()))
            })
            .collect()
    } else {
        ticked
            .iter()
            .map(|row| {
                (
                    row.clone(),
                    field(&fields, &format!("level.{row}")).to_string(),
                )
            })
            .collect()
    };
    for (row, chosen) in rows {
        let (plugin, role) = row_of(&row);
        for level in chosen_levels(&row, &chosen)? {
            entries.push(AccessEntry {
                plugin_instance_id: plugin.to_string(),
                level: level as i32,
                role: role.to_string(),
            });
        }
    }
    for typed in every(pairs, "entries") {
        entries.extend(parse_entries(&typed)?);
    }
    for entry in &mut entries {
        if entry.role.is_empty() {
            if let Some([one]) = meridian_access::known_roles(records, &entry.plugin_instance_id) {
                entry.role = one.clone();
            }
        }
    }
    let data = |level: i32| level != AccessLevel::Admin as i32;
    let mut seen: HashSet<(String, String, bool)> = HashSet::new();
    for entry in &entries {
        if !seen.insert((
            entry.plugin_instance_id.clone(),
            entry.role.clone(),
            data(entry.level),
        )) {
            return Err(format!(
                "`{}` is named twice at a data level, or twice at admin; an access group gives \
                 each plugin's role admin and at most one of read and write, and write includes \
                 read",
                crate::delegation::row_named(&entry.plugin_instance_id, &entry.role)
            ));
        }
    }
    Ok(entries)
}

async fn define_access_group(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let fields = once(&pairs);
    let records = app.records.current(app.clock.now_ns()).unwrap_or_default();
    admin_form!(app, headers, fields, session, {
        match entries_of(&pairs, &records) {
            Err(sentence) => Err(sentence),
            Ok(entries) => {
                let request = DefineAccessGroupRequest {
                    access_group: Some(AccessGroup {
                        access_group_id: field(&fields, "access_group_id").into(),
                        name: field(&fields, "name").into(),
                        entries,
                        built_in: false,
                    }),
                };
                command::<AccessGroup>(
                    &app,
                    &session,
                    "platform.config.command.define-access-group",
                    "meridian.v1.DefineAccessGroupRequest",
                    request,
                )
                .await
                .map(|_| ())
            }
        }
    })
}

async fn grant(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    admin_form!(app, headers, fields, session, {
        let request = GrantPermissionRequest {
            user_group_id: field(&fields, "user_group_id").into(),
            account_group_id: field(&fields, "account_group_id").into(),
            access_group_id: field(&fields, "access_group_id").into(),
        };
        command::<meridian_domain::v1::Permission>(
            &app,
            &session,
            "platform.config.command.grant-permission",
            "meridian.v1.GrantPermissionRequest",
            request,
        )
        .await
        .map(|_| ())
    })
}

async fn withdraw(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    admin_form!(app, headers, fields, session, {
        let request = WithdrawPermissionRequest {
            permission_id: field(&fields, "permission_id").into(),
        };
        let reply: Result<WithdrawPermissionReply, String> = command(
            &app,
            &session,
            "platform.config.command.withdraw-permission",
            "meridian.v1.WithdrawPermissionRequest",
            request,
        )
        .await;
        match reply {
            Ok(reply) if reply.withdrawn => Ok(()),
            Ok(reply) => Err(reply.refusal_reason),
            Err(sentence) => Err(sentence),
        }
    })
}

/// The chrome of the Instruments pages: Settings, then where in them.
fn instruments_chrome<'a>(session: &'a Session, record: Option<&str>) -> Chrome<'a> {
    let crumbs = match record {
        None => format!(
            "{}{}",
            crate::html::crumb_link("/admin", "Settings"),
            crate::html::crumb_here("Instruments", None)
        ),
        Some(id) => format!(
            "{}{}{}",
            crate::html::crumb_link("/admin", "Settings"),
            crate::html::crumb_link("/admin/instruments", "Instruments"),
            crate::html::crumb_here(id, None)
        ),
    };
    Chrome {
        crumbs,
        // The deployment's instrument records are the instrument store's.
        report: crate::html::Report::Concerning("instrument"),
        ..admin_chrome(session)
    }
}

/// The Instruments page's refusal, said as every refusal here is, with
/// "Report a problem" beside it filled with what the page knows: the
/// instrument store, the row it sent, and the refusal in its words (W6.21,
/// Q12: the page's last refusal, on the dashboard's own pages).
fn instruments_refused(
    app: &App,
    session: &Session,
    sentence: &str,
    operation: &str,
    back: &str,
) -> Response {
    let access = match app.records.current(app.clock.now_ns()) {
        Ok(records) => person_access(&records, &session.subject, &session.directory_groups),
        Err(_) => Access::default(),
    };
    let form = crate::tickets::pages::report_form(
        session,
        &access,
        &crate::tickets::pages::Prefill {
            concerns: "instrument".into(),
            operation: operation.into(),
            seen: format!("The Instruments page refused: {sentence}"),
            ..Default::default()
        },
    );
    (
        StatusCode::BAD_REQUEST,
        Html(page_with(
            "Not done",
            &format!(
                "<h1>Not done</h1><p class=\"refused\">{}</p>\
                 <p><a href=\"{}\" onclick=\"history.back();return false\">Back</a></p>\
                 <details class=\"report-refusal\"><summary>Report a problem with this</summary>{form}</details>",
                escape(sentence),
                escape(back)
            ),
            &instruments_chrome(session, None),
        )),
    )
        .into_response()
}

/// W3.11: the Instruments page, for a deployment admin.
async fn instruments_page(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(query): Query<Fields>,
) -> Response {
    let (session, _) = match gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let listed = instruments::list(&app.bus, "").await;
    let body = instruments::list_page(&listed, &token_input(&session), field(&query, "done"));
    Html(page_with(
        "Instruments",
        &body,
        &instruments_chrome(&session, None),
    ))
    .into_response()
}

/// One record, as its page draws it: the record and what it lacks, and its
/// history; or the sentence saying why not.
async fn one_record(
    app: &App,
    instrument: &str,
) -> Result<
    (
        meridian_domain::v1::InstrumentToComplete,
        Vec<meridian_domain::v1::InstrumentVersion>,
    ),
    String,
> {
    let listed = instruments::list(&app.bus, instrument).await?;
    let item = listed
        .instruments
        .into_iter()
        .next()
        .ok_or_else(|| format!("the deployment holds no record {instrument}"))?;
    let versions = instruments::history(&app.bus, instrument)
        .await
        .map(|history| history.versions)
        .unwrap_or_default();
    Ok((item, versions))
}

/// W3.10, W3.12: one record's page.
async fn instrument_page(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(instrument): Path<String>,
    Query(query): Query<Fields>,
) -> Response {
    let (session, _) = match gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let (item, versions) = match one_record(&app, &instrument).await {
        Ok(found) => found,
        Err(why) => return status_page(StatusCode::NOT_FOUND, "No such record", &why),
    };
    let done = field(&query, "done");
    let mut body = instruments::record_page(&item, &versions, None, &token_input(&session));
    if !done.is_empty() {
        body = format!("<p class=\"passed\">{}</p>{body}", escape(done));
    }
    Html(page_with(
        &instrument,
        &body,
        &instruments_chrome(&session, Some(&instrument)),
    ))
    .into_response()
}

/// W3.10: one record's form, sent for the person signed in.
async fn complete_instruments(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, _) = match gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if let Err(response) = form_token_matches(&session, &fields) {
        return *response;
    }
    let filled = instruments::Filled {
        instrument_id: field(&fields, "instrument_id").into(),
        against_version: field(&fields, "against_version").into(),
        asset_class: field(&fields, "asset_class").into(),
        asset_class_source: field(&fields, "asset_class_source").into(),
        currency: field(&fields, "currency").into(),
        currency_source: field(&fields, "currency_source").into(),
        description: field(&fields, "description").into(),
        description_source: field(&fields, "description_source").into(),
        instrument_type: field(&fields, "instrument_type").into(),
        instrument_type_source: field(&fields, "instrument_type_source").into(),
        fund_category: field(&fields, "fund_category").into(),
        fund_investors: field(&fields, "fund_investors").into(),
        fund_nav: field(&fields, "fund_nav").into(),
        fund_liquidity_fee: field(&fields, "fund_liquidity_fee").into(),
        fund_source: field(&fields, "fund_source").into(),
        identifier_scheme: field(&fields, "identifier_scheme").into(),
        identifier_value: field(&fields, "identifier_value").into(),
        identifier_namespace: field(&fields, "identifier_namespace").into(),
        identifier_source: field(&fields, "identifier_source").into(),
        note: field(&fields, "note").into(),
        asset_class_held: field(&fields, "asset_class_held").into(),
        currency_held: field(&fields, "currency_held").into(),
        description_held: field(&fields, "description_held").into(),
        instrument_type_held: field(&fields, "instrument_type_held").into(),
        fund_held: field(&fields, "fund_held").into(),
    };
    let back = format!("/admin/instruments/{}", filled.instrument_id);
    let outcome = match instruments::completion(&filled) {
        Err(why) => Err(why),
        Ok(request) => match instruments::complete(&app.bus, &session.subject, request).await {
            Err(refused) => Err(refused),
            Ok(reply) => instruments::outcome(&reply),
        },
    };
    match outcome {
        Ok(said) => after_to(Ok(()), &format!("{back}?done={}", query_text(&said)), &back),
        Err(why) => instruments_refused(&app, &session, &why, "CompleteInstruments", &back),
    }
}

/// W3.10: the values offered for the records ticked, accepted in one command.
async fn accept_offers(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let (session, _) = match gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let fields: Fields = pairs.iter().cloned().collect();
    if let Err(response) = form_token_matches(&session, &fields) {
        return *response;
    }
    let chosen: Vec<String> = pairs
        .iter()
        .filter(|(name, _)| name == "instrument_id")
        .map(|(_, value)| value.clone())
        .collect();
    let outcome = match instruments::list(&app.bus, "").await {
        Err(why) => Err(format!("the instrument store is not answering: {why}")),
        Ok(listed) => {
            let request = instruments::accepting(&listed.instruments, &chosen);
            if request.completions.is_empty() {
                Err("nothing offered for the records ticked".to_string())
            } else {
                match instruments::complete(&app.bus, &session.subject, request).await {
                    Err(refused) => Err(refused),
                    Ok(reply) => instruments::outcome(&reply),
                }
            }
        }
    };
    match outcome {
        Ok(said) => after_to(
            Ok(()),
            &format!("/admin/instruments?done={}", query_text(&said)),
            "/admin/instruments",
        ),
        Err(why) => instruments_refused(
            &app,
            &session,
            &why,
            "CompleteInstruments",
            "/admin/instruments",
        ),
    }
}

/// W3.13: a conflict's records merged, the one chosen staying.
async fn merge_instruments(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, _) = match gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if let Err(response) = form_token_matches(&session, &fields) {
        return *response;
    }
    let kept = field(&fields, "kept_instrument_id").to_string();
    let merged = field(&fields, "instrument_ids")
        .split(',')
        .map(str::trim)
        .find(|id| !id.is_empty() && *id != kept)
        .unwrap_or_default()
        .to_string();
    let outcome = async {
        if kept.is_empty() || merged.is_empty() {
            return Err("the form names no two records".to_string());
        }
        let version = |id: String| {
            let app = &app;
            async move {
                instruments::list(&app.bus, &id)
                    .await?
                    .instruments
                    .first()
                    .and_then(|item| item.instrument.as_ref())
                    .map(|record| record.version)
                    .ok_or_else(|| format!("the deployment holds no record {id}"))
            }
        };
        let request = meridian_domain::v1::MergeInstrumentsRequest {
            kept_version: version(kept.clone()).await?,
            merged_version: version(merged.clone()).await?,
            kept_instrument_id: kept.clone(),
            merged_instrument_id: merged.clone(),
            take_from_merged: Vec::new(),
            note: field(&fields, "note").to_string(),
        };
        instruments::merge(&app.bus, &session.subject, request)
            .await
            .map(|_| format!("{merged} merged into {kept}"))
    }
    .await;
    match outcome {
        Ok(said) => after_to(
            Ok(()),
            &format!("/admin/instruments?done={}", query_text(&said)),
            "/admin/instruments",
        ),
        Err(why) => instruments_refused(
            &app,
            &session,
            &why,
            "MergeInstruments",
            "/admin/instruments",
        ),
    }
}

/// W3.3: ask the platform about a record, when the person chooses to; the
/// answer is drawn on the record's page, and kept by the instrument store.
async fn ask_the_platform(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(instrument): Path<String>,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, _) = match gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if let Err(response) = form_token_matches(&session, &fields) {
        return *response;
    }
    let (item, _) = match one_record(&app, &instrument).await {
        Ok(found) => found,
        Err(why) => return status_page(StatusCode::NOT_FOUND, "No such record", &why),
    };
    let asked =
        instruments::ask_platform(&app.bus, &item.instrument.clone().unwrap_or_default()).await;
    // Read again: what the platform answered is kept on the record as it
    // arrives, and the page shows what is kept by then.
    let (item, versions) = one_record(&app, &instrument)
        .await
        .unwrap_or((item, Vec::new()));
    let body = instruments::record_page(&item, &versions, Some(&asked), &token_input(&session));
    Html(page_with(
        &instrument,
        &body,
        &instruments_chrome(&session, Some(&instrument)),
    ))
    .into_response()
}

/// Text for a query string: what is not a letter, a digit or a few marks,
/// percent-encoded.
pub(crate) fn query_text(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            b' ' => "+".to_string(),
            other => format!("%{other:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests;

/// W6.25: a deployment admin sets, changes or clears (0 days) a hold on raw
/// records, from the deployment's Settings.
async fn set_hold(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    admin_form!(app, headers, fields, session, {
        match field(&fields, "days").parse::<u32>() {
            Err(_) => Err(format!(
                "{:?} is not a number of days",
                field(&fields, "days")
            )),
            Ok(days) => command::<meridian_domain::v1::Hold>(
                &app,
                &session,
                "platform.config.command.set-hold",
                "meridian.v1.SetHoldRequest",
                meridian_domain::v1::SetHoldRequest {
                    role: field(&fields, "role").to_string(),
                    days,
                    write_once: !field(&fields, "write_once").is_empty(),
                },
            )
            .await
            .map(|_| ()),
        }
    })
}
