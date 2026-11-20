//! A latency or backlog episode's notices (contract v28; W4.16; the latency
//! spec's "Bands, breaches and notices"; rulings 415, 427 and 606).
//!
//! The judge ([`meridian_domain::latency::Monitor::judge`]) opens and closes
//! episodes on each whole minute; each opening and each closing is one
//! notice, its own record (decisions/031), to every person told: the
//! plugin's admins, and for a connectivity plugin's session the admins of
//! the `ems` its link names (W11's "one `ems` per connectivity plugin"). A
//! core component's own hop -- the lake's, the analytics component's -- is
//! no plugin's, so its episodes go to the deployment admins. Read in the
//! Inbox and by `dashboard__read_inbox`, beside the tickets' notices.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, OnceLock};

use meridian_domain::latency::{Scope, Transition};
use meridian_domain::v1::AccessRecords;
use serde_json::{json, Value};

use crate::database::{said, Database};
use crate::html::escape;
use crate::web::App;

/// How many of a person's latency notices the Inbox shows.
pub const SHOWN: usize = 50;

/// How often the judge runs: often enough that a whole minute is judged
/// within a quarter of a minute of its end.
pub const JUDGE_EVERY_SECS: u64 = 15;

/// One notice to one person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub notice_id: String,
    pub subject: String,
    /// The plugin instance, or the core component, the series is of.
    pub plugin_instance_id: String,
    /// `latency_breached`, `latency_recovered`, `backlog_breached` or
    /// `backlog_recovered`.
    pub kind: String,
    /// The dashboard's own words for the series and the minute.
    pub about: String,
    pub changed_ns: i64,
}

/// Where the notices are kept: the dashboard's tables (migration 10), or in
/// memory for tests and a deployment with no database.
pub trait NoticeStore: Send + Sync {
    fn notify(&self, notices: &[Notice]) -> Result<(), String>;
    fn notices(&self, subject: &str, limit: usize) -> Result<Vec<Notice>, String>;
    /// What every notice of `kind` was about, to anyone: what a judge reads
    /// at its start so a notice filed once is never filed again after a
    /// restart (`gap_found`, once per gap; contract v30, stage D).
    fn abouts(&self, kind: &str) -> Result<Vec<String>, String>;
}

#[derive(Default)]
pub struct InMemory {
    held: Mutex<Vec<Notice>>,
}

impl NoticeStore for InMemory {
    fn notify(&self, notices: &[Notice]) -> Result<(), String> {
        let mut held = self
            .held
            .lock()
            .map_err(|_| "the notices' lock".to_string())?;
        // Kept once by its ID, as the dashboard's table keeps it.
        for notice in notices {
            if !held.iter().any(|n| n.notice_id == notice.notice_id) {
                held.push(notice.clone());
            }
        }
        Ok(())
    }

    fn notices(&self, subject: &str, limit: usize) -> Result<Vec<Notice>, String> {
        let mut found: Vec<Notice> = self
            .held
            .lock()
            .map_err(|_| "the notices' lock".to_string())?
            .iter()
            .filter(|n| n.subject == subject)
            .cloned()
            .collect();
        found.sort_by(|a, b| b.notice_id.cmp(&a.notice_id));
        found.truncate(limit);
        Ok(found)
    }

    fn abouts(&self, kind: &str) -> Result<Vec<String>, String> {
        Ok(self
            .held
            .lock()
            .map_err(|_| "the notices' lock".to_string())?
            .iter()
            .filter(|n| n.kind == kind)
            .map(|n| n.about.clone())
            .collect())
    }
}

pub struct InPostgres {
    database: Database,
}

impl InPostgres {
    pub fn on(database: Database) -> Self {
        Self { database }
    }
}

impl NoticeStore for InPostgres {
    fn notify(&self, notices: &[Notice]) -> Result<(), String> {
        let mut conn = self.database.conn()?;
        let mut tx = conn.transaction().map_err(said)?;
        for notice in notices {
            tx.execute(
                "INSERT INTO dashboard_notice (notice_id, subject, ticket_id, kind,
                     author_provenance, plugin_instance_id, about, changed_ns, read)
                 VALUES ($1, $2, NULL, $3, 'rules', $4, $5, $6, false)
                 ON CONFLICT (notice_id) DO NOTHING",
                &[
                    &notice.notice_id,
                    &notice.subject,
                    &notice.kind,
                    &notice.plugin_instance_id,
                    &notice.about,
                    &notice.changed_ns,
                ],
            )
            .map_err(said)?;
        }
        tx.commit().map_err(said)
    }

    fn notices(&self, subject: &str, limit: usize) -> Result<Vec<Notice>, String> {
        let mut conn = self.database.conn()?;
        let rows = conn
            .query(
                "SELECT notice_id, subject, plugin_instance_id, kind, about, changed_ns
                   FROM dashboard_notice
                  WHERE subject = $1 AND plugin_instance_id <> ''
                  ORDER BY notice_id DESC LIMIT $2",
                &[&subject, &(limit as i64)],
            )
            .map_err(said)?;
        Ok(rows
            .iter()
            .map(|row| Notice {
                notice_id: row.get(0),
                subject: row.get(1),
                plugin_instance_id: row.get(2),
                kind: row.get(3),
                about: row.get(4),
                changed_ns: row.get(5),
            })
            .collect())
    }

    fn abouts(&self, kind: &str) -> Result<Vec<String>, String> {
        let mut conn = self.database.conn()?;
        Ok(conn
            .query(
                "SELECT DISTINCT about FROM dashboard_notice WHERE kind = $1",
                &[&kind],
            )
            .map_err(said)?
            .iter()
            .map(|row| row.get(0))
            .collect())
    }
}

static STORE: OnceLock<Arc<dyn NoticeStore>> = OnceLock::new();

/// Keep the notices in `store` from now on: the runtime's, at start. Once.
pub fn keep_in(store: Arc<dyn NoticeStore>) {
    let _ = STORE.set(store);
}

fn store() -> Arc<dyn NoticeStore> {
    Arc::clone(STORE.get_or_init(|| Arc::new(InMemory::default())))
}

/// Keep notices made elsewhere on this path: a component's or a plugin's
/// health (W6.40; stage M), read beside the latency judge's.
pub fn keep(notices: &[Notice]) -> Result<(), String> {
    store().notify(notices)
}

/// What every notice of `kind` was about: see [`NoticeStore::abouts`].
pub async fn abouts(kind: &str) -> Result<Vec<String>, String> {
    let kind = kind.to_string();
    tokio::task::spawn_blocking(move || store().abouts(&kind))
        .await
        .map_err(|failed| failed.to_string())?
}

/// A person's latest latency notices, newest first.
pub async fn of(subject: &str) -> Vec<Notice> {
    let subject = subject.to_string();
    tokio::task::spawn_blocking(move || store().notices(&subject, SHOWN))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default()
}

fn ms(ns: i64) -> String {
    let ms = ns as f64 / 1e6;
    if ms >= 100.0 {
        format!("{ms:.0}")
    } else {
        format!("{ms:.1}")
    }
}

/// The dashboard's words for a transition: the series, and what its minute
/// held against what.
pub fn about(transition: &Transition) -> String {
    let series = match &transition.key.scope {
        Scope::Session { session, metric } => {
            format!("{session} {}", super::metric_word(metric))
        }
        Scope::Dataset { dataset, hop } => format!("{dataset} at {}", super::hop_word(hop)),
    };
    let held = if transition.kind.starts_with("backlog") {
        format!("backlog {}", transition.value)
    } else {
        format!("p99 {} ms", ms(transition.value))
    };
    let against = match (transition.kind, transition.ceiling) {
        (_, 0) if transition.kind == "latency_breached" => {
            " (3 sigma above its own week)".to_string()
        }
        (_, 0) => String::new(),
        (kind, ceiling) if kind.starts_with("backlog") => format!(" (ceiling {ceiling})"),
        (_, ceiling) => format!(" (ceiling {} ms)", ms(ceiling)),
    };
    format!("{series}: {held}{against}")
}

/// Who is told of an episode on `instance`'s series (ruling 427): its
/// admins, and for a session of a connectivity plugin linked to an `ems`
/// that `ems`'s admins; for a core component's own hop, the deployment
/// admins. Only people who have signed in, as every notice.
pub fn told(records: &AccessRecords, instance: &str, scope: &Scope) -> BTreeSet<String> {
    let linked_ems: Option<String> = match scope {
        Scope::Session { .. } => records
            .connectivity_links
            .iter()
            .find(|link| link.connectivity_instance_id == instance)
            .map(|link| link.ems_instance_id.clone())
            .filter(|ems| !ems.is_empty()),
        Scope::Dataset { .. } => None,
    };
    let a_plugin = records
        .known_plugins
        .iter()
        .any(|held| held.plugin_instance_id == instance);
    records
        .people
        .iter()
        .filter(|person| {
            let access = crate::prepared::access_of(records, &person.subject);
            if a_plugin {
                access.administers(instance)
                    || linked_ems
                        .as_deref()
                        .is_some_and(|ems| access.administers(ems))
            } else {
                access.deployment_admin
            }
        })
        .map(|person| person.subject.clone())
        .collect()
}

/// A notice's ID that names its event and the person told, so every
/// dashboard copy (ha runs two, each its own judge) and every restart files
/// the same notice, which the store keeps once (review-F headline 2): a
/// ULID whose time is the event's window and whose rest is the digest of
/// the event's key and the person. The gap judge's first
/// ([`crate::dataset_gaps::notice_id`], its key the gap's ID).
pub fn keyed_id(window_ns: i64, key: &str, subject: &str) -> String {
    use sha2::Digest;
    const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let digest = sha2::Sha256::digest(format!("{key}\0{subject}").as_bytes());
    let mut rest = [0u8; 16];
    rest.copy_from_slice(&digest[..16]);
    let ms = (window_ns.max(0) / 1_000_000) as u128;
    let value: u128 = (ms << 80) | (u128::from_be_bytes(rest) & ((1u128 << 80) - 1));
    (0..26)
        .rev()
        .map(|i| CROCKFORD[((value >> (i * 5)) & 31) as usize] as char)
        .collect()
}

/// A transition's event: its kind, its series and the minute judged, which
/// every judge of the same series names alike.
fn event_of(transition: &Transition) -> String {
    let series = match &transition.key.scope {
        Scope::Session { session, metric } => format!("session|{session}|{metric}"),
        Scope::Dataset { dataset, hop } => format!("dataset|{dataset}|{hop}"),
    };
    format!(
        "{}|{}|{series}|{}",
        transition.kind, transition.key.instance, transition.at_ns
    )
}

/// The notices one judging makes: one per transition per person told,
/// each ID its event's ([`keyed_id`]).
pub fn notices_of(records: &AccessRecords, transitions: &[Transition], now_ns: i64) -> Vec<Notice> {
    let mut out = Vec::new();
    for transition in transitions {
        let instance = &transition.key.instance;
        let event = event_of(transition);
        for subject in told(records, instance, &transition.key.scope) {
            out.push(Notice {
                notice_id: keyed_id(transition.at_ns, &event, &subject),
                subject,
                plugin_instance_id: instance.clone(),
                kind: transition.kind.to_string(),
                about: about(transition),
                changed_ns: now_ns,
            });
        }
    }
    out
}

/// Judge the monitor's whole minutes every [`JUDGE_EVERY_SECS`] from now on,
/// keeping each episode's notices. The task is handed back, for the
/// component to end before its runtime stops (meridian_runtime::keep).
pub fn judge_forever(app: Arc<App>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut every = tokio::time::interval(std::time::Duration::from_secs(JUDGE_EVERY_SECS));
        loop {
            every.tick().await;
            let now = app.clock.now_ns();
            let transitions = meridian_domain::latency::monitor().judge(now);
            if transitions.is_empty() {
                continue;
            }
            let Ok(records) = app.records.current(now) else {
                // Told nobody rather than the wrong people; the episode
                // stays open, and its closing is still told.
                tracing::warn!(
                    episodes = transitions.len(),
                    "a latency episode was judged while the access records were stale; no notice"
                );
                continue;
            };
            let notices = notices_of(&records, &transitions, now);
            if notices.is_empty() {
                continue;
            }
            let kept = tokio::task::spawn_blocking(move || store().notify(&notices)).await;
            if !matches!(kept, Ok(Ok(()))) {
                tracing::error!("a latency episode's notices were not kept");
            }
        }
    })
}

/// A notice as `dashboard__read_inbox` answers it.
pub fn json(notice: &Notice) -> Value {
    json!({
        "notice_id": notice.notice_id,
        "kind": notice.kind,
        "plugin_instance_id": notice.plugin_instance_id,
        "about": notice.about,
        "changed_ns": notice.changed_ns,
    })
}

/// The Inbox's lines for them, above the tickets': one line each. Empty
/// where there are none.
pub fn rows(notices: &[Notice]) -> String {
    if notices.is_empty() {
        return String::new();
    }
    let lines: String = notices
        .iter()
        .map(|notice| {
            // One word a badge, so it fits a phone; the series, and backlog
            // or p99, in the line beside it.
            let (badge, tone) = match notice.kind.as_str() {
                kind if kind.ends_with("breached") => ("Breached", " warn"),
                // A component's or a plugin's health (W6.40; stage M).
                kind if kind.ends_with("-restarted") => ("Restarted", " warn"),
                kind if kind.ends_with("-over-budget") => ("Over budget", " warn"),
                kind if kind.ends_with("-shedding") => ("Shedding", " warn"),
                kind if kind.ends_with("-restarting-repeatedly") => ("Restarting", " warn"),
                kind if kind.ends_with("-memory-climbing") => ("Climbing", " warn"),
                // The conductor silent (approved 2026-11-20).
                kind if kind.ends_with("-unheard") => ("Unheard", " warn"),
                // A gap in a dataset (W10.14; contract v30, stage D).
                "gap_found" => ("Gap found", " warn"),
                _ => ("Recovered", ""),
            };
            format!(
                "<tr data-kind=\"{kind}\" data-plugin=\"{instance}\"><td><span class=\"badge{tone}\" title=\"{kind}\">{badge}</span></td>\
                 <td title=\"{instance}: {about}\"><strong>{instance}</strong> {about}</td>\
                 <td>{when}</td></tr>",
                kind = escape(&notice.kind),
                instance = escape(&notice.plugin_instance_id),
                about = escape(&notice.about),
                when = crate::custody::utc(notice.changed_ns),
            )
        })
        .collect();
    format!(
        "<om-pager search=\"Search latency notices\" data-latency-notices><table class=\"list one-line notices\">\
         <thead><tr><th>What</th><th>Plugin</th><th>When</th></tr></thead>\
         <tbody>{lines}</tbody></table></om-pager>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use meridian_domain::latency::Key;
    use meridian_domain::v1::{
        AccessEntry, AccessGroup, KnownPluginRoles, Permission, SignInRecord, UserGroup,
    };
    use meridian_pb::v1::ConnectivityLink;

    fn group(id: &str, login: &str) -> UserGroup {
        UserGroup {
            user_group_id: id.into(),
            name: id.into(),
            directory_groups: vec![],
            logins: vec![login.into()],
        }
    }

    fn admin_of(
        group: &str,
        access_group: &str,
        instance: &str,
        role: &str,
    ) -> (Permission, AccessGroup) {
        (
            Permission {
                permission_id: format!("P-{group}"),
                user_group_id: group.into(),
                account_group_id: String::new(),
                access_group_id: access_group.into(),
            },
            AccessGroup {
                access_group_id: access_group.into(),
                name: access_group.into(),
                entries: vec![AccessEntry {
                    plugin_instance_id: instance.into(),
                    level: meridian_access::AccessLevel::Admin as i32,
                    role: role.into(),
                }],
                built_in: false,
            },
        )
    }

    /// Ola administers fix-1, Eve the ems-1 it is linked to, Dee the
    /// deployment, and Nils nothing.
    fn records() -> AccessRecords {
        let (p1, a1) = admin_of("UG-OLA", "AG-FIX", "fix-1", "connectivity");
        let (p2, a2) = admin_of("UG-EVE", "AG-EMS", "ems-1", "ems");
        AccessRecords {
            user_groups: vec![
                group("UG-OLA", "local|ola"),
                group("UG-EVE", "local|eve"),
                group("UG-DEE", "local|dee"),
            ],
            permissions: vec![
                p1,
                p2,
                Permission {
                    permission_id: "P-DEE".into(),
                    user_group_id: "UG-DEE".into(),
                    account_group_id: String::new(),
                    access_group_id: meridian_access::DEPLOYMENT_ADMIN.into(),
                },
            ],
            access_groups: vec![a1, a2],
            people: ["local|ola", "local|eve", "local|dee", "local|nils"]
                .iter()
                .map(|s| SignInRecord {
                    subject: (*s).into(),
                    ..Default::default()
                })
                .collect(),
            known_plugins: vec![
                KnownPluginRoles {
                    plugin_instance_id: "fix-1".into(),
                    roles: vec!["connectivity".into()],
                },
                KnownPluginRoles {
                    plugin_instance_id: "ems-1".into(),
                    roles: vec!["ems".into()],
                },
            ],
            connectivity_links: vec![ConnectivityLink {
                connectivity_instance_id: "fix-1".into(),
                available: true,
                ems_instance_id: "ems-1".into(),
                users: vec![],
            }],
            ..Default::default()
        }
    }

    fn transition(instance: &str, scope: Scope, kind: &'static str, ceiling: i64) -> Transition {
        Transition {
            key: Key {
                instance: instance.into(),
                scope,
            },
            kind,
            at_ns: 1_792_400_040_000_000_000,
            value: 2_300_000_000,
            ceiling,
        }
    }

    #[test]
    fn a_sessions_episode_is_told_to_its_admins_and_its_emss_and_a_hops_to_its_admins_alone() {
        let records = records();
        let session = Scope::Session {
            session: "fix-order".into(),
            metric: "LATENCY_METRIC_RTT".into(),
        };
        let told_of = |instance: &str, scope: &Scope| -> Vec<String> {
            told(&records, instance, scope).into_iter().collect()
        };
        assert_eq!(told_of("fix-1", &session), vec!["local|eve", "local|ola"]);
        let hop = Scope::Dataset {
            dataset: "fix-1:quotes".into(),
            hop: "AGE_HOP_BUS".into(),
        };
        assert_eq!(told_of("fix-1", &hop), vec!["local|ola"]);
        // The lake's own hop is no plugin's: the deployment admins'.
        let lake = Scope::Dataset {
            dataset: "fix-1:quotes".into(),
            hop: "AGE_HOP_LAKE".into(),
        };
        assert_eq!(told_of("lake-1", &lake), vec!["local|dee"]);
    }

    #[test]
    fn each_transition_is_one_notice_per_person_in_the_dashboards_words() {
        let records = records();
        let notices = notices_of(
            &records,
            &[
                transition(
                    "fix-1",
                    Scope::Session {
                        session: "fix-order".into(),
                        metric: "LATENCY_METRIC_RTT".into(),
                    },
                    "latency_breached",
                    250_000_000,
                ),
                transition(
                    "fix-1",
                    Scope::Dataset {
                        dataset: "fix-1:quotes".into(),
                        hop: "AGE_HOP_BUS".into(),
                    },
                    "latency_breached",
                    0,
                ),
            ],
            7,
        );
        assert_eq!(notices.len(), 3);
        assert_eq!(
            notices[0].about,
            "fix-order round trip: p99 2300 ms (ceiling 250 ms)"
        );
        assert_eq!(
            notices[2].about,
            "fix-1:quotes at bus: p99 2300 ms (3 sigma above its own week)"
        );
        assert!(notices
            .iter()
            .all(|n| n.changed_ns == 7 && n.plugin_instance_id == "fix-1"));
        let store = InMemory::default();
        store.notify(&notices).unwrap();
        let ola = store.notices("local|ola", SHOWN).unwrap();
        assert_eq!(ola.len(), 2);
        // Each ID its event's and the person's: filed again, kept once.
        store.notify(&notices).unwrap();
        assert_eq!(store.notices("local|ola", SHOWN).unwrap(), ola);
        assert_eq!(
            &ola[0].notice_id[..10],
            &keyed_id(1_792_400_040_000_000_000, "", "")[..10]
        );
        assert_ne!(ola[0].notice_id, ola[1].notice_id);
        let drawn = rows(&ola);
        assert!(drawn.contains(">Breached<"), "{drawn}");
        assert!(drawn.contains("<strong>fix-1</strong>"), "{drawn}");
        assert_eq!(json(&ola[0])["kind"], "latency_breached");
    }

    /// ha runs two dashboards, each its own judge over the same series
    /// (review-F headline 2): one episode, one notice to each person, in
    /// the one store both keep their notices in.
    #[test]
    fn two_judges_over_one_store_file_one_notice() {
        use meridian_domain::latency::{Histogram, Monitor, Said};
        const MINUTE: i64 = 60_000_000_000;
        let t0 = (1_792_400_000_000_000_000 / MINUTE) * MINUTE;
        let key = Key {
            instance: "fix-1".into(),
            scope: Scope::Session {
                session: "fix-order".into(),
                metric: "LATENCY_METRIC_RTT".into(),
            },
        };
        let mut slow = Histogram::default();
        for _ in 0..50 {
            slow.record(2_000_000_000);
        }
        let said = Said {
            ceiling_ns: 250_000_000,
            ..Default::default()
        };
        let (a, b) = (Monitor::default(), Monitor::default());
        for minute in 0..4 {
            for judge in [&a, &b] {
                judge.merge(key.clone(), t0 + minute * MINUTE + 1, &slow, said);
            }
        }
        let records = records();
        let store = InMemory::default();
        // Each copy judges the same whole minutes, seconds apart.
        for (judge, at) in [
            (&a, t0 + 4 * MINUTE + 3_000_000_000),
            (&b, t0 + 4 * MINUTE + 17_000_000_000),
        ] {
            let transitions = judge.judge(at);
            assert_eq!(transitions.len(), 1, "{transitions:?}");
            store
                .notify(&notices_of(&records, &transitions, at))
                .unwrap();
        }
        for person in ["local|ola", "local|eve"] {
            assert_eq!(store.notices(person, SHOWN).unwrap().len(), 1, "{person}");
        }
    }
}
