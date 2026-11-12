//! A reader member's instrument store (contract v30, step 3c stage B;
//! spec/a-deployment-scales-out-and-fails-over-by-configuration sections 1
//! and 5; the product owner's rulings of 2026-11-10).
//!
//! - **Records by ID and what each became** from the hot layer, loaded from
//!   the primary and emptied by each change of the writer's change log the
//!   member follows ([`HotStore::follow`]).
//! - **A record's history** and a match as of a time from the read replica
//!   (`instrument.database.readExistingSecret`) where one is set and has
//!   reached the read's `read_after`; from the primary otherwise.
//! - **Past its wait**, a read still behind its `read_after` is answered from
//!   the primary, every part of it.
//! - **A replica's answer is held to its own snapshot** (contract v30, stage
//!   C2; the resilience spec's section 5): the record's past read on the
//!   replica in one repeatable-read snapshot with its change log's head rows,
//!   and taken where those have reached the read's `read_after`; the primary
//!   answers where they have not. No reference reply carries a watermark of
//!   its own, so the snapshot is what the answer is held to.
//! - **Nothing written**: every write is the writer's.

use std::sync::Arc;

use meridian_bus::read_after::{answering_after, answering_from_primary, behind, Position};

use crate::hot::HotStore;
use crate::store::*;

/// A reader member's store: its hot layer over the primary, the primary, and
/// the read replica (with its change log's position) where one is set.
pub struct ReaderStore {
    hot: Arc<HotStore>,
    primary: Arc<dyn Store>,
    replica: Option<(Arc<dyn Store>, Arc<dyn Position>)>,
}

impl ReaderStore {
    pub fn new(
        hot: Arc<HotStore>,
        primary: Arc<dyn Store>,
        replica: Option<(Arc<dyn Store>, Arc<dyn Position>)>,
    ) -> Self {
        Self {
            hot,
            primary,
            replica,
        }
    }

    fn now(&self) -> &dyn Store {
        if answering_from_primary() {
            self.primary.as_ref()
        } else {
            self.hot.as_ref()
        }
    }

    /// A read of the record's past (`read` asked of a store): from the read
    /// replica, where set, in one snapshot whose head rows have reached the
    /// read's `read_after`; from the primary otherwise (contract v30, stage
    /// C2).
    fn past<T>(&self, read: impl Fn(&dyn Store) -> Result<T>) -> Result<T> {
        if let Some((replica, at)) = self.replica.as_ref().filter(|_| !answering_from_primary()) {
            crate::postgres::stamp_from_snapshot();
            let answered = read(replica.as_ref());
            let snapshot = crate::postgres::snapshot_heads();
            match answered {
                Ok(answer) => {
                    // A store that keeps no snapshot -- in memory, as a
                    // test's -- says its position after the read.
                    let heads = match snapshot {
                        Some(heads) => Ok(heads),
                        None => at.heads(&asked_partitions()),
                    };
                    match heads {
                        Ok(heads) if reached(&heads) => return Ok(answer),
                        Ok(_) => {}
                        Err(failed) => {
                            tracing::warn!(%failed, "the read replica's heads could not be read; the primary answers")
                        }
                    }
                }
                Err(failed) => {
                    tracing::warn!(%failed, "the read replica could not be read; the primary answers")
                }
            }
        }
        read(self.primary.as_ref())
    }
}

fn asked_partitions() -> Vec<String> {
    answering_after()
        .map(|asked| {
            asked
                .partitions
                .iter()
                .map(|p| p.partition.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a replica's snapshot at `heads` has reached the read's
/// `read_after`.
fn reached(heads: &std::collections::HashMap<String, u64>) -> bool {
    answering_after().is_none_or(|asked| behind(&asked, heads).is_empty())
}

fn written() -> StoreError {
    StoreError::Unavailable("a reader member writes nothing: the writer does".into())
}

impl Store for ReaderStore {
    fn by_id(&self, instrument_id: &str) -> Result<Option<Instrument>> {
        self.now().by_id(instrument_id)
    }

    fn windows(
        &self,
        scheme: &str,
        value: &str,
        source: &str,
    ) -> Result<Vec<(String, Identifier)>> {
        self.now().windows(scheme, value, source)
    }

    fn matching(
        &self,
        scheme: &str,
        value: &str,
        source: &str,
        as_of_ns: i64,
    ) -> Result<Vec<Instrument>> {
        if as_of_ns == 0 {
            self.now().matching(scheme, value, source, as_of_ns)
        } else {
            self.past(|store| store.matching(scheme, value, source, as_of_ns))
        }
    }

    fn all(&self) -> Result<Vec<Instrument>> {
        self.now().all()
    }

    fn each(&self, every: &mut dyn FnMut(Instrument) -> Result<bool>) -> Result<()> {
        self.now().each(every)
    }

    fn count(&self) -> Result<usize> {
        self.now().count()
    }

    fn mint(&self, _: Instrument, _: &str, _: Version) -> Result<(Instrument, Stood)> {
        Err(written())
    }

    fn write(&self, _: Instrument, _: i64, _: Version) -> Result<Written> {
        Err(written())
    }

    fn history(&self, instrument_id: &str) -> Result<Vec<Version>> {
        self.past(|store| store.history(instrument_id))
    }

    fn note_conflict(&self, _: Conflict) -> Result<()> {
        Err(written())
    }

    fn conflicts(&self) -> Result<Vec<Conflict>> {
        self.now().conflicts()
    }

    fn replace(&self, _: &str, _: &str, _: i64) -> Result<Replaced> {
        Err(written())
    }

    fn replace_because(&self, _: &str, _: &str, _: i32, _: i64) -> Result<Replaced> {
        Err(written())
    }

    fn replacement_record(&self, replaced_id: &str) -> Result<Option<ReplacementKept>> {
        self.now().replacement_record(replaced_id)
    }

    fn replacements_after(&self, sequence: i64, limit: usize) -> Result<Vec<ReplacementKept>> {
        self.now().replacements_after(sequence, limit)
    }

    fn keep_template(&self, _: &meridian_domain::v1::SecurityTemplate, _: i64) -> Result<bool> {
        Err(written())
    }

    fn templates(&self) -> Result<Vec<meridian_domain::v1::SecurityTemplate>> {
        self.now().templates()
    }

    fn keep_licence_list(&self, _: &meridian_domain::v1::LicenceList, _: i64) -> Result<bool> {
        Err(written())
    }

    fn licence_list(&self) -> Result<Option<meridian_domain::v1::LicenceList>> {
        self.now().licence_list()
    }

    fn state(&self, key: &str) -> Result<Option<String>> {
        self.now().state(key)
    }

    fn set_state(&self, _: &str, _: &str) -> Result<()> {
        Err(written())
    }

    fn replacement_of(&self, instrument_id: &str) -> Result<Option<String>> {
        self.now().replacement_of(instrument_id)
    }

    fn keep_venue(&self, _: &meridian_domain::v1::VenueRecord, _: i64) -> Result<bool> {
        Err(written())
    }

    fn venues(&self) -> Result<Vec<meridian_domain::v1::VenueRecord>> {
        self.now().venues()
    }

    fn keep_calendar(&self, _: &meridian_domain::v1::CalendarRecord, _: i64) -> Result<bool> {
        Err(written())
    }

    fn keep_date_rule(&self, _: &meridian_domain::v1::DateRuleVersion, _: i64) -> Result<bool> {
        Err(written())
    }

    fn calendar_versions(&self) -> Result<Vec<KeptCalendar>> {
        self.now().calendar_versions()
    }

    fn date_rule_versions(&self) -> Result<Vec<KeptDateRule>> {
        self.now().date_rule_versions()
    }

    fn keep_event_group(
        &self,
        _: &meridian_domain::v1::EventGroup,
        _: i64,
    ) -> Result<Option<meridian_domain::v1::EventGroup>> {
        Err(written())
    }

    fn event_groups(&self) -> Result<Vec<meridian_domain::v1::EventGroup>> {
        self.now().event_groups()
    }

    fn keep_company(&self, _: &meridian_domain::v1::CompanyRecord, _: i64) -> Result<bool> {
        Err(written())
    }

    fn company(&self, company_id: &str) -> Result<Option<meridian_domain::v1::CompanyRecord>> {
        self.now().company(company_id)
    }

    fn companies_with(
        &self,
        scheme: &str,
        value: &str,
    ) -> Result<Vec<meridian_domain::v1::CompanyRecord>> {
        self.now().companies_with(scheme, value)
    }
}
