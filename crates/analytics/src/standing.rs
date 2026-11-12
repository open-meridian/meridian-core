//! Standing analytics (W12.1, W12.4; contract v30;
//! sdk-contract/standing-analytics-and-resilience-contract with
//! standing-analytics-compute-once; spec/one-data-access-model section 4;
//! rulings 801, 803 and 817, the step 3 plan's questions 10 to 14 ruled by
//! 1056, and the rulings of 2026-11-09).
//!
//! **Registering.** A job marked `standing` asks the coordinator to keep its
//! answer current. Its inputs are live: one naming `inputs.book_at`,
//! `inputs.lake_as_of_ns`, `business_date` or `what_if` is refused naming
//! `standing`, as is a measure stating no `refresh` where its definition
//! states none (no definition does yet). `RunJob` answers it as any job,
//! with its `job_id` and its current answer.
//!
//! **The key.** A standing job is keyed by its canonical form -- the job's
//! own message, which names no client -- its client's account scope, and
//! the datasets the client may read, as the lake's listing answers on the
//! client's behalf. Two clients with one key share one computation and one
//! `job_id`; clients whose entitlements or scope differ never share. The
//! datasets are every one the client may read, not only those the job
//! reads: two clients sharing less than they might never share a figure one
//! may not read.
//!
//! **Holding.** A client holds a job while its sidecar's renewals name it
//! (`RenewJobs`, one batched message per lease); a holder no renewal named
//! within `analytics.standing.leaseSeconds` is let go, and a job no holder
//! holds is stopped, its memory released. A renewal from an instance not
//! holding the job is answered unknown, so the SDK registers it again by
//! `RunJob`, which checks that client's scope and entitlements.
//!
//! **What is stale.** The coordinator hears the lake's prices, quotes and
//! rates and the book's position changes. A delivery about a subject or an
//! account the job's inputs name marks the job's measures dirty; nothing a
//! job does not name dirties it. A dirty `REFRESH_PER_CHANGE` measure is
//! recomputed at once, deliveries arriving meanwhile conflated into the
//! next; a dirty `REFRESH_CADENCE` one waits for its `cadence_ns`, or for a
//! move of a chain's underlying past `material_move_bp` from its price at
//! the last fit. Only the measures due are recomputed: each a sub-job of
//! the standing job naming those measures alone, its inputs read again by a
//! targeted read on a holder's behalf, so a standing figure is always the
//! figure `RunJob` answers for the same job on the same inputs (requirement
//! 25), which core holds by a property test.
//!
//! **Results.** Each recomputed figure that changed is published on
//! `platform.analytics.event.derived-series-updated`, one event per holding
//! instance, each numbered densely in that holder's own sequence (ruled
//! 2026-11-09), never over 512 KiB or 4,096 entries a list, split where it
//! would be. A figure keyed by a chain's series alone, at the base scenario,
//! is carried as a `DerivedSeries` in `values`; any other as a `Figure` in
//! `figures` (question 1). Published, never recorded.
//!
//! **Across coordinators** (question 13). A standing job is held by one
//! coordinator, chosen by rendezvous hashing of its key over the live
//! coordinators ([`owner`]), which each learns from the others' word on a
//! component topic of its own; none keeps a registry. A `RunJob` reaching
//! another coordinator is handed to the owner; a renewal or a read of a job
//! not held here is asked of the others. A coordinator lost, its jobs'
//! renewals are answered unknown and the SDK registers them again with the
//! new owner; a coordinator added, a job whose key no longer falls to its
//! holder is let go at its next renewal and registered again likewise.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meridian_analytics_engine::canonical;
use meridian_bus::{refusal_with_status, RefusalStatus};
use meridian_domain::exact::Exact;
use meridian_domain::v1::{
    AnswerProvenance, DerivedInput, DerivedSeries, DerivedSeriesUpdatedEvent, Figure, Job,
    JobAnswer, MeasureRef, Money, NotEvaluated, ObservationMeta, Refresh, ScenarioKind, SubjectRef,
    UnitValue, ValueUnit,
};
use prost::Message;
use serde_json::Value;

/// The lease a sidecar's renewals keep a standing job held by: the chart's
/// `analytics.standing.leaseSeconds`, 30 by default.
pub const LEASE: Duration = Duration::from_secs(30);

/// The most standing jobs a coordinator holds: the chart's
/// `analytics.standing.most`. One past it is refused FAILED_PRECONDITION
/// naming `standing` (616).
pub const MOST: usize = 256;

/// The most entries one list of a `DerivedSeriesUpdatedEvent` carries
/// (the dictionary's bounds on `values`, `figures` and `not_evaluated`).
pub const MOST_ENTRIES: usize = 4_096;

/// Where an event is split: about 512 KiB, an event never past it
/// (question 14).
pub const EVENT_BUDGET: usize = 512 * 1024;

/// Room kept for an event's own fields and its envelope.
const KEPT_FOR_THE_REST: usize = 4 * 1024;

/// How long a read of a standing job's answer waits for its first answer.
pub const FIRST_ANSWER_WAIT: Duration = Duration::from_secs(20);

/// How a coordinator holds standing jobs.
#[derive(Debug, Clone)]
pub struct Settings {
    /// A holder no renewal named within it is let go.
    pub lease: Duration,
    /// The most jobs held.
    pub most: usize,
    /// How long a read of a job's answer waits for its first.
    pub first_answer_wait: Duration,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            lease: LEASE,
            most: MOST,
            first_answer_wait: FIRST_ANSWER_WAIT,
        }
    }
}

fn invalid(field: String, words: impl std::fmt::Display) -> String {
    refusal_with_status(RefusalStatus::InvalidArgument, &[field], words)
}

/// A standing job's own refusals (W12.1): live inputs only, and every
/// measure's refresh stated, a cadence with its cadence. Nothing for a job
/// not standing, whose measures' refresh is not read.
pub fn refused(job: &Job) -> Result<(), String> {
    if !job.standing {
        return Ok(());
    }
    let inputs = job.inputs.clone().unwrap_or_default();
    let named = [
        (inputs.book_at.is_some(), "inputs.book_at"),
        (inputs.lake_as_of_ns != 0, "inputs.lake_as_of_ns"),
        (!job.business_date.is_empty(), "business_date"),
        (job.what_if.is_some(), "what_if"),
    ];
    if let Some((_, field)) = named.iter().find(|(set, _)| *set) {
        return Err(invalid(
            "job.standing".into(),
            format!(
                "standing: a standing job's inputs are live, so it names no {field}; run it \
                 not standing to read as of a time or a date, or with a what-if"
            ),
        ));
    }
    for (i, measure) in job.measures.iter().enumerate() {
        match Refresh::try_from(measure.refresh) {
            Ok(Refresh::PerChange) => {
                if measure.cadence_ns != 0 {
                    return Err(invalid(
                        format!("job.measures[{i}].cadence_ns"),
                        "cadence_ns is set only with REFRESH_CADENCE",
                    ));
                }
                if measure.material_move_bp != 0 {
                    return Err(invalid(
                        format!("job.measures[{i}].material_move_bp"),
                        "material_move_bp is set only with REFRESH_CADENCE",
                    ));
                }
            }
            Ok(Refresh::Cadence) => {
                if measure.cadence_ns <= 0 {
                    return Err(invalid(
                        format!("job.measures[{i}].cadence_ns"),
                        "a cadence measure states its cadence_ns, above 0",
                    ));
                }
            }
            // No definition the component serves states a default refresh,
            // so a measure stating none is refused (ruled 2026-11-09, step
            // 3c's question 3): a figure's cost is never chosen by default.
            Ok(Refresh::Unspecified) => {
                return Err(invalid(
                    format!("job.measures[{i}].refresh"),
                    format!(
                        "{} version {} states no default refresh, so a standing job's measure \
                         states REFRESH_PER_CHANGE or REFRESH_CADENCE",
                        measure.definition, measure.version
                    ),
                ));
            }
            Err(_) => {
                return Err(invalid(
                    format!("job.measures[{i}].refresh"),
                    format!("{} is no Refresh", measure.refresh),
                ));
            }
        }
    }
    Ok(())
}

/// A standing job's key: its canonical form, its client's account scope and
/// the datasets the client may read, hashed (question 11).
pub fn key(job: &Job, scope: &Option<Vec<String>>, datasets: &BTreeSet<String>) -> String {
    let mut job = job.clone();
    job.standing = true;
    let mut bytes = job.encode_to_vec();
    bytes.push(0x1f);
    match scope {
        // A core component reading as itself.
        None => bytes.extend_from_slice(b"\x00core"),
        Some(scope) => {
            let mut sorted: Vec<&String> = scope.iter().collect();
            sorted.sort();
            sorted.dedup();
            for account in sorted {
                bytes.extend_from_slice(account.as_bytes());
                bytes.push(0x1e);
            }
        }
    }
    bytes.push(0x1f);
    for dataset in datasets {
        bytes.extend_from_slice(dataset.as_bytes());
        bytes.push(0x1e);
    }
    canonical::sha256_name(&bytes)
}

/// The lake's data types a job reads, as its inputs are resolved: a price
/// always; a chain's series, quotes, statistics, dividends and rate curve
/// where it reads a chain; an event group's outcomes; a saved scenario; a
/// pool's factors and the curves a debt measure reads. A standing job's key
/// counts the client's datasets of these alone (question 11).
pub fn reads(
    job: &Job,
    compiled: &meridian_analytics_engine::compile::Compiled,
) -> BTreeSet<&'static str> {
    use meridian_domain::lake;
    let read = &compiled.reads;
    let mut types: BTreeSet<&'static str> = [lake::PRICE].into();
    if read.chains || read.held_chains || !job.chains.is_empty() {
        types.extend([
            lake::LISTED_SERIES,
            lake::QUOTE,
            lake::STATISTIC,
            lake::CORPORATE_ACTION,
            lake::RATE,
        ]);
    }
    if !read.event_groups.is_empty() {
        types.insert(lake::OUTCOME);
    }
    if job.scenario_ref.is_some() {
        types.insert(lake::SAVED_SCENARIO);
    }
    if read.factors {
        types.insert(lake::FACTOR);
    }
    if read.debt || read.treasury_curve || read.benchmark_curves {
        types.insert(lake::RATE);
    }
    if read.executions {
        types.insert(lake::QUOTE);
    }
    types
}

/// The coordinator holding a key: of the live ones, the one whose name
/// hashed with the key is highest (rendezvous hashing; question 13). Every
/// coordinator computes the same choice from the same members; one lost or
/// added moves only the keys that were, or now are, its own.
pub fn owner<'a>(key: &str, members: &'a [String]) -> Option<&'a String> {
    members
        .iter()
        .max_by_key(|member| canonical::sha256_name(format!("{member}\u{1f}{key}").as_bytes()))
}

/// What a coordinator asks another about a standing job, on the client's
/// behalf: the request as the client sent it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asked {
    RunJob,
    RenewJobs,
    ReadJobAnswer,
}

impl Asked {
    pub fn payload_type(self) -> &'static str {
        match self {
            Asked::RunJob => "meridian.v1.RunJobRequest",
            Asked::RenewJobs => "meridian.v1.RenewJobsRequest",
            Asked::ReadJobAnswer => "meridian.v1.ReadJobAnswerRequest",
        }
    }

    pub fn of(payload_type: &str) -> Option<Asked> {
        [Asked::RunJob, Asked::RenewJobs, Asked::ReadJobAnswer]
            .into_iter()
            .find(|asked| asked.payload_type() == payload_type)
    }
}

/// The live coordinators, and a question asked of one of them for a client.
pub trait Peers: Send + Sync {
    /// This coordinator's name among them.
    fn me(&self) -> String;
    /// Every live coordinator's name, this one's among them, sorted: what a
    /// key is placed over.
    fn live(&self) -> Vec<String>;
    /// Every coordinator worth asking about a job not held here: the live
    /// ones and any heard of lately, so a word missed for a few seconds
    /// never answers a held job unknown. One gone answers no one, at once.
    fn reachable(&self) -> Vec<String> {
        self.live()
    }
    /// `payload`, asked of `member` as `asked` for `client` within its
    /// scope; the reply's bytes, or the refusal it answered.
    fn ask(
        &self,
        member: &str,
        asked: Asked,
        payload: Vec<u8>,
        client: &str,
        scope: &Option<Vec<String>>,
    ) -> Result<Vec<u8>, String>;
}

/// One coordinator alone: the tolerant profile, compose, a test.
pub struct Alone(pub String);

impl Peers for Alone {
    fn me(&self) -> String {
        self.0.clone()
    }
    fn live(&self) -> Vec<String> {
        vec![self.0.clone()]
    }
    fn ask(
        &self,
        member: &str,
        _asked: Asked,
        _payload: Vec<u8>,
        _client: &str,
        _scope: &Option<Vec<String>>,
    ) -> Result<Vec<u8>, String> {
        Err(format!("{member} is not a live coordinator"))
    }
}

/// When a measure of a standing job is recomputed, and what it was last
/// fitted at.
#[derive(Debug, Clone)]
pub(crate) struct Clock {
    pub refresh: Refresh,
    pub cadence_ns: i64,
    pub move_bp: u32,
    pub dirty: bool,
    pub fitted_at_ns: i64,
    /// Each chain underlying's price at the last fit, for a material move.
    pub reference: BTreeMap<String, Exact>,
}

impl Clock {
    fn of(measure: &MeasureRef) -> Clock {
        Clock {
            refresh: Refresh::try_from(measure.refresh).unwrap_or(Refresh::PerChange),
            cadence_ns: measure.cadence_ns,
            move_bp: measure.material_move_bp,
            dirty: false,
            fitted_at_ns: 0,
            reference: BTreeMap::new(),
        }
    }

    /// Whether this measure is recomputed now: a dirty per-change one at
    /// once; a dirty cadence one once its cadence has passed since its last
    /// fit, or sooner on a material move.
    pub fn due(&self, now_ns: i64, prices: &BTreeMap<String, Exact>) -> bool {
        if !self.dirty {
            return false;
        }
        match self.refresh {
            Refresh::Cadence => {
                now_ns.saturating_sub(self.fitted_at_ns) >= self.cadence_ns || self.moved(prices)
            }
            _ => true,
        }
    }

    /// Whether a chain's underlying has moved `move_bp` basis points or more
    /// from its price at the last fit.
    fn moved(&self, prices: &BTreeMap<String, Exact>) -> bool {
        if self.move_bp == 0 {
            return false;
        }
        self.reference.iter().any(|(underlying, at_fit)| {
            prices
                .get(underlying)
                .is_some_and(|now| moved_bp(*at_fit, *now, self.move_bp))
        })
    }
}

/// Whether `now` is `bp` basis points or more from `at_fit`, exactly:
/// |now - at_fit| x 10,000 against bp x |at_fit|, both at one scale. A
/// price of zero, or one too large to compare, never moves.
pub(crate) fn moved_bp(at_fit: Exact, now: Exact, bp: u32) -> bool {
    if at_fit.is_zero() {
        return false;
    }
    let scale = at_fit.scale().max(now.scale());
    let at = |value: Exact| {
        10i128
            .checked_pow(scale - value.scale())
            .and_then(|by| value.integer().checked_mul(by))
    };
    let (Some(fit), Some(now)) = (at(at_fit), at(now)) else {
        return false;
    };
    let moved = (now - fit)
        .checked_abs()
        .and_then(|d| d.checked_mul(10_000));
    let bound = fit
        .checked_abs()
        .and_then(|f| f.checked_mul(i128::from(bp)));
    matches!((moved, bound), (Some(moved), Some(bound)) if moved >= bound)
}

/// One measure's figures, as last computed.
#[derive(Debug, Clone, Default)]
pub(crate) struct Part {
    pub figures: Vec<Figure>,
    pub values: Vec<DerivedSeries>,
    pub not_evaluated: Vec<NotEvaluated>,
}

impl Part {
    /// What it holds, as encoded.
    pub fn bytes(&self) -> u64 {
        let figures: usize = self.figures.iter().map(Message::encoded_len).sum();
        let values: usize = self.values.iter().map(Message::encoded_len).sum();
        let unevaluated: usize = self.not_evaluated.iter().map(Message::encoded_len).sum();
        (figures + values + unevaluated) as u64
    }
}

/// What a standing job holds for each byte of its answer: its measures'
/// parts, the whole answer drawn from them and what was last published,
/// each a copy (stage M: its reservation against the component's budget).
pub const HELD_FACTOR: u64 = 3;

/// A holder of a standing job: a client instance, the scope its sidecar
/// stamped, when a renewal last named the job for it, the last of its own
/// sequence, and the answer it is reading in pages.
#[derive(Debug, Clone)]
pub(crate) struct Holder {
    pub scope: Option<Vec<String>>,
    pub renewed_at_ns: i64,
    pub sequence: i64,
    pub reading: Option<Arc<JobAnswer>>,
    /// Told by `JobFinished` once the first answer is in: its `RunJob` was
    /// answered before it was.
    pub told_later: bool,
}

/// The subjects and accounts a standing job's inputs name: what a delivery
/// must be about to dirty it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Leaves {
    pub subjects: BTreeSet<String>,
    pub accounts: BTreeSet<String>,
}

/// A standing job held.
pub(crate) struct Held {
    pub job_id: String,
    pub key: String,
    pub job: Job,
    pub holders: BTreeMap<String, Holder>,
    pub clocks: Vec<Clock>,
    pub parts: Vec<Part>,
    /// The whole current answer; none until the first is in.
    pub answer: Option<Arc<JobAnswer>>,
    /// The first answer's refusal, where it was refused.
    pub refused: Option<String>,
    pub leaves: Leaves,
    /// Each chain underlying's latest price heard.
    pub prices: BTreeMap<String, Exact>,
    /// What was last published, by each entry's key: its bytes.
    pub published: BTreeMap<Vec<u8>, Vec<u8>>,
    /// A recomputation is under way.
    pub in_flight: bool,
    /// Measures recomputed since registration, each counted once per
    /// recomputation: the engine's line per standing job.
    pub recomputed: u64,
    /// Its memory, reserved against the component's budget while it is
    /// held, and given back when it stops (stage M; approved 2026-11-10).
    pub reservation: Option<meridian_budget::Reservation>,
}

impl Held {
    pub fn new(job_id: String, key: String, job: Job) -> Held {
        let clocks = job.measures.iter().map(Clock::of).collect();
        let parts = vec![Part::default(); job.measures.len()];
        Held {
            job_id,
            key,
            job,
            holders: BTreeMap::new(),
            clocks,
            parts,
            answer: None,
            refused: None,
            leaves: Leaves::default(),
            prices: BTreeMap::new(),
            published: BTreeMap::new(),
            in_flight: false,
            recomputed: 0,
            reservation: None,
        }
    }

    /// The measures due now, in the job's order.
    pub fn due(&self, now_ns: i64) -> Vec<usize> {
        if self.answer.is_none() || self.in_flight {
            return Vec::new();
        }
        self.clocks
            .iter()
            .enumerate()
            .filter(|(_, clock)| clock.due(now_ns, &self.prices))
            .map(|(i, _)| i)
            .collect()
    }

    /// The holder whose reads a recomputation is made on: the one renewed
    /// last, so a holder gone is not read for.
    pub fn reader(&self) -> Option<(String, Option<Vec<String>>)> {
        self.holders
            .iter()
            .max_by_key(|(_, holder)| holder.renewed_at_ns)
            .map(|(client, holder)| (client.clone(), holder.scope.clone()))
    }

    /// The job naming only the measures `due`, read live and once.
    pub fn sub_job(&self, due: &[usize]) -> Job {
        let mut job = self.job.clone();
        job.standing = false;
        job.measures = due.iter().map(|&i| self.job.measures[i].clone()).collect();
        job
    }
}

/// The standing jobs a coordinator holds, by job ID, and by key.
#[derive(Default)]
pub(crate) struct Registry {
    pub jobs: BTreeMap<String, Held>,
    pub by_key: BTreeMap<String, String>,
}

impl Registry {
    pub fn stop(&mut self, job_id: &str) {
        if let Some(held) = self.jobs.remove(job_id) {
            self.by_key.remove(&held.key);
        }
    }
}

/// Whether a measure of the job and a figure's measure are one.
fn same_measure(asked: &MeasureRef, answered: Option<&MeasureRef>) -> bool {
    let Some(answered) = answered else {
        return false;
    };
    asked.definition == answered.definition
        && asked.version == answered.version
        && asked.breakdowns == answered.breakdowns
        && asked.event_group_id == answered.event_group_id
}

/// What a standing job's resolved inputs name: every `INS-` record the
/// document holds, and the job's accounts.
pub(crate) fn leaves(job: &Job, inputs: &Value) -> Leaves {
    fn walk(value: &Value, into: &mut BTreeSet<String>) {
        match value {
            Value::String(text) if text.starts_with("INS-") => {
                into.insert(text.clone());
            }
            Value::Array(items) => items.iter().for_each(|item| walk(item, into)),
            Value::Object(fields) => fields.values().for_each(|field| walk(field, into)),
            _ => {}
        }
    }
    let mut subjects = BTreeSet::new();
    walk(inputs, &mut subjects);
    for chain in &job.chains {
        subjects.insert(chain.underlying_instrument_id.clone());
    }
    Leaves {
        subjects,
        accounts: job.account_ids.iter().cloned().collect(),
    }
}

/// Each chain's underlying, and its price as the inputs read it.
pub(crate) fn spots(inputs: &Value) -> BTreeMap<String, Exact> {
    let mut spots = BTreeMap::new();
    for chain in inputs["chains"].as_array().into_iter().flatten() {
        let Some(underlying) = chain["underlying_instrument_id"].as_str() else {
            continue;
        };
        if let Some(amount) = chain["spot"]["amount"]
            .as_str()
            .and_then(|amount| Exact::from_str(amount).ok())
        {
            spots.insert(underlying.to_string(), amount);
        }
    }
    spots
}

/// Each series' underlying, by the series' first identifier's value, as the
/// inputs' chains list them.
fn underlyings(job: &Job, inputs: &Value) -> BTreeMap<String, String> {
    let mut by_series = BTreeMap::new();
    for chain in inputs["chains"].as_array().into_iter().flatten() {
        let Some(underlying) = chain["underlying_instrument_id"].as_str() else {
            continue;
        };
        for series in chain["series"].as_array().into_iter().flatten() {
            for key in series["series_key"].as_array().into_iter().flatten() {
                if let Some(value) = key["value"].as_str() {
                    by_series.insert(value.to_string(), underlying.to_string());
                }
            }
        }
    }
    // A chain asked by its keys alone names its underlying beside them.
    for chain in &job.chains {
        for key in &chain.series_keys {
            by_series
                .entry(key.value.clone())
                .or_insert_with(|| chain.underlying_instrument_id.clone());
        }
    }
    by_series
}

/// The most characters a row key holds.
const ROW_KEY_MOST: usize = 128;

/// A figure keyed by a chain's series alone, at the base scenario, with a
/// value, as a `DerivedSeries` (W12.4, question 1): its row key and series
/// the definition and its other breakdowns' values, its subject the
/// series' underlying, its rule the definition and version, its inputs the
/// datasets read and the as-of they were read at. None for any other
/// figure, which `figures` carries.
fn value_of(
    figure: &Figure,
    underlyings: &BTreeMap<String, String>,
    provenance: &AnswerProvenance,
    computed_at_ns: i64,
) -> Option<DerivedSeries> {
    let base = figure
        .scenario_key
        .as_ref()
        .is_none_or(|key| matches!(ScenarioKind::try_from(key.kind), Ok(ScenarioKind::Base)));
    if figure.series_key.is_empty() || !figure.account_id.is_empty() || !base {
        return None;
    }
    let value = figure.value?;
    let symbol = figure.series_key.first()?.value.clone();
    let underlying = underlyings.get(&symbol)?.clone();
    let measure = figure.measure.clone().unwrap_or_default();
    let mut series = measure.definition.clone();
    for part in &figure.breakdown {
        if part.breakdown != "series" {
            series.push(':');
            series.push_str(&part.value);
        }
    }
    let row_key: String = format!("{series}:{symbol}")
        .chars()
        .take(ROW_KEY_MOST)
        .collect();
    let (value, money_value) = if figure.currency_instrument_id.is_empty() {
        (
            Some(UnitValue {
                value: Some(value),
                unit: ValueUnit::Unspecified as i32,
                ..Default::default()
            }),
            None,
        )
    } else {
        (
            None,
            Some(Money {
                amount: Some(value),
                currency_code: String::new(),
                instrument_id: figure.currency_instrument_id.clone(),
            }),
        )
    };
    Some(DerivedSeries {
        meta: Some(ObservationMeta {
            row_key,
            subjects: vec![SubjectRef {
                entity_id: underlying,
                ..Default::default()
            }],
            series_key: figure.series_key.clone(),
            valid_from_ns: computed_at_ns,
            ..Default::default()
        }),
        series,
        value,
        rule: Some(MeasureRef {
            definition: measure.definition,
            version: measure.version,
            ..Default::default()
        }),
        inputs: provenance
            .datasets
            .iter()
            .map(|dataset| DerivedInput {
                dataset: dataset.clone(),
                as_of_ns: provenance.lake_as_of_ns,
            })
            .collect(),
        published_by: String::new(),
        money_value,
    })
}

/// A sub-job's answer taken into the job's parts: each due measure's
/// figures and what was not evaluated, each figure keyed by series carried
/// as a value.
pub(crate) fn take(
    held: &mut Held,
    due: &[usize],
    answer: &JobAnswer,
    inputs: &Value,
    computed_at_ns: i64,
) {
    let provenance = answer.provenance.clone().unwrap_or_default();
    let underlyings = underlyings(&held.job, inputs);
    let mut parts: BTreeMap<usize, Part> = due.iter().map(|&i| (i, Part::default())).collect();
    let owners = |measure: Option<&MeasureRef>| -> Vec<usize> {
        let mut found: Vec<usize> = due
            .iter()
            .copied()
            .filter(|&i| same_measure(&held.job.measures[i], measure))
            .collect();
        if found.is_empty() {
            // Said of the job rather than one measure: the first due holds it.
            found.extend(due.first().copied());
        }
        found
    };
    for figure in &answer.figures {
        for i in owners(figure.measure.as_ref()) {
            let part = parts.get_mut(&i).expect("a due measure's part");
            match value_of(figure, &underlyings, &provenance, computed_at_ns) {
                Some(value) => part.values.push(value),
                None => part.figures.push(figure.clone()),
            }
        }
    }
    for unevaluated in &answer.not_evaluated {
        for i in owners(unevaluated.measure.as_ref()) {
            parts
                .get_mut(&i)
                .expect("a due measure's part")
                .not_evaluated
                .push(unevaluated.clone());
        }
    }
    for (i, part) in parts {
        held.parts[i] = part;
    }
    held.recomputed += due.len() as u64;
}

/// The job's whole current answer from its parts, in the job's order, its
/// provenance the last recomputation's with the hash over the whole.
pub(crate) fn whole(held: &Held, provenance: AnswerProvenance) -> JobAnswer {
    let mut answer = JobAnswer {
        job_id: held.job_id.clone(),
        ..JobAnswer::default()
    };
    for part in &held.parts {
        answer.figures.extend(part.figures.iter().cloned());
        answer.values.extend(part.values.iter().cloned());
        answer
            .not_evaluated
            .extend(part.not_evaluated.iter().cloned());
    }
    let mut provenance = provenance;
    provenance.answer_hash = String::new();
    answer.provenance = Some(provenance.clone());
    provenance.answer_hash = canonical::sha256_name(&answer.encode_to_vec());
    answer.provenance = Some(provenance);
    answer
}

/// An entry of an answer: which it is and what names it, apart from its
/// value, so a later one replaces it.
#[derive(Debug, Clone)]
pub(crate) enum Entry {
    Figure(Box<Figure>),
    Value(Box<DerivedSeries>),
    NotEvaluated(Box<NotEvaluated>),
}

impl Entry {
    fn key(&self) -> Vec<u8> {
        match self {
            Entry::Figure(figure) => {
                let named = Figure {
                    measure: figure.measure.clone(),
                    account_id: figure.account_id.clone(),
                    breakdown: figure.breakdown.clone(),
                    scenario_key: figure.scenario_key.clone(),
                    series_key: figure.series_key.clone(),
                    ..Default::default()
                };
                let mut key = vec![b'f'];
                key.extend(named.encode_to_vec());
                key
            }
            Entry::Value(value) => {
                let meta = value.meta.clone().unwrap_or_default();
                let named = DerivedSeries {
                    meta: Some(ObservationMeta {
                        row_key: meta.row_key,
                        subjects: meta.subjects,
                        series_key: meta.series_key,
                        ..Default::default()
                    }),
                    series: value.series.clone(),
                    rule: value.rule.clone(),
                    ..Default::default()
                };
                let mut key = vec![b'v'];
                key.extend(named.encode_to_vec());
                key
            }
            Entry::NotEvaluated(unevaluated) => {
                let mut key = vec![b'n'];
                key.extend(unevaluated.encode_to_vec());
                key
            }
        }
    }

    /// Its bytes, apart from when it was computed: a value recomputed to the
    /// same is not a change.
    fn bytes(&self) -> Vec<u8> {
        match self {
            Entry::Figure(figure) => figure.encode_to_vec(),
            Entry::Value(value) => {
                let mut value = (**value).clone();
                if let Some(meta) = value.meta.as_mut() {
                    meta.valid_from_ns = 0;
                }
                value.encode_to_vec()
            }
            Entry::NotEvaluated(_) => Vec::new(),
        }
    }

    fn encoded_len(&self) -> usize {
        match self {
            Entry::Figure(figure) => figure.encoded_len(),
            Entry::Value(value) => value.encoded_len(),
            Entry::NotEvaluated(unevaluated) => unevaluated.encoded_len(),
        }
    }
}

/// Every entry of an answer, in its order.
pub(crate) fn entries(answer: &JobAnswer) -> Vec<Entry> {
    answer
        .figures
        .iter()
        .cloned()
        .map(|figure| Entry::Figure(Box::new(figure)))
        .chain(
            answer
                .values
                .iter()
                .cloned()
                .map(|value| Entry::Value(Box::new(value))),
        )
        .chain(
            answer
                .not_evaluated
                .iter()
                .cloned()
                .map(|unevaluated| Entry::NotEvaluated(Box::new(unevaluated))),
        )
        .collect()
}

/// What an answer holds, by entry key: what [`changed`] compares against.
pub(crate) fn published(answer: &JobAnswer) -> BTreeMap<Vec<u8>, Vec<u8>> {
    entries(answer)
        .into_iter()
        .map(|entry| (entry.key(), entry.bytes()))
        .collect()
}

/// The entries of `answer` that changed since `before`, conflated per key:
/// only the latest of each is ever carried.
pub(crate) fn changed(answer: &JobAnswer, before: &BTreeMap<Vec<u8>, Vec<u8>>) -> Vec<Entry> {
    entries(answer)
        .into_iter()
        .filter(|entry| before.get(&entry.key()) != Some(&entry.bytes()))
        .collect()
}

/// The changed entries as events, each within [`EVENT_BUDGET`] and
/// [`MOST_ENTRIES`] a list, for one holder: its instance named, each event
/// numbered next in its sequence.
pub(crate) fn events(
    job_id: &str,
    client: &str,
    sequence: &mut i64,
    changes: &[Entry],
) -> Vec<DerivedSeriesUpdatedEvent> {
    events_within(job_id, client, sequence, changes, EVENT_BUDGET)
}

pub(crate) fn events_within(
    job_id: &str,
    client: &str,
    sequence: &mut i64,
    changes: &[Entry],
    budget: usize,
) -> Vec<DerivedSeriesUpdatedEvent> {
    let fresh = |sequence: &mut i64| {
        *sequence += 1;
        DerivedSeriesUpdatedEvent {
            job_id: job_id.to_string(),
            client_instance_id: client.to_string(),
            sequence: *sequence,
            ..Default::default()
        }
    };
    let room = budget.saturating_sub(KEPT_FOR_THE_REST);
    let mut out = Vec::new();
    if changes.is_empty() {
        return out;
    }
    let mut event = fresh(sequence);
    let mut used = event.encoded_len();
    for entry in changes {
        let len = entry.encoded_len();
        let more = 1 + prost::encoding::encoded_len_varint(len as u64) + len;
        let full = match entry {
            Entry::Figure(_) => event.figures.len() >= MOST_ENTRIES,
            Entry::Value(_) => event.values.len() >= MOST_ENTRIES,
            Entry::NotEvaluated(_) => event.not_evaluated.len() >= MOST_ENTRIES,
        };
        let empty =
            event.figures.is_empty() && event.values.is_empty() && event.not_evaluated.is_empty();
        if !empty && (full || used + more > room) {
            out.push(event);
            event = fresh(sequence);
            used = event.encoded_len();
        }
        used += more;
        match entry {
            Entry::Figure(figure) => event.figures.push((**figure).clone()),
            Entry::Value(value) => event.values.push((**value).clone()),
            Entry::NotEvaluated(unevaluated) => event.not_evaluated.push((**unevaluated).clone()),
        }
    }
    out.push(event);
    out
}

/// The subjects a lake row's event is about, and a price's amount where it
/// is one; the account a position change names.
#[derive(Debug, Default)]
pub(crate) struct Heard {
    pub subjects: Vec<String>,
    pub price: Option<Exact>,
    pub account: Option<String>,
}

/// A delivery read for what it is about, or none for one this does not
/// hear.
pub(crate) fn heard(payload_type: &str, payload: &[u8]) -> Option<Heard> {
    use meridian_domain::v1::{
        PositionChangedEvent, PricesRecordedEvent, QuotesRecordedEvent, RatesRecordedEvent,
    };
    let subjects = |meta: Option<&ObservationMeta>| -> Vec<String> {
        meta.map(|meta| meta.subjects.iter().map(|s| s.entity_id.clone()).collect())
            .unwrap_or_default()
    };
    match payload_type {
        "meridian.v1.PricesRecordedEvent" => {
            let price = PricesRecordedEvent::decode(payload).ok()?.price?;
            Some(Heard {
                subjects: subjects(price.meta.as_ref()),
                price: price
                    .price
                    .as_ref()
                    .and_then(|money| money.amount.as_ref())
                    .and_then(|amount| Exact::from_wire(amount).ok()),
                account: None,
            })
        }
        "meridian.v1.QuotesRecordedEvent" => {
            let quote = QuotesRecordedEvent::decode(payload).ok()?.quote?;
            Some(Heard {
                subjects: subjects(quote.meta.as_ref()),
                ..Heard::default()
            })
        }
        "meridian.v1.RatesRecordedEvent" => {
            let rate = RatesRecordedEvent::decode(payload).ok()?.rate?;
            Some(Heard {
                subjects: subjects(rate.meta.as_ref()),
                ..Heard::default()
            })
        }
        "meridian.v1.PositionChangedEvent" => {
            let position = PositionChangedEvent::decode(payload).ok()?.position?;
            Some(Heard {
                account: Some(position.account_id),
                ..Heard::default()
            })
        }
        _ => None,
    }
}

/// The topics a coordinator hears for its standing jobs (topics.tsv: the
/// analytics component subscribes to each, contract v30).
pub const HEARD: [&str; 4] = [
    "platform.lake.*.event.prices-recorded",
    "platform.lake.*.event.quotes-recorded",
    "platform.lake.*.event.rates-recorded",
    "platform.book.event.position-changed",
];

/// The component topic each coordinator says it is live on, and hears the
/// others on. A component topic, not a contract row (topics.md, "Component
/// topics"; approved 2026-11-10).
pub const MEMBERS: &str = "platform.analytics.internal.members";

/// The component topic a coordinator is asked a standing job's question on
/// by another: its own, by its member name (approved 2026-11-10).
pub fn asked_of(member: &str) -> String {
    format!("platform.analytics.internal.standing.{member}")
}

/// How often a coordinator says it is live, and how long one unheard is
/// still counted live.
pub const SAID_EVERY: Duration = Duration::from_secs(2);

/// How long a coordinator heard of is still asked about a job not held
/// here, beside the live ones.
pub const ASKED_FOR: Duration = Duration::from_secs(120);

/// How long another coordinator has to answer a renewal or a read.
pub const ASKED_WITHIN: Duration = Duration::from_secs(3);
pub const LIVE_FOR: Duration = Duration::from_secs(10);

/// The live coordinators, by what each says on [`MEMBERS`], and a
/// question asked of one on its own topic.
pub struct BusPeers {
    bus: Arc<meridian_bus::Bus>,
    handle: tokio::runtime::Handle,
    me: String,
    heard: Mutex<BTreeMap<String, std::time::Instant>>,
    timeout: Duration,
    /// The live set last said, so each change is said once.
    said: Mutex<Vec<String>>,
}

/// The payload type a coordinator's own topic answers a refusal under.
pub const REFUSED_TYPE: &str = "analytics.standing.refused";

/// The payload type a coordinator says it is live under.
const LIVE_TYPE: &str = "analytics.coordinator.live";

impl BusPeers {
    /// Says this coordinator is live every [`SAID_EVERY`], and hears the
    /// others, from now.
    pub fn start(
        bus: Arc<meridian_bus::Bus>,
        handle: tokio::runtime::Handle,
        me: String,
        timeout: Duration,
    ) -> Arc<BusPeers> {
        let peers = Arc::new(BusPeers {
            bus: Arc::clone(&bus),
            handle: handle.clone(),
            me: me.clone(),
            heard: Mutex::new(BTreeMap::new()),
            timeout,
            said: Mutex::new(Vec::new()),
        });
        let mut hearing = bus.subscribe_every(MEMBERS);
        let held = Arc::clone(&peers);
        handle.spawn(async move {
            while let Some(delivery) = hearing.recv().await {
                if delivery.envelope.payload_type != LIVE_TYPE {
                    continue;
                }
                let Ok(said) = serde_json::from_slice::<Value>(&delivery.envelope.payload) else {
                    continue;
                };
                if let Some(member) = said["member"].as_str() {
                    held.heard
                        .lock()
                        .expect("not poisoned")
                        .insert(member.to_string(), std::time::Instant::now());
                }
            }
        });
        let saying = Arc::clone(&bus);
        handle.spawn(async move {
            let payload = serde_json::to_vec(&serde_json::json!({"member": me})).expect("json");
            loop {
                if let Err(failed) = saying.publish(MEMBERS, LIVE_TYPE, payload.clone(), None, None)
                {
                    tracing::debug!(%failed, "this coordinator could not say it is live");
                }
                tokio::time::sleep(SAID_EVERY).await;
            }
        });
        peers
    }
}

impl Peers for BusPeers {
    fn me(&self) -> String {
        self.me.clone()
    }

    fn live(&self) -> Vec<String> {
        let now = std::time::Instant::now();
        let mut live: BTreeSet<String> = self
            .heard
            .lock()
            .expect("not poisoned")
            .iter()
            .filter(|(_, at)| now.duration_since(**at) <= LIVE_FOR)
            .map(|(member, _)| member.clone())
            .collect();
        live.insert(self.me.clone());
        let live: Vec<String> = live.into_iter().collect();
        let mut said = self.said.lock().expect("not poisoned");
        if *said != live {
            tracing::info!(
                me = self.me,
                live = live.join(","),
                "the live coordinators changed"
            );
            *said = live.clone();
        }
        live
    }

    fn reachable(&self) -> Vec<String> {
        let now = std::time::Instant::now();
        let mut heard: BTreeSet<String> = self
            .heard
            .lock()
            .expect("not poisoned")
            .iter()
            .filter(|(_, at)| now.duration_since(**at) <= ASKED_FOR)
            .map(|(member, _)| member.clone())
            .collect();
        heard.extend(self.live());
        heard.into_iter().collect()
    }

    fn ask(
        &self,
        member: &str,
        asked: Asked,
        payload: Vec<u8>,
        client: &str,
        scope: &Option<Vec<String>>,
    ) -> Result<Vec<u8>, String> {
        let stamp = meridian_bus::Stamp {
            account_scope: scope.clone(),
            on_behalf_of_instance_id: client.to_string(),
            ..meridian_bus::Stamp::default()
        };
        let (kind, reply) = self
            .handle
            .block_on(self.bus.call_stamped(
                &asked_of(member),
                asked.payload_type(),
                payload,
                None,
                // A renewal or a read is the coordinator's own memory: a
                // member that does not answer it soon is gone, and its jobs
                // are answered unknown within the lease (1048's 30 s).
                Some(match asked {
                    Asked::RunJob => self.timeout,
                    _ => self.timeout.min(ASKED_WITHIN),
                }),
                &stamp,
            ))
            .map_err(|failed| failed.to_string())?;
        if kind == REFUSED_TYPE {
            return Err(String::from_utf8_lossy(&reply).into_owned());
        }
        Ok(reply)
    }
}

/// A member name a topic can carry: its pod's, dots and wildcards made
/// dashes.
pub fn member_named(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

pub mod coordinator;

#[cfg(test)]
mod tests;
