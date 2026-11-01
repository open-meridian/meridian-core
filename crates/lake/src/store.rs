//! What the lake's store keeps, and what any store of it must provide.
//!
//! One writer per dataset partition: a batch takes its partition's head and
//! numbers its rows from it in its own transaction, so the numbers have no
//! holes. A row is never updated: a changed value under a row key held is a
//! new version, and an identical one changes nothing.

use std::collections::BTreeMap;

use meridian_domain::date::Date;
use meridian_domain::v1::{
    DateCheck, DateCheckReason, EntitlementsChangedEvent, ObservationsWantedEvent, QualityState,
    SourcePriority,
};

use crate::row::{DataType, Observation};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("the lake's store is unavailable: {0}")]
    Unavailable(String),

    #[error("{0}")]
    SchemaAhead(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Which valid time a read asks about (W10.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum When {
    /// The latest in force at a valid time; 0 for now.
    Latest { at_ns: i64 },
    /// A business date's.
    BusinessDate(Date),
    /// A valid-time range, from inclusive to until exclusive.
    Range { from_ns: i64, until_ns: i64 },
    /// The rows each dataset partition named recorded after its sequence
    /// there, whatever their valid time (contract v19, W10.6; the plan's
    /// Q33): how a reader that lost trades catches up. A partition not named
    /// answers nothing.
    After(BTreeMap<String, u64>),
}

/// What a store reads: the rows of these datasets about these subjects, as
/// of a recorded time, each row key at its version in force then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub datasets: Vec<String>,
    pub subjects: Vec<String>,
    pub data_type: DataType,
    /// Prices' kinds; empty for every kind.
    pub kinds: Vec<i32>,
    /// Bars' length; 0 for every length.
    pub interval_ns: i64,
    pub when: When,
    /// The recorded-time cut-off.
    pub as_of_ns: i64,
    /// A rate's published-time cut-off (contract v27, W10.6); 0 for none.
    /// Given, the store answers every version recorded by `as_of_ns`, and
    /// the read chooses the version in force by published time.
    pub published_as_of_ns: i64,
    /// A read of the latest that narrows its rows no further before the
    /// latest is chosen: the store may answer, of the rows it would, only
    /// each one's latest per subject, dataset, kind, venue, length, asset
    /// and point, and the latest of those not invalid, as the read chooses
    /// them ([`crate::read`]). A store may answer every row still.
    pub latest_only: bool,
}

/// What a batch did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Recorded {
    /// The rows it recorded, new and restated, as recorded: their envelopes
    /// filled. An unchanged row is not among them; a rate's confirmation,
    /// counted unchanged, is (contract v27).
    pub rows: Vec<Observation>,
    pub recorded: u32,
    pub restated: u32,
    pub unchanged: u32,
    /// The partition's head once the batch committed.
    pub head: u64,
}

/// A change to a want, its own record (decisions/031).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum WantChangeKind {
    /// Asked of the instance serving the dataset.
    Asked,
    /// Rows recorded for it, by the instance.
    Answered,
    /// Declined per subject, by the instance, with its reason.
    Declined,
    /// A standing want no reader asked within the cadence.
    Withdrawn,
}

impl WantChangeKind {
    pub fn code(&self) -> i16 {
        match self {
            WantChangeKind::Asked => 1,
            WantChangeKind::Answered => 2,
            WantChangeKind::Declined => 3,
            WantChangeKind::Withdrawn => 4,
        }
    }

    pub fn from_code(code: i16) -> Option<Self> {
        match code {
            1 => Some(WantChangeKind::Asked),
            2 => Some(WantChangeKind::Answered),
            3 => Some(WantChangeKind::Declined),
            4 => Some(WantChangeKind::Withdrawn),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WantChange {
    pub want_id: String,
    pub dataset: String,
    pub kind: WantChangeKind,
    pub subjects: Vec<String>,
    /// A decline's reason (UnansweredReason); 0 otherwise.
    pub reason: i32,
    /// The instance that answered or declined, or the readers who asked.
    pub by: Vec<String>,
    pub at_ns: i64,
    /// The want as it was asked, on an `Asked` change.
    pub want: Option<ObservationsWantedEvent>,
}

/// That a dataset not kept was served (W10.7): what, to whom, when; never
/// its values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Served {
    pub dataset: String,
    pub first_sequence: u64,
    pub last_sequence: u64,
    pub subjects: Vec<String>,
    pub fields: Vec<String>,
    pub readers: Vec<String>,
    pub at_ns: i64,
}

/// Rows removed by retention under a dataset's licence, its own record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removal {
    pub dataset: String,
    pub rows: u64,
    pub recorded_before_ns: i64,
    pub why: String,
    pub at_ns: i64,
}

/// A kept row's closed-day flag re-checked on a later version of its
/// dataset's calendar (contract v23, W10.4; the lake plan's vL1c, item 10):
/// journalled beside the row as a backfilled record is (decisions/031),
/// never a new version, since the source restated nothing.
#[derive(Debug, Clone, PartialEq)]
pub struct ClosedDayAmendment {
    pub dataset: String,
    pub sequence: u64,
    /// The flag as re-checked; None where the day is no longer closed.
    pub closed_day: Option<DateCheck>,
    /// Why: the calendar version applied.
    pub why: String,
    pub at_ns: i64,
}

/// A flag's reason, as a store keeps it beside the row: 0 for none.
pub fn closed_reason(flag: Option<&DateCheck>) -> i16 {
    flag.map_or(0, |check| check.reason as i16)
}

/// Each row's closed-day flag as known at a recorded time (0 for now): the
/// latest amendment of it at or before then, else the flag it was recorded
/// with (a point-in-time read shows the flag as known then).
pub fn as_flagged(rows: &mut [Observation], amendments: &[ClosedDayAmendment], as_of_ns: i64) {
    if amendments.is_empty() {
        return;
    }
    let mut latest: BTreeMap<(&str, u64), &ClosedDayAmendment> = BTreeMap::new();
    for amendment in amendments {
        if as_of_ns != 0 && amendment.at_ns > as_of_ns {
            continue;
        }
        let key = (amendment.dataset.as_str(), amendment.sequence);
        match latest.get(&key) {
            Some(held) if held.at_ns > amendment.at_ns => {}
            _ => {
                latest.insert(key, amendment);
            }
        }
    }
    for row in rows.iter_mut() {
        let key = (row.dataset().to_string(), row.meta().sequence);
        if let Some(amendment) = latest.get(&(key.0.as_str(), key.1)) {
            row.meta_mut().closed_day = amendment.closed_day.clone();
        }
    }
}

/// A `signal` plugin's escalation of one row version's quality (contract
/// v28, W10.12; ruling 423): its own record beside the row (decisions/031),
/// never a rewrite of it, kept by the row's dataset and sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assessment {
    pub dataset: String,
    pub sequence: u64,
    pub row_key: String,
    pub version: u64,
    /// `QualityState`: suspicious or invalid.
    pub state: i32,
    /// `QualityReason`s, in the order given.
    pub reasons: Vec<i32>,
    /// The assessing instance, as its sidecar stamped it.
    pub by_instance: String,
    pub at_ns: i64,
}

/// A state's place in the escalation: unspecified reads as valid.
pub fn rank(state: i32) -> i32 {
    if state == QualityState::Suspicious as i32 || state == QualityState::Invalid as i32 {
        state
    } else {
        QualityState::Valid as i32
    }
}

/// Whether a row's effective state is invalid: what a default or
/// side-by-side read leaves out (W10.6; rulings 421 and 422).
pub fn invalid(row: &Observation) -> bool {
    row.meta().quality == QualityState::Invalid as i32
}

/// [`Store::listing`] by reading whole rows, as any store can: what the
/// in-memory store answers, and what Postgres's index read is held to.
pub fn listing_read<S: Store + ?Sized>(
    store: &S,
    datasets: &[String],
    subjects: &[String],
    as_of_ns: i64,
) -> Result<Vec<Listed>> {
    let rows = store.read(&Query {
        datasets: datasets.to_vec(),
        subjects: subjects.to_vec(),
        data_type: DataType::Series,
        kinds: Vec::new(),
        interval_ns: 0,
        when: When::Latest { at_ns: 0 },
        as_of_ns,
        published_as_of_ns: 0,
        latest_only: false,
    })?;
    Ok(rows
        .iter()
        .filter(|row| !invalid(row) && subjects.iter().any(|s| s == row.subject()))
        .filter_map(listed_of)
        .collect())
}

/// A series row as the listing counts it ([`Listed`]); `None` for every
/// other row.
pub fn listed_of(row: &Observation) -> Option<Listed> {
    let (expires, listed_type) = row.listing()?;
    Some(Listed {
        dataset: row.dataset().to_string(),
        series: row.latest_key(),
        expires: expires.and_then(|date| Date::parse(&date).ok()),
        listed_type,
    })
}

/// Each row's effective quality as known at a recorded time (0 for now):
/// the highest of the state it was recorded with and every assessment of it
/// recorded by then, its reasons its own and then each assessment's, once
/// each (W10.4, W10.12). A row no assessment names reads as recorded.
pub fn as_assessed(rows: &mut [Observation], assessments: &[Assessment], as_of_ns: i64) {
    if assessments.is_empty() {
        return;
    }
    let mut of: BTreeMap<(&str, u64), Vec<&Assessment>> = BTreeMap::new();
    for assessment in assessments {
        if as_of_ns != 0 && assessment.at_ns > as_of_ns {
            continue;
        }
        of.entry((assessment.dataset.as_str(), assessment.sequence))
            .or_default()
            .push(assessment);
    }
    for row in rows.iter_mut() {
        let dataset = row.dataset().to_string();
        let Some(found) = of.get(&(dataset.as_str(), row.meta().sequence)) else {
            continue;
        };
        let meta = row.meta_mut();
        for assessment in found {
            if rank(assessment.state) > rank(meta.quality) {
                meta.quality = assessment.state;
            }
            for reason in &assessment.reasons {
                if !meta.quality_reasons.contains(reason) {
                    meta.quality_reasons.push(*reason);
                }
            }
        }
    }
}

/// Whether a flag counts as a closed day (a weekend or a holiday) or as
/// outside coverage, for the Data sources page (W10.9).
pub fn counted(reason: i16) -> (u64, u64) {
    match reason {
        r if r == DateCheckReason::Weekend as i16 || r == DateCheckReason::Holiday as i16 => (1, 0),
        r if r == DateCheckReason::OutsideCoverage as i16 => (0, 1),
        _ => (0, 0),
    }
}

/// A priority set against one read whose `updated_at_ns` has moved since:
/// the one standing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stale {
    pub standing_at_ns: i64,
}

/// One series of the lake's listing as an expirations read counts it
/// (contract v28, amended 2026-10-28; ruling 856): the dataset listing it,
/// the series as its latest is chosen per ([`Observation::latest_key`]), its
/// expiry where it states one, and its listed type as recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub dataset: String,
    pub series: String,
    pub expires: Option<Date>,
    pub listed_type: meridian_domain::v1::InstrumentType,
}

pub trait Store: Send + Sync {
    /// Record a batch of one dataset whole (W10.4, W10.5): each row a new
    /// row key, a restatement, or unchanged; recorded at `now_ns`.
    fn record(&self, dataset: &str, rows: Vec<Observation>, now_ns: i64) -> Result<Recorded>;

    /// A batch of a dataset whose licence forbids keeping it: numbered from
    /// its partition's head as any batch is, each row version 1, its values
    /// kept nowhere; only that it was served, and to whom (W10.7).
    fn serve_unkept(
        &self,
        dataset: &str,
        rows: Vec<Observation>,
        readers: &[String],
        now_ns: i64,
    ) -> Result<Recorded>;

    /// The rows a query names, each row key at its version as of the
    /// query's cut-off, with no choice among them made: which in force is
    /// [`crate::read`]'s.
    fn read(&self, query: &Query) -> Result<Vec<Observation>>;

    /// The lake's listing of `subjects` in `datasets` (contract v28, amended
    /// 2026-10-28; rulings 856 and 858): each series row key at its version
    /// in force at the recorded-time cut-off (0 for now), one whose
    /// effective state is invalid left out (ruling 619), as its dataset,
    /// series, expiry and listed type alone. A store may read whole rows, as
    /// this does; Postgres answers it from its listing index without reading
    /// a series row's body.
    fn listing(
        &self,
        datasets: &[String],
        subjects: &[String],
        as_of_ns: i64,
    ) -> Result<Vec<Listed>> {
        listing_read(self, datasets, subjects, as_of_ns)
    }

    /// Each partition's head.
    fn heads(&self) -> Result<BTreeMap<String, u64>>;

    /// Rows kept per dataset, and of them those carrying a value as reported.
    fn counts(&self) -> Result<BTreeMap<String, (u64, u64)>>;

    /// Per dataset holding any, the rows kept carrying a value as reported:
    /// what the datasets' listing answers (W10.9), read without counting
    /// every row (ruling 816).
    fn unconverted_counts(&self) -> Result<BTreeMap<String, u64>> {
        Ok(self
            .counts()?
            .into_iter()
            .filter(|(_, (_, unconverted))| *unconverted > 0)
            .map(|(dataset, (_, unconverted))| (dataset, unconverted))
            .collect())
    }

    /// The lowest sequence a dataset's partition still keeps, where it keeps
    /// any (contract v19): a watermark below it asks for rows retention
    /// removed.
    fn lowest_kept(&self, dataset: &str) -> Result<Option<u64>>;

    /// A priority replaced whole, journalled, against the one read
    /// (contract v17's stale guard): refused when it has moved.
    fn set_priority(
        &self,
        priority: &SourcePriority,
        against_updated_at_ns: i64,
    ) -> Result<std::result::Result<SourcePriority, Stale>>;

    /// Each data type's and kind's latest priority.
    fn priorities(&self) -> Result<Vec<SourcePriority>>;

    /// Every priority change, in the order journalled.
    fn priority_changes(&self) -> Result<Vec<SourcePriority>>;

    fn record_want(&self, change: &WantChange) -> Result<()>;

    fn want_changes(&self) -> Result<Vec<WantChange>>;

    /// What has been served, not kept.
    fn served(&self) -> Result<Vec<Served>>;

    /// Remove a dataset's rows recorded before a time, under its licence,
    /// and record the removal (decisions/031).
    fn remove_before(
        &self,
        dataset: &str,
        recorded_before_ns: i64,
        why: &str,
        now_ns: i64,
    ) -> Result<u64>;

    fn removals(&self) -> Result<Vec<Removal>>;

    /// One miss an instance reported (W3.2, W3.15), counted.
    fn count_miss(&self, instance: &str, at_ns: i64) -> Result<()>;

    fn miss_counts(&self) -> Result<BTreeMap<String, u64>>;

    /// A merged record followed (W3.8, W10.8): reads for `stays` answer the
    /// rows recorded under `replaced` too, never rewriting one.
    fn keep_alias(&self, replaced: &str, stays: &str, at_ns: i64) -> Result<()>;

    /// Each replaced record and the record that stays.
    fn aliases(&self) -> Result<BTreeMap<String, String>>;

    /// The data configuration as last heard.
    fn keep_configuration(&self, event: &EntitlementsChangedEvent) -> Result<()>;

    // ── The closed-day flag (contract v23, W10.4, W10.9) ──

    /// The first and last business date a dataset's kept rows name.
    fn dated_range(&self, dataset: &str) -> Result<Option<(String, String)>>;

    /// Every kept row of a dataset, every version, dated on one of `dates`,
    /// each with its flag as known now.
    fn dated_on(&self, dataset: &str, dates: &[String]) -> Result<Vec<Observation>>;

    /// Journal re-checks beside their rows.
    fn amend_closed_day(&self, amendments: &[ClosedDayAmendment]) -> Result<()>;

    /// Every re-check journalled, in order.
    fn closed_day_amendments(&self) -> Result<Vec<ClosedDayAmendment>>;

    /// Per dataset: the kept rows flagged on a closed day (a weekend or a
    /// holiday), and those flagged outside coverage, by the flag as known
    /// now.
    fn closed_day_counts(&self) -> Result<BTreeMap<String, (u64, u64)>>;

    /// Per dataset holding any (contract v29, W10.9; ruling 619 (2)): the
    /// rows in force -- each row key at its latest version kept -- whose
    /// effective state (the state recorded, raised by every assessment of
    /// that version since) is valid, suspicious and invalid. A new version
    /// moves its key from the last version's state to its own; a row served
    /// unkept is never in force; retention takes out a key it leaves no
    /// version of.
    fn quality_counts(&self) -> Result<BTreeMap<String, (u64, u64, u64)>>;

    fn configuration(&self) -> Result<Option<EntitlementsChangedEvent>>;

    /// Every row of one dataset and data type, each row key at its version
    /// as of a recorded time (0 for now), whatever its subject: how the
    /// account-scoped execution costs are read (contract v27, W10.11), their
    /// subject the execution's instrument, which a reader does not name.
    fn read_dataset(
        &self,
        dataset: &str,
        data_type: DataType,
        as_of_ns: i64,
    ) -> Result<Vec<Observation>>;

    /// Every version of every row of one dataset and data type, in sequence
    /// order: how a saved scenario is read at a version not its latest
    /// (contract v28, W10.6, W12.8).
    fn read_dataset_versions(&self, dataset: &str, data_type: DataType)
        -> Result<Vec<Observation>>;

    // ── Quality states (contract v28, W10.12; ruling 423) ──

    /// The kept rows of a dataset recorded under a row key at a version,
    /// whatever their data type, each at its effective state now: what an
    /// assessment names. Empty where the lake holds none.
    fn row_version(&self, dataset: &str, row_key: &str, version: u64) -> Result<Vec<Observation>>;

    /// Journal a batch of assessments whole, each its own record beside its
    /// row (decisions/031).
    fn assess(&self, assessments: &[Assessment]) -> Result<()>;

    /// Every assessment journalled, in order.
    fn assessments(&self) -> Result<Vec<Assessment>>;
}

/// Whether a query answers every version it finds, not each row key's
/// latest: an outcome's catch-up after a watermark, since every version is
/// delivered and none conflated (contract v23, the lake plan's vL1c, item
/// 4), so a reader that lost some reads each stage it lost.
///
/// A published-time read of rates (contract v27, W10.6) takes every version
/// too: which is in force by published time is the read's to choose.
///
/// From contract v28 a declared corporate action and a filing are caught up
/// the same way (the amendment of 2026-10-24: an unconflated row is caught
/// up after a watermark, as trades and outcomes are). From contract v29 a
/// factor too (W10.6).
pub fn every_version(query: &Query) -> bool {
    (matches!(
        query.data_type,
        DataType::Outcome | DataType::CorporateAction | DataType::Filing | DataType::Factor
    ) && matches!(query.when, When::After(_)))
        || query.published_as_of_ns != 0
}

/// Each row recorded at or before a cut-off (0 for none), every version.
pub fn recorded_by(rows: Vec<Observation>, as_of_ns: i64) -> Vec<Observation> {
    let mut out: Vec<Observation> = rows
        .into_iter()
        .filter(|row| as_of_ns == 0 || row.meta().recorded_at_ns <= as_of_ns)
        .collect();
    out.sort_by_key(|row| (row.dataset().to_string(), row.meta().sequence));
    out
}

/// Whether a row answers a query's valid time and kind, before versions are
/// chosen.
pub fn matches(row: &Observation, query: &Query) -> bool {
    if row.data_type() != query.data_type {
        return false;
    }
    if !query.kinds.is_empty() && !query.kinds.contains(&row.kind()) {
        return false;
    }
    if query.interval_ns != 0 && row.interval_ns() != query.interval_ns {
        return false;
    }
    let meta = row.meta();
    match &query.when {
        When::Latest { at_ns } => *at_ns == 0 || meta.valid_from_ns <= *at_ns,
        When::BusinessDate(date) => meta.business_date == date.to_string(),
        When::Range { from_ns, until_ns } => {
            meta.valid_from_ns >= *from_ns && (*until_ns == 0 || meta.valid_from_ns < *until_ns)
        }
        When::After(watermark) => watermark
            .get(row.dataset())
            .is_some_and(|after| meta.sequence > *after),
    }
}
