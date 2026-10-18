//! A connectivity plugin's `ems`, on the plugin's Access tab (contract v22;
//! W11.21, W11.22; ruled 2026-10-10).
//!
//! For an instance holding `connectivity`: whether it is available -- a
//! deployment admin makes it so, or withdraws it -- and the one `ems` it is
//! linked to with who may route through it, which that `ems`'s admin sets.
//! Who may route is access, so both are set at this page only (the parity
//! exception) and read through `dashboard__read_plugin_access`. For an
//! instance holding `ems`: the connectivity plugins linked to it and its
//! venue account map, read-only here -- the `ems` maps an account on its own
//! page, acting for its admin (W11.22). Each change is the conductor's own
//! record; this page sends the command and shows what stands
//! (AccessRecords.connectivity_links, venue_account_maps).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::Form;
use meridian_access::person_access;
use meridian_domain::v1::{
    AccessRecords, LinkConnectivityRequest, SetConnectivityAvailableRequest,
};
use meridian_pb::v1::{connectivity_user, ConnectivityLink, ConnectivityUser};

use crate::html::escape;
use crate::web::App;

pub const SET_CONNECTIVITY_AVAILABLE: &str = "platform.config.command.set-connectivity-available";
pub const LINK_CONNECTIVITY: &str = "platform.config.command.link-connectivity";

/// The most a note on a link's change holds (the dictionary's bound).
const NOTE_MOST: usize = 2000;

fn holds(records: &AccessRecords, instance: &str, role: &str) -> bool {
    meridian_access::known_roles(records, instance)
        .is_some_and(|roles| roles.iter().any(|held| held == role))
}

/// The instances holding `ems` the deployment knows.
fn ems_instances(records: &AccessRecords) -> Vec<String> {
    records
        .known_plugins
        .iter()
        .filter(|known| known.roles.iter().any(|role| role == "ems"))
        .map(|known| known.plugin_instance_id.clone())
        .collect()
}

fn link_of(records: &AccessRecords, instance: &str) -> ConnectivityLink {
    records
        .connectivity_links
        .iter()
        .find(|link| link.connectivity_instance_id == instance)
        .cloned()
        .unwrap_or_else(|| ConnectivityLink {
            connectivity_instance_id: instance.to_string(),
            ..Default::default()
        })
}

/// A user as a person reads it, and as the form names it (`kind:id`).
fn user_words(
    records: &AccessRecords,
    user: &ConnectivityUser,
    names: &[(String, String)],
) -> String {
    match &user.user {
        Some(connectivity_user::User::Subject(subject)) => records
            .people
            .iter()
            .find(|person| &person.subject == subject)
            .map(|person| person.display_name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| subject.clone()),
        Some(connectivity_user::User::ServiceAccountId(id)) => names
            .iter()
            .find(|(held, _)| held == id)
            .map(|(_, name)| format!("{name} (service account)"))
            .unwrap_or_else(|| id.clone()),
        Some(connectivity_user::User::UserGroupId(id)) => records
            .user_groups
            .iter()
            .find(|group| &group.user_group_id == id)
            .map(|group| format!("{} (user group)", group.name))
            .unwrap_or_else(|| id.clone()),
        None => String::new(),
    }
}

fn user_key(user: &ConnectivityUser) -> String {
    match &user.user {
        Some(connectivity_user::User::Subject(id)) => format!("person:{id}"),
        Some(connectivity_user::User::ServiceAccountId(id)) => format!("account:{id}"),
        Some(connectivity_user::User::UserGroupId(id)) => format!("group:{id}"),
        None => String::new(),
    }
}

fn user_of(key: &str) -> Option<ConnectivityUser> {
    let (kind, id) = key.split_once(':')?;
    let id = id.trim().to_string();
    if id.is_empty() {
        return None;
    }
    let user = match kind {
        "person" => connectivity_user::User::Subject(id),
        "account" => connectivity_user::User::ServiceAccountId(id),
        "group" => connectivity_user::User::UserGroupId(id),
        _ => return None,
    };
    Some(ConnectivityUser { user: Some(user) })
}

/// What the viewer may do here: whether they are a deployment admin, and
/// which `ems` instances they administer.
pub struct Viewer<'a> {
    pub deployment_admin: bool,
    pub administers_ems: &'a (dyn Fn(&str) -> bool + Sync),
}

/// The Access tab's part, or nothing for an instance holding neither role.
/// `accounts` names each service account, for the users offered and shown.
pub fn part(
    records: &AccessRecords,
    instance: &str,
    viewer: &Viewer,
    accounts: &[(String, String)],
    token: &str,
) -> String {
    let mut out = String::new();
    if holds(records, instance, "connectivity") {
        out.push_str(&connectivity_part(
            records, instance, viewer, accounts, token,
        ));
    }
    if holds(records, instance, "ems") {
        out.push_str(&ems_part(records, instance, viewer, accounts, token));
    }
    out
}

/// Who may route through `link`, as the searched picker named `id` offers
/// them -- user groups, people and service accounts -- with its users chosen.
fn users_picker(
    records: &AccessRecords,
    link: &ConnectivityLink,
    accounts: &[(String, String)],
    id: &str,
) -> String {
    let chosen: Vec<String> = link.users.iter().map(user_key).collect();
    let mut offered: Vec<(String, String)> = records
        .user_groups
        .iter()
        .map(|group| {
            (
                format!("group:{}", group.user_group_id),
                format!("{} (user group)", group.name),
            )
        })
        .collect();
    offered.extend(records.people.iter().map(|person| {
        (
            format!("person:{}", person.subject),
            if person.display_name.is_empty() {
                person.subject.clone()
            } else {
                person.display_name.clone()
            },
        )
    }));
    offered.extend(
        accounts
            .iter()
            .map(|(id, name)| (format!("account:{id}"), format!("{name} (service account)"))),
    );
    // The dashboard's searched picker (the product owner, 2026-09-30:
    // "should have search boxes"), as a user group's people are chosen.
    let choices: Vec<super::picker::Choice> = offered
        .iter()
        .map(|(key, label)| super::picker::Choice {
            value: key.clone(),
            label: label.clone(),
            detail: key
                .split_once(':')
                .map(|(_, id)| id.to_string())
                .unwrap_or_default(),
            ..Default::default()
        })
        .collect();
    let picked: std::collections::HashSet<&str> = chosen.iter().map(String::as_str).collect();
    super::picker::many(
        id,
        "user",
        "Who may route through it",
        "people, service accounts and user groups",
        &choices,
        &picked,
    )
}

/// A link's dialog: its `id` and `title`; `ems`, the select's options,
/// already HTML; `from_ems` when it opens on the ems's own page, which the
/// change returns to.
struct LinkDialog<'a> {
    id: &'a str,
    title: &'a str,
    ems: &'a str,
    from_ems: bool,
}

/// The dialog linking `link`'s connectivity plugin to an ems.
fn link_dialog(
    records: &AccessRecords,
    link: &ConnectivityLink,
    accounts: &[(String, String)],
    token: &str,
    dialog: LinkDialog,
) -> String {
    let LinkDialog {
        id,
        title,
        ems,
        from_ems,
    } = dialog;
    let instance = escape(&link.connectivity_instance_id);
    let boxes = users_picker(records, link, accounts, &format!("{id}-users"));
    let back = if from_ems {
        "<input type=\"hidden\" name=\"back\" value=\"ems\">"
    } else {
        ""
    };
    format!(
        "<dialog id=\"{id}\" aria-labelledby=\"{id}-title\">\
         <form method=\"post\" action=\"/admin/plugins/{instance}/connectivity/link\">{token}{back}\
         <div class=\"dialog-head\"><h2 id=\"{id}-title\">{title}</h2></div>\
         <div class=\"dialog-body\"><label>The ems <select name=\"ems_instance_id\">{ems}</select></label>\
         {boxes}\
         <label>Why <input name=\"note\" maxlength=\"{NOTE_MOST}\"></label>\
         <p class=\"hint\">Each connectivity plugin is linked to one ems. A placement for anyone \
         not chosen is refused and recorded. Unlinking leaves the placements already made as they \
         are.</p></div><div class=\"dialog-foot\"><button type=\"button\" data-dialog-close>Cancel</button>\
         <button type=\"submit\" class=\"primary\">Save</button></div></form></dialog>",
        title = escape(title),
    )
}

fn connectivity_part(
    records: &AccessRecords,
    instance: &str,
    viewer: &Viewer,
    accounts: &[(String, String)],
    token: &str,
) -> String {
    let link = link_of(records, instance);
    let id = escape(instance);
    let users = if link.users.is_empty() {
        "nobody".to_string()
    } else {
        link.users
            .iter()
            .map(|user| escape(&user_words(records, user, accounts)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let ems = if link.ems_instance_id.is_empty() {
        "none".to_string()
    } else {
        format!(
            "<a href=\"{}\">{}</a>",
            escape(&super::view::path(&link.ems_instance_id)),
            escape(&link.ems_instance_id)
        )
    };
    let mut actions = String::new();
    let mut dialogs = String::new();
    if viewer.deployment_admin {
        let (label, value) = if link.available {
            ("Withdraw", "false")
        } else {
            ("Make available", "true")
        };
        actions.push_str(&format!(
            "<button type=\"button\" data-dialog-open=\"connectivity-available\">{label}</button>"
        ));
        dialogs.push_str(&format!(
            "<dialog id=\"connectivity-available\" aria-labelledby=\"connectivity-available-title\">\
             <form method=\"post\" action=\"/admin/plugins/{id}/connectivity/available\">{token}\
             <input type=\"hidden\" name=\"available\" value=\"{value}\">\
             <div class=\"dialog-head\"><h2 id=\"connectivity-available-title\">{label} {id}</h2></div>\
             <div class=\"dialog-body\"><p class=\"hint\">Available, an ems's admin may link it to \
             their ems; withdrawn, it reaches no account. The placements already made stay as they \
             are.</p><label>Why <input name=\"note\" maxlength=\"{NOTE_MOST}\"></label></div>\
             <div class=\"dialog-foot\"><button type=\"button\" data-dialog-close>Cancel</button>\
             <button type=\"submit\" class=\"primary\">{label}</button></div></form></dialog>"
        ));
    }
    let linkable: Vec<String> = ems_instances(records)
        .into_iter()
        .filter(|ems| (viewer.administers_ems)(ems))
        .collect();
    let may_link = !linkable.is_empty()
        && (link.ems_instance_id.is_empty() || linkable.contains(&link.ems_instance_id));
    if may_link && (link.available || !link.ems_instance_id.is_empty()) {
        let options: String = linkable
            .iter()
            .map(|ems| {
                format!(
                    "<option value=\"{e}\"{selected}>{e}</option>",
                    e = escape(ems),
                    selected = if *ems == link.ems_instance_id {
                        " selected"
                    } else {
                        ""
                    }
                )
            })
            .collect();
        actions.push_str(
            "<button type=\"button\" data-dialog-open=\"connectivity-link\">Edit link</button>",
        );
        dialogs.push_str(&link_dialog(
            records,
            &link,
            accounts,
            token,
            LinkDialog {
                id: "connectivity-link",
                title: &format!("Link {instance} to an ems"),
                ems: &format!("<option value=\"\">None (unlink)</option>{options}"),
                from_ems: false,
            },
        ));
    }
    format!(
        "<h2>Its ems</h2><p class=\"hint\">A deployment admin makes a connectivity plugin \
         available; an ems's admin links it to their ems and chooses who may route through it. \
         It reaches the accounts that ems mapped on it, and nothing while withdrawn or linked to \
         none.</p><table class=\"list one-line connectivity\" data-connectivity=\"{id}\"><thead>\
         <tr><th>Available</th><th>Linked to</th><th>Who may route</th></tr></thead><tbody><tr>\
         <td data-available=\"{available}\">{available_words}</td><td>{ems}</td>\
         <td title=\"{users_title}\">{users}</td></tr></tbody></table>{actions}{dialogs}",
        available = link.available,
        available_words = if link.available { "Yes" } else { "No" },
        users_title = users,
        actions = if actions.is_empty() {
            String::new()
        } else {
            format!("<div class=\"actions\">{actions}</div>")
        },
    )
}

/// An ems's part: the connectivity plugins linked to it and its venue
/// account map. Its admin links one here, as W11.21 has it: each plugin
/// linked to it, to change who routes through it or to unlink it, and each
/// a deployment admin made available and nothing links yet.
fn ems_part(
    records: &AccessRecords,
    instance: &str,
    viewer: &Viewer,
    accounts: &[(String, String)],
    token: &str,
) -> String {
    let links: Vec<&ConnectivityLink> = records
        .connectivity_links
        .iter()
        .filter(|link| link.ems_instance_id == instance)
        .collect();
    let admin = (viewer.administers_ems)(instance);
    // Available and linked to no ems: its admin's to link (W11.21).
    let open: Vec<ConnectivityLink> = if admin {
        records
            .known_plugins
            .iter()
            .filter(|known| known.roles.iter().any(|role| role == "connectivity"))
            .map(|known| link_of(records, &known.plugin_instance_id))
            .filter(|link| link.available && link.ems_instance_id.is_empty())
            .collect()
    } else {
        Vec::new()
    };
    let ems = escape(instance);
    let mut dialogs = String::new();
    let mut action = |n: usize, link: &ConnectivityLink, linked: bool| -> String {
        if !admin {
            return String::new();
        }
        let id = format!("ems-link-{n}");
        let (label, title, options) = if linked {
            (
                "Edit",
                format!("{}'s link to {instance}", link.connectivity_instance_id),
                format!(
                    "<option value=\"{ems}\" selected>{ems}</option>\
                     <option value=\"\">None (unlink)</option>"
                ),
            )
        } else {
            (
                "Link",
                format!("Link {} to {instance}", link.connectivity_instance_id),
                format!("<option value=\"{ems}\" selected>{ems}</option>"),
            )
        };
        dialogs.push_str(&link_dialog(
            records,
            link,
            accounts,
            token,
            LinkDialog {
                id: &id,
                title: &title,
                ems: &options,
                from_ems: true,
            },
        ));
        format!(
            "<td class=\"actions\"><button type=\"button\" data-dialog-open=\"{id}\">{label}</button></td>"
        )
    };
    let account_name = |id: &str| {
        records
            .accounts
            .iter()
            .find(|account| account.account_id == id)
            .map(|account| account.name.clone())
            .unwrap_or_else(|| id.to_string())
    };
    let row = |link: &ConnectivityLink, users: String, action: String| {
        format!(
            "<tr data-connectivity=\"{c}\"><td><a href=\"{href}\">{c}</a></td><td>{available}</td>\
             <td>{users}</td>{action}</tr>",
            href = escape(&super::view::path(&link.connectivity_instance_id)),
            c = escape(&link.connectivity_instance_id),
            available = if link.available { "Yes" } else { "Withdrawn" },
        )
    };
    let mut link_rows = String::new();
    for (n, link) in links.iter().enumerate() {
        let act = action(n, link, true);
        link_rows.push_str(&row(link, link.users.len().to_string(), act));
    }
    for (n, link) in open.iter().enumerate() {
        let act = action(links.len() + n, link, false);
        link_rows.push_str(&row(link, "Not linked".into(), act));
    }
    let plugins = if link_rows.is_empty() {
        "<p class=\"empty\" data-linked=\"0\">No connectivity plugin is linked to it yet.</p>"
            .to_string()
    } else {
        format!(
            "<table class=\"list one-line\" data-linked=\"{n}\"><thead><tr><th>Connectivity plugin</th>\
             <th>Available</th><th>Users</th>{act}</tr></thead><tbody>{link_rows}</tbody></table>",
            n = links.len(),
            act = if admin { "<th class=\"actions\"></th>" } else { "" },
        )
    };
    let mappings: Vec<_> = records
        .venue_account_maps
        .iter()
        .filter(|map| map.ems_instance_id == instance)
        .flat_map(|map| map.mappings.iter())
        .collect();
    let map_rows: String = mappings
        .iter()
        .map(|mapping| {
            format!(
                "<tr><td>{c}</td><td>{code}</td><td title=\"{account}\">{account}</td></tr>",
                c = escape(&mapping.connectivity_instance_id),
                code = escape(&mapping.venue_account_code),
                account = escape(&account_name(&mapping.account_id)),
            )
        })
        .collect();
    let map = if map_rows.is_empty() {
        "<p class=\"empty\" data-mapped=\"0\">No account is mapped yet.</p>".to_string()
    } else {
        format!(
            "<om-pager search=\"Search the map\"><table class=\"list one-line\" data-mapped=\"{n}\">\
             <thead><tr><th>Connectivity plugin</th><th>Venue account code</th><th>Account</th></tr>\
             </thead><tbody>{map_rows}</tbody></table></om-pager>",
            n = mappings.len()
        )
    };
    // Its admin's Link and Edit say what they do in their dialogs: the part
    // keeps to one screen on a phone.
    format!(
        "<h2>Its connectivity plugins</h2><p class=\"hint\">The map is set on this ems's own \
         page.</p>{plugins}<h3>Venue account map</h3>{map}{dialogs}"
    )
}

type Pairs = Vec<(String, String)>;

fn field<'a>(fields: &'a super::Fields, name: &str) -> &'a str {
    fields.get(name).map(String::as_str).unwrap_or("")
}

/// `POST /admin/plugins/{instance}/connectivity/available`: a deployment
/// admin's (W11.21).
pub async fn available(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(instance): Path<String>,
    Form(pairs): Form<Pairs>,
) -> Response {
    let fields = super::once(&pairs);
    let back = super::view::tab_href(&instance, super::view::ACCESS);
    let (session, _) = match super::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if let Err(response) = super::form_token_matches(&session, &fields) {
        return *response;
    }
    let outcome = super::command::<ConnectivityLink>(
        &app,
        &session,
        SET_CONNECTIVITY_AVAILABLE,
        "meridian.v1.SetConnectivityAvailableRequest",
        SetConnectivityAvailableRequest {
            connectivity_instance_id: instance.clone(),
            available: field(&fields, "available") == "true",
            note: field(&fields, "note").to_string(),
        },
    )
    .await
    .map(|_| ());
    super::after_to(outcome, &format!("{back}#part-connectivity"), &back)
}

/// `POST /admin/plugins/{instance}/connectivity/link`: the `ems`'s admin's,
/// for an `ems` they administer -- the one it is linked to, to unlink it
/// (W11.21).
pub async fn link(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(instance): Path<String>,
    Form(pairs): Form<Pairs>,
) -> Response {
    let fields = super::once(&pairs);
    let back = super::view::tab_href(&instance, super::view::ACCESS);
    let (session, records) = match super::gate(&app, &headers, false) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if let Err(response) = super::form_token_matches(&session, &fields) {
        return *response;
    }
    let access = person_access(&records, &session.subject, &session.directory_groups);
    let ems = field(&fields, "ems_instance_id").to_string();
    let standing = link_of(&records, &instance).ems_instance_id;
    // An admin of the ems linked to, or of the one it is linked to now when
    // unlinking; an admin of another role is refused (W11.21).
    let deciding = if ems.is_empty() { &standing } else { &ems };
    // Opened on the ems's own page (W11.21), the change returns there.
    let back = if field(&fields, "back") == "ems" && !deciding.is_empty() {
        super::view::tab_href(deciding, super::view::ACCESS)
    } else {
        back
    };
    if deciding.is_empty() || !access.plugin(deciding).administers("ems") {
        return super::after_to(
            Err(format!(
                "ems_instance_id: linking {instance} is the ems's admin's, and you do not \
                 administer {} as an ems",
                if deciding.is_empty() {
                    "any ems"
                } else {
                    deciding.as_str()
                }
            )),
            &back,
            &back,
        );
    }
    let users: Vec<ConnectivityUser> = pairs
        .iter()
        .filter(|(key, _)| key == "user")
        .filter_map(|(_, value)| user_of(value))
        .collect();
    let outcome = super::command::<ConnectivityLink>(
        &app,
        &session,
        LINK_CONNECTIVITY,
        "meridian.v1.LinkConnectivityRequest",
        LinkConnectivityRequest {
            connectivity_instance_id: instance.clone(),
            ems_instance_id: ems,
            users,
            note: field(&fields, "note").to_string(),
        },
    )
    .await
    .map(|_| ());
    super::after_to(outcome, &format!("{back}#part-connectivity"), &back)
}

/// The connectivity plugins `subject` -- a person, or a service account by
/// its SA- ID -- in `user_group_ids` may route through from `instance`
/// (contract v22; W11.8, W11.21; the product owner, 2026-10-12): for an
/// ems, each linked to it, only while available, and only where its users
/// name the subject, the service account or one of the user groups; for
/// any other plugin none, a connectivity plugin creating no placement
/// (ruled 2026-10-13). What the claims carry as connectivity_instance_ids,
/// which the ems's sidecar holds a placement to and stamps
/// connectivity_allowed by.
pub fn routes(
    records: &AccessRecords,
    instance: &str,
    subject: &str,
    user_group_ids: &std::collections::BTreeSet<String>,
) -> Vec<String> {
    records
        .connectivity_links
        .iter()
        .filter(|link| link.available && !link.ems_instance_id.is_empty())
        .filter(|link| link.ems_instance_id == instance)
        .filter(|link| {
            link.users.iter().any(|user| match &user.user {
                Some(connectivity_user::User::Subject(named)) => named == subject,
                Some(connectivity_user::User::ServiceAccountId(named)) => named == subject,
                Some(connectivity_user::User::UserGroupId(named)) => user_group_ids.contains(named),
                None => false,
            })
        })
        .map(|link| link.connectivity_instance_id.clone())
        .collect()
}

/// The environments a connection is demo in (contract v26; W6.11's
/// role-common setting; G1 question 5).
pub const DEMO_ENVIRONMENTS: [&str; 2] = ["sandbox", "paper"];

/// A connectivity instance's environment setting, core's record of what a
/// person with admin on its role set (W6.11): empty where none is set.
pub fn environment_set(records: &AccessRecords, instance: &str) -> String {
    records
        .plugin_settings
        .iter()
        .find(|held| held.plugin_instance_id == instance)
        .and_then(|held| held.values.iter().find(|value| value.name == "environment"))
        .map(|value| value.value.trim().to_string())
        .unwrap_or_default()
}

/// Those of `routes` that are demo by their environment setting (contract
/// v26, W11.8): what the claims carry as demo_connectivity_instance_ids.
/// The session's key -- the venue catalogue's reported environment -- is
/// held by the ems's sidecar as it stamps a placement, so a connection
/// whose setting and session disagree, or whose session reports nothing,
/// is live (failing closed).
pub fn demo_routes(records: &AccessRecords, routes: &[String]) -> Vec<String> {
    routes
        .iter()
        .filter(|instance| DEMO_ENVIRONMENTS.contains(&environment_set(records, instance).as_str()))
        .cloned()
        .collect()
}

/// A connection's badge (W11.8): Demo where its environment setting is
/// sandbox or paper, Live otherwise.
pub fn badge(records: &AccessRecords, instance: &str) -> &'static str {
    if DEMO_ENVIRONMENTS.contains(&environment_set(records, instance).as_str()) {
        "Demo"
    } else {
        "Live"
    }
}

/// What `dashboard__read_plugin_access` answers of an instance's link and
/// map (W11.21, W11.22): its link, for a connectivity plugin; its plugins
/// and its map, for an `ems`; nothing for any other.
pub fn access_json(records: &AccessRecords, instance: &str) -> serde_json::Value {
    let user_json = |user: &ConnectivityUser| match &user.user {
        Some(connectivity_user::User::Subject(id)) => serde_json::json!({"subject": id}),
        Some(connectivity_user::User::ServiceAccountId(id)) => {
            serde_json::json!({"service_account_id": id})
        }
        Some(connectivity_user::User::UserGroupId(id)) => serde_json::json!({"user_group_id": id}),
        None => serde_json::Value::Null,
    };
    let link_json = |link: &ConnectivityLink| {
        serde_json::json!({
            "connectivity_instance_id": link.connectivity_instance_id,
            "available": link.available,
            "ems_instance_id": link.ems_instance_id,
            "users": link.users.iter().map(user_json).collect::<Vec<_>>(),
        })
    };
    let mut out = serde_json::Map::new();
    if holds(records, instance, "connectivity") {
        out.insert(
            "connectivity_link".into(),
            link_json(&link_of(records, instance)),
        );
    }
    if holds(records, instance, "ems") {
        out.insert(
            "connectivity_links".into(),
            records
                .connectivity_links
                .iter()
                .filter(|link| link.ems_instance_id == instance)
                .map(link_json)
                .collect::<Vec<_>>()
                .into(),
        );
        out.insert(
            "venue_account_map".into(),
            serde_json::json!({
                "ems_instance_id": instance,
                "mappings": records
                    .venue_account_maps
                    .iter()
                    .filter(|map| map.ems_instance_id == instance)
                    .flat_map(|map| map.mappings.iter())
                    .map(|mapping| serde_json::json!({
                        "connectivity_instance_id": mapping.connectivity_instance_id,
                        "venue_account_code": mapping.venue_account_code,
                        "account_id": mapping.account_id,
                    }))
                    .collect::<Vec<_>>(),
            }),
        );
    }
    serde_json::Value::Object(out)
}

#[cfg(test)]
mod tests {
    use meridian_domain::v1::{AccountRecord, KnownPluginRoles, SignInRecord, UserGroup};
    use meridian_pb::v1::{VenueAccountMap, VenueAccountMapping};

    use super::*;

    fn records() -> AccessRecords {
        AccessRecords {
            known_plugins: vec![
                KnownPluginRoles {
                    plugin_instance_id: "fix-1".into(),
                    roles: vec!["connectivity".into()],
                },
                KnownPluginRoles {
                    plugin_instance_id: "ems-1".into(),
                    roles: vec!["ems".into()],
                },
            ],
            accounts: vec![AccountRecord {
                account_id: "ACC-1".into(),
                name: "Desk <one>".into(),
                ..Default::default()
            }],
            user_groups: vec![UserGroup {
                user_group_id: "UG-1".into(),
                name: "Traders".into(),
                ..Default::default()
            }],
            people: vec![SignInRecord {
                subject: "local|ada".into(),
                display_name: "Ada".into(),
                ..Default::default()
            }],
            connectivity_links: vec![ConnectivityLink {
                connectivity_instance_id: "fix-1".into(),
                available: true,
                ems_instance_id: "ems-1".into(),
                users: vec![ConnectivityUser {
                    user: Some(connectivity_user::User::UserGroupId("UG-1".into())),
                }],
            }],
            venue_account_maps: vec![VenueAccountMap {
                ems_instance_id: "ems-1".into(),
                mappings: vec![VenueAccountMapping {
                    connectivity_instance_id: "fix-1".into(),
                    venue_account_code: "ACCT-7".into(),
                    account_id: "ACC-1".into(),
                }],
            }],
            ..Default::default()
        }
    }

    #[test]
    fn a_connectivity_plugin_shows_its_link_and_offers_its_changes_to_whom_they_are() {
        let records = records();
        let admin = |_: &str| false;
        let deployment = Viewer {
            deployment_admin: true,
            administers_ems: &admin,
        };
        let page = part(&records, "fix-1", &deployment, &[], "");
        assert!(page.contains("data-available=\"true\""));
        assert!(page.contains("Traders (user group)"));
        assert!(page.contains("connectivity-available") && page.contains("Withdraw"));
        assert!(
            !page.contains("connectivity-link\""),
            "a deployment admin links nothing"
        );
        let ems_admin = |ems: &str| ems == "ems-1";
        let desk = Viewer {
            deployment_admin: false,
            administers_ems: &ems_admin,
        };
        let page = part(&records, "fix-1", &desk, &[], "");
        assert!(page.contains("/admin/plugins/fix-1/connectivity/link"));
        assert!(page.contains("value=\"group:UG-1\" checked"));
        assert!(page.contains("value=\"person:local|ada\""));
        assert!(!page.contains("connectivity-available\""));
    }

    #[test]
    fn an_ems_shows_its_plugins_and_its_map_read_only() {
        let records = records();
        let none = |_: &str| false;
        let page = part(
            &records,
            "ems-1",
            &Viewer {
                deployment_admin: true,
                administers_ems: &none,
            },
            &[],
            "",
        );
        assert!(page.contains("data-linked=\"1\"") && page.contains("data-mapped=\"1\""));
        assert!(page.contains("Desk &lt;one&gt;"));
        assert!(
            !page.contains("<form"),
            "the map is set on the ems's own page"
        );
        let answered = access_json(&records, "ems-1");
        assert_eq!(
            answered["venue_account_map"]["mappings"][0]["venue_account_code"],
            "ACCT-7"
        );
        assert_eq!(
            answered["connectivity_links"][0]["users"][0]["user_group_id"],
            "UG-1"
        );
        assert_eq!(
            access_json(&records, "fix-1")["connectivity_link"]["ems_instance_id"],
            "ems-1"
        );
        assert!(access_json(&records, "other-1")
            .as_object()
            .unwrap()
            .is_empty());
        assert!(user_of("group:").is_none() && user_of("planet:x").is_none());
    }

    #[test]
    fn an_ems_admin_links_a_connectivity_plugin_on_the_ems_page() {
        // W11.21: the ems's admin links, without holding the connectivity
        // plugin's admin; a deployment admin's availability bounds it.
        let mut records = records();
        for (instance, available) in [("fix-2", true), ("fix-3", false)] {
            records.known_plugins.push(KnownPluginRoles {
                plugin_instance_id: instance.into(),
                roles: vec!["connectivity".into()],
            });
            records.connectivity_links.push(ConnectivityLink {
                connectivity_instance_id: instance.into(),
                available,
                ..Default::default()
            });
        }
        let ems_admin = |ems: &str| ems == "ems-1";
        let desk = Viewer {
            deployment_admin: false,
            administers_ems: &ems_admin,
        };
        let page = part(&records, "ems-1", &desk, &[], "<token>");
        // Its linked plugin: changed or unlinked, its users chosen, back here.
        assert!(page.contains("data-linked=\"1\""));
        assert!(page.contains("action=\"/admin/plugins/fix-1/connectivity/link\""));
        assert!(page.contains("value=\"group:UG-1\" checked"));
        assert!(page.contains("None (unlink)"));
        assert!(page.contains("name=\"back\" value=\"ems\""));
        // The available one nothing links: offered; the withdrawn one: not.
        assert!(page.contains("action=\"/admin/plugins/fix-2/connectivity/link\""));
        assert!(page.contains("Not linked"));
        assert!(!page.contains("fix-3"));
        // Another ems's admin, or a deployment admin alone, links nothing here.
        let other = |ems: &str| ems == "ems-2";
        let page = part(
            &records,
            "ems-1",
            &Viewer {
                deployment_admin: true,
                administers_ems: &other,
            },
            &[],
            "",
        );
        assert!(!page.contains("<form") && !page.contains("fix-2"));
    }

    #[test]
    fn the_connectivity_plugins_a_caller_may_route_through_are_the_links_that_name_them() {
        // The product owner, 2026-10-12: option (d).
        let user = |user: connectivity_user::User| ConnectivityUser { user: Some(user) };
        let link =
            |instance: &str, available: bool, users: Vec<ConnectivityUser>| ConnectivityLink {
                connectivity_instance_id: instance.into(),
                available,
                ems_instance_id: "ems-1".into(),
                users,
            };
        let mut records = records();
        records.connectivity_links = vec![
            link(
                "fix-1",
                true,
                vec![user(connectivity_user::User::Subject("local|ada".into()))],
            ),
            link(
                "fix-2",
                true,
                vec![user(connectivity_user::User::ServiceAccountId(
                    "SA-7".into(),
                ))],
            ),
            link(
                "fix-3",
                true,
                vec![user(connectivity_user::User::UserGroupId("UG-1".into()))],
            ),
            link(
                "fix-4",
                false,
                vec![user(connectivity_user::User::Subject("local|ada".into()))],
            ),
            ConnectivityLink {
                ems_instance_id: "ems-2".into(),
                ..link(
                    "fix-5",
                    true,
                    vec![user(connectivity_user::User::Subject("local|ada".into()))],
                )
            },
        ];
        let groups = |ids: &[&str]| ids.iter().map(|id| id.to_string()).collect();
        // A person named, and a member of a user group named; a withdrawn link
        // and another ems's link refused.
        assert_eq!(
            routes(&records, "ems-1", "local|ada", &groups(&["UG-1"])),
            vec!["fix-1", "fix-3"]
        );
        // A service account named.
        assert_eq!(
            routes(&records, "ems-1", "SA-7", &groups(&[])),
            vec!["fix-2"]
        );
        // A non-member named nowhere: none.
        assert!(routes(&records, "ems-1", "local|ben", &groups(&["UG-2"])).is_empty());
        // A connectivity plugin routes through nothing, itself included: it
        // creates no placement (ruled 2026-10-13).
        assert!(routes(&records, "fix-1", "local|ada", &groups(&[])).is_empty());
        assert!(routes(&records, "fix-4", "local|ada", &groups(&[])).is_empty());
    }
}
