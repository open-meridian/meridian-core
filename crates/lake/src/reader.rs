//! A reader member's store (contract v30, step 3c stage B;
//! spec/a-deployment-scales-out-and-fails-over-by-configuration sections 1
//! and 5; the product owner's rulings of 2026-11-10).
//!
//! - **Latest reads** from the hot layer, which the member keeps current by
//!   following the writer's change log ([`HotStore::follow`]); a key not held
//!   is loaded from the primary, so nothing held is older than what it has
//!   applied. A read the lake cut off at its own now, as it cuts every read
//!   its caller did not ([`cut_off_its_own`]), is a latest read: only a
//!   caller's `as_of_ns` is the record's past.
//! - **The record's past** -- a read as of a time, a range, past dates,
//!   versions -- from the read replica (`<store>.database.readExistingSecret`)
//!   where one is set and has reached the read's `read_after`; from the
//!   primary otherwise.
//! - **Past its wait**, a read still behind its `read_after` is answered from
//!   the store's primary database, every part of it
//!   ([`meridian_bus::read_after::answering_from_primary`]).
//! - **Its reply's watermark** is the position it was answered at (section 5,
//!   approved 2026-11-10): the member's own applied position for an answer
//!   from its hot layer, never the primary's heads; the primary's for an
//!   answer from the primary, past its wait or not -- the record's past with
//!   no read replica reached, or a key its hot layer did not hold -- so a
//!   session's next read is never answered behind it; and never past what a
//!   read replica that answered part of it had reached.
//! - **Nothing written**: every write is refused, the writer's alone.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use meridian_bus::read_after::{
    answered_by_primary, answering_after, answering_from_primary, behind, primary_answered_part,
};
use meridian_changes::{ReadChangesReply, ReadError};
use meridian_domain::v1::{EntitlementsChangedEvent, SourcePriority};

use crate::hot::{answers_latest, HotStore};
use crate::row::{DataType, Observation};
use crate::store::{
    Assessment, ClosedDayAmendment, Listed, Query, Recorded, Removal, Result, Served, Stale, Store,
    StoreError, WantChange,
};

/// A reader member's applied position: each partition's head as far as it
/// has applied the writer's change log.
pub type Applied = Arc<dyn Fn() -> BTreeMap<String, u64> + Send + Sync>;

thread_local! {
    /// The read replica's heads where it answered part of the read this
    /// thread answers, as it was first looked at for it: the read's
    /// watermark is held to them.
    static REPLICA_ANSWERED: RefCell<Option<BTreeMap<String, u64>>> = const { RefCell::new(None) };

    /// The cut-off the lake set itself for the read this thread answers --
    /// its own now, where the caller named no `as_of_ns` -- which makes it a
    /// latest read, not the record's past.
    static OWN_CUT_OFF: Cell<Option<i64>> = const { Cell::new(None) };
}

/// A read begins on this thread: no replica or primary has answered any of
/// it yet, and the lake has cut it off nowhere.
pub fn begin() {
    REPLICA_ANSWERED.with(|held| held.borrow_mut().take());
    OWN_CUT_OFF.with(|held| held.set(None));
    crate::hot::noted::begin();
    meridian_bus::read_after::begin_read();
}

/// The lake cut the read this thread answers off at `as_of_ns`, its own now,
/// its caller having named no `as_of_ns`: a latest read, which a reader
/// answers from its hot layer (contract v30, stage B; spec section 5,
/// "latest and by-ID reads from its hot layer").
pub fn cut_off_its_own(as_of_ns: i64) {
    OWN_CUT_OFF.with(|held| held.set(Some(as_of_ns)));
}

/// Whether a read cut off at `as_of_ns` reads the latest: no cut-off, or the
/// lake's own; a caller's is the record's past.
fn latest_cut(as_of_ns: i64) -> bool {
    as_of_ns == 0 || OWN_CUT_OFF.with(Cell::get) == Some(as_of_ns)
}

/// A reader member's store: its hot layer over the primary, the primary, the
/// read replica where one is set, and the position it has applied to.
pub struct ReaderStore {
    hot: Arc<HotStore>,
    primary: Arc<dyn Store>,
    replica: Option<Arc<dyn Store>>,
    applied: Applied,
}

impl ReaderStore {
    /// `hot` is the hot layer over `primary`; `replica`, the read replica;
    /// `applied`, the member's applied position.
    pub fn new(
        hot: Arc<HotStore>,
        primary: Arc<dyn Store>,
        replica: Option<Arc<dyn Store>>,
        applied: Applied,
    ) -> Arc<Self> {
        Arc::new(Self {
            hot,
            primary,
            replica,
            applied,
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
    /// set and at or past the read's `read_after`, its heads kept for the
    /// read's watermark; the primary otherwise, which the read's watermark
    /// then says.
    fn past(&self) -> &dyn Store {
        let Some(replica) = self.replica.as_ref().filter(|_| !answering_from_primary()) else {
            return self.primary_answers();
        };
        match replica.heads() {
            Ok(heads) if reached(&heads) => {
                REPLICA_ANSWERED.with(|held| {
                    held.borrow_mut().get_or_insert(heads);
                });
                replica.as_ref()
            }
            Ok(_) => self.primary_answers(),
            Err(failed) => {
                tracing::warn!(%failed, "the read replica could not be read; the primary answers");
                self.primary_answers()
            }
        }
    }

    /// The primary, answering part of the read: its reply's watermark is the
    /// primary's.
    fn primary_answers(&self) -> &dyn Store {
        primary_answered_part();
        self.primary.as_ref()
    }
}

/// Whether the replica, at `heads`, has reached the read's `read_after`:
/// always, for a read naming none.
fn reached(heads: &BTreeMap<String, u64>) -> bool {
    let Some(asked) = answering_after() else {
        return true;
    };
    let heads: HashMap<String, u64> = heads.iter().map(|(p, s)| (p.clone(), *s)).collect();
    behind(&asked, &heads).is_empty()
}

/// Whether a read is of now -- the latest in force, recorded by now or by the
/// lake's own cut-off -- which the hot layer answers; any other, as of a time
/// its caller named included, is the record's past.
fn of_now(query: &Query) -> bool {
    answers_latest(query) && latest_cut(query.as_of_ns)
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
        if latest_cut(as_of_ns) {
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

    /// The position a read was answered at, which its reply's watermark
    /// says: the primary's past the read's wait; the primary's too where it
    /// answered any part of the read without one (the record's past with no
    /// replica reached, or a key the hot layer did not hold, read from the
    /// primary it loads from); otherwise what this member has applied. Either
    /// lowered to a read replica's where one answered part of the read.
    fn heads(&self) -> Result<BTreeMap<String, u64>> {
        if answering_from_primary() {
            return self.primary.heads();
        }
        let from_record = crate::hot::noted::peek().1 == "record";
        let mut at = if answered_by_primary() || from_record {
            self.primary.heads()?
        } else {
            (self.applied)()
        };
        REPLICA_ANSWERED.with(|held| {
            if let Some(replica) = held.borrow().as_ref() {
                for (partition, sequence) in at.iter_mut() {
                    *sequence = (*sequence).min(replica.get(partition).copied().unwrap_or(0));
                }
            }
        });
        Ok(at)
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

    /// A dataset's gaps, as the record keeps them (W10.14).
    fn dataset_gaps(&self) -> Result<Vec<meridian_domain::v1::DatasetGap>> {
        self.past().dataset_gaps()
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
        if latest_cut(as_of_ns) && published_as_of_ns == 0 {
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
        if latest_cut(as_of_ns) {
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
