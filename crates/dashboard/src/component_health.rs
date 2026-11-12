//! Each component's and plugin's health, judged (W6.40; contract v30, stage
//! M; spec/a-deployment-is-observable-by-open-standards, ruled item 2 and
//! section 7; the notice kinds approved 2026-11-09).
//!
//! Each minute the dashboard asks the conductor for every member's latest
//! report (`ReadComponentHealth`), keeps the last 24 hours of it, and files a
//! notice, once, when one of these opens: the member restarted; it reserved,
//! or holds resident, more than its working budget; it shed a request; it
//! restarted three times or more within an hour; its peak rose in each of
//! the last six hours and stands above half its limit. A core component's
//! notices go to the deployment admins, on the latency judge's path (the
//! Inbox and `dashboard__read_inbox`); a plugin's, the same five kinds, to
//! its admins, from its sidecar's report as it carries them.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meridian_domain::v1::{AccessRecords, ComponentReport, ReadComponentHealthReply};
use prost::Message;

use crate::latency::notices::Notice;
use crate::web::App;

/// The conductor's read of every member's health (W6.40).
pub const READ_COMPONENT_HEALTH: &str = "platform.deployment.query.read-component-health";

/// How often the dashboard asks.
pub const ASK_EVERY: Duration = Duration::from_secs(60);

/// How long a member's samples are kept: what `status` and `doctor` show.
pub const KEPT_NS: i64 = 24 * 60 * 60 * 1_000_000_000;

const HOUR_NS: i64 = 60 * 60 * 1_000_000_000;

/// Restarts within an hour that are "repeatedly".
pub const REPEATEDLY: u32 = 3;

/// Hours in a row a peak must rise, above half the limit, to be "climbing".
pub const CLIMBING_HOURS: usize = 6;

/// One sample of a member's health.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub at_ns: i64,
    pub memory_bytes: u64,
    pub reserved_bytes: u64,
    pub sheds: u64,
    pub restarts: u32,
}

/// A member's kept samples, and which of its conditions stand open.
#[derive(Debug, Default)]
struct Kept {
    samples: VecDeque<Sample>,
    over_budget: bool,
    repeatedly: bool,
    climbing: bool,
}

/// One notice to file: its kind and the dashboard's words for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    /// The component, or the plugin instance.
    pub of: String,
    pub plugin: bool,
    pub kind: String,
    pub about: String,
}

/// The judge's memory of every member, for 24 hours.
#[derive(Debug, Default)]
pub struct Judge {
    members: BTreeMap<String, Kept>,
}

fn mib(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

impl Judge {
    /// Each member's latest report judged against what it said before; the
    /// notices that open. A member not heard for 24 hours is let go.
    pub fn judge(&mut self, members: &[ComponentReport], now_ns: i64) -> Vec<Opened> {
        let mut opened = Vec::new();
        for report in members {
            let key = if report.member.is_empty() {
                report.component.clone()
            } else {
                format!("{}/{}", report.component, report.member)
            };
            // A plugin's sidecar reports as `sidecar-<instance>`.
            let (of, plugin) = match report.component.strip_prefix("sidecar-") {
                Some(instance) => (instance.to_string(), true),
                None => (report.component.clone(), false),
            };
            let kind = |name: &str| {
                if plugin {
                    format!("plugin-{name}")
                } else {
                    format!("component-{name}")
                }
            };
            let sample = Sample {
                at_ns: now_ns,
                memory_bytes: report.memory_bytes,
                reserved_bytes: report.reserved_bytes,
                sheds: report.sheds,
                restarts: report.restarts,
            };
            let kept = self.members.entry(key.clone()).or_default();
            let before = kept.samples.back().copied();
            kept.samples.push_back(sample);
            while kept
                .samples
                .front()
                .is_some_and(|first| first.at_ns < now_ns - KEPT_NS)
            {
                kept.samples.pop_front();
            }
            let mut open = |name: &str, about: String| {
                opened.push(Opened {
                    of: of.clone(),
                    plugin,
                    kind: kind(name),
                    about,
                })
            };
            if let Some(before) = before {
                if sample.restarts > before.restarts {
                    let why = match report.last_restart_reason.as_str() {
                        "OOMKilled" => " after an OOM kill, a defect: lower the bound that held the memory, never raise its limit".to_string(),
                        "" => String::new(),
                        reason => format!(" ({reason})"),
                    };
                    open(
                        "restarted",
                        format!(
                            "{key} restarted{why}; {} restart(s) since its pod started",
                            sample.restarts
                        ),
                    );
                }
                if sample.sheds > before.sheds {
                    open(
                        "shedding",
                        format!(
                            "{key} cancelled {} request(s) above its shed line: its largest in flight, refused for load; lower the bound that holds the memory",
                            sample.sheds - before.sheds
                        ),
                    );
                }
            }
            let budget = report.memory_budget_bytes;
            let over =
                budget > 0 && (sample.reserved_bytes > budget || sample.memory_bytes > budget);
            if over && !kept.over_budget {
                open(
                    "over-budget",
                    format!(
                        "{key} holds {} MiB resident and {} MiB reserved against its working budget of {} MiB (limit {} MiB)",
                        mib(sample.memory_bytes),
                        mib(sample.reserved_bytes),
                        mib(budget),
                        mib(report.memory_limit_bytes)
                    ),
                );
            }
            kept.over_budget = over;
            // Restarts within the last hour: the rise across the samples in it.
            let hour_ago = now_ns - HOUR_NS;
            let first_in_hour = kept
                .samples
                .iter()
                .find(|s| s.at_ns >= hour_ago)
                .map_or(sample.restarts, |s| s.restarts);
            let lately = sample.restarts.saturating_sub(first_in_hour);
            let repeatedly = lately >= REPEATEDLY;
            if repeatedly && !kept.repeatedly {
                open(
                    "restarting-repeatedly",
                    format!("{key} restarted {lately} times within the hour"),
                );
            }
            kept.repeatedly = repeatedly;
            let climbing = climbing(&kept.samples, now_ns, report.memory_limit_bytes);
            if climbing && !kept.climbing {
                open(
                    "memory-climbing",
                    format!(
                        "{key}'s peak memory rose in each of the last {CLIMBING_HOURS} hours, to {} MiB of its {} MiB limit",
                        mib(sample.memory_bytes),
                        mib(report.memory_limit_bytes)
                    ),
                );
            }
            kept.climbing = climbing;
        }
        self.members.retain(|_, kept| {
            kept.samples
                .back()
                .is_some_and(|last| last.at_ns >= now_ns - KEPT_NS)
        });
        opened
    }

    /// A member's peak and restarts over its kept 24 hours: what `status`
    /// and `doctor` show.
    pub fn day_of(&self, member: &str) -> Option<(u64, u32)> {
        let kept = self.members.get(member)?;
        let peak = kept.samples.iter().map(|s| s.memory_bytes).max()?;
        let first = kept.samples.front()?.restarts;
        let last = kept.samples.back()?.restarts;
        Some((peak, last.saturating_sub(first)))
    }
}

/// Whether the hourly peaks of the last [`CLIMBING_HOURS`] whole hours rose
/// each hour, the last above half the limit.
fn climbing(samples: &VecDeque<Sample>, now_ns: i64, limit: u64) -> bool {
    if limit == 0 {
        return false;
    }
    let this_hour = now_ns.div_euclid(HOUR_NS);
    let mut peaks: BTreeMap<i64, u64> = BTreeMap::new();
    for sample in samples {
        let hour = sample.at_ns.div_euclid(HOUR_NS);
        let peak = peaks.entry(hour).or_default();
        *peak = (*peak).max(sample.memory_bytes);
    }
    let hours: Vec<u64> = (0..CLIMBING_HOURS as i64)
        .rev()
        .map(|back| peaks.get(&(this_hour - back)).copied())
        .collect::<Option<Vec<u64>>>()
        .unwrap_or_default();
    hours.len() == CLIMBING_HOURS
        && hours.windows(2).all(|pair| pair[1] > pair[0])
        && hours.last().is_some_and(|last| *last > limit / 2)
}

/// Who is told: a core component's deployment admins, a plugin's admins.
pub fn told(records: &AccessRecords, opened: &Opened) -> BTreeSet<String> {
    records
        .people
        .iter()
        .filter(|person| {
            let access = crate::prepared::access_of(records, &person.subject);
            if opened.plugin {
                access.administers(&opened.of)
            } else {
                access.deployment_admin
            }
        })
        .map(|person| person.subject.clone())
        .collect()
}

/// Ask the conductor every [`ASK_EVERY`], judge, and file what opened on the
/// latency judge's path. The task is handed back, for the component to end
/// before its runtime stops.
pub fn judge_forever(app: Arc<App>) -> tokio::task::JoinHandle<()> {
    let judge = Arc::new(Mutex::new(Judge::default()));
    tokio::spawn(async move {
        let mut every = tokio::time::interval(ASK_EVERY);
        loop {
            every.tick().await;
            let asked = app
                .bus
                .call(
                    READ_COMPONENT_HEALTH,
                    "meridian.v1.ReadComponentHealthRequest",
                    Vec::new(),
                    None,
                    None,
                )
                .await;
            let Ok((_, bytes)) = asked else {
                tracing::debug!(
                    "the conductor did not answer ReadComponentHealth; asked again in a minute"
                );
                continue;
            };
            let Ok(reply) = ReadComponentHealthReply::decode(&bytes[..]) else {
                tracing::warn!("a ReadComponentHealthReply did not decode");
                continue;
            };
            let now = app.clock.now_ns();
            let opened = judge
                .lock()
                .map(|mut judge| judge.judge(&reply.members, now))
                .unwrap_or_default();
            if opened.is_empty() {
                continue;
            }
            let Ok(records) = app.records.current(now) else {
                tracing::warn!(notices = opened.len(), "a component's notice opened while the access records were stale; told nobody rather than the wrong people");
                continue;
            };
            let mut notices = Vec::new();
            for one in &opened {
                tracing::warn!(of = one.of, kind = one.kind, "{}", one.about);
                for subject in told(&records, one) {
                    notices.push(Notice {
                        notice_id: app.tickets.ulid(now),
                        subject,
                        plugin_instance_id: one.of.clone(),
                        kind: one.kind.clone(),
                        about: one.about.clone(),
                        changed_ns: now,
                    });
                }
            }
            if notices.is_empty() {
                continue;
            }
            let kept =
                tokio::task::spawn_blocking(move || crate::latency::notices::keep(&notices)).await;
            if !matches!(kept, Ok(Ok(()))) {
                tracing::error!("a component's notices were not kept");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;
    const T0: i64 = 1_793_000_000_000_000_000;
    const MINUTE: i64 = 60_000_000_000;

    fn lake(memory: u64, reserved: u64, sheds: u64, restarts: u32) -> ComponentReport {
        ComponentReport {
            component: "lake".into(),
            member: "meridian-lake-0".into(),
            memory_bytes: memory,
            memory_limit_bytes: 1024 * MIB,
            memory_budget_bytes: 600 * MIB,
            reserved_bytes: reserved,
            sheds,
            restarts,
            ..Default::default()
        }
    }

    fn kinds(opened: &[Opened]) -> Vec<String> {
        opened.iter().map(|o| o.kind.clone()).collect()
    }

    #[test]
    fn a_restart_a_shed_and_crossing_the_budget_each_open_one_notice_once() {
        let mut judge = Judge::default();
        assert!(judge.judge(&[lake(100 * MIB, 0, 0, 0)], T0).is_empty());
        let mut oom = lake(100 * MIB, 0, 0, 1);
        oom.last_restart_reason = "OOMKilled".into();
        let opened = judge.judge(&[oom], T0 + MINUTE);
        assert_eq!(kinds(&opened), vec!["component-restarted"]);
        assert!(
            opened[0].about.contains("a defect")
                && opened[0].about.contains("never raise its limit")
        );
        let opened = judge.judge(&[lake(700 * MIB, 0, 2, 1)], T0 + 2 * MINUTE);
        assert_eq!(
            kinds(&opened),
            vec!["component-shedding", "component-over-budget"]
        );
        // Still over, nothing shed since: nothing new.
        assert!(judge
            .judge(&[lake(700 * MIB, 0, 2, 1)], T0 + 3 * MINUTE)
            .is_empty());
        // Back under, then over again: opened again.
        assert!(judge
            .judge(&[lake(100 * MIB, 0, 2, 1)], T0 + 4 * MINUTE)
            .is_empty());
        assert_eq!(
            kinds(&judge.judge(&[lake(100 * MIB, 650 * MIB, 2, 1)], T0 + 5 * MINUTE)),
            vec!["component-over-budget"]
        );
    }

    #[test]
    fn three_restarts_within_the_hour_are_repeatedly() {
        let mut judge = Judge::default();
        judge.judge(&[lake(MIB, 0, 0, 0)], T0);
        judge.judge(&[lake(MIB, 0, 0, 1)], T0 + 10 * MINUTE);
        judge.judge(&[lake(MIB, 0, 0, 2)], T0 + 20 * MINUTE);
        let opened = judge.judge(&[lake(MIB, 0, 0, 3)], T0 + 30 * MINUTE);
        assert_eq!(
            kinds(&opened),
            vec!["component-restarted", "component-restarting-repeatedly"]
        );
    }

    #[test]
    fn a_peak_rising_six_hours_above_half_its_limit_is_climbing_and_a_sidecar_is_its_plugins() {
        let mut judge = Judge::default();
        let hour = 60 * MINUTE;
        let start = (T0 / hour) * hour;
        let mut last = Vec::new();
        for h in 0..6 {
            let mut report = lake((400 + 30 * h as u64) * MIB, 0, 0, 0);
            report.component = "sidecar-coinbase-1".into();
            last = judge.judge(&[report], start + h * hour + MINUTE);
        }
        assert_eq!(kinds(&last), vec!["plugin-memory-climbing"]);
        assert_eq!(last[0].of, "coinbase-1");
        assert!(last[0].plugin);
        assert_eq!(
            judge
                .day_of("sidecar-coinbase-1/meridian-lake-0")
                .map(|d| d.1),
            Some(0)
        );
    }

    #[test]
    fn a_member_unheard_for_a_day_is_let_go() {
        let mut judge = Judge::default();
        judge.judge(&[lake(MIB, 0, 0, 0)], T0);
        judge.judge(&[], T0 + KEPT_NS + MINUTE);
        assert!(judge.day_of("lake/meridian-lake-0").is_none());
    }
}
