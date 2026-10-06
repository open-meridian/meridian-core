//! What the street store holds, and what any store of it must provide.
//!
//! Three things, and the relationships between them are the whole design.
//!
//! A **statement** is the connector's snapshot of one account at one moment.
//! It is identified by the connector's name for it, made from the account and
//! the time it read, so a redelivery is recognisable rather than duplicated;
//! it names the external account it was read for and the account that is
//! linked to, and it carries the account's figures where the venue reported
//! them, one set per margin segment.
//!
//! A **holding** is one row of one statement: an account, an instrument or the
//! identifiers we could not turn into one, a side, a quantity and a value where
//! one was reported. Rows are never deleted and never merged. A statement's
//! rows are what it said.
//!
//! A **custodial position** is what the custodian says an account holds of an
//! instrument, on one side. It is derived from the latest statement's rows
//! rather than accumulated across statements, because a holding row states a
//! quantity as of a date and not a change. Adding them up would double one that
//! appeared in two reads. It is not our own book, which does not exist yet and
//! will have its own name when it does.
//!
//! # Every change numbered
//!
//! Each change this store makes that a reader hears -- a position changed or
//! removed, a statement completed -- takes the street partition's next
//! number in the transaction that makes it, and names the previous change of
//! its kind for its account and who caused it (W2.4; spec/plugins-hear-and-
//! read, Q1 and Q3 as clarified 2026-10-01). A reader that heard only some
//! accounts tells a gap in its own from the numbers others used, and catches
//! up by reading the changes since the last it saw: a removed position stays
//! as a tombstone so that read sees the removal.

use std::collections::BTreeSet;

use prost::Message;
use std::fmt;

use crate::amounts::{Money, Quantity, Refused};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("the street store is unavailable: {0}")]
    Unavailable(String),

    /// The database was migrated by a newer release than this binary. Its own
    /// variant because it is the one refusal at start that waiting never
    /// fixes: a starting component waits out every other one.
    #[error("{0}")]
    SchemaAhead(String),

    #[error("no statement {0}")]
    UnknownStatement(String),

    #[error(
        "a holding names both an instrument and unresolved identifiers, which cannot both be true"
    )]
    BothResolvedAndNot,

    #[error("a holding names neither an instrument nor any identifier, so it describes nothing")]
    NeitherResolvedNorIdentified,

    #[error(
        "a holding states no quantity; its trade-date quantity is required, and unset is not zero"
    )]
    NoQuantity,

    #[error("a holding says neither long nor short; a holding says which side it is on")]
    NoSide,

    #[error(
        "a holding on the {side} side states a quantity of {quantity}; the quantity is signed \
         to match its side, negative short"
    )]
    SideContradictsSign { side: Side, quantity: Quantity },

    #[error("{0} is not a cursor this store wrote; start again from the first page")]
    UnreadableCursor(String),

    /// A plugin's read naming an account outside its read scope (W4.11).
    /// The sidecar refuses it first; this is the second line.
    #[error("{0} is not in this plugin's read scope")]
    OutOfScope(String),

    /// A row naming another account than its statement's (W2.2).
    #[error(
        "a row for account {row} belongs to no statement of {statement}: a statement is one \
         account's"
    )]
    AnotherAccount { row: String, statement: String },

    /// A statement's figures that cannot stand as sent (W2.2).
    #[error("{0}")]
    Figures(String),

    /// A holding's sub-balance that cannot stand as sent (W2.3, v8).
    #[error("{0}")]
    Encumbrance(String),

    /// What a plugin closed, or a backfill, that cannot stand as sent (W2.3,
    /// W2.4, contract v11), naming the field.
    #[error("{0}")]
    Edge(String),

    /// A backfill naming a row its statement never had (W2.4, contract v11):
    /// a backfill corrects no row into existence.
    #[error("the statement has no row for {0}; a backfill amends a row already recorded")]
    NoSuchRow(String),

    /// A re-resolution naming no activity recorded (W2.15, contract v15): a
    /// re-resolution resolves no activity into existence.
    #[error(
        "no activity is recorded as {0}; a re-resolution re-resolves an activity already recorded"
    )]
    NoSuchActivity(String),

    /// A quantity or an amount outside what the wire carries, named.
    #[error(transparent)]
    OutOfRange(#[from] Refused),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// The partition the street's changes are numbered in. Data, not
/// vocabulary: the street is one partition, and a reader's watermark names it.
pub const PARTITION: &str = "street";

/// Where a change sits in the street's record: its number in the partition,
/// and the number of the previous change of its kind for its account, 0 for
/// the first. Both 0 on what was recorded before the street numbered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Change {
    pub sequence: u64,
    pub previous: u64,
}

/// Who caused a change, as the store recorded it when it committed (Q3): the
/// instance that sent the command and the person it was sent for, the chain
/// it was in and the command itself.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cause {
    pub instance_id: String,
    pub acting_for_subject: String,
    pub correlation_id: String,
    pub causation_id: String,
    pub committed_at_ns: i64,
}

/// The kinds of change an account's chain runs through: each is its own, so a
/// reader hearing one sees no false gap from the other (Q3 clarified per row,
/// 2026-10-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Chain {
    Position,
    Statement,
    /// The custodian's activity (W2.12, contract v14).
    Activity,
    /// Each sync status the street heard (W2.13, contract v14).
    SyncStatus,
    /// Each re-resolution of an activity (W2.16, contract v15), apart from
    /// the activities, so a reader hearing only those sees no gap.
    ReResolution,
}

impl Chain {
    pub fn as_str(self) -> &'static str {
        match self {
            Chain::Position => "position",
            Chain::Statement => "statement",
            Chain::Activity => "activity",
            Chain::SyncStatus => "sync_status",
            Chain::ReResolution => "re_resolution",
        }
    }
}

/// Whose accounts a read answers for (W4.11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// A core component reading as itself: every account.
    Everything,
    /// A plugin's read, its scope marked as applying: these accounts, and
    /// none when there are none.
    Within(BTreeSet<String>),
}

impl Scope {
    /// Whether an account is read.
    pub fn holds(&self, account_id: &str) -> bool {
        match self {
            Scope::Everything => true,
            Scope::Within(accounts) => accounts.contains(account_id),
        }
    }

    /// The account a read names, refused when it is outside the scope.
    pub fn admit(&self, account_id: &str) -> Result<()> {
        if account_id.is_empty() || self.holds(account_id) {
            return Ok(());
        }
        Err(StoreError::OutOfScope(account_id.to_string()))
    }

    /// Whether a row of `account_id` is answered to a read naming `named`.
    pub fn answers(&self, named: &str, account_id: &str) -> bool {
        (named.is_empty() || named == account_id) && self.holds(account_id)
    }
}

/// The connector's snapshot of one account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub statement_id: String,
    pub source: String,

    /// The connector's name for this snapshot, made from the account and the
    /// time it read it: no venue has a statement of its own. With `source`, it
    /// is what makes a redelivery recognisable.
    pub external_statement_id: String,

    /// What the positions reflect. A date rather than an instant, because that
    /// is what a custodian states.
    pub as_of_date: String,

    /// When we read it. Different from the above, and conflating the two is how
    /// a stale statement is mistaken for a current one.
    pub read_at_ns: i64,

    /// How many rows will follow. W2.2.
    ///
    /// The only thing that marks the end of a statement: rows arrive as
    /// separate messages and none of them is distinguishable as the last. The
    /// connector holds the whole list before it publishes any of it, so it
    /// knows this without reading anything twice.
    pub expected_rows: u32,

    /// The account the statement is of, set by the sidecar from the external
    /// account's link (W2.2). Empty on one from a plugin before v7, which
    /// names none, until its first row lands and gives it the row's.
    pub account_id: String,

    /// The account as the rail knows it, and the institution holding it, as
    /// the connector named them; empty from a plugin before v7.
    pub external_account_id: String,
    pub institution: String,

    /// The account's figures as the venue reported them, one set per margin
    /// segment, no two naming the same. Never derived here.
    pub figures: Vec<Figures>,

    /// The venue stated no currency for the figures, and the connector's is
    /// its own stated assumption.
    pub currency_assumed: bool,

    /// The account servicer's lien or right of set-off over the account, as
    /// reported; absent where the statement does not say (contract v8).
    pub security_interest: Option<bool>,

    /// The raw record the statement's figures were converted from, in the
    /// plugin's own storage, and the provenance of each value the plugin
    /// closed rather than read (contract v11).
    pub raw_record: Option<RawRecord>,
    pub provenance: Vec<Provenance>,

    /// Set once, when the statement completed (W2.5): the change, who caused
    /// it and when.
    pub completed: Option<Completed>,
}

/// A statement's completion as the store recorded it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Completed {
    pub change: Change,
    pub cause: Cause,
}

/// An account's figures for one margin segment, as the venue reported them.
///
/// Each is absent where the venue reported none, which is not zero: an account
/// with no buying power and an account whose venue does not say are different
/// accounts, and a zero standing in for "not said" is how a margin call is
/// missed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Figures {
    /// The segment as the venue names it, verbatim; empty for the account as
    /// a whole.
    pub segment: String,
    pub buying_power: Option<Money>,
    pub margin_requirement: Option<Money>,
    pub maintenance_excess: Option<Money>,
    pub initial_margin: Option<Money>,
    pub variation_margin: Option<Money>,
    pub net_liquidation: Option<Money>,
    pub collateral: Vec<Collateral>,
}

/// One collateral balance under a segment, as reported (W2.2; Q12, Q13). A
/// balance moves nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collateral {
    pub direction: Direction,
    /// Exactly one of these, as on a holding.
    pub instrument_id: Option<String>,
    pub unresolved_identifiers: Vec<Identifier>,
    pub quantity: Quantity,
    pub value: Option<Money>,
    /// A fraction of the value, as reported.
    pub haircut: Option<Quantity>,
    pub value_after_haircut: Option<Money>,
    pub held_at: String,
    /// Whether the receiver may reuse it, as reported (contract v8).
    pub reusable: Option<bool>,
}

/// Whether collateral was posted by the account or received by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Posted,
    Received,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Posted => "posted",
            Direction::Received => "received",
        }
    }

    pub fn parse(text: &str) -> Option<Direction> {
        match text {
            "posted" => Some(Direction::Posted),
            "received" => Some(Direction::Received),
            _ => None,
        }
    }
}

/// One lot of a holding, as the custodian lists it (W2.3): its quantity
/// signed as the holding's, its cost as reported, sign included, and its
/// acquisition date, each absent where not reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lot {
    pub quantity: Quantity,
    pub cost: Option<Money>,
    pub acquired_date: String,
}

/// One encumbered sub-balance of a holding, as the source reports it (W2.3,
/// contract v8; reference/encumbrance-survey): its kind as the wire numbers
/// it, its quantity signed as the holding's, and the rest each empty or
/// absent where the source does not say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encumbrance {
    pub kind: i32,
    pub quantity: Quantity,
    pub available: Option<bool>,
    pub source_code: String,
    pub pledgee: String,
    pub held_at: String,
    pub segment: String,
    pub detail: String,
}

/// A reference to the raw record a row was converted from, in the writing
/// plugin's own storage (contract v11; decisions/028): carried, never
/// followed, here.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RawRecord {
    pub instance_id: String,
    pub key: String,
}

/// Where a value the plugin closed rather than read came from (contract
/// v11): the value's path in its message, the kind as the wire numbers it,
/// and its raw record, second source, person or rule.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Provenance {
    pub field: String,
    pub kind: i32,
    pub raw_record: Option<RawRecord>,
    pub source: String,
    pub person: String,
    pub rule: String,
}

/// A quantity not yet settled, and its value date (contract v11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub value_date: String,
    pub quantity: Quantity,
}

/// The fields a backfill may fill: those contract v11 added to a holding
/// row, each by its path and the version that added it (W2.4).
pub const BACKFILLED: [(&str, &str); 3] = [
    ("raw_record", "v11"),
    ("pending", "v11"),
    ("provenance", "v11"),
];

/// A backfill of a row already recorded (W2.4, contract v11): which row --
/// its statement, account, instrument or identifiers, and side -- the field
/// it fills with its value, and the raw record it was re-converted from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Amendment {
    pub statement_id: String,
    pub account_id: String,
    pub instrument_id: Option<String>,
    pub unresolved_identifiers: Vec<Identifier>,
    pub side: Side,
    pub contract_version: String,
    pub field: String,
    pub raw_record: Option<RawRecord>,
    pub pending: Vec<Pending>,
    pub provenance: Vec<Provenance>,
}

impl Amendment {
    /// Refuse a backfill naming a field no revision added to the row, or the
    /// wrong version for it, naming the field.
    pub fn validate(&self) -> Result<()> {
        match BACKFILLED.iter().find(|(field, _)| *field == self.field) {
            None => Err(StoreError::Edge(format!(
                "backfill.field {:?} is not a field a contract revision added to the row: {}",
                self.field,
                BACKFILLED.map(|(field, _)| field).join(", ")
            ))),
            Some((_, version)) if *version != self.contract_version => {
                Err(StoreError::Edge(format!(
                    "backfill.contract_version is {:?}; {} was added by {version}",
                    self.contract_version, self.field
                )))
            }
            Some(_) if self.field == "raw_record" && self.raw_record.is_none() => Err(
                StoreError::Edge("a backfill of raw_record carries the raw record".into()),
            ),
            Some(_) => Ok(()),
        }
    }

    /// Whether `cost` -- what the row carried, as first recorded and as
    /// amended -- already carries the field: a backfill fills only what the
    /// row did not carry.
    pub fn already_carried(&self, cost: &Cost) -> bool {
        match self.field.as_str() {
            "raw_record" => cost.raw_record.is_some(),
            "pending" => !cost.pending.is_empty(),
            "provenance" => !cost.provenance.is_empty(),
            _ => true,
        }
    }

    /// `cost` with the field filled from this backfill.
    pub fn applied_to(&self, cost: &Cost) -> Cost {
        let mut cost = cost.clone();
        match self.field.as_str() {
            "raw_record" => cost.raw_record = self.raw_record.clone(),
            "pending" => cost.pending = self.pending.clone(),
            "provenance" => cost.provenance = self.provenance.clone(),
            _ => {}
        }
        cost
    }

    /// The row's description in a refusal: its instrument or identifiers, and
    /// its side.
    pub fn describes(&self) -> String {
        let what = match &self.instrument_id {
            Some(instrument_id) => instrument_id.clone(),
            None => self
                .unresolved_identifiers
                .iter()
                .map(|identifier| format!("{}:{}", identifier.scheme, identifier.value))
                .collect::<Vec<_>>()
                .join(" "),
        };
        format!("{what}, {}", self.side)
    }
}

/// What a backfill did (W2.4).
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum Amended {
    /// Journaled beside the row as first recorded; the position behind it,
    /// where the row is the one that last stated it and it now says more,
    /// changed as `Settled` says.
    Amended(Settled),
    /// The row already carried the field, or this backfill was journaled
    /// already: nothing changed and nothing was added.
    Nothing,
}

/// What the custodian reported of a holding's cost and margin, each absent
/// where it reported none and never derived (W2.3; Q-A).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cost {
    /// A total.
    pub cost_basis: Option<Money>,
    /// Per unit, in the venue's unit.
    pub average_cost: Option<Money>,
    pub lots: Vec<Lot>,
    pub margin_requirement: Option<Money>,
    /// Available and not, with what the available figure is net of, and
    /// each encumbered sub-balance, all as reported, none derived (the product
    /// owner, 2026-10-01). Kept with the lots, on the row that last stated the
    /// position.
    pub available_quantity: Option<Quantity>,
    pub not_available_quantity: Option<Quantity>,
    pub available_basis: i32,
    pub encumbrances: Vec<Encumbrance>,
    /// The raw record the row was converted from, the provenance of each
    /// value the plugin closed, and the quantities pending by value date
    /// (contract v11): carried by the position from the row that last stated
    /// it, as the lots are, with what a backfill added to that row.
    pub raw_record: Option<RawRecord>,
    pub provenance: Vec<Provenance>,
    pub pending: Vec<Pending>,
}

/// Which side of an instrument a holding or a position is on.
///
/// Its own field rather than the sign alone, because a venue can report an
/// account's long and short of one instrument at once, and a key without the
/// side would let one overwrite the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Side {
    Long,
    Short,
}

impl Side {
    /// As the store keeps it, and as a cursor spells it. `long` sorts before
    /// `short`, which is the order positions are paged in.
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Long => "long",
            Side::Short => "short",
        }
    }

    pub fn parse(text: &str) -> Option<Side> {
        match text {
            "long" => Some(Side::Long),
            "short" => Some(Side::Short),
            _ => None,
        }
    }

    /// Whether a quantity's sign agrees with this side. Zero agrees with
    /// either: a position closed today is still on the side it was.
    pub fn admits(self, quantity: Quantity) -> bool {
        match self {
            Side::Long => quantity >= Quantity::ZERO,
            Side::Short => quantity <= Quantity::ZERO,
        }
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One row of one statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holding {
    pub holding_id: String,
    pub statement_id: String,
    pub account_id: String,

    /// Exactly one of these is populated. The store refuses both and neither.
    pub instrument_id: Option<String>,
    pub unresolved_identifiers: Vec<Identifier>,

    pub side: Side,

    /// The trade-date quantity, signed to match `side`.
    pub quantity: Quantity,

    /// Where the venue reported them, and absent where it did not.
    pub settle_date_quantity: Option<Quantity>,
    pub market_value: Option<Money>,

    /// The venue stated no currency, and the one here is the connector's.
    pub currency_assumed: bool,

    /// This position's value is also in the account's cash holding as the
    /// venue reports it (a money-market fund SnapTrade counts in cash). Both
    /// are kept as reported; the mark is what lets the book count it once.
    pub also_counted_in_cash: bool,

    /// Its cost, lots and margin requirement, as reported.
    pub cost: Cost,

    /// Whether a reader has asked the platform about the identifiers yet. Only
    /// meaningful on an unresolved row.
    pub escalated: bool,
}

impl Holding {
    pub fn resolved(&self) -> bool {
        self.instrument_id.is_some()
    }

    /// Refuse a row that describes nothing, or two things, or a side its
    /// quantity contradicts.
    ///
    /// Checked here rather than at each caller, because every inbound path
    /// lands in the store and a check spread across callers is a check enforced
    /// by whichever caller remembered.
    pub fn validate(&self) -> Result<()> {
        match (&self.instrument_id, self.unresolved_identifiers.is_empty()) {
            (Some(_), false) => return Err(StoreError::BothResolvedAndNot),
            (None, true) => return Err(StoreError::NeitherResolvedNorIdentified),
            _ => {}
        }
        // Refused rather than one of them believed: a short row stating a
        // positive quantity is a connector that got one of the two wrong, and
        // nothing here can tell which.
        if !self.side.admits(self.quantity) {
            return Err(StoreError::SideContradictsSign {
                side: self.side,
                quantity: self.quantity,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identifier {
    pub scheme: String,
    pub value: String,
    pub source: String,
}

/// What the custodian says an account holds of an instrument, on one side.
///
/// Custodial, and named so deliberately. It is derived from statements and is
/// the custodian's belief. Our own book, calculated from our own activity, does
/// not exist yet and is a different number; their disagreement is the entire
/// subject of reconciliation. A type called `Position` would make every reader
/// guess which one it had, and would quietly change meaning the day the second
/// one arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustodialPosition {
    pub account_id: String,
    pub instrument_id: String,
    pub side: Side,
    pub quantity: Quantity,
    pub settle_date_quantity: Option<Quantity>,
    pub market_value: Option<Money>,

    /// Its value is also in the account's cash holding, as the custodian
    /// reports it.
    pub also_counted_in_cash: bool,

    /// Its cost, lots and margin requirement as the row that last stated it
    /// reported them.
    pub cost: Cost,

    /// Which statement last set this, and what that statement's positions
    /// reflected. Together they say how current this is without a reader
    /// having to ask anything else.
    pub last_statement_id: String,
    pub as_of_date: String,
    pub updated_at_ns: i64,

    /// Its last change: a delivery at or below it is already in it.
    pub last_change: Change,

    /// A tombstone (W2.6, W3.9): moved off a placeholder, kept so a read of
    /// changes sees the removal, and read only by one.
    pub removed: bool,
}

impl CustodialPosition {
    /// Whether this was stated by a later statement than `standing`. W3.9.
    ///
    /// Where an account holds a position under a placeholder and another under
    /// the instrument that replaced it, one of them goes, and the later
    /// statement's stands. Later means the date the positions reflect first,
    /// because that is what a custodian states and what W2.2 keeps apart from
    /// when we read it; an ISO date compares correctly as text. On the same
    /// date, the one recorded later, since the custodian restated it. On both,
    /// the one already under the instrument, so moving changes nothing that
    /// cannot be told apart.
    ///
    /// The statement's own read time would be a finer tiebreak and is not on
    /// the position; the recording time is, and a statement is recorded when
    /// it is read.
    pub fn stated_later_than(&self, standing: &CustodialPosition) -> bool {
        (&self.as_of_date, self.updated_at_ns) > (&standing.as_of_date, standing.updated_at_ns)
    }

    /// Whether this says something different from `other` about what is held,
    /// which is what W2.6 announces. Which statement said it is not a change;
    /// a settle-date quantity that moved alone is, since a trade settling
    /// changes what is held settled.
    pub fn differs_from(&self, other: &CustodialPosition) -> bool {
        self.quantity != other.quantity
            || self.settle_date_quantity != other.settle_date_quantity
            || self.market_value != other.market_value
            || self.also_counted_in_cash != other.also_counted_in_cash
            || self.cost != other.cost
            || self.removed != other.removed
    }

    /// This position removed: kept under its key as a tombstone, holding
    /// nothing, so a reader of changes since learns it is gone (W3.9).
    pub fn tombstone(&self) -> CustodialPosition {
        CustodialPosition {
            quantity: Quantity::ZERO,
            settle_date_quantity: None,
            market_value: None,
            also_counted_in_cash: false,
            cost: Cost::default(),
            removed: true,
            ..self.clone()
        }
    }

    /// Where this sits in the order positions are paged in.
    pub fn key(&self) -> Key {
        Key {
            account_id: self.account_id.clone(),
            instrument_id: self.instrument_id.clone(),
            side: self.side,
        }
    }
}

/// A custodial position's key: account, instrument and side, which is also
/// the order a read pages through them in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key {
    pub account_id: String,
    pub instrument_id: String,
    pub side: Side,
}

impl Key {
    /// As a page's `next_cursor`: the key of its last row, whole.
    ///
    /// The whole key, because a cursor of the instrument alone skipped every
    /// row in a later account whose instrument sorted before it
    /// (kernel/position-paging-skips-rows). Each part is prefixed with its
    /// length, so no identifier's own characters can be read as a separator.
    pub fn cursor(&self) -> String {
        format!(
            "{}:{}{}:{}{}",
            self.account_id.len(),
            self.account_id,
            self.instrument_id.len(),
            self.instrument_id,
            self.side
        )
    }

    /// A cursor read back, or refused: one this store did not write would
    /// start the page somewhere nobody asked for.
    pub fn from_cursor(cursor: &str) -> Result<Key> {
        let unreadable = || StoreError::UnreadableCursor(cursor.to_string());
        let (account_id, rest) = length_prefixed(cursor).ok_or_else(unreadable)?;
        let (instrument_id, side) = length_prefixed(rest).ok_or_else(unreadable)?;
        Ok(Key {
            account_id: account_id.to_string(),
            instrument_id: instrument_id.to_string(),
            side: Side::parse(side).ok_or_else(unreadable)?,
        })
    }
}

fn length_prefixed(text: &str) -> Option<(&str, &str)> {
    let (length, rest) = text.split_once(':')?;
    let length: usize = length.parse().ok()?;
    Some((rest.get(..length)?, rest.get(length..)?))
}

/// What recording a statement did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opened {
    /// New. Its rows have not been seen.
    Opened,

    /// Already held. The rail redelivered, which is a no-op rather than a
    /// duplicate.
    AlreadyRecorded,
}

/// Whether a statement has everything it said was coming. W2.5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    /// Every row has landed, and this row is the one that completed it, as
    /// this change. Said once: a later row does not complete it again,
    /// because a subscriber's arithmetic should not depend on how many times
    /// it heard.
    JustCompleted(Change),

    /// Not yet, or already announced. Both are silence, and deliberately so.
    /// A statement whose rows never all arrive publishes nothing rather than
    /// publishing counts that are wrong.
    Nothing,
}

/// What recording a holding did to the position behind it.
#[derive(Debug, Clone, PartialEq)]
pub enum Settled {
    /// The row resolved and the position changed, or a move removed it.
    /// Carries what it was, so a subscriber renders a delta without keeping
    /// its own history; the change is on the position.
    Changed {
        position: CustodialPosition,
        previous_quantity: Quantity,
    },

    /// The row resolved and said exactly what the position already held.
    Unchanged { position: CustodialPosition },

    /// The row did not resolve, so no position moved. There is nothing to move
    /// until the deployment knows what it holds.
    Unresolved,
}

/// The counts W2.5 publishes, whenever somebody decides when that is.
///
/// Computed and available from the first commit. Nothing publishes them yet,
/// because nothing in the contract says when a statement has ended, and an
/// invented rule gets the unresolved count wrong, which the workflow calls the
/// number an operator actually watches. See `sdk-contract/statement-completion`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    pub received: u32,
    pub resolved: u32,
    pub unresolved: u32,
}

impl Counts {
    /// The postcondition the fixture states, as a method rather than a comment.
    pub fn consistent(&self) -> bool {
        self.resolved + self.unresolved == self.received
    }
}

/// A page of positions, and the rows that could not become one.
#[derive(Debug, Clone, Default)]
pub struct Page {
    pub positions: Vec<CustodialPosition>,
    pub unresolved: Vec<Holding>,
    pub next_cursor: String,

    /// The partition's number the page was read at.
    pub as_of: u64,
}

/// What a read of positions asks (W2.7).
#[derive(Debug, Clone)]
pub struct Read {
    pub scope: Scope,
    /// Empty: every account in the scope.
    pub account_id: String,
    pub include_unresolved: bool,
    pub limit: usize,
    pub cursor: String,
    /// Only the positions changed above it, tombstones included.
    pub since: Option<u64>,
}

/// What a read of completed statements asks (W2.9).
#[derive(Debug, Clone)]
pub struct StatementsRead {
    pub scope: Scope,
    /// Empty: every account in the scope.
    pub account_id: String,
    /// Empty: any date.
    pub as_of_date: String,
    pub limit: usize,
    pub cursor: String,
    /// Only those completed above it.
    pub since: Option<u64>,
}

/// A page of completed statements, each with its counts, in the order they
/// completed.
#[derive(Debug, Clone, Default)]
pub struct StatementPage {
    pub statements: Vec<(Statement, Counts)>,
    pub next_cursor: String,
    pub as_of: u64,
}

/// A statements page's cursor: the last one's completion number and its
/// identifier, which is the order they are read in.
pub fn statement_cursor(statement: &Statement) -> String {
    let sequence = statement
        .completed
        .as_ref()
        .map(|completed| completed.change.sequence)
        .unwrap_or_default();
    format!("{sequence}:{}", statement.statement_id)
}

/// A statements cursor read back, or refused.
pub fn from_statement_cursor(cursor: &str) -> Result<(u64, String)> {
    let unreadable = || StoreError::UnreadableCursor(cursor.to_string());
    let (sequence, statement_id) = cursor.split_once(':').ok_or_else(unreadable)?;
    Ok((
        sequence.parse().map_err(|_| unreadable())?,
        statement_id.to_string(),
    ))
}

/// One activity as the custodian stated it (W2.10, W2.12; contract v14):
/// kept as reported, whole, against its account. Nothing derives a position,
/// a lot or a figure from it (the spec's requirement 8), so the store keeps
/// the activity as the plugin encoded it and takes out only what a read
/// selects and orders by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    /// Minted by the street; empty until it is recorded.
    pub activity_id: String,
    pub account_id: String,
    pub external_account_id: String,
    pub source: String,
    /// The custodian's identifier: with the source and the account, what
    /// makes a redelivery recognisable.
    pub external_activity_id: String,
    pub trade_date: String,
    /// Its kind as the wire numbers it, its instrument (empty where it did not
    /// resolve) and its units where stated: read by the harness's lines, never
    /// to change what is held.
    pub kind: i32,
    pub instrument_id: String,
    pub units: Option<Quantity>,
    /// The CustodialActivity, encoded as it arrived.
    pub record: Vec<u8>,
    /// Its change and who caused it, the cause's time when it was recorded.
    pub recorded: Completed,
}

/// One re-resolution of an activity (W2.15, W2.16; contract v15): the
/// instrument the activity now resolves to and how, kept beside the activity
/// as first recorded, which it never changes (decisions/031).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReResolution {
    /// The activity it sits beside: filled by the store from the three
    /// below, which name it as it was recorded.
    pub activity_id: String,
    pub account_id: String,
    pub source: String,
    pub external_activity_id: String,
    /// Empty where the link it had been resolved by was removed.
    pub instrument_id: String,
    /// The Provenance, encoded as it arrived.
    pub provenance: Vec<u8>,
    /// When what resolves it was made, as the plugin sent it.
    pub resolved_at_ns: i64,
    /// Its change and who caused it, the cause's time when it was recorded.
    pub recorded: Completed,
}

/// What recording an activity, or a re-resolution of one, did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kept {
    /// New, numbered and recorded now: announced (W2.12).
    Recorded,
    /// Held already under its source, account and the custodian's
    /// identifier: the one held is answered, nothing changed, and nothing is
    /// announced again.
    AlreadyRecorded,
}

/// One sync status as the custody plugin published it (W2.13; contract v14),
/// against the account its external account is linked to, or none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncStatus {
    /// Empty where the external account is not linked.
    pub account_id: String,
    pub external_account_id: String,
    pub source: String,
    /// Its state as the wire numbers it, and the first date the source can
    /// read history from: what a read of activity answers (W2.11).
    pub state: i32,
    pub history_from: String,
    /// The SyncStatusEvent, encoded as it was published.
    pub record: Vec<u8>,
    pub recorded: Completed,
    /// Set by the store, never by a caller, on the first sync status it keeps
    /// for a connection and only there: why nothing is known of the
    /// connection's sync status before this one's recorded time
    /// (decisions/031, point 4). Empty on every later one.
    pub not_known_before: String,
}

/// Why nothing is known of a connection's sync status before the first one
/// the street kept (decisions/031, point 4): the street keeps each from
/// contract v14, and before then the dashboard held only the latest, in
/// memory, so there is nothing to backfill from.
pub const SYNC_STATUS_NOT_KNOWN_BEFORE: &str = "not known before: the street keeps each sync \
     status from the first it hears for a connection, and none was kept before it to backfill \
     from";

/// What a read of activity asks (W2.11).
#[derive(Debug, Clone)]
pub struct ActivitiesRead {
    pub scope: Scope,
    /// Empty: every account in the scope.
    pub account_id: String,
    /// Inclusive; empty for no bound on that side.
    pub trade_date_from: String,
    pub trade_date_to: String,
    pub limit: usize,
    pub cursor: String,
    /// Only those recorded above it, in the order recorded; without it, by
    /// trade date.
    pub since: Option<u64>,
}

/// A page of activity, the number it was read at, and the named account's
/// `history_from` as its latest sync status said it.
///
/// Its re-resolutions (W2.11, contract v15): by trade date, every one of the
/// activities answered; since a watermark, those recorded after it, merged
/// with the activities in the order recorded, a page holding at most the
/// read's limit of the two together, so each is answered once across pages.
#[derive(Debug, Clone, Default)]
pub struct ActivityPage {
    pub activities: Vec<Activity>,
    pub re_resolutions: Vec<ReResolution>,
    pub next_cursor: String,
    pub as_of: u64,
    pub history_from: String,
}

/// What a read of sync statuses asks (W2.14).
#[derive(Debug, Clone)]
pub struct SyncStatusesRead {
    pub scope: Scope,
    /// Empty: every account in the scope.
    pub account_id: String,
    pub limit: usize,
    pub cursor: String,
    /// Every one recorded above it, in the order recorded; without it, the
    /// latest of each connection's account.
    pub since: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct SyncStatusPage {
    pub statuses: Vec<SyncStatus>,
    pub next_cursor: String,
    pub as_of: u64,
}

/// Where an activity sits in the order it is read in: by trade date, or by
/// its number when read since a watermark. A page's cursor is its last row's
/// whole place, the date length-prefixed so no date's own characters are
/// read as a separator.
pub fn activity_cursor(activity: &Activity) -> String {
    format!(
        "{}:{}{}",
        activity.trade_date.len(),
        activity.trade_date,
        activity.recorded.change.sequence
    )
}

/// A page's cursor when read since a watermark, whose last item may be a
/// re-resolution: its number alone, the date part empty, which a read since a
/// watermark never compares.
pub fn sequence_cursor(sequence: u64) -> String {
    format!("0:{sequence}")
}

/// An activity cursor read back, or refused.
pub fn from_activity_cursor(cursor: &str) -> Result<(String, u64)> {
    let unreadable = || StoreError::UnreadableCursor(cursor.to_string());
    let (trade_date, sequence) = length_prefixed(cursor).ok_or_else(unreadable)?;
    Ok((
        trade_date.to_string(),
        sequence.parse().map_err(|_| unreadable())?,
    ))
}

/// The connection a sync status is of: its account, source and external
/// account, which is also the order the latest of each is read in. An
/// unlinked one's account is empty, so two unlinked connections stay two.
pub fn connection(status: &SyncStatus) -> (String, String, String) {
    (
        status.account_id.clone(),
        status.source.clone(),
        status.external_account_id.clone(),
    )
}

/// A sync statuses page's cursor: the last one's connection, each part
/// length-prefixed, and its number.
pub fn sync_status_cursor(status: &SyncStatus) -> String {
    format!(
        "{}:{}{}:{}{}:{}{}",
        status.account_id.len(),
        status.account_id,
        status.source.len(),
        status.source,
        status.external_account_id.len(),
        status.external_account_id,
        status.recorded.change.sequence
    )
}

/// A sync statuses cursor read back, or refused.
pub fn from_sync_status_cursor(cursor: &str) -> Result<((String, String, String), u64)> {
    let unreadable = || StoreError::UnreadableCursor(cursor.to_string());
    let (account_id, rest) = length_prefixed(cursor).ok_or_else(unreadable)?;
    let (source, rest) = length_prefixed(rest).ok_or_else(unreadable)?;
    let (external_account_id, sequence) = length_prefixed(rest).ok_or_else(unreadable)?;
    Ok((
        (
            account_id.to_string(),
            source.to_string(),
            external_account_id.to_string(),
        ),
        sequence.parse().map_err(|_| unreadable())?,
    ))
}

/// Where the street store keeps what it has been told.
pub trait Store: Send + Sync {
    /// Open a statement, or recognise one already held. W2.2.
    ///
    /// Carries a completion because zero is a legitimate row count. An account
    /// that holds nothing today is a real answer and a different one from "we
    /// did not read the account", so a statement promising no rows is complete
    /// the moment it opens. Waiting for a row that was never coming would leave
    /// it open forever, which reads as a stuck connector. A completion is
    /// numbered and recorded with `cause`.
    fn open(&self, statement: Statement, cause: &Cause) -> Result<(Statement, Opened, Completion)>;

    fn statement(&self, statement_id: &str) -> Result<Option<Statement>>;

    /// Persist a row, settle the position behind it, and say whether the
    /// statement is now complete. W2.3, W2.4 and the trigger for W2.5.
    ///
    /// One call because they are one transaction: a row recorded without its
    /// position moving, or a position moved without its row, are both states
    /// nothing else in this crate knows how to repair. Completion joins them
    /// for the same reason. Counting rows in one transaction and deciding in
    /// another is how a statement announces itself twice, or not at all.
    ///
    /// Each change -- the position's, the statement's completion -- takes the
    /// partition's next number in the same transaction, chained to the last
    /// of its kind for the account, and is recorded with `cause`, whose time
    /// is when it committed. A statement naming no account takes the row's;
    /// a row naming another than its statement's is refused.
    fn record(&self, holding: Holding, cause: &Cause) -> Result<(Settled, Completion)>;

    /// What a statement said, in counts. Available before anything publishes it.
    fn counts(&self, statement_id: &str) -> Result<Counts>;

    /// W2.7. Custodial positions in key order after `cursor`, within the
    /// read's scope, and optionally the rows that could not become one, which
    /// come with the first page only so a read across pages sees each once.
    /// With `since`, only those changed above it, tombstones included;
    /// without, no tombstone. Answers the number it was read at.
    fn page(&self, read: &Read) -> Result<Page>;

    /// W2.9. Completed statements within the read's scope, in the order they
    /// completed, each with its counts; and the number it was read at.
    fn statements(&self, read: &StatementsRead) -> Result<StatementPage>;

    fn custodial_position(
        &self,
        account_id: &str,
        instrument_id: &str,
        side: Side,
    ) -> Result<Option<CustodialPosition>>;

    /// Move every custodial position held under `replaced_id` onto
    /// `instrument_id`, each on its own side. W3.9.
    ///
    /// Where the account already holds `instrument_id` on that side, the
    /// position stated later stands
    /// ([`CustodialPosition::stated_later_than`]) and the other is removed.
    /// Returns what now stands under `instrument_id` because of the move, as
    /// W2.6 would announce it: a moved position is new under its
    /// instrument, so changed from nothing; one that displaced another is
    /// changed or not by what it states. One that lost to the position already
    /// there returns nothing, because nothing under the instrument changed.
    ///
    /// Holding rows are not touched. They are what a statement said, and they
    /// keep the placeholder they were recorded with.
    ///
    /// One transaction, so no reader sees an account holding a security twice
    /// or not at all. Each position under the placeholder stays as a
    /// tombstone, and each change -- the removal and what stands under the
    /// instrument -- is numbered and recorded with `cause`; a removal is
    /// returned too, as changed to nothing.
    fn move_positions(
        &self,
        replaced_id: &str,
        instrument_id: &str,
        cause: &Cause,
    ) -> Result<Vec<Settled>>;

    /// A backfill of a row already recorded (W2.4, contract v11): journaled
    /// as an amendment beside the row as first recorded, never an overwrite,
    /// with its cause and the raw record it was re-converted from. A field
    /// the row already carries, or a backfill journaled before for the same
    /// row, version and field, changes nothing. A row the statement never had
    /// is refused. Where the row is the one that last stated its position,
    /// the position takes what it now says as a change of its own, numbered
    /// and recorded with `cause`, in the same transaction.
    fn amend(&self, amendment: Amendment, cause: &Cause) -> Result<Amended>;

    /// Every instrument ID a custodial position is held under, each once, in
    /// order: what the sweep asks the instrument store about, for a record
    /// merged into another while the street was not listening (W3.9).
    fn instruments_held(&self) -> Result<Vec<String>>;

    /// Record an activity (W2.10, W2.12): numbered in the partition, chained
    /// to the account's last activity, and recorded with `cause`, whose time
    /// is when it was recorded; or, held already under its source, account
    /// and the custodian's identifier, the one held, changing nothing.
    /// Refused naming no account or no identifier.
    fn record_activity(&self, activity: Activity, cause: &Cause) -> Result<(Activity, Kept)>;

    /// W2.11. Activity within the read's scope, by trade date, or since a
    /// watermark in the order recorded; the number it was read at, and the
    /// named account's `history_from` from its latest sync status.
    fn activities(&self, read: &ActivitiesRead) -> Result<ActivityPage>;

    /// Re-resolve a recorded activity (W2.15, W2.16): the activity named by
    /// its source, account and the custodian's identifier, refused when none
    /// is; numbered in the partition and chained to the account's last
    /// re-resolution, apart from the activities, and recorded with `cause`;
    /// or, naming the instrument and provenance its latest resolution names,
    /// that answered as already recorded, changing nothing and taking no
    /// number.
    fn re_resolve(&self, re: ReResolution, cause: &Cause) -> Result<(ReResolution, Kept)>;

    /// Record a sync status (W2.13): every one heard is a record, numbered
    /// and chained to the last of its account's, '' for an unlinked one.
    fn record_sync_status(&self, status: SyncStatus, cause: &Cause) -> Result<SyncStatus>;

    /// W2.14. The latest sync status of each connection within the read's
    /// scope, or every one recorded since a watermark, in the order recorded.
    fn sync_statuses(&self, read: &SyncStatusesRead) -> Result<SyncStatusPage>;
}

/// Refuse an activity that names no account, source or identifier: the
/// sidecar refuses an unlinked one first and the wire requires the rest, so
/// this is the second line.
pub fn check_activity(activity: &Activity) -> Result<()> {
    if activity.account_id.is_empty() {
        return Err(StoreError::Edge(
            "an activity names no account: its external account is not linked".into(),
        ));
    }
    if activity.source.is_empty() {
        return Err(StoreError::Edge("source names no source".into()));
    }
    if activity.external_activity_id.is_empty() {
        return Err(StoreError::Edge(
            "activity.external_activity_id is empty; the custodian's identifier is what \
             recognises the same activity sent twice"
                .into(),
        ));
    }
    Ok(())
}

/// Refuse a re-resolution that names no account, source or identifier, says
/// not how it was resolved or when (W2.15): the sidecar refuses an unlinked
/// one first and the wire requires the rest, so this is the second line.
pub fn check_re_resolution(re: &ReResolution) -> Result<()> {
    if re.account_id.is_empty() {
        return Err(StoreError::Edge(
            "a re-resolution names no account: its external account is not linked".into(),
        ));
    }
    if re.source.is_empty() {
        return Err(StoreError::Edge("source names no source".into()));
    }
    if re.external_activity_id.is_empty() {
        return Err(StoreError::Edge(
            "external_activity_id is empty; a re-resolution names the activity it re-resolves"
                .into(),
        ));
    }
    let provenance = meridian_pb::v1::Provenance::decode(&re.provenance[..]).unwrap_or_default();
    let kind = meridian_pb::v1::ProvenanceKind::try_from(provenance.kind).ok();
    if !matches!(
        kind,
        Some(meridian_pb::v1::ProvenanceKind::Supplied | meridian_pb::v1::ProvenanceKind::Derived)
    ) {
        return Err(StoreError::Edge(
            "provenance.kind is neither supplied nor derived; a re-resolution is a person's link \
             or a named rule, never the custodian's word"
                .into(),
        ));
    }
    if re.resolved_at_ns == 0 {
        return Err(StoreError::Edge(
            "resolved_at_ns is 0; a re-resolution says when what resolves it was made".into(),
        ));
    }
    Ok(())
}

/// How a re-resolution names its activity in a refusal.
pub fn activity_named(re: &ReResolution) -> String {
    format!(
        "{} from {} on {}",
        re.external_activity_id, re.source, re.account_id
    )
}

/// What an activity's latest resolution names, its instrument and its
/// provenance encoded: its latest re-resolution's, or, with none, the
/// activity's own as first recorded -- its instrument and the provenance it
/// carried for it, none where the custodian's code resolved it.
pub fn latest_resolution(activity: &Activity, latest: Option<&ReResolution>) -> (String, Vec<u8>) {
    if let Some(latest) = latest {
        return (latest.instrument_id.clone(), latest.provenance.clone());
    }
    let own = meridian_domain::v1::CustodialActivity::decode(&activity.record[..])
        .ok()
        .and_then(|first| {
            first
                .provenance
                .into_iter()
                .find(|provenance| provenance.field == "instrument_id")
        })
        .map(|provenance| provenance.encode_to_vec())
        .unwrap_or_default();
    (activity.instrument_id.clone(), own)
}

/// Whether a re-resolution names what the activity's latest resolution
/// already names, so nothing is recorded.
pub fn already_resolved(
    activity: &Activity,
    latest: Option<&ReResolution>,
    re: &ReResolution,
) -> bool {
    let (instrument_id, provenance) = latest_resolution(activity, latest);
    instrument_id == re.instrument_id && provenance == re.provenance
}
