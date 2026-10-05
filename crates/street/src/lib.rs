//! The deployment's street store: statements, holdings and positions, and
//! from contract v14 the custodian's activity and each sync status heard.
//!
//! W2's street half. A connector reads a brokerage, opens a statement, and
//! publishes one row per account, instrument and side; this records them,
//! moves the positions behind them, and answers what is held.
//!
//! # The idea the whole workflow rests on
//!
//! There are two beliefs about what is held, ours and the custodian's. This
//! store holds the custodian's, which is what `street` says and what `kernel`
//! used to say the opposite of: in the v1 vocabulary this project adopted,
//! `meridian_kernel` is the book of record. Decision 012 corrected it. Our own
//! book is calculated from our own activity, does not exist yet, and will not
//! be called the same thing when it does. What to do when the two disagree is
//! a later slice and nothing here pretends otherwise.
//!
//! # How anything else reaches this
//!
//! Over the bus, and only over the bus. A consumer knows commands, queries and
//! events; it does not know there is a database, which one, or what shape it is
//! in.
//!
//! That is what makes this store's storage a private decision. The custodian's
//! side is append-heavy, kept for audit, and tolerant of delay; our own book,
//! when it exists, is small, transactional, and on the path of every decision.
//! Those want different storage, and choosing separately is only possible while
//! nothing outside knows what either one is. A second component linking against
//! this crate's store would make the schema the interface, and from then on
//! changing it would be everybody's problem.
//!
//! `make check-crate-boundaries` enforces it, because nobody argues against the
//! rule. What happens is that somebody adds a dependency for convenience and
//! nothing objects.
//!
//! # Six rules that are easy to get quietly wrong
//!
//! **A position is replaced, not accumulated.** A holding row states a quantity
//! as of a date; it is not a change to one. Adding rows up would double
//! anything that appeared in two statements, and the result looks plausible.
//!
//! **A merged record's positions move; its rows do not.** A row names the
//! deployment's record for what was reported, one minted for identifiers
//! nothing matched as any other. When a person merges that record into
//! another, the positions move onto the one that stays (W3.9) and the rows
//! keep the ID they were recorded with, because they record what was reported
//! (a placeholder replaced by its `INS-` ID, before contract v10, the same).
//!
//! **An unresolved row is recorded and moves nothing.** Dropping it would lose
//! the only evidence that something was held. Guessing at the instrument would
//! be worse. So it is kept, it updates no position, and [`positions`] hands it
//! back beside the positions so the gap is visible where the holdings are.
//!
//! **Every change is numbered, and none is lost.** A change a reader hears
//! takes the street partition's next number in its own transaction and names
//! the previous of its kind for its account, so a reader that heard only some
//! accounts tells a gap in its own; a reader catches up by reading what
//! changed since a number, and a removed position stays as a tombstone so it
//! is read too. Every read answers within the reader's scope (W4.11).
//!
//! **What was not reported stays unreported.** A market value, a settle-date
//! quantity or a margin figure the venue did not give is absent, never zero:
//! zero is a thing a venue can say, and a store that wrote it for silence
//! would put words in the custodian's mouth.
//!
//! **No floating point, anywhere.** A quantity is an integer with its own scale
//! on the wire and a `numeric` in the store, and an amount is that with its
//! currency (decisions/023); a value is kept at the scale it was stated with
//! and compared as a number. [`amounts`] has no constructor from a float and
//! no conversion into one.
//!
//! # How a statement ends
//!
//! It says how many rows will follow, and it is complete when that many have
//! landed. Nothing else marks the end: rows arrive as separate messages and
//! none is distinguishable as the last, so before W2.2 carried a count the
//! store was asked to announce the completion of something whose end it could
//! not observe.
//!
//! Two consequences, both deliberate. A statement whose rows never all arrive
//! is never announced, because counts published early are wrong and wrong
//! quietly, and the unresolved figure is the one an operator watches. And rows
//! beyond the count do not announce it again, because a subscriber's arithmetic
//! should not depend on how many times it heard.

pub mod activity;
pub mod amounts;
pub mod ids;
pub mod migrations;
pub mod positions;
pub mod postgres;
pub mod record;
pub mod service;
pub mod store;

mod memory;

pub use activity::{list_activities, list_sync_statuses, record_activity, record_sync_status};
pub use amounts::{Money, Quantity};
pub use memory::MemoryStore;
pub use positions::{list_positions, list_statements};
pub use postgres::PostgresStore;
pub use record::{move_positions, open_statement, record_holding, Recorded};
pub use store::{
    ActivitiesRead, Activity, ActivityPage, Amended, Amendment, Cause, Chain, Change, Collateral,
    Completed, Completion, Cost, Counts, CustodialPosition, Direction, Encumbrance, Figures,
    Holding, Kept, Key, Lot, Opened, Pending, Provenance, RawRecord, Scope, Settled, Side,
    Statement, Store, StoreError, SyncStatus, SyncStatusPage, SyncStatusesRead, PARTITION,
    SYNC_STATUS_NOT_KNOWN_BEFORE,
};
