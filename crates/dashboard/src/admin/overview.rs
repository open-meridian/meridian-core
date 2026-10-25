//! The administration page: every record a deployment admin authors, a tab
//! each, named rather than numbered.
//!
//! What it posts is unchanged -- the same routes and the same fields, which
//! the contract's fixtures and the cluster runs hold -- and what changed is
//! how it asks. Nobody types an identifier: a record is edited from its own
//! row, in a dialog that already holds it, and a permission is granted by
//! choosing among the groups that exist. Identifiers are still shown, small,
//! beside the names, and every row carries `data-id` and `data-name`, which is
//! what the cluster runs read rather than the table's layout.
//!
//! Every form posts to its tab's anchor, `/admin/accounts#accounts`: a
//! redirect without a fragment keeps the one it was reached by, so after a
//! change the page opens on the tab it was made from.

use std::collections::HashMap;

use meridian_access::{AccessLevel, ALL_PLUGINS_ADMIN, DEPLOYMENT_ADMIN};
use meridian_domain::v1::{AccessEntry, AccessRecords, AccountRecord, AccountState};
use meridian_pb::bounds::{ACCOUNT_RECORD_CUSTODIAN_LENGTH, ACCOUNT_RECORD_NOTE_LENGTH};

use std::collections::HashSet;

use crate::html::escape;

use super::people::Person;
use super::picker::{self, Choice};
use super::view::{self, Line};

fn level_name(level: i32) -> &'static str {
    match AccessLevel::try_from(level) {
        Ok(AccessLevel::Write) => "write",
        Ok(AccessLevel::Admin) => "admin",
        _ => "read",
    }
}

/// What an access group gives one plugin's role, as the form's one choice
/// names it: `read`, `write` or `admin`, or `admin-read` or `admin-write` for
/// admin beside a data level (W6.7; per role from v15).
pub(crate) fn choice_of(entries: &[AccessEntry], plugin: &str, role: &str) -> &'static str {
    let on: Vec<i32> = entries
        .iter()
        .filter(|e| e.plugin_instance_id == plugin && e.role == role)
        .map(|e| e.level)
        .collect();
    let admin = on.contains(&(AccessLevel::Admin as i32));
    let data = if on.contains(&(AccessLevel::Write as i32)) {
        Some("write")
    } else if on.contains(&(AccessLevel::Read as i32)) {
        Some("read")
    } else {
        None
    };
    match (admin, data) {
        (true, Some("write")) => "admin-write",
        (true, Some(_)) => "admin-read",
        (true, None) => "admin",
        (false, Some("write")) => "write",
        _ => "read",
    }
}

/// A name, and its identifier small beside it.
fn named(name: &str, id: &str) -> String {
    format!(
        "<span class=\"name\">{}</span><span class=\"id\">{}</span>",
        escape(name),
        escape(id)
    )
}

fn section(id: &str, title: &str, about: &str, action: &str, body: String) -> String {
    format!(
        "<section class=\"admin-section\" id=\"{id}\">\
         <div class=\"section-head\"><div><h2>{title}</h2><p class=\"hint\">{about}</p></div>{action}</div>\
         {body}</section>"
    )
}

/// What opens a section's dialog for a new record (the product owner,
/// 2026-09-30: "+ Add" on every section). Named for a screen reader by
/// what it adds, which the words it shows begin; the dialog it opens says
/// the whole of it in its heading.
fn add_button(dialog: &str, what: &str) -> String {
    format!(
        "<button type=\"button\" class=\"primary\" data-dialog-open=\"{dialog}\" \
         aria-label=\"Add {what}\">+ Add</button>"
    )
}

/// An Edit that opens `dialog` holding this record: `fill` says what its
/// fields hold (`fields`, by name) and which boxes are ticked (`checked`, by
/// name), and `title` heads it.
fn edit_button(dialog: &str, title: &str, fill: &serde_json::Value) -> String {
    format!(
        "<button type=\"button\" data-dialog-open=\"{dialog}\" data-title=\"{}\" data-fill=\"{}\">Edit</button>",
        escape(title),
        escape(&fill.to_string())
    )
}

/// One dialog per kind of record, for a new one and for each edit, which the
/// page's script fills from the Edit pressed. Its heading and its button say
/// which, from `data-title-new` and `data-label-new`, and the heading names
/// the dialog: "New access group", however short the button that opened it.
fn dialog(id: &str, title: &str, action: &str, token: &str, fields: &str, submit: &str) -> String {
    format!(
        "<dialog id=\"{id}\" aria-labelledby=\"{id}-title\"><form method=\"post\" action=\"{action}\">{token}\
         <div class=\"dialog-head\"><h2 id=\"{id}-title\" data-title-new=\"{title}\">{title}</h2></div>\
         <div class=\"dialog-body\">{fields}</div>\
         <div class=\"dialog-foot\"><button type=\"button\" data-dialog-close>Cancel</button>\
         <button type=\"submit\" class=\"primary\" data-label-new=\"{submit}\">{submit}</button></div></form></dialog>"
    )
}

fn options(chosen: &str, items: &[(String, String)]) -> String {
    items
        .iter()
        .map(|(value, label)| {
            format!(
                "<option value=\"{v}\"{s}>{l}</option>",
                v = escape(value),
                s = if value == chosen { " selected" } else { "" },
                l = escape(label)
            )
        })
        .collect()
}

/// A table of `noun` (the product owner, 2026-09-30: "assuming 100+ items"):
/// a search box narrowing it in the browser with a count of what is shown,
/// headings that sort it, and what it says when a search matches none. The
/// rows come sorted from here, so without script it is in that order, whole.
pub(crate) fn listing(
    id: &str,
    class: &str,
    noun: &str,
    search: &str,
    head: &str,
    rows: &str,
) -> String {
    format!(
        "<div class=\"filter-row\"><input class=\"filter\" type=\"search\" data-filter=\"{id}\" hidden \
         placeholder=\"{search}\" aria-label=\"Search {noun}\">\
         <span class=\"filter-count\" data-filter-count=\"{id}\" aria-live=\"polite\" hidden></span></div>\
         <om-pager><div class=\"scroll\"><table class=\"list{class}\" id=\"{id}\" data-sortable><thead><tr>{head}</tr></thead>\
         <tbody>{rows}</tbody></table></div></om-pager>\
         <p class=\"empty\" data-filter-none=\"{id}\" hidden>No {noun} match that search.</p>"
    )
}

/// The first few of `items`, and how many more: a cell stays a line however
/// big the group. The rest are in the cell, hidden, so a search finds a row
/// by any of them.
fn summary(items: &[String], most: usize) -> String {
    let shown: Vec<String> = items.iter().take(most).map(|i| escape(i)).collect();
    if items.len() <= most {
        return shown.join(", ");
    }
    let rest: Vec<String> = items.iter().skip(most).map(|i| escape(i)).collect();
    format!(
        "{}<span class=\"more\"> and {} more</span><span hidden>, {}</span>",
        shown.join(", "),
        items.len() - most,
        rest.join(", ")
    )
}

/// How many of a group's members a row shows before "and N more".
const SHOWN_MEMBERS: usize = 3;

/// Names in the order a person reads a list: by name ignoring case, then by
/// identifier.
fn by_name<'a>(name: &'a str, id: &'a str) -> (String, &'a str) {
    (name.to_lowercase(), id)
}

#[allow(clippy::too_many_arguments)]
pub fn render(
    records: &AccessRecords,
    delegating: &[(String, String, usize)],
    plugins: &[Line],
    people: &[Person],
    books: &super::books::Books,
    agents: (String, String),
    service_accounts: (String, String),
    time_zone: Option<&meridian_domain::v1::DeploymentTimeZoneSetting>,
    fx_pivots: (
        Option<&meridian_domain::v1::FxPivotCurrenciesSetting>,
        &std::collections::BTreeMap<String, String>,
    ),
    (series_rule, stale_reference): (
        Option<&meridian_domain::v1::SeriesRequestRule>,
        Option<&meridian_domain::v1::StaleReferenceActionsPermittedSetting>,
    ),
    routing: (
        Option<&meridian_domain::v1::ListModelRoutingReply>,
        Option<&meridian_domain::v1::ListMeasureDefinitionsReply>,
    ),
    token: &str,
    notice: &str,
) -> String {
    let user_group_names: HashMap<&str, &str> = records
        .user_groups
        .iter()
        .map(|g| (g.user_group_id.as_str(), g.name.as_str()))
        .collect();
    let account_group_names: HashMap<&str, &str> = records
        .account_groups
        .iter()
        .map(|g| (g.account_group_id.as_str(), g.name.as_str()))
        .collect();
    let access_group_names: HashMap<&str, &str> = records
        .access_groups
        .iter()
        .map(|g| (g.access_group_id.as_str(), g.name.as_str()))
        .collect();
    let account_names: HashMap<&str, &str> = records
        .accounts
        .iter()
        .map(|a| (a.account_id.as_str(), a.name.as_str()))
        .collect();
    let plugin_names: HashMap<&str, &str> = plugins
        .iter()
        .filter_map(|line| {
            line.name
                .as_deref()
                .map(|name| (line.instance.as_str(), name))
        })
        .collect();
    let name_of = |names: &HashMap<&str, &str>, id: &str| -> String {
        names
            .get(id)
            .map(|n| n.to_string())
            .unwrap_or_else(|| id.to_string())
    };

    let mut sections = Vec::new();

    // ── Plugins ─────────────────────────────────────────────────────────────
    // W6.10, and the way to each instance's admin view: its health, what its
    // settings still need, and its external accounts nothing links.
    let body = if plugins.is_empty() {
        "<p class=\"empty\">No plugin has reported or been launched yet.</p>".to_string()
    } else {
        let mut sorted: Vec<&Line> = plugins.iter().collect();
        sorted.sort_by(|a, b| {
            by_name(a.name.as_deref().unwrap_or(&a.instance), &a.instance).cmp(&by_name(
                b.name.as_deref().unwrap_or(&b.instance),
                &b.instance,
            ))
        });
        let rows: String = sorted
            .iter()
            .enumerate()
            .map(|(row, line)| {
                // Why it is as it is, the badge's note on hover (the product
                // owner, 2026-09-30), a line under it without script.
                let (state, detail) =
                    view::state_badge(&line.state, &format!("plugin-health-{row}"));
                let settings = if line.missing.is_empty() {
                    "<span class=\"badge good\">ready</span>".to_string()
                } else {
                    format!(
                        "<span class=\"badge warn\">needs {}</span>",
                        escape(&line.missing.join(", "))
                    )
                };
                format!(
                    "<tr data-id=\"{id}\"><td>{plugin}</td><td><code>{id}</code></td>\
                     <td>{state}{detail}</td>\
                     <td>{settings}</td><td class=\"flags\">{flags}</td>\
                     <td class=\"actions\"><a class=\"button\" href=\"{href}\">Manage</a></td></tr>",
                    id = escape(&line.instance),
                    plugin = match &line.name {
                        Some(name) => format!("<span class=\"name\">{}</span>", escape(name)),
                        None => "<span class=\"faint\">not from the catalogue</span>".to_string(),
                    },
                    flags = view::flags(line, None),
                    href = escape(&view::path(&line.instance)),
                )
            })
            .collect();
        listing(
            "plugins-table",
            " plugins",
            "plugins",
            "Search by plugin, instance, health or what it needs",
            "<th>Plugin</th><th>Instance</th><th>Health</th><th>Settings</th><th>Needs you</th><th></th>",
            &rows,
        )
    };
    sections.push(section(
        "plugins",
        "Plugins",
        "Every plugin instance in this deployment: its health, and what it needs of you. \
         Manage one for its settings, who has access and its own admin page.",
        "",
        body,
    ));

    // ── Permissions ─────────────────────────────────────────────────────────
    let mut permissions: Vec<_> = records.permissions.iter().collect();
    permissions.sort_by_key(|p| {
        (
            name_of(&user_group_names, &p.user_group_id).to_lowercase(),
            name_of(&account_group_names, &p.account_group_id).to_lowercase(),
            name_of(&access_group_names, &p.access_group_id).to_lowercase(),
            p.permission_id.clone(),
        )
    });
    let mut rows = String::new();
    for p in permissions {
        // A permission to a built-in access group, or to one giving only
        // admin, names no account group: it reaches no account (W6.8).
        let accounts = if p.account_group_id.is_empty() {
            "<span class=\"name\">no account</span>".to_string()
        } else {
            named(
                &name_of(&account_group_names, &p.account_group_id),
                &p.account_group_id,
            )
        };
        rows.push_str(&format!(
            "<tr data-id=\"{id}\" data-user-group=\"{ug}\" data-account-group=\"{ag}\" data-access-group=\"{acg}\">\
             <td>{user}</td><td>{accounts}</td><td>{access}</td><td class=\"actions\">\
             <form method=\"post\" action=\"/admin/permissions/withdraw#permissions\" \
             data-confirm=\"Withdraw this permission? The people in the user group lose what it gave them.\">{token}\
             <input type=\"hidden\" name=\"permission_id\" value=\"{id}\"><button type=\"submit\">Withdraw</button></form></td></tr>",
            id = escape(&p.permission_id),
            ug = escape(&p.user_group_id),
            ag = escape(&p.account_group_id),
            acg = escape(&p.access_group_id),
            user = named(&name_of(&user_group_names, &p.user_group_id), &p.user_group_id),
            access = named(&name_of(&access_group_names, &p.access_group_id), &p.access_group_id),
        ));
    }
    let table = if rows.is_empty() {
        "<p class=\"empty\">No permissions yet.</p>".to_string()
    } else {
        listing(
            "permissions-table",
            "",
            "permissions",
            "Search by user group, account group or access group",
            "<th>User group</th><th>On accounts</th><th>Access</th><th></th>",
            &rows,
        )
    };
    let mut user_groups: Vec<(String, String)> = records
        .user_groups
        .iter()
        .map(|g| (g.user_group_id.clone(), g.name.clone()))
        .collect();
    user_groups.sort_by(|a, b| by_name(&a.1, &a.0).cmp(&by_name(&b.1, &b.0)));
    let mut every_group: Vec<(String, String)> = records
        .account_groups
        .iter()
        .map(|g| (g.account_group_id.clone(), g.name.clone()))
        .collect();
    every_group.sort_by(|a, b| by_name(&a.1, &a.0).cmp(&by_name(&b.1, &b.0)));
    let mut account_groups = vec![(
        String::new(),
        "None (admin only, and the built-in access groups)".to_string(),
    )];
    account_groups.extend(every_group);
    let mut access_groups: Vec<(String, String)> = records
        .access_groups
        .iter()
        .map(|g| (g.access_group_id.clone(), g.name.clone()))
        .collect();
    access_groups.sort_by(|a, b| by_name(&a.1, &a.0).cmp(&by_name(&b.1, &b.0)));
    let grant = dialog(
        "new-permission",
        "Grant a permission",
        "/admin/permissions#permissions",
        token,
        &format!(
            "<label>User group<select name=\"user_group_id\" required>{}</select></label>\
             <label>On accounts<select name=\"account_group_id\">{}</select></label>\
             <label>Access<select name=\"access_group_id\" required>{}</select></label>\
             <p class=\"hint\">Deployment admin, All plugins (admin) and an access group \
             giving only admin are granted on no account group: configuring is not an act on an \
             account. Any other access group is granted on one, All accounts among them.</p>",
            options("", &user_groups),
            options("", &account_groups),
            options("", &access_groups),
        ),
        "Grant",
    );
    sections.push(section(
        "permissions",
        "Permissions",
        "Who may do what: the people in a user group, using an access group's plugins, \
         on an account group's accounts.",
        &add_button("new-permission", "a permission"),
        format!("{table}{grant}"),
    ));

    // ── User groups ─────────────────────────────────────────────────────────
    // The three groups' sections are headed User, Account and Access (the
    // product owner, 2026-09-30), "groups" understood from the tab each is
    // reached by; a dialog still says "New user group" in full.
    // People are chosen by their user ID, then their login ID
    // (super::people); one typed in is taken as it always was.
    let person_of: HashMap<&str, &Person> = people.iter().map(|p| (p.login.as_str(), p)).collect();
    let people_choices: Vec<Choice> = people
        .iter()
        .map(|person| Choice {
            value: person.login.clone(),
            label: if person.name.is_empty() {
                person.user_id.clone()
            } else {
                format!("{} ({})", person.user_id, person.name)
            },
            detail: person.login.clone(),
            ..Default::default()
        })
        .collect();
    let user_group_fields = format!(
        "<input type=\"hidden\" name=\"user_group_id\" value=\"\" data-record-id>\
         <label>Name<input name=\"name\" value=\"\" required></label>\
         {picker}\
         <label>Other logins, one per line<textarea name=\"logins\" rows=\"2\"></textarea></label>\
         <label>Directory groups, one per line<textarea name=\"directory_groups\" rows=\"3\"></textarea></label>\
         <p class=\"hint\">Somebody is in the group when their directory says they are in \
         one of its groups, or when their login is chosen or listed. People are listed by user ID, \
         then login ID; somebody not listed yet is added by their login, <code>local|name</code> \
         for an account this deployment holds.</p>",
        picker = if people_choices.is_empty() {
            String::new()
        } else {
            picker::many(
                "user-group-people",
                "login",
                "People",
                "people",
                &people_choices,
                &HashSet::new(),
            )
        },
    );
    let mut groups: Vec<_> = records.user_groups.iter().collect();
    groups.sort_by(|a, b| {
        by_name(&a.name, &a.user_group_id).cmp(&by_name(&b.name, &b.user_group_id))
    });
    let mut rows = String::new();
    for g in groups {
        let mut members: Vec<Person> = g
            .logins
            .iter()
            .map(|login| {
                person_of
                    .get(login.as_str())
                    .map(|p| (*p).clone())
                    .unwrap_or(Person {
                        user_id: super::people::user_id(login),
                        login: login.clone(),
                        name: String::new(),
                    })
            })
            .collect();
        super::people::sort(&mut members);
        let shown: Vec<String> = members.iter().map(|p| p.user_id.clone()).collect();
        let searched: Vec<String> = members
            .iter()
            .map(|p| format!("{} {}", p.login, p.name))
            .collect();
        let fill = serde_json::json!({
            "fields": {
                "user_group_id": g.user_group_id,
                "name": g.name,
                "directory_groups": g.directory_groups.join("\n"),
            },
            "checked": { "login": g.logins },
        });
        rows.push_str(&format!(
            "<tr data-id=\"{id}\" data-name=\"{name}\"><td>{named}</td><td>{dirs}</td>\
             <td>{logins}<span hidden> {searched}</span></td>\
             <td class=\"actions\">{edit}</td></tr>",
            id = escape(&g.user_group_id),
            name = escape(&g.name),
            named = named(&g.name, &g.user_group_id),
            dirs = summary(&g.directory_groups, SHOWN_MEMBERS),
            logins = summary(&shown, SHOWN_MEMBERS),
            searched = escape(&searched.join(" ")),
            edit = edit_button("user-group", &format!("Edit {}", g.name), &fill),
        ));
    }
    let dialogs = dialog(
        "user-group",
        "New user group",
        "/admin/user-groups#user-groups",
        token,
        &user_group_fields,
        "Create",
    );
    let table = if rows.is_empty() {
        "<p class=\"empty\">No user groups yet.</p>".to_string()
    } else {
        listing(
            "user-groups-table",
            "",
            "user groups",
            "Search by name, directory group, user ID or login",
            "<th>Name</th><th>Directory groups</th><th>People</th><th></th>",
            &rows,
        )
    };
    sections.push(section(
        "user-groups",
        "User",
        "People, by the directory groups they are in or by their logins.",
        &add_button("user-group", "a user group"),
        format!("{table}{dialogs}"),
    ));

    // ── Account groups ──────────────────────────────────────────────────────
    // Every account is an option once; a closed one is listed only where a
    // group holds it already, and an account a group names that is not
    // known here is listed by its identifier, so an edit never drops it.
    let mut accounts_sorted: Vec<&AccountRecord> = records.accounts.iter().collect();
    accounts_sorted
        .sort_by(|a, b| by_name(&a.name, &a.account_id).cmp(&by_name(&b.name, &b.account_id)));
    let mut account_choices: Vec<Choice> = accounts_sorted
        .iter()
        .map(|a| {
            let closed = a.state == AccountState::Closed as i32;
            Choice {
                value: a.account_id.clone(),
                label: if closed {
                    format!("{} (closed)", a.name)
                } else {
                    a.name.clone()
                },
                detail: a.account_id.clone(),
                also: [a.custodian.as_str(), &a.account_type, &a.owner].join(" "),
                only_when_chosen: closed,
                ..Default::default()
            }
        })
        .collect();
    let mut unknown: Vec<&str> = records
        .account_groups
        .iter()
        .flat_map(|g| g.account_ids.iter().map(String::as_str))
        .filter(|id| !account_names.contains_key(id))
        .collect();
    unknown.sort_unstable();
    unknown.dedup();
    account_choices.extend(unknown.into_iter().map(|id| Choice {
        value: id.to_string(),
        label: id.to_string(),
        only_when_chosen: true,
        ..Default::default()
    }));
    let account_group_fields = format!(
        "<input type=\"hidden\" name=\"account_group_id\" value=\"\" data-record-id>\
         <label>Name<input name=\"name\" value=\"\" required></label>{}",
        if records.accounts.is_empty() {
            "<p class=\"hint\">No accounts yet: define one under Accounts.</p>".to_string()
        } else {
            picker::many(
                "account-group-accounts",
                "account_ids",
                "Accounts",
                "accounts",
                &account_choices,
                &HashSet::new(),
            )
        }
    );
    let mut groups: Vec<_> = records.account_groups.iter().collect();
    groups.sort_by(|a, b| {
        by_name(&a.name, &a.account_group_id).cmp(&by_name(&b.name, &b.account_group_id))
    });
    let mut rows = String::new();
    for g in groups {
        // All accounts lists none of its own: it holds every account (W6.6).
        let (members, edit) = if g.built_in {
            (
                "every account, those opened later included".to_string(),
                "<span class=\"pill\">built in</span>".to_string(),
            )
        } else {
            let mut members: Vec<String> = g
                .account_ids
                .iter()
                .map(|id| name_of(&account_names, id))
                .collect();
            members.sort_by_key(|m| m.to_lowercase());
            let fill = serde_json::json!({
                "fields": { "account_group_id": g.account_group_id, "name": g.name },
                "checked": { "account_ids": g.account_ids },
            });
            (
                summary(&members, SHOWN_MEMBERS),
                edit_button("account-group", &format!("Edit {}", g.name), &fill),
            )
        };
        rows.push_str(&format!(
            "<tr data-id=\"{id}\" data-name=\"{name}\"><td>{named}</td><td>{members}</td>\
             <td class=\"actions\">{edit}</td></tr>",
            id = escape(&g.account_group_id),
            name = escape(&g.name),
            named = named(&g.name, &g.account_group_id),
        ));
    }
    let dialogs = dialog(
        "account-group",
        "New account group",
        "/admin/account-groups#account-groups",
        token,
        &account_group_fields,
        "Create",
    );
    let table = if rows.is_empty() {
        "<p class=\"empty\">No account groups yet.</p>".to_string()
    } else {
        listing(
            "account-groups-table",
            "",
            "account groups",
            "Search by name or account",
            "<th>Name</th><th>Accounts</th><th></th>",
            &rows,
        )
    };
    sections.push(section(
        "account-groups",
        "Account",
        "Accounts gathered, so a permission can name them together.",
        &add_button("account-group", "an account group"),
        format!("{table}{dialogs}"),
    ));

    // ── Access groups ───────────────────────────────────────────────────────
    // Plugins, each at exactly one level (the product owner, 2026-09-30:
    // write includes read): every plugin known here, and any an access group
    // names that is not, each with its level beside it.
    let mut instances: Vec<(String, String)> = plugins
        .iter()
        .map(|line| (line.instance.clone(), line.name.clone().unwrap_or_default()))
        .collect();
    // And every plugin a sidecar has reported (W6.1, contract v15), whether
    // or not it runs now.
    for known in &records.known_plugins {
        if !instances
            .iter()
            .any(|(id, _)| id == &known.plugin_instance_id)
        {
            instances.push((known.plugin_instance_id.clone(), String::new()));
        }
    }
    for g in &records.access_groups {
        for e in &g.entries {
            if !instances.iter().any(|(id, _)| id == &e.plugin_instance_id) {
                instances.push((e.plugin_instance_id.clone(), String::new()));
            }
        }
    }
    instances.sort_by(|a, b| {
        let name = |(id, name): &(String, String)| {
            if name.is_empty() {
                id.clone()
            } else {
                name.clone()
            }
        };
        by_name(&name(a), &a.0).cmp(&by_name(&name(b), &b.0))
    });
    // One row per role each plugin holds (W6.7, contract v15; the product
    // owner, 2026-10-04: one line a row, paged, never scrolled): a plugin
    // holding one role shows one row with its role filled, one holding none
    // one row with no role; and a row for any entry an access group keeps on a
    // role the plugin no longer holds, flagged, so editing keeps it as written.
    let mut editor_rows: Vec<(String, String, String, bool)> = Vec::new();
    for (id, name) in &instances {
        let roles = known_roles(records, id);
        let shown = if name.is_empty() {
            id.clone()
        } else {
            name.clone()
        };
        if roles.is_empty() {
            editor_rows.push((id.clone(), shown.clone(), String::new(), false));
        }
        for role in &roles {
            editor_rows.push((id.clone(), shown.clone(), role.clone(), false));
        }
        for g in &records.access_groups {
            for e in g.entries.iter().filter(|e| &e.plugin_instance_id == id) {
                let stale = !meridian_access::entry_holds(records, id, &e.role);
                let listed = editor_rows
                    .iter()
                    .any(|(i, _, r, _)| i == id && *r == e.role);
                if stale && !listed {
                    editor_rows.push((id.clone(), shown.clone(), e.role.clone(), true));
                }
            }
        }
    }
    let editor_lines: String = editor_rows
        .iter()
        .map(|(id, name, role, stale)| {
            let role_cell = match (role.is_empty(), stale) {
                (true, false) => "<span class=\"id\">no role</span>".to_string(),
                (false, false) => escape(role),
                (_, true) => format!(
                    "{} <span class=\"flag warn\" title=\"{}\">not held now</span>",
                    if role.is_empty() { "no role".to_string() } else { escape(role) },
                    escape(&roles_said(records, id)),
                ),
            };
            format!(
                "<tr data-instance=\"{i}\" data-role=\"{r}\"{st}><td>{named}</td><td>{role_cell}</td>\
                 <td>{select}</td></tr>",
                i = escape(id),
                r = escape(role),
                st = if *stale { " data-stale" } else { "" },
                named = named(name, if name == id { "" } else { id }),
                select = level_select(id, role),
            )
        })
        .collect();
    let access_group_fields = format!(
        "<input type=\"hidden\" name=\"access_group_id\" value=\"\" data-record-id>\
         <label>Name<input name=\"name\" value=\"\" required></label>{}\
         <p class=\"hint\">Each plugin's role is given admin, to configure that role's side and \
         reach no account, and at most one data level: read to see what it shows, write to act \
         through it too, which includes read. A grant on one role reaches nothing of another.</p>",
        if editor_rows.is_empty() {
            "<p class=\"hint\">No plugins yet: launch one from the catalogue.</p>".to_string()
        } else {
            format!(
                "<fieldset class=\"access-editor\"><legend>Plugins, a row for each role</legend>\
                 <om-pager rows=\"6\"><table class=\"list one-line access-roles\" id=\"access-roles\">\
                 <thead><tr><th>Plugin</th><th>Role</th><th>Level</th></tr></thead>\
                 <tbody>{editor_lines}</tbody></table></om-pager></fieldset>"
            )
        }
    );
    let mut groups: Vec<_> = records.access_groups.iter().collect();
    // The built-in first, then by name.
    groups.sort_by(|a, b| {
        (!a.built_in, a.name.to_lowercase(), &a.access_group_id).cmp(&(
            !b.built_in,
            b.name.to_lowercase(),
            &b.access_group_id,
        ))
    });
    let mut rows = String::new();
    for g in groups {
        let entries: Vec<String> = if g.access_group_id == DEPLOYMENT_ADMIN {
            vec!["the dashboard's own capabilities; no plugin and no account".to_string()]
        } else if g.access_group_id == ALL_PLUGINS_ADMIN {
            vec!["admin on every plugin, those launched later included; no account".to_string()]
        } else if g.built_in {
            vec!["built in".to_string()]
        } else {
            let mut entries: Vec<String> = g
                .entries
                .iter()
                .map(|e| {
                    let stale =
                        if meridian_access::entry_holds(records, &e.plugin_instance_id, &e.role) {
                            ""
                        } else {
                            " (holds nothing: not a role it holds now)"
                        };
                    let role = if e.role.is_empty() {
                        String::new()
                    } else {
                        format!(" {}", e.role)
                    };
                    format!(
                        "{}{role} {}{stale}",
                        name_of(&plugin_names, &e.plugin_instance_id),
                        level_name(e.level)
                    )
                })
                .collect();
            entries.sort_by_key(|e| e.to_lowercase());
            entries
        };
        let action = if g.built_in {
            "<span class=\"pill\">built in</span>".to_string()
        } else {
            let mut fields = serde_json::Map::new();
            fields.insert("access_group_id".into(), g.access_group_id.clone().into());
            fields.insert("name".into(), g.name.clone().into());
            for e in &g.entries {
                fields.insert(
                    format!("level.{}:{}", e.plugin_instance_id, e.role),
                    choice_of(&g.entries, &e.plugin_instance_id, &e.role).into(),
                );
            }
            edit_button(
                "access-group",
                &format!("Edit {}", g.name),
                &serde_json::json!({ "fields": fields }),
            )
        };
        rows.push_str(&format!(
            "<tr data-id=\"{id}\" data-name=\"{name}\"><td>{named}</td><td>{entries}</td>\
             <td class=\"actions\">{action}</td></tr>",
            id = escape(&g.access_group_id),
            name = escape(&g.name),
            named = named(&g.name, &g.access_group_id),
            entries = summary(&entries, SHOWN_MEMBERS),
        ));
    }
    let dialogs = dialog(
        "access-group",
        "New access group",
        "/admin/access-groups#access-groups",
        token,
        &access_group_fields,
        "Create",
    );
    let table = if rows.is_empty() {
        "<p class=\"empty\">No access groups yet.</p>".to_string()
    } else {
        listing(
            "access-groups-table",
            "",
            "access groups",
            "Search by name or plugin",
            "<th>Name</th><th>Gives</th><th></th>",
            &rows,
        )
    };
    sections.push(section(
        "access-groups",
        "Access",
        "What a permission gives: plugins, each at admin, read or write, or admin and one of those.",
        &add_button("access-group", "an access group"),
        format!("{table}{dialogs}"),
    ));

    // ── Accounts ────────────────────────────────────────────────────────────
    // W6.3: a name, and optionally a custodian, a type, an owner and a note,
    // free text and all searchable. An edit sets all four as given. And each
    // account's attributes in the book of record (W9.13, W9.14): one book per
    // deployment (the product owner, 2026-10-10), so they sit on the account
    // they belong to, in its Edit's "In the book", saved to the book with a
    // reason, its own act, as the Books tab did before.
    let account_fields = format!(
        "<input type=\"hidden\" name=\"account_id\" value=\"\" data-record-id>\
         <label>Name<input name=\"name\" value=\"\" required></label>\
         <div class=\"pair\"><label>Custodian<input name=\"custodian\" value=\"\" maxlength=\"{label}\" \
         placeholder=\"Where it is held, e.g. Fidelity\"></label>\
         <label>Type<input name=\"account_type\" value=\"\" maxlength=\"{label}\" \
         placeholder=\"What it is, e.g. Roth IRA\"></label></div>\
         <label>Owner<input name=\"owner\" value=\"\" maxlength=\"{label}\" \
         placeholder=\"One ownership or grouping label, e.g. Fund I\"></label>\
         <label>Note<textarea name=\"note\" rows=\"2\" maxlength=\"{note_most}\"></textarea></label>\
         <p class=\"hint\">All but the name are optional free text, found by the search; \
         leaving one empty clears it.</p>",
        label = ACCOUNT_RECORD_CUSTODIAN_LENGTH.most,
        note_most = ACCOUNT_RECORD_NOTE_LENGTH.most,
    );
    let mut rows = String::new();
    let mut close_dialogs = String::new();
    for (row, a) in accounts_sorted.iter().enumerate() {
        let closed = a.state == AccountState::Closed as i32;
        let (currency, method, since) = super::books::in_book(books, &a.account_id);
        let actions = if closed {
            String::new()
        } else {
            let fill = serde_json::json!({ "fields": {
                "account_id": a.account_id,
                "name": a.name,
                "custodian": a.custodian,
                "account_type": a.account_type,
                "owner": a.owner,
                "note": a.note,
                "base_currency_code": currency,
                "lot_relief_default": method.to_string(),
                "waits_for_compliance": super::books::waits_word(super::books::waits(books, &a.account_id)),
                "account_mark": super::books::mark_word(books, &a.account_id),
                "since": if since.is_empty() { "not yet".to_string() } else { since.clone() },
                "reason": "",
            } });
            // The Close, its dialog where something is still attached
            // (approved 2026-10-14).
            let (close, dialog) = close_control(records, a, row, token);
            close_dialogs.push_str(&dialog);
            format!(
                "{edit}{close}",
                edit = edit_button("account", &format!("Edit {}", a.name), &fill),
            )
        };
        // The note, whole under the name, which is how it reads without
        // script. With script it is one line cut short, and the row's bubble
        // ([`NOTE_SCRIPT`]) holds it whole. Its marker is how a keyboard
        // reaches the bubble, and a screen reader is given the note as the
        // marker's description, from the row itself.
        let (mark, note) = if a.note.is_empty() {
            (String::new(), String::new())
        } else {
            (
                format!(
                    "<button type=\"button\" class=\"note-mark\" aria-label=\"Note\" \
                     aria-describedby=\"account-note-{row}\"></button>"
                ),
                format!(
                    "<span class=\"hint note\" id=\"account-note-{row}\">{}</span>",
                    escape(&a.note)
                ),
            )
        };
        // The owner, behind Edit, is still in the row, hidden, so the search
        // finds an account by it.
        rows.push_str(&format!(
            "<tr data-id=\"{id}\" data-name=\"{name}\"><td><span class=\"name\">{name}</span>{state}{mark}\
             <span class=\"id\">{id}</span>{note}{owner}</td>\
             <td>{custodian}</td><td>{account_type}</td><td>{currency}</td><td>{method}</td>\
             <td>{waits}</td>\
             <td class=\"actions\">{actions}</td></tr>",
            id = escape(&a.account_id),
            name = escape(&a.name),
            custodian = escape(&a.custodian),
            account_type = escape(&a.account_type),
            owner = if a.owner.is_empty() {
                String::new()
            } else {
                format!("<span hidden>{}</span>", escape(&a.owner))
            },
            currency = escape(&currency),
            method = escape(super::books::method_named(method)),
            waits = if super::books::waits(books, &a.account_id) {
                "Waits"
            } else {
                "Does not wait"
            },
            state = if closed {
                " <span class=\"pill\">closed</span>"
            } else {
                ""
            },
        ));
    }
    // One dialog, for a new account and for each Edit: "Account" posts the
    // account (W6.3); "In the book", shown for an account that exists, posts
    // its attributes to the book with a reason (W9.13). Each its own form,
    // its own save and its own record, as before.
    // Its look, on this page alone: every page carries the shared style,
    // and a page as big as the consent page is held to its size.
    let dialogs = format!(
        "<style>{PARTS_STYLE}</style><dialog id=\"account\" class=\"parts\" aria-labelledby=\"account-title\">\
         <div class=\"dialog-head\"><h2 id=\"account-title\" data-title-new=\"New account\">New account</h2></div>\
         <form method=\"post\" action=\"/admin/accounts#accounts\" class=\"dialog-part\">{token}\
         <div class=\"dialog-body\"><h3>Account</h3>{account_fields}\
         <div class=\"save\"><button type=\"submit\" class=\"primary\" data-label-new=\"Create\">Create</button></div>\
         </div></form>{book}\
         <div class=\"dialog-foot\"><button type=\"button\" data-dialog-close>Cancel</button></div></dialog>",
        book = super::books::book_form(token),
    ) + &close_dialogs;
    let not_answering = match books {
        super::books::Books::NotAnswering(why) => format!(
            "<p class=\"refused\">The book of record is not answering: {}</p>",
            escape(why)
        ),
        super::books::Books::Read(_) => String::new(),
    };
    let table = if rows.is_empty() {
        "<p class=\"empty\">No accounts yet.</p>".to_string()
    } else {
        listing(
            "accounts-table",
            " accounts",
            "accounts",
            "Search by name, custodian, type, owner or note",
            "<th>Name</th><th>Custodian</th><th>Type</th><th>Base currency</th>\
             <th>Lot relief</th><th>Compliance</th><th></th>",
            &rows,
        )
    };
    // One bubble for every row, moved to the row pointed at or focused, so
    // the page grows with the notes and not with a bubble per row. Hidden
    // from a screen reader, which has the note from the row.
    let bubble = if accounts_sorted.iter().any(|a| !a.note.is_empty()) {
        "<div class=\"note-bubble\" id=\"accounts-note-bubble\" aria-hidden=\"true\" hidden></div>"
    } else {
        ""
    };
    let table = format!("{not_answering}{table}{bubble}");
    sections.push(section(
        "accounts",
        "Accounts",
        "The firm's accounts, which permissions and plugins work on: where each is held, \
         what it is, and in the book its base currency and the lots a sale relieves when it \
         names none. Closed, never deleted; each change kept with who made it.",
        &add_button("account", "an account"),
        format!("{table}{dialogs}"),
    ));

    // ── Holds ───────────────────────────────────────────────────────────────
    // W6.25 (contract v16): the least time an edge plugin's raw records are
    // kept, per edge role or for every one, a deployment admin's.
    let (holds_body, holds_dialog) = super::holds::section(records, token);
    sections.push(section(
        "holds",
        "Holds",
        "The least time an edge plugin's raw records are kept anywhere, storage or archive, \
         as a firm's recordkeeping rule asks. No window goes below it, and nothing inside it \
         is deleted.",
        &add_button("hold", "a hold"),
        format!("{holds_body}{holds_dialog}"),
    ));

    // ── Time zone ───────────────────────────────────────────────────────────
    // W6.35 (contract v26): the deployment's time zone, a deployment admin's.
    let change = dialog(
        "time-zone-change",
        "Change the time zone",
        "/admin/time-zone#time-zone",
        token,
        &super::time_zone::fields(time_zone),
        "Change",
    );
    sections.push(section(
        "time-zone",
        "Time zone",
        "The zone this deployment keeps its own times in: when a held booking is due, and the \
         times shown here. Each change is kept with who made it and why.",
        "<button type=\"button\" data-dialog-open=\"time-zone-change\">Change</button>",
        format!(
            "{}{change}",
            super::time_zone::lines(time_zone, |subject| crate::tickets::display_name(
                records, subject
            ))
        ),
    ));

    // ── FX pivot currencies ─────────────────────────────────────────────────
    // W6.36 (contract v27; rulings 462 and 483): a deployment admin's.
    let (pivots, pivot_codes) = fx_pivots;
    let order = dialog(
        "fx-pivots-change",
        "Change the FX pivot currencies",
        "/admin/fx-pivot-currencies#fx-pivots",
        token,
        &super::fx_pivots::fields(pivots, pivot_codes),
        "Change",
    );
    sections.push(section(
        "fx-pivots",
        "FX pivot currencies",
        "The fiat currencies a valuation crosses through, in order, where the lake holds no \
         rate between two currencies. Each change is kept with who made it and why.",
        "<button type=\"button\" data-dialog-open=\"fx-pivots-change\">Change</button>",
        format!(
            "{}{order}",
            super::fx_pivots::lines(pivots, pivot_codes, |subject| {
                crate::tickets::display_name(records, subject)
            })
        ),
    ));

    // ── Series request rule ─────────────────────────────────────────────────
    // W6.37 (contract v28; rulings 561 and 589): a deployment admin's.
    let rule_change = dialog(
        "series-rule-change",
        "Change the series request rule",
        "/admin/series-request-rule#series-rule",
        token,
        &super::series_rule::fields(series_rule),
        "Change",
    );
    sections.push(section(
        "series-rule",
        super::series_rule::LABEL,
        "Which wants for a listed option or future this deployment does not hold are asked of \
         the platform with no person, and how many a day. Each change is kept with who made it \
         and why.",
        "<button type=\"button\" data-dialog-open=\"series-rule-change\">Change</button>",
        format!(
            "{}{rule_change}",
            super::series_rule::lines(series_rule, |subject| {
                crate::tickets::display_name(records, subject)
            })
        ),
    ));

    // ── Stale reference actions permitted ───────────────────────────────────
    // W6.38 (contract v28; rulings 517, 600): a deployment admin's.
    let stale_change = dialog(
        "stale-reference-change",
        "Change the stale reference actions permitted",
        "/admin/stale-reference-actions#stale-reference",
        token,
        &super::stale_reference::fields(stale_reference),
        "Change",
    );
    sections.push(section(
        "stale-reference",
        super::stale_reference::LABEL,
        "What an execution plugin's admin may choose when a placement's reference price is past \
         its ceiling. Each change is kept with who made it and why.",
        "<button type=\"button\" data-dialog-open=\"stale-reference-change\">Change</button>",
        format!(
            "{}{stale_change}",
            super::stale_reference::lines(stale_reference, |subject| {
                crate::tickets::display_name(records, subject)
            })
        ),
    ));

    // ── Model routing ───────────────────────────────────────────────────────
    // W12.6 (contract v26): the analytics routing table, a deployment admin's.
    let (table, served) = routing;
    let route = dialog(
        "model-routing-change",
        "Route a model id",
        "/admin/model-routing#model-routing",
        token,
        &super::model_routing::fields(table),
        "Change",
    );
    sections.push(section(
        "model-routing",
        "Model routing",
        "Which implementation prices each model id across the deployment: built-in, or a \
         launched pricing plugin. Each change is kept with who made it and why.",
        "<button type=\"button\" data-dialog-open=\"model-routing-change\">Change</button>",
        format!(
            "{}{route}",
            super::model_routing::lines(table, served, |subject| {
                crate::tickets::display_name(records, subject)
            })
        ),
    ));

    // ── Agents ──────────────────────────────────────────────────────────────
    // W6.29 (contract v20): what agents may do with each act, the ceiling on
    // each plugin's, core's own, per access group, and the lapse period.
    let (agents_body, agents_dialog) = agents;
    sections.push(section(
        "agents",
        "Agents",
        "What a person's agents may do with each act: make it, only prepare it for a person to \
         make, or not reach it; and whether its change needs a second pair of eyes. A plugin's \
         admin sets its acts within the ceiling set here.",
        "",
        format!("{agents_body}{agents_dialog}"),
    ));

    // ── Service accounts ────────────────────────────────────────────────────
    // W6.32 (contract v21): users of the deployment that are not people, each
    // with an owner, a purpose and a setting; granted as anyone is.
    let (service_accounts_body, service_accounts_dialog) = service_accounts;
    sections.push(section(
        "service-accounts",
        "Service accounts",
        "Users of this deployment that are not people, which a plugin's autonomous work runs \
         as: each with an owner who answers for it, a purpose, and a setting, prepare only or \
         act. It never signs in and holds no password; grant it by its ID in a user group's \
         logins, as anyone is. It administers nothing.",
        &add_button("service-account", "a service account"),
        format!("{service_accounts_body}{service_accounts_dialog}"),
    ));

    // ── Connected clients ───────────────────────────────────────────────────
    // W6.14: who has delegated to a client, and the way to each person's
    // delegations, where one or all of them are revoked. By user ID, then
    // login ID.
    let body = if delegating.is_empty() {
        "<p class=\"empty\">Nobody has delegated to a client.</p>".to_string()
    } else {
        let mut held: Vec<(Person, usize)> = delegating
            .iter()
            .map(|(login, name, count)| {
                (
                    Person {
                        user_id: super::people::user_id(login),
                        login: login.clone(),
                        name: name.clone(),
                    },
                    *count,
                )
            })
            .collect();
        held.sort_by(|(a, _), (b, _)| {
            a.user_id
                .to_lowercase()
                .cmp(&b.user_id.to_lowercase())
                .then_with(|| a.login.cmp(&b.login))
        });
        let rows: String = held
            .iter()
            .map(|(person, count)| {
                let called = if person.name.is_empty() {
                    &person.user_id
                } else {
                    &person.name
                };
                let path = crate::web::path_segment(&person.login);
                format!(
                    "<tr data-id=\"{login}\" data-name=\"{name}\"><td><span class=\"name\">{user}</span></td>\
                     <td>{named}</td><td data-count=\"{count}\">{count}</td><td class=\"actions\">\
                     <a href=\"/admin/people/{path}/delegations\">See them</a> \
                     <form method=\"post\" action=\"/admin/people/{path}/delegations/revoke\" \
                     data-confirm=\"Revoke every delegation {called} holds? Each client stops at its next request.\">{token}\
                     <input type=\"hidden\" name=\"all\" value=\"1\">\
                     <button type=\"submit\">Revoke them all</button></form></td></tr>",
                    login = escape(&person.login),
                    name = escape(&person.name),
                    user = escape(&person.user_id),
                    named = named(&person.name, &person.login),
                    called = escape(called),
                    path = escape(&path),
                )
            })
            .collect();
        listing(
            "connected-clients-table",
            "",
            "people",
            "Search by user ID, name or login",
            "<th>User ID</th><th>Person</th><th>Clients</th><th></th>",
            &rows,
        )
    };
    sections.push(section(
        "connected-clients",
        "Connected clients",
        "Who has delegated to a client -- the <code>meridian</code> command on a computer, or \
         an agent -- and the way to revoke one or all of a person's delegations. Their browser \
         sessions are untouched.",
        "",
        body,
    ));

    let notice = if notice.is_empty() {
        String::new()
    } else {
        format!("<p class=\"passed\">{}</p>", escape(notice))
    };
    format!(
        "<div class=\"admin\"><div class=\"page-head\"><h1>Settings</h1>\
         </div>{notice}{}</div>\
         <script>{SCRIPT_START}{}{NOTE_SCRIPT}{SCRIPT_END}{}</script>",
        sections.concat(),
        picker::SCRIPT,
        super::data_sources::PAGER_SCRIPT,
    )
}

/// What an access group gives a plugin's role: nothing, or admin and at most
/// one data level, read or write, which includes read. One choice, so an
/// entry can never name read and write both (W6.7, per role from v15).
fn level_select(instance: &str, role: &str) -> String {
    let on = if role.is_empty() {
        escape(instance)
    } else {
        format!("{} on {}", escape(role), escape(instance))
    };
    format!(
        "<select name=\"level.{id}:{role}\" aria-label=\"Level: {on}\"><option value=\"\">None</option>\
         <option value=\"read\">Read</option>\
         <option value=\"write\">Write</option>\
         <option value=\"admin\">Admin</option>\
         <option value=\"admin-read\">Admin, read</option>\
         <option value=\"admin-write\">Admin, write</option></select>",
        id = escape(instance),
        role = escape(role),
    )
}

/// The roles the records know a plugin holds, none where it holds none or
/// is not listed.
fn known_roles(records: &AccessRecords, instance: &str) -> Vec<String> {
    meridian_access::known_roles(records, instance)
        .map(<[String]>::to_vec)
        .unwrap_or_default()
}

/// A plugin's roles as a flag on an entry that no longer matches says them.
fn roles_said(records: &AccessRecords, instance: &str) -> String {
    match meridian_access::known_roles(records, instance) {
        Some([]) => format!("{instance} holds no role now"),
        Some(roles) => format!("{instance} holds {} now", roles.join(", ")),
        None => format!("{instance}'s roles are not known"),
    }
}

/// Tabs, dialogs, the pickers ([`picker::SCRIPT`], between the two halves),
/// and a question before anything destructive. Without it the page is every
/// section at once and every form posts as it did.
/// The account's dialog of two parts, each its own form; and with script, an
/// account's note behind its marker's bubble and its Edit, its row one line
/// (the product owner, 2026-10-10).
const PARTS_STYLE: &str = ".admin.js #accounts-table .hint.note{display:none}\
#accounts-table :is(td,th){white-space:nowrap}\
dialog.parts{width:min(40rem,calc(100vw - 2rem))}\
dialog.parts .dialog-part+.dialog-part{border-top:1px solid var(--line-soft)}\
dialog.parts h3{margin:.1rem 0 .4rem;font-size:.95rem}dialog.parts h3 .hint{display:inline;font-weight:400}\
dialog.parts label{margin-bottom:.45rem}dialog.parts .hint{margin:.1rem 0 .3rem}\
dialog.parts .pair{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:0 .75rem;align-items:end}\
dialog.parts .save{display:flex;justify-content:flex-end;margin:.2rem 0 .4rem}dialog.parts .pair.save{display:grid}\
dialog.parts .pair.save button{justify-self:end;margin:0 0 .45rem}";

pub(super) const SCRIPT_START: &str = r##"(function () {
  var root = document.querySelector(".admin");
  if (!root) return;
  root.classList.add("js");
  var sections = Array.prototype.slice.call(root.querySelectorAll("section.admin-section"));
  var tabs = Array.prototype.slice.call(root.querySelectorAll("nav.tabs a"));
  function show(id) {
    // The Books tab is the Accounts tab's now (2026-10-10): its address still opens it.
    if (id === "books") id = "accounts";
    if (!sections.some(function (s) { return s.id === id; })) id = sections[0].id;
    sections.forEach(function (s) { s.classList.toggle("current", s.id === id); });
    tabs.forEach(function (t) { t.classList.toggle("here", t.dataset.tab === id); });
  }
  tabs.forEach(function (t) {
    t.addEventListener("click", function (event) {
      event.preventDefault();
      history.replaceState(null, "", "#" + t.dataset.tab);
      show(t.dataset.tab);
    });
  });
  show(location.hash.slice(1));
  window.addEventListener("hashchange", function () { show(location.hash.slice(1)); });
"##;

/// An account's note in a bubble (the product owner, 2026-09-30: "show notes
/// in a bubble when we hover over the line"): pointing at a row with a note,
/// or focusing or pressing its marker, shows the note whole in the one bubble,
/// placed under the row, or over it when there is no room below. It stays
/// while the pointer is on the row or on the bubble, and goes on leaving them,
/// on Escape, on a press elsewhere, and when the table scrolls, is searched or
/// sorted, or the window resizes. The note reaches the bubble as text, never as markup.
pub(super) const NOTE_SCRIPT: &str = r##"
  (function () {
    var table = document.getElementById("accounts-table");
    var bubble = document.getElementById("accounts-note-bubble");
    if (!table || !bubble) return;
    document.body.appendChild(bubble);
    var shown = null;
    var leaving = 0;
    function hide() {
      window.clearTimeout(leaving);
      shown = null;
      bubble.hidden = true;
    }
    function later() {
      window.clearTimeout(leaving);
      leaving = window.setTimeout(hide, 150);
    }
    function show(row) {
      window.clearTimeout(leaving);
      if (row === shown) return;
      var note = row && row.querySelector(".note");
      if (!note) { hide(); return; }
      shown = row;
      bubble.textContent = note.textContent;
      bubble.hidden = false;
      var edge = 8;
      var line = row.getBoundingClientRect();
      var cell = row.cells[0].getBoundingClientRect();
      var width = bubble.offsetWidth;
      var height = bubble.offsetHeight;
      var top = line.bottom;
      if (top + height > window.innerHeight - edge && line.top - height >= edge) top = line.top - height;
      var left = Math.max(edge, Math.min(cell.left, window.innerWidth - width - edge));
      bubble.style.top = top + window.scrollY + "px";
      bubble.style.left = left + window.scrollX + "px";
    }
    table.addEventListener("mouseover", function (event) {
      var row = event.target.closest("tbody tr");
      if (row && table.contains(row)) show(row); else later();
    });
    table.addEventListener("mouseout", function (event) {
      if (!bubble.contains(event.relatedTarget) && !table.contains(event.relatedTarget)) later();
    });
    bubble.addEventListener("mouseenter", function () { window.clearTimeout(leaving); });
    bubble.addEventListener("mouseleave", function (event) {
      if (!shown || !shown.contains(event.relatedTarget)) later();
    });
    table.addEventListener("focusin", function (event) {
      var mark = event.target.closest(".note-mark");
      if (mark) show(mark.closest("tr"));
    });
    table.addEventListener("focusout", function (event) {
      if (event.target.closest(".note-mark")) hide();
    });
    // A press on a marker shows its note, which is how a touch reaches it; a
    // press on anything else but the bubble or the shown row's text hides it.
    document.addEventListener("click", function (event) {
      var mark = event.target.closest(".note-mark");
      if (mark && table.contains(mark)) { show(mark.closest("tr")); return; }
      if (bubble.contains(event.target)) return;
      if (shown && shown.contains(event.target) && !event.target.closest("button, a, form")) return;
      hide();
    });
    document.addEventListener("keydown", function (event) {
      if (event.key === "Escape" && shown) hide();
    });
    window.addEventListener("resize", hide);
    if (table.parentNode) table.parentNode.addEventListener("scroll", hide);
    var filter = document.querySelector("[data-filter=\"accounts-table\"]");
    if (filter) filter.addEventListener("input", hide);
  })();
"##;

pub(super) const SCRIPT_END: &str = r##"
  // One dialog per kind of record: a New opens it empty, an Edit fills it
  // from the record it was drawn with (data-fill: fields by name, and boxes
  // ticked by name), and its heading and button say which.
  function prepare(dialog, opener) {
    var forms = dialog.querySelectorAll("form");
    if (!forms.length) return;
    var fill = null;
    try { fill = JSON.parse(opener.getAttribute("data-fill") || "null"); } catch (e) { fill = null; }
    // A dialog of parts, each its own form: every one filled from the Edit,
    // and a part that needs the record to exist shown only for an Edit.
    Array.prototype.forEach.call(dialog.querySelectorAll("[data-edit-only]"), function (el) { el.hidden = !fill; });
    Array.prototype.forEach.call(forms, function (form) { fillForm(form, fill); });
    var head = dialog.querySelector("[data-title-new]");
    if (head) head.textContent = fill ? opener.getAttribute("data-title") : head.getAttribute("data-title-new");
    var submit = dialog.querySelector("[data-label-new]");
    if (submit) submit.textContent = fill ? "Save" : submit.getAttribute("data-label-new");
    Array.prototype.forEach.call(dialog.querySelectorAll("[data-picker]"), function (p) { if (p.refresh) p.refresh(); });
  }
  function fillForm(form, fill) {
    form.reset();
    Array.prototype.forEach.call(form.querySelectorAll("[data-record-id]"), function (el) { el.value = ""; });
    if (fill) {
      var fields = fill.fields || {};
      var checked = fill.checked || {};
      var ticked = {};
      Object.keys(checked).forEach(function (name) {
        ticked[name] = Object.create(null);
        checked[name].forEach(function (value) { ticked[name][value] = true; });
      });
      Array.prototype.forEach.call(form.elements, function (el) {
        if (el.type === "checkbox") {
          if (ticked[el.name]) el.checked = ticked[el.name][el.value] === true;
        } else if (Object.prototype.hasOwnProperty.call(fields, el.name)) {
          el.value = fields[el.name];
        }
      });
    }
  }
  document.addEventListener("click", function (event) {
    var opener = event.target.closest("[data-dialog-open]");
    if (opener) {
      var dialog = document.getElementById(opener.getAttribute("data-dialog-open"));
      if (dialog) { prepare(dialog, opener); dialog.showModal(); var first = dialog.querySelector("input:not([type=hidden]), select, textarea"); if (first) first.focus(); }
      return;
    }
    var closer = event.target.closest("[data-dialog-close]");
    if (closer) { closer.closest("dialog").close(); return; }
    if (event.target.tagName === "DIALOG") event.target.close();
  });
  // A form's data-confirm asks in the chrome's dialog (html.rs).
})();"##;

/// What still holds an account (approved 2026-10-14): a plugin's link of an
/// external account to it, a plugin's grant of it, an `ems`'s venue
/// account mapping to it -- each with the plugin's page it is undone at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attached {
    pub plugin: String,
    pub words: String,
    /// A grant the dashboard withdraws itself (W6.30); a link and a mapping
    /// are undone at the plugin's own page.
    pub withdrawable: bool,
}

/// Everything attached to an account, links first, then grants, then
/// mappings, as the conductor names them when it refuses the closure.
pub fn attached(records: &AccessRecords, account_id: &str) -> Vec<Attached> {
    let mut out: Vec<Attached> = records
        .links
        .iter()
        .filter(|link| link.account_id == account_id)
        .map(|link| Attached {
            plugin: link.plugin_instance_id.clone(),
            words: format!(
                "{} links its external account {} to it",
                link.plugin_instance_id, link.external_account_id
            ),
            withdrawable: false,
        })
        .collect();
    out.extend(
        records
            .linkable_accounts
            .iter()
            .filter(|granted| granted.account_ids.iter().any(|id| id == account_id))
            .map(|granted| Attached {
                plugin: granted.plugin_instance_id.clone(),
                words: format!("{} is granted it", granted.plugin_instance_id),
                withdrawable: true,
            }),
    );
    out.extend(records.venue_account_maps.iter().flat_map(|map| {
        map.mappings
            .iter()
            .filter(|mapping| mapping.account_id == account_id)
            .map(|mapping| Attached {
                plugin: map.ems_instance_id.clone(),
                words: format!(
                    "{} maps {} at {} to it",
                    map.ems_instance_id,
                    mapping.venue_account_code,
                    mapping.connectivity_instance_id
                ),
                withdrawable: false,
            })
    }));
    out
}

/// The Close for an account (approved 2026-10-14): the usual confirmation
/// where nothing is attached; otherwise a dialog listing everything
/// attached, each linking to its plugin's page, with "Unlink all and close"
/// -- one confirmation, each unlink its own record, then the closure -- and
/// "Cancel". A link or an `ems`'s mapping is undone at its plugin's page:
/// the dialog says so, and the closure waits for it.
pub fn close_control(
    records: &AccessRecords,
    account: &AccountRecord,
    row: usize,
    token: &str,
) -> (String, String) {
    let attached = attached(records, &account.account_id);
    let name = escape(&account.name);
    let id = escape(&account.account_id);
    if attached.is_empty() {
        return (
            format!(
                "<form method=\"post\" action=\"/admin/accounts/close#accounts\" \
                 data-confirm=\"Close {name}? A closed account is kept, and nobody works in it.\">{token}\
                 <input type=\"hidden\" name=\"account_id\" value=\"{id}\"><button type=\"submit\">Close</button></form>"
            ),
            String::new(),
        );
    }
    let items: String = attached
        .iter()
        .map(|item| {
            format!(
                "<li><a href=\"/admin/plugins/{plugin}\">{words}</a></li>",
                plugin = escape(&item.plugin),
                words = escape(&item.words),
            )
        })
        .collect();
    let all_withdrawable = attached.iter().all(|item| item.withdrawable);
    let (foot, hint) = if all_withdrawable {
        (
            "<button type=\"submit\" class=\"primary\" name=\"unlink_all\" value=\"1\">Unlink all and close</button>",
            "Each is withdrawn as its own record, and then the account is closed.",
        )
    } else {
        (
            "",
            "A link and an ems's mapping are undone at the plugin's own page: open each, undo it, \
             and close the account then.",
        )
    };
    let button = format!(
        "<button type=\"button\" data-dialog-open=\"close-account-{row}\" \
         data-attached=\"{n}\">Close</button>",
        n = attached.len(),
    );
    let dialog = format!(
        "<dialog id=\"close-account-{row}\" aria-labelledby=\"close-account-{row}-title\">\
         <form method=\"post\" action=\"/admin/accounts/close#accounts\">{token}\
         <input type=\"hidden\" name=\"account_id\" value=\"{id}\">\
         <div class=\"dialog-head\"><h2 id=\"close-account-{row}-title\">Close {name}</h2></div>\
         <div class=\"dialog-body\"><p>Still attached to it:</p><ul class=\"attached-list\" data-attached-list>{items}</ul>\
         <p class=\"hint\">{hint} A closed account is kept, and nobody works in it.</p></div>\
         <div class=\"dialog-foot\"><button type=\"button\" data-dialog-close>Cancel</button>{foot}</div>\
         </form></dialog>"
    );
    (button, dialog)
}

#[cfg(test)]
mod close_tests {
    use super::*;
    use meridian_domain::v1::{ExternalAccountLink, LinkableAccounts};

    fn account() -> AccountRecord {
        AccountRecord {
            account_id: "ACC-1".into(),
            name: "Primary".into(),
            ..Default::default()
        }
    }

    #[test]
    fn close_lists_what_is_attached_and_offers_unlink_all_only_where_it_can() {
        let mut records = AccessRecords::default();
        let (button, dialog) = close_control(&records, &account(), 0, "<t>");
        assert!(button.contains("data-confirm=\"Close Primary?"), "{button}");
        assert!(dialog.is_empty());

        records.linkable_accounts.push(LinkableAccounts {
            plugin_instance_id: "oanda-1".into(),
            account_ids: vec!["ACC-1".into()],
            ..Default::default()
        });
        let (button, dialog) = close_control(&records, &account(), 0, "<t>");
        assert!(button.contains("data-dialog-open=\"close-account-0\""));
        assert!(dialog.contains("href=\"/admin/plugins/oanda-1\""));
        assert!(dialog.contains(">Unlink all and close</button>"));
        assert!(dialog.contains(">Cancel</button>"));

        records.links.push(ExternalAccountLink {
            plugin_instance_id: "oanda-1".into(),
            external_account_id: "101-001-1".into(),
            account_id: "ACC-1".into(),
        });
        let (_, dialog) = close_control(&records, &account(), 0, "<t>");
        assert!(dialog.contains("101-001-1"));
        assert!(
            !dialog.contains("Unlink all and close"),
            "a link is undone at its plugin's page"
        );
        assert!(dialog.contains(">Cancel</button>"));
        assert_eq!(attached(&records, "ACC-1").len(), 2);
    }
}
