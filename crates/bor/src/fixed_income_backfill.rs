//! The upgrade's fixed-income backfill (contract v29: W11.11, W11.27;
//! decisions/031), once, at the first start of a v29 book.
//!
//! - A fixed-income execution held before v29 keeps its reporting fields
//!   empty, but its capacity is backfilled from its order's where the
//!   order's is single (not mixed, not unspecified), each its own act at the
//!   backfill's own time -- never back-dated (W11.11).
//! - A fixed-income booking held before v29 holds no accrued interest and
//!   none of the customer leg's reporting fields; nothing is invented, and
//!   each is given its own record saying they were not recorded at the time
//!   (W11.27).
//!
//! What v29's start is: the time the control partition's record of the
//! backfill begun states, written before any account is touched, so an
//! execution or a booking a v29 book minted after it -- an identifier sorts
//! by the time it was minted -- is never taken as held before, even where a
//! start is interrupted and the next one finishes the pass. Each account
//! that had anything to backfill has one entry of its own, so a second pass
//! leaves it.

use std::collections::BTreeSet;

use meridian_bus::Bus;
use meridian_domain::v1::{BookingState, Capacity};

use crate::book::Book;
use crate::decide::{made, Context, Draft};
use crate::journal::Body;
use crate::orders::Act;
use crate::store::{Acted, Store, StoreError, CONTROL_ACCOUNT};

/// The entry kinds this writes.
pub const BACKFILLED: &str = "fixed-income-backfilled";
pub const BEGUN: &str = "fixed-income-backfill-begun";
pub const DONE: &str = "fixed-income-backfill-done";

const NOTHING_OWED: &str = "nothing to backfill";

/// The acts an account owes, given its instruments' records: each
/// fixed-income execution minted before `cutoff_ns` with no capacity, on an
/// order of a single one; each live fixed-income booking minted before it.
pub fn owed(book: &Book, ctx: &Context, cutoff_ns: i64) -> Vec<Act> {
    let mut acts = Vec::new();
    for (id, held) in &book.ledger.executions {
        let execution = &held.execution;
        if !crate::ids::minted_before(id, cutoff_ns)
            || !ctx.is_debt(&execution.instrument_id)
            || execution.capacity != Capacity::Unspecified as i32
        {
            continue;
        }
        let Some(order) = book.ledger.orders.get(&execution.order_id) else {
            continue;
        };
        let capacity = order.order.capacity;
        if capacity == Capacity::Unspecified as i32 || capacity == Capacity::Mixed as i32 {
            continue;
        }
        acts.push(Act::CapacityBackfilled {
            execution_id: id.clone(),
            capacity,
        });
    }
    for (id, held) in &book.ledger.bookings {
        if !crate::ids::minted_before(id, cutoff_ns) || held.state == BookingState::Cancelled {
            continue;
        }
        let debt = held.booking.execution_ids.iter().any(|execution_id| {
            book.ledger
                .executions
                .get(execution_id)
                .is_some_and(|e| ctx.is_debt(&e.execution.instrument_id))
        });
        if !debt {
            continue;
        }
        acts.push(Act::NotRecordedBackfilled {
            booking_id: id.clone(),
            allocation_ids: held
                .booking
                .allocations
                .iter()
                .map(|allocation| allocation.allocation_id.clone())
                .collect(),
        });
    }
    acts
}

/// The instruments an account's acts held before `cutoff_ns` name: those a
/// backfill asks the instrument store about.
fn named_before(book: &Book, cutoff_ns: i64) -> Vec<String> {
    let mut named = BTreeSet::new();
    for (id, held) in &book.ledger.executions {
        if crate::ids::minted_before(id, cutoff_ns) {
            named.insert(held.execution.instrument_id.clone());
        }
    }
    named.into_iter().collect()
}

fn context(bus: &Bus) -> Context {
    let now = bus.clock().now_ns();
    Context {
        instance_id: bus.instance_id().to_string(),
        event_time_ns: now,
        received_at_ns: now,
        committed_at_ns: now,
        ..Default::default()
    }
}

/// The control partition's record of the backfill, begun or done.
fn record_control(bus: &Bus, store: &dyn Store, cutoff_ns: i64, done: bool) -> Result<(), String> {
    let ctx = context(bus);
    let acted = store.act(CONTROL_ACCOUNT, "", "", &[], &mut |book, head| {
        let now = book.ledger.fixed_income_backfill;
        if now == Some((cutoff_ns, done)) || (!done && now.is_some()) {
            return Err(StoreError::Invalid(NOTHING_OWED.into()));
        }
        let made = made(
            book,
            head,
            &ctx,
            Draft {
                entry_id: ctx.mint(crate::ids::ENTRY),
                kind: if done { DONE } else { BEGUN },
                effective_date: ctx.today(),
                reason: "contract v29: the fixed-income records held before the upgrade \
                         backfilled, each at its own time (W11.11, W11.27, decisions/031)"
                    .into(),
                break_ids: Vec::new(),
                idempotency_key: String::new(),
                actor: ctx.actor(),
                body: Body {
                    acts: vec![Act::FixedIncomeBackfill { cutoff_ns, done }],
                    ..Default::default()
                },
            },
        )?;
        Ok(crate::service::decided(made, Vec::new()))
    });
    match acted {
        Ok(_) => Ok(()),
        Err(StoreError::Invalid(said)) if said == NOTHING_OWED => Ok(()),
        Err(failed) => Err(failed.to_string()),
    }
}

/// The backfill, run at start until done. Says how many accounts it wrote
/// to; an instrument store that does not answer leaves it to the next start.
pub fn backfill(bus: &Bus, store: &dyn Store) -> Result<usize, String> {
    let control = store
        .book(CONTROL_ACCOUNT)
        .map_err(|failed| failed.to_string())?;
    let cutoff_ns = match control.ledger.fixed_income_backfill {
        Some((_, true)) => return Ok(0),
        Some((cutoff_ns, false)) => cutoff_ns,
        None => {
            let cutoff_ns = bus.clock().now_ns();
            record_control(bus, store, cutoff_ns, false)?;
            store
                .book(CONTROL_ACCOUNT)
                .map_err(|failed| failed.to_string())?
                .ledger
                .fixed_income_backfill
                .map_or(cutoff_ns, |(held, _)| held)
        }
    };
    drop(control);
    let mut written = 0;
    for account in store.accounts().map_err(|failed| failed.to_string())? {
        if account == CONTROL_ACCOUNT {
            continue;
        }
        let book = store.book(&account).map_err(|failed| failed.to_string())?;
        if book.entries.iter().any(|entry| entry.kind == BACKFILLED) {
            continue;
        }
        let named = named_before(&book, cutoff_ns);
        drop(book);
        if named.is_empty() {
            continue;
        }
        let mut ctx = context(bus);
        let found = crate::service::references(bus, &named, ctx.received_at_ns);
        if found.unavailable {
            return Err(
                "the instrument store did not answer; the backfill waits for the next \
                        start"
                    .into(),
            );
        }
        ctx.records = found.records;
        let acted = store.act(&account, "", "", &[], &mut |book, head| {
            let acts = owed(book, &ctx, cutoff_ns);
            if acts.is_empty() {
                return Err(StoreError::Invalid(NOTHING_OWED.into()));
            }
            let made = made(
                book,
                head,
                &ctx,
                Draft {
                    entry_id: ctx.mint(crate::ids::ENTRY),
                    kind: BACKFILLED,
                    effective_date: ctx.today(),
                    reason: "contract v29: a fixed-income execution's capacity backfilled from \
                             its order's where single (W11.11); a fixed-income booking's \
                             accrued interest and reporting fields not recorded at the time, \
                             never invented (W11.27); decisions/031"
                        .into(),
                    break_ids: Vec::new(),
                    idempotency_key: String::new(),
                    actor: ctx.actor(),
                    body: Body {
                        acts,
                        ..Default::default()
                    },
                },
            )?;
            Ok(crate::service::decided(made, Vec::new()))
        });
        match acted {
            Ok(Acted::Committed(decided)) => {
                crate::service::announce(bus, &decided, None, None);
                written += 1;
            }
            Ok(Acted::Duplicate(_)) => {}
            Err(StoreError::Invalid(said)) if said == NOTHING_OWED => {}
            Err(failed) => return Err(failed.to_string()),
        }
    }
    record_control(bus, store, cutoff_ns, true)?;
    Ok(written)
}
