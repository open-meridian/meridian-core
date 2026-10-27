//! What a deployment counts of its licensed data use, and a person's answer
//! to the professional test (contract v28: W10.1, W10.13;
//! spec/every-data-source-is-licensed-on-the-same-terms, sections 2, 4 and
//! 5; rulings 377 to 389 and 754).
//!
//! **The counts are computable from records** (754): who *could* use each
//! dataset, never who was served a field. Per dataset and calendar month:
//!
//! - the **display persons entitled**: each person able to read it, that is
//!   holding read or write on a plugin instance entitled to it, as the
//!   records say (a login a user group names counts before it signs in);
//!   split into professional and non-professional where its terms split
//!   them, an unanswered person counting as professional;
//! - the **non-display plugins**, each instance entitled to it, by category,
//!   defaulted by role (the spec's section 4: an `ems` is
//!   `automated_trading`, every other role `internal`);
//! - the **service accounts** able to read it, on the non-display meter,
//!   never as a person;
//! - and the same again **per underlying source**, so two datasets carrying
//!   one source are counted once.
//!
//! The display persons *served*, and the devices, are counted from what the
//! sidecars serve, which is v30's contract (754): they are shown as "not
//! counted yet", never as a number.
//!
//! What was seen is kept per month with when it was first and last seen
//! (migration 11), each minute, so a month's counts are who could use the
//! dataset at any time in it, and the CSV export carries the report's
//! fields: who, their classification, activated and deactivated, the
//! quantity and the timeliness. Kept at least three years; never sent to the
//! platform and never joined to the plugin usage report.
//!
//! **The professional test** (section 2): a person's first access to a
//! plugin entitled to a dataset whose display terms split professional and
//! non-professional asks them once, on [`pages`]' form -- the standard text,
//! or the vendor's own where the terms say so, one test per dataset then.
//! The answer is its own record, `professional_status_set`; a deployment
//! admin may change it with a note. An agent inherits its person's status;
//! a service account has none.

pub mod pages;
pub mod store;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock, RwLock};

use meridian_access::person_access;
use meridian_domain::v1::{AccessRecords, ListDatasetsReply};
use meridian_pb::v1::{DatasetLicence, LicenceMeter, ProfessionalDefinition};

use crate::admin::data_sources::{self, Row};
use crate::web::App;
use store::{DataUseStore, InMemory, Seen, SeenRow, StatusChange};

/// A note's bound, as every change's.
pub const NOTE_MOST: usize = 2_000;

/// The test every dataset whose terms use the platform's text shares.
pub const STANDARD: &str = "standard";

/// A person's professional status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    Professional,
    NonProfessional,
}

impl Status {
    pub fn value(self) -> &'static str {
        match self {
            Status::Professional => "professional",
            Status::NonProfessional => "non_professional",
        }
    }

    pub fn parse(said: &str) -> Option<Status> {
        match said {
            "professional" => Some(Status::Professional),
            "non_professional" => Some(Status::NonProfessional),
            _ => None,
        }
    }

    pub fn words(self) -> &'static str {
        match self {
            Status::Professional => "Professional",
            Status::NonProfessional => "Non-professional",
        }
    }
}

/// What a seen row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Person,
    Plugin,
    ServiceAccount,
}

impl Kind {
    pub fn value(self) -> &'static str {
        match self {
            Kind::Person => "person",
            Kind::Plugin => "plugin",
            Kind::ServiceAccount => "service_account",
        }
    }

    pub fn parse(said: &str) -> Option<Kind> {
        match said {
            "person" => Some(Kind::Person),
            "plugin" => Some(Kind::Plugin),
            "service_account" => Some(Kind::ServiceAccount),
            _ => None,
        }
    }
}

// ── The store, and what is held in memory beside it ────────────────────────

static STORE: OnceLock<Arc<dyn DataUseStore>> = OnceLock::new();

/// Keep the answers and the counts in `store` from now on: the runtime's,
/// at start. Once.
pub fn keep_in(store: Arc<dyn DataUseStore>) {
    let _ = STORE.set(store);
}

pub(crate) fn store() -> Arc<dyn DataUseStore> {
    Arc::clone(STORE.get_or_init(|| Arc::new(InMemory::default())))
}

/// Which tests each plugin instance's entitlements ask of its readers, as
/// the last sweep or read of the datasets found them.
fn asked() -> &'static RwLock<BTreeMap<String, BTreeSet<String>>> {
    static HELD: OnceLock<RwLock<BTreeMap<String, BTreeSet<String>>>> = OnceLock::new();
    HELD.get_or_init(Default::default)
}

/// The answers standing, by person and test.
pub type Answers = BTreeMap<(String, String), Status>;

/// The answers standing, as last read or made here.
fn answered() -> &'static RwLock<Option<Answers>> {
    static HELD: OnceLock<RwLock<Option<Answers>>> = OnceLock::new();
    HELD.get_or_init(Default::default)
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce(&dyn DataUseStore) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let store = store();
    tokio::task::spawn_blocking(move || work(store.as_ref()))
        .await
        .map_err(|failed| format!("the data-use store's task: {failed}"))?
}

/// Every answer standing, by person and test: the cache, or the store.
pub async fn answers() -> Result<BTreeMap<(String, String), Status>, String> {
    if let Some(held) = answered().read().ok().and_then(|held| held.clone()) {
        return Ok(held);
    }
    let standing = blocking(|store| store.statuses()).await?;
    let read: BTreeMap<(String, String), Status> = standing
        .into_iter()
        .map(|s| ((s.subject, s.test), s.status))
        .collect();
    if let Ok(mut held) = answered().write() {
        *held = Some(read.clone());
    }
    Ok(read)
}

// ── What the terms ask ─────────────────────────────────────────────────────

/// The professional test a dataset's terms ask, or none where its display
/// terms do not split professional and non-professional: the standard
/// text, or the vendor's own, which is that dataset's.
pub fn test_of(dataset: &str, licence: &DatasetLicence) -> Option<String> {
    let terms = licence.display_terms.as_ref()?;
    if !terms.professional_split {
        return None;
    }
    if terms.professional_definition == ProfessionalDefinition::Vendor as i32 {
        Some(dataset.to_string())
    } else {
        Some(STANDARD.to_string())
    }
}

/// A plugin instance's non-display category by its roles (the spec's
/// section 4, Q4): an `ems` trades automatically; every other role --
/// `portfolio`, `oms`, `signal`, `operations`, `reporting`, `pricing`, the
/// `analytics` component and the rest -- is internal. Internalising and
/// crossing (`trading_platform`) and serving clients (`for_clients`) are
/// the deployment admin's to say, which no record carries yet.
pub fn category_of(roles: &[String]) -> &'static str {
    if roles.iter().any(|role| role == "ems") {
        "automated_trading"
    } else {
        "internal"
    }
}

/// A category as the page says it.
pub fn category_words(category: &str) -> &str {
    match category {
        "automated_trading" => "automated trading",
        "trading_platform" => "trading platform",
        "for_clients" => "for clients",
        "internal" => "internal",
        other => other,
    }
}

/// The meters a dataset's terms bill on, in their words.
pub fn meters_of(licence: &DatasetLicence) -> Vec<&'static str> {
    licence
        .meters
        .iter()
        .filter_map(|meter| match LicenceMeter::try_from(*meter) {
            Ok(LicenceMeter::Display) => Some("display"),
            Ok(LicenceMeter::NonDisplay) => Some("non_display"),
            Ok(LicenceMeter::Enterprise) => Some("enterprise"),
            _ => None,
        })
        .collect()
}

/// A dataset's underlying source: its terms', or who originated it.
pub fn source_of(row: &Row) -> String {
    if !row.licence.underlying_source.is_empty() {
        return row.licence.underlying_source.clone();
    }
    if !row.dataset.vendor.is_empty() {
        return row.dataset.vendor.clone();
    }
    row.dataset
        .declaration
        .as_ref()
        .map(|d| d.vendor.clone())
        .filter(|vendor| !vendor.is_empty())
        .unwrap_or_else(|| row.dataset.dataset.clone())
}

/// A dataset's timeliness as the report states it: its delay label, or
/// real time.
pub fn timeliness_of(licence: &DatasetLicence) -> String {
    if !licence.delay_label.is_empty() {
        licence.delay_label.clone()
    } else if licence.delay_minutes > 0 {
        format!("delayed {} minutes", licence.delay_minutes)
    } else {
        "real time".into()
    }
}

// ── What is seen now ───────────────────────────────────────────────────────

/// Who could use each dataset now, from the records and the entitlements:
/// what a sweep keeps for the month.
pub fn seen_now(
    rows: &[Row],
    records: &AccessRecords,
    answers: &BTreeMap<(String, String), Status>,
) -> Vec<Seen> {
    let roles: BTreeMap<&str, &[String]> = records
        .known_plugins
        .iter()
        .map(|p| (p.plugin_instance_id.as_str(), p.roles.as_slice()))
        .collect();
    let mut seen = Vec::new();
    for row in rows {
        let dataset = &row.dataset.dataset;
        let source = source_of(row);
        let test = test_of(dataset, &row.licence);
        let mut readers: BTreeSet<String> = BTreeSet::new();
        for entitlement in &row.entitled {
            readers.extend(data_sources::readers_of(records, &entitlement.instance));
            seen.push(Seen {
                dataset: dataset.clone(),
                kind: Kind::Plugin,
                who: entitlement.instance.clone(),
                classification: category_of(
                    roles
                        .get(entitlement.instance.as_str())
                        .copied()
                        .unwrap_or_default(),
                )
                .to_string(),
                underlying_source: source.clone(),
            });
        }
        for reader in readers {
            if crate::service_accounts::is_service_account(&reader) {
                seen.push(Seen {
                    dataset: dataset.clone(),
                    kind: Kind::ServiceAccount,
                    who: reader,
                    classification: String::new(),
                    underlying_source: source.clone(),
                });
                continue;
            }
            let classification = match &test {
                None => String::new(),
                Some(test) => answers
                    .get(&(reader.clone(), test.clone()))
                    .copied()
                    .unwrap_or(Status::Professional)
                    .value()
                    .to_string(),
            };
            seen.push(Seen {
                dataset: dataset.clone(),
                kind: Kind::Person,
                who: reader,
                classification,
                underlying_source: source.clone(),
            });
        }
    }
    seen
}

/// The month a time falls in, `YYYY-MM`, in UTC.
pub fn month_of(ns: i64) -> String {
    crate::custody::utc(ns).chars().take(7).collect()
}

/// A month's rows as kept, with what is seen now laid over them when the
/// month is the current one: so the page counts now even before the next
/// sweep, and never counts anybody twice.
pub fn merged(kept: Vec<SeenRow>, now: &[Seen], now_ns: i64) -> Vec<SeenRow> {
    let mut by_key: BTreeMap<(String, Kind, String), SeenRow> = kept
        .into_iter()
        .map(|row| {
            (
                (
                    row.seen.dataset.clone(),
                    row.seen.kind,
                    row.seen.who.clone(),
                ),
                row,
            )
        })
        .collect();
    for seen in now {
        let key = (seen.dataset.clone(), seen.kind, seen.who.clone());
        by_key
            .entry(key)
            .and_modify(|row| {
                row.last_seen_ns = row.last_seen_ns.max(now_ns);
                row.seen.classification = seen.classification.clone();
                row.seen.underlying_source = seen.underlying_source.clone();
            })
            .or_insert_with(|| SeenRow {
                seen: seen.clone(),
                first_seen_ns: now_ns,
                last_seen_ns: now_ns,
            });
    }
    by_key.into_values().collect()
}

// ── Counting ───────────────────────────────────────────────────────────────

/// One dataset's counts in one month, or one underlying source's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    /// The dataset, or the underlying source when counted by source.
    pub key: String,
    /// The underlying source (by dataset), or the datasets carrying it (by
    /// source).
    pub beside: String,
    /// The meters its terms bill on, now; empty for none or not known.
    pub meters: Vec<&'static str>,
    /// Whether its display terms split professional and non-professional.
    pub split: bool,
    pub timeliness: String,
    /// Display persons able to read it.
    pub persons: usize,
    pub professional: usize,
    pub non_professional: usize,
    /// Plugin instances entitled, by category.
    pub plugins: BTreeMap<String, usize>,
    pub service_accounts: usize,
}

impl Counts {
    pub fn plugins_total(&self) -> usize {
        self.plugins.values().sum()
    }

    /// The plugins by category in words: `2 internal, 1 automated trading`.
    pub fn plugins_said(&self) -> String {
        if self.plugins.is_empty() {
            return "0".into();
        }
        self.plugins
            .iter()
            .map(|(category, n)| format!("{n} {}", category_words(category)))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// A month's counts per dataset, with each dataset's terms as listed now.
pub fn by_dataset(rows: &[SeenRow], listed: &[Row]) -> Vec<Counts> {
    let terms: BTreeMap<&str, &Row> = listed
        .iter()
        .map(|row| (row.dataset.dataset.as_str(), row))
        .collect();
    let mut keys: BTreeSet<String> = rows.iter().map(|r| r.seen.dataset.clone()).collect();
    keys.extend(listed.iter().map(|row| row.dataset.dataset.clone()));
    keys.into_iter()
        .map(|dataset| {
            let mine: Vec<&SeenRow> = rows.iter().filter(|r| r.seen.dataset == dataset).collect();
            let mut counts = count(&mine);
            counts.key = dataset.clone();
            match terms.get(dataset.as_str()) {
                Some(row) => {
                    counts.beside = source_of(row);
                    counts.meters = meters_of(&row.licence);
                    counts.split = test_of(&dataset, &row.licence).is_some();
                    counts.timeliness = timeliness_of(&row.licence);
                }
                None => {
                    counts.beside = mine
                        .first()
                        .map(|r| r.seen.underlying_source.clone())
                        .unwrap_or_default();
                    counts.split = mine
                        .iter()
                        .any(|r| r.seen.kind == Kind::Person && !r.seen.classification.is_empty());
                }
            }
            counts
        })
        .collect()
}

/// A month's counts per underlying source (Q11): each person, plugin and
/// service account once however many of the source's datasets it could
/// use.
pub fn by_source(rows: &[SeenRow], listed: &[Row]) -> Vec<Counts> {
    let mut datasets_of: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for row in rows {
        datasets_of
            .entry(row.seen.underlying_source.clone())
            .or_default()
            .insert(row.seen.dataset.clone());
    }
    for row in listed {
        datasets_of
            .entry(source_of(row))
            .or_default()
            .insert(row.dataset.dataset.clone());
    }
    datasets_of
        .into_iter()
        .map(|(source, datasets)| {
            // One row per who: a person's status is professional where any
            // of the source's tests says so.
            let mut once: BTreeMap<(Kind, String), SeenRow> = BTreeMap::new();
            for row in rows.iter().filter(|r| r.seen.underlying_source == source) {
                once.entry((row.seen.kind, row.seen.who.clone()))
                    .and_modify(|held| {
                        if row.seen.classification == Status::Professional.value() {
                            held.seen.classification = row.seen.classification.clone();
                        }
                    })
                    .or_insert_with(|| row.clone());
            }
            let held: Vec<&SeenRow> = once.values().collect();
            let mut counts = count(&held);
            counts.key = source;
            counts.beside = datasets.into_iter().collect::<Vec<_>>().join(", ");
            let theirs: Vec<&Row> = listed
                .iter()
                .filter(|row| source_of(row) == counts.key)
                .collect();
            let mut meters: BTreeSet<&'static str> = BTreeSet::new();
            for row in &theirs {
                meters.extend(meters_of(&row.licence));
                counts.split |= test_of(&row.dataset.dataset, &row.licence).is_some();
            }
            counts.meters = meters.into_iter().collect();
            counts
        })
        .collect()
}

fn count(rows: &[&SeenRow]) -> Counts {
    let mut counts = Counts::default();
    for row in rows {
        match row.seen.kind {
            Kind::Person => {
                counts.persons += 1;
                if row.seen.classification == Status::NonProfessional.value() {
                    counts.non_professional += 1;
                } else {
                    counts.professional += 1;
                }
            }
            Kind::Plugin => {
                *counts
                    .plugins
                    .entry(row.seen.classification.clone())
                    .or_default() += 1;
            }
            Kind::ServiceAccount => counts.service_accounts += 1,
        }
    }
    counts
}

/// The CSV the deployment admin files (the spec's section 1e and 4): one
/// line per dataset and who, in the report's fields. `name` says a
/// person's name from their subject.
pub fn csv(month: &str, rows: &[SeenRow], listed: &[Row], name: impl Fn(&str) -> String) -> String {
    let terms: BTreeMap<&str, &Row> = listed
        .iter()
        .map(|row| (row.dataset.dataset.as_str(), row))
        .collect();
    let mut out = String::from(
        "month,dataset,underlying_source,meter,kind,who,name,classification,activated,deactivated,quantity,timeliness\n",
    );
    let mut sorted: Vec<&SeenRow> = rows.iter().collect();
    sorted.sort_by(|a, b| {
        (&a.seen.dataset, a.seen.kind, &a.seen.who).cmp(&(
            &b.seen.dataset,
            b.seen.kind,
            &b.seen.who,
        ))
    });
    for row in sorted {
        let meter = match row.seen.kind {
            Kind::Person => "display",
            Kind::Plugin | Kind::ServiceAccount => "non_display",
        };
        let timeliness = terms
            .get(row.seen.dataset.as_str())
            .map(|r| timeliness_of(&r.licence))
            .unwrap_or_default();
        let name = match row.seen.kind {
            Kind::Person => name(&row.seen.who),
            _ => String::new(),
        };
        let fields = [
            month.to_string(),
            row.seen.dataset.clone(),
            row.seen.underlying_source.clone(),
            meter.to_string(),
            row.seen.kind.value().to_string(),
            row.seen.who.clone(),
            name,
            row.seen.classification.clone(),
            day(row.first_seen_ns),
            day(row.last_seen_ns),
            "1".to_string(),
            timeliness,
        ];
        out.push_str(
            &fields
                .iter()
                .map(|field| csv_field(field))
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push('\n');
    }
    out
}

fn day(ns: i64) -> String {
    crate::custody::utc(ns).chars().take(10).collect()
}

/// One CSV field, quoted where it must be; a leading `=`, `+`, `-` or `@`
/// is prefixed so a spreadsheet never reads a plugin's or a person's words
/// as a formula.
fn csv_field(said: &str) -> String {
    let guarded = if said.starts_with(['=', '+', '-', '@']) {
        format!("'{said}")
    } else {
        said.to_string()
    };
    if guarded.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", guarded.replace('"', "\"\""))
    } else {
        guarded
    }
}

// ── Reading, as the page and the tools both do ─────────────────────────────

/// A month's counts as the page and the tools draw them.
pub struct Read {
    pub month: String,
    pub rows: Vec<SeenRow>,
    pub listed: Vec<Row>,
    /// The months kept, newest first, the current one among them.
    pub months: Vec<String>,
}

/// A month's counts, read: the datasets as the lake lists them now, the
/// month's rows as kept, and, for the current month, what is seen now laid
/// over them. Notes which tests each instance asks, as a sweep does.
pub async fn read(
    app: &App,
    listed: &ListDatasetsReply,
    records: &AccessRecords,
    month: Option<&str>,
) -> Result<Read, String> {
    let now_ns = app.clock.now_ns();
    let current = month_of(now_ns);
    let month = month
        .filter(|m| is_month(m))
        .map(str::to_string)
        .unwrap_or_else(|| current.clone());
    let rows = data_sources::rows(listed, records);
    note_asked(&rows);
    let answers = answers().await?;
    let wanted = month.clone();
    let kept = blocking(move |store| store.month(&wanted)).await?;
    let rows_seen = if month == current {
        merged(kept, &seen_now(&rows, records, &answers), now_ns)
    } else {
        kept
    };
    let mut months: BTreeSet<String> = blocking(|store| store.months())
        .await?
        .into_iter()
        .collect();
    months.insert(current);
    Ok(Read {
        month,
        rows: rows_seen,
        listed: rows,
        months: months.into_iter().rev().collect(),
    })
}

/// `YYYY-MM`.
pub fn is_month(said: &str) -> bool {
    let bytes = said.as_bytes();
    bytes.len() == 7
        && bytes[4] == b'-'
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..].iter().all(u8::is_ascii_digit)
        && matches!(
            &said[5..],
            "01" | "02" | "03" | "04" | "05" | "06" | "07" | "08" | "09" | "10" | "11" | "12"
        )
}

/// Notes which tests each plugin instance's entitlements ask of its
/// readers.
fn note_asked(rows: &[Row]) {
    let mut by_instance: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for row in rows {
        if let Some(test) = test_of(&row.dataset.dataset, &row.licence) {
            for entitlement in &row.entitled {
                by_instance
                    .entry(entitlement.instance.clone())
                    .or_default()
                    .insert(test.clone());
            }
        }
    }
    if let Ok(mut held) = asked().write() {
        *held = by_instance;
    }
}

/// The tests a person owes before they open `instance`'s pages: those its
/// entitlements ask that they have not answered, where they hold read on
/// it. None for a service account, which has no status.
pub async fn owed(
    records: &AccessRecords,
    subject: &str,
    groups: &[String],
    instance: &str,
) -> Vec<String> {
    if crate::service_accounts::is_service_account(subject) {
        return Vec::new();
    }
    let tests: BTreeSet<String> = asked()
        .read()
        .ok()
        .and_then(|held| held.get(instance).cloned())
        .unwrap_or_default();
    if tests.is_empty() {
        return Vec::new();
    }
    let reads = person_access(records, subject, groups)
        .plugins
        .get(instance)
        .is_some_and(|held| held.union().data.is_some());
    if !reads {
        return Vec::new();
    }
    let Ok(answers) = answers().await else {
        return Vec::new();
    };
    tests
        .into_iter()
        .filter(|test| !answers.contains_key(&(subject.to_string(), test.clone())))
        .collect()
}

/// Every test a person may be asked: those of the plugins they read.
pub async fn tests_for(
    records: &AccessRecords,
    subject: &str,
    groups: &[String],
) -> BTreeSet<String> {
    let access = person_access(records, subject, groups);
    let held = asked().read().map(|held| held.clone()).unwrap_or_default();
    held.into_iter()
        .filter(|(instance, _)| {
            access
                .plugins
                .get(instance)
                .is_some_and(|held| held.union().data.is_some())
        })
        .flat_map(|(_, tests)| tests)
        .collect()
}

/// Who answers, and how.
pub struct Answering<'a> {
    /// The person the answer is about.
    pub subject: &'a str,
    pub test: &'a str,
    pub status: Status,
    /// Who set it: the person, or a deployment admin.
    pub by: &'a str,
    pub through_delegation: &'a str,
    pub client_name: &'a str,
    pub note: &'a str,
}

/// Records an answer: what stands, and its own record
/// (`professional_status_set`). Answers the record.
pub async fn answer(answering: Answering<'_>, now_ns: i64) -> Result<StatusChange, String> {
    if answering.note.chars().count() > NOTE_MOST {
        return Err(format!("the note is longer than {NOTE_MOST} characters"));
    }
    let before = answers()
        .await?
        .get(&(answering.subject.to_string(), answering.test.to_string()))
        .copied();
    let change = StatusChange {
        subject: answering.subject.to_string(),
        test: answering.test.to_string(),
        from: before,
        to: answering.status,
        by: answering.by.to_string(),
        through_delegation: answering.through_delegation.to_string(),
        client_name: answering.client_name.to_string(),
        at_ns: now_ns,
        note: answering.note.to_string(),
    };
    let kept = change.clone();
    blocking(move |store| store.set_status(&kept)).await?;
    if let Ok(mut held) = answered().write() {
        if let Some(held) = held.as_mut() {
            held.insert((change.subject.clone(), change.test.clone()), change.to);
        }
    }
    Ok(change)
}

/// A person's answers' records, newest first.
pub async fn changes_of(subject: &str) -> Result<Vec<StatusChange>, String> {
    let subject = subject.to_string();
    blocking(move |store| store.status_changes(&subject)).await
}

/// The sweep: who could use each dataset now, kept for the month. Run each
/// minute; a failure is logged and the next minute tries again.
pub async fn sweep(app: &App, now_ns: i64) -> usize {
    let Ok(records) = app.records.current(now_ns) else {
        return 0;
    };
    let listed = match data_sources::datasets(&app.bus, "").await {
        Ok(listed) => listed,
        Err(failed) => {
            tracing::debug!(%failed, "the data-use sweep could not list the datasets");
            return 0;
        }
    };
    let rows = data_sources::rows(&listed, &records);
    note_asked(&rows);
    // Read the answers afresh each sweep: another dashboard may have kept one.
    if let Ok(mut held) = answered().write() {
        *held = None;
    }
    let answers = match answers().await {
        Ok(answers) => answers,
        Err(failed) => {
            tracing::warn!(%failed, "the data-use sweep could not read the answers");
            return 0;
        }
    };
    let seen = seen_now(&rows, &records, &answers);
    let n = seen.len();
    let month = month_of(now_ns);
    match blocking(move |store| store.note(&month, &seen, now_ns)).await {
        Ok(()) => n,
        Err(failed) => {
            tracing::warn!(%failed, "the data-use sweep could not keep what it saw");
            0
        }
    }
}

/// The counts in JSON, as the tools answer them.
pub fn counts_json(counts: &Counts, by_source: bool) -> serde_json::Value {
    let mut answer = serde_json::json!({
        "meters": counts.meters,
        "timeliness": counts.timeliness,
        "display": {
            "persons_entitled": counts.persons,
            "persons_served": serde_json::Value::Null,
            "devices": serde_json::Value::Null,
            "not_counted_yet": ["persons_served", "devices"],
        },
        "non_display": {
            "plugins": counts.plugins_total(),
            "plugins_by_category": counts.plugins,
            "service_accounts": counts.service_accounts,
        },
    });
    if counts.split {
        answer["display"]["professional"] = counts.professional.into();
        answer["display"]["non_professional"] = counts.non_professional.into();
    }
    if by_source {
        answer["underlying_source"] = counts.key.clone().into();
        answer["datasets"] = counts
            .beside
            .split(", ")
            .filter(|d| !d.is_empty())
            .collect::<Vec<_>>()
            .into();
    } else {
        answer["dataset"] = counts.key.clone().into();
        answer["underlying_source"] = counts.beside.clone().into();
    }
    answer
}

#[cfg(test)]
mod tests;
