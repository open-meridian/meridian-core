//! A listed option's or a future's lifecycle event in the book (contract
//! v28; W9.16 to W9.19; rulings 562 and 563).
//!
//! The broker's report is the record: an exercise, an assignment, an expiry
//! or a day's settlement variation, reported by `custody` or `connectivity`,
//! is journaled as its own entry -- the option's lots closed by the account's
//! relief default, the underlying's opened or relieved as the broker states
//! the deliveries, the premium the closed lots cost carried into the first
//! lot the delivery opens, and the cash it moved -- and kept whole as an
//! `LCE-` record. `servicing` proposes the events it expects, moving
//! nothing; a report matching a proposal by account, instrument, kind and
//! date answers it, and the proposal is never booked as well. `operations`
//! releases a proposal no report came for, with a note, booking it as the
//! report would have been.

use std::collections::BTreeSet;

use meridian_domain::exact::Exact;
use meridian_domain::v1::{
    HoldingSide, InstrumentType, LifecycleEvent, LifecycleEventKind, LifecycleEventState,
    LotSource, LotTerms, MovementLine, RecordLifecycleEventRequest, ReleaseLifecycleEventRequest,
    SettlementBucket,
};

use meridian_pb::v1::RefusalReason;

use crate::book::{Book, Key};
use crate::decide::{attributes_of, made, Context, Draft, Made};
use crate::journal::Body;
use crate::order_decide::{cash_instrument, currency_lines, landing, malformed, open_lots, relief};
use crate::store::{Result, StoreError};
use crate::{ids, numbers};

/// What a lifecycle event's record is named by: `LCE-` and a ULID.
pub const PREFIX: &str = "LCE";

fn kind_of(event: &LifecycleEvent) -> LifecycleEventKind {
    LifecycleEventKind::try_from(event.kind).unwrap_or(LifecycleEventKind::Unspecified)
}

/// The checks every event takes, reported or proposed (W9.16): its account,
/// a listed option or future the instrument store holds, a kind its type
/// takes, a quantity and a date; a variation's settlement price and cash.
fn checked(book: &Book, ctx: &Context, event: &LifecycleEvent, at: &str) -> Result<()> {
    if event.account_id != book.account_id {
        return Err(malformed(
            &[&format!("{at}account_id")],
            format!(
                "the event names {}, not {}",
                event.account_id, book.account_id
            ),
        ));
    }
    crate::order_decide::in_scope(ctx, &event.account_id)?;
    let kind = ctx
        .records
        .get(&event.instrument_id)
        .map(|said| said.instrument_type)
        .and_then(|kind| InstrumentType::try_from(kind).ok())
        .unwrap_or(InstrumentType::Unspecified);
    let option = matches!(
        kind,
        InstrumentType::ListedOption | InstrumentType::FutureOption
    );
    if !option && kind != InstrumentType::Future {
        return Err(malformed(
            &[&format!("{at}instrument_id")],
            format!(
                "{:?} is no listed option or future the instrument store holds",
                event.instrument_id
            ),
        ));
    }
    match kind_of(event) {
        LifecycleEventKind::Unspecified => {
            return Err(malformed(
                &[&format!("{at}kind")],
                "the event's kind is unspecified",
            ))
        }
        LifecycleEventKind::SettlementVariation if option => {
            return Err(malformed(
                &[&format!("{at}kind")],
                "a day's settlement variation is a future's, never an option's",
            ))
        }
        LifecycleEventKind::Exercise
        | LifecycleEventKind::Assignment
        | LifecycleEventKind::Expiry
            if !option =>
        {
            return Err(malformed(
                &[&format!("{at}kind")],
                "an exercise, an assignment or an expiry is an option's; a future is held, \
                 closed or rolled (ruling 562)",
            ))
        }
        _ => {}
    }
    let quantity = numbers::required(&format!("{at}quantity"), event.quantity.as_ref())?;
    if quantity.is_zero() || quantity.is_negative() {
        return Err(malformed(
            &[&format!("{at}quantity")],
            "the contracts are a positive quantity",
        ));
    }
    if meridian_domain::date::Date::parse(&event.event_date).is_err() {
        return Err(malformed(
            &[&format!("{at}event_date")],
            format!("{:?} is no plain date, YYYY-MM-DD", event.event_date),
        ));
    }
    if kind_of(event) == LifecycleEventKind::SettlementVariation {
        if event.settlement_price.is_none() {
            return Err(malformed(
                &[&format!("{at}settlement_price")],
                "a day's variation names the settlement price it was computed from",
            ));
        }
        if event.cash.is_none() {
            return Err(malformed(
                &[&format!("{at}cash")],
                "a day's variation is a cash line: it states the cash moved",
            ));
        }
    }
    for (i, delivery) in event.deliveries.iter().enumerate() {
        let path = format!("{at}deliveries[{i}]");
        if delivery.instrument_id.trim().is_empty() {
            return Err(malformed(
                &[&format!("{path}.instrument_id")],
                "a delivery names its instrument",
            ));
        }
        let moved = numbers::required(&format!("{path}.quantity"), delivery.quantity.as_ref())?;
        if moved.is_zero() {
            return Err(malformed(
                &[&format!("{path}.quantity")],
                "a delivery moves a quantity, never zero",
            ));
        }
    }
    Ok(())
}

/// The side an event closes: an exercise the long's, an assignment the
/// short's, an expiry whichever is held.
fn side_closed(book: &Book, event: &LifecycleEvent) -> HoldingSide {
    match kind_of(event) {
        LifecycleEventKind::Assignment => HoldingSide::Short,
        LifecycleEventKind::Expiry => {
            let instrument = landing(book, &event.instrument_id);
            let long: Key = (instrument, HoldingSide::Long as i32, String::new());
            if open_lots(book, &long, &[]).is_ok_and(|lots| !lots.is_empty()) {
                HoldingSide::Long
            } else {
                HoldingSide::Short
            }
        }
        _ => HoldingSide::Long,
    }
}

/// An event's movement lines, as W9.16 books it: the option's lots closed
/// and the premium they cost, then the deliveries, then the cash; a
/// variation's cash line alone.
fn lines(book: &Book, ctx: &Context, event: &LifecycleEvent) -> Result<Vec<MovementLine>> {
    let kind = kind_of(event);
    let quantity = numbers::required("quantity", event.quantity.as_ref())?;
    let method =
        meridian_domain::v1::LotReliefMethod::try_from(attributes_of(book).lot_relief_default)
            .unwrap_or(meridian_domain::v1::LotReliefMethod::Unspecified);
    let mut lines: Vec<MovementLine> = Vec::new();
    let mut premium: Option<(Exact, String)> = None;
    if kind != LifecycleEventKind::SettlementVariation {
        let side = side_closed(book, event);
        let instrument = landing(book, &event.instrument_id);
        let key: Key = (instrument.clone(), side as i32, String::new());
        let lots = open_lots(book, &key, &[])?;
        let open = lots
            .iter()
            .try_fold(Exact::ZERO, |sum, lot| numbers::add("open", sum, lot.open))?;
        if quantity > open {
            return Err(malformed(
                &["quantity"],
                format!(
                    "{quantity} contracts is more than the {open} the account holds {} of {}",
                    if side == HoldingSide::Long {
                        "long"
                    } else {
                        "short"
                    },
                    event.instrument_id
                ),
            ));
        }
        let costs: Vec<(String, Option<Exact>)> = lots
            .iter()
            .map(|lot| (lot.lot_id.clone(), lot.unit_cost))
            .collect();
        for (lot_id, take) in relief(lots, quantity, method, &[])? {
            lines.push(MovementLine {
                instrument_id: instrument.clone(),
                side: side as i32,
                bucket: SettlementBucket::Settled as i32,
                // A long's lot relieved negative, a short's covered positive.
                quantity: numbers::wire(if side == HoldingSide::Long {
                    take.negated()
                } else {
                    take
                }),
                lot_id: lot_id.clone(),
                ..Default::default()
            });
            if let Some(Some(unit)) = costs.iter().find(|(id, _)| id == &lot_id).map(|(_, c)| *c) {
                // Paid for a long, received for a short; the lot's unit cost
                // is per unit of the underlying, a contract its multiplier
                // of them (559).
                let part = crate::orders::multiply(
                    "premium",
                    crate::orders::multiply("premium", take, unit)?,
                    ctx.multiplier(&event.instrument_id),
                )?;
                let signed = if side == HoldingSide::Long {
                    part
                } else {
                    part.negated()
                };
                let held = premium.take().map_or(Exact::ZERO, |(sum, _)| sum);
                premium = Some((numbers::add("premium", held, signed)?, String::new()));
            }
        }
    }
    // The deliveries as the broker states them: received opens a lot after
    // covering a short, delivered relieves lots and opens a short for the
    // rest; the premium in the first lot the delivery opens.
    for (i, delivery) in event.deliveries.iter().enumerate() {
        let moved = numbers::required(
            &format!("deliveries[{i}].quantity"),
            delivery.quantity.as_ref(),
        )?;
        let instrument = landing(book, &delivery.instrument_id);
        let receiving = !moved.is_negative();
        let magnitude = if receiving { moved } else { moved.negated() };
        let (close_side, open_side) = if receiving {
            (HoldingSide::Short, HoldingSide::Long)
        } else {
            (HoldingSide::Long, HoldingSide::Short)
        };
        let key: Key = (instrument.clone(), close_side as i32, String::new());
        let earlier = lines.clone();
        let mut rest = magnitude;
        for (lot_id, take) in relief(open_lots(book, &key, &earlier)?, magnitude, method, &[])? {
            lines.push(MovementLine {
                instrument_id: instrument.clone(),
                side: close_side as i32,
                bucket: SettlementBucket::Settled as i32,
                quantity: numbers::wire(if receiving { take } else { take.negated() }),
                lot_id,
                ..Default::default()
            });
            rest = numbers::add("delivery", rest, take.negated())?;
        }
        if rest.is_zero() {
            continue;
        }
        let price = delivery.price.clone();
        let mut cost = match &price {
            Some(price) => Some(crate::orders::multiply(
                &format!("deliveries[{i}].price"),
                rest,
                numbers::amount(&format!("deliveries[{i}].price"), price)?,
            )?),
            None => None,
        };
        if let (Some(total), Some((paid, _))) = (cost, premium.take()) {
            cost = Some(numbers::add("cost", total, paid)?);
        }
        lines.push(MovementLine {
            instrument_id: instrument.clone(),
            side: open_side as i32,
            bucket: SettlementBucket::Settled as i32,
            quantity: numbers::wire(if receiving { rest } else { rest.negated() }),
            lot_id: ctx.mint(ids::LOT),
            opens_lot: Some(LotTerms {
                unit_cost: price.clone(),
                cost: match (cost, &price) {
                    (Some(cost), Some(price)) => Some(meridian_domain::v1::Money {
                        amount: numbers::wire(cost),
                        ..price.clone()
                    }),
                    _ => None,
                },
                acquired_date: event.event_date.clone(),
                holding_period_start: String::new(),
                settlement_date: event.event_date.clone(),
                source: LotSource::Adjustment as i32,
            }),
            ..Default::default()
        });
    }
    // The cash it moved: a cash settlement, a deliverable's cash, a day's
    // variation; signed, received positive.
    if let Some(cash) = &event.cash {
        let amount = numbers::amount("cash", cash)?;
        if !amount.is_zero() {
            let earlier = lines.clone();
            lines.extend(currency_lines(
                book,
                ctx,
                &cash_instrument(cash)?,
                amount,
                "",
                "",
                &event.event_date,
                None,
                method,
                &[],
                &earlier,
            )?);
        }
    }
    Ok(lines)
}

/// The proposal a report answers (W9.17): the same account, instrument,
/// kind and date, proposed and not yet answered or released.
fn proposal_answered<'a>(book: &'a Book, event: &LifecycleEvent) -> Option<&'a LifecycleEvent> {
    book.lifecycle_events.values().find(|held| {
        held.state == LifecycleEventState::Proposed as i32
            && held.account_id == event.account_id
            && held.instrument_id == event.instrument_id
            && held.kind == event.kind
            && held.event_date == event.event_date
    })
}

/// W9.16: a reported event booked, its own record and entry; a proposal it
/// answers marked answered. A report sent twice -- the same account and
/// external event -- is answered with the first.
pub fn record(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &RecordLifecycleEventRequest,
) -> Result<Made> {
    crate::decide::opening_required_for(book)?;
    let event = request
        .event
        .clone()
        .ok_or_else(|| malformed(&["event"], "the request carries no event"))?;
    checked(book, ctx, &event, "event.")?;
    if !event.external_event_id.is_empty() {
        if let Some(held) = book.lifecycle_events.values().find(|held| {
            held.external_event_id == event.external_event_id
                && held.state != LifecycleEventState::Proposed as i32
        }) {
            return Err(StoreError::Repeated(prost::Message::encode_to_vec(
                &meridian_domain::v1::RecordLifecycleEventReply {
                    event: Some(held.clone()),
                },
            )));
        }
    }
    let lines = lines(book, ctx, &event)?;
    let mut records = Vec::new();
    let mut booked = LifecycleEvent {
        event_id: ctx.mint(PREFIX),
        state: LifecycleEventState::Booked as i32,
        last_change: None,
        proposal_basis: None,
        ..event.clone()
    };
    if let Some(proposal) = proposal_answered(book, &event) {
        booked.answers_event_id = proposal.event_id.clone();
        records.push(LifecycleEvent {
            state: LifecycleEventState::Answered as i32,
            last_change: None,
            ..proposal.clone()
        });
    }
    records.insert(0, booked);
    made(
        book,
        head,
        ctx,
        Draft {
            entry_id: ctx.mint(ids::ENTRY),
            kind: "lifecycle-event-booked",
            effective_date: event.event_date.clone(),
            reason: String::new(),
            break_ids: Vec::new(),
            idempotency_key: request.idempotency_key.clone(),
            actor: ctx.actor(),
            body: Body {
                lines,
                lifecycle_events: records,
                trade: true,
                ..Default::default()
            },
        },
    )
}

/// W9.17: proposals for one account, each its own record, moving nothing.
pub fn propose(
    book: &Book,
    head: u64,
    ctx: &Context,
    events: &[(usize, LifecycleEvent)],
) -> Result<Made> {
    crate::decide::opening_required_for(book)?;
    let mut records = Vec::new();
    for (i, event) in events {
        let at = format!("events[{i}].");
        checked(book, ctx, event, &at)?;
        if !event.external_event_id.is_empty() || event.raw_record.is_some() {
            return Err(malformed(
                &[&format!("{at}external_event_id")],
                "a proposal names nothing the broker's report would: no external event and no \
                 raw record",
            ));
        }
        records.push(LifecycleEvent {
            event_id: ctx.mint(PREFIX),
            state: LifecycleEventState::Proposed as i32,
            answers_event_id: String::new(),
            last_change: None,
            ..event.clone()
        });
    }
    made(
        book,
        head,
        ctx,
        Draft {
            entry_id: ctx.mint(ids::ENTRY),
            kind: "lifecycle-events-proposed",
            effective_date: ctx.today(),
            reason: String::new(),
            break_ids: Vec::new(),
            idempotency_key: String::new(),
            actor: ctx.actor(),
            body: Body {
                lifecycle_events: records,
                ..Default::default()
            },
        },
    )
}

/// W9.18: a proposal no report answered, booked as proposed, with its note.
pub fn release(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &ReleaseLifecycleEventRequest,
) -> Result<Made> {
    if request.note.trim().is_empty() {
        return Err(StoreError::refused(
            RefusalReason::ReasonRequired,
            "note: a release of a proposed event says why",
        ));
    }
    if request.note.chars().count() > 2_000 {
        return Err(malformed(&["note"], "at most 2,000 characters"));
    }
    let proposal = book
        .lifecycle_events
        .get(&request.event_id)
        .filter(|held| held.state == LifecycleEventState::Proposed as i32)
        .cloned()
        .ok_or_else(|| {
            malformed(
                &["event_id"],
                format!(
                    "{:?} is no proposal waiting in {}: answered, released or never proposed",
                    request.event_id, book.account_id
                ),
            )
        })?;
    checked(book, ctx, &proposal, "")?;
    let lines = lines(book, ctx, &proposal)?;
    let released = LifecycleEvent {
        state: LifecycleEventState::Booked as i32,
        answers_event_id: proposal.event_id.clone(),
        event_id: ctx.mint(PREFIX),
        last_change: None,
        ..proposal.clone()
    };
    let answered = LifecycleEvent {
        state: LifecycleEventState::Answered as i32,
        last_change: None,
        ..proposal.clone()
    };
    made(
        book,
        head,
        ctx,
        Draft {
            entry_id: ctx.mint(ids::ENTRY),
            kind: "lifecycle-event-released",
            effective_date: proposal.event_date.clone(),
            reason: request.note.trim().to_string(),
            break_ids: Vec::new(),
            idempotency_key: String::new(),
            actor: ctx.actor(),
            body: Body {
                lines,
                lifecycle_events: vec![released, answered],
                trade: true,
                ..Default::default()
            },
        },
    )
}

/// W9.19: an account's events by instrument, kinds and dates, after a
/// watermark, in the account's sequence.
pub fn listed(
    book: &Book,
    instrument_id: &str,
    kinds: &[i32],
    from: &str,
    to: &str,
    since: u64,
) -> Vec<LifecycleEvent> {
    let kinds: BTreeSet<i32> = kinds.iter().copied().collect();
    let mut events: Vec<LifecycleEvent> = book
        .lifecycle_events
        .values()
        .filter(|event| instrument_id.is_empty() || event.instrument_id == instrument_id)
        .filter(|event| kinds.is_empty() || kinds.contains(&event.kind))
        .filter(|event| from.is_empty() || event.event_date.as_str() >= from)
        .filter(|event| to.is_empty() || event.event_date.as_str() <= to)
        .filter(|event| {
            event
                .last_change
                .as_ref()
                .is_some_and(|change| change.sequence > since)
        })
        .cloned()
        .collect();
    events.sort_by_key(|event| {
        event
            .last_change
            .as_ref()
            .map_or(0, |change| change.sequence)
    });
    events
}
