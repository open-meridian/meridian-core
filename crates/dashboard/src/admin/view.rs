//! The admin view of one plugin instance, `/admin/plugins/{instance}`
//! (kernel/a-plugins-admin-view; spec/plugin-pages-share-one-kit.md, Q5):
//! the tabs every plugin has -- Overview, its health and what it still needs;
//! Settings, its form; Access, who may use it; and Versions, Activity, Grants
//! & scope, Usage and Diagnostics as they are built
//! (kernel/the-admin-view-tracks-a-plugin) -- and a link to the plugin's own
//! area, where its pages are (W6.9, sdk-contract/a-plugin-has-admins). It
//! frames none of the plugin's pages: they moved to the area at
//! `/plugins/{instance}`, reached from the home, one tab row per level.
//!
//! Shown to the plugin's admins -- a deployment admin being one through All
//! plugins (admin) -- and to a deployment admin, who reaches what is theirs
//! on it: its health, and granting on Access. Settings is an admin of the
//! plugin's (W6.11); a plugin admin reads Access and changes nothing on it,
//! since only a deployment admin grants (decisions/027).
//!
//! A tab is a link, `?tab=settings`, so each opens directly and none needs
//! script: the page holds only the tab asked for.
//!
//! The admin overview's Plugins tab lists every instance with the same line
//! this view heads with: its health, what its settings still need, and how
//! many of its external accounts nothing links (W6.10). Each plugin links its
//! own external accounts on its pages at `admin` (W6.4); the count leads
//! there.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use meridian_access::{AccessLevel, ALL_PLUGINS_ADMIN};
use meridian_domain::v1::{AccessRecords, PluginReport, PluginSettingsRecord};

use crate::custody::{quiet, remedy, utc, Heard};
use crate::health::{self, State};
use crate::html::escape;

use super::settings;

/// Where an instance's admin view is. The instance goes in the path, so it
/// is escaped where it is written into a page.
pub fn path(instance: &str) -> String {
    format!("/admin/plugins/{instance}")
}

/// The view's tabs.
pub const OVERVIEW: &str = "overview";
pub const SETTINGS: &str = "settings";
pub const ACCESS: &str = "access";

/// One tab: what the query names it by, and what it is called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    pub key: String,
    pub title: String,
}

/// The tabs every plugin has, as far as they are built, for this viewer:
/// Settings only for an admin of the plugin (W6.11), and after it a tab for
/// each table setting it declares, titled with its label (the product owner,
/// 2026-10-05).
pub fn tabs(may_set: bool, record: Option<&PluginSettingsRecord>, development: bool) -> Vec<Tab> {
    let tab = |key: &str, title: &str| Tab {
        key: key.into(),
        title: title.into(),
    };
    let mut tabs = vec![tab(OVERVIEW, "Overview")];
    if may_set {
        tabs.push(tab(SETTINGS, "Settings"));
        for table in record
            .map(|record| settings::tables(record, development))
            .unwrap_or_default()
        {
            tabs.push(tab(&settings::table_key(table), settings::label(table)));
        }
    }
    tabs.push(tab(ACCESS, "Access"));
    tabs
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

/// The plugin's area at Manage, where its pages at `admin` are, and where it
/// links its external accounts (W6.4, W6.9).
pub fn area_at_admin(instance: &str) -> String {
    crate::area::href(instance, AccessLevel::Admin, None)
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
    /// Values its source sent that it could not convert, by scheme and code,
    /// with how many of its accounts carry each (contract v11): counted for a
    /// person to map, and the evidence the contract may need to grow.
    pub failed: Vec<(String, String, usize)>,
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
            failed: custody.failed_conversions(instance),
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

/// The badge for a plugin's state, its detail the badge's note on hover
/// ([`crate::html::noted_badge`]): the badge, then the note, whose id is
/// `id`, for the caller to place.
pub fn state_badge(state: &State, id: &str) -> (String, String) {
    crate::html::noted_badge(
        if state.good {
            "badge good"
        } else {
            "badge warn"
        },
        state.word,
        &state.detail,
        id,
    )
}

/// What still needs an admin of the plugin, as flags linking to where it is
/// done. The overview says what the settings need in a column of its own,
/// so only the view flags them; in the view, `admin_pages` is the plugin's
/// area at Manage, where its pages at `admin` link its external accounts.
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
            // Each plugin links its own, on its pages at admin (W6.4).
            Some(pages) => format!(
                "{}. <a href=\"{}\">Link {} on the plugin's pages, under Manage</a>.",
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
    if !line.failed.is_empty() {
        let total: usize = line.failed.iter().map(|(_, _, count)| count).sum();
        let codes = line
            .failed
            .iter()
            .map(|(scheme, code, count)| format!("{scheme} {code} ({count})"))
            .collect::<Vec<_>>()
            .join(", ");
        flags.push_str(&format!(
            "<p class=\"flag\" data-flag=\"as-reported\" data-count=\"{total}\">{} its \
             source sent did not convert, and travel as reported: {}. A person maps them; each \
             is evidence the contract may need to grow.</p>",
            plural(total, "value", "values"),
            escape(&codes)
        ));
    }
    flags
}

/// Who may use the instance: each user group an access group gives it to,
/// at its level and, for `read` and `write`, on its accounts -- `admin`
/// first, then `write`, then `read`; and the user groups linked to All
/// plugins (admin), who administer it and every plugin. The levels are the
/// plugin's, the same three for every plugin, since a plugin declares no tags
/// (decisions/026, 027). A deployment admin is offered the way to grant.
fn access(records: &AccessRecords, instance: &str, may_grant: bool) -> String {
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
    let row = |order: u8,
               permission: &meridian_domain::v1::Permission,
               through: &str,
               level: &str| {
        let accounts = if level == "admin" {
            "<span class=\"id\">no account</span>".to_string()
        } else {
            escape(&name(&permission.account_group_id, &account_groups))
        };
        (
            order,
            format!(
                "<tr data-user-group=\"{ug}\" data-level=\"{level}\">\
                 <td><span class=\"name\">{user}</span><span class=\"id\">through {access}</span></td>\
                 <td><span class=\"badge\">{level}</span></td><td>{accounts}</td></tr>",
                ug = escape(&permission.user_group_id),
                user = escape(&name(&permission.user_group_id, &user_groups)),
                access = escape(through),
            ),
        )
    };
    let mut rows: Vec<(u8, String)> = Vec::new();
    for permission in &records.permissions {
        if permission.access_group_id == ALL_PLUGINS_ADMIN {
            rows.push(row(0, permission, "All plugins (admin)", "admin"));
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
            let (order, level) = match AccessLevel::try_from(entry.level) {
                Ok(AccessLevel::Admin) => (0, "admin"),
                Ok(AccessLevel::Write) => (1, "write"),
                Ok(AccessLevel::Read) => (2, "read"),
                _ => continue,
            };
            rows.push(row(order, permission, &group.name, level));
        }
    }
    // Admin, then write, then read, and otherwise in the order the
    // permissions are.
    rows.sort_by_key(|(order, _)| *order);
    let rows: String = rows.into_iter().map(|(_, row)| row).collect();
    let table = if rows.is_empty() {
        "<p class=\"empty\">No user group holds access to it yet.</p>".to_string()
    } else {
        format!(
            "<div class=\"scroll\"><table class=\"list access\"><thead><tr><th>User group</th>\
             <th>Level</th><th>On accounts</th></tr></thead><tbody>{rows}</tbody></table></div>"
        )
    };
    let hint = "<p class=\"hint\">Admin configures the plugin and reaches no account's data; \
                write and read reach the accounts of the account group granted. A deployment \
                admin holds nothing on it by being one.</p>";
    let grant = if may_grant {
        format!(
            "<p><a href=\"/admin#permissions\">Grant a user group access</a> through an \
             access group naming <code>{}</code>.</p>",
            escape(instance)
        )
    } else {
        "<p class=\"hint\">A deployment admin grants access.</p>".to_string()
    };
    format!("{table}{hint}{grant}")
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
        .enumerate()
        .map(|(row, ((_, external), status))| {
            let (state, what_to_do) = remedy(status);
            // What the plugin said of it, the state's note on hover.
            let (pill, detail) = crate::html::noted_badge(
                if quiet(status) {
                    "pill good"
                } else {
                    "pill warn"
                },
                state,
                &status.status_detail,
                &format!("sync-note-{row}"),
            );
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
                 <td>{pill}{detail}</td><td class=\"remedy\">{what_to_do}</td>\
                 <td>{holdings}</td></tr>",
                id = escape(external),
                holdings = escape(&utc(status.holdings_as_of_ns)),
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
         <th>Holdings as of</th></tr></thead><tbody>{rows}</tbody></table></div></section>"
    )
}

fn health_panel(line: &Line, report: Option<&PluginReport>, admin_pages: Option<&str>) -> String {
    // Why it is as it is, the badge's note: a line under the heading without
    // script, and with it, in the bubble.
    let (badge, detail) = state_badge(&line.state, "health-note");
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
        flags = flags(line, admin_pages),
    )
}

/// What the view shows: the tabs, and the one asked for.
pub struct View<'a> {
    pub line: &'a Line,
    /// What its table settings' columns offer (W6.11, contract v14).
    pub choices: &'a settings::Choices,
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
    /// The plugin's area at Manage, for a viewer who administers it.
    pub area: Option<String>,
    /// Whether the viewer is a deployment admin, who grants.
    pub may_grant: bool,
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

pub fn render(view: &View) -> String {
    let line = view.line;
    let instance = escape(&line.instance);
    let title = escape(line.name.as_deref().unwrap_or(&line.instance));
    // One of its table settings, where the tab asked for is one's.
    let table = view.record.and_then(|record| {
        settings::table_named(record, &view.current.key, view.development)
            .map(|table| (record, table))
    });
    let body = match (view.current.key.as_str(), table) {
        (SETTINGS, _) => super::settings_section(
            view.records,
            view.record,
            view.token,
            view.development,
            &settings::path(&line.instance),
            view.notice,
        ),
        (key, Some((record, table))) => super::table_section(
            view.records,
            record,
            table,
            view.token,
            &super::with_tab(&settings::path(&line.instance), key),
            view.choices,
            view.notice,
        ),
        (ACCESS, _) => format!(
            "<section class=\"panel padded\" id=\"access\"><h2>Who has access</h2>{}</section>",
            access(view.records, &line.instance, view.may_grant)
        ),
        _ => format!(
            "<div class=\"stack\">{}{}</div>",
            health_panel(line, view.report, view.area.as_deref()),
            connections(&line.instance, view.custody, view.records)
        ),
    };
    // The plugin's own pages are in its area, from the home (W6.9).
    let area = view
        .area
        .as_deref()
        .map(|area| {
            format!(
                "<div class=\"actions\"><a class=\"button\" href=\"{}\" data-area>Its pages</a></div>",
                escape(area)
            )
        })
        .unwrap_or_default();
    format!(
        "<div class=\"plugin-view\"><div class=\"page-head\"><div><h1>{title}</h1><p><code>{instance}</code> &middot; \
         the plugin's health, settings and access; its own pages are in its area.</p></div>{area}</div>\
         {nav}<div class=\"tab-body\" data-current=\"{current}\">{body}</div></div>",
        nav = nav(&line.instance, view.tabs, view.current),
        current = escape(&view.current.key),
    )
}
