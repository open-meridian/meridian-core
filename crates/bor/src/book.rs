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
    AccountAttributes, AccountFigures, BookPosition, Break, Encumbrance, FreeBasis, HoldingSide,
    JournalRef, Lot, MarginAgreementRef, MovementLine, OpeningSource, PendingSettlement,
    PendingState, SettlementBucket,
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
}

/// A position's key within its account: instrument and side.
pub type Key = (String, i32);

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
    fn new(instrument_id: &str, side: i32) -> Self {
        PositionState {
            instrument_id: instrument_id.to_string(),
            side,
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

/// An entry as the account's later acts need it: what a reversal negates,
/// what entries a resolution names, and whether it has been reversed.
#[derive(Debug, Clone, PartialEq)]
pub struct EntryIndex {
    pub entry_id: String,
    pub kind: String,
    pub effective_date: String,
    pub first: JournalRef,
    pub body: Body,
    pub reversed_by: Option<String>,
}

/// The records an entry changed, as they stand after it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Changes {
    /// Each position with its trade-date quantity before the entry.
    pub positions: Vec<(BookPosition, Exact)>,
    pub breaks: Vec<Break>,
    pub figures: Vec<AccountFigures>,
    pub attributes: Option<AccountAttributes>,
    /// The entry's place: its first change's.
    pub first: JournalRef,
    pub last_sequence: u64,
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
    /// The standing opening balance: its entry and D0.
    pub opening: Option<(String, String)>,
    /// Opening balances reversed, in order.
    pub reversed_openings: Vec<String>,
    pub entries: Vec<EntryIndex>,
    chains: [u64; 4],
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
            opening: None,
            reversed_openings: Vec::new(),
            entries: Vec::new(),
            chains: [0; 4],
        }
    }

    /// An account replayed from its journal: strictly, so an entry that no
    /// longer applies is a fault rather than a guess.
    pub fn replay(account_id: &str, partition: &str, entries: &[Entry]) -> Result<Book> {
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

    pub fn entry(&self, entry_id: &str) -> Option<&EntryIndex> {
        self.entries.iter().find(|held| held.entry_id == entry_id)
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
            .map(|line| (line.instrument_id.clone(), line.side))
            .collect();
        for adjustment in &body.basis_adjustments {
            if let Some(key) = self.lot_owner(&adjustment.lot_id) {
                keys.insert(key.clone());
            }
        }
        keys.extend(body.tombstones.iter().cloned());
        keys.extend(
            body.encumbrances
                .iter()
                .map(|(instrument, side, _)| (instrument.clone(), *side)),
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
            .map(|line| (line.instrument_id.clone(), line.side))
            .collect();
        for line in &body.lines {
            let key = (line.instrument_id.clone(), line.side);
            let reference = refs[&key].clone();
            let quantity = numbers::required(
                &format!("the line on {}'s quantity", line.instrument_id),
                line.quantity.as_ref(),
            )?;
            let carried = match (&line.opens_lot, line.lot_id.is_empty()) {
                (Some(_), false) => self.carried_lot(&key, &line.lot_id),
                _ => None,
            };
            let position = self
                .positions
                .entry(key.clone())
                .or_insert_with(|| PositionState::new(&line.instrument_id, line.side));
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
            let key = (instrument.clone(), *side);
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
                .or_insert_with(|| PositionState::new(&key.0, key.1));
            if body.tombstones.contains(key) {
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
        self.entries.push(EntryIndex {
            entry_id: entry.entry_id.clone(),
            kind: entry.meta.kind.clone(),
            effective_date: entry.meta.effective_date.clone(),
            first,
            body: body.clone(),
            reversed_by: None,
        });
        Ok(changes)
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
            .map(|line| (line.instrument_id.clone(), line.side))
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
            if !on_its_side(trade) {
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
            if position.has_open_lots() || lot_lines.contains(&key) {
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

/// A line's lot: opened, carried in from another position, added to within
/// the entry that opened it, or relieved.
fn apply_lot(
    position: &mut PositionState,
    line: &MovementLine,
    quantity: Exact,
    carried: Option<LotState>,
    reference: &JournalRef,
    entry_sequence: u64,
    lenient: bool,
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
    } else if magnitude_below(after, open) && state.lot.relieved_by.last() != Some(reference) {
        state.lot.relieved_by.push(reference.clone());
    }
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
