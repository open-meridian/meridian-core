//! Where booking, the alert and the escape hatch meet the bus (W11.26 to
//! W11.37, contract v24).
//!
//! Nine commands and facts in, three reads, two events out beside W11's
//! OrderUpdated and W9's PositionChanged; and two acts of the book's own: the
//! child entries a booking owes the other accounts it allocates to, made
//! after the booking commits and swept for on start and every quarter hour,
//! each idempotent by its key (decisions/024: no act writes two partitions);
//! and, once at the upgrade, the escape hatch recorded on by default
//! (decisions/031).
//!
//! **What a command reads before its account's lock.** A booking's
//! allocations name other accounts, whose marks (error account, allocation
//! account, W9.13) decide who may be allocated to; a block names member orders
//! in other accounts, whose state, instrument, side and idle quantity decide
//! the block (W11.1). Both are read here, before the lock, as the accept reads
//! where each execution is: no decision reads two partitions under one lock.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use meridian_bus::{Bus, Envelope};
use meridian_domain::v1::{
    AcknowledgeAlertRequest, AlertChangedEvent, AlertReply, BookOrderRequest, BookingReply,
    BookingUpdatedEvent, CorrectBookingRequest, FactReply, HandOffBookingRequest, ListAlertsReply,
    ListAlertsRequest, ListBookingsReply, ListBookingsRequest, LocalRecordOrdersSetting,
    RaiseAlertRequest, ReadLocalRecordOrdersRequest, RecordSleeveCrossRequest, ReportGiveUpRequest,
    SetAllocationInstructionRequest, SetLocalRecordOrdersRequest, SleeveCrossReply,
};
use meridian_domain::v1::{
    AgentOrdersDemoOnlySetting, ReadAgentOrdersDemoOnlyRequest, SetAgentOrdersDemoOnlyRequest,
};
use meridian_pb::bounds;
use prost::Message;

use crate::book::Book;
use crate::booking::{self, ChildMove, MemberView};
use crate::decide::{Context, Made};
use crate::service::{command_store, context_of, decode, references, scope_of, Clock};
use crate::store::{
    mark_of, page_limit, watermark_of, Acted, AlertsRead, AttributesRead, BookingsRead, Decided,
    Record, Scope, Store, StoreError, CONTROL_ACCOUNT,
};

pub const SET_ALLOCATION_INSTRUCTION: &str = "platform.book.command.set-allocation-instruction";
pub const BOOK_ORDER: &str = "platform.book.command.book-order";
pub const HAND_OFF_BOOKING: &str = "platform.book.command.hand-off-booking";
pub const CORRECT_BOOKING: &str = "platform.book.command.correct-booking";
pub const REPORT_GIVE_UP: &str = "platform.book.command.report-give-up";
pub const RECORD_SLEEVE_CROSS: &str = "platform.book.command.record-sleeve-cross";
pub const RAISE_ALERT: &str = "platform.book.command.raise-alert";
pub const ACKNOWLEDGE_ALERT: &str = "platform.book.command.acknowledge-alert";
pub const SET_LOCAL_RECORD_ORDERS: &str = "platform.book.command.set-local-record-orders";
/// W11.38 (contract v26): the agents' guard set, and read (W11.39).
pub const SET_AGENT_ORDERS_DEMO_ONLY: &str = "platform.book.command.set-agent-orders-demo-only";
pub const READ_AGENT_ORDERS_DEMO_ONLY: &str = "platform.book.query.read-agent-orders-demo-only";

pub const BOOKING_UPDATED: &str = "platform.book.event.booking-updated";
pub const ALERT_CHANGED: &str = "platform.book.event.alert-changed";

pub const LIST_BOOKINGS: &str = "platform.book.query.list-bookings";
pub const LIST_ALERTS: &str = "platform.book.query.list-alerts";
pub const READ_LOCAL_RECORD_ORDERS: &str = "platform.book.query.read-local-record-orders";
/// W9.10 (contract v25; ruling 155): the book's health per account.
pub const READ_BOOK_HEALTH: &str = "platform.book.query.read-book-health";

/// How often the child entries every booking owes are looked for.
pub const SWEEP_EVERY: Duration = Duration::from_secs(15 * 60);

/// Each booking and alert an entry changed, whole (W11.30, W11.35).
pub fn announce(say: &dyn Fn(&str, &str, Vec<u8>), decided: &Decided) {
    let entry = &decided.entry;
    for booking in &decided.changes.bookings {
        let mut event = BookingUpdatedEvent {
            booking: Some(booking.clone()),
            entry: Some(entry.meta.clone()),
            journal: booking.last_change.clone(),
            cause: Some(entry.cause.clone()),
        };
        meridian_domain::money::fill_known(&mut event);
        say(
            BOOKING_UPDATED,
            "meridian.v1.BookingUpdatedEvent",
            event.encode_to_vec(),
        );
    }
    for alert in &decided.changes.alerts {
        say(
            ALERT_CHANGED,
            "meridian.v1.AlertChangedEvent",
            AlertChangedEvent {
                alert: Some(alert.clone()),
                journal: alert.last_change.clone(),
                cause: Some(entry.cause.clone()),
            }
            .encode_to_vec(),
        );
    }
}

fn booking_reply(made: &Made, booking_id: &str) -> Vec<u8> {
    let booking = made
        .changes
        .bookings
        .iter()
        .find(|held| held.booking_id == booking_id)
        .cloned()
        .or_else(|| made.book.ledger.booking_record(booking_id).ok());
    let mut reply = BookingReply {
        journal: booking.as_ref().and_then(|held| held.last_change.clone()),
        booking,
        entry: Some(made.entry.meta.clone()),
    };
    meridian_domain::money::fill_known(&mut reply);
    reply.encode_to_vec()
}

fn alert_reply(made: &Made) -> Vec<u8> {
    let alert = made.changes.alerts.first().cloned();
    AlertReply {
        journal: alert.as_ref().and_then(|held| held.last_change.clone()),
        alert,
    }
    .encode_to_vec()
}

fn owner(store: &dyn Store, record: Record, id: &str, named: &str) -> Result<String, String> {
    if id.is_empty() {
        return Err(StoreError::Malformed {
            fields: vec![named.into()],
            words: "the command names no record".into(),
        }
        .on_the_bus());
    }
    store
        .owner(record, id)
        .map_err(|failed| failed.on_the_bus())?
        .ok_or_else(|| StoreError::Invalid(format!("the book holds no record {id}")).on_the_bus())
}

/// Each account's attributes, read before the lock (W11.27): which are
/// marked error accounts or allocation accounts.
pub fn account_marks(
    store: &dyn Store,
    accounts: impl IntoIterator<Item = String>,
) -> BTreeMap<String, meridian_domain::v1::AccountAttributes> {
    let mut marks = BTreeMap::new();
    for account in accounts {
        if account.is_empty() || marks.contains_key(&account) {
            continue;
        }
        if let Ok(page) = store.attributes(&AttributesRead {
            scope: Scope::Everything,
            account_id: account.clone(),
            since: None,
            limit: 1,
            cursor: String::new(),
        }) {
            if let Some(held) = page.records.into_iter().next() {
                marks.insert(account, held);
            }
        }
    }
    marks
}

/// A block's members, each read from its own account before the lock
/// (W11.1). One the book does not hold is left out, and the decision names
/// it.
pub fn member_views(store: &dyn Store, members: &[String]) -> BTreeMap<String, MemberView> {
    let mut views = BTreeMap::new();
    for member in members {
        let Ok(Some(account)) = store.owner(Record::Order, member) else {
            continue;
        };
        let Ok(book) = store.book(&account) else {
            continue;
        };
        let Some(held) = book.ledger.orders.get(member) else {
            continue;
        };
        let state = book.ledger.state(member).unwrap_or_default();
        let idle = book
            .ledger
            .quantities(member)
            .ok()
            .and_then(|quantities| quantities.idle)
            .and_then(|idle| meridian_domain::exact::Exact::from_wire(&idle).ok())
            .unwrap_or(meridian_domain::exact::Exact::ZERO);
        let instructed = match held.order.allocation_instruction.as_ref() {
            Some(instruction) if !instruction.lines.is_empty() => instruction
                .lines
                .iter()
                .map(|line| line.account_id.clone())
                .collect(),
            _ => vec![account.clone()],
        };
        let in_block = store
            .blocks_naming(member)
            .ok()
            .and_then(|blocks| blocks.into_iter().next())
            .map(|(_, block)| block)
            .unwrap_or_default();
        views.insert(
            member.clone(),
            MemberView {
                order_id: member.clone(),
                account_id: account,
                instrument_id: held.order.instrument_id.clone(),
                side: held.order.side,
                state: state as i32,
                idle,
                instructed,
                in_block,
            },
        );
    }
    views
}

/// The escape hatch's setting in force (W11.36), read from the control
/// partition: `None` before anything stated it, which reads as on.
pub fn local_record_orders(store: &dyn Store) -> Option<bool> {
    let book = store.book(CONTROL_ACCOUNT).ok()?;
    book.ledger.local_record_orders
}

/// The setting with its count (W11.37): the live orders across the
/// deployment whose instrument's record is provisional, naming no account
/// and no order.
pub fn local_record_orders_setting(bus: &Bus, store: &dyn Store) -> LocalRecordOrdersSetting {
    let instruments = store.live_order_instruments().unwrap_or_default();
    let mut distinct: Vec<String> = instruments.clone();
    distinct.sort();
    distinct.dedup();
    let found = references(bus, &distinct, bus.clock().now_ns());
    let count = instruments
        .iter()
        .filter(|id| found.records.get(*id).is_some_and(|says| says.provisional))
        .count();
    LocalRecordOrdersSetting {
        allows_local_record_orders: local_record_orders(store).unwrap_or(true),
        open_order_count: count as i64,
    }
}

const NOTHING_OWED: &str = "the child entry was made already";

/// The child entries a booking owes, each in its account's own partition,
/// idempotent by its key: what a booking's commit and the sweep both make.
pub fn make_children(
    bus: &Bus,
    store: &dyn Store,
    moves: &[ChildMove],
    withdrawals: &[(String, String, String)],
    correlation: Option<&str>,
    causation: Option<&str>,
) -> usize {
    let mut made = 0;
    let now = bus.clock().now_ns();
    let ctx = Context {
        instance_id: bus.instance_id().to_string(),
        event_time_ns: now,
        received_at_ns: now,
        committed_at_ns: now,
        ..Default::default()
    };
    for (account, booking_id, allocation_id) in withdrawals {
        let key = format!("allocation-out:{booking_id}:{allocation_id}");
        let acted = store.act(account, "", &key, key.as_bytes(), &mut |book, head| {
            match booking::withdraw_allocation(book, head, &ctx, booking_id, allocation_id)? {
                Some(made) => Ok(crate::service::decided(made, Vec::new())),
                None => Err(StoreError::Invalid(NOTHING_OWED.into())),
            }
        });
        made += said(bus, acted, correlation, causation);
    }
    for moved in moves {
        let key = format!(
            "allocation-in:{}:{}",
            moved.booking_id, moved.allocation.allocation_id
        );
        let acted = store.act(
            &moved.allocation.account_id,
            "",
            &key,
            key.as_bytes(),
            &mut |book, head| match booking::receive_allocation(book, head, &ctx, moved)? {
                Some(made) => Ok(crate::service::decided(made, Vec::new())),
                None => Err(StoreError::Invalid(NOTHING_OWED.into())),
            },
        );
        made += said(bus, acted, correlation, causation);
    }
    made
}

fn said(
    bus: &Bus,
    acted: Result<Acted, StoreError>,
    correlation: Option<&str>,
    causation: Option<&str>,
) -> usize {
    match acted {
        Ok(Acted::Committed(decided)) => {
            crate::service::announce(bus, &decided, correlation, causation);
            1
        }
        Ok(Acted::Duplicate(_)) => 0,
        Err(StoreError::Invalid(said)) if said == NOTHING_OWED => 0,
        Err(failed) => {
            tracing::warn!(%failed, "a booking's child entry could not be made; the sweep makes it");
            0
        }
    }
}

/// Every child entry owed, across the deployment (W11.30): each allocation of
/// a live booking received by its account, and each received allocation its
/// booking no longer carries withdrawn. Says how many it made.
pub fn sweep_children(bus: &Bus, store: &dyn Store) -> Result<usize, String> {
    // One account replayed at a time, its book dropped before the next
    // (approved 2026-10-15, after the book's pod was OOMKilled): what the
    // cross-account checks need is kept as keys alone -- the allocations
    // each account received, and those each live booking carries.
    let accounts = store.accounts().map_err(|failed| failed.to_string())?;
    let mut moves = Vec::new();
    let mut received_by: BTreeSet<(String, String, String)> = BTreeSet::new();
    let mut carried_by: BTreeSet<(String, String, String)> = BTreeSet::new();
    let mut receiving: Vec<(String, String, String, String)> = Vec::new();
    for account in &accounts {
        let book = store.book(account).map_err(|failed| failed.to_string())?;
        for (id, held) in &book.ledger.bookings {
            if held.state == meridian_domain::v1::BookingState::Cancelled {
                continue;
            }
            for allocation in &held.booking.allocations {
                carried_by.insert((
                    account.clone(),
                    id.clone(),
                    allocation.allocation_id.clone(),
                ));
            }
            let members: Vec<String> = book
                .ledger
                .orders
                .get(&held.booking.order_id)
                .map(|order| order.order.block_member_order_ids.clone())
                .unwrap_or_default();
            let ctx = Context {
                members: member_views(store, &members),
                ..Default::default()
            };
            moves.extend(booking::child_moves(&book, id, &ctx).map_err(|f| f.to_string())?);
        }
        for ((booking_id, allocation_id), received) in &book.ledger.received {
            received_by.insert((account.clone(), booking_id.clone(), allocation_id.clone()));
            if !received.withdrawn {
                receiving.push((
                    account.clone(),
                    booking_id.clone(),
                    allocation_id.clone(),
                    received.from_account.clone(),
                ));
            }
        }
    }
    moves.retain(|moved| {
        !received_by.contains(&(
            moved.allocation.account_id.clone(),
            moved.booking_id.clone(),
            moved.allocation.allocation_id.clone(),
        ))
    });
    let withdrawals: Vec<(String, String, String)> = receiving
        .into_iter()
        .filter(|(_, booking_id, allocation_id, from)| {
            !carried_by.contains(&(from.clone(), booking_id.clone(), allocation_id.clone()))
        })
        .map(|(account, booking_id, allocation_id, _)| (account, booking_id, allocation_id))
        .collect();
    Ok(make_children(bus, store, &moves, &withdrawals, None, None))
}

/// Sweep now, and then every `every`, for as long as the process runs.
pub async fn sweep_forever(bus: Arc<Bus>, store: Arc<dyn Store>, every: Duration) {
    loop {
        let (sweeping, holding) = (bus.clone(), store.clone());
        match tokio::task::spawn_blocking(move || sweep_children(&sweeping, holding.as_ref())).await
        {
            Ok(Ok(made)) if made > 0 => {
                tracing::info!(made, "child entries a booking owed were made")
            }
            Ok(Ok(_)) => {}
            Ok(Err(why)) => tracing::warn!(why, "could not sweep for the child entries owed"),
            Err(failed) => tracing::warn!(%failed, "the child-entry sweep failed"),
        }
        tokio::time::sleep(every).await;
    }
}

/// The agents' guard as the control partition states it (contract v26,
/// W11.38): `None` before anything stated it, which reads as on.
pub fn agent_orders_demo_only(store: &dyn Store) -> Option<bool> {
    let book = store.book(CONTROL_ACCOUNT).ok()?;
    book.ledger.agent_orders_demo_only
}

/// The upgrade's record of the agents' guard on (contract v26, W11.38,
/// decisions/031): once, the book's own act, at the moment it ran. Says
/// whether it recorded it.
pub fn backfill_agent_orders_demo_only(bus: &Bus, store: &dyn Store) -> Result<bool, String> {
    if agent_orders_demo_only(store).is_some() {
        return Ok(false);
    }
    let now = bus.clock().now_ns();
    let ctx = Context {
        instance_id: bus.instance_id().to_string(),
        event_time_ns: now,
        received_at_ns: now,
        committed_at_ns: now,
        ..Default::default()
    };
    let key = "agent-orders-demo-only:default";
    let acted = store.act(
        CONTROL_ACCOUNT,
        "",
        key,
        key.as_bytes(),
        &mut |book, head| {
            if book.ledger.agent_orders_demo_only.is_some() {
                return Err(StoreError::Invalid(NOTHING_OWED.into()));
            }
            let draft = crate::decide::Draft {
                entry_id: ctx.mint(crate::ids::ENTRY),
                kind: "agent-orders-demo-only-backfilled",
                effective_date: ctx.today(),
                reason: "contract v26: agents may place orders on demo connections only, the \
                         guard recorded on at the upgrade (W11.38, decisions/031)"
                    .into(),
                break_ids: Vec::new(),
                idempotency_key: key.into(),
                actor: ctx.actor(),
                body: crate::journal::Body {
                    acts: vec![crate::orders::Act::AgentOrdersDemoOnlySet { on: true }],
                    ..Default::default()
                },
            };
            let made = crate::decide::made(book, head, &ctx, draft)?;
            Ok(crate::service::decided(made, Vec::new()))
        },
    );
    match acted {
        Ok(Acted::Committed(_)) => Ok(true),
        Ok(Acted::Duplicate(_)) => Ok(false),
        Err(StoreError::Invalid(said)) if said == NOTHING_OWED => Ok(false),
        Err(failed) => Err(failed.to_string()),
    }
}

/// The upgrade's record of the escape hatch on by default (W11.36,
/// decisions/031): once, the book's own act, at the moment it ran. Says
/// whether it recorded it.
pub fn backfill_local_record_orders(bus: &Bus, store: &dyn Store) -> Result<bool, String> {
    if local_record_orders(store).is_some() {
        return Ok(false);
    }
    let now = bus.clock().now_ns();
    let ctx = Context {
        instance_id: bus.instance_id().to_string(),
        event_time_ns: now,
        received_at_ns: now,
        committed_at_ns: now,
        ..Default::default()
    };
    let key = "local-record-orders:default";
    let acted = store.act(
        CONTROL_ACCOUNT,
        "",
        key,
        key.as_bytes(),
        &mut |book, head| {
            if book.ledger.local_record_orders.is_some() {
                return Err(StoreError::Invalid(NOTHING_OWED.into()));
            }
            let draft = crate::decide::Draft {
                entry_id: ctx.mint(crate::ids::ENTRY),
                kind: "local-record-orders-backfilled",
                effective_date: ctx.today(),
                reason: "contract v24: orders on local records allowed by default, the escape \
                         hatch recorded on at the upgrade (W11.36, decisions/031)"
                    .into(),
                break_ids: Vec::new(),
                idempotency_key: key.into(),
                actor: ctx.actor(),
                body: crate::journal::Body {
                    acts: vec![crate::orders::Act::LocalRecordOrdersSet { allows: true }],
                    ..Default::default()
                },
            };
            let made = crate::decide::made(book, head, &ctx, draft)?;
            Ok(crate::service::decided(made, Vec::new()))
        },
    );
    match acted {
        Ok(Acted::Committed(_)) => Ok(true),
        Ok(Acted::Duplicate(_)) => Ok(false),
        Err(StoreError::Invalid(said)) if said == NOTHING_OWED => Ok(false),
        Err(failed) => Err(failed.to_string()),
    }
}

/// One command decided in `account_id`'s book, its refusal said on the bus.
#[allow(clippy::too_many_arguments)]
fn act(
    bus: &Bus,
    store: &dyn Store,
    envelope: &Envelope,
    ctx: Context,
    account_id: &str,
    idempotency_key: &str,
    reply_type: &str,
    decide: &crate::service::Deciding<'_>,
) -> Result<(String, Vec<u8>), String> {
    command_store(
        bus,
        store,
        envelope,
        ctx,
        account_id,
        idempotency_key,
        reply_type,
        decide,
    )
    .map_err(|failed| failed.on_the_bus())
}

/// The accounts a booking's allocations name.
fn allocation_accounts(allocations: &[meridian_domain::v1::Allocation]) -> Vec<String> {
    allocations
        .iter()
        .map(|allocation| allocation.account_id.clone())
        .collect()
}

/// Register every handler booking, the alert and the escape hatch serve.
pub fn serve(bus: Arc<Bus>, store: Arc<dyn Store>, clock: Arc<dyn Clock>) {
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(SET_ALLOCATION_INSTRUCTION, move |envelope| {
            let request: SetAllocationInstructionRequest =
                decode(&envelope, "meridian.v1.SetAllocationInstructionRequest")?;
            let ctx = context_of(&envelope, clock.as_ref());
            let account = owner(store.as_ref(), Record::Order, &request.order_id, "order_id")?;
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &account,
                "",
                "meridian.v1.OrderReply",
                &|book, head, ctx| {
                    let made = booking::set_allocation_instruction(book, head, ctx, &request)?;
                    let order = made.book.ledger.order_record(&request.order_id, false).ok();
                    let mut reply = meridian_domain::v1::OrderReply {
                        journal: order.as_ref().and_then(|order| order.last_change.clone()),
                        order,
                        entry: Some(made.entry.meta.clone()),
                        change_request_id: String::new(),
                    };
                    meridian_domain::money::fill_known(&mut reply);
                    Ok((made, reply.encode_to_vec()))
                },
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(BOOK_ORDER, move |envelope| {
            let request: BookOrderRequest = decode(&envelope, "meridian.v1.BookOrderRequest")?;
            crate::cash::ensure_blocking(
                &bus2,
                &store,
                &meridian_domain::money::codes(&request),
                clock.now_ns(),
            );
            let mut ctx = context_of(&envelope, clock.as_ref());
            let account = owner(store.as_ref(), Record::Order, &request.order_id, "order_id")?;
            ctx.account_marks =
                account_marks(store.as_ref(), allocation_accounts(&request.allocations));
            let members = block_members(store.as_ref(), &account, &request.order_id);
            ctx.members = member_views(store.as_ref(), &members);
            let decided_ctx = ctx.clone();
            let booked = std::sync::Mutex::new(None::<(String, Book)>);
            let answer = act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &account,
                "",
                "meridian.v1.BookingReply",
                &|book, head, ctx| {
                    let made = booking::book_order(book, head, ctx, &request)?;
                    let booking_id = made
                        .entry
                        .body
                        .acts
                        .iter()
                        .find_map(|act| match act {
                            crate::orders::Act::BookingOpened { booking, .. } => {
                                Some(booking.booking_id.clone())
                            }
                            _ => None,
                        })
                        .unwrap_or_default();
                    *booked.lock().expect("unpoisoned") =
                        Some((booking_id.clone(), made.book.clone()));
                    let reply = booking_reply(&made, &booking_id);
                    Ok((made, reply))
                },
            )?;
            if let Some((booking_id, book)) = booked.lock().expect("unpoisoned").take() {
                let moves = booking::child_moves(&book, &booking_id, &decided_ctx)
                    .map_err(|failed| failed.on_the_bus())?;
                let meta = envelope.meta.as_ref();
                make_children(
                    &bus2,
                    store.as_ref(),
                    &moves,
                    &[],
                    meta.map(|meta| meta.correlation_id.as_str()),
                    meta.map(|meta| meta.message_id.as_str()),
                );
            }
            Ok(answer)
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(HAND_OFF_BOOKING, move |envelope| {
            let request: HandOffBookingRequest =
                decode(&envelope, "meridian.v1.HandOffBookingRequest")?;
            let ctx = context_of(&envelope, clock.as_ref());
            let account = owner(
                store.as_ref(),
                Record::Booking,
                &request.booking_id,
                "booking_id",
            )?;
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &account,
                "",
                "meridian.v1.BookingReply",
                &|book, head, ctx| {
                    let made = booking::hand_off_booking(book, head, ctx, &request)?;
                    let reply = booking_reply(&made, &request.booking_id);
                    Ok((made, reply))
                },
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(CORRECT_BOOKING, move |envelope| {
            let request: CorrectBookingRequest =
                decode(&envelope, "meridian.v1.CorrectBookingRequest")?;
            crate::cash::ensure_blocking(
                &bus2,
                &store,
                &meridian_domain::money::codes(&request),
                clock.now_ns(),
            );
            let mut ctx = context_of(&envelope, clock.as_ref());
            let account = owner(
                store.as_ref(),
                Record::Booking,
                &request.booking_id,
                "booking_id",
            )?;
            ctx.account_marks =
                account_marks(store.as_ref(), allocation_accounts(&request.allocations));
            // A block's members, read from their own accounts, as its booking
            // read them: a replace allocates to the same instructed accounts.
            let order_id = booking_order(store.as_ref(), &account, &request.booking_id);
            let members = block_members(store.as_ref(), &account, &order_id);
            ctx.members = member_views(store.as_ref(), &members);
            let decided_ctx = ctx.clone();
            let corrected = std::sync::Mutex::new(None::<(Book, Book)>);
            let answer = act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &account,
                "",
                "meridian.v1.BookingReply",
                &|book, head, ctx| {
                    let made = booking::correct_booking(book, head, ctx, &request)?;
                    *corrected.lock().expect("unpoisoned") =
                        Some((book.clone(), made.book.clone()));
                    let reply = booking_reply(&made, &request.booking_id);
                    Ok((made, reply))
                },
            )?;
            if let Some((before, after)) = corrected.lock().expect("unpoisoned").take() {
                // The version before withdrawn from each other account, and
                // a replace's new allocations received (W11.29, W11.30).
                let withdrawals: Vec<(String, String, String)> = before
                    .ledger
                    .bookings
                    .get(&request.booking_id)
                    .map(|held| {
                        booking::allocations_elsewhere(&held.booking)
                            .into_iter()
                            .map(|allocation| {
                                (
                                    allocation.account_id,
                                    request.booking_id.clone(),
                                    allocation.allocation_id,
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let moves = if after
                    .ledger
                    .bookings
                    .get(&request.booking_id)
                    .is_some_and(|held| held.state != meridian_domain::v1::BookingState::Cancelled)
                {
                    let members = block_members(
                        store.as_ref(),
                        &account,
                        &after.ledger.bookings[&request.booking_id].booking.order_id,
                    );
                    let ctx = Context {
                        members: member_views(store.as_ref(), &members),
                        ..decided_ctx.clone()
                    };
                    booking::child_moves(&after, &request.booking_id, &ctx)
                        .map_err(|failed| failed.on_the_bus())?
                } else {
                    Vec::new()
                };
                let meta = envelope.meta.as_ref();
                make_children(
                    &bus2,
                    store.as_ref(),
                    &moves,
                    &withdrawals,
                    meta.map(|meta| meta.correlation_id.as_str()),
                    meta.map(|meta| meta.message_id.as_str()),
                );
            }
            Ok(answer)
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(REPORT_GIVE_UP, move |envelope| {
            let request: ReportGiveUpRequest =
                decode(&envelope, "meridian.v1.ReportGiveUpRequest")?;
            let ctx = context_of(&envelope, clock.as_ref());
            // A give-up naming no execution the book holds is kept as an
            // unplaced fact, the report whole in its own arm, naming no
            // account (contract v25, W11.32; decision 30), as an execution
            // the book cannot place is (W11.13), its reason from contract
            // v26 UNPLACED_REASON_UNKNOWN_EXECUTION. Held to its shape first.
            if !request.execution_id.is_empty()
                && store
                    .owner(Record::Execution, &request.execution_id)
                    .map_err(|failed| failed.on_the_bus())?
                    .is_none()
            {
                booking::give_up_holds(&request).map_err(|failed| failed.on_the_bus())?;
                return crate::order_service::no_account(
                    &bus2,
                    store.as_ref(),
                    &envelope,
                    ctx,
                    meridian_domain::v1::unplaced_fact::Fact::GiveUp(request.clone()),
                    meridian_domain::v1::UnplacedReason::UnknownExecution,
                    "",
                    request.raw_record.clone(),
                );
            }
            let account = owner(
                store.as_ref(),
                Record::Execution,
                &request.execution_id,
                "execution_id",
            )?;
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &account,
                "",
                "meridian.v1.FactReply",
                &|book, head, ctx| {
                    let made = booking::report_give_up(book, head, ctx, &request)?;
                    let mut reply = FactReply {
                        execution: made
                            .book
                            .ledger
                            .executions
                            .get(&request.execution_id)
                            .map(|held| held.execution.clone()),
                        entry: Some(made.entry.meta.clone()),
                        journal: Some(made.changes.first.clone()),
                        ..Default::default()
                    };
                    meridian_domain::money::fill_known(&mut reply);
                    Ok((made, reply.encode_to_vec()))
                },
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(RECORD_SLEEVE_CROSS, move |envelope| {
            let request: RecordSleeveCrossRequest =
                decode(&envelope, "meridian.v1.RecordSleeveCrossRequest")?;
            crate::cash::ensure_blocking(
                &bus2,
                &store,
                &meridian_domain::money::codes(&request),
                clock.now_ns(),
            );
            let ctx = context_of(&envelope, clock.as_ref());
            let account = request
                .sleeve_cross
                .as_ref()
                .map(|cross| cross.account_id.clone())
                .unwrap_or_default();
            if account.is_empty() {
                return Err(StoreError::Malformed {
                    fields: vec!["sleeve_cross.account_id".into()],
                    words: "a sleeve cross names its account".into(),
                }
                .on_the_bus());
            }
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &account,
                "",
                "meridian.v1.SleeveCrossReply",
                &|book, head, ctx| {
                    let made = booking::record_sleeve_cross(book, head, ctx, &request)?;
                    let cross = made.entry.body.acts.iter().find_map(|act| match act {
                        crate::orders::Act::SleeveCrossed { cross } => Some(cross.clone()),
                        _ => None,
                    });
                    let mut reply = SleeveCrossReply {
                        sleeve_cross: cross,
                        journal: Some(made.changes.first.clone()),
                    };
                    meridian_domain::money::fill_known(&mut reply);
                    Ok((made, reply.encode_to_vec()))
                },
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(RAISE_ALERT, move |envelope| {
            let request: RaiseAlertRequest = decode(&envelope, "meridian.v1.RaiseAlertRequest")?;
            let ctx = context_of(&envelope, clock.as_ref());
            if request.account_id.is_empty() {
                return Err(StoreError::Malformed {
                    fields: vec!["account_id".into()],
                    words: "an alert names its account".into(),
                }
                .on_the_bus());
            }
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &request.account_id,
                "",
                "meridian.v1.AlertReply",
                &|book, head, ctx| {
                    let made = booking::raise_alert(book, head, ctx, &request)?;
                    let reply = alert_reply(&made);
                    Ok((made, reply))
                },
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(ACKNOWLEDGE_ALERT, move |envelope| {
            let request: AcknowledgeAlertRequest =
                decode(&envelope, "meridian.v1.AcknowledgeAlertRequest")?;
            let ctx = context_of(&envelope, clock.as_ref());
            let account = owner(store.as_ref(), Record::Alert, &request.alert_id, "alert_id")?;
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &account,
                "",
                "meridian.v1.AlertReply",
                &|book, head, ctx| {
                    let made = booking::acknowledge_alert(book, head, ctx, &request)?;
                    let alert = made.book.ledger.alerts.get(&request.alert_id).cloned();
                    let reply = AlertReply {
                        journal: alert.as_ref().and_then(|held| held.last_change.clone()),
                        alert,
                    }
                    .encode_to_vec();
                    Ok((made, reply))
                },
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(SET_LOCAL_RECORD_ORDERS, move |envelope| {
            let request: SetLocalRecordOrdersRequest =
                decode(&envelope, "meridian.v1.SetLocalRecordOrdersRequest")?;
            let ctx = context_of(&envelope, clock.as_ref());
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                CONTROL_ACCOUNT,
                "",
                "meridian.v1.LocalRecordOrdersSetting",
                &|book, head, ctx| {
                    let made = booking::set_local_record_orders(book, head, ctx, &request)?;
                    let reply = LocalRecordOrdersSetting {
                        allows_local_record_orders: request.allows_local_record_orders,
                        open_order_count: 0,
                    }
                    .encode_to_vec();
                    Ok((made, reply))
                },
            )
            .map(|(kind, _)| {
                // The count as of the answer, read after the entry commits.
                (
                    kind,
                    local_record_orders_setting(&bus2, store.as_ref()).encode_to_vec(),
                )
            })
        });
    }

    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(SET_AGENT_ORDERS_DEMO_ONLY, move |envelope| {
            let request: SetAgentOrdersDemoOnlyRequest =
                decode(&envelope, "meridian.v1.SetAgentOrdersDemoOnlyRequest")?;
            let ctx = context_of(&envelope, clock.as_ref());
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                CONTROL_ACCOUNT,
                "",
                "meridian.v1.AgentOrdersDemoOnlySetting",
                &|book, head, ctx| {
                    let made = booking::set_agent_orders_demo_only(book, head, ctx, &request)?;
                    let reply = AgentOrdersDemoOnlySetting {
                        agent_orders_demo_only: request.agent_orders_demo_only,
                    }
                    .encode_to_vec();
                    Ok((made, reply))
                },
            )
        });
    }
    {
        let reading = store.clone();
        bus.serve(READ_AGENT_ORDERS_DEMO_ONLY, move |envelope| {
            let _: ReadAgentOrdersDemoOnlyRequest =
                decode(&envelope, "meridian.v1.ReadAgentOrdersDemoOnlyRequest")?;
            Ok((
                "meridian.v1.AgentOrdersDemoOnlySetting".to_string(),
                AgentOrdersDemoOnlySetting {
                    agent_orders_demo_only: agent_orders_demo_only(reading.as_ref())
                        .unwrap_or(true),
                }
                .encode_to_vec(),
            ))
        });
    }

    let (reading, asking) = (store.clone(), bus.clone());
    bus.serve(READ_LOCAL_RECORD_ORDERS, move |envelope| {
        let _: ReadLocalRecordOrdersRequest =
            decode(&envelope, "meridian.v1.ReadLocalRecordOrdersRequest")?;
        Ok((
            "meridian.v1.LocalRecordOrdersSetting".to_string(),
            local_record_orders_setting(&asking, reading.as_ref()).encode_to_vec(),
        ))
    });

    // W9.10 (contract v25; ruling 155): the book's health per account, for
    // a deployment admin through the dashboard, paged by account.
    let reading = store.clone();
    bus.serve(READ_BOOK_HEALTH, move |envelope| {
        let request: meridian_domain::v1::ReadBookHealthRequest =
            decode(&envelope, "meridian.v1.ReadBookHealthRequest")?;
        let limit = match request.page_size {
            n if n <= 0 => 100,
            n => (n as usize).min(1_000),
        };
        let only = (!request.account_id.is_empty()).then_some(request.account_id.as_str());
        let mut accounts = reading
            .book_health(only, &request.cursor, limit + 1)
            .map_err(|failed| failed.on_the_bus())?;
        let next_cursor = if accounts.len() > limit {
            accounts.truncate(limit);
            accounts
                .last()
                .map(|last| last.account_id.clone())
                .unwrap_or_default()
        } else {
            String::new()
        };
        let reply = meridian_domain::v1::ReadBookHealthReply {
            accounts,
            next_cursor,
        };
        Ok((
            "meridian.v1.ReadBookHealthReply".to_string(),
            reply.encode_to_vec(),
        ))
    });

    let reading = store.clone();
    bus.serve(LIST_BOOKINGS, move |envelope| {
        let request: ListBookingsRequest = decode(&envelope, "meridian.v1.ListBookingsRequest")?;
        let page = reading
            .bookings(&BookingsRead {
                scope: scope_of(&envelope),
                account_id: request.account_id.clone(),
                since: mark_of(request.since.as_ref()),
                state: request.state,
                limit: page_limit(
                    request.page_size,
                    bounds::LIST_BOOKINGS_REQUEST_PAGE_SIZE_RANGE,
                ),
                cursor: request.cursor.clone(),
            })
            .map_err(|failed| failed.on_the_bus())?;
        let mut reply = ListBookingsReply {
            bookings: page.records,
            as_of: Some(watermark_of(&page.as_of)),
            next_cursor: page.next_cursor,
        };
        meridian_domain::money::fill_known(&mut reply);
        Ok((
            "meridian.v1.ListBookingsReply".to_string(),
            reply.encode_to_vec(),
        ))
    });

    let reading = store;
    bus.serve(LIST_ALERTS, move |envelope| {
        let request: ListAlertsRequest = decode(&envelope, "meridian.v1.ListAlertsRequest")?;
        let page = reading
            .alerts(&AlertsRead {
                scope: scope_of(&envelope),
                account_id: request.account_id.clone(),
                since: mark_of(request.since.as_ref()),
                state: request.state,
                kind: request.kind,
                limit: page_limit(
                    request.page_size,
                    bounds::LIST_ALERTS_REQUEST_PAGE_SIZE_RANGE,
                ),
                cursor: request.cursor.clone(),
            })
            .map_err(|failed| failed.on_the_bus())?;
        Ok((
            "meridian.v1.ListAlertsReply".to_string(),
            ListAlertsReply {
                alerts: page.records,
                as_of: Some(watermark_of(&page.as_of)),
                next_cursor: page.next_cursor,
            }
            .encode_to_vec(),
        ))
    });
}

/// A block's members, where the order is one (W11.1).
/// The order a booking books, from its account's journal.
fn booking_order(store: &dyn Store, account: &str, booking_id: &str) -> String {
    let Ok(book) = store.book(account) else {
        return String::new();
    };
    book.ledger
        .bookings
        .get(booking_id)
        .map(|held| held.booking.order_id.clone())
        .unwrap_or_default()
}

fn block_members(store: &dyn Store, account: &str, order_id: &str) -> Vec<String> {
    let Ok(book) = store.book(account) else {
        return Vec::new();
    };
    book.ledger
        .orders
        .get(order_id)
        .map(|held| held.order.block_member_order_ids.clone())
        .unwrap_or_default()
}
