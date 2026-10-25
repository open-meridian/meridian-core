//! Core's latency monitor (contract v28's latency remainder; the latency
//! spec's "What reaches core" and "The hops"; rulings 515 to 518 and 570
//! (ii)): log-linear histograms at 2 significant digits from 1 µs to 1 hour,
//! HdrHistogram's layout as the SDK's recorder writes them, merged by adding
//! counts -- per minute for 7 days, per hour for 30 -- keyed by the reporting
//! instance and the session and metric, or the dataset and hop.
//!
//! Two writers, one store: a plugin's `ReportLatency` snapshots, received
//! across its sidecar, and core's own hops, which write here directly and
//! never publish (570 (ii)). One reader: the Data sources page's Age tab and
//! `dashboard__read_data_age`. Measurements, not records: nothing here is
//! journalled, and a restart begins the window again (the latency spec;
//! "Stateless compute, owned storage").

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

/// The layout (ruling 416): 1 µs to 1 hour at 2 significant digits.
pub const LOWEST_NS: i64 = 1_000;
pub const HIGHEST_NS: i64 = 3_600 * 1_000_000_000;
pub const SIGNIFICANT_DIGITS: u32 = 2;

const MINUTE_NS: i64 = 60 * 1_000_000_000;
const HOUR_NS: i64 = 60 * MINUTE_NS;
/// Per minute for 7 days, per hour for 30.
const MINUTES_KEPT: i64 = 7 * 24 * 60;
const HOURS_KEPT: i64 = 30 * 24;

// HdrHistogram's layout for LOWEST_NS and SIGNIFICANT_DIGITS, as the SDK's
// `meridian.roles._latency` computes it.
const UNIT_MAGNITUDE: u32 = 63 - (LOWEST_NS as u64).leading_zeros(); // 9
const SUB_BUCKET_COUNT_MAGNITUDE: u32 = 64 - (2 * 100 - 1_u64).leading_zeros(); // 8
const SUB_BUCKET_HALF_COUNT_MAGNITUDE: u32 = SUB_BUCKET_COUNT_MAGNITUDE - 1;
const SUB_BUCKET_COUNT: i64 = 1 << SUB_BUCKET_COUNT_MAGNITUDE;
const SUB_BUCKET_HALF_COUNT: i64 = SUB_BUCKET_COUNT >> 1;
const SUB_BUCKET_MASK: i64 = (SUB_BUCKET_COUNT - 1) << UNIT_MAGNITUDE;

fn bit_length(value: i64) -> u32 {
    64 - (value as u64).leading_zeros()
}

/// A value's bucket: HdrHistogram's counts-array index. Below 0 is 0; past
/// an hour is held to it.
pub fn bucket_of(value_ns: i64) -> u32 {
    let value = value_ns.clamp(0, HIGHEST_NS);
    let pow2ceiling = bit_length(value | SUB_BUCKET_MASK) as i64;
    let bucket = pow2ceiling - UNIT_MAGNITUDE as i64 - (SUB_BUCKET_HALF_COUNT_MAGNITUDE as i64 + 1);
    let sub = value >> (bucket + UNIT_MAGNITUDE as i64);
    (((bucket + 1) << SUB_BUCKET_HALF_COUNT_MAGNITUDE) + (sub - SUB_BUCKET_HALF_COUNT)) as u32
}

/// The lowest value a bucket holds.
pub fn value_of(index: u32) -> i64 {
    let index = i64::from(index);
    let mut bucket = (index >> SUB_BUCKET_HALF_COUNT_MAGNITUDE) - 1;
    let mut sub = (index & (SUB_BUCKET_HALF_COUNT - 1)) + SUB_BUCKET_HALF_COUNT;
    if bucket < 0 {
        sub -= SUB_BUCKET_HALF_COUNT;
        bucket = 0;
    }
    sub << (bucket + UNIT_MAGNITUDE as i64)
}

/// One sparse histogram: counts by bucket, the least and most seen, and how
/// many.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Histogram {
    pub counts: BTreeMap<u32, u64>,
    pub min_ns: i64,
    pub max_ns: i64,
    pub total: u64,
}

impl Histogram {
    pub fn record(&mut self, value_ns: i64) {
        *self.counts.entry(bucket_of(value_ns)).or_default() += 1;
        self.min_ns = if self.total == 0 {
            value_ns
        } else {
            self.min_ns.min(value_ns)
        };
        self.max_ns = if self.total == 0 {
            value_ns
        } else {
            self.max_ns.max(value_ns)
        };
        self.total += 1;
    }

    /// Another's counts added, as snapshots merge.
    pub fn add(&mut self, other: &Histogram) {
        if other.total == 0 {
            return;
        }
        for (index, count) in &other.counts {
            *self.counts.entry(*index).or_default() += count;
        }
        self.min_ns = if self.total == 0 {
            other.min_ns
        } else {
            self.min_ns.min(other.min_ns)
        };
        self.max_ns = if self.total == 0 {
            other.max_ns
        } else {
            self.max_ns.max(other.max_ns)
        };
        self.total += other.total;
    }

    /// The value at percentile `p` (0 to 100): the highest value of the
    /// bucket the p-th value falls in, held within the least and the most;
    /// 0 when nothing was recorded -- the SDK's reading, value for value.
    pub fn percentile(&self, p: u64) -> i64 {
        if self.total == 0 {
            return 0;
        }
        let rank = if p == 0 {
            1
        } else {
            (p * self.total).div_ceil(100).max(1)
        };
        let mut seen = 0;
        for (index, count) in &self.counts {
            seen += count;
            if seen >= rank {
                return (value_of(index + 1) - 1).clamp(self.min_ns, self.max_ns);
            }
        }
        self.max_ns
    }
}

/// What a series of histograms is about.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    /// A plugin's session and its metric (`LatencyMetric`'s name).
    Session { session: String, metric: String },
    /// A dataset's age at a hop (`AgeHop`'s name).
    Dataset { dataset: String, hop: String },
}

/// The key a series is kept under: who measured it, and what.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    /// The reporting instance, as its sidecar stamped it; `core` for core's
    /// own hops.
    pub instance: String,
    pub scope: Scope,
}

#[derive(Debug, Default)]
struct Series {
    minutes: BTreeMap<i64, Histogram>,
    hours: BTreeMap<i64, Histogram>,
    alive_at_ns: i64,
    /// The latest counters and ceilings a snapshot carried, as reported.
    backlog_peak: u64,
    last_at_ns: i64,
}

/// One series read over a window: its percentiles, its count, and its last
/// sign of life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Read {
    pub key: Key,
    pub p50_ns: i64,
    pub p95_ns: i64,
    pub p99_ns: i64,
    pub count: u64,
    pub last_at_ns: i64,
    pub alive_at_ns: i64,
    pub backlog_peak: u64,
}

/// The monitor's store.
#[derive(Debug, Default)]
pub struct Monitor {
    held: Mutex<BTreeMap<Key, Series>>,
}

impl Monitor {
    /// A snapshot's histogram merged into its minute, and its minute's hour;
    /// the oldest minute past 7 days and hour past 30 let go.
    pub fn merge(
        &self,
        key: Key,
        at_ns: i64,
        histogram: &Histogram,
        alive_at_ns: i64,
        backlog_peak: u64,
    ) {
        let Ok(mut held) = self.held.lock() else {
            return;
        };
        let series = held.entry(key).or_default();
        let minute = at_ns.div_euclid(MINUTE_NS) * MINUTE_NS;
        let hour = at_ns.div_euclid(HOUR_NS) * HOUR_NS;
        series.minutes.entry(minute).or_default().add(histogram);
        series.hours.entry(hour).or_default().add(histogram);
        series.alive_at_ns = series.alive_at_ns.max(alive_at_ns);
        series.backlog_peak = series.backlog_peak.max(backlog_peak);
        series.last_at_ns = series.last_at_ns.max(at_ns);
        let oldest_minute = minute - MINUTES_KEPT * MINUTE_NS;
        let oldest_hour = hour - HOURS_KEPT * HOUR_NS;
        series.minutes.retain(|m, _| *m > oldest_minute);
        series.hours.retain(|h, _| *h > oldest_hour);
    }

    /// Core's own hop, written directly (570 (ii)): one row's age at a
    /// dataset's hop, now.
    pub fn record_age(&self, dataset: &str, hop: &str, age_ns: i64, now_ns: i64) {
        let mut one = Histogram::default();
        one.record(age_ns);
        self.merge(
            Key {
                instance: "core".into(),
                scope: Scope::Dataset {
                    dataset: dataset.to_string(),
                    hop: hop.to_string(),
                },
            },
            now_ns,
            &one,
            0,
            0,
        );
    }

    /// Every series over the last `window_ns` before `now_ns`, merged: the
    /// Age tab's rows and `dashboard__read_data_age`'s, in key order.
    pub fn read(&self, now_ns: i64, window_ns: i64) -> Vec<Read> {
        let Ok(held) = self.held.lock() else {
            return Vec::new();
        };
        let from = now_ns - window_ns;
        held.iter()
            .filter_map(|(key, series)| {
                let mut merged = Histogram::default();
                for (_, histogram) in series
                    .minutes
                    .range(from.div_euclid(MINUTE_NS) * MINUTE_NS..)
                {
                    merged.add(histogram);
                }
                if merged.total == 0 && series.last_at_ns < from {
                    return None;
                }
                Some(Read {
                    key: key.clone(),
                    p50_ns: merged.percentile(50),
                    p95_ns: merged.percentile(95),
                    p99_ns: merged.percentile(99),
                    count: merged.total,
                    last_at_ns: series.last_at_ns,
                    alive_at_ns: series.alive_at_ns,
                    backlog_peak: series.backlog_peak,
                })
            })
            .collect()
    }
}

/// The process's monitor: the dashboard's handler of `ReportLatency` and
/// core's hops in the same process write the one store.
pub fn monitor() -> &'static Monitor {
    static MONITOR: OnceLock<Monitor> = OnceLock::new();
    MONITOR.get_or_init(Monitor::default)
}

/// A row's source time (the latency spec, "The hops"): its `event` source
/// time if present, else the earliest stated, else its valid time.
pub fn source_ns(meta: &crate::v1::ObservationMeta) -> i64 {
    meta.source_times
        .iter()
        .find(|t| t.kind == crate::v1::SourceTimeKind::Event as i32)
        .map(|t| t.at_ns)
        .or_else(|| meta.source_times.iter().map(|t| t.at_ns).min())
        .unwrap_or(meta.valid_from_ns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_buckets_are_the_sdks() {
        // The SDK's bucket_of and value_of, value for value.
        for (value, bucket) in [
            (0, 0),
            (1_000, 1),
            (127_999, 249),
            (128_000, 250),
            (1_000_000, 628),
            (HIGHEST_NS, 3_409),
        ] {
            assert_eq!(bucket_of(value), bucket, "{value}");
        }
        assert_eq!(value_of(381), 259_072);
        for index in [0_u32, 127, 128, 381, 1_000] {
            assert!(bucket_of(value_of(index)) == index, "{index}");
        }
        assert_eq!(bucket_of(HIGHEST_NS * 2), bucket_of(HIGHEST_NS));
    }

    #[test]
    fn snapshots_merge_by_adding_counts_and_read_their_percentiles() {
        let monitor = Monitor::default();
        let key = Key {
            instance: "oanda-1".into(),
            scope: Scope::Session {
                session: "pricing-stream".into(),
                metric: "LATENCY_METRIC_RTT".into(),
            },
        };
        let now = 1_792_300_000_000_000_000;
        let mut first = Histogram::default();
        for ms in 1..=90 {
            first.record(ms * 1_000_000);
        }
        let mut second = Histogram::default();
        for ms in 91..=100 {
            second.record(ms * 1_000_000);
        }
        monitor.merge(key.clone(), now - 20_000_000_000, &first, now - 1, 3);
        monitor.merge(key.clone(), now - 10_000_000_000, &second, now, 1);
        monitor.record_age("tradier-1:options", "AGE_HOP_LAKE", 2_500_000, now);
        let read = monitor.read(now, 60_000_000_000);
        assert_eq!(read.len(), 2);
        let rtt = read.iter().find(|r| r.key == key).unwrap();
        assert_eq!(rtt.count, 100);
        assert!(
            (49_000_000..=51_000_000).contains(&rtt.p50_ns),
            "{}",
            rtt.p50_ns
        );
        assert!(
            (98_000_000..=100_000_000).contains(&rtt.p99_ns),
            "{}",
            rtt.p99_ns
        );
        assert_eq!(rtt.backlog_peak, 3);
        let lake = read.iter().find(|r| r.key.instance == "core").unwrap();
        assert_eq!(lake.count, 1);
        // Outside the window, nothing.
        assert!(monitor
            .read(now + 3_600_000_000_000, 60_000_000_000)
            .is_empty());
    }
}
