//! The admin view of one plugin instance, `/admin/plugins/{instance}`
//! (kernel/a-plugins-admin-view; spec/plugin-pages-share-one-kit.md, Q5),
//! in tabs (W6.9, the product owner, 2026-09-29): Overview, its health and
//! what it still needs; Settings, its form; Access, who may use it; then one
//! tab per admin page the plugin declared (W4.8), in its order, each framing
//! that path on the plugin's host, which the plugin serves to deployment
//! admins alone by the claim that says they are one. A plugin declaring none
//! gets one tab framing its `/admin`.
//!
//! A tab is a link, `?tab=settings` or, for a plugin's page, `?tab=` its
//! title in lower case (`?tab=accounts`; the product owner, 2026-09-29), so
//! each opens directly and none needs script: the page holds only the tab
//! asked for, and frames only that page.
//!
//! The admin overview's Plugins tab lists every instance with the same line
//! this view heads with: its health, what its settings still need, and how
//! many of its external accounts nothing links (W6.10). Each plugin links its
//! own external accounts on its admin pages (W6.4); the count leads there.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use meridian_access::DEPLOYMENT_ADMIN;
use meridian_domain::v1::{AccessLevel, AccessRecords, PluginReport, PluginSettingsRecord};

use crate::custody::{quiet, remedy, utc, Heard};
use crate::health::{self, State};
use crate::html::escape;

use super::settings;

/// Where an instance's admin view is. The instance goes in the path, so it
/// is escaped where it is written into a page.
pub fn path(instance: &str) -> String {
    format!("/admin/plugins/{instance}")
}

/// The view's own tabs, before the plugin's pages.
pub const OVERVIEW: &str = "overview";
pub const SETTINGS: &str = "settings";
pub const ACCESS: &str = "access";
/// What a plugin declaring no admin page is framed at.
const ADMIN: &str = "/admin";

/// One tab: what the query names it by, what it is called, and for one of
/// the plugin's pages, the path framed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    pub key: String,
    pub title: String,
    pub page: Option<String>,
}

/// The view's tabs, then the plugin's admin pages as its report declares
/// them, in its order. A page whose path is not one on the plugin's host is
/// left out, as is a second tab for a path already shown; none left, and the
/// plugin's `/admin` is the one.
pub fn tabs(report: Option<&PluginReport>) -> Vec<Tab> {
    let own = |key: &str, title: &str| Tab {
        key: key.into(),
        title: title.into(),
        page: None,
    };
    let mut tabs = vec![
        own(OVERVIEW, "Overview"),
        own(SETTINGS, "Settings"),
        own(ACCESS, "Access"),
    ];
    let declared = report
        .and_then(|report| report.declared_interface.as_ref())
        .map(|interface| interface.admin_pages.as_slice())
        .unwrap_or_default();
    let mut pages: Vec<Tab> = Vec::new();
    for page in declared {
        let path = page.path.trim();
        if crate::plugins::page_path(path).is_err()
            || pages.iter().any(|t| t.page.as_deref() == Some(path))
        {
            continue;
        }
        let title = page.title.trim();
        let title = if title.is_empty() { path } else { title };
        let key = unique(slug(title), &tabs, &pages);
        pages.push(Tab {
            key,
            title: title.into(),
            page: Some(path.into()),
        });
    }
    if pages.is_empty() {
        pages.push(Tab {
            key: "admin".into(),
            title: "Admin page".into(),
            page: Some(ADMIN.into()),
        });
    }
    tabs.extend(pages);
    tabs
}

/// A title as the query names its tab: lower case, letters and digits, runs of
/// anything else one hyphen. "Accounts" is `accounts`, "Cash ladder" is
/// `cash-ladder`; a title with none of either is `page`.
fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-');
    if out.is_empty() { "page".into() } else { out.into() }
}

/// `key`, or `key-2`, `key-3` and on, whichever no tab already has, so a
/// plugin page called "Settings" does not take the view's own.
fn unique(key: String, own: &[Tab], pages: &[Tab]) -> String {
    let taken = |k: &str| own.iter().chain(pages).any(|t| t.key == k);
    if !taken(&key) {
        return key;
    }
    (2..)
        .map(|n| format!("{key}-{n}"))
        .find(|k| !taken(k))
        .expect("an unused key")
}

/// The tab asked for, or the first when none is, or one that is not there.
pub fn chosen<'a>(tabs: &'a [Tab], asked: &str) -> &'a Tab {
    tabs.iter().find(|tab| tab.key == asked).unwrap_or(&tabs[0])
}

/// Where a tab is: the view itself for the first, and a query for the rest.
pub fn tab_href(instance: &str, key: &str) -> String {
    if key == OVERVIEW {
        return path(instance);
    }
    let mut url = reqwest::Url::parse("http://dashboard.invalid/").expect("a fixed address");
    url.query_pairs_mut().append_pair("tab", key);
    format!("{}?{}", path(instance), url.query().unwrap_or_default())
}

/// What is known of one instance, from wherever it is said: the conductor's
/// record of its settings, its sidecar's report, the catalogue's launch, and
/// what its connector reported.
pub struct Line {
    pub instance: String,
    /// The plugin it runs, where the catalogue launched it.
    pub name: Option<String>,
    pub state: State,
    /// Required settings still to fill in, by label.
    pub missing: Vec<String>,
    /// External accounts it reported or was refused for that nothing links.
    pub unlinked: usize,
}

/// A line for every instance known anywhere, by instance.
pub fn lines(
    records: &AccessRecords,
    reports: &BTreeMap<String, PluginReport>,
    names: &HashMap<String, String>,
    custody: &Heard,
    development: bool,
    now: i64,
) -> Vec<Line> {
    let mut known: BTreeSet<&str> = BTreeSet::new();
    known.extend(
        records
            .plugin_settings
            .iter()
            .map(|r| r.plugin_instance_id.as_str()),
    );
    known.extend(reports.keys().map(String::as_str));
    known.extend(names.keys().map(String::as_str));
    known.extend(custody.reported.keys().map(String::as_str));
    let unlinked = custody.unlinked(&records.links);
    known
        .into_iter()
        .map(|instance| Line {
            instance: instance.to_string(),
            name: names.get(instance).cloned(),
            state: health::state(reports.get(instance), now),
            missing: record_of(records, instance)
                .map(|record| {
                    settings::missing(record, development)
                        .into_iter()
                        .map(|d| settings::label(d).to_string())
                        .collect()
                })
                .unwrap_or_default(),
            unlinked: unlinked
                .iter()
                .filter(|u| u.plugin_instance_id == instance)
                .count(),
        })
        .collect()
}

pub fn record_of<'a>(
    records: &'a AccessRecords,
    instance: &str,
) -> Option<&'a PluginSettingsRecord> {
    records
        .plugin_settings
        .iter()
        .find(|record| record.plugin_instance_id == instance)
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// The badge for a plugin's state.
pub fn state_badge(state: &State) -> String {
    format!(
        "<span class=\"badge {}\">{}</span>",
        if state.good { "good" } else { "warn" },
        escape(state.word)
    )
}

/// What still needs a deployment admin, as flags linking to where it is
/// done. The overview says what the settings need in a column of its own,
/// so only the view flags them; in the view, `admin_pages` is where the
/// plugin's own admin pages start, where it links its external accounts.
pub fn flags(line: &Line, admin_pages: Option<&str>) -> String {
    let view = path(&line.instance);
    let mut flags = String::new();
    if admin_pages.is_some() && !line.missing.is_empty() {
        flags.push_str(&format!(
            "<p class=\"flag\" data-flag=\"settings\">Needs {}: <a href=\"{}\">fill in its settings</a>.</p>",
            escape(&line.missing.join(", ")),
            escape(&tab_href(&line.instance, SETTINGS)),
        ));
    }
    if line.unlinked > 0 {
        let said = format!(
            "{} not linked",
            plural(line.unlinked, "external account", "external accounts")
        );
        let link = match admin_pages {
            // Each plugin links its own, on its admin pages (W6.4).
            Some(pages) => format!(
                "{}. <a href=\"{}\">Link {} on the plugin's admin pages</a>.",
                escape(&said),
                escape(pages),
                if line.unlinked == 1 { "it" } else { "them" }
            ),
            None => format!("<a href=\"{}\">{}</a>", escape(&view), escape(&said)),
        };
        flags.push_str(&format!(
            "<p class=\"flag\" data-flag=\"unlinked\" data-count=\"{}\">{link}</p>",
            line.unlinked
        ));
    }
    flags
}

/// Who may use the instance: each user group an access group gives it to,
/// with the tag, the level and the accounts; and who opens it as a
/// deployment admin.
fn access(records: &AccessRecords, instance: &str) -> String {
    let name = |id: &str, of: &[(&str, &str)]| {
        of.iter()
            .find(|(held, _)| *held == id)
            .map(|(_, name)| name.to_string())
            .unwrap_or_else(|| id.to_string())
    };
    let user_groups: Vec<(&str, &str)> = records
        .user_groups
        .iter()
        .map(|g| (g.user_group_id.as_str(), g.name.as_str()))
        .collect();
    let account_groups: Vec<(&str, &str)> = records
        .account_groups
        .iter()
        .map(|g| (g.account_group_id.as_str(), g.name.as_str()))
        .collect();
    let mut rows = String::new();
    let mut admins = Vec::new();
    for permission in &records.permissions {
        if permission.access_group_id == DEPLOYMENT_ADMIN {
            admins.push(name(&permission.user_group_id, &user_groups));
            continue;
        }
        let Some(group) = records
            .access_groups
            .iter()
            .find(|g| g.access_group_id == permission.access_group_id)
        else {
            continue;
        };
        for entry in group
            .entries
            .iter()
            .filter(|e| e.plugin_instance_id == instance)
        {
            let level = if entry.level == AccessLevel::Write as i32 {
                "write"
            } else {
                "read"
            };
            rows.push_str(&format!(
                "<tr data-user-group=\"{ug}\" data-tag=\"{tag}\" data-level=\"{level}\">\
                 <td><span class=\"name\">{user}</span><span class=\"id\">through {access}</span></td>\
                 <td><code>{tag}</code> <span class=\"badge\">{level}</span></td><td>{accounts}</td></tr>",
                ug = escape(&permission.user_group_id),
                user = escape(&name(&permission.user_group_id, &user_groups)),
                access = escape(&group.name),
                tag = escape(&entry.tag),
                accounts = escape(&name(&permission.account_group_id, &account_groups)),
            ));
        }
    }
    let table = if rows.is_empty() {
        "<p class=\"empty\">No user group holds access to it yet.</p>".to_string()
    } else {
        format!(
            "<div class=\"scroll\"><table class=\"list access\"><thead><tr><th>User group</th>\
             <th>Tag</th><th>On accounts</th></tr></thead><tbody>{rows}</tbody></table></div>"
        )
    };
    let admins = if admins.is_empty() {
        String::new()
    } else {
        admins.sort();
        admins.dedup();
        format!(
            "<p class=\"hint\">Deployment admins open it too, holding nothing on it by that: {}.</p>",
            escape(&admins.join(", "))
        )
    };
    format!(
        "{table}{admins}<p><a href=\"/admin#permissions\">Grant a user group access</a> through an \
         access group naming <code>{}</code>.</p>",
        escape(instance)
    )
}

/// Each of the instance's external accounts with the sync state it last
/// reported (W2.1): what it is, whose fix it is, and the account it is
/// linked to, if any. Shown on its overview, beside its health, since the
/// connection's state is the plugin's own; nothing is linked from here (W6.4).
fn connections(instance: &str, custody: &Heard, records: &AccessRecords) -> String {
    let account_name = |id: &str| {
        records
            .accounts
            .iter()
            .find(|account| account.account_id == id)
            .map(|account| account.name.as_str())
            .unwrap_or(id)
            .to_string()
    };
    let rows: String = custody
        .sync
        .iter()
        .filter(|((held_by, _), _)| held_by == instance)
        .map(|((_, external), status)| {
            let (state, what_to_do) = remedy(status);
            let account = if status.account_id.is_empty() {
                "<span class=\"pill\">not linked</span>".to_string()
            } else {
                format!(
                    "<span class=\"name\">{}</span><span class=\"id\">{}</span>",
                    escape(&account_name(&status.account_id)),
                    escape(&status.account_id)
                )
            };
            format!(
                "<tr data-id=\"{id}\" data-state=\"{state}\"><td>{id}</td><td>{account}</td>\
                 <td><span class=\"pill{tone}\">{state}</span></td><td class=\"remedy\">{what_to_do}</td>\
                 <td>{holdings}</td><td>{detail}</td></tr>",
                id = escape(external),
                tone = if quiet(status) { " good" } else { " warn" },
                holdings = escape(&utc(status.holdings_as_of_ns)),
                detail = escape(&status.status_detail),
            )
        })
        .collect();
    if rows.is_empty() {
        return String::new();
    }
    format!(
        "<section class=\"panel padded\" id=\"connections\"><h2>Connections</h2>\
         <p class=\"hint\">Each external account's sync state, as the plugin last said it, and \
         whose fix it is.</p><div class=\"scroll\"><table class=\"list sync\"><thead><tr>\
         <th>External account</th><th>Account</th><th>State</th><th>What to do</th>\
         <th>Holdings as of</th><th>Detail</th></tr></thead><tbody>{rows}</tbody></table></div></section>"
    )
}

fn health_panel(line: &Line, report: Option<&PluginReport>, admin_pages: &str) -> String {
    let detail = if line.state.detail.is_empty() {
        String::new()
    } else {
        format!("<p>{}</p>", escape(&line.state.detail))
    };
    let facts = match report {
        None => String::new(),
        Some(report) => {
            let refusals = if report.refused_grants == 0 {
                "none".to_string()
            } else {
                format!(
                    "{}; the latest: {}",
                    report.refused_grants,
                    if report.last_refusal_reason.is_empty() {
                        "not said"
                    } else {
                        &report.last_refusal_reason
                    }
                )
            };
            format!(
                "<dl class=\"facts\"><dt>Registered</dt><dd>{}</dd>\
                 <dt>Last heartbeat</dt><dd>{}</dd><dt>Reported</dt><dd>{}</dd>\
                 <dt>Contract</dt><dd>{}</dd><dt>Roles</dt><dd>{}</dd>\
                 <dt>Grants refused</dt><dd>{}</dd></dl>",
                if report.registered { "yes" } else { "no" },
                escape(&utc(report.last_heartbeat_at_ns)),
                escape(&utc(report.reported_at_ns)),
                escape(if report.contract_version.is_empty() {
                    "not said"
                } else {
                    &report.contract_version
                }),
                escape(&if report.roles.is_empty() {
                    "none".to_string()
                } else {
                    report.roles.join(", ")
                }),
                escape(&refusals),
            )
        }
    };
    format!(
        "<section class=\"panel padded\" id=\"health\"><div class=\"row\"><h2>Health</h2>{badge}</div>\
         {detail}{facts}{flags}</section>",
        badge = state_badge(&line.state),
        flags = flags(line, Some(admin_pages)),
    )
}

/// What the view shows: the tabs, the one asked for, and for one of the
/// plugin's pages, how this dashboard can show it.
pub struct View<'a> {
    pub line: &'a Line,
    pub record: Option<&'a PluginSettingsRecord>,
    pub report: Option<&'a PluginReport>,
    pub records: &'a AccessRecords,
    /// What its connector says of its connections (W2.1).
    pub custody: &'a Heard,
    pub token: &'a str,
    pub notice: &'a str,
    pub development: bool,
    pub tabs: &'a [Tab],
    pub current: &'a Tab,
    /// The current tab's page, when it is one of the plugin's.
    pub admin_page: Option<AdminPage>,
}

/// The plugin's own admin page, as this dashboard can show it.
pub enum AdminPage {
    /// Framed: the frame's way in, and the plugin's origin its theme goes to.
    Framed { src: String, origin: String },
    /// A link to it in a window of its own, where no frame keeps its session.
    Linked(String),
    /// Not at all, and why.
    None(&'static str),
}

fn nav(instance: &str, tabs: &[Tab], current: &Tab) -> String {
    let links: String = tabs
        .iter()
        .map(|tab| {
            let here = tab.key == current.key;
            format!(
                "<a href=\"{href}\" data-tab=\"{key}\"{here}>{title}</a>",
                href = escape(&tab_href(instance, &tab.key)),
                key = escape(&tab.key),
                here = if here {
                    " class=\"here\" aria-current=\"page\""
                } else {
                    ""
                },
                title = escape(&tab.title),
            )
        })
        .collect();
    format!("<nav class=\"tabs view-tabs\" aria-label=\"The plugin's admin\">{links}</nav>")
}

fn page_panel(view: &View, title: &str, tab: &Tab) -> String {
    let shown = match &view.admin_page {
        Some(AdminPage::Framed { src, origin }) => format!(
            "<iframe class=\"admin-frame\" src=\"{}\" title=\"{} &middot; {}\" data-plugin-frame \
             data-origin=\"{}\"></iframe>",
            escape(src),
            title,
            escape(&tab.title),
            escape(origin)
        ),
        Some(AdminPage::Linked(href)) => format!(
            "<p class=\"empty\">This dashboard's address has no domain, so a browser keeps no \
             framed page's session. <a href=\"{}\" target=\"_blank\" rel=\"noopener\">Open \
             {}</a> in a window of its own.</p>",
            escape(href),
            escape(&tab.title)
        ),
        Some(AdminPage::None(why)) => format!("<p class=\"empty\">{}</p>", escape(why)),
        None => String::new(),
    };
    format!(
        "<section class=\"panel padded\" id=\"admin-page\" data-page=\"{path}\">\
         <div class=\"row\"><h2>{name}</h2><code>{path}</code></div>\
         <p class=\"hint\">The plugin's own page, served to deployment admins alone.</p>\
         {shown}</section>",
        name = escape(&tab.title),
        path = escape(tab.page.as_deref().unwrap_or_default()),
    )
}

pub fn render(view: &View) -> String {
    let line = view.line;
    let instance = escape(&line.instance);
    let title = escape(line.name.as_deref().unwrap_or(&line.instance));
    let notice = if view.notice.is_empty() {
        String::new()
    } else {
        format!("<p class=\"notice good\">{}</p>", escape(view.notice))
    };
    let first_page = view
        .tabs
        .iter()
        .find(|tab| tab.page.is_some())
        .map(|tab| tab_href(&line.instance, &tab.key))
        .unwrap_or_default();
    let body = match view.current.key.as_str() {
        SETTINGS => {
            let form = match view.record {
                Some(record) => settings::form(record, view.token, view.development),
                None => "<p class=\"empty\">Its settings are not known yet: the plugin has not \
                         reported what it needs.</p>"
                    .to_string(),
            };
            format!(
                "{notice}<section class=\"panel padded\" id=\"settings\"><h2>Settings</h2>\
                 <p class=\"hint\">What the plugin declared it needs. A secret is never shown again \
                 once set: type a new value to replace it.</p>{form}</section>"
            )
        }
        ACCESS => format!(
            "<section class=\"panel padded\" id=\"access\"><h2>Who has access</h2>{}</section>",
            access(view.records, &line.instance)
        ),
        _ if view.current.page.is_some() => page_panel(view, &title, view.current),
        _ => format!(
            "<div class=\"stack\">{}{}</div>",
            health_panel(line, view.report, &first_page),
            connections(&line.instance, view.custody, view.records)
        ),
    };
    format!(
        "<div class=\"plugin-view\"><div class=\"page-head\"><div><h1>{title}</h1><p><code>{instance}</code> &middot; \
         the plugin's health, settings and access, and its own admin pages.</p></div>\
         <div class=\"actions\"><a class=\"button\" href=\"/admin#plugins\">All plugins</a>\
         <a class=\"button primary\" href=\"/plugins/{instance}\">Open its page</a></div></div>\
         {nav}<div class=\"tab-body\" data-current=\"{current}\">{body}</div></div>",
        nav = nav(&line.instance, view.tabs, view.current),
        current = escape(&view.current.key),
    )
}
