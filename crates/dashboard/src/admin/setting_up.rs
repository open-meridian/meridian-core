//! "Setting up", a part of every plugin's Summary (contract v20;
//! spec/plugin-pages-follow-their-role, accepted 2026-10-10, Q2; W6.9): the
//! first-use steps of the plugin's role, each done or not and each linking
//! to where it is done, drawn by core from its own records, so a plugin
//! cannot misreport it, as it cannot its status. First under the status
//! while any step is undone; folded to one line, "Set up", when all are,
//! the steps a click away. A step core cannot judge is left to the plugin's
//! own page and counts for nothing here.
//!
//! The steps, per role, are the spec's "First-use path" tables. A step that
//! is somebody else's names whose: a deployment admin sees the link, anyone
//! else "Ask a deployment admin to ...".

use meridian_domain::v1::{AccessRecords, PluginSettingsRecord};
use meridian_pb::v1::AccessLevel;
use serde_json::{json, Value};

use crate::html::escape;

/// What core knows of one instance that the steps are judged by.
#[derive(Debug, Clone, Default)]
pub struct Facts<'a> {
    pub instance: &'a str,
    pub roles: Vec<String>,
    pub record: Option<&'a PluginSettingsRecord>,
    pub records: Option<&'a AccessRecords>,
    pub healthy: bool,
    pub deployment_admin: bool,
    /// For a custody plugin: whether any external account's sync is heard,
    /// and how many it reported that nothing links.
    pub connected: bool,
    pub unlinked: usize,
    /// For a data plugin: whether every dataset it declares is licensed
    /// with a plugin entitled to it; None where not read.
    pub datasets_ready: Option<bool>,
    /// For a reading plugin: whether it is entitled to a dataset.
    pub entitled: Option<bool>,
}

/// One step: its words, done or not, where it is done, and whose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub words: String,
    /// None: core cannot judge it; the plugin's own page says.
    pub done: Option<bool>,
    pub href: Option<String>,
    /// A deployment admin's step, said as "Ask a deployment admin to ..." to
    /// anyone else.
    pub deployment_admins: bool,
}

fn settings_href(instance: &str) -> String {
    crate::area::href(instance, AccessLevel::Admin, Some(crate::area::SETTINGS))
}

/// Every required secret set; "No key needed" where none is declared.
fn keys_set(facts: &Facts) -> (String, Option<bool>) {
    let Some(record) = facts.record else {
        return ("Enter the key".into(), None);
    };
    let secrets: Vec<&String> = record
        .declared_settings
        .iter()
        .filter(|d| d.secret && d.required)
        .map(|d| &d.name)
        .collect();
    if secrets.is_empty() {
        return ("No key needed".into(), Some(true));
    }
    (
        "Enter the key".into(),
        Some(secrets.iter().all(|name| record.secrets_set.contains(name))),
    )
}

/// Every required setting set (a value, a secret, or a default).
fn settings_set(facts: &Facts) -> Option<bool> {
    let record = facts.record?;
    Some(
        record
            .declared_settings
            .iter()
            .filter(|d| d.required && !d.per_person)
            .all(|d| {
                !d.default_value.is_empty()
                    || record.secrets_set.contains(&d.name)
                    || record
                        .values
                        .iter()
                        .any(|v| v.name == d.name && !v.value.is_empty())
            }),
    )
}

/// A group holds a level on the instance with accounts.
fn group_holds(facts: &Facts, levels: &[AccessLevel]) -> Option<bool> {
    let records = facts.records?;
    let granting: Vec<&str> = records
        .access_groups
        .iter()
        .filter(|group| {
            group.entries.iter().any(|entry| {
                entry.plugin_instance_id == facts.instance
                    && levels.iter().any(|level| entry.level == *level as i32)
            })
        })
        .map(|group| group.access_group_id.as_str())
        .collect();
    Some(records.permissions.iter().any(|permission| {
        granting.contains(&permission.access_group_id.as_str())
            && !permission.account_group_id.is_empty()
    }))
}

/// The steps of the instance's role(s), in the spec's order.
pub fn steps(facts: &Facts) -> Vec<Step> {
    let instance = facts.instance;
    let view_href = |tab: &str| super::view::tab_href(instance, tab);
    let mut steps = Vec::new();
    let roles: Vec<String> = meridian_domain::roles_now(&facts.roles);
    let has = |role: &str| roles.iter().any(|held| held == role);
    if has("data") {
        let (words, done) = keys_set(facts);
        steps.push(Step {
            words,
            done,
            href: Some(settings_href(instance)),
            deployment_admins: false,
        });
        steps.push(Step {
            words: "Check the connection".into(),
            done: Some(facts.healthy),
            href: None,
            deployment_admins: false,
        });
        steps.push(Step {
            words: "License and entitle its datasets".into(),
            done: facts.datasets_ready,
            href: Some("/admin/data-sources".into()),
            deployment_admins: true,
        });
        steps.push(Step {
            words: "See data arrive".into(),
            done: None,
            href: None,
            deployment_admins: false,
        });
    } else if has("custody") {
        let (_, done) = keys_set(facts);
        steps.push(Step {
            words: "Enter the credentials".into(),
            done,
            href: Some(settings_href(instance)),
            deployment_admins: false,
        });
        steps.push(Step {
            words: "Connect".into(),
            done: Some(facts.connected),
            href: None,
            deployment_admins: false,
        });
        let granted = facts.records.map(|records| {
            records
                .linkable_accounts
                .iter()
                .any(|l| l.plugin_instance_id == instance && !l.account_ids.is_empty())
        });
        steps.push(Step {
            words: "Have accounts granted".into(),
            done: granted,
            href: Some(view_href(super::view::ACCOUNTS)),
            deployment_admins: true,
        });
        steps.push(Step {
            words: "Link accounts".into(),
            done: Some(facts.connected && facts.unlinked == 0),
            href: None,
            deployment_admins: false,
        });
    } else if has("operations") {
        steps.push(Step {
            words: "Choose its settings".into(),
            done: settings_set(facts),
            href: Some(settings_href(instance)),
            deployment_admins: false,
        });
        steps.push(Step {
            words: "Have access and a street source".into(),
            done: group_holds(facts, &[AccessLevel::Write]),
            href: Some(view_href(super::view::ACCESS)),
            deployment_admins: true,
        });
    } else if has("reporting") {
        steps.push(Step {
            words: "Choose its settings".into(),
            done: settings_set(facts),
            href: Some(settings_href(instance)),
            deployment_admins: false,
        });
        steps.push(Step {
            words: "Have it entitled to the data it reads".into(),
            done: facts.entitled,
            href: Some("/admin/data-sources".into()),
            deployment_admins: true,
        });
        steps.push(Step {
            words: "Grant people read".into(),
            done: group_holds(facts, &[AccessLevel::Read, AccessLevel::Write]),
            href: Some(view_href(super::view::ACCESS)),
            deployment_admins: true,
        });
    } else {
        steps.push(Step {
            words: "Choose its settings".into(),
            done: settings_set(facts),
            href: Some(settings_href(instance)),
            deployment_admins: false,
        });
        steps.push(Step {
            words: "Check the connection".into(),
            done: Some(facts.healthy),
            href: None,
            deployment_admins: false,
        });
    }
    steps
}

/// Whether every step core judges is done.
pub fn set_up(steps: &[Step]) -> bool {
    steps.iter().all(|step| step.done != Some(false))
}

fn step_line(step: &Step, deployment_admin: bool) -> String {
    let (mark, state) = match step.done {
        Some(true) => ("&#10003;", "done"),
        Some(false) => ("&#9675;", "to do"),
        None => ("&middot;", "on its own page"),
    };
    let words = escape(&step.words);
    let said = match (
        &step.href,
        step.deployment_admins && !deployment_admin,
        step.done,
    ) {
        (_, true, Some(false)) => format!("Ask a deployment admin to {}", words.to_lowercase()),
        (Some(href), _, Some(false)) => format!("<a href=\"{}\">{words}</a>", escape(href)),
        _ => words,
    };
    format!("<li data-step=\"{state}\"><span class=\"mark\" aria-label=\"{state}\">{mark}</span> {said}</li>")
}

/// The part as drawn: the steps, one line each, or "Set up" with them a
/// click away.
pub fn section(steps: &[Step], deployment_admin: bool) -> String {
    let lines: String = steps
        .iter()
        .map(|step| step_line(step, deployment_admin))
        .collect();
    if set_up(steps) {
        format!(
            "<section class=\"panel padded\" id=\"setting-up\" data-set-up=\"true\"><h2>Setting up</h2>\
             <details><summary>Set up</summary><ol class=\"steps\">{lines}</ol></details></section>"
        )
    } else {
        format!(
            "<section class=\"panel padded\" id=\"setting-up\" data-set-up=\"false\"><h2>Setting up</h2>\
             <ol class=\"steps\">{lines}</ol></section>"
        )
    }
}

/// The part as `dashboard__read_plugin_summary` answers it, under
/// `setting-up` (W6.20).
pub fn as_json(steps: &[Step]) -> Value {
    json!({
        "set_up": set_up(steps),
        "steps": steps.iter().map(|step| json!({
            "step": step.words,
            "done": step.done,
            "where": step.href,
            "deployment_admins": step.deployment_admins,
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use meridian_pb::v1::SettingDeclaration;

    #[test]
    fn a_data_plugin_without_its_key_says_enter_it_first_and_folds_when_set_up() {
        let record = PluginSettingsRecord {
            plugin_instance_id: "coin-1".into(),
            declared_settings: vec![SettingDeclaration {
                name: "api_key".into(),
                secret: true,
                required: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        let facts = Facts {
            instance: "coin-1",
            roles: vec!["dgm".into()],
            record: Some(&record),
            healthy: false,
            datasets_ready: Some(false),
            ..Default::default()
        };
        let steps = steps(&facts);
        assert_eq!(steps[0].words, "Enter the key");
        assert_eq!(steps[0].done, Some(false));
        let drawn = section(&steps, false);
        assert!(drawn.contains("data-set-up=\"false\""));
        assert!(
            drawn.contains("Ask a deployment admin to license and entitle its datasets"),
            "{drawn}"
        );
        let mut set = record.clone();
        set.secrets_set = vec!["api_key".into()];
        let facts = Facts {
            record: Some(&set),
            healthy: true,
            datasets_ready: Some(true),
            ..facts
        };
        let drawn = section(&super::steps(&facts), true);
        assert!(
            drawn.contains("data-set-up=\"true\"") && drawn.contains("<summary>Set up</summary>"),
            "{drawn}"
        );
    }

    #[test]
    fn a_custody_plugin_needs_accounts_granted() {
        let records = AccessRecords::default();
        let facts = Facts {
            instance: "snap-1",
            roles: vec!["custody".into()],
            records: Some(&records),
            connected: true,
            ..Default::default()
        };
        let steps = steps(&facts);
        let granted = steps
            .iter()
            .find(|s| s.words == "Have accounts granted")
            .unwrap();
        assert_eq!(granted.done, Some(false));
        assert!(section(&steps, true).contains("tab=accounts"));
    }
}
