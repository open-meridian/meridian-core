//! The deployment's data configuration as the lake applies it (W10.1): every
//! launched dataset with its catalogue entry, each dataset's licence and each
//! entitlement, as the conductor last published them whole. Kept in the
//! lake's store as heard, so a lake that restarts applies the last it heard
//! until the conductor publishes again.

use std::collections::{BTreeMap, BTreeSet};

use meridian_domain::lake;
use meridian_domain::v1::{DatasetEntitlement, DatasetRef, EntitlementsChangedEvent};
use meridian_pb::v1::{DatasetDeclaration, DatasetLicence, ObservationMode};

use crate::row::DataType;

/// Whose read a query is: a plugin's, by its instance, answered within its
/// entitlements; or a core component's -- the dashboard's -- answered whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reader {
    Plugin(String),
    Core,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DataConfig {
    pub datasets: BTreeMap<String, DatasetRef>,
    pub licences: BTreeMap<String, DatasetLicence>,
    pub entitlements: BTreeMap<(String, String), DatasetEntitlement>,
    pub changed_at_ns: i64,
}

impl DataConfig {
    pub fn from_event(event: &EntitlementsChangedEvent) -> DataConfig {
        DataConfig {
            datasets: event
                .datasets
                .iter()
                .map(|d| (d.dataset.clone(), d.clone()))
                .collect(),
            licences: event
                .licences
                .iter()
                .map(|l| (l.dataset.clone(), l.clone()))
                .collect(),
            entitlements: event
                .entitlements
                .iter()
                .map(|e| ((e.dataset.clone(), e.instance.clone()), e.clone()))
                .collect(),
            changed_at_ns: event.changed_at_ns,
        }
    }

    pub fn declaration(&self, dataset: &str) -> Option<&DatasetDeclaration> {
        self.datasets
            .get(dataset)
            .and_then(|d| d.declaration.as_ref())
    }

    /// The licence enforced: the deployment's, or the catalogue's default
    /// until one is set (spec/the-lake, Q8); a dataset declaring none is
    /// kept, with no retention set.
    pub fn licence(&self, dataset: &str) -> DatasetLicence {
        if let Some(licence) = self.licences.get(dataset) {
            return licence.clone();
        }
        self.declaration(dataset)
            .and_then(|d| d.licence_default.clone())
            .map(|terms| DatasetLicence {
                dataset: dataset.to_string(),
                ..terms
            })
            .unwrap_or(DatasetLicence {
                dataset: dataset.to_string(),
                kept: true,
                derived_use: true,
                display: true,
                ..Default::default()
            })
    }

    /// Whether a dataset serves a data type.
    pub fn serves(&self, dataset: &str, data_type: DataType) -> bool {
        self.declaration(dataset)
            .is_some_and(|d| d.data_types.iter().any(|t| t == data_type.name()))
    }

    pub fn has_mode(&self, dataset: &str, mode: ObservationMode) -> bool {
        self.declaration(dataset)
            .is_some_and(|d| d.modes.contains(&(mode as i32)))
    }

    /// The fields a reader may read of a dataset: None when it may read
    /// none of it; an empty set for every field. Its entitlement's, or, where
    /// that names none, the licence's default fields.
    pub fn fields_for(&self, dataset: &str, reader: &Reader) -> Option<BTreeSet<String>> {
        let Reader::Plugin(instance) = reader else {
            return Some(BTreeSet::new());
        };
        let entitlement = self
            .entitlements
            .get(&(dataset.to_string(), instance.clone()))
            .filter(|e| e.allowed)?;
        if !self.datasets.contains_key(dataset) {
            return None;
        }
        let fields: BTreeSet<String> = if entitlement.fields.is_empty() {
            self.licence(dataset).default_fields.into_iter().collect()
        } else {
            entitlement.fields.iter().cloned().collect()
        };
        Some(fields)
    }

    /// The datasets a reader may read that serve a data type, by ID.
    pub fn readable(&self, reader: &Reader, data_type: DataType) -> Vec<String> {
        self.datasets
            .keys()
            .filter(|d| self.serves(d, data_type) && self.fields_for(d, reader).is_some())
            .cloned()
            .collect()
    }

    /// The instances entitled to a dataset: who hears its rows.
    pub fn entitled(&self, dataset: &str) -> Vec<String> {
        self.entitlements
            .values()
            .filter(|e| e.dataset == dataset && e.allowed)
            .map(|e| e.instance.clone())
            .collect()
    }

    /// Whether a dataset is one this instance's catalogue declares.
    pub fn declared_by(&self, dataset: &str, instance: &str) -> bool {
        self.datasets
            .get(dataset)
            .is_some_and(|d| d.instance == instance)
            && lake::instance_of(dataset) == Some(instance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> DataConfig {
        DataConfig::from_event(&EntitlementsChangedEvent {
            datasets: vec![DatasetRef {
                dataset: "coinbase-1:daily".into(),
                instance: "coinbase-1".into(),
                vendor: "Coinbase".into(),
                declaration: Some(DatasetDeclaration {
                    key: "daily".into(),
                    data_types: vec![lake::PRICE.into(), lake::BAR.into()],
                    modes: vec![ObservationMode::Pull as i32],
                    licence_default: Some(DatasetLicence {
                        kept: false,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
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
                    allowed: false,
                    ..Default::default()
                },
            ],
            ..Default::default()
        })
    }

    #[test]
    fn a_reader_reads_what_it_is_entitled_to_and_the_catalogues_terms_apply_until_licensed() {
        let config = config();
        let one = Reader::Plugin("reporting-1".into());
        assert_eq!(
            config.readable(&one, DataType::Bar),
            vec!["coinbase-1:daily"]
        );
        assert!(config
            .fields_for("coinbase-1:daily", &one)
            .unwrap()
            .contains("meridian.v1.Bar.close"));
        assert!(config
            .fields_for("coinbase-1:daily", &Reader::Plugin("reporting-2".into()))
            .is_none());
        assert!(config
            .fields_for("coinbase-1:daily", &Reader::Core)
            .unwrap()
            .is_empty());
        assert!(
            !config.licence("coinbase-1:daily").kept,
            "the catalogue's default"
        );
        assert!(config.declared_by("coinbase-1:daily", "coinbase-1"));
        assert!(!config.declared_by("coinbase-1:daily", "kraken-1"));
        assert_eq!(config.entitled("coinbase-1:daily"), vec!["reporting-1"]);
    }
}
