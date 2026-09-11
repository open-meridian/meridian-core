//! What the ledger holds, and what any store of it must provide.
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
//! A **position** is what an account currently holds of an instrument. It is
//! derived from the latest statement's rows rather than accumulated across
//! statements, because a holding row states a quantity as of a date and not a
//! change. Adding them up would double a position that appeared in two reads.

use crate::amounts::{Money, Overflow, Quantity};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("the ledger is unavailable: {0}")]
    Unavailable(String),

    #[error("no statement {0}")]
    UnknownStatement(String),

    #[error(
        "a holding names both an instrument and unresolved identifiers, which cannot both be true"
    )]
    BothResolvedAndNot,

    #[error("a holding names neither an instrument nor any identifier, so it describes nothing")]
    NeitherResolvedNorIdentified,

    #[error(transparent)]
    Overflow(#[from] Overflow),
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
    pub currency: String,

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

/// What an account currently holds of an instrument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Position {
    pub account_id: String,
    pub instrument_id: String,
    pub quantity: Quantity,
    pub market_value: Money,
    pub currency: String,

    /// Which statement last set this, and what that statement's positions
    /// reflected. Together they say how current this is without a reader
    /// having to ask anything else.
    pub last_statement_id: String,
    pub as_of_date: String,
    pub updated_at_ns: i64,
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

/// What recording a holding did to the position behind it.
#[derive(Debug, Clone, PartialEq)]
pub enum Settled {
    /// The row resolved and the position changed. Carries what it was, so a
    /// subscriber renders a delta without keeping its own history.
    Changed {
        position: Position,
        previous_quantity: Quantity,
    },

    /// The row resolved and said exactly what the position already held.
    Unchanged { position: Position },

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
    pub positions: Vec<Position>,
    pub unresolved: Vec<Holding>,
    pub next_cursor: String,
}

/// Where the ledger keeps what it has been told.
pub trait Store: Send + Sync {
    /// Open a statement, or recognise one already held. W2.2.
    fn open(&self, statement: Statement) -> Result<(Statement, Opened)>;

    fn statement(&self, statement_id: &str) -> Result<Option<Statement>>;

    /// Persist a row and settle the position behind it. W2.3 and W2.4.
    ///
    /// One call because they are one transaction: a row recorded without its
    /// position moving, or a position moved without its row, are both states
    /// nothing else in this crate knows how to repair.
    fn record(&self, holding: Holding, now_ns: i64) -> Result<Settled>;

    /// What a statement said, in counts. Available before anything publishes it.
    fn counts(&self, statement_id: &str) -> Result<Counts>;

    /// W2.7. Positions, and optionally the rows that could not become one.
    fn page(
        &self,
        account_id: &str,
        include_unresolved: bool,
        limit: usize,
        cursor: &str,
    ) -> Result<Page>;

    fn position(&self, account_id: &str, instrument_id: &str) -> Result<Option<Position>>;
}
