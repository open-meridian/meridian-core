//! The book in memory, behind the same trait the Postgres store answers: for
//! the unit tests, which hold the rules to it, and for nothing else. One lock
//! over everything, which is the partition lock and the transaction at once.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use meridian_domain::v1::{AccountAttributes, AccountFigures, BookPosition, Break};

use crate::book::Book;
use crate::journal::Entry;
use crate::order_reads;
use crate::reads::{self, above, position_key};
use crate::store::{
    first_partition_of, is_account, Acted, AttributesRead, BreaksRead, Decide, FiguresRead,
    HistoryRead, InstructionsRead, LocatesRead, Mark, OrdersPage, OrdersRead, Page, PositionsRead,
    Record, Result, Store, StoreError, CONTROL_PARTITION, FIRST_PARTITION,
};

#[derive(Default)]
struct State {
    heads: Mark,
    journals: BTreeMap<String, Vec<Entry>>,
    books: BTreeMap<String, Book>,
    replies: HashMap<String, Vec<u8>>,
    requests: HashMap<String, Vec<u8>>,
    by_message: HashMap<String, String>,
    by_key: HashMap<(String, String), String>,
    cash: BTreeMap<String, meridian_domain::money::Resolution>,
    /// Every partition's epoch (step 3a), and each entry's, by its ID.
    epoch: u64,
    epochs: HashMap<String, u64>,
}

pub struct MemoryStore {
    state: Mutex<State>,
    fence: meridian_changes::Fence,
    /// Its partitions (step 3d): a new account placed over them by its ID,
    /// as a test double places it; the owner label is the writer's to read.
    partitions: u32,
    /// A test's crash (step 3d's property test): an act it names, by its
    /// account and idempotency key, fails as a writer gone mid-way would.
    #[cfg(test)]
    crash: Mutex<Option<Crash>>,
}

/// Which acts a test's crash fails: by account and idempotency key.
#[cfg(test)]
pub(crate) type Crash = Box<dyn Fn(&str, &str) -> bool + Send>;

#[cfg(test)]
impl MemoryStore {
    /// Fail each act `crash` names until it is lifted with `None`.
    pub(crate) fn crash_when(&self, crash: Option<Crash>) {
        *self.crash.lock().expect("unpoisoned") = crash;
    }

    /// `partition` as a restore to a time leaves it (step 3d; section 6.4):
    /// every entry numbered past `sequence` gone, with what it answered and
    /// the keys it held, and each account it touched replayed from what is
    /// left. Another partition is as it was: a restore across databases
    /// takes each database to its own time.
    pub(crate) fn restore_partition_to(&self, partition: &str, sequence: u64) {
        let mut state = self.state();
        let mut gone: Vec<Entry> = Vec::new();
        for journal in state.journals.values_mut() {
            let (kept, lost): (Vec<Entry>, Vec<Entry>) = journal.drain(..).partition(|entry| {
                entry.partition != partition || entry.first_sequence <= sequence
            });
            *journal = kept;
            gone.extend(lost);
        }
        for entry in &gone {
            state.replies.remove(&entry.entry_id);
            state.requests.remove(&entry.entry_id);
            state.by_message.retain(|_, id| *id != entry.entry_id);
            state.by_key.retain(|_, id| *id != entry.entry_id);
        }
        let touched: std::collections::BTreeSet<String> =
            gone.iter().map(|entry| entry.account_id.clone()).collect();
        for account in touched {
            let journal = state.journals.get(&account).cloned().unwrap_or_default();
            match journal.first() {
                Some(first) => {
                    let book = Book::replay(&account, &first.partition.clone(), &journal)
                        .expect("a journal replays");
                    state.books.insert(account, book);
                }
                None => {
                    state.books.remove(&account);
                }
            }
        }
        state.heads.insert(partition.to_string(), sequence);
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStore {
    pub fn new() -> Self {
        let mut state = State::default();
        state.heads.insert(FIRST_PARTITION.to_string(), 0);
        state.heads.insert(CONTROL_PARTITION.to_string(), 0);
        state.epoch = 1;
        MemoryStore {
            state: Mutex::new(state),
            fence: meridian_changes::Fence::new(),
            partitions: 1,
            #[cfg(test)]
            crash: Mutex::new(None),
        }
    }

    /// The same store over `count` partitions (step 3d), each new account
    /// placed by rendezvous hashing of its ID.
    pub fn with_partitions(mut self, count: u32) -> Self {
        self.partitions = count.max(1);
        let mut state = self.state();
        for n in 0..self.partitions {
            state
                .heads
                .entry(crate::store::partition_named(n))
                .or_insert(0);
        }
        drop(state);
        self
    }

    /// Where a new key starts: an account placed over the partitions.
    fn starts_in(&self, key: &str) -> String {
        if self.partitions > 1 && is_account(key) {
            crate::store::placement_of(key, self.partitions)
        } else {
            first_partition_of(key).to_string()
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("the book's lock is poisoned")
    }
}

impl Store for MemoryStore {
    fn partition_count(&self) -> u32 {
        self.partitions
    }

    fn partition_of(&self, account_id: &str) -> Option<String> {
        self.state()
            .books
            .get(account_id)
            .map(|book| book.partition.clone())
    }

    fn act(
        &self,
        account_id: &str,
        message_id: &str,
        idempotency_key: &str,
        request: &[u8],
        decide: &mut Decide<'_>,
    ) -> Result<Acted> {
        #[cfg(test)]
        if self
            .crash
            .lock()
            .expect("unpoisoned")
            .as_ref()
            .is_some_and(|crash| crash(account_id, idempotency_key))
        {
            return Err(StoreError::Unavailable(format!(
                "the writer stopped before {account_id}'s act {idempotency_key}"
            )));
        }
        let mut state = self.state();
        if let Some(entry_id) = (!message_id.is_empty())
            .then(|| state.by_message.get(message_id))
            .flatten()
        {
            return Ok(Acted::Duplicate(state.replies[entry_id].clone()));
        }
        if let Some(entry_id) = (!idempotency_key.is_empty())
            .then(|| {
                state
                    .by_key
                    .get(&(account_id.to_string(), idempotency_key.to_string()))
            })
            .flatten()
        {
            if state.requests[entry_id] != request {
                return Err(StoreError::conflict(idempotency_key, account_id));
            }
            return Ok(Acted::Duplicate(state.replies[entry_id].clone()));
        }
        let mut book = state
            .books
            .get(account_id)
            .cloned()
            .unwrap_or_else(|| Book::new(account_id, &self.starts_in(account_id)));
        let epoch = self.fence.adopt_once(state.epoch);
        self.fence.admits(state.epoch)?;
        let head = state.heads.get(&book.partition).copied().unwrap_or(0);
        // The book holds no entry's body, as a store's never does (contract
        // v26, W9.10): each one a decision asks for is read from the journal,
        // so every test drives the bodies-on-demand path.
        let journal = state.journals.get(account_id);
        let decided = crate::store::with_bodies(
            &mut book,
            |_, ids| {
                Ok(journal
                    .into_iter()
                    .flatten()
                    .filter(|entry| ids.contains(&entry.entry_id))
                    .map(|entry| (entry.entry_id.clone(), entry.body.clone()))
                    .collect())
            },
            |book| decide(book, head),
        )?;
        let entry = decided.entry.clone();
        // Every act any test makes holds the snapshot to the full replay
        // (contract v25, W9.10): the book before it, through a snapshot's
        // bytes, plus this entry, is the book after it.
        #[cfg(test)]
        crate::snapshot::tests::holds_across(&book, &entry, &decided.book);
        state
            .heads
            .insert(entry.partition.clone(), entry.last_sequence);
        state.epochs.insert(entry.entry_id.clone(), epoch);
        state
            .replies
            .insert(entry.entry_id.clone(), decided.reply.clone());
        state
            .requests
            .insert(entry.entry_id.clone(), request.to_vec());
        if !entry.message_id.is_empty() {
            state
                .by_message
                .insert(entry.message_id.clone(), entry.entry_id.clone());
        }
        if !entry.idempotency_key.is_empty() {
            state.by_key.insert(
                (entry.account_id.clone(), entry.idempotency_key.clone()),
                entry.entry_id.clone(),
            );
        }
        state
            .journals
            .entry(account_id.to_string())
            .or_default()
            .push(entry);
        let mut kept = decided.book.clone();
        kept.release_bodies();
        state.books.insert(account_id.to_string(), kept);
        Ok(Acted::Committed(Box::new(decided)))
    }

    fn book(&self, account_id: &str) -> Result<Book> {
        // Every entry's book is kept, which is a snapshot at every entry.
        Ok(self
            .state()
            .books
            .get(account_id)
            .cloned()
            .unwrap_or_else(|| Book::new(account_id, &self.starts_in(account_id))))
    }

    fn journal(&self, account_id: &str) -> Result<Vec<Entry>> {
        Ok(self
            .state()
            .journals
            .get(account_id)
            .cloned()
            .unwrap_or_default())
    }

    fn adopt(&self) -> Result<u64> {
        let state = self.state();
        Ok(self.fence.adopt_once(state.epoch))
    }

    fn epoch(&self) -> u64 {
        self.fence.epoch()
    }

    fn take_over(&self, _by: &meridian_changes::TookOver) -> Result<u64> {
        let mut state = self.state();
        state.epoch += 1;
        self.fence.adopt(state.epoch);
        Ok(state.epoch)
    }

    fn head_epoch(&self) -> Result<u64> {
        Ok(self.state().epoch)
    }

    fn read_changes(
        &self,
        partition: &str,
        after: u64,
        most_bytes: usize,
    ) -> std::result::Result<meridian_changes::ReadChangesReply, meridian_changes::ReadError> {
        let state = self.state();
        let head = *state
            .heads
            .get(partition)
            .ok_or_else(|| meridian_changes::ReadError::UnknownPartition(partition.to_string()))?;
        let mut entries: Vec<&Entry> = state
            .journals
            .values()
            .flatten()
            .filter(|entry| entry.partition == partition && entry.last_sequence > after)
            .collect();
        entries.sort_by_key(|entry| entry.last_sequence);
        let mut page = meridian_changes::Page::new(most_bytes);
        for entry in entries {
            let epoch = state.epochs.get(&entry.entry_id).copied().unwrap_or(1);
            if !page.take(entry.to_change(epoch, meridian_changes::origin())) {
                break;
            }
        }
        Ok(meridian_changes::ReadChangesReply {
            changes: page.changes,
            head,
            lowest_kept: u64::from(head > 0),
            epoch: state.epoch,
            after_sequence: 0,
        })
    }

    fn accounts(&self) -> Result<Vec<String>> {
        Ok(self
            .state()
            .journals
            .keys()
            .filter(|key| is_account(key))
            .cloned()
            .collect())
    }

    fn heads(&self) -> Result<Mark> {
        Ok(self.state().heads.clone())
    }

    fn accounts_holding(&self, instrument_id: &str) -> Result<Vec<String>> {
        Ok(self
            .state()
            .books
            .iter()
            .filter(|(_, book)| {
                book.positions
                    .iter()
                    .any(|((held, _, _), position)| held == instrument_id && !position.removed)
                    || book
                        .breaks
                        .values()
                        .any(|record| names(record, instrument_id))
            })
            .map(|(account, _)| account.clone())
            .collect())
    }

    fn instruments_held(&self) -> Result<Vec<String>> {
        let state = self.state();
        let mut found = std::collections::BTreeSet::new();
        for book in state.books.values() {
            for ((instrument, _, _), position) in &book.positions {
                if !position.removed {
                    found.insert(instrument.clone());
                }
            }
            for record in book.breaks.values() {
                if let Some(instrument) = open_break_instrument(record) {
                    found.insert(instrument.to_string());
                }
            }
        }
        Ok(found.into_iter().collect())
    }

    fn positions(&self, read: &PositionsRead) -> Result<Page<BookPosition>> {
        read.scope.admit(&read.account_id)?;
        if !read.business_date.is_empty() || read.at.is_some() {
            return reads::positions_as_of(self, read);
        }
        let state = self.state();
        let after = if read.cursor.is_empty() {
            None
        } else {
            Some(reads::from_position_cursor(&read.cursor)?)
        };
        let mut records = Vec::new();
        for (account, book) in &state.books {
            if !read.scope.answers(&read.account_id, account) {
                continue;
            }
            for position in book.positions.values() {
                let record = position.record(account)?;
                let wanted = match &read.since {
                    Some(since) => above(record.last_change.as_ref(), since),
                    None => !record.removed,
                };
                if !wanted
                    || after
                        .as_ref()
                        .is_some_and(|after| position_key(&record) <= *after)
                {
                    continue;
                }
                records.push(record);
            }
        }
        records.sort_by_key(position_key);
        Ok(reads::paged(
            records,
            read.limit,
            reads::position_cursor,
            state.heads.clone(),
        ))
    }

    fn breaks(&self, read: &BreaksRead) -> Result<Page<Break>> {
        read.scope.admit(&read.account_id)?;
        let state = self.state();
        let after = if read.cursor.is_empty() {
            None
        } else {
            let held = reads::parts(&read.cursor, 2)?;
            Some((held[0].clone(), held[1].clone()))
        };
        let mut records: Vec<Break> = state
            .books
            .iter()
            .filter(|(account, _)| read.scope.answers(&read.account_id, account))
            .flat_map(|(_, book)| book.breaks.values().cloned())
            .filter(|record| {
                (read.states.is_empty()
                    || read
                        .states
                        .iter()
                        .any(|state| *state as i32 == record.state))
                    && read
                        .since
                        .as_ref()
                        .is_none_or(|since| above(record.last_change.as_ref(), since))
                    && after.as_ref().is_none_or(|after| {
                        (record.account_id.clone(), record.break_id.clone()) > *after
                    })
            })
            .collect();
        records.sort_by(|a, b| (&a.account_id, &a.break_id).cmp(&(&b.account_id, &b.break_id)));
        Ok(reads::paged(
            records,
            read.limit,
            reads::break_cursor,
            state.heads.clone(),
        ))
    }

    fn figures(&self, read: &FiguresRead) -> Result<Page<AccountFigures>> {
        read.scope.admit(&read.account_id)?;
        if read.at.is_some() {
            return reads::figures_as_of(self, read);
        }
        let state = self.state();
        let after = if read.cursor.is_empty() {
            None
        } else {
            let held = reads::parts(&read.cursor, 3)?;
            Some((held[0].clone(), held[1].clone(), held[2].clone()))
        };
        let mut records: Vec<AccountFigures> = state
            .books
            .iter()
            .filter(|(account, _)| read.scope.answers(&read.account_id, account))
            .flat_map(|(_, book)| book.figures.values().cloned())
            .filter(|record| {
                reads::figures_match(record, read)
                    && read
                        .since
                        .as_ref()
                        .is_none_or(|since| above(record.last_change.as_ref(), since))
                    && after
                        .as_ref()
                        .is_none_or(|after| reads::figures_key(record) > *after)
            })
            .collect();
        records.sort_by_key(reads::figures_key);
        Ok(reads::paged(
            records,
            read.limit,
            reads::figures_cursor,
            state.heads.clone(),
        ))
    }

    fn attributes(&self, read: &AttributesRead) -> Result<Page<AccountAttributes>> {
        read.scope.admit(&read.account_id)?;
        let state = self.state();
        let after = if read.cursor.is_empty() {
            None
        } else {
            Some(reads::parts(&read.cursor, 1)?.remove(0))
        };
        let records: Vec<AccountAttributes> = state
            .books
            .iter()
            .filter(|(account, _)| read.scope.answers(&read.account_id, account))
            .filter(|(account, _)| after.as_ref().is_none_or(|after| *account > after))
            .filter_map(|(_, book)| book.attributes.clone())
            .filter(|record| {
                read.since
                    .as_ref()
                    .is_none_or(|since| above(record.last_change.as_ref(), since))
            })
            .collect();
        Ok(reads::paged(
            records,
            read.limit,
            reads::attributes_cursor,
            state.heads.clone(),
        ))
    }

    fn rebuild(&self) -> Result<usize> {
        let mut state = self.state();
        let mut books = BTreeMap::new();
        let mut replayed = 0;
        for (account, entries) in &state.journals {
            let partition = entries
                .first()
                .map(|entry| entry.partition.clone())
                .unwrap_or_else(|| self.starts_in(account));
            books.insert(account.clone(), Book::replay(account, &partition, entries)?);
            replayed += entries.len();
        }
        state.books = books;
        Ok(replayed)
    }

    fn cash_instruments(&self) -> Result<Vec<meridian_domain::money::Resolution>> {
        Ok(self.state().cash.values().cloned().collect())
    }

    fn keep_cash_instrument(&self, resolution: &meridian_domain::money::Resolution) -> Result<()> {
        self.state()
            .cash
            .entry(resolution.code.clone())
            .or_insert_with(|| resolution.clone());
        Ok(())
    }

    fn owner(&self, record: Record, id: &str) -> Result<Option<String>> {
        let state = self.state();
        Ok(state
            .books
            .iter()
            .find(|(_, book)| match record {
                Record::Order => book.ledger.orders.contains_key(id),
                Record::Placement => book.ledger.placements.contains_key(id),
                Record::ChangeRequest => book.ledger.changes.contains_key(id),
                Record::Locate => book.ledger.locates.contains_key(id),
                Record::Execution => book.ledger.executions.contains_key(id),
                Record::Booking => book.ledger.bookings.contains_key(id),
                Record::Alert => book.ledger.alerts.contains_key(id),
            })
            .map(|(account, _)| account.clone()))
    }

    fn orders(&self, read: &OrdersRead) -> Result<OrdersPage> {
        read.scope.admit(&read.account_id)?;
        let state = self.state();
        let mut items = Vec::new();
        for (key, book) in &state.books {
            if order_reads::book_answers(&read.scope, &read.account_id, key) {
                items.extend(order_reads::items_of(
                    key,
                    book,
                    &read.since,
                    read.execution_state,
                )?);
            }
        }
        order_reads::orders_page(items, read, state.heads.clone())
    }

    fn order_history(&self, read: &HistoryRead) -> Result<Page<meridian_domain::v1::EntryMeta>> {
        read.scope.admit(&read.account_id)?;
        let state = self.state();
        let Some((key, book)) = state.books.iter().find(|(key, book)| {
            book.ledger.orders.contains_key(&read.order_id)
                && order_reads::book_answers(&read.scope, &read.account_id, key)
        }) else {
            return order_reads::history_page(Vec::new(), read);
        };
        let ids = book
            .ledger
            .order_entries
            .get(&read.order_id)
            .cloned()
            .unwrap_or_default();
        let entries = state
            .journals
            .get(key)
            .map(|journal| {
                journal
                    .iter()
                    .filter(|entry| ids.contains(&entry.entry_id))
                    .map(|entry| entry.meta.clone())
                    .collect()
            })
            .unwrap_or_default();
        order_reads::history_page(entries, read)
    }

    fn locates(&self, read: &LocatesRead) -> Result<Page<meridian_domain::v1::Locate>> {
        read.scope.admit(&read.account_id)?;
        let state = self.state();
        let mut records = Vec::new();
        for (key, book) in &state.books {
            for locate in order_reads::locates_of(key, book, read)? {
                records.push((format!("{key}\u{1f}{}", locate.locate_record_id), locate));
            }
        }
        order_reads::keyed_page(records, &read.cursor, read.limit, state.heads.clone())
    }

    fn instructions(
        &self,
        read: &InstructionsRead,
    ) -> Result<Page<meridian_domain::v1::TradingInstruction>> {
        let state = self.state();
        let mut records = Vec::new();
        for book in state.books.values() {
            records.extend(order_reads::instructions_of(book, read));
        }
        order_reads::keyed_page(records, &read.cursor, read.limit, state.heads.clone())
    }

    fn expiring(&self, now_ns: i64) -> Result<Vec<(String, String)>> {
        let state = self.state();
        let mut out = Vec::new();
        for (key, book) in &state.books {
            out.extend(order_reads::expiring_of(key, book, now_ns)?);
        }
        Ok(out)
    }

    fn bookings(
        &self,
        read: &crate::store::BookingsRead,
    ) -> Result<Page<meridian_domain::v1::Booking>> {
        read.scope.admit(&read.account_id)?;
        let state = self.state();
        let mut records = Vec::new();
        for (key, book) in &state.books {
            for booking in order_reads::bookings_of(key, book, read)? {
                records.push((format!("{key}\u{1f}{}", booking.booking_id), booking));
            }
        }
        order_reads::keyed_page(records, &read.cursor, read.limit, state.heads.clone())
    }

    fn alerts(&self, read: &crate::store::AlertsRead) -> Result<Page<meridian_domain::v1::Alert>> {
        read.scope.admit(&read.account_id)?;
        let state = self.state();
        let mut records = Vec::new();
        for (key, book) in &state.books {
            for alert in order_reads::alerts_of(key, book, read) {
                records.push((format!("{key}\u{1f}{}", alert.alert_id), alert));
            }
        }
        order_reads::keyed_page(records, &read.cursor, read.limit, state.heads.clone())
    }

    fn blocks_naming(&self, member_order_id: &str) -> Result<Vec<(String, String)>> {
        let state = self.state();
        let mut out = Vec::new();
        for (key, book) in &state.books {
            for (id, held) in &book.ledger.orders {
                if crate::booking::joined_members(&held.order)
                    .iter()
                    .any(|member| member == member_order_id)
                    && book.ledger.order_live(id)?
                {
                    out.push((key.clone(), id.clone()));
                }
            }
        }
        Ok(out)
    }

    fn live_order_instruments(&self) -> Result<Vec<String>> {
        let state = self.state();
        let mut out = Vec::new();
        for (key, book) in &state.books {
            if !is_account(key) {
                continue;
            }
            for (id, held) in &book.ledger.orders {
                if book.ledger.order_live(id)? {
                    out.push(held.order.instrument_id.clone());
                }
            }
        }
        Ok(out)
    }

    fn currency_codes(&self) -> Result<Vec<String>> {
        use meridian_domain::money::{codes, is_iso4217};
        let state = self.state();
        let mut found = std::collections::BTreeSet::new();
        for book in state.books.values() {
            for figures in book.figures.values() {
                found.extend(codes(figures));
            }
            for held in book.breaks.values() {
                found.extend(codes(held));
            }
            if let Some(attributes) = &book.attributes {
                if is_iso4217(&attributes.base_currency_code) {
                    found.insert(attributes.base_currency_code.clone());
                }
            }
        }
        Ok(found.into_iter().collect())
    }
}

/// The instrument an open break names, where it names one.
pub(crate) fn open_break_instrument(record: &Break) -> Option<&str> {
    if record.state != meridian_domain::v1::BreakState::Open as i32 {
        return None;
    }
    match record.subject.as_ref()? {
        meridian_domain::v1::r#break::Subject::Position(key) => Some(key.instrument_id.as_str()),
        meridian_domain::v1::r#break::Subject::Figure(key) => {
            (!key.instrument_id.is_empty()).then_some(key.instrument_id.as_str())
        }
    }
}

fn names(record: &Break, instrument_id: &str) -> bool {
    open_break_instrument(record) == Some(instrument_id)
}
