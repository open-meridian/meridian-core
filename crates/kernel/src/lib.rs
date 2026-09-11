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
//! # What is missing, and why it is missing
//!
//! W2.5 closes a statement and publishes its counts. Nothing in the contract
//! says when a statement has ended: there is no close command, the open carries
//! no expected row count, and no message marks the last row. The counts are
//! computed here and nothing publishes them, because an invented rule gets the
//! unresolved count wrong and that is the number the workflow says an operator
//! actually watches. See `sdk-contract/statement-completion`.

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
pub use store::{Counts, Holding, Opened, Position, Settled, Statement, Store, StoreError};
