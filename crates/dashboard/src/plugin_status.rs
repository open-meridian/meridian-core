//! A plugin instance's status: one word and its reason, judged once for the
//! home page, Settings › Plugins and `list_plugins` alike (the 2026-11-23
//! consensus on the home launcher), so the page and the tool never differ.
//!
//! The first that holds wins:
//! - **Stopped**: its launch stopped or failed, or it was never launched and
//!   says nothing.
//! - **Setting up**: a setting-up step judged from the records a person
//!   writes is open (a key, a required setting, its datasets licensed,
//!   accounts granted, access held). A step judged from its reports never
//!   holds it here: open, it is the plugin not healthy, or something it
//!   needs of a person.
//! - **Starting**: launched within [`SILENT_NS`] and not yet registered or
//!   heard from.
//! - **Not healthy**: its sidecar says so, says nothing past [`SILENT_NS`],
//!   or has not registered it since.
//! - **Working**: otherwise; the note says when its data, or else its last
//!   report, arrived. No staleness is judged here: the age is said.
//!
//! On the home page are the instances neither stopped nor setting up.
//! The words a plugin wrote -- its health detail -- are another's, escaped
//! wherever they are drawn and passed through `others_words` by the tool.

use std::collections::BTreeMap;

use meridian_domain::v1::{
    AccessRecords, ListDatasetsReply, PluginLaunch, PluginLaunchState, PluginReport,
};
use serde_json::{json, Value};

use crate::admin::setting_up::Step;
use crate::health::SILENT_NS;
use crate::html::escape;
use crate::web::App;

/// The five states, in the order they are judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Standing {
    NotHealthy,
    SettingUp,
    Starting,
    Working,
    Stopped,
}

impl Standing {
    pub(crate) const ALL: [Standing; 5] = [
        Standing::NotHealthy,
        Standing::SettingUp,
        Standing::Starting,
        Standing::Working,
        Standing::Stopped,
    ];

    /// The word a person reads.
    pub(crate) fn word(self) -> &'static str {
        match self {
            Standing::Working => "Working",
            Standing::SettingUp => "Setting up",
            Standing::NotHealthy => "Not healthy",
            Standing::Starting => "Starting",
            Standing::Stopped => "Stopped",
        }
    }

    /// Its name for a script, a query (`?show=`) and the tool.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Standing::Working => "working",
            Standing::SettingUp => "setting-up",
            Standing::NotHealthy => "not-healthy",
            Standing::Starting => "starting",
            Standing::Stopped => "stopped",
        }
    }

    pub(crate) fn named(name: &str) -> Option<Standing> {
        Standing::ALL.into_iter().find(|s| s.name() == name)
    }

    /// The kit's om-status state: its badge's tone (busy and none neutral).
    fn state(self) -> &'static str {
        match self {
            Standing::Working => "ok",
            Standing::SettingUp => "warn",
            Standing::NotHealthy => "error",
            Standing::Starting => "busy",
            Standing::Stopped => "",
        }
    }
}

/// An instance's status, its reason, and what it needs of a person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Status {
    pub standing: Standing,
    /// Why, in a sentence.
    pub reason: String,
    /// The moment the reason is as of, and what the note calls it; 0 none.
    pub at_ns: i64,
    pub at_label: &'static str,
    /// What it needs of a person, each a line ("Settings: needs Client ID").
    pub needs_you: Vec<String>,
    /// Where a connectivity plugin trades: "Demo", "Live" or the
    /// connection's own word.
    pub trading: Option<String>,
}

impl Status {
    /// Whether the home page lists it: set up, and not stopped.
    pub(crate) fn on_home(&self) -> bool {
        !matches!(self.standing, Standing::Stopped | Standing::SettingUp)
    }

    /// The fields `list_plugins` gives each instance.
    pub(crate) fn as_json(&self, others: impl Fn(&str) -> String) -> Value {
        json!({
            "status": self.standing.name(),
            "reason": others(&self.reason),
            "needs_you": self.needs_you.iter().map(|need| others(need)).collect::<Vec<_>>(),
            "trading": self.trading,
            "on_home": self.on_home(),
        })
    }
}

/// What the deployment knows of one instance.
pub(crate) struct Known<'a> {
    pub instance: &'a str,
    /// Its latest launch, live or ended; None where the catalogue never
    /// launched it.
    pub launch: Option<&'a PluginLaunch>,
    pub report: Option<&'a PluginReport>,
    pub steps: &'a [Step],
    /// The lake's listing, where its datasets were read.
    pub listed: Option<&'a ListDatasetsReply>,
    /// Who stopped it, as the page names them; the launch's subject where
    /// empty.
    pub stopped_by: &'a str,
    /// Required settings still to fill in, by label.
    pub missing: &'a [String],
    /// External accounts it reported that nothing links.
    pub unlinked: usize,
    pub trading: Option<&'a str>,
    pub now: i64,
}

/// An age as a note says it: "20 s", "3 min", "2 h", "4 days".
pub(crate) fn age(ns: i64) -> String {
    let s = ns.max(0) / 1_000_000_000;
    match s {
        0..=59 => format!("{s} s"),
        60..=3_599 => format!("{} min", s / 60),
        3_600..=172_799 => format!("{} h", s / 3_600),
        _ => format!("{} days", s / 86_400),
    }
}

/// The latest sign of life of any dataset the instance serves; None where
/// it serves none, none has arrived, or the listing was not read.
fn last_data(known: &Known) -> Option<i64> {
    known
        .listed?
        .datasets
        .iter()
        .filter(|dataset| dataset.instance == known.instance)
        .flat_map(|dataset| dataset.liveness.iter().map(|l| l.alive_at_ns))
        .max()
}

/// A step's words inside a sentence: its first letter lower-cased.
fn in_sentence(words: &str) -> String {
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The instance's status (the module's order).
pub(crate) fn status(known: &Known) -> Status {
    let mut needs_you = Vec::new();
    if !known.missing.is_empty() {
        needs_you.push(format!("Settings: needs {}", known.missing.join(", ")));
    }
    // A step judged from its reports, open where the plugin is healthy: what
    // it needs of a person (its connection made, its accounts linked).
    let healthy = known.report.is_some_and(|r| r.healthy && r.registered);
    for step in known.steps {
        if step.live && step.done == Some(false) && healthy && step.words != "Check the connection"
        {
            needs_you.push(step.words.clone());
        }
    }
    if known.unlinked > 0 {
        needs_you.push(format!(
            "{} external account{} not linked",
            known.unlinked,
            if known.unlinked == 1 { "" } else { "s" }
        ));
    }
    let trading = known.trading.map(str::to_string);
    let made = |standing, reason: String, at_ns, at_label| Status {
        standing,
        reason,
        at_ns,
        at_label,
        needs_you: needs_you.clone(),
        trading: trading.clone(),
    };
    let launched = known
        .launch
        .is_some_and(|launch| launch.state == PluginLaunchState::Launched as i32);
    if let Some(launch) = known.launch.filter(|_| !launched) {
        return if launch.state == PluginLaunchState::Stopped as i32 {
            let by = if known.stopped_by.is_empty() {
                launch.stopped_by.as_str()
            } else {
                known.stopped_by
            };
            made(
                Standing::Stopped,
                format!("Stopped by {by}."),
                launch.stopped_at_ns,
                "Stopped",
            )
        } else if launch.failure.trim().is_empty() {
            made(Standing::Stopped, "Not launched.".into(), 0, "")
        } else {
            made(
                Standing::Stopped,
                format!("Not launched: {}", launch.failure.trim()),
                0,
                "",
            )
        };
    }
    if known.launch.is_none() && known.report.is_none() {
        return made(Standing::Stopped, "Not launched.".into(), 0, "");
    }
    if let Some(open) = known
        .steps
        .iter()
        .find(|step| !step.live && step.done == Some(false))
    {
        return made(
            Standing::SettingUp,
            format!("Next step: {}.", in_sentence(&open.words)),
            0,
            "",
        );
    }
    let health = crate::health::state(known.report, known.now);
    if !health.good {
        let unheard = known.report.is_none_or(|report| {
            !report.registered && known.now - report.reported_at_ns <= SILENT_NS
        });
        let starting = launched
            && unheard
            && known
                .launch
                .is_some_and(|launch| known.now - launch.launched_at_ns <= SILENT_NS);
        if starting {
            return made(
                Standing::Starting,
                "Waiting for its first report.".into(),
                0,
                "",
            );
        }
        let at = known.report.map_or(0, |report| report.reported_at_ns);
        let label = if health.word == "Silent" {
            "Last report"
        } else {
            "Reported"
        };
        return made(
            Standing::NotHealthy,
            health.detail.trim().to_string(),
            at,
            label,
        );
    }
    let reason = match last_data(known) {
        Some(at) => format!("Last data {} ago.", age(known.now - at)),
        None => format!(
            "Last report {} ago.",
            age(known.now - known.report.map_or(known.now, |r| r.reported_at_ns))
        ),
    };
    made(Standing::Working, reason, 0, "")
}

/// The status as Settings › Plugins draws it: the kit's worded badge, its
/// reason, moment and needs in the note, and the page's own words inside
/// for a browser without the kit's script.
pub(crate) fn badge(status: &Status) -> String {
    let mut detail = status.reason.clone();
    for need in &status.needs_you {
        detail.push_str(&format!(" Needs you: {need}."));
    }
    // Where it trades, in the note too: a phone shows no trading badge.
    if let Some(trading) = &status.trading {
        detail.push_str(&format!(" Trading: {trading}."));
    }
    let at = if status.at_ns > 0 {
        format!(
            " at=\"{}\" at-label=\"{}\"",
            escape(&crate::terminal::rfc3339(status.at_ns)),
            escape(status.at_label)
        )
    } else {
        String::new()
    };
    let state = match status.standing.state() {
        "" => String::new(),
        state => format!(" state=\"{state}\""),
    };
    format!(
        "<om-status badge{state} label=\"{word}\" detail=\"{detail}\"{at} data-status=\"{name}\">{word}</om-status>",
        word = escape(status.standing.word()),
        detail = escape(&detail),
        name = status.standing.name(),
    )
}

/// Each instance's status, judged from one read of the launches, the
/// reports and the lake's listing: what the home page, Settings › Plugins
/// and `list_plugins` all use.
pub(crate) async fn every(
    app: &App,
    records: &AccessRecords,
    subject: &str,
    deployment_admin: bool,
    instances: &[String],
) -> BTreeMap<String, Status> {
    let launches = crate::catalogue::every_launch(app).await;
    let reports = app.health.view();
    let datasets = if instances
        .iter()
        .any(|instance| crate::admin::reads_data(&crate::admin::roles_of(app, records, instance)))
    {
        crate::admin::data_sources::datasets(&app.bus, subject)
            .await
            .ok()
    } else {
        None
    };
    let custody = app.custody.view();
    let unlinked = custody.unlinked(&records.links);
    let development = crate::html::is_development();
    let now = app.clock.now_ns();
    let mut every = BTreeMap::new();
    for instance in instances {
        let launch = launches
            .iter()
            .filter(|launch| &launch.instance_id == instance)
            .max_by_key(|launch| launch.launched_at_ns);
        let stopped_by = launch
            .and_then(|launch| {
                records
                    .people
                    .iter()
                    .find(|person| person.subject == launch.stopped_by)
            })
            .map(|person| person.display_name.as_str())
            .unwrap_or_default();
        let steps = crate::admin::setting_up_steps_from(
            app,
            instance,
            records,
            deployment_admin,
            datasets.as_ref(),
        );
        let missing = crate::admin::view::missing_settings(records, instance, development);
        let roles = crate::admin::roles_of(app, records, instance);
        let trading = if roles.iter().any(|role| role == "connectivity") {
            Some(crate::admin::connectivity::trading(app, records, instance).await)
        } else {
            None
        };
        every.insert(
            instance.clone(),
            status(&Known {
                instance,
                launch,
                report: reports.get(instance),
                steps: &steps,
                listed: datasets.as_ref(),
                stopped_by,
                missing: &missing,
                unlinked: unlinked
                    .iter()
                    .filter(|u| &u.plugin_instance_id == instance)
                    .count(),
                trading,
                now,
            }),
        );
    }
    every
}

#[cfg(test)]
mod tests;
