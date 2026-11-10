//! A reader member's store (contract v30, step 3c stage B;
//! spec/a-deployment-scales-out-and-fails-over-by-configuration sections 1
//! and 5; the product owner's rulings of 2026-11-10).
//!
//! - **Latest reads** from the hot layer, which the member keeps current by
//!   following the writer's change log ([`HotStore::follow`]); a key not held
//!   is loaded from the primary, so nothing held is older than what it has
//!   applied.
//! - **The record's past** -- a read as of a time, a range, past dates,
//!   versions -- from the read replica (`<store>.database.readExistingSecret`)
//!   where one is set and has reached the read's `read_after`; from the
//!   primary otherwise.
//! - **Past its wait**, a read still behind its `read_after` is answered from
//!   the store's primary database, every part of it
//!   ([`meridian_bus::read_after::answering_from_primary`]).
//! - **Nothing written**: every write is refused, the writer's alone.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use meridian_bus::read_after::{answering_after, answering_from_primary, behind};
use meridian_changes::{ReadChangesReply, ReadError};
use meridian_domain::v1::{EntitlementsChangedEvent, SourcePriority};

use crate::hot::{answers_latest, HotStore};
use crate::row::{DataType, Observation};
use crate::store::{
    Assessment, ClosedDayAmendment, Listed, Query, Recorded, Removal, Result, Served, Stale, Store,
    StoreError, WantChange,
};

/// A reader member's store: its hot layer over the primary, the primary, and
/// the read replica where one is set.
pub struct ReaderStore {
    hot: Arc<HotStore>,
    primary: Arc<dyn Store>,
    replica: Option<Arc<dyn Store>>,
}

impl ReaderStore {
    /// `hot` is the hot layer over `primary`; `replica`, the read replica.
    pub fn new(
        hot: Arc<HotStore>,
        primary: Arc<dyn Store>,
        replica: Option<Arc<dyn Store>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            hot,
            primary,
            replica,
        })
    }

    /// Where a read of now is answered: the hot layer, or the primary past
    /// the read's wait.
    fn now(&self) -> &dyn Store {
        if answering_from_primary() {
            self.primary.as_ref()
        } else {
            self.hot.as_ref()
        }
    }

    /// Where a read of the record's past is answered: the read replica, where
    /// set and at or past the read's `read_after`; the primary otherwise.
    fn past(&self) -> &dyn Store {
        match &self.replica {
            Some(replica) if !answering_from_primary() && reached(replica.as_ref()) => {
                replica.as_ref()
            }
            _ => self.primary.as_ref(),
        }
    }
}

/// Whether the replica has reached the read's `read_after`: always, for a
/// read naming none.
fn reached(replica: &dyn Store) -> bool {
    let Some(asked) = answering_after() else {
        return true;
    };
    match replica.heads() {
        Ok(heads) => behind(&asked, &heads.into_iter().collect::<HashMap<_, _>>()).is_empty(),
        Err(failed) => {
            tracing::warn!(%failed, "the read replica could not be read; the primary answers");
            false
        }
    }
}

/// Whether a read is of now -- the latest in force, recorded by now -- which
/// the hot layer answers; any other, as of a time included, is the record's
/// past.
fn of_now(query: &Query) -> bool {
    answers_latest(query) && query.as_of_ns == 0
}

fn written() -> StoreError {
    StoreError::Unavailable("a reader member writes nothing: the writer does".into())
}

impl Store for ReaderStore {
    fn record(&self, _: &str, _: Vec<Observation>, _: i64) -> Result<Recorded> {
        Err(written())
    }

    fn serve_unkept(&self, _: &str, _: Vec<Observation>, _: &[String], _: i64) -> Result<Recorded> {
        Err(written())
    }

    fn read(&self, query: &Query) -> Result<Vec<Observation>> {
        if of_now(query) {
            self.now().read(query)
        } else {
            self.past().read(query)
        }
    }

    fn read_each(
        &self,
        query: &Query,
        each: &mut dyn FnMut(Observation) -> Result<()>,
    ) -> Result<()> {
        if of_now(query) {
            self.now().read_each(query, each)
        } else {
            self.past().read_each(query, each)
        }
    }

    fn rows_at(
        &self,
        data_type: DataType,
        places: &[(String, u64)],
        as_of_ns: i64,
    ) -> Result<Vec<Observation>> {
        if as_of_ns == 0 {
            self.now().rows_at(data_type, places, as_of_ns)
        } else {
            self.past().rows_at(data_type, places, as_of_ns)
        }
    }

    fn listing(
        &self,
        datasets: &[String],
        subjects: &[String],
        as_of_ns: i64,
    ) -> Result<Vec<Listed>> {
        if as_of_ns == 0 {
            self.now().listing(datasets, subjects, as_of_ns)
        } else {
            self.past().listing(datasets, subjects, as_of_ns)
        }
    }

    fn heads(&self) -> Result<BTreeMap<String, u64>> {
        self.now().heads()
    }

    fn counts(&self) -> Result<BTreeMap<String, (u64, u64)>> {
        self.now().counts()
    }

    fn unconverted_counts(&self) -> Result<BTreeMap<String, u64>> {
        self.now().unconverted_counts()
    }

    fn lowest_kept(&self, dataset: &str) -> Result<Option<u64>> {
        self.now().lowest_kept(dataset)
    }

    fn set_priority(
        &self,
        _: &SourcePriority,
        _: i64,
    ) -> Result<std::result::Result<SourcePriority, Stale>> {
        Err(written())
    }

    fn priorities(&self) -> Result<Vec<SourcePriority>> {
        self.now().priorities()
    }

    fn priority_changes(&self) -> Result<Vec<SourcePriority>> {
        self.past().priority_changes()
    }

    fn record_want(&self, _: &WantChange) -> Result<()> {
        Err(written())
    }

    fn want_changes(&self) -> Result<Vec<WantChange>> {
        self.past().want_changes()
    }

    fn served(&self) -> Result<Vec<Served>> {
        self.past().served()
    }

    fn remove_before(&self, _: &str, _: i64, _: &str, _: i64) -> Result<u64> {
        Err(written())
    }

    fn removals(&self) -> Result<Vec<Removal>> {
        self.past().removals()
    }

    fn count_miss(&self, _: &str, _: i64) -> Result<()> {
        Err(written())
    }

    fn miss_counts(&self) -> Result<BTreeMap<String, u64>> {
        self.now().miss_counts()
    }

    fn keep_alias(&self, _: &str, _: &str, _: i64) -> Result<()> {
        Err(written())
    }

    fn aliases(&self) -> Result<BTreeMap<String, String>> {
        self.now().aliases()
    }

    fn keep_configuration(&self, _: &EntitlementsChangedEvent) -> Result<()> {
        Err(written())
    }

    fn dated_range(&self, dataset: &str) -> Result<Option<(String, String)>> {
        self.past().dated_range(dataset)
    }

    fn dated_on(
        &self,
        dataset: &str,
        dates: &[String],
        after: u64,
        most: usize,
    ) -> Result<Vec<Observation>> {
        self.past().dated_on(dataset, dates, after, most)
    }

    fn amend_closed_day(&self, _: &[ClosedDayAmendment]) -> Result<()> {
        Err(written())
    }

    fn closed_day_amendments(&self) -> Result<Vec<ClosedDayAmendment>> {
        self.past().closed_day_amendments()
    }

    fn closed_day_counts(&self) -> Result<BTreeMap<String, (u64, u64)>> {
        self.now().closed_day_counts()
    }

    fn quality_counts(&self) -> Result<BTreeMap<String, (u64, u64, u64)>> {
        self.now().quality_counts()
    }

    fn configuration(&self) -> Result<Option<EntitlementsChangedEvent>> {
        self.now().configuration()
    }

    fn read_dataset(
        &self,
        dataset: &str,
        data_type: DataType,
        as_of_ns: i64,
        published_as_of_ns: i64,
    ) -> Result<Vec<Observation>> {
        if as_of_ns == 0 && published_as_of_ns == 0 {
            self.now()
                .read_dataset(dataset, data_type, as_of_ns, published_as_of_ns)
        } else {
            self.past()
                .read_dataset(dataset, data_type, as_of_ns, published_as_of_ns)
        }
    }

    fn read_dataset_each(
        &self,
        dataset: &str,
        data_type: DataType,
        as_of_ns: i64,
        each: &mut dyn FnMut(Observation) -> Result<()>,
    ) -> Result<()> {
        if as_of_ns == 0 {
            self.now()
                .read_dataset_each(dataset, data_type, as_of_ns, each)
        } else {
            self.past()
                .read_dataset_each(dataset, data_type, as_of_ns, each)
        }
    }

    fn read_dataset_versions(
        &self,
        dataset: &str,
        data_type: DataType,
    ) -> Result<Vec<Observation>> {
        self.past().read_dataset_versions(dataset, data_type)
    }

    fn row_version(&self, dataset: &str, row_key: &str, version: u64) -> Result<Vec<Observation>> {
        self.past().row_version(dataset, row_key, version)
    }

    fn assess(&self, _: &[Assessment]) -> Result<()> {
        Err(written())
    }

    fn assessments(&self) -> Result<Vec<Assessment>> {
        self.past().assessments()
    }

    fn adopt(&self) -> Result<u64> {
        Err(written())
    }

    fn epoch(&self) -> u64 {
        0
    }

    fn take_over(&self, _: &meridian_changes::TookOver) -> Result<u64> {
        Err(written())
    }

    fn head_epoch(&self) -> Result<u64> {
        self.primary.head_epoch()
    }

    fn read_changes(
        &self,
        partition: &str,
        after: u64,
        most_bytes: usize,
    ) -> std::result::Result<ReadChangesReply, ReadError> {
        self.primary.read_changes(partition, after, most_bytes)
    }
}
