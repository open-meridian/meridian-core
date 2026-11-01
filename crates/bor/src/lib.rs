//! The deployment's book of record: its own positions, breaks and figures.
//!
//! W9, contract v8 (revision B of plans/the-book-holds-positions). The street
//! store holds what custodians say; this holds what the firm says, and the
//! two are kept apart on purpose (decisions/012): the custodian's statement
//! is a street record, and the book is verified against it, never loaded
//! from it. They share no key, join or transaction, and a break names the
//! street record it came from by value.
//!
//! # How an account's book is made
//!
//! An account enters once, with an opening balance a person answers for
//! (W9.1), journalled as movement lines opening every position, lot and
//! pending settlement from zero (W9.2). After that the book changes only by
//! its own entries -- an adjustment or a reversal resolving a break, a
//! merged record followed -- each a set of lines. Every difference an
//! `operations` plugin finds between the book and the street is a break,
//! recorded with its cause and resolved by a justified entry naming it or
//! closed with an explanation; never by an overwrite.
//!
//! # The journal and its projections
//!
//! [`journal`] is the record: append-only, partitioned by account, every
//! record an entry changes numbered from its partition's head in the entry's
//! own transaction. [`book`] replays an account's entries into its
//! positions, lots, pending settlements, breaks, figures and attributes; a
//! command is decided ([`decide`]) against that replay, under the partition's
//! lock, and the store keeps projections of it for reading, which
//! `meridian-bor rebuild` makes again from the journal alone.
//!
//! # How anything else reaches this
//!
//! Over the bus, and only over the bus ([`service`]). A consumer knows the
//! book's commands, queries and events; it does not know there is a
//! database. `make check-crate-boundaries` refuses a second component
//! linking against this crate.
//!
//! # What the book does not do
//!
//! It records; it does not police. It computes no margin, no projection, no
//! scenario and no return, and stores nothing derived as if it were a
//! record: figures are what was reported (W9.5), and a break's age is
//! derived from its dates by whoever reads it. Every quantity is an exact
//! decimal, summed and never rounded (decisions/023).

pub mod became;
pub mod book;
pub mod booking;
pub mod booking_service;
pub mod cash;
pub mod dates;
pub mod deadline;
pub mod decide;
pub mod factors;
pub mod ids;
pub mod journal;
pub mod lifecycle;
pub mod migrations;
pub mod numbers;
pub mod order_decide;
pub mod order_reads;
pub mod order_service;
pub mod orders;
pub mod postgres;
pub mod reads;
pub mod service;
pub mod snapshot;
pub mod store;
#[rustfmt::skip]
pub mod transitions;

mod memory;

#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod order_tests;
#[cfg(test)]
mod tests;

/// The test binary's allocator counts what it holds, for the replay
/// benchmark (contract v25, W9.10).
#[cfg(test)]
#[global_allocator]
static ALLOCATOR: snapshot::tests::Counting = snapshot::tests::Counting;

pub use book::Book;
pub use memory::MemoryStore;
pub use postgres::PostgresStore;
pub use store::{Store, StoreError};
