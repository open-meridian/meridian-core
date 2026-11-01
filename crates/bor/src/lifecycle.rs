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
//!
//! Contract v29 (W9.16 to W9.18; rulings 680 to 683, 988): a debt record's
//! seven records join -- a coupon received, a redemption, a paydown, an
//! accretion, a TBA allocation, a strip and a reconstitution -- each its own
//! `LCE-` record and entry, reported as the custodian states it. Lots of a
//! pool, a TBA or a tranche stay in original face; a paydown relieves their
//! cost pro rata to the factor's fall and moves cash; an accretion moves
//! nothing; a coupon's cash is two lines, the purchased accrued returned and
//! the income (988).

use std::collections::BTreeSet;

use meridian_domain::exact::Exact;
use meridian_domain::v1::{
    basis_adjustment, BasisAdjustment, HoldingSide, InstrumentType, LifecycleEvent,
    LifecycleEventKind, LifecycleEventState, LotReliefMethod, LotSource, LotTerms, Money,
    MovementLine, RecordLifecycleEventRequest, RedemptionEventKind, ReleaseLifecycleEventRequest,
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

/// The kinds an option takes: what closes its lots against a delivery.
fn option_kind(kind: LifecycleEventKind) -> bool {
    matches!(
        kind,
        LifecycleEventKind::Exercise | LifecycleEventKind::Assignment | LifecycleEventKind::Expiry
    )
}

/// Contract v29 (W9.16): the seven kinds a debt record takes.
fn debt_kind(kind: LifecycleEventKind) -> bool {
    matches!(
        kind,
        LifecycleEventKind::CouponReceived
            | LifecycleEventKind::Redemption
            | LifecycleEventKind::Paydown
            | LifecycleEventKind::Accretion
            | LifecycleEventKind::TbaAllocation
            | LifecycleEventKind::Strip
            | LifecycleEventKind::Reconstitution
    )
}

fn type_of(ctx: &Context, instrument: &str) -> InstrumentType {
    ctx.records
        .get(instrument)
        .map(|said| said.instrument_type)
        .and_then(|kind| InstrumentType::try_from(kind).ok())
        .unwrap_or(InstrumentType::Unspecified)
}

fn date_or_refused(path: &str, text: &str, words: &str) -> Result<()> {
    if meridian_domain::date::Date::parse(text).is_err() {
        return Err(malformed(
            &[path],
            format!("{text:?} is no plain date: {words}"),
        ));
    }
    Ok(())
}

/// The checks every event takes, reported or proposed (W9.16): its account,
/// a listed option or future the instrument store holds, a kind its type
/// takes, a quantity and a date; a variation's settlement price and cash.
/// From contract v29 a debt record's seven kinds and what each states.
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
    let kind = type_of(ctx, &event.instrument_id);
    let option = matches!(
        kind,
        InstrumentType::ListedOption | InstrumentType::FutureOption
    );
    let debt = meridian_domain::debt::is_debt(kind);
    let event_kind = kind_of(event);
    if event_kind == LifecycleEventKind::Unspecified {
        return Err(malformed(
            &[&format!("{at}kind")],
            "the event's kind is unspecified",
        ));
    }
    // Contract v29 (W9.16; ruling 992): a factor revision is the book's own
    // record, never reported or proposed.
    if event_kind == LifecycleEventKind::FactorRevision {
        return Err(malformed(
            &[&format!("{at}kind")],
            "a factor revision is the book's own record, written when a provisional factor is              published; it is never reported or proposed",
        ));
    }
    // Contract v29 (W9.16): a fixed-income kind on a record that is no debt
    // record, or an option's or a future's kind on a debt record.
    if debt_kind(event_kind) != debt {
        return Err(malformed(
            &[&format!("{at}kind")],
            if debt {
                "a debt record takes a coupon, a redemption, a paydown, an accretion, a TBA \
                 allocation, a strip or a reconstitution, never an option's or a future's event"
            } else {
                "a coupon, a redemption, a paydown, an accretion, a TBA allocation, a strip or a \
                 reconstitution is a debt record's (bond, mbs_pool, tba, cmo_tranche, \
                 certificate_of_deposit)"
            },
        ));
    }
    if !debt && !option && kind != InstrumentType::Future {
        return Err(malformed(
            &[&format!("{at}instrument_id")],
            format!(
                "{:?} is no listed option, future or debt record the instrument store holds",
                event.instrument_id
            ),
        ));
    }
    match event_kind {
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
            if debt {
                "a debt record's event states its face, a positive quantity"
            } else {
                "the contracts are a positive quantity"
            },
        ));
    }
    if meridian_domain::date::Date::parse(&event.event_date).is_err() {
        return Err(malformed(
            &[&format!("{at}event_date")],
            format!("{:?} is no plain date, YYYY-MM-DD", event.event_date),
        ));
    }
    if event_kind == LifecycleEventKind::SettlementVariation {
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
    if debt {
        debt_checked(ctx, event, event_kind, kind, at)?;
    }
    Ok(())
}

/// Contract v29 (W9.16; rulings 680 to 683): what each of a debt record's
/// kinds states, refused naming the field.
fn debt_checked(
    ctx: &Context,
    event: &LifecycleEvent,
    event_kind: LifecycleEventKind,
    kind: InstrumentType,
    at: &str,
) -> Result<()> {
    if !event.record_date.is_empty() {
        date_or_refused(
            &format!("{at}record_date"),
            &event.record_date,
            "YYYY-MM-DD",
        )?;
    }
    let redemption = RedemptionEventKind::try_from(event.redemption_kind)
        .unwrap_or(RedemptionEventKind::Unspecified);
    if event_kind != LifecycleEventKind::Redemption
        && (redemption != RedemptionEventKind::Unspecified || event.redemption_price.is_some())
    {
        return Err(malformed(
            &[&format!("{at}redemption_kind")],
            "only a redemption states a redemption kind and price",
        ));
    }
    let factors = || -> Result<(Exact, Exact)> {
        let factor = numbers::optional(&format!("{at}factor"), event.factor.as_ref())?;
        let prior = numbers::optional(&format!("{at}prior_factor"), event.prior_factor.as_ref())?;
        match (prior, factor) {
            (Some(prior), Some(factor)) if !factor.is_negative() && !prior.is_negative() => {
                Ok((prior, factor))
            }
            (_, None) | (None, _) => Err(malformed(
                &[&format!("{at}factor")],
                "a paydown or an accretion states the factor it brought the record to and the \
                 one it moved from, as the agency's Factor row (W10.4)",
            )),
            _ => Err(malformed(
                &[&format!("{at}factor")],
                "a factor is never below zero",
            )),
        }
    };
    let pooled = matches!(kind, InstrumentType::MbsPool | InstrumentType::CmoTranche);
    match event_kind {
        LifecycleEventKind::CouponReceived => {
            if event.cash.is_none() {
                return Err(malformed(
                    &[&format!("{at}cash")],
                    "a coupon received states the interest paid, its cash",
                ));
            }
            if event.period_end_date.is_empty() || event.period_start_date.is_empty() {
                return Err(malformed(
                    &[&format!("{at}period_end_date")],
                    "a coupon received states its period, its start and its end",
                ));
            }
            date_or_refused(
                &format!("{at}period_start_date"),
                &event.period_start_date,
                "the accrual start",
            )?;
            date_or_refused(
                &format!("{at}period_end_date"),
                &event.period_end_date,
                "the accrual end",
            )?;
            if event.period_end_date <= event.period_start_date {
                return Err(malformed(
                    &[&format!("{at}period_end_date")],
                    "a coupon's period ends after it starts",
                ));
            }
        }
        LifecycleEventKind::Redemption => {
            if redemption == RedemptionEventKind::Unspecified {
                return Err(malformed(
                    &[&format!("{at}redemption_kind")],
                    "a redemption states what it redeemed: a maturity, a full or partial call, \
                     a put or a sinking-fund draw",
                ));
            }
            match numbers::optional(
                &format!("{at}redemption_price"),
                event.redemption_price.as_ref(),
            )? {
                Some(price) if !price.is_negative() => {}
                _ => {
                    return Err(malformed(
                        &[&format!("{at}redemption_price")],
                        "a redemption states its price per 100 of face, the lots closing at it",
                    ))
                }
            }
        }
        LifecycleEventKind::Paydown | LifecycleEventKind::Accretion => {
            if !pooled {
                return Err(malformed(
                    &[&format!("{at}kind")],
                    "a paydown or an accretion is a pool's or a tranche's (ruling 680)",
                ));
            }
            let (prior, factor) = factors()?;
            if event_kind == LifecycleEventKind::Paydown {
                if factor > prior {
                    return Err(malformed(
                        &[&format!("{at}factor")],
                        "a paydown's factor falls from the one it moved from",
                    ));
                }
            } else {
                if event.face_change.is_none() {
                    return Err(malformed(
                        &[&format!("{at}face_change")],
                        "an accretion states the face it changed, signed",
                    ));
                }
                if event.cash.is_some() {
                    return Err(malformed(
                        &[&format!("{at}cash")],
                        "an accretion moves no cash (ruling 682)",
                    ));
                }
            }
        }
        LifecycleEventKind::TbaAllocation => {
            if kind != InstrumentType::Tba {
                return Err(malformed(
                    &[&format!("{at}kind")],
                    "a TBA allocation is a tba record's (ruling 681)",
                ));
            }
            if event.deliveries.is_empty() {
                return Err(malformed(
                    &[&format!("{at}deliveries")],
                    "a TBA allocation names each pool delivered",
                ));
            }
            let tba = ctx
                .records
                .get(&event.instrument_id)
                .and_then(|said| said.debt.clone())
                .unwrap_or_default();
            for (i, delivery) in event.deliveries.iter().enumerate() {
                let pool = ctx
                    .records
                    .get(&delivery.instrument_id)
                    .filter(|said| said.instrument_type == InstrumentType::MbsPool as i32)
                    .and_then(|said| said.debt.clone());
                let matches = pool.as_ref().is_some_and(|pool| {
                    pool.program == tba.program
                        && pool.pass_through_rate == tba.pass_through_rate
                        && (pool.agency == 0 || tba.agency == 0 || pool.agency == tba.agency)
                });
                if !matches {
                    return Err(malformed(
                        &[&format!("{at}deliveries")],
                        format!(
                            "deliveries[{i}] names {:?}, no mbs_pool of the TBA's agency, program \
                             and coupon",
                            delivery.instrument_id
                        ),
                    ));
                }
                if numbers::required(
                    &format!("{at}deliveries[{i}].quantity"),
                    delivery.quantity.as_ref(),
                )?
                .is_negative()
                {
                    return Err(malformed(
                        &[&format!("{at}deliveries[{i}].quantity")],
                        "a pool delivered into the account is received, a positive face",
                    ));
                }
            }
        }
        LifecycleEventKind::Strip | LifecycleEventKind::Reconstitution => {
            if kind != InstrumentType::Bond {
                return Err(malformed(
                    &[&format!("{at}kind")],
                    "a strip or a reconstitution names the whole bond",
                ));
            }
            if event.deliveries.is_empty() {
                return Err(malformed(
                    &[&format!("{at}deliveries")],
                    "a strip or a reconstitution names each component exchanged",
                ));
            }
            let strip = event_kind == LifecycleEventKind::Strip;
            for (i, delivery) in event.deliveries.iter().enumerate() {
                let component = ctx
                    .records
                    .get(&delivery.instrument_id)
                    .and_then(|said| said.debt.as_ref())
                    .is_some_and(|said| said.stripped_from == event.instrument_id);
                if !component {
                    return Err(malformed(
                        &[&format!("{at}deliveries")],
                        format!(
                            "deliveries[{i}] names {:?}, no component stripped_from {}",
                            delivery.instrument_id, event.instrument_id
                        ),
                    ));
                }
                let moved = numbers::required(
                    &format!("{at}deliveries[{i}].quantity"),
                    delivery.quantity.as_ref(),
                )?;
                if moved.is_negative() == strip {
                    return Err(malformed(
                        &[&format!("{at}deliveries[{i}].quantity")],
                        if strip {
                            "a strip receives its components: each a positive face"
                        } else {
                            "a reconstitution delivers its components: each a negative face"
                        },
                    ));
                }
                if delivery.price.is_none() {
                    return Err(malformed(
                        &[&format!("{at}deliveries[{i}].price")],
                        "each component states its price, the basis allocated by relative value",
                    ));
                }
            }
        }
        _ => {}
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

/// What an event moves: its lines, and from contract v29 a paydown's
/// relief of its lots' cost, each with the lot's holding period start
/// before it (the journal's reversal puts it back).
#[derive(Default)]
struct Moved {
    lines: Vec<MovementLine>,
    basis_adjustments: Vec<BasisAdjustment>,
    prior_holding_period_starts: Vec<String>,
}

/// `cost × part ÷ whole` (contract v29: a paydown's pro rata relief, a TBA's
/// or a whole bond's cost carried by face or by relative value): exact where
/// the quotient ends within the cost's places (at least two), else taken
/// half-even at them; the remainder stays where the cost was, so no cost is
/// made or lost.
pub(crate) fn share(field: &str, cost: Exact, part: Exact, whole: Exact) -> Result<Exact> {
    if whole.is_zero() {
        return Err(StoreError::Invalid(format!("{field}: a share of nothing")));
    }
    let product = crate::orders::multiply(field, cost, part)?;
    let scale = cost.scale().max(2);
    let too_big =
        || StoreError::Invalid(format!("{field}: {cost} × {part} ÷ {whole} does not fit"));
    // product.int × 10^(scale + whole.scale) ÷ (whole.int × 10^product.scale)
    let up = |value: i128, places: u32| -> Result<i128> {
        10i128
            .checked_pow(places)
            .and_then(|power| value.checked_mul(power))
            .ok_or_else(too_big)
    };
    let numerator = up(product.integer(), scale + whole.scale())?;
    let denominator = up(whole.integer(), product.scale())?;
    let negative = (numerator < 0) != (denominator < 0);
    let (n, d) = (numerator.unsigned_abs(), denominator.unsigned_abs());
    let (mut q, r) = (n / d, n % d);
    // Half-even: above half up, exactly half to the even neighbour.
    if r * 2 > d || (r * 2 == d && q % 2 == 1) {
        q += 1;
    }
    let q = i128::try_from(q).map_err(|_| too_big())?;
    Exact::new(if negative { -q } else { q }, scale)
        .map_err(|out| StoreError::Invalid(format!("{field} {out}")))
}

/// A derived amount without the zeros its product carried past cents: the
/// fall of a factor times face, 11091.65000000, is 11091.65.
fn trimmed(value: Exact) -> Exact {
    let (mut integer, mut scale) = (value.integer(), value.scale());
    while scale > 2 && integer % 10 == 0 {
        integer /= 10;
        scale -= 1;
    }
    Exact::new(integer, scale).unwrap_or(value)
}

/// A lot's cost, its money, and the quantity it was opened with.
fn lot_cost(book: &Book, key: &Key, lot_id: &str) -> Option<(Money, Exact, Exact)> {
    let lot = &book.positions.get(key)?.lots.get(lot_id)?.lot;
    let cost = lot.terms.as_ref()?.cost.clone()?;
    let amount = Exact::from_wire(cost.amount.as_ref()?).ok()?;
    let original = Exact::from_wire(lot.original_quantity.as_ref()?).ok()?;
    let original = if original.is_negative() {
        original.negated()
    } else {
        original
    };
    Some((cost, amount, original))
}

/// Relieved lots: the lines, the cost the quantity relieved carried (None
/// where a lot's cost is unknown), and the earliest acquisition among them.
struct Relieved {
    lines: Vec<MovementLine>,
    cost: Option<(Exact, Money)>,
    earliest: String,
}

/// A long position's lots relieved by `quantity` under the account's relief
/// default, refused naming `path` above what the account holds.
fn relieve_long(
    book: &Book,
    instrument_id: &str,
    quantity: Exact,
    method: LotReliefMethod,
    earlier: &[MovementLine],
    path: &str,
) -> Result<Relieved> {
    let instrument = landing(book, instrument_id);
    let key: Key = (instrument.clone(), HoldingSide::Long as i32, String::new());
    let lots = open_lots(book, &key, earlier)?;
    let open = lots
        .iter()
        .try_fold(Exact::ZERO, |sum, lot| numbers::add("open", sum, lot.open))?;
    if quantity > open {
        return Err(malformed(
            &[path],
            format!(
                "{quantity} of face is more than the {open} the account holds long of \
                 {instrument_id}"
            ),
        ));
    }
    let acquired: Vec<(String, String)> = lots
        .iter()
        .map(|lot| (lot.lot_id.clone(), lot.acquired_date.clone()))
        .collect();
    let mut out = Relieved {
        lines: Vec::new(),
        cost: Some((Exact::ZERO, Money::default())),
        earliest: String::new(),
    };
    for (lot_id, take) in relief(lots, quantity, method, &[])? {
        out.lines.push(MovementLine {
            instrument_id: instrument.clone(),
            side: HoldingSide::Long as i32,
            bucket: SettlementBucket::Settled as i32,
            quantity: numbers::wire(take.negated()),
            lot_id: lot_id.clone(),
            ..Default::default()
        });
        if let Some((_, date)) = acquired.iter().find(|(id, _)| id == &lot_id) {
            if !date.is_empty() && (out.earliest.is_empty() || date < &out.earliest) {
                out.earliest = date.clone();
            }
        }
        out.cost = match (out.cost.take(), lot_cost(book, &key, &lot_id)) {
            (Some((sum, _)), Some((money, cost, original))) => {
                let part = share("cost", cost, take, original)?;
                Some((numbers::add("cost", sum, part)?, money))
            }
            _ => None,
        };
    }
    Ok(out)
}

/// A lot opened by an exchange (a TBA allocation, a strip, a
/// reconstitution): its face, the cost carried to it, its holding period
/// tacked to the earliest lot it came from.
fn opened(
    book: &Book,
    ctx: &Context,
    instrument_id: &str,
    quantity: Exact,
    cost: Option<(Exact, &Money)>,
    event: &LifecycleEvent,
    earliest: &str,
) -> MovementLine {
    MovementLine {
        instrument_id: landing(book, instrument_id),
        side: HoldingSide::Long as i32,
        bucket: SettlementBucket::Settled as i32,
        quantity: numbers::wire(quantity),
        lot_id: ctx.mint(ids::LOT),
        opens_lot: Some(LotTerms {
            unit_cost: None,
            cost: cost.map(|(amount, money)| Money {
                amount: numbers::wire(amount),
                ..money.clone()
            }),
            acquired_date: event.event_date.clone(),
            holding_period_start: earliest.to_string(),
            settlement_date: event.event_date.clone(),
            source: LotSource::Adjustment as i32,
        }),
        ..Default::default()
    }
}

/// A cost spread over parts by weight, the last taking the remainder so the
/// parts sum to the whole exactly.
fn spread(total: Exact, weights: &[Exact]) -> Result<Vec<Exact>> {
    let whole = weights
        .iter()
        .try_fold(Exact::ZERO, |sum, w| numbers::add("weight", sum, *w))?;
    let mut out = Vec::new();
    let mut given = Exact::ZERO;
    for (i, weight) in weights.iter().enumerate() {
        let part = if i + 1 == weights.len() {
            numbers::add("cost", total, given.negated())?
        } else {
            share("cost", total, *weight, whole)?
        };
        given = numbers::add("cost", given, part)?;
        out.push(part);
    }
    Ok(out)
}

/// Contract v29 (988): the accrued interest the account paid on purchases
/// of the instrument settling within a coupon's period -- returned by the
/// coupon, never income -- from its bookings and the allocations it
/// received; at most the coupon itself.
pub(crate) fn purchased_accrued(book: &Book, event: &LifecycleEvent) -> Result<Exact> {
    let ledger = &book.ledger;
    let side_of = |order_id: &str| {
        ledger
            .orders
            .get(order_id)
            .map(|held| (held.order.side, held.order.instrument_id.clone()))
    };
    let mut paid = Exact::ZERO;
    let mut count =
        |allocation: &meridian_domain::v1::Allocation, side: i32, instrument: &str| -> Result<()> {
            if side != meridian_domain::v1::Side::Buy as i32
                || !crate::became::same(instrument, &event.instrument_id)
                || allocation.intended_settlement_date.as_str() < event.period_start_date.as_str()
                || allocation.intended_settlement_date.as_str() >= event.period_end_date.as_str()
            {
                return Ok(());
            }
            if let Some(accrued) = &allocation.accrued_interest {
                paid = numbers::add("accrued", paid, numbers::amount("accrued", accrued)?)?;
            }
            Ok(())
        };
    for held in ledger.bookings.values() {
        if held.state == meridian_domain::v1::BookingState::Cancelled {
            continue;
        }
        let Some((side, instrument)) = side_of(&held.booking.order_id) else {
            continue;
        };
        for allocation in &held.booking.allocations {
            if allocation.account_id == book.account_id {
                count(allocation, side, &instrument)?;
            }
        }
    }
    for held in ledger.received.values() {
        if held.withdrawn {
            continue;
        }
        let Some((side, instrument)) = side_of(&held.member_order_id) else {
            continue;
        };
        count(&held.allocation, side, &instrument)?;
    }
    Ok(paid)
}

/// Cash moved by an event, received positive, as the base currency's
/// balance or a currency's lots.
fn cash_moved(
    book: &Book,
    ctx: &Context,
    cash: &Money,
    amount: Exact,
    event: &LifecycleEvent,
    method: LotReliefMethod,
    earlier: &[MovementLine],
) -> Result<Vec<MovementLine>> {
    if amount.is_zero() {
        return Ok(Vec::new());
    }
    currency_lines(
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
        earlier,
    )
}

/// Contract v29 (W9.16; rulings 680 to 683, 988): a debt record's event's
/// movement, as the custodian states it.
fn debt_moved(
    book: &Book,
    ctx: &Context,
    event: &LifecycleEvent,
    method: LotReliefMethod,
) -> Result<Moved> {
    let quantity = numbers::required("quantity", event.quantity.as_ref())?;
    let mut moved = Moved::default();
    // Cash derived where the custodian states none is in the record's
    // currency, its cash instrument as the lots' cost names it.
    let template = |_: Option<&Money>| -> Money {
        let code = ctx
            .records
            .get(&event.instrument_id)
            .map(|said| said.code.clone())
            .unwrap_or_default();
        let instrument = landing(book, &event.instrument_id);
        let key: Key = (instrument, HoldingSide::Long as i32, String::new());
        book.positions
            .get(&key)
            .into_iter()
            .flat_map(|position| position.lots.values())
            .filter_map(|state| state.lot.terms.as_ref()?.cost.clone())
            .find(|cost| cost.currency_code == code && !cost.instrument_id.is_empty())
            .map(|cost| Money {
                amount: None,
                ..cost
            })
            .unwrap_or(Money {
                currency_code: code,
                ..Default::default()
            })
    };
    match kind_of(event) {
        LifecycleEventKind::CouponReceived => {
            // 988: the purchased accrued returned, then the income, each its
            // own cash line.
            let cash = event.cash.clone().unwrap_or_default();
            let total = numbers::amount("cash", &cash)?;
            let mut returned = purchased_accrued(book, event)?;
            if returned > total {
                returned = total;
            }
            if total.is_negative() {
                returned = Exact::ZERO;
            }
            let income = numbers::add("cash", total, returned.negated())?;
            for part in [returned, income] {
                let earlier = moved.lines.clone();
                moved
                    .lines
                    .extend(cash_moved(book, ctx, &cash, part, event, method, &earlier)?);
            }
        }
        LifecycleEventKind::Redemption => {
            let relieved = relieve_long(
                book,
                &event.instrument_id,
                quantity,
                method,
                &[],
                "quantity",
            )?;
            moved.lines.extend(relieved.lines);
            // As stated, or face times the price per 100 of face.
            let cash = match &event.cash {
                Some(cash) => (cash.clone(), numbers::amount("cash", cash)?),
                None => {
                    let price =
                        numbers::required("redemption_price", event.redemption_price.as_ref())?;
                    (
                        template(None),
                        trimmed(crate::orders::multiply(
                            "cash",
                            crate::orders::multiply("cash", quantity, price)?,
                            Exact::new(1, 2).expect("0.01"),
                        )?),
                    )
                }
            };
            let earlier = moved.lines.clone();
            moved.lines.extend(cash_moved(
                book, ctx, &cash.0, cash.1, event, method, &earlier,
            )?);
        }
        LifecycleEventKind::Paydown => {
            // 680: the lots stay in original face; each lot's cost relieved
            // pro rata to the factor's fall; cash the fall times original
            // face where the custodian states none.
            let prior = numbers::required("prior_factor", event.prior_factor.as_ref())?;
            let factor = numbers::required("factor", event.factor.as_ref())?;
            let fall = numbers::add("factor", prior, factor.negated())?;
            let instrument = landing(book, &event.instrument_id);
            let key: Key = (instrument, HoldingSide::Long as i32, String::new());
            let lots = open_lots(book, &key, &[])?;
            let open = lots
                .iter()
                .try_fold(Exact::ZERO, |sum, lot| numbers::add("open", sum, lot.open))?;
            if quantity > open {
                return Err(malformed(
                    &["quantity"],
                    format!(
                        "{quantity} of original face is more than the {open} the account holds \
                         long of {}",
                        event.instrument_id
                    ),
                ));
            }
            if !prior.is_zero() && !fall.is_zero() {
                for lot in &lots {
                    let Some(state) = book
                        .positions
                        .get(&key)
                        .and_then(|position| position.lots.get(&lot.lot_id))
                    else {
                        continue;
                    };
                    let terms = state.lot.terms.clone().unwrap_or_default();
                    let Some(cost) = terms.cost.as_ref() else {
                        continue;
                    };
                    let amount = numbers::amount("cost", cost)?;
                    let relieved = share("cost", amount, fall, prior)?;
                    if relieved.is_zero() {
                        continue;
                    }
                    moved.basis_adjustments.push(BasisAdjustment {
                        lot_id: lot.lot_id.clone(),
                        holding_period_start: String::new(),
                        cost: Some(basis_adjustment::Cost::CostChange(Money {
                            amount: numbers::wire(relieved.negated()),
                            ..cost.clone()
                        })),
                    });
                    moved
                        .prior_holding_period_starts
                        .push(terms.holding_period_start.clone());
                }
            }
            let cash = match &event.cash {
                Some(cash) => (cash.clone(), numbers::amount("cash", cash)?),
                None => (
                    template(None),
                    trimmed(crate::orders::multiply("cash", fall, quantity)?),
                ),
            };
            moved
                .lines
                .extend(cash_moved(book, ctx, &cash.0, cash.1, event, method, &[])?);
        }
        // 682: no cash and no lot; the factor carries the face.
        LifecycleEventKind::Accretion => {}
        LifecycleEventKind::TbaAllocation | LifecycleEventKind::Strip => {
            // 681: the TBA's lots closed, each pool's opened in original face,
            // the cost carried by face; a strip's whole bond closed, each
            // component opened, the cost carried by relative value.
            let relieved = relieve_long(
                book,
                &event.instrument_id,
                quantity,
                method,
                &[],
                "quantity",
            )?;
            moved.lines.extend(relieved.lines);
            let mut weights = Vec::new();
            for (i, delivery) in event.deliveries.iter().enumerate() {
                let face = numbers::required(
                    &format!("deliveries[{i}].quantity"),
                    delivery.quantity.as_ref(),
                )?;
                weights.push(match &delivery.price {
                    Some(price) if kind_of(event) == LifecycleEventKind::Strip => {
                        crate::orders::multiply(
                            "value",
                            face,
                            numbers::amount(&format!("deliveries[{i}].price"), price)?,
                        )?
                    }
                    _ => face,
                });
            }
            let costs = match &relieved.cost {
                Some((total, money)) => spread(*total, &weights)?
                    .into_iter()
                    .map(|part| Some((part, money.clone())))
                    .collect(),
                None => vec![None; weights.len()],
            };
            for (delivery, cost) in event.deliveries.iter().zip(costs) {
                let face = numbers::required("deliveries.quantity", delivery.quantity.as_ref())?;
                moved.lines.push(opened(
                    book,
                    ctx,
                    &delivery.instrument_id,
                    face,
                    cost.as_ref().map(|(amount, money)| (*amount, money)),
                    event,
                    &relieved.earliest,
                ));
            }
        }
        LifecycleEventKind::Reconstitution => {
            // Each component's lots closed, the whole bond's opened with
            // their cost.
            let mut total: Option<(Exact, Money)> = Some((Exact::ZERO, Money::default()));
            let mut earliest = String::new();
            for (i, delivery) in event.deliveries.iter().enumerate() {
                let face = numbers::required(
                    &format!("deliveries[{i}].quantity"),
                    delivery.quantity.as_ref(),
                )?;
                let earlier = moved.lines.clone();
                let relieved = relieve_long(
                    book,
                    &delivery.instrument_id,
                    face.negated(),
                    method,
                    &earlier,
                    &format!("deliveries[{i}].quantity"),
                )?;
                moved.lines.extend(relieved.lines);
                if !relieved.earliest.is_empty()
                    && (earliest.is_empty() || relieved.earliest < earliest)
                {
                    earliest = relieved.earliest.clone();
                }
                total = match (total, relieved.cost) {
                    (Some((sum, _)), Some((part, money))) => {
                        Some((numbers::add("cost", sum, part)?, money))
                    }
                    _ => None,
                };
            }
            moved.lines.push(opened(
                book,
                ctx,
                &event.instrument_id,
                quantity,
                total.as_ref().map(|(amount, money)| (*amount, money)),
                event,
                &earliest,
            ));
        }
        _ => {}
    }
    Ok(moved)
}

/// An event's movement lines, as W9.16 books it: the option's lots closed
/// and the premium they cost, then the deliveries, then the cash; a
/// variation's cash line alone.
fn option_lines(book: &Book, ctx: &Context, event: &LifecycleEvent) -> Result<Vec<MovementLine>> {
    let kind = kind_of(event);
    let quantity = numbers::required("quantity", event.quantity.as_ref())?;
    let method = LotReliefMethod::try_from(attributes_of(book).lot_relief_default)
        .unwrap_or(LotReliefMethod::Unspecified);
    let mut lines: Vec<MovementLine> = Vec::new();
    let mut premium: Option<(Exact, String)> = None;
    if option_kind(kind) {
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

/// What an event moves (W9.16): an option's or a future's lines, or from
/// contract v29 a debt record's.
fn moved(book: &Book, ctx: &Context, event: &LifecycleEvent) -> Result<Moved> {
    if debt_kind(kind_of(event)) {
        let method = LotReliefMethod::try_from(attributes_of(book).lot_relief_default)
            .unwrap_or(LotReliefMethod::Unspecified);
        return debt_moved(book, ctx, event, method);
    }
    Ok(Moved {
        lines: option_lines(book, ctx, event)?,
        ..Default::default()
    })
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
    let moved = moved(book, ctx, &event)?;
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
                lines: moved.lines,
                basis_adjustments: moved.basis_adjustments,
                prior_holding_period_starts: moved.prior_holding_period_starts,
                lifecycle_events: records,
                trade: true,
                ..Default::default()
            },
        },
    )
}

/// Contract v29 (W9.17; 683): `servicing` proposes a coupon, a maturity, a
/// full call, a sinking-fund draw, a paydown and an accretion; a partial
/// call, a put, a TBA allocation, a strip and a reconstitution only a report
/// states, never a proposal.
fn proposable(event: &LifecycleEvent, at: &str) -> Result<()> {
    let redemption = RedemptionEventKind::try_from(event.redemption_kind)
        .unwrap_or(RedemptionEventKind::Unspecified);
    match kind_of(event) {
        LifecycleEventKind::Redemption
            if matches!(
                redemption,
                RedemptionEventKind::PartialCall | RedemptionEventKind::Put
            ) =>
        {
            Err(malformed(
                &[&format!("{at}redemption_kind")],
                "a partial call or a put is only reported, never proposed: a partial call by                  lottery cannot be derived, and a put is the holder's",
            ))
        }
        LifecycleEventKind::TbaAllocation
        | LifecycleEventKind::Strip
        | LifecycleEventKind::Reconstitution => Err(malformed(
            &[&format!("{at}kind")],
            "a TBA allocation, a strip or a reconstitution is only reported, never proposed",
        )),
        _ => Ok(()),
    }
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
        proposable(event, &at)?;
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
    // Contract v29 (W9.18; ruling 992): a factor revision's open claim,
    // marked settled.
    if let Some(claim) = book
        .lifecycle_events
        .get(&request.event_id)
        .filter(|held| held.state == LifecycleEventState::ClaimOpen as i32)
    {
        return crate::factors::claim_settled(book, head, ctx, claim, request.note.trim());
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
                    "{:?} is no proposal waiting in {}, nor a factor revision's open claim: \
                     answered, released, settled or never proposed",
                    request.event_id, book.account_id
                ),
            )
        })?;
    checked(book, ctx, &proposal, "")?;
    let moved = moved(book, ctx, &proposal)?;
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
                lines: moved.lines,
                basis_adjustments: moved.basis_adjustments,
                prior_holding_period_starts: moved.prior_holding_period_starts,
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
