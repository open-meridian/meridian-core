//! The order path in one account's book (W11, contract v22): what the book
//! holds of each order, placement, execution, charge, unplaced fact, locate
//! and trading instruction, and the fold that derives their status.
//!
//! **Status is a fold over the journal** (decisions/035). An entry records
//! what the book did -- an order opened, released, a condition set, a
//! placement's report, an execution -- as [`Act`]s; nothing records a state
//! or a quantity. Replaying an account's acts in order makes every record
//! here, and [`Ledger::order_record`] derives each order's state, conditions
//! and quantities from them after every entry: refused, cancelled or expired
//! if an act ended it; filled if reported filled reaches ordered; working if
//! released with quantity committed or filled; released if released;
//! proposed otherwise.
//!
//! **A child order** (the split link, ORDER_LINK_KIND_SPLIT): its executions
//! are recorded on it and count toward its parent's reported and final
//! filled too, up the chain to the top; while live it commits its open
//! quantity in its parent, as a live placement does. A position moves once,
//! from the child's executions.

use std::collections::{BTreeMap, BTreeSet};

use meridian_domain::exact::Exact;
use meridian_domain::v1::{
    Alert, AlertKind, AlertState, Allocation, Booking, BookingState, ChangeRequest,
    ChangeRequestOutcome, Charge, Check, Execution, ExecutionKind, ExecutionState, FinalityLevel,
    GiveUpRecord, JournalRef, Locate, LocateAnswer, LocateUse, LocateUseKind, Order,
    OrderCondition, OrderLinkKind, OrderOrigin, OrderQuantities, OrderState, OrderStatus,
    Placement, PlacementChangeKind, PlacementCondition, PlacementEvent, PlacementQuote,
    PlacementQuoteOutcome, PlacementState, PlacementStatus, ReportPlacementRequest,
    ReportValueDateRollRequest, SleeveCross, StandingOrderCondition, StandingPlacementCondition,
    TieOutReason, TradingInstruction, UnplacedFact,
};
use serde::{Deserialize, Serialize};

use crate::numbers;
use crate::store::{Result, StoreError};

/// A proto message kept in an entry's JSON as its encoding, so the act's
/// records are the contract's own types and the book defines no second shape.
mod pb {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use prost::Message;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<M: Message, S: Serializer>(message: &M, out: S) -> Result<S::Ok, S::Error> {
        out.serialize_str(&STANDARD.encode(message.encode_to_vec()))
    }

    pub fn deserialize<'de, M: Message + Default, D: Deserializer<'de>>(
        input: D,
    ) -> Result<M, D::Error> {
        let text = String::deserialize(input)?;
        let bytes = STANDARD.decode(text).map_err(serde::de::Error::custom)?;
        M::decode(bytes.as_slice()).map_err(serde::de::Error::custom)
    }
}

/// A list of proto messages kept as their encodings, as [`pb`] keeps one.
mod pb_vec {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use prost::Message;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<M: Message, S: Serializer>(messages: &[M], out: S) -> Result<S::Ok, S::Error> {
        out.collect_seq(
            messages
                .iter()
                .map(|message| STANDARD.encode(message.encode_to_vec())),
        )
    }

    pub fn deserialize<'de, M: Message + Default, D: Deserializer<'de>>(
        input: D,
    ) -> Result<Vec<M>, D::Error> {
        let texts = Vec::<String>::deserialize(input)?;
        texts
            .iter()
            .map(|text| {
                let bytes = STANDARD.decode(text).map_err(serde::de::Error::custom)?;
                M::decode(bytes.as_slice()).map_err(serde::de::Error::custom)
            })
            .collect()
    }
}

/// One thing the book did on the order path, as an entry keeps it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Act {
    /// An order opened: proposed (W11.1), recorded (W11.2), or a child
    /// released on creation within its parent (`released`). Its terms whole,
    /// with no status.
    Opened {
        #[serde(with = "pb")]
        order: Order,
        released: bool,
    },
    /// Its terms as they now stand: an amendment or a quantity change applied
    /// (W11.3). The order's version moves with it.
    Terms {
        #[serde(with = "pb")]
        order: Order,
    },
    /// Through the gate (W11.6): the transfer from `portfolio` to the `oms`.
    Released { order_id: String },
    /// A release asked for while the account waits for compliance: held at
    /// proposed, awaiting check, and released by the book when the gate
    /// passes at the verdict it waited for.
    ReleaseRequested { order_id: String },
    /// Back to proposed (W11.3's recall).
    Recalled { order_id: String },
    /// Ended: refused, cancelled or expired.
    Ended { order_id: String, state: i32 },
    /// An order cancel or its expiry asked of its live placements and child
    /// orders first (W11.3, W11.13): it ends when none is live.
    Ending { order_id: String, state: i32 },
    /// A condition the book keeps set or cleared: approval required,
    /// awaiting check, claimed, held, paused, late fact, or does not tie out
    /// for a restatement.
    Condition {
        order_id: String,
        condition: i32,
        set: bool,
        #[serde(with = "pb")]
        standing: StandingOrderCondition,
    },
    /// A person's override of a block on the order's version (W11.5).
    Overridden { order_id: String },
    /// A verdict on the order, or on one of its placements (W11.4).
    Checked {
        order_id: String,
        #[serde(with = "pb")]
        check: Check,
    },
    /// A change request opened (W11.3, W11.9), tracked to its outcome.
    ChangeOpened {
        #[serde(with = "pb")]
        request: ChangeRequest,
    },
    ChangeOutcome {
        change_request_id: String,
        outcome: i32,
    },
    /// A placement created at queued (W11.8).
    PlacementOpened {
        #[serde(with = "pb")]
        placement: Placement,
    },
    /// Contract v27 (W11.40): a dealer's quote on an RFQ placement,
    /// recorded or moved to a later outcome: the quote whole as it now
    /// stands. A picked quote moved on clears the pick.
    PlacementQuoted {
        placement_id: String,
        #[serde(with = "pb")]
        quote: PlacementQuote,
    },
    /// Contract v27 (W11.41): the `ems`'s pick of an open quote.
    QuotePicked {
        placement_id: String,
        quote_id: String,
    },
    /// Contract v27 (W11.1): a child entry in a net member's own account,
    /// the book's act: the member linked to the net order, in the
    /// allocation account, as its parent.
    NetLinked {
        order_id: String,
        net_order_id: String,
        net_account_id: String,
    },
    /// A placement refused and recorded (REFUSAL_REASON_CONNECTIVITY_NOT_ALLOWED,
    /// W11.8): the entry names the placing `ems`, the instance asked for, the
    /// actor and the reason, on the order.
    PlacementRefused { order_id: String },
    /// An accept to a new order refused and recorded (contract v26, W11.23,
    /// W11.38): an agent's, on a connection not stamped demo while the guard
    /// is on; its executions stay unmatched.
    AcceptRefused { execution_ids: Vec<String> },
    /// What the venue did with a placement (W11.10).
    PlacementReported {
        #[serde(with = "pb")]
        report: ReportPlacementRequest,
    },
    /// An execution recorded (W11.11), never edited, and the instance that
    /// reported it. From contract v23 a fill on no placement keeps the venue
    /// account code and the venue's client order ID it came with, which the
    /// Execution does not carry: the code is what the placement an accept
    /// creates names (W11.23).
    Executed {
        #[serde(with = "pb")]
        execution: Execution,
        sender: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        venue_account_code: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        venue_client_order_id: String,
        /// Contract v29 (W11.11; ruling 986, amended at core stage C): the
        /// latest published factor a pool's or a tranche's fill stating none
        /// was taken at, as exact decimal text; empty where it stated one,
        /// or is no pool's. Kept with the act so a rebuild prices it alike.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        factor_taken: String,
        /// Contract v29 (W11.11; rulings 991, 992, amended at core stage
        /// E): the row the factor taken was read from, and the factor month
        /// a provisional one awaits. Empty with factor_taken.
        #[serde(default, skip_serializing_if = "FactorRow::is_empty")]
        factor_row: FactorRow,
    },
    /// Contract v29 (W9.16, W11.11; ruling 992): the factor a pool's or a
    /// tranche's fill was taken at, revised by the book when the awaited
    /// month's row is published or the row taken republished: the new
    /// factor and its row, and the lots the fill opened, whose provisional
    /// mark clears. Its lines and its lifecycle event are the entry's.
    FactorRevised {
        execution_id: String,
        factor_taken: String,
        factor_row: FactorRow,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        lot_ids: Vec<String>,
    },
    /// Unmatched executions accepted by an `ems` (W11.23, contract v23):
    /// each linked to the order and to the placement the accept created,
    /// matched (transition e3). `line_counts` is how many of the entry's
    /// movement lines each execution moved, in order, so a later bust
    /// reverses its own and no other.
    Accepted {
        execution_ids: Vec<String>,
        order_id: String,
        placement_id: String,
        line_counts: Vec<usize>,
        /// A new order's note (NewOrderFromExecutions.note), kept with the
        /// act that made it: the Order has no field for it.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        new_order_note: String,
    },
    /// Unmatched executions answered don't know (W11.24, contract v23):
    /// closed, never moving a position (transition e4).
    DontKnow {
        execution_ids: Vec<String>,
        reason: i32,
        text: String,
    },
    /// A venue's roll of a spot position to the next value date (W11.25,
    /// contract v23), as the connectivity plugin reported it, and the instance
    /// that reported it: the entry's lines move the pending lines; no order,
    /// placement or execution changes.
    ValueDateRolled {
        #[serde(with = "pb")]
        roll: ReportValueDateRollRequest,
        sender: String,
    },
    /// A fee with no fill (W11.12).
    Charged {
        #[serde(with = "pb")]
        charge: Charge,
    },
    /// A fact the book could not place, kept (W11.13), and the instance that
    /// sent it.
    Unplaced {
        #[serde(with = "pb")]
        fact: UnplacedFact,
        sender: String,
    },
    /// An unplaced fact a later record claimed.
    Placed { unplaced_id: String },
    /// A locate requested (W11.18).
    LocateOpened {
        #[serde(with = "pb")]
        locate: Locate,
    },
    /// The prime's answer on it.
    LocateAnswered {
        locate_record_id: String,
        #[serde(with = "pb")]
        answer: LocateAnswer,
    },
    /// A use of it (W11.19).
    LocateUsed {
        locate_record_id: String,
        #[serde(with = "pb")]
        used: LocateUse,
    },
    /// A trading instruction set (W11.15), standing under its key: its
    /// target and its setter's level.
    Instructed {
        key: String,
        #[serde(with = "pb")]
        instruction: TradingInstruction,
    },

    // ── Booking (W11.26 to W11.37, contract v24) ──
    /// A booking opened (W11.27, rows b1 and b2), its allocations whole, in
    /// the order's account. `lines` is where its own movement lines are in
    /// the entry -- the first and how many -- so a cancel or a replace
    /// reverses those and no other.
    BookingOpened {
        #[serde(with = "pb")]
        booking: Booking,
        lines: (usize, usize),
    },
    /// Held with its reason (rows b4, b5), and a hand-off tried again while
    /// held: the reason kept, the state unchanged.
    BookingHeld { booking_id: String, reason: String },
    /// Handed off to `operations` (row b3).
    BookingHandedOff { booking_id: String },
    /// A client's booking closed, its error-account allocations handed off
    /// in the same act (row b3).
    BookingClosed { booking_id: String },
    /// Cancelled by the `oms` before the hand-off (row b6).
    BookingCancelled { booking_id: String, reason: String },
    /// Replaced by `operations` after the hand-off (row b7): a new version,
    /// the state kept, its lines where `lines` says.
    BookingReplaced {
        booking_id: String,
        #[serde(with = "pb_vec")]
        allocations: Vec<Allocation>,
        effective_date: String,
        reason: String,
        lines: (usize, usize),
    },
    /// Its executions moved by a fact after the hand-off or closing (row b9):
    /// the state kept, heard so `operations` corrects it.
    BookingMoved { booking_id: String, reason: String },
    /// An allocation of another account's booking received here, a child
    /// entry naming the booking (W11.30): its lines where `lines` says, and
    /// the block's member order in this account it fills, where one.
    AllocationReceived {
        booking_id: String,
        from_account: String,
        #[serde(with = "pb")]
        allocation: Allocation,
        member_order_id: String,
        lines: (usize, usize),
        /// Contract v29 (W11.27; rulings 991, 992): a pool's or a tranche's
        /// booking's factor, derived from its fills -- what a sale's relief
        /// here records its proceeds at -- and the factor month its
        /// provisional fills await, which marks the lots it opens. Empty on
        /// any other.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        factor: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        factor_month_awaited: String,
    },
    /// An allocation received here withdrawn by its booking's cancel or
    /// replace: its lines reversed by this entry.
    AllocationWithdrawn {
        booking_id: String,
        allocation_id: String,
    },
    /// A prime's answer to a give-up (W11.32, row b8), on the execution and,
    /// where one is booked, the allocation.
    GiveUpRecorded {
        execution_id: String,
        booking_id: String,
        allocation_id: String,
        #[serde(with = "pb")]
        record: GiveUpRecord,
    },
    /// A cross between two sleeves of the account (W11.33).
    SleeveCrossed {
        #[serde(with = "pb")]
        cross: SleeveCross,
    },
    /// An alert raised (W11.34): the book's own, or `compliance`'s.
    AlertRaised {
        #[serde(with = "pb")]
        alert: Alert,
    },
    /// An alert acknowledged for a person, with a reason; a held booking's
    /// is recorded and never clears it (ruled 2026-10-14).
    AlertAcknowledged { alert_id: String, reason: String },
    /// A held booking's alert cleared by the book.
    AlertCleared { alert_id: String },
    /// The escape hatch's setting (W11.36), in the control partition.
    LocalRecordOrdersSet { allows: bool },
    /// The agents' guard (contract v26, W11.38), in the control partition.
    AgentOrdersDemoOnlySet { on: bool },
}

/// An order as the book holds it: its terms, and what its journal says.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OrderHeld {
    /// Its terms whole, as proposed or recorded and as changed since.
    pub order: Order,
    pub released: bool,
    pub release_requested: bool,
    pub ended: Option<OrderState>,
    /// A cancel or an expiry waiting on its live placements and children.
    pub ending: Option<OrderState>,
    /// The conditions the book keeps, by condition.
    pub conditions: BTreeMap<i32, StandingOrderCondition>,
    pub checks: Vec<Check>,
    /// The version a person overrode a block on.
    pub overridden_version: Option<u64>,
    pub change_requests: Vec<String>,
    pub placements: Vec<String>,
    pub executions: Vec<String>,
    pub charges: Vec<String>,
    pub children: Vec<String>,
    /// The journal sequence of the entry that last set its terms: the version
    /// a check names (W11.4).
    pub version: u64,
    pub last_change: Option<JournalRef>,
    /// The instance that proposed or recorded it: whose own order a
    /// quantity change is (step 5's Q20).
    pub opened_by: String,
    /// When the book received the act that opened it: where a day order's
    /// time in force is counted from.
    pub opened_at_ns: i64,
}

/// A placement as the book holds it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlacementHeld {
    pub placement: Placement,
    pub state: PlacementState,
    pub conditions: BTreeMap<i32, StandingPlacementCondition>,
    pub checks: Vec<Check>,
    pub change_requests: Vec<String>,
    pub executions: Vec<String>,
    pub charges: Vec<String>,
    pub last_change: Option<JournalRef>,
}

/// An execution, the entry that recorded it, and what superseded it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ExecutionHeld {
    pub execution: Execution,
    pub entry_id: String,
    pub sender: String,
    pub superseded_by: Option<String>,
    /// The venue account code a fill on no placement came with (contract
    /// v23), for the placement an accept creates.
    pub venue_account_code: String,
    pub venue_client_order_id: String,
    /// Where an accept moved its position: the accept's entry, and the first
    /// of its lines and how many (W11.23). None where the entry that
    /// recorded it moved it, or nothing did.
    pub accepted_lines: Option<(String, usize, usize)>,
    /// Contract v29 (W11.11; ruling 986, amended at core stage C): the
    /// published factor a pool's or a tranche's fill stating none was taken
    /// at; empty otherwise.
    pub factor_taken: String,
    /// Contract v29 (rulings 991, 992): the row it was read from, and the
    /// factor month a provisional factor awaits.
    pub factor_row: FactorRow,
}

/// Contract v29 (W11.11; rulings 991, 992, amended at core stage E): the
/// `Factor` row a factor was taken from -- its month, version and payment
/// date -- and, where it is provisional, the factor month awaited.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FactorRow {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub month: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub version: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub payment_date: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub awaited: String,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl FactorRow {
    pub fn is_empty(&self) -> bool {
        self == &FactorRow::default()
    }
}

impl ExecutionHeld {
    /// The factor the fill is priced at (contract v29, W11.11): the one it
    /// states, or the published one the book took for it.
    pub fn factor(&self) -> Option<meridian_pb::v1::Decimal> {
        factor_of(&self.execution, &self.factor_taken)
    }

    /// A bust: a superseding fact of no quantity.
    pub fn is_bust(&self) -> bool {
        !self.execution.supersedes.is_empty()
            && self
                .execution
                .quantity
                .as_ref()
                .and_then(|quantity| Exact::from_wire(quantity).ok())
                .is_none_or(|quantity| quantity.is_zero())
    }

    /// Unmatched and still waiting for an answer (W11.17): neither
    /// superseded nor a bust. A busted unmatched fill stays unmatched with
    /// nothing to answer (transition e6); a corrected one is answered as its
    /// correction (e5).
    pub fn waiting(&self) -> bool {
        self.execution.state == ExecutionState::Unmatched as i32
            && self.superseded_by.is_none()
            && !self.is_bust()
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct LocateHeld {
    pub locate: Locate,
    pub uses: Vec<LocateUse>,
}

/// A booking as the book holds it (W11.27 to W11.30): its terms as the
/// latest version states them, and what its journal says.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BookingHeld {
    pub booking: Booking,
    pub state: BookingState,
    pub held_reason: String,
    /// Where the current version's lines are: the entry, the first, how many.
    pub lines: (String, usize, usize),
    /// Each version's number; 1 when booked, one more for each replace.
    pub version: u32,
    /// The held-booking alert standing against it, where one is raised.
    pub held_alert: String,
    /// The entry it was booked or last replaced in, whose sequence a later
    /// fact is compared with.
    pub booked_at: u64,
}

/// An allocation of another account's booking this account received, a
/// child entry (W11.30).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReceivedHeld {
    pub booking_id: String,
    pub from_account: String,
    pub allocation: Allocation,
    pub member_order_id: String,
    pub lines: (String, usize, usize),
    pub withdrawn: bool,
}

/// The order path's records in one account's book.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Ledger {
    pub orders: BTreeMap<String, OrderHeld>,
    pub placements: BTreeMap<String, PlacementHeld>,
    pub executions: BTreeMap<String, ExecutionHeld>,
    pub charges: BTreeMap<String, Charge>,
    pub changes: BTreeMap<String, ChangeRequest>,
    pub unplaced: BTreeMap<String, UnplacedFact>,
    pub locates: BTreeMap<String, LocateHeld>,
    pub instructions: BTreeMap<String, TradingInstruction>,
    /// Each venue fact applied, by the sending instance and the venue's
    /// identifiers, to the record it made (requirement 14).
    pub venue_facts: BTreeMap<String, String>,
    /// Each unplaced execution, by the sending instance and the venue's
    /// execution ID: what a later placed one claims.
    pub unplaced_executions: BTreeMap<String, String>,
    /// The entry each record last changed in, for an order's history.
    pub order_entries: BTreeMap<String, Vec<String>>,
    /// Contract v24 (W11.27 to W11.35): the bookings of this account's
    /// orders, the allocations of other accounts' bookings received here
    /// (by booking and allocation), the sleeve crosses and the alerts.
    pub bookings: BTreeMap<String, BookingHeld>,
    pub received: BTreeMap<(String, String), ReceivedHeld>,
    pub sleeve_crosses: BTreeMap<String, SleeveCross>,
    pub alerts: BTreeMap<String, Alert>,
    /// The escape hatch's setting, where the control partition states it
    /// (W11.36): `None` before any entry has.
    pub local_record_orders: Option<bool>,
    /// The agents' guard, where the control partition states it (contract
    /// v26, W11.38): `None` before any entry has, which reads as on.
    pub agent_orders_demo_only: Option<bool>,
}

/// The records an entry's acts change, each once, in the order they are
/// numbered.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Touched {
    pub orders: Vec<String>,
    pub placements: Vec<String>,
    pub executions: Vec<String>,
    pub unplaced: Vec<String>,
    pub locates: Vec<String>,
    pub instructions: Vec<String>,
    /// Contract v24.
    pub bookings: Vec<String>,
    pub alerts: Vec<String>,
    /// The control partition's settings an entry states (W11.36).
    pub settings: Vec<String>,
}

fn push_once(list: &mut Vec<String>, id: &str) {
    if !id.is_empty() && !list.iter().any(|held| held == id) {
        list.push(id.to_string());
    }
}

/// A dedup key's parts joined by a separator no identifier holds.
pub fn key_of(parts: &[&str]) -> String {
    parts.join("\u{1f}")
}

/// The open states: a placement in one commits its open quantity.
pub fn placement_state_live(state: PlacementState) -> bool {
    matches!(
        state,
        PlacementState::Queued
            | PlacementState::Sent
            | PlacementState::Working
            | PlacementState::DoneForDay
    )
}

/// Whether an execution counts toward reported filled: matched or later,
/// and a venue with no steps (finality unset) as final.
fn reported(execution: &Execution) -> bool {
    matches!(
        finality_of(execution),
        FinalityLevel::Matched
            | FinalityLevel::Included
            | FinalityLevel::Final
            | FinalityLevel::Unspecified
    )
}

pub fn finality_of(execution: &Execution) -> FinalityLevel {
    execution
        .finality
        .as_ref()
        .and_then(|finality| FinalityLevel::try_from(finality.level).ok())
        .unwrap_or(FinalityLevel::Unspecified)
}

/// Whether an execution moves the trade-date position: reported filled moves
/// it (the order-path plan's Q15).
pub fn moves_position(execution: &Execution) -> bool {
    reported(execution)
}

fn is_final(execution: &Execution) -> bool {
    matches!(
        finality_of(execution),
        FinalityLevel::Final | FinalityLevel::Unspecified
    )
}

/// `a × b`, exact: the product's scale is the sum of the two, trailing zeros
/// taken off where the wire's places would not hold it; refused, never
/// rounded, where it still does not fit.
pub fn multiply(field: &str, a: Exact, b: Exact) -> Result<Exact> {
    let mut integer = a.integer().checked_mul(b.integer()).ok_or_else(|| {
        StoreError::Invalid(format!("{field} {a} × {b} does not fit an exact decimal"))
    })?;
    let mut scale = a.scale() + b.scale();
    while scale > 18 && integer % 10 == 0 {
        integer /= 10;
        scale -= 1;
    }
    Exact::new(integer, scale).map_err(|out| StoreError::Invalid(format!("{field} {out}")))
}

fn zero_floor(value: Exact) -> Exact {
    if value.is_negative() {
        Exact::ZERO
    } else {
        value
    }
}

fn sub(field: &str, a: Exact, b: Exact) -> Result<Exact> {
    numbers::add(field, a, b.negated())
}

impl Ledger {
    /// The records `acts` change, the orders of every placement, execution
    /// and charge they touch included, and each touched order's parents up
    /// to the top: a child's change is its parent's too.
    pub fn touched(&self, acts: &[Act]) -> Touched {
        let mut touched = Touched::default();
        let mut opened_parents: BTreeMap<String, String> = BTreeMap::new();
        for act in acts {
            match act {
                Act::Opened { order, .. } | Act::Terms { order } => {
                    push_once(&mut touched.orders, &order.order_id);
                    if let Some(link) = &order.link {
                        opened_parents.insert(order.order_id.clone(), link.parent_order_id.clone());
                    }
                }
                Act::Released { order_id }
                | Act::ReleaseRequested { order_id }
                | Act::Recalled { order_id }
                | Act::Ended { order_id, .. }
                | Act::Ending { order_id, .. }
                | Act::Condition { order_id, .. }
                | Act::Overridden { order_id }
                | Act::PlacementRefused { order_id } => push_once(&mut touched.orders, order_id),
                Act::AcceptRefused { execution_ids } => {
                    for id in execution_ids {
                        push_once(&mut touched.executions, id);
                    }
                }
                Act::Checked { order_id, check } => {
                    if check.placement_id.is_empty() {
                        push_once(&mut touched.orders, order_id);
                    } else {
                        push_once(&mut touched.placements, &check.placement_id);
                        push_once(&mut touched.orders, order_id);
                    }
                }
                Act::ChangeOpened { request } => {
                    push_once(&mut touched.orders, &request.order_id);
                    push_once(&mut touched.placements, &request.placement_id);
                }
                Act::ChangeOutcome {
                    change_request_id, ..
                } => {
                    if let Some(request) = self.changes.get(change_request_id) {
                        push_once(&mut touched.orders, &request.order_id);
                        push_once(&mut touched.placements, &request.placement_id);
                    }
                }
                Act::PlacementOpened { placement } => {
                    push_once(&mut touched.placements, &placement.placement_id);
                    push_once(&mut touched.orders, &placement.order_id);
                }
                Act::PlacementReported { report } => {
                    push_once(&mut touched.placements, &report.placement_id);
                    if let Some(held) = self.placements.get(&report.placement_id) {
                        push_once(&mut touched.orders, &held.placement.order_id);
                    }
                }
                Act::PlacementQuoted { placement_id, .. }
                | Act::QuotePicked { placement_id, .. } => {
                    push_once(&mut touched.placements, placement_id);
                }
                Act::NetLinked { order_id, .. } => push_once(&mut touched.orders, order_id),
                Act::Accepted {
                    execution_ids,
                    order_id,
                    placement_id,
                    ..
                } => {
                    for id in execution_ids {
                        push_once(&mut touched.executions, id);
                    }
                    push_once(&mut touched.placements, placement_id);
                    push_once(&mut touched.orders, order_id);
                }
                Act::DontKnow { execution_ids, .. } => {
                    for id in execution_ids {
                        push_once(&mut touched.executions, id);
                    }
                }
                Act::FactorRevised { execution_id, .. } => {
                    push_once(&mut touched.executions, execution_id)
                }
                Act::Executed { execution, .. } => {
                    push_once(&mut touched.executions, &execution.execution_id);
                    push_once(&mut touched.placements, &execution.placement_id);
                    push_once(&mut touched.orders, &execution.order_id);
                    if !execution.supersedes.is_empty() {
                        push_once(&mut touched.executions, &execution.supersedes);
                    }
                }
                Act::Charged { charge } => {
                    push_once(&mut touched.placements, &charge.placement_id);
                    push_once(&mut touched.orders, &charge.order_id);
                }
                // Its positions are its lines'; no order-path record.
                Act::ValueDateRolled { .. } => {}
                Act::Unplaced { fact, .. } => push_once(&mut touched.unplaced, &fact.unplaced_id),
                Act::Placed { unplaced_id } => push_once(&mut touched.unplaced, unplaced_id),
                Act::LocateOpened { locate } => {
                    push_once(&mut touched.locates, &locate.locate_record_id)
                }
                Act::LocateAnswered {
                    locate_record_id, ..
                }
                | Act::LocateUsed {
                    locate_record_id, ..
                } => push_once(&mut touched.locates, locate_record_id),
                Act::Instructed { key, .. } => push_once(&mut touched.instructions, key),
                Act::BookingOpened { booking, .. } => {
                    push_once(&mut touched.bookings, &booking.booking_id)
                }
                Act::BookingHeld { booking_id, .. }
                | Act::BookingHandedOff { booking_id }
                | Act::BookingClosed { booking_id }
                | Act::BookingCancelled { booking_id, .. }
                | Act::BookingReplaced { booking_id, .. }
                | Act::BookingMoved { booking_id, .. } => {
                    push_once(&mut touched.bookings, booking_id)
                }
                // A child entry: the block's member order it fills.
                Act::AllocationReceived {
                    member_order_id, ..
                } => push_once(&mut touched.orders, member_order_id),
                Act::AllocationWithdrawn {
                    booking_id,
                    allocation_id,
                } => {
                    if let Some(held) = self
                        .received
                        .get(&(booking_id.clone(), allocation_id.clone()))
                    {
                        push_once(&mut touched.orders, &held.member_order_id);
                    }
                }
                Act::GiveUpRecorded {
                    execution_id,
                    booking_id,
                    ..
                } => {
                    push_once(&mut touched.executions, execution_id);
                    push_once(&mut touched.bookings, booking_id);
                }
                Act::SleeveCrossed { cross } => {
                    for order_id in &cross.order_ids {
                        push_once(&mut touched.orders, order_id);
                    }
                }
                Act::AlertRaised { alert } => push_once(&mut touched.alerts, &alert.alert_id),
                Act::AlertAcknowledged { alert_id, .. } | Act::AlertCleared { alert_id } => {
                    push_once(&mut touched.alerts, alert_id)
                }
                // The control partition's setting, chained of its own
                // (W11.36).
                Act::LocalRecordOrdersSet { .. } => {
                    push_once(&mut touched.settings, "local-record-orders")
                }
                Act::AgentOrdersDemoOnlySet { .. } => {
                    push_once(&mut touched.settings, "agent-orders-demo-only")
                }
            }
        }
        // Each touched order's parents, up to the top.
        let mut index = 0;
        while index < touched.orders.len() {
            let order_id = touched.orders[index].clone();
            // A net member's link names the net in the allocation account
            // (contract v27), never a parent here.
            let parent = self
                .orders
                .get(&order_id)
                .and_then(|held| held.order.link.as_ref())
                .filter(|link| link.kind != OrderLinkKind::Net as i32)
                .map(|link| link.parent_order_id.clone())
                .or_else(|| opened_parents.get(&order_id).cloned());
            if let Some(parent) = parent {
                push_once(&mut touched.orders, &parent);
            }
            index += 1;
        }
        touched
    }

    /// One act applied. `entry_id` is the entry's; `versions` the journal
    /// sequence each order's terms take in it (empty when only deciding).
    pub fn apply(&mut self, act: &Act, entry_id: &str, versions: &BTreeMap<String, u64>) {
        let version_of =
            |order_id: &str, held: u64| versions.get(order_id).copied().unwrap_or(held);
        match act {
            Act::Opened {
                order, released, ..
            } => {
                let order_id = order.order_id.clone();
                if let Some(link) = &order.link {
                    if let Some(parent) = self.orders.get_mut(&link.parent_order_id) {
                        if !parent.children.contains(&order_id) {
                            parent.children.push(order_id.clone());
                        }
                    }
                }
                // A recorded order's venue order ID, so a second recording it
                // is refused (W11.2).
                if order.origin != OrderOrigin::Unspecified as i32
                    && !order.venue_order_id.is_empty()
                {
                    self.venue_facts
                        .insert(recorded_order_key(&order.venue_order_id), order_id.clone());
                }
                let held = self.orders.entry(order_id.clone()).or_default();
                held.order = order.clone();
                held.released = *released;
                held.version = version_of(&order_id, held.version);
            }
            Act::Terms { order } => {
                if let Some(held) = self.orders.get_mut(&order.order_id) {
                    held.order = order.clone();
                    held.version = version_of(&order.order_id, held.version);
                }
            }
            Act::Released { order_id } => {
                if let Some(held) = self.orders.get_mut(order_id) {
                    held.released = true;
                    held.release_requested = false;
                    held.conditions
                        .remove(&(OrderCondition::AwaitingCheck as i32));
                }
            }
            Act::ReleaseRequested { order_id } => {
                if let Some(held) = self.orders.get_mut(order_id) {
                    held.release_requested = true;
                }
            }
            Act::Recalled { order_id } => {
                if let Some(held) = self.orders.get_mut(order_id) {
                    held.released = false;
                    held.release_requested = false;
                }
            }
            Act::Ended { order_id, state } => {
                if let Some(held) = self.orders.get_mut(order_id) {
                    held.ended = OrderState::try_from(*state).ok();
                    held.ending = None;
                    held.release_requested = false;
                    // Conditions that hold only while it can be worked.
                    for condition in [
                        OrderCondition::ApprovalRequired,
                        OrderCondition::AwaitingCheck,
                        OrderCondition::Claimed,
                        OrderCondition::Held,
                        OrderCondition::Paused,
                    ] {
                        held.conditions.remove(&(condition as i32));
                    }
                }
            }
            Act::Ending { order_id, state } => {
                if let Some(held) = self.orders.get_mut(order_id) {
                    held.ending = OrderState::try_from(*state).ok();
                }
            }
            Act::Condition {
                order_id,
                condition,
                set,
                standing,
            } => {
                if let Some(held) = self.orders.get_mut(order_id) {
                    if *set {
                        held.conditions.insert(*condition, standing.clone());
                    } else {
                        held.conditions.remove(condition);
                    }
                }
            }
            Act::Overridden { order_id } => {
                if let Some(held) = self.orders.get_mut(order_id) {
                    held.overridden_version = Some(held.version);
                }
            }
            Act::Checked { order_id, check } => {
                if check.placement_id.is_empty() {
                    if let Some(held) = self.orders.get_mut(order_id) {
                        held.checks.push(check.clone());
                    }
                } else if let Some(held) = self.placements.get_mut(&check.placement_id) {
                    held.checks.push(check.clone());
                }
            }
            Act::ChangeOpened { request } => {
                let id = request.change_request_id.clone();
                self.changes.insert(id.clone(), request.clone());
                if request.placement_id.is_empty() {
                    if let Some(held) = self.orders.get_mut(&request.order_id) {
                        held.change_requests.push(id);
                    }
                } else if let Some(held) = self.placements.get_mut(&request.placement_id) {
                    held.change_requests.push(id);
                }
            }
            Act::ChangeOutcome {
                change_request_id,
                outcome,
            } => {
                if let Some(request) = self.changes.get_mut(change_request_id) {
                    request.outcome = *outcome;
                }
            }
            Act::PlacementOpened { placement } => {
                let id = placement.placement_id.clone();
                if let Some(order) = self.orders.get_mut(&placement.order_id) {
                    if !order.placements.contains(&id) {
                        order.placements.push(id.clone());
                    }
                }
                // Contract v27 (W11.8): a warning it was opened with, sent
                // on a stale reference, kept as its condition; its status
                // is otherwise derived.
                let mut opened = placement.clone();
                let conditions = opened
                    .status
                    .take()
                    .map(|status| status.conditions)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|c| c.condition == PlacementCondition::SentOnStaleReference as i32)
                    .map(|c| (c.condition, c))
                    .collect();
                self.placements.insert(
                    id,
                    PlacementHeld {
                        placement: opened,
                        state: PlacementState::Queued,
                        conditions,
                        ..Default::default()
                    },
                );
            }
            Act::PlacementQuoted {
                placement_id,
                quote,
            } => {
                if let Some(held) = self.placements.get_mut(placement_id) {
                    let quotes = &mut held.placement.quotes;
                    match quotes.iter_mut().find(|q| q.quote_id == quote.quote_id) {
                        Some(kept) => *kept = quote.clone(),
                        None => quotes.push(quote.clone()),
                    }
                    if held.placement.picked_quote_id == quote.quote_id
                        && matches!(
                            PlacementQuoteOutcome::try_from(quote.outcome),
                            Ok(PlacementQuoteOutcome::Expired | PlacementQuoteOutcome::Withdrawn)
                        )
                    {
                        held.placement.picked_quote_id.clear();
                    }
                }
            }
            Act::NetLinked {
                order_id,
                net_order_id,
                ..
            } => {
                if let Some(held) = self.orders.get_mut(order_id) {
                    held.order.link = Some(meridian_domain::v1::OrderLink {
                        kind: OrderLinkKind::Net as i32,
                        parent_order_id: net_order_id.clone(),
                    });
                }
            }
            Act::QuotePicked {
                placement_id,
                quote_id,
            } => {
                if let Some(held) = self.placements.get_mut(placement_id) {
                    if let Some(quote) = held
                        .placement
                        .quotes
                        .iter_mut()
                        .find(|q| &q.quote_id == quote_id)
                    {
                        quote.outcome = PlacementQuoteOutcome::Picked as i32;
                    }
                    held.placement.picked_quote_id = quote_id.clone();
                }
            }
            Act::PlacementRefused { .. } | Act::AcceptRefused { .. } => {}
            Act::PlacementReported { report } => self.reported(report),
            Act::Executed {
                execution,
                sender,
                venue_account_code,
                venue_client_order_id,
                factor_taken,
                factor_row,
            } => {
                let id = execution.execution_id.clone();
                if !execution.supersedes.is_empty() {
                    if let Some(earlier) = self.executions.get_mut(&execution.supersedes) {
                        earlier.superseded_by = Some(id.clone());
                    }
                }
                if let Some(placement) = self.placements.get_mut(&execution.placement_id) {
                    if !placement.executions.contains(&id) {
                        placement.executions.push(id.clone());
                    }
                }
                if let Some(order) = self.orders.get_mut(&execution.order_id) {
                    if !order.executions.contains(&id) {
                        order.executions.push(id.clone());
                    }
                }
                if !execution.venue_execution_id.is_empty() {
                    self.venue_facts
                        .insert(venue_key(sender, execution), id.clone());
                }
                // Its state is derived, never stored (contract v23,
                // decisions/035 as amended): on a placement or, done away, on
                // its order, matched (transition e1); on neither, unmatched
                // (e2), and so is what supersedes it (e5, e6).
                let mut execution = execution.clone();
                if execution.venue_client_order_id.is_empty() {
                    execution.venue_client_order_id = venue_client_order_id.clone();
                }
                execution.state =
                    if execution.order_id.is_empty() && execution.placement_id.is_empty() {
                        ExecutionState::Unmatched as i32
                    } else {
                        ExecutionState::Matched as i32
                    };
                // Contract v29 (991, 992): the factor the book took, and the
                // month a provisional one awaits, on the record.
                taken_onto(&mut execution, factor_taken, factor_row);
                self.executions.insert(
                    id,
                    ExecutionHeld {
                        execution,
                        entry_id: entry_id.to_string(),
                        sender: sender.clone(),
                        superseded_by: None,
                        venue_account_code: venue_account_code.clone(),
                        venue_client_order_id: venue_client_order_id.clone(),
                        accepted_lines: None,
                        factor_taken: factor_taken.clone(),
                        factor_row: factor_row.clone(),
                    },
                );
            }
            Act::FactorRevised {
                execution_id,
                factor_taken,
                factor_row,
                ..
            } => {
                if let Some(held) = self.executions.get_mut(execution_id) {
                    held.factor_taken = factor_taken.clone();
                    held.factor_row = factor_row.clone();
                    taken_onto(&mut held.execution, factor_taken, factor_row);
                }
            }
            Act::Accepted {
                execution_ids,
                order_id,
                placement_id,
                line_counts,
                ..
            } => {
                let sleeve = self
                    .orders
                    .get(order_id)
                    .map(|held| held.order.sleeve.clone())
                    .unwrap_or_default();
                let mut start = 0;
                for (index, id) in execution_ids.iter().enumerate() {
                    let count = line_counts.get(index).copied().unwrap_or(0);
                    if let Some(held) = self.executions.get_mut(id) {
                        held.execution.order_id = order_id.clone();
                        held.execution.placement_id = placement_id.clone();
                        held.execution.sleeve = sleeve.clone();
                        held.execution.state = ExecutionState::Matched as i32;
                        held.accepted_lines = Some((entry_id.to_string(), start, count));
                    }
                    start += count;
                    if let Some(placement) = self.placements.get_mut(placement_id) {
                        if !placement.executions.contains(id) {
                            placement.executions.push(id.clone());
                        }
                    }
                    if let Some(order) = self.orders.get_mut(order_id) {
                        if !order.executions.contains(id) {
                            order.executions.push(id.clone());
                        }
                    }
                }
            }
            Act::DontKnow {
                execution_ids,
                reason,
                text,
            } => {
                for id in execution_ids {
                    if let Some(held) = self.executions.get_mut(id) {
                        held.execution.state = ExecutionState::DontKnow as i32;
                        held.execution.dk_reason = *reason;
                        held.execution.dk_text = text.clone();
                    }
                }
            }
            Act::Charged { charge } => {
                let id = charge.charge_id.clone();
                if let Some(placement) = self.placements.get_mut(&charge.placement_id) {
                    placement.charges.push(id.clone());
                }
                if let Some(order) = self.orders.get_mut(&charge.order_id) {
                    order.charges.push(id.clone());
                }
                self.charges.insert(id, charge.clone());
            }
            Act::Unplaced { fact, sender } => {
                if let Some(meridian_domain::v1::unplaced_fact::Fact::Execution(execution)) =
                    &fact.fact
                {
                    if !execution.venue_execution_id.is_empty() {
                        self.unplaced_executions.insert(
                            key_of(&[sender, &execution.venue_execution_id]),
                            fact.unplaced_id.clone(),
                        );
                    }
                }
                self.unplaced.insert(fact.unplaced_id.clone(), fact.clone());
            }
            Act::Placed { .. } | Act::ValueDateRolled { .. } => {}
            Act::LocateOpened { locate } => {
                self.locates.insert(
                    locate.locate_record_id.clone(),
                    LocateHeld {
                        locate: locate.clone(),
                        uses: Vec::new(),
                    },
                );
            }
            Act::LocateAnswered {
                locate_record_id,
                answer,
            } => {
                if let Some(held) = self.locates.get_mut(locate_record_id) {
                    held.locate.answer = Some(answer.clone());
                    held.locate.locate_id = answer.locate_id.clone();
                }
            }
            Act::LocateUsed {
                locate_record_id,
                used,
            } => {
                if let Some(held) = self.locates.get_mut(locate_record_id) {
                    held.uses.push(used.clone());
                }
            }
            Act::Instructed { key, instruction } => {
                self.instructions.insert(key.clone(), instruction.clone());
            }
            Act::BookingOpened { booking, lines } => {
                self.bookings.insert(
                    booking.booking_id.clone(),
                    BookingHeld {
                        booking: booking.clone(),
                        state: BookingState::Booked,
                        held_reason: String::new(),
                        lines: (entry_id.to_string(), lines.0, lines.1),
                        version: 1,
                        held_alert: String::new(),
                        booked_at: 0,
                    },
                );
            }
            Act::BookingHeld { booking_id, reason } => {
                if let Some(held) = self.bookings.get_mut(booking_id) {
                    if held.state == BookingState::Booked {
                        held.state = BookingState::Held;
                    }
                    held.held_reason = reason.clone();
                }
            }
            Act::BookingHandedOff { booking_id } => {
                if let Some(held) = self.bookings.get_mut(booking_id) {
                    held.state = BookingState::HandedOff;
                    held.held_reason.clear();
                }
            }
            Act::BookingClosed { booking_id } => {
                if let Some(held) = self.bookings.get_mut(booking_id) {
                    held.state = BookingState::Closed;
                    held.held_reason.clear();
                }
            }
            Act::BookingCancelled { booking_id, .. } => {
                if let Some(held) = self.bookings.get_mut(booking_id) {
                    held.state = BookingState::Cancelled;
                    held.held_reason.clear();
                }
            }
            Act::BookingReplaced {
                booking_id,
                allocations,
                lines,
                ..
            } => {
                if let Some(held) = self.bookings.get_mut(booking_id) {
                    held.booking.allocations = allocations.clone();
                    held.lines = (entry_id.to_string(), lines.0, lines.1);
                    held.version += 1;
                }
            }
            Act::BookingMoved { .. } => {}
            Act::AllocationReceived {
                booking_id,
                from_account,
                allocation,
                member_order_id,
                lines,
                ..
            } => {
                self.received.insert(
                    (booking_id.clone(), allocation.allocation_id.clone()),
                    ReceivedHeld {
                        booking_id: booking_id.clone(),
                        from_account: from_account.clone(),
                        allocation: allocation.clone(),
                        member_order_id: member_order_id.clone(),
                        lines: (entry_id.to_string(), lines.0, lines.1),
                        withdrawn: false,
                    },
                );
            }
            Act::AllocationWithdrawn {
                booking_id,
                allocation_id,
            } => {
                if let Some(held) = self
                    .received
                    .get_mut(&(booking_id.clone(), allocation_id.clone()))
                {
                    held.withdrawn = true;
                }
            }
            Act::GiveUpRecorded {
                execution_id,
                booking_id,
                allocation_id,
                record,
            } => {
                if let Some(held) = self.executions.get_mut(execution_id) {
                    held.execution.give_up_answers.push(record.clone());
                }
                if let Some(held) = self.bookings.get_mut(booking_id) {
                    if let Some(allocation) = held
                        .booking
                        .allocations
                        .iter_mut()
                        .find(|allocation| &allocation.allocation_id == allocation_id)
                    {
                        allocation.give_up_answers.push(record.clone());
                    }
                }
            }
            Act::SleeveCrossed { cross } => {
                self.sleeve_crosses
                    .insert(cross.sleeve_cross_id.clone(), cross.clone());
                for order_id in &cross.order_ids {
                    if let Some(held) = self.orders.get_mut(order_id) {
                        if !held.order.sleeve_cross_ids.contains(&cross.sleeve_cross_id) {
                            held.order
                                .sleeve_cross_ids
                                .push(cross.sleeve_cross_id.clone());
                        }
                    }
                }
            }
            Act::AlertRaised { alert } => {
                self.alerts.insert(alert.alert_id.clone(), alert.clone());
                if AlertKind::try_from(alert.kind) == Ok(AlertKind::BookingHeld) {
                    if let Some(subject) = &alert.subject {
                        if let Some(held) = self.bookings.get_mut(&subject.booking_id) {
                            held.held_alert = alert.alert_id.clone();
                        }
                    }
                }
            }
            Act::AlertAcknowledged { alert_id, reason } => {
                if let Some(held) = self.alerts.get_mut(alert_id) {
                    // A held booking's alert clears only by the book's own
                    // act: an acknowledgement is recorded beside it, never
                    // clearing it (ruled 2026-10-14).
                    if AlertKind::try_from(held.kind) != Ok(AlertKind::BookingHeld)
                        && held.state == AlertState::Raised as i32
                    {
                        held.state = AlertState::Acknowledged as i32;
                        let _ = reason;
                    }
                }
            }
            Act::AlertCleared { alert_id } => {
                if let Some(held) = self.alerts.get_mut(alert_id) {
                    held.state = AlertState::Cleared as i32;
                }
                for booking in self.bookings.values_mut() {
                    if &booking.held_alert == alert_id {
                        booking.held_alert.clear();
                    }
                }
            }
            Act::LocalRecordOrdersSet { allows } => {
                self.local_record_orders = Some(*allows);
            }
            Act::AgentOrdersDemoOnlySet { on } => {
                self.agent_orders_demo_only = Some(*on);
            }
        }
    }

    /// What the venue did with a placement (W11.10), in the contract's
    /// meanings. Never refused: a report that does not fit the placement's
    /// state is recorded and leaves an ended placement ended.
    fn reported(&mut self, report: &ReportPlacementRequest) {
        let pending: Vec<String> = match self.placements.get(&report.placement_id) {
            Some(held) => held
                .change_requests
                .iter()
                .filter(|id| {
                    self.changes.get(*id).is_some_and(|request| {
                        request.outcome == ChangeRequestOutcome::Pending as i32
                    })
                })
                .cloned()
                .collect(),
            None => return,
        };
        let change_kind = |changes: &BTreeMap<String, ChangeRequest>, id: &str| -> i32 {
            match changes.get(id).and_then(|request| request.change.as_ref()) {
                Some(meridian_domain::v1::change_request::Change::PlacementChange(change)) => {
                    change.kind
                }
                _ => 0,
            }
        };
        let event = PlacementEvent::try_from(report.event).unwrap_or(PlacementEvent::Unspecified);
        // A stop or stop-limit order is held at the venue until it triggers
        // (row 24): awaiting trigger from its acknowledgement.
        let held_for_trigger = self
            .placements
            .get(&report.placement_id)
            .and_then(|held| self.orders.get(&held.placement.order_id))
            .is_some_and(|order| {
                order.order.order_type == meridian_domain::v1::OrderType::Stop as i32
                    || order.order.order_type == meridian_domain::v1::OrderType::StopLimit as i32
            });
        let mut outcomes: Vec<(String, ChangeRequestOutcome)> = Vec::new();
        {
            let held = self
                .placements
                .get_mut(&report.placement_id)
                .expect("found above");
            if !report.venue_order_id.is_empty() {
                held.placement.venue_order_id = report.venue_order_id.clone();
                if !held
                    .placement
                    .venue_order_ids
                    .contains(&report.venue_order_id)
                {
                    held.placement
                        .venue_order_ids
                        .push(report.venue_order_id.clone());
                }
            }
            if report.routed_at_ns != 0 {
                held.placement.routed_at_ns = report.routed_at_ns;
                held.placement.routed_precision = report.routed_precision;
            }
            let live = placement_state_live(held.state);
            let condition = |kind: PlacementCondition, reason: &str| StandingPlacementCondition {
                condition: kind as i32,
                reason: reason.to_string(),
                unknown_reason: report.unknown_reason,
                status_as_reported: report.status_as_reported.clone(),
                ..Default::default()
            };
            match event {
                PlacementEvent::Sent => {
                    if held.state == PlacementState::Queued {
                        held.state = PlacementState::Sent;
                    }
                }
                PlacementEvent::Acknowledged => {
                    if matches!(held.state, PlacementState::Queued | PlacementState::Sent) {
                        held.state = PlacementState::Working;
                        if held_for_trigger {
                            held.conditions.insert(
                                PlacementCondition::AwaitingTrigger as i32,
                                condition(
                                    PlacementCondition::AwaitingTrigger,
                                    "held at the venue until it triggers",
                                ),
                            );
                        }
                    }
                }
                PlacementEvent::Replaced => {
                    if report.quantity.is_some() {
                        held.placement.quantity = report.quantity;
                    }
                    if report.limit_price.is_some() {
                        held.placement.limit_price = report.limit_price.clone();
                    }
                    for id in &pending {
                        if change_kind(&self.changes, id) == PlacementChangeKind::Replace as i32 {
                            outcomes.push((id.clone(), ChangeRequestOutcome::Applied));
                        }
                    }
                }
                PlacementEvent::DoneForDay => {
                    if live {
                        held.state = PlacementState::DoneForDay;
                    }
                }
                PlacementEvent::Restated => {
                    if report.quantity.is_some() {
                        held.placement.quantity = report.quantity;
                    }
                    if report.limit_price.is_some() {
                        held.placement.limit_price = report.limit_price.clone();
                    }
                    if held.state == PlacementState::DoneForDay {
                        held.state = PlacementState::Working;
                    }
                }
                PlacementEvent::Suspended => {
                    held.conditions.insert(
                        PlacementCondition::Suspended as i32,
                        condition(PlacementCondition::Suspended, "suspended by the venue"),
                    );
                }
                PlacementEvent::Triggered => {
                    held.conditions
                        .remove(&(PlacementCondition::AwaitingTrigger as i32));
                    held.conditions
                        .remove(&(PlacementCondition::Suspended as i32));
                }
                PlacementEvent::Cancelled | PlacementEvent::Expired => {
                    if live {
                        held.state = if event == PlacementEvent::Cancelled {
                            PlacementState::Cancelled
                        } else {
                            PlacementState::Expired
                        };
                    }
                    held.conditions
                        .remove(&(PlacementCondition::OutcomeUnknown as i32));
                    for id in &pending {
                        let applied = event == PlacementEvent::Cancelled
                            && change_kind(&self.changes, id) == PlacementChangeKind::Cancel as i32;
                        outcomes.push((
                            id.clone(),
                            if applied {
                                ChangeRequestOutcome::Applied
                            } else {
                                ChangeRequestOutcome::Rejected
                            },
                        ));
                    }
                }
                PlacementEvent::Rejected => {
                    // A rejection of a working placement with a change
                    // pending is the venue refusing the change (row 14, as
                    // FIX's OrderCancelReject): the latest request ends
                    // refused and the placement works on. Otherwise the
                    // placement itself is rejected.
                    let acknowledged = matches!(
                        held.state,
                        PlacementState::Working | PlacementState::DoneForDay
                    );
                    if acknowledged && !pending.is_empty() {
                        outcomes.push((
                            pending.last().cloned().expect("not empty"),
                            ChangeRequestOutcome::Rejected,
                        ));
                    } else {
                        if live {
                            held.state = PlacementState::Rejected;
                        }
                        held.placement.rejection_reason = report.rejection_reason;
                        held.placement.rejection_as_reported = report.rejection_as_reported.clone();
                        for id in &pending {
                            outcomes.push((id.clone(), ChangeRequestOutcome::Rejected));
                        }
                    }
                }
                PlacementEvent::OutcomeUnknown => {
                    held.conditions.insert(
                        PlacementCondition::OutcomeUnknown as i32,
                        condition(PlacementCondition::OutcomeUnknown, "outcome unknown"),
                    );
                }
                PlacementEvent::OutcomeResolved => {
                    held.conditions
                        .remove(&(PlacementCondition::OutcomeUnknown as i32));
                }
                PlacementEvent::Unspecified => {}
            }
        }
        for (id, outcome) in outcomes {
            if let Some(request) = self.changes.get_mut(&id) {
                request.outcome = outcome as i32;
            }
        }
    }

    // ── The fold ────────────────────────────────────────────────────────────

    /// The ordered quantity: the quantity, or for an order for an amount
    /// its cash quantity's amount.
    pub fn ordered(&self, held: &OrderHeld) -> Exact {
        if let Some(quantity) = held.order.quantity.as_ref() {
            return Exact::from_wire(quantity).unwrap_or(Exact::ZERO);
        }
        held.order
            .cash_quantity
            .as_ref()
            .and_then(|money| money.amount.as_ref())
            .and_then(|amount| Exact::from_wire(amount).ok())
            .unwrap_or(Exact::ZERO)
    }

    /// What an execution counts toward its order: its quantity, or for an
    /// order for an amount, its quantity at its price.
    pub fn counted(&self, order: Option<&OrderHeld>, execution: &Execution) -> Result<Exact> {
        let quantity = numbers::required("execution.quantity", execution.quantity.as_ref())?;
        let by_amount = order.is_some_and(|held| held.order.quantity.is_none());
        if !by_amount {
            return Ok(quantity);
        }
        let price = execution
            .price
            .as_ref()
            .and_then(|price| price.amount.as_ref())
            .and_then(|amount| Exact::from_wire(amount).ok())
            .unwrap_or(Exact::ZERO);
        multiply("execution's amount", quantity, price)
    }

    /// Reported filled, final filled and awaiting acceptance: its own
    /// executions not superseded, and its children's, up the chain.
    pub fn filled(&self, order_id: &str) -> Result<(Exact, Exact, Exact)> {
        let Some(held) = self.orders.get(order_id) else {
            return Ok((Exact::ZERO, Exact::ZERO, Exact::ZERO));
        };
        let (mut reported_sum, mut final_sum, mut awaiting) =
            (Exact::ZERO, Exact::ZERO, Exact::ZERO);
        for id in &held.executions {
            let Some(execution) = self.executions.get(id) else {
                continue;
            };
            if execution.superseded_by.is_some() {
                continue;
            }
            // Contract v27 (W11.11): a net's internal crosses offset its
            // members against each other; only its street fills fill it.
            if execution.execution.kind == ExecutionKind::InternalCross as i32 {
                continue;
            }
            let amount = self.counted(Some(held), &execution.execution)?;
            if reported(&execution.execution) {
                reported_sum = numbers::add("reported_filled", reported_sum, amount)?;
            }
            if is_final(&execution.execution) {
                final_sum = numbers::add("final_filled", final_sum, amount)?;
            }
            if finality_of(&execution.execution) == FinalityLevel::AwaitingAcceptance {
                awaiting = numbers::add("awaiting_acceptance", awaiting, amount)?;
            }
        }
        for child in &held.children {
            let (r, f, a) = self.filled(child)?;
            reported_sum = numbers::add("reported_filled", reported_sum, r)?;
            final_sum = numbers::add("final_filled", final_sum, f)?;
            awaiting = numbers::add("awaiting_acceptance", awaiting, a)?;
        }
        // Contract v24: a block's member is filled by its allocations (step
        // 5's Q22, W11.27), and an order a sleeve cross stands in for by the
        // cross (W11.33), neither minting an execution.
        let mut also = Exact::ZERO;
        for received in self.received.values() {
            if received.member_order_id == order_id && !received.withdrawn {
                let quantity = numbers::required(
                    "allocation.quantity",
                    received.allocation.quantity.as_ref(),
                )?;
                also = numbers::add("allocated", also, quantity)?;
            }
        }
        for cross in self.sleeve_crosses.values() {
            if cross.order_ids.iter().any(|id| id == order_id) {
                let quantity = numbers::required("sleeve_cross.quantity", cross.quantity.as_ref())?;
                also = numbers::add("crossed", also, quantity)?;
            }
        }
        reported_sum = numbers::add("reported_filled", reported_sum, also)?;
        final_sum = numbers::add("final_filled", final_sum, also)?;
        Ok((reported_sum, final_sum, awaiting))
    }

    /// The quantity of an order's live bookings allocated to accounts marked
    /// error accounts (W11.27): what an overfill booked there takes off its
    /// does-not-tie-out (W11.14).
    pub fn error_booked(&self, order_id: &str) -> Result<Exact> {
        let mut total = Exact::ZERO;
        for held in self.bookings.values() {
            if held.booking.order_id != order_id || held.state == BookingState::Cancelled {
                continue;
            }
            for allocation in &held.booking.allocations {
                if allocation.error_cause != 0 {
                    let quantity =
                        numbers::required("allocation.quantity", allocation.quantity.as_ref())?;
                    total = numbers::add("error booked", total, quantity)?;
                }
            }
        }
        Ok(total)
    }

    /// A booking whole, its state and held reason derived (W11.30).
    pub fn booking_record(&self, booking_id: &str) -> Result<Booking> {
        let held = self.bookings.get(booking_id).ok_or_else(|| {
            StoreError::Invalid(format!("the book holds no booking {booking_id}"))
        })?;
        Ok(Booking {
            state: held.state as i32,
            held_reason: held.held_reason.clone(),
            ..held.booking.clone()
        })
    }

    /// Whether an execution is in a live booking -- any not cancelled.
    pub fn booked_in(&self, execution_id: &str) -> Option<&BookingHeld> {
        self.bookings.values().find(|held| {
            held.state != BookingState::Cancelled
                && held
                    .booking
                    .execution_ids
                    .iter()
                    .any(|id| id == execution_id)
        })
    }

    /// A placement's reported filled, in its order's terms.
    pub fn placement_filled(&self, placement_id: &str) -> Result<Exact> {
        let Some(held) = self.placements.get(placement_id) else {
            return Ok(Exact::ZERO);
        };
        let order = self.orders.get(&held.placement.order_id);
        let mut total = Exact::ZERO;
        for id in &held.executions {
            if let Some(execution) = self.executions.get(id) {
                if execution.superseded_by.is_none() && reported(&execution.execution) {
                    let amount = self.counted(order, &execution.execution)?;
                    total = numbers::add("placement filled", total, amount)?;
                }
            }
        }
        Ok(total)
    }

    /// A placement's state: as reported, filled once its fills reach its
    /// quantity.
    pub fn placement_state(&self, placement_id: &str) -> Result<PlacementState> {
        let Some(held) = self.placements.get(placement_id) else {
            return Ok(PlacementState::Unspecified);
        };
        if placement_state_live(held.state) {
            let quantity = held
                .placement
                .quantity
                .as_ref()
                .and_then(|quantity| Exact::from_wire(quantity).ok())
                .unwrap_or(Exact::ZERO);
            let filled = self.placement_filled(placement_id)?;
            if !quantity.is_zero() && filled >= quantity {
                return Ok(PlacementState::Filled);
            }
        }
        Ok(held.state)
    }

    /// Whether a placement is live: an open state, or its outcome unknown
    /// (requirement 20: the quantity stays committed).
    pub fn placement_live(&self, placement_id: &str) -> Result<bool> {
        let Some(held) = self.placements.get(placement_id) else {
            return Ok(false);
        };
        let state = self.placement_state(placement_id)?;
        Ok(placement_state_live(state)
            || (held
                .conditions
                .contains_key(&(PlacementCondition::OutcomeUnknown as i32))
                && state != PlacementState::Filled))
    }

    /// A live placement's open quantity: its quantity less its fills.
    pub fn placement_open(&self, placement_id: &str) -> Result<Exact> {
        if !self.placement_live(placement_id)? {
            return Ok(Exact::ZERO);
        }
        let held = &self.placements[placement_id];
        let quantity = held
            .placement
            .quantity
            .as_ref()
            .and_then(|quantity| Exact::from_wire(quantity).ok())
            .unwrap_or(Exact::ZERO);
        Ok(zero_floor(sub(
            "placement open",
            quantity,
            self.placement_filled(placement_id)?,
        )?))
    }

    /// Whether an order is live: not ended and not filled.
    pub fn order_live(&self, order_id: &str) -> Result<bool> {
        Ok(!matches!(
            self.state(order_id)?,
            OrderState::Refused
                | OrderState::Cancelled
                | OrderState::Expired
                | OrderState::Filled
                | OrderState::Unspecified
        ))
    }

    /// Open at live placements and live child orders.
    pub fn committed(&self, order_id: &str) -> Result<Exact> {
        let Some(held) = self.orders.get(order_id) else {
            return Ok(Exact::ZERO);
        };
        let mut total = Exact::ZERO;
        for id in &held.placements {
            total = numbers::add("committed", total, self.placement_open(id)?)?;
        }
        for child in &held.children {
            if self.order_live(child)? {
                let child_held = &self.orders[child];
                let (filled, _, _) = self.filled(child)?;
                let open = zero_floor(sub("committed", self.ordered(child_held), filled)?);
                total = numbers::add("committed", total, open)?;
            }
        }
        Ok(total)
    }

    /// The fold (decisions/035): refused, cancelled or expired if an act
    /// ended it; filled if reported filled reaches ordered; working if
    /// released with quantity committed or filled; released if released;
    /// proposed otherwise.
    pub fn state(&self, order_id: &str) -> Result<OrderState> {
        let Some(held) = self.orders.get(order_id) else {
            return Ok(OrderState::Unspecified);
        };
        if let Some(ended) = held.ended {
            return Ok(ended);
        }
        let ordered = self.ordered(held);
        let (filled, _, _) = self.filled(order_id)?;
        if !ordered.is_zero() && filled >= ordered {
            return Ok(OrderState::Filled);
        }
        if held.released {
            if !filled.is_zero() || !self.committed(order_id)?.is_zero() {
                return Ok(OrderState::Working);
            }
            return Ok(OrderState::Released);
        }
        Ok(OrderState::Proposed)
    }

    /// The quantities, each derived; idle may go negative (Q3).
    pub fn quantities(&self, order_id: &str) -> Result<OrderQuantities> {
        let held = self
            .orders
            .get(order_id)
            .ok_or_else(|| StoreError::Invalid(format!("the book holds no order {order_id}")))?;
        let ordered = self.ordered(held);
        let (filled, final_filled, awaiting) = self.filled(order_id)?;
        let committed = self.committed(order_id)?;
        let idle = if held.released && held.ended.is_none() {
            sub("idle", sub("idle", ordered, filled)?, committed)?
        } else {
            Exact::ZERO
        };
        Ok(OrderQuantities {
            ordered: numbers::wire(ordered),
            reported_filled: numbers::wire(filled),
            final_filled: numbers::wire(final_filled),
            awaiting_acceptance: numbers::wire(awaiting),
            committed: numbers::wire(committed),
            idle: numbers::wire(idle),
            leaves: numbers::wire(sub("leaves", ordered, filled)?),
        })
    }

    /// The change requests outstanding on an order or its placements.
    pub fn pending_changes(&self, order_id: &str) -> Vec<&ChangeRequest> {
        let Some(held) = self.orders.get(order_id) else {
            return Vec::new();
        };
        let mut ids: Vec<&String> = held.change_requests.iter().collect();
        for placement in &held.placements {
            if let Some(placed) = self.placements.get(placement) {
                ids.extend(placed.change_requests.iter());
            }
        }
        ids.into_iter()
            .filter_map(|id| self.changes.get(id))
            .filter(|request| request.outcome == ChangeRequestOutcome::Pending as i32)
            .collect()
    }

    /// Its conditions: those the book keeps, and those it derives -- change
    /// requested, outcome unknown, does not tie out for an overfill.
    pub fn conditions(&self, order_id: &str) -> Result<Vec<StandingOrderCondition>> {
        let held = self
            .orders
            .get(order_id)
            .ok_or_else(|| StoreError::Invalid(format!("the book holds no order {order_id}")))?;
        let mut conditions = held.conditions.clone();
        let pending = self.pending_changes(order_id);
        if let Some(latest) = pending.last() {
            conditions.insert(
                OrderCondition::ChangeRequested as i32,
                StandingOrderCondition {
                    condition: OrderCondition::ChangeRequested as i32,
                    reason: change_words(latest),
                    actor: latest.entry.as_ref().and_then(|entry| entry.actor.clone()),
                    ..Default::default()
                },
            );
        }
        for placement in &held.placements {
            if let Some(unknown) = self.placements.get(placement).and_then(|held| {
                held.conditions
                    .get(&(PlacementCondition::OutcomeUnknown as i32))
            }) {
                conditions.insert(
                    OrderCondition::OutcomeUnknown as i32,
                    StandingOrderCondition {
                        condition: OrderCondition::OutcomeUnknown as i32,
                        reason: format!("placement {placement}'s outcome is unknown"),
                        actor: unknown.actor.clone(),
                        ..Default::default()
                    },
                );
            }
        }
        let ordered = self.ordered(held);
        let (filled, _, _) = self.filled(order_id)?;
        // An overfill's excess booked to an error account clears it (W11.14,
        // W11.27; contract v24).
        let filled = sub("filled", filled, self.error_booked(order_id)?)?;
        if !ordered.is_zero()
            && filled > ordered
            && !conditions.contains_key(&(OrderCondition::DoesNotTieOut as i32))
        {
            conditions.insert(
                OrderCondition::DoesNotTieOut as i32,
                StandingOrderCondition {
                    condition: OrderCondition::DoesNotTieOut as i32,
                    reason: format!("reported filled {filled} exceeds ordered {ordered}"),
                    tie_out_reason: TieOutReason::Overfill as i32,
                    ..Default::default()
                },
            );
        }
        Ok(conditions.into_values().collect())
    }

    pub fn placement_conditions(&self, placement_id: &str) -> Vec<StandingPlacementCondition> {
        let held = &self.placements[placement_id];
        let mut conditions = held.conditions.clone();
        if let Some(latest) = held
            .change_requests
            .iter()
            .filter_map(|id| self.changes.get(id))
            .rfind(|request| request.outcome == ChangeRequestOutcome::Pending as i32)
        {
            conditions.insert(
                PlacementCondition::ChangeRequested as i32,
                StandingPlacementCondition {
                    condition: PlacementCondition::ChangeRequested as i32,
                    reason: change_words(latest),
                    actor: latest.entry.as_ref().and_then(|entry| entry.actor.clone()),
                    ..Default::default()
                },
            );
        }
        conditions.into_values().collect()
    }

    /// A placement whole, its status derived.
    pub fn placement_record(&self, placement_id: &str) -> Result<Placement> {
        let held = self.placements.get(placement_id).ok_or_else(|| {
            StoreError::Invalid(format!("the book holds no placement {placement_id}"))
        })?;
        Ok(Placement {
            status: Some(PlacementStatus {
                state: self.placement_state(placement_id)? as i32,
                conditions: self.placement_conditions(placement_id),
            }),
            checks: held.checks.clone(),
            // Its own change requests, each with its PlacementChange and
            // outcome, so its connectivity plugin hears each on the
            // placement (W11.9; approved 2026-10-13).
            change_requests: held
                .change_requests
                .iter()
                .filter_map(|id| self.changes.get(id).cloned())
                .collect(),
            last_change: held.last_change.clone(),
            ..held.placement.clone()
        })
    }

    /// An order whole: its terms, its status derived, its checks and change
    /// requests; with `with_records`, its placements, executions and
    /// charges (W11.17).
    pub fn order_record(&self, order_id: &str, with_records: bool) -> Result<Order> {
        let held = self
            .orders
            .get(order_id)
            .ok_or_else(|| StoreError::Invalid(format!("the book holds no order {order_id}")))?;
        let mut order = held.order.clone();
        order.status = Some(OrderStatus {
            state: self.state(order_id)? as i32,
            conditions: self.conditions(order_id)?,
            quantities: Some(self.quantities(order_id)?),
        });
        order.checks = held.checks.clone();
        order.change_requests = held
            .change_requests
            .iter()
            .filter_map(|id| self.changes.get(id).cloned())
            .collect();
        order.last_change = held.last_change.clone();
        if with_records {
            order.placements = held
                .placements
                .iter()
                .map(|id| self.placement_record(id))
                .collect::<Result<_>>()?;
            order.executions = held
                .executions
                .iter()
                .filter_map(|id| self.executions.get(id))
                .map(|held| held.execution.clone())
                .collect();
            order.charges = held
                .charges
                .iter()
                .filter_map(|id| self.charges.get(id).cloned())
                .collect();
        }
        Ok(order)
    }

    pub fn locate_record(&self, locate_record_id: &str) -> Result<Locate> {
        let held = self.locates.get(locate_record_id).ok_or_else(|| {
            StoreError::Invalid(format!("the book holds no locate {locate_record_id}"))
        })?;
        let approved = held
            .locate
            .answer
            .as_ref()
            .and_then(|answer| answer.quantity_approved.as_ref())
            .and_then(|quantity| Exact::from_wire(quantity).ok())
            .unwrap_or(Exact::ZERO);
        let mut remaining = approved;
        for used in &held.uses {
            let quantity = used
                .quantity
                .as_ref()
                .and_then(|quantity| Exact::from_wire(quantity).ok())
                .unwrap_or(Exact::ZERO);
            remaining = match LocateUseKind::try_from(used.kind) {
                Ok(LocateUseKind::GiveBack) => numbers::add("remaining", remaining, quantity)?,
                Ok(LocateUseKind::Reserve) | Ok(LocateUseKind::Consume) => {
                    sub("remaining", remaining, quantity)?
                }
                _ => remaining,
            };
        }
        Ok(Locate {
            remaining: numbers::wire(remaining),
            uses: held.uses.clone(),
            ..held.locate.clone()
        })
    }

    /// The top of an order's chain of parents.
    pub fn top(&self, order_id: &str) -> String {
        let mut current = order_id.to_string();
        let mut seen = BTreeSet::new();
        while let Some(parent) = self
            .orders
            .get(&current)
            .and_then(|held| held.order.link.as_ref())
            .map(|link| link.parent_order_id.clone())
        {
            if !seen.insert(parent.clone()) || !self.orders.contains_key(&parent) {
                break;
            }
            current = parent;
        }
        current
    }

    /// The live placements and live child orders of an order.
    pub fn live_parts(&self, order_id: &str) -> Result<(Vec<String>, Vec<String>)> {
        let Some(held) = self.orders.get(order_id) else {
            return Ok((Vec::new(), Vec::new()));
        };
        let mut placements = Vec::new();
        for id in &held.placements {
            if self.placement_live(id)? {
                placements.push(id.clone());
            }
        }
        let mut children = Vec::new();
        for id in &held.children {
            if self.order_live(id)? {
                children.push(id.clone());
            }
        }
        Ok((placements, children))
    }
}

const DAY_NS: i64 = 86_400 * 1_000_000_000;

/// When an order's own time in force ends, by the deployment's clock
/// (W11.13), where it ends by the clock at all: a day order (and one at the
/// open or the close) at the end of the UTC date it opened on; a
/// good-till-date order at the end of its date; a good-till-time order at
/// its kill time. Immediate-or-cancel and fill-or-kill end by the venue's
/// facts, and a good-till-cancelled order never by the clock.
pub fn expires_at(held: &OrderHeld) -> Option<i64> {
    use meridian_domain::date::Date;
    use meridian_domain::v1::TimeInForce;
    let end_of = |date: Date| (date.days_since_epoch() + 1).checked_mul(DAY_NS);
    match TimeInForce::try_from(held.order.time_in_force).ok()? {
        TimeInForce::Day | TimeInForce::AtTheOpen | TimeInForce::AtTheClose => {
            end_of(Date::of_utc_instant(held.opened_at_ns))
        }
        TimeInForce::GoodTillDate => Date::parse(&held.order.expire_date).ok().and_then(end_of),
        TimeInForce::GoodTillTime => {
            (held.order.expire_time_ns > 0).then_some(held.order.expire_time_ns)
        }
        _ => None,
    }
}

/// When an order comes due to expire by the deployment's clock (W11.13):
/// a released order at the end of its time in force (row i4); one never
/// released only where it is a day order, at its day's end (row i7, ruled
/// 2026-10-12); none that is ending or ended.
pub fn due_at(held: &OrderHeld) -> Option<i64> {
    use meridian_domain::v1::TimeInForce;
    if held.ending.is_some() || held.ended.is_some() {
        return None;
    }
    let day = matches!(
        TimeInForce::try_from(held.order.time_in_force),
        Ok(TimeInForce::Day | TimeInForce::AtTheOpen | TimeInForce::AtTheClose)
    );
    if !held.released && !day {
        return None;
    }
    expires_at(held)
}

/// The dedup key of a venue's execution: the instance that sent it and the
/// venue's identifiers, with what it supersedes and its finality, so a
/// finality move or a correction is a fact of its own and a repeat is not.
/// A recorded order's venue or broker order ID, kept in its account's book
/// so a second order recording it is refused (W11.2).
pub fn recorded_order_key(venue_order_id: &str) -> String {
    key_of(&["order", venue_order_id])
}

pub fn venue_key(sender: &str, execution: &Execution) -> String {
    key_of(&[
        sender,
        &execution.venue_execution_id,
        finality_of(execution).as_str_name(),
        &execution.supersedes,
    ])
}

/// A change request in words, for its condition's reason.
fn change_words(request: &ChangeRequest) -> String {
    use meridian_domain::v1::change_request::Change;
    use meridian_domain::v1::OrderChangeKind;
    match request.change.as_ref() {
        Some(Change::OrderChange(change)) => match OrderChangeKind::try_from(change.kind) {
            Ok(OrderChangeKind::Cancel) => "cancel requested".into(),
            Ok(OrderChangeKind::Amend) => "amendment requested".into(),
            Ok(OrderChangeKind::Quantity) => "quantity change requested".into(),
            Ok(OrderChangeKind::Recall) => "recall requested".into(),
            Ok(OrderChangeKind::Withdraw) => "withdrawal requested".into(),
            _ => "change requested".into(),
        },
        Some(Change::PlacementChange(change)) => match PlacementChangeKind::try_from(change.kind) {
            Ok(PlacementChangeKind::Cancel) => "cancel requested".into(),
            Ok(PlacementChangeKind::Replace) => "replace requested".into(),
            _ => "change requested".into(),
        },
        None => "change requested".into(),
    }
}

/// Each top-level field of an order that differs between `before` and
/// `after`, by its name in the data dictionary: what OrderUpdated says
/// changed.
pub fn order_changed(before: Option<&Order>, after: &Order) -> Vec<String> {
    let empty = Order::default();
    let before = before.unwrap_or(&empty);
    let mut changed = Vec::new();
    macro_rules! compare {
        ($($field:ident),*) => {
            $(if before.$field != after.$field {
                changed.push(stringify!($field).to_string());
            })*
        };
    }
    compare!(
        order_id,
        account_id,
        instrument_id,
        sleeve,
        side,
        locate_id,
        quantity,
        cash_quantity,
        order_type,
        limit_price,
        stop_price,
        dealt_currency,
        legs,
        allocation_instruction,
        time_in_force,
        expire_date,
        expire_time_ns,
        execution_instructions,
        lot_relief_method,
        relieve_lot_ids,
        capacity,
        direction,
        parties,
        person_roles,
        customer_id,
        firm_designated_id,
        handling_instructions,
        received_at_ns,
        received_precision,
        quote_at_receipt,
        represents_order_ids,
        axe_source,
        axe_counterparty_id,
        link,
        origin,
        venue_order_id,
        block_member_order_ids,
        sleeve_cross_ids,
        status,
        checks,
        change_requests,
        placements,
        executions,
        charges
    );
    changed
}

/// The same, for a placement (PlacementUpdated's `changed`).
pub fn placement_changed(before: Option<&Placement>, after: &Placement) -> Vec<String> {
    let empty = Placement::default();
    let before = before.unwrap_or(&empty);
    let mut changed = Vec::new();
    macro_rules! compare {
        ($($field:ident),*) => {
            $(if before.$field != after.$field {
                changed.push(stringify!($field).to_string());
            })*
        };
    }
    compare!(
        placement_id,
        order_id,
        account_id,
        ems_instance_id,
        connectivity_instance_id,
        venue_account_code,
        quantity,
        limit_price,
        time_in_force,
        strategy,
        destination,
        route_type,
        routed_at_ns,
        routed_precision,
        handling_instructions,
        rfq_id,
        axe_id,
        venue_order_id,
        status,
        person_roles,
        checks,
        rejection_reason,
        rejection_as_reported,
        venue_order_ids
    );
    changed
}

/// The factor taken and the month awaited, onto the execution's record
/// (contract v29, W11.11; rulings 991, 992): `factor_taken` and
/// `factor_month_awaited` as the book decided them.
pub fn taken_onto(execution: &mut Execution, taken: &str, row: &FactorRow) {
    execution.factor_taken = taken
        .parse::<meridian_domain::exact::Exact>()
        .ok()
        .map(|factor| factor.to_wire());
    execution.factor_month_awaited = row.awaited.clone();
}

/// The factor a fill is priced at (contract v29, W11.11; ruling 986, amended
/// at core stage C): the one it states, or else the published one the book
/// took for it, kept as exact decimal text.
pub fn factor_of(execution: &Execution, taken: &str) -> Option<meridian_pb::v1::Decimal> {
    execution.factor_used.or_else(|| {
        taken
            .parse::<meridian_domain::exact::Exact>()
            .ok()
            .map(|factor| factor.to_wire())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_act_reads_back_as_it_was_kept() {
        let act = Act::Opened {
            order: Order {
                order_id: "ORD-1".into(),
                account_id: "ACC-1".into(),
                ..Default::default()
            },
            released: true,
        };
        let text = serde_json::to_string(&act).unwrap();
        assert_eq!(serde_json::from_str::<Act>(&text).unwrap(), act);
    }

    #[test]
    fn a_product_is_exact_and_never_rounded() {
        let a: Exact = "227.41".parse().unwrap();
        let b: Exact = "4".parse().unwrap();
        assert_eq!(multiply("x", a, b).unwrap().to_string(), "909.64");
        let tiny: Exact = "0.000000001".parse().unwrap();
        let tinier: Exact = "0.0000000001".parse().unwrap();
        assert!(multiply("x", tiny, tinier).is_err());
    }
}
