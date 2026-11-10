//! A reader member's book (contract v30, step 3c stage B;
//! spec/a-deployment-scales-out-and-fails-over-by-configuration sections 1
//! and 5; the product owner's rulings of 2026-11-10).
//!
//! - **Books at their head** held in memory, as the writer's are
//!   ([`crate::writer`]): loaded from the primary, and each account's
//!   forgotten as the writer's change log names a new entry of it, so no
//!   book held is older than what the member has applied. A book loaded while
//!   one was forgotten is answered and not held.
//! - **The record's past** -- a book as of a time, a page by business date or
//!   at a watermark, an order's history, a journal -- from the read replica
//!   (`bor.database.readExistingSecret`) where one is set and has reached the
//!   read's `read_after`; from the primary otherwise.
//! - **Past its wait**, a read still behind its `read_after` is answered from
//!   the primary, every part of it.
//! - **Nothing written**: every act is the writer's.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use meridian_bus::read_after::{answering_after, answering_from_primary, behind};
use meridian_domain::v1::{AccountAttributes, AccountFigures, BookPosition, Break};

use crate::journal::Entry;
use crate::store::{
    Acted, AlertsRead, AttributesRead, BookingsRead, BreaksRead, Decide, FiguresRead, HistoryRead,
    InstructionsRead, LocatesRead, Mark, OrdersPage, OrdersRead, Page, PositionsRead, Record,
    Result, Store, StoreError,
};
use crate::writer::Books;

/// A reader member's book: its books held over the primary, the primary,
/// and the read replica where one is set.
pub struct ReaderStore {
    primary: Arc<dyn Store>,
    replica: Option<Arc<dyn Store>>,
    books: Mutex<Books>,
    bound: usize,
    /// Moved as each book is forgotten: a load begun before is not held.
    forgotten: AtomicU64,
}

impl ReaderStore {
    pub fn new(primary: Arc<dyn Store>, replica: Option<Arc<dyn Store>>, bound: usize) -> Self {
        Self {
            primary,
            replica,
            books: Mutex::new(Books::default()),
            bound,
            forgotten: AtomicU64::new(0),
        }
    }

    fn books(&self) -> std::sync::MutexGuard<'_, Books> {
        self.books.lock().expect("the reader's books are poisoned")
    }

    /// How many books it holds, and their bytes.
    pub fn held(&self) -> (usize, usize) {
        self.books().held()
    }

    /// Changes the writer committed to a partition, in order: each entry's
    /// account's book forgotten, loaded again when next read. A change that
    /// is not an entry forgets every book.
    pub fn follow(&self, changes: &[meridian_changes::Change]) {
        let mut books = self.books();
        for change in changes {
            match Entry::from_change(change) {
                Ok(entry) => books.forget(&entry.account_id),
                Err(_) if change.partition == meridian_changes::CONTROL => {}
                Err(_) => *books = Books::default(),
            }
        }
        self.forgotten.fetch_add(1, Ordering::SeqCst);
    }

    /// Every book dropped: a partition whose changes it did not follow.
    pub fn forget_all(&self) {
        *self.books() = Books::default();
        self.forgotten.fetch_add(1, Ordering::SeqCst);
    }

    /// Where a read of now is answered: the primary (past the read's wait,
    /// or for what is not held in memory).
    fn now(&self) -> &dyn Store {
        self.primary.as_ref()
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

fn reached(replica: &dyn Store) -> bool {
    let Some(asked) = answering_after() else {
        return true;
    };
    match replica.heads() {
        Ok(heads) => behind(&asked, &heads.into_iter().collect()).is_empty(),
        Err(failed) => {
            tracing::warn!(%failed, "the read replica could not be read; the primary answers");
            false
        }
    }
}

fn acted() -> StoreError {
    StoreError::Unavailable("a reader member acts on nothing: the writer does".into())
}

/// Whether a positions page reads the record's past: by business date, or at
/// a watermark.
fn past_positions(read: &PositionsRead) -> bool {
    read.at.is_some() || !read.business_date.is_empty()
}

impl Store for ReaderStore {
    fn act(&self, _: &str, _: &str, _: &str, _: &[u8], _: &mut Decide<'_>) -> Result<Acted> {
        Err(acted())
    }

    fn journal(&self, account_id: &str) -> Result<Vec<Entry>> {
        self.past().journal(account_id)
    }

    fn adopt(&self) -> Result<u64> {
        Err(acted())
    }

    fn epoch(&self) -> u64 {
        0
    }

    fn take_over(&self, _: &meridian_changes::TookOver) -> Result<u64> {
        Err(acted())
    }

    fn head_epoch(&self) -> Result<u64> {
        self.primary.head_epoch()
    }

    fn read_changes(
        &self,
        partition: &str,
        after: u64,
        most_bytes: usize,
    ) -> std::result::Result<meridian_changes::ReadChangesReply, meridian_changes::ReadError> {
        self.primary.read_changes(partition, after, most_bytes)
    }

    fn bodies(
        &self,
        account_id: &str,
        entry_ids: &[String],
    ) -> Result<Vec<(String, crate::journal::Body)>> {
        self.past().bodies(account_id, entry_ids)
    }

    fn book(&self, account_id: &str) -> Result<crate::book::Book> {
        if answering_from_primary() {
            return self.primary.book(account_id);
        }
        if let Some(held) = self.books().get(account_id) {
            return Ok(held);
        }
        let before = self.forgotten.load(Ordering::SeqCst);
        let book = self.primary.book(account_id)?;
        let mut books = self.books();
        if self.forgotten.load(Ordering::SeqCst) == before {
            books.put(book.clone(), self.bound);
        }
        Ok(book)
    }

    fn book_health(
        &self,
        only: Option<&str>,
        after: &str,
        limit: usize,
    ) -> Result<Vec<meridian_domain::v1::AccountBookHealth>> {
        self.now().book_health(only, after, limit)
    }

    fn book_as_of(
        &self,
        account_id: &str,
        at: &Mark,
        until: Option<&str>,
    ) -> Result<crate::book::Book> {
        self.past().book_as_of(account_id, at, until)
    }

    fn accounts(&self) -> Result<Vec<String>> {
        self.now().accounts()
    }

    fn heads(&self) -> Result<Mark> {
        self.now().heads()
    }

    fn accounts_holding(&self, instrument_id: &str) -> Result<Vec<String>> {
        self.now().accounts_holding(instrument_id)
    }

    fn instruments_held(&self) -> Result<Vec<String>> {
        self.now().instruments_held()
    }

    fn positions(&self, read: &PositionsRead) -> Result<Page<BookPosition>> {
        if past_positions(read) {
            self.past().positions(read)
        } else {
            self.now().positions(read)
        }
    }

    fn breaks(&self, read: &BreaksRead) -> Result<Page<Break>> {
        self.now().breaks(read)
    }

    fn figures(&self, read: &FiguresRead) -> Result<Page<AccountFigures>> {
        self.now().figures(read)
    }

    fn attributes(&self, read: &AttributesRead) -> Result<Page<AccountAttributes>> {
        self.now().attributes(read)
    }

    fn rebuild(&self) -> Result<usize> {
        Err(acted())
    }

    fn cash_instruments(&self) -> Result<Vec<meridian_domain::money::Resolution>> {
        self.now().cash_instruments()
    }

    fn keep_cash_instrument(&self, _: &meridian_domain::money::Resolution) -> Result<()> {
        Err(acted())
    }

    fn currency_codes(&self) -> Result<Vec<String>> {
        self.now().currency_codes()
    }

    fn owner(&self, record: Record, id: &str) -> Result<Option<String>> {
        self.now().owner(record, id)
    }

    fn orders(&self, read: &OrdersRead) -> Result<OrdersPage> {
        self.now().orders(read)
    }

    fn order_history(&self, read: &HistoryRead) -> Result<Page<meridian_domain::v1::EntryMeta>> {
        self.past().order_history(read)
    }

    fn locates(&self, read: &LocatesRead) -> Result<Page<meridian_domain::v1::Locate>> {
        self.now().locates(read)
    }

    fn instructions(
        &self,
        read: &InstructionsRead,
    ) -> Result<Page<meridian_domain::v1::TradingInstruction>> {
        self.now().instructions(read)
    }

    fn expiring(&self, now_ns: i64) -> Result<Vec<(String, String)>> {
        self.now().expiring(now_ns)
    }

    fn bookings(&self, read: &BookingsRead) -> Result<Page<meridian_domain::v1::Booking>> {
        self.now().bookings(read)
    }

    fn alerts(&self, read: &AlertsRead) -> Result<Page<meridian_domain::v1::Alert>> {
        self.now().alerts(read)
    }

    fn blocks_naming(&self, member_order_id: &str) -> Result<Vec<(String, String)>> {
        self.now().blocks_naming(member_order_id)
    }

    fn live_order_instruments(&self) -> Result<Vec<String>> {
        self.now().live_order_instruments()
    }
}
