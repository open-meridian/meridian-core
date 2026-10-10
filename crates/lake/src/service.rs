//! Where the lake meets the bus (W10).
//!
//! Three commands and four queries in; three events heard and one more kept
//! from; three queries asked; four events out. A `dgm` records prices and bars
//! in batches and declines what it cannot serve of a want; the reading roles
//! read prices, bars and the datasets; the dashboard sets and lists the
//! priority and lists the datasets. The conductor's data configuration is
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

use meridian_bus::{Bus, Envelope};
use meridian_domain::date::Date;
use meridian_domain::lake;
use meridian_domain::money::{asset_of, Asset, CashInstruments, ISO4217};
use meridian_domain::v1::PartitionSequence;
use meridian_domain::v1::{
    BarsRecordedEvent, DatasetRef, DeclineWantReply, DeclineWantRequest, EntitlementsChangedEvent,
    Identifier, InstrumentReplacedEvent, ListBarsReply, ListBarsRequest, ListDatasetsReply,
    ListDatasetsRequest, ListPricesReply, ListPricesRequest, ListSourcePrioritiesReply,
    ListSourcePrioritiesRequest, MissingInstrumentDetectedEvent, MissingVenueDetectedEvent,
    ObservationsWantedEvent, PluginCatalogue, PluginCatalogueRequest, PluginLaunchState,
    PriceBasis, PriceKind, PricesRecordedEvent, RecordBarsRequest, RecordObservationsReply,
    RecordPricesRequest, ResolveIdentifierReply, ResolveIdentifierRequest, ResolveInstrumentReply,
    ResolveInstrumentRequest, SetSourcePriorityRequest, SourceChoice, SourcePriority,
    SourceTimeKind, SubjectRef, Unanswered, UnansweredReason, WantWithdrawnEvent, Watermark,
};
use meridian_pb::bounds;
use prost::Message;

pub use meridian_clock::Clock;

use crate::config::{DataConfig, Reader};
use crate::read::{self, Against, Read};
use crate::row::{DataType, Observation};
use crate::store::{Store, WantChange, WantChangeKind, When};

pub const RECORD_PRICES: &str = "platform.lake.command.record-prices";
pub const RECORD_BARS: &str = "platform.lake.command.record-bars";
pub const DECLINE_WANT: &str = "platform.lake.command.decline-want";
pub const SET_SOURCE_PRIORITY: &str = "platform.lake.command.set-source-priority";
pub const LIST_PRICES: &str = "platform.lake.query.list-prices";
pub const LIST_BARS: &str = "platform.lake.query.list-bars";
pub const LIST_DATASETS: &str = "platform.lake.query.list-datasets";
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

/// A dataset's subject for a data type's recorded rows.
pub fn recorded_topic(dataset: &str, data_type: DataType) -> String {
    match data_type {
        DataType::Price => format!("platform.lake.{dataset}.event.prices-recorded"),
        DataType::Bar => format!("platform.lake.{dataset}.event.bars-recorded"),
    }
}

/// How long the lake waits on the instrument store or the conductor: a
/// batch the store does not answer in it is refused to be tried again.
const ASKING: Duration = Duration::from_secs(3);

/// How often a want asked and not answered is asked again.
const ASK_AGAIN_NS: i64 = 60 * 1_000_000_000;

/// How often the standing wants are swept for withdrawal, and how often
/// retention is applied.
pub const SWEEP_EVERY: Duration = Duration::from_secs(15);
pub const RETENTION_EVERY: Duration = Duration::from_secs(3600);

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
    clock: Arc<dyn Clock>,
    config: RwLock<DataConfig>,
    wants: Mutex<Wants>,
    declined: Mutex<BTreeMap<(String, String), (UnansweredReason, i64)>>,
    held: Mutex<BTreeSet<String>>,
    cash: Mutex<CashInstruments>,
    versions: Mutex<(BTreeMap<String, String>, i64)>,
}

fn reader_of(envelope: &Envelope) -> Reader {
    match envelope.meta.as_ref() {
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
            held: Mutex::new(BTreeSet::new()),
            cash: Mutex::new(CashInstruments::default()),
            versions: Mutex::new((BTreeMap::new(), 0)),
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
        if self.held.lock().expect("held poisoned").contains(entity) {
            return Ok(true);
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
        let held = reply.found && reply.instrument.is_some();
        if held {
            self.held
                .lock()
                .expect("held poisoned")
                .insert(entity.to_string());
        }
        Ok(held)
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
        let read: Result<PluginCatalogue, String> = self.ask(
            PLUGIN_CATALOGUE,
            "meridian.v1.PluginCatalogueRequest",
            PluginCatalogueRequest {},
        );
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
            if !(1..=8).contains(&meta.subjects.len()) {
                return Err(format!(
                    "{at}.meta.subjects names {}; 1 to 8",
                    meta.subjects.len()
                ));
            }
            for (j, subject) in meta.subjects.iter().enumerate() {
                if !(1..=64).contains(&subject.entity_id.chars().count()) {
                    return Err(format!(
                        "{at}.meta.subjects[{j}].entity_id is 1 to 64 characters"
                    ));
                }
                if !self.subject_held(&subject.entity_id)? {
                    return Err(format!(
                        "{at}.meta.subjects[{j}].entity_id {:?} is no record the deployment holds",
                        subject.entity_id
                    ));
                }
            }
            meridian_domain::date::optional(
                &format!("{at}.meta.business_date"),
                &meta.business_date,
            )?;
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
        let bound = if field == "prices" {
            bounds::RECORD_PRICES_REQUEST_PRICES_COUNT
        } else {
            bounds::RECORD_BARS_REQUEST_BARS_COUNT
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
        for (dataset, rows) in by_dataset {
            datasets.insert(dataset.clone());
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
            for row in &done.rows {
                subjects.extend(row.subjects());
                self.publish(&dataset, row);
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

    fn publish(&self, dataset: &str, row: &Observation) {
        let (payload_type, payload) = match row {
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
    /// identical asks and asking again only after a minute.
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
                When::Latest { .. } => (String::new(), 0, 0),
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
        {
            let mut wants = self.wants.lock().expect("wants poisoned");
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
        let reader = reader_of(envelope);
        let field = |name: &str| name.to_string();
        if !bounds::LIST_PRICES_REQUEST_SUBJECTS_COUNT.admits(request.subjects.len()) {
            return Err(format!(
                "{}: subjects names {}; 1 to 500",
                field("subjects"),
                request.subjects.len()
            ));
        }
        let given = [
            request.at_ns != 0,
            !request.business_date.is_empty(),
            request.valid_from_ns != 0 || request.valid_until_ns != 0,
        ];
        if given.iter().filter(|g| **g).count() > 1 {
            return Err("at_ns, business_date and a valid-time range: at most one is given".into());
        }
        let when = if let Some(date) =
            meridian_domain::date::optional("business_date", &request.business_date)?
        {
            When::BusinessDate(date)
        } else if given[2] {
            if request.valid_until_ns != 0 && request.valid_until_ns <= request.valid_from_ns {
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
        if offset == 0 {
            self.want(answered.asks.clone(), &reader);
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

    pub(crate) fn list_datasets(&self, envelope: &Envelope) -> Result<ListDatasetsReply, String> {
        let reader = reader_of(envelope);
        let config = self.config();
        let counts = self.store.counts().map_err(|failed| failed.to_string())?;
        let misses = self
            .store
            .miss_counts()
            .map_err(|failed| failed.to_string())?;
        let mut reply = ListDatasetsReply::default();
        for (dataset, held) in &config.datasets {
            if config.fields_for(dataset, &reader).is_none() {
                continue;
            }
            reply.datasets.push(DatasetRef {
                unconverted_count: counts.get(dataset).map_or(0, |c| c.1),
                miss_count: misses.get(&held.instance).copied().unwrap_or(0),
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
            DataType::Bar if kind != PriceKind::Unspecified => {
                return Err("kind: a bar's priority names no kind".into())
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
        let dataset = self
            .wants
            .lock()
            .expect("wants poisoned")
            .open
            .get(&request.want_id)
            .map(|w| w.event.dataset.clone())
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
            let mut declined = self.declined.lock().expect("declined poisoned");
            for subject in &subjects {
                declined.insert((dataset.clone(), subject.clone()), (reason, now));
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

/// A read's request, prices' or bars'.
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
}

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
    let lake = Lake::new(Arc::clone(&bus), store, clock);
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
                },
                envelope,
            )?;
            Ok(ListPricesReply {
                prices: answer
                    .rows
                    .into_iter()
                    .filter_map(|r| match r {
                        Observation::Price(p) => Some(p),
                        Observation::Bar(_) => None,
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
                },
                envelope,
            )?;
            Ok(ListBarsReply {
                bars: answer
                    .rows
                    .into_iter()
                    .filter_map(|r| match r {
                        Observation::Bar(b) => Some(b),
                        Observation::Price(_) => None,
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
    let mut configuration = bus.subscribe(ENTITLEMENTS_CHANGED);
    let mut replaced = bus.subscribe(INSTRUMENT_REPLACED);
    let mut instrument_missing = bus.subscribe(INSTRUMENT_MISSING);
    let mut venue_missing = bus.subscribe(VENUE_MISSING);
    let hearing = Arc::clone(&lake);
    tokio::spawn(async move {
        loop {
            let heard = tokio::select! {
                Some(d) = configuration.recv() => d,
                Some(d) = replaced.recv() => d,
                Some(d) = instrument_missing.recv() => d,
                Some(d) = venue_missing.recv() => d,
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
                    let stays = event
                        .instrument
                        .map(|i| i.instrument_id)
                        .unwrap_or_default();
                    if !event.replaced_instrument_id.is_empty() && !stays.is_empty() {
                        if let Err(failed) =
                            self.store
                                .keep_alias(&event.replaced_instrument_id, &stays, now)
                        {
                            tracing::warn!(%failed, "a replacement was not kept");
                        }
                        self.held.lock().expect("held poisoned").insert(stays);
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
