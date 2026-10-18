//! An account's projection kept as at one entry, so the book replays only the
//! journal's tail (W9.10, contract v25; the v25 build plan's question 8,
//! ruled 2026-10-15 and 2026-10-16, item 122).
//!
//! Until v25 every command and every dated read replayed the account's whole
//! journal, and its cost grew with the account's history. From v25 the book
//! keeps, every so many entries -- the snapshot interval, `bor.snapshotInterval`
//! in the chart -- the account's [`Book`] as at its latest entry, and starts
//! from the latest snapshot at or before the entry it needs, replaying only
//! what came after. A snapshot is core's own and no record kind: nothing a
//! plugin reads or hears changes, the journal stays the record (decisions/031),
//! and a snapshot plus its tail is exactly what a full replay makes, which
//! [`tests::a_snapshot_and_its_tail_are_the_full_replay`] and the store's
//! property tests hold it to. A snapshot is derived, never the record: taken
//! when a command's replay reaches the interval, and from v26 when a read
//! replays past it and at the book's start for an account past it with none
//! of this [`FORMAT`] (an upgraded or a quiet account's); one unreadable or of
//! another format is passed over until then.
//!
//! Kept as JSON, every record the contract's own type in its protobuf
//! encoding, as an entry is (crate::journal), so the book defines no second
//! shape for a record. From format 2 (contract v26) it holds no entry's
//! body: the book holds only what a fold needs and loads a body from the
//! journal when a decision asks for it, so neither a snapshot nor the book's
//! memory grows with what the entries carried. Each of the book's own structures is written field by
//! field from an exhaustive destructuring, so a field added to one fails to
//! compile here until the snapshot carries it.

use std::collections::BTreeMap;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use meridian_domain::exact::Exact;
use meridian_domain::v1::{BookingState, OrderState, PlacementState};
use prost::Message;
use serde::{Deserialize, Serialize};

use crate::book::{Book, EntryIndex, LotState, PositionState};
use crate::orders::{
    BookingHeld, ExecutionHeld, Ledger, LocateHeld, OrderHeld, PlacementHeld, ReceivedHeld,
};
use crate::store::{Result, StoreError};

/// The snapshot's shape and the projection it was made by. Raised whenever
/// what a snapshot holds, or how the book folds an entry into it, changes:
/// a snapshot of another format is stale, and rebuilt from the journal.
/// Format 2 (contract v26, W9.10; ruling 165): an entry's index holds no
/// body, which the book loads from the journal when a decision asks.
pub const FORMAT: i32 = 2;

/// The snapshot interval with nothing set: an account's projection kept
/// every this many entries. From core's replay benchmark (item 122;
/// `tests::replay_benchmark`, 2026-10-16, release build): a replay costs
/// about 7 us an entry and a snapshot about 600 bytes an entry of history,
/// read back in about a third of a full replay's time. At 1,000 a command's
/// tail is at most 1,000 entries (about 7 ms, 3.5 ms on average), close to a
/// snapshot's read at a few thousand entries of history, and a snapshot's
/// bytes are written once a thousand entries.
pub const DEFAULT_INTERVAL: u64 = 1_000;

/// The least and most interval the chart may set.
pub const INTERVAL_RANGE: (u64, u64) = (10, 100_000);

/// How many snapshots an account keeps: the latest, and the two before it, so
/// a read at a recent watermark starts from one too.
pub const KEPT: usize = 3;

/// The interval as the environment sets it (`MERIDIAN_BOR_SNAPSHOT_INTERVAL`,
/// the chart's `bor.snapshotInterval`), or the default; refused outside
/// [`INTERVAL_RANGE`], naming the variable.
pub fn interval_from(set: Option<&str>) -> std::result::Result<u64, String> {
    let Some(text) = set.map(str::trim).filter(|text| !text.is_empty()) else {
        return Ok(DEFAULT_INTERVAL);
    };
    match text.parse::<u64>() {
        Ok(n) if (INTERVAL_RANGE.0..=INTERVAL_RANGE.1).contains(&n) => Ok(n),
        _ => Err(format!(
            "MERIDIAN_BOR_SNAPSHOT_INTERVAL is {text:?}; an interval of {} to {} entries",
            INTERVAL_RANGE.0, INTERVAL_RANGE.1
        )),
    }
}

// ── The shape ───────────────────────────────────────────────────────────────

fn pb<M: Message>(message: &M) -> String {
    STANDARD.encode(message.encode_to_vec())
}

fn un<M: Message + Default>(text: &str) -> Result<M> {
    let bytes = STANDARD
        .decode(text)
        .map_err(|failed| StoreError::Invalid(format!("a snapshot's record: {failed}")))?;
    M::decode(bytes.as_slice())
        .map_err(|failed| StoreError::Invalid(format!("a snapshot's record: {failed}")))
}

fn pbs<M: Message>(messages: &[M]) -> Vec<String> {
    messages.iter().map(pb).collect()
}

fn uns<M: Message + Default>(texts: &[String]) -> Result<Vec<M>> {
    texts.iter().map(|text| un(text)).collect()
}

fn opt<M: Message>(message: &Option<M>) -> Option<String> {
    message.as_ref().map(pb)
}

fn unopt<M: Message + Default>(text: &Option<String>) -> Result<Option<M>> {
    text.as_deref().map(un).transpose()
}

fn exact(value: Exact) -> String {
    pb(&value.to_wire())
}

fn unexact(text: &str) -> Result<Exact> {
    let wire: meridian_pb::v1::Decimal = un(text)?;
    Exact::from_wire(&wire)
        .map_err(|_| StoreError::Invalid("a snapshot's quantity is out of range".into()))
}

fn map_pb<K: Clone, M: Message>(map: &BTreeMap<K, M>) -> Vec<(K, String)> {
    map.iter()
        .map(|(key, value)| (key.clone(), pb(value)))
        .collect()
}

fn unmap_pb<K: Ord + Clone, M: Message + Default>(pairs: &[(K, String)]) -> Result<BTreeMap<K, M>> {
    pairs
        .iter()
        .map(|(key, text)| Ok((key.clone(), un(text)?)))
        .collect()
}

fn enum_of<E: TryFrom<i32>>(value: i32, what: &str) -> Result<E> {
    E::try_from(value)
        .map_err(|_| StoreError::Invalid(format!("a snapshot's {what} {value} is none")))
}

#[derive(Serialize, Deserialize)]
struct Shape {
    format: i32,
    account_id: String,
    partition: String,
    positions: Vec<((String, i32, String), PositionShape)>,
    breaks: Vec<(String, String)>,
    figures: Vec<((String, String), String)>,
    attributes: Option<String>,
    opening: Option<(String, String)>,
    reversed_openings: Vec<String>,
    entries: Vec<EntryShape>,
    chains: Vec<u64>,
    ledger: LedgerShape,
    waits_stated: bool,
}

#[derive(Serialize, Deserialize)]
struct PositionShape {
    instrument_id: String,
    side: i32,
    sleeve: String,
    settled: String,
    pending: Vec<(String, String, Option<String>)>,
    not_stated: String,
    lots: Vec<(String, String, u64, u64)>,
    opened_lots: u64,
    opened_from: Vec<String>,
    effective_date: String,
    last_change: Option<String>,
    removed: bool,
    encumbrances: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct EntryShape {
    entry_id: String,
    kind: String,
    effective_date: String,
    first: String,
    reversed_by: Option<String>,
    moves_positions: bool,
    reverses: String,
    acts: bool,
    rolls: bool,
}

#[derive(Serialize, Deserialize)]
struct LedgerShape {
    orders: Vec<(String, OrderShape)>,
    placements: Vec<(String, PlacementShape)>,
    executions: Vec<(String, ExecutionShape)>,
    charges: Vec<(String, String)>,
    changes: Vec<(String, String)>,
    unplaced: Vec<(String, String)>,
    locates: Vec<(String, String, Vec<String>)>,
    instructions: Vec<(String, String)>,
    venue_facts: Vec<(String, String)>,
    unplaced_executions: Vec<(String, String)>,
    order_entries: Vec<(String, Vec<String>)>,
    bookings: Vec<(String, BookingShape)>,
    received: Vec<((String, String), ReceivedShape)>,
    sleeve_crosses: Vec<(String, String)>,
    alerts: Vec<(String, String)>,
    local_record_orders: Option<bool>,
    #[serde(default)]
    agent_orders_demo_only: Option<bool>,
}

#[derive(Serialize, Deserialize)]
struct OrderShape {
    order: String,
    released: bool,
    release_requested: bool,
    ended: Option<i32>,
    ending: Option<i32>,
    conditions: Vec<(i32, String)>,
    checks: Vec<String>,
    overridden_version: Option<u64>,
    change_requests: Vec<String>,
    placements: Vec<String>,
    executions: Vec<String>,
    charges: Vec<String>,
    children: Vec<String>,
    version: u64,
    last_change: Option<String>,
    opened_by: String,
    opened_at_ns: i64,
}

#[derive(Serialize, Deserialize)]
struct PlacementShape {
    placement: String,
    state: i32,
    conditions: Vec<(i32, String)>,
    checks: Vec<String>,
    change_requests: Vec<String>,
    executions: Vec<String>,
    charges: Vec<String>,
    last_change: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct ExecutionShape {
    execution: String,
    entry_id: String,
    sender: String,
    superseded_by: Option<String>,
    venue_account_code: String,
    venue_client_order_id: String,
    accepted_lines: Option<(String, usize, usize)>,
}

#[derive(Serialize, Deserialize)]
struct BookingShape {
    booking: String,
    state: i32,
    held_reason: String,
    lines: (String, usize, usize),
    version: u32,
    held_alert: String,
    booked_at: u64,
}

#[derive(Serialize, Deserialize)]
struct ReceivedShape {
    booking_id: String,
    from_account: String,
    allocation: String,
    member_order_id: String,
    lines: (String, usize, usize),
    withdrawn: bool,
}

// ── Writing ─────────────────────────────────────────────────────────────────

/// The account's projection as a snapshot's bytes.
pub fn encode(book: &Book) -> Vec<u8> {
    let Book {
        account_id,
        partition,
        positions,
        breaks,
        figures,
        attributes,
        opening,
        reversed_openings,
        entries,
        chains,
        ledger,
        waits_stated,
        // The bodies a decision asked for: the journal's, never a
        // snapshot's (format 2).
        bodies: _,
    } = book;
    let shape = Shape {
        format: FORMAT,
        account_id: account_id.clone(),
        partition: partition.clone(),
        positions: positions
            .iter()
            .map(|(key, position)| (key.clone(), position_shape(position)))
            .collect(),
        breaks: map_pb(breaks),
        figures: map_pb(figures),
        attributes: opt(attributes),
        opening: opening.clone(),
        reversed_openings: reversed_openings.clone(),
        entries: entries.iter().map(entry_shape).collect(),
        chains: chains.to_vec(),
        ledger: ledger_shape(ledger),
        waits_stated: *waits_stated,
    };
    serde_json::to_vec(&shape).expect("a snapshot's shape always serializes")
}

fn position_shape(position: &PositionState) -> PositionShape {
    let PositionState {
        instrument_id,
        side,
        sleeve,
        settled,
        pending,
        not_stated,
        lots,
        opened_lots,
        opened_from,
        effective_date,
        last_change,
        removed,
        encumbrances,
    } = position;
    PositionShape {
        instrument_id: instrument_id.clone(),
        side: *side,
        sleeve: sleeve.clone(),
        settled: exact(*settled),
        pending: pending
            .iter()
            .map(|(date, (quantity, state))| (date.clone(), exact(*quantity), opt(state)))
            .collect(),
        not_stated: exact(*not_stated),
        lots: lots
            .iter()
            .map(|(id, held)| {
                let LotState {
                    lot,
                    opened_in,
                    ordinal,
                } = held;
                (id.clone(), pb(lot), *opened_in, *ordinal)
            })
            .collect(),
        opened_lots: *opened_lots,
        opened_from: pbs(opened_from),
        effective_date: effective_date.clone(),
        last_change: opt(last_change),
        removed: *removed,
        encumbrances: pbs(encumbrances),
    }
}

fn entry_shape(entry: &EntryIndex) -> EntryShape {
    let EntryIndex {
        entry_id,
        kind,
        effective_date,
        first,
        reversed_by,
        moves_positions,
        reverses,
        acts,
        rolls,
    } = entry;
    EntryShape {
        entry_id: entry_id.clone(),
        kind: kind.clone(),
        effective_date: effective_date.clone(),
        first: pb(first),
        reversed_by: reversed_by.clone(),
        moves_positions: *moves_positions,
        reverses: reverses.clone(),
        acts: *acts,
        rolls: *rolls,
    }
}

fn ledger_shape(ledger: &Ledger) -> LedgerShape {
    let Ledger {
        orders,
        placements,
        executions,
        charges,
        changes,
        unplaced,
        locates,
        instructions,
        venue_facts,
        unplaced_executions,
        order_entries,
        bookings,
        received,
        sleeve_crosses,
        alerts,
        local_record_orders,
        agent_orders_demo_only,
    } = ledger;
    LedgerShape {
        orders: orders
            .iter()
            .map(|(id, held)| (id.clone(), order_shape(held)))
            .collect(),
        placements: placements
            .iter()
            .map(|(id, held)| (id.clone(), placement_shape(held)))
            .collect(),
        executions: executions
            .iter()
            .map(|(id, held)| (id.clone(), execution_shape(held)))
            .collect(),
        charges: map_pb(charges),
        changes: map_pb(changes),
        unplaced: map_pb(unplaced),
        locates: locates
            .iter()
            .map(|(id, held)| {
                let LocateHeld { locate, uses } = held;
                (id.clone(), pb(locate), pbs(uses))
            })
            .collect(),
        instructions: map_pb(instructions),
        venue_facts: venue_facts
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        unplaced_executions: unplaced_executions
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        order_entries: order_entries
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        bookings: bookings
            .iter()
            .map(|(id, held)| (id.clone(), booking_shape(held)))
            .collect(),
        received: received
            .iter()
            .map(|(key, held)| (key.clone(), received_shape(held)))
            .collect(),
        sleeve_crosses: map_pb(sleeve_crosses),
        alerts: map_pb(alerts),
        local_record_orders: *local_record_orders,
        agent_orders_demo_only: *agent_orders_demo_only,
    }
}

fn order_shape(held: &OrderHeld) -> OrderShape {
    let OrderHeld {
        order,
        released,
        release_requested,
        ended,
        ending,
        conditions,
        checks,
        overridden_version,
        change_requests,
        placements,
        executions,
        charges,
        children,
        version,
        last_change,
        opened_by,
        opened_at_ns,
    } = held;
    OrderShape {
        order: pb(order),
        released: *released,
        release_requested: *release_requested,
        ended: ended.map(|state| state as i32),
        ending: ending.map(|state| state as i32),
        conditions: map_pb(conditions),
        checks: pbs(checks),
        overridden_version: *overridden_version,
        change_requests: change_requests.clone(),
        placements: placements.clone(),
        executions: executions.clone(),
        charges: charges.clone(),
        children: children.clone(),
        version: *version,
        last_change: opt(last_change),
        opened_by: opened_by.clone(),
        opened_at_ns: *opened_at_ns,
    }
}

fn placement_shape(held: &PlacementHeld) -> PlacementShape {
    let PlacementHeld {
        placement,
        state,
        conditions,
        checks,
        change_requests,
        executions,
        charges,
        last_change,
    } = held;
    PlacementShape {
        placement: pb(placement),
        state: *state as i32,
        conditions: map_pb(conditions),
        checks: pbs(checks),
        change_requests: change_requests.clone(),
        executions: executions.clone(),
        charges: charges.clone(),
        last_change: opt(last_change),
    }
}

fn execution_shape(held: &ExecutionHeld) -> ExecutionShape {
    let ExecutionHeld {
        execution,
        entry_id,
        sender,
        superseded_by,
        venue_account_code,
        venue_client_order_id,
        accepted_lines,
    } = held;
    ExecutionShape {
        execution: pb(execution),
        entry_id: entry_id.clone(),
        sender: sender.clone(),
        superseded_by: superseded_by.clone(),
        venue_account_code: venue_account_code.clone(),
        venue_client_order_id: venue_client_order_id.clone(),
        accepted_lines: accepted_lines.clone(),
    }
}

fn booking_shape(held: &BookingHeld) -> BookingShape {
    let BookingHeld {
        booking,
        state,
        held_reason,
        lines,
        version,
        held_alert,
        booked_at,
    } = held;
    BookingShape {
        booking: pb(booking),
        state: *state as i32,
        held_reason: held_reason.clone(),
        lines: lines.clone(),
        version: *version,
        held_alert: held_alert.clone(),
        booked_at: *booked_at,
    }
}

fn received_shape(held: &ReceivedHeld) -> ReceivedShape {
    let ReceivedHeld {
        booking_id,
        from_account,
        allocation,
        member_order_id,
        lines,
        withdrawn,
    } = held;
    ReceivedShape {
        booking_id: booking_id.clone(),
        from_account: from_account.clone(),
        allocation: pb(allocation),
        member_order_id: member_order_id.clone(),
        lines: lines.clone(),
        withdrawn: *withdrawn,
    }
}

// ── Reading ─────────────────────────────────────────────────────────────────

/// A snapshot's bytes as the account's projection; refused where they do
/// not read or are of another [`FORMAT`], which a store takes as no snapshot.
pub fn decode(bytes: &[u8]) -> Result<Book> {
    let shape: Shape = serde_json::from_slice(bytes)
        .map_err(|failed| StoreError::Invalid(format!("a snapshot does not read: {failed}")))?;
    if shape.format != FORMAT {
        return Err(StoreError::Invalid(format!(
            "a snapshot of format {} where this book reads {FORMAT}",
            shape.format
        )));
    }
    let chains: [u64; crate::book::CHAINS] = shape
        .chains
        .as_slice()
        .try_into()
        .map_err(|_| StoreError::Invalid("a snapshot's chains are not the book's".into()))?;
    Ok(Book {
        account_id: shape.account_id,
        partition: shape.partition,
        positions: shape
            .positions
            .into_iter()
            .map(|(key, position)| Ok((key, position_from(position)?)))
            .collect::<Result<_>>()?,
        breaks: unmap_pb(&shape.breaks)?,
        figures: unmap_pb(&shape.figures)?,
        attributes: unopt(&shape.attributes)?,
        opening: shape.opening,
        reversed_openings: shape.reversed_openings,
        entries: shape
            .entries
            .into_iter()
            .map(entry_from)
            .collect::<Result<_>>()?,
        chains,
        ledger: ledger_from(shape.ledger)?,
        waits_stated: shape.waits_stated,
        bodies: Default::default(),
    })
}

fn position_from(shape: PositionShape) -> Result<PositionState> {
    Ok(PositionState {
        instrument_id: shape.instrument_id,
        side: shape.side,
        sleeve: shape.sleeve,
        settled: unexact(&shape.settled)?,
        pending: shape
            .pending
            .iter()
            .map(|(date, quantity, state)| Ok((date.clone(), (unexact(quantity)?, unopt(state)?))))
            .collect::<Result<_>>()?,
        not_stated: unexact(&shape.not_stated)?,
        lots: shape
            .lots
            .iter()
            .map(|(id, lot, opened_in, ordinal)| {
                Ok((
                    id.clone(),
                    LotState {
                        lot: un(lot)?,
                        opened_in: *opened_in,
                        ordinal: *ordinal,
                    },
                ))
            })
            .collect::<Result<_>>()?,
        opened_lots: shape.opened_lots,
        opened_from: uns(&shape.opened_from)?,
        effective_date: shape.effective_date,
        last_change: unopt(&shape.last_change)?,
        removed: shape.removed,
        encumbrances: uns(&shape.encumbrances)?,
    })
}

fn entry_from(shape: EntryShape) -> Result<EntryIndex> {
    Ok(EntryIndex {
        entry_id: shape.entry_id,
        kind: shape.kind,
        effective_date: shape.effective_date,
        first: un(&shape.first)?,
        reversed_by: shape.reversed_by,
        moves_positions: shape.moves_positions,
        reverses: shape.reverses,
        acts: shape.acts,
        rolls: shape.rolls,
    })
}

fn ledger_from(shape: LedgerShape) -> Result<Ledger> {
    Ok(Ledger {
        orders: shape
            .orders
            .into_iter()
            .map(|(id, held)| Ok((id, order_from(held)?)))
            .collect::<Result<_>>()?,
        placements: shape
            .placements
            .into_iter()
            .map(|(id, held)| Ok((id, placement_from(held)?)))
            .collect::<Result<_>>()?,
        executions: shape
            .executions
            .into_iter()
            .map(|(id, held)| Ok((id, execution_from(held)?)))
            .collect::<Result<_>>()?,
        charges: unmap_pb(&shape.charges)?,
        changes: unmap_pb(&shape.changes)?,
        unplaced: unmap_pb(&shape.unplaced)?,
        locates: shape
            .locates
            .iter()
            .map(|(id, locate, uses)| {
                Ok((
                    id.clone(),
                    LocateHeld {
                        locate: un(locate)?,
                        uses: uns(uses)?,
                    },
                ))
            })
            .collect::<Result<_>>()?,
        instructions: unmap_pb(&shape.instructions)?,
        venue_facts: shape.venue_facts.into_iter().collect(),
        unplaced_executions: shape.unplaced_executions.into_iter().collect(),
        order_entries: shape.order_entries.into_iter().collect(),
        bookings: shape
            .bookings
            .into_iter()
            .map(|(id, held)| Ok((id, booking_from(held)?)))
            .collect::<Result<_>>()?,
        received: shape
            .received
            .into_iter()
            .map(|(key, held)| Ok((key, received_from(held)?)))
            .collect::<Result<_>>()?,
        sleeve_crosses: unmap_pb(&shape.sleeve_crosses)?,
        alerts: unmap_pb(&shape.alerts)?,
        local_record_orders: shape.local_record_orders,
        agent_orders_demo_only: shape.agent_orders_demo_only,
    })
}

fn order_from(shape: OrderShape) -> Result<OrderHeld> {
    Ok(OrderHeld {
        order: un(&shape.order)?,
        released: shape.released,
        release_requested: shape.release_requested,
        ended: shape
            .ended
            .map(|state| enum_of::<OrderState>(state, "order state"))
            .transpose()?,
        ending: shape
            .ending
            .map(|state| enum_of::<OrderState>(state, "order state"))
            .transpose()?,
        conditions: unmap_pb(&shape.conditions)?,
        checks: uns(&shape.checks)?,
        overridden_version: shape.overridden_version,
        change_requests: shape.change_requests,
        placements: shape.placements,
        executions: shape.executions,
        charges: shape.charges,
        children: shape.children,
        version: shape.version,
        last_change: unopt(&shape.last_change)?,
        opened_by: shape.opened_by,
        opened_at_ns: shape.opened_at_ns,
    })
}

fn placement_from(shape: PlacementShape) -> Result<PlacementHeld> {
    Ok(PlacementHeld {
        placement: un(&shape.placement)?,
        state: enum_of::<PlacementState>(shape.state, "placement state")?,
        conditions: unmap_pb(&shape.conditions)?,
        checks: uns(&shape.checks)?,
        change_requests: shape.change_requests,
        executions: shape.executions,
        charges: shape.charges,
        last_change: unopt(&shape.last_change)?,
    })
}

fn execution_from(shape: ExecutionShape) -> Result<ExecutionHeld> {
    Ok(ExecutionHeld {
        execution: un(&shape.execution)?,
        entry_id: shape.entry_id,
        sender: shape.sender,
        superseded_by: shape.superseded_by,
        venue_account_code: shape.venue_account_code,
        venue_client_order_id: shape.venue_client_order_id,
        accepted_lines: shape.accepted_lines,
    })
}

fn booking_from(shape: BookingShape) -> Result<BookingHeld> {
    Ok(BookingHeld {
        booking: un(&shape.booking)?,
        state: enum_of::<BookingState>(shape.state, "booking state")?,
        held_reason: shape.held_reason,
        lines: shape.lines,
        version: shape.version,
        held_alert: shape.held_alert,
        booked_at: shape.booked_at,
    })
}

fn received_from(shape: ReceivedShape) -> Result<ReceivedHeld> {
    Ok(ReceivedHeld {
        booking_id: shape.booking_id,
        from_account: shape.from_account,
        allocation: un(&shape.allocation)?,
        member_order_id: shape.member_order_id,
        lines: shape.lines,
        withdrawn: shape.withdrawn,
    })
}

// ── Resuming ────────────────────────────────────────────────────────────────

/// The account's book from a snapshot and the journal's tail after it: each
/// entry applied strictly, as a full replay applies it. The tail must begin
/// after the snapshot's last entry.
pub fn resume(mut book: Book, tail: &[crate::journal::Entry]) -> Result<Book> {
    for entry in tail {
        book.apply(entry, false)?;
    }
    Ok(book)
}

/// The journal sequence of a book's latest entry (its first change's): the
/// tail after a snapshot of it is every entry numbered above. Zero for a book
/// with no entry.
pub fn last_sequence(book: &Book) -> u64 {
    book.entries
        .last()
        .map(|entry| entry.first.sequence)
        .unwrap_or(0)
}

/// The latest effective date among a book's entries: a read by business date
/// may start from a snapshot only where none of the entries it holds is
/// effective after the date asked for.
pub fn latest_effective_date(book: &Book) -> String {
    book.entries
        .iter()
        .map(|entry| entry.effective_date.as_str())
        .max()
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
pub(crate) mod tests;
