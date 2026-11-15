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
//! - **Its reply's watermark** is the position it was answered at (section 5,
//!   approved 2026-11-10): a page read from the primary or the replica says
//!   that store's own; a read from the books it holds, the member's applied
//!   position, never the primary's heads; a read the primary answered any
//!   part of -- a book not held, loaded from it, or the record's past with no
//!   replica reached -- or past its wait, the primary's.
//! - **A replica's answer is stamped from its own snapshot** (contract v30,
//!   stage C2; the resilience spec's section 5): the record's past read on
//!   the replica in one repeatable-read snapshot, its partitions' head rows
//!   read in the same, and the answer held to them -- taken where they have
//!   reached the read's `read_after` (and the watermark it is read at),
//!   stamped with them; answered by the primary where not. Exact, with no
//!   write position mapped: the head rows replicate with the entries.
//! - **Nothing written**: every act is the writer's.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use meridian_bus::read_after::{
    answered_by_primary, answering_after, answering_from_primary, behind, primary_answered_part,
    this_read,
};
use meridian_domain::v1::{AccountAttributes, AccountFigures, BookPosition, Break};

use crate::journal::Entry;
use crate::store::{
    Acted, AlertsRead, AttributesRead, BookingsRead, BreaksRead, Decide, FiguresRead, HistoryRead,
    InstructionsRead, LocatesRead, Mark, OrdersPage, OrdersRead, Page, PositionsRead, Record,
    Result, Store, StoreError,
};
use crate::writer::Books;

/// A reader member's applied position: each partition's head as far as it
/// has applied the writer's change log.
pub type Applied = Arc<dyn Fn() -> Mark + Send + Sync>;

/// A reader member's book: its books held over the primary, the primary,
/// the read replica where one is set, and the position it has applied to.
pub struct ReaderStore {
    primary: Arc<dyn Store>,
    replica: Option<Arc<dyn Store>>,
    books: Mutex<Books>,
    bound: usize,
    /// Moved as each book is forgotten: a load begun before is not held.
    forgotten: AtomicU64,
    applied: Applied,
}

impl ReaderStore {
    pub fn new(
        primary: Arc<dyn Store>,
        replica: Option<Arc<dyn Store>>,
        bound: usize,
        applied: Applied,
    ) -> Self {
        Self {
            primary,
            replica,
            books: Mutex::new(Books::default()),
            bound,
            forgotten: AtomicU64::new(0),
            applied,
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

    /// A read of the record's past (`read` asked of a store): from the read
    /// replica, where set, in one snapshot whose head rows have reached the
    /// read's `read_after` and `at` (a watermark it is read at), stamped with
    /// them; from the primary otherwise, which the read's watermark then
    /// says (contract v30, stage C2).
    fn past<T>(&self, at: Option<&Mark>, read: impl Fn(&dyn Store) -> Result<T>) -> Result<T> {
        if let Some(replica) = self.replica.as_ref().filter(|_| !answering_from_primary()) {
            crate::postgres::stamp_from_snapshot();
            let answered = read(replica.as_ref());
            let snapshot = crate::postgres::snapshot_heads();
            match answered {
                Ok(answer) => {
                    // A store that keeps no snapshot -- in memory, as a
                    // test's -- says its heads after the read.
                    match snapshot.map_or_else(|| replica.heads(), Ok) {
                        Ok(heads) if reached(&heads, at) => {
                            REPLICA_STAMP
                                .with(|kept| *kept.borrow_mut() = Some((this_read(), heads)));
                            return Ok(answer);
                        }
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
        primary_answered_part();
        read(self.primary.as_ref())
    }
}

thread_local! {
    /// The heads of the replica's snapshot the read this thread answers was
    /// answered from, with the read they are of ([`this_read`]).
    static REPLICA_STAMP: std::cell::RefCell<Option<(u64, Mark)>> = const { std::cell::RefCell::new(None) };
}

/// The replica's snapshot this read was answered from, where it was.
fn replica_stamp() -> Option<Mark> {
    REPLICA_STAMP.with(|kept| {
        kept.borrow()
            .as_ref()
            .filter(|(read, _)| *read == this_read())
            .map(|(_, heads)| heads.clone())
    })
}

/// Whether a replica's snapshot at `heads` has reached the read's
/// `read_after`, and `at` where the read is at a watermark.
fn reached(heads: &Mark, at: Option<&Mark>) -> bool {
    let heads: std::collections::HashMap<String, u64> = heads
        .iter()
        .map(|(partition, head)| (partition.clone(), *head))
        .collect();
    let after = answering_after().is_none_or(|asked| behind(&asked, &heads).is_empty());
    let at_reached = at.is_none_or(|at| {
        at.iter()
            .all(|(partition, wanted)| heads.get(partition).copied().unwrap_or(0) >= *wanted)
    });
    after && at_reached
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
        self.past(None, |store| store.journal(account_id))
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
        self.past(None, |store| store.bodies(account_id, entry_ids))
    }

    fn book(&self, account_id: &str) -> Result<crate::book::Book> {
        if answering_from_primary() {
            return self.primary.book(account_id);
        }
        if let Some(held) = self.books().get(account_id) {
            return Ok(held);
        }
        // Not held: the primary's book, and the read's watermark the
        // primary's.
        primary_answered_part();
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
        self.past(Some(at), |store| store.book_as_of(account_id, at, until))
    }

    fn accounts(&self) -> Result<Vec<String>> {
        self.now().accounts()
    }

    /// The position a read was answered at, where the read says its own
    /// (an account's lifecycle events, read from the books it holds): what
    /// this member has applied -- each book it holds at or past it -- and
    /// the primary's where the primary answered any of it, a book not held
    /// or past the read's wait. A page read from a store carries that
    /// store's own.
    fn heads(&self) -> Result<Mark> {
        if answered_by_primary() {
            return self.primary.heads();
        }
        // The record's past answered by the replica: its snapshot's heads.
        if let Some(stamped) = replica_stamp() {
            return Ok(stamped);
        }
        Ok((self.applied)())
    }

    fn accounts_holding(&self, instrument_id: &str) -> Result<Vec<String>> {
        self.now().accounts_holding(instrument_id)
    }

    fn instruments_held(&self) -> Result<Vec<String>> {
        self.now().instruments_held()
    }

    fn positions(&self, read: &PositionsRead) -> Result<Page<BookPosition>> {
        if past_positions(read) {
            self.past(read.at.as_ref(), |store| store.positions(read))
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
        self.past(None, |store| store.order_history(read))
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
