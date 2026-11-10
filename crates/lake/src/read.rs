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
//! history, not kept, or not covered. A decline stands for the date or range
//! its want asked, or the latest, and for no other (contract v19).
//!
//! The latest is chosen per subject, dataset, kind, venue and the row's
//! asset (contract v19, approved 2026-10-10): BTC priced in USD and in USDC
//! on one venue answers both, as a quote's latest per its asset; a rate's
//! per its tenor, maturity and convention besides (contract v27, W10.6).
//! A rate read as published by a time (contract v27, shared additions 1 and
//! 2) answers each row key at the version in force by published time among
//! those recorded by the recorded-time cut. A default read of rates takes
//! each row key's version in force by published time too (ruling 1006 (b);
//! [`crate::store::force_order`]): a late-arriving older publication is the
//! next version, kept, and never takes force. A catch-up
//! after a watermark (trades, the plan's Q33) asks no source: what was not
//! recorded since is nothing new.
//!
//! From contract v28 (W10.6; rulings 421, 422 and 619) every row answers its
//! effective quality, and a read that does not name its datasets with
//! `include_quarantined` leaves a row whose state is invalid out before the
//! latest is chosen, so the last row not invalid stands; a dataset left with
//! nothing for a subject is source silent, asked nothing and never fallen
//! back to. A suspicious row is served, flagged.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use meridian_domain::date::Date;
use meridian_domain::v1::{
    InstrumentType, SourceChoice, SourcePriority, SourceTimeKind, Tenor, UnansweredReason,
};
use meridian_pb::v1::ObservationMode;

use crate::config::{DataConfig, Reader};
use crate::row::{DataType, Observation};
use crate::store::{invalid, Query, Store, When};
use meridian_bus::{refusal_with_status, RefusalStatus};

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
#[derive(Debug, Clone, PartialEq)]
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
    /// Contract v28, amended 2026-10-28 (ruling 821; the v29 plan's item
    /// E9): the read's series, named by key or every series of the
    /// subjects, and its expiration window, so a chain want is told apart
    /// from the underlying's own and the data plugin fetches only that.
    pub series_keys: Vec<meridian_domain::v1::Identifier>,
    pub all_series: bool,
    pub window: Window,
}

/// An expiration window, plain dates inclusive (contract v28, amended
/// 2026-10-28; ruling 821): `None` on a side for no bound there.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Window {
    pub from: Option<Date>,
    pub to: Option<Date>,
}

impl Window {
    /// Whether the window bounds anything.
    pub fn is_set(&self) -> bool {
        self.from.is_some() || self.to.is_some()
    }

    /// Whether a series expiring on `date` is in the window.
    pub fn holds(&self, date: &Date) -> bool {
        self.from.as_ref().is_none_or(|from| date >= from)
            && self.to.as_ref().is_none_or(|to| date <= to)
    }

    /// Whether every date `other` holds, this holds.
    pub fn covers(&self, other: &Window) -> bool {
        let from = match (&self.from, &other.from) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(a), Some(b)) => a <= b,
        };
        let to = match (&self.to, &other.to) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(a), Some(b)) => a >= b,
        };
        from && to
    }

    /// The least window holding both: a standing want's window is the
    /// union of its readers' windows (ruling 821).
    pub fn union(&self, other: &Window) -> Window {
        Window {
            from: match (&self.from, &other.from) {
                (Some(a), Some(b)) => Some(*a.min(b)),
                _ => None,
            },
            to: match (&self.to, &other.to) {
                (Some(a), Some(b)) => Some(*a.max(b)),
                _ => None,
            },
        }
    }

    /// Its sides as a want and a request carry them: plain dates, empty for
    /// no bound.
    pub fn sides(&self) -> (String, String) {
        (
            self.from.as_ref().map(Date::to_string).unwrap_or_default(),
            self.to.as_ref().map(Date::to_string).unwrap_or_default(),
        )
    }
}

/// When a listed series expires (contract v28): its template's
/// `expiration_date`, else its `last_trade_date`, else its
/// `final_settlement_date`; `None` where it states none of them.
pub fn expires(series: &meridian_domain::v1::ListedSeries) -> Option<Date> {
    [
        "expiration_date",
        "last_trade_date",
        "final_settlement_date",
    ]
    .iter()
    .find_map(|path| {
        series
            .fields
            .iter()
            .find(|field| field.path == *path && !field.value.is_empty())
            .map(|field| field.value.clone())
    })
    .and_then(|date| Date::parse(&date).ok())
}

/// A listed series' type as its row states it (W1.24): a future names its
/// product code, every other series is an option. An option whose
/// underlying is a future record is a future option, told by the
/// underlying ([`as_listed`]).
pub fn listed_type(series: &meridian_domain::v1::ListedSeries) -> InstrumentType {
    let product_code = series
        .fields
        .iter()
        .any(|field| field.path == "product_code" && !field.value.is_empty());
    if product_code {
        InstrumentType::Future
    } else {
        InstrumentType::ListedOption
    }
}

/// A series' type under its underlying (W1.24): an option on a future
/// record is a future option; `underlying` is the record's type, 0 where
/// the deployment holds none.
pub fn as_listed(recorded: InstrumentType, underlying: i32) -> InstrumentType {
    if recorded == InstrumentType::ListedOption && underlying == InstrumentType::Future as i32 {
        InstrumentType::FutureOption
    } else {
        recorded
    }
}

/// A series key as the window's listing is matched by: its scheme and
/// value, and for a ticker its venue's MIC in `source`, as
/// [`meridian_domain::lake::series_asked`] matches one.
pub fn series_named(identifier: &meridian_domain::v1::Identifier) -> String {
    if identifier.scheme == meridian_domain::lake::TICKER_SCHEME {
        format!(
            "{}\u{1f}{}\u{1f}{}",
            identifier.scheme,
            identifier.value,
            identifier.source.to_ascii_uppercase()
        )
    } else {
        format!("{}\u{1f}{}", identifier.scheme, identifier.value)
    }
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
    /// A rate read's tenors (contract v27); empty for every tenor.
    pub tenors: Vec<Tenor>,
    /// A rate read's published-time cut-off (contract v27); 0 for none.
    pub published_as_of_ns: i64,
    /// Contract v28 (ruling 561): the series asked for by key, and whether
    /// every series of the subjects is; neither, and a row keyed by a series
    /// is left out, so a v27 reader of an underlying never receives its
    /// chain.
    pub series_keys: Vec<meridian_domain::v1::Identifier>,
    pub all_series: bool,
    /// Contract v28 (ListStatistics): business dates from and to,
    /// inclusive; `None` for no range. A corporate action's are its kind's
    /// date ([`Observation::date`]).
    pub dates: Option<(Date, Date)>,
    /// Contract v28 (the filings slice, W10.6): each subject asked with the
    /// role it must hold on a row, where the read gives one; none for a
    /// subject in any role.
    pub roles: Vec<(String, i32)>,
    /// Contract v28 (the filings slice, W10.6): what else the read keeps.
    pub narrowed: Narrowed,
    /// Contract v28, amended 2026-10-28 (ruling 821): the expiration window
    /// `all_series` is narrowed by. A listed series' own row is kept by its
    /// own expiration ([`expires`]); any other row keyed by a series by
    /// `listed`.
    pub window: Window,
    /// The series the lake's listing holds expiring in `window`, as
    /// [`series_named`] names them; `None` where no window is asked. A row
    /// about a series the listing does not hold is answered only to a read
    /// naming it by key.
    pub listed: Option<BTreeSet<String>>,
    /// Contract v28, amended 2026-10-28 (ruling 821): the dictionary entries
    /// of the row's message to answer; none for every field. The envelope
    /// is always answered, and a field not asked is left unset without being
    /// named unanswered.
    pub fields: BTreeSet<String>,
}

/// Whether a read keeps a row by its series (contract v28, W10.6, amended
/// 2026-10-28): asked by key or with `all_series`, and within the window
/// where one is asked.
pub fn series_kept(read: &Read, row: &Observation) -> bool {
    let key = &row.meta().series_key;
    if !meridian_domain::lake::series_asked(key, &read.series_keys, read.all_series) {
        return false;
    }
    if !read.window.is_set() || key.is_empty() {
        return true;
    }
    match row {
        Observation::Series(series) => expires(series).is_some_and(|date| read.window.holds(&date)),
        _ => read.listed.as_ref().is_some_and(|listed| {
            key.iter()
                .any(|identifier| listed.contains(&series_named(identifier)))
        }),
    }
}

/// A row stripped as its reader may read it and as its read asked (W10.1;
/// contract v28, amended 2026-10-28): each field the reader may not read
/// that the read asked for, or every one where it asked for none, returned
/// to be named NOT_ENTITLED; then each field not asked, unset and not named.
fn strip_for(row: &mut Observation, allowed: &BTreeSet<String>, read: &Read) -> Vec<&'static str> {
    let mut removed = row.strip(allowed);
    if !read.fields.is_empty() {
        removed.retain(|field| read.fields.contains(*field));
        let mut asked = read.fields.clone();
        if let Some(meta) = meridian_domain::lake::fields_of(read.data_type.name()).first() {
            asked.insert((*meta).to_string());
        }
        row.strip(&asked);
    }
    removed
}

/// A row read again for a held answer's page, stripped as the read stripped
/// it when it was answered: each field its reader may not read, and each
/// field the read did not ask for.
pub fn strip_answered(row: &mut Observation, read: &Read, against: &Against<'_>) {
    let allowed = against
        .config
        .fields_for(row.dataset(), &read.reader)
        .unwrap_or_default();
    strip_for(row, &allowed, read);
}

/// What a read of the filings slice keeps besides its subjects, kinds and
/// dates (contract v28, W10.6), before a page is cut: a reported fact's
/// concepts, none for every concept; and a holding's reports, by dataset
/// and row key, `None` for any report -- how ListHoldings narrows holdings by
/// their report's kind and date.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Narrowed {
    pub concepts: BTreeSet<String>,
    pub reports: Option<BTreeSet<(String, String)>>,
}

/// What a source declined, by dataset, subject and the date or range its
/// want asked ([`asked`]): its reason and when (contract v19).
pub type Declined = BTreeMap<(String, String, String), (UnansweredReason, i64)>;

/// What a read is answered against besides the store.
pub struct Against<'a> {
    pub config: &'a DataConfig,
    pub priorities: &'a [SourcePriority],
    /// What a source declined, by dataset, subject and the date or range
    /// its want asked ([`asked`]): its reason and when.
    pub declined: &'a Declined,
    /// Each replaced record and the one that stays (W10.8).
    pub aliases: &'a BTreeMap<String, String>,
    pub now_ns: i64,
}

/// How long a source's decline answers for it before it is asked again.
pub const DECLINE_STANDS_NS: i64 = NS_PER_DAY;

/// What a want asked, as a decline is kept by (contract v19, approved
/// 2026-10-10): its business date, its range, or the latest at a time.
pub fn asked(when: &When) -> String {
    match when {
        When::BusinessDate(date) => format!("date {date}"),
        When::Range { from_ns, until_ns } => format!("range {from_ns} {until_ns}"),
        When::Latest { at_ns } => format!("latest {at_ns}"),
        When::After(_) => "after".to_string(),
    }
}

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
    by_priority(read.data_type, kind, against, readable)
}

/// The datasets readable, in the deployment's source priority for a data
/// type and kind (W10.2): what a default read answers from first. Every
/// readable one, as listed, where no priority is set.
pub fn by_priority(
    data_type: DataType,
    kind: i32,
    against: &Against<'_>,
    readable: &[String],
) -> Vec<String> {
    // Only a price's priority names a kind (a rate's names none, W10.3).
    let kind = if data_type == DataType::Price {
        kind
    } else {
        0
    };
    let set = against
        .priorities
        .iter()
        .find(|p| p.data_type == data_type.name() && p.kind == kind)
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

/// Whether a dataset's latest row for a subject, recorded then, is older
/// than its cadence: silent (W10.2).
fn silent_at(dataset: &str, recorded_at_ns: i64, against: &Against<'_>) -> bool {
    let cadence = against
        .config
        .declaration(dataset)
        .map_or(0, |d| i64::from(d.cadence));
    cadence > 0 && recorded_at_ns < against.now_ns - cadence * 1_000_000_000
}

/// A fixed zone's offset from UTC, where the zone never moves its clocks:
/// UTC and its names, and the `Etc/GMT±N` zones (whose sign is the POSIX
/// one, inverted). `None` for a zone that keeps daylight saving, whose
/// offset only a time zone database knows, and the kernel holds none.
fn fixed_offset_ns(zone: &str) -> Option<i64> {
    const HOUR: i64 = 3_600 * 1_000_000_000;
    match zone {
        "" | "UTC" | "Etc/UTC" | "Etc/GMT" | "GMT" | "Etc/Universal" | "Universal" | "Etc/Zulu"
        | "Zulu" | "Etc/UCT" | "UCT" | "Etc/GMT0" | "Etc/GMT+0" | "Etc/GMT-0" | "GMT0"
        | "Etc/Greenwich" | "Greenwich" => Some(0),
        _ => {
            let rest = zone.strip_prefix("Etc/GMT")?;
            let hours: i64 = rest.parse().ok()?;
            Some(-hours * HOUR)
        }
    }
}

/// A range of trades or quotes held to one day of a dataset as it declares
/// its day (contract v19, W10.6; ruled 2026-10-09): its time zone and the
/// minute its day ends, never a calendar. Where the zone's offset is fixed
/// the range lies within one of its days; where the zone keeps daylight
/// saving, which the kernel cannot place, it is at most one day long, the
/// longest such day (25 hours). `until_ns` is exclusive. The refusal names
/// the range and the day.
pub fn within_a_day(
    dataset: &str,
    declaration: Option<&meridian_pb::v1::DatasetDeclaration>,
    from_ns: i64,
    until_ns: i64,
) -> Result<(), String> {
    let zone = declaration.map_or("", |d| d.day_time_zone.as_str());
    let end_minute = declaration.map_or(0, |d| i64::from(d.day_end_minute));
    let said = || {
        format!(
            "valid_from_ns to valid_until_ns spans more than one day of {dataset} as it declares \
             its day ({}, ending at minute {end_minute}); trades and quotes are read a day at a time",
            if zone.is_empty() { "Etc/UTC" } else { zone }
        )
    };
    if until_ns <= from_ns {
        return Err("valid_until_ns is after valid_from_ns".into());
    }
    match fixed_offset_ns(zone) {
        Some(offset) => {
            let day_of =
                |ns: i64| (ns + offset - end_minute * 60 * 1_000_000_000).div_euclid(NS_PER_DAY);
            if day_of(from_ns) != day_of(until_ns - 1) {
                return Err(said());
            }
        }
        None => {
            if until_ns - from_ns > NS_PER_DAY + NS_PER_DAY / 24 {
                return Err(said());
            }
        }
    }
    Ok(())
}

/// Why a dataset holds nothing for a subject, and whether to ask it.
fn why_not(
    read: &Read,
    against: &Against<'_>,
    dataset: &str,
    subject: &str,
) -> (UnansweredReason, bool) {
    let config = against.config;
    if let Some((reason, at)) =
        against
            .declined
            .get(&(dataset.to_string(), subject.to_string(), asked(&read.when)))
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

/// The latest in force per subject, dataset, kind, venue, length and asset,
/// and a rate's point and convention: what a read of the latest at a valid
/// time answers.
fn latest_in_force(rows: Vec<Observation>) -> Vec<Observation> {
    type Key = (String, String, i32, String, i64, String, String);
    let mut chosen: BTreeMap<Key, Observation> = BTreeMap::new();
    for row in rows {
        let key = (
            row.subject().to_string(),
            row.dataset().to_string(),
            row.kind(),
            row.venue().to_string(),
            row.interval_ns(),
            row.asset(),
            row.point(),
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

/// When a source published a row: its published source time, or where the
/// source states none, when the lake recorded it (contract v27, W10.4).
pub fn published_at(row: &Observation) -> i64 {
    row.meta()
        .source_times
        .iter()
        .find(|time| time.kind == SourceTimeKind::Published as i32)
        .map_or(row.meta().recorded_at_ns, |time| time.at_ns)
}

/// Each row key at the version in force by published time (contract v27,
/// W10.6; shared additions 1 and 2): of the versions given -- every one
/// recorded by the recorded-time cut -- the latest published by
/// `published_as_of_ns`, versions ordered by published time, then by
/// version. A row key nothing of which was published by then answers
/// nothing.
pub fn as_published(rows: Vec<Observation>, published_as_of_ns: i64) -> Vec<Observation> {
    let mut chosen: BTreeMap<(String, i16, String), Observation> = BTreeMap::new();
    for row in rows {
        let published = published_at(&row);
        if published > published_as_of_ns {
            continue;
        }
        let key = (
            row.dataset().to_string(),
            row.data_type().code(),
            row.meta().row_key.clone(),
        );
        let later = |held: &Observation| {
            (published, row.meta().version) > (published_at(held), held.meta().version)
        };
        match chosen.get(&key) {
            Some(held) if !later(held) => {}
            _ => {
                chosen.insert(key, row);
            }
        }
    }
    let mut out: Vec<Observation> = chosen.into_values().collect();
    out.sort_by_key(|row| (row.dataset().to_string(), row.meta().sequence));
    out
}

/// Contract v28 (W10.6; rulings 421 and 619): `include_quarantined` on a
/// read that names no dataset -- a default or a side-by-side read -- is
/// refused naming it.
pub fn quarantine_refused(sources: &SourceChoice) -> Result<(), String> {
    if sources.include_quarantined && sources.named.is_empty() {
        return Err(refusal_with_status(
            RefusalStatus::InvalidArgument,
            &["sources.include_quarantined".to_string()],
            "sources.include_quarantined is set only on a read naming its datasets; a default or \
             side-by-side read leaves invalid rows out",
        ));
    }
    Ok(())
}

/// Where a row of an answer is, and so how the page carrying it reads it.
#[derive(Debug, Clone)]
pub enum Place {
    /// A row the lake keeps, by its dataset and sequence: read again from
    /// the record with the page that carries it ([`Store::rows_at`]).
    Kept(Arc<str>, u64),
    /// A row held as it was read and stripped: one served and not kept
    /// (W10.7; ruling 734), which nothing could read again; or, while its
    /// answer holds no more than [`HELD_INLINE_MOST`], one the lake keeps.
    Held(Box<Observation>),
}

/// The most an answer holds of its rows themselves while it is chosen;
/// past it, every row the lake keeps is held by its [`Place`] alone, a few
/// dozen bytes a row. Found on dev 2026-11-09: a chain's statistics over
/// its dates, 49,412 rows in force, chosen from every row decoded, took
/// half a GiB a page, and the lake past its 1 GiB at the second reader. An
/// answer of a page or two -- what a live page reads, from memory -- is
/// held whole and read again from nowhere.
pub const HELD_INLINE_MOST: usize = 4 * 1024 * 1024;

/// A row as a read chooses among rows (W10.2, W10.6): where it is, and
/// what the choice and the answer's order read of it -- never the row
/// itself past [`HELD_INLINE_MOST`].
#[derive(Debug, Clone)]
pub struct Candidate {
    pub place: Place,
    /// Whether the store answered it, so its place reads it again.
    kept: bool,
    subject: Arc<str>,
    subjects: Arc<[Arc<str>]>,
    dataset: Arc<str>,
    kind: i32,
    series: Arc<str>,
    valid_from_ns: i64,
    sequence: u64,
    recorded_at_ns: i64,
    /// The fields stripped from it as its reader may read them.
    stripped: Box<[&'static str]>,
}

impl Candidate {
    pub fn subjects(&self) -> &[Arc<str>] {
        &self.subjects
    }

    pub fn dataset(&self) -> &str {
        &self.dataset
    }

    fn order(&self) -> (&str, &str, i32, i64, u64) {
        (
            &self.subject,
            &self.dataset,
            self.kind,
            self.valid_from_ns,
            self.sequence,
        )
    }
}

/// Each text an answer's candidates name, held once.
#[derive(Default)]
struct Names {
    texts: std::collections::HashSet<Arc<str>>,
    lists: BTreeMap<Vec<Arc<str>>, Arc<[Arc<str>]>>,
}

impl Names {
    fn of(&mut self, text: &str) -> Arc<str> {
        if let Some(held) = self.texts.get(text) {
            return Arc::clone(held);
        }
        let held: Arc<str> = Arc::from(text);
        self.texts.insert(Arc::clone(&held));
        held
    }

    fn list(&mut self, texts: Vec<String>) -> Arc<[Arc<str>]> {
        let named: Vec<Arc<str>> = texts.iter().map(|text| self.of(text)).collect();
        if let Some(held) = self.lists.get(&named) {
            return Arc::clone(held);
        }
        let held: Arc<[Arc<str>]> = Arc::from(named.clone());
        self.lists.insert(named, Arc::clone(&held));
        held
    }
}

/// What a row held whole costs, as a held answer counts it.
/// How much more a read holds before it grows its reservation: a quarter of
/// a page's worth, so a page's read takes a few reservations, not one a row.
const RESERVE_STEP: usize = 128 * 1024;

/// How many rows a read takes between looks at whether the shed line
/// cancelled it.
const CHECK_EVERY: u32 = 1024;

fn held_cost(row: &Observation) -> usize {
    crate::pages::row_bytes(row)
}

/// A read, chosen: its rows by their candidates, in the answer's order, and
/// what it could not answer and would ask.
#[derive(Debug, Clone, Default)]
pub struct Chosen {
    pub rows: Vec<Candidate>,
    pub not_served: Vec<NotServed>,
    pub asks: Vec<Ask>,
}

type LatestKey = (Arc<str>, Arc<str>, i32, Arc<str>, i64, Arc<str>, Arc<str>);

/// The rows a read is chosen from, gathered one at a time as the store
/// reads them: each narrowed as the read narrows it, then held as its
/// [`Candidate`] alone, so choosing among rows holds a few dozen bytes a
/// row, never every row read.
struct Gather<'a, 'b> {
    read: &'a Read,
    against: &'a Against<'b>,
    names: Names,
    /// Each subject and dataset a rate read held, and those holding a tenor
    /// asked (W10.4, a-tenor-not-published).
    tenor_seen: BTreeSet<(String, String)>,
    tenor_asked: BTreeSet<(String, String)>,
    /// Each subject, dataset and kind with a row left out invalid, and with
    /// one not (W10.6; rulings 421, 422 and 619).
    invalid_seen: BTreeSet<(String, String, i32)>,
    valid_seen: BTreeSet<(String, String, i32)>,
    /// A read of the latest: the latest in force per key so far.
    latest: Option<BTreeMap<LatestKey, Candidate>>,
    rows: Vec<Candidate>,
    held_bytes: usize,
    /// Past [`HELD_INLINE_MOST`]: every row kept is held by its place.
    placing: bool,
}

impl<'a, 'b> Gather<'a, 'b> {
    fn new(read: &'a Read, against: &'a Against<'b>) -> Self {
        Gather {
            read,
            against,
            names: Names::default(),
            tenor_seen: BTreeSet::new(),
            tenor_asked: BTreeSet::new(),
            invalid_seen: BTreeSet::new(),
            valid_seen: BTreeSet::new(),
            latest: matches!(read.when, When::Latest { .. }).then(BTreeMap::new),
            rows: Vec::new(),
            held_bytes: 0,
            placing: false,
        }
    }

    fn alias_of(&self, subject: &str) -> String {
        alias_of(self.read, self.against, subject)
    }

    /// One row read: narrowed, then held as its candidate.
    fn admit(&mut self, row: Observation) {
        let read = self.read;
        // A chain's rows only to a read that asks for series (contract v28,
        // W10.6): a listed series is always keyed by one, and read by ListSeries.
        if !series_kept(read, &row) {
            return;
        }
        if let Some((from, to)) = &read.dates {
            if !Date::parse(&row.date()).is_ok_and(|date| &date >= from && &date <= to) {
                return;
            }
        }
        // A fact's concepts, a holding's reports (contract v28, W10.6).
        if !read.narrowed.concepts.is_empty() {
            if let Observation::ReportedFact(fact) = &row {
                if !read.narrowed.concepts.contains(&fact.concept) {
                    return;
                }
            }
        }
        if let Some(reports) = &read.narrowed.reports {
            if let Observation::Holding(holding) = &row {
                if !reports.contains(&(row.dataset().to_string(), holding.report_row_key.clone())) {
                    return;
                }
            }
        }
        // A subject asked in a role (contract v28, W10.6): a row naming it in
        // another is not its; an unspecified role on a row reads as `subject`.
        if !read.roles.is_empty()
            && !row.meta().subjects.iter().any(|named| {
                let subject = self.alias_of(&named.entity_id);
                read.subjects.contains(&subject)
                    && read
                        .roles
                        .iter()
                        .all(|(entity, role)| entity != &subject || named.role.max(1) == *role)
            })
        {
            return;
        }
        // A rate read narrowed by tenor: a row with no tenor (a fixing, a
        // point named by maturity) is no tenor asked. A dataset that holds
        // the subject for the time asked and none of the tenors asked did
        // not publish them: source silent, never asked again (W10.4,
        // a-tenor-not-published).
        if !read.tenors.is_empty() {
            let held = (self.alias_of(row.subject()), row.dataset().to_string());
            self.tenor_seen.insert(held.clone());
            let asked = match &row {
                Observation::Rate(rate) => rate
                    .tenor
                    .as_ref()
                    .is_some_and(|tenor| read.tenors.contains(tenor)),
                _ => true,
            };
            if !asked {
                return;
            }
            self.tenor_asked.insert(held);
        }
        // Quarantine (contract v28, W10.6; rulings 421, 422 and 619): a row
        // whose effective state is invalid is left out before the one in
        // force is chosen, so a subject's last valid row stands, unless the
        // read names its datasets with `include_quarantined`.
        if !read.sources.include_quarantined {
            let held = (
                self.alias_of(row.subject()),
                row.dataset().to_string(),
                row.kind(),
            );
            if invalid(&row) {
                self.invalid_seen.insert(held);
                return;
            }
            self.valid_seen.insert(held);
        }
        let latest_key = self.latest.is_some().then(|| {
            (
                self.names.of(row.subject()),
                self.names.of(row.dataset()),
                row.kind(),
                self.names.of(row.venue()),
                row.interval_ns(),
                self.names.of(&row.asset()),
                self.names.of(&row.point()),
            )
        });
        let candidate = self.candidate(row, true);
        match (latest_key, self.latest.as_mut()) {
            (Some(key), Some(latest)) => {
                let later = |held: &Candidate| {
                    (candidate.valid_from_ns, candidate.sequence)
                        > (held.valid_from_ns, held.sequence)
                };
                match latest.get(&key) {
                    Some(held) if !later(held) => {
                        let gone = held_bytes_of(&candidate);
                        self.held_bytes -= gone;
                    }
                    _ => {
                        if let Some(replaced) = latest.insert(key, candidate) {
                            self.held_bytes -= held_bytes_of(&replaced);
                        }
                    }
                }
            }
            _ => self.rows.push(candidate),
        }
    }

    /// A row's candidate: what the choice reads of it, its fields stripped
    /// as its reader may read them (W10.1), and the row itself only while
    /// the answer is small.
    fn candidate(&mut self, mut row: Observation, kept: bool) -> Candidate {
        let subject = self.names.of(row.subject());
        let subjects = self.names.list(row.subjects());
        let dataset = self.names.of(row.dataset());
        let series = self.names.of(&row.series());
        let meta = row.meta();
        let (valid_from_ns, sequence, recorded_at_ns) =
            (meta.valid_from_ns, meta.sequence, meta.recorded_at_ns);
        let kind = row.kind();
        let allowed = self
            .against
            .config
            .fields_for(&dataset, &self.read.reader)
            .unwrap_or_default();
        let stripped = strip_for(&mut row, &allowed, self.read).into_boxed_slice();
        let cost = held_cost(&row);
        if kept && !self.placing && self.held_bytes + cost > HELD_INLINE_MOST {
            self.by_place();
        }
        let place = if !kept || !self.placing {
            self.held_bytes += cost;
            Place::Held(Box::new(row))
        } else {
            Place::Kept(Arc::clone(&dataset), sequence)
        };
        Candidate {
            place,
            kept,
            subject,
            subjects,
            dataset,
            kind,
            series,
            valid_from_ns,
            sequence,
            recorded_at_ns,
            stripped,
        }
    }

    /// The answer has passed [`HELD_INLINE_MOST`]: every row the lake keeps
    /// held so far is held by its place from now.
    fn by_place(&mut self) {
        self.placing = true;
        let mut placed = |candidate: &mut Candidate| {
            if candidate.kept && matches!(candidate.place, Place::Held(_)) {
                candidate.place = Place::Kept(Arc::clone(&candidate.dataset), candidate.sequence);
            }
        };
        self.rows.iter_mut().for_each(&mut placed);
        if let Some(latest) = self.latest.as_mut() {
            latest.values_mut().for_each(&mut placed);
        }
        self.held_bytes = self
            .rows
            .iter()
            .chain(self.latest.iter().flat_map(|latest| latest.values()))
            .map(held_bytes_of)
            .sum();
    }
}

/// What a candidate holds of its row.
fn held_bytes_of(candidate: &Candidate) -> usize {
    match &candidate.place {
        Place::Held(row) => held_cost(row),
        Place::Kept(..) => 0,
    }
}

fn alias_of(read: &Read, against: &Against<'_>, subject: &str) -> String {
    against
        .aliases
        .get(subject)
        .filter(|stays| read.subjects.contains(stays))
        .cloned()
        .unwrap_or_else(|| subject.to_string())
}

/// Answer a read from the store, as the reader may read it: every row
/// chosen, read whole. A paged read takes [`choose`] and reads its page's
/// rows alone ([`rows_of`]).
pub fn answer(store: &dyn Store, read: &Read, against: &Against<'_>) -> Result<Answered, String> {
    let chosen = choose(store, read, against)?;
    let places: Vec<Place> = chosen.rows.into_iter().map(|c| c.place).collect();
    let rows = rows_of(store, read, against, &places)?
        .ok_or("a row the answer chose is no longer kept; read it again")?;
    Ok(Answered {
        rows,
        not_served: chosen.not_served,
        asks: chosen.asks,
    })
}

/// The rows at these places, in order: each the lake keeps read again from
/// the record as the read answered it -- its flag and quality as of the
/// read's cut-off ([`Store::rows_at`]), its fields stripped as its reader
/// may read them and as it asked -- and each held, as held. `None` where a
/// row kept is no longer (retention removed it since it was chosen).
pub fn rows_of(
    store: &dyn Store,
    read: &Read,
    against: &Against<'_>,
    places: &[Place],
) -> Result<Option<Vec<Observation>>, String> {
    let kept: Vec<(String, u64)> = places
        .iter()
        .filter_map(|place| match place {
            Place::Kept(dataset, sequence) => Some((dataset.to_string(), *sequence)),
            Place::Held(_) => None,
        })
        .collect();
    let mut found = if kept.is_empty() {
        Vec::new()
    } else {
        store
            .rows_at(read.data_type, &kept, read.as_of_ns)
            .map_err(|failed| failed.to_string())?
    }
    .into_iter();
    if found.len() != kept.len() {
        return Ok(None);
    }
    let mut rows = Vec::with_capacity(places.len());
    for place in places {
        match place {
            Place::Kept(..) => {
                let mut row = found.next().expect("counted");
                strip_answered(&mut row, read, against);
                rows.push(row);
            }
            Place::Held(row) => rows.push((**row).clone()),
        }
    }
    Ok(Some(rows))
}

/// Choose a read's answer from the store, as the reader may read it: each
/// row as its [`Candidate`], gathered as the store reads it, so what is held
/// while choosing is a few dozen bytes a row past [`HELD_INLINE_MOST`].
pub fn choose(store: &dyn Store, read: &Read, against: &Against<'_>) -> Result<Chosen, String> {
    quarantine_refused(&read.sources)?;
    let config = against.config;
    let mut out = Chosen::default();
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
    let alias_of = |subject: &str| -> String { alias_of(read, against, subject) };

    let query = Query {
        datasets: candidates.clone(),
        subjects: asked,
        data_type: read.data_type,
        kinds: read.kinds.clone(),
        interval_ns: read.interval_ns,
        when: read.when.clone(),
        as_of_ns: read.as_of_ns,
        published_as_of_ns: read.published_as_of_ns,
        // Only where nothing narrows the rows before the latest is
        // chosen but what commutes with it -- a series and a tenor are
        // in the point, an invalid row's successor is answered beside
        // it -- may the store choose it.
        latest_only: matches!(read.when, When::Latest { .. })
            && read.published_as_of_ns == 0
            && read.dates.is_none()
            && read.roles.is_empty()
            && read.narrowed == Narrowed::default(),
    };
    let mut gather = Gather::new(read, against);
    if read.published_as_of_ns != 0 {
        // A rate read as published by a time: each row key's version in
        // force by then, chosen of every version read.
        let rows = store.read(&query).map_err(|failed| failed.to_string())?;
        for row in as_published(rows, read.published_as_of_ns) {
            gather.admit(row);
        }
    } else {
        // Its working memory reserved from the rows as it holds them (stage
        // M; the resilience spec's section 10a): a read the budget has no
        // room for, or one the shed line cancelled, stops reading and is
        // refused for load, never grown past the budget.
        let mut refused = None;
        let mut reserved = 0usize;
        let mut since_check = 0u32;
        let stopped = store.read_each(&query, &mut |row| {
            gather.admit(row);
            since_check += 1;
            let held = gather.held_bytes;
            let over = if held >= reserved + RESERVE_STEP {
                let grown = meridian_budget::grow((held - reserved) as u64);
                reserved = held;
                grown.err()
            } else if since_check >= CHECK_EVERY {
                since_check = 0;
                meridian_budget::check().err()
            } else {
                None
            };
            match over {
                None => Ok(()),
                Some(over) => {
                    refused = Some(over);
                    Err(crate::store::StoreError::Unavailable(
                        "refused for load".into(),
                    ))
                }
            }
        });
        if let Some(over) = refused {
            return Err(meridian_bus::overloaded(&over));
        }
        stopped.map_err(|failed| failed.to_string())?;
    }
    let Gather {
        tenor_seen,
        tenor_asked,
        invalid_seen,
        valid_seen,
        latest,
        mut rows,
        ..
    } = gather;
    let tenor_silent: BTreeSet<(String, String)> =
        tenor_seen.difference(&tenor_asked).cloned().collect();
    // A dataset left with nothing for a subject and kind is source silent:
    // asked nothing, and never fallen back to.
    let left_out: BTreeSet<(String, String, i32)> =
        invalid_seen.difference(&valid_seen).cloned().collect();
    let why_not = |dataset: &str, subject: &str, kind: i32| {
        if left_out.contains(&(subject.to_string(), dataset.to_string(), kind)) {
            // Contract v29 (619 (1); W10.6): a dataset whose only rows for
            // the subject are invalid is named quarantined, apart from a
            // silent one; still asked nothing and never fallen back to.
            (UnansweredReason::Quarantined, false)
        } else if tenor_silent.contains(&(subject.to_string(), dataset.to_string())) {
            (UnansweredReason::SourceSilent, false)
        } else {
            why_not(read, against, dataset, subject)
        }
    };
    if let Some(latest) = latest {
        rows = latest.into_values().collect();
    }

    // By the subject asked about, then kind. A row of the filings slice
    // names its subjects in roles -- a corporate action its security and
    // its issuer, a holding its holder and what is held, a rating action
    // the rated and the rater (contract v28, W10.4) -- and is the answer to
    // whichever of them was asked first, once.
    let mut by_subject: BTreeMap<(String, i32), Vec<Candidate>> = BTreeMap::new();
    for row in rows {
        let subject = if read.data_type.filings() {
            row.subjects
                .iter()
                .map(|s| alias_of(s))
                .find(|s| read.subjects.contains(s))
                .unwrap_or_else(|| alias_of(&row.subject))
        } else {
            alias_of(&row.subject)
        };
        by_subject.entry((subject, row.kind)).or_default().push(row);
    }
    let kinds_asked: Vec<i32> = if read.kinds.is_empty() {
        vec![]
    } else {
        read.kinds.clone()
    };

    let mut asks: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut chosen: Vec<Candidate> = Vec::new();
    let catching_up = matches!(read.when, When::After(_));
    let series_asked = read.all_series || !read.series_keys.is_empty();
    let silent = |row: &Candidate| silent_at(&row.dataset, row.recorded_at_ns, against);
    // How many times each subject is still to be answered: its rows move
    // into the answer at its last, and are copied only for one asked twice.
    let mut still: BTreeMap<&String, usize> = BTreeMap::new();
    for subject in &read.subjects {
        *still.entry(subject).or_default() += 1;
    }
    for subject in &read.subjects {
        let last = still.get_mut(subject).is_some_and(|left| {
            *left -= 1;
            *left == 0
        });
        // The kinds to answer: those asked, or those any dataset holds; a
        // bar's is 0.
        let mut kinds: Vec<i32> = kinds_asked.clone();
        if kinds.is_empty() {
            let mut found: BTreeSet<i32> = by_subject
                .keys()
                .filter(|(s, _)| s == subject)
                .map(|(_, k)| *k)
                .collect();
            // A kind whose every row was left out is answered too: source
            // silent where it was held.
            found.extend(
                left_out
                    .iter()
                    .filter(|(s, _, _)| s == subject)
                    .map(|(_, _, k)| *k),
            );
            kinds = found.into_iter().collect();
        }
        let nothing_held = kinds.is_empty();
        if nothing_held {
            kinds.push(0);
        }
        for kind in kinds {
            // Taken, never copied (ruling 959): each subject and kind is
            // chosen from once, and its rows move into the answer.
            let key = (subject.clone(), kind);
            let mut held = if last {
                by_subject.remove(&key)
            } else {
                by_subject.get(&key).cloned()
            }
            .unwrap_or_default();
            let order = if read.sources.side_by_side {
                candidates.clone()
            } else {
                order_for(read, against, kind, &candidates)
            };
            if catching_up {
                // A catch-up answers what was recorded since, from every
                // partition named, and asks nothing of a source.
                chosen.extend(held);
                continue;
            }
            if read.sources.side_by_side {
                for dataset in &order {
                    let (of, rest): (Vec<Candidate>, Vec<Candidate>) = std::mem::take(&mut held)
                        .into_iter()
                        .partition(|r| &*r.dataset == dataset);
                    held = rest;
                    if of.is_empty() {
                        let (reason, ask) = why_not(dataset, subject, kind);
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
                        chosen.extend(of);
                    }
                }
                continue;
            }
            // Default or named: the first dataset in order with a row that is
            // not silent; each above it said, with why. A read that asks for
            // series chooses per series (contract v28, W10.6): an
            // underlying's own quote in one dataset and its chain's in
            // another each answer, one source per series, never mixed
            // within one. A dataset holding the subject's other series is
            // not named for this one; it answered what it holds.
            let holding: BTreeSet<Arc<str>> = held.iter().map(|r| Arc::clone(&r.dataset)).collect();
            let groups: Vec<Vec<Candidate>> = if series_asked && !held.is_empty() {
                let mut by_series: BTreeMap<Arc<str>, Vec<Candidate>> = BTreeMap::new();
                for row in held {
                    by_series
                        .entry(Arc::clone(&row.series))
                        .or_default()
                        .push(row);
                }
                by_series.into_values().collect()
            } else {
                vec![held]
            };
            // One instance is asked at a time per subject and kind: the
            // first that can be.
            let mut asked_of: Option<String> = None;
            let mut fallen: Vec<NotServed> = Vec::new();
            for mut group in groups {
                let mut answered = false;
                let mut first_silent: Option<Vec<Candidate>> = None;
                for dataset in &order {
                    let (of, rest): (Vec<Candidate>, Vec<Candidate>) = std::mem::take(&mut group)
                        .into_iter()
                        .partition(|r| &*r.dataset == dataset);
                    group = rest;
                    let latest_silent = !of.is_empty()
                        && matches!(read.when, When::Latest { .. })
                        && of.iter().all(silent);
                    if of.is_empty() && holding.contains(dataset.as_str()) {
                        continue;
                    }
                    if of.is_empty() || latest_silent {
                        let (reason, ask) = why_not(dataset, subject, kind);
                        let reason = if latest_silent {
                            // Only rows older than the dataset's cadence:
                            // its source is asked again as when nothing is
                            // held, which keeps the want current -- the
                            // lake's wants live in memory, and a chain read
                            // after a restart froze. A re-ask, not the one
                            // instance a read falls through to: the next
                            // dataset is still asked where it holds nothing.
                            if ask {
                                asks.entry(dataset.clone())
                                    .or_default()
                                    .insert(subject.clone());
                                UnansweredReason::AskedSource
                            } else if reason == UnansweredReason::Quarantined {
                                reason
                            } else {
                                UnansweredReason::SourceSilent
                            }
                        } else if ask {
                            // One instance is asked at a time: the first
                            // that can be.
                            match &asked_of {
                                None => {
                                    asks.entry(dataset.clone())
                                        .or_default()
                                        .insert(subject.clone());
                                    asked_of = Some(dataset.clone());
                                    reason
                                }
                                Some(asked) if asked == dataset => reason,
                                Some(_) => UnansweredReason::NotCovered,
                            }
                        } else {
                            reason
                        };
                        let said = NotServed {
                            subject: subject.clone(),
                            dataset: dataset.clone(),
                            field: String::new(),
                            reason,
                        };
                        if !fallen.contains(&said) {
                            fallen.push(said);
                        }
                        if latest_silent {
                            first_silent.get_or_insert(of);
                        }
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
            }
            out.not_served.extend(fallen);
        }
    }

    // Fields the reader may not read, stripped and named once per dataset.
    let mut named_fields: BTreeSet<(String, String)> = BTreeSet::new();
    for row in &chosen {
        for field in row.stripped.iter() {
            if named_fields.insert((row.dataset.to_string(), field.to_string())) {
                out.not_served.push(NotServed {
                    subject: String::new(),
                    dataset: row.dataset.to_string(),
                    field: field.to_string(),
                    reason: UnansweredReason::NotEntitled,
                });
            }
        }
    }
    chosen.sort_by(|a, b| a.order().cmp(&b.order()));
    // Held at what it holds, not at the room its vector grew to.
    chosen.shrink_to_fit();
    out.rows = chosen;

    // A pool's or a class's factor is wanted standing (contract v29, W10.7):
    // each month's is published on the agency's schedule.
    let standing = matches!(read.when, When::Latest { at_ns: 0 })
        || (read.data_type == DataType::Factor && !matches!(read.when, When::After(_)));
    for (dataset, subjects) in asks {
        let cadence = config.declaration(&dataset).map_or(0, |d| d.cadence);
        // Kept current: by its schedule where it has a cadence, by the
        // vendor's stream where it is streamed (contract v19, W10.7).
        // A dataset served, not kept, is never kept current: each read asks
        // it again and hears what is served (ruling 734).
        let current = (cadence > 0 || config.has_mode(&dataset, ObservationMode::Stream))
            && config.licence(&dataset).kept;
        out.asks.push(Ask {
            dataset,
            data_type: read.data_type,
            subjects: subjects.into_iter().collect(),
            // A want names prices' kinds alone (W10.7): a rate's want asks
            // for the subject, every kind and tenor the source publishes.
            kinds: if read.data_type == DataType::Price {
                read.kinds.clone()
            } else {
                Vec::new()
            },
            interval_ns: read.interval_ns,
            when: read.when.clone(),
            standing: standing && current,
            series_keys: read.series_keys.clone(),
            all_series: read.all_series,
            window: read.window.clone(),
        });
    }
    Ok(out)
}

/// What a dataset served, not kept, served the read that asked it, added
/// to the answer (W10.7, "the lake answers the waiting reader"; ruling
/// 734): narrowed and chosen as kept rows are -- the subjects, kinds and
/// series asked, an invalid row left out unless named with
/// `include_quarantined`, the latest per key for a read of the latest --
/// and its fields stripped as the reader may read them. A subject and kind
/// another dataset answered is never mixed with it but on a side-by-side
/// read. Every dataset served, not kept, that did not answer a subject is
/// named NOT_KEPT, never ASKED_SOURCE: nothing asked is kept to be read
/// again.
pub fn with_served(
    read: &Read,
    against: &Against<'_>,
    answered: &mut Chosen,
    served: BTreeMap<String, Vec<Observation>>,
) {
    let config = against.config;
    let alias_of = |subject: &str| -> String { alias_of(read, against, subject) };
    let mut held: BTreeSet<(String, i32)> = answered
        .rows
        .iter()
        .map(|row| (alias_of(&row.subject), row.kind))
        .collect();
    let mut gather = Gather::new(read, against);
    let mut added = false;
    for (dataset, rows) in served {
        let mut rows: Vec<Observation> = rows
            .into_iter()
            .filter(|row| row.data_type() == read.data_type)
            .filter(|row| read.subjects.contains(&alias_of(row.subject())))
            .filter(|row| read.kinds.is_empty() || read.kinds.contains(&row.kind()))
            .filter(|row| series_kept(read, row))
            .filter(|row| read.sources.include_quarantined || !invalid(row))
            .collect();
        if matches!(read.when, When::Latest { .. }) {
            rows = latest_in_force(rows);
        }
        let mut answering: BTreeSet<(String, i32)> = BTreeSet::new();
        for row in rows {
            let key = (alias_of(row.subject()), row.kind());
            if !read.sources.side_by_side && held.contains(&key) && !answering.contains(&key) {
                continue;
            }
            let candidate = gather.candidate(row, false);
            for field in candidate.stripped.iter() {
                let said = NotServed {
                    subject: String::new(),
                    dataset: dataset.clone(),
                    field: field.to_string(),
                    reason: UnansweredReason::NotEntitled,
                };
                if !answered.not_served.contains(&said) {
                    answered.not_served.push(said);
                }
            }
            answering.insert(key);
            answered.rows.push(candidate);
            added = true;
        }
        let subjects: BTreeSet<String> = answering.iter().map(|(s, _)| s.clone()).collect();
        answered.not_served.retain(|n| {
            !(n.dataset == dataset
                && n.reason == UnansweredReason::AskedSource
                && subjects.contains(&n.subject))
        });
        held.extend(answering);
    }
    for said in answered.not_served.iter_mut() {
        if said.reason == UnansweredReason::AskedSource && !config.licence(&said.dataset).kept {
            said.reason = UnansweredReason::NotKept;
        }
    }
    if added {
        answered.rows.sort_by(|a, b| a.order().cmp(&b.order()));
    }
}
