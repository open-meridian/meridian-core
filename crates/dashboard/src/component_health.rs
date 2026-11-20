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
//! its admins, from its sidecar's report as it carries them. The conductor
//! not answering three asks in a row is a notice too, `component-unheard`
//! about the conductor, to the deployment admins (approved 2026-11-20):
//! nobody's health is judged while it is silent.

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

/// Asks in a row the conductor leaves unanswered before its silence is a
/// notice (`component-unheard`, approved 2026-11-20): three minutes.
pub const UNHEARD_ASKS: u32 = 3;

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

/// The conductor's silence: asks unanswered in a row, the first of them,
/// and whether its notice has opened.
#[derive(Debug, Default)]
struct Unheard {
    asks: u32,
    since_ns: i64,
    open: bool,
}

/// One notice to file: its kind and the dashboard's words for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    /// The component, or the plugin instance.
    pub of: String,
    pub plugin: bool,
    pub kind: String,
    pub about: String,
    /// What it is of, its kind and its window (review-F headline 2): every
    /// copy of the dashboard judging the same reports names the same event,
    /// so the notices' store keeps it once. A restart's window is the
    /// cluster's time of it; every other kind's, the conductor's silence
    /// among them, the hour it opened in, so two copies asking seconds apart
    /// file one notice, and one condition opening again within the hour
    /// files none again (decision 40). Two copies judging
    /// either side of an hour's boundary still file two, until v32.
    pub event: String,
    pub window_ns: i64,
}

/// The judge's memory of every member, for 24 hours.
#[derive(Debug, Default)]
pub struct Judge {
    members: BTreeMap<String, Kept>,
    unheard: Unheard,
}

fn mib(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

impl Judge {
    /// Each member's latest report judged against what it said before; the
    /// notices that open. A member not heard for 24 hours is let go.
    pub fn judge(&mut self, members: &[ComponentReport], now_ns: i64) -> Vec<Opened> {
        // Heard: its silence over, and a notice again should it fall silent.
        self.unheard = Unheard::default();
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
            let hour = now_ns.div_euclid(HOUR_NS) * HOUR_NS;
            let mut open = |name: &str, about: String, window: Option<(i64, String)>| {
                let kind = kind(name);
                let (window_ns, at) = window.unwrap_or((hour, String::new()));
                opened.push(Opened {
                    of: of.clone(),
                    plugin,
                    event: format!("{kind}|{key}|{window_ns}{at}"),
                    kind,
                    about,
                    window_ns,
                })
            };
            if let Some(before) = before {
                if sample.restarts > before.restarts {
                    let why = match report.last_restart_reason.as_str() {
                        "OOMKilled" => " after an OOM kill, a defect: lower the bound that held the memory, never raise its limit".to_string(),
                        "" => String::new(),
                        reason => format!(" ({reason})"),
                    };
                    // The restart itself, as the cluster times it; where
                    // the conductor may not read the pod, the restarts
                    // counted within the hour.
                    let window = if report.last_restart_at_ns > 0 {
                        (report.last_restart_at_ns, String::new())
                    } else {
                        (hour, format!("|{}", sample.restarts))
                    };
                    open(
                        "restarted",
                        format!(
                            "{key} restarted{why}; {} restart(s) since its pod started",
                            sample.restarts
                        ),
                        Some(window),
                    );
                }
                if sample.sheds > before.sheds {
                    open(
                        "shedding",
                        format!(
                            "{key} cancelled {} request(s) above its shed line: its largest in flight, refused for load; lower the bound that holds the memory",
                            sample.sheds - before.sheds
                        ),
                        None,
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
                    None,
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
                    None,
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
                    None,
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

    /// The conductor did not answer at `now_ns`: its silence opens a notice
    /// to the deployment admins at the [`UNHEARD_ASKS`]th ask in a row, once
    /// until it answers again (`component-unheard`, approved 2026-11-20).
    /// Its window is the hour of the ask that opens it, as every other
    /// kind's is the hour it opened in (decision 40), so every copy names one
    /// event, and a silence opening again within that hour files none again.
    pub fn unanswered(&mut self, now_ns: i64) -> Vec<Opened> {
        let unheard = &mut self.unheard;
        if unheard.asks == 0 {
            unheard.since_ns = now_ns;
        }
        unheard.asks = unheard.asks.saturating_add(1);
        if unheard.open || unheard.asks < UNHEARD_ASKS {
            return Vec::new();
        }
        unheard.open = true;
        let kind = "component-unheard".to_string();
        let window_ns = now_ns.div_euclid(HOUR_NS) * HOUR_NS;
        vec![Opened {
            of: "conductor".into(),
            plugin: false,
            event: format!("{kind}|conductor|{window_ns}"),
            about: format!(
                "the conductor has not answered for {} asks in a row, since {}: no component's or plugin's health is judged until it does",
                unheard.asks,
                crate::custody::utc(unheard.since_ns)
            ),
            kind,
            window_ns,
        }]
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

/// The notices one judging files: one per opening per person told, each ID
/// its event's ([`crate::latency::notices::keyed_id`]).
pub fn notices_of(records: &AccessRecords, opened: &[Opened], now_ns: i64) -> Vec<Notice> {
    let mut notices = Vec::new();
    for one in opened {
        for subject in told(records, one) {
            notices.push(Notice {
                notice_id: crate::latency::notices::keyed_id(one.window_ns, &one.event, &subject),
                subject,
                plugin_instance_id: one.of.clone(),
                kind: one.kind.clone(),
                about: one.about.clone(),
                changed_ns: now_ns,
            });
        }
    }
    notices
}

/// Ask the conductor every [`ASK_EVERY`], judge its answer or its silence,
/// and file what opened on the latency judge's path. The task is handed
/// back, for the component to end before its runtime stops.
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
            let members = match asked {
                Ok((_, bytes)) => match ReadComponentHealthReply::decode(&bytes[..]) {
                    Ok(reply) => Some(reply.members),
                    Err(_) => {
                        // An answer, if not one that reads: not silence.
                        tracing::warn!("a ReadComponentHealthReply did not decode");
                        continue;
                    }
                },
                Err(failed) => {
                    tracing::debug!(%failed, "the conductor did not answer ReadComponentHealth; asked again in a minute");
                    None
                }
            };
            let now = app.clock.now_ns();
            let opened = judge
                .lock()
                .map(|mut judge| match &members {
                    Some(members) => judge.judge(members, now),
                    None => judge.unanswered(now),
                })
                .unwrap_or_default();
            if opened.is_empty() {
                continue;
            }
            let Ok(records) = app.records.current(now) else {
                tracing::warn!(notices = opened.len(), "a component's notice opened while the access records were stale; told nobody rather than the wrong people");
                continue;
            };
            for one in &opened {
                tracing::warn!(of = one.of, kind = one.kind, "{}", one.about);
            }
            let notices = notices_of(&records, &opened, now);
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

    /// The deployment admin, Dee, and nobody else.
    fn records() -> AccessRecords {
        use meridian_domain::v1::{Permission, SignInRecord, UserGroup};
        AccessRecords {
            user_groups: vec![UserGroup {
                user_group_id: "UG-DEE".into(),
                name: "UG-DEE".into(),
                directory_groups: vec![],
                logins: vec!["local|dee".into()],
            }],
            permissions: vec![Permission {
                permission_id: "P-DEE".into(),
                user_group_id: "UG-DEE".into(),
                account_group_id: String::new(),
                access_group_id: meridian_access::DEPLOYMENT_ADMIN.into(),
            }],
            people: vec![SignInRecord {
                subject: "local|dee".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// ha runs two dashboards, each asking the conductor on its own minute
    /// and judging (review-F headline 2): one restart, one shed and one
    /// crossing of the budget are each one notice in the one store both
    /// keep their notices in.
    #[test]
    fn two_judges_over_one_store_file_one_notice() {
        use crate::latency::notices::{InMemory, NoticeStore as _};
        let records = records();
        let store = InMemory::default();
        let (mut a, mut b) = (Judge::default(), Judge::default());
        let mut restarted = lake(700 * MIB, 0, 2, 1);
        restarted.last_restart_reason = "OOMKilled".into();
        restarted.last_restart_at_ns = T0 + 30_000_000_000;
        // The same reports, read by each copy 40 seconds apart.
        for (judge, offset) in [(&mut a, 0), (&mut b, 40_000_000_000)] {
            judge.judge(&[lake(100 * MIB, 0, 0, 0)], T0 - MINUTE + offset);
            let at = T0 + MINUTE + offset;
            let opened = judge.judge(&[restarted.clone()], at);
            assert_eq!(
                kinds(&opened),
                vec![
                    "component-restarted",
                    "component-shedding",
                    "component-over-budget"
                ]
            );
            store.notify(&notices_of(&records, &opened, at)).unwrap();
        }
        let filed = store.notices("local|dee", 50).unwrap();
        assert_eq!(filed.len(), 3, "{filed:#?}");
    }

    /// The conductor silent (`component-unheard`, approved 2026-11-20):
    /// three asks unanswered in a row are a notice about the conductor to
    /// the deployment admins, its window the hour of the ask that opens it,
    /// as the other conditions' is (decision 40). Every copy judges, and
    /// the keyed ID makes their notices one: within one hour a silence, an
    /// answer and a silence again are one notice, from either copy.
    #[test]
    fn a_silent_conductor_within_one_hour_is_one_notice() {
        use crate::latency::notices::{InMemory, NoticeStore as _};
        let records = records();
        let store = InMemory::default();
        let hour = T0.div_euclid(HOUR_NS) * HOUR_NS;
        let heard = [lake(MIB, 0, 0, 0)];
        // Two copies, asking 40 seconds apart.
        for offset in [0, 40_000_000_000] {
            let mut judge = Judge::default();
            let mut ask = |answered: bool, minute: i64| {
                let at = hour + minute * MINUTE + offset;
                let opened = if answered {
                    judge.judge(&heard, at)
                } else {
                    judge.unanswered(at)
                };
                store.notify(&notices_of(&records, &opened, at)).unwrap();
            };
            ask(true, 0);
            for minute in 1..=3 {
                ask(false, minute);
            }
            ask(true, 6);
            for minute in 7..=9 {
                ask(false, minute);
            }
        }
        let filed = store.notices("local|dee", 50).unwrap();
        assert_eq!(filed.len(), 1, "{filed:#?}");
        assert_eq!(filed[0].kind, "component-unheard");
        assert_eq!(filed[0].plugin_instance_id, "conductor");
        assert!(crate::latency::notices::rows(&filed).contains(">Unheard<"));
    }

    /// The conductor's silence takes the hour of the ask that opens it as
    /// its window, as every other kind takes the hour it opened in
    /// (decision 40; the delta review of 77afd5d0, finding 2): a silence
    /// begun before the hour and opened in it, an answer, and a silence
    /// again within that hour are one notice.
    #[test]
    fn a_silence_opened_within_one_hour_is_one_notice_whenever_it_began() {
        use crate::latency::notices::{InMemory, NoticeStore as _};
        let records = records();
        let store = InMemory::default();
        let eleven = T0.div_euclid(HOUR_NS) * HOUR_NS + HOUR_NS;
        let heard = [lake(MIB, 0, 0, 0)];
        let mut judge = Judge::default();
        let mut ask = |answered: bool, minute: i64| {
            let at = eleven + minute * MINUTE;
            let opened = if answered {
                judge.judge(&heard, at)
            } else {
                judge.unanswered(at)
            };
            store.notify(&notices_of(&records, &opened, at)).unwrap();
        };
        // Unheard at 10:58, 10:59 and 11:00; answered at 11:01; unheard at
        // 11:02 to 11:04.
        for minute in -2..=0 {
            ask(false, minute);
        }
        ask(true, 1);
        for minute in 2..=4 {
            ask(false, minute);
        }
        let filed = store.notices("local|dee", 50).unwrap();
        assert_eq!(filed.len(), 1, "{filed:#?}");
    }

    #[test]
    fn a_member_unheard_for_a_day_is_let_go() {
        let mut judge = Judge::default();
        judge.judge(&[lake(MIB, 0, 0, 0)], T0);
        judge.judge(&[], T0 + KEPT_NS + MINUTE);
        assert!(judge.day_of("lake/meridian-lake-0").is_none());
    }
}
