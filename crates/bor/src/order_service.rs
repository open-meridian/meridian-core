//! Where the order path meets the bus (W11, contract v22).
//!
//! Fourteen commands and facts in (from contract v23 the accept, the DK and
//! a value-date roll besides), four reads, six events out beside W9's
//! PositionChanged and AccountAttributeChanged; and two acts of the book's
//! own: expiring an order by the deployment's clock (W11.13), and, once at
//! the upgrade, recording that every account it held waits for compliance
//! (W9.13, decisions/031).
//!
//! **Which account a command is in.** A proposal, a recorded order, an
//! execution and a locate name it (an execution's and a recorded order's and
//! a locate's stamped from the venue account map); every other command names
//! its record by ID alone, and the book finds the record's account and holds
//! it to the write scope the sidecar stamped (W4.9). A deployment admin's
//! trading instruction on one account is in that account's partition; every
//! other is in the control partition (W11.15). A venue's fact naming no
//! account the book can place it in is kept unplaced, naming none (W11.13).

use std::sync::Arc;
use std::time::Duration;

use meridian_bus::{Bus, Envelope};
use meridian_domain::v1::{
    unplaced_fact, AcceptExecutionReply, AcceptExecutionRequest, AcknowledgeFlagRequest,
    CreatePlacementRequest, DecideApprovalRequest, DontKnowExecutionReply,
    DontKnowExecutionRequest, ExecutionRecordedEvent, FactReply, ListLocatesReply,
    ListLocatesRequest, ListOrdersReply, ListOrdersRequest, ListTradingInstructionsReply,
    ListTradingInstructionsRequest, LocateReply, LocateUpdatedEvent, OrderHistoryReply,
    OrderHistoryRequest, OrderReply, OrderUpdatedEvent, PlacementReply, PlacementUpdatedEvent,
    ProposeOrderRequest, RecordCheckRequest, RecordLocateRequest, RecordLocateUseRequest,
    RecordOrderRequest, ReleaseOrderRequest, ReportChargeRequest, ReportExecutionRequest,
    ReportPlacementRequest, ReportValueDateRollRequest, RequestOrderChangeRequest,
    RequestPlacementChangeRequest, SetTradingInstructionRequest, SetWorkingConditionRequest,
    TradingInstructionChangedEvent, TradingInstructionReply, UnplacedFactRecordedEvent,
    UnplacedReason,
};
use meridian_pb::bounds;
use prost::Message;

use crate::decide::{Context, Made};
use crate::order_decide as orders;
use crate::service::{command_store, context_of, decode, references, scope_of, Clock, Deciding};
use crate::store::{
    mark_of, page_limit, watermark_of, Decided, HistoryRead, InstructionsRead, LocatesRead,
    OrdersRead, Record, Store, StoreError, CONTROL_ACCOUNT, CONTROL_PARTITION, NO_ACCOUNT,
};

pub const PROPOSE_ORDER: &str = "platform.book.command.propose-order";
pub const RECORD_ORDER: &str = "platform.book.command.record-order";
pub const REQUEST_ORDER_CHANGE: &str = "platform.book.command.request-order-change";
pub const RECORD_CHECK: &str = "platform.book.command.record-check";
pub const DECIDE_APPROVAL: &str = "platform.book.command.decide-approval";
pub const RELEASE_ORDER: &str = "platform.book.command.release-order";
pub const SET_WORKING_CONDITION: &str = "platform.book.command.set-working-condition";
pub const CREATE_PLACEMENT: &str = "platform.book.command.create-placement";
pub const REQUEST_PLACEMENT_CHANGE: &str = "platform.book.command.request-placement-change";
pub const REPORT_PLACEMENT: &str = "platform.book.command.report-placement";
pub const REPORT_EXECUTION: &str = "platform.book.command.report-execution";
pub const REPORT_CHARGE: &str = "platform.book.command.report-charge";
pub const ACKNOWLEDGE_FLAG: &str = "platform.book.command.acknowledge-flag";
pub const SET_TRADING_INSTRUCTION: &str = "platform.book.command.set-trading-instruction";
pub const RECORD_LOCATE: &str = "platform.book.command.record-locate";
pub const RECORD_LOCATE_USE: &str = "platform.book.command.record-locate-use";
pub const ACCEPT_EXECUTION: &str = "platform.book.command.accept-execution";
pub const DONT_KNOW_EXECUTION: &str = "platform.book.command.dont-know-execution";
pub const REPORT_VALUE_DATE_ROLL: &str = "platform.book.command.report-value-date-roll";

pub const ORDER_UPDATED: &str = "platform.book.event.order-updated";
pub const PLACEMENT_UPDATED: &str = "platform.book.event.placement-updated";
pub const EXECUTION_RECORDED: &str = "platform.book.event.execution-recorded";
pub const UNPLACED_FACT_RECORDED: &str = "platform.book.event.unplaced-fact-recorded";
pub const TRADING_INSTRUCTION_CHANGED: &str = "platform.book.event.trading-instruction-changed";
pub const LOCATE_UPDATED: &str = "platform.book.event.locate-updated";

pub const LIST_ORDERS: &str = "platform.book.query.list-orders";
pub const ORDER_HISTORY: &str = "platform.book.query.order-history";
pub const LIST_TRADING_INSTRUCTIONS: &str = "platform.book.query.list-trading-instructions";
pub const LIST_LOCATES: &str = "platform.book.query.list-locates";

/// How often orders whose time in force ended are looked for.
pub const EXPIRE_EVERY: Duration = Duration::from_secs(15);

/// Every order-path record an entry changed, whole, in the order it
/// numbered them (W11.13).
pub fn announce(say: &dyn Fn(&str, &str, Vec<u8>), decided: &Decided) {
    let entry = &decided.entry;
    let changes = &decided.changes;
    for (order, changed) in &changes.orders {
        let mut event = OrderUpdatedEvent {
            order: Some(order.clone()),
            changed: changed.clone(),
            entry: Some(entry.meta.clone()),
            journal: order.last_change.clone(),
            cause: Some(entry.cause.clone()),
        };
        meridian_domain::money::fill_known(&mut event);
        say(
            ORDER_UPDATED,
            "meridian.v1.OrderUpdatedEvent",
            event.encode_to_vec(),
        );
    }
    for (placement, changed) in &changes.placements {
        let mut event = PlacementUpdatedEvent {
            placement: Some(placement.clone()),
            changed: changed.clone(),
            entry: Some(entry.meta.clone()),
            journal: placement.last_change.clone(),
            cause: Some(entry.cause.clone()),
        };
        meridian_domain::money::fill_known(&mut event);
        say(
            PLACEMENT_UPDATED,
            "meridian.v1.PlacementUpdatedEvent",
            event.encode_to_vec(),
        );
    }
    for execution in &changes.executions {
        let mut event = ExecutionRecordedEvent {
            execution: Some(execution.clone()),
            entry: Some(entry.meta.clone()),
            journal: execution.last_change.clone(),
            cause: Some(entry.cause.clone()),
        };
        meridian_domain::money::fill_known(&mut event);
        say(
            EXECUTION_RECORDED,
            "meridian.v1.ExecutionRecordedEvent",
            event.encode_to_vec(),
        );
    }
    for unplaced in &changes.unplaced {
        say(
            UNPLACED_FACT_RECORDED,
            "meridian.v1.UnplacedFactRecordedEvent",
            UnplacedFactRecordedEvent {
                unplaced: Some(unplaced.clone()),
                entry: Some(entry.meta.clone()),
                journal: unplaced.last_change.clone(),
                cause: Some(entry.cause.clone()),
            }
            .encode_to_vec(),
        );
    }
    for locate in &changes.locates {
        say(
            LOCATE_UPDATED,
            "meridian.v1.LocateUpdatedEvent",
            LocateUpdatedEvent {
                locate: Some(locate.clone()),
                entry: Some(entry.meta.clone()),
                journal: locate.last_change.clone(),
                cause: Some(entry.cause.clone()),
            }
            .encode_to_vec(),
        );
    }
    // Contract v24: each booking and alert changed (W11.30, W11.35).
    crate::booking_service::announce(say, decided);
    for instruction in &changes.instructions {
        say(
            TRADING_INSTRUCTION_CHANGED,
            "meridian.v1.TradingInstructionChangedEvent",
            TradingInstructionChangedEvent {
                instruction: Some(instruction.clone()),
                journal: instruction.last_change.clone(),
                cause: Some(entry.cause.clone()),
            }
            .encode_to_vec(),
        );
    }
}

fn order_reply(made: &Made, order_id: &str, change_request_id: &str) -> Vec<u8> {
    let order = made
        .changes
        .orders
        .iter()
        .find(|(order, _)| order.order_id == order_id)
        .map(|(order, _)| order.clone())
        .or_else(|| made.book.ledger.order_record(order_id, false).ok());
    let mut reply = OrderReply {
        journal: order.as_ref().and_then(|order| order.last_change.clone()),
        order,
        entry: Some(made.entry.meta.clone()),
        change_request_id: change_request_id.to_string(),
    };
    meridian_domain::money::fill_known(&mut reply);
    reply.encode_to_vec()
}

fn placement_reply(made: &Made, placement_id: &str, change_request_id: &str) -> Vec<u8> {
    if placement_id.is_empty() {
        return PlacementReply {
            entry: Some(made.entry.meta.clone()),
            journal: Some(made.changes.first.clone()),
            ..Default::default()
        }
        .encode_to_vec();
    }
    let placement = made
        .changes
        .placements
        .iter()
        .find(|(placement, _)| placement.placement_id == placement_id)
        .map(|(placement, _)| placement.clone())
        .or_else(|| made.book.ledger.placement_record(placement_id).ok());
    let mut reply = PlacementReply {
        journal: placement
            .as_ref()
            .and_then(|placement| placement.last_change.clone()),
        placement,
        entry: Some(made.entry.meta.clone()),
        change_request_id: change_request_id.to_string(),
    };
    meridian_domain::money::fill_known(&mut reply);
    reply.encode_to_vec()
}

fn fact_reply(made: &Made) -> Vec<u8> {
    let changes = &made.changes;
    let mut reply = FactReply {
        placement: changes
            .placements
            .first()
            .map(|(placement, _)| placement.clone()),
        execution: changes.executions.first().cloned(),
        order: changes.orders.first().map(|(order, _)| order.clone()),
        charge: made.entry.body.acts.iter().find_map(|act| match act {
            crate::orders::Act::Charged { charge } => Some(charge.clone()),
            _ => None,
        }),
        unplaced: changes.unplaced.first().cloned(),
        entry: Some(made.entry.meta.clone()),
        journal: Some(changes.first.clone()),
    };
    meridian_domain::money::fill_known(&mut reply);
    reply.encode_to_vec()
}

/// The executions a command named, as they stand after it, in its order.
fn executions_of(made: &Made, ids: &[String]) -> Vec<meridian_domain::v1::Execution> {
    ids.iter()
        .filter_map(|id| made.book.ledger.executions.get(id))
        .map(|held| held.execution.clone())
        .collect()
}

/// An accept's answer (W11.23): each execution matched, the order and the
/// placement it linked them to.
fn accept_reply(made: &Made, ids: &[String]) -> Vec<u8> {
    let (order_id, placement_id) = made
        .entry
        .body
        .acts
        .iter()
        .find_map(|act| match act {
            crate::orders::Act::Accepted {
                order_id,
                placement_id,
                ..
            } => Some((order_id.clone(), placement_id.clone())),
            _ => None,
        })
        .unwrap_or_default();
    let mut reply = AcceptExecutionReply {
        executions: executions_of(made, ids),
        order: made.book.ledger.order_record(&order_id, false).ok(),
        placement: made.book.ledger.placement_record(&placement_id).ok(),
        journal: Some(made.changes.first.clone()),
    };
    meridian_domain::money::fill_known(&mut reply);
    reply.encode_to_vec()
}

fn locate_reply(made: &Made) -> Vec<u8> {
    LocateReply {
        locate: made.changes.locates.first().cloned(),
        entry: Some(made.entry.meta.clone()),
        journal: made
            .changes
            .locates
            .first()
            .and_then(|locate| locate.last_change.clone()),
    }
    .encode_to_vec()
}

/// The order a command names, found in its account, or the words saying the
/// book holds none.
fn owner_of(store: &dyn Store, record: Record, id: &str) -> Result<String, StoreError> {
    if id.is_empty() {
        return Err(StoreError::Invalid("the command names no record".into()));
    }
    store.owner(record, id)?.ok_or_else(|| {
        StoreError::Invalid(format!("the book holds no {} {id}", record_named(record)))
    })
}

fn record_named(record: Record) -> &'static str {
    match record {
        Record::Order => "order",
        Record::Placement => "placement",
        Record::ChangeRequest => "change request",
        Record::Locate => "locate",
        Record::Execution => "execution",
        Record::Booking => "booking",
        Record::Alert => "alert",
    }
}

/// One command, decided in `account_id`'s book; a fact the book reads to
/// keep unplaced, kept so in the book of no account.
#[allow(clippy::too_many_arguments)]
fn act(
    bus: &Bus,
    store: &dyn Store,
    envelope: &Envelope,
    ctx: Context,
    account_id: &str,
    idempotency_key: &str,
    reply_type: &str,
    decide: &Deciding<'_>,
    unplace: Option<(
        unplaced_fact::Fact,
        String,
        Option<meridian_pb::v1::RawRecordRef>,
    )>,
) -> Result<(String, Vec<u8>), String> {
    let first = command_store(
        bus,
        store,
        envelope,
        ctx.clone(),
        account_id,
        idempotency_key,
        reply_type,
        decide,
    );
    match (first, unplace) {
        (Err(StoreError::Unplace { reason, words }), Some((fact, code, raw))) => {
            tracing::info!(words, "a venue's fact is kept unplaced, naming no account");
            let reason = UnplacedReason::try_from(reason).unwrap_or(UnplacedReason::Unspecified);
            command_store(
                bus,
                store,
                envelope,
                ctx,
                NO_ACCOUNT,
                "",
                "meridian.v1.FactReply",
                &|book, head, ctx| {
                    let made = orders::unplaced(
                        book,
                        head,
                        ctx,
                        fact.clone(),
                        reason,
                        &code,
                        raw.clone(),
                    )?;
                    let reply = fact_reply(&made);
                    Ok((made, reply))
                },
            )
            .map_err(|failed| failed.on_the_bus())
        }
        (outcome, _) => outcome.map_err(|failed| failed.on_the_bus()),
    }
}

/// A fact naming no account the book holds: kept unplaced, naming none.
#[allow(clippy::too_many_arguments)]
fn no_account(
    bus: &Bus,
    store: &dyn Store,
    envelope: &Envelope,
    ctx: Context,
    fact: unplaced_fact::Fact,
    reason: UnplacedReason,
    code: &str,
    raw: Option<meridian_pb::v1::RawRecordRef>,
) -> Result<(String, Vec<u8>), String> {
    // Answered in the row's own reply: a locate's a LocateReply, naming the
    // entry that kept it and no record.
    let reply_type = match &fact {
        unplaced_fact::Fact::Locate(_) => "meridian.v1.LocateReply",
        _ => "meridian.v1.FactReply",
    };
    command_store(
        bus,
        store,
        envelope,
        ctx,
        NO_ACCOUNT,
        "",
        reply_type,
        &|book, head, ctx| {
            let made = orders::unplaced(book, head, ctx, fact.clone(), reason, code, raw.clone())?;
            let reply = kept_reply(&made, reply_type);
            Ok((made, reply))
        },
    )
    .map_err(|failed| failed.on_the_bus())
}

/// An unplaced fact's answer in its row's reply type.
fn kept_reply(made: &Made, reply_type: &str) -> Vec<u8> {
    let entry = Some(made.entry.meta.clone());
    let journal = Some(made.changes.first.clone());
    match reply_type {
        "meridian.v1.OrderReply" => OrderReply {
            entry,
            journal,
            ..Default::default()
        }
        .encode_to_vec(),
        "meridian.v1.LocateReply" => LocateReply {
            entry,
            journal,
            ..Default::default()
        }
        .encode_to_vec(),
        _ => fact_reply(made),
    }
}

/// Register every order-path handler the book serves.
pub fn serve(bus: Arc<Bus>, store: Arc<dyn Store>, clock: Arc<dyn Clock>) {
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(PROPOSE_ORDER, move |envelope| {
            let request: ProposeOrderRequest =
                decode(&envelope, "meridian.v1.ProposeOrderRequest")?;
            crate::cash::ensure_blocking(
                &bus2,
                &store,
                &meridian_domain::money::codes(&request),
                clock.now_ns(),
            );
            let mut ctx = context_of(&envelope, clock.as_ref());
            let found = references(
                &bus2,
                std::slice::from_ref(&request.instrument_id),
                ctx.received_at_ns,
            );
            ctx.reference_versions = found.versions;
            ctx.records = found.records;
            ctx.reference_unavailable = found.unavailable;
            // Contract v24: the escape hatch's setting (W11.36), and a
            // block's members read from their own accounts (W11.1).
            ctx.local_record_orders = crate::booking_service::local_record_orders(store.as_ref());
            if !request.block_member_order_ids.is_empty() {
                ctx.members = crate::booking_service::member_views(
                    store.as_ref(),
                    &request.block_member_order_ids,
                );
            }
            if request.account_id.is_empty() {
                return Err(StoreError::Malformed {
                    fields: vec!["account_id".into()],
                    words: "an order names its account".into(),
                }
                .on_the_bus());
            }
            // A child order is in its parent's account (Q6).
            if let Some(link) = request
                .link
                .as_ref()
                .filter(|link| !link.parent_order_id.is_empty())
            {
                if let Ok(Some(parent_account)) = store.owner(Record::Order, &link.parent_order_id)
                {
                    if parent_account != request.account_id {
                        return Err(StoreError::Malformed {
                            fields: vec!["account_id".into()],
                            words: format!(
                                "a child order is in its parent's account, {parent_account}"
                            ),
                        }
                        .on_the_bus());
                    }
                }
            }
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &request.account_id,
                &request.idempotency_key,
                "meridian.v1.OrderReply",
                &|book, head, ctx| {
                    let made = orders::propose_order(book, head, ctx, &request)?;
                    let order_id = made
                        .entry
                        .body
                        .acts
                        .iter()
                        .find_map(|act| match act {
                            crate::orders::Act::Opened { order, .. } => {
                                Some(order.order_id.clone())
                            }
                            _ => None,
                        })
                        .unwrap_or_default();
                    let reply = order_reply(&made, &order_id, "");
                    Ok((made, reply))
                },
                None,
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(RECORD_ORDER, move |envelope| {
            let request: RecordOrderRequest = decode(&envelope, "meridian.v1.RecordOrderRequest")?;
            crate::cash::ensure_blocking(
                &bus2,
                &store,
                &meridian_domain::money::codes(&request),
                clock.now_ns(),
            );
            let mut ctx = context_of(&envelope, clock.as_ref());
            // The oms's command for a user (W11.2): a connectivity plugin
            // records no order (ruled 2026-10-13), so none is kept unplaced.
            if request.account_id.is_empty() {
                return Err(StoreError::Malformed {
                    fields: vec!["account_id".into()],
                    words: "a recorded order names its account".into(),
                }
                .on_the_bus());
            }
            let found = references(
                &bus2,
                std::slice::from_ref(&request.instrument_id),
                ctx.received_at_ns,
            );
            ctx.reference_versions = found.versions;
            ctx.records = found.records;
            ctx.reference_unavailable = found.unavailable;
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &request.account_id,
                &request.idempotency_key,
                "meridian.v1.OrderReply",
                &|book, head, ctx| {
                    let made = orders::record_order(book, head, ctx, &request)?;
                    let order_id = made.entry.body.acts.iter().find_map(|act| match act {
                        crate::orders::Act::Opened { order, .. } => Some(order.order_id.clone()),
                        _ => None,
                    });
                    let reply = match order_id {
                        Some(order_id) => order_reply(&made, &order_id, ""),
                        None => kept_reply(&made, "meridian.v1.OrderReply"),
                    };
                    Ok((made, reply))
                },
                None,
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(REQUEST_ORDER_CHANGE, move |envelope| {
            let request: RequestOrderChangeRequest =
                decode(&envelope, "meridian.v1.RequestOrderChangeRequest")?;
            crate::cash::ensure_blocking(
                &bus2,
                &store,
                &meridian_domain::money::codes(&request),
                clock.now_ns(),
            );
            let ctx = context_of(&envelope, clock.as_ref());
            let account = owner_of(store.as_ref(), Record::Order, &request.order_id)
                .map_err(|f| f.on_the_bus())?;
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &account,
                &request.idempotency_key,
                "meridian.v1.OrderReply",
                &|book, head, ctx| {
                    let (made, crq) = orders::request_order_change(book, head, ctx, &request)?;
                    let reply = order_reply(&made, &request.order_id, &crq);
                    Ok((made, reply))
                },
                None,
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(RECORD_CHECK, move |envelope| {
            let request: RecordCheckRequest = decode(&envelope, "meridian.v1.RecordCheckRequest")?;
            let ctx = context_of(&envelope, clock.as_ref());
            let account = if request.placement_id.is_empty() {
                owner_of(store.as_ref(), Record::Order, &request.order_id)
            } else {
                owner_of(store.as_ref(), Record::Placement, &request.placement_id)
            }
            .map_err(|f| f.on_the_bus())?;
            let on_placement = !request.placement_id.is_empty();
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &account,
                &request.idempotency_key,
                if on_placement {
                    "meridian.v1.PlacementReply"
                } else {
                    "meridian.v1.OrderReply"
                },
                &|book, head, ctx| {
                    let made = orders::record_check(book, head, ctx, &request)?;
                    let reply = if on_placement {
                        placement_reply(&made, &request.placement_id, "")
                    } else {
                        order_reply(&made, &request.order_id, "")
                    };
                    Ok((made, reply))
                },
                None,
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(DECIDE_APPROVAL, move |envelope| {
            let request: DecideApprovalRequest =
                decode(&envelope, "meridian.v1.DecideApprovalRequest")?;
            let ctx = context_of(&envelope, clock.as_ref());
            let account = owner_of(store.as_ref(), Record::Order, &request.order_id)
                .map_err(|f| f.on_the_bus())?;
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &account,
                &request.idempotency_key,
                "meridian.v1.OrderReply",
                &|book, head, ctx| {
                    let made = orders::decide_approval(book, head, ctx, &request)?;
                    let reply = order_reply(&made, &request.order_id, "");
                    Ok((made, reply))
                },
                None,
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(RELEASE_ORDER, move |envelope| {
            let request: ReleaseOrderRequest =
                decode(&envelope, "meridian.v1.ReleaseOrderRequest")?;
            let ctx = context_of(&envelope, clock.as_ref());
            let account = owner_of(store.as_ref(), Record::Order, &request.order_id)
                .map_err(|f| f.on_the_bus())?;
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &account,
                &request.idempotency_key,
                "meridian.v1.OrderReply",
                &|book, head, ctx| {
                    let made = orders::release_order(book, head, ctx, &request)?;
                    let reply = order_reply(&made, &request.order_id, "");
                    Ok((made, reply))
                },
                None,
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(SET_WORKING_CONDITION, move |envelope| {
            let request: SetWorkingConditionRequest =
                decode(&envelope, "meridian.v1.SetWorkingConditionRequest")?;
            let ctx = context_of(&envelope, clock.as_ref());
            let account = owner_of(store.as_ref(), Record::Order, &request.order_id)
                .map_err(|f| f.on_the_bus())?;
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &account,
                &request.idempotency_key,
                "meridian.v1.OrderReply",
                &|book, head, ctx| {
                    let made = orders::set_working_condition(book, head, ctx, &request)?;
                    let reply = order_reply(&made, &request.order_id, "");
                    Ok((made, reply))
                },
                None,
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(CREATE_PLACEMENT, move |envelope| {
            let request: CreatePlacementRequest =
                decode(&envelope, "meridian.v1.CreatePlacementRequest")?;
            crate::cash::ensure_blocking(
                &bus2,
                &store,
                &meridian_domain::money::codes(&request),
                clock.now_ns(),
            );
            let ctx = context_of(&envelope, clock.as_ref());
            let account = owner_of(store.as_ref(), Record::Order, &request.order_id)
                .map_err(|f| f.on_the_bus())?;
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &account,
                &request.idempotency_key,
                "meridian.v1.PlacementReply",
                &|book, head, ctx| {
                    let made = orders::create_placement(book, head, ctx, &request)?;
                    let placement_id = made
                        .entry
                        .body
                        .acts
                        .iter()
                        .find_map(|act| match act {
                            crate::orders::Act::PlacementOpened { placement } => {
                                Some(placement.placement_id.clone())
                            }
                            _ => None,
                        })
                        .unwrap_or_default();
                    let reply = placement_reply(&made, &placement_id, "");
                    Ok((made, reply))
                },
                None,
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(REQUEST_PLACEMENT_CHANGE, move |envelope| {
            let request: RequestPlacementChangeRequest =
                decode(&envelope, "meridian.v1.RequestPlacementChangeRequest")?;
            crate::cash::ensure_blocking(
                &bus2,
                &store,
                &meridian_domain::money::codes(&request),
                clock.now_ns(),
            );
            let ctx = context_of(&envelope, clock.as_ref());
            let account = owner_of(store.as_ref(), Record::Placement, &request.placement_id)
                .map_err(|f| f.on_the_bus())?;
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &account,
                &request.idempotency_key,
                "meridian.v1.PlacementReply",
                &|book, head, ctx| {
                    let (made, crq) = orders::request_placement_change(book, head, ctx, &request)?;
                    let reply = placement_reply(&made, &request.placement_id, &crq);
                    Ok((made, reply))
                },
                None,
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(REPORT_PLACEMENT, move |envelope| {
            let request: ReportPlacementRequest =
                decode(&envelope, "meridian.v1.ReportPlacementRequest")?;
            let ctx = context_of(&envelope, clock.as_ref());
            let fact = unplaced_fact::Fact::Placement(request.clone());
            let account = match store
                .owner(Record::Placement, &request.placement_id)
                .map_err(|f| f.on_the_bus())?
            {
                Some(account) => account,
                None => {
                    return no_account(
                        &bus2,
                        store.as_ref(),
                        &envelope,
                        ctx,
                        fact,
                        UnplacedReason::UnknownPlacement,
                        "",
                        request.raw_record.clone(),
                    )
                }
            };
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &account,
                "",
                "meridian.v1.FactReply",
                &|book, head, ctx| {
                    let made = orders::report_placement(book, head, ctx, &request)?;
                    let reply = fact_reply(&made);
                    Ok((made, reply))
                },
                Some((fact.clone(), String::new(), request.raw_record.clone())),
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(REPORT_EXECUTION, move |envelope| {
            let request: ReportExecutionRequest =
                decode(&envelope, "meridian.v1.ReportExecutionRequest")?;
            crate::cash::ensure_blocking(
                &bus2,
                &store,
                &meridian_domain::money::codes(&request),
                clock.now_ns(),
            );
            let ctx = context_of(&envelope, clock.as_ref());
            let fact = unplaced_fact::Fact::Execution(request.clone());
            if request.account_id.is_empty() {
                if !ctx.acting_for.is_empty() {
                    return Err(StoreError::Malformed {
                        fields: vec!["account_id".into()],
                        words: "an entered execution names its account".into(),
                    }
                    .on_the_bus());
                }
                // Held to the same shape as a placed fact before it is kept.
                if request.settlement_date.is_empty()
                    && request.computed_settlement_date.is_empty()
                    && request.supersedes.is_empty()
                {
                    return Err(StoreError::Malformed {
                        fields: vec!["settlement_date".into()],
                        words: "a fill states its settlement date, or the date read's beside it"
                            .into(),
                    }
                    .on_the_bus());
                }
                return no_account(
                    &bus2,
                    store.as_ref(),
                    &envelope,
                    ctx,
                    fact,
                    UnplacedReason::UnmappedVenueAccount,
                    &request.venue_account_code,
                    request.raw_record.clone(),
                );
            }
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &request.account_id,
                &request.idempotency_key,
                "meridian.v1.FactReply",
                &|book, head, ctx| {
                    let made = orders::report_execution(book, head, ctx, &request)?;
                    let reply = fact_reply(&made);
                    Ok((made, reply))
                },
                Some((
                    fact.clone(),
                    request.venue_account_code.clone(),
                    request.raw_record.clone(),
                )),
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(REPORT_CHARGE, move |envelope| {
            let request: ReportChargeRequest =
                decode(&envelope, "meridian.v1.ReportChargeRequest")?;
            crate::cash::ensure_blocking(
                &bus2,
                &store,
                &meridian_domain::money::codes(&request),
                clock.now_ns(),
            );
            let ctx = context_of(&envelope, clock.as_ref());
            let fact = unplaced_fact::Fact::Charge(request.clone());
            let account = match store
                .owner(Record::Placement, &request.placement_id)
                .map_err(|f| f.on_the_bus())?
            {
                Some(account) => account,
                None => {
                    return no_account(
                        &bus2,
                        store.as_ref(),
                        &envelope,
                        ctx,
                        fact,
                        UnplacedReason::UnknownPlacement,
                        "",
                        request.raw_record.clone(),
                    )
                }
            };
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &account,
                "",
                "meridian.v1.FactReply",
                &|book, head, ctx| {
                    let made = orders::report_charge(book, head, ctx, &request)?;
                    let reply = fact_reply(&made);
                    Ok((made, reply))
                },
                Some((fact.clone(), String::new(), request.raw_record.clone())),
            )
        });
    }
    {
        // W11.25 (contract v23): a venue's roll of a spot position, its
        // account stamped from the venue account map.
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(REPORT_VALUE_DATE_ROLL, move |envelope| {
            let request: ReportValueDateRollRequest =
                decode(&envelope, "meridian.v1.ReportValueDateRollRequest")?;
            let mut ctx = context_of(&envelope, clock.as_ref());
            // The pair's records, so a pair that is not two cash
            // instruments is refused (approved 2026-10-13).
            let found = references(&bus2, &request.pair, ctx.received_at_ns);
            ctx.reference_versions = found.versions;
            ctx.records = found.records;
            ctx.reference_unavailable = found.unavailable;
            if request.account_id.is_empty() {
                // Held to its shape, then kept unplaced naming no account,
                // UNPLACED_REASON_UNMAPPED_VENUE_ACCOUNT, as an execution on
                // the code is (W11.25; approved 2026-10-13).
                orders::roll_holds(&ctx, &request).map_err(|f| f.on_the_bus())?;
                return no_account(
                    &bus2,
                    store.as_ref(),
                    &envelope,
                    ctx,
                    unplaced_fact::Fact::ValueDateRoll(request.clone()),
                    UnplacedReason::UnmappedVenueAccount,
                    &request.venue_account_code,
                    request.raw_record.clone(),
                );
            }
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &request.account_id,
                &request.idempotency_key,
                "meridian.v1.FactReply",
                &|book, head, ctx| {
                    let made = orders::report_value_date_roll(book, head, ctx, &request)?;
                    let reply = fact_reply(&made);
                    Ok((made, reply))
                },
                None,
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(ACKNOWLEDGE_FLAG, move |envelope| {
            let request: AcknowledgeFlagRequest =
                decode(&envelope, "meridian.v1.AcknowledgeFlagRequest")?;
            let ctx = context_of(&envelope, clock.as_ref());
            let account = owner_of(store.as_ref(), Record::Order, &request.order_id)
                .map_err(|f| f.on_the_bus())?;
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &account,
                &request.idempotency_key,
                "meridian.v1.OrderReply",
                &|book, head, ctx| {
                    let made = orders::acknowledge_flag(book, head, ctx, &request)?;
                    let reply = order_reply(&made, &request.order_id, "");
                    Ok((made, reply))
                },
                None,
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(SET_TRADING_INSTRUCTION, move |envelope| {
            let request: SetTradingInstructionRequest =
                decode(&envelope, "meridian.v1.SetTradingInstructionRequest")?;
            let ctx = context_of(&envelope, clock.as_ref());
            // A deployment admin's on one account is in that account's
            // partition; every other in the control partition (W11.15).
            let target = request.target.clone().unwrap_or_default();
            let on_one_account = !target.account_id.is_empty() && target.instance_id.is_empty();
            let key = if on_one_account {
                target.account_id.clone()
            } else {
                CONTROL_ACCOUNT.to_string()
            };
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &key,
                "",
                "meridian.v1.TradingInstructionReply",
                &|book, head, ctx| {
                    let made = orders::set_trading_instruction(book, head, ctx, &request)?;
                    let instruction = made.changes.instructions.first().cloned();
                    let reply = TradingInstructionReply {
                        journal: instruction
                            .as_ref()
                            .and_then(|held| held.last_change.clone()),
                        instruction,
                    }
                    .encode_to_vec();
                    Ok((made, reply))
                },
                None,
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(RECORD_LOCATE, move |envelope| {
            let request: RecordLocateRequest =
                decode(&envelope, "meridian.v1.RecordLocateRequest")?;
            let ctx = context_of(&envelope, clock.as_ref());
            let account = if !request.locate_record_id.is_empty() {
                store
                    .owner(Record::Locate, &request.locate_record_id)
                    .map_err(|f| f.on_the_bus())?
                    .unwrap_or_else(|| request.account_id.clone())
            } else {
                request.account_id.clone()
            };
            if account.is_empty() {
                return no_account(
                    &bus2,
                    store.as_ref(),
                    &envelope,
                    ctx,
                    unplaced_fact::Fact::Locate(request.clone()),
                    UnplacedReason::UnmappedVenueAccount,
                    &request.venue_account_code,
                    request.raw_record.clone(),
                );
            }
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &account,
                &request.idempotency_key,
                "meridian.v1.LocateReply",
                &|book, head, ctx| {
                    let made = orders::record_locate(book, head, ctx, &request)?;
                    let reply = locate_reply(&made);
                    Ok((made, reply))
                },
                None,
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(RECORD_LOCATE_USE, move |envelope| {
            let request: RecordLocateUseRequest =
                decode(&envelope, "meridian.v1.RecordLocateUseRequest")?;
            let ctx = context_of(&envelope, clock.as_ref());
            let account = owner_of(store.as_ref(), Record::Locate, &request.locate_record_id)
                .map_err(|f| f.on_the_bus())?;
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &account,
                &request.idempotency_key,
                "meridian.v1.LocateReply",
                &|book, head, ctx| {
                    let made = orders::record_locate_use(book, head, ctx, &request)?;
                    let reply = locate_reply(&made);
                    Ok((made, reply))
                },
                None,
            )
        });
    }

    // W11.23 and W11.24 (contract v23): the `ems`'s answers to unmatched
    // executions, named by their IDs alone, decided in the first one's
    // account and held to the write scope the sidecar stamped.
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(ACCEPT_EXECUTION, move |envelope| {
            let request: AcceptExecutionRequest =
                decode(&envelope, "meridian.v1.AcceptExecutionRequest")?;
            let mut ctx = context_of(&envelope, clock.as_ref());
            let account = owner_of(
                store.as_ref(),
                Record::Execution,
                request
                    .execution_ids
                    .first()
                    .map(String::as_str)
                    .unwrap_or_default(),
            )
            .map_err(|f| f.on_the_bus())?;
            // Where each execution named is, read before the account's lock
            // is taken: an execution in another account is never in this
            // account's book.
            let owners: std::collections::BTreeMap<String, String> = request
                .execution_ids
                .iter()
                .filter_map(|id| {
                    store
                        .owner(Record::Execution, id)
                        .ok()
                        .flatten()
                        .map(|account| (id.clone(), account))
                })
                .collect();
            let elsewhere = |id: &str| owners.get(id).cloned();
            // Contract v24 (row e3): the fills' instrument's record, read for
            // whether it is a local record, and the escape hatch's setting.
            let instrument = execution_instrument(
                store.as_ref(),
                &account,
                request
                    .execution_ids
                    .first()
                    .map(String::as_str)
                    .unwrap_or_default(),
            );
            if let Some(instrument) = instrument {
                let found = references(&bus2, &[instrument], ctx.received_at_ns);
                ctx.records = found.records;
            }
            ctx.local_record_orders = crate::booking_service::local_record_orders(store.as_ref());
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &account,
                "",
                "meridian.v1.AcceptExecutionReply",
                &|book, head, ctx| {
                    let made = orders::accept_execution(book, head, ctx, &request, &elsewhere)?;
                    let reply = accept_reply(&made, &request.execution_ids);
                    Ok((made, reply))
                },
                None,
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(DONT_KNOW_EXECUTION, move |envelope| {
            let request: DontKnowExecutionRequest =
                decode(&envelope, "meridian.v1.DontKnowExecutionRequest")?;
            let ctx = context_of(&envelope, clock.as_ref());
            let account = owner_of(
                store.as_ref(),
                Record::Execution,
                request
                    .execution_ids
                    .first()
                    .map(String::as_str)
                    .unwrap_or_default(),
            )
            .map_err(|f| f.on_the_bus())?;
            // Where each execution named is, read before the account's lock
            // is taken: an execution in another account is never in this
            // account's book.
            let owners: std::collections::BTreeMap<String, String> = request
                .execution_ids
                .iter()
                .filter_map(|id| {
                    store
                        .owner(Record::Execution, id)
                        .ok()
                        .flatten()
                        .map(|account| (id.clone(), account))
                })
                .collect();
            let elsewhere = |id: &str| owners.get(id).cloned();
            act(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &account,
                "",
                "meridian.v1.DontKnowExecutionReply",
                &|book, head, ctx| {
                    let made = orders::dont_know_execution(book, head, ctx, &request, &elsewhere)?;
                    let mut reply = DontKnowExecutionReply {
                        executions: executions_of(&made, &request.execution_ids),
                        journal: Some(made.changes.first.clone()),
                    };
                    meridian_domain::money::fill_known(&mut reply);
                    Ok((made, reply.encode_to_vec()))
                },
                None,
            )
        });
    }

    let reading = store.clone();
    bus.serve(LIST_ORDERS, move |envelope| {
        let request: ListOrdersRequest = decode(&envelope, "meridian.v1.ListOrdersRequest")?;
        let page = reading
            .orders(&OrdersRead {
                scope: scope_of(&envelope),
                account_id: request.account_id.clone(),
                since: mark_of(request.since.as_ref()),
                limit: page_limit(
                    request.page_size,
                    bounds::LIST_ORDERS_REQUEST_PAGE_SIZE_RANGE,
                ),
                cursor: request.cursor.clone(),
                // Contract v23 (W11.17): only the executions in the state
                // asked for, filtered where the page is made.
                execution_state: request.execution_state,
            })
            .map_err(|failed| failed.on_the_bus())?;
        let mut reply = ListOrdersReply {
            orders: page.orders,
            as_of: Some(watermark_of(&page.as_of)),
            next_cursor: page.next_cursor,
            placements: page.placements,
            executions: page.executions,
            unplaced_facts: page.unplaced,
        };
        meridian_domain::money::fill_known(&mut reply);
        Ok((
            "meridian.v1.ListOrdersReply".to_string(),
            reply.encode_to_vec(),
        ))
    });

    let reading = store.clone();
    bus.serve(ORDER_HISTORY, move |envelope| {
        let request: OrderHistoryRequest = decode(&envelope, "meridian.v1.OrderHistoryRequest")?;
        let page = reading
            .order_history(&HistoryRead {
                scope: scope_of(&envelope),
                account_id: request.account_id.clone(),
                order_id: request.order_id.clone(),
                limit: page_limit(
                    request.page_size,
                    bounds::ORDER_HISTORY_REQUEST_PAGE_SIZE_RANGE,
                ),
                cursor: request.cursor.clone(),
            })
            .map_err(|failed| failed.on_the_bus())?;
        Ok((
            "meridian.v1.OrderHistoryReply".to_string(),
            OrderHistoryReply {
                entries: page.records,
                next_cursor: page.next_cursor,
            }
            .encode_to_vec(),
        ))
    });

    let reading = store.clone();
    bus.serve(LIST_TRADING_INSTRUCTIONS, move |envelope| {
        let request: ListTradingInstructionsRequest =
            decode(&envelope, "meridian.v1.ListTradingInstructionsRequest")?;
        let page = reading
            .instructions(&InstructionsRead {
                since: mark_of(request.since.as_ref()),
                limit: page_limit(
                    request.page_size,
                    bounds::LIST_TRADING_INSTRUCTIONS_REQUEST_PAGE_SIZE_RANGE,
                ),
                cursor: request.cursor.clone(),
            })
            .map_err(|failed| failed.on_the_bus())?;
        // The control partition's head in the read's as_of: the invariant the
        // product owner kept (2026-10-12).
        let control_sequence = page.as_of.get(CONTROL_PARTITION).copied().unwrap_or(0);
        Ok((
            "meridian.v1.ListTradingInstructionsReply".to_string(),
            ListTradingInstructionsReply {
                instructions: page.records,
                control_sequence,
                as_of: Some(watermark_of(&page.as_of)),
                next_cursor: page.next_cursor,
            }
            .encode_to_vec(),
        ))
    });

    let reading = store;
    bus.serve(LIST_LOCATES, move |envelope| {
        let request: ListLocatesRequest = decode(&envelope, "meridian.v1.ListLocatesRequest")?;
        let page = reading
            .locates(&LocatesRead {
                scope: scope_of(&envelope),
                account_id: request.account_id.clone(),
                since: mark_of(request.since.as_ref()),
                limit: page_limit(
                    request.page_size,
                    bounds::LIST_LOCATES_REQUEST_PAGE_SIZE_RANGE,
                ),
                cursor: request.cursor.clone(),
            })
            .map_err(|failed| failed.on_the_bus())?;
        Ok((
            "meridian.v1.ListLocatesReply".to_string(),
            ListLocatesReply {
                locates: page.records,
                as_of: Some(watermark_of(&page.as_of)),
                next_cursor: page.next_cursor,
            }
            .encode_to_vec(),
        ))
    });
}

/// The instrument an execution is of, read from its account's journal
/// before the lock (contract v24, row e3).
fn execution_instrument(store: &dyn Store, account: &str, execution_id: &str) -> Option<String> {
    let entries = store.journal(account).ok()?;
    let book =
        crate::book::Book::replay(account, crate::store::first_partition_of(account), &entries)
            .ok()?;
    book.ledger
        .executions
        .get(execution_id)
        .map(|held| held.execution.instrument_id.clone())
}

/// The book's own expiry (W11.13, row i4): every order whose time in force
/// ended by the deployment's clock, each in its own entry, actor the
/// system. Says how many it acted on.
pub fn expire_due(bus: &Bus, store: &Arc<dyn Store>) -> Result<usize, String> {
    let now = bus.clock().now_ns();
    let due = store.expiring(now).map_err(|failed| failed.to_string())?;
    let mut acted = 0;
    for (account, order_id) in due {
        let ctx = Context {
            instance_id: bus.instance_id().to_string(),
            event_time_ns: now,
            received_at_ns: now,
            committed_at_ns: now,
            control_sequence: store
                .heads()
                .ok()
                .and_then(|heads| heads.get(CONTROL_PARTITION).copied())
                .unwrap_or(0),
            ..Default::default()
        };
        let outcome = store.act(
            &account,
            "",
            "",
            &[],
            &mut |book, head| match orders::expire(book, head, &ctx, &order_id)? {
                Some(made) => Ok(crate::service::decided(made, Vec::new())),
                None => Err(StoreError::Invalid(NOTHING_DUE.into())),
            },
        );
        match outcome {
            Ok(crate::store::Acted::Committed(decided)) => {
                crate::service::announce(bus, &decided, None, None);
                acted += 1;
            }
            Ok(crate::store::Acted::Duplicate(_)) => {}
            Err(StoreError::Invalid(said)) if said == NOTHING_DUE => {}
            Err(failed) => return Err(failed.to_string()),
        }
    }
    Ok(acted)
}

const NOTHING_DUE: &str = "the order is no longer due to expire";

/// Expire what is due now, and then every `every`, for as long as the
/// process runs.
pub async fn expire_forever(bus: Arc<Bus>, store: Arc<dyn Store>, every: Duration) {
    loop {
        let (expiring, holding) = (bus.clone(), store.clone());
        match tokio::task::spawn_blocking(move || expire_due(&expiring, &holding)).await {
            Ok(Ok(acted)) => {
                if acted > 0 {
                    tracing::info!(acted, "orders whose time in force ended were expired");
                }
            }
            Ok(Err(why)) => tracing::warn!(why, "could not expire the orders due"),
            Err(failed) => tracing::warn!(%failed, "the expiry task failed"),
        }
        tokio::time::sleep(every).await;
    }
}

/// The upgrade's backfill (W9.13, decisions/031): every account the book
/// held before contract v22 waits for compliance, recorded so, each its own
/// entry, the book's own act, at the moment it ran -- never back-dated. An
/// account whose entries state it already is left. Says how many.
pub fn backfill_waits(bus: &Bus, store: &dyn Store) -> Result<usize, String> {
    let mut recorded = 0;
    for account in store.accounts().map_err(|failed| failed.to_string())? {
        let now = bus.clock().now_ns();
        let ctx = Context {
            instance_id: bus.instance_id().to_string(),
            event_time_ns: now,
            received_at_ns: now,
            committed_at_ns: now,
            ..Default::default()
        };
        let outcome = store.act(&account, "", "", &[], &mut |book, head| {
            if book.waits_stated || book.entries.is_empty() {
                return Err(StoreError::Invalid(NOTHING_DUE.into()));
            }
            let mut attributes = crate::decide::attributes_of(book);
            attributes.waits_for_compliance = true;
            let made = crate::decide::made(
                book,
                head,
                &ctx,
                crate::decide::Draft {
                    entry_id: ctx.mint(crate::ids::ENTRY),
                    kind: "waits-for-compliance-backfilled",
                    effective_date: ctx.today(),
                    reason: "contract v22: releases wait for compliance on every account the \
                             book held at the upgrade (W9.13, decisions/031)"
                        .into(),
                    break_ids: Vec::new(),
                    idempotency_key: String::new(),
                    actor: ctx.actor(),
                    body: crate::journal::Body {
                        attributes: Some(attributes),
                        states_waits: true,
                        ..Default::default()
                    },
                },
            )?;
            Ok(crate::service::decided(made, Vec::new()))
        });
        match outcome {
            Ok(crate::store::Acted::Committed(decided)) => {
                crate::service::announce(bus, &decided, None, None);
                recorded += 1;
            }
            Ok(crate::store::Acted::Duplicate(_)) => {}
            Err(StoreError::Invalid(said)) if said == NOTHING_DUE => {}
            Err(failed) => return Err(failed.to_string()),
        }
    }
    Ok(recorded)
}
