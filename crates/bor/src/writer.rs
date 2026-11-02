//! What the book's writer does after each commit (step 3a): the entry on its
//! partition's change log's tail, published after commit and at most once.
//!
//! A store wrapped, so no command path can commit without it: every path
//! reaches the journal through [`Store::act`].

use std::sync::Arc;

use meridian_bus::Bus;

use meridian_domain::v1::{AccountAttributes, AccountFigures, BookPosition, Break};

use crate::journal::Entry;
use crate::store::{
    Acted, AlertsRead, AttributesRead, BookingsRead, BreaksRead, Decide, FiguresRead, HistoryRead,
    InstructionsRead, LocatesRead, Mark, OrdersPage, OrdersRead, Page, PositionsRead, Record,
    Result, Store,
};

/// The book's change log's domain in its component topics.
pub const DOMAIN: &str = "book";

/// A store whose every commit is told on its partition's tail.
pub struct Writer {
    inner: Arc<dyn Store>,
    bus: Arc<Bus>,
}

impl Writer {
    pub fn new(inner: Arc<dyn Store>, bus: Arc<Bus>) -> Self {
        Self { inner, bus }
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
            let change = decided
                .entry
                .to_change(self.inner.epoch(), meridian_changes::origin());
            meridian_changes::publish(&self.bus, DOMAIN, &[change]);
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

    fn take_over(&self) -> Result<u64> {
        self.inner.take_over()
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
        self.inner.book(account_id)
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
        self.inner.rebuild()
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
