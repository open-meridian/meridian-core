//! The weekly plugin usage report (W5.19, contract v25; the spec's
//! requirements 46 to 48; item 78): for each installed community or
//! verified version, its launched instances, the people holding `read`,
//! `write` and `admin` on it, and seven daily counts of the operations its
//! sidecars carried and of page and tool requests, summed from their
//! reports (amendment 4). It names no person, account, instrument or local
//! plugin; a version whose sidecars have counted nothing yet is not
//! reported. Sent over HTTPS on the deployment's assertion unless the
//! deployment admin turned it off; each report sent, received or not, kept
//! as its own record; one not received is not sent again, the next week's
//! replacing it on the platform.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meridian_bus::Bus;
use meridian_domain::date::Date;
use meridian_domain::v1::{
    AccessRecords, PluginLaunchState, PluginReport, PluginUsageCount, PluginUsageReport,
    SentPluginUsageReport,
};
use prost::Message;

use super::Hold;
use crate::service::PLUGIN_REPORT;

/// How often the sender looks whether a week's report is due.
pub const LOOK_EVERY: Duration = Duration::from_secs(3_600);

const DAY_NS: i64 = 86_400_000_000_000;

/// The week a report covers: the last whole one, Monday to Sunday, before
/// the week `today` is in.
pub fn last_week(today: Date) -> (Date, Date) {
    let days = today.days_since_epoch();
    let weekday = (days + 3).rem_euclid(7);
    let this_monday = days - weekday;
    let start = Date::from_days(this_monday - 7).unwrap_or(today);
    let end = Date::from_days(this_monday - 1).unwrap_or(today);
    (start, end)
}

/// When an instance reported, and its daily operations and requests.
type Reported = (i64, Vec<u64>, Vec<u64>);

/// A week's counts per day: the operations, then the requests.
type Week = ([u64; 7], [u64; 7]);

/// Each instance's latest daily counts, as its sidecar reported them.
#[derive(Default)]
pub struct Counted {
    latest: Mutex<BTreeMap<String, Reported>>,
}

impl Counted {
    pub fn hear(&self, report: &PluginReport) {
        if report.daily_operations.is_empty() && report.daily_requests.is_empty() {
            return;
        }
        let mut latest = self.latest.lock().expect("counts lock poisoned");
        let newer = latest
            .get(&report.plugin_instance_id)
            .is_none_or(|(at, _, _)| *at <= report.reported_at_ns);
        if newer {
            latest.insert(
                report.plugin_instance_id.clone(),
                (
                    report.reported_at_ns,
                    report.daily_operations.clone(),
                    report.daily_requests.clone(),
                ),
            );
        }
    }

    /// An instance's counts on each day of the week `start`..=`end`, or
    /// none where it reported none.
    fn week(&self, instance: &str, start: Date) -> Option<([u64; 7], [u64; 7])> {
        let latest = self.latest.lock().expect("counts lock poisoned");
        let (at, operations, requests) = latest.get(instance)?;
        let last_day = at.div_euclid(DAY_NS);
        let mut week = ([0u64; 7], [0u64; 7]);
        for (series, out) in [(operations, &mut week.0), (requests, &mut week.1)] {
            let first_day = last_day - series.len() as i64 + 1;
            for (n, count) in series.iter().enumerate() {
                let offset = first_day + n as i64 - start.days_since_epoch();
                if (0..7).contains(&offset) {
                    out[offset as usize] += count;
                }
            }
        }
        Some(week)
    }
}

/// The people holding each level on a version's instances, by login: each
/// counted once, at the highest level they hold on any of them. A person
/// known only through a directory group is not countable from here.
fn people(records: &AccessRecords, instances: &[String]) -> (u32, u32, u32) {
    let mut held: BTreeMap<&str, i32> = BTreeMap::new();
    for permission in &records.permissions {
        let Some(group) = records
            .access_groups
            .iter()
            .find(|group| group.access_group_id == permission.access_group_id)
        else {
            continue;
        };
        let level = group
            .entries
            .iter()
            .filter(|entry| instances.contains(&entry.plugin_instance_id))
            .map(|entry| entry.level)
            .max();
        let Some(level) = level else { continue };
        let logins = records
            .user_groups
            .iter()
            .filter(|users| users.user_group_id == permission.user_group_id)
            .flat_map(|users| users.logins.iter());
        for login in logins {
            let slot = held.entry(login).or_default();
            *slot = (*slot).max(level);
        }
    }
    let count = |level: meridian_access::AccessLevel| {
        held.values().filter(|held| **held == level as i32).count() as u32
    };
    (
        count(meridian_access::AccessLevel::Read),
        count(meridian_access::AccessLevel::Write),
        count(meridian_access::AccessLevel::Admin),
    )
}

/// The report for a week, from the catalogue's launches, the access
/// records and the counts heard.
pub fn report_for(
    snapshot: &crate::store::Snapshot,
    counted: &Counted,
    start: Date,
    end: Date,
) -> PluginUsageReport {
    let mut versions: BTreeMap<(String, String, String), Vec<String>> = BTreeMap::new();
    for launch in snapshot
        .catalogue
        .launches
        .iter()
        .filter(|launch| launch.state == PluginLaunchState::Launched as i32)
    {
        let Some(version) = snapshot.catalogue.versions.iter().find(|held| {
            held.metadata
                .as_ref()
                .is_some_and(|m| m.name == launch.name && m.version == launch.version)
        }) else {
            continue;
        };
        if version.listing.is_empty() || version.source_registry == "local" {
            continue;
        }
        versions
            .entry((
                version.listing.clone(),
                launch.version.clone(),
                version.source_registry.clone(),
            ))
            .or_default()
            .push(launch.instance_id.clone());
    }
    let mut counts = Vec::new();
    for ((listing, version, registry), instances) in versions {
        let weeks: Vec<_> = instances
            .iter()
            .filter_map(|instance| counted.week(instance, start))
            .collect();
        if weeks.is_empty() {
            continue;
        }
        let sum = |pick: fn(&Week) -> &[u64; 7]| -> Vec<u64> {
            (0..7)
                .map(|day| weeks.iter().map(|w| pick(w)[day]).sum())
                .collect()
        };
        let (people_read, people_write, people_admin) = people(&snapshot.records, &instances);
        counts.push(PluginUsageCount {
            listing,
            version,
            registry,
            instances: instances.len() as u32,
            people_read,
            people_write,
            people_admin,
            daily_operations: sum(|w| &w.0),
            daily_requests: sum(|w| &w.1),
        });
    }
    PluginUsageReport {
        period_start: start.to_string(),
        period_end: end.to_string(),
        counts,
    }
}

/// Sends the week's report once it is due.
pub struct UsageSender {
    hold: Arc<Hold>,
    counted: Arc<Counted>,
}

impl UsageSender {
    pub fn new(hold: Arc<Hold>) -> Self {
        UsageSender {
            hold,
            counted: Arc::default(),
        }
    }

    pub fn counted(&self) -> &Arc<Counted> {
        &self.counted
    }

    /// Last week's report, unless it was sent already, the report is off,
    /// or nothing installed from a registry runs. Answers what was sent.
    pub async fn send_if_due(&self) -> Result<Option<SentPluginUsageReport>, String> {
        let store = Arc::clone(self.hold.store());
        let (on, sent, snapshot) = tokio::task::spawn_blocking(move || {
            Ok::<_, String>((
                store.reports_plugin_usage().map_err(|f| f.to_string())?,
                store.plugin_usage_reports(1).map_err(|f| f.to_string())?,
                store.snapshot().map_err(|f| f.to_string())?,
            ))
        })
        .await
        .map_err(|failed| failed.to_string())??;
        if !on {
            return Ok(None);
        }
        let now = self.hold.clock.now_ns();
        let (start, end) = last_week(Date::of_utc_instant(now));
        let covered = sent.first().and_then(|last| {
            last.report
                .as_ref()
                .and_then(|report| Date::parse(&report.period_end).ok())
        });
        if covered.is_some_and(|covered| covered >= end) {
            return Ok(None);
        }
        let report = report_for(&snapshot, &self.counted, start, end);
        if report.counts.is_empty() {
            return Ok(None);
        }
        let received = match self.hold.upstream().report_usage(&report).await {
            Ok(received) => received,
            Err(failed) => {
                tracing::info!(%failed, "the plugin usage report was not received; the next replaces it");
                false
            }
        };
        let kept = SentPluginUsageReport {
            sent_at_ns: now,
            received,
            report: Some(report),
        };
        let store = Arc::clone(self.hold.store());
        let record = kept.clone();
        tokio::task::spawn_blocking(move || store.record_plugin_usage_report(&record))
            .await
            .map_err(|failed| failed.to_string())?
            .map_err(|failed| failed.to_string())?;
        Ok(Some(kept))
    }

    /// Hear the sidecars' reports, and look hourly whether a week is due.
    pub async fn start(self: Arc<Self>, bus: Arc<Bus>) {
        let mut reports = bus.subscribe_every(PLUGIN_REPORT);
        let counted = Arc::clone(&self.counted);
        tokio::spawn(async move {
            while let Some(delivery) = reports.recv().await {
                if let Ok(report) = PluginReport::decode(&delivery.envelope.payload[..]) {
                    counted.hear(&report);
                }
            }
        });
        loop {
            if let Err(failed) = self.send_if_due().await {
                tracing::info!(%failed, "the plugin usage report was not sent; tried again shortly");
            }
            tokio::time::sleep(LOOK_EVERY).await;
        }
    }
}

/// Pull the catalogue hourly, from the start (requirement 32).
pub async fn pull_hourly(hold: Arc<Hold>) {
    loop {
        if let Err(failed) = hold.pull().await {
            tracing::info!(%failed, "the plugin catalogue was not pulled; tried again in an hour");
        }
        tokio::time::sleep(LOOK_EVERY).await;
    }
}

#[cfg(test)]
mod tests {
    use meridian_domain::v1::{
        AccessEntry, AccessGroup, Permission, PluginLaunch, PluginMetadata, PluginVersion,
        UserGroup,
    };

    use super::*;

    // 2026-10-14, a Wednesday: last week is 2026-10-05 to 2026-10-11.
    const NOW: i64 = 1_792_022_400_000_000_000;

    fn snapshot() -> crate::store::Snapshot {
        let version = |name: &str, registry: &str, listing: &str| PluginVersion {
            metadata: Some(PluginMetadata {
                name: name.into(),
                version: "1.0.0".into(),
                ..Default::default()
            }),
            source_registry: registry.into(),
            listing: listing.into(),
            ..Default::default()
        };
        let launch = |instance: &str, name: &str| PluginLaunch {
            instance_id: instance.into(),
            name: name.into(),
            version: "1.0.0".into(),
            state: PluginLaunchState::Launched as i32,
            ..Default::default()
        };
        let mut held = crate::store::Snapshot::default();
        held.catalogue.versions = vec![
            version("acme/fx", "community", "acme/fx"),
            version("firm-own", "local", ""),
            version("acme/quiet", "community", "acme/quiet"),
        ];
        held.catalogue.launches = vec![
            launch("fx-1", "acme/fx"),
            launch("fx-2", "acme/fx"),
            launch("own-1", "firm-own"),
            launch("quiet-1", "acme/quiet"),
        ];
        let entry = |instance: &str, level: meridian_access::AccessLevel| AccessEntry {
            plugin_instance_id: instance.into(),
            level: level as i32,
            role: String::new(),
        };
        held.records.user_groups = vec![
            UserGroup {
                user_group_id: "UG-1".into(),
                logins: vec!["ada".into(), "ben".into()],
                ..Default::default()
            },
            UserGroup {
                user_group_id: "UG-2".into(),
                logins: vec!["ada".into()],
                ..Default::default()
            },
        ];
        held.records.access_groups = vec![
            AccessGroup {
                access_group_id: "AG-1".into(),
                entries: vec![entry("fx-1", meridian_access::AccessLevel::Read)],
                ..Default::default()
            },
            AccessGroup {
                access_group_id: "AG-2".into(),
                entries: vec![entry("fx-2", meridian_access::AccessLevel::Admin)],
                ..Default::default()
            },
        ];
        held.records.permissions = vec![
            Permission {
                permission_id: "P-1".into(),
                user_group_id: "UG-1".into(),
                access_group_id: "AG-1".into(),
                ..Default::default()
            },
            Permission {
                permission_id: "P-2".into(),
                user_group_id: "UG-2".into(),
                access_group_id: "AG-2".into(),
                ..Default::default()
            },
        ];
        held
    }

    #[test]
    fn a_week_is_reported_per_registry_version_and_never_a_local_one() {
        let counted = Counted::default();
        // fx-1 reported on 2026-10-12 (its seven days 10-06 to 10-12).
        let reported = Date::parse("2026-10-12").unwrap().days_since_epoch() * DAY_NS + 60;
        counted.hear(&PluginReport {
            plugin_instance_id: "fx-1".into(),
            reported_at_ns: reported,
            daily_operations: vec![1, 2, 3, 4, 5, 6, 7],
            daily_requests: vec![0, 0, 0, 0, 0, 0, 9],
            ..Default::default()
        });
        counted.hear(&PluginReport {
            plugin_instance_id: "own-1".into(),
            reported_at_ns: reported,
            daily_operations: vec![5; 7],
            daily_requests: vec![5; 7],
            ..Default::default()
        });
        let (start, end) = last_week(Date::of_utc_instant(NOW));
        assert_eq!(
            (start.to_string(), end.to_string()),
            ("2026-10-05".into(), "2026-10-11".into())
        );
        let report = report_for(&snapshot(), &counted, start, end);
        assert_eq!(
            report.counts.len(),
            1,
            "no local plugin, and none with no counts yet"
        );
        let fx = &report.counts[0];
        assert_eq!(
            (fx.listing.as_str(), fx.registry.as_str(), fx.instances),
            ("acme/fx", "community", 2)
        );
        // 10-05 not reported; 10-06 to 10-11 the first six of the seven.
        assert_eq!(fx.daily_operations, [0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(fx.daily_requests, [0; 7]);
        // ben reads fx-1; ada reads fx-1 and administers fx-2: once, at admin.
        assert_eq!(
            (fx.people_read, fx.people_write, fx.people_admin),
            (1, 0, 1)
        );
    }
}
