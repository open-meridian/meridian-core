//! The configuration store's migration history.
//!
//! Written from the first commit, because the argument against the conductor
//! holding a store was that nobody would write its migrations. Same shape as
//! the street store's: [`apply`](crate::PostgresStore::migrate) runs once per
//! release from `meridian-conductor migrate`, and starting only verifies.
//!
//! The history table is `config_schema_migration`, not the street store's
//! `schema_migration`, because a deployment may put both stores in one schema.

use postgres::Transaction;

use crate::store::{Result, StoreError};

pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static str,
}

/// In order, and never reordered or edited after release: the record of what
/// ran names a version, and editing one makes that record a lie.
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "config",
    sql: include_str!("../migrations/0001_config.sql"),
}];

pub const HISTORY: &str = "\
CREATE TABLE IF NOT EXISTS config_schema_migration (
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
            "the configuration store's database has no schema; it is at no version and this binary \
             expects {latest}. Run `meridian-conductor migrate` before starting."
        ))),
        Some(at) if at < latest => Err(StoreError::Unavailable(format!(
            "the configuration store's database is at schema version {at} and this binary expects \
             {latest}. Run `meridian-conductor migrate` before starting."
        ))),
        // Ahead, which is a rollback: the database has been migrated by a newer
        // release. Refused rather than tolerated, because this binary does not
        // know what that release changed and its queries may already be wrong.
        Some(at) if at > latest => Err(StoreError::Unavailable(format!(
            "the configuration store's database is at schema version {at}, which is newer than this \
             binary understands ({latest}). Run the release that migrated it, or restore \
             a database at {latest}."
        ))),
        Some(_) => Ok(()),
    }
}

/// Record a migration as applied, in the same transaction that ran it.
pub fn record(tx: &mut Transaction<'_>, migration: &Migration, at_ns: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO config_schema_migration (version, name, applied_at_ns) VALUES ($1, $2, $3)",
        &[&migration.version, &migration.name, &at_ns],
    )
    .map_err(|failed| StoreError::Unavailable(failed.to_string()))?;
    Ok(())
}
