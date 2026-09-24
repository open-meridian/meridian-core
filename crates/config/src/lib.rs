//! The conductor's configuration store: what a deployment admin authors.
//!
//! Accounts; the three dimensions of access -- user groups, account groups,
//! access groups -- and the permissions joining them; external-account links;
//! who has signed in; and which plugins have reported what they carry. Every
//! record a deployment admin authors lives here, beside plugin settings, and
//! the platform receives none of it (spec/deployment-dashboard-and-access).
//!
//! # Why the conductor holds a store after all
//!
//! It held none, on the argument that a control process accumulating a store
//! becomes a fourth thing to migrate and the one nobody writes migrations
//! for. The product owner ruled on 2026-09-21 that configuration lives with
//! the conductor, as it did in v1, so this store was written with its
//! migration history from its first commit, and answers that argument rather
//! than ignoring it.
//!
//! # How anything else reaches it
//!
//! Over the bus, in the `config` domain. The dashboard writes by command and
//! reads by query; a sidecar reads its own plugin's part. Evaluating access is
//! [`meridian_access`], which the dashboard links too; this crate's store it
//! does not, and `make check-crate-boundaries` keeps it that way.

pub mod ids;
pub mod migrations;
pub mod postgres;
pub mod rules;
pub mod service;
pub mod store;

mod memory;

pub use memory::MemoryStore;
pub use meridian_access::DEPLOYMENT_ADMIN;
pub use postgres::PostgresStore;
pub use service::{
    configuration, deployment_admin, install_named_administrator, serve, Clock, SystemClock,
    Upstream,
};
pub use store::{KnownPlugin, LocalAccount, Snapshot, Store, StoreError, Withdrawal};

#[cfg(test)]
mod tests;
