//! What the book holds, and what any store of it must provide.
//!
//! Two things, and the relationship between them is the whole design (W9.8).
//!
//! The **journal**: every act the book took, an entry in its account's
//! partition, each record it changed numbered from that partition's head in
//! the entry's own transaction. Append-only: a correction is a later entry,
//! and the Postgres store's trigger refuses an update or a delete of an entry
//! whoever asks.
//!
//! The **projections**: positions, lots and pending settlements, breaks,
//! figures and attributes, each the sum of an account's entries, kept for
//! reading and rebuilt from the journal by `meridian-bor rebuild`. A command
//! is decided against the account replayed from its journal, under the
//! partition's lock, so a projection is never the thing a decision reads.

use std::collections::{BTreeMap, BTreeSet};

use meridian_domain::v1::{
    AccountAttributes, AccountFigures, BookPosition, Break, BreakState, MarginAgreementRef,
    Watermark,
};
use meridian_pb::v1::RefusalReason;

use crate::book::{Book, Changes};
use crate::journal::Entry;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("the book is unavailable: {0}")]
    Unavailable(String),

    /// The database was migrated by a newer release than this binary: the
    /// one refusal at start that waiting never fixes.
    #[error("{0}")]
    SchemaAhead(String),

    /// A refusal the plugin acts on by its code (W9.1 to W9.7; the
    /// typed-operations refusal catalogue, contract v8).
    #[error("{words}")]
    Refused {
        reason: RefusalReason,
        words: String,
        /// Each field the command left out, by its path, for
        /// REFUSAL_REASON_INCOMPLETE (contract v9); empty for every other.
        fields: Vec<String>,
    },

    /// A command or a read that cannot stand as sent, with no code: the words
    /// say what to fix.
    #[error("{0}")]
    Invalid(String),

    /// A plugin's read naming an account outside its read scope (W4.11). The
    /// sidecar refuses it first; this is the second line.
    #[error("{0} is not in this plugin's read scope")]
    OutOfScope(String),

    #[error("{0} is not a cursor this store wrote; start again from the first page")]
    UnreadableCursor(String),

    /// A command the book reads as malformed, naming each field it got
    /// wrong (contract v22: INVALID_ARGUMENT, no reason code): an order
    /// naming both a quantity and a cash quantity, or neither; a child on
    /// another side; a fill with no settlement date.
    #[error("{words}")]
    Malformed { fields: Vec<String>, words: String },

    /// A command naming its record by ID alone whose account is outside the
    /// write scope the sidecar stamped on it (contract v22, W4.9): refused
    /// PERMISSION_DENIED with no reason code, nothing changed.
    #[error("{0}")]
    Denied(String),

    /// A venue's fact already applied, recognised by its venue identifiers
    /// (requirement 14): answered as it stands, nothing applied.
    #[error("the fact was applied already")]
    Repeated(Vec<u8>),

    /// A venue's fact the account's book cannot place -- its placement not
    /// the sender's, or its account outside the sender's scope -- to be kept
    /// as an unplaced fact naming no account (W11.13).
    #[error("{words}")]
    Unplace { reason: i32, words: String },

    /// A refusal the book recorded (W11.8), as it is said on the bus.
    #[error("{0}")]
    Recorded(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

impl StoreError {
    /// A key the account's book holds for another command (Q12).
    pub fn conflict(idempotency_key: &str, account_id: &str) -> StoreError {
        StoreError::refused(
            RefusalReason::IdempotencyConflict,
            format!(
                "account {account_id}'s book holds idempotency key {idempotency_key:?} for \
                 another command; a key names one command, and the same command again is \
                 answered as the first was"
            ),
        )
    }

    pub fn refused(reason: RefusalReason, words: impl Into<String>) -> StoreError {
        StoreError::Refused {
            reason,
            words: words.into(),
            fields: Vec::new(),
        }
    }

    /// An entry missing what the book requires for tax tracking, valuation,
    /// confirmation or settlement, naming each field by its path (W9.1,
    /// W9.7, contract v9): nothing applied.
    pub fn incomplete(what: &str, fields: Vec<String>) -> StoreError {
        let words = format!(
            "{what} is incomplete: the book requires {}; supply each before it is sent",
            fields.join(", ")
        );
        StoreError::Refused {
            reason: RefusalReason::Incomplete,
            words,
            fields,
        }
    }

    /// As the bus carries it: a refusal's code ahead of its words, which the
    /// sidecar puts beside the status (open point 13).
    pub fn on_the_bus(&self) -> String {
        match self {
            StoreError::Refused {
                reason,
                words,
                fields,
            } => meridian_bus::refusal_naming(*reason as i32, fields, words),
            StoreError::Malformed { fields, words } => meridian_bus::refusal_with_status(
                meridian_bus::RefusalStatus::InvalidArgument,
                fields,
                words,
            ),
            StoreError::Denied(words) => meridian_bus::refusal_with_status(
                meridian_bus::RefusalStatus::PermissionDenied,
                &[],
                words,
            ),
            StoreError::Recorded(said) => said.clone(),
            other => other.to_string(),
        }
    }
}

/// The partition every account is in to start (decisions/024, as noted
/// 2026-10-01): partitions are data, and an account's is recorded with it.
pub const FIRST_PARTITION: &str = "P0";

/// The control partition, which holds no account; its sequence in force is
/// stamped on every entry. From contract v22 it holds the trading
/// instructions that are not a deployment admin's on one account (W11.15).
pub const CONTROL_PARTITION: &str = "control";

/// The control partition's book, as the store keys it: no account's, never
/// listed among the accounts, and read by no plugin's scope.
pub const CONTROL_ACCOUNT: &str = "~control";

/// Where a venue's fact naming no account the book can place it in is kept
/// (W11.13): an unplaced fact with an empty account, delivered to no plugin
/// and counted on its connectivity plugin's admin view. Never listed among
/// the accounts, and read by no plugin's scope.
pub const NO_ACCOUNT: &str = "~unplaced";

/// Whether a key the store holds a book under is an account's.
pub fn is_account(key: &str) -> bool {
    !key.starts_with('~')
}

/// The partition a book key starts in.
pub fn first_partition_of(key: &str) -> &'static str {
    if key == CONTROL_ACCOUNT {
        CONTROL_PARTITION
    } else {
        FIRST_PARTITION
    }
}

/// A record a command may name by its ID alone (contract v22, W4.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Record {
    Order,
    Placement,
    ChangeRequest,
    Locate,
    /// An execution, which an accept or a DK names (contract v23).
    Execution,
    /// A booking and an alert, which a hand-off, a correction or an
    /// acknowledgement names (contract v24).
    Booking,
    Alert,
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

    /// Whether a record of `account_id` answers a read naming `named`.
    pub fn answers(&self, named: &str, account_id: &str) -> bool {
        (named.is_empty() || named == account_id) && self.holds(account_id)
    }
}

/// A watermark as the book reads one: a sequence per partition.
pub type Mark = BTreeMap<String, u64>;

pub fn mark_of(watermark: Option<&Watermark>) -> Option<Mark> {
    watermark.map(|watermark| {
        watermark
            .partitions
            .iter()
            .map(|held| (held.partition.clone(), held.sequence))
            .collect()
    })
}

pub fn watermark_of(mark: &Mark) -> Watermark {
    Watermark {
        partitions: mark
            .iter()
            .map(
                |(partition, sequence)| meridian_domain::v1::PartitionSequence {
                    partition: partition.clone(),
                    sequence: *sequence,
                },
            )
            .collect(),
    }
}

/// How a page is sized: 100 when none is asked, and at most the read's
/// page_size entry allows, answered at the bound rather than refused (as the
/// street's reads are); `bound` is that entry's, from `meridian_pb::bounds`.
pub fn page_limit(page_size: i32, bound: meridian_pb::bounds::Range) -> usize {
    match page_size {
        size if size <= 0 => 100,
        size => (size as i64).min(bound.most) as usize,
    }
}

/// W9.10.
#[derive(Debug, Clone)]
pub struct PositionsRead {
    pub scope: Scope,
    pub account_id: String,
    pub since: Option<Mark>,
    /// By replay of the journal: the end of this business date, as known at
    /// `at` or now (Q22).
    pub business_date: String,
    pub at: Option<Mark>,
    pub limit: usize,
    pub cursor: String,
}

/// W9.11.
#[derive(Debug, Clone)]
pub struct BreaksRead {
    pub scope: Scope,
    pub account_id: String,
    pub states: Vec<BreakState>,
    pub since: Option<Mark>,
    pub limit: usize,
    pub cursor: String,
}

/// W9.12.
#[derive(Debug, Clone)]
pub struct FiguresRead {
    pub scope: Scope,
    pub account_id: String,
    pub agreement: Option<MarginAgreementRef>,
    pub from_date: String,
    pub to_date: String,
    pub since: Option<Mark>,
    pub at: Option<Mark>,
    pub limit: usize,
    pub cursor: String,
}

/// W9.14.
#[derive(Debug, Clone)]
pub struct AttributesRead {
    pub scope: Scope,
    pub account_id: String,
    pub since: Option<Mark>,
    pub limit: usize,
    pub cursor: String,
}

/// W11.17: the orders and the records the order path delivers, as of now
/// or changed since a watermark.
#[derive(Debug, Clone)]
pub struct OrdersRead {
    pub scope: Scope,
    pub account_id: String,
    pub since: Option<Mark>,
    pub limit: usize,
    pub cursor: String,
    /// Contract v23 (W11.17): only executions in this state, 0 for every
    /// record; unmatched answers those still waiting for an answer.
    pub execution_state: i32,
}

/// A page of them: each kind in its own list, paged as one stream in the
/// order orders, placements, executions, unplaced facts.
#[derive(Debug, Clone, Default)]
pub struct OrdersPage {
    pub orders: Vec<meridian_domain::v1::Order>,
    pub placements: Vec<meridian_domain::v1::Placement>,
    pub executions: Vec<meridian_domain::v1::Execution>,
    pub unplaced: Vec<meridian_domain::v1::UnplacedFact>,
    pub next_cursor: String,
    pub as_of: Mark,
}

/// W11.17: one order's journal from its first entry.
#[derive(Debug, Clone)]
pub struct HistoryRead {
    pub scope: Scope,
    pub account_id: String,
    pub order_id: String,
    pub limit: usize,
    pub cursor: String,
}

/// W11.18: the locates.
#[derive(Debug, Clone)]
pub struct LocatesRead {
    pub scope: Scope,
    pub account_id: String,
    pub since: Option<Mark>,
    pub limit: usize,
    pub cursor: String,
}

/// W11.31: the bookings, as of now or changed since a watermark, filtered by
/// state (contract v24).
#[derive(Debug, Clone)]
pub struct BookingsRead {
    pub scope: Scope,
    pub account_id: String,
    pub since: Option<Mark>,
    /// 0 for every state.
    pub state: i32,
    pub limit: usize,
    pub cursor: String,
}

/// W11.35: the alerts, as of now or changed since a watermark, filtered by
/// state and kind (contract v24).
#[derive(Debug, Clone)]
pub struct AlertsRead {
    pub scope: Scope,
    pub account_id: String,
    pub since: Option<Mark>,
    pub state: i32,
    pub kind: i32,
    pub limit: usize,
    pub cursor: String,
}

/// W11.16: the trading instructions in force, whatever the reader's scope.
#[derive(Debug, Clone)]
pub struct InstructionsRead {
    pub since: Option<Mark>,
    pub limit: usize,
    pub cursor: String,
}

#[derive(Debug, Clone, Default)]
pub struct Page<T> {
    pub records: Vec<T>,
    pub next_cursor: String,
    pub as_of: Mark,
}

/// What a command's decision is handed: the account replayed from its
/// journal, and the head of its partition, from which its changes are
/// numbered.
pub type Decide<'a> = dyn FnMut(&Book, u64) -> Result<Decided> + 'a;

/// A decision: the entry, numbered, and the account after it.
pub struct Decided {
    pub entry: Entry,
    pub book: Book,
    pub changes: Changes,
    /// The command's answer, encoded, kept with the entry so a duplicate is
    /// answered with it (decisions/024).
    pub reply: Vec<u8>,
}

/// What acting did.
pub enum Acted {
    /// The same command again, by its message identifier or its idempotency
    /// key: answered with the first's reply, nothing applied.
    Duplicate(Vec<u8>),
    Committed(Box<Decided>),
}

/// Where the book keeps its journal and projections.
pub trait Store: Send + Sync {
    /// The command path (W9.8). Under the account's partition lock, in one
    /// transaction: answer a duplicate with the first's reply; otherwise
    /// replay the account's journal, ask `decide` for the entry, append it,
    /// move the partition's head to its last number, and write the
    /// projections it changed. Nothing is decided against a projection.
    ///
    /// A key is unique per account (Q12): the same command under it again --
    /// `request`, its encoding, the same -- is a duplicate; a different one
    /// is refused, `REFUSAL_REASON_IDEMPOTENCY_CONFLICT`, nothing applied.
    fn act(
        &self,
        account_id: &str,
        message_id: &str,
        idempotency_key: &str,
        request: &[u8],
        decide: &mut Decide<'_>,
    ) -> Result<Acted>;

    /// An account's journal, in order.
    fn journal(&self, account_id: &str) -> Result<Vec<Entry>>;

    /// An account's book as its journal stands: from contract v25 (W9.10)
    /// from its latest snapshot and the journal's tail where the store keeps
    /// snapshots, else replayed whole.
    fn book(&self, account_id: &str) -> Result<crate::book::Book> {
        let entries = self.journal(account_id)?;
        let partition = entries
            .first()
            .map(|entry| entry.partition.clone())
            .unwrap_or_else(|| first_partition_of(account_id).to_string());
        crate::book::Book::replay(account_id, &partition, &entries)
    }

    /// An account's book at the end of a business date as known at a
    /// watermark (W9.10, Q22): the entries numbered at or below `at` on the
    /// account's partition, those effective on or before `until`, from the
    /// latest snapshot that holds none later where the store keeps them.
    /// `at` is the partition's watermark as a read names it per partition.
    fn book_as_of(
        &self,
        account_id: &str,
        at: &Mark,
        until: Option<&str>,
    ) -> Result<crate::book::Book> {
        let entries = self.journal(account_id)?;
        let partition = entries
            .first()
            .map(|entry| entry.partition.clone())
            .unwrap_or_else(|| FIRST_PARTITION.to_string());
        let at = at.get(&partition).copied().unwrap_or(0);
        crate::book::Book::as_of(account_id, &partition, &entries, Some(at), until)
    }

    /// Every account the book holds an entry for.
    fn accounts(&self) -> Result<Vec<String>>;

    /// The head of every partition, as one read sees them.
    fn heads(&self) -> Result<Mark>;

    /// Accounts holding a position, tombstones aside, under an instrument.
    fn accounts_holding(&self, instrument_id: &str) -> Result<Vec<String>>;

    /// Every instrument ID a position or an open break is held under, each
    /// once, in order: what the sweep asks the instrument store about, for a
    /// record merged into another while the book was not listening (W9.9).
    fn instruments_held(&self) -> Result<Vec<String>>;

    fn positions(&self, read: &PositionsRead) -> Result<Page<BookPosition>>;
    fn breaks(&self, read: &BreaksRead) -> Result<Page<Break>>;
    fn figures(&self, read: &FiguresRead) -> Result<Page<AccountFigures>>;
    fn attributes(&self, read: &AttributesRead) -> Result<Page<AccountAttributes>>;

    /// Rebuild every projection from the journal (`meridian-bor rebuild`),
    /// and say how many entries were replayed.
    fn rebuild(&self) -> Result<usize>;

    /// Each code's cash instrument as resolved (contract v18), its own
    /// record.
    fn cash_instruments(&self) -> Result<Vec<meridian_domain::money::Resolution>>;

    /// Keep a code's resolution, once: a code resolved already is left.
    fn keep_cash_instrument(&self, resolution: &meridian_domain::money::Resolution) -> Result<()>;

    /// Every ISO 4217 code an amount or a base currency the book holds is in.
    fn currency_codes(&self) -> Result<Vec<String>>;

    // ── The order path (W11, contract v22) ──

    /// The account a record named by its ID alone is in, where the book
    /// holds it.
    fn owner(&self, record: Record, id: &str) -> Result<Option<String>>;

    fn orders(&self, read: &OrdersRead) -> Result<OrdersPage>;
    fn order_history(&self, read: &HistoryRead) -> Result<Page<meridian_domain::v1::EntryMeta>>;
    fn locates(&self, read: &LocatesRead) -> Result<Page<meridian_domain::v1::Locate>>;
    fn instructions(
        &self,
        read: &InstructionsRead,
    ) -> Result<Page<meridian_domain::v1::TradingInstruction>>;

    /// Each live order whose own time in force ended at or before `now_ns`
    /// and is not yet ending, with its account (W11.13).
    fn expiring(&self, now_ns: i64) -> Result<Vec<(String, String)>>;

    // ── Booking (W11.26 to W11.37, contract v24) ──

    fn bookings(&self, read: &BookingsRead) -> Result<Page<meridian_domain::v1::Booking>>;
    fn alerts(&self, read: &AlertsRead) -> Result<Page<meridian_domain::v1::Alert>>;

    /// The live block orders naming `member_order_id`, each with its account
    /// (W11.1): a member in one is refused another.
    fn blocks_naming(&self, member_order_id: &str) -> Result<Vec<(String, String)>>;

    /// Every live order with its instrument, across the deployment: what the
    /// escape hatch's count reads (W11.37), naming no account to its reader.
    fn live_order_instruments(&self) -> Result<Vec<String>>;
}
