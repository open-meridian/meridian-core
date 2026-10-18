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
        .route("/admin/agents", post(set_agents_may))
        .route("/admin/agents/lapse", post(set_lapse))
        .route(
            "/admin/plugins/{instance}/agents",
            post(set_plugin_agents_may),
        )
        .route("/admin/plugins/{instance}/accounts", post(set_linkable))
        .route(
            "/admin/plugins/{instance}/connectivity/available",
            post(connectivity::available),
        )
        .route(
            "/admin/plugins/{instance}/connectivity/link",
            post(connectivity::link),
        )
        .route(
            "/admin/trading-instructions",
            get(trading_page).post(set_trading_instruction),
        )
        .route(trading::LOCAL_RECORDS_PATH, post(set_local_record_orders))
        .route("/admin/time-zone", post(set_deployment_time_zone))
        .route("/admin/data-sources", get(data_sources_page))
        .route("/admin/data-sources/licence", post(set_dataset_licence))
        .route(
            "/admin/data-sources/entitlement",
            post(set_dataset_entitlement),
        )
        .route("/admin/data-sources/priority", post(set_source_priority))
        .route("/admin/instruments", get(instruments_page))
        .route("/admin/instruments/complete", post(complete_instruments))
        .route("/admin/instruments/accept", post(accept_offers))
        .route("/admin/instruments/merge", post(merge_instruments))
        .route("/admin/instruments/search", get(search_instruments))
        .route(instruments::USAGE_PATH, post(set_reports_usage))
        .route(
            instruments::LICENSED_SCHEMES_PATH,
            post(set_licensed_schemes),
        )
        .route(instruments::LINES_PATH, post(apply_migration_lines))
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

/// What a dialog of core's posted asks for (contract v20): its act prepared
/// for someone to make (W6.26, the plan's Q20), or done -- where the act's
/// second pair of eyes lets the person do it alone (W6.29).
pub(crate) enum Posted {
    /// Prepared: the change's page is where the person goes.
    Prepared(String),
    /// Not prepared, or not to be done alone: the sentence says why.
    Refused(String),
    /// Done as the dialog always did it.
    Go,
}

/// A dialog's fields as `tool`'s arguments, prepared where it posted the
/// reserved field `prepare`, checked by the act's own code and kept with
/// nothing changed; otherwise whether the act's second pair of eyes refuses
/// the person doing it alone.
pub(crate) async fn prepare_or_gate(
    app: &App,
    session: &Session,
    fields: &Fields,
    tool: &str,
    arguments: impl FnOnce() -> Result<serde_json::Value, String>,
) -> Posted {
    let caller = crate::prepared::person_caller(
        &session.subject,
        &session.display_name,
        &session.directory_groups,
    );
    if !field(fields, crate::prepared::handoff::PREPARE_FIELD).is_empty() {
        let args = match arguments() {
            Ok(args) => args,
            Err(why) => return Posted::Refused(why),
        };
        return match crate::prepared::prepare(app, &caller, tool, args, field(fields, "note")).await
        {
            Ok(kept) => Posted::Prepared(kept.prepared_call_id),
            Err(refusal) => Posted::Refused(
                refusal
                    .get("detail")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("not prepared")
                    .to_string(),
            ),
        };
    }
    if crate::prepared::applies(app, &caller, tool, true)
        .await
        .second_pair_of_eyes
    {
        return Posted::Refused(
            "this change needs a second pair of eyes: prepare it, and someone other than you \
             makes it"
                .into(),
        );
    }
    Posted::Go
}

/// The way a dialog's post goes on from [`prepare_or_gate`]: to the
/// prepared change's page, or the sentence saying why not.
pub(crate) fn posted_to(posted: Posted, back: &str) -> Option<Response> {
    match posted {
        Posted::Prepared(id) => Some(after_to(Ok(()), &format!("/prepared/{id}"), back)),
        Posted::Refused(why) => Some(after_to(Err(why), back, back)),
        Posted::Go => None,
    }
}

/// The page saying a change was not done, in the store's sentence, with a
/// way back to `back`.
pub(crate) fn not_done(sentence: &str, back: &str) -> Response {
    after_to(Err(sentence.to_string()), back, back)
}

/// To `done` when it was, and otherwise the store's sentence, with a way
/// back to `back`.
pub(crate) fn after_to(outcome: Result<(), String>, done: &str, back: &str) -> Response {
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
pub(crate) use books::{method_of as books_method_of, method_value as books_method_value};
pub mod calendars;
pub mod connectivity;
pub mod data_sources;
pub mod event_groups;
pub mod holds;
pub mod instruments;
pub(crate) mod overview;
pub mod time_zone;
pub(crate) use overview::listing;
pub mod people;
pub(crate) mod picker;
pub mod trading;

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
    let levels = app.prepared.levels().await.unwrap_or_default();
    let agents = agents::deployment_section(
        &agents::acts(&app),
        &levels,
        app.prepared.lapse_days().await,
        &records,
        &token_input(&session),
    );
    // Service accounts (W6.32, contract v21): their tab, and each one a user
    // group may list in its logins by its SA- ID, as a person is.
    let accounts = app.service_accounts.accounts().await.unwrap_or_default();
    let chosen = app.service_accounts.runs_as().await.unwrap_or_default();
    let service_accounts = crate::service_accounts::pages::settings_section(
        &crate::service_accounts::owner_names(&app, &records, &accounts).await,
        &accounts,
        &chosen,
        &people,
        &session.subject,
        &token_input(&session),
    );
    let mut people = people;
    people.extend(
        accounts
            .iter()
            .filter(|account| account.state != crate::service_accounts::store::State::Removed)
            .map(|account| people::Person {
                login: account.service_account_id.clone(),
                user_id: account.service_account_id.clone(),
                name: format!("{} (service account)", account.name),
            }),
    );
    // Contract v26 (W6.35): the deployment's time zone, its tab.
    let zone = time_zone::read(&app.bus).await;
    if let Err(failed) = &zone {
        tracing::warn!(%failed, "the deployment's time zone could not be read");
    }
    let body = overview::render(
        &records,
        &delegating,
        &lines,
        &people,
        &books,
        agents,
        service_accounts,
        zone.as_ref().ok(),
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
    // The Settings form started from a prepared change (W6.27): its values
    // as prepared, "prepared by" and the reason shown, posted with its ID.
    let caller = crate::prepared::person_caller(
        &session.subject,
        &session.display_name,
        &session.directory_groups,
    );
    let mut prepared: Option<crate::prepared::store::Kept> = None;
    if current.key == view::SETTINGS {
        if let Some(id) =
            Some(field(&query, "prepared")).filter(|id| crate::prepared::is_prepared_call_id(id))
        {
            if let Ok(Some(kept)) = app.prepared.get(id).await {
                if kept.tool_name == "dashboard__set_plugin_settings"
                    && crate::prepared::instance_named(&kept).as_deref() == Some(instance.as_str())
                {
                    if let Some(act) = crate::prepared::resolve_kept(&app, &kept, &records).await {
                        let eyes = app.prepared.eyes(&app, &act, &kept).await;
                        if crate::prepared::Prepared::may_make(
                            &app,
                            &records,
                            &act,
                            &kept,
                            eyes,
                            &session.subject,
                        )
                        .is_ok()
                        {
                            prepared = Some(kept);
                        }
                    }
                }
            }
        }
    }
    let eyes = crate::prepared::applies(&app, &caller, "dashboard__set_plugin_settings", true)
        .await
        .second_pair_of_eyes;
    let started: Option<PluginSettingsRecord> = prepared
        .as_ref()
        .and_then(|kept| settings_of(&records, &instance).map(|record| started_from(record, kept)));
    let token = match &prepared {
        Some(kept) => format!(
            "{}<input type=\"hidden\" name=\"prepared_call_id\" value=\"{}\">",
            token_input(&session),
            escape(&kept.prepared_call_id)
        ),
        None => token_input(&session),
    };
    let choices = match settings_of(&records, &instance) {
        Some(record) => choices(&app, &instance, record, &records).await,
        None => settings::Choices::default(),
    };
    // A connectivity plugin's ems, or an ems's plugins and map (W11.21,
    // W11.22, contract v22).
    let connectivity_part = {
        let names: Vec<(String, String)> = app
            .service_accounts
            .accounts()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|account| (account.service_account_id, account.name))
            .collect();
        let administers_ems = |ems: &str| access.plugin(ems).administers("ems");
        connectivity::part(
            &records,
            &instance,
            &connectivity::Viewer {
                deployment_admin: access.deployment_admin,
                administers_ems: &administers_ems,
            },
            &names,
            &token_input(&session),
        )
    };
    let body = view::render(&view::View {
        line,
        choices: &choices,
        record: started.as_ref().or(settings_of(&records, &instance)),
        report,
        records: &records,
        custody: &custody,
        token: &token,
        notice,
        development: crate::html::is_development(),
        tabs: &tabs,
        current,
        area: administers.then(|| view::area_at_admin(&instance)),
        may_grant: access.deployment_admin,
        held: &access.plugin(&instance),
        agents: {
            let held = access.plugin(&instance);
            agents::plugin_section(
                &instance,
                &agents::acts(&app),
                &app.prepared.levels().await.unwrap_or_default(),
                &records,
                &|line: &agents::ActLine| held.administers_every(&line.roles),
                &token_input(&session),
            )
        },
        accounts: agents::accounts_section(
            &instance,
            &records,
            access.deployment_admin,
            &token_input(&session),
        ),
        connectivity: connectivity_part,
        runs_as: {
            let held = access.plugin(&instance);
            // The held view of the accounts, read afresh for the choices.
            let _ = app.service_accounts.accounts().await;
            crate::service_accounts::pages::runs_as_part(
                &app,
                &records,
                &instance,
                &app.service_accounts.runs_as().await.unwrap_or_default(),
                &|role: &str| held.administers(role),
                &token_input(&session),
            )
        },
    });
    let mut body = body;
    if let Some(kept) = &prepared {
        body = body.replacen(
            "<section class=\"panel padded settings-page\"",
            &format!(
                "{}<section class=\"panel padded settings-page\"",
                crate::prepared::pages::band(kept)
            ),
            1,
        );
    }
    // Under a second pair of eyes nobody saves alone: the form offers only
    // Prepare, unless it was opened from someone else's prepared change.
    if eyes && prepared.is_none() {
        body = body.replace(SAVE_SETTINGS, "").replace(SAVE_TABLE, "");
    }
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
    let caller = crate::prepared::person_caller(
        &session.subject,
        &session.display_name,
        &session.directory_groups,
    );
    // Prepare (W6.26, the plan's Q20): the form as the settings tool's
    // arguments, checked by the same code, kept for someone to make.
    if !field(fields, "prepare").is_empty() {
        let outcome = match settings_args(instance, fields) {
            Err(why) => Err(why),
            Ok(args) => crate::prepared::prepare(
                app,
                &caller,
                "dashboard__set_plugin_settings",
                args,
                field(fields, "note"),
            )
            .await
            .map(|kept| kept.prepared_call_id)
            .map_err(|refusal| {
                refusal
                    .get("detail")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("not prepared")
                    .to_string()
            }),
        };
        return match outcome {
            Ok(id) => after_to(Ok(()), &format!("/prepared/{id}"), back),
            Err(why) => after_to(Err(why), back, back),
        };
    }
    // Made from a prepared change the page was opened from, where the person
    // may make it (W6.27); and the act's second pair of eyes, which refuses
    // a save made alone (W6.29).
    let made_from = match fields.get("prepared_call_id").filter(|id| !id.is_empty()) {
        None => None,
        Some(id) => {
            let kept = app.prepared.get(id).await.ok().flatten();
            let Some(kept) = kept.filter(|kept| kept.tool_name == "dashboard__set_plugin_settings")
            else {
                return after_to(
                    Err(format!("no prepared change {id} of these settings waits")),
                    back,
                    back,
                );
            };
            let act = crate::prepared::resolve_kept(app, &kept, &records).await;
            let Some(act) = act else {
                return after_to(
                    Err(format!("{id} names an act no longer offered")),
                    back,
                    back,
                );
            };
            let eyes = app.prepared.eyes(app, &act, &kept).await;
            if let Err(why) = crate::prepared::Prepared::may_make(
                app,
                &records,
                &act,
                &kept,
                eyes,
                &session.subject,
            ) {
                return after_to(Err(why), back, back);
            }
            Some(kept)
        }
    };
    if made_from.is_none()
        && crate::prepared::applies(app, &caller, "dashboard__set_plugin_settings", true)
            .await
            .second_pair_of_eyes
    {
        return after_to(
            Err("these settings need a second pair of eyes: prepare the change, and someone other than you makes it".into()),
            back,
            back,
        );
    }
    let mut request = match settings_change(app, record, &records, &access, fields).await {
        Err(SettingsRefused::Cells(_, said) | SettingsRefused::NotAdministered(_, said)) => {
            return after_to(Err(said), back, back)
        }
        Ok(None) => return after_to(Ok(()), &format!("{back}&saved=none"), back),
        Ok(Some(request)) => request,
    };
    // Optional at the page (contract v17, the plan's Q8).
    request.note = field(fields, "note").trim().to_string();
    let outcome = match &made_from {
        // Made from a prepared change: stamped with it (W4.9, W6.27).
        Some(kept) => {
            crate::mcp::Acting::making(
                &kept.prepared_call_id,
                command_stamped::<PluginSettingsRecord>(
                    app,
                    &session,
                    "platform.config.command.set-plugin-settings",
                    "meridian.v1.SetPluginSettingsRequest",
                    request,
                ),
            )
            .await
        }
        None => {
            command::<PluginSettingsRecord>(
                app,
                &session,
                "platform.config.command.set-plugin-settings",
                "meridian.v1.SetPluginSettingsRequest",
                request,
            )
            .await
        }
    }
    .map(|_| ());
    if let (Ok(()), Some(kept)) = (&outcome, &made_from) {
        if let Ok(made) = settings_args(instance, fields) {
            crate::prepared::settle_from_page(app, &caller, &kept.prepared_call_id, &made).await;
        }
    }
    after_to(outcome, &format!("{back}&saved=1"), back)
}

/// The settings record as a prepared change would leave it: each value it
/// names as prepared, each table's rows as prepared (W6.27).
fn started_from(
    record: &PluginSettingsRecord,
    kept: &crate::prepared::store::Kept,
) -> PluginSettingsRecord {
    let args: serde_json::Value = serde_json::from_str(&kept.arguments_json).unwrap_or_default();
    let mut started = record.clone();
    let mut set = |name: &str, value: String| {
        started.values.retain(|held| held.name != name);
        started
            .values
            .push(meridian_domain::v1::PluginSettingValue {
                name: name.to_string(),
                value,
            });
    };
    if let Some(values) = args.get("value").and_then(serde_json::Value::as_object) {
        for (name, value) in values {
            set(
                name,
                value
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| value.to_string()),
            );
        }
    }
    if let Some(tables) = args.get("table").and_then(serde_json::Value::as_object) {
        for (name, rows) in tables {
            let rows: Vec<meridian_domain::setting_table::Cells> = rows
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|row| row.as_object())
                .map(|row| {
                    row.iter()
                        .map(|(column, cell)| {
                            (
                                column.clone(),
                                cell.as_str()
                                    .map(str::to_string)
                                    .unwrap_or_else(|| cell.to_string()),
                            )
                        })
                        .collect()
                })
                .collect();
            set(name, meridian_domain::setting_table::cells_written(&rows));
        }
    }
    started
}

/// A settings form's fields as the settings tool's arguments (contract
/// v20): the form's own grammar as JSON, one record from the form and the
/// tool, so a prepared change compares with the made one. A secret's value
/// is never prepared: refused by name.
pub(crate) fn settings_args(instance: &str, fields: &Fields) -> Result<serde_json::Value, String> {
    use serde_json::{json, Map, Value};
    let mut value = Map::new();
    let mut table: std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<usize, Map<String, Value>>,
    > = Default::default();
    let mut clear = Map::new();
    for (name, said) in fields {
        if let Some(setting) = name.strip_prefix(settings::VALUE_FIELD) {
            value.insert(setting.to_string(), Value::String(said.clone()));
        } else if let Some(setting) = name.strip_prefix(settings::CLEAR_FIELD) {
            clear.insert(setting.to_string(), Value::Bool(true));
        } else if let Some(setting) = name.strip_prefix(settings::SECRET_FIELD) {
            if !said.is_empty() {
                return Err(format!(
                    "secret.{setting}: a secret's value is never prepared; a person enters it at \
                     the Settings form and saves it"
                ));
            }
        } else if let Some(rest) = name.strip_prefix(settings::TABLE_FIELD) {
            match rest.split_once('[') {
                None => {
                    table.entry(rest.to_string()).or_default();
                }
                Some((setting, cell)) => {
                    let Some((row, column)) = cell.split_once("].") else {
                        continue;
                    };
                    let Ok(row) = row.parse::<usize>() else {
                        continue;
                    };
                    table
                        .entry(setting.to_string())
                        .or_default()
                        .entry(row)
                        .or_default()
                        .insert(column.to_string(), Value::String(said.clone()));
                }
            }
        }
    }
    let against: i64 = field(fields, settings::AGAINST_FIELD).parse().unwrap_or(0);
    let mut args = json!({
        "plugin_instance_id": instance,
        // A decimal string: a stamp above 2^53 loses digits as a number (O3).
        "against_updated_at_ns": against.to_string(),
        "note": field(fields, "note"),
    });
    if !value.is_empty() {
        args["value"] = Value::Object(value);
    }
    if !clear.is_empty() {
        args["clear"] = Value::Object(clear);
    }
    if !table.is_empty() {
        args["table"] = Value::Object(
            table
                .into_iter()
                .map(|(name, rows)| {
                    (
                        name,
                        Value::Array(rows.into_values().map(Value::Object).collect()),
                    )
                })
                .collect(),
        );
    }
    Ok(args)
}

/// [`command`], stamped with the prepared change the code making it runs
/// from ([`crate::mcp::Acting`], contract v20).
async fn command_stamped<Rep: Message + Default>(
    app: &App,
    session: &Session,
    topic: &str,
    request_type: &str,
    request: impl Message,
) -> Result<Rep, String> {
    let stamp = meridian_bus::Stamp {
        acting_for_subject: session.subject.clone(),
        prepared_call_id: crate::mcp::Acting::now().prepared_call_id,
        ..meridian_bus::Stamp::default()
    };
    let answered = app
        .bus
        .call_stamped(
            topic,
            request_type,
            request.encode_to_vec(),
            None,
            Some(Duration::from_secs(10)),
            &stamp,
        )
        .await;
    let (_, bytes) = answered.map_err(|failed| match failed {
        BusError::HandlerFailed { detail, .. } => detail,
        other => other.to_string(),
    })?;
    if let Err(failed) = records::refresh(&app.bus, &app.records, app.clock.as_ref()).await {
        tracing::warn!("the records could not be re-read after a change: {failed}");
    }
    Rep::decode(&bytes[..]).map_err(|failed| format!("an undecodable reply: {failed}"))
}

/// Why a settings change was not sent (W6.11): cells that do not read, each
/// by its path in the form's grammar without the `table.` prefix
/// (`<setting>[<row>].<column>`), with the sentence the page shows; or a
/// setting serving a role the person does not administer, by its name.
pub(crate) enum SettingsRefused {
    Cells(Vec<meridian_domain::setting_table::Problem>, String),
    NotAdministered(String, String),
}

/// The change a form's fields ask for, checked before anything is sent: one
/// code path for the Settings form and core's settings tool (W6.20, contract
/// v17), so a cell refused on the page and an argument refused through
/// `/mcp` are refused by the same code with the same path. A table's cells
/// are checked as the conductor checks them, an external account one the
/// plugin reported and an instrument one the deployment holds; and a
/// setting serving several roles is set by an admin of every one (W6.11,
/// decisions/033 point 2). None when the fields ask for no change.
pub(crate) async fn settings_change(
    app: &App,
    record: &PluginSettingsRecord,
    records: &AccessRecords,
    access: &Access,
    fields: &Fields,
) -> Result<Option<meridian_domain::v1::SetPluginSettingsRequest>, SettingsRefused> {
    let instance = record.plugin_instance_id.as_str();
    let mut offered = choices(app, instance, record, records).await;
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
        let said = settings::said(record, &problems);
        return Err(SettingsRefused::Cells(problems, said));
    }
    let Some(request) = settings::request(record, fields, crate::html::is_development()) else {
        return Ok(None);
    };
    let plugin_roles = access
        .known_roles
        .get(instance)
        .cloned()
        .unwrap_or_default();
    let held = access.plugin(instance);
    if let Some((name, said)) = settings::not_administered(record, &request, &held, &plugin_roles) {
        return Err(SettingsRefused::NotAdministered(name, said));
    }
    Ok(Some(request))
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
    /// Its launch, where the instance is stopped (contract v25): the
    /// Summary says so, and when, and draws nothing a running plugin
    /// reports.
    pub stopped: Option<&'a meridian_domain::v1::PluginLaunch>,
    /// Its version's pull from the catalogue, where no deployment admin has
    /// kept it since (W8.12, contract v25): one line heading the Summary.
    pub pulled: Option<meridian_domain::v1::PluginPull>,
    pub session: &'a Session,
    pub notice: &'a str,
    pub now: i64,
    /// Its moves and archive as the conductor answers them, for a plugin
    /// keeping raw records (W6.9, contract v16); and the page of moves asked.
    pub moves: Option<&'a Result<meridian_domain::v1::ReadMovesReply, String>>,
    pub moves_cursor: &'a str,
    /// Its role's first-use steps (contract v20), judged from core's records.
    pub setting_up: Vec<setting_up::Step>,
    /// Whether the settings act's second pair of eyes is on for the person:
    /// the form then offers only Prepare (contract v20, Q21).
    pub eyes: bool,
}

pub mod setting_up;

/// The first-use steps of an instance's role, judged from core's own
/// records now (W6.9, contract v20): its settings, its health, the custody
/// connector's accounts and links, the account grants, and the lake's
/// licences and entitlements where its role needs them.
pub(crate) async fn setting_up_steps(
    app: &App,
    instance: &str,
    records: &AccessRecords,
    subject: &str,
    deployment_admin: bool,
) -> Vec<setting_up::Step> {
    let reports = app.health.view();
    let report = reports.get(instance);
    let roles: Vec<String> = report
        .map(|r| r.roles.clone())
        .filter(|roles| !roles.is_empty())
        .or_else(|| meridian_access::known_roles(records, instance).map(<[String]>::to_vec))
        .unwrap_or_default();
    let roles = meridian_domain::roles_now(&roles);
    let custody = app.custody.view();
    let connected = custody.sync.keys().any(|(held_by, _)| held_by == instance);
    let unlinked = custody
        .reported
        .get(instance)
        .map(|accounts| {
            accounts
                .iter()
                .filter(|account| {
                    !records.links.iter().any(|link| {
                        link.plugin_instance_id == instance
                            && link.external_account_id == account.external_account_id
                    })
                })
                .count()
        })
        .unwrap_or_default();
    let reads_data = roles
        .iter()
        .any(|role| role == "data" || role == "reporting");
    let (datasets_ready, entitled) = if reads_data {
        match data_sources::datasets(&app.bus, subject).await {
            Ok(listed) => {
                let mine: Vec<&String> = listed
                    .datasets
                    .iter()
                    .filter(|d| d.instance == instance)
                    .map(|d| &d.dataset)
                    .collect();
                let ready = !mine.is_empty()
                    && mine.iter().all(|dataset| {
                        listed
                            .licences
                            .iter()
                            .any(|l| &&l.dataset == dataset && l.updated_at_ns > 0)
                            && listed
                                .entitlements
                                .iter()
                                .any(|e| &&e.dataset == dataset && e.allowed)
                    });
                let entitled = listed
                    .entitlements
                    .iter()
                    .any(|e| e.instance == instance && e.allowed);
                (Some(ready), Some(entitled))
            }
            Err(_) => (None, None),
        }
    } else {
        (None, None)
    };
    setting_up::steps(&setting_up::Facts {
        instance,
        roles,
        record: settings_of(records, instance),
        records: Some(records),
        healthy: report.is_some_and(|r| r.healthy),
        deployment_admin,
        connected,
        unlinked,
        datasets_ready,
        entitled,
    })
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
    // A stopped instance (contract v25): stopped, by whom and when.
    let stopped = manage.stopped.map_or(String::new(), |launch| {
        format!(
            "<dt>Stopped</dt><dd data-stopped>{} by {}</dd>",
            &crate::terminal::rfc3339(launch.stopped_at_ns)[..16].replace('T', " "),
            escape(&launch.stopped_by)
        )
    });
    // The sidecar's refusals at registration -- a tool, a page, and on a
    // live instance a difference from its version's declaration -- as
    // warnings, one line each (W4.8, contract v25).
    let warnings: String = manage
        .report
        .map(|report| report.tool_refusals.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|why| format!("<li class=\"refused\" data-warning>{}</li>", escape(why)))
        .collect();
    let warnings = if warnings.is_empty() {
        String::new()
    } else {
        format!("<ul class=\"warnings one-line\">{warnings}</ul>")
    };
    // What it received and did not carry since it started, as counts.
    let seen: Vec<String> = manage
        .report
        .map(|report| report.not_carried_seen.as_slice())
        .unwrap_or_default()
        .iter()
        .filter(|seen| seen.count > 0)
        .map(|seen| {
            format!(
                "{} {} ({})",
                escape(&seen.scheme),
                escape(&seen.name),
                seen.count
            )
        })
        .collect();
    let seen = if seen.is_empty() {
        String::new()
    } else {
        format!(
            "<dt>Not carried</dt><dd data-not-carried-seen>{}</dd>",
            seen.join(", ")
        )
    };
    let contract = said(
        manage.report.map(|report| report.contract_version.as_str()),
        "not said",
    );
    let figures = crate::figures::section(manage.report, manage.now);
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
         <dt>Contract</dt><dd data-contract>{contract}</dd>{stopped}{seen}</dl>{warnings}\
         <p class=\"reserved\" data-reserved=\"lifecycle\">Restarting it, moving it to another version and \
         holding it at one will be here. They are not built yet.</p></section>{figures}"
    );
    let deployment_admin = person_access(
        manage.records,
        &manage.session.subject,
        &manage.session.directory_groups,
    )
    .deployment_admin;
    let pulled = manage.pulled.as_ref().map_or(String::new(), |pull| {
        format!(
            "<p class=\"error\" role=\"alert\" data-pulled>Its version was pulled from the catalogue: {}. \
             <a href=\"/admin/plugins/registry/acknowledge?instance={}\">Keep it</a> or stop it.</p>",
            crate::registry::pull_kind(pull),
            escape(manage.instance)
        )
    });
    format!(
        "{pulled}{notice}{parts}",
        notice = notice_line(manage.notice),
        parts = summary_parts(
            &SUMMARY_PART_KEYS
                .iter()
                .zip([
                    status,
                    if manage.setting_up.is_empty() {
                        String::new()
                    } else {
                        setting_up::section(&manage.setting_up, deployment_admin)
                    },
                    records,
                    moves,
                ])
                .map(|((key, title), drawn)| (*key, *title, drawn))
                .collect::<Vec<_>>()
        ),
    )
}

/// The Summary's parts, by key and title, in the order drawn: each has a
/// plugin-area tool reaching it (W6.20, contract v17; the parity check in
/// [`crate::mcp::plugin_area`]).
pub(crate) const SUMMARY_PART_KEYS: [(&str, &str); 4] = [
    ("status", "Status"),
    ("setting-up", "Setting up"),
    ("records", "Raw records"),
    ("moves", "Moves"),
];

/// The Summary's parts -- its status and figures, an edge plugin's raw
/// records and their moves (contract v16), what it declares, its tools -- as
/// tabs of their own where there are two or more, so the Summary fits one
/// screen (the product owner, 2026-10-04): one shown at a time, the one the
/// address names or else the first, its status. Without script every part
/// shows, under its own heading.
fn summary_parts(parts: &[(&str, &str, String)]) -> String {
    parts_shown("The Summary's parts", parts)
}

/// Parts of a page as tabs of their own, one shown at a time, the one the
/// address names or else the first ([`summary_parts`]; a plugin's Access tab,
/// who holds it and what agents may do, contract v20). Without script every
/// part shows.
pub(crate) fn parts_shown(label: &str, parts: &[(&str, &str, String)]) -> String {
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
        "<nav class=\"tabs summary-parts\" data-summary-parts aria-label=\"{label}\">{links}</nav>\
         {sections}<script>{SUMMARY_PARTS}</script>",
        label = escape(label)
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
    let section = settings_section(
        manage.records,
        settings_of(manage.records, manage.instance),
        &token_input(manage.session),
        crate::html::is_development(),
        &crate::area::settings_path(manage.instance),
        manage.notice,
        Some(&held),
    );
    if manage.eyes {
        section.replace(SAVE_SETTINGS, "")
    } else {
        section
    }
}

/// The settings form's Save, left out where a second pair of eyes is on.
const SAVE_TABLE: &str = "<button type=\"submit\" class=\"primary\" data-save>Save</button>";
const SAVE_SETTINGS: &str =
    "<button type=\"submit\" class=\"primary\" data-save>Save settings</button>";

/// The dashboard's tab for one of the plugin's table settings, in its area
/// under Manage (the product owner, 2026-10-05: each table setting a tab of
/// its own beside Settings, titled with its label): the table's page alone,
/// posted to the area's own address and back to this tab.
pub(crate) fn table_tab(manage: &Manage, key: &str) -> String {
    let development = crate::html::is_development();
    match settings_of(manage.records, manage.instance)
        .and_then(|record| Some((record, settings::table_named(record, key, development)?)))
    {
        Some((record, table)) => {
            let section = table_section(
                manage.records,
                record,
                table,
                &token_input(manage.session),
                &with_tab(&crate::area::settings_path(manage.instance), key),
                manage.choices,
                manage.notice,
            );
            if manage.eyes {
                section.replace(SAVE_TABLE, "")
            } else {
                section
            }
        }
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
            &|subject| crate::tickets::display_name(records, subject),
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
/// one that changed its own act with the reason given; from contract v22
/// whether its releases wait for compliance among them, and one change
/// prepared for someone to make (dashboard__set_account_attribute, W6.20).
async fn set_book(
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
    let account_id = field(&fields, "account_id");
    let asked = match books::read(&app.bus).await {
        books::Books::NotAnswering(why) => {
            return after(Err(format!("the book of record is not answering: {why}")))
        }
        books::Books::Read(held) => {
            let held = held.iter().find(|a| a.account_id == account_id);
            books::requests_marked(
                account_id,
                field(&fields, "base_currency_code"),
                field(&fields, "lot_relief_default"),
                field(&fields, "waits_for_compliance"),
                field(&fields, "account_mark"),
                field(&fields, "note"),
                held,
            )
        }
    };
    let asked = match asked {
        Ok(asked) => asked,
        Err(sentence) => return after(Err(sentence)),
    };
    let preparing = !field(&fields, crate::prepared::handoff::PREPARE_FIELD).is_empty();
    if preparing && asked.len() > 1 {
        return after(Err(
            "a prepared change sets one attribute: change one at a time to prepare it".into(),
        ));
    }
    let first = asked[0].clone();
    let posted = prepare_or_gate(&app, &session, &fields, SET_ACCOUNT_ATTRIBUTE_TOOL, || {
        Ok(books::arguments(&first))
    })
    .await;
    if let Some(response) = posted_to(posted, "/admin#accounts") {
        return response;
    }
    let mut outcome = Ok(());
    for request in asked {
        if let Err(refused) = books::set(&app.bus, &session.subject, request).await {
            outcome = Err(refused);
            break;
        }
    }
    after(outcome)
}

/// The account attribute's act on the surface (W6.20, contract v22).
const SET_ACCOUNT_ATTRIBUTE_TOOL: &str = "dashboard__set_account_attribute";

async fn close_account(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    admin_form!(app, headers, fields, session, {
        let account_id = field(&fields, "account_id").to_string();
        // "Unlink all and close" (approved 2026-10-14; contract v24's
        // CloseAccountRequest.unlink_attached): the conductor withdraws each
        // grant as its own record and closes, in one step. A link or an
        // ems's mapping is undone at its plugin's page; the conductor
        // refuses the closure, naming each, while one stands.
        command::<meridian_domain::v1::AccountRecord>(
            &app,
            &session,
            "platform.config.command.close-account",
            "meridian.v1.CloseAccountRequest",
            CloseAccountRequest {
                account_id,
                unlink_attached: field(&fields, "unlink_all") == "1",
                note: field(&fields, "note").trim().to_string(),
            },
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
pub(crate) fn once(pairs: &[(String, String)]) -> Fields {
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
    // The Calendars tab (W3.21, contract v22): what is held, and the year of
    // the calendar chosen, this year by default.
    let filter = calendars::Filter::from_page(
        field(&query, "calendar_kind"),
        field(&query, "calendar_query"),
    );
    let held = calendars::held(&app.bus, &filter).await;
    let codes = match &held {
        Ok(listed) => calendars::subject_codes(&app.bus, listed).await,
        Err(_) => Default::default(),
    };
    let chosen = field(&query, "calendar");
    let year = field(&query, "year")
        .parse::<u16>()
        .unwrap_or_else(|_| meridian_domain::date::Date::of_utc_instant(app.clock.now_ns()).year());
    let read = if chosen.is_empty() {
        None
    } else {
        Some(calendars::year(&app.bus, chosen, year).await)
    };
    let tab = calendars::tab(
        &held,
        &filter,
        &codes,
        read.as_ref().map(|read| (chosen, year, read)),
    );
    // Contract v24 (W3.11, W3.28): the provisional records, and the usage
    // report's setting with the reports sent.
    let provisional = instruments::provisional(&app.bus).await;
    let usage = instruments::usage_reports(&app.bus).await;
    let body = instruments::list_page_full(
        &listed,
        Some(&provisional),
        Some(&usage),
        &token_input(&session),
        field(&query, "done"),
        &tab,
        app.clock.now_ns(),
    );
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
    // Contract v26 (W6.9, W3.30): the groups it is a member of, as they
    // stood at the date asked, and their members' records for the bounds.
    let as_of = field(&query, "as_of").to_string();
    let groups = event_groups::list(
        &app.bus,
        &session.subject,
        meridian_domain::v1::ListEventGroupsRequest {
            instrument_ids: vec![instrument.clone()],
            as_of_ns: event_groups::as_of_ns(&as_of),
            page_size: 20,
            ..Default::default()
        },
    )
    .await
    .map(|reply| reply.event_groups)
    .unwrap_or_default();
    let mut members = HashMap::new();
    for member in groups
        .iter()
        .flat_map(|group| group.member_instrument_ids.iter())
        .take(event_groups::MOST_MEMBERS)
    {
        if members.contains_key(member) {
            continue;
        }
        if let Ok(listed) = instruments::list(&app.bus, member).await {
            if let Some(record) = listed
                .instruments
                .into_iter()
                .next()
                .and_then(|i| i.instrument)
            {
                members.insert(member.clone(), record);
            }
        }
    }
    let groups_section = event_groups::section(&instrument, &groups, &members, &as_of);
    let mut body = instruments::record_page(
        &item,
        &versions,
        None,
        &token_input(&session),
        &groups_section,
    );
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
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let fields = once(&pairs);
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
        contract_amount: field(&fields, "contract_amount").into(),
        contract_asset: field(&fields, "contract_asset").into(),
        contract_closes: field(&fields, "contract_closes").into(),
        contract_source: field(&fields, "contract_source").into(),
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
        contract_held: field(&fields, "contract_held").into(),
        ends: every(&pairs, "end"),
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
    let body = instruments::record_page(&item, &versions, Some(&asked), &token_input(&session), "");
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
    let (session, _) = match gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if let Err(response) = form_token_matches(&session, &fields) {
        return *response;
    }
    let posted = prepare_or_gate(&app, &session, &fields, "dashboard__set_hold", || {
        let days: u32 = field(&fields, "days")
            .parse()
            .map_err(|_| format!("{:?} is not a number of days", field(&fields, "days")))?;
        Ok(serde_json::json!({
            "role": field(&fields, "role"),
            "days": days,
            "write_once": !field(&fields, "write_once").is_empty(),
            "note": field(&fields, "note").trim(),
        }))
    })
    .await;
    if let Some(response) = posted_to(posted, "/admin#holds") {
        return response;
    }
    after({
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
                    // Optional at the page (contract v17, the plan's Q8).
                    note: field(&fields, "note").trim().to_string(),
                },
            )
            .await
            .map(|_| ()),
        }
    })
}

// ── The instrument picker's search (contract v20) ──────────────────────────

/// `GET /admin/instruments/search?q=&limit=`: the records the words find,
/// for the kit's instrument column on core's own pages (meridian-ui 0.12.0),
/// as its picker reads them; an exact ID answers too, so a table's IDs show
/// by name. For whoever reaches core's settings: a deployment admin or a
/// plugin's admin. It never lists every record.
async fn search_instruments(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(query): Query<Fields>,
) -> Response {
    let (session, records) = match gate(&app, &headers, false) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let access = person_access(&records, &session.subject, &session.directory_groups);
    if !(access.deployment_admin || access.administers_any()) {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({"error": "a deployment admin's or a plugin admin's"})),
        )
            .into_response();
    }
    let words = field(&query, "q");
    let limit: usize = field(&query, "limit").parse().unwrap_or(8).clamp(1, 50);
    if words.is_empty() {
        return axum::Json(serde_json::json!({"results": []})).into_response();
    }
    match instruments::search(&app.bus, words, limit).await {
        Ok(found) => axum::Json(serde_json::json!({
            "results": found
                .instruments
                .iter()
                .filter_map(|each| each.instrument.as_ref())
                .take(limit)
                .map(instruments::found_json)
                .collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(why) => (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"error": why})),
        )
            .into_response(),
    }
}

// ── What agents may do (W6.29) and the accounts a plugin may link (W6.30) ──

pub mod agents;

/// The level, the gate and the note a level's form posts, held to the act's
/// rules; or why not.
fn level_posted(
    fields: &Fields,
    line: &agents::ActLine,
) -> Result<(crate::prepared::levels::Level, bool, String), String> {
    let level = crate::prepared::levels::Level::parse(field(fields, "level")).ok_or_else(|| {
        format!(
            "{:?} is not may_make, prepare_only or may_not",
            field(fields, "level")
        )
    })?;
    let eyes = !field(fields, "second_pair_of_eyes").is_empty();
    if let Some(why) = crate::prepared::levels::refused_for(&line.act, level, eyes, line.preparable)
    {
        return Err(why);
    }
    let note = field(fields, "note").to_string();
    if note.chars().count() > 2_000 {
        return Err("note: at most 2,000 characters".into());
    }
    Ok((level, eyes, note))
}

/// A scope a form names: `act`, `ceiling` or an access group's.
fn scope_posted(
    fields: &Fields,
    records: &AccessRecords,
    allowed: &[&str],
) -> Result<String, String> {
    let scope = field(fields, "scope");
    if allowed.contains(&scope) {
        return Ok(scope.to_string());
    }
    if let Some(group) = scope.strip_prefix(crate::prepared::levels::GROUP) {
        if records
            .access_groups
            .iter()
            .any(|g| g.access_group_id == group && !g.built_in)
        {
            return Ok(scope.to_string());
        }
    }
    Err(format!("{scope:?} is not a place this page sets a level"))
}

/// The Agents tab's change (W6.29): a deployment admin's, for core's acts
/// and the ceiling on a plugin's, each also per access group.
async fn set_agents_may(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, records) = match gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if let Err(response) = form_token_matches(&session, &fields) {
        return *response;
    }
    let outcome = async {
        let acts = agents::acts(&app);
        let line = acts
            .iter()
            .find(|line| line.act == field(&fields, "act"))
            .ok_or_else(|| {
                format!(
                    "{:?} is no act this deployment offers",
                    field(&fields, "act")
                )
            })?;
        let allowed: &[&str] = if line.instance.is_empty() {
            &["act"]
        } else {
            &["ceiling"]
        };
        let scope = scope_posted(&fields, &records, allowed)?;
        let (level, eyes, note) = level_posted(&fields, line)?;
        let by = crate::prepared::person_caller(
            &session.subject,
            &session.display_name,
            &session.directory_groups,
        );
        app.prepared
            .set_level(
                &line.act,
                &scope,
                Some((level, eyes)),
                &by,
                &note,
                app.clock.now_ns(),
            )
            .await?;
        Ok(())
    }
    .await;
    after_to(outcome, "/admin#agents", "/admin#agents")
}

/// The lapse period (W6.28), a deployment admin's, its own record.
async fn set_lapse(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    admin_form!(app, headers, fields, session, {
        match field(&fields, "days").parse::<u32>() {
            Ok(days) if (1..=crate::prepared::levels::LAPSE_MOST).contains(&days) => {
                let by = crate::prepared::person_caller(
                    &session.subject,
                    &session.display_name,
                    &session.directory_groups,
                );
                app.prepared
                    .set_lapse_days(days, &by, field(&fields, "note"), app.clock.now_ns())
                    .await
                    .map(|_| ())
            }
            _ => Err(format!(
                "days: a whole number of days, 1 to {}",
                crate::prepared::levels::LAPSE_MOST
            )),
        }
    })
}

/// A plugin's act's level and gate, set by an admin of every role it
/// serves, within the ceiling (W6.29).
async fn set_plugin_agents_may(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(instance): Path<String>,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, records, access) = match gate_plugin(&app, &headers, &instance, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if let Err(response) = form_token_matches(&session, &fields) {
        return *response;
    }
    let back = view::tab_href(&instance, view::ACCESS);
    let outcome = async {
        let acts = agents::acts(&app);
        let line = acts
            .iter()
            .find(|line| line.act == field(&fields, "act") && line.instance == instance)
            .ok_or_else(|| format!("{:?} is no act of {instance}'s", field(&fields, "act")))?;
        if !access.plugin(&instance).administers_every(&line.roles) {
            return Err(format!(
                "{} serves {}, and its level is set by an admin of every one",
                line.act,
                line.roles.join(" and ")
            ));
        }
        let scope = scope_posted(&fields, &records, &["act"])?;
        let (level, eyes, note) = level_posted(&fields, line)?;
        let by = crate::prepared::person_caller(
            &session.subject,
            &session.display_name,
            &session.directory_groups,
        );
        app.prepared
            .set_level(
                &line.act,
                &scope,
                Some((level, eyes)),
                &by,
                &note,
                app.clock.now_ns(),
            )
            .await?;
        Ok(())
    }
    .await;
    after_to(outcome, &back, &back)
}

/// The plugin's Accounts tab (W6.30): a deployment admin grants or
/// withdraws an account, each its own record in the conductor.
async fn set_linkable(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(instance): Path<String>,
    Form(fields): Form<Fields>,
) -> Response {
    let back = view::tab_href(&instance, view::ACCOUNTS);
    let (session, _) = match gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if let Err(response) = form_token_matches(&session, &fields) {
        return *response;
    }
    let grant: Vec<String> = Some(field(&fields, "grant"))
        .filter(|id| !id.is_empty())
        .map(String::from)
        .into_iter()
        .collect();
    let withdraw: Vec<String> = Some(field(&fields, "withdraw"))
        .filter(|id| !id.is_empty())
        .map(String::from)
        .into_iter()
        .collect();
    let outcome = command::<meridian_domain::v1::LinkableAccounts>(
        &app,
        &session,
        "platform.config.command.set-linkable-accounts",
        "meridian.v1.SetLinkableAccountsRequest",
        meridian_domain::v1::SetLinkableAccountsRequest {
            plugin_instance_id: instance.clone(),
            grant_account_ids: grant,
            withdraw_account_ids: withdraw,
            note: field(&fields, "note").to_string(),
        },
    )
    .await
    .map(|_| ());
    after_to(outcome, &back, &back)
}

// ── The trading instructions (W11.15, contract v22) ─────────────────────────

/// Who is asking about the trading instructions: a deployment admin, or a
/// plugin's admin for what names their plugins; or the refusal.
fn gate_trading(
    app: &App,
    headers: &HeaderMap,
) -> Result<(Session, AccessRecords, Access), Box<Response>> {
    let (session, records) = gate(app, headers, false)?;
    let access = person_access(&records, &session.subject, &session.directory_groups);
    if !(access.deployment_admin || access.administers_any()) {
        return Err(Box::new(status_page(
            StatusCode::FORBIDDEN,
            "Not permitted",
            "the trading instructions are for deployment admins and plugins' admins",
        )));
    }
    Ok((session, records, access))
}

fn trading_chrome(session: &Session) -> Chrome<'_> {
    Chrome {
        crumbs: format!(
            "{}{}",
            crate::html::crumb_link("/admin", "Settings"),
            crate::html::crumb_here("Trading instructions", None)
        ),
        ..admin_chrome(session)
    }
}

/// W11.15, W11.16: the trading instructions in force, and the dialog that
/// sets one.
async fn trading_page(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(query): Query<Fields>,
) -> Response {
    let (session, records, access) = match gate_trading(&app, &headers) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let held = trading::read(&app.bus).await;
    // Contract v24 (W11.36, W11.37): the escape hatch's setting, a
    // deployment admin's, and the live orders on local records.
    let local = if access.deployment_admin {
        Some(trading::read_local_record_orders(&app.bus).await)
    } else {
        None
    };
    let body = trading::page_with(
        &held,
        &records,
        &access,
        &token_input(&session),
        field(&query, "done"),
        local.as_ref(),
    );
    Html(page_with(
        "Trading instructions",
        &body,
        &trading_chrome(&session),
    ))
    .into_response()
}

/// W11.15: an instruction set, at the setter's level the dashboard stamps
/// from their access; or prepared for someone to make. Nothing stands
/// between a person and a stop but the act's own second pair of eyes, where
/// an admin turned it on.
async fn set_trading_instruction(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, _, access) = match gate_trading(&app, &headers) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if let Err(response) = form_token_matches(&session, &fields) {
        return *response;
    }
    let asked = trading::target_from(
        field(&fields, "kind"),
        field(&fields, "id"),
        field(&fields, "instance"),
    )
    .and_then(|target| {
        let value = trading::value_of(field(&fields, "value"))
            .ok_or_else(|| format!("{:?} is not active, pause or stop", field(&fields, "value")))?;
        trading::request(&access, target, value, field(&fields, "note"))
    });
    let request = match asked {
        Ok(request) => request,
        Err(sentence) => return not_done(&sentence, trading::PATH),
    };
    let arguments = trading::arguments(&request);
    let posted = prepare_or_gate(
        &app,
        &session,
        &fields,
        SET_TRADING_INSTRUCTION_TOOL,
        || Ok(arguments),
    )
    .await;
    if let Some(response) = posted_to(posted, trading::PATH) {
        return response;
    }
    let outcome = app
        .bus
        .call_for(
            trading::SET_TRADING_INSTRUCTION,
            "meridian.v1.SetTradingInstructionRequest",
            request.encode_to_vec(),
            None,
            Some(Duration::from_secs(10)),
            &session.subject,
        )
        .await
        .map(|_| ())
        .map_err(|failed| match failed {
            BusError::HandlerFailed { detail, .. } => {
                match meridian_bus::read_refusal_status(&detail) {
                    Some((_, _, words)) => words.to_string(),
                    None => refusal_words(detail),
                }
            }
            other => other.to_string(),
        });
    after_to(
        outcome,
        &format!("{}?done=Set", trading::PATH),
        trading::PATH,
    )
}

/// The trading instruction's act on the surface (W6.20, contract v22).
const SET_TRADING_INSTRUCTION_TOOL: &str = "dashboard__set_trading_instruction";

/// The words a refusal from the bus is shown in.
fn said_of_refusal(failed: BusError) -> String {
    match failed {
        BusError::HandlerFailed { detail, .. } => {
            match meridian_bus::read_refusal_status(&detail) {
                Some((_, _, words)) => words.to_string(),
                None => refusal_words(detail),
            }
        }
        other => other.to_string(),
    }
}

/// W11.36 (contract v24): "Allow orders on local records", a deployment
/// admin's, with a reason; or prepared for someone to make.
async fn set_local_record_orders(
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
    let allows = match field(&fields, "allows") {
        "on" => true,
        "off" => false,
        other => return not_done(&format!("{other:?} is neither on nor off"), trading::PATH),
    };
    let note = field(&fields, "note").trim().to_string();
    if note.is_empty() {
        return not_done("say why the setting changes", trading::PATH);
    }
    let posted = prepare_or_gate(
        &app,
        &session,
        &fields,
        SET_LOCAL_RECORD_ORDERS_TOOL,
        || Ok(serde_json::json!({"allows_local_record_orders": allows, "note": note})),
    )
    .await;
    if let Some(response) = posted_to(posted, trading::PATH) {
        return response;
    }
    let outcome = app
        .bus
        .call_for(
            trading::SET_LOCAL_RECORD_ORDERS,
            "meridian.v1.SetLocalRecordOrdersRequest",
            meridian_domain::v1::SetLocalRecordOrdersRequest {
                allows_local_record_orders: allows,
                reason: note.clone(),
            }
            .encode_to_vec(),
            None,
            Some(Duration::from_secs(10)),
            &session.subject,
        )
        .await
        .map(|_| ())
        .map_err(said_of_refusal);
    after_to(
        outcome,
        &format!(
            "{}?done={}",
            trading::PATH,
            if allows {
                "Orders+on+local+records+allowed"
            } else {
                "Orders+on+local+records+refused"
            }
        ),
        trading::PATH,
    )
}

/// The escape hatch's act on the surface (W6.20, contract v24).
const SET_LOCAL_RECORD_ORDERS_TOOL: &str = "dashboard__set_local_record_orders";

/// W6.35 (contract v26): the deployment's time zone set, a deployment
/// admin's, with a reason; or prepared for someone to make. The conductor
/// checks the ID and keeps each change its own record.
async fn set_deployment_time_zone(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    let back = "/admin#time-zone";
    let (session, _) = match gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if let Err(response) = form_token_matches(&session, &fields) {
        return *response;
    }
    let zone = field(&fields, "deployment_time_zone").trim().to_string();
    let note = field(&fields, "note").trim().to_string();
    if note.is_empty() {
        return not_done("say why the time zone changes", back);
    }
    let posted = prepare_or_gate(
        &app,
        &session,
        &fields,
        SET_DEPLOYMENT_TIME_ZONE_TOOL,
        || Ok(serde_json::json!({"deployment_time_zone": zone, "note": note})),
    )
    .await;
    if let Some(response) = posted_to(posted, back) {
        return response;
    }
    let outcome = app
        .bus
        .call_for(
            time_zone::SET_DEPLOYMENT_TIME_ZONE,
            "meridian.v1.SetDeploymentTimeZoneRequest",
            meridian_domain::v1::SetDeploymentTimeZoneRequest {
                deployment_time_zone: zone.clone(),
                reason: note.clone(),
            }
            .encode_to_vec(),
            None,
            Some(Duration::from_secs(10)),
            &session.subject,
        )
        .await
        .map(|_| ())
        .map_err(said_of_refusal);
    after_to(outcome, "/admin?done=Time+zone+set#time-zone", back)
}

/// The time zone's act on the surface (W6.35, contract v26).
const SET_DEPLOYMENT_TIME_ZONE_TOOL: &str = "dashboard__set_deployment_time_zone";

/// W3.28 (contract v24): "Report identifier usage", a deployment admin's,
/// with a reason; or prepared for someone to make.
async fn set_reports_usage(
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
    let back = "/admin/instruments#licensed";
    let on = match field(&fields, "reports_usage") {
        "on" => true,
        "off" => false,
        other => return not_done(&format!("{other:?} is neither on nor off"), back),
    };
    let note = field(&fields, "note").trim().to_string();
    if note.is_empty() {
        return not_done("say why the setting changes", back);
    }
    let posted = prepare_or_gate(&app, &session, &fields, SET_REPORTS_USAGE_TOOL, || {
        Ok(serde_json::json!({"reports_usage": on, "note": note}))
    })
    .await;
    if let Some(response) = posted_to(posted, back) {
        return response;
    }
    let outcome = app
        .bus
        .call_for(
            instruments::SET_REPORTS_USAGE,
            "meridian.v1.SetReportsUsageRequest",
            meridian_domain::v1::SetReportsUsageRequest {
                reports_usage: on,
                reason: note.clone(),
            }
            .encode_to_vec(),
            None,
            Some(Duration::from_secs(10)),
            &session.subject,
        )
        .await
        .map(|_| ())
        .map_err(said_of_refusal);
    after_to(
        outcome,
        &format!(
            "/admin/instruments?done={}#licensed",
            if on {
                "Identifier+usage+is+reported"
            } else {
                "Identifier+usage+is+not+reported"
            }
        ),
        back,
    )
}

/// The usage report's act on the surface (W6.20, contract v24).
const SET_REPORTS_USAGE_TOOL: &str = "dashboard__set_reports_usage";

/// W3.28 (contract v25): the licensed schemes declared, a deployment
/// admin's, the whole list with a note; or prepared for someone to make.
async fn set_licensed_schemes(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let fields = once(&pairs);
    let (session, _) = match gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if let Err(response) = form_token_matches(&session, &fields) {
        return *response;
    }
    let back = "/admin/instruments#licensed";
    let note = field(&fields, "note").trim().to_string();
    if note.is_empty() {
        return not_done("say why the licensed schemes change", back);
    }
    let schemes: Vec<String> = every(&pairs, "licensed_scheme");
    let posted = prepare_or_gate(&app, &session, &fields, SET_LICENSED_SCHEMES_TOOL, || {
        Ok(serde_json::json!({"licensed_schemes": schemes, "note": note}))
    })
    .await;
    if let Some(response) = posted_to(posted, back) {
        return response;
    }
    let outcome = app
        .bus
        .call_for(
            instruments::SET_LICENSED_SCHEMES,
            "meridian.v1.SetLicensedSchemesRequest",
            meridian_domain::v1::SetLicensedSchemesRequest {
                licensed_schemes: schemes.clone(),
                reason: note.clone(),
            }
            .encode_to_vec(),
            None,
            Some(Duration::from_secs(10)),
            &session.subject,
        )
        .await
        .map(|_| ())
        .map_err(said_of_refusal);
    after_to(
        outcome,
        "/admin/instruments?done=The+licensed+schemes+are+set#licensed",
        back,
    )
}

/// The licensed schemes' act on the surface (W6.20, contract v25).
const SET_LICENSED_SCHEMES_TOOL: &str = "dashboard__set_licensed_schemes";

/// W3.26 (contract v24): the migration's lines made from the Provisional
/// tab -- the exact lines as one change, a line held back by its box, or one
/// uncertain line made or held back -- with a note saying why; or prepared
/// for someone to make (W6.26, W6.27).
async fn apply_migration_lines(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let fields = once(&pairs);
    let (session, _) = match gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if let Err(response) = form_token_matches(&session, &fields) {
        return *response;
    }
    let back = "/admin/instruments#provisional";
    let note = field(&fields, "note").trim().to_string();
    if note.is_empty() {
        return not_done("say why the lines are made", back);
    }
    // The exact lines' dialog names each line and the boxes ticked; the
    // uncertain lines' names one line, its records together.
    let held: Vec<String> = every(&pairs, "hold");
    let (instrument_ids, held_back_ids): (Vec<String>, Vec<String>) =
        match field(&fields, "group").trim() {
            "" => {
                let lines = every(&pairs, "line");
                let made = lines
                    .iter()
                    .filter(|id| !held.contains(id))
                    .cloned()
                    .collect();
                let kept = lines.into_iter().filter(|id| held.contains(id)).collect();
                (made, kept)
            }
            group => {
                let ids: Vec<String> = group
                    .split(',')
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string)
                    .collect();
                if field(&fields, "hold_all") == "1" {
                    (Vec::new(), ids)
                } else {
                    (ids, Vec::new())
                }
            }
        };
    if instrument_ids.is_empty() && held_back_ids.is_empty() {
        return not_done("choose a line to make or hold back", back);
    }
    let arguments = serde_json::json!({
        "instrument_ids": instrument_ids,
        "held_back_ids": held_back_ids,
        "note": note,
    });
    let posted = prepare_or_gate(&app, &session, &fields, APPLY_MIGRATION_TOOL, || {
        Ok(arguments.clone())
    })
    .await;
    if let Some(response) = posted_to(posted, back) {
        return response;
    }
    let made = instrument_ids.len();
    let kept = held_back_ids.len();
    let outcome = app
        .bus
        .call_for(
            instruments::APPLY_INSTRUMENT_MIGRATION,
            "meridian.v1.ApplyInstrumentMigrationRequest",
            meridian_domain::v1::ApplyInstrumentMigrationRequest {
                instrument_ids,
                held_back_ids,
                note,
            }
            .encode_to_vec(),
            None,
            Some(Duration::from_secs(30)),
            &session.subject,
        )
        .await
        .map(|_| ())
        .map_err(said_of_refusal);
    after_to(
        outcome,
        &format!("/admin/instruments?done={made}+made,+{kept}+held+back#provisional"),
        back,
    )
}

/// Making the migration's lines on the surface (W6.20, contract v24).
const APPLY_MIGRATION_TOOL: &str = "dashboard__apply_migration";

// ── The Data sources page ───────────────────────────────────────────────────

fn data_sources_chrome(session: &Session) -> Chrome<'_> {
    Chrome {
        crumbs: format!(
            "{}{}",
            crate::html::crumb_link("/admin", "Settings"),
            crate::html::crumb_here("Data sources", None)
        ),
        // Reported as the dashboard's page: the ticket subjects name no
        // lake (contract v13's SUBJECTS).
        ..admin_chrome(session)
    }
}

/// W10.1, W10.2: the Data sources page, for a deployment admin.
async fn data_sources_page(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(query): Query<Fields>,
) -> Response {
    let (session, records) = match gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let (listed, priorities) = tokio::join!(
        data_sources::datasets(&app.bus, &session.subject),
        data_sources::priorities(&app.bus, &session.subject),
    );
    let body = data_sources::page(
        &listed,
        &priorities,
        &records,
        &token_input(&session),
        field(&query, "done"),
    );
    Html(page_with(
        "Data sources",
        &body,
        &data_sources_chrome(&session),
    ))
    .into_response()
}

const DATA_SOURCES: &str = "/admin/data-sources";

/// A list typed as words between commas, blanks dropped.
fn words(fields: &Fields, name: &str) -> Vec<String> {
    list(fields, name)
}

/// A refusal's words, without the code it travels with: what the page says.
fn refusal_words(detail: String) -> String {
    meridian_bus::read_refusal(&detail).map_or(detail.clone(), |(_, words)| words.to_string())
}

/// W10.1: a deployment admin licenses a dataset, replacing its terms whole.
async fn set_dataset_licence(
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
    // "Kept for" is gated on "kept" (kit 0.11.0): read only with it, and a
    // licence serving rows unkept keeps nothing for any time.
    let kept = !field(&fields, "kept").is_empty();
    let posted = prepare_or_gate(
        &app,
        &session,
        &fields,
        "dashboard__set_dataset_licence",
        || {
            let retention_days: u32 = if kept {
                field(&fields, "retention_days").parse().map_err(|_| {
                    format!(
                        "{:?} is not a number of days",
                        field(&fields, "retention_days")
                    )
                })?
            } else {
                0
            };
            Ok(serde_json::json!({
                "dataset": field(&fields, "dataset"),
                "kept": kept,
                "retention_days": retention_days,
                "derived_use": !field(&fields, "derived_use").is_empty(),
                "display": !field(&fields, "display").is_empty(),
                "default_fields": words(&fields, "default_fields"),
                "personal_use": !field(&fields, "personal_use").is_empty(),
                "note": field(&fields, "note"),
            }))
        },
    )
    .await;
    if let Some(response) = posted_to(posted, DATA_SOURCES) {
        return response;
    }
    let retention = if kept {
        field(&fields, "retention_days")
    } else {
        "0"
    };
    let outcome = match retention.parse::<u32>() {
        Err(_) => Err(format!(
            "{:?} is not a number of days",
            field(&fields, "retention_days")
        )),
        Ok(retention_days) => command::<meridian_pb::v1::DatasetLicence>(
            &app,
            &session,
            data_sources::SET_DATASET_LICENCE,
            "meridian.v1.SetDatasetLicenceRequest",
            meridian_domain::v1::SetDatasetLicenceRequest {
                dataset: field(&fields, "dataset").to_string(),
                licence: Some(meridian_pb::v1::DatasetLicence {
                    kept,
                    retention_days,
                    derived_use: !field(&fields, "derived_use").is_empty(),
                    display: !field(&fields, "display").is_empty(),
                    default_fields: words(&fields, "default_fields"),
                    personal_use: !field(&fields, "personal_use").is_empty(),
                    ..Default::default()
                }),
                // Optional at the page, as a hold's is; through /mcp
                // every change carries one.
                note: field(&fields, "note").to_string(),
            },
        )
        .await
        .map(|_| ())
        .map_err(refusal_words),
    };
    after_to(outcome, DATA_SOURCES, DATA_SOURCES)
}

/// W10.1: a deployment admin entitles a plugin instance to a dataset, or
/// withdraws it (no `allowed`).
async fn set_dataset_entitlement(
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
    let posted = prepare_or_gate(
        &app,
        &session,
        &fields,
        "dashboard__set_dataset_entitlement",
        || {
            Ok(serde_json::json!({
                "dataset": field(&fields, "dataset"),
                "instance": field(&fields, "instance"),
                "allowed": !field(&fields, "allowed").is_empty(),
                "fields": words(&fields, "fields"),
                "note": field(&fields, "note"),
            }))
        },
    )
    .await;
    if let Some(response) = posted_to(posted, DATA_SOURCES) {
        return response;
    }
    let outcome = command::<meridian_domain::v1::DatasetEntitlement>(
        &app,
        &session,
        data_sources::SET_DATASET_ENTITLEMENT,
        "meridian.v1.SetDatasetEntitlementRequest",
        meridian_domain::v1::SetDatasetEntitlementRequest {
            dataset: field(&fields, "dataset").to_string(),
            instance: field(&fields, "instance").to_string(),
            allowed: !field(&fields, "allowed").is_empty(),
            fields: words(&fields, "fields"),
            note: field(&fields, "note").to_string(),
        },
    )
    .await
    .map(|_| ())
    .map_err(refusal_words);
    after_to(outcome, DATA_SOURCES, DATA_SOURCES)
}

/// W10.2: a deployment admin sets a priority, replaced whole, against the
/// priority as it was read.
async fn set_source_priority(
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
    // "Kind, for prices" is gated on the data type (kit 0.11.0): read only
    // for prices, which alone have one.
    let kind = if field(&fields, "data_type") == meridian_domain::lake::PRICE {
        field(&fields, "kind")
    } else {
        ""
    };
    let posted = prepare_or_gate(
        &app,
        &session,
        &fields,
        "dashboard__set_source_priority",
        || {
            let against: i64 = field(&fields, "against_updated_at_ns")
                .parse()
                .map_err(|_| {
                    "the priority as it was read is not said; read it again".to_string()
                })?;
            let mut args = serde_json::json!({
                "data_type": field(&fields, "data_type"),
                "datasets": lines(&fields, "datasets"),
                "against_updated_at_ns": against.to_string(),
                "note": field(&fields, "note"),
            });
            if !kind.is_empty() {
                args["kind"] = kind.into();
            }
            Ok(args)
        },
    )
    .await;
    if let Some(response) = posted_to(posted, DATA_SOURCES) {
        return response;
    }
    let outcome = match (
        field(&fields, "against_updated_at_ns").parse::<i64>(),
        kind.is_empty()
            .then_some(meridian_domain::v1::PriceKind::Unspecified)
            .or_else(|| data_sources::kind_of(kind)),
    ) {
        (Err(_), _) => Err("the priority as it was read is not said; read it again".to_string()),
        (_, None) => Err(format!("{kind:?} is not a kind of price")),
        (Ok(against_updated_at_ns), Some(kind)) => command::<meridian_domain::v1::SourcePriority>(
            &app,
            &session,
            data_sources::SET_SOURCE_PRIORITY,
            "meridian.v1.SetSourcePriorityRequest",
            meridian_domain::v1::SetSourcePriorityRequest {
                data_type: field(&fields, "data_type").to_string(),
                kind: kind as i32,
                datasets: lines(&fields, "datasets"),
                note: field(&fields, "note").to_string(),
                against_updated_at_ns,
            },
        )
        .await
        .map(|_| ())
        .map_err(refusal_words),
    };
    after_to(outcome, DATA_SOURCES, DATA_SOURCES)
}
