//! The lake's migration history.
//!
//! Applied once per release, by `meridian-lake migrate`; a start verifies,
//! taking no lock, and refuses a schema it does not recognise. The history
//! table is `lake_schema_migration`, apart from every other store's, because
//! a deployment may put every store in one schema.

use postgres::Transaction;

use crate::store::{Result, StoreError};

pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static str,
}

/// In order, and never reordered or edited after release.
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "lake",
    sql: include_str!("../migrations/0001_lake.sql"),
}];

pub const HISTORY: &str = "\
CREATE TABLE IF NOT EXISTS lake_schema_migration (
    version       bigint PRIMARY KEY,
    name          text   NOT NULL,
    applied_at_ns bigint NOT NULL
)";

pub fn latest() -> i64 {
    MIGRATIONS.last().map(|m| m.version).unwrap_or(0)
}

/// What a start does: refuse a schema this binary does not recognise,
/// naming the fix.
pub fn verify(applied: Option<i64>) -> Result<()> {
    let latest = latest();
    match applied {
        None => Err(StoreError::Unavailable(format!(
            "the lake's database has no schema; this binary expects {latest}. Run `meridian-lake \
             migrate` before starting."
        ))),
        Some(at) if at < latest => Err(StoreError::Unavailable(format!(
            "the lake's database is at schema version {at} and this binary expects {latest}. Run \
             `meridian-lake migrate` before starting."
        ))),
        Some(at) if at > latest => Err(StoreError::SchemaAhead(format!(
            "the lake's database is at schema version {at}, newer than this binary understands \
             ({latest}). Run the release that migrated it, or restore a database at {latest}."
        ))),
        Some(_) => Ok(()),
    }
}

pub fn record(tx: &mut Transaction<'_>, migration: &Migration, at_ns: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO lake_schema_migration (version, name, applied_at_ns) VALUES ($1, $2, $3)",
        &[&migration.version, &migration.name, &at_ns],
    )
    .map_err(|failed| StoreError::Unavailable(failed.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_start_refuses_a_schema_it_does_not_recognise() {
        assert!(verify(None).is_err());
        assert!(verify(Some(latest())).is_ok());
        assert!(matches!(
            verify(Some(latest() + 1)),
            Err(StoreError::SchemaAhead(_))
        ));
    }
}
