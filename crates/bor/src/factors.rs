//! A pool's or a tranche's factor, revised (contract v29: W11.11, W9.16,
//! W9.18; rulings 991 to 993, amended at core stage E).
//!
//! A fill stating no `factor_used` is priced at the factor the agency
//! published for its settlement month, which the book reads from the lake;
//! while that month's row is unpublished, at the latest published before it,
//! provisional (`Execution.factor_month_awaited`, `Lot.factor_month_awaited`).
//! The book reads the lake for each such fill on its sweep -- at start and
//! every quarter hour -- and, for a final factor it took, until the row's
//! payment date passes; when the awaited row is published, or the row taken
//! is republished at another factor, it writes its own record, a
//! `LIFECYCLE_EVENT_KIND_FACTOR_REVISION`: the old and new factor, the cash
//! difference and the change to the cost of the lots the fill opened, each
//! the difference of two amounts rounded to the cent half-even (993).
//!
//! The cash difference is pending to the fill's settlement date where that
//! is later; on or after it, the difference is an open receivable or
//! payable, pending and dated nothing, the event
//! `LIFECYCLE_EVENT_STATE_CLAIM_OPEN`, until a person marks it settled
//! (`ReleaseLifecycleEvent`, W9.18). SIFMA's Uniform Practices (ch. 2's
//! blackout period; ch. 9 J.2) settle such a trade on the previous factor
//! and cancel and correct it once the current one is published, the money
//! difference disbursed or collected after the correction: the revision is
//! that correction, the open claim that difference until it moves. A
//! booking of the fill is held, or marked moved after its hand-off, as any
//! fact after booking (W11.27), and its correction restates it (W11.29).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use meridian_bus::Bus;
use meridian_domain::exact::Exact;
use meridian_domain::v1::{
    basis_adjustment, BasisAdjustment, Factor, HoldingSide, LifecycleEvent, LifecycleEventKind,
    LifecycleEventState, LotReliefMethod, MovementLine, SettlementBucket, Side,
};

use crate::book::Book;
use crate::decide::{attributes_of, made, Context, Draft, Made};
use crate::journal::Body;
use crate::numbers;
use crate::order_decide::{cash_instrument, currency_lines};
use crate::orders::{self, Act, FactorRow};
use crate::store::{Result, Store, StoreError};

/// How often the book reads the lake for the factors its fills await.
pub const SWEEP_EVERY: Duration = Duration::from_secs(15 * 60);

/// The entry kinds this writes.
pub const REVISED: &str = "factor-revised";
pub const CLAIM_SETTLED: &str = "factor-claim-settled";

const NOTHING_DUE: &str = "no factor revision is due";

/// A fill's revision due: the factor now published for it and its row.
#[derive(Debug, Clone, PartialEq)]
pub struct Due {
    pub execution_id: String,
    pub factor: Exact,
    pub row: FactorRow,
}

/// Whether a fill's taken factor is still watched: provisional, or final
/// and its row's payment date not yet passed.
fn watched(row: &FactorRow, today: &str) -> bool {
    !row.awaited.is_empty()
        || (!row.month.is_empty()
            && !row.payment_date.is_empty()
            && today <= row.payment_date.as_str())
}

/// The instruments whose fills in this book the lake is read for.
pub fn watched_instruments(book: &Book, today: &str) -> BTreeSet<String> {
    book.ledger
        .executions
        .values()
        .filter(|held| {
            !held.factor_taken.is_empty()
                && held.superseded_by.is_none()
                && !held.is_bust()
                && watched(&held.factor_row, today)
        })
        .map(|held| crate::became::current(&held.execution.instrument_id))
        .collect()
}

/// The latest version of a month's row, with its version.
fn latest_of<'a>(rows: &'a [Factor], month: &str) -> Option<(&'a Factor, u64)> {
    rows.iter()
        .filter(|row| row.factor.is_some() && row.factor_month == month)
        .map(|row| (row, row.meta.as_ref().map_or(0, |m| m.version)))
        .max_by_key(|(_, version)| *version)
}

/// What each watched fill of this book on `instrument` is due, as the lake's
/// rows stand: a provisional fill whose awaited month is published; a final
/// one whose row was republished at another factor.
pub fn due(book: &Book, instrument: &str, rows: &[Factor], today: &str) -> Vec<Due> {
    let mut out = Vec::new();
    for (id, held) in &book.ledger.executions {
        if held.factor_taken.is_empty()
            || held.superseded_by.is_some()
            || held.is_bust()
            || !watched(&held.factor_row, today)
            || !crate::became::same(&held.execution.instrument_id, instrument)
        {
            continue;
        }
        let Ok(old) = held.factor_taken.parse::<Exact>() else {
            continue;
        };
        let row = &held.factor_row;
        let (month, newer_than) = if row.awaited.is_empty() {
            (row.month.as_str(), Some(row.version))
        } else {
            (row.awaited.as_str(), None)
        };
        let Some((published, version)) = latest_of(rows, month) else {
            continue;
        };
        if newer_than.is_some_and(|held_version| version <= held_version) {
            continue;
        }
        let Some(factor) = published
            .factor
            .as_ref()
            .and_then(|f| Exact::from_wire(f).ok())
        else {
            continue;
        };
        // A republication at the same factor changes nothing to revise.
        if newer_than.is_some() && factor == old {
            continue;
        }
        out.push(Due {
            execution_id: id.clone(),
            factor,
            row: FactorRow {
                month: month.to_string(),
                version,
                payment_date: published.payment_date.clone(),
                awaited: String::new(),
            },
        });
    }
    out
}

/// A pool's fill's money at a factor: original face times price per 100
/// times the factor, to the cent half-even (993).
fn at_factor(field: &str, face: Exact, unit: Exact, factor: Exact) -> Result<Exact> {
    let hundredth = Exact::new(1, 2).expect("0.01");
    let per = orders::multiply(field, orders::multiply(field, face, unit)?, hundredth)?;
    numbers::cents(field, orders::multiply(field, per, factor)?)
}

fn magnitude(value: Exact) -> Exact {
    if value.is_negative() {
        value.negated()
    } else {
        value
    }
}

/// The lots a fill opened, each with its quantity: its own entry's lines,
/// or the accept's that moved it (W11.23).
fn opened_lots(book: &Book, execution_id: &str) -> Result<Vec<(String, Exact, String)>> {
    let Some(held) = book.ledger.executions.get(execution_id) else {
        return Ok(Vec::new());
    };
    let (entry_id, range) = match &held.accepted_lines {
        Some((entry_id, start, count)) => (entry_id.as_str(), Some((*start, *count))),
        None => (held.entry_id.as_str(), None),
    };
    let body = book.body(entry_id)?;
    let lines: &[MovementLine] = match range {
        Some((start, count)) => body.lines.get(start..start + count).unwrap_or_default(),
        None => &body.lines,
    };
    let mut out = Vec::new();
    for line in lines {
        if line.opens_lot.is_none()
            || line.lot_id.is_empty()
            || !crate::became::same(&line.instrument_id, &held.execution.instrument_id)
        {
            continue;
        }
        let quantity = numbers::required("line.quantity", line.quantity.as_ref())?;
        let holding = book
            .lot_owner(&line.lot_id)
            .and_then(|key| book.positions.get(key))
            .and_then(|position| position.lots.get(&line.lot_id))
            .and_then(|state| state.lot.terms.as_ref())
            .map(|terms| terms.holding_period_start.clone())
            .unwrap_or_default();
        out.push((line.lot_id.clone(), magnitude(quantity), holding));
    }
    Ok(out)
}

/// The book's own record of each revision due in one account: an entry
/// moving each fill's cash difference, adjusting the cost of the lots it
/// opened and clearing their mark, writing its factor revision, and holding
/// each booking of it (W9.16, W11.11, W11.27).
pub fn revise(book: &Book, head: u64, ctx: &Context, dues: &[Due]) -> Result<Made> {
    if dues.is_empty() {
        return Err(StoreError::Invalid(NOTHING_DUE.into()));
    }
    let today = ctx.today();
    let method = LotReliefMethod::try_from(attributes_of(book).lot_relief_default)
        .unwrap_or(LotReliefMethod::Unspecified);
    let mut body = Body {
        trade: true,
        ..Default::default()
    };
    let mut held_orders: BTreeMap<String, String> = BTreeMap::new();
    for due in dues {
        let held = book
            .ledger
            .executions
            .get(&due.execution_id)
            .ok_or_else(|| {
                StoreError::Invalid(format!("no execution {} to revise", due.execution_id))
            })?;
        let execution = &held.execution;
        let old = held
            .factor_taken
            .parse::<Exact>()
            .map_err(|_| StoreError::Invalid(format!("{} took no factor", due.execution_id)))?;
        let face = numbers::required("quantity", execution.quantity.as_ref())?;
        let price = execution
            .price
            .clone()
            .ok_or_else(|| StoreError::Invalid(format!("{} states no price", due.execution_id)))?;
        let unit = numbers::amount("price", &price)?;
        let cash_old = at_factor("cash", face, unit, old)?;
        let cash_new = at_factor("cash", face, unit, due.factor)?;
        let difference = numbers::add("cash", cash_new, cash_old.negated())?;
        let buy = execution.side == Side::Buy as i32;
        // Signed as received: a buy pays more where the factor rose.
        let received = if buy {
            difference.negated()
        } else {
            difference
        };
        let value_date = if execution.settlement_date.is_empty() {
            execution.computed_settlement_date.clone()
        } else {
            execution.settlement_date.clone()
        };
        // Before settlement, to its date; on or after it, an open claim.
        let claim = !value_date.is_empty() && value_date.as_str() <= today.as_str();
        let line_date = if claim {
            String::new()
        } else {
            value_date.clone()
        };
        let cash = cash_instrument(&price)?;
        let mut moved = if received.is_zero() {
            Vec::new()
        } else {
            currency_lines(
                book,
                ctx,
                &cash,
                received,
                &line_date,
                &execution.sleeve,
                &execution.trade_date,
                None,
                method,
                &[],
                &body.lines,
            )?
        };
        // Pending either way: to the settlement date, or dated nothing.
        for line in &mut moved {
            line.bucket = SettlementBucket::Pending as i32;
        }
        body.lines.extend(moved);
        let mut cost_change = Exact::ZERO;
        let mut lot_ids = Vec::new();
        for (lot_id, quantity, holding) in opened_lots(book, &due.execution_id)? {
            let change = numbers::add(
                "cost",
                at_factor("cost", quantity, unit, due.factor)?,
                at_factor("cost", quantity, unit, old)?.negated(),
            )?;
            cost_change = numbers::add("cost", cost_change, change)?;
            if !change.is_zero() {
                body.basis_adjustments.push(BasisAdjustment {
                    lot_id: lot_id.clone(),
                    holding_period_start: String::new(),
                    cost: Some(basis_adjustment::Cost::CostChange(
                        meridian_domain::v1::Money {
                            amount: numbers::wire(change),
                            ..price.clone()
                        },
                    )),
                });
                body.prior_holding_period_starts.push(holding);
            }
            lot_ids.push(lot_id);
        }
        body.lifecycle_events.push(LifecycleEvent {
            event_id: ctx.mint(crate::lifecycle::PREFIX),
            account_id: book.account_id.clone(),
            instrument_id: crate::became::current(&execution.instrument_id),
            kind: LifecycleEventKind::FactorRevision as i32,
            state: if claim {
                LifecycleEventState::ClaimOpen as i32
            } else {
                LifecycleEventState::Booked as i32
            },
            quantity: numbers::wire(face),
            event_date: today.clone(),
            cash: Some(meridian_domain::v1::Money {
                amount: numbers::wire(received),
                ..price.clone()
            }),
            factor: numbers::wire(due.factor),
            prior_factor: numbers::wire(old),
            cost_change: Some(meridian_domain::v1::Money {
                amount: numbers::wire(cost_change),
                ..price.clone()
            }),
            execution_id: due.execution_id.clone(),
            ..Default::default()
        });
        body.acts.push(Act::FactorRevised {
            execution_id: due.execution_id.clone(),
            factor_taken: due.factor.to_string(),
            factor_row: due.row.clone(),
            lot_ids,
        });
        if !execution.order_id.is_empty() {
            held_orders.insert(
                execution.order_id.clone(),
                format!(
                    "the factor for {} was published at {} (execution {} was priced at {})",
                    due.row.month, due.factor, due.execution_id, old
                ),
            );
        }
    }
    // A booking of a revised fill is held, or marked moved after its
    // hand-off, as any fact after booking (W11.27): its correction restates
    // its allocations.
    let mut after = Vec::new();
    for (order_id, why) in &held_orders {
        after.extend(crate::booking::after_facts(book, order_id, why));
    }
    let alerts = crate::booking::held_alerts_for(ctx, book, &after);
    body.acts.extend(after);
    body.acts.extend(alerts);
    let revised: Vec<&str> = dues.iter().map(|due| due.execution_id.as_str()).collect();
    made(
        book,
        head,
        ctx,
        Draft {
            entry_id: ctx.mint(crate::ids::ENTRY),
            kind: REVISED,
            effective_date: today,
            reason: format!(
                "contract v29 (W11.11; ruling 992): the factor published for {} revised",
                revised.join(", ")
            ),
            break_ids: Vec::new(),
            idempotency_key: String::new(),
            actor: ctx.actor(),
            body,
        },
    )
}

/// A factor revision's open claim marked settled (W9.18; ruling 992): its
/// cash moved from pending, dated nothing, to settled, in its own entry
/// naming the person and the note; the event booked.
pub fn claim_settled(
    book: &Book,
    head: u64,
    ctx: &Context,
    claim: &LifecycleEvent,
    note: &str,
) -> Result<Made> {
    crate::order_decide::in_scope(ctx, &claim.account_id)?;
    let cash = claim
        .cash
        .clone()
        .ok_or_else(|| StoreError::Invalid(format!("{} moves no cash", claim.event_id)))?;
    let amount = numbers::amount("cash", &cash)?;
    let instrument = cash_instrument(&cash)?;
    let sleeve = book
        .ledger
        .executions
        .get(&claim.execution_id)
        .map(|held| held.execution.sleeve.clone())
        .unwrap_or_default();
    let line = |bucket: SettlementBucket, quantity: Exact| MovementLine {
        instrument_id: instrument.clone(),
        side: HoldingSide::Long as i32,
        bucket: bucket as i32,
        value_date: String::new(),
        quantity: numbers::wire(quantity),
        sleeve: sleeve.clone(),
        ..Default::default()
    };
    let lines = if amount.is_zero() {
        Vec::new()
    } else {
        vec![
            line(SettlementBucket::Pending, amount.negated()),
            line(SettlementBucket::Settled, amount),
        ]
    };
    let settled = LifecycleEvent {
        state: LifecycleEventState::Booked as i32,
        last_change: None,
        ..claim.clone()
    };
    made(
        book,
        head,
        ctx,
        Draft {
            entry_id: ctx.mint(crate::ids::ENTRY),
            kind: CLAIM_SETTLED,
            effective_date: ctx.today(),
            reason: note.to_string(),
            break_ids: Vec::new(),
            idempotency_key: String::new(),
            actor: ctx.actor(),
            body: Body {
                lines,
                lifecycle_events: vec![settled],
                trade: true,
                ..Default::default()
            },
        },
    )
}

/// Every account's due revisions, each account its own entry, the book's own
/// act at the moment it ran. Says how many entries it made.
pub fn sweep(bus: &Bus, store: &dyn Store) -> std::result::Result<usize, String> {
    let today = crate::dates::date_of(bus.clock().now_ns());
    let mut rows: BTreeMap<String, Option<Vec<Factor>>> = BTreeMap::new();
    let mut made_entries = 0;
    for account in store.accounts().map_err(|failed| failed.to_string())? {
        let book = store.book(&account).map_err(|failed| failed.to_string())?;
        let instruments = watched_instruments(&book, &today);
        drop(book);
        for instrument in instruments {
            let held = rows
                .entry(instrument.clone())
                .or_insert_with(|| crate::service::factor_rows(bus, &instrument))
                .clone();
            let Some(held) = held else {
                tracing::warn!(
                    instrument,
                    "the lake did not answer for the factors awaited"
                );
                continue;
            };
            let now = bus.clock().now_ns();
            let ctx = Context {
                instance_id: bus.instance_id().to_string(),
                event_time_ns: now,
                received_at_ns: now,
                committed_at_ns: now,
                ..Default::default()
            };
            let outcome = store.act(&account, "", "", &[], &mut |book, head| {
                let dues = due(book, &instrument, &held, &ctx.today());
                if dues.is_empty() {
                    return Err(StoreError::Invalid(NOTHING_DUE.into()));
                }
                let made = revise(book, head, &ctx, &dues)?;
                Ok(crate::service::decided(made, Vec::new()))
            });
            match outcome {
                Ok(crate::store::Acted::Committed(decided)) => {
                    crate::service::announce(bus, &decided, None, None);
                    made_entries += 1;
                }
                Ok(crate::store::Acted::Duplicate(_)) => {}
                Err(StoreError::Invalid(said)) if said == NOTHING_DUE => {}
                Err(failed) => return Err(failed.to_string()),
            }
        }
    }
    Ok(made_entries)
}

/// Sweep now, and then every `every`, for as long as the process runs.
pub async fn sweep_forever(bus: Arc<Bus>, store: Arc<dyn Store>, every: Duration) {
    loop {
        let (sweeping, holding) = (bus.clone(), store.clone());
        match tokio::task::spawn_blocking(move || sweep(&sweeping, holding.as_ref())).await {
            Ok(Ok(made)) if made > 0 => tracing::info!(made, "factor revisions were written"),
            Ok(Ok(_)) => {}
            Ok(Err(why)) => tracing::warn!(why, "could not sweep for the factors awaited"),
            Err(failed) => tracing::warn!(%failed, "the factor sweep failed"),
        }
        tokio::time::sleep(every).await;
    }
}
