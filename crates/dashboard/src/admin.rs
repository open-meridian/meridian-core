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

use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use meridian_access::{person_access, DEPLOYMENT_ADMIN};
use meridian_bus::BusError;
use meridian_domain::v1::{
    AccessEntry, AccessGroup, AccessLevel, AccessRecords, AccountGroup, AccountState,
    ClaimCodePurpose, CloseAccountRequest, DefineAccessGroupRequest, DefineAccountGroupRequest,
    DefineAccountRequest, DefineUserGroupRequest, GrantPermissionRequest,
    LinkExternalAccountRequest, RedeemClaimCodeReply, RedeemClaimCodeRequest, UserGroup,
    WithdrawPermissionReply, WithdrawPermissionRequest,
};
use prost::Message;

use crate::html::{escape, page};
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
    match outcome {
        Ok(()) => (
            StatusCode::SEE_OTHER,
            [(axum::http::header::LOCATION, "/admin")],
        )
            .into_response(),
        Err(sentence) => status_page(StatusCode::BAD_REQUEST, "Not done", &sentence),
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

fn token_input(session: &Session) -> String {
    format!(
        "<input type=\"hidden\" name=\"form_token\" value=\"{}\">",
        escape(&session.form_token)
    )
}

// ── The overview ────────────────────────────────────────────────────────────

fn level_name(level: i32) -> &'static str {
    if level == AccessLevel::Write as i32 {
        "write"
    } else {
        "read"
    }
}

async fn admin_page(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(query): Query<Fields>,
) -> Response {
    let (session, records) = match gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let token = token_input(&session);
    let mut body = String::from("<h1>Administer this deployment</h1><p><a href=\"/\">Home</a></p>");
    // What the last form did, where it has something to say.
    if let Ok(ended) = field(&query, "terminal_sessions_ended").parse::<usize>() {
        body.push_str(&format!(
            "<p class=\"done\">Ended {ended} terminal session{}.</p>",
            if ended == 1 { "" } else { "s" }
        ));
    }

    body.push_str(
        "<h2>Accounts</h2><table><tr><th>Account</th><th>Name</th><th>State</th><th></th></tr>",
    );
    for account in &records.accounts {
        let closed = account.state == AccountState::Closed as i32;
        body.push_str(&format!(
            "<tr><td>{id}</td><td>{name}</td><td>{state}</td><td>{close}</td></tr>",
            id = escape(&account.account_id),
            name = escape(&account.name),
            state = if closed { "closed" } else { "open" },
            close = if closed {
                String::new()
            } else {
                format!(
                    "<form method=\"post\" action=\"/admin/accounts/close\">{token}\
                     <input type=\"hidden\" name=\"account_id\" value=\"{}\"><button>Close</button></form>",
                    escape(&account.account_id)
                )
            },
        ));
    }
    body.push_str(&format!(
        "</table><form method=\"post\" action=\"/admin/accounts\">{token}\
         <label>Account to rename (empty creates) <input name=\"account_id\"></label> \
         <label>Name <input name=\"name\" required></label> <button>Save account</button></form>"
    ));

    body.push_str("<h2>User groups</h2><table><tr><th>Group</th><th>Name</th><th>Directory groups</th><th>Logins</th></tr>");
    for group in &records.user_groups {
        body.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape(&group.user_group_id),
            escape(&group.name),
            escape(&group.directory_groups.join("; ")),
            escape(&group.logins.join(", "))
        ));
    }
    body.push_str(&format!(
        "</table><form method=\"post\" action=\"/admin/user-groups\">{token}\
         <label>Group to change (empty creates) <input name=\"user_group_id\"></label> \
         <label>Name <input name=\"name\" required></label> \
         <label>Directory groups, one per line <textarea name=\"directory_groups\"></textarea></label> \
         <label>Logins <input name=\"logins\"></label> <button>Save user group</button></form>"
    ));

    body.push_str(
        "<h2>Account groups</h2><table><tr><th>Group</th><th>Name</th><th>Accounts</th></tr>",
    );
    for group in &records.account_groups {
        body.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape(&group.account_group_id),
            escape(&group.name),
            escape(&group.account_ids.join(", "))
        ));
    }
    body.push_str(&format!(
        "</table><form method=\"post\" action=\"/admin/account-groups\">{token}\
         <label>Group to change (empty creates) <input name=\"account_group_id\"></label> \
         <label>Name <input name=\"name\" required></label> \
         <label>Accounts <input name=\"account_ids\"></label> <button>Save account group</button></form>"
    ));

    body.push_str(
        "<h2>Access groups</h2><table><tr><th>Group</th><th>Name</th><th>Entries</th></tr>",
    );
    for group in &records.access_groups {
        let entries = if group.built_in {
            "the dashboard, and every account".to_string()
        } else {
            group
                .entries
                .iter()
                .map(|e| format!("{} {} {}", e.plugin_instance_id, e.tag, level_name(e.level)))
                .collect::<Vec<_>>()
                .join("; ")
        };
        body.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape(&group.access_group_id),
            escape(&group.name),
            escape(&entries)
        ));
    }
    body.push_str(&format!(
        "</table><form method=\"post\" action=\"/admin/access-groups\">{token}\
         <label>Group to change (empty creates) <input name=\"access_group_id\"></label> \
         <label>Name <input name=\"name\" required></label> \
         <label>Entries, one per line: plugin tag read|write <textarea name=\"entries\"></textarea></label> \
         <button>Save access group</button></form>"
    ));

    body.push_str("<h2>Permissions</h2><table><tr><th>Permission</th><th>User group</th><th>Account group</th><th>Access group</th><th></th></tr>");
    for permission in &records.permissions {
        body.push_str(&format!(
            "<tr><td>{id}</td><td>{}</td><td>{}</td><td>{}</td><td>\
             <form method=\"post\" action=\"/admin/permissions/withdraw\">{token}\
             <input type=\"hidden\" name=\"permission_id\" value=\"{id}\"><button>Withdraw</button></form></td></tr>",
            escape(&permission.user_group_id),
            escape(if permission.account_group_id.is_empty() { "every account" } else { &permission.account_group_id }),
            escape(&permission.access_group_id),
            id = escape(&permission.permission_id),
        ));
    }
    body.push_str(&format!(
        "</table><form method=\"post\" action=\"/admin/permissions\">{token}\
         <label>User group <input name=\"user_group_id\" required></label> \
         <label>Account group (empty for deployment admin) <input name=\"account_group_id\"></label> \
         <label>Access group <input name=\"access_group_id\" required></label> <button>Grant</button></form>"
    ));

    body.push_str(&format!(
        "<h2>External accounts</h2><form method=\"post\" action=\"/admin/links\">{token}\
         <label>Plugin <input name=\"plugin_instance_id\" required></label> \
         <label>External account <input name=\"external_account_id\" required></label> \
         <label>Account (empty unlinks) <input name=\"account_id\"></label> <button>Link</button></form>"
    ));

    // W6.14. Per person: what is being ended is their access from a
    // terminal, so there is no choosing among their sessions to offer.
    body.push_str("<h2>Terminal sessions</h2>");
    let holders = app.terminals.holders(app.clock.now_ns());
    if holders.is_empty() {
        body.push_str("<p>Nobody holds a terminal session.</p>");
    } else {
        body.push_str("<table><tr><th>Person</th><th>Login</th><th>Sessions</th><th></th></tr>");
        for (login, name, count) in &holders {
            body.push_str(&format!(
                "<tr><td>{name}</td><td>{login}</td><td>{count}</td><td>\
                 <form method=\"post\" action=\"/admin/end-terminal-sessions\">{token}\
                 <input type=\"hidden\" name=\"login\" value=\"{login}\">\
                 <button>End them</button></form></td></tr>",
                name = escape(name),
                login = escape(login),
            ));
        }
        body.push_str("</table>");
    }

    Html(page("Administer", &body)).into_response()
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

async fn define_account_group(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
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
