//! What a sidecar does for the lake (contract v18, W10; W4.3).
//!
//! **Its plugin's entitlements**, kept from the data configuration the
//! conductor publishes whole (W10.1): which datasets the plugin may read,
//! and of each which fields. A lake row's subject names its dataset; the
//! sidecar subscribes to the subjects of the datasets its plugin is entitled
//! to alone, as the broker lets it (W10.1's first place), and strips from
//! each row the fields its plugin may not read (its second); the lake
//! refuses the rest at read (its third).
//!
//! **Narrowed to the subjects a receive names** (spec/the-lake, Q12): a
//! plugin holding ten instruments hears rows about those ten, never a whole
//! market. A want is delivered to the instance serving its dataset alone.
//!
//! **Conflated, or not** (decisions/024; contract v19): prices latest value
//! first per subject, dataset, kind, venue and asset, bars per subject,
//! dataset, venue and interval start, quotes per subject, dataset, venue and
//! asset; trades never, each delivered or its loss marked, and the plugin
//! reads those recorded after the watermark it last saw (W4.3, W10.6).
//!
//! **A business date is a date** (ruling 3 of 2026-10-09): a batch or a read
//! naming one that is not a day that exists is refused here, naming its
//! field, before anything is sent.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use meridian_bus::Bus;
use meridian_domain::v1::{
    AgeHop, BarsRecordedEvent, CorporateActionsRecordedEvent, EntitlementsChangedEvent,
    FactorsRecordedEvent, FilingsRecordedEvent, ListBarsRequest, ListFactorsRequest,
    ListPricesRequest, ListRatesRequest, LivenessRecordedEvent, ObservationMeta,
    ObservationsWantedEvent, OutcomesRecordedEvent, PricesRecordedEvent, QuotesRecordedEvent,
    RatesRecordedEvent, RecordBarsRequest, RecordFactorsRequest, RecordPricesRequest,
    RecordQuotesRequest, RecordRatesRequest, RecordTradesRequest, SeriesRecordedEvent,
    StatisticsRecordedEvent, TradesRecordedEvent, WantWithdrawnEvent,
};
use prost::Message;
use tokio::sync::watch;

pub const ENTITLEMENTS_CHANGED: &str = "platform.config.event.entitlements-changed";

/// The segment a lake row's topic names its dataset by.
pub const DATASET: &str = "{dataset}";

/// By dataset, the fields a plugin may read; an empty set for every field.
type Fields = BTreeMap<String, BTreeSet<String>>;

/// The plugin's entitlements, as the conductor last published them: by
/// dataset, the fields it may read, an empty set for every field. None
/// until the configuration is first heard, when it reads nothing.
#[derive(Clone)]
pub struct Entitled {
    instance: String,
    held: Arc<RwLock<Option<Fields>>>,
    watching: Arc<AtomicBool>,
    changed: Arc<watch::Sender<u64>>,
}

impl Entitled {
    pub(crate) fn new(instance: String) -> Entitled {
        Entitled {
            instance,
            held: Arc::default(),
            watching: Arc::default(),
            changed: Arc::new(watch::channel(0).0),
        }
    }

    /// Hear the data configuration from now on: once, however often asked.
    /// Inside a runtime.
    pub(crate) fn watch(&self, bus: &Arc<Bus>) {
        if self.watching.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut heard = bus.subscribe_every(ENTITLEMENTS_CHANGED);
        let entitled = self.clone();
        tokio::spawn(async move {
            while let Some(delivery) = heard.recv().await {
                match EntitlementsChangedEvent::decode(&delivery.envelope.payload[..]) {
                    Ok(event) => entitled.heard(&event),
                    Err(failed) => tracing::warn!(%failed, "a data configuration did not read"),
                }
            }
        });
    }

    /// The configuration heard: this plugin's part kept, and whatever waits
    /// on it woken where it changed.
    pub fn heard(&self, event: &EntitlementsChangedEvent) {
        let mine = meridian_domain::lake::entitled_fields(event, &self.instance);
        let mut held = self.held.write().expect("entitlements poisoned");
        if held.as_ref() != Some(&mine) {
            *held = Some(mine);
            drop(held);
            self.changed.send_modify(|seen| *seen += 1);
        }
    }

    /// The datasets the plugin may read now.
    pub fn datasets(&self) -> BTreeSet<String> {
        self.held
            .read()
            .expect("entitlements poisoned")
            .as_ref()
            .map(|held| held.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// The fields it may read of a dataset: None for none of it.
    pub fn fields(&self, dataset: &str) -> Option<BTreeSet<String>> {
        self.held
            .read()
            .expect("entitlements poisoned")
            .as_ref()
            .and_then(|held| held.get(dataset).cloned())
    }

    pub(crate) fn changes(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }
}

/// What a lake row says of itself, as the receive decides by it.
pub(crate) struct Said {
    pub dataset: String,
    pub subjects: Vec<String>,
    /// The latest value first under load, per this key (decisions/024):
    /// a price per subject, dataset, kind, venue and asset; a bar per
    /// subject, dataset, venue and interval start; a quote per subject,
    /// dataset, venue and asset (contract v19); a rate per subject, dataset,
    /// kind, tenor, maturity and convention (contract v27); a want per want.
    /// None for a trade, which is never conflated.
    pub key: Option<String>,
    pub is_want: bool,
    /// Its series key (contract v28, ruling 561), empty for a row keyed by
    /// none: what a subscription's series and window are held to (contract
    /// v28, amended 2026-10-30).
    pub series: Vec<meridian_domain::v1::Identifier>,
}

/// A row's series key as a key's last part (contract v28, ruling 561): a
/// chain's rows under one underlying are conflated per series; a row keyed
/// by none keeps the key it had.
fn series_said(meta: Option<&ObservationMeta>) -> String {
    let series = meta
        .map(|meta| meridian_domain::lake::series_key_text(&meta.series_key))
        .unwrap_or_default();
    if series.is_empty() {
        series
    } else {
        format!("|#{series}")
    }
}

/// A row's series key, empty for a row keyed by none.
fn series_text(meta: Option<&ObservationMeta>) -> Vec<meridian_domain::v1::Identifier> {
    meta.map(|meta| meta.series_key.clone()).unwrap_or_default()
}

/// The envelope's dictionary entry of a lake row's message, which a
/// subscription's fields always keep (contract v28, amended 2026-10-30).
pub(crate) fn meta_entry(payload_type: &str) -> Option<&'static str> {
    let data_type = match payload_type {
        "meridian.v1.PricesRecordedEvent" => meridian_domain::lake::PRICE,
        "meridian.v1.BarsRecordedEvent" => meridian_domain::lake::BAR,
        "meridian.v1.TradesRecordedEvent" => meridian_domain::lake::TRADE,
        "meridian.v1.QuotesRecordedEvent" => meridian_domain::lake::QUOTE,
        "meridian.v1.OutcomesRecordedEvent" => meridian_domain::lake::OUTCOME,
        "meridian.v1.RatesRecordedEvent" => meridian_domain::lake::RATE,
        "meridian.v1.StatisticsRecordedEvent" => meridian_domain::lake::STATISTIC,
        "meridian.v1.SeriesRecordedEvent" => meridian_domain::lake::LISTED_SERIES,
        "meridian.v1.FilingsRecordedEvent" => meridian_domain::lake::FILING,
        "meridian.v1.CorporateActionsRecordedEvent" => meridian_domain::lake::CORPORATE_ACTION,
        "meridian.v1.FactorsRecordedEvent" => meridian_domain::lake::FACTOR,
        _ => return None,
    };
    meridian_domain::lake::fields_of(data_type).first().copied()
}

fn meta_said(meta: Option<&ObservationMeta>) -> (String, Vec<String>, String) {
    let meta = meta.cloned().unwrap_or_default();
    let source = meta.source.unwrap_or_default();
    (
        source.dataset,
        meta.subjects.into_iter().map(|s| s.entity_id).collect(),
        source.venue_id,
    )
}

/// Each real-time row's age at `hop` now -- the batch a data plugin sent,
/// accepted (`sidecar`), or a row delivered to a reader (`bus`) -- recorded
/// for this sidecar's next report to the monitor (W4.16; the latency spec's
/// "The hops"; ruling 606). Trades and quotes alone: a daily value's age is
/// its date's, not a hop's.
pub(crate) fn record_ages(payload_type: &str, payload: &[u8], hop: AgeHop, now_ns: i64) {
    let metas: Vec<ObservationMeta> = match payload_type {
        "meridian.v1.TradesRecordedEvent" => TradesRecordedEvent::decode(payload)
            .ok()
            .and_then(|e| e.trade)
            .and_then(|t| t.meta)
            .into_iter()
            .collect(),
        "meridian.v1.QuotesRecordedEvent" => QuotesRecordedEvent::decode(payload)
            .ok()
            .and_then(|e| e.quote)
            .and_then(|q| q.meta)
            .into_iter()
            .collect(),
        "meridian.v1.RecordTradesRequest" => RecordTradesRequest::decode(payload)
            .map(|r| r.trades.into_iter().filter_map(|t| t.meta).collect())
            .unwrap_or_default(),
        "meridian.v1.RecordQuotesRequest" => RecordQuotesRequest::decode(payload)
            .map(|r| r.quotes.into_iter().filter_map(|q| q.meta).collect())
            .unwrap_or_default(),
        _ => return,
    };
    let recorder = meridian_domain::latency::recorder();
    for meta in &metas {
        let dataset = meta
            .source
            .as_ref()
            .map(|s| s.dataset.as_str())
            .unwrap_or_default();
        recorder.record(
            dataset,
            hop,
            now_ns - meridian_domain::latency::source_ns(meta),
        );
    }
}

/// A lake row's payload read for what the receive decides by; None for a
/// payload of no lake row.
pub(crate) fn said(payload_type: &str, payload: &[u8]) -> Option<Said> {
    match payload_type {
        "meridian.v1.PricesRecordedEvent" => {
            let price = PricesRecordedEvent::decode(payload).ok()?.price?;
            let (dataset, subjects, venue) = meta_said(price.meta.as_ref());
            let asset = meridian_domain::lake::asset_key(price.price.as_ref());
            let series = series_said(price.meta.as_ref());
            Some(Said {
                key: Some(format!(
                    "price|{}|{dataset}|{}|{venue}|{asset}{series}",
                    subjects.first().cloned().unwrap_or_default(),
                    price.kind
                )),
                dataset,
                subjects,
                is_want: false,
                series: series_text(price.meta.as_ref()),
            })
        }
        "meridian.v1.BarsRecordedEvent" => {
            let bar = BarsRecordedEvent::decode(payload).ok()?.bar?;
            let from = bar.meta.as_ref().map_or(0, |m| m.valid_from_ns);
            let (dataset, subjects, venue) = meta_said(bar.meta.as_ref());
            let series = series_said(bar.meta.as_ref());
            Some(Said {
                key: Some(format!(
                    "bar|{}|{dataset}|{venue}|{from}{series}",
                    subjects.first().cloned().unwrap_or_default()
                )),
                dataset,
                subjects,
                is_want: false,
                series: series_text(bar.meta.as_ref()),
            })
        }
        "meridian.v1.TradesRecordedEvent" => {
            let trade = TradesRecordedEvent::decode(payload).ok()?.trade?;
            let (dataset, subjects, _) = meta_said(trade.meta.as_ref());
            Some(Said {
                key: None,
                dataset,
                subjects,
                is_want: false,
                series: series_text(trade.meta.as_ref()),
            })
        }
        // Every declared corporate action, never conflated (contract v28,
        // W10.5): narrowed to the subjects a receive names, the security's
        // or its issuer's.
        "meridian.v1.CorporateActionsRecordedEvent" => {
            let action = CorporateActionsRecordedEvent::decode(payload)
                .ok()?
                .action?;
            let (dataset, subjects, _) = meta_said(action.meta.as_ref());
            Some(Said {
                key: None,
                dataset,
                subjects,
                is_want: false,
                series: Vec::new(),
            })
        }
        // Every filing, never conflated (contract v28, W10.5): narrowed to
        // the subjects a receive names, its filer's or another's.
        "meridian.v1.FilingsRecordedEvent" => {
            let filing = FilingsRecordedEvent::decode(payload).ok()?.filing?;
            let (dataset, subjects, _) = meta_said(filing.meta.as_ref());
            Some(Said {
                key: None,
                dataset,
                subjects,
                is_want: false,
                series: Vec::new(),
            })
        }
        // Every factor, never conflated (contract v29, W10.5): narrowed to
        // the pools and classes a receive names.
        "meridian.v1.FactorsRecordedEvent" => {
            let factor = FactorsRecordedEvent::decode(payload).ok()?.factor?;
            let (dataset, subjects, _) = meta_said(factor.meta.as_ref());
            Some(Said {
                key: None,
                dataset,
                subjects,
                is_want: false,
                series: Vec::new(),
            })
        }
        // Every version of an outcome, never conflated (contract v23).
        "meridian.v1.OutcomesRecordedEvent" => {
            let outcome = OutcomesRecordedEvent::decode(payload).ok()?.outcome?;
            let (dataset, subjects, _) = meta_said(outcome.meta.as_ref());
            Some(Said {
                key: None,
                dataset,
                subjects,
                is_want: false,
                series: Vec::new(),
            })
        }
        "meridian.v1.QuotesRecordedEvent" => {
            let quote = QuotesRecordedEvent::decode(payload).ok()?.quote?;
            let (dataset, subjects, venue) = meta_said(quote.meta.as_ref());
            let asset = meridian_domain::lake::quote_asset(&quote);
            let series = series_said(quote.meta.as_ref());
            Some(Said {
                key: Some(format!(
                    "quote|{}|{dataset}|{venue}|{asset}{series}",
                    subjects.first().cloned().unwrap_or_default()
                )),
                dataset,
                subjects,
                is_want: false,
                series: series_text(quote.meta.as_ref()),
            })
        }
        // Contract v27, the rates slice (W10.5).
        "meridian.v1.RatesRecordedEvent" => {
            let rate = RatesRecordedEvent::decode(payload).ok()?.rate?;
            let (dataset, subjects, _) = meta_said(rate.meta.as_ref());
            Some(Said {
                key: Some(format!(
                    "rate|{}|{dataset}|{}|{}",
                    subjects.first().cloned().unwrap_or_default(),
                    rate.kind,
                    meridian_domain::lake::rate_point(&rate)
                )),
                dataset,
                subjects,
                is_want: false,
                series: Vec::new(),
            })
        }
        // Contract v28 (W10.5): per subject, series key, kind and business
        // date.
        "meridian.v1.StatisticsRecordedEvent" => {
            let statistic = StatisticsRecordedEvent::decode(payload).ok()?.statistic?;
            let (dataset, subjects, _) = meta_said(statistic.meta.as_ref());
            let date = statistic
                .meta
                .as_ref()
                .map(|meta| meta.business_date.clone())
                .unwrap_or_default();
            Some(Said {
                key: Some(format!(
                    "statistic|{}|{dataset}|{}|{date}{}",
                    subjects.first().cloned().unwrap_or_default(),
                    statistic.kind,
                    series_said(statistic.meta.as_ref())
                )),
                dataset,
                subjects,
                is_want: false,
                series: series_text(statistic.meta.as_ref()),
            })
        }
        // Contract v27 (W10.5; ruling 418): a dataset's liveness, about no
        // subject, heard by every reader of the dataset, conflated per
        // dataset and venue.
        "meridian.v1.LivenessRecordedEvent" => {
            let alive = LivenessRecordedEvent::decode(payload).ok()?.liveness?;
            Some(Said {
                key: Some(format!("liveness|{}|{}", alive.dataset, alive.venue_id)),
                dataset: alive.dataset,
                subjects: Vec::new(),
                is_want: false,
                series: Vec::new(),
            })
        }
        // Contract v28, amended 2026-10-30 (W10.5; rulings 847 and 886): a
        // listed series, conflated per subject, dataset and series key.
        "meridian.v1.SeriesRecordedEvent" => {
            let series = SeriesRecordedEvent::decode(payload).ok()?.series?;
            let (dataset, subjects, _) = meta_said(series.meta.as_ref());
            Some(Said {
                key: Some(format!(
                    "series|{}|{dataset}{}",
                    subjects.first().cloned().unwrap_or_default(),
                    series_said(series.meta.as_ref())
                )),
                dataset,
                subjects,
                is_want: false,
                series: series_text(series.meta.as_ref()),
            })
        }
        "meridian.v1.ObservationsWantedEvent" => {
            let want = ObservationsWantedEvent::decode(payload).ok()?;
            Some(Said {
                key: Some(format!("want|{}", want.want_id)),
                dataset: want.dataset,
                subjects: Vec::new(),
                is_want: true,
                series: Vec::new(),
            })
        }
        "meridian.v1.WantWithdrawnEvent" => {
            let withdrawn = WantWithdrawnEvent::decode(payload).ok()?;
            Some(Said {
                key: Some(format!("want|{}", withdrawn.want_id)),
                dataset: withdrawn.dataset,
                subjects: Vec::new(),
                is_want: true,
                series: Vec::new(),
            })
        }
        _ => None,
    }
}

/// A recorded row with the fields its reader may not read removed, as the
/// row's message re-encoded; the payload as it came for any other.
pub(crate) fn stripped(payload_type: &str, payload: &[u8], allowed: &BTreeSet<String>) -> Vec<u8> {
    if allowed.is_empty() {
        return payload.to_vec();
    }
    match payload_type {
        "meridian.v1.PricesRecordedEvent" => match PricesRecordedEvent::decode(payload) {
            Ok(mut event) => {
                if let Some(price) = event.price.as_mut() {
                    meridian_domain::lake::strip_price(price, allowed);
                }
                event.encode_to_vec()
            }
            Err(_) => payload.to_vec(),
        },
        "meridian.v1.BarsRecordedEvent" => match BarsRecordedEvent::decode(payload) {
            Ok(mut event) => {
                if let Some(bar) = event.bar.as_mut() {
                    meridian_domain::lake::strip_bar(bar, allowed);
                }
                event.encode_to_vec()
            }
            Err(_) => payload.to_vec(),
        },
        "meridian.v1.TradesRecordedEvent" => match TradesRecordedEvent::decode(payload) {
            Ok(mut event) => {
                if let Some(trade) = event.trade.as_mut() {
                    meridian_domain::lake::strip_trade(trade, allowed);
                }
                event.encode_to_vec()
            }
            Err(_) => payload.to_vec(),
        },
        "meridian.v1.OutcomesRecordedEvent" => match OutcomesRecordedEvent::decode(payload) {
            Ok(mut event) => {
                if let Some(outcome) = event.outcome.as_mut() {
                    meridian_domain::lake::strip_outcome(outcome, allowed);
                }
                event.encode_to_vec()
            }
            Err(_) => payload.to_vec(),
        },
        "meridian.v1.QuotesRecordedEvent" => match QuotesRecordedEvent::decode(payload) {
            Ok(mut event) => {
                if let Some(quote) = event.quote.as_mut() {
                    meridian_domain::lake::strip_quote(quote, allowed);
                }
                event.encode_to_vec()
            }
            Err(_) => payload.to_vec(),
        },
        "meridian.v1.RatesRecordedEvent" => match RatesRecordedEvent::decode(payload) {
            Ok(mut event) => {
                if let Some(rate) = event.rate.as_mut() {
                    meridian_domain::lake::strip_rate(rate, allowed);
                }
                event.encode_to_vec()
            }
            Err(_) => payload.to_vec(),
        },
        "meridian.v1.CorporateActionsRecordedEvent" => {
            match CorporateActionsRecordedEvent::decode(payload) {
                Ok(mut event) => {
                    if let Some(action) = event.action.as_mut() {
                        meridian_domain::lake::strip_corporate_action(action, allowed);
                    }
                    event.encode_to_vec()
                }
                Err(_) => payload.to_vec(),
            }
        }
        "meridian.v1.FactorsRecordedEvent" => match FactorsRecordedEvent::decode(payload) {
            Ok(mut event) => {
                if let Some(factor) = event.factor.as_mut() {
                    meridian_domain::lake::strip_factor(factor, allowed);
                }
                event.encode_to_vec()
            }
            Err(_) => payload.to_vec(),
        },
        "meridian.v1.FilingsRecordedEvent" => match FilingsRecordedEvent::decode(payload) {
            Ok(mut event) => {
                if let Some(filing) = event.filing.as_mut() {
                    meridian_domain::lake::strip_filing(filing, allowed);
                }
                event.encode_to_vec()
            }
            Err(_) => payload.to_vec(),
        },
        "meridian.v1.StatisticsRecordedEvent" => match StatisticsRecordedEvent::decode(payload) {
            Ok(mut event) => {
                if let Some(statistic) = event.statistic.as_mut() {
                    meridian_domain::lake::strip_statistic(statistic, allowed);
                }
                event.encode_to_vec()
            }
            Err(_) => payload.to_vec(),
        },
        "meridian.v1.SeriesRecordedEvent" => match SeriesRecordedEvent::decode(payload) {
            Ok(mut event) => {
                if let Some(series) = event.series.as_mut() {
                    meridian_domain::lake::strip_series(series, allowed);
                }
                event.encode_to_vec()
            }
            Err(_) => payload.to_vec(),
        },
        _ => payload.to_vec(),
    }
}

/// The business dates a plugin's batch or read names, each a day that
/// exists, or the refusal naming the first that is not.
pub(crate) fn dates_refused(payload_type: &str, payload: &[u8]) -> Option<String> {
    let check = |field: String, text: &str| meridian_domain::date::optional(&field, text).err();
    match payload_type {
        "meridian.v1.RecordPricesRequest" => {
            let request = RecordPricesRequest::decode(payload).ok()?;
            request.prices.iter().enumerate().find_map(|(i, price)| {
                let date = price
                    .meta
                    .as_ref()
                    .map(|m| m.business_date.as_str())
                    .unwrap_or("");
                check(format!("prices[{i}].meta.business_date"), date)
            })
        }
        "meridian.v1.RecordBarsRequest" => {
            let request = RecordBarsRequest::decode(payload).ok()?;
            request.bars.iter().enumerate().find_map(|(i, bar)| {
                let date = bar
                    .meta
                    .as_ref()
                    .map(|m| m.business_date.as_str())
                    .unwrap_or("");
                check(format!("bars[{i}].meta.business_date"), date)
            })
        }
        "meridian.v1.ListPricesRequest" => {
            let request = ListPricesRequest::decode(payload).ok()?;
            check("business_date".into(), &request.business_date)
        }
        "meridian.v1.ListBarsRequest" => {
            let request = ListBarsRequest::decode(payload).ok()?;
            check("business_date".into(), &request.business_date)
        }
        // Contract v27, the rates slice.
        "meridian.v1.RecordRatesRequest" => {
            let request = RecordRatesRequest::decode(payload).ok()?;
            request.rates.iter().enumerate().find_map(|(i, rate)| {
                let date = rate
                    .meta
                    .as_ref()
                    .map(|m| m.business_date.as_str())
                    .unwrap_or("");
                check(format!("rates[{i}].meta.business_date"), date)
                    .or_else(|| check(format!("rates[{i}].maturity_date"), &rate.maturity_date))
            })
        }
        "meridian.v1.ListRatesRequest" => {
            let request = ListRatesRequest::decode(payload).ok()?;
            check("business_date".into(), &request.business_date)
        }
        // Contract v29 (W10.4, W10.6): a factor's publication, record and
        // payment dates, and a read's payment dates.
        "meridian.v1.RecordFactorsRequest" => {
            let request = RecordFactorsRequest::decode(payload).ok()?;
            request.factors.iter().enumerate().find_map(|(i, factor)| {
                let date = factor
                    .meta
                    .as_ref()
                    .map(|m| m.business_date.as_str())
                    .unwrap_or("");
                check(format!("factors[{i}].meta.business_date"), date)
                    .or_else(|| check(format!("factors[{i}].record_date"), &factor.record_date))
                    .or_else(|| check(format!("factors[{i}].payment_date"), &factor.payment_date))
            })
        }
        "meridian.v1.ListFactorsRequest" => {
            let request = ListFactorsRequest::decode(payload).ok()?;
            check("from_date".into(), &request.from_date)
                .or_else(|| check("to_date".into(), &request.to_date))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use meridian_domain::v1::{DatasetEntitlement, DatasetRef, Price, Source, SubjectRef};

    #[test]
    fn a_plugin_reads_the_datasets_it_is_entitled_to_with_their_fields() {
        let entitled = Entitled::new("reporting-1".into());
        assert!(entitled.datasets().is_empty(), "nothing until heard");
        entitled.heard(&EntitlementsChangedEvent {
            datasets: vec![DatasetRef {
                dataset: "coinbase-1:daily".into(),
                ..Default::default()
            }],
            entitlements: vec![
                DatasetEntitlement {
                    dataset: "coinbase-1:daily".into(),
                    instance: "reporting-1".into(),
                    allowed: true,
                    fields: vec!["meridian.v1.Bar.close".into()],
                    ..Default::default()
                },
                DatasetEntitlement {
                    dataset: "coinbase-1:daily".into(),
                    instance: "reporting-2".into(),
                    allowed: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
        assert_eq!(entitled.datasets(), ["coinbase-1:daily".to_string()].into());
        assert!(entitled
            .fields("coinbase-1:daily")
            .unwrap()
            .contains("meridian.v1.Bar.close"));
        assert!(entitled.fields("kraken-1:daily").is_none());
    }

    #[test]
    fn a_row_says_its_dataset_subjects_and_conflation_key_and_a_bad_date_is_refused() {
        let price = PricesRecordedEvent {
            price: Some(Price {
                meta: Some(ObservationMeta {
                    subjects: vec![SubjectRef {
                        entity_id: "LCL-BTC".into(),
                        ..Default::default()
                    }],
                    source: Some(Source {
                        dataset: "coinbase-1:daily".into(),
                        ..Default::default()
                    }),
                    business_date: "2026-02-30".into(),
                    ..Default::default()
                }),
                kind: 1,
                ..Default::default()
            }),
        };
        let said = said("meridian.v1.PricesRecordedEvent", &price.encode_to_vec()).unwrap();
        assert_eq!(said.dataset, "coinbase-1:daily");
        assert_eq!(said.subjects, vec!["LCL-BTC"]);
        assert_eq!(
            said.key.as_deref(),
            Some("price|LCL-BTC|coinbase-1:daily|1||")
        );
        let batch = RecordPricesRequest {
            prices: vec![price.price.unwrap()],
            want_id: String::new(),
        };
        let refused =
            dates_refused("meridian.v1.RecordPricesRequest", &batch.encode_to_vec()).unwrap();
        assert!(
            refused.starts_with("prices[0].meta.business_date:"),
            "{refused}"
        );
        let read = ListPricesRequest {
            business_date: "2026-10-09".into(),
            ..Default::default()
        };
        assert!(dates_refused("meridian.v1.ListPricesRequest", &read.encode_to_vec()).is_none());
    }

    #[test]
    fn a_filing_is_never_conflated_heard_by_its_filer_and_stripped() {
        // Contract v28, the filings slice (W10.5).
        use meridian_domain::v1::{Filing, FilingRef, ObservationMeta};
        let event = FilingsRecordedEvent {
            filing: Some(Filing {
                meta: Some(ObservationMeta {
                    row_key: "0000320193-26-000105".into(),
                    subjects: vec![SubjectRef {
                        entity_id: "ENT-APPLE".into(),
                        role: meridian_domain::v1::SubjectRole::Filer as i32,
                    }],
                    source: Some(Source {
                        dataset: "sec-edgar-1:filings".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                filing: Some(FilingRef {
                    accession_number: "0000320193-26-000105".into(),
                    form: "8-K".into(),
                }),
                kind: 3,
                items: vec!["2.02".into(), "9.01".into()],
                filed_date: "2026-10-30".into(),
                ..Default::default()
            }),
        }
        .encode_to_vec();
        let heard = said("meridian.v1.FilingsRecordedEvent", &event).unwrap();
        assert_eq!(heard.key, None, "every version delivered");
        assert_eq!(heard.dataset, "sec-edgar-1:filings");
        assert_eq!(heard.subjects, vec!["ENT-APPLE"]);
        let allowed: BTreeSet<String> =
            ["meridian.v1.Filing.meta", "meridian.v1.Filing.filed_date"]
                .into_iter()
                .map(String::from)
                .collect();
        let kept = FilingsRecordedEvent::decode(
            &stripped("meridian.v1.FilingsRecordedEvent", &event, &allowed)[..],
        )
        .unwrap()
        .filing
        .unwrap();
        assert!(kept.items.is_empty() && kept.filing.is_none());
        assert_eq!(kept.filed_date, "2026-10-30");
        assert_eq!(kept.kind, 3, "what the row is is never stripped");
    }

    #[test]
    fn a_factor_is_never_conflated_heard_by_its_pool_stripped_and_its_dates_checked() {
        // Contract v29 (ruling 680; W10.4, W10.5).
        use meridian_domain::v1::Factor;
        let row = Factor {
            meta: Some(ObservationMeta {
                subjects: vec![SubjectRef {
                    entity_id: "INS-POOL".into(),
                    ..Default::default()
                }],
                source: Some(Source {
                    dataset: "gse-disclosures-1:fannie_mae".into(),
                    ..Default::default()
                }),
                business_date: "2026-11-04".into(),
                ..Default::default()
            }),
            factor: Some(meridian_pb::v1::Decimal::default()),
            factor_month: "202610".into(),
            record_date: "2026-10-31".into(),
            payment_date: "2026-11-25".into(),
            coupon_rate: Some(meridian_pb::v1::Decimal::default()),
        };
        let event = FactorsRecordedEvent {
            factor: Some(row.clone()),
        }
        .encode_to_vec();
        let heard = said("meridian.v1.FactorsRecordedEvent", &event).unwrap();
        assert!(heard.key.is_none(), "every factor is delivered");
        assert_eq!(heard.subjects, vec!["INS-POOL"]);
        assert_eq!(
            meta_entry("meridian.v1.FactorsRecordedEvent"),
            Some("meridian.v1.Factor.meta")
        );
        let allowed: BTreeSet<String> = ["meridian.v1.Factor.meta", "meridian.v1.Factor.factor"]
            .into_iter()
            .map(String::from)
            .collect();
        let kept = FactorsRecordedEvent::decode(
            &stripped("meridian.v1.FactorsRecordedEvent", &event, &allowed)[..],
        )
        .unwrap()
        .factor
        .unwrap();
        assert!(kept.factor.is_some() && kept.coupon_rate.is_none());
        assert!(kept.factor_month.is_empty() && kept.payment_date.is_empty());
        let mut wrong = row;
        wrong.payment_date = "2026-11-31".into();
        let batch = RecordFactorsRequest {
            factors: vec![wrong],
            want_id: String::new(),
        };
        let refused =
            dates_refused("meridian.v1.RecordFactorsRequest", &batch.encode_to_vec()).unwrap();
        assert!(refused.starts_with("factors[0].payment_date"), "{refused}");
        let read = ListFactorsRequest {
            to_date: "2026-02-30".into(),
            ..Default::default()
        };
        let refused =
            dates_refused("meridian.v1.ListFactorsRequest", &read.encode_to_vec()).unwrap();
        assert!(refused.starts_with("to_date"), "{refused}");
    }

    #[test]
    fn a_corporate_action_is_never_conflated_heard_by_either_subject_and_stripped() {
        // Contract v28, the filings slice (W10.5).
        use meridian_domain::v1::{CorporateActionAnnounced, Money, ObservationMeta};
        let event = CorporateActionsRecordedEvent {
            action: Some(CorporateActionAnnounced {
                meta: Some(ObservationMeta {
                    subjects: vec![
                        SubjectRef {
                            entity_id: "INS-AAPL".into(),
                            ..Default::default()
                        },
                        SubjectRef {
                            entity_id: "ENT-APPLE".into(),
                            role: meridian_domain::v1::SubjectRole::Issuer as i32,
                        },
                    ],
                    source: Some(Source {
                        dataset: "sec-edgar-1:dividends".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                kind: 1,
                amount_per_share: Some(Money::default()),
                ex_date: "2026-11-10".into(),
                ..Default::default()
            }),
        }
        .encode_to_vec();
        let heard = said("meridian.v1.CorporateActionsRecordedEvent", &event).unwrap();
        assert_eq!(heard.key, None, "every version delivered");
        assert_eq!(heard.dataset, "sec-edgar-1:dividends");
        assert_eq!(heard.subjects, vec!["INS-AAPL", "ENT-APPLE"]);
        let allowed: BTreeSet<String> = [
            "meridian.v1.CorporateActionAnnounced.meta",
            "meridian.v1.CorporateActionAnnounced.kind",
            "meridian.v1.CorporateActionAnnounced.ex_date",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let kept = CorporateActionsRecordedEvent::decode(
            &stripped(
                "meridian.v1.CorporateActionsRecordedEvent",
                &event,
                &allowed,
            )[..],
        )
        .unwrap()
        .action
        .unwrap();
        assert!(kept.amount_per_share.is_none());
        assert_eq!(kept.ex_date, "2026-11-10");
        assert_eq!(kept.kind, 1);
    }

    #[test]
    fn a_rate_is_conflated_per_kind_tenor_maturity_and_convention() {
        // Contract v27 (W10.5): a bill's discount and coupon-equivalent rates
        // for one tenor never collapse, nor two tenors of one curve; the same
        // point again does.
        use meridian_domain::v1::{Rate, RateConvention, Tenor};
        let rate = |kind: i32, months: i32, compounding: i32| RatesRecordedEvent {
            rate: Some(Rate {
                meta: Some(ObservationMeta {
                    subjects: vec![SubjectRef {
                        entity_id: "INS-BILLS".into(),
                        ..Default::default()
                    }],
                    source: Some(Source {
                        dataset: "us-treasury-1:ust_bills".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                kind,
                tenor: Some(Tenor {
                    count: months,
                    unit: 3,
                }),
                convention: Some(RateConvention {
                    day_count: 1,
                    compounding,
                    periods_per_year: 0,
                }),
                ..Default::default()
            }),
        };
        let key = |event: RatesRecordedEvent| {
            let said = said("meridian.v1.RatesRecordedEvent", &event.encode_to_vec()).unwrap();
            assert_eq!(said.dataset, "us-treasury-1:ust_bills");
            assert_eq!(said.subjects, vec!["INS-BILLS"]);
            said.key.unwrap()
        };
        let discount = key(rate(6, 3, 4));
        assert_ne!(discount, key(rate(7, 3, 2)), "two conventions, one tenor");
        assert_ne!(discount, key(rate(6, 6, 4)), "two tenors");
        assert_eq!(discount, key(rate(6, 3, 4)), "the same point");
        assert!(discount.starts_with("rate|INS-BILLS|us-treasury-1:ust_bills|6|"));
    }

    #[test]
    fn a_chains_quotes_and_statistics_are_conflated_per_series() {
        // Contract v28 (W10.5; ruling 561): two series under one underlying
        // never collapse; a statistic per series, kind and trading date.
        use meridian_domain::v1::{Identifier, Quote, Statistic};
        let meta = |osi: &str, date: &str| {
            Some(ObservationMeta {
                subjects: vec![SubjectRef {
                    entity_id: "INS-AAPL".into(),
                    ..Default::default()
                }],
                source: Some(Source {
                    dataset: "tradier-1:options".into(),
                    ..Default::default()
                }),
                business_date: date.into(),
                series_key: vec![Identifier {
                    scheme: "occ_osi".into(),
                    value: osi.into(),
                    ..Default::default()
                }],
                ..Default::default()
            })
        };
        let quote = |osi: &str| {
            said(
                "meridian.v1.QuotesRecordedEvent",
                &QuotesRecordedEvent {
                    quote: Some(Quote {
                        meta: meta(osi, ""),
                        ..Default::default()
                    }),
                }
                .encode_to_vec(),
            )
            .unwrap()
            .key
            .unwrap()
        };
        assert_ne!(
            quote("AAPL  261120C00230000"),
            quote("AAPL  261120P00230000")
        );
        assert!(quote("AAPL  261120C00230000").ends_with("|#occ_osi=AAPL  261120C00230000"));
        let statistic = |osi: &str, date: &str, kind: i32| {
            said(
                "meridian.v1.StatisticsRecordedEvent",
                &StatisticsRecordedEvent {
                    statistic: Some(Statistic {
                        meta: meta(osi, date),
                        kind,
                        value: None,
                    }),
                }
                .encode_to_vec(),
            )
            .unwrap()
            .key
            .unwrap()
        };
        let oi = statistic("AAPL  261120C00230000", "2026-10-07", 1);
        assert_eq!(oi, statistic("AAPL  261120C00230000", "2026-10-07", 1));
        assert_ne!(oi, statistic("AAPL  261120C00230000", "2026-10-08", 1));
        assert_ne!(oi, statistic("AAPL  261120C00230000", "2026-10-07", 2));
        assert_ne!(oi, statistic("AAPL  261120P00230000", "2026-10-07", 1));
    }

    #[test]
    fn a_trade_is_never_conflated_and_a_quote_or_price_is_keyed_by_its_asset() {
        use meridian_domain::v1::{Money, Quote, Trade};
        let meta = || {
            Some(ObservationMeta {
                subjects: vec![SubjectRef {
                    entity_id: "LCL-BTC".into(),
                    ..Default::default()
                }],
                source: Some(Source {
                    dataset: "coinbase-1:live".into(),
                    venue_id: "VEN-1".into(),
                    ..Default::default()
                }),
                ..Default::default()
            })
        };
        let in_asset = |instrument: &str| Money {
            instrument_id: instrument.into(),
            ..Default::default()
        };
        let trade = TradesRecordedEvent {
            trade: Some(Trade {
                meta: meta(),
                price: Some(in_asset("LCL-USD")),
                ..Default::default()
            }),
        };
        let said_trade = said("meridian.v1.TradesRecordedEvent", &trade.encode_to_vec()).unwrap();
        assert_eq!(said_trade.key, None, "every trade delivered");
        assert_eq!(said_trade.dataset, "coinbase-1:live");
        assert_eq!(said_trade.subjects, vec!["LCL-BTC"]);
        let quote = |asset: &str| QuotesRecordedEvent {
            quote: Some(Quote {
                meta: meta(),
                bid: Some(in_asset(asset)),
                ..Default::default()
            }),
        };
        let usd = said(
            "meridian.v1.QuotesRecordedEvent",
            &quote("LCL-USD").encode_to_vec(),
        )
        .unwrap()
        .key;
        let usdc = said(
            "meridian.v1.QuotesRecordedEvent",
            &quote("LCL-USDC").encode_to_vec(),
        )
        .unwrap()
        .key;
        assert_eq!(
            usd.as_deref(),
            Some("quote|LCL-BTC|coinbase-1:live|VEN-1|LCL-USD")
        );
        assert_ne!(usd, usdc, "BTC in USD and in USDC never collapse");
        let price = |asset: &str| PricesRecordedEvent {
            price: Some(Price {
                meta: meta(),
                kind: 2,
                price: Some(in_asset(asset)),
                ..Default::default()
            }),
        };
        assert_ne!(
            said(
                "meridian.v1.PricesRecordedEvent",
                &price("LCL-USD").encode_to_vec()
            )
            .unwrap()
            .key,
            said(
                "meridian.v1.PricesRecordedEvent",
                &price("LCL-USDC").encode_to_vec()
            )
            .unwrap()
            .key
        );
    }

    #[test]
    fn a_batch_accepted_and_a_row_delivered_record_their_ages_at_their_hops() {
        // Contract v28 (W4.16; ruling 606): the sidecar's own hops, sent on
        // report-latency under its plugin's instance.
        let now = 1_792_600_000_000_000_000;
        let quote = |dataset: &str| meridian_domain::v1::Quote {
            meta: Some(ObservationMeta {
                row_key: "q-1".into(),
                source: Some(Source {
                    dataset: dataset.into(),
                    ..Default::default()
                }),
                valid_from_ns: now - 4_000_000,
                ..Default::default()
            }),
            ..Default::default()
        };
        let batch = RecordQuotesRequest {
            quotes: vec![quote("v28-age-sidecar:quotes")],
            ..Default::default()
        };
        record_ages(
            "meridian.v1.RecordQuotesRequest",
            &batch.encode_to_vec(),
            AgeHop::Sidecar,
            now,
        );
        let heard = QuotesRecordedEvent {
            quote: Some(quote("v28-age-bus:quotes")),
        };
        record_ages(
            "meridian.v1.QuotesRecordedEvent",
            &heard.encode_to_vec(),
            AgeHop::Bus,
            now,
        );
        // A daily value is no hop's.
        record_ages("meridian.v1.PricesRecordedEvent", &[], AgeHop::Bus, now);
        let taken = meridian_domain::latency::recorder().take(now);
        let at = |dataset: &str, hop: AgeHop| {
            taken
                .iter()
                .find(|s| s.dataset == dataset && s.hop == hop as i32)
                .map(|s| s.histogram.as_ref().unwrap().min_ns)
        };
        assert_eq!(
            at("v28-age-sidecar:quotes", AgeHop::Sidecar),
            Some(4_000_000)
        );
        assert_eq!(at("v28-age-bus:quotes", AgeHop::Bus), Some(4_000_000));
    }
}
