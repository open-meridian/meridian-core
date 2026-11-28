//! Each component's and plugin's health, judged (W6.40; contract v30, stage
//! M; spec/a-deployment-is-observable-by-open-standards, ruled item 2 and
//! section 7; the notice kinds approved 2026-11-09).
//!
//! Every 20 seconds the dashboard asks the conductor for every member's latest
//! report (`ReadComponentHealth`), keeps the last 24 hours of it, and files a
//! notice, once, when one of these opens: the member restarted; it reserved,
//! or holds resident, more than its working budget; it shed a request; it
//! restarted three times or more within an hour; its peak rose in each of
//! the last six hours and stands above half its limit. A core component's
//! notices go to the deployment admins, on the latency judge's path (the
//! Inbox and `dashboard__read_inbox`); a plugin's, the same five kinds, to
//! its admins, from its sidecar's report as it carries them. The conductor
//! not answering nine asks in a row, three minutes, is a notice too,
//! `component-unheard` about the conductor, to the deployment admins
//! (approved 2026-11-20):
//! nobody's health is judged while it is silent. The lake's write-ahead
//! stream stuck full, as the lake's writer says first in its report's words
//! (v31a item 5, 0-J), is a standing notice to the deployment admins,
//! `component-write-ahead-stuck-full`, and its clearing once no member says
//! so, `component-write-ahead-drained`. The broker refusing the lake's
//! write-ahead writes, or not answering them, for 30 s, as the writer says
//! it apart from stuck full, is one too, `component-write-ahead-refused`,
//! and its clearing once no member says so, `component-write-ahead-accepted`
//! (INTERIM(v32), the proposal of 2026-11-27, A1).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meridian_domain::v1::{AccessRecords, ComponentReport, ReadComponentHealthReply};
use prost::Message;

use crate::latency::notices::Notice;
use crate::web::App;

/// The conductor's read of every member's health (W6.40).
pub const READ_COMPONENT_HEALTH: &str = "platform.deployment.query.read-component-health";

/// How often the dashboard asks: 20 s, a member's report interval
/// (`meridian_runtime::REPORT_INWARD_EVERY`), so a member's words are judged
/// within 40 s of its saying them -- the refused notice within its 120 s
/// (the consensus of 2026-11-28, part 2 (B)). No bound of the judge's but
/// [`UNHEARD_ASKS`] is counted in asks: the others are hours by the clock.
pub const ASK_EVERY: Duration = Duration::from_secs(20);

/// How long a member's samples are kept: what `status` and `doctor` show.
pub const KEPT_NS: i64 = 24 * 60 * 60 * 1_000_000_000;

const HOUR_NS: i64 = 60 * 60 * 1_000_000_000;

/// Restarts within an hour that are "repeatedly".
pub const REPEATEDLY: u32 = 3;

/// Hours in a row a peak must rise, above half the limit, to be "climbing".
pub const CLIMBING_HOURS: usize = 6;

/// Asks in a row the conductor leaves unanswered before its silence is a
/// notice (`component-unheard`, approved 2026-11-20): three minutes, nine
/// asks [`ASK_EVERY`] apart.
pub const UNHEARD_ASKS: u32 = 9;

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
/// and whether its notice has been kept.
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
    /// either side of an hour's boundary still file two, until v32. A
    /// write-ahead stream's stuck spell, and its draining, are windowed by
    /// the time it went stuck, as its writer says it: one event to every
    /// copy, on either side of an hour.
    pub event: String,
    pub window_ns: i64,
}

/// The judge's memory of every member, for 24 hours.
#[derive(Debug, Default)]
pub struct Judge {
    members: BTreeMap<String, Kept>,
    unheard: Unheard,
    /// The core components whose write-ahead stream stands stuck full, as
    /// one of their members said (v31a item 5), each with its spell: the
    /// time its stream went stuck. One is here once its stuck notice is
    /// kept, and until its drained notice is ([`Judge::kept`]).
    stuck_full: BTreeMap<String, i64>,
    /// The core components whose broker refuses their write-ahead writes,
    /// or does not answer them, as one of their members said (INTERIM(v32),
    /// the proposal of 2026-11-27, A1), each with its episode: the time the
    /// refusals began. One is here once its refused notice is kept, and
    /// until its accepted notice is ([`Judge::kept`]).
    refused: BTreeMap<String, i64>,
}

fn mib(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

impl Judge {
    /// Each member's latest report judged against what it said before; the
    /// notices that open. A member not heard for 24 hours is let go.
    pub fn judge(&mut self, members: &[ComponentReport], now_ns: i64) -> Vec<Opened> {
        self.heard();
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
        opened.extend(self.stuck_full(members, now_ns));
        opened.extend(self.refused(members, now_ns));
        opened
    }

    /// The lake's write-ahead stream stuck full, as a core member's report
    /// says first in its words (v31a item 5, 0-J): a standing notice, opened
    /// once while any member of the component says so, and its drained
    /// notice once none of those reporting does.
    ///
    /// One stuck spell is one event, whoever judges it and when (the review
    /// of 4c, fix B item 1): its window is the time the stream went stuck,
    /// as the lake's guard says it in those words
    /// ([`meridian_bus::write_ahead::stuck_since`]), the earliest said, so
    /// every copy of the dashboard, and one started again, names the same
    /// event and the notices' store keeps it once; a second spell within
    /// the hour is another event. Its drained notice carries the same
    /// window. Words saying no time are a spell of the hour judged, as
    /// every other kind's window is (decision 40).
    ///
    /// Each is offered at every judging until [`Judge::kept`] says it was
    /// kept: an opening dropped -- the records stale, or the keeping failed
    /// -- is not lost, and its keyed ID makes a repeat harmless. A writer
    /// moved while its stream stands stuck says a later time: the same
    /// spell to a copy that kept its notice, another to one started since.
    fn stuck_full(&self, members: &[ComponentReport], now_ns: i64) -> Vec<Opened> {
        let stuck = meridian_bus::write_ahead::STUCK_FULL;
        standing(
            members,
            now_ns,
            &self.stuck_full,
            |detail| detail.starts_with(stuck),
            meridian_bus::write_ahead::stuck_since,
            (
                "write-ahead-stuck-full",
                "Standing until it drains; its cause goes to a diagnosis",
            ),
            (
                "write-ahead-drained",
                "the lake's write-ahead stream drained, no longer at a bound",
            ),
        )
    }

    /// The broker refusing the lake's write-ahead writes, or not answering
    /// them, as a core member's report says (INTERIM(v32), the proposal of
    /// 2026-11-27, A1; [`meridian_bus::write_ahead::REFUSED`]): a standing
    /// notice, `component-write-ahead-refused`, opened once an episode while
    /// any member of the component says so, and its clearing,
    /// `component-write-ahead-accepted`, once none of those reporting does
    /// -- the shape, the windows and the offering again of
    /// [`Judge::stuck_full`]'s, an episode windowed by the time its
    /// refusals began as the lake's guard says it. Its words name the
    /// component and say what the broker does; why is not inferred.
    fn refused(&self, members: &[ComponentReport], now_ns: i64) -> Vec<Opened> {
        standing(
            members,
            now_ns,
            &self.refused,
            meridian_bus::write_ahead::says_refused,
            meridian_bus::write_ahead::refused_since,
            (
                "write-ahead-refused",
                "Standing until a write is accepted again",
            ),
            (
                "write-ahead-accepted",
                "the lake's write-ahead writes are accepted by the broker again",
            ),
        )
    }

    /// The conductor did not answer at `now_ns`: its silence opens a notice
    /// to the deployment admins at the [`UNHEARD_ASKS`]th ask in a row
    /// (`component-unheard`, approved 2026-11-20), offered again at each ask
    /// until [`Judge::kept`] says it was kept, then not until it answers and
    /// falls silent again: an opening dropped -- the records stale, or the
    /// keeping failed -- is not lost, and its keyed ID makes a repeat
    /// harmless.
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

    /// The conductor answered, whether or not its answer reads: its silence
    /// over, and a notice again should it fall silent.
    pub fn heard(&mut self) {
        self.unheard = Unheard::default();
    }

    /// What one judging opened is kept, its notices filed ([`heard_by`]:
    /// one with nobody to tell is not, and is offered again): the
    /// conductor's silence, among them, is not offered again while it lasts,
    /// nor a stream's stuck spell while it stands, nor its draining, nor a
    /// refused episode while it stands, nor its writes accepted again.
    pub fn kept(&mut self, opened: &[Opened]) {
        for one in opened {
            match one.kind.as_str() {
                "component-unheard" => self.unheard.open = true,
                // A stuck spell stands from its notice kept to its drained
                // notice kept.
                "component-write-ahead-stuck-full" => {
                    self.stuck_full.insert(one.of.clone(), one.window_ns);
                }
                "component-write-ahead-drained" => {
                    self.stuck_full.remove(&one.of);
                }
                // A refused episode stands from its notice kept to its
                // accepted notice kept (INTERIM(v32)).
                "component-write-ahead-refused" => {
                    self.refused.insert(one.of.clone(), one.window_ns);
                }
                "component-write-ahead-accepted" => {
                    self.refused.remove(&one.of);
                }
                _ => {}
            }
        }
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

/// A standing condition of a core component's write-ahead stream, as its
/// members' reports say it ([`Judge::stuck_full`], [`Judge::refused`]):
/// opened as `open` while a member `says` it, once (`standing` holds the
/// components whose opening was kept, each with its window), and cleared
/// as `clear` once none of those reporting does. The window is the
/// earliest time a member says (`since`), or the hour judged where none
/// reads; the clearing carries the opening's. Each is `(name, words)`: the
/// opening's words follow the member's, the clearing's the component's.
fn standing(
    members: &[ComponentReport],
    now_ns: i64,
    standing: &BTreeMap<String, i64>,
    says: impl Fn(&str) -> bool,
    since: impl Fn(&str) -> Option<i64>,
    open: (&str, &str),
    clear: (&str, &str),
) -> Vec<Opened> {
    let hour_ns = now_ns.div_euclid(HOUR_NS) * HOUR_NS;
    // Each core component reporting, and of its members saying so the
    // one whose condition began first.
    let mut said: BTreeMap<&str, Option<(i64, &ComponentReport)>> = BTreeMap::new();
    for report in members
        .iter()
        .filter(|r| !r.component.starts_with("sidecar-"))
    {
        let saying = said.entry(report.component.as_str()).or_default();
        if says(&report.detail) {
            let began = since(&report.detail).unwrap_or(hour_ns);
            if saying.is_none_or(|(first, _)| began < first) {
                *saying = Some((began, report));
            }
        }
    }
    let mut opened = Vec::new();
    for (component, saying) in said {
        let (name, window_ns, about) = match (saying, standing.get(component)) {
            (Some((began, report)), None) => (
                open.0,
                began,
                format!(
                    "{component}/{}: {}. {}",
                    report.member, report.detail, open.1
                ),
            ),
            (None, Some(began)) => (clear.0, *began, format!("{component}: {}", clear.1)),
            _ => continue,
        };
        let kind = format!("component-{name}");
        opened.push(Opened {
            of: component.to_string(),
            plugin: false,
            event: format!("{kind}|{component}|{window_ns}"),
            kind,
            about,
            window_ns,
        });
    }
    opened
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

/// Who is told: a core component's deployment admins, a plugin's admins,
/// of every subject the records name -- each person who has signed in, and
/// each login a user group names, as a gap's notice is told
/// ([`crate::dataset_gaps::told`]) -- so an admin who has not signed in
/// since finds it in their inbox.
pub fn told(records: &AccessRecords, opened: &Opened) -> BTreeSet<String> {
    crate::prepared::subjects_named(records)
        .into_iter()
        .filter(|subject| {
            let access = crate::prepared::access_of(records, subject);
            if opened.plugin {
                access.administers(&opened.of)
            } else {
                access.deployment_admin
            }
        })
        .collect()
}

/// The openings with somebody to tell ([`told`]): what one judging files
/// and marks kept ([`Judge::kept`]). One with nobody to tell yet stays
/// unfiled, as a gap's does: a standing condition, or the conductor's
/// silence, is offered at the next judging, and told once an admin is
/// named.
pub fn heard_by(records: &AccessRecords, opened: Vec<Opened>) -> Vec<Opened> {
    opened
        .into_iter()
        .filter(|one| !told(records, one).is_empty())
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
                        // An answer, if not one that reads: the silence's
                        // count ends.
                        tracing::warn!("a ReadComponentHealthReply did not decode");
                        if let Ok(mut judge) = judge.lock() {
                            judge.heard();
                        }
                        continue;
                    }
                },
                Err(failed) => {
                    tracing::debug!(%failed, "the conductor did not answer ReadComponentHealth; asked again in 20 s");
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
            // Nobody to tell yet: left unfiled and unmarked, offered again.
            let opened = heard_by(&records, opened);
            if opened.is_empty() {
                continue;
            }
            let notices = notices_of(&records, &opened, now);
            let kept =
                tokio::task::spawn_blocking(move || crate::latency::notices::keep(&notices)).await;
            if !matches!(kept, Ok(Ok(()))) {
                tracing::error!("a component's notices were not kept");
                continue;
            }
            if let Ok(mut judge) = judge.lock() {
                judge.kept(&opened);
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
    /// One ask's interval, and the asks unanswered that are a silence.
    const ASK: i64 = ASK_EVERY.as_nanos() as i64;
    const ASKS: i64 = UNHEARD_ASKS as i64;

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

    /// v31a item 5 (0-J): the lake's writer saying its write-ahead stream
    /// is stuck full opens a standing notice to the deployment admins, once
    /// while any member says so -- the writer moved, its successor saying
    /// it too -- and its drained notice once none does; a later stuck opens
    /// again.
    #[test]
    fn a_stream_stuck_full_opens_a_standing_notice_once_and_a_drained_notice_when_it_drains() {
        let stuck = |member: &str| ComponentReport {
            member: member.into(),
            detail: format!(
                "{} for 30s: holds 256.0 MiB of its 256.0 MiB bound",
                meridian_bus::write_ahead::STUCK_FULL
            ),
            ..lake(MIB, 0, 0, 0)
        };
        let mut judge = Judge::default();
        assert!(judge.judge(&[lake(MIB, 0, 0, 0)], T0).is_empty());
        let opened = judge.judge(&[stuck("meridian-lake-0")], T0 + MINUTE);
        judge.kept(&opened);
        assert_eq!(kinds(&opened), vec!["component-write-ahead-stuck-full"]);
        assert!(!opened[0].plugin && opened[0].of == "lake");
        assert!(
            opened[0].about.contains("meridian-lake-0")
                && opened[0]
                    .about
                    .contains("holds 256.0 MiB of its 256.0 MiB bound"),
            "{}",
            opened[0].about
        );
        assert!(judge
            .judge(&[stuck("meridian-lake-0")], T0 + 2 * MINUTE)
            .is_empty());
        assert!(
            judge
                .judge(&[stuck("meridian-lake-1")], T0 + 3 * MINUTE)
                .is_empty(),
            "the writer moved, the stream still stuck: nothing new"
        );
        let opened = judge.judge(&[lake(MIB, 0, 0, 0)], T0 + 4 * MINUTE);
        assert_eq!(kinds(&opened), vec!["component-write-ahead-drained"]);
        judge.kept(&opened);
        assert!(judge
            .judge(&[lake(MIB, 0, 0, 0)], T0 + 5 * MINUTE)
            .is_empty());
        let opened = judge.judge(&[stuck("meridian-lake-1")], T0 + 120 * MINUTE);
        assert_eq!(kinds(&opened), vec!["component-write-ahead-stuck-full"]);
    }

    /// The lake's writer's words for a stream stuck full since `since`, as
    /// its guard says them.
    fn stuck_since(member: &str, since: &str) -> ComponentReport {
        ComponentReport {
            member: member.into(),
            detail: format!(
                "{} for 30s, since {since}: holds 256.0 MiB of its 256.0 MiB bound",
                meridian_bus::write_ahead::STUCK_FULL
            ),
            ..lake(MIB, 0, 0, 0)
        }
    }

    /// The review of 4c, fix B item 1: a stuck spell is one episode, keyed
    /// by the time its stream went stuck, which the lake's guard says. A
    /// second spell within the hour of the first is another episode, with
    /// its own stuck and drained notices.
    #[test]
    fn a_second_stuck_spell_within_the_hour_opens_its_own_notices() {
        use crate::latency::notices::{InMemory, NoticeStore as _};
        let (records, store) = (records(), InMemory::default());
        let mut judge = Judge::default();
        let mut events = Vec::new();
        let spells = [
            (Some("2026-10-26T07:33:50Z"), MINUTE),
            (None, 3 * MINUTE),
            (Some("2026-10-26T07:37:40Z"), 5 * MINUTE),
            (None, 7 * MINUTE),
        ];
        for (since, after) in spells {
            let report = match since {
                Some(since) => stuck_since("meridian-lake-0", since),
                None => lake(MIB, 0, 0, 0),
            };
            let opened = judge.judge(&[report], T0 + after);
            assert_eq!(opened.len(), 1, "{since:?}: {opened:#?}");
            store
                .notify(&notices_of(&records, &opened, T0 + after))
                .unwrap();
            judge.kept(&opened);
            events.push(opened[0].event.clone());
        }
        assert_ne!(events[0], events[2], "each spell its own event");
        assert_ne!(events[1], events[3]);
        let filed = store.notices("local|dee", 50).unwrap();
        assert_eq!(filed.len(), 4, "{filed:#?}");
    }

    /// Fix B item 1: two copies of the dashboard, judging the same episode
    /// either side of an hour's boundary -- or one started again within it
    /// -- file its stuck notice once, and its drained notice once.
    #[test]
    fn two_judges_seeing_one_stuck_episode_file_one_notice_and_one_drained() {
        use crate::latency::notices::{InMemory, NoticeStore as _};
        let (records, store) = (records(), InMemory::default());
        // The hour after T0 starts 1,600 s after it.
        let hour = T0 + 1_600_000_000_000;
        let (mut a, mut b) = (Judge::default(), Judge::default());
        let stuck = stuck_since("meridian-lake-0", "2026-10-26T07:59:00Z");
        for (stage, kind) in [
            (stuck, "component-write-ahead-stuck-full"),
            (lake(MIB, 0, 0, 0), "component-write-ahead-drained"),
        ] {
            for (judge, at) in [
                (&mut a, hour - 20_000_000_000),
                (&mut b, hour + 20_000_000_000),
            ] {
                let opened = judge.judge(std::slice::from_ref(&stage), at);
                assert_eq!(kinds(&opened), vec![kind]);
                store.notify(&notices_of(&records, &opened, at)).unwrap();
                judge.kept(&opened);
            }
        }
        // A copy started again while it stands stuck files none again.
        let mut again = Judge::default();
        let stuck = stuck_since("meridian-lake-0", "2026-10-26T07:59:00Z");
        let opened = again.judge(&[stuck], hour + MINUTE);
        store
            .notify(&notices_of(&records, &opened, hour + MINUTE))
            .unwrap();
        let filed = store.notices("local|dee", 50).unwrap();
        assert_eq!(filed.len(), 2, "{filed:#?}");
    }

    /// Fix B item 1: the judge takes a stream for stuck, or drained, only
    /// once its notice is kept: one whose keeping failed is offered again
    /// at the next judging, as the same event.
    #[test]
    fn a_stuck_notice_that_was_not_kept_is_offered_again() {
        let mut judge = Judge::default();
        let stuck = stuck_since("meridian-lake-0", "2026-10-26T07:33:50Z");
        let dropped = judge.judge(std::slice::from_ref(&stuck), T0 + MINUTE);
        let opened = judge.judge(std::slice::from_ref(&stuck), T0 + 2 * MINUTE);
        assert_eq!(kinds(&opened), vec!["component-write-ahead-stuck-full"]);
        assert_eq!(opened[0].event, dropped[0].event);
        judge.kept(&opened);
        assert!(judge.judge(&[stuck], T0 + 3 * MINUTE).is_empty());
        let dropped = judge.judge(&[lake(MIB, 0, 0, 0)], T0 + 4 * MINUTE);
        let opened = judge.judge(&[lake(MIB, 0, 0, 0)], T0 + 5 * MINUTE);
        assert_eq!(kinds(&opened), vec!["component-write-ahead-drained"]);
        assert_eq!(opened[0].event, dropped[0].event);
    }

    /// The lake's writer's words for the broker refusing its write-ahead
    /// stream since `since`, as its guard says them (INTERIM(v32), A1).
    fn refused_since(member: &str, since: &str) -> ComponentReport {
        ComponentReport {
            member: member.into(),
            detail: format!(
                "{} for 30s, since {since}: every look at the stream refused or not answered; \
                 the broker's last: JetStream system temporarily unavailable (503, 10008)",
                meridian_bus::write_ahead::REFUSED
            ),
            ..lake(MIB, 0, 0, 0)
        }
    }

    /// The proposal of 2026-11-27, A1 (INTERIM(v32)): the lake's writer
    /// saying the broker refuses its write-ahead writes, or does not answer
    /// them, opens a standing notice to the deployment admins once an
    /// episode -- the writer moved, its successor saying it too -- naming
    /// the component and inferring nothing more; and its accepted notice
    /// once no member says it. A second episode within the hour is its own.
    /// Neither is stuck full, nor drained.
    #[test]
    fn a_refused_write_ahead_opens_a_standing_notice_once_an_episode_and_an_accepted_notice() {
        let mut judge = Judge::default();
        let first = refused_since("meridian-lake-0", "2026-10-26T07:33:50Z");
        let opened = judge.judge(std::slice::from_ref(&first), T0 + MINUTE);
        assert_eq!(kinds(&opened), vec!["component-write-ahead-refused"]);
        assert!(!opened[0].plugin && opened[0].of == "lake");
        let about = &opened[0].about;
        for part in [
            "lake/meridian-lake-0",
            "refused or not answered by the broker",
        ] {
            assert!(about.contains(part), "{part} in {about}");
        }
        assert!(!about.contains("disk"), "nothing inferred: {about}");
        let event = opened[0].event.clone();
        judge.kept(&opened);
        assert!(judge.judge(&[first], T0 + 2 * MINUTE).is_empty());
        assert!(
            judge
                .judge(
                    &[refused_since("meridian-lake-1", "2026-10-26T07:35:10Z")],
                    T0 + 3 * MINUTE
                )
                .is_empty(),
            "the writer moved, still refused: nothing new"
        );
        let opened = judge.judge(&[lake(MIB, 0, 0, 0)], T0 + 4 * MINUTE);
        assert_eq!(kinds(&opened), vec!["component-write-ahead-accepted"]);
        assert!(opened[0].about.starts_with("lake: "), "{}", opened[0].about);
        assert_eq!(opened[0].window_ns, judge.refused["lake"]);
        judge.kept(&opened);
        assert!(judge
            .judge(&[lake(MIB, 0, 0, 0)], T0 + 5 * MINUTE)
            .is_empty());
        let opened = judge.judge(
            &[refused_since("meridian-lake-1", "2026-10-26T07:39:00Z")],
            T0 + 6 * MINUTE,
        );
        assert_eq!(kinds(&opened), vec!["component-write-ahead-refused"]);
        assert_ne!(opened[0].event, event, "a second episode its own event");
    }

    /// Stuck full files as it did beside a refusal: a member saying both
    /// opens both notices, each its own; refused alone opens no stuck-full
    /// notice, stuck alone no refused one; and each clears on its own.
    #[test]
    fn stuck_full_and_refused_open_their_own_notices_side_by_side() {
        let mut judge = Judge::default();
        let stuck = stuck_since("meridian-lake-0", "2026-10-26T07:33:20Z");
        let opened = judge.judge(std::slice::from_ref(&stuck), T0 + MINUTE);
        assert_eq!(kinds(&opened), vec!["component-write-ahead-stuck-full"]);
        judge.kept(&opened);
        let both = ComponentReport {
            detail: format!(
                "{}; {}",
                stuck.detail,
                refused_since("meridian-lake-0", "2026-10-26T07:34:00Z").detail
            ),
            ..stuck.clone()
        };
        let opened = judge.judge(std::slice::from_ref(&both), T0 + 2 * MINUTE);
        assert_eq!(kinds(&opened), vec!["component-write-ahead-refused"]);
        judge.kept(&opened);
        let opened = judge.judge(std::slice::from_ref(&stuck), T0 + 3 * MINUTE);
        assert_eq!(kinds(&opened), vec!["component-write-ahead-accepted"]);
        judge.kept(&opened);
        let opened = judge.judge(&[lake(MIB, 0, 0, 0)], T0 + 4 * MINUTE);
        assert_eq!(kinds(&opened), vec!["component-write-ahead-drained"]);
        judge.kept(&opened);
        assert!(judge.stuck_full.is_empty() && judge.refused.is_empty());
    }

    /// Like a stuck notice, a refused or accepted notice whose keeping
    /// failed is offered again at the next judging, as the same event.
    #[test]
    fn a_refused_notice_that_was_not_kept_is_offered_again() {
        let mut judge = Judge::default();
        let refused = refused_since("meridian-lake-0", "2026-10-26T07:33:50Z");
        let dropped = judge.judge(std::slice::from_ref(&refused), T0 + MINUTE);
        let opened = judge.judge(std::slice::from_ref(&refused), T0 + 2 * MINUTE);
        assert_eq!(kinds(&opened), vec!["component-write-ahead-refused"]);
        assert_eq!(opened[0].event, dropped[0].event);
        judge.kept(&opened);
        let dropped = judge.judge(&[lake(MIB, 0, 0, 0)], T0 + 3 * MINUTE);
        let opened = judge.judge(&[lake(MIB, 0, 0, 0)], T0 + 4 * MINUTE);
        assert_eq!(kinds(&opened), vec!["component-write-ahead-accepted"]);
        assert_eq!(opened[0].event, dropped[0].event);
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

    /// A deployment admin a user group names by login, who has not signed
    /// in, is told a core component's notice, as a gap's is
    /// ([`crate::dataset_gaps::told`]): found in their inbox at their first
    /// sign-in. Failover-repeat 46090125342 judged the refused notice and
    /// told nobody, its only deployment admin never signed in. A plugin's
    /// admin likewise.
    #[test]
    fn a_deployment_admin_who_has_not_signed_in_is_told() {
        let mut records = records();
        records.people.clear();
        let mut judge = Judge::default();
        let opened = judge.judge(
            &[refused_since("meridian-lake-0", "2026-10-26T07:33:50Z")],
            T0,
        );
        assert_eq!(kinds(&opened), vec!["component-write-ahead-refused"]);
        assert_eq!(
            told(&records, &opened[0]),
            BTreeSet::from(["local|dee".to_string()])
        );
        let filed = notices_of(&records, &opened, T0);
        assert_eq!(filed.len(), 1, "one notice, to the admin not signed in");
        assert_eq!(filed[0].subject, "local|dee");
        assert_eq!(filed[0].kind, "component-write-ahead-refused");
        // Somebody signed in who holds nothing is still told nothing.
        records.people.push(meridian_domain::v1::SignInRecord {
            subject: "local|bob".into(),
            ..Default::default()
        });
        assert_eq!(
            told(&records, &opened[0]),
            BTreeSet::from(["local|dee".to_string()])
        );
    }

    /// With nobody to tell, an episode stays unfiled and is not marked
    /// kept, as a gap's is ([`crate::dataset_gaps::judge_forever`]): it is
    /// offered at every judging, and a deployment admin named later is told
    /// it, once. The conductor's silence likewise.
    #[test]
    fn with_nobody_to_tell_an_episode_stays_unfiled_and_an_admin_named_later_is_told() {
        let nobody = AccessRecords::default();
        let named = records();
        let ask = ASK_EVERY.as_nanos() as i64;
        let said = [refused_since("meridian-lake-0", "2026-10-26T07:33:50Z")];
        let mut judge = Judge::default();
        let opened = judge.judge(&said, T0);
        assert_eq!(kinds(&opened), vec!["component-write-ahead-refused"]);
        let heard = heard_by(&nobody, opened);
        assert!(heard.is_empty(), "nobody to tell: {heard:#?}");
        judge.kept(&heard);
        // Offered again, and told to the admin the records now name.
        let opened = judge.judge(&said, T0 + ask);
        assert_eq!(kinds(&opened), vec!["component-write-ahead-refused"]);
        let heard = heard_by(&named, opened);
        let filed = notices_of(&named, &heard, T0 + ask);
        assert_eq!(filed.len(), 1, "{filed:#?}");
        assert_eq!(filed[0].subject, "local|dee");
        judge.kept(&heard);
        assert!(judge.judge(&said, T0 + 2 * ask).is_empty(), "told once");

        let mut judge = Judge::default();
        let mut opened = Vec::new();
        for n in 1..=i64::from(UNHEARD_ASKS) {
            opened = judge.unanswered(T0 + n * ask);
        }
        assert_eq!(kinds(&opened), vec!["component-unheard"]);
        let heard = heard_by(&nobody, opened);
        assert!(heard.is_empty());
        judge.kept(&heard);
        let opened = judge.unanswered(T0 + (i64::from(UNHEARD_ASKS) + 1) * ask);
        assert_eq!(heard_by(&named, opened).len(), 1, "offered again");
    }

    /// The conductor is asked every 20 s, so a member's words reach the
    /// judge within 20 s of the conductor hearing them (the consensus of
    /// 2026-11-28, part 2 (B): the refused notice within its 120 s), and
    /// its silence is a notice at the ninth ask in a row, not the eighth:
    /// three minutes, as it was at three asks a minute apart.
    #[test]
    fn the_conductor_is_asked_every_twenty_seconds_and_its_silence_is_a_notice_at_three_minutes() {
        assert_eq!(ASK_EVERY, Duration::from_secs(20));
        assert_eq!(ASK_EVERY * UNHEARD_ASKS, Duration::from_secs(180));
        let ask = ASK_EVERY.as_nanos() as i64;
        let mut judge = Judge::default();
        for n in 1..=8 {
            assert!(judge.unanswered(T0 + n * ask).is_empty(), "ask {n}");
        }
        let opened = judge.unanswered(T0 + 9 * ask);
        assert_eq!(kinds(&opened), vec!["component-unheard"]);
        assert!(
            opened[0].about.contains("for 9 asks in a row")
                && opened[0].about.contains(&crate::custody::utc(T0 + ask)),
            "{}",
            opened[0].about
        );
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
    /// nine asks unanswered in a row are a notice about the conductor to
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
            let mut ask = |answered: bool, n: i64| {
                let at = hour + n * ASK + offset;
                let opened = if answered {
                    judge.judge(&heard, at)
                } else {
                    judge.unanswered(at)
                };
                store.notify(&notices_of(&records, &opened, at)).unwrap();
            };
            ask(true, 0);
            for n in 1..=ASKS {
                ask(false, n);
            }
            ask(true, ASKS + 3);
            for n in ASKS + 4..=2 * ASKS + 3 {
                ask(false, n);
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
        let mut ask = |answered: bool, n: i64| {
            let at = eleven + n * ASK;
            let opened = if answered {
                judge.judge(&heard, at)
            } else {
                judge.unanswered(at)
            };
            store.notify(&notices_of(&records, &opened, at)).unwrap();
        };
        // Unheard for nine asks, from 10:57:20 to 11:00:00; answered at
        // 11:00:20; unheard for nine more, 11:00:40 to 11:03:20.
        for n in 1 - ASKS..=0 {
            ask(false, n);
        }
        ask(true, 1);
        for n in 2..=ASKS + 1 {
            ask(false, n);
        }
        let filed = store.notices("local|dee", 50).unwrap();
        assert_eq!(filed.len(), 1, "{filed:#?}");
    }

    /// The silence's opening is offered until it is kept (the delta review
    /// of 77afd5d0, finding 3): one dropped -- the records stale, or the
    /// keeping failed -- is offered again at the next ask; its keyed ID
    /// makes a repeat harmless. Once kept, none again while it is silent.
    #[test]
    fn a_silence_whose_notice_was_not_kept_is_offered_again() {
        let mut judge = Judge::default();
        for n in 1..=ASKS {
            let _dropped = judge.unanswered(T0 + n * ASK);
        }
        let opened = judge.unanswered(T0 + (ASKS + 1) * ASK);
        assert_eq!(kinds(&opened), vec!["component-unheard"]);
        judge.kept(&opened);
        assert!(judge.unanswered(T0 + (ASKS + 2) * ASK).is_empty());
    }

    /// An answer that does not decode is an answer: it ends the silence's
    /// count (the delta review of 77afd5d0, finding 4).
    #[test]
    fn an_answer_that_does_not_read_ends_the_silences_count() {
        let mut judge = Judge::default();
        for n in 1..ASKS {
            assert!(judge.unanswered(T0 + n * ASK).is_empty());
        }
        judge.heard();
        for n in ASKS..2 * ASKS - 1 {
            assert!(judge.unanswered(T0 + n * ASK).is_empty(), "ask {n}");
        }
    }

    #[test]
    fn a_member_unheard_for_a_day_is_let_go() {
        let mut judge = Judge::default();
        judge.judge(&[lake(MIB, 0, 0, 0)], T0);
        judge.judge(&[], T0 + KEPT_NS + MINUTE);
        assert!(judge.day_of("lake/meridian-lake-0").is_none());
    }
}
