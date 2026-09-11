//! The deployment's ledger: statements, holdings and positions.
//!
//! W2's kernel half. A connector reads a brokerage, opens a statement, and
//! publishes one row per account and instrument; this records them, moves the
//! positions behind them, and answers what is held.
//!
//! # The idea the whole workflow rests on
//!
//! The kernel holds our belief and the rail holds the custodian's. This is how
//! the custodian's belief arrives. What to do when the two disagree is a later
//! slice and nothing here pretends otherwise.
//!
//! # How anything else reaches this
//!
//! Over the bus, and only over the bus. A consumer knows commands, queries and
//! events; it does not know there is a database, which one, or what shape it is
//! in.
//!
//! That is what makes the ledger's storage a private decision. The custodian's
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
//! # Three rules that are easy to get quietly wrong
//!
//! **A position is replaced, not accumulated.** A holding row states a quantity
//! as of a date; it is not a change to one. Adding rows up would double
//! anything that appeared in two statements, and the result looks plausible.
//!
//! **An unresolved row is recorded and moves nothing.** Dropping it would lose
//! the only evidence that something was held. Guessing at the instrument would
//! be worse. So it is kept, it updates no position, and [`positions`] hands it
//! back beside the positions so the gap is visible where the holdings are.
//!
//! **No floating point, anywhere.** Quantities and money are integers scaled by
//! 1e8 on the wire and in the store, so there is no conversion to get wrong.
//! [`amounts`] has no constructor from a float and no conversion into one.
//!
//! # How a statement ends
//!
//! It says how many rows will follow, and it is complete when that many have
//! landed. Nothing else marks the end: rows arrive as separate messages and
//! none is distinguishable as the last, so before W2.2 carried a count the
//! kernel was asked to announce the completion of something whose end it could
//! not observe.
//!
//! Two consequences, both deliberate. A statement whose rows never all arrive
//! is never announced, because counts published early are wrong and wrong
//! quietly, and the unresolved figure is the one an operator watches. And rows
//! beyond the count do not announce it again, because a subscriber's arithmetic
//! should not depend on how many times it heard.

pub mod amounts;
pub mod ids;
pub mod positions;
pub mod postgres;
pub mod record;
pub mod service;
pub mod store;

mod memory;

pub use amounts::{Money, Quantity};
pub use memory::MemoryStore;
pub use positions::list_positions;
pub use postgres::PostgresStore;
pub use record::{open_statement, record_holding, Recorded};
pub use store::{
    Completion, Counts, CustodialPosition, Holding, Opened, Settled, Statement, Store, StoreError,
};
