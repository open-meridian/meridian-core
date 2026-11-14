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
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "lake",
        sql: include_str!("../migrations/0001_lake.sql"),
    },
    Migration {
        version: 2,
        name: "closed_day",
        sql: include_str!("../migrations/0002_closed_day.sql"),
    },
    // Contract v28's quality states (W10.12; ruling 423).
    Migration {
        version: 3,
        name: "quality",
        sql: include_str!("../migrations/0003_quality.sql"),
    },
    // The latest in force chosen in SQL (found on dev 2026-10-26).
    Migration {
        version: 4,
        name: "latest",
        sql: include_str!("../migrations/0004_latest.sql"),
    },
    // The latest read served by an index (ruling 770).
    Migration {
        version: 5,
        name: "latest_index",
        sql: include_str!("../migrations/0005_latest_index.sql"),
    },
    // The datasets' listing's counts served by indexes (ruling 816).
    Migration {
        version: 6,
        name: "dataset_counts",
        sql: include_str!("../migrations/0006_dataset_counts.sql"),
    },
    // An underlying's expirations read from the listing alone (856, 858).
    Migration {
        version: 7,
        name: "listing",
        sql: include_str!("../migrations/0007_listing.sql"),
    },
    // The listing's quality counts, served without counting every row
    // (contract v29; rulings 619 and 816).
    Migration {
        version: 8,
        name: "quality_counts",
        sql: include_str!("../migrations/0008_quality_counts.sql"),
    },
    Migration {
        version: 9,
        name: "change_log",
        sql: include_str!("../migrations/0009_change_log.sql"),
    },
    Migration {
        version: 10,
        name: "writer_took_over",
        sql: include_str!("../migrations/0010_writer_took_over.sql"),
    },
    Migration {
        version: 11,
        name: "control_log",
        sql: include_str!("../migrations/0011_control_log.sql"),
    },
    // Integrity at rest (step 3b): each row's digest, its findings and its
    // scrubs.
    Migration {
        version: 12,
        name: "row_digest",
        sql: include_str!("../migrations/0012_row_digest.sql"),
    },
    // The record path batched (ruled 2026-11-10): each subject's last
    // sequence in a dataset, found by an index.
    Migration {
        version: 13,
        name: "subject_sequence",
        sql: include_str!("../migrations/0013_subject_sequence.sql"),
    },
    // The write-ahead stream's position (the `ha` profile; the resilience
    // spec's section 4, rulings of 2026-11-08): committed with each batch.
    Migration {
        version: 14,
        name: "write_ahead",
        sql: include_str!("../migrations/0014_write_ahead.sql"),
    },
    // A dataset's gaps (W10.14): what the stream dropped before the lake
    // took it, each gap's every state, and where a dataset's next begins.
    Migration {
        version: 15,
        name: "dataset_gap",
        sql: include_str!("../migrations/0015_dataset_gap.sql"),
    },
    // A new database timeline raises the epoch (contract v30, stage C): the
    // identity the writer last saw.
    Migration {
        version: 16,
        name: "database_identity",
        sql: include_str!("../migrations/0016_database_identity.sql"),
    },
    // The unattributed gap (contract v30, stage C2; approved 2026-11-11):
    // each run the stream skipped keeps the gap recorded for it at once.
    Migration {
        version: 17,
        name: "unattributed_gap",
        sql: include_str!("../migrations/0017_unattributed_gap.sql"),
    },
    // Writer groups (step 3d; approved 2026-11-12): each group past the
    // first's epoch, which a dataset its writer begins is checked against.
    Migration {
        version: 18,
        name: "writer_group",
        sql: include_str!("../migrations/0018_writer_group.sql"),
    },
    // Hash ranges (step 3d; section 6.1, approved 2026-11-12): each range's
    // own head row, and each row's range and anchor.
    Migration {
        version: 19,
        name: "ranges",
        sql: include_str!("../migrations/0019_ranges.sql"),
    },
    // lake_row partitioned online (step 3d; section 6.2): the rows kept so
    // far said to name no range, checked without holding reads back, then
    // the table kept so far made the new table's default partition, and
    // `lake_latest` (approved 2026-11-12).
    Migration {
        version: 20,
        name: "partitioned_prepare",
        sql: include_str!("../migrations/0020_partitioned_prepare.sql"),
    },
    Migration {
        version: 21,
        name: "partitioned_validate",
        sql: include_str!("../migrations/0021_partitioned_validate.sql"),
    },
    Migration {
        version: 22,
        name: "partitioned",
        sql: include_str!("../migrations/0022_partitioned.sql"),
    },
    // lake_row's partitions made by lake_row's owner (step 3d): the lake
    // serves under a login that may not create in the schema.
    Migration {
        version: 23,
        name: "leaves",
        sql: include_str!("../migrations/0023_leaves.sql"),
    },
    // A month's table analysed once its first batch is in (step 3d;
    // `lake_analyze_month` approved 2026-11-14).
    Migration {
        version: 24,
        name: "analyse_months",
        sql: include_str!("../migrations/0024_analyse_months.sql"),
    },
    // A range's writer group (step 3d; ruled 2026-11-14): set by a move's
    // switch, and filled with the rendezvous owner at start.
    Migration {
        version: 25,
        name: "writer_group",
        sql: include_str!("../migrations/0025_writer_group.sql"),
    },
];

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
