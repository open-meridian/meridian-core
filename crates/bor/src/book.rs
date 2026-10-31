//! One account's book, as its journal replays it.
//!
//! A position is its account's opening balance plus every entry of the book's
//! own since, each a set of movement lines, and nothing replaces it (W9's
//! second invariant). So the projection here only ever sums lines: it never
//! reads an entry's kind, which is an open list a reader takes as data
//! (question 19). Lines move a position's settlement buckets and, where they
//! name one, its lots; every other record an entry changes it sets whole.
//!
//! # Every change numbered
//!
//! Each record an entry changes takes the partition's next number, in a fixed
//! order -- positions by instrument and side, then breaks, figures and the
//! account's attributes as the entry lists them -- and the entry's own number
//! is the first. Each names the previous change the same row made for the
//! account, so a reader hearing only some accounts, or only some records of
//! one entry, tells a gap in its own (W9.8, W4.3). Replaying an entry gives
//! every record the number it had when the entry was made, which is what lets
//! `meridian-bor rebuild` reproduce the projections and their chains exactly.

use std::collections::{BTreeMap, BTreeSet};

use meridian_domain::exact::Exact;
use meridian_domain::v1::{
    AccountAttributes, AccountFigures, BookPosition, Break, Encumbrance, Execution, FreeBasis,
    HoldingSide, JournalRef, Locate, Lot, LotRelief, MarginAgreementRef, Money, MovementLine,
    OpeningSource, Order, PendingSettlement, PendingState, Placement, SettlementBucket,
    TradingInstruction, UnplacedFact,
};
use meridian_pb::v1::RefusalReason;

use crate::journal::{Body, Entry};
use crate::numbers;
use crate::store::{Result, StoreError};

/// The rows an account's changes are chained per (W9.8): one each, so a
/// reader hearing positions sees no false gap from a break.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chain {
    Position = 0,
    Break = 1,
    Figures = 2,
    Attributes = 3,
    // The order path's rows (W11.13, contract v22), each chained of its own.
    Order = 4,
    Placement = 5,
    Execution = 6,
    Unplaced = 7,
    Locate = 8,
    Instruction = 9,
    // Booking's rows (W11.30, W11.35; contract v24), each chained of its own.
    Booking = 10,
    Alert = 11,
    /// The control partition's settings (W11.36).
    Setting = 12,
    /// Contract v28 (W9.16 to W9.19): lifecycle events.
    Lifecycle = 13,
}

/// How many rows an account's changes are chained per.
pub(crate) const CHAINS: usize = 14;

/// A position's key within its account: instrument, side and, from contract
/// v22, sleeve (W9's invariants; empty for the account's unsleeved part).
pub type Key = (String, i32, String);

/// A lot as the book holds it: the record, and the entry that opened it,
/// within which further lines add to its original quantity.
#[derive(Debug, Clone, PartialEq)]
pub struct LotState {
    pub lot: Lot,
    pub opened_in: u64,
    /// The order it was opened on its position in, which the record lists
    /// lots in: identifiers minted in one millisecond do not sort by it.
    pub ordinal: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PositionState {
    pub instrument_id: String,
    pub side: i32,
    /// Its sleeve within the account (contract v22): empty for the unsleeved
    /// part, as every position before v22 is.
    pub sleeve: String,
    pub settled: Exact,
    /// Pending by value date, "" for "date not stated" at an opening balance.
    pub pending: BTreeMap<String, (Exact, Option<PendingState>)>,
    pub not_stated: Exact,
    /// Every lot it has held, closed ones too: a reversal may reopen one.
    pub lots: BTreeMap<String, LotState>,
    pub opened_lots: u64,
    pub opened_from: Vec<OpeningSource>,
    pub effective_date: String,
    pub last_change: Option<JournalRef>,
    pub removed: bool,
    /// What of it cannot move, as last recorded from a statement (W9.15):
    /// an attribute, never a movement.
    pub encumbrances: Vec<Encumbrance>,
}

impl PositionState {
    fn new(instrument_id: &str, side: i32, sleeve: &str) -> Self {
        PositionState {
            instrument_id: instrument_id.to_string(),
            side,
            sleeve: sleeve.to_string(),
            settled: Exact::ZERO,
            pending: BTreeMap::new(),
            not_stated: Exact::ZERO,
            lots: BTreeMap::new(),
            opened_lots: 0,
            opened_from: Vec::new(),
            effective_date: String::new(),
            last_change: None,
            removed: false,
            encumbrances: Vec::new(),
        }
    }

    /// Free, derived (W9.15, Q2): the settled quantity less its
    /// encumbrances; unknown while any of the quantity is not stated.
    pub fn free(&self) -> Result<Option<Exact>> {
        if !self.not_stated.is_zero() {
            return Ok(None);
        }
        let mut free = self.settled;
        for encumbrance in &self.encumbrances {
            let quantity =
                numbers::required("encumbrances.quantity", encumbrance.quantity.as_ref())?;
            free = numbers::add("free_quantity", free, quantity.negated())?;
        }
        Ok(Some(free))
    }

    /// Settled, plus pending, plus not stated: by construction.
    pub fn trade_date(&self) -> Result<Exact> {
        let mut total = numbers::add("trade_date_quantity", self.settled, self.not_stated)?;
        for (quantity, _) in self.pending.values() {
            total = numbers::add("trade_date_quantity", total, *quantity)?;
        }
        Ok(total)
    }

    /// The open lots' quantities, summed.
    pub fn open_lots(&self) -> Result<Exact> {
        let mut total = Exact::ZERO;
        for state in self.lots.values() {
            let open = numbers::required("lots.open_quantity", state.lot.open_quantity.as_ref())?;
            total = numbers::add("lots.open_quantity", total, open)?;
        }
        Ok(total)
    }

    pub fn has_open_lots(&self) -> bool {
        self.lots
            .values()
            .any(|state| !is_zero(&state.lot.open_quantity))
    }

    /// Whether it holds nothing: every bucket zero and no lot open.
    pub fn flat(&self) -> bool {
        self.settled.is_zero()
            && self.not_stated.is_zero()
            && self
                .pending
                .values()
                .all(|(quantity, _)| quantity.is_zero())
            && !self.has_open_lots()
    }

    /// As it is read and delivered: open lots only, pending by value date.
    pub fn record(&self, account_id: &str) -> Result<BookPosition> {
        Ok(BookPosition {
            account_id: account_id.to_string(),
            instrument_id: self.instrument_id.clone(),
            side: self.side,
            // Keyed by its sleeve (contract v22; W9's invariants).
            sleeve: self.sleeve.clone(),
            trade_date_quantity: numbers::wire(self.trade_date()?),
            // Unknown, never zero, while any of the quantity is not stated.
            settled_quantity: self.not_stated.is_zero().then(|| self.settled.to_wire()),
            not_stated_quantity: numbers::wire(self.not_stated),
            pending: self
                .pending
                .iter()
                .filter(|(_, (quantity, _))| !quantity.is_zero())
                .map(|(value_date, (quantity, state))| PendingSettlement {
                    value_date: value_date.clone(),
                    quantity: numbers::wire(*quantity),
                    state: state.clone(),
                })
                .collect(),
            lots: {
                let mut open: Vec<&LotState> = self
                    .lots
                    .values()
                    .filter(|state| !is_zero(&state.lot.open_quantity))
                    .collect();
                open.sort_by_key(|state| state.ordinal);
                open.into_iter().map(|state| state.lot.clone()).collect()
            },
            opened_from: self.opened_from.clone(),
            effective_date: self.effective_date.clone(),
            last_change: self.last_change.clone(),
            removed: self.removed,
            encumbrances: self.encumbrances.clone(),
            free_quantity: self.free()?.map(numbers::wire).unwrap_or_default(),
            free_basis: FreeBasis::Settled as i32,
        })
    }

    fn add_to_bucket(&mut self, line: &MovementLine, quantity: Exact) -> Result<()> {
        match SettlementBucket::try_from(line.bucket) {
            Ok(SettlementBucket::Settled) => {
                self.settled = numbers::add("settled_quantity", self.settled, quantity)?;
            }
            Ok(SettlementBucket::Pending) => {
                let held = self
                    .pending
                    .entry(line.value_date.clone())
                    .or_insert((Exact::ZERO, None));
                held.0 = numbers::add("pending.quantity", held.0, quantity)?;
                if line.pending_state.is_some() {
                    held.1 = line.pending_state.clone();
                }
                if held.0.is_zero() {
                    self.pending.remove(&line.value_date);
                }
            }
            Ok(SettlementBucket::NotStated) => {
                self.not_stated = numbers::add("not_stated_quantity", self.not_stated, quantity)?;
            }
            _ => {
                return Err(StoreError::Invalid(format!(
                    "a line on {} names no settlement bucket",
                    line.instrument_id
                )))
            }
        }
        Ok(())
    }
}

fn is_zero(value: &Option<meridian_pb::v1::Decimal>) -> bool {
    value
        .as_ref()
        .and_then(|wire| Exact::from_wire(wire).ok())
        .map(Exact::is_zero)
        .unwrap_or(true)
}

/// An entry as the account's later acts need it: what entries a resolution
/// names, whether it has been reversed, and what a scan over later entries
/// asks of each. Its body -- what a reversal negates, a fill's lines -- is
/// the journal's, loaded when a decision asks for it ([`Book::body`];
/// contract v26, W9.10, ruling 165), so the book's memory stays flat as
/// journals grow.
#[derive(Debug, Clone, PartialEq)]
pub struct EntryIndex {
    pub entry_id: String,
    pub kind: String,
    pub effective_date: String,
    pub first: JournalRef,
    pub reversed_by: Option<String>,
    /// Whether it moves a position (`Body::moves_positions`): what a
    /// reversal of the opening balance is refused over while it stands.
    pub moves_positions: bool,
    /// The entry it reverses, empty for none.
    pub reverses: String,
    /// Whether it did anything on the order path: a value-date roll reads
    /// only such entries' bodies.
    pub acts: bool,
    /// Whether one of its acts is a value-date roll (W11.25), which moves a
    /// pending line an earlier entry left.
    pub rolls: bool,
}

impl EntryIndex {
    fn of(entry: &Entry, first: JournalRef) -> EntryIndex {
        let body = &entry.body;
        EntryIndex {
            entry_id: entry.entry_id.clone(),
            kind: entry.meta.kind.clone(),
            effective_date: entry.meta.effective_date.clone(),
            first,
            reversed_by: None,
            moves_positions: body.moves_positions(),
            reverses: body.reverses.clone(),
            acts: !body.acts.is_empty(),
            rolls: body
                .acts
                .iter()
                .any(|act| matches!(act, crate::orders::Act::ValueDateRolled { .. })),
        }
    }
}

/// The bodies of entries a decision has asked for, loaded from the journal
/// (contract v26, W9.10): each entry's body is immutable and named by its
/// entry, so however a book holds them -- every one after a replay in a
/// test, only those asked for after a store's -- it is the same book, and
/// two books compare equal whatever each holds.
#[derive(Clone, Default)]
pub struct Bodies(BTreeMap<String, std::sync::Arc<Body>>);

impl PartialEq for Bodies {
    fn eq(&self, _: &Bodies) -> bool {
        true
    }
}

impl std::fmt::Debug for Bodies {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} bodies held", self.0.len())
    }
}

impl Bodies {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The records an entry changed, as they stand after it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Changes {
    /// Each position with its trade-date quantity before the entry.
    pub positions: Vec<(BookPosition, Exact)>,
    pub breaks: Vec<Break>,
    pub figures: Vec<AccountFigures>,
    pub attributes: Option<AccountAttributes>,
    /// Contract v28: the lifecycle events the entry changed, whole.
    pub lifecycle_events: Vec<meridian_domain::v1::LifecycleEvent>,
    /// The order path's records the entry changed, whole, with what changed
    /// on an order or a placement (W11.13).
    pub orders: Vec<(Order, Vec<String>)>,
    pub placements: Vec<(Placement, Vec<String>)>,
    pub executions: Vec<Execution>,
    pub unplaced: Vec<UnplacedFact>,
    pub locates: Vec<Locate>,
    pub instructions: Vec<TradingInstruction>,
    /// Contract v24: the bookings and alerts the entry changed, whole.
    pub bookings: Vec<meridian_domain::v1::Booking>,
    pub alerts: Vec<meridian_domain::v1::Alert>,
    /// When each order changed may expire by the deployment's clock, None
    /// where it cannot or is ending already (W11.13).
    pub order_expiries: BTreeMap<String, Option<i64>>,
    /// The change requests the entry opened, each with its order and
    /// placement, and the entry itself, for the store's indexes.
    pub change_requests: Vec<(String, String, String)>,
    /// The entry's place: its first change's.
    pub first: JournalRef,
    pub last_sequence: u64,
}

/// The reference each order-path record takes in an entry.
struct ActRefs {
    orders: Vec<(String, JournalRef)>,
    placements: Vec<(String, JournalRef)>,
    executions: Vec<(String, JournalRef)>,
    unplaced: Vec<(String, JournalRef)>,
    locates: Vec<(String, JournalRef)>,
    instructions: Vec<(String, JournalRef)>,
    bookings: Vec<(String, JournalRef)>,
    alerts: Vec<(String, JournalRef)>,
}

/// One account's book.
#[derive(Debug, Clone, PartialEq)]
pub struct Book {
    pub account_id: String,
    pub partition: String,
    pub positions: BTreeMap<Key, PositionState>,
    pub breaks: BTreeMap<String, Break>,
    pub figures: BTreeMap<(String, String), AccountFigures>,
    pub attributes: Option<AccountAttributes>,
    /// Contract v28 (W9.16 to W9.19): each lifecycle event by its `LCE-` ID.
    pub lifecycle_events: BTreeMap<String, meridian_domain::v1::LifecycleEvent>,
    /// The standing opening balance: its entry and D0.
    pub opening: Option<(String, String)>,
    /// Opening balances reversed, in order.
    pub reversed_openings: Vec<String>,
    pub entries: Vec<EntryIndex>,
    pub(crate) chains: [u64; CHAINS],
    /// The order path's records (W11, contract v22).
    pub ledger: crate::orders::Ledger,
    /// Whether an entry has stated whether releases wait for compliance
    /// (contract v22): false for an account the book held before, until the
    /// upgrade's backfill records it (decisions/031).
    pub waits_stated: bool,
    /// The entries' bodies it holds (contract v26, W9.10): none in a
    /// snapshot, and after a store's build only those a decision asked for.
    pub bodies: Bodies,
}

/// How an agreement is keyed: the statement's external account and segment
/// (W9.5, Q12); the counterparty is as reported and keys nothing.
pub fn agreement_key(agreement: Option<&MarginAgreementRef>) -> String {
    use meridian_domain::v1::margin_agreement_ref::Agreement;
    match agreement.and_then(|held| held.agreement.as_ref()) {
        Some(Agreement::StatementSegment(segment)) => format!(
            "segment\u{1f}{}\u{1f}{}",
            segment.external_account_id, segment.segment
        ),
        None => String::new(),
    }
}

impl Book {
    pub fn new(account_id: &str, partition: &str) -> Self {
        Book {
            account_id: account_id.to_string(),
            partition: partition.to_string(),
            positions: BTreeMap::new(),
            breaks: BTreeMap::new(),
            figures: BTreeMap::new(),
            attributes: None,
            lifecycle_events: BTreeMap::new(),
            opening: None,
            reversed_openings: Vec::new(),
            entries: Vec::new(),
            chains: [0; CHAINS],
            ledger: crate::orders::Ledger::default(),
            waits_stated: false,
            bodies: Bodies::default(),
        }
    }

    /// An account replayed from its journal: strictly, so an entry that no
    /// longer applies is a fault rather than a guess. It holds every entry's
    /// body, the journal being in hand; a store's build holds none
    /// ([`Book::replay_lean`]) and loads what a decision asks for.
    pub fn replay(account_id: &str, partition: &str, entries: &[Entry]) -> Result<Book> {
        let mut book = Book::replay_lean(account_id, partition, entries)?;
        book.hold_bodies(
            entries
                .iter()
                .map(|entry| (entry.entry_id.clone(), entry.body.clone())),
        );
        Ok(book)
    }

    /// [`Book::replay`], holding no entry's body (contract v26, W9.10).
    pub fn replay_lean(account_id: &str, partition: &str, entries: &[Entry]) -> Result<Book> {
        let mut book = Book::new(account_id, partition);
        for entry in entries {
            book.apply(entry, false)?;
        }
        Ok(book)
    }

    /// The positions at the end of a business date as known at a watermark
    /// (Q22): the entries numbered at or below `at`, those effective on or
    /// before `until` alone, leniently, since an entry left out may have
    /// opened a lot a later one names.
    pub fn as_of(
        account_id: &str,
        partition: &str,
        entries: &[Entry],
        at: Option<u64>,
        until: Option<&str>,
    ) -> Result<Book> {
        let mut book = Book::new(account_id, partition);
        for entry in entries {
            if at.is_some_and(|at| entry.first_sequence > at) {
                break;
            }
            if until.is_some_and(|until| entry.meta.effective_date.as_str() > until) {
                continue;
            }
            book.apply(entry, true)?;
        }
        Ok(book)
    }

    /// [`Book::as_of`] from a book already holding the entries up to some
    /// point -- a snapshot (contract v25, W9.10) -- and the entries after it.
    pub fn resume_as_of(
        mut self,
        entries: &[Entry],
        at: Option<u64>,
        until: Option<&str>,
    ) -> Result<Book> {
        for entry in entries {
            if at.is_some_and(|at| entry.first_sequence > at) {
                break;
            }
            if until.is_some_and(|until| entry.meta.effective_date.as_str() > until) {
                continue;
            }
            self.apply(entry, true)?;
        }
        Ok(self)
    }

    pub fn entry(&self, entry_id: &str) -> Option<&EntryIndex> {
        self.entries.iter().find(|held| held.entry_id == entry_id)
    }

    /// An entry's body, as the journal holds it (contract v26, W9.10): where
    /// this book does not hold it, [`StoreError::BodiesNeeded`] naming it,
    /// which the store answers by loading it and deciding again
    /// ([`crate::store::with_bodies`]).
    pub fn body(&self, entry_id: &str) -> Result<&Body> {
        match self.bodies.0.get(entry_id) {
            Some(body) => Ok(body),
            None => Err(StoreError::BodiesNeeded(vec![entry_id.to_string()])),
        }
    }

    /// The bodies of several entries, in their order; every one this book
    /// does not hold named at once, so a scan loads them in one read.
    pub fn bodies_of<'a>(
        &self,
        entries: impl IntoIterator<Item = &'a EntryIndex>,
    ) -> Result<Vec<(&'a EntryIndex, &Body)>> {
        let mut found = Vec::new();
        let mut missing = Vec::new();
        for entry in entries {
            match self.bodies.0.get(&entry.entry_id) {
                Some(body) => found.push((entry, body.as_ref())),
                None => missing.push(entry.entry_id.clone()),
            }
        }
        if !missing.is_empty() {
            return Err(StoreError::BodiesNeeded(missing));
        }
        Ok(found)
    }

    /// Hold these bodies, loaded from the journal.
    pub fn hold_bodies(&mut self, loaded: impl IntoIterator<Item = (String, Body)>) {
        for (entry_id, body) in loaded {
            self.bodies.0.insert(entry_id, std::sync::Arc::new(body));
        }
    }

    /// Hold none: what a store's build leaves, the bodies a replay applied
    /// let go, so a book holds only what a fold needs.
    pub fn release_bodies(&mut self) {
        self.bodies.0.clear();
    }

    /// The position holding a lot, by the lot's identifier.
    pub fn lot_owner(&self, lot_id: &str) -> Option<&Key> {
        self.positions
            .iter()
            .find(|(_, position)| position.lots.contains_key(lot_id))
            .map(|(key, _)| key)
    }

    /// The keys of every position an entry's body changes.
    fn touched(&self, body: &Body) -> BTreeSet<Key> {
        let mut keys: BTreeSet<Key> = body
            .lines
            .iter()
            .map(|line| (line.instrument_id.clone(), line.side, line.sleeve.clone()))
            .collect();
        for adjustment in &body.basis_adjustments {
            if let Some(key) = self.lot_owner(&adjustment.lot_id) {
                keys.insert(key.clone());
            }
        }
        // A merged record's tombstones and a statement's encumbrances are
        // the account's, unsleeved (W9.9, W9.15).
        keys.extend(
            body.tombstones
                .iter()
                .map(|(instrument, side)| (instrument.clone(), *side, String::new())),
        );
        keys.extend(
            body.encumbrances
                .iter()
                .map(|(instrument, side, _)| (instrument.clone(), *side, String::new())),
        );
        keys
    }

    fn next_ref(&mut self, chain: Chain, sequence: u64) -> JournalRef {
        let previous = self.chains[chain as usize];
        self.chains[chain as usize] = sequence;
        JournalRef {
            partition: self.partition.clone(),
            sequence,
            previous_sequence: previous,
        }
    }

    /// Replay one entry onto the account, numbering each record it changes
    /// from the entry's first sequence; and say what changed. An entry being
    /// made has no last sequence yet, and this is where it gets one; an entry
    /// read back must have the one this gives it, or the journal is not what
    /// the book wrote.
    pub fn apply(&mut self, entry: &Entry, lenient: bool) -> Result<Changes> {
        let body = &entry.body;
        let keys = self.touched(body);
        let mut previous: BTreeMap<Key, Exact> = BTreeMap::new();
        for key in &keys {
            let before = match self.positions.get(key) {
                Some(position) => position.trade_date()?,
                None => Exact::ZERO,
            };
            previous.insert(key.clone(), before);
        }

        // Numbered in one fixed order, so a replay numbers as the act did.
        let mut sequence = entry.first_sequence;
        let mut refs: BTreeMap<Key, JournalRef> = BTreeMap::new();
        let mut first: Option<JournalRef> = None;
        let mut take = |book: &mut Book, chain: Chain| {
            let reference = book.next_ref(chain, sequence);
            sequence += 1;
            if first.is_none() {
                first = Some(reference.clone());
            }
            reference
        };
        for key in &keys {
            let reference = take(self, Chain::Position);
            refs.insert(key.clone(), reference);
        }
        let break_refs: Vec<JournalRef> = body
            .breaks
            .iter()
            .map(|_| take(self, Chain::Break))
            .collect();
        let figure_refs: Vec<JournalRef> = body
            .figures
            .iter()
            .map(|_| take(self, Chain::Figures))
            .collect();
        let attribute_ref = body
            .attributes
            .as_ref()
            .map(|_| take(self, Chain::Attributes));
        let lifecycle_refs: Vec<JournalRef> = body
            .lifecycle_events
            .iter()
            .map(|_| take(self, Chain::Lifecycle))
            .collect();
        // The order path's records (W11.13), each row chained of its own, in
        // the order their acts name them.
        let touched = self.ledger.touched(&body.acts);
        let order_refs: Vec<(String, JournalRef)> = touched
            .orders
            .iter()
            .map(|id| (id.clone(), take(self, Chain::Order)))
            .collect();
        let placement_refs: Vec<(String, JournalRef)> = touched
            .placements
            .iter()
            .map(|id| (id.clone(), take(self, Chain::Placement)))
            .collect();
        let execution_refs: Vec<(String, JournalRef)> = touched
            .executions
            .iter()
            .map(|id| (id.clone(), take(self, Chain::Execution)))
            .collect();
        let unplaced_refs: Vec<(String, JournalRef)> = touched
            .unplaced
            .iter()
            .map(|id| (id.clone(), take(self, Chain::Unplaced)))
            .collect();
        let locate_refs: Vec<(String, JournalRef)> = touched
            .locates
            .iter()
            .map(|id| (id.clone(), take(self, Chain::Locate)))
            .collect();
        let instruction_refs: Vec<(String, JournalRef)> = touched
            .instructions
            .iter()
            .map(|id| (id.clone(), take(self, Chain::Instruction)))
            .collect();
        let booking_refs: Vec<(String, JournalRef)> = touched
            .bookings
            .iter()
            .map(|id| (id.clone(), take(self, Chain::Booking)))
            .collect();
        let alert_refs: Vec<(String, JournalRef)> = touched
            .alerts
            .iter()
            .map(|id| (id.clone(), take(self, Chain::Alert)))
            .collect();
        for _ in &touched.settings {
            take(self, Chain::Setting);
        }
        let Some(first) = first else {
            return Err(StoreError::Invalid(
                "an entry that changes no record changes nothing; nothing was journalled".into(),
            ));
        };
        let last_sequence = sequence - 1;
        if entry.last_sequence != 0 && entry.last_sequence != last_sequence {
            return Err(StoreError::Unavailable(format!(
                "entry {} was numbered {} to {} and replays as {} to {}; the journal is not \
                 what the book wrote",
                entry.entry_id,
                entry.first_sequence,
                entry.last_sequence,
                entry.first_sequence,
                last_sequence
            )));
        }

        // Lines: buckets, and the lots they name.
        let lines_touch: BTreeSet<Key> = body
            .lines
            .iter()
            .map(|line| (line.instrument_id.clone(), line.side, line.sleeve.clone()))
            .collect();
        // Contract v27 (W9.10; rulings 320 and 396): what the entry sold
        // each instrument at -- its execution's price, or a booking's
        // average price -- so each relief records its proceeds.
        let sales = sales_of(body, &entry.meta.effective_date, &self.account_id);
        for line in &body.lines {
            let key = (line.instrument_id.clone(), line.side, line.sleeve.clone());
            let reference = refs[&key].clone();
            let sale = sales
                .iter()
                .find(|(instrument, _, _, _)| crate::became::same(instrument, &line.instrument_id))
                .map(|(_, price, date, factored)| (price, date.as_str(), *factored));
            let quantity = numbers::required(
                &format!("the line on {}'s quantity", line.instrument_id),
                line.quantity.as_ref(),
            )?;
            let carried = match (&line.opens_lot, line.lot_id.is_empty()) {
                (Some(_), false) => self.carried_lot(&key, &line.lot_id),
                _ => None,
            };
            let position = self.positions.entry(key.clone()).or_insert_with(|| {
                PositionState::new(&line.instrument_id, line.side, &line.sleeve)
            });
            position.removed = false;
            position.add_to_bucket(line, quantity)?;
            apply_lot(
                position,
                line,
                quantity,
                carried,
                &reference,
                entry.first_sequence,
                lenient,
                sale,
            )?;
        }

        // Basis adjustments: a lot's cost, changed or stated, and where its
        // holding period starts.
        for adjustment in &body.basis_adjustments {
            let Some(key) = self.lot_owner(&adjustment.lot_id).cloned() else {
                if lenient {
                    continue;
                }
                return Err(StoreError::Invalid(format!(
                    "no lot {} in account {}",
                    adjustment.lot_id, self.account_id
                )));
            };
            let reference = refs[&key].clone();
            let position = self.positions.get_mut(&key).expect("the owner holds it");
            let state = position
                .lots
                .get_mut(&adjustment.lot_id)
                .expect("the owner holds it");
            use meridian_domain::v1::basis_adjustment::Cost;
            let terms = state.lot.terms.get_or_insert_with(Default::default);
            match &adjustment.cost {
                Some(Cost::CostChange(change)) => {
                    terms.cost = match &terms.cost {
                        Some(cost) => Some(numbers::add_money("lots.terms.cost", cost, change)?),
                        None if lenient => None,
                        None => {
                            return Err(StoreError::Invalid(format!(
                                "lot {}'s cost is unknown; state it (stated_cost) rather than \
                                 change it",
                                adjustment.lot_id
                            )))
                        }
                    };
                }
                Some(Cost::StatedCost(stated)) => {
                    if terms.cost.is_some() && !lenient {
                        return Err(StoreError::Invalid(format!(
                            "lot {}'s cost is known; a known cost is changed by an amount \
                             (cost_change), not stated again",
                            adjustment.lot_id
                        )));
                    }
                    terms.cost = Some(stated.clone());
                }
                None => {}
            }
            if body.restored_unknown_costs.contains(&adjustment.lot_id) {
                terms.cost = None;
            }
            let restoring = !body.reverses.is_empty();
            if restoring || !adjustment.holding_period_start.is_empty() {
                state
                    .lot
                    .terms
                    .get_or_insert_with(Default::default)
                    .holding_period_start = adjustment.holding_period_start.clone();
            }
            state.lot.adjusted_by.push(reference);
        }

        // Encumbrances, the whole set per position named, each naming the
        // change that set it: an attribute; nothing moves.
        for (instrument, side, held) in &body.encumbrances {
            let key = (instrument.clone(), *side, String::new());
            let reference = refs[&key].clone();
            if let Some(position) = self.positions.get_mut(&key) {
                position.encumbrances = held
                    .iter()
                    .map(|encumbrance| Encumbrance {
                        set_by: Some(reference.clone()),
                        ..encumbrance.clone()
                    })
                    .collect();
            }
        }

        // An opening balance's sources, on every position its lines open.
        if !body.sources.is_empty() {
            for key in &lines_touch {
                if let Some(position) = self.positions.get_mut(key) {
                    position.opened_from = body.sources.clone();
                }
            }
        }

        let mut changes = Changes {
            first: first.clone(),
            last_sequence,
            ..Default::default()
        };

        for key in &keys {
            let position = self
                .positions
                .entry(key.clone())
                .or_insert_with(|| PositionState::new(&key.0, key.1, &key.2));
            if key.2.is_empty() && body.tombstones.contains(&(key.0.clone(), key.1)) {
                position.removed = true;
            }
            position.effective_date = entry.meta.effective_date.clone();
            position.last_change = Some(refs[key].clone());
            changes
                .positions
                .push((position.record(&self.account_id)?, previous[key]));
        }

        for (held, reference) in body.breaks.iter().zip(break_refs) {
            let mut record = held.clone();
            if body.resolved_by_this.contains(&record.break_id) {
                record
                    .resolution
                    .get_or_insert_with(Default::default)
                    .entries = vec![first.clone()];
            }
            record.last_change = Some(reference);
            self.breaks.insert(record.break_id.clone(), record.clone());
            changes.breaks.push(record);
        }

        for (held, reference) in body.lifecycle_events.iter().zip(lifecycle_refs) {
            let mut record = held.clone();
            record.last_change = Some(reference);
            self.lifecycle_events
                .insert(record.event_id.clone(), record.clone());
            changes.lifecycle_events.push(record);
        }

        for (held, reference) in body.figures.iter().zip(figure_refs) {
            let mut record = held.clone();
            record.last_change = Some(reference);
            self.figures.insert(
                (
                    agreement_key(record.agreement.as_ref()),
                    record.business_date.clone(),
                ),
                record.clone(),
            );
            changes.figures.push(record);
        }

        if let (Some(held), Some(reference)) = (&body.attributes, attribute_ref) {
            let mut record = held.clone();
            if let Some(opening) = record.opening_balance.as_mut() {
                if body.opening.is_some() {
                    opening.journal = Some(first.clone());
                }
            }
            record.last_change = Some(reference);
            self.attributes = Some(record.clone());
            changes.attributes = Some(record);
        }

        self.apply_acts(
            entry,
            &mut changes,
            ActRefs {
                orders: order_refs,
                placements: placement_refs,
                executions: execution_refs,
                unplaced: unplaced_refs,
                locates: locate_refs,
                instructions: instruction_refs,
                bookings: booking_refs,
                alerts: alert_refs,
            },
        )?;
        if body.states_waits {
            self.waits_stated = true;
        }

        if let Some(opening) = &body.opening {
            self.opening = Some((opening.entry_id.clone(), opening.as_of_date.clone()));
        }
        if body.clears_opening {
            if let Some((entry_id, _)) = self.opening.take() {
                self.reversed_openings.push(entry_id);
            }
        }
        if !body.reverses.is_empty() {
            if let Some(reversed) = self
                .entries
                .iter_mut()
                .find(|held| held.entry_id == body.reverses)
            {
                reversed.reversed_by = Some(entry.entry_id.clone());
            }
        }
        self.entries.push(EntryIndex::of(entry, first));
        Ok(changes)
    }

    /// The order path's acts applied to the ledger, each record they change
    /// given its reference and set whole in `changes` as it stands after
    /// them (W11.13).
    fn apply_acts(&mut self, entry: &Entry, changes: &mut Changes, refs: ActRefs) -> Result<()> {
        if entry.body.acts.is_empty() {
            return Ok(());
        }
        let before_orders: BTreeMap<String, Order> = refs
            .orders
            .iter()
            .filter(|(id, _)| self.ledger.orders.contains_key(id))
            .map(|(id, _)| Ok((id.clone(), self.ledger.order_record(id, true)?)))
            .collect::<Result<_>>()?;
        let before_placements: BTreeMap<String, Placement> = refs
            .placements
            .iter()
            .filter(|(id, _)| self.ledger.placements.contains_key(id))
            .map(|(id, _)| Ok((id.clone(), self.ledger.placement_record(id)?)))
            .collect::<Result<_>>()?;
        // An order's terms take the sequence of its own change in the entry
        // that sets them: the version a check names (W11.4).
        let versions: BTreeMap<String, u64> = refs
            .orders
            .iter()
            .map(|(id, reference)| (id.clone(), reference.sequence))
            .collect();
        for act in &entry.body.acts {
            // A give-up's answer names its entry (W11.32).
            let stamped;
            let act = match act {
                crate::orders::Act::GiveUpRecorded {
                    execution_id,
                    booking_id,
                    allocation_id,
                    record,
                } => {
                    stamped = crate::orders::Act::GiveUpRecorded {
                        execution_id: execution_id.clone(),
                        booking_id: booking_id.clone(),
                        allocation_id: allocation_id.clone(),
                        record: meridian_domain::v1::GiveUpRecord {
                            entry: Some(entry.meta.clone()),
                            ..record.clone()
                        },
                    };
                    &stamped
                }
                other => other,
            };
            self.ledger.apply(act, &entry.entry_id, &versions);
            match act {
                crate::orders::Act::BookingOpened { booking, .. } => {
                    if let Some(held) = self.ledger.bookings.get_mut(&booking.booking_id) {
                        held.booked_at = entry.first_sequence;
                    }
                }
                crate::orders::Act::BookingReplaced { booking_id, .. } => {
                    if let Some(held) = self.ledger.bookings.get_mut(booking_id) {
                        held.booked_at = entry.first_sequence;
                    }
                }
                // Whose own order it is, and when it opened: the instance
                // that sent it, as its sidecar stamped it.
                crate::orders::Act::Opened { order, .. } => {
                    if let Some(held) = self.ledger.orders.get_mut(&order.order_id) {
                        held.opened_by = entry.cause.instance_id.clone();
                        held.opened_at_ns = entry.meta.received_at_ns;
                    }
                }
                crate::orders::Act::ChangeOpened { request } => {
                    changes.change_requests.push((
                        request.change_request_id.clone(),
                        request.order_id.clone(),
                        request.placement_id.clone(),
                    ));
                    if let Some(held) = self.ledger.changes.get_mut(&request.change_request_id) {
                        if held.entry.is_none() {
                            held.entry = Some(entry.meta.clone());
                        }
                    }
                }
                _ => {}
            }
        }
        // Every record's reference first, so an order whole carries its
        // placements and executions as they stand after the entry.
        for (id, reference) in &refs.orders {
            if let Some(held) = self.ledger.orders.get_mut(id) {
                held.last_change = Some(reference.clone());
                self.ledger
                    .order_entries
                    .entry(id.clone())
                    .or_default()
                    .push(entry.entry_id.clone());
            }
        }
        for (id, reference) in &refs.placements {
            if let Some(held) = self.ledger.placements.get_mut(id) {
                held.last_change = Some(reference.clone());
            }
        }
        for (id, reference) in &refs.executions {
            if let Some(held) = self.ledger.executions.get_mut(id) {
                held.execution.last_change = Some(reference.clone());
            }
        }
        for (id, _) in &refs.orders {
            let Some(held) = self.ledger.orders.get(id) else {
                continue;
            };
            let expiry = crate::orders::due_at(held);
            changes.order_expiries.insert(id.clone(), expiry);
            let after = self.ledger.order_record(id, true)?;
            let changed = crate::orders::order_changed(before_orders.get(id), &after);
            changes.orders.push((after, changed));
        }
        for (id, _) in &refs.placements {
            if !self.ledger.placements.contains_key(id) {
                continue;
            }
            let after = self.ledger.placement_record(id)?;
            let changed = crate::orders::placement_changed(before_placements.get(id), &after);
            changes.placements.push((after, changed));
        }
        for (id, _) in &refs.executions {
            if let Some(held) = self.ledger.executions.get(id) {
                changes.executions.push(held.execution.clone());
            }
        }
        for (id, reference) in &refs.unplaced {
            if let Some(held) = self.ledger.unplaced.get_mut(id) {
                let placed = entry.body.acts.iter().any(|act| {
                    matches!(act, crate::orders::Act::Placed { unplaced_id } if unplaced_id == id)
                });
                if placed {
                    held.placed_by = Some(reference.clone());
                }
                held.last_change = Some(reference.clone());
                changes.unplaced.push(held.clone());
            }
        }
        for (id, reference) in &refs.locates {
            if let Some(held) = self.ledger.locates.get_mut(id) {
                held.locate.last_change = Some(reference.clone());
                changes.locates.push(self.ledger.locate_record(id)?);
            }
        }
        for (key, reference) in &refs.instructions {
            if let Some(held) = self.ledger.instructions.get_mut(key) {
                held.last_change = Some(reference.clone());
                held.entry = Some(entry.meta.clone());
                changes.instructions.push(held.clone());
            }
        }
        for (id, reference) in &refs.bookings {
            if let Some(held) = self.ledger.bookings.get_mut(id) {
                held.booking.last_change = Some(reference.clone());
                changes.bookings.push(self.ledger.booking_record(id)?);
            }
        }
        for (id, reference) in &refs.alerts {
            if let Some(held) = self.ledger.alerts.get_mut(id) {
                held.last_change = Some(reference.clone());
                changes.alerts.push(held.clone());
            }
        }
        Ok(())
    }

    /// The account's base currency's cash instrument, where its attributes
    /// name a base currency this process has resolved (W9.13; contract v24):
    /// the one currency that is a balance, every other opening lots.
    pub fn base_cash_instrument(&self) -> Option<String> {
        let code = self
            .attributes
            .as_ref()
            .map(|held| held.base_currency_code.clone())
            .filter(|code| !code.is_empty())?;
        meridian_domain::money::known(&code)
    }

    /// Whether `instrument_id` is the account's base currency (W11.13).
    pub fn is_base(&self, instrument_id: &str) -> bool {
        let Some(code) = self
            .attributes
            .as_ref()
            .map(|held| held.base_currency_code.clone())
            .filter(|code| !code.is_empty())
        else {
            return false;
        };
        meridian_domain::money::known(&code).as_deref() == Some(instrument_id)
            || meridian_domain::money::code_of(instrument_id).as_deref() == Some(code.as_str())
    }

    /// A lot of this account held under another position, whose record a
    /// line moving it carries over: a merged record's lots keep their
    /// identifiers, costs and dates (W9.9).
    fn carried_lot(&self, key: &Key, lot_id: &str) -> Option<LotState> {
        self.positions
            .iter()
            .filter(|(held, _)| *held != key)
            .find_map(|(_, position)| position.lots.get(lot_id).cloned())
    }

    /// What must hold of every position an entry changed, checked on the
    /// account after it: the side its quantity is on, every lot on that side,
    /// and the lots of a position that has any summing to its trade-date
    /// quantity, cash excepted, which has none (W9's sixth invariant).
    pub fn check(&self, body: &Body) -> Result<()> {
        let lot_lines: BTreeSet<Key> = body
            .lines
            .iter()
            .filter(|line| !line.lot_id.is_empty() || line.opens_lot.is_some())
            .map(|line| (line.instrument_id.clone(), line.side, line.sleeve.clone()))
            .collect();
        for key in self.touched(body) {
            let Some(position) = self.positions.get(&key) else {
                continue;
            };
            if position.removed {
                continue;
            }
            let trade = position.trade_date()?;
            let long = position.side == HoldingSide::Long as i32;
            let on_its_side = |value: Exact| {
                if long {
                    !value.is_negative()
                } else {
                    !value.negated().is_negative()
                }
            };
            // A fill's cash may go below what the account holds: a fact is
            // never refused for it (W11's invariants). From contract v24 the
            // account's base currency is a signed balance whatever lots it
            // held before its base was set (W9's invariant, W11.13).
            let base = self.is_base(&key.0);
            let cash_of_a_trade =
                (body.trade && position.lots.is_empty() && !lot_lines.contains(&key)) || base;
            if !on_its_side(trade) && !cash_of_a_trade {
                return Err(StoreError::Invalid(format!(
                    "the entry leaves {} on the {} side at {trade}; a position's quantity is \
                     signed to match its side",
                    key.0,
                    side_named(position.side)
                )));
            }
            for state in position.lots.values() {
                let open =
                    numbers::required("lots.open_quantity", state.lot.open_quantity.as_ref())?;
                if !on_its_side(open) {
                    return Err(StoreError::Invalid(format!(
                        "the entry relieves more of lot {} than is open, leaving {open}",
                        state.lot.lot_id
                    )));
                }
            }
            if !base && (position.has_open_lots() || lot_lines.contains(&key)) {
                let lots = position.open_lots()?;
                if lots != trade {
                    return Err(StoreError::refused(
                        RefusalReason::LotsUnbalanced,
                        format!(
                            "{}'s open lots sum to {lots} and its trade-date quantity is {trade}; \
                             a position's lots sum to its quantity",
                            key.0
                        ),
                    ));
                }
            }
        }
        Ok(())
    }
}

pub fn side_named(side: i32) -> &'static str {
    match HoldingSide::try_from(side) {
        Ok(HoldingSide::Long) => "long",
        Ok(HoldingSide::Short) => "short",
        _ => "unspecified",
    }
}

/// What an entry sold each instrument at (W9.10, contract v27): a fill's
/// price and trade date, an allocation received at its average price on the
/// entry's date, or a booking's own allocation. None for an entry with no
/// execution behind it -- a transfer, a correction -- whose reliefs carry no
/// proceeds.
/// From contract v29 a debt fill stating its factor names the multiplier its
/// proceeds take, per 100 of face times that factor (986, 987); every other
/// sale's is the lot's own.
#[allow(clippy::type_complexity)]
fn sales_of(
    body: &Body,
    effective_date: &str,
    account_id: &str,
) -> Vec<(String, Money, String, Option<Exact>)> {
    use crate::orders::Act;
    let mut out = Vec::new();
    for act in &body.acts {
        match act {
            Act::Executed {
                execution,
                factor_taken,
                ..
            } => {
                let fills = execution
                    .quantity
                    .as_ref()
                    .and_then(|q| Exact::from_wire(q).ok())
                    .is_some_and(|q| !q.is_zero());
                if let (true, Some(price)) = (fills, execution.price.as_ref()) {
                    // Contract v29: the factor stated, or the published
                    // one the book took for it (amended at core stage C).
                    let factored = crate::orders::factor_of(execution, factor_taken)
                        .as_ref()
                        .and_then(|f| Exact::from_wire(f).ok())
                        .and_then(|f| {
                            crate::orders::multiply("factor", Exact::new(1, 2).ok()?, f).ok()
                        });
                    out.push((
                        execution.instrument_id.clone(),
                        price.clone(),
                        execution.trade_date.clone(),
                        factored,
                    ));
                }
            }
            Act::AllocationReceived { allocation, .. } => {
                if let Some(price) = allocation.average_price.as_ref() {
                    out.push((
                        String::new(),
                        price.clone(),
                        effective_date.to_string(),
                        None,
                    ));
                }
            }
            Act::BookingOpened { booking, .. } => {
                for allocation in &booking.allocations {
                    if allocation.account_id == account_id {
                        if let Some(price) = allocation.average_price.as_ref() {
                            out.push((
                                String::new(),
                                price.clone(),
                                effective_date.to_string(),
                                None,
                            ));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    // An allocation names no instrument: it is the entry's one traded.
    let traded = body
        .lines
        .iter()
        .find(|line| line.opens_lot.is_some() || !line.lot_id.is_empty())
        .map(|line| line.instrument_id.clone())
        .unwrap_or_default();
    for sale in &mut out {
        if sale.0.is_empty() {
            sale.0 = traded.clone();
        }
    }
    out
}

/// A line's lot: opened, carried in from another position, added to within
/// the entry that opened it, or relieved; a relief recorded with what the
/// quantity relieved was sold for, where the entry sold it (contract v27).
#[allow(clippy::too_many_arguments)]
fn apply_lot(
    position: &mut PositionState,
    line: &MovementLine,
    quantity: Exact,
    carried: Option<LotState>,
    reference: &JournalRef,
    entry_sequence: u64,
    lenient: bool,
    sale: Option<(&Money, &str, Option<Exact>)>,
) -> Result<()> {
    if let Some(terms) = &line.opens_lot {
        if line.lot_id.is_empty() {
            return Err(StoreError::Invalid(format!(
                "a line opening a lot on {} carries no lot identifier",
                line.instrument_id
            )));
        }
        position.opened_lots += 1;
        if let Some(mut moved) = carried {
            moved.lot.open_quantity = numbers::wire(quantity);
            moved.ordinal = position.opened_lots;
            position.lots.insert(line.lot_id.clone(), moved);
            return Ok(());
        }
        if position.lots.contains_key(&line.lot_id) && !lenient {
            return Err(StoreError::Invalid(format!(
                "lot {} is already open on {}",
                line.lot_id, line.instrument_id
            )));
        }
        position.lots.insert(
            line.lot_id.clone(),
            LotState {
                lot: Lot {
                    lot_id: line.lot_id.clone(),
                    open_quantity: numbers::wire(quantity),
                    original_quantity: numbers::wire(quantity),
                    terms: Some(terms.clone()),
                    opened_by: Some(reference.clone()),
                    relieved_by: Vec::new(),
                    adjusted_by: Vec::new(),
                    sleeve: line.sleeve.clone(),
                    reliefs: Vec::new(),
                },
                opened_in: entry_sequence,
                ordinal: position.opened_lots,
            },
        );
        return Ok(());
    }
    if line.lot_id.is_empty() {
        return Ok(());
    }
    let state = match position.lots.get_mut(&line.lot_id) {
        Some(state) => state,
        None if lenient => position
            .lots
            .entry(line.lot_id.clone())
            .or_insert_with(|| LotState {
                lot: Lot {
                    lot_id: line.lot_id.clone(),
                    open_quantity: numbers::wire(Exact::ZERO),
                    original_quantity: numbers::wire(Exact::ZERO),
                    ..Default::default()
                },
                opened_in: 0,
                ordinal: u64::MAX,
            }),
        None => {
            return Err(StoreError::Invalid(format!(
                "no lot {} on {} {}",
                line.lot_id,
                side_named(line.side),
                line.instrument_id
            )))
        }
    };
    let open = numbers::required("lots.open_quantity", state.lot.open_quantity.as_ref())?;
    let after = numbers::add("lots.open_quantity", open, quantity)?;
    state.lot.open_quantity = numbers::wire(after);
    if state.opened_in == entry_sequence {
        let original = numbers::required(
            "lots.original_quantity",
            state.lot.original_quantity.as_ref(),
        )?;
        state.lot.original_quantity =
            numbers::wire(numbers::add("lots.original_quantity", original, quantity)?);
    } else if magnitude_below(after, open) {
        if state.lot.relieved_by.last() != Some(reference) {
            state.lot.relieved_by.push(reference.clone());
        }
        let relieved = numbers::add("relief", open, after.negated())?;
        let relieved = if relieved.is_negative() {
            relieved.negated()
        } else {
            relieved
        };
        record_relief(&mut state.lot, reference, relieved, sale)?;
    }
    Ok(())
}

/// The multiplier a lot was costed by (contract v28, 559): its cost over its
/// original quantity times its unit cost, where that is exactly a number the
/// two multiply back to -- a listed option's 100 -- and one otherwise. Read
/// from the lot itself, so a replay needs no instrument store.
fn lot_multiplier(lot: &Lot) -> Exact {
    let read = |d: Option<&meridian_pb::v1::Decimal>| d.and_then(|d| Exact::from_wire(d).ok());
    let Some(terms) = lot.terms.as_ref() else {
        return Exact::ONE;
    };
    let (Some(unit), Some(cost), Some(quantity)) = (
        read(terms.unit_cost.as_ref().and_then(|m| m.amount.as_ref())),
        read(terms.cost.as_ref().and_then(|m| m.amount.as_ref())),
        read(lot.original_quantity.as_ref()),
    ) else {
        return Exact::ONE;
    };
    let Ok(per) = crate::orders::multiply("a lot's cost", unit, quantity) else {
        return Exact::ONE;
    };
    let as_f64 = |e: Exact| e.to_string().parse::<f64>().unwrap_or(0.0);
    let (per_f, cost_f) = (as_f64(per), as_f64(cost));
    if per_f == 0.0 || cost_f == 0.0 {
        return Exact::ONE;
    }
    // Six places for a series' multiplier; from contract v29 twelve, for a
    // pool's per 100 of face times its factor (0.0084211375).
    for places in [6, 12] {
        let guess = format!("{:.*}", places, (cost_f / per_f).abs());
        let guess = guess.trim_end_matches('0').trim_end_matches('.');
        if let Ok(m) = guess.parse::<Exact>() {
            if !m.is_zero()
                && crate::orders::multiply("a lot's cost", per, m)
                    .is_ok_and(|back| back == cost || back == cost.negated())
            {
                return m;
            }
        }
    }
    Exact::ONE
}

/// One relief of a lot (W9.10, contract v27): the entry, the quantity, and
/// its proceeds where the entry sold it; two lines of one entry relieving
/// the same lot are one relief.
fn record_relief(
    lot: &mut Lot,
    reference: &JournalRef,
    relieved: Exact,
    sale: Option<(&Money, &str, Option<Exact>)>,
) -> Result<()> {
    let multiplier = sale
        .and_then(|(_, _, factored)| factored)
        .unwrap_or_else(|| lot_multiplier(lot));
    let proceeds = |quantity: Exact| -> Result<Option<Money>> {
        let Some((price, _, _)) = sale else {
            return Ok(None);
        };
        let Some(unit) = price.amount.as_ref().and_then(|a| Exact::from_wire(a).ok()) else {
            return Ok(None);
        };
        let amount = crate::orders::multiply(
            "a relief's proceeds",
            crate::orders::multiply("a relief's proceeds", quantity, unit)?,
            multiplier,
        )?;
        Ok(Some(Money {
            amount: numbers::wire(amount),
            ..price.clone()
        }))
    };
    if let Some(last) = lot
        .reliefs
        .last_mut()
        .filter(|last| last.relieved_by.as_ref() == Some(reference))
    {
        let before = numbers::required("relief.quantity", last.quantity.as_ref())?;
        let total = numbers::add("relief.quantity", before, relieved)?;
        last.quantity = numbers::wire(total);
        last.proceeds = proceeds(total)?;
        return Ok(());
    }
    lot.reliefs.push(LotRelief {
        relieved_by: Some(reference.clone()),
        quantity: numbers::wire(relieved),
        proceeds: proceeds(relieved)?,
        trade_date: sale
            .map(|(_, date, _)| date.to_string())
            .unwrap_or_default(),
    });
    Ok(())
}

/// Whether `after` is nearer zero than `before`: a relief.
fn magnitude_below(after: Exact, before: Exact) -> bool {
    let size = |value: Exact| {
        if value.is_negative() {
            value.negated()
        } else {
            value
        }
    };
    size(after) < size(before)
}
