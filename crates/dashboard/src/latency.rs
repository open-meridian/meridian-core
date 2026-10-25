//! Core's latency monitor, in the dashboard (contract v28's latency
//! remainder; W4.16, W10.9; rulings 515 to 518 and 606): `ReportLatency`
//! received from every data, connectivity and `ems` plugin's sidecar and,
//! from the third amendment, from core's own components -- the sidecars,
//! the lake and the analytics component, each for the hops it records --
//! merged into the monitor's store (`meridian_domain::latency`), read by the
//! Data sources page's Age tab and `dashboard__read_data_age`, the
//! deployment admin's, and by a plugin's Summary. Measurements, not
//! records: no row, nothing journalled. The episodes the judge opens and
//! closes are records: one notice each, to the plugin's admins and, for a
//! connectivity plugin's session, its `ems`'s (ruling 427).

use std::sync::Arc;

use meridian_domain::latency::{self, Histogram, Key, Read, Said, Scope};
use meridian_domain::v1::{
    AgeHop, LatencyCeilings, LatencyMetric, ReportLatencyReply, ReportLatencyRequest,
};
use prost::Message;
use serde_json::{json, Value};

use crate::html::escape;
use crate::web::App;

pub mod notices;

/// The monitor's topic (W4.16): a plugin's snapshots, from its sidecar.
pub const REPORT_LATENCY: &str = "platform.deployment.command.report-latency";

/// The most snapshots one call carries.
const MOST: usize = 500;

/// The window a read covers: the last minute, as the Age tab says.
pub const WINDOW_NS: i64 = 60 * 1_000_000_000;

fn histogram(held: Option<&meridian_domain::v1::LatencyHistogram>) -> Histogram {
    let Some(held) = held else {
        return Histogram::default();
    };
    Histogram {
        counts: held.buckets.iter().map(|b| (b.bucket, b.count)).collect(),
        min_ns: held.min_ns,
        max_ns: held.max_ns,
        total: held.total,
    }
}

/// The ceiling that applies to a snapshot's metric or hop, as it carried
/// it: the round trip's, the venue's acknowledgement's and fill delay's,
/// and the feed delay's at the edge; none at the other hops, which are
/// judged against their own week alone.
fn ceiling_of(scope: &Scope, ceilings: Option<&LatencyCeilings>) -> i64 {
    let Some(c) = ceilings else {
        return 0;
    };
    match scope {
        Scope::Session { metric, .. } => match metric.as_str() {
            "LATENCY_METRIC_RTT" => c.rtt_p99_ns,
            "LATENCY_METRIC_ORDER_ACK" => c.order_ack_p99_ns,
            "LATENCY_METRIC_FILL_DELAY" => c.fill_delay_p99_ns,
            _ => 0,
        },
        Scope::Dataset { hop, .. } if hop == "AGE_HOP_EDGE" => c.feed_delay_p99_ns,
        Scope::Dataset { .. } => 0,
    }
}

/// One call's snapshots merged, each under the instance its sidecar
/// stamped -- the envelope's publisher where a snapshot names none or
/// another; the component's own name for a core component's hops (606) --
/// or refused whole, naming the snapshot.
pub fn receive(
    request: &ReportLatencyRequest,
    publisher: &str,
    now_ns: i64,
) -> Result<u32, String> {
    if request.snapshots.is_empty() || request.snapshots.len() > MOST {
        return Err(format!(
            "invalid_argument: snapshots holds {}; a batch is 1 to {MOST}",
            request.snapshots.len()
        ));
    }
    let mut merged = Vec::with_capacity(request.snapshots.len());
    for (i, snapshot) in request.snapshots.iter().enumerate() {
        let metric = LatencyMetric::try_from(snapshot.metric).unwrap_or(LatencyMetric::Unspecified);
        let hop = AgeHop::try_from(snapshot.hop).unwrap_or(AgeHop::Unspecified);
        let scope = match (snapshot.session.is_empty(), snapshot.dataset.is_empty()) {
            (false, true) if metric != LatencyMetric::Unspecified => Scope::Session {
                session: snapshot.session.clone(),
                metric: metric.as_str_name().to_string(),
            },
            (true, false) if hop != AgeHop::Unspecified => Scope::Dataset {
                dataset: snapshot.dataset.clone(),
                hop: hop.as_str_name().to_string(),
            },
            _ => {
                return Err(format!(
                    "invalid_argument: snapshots[{i}] names a session and its metric, or a dataset \
                     and its hop"
                ))
            }
        };
        let held = histogram(snapshot.histogram.as_ref());
        if held.counts.values().sum::<u64>() != held.total {
            return Err(format!(
                "invalid_argument: snapshots[{i}].histogram's buckets do not sum to its total"
            ));
        }
        let at = match snapshot.interval_start_ns + snapshot.interval_ns {
            0 => now_ns,
            end => end,
        };
        let said = Said {
            alive_at_ns: snapshot.alive_at_ns,
            backlog_peak: snapshot.counters.as_ref().map_or(0, |c| c.backlog_peak),
            ceiling_ns: ceiling_of(&scope, snapshot.ceilings.as_ref()),
            backlog_max: snapshot
                .ceilings
                .as_ref()
                .map_or(0, |c| u64::from(c.backlog_max)),
        };
        merged.push((
            Key {
                instance: publisher.to_string(),
                scope,
            },
            at,
            held,
            said,
        ));
    }
    let n = merged.len() as u32;
    for (key, at, held, said) in merged {
        latency::monitor().merge(key, at, &held, said);
    }
    Ok(n)
}

/// Answer the monitor's topic on the bus, from now on.
pub fn serve(app: Arc<App>) {
    let clock = Arc::clone(&app.clock);
    app.bus.serve(REPORT_LATENCY, move |envelope| {
        let publisher = envelope
            .meta
            .as_ref()
            .map(|meta| meta.publisher_instance_id.clone())
            .unwrap_or_default();
        let request = ReportLatencyRequest::decode(&envelope.payload[..])
            .map_err(|failed| format!("invalid_argument: the report did not read: {failed}"))?;
        let accepted = receive(&request, &publisher, clock.now_ns())?;
        Ok((
            "meridian.v1.ReportLatencyReply".to_string(),
            ReportLatencyReply { accepted }.encode_to_vec(),
        ))
    });
}

/// The monitor's reads now, over the last minute.
pub fn reads(now_ns: i64) -> Vec<Read> {
    latency::monitor().read(now_ns, WINDOW_NS)
}

fn ms(ns: i64) -> String {
    let ms = ns as f64 / 1e6;
    if ms >= 100.0 {
        format!("{ms:.0}")
    } else {
        format!("{ms:.1}")
    }
}

pub(crate) fn hop_word(hop: &str) -> &str {
    match hop {
        "AGE_HOP_EDGE" => "edge",
        "AGE_HOP_SIDECAR" => "sidecar",
        "AGE_HOP_LAKE" => "lake",
        "AGE_HOP_BUS" => "bus",
        "AGE_HOP_ANALYTICS" => "analytics",
        "AGE_HOP_EMS_CHECK" => "ems check",
        "AGE_HOP_PAGE" => "page",
        other => other,
    }
}

pub(crate) fn metric_word(metric: &str) -> &str {
    match metric {
        "LATENCY_METRIC_RTT" => "round trip",
        "LATENCY_METRIC_ORDER_ACK" => "order ack",
        "LATENCY_METRIC_FILL_DELAY" => "fill delay",
        "LATENCY_METRIC_AGE" => "age",
        other => other,
    }
}

/// One instance's series over the last minute: its sessions' latency and
/// its datasets' ages at each hop, its sidecar's hops among them.
pub fn of_instance(instance: &str, now_ns: i64) -> Vec<Read> {
    reads(now_ns)
        .into_iter()
        .filter(|read| read.key.instance == instance)
        .collect()
}

fn series_words(read: &Read) -> String {
    match &read.key.scope {
        Scope::Session { session, metric } => format!("{session} {}", metric_word(metric)),
        Scope::Dataset { dataset, hop } => format!("{dataset} at {}", hop_word(hop)),
    }
}

/// The Summary's status line for latency (W4.16, "core's Summary status
/// line"): an episode open, the first named with when it opened; else the
/// highest p99 measured in the last minute. Empty where nothing was.
pub fn summary_line(instance: &str, now_ns: i64) -> String {
    let reads = of_instance(instance, now_ns);
    let open: Vec<&Read> = reads
        .iter()
        .filter(|r| r.latency_since_ns != 0 || r.backlog_since_ns != 0)
        .collect();
    let said = if let Some(first) = open.first() {
        let since = first.latency_since_ns.max(first.backlog_since_ns);
        format!(
            "<span class=\"badge warn\">Breached</span> {} since {}{}",
            escape(&series_words(first)),
            escape(&crate::custody::utc(since)),
            if open.len() > 1 {
                format!(", and {} more", open.len() - 1)
            } else {
                String::new()
            }
        )
    } else if let Some(worst) = reads.iter().max_by_key(|r| r.p99_ns) {
        format!(
            "Within its ceilings; highest p99 {} ms, {}",
            ms(worst.p99_ns),
            escape(&series_words(worst))
        )
    } else {
        return String::new();
    };
    // One line, cut with an ellipsis on a phone; the whole of it in the
    // title (W4.16; every page fits one screen).
    let whole = said.replace("<span class=\"badge warn\">Breached</span>", "Breached:");
    format!("<dt>Latency</dt><dd data-latency title=\"{whole}\">{said}</dd>")
}

/// The Summary's latency part as `dashboard__read_plugin_summary` answers
/// it: each series of the instance over the last minute, with its ceiling
/// and its open episodes.
pub fn summary_json(instance: &str, now_ns: i64) -> Value {
    Value::Array(
        of_instance(instance, now_ns)
            .iter()
            .map(|read| {
                let mut said = json(read);
                said["ceiling_ns"] = read.ceiling_ns.to_string().into();
                said["latency_breached_since_ns"] = read.latency_since_ns.to_string().into();
                said["backlog_breached_since_ns"] = read.backlog_since_ns.to_string().into();
                said
            })
            .collect(),
    )
}

/// A read as the tool answers it.
pub fn json(read: &Read) -> Value {
    let (kind, name, what) = match &read.key.scope {
        Scope::Dataset { dataset, hop } => ("dataset", dataset.as_str(), hop_word(hop)),
        Scope::Session { session, metric } => ("session", session.as_str(), metric_word(metric)),
    };
    json!({
        "instance": read.key.instance,
        "kind": kind,
        "name": name,
        "hop_or_metric": what,
        "p50_ns": read.p50_ns.to_string(),
        "p95_ns": read.p95_ns.to_string(),
        "p99_ns": read.p99_ns.to_string(),
        "count": read.count,
        "last_at_ns": read.last_at_ns.to_string(),
        "alive_at_ns": read.alive_at_ns.to_string(),
        "backlog_peak": read.backlog_peak,
    })
}

/// The Age tab's body (W10.9): a line per dataset and hop, and per plugin
/// session and metric, over the last minute; searched and paged.
pub fn tab(reads: &[Read]) -> String {
    if reads.is_empty() {
        return "<p class=\"empty\" data-ages=\"0\">Nothing measured in the last minute: a data, \
                connectivity or execution plugin reports its data age and its sessions' latency \
                every ten seconds.</p>"
            .to_string();
    }
    let lines: String = reads
        .iter()
        .map(|read| {
            let (name, what) = match &read.key.scope {
                Scope::Dataset { dataset, hop } => (dataset.as_str(), hop_word(hop)),
                Scope::Session { session, metric } => (session.as_str(), metric_word(metric)),
            };
            format!(
                "<tr data-id=\"{instance} {name} {what}\"><td title=\"{name}\">{name}</td>\
                 <td class=\"wide\" title=\"{instance}\">{instance}</td><td>{what}</td>\
                 <td class=\"number wide\">{p50}</td><td class=\"number wide\">{p95}</td>\
                 <td class=\"number\">{p99}</td><td class=\"number wide\">{count}</td></tr>",
                instance = escape(&read.key.instance),
                name = escape(name),
                what = escape(what),
                p50 = ms(read.p50_ns),
                p95 = ms(read.p95_ns),
                p99 = ms(read.p99_ns),
                count = read.count,
            )
        })
        .collect();
    format!(
        "<om-pager search=\"Search data age\"><table class=\"list one-line ages\" data-ages=\"{n}\">\
         <thead><tr><th>Dataset or session</th><th class=\"wide\">Plugin</th><th>Hop</th><th class=\"wide\">p50 ms</th>\
         <th class=\"wide\">p95 ms</th><th>p99 ms</th><th class=\"wide\">Count</th></tr></thead>\
         <tbody>{lines}</tbody></table></om-pager>",
        n = reads.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use meridian_domain::v1::{LatencyBucket, LatencyHistogram, LatencySnapshot};

    #[test]
    fn a_report_is_merged_under_its_sidecars_instance_or_refused_whole() {
        let now = 1_792_400_000_000_000_000;
        let snapshot = |dataset: &str, hop: AgeHop| LatencySnapshot {
            interval_start_ns: now - 10_000_000_000,
            interval_ns: 10_000_000_000,
            dataset: dataset.into(),
            hop: hop as i32,
            metric: LatencyMetric::Age as i32,
            histogram: Some(LatencyHistogram {
                lowest_ns: 1_000,
                significant_digits: 2,
                buckets: vec![LatencyBucket {
                    bucket: latency::bucket_of(4_000_000),
                    count: 3,
                }],
                min_ns: 4_000_000,
                max_ns: 4_000_000,
                total: 3,
            }),
            instance_id: "someone-else".into(),
            ..Default::default()
        };
        let accepted = receive(
            &ReportLatencyRequest {
                snapshots: vec![snapshot("v28-latency-test:quotes", AgeHop::Edge)],
            },
            "v28-latency-test",
            now,
        )
        .unwrap();
        assert_eq!(accepted, 1);
        let read = reads(now)
            .into_iter()
            .find(|r| r.key.instance == "v28-latency-test")
            .unwrap();
        assert_eq!(read.count, 3);
        assert_eq!(json(&read)["hop_or_metric"], "edge");
        assert!(tab(&[read]).contains("v28-latency-test:quotes"));
        // A snapshot naming neither, or both, refuses the call whole.
        let mut both = snapshot("v28-latency-test:quotes", AgeHop::Edge);
        both.session = "pricing".into();
        let said = receive(
            &ReportLatencyRequest {
                snapshots: vec![snapshot("x:y", AgeHop::Edge), both],
            },
            "v28-latency-test-2",
            now,
        )
        .unwrap_err();
        assert!(said.contains("snapshots[1]"), "{said}");
        assert!(!reads(now)
            .iter()
            .any(|r| r.key.instance == "v28-latency-test-2"));
        let said = receive(&ReportLatencyRequest::default(), "x", now).unwrap_err();
        assert!(said.contains("1 to 500"), "{said}");
    }

    #[test]
    fn the_summary_says_an_open_episode_or_else_the_highest_p99() {
        let instance = "v28-summary-line";
        let start = 1_792_500_000_000_000_000 / 60_000_000_000 * 60_000_000_000;
        assert_eq!(summary_line(instance, start), "");
        let snapshot = |at: i64, ms: i64| LatencySnapshot {
            interval_start_ns: at,
            interval_ns: 10_000_000_000,
            session: "pricing-stream".into(),
            metric: LatencyMetric::Rtt as i32,
            histogram: Some(LatencyHistogram {
                lowest_ns: 1_000,
                significant_digits: 2,
                buckets: vec![LatencyBucket {
                    bucket: latency::bucket_of(ms * 1_000_000),
                    count: 1,
                }],
                min_ns: ms * 1_000_000,
                max_ns: ms * 1_000_000,
                total: 1,
            }),
            ceilings: Some(LatencyCeilings {
                rtt_p99_ns: 250_000_000,
                ..Default::default()
            }),
            ..Default::default()
        };
        receive(
            &ReportLatencyRequest {
                snapshots: vec![snapshot(start, 40)],
            },
            instance,
            start,
        )
        .unwrap();
        let line = summary_line(instance, start + 20_000_000_000);
        assert!(line.contains("Within its ceilings"), "{line}");
        assert!(line.contains("pricing-stream round trip"), "{line}");
        for minute in 1..=3 {
            let at = start + minute * 60_000_000_000;
            receive(
                &ReportLatencyRequest {
                    snapshots: vec![snapshot(at, 2_000)],
                },
                instance,
                at,
            )
            .unwrap();
        }
        let judged = latency::monitor().judge(start + 4 * 60_000_000_000 + 1);
        assert!(judged
            .iter()
            .any(|t| t.key.instance == instance && t.kind == "latency_breached"));
        let now = start + 4 * 60_000_000_000;
        let line = summary_line(instance, now);
        assert!(line.contains("Breached"), "{line}");
        let part = summary_json(instance, now);
        assert_eq!(part[0]["ceiling_ns"], "250000000");
        assert_ne!(part[0]["latency_breached_since_ns"], "0");
    }
}
