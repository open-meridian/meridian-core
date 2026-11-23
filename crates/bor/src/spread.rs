//! The book over several databases (step 3d; spec/a-deployment-scales-out-
//! and-fails-over-by-configuration section 6.4, ruled 2026-11-10 and
//! 2026-11-14): `bor.databases` databases, writer group `g` writing to
//! database `g mod count`.
//!
//! - **A partition's journal, projections and placements** are in its
//!   writer group's database: an act reads its account's partition again
//!   under the partition's lock, in the same database (section 6.3).
//! - **Acts** go to this member's own database's store, which commits only
//!   its group's partitions and hands every other's on (section 6.7). An
//!   account's placement is looked for in every database, so an act on an
//!   account placed elsewhere is handed on, never placed again here.
//! - **Reads** naming an account go to its database; a read of every
//!   account fans out, a page at a time from each database in turn, its
//!   cursor naming the database it reached.
//! - **The store's own records** -- `~control`, the cash instruments -- are
//!   database 0's. Each database's takeovers and findings, kept beside its
//!   journal, are owed to database 0's `~control` and journalled there by
//!   group 0, each once.
//!
//! With one database, the default, the book uses its one store as it always
//! has, and none of this is built.

use std::sync::Arc;

use meridian_changes::groups::Groups;
use meridian_domain::v1::{AccountAttributes, AccountFigures, BookPosition, Break};

use crate::postgres::PostgresStore;
use crate::store::{
    Acted, AttributesRead, BreaksRead, Committed, Decide, FiguresRead, HistoryRead,
    InstructionsRead, LocatesRead, Mark, OrdersPage, OrdersRead, Page, PositionsRead, Record,
    Result, Store, StoreError,
};

/// A book member's stores, one per database, database 0 first.
pub struct Spread {
    stores: Vec<Arc<PostgresStore>>,
    groups: Groups,
    /// Each partition found at its owner's home database, by owner: read
    /// there from then on, as only a move takes it away -- to another
    /// group, so another owner (step 3d; ruled 2026-11-14).
    settled: std::sync::Mutex<std::collections::BTreeSet<(String, u32)>>,
    /// The revision this member runs and how recently a member must have
    /// been seen to be waited on (step 3d; ruled 2026-11-15): an account's
    /// move to another database waits until every member seen within that
    /// long runs it. None set, nothing is waited on.
    rollout: Option<(String, std::time::Duration, Arc<dyn meridian_clock::Clock>)>,
}

/// How many times an act is routed again to the database its partition's
/// head is in: once moved home, it is there.
const ROUTED_AGAIN: usize = 3;

/// How many times a read naming an account is made again where the
/// account went while it was read.
const READ_AGAIN: usize = 3;

/// A page some read answers, filled from one database after another.
trait Filled: Default {
    fn len(&self) -> usize;
    fn cursor(&self) -> &str;
    fn set_cursor(&mut self, cursor: String);
    fn take(&mut self, other: Self);
}

impl<T: Default> Filled for Page<T> {
    fn len(&self) -> usize {
        self.records.len()
    }
    fn cursor(&self) -> &str {
        &self.next_cursor
    }
    fn set_cursor(&mut self, cursor: String) {
        self.next_cursor = cursor;
    }
    fn take(&mut self, other: Self) {
        self.records.extend(other.records);
        merge_mark(&mut self.as_of, other.as_of);
    }
}

impl Filled for OrdersPage {
    fn len(&self) -> usize {
        self.orders.len() + self.placements.len() + self.executions.len() + self.unplaced.len()
    }
    fn cursor(&self) -> &str {
        &self.next_cursor
    }
    fn set_cursor(&mut self, cursor: String) {
        self.next_cursor = cursor;
    }
    fn take(&mut self, other: Self) {
        self.orders.extend(other.orders);
        self.placements.extend(other.placements);
        self.executions.extend(other.executions);
        self.unplaced.extend(other.unplaced);
        merge_mark(&mut self.as_of, other.as_of);
    }
}

fn merge_mark(into: &mut Mark, from: Mark) {
    for (partition, head) in from {
        let at = into.entry(partition).or_insert(head);
        *at = (*at).max(head);
    }
}

/// A fanned-out read's cursor: the database it reached and that database's
/// own cursor, `<d>:<cursor>`.
fn split_cursor(cursor: &str) -> Result<(usize, String)> {
    if cursor.is_empty() {
        return Ok((0, String::new()));
    }
    let (d, rest) = cursor.split_once(':').ok_or_else(|| {
        StoreError::Invalid(format!("cursor {cursor:?} is no cursor this read gave"))
    })?;
    let d = d.parse().map_err(|_| {
        StoreError::Invalid(format!("cursor {cursor:?} is no cursor this read gave"))
    })?;
    Ok((d, rest.to_string()))
}

impl Spread {
    /// The stores of every database, database 0 first, for a member of
    /// `groups`; each store finds an account placed in another database.
    pub fn new(stores: Vec<Arc<PostgresStore>>, groups: Groups) -> Self {
        assert!(!stores.is_empty(), "a book has a database");
        for store in &stores {
            store.link_databases(&stores);
        }
        Self {
            stores,
            groups,
            settled: Default::default(),
            rollout: None,
        }
    }

    /// An account is moved to another database only once every member of
    /// the book seen within `within`, by `clock`, runs `revision`, this
    /// member's own (step 3d; ruled 2026-11-15).
    pub fn waiting_for_rollout(
        mut self,
        revision: String,
        within: std::time::Duration,
        clock: Arc<dyn meridian_clock::Clock>,
    ) -> Self {
        self.rollout = Some((revision, within, clock));
        self
    }

    /// How many databases.
    pub fn count(&self) -> u32 {
        self.stores.len() as u32
    }

    /// The store of database `d`.
    /// A plugin shard's epoch raised in every database this group may
    /// write a command to (step 3d, checkpoint 5, item 32): the highest held
    /// after.
    fn raise_everywhere(&self, instance: &str, shard: u32, epoch: u64) -> Result<u64> {
        let mut held = 0;
        for store in &self.stores {
            held = held.max(store.raise_shard(instance, shard, epoch)?);
        }
        Ok(held)
    }

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

    fn home(&self) -> &Arc<PostgresStore> {
        &self.stores[0]
    }

    /// The account copied into database `here`, where its act goes, from
    /// any other whose row names a partition written there: its move closed
    /// there, toward this database (step 3d; section 6.4). Nothing where it
    /// is here already, or closed toward nowhere here.
    fn bring_in(&self, account_id: &str, here: u32) -> Result<()> {
        let into = self.database(here);
        for (d, store) in self.stores.iter().enumerate() {
            if Arc::ptr_eq(store, into) {
                continue;
            }
            let Some(partition) = store.account_row(account_id)? else {
                continue;
            };
            if self.of(&partition) == here {
                let added = into.copy_account_from(store, account_id)?;
                tracing::info!(
                    account_id,
                    from = d,
                    added,
                    "an account's move copied it into this database"
                );
            }
        }
        Ok(())
    }

    /// The databases past this member's own where its group holds a
    /// partition further on than at home: the database count was raised and
    /// they are not moved home yet (step 3d; ruled 2026-11-14). Its group's
    /// epoch is adopted, or taken over, in each, as at home, so a writer of
    /// its group before is fenced there too.
    fn ahead_away(&self) -> Result<Vec<&Arc<PostgresStore>>> {
        let own = self.own();
        let mut away = Vec::new();
        for store in &self.stores {
            if Arc::ptr_eq(store, own) {
                continue;
            }
            let ahead = (0..self.partition_count())
                .map(crate::store::partition_named)
                .filter(|partition| self.groups.owns(partition))
                .try_fold(false, |ahead, partition| -> Result<bool> {
                    if ahead {
                        return Ok(true);
                    }
                    let there = store.tip(&partition)?;
                    // A move's pointer (its epoch `POINTER_EPOCH`) is never written there.
                    let named_mine = there.as_ref().is_some_and(|tip| {
                        tip.epoch != meridian_changes::moves::POINTER
                            && tip
                                .writer_group
                                .is_none_or(|group| group == self.groups.mine())
                    });
                    let head_there = there.map_or(0, |tip| tip.head);
                    let head_home = own.tip(&partition)?.map_or(0, |tip| tip.head);
                    Ok(named_mine && head_there > head_home)
                })?;
            if ahead {
                away.push(store);
            }
        }
        Ok(away)
    }

    /// The database holding `partition`'s head (`control` is database
    /// 0's; step 3d, ruled 2026-11-14): among the databases whose head of it
    /// names its owner, the one furthest on, its owner's home where two
    /// stand level -- a move home switched -- or none has begun it. Once
    /// there, it is read there until its owner changes.
    pub fn of(&self, partition: &str) -> u32 {
        if partition == crate::store::CONTROL_PARTITION {
            return 0;
        }
        let owner = self.groups.owner(partition);
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
        let mut best = (0u64, home);
        for (d, store) in self.stores.iter().enumerate() {
            // A pointer (its epoch `POINTER_EPOCH`) is where a move let it go, never where
            // it is written.
            let Ok(Some(tip)) = store.tip(partition) else {
                continue;
            };
            if tip.epoch == meridian_changes::moves::POINTER {
                continue;
            }
            let named = tip
                .writer_group
                .filter(|group| *group < self.groups.count())
                .unwrap_or_else(|| meridian_changes::groups::owner(partition, self.groups.count()));
            if named == owner && tip.head > best.0 {
                best = (tip.head, d as u32);
            }
        }
        // Level with the furthest: home.
        if best.1 != home
            && self
                .database(home)
                .tip(partition)
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

    /// `partition` routed again by its head from now on: a write found it
    /// moved home.
    fn route_again(&self, partition: &str) {
        self.settled
            .lock()
            .expect("settled poisoned")
            .retain(|(held, _)| held != partition);
    }

    /// The database a read naming an account of `partition` is sent to, by
    /// the heads it has just read, where this member's view of the
    /// partition's owner was read before a move (step 3d; sections 5 and
    /// 6.4; merge-queue run 45104369464, Elle). A member reads the writer
    /// groups at its start, every few seconds and on each write handed back
    /// to it, and a reader is handed none: until its next look, a read
    /// naming an account went by the view to the partition's source, which
    /// holds what it closed with until it lets its accounts go, while the
    /// read's `read_after` had passed on the target's head.
    ///
    /// - **One head asked** where the database the view names writes the
    ///   partition: there. On that database's pool's own wait, as the read
    ///   itself is (the review of 2026-11-23 of this routing, finding 2).
    /// - **Every other's** only where that head is a move's pointer, or the
    ///   database has not begun the partition: the written head furthest
    ///   on, where it is at or past every pointer read with it; else the
    ///   view's. A head left written behind a move's pointers -- a move
    ///   home whose source's commit failed leaves one for good -- is never
    ///   the partition's (the same review, finding 1).
    /// - **Refused UNAVAILABLE** where a database that may write it cannot
    ///   be asked within [`crate::postgres::PostgresStore::head_at`]'s
    ///   bound: never answered from a copy a move let go, nor held for a
    ///   pool's 30 s by another database's outage (the review of
    ///   2026-11-23, finding 4).
    /// - **This read's alone**: the view is never written here (the same
    ///   review, finding 3). A reader's replicas share it with its own
    ///   stores, and one behind a move would write the owner before it.
    ///
    /// INTERIM(v32): a stop-gap for a view of the owners read on a timer,
    /// for reads naming an account alone; gone when v32 holds that a read
    /// with `read_after` is never answered from a copy a move let go, and a
    /// move during a read is refused or read again.
    fn follow_owner(&self, partition: &str) -> Result<u32> {
        let sent = self.of(partition);
        if self.stores.len() == 1 || partition == crate::store::CONTROL_PARTITION {
            return Ok(sent);
        }
        let head = |d: u32| {
            self.database(d)
                .head_at(partition, d == sent)
                .map_err(|failed| {
                    StoreError::Unavailable(format!(
                        "partition {partition}'s head in database {d}, which may write it, could not be read ({failed}): ask again"
                    ))
                })
        };
        // The furthest pointer read: where a move let the partition go.
        let mut pointed = match head(sent)? {
            Some((at, true)) if at > 0 => return Ok(sent),
            Some((at, false)) => at,
            _ => 0,
        };
        let mut furthest: Option<(u64, u32)> = None;
        for d in (0..self.count()).filter(|d| *d != sent) {
            match head(d)? {
                Some((at, true)) => {
                    if at > 0 && furthest.is_none_or(|(held, _)| at > held) {
                        furthest = Some((at, d));
                    }
                }
                Some((at, false)) => pointed = pointed.max(at),
                None => {}
            }
        }
        Ok(match furthest {
            Some((at, d)) if at >= pointed => d,
            _ => sent,
        })
    }

    /// The partition `account_id` is placed in, else its first.
    fn partition_or_first(&self, account_id: &str) -> String {
        self.partition_of(account_id)
            .unwrap_or_else(|| crate::store::first_partition_of(account_id).to_string())
    }

    /// Each database's partitions, those it writes: each partition at the
    /// database [`Self::of`] names, `control` at database 0.
    fn written(&self) -> Vec<Vec<String>> {
        let mut written: Vec<Vec<String>> = vec![Vec::new(); self.stores.len()];
        let partitions = (0..self.partition_count())
            .map(crate::store::partition_named)
            .chain([crate::store::CONTROL_PARTITION.to_string()]);
        for partition in partitions {
            written[self.of(&partition) as usize].push(partition);
        }
        written
    }

    /// Each database's share of a read of every account: the accounts of
    /// the partitions it writes (the review's C5), and an account between
    /// its move's close and its open -- its source's row naming a partition
    /// another database writes, which holds no row of it yet -- answered at
    /// its source and left out at its target (m9): never missed while its
    /// target holds nothing of it, and never read twice, an open made
    /// between this look and the target's read included.
    fn shares(&self) -> Result<Vec<crate::postgres::Share>> {
        let written = self.written();
        let mut shares: Vec<crate::postgres::Share> = written
            .iter()
            .map(|partitions| crate::postgres::Share {
                partitions: partitions.clone(),
                ..Default::default()
            })
            .collect();
        if self.stores.len() == 1 {
            return Ok(shares);
        }
        for (d, store) in self.stores.iter().enumerate() {
            // By the database that writes each named partition.
            let mut toward: Vec<Vec<String>> = vec![Vec::new(); self.stores.len()];
            for (account, partition) in store.accounts_placed_outside(&written[d])? {
                if let Some(e) = written.iter().position(|held| held.contains(&partition)) {
                    if e != d {
                        toward[e].push(account);
                    }
                }
            }
            for (e, accounts) in toward.into_iter().enumerate() {
                if accounts.is_empty() {
                    continue;
                }
                let opened = self.stores[e].accounts_held(&accounts)?;
                for account in accounts {
                    if !opened.contains(&account) {
                        shares[e].not.push(account.clone());
                        shares[d].also.push(account);
                    }
                }
            }
        }
        Ok(shares)
    }

    /// `read` of each database in turn, each answering only its share of
    /// the accounts ([`Self::shares`]; the review's C5): an account a move
    /// copied into another database -- warming, switching, or let go and not
    /// yet forgotten -- is read once, from the database writing it; one
    /// between its close and its open, once, from its source.
    fn each_written<T>(&self, mut read: impl FnMut(&PostgresStore) -> Result<T>) -> Result<Vec<T>> {
        let shares = self.shares()?;
        self.stores
            .iter()
            .zip(&shares)
            .map(|(store, share)| crate::postgres::answering_for(share, || read(store)))
            .collect()
    }

    /// The store holding `account_id`'s partition: where it is placed, else
    /// the database its first partition is in.
    fn holding(&self, account_id: &str) -> &Arc<PostgresStore> {
        let partition = self.partition_or_first(account_id);
        self.held_from(account_id, self.database(self.of(&partition)))
    }

    /// `placed`, the database `account_id`'s partition is read in, or the
    /// one holding the account where that one holds nothing of it.
    fn held_from<'a>(
        &'a self,
        account_id: &str,
        placed: &'a Arc<PostgresStore>,
    ) -> &'a Arc<PostgresStore> {
        // Between its move's close and its open across databases, its new
        // database holds nothing of it yet: read where it is held, whole
        // (step 3d; the Elle read of CI 43631481777, an account read as
        // empty after its close).
        if self.stores.len() > 1 && matches!(placed.account_row(account_id), Ok(None)) {
            if let Some(held) = self
                .stores
                .iter()
                .find(|store| matches!(store.account_row(account_id), Ok(Some(_))))
            {
                return held;
            }
        }
        placed
    }

    /// A read naming `account_id`, from the database holding it, and from
    /// the one it went to where it left that database while it was read
    /// (step 3d; sections 6.4 and 6.8). Where it is held is looked up, and
    /// then read: a move across databases that opened and let go between
    /// the two left the read with a database that holds nothing of the
    /// account any more, answered as though the account were empty. So the
    /// database is asked again after the read, and a read of one the
    /// account has left since is made again where it is now.
    fn read_held<T>(
        &self,
        account_id: &str,
        read: impl Fn(&PostgresStore) -> Result<T>,
    ) -> Result<T> {
        if self.stores.len() == 1 {
            return read(self.holding(account_id));
        }
        // Its partition moved since this member last read the writer
        // groups: by its heads, not the view, for this read alone.
        let partition = self.partition_or_first(account_id);
        let routed = self.follow_owner(&partition)?;
        let of = |named: &str| {
            if named == partition {
                routed
            } else {
                self.of(named)
            }
        };
        let mut store = self.held_from(account_id, self.database(routed));
        for _ in 0..READ_AGAIN {
            // What a test arms to move the account between the two.
            if let Err(stopped) =
                meridian_changes::moves::points::reached("book.read.held", account_id)
            {
                return Err(StoreError::Unavailable(stopped));
            }
            let answer = read(store)?;
            // Where it is now: the database of the partition its row names
            // where that database has opened it; else any that holds a row
            // of it. Its row here still naming a partition of this
            // database, or its move's open not yet made, or the account
            // held nowhere (not yet written): the answer is its own.
            let now = match store.account_row(account_id) {
                Err(_) => return Ok(answer),
                Ok(Some(named)) => {
                    let placed = self.database(of(&named));
                    if Arc::ptr_eq(placed, store)
                        || !matches!(placed.account_row(account_id), Ok(Some(_)))
                    {
                        return Ok(answer);
                    }
                    placed
                }
                Ok(None) => {
                    let Some(held) = self
                        .stores
                        .iter()
                        .find(|other| matches!(other.account_row(account_id), Ok(Some(_))))
                    else {
                        return Ok(answer);
                    };
                    held
                }
            };
            store = now;
        }
        read(store)
    }

    /// A read naming an account from its database; one of every account
    /// from each database in turn, `limit` at most, its cursor naming the
    /// database it reached.
    fn fan<P: Filled>(
        &self,
        account_id: &str,
        cursor: &str,
        limit: usize,
        read: impl Fn(&PostgresStore, &str, usize) -> Result<P>,
    ) -> Result<P> {
        if !account_id.is_empty() {
            return self.read_held(account_id, |store| read(store, cursor, limit));
        }
        let (mut d, mut inner) = split_cursor(cursor)?;
        // Each database answering its share of the accounts (the review's
        // C5; m9): never one read twice, nor one between its close and its
        // open missed.
        let shares = self.shares()?;
        let mut page = P::default();
        while d < self.stores.len() {
            let room = limit.saturating_sub(page.len()).max(1);
            let part =
                crate::postgres::answering_for(&shares[d], || read(&self.stores[d], &inner, room))?;
            let next = part.cursor().to_string();
            page.take(part);
            if !next.is_empty() {
                page.set_cursor(format!("{d}:{next}"));
                return Ok(page);
            }
            d += 1;
            inner = String::new();
            if page.len() >= limit && d < self.stores.len() {
                page.set_cursor(format!("{d}:"));
                return Ok(page);
            }
        }
        page.set_cursor(String::new());
        Ok(page)
    }

    /// Each database's takeovers and findings owed to database 0's
    /// `~control`, and every database's partitions whose writer group its
    /// last assignment did not name: group 0's.
    fn owed_everywhere(&self) -> Result<Vec<crate::control::Owed>> {
        let mut owed = self.home().control_records_owed()?;
        for (d, store) in self.stores.iter().enumerate().skip(1) {
            owed.extend(self.home().control_records_owed_of(store, d as u32)?);
        }
        Ok(owed)
    }
}

impl Store for Spread {
    fn raise_shard(&self, instance: &str, shard: u32, epoch: u64) -> Result<u64> {
        self.raise_everywhere(instance, shard, epoch)
    }

    fn act_stamped(
        &self,
        account_id: &str,
        message_id: &str,
        idempotency_key: &str,
        request: &[u8],
        shard: Option<&crate::store::ShardStamp>,
        decide: &mut Decide<'_>,
    ) -> Result<Acted> {
        // Where it is placed, wherever that is: the store then hands an
        // account of another group's on, never placing it again here.
        let placed = self.partition_of(account_id);
        // A write that finds its partition moving between databases waits
        // for the target's open within the hold (step 3d; the review's C3),
        // and never past its caller's deadline (section 6.8): each
        // database's store holds it that long, and so does this.
        let deadline = std::time::Instant::now() + crate::moves::hold_now();
        let mut routed = 0;
        while routed < ROUTED_AGAIN {
            // Its partition's head's database (step 3d; ruled 2026-11-14):
            // where its group writes it until it moves home.
            let partition = placed
                .clone()
                .unwrap_or_else(|| crate::store::first_partition_of(account_id).to_string());
            let d = if self.groups.owns(&partition) {
                self.of(&partition)
            } else {
                meridian_changes::databases::of_group(self.groups.mine(), self.count())
            };
            let store = self.database(d);
            if let Some(partition) = &placed {
                store.learn_placement(account_id, partition);
            }
            // A move's open in this database, the account closed in another:
            // everything that one holds of it copied here first (step 3d;
            // section 6.4, "a move between databases copies the data").
            if crate::moves::is_open_key(idempotency_key) {
                self.bring_in(account_id, d)?;
            }
            match store.act_stamped(
                account_id,
                message_id,
                idempotency_key,
                request,
                shard,
                decide,
            ) {
                // Its own group's partition headed elsewhere: moved home, or
                // not yet -- routed again.
                Err(StoreError::NotOwned { partition, group }) if group == self.groups.mine() => {
                    self.route_again(&partition);
                    routed += 1;
                }
                Err(StoreError::Held(_)) if std::time::Instant::now() < deadline => {
                    self.route_again(&partition);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                other => return other,
            }
        }
        Err(StoreError::Held(format!(
            "account {account_id}'s partition is switching between writer group {}'s databases: ask again in a moment",
            self.groups.mine()
        )))
    }

    fn group_of_partition(&self, partition: &str) -> u32 {
        self.groups.owner(partition)
    }

    fn rollout_holds(&self, account_id: &str, target_partition: &str) -> Result<Option<String>> {
        let Some((revision, within, clock)) = &self.rollout else {
            return Ok(None);
        };
        let from = self.holding(account_id);
        let to = self.database(self.of(target_partition));
        if Arc::ptr_eq(from, to) {
            return Ok(None);
        }
        let behind =
            self.stores[0].members_behind(revision, clock.now_ns(), within.as_nanos() as i64)?;
        Ok(behind.first().map(|member| {
            format!(
                "member {} runs revision {}, and this one {revision}",
                member.member, member.revision
            )
        }))
    }

    fn warm_account(
        &self,
        account_id: &str,
        target_partition: &str,
    ) -> Result<Option<crate::store::Warmed>> {
        // Across databases: its entries copied to the target's before the
        // close, and the copy compared there (step 3d; section 6.8). Its row
        // and projections wait for the open, so the account is in one
        // database until then.
        let from = self.holding(account_id);
        let to = self.database(self.of(target_partition));
        if Arc::ptr_eq(from, to) {
            return Ok(None);
        }
        to.warm_account_from(from, account_id).map(Some)
    }

    fn settle_move(&self, account_id: &str) -> Result<()> {
        // Its database: where its row names a partition placed in the
        // database holding that row, and holds its open.
        let Some(at) = self.stores.iter().enumerate().find_map(|(d, store)| {
            store
                .account_row(account_id)
                .ok()
                .flatten()
                .filter(|partition| self.of(partition) == d as u32)
                .map(|_| d)
        }) else {
            return Ok(());
        };
        for (d, store) in self.stores.iter().enumerate() {
            if d != at && store.account_row(account_id)?.is_some() {
                let gone = store.forget_moved_out(account_id)?;
                tracing::info!(
                    account_id,
                    database = d,
                    gone,
                    "an account moved out of a database: its projections let go, its entries kept"
                );
            }
        }
        Ok(())
    }

    fn on_commit(&self, observer: Committed) -> bool {
        // Every database's commits: its group writes a partition where its
        // head is (step 3d).
        let observer: Arc<Committed> = Arc::new(observer);
        self.stores.iter().all(|store| {
            let observer = Arc::clone(&observer);
            store.on_commit(Box::new(move |epoch, decided| observer(epoch, decided)))
        })
    }

    fn writes(&self, account_id: &str) -> bool {
        match self.partition_of(account_id) {
            Some(partition) => self.groups.owns(&partition),
            None => self.own().writes(account_id),
        }
    }

    fn owner_changes(&self) -> u64 {
        // One view, shared by every database's store.
        self.own().owner_changes()
    }

    fn writer_group_of(&self, account_id: &str) -> Option<u32> {
        if !self.groups.several() {
            return None;
        }
        self.partition_of(account_id)
            .map(|partition| self.groups.owner(&partition))
    }

    fn control_records_owed(&self) -> Result<Vec<crate::control::Owed>> {
        self.owed_everywhere()
    }

    fn owe_control(&self, record: &crate::control::Owed, kept_at_ns: i64) -> Result<()> {
        // In this member's own database, where its group writes.
        self.own().owe_control(record, kept_at_ns)
    }

    fn placement_needed(&self, account_id: &str) -> Result<bool> {
        if self.partition_of(account_id).is_some() {
            return Ok(false);
        }
        self.own().placement_needed(account_id)
    }

    fn place_first(&self, account_id: &str, key: &str) -> Result<String> {
        // In the database its partition is in, with its journal.
        let partition = crate::store::placement_of(key, self.partition_count());
        let placed = self
            .database(self.of(&partition))
            .place_first(account_id, key)?;
        self.own().learn_placement(account_id, &placed);
        Ok(placed)
    }

    fn partition_count(&self) -> u32 {
        self.own().partition_count()
    }

    fn partition_of(&self, account_id: &str) -> Option<String> {
        self.stores
            .iter()
            .find_map(|store| store.placement_held(account_id))
    }

    fn journal(&self, account_id: &str) -> Result<Vec<crate::journal::Entry>> {
        // An account moved between databases has entries in each, in the
        // order written across its move (step 3d; W9.8): each read from the
        // database that writes its partition, never from a copy a move
        // brought another -- so a copy that differs, its move aborted, is
        // never read as the account's.
        if self.stores.len() == 1 {
            return self.home().journal(account_id);
        }
        // Its partition moved since this member last read the writer
        // groups: by its heads, not the view, for this read alone.
        let partition = self.partition_or_first(account_id);
        let routed = self.follow_owner(&partition)? as usize;
        let mut written = self.written();
        for (d, partitions) in written.iter_mut().enumerate() {
            partitions.retain(|held| *held != partition);
            if d == routed {
                partitions.push(partition.clone());
            }
        }
        let mut entries = Vec::new();
        for (store, partitions) in self.stores.iter().zip(&written) {
            entries.extend(store.journal_written_in(account_id, partitions)?);
        }
        Ok(crate::journal::in_order(entries))
    }

    fn adopt(&self) -> Result<u64> {
        let epoch = self.own().adopt()?;
        // Where its partitions' heads are ahead of home (step 3d).
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
        // Its group's writer before fenced where its partitions' heads are
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
        use meridian_changes::timeline::Timeline;
        let mut found = self.own().follow_timeline()?;
        // And each database where its group still writes a partition
        // further on than at home (a raised database count): a new
        // timeline there raises its epoch there too, so its hot books
        // reload what that database kept (section 6.4, restore; the
        // review's A2).
        for store in self.ahead_away()? {
            if let Timeline::Moved(epoch) = store.follow_timeline()? {
                found = Timeline::Moved(epoch);
            }
        }
        Ok(found)
    }

    fn read_changes(
        &self,
        partition: &str,
        after: u64,
        most_bytes: usize,
    ) -> std::result::Result<meridian_changes::ReadChangesReply, meridian_changes::ReadError> {
        self.database(self.of(partition))
            .read_changes(partition, after, most_bytes)
    }

    fn bodies(
        &self,
        account_id: &str,
        entry_ids: &[String],
    ) -> Result<Vec<(String, crate::journal::Body)>> {
        self.read_held(account_id, |store| store.bodies(account_id, entry_ids))
    }

    fn book(&self, account_id: &str) -> Result<crate::book::Book> {
        self.read_held(account_id, |store| store.book(account_id))
    }

    fn book_health(
        &self,
        only: Option<&str>,
        after: &str,
        limit: usize,
    ) -> Result<Vec<meridian_domain::v1::AccountBookHealth>> {
        if let Some(only) = only {
            return self.read_held(only, |store| store.book_health(Some(only), after, limit));
        }
        // In account order across every database, each account from the
        // database writing it.
        let mut out: Vec<_> = self
            .each_written(|store| store.book_health(None, after, limit))?
            .into_iter()
            .flatten()
            .collect();
        out.sort_by(|a, b| a.account_id.cmp(&b.account_id));
        out.truncate(limit);
        Ok(out)
    }

    fn book_as_of(
        &self,
        account_id: &str,
        at: &Mark,
        until: Option<&str>,
    ) -> Result<crate::book::Book> {
        self.read_held(account_id, |store| store.book_as_of(account_id, at, until))
    }

    fn accounts(&self) -> Result<Vec<String>> {
        let mut out: Vec<String> = self
            .each_written(|store| store.accounts())?
            .into_iter()
            .flatten()
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    fn heads(&self) -> Result<Mark> {
        let mut out = Mark::new();
        for store in &self.stores {
            merge_mark(&mut out, store.heads()?);
        }
        Ok(out)
    }

    fn accounts_holding(&self, instrument_id: &str) -> Result<Vec<String>> {
        let mut out: Vec<String> = self
            .each_written(|store| store.accounts_holding(instrument_id))?
            .into_iter()
            .flatten()
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    fn instruments_held(&self) -> Result<Vec<String>> {
        let mut out: Vec<String> = self
            .each_written(|store| store.instruments_held())?
            .into_iter()
            .flatten()
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    fn positions(&self, read: &PositionsRead) -> Result<Page<BookPosition>> {
        self.fan(
            &read.account_id,
            &read.cursor,
            read.limit,
            |store, cursor, limit| {
                let mut part = read.clone();
                part.cursor = cursor.to_string();
                part.limit = limit;
                store.positions(&part)
            },
        )
    }

    fn breaks(&self, read: &BreaksRead) -> Result<Page<Break>> {
        self.fan(
            &read.account_id,
            &read.cursor,
            read.limit,
            |store, cursor, limit| {
                let mut part = read.clone();
                part.cursor = cursor.to_string();
                part.limit = limit;
                store.breaks(&part)
            },
        )
    }

    fn figures(&self, read: &FiguresRead) -> Result<Page<AccountFigures>> {
        self.fan(
            &read.account_id,
            &read.cursor,
            read.limit,
            |store, cursor, limit| {
                let mut part = read.clone();
                part.cursor = cursor.to_string();
                part.limit = limit;
                store.figures(&part)
            },
        )
    }

    fn attributes(&self, read: &AttributesRead) -> Result<Page<AccountAttributes>> {
        self.fan(
            &read.account_id,
            &read.cursor,
            read.limit,
            |store, cursor, limit| {
                let mut part = read.clone();
                part.cursor = cursor.to_string();
                part.limit = limit;
                store.attributes(&part)
            },
        )
    }

    fn rebuild(&self) -> Result<usize> {
        let mut rebuilt = 0;
        for store in &self.stores {
            rebuilt += store.rebuild()?;
        }
        Ok(rebuilt)
    }

    fn cash_instruments(&self) -> Result<Vec<meridian_domain::money::Resolution>> {
        self.home().cash_instruments()
    }

    fn keep_cash_instrument(&self, resolution: &meridian_domain::money::Resolution) -> Result<()> {
        self.home().keep_cash_instrument(resolution)
    }

    fn currency_codes(&self) -> Result<Vec<String>> {
        self.home().currency_codes()
    }

    fn owner(&self, record: Record, id: &str) -> Result<Option<String>> {
        for store in &self.stores {
            if let Some(owner) = store.owner(record, id)? {
                return Ok(Some(owner));
            }
        }
        Ok(None)
    }

    fn orders(&self, read: &OrdersRead) -> Result<OrdersPage> {
        self.fan(
            &read.account_id,
            &read.cursor,
            read.limit,
            |store, cursor, limit| {
                let mut part = read.clone();
                part.cursor = cursor.to_string();
                part.limit = limit;
                store.orders(&part)
            },
        )
    }

    fn order_history(&self, read: &HistoryRead) -> Result<Page<meridian_domain::v1::EntryMeta>> {
        self.fan(
            &read.account_id,
            &read.cursor,
            read.limit,
            |store, cursor, limit| {
                let mut part = read.clone();
                part.cursor = cursor.to_string();
                part.limit = limit;
                store.order_history(&part)
            },
        )
    }

    fn locates(&self, read: &LocatesRead) -> Result<Page<meridian_domain::v1::Locate>> {
        self.fan(
            &read.account_id,
            &read.cursor,
            read.limit,
            |store, cursor, limit| {
                let mut part = read.clone();
                part.cursor = cursor.to_string();
                part.limit = limit;
                store.locates(&part)
            },
        )
    }

    fn instructions(
        &self,
        read: &InstructionsRead,
    ) -> Result<Page<meridian_domain::v1::TradingInstruction>> {
        self.fan("", &read.cursor, read.limit, |store, cursor, limit| {
            let mut part = read.clone();
            part.cursor = cursor.to_string();
            part.limit = limit;
            store.instructions(&part)
        })
    }

    fn expiring(&self, now_ns: i64) -> Result<Vec<(String, String)>> {
        Ok(self
            .each_written(|store| store.expiring(now_ns))?
            .into_iter()
            .flatten()
            .collect())
    }

    fn bookings(
        &self,
        read: &crate::store::BookingsRead,
    ) -> Result<Page<meridian_domain::v1::Booking>> {
        self.fan(
            &read.account_id,
            &read.cursor,
            read.limit,
            |store, cursor, limit| {
                let mut part = read.clone();
                part.cursor = cursor.to_string();
                part.limit = limit;
                store.bookings(&part)
            },
        )
    }

    fn alerts(&self, read: &crate::store::AlertsRead) -> Result<Page<meridian_domain::v1::Alert>> {
        self.fan(
            &read.account_id,
            &read.cursor,
            read.limit,
            |store, cursor, limit| {
                let mut part = read.clone();
                part.cursor = cursor.to_string();
                part.limit = limit;
                store.alerts(&part)
            },
        )
    }

    fn blocks_naming(&self, member_order_id: &str) -> Result<Vec<(String, String)>> {
        Ok(self
            .each_written(|store| store.blocks_naming(member_order_id))?
            .into_iter()
            .flatten()
            .collect())
    }

    fn live_order_instruments(&self) -> Result<Vec<String>> {
        let mut out: Vec<String> = self
            .each_written(|store| store.live_order_instruments())?
            .into_iter()
            .flatten()
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fanned_cursor_names_its_database() {
        assert_eq!(split_cursor("").unwrap(), (0, String::new()));
        assert_eq!(split_cursor("1:abc:def").unwrap(), (1, "abc:def".into()));
        assert!(split_cursor("nope").is_err());
    }
}
