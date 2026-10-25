//! Core's latency monitor (contract v28's latency remainder; the latency
//! spec's "What reaches core" and "The hops"; rulings 515 to 518 and 570
//! (ii)): log-linear histograms at 2 significant digits from 1 µs to 1 hour,
//! HdrHistogram's layout as the SDK's recorder writes them, merged by adding
//! counts -- per minute for 7 days, per hour for 30 -- keyed by the reporting
//! instance and the session and metric, or the dataset and hop.
//!
//! One writer, one store: `ReportLatency` snapshots, a plugin's across its
//! sidecar and, from the third amendment (ruling 606, reversing 570 (ii)),
//! core's own components' -- the sidecar's `sidecar` and `bus` hops, the
//! lake's `lake` hop, the analytics component's `analytics` hop -- each
//! recorded in its own process by a [`Recorder`] and sent on the same topic
//! every 10 seconds. Readers: the Data sources page's Age tab and
//! `dashboard__read_data_age`, the Summary's latency part, and the judge
//! ([`Monitor::judge`]) whose episodes become notices. Measurements, not
//! records: nothing here is journalled, and a restart begins the window
//! again (the latency spec; "Stateless compute, owned storage").

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
    /// The reporting instance, as its sidecar stamped it -- a plugin's own
    /// snapshots and its sidecar's hops alike -- or, for the lake's and the
    /// analytics component's own hops, the component that sent them (606).
    pub instance: String,
    pub scope: Scope,
}

#[derive(Debug, Default)]
struct Series {
    minutes: BTreeMap<i64, Histogram>,
    hours: BTreeMap<i64, Histogram>,
    /// Each minute's backlog peak, as its snapshots reported it.
    backlog_minutes: BTreeMap<i64, u64>,
    alive_at_ns: i64,
    /// The latest counters and ceilings a snapshot carried, as reported.
    backlog_peak: u64,
    last_at_ns: i64,
    /// The ceiling in force for this series' metric, as the latest snapshot
    /// carried it (ruling 415: it travels with the snapshot); 0 for none.
    ceiling_ns: i64,
    /// The backlog's ceiling, likewise; 0 for none.
    backlog_max: u64,
    /// The judge's place: every minute before it has been judged.
    judged_to: i64,
    /// In-session minutes in a row over, and clear, since the last change.
    over: u32,
    clear: u32,
    /// The open episodes, by the minute each opened; 0 for none.
    latency_since: i64,
    backlog_since: i64,
}

/// What one snapshot says beside its histogram.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Said {
    pub alive_at_ns: i64,
    pub backlog_peak: u64,
    /// The ceiling for its metric or hop, 0 for none.
    pub ceiling_ns: i64,
    pub backlog_max: u64,
}

/// An episode opening or closing (ruling 427): one notice for each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub key: Key,
    /// `latency_breached`, `latency_recovered`, `backlog_breached` or
    /// `backlog_recovered`.
    pub kind: &'static str,
    /// The minute judged.
    pub at_ns: i64,
    /// What the minute held: its p99, or its backlog peak.
    pub value: i64,
    /// The ceiling it was judged against; 0 where the breach was relative.
    pub ceiling: i64,
}

/// In-session minutes in a row over the ceiling that open an episode, and
/// clear that close one (the latency spec, "Bands, breaches and notices").
pub const MINUTES_IN_A_ROW: u32 = 3;
/// The relative rule's baseline: at least this many in-session minutes of
/// the trailing 7 days, and 3 standard deviations above their mean.
pub const BASELINE_LEAST: usize = 20;
pub const SIGMAS: f64 = 3.0;

/// One series read over a window: its percentiles, its count, its last
/// sign of life, and its open episodes.
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
    pub ceiling_ns: i64,
    /// The minute a latency or a backlog episode opened; 0 for none open.
    pub latency_since_ns: i64,
    pub backlog_since_ns: i64,
}

/// The monitor's store.
#[derive(Debug, Default)]
pub struct Monitor {
    held: Mutex<BTreeMap<Key, Series>>,
}

impl Monitor {
    /// A snapshot's histogram merged into its minute, and its minute's hour;
    /// the oldest minute past 7 days and hour past 30 let go.
    pub fn merge(&self, key: Key, at_ns: i64, histogram: &Histogram, said: Said) {
        let Ok(mut held) = self.held.lock() else {
            return;
        };
        let series = held.entry(key).or_default();
        let minute = at_ns.div_euclid(MINUTE_NS) * MINUTE_NS;
        let hour = at_ns.div_euclid(HOUR_NS) * HOUR_NS;
        series.minutes.entry(minute).or_default().add(histogram);
        series.hours.entry(hour).or_default().add(histogram);
        let peak = series.backlog_minutes.entry(minute).or_default();
        *peak = (*peak).max(said.backlog_peak);
        series.alive_at_ns = series.alive_at_ns.max(said.alive_at_ns);
        series.backlog_peak = series.backlog_peak.max(said.backlog_peak);
        series.last_at_ns = series.last_at_ns.max(at_ns);
        series.ceiling_ns = said.ceiling_ns;
        series.backlog_max = said.backlog_max;
        let oldest_minute = minute - MINUTES_KEPT * MINUTE_NS;
        let oldest_hour = hour - HOURS_KEPT * HOUR_NS;
        series.minutes.retain(|m, _| *m > oldest_minute);
        series.backlog_minutes.retain(|m, _| *m > oldest_minute);
        series.hours.retain(|h, _| *h > oldest_hour);
    }

    /// Every whole minute not yet judged, judged (the latency spec, "Bands,
    /// breaches and notices"; rulings 415 and 427): a minute is in session
    /// when it holds measurements, and a minute with none neither breaks
    /// nor clears a run. **Over** is its p99 above the ceiling that travelled
    /// with the snapshot (absolute), or 3 standard deviations above the mean
    /// of the series' own in-session minutes of the trailing 7 days, at
    /// least 20 of them (relative). 3 minutes over in a row open a latency
    /// episode and 3 clear in a row close it; a minute whose backlog peak
    /// passed the backlog's ceiling opens a backlog episode and the first
    /// back under it closes it. Each opening and closing is answered once.
    pub fn judge(&self, now_ns: i64) -> Vec<Transition> {
        let Ok(mut held) = self.held.lock() else {
            return Vec::new();
        };
        let current = now_ns.div_euclid(MINUTE_NS) * MINUTE_NS;
        let mut out = Vec::new();
        for (key, series) in held.iter_mut() {
            let minutes: Vec<i64> = series
                .minutes
                .range(series.judged_to..current)
                .map(|(m, _)| *m)
                .collect();
            for minute in minutes {
                judge_minute(key, series, minute, &mut out);
            }
            series.judged_to = series.judged_to.max(current);
        }
        out
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
                // Quiet in the window and no episode open: nothing to say.
                // An open episode stays read until it closes.
                let open = series.latency_since != 0 || series.backlog_since != 0;
                if merged.total == 0 && series.last_at_ns < from && !open {
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
                    ceiling_ns: series.ceiling_ns,
                    latency_since_ns: series.latency_since,
                    backlog_since_ns: series.backlog_since,
                })
            })
            .collect()
    }
}

/// One minute of one series judged, its transitions pushed.
fn judge_minute(key: &Key, series: &mut Series, minute: i64, out: &mut Vec<Transition>) {
    let Some(histogram) = series.minutes.get(&minute) else {
        return;
    };
    if histogram.total == 0 {
        return;
    }
    let p99 = histogram.percentile(99);
    let absolute = series.ceiling_ns > 0 && p99 > series.ceiling_ns;
    let baseline: Vec<f64> = series
        .minutes
        .range(minute - MINUTES_KEPT * MINUTE_NS..minute)
        .filter(|(_, h)| h.total > 0)
        .map(|(_, h)| h.percentile(99) as f64)
        .collect();
    let relative = baseline.len() >= BASELINE_LEAST && {
        let n = baseline.len() as f64;
        let mean = baseline.iter().sum::<f64>() / n;
        let sd = (baseline.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n).sqrt();
        // No spread means no sigma (check_bands' rule).
        sd > 0.0 && (p99 as f64 - mean) / sd >= SIGMAS
    };
    let transition = |kind, value, ceiling| Transition {
        key: key.clone(),
        kind,
        at_ns: minute,
        value,
        ceiling,
    };
    if absolute || relative {
        series.over += 1;
        series.clear = 0;
        if series.latency_since == 0 && series.over >= MINUTES_IN_A_ROW {
            series.latency_since = minute;
            out.push(transition(
                "latency_breached",
                p99,
                if absolute { series.ceiling_ns } else { 0 },
            ));
        }
    } else {
        series.clear += 1;
        series.over = 0;
        if series.latency_since != 0 && series.clear >= MINUTES_IN_A_ROW {
            series.latency_since = 0;
            out.push(transition("latency_recovered", p99, series.ceiling_ns));
        }
    }
    let peak = series.backlog_minutes.get(&minute).copied().unwrap_or(0);
    if series.backlog_max > 0 {
        let backlog_max = series.backlog_max as i64;
        if peak > series.backlog_max && series.backlog_since == 0 {
            series.backlog_since = minute;
            out.push(transition("backlog_breached", peak as i64, backlog_max));
        } else if peak <= series.backlog_max && series.backlog_since != 0 {
            series.backlog_since = 0;
            out.push(transition("backlog_recovered", peak as i64, backlog_max));
        }
    }
}

/// The process's monitor: the dashboard's handler of `ReportLatency` writes
/// it, and its readers read it.
pub fn monitor() -> &'static Monitor {
    static MONITOR: OnceLock<Monitor> = OnceLock::new();
    MONITOR.get_or_init(Monitor::default)
}

/// What a core component measures of its own hops (ruling 606): each row's
/// age at a dataset's hop, kept per dataset and hop until the component's
/// next report takes it, every value recorded, never sampled (ruling 416).
#[derive(Debug, Default)]
pub struct Recorder {
    held: Mutex<RecorderHeld>,
}

#[derive(Debug, Default)]
struct RecorderHeld {
    since_ns: i64,
    ages: BTreeMap<(String, crate::v1::AgeHop), Histogram>,
}

impl Recorder {
    /// One row's age at a hop: the hop's reading of the one clock minus the
    /// row's source time ([`source_ns`]).
    pub fn record(&self, dataset: &str, hop: crate::v1::AgeHop, age_ns: i64) {
        if dataset.is_empty() {
            return;
        }
        let Ok(mut held) = self.held.lock() else {
            return;
        };
        held.ages
            .entry((dataset.to_string(), hop))
            .or_default()
            .record(age_ns.max(0));
    }

    /// What was recorded since the last take, as the snapshots the
    /// component sends on `report-latency`: per dataset and hop, an age
    /// histogram over the interval, sparse; none when nothing was recorded.
    /// No instance: the sidecar stamps its plugin's, and the lake's and the
    /// analytics component's are no plugin's (606).
    pub fn take(&self, now_ns: i64) -> Vec<crate::v1::LatencySnapshot> {
        let Ok(mut held) = self.held.lock() else {
            return Vec::new();
        };
        let since = if held.since_ns > 0 && held.since_ns < now_ns {
            held.since_ns
        } else {
            now_ns - REPORT_EVERY_NS
        };
        held.since_ns = now_ns;
        std::mem::take(&mut held.ages)
            .into_iter()
            .filter(|(_, h)| h.total > 0)
            .map(|((dataset, hop), h)| crate::v1::LatencySnapshot {
                interval_start_ns: since,
                interval_ns: now_ns - since,
                dataset,
                hop: hop as i32,
                metric: crate::v1::LatencyMetric::Age as i32,
                histogram: Some(crate::v1::LatencyHistogram {
                    lowest_ns: LOWEST_NS,
                    significant_digits: SIGNIFICANT_DIGITS,
                    buckets: h
                        .counts
                        .iter()
                        .map(|(bucket, count)| crate::v1::LatencyBucket {
                            bucket: *bucket,
                            count: *count,
                        })
                        .collect(),
                    min_ns: h.min_ns,
                    max_ns: h.max_ns,
                    total: h.total,
                }),
                ..Default::default()
            })
            .collect()
    }
}

/// How often a component sends what it recorded (ruling 416).
pub const REPORT_EVERY_NS: i64 = 10 * 1_000_000_000;

/// The process's recorder: one per component process.
pub fn recorder() -> &'static Recorder {
    static RECORDER: OnceLock<Recorder> = OnceLock::new();
    RECORDER.get_or_init(Recorder::default)
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
        let said = |alive, backlog| Said {
            alive_at_ns: alive,
            backlog_peak: backlog,
            ..Said::default()
        };
        monitor.merge(key.clone(), now - 20_000_000_000, &first, said(now - 1, 3));
        monitor.merge(key.clone(), now - 10_000_000_000, &second, said(now, 1));
        // The lake's own hop, as its snapshot reaches the monitor (606).
        let mut lake_age = Histogram::default();
        lake_age.record(2_500_000);
        monitor.merge(
            Key {
                instance: "lake".into(),
                scope: Scope::Dataset {
                    dataset: "tradier-1:options".into(),
                    hop: "AGE_HOP_LAKE".into(),
                },
            },
            now,
            &lake_age,
            Said::default(),
        );
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
        let lake = read.iter().find(|r| r.key.instance == "lake").unwrap();
        assert_eq!(lake.count, 1);
        // Outside the window, nothing.
        assert!(monitor
            .read(now + 3_600_000_000_000, 60_000_000_000)
            .is_empty());
    }

    fn rtt_key() -> Key {
        Key {
            instance: "oanda-1".into(),
            scope: Scope::Session {
                session: "pricing-stream".into(),
                metric: "LATENCY_METRIC_RTT".into(),
            },
        }
    }

    fn minute_of(monitor: &Monitor, at: i64, ms: i64, said: Said) {
        let mut h = Histogram::default();
        for _ in 0..10 {
            h.record(ms * 1_000_000);
        }
        monitor.merge(rtt_key(), at, &h, said);
    }

    #[test]
    fn three_minutes_over_the_ceiling_open_an_episode_and_three_clear_close_it() {
        let monitor = Monitor::default();
        let start = 1_792_300_000_020_000_000 / MINUTE_NS * MINUTE_NS;
        let said = Said {
            ceiling_ns: 250_000_000,
            ..Said::default()
        };
        // Two over, one clear, then three over: one episode, opened at the
        // third over in a row.
        for (i, ms) in [2_000, 2_000, 10, 2_000, 2_000, 2_000, 10, 10, 10]
            .iter()
            .enumerate()
        {
            minute_of(&monitor, start + i as i64 * MINUTE_NS + 1, *ms, said);
        }
        let judged = monitor.judge(start + 9 * MINUTE_NS);
        let kinds: Vec<(&str, i64)> = judged
            .iter()
            .map(|t| (t.kind, (t.at_ns - start) / MINUTE_NS))
            .collect();
        assert_eq!(
            kinds,
            vec![("latency_breached", 5), ("latency_recovered", 8)]
        );
        assert_eq!(judged[0].ceiling, 250_000_000);
        assert!(judged[0].value > 250_000_000);
        // Judged once: nothing again.
        assert!(monitor.judge(start + 9 * MINUTE_NS).is_empty());
    }

    #[test]
    fn a_minute_with_nothing_measured_neither_breaks_nor_clears_a_run() {
        let monitor = Monitor::default();
        let start = 1_792_300_000_000_000_000 / MINUTE_NS * MINUTE_NS;
        let said = Said {
            ceiling_ns: 250_000_000,
            ..Said::default()
        };
        // Over at 0 and 1, nothing at 2 and 3 (out of session), over at 4.
        for i in [0_i64, 1, 4] {
            minute_of(&monitor, start + i * MINUTE_NS, 2_000, said);
        }
        let judged = monitor.judge(start + 5 * MINUTE_NS);
        assert_eq!(judged.len(), 1);
        assert_eq!(judged[0].at_ns, start + 4 * MINUTE_NS);
    }

    #[test]
    fn a_session_far_above_its_own_week_breaches_with_no_ceiling() {
        let monitor = Monitor::default();
        let start = 1_792_300_000_000_000_000 / MINUTE_NS * MINUTE_NS;
        for i in 0..30_i64 {
            // A steady 9 to 11 ms.
            minute_of(&monitor, start + i * MINUTE_NS, 9 + i % 3, Said::default());
        }
        for i in 30..33_i64 {
            minute_of(&monitor, start + i * MINUTE_NS, 400, Said::default());
        }
        let judged = monitor.judge(start + 33 * MINUTE_NS);
        assert_eq!(judged.len(), 1);
        assert_eq!(judged[0].kind, "latency_breached");
        assert_eq!(judged[0].ceiling, 0, "relative, against its own week");
    }

    #[test]
    fn a_backlog_past_its_ceiling_opens_and_the_first_minute_under_closes() {
        let monitor = Monitor::default();
        let start = 1_792_300_000_000_000_000 / MINUTE_NS * MINUTE_NS;
        for (i, peak) in [10_u64, 1_500, 1_200, 40].iter().enumerate() {
            minute_of(
                &monitor,
                start + i as i64 * MINUTE_NS,
                10,
                Said {
                    backlog_peak: *peak,
                    backlog_max: 1_000,
                    ..Said::default()
                },
            );
        }
        let kinds: Vec<&str> = monitor
            .judge(start + 4 * MINUTE_NS)
            .iter()
            .map(|t| t.kind)
            .collect();
        assert_eq!(kinds, vec!["backlog_breached", "backlog_recovered"]);
    }

    #[test]
    fn a_components_recorder_answers_its_hops_as_snapshots_once() {
        let recorder = Recorder::default();
        let now = 1_792_300_000_000_000_000;
        recorder.record("tradier-1:quotes", crate::v1::AgeHop::Lake, 3_000_000);
        recorder.record("tradier-1:quotes", crate::v1::AgeHop::Lake, 5_000_000);
        recorder.record("", crate::v1::AgeHop::Lake, 5_000_000);
        let taken = recorder.take(now);
        assert_eq!(taken.len(), 1);
        let snapshot = &taken[0];
        assert_eq!(snapshot.hop, crate::v1::AgeHop::Lake as i32);
        assert_eq!(snapshot.metric, crate::v1::LatencyMetric::Age as i32);
        assert!(snapshot.instance_id.is_empty(), "no instance (606)");
        assert_eq!(snapshot.interval_start_ns + snapshot.interval_ns, now);
        let histogram = snapshot.histogram.as_ref().unwrap();
        assert_eq!(histogram.total, 2);
        assert_eq!(
            histogram.buckets.iter().map(|b| b.count).sum::<u64>(),
            histogram.total
        );
        assert!(recorder.take(now + REPORT_EVERY_NS).is_empty());
    }
}
