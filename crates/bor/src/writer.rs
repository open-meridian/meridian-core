//! What the book's writer does after each commit (step 3a): the entry on its
//! partition's change log's tail, published after commit and at most once,
//! and each account's book at its head held in memory -- the book's hot
//! layer (spec/one-data-access-model section 3).
//!
//! A store wrapped, so no command path can commit without it: every path
//! reaches the journal through [`Store::act`].
//!
//! **Books at their head.** `book(account)` -- what the order path, the
//! booking path and the reads ask before deciding -- is answered from memory
//! once the account's book is held: the book the commit made, which is the
//! journal replayed to that entry, the same the record builds from its
//! snapshot and tail. A book loaded from the record is held only if no
//! later commit of the account was held meanwhile (each book carries the
//! sequence of its last entry). Bounded by `bor.hotLayer.booksMiB`, the
//! least recently read evicted first; a rebuild empties it. Any other read
//! -- as of a time, a page of positions -- is the record's.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use meridian_bus::Bus;

use meridian_domain::v1::{AccountAttributes, AccountFigures, BookPosition, Break};

use crate::book::{Bodies, Book};
use crate::journal::Entry;
use crate::store::{
    Acted, AlertsRead, AttributesRead, BookingsRead, BreaksRead, Decide, FiguresRead, HistoryRead,
    InstructionsRead, LocatesRead, Mark, OrdersPage, OrdersRead, Page, PositionsRead, Record,
    Result, Store,
};

/// The book's change log's domain in its component topics.
pub const DOMAIN: &str = "book";

/// The default bound: 128 MiB of books, within the book's 512 MiB.
pub const DEFAULT_BOOKS_BYTES: usize = 128 * 1024 * 1024;

/// The bound from the chart's value, MiB; the default where unset.
pub fn books_bytes_from_mib(said: Option<&str>) -> std::result::Result<usize, String> {
    match said {
        None | Some("") => Ok(DEFAULT_BOOKS_BYTES),
        Some(text) => text
            .parse::<usize>()
            .ok()
            .filter(|mib| *mib > 0 && *mib <= 4096)
            .map(|mib| mib * 1024 * 1024)
            .ok_or_else(|| {
                format!("bor.hotLayer.booksMiB is {text:?}; a whole number of MiB from 1 to 4096")
            }),
    }
}

/// The sequence of a book's last entry: how far into its journal it is.
fn reached(book: &Book) -> u64 {
    book.entries.last().map_or(0, |entry| entry.first.sequence)
}

/// What a book costs held, roughly.
fn cost(book: &Book) -> usize {
    1_024
        + book.positions.len() * 640
        + book.entries.len() * 160
        + book.breaks.len() * 512
        + book.figures.len() * 512
        + book.lifecycle_events.len() * 512
        + (book.ledger.orders.len()
            + book.ledger.placements.len()
            + book.ledger.executions.len()
            + book.ledger.instructions.len())
            * 768
}

#[derive(Default)]
pub(crate) struct Books {
    held: BTreeMap<String, (Book, usize, u64)>,
    by_use: BTreeMap<u64, String>,
    bytes: usize,
    tick: u64,
}

impl Books {
    pub(crate) fn put(&mut self, mut book: Book, bound: usize) {
        // A book held answers as the record's does: no entry's body in it.
        book.bodies = Bodies::default();
        let account = book.account_id.clone();
        if let Some((held, _, _)) = self.held.get(&account) {
            if reached(held) > reached(&book) {
                return;
            }
        }
        self.forget(&account);
        let size = cost(&book);
        self.tick += 1;
        self.bytes += size;
        self.by_use.insert(self.tick, account.clone());
        self.held.insert(account, (book, size, self.tick));
        while self.bytes > bound {
            let Some((_, oldest)) = self.by_use.pop_first() else {
                break;
            };
            if let Some((_, size, _)) = self.held.remove(&oldest) {
                self.bytes -= size;
            }
        }
    }

    pub(crate) fn get(&mut self, account: &str) -> Option<Book> {
        let (book, _, used) = self.held.get(account)?;
        let (book, used) = (book.clone(), *used);
        self.by_use.remove(&used);
        self.tick += 1;
        self.by_use.insert(self.tick, account.to_string());
        if let Some(held) = self.held.get_mut(account) {
            held.2 = self.tick;
        }
        Some(book)
    }

    /// How many books it holds, and their bytes.
    pub(crate) fn held(&self) -> (usize, usize) {
        (self.held.len(), self.bytes)
    }

    pub(crate) fn forget(&mut self, account: &str) {
        if let Some((_, size, used)) = self.held.remove(account) {
            self.bytes -= size;
            self.by_use.remove(&used);
        }
    }
}

/// A store whose every commit is told on its partition's tail, and whose
/// accounts' books at their head are held in memory.
pub struct Writer {
    inner: Arc<dyn Store>,
    bus: Arc<Bus>,
    books: Mutex<Books>,
    bound: usize,
}

impl Writer {
    pub fn new(inner: Arc<dyn Store>, bus: Arc<Bus>) -> Self {
        Self::bounded(inner, bus, DEFAULT_BOOKS_BYTES)
    }

    /// The same, its books bounded by `bound` bytes.
    pub fn bounded(inner: Arc<dyn Store>, bus: Arc<Bus>, bound: usize) -> Self {
        Self {
            inner,
            bus,
            books: Mutex::new(Books::default()),
            bound,
        }
    }

    fn books(&self) -> std::sync::MutexGuard<'_, Books> {
        self.books.lock().expect("the book's hot layer is poisoned")
    }

    /// How many books it holds, and their bytes.
    pub fn held(&self) -> (usize, usize) {
        let books = self.books();
        (books.held.len(), books.bytes)
    }
}

impl Store for Writer {
    fn act(
        &self,
        account_id: &str,
        message_id: &str,
        idempotency_key: &str,
        request: &[u8],
        decide: &mut Decide<'_>,
    ) -> Result<Acted> {
        let acted = self
            .inner
            .act(account_id, message_id, idempotency_key, request, decide)?;
        if let Acted::Committed(decided) = &acted {
            self.books().put(decided.book.clone(), self.bound);
            let change = decided
                .entry
                .to_change(self.inner.epoch(), meridian_changes::origin());
            let after = decided.entry.first_sequence.saturating_sub(1);
            meridian_changes::publish(&self.bus, DOMAIN, after, &[change]);
        }
        Ok(acted)
    }

    fn journal(&self, account_id: &str) -> Result<Vec<Entry>> {
        self.inner.journal(account_id)
    }

    fn adopt(&self) -> Result<u64> {
        self.inner.adopt()
    }

    fn epoch(&self) -> u64 {
        self.inner.epoch()
    }

    fn take_over(&self, by: &meridian_changes::TookOver) -> Result<u64> {
        self.inner.take_over(by)
    }

    fn head_epoch(&self) -> Result<u64> {
        self.inner.head_epoch()
    }

    fn read_changes(
        &self,
        partition: &str,
        after: u64,
        most_bytes: usize,
    ) -> std::result::Result<meridian_changes::ReadChangesReply, meridian_changes::ReadError> {
        self.inner.read_changes(partition, after, most_bytes)
    }

    fn bodies(
        &self,
        account_id: &str,
        entry_ids: &[String],
    ) -> Result<Vec<(String, crate::journal::Body)>> {
        self.inner.bodies(account_id, entry_ids)
    }

    fn book(&self, account_id: &str) -> Result<crate::book::Book> {
        if let Some(held) = self.books().get(account_id) {
            return Ok(held);
        }
        let book = self.inner.book(account_id)?;
        self.books().put(book.clone(), self.bound);
        Ok(book)
    }

    fn book_health(
        &self,
        only: Option<&str>,
        after: &str,
        limit: usize,
    ) -> Result<Vec<meridian_domain::v1::AccountBookHealth>> {
        self.inner.book_health(only, after, limit)
    }

    fn book_as_of(
        &self,
        account_id: &str,
        at: &Mark,
        until: Option<&str>,
    ) -> Result<crate::book::Book> {
        self.inner.book_as_of(account_id, at, until)
    }

    fn accounts(&self) -> Result<Vec<String>> {
        self.inner.accounts()
    }

    fn heads(&self) -> Result<Mark> {
        self.inner.heads()
    }

    fn accounts_holding(&self, instrument_id: &str) -> Result<Vec<String>> {
        self.inner.accounts_holding(instrument_id)
    }

    fn instruments_held(&self) -> Result<Vec<String>> {
        self.inner.instruments_held()
    }

    fn positions(&self, read: &PositionsRead) -> Result<Page<BookPosition>> {
        self.inner.positions(read)
    }

    fn breaks(&self, read: &BreaksRead) -> Result<Page<Break>> {
        self.inner.breaks(read)
    }

    fn figures(&self, read: &FiguresRead) -> Result<Page<AccountFigures>> {
        self.inner.figures(read)
    }

    fn attributes(&self, read: &AttributesRead) -> Result<Page<AccountAttributes>> {
        self.inner.attributes(read)
    }

    fn rebuild(&self) -> Result<usize> {
        let rebuilt = self.inner.rebuild();
        *self.books() = Books::default();
        rebuilt
    }

    fn cash_instruments(&self) -> Result<Vec<meridian_domain::money::Resolution>> {
        self.inner.cash_instruments()
    }

    fn keep_cash_instrument(&self, resolution: &meridian_domain::money::Resolution) -> Result<()> {
        self.inner.keep_cash_instrument(resolution)
    }

    fn currency_codes(&self) -> Result<Vec<String>> {
        self.inner.currency_codes()
    }

    fn owner(&self, record: Record, id: &str) -> Result<Option<String>> {
        self.inner.owner(record, id)
    }

    fn orders(&self, read: &OrdersRead) -> Result<OrdersPage> {
        self.inner.orders(read)
    }

    fn order_history(&self, read: &HistoryRead) -> Result<Page<meridian_domain::v1::EntryMeta>> {
        self.inner.order_history(read)
    }

    fn locates(&self, read: &LocatesRead) -> Result<Page<meridian_domain::v1::Locate>> {
        self.inner.locates(read)
    }

    fn instructions(
        &self,
        read: &InstructionsRead,
    ) -> Result<Page<meridian_domain::v1::TradingInstruction>> {
        self.inner.instructions(read)
    }

    fn expiring(&self, now_ns: i64) -> Result<Vec<(String, String)>> {
        self.inner.expiring(now_ns)
    }

    fn bookings(&self, read: &BookingsRead) -> Result<Page<meridian_domain::v1::Booking>> {
        self.inner.bookings(read)
    }

    fn alerts(&self, read: &AlertsRead) -> Result<Page<meridian_domain::v1::Alert>> {
        self.inner.alerts(read)
    }

    fn blocks_naming(&self, member_order_id: &str) -> Result<Vec<(String, String)>> {
        self.inner.blocks_naming(member_order_id)
    }

    fn live_order_instruments(&self) -> Result<Vec<String>> {
        self.inner.live_order_instruments()
    }
}

/// The book's change log, read from its store.
pub struct Log(pub Arc<dyn Store>);

impl meridian_changes::ChangeLog for Log {
    fn read_changes(
        &self,
        partition: &str,
        after: u64,
        most_bytes: usize,
    ) -> std::result::Result<meridian_changes::ReadChangesReply, meridian_changes::ReadError> {
        self.0.read_changes(partition, after, most_bytes)
    }
}
