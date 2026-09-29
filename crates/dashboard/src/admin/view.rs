//! The admin view of one plugin instance, `/admin/plugins/{instance}`
//! (kernel/a-plugins-admin-view; spec/plugin-pages-share-one-kit.md, Q5):
//! its settings form, its health, who has access to it, and the plugin's own
//! admin page framed, which the plugin serves to deployment admins alone by
//! the claim that says they are one (W6.9).
//!
//! The admin overview's Plugins tab lists every instance with the same line
//! this view heads with: its health, what its settings still need, and how
//! many of its external accounts nothing links (W6.10). Each plugin links its
//! own external accounts from its admin page, by the product owner's ruling
//! (point 8); until it can, the overview's External accounts tab still does.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use meridian_access::DEPLOYMENT_ADMIN;
use meridian_domain::v1::{AccessLevel, AccessRecords, PluginReport, PluginSettingsRecord};

use crate::custody::{utc, Heard};
use crate::health::{self, State};
use crate::html::escape;

use super::settings;

/// Where an instance's admin view is. The instance goes in the path, so it
/// is escaped where it is written into a page.
pub fn path(instance: &str) -> String {
    format!("/admin/plugins/{instance}")
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
/// so only the view flags them.
pub fn flags(line: &Line, from_the_view: bool) -> String {
    let view = path(&line.instance);
    let mut flags = String::new();
    if from_the_view && !line.missing.is_empty() {
        flags.push_str(&format!(
            "<p class=\"flag\" data-flag=\"settings\">Needs {}: <a href=\"#settings\">fill in its settings</a>.</p>",
            escape(&line.missing.join(", ")),
        ));
    }
    if line.unlinked > 0 {
        let said = format!(
            "{} not linked",
            plural(line.unlinked, "external account", "external accounts")
        );
        let link = if from_the_view {
            // The plugin's own admin page links them (point 8); until it can,
            // the overview's tab does.
            format!(
                "{}. <a href=\"#admin-page\">Link them from the plugin's admin page</a>, or \
                 <a href=\"/admin#external-accounts\">under External accounts</a>.",
                escape(&said)
            )
        } else {
            format!("<a href=\"{}\">{}</a>", escape(&view), escape(&said))
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

fn health_panel(line: &Line, report: Option<&PluginReport>) -> String {
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
        flags = flags(line, true),
    )
}

/// What the view shows beside the form, and the plugin's admin page when
/// this dashboard can frame it.
pub struct View<'a> {
    pub line: &'a Line,
    pub record: Option<&'a PluginSettingsRecord>,
    pub report: Option<&'a PluginReport>,
    pub records: &'a AccessRecords,
    pub token: &'a str,
    pub notice: &'a str,
    pub development: bool,
    pub admin_page: AdminPage,
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

pub fn render(view: &View) -> String {
    let line = view.line;
    let instance = escape(&line.instance);
    let title = escape(line.name.as_deref().unwrap_or(&line.instance));
    let notice = if view.notice.is_empty() {
        String::new()
    } else {
        format!("<p class=\"notice good\">{}</p>", escape(view.notice))
    };
    let form = match view.record {
        Some(record) => settings::form(record, view.token, view.development),
        None => "<p class=\"empty\">Its settings are not known yet: the plugin has not \
                 reported what it needs.</p>"
            .to_string(),
    };
    let admin_page = match &view.admin_page {
        AdminPage::Framed { src, origin } => format!(
            "<iframe class=\"admin-frame\" src=\"{}\" title=\"{} admin page\" data-plugin-frame \
             data-origin=\"{}\"></iframe>",
            escape(src),
            title,
            escape(origin)
        ),
        AdminPage::Linked(href) => format!(
            "<p class=\"empty\">This dashboard's address has no domain, so a browser keeps no \
             framed page's session. <a href=\"{}\" target=\"_blank\" rel=\"noopener\">Open its \
             admin page</a> in a window of its own.</p>",
            escape(href)
        ),
        AdminPage::None(why) => format!("<p class=\"empty\">{}</p>", escape(why)),
    };
    format!(
        "<div class=\"page-head\"><div><h1>{title}</h1><p><code>{instance}</code> &middot; \
         the plugin's settings, health and access, and its own admin page.</p></div>\
         <div class=\"actions\"><a class=\"button\" href=\"/admin#plugins\">All plugins</a>\
         <a class=\"button primary\" href=\"/plugins/{instance}\">Open its page</a></div></div>{notice}\
         <div class=\"view-grid\"><div class=\"stack\">\
         <section class=\"panel padded\" id=\"settings\"><h2>Settings</h2>\
         <p class=\"hint\">What the plugin declared it needs. A secret is never shown again once set: \
         type a new value to replace it.</p>{form}</section>\
         <section class=\"panel padded\" id=\"admin-page\"><h2>The plugin's admin page</h2>\
         <p class=\"hint\">What only the plugin knows, served to deployment admins alone.</p>\
         {admin_page}</section></div>\
         <div class=\"stack\">{health}\
         <section class=\"panel padded\" id=\"access\"><h2>Who has access</h2>{access}</section>\
         </div></div>",
        health = health_panel(line, view.report),
        access = access(view.records, &line.instance),
    )
}
