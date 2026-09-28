//! What the street store holds, and what any store of it must provide.
//!
//! Three things, and the relationships between them are the whole design.
//!
//! A **statement** is one read of one source at one moment. It is identified by
//! the rail's own name for it, so a redelivery is recognisable rather than
//! duplicated.
//!
//! A **holding** is one row of one statement: an account, an instrument or the
//! identifiers we could not turn into one, a quantity and a value. Rows are
//! never deleted and never merged. A statement's rows are what it said.
//!
//! A **custodial position** is what the custodian says an account holds of an
//! instrument. It is derived from the latest statement's rows rather than
//! accumulated across statements, because a holding row states a quantity as of
//! a date and not a change. Adding them up would double one that appeared in
//! two reads. It is not our own book, which does not exist yet and will have its
//! own name when it does.

use crate::amounts::{Money, Quantity, Refused};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("the street store is unavailable: {0}")]
    Unavailable(String),

    #[error("no statement {0}")]
    UnknownStatement(String),

    #[error(
        "a holding names both an instrument and unresolved identifiers, which cannot both be true"
    )]
    BothResolvedAndNot,

    #[error("a holding names neither an instrument nor any identifier, so it describes nothing")]
    NeitherResolvedNorIdentified,

    /// A quantity or an amount outside what the wire carries, named.
    #[error(transparent)]
    OutOfRange(#[from] Refused),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// One read of one source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub statement_id: String,
    pub source: String,

    /// The rail's own name for this statement. With `source`, it is what makes
    /// a redelivery recognisable.
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

    pub quantity: Quantity,
    pub market_value: Money,

    /// Whether a reader has asked the platform about the identifiers yet. Only
    /// meaningful on an unresolved row.
    pub escalated: bool,
}

impl Holding {
    pub fn resolved(&self) -> bool {
        self.instrument_id.is_some()
    }

    /// Refuse a row that describes nothing, or two things.
    ///
    /// Checked here rather than at each caller, because every inbound path
    /// lands in the store and a check spread across callers is a check enforced
    /// by whichever caller remembered.
    pub fn validate(&self) -> Result<()> {
        match (&self.instrument_id, self.unresolved_identifiers.is_empty()) {
            (Some(_), false) => Err(StoreError::BothResolvedAndNot),
            (None, true) => Err(StoreError::NeitherResolvedNorIdentified),
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identifier {
    pub scheme: String,
    pub value: String,
    pub source: String,
}

/// What the custodian says an account holds of an instrument.
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
    pub quantity: Quantity,
    pub market_value: Money,

    /// Which statement last set this, and what that statement's positions
    /// reflected. Together they say how current this is without a reader
    /// having to ask anything else.
    pub last_statement_id: String,
    pub as_of_date: String,
    pub updated_at_ns: i64,
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
    /// which is what W2.6 announces. Which statement said it is not a change.
    pub fn differs_from(&self, other: &CustodialPosition) -> bool {
        self.quantity != other.quantity || self.market_value != other.market_value
    }
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
    /// Every row has landed, and this row is the one that completed it. Said
    /// once: a later row does not complete it again, because a subscriber's
    /// arithmetic should not depend on how many times it heard.
    JustCompleted,

    /// Not yet, or already announced. Both are silence, and deliberately so.
    /// A statement whose rows never all arrive publishes nothing rather than
    /// publishing counts that are wrong.
    Nothing,
}

/// What recording a holding did to the position behind it.
#[derive(Debug, Clone, PartialEq)]
pub enum Settled {
    /// The row resolved and the position changed. Carries what it was, so a
    /// subscriber renders a delta without keeping its own history.
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
}

/// Where the street store keeps what it has been told.
pub trait Store: Send + Sync {
    /// Open a statement, or recognise one already held. W2.2.
    ///
    /// Carries a completion because zero is a legitimate row count. An account
    /// that holds nothing today is a real answer and a different one from "we
    /// did not read the account", so a statement promising no rows is complete
    /// the moment it opens. Waiting for a row that was never coming would leave
    /// it open forever, which reads as a stuck connector.
    fn open(&self, statement: Statement) -> Result<(Statement, Opened, Completion)>;

    fn statement(&self, statement_id: &str) -> Result<Option<Statement>>;

    /// Persist a row, settle the position behind it, and say whether the
    /// statement is now complete. W2.3, W2.4 and the trigger for W2.5.
    ///
    /// One call because they are one transaction: a row recorded without its
    /// position moving, or a position moved without its row, are both states
    /// nothing else in this crate knows how to repair. Completion joins them
    /// for the same reason. Counting rows in one transaction and deciding in
    /// another is how a statement announces itself twice, or not at all.
    fn record(&self, holding: Holding, now_ns: i64) -> Result<(Settled, Completion)>;

    /// What a statement said, in counts. Available before anything publishes it.
    fn counts(&self, statement_id: &str) -> Result<Counts>;

    /// W2.7. Custodial positions, and optionally the rows that could not
    /// become one.
    fn page(
        &self,
        account_id: &str,
        include_unresolved: bool,
        limit: usize,
        cursor: &str,
    ) -> Result<Page>;

    fn custodial_position(
        &self,
        account_id: &str,
        instrument_id: &str,
    ) -> Result<Option<CustodialPosition>>;

    /// Move every custodial position held under `replaced_id` onto
    /// `instrument_id`. W3.9.
    ///
    /// Where the account already holds `instrument_id`, the position stated
    /// later stands ([`CustodialPosition::stated_later_than`]) and the other
    /// is removed. Returns what now stands under `instrument_id` because of
    /// the move, as W2.6 would announce it: a moved position is new under its
    /// instrument, so changed from nothing; one that displaced another is
    /// changed or not by what it states. One that lost to the position already
    /// there returns nothing, because nothing under the instrument changed.
    ///
    /// Holding rows are not touched. They are what a statement said, and they
    /// keep the placeholder they were recorded with.
    ///
    /// One transaction, so no reader sees an account holding a security twice
    /// or not at all.
    fn move_positions(&self, replaced_id: &str, instrument_id: &str) -> Result<Vec<Settled>>;

    /// Every instrument ID a custodial position is held under that is a
    /// placeholder (`LCL-`), each once, in order. What the sweep asks about.
    fn placeholder_instruments(&self) -> Result<Vec<String>>;
}
