//! Where the lake meets the bus (W10).
//!
//! Five commands and six queries in; three events heard and one more kept
//! from; three queries asked; six events out. A data plugin records prices, bars,
//! and from contract v19 trades and quotes, in batches, and declines what it
//! cannot serve of a want; the reading roles read prices, bars and the
//! datasets, and `signal` and `ems` trades and quotes; the dashboard sets and
//! lists the priority and lists the datasets.
//!
//! Trades are published unconflated, every one, and a reader that lost some
//! reads those recorded after the watermark it last saw (W10.5, W10.6; the
//! plan's Q33); quotes are conflated per subject, dataset, venue and asset,
//! as prices are per subject, dataset, kind, venue and asset. The conductor's data configuration is
//! heard and kept; a merged record (W3.8) is followed by alias; the misses a
//! plugin reports (W3.2, W3.15) are counted. The instrument store is asked
//! whether a row's subjects are held (W3.6) and what cash instrument a
//! currency's code names (W3.1), and the conductor which version each
//! instance was launched at (W8.2).
//!
//! # Who asked
//!
//! A query's envelope says whose read it is: a plugin's, its scope marked
//! as applying, answered within the plugin's entitlements and named by the
//! instance its sidecar stamped; or a core component's -- the dashboard's --
//! answered whole. A batch is the instance's its sidecar stamped as the
//! publisher, and every row's `Source.instance` must be that one.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use meridian_bus::{refusal_with_status, Bus, Envelope, RefusalStatus};
use meridian_domain::date::Date;
use meridian_domain::lake;
use meridian_domain::money::{asset_of, Asset, CashInstruments, ISO4217};
use meridian_domain::v1::PartitionSequence;
use meridian_domain::v1::{
    AssetClass, Compounding, InstrumentType, ListRatesReply, ListRatesRequest, RateKind,
    RecordRatesRequest, TenorUnit, ValueUnit,
};
use meridian_domain::v1::{
    BarsRecordedEvent, DatasetRef, DeclineWantReply, DeclineWantRequest, EntitlementsChangedEvent,
    ExecutionCost, Identifier, InstrumentReplacedEvent, ListBarsReply, ListBarsRequest,
    ListDatasetsReply, ListDatasetsRequest, ListExecutionCostsReply, ListExecutionCostsRequest,
    ListOutcomesReply, ListOutcomesRequest, ListPricesReply, ListPricesRequest, ListQuotesReply,
    ListQuotesRequest, ListSourcePrioritiesReply, ListSourcePrioritiesRequest, ListTradesReply,
    ListTradesRequest, MissingInstrumentDetectedEvent, MissingVenueDetectedEvent,
    ObservationsWantedEvent, OutcomesRecordedEvent, PluginCatalogue, PluginCatalogueRequest,
    PluginLaunchState, PriceBasis, PriceKind, PricesRecordedEvent, QuoteCharacteristic,
    QuotesRecordedEvent, RatesRecordedEvent, RecordBarsRequest, RecordExecutionCostsRequest,
    RecordObservationsReply, RecordOutcomesRequest, RecordPricesRequest, RecordQuotesRequest,
    RecordTradesRequest, ResolveIdentifierReply, ResolveIdentifierRequest, ResolveInstrumentReply,
    ResolveInstrumentRequest, SetSourcePriorityRequest, SourceChoice, SourcePriority,
    SourceTimeKind, SubjectRef, TcaBenchmark, TradeCharacteristic, TradesRecordedEvent, Unanswered,
    UnansweredReason, WantWithdrawnEvent, Watermark,
};
use meridian_domain::v1::{
    CorporateActionKind, CorporateActionsRecordedEvent, ListCorporateActionsReply,
    ListCorporateActionsRequest, RecordCorporateActionsRequest, ResolveCompanyRequest,
};
use meridian_domain::v1::{
    ListSeriesReply, ListSeriesRequest, ListStatisticsReply, ListStatisticsRequest,
    RecordSeriesRequest, RecordStatisticsRequest, StatisticKind, StatisticsRecordedEvent,
    SubjectRole,
};
use meridian_domain::v1::{
    QualityReason, QualityState, RecordQualityAssessmentsReply, RecordQualityAssessmentsRequest,
};
use meridian_pb::bounds;
use prost::Message;

pub use meridian_clock::Clock;

mod filings;

use crate::config::{DataConfig, Reader};
use crate::read::{self, Against, Read};
use crate::row::{DataType, Observation};
use crate::store::{rank, Assessment, Store, WantChange, WantChangeKind, When};

pub const RECORD_PRICES: &str = "platform.lake.command.record-prices";
pub const RECORD_BARS: &str = "platform.lake.command.record-bars";
pub const RECORD_TRADES: &str = "platform.lake.command.record-trades";
pub const RECORD_QUOTES: &str = "platform.lake.command.record-quotes";
pub const RECORD_OUTCOMES: &str = "platform.lake.command.record-outcomes";
/// Contract v27, the rates slice (W10.4, W10.6).
pub const RECORD_RATES: &str = "platform.lake.command.record-rates";
/// Contract v27 (W10.4, W10.5; ruling 418): a dataset's liveness.
pub const RECORD_LIVENESS: &str = "platform.lake.command.record-liveness";
/// Contract v28 (W10.12; ruling 423): a signal plugin's escalations.
pub const RECORD_QUALITY_ASSESSMENTS: &str = "platform.lake.command.record-quality-assessments";
pub const LIST_RATES: &str = "platform.lake.query.list-rates";
/// Contract v28, listed options and futures (W10.4, W10.6; ruling 561).
pub const RECORD_SERIES: &str = "platform.lake.command.record-series";
pub const LIST_SERIES: &str = "platform.lake.query.list-series";
pub const RECORD_STATISTICS: &str = "platform.lake.command.record-statistics";
pub const LIST_STATISTICS: &str = "platform.lake.query.list-statistics";
/// Contract v28, track B (W10.4, W10.6, W12.8): a signal plugin's derived
/// series and saved scenarios.
pub const RECORD_DERIVED_SERIES: &str = "platform.lake.command.record-derived-series";
pub const LIST_DERIVED_SERIES: &str = "platform.lake.query.list-derived-series";
pub const RECORD_SCENARIOS: &str = "platform.lake.command.record-scenarios";
pub const LIST_SCENARIOS: &str = "platform.lake.query.list-scenarios";
/// Contract v28, the filings slice (W10.4, W10.6): declared corporate
/// actions, a cash dividend in v28.
pub const RECORD_CORPORATE_ACTIONS: &str = "platform.lake.command.record-corporate-actions";
pub const LIST_CORPORATE_ACTIONS: &str = "platform.lake.query.list-corporate-actions";
/// Contract v28, the filings slice (W10.4 to W10.6): filings, reported
/// facts and rating actions; and, the guardrail's second move-out, holdings
/// reports and their holdings ([`filings`]).
pub const RECORD_FILINGS: &str = "platform.lake.command.record-filings";
pub const LIST_FILINGS: &str = "platform.lake.query.list-filings";
pub const RECORD_REPORTED_FACTS: &str = "platform.lake.command.record-reported-facts";
pub const LIST_REPORTED_FACTS: &str = "platform.lake.query.list-reported-facts";
pub const RECORD_RATING_ACTIONS: &str = "platform.lake.command.record-rating-actions";
pub const LIST_RATING_ACTIONS: &str = "platform.lake.query.list-rating-actions";
pub const RECORD_HOLDINGS_REPORTS: &str = "platform.lake.command.record-holdings-reports";
pub const RECORD_HOLDINGS: &str = "platform.lake.command.record-holdings";
pub const LIST_HOLDINGS: &str = "platform.lake.query.list-holdings";
/// A company subject's record (contract v28, W3.32): asked by its `ENT-` ID.
pub const RESOLVE_COMPANY: &str = "platform.reference.query.resolve-company";
pub const DECLINE_WANT: &str = "platform.lake.command.decline-want";
pub const SET_SOURCE_PRIORITY: &str = "platform.lake.command.set-source-priority";
pub const LIST_PRICES: &str = "platform.lake.query.list-prices";
/// W10.10 (contract v26): one bounded, read-only query over a kind.
pub const QUERY_LAKE: &str = "platform.lake.query.query-lake";
pub const LIST_BARS: &str = "platform.lake.query.list-bars";
pub const LIST_TRADES: &str = "platform.lake.query.list-trades";
pub const LIST_QUOTES: &str = "platform.lake.query.list-quotes";
pub const LIST_OUTCOMES: &str = "platform.lake.query.list-outcomes";
pub const LIST_DATASETS: &str = "platform.lake.query.list-datasets";
// Contract v27 (W10.11): execution costs, the analytics component's rows.
pub const RECORD_EXECUTION_COSTS: &str = "platform.lake.command.record-execution-costs";
pub const LIST_EXECUTION_COSTS: &str = "platform.lake.query.list-execution-costs";
pub const LIST_SOURCE_PRIORITIES: &str = "platform.lake.query.list-source-priorities";
pub const OBSERVATIONS_WANTED: &str = "platform.lake.event.observations-wanted";
pub const WANT_WITHDRAWN: &str = "platform.lake.event.want-withdrawn";
pub const ENTITLEMENTS_CHANGED: &str = "platform.config.event.entitlements-changed";
pub const INSTRUMENT_REPLACED: &str = "platform.reference.event.instrument-replaced";
pub const INSTRUMENT_MISSING: &str = "platform.reference.event.instrument-missing";
pub const VENUE_MISSING: &str = "platform.reference.event.venue-missing";
pub const RESOLVE_INSTRUMENT: &str = "platform.reference.query.resolve-instrument";
pub const RESOLVE_IDENTIFIER: &str = "platform.reference.query.resolve-identifier";
pub const PLUGIN_CATALOGUE: &str = "platform.config.query.plugin-catalogue";

/// Every launch in the catalogue, read a page at a time with
/// `launches_only` (contract v28's amendment, ruling 696), `ask` putting
/// each request to the conductor.
pub(crate) fn launches_in_pages(
    ask: impl Fn(PluginCatalogueRequest) -> Result<PluginCatalogue, String>,
) -> Result<PluginCatalogue, String> {
    let mut asked = PluginCatalogueRequest {
        launches_only: true,
        ..Default::default()
    };
    let mut held = PluginCatalogue::default();
    // More pages than any catalogue's launches fill.
    for _ in 0..1000 {
        let page = ask(asked.clone())?;
        held.launches.extend(page.launches);
        if page.next_cursor.is_empty() || page.next_cursor == asked.cursor {
            return Ok(held);
        }
        asked.cursor = page.next_cursor;
    }
    Err("the catalogue's launches ran past 1000 pages".to_string())
}
/// A dataset's calendar, read for the closed-day flag (W3.19, W10.4).
pub const READ_CALENDAR: &str = "platform.reference.query.read-calendar";
/// A calendar's new version, heard to re-check the flag (W3.17, W10.4).
pub const CALENDAR_APPLIED: &str = "platform.reference.event.calendar-applied";

/// A company's platform ID begins with this (contract v28, ruling 594).
const COMPANY_PREFIX: &str = "ENT-";

/// A dataset's subject for a data type's recorded rows.
pub fn recorded_topic(dataset: &str, data_type: DataType) -> String {
    match data_type {
        DataType::Price => format!("platform.lake.{dataset}.event.prices-recorded"),
        DataType::Bar => format!("platform.lake.{dataset}.event.bars-recorded"),
        DataType::Trade => format!("platform.lake.{dataset}.event.trades-recorded"),
        DataType::Quote => format!("platform.lake.{dataset}.event.quotes-recorded"),
        DataType::Outcome => format!("platform.lake.{dataset}.event.outcomes-recorded"),
        DataType::Rate => format!("platform.lake.{dataset}.event.rates-recorded"),
        // Contract v28 (W10.5): conflated per subject, series key, kind and
        // business date.
        DataType::Statistic => format!("platform.lake.{dataset}.event.statistics-recorded"),
        // Contract v28, the filings slice (W10.5): never conflated.
        DataType::CorporateAction => {
            format!("platform.lake.{dataset}.event.corporate-actions-recorded")
        }
        DataType::Filing => format!("platform.lake.{dataset}.event.filings-recorded"),
        // Read, not heard (contract v28, W10.5): facts, holdings and rating
        // actions arrive in bulk and lag their periods by weeks.
        DataType::ReportedFact
        | DataType::RatingAction
        | DataType::HoldingsReport
        | DataType::Holding => String::new(),
        // Never published: read within each reader's account scope (W10.11);
        // a listed series is read when a chain is built (contract v28, W10.5).
        // A signal plugin's derived values and saved scenarios are read,
        // never published (contract v28, W10.5).
        DataType::ExecutionCost | DataType::Series | DataType::Derived | DataType::Scenario => {
            String::new()
        }
    }
}

/// How long the lake waits on the instrument store or the conductor: a
/// batch the store does not answer in it is refused to be tried again.
const ASKING: Duration = Duration::from_secs(3);

/// How often a want asked and not answered is asked again.
const ASK_AGAIN_NS: i64 = 60 * 1_000_000_000;

/// How often a standing want is delivered again, the same want under the
/// same ID: a data plugin restarted -- relaunched, upgraded -- holds nothing it
/// cannot lose, and the lake hears no registration, so a stream it kept
/// current resumes on the next delivery, within this.
pub const REDELIVER_STANDING_NS: i64 = ASK_AGAIN_NS;

/// How often the standing wants are swept for withdrawal, and how often
/// retention is applied.
pub const SWEEP_EVERY: Duration = Duration::from_secs(15);
pub const RETENTION_EVERY: Duration = Duration::from_secs(3600);

/// How often the trades recorded are logged, per second over the minute:
/// what the `lake_trades_recorded_per_second` band reads (contract v19, Q40).
pub const TRADES_LOGGED_EVERY: Duration = Duration::from_secs(60);

/// How long the catalogue's launched versions are read again after.
const VERSIONS_FOR_NS: i64 = 300 * 1_000_000_000;

/// A want open, coalescing readers' identical asks.
#[derive(Debug, Clone)]
struct OpenWant {
    event: ObservationsWantedEvent,
    asked_by: BTreeSet<String>,
    asked_at_ns: i64,
    published_at_ns: i64,
    cadence_ns: i64,
    settled: BTreeSet<String>,
}

type WantKey = (
    String,
    String,
    Vec<i32>,
    i64,
    String,
    i64,
    i64,
    bool,
    Vec<String>,
);

#[derive(Default)]
struct Wants {
    by_key: BTreeMap<WantKey, String>,
    open: BTreeMap<String, OpenWant>,
}

/// The lake, serving.
pub struct Lake {
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    pub(crate) clock: Arc<dyn Clock>,
    config: RwLock<DataConfig>,
    wants: Mutex<Wants>,
    /// What a source declined, by dataset, subject and the date or range its
    /// want asked (contract v19): its reason and when.
    declined: Mutex<read::Declined>,
    /// Trades recorded since the band last read them.
    trades_recorded: std::sync::atomic::AtomicU64,
    /// Each record the instrument store said it holds, with its asset class
    /// and type: a rate's subject is a referential record (contract v27).
    held: Mutex<BTreeMap<String, (i32, i32)>>,
    cash: Mutex<CashInstruments>,
    versions: Mutex<(BTreeMap<String, String>, i64)>,
    /// Contract v27 (W10.4; ruling 418): the latest liveness per dataset
    /// and venue. Not a row, never kept as one: measurement a data plugin
    /// sends again within seconds, so a restart loses nothing it needs.
    liveness: Mutex<BTreeMap<(String, String), meridian_domain::v1::DatasetLiveness>>,
}

/// The analytics component's instance where nothing names another (W12.2).
pub const ANALYTICS: &str = "analytics";

static ANALYTICS_INSTANCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Name the analytics component's instance, every copy of it: the one
/// publisher whose `on_behalf_of_instance_id` the lake reads. Set once, by
/// the lake's process at start, from what the chart says; [`ANALYTICS`]
/// otherwise.
pub fn reads_for_analytics(instance: &str) {
    let _ = ANALYTICS_INSTANCE.set(instance.to_string());
}

fn analytics_instance() -> &'static str {
    ANALYTICS_INSTANCE
        .get()
        .map(String::as_str)
        .unwrap_or(ANALYTICS)
}

pub(crate) fn reader_of(envelope: &Envelope) -> Reader {
    match envelope.meta.as_ref() {
        // A job's read (contract v26, W12.2): within the client's
        // entitlements and its role's kinds, never the component's own.
        Some(meta)
            if meta.account_scope_applies
                && meta.publisher_instance_id == analytics_instance()
                && !meta.on_behalf_of_instance_id.is_empty() =>
        {
            Reader::Plugin(meta.on_behalf_of_instance_id.clone())
        }
        // A sidecar never sets the field, and from anyone else it is
        // ignored: the reader is the publisher.
        Some(meta) if meta.account_scope_applies => {
            Reader::Plugin(meta.publisher_instance_id.clone())
        }
        _ => Reader::Core,
    }
}

fn publisher(envelope: &Envelope) -> String {
    envelope
        .meta
        .as_ref()
        .map(|meta| meta.publisher_instance_id.clone())
        .unwrap_or_default()
}

fn decode<M: Message + Default>(envelope: &Envelope, wanted: &str) -> Result<M, String> {
    if envelope.payload_type != wanted {
        return Err(format!("expected {wanted}, got {}", envelope.payload_type));
    }
    M::decode(&envelope.payload[..]).map_err(|failed| format!("undecodable {wanted}: {failed}"))
}

fn watermark(heads: &BTreeMap<String, u64>, datasets: &BTreeSet<String>) -> Watermark {
    Watermark {
        partitions: datasets
            .iter()
            .map(|d| PartitionSequence {
                partition: d.clone(),
                sequence: heads.get(d).copied().unwrap_or(0),
            })
            .collect(),
    }
}

fn subject_ref(subject: &str) -> Option<SubjectRef> {
    (!subject.is_empty()).then(|| SubjectRef {
        entity_id: subject.to_string(),
        ..Default::default()
    })
}

impl Lake {
    pub fn new(bus: Arc<Bus>, store: Arc<dyn Store>, clock: Arc<dyn Clock>) -> Arc<Lake> {
        let config = store
            .configuration()
            .ok()
            .flatten()
            .map(|event| DataConfig::from_event(&event))
            .unwrap_or_default();
        Arc::new(Lake {
            bus,
            store,
            clock,
            config: RwLock::new(config),
            wants: Mutex::new(Wants::default()),
            declined: Mutex::new(BTreeMap::new()),
            trades_recorded: std::sync::atomic::AtomicU64::new(0),
            held: Mutex::new(BTreeMap::new()),
            cash: Mutex::new(CashInstruments::default()),
            versions: Mutex::new((BTreeMap::new(), 0)),
            liveness: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn config(&self) -> DataConfig {
        self.config.read().expect("config poisoned").clone()
    }

    fn ask<Q: Message, R: Message + Default>(
        &self,
        topic: &str,
        kind: &str,
        request: Q,
    ) -> Result<R, String> {
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| "the lake has no runtime to ask on".to_string())?;
        let (payload_type, payload) = handle
            .block_on(
                self.bus
                    .call(topic, kind, request.encode_to_vec(), None, Some(ASKING)),
            )
            .map_err(|failed| failed.to_string())?;
        R::decode(&payload[..]).map_err(|failed| format!("{payload_type} did not read: {failed}"))
    }

    /// Whether the instrument store holds a record by this ID (W3.6),
    /// remembered once it does.
    fn subject_held(&self, entity: &str) -> Result<bool, String> {
        Ok(self.subject_record(entity)?.is_some())
    }

    /// Whether the instrument store holds a company by its `ENT-` ID
    /// (contract v28, W3.32), remembered once it does: its refusal is a
    /// company it does not hold; no answer in time records nothing.
    fn company_held(&self, entity: &str) -> Result<bool, String> {
        if self
            .held
            .lock()
            .expect("held poisoned")
            .contains_key(entity)
        {
            return Ok(true);
        }
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| "the lake has no runtime to ask on".to_string())?;
        let asked = handle.block_on(
            self.bus.call(
                RESOLVE_COMPANY,
                "meridian.v1.ResolveCompanyRequest",
                ResolveCompanyRequest {
                    company_id: entity.to_string(),
                    ..Default::default()
                }
                .encode_to_vec(),
                None,
                Some(ASKING),
            ),
        );
        match asked {
            Ok(_) => {
                self.held
                    .lock()
                    .expect("held poisoned")
                    .insert(entity.to_string(), (0, 0));
                Ok(true)
            }
            Err(meridian_bus::BusError::HandlerFailed { .. }) => Ok(false),
            Err(failed) => Err(format!(
                "the instrument store did not say whether it holds {entity}, and nothing was \
                 recorded: {failed}"
            )),
        }
    }

    /// The asset class and type of the record the instrument store holds
    /// by this ID, remembered once it does; `None` where it holds none.
    fn subject_record(&self, entity: &str) -> Result<Option<(i32, i32)>, String> {
        if let Some(held) = self.held.lock().expect("held poisoned").get(entity) {
            return Ok(Some(*held));
        }
        let reply: ResolveInstrumentReply = self
            .ask(
                RESOLVE_INSTRUMENT,
                "meridian.v1.ResolveInstrumentRequest",
                ResolveInstrumentRequest {
                    instrument_id: entity.to_string(),
                    as_of_ns: self.clock.now_ns(),
                },
            )
            .map_err(|failed| format!("the instrument store did not say whether it holds {entity}, and nothing was recorded: {failed}"))?;
        let Some(record) = reply.instrument.filter(|_| reply.found) else {
            return Ok(None);
        };
        let said = (record.asset_class, record.instrument_type);
        self.held
            .lock()
            .expect("held poisoned")
            .insert(entity.to_string(), said);
        Ok(Some(said))
    }

    /// The cash instrument a currency's ISO 4217 code names (W3.1, W10.5),
    /// resolved for a valid time and remembered.
    fn cash_instrument(&self, code: &str, as_of_ns: i64) -> Result<String, String> {
        if let Some(held) = self.cash.lock().expect("cash poisoned").instrument(code) {
            return Ok(held.to_string());
        }
        let reply: ResolveIdentifierReply = self
            .ask(
                RESOLVE_IDENTIFIER,
                "meridian.v1.ResolveIdentifierRequest",
                ResolveIdentifierRequest {
                    identifiers: vec![Identifier {
                        scheme: ISO4217.into(),
                        value: code.to_string(),
                        source: String::new(),
                        ..Default::default()
                    }],
                    as_of_ns,
                    ..Default::default()
                },
            )
            .map_err(|failed| format!("the instrument store did not resolve {code}, and nothing was recorded: {failed}"))?;
        if !reply.found || reply.instrument_id.is_empty() {
            return Err(format!(
                "{code} names no one cash instrument the deployment holds ({})",
                meridian_domain::v1::MissReason::try_from(reply.miss_reason)
                    .map(|r| r.as_str_name())
                    .unwrap_or("not said")
            ));
        }
        self.cash
            .lock()
            .expect("cash poisoned")
            .insert(code, reply.instrument_id.clone());
        Ok(reply.instrument_id)
    }

    /// The version each instance was launched at, from the catalogue
    /// (W8.2), read again every five minutes or for an instance not seen.
    fn plugin_version(&self, instance: &str) -> String {
        let now = self.clock.now_ns();
        {
            let held = self.versions.lock().expect("versions poisoned");
            let known = held.0.contains_key(instance);
            let fresh = now - held.1 < VERSIONS_FOR_NS;
            // An instance not seen is read again after half a minute.
            if fresh && (known || now - held.1 < 30 * 1_000_000_000) {
                return held.0.get(instance).cloned().unwrap_or_default();
            }
        }
        // The launches alone, page by page (contract v28's amendment,
        // ruling 696): no version's declaration is needed here.
        let read = launches_in_pages(|asked| {
            self.ask(
                PLUGIN_CATALOGUE,
                "meridian.v1.PluginCatalogueRequest",
                asked,
            )
        });
        let mut held = self.versions.lock().expect("versions poisoned");
        match read {
            Ok(catalogue) => {
                held.0 = catalogue
                    .launches
                    .into_iter()
                    .filter(|l| l.state == PluginLaunchState::Launched as i32)
                    .map(|l| (l.instance_id, l.version))
                    .collect();
                held.1 = now;
            }
            Err(failed) => {
                tracing::debug!(%failed, "the catalogue could not be read for launched versions");
                held.1 = now;
            }
        }
        held.0.get(instance).cloned().unwrap_or_default()
    }

    /// A row's series key held to W10.4 (contract v28, ruling 561): one
    /// identifier, an option's `occ_osi` symbol or a future's or future
    /// option's `ticker` carrying its venue's MIC in `source`; on a quote, a trade, a
    /// price, a bar or a statistic whose subject is the underlying, never on
    /// an option's own record, which is the series; required on a listed
    /// series.
    fn series_key_refused(&self, at: &str, row: &Observation) -> Result<(), String> {
        let meta = row.meta();
        let key = &meta.series_key;
        let data_type = row.data_type();
        if key.is_empty() {
            if data_type == DataType::Series {
                return Err(format!(
                    "{at}.meta.series_key is required: a listed series names its key"
                ));
            }
            return Ok(());
        }
        if matches!(
            data_type,
            DataType::Outcome | DataType::Rate | DataType::ExecutionCost
        ) || data_type.filings()
        {
            return Err(format!(
                "{at}.meta.series_key: {} is keyed by no series",
                data_type.name()
            ));
        }
        if let Some((path, why)) = lake::series_key_problem(key) {
            return Err(format!("{at}.meta.series_key{path}: {why}"));
        }
        // A key names a series under its subject: a row about a held
        // option is keyed by its record and names no key.
        for (j, subject) in meta.subjects.iter().enumerate() {
            if let Some((_, kind)) = self.subject_record(&subject.entity_id)? {
                if kind == InstrumentType::ListedOption as i32
                    || kind == InstrumentType::FutureOption as i32
                {
                    return Err(format!(
                        "{at}.meta.series_key: the subject {:?} (meta.subjects[{j}]) is the \
                         series' own record; a row about it names no key",
                        subject.entity_id
                    ));
                }
            }
        }
        Ok(())
    }

    /// A saved scenario held to W12.8 (contract v28; rulings 566, 581 to
    /// 584): one scenario or one scenario set, named; each shock its object,
    /// a target held or every object of its kind, and a form and unit its
    /// object takes; a set's axes and their steps within bounds.
    fn scenario_refused(
        &self,
        at: &str,
        saved: &meridian_domain::v1::SavedScenario,
    ) -> Result<(), String> {
        if saved.description.chars().count() > 2_000 {
            return Err(format!("{at}.description is at most 2,000 characters"));
        }
        match (&saved.scenario, &saved.scenario_set) {
            (Some(scenario), None) => self.one_scenario(&format!("{at}.scenario"), scenario),
            (None, Some(set)) => {
                let path = format!("{at}.scenario_set");
                if !bounds::SCENARIO_SET_NAME_LENGTH.admits(set.name.chars().count()) {
                    return Err(format!("{path}.name is 1 to 200 characters"));
                }
                if !bounds::SCENARIO_SET_AXES_COUNT.admits(set.axes.len()) {
                    return Err(format!("{path}.axes names {}; 1 to 4", set.axes.len()));
                }
                for (a, axis) in set.axes.iter().enumerate() {
                    let path = format!("{path}.axes[{a}]");
                    if !bounds::SCENARIO_AXIS_NAME_LENGTH.admits(axis.name.chars().count()) {
                        return Err(format!("{path}.name is 1 to 64 characters"));
                    }
                    if !bounds::SCENARIO_AXIS_STEPS_COUNT.admits(axis.steps.len()) {
                        return Err(format!("{path}.steps names {}; 1 to 32", axis.steps.len()));
                    }
                    for (s, step) in axis.steps.iter().enumerate() {
                        self.one_scenario(&format!("{path}.steps[{s}]"), step)?;
                    }
                }
                Ok(())
            }
            _ => Err(format!(
                "{at}: a saved scenario is one scenario or one scenario set, exactly one"
            )),
        }
    }

    fn one_scenario(
        &self,
        at: &str,
        scenario: &meridian_domain::v1::Scenario,
    ) -> Result<(), String> {
        use meridian_domain::v1::{ShockForm, ShockObject, ShockUnit};
        if !bounds::SCENARIO_NAME_LENGTH.admits(scenario.name.chars().count()) {
            return Err(format!("{at}.name is 1 to 200 characters"));
        }
        if !bounds::SCENARIO_SHOCKS_COUNT.admits(scenario.shocks.len()) {
            return Err(format!(
                "{at}.shocks names {}; at most 64",
                scenario.shocks.len()
            ));
        }
        for (i, shock) in scenario.shocks.iter().enumerate() {
            let path = format!("{at}.shocks[{i}]");
            let object = ShockObject::try_from(shock.object).unwrap_or(ShockObject::Unspecified);
            let form = ShockForm::try_from(shock.form).unwrap_or(ShockForm::Unspecified);
            let unit = ShockUnit::try_from(shock.unit).unwrap_or(ShockUnit::Unspecified);
            if object == ShockObject::Unspecified {
                return Err(format!("{path}.object is unspecified"));
            }
            // Each object's forms and units (581, 584).
            let fits = match object {
                ShockObject::Spot => matches!(
                    (form, unit),
                    (ShockForm::Relative, ShockUnit::Percent)
                        | (ShockForm::Absolute, ShockUnit::Price)
                ),
                ShockObject::VolSurface => matches!(
                    (form, unit),
                    (
                        ShockForm::Parallel | ShockForm::Bucketed,
                        ShockUnit::VolPoints
                    )
                ),
                ShockObject::Rate => matches!(
                    (form, unit),
                    (
                        ShockForm::Parallel | ShockForm::Bucketed,
                        ShockUnit::BasisPoints
                    )
                ),
                ShockObject::Time => matches!(
                    (form, unit),
                    (ShockForm::Absolute, ShockUnit::Days | ShockUnit::Minutes)
                        | (ShockForm::ToSettlement, _)
                ),
                ShockObject::Unspecified => false,
            };
            if !fits {
                return Err(format!(
                    "{path}.form and unit: a {} shock takes {}",
                    object
                        .as_str_name()
                        .trim_start_matches("SHOCK_OBJECT_")
                        .to_lowercase(),
                    match object {
                        ShockObject::Spot => "relative in percent, or absolute in price",
                        ShockObject::VolSurface => "vol points, parallel or per expiry bucket",
                        ShockObject::Rate => "basis points, parallel or per tenor",
                        _ => "days or minutes, or to settlement",
                    }
                ));
            }
            if form == ShockForm::Bucketed {
                if shock.buckets.is_empty() {
                    return Err(format!(
                        "{path}.buckets: a bucketed shock names its buckets"
                    ));
                }
            } else if form != ShockForm::ToSettlement && shock.value.is_none() {
                return Err(format!("{path}.value is required"));
            }
            let target = shock.target.clone().unwrap_or_default();
            match (target.every, target.subject_id.is_empty()) {
                (true, true) => {}
                (false, false) => {
                    if !self.subject_held(&target.subject_id)? {
                        return Err(format!(
                            "{path}.target.subject_id {:?} is no record the deployment holds",
                            target.subject_id
                        ));
                    }
                }
                _ => {
                    return Err(format!(
                        "{path}.target: an object by its subject ID, or every object of its kind, \
                         exactly one"
                    ))
                }
            }
        }
        Ok(())
    }

    /// W10.6, contract v28 (track B): a signal plugin's derived values for
    /// 1 to 64 subjects, by series and a range of business dates, every
    /// readable derived dataset side by side, each value's latest version.
    pub(crate) fn list_derived_series(
        &self,
        request: meridian_domain::v1::ListDerivedSeriesRequest,
        envelope: &Envelope,
    ) -> Result<meridian_domain::v1::ListDerivedSeriesReply, String> {
        if !bounds::LIST_DERIVED_SERIES_REQUEST_SUBJECTS_COUNT.admits(request.subjects.len()) {
            return Err(format!(
                "subjects names {}; 1 to 64",
                request.subjects.len()
            ));
        }
        let from = meridian_domain::date::optional("from_date", &request.from_date)?
            .unwrap_or(Date::of_utc_instant(0));
        let to = meridian_domain::date::optional("to_date", &request.to_date)?
            .unwrap_or(Date::of_utc_instant(self.clock.now_ns()));
        let answer = self.read(
            ReadRequest {
                data_type: DataType::Derived,
                subjects: request.subjects,
                kinds: Vec::new(),
                interval_ns: 0,
                sources: Some(SourceChoice {
                    side_by_side: true,
                    ..Default::default()
                }),
                at_ns: 0,
                business_date: String::new(),
                valid_from_ns: 1,
                valid_until_ns: 0,
                as_of_ns: 0,
                page_size: 500,
                cursor: String::new(),
                after: None,
                tenors: Vec::new(),
                published_as_of_ns: 0,
                series_keys: Vec::new(),
                all_series: false,
                dates: Some((from, to)),
            },
            envelope,
        )?;
        Ok(meridian_domain::v1::ListDerivedSeriesReply {
            values: answer
                .rows
                .into_iter()
                .filter_map(|row| match row {
                    Observation::Derived(value)
                        if request.series.is_empty() || request.series.contains(&value.series) =>
                    {
                        Some(value)
                    }
                    _ => None,
                })
                .collect(),
        })
    }

    /// W12.8, contract v28: the saved scenarios of every dataset the reader
    /// may read, by row key, each at its latest version, or at the version
    /// asked of one row key, whichever version that is (each edit is the
    /// next version of its key, and a job names any).
    pub(crate) fn list_scenarios(
        &self,
        request: meridian_domain::v1::ListScenariosRequest,
        envelope: &Envelope,
    ) -> Result<meridian_domain::v1::ListScenariosReply, String> {
        if request.version != 0 && request.row_keys.len() != 1 {
            return Err("version: with one row key".into());
        }
        let reader = reader_of(envelope);
        let config = self.config();
        let mut scenarios = Vec::new();
        for dataset in config.readable(&reader, DataType::Scenario) {
            let rows = if request.version == 0 {
                self.store.read_dataset(&dataset, DataType::Scenario, 0)
            } else {
                self.store
                    .read_dataset_versions(&dataset, DataType::Scenario)
            }
            .map_err(|failed| failed.to_string())?;
            // A saved scenario whose effective state is invalid is left out,
            // as every default read leaves one out (W10.6; ruling 421).
            for row in rows.into_iter().filter(|row| !crate::store::invalid(row)) {
                let Observation::Scenario(saved) = row else {
                    continue;
                };
                let meta = saved.meta.clone().unwrap_or_default();
                if !request.row_keys.is_empty() && !request.row_keys.contains(&meta.row_key) {
                    continue;
                }
                if request.version != 0 && meta.version != request.version {
                    continue;
                }
                scenarios.push(saved);
            }
        }
        Ok(meridian_domain::v1::ListScenariosReply { scenarios })
    }

    /// A batch checked as W10.4 says, each Money's instrument resolved, its
    /// envelope's stamps decided: the rows to record by dataset, or the
    /// refusal naming the item and field.
    fn checked(
        &self,
        field: &str,
        rows: Vec<Observation>,
        envelope: &Envelope,
    ) -> Result<BTreeMap<String, Vec<Observation>>, String> {
        let instance = publisher(envelope);
        let config = self.config();
        let sent_at = envelope.meta.as_ref().map_or(0, |m| m.published_at_ns);
        let version = self.plugin_version(&instance);
        let mut by_dataset: BTreeMap<String, Vec<Observation>> = BTreeMap::new();
        for (i, mut row) in rows.into_iter().enumerate() {
            let at = format!("{field}[{i}]");
            let data_type = row.data_type();
            let meta = row.meta().clone();
            let source = meta.source.clone().unwrap_or_default();
            if source.instance != instance {
                return Err(format!(
                    "{at}.meta.source.instance is not the instance recording it"
                ));
            }
            if !(1..=128).contains(&meta.row_key.chars().count()) {
                return Err(format!("{at}.meta.row_key is 1 to 128 characters"));
            }
            if !config.declared_by(&source.dataset, &instance) {
                return Err(format!(
                    "{at}.meta.source.dataset {:?} is no dataset this instance's catalogue declares",
                    source.dataset
                ));
            }
            if !config.serves(&source.dataset, data_type) {
                return Err(format!(
                    "{at}: {} does not serve {}",
                    source.dataset,
                    data_type.name()
                ));
            }
            if !source.venue_id.is_empty() && !source.venue_id.starts_with("VEN-") {
                return Err(format!(
                    "{at}.meta.source.venue_id {:?} is not a venue master ID; a venue not held travels as reported in unconverted",
                    source.venue_id
                ));
            }
            // A saved scenario names its objects in its shocks, and no
            // subject (contract v28, W12.8).
            if data_type == DataType::Scenario {
                if !meta.subjects.is_empty() {
                    return Err(format!(
                        "{at}.meta.subjects: a saved scenario names no subject; its shocks name \
                         their objects"
                    ));
                }
            } else if !(1..=8).contains(&meta.subjects.len()) {
                return Err(format!(
                    "{at}.meta.subjects names {}; 1 to 8",
                    meta.subjects.len()
                ));
            }
            // The filings slice's subjects in their roles (contract v28,
            // W10.4): a company by its platform record, in a role its kind
            // gives a company; and what each kind must name.
            if data_type.filings() {
                self.filings_subjects_refused(&at, data_type, &meta.subjects)?;
                if meta.valid_from_ns <= 0 {
                    return Err(format!(
                        "{at}.meta.valid_from_ns is required: when it became true, never 0"
                    ));
                }
            }
            for (j, subject) in meta.subjects.iter().enumerate() {
                if !(1..=64).contains(&subject.entity_id.chars().count()) {
                    return Err(format!(
                        "{at}.meta.subjects[{j}].entity_id is 1 to 64 characters"
                    ));
                }
                if data_type.filings() {
                    continue;
                }
                if subject.entity_id.starts_with(COMPANY_PREFIX) {
                    return Err(format!(
                        "{at}.meta.subjects[{j}].entity_id {:?}: a company is no subject of {}",
                        subject.entity_id,
                        data_type.name()
                    ));
                }
                let Some((class, kind)) = self.subject_record(&subject.entity_id)? else {
                    return Err(format!(
                        "{at}.meta.subjects[{j}].entity_id {:?} is no record the deployment holds",
                        subject.entity_id
                    ));
                };
                // A rate's subject is a referential record (contract v27,
                // W1.1, W10.4): an interest rate, a rate curve or an index.
                if data_type == DataType::Rate {
                    if class != AssetClass::Referential as i32 {
                        return Err(format!(
                            "{at}.meta.subjects[{j}].entity_id {:?} is no referential record; a \
                             rate's subject is an interest rate, a rate curve or an index",
                            subject.entity_id
                        ));
                    }
                    if kind == InstrumentType::RateCurve as i32 {
                        if let Observation::Rate(rate) = &row {
                            if rate.tenor.is_none() && rate.maturity_date.is_empty() {
                                return Err(format!(
                                    "{at}.tenor: a point of the rate curve {:?} names its \
                                     tenor or its maturity_date",
                                    subject.entity_id
                                ));
                            }
                        }
                    }
                }
            }
            // Contract v28 (ruling 561, W10.4): the series key on the
            // envelope, the underlying the row's subject.
            self.series_key_refused(&at, &row)?;
            meridian_domain::date::optional(
                &format!("{at}.meta.business_date"),
                &meta.business_date,
            )?;
            // A statistic is a value per trading date (contract v28, W10.4).
            if data_type == DataType::Derived && meta.business_date.is_empty() {
                return Err(format!(
                    "{at}.meta.business_date is required: a derived value's business date"
                ));
            }
            if data_type == DataType::Statistic && meta.business_date.is_empty() {
                return Err(format!(
                    "{at}.meta.business_date is required: a statistic's trading date"
                ));
            }
            // Real-time data is keyed by UTC instants, with no business date
            // (contract v19; ruling 3 of 2026-10-09); nor is an outcome a
            // daily value (contract v23, the lake plan's vL1c, item 3).
            if !data_type.dated() && !meta.business_date.is_empty() {
                return Err(format!(
                    "{at}.meta.business_date: {} names no business date; it is keyed by its \
                     valid time",
                    match data_type {
                        DataType::Trade => "a trade",
                        DataType::Quote => "a quote",
                        DataType::CorporateAction => "a corporate action, dated by its own fields,",
                        kind if kind.filings() =>
                            "a row of the filings slice, dated by its own fields,",
                        _ => "an outcome",
                    }
                ));
            }
            if meta.source_times.len() > 6 {
                return Err(format!(
                    "{at}.meta.source_times names {}; at most 6",
                    meta.source_times.len()
                ));
            }
            let mut kinds = BTreeSet::new();
            for (j, time) in meta.source_times.iter().enumerate() {
                if SourceTimeKind::try_from(time.kind).unwrap_or(SourceTimeKind::Unspecified)
                    == SourceTimeKind::Unspecified
                {
                    return Err(format!("{at}.meta.source_times[{j}].kind is unspecified"));
                }
                if !kinds.insert(time.kind) {
                    return Err(format!("{at}.meta.source_times[{j}].kind is named twice"));
                }
            }
            if meta.unconverted.len() > 16 {
                return Err(format!(
                    "{at}.meta.unconverted names {}; at most 16",
                    meta.unconverted.len()
                ));
            }
            // The edge's judgment (contract v28, W10.4): a row not valid
            // says why, each reason one the dictionary names.
            if rank(meta.quality) != QualityState::Valid as i32 {
                for (j, reason) in meta.quality_reasons.iter().enumerate() {
                    if QualityReason::try_from(*reason).unwrap_or(QualityReason::Unspecified)
                        == QualityReason::Unspecified
                    {
                        return Err(refusal_with_status(
                            RefusalStatus::InvalidArgument,
                            &[format!("{at}.meta.quality_reasons[{j}]")],
                            format!(
                                "{at}.meta.quality_reasons[{j}] is unspecified on a row whose \
                                 state is not valid"
                            ),
                        ));
                    }
                }
            }
            if let Observation::Price(price) = &row {
                if PriceKind::try_from(price.kind).unwrap_or(PriceKind::Unspecified)
                    == PriceKind::Unspecified
                {
                    return Err(format!("{at}.kind is unspecified"));
                }
                if PriceBasis::try_from(price.basis).unwrap_or(PriceBasis::Unspecified)
                    == PriceBasis::Unspecified
                {
                    return Err(format!("{at}.basis is unspecified"));
                }
                if price.price.is_none() && meta.unconverted.is_empty() {
                    return Err(format!(
                        "{at}.price is required, or its value as reported in unconverted"
                    ));
                }
            }
            // Each Money names its cash instrument (decisions/023 as amended).
            let mut instruments = BTreeSet::new();
            for (name, money) in row.moneys_mut() {
                let path = format!("{at}.{name}");
                let instrument = match asset_of(&path, money)? {
                    Asset::Code(code) => self.cash_instrument(&code, meta.valid_from_ns)?,
                    Asset::Instrument(instrument) => {
                        if !self.subject_held(&instrument)? {
                            return Err(format!(
                                "{path}.instrument_id {instrument:?} is no record the deployment holds"
                            ));
                        }
                        instrument
                    }
                    Asset::Both { code, instrument } => {
                        let named = self.cash_instrument(&code, meta.valid_from_ns)?;
                        if named != instrument {
                            return Err(format!(
                                "{path}: currency_code {code} names {named}, and instrument_id names {instrument}; both set name the same asset"
                            ));
                        }
                        instrument
                    }
                };
                money.instrument_id = instrument.clone();
                instruments.insert(instrument);
            }
            if data_type == DataType::Bar && instruments.len() > 1 {
                return Err(format!(
                    "{at}: a bar's prices are in one asset, and these are in {}",
                    instruments.len()
                ));
            }
            if data_type == DataType::Quote && instruments.len() > 1 {
                return Err(format!(
                    "{at}: a quote's two sides are in one asset, and these are in {}",
                    instruments.len()
                ));
            }
            match &row {
                Observation::Trade(trade) => trade_refused(&at, trade)?,
                Observation::Quote(quote) => quote_refused(&at, quote)?,
                Observation::Outcome(outcome) => outcome_refused(&at, outcome)?,
                Observation::Rate(rate) => rate_refused(&at, rate)?,
                Observation::Statistic(statistic) => statistic_refused(&at, statistic)?,
                Observation::Series(series) => series_refused(&at, series)?,
                Observation::Derived(value) => derived_refused(&at, value)?,
                Observation::Scenario(saved) => self.scenario_refused(&at, saved)?,
                Observation::CorporateAction(action) => corporate_action_refused(&at, action)?,
                Observation::Filing(filing) => filings::filing_refused(&at, filing)?,
                Observation::ReportedFact(fact) => filings::reported_fact_refused(&at, fact)?,
                Observation::RatingAction(action) => filings::rating_action_refused(&at, action)?,
                Observation::HoldingsReport(report) => {
                    filings::holdings_report_refused(&at, report)?
                }
                Observation::Holding(holding) => filings::holding_refused(&at, holding)?,
                _ => {}
            }
            if let Observation::Bar(bar) = &row {
                if let Some(volume) = &bar.volume {
                    let exact = meridian_domain::exact::Exact::from_wire(volume)
                        .map_err(|out| format!("{at}.volume {out}"))?;
                    if exact < meridian_domain::exact::Exact::ZERO {
                        return Err(format!("{at}.volume is never negative"));
                    }
                }
            }
            // What the lake decides, never a plugin's word.
            let meta = row.meta_mut();
            let source = meta.source.get_or_insert_with(Default::default);
            source.plugin_version = version.clone();
            meta.sent_at_ns = sent_at;
            meta.version = 0;
            meta.sequence = 0;
            meta.previous_sequence = 0;
            meta.recorded_at_ns = 0;
            meta.closed_day = None;
            by_dataset
                .entry(source.dataset.clone())
                .or_default()
                .push(row);
        }
        Ok(by_dataset)
    }

    /// Record a checked batch, publish each row recorded, settle the want it
    /// answers, and answer.
    pub(crate) fn record(
        &self,
        field: &str,
        rows: Vec<Observation>,
        want_id: &str,
        envelope: &Envelope,
    ) -> Result<RecordObservationsReply, String> {
        let bound = match field {
            "prices" => bounds::RECORD_PRICES_REQUEST_PRICES_COUNT,
            "trades" => bounds::RECORD_TRADES_REQUEST_TRADES_COUNT,
            "quotes" => bounds::RECORD_QUOTES_REQUEST_QUOTES_COUNT,
            "outcomes" => bounds::RECORD_OUTCOMES_REQUEST_OUTCOMES_COUNT,
            "rates" => bounds::RECORD_RATES_REQUEST_RATES_COUNT,
            "statistics" => bounds::RECORD_STATISTICS_REQUEST_STATISTICS_COUNT,
            "series" => bounds::RECORD_SERIES_REQUEST_SERIES_COUNT,
            "values" => bounds::RECORD_DERIVED_SERIES_REQUEST_VALUES_COUNT,
            "scenarios" => bounds::RECORD_SCENARIOS_REQUEST_SCENARIOS_COUNT,
            // Two requests name their rows `actions` (contract v28).
            "actions"
                if rows
                    .first()
                    .is_some_and(|row| row.data_type() == DataType::RatingAction) =>
            {
                bounds::RECORD_RATING_ACTIONS_REQUEST_ACTIONS_COUNT
            }
            "actions" => bounds::RECORD_CORPORATE_ACTIONS_REQUEST_ACTIONS_COUNT,
            "filings" => bounds::RECORD_FILINGS_REQUEST_FILINGS_COUNT,
            "facts" => bounds::RECORD_REPORTED_FACTS_REQUEST_FACTS_COUNT,
            "reports" => bounds::RECORD_HOLDINGS_REPORTS_REQUEST_REPORTS_COUNT,
            "holdings" => bounds::RECORD_HOLDINGS_REQUEST_HOLDINGS_COUNT,
            _ => bounds::RECORD_BARS_REQUEST_BARS_COUNT,
        };
        if !bound.admits(rows.len()) {
            return Err(format!(
                "{field} holds {} rows; a batch is {} to {}",
                rows.len(),
                bound.least,
                bound.most
            ));
        }
        let by_dataset = self.checked(field, rows, envelope)?;
        let config = self.config();
        let now = self.clock.now_ns();
        let mut reply = RecordObservationsReply::default();
        let mut datasets = BTreeSet::new();
        let mut subjects = Vec::new();
        for (dataset, mut rows) in by_dataset {
            datasets.insert(dataset.clone());
            self.flag_closed_days(&dataset, &mut rows);
            let done = if config.licence(&dataset).kept {
                self.store.record(&dataset, rows, now)
            } else {
                let readers: Vec<String> = self
                    .wants
                    .lock()
                    .expect("wants poisoned")
                    .open
                    .get(want_id)
                    .map(|w| w.asked_by.iter().cloned().collect())
                    .unwrap_or_else(|| config.entitled(&dataset));
                self.store.serve_unkept(&dataset, rows, &readers, now)
            }
            .map_err(|failed| failed.to_string())?;
            reply.recorded += done.recorded;
            reply.restated += done.restated;
            reply.unchanged += done.unchanged;
            // Each real-time row's age at the `lake` hop, on commit (W4.16;
            // the latency spec's "The hops"; ruling 606): sent to the
            // monitor with the component's next report.
            let committed = self.clock.now_ns();
            for row in &done.rows {
                subjects.extend(row.subjects());
                self.publish(&dataset, row);
                if row.data_type().real_time() {
                    meridian_domain::latency::recorder().record(
                        &dataset,
                        meridian_domain::v1::AgeHop::Lake,
                        committed - meridian_domain::latency::source_ns(row.meta()),
                    );
                }
            }
            let trades = done
                .rows
                .iter()
                .filter(|row| row.data_type() == DataType::Trade)
                .count() as u64;
            if trades > 0 {
                self.trades_recorded
                    .fetch_add(trades, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let heads = self.store.heads().map_err(|failed| failed.to_string())?;
        reply.watermark = Some(watermark(&heads, &datasets));
        // What the `lake_record_to_delivery_ms` band reads: from the batch
        // sent to its rows published.
        let sent_at = envelope.meta.as_ref().map_or(0, |m| m.published_at_ns);
        if sent_at > 0 && reply.recorded + reply.restated > 0 {
            tracing::info!(
                lake_record_to_delivery_ms = (self.clock.now_ns() - sent_at).max(0) / 1_000_000,
                recorded = reply.recorded,
                restated = reply.restated,
                "a batch recorded and published"
            );
        }
        if !want_id.is_empty() {
            self.settle(
                want_id,
                &subjects,
                WantChangeKind::Answered,
                0,
                &publisher(envelope),
                now,
            );
        }
        Ok(reply)
    }

    /// W10.11 (contract v27; ruling 481): execution costs, the analytics
    /// component's alone, recorded whole or refused naming the row and
    /// field, in the dataset `analytics:execution_cost`, which no catalogue
    /// declares. Each row is keyed by execution and benchmark: the same
    /// value again a repeat, a different one the next version. Never
    /// published: a reader reads them within its account scope.
    pub(crate) fn record_execution_costs(
        &self,
        request: RecordExecutionCostsRequest,
        envelope: &Envelope,
    ) -> Result<RecordObservationsReply, String> {
        let invalid = |field: &str, words: String| {
            meridian_bus::refusal_with_status(
                meridian_bus::RefusalStatus::InvalidArgument,
                &[field.to_string()],
                words,
            )
        };
        if publisher(envelope) != analytics_instance() {
            return Err(meridian_bus::refusal_with_status(
                meridian_bus::RefusalStatus::PermissionDenied,
                &[],
                "execution costs are the analytics component's alone to record",
            ));
        }
        if request.costs.is_empty() || request.costs.len() > 500 {
            return Err(invalid(
                "costs",
                format!(
                    "costs holds {} rows; a batch is 1 to 500",
                    request.costs.len()
                ),
            ));
        }
        let mut rows = Vec::with_capacity(request.costs.len());
        for (n, mut cost) in request.costs.into_iter().enumerate() {
            let at = format!("costs[{n}]");
            if cost.execution_id.is_empty() {
                return Err(invalid(
                    &format!("{at}.execution_id"),
                    "names its execution".into(),
                ));
            }
            if cost.account_id.is_empty() {
                return Err(invalid(
                    &format!("{at}.account_id"),
                    "names its account".into(),
                ));
            }
            let benchmark = TcaBenchmark::try_from(cost.benchmark)
                .ok()
                .filter(|b| *b != TcaBenchmark::Unspecified)
                .ok_or_else(|| invalid(&format!("{at}.benchmark"), "names its benchmark".into()))?;
            let meta = cost.meta.get_or_insert_with(Default::default);
            if meta.subjects.is_empty() {
                return Err(invalid(
                    &format!("{at}.meta.subjects"),
                    "names the execution's instrument".into(),
                ));
            }
            if meta.row_key.is_empty() {
                // `EXE-…:arrival`, as fixtures/lake/list-execution-costs.yaml.
                meta.row_key = format!(
                    "{}:{}",
                    cost.execution_id,
                    benchmark
                        .as_str_name()
                        .trim_start_matches("TCA_BENCHMARK_")
                        .to_ascii_lowercase()
                );
            }
            let source = meta.source.get_or_insert_with(Default::default);
            source.dataset = lake::EXECUTION_COST_DATASET.to_string();
            source.instance = lake::ANALYTICS_INSTANCE.to_string();
            meta.version = 0;
            meta.sequence = 0;
            meta.previous_sequence = 0;
            meta.recorded_at_ns = 0;
            meta.closed_day = None;
            rows.push(Observation::ExecutionCost(cost));
        }
        let done = self
            .store
            .record(lake::EXECUTION_COST_DATASET, rows, self.clock.now_ns())
            .map_err(|failed| failed.to_string())?;
        let heads = self.store.heads().map_err(|failed| failed.to_string())?;
        Ok(RecordObservationsReply {
            recorded: done.recorded,
            restated: done.restated,
            unchanged: done.unchanged,
            watermark: Some(watermark(
                &heads,
                &[lake::EXECUTION_COST_DATASET.to_string()].into(),
            )),
        })
    }

    /// W10.11 (contract v27; ruling 481): one account's execution costs, by
    /// execution, by order or by trade date, as of a recorded time; the
    /// lake's first account-scoped read. The sidecar checks the account
    /// against the reader's read scope, and the lake again against the
    /// scope the envelope carries: outside it, PERMISSION_DENIED naming
    /// `account_id`.
    pub(crate) fn list_execution_costs(
        &self,
        request: ListExecutionCostsRequest,
        envelope: &Envelope,
    ) -> Result<ListExecutionCostsReply, String> {
        let invalid = |field: &str, words: &str| {
            meridian_bus::refusal_with_status(
                meridian_bus::RefusalStatus::InvalidArgument,
                &[field.to_string()],
                words,
            )
        };
        if request.account_id.is_empty() {
            return Err(invalid("account_id", "a read names one account"));
        }
        if let Some(meta) = envelope.meta.as_ref() {
            if meta.account_scope_applies && !meta.account_scope.contains(&request.account_id) {
                return Err(meridian_bus::refusal_with_status(
                    meridian_bus::RefusalStatus::PermissionDenied,
                    &["account_id".to_string()],
                    format!(
                        "account {} is outside this reader's scope",
                        request.account_id
                    ),
                ));
            }
        }
        if !bounds::LIST_EXECUTION_COSTS_REQUEST_EXECUTION_IDS_COUNT
            .admits(request.execution_ids.len())
        {
            return Err(invalid("execution_ids", "at most 500 executions"));
        }
        if !request.trade_date.is_empty() && Date::parse(&request.trade_date).is_err() {
            return Err(invalid("trade_date", "a date is YYYY-MM-DD"));
        }
        let page = match request.page_size {
            0 => 100,
            n => n.min(500),
        } as usize;
        let skip: usize = if request.cursor.is_empty() {
            0
        } else {
            request
                .cursor
                .parse()
                .map_err(|_| invalid("cursor", "not a cursor this lake wrote"))?
        };
        let mut costs: Vec<ExecutionCost> = self
            .store
            .read_dataset(
                lake::EXECUTION_COST_DATASET,
                DataType::ExecutionCost,
                request.as_of_ns,
            )
            .map_err(|failed| failed.to_string())?
            .into_iter()
            // Left out where invalid, as a default read leaves it (W10.6).
            .filter(|row| !crate::store::invalid(row))
            .filter_map(|row| match row {
                Observation::ExecutionCost(cost) => Some(cost),
                _ => None,
            })
            .filter(|cost| cost.account_id == request.account_id)
            .filter(|cost| {
                request.execution_ids.is_empty()
                    || request.execution_ids.contains(&cost.execution_id)
            })
            .filter(|cost| request.order_id.is_empty() || cost.order_id == request.order_id)
            .filter(|cost| {
                request.trade_date.is_empty()
                    || cost
                        .meta
                        .as_ref()
                        .is_some_and(|meta| meta.business_date == request.trade_date)
            })
            .filter(|cost| {
                request.benchmarks.is_empty() || request.benchmarks.contains(&cost.benchmark)
            })
            .collect();
        costs.sort_by(|a, b| (&a.execution_id, a.benchmark).cmp(&(&b.execution_id, b.benchmark)));
        let next = skip + page;
        let next_cursor = if next < costs.len() {
            next.to_string()
        } else {
            String::new()
        };
        let mut reply = ListExecutionCostsReply {
            costs: costs.into_iter().skip(skip).take(page).collect(),
            next_cursor,
        };
        meridian_domain::money::fill_known(&mut reply);
        Ok(reply)
    }

    /// Contract v23 (W10.4; the lake plan's vL1c, items 8 to 11): each row
    /// with a business date -- a daily price or bar -- dated on a closed day
    /// of its dataset's calendar (DatasetDeclaration.calendar_id), or
    /// outside its coverage, carries the flag, decided here and recorded as
    /// reported, never refused. A calendar not held, or a read not answered,
    /// leaves the rows unflagged: the instrument store reports a calendar it
    /// does not hold on the read itself (W3.18), and CalendarApplied
    /// re-checks the rows once it lands.
    fn flag_closed_days(&self, dataset: &str, rows: &mut [Observation]) {
        let config = self.config();
        let Some(calendar_id) = config
            .declaration(dataset)
            .map(|declaration| declaration.calendar_id.clone())
            .filter(|id| !id.is_empty())
        else {
            return;
        };
        let dates: Vec<Date> = rows
            .iter()
            .filter(|row| row.data_type().dated())
            .filter_map(|row| Date::parse(&row.meta().business_date).ok())
            .collect();
        let (Some(first), Some(last)) = (dates.iter().min(), dates.iter().max()) else {
            return;
        };
        let reply = match self.read_calendar(&calendar_id, *first, *last) {
            Ok(reply) => reply,
            Err(failed) => {
                tracing::warn!(dataset, calendar_id, %failed, "the dataset's calendar was not read; recorded unflagged");
                return;
            }
        };
        if !reply.found {
            tracing::info!(
                dataset,
                calendar_id,
                "the dataset's calendar is not held; recorded unflagged and reported missing"
            );
            return;
        }
        for row in rows.iter_mut() {
            if !row.data_type().dated() {
                continue;
            }
            let Ok(date) = Date::parse(&row.meta().business_date) else {
                continue;
            };
            row.meta_mut().closed_day = lake::closed_day(&reply, date);
        }
    }

    fn read_calendar(
        &self,
        calendar_id: &str,
        from: Date,
        to: Date,
    ) -> Result<meridian_domain::v1::ReadCalendarReply, String> {
        self.ask(
            READ_CALENDAR,
            "meridian.v1.ReadCalendarRequest",
            meridian_domain::v1::ReadCalendarRequest {
                calendar_id: calendar_id.to_string(),
                from_date: from.to_string(),
                to_date: to.to_string(),
                as_of_ns: 0,
            },
        )
    }

    /// Contract v23 (W10.4; the lake plan's vL1c, item 10): a calendar's new
    /// version re-checks the kept rows of every dataset following it, and a
    /// row whose day the version changes -- closed, open, outside coverage,
    /// or a holiday renamed -- has the re-check journalled beside it as an
    /// amendment, never a new version: the source restated nothing.
    pub fn recheck(&self, event: &meridian_domain::v1::CalendarAppliedEvent) {
        if event.calendar_id.is_empty() {
            return;
        }
        let config = self.config();
        let now = self.clock.now_ns();
        for (dataset, held) in &config.datasets {
            let follows = held
                .declaration
                .as_ref()
                .is_some_and(|declaration| declaration.calendar_id == event.calendar_id);
            if !follows {
                continue;
            }
            if let Err(failed) = self.recheck_dataset(dataset, event, now) {
                tracing::warn!(dataset, calendar_id = event.calendar_id, %failed, "the closed-day flag was not re-checked");
            }
        }
    }

    fn recheck_dataset(
        &self,
        dataset: &str,
        event: &meridian_domain::v1::CalendarAppliedEvent,
        now: i64,
    ) -> Result<usize, String> {
        let Some((first, last)) = self
            .store
            .dated_range(dataset)
            .map_err(|failed| failed.to_string())?
        else {
            return Ok(0);
        };
        let (first, last) = (
            Date::parse(&first).map_err(|failed| failed.to_string())?,
            Date::parse(&last).map_err(|failed| failed.to_string())?,
        );
        let reply = self.read_calendar(&event.calendar_id, first, last)?;
        if !reply.found {
            return Ok(0);
        }
        let mut dates = Vec::new();
        let mut day = Some(first);
        while let Some(date) = day.filter(|date| *date <= last) {
            dates.push(date.to_string());
            day = date.next();
        }
        let rows = self
            .store
            .dated_on(dataset, &dates)
            .map_err(|failed| failed.to_string())?;
        let status = |flag: Option<&meridian_domain::v1::DateCheck>| {
            flag.map(|check| (check.reason, check.holiday_name.clone()))
        };
        let amendments: Vec<crate::store::ClosedDayAmendment> = rows
            .iter()
            .filter(|row| row.data_type().dated())
            .filter_map(|row| {
                let date = Date::parse(&row.meta().business_date).ok()?;
                let now_flag = lake::closed_day(&reply, date);
                (status(now_flag.as_ref()) != status(row.meta().closed_day.as_ref())).then(|| {
                    crate::store::ClosedDayAmendment {
                        dataset: dataset.to_string(),
                        sequence: row.meta().sequence,
                        closed_day: now_flag,
                        why: format!(
                            "calendar {} version {} applied",
                            event.calendar_id, reply.version
                        ),
                        at_ns: now,
                    }
                })
            })
            .collect();
        if !amendments.is_empty() {
            self.store
                .amend_closed_day(&amendments)
                .map_err(|failed| failed.to_string())?;
            tracing::info!(
                dataset,
                calendar_id = event.calendar_id,
                version = reply.version,
                amended = amendments.len(),
                "the closed-day flag re-checked on a calendar's new version"
            );
        }
        Ok(amendments.len())
    }

    fn publish(&self, dataset: &str, row: &Observation) {
        let (payload_type, payload) = match row {
            // Never published: read within each reader's scope (W10.11); a
            // series' terms change rarely and are read (contract v28, W10.5).
            Observation::ExecutionCost(_)
            | Observation::Series(_)
            | Observation::Derived(_)
            | Observation::Scenario(_) => return,
            // Read, not heard (contract v28, W10.5).
            Observation::ReportedFact(_)
            | Observation::RatingAction(_)
            | Observation::HoldingsReport(_)
            | Observation::Holding(_) => return,
            // Every version, never conflated (contract v28, W10.5): a filing
            // is an event, not a value in force.
            Observation::Filing(filing) => (
                "meridian.v1.FilingsRecordedEvent",
                meridian_domain::v1::FilingsRecordedEvent {
                    filing: Some(filing.clone()),
                }
                .encode_to_vec(),
            ),
            // Every version, never conflated (contract v28, W10.5): an
            // announcement is an event, not a value in force.
            Observation::CorporateAction(action) => (
                "meridian.v1.CorporateActionsRecordedEvent",
                CorporateActionsRecordedEvent {
                    action: Some(action.clone()),
                }
                .encode_to_vec(),
            ),
            Observation::Statistic(statistic) => (
                "meridian.v1.StatisticsRecordedEvent",
                StatisticsRecordedEvent {
                    statistic: Some(statistic.clone()),
                }
                .encode_to_vec(),
            ),
            Observation::Price(price) => (
                "meridian.v1.PricesRecordedEvent",
                PricesRecordedEvent {
                    price: Some(price.clone()),
                }
                .encode_to_vec(),
            ),
            Observation::Bar(bar) => (
                "meridian.v1.BarsRecordedEvent",
                BarsRecordedEvent {
                    bar: Some(bar.clone()),
                }
                .encode_to_vec(),
            ),
            Observation::Trade(trade) => (
                "meridian.v1.TradesRecordedEvent",
                TradesRecordedEvent {
                    trade: Some(trade.clone()),
                }
                .encode_to_vec(),
            ),
            Observation::Quote(quote) => (
                "meridian.v1.QuotesRecordedEvent",
                QuotesRecordedEvent {
                    quote: Some(quote.clone()),
                }
                .encode_to_vec(),
            ),
            // Every version, never conflated (contract v23, the lake plan's
            // vL1c, item 4): the receive keys no outcome.
            Observation::Outcome(outcome) => (
                "meridian.v1.OutcomesRecordedEvent",
                OutcomesRecordedEvent {
                    outcome: Some(outcome.clone()),
                }
                .encode_to_vec(),
            ),
            // Conflated by the receive per subject, dataset, kind, tenor,
            // maturity and convention (contract v27, W10.5).
            Observation::Rate(rate) => (
                "meridian.v1.RatesRecordedEvent",
                RatesRecordedEvent {
                    rate: Some(rate.clone()),
                }
                .encode_to_vec(),
            ),
        };
        let topic = recorded_topic(dataset, row.data_type());
        if let Err(failed) = self.bus.publish(&topic, payload_type, payload, None, None) {
            // Recorded is recorded: a reader catches up by subject.
            tracing::warn!(topic, %failed, "a row was recorded and not published");
        }
    }

    /// A want's subjects answered or declined, recorded; a want every
    /// subject of which is settled, and not standing, closed.
    fn settle(
        &self,
        want_id: &str,
        subjects: &[String],
        kind: WantChangeKind,
        reason: i32,
        by: &str,
        now: i64,
    ) {
        let mut wants = self.wants.lock().expect("wants poisoned");
        let Some(open) = wants.open.get_mut(want_id) else {
            return;
        };
        let mine: Vec<String> = subjects
            .iter()
            .filter(|s| open.event.subjects.iter().any(|w| &w.entity_id == *s))
            .cloned()
            .collect();
        open.settled.extend(mine.iter().cloned());
        let change = WantChange {
            want_id: want_id.to_string(),
            dataset: open.event.dataset.clone(),
            kind,
            subjects: mine,
            reason,
            by: vec![by.to_string()],
            at_ns: now,
            want: None,
        };
        let done = !open.event.standing && open.settled.len() >= open.event.subjects.len();
        if done {
            wants.open.remove(want_id);
            wants.by_key.retain(|_, id| id != want_id);
        }
        drop(wants);
        if let Err(failed) = self.store.record_want(&change) {
            tracing::warn!(%failed, "a want's change was not recorded");
        }
    }

    /// Ask the instances serving what a read could not answer, coalescing
    /// identical asks and asking again only after a minute. From contract
    /// v27 a provisional (`LCL-`) subject is wanted as any other (ruling 484,
    /// W10.6, W10.7): the data plugin answers it by the identifiers the
    /// record carries, or declines it; only valuing a local line is
    /// refused, by the analytics component.
    fn want(&self, asks: Vec<read::Ask>, reader: &Reader) {
        let now = self.clock.now_ns();
        let config = self.config();
        let by = match reader {
            Reader::Plugin(instance) => instance.clone(),
            Reader::Core => "dashboard".to_string(),
        };
        for ask in asks {
            let (date, from, until) = match &ask.when {
                When::BusinessDate(date) => (date.to_string(), 0, 0),
                When::Range { from_ns, until_ns } => (String::new(), *from_ns, *until_ns),
                When::Latest { .. } | When::After(_) => (String::new(), 0, 0),
            };
            let mut kinds = ask.kinds.clone();
            kinds.sort();
            let key: WantKey = (
                ask.dataset.clone(),
                ask.data_type.name().to_string(),
                kinds.clone(),
                ask.interval_ns,
                date.clone(),
                from,
                until,
                ask.standing,
                ask.subjects.clone(),
            );
            let mut wants = self.wants.lock().expect("wants poisoned");
            let existing = wants.by_key.get(&key).cloned();
            let (want_id, publish) = match existing
                .and_then(|id| wants.open.get_mut(&id).map(|w| (id, w)))
            {
                Some((id, open)) => {
                    open.asked_by.insert(by.clone());
                    open.asked_at_ns = now;
                    let again = !open.event.standing && now - open.published_at_ns >= ASK_AGAIN_NS;
                    if again {
                        open.published_at_ns = now;
                    }
                    (id, again)
                }
                None => {
                    let id = uuid::Uuid::new_v4().to_string();
                    let cadence = config
                        .declaration(&ask.dataset)
                        .map_or(0, |d| i64::from(d.cadence));
                    let event = ObservationsWantedEvent {
                        want_id: id.clone(),
                        dataset: ask.dataset.clone(),
                        data_type: ask.data_type.name().to_string(),
                        subjects: ask
                            .subjects
                            .iter()
                            .map(|s| SubjectRef {
                                entity_id: s.clone(),
                                ..Default::default()
                            })
                            .collect(),
                        kinds: kinds.clone(),
                        interval_ns: ask.interval_ns,
                        business_date: date.clone(),
                        valid_from_ns: from,
                        valid_until_ns: until,
                        standing: ask.standing,
                    };
                    wants.open.insert(
                        id.clone(),
                        OpenWant {
                            event,
                            asked_by: [by.clone()].into(),
                            asked_at_ns: now,
                            published_at_ns: now,
                            cadence_ns: cadence * 1_000_000_000,
                            settled: BTreeSet::new(),
                        },
                    );
                    wants.by_key.insert(key, id.clone());
                    (id, true)
                }
            };
            let event = wants.open.get(&want_id).map(|w| w.event.clone());
            drop(wants);
            let Some(event) = event else { continue };
            if publish {
                if let Err(failed) = self.bus.publish(
                    OBSERVATIONS_WANTED,
                    "meridian.v1.ObservationsWantedEvent",
                    event.encode_to_vec(),
                    None,
                    None,
                ) {
                    tracing::warn!(%failed, "a want was not published");
                }
                let change = WantChange {
                    want_id: want_id.clone(),
                    dataset: event.dataset.clone(),
                    kind: WantChangeKind::Asked,
                    subjects: ask.subjects.clone(),
                    reason: 0,
                    by: vec![by.clone()],
                    at_ns: now,
                    want: Some(event),
                };
                if let Err(failed) = self.store.record_want(&change) {
                    tracing::warn!(%failed, "a want was not recorded");
                }
            }
        }
    }

    /// The standing wants no reader asked within their dataset's cadence
    /// (at least a minute), withdrawn (W10.7).
    pub fn sweep_wants(&self) {
        let now = self.clock.now_ns();
        let mut withdrawn = Vec::new();
        let mut again = Vec::new();
        {
            let mut wants = self.wants.lock().expect("wants poisoned");
            // Standing wants still asked for, delivered again as they were
            // first delivered: a data plugin that already holds one hears the same
            // want ID, and one that restarted resumes keeping it current. Not
            // a change to the want, so no record of it.
            for open in wants.open.values_mut() {
                let window = open.cadence_ns.max(ASK_AGAIN_NS) * 2;
                if open.event.standing
                    && now - open.asked_at_ns <= window
                    && now - open.published_at_ns >= REDELIVER_STANDING_NS
                {
                    open.published_at_ns = now;
                    again.push(open.event.clone());
                }
            }
            let stale: Vec<String> = wants
                .open
                .iter()
                .filter(|(_, w)| {
                    let window = w.cadence_ns.max(ASK_AGAIN_NS) * 2;
                    w.event.standing && now - w.asked_at_ns > window
                })
                .map(|(id, _)| id.clone())
                .collect();
            for id in stale {
                if let Some(open) = wants.open.remove(&id) {
                    wants.by_key.retain(|_, held| held != &id);
                    withdrawn.push(open);
                }
            }
        }
        for event in again {
            tracing::debug!(
                want_id = event.want_id,
                dataset = event.dataset,
                "a standing want delivered again"
            );
            if let Err(failed) = self.bus.publish(
                OBSERVATIONS_WANTED,
                "meridian.v1.ObservationsWantedEvent",
                event.encode_to_vec(),
                None,
                None,
            ) {
                tracing::warn!(%failed, "a standing want was not delivered again");
            }
        }
        for open in withdrawn {
            let event = WantWithdrawnEvent {
                want_id: open.event.want_id.clone(),
                dataset: open.event.dataset.clone(),
            };
            if let Err(failed) = self.bus.publish(
                WANT_WITHDRAWN,
                "meridian.v1.WantWithdrawnEvent",
                event.encode_to_vec(),
                None,
                None,
            ) {
                tracing::warn!(%failed, "a want's withdrawal was not published");
            }
            let change = WantChange {
                want_id: open.event.want_id.clone(),
                dataset: open.event.dataset.clone(),
                kind: WantChangeKind::Withdrawn,
                subjects: open
                    .event
                    .subjects
                    .iter()
                    .map(|s| s.entity_id.clone())
                    .collect(),
                reason: 0,
                by: vec!["lake".into()],
                at_ns: now,
                want: None,
            };
            if let Err(failed) = self.store.record_want(&change) {
                tracing::warn!(%failed, "a want's withdrawal was not recorded");
            }
        }
    }

    /// Rows past each dataset's retention removed, each removal recorded.
    pub fn apply_retention(&self) {
        for (dataset, rows) in self.rows_per_dataset() {
            tracing::info!(dataset, lake_rows_per_dataset = rows, "rows kept");
        }
        let now = self.clock.now_ns();
        let config = self.config();
        for dataset in config.datasets.keys() {
            let licence = config.licence(dataset);
            if licence.retention_days == 0 {
                continue;
            }
            let before = now - i64::from(licence.retention_days) * 86_400 * 1_000_000_000;
            let why = format!(
                "retention under the dataset's licence: {} days from when each row was recorded",
                licence.retention_days
            );
            match self.store.remove_before(dataset, before, &why, now) {
                Ok(0) => {}
                Ok(removed) => {
                    tracing::info!(dataset, removed, "rows past their retention removed")
                }
                Err(failed) => tracing::warn!(dataset, %failed, "retention was not applied"),
            }
        }
    }

    pub(crate) fn read(
        &self,
        request: ReadRequest,
        envelope: &Envelope,
    ) -> Result<ReadAnswer, String> {
        self.read_as(reader_of(envelope), request, true)
    }

    /// A read as `reader`; `wants` says whether what it finds missing is
    /// asked of the data plugins (a list read's; never QueryLake's, which
    /// is read-only, W10.10).
    pub(crate) fn read_as(
        &self,
        reader: Reader,
        request: ReadRequest,
        wants: bool,
    ) -> Result<ReadAnswer, String> {
        self.read_within(reader, request, wants, read::Narrowed::default())
    }

    /// [`Lake::read_as`], keeping only the rows `narrowed` keeps besides
    /// (contract v28, the filings slice: a fact's concepts, a holding's
    /// reports), before the page is cut.
    pub(crate) fn read_within(
        &self,
        reader: Reader,
        request: ReadRequest,
        wants: bool,
        narrowed: read::Narrowed,
    ) -> Result<ReadAnswer, String> {
        if !bounds::LIST_PRICES_REQUEST_SUBJECTS_COUNT.admits(request.subjects.len()) {
            return Err(format!(
                "subjects names {}; 1 to 500",
                request.subjects.len()
            ));
        }
        let ranged = request.valid_from_ns != 0 || request.valid_until_ns != 0;
        let after = request
            .after
            .as_ref()
            .filter(|watermark| !watermark.partitions.is_empty());
        let when = match request.data_type {
            // Trades (contract v19): a valid-time range, or the rows recorded
            // after a watermark (the plan's Q33); exactly one.
            DataType::Trade => match (ranged, after) {
                (true, None) => When::Range {
                    from_ns: request.valid_from_ns,
                    until_ns: request.valid_until_ns,
                },
                (false, Some(watermark)) => {
                    let mut at: BTreeMap<String, u64> = BTreeMap::new();
                    for (i, partition) in watermark.partitions.iter().enumerate() {
                        if at
                            .insert(partition.partition.clone(), partition.sequence)
                            .is_some()
                        {
                            return Err(format!(
                                "after_watermark.partitions[{i}] {:?} is named twice",
                                partition.partition
                            ));
                        }
                    }
                    When::After(at)
                }
                _ => {
                    return Err(
                        "a valid-time range or after_watermark: exactly one is given".into(),
                    )
                }
            },
            // Outcomes (contract v23) are read by ListOutcomes alone.
            DataType::Outcome => return Err("an outcome is read by ListOutcomes".into()),
            // Execution costs (contract v27) by ListExecutionCosts alone.
            DataType::ExecutionCost => {
                return Err("an execution cost is read by ListExecutionCosts".into())
            }
            // Quotes (contract v19): the latest in force at a time, or a
            // valid-time range; at most one.
            DataType::Quote => {
                if request.at_ns != 0 && ranged {
                    return Err("at_ns and a valid-time range: at most one is given".into());
                }
                if ranged {
                    When::Range {
                        from_ns: request.valid_from_ns,
                        until_ns: request.valid_until_ns,
                    }
                } else {
                    When::Latest {
                        at_ns: request.at_ns,
                    }
                }
            }
            // A listed series (contract v28): its terms in force at the as-of,
            // never a date or a range.
            DataType::Series => When::Latest { at_ns: 0 },
            // A saved scenario (contract v28) is read by ListScenarios.
            DataType::Scenario => return Err("a saved scenario is read by ListScenarios".into()),
            // A declared corporate action (contract v28, W10.6): every row
            // key's version in force, narrowed by its kind's date; or the
            // versions recorded after a watermark, whatever their dates.
            DataType::CorporateAction | DataType::Filing => match after {
                Some(watermark) => {
                    let mut at: BTreeMap<String, u64> = BTreeMap::new();
                    for (i, partition) in watermark.partitions.iter().enumerate() {
                        if at
                            .insert(partition.partition.clone(), partition.sequence)
                            .is_some()
                        {
                            return Err(format!(
                                "after_watermark.partitions[{i}] {:?} is named twice",
                                partition.partition
                            ));
                        }
                    }
                    When::After(at)
                }
                None => When::Range {
                    from_ns: 1,
                    until_ns: 0,
                },
            },
            // The filings slice's other kinds (contract v28, W10.6): every row
            // key's version in force, narrowed by its kind's date.
            DataType::ReportedFact | DataType::RatingAction | DataType::Holding => When::Range {
                from_ns: 1,
                until_ns: 0,
            },
            DataType::HoldingsReport => {
                return Err("a holdings report is read by ListHoldings, beside its holdings".into())
            }
            // A rate is read as a price is (contract v27, W10.6); and a
            // statistic (contract v28).
            DataType::Price
            | DataType::Bar
            | DataType::Rate
            | DataType::Statistic
            | DataType::Derived => {
                let given = [
                    request.at_ns != 0,
                    !request.business_date.is_empty(),
                    ranged,
                ];
                if given.iter().filter(|g| **g).count() > 1 {
                    return Err(
                        "at_ns, business_date and a valid-time range: at most one is given".into(),
                    );
                }
                if let Some(date) =
                    meridian_domain::date::optional("business_date", &request.business_date)?
                {
                    When::BusinessDate(date)
                } else if ranged {
                    if request.valid_until_ns != 0
                        && request.valid_until_ns <= request.valid_from_ns
                    {
                        return Err("valid_until_ns is after valid_from_ns".into());
                    }
                    When::Range {
                        from_ns: request.valid_from_ns,
                        until_ns: request.valid_until_ns,
                    }
                } else {
                    When::Latest {
                        at_ns: request.at_ns,
                    }
                }
            }
        };
        let sources = request.sources.unwrap_or_default();
        let chosen = [
            sources.default,
            !sources.named.is_empty(),
            sources.side_by_side,
        ]
        .iter()
        .filter(|c| **c)
        .count();
        if chosen > 1 {
            return Err("sources: one of default, named or side_by_side".into());
        }
        if sources.named.len() > 16 {
            return Err(format!(
                "sources.named names {}; at most 16",
                sources.named.len()
            ));
        }
        // The cut-off, carried by the cursor so every page reads as one.
        let (as_of, offset) = match cursor_of(&request.cursor)? {
            Some((as_of, offset)) => (as_of, offset),
            None => (
                if request.as_of_ns == 0 {
                    self.clock.now_ns()
                } else {
                    request.as_of_ns
                },
                0,
            ),
        };
        let page = match request.page_size {
            0 => 100,
            n => n.min(500) as usize,
        };
        let config = self.config();
        // A range of trades or quotes is one day of each dataset that could
        // answer it, as it declares its day (contract v19, W10.6).
        let mut when = when;
        if request.data_type.real_time() {
            if let When::Range { from_ns, until_ns } = when {
                let until_ns = if until_ns == 0 {
                    self.clock.now_ns().max(from_ns + 1)
                } else {
                    until_ns
                };
                for dataset in config.readable(&reader, request.data_type) {
                    if !sources.named.is_empty() && !sources.named.contains(&dataset) {
                        continue;
                    }
                    read::within_a_day(&dataset, config.declaration(&dataset), from_ns, until_ns)?;
                }
                when = When::Range { from_ns, until_ns };
            }
        }
        let priorities = self
            .store
            .priorities()
            .map_err(|failed| failed.to_string())?;
        let declined = self.declined.lock().expect("declined poisoned").clone();
        let aliases = self.store.aliases().map_err(|failed| failed.to_string())?;
        let read = Read {
            reader: reader.clone(),
            data_type: request.data_type,
            subjects: request
                .subjects
                .iter()
                .map(|s| s.entity_id.clone())
                .collect(),
            kinds: request.kinds,
            interval_ns: request.interval_ns,
            sources,
            when,
            as_of_ns: as_of,
            tenors: request.tenors,
            published_as_of_ns: request.published_as_of_ns,
            series_keys: request.series_keys,
            all_series: request.all_series,
            dates: request.dates,
            // A subject asked in a role (contract v28, W10.6).
            roles: request
                .subjects
                .iter()
                .filter(|s| s.role != SubjectRole::Unspecified as i32)
                .map(|s| (s.entity_id.clone(), s.role))
                .collect(),
            narrowed,
        };
        let answered = read::answer(
            self.store.as_ref(),
            &read,
            &Against {
                config: &config,
                priorities: &priorities,
                declined: &declined,
                aliases: &aliases,
                now_ns: self.clock.now_ns(),
            },
        )?;
        if offset == 0 && wants {
            self.want(answered.asks.clone(), &reader);
        }
        // What a catch-up cannot answer, per partition it named (contract
        // v19): a dataset not read, one not kept, or rows since the
        // watermark that retention removed.
        let mut answered = answered;
        if let When::After(watermark) = &read.when {
            let readable = config.readable(&reader, read.data_type);
            let heads = self.store.heads().map_err(|failed| failed.to_string())?;
            for (dataset, sequence) in watermark {
                let reason = if !readable.contains(dataset) {
                    Some(
                        if config.datasets.contains_key(dataset)
                            && config.serves(dataset, read.data_type)
                        {
                            UnansweredReason::NotEntitled
                        } else {
                            UnansweredReason::NotCovered
                        },
                    )
                } else if !config.licence(dataset).kept {
                    Some(UnansweredReason::NotKept)
                } else {
                    let head = heads.get(dataset).copied().unwrap_or(0);
                    let lowest = self
                        .store
                        .lowest_kept(dataset)
                        .map_err(|failed| failed.to_string())?;
                    let removed = match lowest {
                        Some(lowest) => lowest > sequence + 1,
                        None => head > *sequence,
                    };
                    (head > *sequence && removed).then_some(UnansweredReason::BeyondHistory)
                };
                if let Some(reason) = reason {
                    answered.not_served.push(read::NotServed {
                        subject: String::new(),
                        dataset: dataset.clone(),
                        field: String::new(),
                        reason,
                    });
                }
            }
        }
        let total = answered.rows.len();
        let rows: Vec<Observation> = answered.rows.into_iter().skip(offset).take(page).collect();
        let next = (offset + rows.len() < total).then(|| cursor(as_of, offset + rows.len()));
        let datasets: BTreeSet<String> = rows.iter().map(|r| r.dataset().to_string()).collect();
        let heads = self.store.heads().map_err(|failed| failed.to_string())?;
        let mut answered_from: BTreeSet<String> = datasets.clone();
        answered_from.extend(
            answered
                .not_served
                .iter()
                .filter(|n| config.datasets.contains_key(&n.dataset))
                .map(|n| n.dataset.clone()),
        );
        Ok(ReadAnswer {
            rows,
            unanswered: answered
                .not_served
                .into_iter()
                .map(|n| Unanswered {
                    subject: subject_ref(&n.subject),
                    dataset: n.dataset,
                    field: n.field,
                    reason: n.reason as i32,
                })
                .collect(),
            datasets: datasets
                .iter()
                .filter_map(|d| config.datasets.get(d))
                .map(|d| DatasetRef {
                    declaration: None,
                    unconverted_count: 0,
                    miss_count: 0,
                    ..d.clone()
                })
                .collect(),
            watermark: watermark(&heads, &answered_from),
            next_cursor: next.unwrap_or_default(),
        })
    }

    /// W10.6, contract v20: the last trade a data plugin's own instance
    /// recorded of each subject asked, per dataset and venue, with the watermark it was read at, so the
    /// plugin backfills a restart's gap from the venue's history. Its own
    /// rows alone, by no entitlement; refused from any other reader.
    /// W10.6, contract v27 (the rates slice): rates for 1 to 500 subjects,
    /// as prices are read, narrowed by kind and tenor; as of a recorded
    /// time, and optionally as the source had published them by a time.
    pub(crate) fn list_rates(
        &self,
        request: ListRatesRequest,
        envelope: &Envelope,
    ) -> Result<ListRatesReply, String> {
        if request.published_as_of_ns < 0 {
            return Err("published_as_of_ns is a time, or 0 to read by recorded time".into());
        }
        if !bounds::LIST_RATES_REQUEST_SUBJECTS_COUNT.admits(request.subjects.len()) {
            return Err(format!(
                "subjects names {}; 1 to 500",
                request.subjects.len()
            ));
        }
        for (i, tenor) in request.tenors.iter().enumerate() {
            if TenorUnit::try_from(tenor.unit).unwrap_or(TenorUnit::Unspecified)
                == TenorUnit::Unspecified
            {
                return Err(format!("tenors[{i}].unit is unspecified"));
            }
        }
        let answer = self.read(
            ReadRequest {
                data_type: DataType::Rate,
                subjects: request.subjects,
                kinds: request.kinds,
                interval_ns: 0,
                sources: request.sources,
                at_ns: request.at_ns,
                business_date: request.business_date,
                valid_from_ns: request.valid_from_ns,
                valid_until_ns: request.valid_until_ns,
                as_of_ns: request.as_of_ns,
                page_size: request.page_size,
                cursor: request.cursor,
                after: None,
                tenors: request.tenors,
                published_as_of_ns: request.published_as_of_ns,
                series_keys: Vec::new(),
                all_series: false,
                dates: None,
            },
            envelope,
        )?;
        Ok(ListRatesReply {
            rates: answer
                .rows
                .into_iter()
                .filter_map(|r| match r {
                    Observation::Rate(rate) => Some(rate),
                    _ => None,
                })
                .collect(),
            unanswered: answer.unanswered,
            datasets: answer.datasets,
            watermark: Some(answer.watermark),
            next_cursor: answer.next_cursor,
        })
    }

    /// W10.6, contract v28 (ruling 561): the listed series under 1 to 64
    /// underlyings, each at its version in force at the as-of, narrowed by
    /// listed type, expiration window and series key, and each naming the
    /// deployment's record for it where one is held -- matched, never
    /// minted. The type and window narrow a page once read, so a page may
    /// hold fewer rows than its size while its cursor goes on.
    pub(crate) fn list_series(
        &self,
        request: ListSeriesRequest,
        envelope: &Envelope,
    ) -> Result<ListSeriesReply, String> {
        if !bounds::LIST_SERIES_REQUEST_SUBJECTS_COUNT.admits(request.subjects.len()) {
            return Err(format!(
                "subjects names {}; 1 to 64",
                request.subjects.len()
            ));
        }
        if !bounds::LIST_SERIES_REQUEST_SERIES_KEYS_COUNT.admits(request.series_keys.len()) {
            return Err(format!(
                "series_keys names {}; at most 500",
                request.series_keys.len()
            ));
        }
        let listed = [
            InstrumentType::Unspecified,
            InstrumentType::ListedOption,
            InstrumentType::Future,
            InstrumentType::FutureOption,
        ];
        let kind = InstrumentType::try_from(request.instrument_type)
            .ok()
            .filter(|kind| listed.contains(kind))
            .ok_or("instrument_type is listed_option, future or future_option, or unspecified")?;
        let from = meridian_domain::date::optional("expiration_from", &request.expiration_from)?;
        let to = meridian_domain::date::optional("expiration_to", &request.expiration_to)?;
        if let (Some(from), Some(to)) = (&from, &to) {
            if to < from {
                return Err("expiration_to is on or after expiration_from".into());
            }
        }
        let underlyings: BTreeMap<String, i32> = request
            .subjects
            .iter()
            .map(|subject| {
                let kind = self
                    .subject_record(&subject.entity_id)
                    .ok()
                    .flatten()
                    .map_or(0, |(_, kind)| kind);
                (subject.entity_id.clone(), kind)
            })
            .collect();
        let all_series = request.series_keys.is_empty();
        // The page asked, filled after the type and the window are applied
        // (a filtered page is never short while rows remain): each read asks
        // only for what the page still lacks, so the cursor it answers is the
        // page's, never past a row left out.
        let want = match request.page_size {
            0 => 100,
            n => n.min(500),
        };
        let mut cursor = request.cursor.clone();
        let mut series = Vec::new();
        let mut unanswered = Vec::new();
        let mut datasets: Vec<meridian_domain::v1::DatasetRef> = Vec::new();
        let mut watermark = None;
        let next_cursor = loop {
            let lacking = want - series.len() as u32;
            let answer = self.read(
                ReadRequest {
                    data_type: DataType::Series,
                    subjects: request.subjects.clone(),
                    kinds: Vec::new(),
                    interval_ns: 0,
                    sources: request.sources.clone(),
                    at_ns: 0,
                    business_date: String::new(),
                    valid_from_ns: 0,
                    valid_until_ns: 0,
                    as_of_ns: request.as_of_ns,
                    page_size: lacking,
                    cursor: cursor.clone(),
                    after: None,
                    tenors: Vec::new(),
                    published_as_of_ns: 0,
                    series_keys: request.series_keys.clone(),
                    all_series,
                    dates: None,
                },
                envelope,
            )?;
            if watermark.is_none() {
                watermark = Some(answer.watermark.clone());
                unanswered = answer.unanswered.clone();
            }
            for dataset in answer.datasets {
                if !datasets.iter().any(|d| d.dataset == dataset.dataset) {
                    datasets.push(dataset);
                }
            }
            for row in answer.rows {
                let Observation::Series(mut listed) = row else {
                    continue;
                };
                let meta = listed.meta.clone().unwrap_or_default();
                let field = |path: &str| {
                    listed
                        .fields
                        .iter()
                        .find(|field| field.path == path)
                        .map(|field| field.value.clone())
                        .unwrap_or_default()
                };
                // Its type: a future names its product code; an option on a
                // future record is a future option (W1.24).
                let underlying = meta
                    .subjects
                    .first()
                    .and_then(|subject| underlyings.get(&subject.entity_id))
                    .copied()
                    .unwrap_or(0);
                let its_kind = if !field("product_code").is_empty() {
                    InstrumentType::Future
                } else if underlying == InstrumentType::Future as i32 {
                    InstrumentType::FutureOption
                } else {
                    InstrumentType::ListedOption
                };
                if kind != InstrumentType::Unspecified && kind != its_kind {
                    continue;
                }
                if from.is_some() || to.is_some() {
                    let expires = [
                        "expiration_date",
                        "last_trade_date",
                        "final_settlement_date",
                    ]
                    .iter()
                    .map(|path| field(path))
                    .find(|date| !date.is_empty())
                    .and_then(|date| Date::parse(&date).ok());
                    let Some(expires) = expires else {
                        continue;
                    };
                    if from.as_ref().is_some_and(|from| &expires < from)
                        || to.as_ref().is_some_and(|to| &expires > to)
                    {
                        continue;
                    }
                }
                listed.instrument_id = self.series_record(&meta.series_key);
                series.push(listed);
            }
            if answer.next_cursor.is_empty() || series.len() as u32 >= want {
                break answer.next_cursor;
            }
            cursor = answer.next_cursor;
        };
        Ok(ListSeriesReply {
            series,
            unanswered,
            datasets,
            watermark,
            next_cursor,
        })
    }

    /// The deployment's record for a series key, matched read-only by the
    /// instrument store (W3.1): empty where none is held, never minted
    /// (ruling 561).
    fn series_record(&self, key: &[Identifier]) -> String {
        if key.is_empty() {
            return String::new();
        }
        let reply: Result<ResolveIdentifierReply, String> = self.ask(
            RESOLVE_IDENTIFIER,
            "meridian.v1.ResolveIdentifierRequest",
            ResolveIdentifierRequest {
                identifiers: key.to_vec(),
                as_of_ns: self.clock.now_ns(),
                ..Default::default()
            },
        );
        match reply {
            Ok(reply) if reply.found && !reply.minted && !reply.provisional => reply.instrument_id,
            _ => String::new(),
        }
    }

    /// W10.6, contract v28 (G11): statistics for 1 to 64 subjects, by kind,
    /// a business date or a range of them, and series, point in time as
    /// every read is.
    pub(crate) fn list_statistics(
        &self,
        request: ListStatisticsRequest,
        envelope: &Envelope,
    ) -> Result<ListStatisticsReply, String> {
        if !bounds::LIST_STATISTICS_REQUEST_SUBJECTS_COUNT.admits(request.subjects.len()) {
            return Err(format!(
                "subjects names {}; 1 to 64",
                request.subjects.len()
            ));
        }
        if !bounds::LIST_STATISTICS_REQUEST_SERIES_KEYS_COUNT.admits(request.series_keys.len()) {
            return Err(format!(
                "series_keys names {}; at most 500",
                request.series_keys.len()
            ));
        }
        for (i, kind) in request.kinds.iter().enumerate() {
            if StatisticKind::try_from(*kind).unwrap_or(StatisticKind::Unspecified)
                == StatisticKind::Unspecified
            {
                return Err(format!("kinds[{i}] is unspecified"));
            }
        }
        let ranged = !request.from_date.is_empty() || !request.to_date.is_empty();
        if ranged && !request.business_date.is_empty() {
            return Err("business_date and from_date or to_date: at most one is given".into());
        }
        let dates = if ranged {
            let from = meridian_domain::date::optional("from_date", &request.from_date)?
                .unwrap_or(Date::of_utc_instant(0));
            let to = match meridian_domain::date::optional("to_date", &request.to_date)? {
                Some(to) => to,
                None => Date::of_utc_instant(self.clock.now_ns()),
            };
            if to < from {
                return Err("to_date is on or after from_date".into());
            }
            Some((from, to))
        } else {
            None
        };
        let answer = self.read(
            ReadRequest {
                data_type: DataType::Statistic,
                subjects: request.subjects,
                kinds: request.kinds,
                interval_ns: 0,
                sources: request.sources,
                at_ns: 0,
                business_date: request.business_date,
                // A range of dates reads every version in force, then by
                // the row's business date.
                valid_from_ns: if ranged { 1 } else { 0 },
                valid_until_ns: 0,
                as_of_ns: request.as_of_ns,
                page_size: request.page_size,
                cursor: request.cursor,
                after: None,
                tenors: Vec::new(),
                published_as_of_ns: 0,
                series_keys: request.series_keys,
                all_series: request.all_series,
                dates,
            },
            envelope,
        )?;
        Ok(ListStatisticsReply {
            statistics: answer
                .rows
                .into_iter()
                .filter_map(|r| match r {
                    Observation::Statistic(statistic) => Some(statistic),
                    _ => None,
                })
                .collect(),
            unanswered: answer.unanswered,
            datasets: answer.datasets,
            watermark: Some(answer.watermark),
            next_cursor: answer.next_cursor,
        })
    }

    /// W10.6, contract v28 (the filings slice): the declared corporate
    /// actions of 1 to 64 subjects -- a security, or its issuer as a company
    /// in the role asked -- by kind and a range of the kind's date (a cash
    /// dividend's ex-date), point in time by recorded time or by what the
    /// source had published; or the versions recorded after a watermark,
    /// whatever their dates, which is how a reader that lost some catches up
    /// (ruling 598).
    pub(crate) fn list_corporate_actions(
        &self,
        request: ListCorporateActionsRequest,
        envelope: &Envelope,
    ) -> Result<ListCorporateActionsReply, String> {
        for (i, kind) in request.kinds.iter().enumerate() {
            if CorporateActionKind::try_from(*kind).unwrap_or(CorporateActionKind::Unspecified)
                == CorporateActionKind::Unspecified
            {
                return Err(format!("kinds[{i}] is unspecified"));
            }
        }
        let asked = filings::Asked {
            data_type: DataType::CorporateAction,
            subjects: request.subjects,
            subjects_bound: bounds::LIST_CORPORATE_ACTIONS_REQUEST_SUBJECTS_COUNT,
            kinds: request.kinds,
            from_date: request.from_date,
            to_date: request.to_date,
            published_as_of_ns: request.published_as_of_ns,
            sources: request.sources,
            as_of_ns: request.as_of_ns,
            page_size: request.page_size,
            cursor: request.cursor,
            after: request.after_watermark,
        };
        let answer = self.read(asked.request()?, envelope)?;
        Ok(ListCorporateActionsReply {
            actions: answer
                .rows
                .into_iter()
                .filter_map(|r| match r {
                    Observation::CorporateAction(action) => Some(action),
                    _ => None,
                })
                .collect(),
            unanswered: answer.unanswered,
            datasets: answer.datasets,
            watermark: Some(answer.watermark),
            next_cursor: answer.next_cursor,
        })
    }

    pub(crate) fn latest_recorded(
        &self,
        request: &ListTradesRequest,
        envelope: &Envelope,
    ) -> Result<ListTradesReply, String> {
        let Reader::Plugin(instance) = reader_of(envelope) else {
            return Err(
                "latest_recorded: a data plugin's own read, and this is no plugin's".into(),
            );
        };
        if request.valid_from_ns != 0
            || request.valid_until_ns != 0
            || request
                .after_watermark
                .as_ref()
                .is_some_and(|w| !w.partitions.is_empty())
        {
            return Err(
                "latest_recorded: the range and after_watermark are left empty, as it reads the last \
                 trade recorded"
                    .into(),
            );
        }
        if !bounds::LIST_PRICES_REQUEST_SUBJECTS_COUNT.admits(request.subjects.len()) {
            return Err(format!(
                "subjects names {}; 1 to 500",
                request.subjects.len()
            ));
        }
        let config = self.config();
        let own: Vec<String> = config
            .datasets
            .iter()
            .filter(|(dataset, held)| {
                held.instance == instance && config.serves(dataset, DataType::Trade)
            })
            .map(|(dataset, _)| dataset.clone())
            .collect();
        if own.is_empty() {
            return Err(format!(
                "latest_recorded: {instance} declares no dataset of trades, and reads only its own rows"
            ));
        }
        let heads = self.store.heads().map_err(|failed| failed.to_string())?;
        let now = self.clock.now_ns();
        let subjects: Vec<String> = request
            .subjects
            .iter()
            .map(|s| s.entity_id.clone())
            .collect();
        let mut latest: BTreeMap<(String, String, String), meridian_domain::v1::Trade> =
            BTreeMap::new();
        for subject in &subjects {
            for dataset in &own {
                // The last day first, then wider, so a quiet subject's last
                // trade is found without reading all its history.
                for days in [1_i64, 7, 30, 365] {
                    let rows = self
                        .store
                        .read(&crate::store::Query {
                            datasets: vec![dataset.clone()],
                            subjects: vec![subject.clone()],
                            data_type: DataType::Trade,
                            kinds: Vec::new(),
                            interval_ns: 0,
                            when: When::Range {
                                from_ns: now - days * 86_400 * 1_000_000_000,
                                // A venue's clock ahead of the deployment's
                                // is still its last trade.
                                until_ns: i64::MAX,
                            },
                            as_of_ns: now,
                            published_as_of_ns: 0,
                            latest_only: false,
                        })
                        .map_err(|failed| failed.to_string())?;
                    let mut found = false;
                    for row in rows {
                        let Observation::Trade(trade) = row else {
                            continue;
                        };
                        let Some(meta) = trade.meta.as_ref() else {
                            continue;
                        };
                        let venue = meta
                            .source
                            .as_ref()
                            .map(|source| source.venue_id.clone())
                            .unwrap_or_default();
                        let key = (subject.clone(), dataset.clone(), venue);
                        let later = latest.get(&key).is_none_or(|held| {
                            let held_at = held.meta.as_ref().map_or(0, |m| m.valid_from_ns);
                            (meta.valid_from_ns, trade.source_sequence)
                                > (held_at, held.source_sequence)
                        });
                        if later {
                            latest.insert(key, trade.clone());
                        }
                        found = true;
                    }
                    if found {
                        break;
                    }
                }
            }
        }
        Ok(ListTradesReply {
            trades: latest.into_values().collect(),
            unanswered: Vec::new(),
            datasets: own
                .iter()
                .filter_map(|dataset| config.datasets.get(dataset).cloned())
                .collect(),
            watermark: Some(watermark(&heads, &own.iter().cloned().collect())),
            next_cursor: String::new(),
        })
    }

    /// W10.6, contract v23 (the lake plan's vL1c, item 4): outcomes for event
    /// contracts' instruments, from every dataset holding them the reader may
    /// read -- an outcome takes no default among sources -- the version in
    /// force of each row key as of a recorded time, or the versions recorded
    /// after a watermark, whatever their valid time, which is how a reader
    /// that lost some catches up. A source is never asked: what a venue has
    /// not determined is nothing to want.
    pub(crate) fn list_outcomes(
        &self,
        request: ListOutcomesRequest,
        envelope: &Envelope,
    ) -> Result<ListOutcomesReply, String> {
        self.list_outcomes_as(reader_of(envelope), request)
    }

    pub(crate) fn list_outcomes_as(
        &self,
        reader: Reader,
        request: ListOutcomesRequest,
    ) -> Result<ListOutcomesReply, String> {
        if !bounds::LIST_OUTCOMES_REQUEST_SUBJECTS_COUNT.admits(request.subjects.len()) {
            return Err(format!(
                "subjects names {}; 1 to 500",
                request.subjects.len()
            ));
        }
        let when = match request
            .after_watermark
            .as_ref()
            .filter(|watermark| !watermark.partitions.is_empty())
        {
            Some(watermark) => {
                let mut at: BTreeMap<String, u64> = BTreeMap::new();
                for (i, partition) in watermark.partitions.iter().enumerate() {
                    if at
                        .insert(partition.partition.clone(), partition.sequence)
                        .is_some()
                    {
                        return Err(format!(
                            "after_watermark.partitions[{i}] {:?} is named twice",
                            partition.partition
                        ));
                    }
                }
                When::After(at)
            }
            None => When::Latest { at_ns: 0 },
        };
        if request.as_of_ns < 0 {
            return Err("as_of_ns is never negative".into());
        }
        let as_of = if request.as_of_ns == 0 {
            self.clock.now_ns()
        } else {
            request.as_of_ns
        };
        let config = self.config();
        let aliases = self.store.aliases().map_err(|failed| failed.to_string())?;
        let read = Read {
            reader: reader.clone(),
            data_type: DataType::Outcome,
            subjects: request
                .subjects
                .iter()
                .map(|s| s.entity_id.clone())
                .collect(),
            kinds: Vec::new(),
            interval_ns: 0,
            sources: SourceChoice {
                side_by_side: true,
                ..Default::default()
            },
            when,
            as_of_ns: as_of,
            tenors: Vec::new(),
            published_as_of_ns: 0,
            series_keys: Vec::new(),
            all_series: false,
            dates: None,
            roles: Vec::new(),
            narrowed: read::Narrowed::default(),
        };
        let answered = read::answer(
            self.store.as_ref(),
            &read,
            &Against {
                config: &config,
                priorities: &[],
                declined: &read::Declined::new(),
                aliases: &aliases,
                now_ns: self.clock.now_ns(),
            },
        )?;
        let heads = self.store.heads().map_err(|failed| failed.to_string())?;
        let readable: BTreeSet<String> = config
            .readable(&reader, DataType::Outcome)
            .into_iter()
            .collect();
        Ok(ListOutcomesReply {
            outcomes: answered
                .rows
                .into_iter()
                .filter_map(|row| match row {
                    Observation::Outcome(outcome) => Some(outcome),
                    _ => None,
                })
                .collect(),
            watermark: Some(watermark(&heads, &readable)),
        })
    }

    pub(crate) fn list_datasets(&self, envelope: &Envelope) -> Result<ListDatasetsReply, String> {
        let reader = reader_of(envelope);
        let config = self.config();
        let counts = self.store.counts().map_err(|failed| failed.to_string())?;
        let misses = self
            .store
            .miss_counts()
            .map_err(|failed| failed.to_string())?;
        let flagged = self
            .store
            .closed_day_counts()
            .map_err(|failed| failed.to_string())?;
        let mut reply = ListDatasetsReply::default();
        for (dataset, held) in &config.datasets {
            // A data plugin learns its own datasets' licences (contract v20,
            // W10.9): the datasets its catalogue declares, beside those it
            // may read.
            let own = matches!(&reader, Reader::Plugin(instance) if &held.instance == instance);
            if config.fields_for(dataset, &reader).is_none() && !own {
                continue;
            }
            reply.datasets.push(DatasetRef {
                unconverted_count: counts.get(dataset).map_or(0, |c| c.1),
                miss_count: misses.get(&held.instance).copied().unwrap_or(0),
                // Contract v23 (W10.9): the rows flagged on a closed day, and
                // those outside the calendar's coverage, counted apart.
                closed_day_count: flagged.get(dataset).map_or(0, |c| c.0),
                outside_coverage_count: flagged.get(dataset).map_or(0, |c| c.1),
                // Contract v27 (W10.9; ruling 418): the latest liveness the
                // lake keeps, one per venue.
                liveness: self
                    .liveness
                    .lock()
                    .expect("liveness poisoned")
                    .range((dataset.clone(), String::new())..)
                    .take_while(|((held, _), _)| held == dataset)
                    .map(|(_, alive)| alive.clone())
                    .collect(),
                ..held.clone()
            });
            match &reader {
                Reader::Core => {
                    if let Some(licence) = config.licences.get(dataset) {
                        reply.licences.push(licence.clone());
                    }
                }
                Reader::Plugin(_) => reply.licences.push(config.licence(dataset)),
            }
        }
        reply.entitlements = config
            .entitlements
            .values()
            .filter(|e| match &reader {
                Reader::Core => true,
                Reader::Plugin(instance) => &e.instance == instance,
            })
            .cloned()
            .collect();
        Ok(reply)
    }

    /// W10.4, contract v27 (ruling 418): a data plugin's datasets' liveness,
    /// 1 to 500 items, each a dataset its catalogue declares, the venue where
    /// it has one, and `alive_at_ns`, the source's last heartbeat or poll;
    /// recorded whole or refused naming the item and field. The latest per
    /// dataset and venue is kept, an earlier one heard late changing
    /// nothing, and published on the dataset's subject (W10.5), conflated per
    /// dataset and venue; it is answered in the datasets' listing (W10.9).
    pub(crate) fn record_liveness(
        &self,
        request: meridian_domain::v1::RecordLivenessRequest,
        envelope: &Envelope,
    ) -> Result<meridian_domain::v1::RecordLivenessReply, String> {
        let instance = publisher(envelope);
        let bound = bounds::RECORD_LIVENESS_REQUEST_LIVENESS_COUNT;
        if !bound.admits(request.liveness.len()) {
            return Err(format!(
                "liveness holds {} items; a batch is {} to {}",
                request.liveness.len(),
                bound.least,
                bound.most
            ));
        }
        let config = self.config();
        let mut checked = Vec::with_capacity(request.liveness.len());
        for (i, mut alive) in request.liveness.into_iter().enumerate() {
            let at = format!("liveness[{i}]");
            if !config.declared_by(&alive.dataset, &instance) {
                return Err(format!(
                    "{at}.dataset {:?} is no dataset this instance's catalogue declares",
                    alive.dataset
                ));
            }
            if !alive.venue_id.is_empty() && !alive.venue_id.starts_with("VEN-") {
                return Err(format!(
                    "{at}.venue_id {:?} is not a venue master ID",
                    alive.venue_id
                ));
            }
            if alive.alive_at_ns <= 0 {
                return Err(format!(
                    "{at}.alive_at_ns is the source's last heartbeat or poll, a time"
                ));
            }
            // Whose it is, the lake's word, never the plugin's.
            alive.instance = instance.clone();
            checked.push(alive);
        }
        let mut kept = 0;
        for alive in checked {
            let key = (alive.dataset.clone(), alive.venue_id.clone());
            let newer = {
                let mut held = self.liveness.lock().expect("liveness poisoned");
                let newer = held
                    .get(&key)
                    .is_none_or(|before| before.alive_at_ns < alive.alive_at_ns);
                if newer {
                    held.insert(key, alive.clone());
                }
                newer
            };
            if !newer {
                continue;
            }
            kept += 1;
            let topic = format!("platform.lake.{}.event.liveness-recorded", alive.dataset);
            let event = meridian_domain::v1::LivenessRecordedEvent {
                liveness: Some(alive),
            };
            if let Err(failed) = self.bus.publish(
                &topic,
                "meridian.v1.LivenessRecordedEvent",
                event.encode_to_vec(),
                None,
                None,
            ) {
                tracing::warn!(topic, %failed, "a dataset's liveness was kept and not published");
            }
        }
        Ok(meridian_domain::v1::RecordLivenessReply { kept })
    }

    /// W10.12, contract v28 (rulings 423 and 619): a `signal` plugin's
    /// escalation of recorded rows' quality, each assessment its own record
    /// beside the row it names (decisions/031), never a rewrite. States only
    /// escalate: one lower than the row's effective state -- the highest of
    /// its own and every assessment of it, those earlier in the batch
    /// included -- is refused naming its `state`; one naming a row or
    /// version the lake does not hold, or one the plugin may not read, its
    /// `row_key`. Recorded whole or refused naming the item and field.
    pub(crate) fn record_quality_assessments(
        &self,
        request: RecordQualityAssessmentsRequest,
        envelope: &Envelope,
    ) -> Result<RecordQualityAssessmentsReply, String> {
        let refused = |field: String, words: String| {
            refusal_with_status(RefusalStatus::InvalidArgument, &[field], words)
        };
        let bound = bounds::RECORD_QUALITY_ASSESSMENTS_REQUEST_ASSESSMENTS_COUNT;
        if !bound.admits(request.assessments.len()) {
            return Err(refused(
                "assessments".into(),
                format!(
                    "assessments holds {} items; a batch is {} to {}",
                    request.assessments.len(),
                    bound.least,
                    bound.most
                ),
            ));
        }
        let reader = reader_of(envelope);
        let config = self.config();
        let instance = publisher(envelope);
        let now = self.clock.now_ns();
        let mut journal: Vec<Assessment> = Vec::new();
        // Each row's state as the batch has raised it so far.
        let mut raised: BTreeMap<(String, u64), i32> = BTreeMap::new();
        for (i, assessment) in request.assessments.iter().enumerate() {
            let at = format!("assessments[{i}]");
            if assessment.dataset.is_empty() {
                return Err(refused(
                    format!("{at}.dataset"),
                    format!("{at}.dataset names the dataset of the row assessed"),
                ));
            }
            if assessment.row_key.is_empty() {
                return Err(refused(
                    format!("{at}.row_key"),
                    format!("{at}.row_key names the row assessed"),
                ));
            }
            if assessment.version == 0 {
                return Err(refused(
                    format!("{at}.version"),
                    format!("{at}.version names the row key's version assessed, never 0"),
                ));
            }
            if assessment.state != QualityState::Suspicious as i32
                && assessment.state != QualityState::Invalid as i32
            {
                return Err(refused(
                    format!("{at}.state"),
                    format!("{at}.state is suspicious or invalid"),
                ));
            }
            if assessment.reasons.is_empty() {
                return Err(refused(
                    format!("{at}.reasons"),
                    format!("{at}.reasons says why, never empty"),
                ));
            }
            for (j, reason) in assessment.reasons.iter().enumerate() {
                if QualityReason::try_from(*reason).unwrap_or(QualityReason::Unspecified)
                    == QualityReason::Unspecified
                {
                    return Err(refused(
                        format!("{at}.reasons[{j}]"),
                        format!("{at}.reasons[{j}] is unspecified"),
                    ));
                }
            }
            let rows: Vec<Observation> = self
                .store
                .row_version(&assessment.dataset, &assessment.row_key, assessment.version)
                .map_err(|failed| failed.to_string())?
                .into_iter()
                .filter(|row| {
                    config
                        .readable(&reader, row.data_type())
                        .contains(&assessment.dataset)
                })
                .collect();
            if rows.is_empty() {
                return Err(refused(
                    format!("{at}.row_key"),
                    format!(
                        "{at}.row_key: {} holds no row {:?} at version {} that this plugin may read",
                        assessment.dataset, assessment.row_key, assessment.version
                    ),
                ));
            }
            for row in rows {
                let key = (assessment.dataset.clone(), row.meta().sequence);
                let effective =
                    rank(row.meta().quality).max(raised.get(&key).copied().unwrap_or(0));
                if rank(assessment.state) < effective {
                    return Err(refused(
                        format!("{at}.state"),
                        format!(
                            "{at}.state is lower than the row's effective state, {}; states only \
                             escalate",
                            QualityState::try_from(effective)
                                .map_or("unknown", |state| state.as_str_name())
                        ),
                    ));
                }
                raised.insert(key, effective.max(rank(assessment.state)));
                journal.push(Assessment {
                    dataset: assessment.dataset.clone(),
                    sequence: row.meta().sequence,
                    row_key: assessment.row_key.clone(),
                    version: assessment.version,
                    state: assessment.state,
                    reasons: assessment.reasons.clone(),
                    // Stamped by the sidecar; the publisher where it is not.
                    by_instance: if assessment.assessed_by_instance.is_empty() {
                        instance.clone()
                    } else {
                        assessment.assessed_by_instance.clone()
                    },
                    at_ns: now,
                });
            }
        }
        self.store
            .assess(&journal)
            .map_err(|failed| failed.to_string())?;
        Ok(RecordQualityAssessmentsReply {
            recorded: request.assessments.len() as u32,
        })
    }

    pub(crate) fn set_priority(
        &self,
        request: SetSourcePriorityRequest,
        envelope: &Envelope,
    ) -> Result<SourcePriority, String> {
        let meta = envelope.meta.clone().unwrap_or_default();
        if meta.acting_for_subject.is_empty() {
            return Err(
                "a priority is a deployment admin's to set, and this is sent for nobody".into(),
            );
        }
        let Some(data_type) = DataType::named(&request.data_type) else {
            return Err(format!(
                "data_type: {:?} is not a data type the lake has ({})",
                request.data_type,
                lake::DATA_TYPES.join(", ")
            ));
        };
        let kind = PriceKind::try_from(request.kind).unwrap_or(PriceKind::Unspecified);
        match data_type {
            DataType::Price if kind == PriceKind::Unspecified => {
                return Err("kind: a price's priority names its kind".into())
            }
            DataType::Bar
            | DataType::Trade
            | DataType::Quote
            | DataType::Outcome
            | DataType::Rate
                if kind != PriceKind::Unspecified =>
            {
                return Err(format!(
                    "kind: a priority for {} names no kind; only prices have one",
                    data_type.name()
                ))
            }
            _ => {}
        }
        if !bounds::SET_SOURCE_PRIORITY_REQUEST_DATASETS_COUNT.admits(request.datasets.len()) {
            return Err(format!(
                "datasets names {}; at most 16",
                request.datasets.len()
            ));
        }
        let config = self.config();
        let mut seen = BTreeSet::new();
        for (i, dataset) in request.datasets.iter().enumerate() {
            if !seen.insert(dataset) {
                return Err(format!("datasets[{i}] {dataset:?} is named twice"));
            }
            if !config.serves(dataset, data_type) {
                return Err(format!(
                    "datasets[{i}]: {dataset:?} is no dataset a launched catalogue declares for {}",
                    data_type.name()
                ));
            }
        }
        if request.note.chars().count() > 2_000 {
            return Err("note: a note is at most 2000 characters, and nothing was changed".into());
        }
        let delegation = meta.acting_through_delegation.clone();
        let priority = SourcePriority {
            data_type: request.data_type.clone(),
            kind: request.kind,
            datasets: request.datasets.clone(),
            updated_by: meta.acting_for_subject.clone(),
            updated_at_ns: self.clock.now_ns(),
            client_name: if delegation.is_empty() {
                String::new()
            } else {
                meta.acting_through_client.clone()
            },
            acting_through_delegation: delegation,
            note: request.note.clone(),
            // Contract v20: the prepared change the person made it from.
            prepared_call_id: if meta.acting_for_subject.is_empty() {
                String::new()
            } else {
                meta.prepared_call_id.clone()
            },
        };
        match self
            .store
            .set_priority(&priority, request.against_updated_at_ns)
            .map_err(|failed| failed.to_string())?
        {
            Ok(set) => {
                tracing::info!(data_type = set.data_type, kind = set.kind, by = set.updated_by, "a source priority set");
                Ok(set)
            }
            Err(stale) => Err(meridian_bus::refusal_naming(
                meridian_pb::v1::RefusalReason::RecordChanged as i32,
                &["against_updated_at_ns".to_string()],
                format!(
                    "the priority changed since it was read (now {}, read at {}); read it again, and nothing was changed",
                    stale.standing_at_ns, request.against_updated_at_ns
                ),
            )),
        }
    }

    pub(crate) fn decline(
        &self,
        request: DeclineWantRequest,
        envelope: &Envelope,
    ) -> Result<DeclineWantReply, String> {
        let instance = publisher(envelope);
        let reason =
            UnansweredReason::try_from(request.reason).unwrap_or(UnansweredReason::Unspecified);
        if !matches!(
            reason,
            UnansweredReason::NotCovered
                | UnansweredReason::SourceSilent
                | UnansweredReason::BeyondHistory
        ) {
            return Err("reason: not_covered, source_silent or beyond_history".into());
        }
        if !(1..=500).contains(&request.subjects.len()) {
            return Err(format!(
                "subjects names {}; 1 to 500",
                request.subjects.len()
            ));
        }
        let (dataset, asked) = self
            .wants
            .lock()
            .expect("wants poisoned")
            .open
            .get(&request.want_id)
            .map(|w| (w.event.dataset.clone(), want_asked(&w.event)))
            .ok_or_else(|| format!("want_id: no want {} is open", request.want_id))?;
        if lake::instance_of(&dataset) != Some(instance.as_str()) {
            return Err(format!(
                "want_id: the want is of {dataset}, which this instance does not serve"
            ));
        }
        let now = self.clock.now_ns();
        let subjects: Vec<String> = request
            .subjects
            .iter()
            .map(|s| s.entity_id.clone())
            .collect();
        {
            // For the date or range the want asked, or the latest, alone
            // (contract v19, approved 2026-10-10).
            let mut declined = self.declined.lock().expect("declined poisoned");
            for subject in &subjects {
                declined.insert(
                    (dataset.clone(), subject.clone(), asked.clone()),
                    (reason, now),
                );
            }
        }
        self.settle(
            &request.want_id,
            &subjects,
            WantChangeKind::Declined,
            reason as i32,
            &instance,
            now,
        );
        Ok(DeclineWantReply {})
    }

    /// The data configuration heard, applied and kept.
    pub fn configured(&self, event: &EntitlementsChangedEvent) {
        *self.config.write().expect("config poisoned") = DataConfig::from_event(event);
        if let Err(failed) = self.store.keep_configuration(event) {
            tracing::warn!(%failed, "the data configuration was not kept");
        }
    }
}

/// What a want asked, as a decline of it is kept by: its business date, its
/// range, or the latest (contract v19).
fn want_asked(event: &ObservationsWantedEvent) -> String {
    let when = if let Ok(date) = Date::parse(&event.business_date) {
        When::BusinessDate(date)
    } else if event.valid_from_ns != 0 || event.valid_until_ns != 0 {
        When::Range {
            from_ns: event.valid_from_ns,
            until_ns: event.valid_until_ns,
        }
    } else {
        When::Latest { at_ns: 0 }
    };
    read::asked(&when)
}

/// A trade held to W10.4 (contract v19): a price naming its asset with an
/// amount, a quantity more than zero, characteristics each in the list and at
/// most sixteen, and an aggressor and eligibilities the enums hold.
fn trade_refused(at: &str, trade: &meridian_domain::v1::Trade) -> Result<(), String> {
    match trade.price.as_ref() {
        None => {
            return Err(format!(
                "{at}.price is required: a trade with no price is refused"
            ))
        }
        Some(price) if price.amount.is_none() => {
            return Err(format!("{at}.price.amount is required"))
        }
        Some(_) => {}
    }
    match trade.quantity.as_ref() {
        None => return Err(format!("{at}.quantity is required, more than zero")),
        Some(quantity) => {
            let exact = meridian_domain::exact::Exact::from_wire(quantity)
                .map_err(|out| format!("{at}.quantity {out}"))?;
            if exact <= meridian_domain::exact::Exact::ZERO {
                return Err(format!("{at}.quantity is more than zero"));
            }
        }
    }
    if let Some(attributes) = trade.attributes.as_ref() {
        if !bounds::TRADE_ATTRIBUTES_CHARACTERISTICS_COUNT.admits(attributes.characteristics.len())
        {
            return Err(format!(
                "{at}.attributes.characteristics names {}; at most 16",
                attributes.characteristics.len()
            ));
        }
        for (j, value) in attributes.characteristics.iter().enumerate() {
            let known =
                TradeCharacteristic::try_from(*value).unwrap_or(TradeCharacteristic::Unspecified);
            if known == TradeCharacteristic::Unspecified {
                return Err(format!(
                    "{at}.attributes.characteristics[{j}] is unspecified: a code that did not \
                     convert travels as reported in unconverted"
                ));
            }
        }
    }
    Ok(())
}

/// A quote held to W10.4 (contract v19): each side's amount stated, each
/// quantity more than zero, characteristics each in the list and at most
/// four. Its two sides in one asset is checked beside its amounts.
/// An outcome held to W10.4 (contract v23): its determination and stage
/// said, its settlement value set and never negative, its venue's word kept.
/// A statistic held to W10.4 (contract v28, G11): its kind and its value, a
/// count never negative; a source that does not state one records no row,
/// never a zero.
fn statistic_refused(at: &str, statistic: &meridian_domain::v1::Statistic) -> Result<(), String> {
    if StatisticKind::try_from(statistic.kind).unwrap_or(StatisticKind::Unspecified)
        == StatisticKind::Unspecified
    {
        return Err(format!("{at}.kind is unspecified"));
    }
    let Some(value) = statistic.value.as_ref() else {
        return Err(format!(
            "{at}.value is required: contracts, or shares for an equity"
        ));
    };
    let Some(amount) = value.value.as_ref() else {
        return Err(format!("{at}.value.value is required"));
    };
    let exact = meridian_domain::exact::Exact::from_wire(amount)
        .map_err(|out| format!("{at}.value.value {out}"))?;
    if exact < meridian_domain::exact::Exact::ZERO {
        return Err(format!("{at}.value.value is never negative"));
    }
    Ok(())
}

/// A derived value held to W10.4 (contract v28, track B): its series named,
/// its value stated, its rule a definition and version, and the datasets it
/// was derived from.
/// A declared corporate action held to W10.4 (contract v28, the filings
/// slice): its kind stated; a cash dividend's amount per share and each of
/// its four dates, a plain date, as the rule found them -- the plugin never
/// infers one (FDP Q12); the rule named; the filing's accession and form
/// where it names one.
fn corporate_action_refused(
    at: &str,
    action: &meridian_domain::v1::CorporateActionAnnounced,
) -> Result<(), String> {
    let kind =
        CorporateActionKind::try_from(action.kind).unwrap_or(CorporateActionKind::Unspecified);
    if kind == CorporateActionKind::Unspecified {
        return Err(format!("{at}.kind is unspecified"));
    }
    if kind == CorporateActionKind::CashDividend {
        if action
            .amount_per_share
            .as_ref()
            .and_then(|m| m.amount.as_ref())
            .is_none()
        {
            return Err(format!(
                "{at}.amount_per_share is required on a cash dividend, as declared"
            ));
        }
        for (name, date) in [
            ("declared_date", &action.declared_date),
            ("ex_date", &action.ex_date),
            ("record_date", &action.record_date),
            ("payment_date", &action.payment_date),
        ] {
            if date.is_empty() {
                return Err(format!(
                    "{at}.{name} is required: a row is recorded only where the rule finds it"
                ));
            }
            meridian_domain::date::optional(&format!("{at}.{name}"), date)?;
        }
    }
    let rule = action.rule.clone().unwrap_or_default();
    if rule.definition.trim().is_empty() || rule.version < 1 {
        return Err(format!(
            "{at}.rule is required: the definition that found it and its version, from 1"
        ));
    }
    if let Some(filing) = &action.filing {
        if !bounds::FILING_REF_ACCESSION_NUMBER_LENGTH
            .admits(filing.accession_number.chars().count())
            || filing.accession_number.trim().is_empty()
        {
            return Err(format!(
                "{at}.filing.accession_number is 1 to 32 characters"
            ));
        }
        if !bounds::FILING_REF_FORM_LENGTH.admits(filing.form.chars().count())
            || filing.form.trim().is_empty()
        {
            return Err(format!("{at}.filing.form is 1 to 32 characters"));
        }
    }
    Ok(())
}

fn derived_refused(at: &str, value: &meridian_domain::v1::DerivedSeries) -> Result<(), String> {
    if !bounds::DERIVED_SERIES_SERIES_LENGTH.admits(value.series.chars().count()) {
        return Err(format!("{at}.series is 1 to 64 characters"));
    }
    if value
        .value
        .as_ref()
        .and_then(|v| v.value.as_ref())
        .is_none()
    {
        return Err(format!("{at}.value is required, with its unit"));
    }
    let rule = value.rule.clone().unwrap_or_default();
    if rule.definition.trim().is_empty() || rule.version < 1 {
        return Err(format!(
            "{at}.rule is required: the definition it was derived by and its version, from 1"
        ));
    }
    if !bounds::DERIVED_SERIES_INPUTS_COUNT.admits(value.inputs.len()) {
        return Err(format!(
            "{at}.inputs names {}; 1 to 32, each dataset read",
            value.inputs.len()
        ));
    }
    for (i, input) in value.inputs.iter().enumerate() {
        if input.dataset.trim().is_empty() {
            return Err(format!("{at}.inputs[{i}].dataset is required"));
        }
    }
    Ok(())
}

/// A listed series held to W10.4 (contract v28, ruling 561): its template
/// and version named, each field by its path, and no instrument ID, which
/// is the lake's answer on a read.
fn series_refused(at: &str, series: &meridian_domain::v1::ListedSeries) -> Result<(), String> {
    if !series.template_id.starts_with("TPL-") {
        return Err(format!(
            "{at}.template_id {:?} is not a template's TPL- ID",
            series.template_id
        ));
    }
    if series.template_version.trim().is_empty() {
        return Err(format!(
            "{at}.template_version is required: a published version"
        ));
    }
    if !bounds::LISTED_SERIES_FIELDS_COUNT.admits(series.fields.len()) {
        return Err(format!(
            "{at}.fields names {}; 1 to 64",
            series.fields.len()
        ));
    }
    let mut paths = BTreeSet::new();
    for (k, field) in series.fields.iter().enumerate() {
        if field.path.trim().is_empty() {
            return Err(format!("{at}.fields[{k}].path is required"));
        }
        if !paths.insert(field.path.as_str()) {
            return Err(format!(
                "{at}.fields[{k}].path {:?} is named twice",
                field.path
            ));
        }
    }
    if !series.instrument_id.is_empty() {
        return Err(format!(
            "{at}.instrument_id is the lake's answer on a read, never set by a writer"
        ));
    }
    Ok(())
}

/// A rate held to W10.4 (contract v27, the rates slice): its kind, its value
/// with its unit and its convention's compounding stated; a compounded rate
/// names its periods a year; a tenor names its unit and a count; a maturity
/// is a date; a percentile is 1 to 99. Whether a curve point names its tenor
/// or maturity is checked against its subject's type.
fn rate_refused(at: &str, rate: &meridian_domain::v1::Rate) -> Result<(), String> {
    if RateKind::try_from(rate.kind).unwrap_or(RateKind::Unspecified) == RateKind::Unspecified {
        return Err(format!("{at}.kind is unspecified"));
    }
    let Some(value) = rate.value.as_ref() else {
        return Err(format!("{at}.value is required: the rate with its unit"));
    };
    let Some(number) = value.value.as_ref() else {
        return Err(format!("{at}.value.value is required"));
    };
    meridian_domain::exact::Exact::from_wire(number)
        .map_err(|out| format!("{at}.value.value {out}"))?;
    let unit = ValueUnit::try_from(value.unit).unwrap_or(ValueUnit::Unspecified);
    if unit == ValueUnit::Unspecified {
        return Err(format!("{at}.value.unit is unspecified"));
    }
    if unit == ValueUnit::Ucum && value.ucum_code.trim().is_empty() {
        return Err(format!("{at}.value.ucum_code names the physical unit"));
    }
    let Some(convention) = rate.convention.as_ref() else {
        return Err(format!("{at}.convention is required: how the rate accrues"));
    };
    let compounding =
        Compounding::try_from(convention.compounding).unwrap_or(Compounding::Unspecified);
    if compounding == Compounding::Unspecified {
        return Err(format!("{at}.convention.compounding is unspecified"));
    }
    if compounding == Compounding::Compounded && convention.periods_per_year == 0 {
        return Err(format!(
            "{at}.convention.periods_per_year: a compounded rate names its periods a year"
        ));
    }
    if let Some(tenor) = rate.tenor.as_ref() {
        if TenorUnit::try_from(tenor.unit).unwrap_or(TenorUnit::Unspecified)
            == TenorUnit::Unspecified
        {
            return Err(format!("{at}.tenor.unit is unspecified"));
        }
        if tenor.count < 1 {
            return Err(format!("{at}.tenor.count is at least 1"));
        }
    }
    meridian_domain::date::optional(&format!("{at}.maturity_date"), &rate.maturity_date)?;
    for (i, percentile) in rate.percentiles.iter().enumerate() {
        if !(1..=99).contains(&percentile.percentile) {
            return Err(format!("{at}.percentiles[{i}].percentile is 1 to 99"));
        }
        if percentile.value.is_none() {
            return Err(format!("{at}.percentiles[{i}].value is required"));
        }
    }
    Ok(())
}

fn outcome_refused(at: &str, outcome: &meridian_domain::v1::Outcome) -> Result<(), String> {
    use meridian_domain::v1::{OutcomeDetermination, OutcomeStage};
    if OutcomeDetermination::try_from(outcome.determination)
        .unwrap_or(OutcomeDetermination::Unspecified)
        == OutcomeDetermination::Unspecified
    {
        return Err(format!("{at}.determination is unspecified"));
    }
    if OutcomeStage::try_from(outcome.stage).unwrap_or(OutcomeStage::Unspecified)
        == OutcomeStage::Unspecified
    {
        return Err(format!("{at}.stage is unspecified"));
    }
    let Some(value) = outcome.settlement_value.as_ref() else {
        return Err(format!(
            "{at}.settlement_value is required: what one contract settles at, in the payout asset"
        ));
    };
    if let Some(amount) = value.amount.as_ref() {
        let exact = meridian_domain::exact::Exact::from_wire(amount)
            .map_err(|out| format!("{at}.settlement_value.amount {out}"))?;
        if exact.is_negative() {
            return Err(format!("{at}.settlement_value is never negative"));
        }
    }
    if outcome.settled_at_ns < 0 {
        return Err(format!("{at}.settled_at_ns is never negative"));
    }
    if outcome.determination_as_reported.is_none() {
        return Err(format!(
            "{at}.determination_as_reported is required: the venue's own word, as reported"
        ));
    }
    Ok(())
}

fn quote_refused(at: &str, quote: &meridian_domain::v1::Quote) -> Result<(), String> {
    for (name, side) in [("bid", &quote.bid), ("ask", &quote.ask)] {
        if side.as_ref().is_some_and(|money| money.amount.is_none()) {
            return Err(format!("{at}.{name}.amount is required"));
        }
    }
    for (name, quantity) in [
        ("bid_quantity", &quote.bid_quantity),
        ("ask_quantity", &quote.ask_quantity),
    ] {
        if let Some(quantity) = quantity {
            let exact = meridian_domain::exact::Exact::from_wire(quantity)
                .map_err(|out| format!("{at}.{name} {out}"))?;
            if exact <= meridian_domain::exact::Exact::ZERO {
                return Err(format!("{at}.{name} is more than zero"));
            }
        }
    }
    if !bounds::QUOTE_CHARACTERISTICS_COUNT.admits(quote.characteristics.len()) {
        return Err(format!(
            "{at}.characteristics names {}; at most 4",
            quote.characteristics.len()
        ));
    }
    for (j, value) in quote.characteristics.iter().enumerate() {
        let known =
            QuoteCharacteristic::try_from(*value).unwrap_or(QuoteCharacteristic::Unspecified);
        if known == QuoteCharacteristic::Unspecified {
            return Err(format!("{at}.characteristics[{j}] is unspecified"));
        }
    }
    Ok(())
}

/// A read's request: prices', bars', and from contract v19 trades' or
/// quotes'.
pub struct ReadRequest {
    pub data_type: DataType,
    pub subjects: Vec<SubjectRef>,
    pub kinds: Vec<i32>,
    pub interval_ns: i64,
    pub sources: Option<SourceChoice>,
    pub at_ns: i64,
    pub business_date: String,
    pub valid_from_ns: i64,
    pub valid_until_ns: i64,
    pub as_of_ns: i64,
    pub page_size: u32,
    pub cursor: String,
    /// A trades read's catch-up (contract v19): the watermark the reader
    /// last saw.
    pub after: Option<Watermark>,
    /// A rates read's tenors (contract v27); empty for every tenor.
    pub tenors: Vec<meridian_domain::v1::Tenor>,
    /// A rates read's published-time cut-off (contract v27); 0 for none.
    pub published_as_of_ns: i64,
    /// Contract v28 (ruling 561, W10.6): the series asked for by key, and
    /// whether every series of the subjects is; neither, and rows keyed by
    /// a series are left out.
    pub series_keys: Vec<Identifier>,
    pub all_series: bool,
    /// Contract v28 (ListStatistics): a range of business dates, from and
    /// to inclusive, over every version in force; `None` for none.
    pub dates: Option<(Date, Date)>,
}

#[derive(Debug)]
pub struct ReadAnswer {
    pub rows: Vec<Observation>,
    pub unanswered: Vec<Unanswered>,
    pub datasets: Vec<DatasetRef>,
    pub watermark: Watermark,
    pub next_cursor: String,
}

fn cursor(as_of: i64, offset: usize) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("lake:{as_of}:{offset}"))
}

fn cursor_of(text: &str) -> Result<Option<(i64, usize)>, String> {
    use base64::Engine as _;
    if text.is_empty() {
        return Ok(None);
    }
    let bad = || "cursor: not a cursor this lake gave".to_string();
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text)
        .map_err(|_| bad())?;
    let said = String::from_utf8(bytes).map_err(|_| bad())?;
    let mut parts = said.split(':');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("lake"), Some(as_of), Some(offset), None) => Ok(Some((
            as_of.parse().map_err(|_| bad())?,
            offset.parse().map_err(|_| bad())?,
        ))),
        _ => Err(bad()),
    }
}

fn serve_on<Req, Rep, F>(
    bus: &Arc<Bus>,
    lake: &Arc<Lake>,
    topic: &'static str,
    types: (&'static str, &'static str),
    handle: F,
) where
    Req: Message + Default,
    Rep: Message,
    F: Fn(&Lake, Req, &Envelope) -> Result<Rep, String> + Send + Sync + 'static,
{
    let (request_type, reply_type) = types;
    let lake = Arc::clone(lake);
    bus.serve(topic, move |envelope| {
        let request: Req = decode(&envelope, request_type)?;
        let reply = handle(&lake, request, &envelope)?;
        Ok((reply_type.to_string(), reply.encode_to_vec()))
    });
}

/// Register every handler, and subscribe to every event the lake hears,
/// before returning; the loops consuming them are spawned.
pub fn serve(bus: Arc<Bus>, store: Arc<dyn Store>, clock: Arc<dyn Clock>) -> Arc<Lake> {
    // Lake::new reads the configuration last heard from the store, which
    // blocks: off the runtime's own threads.
    let lake = tokio::task::block_in_place(|| Lake::new(Arc::clone(&bus), store, clock));
    serve_on(
        &bus,
        &lake,
        RECORD_PRICES,
        (
            "meridian.v1.RecordPricesRequest",
            "meridian.v1.RecordObservationsReply",
        ),
        |lake, request: RecordPricesRequest, envelope| {
            lake.record(
                "prices",
                request.prices.into_iter().map(Observation::Price).collect(),
                &request.want_id,
                envelope,
            )
        },
    );
    serve_on(
        &bus,
        &lake,
        RECORD_BARS,
        (
            "meridian.v1.RecordBarsRequest",
            "meridian.v1.RecordObservationsReply",
        ),
        |lake, request: RecordBarsRequest, envelope| {
            lake.record(
                "bars",
                request.bars.into_iter().map(Observation::Bar).collect(),
                &request.want_id,
                envelope,
            )
        },
    );
    serve_on(
        &bus,
        &lake,
        RECORD_TRADES,
        (
            "meridian.v1.RecordTradesRequest",
            "meridian.v1.RecordObservationsReply",
        ),
        |lake, request: RecordTradesRequest, envelope| {
            lake.record(
                "trades",
                request.trades.into_iter().map(Observation::Trade).collect(),
                &request.want_id,
                envelope,
            )
        },
    );
    serve_on(
        &bus,
        &lake,
        RECORD_QUOTES,
        (
            "meridian.v1.RecordQuotesRequest",
            "meridian.v1.RecordObservationsReply",
        ),
        |lake, request: RecordQuotesRequest, envelope| {
            lake.record(
                "quotes",
                request.quotes.into_iter().map(Observation::Quote).collect(),
                &request.want_id,
                envelope,
            )
        },
    );
    serve_on(
        &bus,
        &lake,
        RECORD_OUTCOMES,
        (
            "meridian.v1.RecordOutcomesRequest",
            "meridian.v1.RecordObservationsReply",
        ),
        |lake, request: RecordOutcomesRequest, envelope| {
            lake.record(
                "outcomes",
                request
                    .outcomes
                    .into_iter()
                    .map(Observation::Outcome)
                    .collect(),
                "",
                envelope,
            )
        },
    );
    // Contract v27, the rates slice (W10.4).
    serve_on(
        &bus,
        &lake,
        RECORD_RATES,
        (
            "meridian.v1.RecordRatesRequest",
            "meridian.v1.RecordObservationsReply",
        ),
        |lake, request: RecordRatesRequest, envelope| {
            lake.record(
                "rates",
                request.rates.into_iter().map(Observation::Rate).collect(),
                &request.want_id,
                envelope,
            )
        },
    );
    // Contract v28, listed options and futures (W10.4, W10.6; ruling 561).
    serve_on(
        &bus,
        &lake,
        RECORD_SERIES,
        (
            "meridian.v1.RecordSeriesRequest",
            "meridian.v1.RecordObservationsReply",
        ),
        |lake, request: RecordSeriesRequest, envelope| {
            lake.record(
                "series",
                request
                    .series
                    .into_iter()
                    .map(Observation::Series)
                    .collect(),
                &request.want_id,
                envelope,
            )
        },
    );
    serve_on(
        &bus,
        &lake,
        LIST_SERIES,
        (
            "meridian.v1.ListSeriesRequest",
            "meridian.v1.ListSeriesReply",
        ),
        |lake, request: ListSeriesRequest, envelope| lake.list_series(request, envelope),
    );
    serve_on(
        &bus,
        &lake,
        RECORD_STATISTICS,
        (
            "meridian.v1.RecordStatisticsRequest",
            "meridian.v1.RecordObservationsReply",
        ),
        |lake, request: RecordStatisticsRequest, envelope| {
            lake.record(
                "statistics",
                request
                    .statistics
                    .into_iter()
                    .map(Observation::Statistic)
                    .collect(),
                &request.want_id,
                envelope,
            )
        },
    );
    serve_on(
        &bus,
        &lake,
        LIST_STATISTICS,
        (
            "meridian.v1.ListStatisticsRequest",
            "meridian.v1.ListStatisticsReply",
        ),
        |lake, request: ListStatisticsRequest, envelope| lake.list_statistics(request, envelope),
    );
    // Contract v28, the filings slice (W10.4 to W10.6): declared corporate
    // actions, published unconflated and read by subject, kind and date.
    serve_on(
        &bus,
        &lake,
        RECORD_CORPORATE_ACTIONS,
        (
            "meridian.v1.RecordCorporateActionsRequest",
            "meridian.v1.RecordObservationsReply",
        ),
        |lake, request: RecordCorporateActionsRequest, envelope| {
            lake.record(
                "actions",
                request
                    .actions
                    .into_iter()
                    .map(Observation::CorporateAction)
                    .collect(),
                &request.want_id,
                envelope,
            )
        },
    );
    serve_on(
        &bus,
        &lake,
        LIST_CORPORATE_ACTIONS,
        (
            "meridian.v1.ListCorporateActionsRequest",
            "meridian.v1.ListCorporateActionsReply",
        ),
        |lake, request: ListCorporateActionsRequest, envelope| {
            lake.list_corporate_actions(request, envelope)
        },
    );
    filings::serve(&bus, &lake);
    // Contract v28, track B (W10.4, W10.6, W12.8): a signal plugin's
    // derived series and saved scenarios.
    serve_on(
        &bus,
        &lake,
        RECORD_DERIVED_SERIES,
        (
            "meridian.v1.RecordDerivedSeriesRequest",
            "meridian.v1.RecordObservationsReply",
        ),
        |lake, request: meridian_domain::v1::RecordDerivedSeriesRequest, envelope| {
            lake.record(
                "values",
                request
                    .values
                    .into_iter()
                    .map(Observation::Derived)
                    .collect(),
                "",
                envelope,
            )
        },
    );
    serve_on(
        &bus,
        &lake,
        LIST_DERIVED_SERIES,
        (
            "meridian.v1.ListDerivedSeriesRequest",
            "meridian.v1.ListDerivedSeriesReply",
        ),
        |lake, request: meridian_domain::v1::ListDerivedSeriesRequest, envelope| {
            lake.list_derived_series(request, envelope)
        },
    );
    serve_on(
        &bus,
        &lake,
        RECORD_SCENARIOS,
        (
            "meridian.v1.RecordScenariosRequest",
            "meridian.v1.RecordObservationsReply",
        ),
        |lake, request: meridian_domain::v1::RecordScenariosRequest, envelope| {
            lake.record(
                "scenarios",
                request
                    .scenarios
                    .into_iter()
                    .map(Observation::Scenario)
                    .collect(),
                "",
                envelope,
            )
        },
    );
    serve_on(
        &bus,
        &lake,
        LIST_SCENARIOS,
        (
            "meridian.v1.ListScenariosRequest",
            "meridian.v1.ListScenariosReply",
        ),
        |lake, request: meridian_domain::v1::ListScenariosRequest, envelope| {
            lake.list_scenarios(request, envelope)
        },
    );
    // Contract v27 (W10.4; ruling 418): a dataset's liveness.
    serve_on(
        &bus,
        &lake,
        RECORD_LIVENESS,
        (
            "meridian.v1.RecordLivenessRequest",
            "meridian.v1.RecordLivenessReply",
        ),
        |lake, request: meridian_domain::v1::RecordLivenessRequest, envelope| {
            lake.record_liveness(request, envelope)
        },
    );
    // Contract v27, the rates slice (W10.6): read as prices are, narrowed
    // by kind and tenor, and optionally as the source had published them.
    serve_on(
        &bus,
        &lake,
        RECORD_QUALITY_ASSESSMENTS,
        (
            "meridian.v1.RecordQualityAssessmentsRequest",
            "meridian.v1.RecordQualityAssessmentsReply",
        ),
        |lake, request: RecordQualityAssessmentsRequest, envelope| {
            lake.record_quality_assessments(request, envelope)
        },
    );
    serve_on(
        &bus,
        &lake,
        LIST_RATES,
        ("meridian.v1.ListRatesRequest", "meridian.v1.ListRatesReply"),
        |lake, request: ListRatesRequest, envelope| lake.list_rates(request, envelope),
    );
    serve_on(
        &bus,
        &lake,
        RECORD_EXECUTION_COSTS,
        (
            "meridian.v1.RecordExecutionCostsRequest",
            "meridian.v1.RecordObservationsReply",
        ),
        |lake, request: RecordExecutionCostsRequest, envelope| {
            lake.record_execution_costs(request, envelope)
        },
    );
    serve_on(
        &bus,
        &lake,
        LIST_EXECUTION_COSTS,
        (
            "meridian.v1.ListExecutionCostsRequest",
            "meridian.v1.ListExecutionCostsReply",
        ),
        |lake, request: ListExecutionCostsRequest, envelope| {
            lake.list_execution_costs(request, envelope)
        },
    );
    serve_on(
        &bus,
        &lake,
        LIST_OUTCOMES,
        (
            "meridian.v1.ListOutcomesRequest",
            "meridian.v1.ListOutcomesReply",
        ),
        |lake, request: ListOutcomesRequest, envelope| lake.list_outcomes(request, envelope),
    );
    serve_on(
        &bus,
        &lake,
        LIST_TRADES,
        (
            "meridian.v1.ListTradesRequest",
            "meridian.v1.ListTradesReply",
        ),
        |lake, request: ListTradesRequest, envelope| {
            // Contract v20 (W10.6): a data plugin's own last trades, for the
            // restart's backfill.
            if request.latest_recorded {
                return lake.latest_recorded(&request, envelope);
            }
            let answer = lake.read(
                ReadRequest {
                    data_type: DataType::Trade,
                    subjects: request.subjects,
                    kinds: Vec::new(),
                    interval_ns: 0,
                    sources: request.sources,
                    at_ns: 0,
                    business_date: String::new(),
                    valid_from_ns: request.valid_from_ns,
                    valid_until_ns: request.valid_until_ns,
                    as_of_ns: request.as_of_ns,
                    page_size: request.page_size,
                    cursor: request.cursor,
                    after: request.after_watermark,
                    tenors: Vec::new(),
                    published_as_of_ns: 0,
                    // Contract v28 (W10.6): a chain's trades, asked for.
                    series_keys: request.series_keys,
                    all_series: request.all_series,
                    dates: None,
                },
                envelope,
            )?;
            Ok(ListTradesReply {
                trades: answer
                    .rows
                    .into_iter()
                    .filter_map(|r| match r {
                        Observation::Trade(t) => Some(t),
                        _ => None,
                    })
                    .collect(),
                unanswered: answer.unanswered,
                datasets: answer.datasets,
                watermark: Some(answer.watermark),
                next_cursor: answer.next_cursor,
            })
        },
    );
    serve_on(
        &bus,
        &lake,
        LIST_QUOTES,
        (
            "meridian.v1.ListQuotesRequest",
            "meridian.v1.ListQuotesReply",
        ),
        |lake, request: ListQuotesRequest, envelope| {
            let answer = lake.read(
                ReadRequest {
                    data_type: DataType::Quote,
                    subjects: request.subjects,
                    kinds: Vec::new(),
                    interval_ns: 0,
                    sources: request.sources,
                    at_ns: request.at_ns,
                    business_date: String::new(),
                    valid_from_ns: request.valid_from_ns,
                    valid_until_ns: request.valid_until_ns,
                    as_of_ns: request.as_of_ns,
                    page_size: request.page_size,
                    cursor: request.cursor,
                    after: None,
                    tenors: Vec::new(),
                    published_as_of_ns: 0,
                    // Contract v28 (W10.6): a chain's quotes, asked for.
                    series_keys: request.series_keys,
                    all_series: request.all_series,
                    dates: None,
                },
                envelope,
            )?;
            Ok(ListQuotesReply {
                quotes: answer
                    .rows
                    .into_iter()
                    .filter_map(|r| match r {
                        Observation::Quote(q) => Some(q),
                        _ => None,
                    })
                    .collect(),
                unanswered: answer.unanswered,
                datasets: answer.datasets,
                watermark: Some(answer.watermark),
                next_cursor: answer.next_cursor,
            })
        },
    );
    serve_on(
        &bus,
        &lake,
        QUERY_LAKE,
        ("meridian.v1.QueryLakeRequest", "meridian.v1.QueryLakeReply"),
        |lake, request: meridian_domain::v1::QueryLakeRequest, envelope| {
            lake.query_lake(request, envelope)
        },
    );
    serve_on(
        &bus,
        &lake,
        LIST_PRICES,
        (
            "meridian.v1.ListPricesRequest",
            "meridian.v1.ListPricesReply",
        ),
        |lake, request: ListPricesRequest, envelope| {
            let answer = lake.read(
                ReadRequest {
                    data_type: DataType::Price,
                    subjects: request.subjects,
                    kinds: request.kinds,
                    interval_ns: 0,
                    sources: request.sources,
                    at_ns: request.at_ns,
                    business_date: request.business_date,
                    valid_from_ns: request.valid_from_ns,
                    valid_until_ns: request.valid_until_ns,
                    as_of_ns: request.as_of_ns,
                    page_size: request.page_size,
                    cursor: request.cursor,
                    after: None,
                    tenors: Vec::new(),
                    published_as_of_ns: 0,
                    series_keys: Vec::new(),
                    all_series: false,
                    dates: None,
                },
                envelope,
            )?;
            Ok(ListPricesReply {
                prices: answer
                    .rows
                    .into_iter()
                    .filter_map(|r| match r {
                        Observation::Price(p) => Some(p),
                        _ => None,
                    })
                    .collect(),
                unanswered: answer.unanswered,
                datasets: answer.datasets,
                watermark: Some(answer.watermark),
                next_cursor: answer.next_cursor,
            })
        },
    );
    serve_on(
        &bus,
        &lake,
        LIST_BARS,
        ("meridian.v1.ListBarsRequest", "meridian.v1.ListBarsReply"),
        |lake, request: ListBarsRequest, envelope| {
            let answer = lake.read(
                ReadRequest {
                    data_type: DataType::Bar,
                    subjects: request.subjects,
                    kinds: Vec::new(),
                    interval_ns: request.interval_ns,
                    sources: request.sources,
                    at_ns: request.at_ns,
                    business_date: request.business_date,
                    valid_from_ns: request.valid_from_ns,
                    valid_until_ns: request.valid_until_ns,
                    as_of_ns: request.as_of_ns,
                    page_size: request.page_size,
                    cursor: request.cursor,
                    after: None,
                    tenors: Vec::new(),
                    published_as_of_ns: 0,
                    series_keys: Vec::new(),
                    all_series: false,
                    dates: None,
                },
                envelope,
            )?;
            Ok(ListBarsReply {
                bars: answer
                    .rows
                    .into_iter()
                    .filter_map(|r| match r {
                        Observation::Bar(b) => Some(b),
                        _ => None,
                    })
                    .collect(),
                unanswered: answer.unanswered,
                datasets: answer.datasets,
                watermark: Some(answer.watermark),
                next_cursor: answer.next_cursor,
            })
        },
    );
    serve_on(
        &bus,
        &lake,
        LIST_DATASETS,
        (
            "meridian.v1.ListDatasetsRequest",
            "meridian.v1.ListDatasetsReply",
        ),
        |lake, _: ListDatasetsRequest, envelope| lake.list_datasets(envelope),
    );
    serve_on(
        &bus,
        &lake,
        SET_SOURCE_PRIORITY,
        (
            "meridian.v1.SetSourcePriorityRequest",
            "meridian.v1.SourcePriority",
        ),
        |lake, request: SetSourcePriorityRequest, envelope| lake.set_priority(request, envelope),
    );
    serve_on(
        &bus,
        &lake,
        LIST_SOURCE_PRIORITIES,
        (
            "meridian.v1.ListSourcePrioritiesRequest",
            "meridian.v1.ListSourcePrioritiesReply",
        ),
        |lake, _: ListSourcePrioritiesRequest, _| {
            Ok(ListSourcePrioritiesReply {
                priorities: lake
                    .store
                    .priorities()
                    .map_err(|failed| failed.to_string())?,
            })
        },
    );
    serve_on(
        &bus,
        &lake,
        DECLINE_WANT,
        (
            "meridian.v1.DeclineWantRequest",
            "meridian.v1.DeclineWantReply",
        ),
        |lake, request: DeclineWantRequest, envelope| lake.decline(request, envelope),
    );

    // Heard, subscribed before this returns.
    let mut configuration = bus.subscribe_every(ENTITLEMENTS_CHANGED);
    let mut replaced = bus.subscribe_every(INSTRUMENT_REPLACED);
    let mut instrument_missing = bus.subscribe_every(INSTRUMENT_MISSING);
    let mut venue_missing = bus.subscribe_every(VENUE_MISSING);
    let mut calendar_applied = bus.subscribe_every(CALENDAR_APPLIED);
    let hearing = Arc::clone(&lake);
    tokio::spawn(async move {
        loop {
            let heard = tokio::select! {
                Some(d) = configuration.recv() => d,
                Some(d) = replaced.recv() => d,
                Some(d) = instrument_missing.recv() => d,
                Some(d) = venue_missing.recv() => d,
                Some(d) = calendar_applied.recv() => d,
                else => return,
            };
            let lake = Arc::clone(&hearing);
            let _ = tokio::task::spawn_blocking(move || lake.hear(heard.envelope)).await;
        }
    });
    let sweeping = Arc::clone(&lake);
    tokio::spawn(async move {
        let mut every = tokio::time::interval(SWEEP_EVERY);
        loop {
            every.tick().await;
            let lake = Arc::clone(&sweeping);
            let _ = tokio::task::spawn_blocking(move || lake.sweep_wants()).await;
        }
    });
    let counting = Arc::clone(&lake);
    tokio::spawn(async move {
        let mut every = tokio::time::interval(TRADES_LOGGED_EVERY);
        every.tick().await;
        loop {
            every.tick().await;
            counting.log_trades_recorded(TRADES_LOGGED_EVERY);
        }
    });
    let keeping = Arc::clone(&lake);
    tokio::spawn(async move {
        let mut every = tokio::time::interval(RETENTION_EVERY);
        loop {
            every.tick().await;
            let lake = Arc::clone(&keeping);
            let _ = tokio::task::spawn_blocking(move || lake.apply_retention()).await;
        }
    });
    lake
}

impl Lake {
    /// One event heard.
    pub fn hear(&self, envelope: Envelope) {
        let now = self.clock.now_ns();
        match envelope.payload_type.as_str() {
            "meridian.v1.EntitlementsChangedEvent" => {
                match EntitlementsChangedEvent::decode(&envelope.payload[..]) {
                    Ok(event) => self.configured(&event),
                    Err(failed) => tracing::warn!(%failed, "the data configuration did not read"),
                }
            }
            "meridian.v1.InstrumentReplacedEvent" => {
                if let Ok(event) = InstrumentReplacedEvent::decode(&envelope.payload[..]) {
                    let record = event.instrument.unwrap_or_default();
                    let said = (record.asset_class, record.instrument_type);
                    let stays = record.instrument_id;
                    if !event.replaced_instrument_id.is_empty() && !stays.is_empty() {
                        if let Err(failed) =
                            self.store
                                .keep_alias(&event.replaced_instrument_id, &stays, now)
                        {
                            tracing::warn!(%failed, "a replacement was not kept");
                        }
                        self.held.lock().expect("held poisoned").insert(stays, said);
                    }
                }
            }
            "meridian.v1.MissingInstrumentDetectedEvent" => {
                if let Ok(event) = MissingInstrumentDetectedEvent::decode(&envelope.payload[..]) {
                    self.miss(&event.publisher_instance_id, now);
                }
            }
            "meridian.v1.MissingVenueDetectedEvent" => {
                if let Ok(event) = MissingVenueDetectedEvent::decode(&envelope.payload[..]) {
                    self.miss(&event.publisher_instance_id, now);
                }
            }
            "meridian.v1.CalendarAppliedEvent" => {
                match meridian_domain::v1::CalendarAppliedEvent::decode(&envelope.payload[..]) {
                    Ok(event) => self.recheck(&event),
                    Err(failed) => tracing::warn!(%failed, "a calendar applied did not read"),
                }
            }
            _ => {}
        }
    }

    fn miss(&self, instance: &str, now: i64) {
        if instance.is_empty() {
            return;
        }
        if let Err(failed) = self.store.count_miss(instance, now) {
            tracing::warn!(%failed, "a miss was not counted");
        }
    }

    /// Trades recorded over the window just ended, per second: what the
    /// `lake_trades_recorded_per_second` band reads (contract v19, the plan's
    /// Q40). Answers what it logged.
    pub fn log_trades_recorded(&self, window: Duration) -> f64 {
        let trades = self
            .trades_recorded
            .swap(0, std::sync::atomic::Ordering::Relaxed);
        let per_second = trades as f64 / window.as_secs_f64().max(1.0);
        tracing::info!(
            lake_trades_recorded_per_second = per_second,
            trades,
            "trades recorded this minute"
        );
        per_second
    }

    /// Rows kept per dataset: what the `lake_rows_per_dataset` band reads.
    pub fn rows_per_dataset(&self) -> BTreeMap<String, u64> {
        self.store
            .counts()
            .map(|c| c.into_iter().map(|(d, (rows, _))| (d, rows)).collect())
            .unwrap_or_default()
    }

    /// The business date a moment falls on in UTC: a convenience for a
    /// caller's want of today's close.
    pub fn today(&self) -> Date {
        Date::of_utc_instant(self.clock.now_ns())
    }
}
