//! What each plugin's sidecar last said about its plugin (W4.8), for the
//! deployment's health in detail (W6.10): registered or not, healthy or not
//! and why, the last heartbeat, the contract version, and grants refused.
//!
//! Heard on the bus and held in memory, as [`crate::custody`] holds what the
//! connectors say: a sidecar reports every 30 seconds, so a restart forgets
//! a plugin only until then. A report counts only from the plugin's own
//! sidecar, which is on the bus as the plugin's instance, as the conductor
//! reads it: how a plugin is doing is not another component's to say.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use meridian_bus::Bus;
use meridian_domain::v1::PluginReport;
use meridian_pb::v1::StoredSpan;
use prost::Message;

use crate::clock::SECOND_NS;

/// W4.8, from every plugin's sidecar.
pub const PLUGIN_REPORT: &str = "platform.deployment.event.plugin-report";

/// A report older than this is from a sidecar that has stopped saying
/// anything: three of its 30-second reports missed.
pub const SILENT_NS: i64 = 90 * SECOND_NS;

/// Each instance's shards' storage: by instance, then shard, when its last
/// registered report was made and what it said it stores (item 30).
type ShardsStored = BTreeMap<String, BTreeMap<u32, (i64, Vec<StoredSpan>)>>;

#[derive(Debug, Default)]
pub struct Health {
    heard: Mutex<BTreeMap<String, PluginReport>>,
    /// What each shard of an instance keeps, by instance and shard, as its
    /// last registered report said, and when (step 3d, checkpoint 5, item
    /// 30): each shard writes its own folder of the instance's storage.
    shards: Mutex<ShardsStored>,
}

/// An instance's shards' storage summed, kind by kind: the records and
/// bytes added, the span from the earliest first to the latest last.
fn summed<'a>(each: impl Iterator<Item = &'a Vec<StoredSpan>>) -> Vec<StoredSpan> {
    let mut kinds: BTreeMap<String, StoredSpan> = BTreeMap::new();
    for span in each.flatten() {
        let held = kinds
            .entry(span.record_kind.clone())
            .or_insert_with(|| StoredSpan {
                record_kind: span.record_kind.clone(),
                ..Default::default()
            });
        held.record_count += span.record_count;
        held.bytes += span.bytes;
        if span.first_received_ns != 0
            && (held.first_received_ns == 0 || span.first_received_ns < held.first_received_ns)
        {
            held.first_received_ns = span.first_received_ns;
        }
        held.last_received_ns = held.last_received_ns.max(span.last_received_ns);
    }
    kinds.into_values().collect()
}

impl Health {
    fn heard(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, PluginReport>> {
        // Display data; what a poisoned lock holds is still worth showing.
        self.heard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Keep a report, if it is its plugin's own sidecar saying it.
    pub fn hear(&self, publisher: &str, report: PluginReport) {
        if publisher != report.plugin_instance_id {
            return;
        }
        // A registered plugin's storage, kept by its shard (item 30); a pod
        // holding no shard has no plugin registered, and adds nothing.
        if report.registered {
            self.shards
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .entry(report.plugin_instance_id.clone())
                .or_default()
                .insert(report.shard, (report.reported_at_ns, report.stored.clone()));
        }
        self.heard()
            .insert(report.plugin_instance_id.clone(), report);
    }

    /// Every plugin heard from, by instance: its last report, with what it
    /// stores summed over its shards that reported within [`SILENT_NS`] of
    /// it (item 30); a shard retired, or a pod gone, falls out after that.
    pub fn view(&self) -> BTreeMap<String, PluginReport> {
        let mut view = self.heard().clone();
        let shards = self
            .shards
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (instance, report) in &mut view {
            let Some(held) = shards.get(instance) else {
                continue;
            };
            let latest = held
                .values()
                .map(|(at, _)| *at)
                .max()
                .unwrap_or_default()
                .max(report.reported_at_ns);
            report.stored = summed(
                held.values()
                    .filter(|(at, _)| latest - *at <= SILENT_NS)
                    .map(|(_, stored)| stored),
            );
        }
        view
    }
}

/// How a plugin is, in a word and a sentence, from its last report as of
/// `now`: the word for a badge, whether it asks anything of anybody, and the
/// state of the status dot the plugin's area draws for it where a page tells
/// none of its own (meridian-ui's `om-status`: `ok`, `warn` or `error`).
pub struct State {
    pub word: &'static str,
    pub good: bool,
    pub dot: &'static str,
    pub detail: String,
}

pub fn state(report: Option<&PluginReport>, now: i64) -> State {
    let Some(report) = report else {
        return State {
            word: "Not heard from",
            good: false,
            dot: "warn",
            detail: "Its sidecar has not reported since this dashboard started.".into(),
        };
    };
    if now - report.reported_at_ns > SILENT_NS {
        return State {
            word: "Silent",
            good: false,
            dot: "error",
            detail: "Its sidecar has stopped reporting.".into(),
        };
    }
    if !report.registered {
        return State {
            word: "Not registered",
            good: false,
            dot: "warn",
            detail: "The plugin has not registered with its sidecar: it is starting, or it left."
                .into(),
        };
    }
    if !report.healthy {
        return State {
            word: "Not healthy",
            good: false,
            dot: "error",
            detail: if report.health_detail.is_empty() {
                "The plugin says it is not healthy, and not why.".into()
            } else {
                report.health_detail.clone()
            },
        };
    }
    State {
        word: "Healthy",
        good: true,
        dot: "ok",
        detail: report.health_detail.clone(),
    }
}

/// Subscribed before this returns: what arrives before a subscriber exists
/// is not heard.
pub fn listen(bus: &Bus, health: Arc<Health>) {
    let mut reports = bus.subscribe_every(PLUGIN_REPORT);
    tokio::spawn(async move {
        while let Some(delivery) = reports.recv().await {
            let publisher = delivery
                .envelope
                .meta
                .as_ref()
                .map(|meta| meta.publisher_instance_id.clone())
                .unwrap_or_default();
            match PluginReport::decode(&delivery.envelope.payload[..]) {
                Ok(report) => health.hear(&publisher, report),
                Err(failed) => tracing::warn!("a plugin report did not decode: {failed}"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use meridian_bus::MemoryBackend;

    use super::*;

    const T0: i64 = 1_790_380_800_000_000_000;

    fn report(instance: &str, healthy: bool) -> PluginReport {
        PluginReport {
            plugin_instance_id: instance.into(),
            registered: true,
            healthy,
            health_detail: if healthy {
                String::new()
            } else {
                "required setting api_key is not set".into()
            },
            reported_at_ns: T0,
            ..Default::default()
        }
    }

    #[test]
    fn a_report_counts_only_from_the_plugins_own_sidecar() {
        let health = Health::default();
        health.hear("snaptrade-2", report("snaptrade-1", true));
        assert!(health.view().is_empty(), "another sidecar's word about it");
        health.hear("snaptrade-1", report("snaptrade-1", false));
        assert!(!health.view()["snaptrade-1"].healthy);
    }

    fn span(kind: &str, count: u64, first: i64, last: i64, bytes: u64) -> StoredSpan {
        StoredSpan {
            record_kind: kind.into(),
            record_count: count,
            first_received_ns: first,
            last_received_ns: last,
            bytes,
        }
    }

    /// Item 30: each shard keeps its own storage, so what an instance holds
    /// is its shards' reports summed, not whichever pod spoke last; a pod
    /// holding no shard, and a shard silent past SILENT_NS, add nothing.
    #[test]
    fn what_an_instance_stores_is_its_shards_summed() {
        let health = Health::default();
        let shard = |n: u32, at: i64, stored: Vec<StoredSpan>| PluginReport {
            shard: n,
            reported_at_ns: at,
            stored,
            ..report("feed-1", true)
        };
        health.hear(
            "feed-1",
            shard(0, T0, vec![span("responses", 3, T0 - 30, T0 - 10, 100)]),
        );
        health.hear(
            "feed-1",
            shard(
                1,
                T0 + 1,
                vec![
                    span("responses", 2, T0 - 50, T0 - 20, 40),
                    span("quotes", 1, T0 - 5, T0 - 5, 0),
                ],
            ),
        );
        // A standby: shard 0 as it holds none, its plugin not registered.
        health.hear(
            "feed-1",
            PluginReport {
                registered: false,
                reported_at_ns: T0 + 2,
                ..report("feed-1", true)
            },
        );
        let stored = &health.view()["feed-1"].stored;
        assert_eq!(
            stored,
            &vec![
                span("quotes", 1, T0 - 5, T0 - 5, 0),
                span("responses", 5, T0 - 50, T0 - 10, 140),
            ],
            "the instance's shards summed, the standby adding nothing"
        );
        // Shard 1 retired: silent past SILENT_NS, it is no longer counted.
        health.hear(
            "feed-1",
            shard(
                0,
                T0 + SILENT_NS + 2,
                vec![span("responses", 4, T0 - 30, T0 + 9, 120)],
            ),
        );
        assert_eq!(
            health.view()["feed-1"].stored,
            vec![span("responses", 4, T0 - 30, T0 + 9, 120)]
        );
    }

    #[test]
    fn a_state_says_why_and_a_silent_sidecar_is_not_taken_as_healthy() {
        let unwell = report("snaptrade-1", false);
        let said = state(Some(&unwell), T0);
        assert_eq!(said.word, "Not healthy");
        assert!(!said.good);
        assert_eq!(said.dot, "error");
        assert_eq!(said.detail, "required setting api_key is not set");

        let well = report("snaptrade-1", true);
        assert!(state(Some(&well), T0 + SILENT_NS).good);
        assert_eq!(state(Some(&well), T0 + SILENT_NS).dot, "ok");
        assert_eq!(state(Some(&well), T0 + SILENT_NS + 1).word, "Silent");
        assert_eq!(state(Some(&well), T0 + SILENT_NS + 1).dot, "error");

        let left = PluginReport {
            registered: false,
            ..well
        };
        assert_eq!(state(Some(&left), T0).word, "Not registered");
        assert_eq!(state(Some(&left), T0).dot, "warn");
        assert_eq!(state(None, T0).word, "Not heard from");
        assert_eq!(state(None, T0).dot, "warn");
    }

    #[tokio::test]
    async fn what_a_sidecar_publishes_is_heard() {
        let backend = Arc::new(MemoryBackend::new());
        let dashboard = Bus::single(
            "dashboard-1",
            backend.clone(),
            Arc::new(meridian_clock::SystemClock),
        );
        let health = Arc::new(Health::default());
        listen(&dashboard, Arc::clone(&health));
        Bus::single(
            "snaptrade-1",
            backend,
            Arc::new(meridian_clock::SystemClock),
        )
        .publish(
            PLUGIN_REPORT,
            "meridian.v1.PluginReport",
            report("snaptrade-1", true).encode_to_vec(),
            None,
            None,
        )
        .unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while health.view().is_empty() {
            assert!(tokio::time::Instant::now() < deadline, "nothing was heard");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(health.view()["snaptrade-1"].healthy);
    }
}
