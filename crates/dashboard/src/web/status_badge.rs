//! A plugin's status badge on its home card: one of four words, its why the
//! badge's note on hover, drawn by the plugin page's own noted badge
//! ([`crate::html::noted_badge`]) in its tones, so the two read alike.
//!
//! Judged only from what the deployment already knows of the instance: its
//! launch (the catalogue's), its sidecar's last report (the Summary's
//! status), its setting-up steps (the Summary's "Setting up") and, for an
//! instance serving datasets, the lake's liveness of them against each
//! one's stale ceiling (`dataset_thresholds`' `stale_after`). The words a
//! plugin wrote -- its health detail -- are escaped as any other's.

use meridian_domain::v1::{ListDatasetsReply, PluginLaunch, PluginLaunchState, PluginReport};

use crate::admin::setting_up::Step;

/// The four states, worst first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Standing {
    /// Not launched, or stopped.
    Stopped,
    /// Its sidecar says it is not healthy, or says nothing.
    NotHealthy,
    /// A setting-up step is open, or its data has not arrived lately.
    SetUp,
    /// Healthy, set up, and its data arriving.
    Working,
}

impl Standing {
    pub(crate) fn word(self) -> &'static str {
        match self {
            Standing::Stopped => "Stopped",
            Standing::NotHealthy => "Not healthy",
            Standing::SetUp => "Set up",
            Standing::Working => "Working",
        }
    }

    /// The kit's tone: green, red, amber, and the plain badge's grey.
    fn class(self) -> &'static str {
        match self {
            Standing::Working => "badge good",
            Standing::SetUp => "badge warn",
            Standing::NotHealthy => "badge bad",
            Standing::Stopped => "badge",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Standing::Stopped => "stopped",
            Standing::NotHealthy => "not-healthy",
            Standing::SetUp => "set-up",
            Standing::Working => "working",
        }
    }
}

/// The badge's state and its note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Status {
    pub standing: Standing,
    pub note: String,
}

/// What the deployment knows of one instance, for its badge.
pub(crate) struct Known<'a> {
    pub instance: &'a str,
    /// Its latest launch, live or ended; None where the catalogue never
    /// launched it.
    pub launch: Option<&'a PluginLaunch>,
    pub report: Option<&'a PluginReport>,
    pub steps: &'a [Step],
    /// The lake's listing, where its datasets were read.
    pub listed: Option<&'a ListDatasetsReply>,
    /// Who stopped it, as the page names them; its launch's subject where
    /// empty.
    pub stopped_by: &'a str,
    /// A connectivity plugin's own data (ruling 831): "Data: Live".
    pub data_live: bool,
    pub now: i64,
}

/// The age of the latest sign of life of each dataset the instance serves,
/// and whether any is past its stale ceiling; None where it serves none or
/// the listing was not read.
fn data_age(known: &Known) -> Option<(Option<i64>, bool)> {
    let listed = known.listed?;
    let mine: Vec<_> = listed
        .datasets
        .iter()
        .filter(|dataset| dataset.instance == known.instance)
        .collect();
    if mine.is_empty() {
        return None;
    }
    let mut latest: Option<i64> = None;
    let mut stale = false;
    for dataset in mine {
        let alive = dataset.liveness.iter().map(|l| l.alive_at_ns).max();
        if let Some(alive) = alive {
            latest = Some(latest.map_or(alive, |held| held.max(alive)));
        }
        // As the lake judges a default read: stale only past a ceiling the
        // data plugin admin set; none set, never.
        if dataset.stale_after_ns > 0
            && alive.is_none_or(|alive| known.now - alive > dataset.stale_after_ns)
        {
            stale = true;
        }
    }
    Some((latest.map(|at| known.now - at), stale))
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

fn when(ns: i64) -> String {
    crate::terminal::rfc3339(ns)[..16].replace('T', " ")
}

/// The instance's status, worst first: stopped, not healthy, a step open,
/// its data not arriving, working.
pub(crate) fn status(known: &Known) -> Status {
    let launched = known
        .launch
        .is_some_and(|launch| launch.state == PluginLaunchState::Launched as i32);
    let ended = known.launch.filter(|_| !launched);
    if let Some(launch) = ended {
        let note = if launch.state == PluginLaunchState::Stopped as i32 {
            format!(
                "Stopped {} by {}",
                when(launch.stopped_at_ns),
                if known.stopped_by.is_empty() {
                    launch.stopped_by.as_str()
                } else {
                    known.stopped_by
                }
            )
        } else if launch.failure.trim().is_empty() {
            "Not launched".to_string()
        } else {
            format!("Not launched: {}", launch.failure.trim())
        };
        return Status {
            standing: Standing::Stopped,
            note,
        };
    }
    if known.launch.is_none() && known.report.is_none() {
        return Status {
            standing: Standing::Stopped,
            note: "Not launched".into(),
        };
    }
    let data = if known.data_live { "Data: Live" } else { "" };
    let said = |parts: &[&str]| {
        parts
            .iter()
            .filter(|part| !part.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" \u{b7} ")
    };
    let health = crate::health::state(known.report, known.now);
    if !health.good {
        return Status {
            standing: Standing::NotHealthy,
            note: said(&[health.detail.trim(), data]),
        };
    }
    if let Some(open) = known.steps.iter().find(|step| step.done == Some(false)) {
        return Status {
            standing: Standing::SetUp,
            note: said(&[&open.words, data]),
        };
    }
    let (standing, ago) = match data_age(known) {
        None => (Standing::Working, String::new()),
        Some((None, _)) => (Standing::SetUp, String::new()),
        Some((Some(ago), stale)) => (
            if stale {
                Standing::SetUp
            } else {
                Standing::Working
            },
            format!("last data {} ago", age(ago)),
        ),
    };
    let arrive = if standing == Standing::SetUp {
        "See data arrive"
    } else {
        ""
    };
    Status {
        standing,
        note: said(&[arrive, &ago, data]),
    }
}

/// The badge as drawn, and its note for the caller to place: the plugin
/// page's noted badge, its state named for a page's script and a test.
pub(crate) fn badge(status: &Status, instance: &str) -> (String, String) {
    let (badge, note) = crate::html::noted_badge(
        status.standing.class(),
        status.standing.word(),
        &status.note,
        &format!("status-{instance}"),
    );
    // The badge's opening tag, its state added: a span, or a button with a
    // note.
    let badge = badge.replacen(
        " class=",
        &format!(" data-status=\"{}\" class=", status.standing.name()),
        1,
    );
    (badge, note)
}

#[cfg(test)]
mod tests {
    use meridian_domain::v1::{DatasetLiveness, DatasetRef};

    use super::*;

    const NOW: i64 = 1_790_380_800_000_000_000;
    const S: i64 = 1_000_000_000;

    fn launch(state: PluginLaunchState) -> PluginLaunch {
        PluginLaunch {
            instance_id: "alpaca-1".into(),
            state: state as i32,
            stopped_by: "https://directory.example.org|ada".into(),
            stopped_at_ns: NOW - 3_600 * S,
            ..Default::default()
        }
    }

    fn report(healthy: bool, detail: &str) -> PluginReport {
        PluginReport {
            plugin_instance_id: "alpaca-1".into(),
            registered: true,
            healthy,
            health_detail: detail.into(),
            reported_at_ns: NOW,
            ..Default::default()
        }
    }

    fn listed(alive_ago_s: Option<i64>, stale_after_s: i64) -> ListDatasetsReply {
        ListDatasetsReply {
            datasets: vec![DatasetRef {
                dataset: "alpaca-1:bars".into(),
                instance: "alpaca-1".into(),
                stale_after_ns: stale_after_s * S,
                liveness: alive_ago_s
                    .map(|ago| DatasetLiveness {
                        dataset: "alpaca-1:bars".into(),
                        alive_at_ns: NOW - ago * S,
                        ..Default::default()
                    })
                    .into_iter()
                    .collect(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn step(words: &str, done: Option<bool>) -> Step {
        Step {
            words: words.into(),
            done,
            href: None,
            deployment_admins: true,
        }
    }

    fn known<'a>(
        launch: Option<&'a PluginLaunch>,
        report: Option<&'a PluginReport>,
        steps: &'a [Step],
        listed: Option<&'a ListDatasetsReply>,
    ) -> Known<'a> {
        Known {
            instance: "alpaca-1",
            launch,
            report,
            steps,
            listed,
            stopped_by: "Ada Lovelace",
            data_live: true,
            now: NOW,
        }
    }

    #[test]
    fn a_healthy_set_up_plugin_whose_data_arrives_is_working_since_its_last_data() {
        let launched = launch(PluginLaunchState::Launched);
        let healthy = report(true, "");
        let listed = listed(Some(20), 60);
        let steps = [step("License and entitle its datasets", Some(true))];
        let said = status(&known(
            Some(&launched),
            Some(&healthy),
            &steps,
            Some(&listed),
        ));
        assert_eq!(
            said,
            Status {
                standing: Standing::Working,
                note: "last data 20 s ago \u{b7} Data: Live".into()
            }
        );
        let (badge, note) = badge(&said, "alpaca-1");
        assert_eq!(
            badge,
            "<button type=\"button\" data-status=\"working\" class=\"badge good\" data-note \
             aria-describedby=\"status-alpaca-1\">Working</button>"
        );
        assert_eq!(
            note,
            "<span class=\"hint noted\" id=\"status-alpaca-1\">last data 20 s ago \u{b7} Data: Live</span>"
        );
    }

    #[test]
    fn an_open_step_is_set_up_in_the_steps_own_words() {
        let launched = launch(PluginLaunchState::Launched);
        let healthy = report(true, "");
        let steps = [
            step("Enter the key", Some(true)),
            step("License and entitle its datasets", Some(false)),
        ];
        let said = status(&known(Some(&launched), Some(&healthy), &steps, None));
        assert_eq!(said.standing, Standing::SetUp);
        assert_eq!(
            said.note,
            "License and entitle its datasets \u{b7} Data: Live"
        );
        assert!(badge(&said, "alpaca-1").0.contains("class=\"badge warn\""));
    }

    #[test]
    fn data_past_its_stale_ceiling_or_never_seen_is_set_up_to_see_it_arrive() {
        let launched = launch(PluginLaunchState::Launched);
        let healthy = report(true, "");
        let stale = listed(Some(900), 120);
        let said = status(&known(Some(&launched), Some(&healthy), &[], Some(&stale)));
        assert_eq!(said.standing, Standing::SetUp);
        assert_eq!(
            said.note,
            "See data arrive \u{b7} last data 15 min ago \u{b7} Data: Live"
        );
        let never = listed(None, 0);
        let said = status(&known(Some(&launched), Some(&healthy), &[], Some(&never)));
        assert_eq!(said.standing, Standing::SetUp);
        assert_eq!(said.note, "See data arrive \u{b7} Data: Live");
        // No ceiling set: never stale, as the lake judges it.
        let old = listed(Some(7_200), 0);
        let said = status(&known(Some(&launched), Some(&healthy), &[], Some(&old)));
        assert_eq!(said.standing, Standing::Working);
        assert_eq!(said.note, "last data 2 h ago \u{b7} Data: Live");
    }

    #[test]
    fn a_plugin_saying_it_is_not_healthy_is_red_with_its_own_words_escaped() {
        let launched = launch(PluginLaunchState::Launched);
        let unhealthy = report(false, "token <refused>");
        let steps = [step("License and entitle its datasets", Some(false))];
        let said = status(&known(Some(&launched), Some(&unhealthy), &steps, None));
        assert_eq!(said.standing, Standing::NotHealthy);
        let (badge, note) = badge(&said, "alpaca-1");
        assert!(badge.contains("class=\"badge bad\"") && badge.contains(">Not healthy<"));
        assert!(
            note.contains("token &lt;refused&gt; \u{b7} Data: Live"),
            "{note}"
        );
    }

    #[test]
    fn stopped_says_when_and_by_whom_and_never_launched_says_so() {
        let stopped = launch(PluginLaunchState::Stopped);
        let said = status(&known(Some(&stopped), None, &[], None));
        assert_eq!(said.standing, Standing::Stopped);
        assert_eq!(said.note, "Stopped 2026-09-25 23:00 by Ada Lovelace");
        assert!(badge(&said, "alpaca-1").0.contains("class=\"badge\""));
        let said = status(&known(None, None, &[], None));
        assert_eq!(
            said,
            Status {
                standing: Standing::Stopped,
                note: "Not launched".into()
            }
        );
        // Installed without the catalogue and reporting: running.
        let healthy = report(true, "");
        assert_eq!(
            status(&known(None, Some(&healthy), &[], None)).standing,
            Standing::Working
        );
    }

    #[test]
    fn ages_read_in_the_largest_whole_unit() {
        assert_eq!(age(4 * S), "4 s");
        assert_eq!(age(185 * S), "3 min");
        assert_eq!(age(7_300 * S), "2 h");
        assert_eq!(age(5 * 86_400 * S), "5 days");
    }
}
