//! The street store's migration history: what exists, what has run, and what a
//! binary will run against.
//!
//! The replica creates its schema on start and that is right for it: it holds
//! what the platform can send again, so a fresh schema loses nothing. The
//! street store holds statements and positions nothing can reconstruct, and
//! `CREATE TABLE IF NOT EXISTS` adds no column to a table that already exists,
//! so the first change that is not an addition has nowhere to go.
//!
//! # Applying is not starting
//!
//! [`apply`] runs once per release, from `meridian-street migrate`. Starting
//! calls [`verify`], which reads one table and takes no lock, and refuses to
//! serve against a schema this binary does not recognise. Two reasons, and the
//! second is the one that bites: N replicas starting together would race to
//! apply the same migration, and a process that migrates on start is a process
//! that silently changes a customer's database because somebody restarted a
//! pod.
//!
//! # Adoption
//!
//! A database made before this history existed has the tables and no record of
//! them. Baseline is recorded for such a database rather than re-running the
//! first migration, and everything after it is guarded so it may run against
//! either state.

use postgres::Transaction;

use crate::store::{Result, StoreError};

pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static str,
}

/// In order, and never reordered or edited after release: the record of what
/// ran names a version, and editing one makes that record a lie.
///
/// Which is why migration 1 is still called `ledger` and its file is still
/// `0001_ledger.sql`, after decision 012 renamed the component to `street`.
/// Every database that has run it holds that name in `schema_migration`, so
/// changing it here would not rename anything -- it would make this table
/// disagree with every deployment, which is the one thing the record exists
/// to prevent. The name of a migration is history, not documentation.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "ledger",
        sql: include_str!("../migrations/0001_ledger.sql"),
    },
    Migration {
        version: 2,
        name: "custodial_position_and_completion",
        sql: include_str!("../migrations/0002_custodial_position_and_completion.sql"),
    },
];

pub const HISTORY: &str = "\
CREATE TABLE IF NOT EXISTS schema_migration (
    version     bigint PRIMARY KEY,
    name        text   NOT NULL,
    applied_at_ns bigint NOT NULL
)";

pub fn latest() -> i64 {
    MIGRATIONS.last().map(|m| m.version).unwrap_or(0)
}

/// What a start does. Reads one table, takes no lock, and answers with a
/// sentence naming the fix rather than failing later on a query.
pub fn verify(applied: Option<i64>) -> Result<()> {
    let latest = latest();
    match applied {
        None => Err(StoreError::Unavailable(format!(
            "the street store's database has no schema; it is at no version and this binary \
             expects {latest}. Run `meridian-street migrate` before starting."
        ))),
        Some(at) if at < latest => Err(StoreError::Unavailable(format!(
            "the street store's database is at schema version {at} and this binary expects \
             {latest}. Run `meridian-street migrate` before starting."
        ))),
        // Ahead, which is a rollback: the database has been migrated by a newer
        // release. Refused rather than tolerated, because this binary does not
        // know what that release changed and its queries may already be wrong.
        Some(at) if at > latest => Err(StoreError::Unavailable(format!(
            "the street store's database is at schema version {at}, which is newer than this \
             binary understands ({latest}). Run the release that migrated it, or restore \
             a database at {latest}."
        ))),
        Some(_) => Ok(()),
    }
}

/// Record a migration as applied, in the same transaction that ran it.
pub fn record(tx: &mut Transaction<'_>, migration: &Migration, at_ns: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO schema_migration (version, name, applied_at_ns) VALUES ($1, $2, $3)",
        &[&migration.version, &migration.name, &at_ns],
    )
    .map_err(|failed| StoreError::Unavailable(failed.to_string()))?;
    Ok(())
}
