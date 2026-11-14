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
}

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

    fn home(&self) -> &Arc<PostgresStore> {
        &self.stores[0]
    }

    /// The account copied into this member's own database from any other
    /// whose row names a partition placed here: its move closed there,
    /// toward this database (step 3d; section 6.4). Nothing where it is
    /// here already, or closed toward nowhere here.
    fn bring_in(&self, account_id: &str) -> Result<()> {
        let own = self.own();
        let here = meridian_changes::databases::of_group(self.groups.mine(), self.count());
        for (d, store) in self.stores.iter().enumerate() {
            if Arc::ptr_eq(store, own) {
                continue;
            }
            let Some(partition) = store.account_row(account_id)? else {
                continue;
            };
            if self.of(&partition) == here {
                let added = own.copy_account_from(store, account_id)?;
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

    /// The database holding `partition` (`control` is database 0's).
    pub fn of(&self, partition: &str) -> u32 {
        if partition == crate::store::CONTROL_PARTITION {
            return 0;
        }
        meridian_changes::databases::of_group(self.groups.owner(partition), self.count())
    }

    /// The store holding `account_id`'s partition: where it is placed, else
    /// the database its first partition is in.
    fn holding(&self, account_id: &str) -> &Arc<PostgresStore> {
        let partition = self
            .partition_of(account_id)
            .unwrap_or_else(|| crate::store::first_partition_of(account_id).to_string());
        self.database(self.of(&partition))
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
            return read(self.holding(account_id), cursor, limit);
        }
        let (mut d, mut inner) = split_cursor(cursor)?;
        let mut page = P::default();
        while d < self.stores.len() {
            let room = limit.saturating_sub(page.len()).max(1);
            let part = read(&self.stores[d], &inner, room)?;
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
    fn act(
        &self,
        account_id: &str,
        message_id: &str,
        idempotency_key: &str,
        request: &[u8],
        decide: &mut Decide<'_>,
    ) -> Result<Acted> {
        // Where it is placed, wherever that is: the own store then hands an
        // account of another group's on, never placing it again here.
        if let Some(partition) = self.partition_of(account_id) {
            self.own().learn_placement(account_id, &partition);
        }
        // A move's open in this database, the account closed in another:
        // everything that one holds of it copied here first (step 3d;
        // section 6.4, "a move between databases copies the data").
        if crate::moves::is_open_key(idempotency_key) {
            self.bring_in(account_id)?;
        }
        self.own()
            .act(account_id, message_id, idempotency_key, request, decide)
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
        self.own().on_commit(observer)
    }

    fn writes(&self, account_id: &str) -> bool {
        match self.partition_of(account_id) {
            Some(partition) => self.groups.owns(&partition),
            None => self.own().writes(account_id),
        }
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
        // order written across its move (step 3d; W9.8).
        let mut entries = Vec::new();
        for store in &self.stores {
            entries.extend(store.journal(account_id)?);
        }
        Ok(crate::journal::in_order(entries))
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
    ) -> std::result::Result<meridian_changes::ReadChangesReply, meridian_changes::ReadError> {
        self.database(self.of(partition))
            .read_changes(partition, after, most_bytes)
    }

    fn bodies(
        &self,
        account_id: &str,
        entry_ids: &[String],
    ) -> Result<Vec<(String, crate::journal::Body)>> {
        self.holding(account_id).bodies(account_id, entry_ids)
    }

    fn book(&self, account_id: &str) -> Result<crate::book::Book> {
        self.holding(account_id).book(account_id)
    }

    fn book_health(
        &self,
        only: Option<&str>,
        after: &str,
        limit: usize,
    ) -> Result<Vec<meridian_domain::v1::AccountBookHealth>> {
        if let Some(only) = only {
            return self.holding(only).book_health(Some(only), after, limit);
        }
        // In account order across every database.
        let mut out = Vec::new();
        for store in &self.stores {
            out.extend(store.book_health(None, after, limit)?);
        }
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
        self.holding(account_id).book_as_of(account_id, at, until)
    }

    fn accounts(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for store in &self.stores {
            out.extend(store.accounts()?);
        }
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
        let mut out = Vec::new();
        for store in &self.stores {
            out.extend(store.accounts_holding(instrument_id)?);
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    fn instruments_held(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for store in &self.stores {
            out.extend(store.instruments_held()?);
        }
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
        let mut out = Vec::new();
        for store in &self.stores {
            out.extend(store.expiring(now_ns)?);
        }
        Ok(out)
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
        let mut out = Vec::new();
        for store in &self.stores {
            out.extend(store.blocks_naming(member_order_id)?);
        }
        Ok(out)
    }

    fn live_order_instruments(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for store in &self.stores {
            out.extend(store.live_order_instruments()?);
        }
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
