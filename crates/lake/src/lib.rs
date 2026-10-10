//! The lake: what sources say about entities (W10, contract v18;
//! spec/the-lake; plans/the-lake-prices-the-book, the lake's 1a).
//!
//! # What it keeps
//!
//! Rows of the lake's data types -- prices and bars in the 1a -- each under
//! one envelope ([`row`]): its row key, the dataset it came from, its
//! subjects (the deployment's own entities), its valid time and business
//! date, the source's own times, when it was recorded, its version and its
//! sequence. Append-only and bitemporal: a correction is a new version of
//! the row key, never an update, so a read as of a recorded time answers what
//! the lake knew then. Each dataset is a partition numbered from its head in
//! the change's own transaction, with no holes, and each row names the
//! previous sequence of its subject, so a reader narrowed to some subjects
//! sees no false gap.
//!
//! # What it applies
//!
//! The deployment's data configuration ([`config`]), as the conductor
//! publishes it: each dataset's licence -- kept or served, its retention --
//! and each plugin instance's entitlement to it, every field or some; and the
//! deployment's priority among the datasets a default read takes, which the
//! lake keeps and journals itself. A read the lake cannot answer from what it
//! keeps is wanted of the instance serving the dataset (W10.7).
//!
//! # How anything reaches it
//!
//! Over the bus, and only over it ([`service`]). `make
//! check-crate-boundaries` refuses a second component linking against this
//! crate. Licensed data never leaves the deployment: nothing here reaches
//! the platform, and no row, licence or entitlement is in any report.

pub mod config;
pub mod migrations;
pub mod postgres;
pub mod read;
pub mod row;
pub mod service;
pub mod store;

mod memory;

#[cfg(test)]
mod tests;

pub use memory::MemoryStore;
pub use postgres::PostgresStore;
pub use store::{Store, StoreError};
