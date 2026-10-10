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
//! **A business date is a date** (ruling 3 of 2026-10-09): a batch or a read
//! naming one that is not a day that exists is refused here, naming its
//! field, before anything is sent.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use meridian_bus::Bus;
use meridian_domain::v1::{
    BarsRecordedEvent, EntitlementsChangedEvent, ListBarsRequest, ListPricesRequest,
    ObservationMeta, ObservationsWantedEvent, PricesRecordedEvent, RecordBarsRequest,
    RecordPricesRequest, WantWithdrawnEvent,
};
use prost::Message;
use tokio::sync::watch;

pub const ENTITLEMENTS_CHANGED: &str = "platform.config.event.entitlements-changed";

/// The segment a lake row's topic names its dataset by.
pub const DATASET: &str = "{dataset}";

/// The plugin's entitlements, as the conductor last published them: by
/// dataset, the fields it may read, an empty set for every field. None
/// until the configuration is first heard, when it reads nothing.
#[derive(Clone)]
pub struct Entitled {
    instance: String,
    held: Arc<RwLock<Option<BTreeMap<String, BTreeSet<String>>>>>,
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
        let mut heard = bus.subscribe(ENTITLEMENTS_CHANGED);
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
    /// a price per subject, dataset, kind and venue; a bar per subject,
    /// dataset, venue and interval start; a want per want.
    pub key: String,
    pub is_want: bool,
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

/// A lake row's payload read for what the receive decides by; None for a
/// payload of no lake row.
pub(crate) fn said(payload_type: &str, payload: &[u8]) -> Option<Said> {
    match payload_type {
        "meridian.v1.PricesRecordedEvent" => {
            let price = PricesRecordedEvent::decode(payload).ok()?.price?;
            let (dataset, subjects, venue) = meta_said(price.meta.as_ref());
            Some(Said {
                key: format!(
                    "price|{}|{dataset}|{}|{venue}",
                    subjects.first().cloned().unwrap_or_default(),
                    price.kind
                ),
                dataset,
                subjects,
                is_want: false,
            })
        }
        "meridian.v1.BarsRecordedEvent" => {
            let bar = BarsRecordedEvent::decode(payload).ok()?.bar?;
            let from = bar.meta.as_ref().map_or(0, |m| m.valid_from_ns);
            let (dataset, subjects, venue) = meta_said(bar.meta.as_ref());
            Some(Said {
                key: format!(
                    "bar|{}|{dataset}|{venue}|{from}",
                    subjects.first().cloned().unwrap_or_default()
                ),
                dataset,
                subjects,
                is_want: false,
            })
        }
        "meridian.v1.ObservationsWantedEvent" => {
            let want = ObservationsWantedEvent::decode(payload).ok()?;
            Some(Said {
                key: format!("want|{}", want.want_id),
                dataset: want.dataset,
                subjects: Vec::new(),
                is_want: true,
            })
        }
        "meridian.v1.WantWithdrawnEvent" => {
            let withdrawn = WantWithdrawnEvent::decode(payload).ok()?;
            Some(Said {
                key: format!("want|{}", withdrawn.want_id),
                dataset: withdrawn.dataset,
                subjects: Vec::new(),
                is_want: true,
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
        assert_eq!(said.key, "price|LCL-BTC|coinbase-1:daily|1|");
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
}
