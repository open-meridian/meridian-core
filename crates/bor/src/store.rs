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

    /// A command refused by its partition's epoch: another writer took the
    /// book over, and this one steps down (step 3a; ruling 1060).
    #[error("{0}")]
    Fenced(#[from] meridian_changes::Fenced),

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

    /// A decision asked for entries' bodies the book does not hold
    /// (contract v26, W9.10): the store loads them from the journal and
    /// decides again ([`with_bodies`]). Never leaves the store.
    #[error("the book holds no body for entries {0:?}")]
    BodiesNeeded(Vec<String>),

    /// An account found, under its partition's lock, in another partition
    /// than the one its act was queued in (step 3d; the resilience spec's
    /// section 6.3): the act goes on in that one. Never leaves the store.
    #[error("the account is in partition {0}")]
    Moved(String),

    /// An act on a partition another writer group owns (step 3d; the
    /// resilience spec's section 6.7): handed to that group's writer.
    #[error("partition {partition} is writer group {group}'s to write; this member's group does not own it")]
    NotOwned { partition: String, group: u32 },

    /// An account's first act, refused because its owner label could not
    /// be read to place it (step 3d; section 6.3, ruled 2026-11-13): never
    /// placed by its ID instead. UNAVAILABLE, with a retry-after.
    #[error("{0}")]
    NotPlaced(String),

    /// An act on an account between its move's close and its open (step
    /// 3d; the resilience spec's section 6.8, the hold): UNAVAILABLE, with
    /// a retry-after, until the open.
    #[error("{0}")]
    Held(String),

    /// An act refused while something else under way stands in its way
    /// (step 3d: a move of the same account), naming the field:
    /// FAILED_PRECONDITION.
    #[error("{words}")]
    Precondition { field: &'static str, words: String },
}

/// How many times a decision may ask for bodies before it is a fault: each
/// round loads every body the decision named at once.
pub const BODY_ROUNDS: usize = 16;

/// `decide` against `book`, loading from `load` each entry's body it asks
/// for and deciding again (contract v26, W9.10; ruling 165): a decision is a
/// pure function of the book, so deciding again with the bodies held is the
/// decision a book holding every body makes.
pub fn with_bodies<T>(
    book: &mut Book,
    mut load: impl FnMut(&str, &[String]) -> Result<Vec<(String, crate::journal::Body)>>,
    mut decide: impl FnMut(&Book) -> Result<T>,
) -> Result<T> {
    for _ in 0..BODY_ROUNDS {
        match decide(book) {
            Err(StoreError::BodiesNeeded(entry_ids)) => {
                let loaded = load(&book.account_id, &entry_ids)?;
                if loaded.len() != entry_ids.len() {
                    return Err(StoreError::Unavailable(format!(
                        "account {}'s journal holds {} of the {} entries a decision named",
                        book.account_id,
                        loaded.len(),
                        entry_ids.len()
                    )));
                }
                book.hold_bodies(loaded);
            }
            other => return other,
        }
    }
    Err(StoreError::Unavailable(format!(
        "a decision on account {} asked for entries' bodies {BODY_ROUNDS} times",
        book.account_id
    )))
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
            StoreError::NotPlaced(words) => meridian_bus::refusal_status_retrying(
                meridian_bus::RefusalStatus::Unavailable,
                PLACEMENT_RETRY_SECONDS,
                words,
            ),
            StoreError::Precondition { field, words } => meridian_bus::refusal_with_status(
                meridian_bus::RefusalStatus::FailedPrecondition,
                &[field.to_string()],
                words,
            ),
            // Held past the hold (section 6.8): OVERLOADED, its retry-after.
            StoreError::Held(words) => meridian_bus::refusal_retrying(
                meridian_pb::v1::RefusalReason::Overloaded as i32,
                crate::moves::HELD_RETRY_SECONDS,
                words,
            ),
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

/// How long an account's first act waits before asking again when its
/// owner label could not be read (step 3d).
pub const PLACEMENT_RETRY_SECONDS: u32 = 5;

/// The conductor's accounts read the book asks at an account's first
/// placement (matrix row ReadAccountsForPlacement, ruled 2026-11-13).
pub const READ_ACCOUNTS: &str = "platform.config.query.accounts";

/// The book's count of partitions in the environment (step 3d; section
/// 6.3; `bor.partitions`, the variable ruled 2026-11-13): `P0` to `P<n-1>`.
pub const PARTITIONS_VAR: &str = "MERIDIAN_BOR_PARTITIONS";

/// The most partitions a book takes, as the most writer groups.
pub const PARTITIONS_MOST: u32 = meridian_changes::groups::MOST;

/// The count of partitions the chart said; 1 where it said nothing.
pub fn partitions_from(said: Option<&str>) -> std::result::Result<u32, String> {
    match said.map(str::trim).filter(|said| !said.is_empty()) {
        None => Ok(1),
        Some(text) => text
            .parse::<u32>()
            .ok()
            .filter(|n| (1..=PARTITIONS_MOST).contains(n))
            .ok_or_else(|| {
                format!("{PARTITIONS_VAR} is {text:?}; a whole number of partitions from 1 to {PARTITIONS_MOST}")
            }),
    }
}

/// A partition's name: `P<n>`.
pub fn partition_named(n: u32) -> String {
    format!("P{n}")
}

/// The partition a new account is placed in (step 3d; section 6.3): by
/// rendezvous hashing of its placement key -- its owner label when set,
/// else its ID -- over `P0` to `P<count-1>`, so a fund's or a household's
/// accounts land together, and raising the count moves none placed before.
pub fn placement_of(key: &str, count: u32) -> String {
    partition_named(meridian_changes::groups::owner(key, count.max(1)))
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

/// The watermark a read of `account_id` is answered at (step 3d): its own
/// partition's head alone where the book has several, since a record of the
/// account is numbered in its partition and no other's number is comparable
/// with it. A follower that read an account with nothing to hand on is had
/// to its partition's head, never another partition's (found by e2e-orders
/// at two partitions: a venue's SDK took P0's head for an account in P1, and
/// passed over the account's first placement as had). No partition where
/// the account is not placed yet; every partition for a read of no account,
/// and with one partition, as ever.
pub fn watermark_for(store: &dyn Store, account_id: &str, mark: &Mark) -> Watermark {
    if account_id.is_empty() || store.partition_count() < 2 {
        return watermark_of(mark);
    }
    let own: Mark = store
        .partition_of(account_id)
        .and_then(|partition| mark.get(&partition).map(|head| (partition, *head)))
        .into_iter()
        .collect();
    watermark_of(&own)
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

/// What a store tells of a batch's commits, once they commit (step 3d,
/// group commit): the epoch they committed under, and each decision in its
/// order in the partition.
pub type Committed = Box<dyn Fn(u64, &[&Decided]) + Send + Sync>;

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

    /// Have `observer` told of each batch's commits, in order, once they
    /// commit (step 3d, group commit), and answer whether this store tells
    /// it. One that does not leaves its wrapper to tell each act's own.
    fn on_commit(&self, observer: Committed) -> bool {
        let _ = observer;
        false
    }

    /// Whether this store's writer group writes `account_id`'s partition
    /// (step 3d; section 6.3): a book of another group's is never held in
    /// this member's memory, which that group's commits never reach.
    fn writes(&self, account_id: &str) -> bool {
        let _ = account_id;
        true
    }

    /// The writer group `account_id`'s commands are its owner's by, decided
    /// from its placement as this store last found it, never read from the
    /// record (step 3d; the ownership pre-check decided 2026-11-13): none
    /// with one writer group, where nothing is handed on.
    fn writer_group_of(&self, account_id: &str) -> Option<u32> {
        let _ = account_id;
        None
    }

    /// An account's move settled where it crossed databases (step 3d;
    /// section 6.4, "a move between databases copies the data"): once its
    /// open is journalled in the new partition's database, every other
    /// database lets go of what it projected of the account, keeping its
    /// entries. Idempotent; nothing for a store of one database.
    fn settle_move(&self, account_id: &str) -> Result<()> {
        let _ = account_id;
        Ok(())
    }

    /// The writer group that owns `partition`, as this store last read it
    /// (step 3d): 0 with one group.
    fn group_of_partition(&self, partition: &str) -> u32 {
        let _ = partition;
        0
    }

    /// An account's move warmed (step 3d; section 6.8): where its target
    /// partition is in another database, everything the account's database
    /// holds of it copied there before the close, and the digest of its
    /// last entry in each answered -- (source, target). None within one
    /// database, where the target reads the same rows.
    fn warm_account(
        &self,
        account_id: &str,
        target_partition: &str,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let _ = (account_id, target_partition);
        Ok(None)
    }

    /// The records the `~control` journal owes (step 3d; section 6.3,
    /// option (c)): each takeover and finding kept beside the journal and
    /// not yet in it, and each partition whose writer group is not the one
    /// its last assignment named. A store keeping none owes none.
    fn control_records_owed(&self) -> Result<Vec<crate::control::Owed>> {
        Ok(Vec::new())
    }

    /// Whether `account_id` is to be placed before its first act (step 3d;
    /// section 6.3): an account the store has not placed, where it has more
    /// than one partition. Never the book's own keys.
    fn placement_needed(&self, account_id: &str) -> Result<bool> {
        let _ = account_id;
        Ok(false)
    }

    /// Place `account_id` by `key` -- its owner label, else its ID -- where
    /// nothing placed it yet, and answer its partition; an account placed
    /// already keeps its own, whatever the key.
    fn place_first(&self, account_id: &str, key: &str) -> Result<String> {
        let _ = (account_id, key);
        Ok(FIRST_PARTITION.to_string())
    }

    /// How many partitions the book places accounts over (step 3d;
    /// `bor.partitions`): 1, the default, and a booking is as it always was.
    fn partition_count(&self) -> u32 {
        1
    }

    /// The partition `account_id` is in, where it is placed.
    fn partition_of(&self, account_id: &str) -> Option<String> {
        let _ = account_id;
        None
    }

    /// An account's journal, in order.
    fn journal(&self, account_id: &str) -> Result<Vec<Entry>>;

    /// The writer's epoch, adopted from the `control` partition where none
    /// was yet (step 3a): what a starting book does before its first
    /// command, and what a store not started as a writer does at its first.
    fn adopt(&self) -> Result<u64>;

    /// The epoch this store's commands commit under; 0 before it adopted one.
    fn epoch(&self) -> u64;

    /// Take the book over: raise every partition's epoch in one transaction,
    /// record who took it over and why (step 3b, `WriterTookOver`), and
    /// adopt it. Every command another writer commits under the old one is
    /// refused.
    fn take_over(&self, by: &meridian_changes::TookOver) -> Result<u64>;

    /// The `control` partition's epoch now, as the record says (step 3b): a
    /// writer steps down once it moves past its own.
    fn head_epoch(&self) -> Result<u64>;

    /// Follow the database's timeline (contract v30, stage C; the
    /// resilience spec's section 3): on a new one every partition's epoch
    /// raised as a takeover raises it and adopted, the takeover recorded
    /// with that cause. A store with no database never moves.
    fn follow_timeline(&self) -> Result<meridian_changes::timeline::Timeline> {
        Ok(meridian_changes::timeline::Timeline::Same)
    }

    /// A page of `partition`'s change log after `after` (step 3a): its
    /// journal's entries in sequence, each one change, at most `most_bytes`.
    fn read_changes(
        &self,
        partition: &str,
        after: u64,
        most_bytes: usize,
    ) -> std::result::Result<meridian_changes::ReadChangesReply, meridian_changes::ReadError>;

    /// The bodies of the named entries of an account's journal (contract
    /// v26, W9.10), each with its entry: what a decision asks for that the
    /// book does not hold. An entry the journal does not have is left out.
    fn bodies(
        &self,
        account_id: &str,
        entry_ids: &[String],
    ) -> Result<Vec<(String, crate::journal::Body)>> {
        Ok(self
            .journal(account_id)?
            .into_iter()
            .filter(|entry| entry_ids.contains(&entry.entry_id))
            .map(|entry| (entry.entry_id, entry.body))
            .collect())
    }

    /// An account's book as its journal stands: from contract v25 (W9.10)
    /// from its latest snapshot and the journal's tail where the store keeps
    /// snapshots, else replayed whole.
    fn book(&self, account_id: &str) -> Result<crate::book::Book> {
        let entries = self.journal(account_id)?;
        let partition = entries
            .first()
            .map(|entry| entry.partition.clone())
            .unwrap_or_else(|| first_partition_of(account_id).to_string());
        crate::book::Book::replay_lean(account_id, &partition, &entries)
    }

    /// The book's health per account (W9.10, contract v25; ruling 155): its
    /// journal's length, the entries a command replays after its latest
    /// snapshot, and when that snapshot was taken -- `only` that account, or
    /// the accounts after `after` in order, at most `limit`. A store keeping
    /// no snapshots replays the whole journal, and says so.
    fn book_health(
        &self,
        only: Option<&str>,
        after: &str,
        limit: usize,
    ) -> Result<Vec<meridian_domain::v1::AccountBookHealth>> {
        let mut out = Vec::new();
        for account in self.accounts()? {
            if account.as_str() <= after || only.is_some_and(|only| only != account) {
                continue;
            }
            if out.len() == limit {
                break;
            }
            let entries = self.journal(&account)?.len() as u64;
            out.push(meridian_domain::v1::AccountBookHealth {
                account_id: account,
                journal_entries: entries,
                entries_since_snapshot: entries,
                last_snapshot_at_ns: 0,
            });
        }
        Ok(out)
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
