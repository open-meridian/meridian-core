//! Service accounts' pages (contract v21): the deployment's Settings,
//! **Service accounts** tab (W6.32), a deployment admin's; an owner's own
//! rows and what each account did, with Disable (W6.34); and **Runs as** on
//! a plugin's Access tab (W6.33), each role's admin's.
//!
//! The one-screen rule throughout: one line a row, behind a pager with
//! search where rows scale, a dialog per change. Creating, changing,
//! granting, disabling and removing an account, and choosing what a role
//! runs as, are set at the page only: who acts is who holds access, the
//! ruled exception to parity. `dashboard__list_service_accounts` reads the
//! tab, and `dashboard__read_plugin_access` each role's choice ([`PARITY`]).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use meridian_access::{person_access, AccessLevel};
use meridian_domain::v1::AccessRecords;

use super::store::{RunsAs, ServiceAccount, Setting, State as AccountState};
use super::{Asked, By};
use crate::admin::people::Person;
use crate::html::{crumb_here, crumb_link, escape, page_with, Chrome, Report, Viewer};
use crate::session::Session;
use crate::web::App;

/// The routes and parts these pages draw, each against the tool that reads
/// it or its ruled exception (the plan's "Its pages and tools, under
/// parity"): [`tests`] fails the build where one has neither.
pub const PARITY: &[(&str, &str)] = &[
    ("tab service-accounts", "list_service_accounts"),
    ("view runs-as", "read_plugin_access"),
    (
        "/admin/service-accounts",
        "list_service_accounts; exception: creating and changing a service account is set at the page only (who holds access)",
    ),
    (
        "/admin/service-accounts/state",
        "list_service_accounts; exception: disabling, re-enabling and removing are set at the page only (who holds access)",
    ),
    ("/service-accounts", "list_service_accounts"),
    ("/service-accounts/{id}", "list_service_accounts"),
    (
        "/admin/plugins/{instance}/runs-as",
        "read_plugin_access; exception: what a role runs as is chosen at the page only (who holds access)",
    ),
];

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/admin/service-accounts", post(define))
        .route("/admin/service-accounts/state", post(set_state))
        .route("/service-accounts", get(owned_page))
        .route("/service-accounts/{id}", get(account_page))
        .route("/admin/plugins/{instance}/runs-as", post(choose))
}

type Fields = HashMap<String, String>;

fn field<'a>(fields: &'a Fields, name: &str) -> &'a str {
    fields.get(name).map(String::as_str).unwrap_or_default()
}

fn by_of(session: &Session, records: &AccessRecords) -> By {
    By {
        subject: session.subject.clone(),
        display_name: session.display_name.clone(),
        deployment_admin: person_access(records, &session.subject, &session.directory_groups)
            .deployment_admin,
    }
}

/// When, as a row says it: a date and a time, or a dash.
fn when(at: Option<i64>) -> String {
    at.map(crate::custody::utc).unwrap_or_else(|| "--".into())
}

fn token_input(session: &Session) -> String {
    format!(
        "<input type=\"hidden\" name=\"form_token\" value=\"{}\">",
        escape(&session.form_token)
    )
}

fn setting_badge(setting: Setting) -> String {
    format!(
        "<span class=\"badge{}\" data-setting=\"{}\">{}</span>",
        if setting == Setting::Act { " warn" } else { "" },
        setting.value(),
        setting.words()
    )
}

fn state_badge(state: AccountState) -> String {
    format!(
        "<span class=\"badge{}\" data-state=\"{}\">{}</span>",
        match state {
            AccountState::Active => " good",
            AccountState::Disabled => " warn",
            AccountState::Removed => "",
        },
        state.value(),
        state.words()
    )
}

/// The state form a row's buttons post: Disable, Re-enable or Remove.
fn state_button(id: &str, to: AccountState, label: &str, confirm: &str, token: &str) -> String {
    format!(
        "<form method=\"post\" action=\"/admin/service-accounts/state\" class=\"inline\" \
         data-confirm=\"{confirm}\">{token}<input type=\"hidden\" name=\"service_account_id\" value=\"{id}\">\
         <input type=\"hidden\" name=\"state\" value=\"{to}\"><button type=\"submit\">{label}</button></form>",
        id = escape(id),
        to = to.value(),
        confirm = escape(confirm),
    )
}

/// One account's line, as the tab and an owner's page draw it: name, owner,
/// setting, state, the roles it runs for, last used and last refusal, and
/// what the viewer may do with it.
fn line(
    owners: &BTreeMap<String, String>,
    account: &ServiceAccount,
    runs_for: &BTreeMap<String, BTreeSet<String>>,
    admin: bool,
    viewer: &str,
    token: &str,
) -> String {
    let id = &account.service_account_id;
    let roles: Vec<String> = runs_for
        .get(id)
        .map(|roles| roles.iter().cloned().collect())
        .unwrap_or_default();
    let mut actions = format!(
        "<a href=\"/service-accounts/{}\">What it did</a>",
        escape(id)
    );
    if admin && account.state != AccountState::Removed {
        let fill = serde_json::json!({"fields": {
            "service_account_id": id,
            "name": account.name,
            "purpose": account.purpose,
            "owner_subject": account.owner_subject,
            "setting": account.setting.value(),
        }});
        actions.push_str(&format!(
            " <button type=\"button\" data-dialog-open=\"service-account\" data-title=\"{}\" data-fill=\"{}\">Edit</button>",
            escape(&account.name),
            escape(&fill.to_string())
        ));
    }
    let owner = account.owner_subject == viewer;
    match account.state {
        AccountState::Active if admin || owner => actions.push_str(&state_button(
            id,
            AccountState::Disabled,
            "Disable",
            "Disable this service account? Its next call is refused and its waiting prepared \
             changes lapse.",
            token,
        )),
        AccountState::Disabled if admin => actions.push_str(&state_button(
            id,
            AccountState::Active,
            "Re-enable",
            "Re-enable this service account? Its calls are admitted again, within its grants.",
            token,
        )),
        _ => {}
    }
    if admin && account.state != AccountState::Removed {
        actions.push_str(&state_button(
            id,
            AccountState::Removed,
            "Remove",
            "Remove this service account? It is gone from use; every record it left is kept.",
            token,
        ));
    }
    let refusal = if account.last_refusal.is_empty() {
        "--".to_string()
    } else {
        format!(
            "<span title=\"{said}\">{at}: {said}</span>",
            said = escape(&account.last_refusal),
            at = escape(&when(account.last_refusal_ns))
        )
    };
    format!(
        "<tr data-id=\"{id}\" data-name=\"{name}\"><td title=\"{id}\"><span class=\"name\">{name}</span></td>\
         <td>{owner_name}</td><td>{setting}</td><td>{state}</td><td class=\"wide\">{roles}</td>\
         <td>{used}</td><td class=\"wide\">{refusal}</td><td class=\"actions\">{actions}</td></tr>",
        id = escape(id),
        name = escape(&account.name),
        owner_name = escape(
            owners
                .get(&account.owner_subject)
                .map(String::as_str)
                .unwrap_or(&account.owner_subject)
        ),
        setting = setting_badge(account.setting),
        state = state_badge(account.state),
        roles = if roles.is_empty() {
            "--".to_string()
        } else {
            escape(&roles.join(", "))
        },
        used = escape(&when(account.last_used_ns)),
    )
}

const HEAD: &str = "<th>Name</th><th>Owner</th><th>Setting</th><th>State</th>\
     <th class=\"wide\">Runs for</th><th>Last used</th><th class=\"wide\">Last refusal</th>\
     <th class=\"actions\"></th>";

/// The dialog creating or changing one: name, purpose, owner and setting,
/// and why.
fn dialog(people: &[Person], token: &str) -> String {
    let owners: String = people
        .iter()
        .filter(|person| !super::is_service_account(&person.login))
        .map(|person| {
            format!(
                "<option value=\"{login}\">{name}</option>",
                login = escape(&person.login),
                name = escape(if person.name.is_empty() {
                    &person.user_id
                } else {
                    &person.name
                }),
            )
        })
        .collect();
    format!(
        "<dialog id=\"service-account\" aria-labelledby=\"service-account-title\">\
         <form method=\"post\" action=\"/admin/service-accounts\">{token}\
         <input type=\"hidden\" name=\"service_account_id\" value=\"\">\
         <div class=\"dialog-head\"><h2 id=\"service-account-title\" data-title-new=\"A service account\">A service account</h2></div>\
         <div class=\"dialog-body\">\
         <label>Name <input name=\"name\" required maxlength=\"{name_most}\"></label>\
         <label>Purpose <textarea name=\"purpose\" maxlength=\"{purpose_most}\" rows=\"2\"></textarea></label>\
         <label>Owner <select name=\"owner_subject\" required><option value=\"\">A person</option>{owners}</select></label>\
         <label>Setting <select name=\"setting\"><option value=\"prepare_only\" selected>Prepare only</option>\
         <option value=\"act\">Act</option></select></label>\
         <label>Why <input name=\"note\" maxlength=\"{note_most}\"></label>\
         <p class=\"hint\">A user of this deployment that is not a person: it never signs in and holds \
         no password. Grant it as anyone is, by its ID in a user group's logins. Prepare only: every \
         act it would make is prepared for a person to make. Act: it makes an act where every level \
         that applies lets agents make it and no second pair of eyes is on. Its owner sees what it \
         does and may disable it.</p></div>\
         <div class=\"dialog-foot\"><button type=\"button\" data-dialog-close>Cancel</button>\
         <button type=\"submit\" class=\"primary\" data-label-new=\"Create\">Save</button></div></form></dialog>",
        name_most = super::NAME_MOST,
        purpose_most = super::PURPOSE_MOST,
        note_most = super::NOTE_MOST,
    )
}

/// The Settings tab (W6.32): its body and its dialog.
pub fn settings_section(
    owners: &BTreeMap<String, String>,
    accounts: &[ServiceAccount],
    chosen: &[RunsAs],
    people: &[Person],
    viewer: &str,
    token: &str,
) -> (String, String) {
    let runs_for = super::runs_for(chosen);
    let body = if accounts.is_empty() {
        "<p class=\"empty\">No service account yet. One runs a plugin's autonomous work -- a \
         scheduled sweep, a strategy -- as a user of its own, granted as anyone is.</p>"
            .to_string()
    } else {
        let rows: String = accounts
            .iter()
            .map(|account| line(owners, account, &runs_for, true, viewer, token))
            .collect();
        crate::admin::listing(
            "service-accounts-table",
            " one-line service-accounts",
            "service accounts",
            "Search by name, owner, state or role",
            HEAD,
            &rows,
        )
    };
    (body, dialog(people, token))
}

/// Each role's "runs as" on a plugin's Access tab (W6.33): the account
/// chosen and its level, or nobody; set there by the role's admin among the
/// accounts granted on it.
pub fn runs_as_part(
    app: &App,
    records: &AccessRecords,
    instance: &str,
    chosen: &[RunsAs],
    administers: &dyn Fn(&str) -> bool,
    token: &str,
) -> String {
    let mut roles = super::roles_of(records, instance);
    if roles.is_empty() {
        roles.push(String::new());
    }
    let mut dialogs = String::new();
    let rows: String = roles
        .iter()
        .map(|role| {
            let held = chosen
                .iter()
                .find(|held| held.instance == instance && &held.role == role);
            let granted = super::granted_accounts(app, records, instance, role);
            let (runs, standing) = match held {
                None => ("nobody".to_string(), String::new()),
                Some(held) => {
                    let account = app.service_accounts.held(&held.service_account_id);
                    let name = account
                        .as_ref()
                        .map(|account| account.name.clone())
                        .unwrap_or_else(|| held.service_account_id.clone());
                    // Shown so where its grant was withdrawn, or it was
                    // disabled or removed: every call is refused until the
                    // grant returns or the admin chooses again.
                    let grant = super::granted_on(records, &held.service_account_id, instance, role);
                    let standing = match (&account, grant) {
                        (Some(account), _) if account.state != AccountState::Active => {
                            format!(" <span class=\"badge warn\">{}</span>", account.state.words())
                        }
                        (_, None) => " <span class=\"badge warn\">Not granted</span>".to_string(),
                        (_, Some(AccessLevel::Read)) if held.level == "write" => {
                            " <span class=\"badge warn\">Granted read</span>".to_string()
                        }
                        _ => String::new(),
                    };
                    (format!("{name} at {}", held.level), standing)
                }
            };
            let key = if role.is_empty() { "plugin" } else { role.as_str() };
            let edit = if administers(role) {
                let id = format!("runs-as-{}", key);
                let options: String = granted
                    .iter()
                    .map(|(account, level)| {
                        format!(
                            "<option value=\"{id}\"{selected}>{name} ({level})</option>",
                            id = escape(&account.service_account_id),
                            selected = if held.is_some_and(|h| h.service_account_id == account.service_account_id) {
                                " selected"
                            } else {
                                ""
                            },
                            name = escape(&account.name),
                            level = meridian_access::level_name(*level),
                        )
                    })
                    .collect();
                dialogs.push_str(&format!(
                    "<dialog id=\"{id}\" aria-labelledby=\"{id}-title\"><form method=\"post\" \
                     action=\"/admin/plugins/{instance}/runs-as\">{token}<input type=\"hidden\" name=\"role\" value=\"{role}\">\
                     <div class=\"dialog-head\"><h2 id=\"{id}-title\">What {label} runs as</h2></div><div class=\"dialog-body\">\
                     <label>Runs as <select name=\"service_account_id\"><option value=\"\">Nobody</option>{options}</select></label>\
                     <label>Level <select name=\"level\"><option value=\"write\">Write</option><option value=\"read\">Read</option></select></label>\
                     <label>Why <input name=\"note\" maxlength=\"{note_most}\"></label>\
                     <p class=\"hint\">Among the service accounts a deployment admin granted on {label}, at a level \
                     no higher than its grant. An act needs write. What agents may do with each act binds it as \
                     anyone, and its own setting holds it back further.</p></div>\
                     <div class=\"dialog-foot\"><button type=\"button\" data-dialog-close>Cancel</button>\
                     <button type=\"submit\" class=\"primary\">Save</button></div></form></dialog>",
                    instance = escape(instance),
                    role = escape(role),
                    label = escape(if role.is_empty() { "this plugin" } else { role }),
                    note_most = super::NOTE_MOST,
                ));
                format!("<button type=\"button\" data-dialog-open=\"{id}\">Edit</button>")
            } else {
                String::new()
            };
            format!(
                "<tr data-id=\"{key}\" data-name=\"{label}\"><td><span class=\"name\">{label}</span></td>\
                 <td data-runs-as=\"{chosen_id}\">{runs}{standing}</td><td>{granted}</td><td class=\"actions\">{edit}</td></tr>",
                key = escape(key),
                label = escape(if role.is_empty() { "the plugin" } else { role }),
                chosen_id = escape(held.map(|h| h.service_account_id.as_str()).unwrap_or("")),
                runs = escape(&runs),
                granted = granted.len(),
            )
        })
        .collect();
    let table = crate::admin::listing(
        "runs-as-table",
        " one-line runs-as",
        "roles",
        "Search by role",
        "<th>Role</th><th>Runs as</th><th>Granted</th><th class=\"actions\"></th>",
        &rows,
    );
    format!(
        "<h2>Runs as</h2><p class=\"hint\">The service account each role's autonomous work runs \
         as, chosen by the role's admin among those granted on it. A role runs as nobody until \
         one is chosen, and its autonomous work is refused, saying so.</p>{table}{dialogs}"
    )
}

/// `POST /admin/service-accounts`: create one, or change one named.
async fn define(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, records) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if let Err(response) = crate::admin::form_token_matches(&session, &fields) {
        return *response;
    }
    let by = by_of(&session, &records);
    let setting = match field(&fields, "setting") {
        "" => None,
        said => match Setting::parse(said) {
            Some(setting) => Some(setting),
            None => {
                return crate::admin::after_to(
                    Err(format!("setting: {said:?} is not prepare_only or act")),
                    "/admin#service-accounts",
                    "/admin#service-accounts",
                )
            }
        },
    };
    let asked = Asked {
        name: field(&fields, "name").to_string(),
        purpose: field(&fields, "purpose").to_string(),
        owner_subject: field(&fields, "owner_subject").to_string(),
        setting,
    };
    let note = field(&fields, "note");
    let id = field(&fields, "service_account_id");
    let outcome = if id.is_empty() {
        super::create(&app, &by, &asked, note).await.map(|_| ())
    } else {
        super::change_parts(&app, &by, id, &asked, note)
            .await
            .map(|_| ())
    };
    crate::admin::after_to(
        outcome.map_err(|refused| refused.to_string()),
        "/admin#service-accounts",
        "/admin#service-accounts",
    )
}

/// `POST /admin/service-accounts/state`: disable (a deployment admin or the
/// owner), re-enable or remove (a deployment admin).
async fn set_state(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, records) = match crate::admin::gate(&app, &headers, false) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if let Err(response) = crate::admin::form_token_matches(&session, &fields) {
        return *response;
    }
    let by = by_of(&session, &records);
    let back = if by.deployment_admin {
        "/admin#service-accounts"
    } else {
        "/service-accounts"
    };
    let outcome = match AccountState::parse(field(&fields, "state")) {
        Some(to) => super::set_state(
            &app,
            &by,
            field(&fields, "service_account_id"),
            to,
            field(&fields, "note"),
        )
        .await
        .map(|_| ())
        .map_err(|refused| refused.to_string()),
        None => Err(format!(
            "state: {:?} is not active, disabled or removed",
            field(&fields, "state")
        )),
    };
    crate::admin::after_to(outcome, back, back)
}

fn person_chrome<'a>(session: &'a Session, admin: bool, crumbs: String) -> Chrome<'a> {
    Chrome {
        viewer: Some(Viewer {
            display_name: &session.display_name,
            form_token: &session.form_token,
            admin,
        }),
        crumbs,
        main: "page",
        in_admin: false,
        report: Report::Dashboard,
    }
}

/// `GET /service-accounts`: the service accounts the person owns, read
/// only, each with Disable and what it did (W6.34); every one for a
/// deployment admin.
async fn owned_page(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let (session, records) = match crate::admin::gate(&app, &headers, false) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let by = by_of(&session, &records);
    let (accounts, chosen) = match (
        app.service_accounts.accounts().await,
        app.service_accounts.runs_as().await,
    ) {
        (Ok(accounts), Ok(chosen)) => (accounts, chosen),
        (Err(why), _) | (_, Err(why)) => return crate::web::refused(&why),
    };
    let mine: Vec<&ServiceAccount> = accounts
        .iter()
        .filter(|account| by.deployment_admin || account.owner_subject == session.subject)
        .collect();
    let runs_for = super::runs_for(&chosen);
    let owners = super::owner_names(&app, &records, &accounts).await;
    let token = token_input(&session);
    let body = if mine.is_empty() {
        "<p class=\"empty\">You own no service account. A deployment admin names its owner when \
         creating one.</p>"
            .to_string()
    } else {
        let rows: String = mine
            .iter()
            .map(|account| line(&owners, account, &runs_for, false, &session.subject, &token))
            .collect();
        crate::admin::listing(
            "owned-service-accounts-table",
            " one-line service-accounts",
            "service accounts",
            "Search by name, state or role",
            HEAD,
            &rows,
        )
    };
    let body = format!(
        "<div class=\"admin\"><div class=\"page-head\"><div><h1>Service accounts</h1>\
         <p>The service accounts you answer for: what each runs, and when it was last used and \
         refused. You may disable one at once; re-enabling and changing it are a deployment \
         admin's.</p></div></div>{body}</div>"
    );
    Html(page_with(
        "Service accounts",
        &body,
        &person_chrome(
            &session,
            by.deployment_admin,
            crumb_link("/", "Home") + &crumb_here("Service accounts", None),
        ),
    ))
    .into_response()
}

/// `GET /service-accounts/{id}`: what one did -- each act it made, each
/// change it prepared and each refusal, by name and outcome, from the call
/// record -- its prepared changes, and its record of changes; for its owner
/// and a deployment admin. A prepared change's contents open on its own page,
/// within the reader's own read access.
async fn account_page(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let (session, records) = match crate::admin::gate(&app, &headers, false) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let by = by_of(&session, &records);
    let account = match app.service_accounts.account(&id).await {
        Ok(Some(account)) if by.deployment_admin || account.owner_subject == session.subject => {
            account
        }
        Ok(_) => return (
            StatusCode::NOT_FOUND,
            Html(page_with(
                "Not found",
                "<h1>Not found</h1><p>No service account you own or administer has that ID.</p>",
                &person_chrome(&session, by.deployment_admin, crumb_link("/", "Home")),
            )),
        )
            .into_response(),
        Err(why) => return crate::web::refused(&why),
    };
    let calls = app
        .delegations
        .calls(
            crate::delegation::CallsOf::Person(account.service_account_id.clone()),
            200,
        )
        .await
        .unwrap_or_default();
    let prepared = {
        let id = account.service_account_id.clone();
        app.prepared
            .blocking(move |store| store.by_preparer(&id, 200))
            .await
            .unwrap_or_default()
    };
    let changes = app
        .service_accounts
        .changes(&account.service_account_id)
        .await
        .unwrap_or_default();
    let call_rows: String = calls
        .iter()
        .map(|call| {
            format!(
                "<tr data-outcome=\"{outcome}\"><td>{at}</td><td title=\"{tool}\"><span class=\"name\">{tool}</span></td>\
                 <td><span class=\"badge{tone}\">{outcome}</span></td><td class=\"wide\" title=\"{reason}\">{reason}</td></tr>",
                at = escape(&crate::custody::utc(call.called_at_ns)),
                tool = escape(&call.tool),
                outcome = escape(&call.outcome),
                tone = match call.outcome.as_str() {
                    "refused" => " warn",
                    "made" => " good",
                    _ => "",
                },
                reason = escape(&call.reason),
            )
        })
        .collect();
    let prepared_rows: String = prepared
        .iter()
        .map(|kept| {
            format!(
                "<tr data-id=\"{id}\"><td><a href=\"/prepared/{id}\"><code>{id}</code></a></td>\
                 <td title=\"{tool}\">{tool}</td><td>{state}</td><td>{at}</td></tr>",
                id = escape(&kept.prepared_call_id),
                tool = escape(&kept.tool_name),
                state = crate::prepared::state_words(kept.state),
                at = escape(&crate::custody::utc(kept.prepared_at_ns)),
            )
        })
        .collect();
    let change_rows: String = changes
        .iter()
        .rev()
        .map(|change| {
            format!(
                "<tr><td>{at}</td><td>{scope}</td><td class=\"wide\" title=\"{from}\">{from}</td>\
                 <td class=\"wide\" title=\"{to}\">{to}</td><td>{who}</td><td class=\"wide\" title=\"{note}\">{note}</td></tr>",
                at = escape(&crate::custody::utc(change.at_ns)),
                scope = escape(if change.scope.is_empty() { "the account" } else { &change.scope }),
                from = escape(&change.from),
                to = escape(&change.to),
                who = escape(&crate::tickets::display_name(&records, &change.by)),
                note = escape(&change.note),
            )
        })
        .collect();
    let token = token_input(&session);
    let runs_for = super::runs_for(&app.service_accounts.runs_as().await.unwrap_or_default());
    let head = crate::admin::listing(
        "service-account-row",
        " one-line service-accounts",
        "service accounts",
        "",
        HEAD,
        &line(
            &super::owner_names(&app, &records, std::slice::from_ref(&account)).await,
            &account,
            &runs_for,
            by.deployment_admin,
            &session.subject,
            &token,
        ),
    );
    let parts = crate::admin::parts_shown(
        "What it did",
        &[
            (
                "calls",
                "Its calls",
                if call_rows.is_empty() {
                    "<p class=\"empty\">It has made no call yet.</p>".to_string()
                } else {
                    crate::admin::listing(
                        "service-account-calls",
                        " one-line calls",
                        "calls",
                        "Search by act or outcome",
                        "<th>When</th><th>Act</th><th>Outcome</th><th class=\"wide\">Why refused</th>",
                        &call_rows,
                    )
                },
            ),
            (
                "prepared",
                "Its prepared changes",
                if prepared_rows.is_empty() {
                    "<p class=\"empty\">It has prepared no change.</p>".to_string()
                } else {
                    crate::admin::listing(
                        "service-account-prepared",
                        " one-line prepared",
                        "prepared changes",
                        "Search by act or state",
                        "<th>Prepared change</th><th>Act</th><th>State</th><th>When</th>",
                        &prepared_rows,
                    )
                },
            ),
            (
                "changes",
                "Its record",
                crate::admin::listing(
                    "service-account-changes",
                    " one-line changes",
                    "changes",
                    "Search by part or person",
                    "<th>When</th><th>Scope</th><th class=\"wide\">From</th><th class=\"wide\">To</th>\
                     <th>By</th><th class=\"wide\">Why</th>",
                    &change_rows,
                ),
            ),
        ],
    );
    let back = if by.deployment_admin {
        crumb_link("/admin", "Settings")
            + &crumb_link("/admin#service-accounts", "Service accounts")
    } else {
        crumb_link("/", "Home") + &crumb_link("/service-accounts", "Service accounts")
    };
    let body = format!(
        "<div class=\"admin\"><div class=\"page-head\"><div><h1>{name}</h1><p><code>{id}</code> &middot; \
         a service account: what it did, by name and outcome. A prepared change opens on its own \
         page, within your own read access.</p></div></div>{head}{parts}</div>",
        name = escape(&account.name),
        id = escape(&account.service_account_id),
    );
    Html(page_with(
        &account.name,
        &body,
        &person_chrome(
            &session,
            by.deployment_admin,
            back + &crumb_here(&account.name, Some(&account.service_account_id)),
        ),
    ))
    .into_response()
}

/// `POST /admin/plugins/{instance}/runs-as`: a role's admin chooses what the
/// role runs as, or clears it (W6.33).
async fn choose(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(instance): Path<String>,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, records) = match crate::admin::gate(&app, &headers, false) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if let Err(response) = crate::admin::form_token_matches(&session, &fields) {
        return *response;
    }
    let back = crate::admin::view::tab_href(&instance, crate::admin::view::ACCESS);
    let role = field(&fields, "role");
    let access = person_access(&records, &session.subject, &session.directory_groups);
    let held = access.plugin(&instance);
    let administers = if role.is_empty() {
        held.administers("")
    } else {
        held.administers(role)
    };
    let id = field(&fields, "service_account_id");
    let level = match field(&fields, "level") {
        "" | "write" => AccessLevel::Write,
        "read" => AccessLevel::Read,
        other => {
            return crate::admin::after_to(
                Err(format!("level: {other:?} is not write or read")),
                &back,
                &back,
            )
        }
    };
    let by = by_of(&session, &records);
    let outcome = super::choose(
        &app,
        &by,
        administers,
        &instance,
        role,
        (!id.is_empty()).then_some((id, level)),
        field(&fields, "note"),
    )
    .await
    .map(|_| ())
    .map_err(|refused| refused.to_string());
    let done = format!("{back}#part-runs-as");
    crate::admin::after_to(outcome, &done, &back)
}
