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
use meridian_changes::{Change, ReadChangesReply, ReadError};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("the lake's store is unavailable: {0}")]
    Unavailable(String),

    #[error("{0}")]
    SchemaAhead(String),

    /// A commit refused by its partition's epoch: another writer took the
    /// lake over, and this one steps down (step 3a; ruling 1060).
    #[error("{0}")]
    Fenced(#[from] meridian_changes::Fenced),

    /// A write-ahead batch refused because the store's position in the
    /// stream is not the one the batch follows (the `ha` profile; the
    /// resilience spec's section 4): the database failed over and lost the
    /// commits after it, or another writer moved it. The writer reads the
    /// position again and goes on from there.
    #[error("{0}")]
    PositionMoved(String),

    /// A write to a dataset another writer group owns (step 3d; the
    /// resilience spec's section 6.7): handed to that group's writer.
    #[error("dataset {partition} is writer group {group}'s to write; this member's group does not own it")]
    NotOwned { partition: String, group: u32 },

    /// A write to a range a move is switching (step 3d; section 6.8, the
    /// hold): tried again until the switch ends, at most
    /// `moves.holdSeconds`, then refused OVERLOADED with a retry-after.
    #[error("{0}")]
    Held(String),

    /// A restatement that changes its row key's valid time or first subject
    /// (step 3d; the-lake rule 5, ruled 2026-11-12): refused
    /// INVALID_ARGUMENT naming the field, `field` its path within the row
    /// (`meta.valid_from_ns`, `meta.subjects[0].entity_id`). The plugin
    /// cancels the old key and records a new one.
    #[error("{words}")]
    KeyFixed {
        data_type: DataType,
        row_key: String,
        field: &'static str,
        words: String,
    },

    /// A partition act refused, naming the field (step 3d; section 6.1):
    /// `SplitPartition`'s INVALID_ARGUMENT naming `partition`.
    #[error("{words}")]
    Invalid { field: &'static str, words: String },
}

/// The field a restated row's valid time is refused by (the-lake rule 5),
/// within the row: kept exact, since SDK 0.36.3's `SeriesTimes` reads a
/// series re-send refused naming `series[i].meta.valid_from_ns` as recorded.
pub const VALID_TIME_FIELD: &str = "meta.valid_from_ns";

/// The field a restated row's first subject is refused by, within the row.
pub const FIRST_SUBJECT_FIELD: &str = "meta.subjects[0].entity_id";

pub type Result<T> = std::result::Result<T, StoreError>;

/// What a store that keeps no write-ahead position answers: a reader's.
fn no_write_ahead() -> StoreError {
    StoreError::Unavailable("this member keeps no write-ahead position: the writer does".into())
}

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
    /// A published-time cut-off (contract v27, W10.6); 0 for none. Given,
    /// the store answers each row key at its version in force by then, of
    /// those recorded by `as_of_ns` ([`crate::read::as_published`]), chosen
    /// as it reads: never every version held to choose from.
    pub published_as_of_ns: i64,
    /// A read of the latest that narrows its rows no further before the
    /// latest is chosen: the store may answer, of the rows it would, only
    /// each one's latest per subject, dataset, kind, venue, length, asset
    /// and point, and the latest of those not invalid, as the read chooses
    /// them ([`crate::read`]). A store may answer every row still.
    pub latest_only: bool,
}

/// The lake's place in its write-ahead stream (the `ha` profile; the
/// resilience spec's section 4): the last of the stream's sequences whose
/// writes the lake has recorded, and the stream's time for it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriteAheadPosition {
    pub sequence: u64,
    pub at_ns: i64,
}

/// Writes taken from the write-ahead stream, recorded as one: each part one
/// request's rows of one dataset that the lake keeps, in the stream's order,
/// and the position they take the lake to, committed with them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WriteAheadBatch {
    /// The stream's name.
    pub stream: String,
    /// The position the writer last read or committed: the batch is refused
    /// [`StoreError::PositionMoved`] where the store holds another.
    pub after: Option<WriteAheadPosition>,
    /// The position the batch commits.
    pub through: WriteAheadPosition,
    pub parts: Vec<(String, Vec<Observation>)>,
    /// Sequences the stream dropped before the lake took them, found by
    /// this batch: kept, so a later write naming one is known to follow a
    /// gap (W10.14).
    pub dropped: Vec<Dropped>,
    /// Each dataset's latest valid time the batch recorded: where a gap of
    /// it found later begins.
    pub valid: Vec<(String, i64)>,
    /// Each gap the batch found, as first recorded (W10.14): its own record,
    /// in the same transaction, so no gap found is lost with its batch.
    pub gaps: Vec<meridian_domain::v1::DatasetGap>,
}

/// A run of the write-ahead stream's sequences it dropped at its bound
/// before the lake took them (W10.14): the first and last, the stream's
/// time of the last write the lake had before them and of the first it
/// took after, and the gap recorded for it the moment it was found, its
/// datasets not yet known (approved 2026-11-11): each dataset's gap found
/// from the run names it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dropped {
    pub first: u64,
    pub last: u64,
    pub after_at_ns: i64,
    pub until_at_ns: i64,
    /// The unattributed gap's ID; empty for a run found before the lake
    /// recorded one.
    pub gap_id: String,
}

impl Dropped {
    /// Whether the stream dropped `sequence`.
    pub fn holds(&self, sequence: u64) -> bool {
        (self.first..=self.last).contains(&sequence)
    }
}

/// A dataset gap's control change's kind (W10.14): each change of its state.
pub const CONTROL_GAP: &str = "meridian.v1.DatasetGap";

/// A default read's source switched, or switched back (W10.6; the
/// resilience spec's section 8): its control change's kind.
pub const CONTROL_SWITCH: &str = "meridian.v1.SourceSwitched";

/// A dataset's assignment to a writer group (step 3d; section 6.1, approved
/// 2026-11-12): its `PartitionOwner`, in the `control` partition, recorded
/// only when its group changes. None recorded means group 0.
pub const CONTROL_OWNER: &str = "meridian.v1.PartitionOwner";

/// A range's split (step 3d; section 6.1, approved 2026-11-12): its
/// `PartitionSplit`, in the `control` partition.
pub const CONTROL_SPLIT: &str = "meridian.v1.PartitionSplit";

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
    /// Each row's partition, in step with `rows` (step 3d; section 6.1):
    /// empty where every row is in the dataset's own full range.
    pub partitions: Vec<String>,
}

impl Recorded {
    /// The partition `rows[at]` was recorded in.
    pub fn partition_of<'a>(&'a self, dataset: &'a str, at: usize) -> &'a str {
        self.partitions.get(at).map_or(dataset, String::as_str)
    }

    /// Another range's part of the same batch added to this one: its rows
    /// after these, its counts added.
    pub fn joined(&mut self, dataset: &str, more: Recorded, partition: &str) {
        if !self.partitions.is_empty() || partition != dataset {
            // Rows before these, unnamed, are in the dataset's own range.
            self.partitions.resize(self.rows.len(), dataset.to_string());
            self.partitions
                .extend(std::iter::repeat_n(partition.to_string(), more.rows.len()));
        }
        self.rows.extend(more.rows);
        self.recorded += more.recorded;
        self.restated += more.restated;
        self.unchanged += more.unchanged;
        self.head = self.head.max(more.head);
    }
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
    /// The row's key (step 3d): two ranges split from one number on from
    /// the same sequence. Empty on a re-check kept before 3d, of the one
    /// row its dataset held at that sequence.
    pub row_key: String,
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
    let mut latest: BTreeMap<(&str, u64, &str), &ClosedDayAmendment> = BTreeMap::new();
    for amendment in amendments {
        if as_of_ns != 0 && amendment.at_ns > as_of_ns {
            continue;
        }
        let key = (
            amendment.dataset.as_str(),
            amendment.sequence,
            amendment.row_key.as_str(),
        );
        match latest.get(&key) {
            Some(held) if held.at_ns > amendment.at_ns => {}
            _ => {
                latest.insert(key, amendment);
            }
        }
    }
    for row in rows.iter_mut() {
        let (dataset, sequence) = (row.dataset().to_string(), row.meta().sequence);
        let row_key = row.meta().row_key.clone();
        // The row's own re-check; one kept before 3d names no row key.
        let found = [row_key.as_str(), ""]
            .into_iter()
            .filter_map(|key| latest.get(&(dataset.as_str(), sequence, key)))
            .max_by_key(|amendment| amendment.at_ns);
        if let Some(amendment) = found {
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

/// The lake's control partition's kinds (step 3b; ruling 1062's carried
/// item): what is not a dataset's row, in the order it was committed, so a
/// follower's quality states, flags, priorities and aliases match the
/// writer's.
pub const CONTROL_ASSESSMENT: &str = "assessment";
pub const CONTROL_CLOSED_DAY: &str = "closed_day_amendment";
pub const CONTROL_PRIORITY: &str = "meridian.v1.SourcePriority";
pub const CONTROL_ALIAS: &str = "alias";

/// An assessment as its control change's body.
pub fn assessment_body(assessment: &Assessment) -> Vec<u8> {
    serde_json::json!({
        "dataset": assessment.dataset,
        "sequence": assessment.sequence,
        "row_key": assessment.row_key,
        "version": assessment.version,
        "state": assessment.state,
        "reasons": assessment.reasons,
        "by_instance": assessment.by_instance,
        "at_ns": assessment.at_ns,
    })
    .to_string()
    .into_bytes()
}

/// A closed-day amendment as its control change's body: the flag as
/// re-checked, encoded, where the day is still closed.
pub fn amendment_body(amendment: &ClosedDayAmendment) -> Vec<u8> {
    use base64::Engine as _;
    use prost::Message as _;
    serde_json::json!({
        "dataset": amendment.dataset,
        "sequence": amendment.sequence,
        "row_key": amendment.row_key,
        "closed_day": amendment.closed_day.as_ref().map(|check| {
            base64::engine::general_purpose::STANDARD.encode(check.encode_to_vec())
        }),
        "why": amendment.why,
        "at_ns": amendment.at_ns,
    })
    .to_string()
    .into_bytes()
}

/// An alias kept as its control change's body.
pub fn alias_body(replaced: &str, stays: &str, at_ns: i64) -> Vec<u8> {
    serde_json::json!({ "replaced": replaced, "stays": stays, "at_ns": at_ns })
        .to_string()
        .into_bytes()
}

/// A control change, as the log carries it.
pub fn control_change(
    sequence: u64,
    epoch: u64,
    at_ns: i64,
    kind: &str,
    body: Vec<u8>,
) -> meridian_changes::Change {
    meridian_changes::Change {
        partition: meridian_changes::CONTROL.to_string(),
        sequence,
        epoch,
        origin: meridian_changes::origin().to_string(),
        committed_at_ns: at_ns,
        kind: kind.to_string(),
        digest: meridian_changes::integrity::digest(&body),
        body,
    }
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
    // By the row's dataset, sequence and key: two ranges split from one
    // number on from the same sequence (step 3d).
    let mut of: BTreeMap<(&str, u64, &str), Vec<&Assessment>> = BTreeMap::new();
    for assessment in assessments {
        if as_of_ns != 0 && assessment.at_ns > as_of_ns {
            continue;
        }
        of.entry((
            assessment.dataset.as_str(),
            assessment.sequence,
            assessment.row_key.as_str(),
        ))
        .or_default()
        .push(assessment);
    }
    for row in rows.iter_mut() {
        let dataset = row.dataset().to_string();
        let row_key = row.meta().row_key.clone();
        let Some(found) = of.get(&(dataset.as_str(), row.meta().sequence, row_key.as_str())) else {
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

    /// Several batches of one dataset recorded as one (step 3d, group
    /// commit; the resilience spec's section 6.5): numbered in turn, each
    /// decided as [`Store::record`] decides a batch against what is held and
    /// what the parts before it numbered, and answered with what it did, in
    /// the parts' order. A store that commits them together tries each
    /// again alone after an unexpected database error.
    fn record_parts(
        &self,
        dataset: &str,
        parts: Vec<Vec<Observation>>,
        now_ns: i64,
    ) -> Vec<Result<Recorded>> {
        parts
            .into_iter()
            .map(|rows| self.record(dataset, rows, now_ns))
            .collect()
    }

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

    /// The lake's position in its write-ahead stream `stream`, none before
    /// its first batch from it (the `ha` profile).
    fn write_ahead_position(&self, stream: &str) -> Result<Option<WriteAheadPosition>> {
        let _ = stream;
        Err(no_write_ahead())
    }

    /// Writes from the write-ahead stream recorded as one (the `ha`
    /// profile; the resilience spec's section 4): each dataset's parts
    /// numbered in turn, as one batch of it, decided as [`Store::record`]
    /// decides a batch, and the stream's position moved to `through` in the
    /// same transaction, so the rows and the position commit or roll back
    /// together. Answered with what each part did, in the parts' order.
    /// Each part's answer in the batch's order: a part refused alone (the
    /// lake's rule 5) is its own refusal, the batch and its position
    /// committed without it.
    fn record_write_ahead(
        &self,
        batch: WriteAheadBatch,
        now_ns: i64,
    ) -> Result<Vec<Result<Recorded>>> {
        let _ = (batch, now_ns);
        Err(no_write_ahead())
    }

    /// What the write-ahead stream dropped that the lake has found, and each
    /// dataset's latest valid time recorded from it (W10.14): what a writer
    /// that starts goes on with.
    fn write_ahead_found(&self, stream: &str) -> Result<(Vec<Dropped>, BTreeMap<String, i64>)> {
        let _ = stream;
        Err(no_write_ahead())
    }

    /// A dataset gap's next state, its own record (W10.14; decisions/031),
    /// in the `control` partition.
    fn record_gap(&self, gap: &meridian_domain::v1::DatasetGap) -> Result<()> {
        let _ = gap;
        Err(no_write_ahead())
    }

    /// Every dataset gap's every state, in the order recorded (W10.14).
    fn dataset_gaps(&self) -> Result<Vec<meridian_domain::v1::DatasetGap>> {
        Ok(Vec::new())
    }

    /// Switches of the default reads' sources, and switches back, each its
    /// own record (W10.6; the resilience spec's section 8; decisions/031),
    /// in the `control` partition, in one transaction.
    fn record_switches(&self, switches: &[meridian_domain::v1::SourceSwitched]) -> Result<()> {
        let _ = switches;
        Err(StoreError::Unavailable(
            "this store keeps no source switches".into(),
        ))
    }

    /// Every switch recorded, in the order recorded (W10.6).
    /// Each dataset whose writer group under `groups` is not the one its
    /// assignments last recorded (none recorded: group 0) recorded in the
    /// `control` partition as its `PartitionOwner` (step 3d): its group and
    /// its epoch now. Answers how many it recorded. A store with no
    /// partitions records none.
    fn record_assignments(
        &self,
        groups: meridian_changes::groups::Groups,
        at_ns: i64,
    ) -> Result<usize> {
        let _ = (groups, at_ns);
        Ok(0)
    }

    fn source_switches(&self) -> Result<Vec<meridian_domain::v1::SourceSwitched>> {
        Ok(Vec::new())
    }

    /// Halve the open range `partition` at its midpoint (step 3d;
    /// `SplitPartition`, section 6.1): in one transaction of its writer, the
    /// parent closed at its head N and both children opened after it,
    /// numbering from N + 1 and staying with the parent's group, and the
    /// split's `PartitionSplit` recorded in the `control` partition. Copies
    /// nothing. Refused [`StoreError::Invalid`] naming `partition` for the
    /// control partition, a book partition, a range not open and a range of
    /// one hash value.
    fn split_partition(
        &self,
        partition: &str,
        split_by: &str,
        reason: &str,
        now_ns: i64,
    ) -> Result<meridian_domain::v1::PartitionSplit> {
        let _ = (split_by, reason, now_ns);
        Err(StoreError::Invalid {
            field: "partition",
            words: format!("{partition} is not a range this lake has open"),
        })
    }

    /// Every split recorded, in the order recorded (section 6.1).
    fn splits(&self) -> Result<Vec<meridian_domain::v1::PartitionSplit>> {
        Ok(Vec::new())
    }

    /// The rows a query names, each row key at its version as of the
    /// query's cut-off, with no choice among them made: which in force is
    /// [`crate::read`]'s.
    fn read(&self, query: &Query) -> Result<Vec<Observation>>;

    /// [`Store::read`]'s rows, each handed to `each` as it is read, so a
    /// reader that keeps less than every row never holds them all (found on
    /// dev 2026-11-09: a chain's statistics over its dates, 358,688 rows
    /// decoded at once to answer a page of 500, took the lake past its 1
    /// GiB). In no order a reader may count on.
    fn read_each(
        &self,
        query: &Query,
        each: &mut dyn FnMut(Observation) -> Result<()>,
    ) -> Result<()> {
        for row in self.read(query)? {
            each(row)?;
        }
        Ok(())
    }

    /// The rows kept at these places, by dataset and sequence, each as
    /// [`Store::read`] answers it as of `as_of_ns` -- its closed-day flag
    /// and its effective quality as known then -- in the order given. A row
    /// no longer kept (retention removed it) is left out. What a held
    /// answer's later page reads, its rows held by their places alone
    /// (found on dev 2026-11-09: a chain's statistics, 49,412 rows, held
    /// whole or answered again whole at each page).
    /// The rows at `places`, each named by its dataset, sequence and its row
    /// key's hash ([`crate::range::key_hash`]; step 3d: two ranges split
    /// from one number on from the same sequence).
    fn rows_at(
        &self,
        data_type: DataType,
        places: &[(String, u64, u32)],
        as_of_ns: i64,
    ) -> Result<Vec<Observation>>;

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

    /// Per dataset holding any, the rows in force carrying a value as
    /// reported -- each row key at its version in force ([`force_order`];
    /// ruling 1006 (c)), so a key corrected by a converted value counts no
    /// more: what the datasets' listing answers (W10.9), read without
    /// counting every row (ruling 816).
    fn unconverted_counts(&self) -> Result<BTreeMap<String, u64>>;

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

    /// The anchor each of `keys` (data type code, row key) of `dataset` is
    /// placed by -- its first version's first subject's hash -- where the
    /// lake holds one (step 3d; ruling 2 of 2026-11-14): what a write's
    /// router reads, in one look, for a key restated under the record that
    /// replaced its subject. None where the store keeps no ranges.
    fn anchors(
        &self,
        dataset: &str,
        keys: &[(i16, String)],
    ) -> Result<BTreeMap<(i16, String), u16>> {
        let _ = (dataset, keys);
        Ok(BTreeMap::new())
    }

    /// The data configuration as last heard.
    fn keep_configuration(&self, event: &EntitlementsChangedEvent) -> Result<()>;

    // ── The closed-day flag (contract v23, W10.4, W10.9) ──

    /// The first and last business date a dataset's kept rows name.
    fn dated_range(&self, dataset: &str) -> Result<Option<(String, String)>>;

    /// A dataset's kept rows, every version, dated on one of `dates`, each
    /// with its flag as known now: at most `most` of them, those after
    /// sequence `after`, in sequence order. A calendar's re-check reads a
    /// dataset this way, [`READ_CHUNK`] at a time, never holding it whole
    /// (found 2026-11-09: a chain's dataset kept about 565,000 dated rows,
    /// about 216 MB of bodies, every one read and decoded at once).
    fn dated_on(
        &self,
        dataset: &str,
        dates: &[String],
        after: u64,
        most: usize,
    ) -> Result<Vec<Observation>>;

    /// Journal re-checks beside their rows.
    fn amend_closed_day(&self, amendments: &[ClosedDayAmendment]) -> Result<()>;

    /// Every re-check journalled, in order.
    fn closed_day_amendments(&self) -> Result<Vec<ClosedDayAmendment>>;

    /// Per dataset: the rows in force flagged on a closed day (a weekend or
    /// a holiday), and those flagged outside coverage, by the flag as known
    /// now -- each row key at its version in force ([`force_order`]; ruling
    /// 1006 (c)), as every count of a dataset's rows is.
    fn closed_day_counts(&self) -> Result<BTreeMap<String, (u64, u64)>>;

    /// Per dataset holding any (contract v29, W10.9; ruling 619 (2)): the
    /// rows in force -- each row key at its version in force, the one a
    /// default read serves ([`force_order`]; ruling 1006 (b)) -- whose
    /// effective state (the state recorded, raised by every assessment of
    /// that version since) is valid, suspicious and invalid. A version
    /// taking force moves its key from the last one's state to its own; a
    /// rate's late-arriving older publication moves nothing; a row served
    /// unkept is never in force; retention takes out a key it leaves no
    /// version of.
    fn quality_counts(&self) -> Result<BTreeMap<String, (u64, u64, u64)>>;

    fn configuration(&self) -> Result<Option<EntitlementsChangedEvent>>;

    /// Every row of one dataset and data type, each row key at its version
    /// as of a recorded time (0 for now), whatever its subject: how the
    /// account-scoped execution costs are read (contract v27, W10.11), their
    /// subject the execution's instrument, which a reader does not name.
    /// Given a published-time cut (0 for none), each row key at its version
    /// in force by then of those recorded by `as_of_ns`
    /// ([`crate::read::as_published`]), chosen as the store reads.
    fn read_dataset(
        &self,
        dataset: &str,
        data_type: DataType,
        as_of_ns: i64,
        published_as_of_ns: i64,
    ) -> Result<Vec<Observation>>;

    /// [`Store::read_dataset`], each row handed on as it is read, in
    /// sequence order (stage M; the store memory audit of 2026-11-09, item
    /// 6): a reader keeps what it answers -- one account's execution costs,
    /// the scenarios asked -- never the dataset. A store that reads a chunk
    /// at a time says so by overriding this.
    fn read_dataset_each(
        &self,
        dataset: &str,
        data_type: DataType,
        as_of_ns: i64,
        each: &mut dyn FnMut(Observation) -> Result<()>,
    ) -> Result<()> {
        for row in self.read_dataset(dataset, data_type, as_of_ns, 0)? {
            each(row)?;
        }
        Ok(())
    }

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

    /// The writer's epoch, adopted from the `control` partition where none
    /// was yet (step 3a): what a starting lake does before its first batch,
    /// and what a store not started as a writer does at its first.
    fn adopt(&self) -> Result<u64>;

    /// The epoch this store's batches commit under; 0 before it adopted one.
    fn epoch(&self) -> u64;

    /// Take the lake over: raise the epoch of the `control` partition and
    /// of every dataset's head in one transaction, record who took it over
    /// and why (step 3b, `WriterTookOver`), and adopt it. Every batch
    /// another writer commits under the old one is refused.
    fn take_over(&self, by: &meridian_changes::TookOver) -> Result<u64>;

    /// The `control` partition's epoch now, as the record says (step 3b): a
    /// writer steps down once it moves past its own.
    fn head_epoch(&self) -> Result<u64>;

    /// Follow the database's timeline (contract v30, stage C; the
    /// resilience spec's section 3): on a new one the epoch raised as a
    /// takeover raises it and adopted, the takeover recorded with that
    /// cause. A store with no database never moves.
    fn follow_timeline(&self) -> Result<meridian_changes::timeline::Timeline> {
        Ok(meridian_changes::timeline::Timeline::Same)
    }

    /// A page of `partition`'s change log after `after` (step 3a): its rows
    /// in sequence, each range it served and did not keep as one change, at
    /// most `most_bytes`; the `control` partition's none yet.
    fn read_changes(
        &self,
        partition: &str,
        after: u64,
        most_bytes: usize,
    ) -> std::result::Result<ReadChangesReply, ReadError>;
}

/// How many rows a store reads from its database at once, decoded and
/// handed on before the next: a streamed read's chunk, and a re-check's.
pub const READ_CHUNK: usize = 1_000;

/// Whether a query answers every version it finds, not each row key's
/// latest: an outcome's catch-up after a watermark, since every version is
/// delivered and none conflated (contract v23, the lake plan's vL1c, item
/// 4), so a reader that lost some reads each stage it lost.
///
/// A read as published by a time (contract v27, W10.6) is never the
/// latest's: which version is in force by then is chosen from every one
/// the read would load, by the store as it reads
/// ([`Query::published_as_of_ns`]), never by a latest read's index.
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

/// The order a row key's versions take force in (ruling 1006 (b)): the
/// version in force is the one a default read serves, the greatest by this
/// order among those it would load, and every count of rows in force
/// follows it.
///
/// A rate's versions are ordered by published time, then by version
/// (contract v27, W10.4: a publisher's revision is the next version of the
/// row key, ordered by published time), so a late-arriving older
/// publication is kept as the next version but never takes force; every
/// other data type's by version alone. What `lake_row.published_at_ns`
/// keeps (migration 8).
pub fn force_order(row: &Observation) -> (i64, u64) {
    (published_order(row).unwrap_or(0), row.meta().version)
}

/// A rate's published time, by which its versions take force (ruling 1006
/// (b)); none for any other data type.
pub fn published_order(row: &Observation) -> Option<i64> {
    (row.data_type() == DataType::Rate).then(|| crate::read::published_at(row))
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

/// A row as its partition's change: its data type, its message encoded, the
/// epoch it was written under and the deployment it was made in.
pub fn row_change(dataset: &str, row: &Observation, epoch: u64, origin: &str) -> Change {
    Change {
        partition: dataset.to_string(),
        sequence: row.meta().sequence,
        epoch,
        origin: origin.to_string(),
        committed_at_ns: row.meta().recorded_at_ns,
        kind: row.data_type().name().to_string(),
        digest: meridian_changes::integrity::digest(&row.encode()),
        body: row.encode(),
    }
}

/// Rows served and not kept, as one change: the last sequence they took,
/// and the first, their subjects and fields and who they reached in its
/// body, as JSON.
pub fn served_change(served: &Served, epoch: u64, origin: &str) -> Change {
    let list = |items: &[String]| {
        items
            .iter()
            .map(|item| format!("{item:?}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    Change {
        partition: served.dataset.clone(),
        sequence: served.last_sequence,
        epoch,
        origin: origin.to_string(),
        committed_at_ns: served.at_ns,
        kind: "served".to_string(),
        body: format!(
            "{{\"first_sequence\":{},\"last_sequence\":{},\"subjects\":[{}],\"fields\":[{}],\"readers\":[{}]}}",
            served.first_sequence,
            served.last_sequence,
            list(&served.subjects),
            list(&served.fields),
            list(&served.readers)
        )
        .into_bytes(),
        digest: Vec::new(),
    }
}
