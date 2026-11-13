//! What the deployment survives, as the conductor answers it (W6.39;
//! contract v30, stage D; the resilience spec's section 12).
//!
//! - **`ReadResilience`** on `platform.config.query.read-resilience`: each
//!   member by component with its mode, zone and lag, as it reported last;
//!   each partition of the lake and the book with its writer and epoch; and
//!   the history, newest first and paged: each store's takeovers and
//!   integrity findings and the lake's source switches, read from each
//!   store's `control` partition over its own
//!   `platform.{domain}.internal.read-changes` ([`meridian_changes::records`]).
//!   The book's account moves and its own takeovers and findings join it as
//!   a [`Source`] when its journal carries them.
//! - **`ReadResilienceProfile`** on
//!   `platform.deployment.query.read-resilience-profile`: per failure class
//!   (section 10) and per tier (1048), met, met by restart or not met, each
//!   not met naming its change and cost; never blind per live data type;
//!   the level and what the next lacks. Computed from the configuration
//!   ([`Values`]: the chart's as the release states them where the chart
//!   says them, else what runs), the members' reports, the pods' nodes, the
//!   Leases and the lake's source priorities -- never from a preset's name.
//!   A class or tier is met only where CI tests that configuration: the
//!   tested configurations are the claims ledger's classes and profiles,
//!   each from a green run ([`Tested`]), never a hand list.
//!
//! Both are reads. Neither changes anything, and neither carries a secret.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meridian_bus::Bus;
use meridian_changes::records::{self, Record};
use meridian_domain::v1::{
    resilience_history_item::Item, ComponentReport, NeverBlindLine, PartitionOwner,
    ReadResilienceProfileReply, ReadResilienceReply, ReadResilienceRequest, ResilienceChange,
    ResilienceHistoryItem, ResilienceLine, ResilienceState, ServiceLevel, SourcePriority,
    SourceSwitched,
};
use prost::Message;
use serde_json::Value;

/// The Resilience page's read (W6.39; step 3c's question 4).
pub const READ_RESILIENCE_TOPIC: &str = "platform.config.query.read-resilience";

/// The resilience profile's read (W6.39).
pub const READ_RESILIENCE_PROFILE_TOPIC: &str = "platform.deployment.query.read-resilience-profile";

/// A page of the history where none is asked, and the most one holds.
pub const HISTORY_PAGE: usize = 50;
pub const HISTORY_PAGE_MOST: usize = 200;

/// The newest history items the conductor holds: every record is kept for
/// good in its store, and the page shows the recent ones. A bound, so the
/// history never grows the conductor's memory with the deployment's age.
pub const HISTORY_KEPT: usize = 2_000;

/// How long an ask of another component waits.
const ASK: Duration = Duration::from_secs(3);

/// The stores with members (a writer, a standby, readers), as their reports
/// and the chart's values name them, and how a person reads each.
pub const STORES: &[(&str, &str)] = &[
    ("lake", "the lake"),
    ("bor", "the book"),
    ("instrument", "the instrument store"),
    ("street", "the street store"),
    ("conductor", "the conductor"),
    ("launcher", "the launcher"),
];

fn store_name(component: &str) -> &str {
    STORES
        .iter()
        .find(|(c, _)| *c == component)
        .map_or(component, |(_, name)| name)
}

// ── What runs: the cluster's pods and Leases ────────────────────────────────

/// A pod of the release, as the cluster says it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pod {
    pub name: String,
    /// Its `meridian.dev/component` label.
    pub component: String,
    /// A plugin's pod's `meridian.dev/instance`; empty for the rest.
    pub instance: String,
    /// The node it runs on; empty while it is not scheduled.
    pub node: String,
}

/// The pods of a pod list.
pub fn pods_of(list: &Value) -> Vec<Pod> {
    list["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|pod| {
            let label = |key: &str| {
                pod["metadata"]["labels"][key]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            };
            Pod {
                name: pod["metadata"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                component: label("meridian.dev/component"),
                instance: label("meridian.dev/instance"),
                node: pod["spec"]["nodeName"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            }
        })
        .collect()
}

/// Each store's writer, as its Lease names it (`<release>-<component>`):
/// the holder's member, its pod's name; empty where the Lease is held by
/// nobody. A store with no Lease (one member, no standby) is absent.
pub fn lease_holders(list: &Value) -> BTreeMap<String, String> {
    let mut held = BTreeMap::new();
    for lease in list["items"].as_array().into_iter().flatten() {
        let name = lease["metadata"]["name"].as_str().unwrap_or_default();
        // A writer group past the first's Lease (step 3d):
        // `<release>-<store>-group-<g>`, held as `<store>-group-<g>`.
        let (name, group) = match name.rsplit_once("-group-") {
            Some((store, group)) if group.parse::<u32>().is_ok() => (store, Some(group)),
            _ => (name, None),
        };
        let Some((component, _)) = STORES
            .iter()
            .find(|(component, _)| name.ends_with(&format!("-{component}")))
        else {
            continue;
        };
        let holder = lease["spec"]["holderIdentity"].as_str().unwrap_or_default();
        let key = match group {
            Some(group) => format!("{component}-group-{group}"),
            None => component.to_string(),
        };
        held.insert(key, crate::member::holder_member(holder));
    }
    held
}

// ── The configuration ─────────────────────────────────────────────────────────

/// The configuration the profile judges: each count and setting the
/// resilience spec's section 11 makes a chart value. Read from the chart's
/// values as the release states them where the chart hands them to the
/// conductor ([`Values::from_json`]), else from what runs ([`Values::observed`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Values {
    /// Whether these are the chart's values as the release states them,
    /// rather than what was seen running.
    pub stated: bool,
    /// Each store's standbys (`<store>.standby`).
    pub standby: BTreeMap<String, u32>,
    /// Whether the database is the deployment's own, not the chart's.
    pub external_database: bool,
    /// The brought database's standbys (`database.brought.standbys`).
    pub database_standbys: u32,
    /// `broker.replicas`.
    pub broker_replicas: u32,
    /// `zones`.
    pub zones: Vec<String>,
    /// `backup.schedule` set, and `backup.walArchive.enabled`.
    pub backup_schedule: bool,
    pub wal_archive: bool,
    /// The stores' scrub interval in days (`<store>.scrub.intervalDays`),
    /// the least of them; 0 for never.
    pub scrub_days: f64,
    /// `analytics.coordinator.replicaCount`, `analytics.worker.replicaCount`.
    pub analytics_coordinators: u32,
    pub analytics_workers: u32,
    /// The lake's and the book's writer groups (`lake.writerGroups`,
    /// `bor.writerGroups`; step 3d), where above 1; none means one.
    pub writer_groups: BTreeMap<String, u32>,
}

impl Values {
    /// `component`'s writer groups: 1 where the values say none.
    pub fn writer_groups(&self, component: &str) -> u32 {
        self.writer_groups
            .get(component)
            .copied()
            .unwrap_or(1)
            .max(1)
    }

    /// The chart's values as the release states them: a JSON object of the
    /// settings above by their chart paths. Carries no secret and no
    /// credential; a key it does not know is passed over.
    pub fn from_json(text: &str) -> Result<Values, String> {
        let said: Value = serde_json::from_str(text)
            .map_err(|failed| format!("the chart's values did not read: {failed}"))?;
        let count = |value: &Value| value.as_u64().unwrap_or_default() as u32;
        let mut standby = BTreeMap::new();
        for (component, _) in STORES {
            standby.insert(component.to_string(), count(&said[*component]["standby"]));
        }
        let scrub_days = STORES
            .iter()
            .filter_map(|(component, _)| said[*component]["scrub"]["intervalDays"].as_f64())
            .fold(f64::INFINITY, f64::min);
        Ok(Values {
            stated: true,
            standby,
            external_database: !said["database"]["brought"]["enabled"]
                .as_bool()
                .unwrap_or(true),
            database_standbys: count(&said["database"]["brought"]["standbys"]),
            broker_replicas: count(&said["broker"]["replicas"]),
            zones: said["zones"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|zone| zone.as_str().map(str::to_string))
                .collect(),
            backup_schedule: !said["backup"]["schedule"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .is_empty(),
            wal_archive: said["backup"]["walArchive"]["enabled"]
                .as_bool()
                .unwrap_or(false),
            scrub_days: if scrub_days.is_finite() {
                scrub_days
            } else {
                7.0
            },
            analytics_coordinators: count(&said["analytics"]["coordinator"]["replicaCount"]),
            analytics_workers: count(&said["analytics"]["worker"]["replicaCount"]),
            writer_groups: ["lake", "bor"]
                .into_iter()
                .filter_map(|component| {
                    let groups = count(&said[component]["writerGroups"]);
                    (groups > 1).then(|| (component.to_string(), groups))
                })
                .collect(),
        })
    }

    /// What runs, where the chart's values are not read: each store's
    /// standbys as its members report their mode, the database's members and
    /// the broker's servers as their pods, the zones the members report.
    /// What no pod shows -- a backup schedule, the WAL archive -- reads as
    /// not set, and the profile says it was not seen.
    pub fn observed(members: &[ComponentReport], pods: &[Pod], scrub_days: f64) -> Values {
        let mut standby = BTreeMap::new();
        for (component, _) in STORES {
            let n = members
                .iter()
                .filter(|m| m.component == *component && m.mode == "standby")
                .count() as u32;
            standby.insert(component.to_string(), n);
        }
        let of = |component: &'static str| pods.iter().filter(move |p| p.component == component);
        // The brought database's members 0 to 2: a primary and its
        // standbys; from 3, its record-read replicas.
        let database: Vec<&Pod> = of("database").collect();
        let ordinal = |pod: &&Pod| {
            pod.name
                .rsplit_once('-')
                .and_then(|(_, n)| n.parse::<u32>().ok())
                .unwrap_or(0)
        };
        let zones: BTreeSet<String> = members
            .iter()
            .filter(|m| !m.zone.is_empty())
            .map(|m| m.zone.clone())
            .collect();
        Values {
            stated: false,
            standby,
            external_database: database.is_empty(),
            database_standbys: database
                .iter()
                .filter(|p| (1..=2).contains(&ordinal(p)))
                .count() as u32,
            broker_replicas: of("broker").count() as u32,
            zones: zones.into_iter().collect(),
            backup_schedule: false,
            wal_archive: false,
            scrub_days,
            analytics_coordinators: of("analytics-coordinator").count() as u32,
            analytics_workers: of("analytics-worker").count() as u32,
            writer_groups: BTreeMap::new(),
        }
    }
}

/// Where the chart hands the conductor its values as the release states
/// them (approved 2026-11-11): JSON, by chart path, no secret.
pub const VALUES_VAR: &str = "MERIDIAN_RESILIENCE_VALUES";

/// The chart's values from [`VALUES_VAR`], where the chart set it and it
/// reads; none otherwise (said once), and the profile judges what runs.
pub fn values_from_env() -> Option<Values> {
    let said = crate::var(VALUES_VAR)?;
    match Values::from_json(&said) {
        Ok(values) => Some(values),
        Err(failed) => {
            tracing::warn!(%failed, "the chart's values for the resilience profile did not read: it judges what runs");
            None
        }
    }
}

// ── What CI tests ─────────────────────────────────────────────────────────────

/// The configurations CI tests, by failure class and profile: the claims
/// ledger's entries, each written from a green run (366), so a class is
/// tested in a profile only where a green run measured it there.
#[derive(Debug, Clone, Default)]
pub struct Tested(BTreeSet<(String, String)>);

/// The claims ledger this build was made with.
const LEDGER: &str = include_str!("../../../deploy/claims-ledger.json");

impl Tested {
    /// The classes and profiles of a claims ledger.
    pub fn from_ledger(text: &str) -> Tested {
        let ledger: Value = serde_json::from_str(text).unwrap_or_default();
        Tested(
            ledger["entries"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|entry| {
                    Some((
                        entry["class"].as_str()?.to_string(),
                        entry["profile"].as_str()?.to_string(),
                    ))
                })
                .collect(),
        )
    }

    /// The ledger this build carries.
    pub fn compiled() -> Tested {
        Tested::from_ledger(LEDGER)
    }

    /// Whether every one of `classes` is tested in `profile`.
    pub fn all(&self, classes: &[&str], profile: &str) -> bool {
        classes
            .iter()
            .all(|class| self.0.contains(&(class.to_string(), profile.to_string())))
    }
}

// ── The profile ─────────────────────────────────────────────────────────────

/// What the profile reads besides the configuration.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    /// Every member's latest report, the conductor's own among them.
    pub members: Vec<ComponentReport>,
    /// The release's pods.
    pub pods: Vec<Pod>,
    /// The lake's source priorities in force.
    pub priorities: Vec<SourcePriority>,
}

fn change(change: &str, cost: &str) -> ResilienceChange {
    ResilienceChange {
        change: change.to_string(),
        cost: cost.to_string(),
    }
}

fn line(
    subject: &str,
    state: ResilienceState,
    reason: String,
    changes: Vec<ResilienceChange>,
) -> ResilienceLine {
    ResilienceLine {
        subject: subject.to_string(),
        state: state as i32,
        reason,
        changes: if state == ResilienceState::Met {
            Vec::new()
        } else {
            changes
        },
    }
}

/// A list as a person reads it: "a, b and c".
fn listed(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// The judge's reading of the configuration and what runs.
struct Shape<'a> {
    values: &'a Values,
    tested: &'a Tested,
    /// The stores with no standby.
    lacking_standby: Vec<&'static str>,
    /// The nodes the members run on.
    nodes: BTreeSet<String>,
    /// The zones the members report.
    zones: BTreeSet<String>,
    /// The stores whose writer and standby share a node.
    shared_node: Vec<&'static str>,
}

impl Shape<'_> {
    fn database_standby(&self) -> bool {
        self.values.external_database || self.values.database_standbys >= 2
    }

    /// The HA configuration's shape: a standby per store, the database with
    /// its synchronous standbys (or the deployment's own), the broker at
    /// three -- what CI's `ha` installs. Anything less is judged as
    /// `tolerant`, the only other configuration CI tests (1047).
    fn ha(&self) -> bool {
        self.lacking_standby.is_empty()
            && self.database_standby()
            && self.values.broker_replicas >= 3
    }

    fn standby_changes(&self) -> Vec<ResilienceChange> {
        self.lacking_standby
            .iter()
            .map(|component| {
                let cost = match *component {
                    "lake" | "bor" | "instrument" => {
                        format!(
                            "one pod and {}'s hot layer again in memory",
                            store_name(component)
                        )
                    }
                    _ => "one pod".to_string(),
                };
                change(&format!("{component}.standby: 1"), &cost)
            })
            .collect()
    }

    fn database_changes(&self) -> Vec<ResilienceChange> {
        if self.database_standby() {
            Vec::new()
        } else {
            vec![change(
                "database.brought.standbys: 2",
                "two database pods and their volumes",
            )]
        }
    }

    fn broker_changes(&self) -> Vec<ResilienceChange> {
        if self.values.broker_replicas >= 3 {
            Vec::new()
        } else {
            vec![change("broker.replicas: 3", "two broker pods")]
        }
    }

    fn ha_changes(&self) -> Vec<ResilienceChange> {
        let mut changes = self.standby_changes();
        changes.extend(self.database_changes());
        changes.extend(self.broker_changes());
        changes
    }

    fn node_changes(&self) -> Vec<ResilienceChange> {
        if self.nodes.len() >= 2 && self.shared_node.is_empty() {
            Vec::new()
        } else {
            vec![change("add a node", "one node or more")]
        }
    }

    fn zone_changes(&self) -> Vec<ResilienceChange> {
        if self.zones.len() >= 3 && self.values.zones.len() >= 3 {
            Vec::new()
        } else {
            vec![change(
                "add nodes in two more zones and set zones",
                "two nodes or more",
            )]
        }
    }

    /// Why a store or the database would wait for a restart.
    fn standby_reason(&self) -> String {
        let named: Vec<String> = self
            .lacking_standby
            .iter()
            .map(|component| store_name(component).to_string())
            .collect();
        match named.as_slice() {
            [one] if one == "the book" => "one book writer, no standby".to_string(),
            _ => format!("no standby for {}", listed(&named)),
        }
    }

    fn node_reason(&self) -> String {
        if self.nodes.len() <= 1 {
            "this deployment runs on one node".to_string()
        } else {
            let named: Vec<String> = self
                .shared_node
                .iter()
                .map(|component| store_name(component).to_string())
                .collect();
            format!("{}'s writer and standby on one node", listed(&named))
        }
    }

    fn zone_reason(&self) -> String {
        match self.zones.len() {
            0 | 1 => "every node in one zone".to_string(),
            n => format!("the members run in {n} zones; a zone's loss needs three"),
        }
    }
}

/// How the tolerant default answers a class (section 10's tolerant column).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tolerant {
    /// Healed by Kubernetes' restart within the tolerant bound.
    Restart,
    /// Healed without a restart: refused and recovered, or waited out.
    Healed,
    /// Not healed: an admin's restore, or stopped until it is back.
    No,
}

/// One failure class of section 10.
struct Class {
    subject: &'static str,
    /// CI's classes that inject it (section 15).
    ci: &'static [&'static str],
    tolerant: Tolerant,
    /// What the tolerant default does, in plain words.
    tolerant_said: &'static str,
    /// What the HA configuration does, in plain words.
    ha_said: &'static str,
}

const CLASSES: &[Class] = &[
    Class {
        subject: "a pod",
        ci: &["lake-writer", "bor-writer", "order-plugin"],
        tolerant: Tolerant::Restart,
        tolerant_said: "Kubernetes restarts it, answering again within 2 minutes",
        ha_said: "a standby takes over: within 10 s for the book, street and order path, 15 s for the lake's writes",
    },
    Class {
        subject: "memory exhausted, or one plugin flooding",
        ci: &["memory-pressure", "noisy-plugin"],
        tolerant: Tolerant::Healed,
        tolerant_said: "refused with a retry-after and recovered as load falls, never restarted",
        ha_said: "refused with a retry-after and recovered as load falls, never restarted",
    },
    Class {
        subject: "a node or hypervisor lost",
        ci: &["node"],
        tolerant: Tolerant::No,
        tolerant_said: "the members wait for their node",
        ha_said: "standbys on other nodes take over",
    },
    Class {
        subject: "a node frozen",
        ci: &["node-freeze"],
        tolerant: Tolerant::No,
        tolerant_said: "the members wait for their node",
        ha_said: "the old writer's commits are refused and it follows as a standby",
    },
    Class {
        subject: "a network partition",
        ci: &["split-brain", "broker"],
        tolerant: Tolerant::No,
        tolerant_said: "members cut off go unready and wait",
        ha_said: "one writer per partition by the epoch; the broker's majority serves",
    },
    Class {
        subject: "the Kubernetes API server unreachable",
        ci: &["apiserver"],
        tolerant: Tolerant::Healed,
        tolerant_said: "writers keep writing under their epoch; nothing is taken over",
        ha_said: "writers keep writing under their epoch; nothing is taken over",
    },
    Class {
        subject: "a database failover or restore that lost a committed tail",
        ci: &["db-primary"],
        tolerant: Tolerant::Restart,
        tolerant_said: "the database restarts on its volume; the epoch is raised and followers read again",
        ha_said: "a standby is promoted; the epoch is raised and followers read again",
    },
    Class {
        subject: "a firewall or NetworkPolicy change",
        ci: &["network-policy"],
        tolerant: Tolerant::Healed,
        tolerant_said: "blocked members wait unready and are healed when it is removed, no restart",
        ha_said: "members whose paths work keep serving",
    },
    Class {
        subject: "silent bit rot",
        ci: &["bit-rot"],
        tolerant: Tolerant::No,
        tolerant_said: "the read fails loudly and the scrub records it; a restore is the admin's",
        ha_said: "a verifying member serves and the bad one is re-cloned",
    },
    Class {
        subject: "a torn write",
        ci: &["torn-write"],
        tolerant: Tolerant::Restart,
        tolerant_said: "crash recovery as the database restarts, within 2 minutes",
        ha_said: "crash recovery on the promoted or restarted member",
    },
    Class {
        subject: "a volume lost",
        ci: &["volume-lost"],
        tolerant: Tolerant::No,
        tolerant_said: "a restore from the last backup, the admin's act",
        ha_said: "a standby is re-cloned; a primary's loss fails over first",
    },
    Class {
        subject: "a zone's node pool",
        ci: &["zone"],
        tolerant: Tolerant::No,
        tolerant_said: "stopped until the zone is back or a restore",
        ha_said: "spread members take over from another zone",
    },
];

/// One class judged.
fn judge(class: &Class, shape: &Shape) -> ResilienceLine {
    let tested = |profile| shape.tested.all(class.ci, profile);
    // What the HA configuration needs beyond its shape, for this class.
    let (needs_more, more_reason, more_changes) = match class.subject {
        "a node or hypervisor lost" | "a node frozen" | "a network partition" => (
            !shape.node_changes().is_empty(),
            shape.node_reason(),
            shape.node_changes(),
        ),
        "a zone's node pool" => (
            !shape.zone_changes().is_empty(),
            shape.zone_reason(),
            shape.zone_changes(),
        ),
        _ => (false, String::new(), Vec::new()),
    };
    if shape.ha() && tested("ha") && !needs_more {
        return line(
            class.subject,
            ResilienceState::Met,
            format!("{}, as CI tests it in ha", class.ha_said),
            Vec::new(),
        );
    }
    // Toward the HA configuration: its shape, then what this class needs.
    let mut changes = shape.ha_changes();
    changes.extend(more_changes);
    let why_not = if shape.ha() {
        if needs_more {
            more_reason
        } else {
            format!(
                "not tested by CI in this configuration ({})",
                class.ci.join(", ")
            )
        }
    } else if !shape.lacking_standby.is_empty() {
        shape.standby_reason()
    } else if !shape.database_standby() {
        "the database has no synchronous standby".to_string()
    } else {
        "the broker runs alone".to_string()
    };
    match class.tolerant {
        Tolerant::Restart if tested("tolerant") => line(
            class.subject,
            ResilienceState::MetByRestart,
            format!("{why_not}: {}", class.tolerant_said),
            changes,
        ),
        Tolerant::Healed if tested("tolerant") => line(
            class.subject,
            ResilienceState::Met,
            format!("{}, as CI tests it in tolerant", class.tolerant_said),
            Vec::new(),
        ),
        _ => {
            let reason = if class.subject == "a zone's node pool" {
                shape.zone_reason()
            } else if matches!(class.subject, "a node or hypervisor lost" | "a node frozen")
                && shape.nodes.len() <= 1
            {
                shape.node_reason()
            } else {
                format!("{why_not}: {}", class.tolerant_said)
            };
            line(class.subject, ResilienceState::NotMet, reason, changes)
        }
    }
}

/// The tiers of 1048's table, each judged for the HA configuration.
fn tiers(shape: &Shape) -> Vec<ResilienceLine> {
    let columns: [(&str, &[&str], &[&str]); 3] = [
        (
            "the book, street and order path",
            &["bor", "street"],
            &["bor-writer", "street-writer", "order-plugin"],
        ),
        ("the lake", &["lake"], &["lake-writer"]),
        ("analytics", &[], &["standing-coordinator"]),
    ];
    let mut lines = Vec::new();
    for (column, stores, ci) in columns {
        // A component or a node.
        let subject = format!("a component or a node: {column}");
        let lacking: Vec<&str> = shape
            .lacking_standby
            .iter()
            .copied()
            .filter(|c| stores.contains(c))
            .collect();
        let analytics_short = column == "analytics"
            && (shape.values.analytics_coordinators < 2 || shape.values.analytics_workers < 2);
        let mut classes = ci.to_vec();
        classes.push("node");
        if !lacking.is_empty() {
            let named: Vec<String> = lacking.iter().map(|c| store_name(c).to_string()).collect();
            let changes = shape
                .standby_changes()
                .into_iter()
                .filter(|c| {
                    lacking
                        .iter()
                        .any(|l| c.change.starts_with(&format!("{l}.")))
                })
                .collect();
            lines.push(line(
                &subject,
                ResilienceState::NotMet,
                format!("no standby for {}", listed(&named)),
                changes,
            ));
        } else if analytics_short {
            lines.push(line(
                &subject,
                ResilienceState::NotMet,
                "one analytics coordinator or worker".to_string(),
                vec![
                    change(
                        "analytics.coordinator.replicaCount: 2",
                        "one more coordinator pod",
                    ),
                    change("analytics.worker.replicaCount: 2", "one more worker pod"),
                ],
            ));
        } else if !shape.node_changes().is_empty() {
            lines.push(line(
                &subject,
                ResilienceState::NotMet,
                shape.node_reason(),
                shape.node_changes(),
            ));
        } else if !shape.ha() || !shape.tested.all(&classes, "ha") {
            lines.push(line(
                &subject,
                ResilienceState::NotMet,
                format!(
                    "not tested by CI in this configuration ({})",
                    classes.join(", ")
                ),
                shape.ha_changes(),
            ));
        } else {
            lines.push(line(
                &subject,
                ResilienceState::Met,
                "a standby on another node takes over, as CI tests it in ha".to_string(),
                Vec::new(),
            ));
        }
    }
    for (column, _, _) in columns {
        let subject = format!("the database primary: {column}");
        if shape.values.external_database {
            lines.push(line(
                &subject,
                ResilienceState::NotMet,
                "the database is the deployment's own: its failover is its provider's, which CI does not measure".to_string(),
                vec![change("read the database provider's stated failover", "none")],
            ));
        } else if !shape.database_standby() {
            lines.push(line(
                &subject,
                ResilienceState::NotMet,
                "the database has no synchronous standby".to_string(),
                shape.database_changes(),
            ));
        } else if !shape.ha() || !shape.tested.all(&["db-primary"], "ha") {
            lines.push(line(
                &subject,
                ResilienceState::NotMet,
                "not tested by CI in this configuration (db-primary)".to_string(),
                shape.ha_changes(),
            ));
        } else {
            lines.push(line(
                &subject,
                ResilienceState::Met,
                "a synchronous standby is promoted, as CI tests it in ha".to_string(),
                Vec::new(),
            ));
        }
    }
    for (column, _, _) in columns {
        let subject = format!("a zone: {column}");
        if !shape.zone_changes().is_empty() {
            let mut changes = shape.ha_changes();
            changes.extend(shape.zone_changes());
            lines.push(line(
                &subject,
                ResilienceState::NotMet,
                shape.zone_reason(),
                changes,
            ));
        } else if !shape.ha() || !shape.tested.all(&["zone"], "ha") {
            lines.push(line(
                &subject,
                ResilienceState::NotMet,
                "not tested by CI in this configuration (zone)".to_string(),
                shape.ha_changes(),
            ));
        } else {
            lines.push(line(
                &subject,
                ResilienceState::Met,
                "members in another zone take over, as CI tests it in ha".to_string(),
                Vec::new(),
            ));
        }
    }
    let backups = shape.values.backup_schedule && shape.values.wal_archive;
    lines.push(if backups && shape.tested.all(&["restore-to-time"], "tolerant") {
        line(
            "a region (recovery)",
            ResilienceState::Met,
            "restored into a new cluster from the off-cluster archive, to a point in time (`meridian restore --to-time`), as CI tests it".to_string(),
            Vec::new(),
        )
    } else {
        line(
            "a region (recovery)",
            ResilienceState::NotMet,
            backup_reason(shape.values),
            vec![backup_change()],
        )
    });
    lines
}

fn backup_change() -> ResilienceChange {
    change(
        "backup.walArchive: on, with a backup schedule",
        "a backup volume and its storage",
    )
}

fn backup_reason(values: &Values) -> String {
    if values.stated {
        "backups not scheduled".to_string()
    } else {
        "no backup schedule or WAL archive seen".to_string()
    }
}

/// Never blind's properties per live data type in the priorities (section
/// 8): a second source in its priority, standby pods for its sources, and
/// each value's age and source on its pages (always, in either
/// configuration: the kit's live cells).
fn never_blind(seen: &Seen) -> Vec<NeverBlindLine> {
    let mut by_type: BTreeMap<String, (bool, bool, bool)> = BTreeMap::new();
    for priority in &seen.priorities {
        if priority.datasets.is_empty() {
            continue;
        }
        let standby = priority.datasets.iter().all(|dataset| {
            let instance = dataset.split_once(':').map_or(dataset.as_str(), |(i, _)| i);
            seen.pods
                .iter()
                .filter(|p| p.component == "sidecar" && p.instance == instance)
                .count()
                >= 2
        });
        let held = by_type
            .entry(priority.data_type.clone())
            .or_insert((false, true, true));
        held.0 |= priority.datasets.len() >= 2;
        held.1 &= standby;
    }
    by_type
        .into_iter()
        .map(
            |(data_type, (second_source, standby_pods, age_and_source))| NeverBlindLine {
                data_type,
                second_source,
                standby_pods,
                age_and_source,
            },
        )
        .collect()
}

/// The profile: what the configuration meets and what would meet more.
pub fn profile(
    values: &Values,
    seen: &Seen,
    tested: &Tested,
    now_ns: i64,
) -> ReadResilienceProfileReply {
    let lacking_standby: Vec<&'static str> = STORES
        .iter()
        .map(|(component, _)| *component)
        .filter(|component| values.standby.get(*component).copied().unwrap_or(0) == 0)
        .collect();
    let node_of: BTreeMap<&str, &str> = seen
        .pods
        .iter()
        .map(|p| (p.name.as_str(), p.node.as_str()))
        .collect();
    let nodes: BTreeSet<String> = seen
        .members
        .iter()
        .filter_map(|m| node_of.get(m.member.as_str()))
        .filter(|node| !node.is_empty())
        .map(|node| node.to_string())
        .collect();
    let zones: BTreeSet<String> = seen
        .members
        .iter()
        .filter(|m| !m.zone.is_empty())
        .map(|m| m.zone.clone())
        .collect();
    let shared_node = STORES
        .iter()
        .map(|(component, _)| *component)
        .filter(|component| {
            let on: Vec<&str> = seen
                .members
                .iter()
                .filter(|m| {
                    m.component == *component && (m.mode == "writer" || m.mode == "standby")
                })
                .filter_map(|m| node_of.get(m.member.as_str()).copied())
                .collect();
            on.len() >= 2 && on.iter().all(|node| *node == on[0])
        })
        .collect();
    let shape = Shape {
        values,
        tested,
        lacking_standby,
        nodes,
        zones,
        shared_node,
    };
    let failures: Vec<ResilienceLine> = CLASSES.iter().map(|class| judge(class, &shape)).collect();
    let tiers = tiers(&shape);
    let met = |subject: &str| {
        failures
            .iter()
            .find(|l| l.subject == subject)
            .is_some_and(|l| l.state == ResilienceState::Met as i32)
    };
    let held = |subject: &str| {
        failures
            .iter()
            .find(|l| l.subject == subject)
            .is_some_and(|l| l.state != ResilienceState::NotMet as i32)
    };
    let every_class_but_zone = CLASSES
        .iter()
        .filter(|c| c.subject != "a zone's node pool")
        .all(|c| met(c.subject));
    let single_node = held("a pod")
        && held("a torn write")
        && values.backup_schedule
        && values.wal_archive
        && values.scrub_days > 0.0;
    let level = if every_class_but_zone && met("a zone's node pool") {
        ServiceLevel::MultiZone
    } else if every_class_but_zone {
        ServiceLevel::MultiNode
    } else if single_node {
        ServiceLevel::SingleNode
    } else {
        ServiceLevel::Desktop
    };
    let mut next_level_changes: Vec<ResilienceChange> = Vec::new();
    let mut add = |changes: &[ResilienceChange]| {
        for c in changes {
            if !next_level_changes
                .iter()
                .any(|held| held.change == c.change)
            {
                next_level_changes.push(c.clone());
            }
        }
    };
    match level {
        ServiceLevel::Desktop => {
            if !(values.backup_schedule && values.wal_archive) {
                add(&[backup_change()]);
            }
            if values.scrub_days <= 0.0 {
                add(&[change(
                    "<store>.scrub.intervalDays: 7",
                    "a weekly re-read of each store, at low priority",
                )]);
            }
            if !held("a pod") || !held("a torn write") {
                for l in &failures {
                    if l.subject == "a pod" || l.subject == "a torn write" {
                        add(&l.changes);
                    }
                }
            }
        }
        ServiceLevel::SingleNode => {
            for l in failures
                .iter()
                .filter(|l| l.subject != "a zone's node pool")
            {
                add(&l.changes);
            }
        }
        ServiceLevel::MultiNode => {
            for l in failures
                .iter()
                .filter(|l| l.subject == "a zone's node pool")
            {
                add(&l.changes);
            }
        }
        _ => {}
    }
    ReadResilienceProfileReply {
        level: level as i32,
        next_level_changes,
        failures,
        tiers,
        never_blind: never_blind(seen),
        computed_at_ns: now_ns,
    }
}

// ── The history ─────────────────────────────────────────────────────────────

/// A store whose `control` partition the history reads: its change log's
/// domain and the component its records name.
#[derive(Debug, Clone, Copy)]
pub struct Source {
    pub domain: &'static str,
    pub component: &'static str,
}

/// The stores whose control partitions carry their takeovers and findings
/// (contract v30, stage D): the lake (and its source switches), the
/// instrument store, the street store and the configuration store. The
/// book's joins when its journal carries them, and its account moves.
pub const SOURCES: &[Source] = &[
    Source {
        domain: "lake",
        component: "lake",
    },
    Source {
        domain: "reference",
        component: "instrument",
    },
    Source {
        domain: "street",
        component: "street",
    },
    Source {
        domain: "config",
        component: "conductor",
    },
    // The book's control journal (step 3d; section 6.3, option (c)): its
    // takeovers and findings as `~control` entries.
    Source {
        domain: "book",
        component: "bor",
    },
];

/// The history the conductor holds: the newest items of every source, each
/// source read from where it was read to.
#[derive(Debug, Default)]
pub struct History {
    held: Mutex<Held>,
    /// One catch-up at a time.
    catching_up: tokio::sync::Mutex<()>,
}

#[derive(Debug, Default)]
struct Held {
    /// Each source's control partition, read to this sequence.
    read_to: BTreeMap<&'static str, u64>,
    /// The items, oldest first, by when they happened.
    items: Vec<(i64, ResilienceHistoryItem)>,
    /// The lake's source priorities in force, by data type and kind.
    priorities: BTreeMap<(String, i32), SourcePriority>,
}

fn at_of(item: &ResilienceHistoryItem) -> i64 {
    match &item.item {
        Some(Item::WriterTookOver(r)) => r.took_over_at_ns,
        Some(Item::PartitionMoved(r)) => r.moved_at_ns,
        Some(Item::SourceSwitched(r)) => r.switched_at_ns,
        Some(Item::AccountMoved(r)) => r.moved_at_ns,
        Some(Item::IntegrityFinding(r)) => r.found_at_ns,
        // Contract v30's 3d records (approved 2026-11-12): the split from
        // step 3d's stage H; the move that did not switch from stage M.
        Some(Item::PartitionSplit(r)) => r.split_at_ns,
        Some(Item::PartitionMoveAborted(r)) => r.aborted_at_ns,
        None => 0,
    }
}

impl History {
    /// What one control change adds: its records as items, and a lake
    /// source priority in force.
    fn take(held: &mut Held, change: &meridian_changes::Change) {
        // A book change is a `~control` entry: the record it carries, as a
        // control change of every other store carries one.
        let carried;
        let change = if change.partition == meridian_changes::CONTROL
            && change.kind.starts_with("meridian.v1.")
            && change.body.first() == Some(&b'{')
        {
            match meridian_bor::control::record_of_change(change) {
                Some((kind, body)) => {
                    carried = meridian_changes::Change {
                        kind,
                        body,
                        ..change.clone()
                    };
                    &carried
                }
                None => change,
            }
        } else {
            change
        };
        let mut items: Vec<ResilienceHistoryItem> = records::of_change(change)
            .into_iter()
            .map(|record| ResilienceHistoryItem {
                item: Some(match record {
                    Record::TookOver(r) => Item::WriterTookOver(r),
                    Record::Finding(r) => Item::IntegrityFinding(r),
                }),
            })
            .collect();
        match change.kind.as_str() {
            "meridian.v1.SourceSwitched" => {
                if let Ok(switch) = SourceSwitched::decode(change.body.as_slice()) {
                    items.push(ResilienceHistoryItem {
                        item: Some(Item::SourceSwitched(switch)),
                    });
                }
            }
            // A lake range's split (step 3d; section 6.1).
            "meridian.v1.PartitionSplit" => {
                if let Ok(split) =
                    meridian_domain::v1::PartitionSplit::decode(change.body.as_slice())
                {
                    items.push(ResilienceHistoryItem {
                        item: Some(Item::PartitionSplit(split)),
                    });
                }
            }
            "meridian.v1.SourcePriority" => {
                if let Ok(priority) = SourcePriority::decode(change.body.as_slice()) {
                    held.priorities
                        .insert((priority.data_type.clone(), priority.kind), priority);
                }
            }
            _ => {}
        }
        for item in items {
            held.items.push((at_of(&item), item));
        }
    }

    /// Read each source's control partition on from where it was read to:
    /// a page at a time, its interest in each log held only while it reads.
    /// A source that does not answer is read again at the next ask.
    pub async fn catch_up(&self, bus: &Bus) {
        let _alone = self.catching_up.lock().await;
        for source in SOURCES {
            let mut after = self
                .held
                .lock()
                .expect("history poisoned")
                .read_to
                .get(source.domain)
                .copied()
                .unwrap_or(0);
            loop {
                let asked = meridian_changes::ReadChangesRequest {
                    partition: meridian_changes::CONTROL.to_string(),
                    after_sequence: after,
                    page_size: 0,
                };
                let page = match bus
                    .call(
                        &meridian_changes::read_changes_topic(source.domain),
                        "meridian.v1.ReadChangesRequest",
                        asked.encode_to_vec(),
                        None,
                        Some(ASK),
                    )
                    .await
                    .map_err(|failed| failed.to_string())
                    .and_then(|(_, body)| {
                        meridian_changes::ReadChangesReply::decode(body.as_slice())
                            .map_err(|failed| failed.to_string())
                    }) {
                    Ok(page) => page,
                    Err(failed) => {
                        // Behind its lowest kept: read on from there.
                        if meridian_changes::is_behind(&failed) && after == 0 {
                            tracing::debug!(domain = source.domain, %failed, "a control log read from its lowest kept");
                        } else {
                            tracing::debug!(domain = source.domain, %failed, "a control log was not read for the history");
                        }
                        break;
                    }
                };
                let mut held = self.held.lock().expect("history poisoned");
                for change in &page.changes {
                    Self::take(&mut held, change);
                    after = change.sequence;
                }
                held.read_to.insert(source.domain, after);
                held.items.sort_by_key(|(at, _)| *at);
                let excess = held.items.len().saturating_sub(HISTORY_KEPT);
                held.items.drain(..excess);
                if page.changes.is_empty() || after >= page.head {
                    break;
                }
            }
        }
    }

    /// A page of the history, newest first, from `cursor` (an offset this
    /// read gave).
    pub fn page(
        &self,
        page_size: u32,
        cursor: &str,
    ) -> Result<(Vec<ResilienceHistoryItem>, String), String> {
        let page = match page_size {
            0 => HISTORY_PAGE,
            asked => (asked as usize).min(HISTORY_PAGE_MOST),
        };
        let skip: usize = if cursor.is_empty() {
            0
        } else {
            cursor
                .parse()
                .map_err(|_| format!("cursor {cursor:?} is no cursor this read gave"))?
        };
        let held = self.held.lock().expect("history poisoned");
        let items: Vec<ResilienceHistoryItem> = held
            .items
            .iter()
            .rev()
            .skip(skip)
            .take(page)
            .map(|(_, item)| item.clone())
            .collect();
        let next = skip + page;
        Ok((
            items,
            if next < held.items.len() {
                next.to_string()
            } else {
                String::new()
            },
        ))
    }

    /// The lake's source priorities in force.
    pub fn priorities(&self) -> Vec<SourcePriority> {
        self.held
            .lock()
            .expect("history poisoned")
            .priorities
            .values()
            .cloned()
            .collect()
    }
}

// ── Answering ─────────────────────────────────────────────────────────────────

/// What the two reads are answered from.
pub struct Answering {
    pub heard: crate::MembersHeard,
    pub restarts: crate::PodRestarts,
    pub own: Arc<dyn Fn() -> ComponentReport + Send + Sync>,
    pub cluster: Option<Arc<meridian_first_run::cluster::ApiServer>>,
    /// The chart's values as the release states them, where the chart hands
    /// them to the conductor; what runs otherwise.
    pub values: Option<Values>,
    pub history: Arc<History>,
}

impl Answering {
    fn members(&self) -> Result<Vec<ComponentReport>, String> {
        let mut members = vec![(self.own)()];
        members.extend(
            self.heard
                .lock()
                .map_err(|_| "the member reports' lock".to_string())?
                .values()
                .map(|(report, _)| report.clone()),
        );
        let restarts = self
            .restarts
            .lock()
            .map_err(|_| "the pods' restarts' lock".to_string())?
            .clone();
        Ok(members
            .into_iter()
            .map(|report| crate::with_restarts(report, &restarts))
            .collect())
    }

    async fn pods(&self) -> Vec<Pod> {
        match &self.cluster {
            Some(api) => match api.pods().await {
                Ok(list) => pods_of(&list),
                Err(failed) => {
                    tracing::debug!(%failed, "the pods were not read for the resilience reads");
                    Vec::new()
                }
            },
            None => Vec::new(),
        }
    }

    async fn holders(&self) -> BTreeMap<String, String> {
        match &self.cluster {
            Some(api) => match api.leases().await {
                Ok(list) => lease_holders(&list),
                Err(failed) => {
                    tracing::debug!(%failed, "the Leases were not read for the resilience reads");
                    BTreeMap::new()
                }
            },
            None => BTreeMap::new(),
        }
    }

    /// Each partition of the lake and the book, its writer and its epoch:
    /// the partitions its writer reports, each epoch as its change log says
    /// it now, the writer as its Lease names it (or its one member's, with
    /// no Lease).
    async fn partitions(&self, bus: &Bus, members: &[ComponentReport]) -> Vec<PartitionOwner> {
        let holders = self.holders().await;
        let mut owners = Vec::new();
        for (component, domain) in [("lake", "lake"), ("bor", "book")] {
            let writer = members
                .iter()
                .find(|m| m.component == component && m.mode == "writer");
            let mut partitions: BTreeSet<String> = writer
                .map(|w| w.partitions.iter().cloned().collect())
                .unwrap_or_default();
            if writer.is_none() && members.iter().all(|m| m.component != component) {
                continue;
            }
            partitions.insert(meridian_changes::CONTROL.to_string());
            let named = match holders.get(component) {
                Some(holder) => holder.clone(),
                None => writer.map(|w| w.member.clone()).unwrap_or_default(),
            };
            // Each partition's writer group (step 3d): by the store's count
            // of groups, its writer the holder of that group's Lease.
            let groups = self
                .values
                .as_ref()
                .map_or(1, |values| values.writer_groups(component));
            for partition in partitions {
                // A lake range is its dataset's group's (step 3d).
                let owned_by = if component == "lake" {
                    meridian_lake::range::dataset_of(&partition)
                } else {
                    partition.as_str()
                };
                let group = meridian_changes::groups::owner(owned_by, groups);
                let named = if group == 0 {
                    named.clone()
                } else {
                    holders
                        .get(&format!("{component}-group-{group}"))
                        .cloned()
                        .unwrap_or_default()
                };
                let asked = meridian_changes::ReadChangesRequest {
                    partition: partition.clone(),
                    after_sequence: i64::MAX as u64,
                    page_size: 1,
                };
                let epoch = bus
                    .call(
                        &meridian_changes::read_changes_topic(domain),
                        "meridian.v1.ReadChangesRequest",
                        asked.encode_to_vec(),
                        None,
                        Some(ASK),
                    )
                    .await
                    .ok()
                    .and_then(|(_, body)| {
                        meridian_changes::ReadChangesReply::decode(body.as_slice()).ok()
                    })
                    .map_or(0, |reply| reply.epoch);
                if epoch == 0 {
                    // Not answered: no partition is shown with an epoch it
                    // does not have.
                    continue;
                }
                owners.push(PartitionOwner {
                    component: component.to_string(),
                    partition,
                    group,
                    writer: named,
                    epoch,
                });
            }
        }
        owners
    }
}

/// Answer `ReadResilience` and `ReadResilienceProfile` (W6.39).
pub fn serve(bus: &Arc<Bus>, answering: Answering) {
    let answering = Arc::new(answering);
    let tested = Arc::new(Tested::compiled());
    {
        let answering = Arc::clone(&answering);
        let asking = Arc::clone(bus);
        bus.serve(READ_RESILIENCE_TOPIC, move |envelope| {
            let request = ReadResilienceRequest::decode(envelope.payload.as_slice())
                .map_err(|failed| format!("a ReadResilienceRequest did not read: {failed}"))?;
            let handle = tokio::runtime::Handle::try_current()
                .map_err(|_| "the conductor has no runtime to ask on".to_string())?;
            let members = answering.members()?;
            let partitions = handle.block_on(async {
                answering.history.catch_up(&asking).await;
                answering.partitions(&asking, &members).await
            });
            let (history, next_cursor) =
                answering.history.page(request.page_size, &request.cursor)?;
            Ok((
                "meridian.v1.ReadResilienceReply".to_string(),
                ReadResilienceReply {
                    members,
                    partitions,
                    history,
                    next_cursor,
                }
                .encode_to_vec(),
            ))
        });
    }
    let asking = Arc::clone(bus);
    let clock = bus.clock();
    bus.serve(READ_RESILIENCE_PROFILE_TOPIC, move |_| {
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| "the conductor has no runtime to ask on".to_string())?;
        let members = answering.members()?;
        let pods = handle.block_on(async {
            answering.history.catch_up(&asking).await;
            answering.pods().await
        });
        let seen = Seen {
            members,
            pods,
            priorities: answering.history.priorities(),
        };
        let values = answering.values.clone().unwrap_or_else(|| {
            Values::observed(
                &seen.members,
                &seen.pods,
                meridian_changes::integrity::scrub_every()
                    .map_or(0.0, |every| every.as_secs_f64() / 86_400.0),
            )
        });
        Ok((
            "meridian.v1.ReadResilienceProfileReply".to_string(),
            profile(&values, &seen, &tested, clock.now_ns()).encode_to_vec(),
        ))
    });
}

#[cfg(test)]
mod tests;
