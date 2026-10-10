//! What the lake's store keeps, and what any store of it must provide.
//!
//! One writer per dataset partition: a batch takes its partition's head and
//! numbers its rows from it in its own transaction, so the numbers have no
//! holes. A row is never updated: a changed value under a row key held is a
//! new version, and an identical one changes nothing.

use std::collections::BTreeMap;

use meridian_domain::date::Date;
use meridian_domain::v1::{EntitlementsChangedEvent, ObservationsWantedEvent, SourcePriority};

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
}

/// What a batch did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Recorded {
    /// The rows it recorded, new and restated, as recorded: their envelopes
    /// filled. An unchanged row is not among them.
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

/// A priority set against one read whose `updated_at_ns` has moved since:
/// the one standing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stale {
    pub standing_at_ns: i64,
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

    /// Each partition's head.
    fn heads(&self) -> Result<BTreeMap<String, u64>>;

    /// Rows kept per dataset, and of them those carrying a value as reported.
    fn counts(&self) -> Result<BTreeMap<String, (u64, u64)>>;

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

    fn configuration(&self) -> Result<Option<EntitlementsChangedEvent>>;
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
    }
}
