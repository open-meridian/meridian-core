//! Booking: an order's executions booked to the accounts they were for
//! (W11.26 to W11.37, contract v24; the order-path plan's "v24: booking
//! (slice B)", items 1 to 24, every name approved 2026-10-13 and 2026-10-14).
//!
//! **A booking is in its order's account's partition** (decisions/024: no act
//! writes two partitions). Booking re-states the fill-time lots by account:
//! in the order's account (the allocation account, for a block) its entry
//! closes the fill's lots with no realised result and opens one lot per
//! allocation to that account, at the allocation's average price plus its
//! capitalised fees; an allocation to another account moves out of this one
//! here, and into that one by a child entry in that account's own partition,
//! naming the booking ([`receive_allocation`]), which the service makes after
//! the booking commits, each idempotent by its key, so a crash between them
//! is put right by the next sweep ([`crate::booking_service`]).
//!
//! **Core checks exactly and polices nothing else** (step 5's Q1, Q16, Q29):
//! quantities sum to the executions booked, gross to their gross, each fee
//! kind to theirs; no tolerance, the residual on a named allocation. Whether
//! a capitalised fee enters a lot's cost, which lots a jurisdiction lets an
//! account relieve, and fairness across a block's members are the plugins'.
//!
//! **States are a fold** (decisions/035 as amended at v24): booked, held,
//! handed off, closed and cancelled, each move a row of the generated table
//! ([`crate::transitions::BOOKING_TRANSITIONS`], rows b1 to b9).

use std::collections::{BTreeMap, BTreeSet};

use meridian_domain::exact::Exact;
use meridian_domain::v1::{
    Alert, AlertKind, AlertState, AlertSubject, Allocation, BookOrderRequest, Booking,
    BookingCorrectionKind, BookingState, CorrectBookingRequest, ErrorCause, Execution,
    GiveUpAnswer, GiveUpRecord, HandOffBookingRequest, HoldingSide, LotReliefMethod, LotSource,
    LotTerms, Money, MovementLine, OrderState, RaiseAlertRequest, RecordSleeveCrossRequest,
    ReportGiveUpRequest, SetAllocationInstructionRequest, SetLocalRecordOrdersRequest,
    SettlementBucket, Side, SleeveCross,
};
use meridian_pb::v1::RefusalReason;

use crate::book::{Book, Key};
use crate::decide::{attributes_of, Context, Made};
use crate::ids;
use crate::journal::Body;
use crate::numbers;
use crate::order_decide::{
    amount_of, cash_instrument, currency_lines, decimal, fact_actor, finish, held, in_scope,
    malformed, open_lots, person_required, relief, state_of, state_refused, user_required, Plan,
};
use crate::orders::{self, Act};
use crate::store::{Result, StoreError};
use crate::transitions::{From, To, Transition, BOOKING_TRANSITIONS};

/// A booking's minted identifiers (decisions/035, point 7; approved
/// 2026-10-14).
pub const BOOKING: &str = "BKG";
pub const ALLOCATION: &str = "ALC";
pub const ALERT: &str = "ALR";
pub const SLEEVE_CROSS: &str = "SXC";

/// A booking's entry kinds (W9.8's open list; approved 2026-10-14).
pub const BOOKED: &str = "booking-booked";
pub const HELD: &str = "booking-held";
pub const HANDED_OFF: &str = "booking-handed-off";
pub const CLOSED: &str = "booking-closed";
pub const CANCELLED: &str = "booking-cancelled";
pub const CORRECTED: &str = "booking-corrected";
pub const ALLOCATION_MOVED: &str = "allocation-moved";

/// A block's member, read from its own account before the lock (W11.1).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemberView {
    pub order_id: String,
    pub account_id: String,
    pub instrument_id: String,
    pub side: i32,
    pub state: i32,
    pub idle: Exact,
    /// The accounts its allocation instruction names; its own where none.
    pub instructed: Vec<String>,
    /// A live block naming it already, where one does (question 27).
    pub in_block: String,
}

// ── The table (decisions/035 as amended at v24) ────────────────────────────

pub fn booking_row(id: &str) -> &'static Transition<BookingState> {
    BOOKING_TRANSITIONS
        .iter()
        .find(|row| row.row == id)
        .unwrap_or_else(|| panic!("the transition table names no booking row {id}"))
}

fn starts_from(row: &Transition<BookingState>, from: Option<BookingState>) -> bool {
    row.from.iter().any(|held| match held {
        From::Any => true,
        From::None => from.is_none(),
        From::State(state) => Some(*state) == from,
    })
}

fn reaches(row: &Transition<BookingState>, from: BookingState, to: BookingState) -> bool {
    to == from
        || row.to.iter().any(|held| match held {
            To::Unchanged => to == from,
            To::State(state) => *state == to,
        })
}

pub fn booking_state_named(state: BookingState) -> &'static str {
    match state {
        BookingState::Booked => "booked",
        BookingState::Held => "held",
        BookingState::HandedOff => "handed off",
        BookingState::Closed => "closed",
        BookingState::Cancelled => "cancelled",
        BookingState::Unspecified => "unknown to the book",
    }
}

fn booking_refused(row_id: &str, state: BookingState, what: &str) -> StoreError {
    let row = booking_row(row_id);
    StoreError::refused(
        RefusalReason::OrderState,
        format!(
            "{what} is not allowed on a booking that is {} (the transition table's row {}: {})",
            booking_state_named(state),
            row.row,
            row.cause
        ),
    )
}

// ── What was executed, and what an allocation set ties out to ──────────────

/// The executions a booking books, summed as the check reads them: the
/// quantity, the gross (quantity times price, exact), and each fee kind.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Executed {
    pub quantity: Exact,
    pub gross: Exact,
    pub fees: BTreeMap<i32, Exact>,
    pub instrument_id: String,
    pub side: i32,
    pub sleeve: String,
    pub trade_date: String,
    pub value_date: String,
    /// The price's money, naming its currency, which the cash moves in.
    pub price: Money,
}

fn value_date_of(execution: &Execution) -> String {
    if execution.settlement_date.is_empty() {
        execution.computed_settlement_date.clone()
    } else {
        execution.settlement_date.clone()
    }
}

/// An execution as it stands now: itself, or what superseded it, a bust
/// counting nothing (W11.11). What a hand-off's check and a replace compare.
fn as_it_stands<'a>(book: &'a Book, execution_id: &str) -> Option<&'a Execution> {
    let mut current = book.ledger.executions.get(execution_id)?;
    let mut seen = BTreeSet::new();
    while let Some(next) = current.superseded_by.as_ref() {
        if !seen.insert(next.clone()) {
            break;
        }
        match book.ledger.executions.get(next) {
            Some(held) => current = held,
            None => break,
        }
    }
    if current.is_bust() {
        return None;
    }
    Some(&current.execution)
}

/// The executions summed, each as it stands now.
pub fn executed(book: &Book, execution_ids: &[String]) -> Result<Executed> {
    let mut out = Executed::default();
    for id in execution_ids {
        let Some(execution) = as_it_stands(book, id) else {
            continue;
        };
        let quantity = numbers::required("execution.quantity", execution.quantity.as_ref())?;
        let price = execution
            .price
            .clone()
            .ok_or_else(|| malformed(&["price"], format!("execution {id} states no price")))?;
        let unit = numbers::amount("execution.price", &price)?;
        out.quantity = numbers::add("executed quantity", out.quantity, quantity)?;
        out.gross = numbers::add(
            "executed gross",
            out.gross,
            orders::multiply("gross", quantity, unit)?,
        )?;
        for fee in &execution.fees {
            let amount = fee
                .amount
                .as_ref()
                .map(|amount| numbers::amount("fee", amount))
                .transpose()?
                .unwrap_or(Exact::ZERO);
            let held = out.fees.entry(fee.kind).or_insert(Exact::ZERO);
            *held = numbers::add("executed fees", *held, amount)?;
        }
        if out.instrument_id.is_empty() {
            out.instrument_id = execution.instrument_id.clone();
            out.side = execution.side;
            out.sleeve = execution.sleeve.clone();
            out.trade_date = execution.trade_date.clone();
            out.value_date = value_date_of(execution);
            out.price = price;
        }
    }
    Ok(out)
}

/// The allocations summed the same way.
fn allocated(allocations: &[Allocation]) -> Result<(Exact, Exact, BTreeMap<i32, Exact>)> {
    let (mut quantity, mut gross) = (Exact::ZERO, Exact::ZERO);
    let mut fees: BTreeMap<i32, Exact> = BTreeMap::new();
    for (index, allocation) in allocations.iter().enumerate() {
        let field = format!("allocations[{index}]");
        quantity = numbers::add(
            "allocated quantity",
            quantity,
            numbers::required(&format!("{field}.quantity"), allocation.quantity.as_ref())?,
        )?;
        gross = numbers::add(
            "allocated gross",
            gross,
            numbers::amount(
                &format!("{field}.gross_amount"),
                allocation
                    .gross_amount
                    .as_ref()
                    .ok_or_else(|| malformed(&[&format!("{field}.gross_amount")], "required"))?,
            )?,
        )?;
        for (at, fee) in allocation.fees.iter().enumerate() {
            let amount = fee
                .amount
                .as_ref()
                .ok_or_else(|| {
                    malformed(
                        &[&format!("{field}.fees[{at}].amount")],
                        "a fee states its amount",
                    )
                })
                .and_then(|amount| {
                    numbers::amount(&format!("{field}.fees[{at}].amount"), amount)
                })?;
            let held = fees.entry(fee.kind).or_insert(Exact::ZERO);
            *held = numbers::add("allocated fees", *held, amount)?;
        }
    }
    Ok((quantity, gross, fees))
}

/// Whether the allocations tie out exactly to what was executed (W11.27,
/// W11.28): `None` where they do, else the words saying what differs.
fn ties_out(executed: &Executed, allocations: &[Allocation]) -> Result<Option<String>> {
    let (quantity, gross, fees) = allocated(allocations)?;
    if quantity != executed.quantity {
        return Ok(Some(format!(
            "the allocations' quantities sum to {quantity} and the executions booked to {}",
            executed.quantity
        )));
    }
    if gross != executed.gross {
        return Ok(Some(format!(
            "the allocations' gross sums to {gross} and the executions' to {}; the residual \
             goes on a named allocation",
            executed.gross
        )));
    }
    let kinds: BTreeSet<i32> = fees.keys().chain(executed.fees.keys()).copied().collect();
    for kind in kinds {
        let a = fees.get(&kind).copied().unwrap_or(Exact::ZERO);
        let e = executed.fees.get(&kind).copied().unwrap_or(Exact::ZERO);
        if a != e {
            let named = meridian_domain::v1::FeeKind::try_from(kind)
                .map(|kind| kind.as_str_name())
                .unwrap_or("unspecified");
            return Ok(Some(format!(
                "the allocations' {named} fees sum to {a} and the executions' to {e}"
            )));
        }
    }
    Ok(None)
}

// ── Who may be allocated to ────────────────────────────────────────────────

/// The accounts an order's instruction names (W11.26), or its own where it
/// names none; for a block, its members' (W11.1).
fn instructed_accounts(book: &Book, ctx: &Context, order_id: &str) -> BTreeSet<String> {
    let mut accounts = BTreeSet::new();
    let Some(held) = book.ledger.orders.get(order_id) else {
        return accounts;
    };
    if !held.order.block_member_order_ids.is_empty() {
        for member in &held.order.block_member_order_ids {
            if let Some(view) = ctx.members.get(member) {
                accounts.extend(view.instructed.iter().cloned());
            }
        }
        return accounts;
    }
    match held.order.allocation_instruction.as_ref() {
        Some(instruction) if !instruction.lines.is_empty() => {
            accounts.extend(instruction.lines.iter().map(|line| line.account_id.clone()))
        }
        _ => {
            accounts.insert(book.account_id.clone());
        }
    }
    accounts
}

fn is_error_account(ctx: &Context, book: &Book, account_id: &str) -> bool {
    if account_id == book.account_id {
        return attributes_of(book).error_account;
    }
    ctx.account_marks
        .get(account_id)
        .is_some_and(|attributes| attributes.error_account)
}

/// Each allocation held to who may receive it, and its own fields: an account
/// the instruction names or one marked an error account; an error-account
/// allocation's reason and cause, text with other (W11.27).
fn allocations_hold(
    book: &Book,
    ctx: &Context,
    order_id: &str,
    allocations: &[Allocation],
) -> Result<()> {
    if allocations.is_empty() {
        return Err(malformed(
            &["allocations"],
            "a booking allocates to at least one account",
        ));
    }
    let instructed = instructed_accounts(book, ctx, order_id);
    for (index, allocation) in allocations.iter().enumerate() {
        let field = format!("allocations[{index}]");
        if allocation.account_id.is_empty() {
            return Err(malformed(
                &[&format!("{field}.account_id")],
                "an allocation names its account",
            ));
        }
        match decimal(&format!("{field}.quantity"), allocation.quantity.as_ref())? {
            Some(quantity) if !quantity.is_negative() => {}
            _ => {
                return Err(malformed(
                    &[&format!("{field}.quantity")],
                    "an allocation states its quantity, never below zero",
                ))
            }
        }
        if allocation.average_price.is_none() {
            return Err(malformed(
                &[&format!("{field}.average_price")],
                "an allocation states the average price its confirmation states",
            ));
        }
        let error = is_error_account(ctx, book, &allocation.account_id);
        if !error && !instructed.contains(&allocation.account_id) {
            return Err(StoreError::Refused {
                reason: RefusalReason::AccountNotInstructed,
                words: format!(
                    "account {} is neither named by order {order_id}'s allocation instruction \
                     nor marked an error account; portfolio adds it to the instruction, with a \
                     reason, before it is booked to",
                    allocation.account_id
                ),
                fields: vec![format!("{field}.account_id")],
            });
        }
        if error {
            if allocation.reason.trim().is_empty() {
                return Err(StoreError::Refused {
                    reason: RefusalReason::ReasonRequired,
                    words: format!(
                        "an allocation to error account {} carries its reason",
                        allocation.account_id
                    ),
                    fields: vec![format!("{field}.reason")],
                });
            }
            match ErrorCause::try_from(allocation.error_cause) {
                Ok(ErrorCause::Unspecified) | Err(_) => {
                    return Err(malformed(
                        &[&format!("{field}.error_cause")],
                        "an allocation to an error account names its cause",
                    ))
                }
                Ok(ErrorCause::Other) if allocation.error_text.trim().is_empty() => {
                    return Err(malformed(
                        &[&format!("{field}.error_text")],
                        "an allocation whose cause is other says why in its text, as FIX's \
                         Text (58)",
                    ))
                }
                _ => {}
            }
        } else if allocation.error_cause != ErrorCause::Unspecified as i32 {
            return Err(malformed(
                &[&format!("{field}.error_cause")],
                format!(
                    "account {} is not marked an error account, so its allocation names no \
                     error cause",
                    allocation.account_id
                ),
            ));
        }
    }
    Ok(())
}

// ── The lines a booking moves ──────────────────────────────────────────────

/// The lots the booked executions opened, which booking closes first.
fn fill_lots(book: &Book, execution_ids: &[String]) -> Vec<String> {
    let mut lots = Vec::new();
    for id in execution_ids {
        let Some(held) = book.ledger.executions.get(id) else {
            continue;
        };
        let (entry_id, range) = match &held.accepted_lines {
            Some((entry_id, start, count)) => (entry_id.clone(), Some((*start, *count))),
            None => (held.entry_id.clone(), None),
        };
        let Some(entry) = book.entry(&entry_id) else {
            continue;
        };
        let lines: &[MovementLine] = match range {
            Some((start, count)) => entry
                .body
                .lines
                .get(start..start + count)
                .unwrap_or_default(),
            None => &entry.body.lines,
        };
        for line in lines {
            if line.opens_lot.is_some()
                && crate::became::same(&line.instrument_id, &held.execution.instrument_id)
            {
                lots.push(line.lot_id.clone());
            }
        }
    }
    lots
}

fn method_of(book: &Book) -> LotReliefMethod {
    LotReliefMethod::try_from(attributes_of(book).lot_relief_default)
        .unwrap_or(LotReliefMethod::Unspecified)
}

fn signed_line(
    instrument: &str,
    side: HoldingSide,
    value_date: &str,
    sleeve: &str,
    quantity: Exact,
) -> MovementLine {
    MovementLine {
        instrument_id: instrument.to_string(),
        side: side as i32,
        bucket: if value_date.is_empty() {
            SettlementBucket::Settled as i32
        } else {
            SettlementBucket::Pending as i32
        },
        value_date: value_date.to_string(),
        quantity: numbers::wire(quantity),
        sleeve: sleeve.to_string(),
        ..Default::default()
    }
}

/// The security moved into or out of a position by `delta` (positive in):
/// a receipt covering a short first and opening a lot for the rest with
/// `terms`; a removal relieving lots, `named` first, and shorting beyond
/// them. The account's base currency is a balance line (W11.13).
#[allow(clippy::too_many_arguments)]
fn security_lines(
    book: &Book,
    ctx: &Context,
    instrument: &str,
    delta: Exact,
    value_date: &str,
    sleeve: &str,
    terms: LotTerms,
    named: &[String],
    method: LotReliefMethod,
    earlier: &[MovementLine],
) -> Result<Vec<MovementLine>> {
    if delta.is_zero() {
        return Ok(Vec::new());
    }
    if book.is_base(instrument) {
        return Ok(vec![signed_line(
            instrument,
            HoldingSide::Long,
            value_date,
            sleeve,
            delta,
        )]);
    }
    let long: Key = (
        instrument.to_string(),
        HoldingSide::Long as i32,
        sleeve.to_string(),
    );
    let short: Key = (
        instrument.to_string(),
        HoldingSide::Short as i32,
        sleeve.to_string(),
    );
    let mut lines = Vec::new();
    let opening = |side: HoldingSide, magnitude: Exact| -> MovementLine {
        let mut line = signed_line(
            instrument,
            side,
            value_date,
            sleeve,
            if side == HoldingSide::Short {
                magnitude.negated()
            } else {
                magnitude
            },
        );
        line.lot_id = ctx.mint(ids::LOT);
        line.opens_lot = Some(terms.clone());
        line
    };
    if !delta.is_negative() {
        let mut rest = delta;
        for (lot_id, take) in relief(open_lots(book, &short, earlier)?, delta, method, named)? {
            let mut line = signed_line(instrument, HoldingSide::Short, value_date, sleeve, take);
            line.lot_id = lot_id;
            lines.push(line);
            rest = numbers::add("cover", rest, take.negated())?;
        }
        if !rest.is_zero() {
            lines.push(opening(HoldingSide::Long, rest));
        }
    } else {
        let out = delta.negated();
        let mut rest = out;
        for (lot_id, take) in relief(open_lots(book, &long, earlier)?, out, method, named)? {
            let mut line = signed_line(
                instrument,
                HoldingSide::Long,
                value_date,
                sleeve,
                take.negated(),
            );
            line.lot_id = lot_id;
            lines.push(line);
            rest = numbers::add("relief", rest, take.negated())?;
        }
        if !rest.is_zero() {
            lines.push(opening(HoldingSide::Short, rest));
        }
    }
    Ok(lines)
}

/// What a lot an allocation opens is acquired at (W11.27, item 3): the trade
/// date, the allocation's average price, and its cost its gross plus its
/// capitalised fees in the same currency.
fn allocation_terms(allocation: &Allocation, executed: &Executed) -> Result<LotTerms> {
    let gross = allocation.gross_amount.clone().unwrap_or_default();
    let mut cost = numbers::amount("gross_amount", &gross)?;
    for fee in allocation.fees.iter().filter(|fee| fee.capitalised) {
        if let Some(amount) = fee.amount.as_ref() {
            if amount.currency_code == gross.currency_code
                && amount.instrument_id == gross.instrument_id
            {
                cost = numbers::add("cost", cost, numbers::amount("fee", amount)?)?;
            }
        }
    }
    Ok(LotTerms {
        unit_cost: allocation.average_price.clone(),
        cost: Some(Money {
            amount: numbers::wire(cost),
            ..gross
        }),
        acquired_date: executed.trade_date.clone(),
        holding_period_start: String::new(),
        settlement_date: if allocation.intended_settlement_date.is_empty() {
            executed.value_date.clone()
        } else {
            allocation.intended_settlement_date.clone()
        },
        source: LotSource::Execution as i32,
    })
}

fn buys(side: i32) -> bool {
    Side::try_from(side) == Ok(Side::Buy)
}

/// The lines of a booking in the order's account: per allocation to this
/// account, a buy's fill lots closed and its own lot opened (a sale's lots
/// unchanged, item 4); per allocation to another account, its security,
/// cash and fees moved out, the other side of its child entry (W11.30).
fn booking_lines(
    book: &Book,
    ctx: &Context,
    executed: &Executed,
    execution_ids: &[String],
    allocations: &[Allocation],
    earlier: &[MovementLine],
) -> Result<Vec<MovementLine>> {
    let mut lines: Vec<MovementLine> = Vec::new();
    let mut seen: Vec<MovementLine> = earlier.to_vec();
    let fills = fill_lots(book, execution_ids);
    let method = method_of(book);
    let buy = buys(executed.side);
    let instrument = crate::order_decide::landing(book, &executed.instrument_id);
    let sleeve = executed.sleeve.clone();
    let date = executed.value_date.clone();
    for allocation in allocations {
        let quantity = numbers::required("allocation.quantity", allocation.quantity.as_ref())?;
        let terms = allocation_terms(allocation, executed)?;
        let mut made = Vec::new();
        if allocation.account_id == book.account_id {
            if buy {
                made.extend(security_lines(
                    book,
                    ctx,
                    &instrument,
                    quantity.negated(),
                    &date,
                    &sleeve,
                    terms.clone(),
                    &fills,
                    method,
                    &seen,
                )?);
                seen.extend(made.iter().cloned());
                let reopened = security_lines(
                    book,
                    ctx,
                    &instrument,
                    quantity,
                    &date,
                    &allocation.sleeve,
                    terms,
                    &[],
                    method,
                    &seen,
                )?;
                seen.extend(reopened.iter().cloned());
                made.extend(reopened);
            }
        } else {
            // Out of this account: the opposite of what the fills moved.
            let delta = if buy { quantity.negated() } else { quantity };
            made.extend(security_lines(
                book,
                ctx,
                &instrument,
                delta,
                &date,
                &sleeve,
                terms,
                &fills,
                method,
                &seen,
            )?);
            seen.extend(made.iter().cloned());
            let gross = allocation.gross_amount.clone().unwrap_or_default();
            let cash = cash_instrument(&gross)?;
            let value = numbers::amount("gross_amount", &gross)?;
            let moved = currency_lines(
                book,
                ctx,
                &cash,
                if buy { value } else { value.negated() },
                &date,
                &sleeve,
                &executed.trade_date,
                None,
                method,
                &[],
                &seen,
            )?;
            seen.extend(moved.iter().cloned());
            made.extend(moved);
            for fee in &allocation.fees {
                let Some(amount) = fee.amount.as_ref() else {
                    continue;
                };
                let moved = currency_lines(
                    book,
                    ctx,
                    &cash_instrument(amount)?,
                    numbers::amount("fee", amount)?,
                    &date,
                    &sleeve,
                    &executed.trade_date,
                    None,
                    method,
                    &[],
                    &seen,
                )?;
                seen.extend(moved.iter().cloned());
                made.extend(moved);
            }
        }
        lines.extend(made);
    }
    Ok(lines)
}

/// The lines of an allocation received from another account's booking, in
/// the receiving account: its security in at the allocation's average price
/// plus its capitalised fees on a buy, or out by its own lots on a sale; its
/// cash and each fee as the fills moved them (W11.27, W11.30).
#[allow(clippy::too_many_arguments)]
fn received_lines(
    book: &Book,
    ctx: &Context,
    allocation: &Allocation,
    instrument: &str,
    side: i32,
    trade_date: &str,
    value_date: &str,
    reverse: bool,
) -> Result<Vec<MovementLine>> {
    let quantity = numbers::required("allocation.quantity", allocation.quantity.as_ref())?;
    let buy = buys(side) != reverse;
    let date = if allocation.intended_settlement_date.is_empty() {
        value_date.to_string()
    } else {
        allocation.intended_settlement_date.clone()
    };
    let executed = Executed {
        trade_date: trade_date.to_string(),
        value_date: date.clone(),
        ..Default::default()
    };
    let terms = allocation_terms(allocation, &executed)?;
    let method = method_of(book);
    let sleeve = allocation.sleeve.clone();
    let mut lines = security_lines(
        book,
        ctx,
        instrument,
        if buy { quantity } else { quantity.negated() },
        &date,
        &sleeve,
        terms,
        &allocation.relieve_lot_ids,
        method,
        &[],
    )?;
    let gross = allocation.gross_amount.clone().unwrap_or_default();
    let value = numbers::amount("gross_amount", &gross)?;
    let moved = currency_lines(
        book,
        ctx,
        &cash_instrument(&gross)?,
        if buy { value.negated() } else { value },
        &date,
        &sleeve,
        trade_date,
        None,
        method,
        &[],
        &lines,
    )?;
    lines.extend(moved);
    for fee in &allocation.fees {
        let Some(amount) = fee.amount.as_ref() else {
            continue;
        };
        let fee_value = numbers::amount("fee", amount)?;
        let moved = currency_lines(
            book,
            ctx,
            &cash_instrument(amount)?,
            if reverse {
                fee_value
            } else {
                fee_value.negated()
            },
            &date,
            &sleeve,
            trade_date,
            None,
            method,
            &[],
            &lines,
        )?;
        lines.extend(moved);
    }
    Ok(lines)
}

/// An entry's lines between `start` and `start + count`, negated: what a
/// cancel or a replace reverses, a lot it opened relieved to zero and one it
/// relieved reopened by the same identifiers (W11.29).
fn negated(book: &Book, entry_id: &str, start: usize, count: usize) -> Vec<MovementLine> {
    let Some(at) = book
        .entries
        .iter()
        .position(|held| held.entry_id == entry_id)
    else {
        return Vec::new();
    };
    book.entries[at]
        .body
        .lines
        .get(start..start + count)
        .unwrap_or_default()
        .iter()
        .map(|line| {
            let mut out = line.clone();
            out.opens_lot = None;
            if line.bucket == SettlementBucket::Pending as i32 {
                out.value_date = crate::order_decide::rolled_to(book, at, line);
            }
            out.quantity = line.quantity.as_ref().map(|quantity| {
                Exact::from_wire(quantity)
                    .map(|value| value.negated().to_wire())
                    .unwrap_or(*quantity)
            });
            out
        })
        .collect()
}

// ── Alerts the book raises (W11.34) ────────────────────────────────────────

fn alert(
    ctx: &Context,
    account_id: &str,
    kind: AlertKind,
    reason: String,
    subject: AlertSubject,
) -> Act {
    Act::AlertRaised {
        alert: Alert {
            alert_id: ctx.mint(ALERT),
            account_id: account_id.to_string(),
            kind: kind as i32,
            state: AlertState::Raised as i32,
            reason,
            subject: Some(subject),
            // The operations plugin's affirmation_deadline has no carrier in
            // the contract yet, so a held booking's alert names no moment it
            // is due by; zero, as the field reads for every other alert.
            due_by: 0,
            last_change: None,
        },
    }
}

fn error_alerts(ctx: &Context, book: &Book, booking: &Booking) -> Vec<Act> {
    booking
        .allocations
        .iter()
        .filter(|allocation| allocation.error_cause != ErrorCause::Unspecified as i32)
        .map(|allocation| {
            alert(
                ctx,
                &book.account_id,
                AlertKind::ErrorAccountBooking,
                format!(
                    "booked {} to error account {}: {}",
                    allocation
                        .quantity
                        .as_ref()
                        .and_then(|quantity| Exact::from_wire(quantity).ok())
                        .map(|quantity| quantity.to_string())
                        .unwrap_or_default(),
                    allocation.account_id,
                    allocation.reason
                ),
                AlertSubject {
                    order_id: booking.order_id.clone(),
                    booking_id: booking.booking_id.clone(),
                    allocation_id: allocation.allocation_id.clone(),
                    position: None,
                },
            )
        })
        .collect()
}

fn held_alert(ctx: &Context, book: &Book, booking: &Booking, reason: &str) -> Act {
    alert(
        ctx,
        &book.account_id,
        AlertKind::BookingHeld,
        format!("booking {} held: {reason}", booking.booking_id),
        AlertSubject {
            order_id: booking.order_id.clone(),
            booking_id: booking.booking_id.clone(),
            allocation_id: String::new(),
            position: None,
        },
    )
}

// ── W11.26: an order's allocation instruction ──────────────────────────────

pub fn set_allocation_instruction(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &SetAllocationInstructionRequest,
) -> Result<Made> {
    user_required(ctx, "setting an allocation instruction")?;
    if request.reason.trim().is_empty() {
        return Err(StoreError::refused(
            RefusalReason::ReasonRequired,
            "an allocation instruction is amended with a reason",
        ));
    }
    in_scope(ctx, &book.account_id)?;
    let order = held(book, &request.order_id)?;
    let instruction = request.allocation_instruction.clone().unwrap_or_default();
    // What its live bookings allocate already, per account: an amendment
    // may not take an account's amount below it (the store's test for
    // "already booked", W11.26).
    let mut booked: BTreeMap<String, Exact> = BTreeMap::new();
    for held in book.ledger.bookings.values() {
        if held.booking.order_id != request.order_id || held.state == BookingState::Cancelled {
            continue;
        }
        for allocation in &held.booking.allocations {
            if allocation.error_cause != ErrorCause::Unspecified as i32 {
                continue;
            }
            let quantity = numbers::required("allocation.quantity", allocation.quantity.as_ref())?;
            let entry = booked
                .entry(allocation.account_id.clone())
                .or_insert(Exact::ZERO);
            *entry = numbers::add("booked", *entry, quantity)?;
        }
    }
    for (account, quantity) in &booked {
        let line = instruction
            .lines
            .iter()
            .find(|line| &line.account_id == account);
        let amount = match line {
            None => Some(Exact::ZERO),
            Some(line) if line.weight.is_some() && line.quantity.is_none() => None,
            Some(line) => decimal(
                "allocation_instruction.lines.quantity",
                line.quantity.as_ref(),
            )?,
        };
        match amount {
            Some(amount) if amount < *quantity => {
                return Err(StoreError::refused(
                    RefusalReason::OrderState,
                    format!(
                        "account {account}'s amount would fall to {amount}, below the {quantity} \
                         order {}'s live bookings allocate it already",
                        request.order_id
                    ),
                ))
            }
            None => {
                return Err(StoreError::refused(
                    RefusalReason::OrderState,
                    format!(
                        "a weight would re-split the {quantity} order {}'s live bookings \
                         allocate account {account} already",
                        request.order_id
                    ),
                ))
            }
            _ => {}
        }
    }
    let mut terms = order.order.clone();
    terms.allocation_instruction = Some(instruction);
    let plan = Plan::new(
        "allocation-instruction-set",
        ctx.actor(),
        vec![Act::Terms { order: terms }],
    )
    .reason(&request.reason);
    finish(book, head, ctx, plan)
}

// ── W11.27: book an order's executions ─────────────────────────────────────

/// The top of an execution's order chain: a child's executions are booked
/// as its parent's (W11.1).
fn booked_on(book: &Book, execution: &Execution) -> String {
    book.ledger.top(&execution.order_id)
}

pub fn book_order(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &BookOrderRequest,
) -> Result<Made> {
    user_required(ctx, "booking an order")?;
    in_scope(ctx, &book.account_id)?;
    let order = held(book, &request.order_id)?;
    if request.execution_ids.is_empty() {
        return Err(malformed(
            &["execution_ids"],
            "a booking books an explicit list of executions",
        ));
    }
    let mut seen = BTreeSet::new();
    let mut first: Option<&Execution> = None;
    for (index, id) in request.execution_ids.iter().enumerate() {
        let field = format!("execution_ids[{index}]");
        if !seen.insert(id.as_str()) {
            return Err(malformed(&[&field], format!("names execution {id} twice")));
        }
        let Some(held) = book.ledger.executions.get(id) else {
            return Err(malformed(
                &[&field],
                format!("account {} holds no execution {id}", book.account_id),
            ));
        };
        let execution = &held.execution;
        if booked_on(book, execution) != request.order_id {
            return Err(malformed(
                &[&field],
                format!(
                    "execution {id} is on order {}, not {}: a booking books one order's \
                     executions, its children's among them",
                    execution.order_id, request.order_id
                ),
            ));
        }
        let why = if execution.state != meridian_domain::v1::ExecutionState::Matched as i32 {
            Some("it is not matched to the order".to_string())
        } else if let Some(by) = &held.superseded_by {
            Some(format!("execution {by} supersedes it"))
        } else if held.is_bust() {
            Some("it is a bust".to_string())
        } else {
            book.ledger
                .booked_in(id)
                .map(|booking| format!("it is in live booking {}", booking.booking.booking_id))
        };
        if let Some(why) = why {
            return Err(StoreError::Refused {
                reason: RefusalReason::OrderState,
                words: format!(
                    "execution {id} cannot be booked: {why} (the transition table's row b1)"
                ),
                fields: vec![field],
            });
        }
        if let Some(first) = first {
            let differs = if !crate::became::same(&first.instrument_id, &execution.instrument_id) {
                Some("instrument")
            } else if first.side != execution.side {
                Some("side")
            } else if first.trade_date != execution.trade_date {
                Some("trade date")
            } else if value_date_of(first) != value_date_of(execution) {
                Some("settlement date")
            } else {
                None
            };
            if let Some(what) = differs {
                return Err(malformed(
                    &[&field],
                    format!(
                        "execution {id}'s {what} differs from execution {}'s: a booking's \
                         executions share instrument, side, trade date and settlement date, as \
                         FIX's AllocationInstruction (J) requires of NoExecs",
                        first.execution_id
                    ),
                ));
            }
        } else {
            first = Some(execution);
        }
    }
    allocations_hold(book, ctx, &request.order_id, &request.allocations)?;
    let executed = executed(book, &request.execution_ids)?;
    if let Some(words) = ties_out(&executed, &request.allocations)? {
        return Err(StoreError::refused(RefusalReason::BookingUnbalanced, words));
    }
    let booking_id = ctx.mint(BOOKING);
    let allocations: Vec<Allocation> = request
        .allocations
        .iter()
        .map(|allocation| Allocation {
            allocation_id: ctx.mint(ALLOCATION),
            give_up_answers: Vec::new(),
            intended_settlement_date: if allocation.intended_settlement_date.is_empty() {
                executed.value_date.clone()
            } else {
                allocation.intended_settlement_date.clone()
            },
            ..allocation.clone()
        })
        .collect();
    let booking = Booking {
        booking_id: booking_id.clone(),
        order_id: request.order_id.clone(),
        account_id: book.account_id.clone(),
        execution_ids: request.execution_ids.clone(),
        allocations,
        ..Default::default()
    };
    let lines = booking_lines(
        book,
        ctx,
        &executed,
        &request.execution_ids,
        &booking.allocations,
        &[],
    )?;
    let mut acts = vec![Act::BookingOpened {
        booking: booking.clone(),
        lines: (0, lines.len()),
    }];
    acts.extend(error_alerts(ctx, book, &booking));
    let _ = order;
    let mut plan = Plan::new(BOOKED, ctx.actor(), acts);
    plan.lines = lines;
    plan.effective_date = executed.trade_date.clone();
    let made = finish(book, head, ctx, plan)?;
    if !request.hand_off {
        return Ok(made);
    }
    // Ready-To-Book in one step (item 8; row b2): handed off in the same act
    // where the check passes, booked where it does not.
    if let Some(why) = hand_off_check(&made.book, &booking_id)? {
        let _ = why;
        return Ok(made);
    }
    let mut plan = Plan::new(BOOKED, ctx.actor(), Vec::new());
    plan.acts = made.entry.body.acts.clone();
    plan.acts.push(Act::BookingHandedOff {
        booking_id: booking_id.clone(),
    });
    plan.lines = made.entry.body.lines.clone();
    plan.effective_date = executed.trade_date.clone();
    finish(book, head, ctx, plan)
}

/// Whether a booking may be handed off (W11.28): executed equal to
/// allocated, in quantity, gross and each fee kind, the executions as they
/// stand now; no fee reported since; and, in an allocation account, nothing
/// left in the instrument. `None` where it may, else why not.
pub fn hand_off_check(book: &Book, booking_id: &str) -> Result<Option<String>> {
    let held =
        book.ledger.bookings.get(booking_id).ok_or_else(|| {
            StoreError::Invalid(format!("the book holds no booking {booking_id}"))
        })?;
    let executed = executed(book, &held.booking.execution_ids)?;
    if let Some(words) = ties_out(&executed, &held.booking.allocations)? {
        return Ok(Some(words));
    }
    if !held.held_reason.is_empty() && held.state == BookingState::Held {
        return Ok(Some(held.held_reason.clone()));
    }
    if attributes_of(book).allocation_account {
        let instrument = book
            .ledger
            .executions
            .get(
                held.booking
                    .execution_ids
                    .first()
                    .map(String::as_str)
                    .unwrap_or_default(),
            )
            .map(|held| held.execution.instrument_id.clone())
            .unwrap_or_default();
        for ((id, _, _), position) in &book.positions {
            if id == &instrument && !position.removed && !position.trade_date()?.is_zero() {
                return Ok(Some(format!(
                    "allocation account {} still holds {} of {instrument} after booking",
                    book.account_id,
                    position.trade_date()?
                )));
            }
        }
    }
    Ok(None)
}

// ── W11.28: hand off, or close ─────────────────────────────────────────────

pub fn hand_off_booking(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &HandOffBookingRequest,
) -> Result<Made> {
    user_required(ctx, "handing a booking off")?;
    in_scope(ctx, &book.account_id)?;
    let held = book
        .ledger
        .bookings
        .get(&request.booking_id)
        .ok_or_else(|| {
            StoreError::Invalid(format!(
                "account {} holds no booking {}",
                book.account_id, request.booking_id
            ))
        })?;
    let from = held.state;
    if !starts_from(booking_row("b3"), Some(from)) {
        return Err(booking_refused(
            "b3",
            from,
            if request.close {
                "closing"
            } else {
                "a hand-off"
            },
        ));
    }
    let booking = book.ledger.booking_record(&request.booking_id)?;
    let mut acts = Vec::new();
    let (kind, to) = match hand_off_check(book, &request.booking_id)? {
        None => {
            if !held.held_alert.is_empty() {
                acts.push(Act::AlertCleared {
                    alert_id: held.held_alert.clone(),
                });
            }
            if request.close {
                acts.push(Act::BookingClosed {
                    booking_id: request.booking_id.clone(),
                });
                (CLOSED, BookingState::Closed)
            } else {
                acts.push(Act::BookingHandedOff {
                    booking_id: request.booking_id.clone(),
                });
                (HANDED_OFF, BookingState::HandedOff)
            }
        }
        Some(reason) => {
            // Row b4 from booked; tried again while held, the reason kept.
            acts.push(Act::BookingHeld {
                booking_id: request.booking_id.clone(),
                reason: reason.clone(),
            });
            if held.held_alert.is_empty() {
                acts.push(held_alert(ctx, book, &booking, &reason));
            }
            (HELD, BookingState::Held)
        }
    };
    let row = if to == BookingState::Held {
        booking_row("b4")
    } else {
        booking_row("b3")
    };
    if from != BookingState::Held && !reaches(row, from, to) {
        return Err(booking_refused(row.row, from, "this change"));
    }
    let plan = Plan::new(kind, ctx.actor(), acts);
    finish(book, head, ctx, plan)
}

// ── W11.29: correct a booking ──────────────────────────────────────────────

pub fn correct_booking(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &CorrectBookingRequest,
) -> Result<Made> {
    user_required(ctx, "correcting a booking")?;
    if request.reason.trim().is_empty() {
        return Err(StoreError::refused(
            RefusalReason::ReasonRequired,
            "a booking's correction always carries its reason",
        ));
    }
    in_scope(ctx, &book.account_id)?;
    let held = book
        .ledger
        .bookings
        .get(&request.booking_id)
        .ok_or_else(|| {
            StoreError::Invalid(format!(
                "account {} holds no booking {}",
                book.account_id, request.booking_id
            ))
        })?;
    let from = held.state;
    let (start, count) = (held.lines.1, held.lines.2);
    let entry_id = held.lines.0.clone();
    match BookingCorrectionKind::try_from(request.kind) {
        Ok(BookingCorrectionKind::Cancel) => {
            if !starts_from(booking_row("b6"), Some(from)) {
                return Err(booking_refused("b6", from, "a cancel"));
            }
            let mut acts = Vec::new();
            if !held.held_alert.is_empty() {
                acts.push(Act::AlertCleared {
                    alert_id: held.held_alert.clone(),
                });
            }
            acts.push(Act::BookingCancelled {
                booking_id: request.booking_id.clone(),
                reason: request.reason.clone(),
            });
            let mut plan = Plan::new(CANCELLED, ctx.actor(), acts).reason(&request.reason);
            plan.lines =
                crate::order_decide::reroute_reliefs(book, negated(book, &entry_id, start, count))?;
            finish(book, head, ctx, plan)
        }
        Ok(BookingCorrectionKind::Replace) => {
            if !starts_from(booking_row("b7"), Some(from)) {
                return Err(booking_refused("b7", from, "a replace"));
            }
            allocations_hold(book, ctx, &held.booking.order_id, &request.allocations)?;
            let executed = executed(book, &held.booking.execution_ids)?;
            if let Some(words) = ties_out(&executed, &request.allocations)? {
                return Err(StoreError::refused(RefusalReason::BookingUnbalanced, words));
            }
            if !request.effective_date.is_empty() && !crate::dates::is_date(&request.effective_date)
            {
                return Err(malformed(
                    &["effective_date"],
                    format!(
                        "effective_date is {:?}; a date is YYYY-MM-DD",
                        request.effective_date
                    ),
                ));
            }
            let allocations: Vec<Allocation> = request
                .allocations
                .iter()
                .map(|allocation| Allocation {
                    allocation_id: ctx.mint(ALLOCATION),
                    give_up_answers: Vec::new(),
                    intended_settlement_date: if allocation.intended_settlement_date.is_empty() {
                        executed.value_date.clone()
                    } else {
                        allocation.intended_settlement_date.clone()
                    },
                    ..allocation.clone()
                })
                .collect();
            let reversed =
                crate::order_decide::reroute_reliefs(book, negated(book, &entry_id, start, count))?;
            let lines = booking_lines(
                book,
                ctx,
                &executed,
                &held.booking.execution_ids,
                &allocations,
                &reversed,
            )?;
            let effective = if request.effective_date.is_empty() {
                executed.trade_date.clone()
            } else {
                request.effective_date.clone()
            };
            let replaced = Booking {
                allocations: allocations.clone(),
                ..held.booking.clone()
            };
            let mut acts = vec![Act::BookingReplaced {
                booking_id: request.booking_id.clone(),
                allocations,
                effective_date: effective.clone(),
                reason: request.reason.clone(),
                lines: (reversed.len(), lines.len()),
            }];
            acts.extend(error_alerts(ctx, book, &replaced));
            let mut plan = Plan::new(CORRECTED, ctx.actor(), acts).reason(&request.reason);
            let mut all = reversed;
            all.extend(lines);
            plan.lines = all;
            plan.effective_date = effective;
            finish(book, head, ctx, plan)
        }
        _ => Err(malformed(
            &["kind"],
            "a correction is a cancel or a replace",
        )),
    }
}

/// An account a booking's correction or cancel moves, as the service makes
/// its child entries: each allocation of the version before that went to
/// another account, to be withdrawn.
pub fn allocations_elsewhere(booking: &Booking) -> Vec<Allocation> {
    booking
        .allocations
        .iter()
        .filter(|allocation| allocation.account_id != booking.account_id)
        .cloned()
        .collect()
}

// ── The child entries (W11.30) ─────────────────────────────────────────────

/// What a child entry needs of its booking, read from the order's account
/// after the booking committed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChildMove {
    pub booking_id: String,
    pub from_account: String,
    pub allocation: Allocation,
    pub member_order_id: String,
    pub instrument_id: String,
    pub side: i32,
    pub trade_date: String,
    pub value_date: String,
}

/// Each allocation of a booking to another account, as its child entry
/// moves it, the block's member order in that account it fills named where
/// one is.
pub fn child_moves(book: &Book, booking_id: &str, ctx: &Context) -> Result<Vec<ChildMove>> {
    let Some(held) = book.ledger.bookings.get(booking_id) else {
        return Ok(Vec::new());
    };
    let executed = executed(book, &held.booking.execution_ids)?;
    let members: Vec<&String> = book
        .ledger
        .orders
        .get(&held.booking.order_id)
        .map(|order| order.order.block_member_order_ids.iter().collect())
        .unwrap_or_default();
    let mut out = Vec::new();
    for allocation in allocations_elsewhere(&held.booking) {
        let member_order_id = members
            .iter()
            .find(|member| {
                ctx.members
                    .get(member.as_str())
                    .is_some_and(|view| view.account_id == allocation.account_id)
            })
            .map(|member| member.to_string())
            .unwrap_or_default();
        out.push(ChildMove {
            booking_id: booking_id.to_string(),
            from_account: book.account_id.clone(),
            allocation,
            member_order_id,
            instrument_id: crate::became::current(&executed.instrument_id),
            side: executed.side,
            trade_date: executed.trade_date.clone(),
            value_date: executed.value_date.clone(),
        });
    }
    Ok(out)
}

/// The child entry receiving an allocation (W11.30), the book's own act in
/// the receiving account's partition, naming the booking. `None` where it is
/// received already.
pub fn receive_allocation(
    book: &Book,
    head: u64,
    ctx: &Context,
    moved: &ChildMove,
) -> Result<Option<Made>> {
    let key = (
        moved.booking_id.clone(),
        moved.allocation.allocation_id.clone(),
    );
    if book.ledger.received.contains_key(&key) {
        return Ok(None);
    }
    let lines = received_lines(
        book,
        ctx,
        &moved.allocation,
        &moved.instrument_id,
        moved.side,
        &moved.trade_date,
        &moved.value_date,
        false,
    )?;
    let mut plan = Plan::new(
        ALLOCATION_MOVED,
        ctx.actor(),
        vec![Act::AllocationReceived {
            booking_id: moved.booking_id.clone(),
            from_account: moved.from_account.clone(),
            allocation: moved.allocation.clone(),
            member_order_id: moved.member_order_id.clone(),
            lines: (0, lines.len()),
        }],
    )
    .reason(&format!(
        "allocation {} of booking {} in account {}",
        moved.allocation.allocation_id, moved.booking_id, moved.from_account
    ));
    plan.lines = lines;
    plan.effective_date = moved.trade_date.clone();
    finish(book, head, ctx, plan).map(Some)
}

/// The child entry withdrawing an allocation a cancel or a replace took
/// away: its received lines reversed. `None` where there is nothing to
/// withdraw.
pub fn withdraw_allocation(
    book: &Book,
    head: u64,
    ctx: &Context,
    booking_id: &str,
    allocation_id: &str,
) -> Result<Option<Made>> {
    let key = (booking_id.to_string(), allocation_id.to_string());
    let Some(received) = book.ledger.received.get(&key) else {
        return Ok(None);
    };
    if received.withdrawn {
        return Ok(None);
    }
    let (entry_id, start, count) = received.lines.clone();
    let mut plan = Plan::new(
        ALLOCATION_MOVED,
        ctx.actor(),
        vec![Act::AllocationWithdrawn {
            booking_id: booking_id.to_string(),
            allocation_id: allocation_id.to_string(),
        }],
    )
    .reason(&format!(
        "allocation {allocation_id} of booking {booking_id} withdrawn by its correction"
    ));
    plan.lines =
        crate::order_decide::reroute_reliefs(book, negated(book, &entry_id, start, count))?;
    finish(book, head, ctx, plan).map(Some)
}

// ── Facts after booking (rows b5, b9) ──────────────────────────────────────

/// The acts holding or touching each live booking of `order_id` a fact moved
/// after it was booked (rows b5, b9): a bust, a correction, a late fill, or a
/// fee or charge reported later. Never refused: a fact never is.
pub fn after_facts(book: &Book, order_id: &str, why: &str) -> Vec<Act> {
    let top = book.ledger.top(order_id);
    let mut acts = Vec::new();
    for held in book.ledger.bookings.values() {
        if held.booking.order_id != top {
            continue;
        }
        match held.state {
            BookingState::Booked => {
                acts.push(Act::BookingHeld {
                    booking_id: held.booking.booking_id.clone(),
                    reason: why.to_string(),
                });
            }
            BookingState::HandedOff | BookingState::Closed => {
                acts.push(Act::BookingMoved {
                    booking_id: held.booking.booking_id.clone(),
                    reason: why.to_string(),
                });
            }
            _ => {}
        }
    }
    acts
}

/// The held-booking alerts the acts above owe: one for each booking they
/// newly hold (W11.34).
pub fn held_alerts_for(ctx: &Context, book: &Book, acts: &[Act]) -> Vec<Act> {
    acts.iter()
        .filter_map(|act| match act {
            Act::BookingHeld { booking_id, reason } => {
                let held = book.ledger.bookings.get(booking_id)?;
                if !held.held_alert.is_empty() {
                    return None;
                }
                let booking = book.ledger.booking_record(booking_id).ok()?;
                Some(held_alert(ctx, book, &booking, reason))
            }
            _ => None,
        })
        .collect()
}

// ── W11.32: a give-up's answer ─────────────────────────────────────────────

pub fn report_give_up(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &ReportGiveUpRequest,
) -> Result<Made> {
    let answer = GiveUpAnswer::try_from(request.answer).unwrap_or(GiveUpAnswer::Unspecified);
    if answer == GiveUpAnswer::Unspecified {
        return Err(malformed(
            &["answer"],
            "a give-up's answer is accepted or rejected",
        ));
    }
    let Some(execution) = book.ledger.executions.get(&request.execution_id) else {
        return Err(malformed(
            &["execution_id"],
            format!(
                "account {} holds no execution {}",
                book.account_id, request.execution_id
            ),
        ));
    };
    if !crate::order_decide::fact_in_scope(ctx, &book.account_id) {
        return Err(StoreError::Denied(format!(
            "execution {}'s account is outside {}'s scope",
            request.execution_id, ctx.instance_id
        )));
    }
    let booking_id = if request.allocation_id.is_empty() {
        String::new()
    } else {
        book.ledger
            .bookings
            .values()
            .find(|held| {
                held.booking
                    .allocations
                    .iter()
                    .any(|allocation| allocation.allocation_id == request.allocation_id)
            })
            .map(|held| held.booking.booking_id.clone())
            .ok_or_else(|| {
                malformed(
                    &["allocation_id"],
                    format!(
                        "account {} holds no allocation {}",
                        book.account_id, request.allocation_id
                    ),
                )
            })?
    };
    let _ = execution;
    let record = GiveUpRecord {
        answer: answer as i32,
        prime_reference: request.prime_reference.clone(),
        reason: request.reason.clone(),
        entry: None,
    };
    let plan = Plan::new(
        "give-up-answered",
        fact_actor(ctx),
        vec![Act::GiveUpRecorded {
            execution_id: request.execution_id.clone(),
            booking_id,
            allocation_id: request.allocation_id.clone(),
            record,
        }],
    )
    .reason(&request.reason);
    finish(book, head, ctx, plan)
}

// ── W11.33: a sleeve cross ─────────────────────────────────────────────────

pub fn record_sleeve_cross(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &RecordSleeveCrossRequest,
) -> Result<Made> {
    user_required(ctx, "recording a sleeve cross")?;
    let cross = request.sleeve_cross.clone().unwrap_or_default();
    if cross.reason.trim().is_empty() {
        return Err(StoreError::refused(
            RefusalReason::ReasonRequired,
            "a sleeve cross records who decided it and why",
        ));
    }
    in_scope(ctx, &book.account_id)?;
    if cross.instrument_id.is_empty() {
        return Err(malformed(
            &["sleeve_cross.instrument_id"],
            "a sleeve cross names its instrument",
        ));
    }
    if cross.giving_sleeve == cross.receiving_sleeve {
        return Err(malformed(
            &["sleeve_cross.receiving_sleeve"],
            "a sleeve cross is between two sleeves",
        ));
    }
    let quantity = match decimal("sleeve_cross.quantity", cross.quantity.as_ref())? {
        Some(quantity) if !quantity.is_negative() && !quantity.is_zero() => quantity,
        _ => {
            return Err(malformed(
                &["sleeve_cross.quantity"],
                "a sleeve cross states its quantity",
            ))
        }
    };
    let price = cross.cross_price.clone().ok_or_else(|| {
        malformed(
            &["sleeve_cross.cross_price"],
            "a sleeve cross states its cross price",
        )
    })?;
    let unit = numbers::amount("sleeve_cross.cross_price", &price)?;
    // Its orders, two of one account and instrument and opposite sides, none
    // ended (W11.33).
    if !cross.order_ids.is_empty() {
        if cross.order_ids.len() != 2 {
            return Err(malformed(
                &["sleeve_cross.order_ids"],
                "a sleeve cross stands in for two opposite orders, or for none",
            ));
        }
        let mut sides = Vec::new();
        for order_id in &cross.order_ids {
            let order = book.ledger.orders.get(order_id).ok_or_else(|| {
                malformed(
                    &["sleeve_cross.order_ids"],
                    format!("account {} holds no order {order_id}", book.account_id),
                )
            })?;
            if !crate::became::same(&order.order.instrument_id, &cross.instrument_id) {
                return Err(malformed(
                    &["sleeve_cross.order_ids"],
                    format!("order {order_id} is not of {}", cross.instrument_id),
                ));
            }
            let state = state_of(book, order_id)?;
            if matches!(
                state,
                OrderState::Refused | OrderState::Cancelled | OrderState::Expired
            ) {
                return Err(StoreError::refused(
                    RefusalReason::OrderState,
                    format!("order {order_id} has ended, so no cross fills it"),
                ));
            }
            sides.push(buys(order.order.side));
        }
        if sides[0] == sides[1] {
            return Err(malformed(
                &["sleeve_cross.order_ids"],
                "a sleeve cross stands in for two orders of opposite sides",
            ));
        }
    }
    let sleeve_cross = SleeveCross {
        sleeve_cross_id: ctx.mint(SLEEVE_CROSS),
        account_id: book.account_id.clone(),
        ..cross.clone()
    };
    // The tax layer: each lot moves between the sleeves keeping its cost and
    // date, a partial cross splitting it; the performance layer, each moved
    // lot's cost at the cross price, is read from the cross itself (W11.33).
    // The cash at the cross price moves the same way, so the account's
    // quantity and cash are unchanged.
    let giving: Key = (
        cross.instrument_id.clone(),
        HoldingSide::Long as i32,
        cross.giving_sleeve.clone(),
    );
    let method = method_of(book);
    let mut lines = Vec::new();
    let mut rest = quantity;
    for (lot_id, take) in relief(open_lots(book, &giving, &[])?, quantity, method, &[])? {
        let terms = book
            .positions
            .get(&giving)
            .and_then(|position| position.lots.get(&lot_id))
            .and_then(|state| state.lot.terms.clone())
            .unwrap_or_default();
        let original = book
            .positions
            .get(&giving)
            .and_then(|position| position.lots.get(&lot_id))
            .and_then(|state| state.lot.original_quantity)
            .and_then(|wire| Exact::from_wire(&wire).ok())
            .unwrap_or(take);
        let mut out = signed_line(
            &cross.instrument_id,
            HoldingSide::Long,
            "",
            &cross.giving_sleeve,
            take.negated(),
        );
        out.lot_id = lot_id;
        lines.push(out);
        // The moved part keeps its basis: its share of the lot's cost.
        let cost = terms.cost.as_ref().and_then(|cost| {
            let amount = amount_of(Some(cost))?;
            if original.is_zero() || take == original {
                return Some(cost.clone());
            }
            let scale = amount.scale().max(original.scale()) + 6;
            let numerator = amount.integer().checked_mul(take.integer())?;
            let denominator = original.integer();
            let shifted = numerator.checked_mul(
                10i128.pow(scale - amount.scale() - take.scale() + original.scale()),
            )?;
            Exact::new(shifted / denominator, scale)
                .ok()
                .map(|value| Money {
                    amount: numbers::wire(value),
                    ..cost.clone()
                })
        });
        let mut into = signed_line(
            &cross.instrument_id,
            HoldingSide::Long,
            "",
            &cross.receiving_sleeve,
            take,
        );
        into.lot_id = ctx.mint(ids::LOT);
        into.opens_lot = Some(LotTerms { cost, ..terms });
        lines.push(into);
        rest = numbers::add("crossed", rest, take.negated())?;
    }
    if !rest.is_zero() {
        return Err(malformed(
            &["sleeve_cross.quantity"],
            format!(
                "sleeve {:?} holds {} less of {} than the cross moves",
                cross.giving_sleeve, rest, cross.instrument_id
            ),
        ));
    }
    let cash = cash_instrument(&price)?;
    let value = orders::multiply("cross cash", quantity, unit)?;
    lines.push(signed_line(
        &cash,
        HoldingSide::Long,
        "",
        &cross.giving_sleeve,
        value,
    ));
    lines.push(signed_line(
        &cash,
        HoldingSide::Long,
        "",
        &cross.receiving_sleeve,
        value.negated(),
    ));
    let mut plan = Plan::new(
        "sleeve-crossed",
        ctx.actor(),
        vec![Act::SleeveCrossed {
            cross: sleeve_cross,
        }],
    )
    .reason(&cross.reason);
    plan.lines = lines;
    finish(book, head, ctx, plan)
}

// ── W11.34: alerts ─────────────────────────────────────────────────────────

pub fn raise_alert(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &RaiseAlertRequest,
) -> Result<Made> {
    if request.reason.trim().is_empty() {
        return Err(StoreError::refused(
            RefusalReason::ReasonRequired,
            "an alert says why it is raised",
        ));
    }
    if AlertKind::try_from(request.kind) != Ok(AlertKind::Compliance) {
        return Err(malformed(
            &["kind"],
            "compliance raises its own alerts, ALERT_KIND_COMPLIANCE; the book raises the others",
        ));
    }
    let subject = request.subject.clone().unwrap_or_default();
    let plan = Plan::new(
        "alert-raised",
        fact_actor(ctx),
        vec![alert(
            ctx,
            &book.account_id,
            AlertKind::Compliance,
            request.reason.clone(),
            subject,
        )],
    )
    .reason(&request.reason);
    finish(book, head, ctx, plan)
}

pub fn acknowledge_alert(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &meridian_domain::v1::AcknowledgeAlertRequest,
) -> Result<Made> {
    person_required(ctx, "acknowledging an alert")?;
    if request.reason.trim().is_empty() {
        return Err(StoreError::refused(
            RefusalReason::ReasonRequired,
            "an alert is acknowledged with a reason",
        ));
    }
    in_scope(ctx, &book.account_id)?;
    if !book.ledger.alerts.contains_key(&request.alert_id) {
        return Err(StoreError::Invalid(format!(
            "account {} holds no alert {}",
            book.account_id, request.alert_id
        )));
    }
    let plan = Plan::new(
        "alert-acknowledged",
        ctx.actor(),
        vec![Act::AlertAcknowledged {
            alert_id: request.alert_id.clone(),
            reason: request.reason.clone(),
        }],
    )
    .reason(&request.reason);
    finish(book, head, ctx, plan)
}

// ── W11.36: the escape hatch's setting ─────────────────────────────────────

/// The setting in force (W11.36): on unless the control partition last said
/// off, as it is by default.
pub fn allows_local_record_orders(control: &Book) -> bool {
    control.ledger.local_record_orders.unwrap_or(true)
}

pub fn set_local_record_orders(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &SetLocalRecordOrdersRequest,
) -> Result<Made> {
    user_required(ctx, "setting whether orders may name local records")?;
    if request.reason.trim().is_empty() {
        return Err(StoreError::refused(
            RefusalReason::ReasonRequired,
            "the escape hatch is set with a reason",
        ));
    }
    let plan = Plan::new(
        "local-record-orders-set",
        ctx.actor(),
        vec![Act::LocalRecordOrdersSet {
            allows: request.allows_local_record_orders,
        }],
    )
    .reason(&request.reason);
    made_without_records(book, head, ctx, plan)
}

/// An entry of the control partition changing no record row but the
/// setting itself: numbered by the entry's own sequence (W11.36).
fn made_without_records(book: &Book, head: u64, ctx: &Context, plan: Plan) -> Result<Made> {
    let draft = crate::decide::Draft {
        entry_id: ctx.mint(ids::ENTRY),
        kind: plan.kind,
        effective_date: ctx.today(),
        reason: plan.reason,
        break_ids: Vec::new(),
        idempotency_key: plan.idempotency_key,
        actor: plan.actor,
        body: Body {
            acts: plan.acts,
            ..Default::default()
        },
    };
    crate::decide::made(book, head, ctx, draft)
}

/// The escape hatch held to a proposal or an accept (W11's invariant, rows
/// 23b and e3): an order naming a provisional record is refused while the
/// setting is off.
pub fn local_record_allowed(ctx: &Context, instrument_id: &str, what: &str) -> Result<()> {
    let provisional = ctx
        .records
        .get(instrument_id)
        .is_some_and(|says| says.provisional);
    if provisional && !ctx.local_record_orders.unwrap_or(true) {
        return Err(StoreError::refused(
            RefusalReason::LocalRecordNotAllowed,
            format!(
                "{what} names {instrument_id}, a local record the platform's master does not \
                 hold yet, and this deployment's admin has turned orders on local records off; \
                 an order recorded for a telephoned or outside order is still recorded"
            ),
        ));
    }
    Ok(())
}

/// An alert's state read by its kind and whether a person acknowledged it,
/// for a reader's words.
pub fn alert_state_named(state: i32) -> &'static str {
    match AlertState::try_from(state) {
        Ok(AlertState::Raised) => "raised",
        Ok(AlertState::Acknowledged) => "acknowledged",
        Ok(AlertState::Cleared) => "cleared",
        _ => "unknown",
    }
}

/// The ID of a block's members a proposal names, each held to the rule
/// (W11.1): released or working, of the block's instrument and side, in no
/// other live block. Answers the block's quantity: the sum of their idle.
pub fn block_holds(
    ctx: &Context,
    instrument_id: &str,
    side: i32,
    members: &[String],
) -> Result<Exact> {
    let mut total = Exact::ZERO;
    let mut seen = BTreeSet::new();
    for (index, member) in members.iter().enumerate() {
        let field = format!("block_member_order_ids[{index}]");
        if !seen.insert(member.as_str()) {
            return Err(malformed(&[&field], format!("names order {member} twice")));
        }
        let Some(view) = ctx.members.get(member) else {
            return Err(malformed(
                &[&field],
                format!("the book holds no order {member}"),
            ));
        };
        let state = OrderState::try_from(view.state).unwrap_or(OrderState::Unspecified);
        if !matches!(state, OrderState::Released | OrderState::Working) {
            return Err(state_refused(
                crate::order_decide::row("i2"),
                state,
                &format!("naming order {member}, which is not released, in a block"),
            ));
        }
        if !crate::became::same(&view.instrument_id, instrument_id) {
            return Err(malformed(
                &[&field],
                format!(
                    "order {member} is of {}, and a block's members are of its instrument, {instrument_id}",
                    view.instrument_id
                ),
            ));
        }
        if view.side != side {
            return Err(malformed(
                &[&field],
                format!("order {member} is on another side than the block"),
            ));
        }
        if !view.in_block.is_empty() {
            return Err(StoreError::refused(
                RefusalReason::OrderState,
                format!(
                    "order {member} is in live block {} already: a member's filled quantity is \
                     its allocation's, and two blocks would count it twice",
                    view.in_block
                ),
            ));
        }
        total = numbers::add("block quantity", total, view.idle)?;
    }
    Ok(total)
}
