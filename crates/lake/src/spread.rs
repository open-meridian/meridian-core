//! The lake over several databases (step 3d; spec/a-deployment-scales-out-
//! and-fails-over-by-configuration section 6.4, ruled 2026-11-10 and
//! 2026-11-14): `lake.databases` databases, writer group `g` writing to
//! database `g mod count`.
//!
//! - **Writes** go to this member's own database alone, whose store refuses
//!   a dataset another group owns (it is handed on, section 6.7).
//! - **Reads** fan out to the databases holding the datasets asked for and
//!   merge here, each answer's watermark naming each partition as every
//!   answer already does. What is the whole store's -- priorities, wants,
//!   the configuration, switches, misses, aliases, the annotations' record
//!   -- is read from database 0, the store's home.
//! - **What database 0 decides about rows elsewhere** -- an alias, a
//!   closed-day re-check, a quality assessment -- is recorded and logged in
//!   database 0 as before, which is its outbox, and copied into the database
//!   holding those rows (an alias into every database), idempotently, at
//!   once and again by [`Spread::reconcile`] whenever a member starts and at
//!   each sweep: a database missing one is found, repaired and said, never
//!   silent (approved 2026-11-14).
//! - **What a database past the first records itself** -- its writer
//!   groups' takeovers, its ranges' splits, its datasets' gaps -- is in its
//!   own control journal, atomic with what it records, and relayed into
//!   database 0's by [`Spread::relay`], each once, keeping its own times:
//!   database 0's journal is the one the history and the followers read.
//!
//! With one database, the default, a lake member uses its one store as it
//! always has, and none of this is built.

use std::collections::BTreeMap;
use std::sync::Arc;

use meridian_changes::groups::Groups;
use meridian_changes::{ReadChangesReply, ReadError};
use meridian_domain::v1::{EntitlementsChangedEvent, SourcePriority};

use crate::postgres::PostgresStore;
use crate::row::{DataType, Observation};
use crate::store::{
    Assessment, ClosedDayAmendment, Dropped, Query, Recorded, Removal, Result, Served, Stale,
    Store, StoreError, WantChange, WriteAheadBatch, WriteAheadPosition,
};

/// A lake member's stores, one per database, database 0 first.
pub struct Spread {
    stores: Vec<Arc<PostgresStore>>,
    groups: Groups,
}

/// What [`Spread::reconcile`] found missing and copied, per database.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Repaired {
    pub aliases: u64,
    pub amendments: u64,
    pub assessments: u64,
}

impl Repaired {
    pub fn any(&self) -> bool {
        self.aliases + self.amendments + self.assessments > 0
    }
}

/// A page of database 0's annotations read at once by a reconcile.
const RECONCILE_PAGE: usize = 2_000;

impl Spread {
    /// The stores of every database, database 0 first, for a member of
    /// `groups`.
    pub fn new(stores: Vec<Arc<PostgresStore>>, groups: Groups) -> Self {
        assert!(!stores.is_empty(), "a lake has a database");
        Self { stores, groups }
    }

    /// How many databases.
    pub fn count(&self) -> u32 {
        self.stores.len() as u32
    }

    /// The store of database `d`.
    pub fn database(&self, d: u32) -> &Arc<PostgresStore> {
        &self.stores[d as usize]
    }

    /// This member's own database's store: where its group writes.
    pub fn own(&self) -> &Arc<PostgresStore> {
        self.database(meridian_changes::databases::of_group(
            self.groups.mine(),
            self.count(),
        ))
    }

    /// Database 0's store: the store's home.
    fn home(&self) -> &Arc<PostgresStore> {
        &self.stores[0]
    }

    /// The database holding `partition` (a dataset or one of its ranges;
    /// `control` is database 0's).
    pub fn of(&self, partition: &str) -> u32 {
        if partition == meridian_changes::CONTROL {
            return 0;
        }
        meridian_changes::databases::of_group(
            self.groups.owner(crate::range::dataset_of(partition)),
            self.count(),
        )
    }

    fn holding(&self, partition: &str) -> &Arc<PostgresStore> {
        self.database(self.of(partition))
    }

    /// Database 0's own writes: refused on a member whose group writes
    /// another database (the store's own work is group 0's).
    fn home_writes(&self) -> Result<&Arc<PostgresStore>> {
        if !Arc::ptr_eq(self.own(), self.home()) {
            return Err(StoreError::Unavailable(format!(
                "the lake's own records are database 0's, which writer group {} does not write",
                self.groups.mine()
            )));
        }
        Ok(self.home())
    }

    /// `datasets` in order, in runs of one database each: a read answering
    /// them in order reads each run from its database in turn.
    fn runs(&self, datasets: &[String]) -> Vec<(u32, Vec<String>)> {
        let mut sorted: Vec<String> = datasets.to_vec();
        sorted.sort();
        sorted.dedup();
        let mut runs: Vec<(u32, Vec<String>)> = Vec::new();
        for dataset in sorted {
            let d = self.of(&dataset);
            match runs.last_mut() {
                Some((at, run)) if *at == d => run.push(dataset),
                _ => runs.push((d, vec![dataset])),
            }
        }
        runs
    }

    /// Each database's answer, merged: maps by key.
    fn union_maps<V>(
        &self,
        each: impl Fn(&PostgresStore) -> Result<BTreeMap<String, V>>,
    ) -> Result<BTreeMap<String, V>> {
        let mut out = BTreeMap::new();
        for store in &self.stores {
            out.extend(each(store)?);
        }
        Ok(out)
    }

    /// Database 0's aliases, closed-day re-checks and assessments copied
    /// into every other database that should hold them, where missing:
    /// answers what each database was missing. Run whenever a member starts
    /// and at each sweep, and after each annotation database 0 keeps, so a
    /// crash between databases is repaired.
    pub fn reconcile(&self) -> Result<Vec<Repaired>> {
        let mut repaired = vec![Repaired::default(); self.stores.len()];
        if self.stores.len() < 2 {
            return Ok(repaired);
        }
        let aliases = self.home().alias_rows()?;
        for (d, store) in self.stores.iter().enumerate().skip(1) {
            repaired[d].aliases = store.copy_aliases(&aliases)?;
        }
        let amendments = self.home().closed_day_amendments()?;
        for chunk in amendments.chunks(RECONCILE_PAGE) {
            for (d, part) in self.by_database(chunk, |a| &a.dataset) {
                if d != 0 {
                    repaired[d as usize].amendments += self.database(d).copy_amendments(&part)?;
                }
            }
        }
        let assessments = self.home().assessments()?;
        for chunk in assessments.chunks(RECONCILE_PAGE) {
            for (d, part) in self.by_database(chunk, |a| &a.dataset) {
                if d != 0 {
                    repaired[d as usize].assessments += self.database(d).copy_assessments(&part)?;
                }
            }
        }
        Ok(repaired)
    }

    /// [`Self::reconcile`], saying what any database was missing.
    pub fn reconcile_and_say(&self) {
        match self.reconcile() {
            Ok(repaired) => {
                for (d, found) in repaired.iter().enumerate() {
                    if found.any() {
                        tracing::warn!(
                            database = d,
                            aliases = found.aliases,
                            amendments = found.amendments,
                            assessments = found.assessments,
                            "a lake database was missing what database 0 kept about its rows: copied"
                        );
                    }
                }
            }
            Err(failed) => {
                tracing::warn!(%failed, "the lake's databases were not reconciled; again at the next sweep")
            }
        }
    }

    /// Each database past the first's own control journal relayed into
    /// database 0's, each record once: answers how many were new. Group
    /// 0's, which writes database 0.
    pub fn relay(&self) -> Result<usize> {
        let home = self.home_writes()?;
        let mut relayed = 0;
        for store in self.stores.iter().skip(1) {
            relayed += home.relay_control(&store.control_entries()?)?;
        }
        Ok(relayed)
    }

    fn by_database<T: Clone>(
        &self,
        items: &[T],
        dataset: impl Fn(&T) -> &String,
    ) -> BTreeMap<u32, Vec<T>> {
        let mut out: BTreeMap<u32, Vec<T>> = BTreeMap::new();
        for item in items {
            out.entry(self.of(dataset(item)))
                .or_default()
                .push(item.clone());
        }
        out
    }

    /// A read's rows from each run's database in turn, merged in the order
    /// one database answers them: by dataset, then as each answered.
    fn read_runs(
        &self,
        query: &Query,
        each: &mut dyn FnMut(Observation) -> Result<()>,
    ) -> Result<()> {
        let runs = self.runs(&query.datasets);
        if runs.len() <= 1 {
            let d = runs.first().map_or(0, |(d, _)| *d);
            return self.database(d).read_each(query, each);
        }
        for (d, datasets) in runs {
            let mut part = query.clone();
            part.datasets = datasets;
            self.database(d).read_each(&part, each)?;
        }
        Ok(())
    }
}

impl Store for Spread {
    fn record(&self, dataset: &str, rows: Vec<Observation>, now_ns: i64) -> Result<Recorded> {
        self.own().record(dataset, rows, now_ns)
    }

    fn record_parts(
        &self,
        dataset: &str,
        parts: Vec<Vec<Observation>>,
        now_ns: i64,
    ) -> Vec<Result<Recorded>> {
        self.own().record_parts(dataset, parts, now_ns)
    }

    fn serve_unkept(
        &self,
        dataset: &str,
        rows: Vec<Observation>,
        readers: &[String],
        now_ns: i64,
    ) -> Result<Recorded> {
        self.own().serve_unkept(dataset, rows, readers, now_ns)
    }

    fn write_ahead_position(&self, stream: &str) -> Result<Option<WriteAheadPosition>> {
        self.own().write_ahead_position(stream)
    }

    fn record_write_ahead(
        &self,
        batch: WriteAheadBatch,
        now_ns: i64,
    ) -> Result<Vec<Result<Recorded>>> {
        self.own().record_write_ahead(batch, now_ns)
    }

    fn write_ahead_found(&self, stream: &str) -> Result<(Vec<Dropped>, BTreeMap<String, i64>)> {
        self.own().write_ahead_found(stream)
    }

    fn record_gap(&self, gap: &meridian_domain::v1::DatasetGap) -> Result<()> {
        self.own().record_gap(gap)
    }

    fn dataset_gaps(&self) -> Result<Vec<meridian_domain::v1::DatasetGap>> {
        // Each database's own, a gap kept once in the one its dataset is in.
        let mut out = Vec::new();
        for store in &self.stores {
            out.extend(store.dataset_gaps()?);
        }
        out.sort_by_key(|gap| gap.recorded_at_ns);
        Ok(out)
    }

    fn record_switches(&self, switches: &[meridian_domain::v1::SourceSwitched]) -> Result<()> {
        self.home_writes()?.record_switches(switches)
    }

    fn record_assignments(&self, groups: Groups, at_ns: i64) -> Result<usize> {
        // Database 0's record of every database's datasets.
        let home = self.home_writes()?;
        let mut recorded = home.record_assignments(groups, at_ns)?;
        for store in self.stores.iter().skip(1) {
            recorded += home.record_assignments_of(store, groups, at_ns)?;
        }
        Ok(recorded)
    }

    fn source_switches(&self) -> Result<Vec<meridian_domain::v1::SourceSwitched>> {
        self.home().source_switches()
    }

    fn split_partition(
        &self,
        partition: &str,
        split_by: &str,
        reason: &str,
        now_ns: i64,
    ) -> Result<meridian_domain::v1::PartitionSplit> {
        self.own()
            .split_partition(partition, split_by, reason, now_ns)
    }

    fn splits(&self) -> Result<Vec<meridian_domain::v1::PartitionSplit>> {
        // Each split once: kept in its range's database, and relayed.
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        for store in &self.stores {
            for split in store.splits()? {
                if seen.insert((split.parent.clone(), split.closing_sequence)) {
                    out.push(split);
                }
            }
        }
        out.sort_by_key(|split| split.split_at_ns);
        Ok(out)
    }

    fn read(&self, query: &Query) -> Result<Vec<Observation>> {
        let mut out = Vec::new();
        self.read_runs(query, &mut |row| {
            out.push(row);
            Ok(())
        })?;
        Ok(out)
    }

    fn read_each(
        &self,
        query: &Query,
        each: &mut dyn FnMut(Observation) -> Result<()>,
    ) -> Result<()> {
        self.read_runs(query, each)
    }

    fn rows_at(
        &self,
        data_type: DataType,
        places: &[(String, u64, u32)],
        as_of_ns: i64,
    ) -> Result<Vec<Observation>> {
        let mut by: BTreeMap<u32, Vec<(String, u64, u32)>> = BTreeMap::new();
        for place in places {
            by.entry(self.of(&place.0)).or_default().push(place.clone());
        }
        if by.len() <= 1 {
            let d = by.keys().next().copied().unwrap_or(0);
            return self.database(d).rows_at(data_type, places, as_of_ns);
        }
        // Each place's row in the order the places were named.
        let order: BTreeMap<(String, u64, u32), usize> = places
            .iter()
            .enumerate()
            .map(|(at, place)| (place.clone(), at))
            .collect();
        let mut out: Vec<(usize, Observation)> = Vec::new();
        for (d, part) in by {
            for row in self.database(d).rows_at(data_type, &part, as_of_ns)? {
                let key = (
                    row.dataset().to_string(),
                    row.meta().sequence,
                    crate::range::key_hash(&row.meta().row_key),
                );
                let at = order.get(&key).copied().unwrap_or(usize::MAX);
                out.push((at, row));
            }
        }
        out.sort_by_key(|(at, _)| *at);
        Ok(out.into_iter().map(|(_, row)| row).collect())
    }

    fn heads(&self) -> Result<BTreeMap<String, u64>> {
        self.union_maps(|store| store.heads())
    }

    fn counts(&self) -> Result<BTreeMap<String, (u64, u64)>> {
        self.union_maps(|store| store.counts())
    }

    fn unconverted_counts(&self) -> Result<BTreeMap<String, u64>> {
        self.union_maps(|store| store.unconverted_counts())
    }

    fn lowest_kept(&self, dataset: &str) -> Result<Option<u64>> {
        self.holding(dataset).lowest_kept(dataset)
    }

    fn set_priority(
        &self,
        priority: &SourcePriority,
        against_updated_at_ns: i64,
    ) -> Result<std::result::Result<SourcePriority, Stale>> {
        self.home_writes()?
            .set_priority(priority, against_updated_at_ns)
    }

    fn priorities(&self) -> Result<Vec<SourcePriority>> {
        self.home().priorities()
    }

    fn priority_changes(&self) -> Result<Vec<SourcePriority>> {
        self.home().priority_changes()
    }

    fn record_want(&self, change: &WantChange) -> Result<()> {
        self.home_writes()?.record_want(change)
    }

    fn want_changes(&self) -> Result<Vec<WantChange>> {
        self.home().want_changes()
    }

    fn served(&self) -> Result<Vec<Served>> {
        let mut out = Vec::new();
        for store in &self.stores {
            out.extend(store.served()?);
        }
        out.sort_by_key(|served| served.at_ns);
        Ok(out)
    }

    fn remove_before(
        &self,
        dataset: &str,
        recorded_before_ns: i64,
        why: &str,
        now_ns: i64,
    ) -> Result<u64> {
        self.own()
            .remove_before(dataset, recorded_before_ns, why, now_ns)
    }

    fn removals(&self) -> Result<Vec<Removal>> {
        let mut out = Vec::new();
        for store in &self.stores {
            out.extend(store.removals()?);
        }
        out.sort_by_key(|removal| removal.at_ns);
        Ok(out)
    }

    fn count_miss(&self, instance: &str, at_ns: i64) -> Result<()> {
        self.home_writes()?.count_miss(instance, at_ns)
    }

    fn miss_counts(&self) -> Result<BTreeMap<String, u64>> {
        self.home().miss_counts()
    }

    fn keep_alias(&self, replaced: &str, stays: &str, at_ns: i64) -> Result<()> {
        // Kept and logged in database 0, then in every other database,
        // where rule 5 reads it in the record's own transaction; one that
        // does not take it now is repaired by the next reconcile.
        self.home_writes()?.keep_alias(replaced, stays, at_ns)?;
        let alias = [(replaced.to_string(), stays.to_string(), at_ns)];
        for (d, store) in self.stores.iter().enumerate().skip(1) {
            if let Err(failed) = store.copy_aliases(&alias) {
                tracing::warn!(database = d, %failed, "an alias was not copied into a lake database; the next reconcile copies it");
            }
        }
        Ok(())
    }

    fn anchors(
        &self,
        dataset: &str,
        keys: &[(i16, String)],
    ) -> Result<BTreeMap<(i16, String), u16>> {
        // A key's latest is in the database of its range: each asked for
        // the keys not found yet.
        let mut found = BTreeMap::new();
        for store in &self.stores {
            let left: Vec<(i16, String)> = keys
                .iter()
                .filter(|key| !found.contains_key(*key))
                .cloned()
                .collect();
            if left.is_empty() {
                break;
            }
            found.extend(store.anchors(dataset, &left)?);
        }
        Ok(found)
    }

    fn aliases(&self) -> Result<BTreeMap<String, String>> {
        self.home().aliases()
    }

    fn keep_configuration(&self, event: &EntitlementsChangedEvent) -> Result<()> {
        self.home_writes()?.keep_configuration(event)
    }

    fn dated_range(&self, dataset: &str) -> Result<Option<(String, String)>> {
        self.holding(dataset).dated_range(dataset)
    }

    fn dated_on(
        &self,
        dataset: &str,
        dates: &[String],
        after: u64,
        most: usize,
    ) -> Result<Vec<Observation>> {
        self.holding(dataset).dated_on(dataset, dates, after, most)
    }

    fn amend_closed_day(&self, amendments: &[ClosedDayAmendment]) -> Result<()> {
        // Recorded and logged in database 0, its outbox; copied into each
        // row's own database.
        self.home_writes()?.amend_closed_day(amendments)?;
        for (d, part) in self.by_database(amendments, |a| &a.dataset) {
            if d != 0 {
                if let Err(failed) = self.database(d).copy_amendments(&part) {
                    tracing::warn!(database = d, %failed, "closed-day re-checks were not copied into a lake database; the next reconcile copies them");
                }
            }
        }
        Ok(())
    }

    fn closed_day_amendments(&self) -> Result<Vec<ClosedDayAmendment>> {
        self.home().closed_day_amendments()
    }

    fn closed_day_counts(&self) -> Result<BTreeMap<String, (u64, u64)>> {
        // Each counted where its rows are: a re-check database 0 keeps of
        // another database's row joins no row there.
        self.union_maps(|store| store.closed_day_counts())
    }

    fn quality_counts(&self) -> Result<BTreeMap<String, (u64, u64, u64)>> {
        self.union_maps(|store| store.quality_counts())
    }

    fn configuration(&self) -> Result<Option<EntitlementsChangedEvent>> {
        self.home().configuration()
    }

    fn read_dataset(
        &self,
        dataset: &str,
        data_type: DataType,
        as_of_ns: i64,
        published_as_of_ns: i64,
    ) -> Result<Vec<Observation>> {
        self.holding(dataset)
            .read_dataset(dataset, data_type, as_of_ns, published_as_of_ns)
    }

    fn read_dataset_each(
        &self,
        dataset: &str,
        data_type: DataType,
        as_of_ns: i64,
        each: &mut dyn FnMut(Observation) -> Result<()>,
    ) -> Result<()> {
        self.holding(dataset)
            .read_dataset_each(dataset, data_type, as_of_ns, each)
    }

    fn read_dataset_versions(
        &self,
        dataset: &str,
        data_type: DataType,
    ) -> Result<Vec<Observation>> {
        self.holding(dataset)
            .read_dataset_versions(dataset, data_type)
    }

    fn row_version(&self, dataset: &str, row_key: &str, version: u64) -> Result<Vec<Observation>> {
        self.holding(dataset).row_version(dataset, row_key, version)
    }

    fn assess(&self, assessments: &[Assessment]) -> Result<()> {
        self.home_writes()?.assess(assessments)?;
        for (d, part) in self.by_database(assessments, |a| &a.dataset) {
            if d != 0 {
                if let Err(failed) = self.database(d).copy_assessments(&part) {
                    tracing::warn!(database = d, %failed, "assessments were not copied into a lake database; the next reconcile copies them");
                }
            }
        }
        Ok(())
    }

    fn assessments(&self) -> Result<Vec<Assessment>> {
        self.home().assessments()
    }

    fn adopt(&self) -> Result<u64> {
        self.own().adopt()
    }

    fn epoch(&self) -> u64 {
        self.own().epoch()
    }

    fn take_over(&self, by: &meridian_changes::TookOver) -> Result<u64> {
        self.own().take_over(by)
    }

    fn head_epoch(&self) -> Result<u64> {
        self.own().head_epoch()
    }

    fn follow_timeline(&self) -> Result<meridian_changes::timeline::Timeline> {
        self.own().follow_timeline()
    }

    fn read_changes(
        &self,
        partition: &str,
        after: u64,
        most_bytes: usize,
    ) -> std::result::Result<ReadChangesReply, ReadError> {
        self.holding(partition)
            .read_changes(partition, after, most_bytes)
    }
}
