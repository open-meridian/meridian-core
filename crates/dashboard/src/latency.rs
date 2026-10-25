//! Core's latency monitor, in the dashboard (contract v28's latency
//! remainder; W4.16, W10.9; rulings 515 to 518): `ReportLatency` received
//! from every data, connectivity and `ems` plugin's sidecar and merged into
//! the monitor's store (`meridian_domain::latency`), read by the Data
//! sources page's Age tab and `dashboard__read_data_age`, the deployment
//! admin's. Measurements, not records: no row, nothing journalled.

use std::sync::Arc;

use meridian_domain::latency::{self, Histogram, Key, Read, Scope};
use meridian_domain::v1::{AgeHop, LatencyMetric, ReportLatencyReply, ReportLatencyRequest};
use prost::Message;
use serde_json::{json, Value};

use crate::html::escape;
use crate::web::App;

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

/// One call's snapshots merged, each under the instance its sidecar
/// stamped -- the envelope's publisher where a snapshot names none or
/// another -- or refused whole, naming the snapshot.
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
        merged.push((
            Key {
                instance: publisher.to_string(),
                scope,
            },
            at,
            held,
            snapshot.alive_at_ns,
            snapshot.counters.as_ref().map_or(0, |c| c.backlog_peak),
        ));
    }
    let n = merged.len() as u32;
    for (key, at, held, alive, backlog) in merged {
        latency::monitor().merge(key, at, &held, alive, backlog);
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

fn hop_word(hop: &str) -> &str {
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

fn metric_word(metric: &str) -> &str {
    match metric {
        "LATENCY_METRIC_RTT" => "round trip",
        "LATENCY_METRIC_ORDER_ACK" => "order ack",
        "LATENCY_METRIC_FILL_DELAY" => "fill delay",
        "LATENCY_METRIC_AGE" => "age",
        other => other,
    }
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
                 <td title=\"{instance}\">{instance}</td><td>{what}</td>\
                 <td class=\"number\">{p50}</td><td class=\"number wide\">{p95}</td>\
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
         <thead><tr><th>Dataset or session</th><th>Plugin</th><th>Hop</th><th>p50 ms</th>\
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
}
