//! Reading the lake: point in time, by source (W10.6), and what it could
//! not answer (W10.7).
//!
//! A read names its subjects, a valid time -- the latest in force, a business
//! date, or a range -- a recorded-time cut-off, and which datasets answer:
//! the deployment's default (its priority for the data type and kind, with
//! failover), named datasets in order, or every entitled dataset side by
//! side. No read silently mixes sources: a default read names, for each
//! dataset above the one that answered, why it did not (the intent's Q4).
//! A dataset the reader is not entitled to is refused per dataset, and a
//! field it may not read is stripped and named (W10.1).
//!
//! What no kept row answers becomes a want of the instance serving the
//! dataset where its catalogue says it can be asked, and the reader is told
//! it was asked; otherwise why not: declined by its source, older than its
//! history, not kept, or not covered.

use std::collections::{BTreeMap, BTreeSet};

use meridian_domain::date::Date;
use meridian_domain::v1::{SourceChoice, SourcePriority, UnansweredReason};
use meridian_pb::v1::ObservationMode;

use crate::config::{DataConfig, Reader};
use crate::row::{DataType, Observation};
use crate::store::{Query, Store, When};

const NS_PER_DAY: i64 = 86_400 * 1_000_000_000;

/// One part of a read that was not served, and why.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NotServed {
    pub subject: String,
    pub dataset: String,
    pub field: String,
    pub reason: UnansweredReason,
}

/// What a read asks of a dataset's instance: the subjects no kept row
/// answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub dataset: String,
    pub data_type: DataType,
    pub subjects: Vec<String>,
    pub kinds: Vec<i32>,
    pub interval_ns: i64,
    pub when: When,
    /// Keep the subjects current: a read of the latest, of a dataset with a
    /// cadence.
    pub standing: bool,
}

/// A read, answered.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Answered {
    pub rows: Vec<Observation>,
    pub not_served: Vec<NotServed>,
    pub asks: Vec<Ask>,
}

/// What a read is, once its request is read.
#[derive(Debug, Clone)]
pub struct Read {
    pub reader: Reader,
    pub data_type: DataType,
    pub subjects: Vec<String>,
    pub kinds: Vec<i32>,
    pub interval_ns: i64,
    pub sources: SourceChoice,
    pub when: When,
    pub as_of_ns: i64,
}

/// What a read is answered against besides the store.
pub struct Against<'a> {
    pub config: &'a DataConfig,
    pub priorities: &'a [SourcePriority],
    /// What a source declined, by dataset and subject: its reason and when.
    pub declined: &'a BTreeMap<(String, String), (UnansweredReason, i64)>,
    /// Each replaced record and the one that stays (W10.8).
    pub aliases: &'a BTreeMap<String, String>,
    pub now_ns: i64,
}

/// How long a source's decline answers for it before it is asked again.
pub const DECLINE_STANDS_NS: i64 = NS_PER_DAY;

fn order_for(read: &Read, against: &Against<'_>, kind: i32, readable: &[String]) -> Vec<String> {
    if !read.sources.named.is_empty() {
        return read
            .sources
            .named
            .iter()
            .filter(|d| readable.contains(d))
            .cloned()
            .collect();
    }
    let set = against
        .priorities
        .iter()
        .find(|p| p.data_type == read.data_type.name() && p.kind == kind)
        .filter(|p| !p.datasets.is_empty());
    match set {
        Some(priority) => priority
            .datasets
            .iter()
            .filter(|d| readable.contains(d))
            .cloned()
            .collect(),
        None => readable.to_vec(),
    }
}

/// Whether a dataset's latest row for a subject is older than its cadence:
/// silent (W10.2).
fn silent(row: &Observation, against: &Against<'_>) -> bool {
    let cadence = against
        .config
        .declaration(row.dataset())
        .map_or(0, |d| i64::from(d.cadence));
    cadence > 0 && row.meta().recorded_at_ns < against.now_ns - cadence * 1_000_000_000
}

/// Why a dataset holds nothing for a subject, and whether to ask it.
fn why_not(
    read: &Read,
    against: &Against<'_>,
    dataset: &str,
    subject: &str,
) -> (UnansweredReason, bool) {
    let config = against.config;
    if let Some((reason, at)) = against
        .declined
        .get(&(dataset.to_string(), subject.to_string()))
    {
        if against.now_ns - at < DECLINE_STANDS_NS {
            return (*reason, false);
        }
    }
    if let When::BusinessDate(date) = &read.when {
        let history = config.declaration(dataset).map_or(0, |d| d.history);
        let today = Date::of_utc_instant(against.now_ns).days_since_epoch();
        if history > 0 && date.days_since_epoch() < today - i64::from(history) {
            return (UnansweredReason::BeyondHistory, false);
        }
    }
    let latest = matches!(read.when, When::Latest { at_ns: 0 });
    let askable = config.has_mode(dataset, ObservationMode::Pull)
        || (latest && config.has_mode(dataset, ObservationMode::Stream));
    if askable {
        return (UnansweredReason::AskedSource, true);
    }
    if !config.licence(dataset).kept {
        return (UnansweredReason::NotKept, false);
    }
    (UnansweredReason::NotCovered, false)
}

/// The latest in force per subject, dataset, kind, venue and length: what a
/// read of the latest at a valid time answers.
fn latest_in_force(rows: Vec<Observation>) -> Vec<Observation> {
    let mut chosen: BTreeMap<(String, String, i32, String, i64), Observation> = BTreeMap::new();
    for row in rows {
        let key = (
            row.subject().to_string(),
            row.dataset().to_string(),
            row.kind(),
            row.venue().to_string(),
            row.interval_ns(),
        );
        let later = |held: &Observation| {
            (row.meta().valid_from_ns, row.meta().sequence)
                > (held.meta().valid_from_ns, held.meta().sequence)
        };
        match chosen.get(&key) {
            Some(held) if !later(held) => {}
            _ => {
                chosen.insert(key, row);
            }
        }
    }
    chosen.into_values().collect()
}

/// Answer a read from the store, as the reader may read it.
pub fn answer(store: &dyn Store, read: &Read, against: &Against<'_>) -> Result<Answered, String> {
    let config = against.config;
    let mut out = Answered::default();
    let readable = config.readable(&read.reader, read.data_type);

    // Named datasets the reader may not read, or that are no dataset.
    for named in &read.sources.named {
        if readable.contains(named) {
            continue;
        }
        let reason = if config.datasets.contains_key(named) && config.serves(named, read.data_type)
        {
            UnansweredReason::NotEntitled
        } else {
            UnansweredReason::NotCovered
        };
        out.not_served.push(NotServed {
            subject: String::new(),
            dataset: named.clone(),
            field: String::new(),
            reason,
        });
    }

    let candidates: Vec<String> = if read.sources.named.is_empty() {
        readable.clone()
    } else {
        read.sources
            .named
            .iter()
            .filter(|d| readable.contains(d))
            .cloned()
            .collect()
    };
    if candidates.is_empty() {
        if readable.is_empty() && read.sources.named.is_empty() {
            for subject in &read.subjects {
                out.not_served.push(NotServed {
                    subject: subject.clone(),
                    dataset: String::new(),
                    field: String::new(),
                    reason: if config
                        .datasets
                        .keys()
                        .any(|d| config.serves(d, read.data_type))
                    {
                        UnansweredReason::NotEntitled
                    } else {
                        UnansweredReason::NotCovered
                    },
                });
            }
        }
        return Ok(out);
    }

    // The subjects, and the records each replaced (W10.8).
    let mut asked: Vec<String> = read.subjects.clone();
    for (replaced, stays) in against.aliases {
        if read.subjects.contains(stays) && !asked.contains(replaced) {
            asked.push(replaced.clone());
        }
    }
    let alias_of = |subject: &str| -> String {
        against
            .aliases
            .get(subject)
            .filter(|stays| read.subjects.contains(stays))
            .cloned()
            .unwrap_or_else(|| subject.to_string())
    };

    let mut rows = store
        .read(&Query {
            datasets: candidates.clone(),
            subjects: asked,
            data_type: read.data_type,
            kinds: read.kinds.clone(),
            interval_ns: read.interval_ns,
            when: read.when.clone(),
            as_of_ns: read.as_of_ns,
        })
        .map_err(|failed| failed.to_string())?;
    if matches!(read.when, When::Latest { .. }) {
        rows = latest_in_force(rows);
    }

    // By the subject asked about, then kind.
    let mut by_subject: BTreeMap<(String, i32), Vec<Observation>> = BTreeMap::new();
    for row in rows {
        by_subject
            .entry((alias_of(row.subject()), row.kind()))
            .or_default()
            .push(row);
    }
    let kinds_asked: Vec<i32> = if read.kinds.is_empty() {
        vec![]
    } else {
        read.kinds.clone()
    };

    let mut asks: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut chosen: Vec<Observation> = Vec::new();
    for subject in &read.subjects {
        // The kinds to answer: those asked, or those any dataset holds; a
        // bar's is 0.
        let mut kinds: Vec<i32> = kinds_asked.clone();
        if kinds.is_empty() {
            kinds = by_subject
                .keys()
                .filter(|(s, _)| s == subject)
                .map(|(_, k)| *k)
                .collect();
        }
        let nothing_held = kinds.is_empty();
        if nothing_held {
            kinds.push(0);
        }
        for kind in kinds {
            let held = by_subject
                .get(&(subject.clone(), kind))
                .cloned()
                .unwrap_or_default();
            let order = if read.sources.side_by_side {
                candidates.clone()
            } else {
                order_for(read, against, kind, &candidates)
            };
            if read.sources.side_by_side {
                for dataset in &order {
                    let of: Vec<&Observation> =
                        held.iter().filter(|r| r.dataset() == dataset).collect();
                    if of.is_empty() {
                        let (reason, ask) = why_not(read, against, dataset, subject);
                        if ask {
                            asks.entry(dataset.clone())
                                .or_default()
                                .insert(subject.clone());
                        }
                        out.not_served.push(NotServed {
                            subject: subject.clone(),
                            dataset: dataset.clone(),
                            field: String::new(),
                            reason,
                        });
                    } else {
                        chosen.extend(of.into_iter().cloned());
                    }
                }
                continue;
            }
            // Default or named: the first dataset in order with a row that is
            // not silent; each above it said, with why.
            let mut fallen: Vec<NotServed> = Vec::new();
            let mut answered = false;
            let mut first_silent: Option<Vec<Observation>> = None;
            let mut asked_one = false;
            for dataset in &order {
                let of: Vec<Observation> = held
                    .iter()
                    .filter(|r| r.dataset() == dataset)
                    .cloned()
                    .collect();
                if of.is_empty() {
                    let (reason, ask) = why_not(read, against, dataset, subject);
                    let reason = if ask && asked_one {
                        // One instance is asked at a time: the first that can be.
                        UnansweredReason::NotCovered
                    } else {
                        reason
                    };
                    if ask && !asked_one {
                        asks.entry(dataset.clone())
                            .or_default()
                            .insert(subject.clone());
                        asked_one = true;
                    }
                    fallen.push(NotServed {
                        subject: subject.clone(),
                        dataset: dataset.clone(),
                        field: String::new(),
                        reason,
                    });
                    continue;
                }
                if matches!(read.when, When::Latest { .. }) && of.iter().all(|r| silent(r, against))
                {
                    fallen.push(NotServed {
                        subject: subject.clone(),
                        dataset: dataset.clone(),
                        field: String::new(),
                        reason: UnansweredReason::SourceSilent,
                    });
                    first_silent.get_or_insert(of);
                    continue;
                }
                chosen.extend(of);
                answered = true;
                break;
            }
            if !answered {
                if let Some(of) = first_silent {
                    chosen.extend(of);
                }
            }
            out.not_served.extend(fallen);
        }
    }

    // Fields the reader may not read, stripped and named once per dataset.
    let mut named_fields: BTreeSet<(String, String)> = BTreeSet::new();
    for row in chosen.iter_mut() {
        let dataset = row.dataset().to_string();
        let allowed = config
            .fields_for(&dataset, &read.reader)
            .unwrap_or_default();
        for field in row.strip(&allowed) {
            if named_fields.insert((dataset.clone(), field.to_string())) {
                out.not_served.push(NotServed {
                    subject: String::new(),
                    dataset: dataset.clone(),
                    field: field.to_string(),
                    reason: UnansweredReason::NotEntitled,
                });
            }
        }
    }
    chosen.sort_by(|a, b| {
        (
            a.subject(),
            a.dataset(),
            a.kind(),
            a.meta().valid_from_ns,
            a.meta().sequence,
        )
            .cmp(&(
                b.subject(),
                b.dataset(),
                b.kind(),
                b.meta().valid_from_ns,
                b.meta().sequence,
            ))
    });
    out.rows = chosen;

    let standing = matches!(read.when, When::Latest { at_ns: 0 });
    for (dataset, subjects) in asks {
        let cadence = config.declaration(&dataset).map_or(0, |d| d.cadence);
        out.asks.push(Ask {
            dataset,
            data_type: read.data_type,
            subjects: subjects.into_iter().collect(),
            kinds: read.kinds.clone(),
            interval_ns: read.interval_ns,
            when: read.when.clone(),
            standing: standing && cadence > 0,
        });
    }
    Ok(out)
}
