//! What each act of W11 is, decided against the account as its journal
//! stands (contract v22).
//!
//! **No plugin sets a state.** A command asks for a change and a fact
//! reports one; each function here decides it against the account replayed
//! from its journal, under the partition's lock, and answers with the entry
//! -- its acts, from which the fold derives every status -- or with the
//! refusal, changing nothing. The transition table is the generated one
//! ([`crate::transitions`], decisions/035): a command meeting a row in a
//! state the row does not start from is refused REFUSAL_REASON_ORDER_STATE;
//! a venue's fact is never refused for its state, and a fact the book cannot
//! place is kept as an unplaced fact.
//!
//! **Core records; it does not police** (step 5's Q1). The gate, the
//! claim, the ownership-transfer equality and the connectivity link are the
//! book's; limits, approvals by policy and every trading instruction are a
//! plugin's, recorded here.

use std::collections::{BTreeMap, BTreeSet};

use meridian_domain::exact::Exact;
use meridian_domain::v1::{
    accept_execution_request, actor, change_request, unplaced_fact, AcceptExecutionRequest,
    AcknowledgeFlagRequest, Actor, ApprovalDecision, ChangeRequest, ChangeRequestOutcome, Charge,
    Check, CheckVerdict, Claimant, CreatePlacementRequest, DecideApprovalRequest, DkReason,
    DontKnowExecutionRequest, Execution, ExecutionKind, ExecutionState, HoldingSide, LocateUseKind,
    LotReliefMethod, LotSource, LotTerms, Money, MovementLine, Order, OrderChange, OrderChangeKind,
    OrderCondition, OrderLinkKind, OrderOrigin, OrderState, OrderType, PersonRoles, Placement,
    PlacementChange, PlacementChangeKind, PlacementEvent, PlacementState, ProposeOrderRequest,
    RecordCheckRequest, RecordLocateRequest, RecordLocateUseRequest, RecordOrderRequest,
    ReleaseOrderRequest, ReportChargeRequest, ReportExecutionRequest, ReportPlacementRequest,
    RequestOrderChangeRequest, RequestPlacementChangeRequest, SetTradingInstructionRequest,
    SetWorkingConditionRequest, SetterLevel, SettlementBucket, Side, StandingOrderCondition,
    TieOutReason, TimeInForce, TradingInstruction, TradingInstructionTarget,
    TradingInstructionValue, UnplacedFact, UnplacedReason, VenueActor,
};
use meridian_pb::v1::RefusalReason;

use crate::book::{Book, Key};
use crate::decide::{attributes_of, first_attributes, made, Context, Draft, Made};
use crate::ids;
use crate::journal::Body;
use crate::numbers;
use crate::orders::{self, key_of, Act, OrderHeld};
use crate::store::{is_account, Result, StoreError};
use crate::transitions::{From, To, Transition, EXECUTION_TRANSITIONS, TRANSITIONS};

// ── The table (decisions/035) ───────────────────────────────────────────────

/// A row of the generated table, by its number.
pub fn row(id: &str) -> &'static Transition {
    TRANSITIONS
        .iter()
        .find(|row| row.row == id)
        .unwrap_or_else(|| panic!("the transition table names no row {id}"))
}

/// Whether a row starts from `from` (None: a record being created).
pub fn starts_from(row: &Transition, from: Option<OrderState>) -> bool {
    row.from.iter().any(|held| match held {
        From::Any => true,
        From::None => from.is_none(),
        From::State(state) => Some(*state) == from,
    })
}

/// Whether a row allows `from` to `to`: it starts from `from`, and `to` is
/// one it reaches or decides among, or the state left as it was.
pub fn admits(row: &Transition, from: Option<OrderState>, to: OrderState) -> bool {
    starts_from(row, from)
        && (Some(to) == from
            || row.to.iter().any(|held| match held {
                To::Unchanged => Some(to) == from,
                To::State(state) => *state == to,
            }))
}

/// A command refused for the state its order is in, by its row.
fn state_refused(row: &Transition, state: OrderState, what: &str) -> StoreError {
    StoreError::refused(
        RefusalReason::OrderState,
        format!(
            "{what} is not allowed on an order that is {} (the transition table's row {}: {})",
            state_named(state),
            row.row,
            row.cause
        ),
    )
}

/// A command's row checked against the order's state before it is applied.
fn command_from(row_id: &str, state: OrderState, what: &str) -> Result<()> {
    let row = row(row_id);
    if starts_from(row, Some(state)) {
        return Ok(());
    }
    Err(state_refused(row, state, what))
}

pub fn state_named(state: OrderState) -> &'static str {
    match state {
        OrderState::Proposed => "proposed",
        OrderState::Refused => "refused",
        OrderState::Released => "released",
        OrderState::Working => "working",
        OrderState::Filled => "filled",
        OrderState::Cancelled => "cancelled",
        OrderState::Expired => "expired",
        OrderState::Unspecified => "unknown to the book",
    }
}

fn ended(state: OrderState) -> bool {
    matches!(
        state,
        OrderState::Refused | OrderState::Cancelled | OrderState::Expired
    )
}

// ── Who, and what may be named ──────────────────────────────────────────────

/// A command other than a fact report needs a user: a person or a service
/// account (W11's invariants; REFUSAL_REASON_ACTOR_REQUIRED).
fn user_required(ctx: &Context, what: &str) -> Result<()> {
    if ctx.acting_for.is_empty() {
        return Err(StoreError::refused(
            RefusalReason::ActorRequired,
            format!("{what} is a user's act, a person's or a service account's, and this was sent for no one"),
        ));
    }
    Ok(())
}

/// An approval or an override is a person's (W11.5).
fn person_required(ctx: &Context, what: &str) -> Result<()> {
    if ctx.acting_for.is_empty()
        || ctx.acting_for_kind == meridian_pb::v1::SubjectKind::ServiceAccount as i32
    {
        return Err(StoreError::refused(
            RefusalReason::ActorRequired,
            format!("{what} is a person's act, and no person stands behind this"),
        ));
    }
    Ok(())
}

/// A command naming its record by ID alone is held to the write scope the
/// sidecar stamped on it (W4.9; the order-path plan's protos, point 8):
/// PERMISSION_DENIED with no reason code, nothing changed.
fn in_scope(ctx: &Context, account_id: &str) -> Result<()> {
    match &ctx.account_scope {
        Some(scope) if !scope.contains(account_id) => Err(StoreError::Denied(format!(
            "the record is in account {account_id}, which is outside the write scope this \
             command was sent with: nobody may write it through this plugin"
        ))),
        _ => Ok(()),
    }
}

/// Whether a fact's account is within the sender's scope, where one was
/// stamped: a venue's fact outside it is kept unplaced, never placed.
fn fact_in_scope(ctx: &Context, account_id: &str) -> bool {
    ctx.account_scope
        .as_ref()
        .is_none_or(|scope| scope.contains(account_id))
}

fn malformed(fields: &[&str], words: impl Into<String>) -> StoreError {
    StoreError::Malformed {
        fields: fields.iter().map(|field| field.to_string()).collect(),
        words: words.into(),
    }
}

/// The venue, through the connectivity plugin reporting it (W11.10): a
/// fact's actor, never a person's act.
fn venue_actor(ctx: &Context) -> Actor {
    Actor {
        kind: Some(actor::Kind::Venue(VenueActor {
            instance_id: ctx.instance_id.clone(),
            venue_id: String::new(),
        })),
    }
}

/// A fact's actor: the user the command was sent for where there is one
/// (an entered execution, a telephoned order), else the venue.
fn fact_actor(ctx: &Context) -> Actor {
    if ctx.acting_for.is_empty() {
        venue_actor(ctx)
    } else {
        ctx.actor()
    }
}

fn standing(
    ctx: &Context,
    condition: OrderCondition,
    reason: impl Into<String>,
) -> StandingOrderCondition {
    StandingOrderCondition {
        condition: condition as i32,
        reason: reason.into(),
        actor: Some(ctx.actor()),
        ..Default::default()
    }
}

fn condition(order_id: &str, set: bool, standing: StandingOrderCondition) -> Act {
    Act::Condition {
        order_id: order_id.to_string(),
        condition: standing.condition,
        set,
        standing,
    }
}

/// The order a command names, in this account's book.
fn held<'a>(book: &'a Book, order_id: &str) -> Result<&'a OrderHeld> {
    book.ledger.orders.get(order_id).ok_or_else(|| {
        StoreError::Invalid(format!(
            "account {} holds no order {order_id}",
            book.account_id
        ))
    })
}

/// Whether a claim lets this command's sender create or change the order's
/// placements and children: none stands, or it is the claimant's, the same
/// instance for the same user (Q4).
fn claim_allows(held: &OrderHeld, ctx: &Context) -> bool {
    let Some(claim) = held
        .conditions
        .get(&(OrderCondition::Claimed as i32))
        .and_then(|standing| standing.claimant.as_ref())
    else {
        return true;
    };
    let user = match claim.user.as_ref() {
        Some(meridian_domain::v1::claimant::User::Person(person)) => person.subject.as_str(),
        Some(meridian_domain::v1::claimant::User::ServiceAccount(account)) => {
            account.subject.as_str()
        }
        None => "",
    };
    claim.instance_id == ctx.instance_id && user == ctx.acting_for
}

fn claim_refused(held: &OrderHeld) -> StoreError {
    let by = held
        .conditions
        .get(&(OrderCondition::Claimed as i32))
        .and_then(|standing| standing.claimant.as_ref())
        .map(|claim| claim.instance_id.clone())
        .unwrap_or_default();
    StoreError::refused(
        RefusalReason::Claimed,
        format!(
            "order {} is claimed through {by}: only the claimant creates or changes its \
             placements and child orders, and anyone who may write the account may still \
             cancel it or remove the claim",
            held.order.order_id
        ),
    )
}

fn decimal(field: &str, wire: Option<&meridian_pb::v1::Decimal>) -> Result<Option<Exact>> {
    wire.map(|wire| {
        Exact::from_wire(wire).map_err(|out| malformed(&[field], format!("{field} {out}")))
    })
    .transpose()
}

fn amount_of(money: Option<&Money>) -> Option<Exact> {
    money
        .and_then(|money| money.amount.as_ref())
        .and_then(|amount| Exact::from_wire(amount).ok())
}

// ── Making an entry ─────────────────────────────────────────────────────────

/// What an order-path entry is, before its consequences are worked out.
struct Plan {
    kind: &'static str,
    actor: Actor,
    acts: Vec<Act>,
    lines: Vec<MovementLine>,
    reason: String,
    idempotency_key: String,
    effective_date: String,
}

impl Plan {
    fn new(kind: &'static str, actor: Actor, acts: Vec<Act>) -> Plan {
        Plan {
            kind,
            actor,
            acts,
            lines: Vec::new(),
            reason: String::new(),
            idempotency_key: String::new(),
            effective_date: String::new(),
        }
    }

    fn reason(mut self, reason: &str) -> Plan {
        self.reason = reason.to_string();
        self
    }

    fn key(mut self, key: &str) -> Plan {
        self.idempotency_key = key.to_string();
        self
    }
}

/// Every order whose cancel or expiry was waiting and now has nothing live
/// ends (W11.3, W11.13), up the chain of parents, and a cancel's own
/// request is applied with it.
fn settle(book: &Book, acts: Vec<Act>) -> Result<Vec<Act>> {
    let mut ledger = book.ledger.clone();
    let none = BTreeMap::new();
    for act in &acts {
        ledger.apply(act, "", &none);
    }
    let mut out = acts;
    loop {
        let mut more = Vec::new();
        for (order_id, held) in &ledger.orders {
            let (Some(ending), None) = (held.ending, held.ended) else {
                continue;
            };
            let (placements, children) = ledger.live_parts(order_id)?;
            if !placements.is_empty() || !children.is_empty() {
                continue;
            }
            if ending == OrderState::Cancelled {
                for id in &held.change_requests {
                    let pending = ledger.changes.get(id).is_some_and(|request| {
                        request.outcome == ChangeRequestOutcome::Pending as i32
                            && matches!(
                                request.change.as_ref(),
                                Some(change_request::Change::OrderChange(change))
                                    if change.kind == OrderChangeKind::Cancel as i32
                            )
                    });
                    if pending {
                        more.push(Act::ChangeOutcome {
                            change_request_id: id.clone(),
                            outcome: ChangeRequestOutcome::Applied as i32,
                        });
                    }
                }
            }
            more.push(Act::Ended {
                order_id: order_id.clone(),
                state: ending as i32,
            });
        }
        if more.is_empty() {
            break;
        }
        for act in &more {
            ledger.apply(act, "", &none);
        }
        out.extend(more);
    }
    Ok(out)
}

/// The entry made: its consequences settled, a new account's first
/// attributes stated (W9.13), numbered and replayed.
fn finish(book: &Book, head: u64, ctx: &Context, plan: Plan) -> Result<Made> {
    let acts = settle(book, plan.acts)?;
    let trade = !plan.lines.is_empty();
    let first = first_attributes(book) && is_account(&book.account_id);
    let draft = Draft {
        entry_id: ctx.mint(ids::ENTRY),
        kind: plan.kind,
        effective_date: if plan.effective_date.is_empty() {
            ctx.today()
        } else {
            plan.effective_date
        },
        reason: plan.reason,
        break_ids: Vec::new(),
        idempotency_key: plan.idempotency_key,
        actor: plan.actor,
        body: Body {
            acts,
            lines: plan.lines,
            trade,
            attributes: first.then(|| attributes_of(book)),
            states_waits: first,
            ..Default::default()
        },
    };
    made(book, head, ctx, draft)
}

/// The state an order is in, by the fold.
fn state_of(book: &Book, order_id: &str) -> Result<OrderState> {
    book.ledger.state(order_id)
}

/// After a command is decided, its order's state checked against its row:
/// the fold may only reach what the row allows.
fn check_reached(
    book: &Book,
    made: &Made,
    order_id: &str,
    row_id: &str,
    from: Option<OrderState>,
) -> Result<()> {
    let row = row(row_id);
    let to = made.book.ledger.state(order_id)?;
    if admits(row, from, to) {
        return Ok(());
    }
    let _ = book;
    Err(state_refused(
        row,
        from.unwrap_or(OrderState::Unspecified),
        &format!(
            "this change, which would leave order {order_id} {}",
            state_named(to)
        ),
    ))
}

/// A venue's fact held to the table, logged when it does not fit and never
/// refused for it (requirement 12).
fn fact_reached(made: &Made, order_id: &str, row_ids: &[&str], from: OrderState) {
    let Ok(to) = made.book.ledger.state(order_id) else {
        return;
    };
    if to == from || row_ids.iter().any(|id| admits(row(id), Some(from), to)) {
        return;
    }
    tracing::warn!(
        order_id,
        from = state_named(from),
        to = state_named(to),
        rows = row_ids.join(","),
        "a venue's fact took an order where the table does not; recorded, never refused"
    );
}

// ── W11.1: propose an order ─────────────────────────────────────────────────

pub fn propose_order(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &ProposeOrderRequest,
) -> Result<Made> {
    user_required(ctx, "proposing an order")?;
    match (request.quantity.is_some(), request.cash_quantity.is_some()) {
        (true, true) | (false, false) => {
            return Err(malformed(
                &["quantity", "cash_quantity"],
                "an order states exactly one of quantity and cash_quantity, as FIX's OrderQty \
                 and CashOrderQty are alternatives",
            ))
        }
        _ => {}
    }
    if let Some(quantity) = decimal("quantity", request.quantity.as_ref())? {
        if quantity.is_zero() {
            return Err(malformed(
                &["quantity"],
                "an order of 0, a pass, is not defined until slice C",
            ));
        }
        if quantity.is_negative() {
            return Err(malformed(
                &["quantity"],
                "an order's quantity is positive; its side says which way",
            ));
        }
    }
    if let Some(cash) = &request.cash_quantity {
        match amount_of(Some(cash)) {
            Some(amount) if !amount.is_zero() && !amount.is_negative() => {}
            _ => {
                return Err(malformed(
                    &["cash_quantity"],
                    "an order for an amount states a positive amount",
                ))
            }
        }
    }
    if request.instrument_id.is_empty() {
        return Err(malformed(
            &["instrument_id"],
            "an order names its instrument",
        ));
    }
    if request.side == Side::Unspecified as i32 || Side::try_from(request.side).is_err() {
        return Err(malformed(&["side"], "an order names its side"));
    }
    let child = request
        .link
        .as_ref()
        .filter(|link| link.kind == OrderLinkKind::Split as i32);
    if let Some(link) = request.link.as_ref() {
        if link.kind != OrderLinkKind::Split as i32 {
            return Err(malformed(
                &["link.kind"],
                "the split is the one named link in v22",
            ));
        }
    }
    ctx.references_answered()?;
    let missing = ctx.instrument_missing("", &request.instrument_id);
    if !missing.is_empty() {
        return Err(StoreError::incomplete(
            "the order's instrument",
            missing
                .into_iter()
                .map(|field| field.trim_start_matches('.').to_string())
                .collect(),
        ));
    }

    let mut order = Order {
        order_id: ctx.mint(ids::ORDER),
        account_id: book.account_id.clone(),
        instrument_id: request.instrument_id.clone(),
        sleeve: request.sleeve.clone(),
        side: request.side,
        locate_id: request.locate_id.clone(),
        quantity: request.quantity,
        cash_quantity: request.cash_quantity.clone(),
        order_type: request.order_type,
        limit_price: request.limit_price.clone(),
        stop_price: request.stop_price.clone(),
        dealt_currency: request.dealt_currency.clone(),
        legs: request.legs.clone(),
        allocation_instruction: request.allocation_instruction.clone(),
        time_in_force: request.time_in_force,
        expire_date: request.expire_date.clone(),
        expire_time_ns: request.expire_time_ns,
        execution_instructions: request.execution_instructions.clone(),
        lot_relief_method: request.lot_relief_method,
        relieve_lot_ids: request.relieve_lot_ids.clone(),
        capacity: request.capacity,
        direction: request.direction,
        parties: request.parties.clone(),
        person_roles: Some(PersonRoles {
            entered_by: Some(ctx.actor()),
            executed_by: None,
            decided_by: request.decided_by.clone(),
        }),
        customer_id: request.customer_id.clone(),
        firm_designated_id: request.firm_designated_id.clone(),
        handling_instructions: request.handling_instructions.clone(),
        received_at_ns: request.received_at_ns,
        received_precision: request.received_precision,
        quote_at_receipt: request.quote_at_receipt.clone(),
        represents_order_ids: request.represents_order_ids.clone(),
        axe_source: request.axe_source.clone(),
        axe_counterparty_id: request.axe_counterparty_id.clone(),
        link: request.link.clone(),
        ..Default::default()
    };

    let Some(link) = child else {
        let plan = Plan::new(
            "order-proposed",
            ctx.actor(),
            vec![Act::Opened {
                order,
                released: false,
            }],
        )
        .key(&request.idempotency_key);
        return finish(book, head, ctx, plan);
    };

    // A child order (the split link, Q6, Q19 (i), Q46 to Q48): in its
    // parent's account, under a released parent, with its parent's
    // instrument and side and no allocation instruction of its own;
    // released on creation within the parent, no gate and no wait again.
    let parent = held(book, &link.parent_order_id)?;
    if request.instrument_id != parent.order.instrument_id {
        return Err(malformed(
            &["instrument_id"],
            format!(
                "a child order has its parent's instrument, {}",
                parent.order.instrument_id
            ),
        ));
    }
    if request.side != parent.order.side {
        return Err(malformed(&["side"], "a child order has its parent's side"));
    }
    if request
        .allocation_instruction
        .as_ref()
        .is_some_and(|instruction| !instruction.lines.is_empty())
    {
        return Err(malformed(
            &["allocation_instruction"],
            "a child order carries no allocation instruction of its own: its parent's governs",
        ));
    }
    let parent_state = state_of(book, &link.parent_order_id)?;
    if !matches!(parent_state, OrderState::Released | OrderState::Working) {
        return Err(state_refused(
            row("i2"),
            parent_state,
            &format!(
                "a child order under order {}, which is {},",
                link.parent_order_id,
                state_named(parent_state)
            ),
        ));
    }
    if !claim_allows(parent, ctx) {
        return Err(claim_refused(parent));
    }
    order.sleeve = parent.order.sleeve.clone();
    let plan = Plan::new(
        "order-proposed",
        ctx.actor(),
        vec![Act::Opened {
            order,
            released: true,
        }],
    )
    .key(&request.idempotency_key);
    finish(book, head, ctx, plan)
}

// ── W11.2: record an order the book did not create ──────────────────────────

/// A telephoned or outside order, recorded by the `oms` for a user: a fact
/// (row 23a), never through the gate, and a command, refused naming what it
/// lacks. A connectivity plugin records no order (ruled 2026-10-13): its
/// sidecar refuses the row before it reaches the book.
pub fn record_order(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &RecordOrderRequest,
) -> Result<Made> {
    user_required(ctx, "recording an order")?;
    if request.instrument_id.is_empty() {
        return Err(malformed(
            &["instrument_id"],
            "an order names its instrument",
        ));
    }
    if request.side == Side::Unspecified as i32 || Side::try_from(request.side).is_err() {
        return Err(malformed(&["side"], "an order names its side"));
    }
    match decimal("quantity", request.quantity.as_ref())? {
        Some(quantity) if !quantity.is_negative() && !quantity.is_zero() => {}
        _ => {
            return Err(malformed(
                &["quantity"],
                "a recorded order states its quantity",
            ))
        }
    }
    ctx.references_answered()?;
    let missing = ctx.instrument_missing("", &request.instrument_id);
    if !missing.is_empty() {
        return Err(StoreError::incomplete(
            "the order's instrument",
            missing
                .into_iter()
                .map(|field| field.trim_start_matches('.').to_string())
                .collect(),
        ));
    }
    if !request.venue_order_id.is_empty() {
        if let Some(order_id) = book
            .ledger
            .venue_facts
            .get(&orders::recorded_order_key(&request.venue_order_id))
        {
            return Err(StoreError::refused(
                RefusalReason::IdentifierInUse,
                format!(
                    "venue order {} is recorded already, as order {order_id}",
                    request.venue_order_id
                ),
            ));
        }
    }
    let actor = ctx.actor();
    let order = Order {
        order_id: ctx.mint(ids::ORDER),
        account_id: book.account_id.clone(),
        instrument_id: request.instrument_id.clone(),
        sleeve: request.sleeve.clone(),
        side: request.side,
        quantity: request.quantity,
        order_type: request.order_type,
        limit_price: request.limit_price.clone(),
        time_in_force: request.time_in_force,
        origin: OrderOrigin::Telephoned as i32,
        venue_order_id: request.venue_order_id.clone(),
        person_roles: Some(PersonRoles {
            entered_by: Some(actor.clone()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let plan = Plan::new(
        "order-recorded",
        actor,
        vec![Act::Opened {
            order,
            released: true,
        }],
    )
    .key(&request.idempotency_key);
    finish(book, head, ctx, plan)
}

// ── W11.3: ask for a change to an order ─────────────────────────────────────

/// The acts that ask an order's live placements and live children to cancel,
/// each child's own parts first, and mark each as ending (W11.3, W11.13).
fn cancel_parts(
    book: &Book,
    ctx: &Context,
    order_id: &str,
    ending: OrderState,
    reason: &str,
    acts: &mut Vec<Act>,
) -> Result<()> {
    let (placements, children) = book.ledger.live_parts(order_id)?;
    for placement_id in placements {
        let already = book.ledger.placements[&placement_id]
            .change_requests
            .iter()
            .filter_map(|id| book.ledger.changes.get(id))
            .any(|request| {
                request.outcome == ChangeRequestOutcome::Pending as i32
                    && matches!(
                        request.change.as_ref(),
                        Some(change_request::Change::PlacementChange(change))
                            if change.kind == PlacementChangeKind::Cancel as i32
                    )
            });
        if already {
            continue;
        }
        acts.push(Act::ChangeOpened {
            request: ChangeRequest {
                change_request_id: ctx.mint(ids::CHANGE_REQUEST),
                order_id: order_id.to_string(),
                placement_id,
                change: Some(change_request::Change::PlacementChange(PlacementChange {
                    kind: PlacementChangeKind::Cancel as i32,
                    ..Default::default()
                })),
                reason: reason.to_string(),
                outcome: ChangeRequestOutcome::Pending as i32,
                ..Default::default()
            },
        });
    }
    for child in children {
        acts.push(Act::ChangeOpened {
            request: ChangeRequest {
                change_request_id: ctx.mint(ids::CHANGE_REQUEST),
                order_id: child.clone(),
                change: Some(change_request::Change::OrderChange(OrderChange {
                    kind: OrderChangeKind::Cancel as i32,
                    ..Default::default()
                })),
                reason: reason.to_string(),
                outcome: ChangeRequestOutcome::Pending as i32,
                ..Default::default()
            },
        });
        cancel_parts(book, ctx, &child, ending, reason, acts)?;
        acts.push(Act::Ending {
            order_id: child,
            state: ending as i32,
        });
    }
    Ok(())
}

/// Whether an allocation instruction equals the ordered quantity (principle
/// 1): its amounts summing to it, its weights to one, or a mix whose amounts
/// and weighted shares do; with none, the whole is in the order's account.
fn allocation_balanced(book: &Book, held: &OrderHeld) -> Result<bool> {
    let Some(instruction) = held
        .order
        .allocation_instruction
        .as_ref()
        .filter(|instruction| !instruction.lines.is_empty())
    else {
        return Ok(true);
    };
    let ordered = book.ledger.ordered(held);
    let mut amounts = Exact::ZERO;
    let mut weights = Exact::ZERO;
    for line in &instruction.lines {
        if let Some(quantity) = decimal(
            "allocation_instruction.lines.quantity",
            line.quantity.as_ref(),
        )? {
            amounts = numbers::add("allocation", amounts, quantity)?;
        } else if let Some(weight) =
            decimal("allocation_instruction.lines.weight", line.weight.as_ref())?
        {
            weights = numbers::add("allocation", weights, weight)?;
        } else {
            return Ok(false);
        }
    }
    let weighted = orders::multiply("allocation", weights, ordered)?;
    Ok(numbers::add("allocation", amounts, weighted)? == ordered)
}

pub fn request_order_change(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &RequestOrderChangeRequest,
) -> Result<(Made, String)> {
    let order = held(book, &request.order_id)?;
    in_scope(ctx, &order.order.account_id)?;
    user_required(ctx, "asking for a change to an order")?;
    let change = request
        .change
        .clone()
        .ok_or_else(|| malformed(&["change"], "a change request says what it asks"))?;
    let kind = OrderChangeKind::try_from(change.kind).unwrap_or(OrderChangeKind::Unspecified);
    let state = state_of(book, &request.order_id)?;
    let order_id = request.order_id.clone();
    let crq = ctx.mint(ids::CHANGE_REQUEST);
    let opened = |outcome: ChangeRequestOutcome| Act::ChangeOpened {
        request: ChangeRequest {
            change_request_id: crq.clone(),
            order_id: order_id.clone(),
            change: Some(change_request::Change::OrderChange(change.clone())),
            reason: request.reason.clone(),
            outcome: outcome as i32,
            ..Default::default()
        },
    };
    let (row_id, kind_name, acts): (&str, &'static str, Vec<Act>) = match kind {
        // An order cancel (row i3): each live placement and child asked to
        // cancel; cancelled when none is live. Anyone who may write the
        // account cancels past a claim (Q4).
        OrderChangeKind::Cancel => {
            command_from("i3", state, "a cancel")?;
            let mut acts = vec![opened(ChangeRequestOutcome::Pending)];
            cancel_parts(
                book,
                ctx,
                &order_id,
                OrderState::Cancelled,
                &request.reason,
                &mut acts,
            )?;
            acts.push(Act::Ending {
                order_id: order_id.clone(),
                state: OrderState::Cancelled as i32,
            });
            ("i3", "order-cancel-requested", acts)
        }
        // A proposal withdrawn (row i1).
        OrderChangeKind::Withdraw => {
            command_from("i1", state, "a withdrawal")?;
            (
                "i1",
                "order-withdrawn",
                vec![
                    opened(ChangeRequestOutcome::Applied),
                    Act::Ended {
                        order_id: order_id.clone(),
                        state: OrderState::Cancelled as i32,
                    },
                ],
            )
        }
        // Back to proposed, with nothing committed or filled (row 5).
        OrderChangeKind::Recall => {
            command_from("5", state, "a recall")?;
            if !claim_allows(order, ctx) {
                return Err(claim_refused(order));
            }
            let quantities = book.ledger.quantities(&order_id)?;
            let nothing = |value: &Option<meridian_pb::v1::Decimal>| {
                value
                    .as_ref()
                    .and_then(|value| Exact::from_wire(value).ok())
                    .is_none_or(Exact::is_zero)
            };
            if !nothing(&quantities.committed) || !nothing(&quantities.reported_filled) {
                return Err(state_refused(
                    row("5"),
                    state,
                    "a recall of an order with quantity committed or filled",
                ));
            }
            (
                "5",
                "order-recalled",
                vec![
                    opened(ChangeRequestOutcome::Applied),
                    Act::Recalled {
                        order_id: order_id.clone(),
                    },
                ],
            )
        }
        // Its limit, time in force or execution instructions: applied to the
        // order at once, and carried to its live placements as their
        // replaces (rows 15, 16).
        OrderChangeKind::Amend => {
            if ended(state) || state == OrderState::Filled {
                return Err(state_refused(row("15"), state, "an amendment"));
            }
            if !claim_allows(order, ctx) {
                return Err(claim_refused(order));
            }
            let mut terms = order.order.clone();
            if change.limit_price.is_some() {
                terms.limit_price = change.limit_price.clone();
            }
            if change.time_in_force != TimeInForce::Unspecified as i32 {
                terms.time_in_force = change.time_in_force;
            }
            if !change.execution_instructions.is_empty() {
                terms.execution_instructions = change.execution_instructions.clone();
            }
            let mut acts = vec![
                opened(ChangeRequestOutcome::Applied),
                Act::Terms { order: terms },
            ];
            if state != OrderState::Proposed {
                for placement_id in book.ledger.live_parts(&order_id)?.0 {
                    acts.push(Act::ChangeOpened {
                        request: ChangeRequest {
                            change_request_id: ctx.mint(ids::CHANGE_REQUEST),
                            order_id: order_id.clone(),
                            placement_id,
                            change: Some(change_request::Change::PlacementChange(
                                PlacementChange {
                                    kind: PlacementChangeKind::Replace as i32,
                                    limit_price: change.limit_price.clone(),
                                    time_in_force: change.time_in_force,
                                    quantity: None,
                                },
                            )),
                            reason: request.reason.clone(),
                            outcome: ChangeRequestOutcome::Pending as i32,
                            ..Default::default()
                        },
                    });
                }
            }
            let row_id = if state == OrderState::Proposed {
                ""
            } else {
                "15"
            };
            (row_id, "order-amended", acts)
        }
        // An increase or a reduction: the order's own proposer's, its
        // allocation amended in the same act; any other's only as a
        // trader's alignment, with its reason, bringing ordered to what was
        // transacted (rows 18, 19; Q16, Q20, Q21).
        OrderChangeKind::Quantity => {
            if ended(state) {
                return Err(state_refused(row("18"), state, "a quantity change"));
            }
            let quantity =
                decimal("change.quantity", change.quantity.as_ref())?.ok_or_else(|| {
                    malformed(
                        &["change.quantity"],
                        "a quantity change states the new ordered quantity",
                    )
                })?;
            if quantity.is_negative() {
                return Err(malformed(
                    &["change.quantity"],
                    "an ordered quantity is never below zero",
                ));
            }
            let own = !order.opened_by.is_empty() && order.opened_by == ctx.instance_id;
            let (filled, _, _) = book.ledger.filled(&order_id)?;
            let alignment = quantity == filled;
            if !own {
                if !alignment {
                    return Err(state_refused(
                        row("18"),
                        state,
                        "a quantity change by other than the order's own proposer that is not \
                         an alignment bringing ordered to what was transacted",
                    ));
                }
                if request.reason.trim().is_empty() {
                    return Err(StoreError::refused(
                        RefusalReason::ReasonRequired,
                        "an alignment is a justified act, and this carries no reason",
                    ));
                }
            }
            if !claim_allows(order, ctx) && !own {
                return Err(claim_refused(order));
            }
            let ordered = book.ledger.ordered(order);
            let mut terms = order.order.clone();
            if terms.quantity.is_some() || terms.cash_quantity.is_none() {
                terms.quantity = numbers::wire(quantity);
            } else if let Some(cash) = terms.cash_quantity.as_mut() {
                cash.amount = numbers::wire(quantity);
            }
            if let Some(instruction) = change.allocation_instruction.clone() {
                terms.allocation_instruction = Some(instruction);
            }
            let mut acts = vec![
                opened(ChangeRequestOutcome::Applied),
                Act::Terms { order: terms },
            ];
            // An increase on an account waiting for compliance: awaiting
            // check on the new version, refusing nothing (Q20).
            let gated = attributes_of(book).waits_for_compliance;
            if quantity > ordered && gated && order.released {
                acts.push(condition(
                    &order_id,
                    true,
                    standing(
                        ctx,
                        OrderCondition::AwaitingCheck,
                        "an increase waits for compliance on the new version",
                    ),
                ));
            }
            // An alignment makes the order tie out: a restatement's flag goes
            // with it (Q21).
            if alignment
                && order
                    .conditions
                    .contains_key(&(OrderCondition::DoesNotTieOut as i32))
            {
                acts.push(condition(
                    &order_id,
                    false,
                    standing(ctx, OrderCondition::DoesNotTieOut, "aligned"),
                ));
            }
            let row_id = if quantity > ordered { "18" } else { "19" };
            let _ = row_id;
            ("", "order-quantity-changed", acts)
        }
        OrderChangeKind::Unspecified => {
            return Err(malformed(
                &["change.kind"],
                "a change request says what it asks",
            ))
        }
    };
    let plan = Plan::new(kind_name, ctx.actor(), acts)
        .reason(&request.reason)
        .key(&request.idempotency_key);
    let made = finish(book, head, ctx, plan)?;
    if !row_id.is_empty() {
        check_reached(book, &made, &order_id, row_id, Some(state))?;
    }
    Ok((made, crq))
}

// ── W11.4: record a check ───────────────────────────────────────────────────

/// Whether a verdict stands on the order's current version.
fn verdict_on_version(held: &OrderHeld) -> bool {
    held.checks
        .iter()
        .any(|check| check.order_version == held.version && check.placement_id.is_empty())
}

/// A block on the current version with no override on it (W11.5).
fn block_stands(held: &OrderHeld) -> bool {
    held.overridden_version != Some(held.version)
        && held.checks.iter().any(|check| {
            check.placement_id.is_empty()
                && check.order_version == held.version
                && check.verdict == CheckVerdict::Block as i32
        })
}

/// The gate's first three steps, in their order (requirement 43): the
/// allocation, a block standing, approval required.
fn gate(book: &Book, held: &OrderHeld) -> Result<()> {
    if !allocation_balanced(book, held)? {
        return Err(StoreError::refused(
            RefusalReason::AllocationUnbalanced,
            format!(
                "order {}'s allocation instruction does not sum to its ordered quantity; at \
                 release ordered equals allocated (principle 1)",
                held.order.order_id
            ),
        ));
    }
    if block_stands(held) {
        return Err(StoreError::refused(
            RefusalReason::CheckBlocks,
            format!(
                "a block stands on order {}'s version {} without an override",
                held.order.order_id, held.version
            ),
        ));
    }
    if held
        .conditions
        .contains_key(&(OrderCondition::ApprovalRequired as i32))
    {
        return Err(StoreError::refused(
            RefusalReason::ApprovalRequired,
            format!("order {} requires approval, not given", held.order.order_id),
        ));
    }
    Ok(())
}

pub fn record_check(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &RecordCheckRequest,
) -> Result<Made> {
    let order_id = if request.placement_id.is_empty() {
        request.order_id.clone()
    } else {
        book.ledger
            .placements
            .get(&request.placement_id)
            .map(|held| held.placement.order_id.clone())
            .ok_or_else(|| {
                StoreError::Invalid(format!(
                    "account {} holds no placement {}",
                    book.account_id, request.placement_id
                ))
            })?
    };
    let order = held(book, &order_id)?;
    in_scope(ctx, &order.order.account_id)?;
    user_required(ctx, "recording a check")?;
    if request.verdict == CheckVerdict::Unspecified as i32
        || CheckVerdict::try_from(request.verdict).is_err()
    {
        return Err(malformed(&["verdict"], "a check records its verdict"));
    }
    let on_placement = !request.placement_id.is_empty();
    let version = if on_placement {
        0
    } else if request.order_version == 0 {
        order.version
    } else {
        request.order_version
    };
    let check = Check {
        verdict: request.verdict,
        order_version: version,
        placement_id: request.placement_id.clone(),
        rule: request.rule.clone(),
        note: request.note.clone(),
        instance_id: ctx.instance_id.clone(),
        recorded_by: Some(ctx.actor()),
    };
    let state = state_of(book, &order_id)?;
    let mut acts = vec![Act::Checked {
        order_id: order_id.clone(),
        check: check.clone(),
    }];
    let mut row_id = "";
    if !on_placement {
        // A verdict of refuse ends a proposed order refused (row 4).
        if request.verdict == CheckVerdict::Refuse as i32 && state == OrderState::Proposed {
            acts.push(Act::Ended {
                order_id: order_id.clone(),
                state: OrderState::Refused as i32,
            });
            row_id = "4";
        } else if version == order.version
            && order
                .conditions
                .contains_key(&(OrderCondition::AwaitingCheck as i32))
        {
            // The verdict awaited: awaiting check cleared, and a release
            // asked for goes through the gate now, released where it passes.
            acts.push(condition(
                &order_id,
                false,
                standing(ctx, OrderCondition::AwaitingCheck, "answered"),
            ));
            if order.release_requested && state == OrderState::Proposed {
                let mut after = order.clone();
                after.checks.push(check);
                if gate(book, &after).is_ok() {
                    acts.push(Act::Released {
                        order_id: order_id.clone(),
                    });
                    row_id = "1";
                }
            }
        }
    }
    let plan = Plan::new("check-recorded", ctx.actor(), acts).key(&request.idempotency_key);
    let made = finish(book, head, ctx, plan)?;
    if !row_id.is_empty() {
        check_reached(book, &made, &order_id, row_id, Some(state))?;
    }
    Ok(made)
}

// ── W11.5: approval ─────────────────────────────────────────────────────────

pub fn decide_approval(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &DecideApprovalRequest,
) -> Result<Made> {
    let order = held(book, &request.order_id)?;
    in_scope(ctx, &order.order.account_id)?;
    let decision =
        ApprovalDecision::try_from(request.decision).unwrap_or(ApprovalDecision::Unspecified);
    let order_id = request.order_id.clone();
    let state = state_of(book, &order_id)?;
    let (row_id, acts) = match decision {
        ApprovalDecision::Require => {
            user_required(ctx, "requiring approval")?;
            command_from("2", state, "requiring approval")?;
            (
                "2",
                vec![condition(
                    &order_id,
                    true,
                    standing(
                        ctx,
                        OrderCondition::ApprovalRequired,
                        request.reason.clone(),
                    ),
                )],
            )
        }
        ApprovalDecision::Approve => {
            person_required(ctx, "an approval")?;
            command_from("2", state, "an approval")?;
            (
                "2",
                vec![condition(
                    &order_id,
                    false,
                    standing(
                        ctx,
                        OrderCondition::ApprovalRequired,
                        request.reason.clone(),
                    ),
                )],
            )
        }
        ApprovalDecision::Override => {
            person_required(ctx, "an override")?;
            if request.reason.trim().is_empty() {
                return Err(StoreError::refused(
                    RefusalReason::ReasonRequired,
                    "an override is a justified act, and this carries no reason",
                ));
            }
            command_from("3", state, "an override")?;
            (
                "3",
                vec![Act::Overridden {
                    order_id: order_id.clone(),
                }],
            )
        }
        ApprovalDecision::Reject => {
            user_required(ctx, "a rejection")?;
            command_from("4", state, "a rejection")?;
            (
                "4",
                vec![Act::Ended {
                    order_id: order_id.clone(),
                    state: OrderState::Refused as i32,
                }],
            )
        }
        ApprovalDecision::Unspecified => {
            return Err(malformed(&["decision"], "an approval names its decision"))
        }
    };
    let plan = Plan::new("approval-decided", ctx.actor(), acts)
        .reason(&request.reason)
        .key(&request.idempotency_key);
    let made = finish(book, head, ctx, plan)?;
    check_reached(book, &made, &order_id, row_id, Some(state))?;
    Ok(made)
}

// ── W11.6: release ──────────────────────────────────────────────────────────

pub fn release_order(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &ReleaseOrderRequest,
) -> Result<Made> {
    let order = held(book, &request.order_id)?;
    in_scope(ctx, &order.order.account_id)?;
    user_required(ctx, "releasing an order")?;
    let order_id = request.order_id.clone();
    let state = state_of(book, &order_id)?;
    command_from("1", state, "a release")?;
    gate(book, order)?;
    // The gate's fourth step (Q5): where the account waits for compliance
    // and no verdict stands on this version, the order stays proposed,
    // awaiting check, without a bound -- nothing refused.
    let waits = attributes_of(book).waits_for_compliance;
    let acts = if waits && !verdict_on_version(order) {
        vec![
            Act::ReleaseRequested {
                order_id: order_id.clone(),
            },
            condition(
                &order_id,
                true,
                standing(
                    ctx,
                    OrderCondition::AwaitingCheck,
                    "the account waits for compliance, which has not answered on this version",
                ),
            ),
        ]
    } else {
        vec![Act::Released {
            order_id: order_id.clone(),
        }]
    };
    let plan = Plan::new("order-released", ctx.actor(), acts).key(&request.idempotency_key);
    let made = finish(book, head, ctx, plan)?;
    check_reached(book, &made, &order_id, "1", Some(state))?;
    Ok(made)
}

// ── W11.7: claim, hold, pause ───────────────────────────────────────────────

pub fn set_working_condition(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &SetWorkingConditionRequest,
) -> Result<Made> {
    let order = held(book, &request.order_id)?;
    in_scope(ctx, &order.order.account_id)?;
    user_required(ctx, "claiming, holding or pausing an order")?;
    let order_id = request.order_id.clone();
    let state = state_of(book, &order_id)?;
    let kind = OrderCondition::try_from(request.condition).unwrap_or(OrderCondition::Unspecified);
    if !matches!(
        kind,
        OrderCondition::Claimed | OrderCondition::Held | OrderCondition::Paused
    ) {
        return Err(malformed(
            &["condition"],
            "a working condition is claimed, held or paused, and no more",
        ));
    }
    if ended(state) || state == OrderState::Filled {
        return Err(state_refused(row("9"), state, "a working condition"));
    }
    if kind == OrderCondition::Paused {
        command_from("9", state, "a pause or a resume")?;
    }
    let claimed = order
        .conditions
        .contains_key(&(OrderCondition::Claimed as i32));
    // Anyone who may write the account may remove a claim (Q4); a claim, a
    // hold and a pause are otherwise the claimant's while one stands.
    let removing_claim = kind == OrderCondition::Claimed && !request.set;
    if claimed && !removing_claim && !claim_allows(order, ctx) {
        return Err(claim_refused(order));
    }
    let mut held_condition = standing(ctx, kind, request.reason.clone());
    if kind == OrderCondition::Claimed {
        held_condition.claimant = Some(Claimant {
            instance_id: ctx.instance_id.clone(),
            user: match ctx.actor().kind {
                Some(actor::Kind::Person(person)) => {
                    Some(meridian_domain::v1::claimant::User::Person(person))
                }
                Some(actor::Kind::ServiceAccount(account)) => {
                    Some(meridian_domain::v1::claimant::User::ServiceAccount(account))
                }
                _ => None,
            },
        });
    }
    let plan = Plan::new(
        "working-condition-set",
        ctx.actor(),
        vec![condition(&order_id, request.set, held_condition)],
    )
    .reason(&request.reason)
    .key(&request.idempotency_key);
    finish(book, head, ctx, plan)
}

// ── W11.8: create a placement ───────────────────────────────────────────────

pub fn create_placement(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &CreatePlacementRequest,
) -> Result<Made> {
    let order = held(book, &request.order_id)?;
    in_scope(ctx, &order.order.account_id)?;
    user_required(ctx, "creating a placement")?;
    let order_id = request.order_id.clone();
    let ems = if request.ems_instance_id.is_empty() {
        ctx.instance_id.clone()
    } else {
        request.ems_instance_id.clone()
    };
    // Refused and recorded (W11.8, the refresh's Q30): a connectivity
    // plugin not linked to the placing `ems`, or not made available to the
    // user the command acts for, by the sidecar's verdict.
    if !ctx.connectivity_allowed {
        let words = format!(
            "a placement of order {order_id} by {ems} at {} is refused: that connectivity \
             plugin is not one {ems} is linked to and the user may route through",
            if request.connectivity_instance_id.is_empty() {
                "no connectivity plugin".to_string()
            } else {
                request.connectivity_instance_id.clone()
            }
        );
        let plan = Plan::new(
            "placement-refused",
            ctx.actor(),
            vec![Act::PlacementRefused {
                order_id: order_id.clone(),
            }],
        )
        .reason(&format!(
            "REFUSAL_REASON_CONNECTIVITY_NOT_ALLOWED: ems {ems}, connectivity {}",
            request.connectivity_instance_id
        ))
        .key(&request.idempotency_key);
        let mut made = finish(book, head, ctx, plan)?;
        made.refusal =
            Some(StoreError::refused(RefusalReason::ConnectivityNotAllowed, words).on_the_bus());
        return Ok(made);
    }
    if !claim_allows(order, ctx) {
        return Err(claim_refused(order));
    }
    let state = state_of(book, &order_id)?;
    if !matches!(state, OrderState::Released | OrderState::Working) {
        return Err(state_refused(row("6"), state, "a placement"));
    }
    match decimal("quantity", request.quantity.as_ref())? {
        Some(quantity) if !quantity.is_zero() && !quantity.is_negative() => {}
        _ => {
            return Err(malformed(
                &["quantity"],
                "a placement states a positive quantity",
            ))
        }
    }
    let placement = Placement {
        placement_id: ctx.mint(ids::PLACEMENT),
        order_id: order_id.clone(),
        account_id: order.order.account_id.clone(),
        ems_instance_id: ems,
        connectivity_instance_id: request.connectivity_instance_id.clone(),
        venue_account_code: request.venue_account_code.clone(),
        quantity: request.quantity,
        limit_price: request.limit_price.clone(),
        time_in_force: if request.time_in_force == TimeInForce::Unspecified as i32 {
            order.order.time_in_force
        } else {
            request.time_in_force
        },
        strategy: request.strategy.clone(),
        destination: request.destination.clone(),
        route_type: request.route_type,
        handling_instructions: request.handling_instructions.clone(),
        rfq_id: request.rfq_id.clone(),
        axe_id: request.axe_id.clone(),
        person_roles: Some(PersonRoles {
            executed_by: Some(ctx.actor()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let plan = Plan::new(
        "placement-created",
        ctx.actor(),
        vec![Act::PlacementOpened { placement }],
    )
    .key(&request.idempotency_key);
    let made = finish(book, head, ctx, plan)?;
    check_reached(book, &made, &order_id, "6", Some(state))
        .or_else(|_| check_reached(book, &made, &order_id, "8", Some(state)))?;
    Ok(made)
}

// ── W11.9: ask for a change to a placement ──────────────────────────────────

pub fn request_placement_change(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &RequestPlacementChangeRequest,
) -> Result<(Made, String)> {
    let placed = book
        .ledger
        .placements
        .get(&request.placement_id)
        .ok_or_else(|| {
            StoreError::Invalid(format!(
                "account {} holds no placement {}",
                book.account_id, request.placement_id
            ))
        })?;
    let order = held(book, &placed.placement.order_id)?;
    in_scope(ctx, &order.order.account_id)?;
    let change = request
        .change
        .clone()
        .ok_or_else(|| malformed(&["change"], "a change request says what it asks"))?;
    let kind =
        PlacementChangeKind::try_from(change.kind).unwrap_or(PlacementChangeKind::Unspecified);
    if kind == PlacementChangeKind::Unspecified {
        return Err(malformed(
            &["change.kind"],
            "a placement's change is a cancel or a replace",
        ));
    }
    let state = state_of(book, &order.order.order_id)?;
    let from_the_venue = placed.placement.connectivity_instance_id == ctx.instance_id;
    let (actor, reason, kind_name) = if from_the_venue {
        // A cancel sent at the venue under a stop, reported as a fact (v21's
        // Q17): actor the venue, cause the instruction's entry; not gated,
        // past a claim, needing no service account. Any other change by a
        // connectivity plugin is refused.
        let cause = request
            .cause
            .as_ref()
            .map(|cause| cause.entry_id.clone())
            .unwrap_or_default();
        if kind != PlacementChangeKind::Cancel || cause.is_empty() {
            return Err(state_refused(
                row("i6"),
                state,
                "a connectivity plugin's placement change other than a cancel it sent under a stop",
            ));
        }
        (
            venue_actor(ctx),
            if request.reason.is_empty() {
                format!("cancelled at the venue under the trading instruction of entry {cause}")
            } else {
                format!(
                    "{} (under the trading instruction of entry {cause})",
                    request.reason
                )
            },
            "placement-cancel-reported",
        )
    } else {
        user_required(ctx, "asking for a change to a placement")?;
        if !claim_allows(order, ctx) {
            return Err(claim_refused(order));
        }
        if !book.ledger.placement_live(&request.placement_id)? {
            return Err(state_refused(
                row("16"),
                state,
                &format!(
                    "a change of placement {}, which has ended,",
                    request.placement_id
                ),
            ));
        }
        (
            ctx.actor(),
            request.reason.clone(),
            "placement-change-requested",
        )
    };
    let crq = ctx.mint(ids::CHANGE_REQUEST);
    let plan = Plan::new(
        kind_name,
        actor,
        vec![Act::ChangeOpened {
            request: ChangeRequest {
                change_request_id: crq.clone(),
                order_id: order.order.order_id.clone(),
                placement_id: request.placement_id.clone(),
                change: Some(change_request::Change::PlacementChange(change)),
                reason: reason.clone(),
                outcome: ChangeRequestOutcome::Pending as i32,
                ..Default::default()
            },
        }],
    )
    .reason(&reason)
    .key(&request.idempotency_key);
    Ok((finish(book, head, ctx, plan)?, crq))
}

// ── W11.10: a placement's progress ──────────────────────────────────────────

/// Whether a restatement is past the order's intent (Q15): more than the
/// order's ordered quantity, or a limit worse than the order's for its side.
fn past_intent(book: &Book, order: &OrderHeld, report: &ReportPlacementRequest) -> bool {
    let ordered = book.ledger.ordered(order);
    let quantity_past = report
        .quantity
        .as_ref()
        .and_then(|quantity| Exact::from_wire(quantity).ok())
        .is_some_and(|quantity| quantity > ordered);
    let limit_past = match (
        amount_of(report.limit_price.as_ref()),
        amount_of(order.order.limit_price.as_ref()),
    ) {
        (Some(restated), Some(limit)) => {
            if order.order.side == Side::Buy as i32 {
                restated > limit
            } else {
                restated < limit
            }
        }
        _ => false,
    };
    quantity_past || limit_past
}

pub fn report_placement(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &ReportPlacementRequest,
) -> Result<Made> {
    let event = PlacementEvent::try_from(request.event).unwrap_or(PlacementEvent::Unspecified);
    if event == PlacementEvent::Unspecified {
        return Err(malformed(
            &["event"],
            "a placement's report says what the venue did",
        ));
    }
    let Some(placed) = book.ledger.placements.get(&request.placement_id) else {
        return unplaced(
            book,
            head,
            ctx,
            unplaced_fact::Fact::Placement(request.clone()),
            UnplacedReason::UnknownPlacement,
            "",
            request.raw_record.clone(),
        );
    };
    if placed.placement.connectivity_instance_id != ctx.instance_id {
        return Err(StoreError::Unplace {
            reason: UnplacedReason::UnknownPlacement as i32,
            words: format!(
                "placement {} names {}, not {}",
                request.placement_id, placed.placement.connectivity_instance_id, ctx.instance_id
            ),
        });
    }
    if !fact_in_scope(ctx, &placed.placement.account_id) {
        return Err(StoreError::Unplace {
            reason: UnplacedReason::UnmappedVenueAccount as i32,
            words: format!(
                "placement {}'s account is outside {}'s scope",
                request.placement_id, ctx.instance_id
            ),
        });
    }
    let order_id = placed.placement.order_id.clone();
    let order = held(book, &order_id)?;
    let from = state_of(book, &order_id)?;
    let mut acts = vec![Act::PlacementReported {
        report: request.clone(),
    }];
    let mut rows: Vec<&str> = vec!["8", "14", "20", "22", "24", "26"];
    // A restatement past the order's intent: late fact, and does not tie out
    // until a trader's justified alignment (row 22; Q15, Q21).
    if event == PlacementEvent::Restated && past_intent(book, order, request) {
        acts.push(condition(
            &order_id,
            true,
            StandingOrderCondition {
                condition: OrderCondition::LateFact as i32,
                reason: format!(
                    "placement {} restated past the order's intent",
                    request.placement_id
                ),
                actor: Some(venue_actor(ctx)),
                ..Default::default()
            },
        ));
        acts.push(condition(
            &order_id,
            true,
            StandingOrderCondition {
                condition: OrderCondition::DoesNotTieOut as i32,
                reason: "a restatement past the order's intent".into(),
                actor: Some(venue_actor(ctx)),
                tie_out_reason: TieOutReason::Restatement as i32,
                ..Default::default()
            },
        ));
    }
    // An immediate-or-cancel or fill-or-kill placement ended unfilled: the
    // order cancelled if its own time in force is one of these (row 21).
    let ends_unfilled = matches!(
        event,
        PlacementEvent::Cancelled | PlacementEvent::Expired | PlacementEvent::Rejected
    ) && book
        .ledger
        .placement_filled(&request.placement_id)?
        .is_zero();
    let immediate = |tif: i32| {
        tif == TimeInForce::ImmediateOrCancel as i32 || tif == TimeInForce::FillOrKill as i32
    };
    if ends_unfilled
        && immediate(placed.placement.time_in_force)
        && immediate(order.order.time_in_force)
        && matches!(from, OrderState::Released | OrderState::Working)
    {
        let mut ledger = book.ledger.clone();
        ledger.apply(&acts[0], "", &BTreeMap::new());
        if ledger.live_parts(&order_id)?.0.is_empty() && ledger.live_parts(&order_id)?.1.is_empty()
        {
            acts.push(Act::Ended {
                order_id: order_id.clone(),
                state: OrderState::Cancelled as i32,
            });
            rows.push("21");
        }
    }
    let plan = Plan::new("placement-reported", venue_actor(ctx), acts);
    let made = finish(book, head, ctx, plan)?;
    fact_reached(&made, &order_id, &rows, from);
    Ok(made)
}

// ── W11.11: an execution ────────────────────────────────────────────────────

/// One open lot as a sale may relieve it.
struct LotView {
    lot_id: String,
    open: Exact,
    acquired_date: String,
    unit_cost: Option<Exact>,
    ordinal: u64,
}

/// A position's open lots, by magnitude, with the lines already in this
/// entry applied (a superseded execution's reversal).
fn open_lots(book: &Book, key: &Key, earlier: &[MovementLine]) -> Result<Vec<LotView>> {
    let mut views: BTreeMap<String, LotView> = BTreeMap::new();
    if let Some(position) = book.positions.get(key) {
        for state in position.lots.values() {
            let open = numbers::required("lots.open_quantity", state.lot.open_quantity.as_ref())?;
            let terms = state.lot.terms.clone().unwrap_or_default();
            let original = numbers::required(
                "lots.original_quantity",
                state.lot.original_quantity.as_ref(),
            )?;
            let unit_cost = amount_of(terms.unit_cost.as_ref()).or_else(|| {
                // Cost per unit where only the lot's cost is known: compared,
                // never stored, so an inexact quotient is never kept.
                let cost = amount_of(terms.cost.as_ref())?;
                (!original.is_zero()).then(|| {
                    let scale = cost.scale().max(original.scale()) + 6;
                    let numerator =
                        cost.integer() * 10i128.pow(scale - cost.scale() + original.scale());
                    Exact::new(numerator / original.integer(), scale).unwrap_or(Exact::ZERO)
                })
            });
            views.insert(
                state.lot.lot_id.clone(),
                LotView {
                    lot_id: state.lot.lot_id.clone(),
                    open: if open.is_negative() {
                        open.negated()
                    } else {
                        open
                    },
                    acquired_date: terms.acquired_date.clone(),
                    unit_cost,
                    ordinal: state.ordinal,
                },
            );
        }
    }
    for line in earlier {
        if (line.instrument_id.as_str(), line.side, line.sleeve.as_str())
            != (key.0.as_str(), key.1, key.2.as_str())
            || line.lot_id.is_empty()
        {
            continue;
        }
        let quantity = numbers::required("line.quantity", line.quantity.as_ref())?;
        let magnitude_change = if key.1 == HoldingSide::Short as i32 {
            quantity.negated()
        } else {
            quantity
        };
        if let Some(view) = views.get_mut(&line.lot_id) {
            view.open = numbers::add("lot", view.open, magnitude_change)?;
        }
    }
    Ok(views
        .into_values()
        .filter(|view| !view.open.is_zero() && !view.open.is_negative())
        .collect())
}

/// Which lots a sale relieves, and how much of each: the lots the order
/// names first, then by its method or the account's default (W9.13,
/// W11.13), first in first out where neither names one. Average cost
/// relieves in acquisition order: its cost is the average, its lots are not
/// chosen by it.
fn relief(
    mut lots: Vec<LotView>,
    mut quantity: Exact,
    method: LotReliefMethod,
    named: &[String],
) -> Result<Vec<(String, Exact)>> {
    let key_fifo = |lot: &LotView| (lot.acquired_date.clone(), lot.ordinal);
    match method {
        LotReliefMethod::LastInFirstOut => {
            lots.sort_by_key(key_fifo);
            lots.reverse();
        }
        LotReliefMethod::HighestCost => lots.sort_by(|a, b| {
            b.unit_cost
                .cmp(&a.unit_cost)
                .then(key_fifo(a).cmp(&key_fifo(b)))
        }),
        LotReliefMethod::LowestCost => lots.sort_by(|a, b| {
            a.unit_cost
                .cmp(&b.unit_cost)
                .then(key_fifo(a).cmp(&key_fifo(b)))
        }),
        _ => lots.sort_by_key(key_fifo),
    }
    lots.sort_by_key(|lot| !named.contains(&lot.lot_id));
    let mut out = Vec::new();
    for lot in lots {
        if quantity.is_zero() {
            break;
        }
        let take = if lot.open < quantity {
            lot.open
        } else {
            quantity
        };
        quantity = numbers::add("relief", quantity, take.negated())?;
        out.push((lot.lot_id, take));
    }
    Ok(out)
}

/// The cash instrument an amount is in: the one it names, or its code's as
/// the book resolved it (contract v18). Unresolved, the fact cannot be
/// checked, and is refused to be sent again.
fn cash_instrument(money: &Money) -> Result<String> {
    if !money.instrument_id.is_empty() {
        return Ok(money.instrument_id.clone());
    }
    meridian_domain::money::known(&money.currency_code).ok_or_else(|| {
        StoreError::refused(
            RefusalReason::ReferenceUnavailable,
            format!(
                "{}'s cash instrument is not resolved yet, so the cash this moves could not be \
                 recorded; nothing was recorded, and the same fact may be sent again",
                money.currency_code
            ),
        )
    })
}

/// The lot relief method a fill on `order_id` relieves by: its own, its
/// parents', or the account's default.
fn relief_method(book: &Book, order_id: &str) -> (LotReliefMethod, Vec<String>) {
    let mut current = order_id.to_string();
    let mut seen = BTreeSet::new();
    while let Some(held) = book.ledger.orders.get(&current) {
        if held.order.lot_relief_method != LotReliefMethod::Unspecified as i32
            || !held.order.relieve_lot_ids.is_empty()
        {
            return (
                LotReliefMethod::try_from(held.order.lot_relief_method)
                    .unwrap_or(LotReliefMethod::Unspecified),
                held.order.relieve_lot_ids.clone(),
            );
        }
        match held.order.link.as_ref() {
            Some(link) if seen.insert(link.parent_order_id.clone()) => {
                current = link.parent_order_id.clone();
            }
            _ => break,
        }
    }
    (
        LotReliefMethod::try_from(attributes_of(book).lot_relief_default)
            .unwrap_or(LotReliefMethod::Unspecified),
        Vec::new(),
    )
}

/// What a reported fill moves (W11.13; the order-path plan's Q15): a buy
/// covers a short first and opens a lot at the fill's price for the rest; a
/// sale relieves lots and opens a short for what it sells beyond them; a
/// short sale opens a short lot. The security and the cash pend to the
/// value date; each fee is a cash line of its own.
fn trade_lines(
    book: &Book,
    ctx: &Context,
    order_id: &str,
    execution: &Execution,
    earlier: &[MovementLine],
) -> Result<Vec<MovementLine>> {
    let quantity = numbers::required("quantity", execution.quantity.as_ref())?;
    let price = execution
        .price
        .clone()
        .ok_or_else(|| malformed(&["price"], "a fill states its price"))?;
    let unit = numbers::amount("price", &price)?;
    let value_date = if execution.settlement_date.is_empty() {
        execution.computed_settlement_date.clone()
    } else {
        execution.settlement_date.clone()
    };
    let sleeve = execution.sleeve.clone();
    let instrument = execution.instrument_id.clone();
    let line = |side: HoldingSide, quantity: Exact| MovementLine {
        instrument_id: instrument.clone(),
        side: side as i32,
        bucket: SettlementBucket::Pending as i32,
        value_date: value_date.clone(),
        quantity: numbers::wire(quantity),
        sleeve: sleeve.clone(),
        ..Default::default()
    };
    let opening = |side: HoldingSide, magnitude: Exact| -> Result<MovementLine> {
        let mut opened = line(
            side,
            if side == HoldingSide::Short {
                magnitude.negated()
            } else {
                magnitude
            },
        );
        opened.lot_id = ctx.mint(ids::LOT);
        opened.opens_lot = Some(LotTerms {
            unit_cost: Some(price.clone()),
            cost: Some(Money {
                amount: numbers::wire(orders::multiply("cost", magnitude, unit)?),
                ..price.clone()
            }),
            acquired_date: execution.trade_date.clone(),
            holding_period_start: String::new(),
            settlement_date: value_date.clone(),
            source: LotSource::Execution as i32,
        });
        Ok(opened)
    };
    let side = Side::try_from(execution.side).unwrap_or(Side::Unspecified);
    let (method, named) = relief_method(book, order_id);
    let mut lines = Vec::new();
    match side {
        Side::Buy => {
            let short_key: Key = (
                instrument.clone(),
                HoldingSide::Short as i32,
                sleeve.clone(),
            );
            let shorts = open_lots(book, &short_key, earlier)?;
            let mut rest = quantity;
            for (lot_id, take) in relief(shorts, quantity, method, &named)? {
                let mut covering = line(HoldingSide::Short, take);
                covering.lot_id = lot_id;
                lines.push(covering);
                rest = numbers::add("cover", rest, take.negated())?;
            }
            if !rest.is_zero() {
                lines.push(opening(HoldingSide::Long, rest)?);
            }
        }
        Side::Sell => {
            let long_key: Key = (instrument.clone(), HoldingSide::Long as i32, sleeve.clone());
            let longs = open_lots(book, &long_key, earlier)?;
            let mut rest = quantity;
            for (lot_id, take) in relief(longs, quantity, method, &named)? {
                let mut selling = line(HoldingSide::Long, take.negated());
                selling.lot_id = lot_id;
                lines.push(selling);
                rest = numbers::add("sale", rest, take.negated())?;
            }
            if !rest.is_zero() {
                lines.push(opening(HoldingSide::Short, rest)?);
            }
        }
        Side::SellShort | Side::SellShortExempt => {
            lines.push(opening(HoldingSide::Short, quantity)?)
        }
        Side::Unspecified => return Err(malformed(&["side"], "the order names no side")),
    }
    // The cash, pending to the value date: paid on a buy, received on a sale.
    let notional = orders::multiply("cash", quantity, unit)?;
    let cash = cash_instrument(&price)?;
    let signed = if side == Side::Buy {
        notional.negated()
    } else {
        notional
    };
    lines.push(MovementLine {
        instrument_id: cash,
        side: HoldingSide::Long as i32,
        bucket: SettlementBucket::Pending as i32,
        value_date: value_date.clone(),
        quantity: numbers::wire(signed),
        sleeve: sleeve.clone(),
        ..Default::default()
    });
    for (index, fee) in execution.fees.iter().enumerate() {
        lines.push(fee_line(
            &format!("fees[{index}]"),
            fee,
            &value_date,
            &sleeve,
        )?);
    }
    Ok(lines)
}

/// A fee as a cash line: signed as the contract signs it, a rebate negative,
/// so a fee paid takes cash and a rebate adds it (requirement 26).
fn fee_line(
    field: &str,
    fee: &meridian_domain::v1::Fee,
    value_date: &str,
    sleeve: &str,
) -> Result<MovementLine> {
    let amount = fee
        .amount
        .as_ref()
        .ok_or_else(|| malformed(&[field], "a fee states its amount"))?;
    let value = numbers::amount(field, amount)?;
    Ok(MovementLine {
        instrument_id: cash_instrument(amount)?,
        side: HoldingSide::Long as i32,
        bucket: if value_date.is_empty() {
            SettlementBucket::Settled as i32
        } else {
            SettlementBucket::Pending as i32
        },
        value_date: value_date.to_string(),
        quantity: numbers::wire(value.negated()),
        sleeve: sleeve.to_string(),
        ..Default::default()
    })
}

/// A superseded execution's lines, negated: the position it moved reversed
/// by the superseding entry (W11.13).
fn reversal_lines(book: &Book, superseded: &str) -> Result<Vec<MovementLine>> {
    let Some(held) = book.ledger.executions.get(superseded) else {
        return Ok(Vec::new());
    };
    if held.superseded_by.is_some() {
        return Ok(Vec::new());
    }
    // An accepted execution moved its position by the accept's entry, its
    // own lines alone of the several the accept may have moved (W11.23).
    let (entry_id, range) = match &held.accepted_lines {
        Some((entry_id, start, count)) => (entry_id, Some((*start, *count))),
        None => (&held.entry_id, None),
    };
    let Some(entry) = book.entry(entry_id) else {
        return Ok(Vec::new());
    };
    let lines: &[MovementLine] = match range {
        Some((start, count)) => entry
            .body
            .lines
            .get(start..start + count)
            .unwrap_or_default(),
        None => &entry.body.lines,
    };
    Ok(lines
        .iter()
        .map(|line| {
            let mut negated = line.clone();
            negated.opens_lot = None;
            negated.quantity = line.quantity.as_ref().map(|quantity| {
                Exact::from_wire(quantity)
                    .map(|value| value.negated().to_wire())
                    .unwrap_or(*quantity)
            });
            negated
        })
        .collect())
}

fn date_field(field: &str, text: &str) -> Result<()> {
    if text.is_empty() || crate::dates::is_date(text) {
        return Ok(());
    }
    Err(malformed(
        &[field],
        format!("{field} is {text:?}; a date is YYYY-MM-DD"),
    ))
}

pub fn report_execution(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &ReportExecutionRequest,
) -> Result<Made> {
    let entered = !ctx.acting_for.is_empty();
    let kind = ExecutionKind::try_from(request.kind).unwrap_or(ExecutionKind::Unspecified);
    if kind == ExecutionKind::Manual && !entered {
        return Err(StoreError::refused(
            RefusalReason::ActorRequired,
            "a manual execution is entered for a person, and this was sent for no one",
        ));
    }
    let quantity = decimal("quantity", request.quantity.as_ref())?
        .ok_or_else(|| malformed(&["quantity"], "an execution states its quantity"))?;
    if quantity.is_negative() {
        return Err(malformed(
            &["quantity"],
            "an execution's quantity is never below zero",
        ));
    }
    let bust = !request.supersedes.is_empty() && quantity.is_zero();
    if !bust && quantity.is_zero() {
        return Err(malformed(
            &["quantity"],
            "a fill states its quantity; a bust names what it supersedes",
        ));
    }
    date_field("trade_date", &request.trade_date)?;
    date_field("settlement_date", &request.settlement_date)?;
    date_field(
        "computed_settlement_date",
        &request.computed_settlement_date,
    )?;
    date_field("effective_date", &request.effective_date)?;
    // A fill with no settlement date, stated or computed, is malformed (the
    // refresh's Q39): the plugin asks the date read and sends it again.
    if !bust && request.settlement_date.is_empty() && request.computed_settlement_date.is_empty() {
        return Err(malformed(
            &["settlement_date"],
            "a fill states its settlement date, or the date read's beside it where the venue \
             states none; the book never guesses one",
        ));
    }
    if !bust && request.price.is_none() {
        return Err(malformed(&["price"], "a fill states its price"));
    }

    // Placed on its placement or, done away, its order, in this account.
    let (order_id, placement_id) = if !request.placement_id.is_empty() {
        match book.ledger.placements.get(&request.placement_id) {
            Some(placed)
                if entered || placed.placement.connectivity_instance_id == ctx.instance_id =>
            {
                (
                    placed.placement.order_id.clone(),
                    request.placement_id.clone(),
                )
            }
            _ if entered => {
                return Err(StoreError::Invalid(format!(
                    "account {} holds no placement {}",
                    book.account_id, request.placement_id
                )))
            }
            _ => {
                return unplaced(
                    book,
                    head,
                    ctx,
                    unplaced_fact::Fact::Execution(request.clone()),
                    UnplacedReason::UnknownPlacement,
                    &request.venue_account_code,
                    request.raw_record.clone(),
                )
            }
        }
    } else if !request.order_id.is_empty() {
        match book.ledger.orders.get(&request.order_id) {
            Some(_) => (request.order_id.clone(), String::new()),
            None if entered => {
                return Err(StoreError::Invalid(format!(
                    "account {} holds no order {}",
                    book.account_id, request.order_id
                )))
            }
            None => {
                return unplaced(
                    book,
                    head,
                    ctx,
                    unplaced_fact::Fact::Execution(request.clone()),
                    UnplacedReason::UnknownPlacement,
                    &request.venue_account_code,
                    request.raw_record.clone(),
                )
            }
        }
    } else if !entered {
        // A venue's fill on no order and no placement (contract v23).
        return report_unmatched(book, head, ctx, request, bust);
    } else {
        return Err(malformed(
            &["placement_id"],
            "an entered execution names its placement, or, done away, its order",
        ));
    };
    let order = held(book, &order_id)?;
    let sender = ctx.instance_id.clone();
    let mut execution = Execution {
        order_id: order_id.clone(),
        placement_id: placement_id.clone(),
        instrument_id: order.order.instrument_id.clone(),
        sleeve: order.order.sleeve.clone(),
        side: order.order.side,
        ..execution_of(book, ctx, request, entered)
    };
    // A repeat of a fact already applied, by the venue's identifiers, is
    // recognised and answered as it stands (requirement 14).
    if !execution.venue_execution_id.is_empty() {
        let seen = orders::venue_key(&sender, &execution);
        if let Some(earlier) = book.ledger.venue_facts.get(&seen) {
            let held = &book.ledger.executions[earlier];
            execution = held.execution.clone();
            return Err(StoreError::Repeated(repeated_fact(book, &execution)?));
        }
    }
    let from = state_of(book, &order_id)?;
    let mut lines = Vec::new();
    if !execution.supersedes.is_empty() {
        lines.extend(reversal_lines(book, &execution.supersedes)?);
    }
    if !bust && orders::moves_position(&execution) {
        let own = trade_lines(book, ctx, &order_id, &execution, &lines)?;
        lines.extend(own);
    }
    let mut acts = vec![Act::Executed {
        execution: execution.clone(),
        sender: sender.clone(),
        venue_account_code: request.venue_account_code.clone(),
        venue_client_order_id: request.venue_client_order_id.clone(),
    }];
    // A fill after the order ended: accepted, recorded, late fact (row 12).
    if ended(from) {
        acts.push(condition(
            &order_id,
            true,
            StandingOrderCondition {
                condition: OrderCondition::LateFact as i32,
                reason: format!(
                    "execution {} arrived after the order was {}",
                    execution.execution_id,
                    state_named(from)
                ),
                actor: Some(fact_actor(ctx)),
                ..Default::default()
            },
        ));
    }
    acts.extend(claimed_unplaced(book, &sender, &execution));
    let mut plan =
        Plan::new("execution-recorded", fact_actor(ctx), acts).key(&request.idempotency_key);
    plan.lines = lines;
    plan.effective_date = if !request.effective_date.is_empty() {
        request.effective_date.clone()
    } else {
        request.trade_date.clone()
    };
    let made = finish(book, head, ctx, plan)?;
    fact_reached(
        &made,
        &order_id,
        &["10", "11", "12", "13", "17", "27", "28", "29", "30", "31"],
        from,
    );
    Ok(made)
}

/// An execution as the request reports it, minted, on no order or placement
/// yet: the caller places it.
fn execution_of(
    book: &Book,
    ctx: &Context,
    request: &ReportExecutionRequest,
    entered: bool,
) -> Execution {
    Execution {
        execution_id: ctx.mint(ids::EXECUTION),
        account_id: book.account_id.clone(),
        instrument_id: request.instrument_id.clone(),
        side: request.side,
        quantity: request.quantity,
        price: request.price.clone(),
        trade_time_ns: request.trade_time_ns,
        trade_date: request.trade_date.clone(),
        venue_execution_id: request.venue_execution_id.clone(),
        kind: request.kind,
        source: request.source,
        finality: request.finality,
        fees: request.fees.clone(),
        settlement_date: request.settlement_date.clone(),
        computed_settlement_date: request.computed_settlement_date.clone(),
        settlement_date_basis: request.settlement_date_basis.clone(),
        supersedes: request.supersedes.clone(),
        venue_id: request.venue_id.clone(),
        uti: request.uti.clone(),
        prior_uti: request.prior_uti.clone(),
        upi: request.upi.clone(),
        given_up_to: request.given_up_to.clone(),
        trade_chain_reference: request.trade_chain_reference.clone(),
        settlement_counterparty: request.settlement_counterparty.clone(),
        person_roles: entered.then(|| PersonRoles {
            executed_by: Some(ctx.actor()),
            ..Default::default()
        }),
        raw_record: request.raw_record.clone(),
        effective_date: request.effective_date.clone(),
        counterparty: request.counterparty.clone(),
        liquidity_indicator: request.liquidity_indicator,
        uti_generated_by: request.uti_generated_by.clone(),
        // The venue's order ID as the venue states it (contract v23).
        venue_order_id: request.venue_order_id.clone(),
        ..Default::default()
    }
}

/// A later record claiming an unplaced fact of the same venue execution.
fn claimed_unplaced(book: &Book, sender: &str, execution: &Execution) -> Option<Act> {
    if execution.venue_execution_id.is_empty() {
        return None;
    }
    book.ledger
        .unplaced_executions
        .get(&key_of(&[sender, &execution.venue_execution_id]))
        .map(|unplaced_id| Act::Placed {
            unplaced_id: unplaced_id.clone(),
        })
}

/// A row of the execution's transitions (matrix/transitions.tsv's e-rows),
/// by its key.
pub fn execution_row(id: &str) -> &'static Transition<ExecutionState> {
    EXECUTION_TRANSITIONS
        .iter()
        .find(|row| row.row == id)
        .unwrap_or_else(|| panic!("the transition table names no execution row {id}"))
}

/// Whether an execution's row starts from `from` (None: one being recorded).
pub fn execution_starts_from(
    row: &Transition<ExecutionState>,
    from: Option<ExecutionState>,
) -> bool {
    row.from.iter().any(|held| match held {
        From::Any => true,
        From::None => from.is_none(),
        From::State(state) => Some(*state) == from,
    })
}

/// Whether an execution's row takes `from` to `to`.
pub fn execution_admits(
    row: &Transition<ExecutionState>,
    from: Option<ExecutionState>,
    to: ExecutionState,
) -> bool {
    execution_starts_from(row, from)
        && row.to.iter().any(|held| match held {
            To::Unchanged => Some(to) == from,
            To::State(state) => *state == to,
        })
}

pub fn execution_state_named(state: ExecutionState) -> &'static str {
    match state {
        ExecutionState::Unmatched => "unmatched",
        ExecutionState::Matched => "matched",
        ExecutionState::DontKnow => "answered DK",
        ExecutionState::Unspecified => "unknown to the book",
    }
}

/// A venue's fact on an execution held to its rows, logged where it does not
/// fit and never refused for it (requirement 12).
fn execution_fact_reached(
    made: &Made,
    execution_id: &str,
    rows: &[&str],
    from: Option<ExecutionState>,
) {
    let Some(to) = made
        .book
        .ledger
        .executions
        .get(execution_id)
        .and_then(|held| ExecutionState::try_from(held.execution.state).ok())
    else {
        return;
    };
    if rows
        .iter()
        .any(|id| execution_admits(execution_row(id), from, to))
    {
        return;
    }
    tracing::warn!(
        execution_id,
        to = execution_state_named(to),
        rows = rows.join(","),
        "a venue's fact took an execution where the table does not; recorded, never refused"
    );
}

/// W11.11 from contract v23 (rulings 1, 8, 9, 18): a venue's fill on no
/// order and no placement, its account stamped from the `ems`'s map,
/// recorded as an unmatched execution moving nothing (transition e2) until
/// an `ems` accepts it (W11.23) or answers it DK (W11.24). It names its
/// instrument, its side and the venue's order ID, each refused
/// INVALID_ARGUMENT by name when missing; a bust takes them from the fill it
/// busts. A correction of an unmatched fill supersedes it and is unmatched
/// in its place (e5); a bust supersedes it, which stays unmatched with
/// nothing to answer (e6). One superseding a fill on a placement names that
/// placement, as in v22.
fn report_unmatched(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &ReportExecutionRequest,
    bust: bool,
) -> Result<Made> {
    let superseded = (!request.supersedes.is_empty())
        .then(|| book.ledger.executions.get(&request.supersedes))
        .flatten();
    if superseded.is_some_and(|held| held.execution.state == ExecutionState::Matched as i32) {
        return Err(malformed(
            &["placement_id"],
            format!(
                "execution {} is on a placement: a bust or correction of it names that placement",
                request.supersedes
            ),
        ));
    }
    let sender = ctx.instance_id.clone();
    let mut execution = execution_of(book, ctx, request, false);
    if bust {
        if let Some(held) = superseded {
            if execution.instrument_id.is_empty() {
                execution.instrument_id = held.execution.instrument_id.clone();
            }
            if execution.side == Side::Unspecified as i32 {
                execution.side = held.execution.side;
            }
            if execution.venue_order_id.is_empty() {
                execution.venue_order_id = held.execution.venue_order_id.clone();
            }
        }
    }
    let mut missing = Vec::new();
    if execution.instrument_id.is_empty() {
        missing.push("instrument_id");
    }
    if execution.side == Side::Unspecified as i32 || Side::try_from(execution.side).is_err() {
        missing.push("side");
    }
    if execution.venue_order_id.is_empty() {
        missing.push("venue_order_id");
    }
    if !missing.is_empty() {
        return Err(malformed(
            &missing,
            format!(
                "a fill on no placement names its instrument, its side and the venue's order ID \
                 (contract v23); this left out {}",
                missing.join(", ")
            ),
        ));
    }
    if !execution.venue_execution_id.is_empty() {
        let seen = orders::venue_key(&sender, &execution);
        if let Some(earlier) = book.ledger.venue_facts.get(&seen) {
            let held = book.ledger.executions[earlier].execution.clone();
            return Err(StoreError::Repeated(repeated_fact(book, &held)?));
        }
    }
    let from = superseded.and_then(|held| ExecutionState::try_from(held.execution.state).ok());
    let execution_id = execution.execution_id.clone();
    let mut acts = vec![Act::Executed {
        execution: execution.clone(),
        sender: sender.clone(),
        venue_account_code: request.venue_account_code.clone(),
        venue_client_order_id: request.venue_client_order_id.clone(),
    }];
    acts.extend(claimed_unplaced(book, &sender, &execution));
    let mut plan =
        Plan::new("execution-recorded", fact_actor(ctx), acts).key(&request.idempotency_key);
    plan.effective_date = if !request.effective_date.is_empty() {
        request.effective_date.clone()
    } else {
        request.trade_date.clone()
    };
    let made = finish(book, head, ctx, plan)?;
    match from {
        None => execution_fact_reached(&made, &execution_id, &["e2"], None),
        Some(_) => {
            let superseded_id = request.supersedes.as_str();
            let rows: &[&str] = if bust { &["e6", "e7"] } else { &["e5", "e7"] };
            execution_fact_reached(&made, superseded_id, rows, from);
        }
    }
    Ok(made)
}

/// A repeated fact's answer: its records as they stand.
fn repeated_fact(book: &Book, execution: &Execution) -> Result<Vec<u8>> {
    use prost::Message;
    let mut reply = meridian_domain::v1::FactReply {
        execution: Some(execution.clone()),
        order: book
            .ledger
            .orders
            .contains_key(&execution.order_id)
            .then(|| book.ledger.order_record(&execution.order_id, false))
            .transpose()?,
        placement: book
            .ledger
            .placements
            .contains_key(&execution.placement_id)
            .then(|| book.ledger.placement_record(&execution.placement_id))
            .transpose()?,
        journal: execution.last_change.clone(),
        ..Default::default()
    };
    meridian_domain::money::fill_known(&mut reply);
    Ok(reply.encode_to_vec())
}

// ── W11.12: a charge ────────────────────────────────────────────────────────

pub fn report_charge(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &ReportChargeRequest,
) -> Result<Made> {
    let fee = request
        .fee
        .clone()
        .ok_or_else(|| malformed(&["fee"], "a charge states its fee"))?;
    date_field("trade_date", &request.trade_date)?;
    date_field("settlement_date", &request.settlement_date)?;
    let Some(placed) = book.ledger.placements.get(&request.placement_id) else {
        return unplaced(
            book,
            head,
            ctx,
            unplaced_fact::Fact::Charge(request.clone()),
            UnplacedReason::UnknownPlacement,
            "",
            request.raw_record.clone(),
        );
    };
    if placed.placement.connectivity_instance_id != ctx.instance_id {
        return Err(StoreError::Unplace {
            reason: UnplacedReason::UnknownPlacement as i32,
            words: format!(
                "placement {} is not {}'s",
                request.placement_id, ctx.instance_id
            ),
        });
    }
    if !fact_in_scope(ctx, &placed.placement.account_id) {
        return Err(StoreError::Unplace {
            reason: UnplacedReason::UnmappedVenueAccount as i32,
            words: format!(
                "placement {}'s account is outside {}'s scope",
                request.placement_id, ctx.instance_id
            ),
        });
    }
    let order = held(book, &placed.placement.order_id)?;
    let value_date = if request.settlement_date.is_empty() {
        request.trade_date.clone()
    } else {
        request.settlement_date.clone()
    };
    let charge = Charge {
        charge_id: ctx.mint(ids::CHARGE),
        placement_id: request.placement_id.clone(),
        order_id: placed.placement.order_id.clone(),
        account_id: book.account_id.clone(),
        fee: Some(fee.clone()),
        trade_date: request.trade_date.clone(),
        settlement_date: request.settlement_date.clone(),
        raw_record: request.raw_record.clone(),
    };
    let mut plan = Plan::new(
        "charge-recorded",
        venue_actor(ctx),
        vec![Act::Charged { charge }],
    );
    plan.lines = vec![fee_line("fee", &fee, &value_date, &order.order.sleeve)?];
    plan.effective_date = request.trade_date.clone();
    finish(book, head, ctx, plan)
}

// ── W11.13: an unplaced fact; expiry ────────────────────────────────────────

/// A well-formed fact the book cannot place, kept with its reason, never
/// dropped (requirement 13): in the account it names, or naming none.
pub fn unplaced(
    book: &Book,
    head: u64,
    ctx: &Context,
    fact: unplaced_fact::Fact,
    reason: UnplacedReason,
    venue_account_code: &str,
    raw_record: Option<meridian_pb::v1::RawRecordRef>,
) -> Result<Made> {
    let fact_kind = match &fact {
        unplaced_fact::Fact::Execution(_) => "execution",
        unplaced_fact::Fact::Placement(_) => "placement",
        unplaced_fact::Fact::Charge(_) => "charge",
        unplaced_fact::Fact::Locate(_) => "locate",
    };
    // A repeat of an unplaced execution is the same fact, kept once.
    if let unplaced_fact::Fact::Execution(execution) = &fact {
        if !execution.venue_execution_id.is_empty()
            && book
                .ledger
                .unplaced_executions
                .contains_key(&key_of(&[&ctx.instance_id, &execution.venue_execution_id]))
        {
            let id = &book.ledger.unplaced_executions
                [&key_of(&[&ctx.instance_id, &execution.venue_execution_id])];
            let kept = book.ledger.unplaced[id].clone();
            use prost::Message;
            return Err(StoreError::Repeated(
                meridian_domain::v1::FactReply {
                    journal: kept.last_change.clone(),
                    unplaced: Some(kept),
                    ..Default::default()
                }
                .encode_to_vec(),
            ));
        }
    }
    let record = UnplacedFact {
        unplaced_id: ctx.mint(ids::UNPLACED),
        account_id: if is_account(&book.account_id) {
            book.account_id.clone()
        } else {
            String::new()
        },
        fact_kind: fact_kind.to_string(),
        reason: reason as i32,
        raw_record,
        venue_account_code: venue_account_code.to_string(),
        fact: Some(fact),
        placed_by: None,
        last_change: None,
    };
    let plan = Plan::new(
        "unplaced-fact-recorded",
        fact_actor(ctx),
        vec![Act::Unplaced {
            fact: record,
            sender: ctx.instance_id.clone(),
        }],
    );
    finish(book, head, ctx, plan)
}

/// The book's own act (row i4): an order whose time in force ended by the
/// deployment's clock, its live placements and children asked to cancel
/// first, expired when none is live. Actor the system.
pub fn expire(book: &Book, head: u64, ctx: &Context, order_id: &str) -> Result<Option<Made>> {
    let Some(held) = book.ledger.orders.get(order_id) else {
        return Ok(None);
    };
    let state = state_of(book, order_id)?;
    if orders::due_at(held).is_none() {
        return Ok(None);
    }
    // A day order never released expires at its day's end (row i7), its
    // awaiting check or approval ending with it.
    if state == OrderState::Proposed && starts_from(row("i7"), Some(state)) {
        let plan = Plan::new(
            "order-expired",
            ctx.actor(),
            vec![Act::Ended {
                order_id: order_id.to_string(),
                state: OrderState::Expired as i32,
            }],
        )
        .reason("a day order never released: its day ended by the deployment's clock");
        return Ok(Some(finish(book, head, ctx, plan)?));
    }
    if !starts_from(row("i4"), Some(state)) {
        return Ok(None);
    }
    let mut acts = Vec::new();
    cancel_parts(
        book,
        ctx,
        order_id,
        OrderState::Expired,
        "its time in force ended",
        &mut acts,
    )?;
    acts.push(Act::Ending {
        order_id: order_id.to_string(),
        state: OrderState::Expired as i32,
    });
    let plan = Plan::new("order-expiring", ctx.actor(), acts)
        .reason("its time in force ended by the deployment's clock");
    Ok(Some(finish(book, head, ctx, plan)?))
}

// ── W11.14: acknowledge ─────────────────────────────────────────────────────

pub fn acknowledge_flag(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &AcknowledgeFlagRequest,
) -> Result<Made> {
    let order = held(book, &request.order_id)?;
    in_scope(ctx, &order.order.account_id)?;
    user_required(ctx, "acknowledging a flag")?;
    let state = state_of(book, &request.order_id)?;
    match OrderCondition::try_from(request.condition) {
        Ok(OrderCondition::LateFact) => {}
        Ok(OrderCondition::DoesNotTieOut) => {
            return Err(StoreError::refused(
                RefusalReason::OrderState,
                "does not tie out is never acknowledged: an alignment, a booking to an error \
                 account or a bust clears it (Q21)",
            ))
        }
        _ => {
            return Err(malformed(
                &["condition"],
                "a late fact is the flag a person acknowledges",
            ))
        }
    }
    if !order
        .conditions
        .contains_key(&(OrderCondition::LateFact as i32))
    {
        return Err(StoreError::refused(
            RefusalReason::OrderState,
            format!(
                "order {} holds no late fact to acknowledge; it is {}",
                request.order_id,
                state_named(state)
            ),
        ));
    }
    let plan = Plan::new(
        "flag-acknowledged",
        ctx.actor(),
        vec![condition(
            &request.order_id,
            false,
            standing(ctx, OrderCondition::LateFact, request.reason.clone()),
        )],
    )
    .reason(&request.reason)
    .key(&request.idempotency_key);
    finish(book, head, ctx, plan)
}

// ── W11.15: a trading instruction ───────────────────────────────────────────

/// The setter's level an instruction is recorded at (W11.15; approved
/// 2026-10-12): stamped by the dashboard from the setter's claims, never
/// entered. A plugin admin's names their plugin, alone or beside a person,
/// service account or account within it; All accounts is a deployment
/// admin's alone.
pub fn setter_level(request: &SetTradingInstructionRequest) -> Result<SetterLevel> {
    let level = SetterLevel::try_from(request.setter_level).unwrap_or(SetterLevel::Unspecified);
    let target = request.target.clone().unwrap_or_default();
    match level {
        SetterLevel::Unspecified => Err(malformed(
            &["setter_level"],
            "an instruction is set at a level the dashboard stamps from the setter's claims, \
             and this names none",
        )),
        SetterLevel::PluginAdmin if target.instance_id.is_empty() || target.all_accounts => {
            Err(malformed(
                &["target"],
                "a plugin admin's instruction names their plugin, alone or beside a person, \
                 service account or account within it",
            ))
        }
        level => Ok(level),
    }
}

/// The key an instruction stands under: its target and its level, so one
/// set at one level is lifted only at that level.
pub fn instruction_key(target: &TradingInstructionTarget, level: SetterLevel) -> String {
    key_of(&[
        level.as_str_name(),
        if target.all_accounts { "all" } else { "" },
        &target.account_id,
        &target.instance_id,
        &target.subject,
        &target.service_account_id,
        &target.user_group_id,
    ])
}

/// The target's shape held to what a setter may name.
pub fn check_target(target: &TradingInstructionTarget) -> Result<()> {
    let primary = [target.all_accounts, !target.user_group_id.is_empty()]
        .iter()
        .filter(|named| **named)
        .count();
    let within = [
        !target.account_id.is_empty(),
        !target.subject.is_empty(),
        !target.service_account_id.is_empty(),
    ]
    .iter()
    .filter(|named| **named)
    .count();
    let plugin = !target.instance_id.is_empty();
    let fits = matches!(
        (primary, within, plugin),
        (1, 0, false) | (0, 1, _) | (0, 0, true)
    );
    if !fits {
        return Err(malformed(
            &["target"],
            "an instruction names one target: an account, All accounts, a plugin, a person, a \
             service account or a user group; within a plugin, a person, service account or \
             account beside it",
        ));
    }
    Ok(())
}

pub fn set_trading_instruction(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &SetTradingInstructionRequest,
) -> Result<Made> {
    user_required(ctx, "setting a trading instruction")?;
    if request.reason.trim().is_empty() {
        return Err(StoreError::refused(
            RefusalReason::ReasonRequired,
            "a trading instruction is set with its reason, and this carries none",
        ));
    }
    let target = request
        .target
        .clone()
        .ok_or_else(|| malformed(&["target"], "an instruction names its target"))?;
    check_target(&target)?;
    // A user group is not resolved to its persons in v22 (covers carried
    // empty; v27 resolves groups), so an instruction on one is refused
    // naming the field rather than standing unapplied (the product owner,
    // 2026-10-12).
    if !target.user_group_id.is_empty() {
        return Err(malformed(
            &["target.user_group_id"],
            "an instruction on a user group is not taken in v22: its persons are not resolved \
             until v27; set it on each person, service account, plugin or account instead",
        ));
    }
    if request.value == TradingInstructionValue::Unspecified as i32
        || TradingInstructionValue::try_from(request.value).is_err()
    {
        return Err(malformed(
            &["value"],
            "an instruction is active, pause or stop",
        ));
    }
    let level = setter_level(request)?;
    let key = instruction_key(&target, level);
    let instruction = TradingInstruction {
        target: Some(target),
        value: request.value,
        setter_level: level as i32,
        covers: Vec::new(),
        reason: request.reason.clone(),
        entry: None,
        last_change: None,
    };
    let plan = Plan::new(
        "trading-instruction-set",
        ctx.actor(),
        vec![Act::Instructed { key, instruction }],
    )
    .reason(&request.reason);
    finish(book, head, ctx, plan)
}

// ── W11.18, W11.19: locates ─────────────────────────────────────────────────

pub fn record_locate(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &RecordLocateRequest,
) -> Result<Made> {
    if !request.locate_record_id.is_empty() {
        let Some(_) = book.ledger.locates.get(&request.locate_record_id) else {
            return Err(StoreError::Invalid(format!(
                "account {} holds no locate {}",
                book.account_id, request.locate_record_id
            )));
        };
        let answer = request.answer.clone().ok_or_else(|| {
            malformed(
                &["answer"],
                "an answer on a locate states the prime's answer",
            )
        })?;
        let plan = Plan::new(
            "locate-answered",
            fact_actor(ctx),
            vec![Act::LocateAnswered {
                locate_record_id: request.locate_record_id.clone(),
                answer,
            }],
        )
        .key(&request.idempotency_key);
        return finish(book, head, ctx, plan);
    }
    if request.instrument_id.is_empty() {
        return Err(malformed(
            &["instrument_id"],
            "a locate names its instrument",
        ));
    }
    match decimal("quantity_requested", request.quantity_requested.as_ref())? {
        Some(quantity) if !quantity.is_zero() && !quantity.is_negative() => {}
        _ => {
            return Err(malformed(
                &["quantity_requested"],
                "a locate states the quantity asked",
            ))
        }
    }
    let locate = meridian_domain::v1::Locate {
        locate_record_id: ctx.mint(ids::LOCATE),
        account_id: book.account_id.clone(),
        instrument_id: request.instrument_id.clone(),
        quantity_requested: request.quantity_requested,
        answer: request.answer.clone(),
        locate_id: request
            .answer
            .as_ref()
            .map(|answer| answer.locate_id.clone())
            .unwrap_or_default(),
        raw_record: request.raw_record.clone(),
        ..Default::default()
    };
    let plan = Plan::new(
        "locate-recorded",
        fact_actor(ctx),
        vec![Act::LocateOpened { locate }],
    )
    .key(&request.idempotency_key);
    finish(book, head, ctx, plan)
}

pub fn record_locate_use(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &RecordLocateUseRequest,
) -> Result<Made> {
    let held = book
        .ledger
        .locates
        .get(&request.locate_record_id)
        .ok_or_else(|| {
            StoreError::Invalid(format!(
                "account {} holds no locate {}",
                book.account_id, request.locate_record_id
            ))
        })?;
    in_scope(ctx, &held.locate.account_id)?;
    user_required(ctx, "recording a locate's use")?;
    let used = request
        .r#use
        .clone()
        .ok_or_else(|| malformed(&["use"], "a use says what it is"))?;
    if used.kind == LocateUseKind::Unspecified as i32 || LocateUseKind::try_from(used.kind).is_err()
    {
        return Err(malformed(
            &["use.kind"],
            "a use reserves, gives back or consumes",
        ));
    }
    match decimal("use.quantity", used.quantity.as_ref())? {
        Some(quantity) if !quantity.is_negative() => {}
        _ => return Err(malformed(&["use.quantity"], "a use states its quantity")),
    }
    let expires = held
        .locate
        .answer
        .as_ref()
        .map(|answer| answer.expires_at_ns)
        .unwrap_or(0);
    if expires != 0 && expires <= ctx.received_at_ns {
        return Err(StoreError::refused(
            RefusalReason::OrderState,
            format!("locate {} expired", request.locate_record_id),
        ));
    }
    let plan = Plan::new(
        "locate-used",
        ctx.actor(),
        vec![Act::LocateUsed {
            locate_record_id: request.locate_record_id.clone(),
            used,
        }],
    )
    .key(&request.idempotency_key);
    finish(book, head, ctx, plan)
}

// ── W11.23, W11.24: the `ems` answers an unmatched execution ───────────────

/// The executions an accept or a DK names, each held in this account's book,
/// within the write scope the command was sent with: 1 to 500, each once.
/// One the book holds in another account is REFUSAL_REASON_EXECUTIONS_DIFFER
/// on an accept (one account per accept), and named as not held otherwise.
fn executions_named<'a>(
    book: &'a Book,
    ctx: &Context,
    ids: &[String],
    bound: meridian_pb::bounds::Count,
    elsewhere: &dyn Fn(&str) -> Option<String>,
    differ: bool,
) -> Result<Vec<&'a orders::ExecutionHeld>> {
    if ids.len() < bound.least || ids.len() > bound.most {
        return Err(malformed(
            &["execution_ids"],
            format!(
                "names {} executions; it names {} to {}",
                ids.len(),
                bound.least,
                bound.most
            ),
        ));
    }
    let mut seen = BTreeSet::new();
    let mut held = Vec::new();
    for (index, id) in ids.iter().enumerate() {
        if !seen.insert(id.as_str()) {
            return Err(malformed(
                &[&format!("execution_ids[{index}]")],
                format!("names execution {id} twice"),
            ));
        }
        match book.ledger.executions.get(id) {
            Some(found) => held.push(found),
            None => {
                if let Some(account) = elsewhere(id) {
                    if differ {
                        return Err(StoreError::refused(
                            RefusalReason::ExecutionsDiffer,
                            format!(
                                "execution {id} is in account {account} and {} is in {}: one \
                                 accept answers the executions of one account, instrument and \
                                 side",
                                ids[0], book.account_id
                            ),
                        ));
                    }
                    return Err(StoreError::Invalid(format!(
                        "execution {id} is in account {account}, not {}: one command answers \
                         the executions of one account",
                        book.account_id
                    )));
                }
                return Err(StoreError::Invalid(format!(
                    "the book holds no execution {id}"
                )));
            }
        }
    }
    in_scope(ctx, &book.account_id)?;
    Ok(held)
}

/// An execution held to the row an answer takes it by: unmatched and still
/// waiting, never superseded by a bust or a correction, nor a bust itself
/// (transitions e3 to e6; the four points' (c)).
fn answerable(held: &orders::ExecutionHeld, row_id: &str, what: &str) -> Result<()> {
    let row = execution_row(row_id);
    let state =
        ExecutionState::try_from(held.execution.state).unwrap_or(ExecutionState::Unspecified);
    let id = &held.execution.execution_id;
    let why = if !execution_starts_from(row, Some(state)) {
        format!("it is {}", execution_state_named(state))
    } else if let Some(by) = &held.superseded_by {
        format!("execution {by} supersedes it")
    } else if held.is_bust() {
        "it is a bust, with nothing to answer".to_string()
    } else {
        return Ok(());
    };
    Err(StoreError::refused(
        RefusalReason::ExecutionState,
        format!(
            "{what} of execution {id} is refused: {why} (the transition table's row {}: {})",
            row.row, row.cause
        ),
    ))
}

/// A fill's side as an order's: a buy matches a buy, and any sell the
/// order's sell, the venue's marking kept on the execution (ruling 10).
fn sells(side: i32) -> bool {
    matches!(
        Side::try_from(side),
        Ok(Side::Sell | Side::SellShort | Side::SellShortExempt)
    )
}

fn same_side(a: i32, b: i32) -> bool {
    a == b || (sells(a) && sells(b))
}

fn differ(words: String) -> StoreError {
    StoreError::refused(RefusalReason::ExecutionsDiffer, words)
}

/// W11.23 (contract v23; rulings 4, 6, 10 to 14): an `ems` accepts 1 to 500
/// unmatched executions of one account, instrument and side, for a person or
/// its service account, to an existing order it may place against --
/// released, working or filled, never ended -- or to a new order made from
/// them, filled and never gated (ORDER_ORIGIN_ACCEPTED_EXECUTION, market and
/// day). The book creates the accepting `ems`'s placement at filled, routed
/// at 0, naming the connectivity plugin that reported the fills, and links
/// the executions to it, each matched (transition e3); the trade-date
/// position moves as of the fills' trade date, by this entry.
pub fn accept_execution(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &AcceptExecutionRequest,
    elsewhere: &dyn Fn(&str) -> Option<String>,
) -> Result<Made> {
    let named = executions_named(
        book,
        ctx,
        &request.execution_ids,
        meridian_pb::bounds::ACCEPT_EXECUTION_REQUEST_EXECUTION_IDS_COUNT,
        elsewhere,
        true,
    )?;
    user_required(ctx, "accepting an execution")?;
    for held in &named {
        answerable(held, "e3", "an accept")?;
    }
    let first = named[0];
    for held in &named[1..] {
        let (a, b) = (&first.execution, &held.execution);
        if a.instrument_id != b.instrument_id {
            return Err(differ(format!(
                "execution {} is of {} and {} of {}: one accept answers the executions of one \
                 account, instrument and side",
                a.execution_id, a.instrument_id, b.execution_id, b.instrument_id
            )));
        }
        if !same_side(a.side, b.side) {
            return Err(differ(format!(
                "executions {} and {} are on opposite sides: one accept answers the executions \
                 of one account, instrument and side",
                a.execution_id, b.execution_id
            )));
        }
        if held.sender != first.sender {
            return Err(differ(format!(
                "execution {} was reported by {} and {} by {}: the placement an accept creates \
                 names the one connectivity plugin that reported its fills",
                a.execution_id, first.sender, b.execution_id, held.sender
            )));
        }
    }
    // The accepted quantity, in shares, and in the order's terms below.
    let mut accepted = Exact::ZERO;
    for held in &named {
        let quantity = numbers::required("quantity", held.execution.quantity.as_ref())?;
        accepted = numbers::add("accepted quantity", accepted, quantity)?;
    }
    let mut acts = Vec::new();
    let order_id = match request.matched_order.as_ref() {
        None => {
            return Err(malformed(
                &["matched_order"],
                "an accept names an existing order (matched_order.order_id) or asks for a new \
                 one (matched_order.new_order)",
            ))
        }
        Some(accept_execution_request::MatchedOrder::OrderId(order_id)) => {
            if order_id.is_empty() {
                return Err(malformed(
                    &["matched_order.order_id"],
                    "an accept to an existing order names it",
                ));
            }
            let order = book.ledger.orders.get(order_id).ok_or_else(|| {
                StoreError::Invalid(format!(
                    "account {} holds no order {order_id}",
                    book.account_id
                ))
            })?;
            let state = state_of(book, order_id)?;
            if !matches!(
                state,
                OrderState::Released | OrderState::Working | OrderState::Filled
            ) {
                return Err(StoreError::refused(
                    RefusalReason::OrderState,
                    format!(
                        "an accept to order {order_id} is refused: it is {}, and an execution \
                         is accepted to an order released, working or filled, never ended or \
                         still proposed (the transition table's row e3)",
                        state_named(state)
                    ),
                ));
            }
            if order.order.instrument_id != first.execution.instrument_id {
                return Err(differ(format!(
                    "order {order_id} is of {} and the executions of {}",
                    order.order.instrument_id, first.execution.instrument_id
                )));
            }
            if !same_side(order.order.side, first.execution.side) {
                return Err(differ(format!(
                    "order {order_id} is on the other side of the executions: a buy fill \
                     matches a buy order, and any sell the order's sell"
                )));
            }
            let ordered = book.ledger.ordered(order);
            let (filled, _, _) = book.ledger.filled(order_id)?;
            let mut counted = Exact::ZERO;
            for held in &named {
                counted = numbers::add(
                    "accepted",
                    counted,
                    book.ledger.counted(Some(order), &held.execution)?,
                )?;
            }
            let after = numbers::add("filled", filled, counted)?;
            if after > ordered {
                return Err(StoreError::refused(
                    RefusalReason::QuantityExceedsOrder,
                    format!(
                        "an accept of {counted} to order {order_id} is refused: it is filled \
                         {filled} of {ordered} ordered, and the accept would take it to \
                         {after}; accept to a new order instead, or have portfolio change the \
                         ordered quantity first"
                    ),
                ));
            }
            order_id.clone()
        }
        Some(accept_execution_request::MatchedOrder::NewOrder(new_order)) => {
            let order = Order {
                order_id: ctx.mint(ids::ORDER),
                account_id: book.account_id.clone(),
                instrument_id: first.execution.instrument_id.clone(),
                sleeve: new_order.sleeve.clone(),
                side: first.execution.side,
                quantity: numbers::wire(accepted),
                order_type: OrderType::Market as i32,
                time_in_force: TimeInForce::Day as i32,
                origin: OrderOrigin::AcceptedExecution as i32,
                venue_order_id: first.execution.venue_order_id.clone(),
                person_roles: Some(PersonRoles {
                    entered_by: Some(ctx.actor()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let order_id = order.order_id.clone();
            acts.push(Act::Opened {
                order,
                released: true,
            });
            order_id
        }
    };
    let sleeve = match request.matched_order.as_ref() {
        Some(accept_execution_request::MatchedOrder::NewOrder(new_order)) => {
            new_order.sleeve.clone()
        }
        _ => book.ledger.orders[&order_id].order.sleeve.clone(),
    };
    let time_in_force = match request.matched_order.as_ref() {
        Some(accept_execution_request::MatchedOrder::NewOrder(_)) => TimeInForce::Day as i32,
        _ => book.ledger.orders[&order_id].order.time_in_force,
    };
    let mut venue_order_ids: Vec<String> = Vec::new();
    for held in &named {
        let id = &held.execution.venue_order_id;
        if !id.is_empty() && !venue_order_ids.contains(id) {
            venue_order_ids.push(id.clone());
        }
    }
    let placement = Placement {
        placement_id: ctx.mint(ids::PLACEMENT),
        order_id: order_id.clone(),
        account_id: book.account_id.clone(),
        ems_instance_id: ctx.instance_id.clone(),
        connectivity_instance_id: first.sender.clone(),
        venue_account_code: first.venue_account_code.clone(),
        quantity: numbers::wire(accepted),
        time_in_force,
        destination: first.execution.venue_id.clone(),
        venue_order_id: first.execution.venue_order_id.clone(),
        venue_order_ids,
        // routed_at_ns stays 0: the platform never routed it (ruling 12).
        person_roles: Some(PersonRoles {
            executed_by: Some(ctx.actor()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let placement_id = placement.placement_id.clone();
    acts.push(Act::PlacementOpened { placement });
    // Each fill's lines, as of its trade date, on the order's sleeve.
    let mut lines: Vec<MovementLine> = Vec::new();
    let mut line_counts = Vec::new();
    let mut trade_dates = BTreeSet::new();
    for held in &named {
        let mut execution = held.execution.clone();
        execution.order_id = order_id.clone();
        execution.placement_id = placement_id.clone();
        execution.sleeve = sleeve.clone();
        trade_dates.insert(execution.trade_date.clone());
        let own = if orders::moves_position(&execution) {
            trade_lines(book, ctx, &order_id, &execution, &lines)?
        } else {
            Vec::new()
        };
        line_counts.push(own.len());
        lines.extend(own);
    }
    acts.push(Act::Accepted {
        execution_ids: named
            .iter()
            .map(|held| held.execution.execution_id.clone())
            .collect(),
        order_id: order_id.clone(),
        placement_id,
        line_counts,
    });
    let mut plan = Plan::new("execution-accepted", ctx.actor(), acts).reason(&request.note);
    plan.lines = lines;
    // As of the fills' trade date (ruling 13); an accept of fills of more
    // than one trade date is journalled at the first of them.
    plan.effective_date = trade_dates
        .into_iter()
        .find(|date| !date.is_empty())
        .unwrap_or_default();
    let made = finish(book, head, ctx, plan)?;
    for id in &request.execution_ids {
        let to = made
            .book
            .ledger
            .executions
            .get(id)
            .and_then(|held| ExecutionState::try_from(held.execution.state).ok())
            .unwrap_or(ExecutionState::Unspecified);
        if !execution_admits(execution_row("e3"), Some(ExecutionState::Unmatched), to) {
            return Err(StoreError::Unavailable(format!(
                "an accept left execution {id} {}, which the table's row e3 does not reach",
                execution_state_named(to)
            )));
        }
    }
    Ok(made)
}

/// W11.24 (contract v23; rulings 3, 5, 6, 10): an `ems` answers 1 to 500
/// unmatched executions don't know, for a person or its service account,
/// with a reason from the closed list mirroring FIX's DKReason (127) and
/// text, required with DK_REASON_OTHER. Each is closed at DK, moving no
/// position (transition e4); the connectivity plugin that reported it hears
/// it, and a later fill for the same venue order arrives unmatched on its
/// own (e7).
pub fn dont_know_execution(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &DontKnowExecutionRequest,
    elsewhere: &dyn Fn(&str) -> Option<String>,
) -> Result<Made> {
    let named = executions_named(
        book,
        ctx,
        &request.execution_ids,
        meridian_pb::bounds::DONT_KNOW_EXECUTION_REQUEST_EXECUTION_IDS_COUNT,
        elsewhere,
        false,
    )?;
    let reason = DkReason::try_from(request.reason).unwrap_or(DkReason::Unspecified);
    if reason == DkReason::Unspecified {
        return Err(malformed(
            &["reason"],
            "a DK says why, by a reason from the closed list",
        ));
    }
    if reason == DkReason::Other && request.text.trim().is_empty() {
        return Err(malformed(
            &["text"],
            "a DK whose reason is other says why in its text, as FIX requires Text (58) when \
             DKReason is Other",
        ));
    }
    user_required(ctx, "answering an execution DK")?;
    for held in &named {
        answerable(held, "e4", "a DK")?;
    }
    let plan = Plan::new(
        "execution-dont-know",
        ctx.actor(),
        vec![Act::DontKnow {
            execution_ids: named
                .iter()
                .map(|held| held.execution.execution_id.clone())
                .collect(),
            reason: reason as i32,
            text: request.text.clone(),
        }],
    )
    .reason(&request.text);
    let made = finish(book, head, ctx, plan)?;
    for id in &request.execution_ids {
        let to = made
            .book
            .ledger
            .executions
            .get(id)
            .and_then(|held| ExecutionState::try_from(held.execution.state).ok())
            .unwrap_or(ExecutionState::Unspecified);
        if !execution_admits(execution_row("e4"), Some(ExecutionState::Unmatched), to) {
            return Err(StoreError::Unavailable(format!(
                "a DK left execution {id} {}, which the table's row e4 does not reach",
                execution_state_named(to)
            )));
        }
    }
    Ok(made)
}

/// What an order type is called, for words.
pub fn order_type_named(order_type: i32) -> &'static str {
    match OrderType::try_from(order_type) {
        Ok(OrderType::Market) => "market",
        Ok(OrderType::Limit) => "limit",
        Ok(OrderType::Stop) => "stop",
        Ok(OrderType::StopLimit) => "stop limit",
        Ok(OrderType::RequestForQuote) => "request for quote",
        _ => "unspecified",
    }
}

/// Whether a placement's state is one a venue's fact can still move.
pub fn placement_open(state: PlacementState) -> bool {
    orders::placement_state_live(state)
}
