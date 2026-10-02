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
use meridian_domain::account;
use meridian_domain::v1::{AccessEntry, AccessRecords, AccountRecord, AccountState};

use std::collections::HashSet;

use crate::html::escape;

use super::people::Person;
use super::picker::{self, Choice};
use super::view::{self, Line};

/// The sections, in the order an administrator reaches for them: who may do
/// what first, then the parts it is made of.
const TABS: [(&str, &str); 9] = [
    ("plugins", "Plugins"),
    ("permissions", "Permissions"),
    ("user-groups", "User groups"),
    ("account-groups", "Account groups"),
    ("access-groups", "Access groups"),
    ("accounts", "Accounts"),
    ("books", "Books"),
    ("connected-clients", "Connected clients"),
    ("terminal-sessions", "Terminal sessions"),
];

fn level_name(level: i32) -> &'static str {
    match AccessLevel::try_from(level) {
        Ok(AccessLevel::Write) => "write",
        Ok(AccessLevel::Admin) => "admin",
        _ => "read",
    }
}

/// What an access group gives one plugin, as the form's one choice names it:
/// `read`, `write` or `admin`, or `admin-read` or `admin-write` for admin
/// beside a data level (W6.7).
pub(crate) fn choice_of(entries: &[AccessEntry], plugin: &str) -> &'static str {
    let on: Vec<i32> = entries
        .iter()
        .filter(|e| e.plugin_instance_id == plugin)
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
fn listing(id: &str, class: &str, noun: &str, search: &str, head: &str, rows: &str) -> String {
    format!(
        "<div class=\"filter-row\"><input class=\"filter\" type=\"search\" data-filter=\"{id}\" hidden \
         placeholder=\"{search}\" aria-label=\"Search {noun}\">\
         <span class=\"filter-count\" data-filter-count=\"{id}\" aria-live=\"polite\" hidden></span></div>\
         <div class=\"scroll\"><table class=\"list{class}\" id=\"{id}\" data-sortable><thead><tr>{head}</tr></thead>\
         <tbody>{rows}</tbody></table></div>\
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
    holders: &[(String, String, usize)],
    delegating: &[(String, String, usize)],
    plugins: &[Line],
    people: &[Person],
    books: &super::books::Books,
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
    let plugin_choices: Vec<Choice> = instances
        .iter()
        .map(|(id, name)| Choice {
            value: id.clone(),
            label: if name.is_empty() {
                id.clone()
            } else {
                name.clone()
            },
            detail: id.clone(),
            after: level_select(id),
            ..Default::default()
        })
        .collect();
    let access_group_fields = format!(
        "<input type=\"hidden\" name=\"access_group_id\" value=\"\" data-record-id>\
         <label>Name<input name=\"name\" value=\"\" required></label>{}\
         <p class=\"hint\">Each plugin chosen is given admin, to configure it and reach no \
         account, and at most one data level: read to see what it shows, write to act through it \
         too, which includes read. A person holding admin and a data level chooses Manage, Open or \
         View on the home.</p>",
        if plugin_choices.is_empty() {
            "<p class=\"hint\">No plugins yet: launch one from the catalogue.</p>".to_string()
        } else {
            picker::many(
                "access-group-plugins",
                "plugin",
                "Plugins",
                "plugins",
                &plugin_choices,
                &HashSet::new(),
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
                    format!(
                        "{} {}",
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
                    format!("level.{}", e.plugin_instance_id),
                    choice_of(&g.entries, &e.plugin_instance_id).into(),
                );
            }
            let mut chosen: Vec<&str> = g
                .entries
                .iter()
                .map(|e| e.plugin_instance_id.as_str())
                .collect();
            chosen.dedup();
            edit_button(
                "access-group",
                &format!("Edit {}", g.name),
                &serde_json::json!({ "fields": fields, "checked": { "plugin": chosen } }),
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
    // free text and all searchable. An edit sets all four as given.
    let account_fields = format!(
        "<input type=\"hidden\" name=\"account_id\" value=\"\" data-record-id>\
         <label>Name<input name=\"name\" value=\"\" required></label>\
         <label>Custodian<input name=\"custodian\" value=\"\" maxlength=\"{label}\" \
         placeholder=\"Where it is held, e.g. Fidelity\"></label>\
         <label>Type<input name=\"account_type\" value=\"\" maxlength=\"{label}\" \
         placeholder=\"What it is, e.g. Roth IRA\"></label>\
         <label>Owner<input name=\"owner\" value=\"\" maxlength=\"{label}\" \
         placeholder=\"One ownership or grouping label, e.g. Fund I\"></label>\
         <label>Note<textarea name=\"note\" rows=\"3\" maxlength=\"{note_most}\"></textarea></label>\
         <p class=\"hint\">All but the name are optional and free text, and the search box \
         finds an account by any of them. Leaving one empty clears it.</p>",
        label = account::LABEL_MOST,
        note_most = account::NOTE_MOST,
    );
    let mut rows = String::new();
    for (row, a) in accounts_sorted.iter().enumerate() {
        let closed = a.state == AccountState::Closed as i32;
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
            } });
            format!(
                "{edit}<form method=\"post\" action=\"/admin/accounts/close#accounts\" \
                 data-confirm=\"Close {name}? A closed account is kept, and nobody works in it.\">{token}\
                 <input type=\"hidden\" name=\"account_id\" value=\"{id}\"><button type=\"submit\">Close</button></form>",
                edit = edit_button("account", &format!("Edit {}", a.name), &fill),
                name = escape(&a.name),
                id = escape(&a.account_id),
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
        rows.push_str(&format!(
            "<tr data-id=\"{id}\" data-name=\"{name}\"><td><span class=\"name\">{name}</span>{mark}\
             <span class=\"id\">{id}</span>{note}</td>\
             <td>{custodian}</td><td>{account_type}</td><td>{owner}</td><td>{state}</td>\
             <td class=\"actions\">{actions}</td></tr>",
            id = escape(&a.account_id),
            name = escape(&a.name),
            custodian = escape(&a.custodian),
            account_type = escape(&a.account_type),
            owner = escape(&a.owner),
            state = if closed {
                "<span class=\"pill\">closed</span>"
            } else {
                "<span class=\"pill good\">open</span>"
            },
        ));
    }
    let dialogs = dialog(
        "account",
        "New account",
        "/admin/accounts#accounts",
        token,
        &account_fields,
        "Create",
    );
    let table = if rows.is_empty() {
        "<p class=\"empty\">No accounts yet.</p>".to_string()
    } else {
        listing(
            "accounts-table",
            " accounts",
            "accounts",
            "Search by name, custodian, type, owner or note",
            "<th>Name</th><th>Custodian</th><th>Type</th><th>Owner</th><th>State</th><th></th>",
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
    let table = format!("{table}{bubble}");
    sections.push(section(
        "accounts",
        "Accounts",
        "The firm's accounts, which permissions and plugins work on: where each is held, \
         what it is and who owns it. Closed, never deleted.",
        &add_button("account", "an account"),
        format!("{table}{dialogs}"),
    ));

    // ── Books ───────────────────────────────────────────────────────────────
    // W9.13, W9.14: each open account's attributes in the book of record,
    // set for a deployment admin with a reason; no account's data.
    let open: Vec<AccountRecord> = records
        .accounts
        .iter()
        .filter(|a| a.state != meridian_domain::v1::AccountState::Closed as i32)
        .cloned()
        .collect();
    let (books_body, books_dialog) = super::books::section(&open, books, token);
    sections.push(section(
        "books",
        "Books",
        "Each account's base currency and the lots a sale relieves when it names none, in          the book of record, and the date its opening balance stands for. Each change is          journalled with its reason.",
        "",
        format!("{books_body}{books_dialog}"),
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

    // ── Terminal sessions ───────────────────────────────────────────────────
    // W6.14. Per person: what is being ended is their access from a terminal,
    // so there is no choosing among their sessions to offer. By user ID, then
    // login ID.
    let body = if holders.is_empty() {
        "<p class=\"empty\">Nobody holds a terminal session.</p>".to_string()
    } else {
        let mut held: Vec<(Person, usize)> = holders
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
                format!(
                    "<tr data-id=\"{login}\" data-name=\"{name}\"><td><span class=\"name\">{user}</span></td>\
                     <td>{named}</td><td data-count=\"{count}\">{count}</td><td class=\"actions\">\
                     <form method=\"post\" action=\"/admin/end-terminal-sessions#terminal-sessions\" \
                     data-confirm=\"End {called}'s terminal sessions? Their CLI signs in again.\">{token}\
                     <input type=\"hidden\" name=\"login\" value=\"{login}\">\
                     <button type=\"submit\">End them</button></form></td></tr>",
                    login = escape(&person.login),
                    name = escape(&person.name),
                    user = escape(&person.user_id),
                    named = named(&person.name, &person.login),
                    called = escape(called),
                )
            })
            .collect();
        listing(
            "terminal-sessions-table",
            "",
            "people",
            "Search by user ID, name or login",
            "<th>User ID</th><th>Person</th><th>Sessions</th><th></th>",
            &rows,
        )
    };
    sections.push(section(
        "terminal-sessions",
        "Terminal sessions",
        "Who is signed in from a terminal by a CLI from before delegations, honoured until \
         each session lapses.",
        "",
        body,
    ));

    let tabs: String = TABS
        .iter()
        .map(|(id, title)| format!("<a href=\"#{id}\" data-tab=\"{id}\">{title}</a>"))
        .collect();
    let notice = if notice.is_empty() {
        String::new()
    } else {
        format!("<p class=\"passed\">{}</p>", escape(notice))
    };
    format!(
        "<div class=\"admin\"><div class=\"page-head\"><h1>Settings</h1></div>\
         {notice}<nav class=\"tabs\">{tabs}</nav>{}</div>\
         <script>{SCRIPT_START}{}{NOTE_SCRIPT}{SCRIPT_END}</script>",
        sections.concat(),
        picker::SCRIPT,
    )
}

/// What an access group gives a plugin: admin, and at most one data level,
/// read or write, which includes read. One choice, so an entry can never name
/// read and write both (W6.7).
fn level_select(instance: &str) -> String {
    format!(
        "<select name=\"level.{id}\" aria-label=\"Level on {id}\"><option value=\"read\">Read</option>\
         <option value=\"write\">Write (includes read)</option>\
         <option value=\"admin\">Admin (configures it, no account)</option>\
         <option value=\"admin-read\">Admin and read</option>\
         <option value=\"admin-write\">Admin and write</option></select>",
        id = escape(instance)
    )
}

/// Tabs, dialogs, the pickers ([`picker::SCRIPT`], between the two halves),
/// and a question before anything destructive. Without it the page is every
/// section at once and every form posts as it did.
const SCRIPT_START: &str = r##"(function () {
  var root = document.querySelector(".admin");
  if (!root) return;
  root.classList.add("js");
  var sections = Array.prototype.slice.call(root.querySelectorAll("section.admin-section"));
  var tabs = Array.prototype.slice.call(root.querySelectorAll("nav.tabs a"));
  function show(id) {
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

const SCRIPT_END: &str = r##"
  // One dialog per kind of record: a New opens it empty, an Edit fills it
  // from the record it was drawn with (data-fill: fields by name, and boxes
  // ticked by name), and its heading and button say which.
  function prepare(dialog, opener) {
    var form = dialog.querySelector("form");
    if (!form) return;
    form.reset();
    Array.prototype.forEach.call(form.querySelectorAll("[data-record-id]"), function (el) { el.value = ""; });
    var fill = null;
    try { fill = JSON.parse(opener.getAttribute("data-fill") || "null"); } catch (e) { fill = null; }
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
    var head = dialog.querySelector("[data-title-new]");
    if (head) head.textContent = fill ? opener.getAttribute("data-title") : head.getAttribute("data-title-new");
    var submit = dialog.querySelector("[data-label-new]");
    if (submit) submit.textContent = fill ? "Save" : submit.getAttribute("data-label-new");
    Array.prototype.forEach.call(dialog.querySelectorAll("[data-picker]"), function (p) { if (p.refresh) p.refresh(); });
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
  document.addEventListener("submit", function (event) {
    var form = event.target;
    if (form.hasAttribute("data-confirm") && !window.confirm(form.getAttribute("data-confirm"))) {
      event.preventDefault();
    }
  });
})();"##;
