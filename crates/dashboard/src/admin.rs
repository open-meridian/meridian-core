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
//! Every page but the claim page is refused to anybody not holding deployment
//! admin, checked against the records on each request.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use meridian_access::{person_access, DEPLOYMENT_ADMIN};
use meridian_bus::BusError;
use meridian_domain::v1::{
    AccessEntry, AccessGroup, AccessLevel, AccessRecords, AccountGroup, ClaimCodePurpose,
    CloseAccountRequest, DefineAccessGroupRequest, DefineAccountGroupRequest, DefineAccountRequest,
    DefineUserGroupRequest, GrantPermissionRequest, LinkExternalAccountRequest,
    PluginSettingsRecord, RedeemClaimCodeReply, RedeemClaimCodeRequest, UserGroup,
    WithdrawPermissionReply, WithdrawPermissionRequest,
};
use prost::Message;

use crate::html::{escape, page, page_with, Chrome, Viewer};
use crate::records;
use crate::session::Session;
use crate::web::{refused, session_of, App};

type Fields = HashMap<String, String>;

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/claim", get(claim_page).post(claim))
        .route("/admin", get(admin_page))
        .route("/admin/accounts", post(define_account))
        .route("/admin/accounts/close", post(close_account))
        .route("/admin/links", post(link))
        .route("/admin/user-groups", post(define_user_group))
        .route("/admin/account-groups", post(define_account_group))
        .route("/admin/access-groups", post(define_access_group))
        .route("/admin/permissions", post(grant))
        .route("/admin/permissions/withdraw", post(withdraw))
        .route("/admin/end-terminal-sessions", post(end_terminal_sessions))
        .route("/admin/plugins/{instance}", get(plugin_view))
        .route(
            "/admin/plugins/{instance}/settings",
            get(settings_page).post(set_settings),
        )
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

fn status_page(status: StatusCode, title: &str, sentence: &str) -> Response {
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
fn gate(
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

fn form_token_matches(session: &Session, fields: &Fields) -> Result<(), Box<Response>> {
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

/// W6.14: every terminal session a person holds, ended. Nothing goes to the
/// conductor: the sessions are this dashboard's, and so is ending them.
async fn end_terminal_sessions(
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
    let login = field(&fields, "login");
    let ended = app.terminals.end_person(login);
    tracing::info!(login, ended, by = %session.subject, "terminal sessions ended");
    (
        StatusCode::SEE_OTHER,
        [(
            axum::http::header::LOCATION,
            format!("/admin?terminal_sessions_ended={ended}"),
        )],
    )
        .into_response()
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
fn admin_chrome(session: &Session) -> Chrome<'_> {
    Chrome {
        viewer: Some(Viewer {
            display_name: &session.display_name,
            form_token: &session.form_token,
            admin: true,
        }),
        crumbs: "<a href=\"/admin\">Admin portal</a>".into(),
        main: "page",
        in_admin: true,
    }
}

fn token_input(session: &Session) -> String {
    format!(
        "<input type=\"hidden\" name=\"form_token\" value=\"{}\">",
        escape(&session.form_token)
    )
}

// ── The overview ────────────────────────────────────────────────────────────

mod overview;

async fn admin_page(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(query): Query<Fields>,
) -> Response {
    let (session, records) = match gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    // What the last form did, where it has something to say.
    let notice = match field(&query, "terminal_sessions_ended").parse::<usize>() {
        Ok(ended) => format!(
            "Ended {ended} terminal session{}.",
            if ended == 1 { "" } else { "s" }
        ),
        Err(_) => String::new(),
    };
    let holders = app.terminals.holders(app.clock.now_ns());
    let custody = app.custody.view();
    let lines = plugin_lines(&app, &records, &custody).await;
    let body = overview::render(
        &records,
        &holders,
        &custody,
        &lines,
        &token_input(&session),
        &notice,
    );
    Html(page_with("Administer", &body, &admin_chrome(&session))).into_response()
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
    let (session, records) = match gate(&app, &headers, true) {
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
    // The plugin's own admin page, at /admin on its host, framed; the plugin
    // serves it to deployment admins alone, by the claim (W6.9).
    let theme = crate::plugins::Theme::of_mode(crate::web::mode_of(&app, &headers));
    let admin_page = match app.plugins.as_deref() {
        None => view::AdminPage::None(
            "This dashboard serves no plugin pages, so it cannot frame this one's.",
        ),
        Some(_) if !crate::plugins::is_instance(&instance) => view::AdminPage::None(
            "This instance's name cannot be a host, so its page cannot be framed.",
        ),
        Some(plugins) if plugins.frames() => view::AdminPage::Framed {
            src: crate::plugins::entrance(&instance, "/admin", &theme),
            origin: plugins.origin(&instance),
        },
        Some(_) => view::AdminPage::Linked(crate::plugins::entrance(&instance, "/admin", &theme)),
    };
    let reports = app.health.view();
    let body = view::render(&view::View {
        line,
        record: settings_of(&records, &instance),
        report: reports.get(&instance),
        records: &records,
        token: &token_input(&session),
        notice,
        development: crate::html::is_development(),
        admin_page,
    });
    let mut chrome = admin_chrome(&session);
    chrome.crumbs = format!(
        "<a href=\"/admin\">Admin portal</a><span aria-hidden=\"true\">/</span>\
         <a href=\"/admin#plugins\">Plugins</a><span aria-hidden=\"true\">/</span>\
         <span class=\"here\"><strong>{}</strong><code>{}</code></span>",
        escape(line.name.as_deref().unwrap_or(&instance)),
        escape(&instance)
    );
    Html(page_with(
        &format!("{} admin", line.name.as_deref().unwrap_or(&instance)),
        &body,
        &chrome,
    ))
    .into_response()
}

// ── A plugin instance's settings (W6.11) ────────────────────────────────────

mod settings;

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
    if let Err(response) = gate(&app, &headers, true) {
        return *response;
    }
    (
        StatusCode::SEE_OTHER,
        [(
            axum::http::header::LOCATION,
            format!("{}#settings", view::path(&instance)),
        )],
    )
        .into_response()
}

/// The form, as one command to the conductor. What was typed into a secret's
/// field goes there and nowhere else: not into a log line, and not back into
/// a page, including the one saying it was refused.
async fn set_settings(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(instance): Path<String>,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, records) = match gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if let Err(response) = form_token_matches(&session, &fields) {
        return *response;
    }
    let Some(record) = settings_of(&records, &instance) else {
        return no_such_plugin(&instance);
    };
    let back = view::path(&instance);
    let Some(request) = settings::request(record, &fields, crate::html::is_development()) else {
        return after_to(Ok(()), &format!("{back}?saved=none#settings"), &back);
    };
    let outcome = command::<PluginSettingsRecord>(
        &app,
        &session,
        "platform.config.command.set-plugin-settings",
        "meridian.v1.SetPluginSettingsRequest",
        request,
    )
    .await
    .map(|_| ());
    after_to(outcome, &format!("{back}?saved=1#settings"), &back)
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

async fn link(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    admin_form!(app, headers, fields, session, {
        let request = LinkExternalAccountRequest {
            plugin_instance_id: field(&fields, "plugin_instance_id").into(),
            external_account_id: field(&fields, "external_account_id").into(),
            account_id: field(&fields, "account_id").into(),
        };
        command::<meridian_domain::v1::ExternalAccountLink>(
            &app,
            &session,
            "platform.config.command.link-external-account",
            "meridian.v1.LinkExternalAccountRequest",
            request,
        )
        .await
        .map(|_| ())
    })
}

async fn define_user_group(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    admin_form!(app, headers, fields, session, {
        let request = DefineUserGroupRequest {
            user_group: Some(UserGroup {
                user_group_id: field(&fields, "user_group_id").into(),
                name: field(&fields, "name").into(),
                directory_groups: lines(&fields, "directory_groups"),
                logins: list(&fields, "logins"),
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

/// "plugin tag read|write", one per line.
pub fn parse_entries(text: &str) -> Result<Vec<AccessEntry>, String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            let [plugin, tag, level] = parts[..] else {
                return Err(format!("`{line}` is not `plugin tag read|write`"));
            };
            let level = match level {
                "read" => AccessLevel::Read,
                "write" => AccessLevel::Write,
                other => return Err(format!("`{other}` is not read or write")),
            };
            Ok(AccessEntry {
                plugin_instance_id: plugin.into(),
                tag: tag.into(),
                level: level as i32,
            })
        })
        .collect()
}

async fn define_access_group(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    admin_form!(app, headers, fields, session, {
        match parse_entries(field(&fields, "entries")) {
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

#[cfg(test)]
mod tests;
