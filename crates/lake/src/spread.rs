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

use crate::postgres::{PostgresStore, Standing};
use crate::row::{DataType, Observation};
use crate::store::{shard_at, ShardStamp};
use crate::store::{
    Assessment, ClosedDayAmendment, Dropped, Query, Recorded, Removal, Result, Served, Stale,
    Store, StoreError, WantChange, WriteAheadBatch, WriteAheadPosition,
};

/// A lake member's stores, one per database, database 0 first.
pub struct Spread {
    stores: Vec<Arc<PostgresStore>>,
    groups: Groups,
    /// Each range found at its owner's home database, by owner: written and
    /// read there from then on, as only a move takes it away -- to another
    /// group, so another owner (step 3d; ruled 2026-11-14).
    settled: std::sync::Mutex<std::collections::BTreeSet<(String, u32)>>,
}

/// What one read of a dataset goes by ([`Spread::follow_owners`]): the
/// databases it asks, and each range a database among those it read had
/// let go to another's.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Followed {
    databases: std::collections::BTreeSet<u32>,
    let_go: std::collections::BTreeSet<(u32, String)>,
}

/// How many times a write's rows are routed again to the database their
/// range's head is in: once moved home, it is there.
const ROUTED_AGAIN: usize = 3;

/// `more`, another database's part of the same write, added to `into`.
fn merged(dataset: &str, into: &mut Recorded, more: Recorded) {
    if !into.partitions.is_empty() || !more.partitions.is_empty() {
        into.partitions.resize(into.rows.len(), dataset.to_string());
        let mut named = more.partitions;
        named.resize(more.rows.len(), dataset.to_string());
        into.partitions.extend(named);
    }
    into.unchanged_in.extend(more.unchanged_in);
    into.rows.extend(more.rows);
    into.recorded += more.recorded;
    into.restated += more.restated;
    into.unchanged += more.unchanged;
    into.head = into.head.max(more.head);
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
        for store in &stores {
            store.link_databases(&stores);
        }
        Self {
            stores,
            groups,
            settled: Default::default(),
        }
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

    /// The databases past this member's own where its group holds a range
    /// further on than at home: the database count was raised and they are
    /// not moved home yet (step 3d; ruled 2026-11-14). Its group's epoch is
    /// adopted, or taken over, in each, as at home.
    fn ahead_away(&self) -> Result<Vec<&Arc<PostgresStore>>> {
        let own = self.own();
        let mut away = Vec::new();
        for store in &self.stores {
            if Arc::ptr_eq(store, own) {
                continue;
            }
            let mut ahead = false;
            for (range, head) in store.open_ranges_of(self.groups.mine())? {
                let home = own.range_tip(&range)?.map_or(0, |tip| tip.head);
                if head > home {
                    ahead = true;
                    break;
                }
            }
            if ahead {
                away.push(store);
            }
        }
        Ok(away)
    }

    /// The furthest position writer group `group` keeps under `key` in any
    /// database, never one beside a move's pointer naming the group (the
    /// review of item 191's finding 4): a range's switch between databases
    /// keeps there, under the target's own key, the place its target takes
    /// the range's rows from -- the source's, which the target has not
    /// committed to. Taken for the group's own, a fresh target skipped its
    /// own ranges' writes up to it with no gap.
    fn furthest_position(&self, key: &str, group: u32) -> Result<Option<WriteAheadPosition>> {
        let mut furthest: Option<WriteAheadPosition> = None;
        for store in &self.stores {
            if let Some(at) = store.write_ahead_position(key)? {
                if store.points_to(group)? {
                    continue;
                }
                if furthest.is_none_or(|held| at.sequence > held.sequence) {
                    furthest = Some(at);
                }
            }
        }
        Ok(furthest)
    }

    /// `partition` routed again by its head from now on: a write found it
    /// moved home.
    fn route_again(&self, partition: &str) {
        self.settled
            .lock()
            .expect("settled poisoned")
            .retain(|(held, _)| held != partition);
    }

    /// The database each of `rows` of `dataset` is written in: its range's
    /// head's, for a range this member's group writes; this member's own
    /// for another group's, which its store hands on. One for every row
    /// where the dataset's ranges are in one database.
    fn write_dbs(&self, dataset: &str, rows: &[Observation]) -> Vec<u32> {
        let mine = self.groups.mine();
        let own = meridian_changes::databases::of_group(mine, self.count());
        let leaves = crate::range::leaves(&self.groups.ranges_of(dataset));
        let ranges: Vec<String> = if leaves.is_empty() {
            vec![dataset.to_string()]
        } else {
            leaves
                .into_iter()
                .map(|(range, _)| range.partition)
                .collect()
        };
        let dbs: BTreeMap<String, u32> = ranges
            .into_iter()
            .map(|range| {
                let d = if self.groups.owner_by(&range, dataset) == mine {
                    self.of(&range)
                } else {
                    own
                };
                (range, d)
            })
            .collect();
        let distinct: std::collections::BTreeSet<u32> = dbs.values().copied().collect();
        if distinct.len() <= 1 {
            return vec![distinct.first().copied().unwrap_or(own); rows.len()];
        }
        crate::service::ranges_of_rows(self.groups, self, dataset, rows)
            .into_iter()
            .map(|range| dbs.get(&range).copied().unwrap_or(own))
            .collect()
    }

    /// `parts` of `dataset` written where each row's range's head is, by
    /// `write` on each database's store: each part's answer, its rows'
    /// parts in several databases merged. A part found moved home is
    /// routed again (the same-group fence), never refused for it.
    ///
    /// Each part's plugin shard stamp, by its place in `parts`, goes with
    /// every database's piece of it (step 3d, checkpoint 5, item 32): each
    /// database checks it against its own `lake_shard_fence`.
    fn routed(
        &self,
        dataset: &str,
        parts: Vec<Vec<Observation>>,
        shards: &[Option<ShardStamp>],
        write: impl Fn(
            &PostgresStore,
            Vec<Vec<Observation>>,
            &[Option<ShardStamp>],
        ) -> Vec<Result<Recorded>>,
    ) -> Vec<Result<Recorded>> {
        let mine = self.groups.mine();
        let mut answers: Vec<Option<Result<Recorded>>> = (0..parts.len()).map(|_| None).collect();
        let mut pending: Vec<(usize, Vec<Observation>)> = parts.into_iter().enumerate().collect();
        for round in 0..=ROUTED_AGAIN {
            if pending.is_empty() {
                break;
            }
            let mut by_db: BTreeMap<u32, Vec<(usize, Vec<Observation>)>> = BTreeMap::new();
            for (at, rows) in pending.drain(..) {
                let dbs = self.write_dbs(dataset, &rows);
                let mut split: BTreeMap<u32, Vec<Observation>> = BTreeMap::new();
                for (row, d) in rows.into_iter().zip(dbs) {
                    split.entry(d).or_default().push(row);
                }
                for (d, rows) in split {
                    by_db.entry(d).or_default().push((at, rows));
                }
            }
            for (d, subs) in by_db {
                let (places, rows): (Vec<usize>, Vec<Vec<Observation>>) = subs.into_iter().unzip();
                let kept = rows.clone();
                let stamps: Vec<Option<ShardStamp>> = places
                    .iter()
                    .map(|at| shard_at(shards, *at).cloned())
                    .collect();
                let done = write(self.database(d), rows, &stamps);
                for ((at, result), rows) in places.into_iter().zip(done).zip(kept) {
                    match result {
                        Err(StoreError::NotOwned { partition, group })
                            if group == mine && round < ROUTED_AGAIN =>
                        {
                            self.route_again(&partition);
                            pending.push((at, rows));
                        }
                        Ok(recorded) => match &mut answers[at] {
                            Some(Ok(into)) => merged(dataset, into, recorded),
                            Some(Err(_)) => {}
                            none => *none = Some(Ok(recorded)),
                        },
                        Err(failed) => {
                            if !matches!(answers[at], Some(Err(_))) {
                                answers[at] = Some(Err(failed));
                            }
                        }
                    }
                }
            }
        }
        answers
            .into_iter()
            .map(|answer| answer.unwrap_or_else(|| Ok(Recorded::default())))
            .collect()
    }

    /// Database 0's store: the store's home.
    fn home(&self) -> &Arc<PostgresStore> {
        &self.stores[0]
    }

    /// The database holding `partition`'s head (a dataset or one of its
    /// ranges; `control` is database 0's; step 3d, ruled 2026-11-14): among
    /// the databases whose open row of it names its owner, the one furthest
    /// on, its owner's home where two stand level -- a move home switched
    /// -- or none has begun it. Once there, it is there until its owner
    /// changes.
    pub fn of(&self, partition: &str) -> u32 {
        if partition == meridian_changes::CONTROL {
            return 0;
        }
        let dataset = crate::range::dataset_of(partition);
        let owner = self.groups.owner_by(partition, dataset);
        let home = meridian_changes::databases::of_group(owner, self.count());
        let key = (partition.to_string(), owner);
        if self.stores.len() == 1
            || self
                .settled
                .lock()
                .expect("settled poisoned")
                .contains(&key)
        {
            return home;
        }
        let count = self.groups.count();
        let mut best = (0u64, home);
        for (d, store) in self.stores.iter().enumerate() {
            let Ok(Some(tip)) = store.range_tip(partition) else {
                continue;
            };
            // A move's pointer is where it let go, never where it is
            // written (the review's C3).
            if tip.epoch == meridian_changes::moves::POINTER {
                continue;
            }
            let named = tip
                .writer_group
                .filter(|group| *group < count)
                .unwrap_or_else(|| meridian_changes::groups::owner(dataset, count));
            if named == owner && !tip.closed && tip.head > best.0 {
                best = (tip.head, d as u32);
            }
        }
        if best.1 != home
            && self
                .database(home)
                .range_tip(partition)
                .ok()
                .flatten()
                .map(|tip| tip.head)
                == Some(best.0)
        {
            best.1 = home;
        }
        if best.1 == home {
            self.settled.lock().expect("settled poisoned").insert(key);
        }
        best.1
    }

    /// The databases a read of `dataset` asks first: its owner's home and
    /// each of its ranges' owners', by this member's view. Named, never
    /// looked for: where a partition stands in every database
    /// ([`Self::of`]) is a write's to find, on each pool's own wait, and a
    /// read's own look says more ([`Self::follow_owners`]; the review of
    /// 2026-11-23 of this routing, findings 1 and 2).
    fn homes(&self, dataset: &str) -> std::collections::BTreeSet<u32> {
        let home = |owner| meridian_changes::databases::of_group(owner, self.count());
        let mut out =
            std::collections::BTreeSet::from([home(self.groups.owner_by(dataset, dataset))]);
        for (_, owner) in self.groups.ranges_of(dataset) {
            out.insert(home(owner));
        }
        out
    }

    /// The databases one read of `dataset` asks, by the rows of it just
    /// read, where this member's view of its ranges' owners was read
    /// before a move (step 3d; sections 5 and 6.4; the book's stale read of
    /// merge-queue run 45104369464, the same shape here). A member reads
    /// the writer groups at its start, every few seconds and on a write
    /// routed again, and a reader writes nothing: until its next look, a
    /// read went by the view to a range's source, which answers nothing of
    /// a range it let go, while the read's `read_after` had passed on the
    /// target's head. Each database answers the ranges its own rows say
    /// are read there, so a read needs only the right databases.
    ///
    /// - **Its owners' homes asked first** ([`Self::homes`]), one read of
    ///   each, on the pool's own wait as the read's rows are: theirs where
    ///   they have begun the dataset and each range one of them let go is
    ///   held by another of them, at or past where it was let go.
    /// - **Every other's on that wait too** where the homes have begun
    ///   nothing of the dataset: held away from home -- the database count
    ///   raised, the dataset not moved home yet -- it is read where it is
    ///   held, a database the read needs as it needs its home (the review
    ///   of 2026-11-23 of this routing, finding 1).
    /// - **A dataset no database has begun** is its home's to answer,
    ///   empty: asked of every other once, none holding a row of it, the
    ///   member keeps that as a write's routing does ([`Self::of`]), and
    ///   asks its home alone until its home begins it or its owner
    ///   changes. Main answered so once it had looked (the same review,
    ///   finding 6).
    /// - **Every other's for 1 s at most** only where a home holds a
    ///   move's pointer of a range none of them holds: each holding a
    ///   range of the dataset is asked as well. A range a split closed
    ///   sends no read on: it moves nowhere, and where it was split in the
    ///   database it moved to, its closed row there is what its source's
    ///   pointer is held by.
    /// - **Refused UNAVAILABLE** where such a database cannot be asked:
    ///   never answered without a range a move took there or a dataset
    ///   held there, nor held for a pool's 30 s by the outage of a
    ///   database only a pointer names (the review of 2026-11-23, finding
    ///   4).
    /// - **This read's alone**: the view is never written here (the same
    ///   review, finding 3). A reader's replicas share it with its own
    ///   stores, and one behind a move would write the owner before it.
    ///
    /// INTERIM(v32): a stop-gap for a view of the owners read on a timer;
    /// gone when v32's ownership record says where a range is read, and a
    /// read with `read_after` is never answered without a range a move
    /// took elsewhere.
    fn follow_owners(&self, dataset: &str) -> Result<Followed> {
        let mut databases = self.homes(dataset);
        if self.stores.len() == 1 || dataset == meridian_changes::CONTROL {
            return Ok(Followed {
                databases,
                let_go: Default::default(),
            });
        }
        let count = self.groups.count();
        // Whether `row`, in database `d`, is of a range read there: written
        // there, or a move's pointer to a group whose home it is (its
        // target, not opened yet).
        let here = |d: u32, row: &Standing| {
            let group = row
                .group
                .filter(|group| *group < count)
                .unwrap_or_else(|| meridian_changes::groups::owner(dataset, count));
            row.written || meridian_changes::databases::of_group(group, self.count()) == d
        };
        let mut rows: Vec<(u32, Standing)> = Vec::new();
        for d in &databases {
            for row in self.database(*d).standing_of(dataset, false)? {
                rows.push((*d, row));
            }
        }
        // Each range's row furthest on among those read where they are: a
        // written row a failed move left behind a pointer stands for
        // nothing (the review of the book's fix, finding 1). One a split
        // closed where it moved to stands for it as an open one does, at
        // or past its source's pointer (the review of 2026-11-23 of this
        // routing, finding 4).
        let mut held: BTreeMap<&str, u64> = BTreeMap::new();
        for (_, row) in rows.iter().filter(|(d, row)| here(*d, row)) {
            let at = held.entry(row.partition.as_str()).or_insert(row.head);
            *at = (*at).max(row.head);
        }
        let begun = rows.iter().any(|(d, row)| row.head > 0 && here(*d, row));
        let astray = rows.iter().any(|(d, row)| {
            !row.closed
                && !here(*d, row)
                && held
                    .get(row.partition.as_str())
                    .is_none_or(|at| *at < row.head)
        });
        // Begun nowhere when every database was last asked: its home's
        // answer is the read's, as a write's routing has it (`of`).
        let key = (dataset.to_string(), self.groups.owner_by(dataset, dataset));
        let nowhere = rows.is_empty()
            && self
                .settled
                .lock()
                .expect("settled poisoned")
                .contains(&key);
        if (!begun && !nowhere) || astray {
            let others: Vec<u32> = (0..self.count())
                .filter(|d| !databases.contains(d))
                .collect();
            for d in others {
                // On the short wait only where a pointer sent the read on.
                let asked = self.database(d).standing_of(dataset, astray).map_err(|failed| {
                    StoreError::Unavailable(format!(
                        "dataset {dataset}'s ranges in database {d}, which may hold one, could not be read ({failed}): ask again"
                    ))
                })?;
                if asked.iter().any(|row| !row.closed && here(d, row)) {
                    databases.insert(d);
                }
                rows.extend(asked.into_iter().map(|row| (d, row)));
            }
        }
        if rows.is_empty() {
            self.settled.lock().expect("settled poisoned").insert(key);
        }
        let let_go = rows
            .iter()
            .filter(|(d, row)| !row.closed && !here(*d, row))
            .map(|(d, row)| (*d, row.partition.clone()))
            .collect();
        Ok(Followed { databases, let_go })
    }

    /// `read` of `datasets`, by what each is followed to
    /// ([`Self::follow_owners`]), refused UNAVAILABLE where a range of one
    /// moved between databases under it (the review of 2026-11-23, finding
    /// 1): each database answers from its own snapshot the ranges its rows
    /// say are read there, so a range let go between this read's routing
    /// and a database's snapshot was answered by neither -- its rows from
    /// long before lost -- and one taken between two databases' snapshots
    /// by both. Followed again once the rows are read: where the databases
    /// or the ranges let go differ, the answer is not given, and the read
    /// is asked again. Nothing is held back to say so: a caller handed
    /// rows before the refusal drops them with it, as on any failed read.
    ///
    /// INTERIM(v32): gone with [`Self::follow_owners`], when v32's
    /// ownership record refuses a read a move went by or reads it again.
    fn standing<T>(
        &self,
        datasets: &[String],
        read: impl FnOnce(&BTreeMap<String, Followed>) -> Result<T>,
    ) -> Result<T> {
        let follow = || -> Result<BTreeMap<String, Followed>> {
            let mut followed = BTreeMap::new();
            for dataset in datasets {
                if !followed.contains_key(dataset) {
                    followed.insert(dataset.clone(), self.follow_owners(dataset)?);
                }
            }
            Ok(followed)
        };
        let before = follow()?;
        if self.stores.len() == 1 {
            return read(&before);
        }
        for dataset in before.keys() {
            // What a test arms to move a range between the two.
            meridian_changes::moves::points::reached("lake.read.routed", dataset)
                .map_err(StoreError::Unavailable)?;
        }
        let answer = read(&before)?;
        if let Some((dataset, _)) = follow()?
            .iter()
            .find(|(dataset, after)| before.get(*dataset) != Some(after))
        {
            return Err(StoreError::Unavailable(format!(
                "a range of dataset {dataset} moved between databases under the read: ask again"
            )));
        }
        Ok(answer)
    }

    /// [`Self::standing`] for a read of one dataset, by its databases.
    fn standing_one<T>(
        &self,
        dataset: &str,
        read: impl FnOnce(&std::collections::BTreeSet<u32>) -> Result<T>,
    ) -> Result<T> {
        self.standing(&[dataset.to_string()], |followed| {
            read(&followed[dataset].databases)
        })
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
    fn runs(followed: &BTreeMap<String, Followed>) -> Vec<(u32, Vec<String>)> {
        let mut runs: Vec<(u32, Vec<String>)> = Vec::new();
        for (dataset, followed) in followed {
            // A dataset whose ranges are in several databases is read from
            // each, each answering its own ranges (step 3d).
            for d in &followed.databases {
                match runs.last_mut() {
                    Some((at, run)) if at == d => run.push(dataset.clone()),
                    _ => runs.push((*d, vec![dataset.clone()])),
                }
            }
        }
        runs
    }

    /// Each database's counts, a dataset's summed across them (step 3d):
    /// each database counts what is read there, so nothing twice.
    fn summed<V: Default>(
        &self,
        each: impl Fn(&PostgresStore) -> Result<BTreeMap<String, V>>,
        add: impl Fn(&mut V, V),
    ) -> Result<BTreeMap<String, V>> {
        let mut out: BTreeMap<String, V> = BTreeMap::new();
        for store in &self.stores {
            for (dataset, counted) in each(store)? {
                add(out.entry(dataset).or_default(), counted);
            }
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
        self.standing(&query.datasets, |followed| {
            self.read_routed(query, Self::runs(followed), each)
        })
    }

    /// [`Self::read_runs`]' rows, from `runs`.
    fn read_routed(
        &self,
        query: &Query,
        runs: Vec<(u32, Vec<String>)>,
        each: &mut dyn FnMut(Observation) -> Result<()>,
    ) -> Result<()> {
        if runs.len() <= 1 {
            let d = runs.first().map_or(0, |(d, _)| *d);
            return self.database(d).read_each(query, each);
        }
        // A dataset read from several databases (step 3d): a key last
        // written before its range moved may be answered by the source's
        // database from the range's parent and by the target's once
        // restated there -- the higher version is the latest.
        let spans = {
            let mut seen = std::collections::BTreeSet::new();
            runs.iter()
                .flat_map(|(_, datasets)| datasets.iter())
                .any(|dataset| !seen.insert(dataset.clone()))
        };
        if spans && matches!(query.when, crate::store::When::Latest { .. }) {
            let mut latest: BTreeMap<(String, i16, String), Observation> = BTreeMap::new();
            for (d, datasets) in runs {
                let mut part = query.clone();
                part.datasets = datasets;
                self.database(d).read_each(&part, &mut |row| {
                    let key = (
                        row.dataset().to_string(),
                        row.data_type().code(),
                        row.meta().row_key.clone(),
                    );
                    match latest.get(&key) {
                        Some(held) if held.meta().version >= row.meta().version => {}
                        _ => {
                            latest.insert(key, row);
                        }
                    }
                    Ok(())
                })?;
            }
            for row in latest.into_values() {
                each(row)?;
            }
            return Ok(());
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
        // Where each range's head is (step 3d; ruled 2026-11-14).
        self.routed(dataset, vec![rows], &[], |store, mut parts, _| {
            vec![store.record(dataset, parts.pop().unwrap_or_default(), now_ns)]
        })
        .pop()
        .unwrap_or_else(|| Ok(Recorded::default()))
    }

    fn record_parts(
        &self,
        dataset: &str,
        parts: Vec<Vec<Observation>>,
        shards: &[Option<ShardStamp>],
        now_ns: i64,
    ) -> Vec<Result<Recorded>> {
        self.routed(dataset, parts, shards, |store, parts, shards| {
            store.record_parts(dataset, parts, shards, now_ns)
        })
    }

    fn serve_unkept(
        &self,
        dataset: &str,
        rows: Vec<Observation>,
        readers: &[String],
        shard: Option<&ShardStamp>,
        now_ns: i64,
    ) -> Result<Recorded> {
        self.routed(
            dataset,
            vec![rows],
            &[shard.cloned()],
            |store, mut parts, shards| {
                vec![store.serve_unkept(
                    dataset,
                    parts.pop().unwrap_or_default(),
                    readers,
                    shard_at(shards, 0),
                    now_ns,
                )]
            },
        )
        .pop()
        .unwrap_or_else(|| Ok(Recorded::default()))
    }

    fn take_shard(
        &self,
        took: meridian_domain::v1::ShardTookOver,
        taking: &meridian_changes::shard_fence::Taking,
    ) -> Result<meridian_domain::v1::ShardTookOver> {
        // Numbered at home, where the control partition is; every other
        // database is raised by the group's raise (step 3d, item 32).
        self.own().take_shard(took, taking)
    }

    fn record_shard_take(&self, took: &meridian_domain::v1::ShardTookOver) -> Result<()> {
        self.own().record_shard_take(took)
    }

    fn raise_shard(
        &self,
        instance: &str,
        shard: u32,
        epoch: u64,
        admit: Option<meridian_changes::shard_fence::Admit>,
    ) -> Result<u64> {
        // Every database this group may write a part to: a range moved
        // between databases is written where its head is (step 3d).
        let mut held = 0;
        for store in &self.stores {
            held = held.max(store.raise_shard(instance, shard, epoch, admit)?);
        }
        Ok(held)
    }

    fn reseat_shards(&self) -> Result<usize> {
        // Every database, each from database 0's rows (review c5a L1,
        // c5a3 H1): a database added later holds them before it serves.
        let mut raised = 0;
        for store in &self.stores {
            raised += store.reseat_shards()?;
        }
        Ok(raised)
    }

    fn in_stream(&self, identity: i64) {
        for store in &self.stores {
            store.in_stream(identity);
        }
    }

    fn write_ahead_carried(&self, from: &str, to: &str, made_ns: i64) -> Result<bool> {
        // In every database: a group's places kept before the count was
        // raised are still its own (step 3d), each carried where it is.
        let mut carried = false;
        for store in &self.stores {
            carried |= store.write_ahead_carried(from, to, made_ns)?;
        }
        Ok(carried)
    }

    fn write_ahead_position(&self, stream: &str) -> Result<Option<WriteAheadPosition>> {
        if let Some(at) = self.own().write_ahead_position(stream)? {
            return Ok(Some(at));
        }
        // None at home: its group committed in another database before the
        // count was raised (step 3d; "lake loss is re-sent, never silent").
        // That position is carried home with nothing else, so the writer
        // goes on from it and a run the stream dropped past it is a gap --
        // never mistaken for a group's first start. Never a point a switch
        // kept beside a pointer, which is the source's (finding 4).
        let Some(at) = self.furthest_position(stream, self.groups.mine())? else {
            return Ok(None);
        };
        self.own().record_write_ahead(
            WriteAheadBatch {
                stream: stream.to_string(),
                after: None,
                through: at,
                ..Default::default()
            },
            meridian_changes::now_ns(),
        )?;
        self.own().write_ahead_position(stream)
    }

    fn read_owners_again(&self) -> Result<()> {
        if self.stores.len() == 1 {
            self.own().read_owners()?;
            return Ok(());
        }
        // Each range's group as its furthest row names it, a written row
        // before a move's pointer at the same head -- never the last
        // database read (the review's C4).
        let mut furthest: BTreeMap<String, ((u64, bool), Option<u32>)> = BTreeMap::new();
        for store in &self.stores {
            for (partition, named, head, written) in store.owner_rows()? {
                let at = (head, written);
                if furthest.get(&partition).is_none_or(|(held, _)| *held < at) {
                    furthest.insert(partition, (at, named));
                }
            }
        }
        for (partition, (_, named)) in furthest {
            if let Some(group) = named {
                self.groups.assign(&partition, Some(group));
            }
        }
        Ok(())
    }

    fn group_position(&self, key: &str, group: u32) -> Result<Option<WriteAheadPosition>> {
        // Where the group keeps it: at its home, else where it was before
        // the count was raised (step 3d). Its home's first, never the
        // furthest: a range's switch between databases keeps, beside the
        // source's pointer, the place its target takes the range's rows
        // from under the target's own key, which is the source's place and
        // not one the target has committed to.
        let home = meridian_changes::databases::of_group(group, self.count());
        if let Some(at) = self.database(home).write_ahead_position(key)? {
            return Ok(Some(at));
        }
        self.furthest_position(key, group)
    }

    fn range_between(&self, dataset: &str) -> Result<bool> {
        if self.stores.len() == 1 {
            return Ok(false);
        }
        // Each of its ranges by its furthest row, a written one before a
        // move's pointer at the same head: a range whose furthest is a
        // pointer was let go and is not opened yet.
        let mut furthest: BTreeMap<String, (u64, bool)> = BTreeMap::new();
        for store in &self.stores {
            for (range, head, written) in store.rows_of(dataset)? {
                let at = (head, written);
                if furthest.get(&range).is_none_or(|held| *held < at) {
                    furthest.insert(range, at);
                }
            }
        }
        Ok(furthest.values().any(|(_, written)| !written))
    }

    fn write_ahead_part(
        &self,
        key: &str,
        group: u32,
        sequence: u64,
    ) -> Result<Option<crate::store::Counts>> {
        // Kept with the group's position, wherever that was at each take.
        let mut all: Option<crate::store::Counts> = None;
        for store in &self.stores {
            if let Some(part) = store.write_ahead_part(key, group, sequence)? {
                all.get_or_insert_with(Default::default).add(part);
            }
        }
        Ok(all)
    }

    fn write_ahead_refused(&self, key: &str, group: u32, sequence: u64) -> Result<Option<String>> {
        // Kept with the group's part, wherever its position was then.
        for store in &self.stores {
            if let Some(refused) = store.write_ahead_refused(key, group, sequence)? {
                return Ok(Some(refused));
            }
        }
        Ok(None)
    }

    fn record_write_ahead(
        &self,
        mut batch: WriteAheadBatch,
        now_ns: i64,
    ) -> Result<Vec<Result<Recorded>>> {
        // Each part where its ranges' heads are (step 3d; ruled
        // 2026-11-14): those wholly this member's own database's with the
        // stream's position, in one transaction; the rest first, each in
        // its own database -- a batch refused after them is taken again,
        // and each row is recorded once.
        let own = meridian_changes::databases::of_group(self.groups.mine(), self.count());
        let mut answers: Vec<Option<Result<Recorded>>> =
            (0..batch.parts.len()).map(|_| None).collect();
        let mut places = Vec::new();
        let mut here = Vec::new();
        let datasets: Vec<String> = batch
            .parts
            .iter()
            .map(|(dataset, _)| dataset.clone())
            .collect();
        let sizes: Vec<usize> = batch.parts.iter().map(|(_, rows)| rows.len()).collect();
        let shards = std::mem::take(&mut batch.shards);
        let mut here_shards = Vec::new();
        for (at, (dataset, rows)) in std::mem::take(&mut batch.parts).into_iter().enumerate() {
            let shard = shard_at(&shards, at).cloned();
            if self.write_dbs(&dataset, &rows).iter().all(|d| *d == own) {
                places.push(at);
                here.push((dataset, rows));
                here_shards.push(shard);
            } else {
                answers[at] = self
                    .routed(&dataset, vec![rows], &[shard], |store, parts, shards| {
                        store.record_parts(&dataset, parts, shards, now_ns)
                    })
                    .pop();
            }
        }
        // A part held by a move's switch elsewhere, or another group's: the
        // batch refused whole before its position commits, and taken again
        // (step 3d; as PostgresStore's own).
        if let Some(refused) = crate::postgres::write_ahead_refusal(
            answers
                .iter()
                .filter_map(|answer| answer.as_ref()?.as_ref().err()),
            self.groups.mine(),
        ) {
            return Err(refused);
        }
        // Each counted write's parts by their places here; what its parts
        // committed elsewhere did, counted with them (spec 6.7), each range
        // by where its source stood when it last moved to this group.
        let taken_from = |range: &str| {
            self.stores
                .iter()
                .filter_map(|store| store.taken_from(&[range.to_string()]).ok())
                .filter_map(|taken| taken.get(range).copied())
                .max()
        };
        for counted in &mut batch.counted {
            let mut kept = Vec::new();
            for place in std::mem::take(&mut counted.parts) {
                match places.iter().position(|at| *at == place) {
                    Some(here) => kept.push(here),
                    None => match (answers.get_mut(place), datasets.get(place)) {
                        (Some(Some(Ok(done))), Some(dataset)) => {
                            let did = counted.did_and_answers(dataset, done, &taken_from);
                            counted.already.add(did);
                        }
                        // Refused there by the lake's rule 5: kept with
                        // the part, for the group answering the write.
                        (Some(Some(Err(failed))), _) => {
                            let rows = sizes.get(place).copied().unwrap_or(0);
                            counted.refuse(failed, rows);
                        }
                        _ => {}
                    },
                }
            }
            counted.parts = kept;
        }
        let again: Vec<_> = here.clone().into_iter().zip(here_shards.clone()).collect();
        batch.parts = here;
        batch.shards = here_shards;
        let done = self.own().record_write_ahead(batch, now_ns)?;
        for ((at, result), ((dataset, rows), shard)) in places.into_iter().zip(done).zip(again) {
            answers[at] = Some(match result {
                // Moved home since it was routed: routed again.
                Err(StoreError::NotOwned { partition, group }) if group == self.groups.mine() => {
                    self.route_again(&partition);
                    self.routed(&dataset, vec![rows], &[shard], |store, parts, shards| {
                        store.record_parts(&dataset, parts, shards, now_ns)
                    })
                    .pop()
                    .unwrap_or_else(|| Ok(Recorded::default()))
                }
                other => other,
            });
        }
        Ok(answers
            .into_iter()
            .map(|answer| answer.unwrap_or_else(|| Ok(Recorded::default())))
            .collect())
    }

    fn write_ahead_found(&self, stream: &str) -> Result<(Vec<Dropped>, BTreeMap<String, i64>)> {
        // Every database's: a group's runs and valid times found before the
        // count was raised are still its own (step 3d).
        let (mut dropped, mut valid) = self.own().write_ahead_found(stream)?;
        for store in &self.stores {
            if Arc::ptr_eq(store, self.own()) {
                continue;
            }
            let (more, times) = store.write_ahead_found(stream)?;
            for run in more {
                if !dropped.iter().any(|held| held.first == run.first) {
                    dropped.push(run);
                }
            }
            for (dataset, at) in times {
                let held = valid.entry(dataset).or_insert(at);
                *held = (*held).max(at);
            }
        }
        dropped.sort_by_key(|run| run.first);
        Ok((dropped, valid))
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
        // A range moved between databases has a head in each, its source's
        // stopped at the switch: the furthest is its own (step 3d).
        let mut out: BTreeMap<String, u64> = BTreeMap::new();
        for store in &self.stores {
            for (partition, head) in store.heads()? {
                let at = out.entry(partition).or_insert(head);
                *at = (*at).max(head);
            }
        }
        Ok(out)
    }

    fn counts(&self) -> Result<BTreeMap<String, (u64, u64)>> {
        // A dataset whose ranges are in several databases: each database's
        // count of the ranges read there, summed (step 3d).
        self.summed(
            |store| store.counts(),
            |sum, (rows, unconverted)| {
                sum.0 += rows;
                sum.1 += unconverted;
            },
        )
    }

    fn unconverted_counts(&self) -> Result<BTreeMap<String, u64>> {
        self.summed(|store| store.unconverted_counts(), |sum, n| *sum += n)
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
        self.standing_one(dataset, |databases| {
            let mut out = Vec::new();
            for d in databases {
                out.extend(self.database(*d).dated_on(dataset, dates, after, most)?);
            }
            if databases.len() > 1 {
                out.sort_by_key(|row| row.meta().sequence);
                out.truncate(most);
            }
            Ok(out)
        })
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
        self.summed(
            |store| store.closed_day_counts(),
            |sum, (one, two)| {
                sum.0 += one;
                sum.1 += two;
            },
        )
    }

    fn quality_counts(&self) -> Result<BTreeMap<String, (u64, u64, u64)>> {
        self.summed(
            |store| store.quality_counts(),
            |sum, (fine, suspicious, invalid)| {
                sum.0 += fine;
                sum.1 += suspicious;
                sum.2 += invalid;
            },
        )
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
        self.standing_one(dataset, |databases| {
            let mut out = Vec::new();
            for d in databases {
                out.extend(self.database(*d).read_dataset(
                    dataset,
                    data_type,
                    as_of_ns,
                    published_as_of_ns,
                )?);
            }
            Ok(out)
        })
    }

    fn read_dataset_each(
        &self,
        dataset: &str,
        data_type: DataType,
        as_of_ns: i64,
        each: &mut dyn FnMut(Observation) -> Result<()>,
    ) -> Result<()> {
        self.standing_one(dataset, |databases| {
            for d in databases {
                self.database(*d)
                    .read_dataset_each(dataset, data_type, as_of_ns, each)?;
            }
            Ok(())
        })
    }

    fn read_dataset_versions(
        &self,
        dataset: &str,
        data_type: DataType,
    ) -> Result<Vec<Observation>> {
        self.standing_one(dataset, |databases| {
            let mut out = Vec::new();
            for d in databases {
                out.extend(
                    self.database(*d)
                        .read_dataset_versions(dataset, data_type)?,
                );
            }
            Ok(out)
        })
    }

    fn row_version(&self, dataset: &str, row_key: &str, version: u64) -> Result<Vec<Observation>> {
        // The key's range is in one of them.
        self.standing_one(dataset, |databases| {
            for d in databases {
                let found = self.database(*d).row_version(dataset, row_key, version)?;
                if !found.is_empty() {
                    return Ok(found);
                }
            }
            Ok(Vec::new())
        })
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
        let epoch = self.own().adopt()?;
        // Where its ranges' heads are ahead of home (step 3d).
        for store in self.ahead_away()? {
            store.adopt()?;
        }
        Ok(epoch)
    }

    fn epoch(&self) -> u64 {
        self.own().epoch()
    }

    fn take_over(&self, by: &meridian_changes::TookOver) -> Result<u64> {
        let epoch = self.own().take_over(by)?;
        // Its group's writer before fenced where its ranges' heads are
        // ahead of home too (step 3d).
        for store in self.ahead_away()? {
            store.take_over(by)?;
        }
        Ok(epoch)
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
