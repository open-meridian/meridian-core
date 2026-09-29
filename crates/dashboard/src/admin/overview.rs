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

use meridian_domain::account;
use meridian_domain::v1::{AccessLevel, AccessRecords, AccountRecord, AccountState};

use crate::html::escape;

use super::view::{self, Line};

/// The sections, in the order an administrator reaches for them: who may do
/// what first, then the parts it is made of.
const TABS: [(&str, &str); 7] = [
    ("plugins", "Plugins"),
    ("permissions", "Permissions"),
    ("user-groups", "User groups"),
    ("account-groups", "Account groups"),
    ("access-groups", "Access groups"),
    ("accounts", "Accounts"),
    ("terminal-sessions", "Terminal sessions"),
];

fn level_name(level: i32) -> &'static str {
    if level == AccessLevel::Write as i32 {
        "write"
    } else {
        "read"
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

fn new_button(dialog: &str, label: &str) -> String {
    format!(
        "<button type=\"button\" class=\"primary\" data-dialog-open=\"{dialog}\">{label}</button>"
    )
}

fn dialog(id: &str, title: &str, action: &str, token: &str, fields: &str, submit: &str) -> String {
    format!(
        "<dialog id=\"{id}\"><form method=\"post\" action=\"{action}\">{token}\
         <div class=\"dialog-head\"><h2>{title}</h2></div>\
         <div class=\"dialog-body\">{fields}</div>\
         <div class=\"dialog-foot\"><button type=\"button\" data-dialog-close>Cancel</button>\
         <button type=\"submit\" class=\"primary\">{submit}</button></div></form></dialog>"
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

pub fn render(
    records: &AccessRecords,
    holders: &[(String, String, usize)],
    plugins: &[Line],
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
        let rows: String = plugins
            .iter()
            .map(|line| {
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
                     <td>{state}<span class=\"hint\">{detail}</span></td>\
                     <td>{settings}</td><td class=\"flags\">{flags}</td>\
                     <td class=\"actions\"><a class=\"button\" href=\"{href}\">Manage</a></td></tr>",
                    id = escape(&line.instance),
                    plugin = match &line.name {
                        Some(name) => format!("<span class=\"name\">{}</span>", escape(name)),
                        None => "<span class=\"faint\">not from the catalogue</span>".to_string(),
                    },
                    state = view::state_badge(&line.state),
                    detail = escape(&line.state.detail),
                    flags = view::flags(line, None),
                    href = escape(&view::path(&line.instance)),
                )
            })
            .collect();
        format!(
            "<input class=\"filter\" type=\"search\" data-filter=\"plugins-table\" hidden \
             placeholder=\"Search by plugin, instance, health or what it needs\" aria-label=\"Search plugins\">\
             <div class=\"scroll\"><table class=\"list plugins\" id=\"plugins-table\"><thead><tr><th>Plugin</th>\
             <th>Instance</th><th>Health</th><th>Settings</th><th>Needs you</th><th></th></tr></thead>\
             <tbody>{rows}</tbody></table></div>"
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
    let mut rows = String::new();
    for p in &records.permissions {
        let accounts = if p.account_group_id.is_empty() {
            "<span class=\"name\">every account</span>".to_string()
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
        format!("<div class=\"scroll\"><table class=\"list\"><thead><tr><th>User group</th><th>On accounts</th><th>Access</th><th></th></tr></thead><tbody>{rows}</tbody></table></div>")
    };
    let user_groups: Vec<(String, String)> = records
        .user_groups
        .iter()
        .map(|g| (g.user_group_id.clone(), g.name.clone()))
        .collect();
    let mut account_groups = vec![(
        String::new(),
        "Every account (deployment admin only)".to_string(),
    )];
    account_groups.extend(
        records
            .account_groups
            .iter()
            .map(|g| (g.account_group_id.clone(), g.name.clone())),
    );
    let access_groups: Vec<(String, String)> = records
        .access_groups
        .iter()
        .map(|g| (g.access_group_id.clone(), g.name.clone()))
        .collect();
    let grant = dialog(
        "new-permission",
        "Grant a permission",
        "/admin/permissions#permissions",
        token,
        &format!(
            "<label>User group<select name=\"user_group_id\" required>{}</select></label>\
             <label>On accounts<select name=\"account_group_id\">{}</select></label>\
             <label>Access<select name=\"access_group_id\" required>{}</select></label>\
             <p class=\"hint\">Deployment admin is granted on every account; any other access \
             group, on an account group.</p>",
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
        &new_button("new-permission", "Grant a permission"),
        format!("{table}{grant}"),
    ));

    // ── User groups ─────────────────────────────────────────────────────────
    let user_group_fields = |id: &str, name: &str, directory: &str, logins: &str| {
        format!(
            "<input type=\"hidden\" name=\"user_group_id\" value=\"{}\">\
             <label>Name<input name=\"name\" value=\"{}\" required></label>\
             <label>Directory groups, one per line<textarea name=\"directory_groups\" rows=\"3\">{}</textarea></label>\
             <label>Logins, one per line<textarea name=\"logins\" rows=\"3\">{}</textarea></label>\
             <p class=\"hint\">Somebody is in the group when their directory says they are in \
             one of its groups, or when their login is listed. A login here is <code>local|name</code> \
             for an account this deployment holds.</p>",
            escape(id),
            escape(name),
            escape(directory),
            escape(logins)
        )
    };
    let mut rows = String::new();
    let mut dialogs = String::new();
    for g in &records.user_groups {
        let edit = format!("edit-{}", g.user_group_id);
        rows.push_str(&format!(
            "<tr data-id=\"{id}\" data-name=\"{name}\"><td>{named}</td><td>{dirs}</td><td>{logins}</td>\
             <td class=\"actions\"><button type=\"button\" data-dialog-open=\"{edit}\">Edit</button></td></tr>",
            id = escape(&g.user_group_id),
            name = escape(&g.name),
            named = named(&g.name, &g.user_group_id),
            dirs = escape(&g.directory_groups.join(", ")),
            logins = escape(&g.logins.join(", ")),
            edit = escape(&edit),
        ));
        dialogs.push_str(&dialog(
            &escape(&edit),
            &format!("Edit {}", escape(&g.name)),
            "/admin/user-groups#user-groups",
            token,
            &user_group_fields(
                &g.user_group_id,
                &g.name,
                &g.directory_groups.join("\n"),
                &g.logins.join("\n"),
            ),
            "Save",
        ));
    }
    dialogs.push_str(&dialog(
        "new-user-group",
        "New user group",
        "/admin/user-groups#user-groups",
        token,
        &user_group_fields("", "", "", ""),
        "Create",
    ));
    let table = if rows.is_empty() {
        "<p class=\"empty\">No user groups yet.</p>".to_string()
    } else {
        format!("<div class=\"scroll\"><table class=\"list\"><thead><tr><th>Name</th><th>Directory groups</th><th>Logins</th><th></th></tr></thead><tbody>{rows}</tbody></table></div>")
    };
    sections.push(section(
        "user-groups",
        "User groups",
        "People, by the directory groups they are in or by their logins.",
        &new_button("new-user-group", "New user group"),
        format!("{table}{dialogs}"),
    ));

    // ── Account groups ──────────────────────────────────────────────────────
    let account_group_fields = |id: &str, name: &str, members: &[String]| {
        let boxes: String = records
            .accounts
            .iter()
            .filter(|a| a.state != AccountState::Closed as i32 || members.contains(&a.account_id))
            .map(|a| {
                format!(
                    "<label class=\"check\"><input type=\"checkbox\" name=\"account_ids\" value=\"{}\"{}> {}</label>",
                    escape(&a.account_id),
                    if members.contains(&a.account_id) { " checked" } else { "" },
                    escape(&a.name)
                )
            })
            .collect();
        let boxes = if boxes.is_empty() {
            "<p class=\"hint\">No accounts yet: define one under Accounts.</p>".to_string()
        } else {
            format!("<fieldset class=\"checks\"><legend>Accounts</legend>{boxes}</fieldset>")
        };
        format!(
            "<input type=\"hidden\" name=\"account_group_id\" value=\"{}\">\
             <label>Name<input name=\"name\" value=\"{}\" required></label>{boxes}",
            escape(id),
            escape(name)
        )
    };
    let mut rows = String::new();
    let mut dialogs = String::new();
    for g in &records.account_groups {
        let edit = format!("edit-{}", g.account_group_id);
        let members: Vec<String> = g
            .account_ids
            .iter()
            .map(|id| name_of(&account_names, id))
            .collect();
        rows.push_str(&format!(
            "<tr data-id=\"{id}\" data-name=\"{name}\"><td>{named}</td><td>{members}</td>\
             <td class=\"actions\"><button type=\"button\" data-dialog-open=\"{edit}\">Edit</button></td></tr>",
            id = escape(&g.account_group_id),
            name = escape(&g.name),
            named = named(&g.name, &g.account_group_id),
            members = escape(&members.join(", ")),
            edit = escape(&edit),
        ));
        dialogs.push_str(&dialog(
            &escape(&edit),
            &format!("Edit {}", escape(&g.name)),
            "/admin/account-groups#account-groups",
            token,
            &account_group_fields(&g.account_group_id, &g.name, &g.account_ids),
            "Save",
        ));
    }
    dialogs.push_str(&dialog(
        "new-account-group",
        "New account group",
        "/admin/account-groups#account-groups",
        token,
        &account_group_fields("", "", &[]),
        "Create",
    ));
    let table = if rows.is_empty() {
        "<p class=\"empty\">No account groups yet.</p>".to_string()
    } else {
        format!("<div class=\"scroll\"><table class=\"list\"><thead><tr><th>Name</th><th>Accounts</th><th></th></tr></thead><tbody>{rows}</tbody></table></div>")
    };
    sections.push(section(
        "account-groups",
        "Account groups",
        "Accounts gathered, so a permission can name them together.",
        &new_button("new-account-group", "New account group"),
        format!("{table}{dialogs}"),
    ));

    // ── Access groups ───────────────────────────────────────────────────────
    let access_group_fields = |id: &str, name: &str, entries: &str| {
        format!(
            "<input type=\"hidden\" name=\"access_group_id\" value=\"{}\">\
             <label>Name<input name=\"name\" value=\"{}\" required></label>\
             <label>Entries, one per line<textarea name=\"entries\" rows=\"4\" \
             placeholder=\"snaptrade-1 read\">{}</textarea></label>\
             <p class=\"hint\">Each line is a plugin instance and <code>read</code> \
             or <code>write</code>: read to see what it shows, write to act through it too.</p>",
            escape(id),
            escape(name),
            escape(entries)
        )
    };
    let mut rows = String::new();
    let mut dialogs = String::new();
    for g in &records.access_groups {
        let entries = if g.built_in {
            "the dashboard, and every account".to_string()
        } else {
            g.entries
                .iter()
                .map(|e| format!("{} {}", e.plugin_instance_id, level_name(e.level)))
                .collect::<Vec<_>>()
                .join("; ")
        };
        let action = if g.built_in {
            "<span class=\"pill\">built in</span>".to_string()
        } else {
            let edit = format!("edit-{}", g.access_group_id);
            dialogs.push_str(&dialog(
                &escape(&edit),
                &format!("Edit {}", escape(&g.name)),
                "/admin/access-groups#access-groups",
                token,
                &access_group_fields(
                    &g.access_group_id,
                    &g.name,
                    &g.entries
                        .iter()
                        .map(|e| format!("{} {}", e.plugin_instance_id, level_name(e.level)))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                "Save",
            ));
            format!(
                "<button type=\"button\" data-dialog-open=\"{}\">Edit</button>",
                escape(&edit)
            )
        };
        rows.push_str(&format!(
            "<tr data-id=\"{id}\" data-name=\"{name}\"><td>{named}</td><td>{entries}</td>\
             <td class=\"actions\">{action}</td></tr>",
            id = escape(&g.access_group_id),
            name = escape(&g.name),
            named = named(&g.name, &g.access_group_id),
            entries = escape(&entries),
        ));
    }
    dialogs.push_str(&dialog(
        "new-access-group",
        "New access group",
        "/admin/access-groups#access-groups",
        token,
        &access_group_fields("", "", ""),
        "Create",
    ));
    let table = if rows.is_empty() {
        "<p class=\"empty\">No access groups yet.</p>".to_string()
    } else {
        format!("<div class=\"scroll\"><table class=\"list\"><thead><tr><th>Name</th><th>Gives</th><th></th></tr></thead><tbody>{rows}</tbody></table></div>")
    };
    sections.push(section(
        "access-groups",
        "Access groups",
        "What a permission gives: plugins, each at read or write.",
        &new_button("new-access-group", "New access group"),
        format!("{table}{dialogs}"),
    ));

    // ── Accounts ────────────────────────────────────────────────────────────
    // W6.3: a name, and optionally a custodian, a type, an owner and a note,
    // free text and all searchable. An edit sets all four as given.
    let account_fields = |a: Option<&AccountRecord>| {
        let a = a.cloned().unwrap_or_default();
        format!(
            "<input type=\"hidden\" name=\"account_id\" value=\"{id}\">\
             <label>Name<input name=\"name\" value=\"{name}\" required></label>\
             <label>Custodian<input name=\"custodian\" value=\"{custodian}\" maxlength=\"{label}\" \
             placeholder=\"Where it is held, e.g. Fidelity\"></label>\
             <label>Type<input name=\"account_type\" value=\"{account_type}\" maxlength=\"{label}\" \
             placeholder=\"What it is, e.g. Roth IRA\"></label>\
             <label>Owner<input name=\"owner\" value=\"{owner}\" maxlength=\"{label}\" \
             placeholder=\"One ownership or grouping label, e.g. Fund I\"></label>\
             <label>Note<textarea name=\"note\" rows=\"3\" maxlength=\"{note_most}\">{note}</textarea></label>\
             <p class=\"hint\">All but the name are optional and free text, and the search box \
             finds an account by any of them. Leaving one empty clears it.</p>",
            id = escape(&a.account_id),
            name = escape(&a.name),
            custodian = escape(&a.custodian),
            account_type = escape(&a.account_type),
            owner = escape(&a.owner),
            note = escape(&a.note),
            label = account::LABEL_MOST,
            note_most = account::NOTE_MOST,
        )
    };
    let mut rows = String::new();
    let mut dialogs = String::new();
    for a in &records.accounts {
        let closed = a.state == AccountState::Closed as i32;
        let edit = format!("edit-{}", a.account_id);
        let actions = if closed {
            String::new()
        } else {
            dialogs.push_str(&dialog(
                &escape(&edit),
                &format!("Edit {}", escape(&a.name)),
                "/admin/accounts#accounts",
                token,
                &account_fields(Some(a)),
                "Save",
            ));
            format!(
                "<button type=\"button\" data-dialog-open=\"{edit}\">Edit</button>\
                 <form method=\"post\" action=\"/admin/accounts/close#accounts\" \
                 data-confirm=\"Close {name}? A closed account is kept, and nobody works in it.\">{token}\
                 <input type=\"hidden\" name=\"account_id\" value=\"{id}\"><button type=\"submit\">Close</button></form>",
                edit = escape(&edit),
                name = escape(&a.name),
                id = escape(&a.account_id),
            )
        };
        let note = if a.note.is_empty() {
            String::new()
        } else {
            format!("<span class=\"hint\">{}</span>", escape(&a.note))
        };
        rows.push_str(&format!(
            "<tr data-id=\"{id}\" data-name=\"{name}\"><td>{named}{note}</td>\
             <td>{custodian}</td><td>{account_type}</td><td>{owner}</td><td>{state}</td>\
             <td class=\"actions\">{actions}</td></tr>",
            id = escape(&a.account_id),
            name = escape(&a.name),
            named = named(&a.name, &a.account_id),
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
    dialogs.push_str(&dialog(
        "new-account",
        "New account",
        "/admin/accounts#accounts",
        token,
        &account_fields(None),
        "Create",
    ));
    let table = if rows.is_empty() {
        "<p class=\"empty\">No accounts yet.</p>".to_string()
    } else {
        format!(
            "<input class=\"filter\" type=\"search\" data-filter=\"accounts-table\" hidden \
             placeholder=\"Search by name, custodian, type, owner or note\" aria-label=\"Search accounts\">\
             <div class=\"scroll\"><table class=\"list accounts\" id=\"accounts-table\"><thead><tr><th>Name</th>\
             <th>Custodian</th><th>Type</th><th>Owner</th><th>State</th><th></th></tr></thead>\
             <tbody>{rows}</tbody></table></div>"
        )
    };
    sections.push(section(
        "accounts",
        "Accounts",
        "The firm's accounts, which permissions and plugins work on: where each is held, \
         what it is and who owns it. Closed, never deleted.",
        &new_button("new-account", "New account"),
        format!("{table}{dialogs}"),
    ));

    // ── Terminal sessions ───────────────────────────────────────────────────
    // W6.14. Per person: what is being ended is their access from a terminal,
    // so there is no choosing among their sessions to offer.
    let body = if holders.is_empty() {
        "<p class=\"empty\">Nobody holds a terminal session.</p>".to_string()
    } else {
        let rows: String = holders
            .iter()
            .map(|(login, name, count)| {
                format!(
                    "<tr data-id=\"{login}\" data-name=\"{name}\"><td>{named}</td><td>{count}</td><td class=\"actions\">\
                     <form method=\"post\" action=\"/admin/end-terminal-sessions#terminal-sessions\" \
                     data-confirm=\"End {name}'s terminal sessions? Their CLI signs in again.\">{token}\
                     <input type=\"hidden\" name=\"login\" value=\"{login}\">\
                     <button type=\"submit\">End them</button></form></td></tr>",
                    login = escape(login),
                    name = escape(name),
                    named = named(name, login),
                )
            })
            .collect();
        format!("<div class=\"scroll\"><table class=\"list\"><thead><tr><th>Person</th><th>Sessions</th><th></th></tr></thead><tbody>{rows}</tbody></table></div>")
    };
    sections.push(section(
        "terminal-sessions",
        "Terminal sessions",
        "Who is signed in from a terminal, by <code>meridian connect</code>.",
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
        "<div class=\"admin\"><div class=\"page-head\"><h1>Administer this deployment</h1></div>\
         {notice}<nav class=\"tabs\">{tabs}</nav>{}</div>\
         <script>{SCRIPT}</script>",
        sections.concat()
    )
}

/// Tabs, dialogs, and a question before anything destructive. Without it the
/// page is every section at once and every form posts as it did.
const SCRIPT: &str = r##"(function () {
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
  document.addEventListener("click", function (event) {
    var opener = event.target.closest("[data-dialog-open]");
    if (opener) {
      var dialog = document.getElementById(opener.getAttribute("data-dialog-open"));
      if (dialog) { dialog.showModal(); var first = dialog.querySelector("input:not([type=hidden]), select, textarea"); if (first) first.focus(); }
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
