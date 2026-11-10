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

    fn past(&self) -> &dyn Store {
        match &self.replica {
            Some((replica, at)) if !answering_from_primary() && reached(at.as_ref()) => {
                replica.as_ref()
            }
            _ => self.primary.as_ref(),
        }
    }
}

fn reached(at: &dyn Position) -> bool {
    let Some(asked) = answering_after() else {
        return true;
    };
    let partitions: Vec<String> = asked
        .partitions
        .iter()
        .map(|p| p.partition.clone())
        .collect();
    match at.heads(&partitions) {
        Ok(heads) => behind(&asked, &heads).is_empty(),
        Err(failed) => {
            tracing::warn!(%failed, "the read replica could not be read; the primary answers");
            false
        }
    }
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
            self.past().matching(scheme, value, source, as_of_ns)
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
        self.past().history(instrument_id)
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
