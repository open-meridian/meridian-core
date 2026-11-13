//! `lake_row`'s partitions (step 3d; spec/a-deployment-scales-out-and-fails-
//! over-by-configuration section 6.2): LIST by the row's lake partition (a
//! dataset's range), then RANGE by month of valid time. A row key fixes its
//! valid time (the lake's rule 5), so a row never moves across months.
//!
//! Each range's table and each of its months' are made by the writer as it
//! first needs them: made empty, given their indexes while empty, then
//! attached, so nothing is ever read to attach it and no index is built over
//! rows. Their names are a digest of the range's ID, which a table name
//! could not always carry (63 bytes; any character).

use std::collections::BTreeSet;

use postgres::GenericClient;

/// The advisory lock every partition's making takes: two writers making the
/// same range's table, or two months of it, take turns.
const LEAVES_LOCK: i64 = 0x6c61_6b65_0000_0002_u64 as i64;

/// A range's months' lock: its making's alone.
fn month_lock(partition: &str) -> i64 {
    let digest = meridian_changes::integrity::digest(partition.as_bytes());
    i64::from_be_bytes([
        0x6c, 0x61, 0x6b, 0x66, digest[0], digest[1], digest[2], digest[3],
    ])
}

/// A month of valid time: its first nanosecond and the next month's first,
/// none past either end of the 64-bit range (Postgres's MINVALUE, MAXVALUE).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Month {
    pub year: i64,
    pub month: u32,
}

const NS_PER_DAY: i128 = 86_400_000_000_000;

impl Month {
    /// The month holding `valid_from_ns` (UTC).
    pub fn of(valid_from_ns: i64) -> Month {
        let days = (i128::from(valid_from_ns)).div_euclid(NS_PER_DAY) as i64;
        let (year, month, _) = civil_from_days(days);
        Month { year, month }
    }

    fn next(self) -> Month {
        if self.month == 12 {
            Month {
                year: self.year + 1,
                month: 1,
            }
        } else {
            Month {
                year: self.year,
                month: self.month + 1,
            }
        }
    }

    fn start_ns(self) -> i128 {
        i128::from(days_from_civil(self.year, self.month, 1)) * NS_PER_DAY
    }

    /// Its bounds as a RANGE partition's: `FROM (...) TO (...)`.
    fn bounds(self) -> (String, String) {
        let edge = |ns: i128| {
            if ns <= i128::from(i64::MIN) {
                "MINVALUE".to_string()
            } else if ns > i128::from(i64::MAX) {
                "MAXVALUE".to_string()
            } else {
                ns.to_string()
            }
        };
        (edge(self.start_ns()), edge(self.next().start_ns()))
    }

    fn tag(self) -> String {
        if self.year < 0 {
            format!("n{:04}{:02}", -self.year, self.month)
        } else {
            format!("{:04}{:02}", self.year, self.month)
        }
    }
}

/// Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// A range's table's name: `lake_row_` and the first eight bytes of the
/// SHA-256 of its ID, in hex.
pub fn range_table(partition: &str) -> String {
    let digest = meridian_changes::integrity::digest(partition.as_bytes());
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("lake_row_{hex}")
}

/// A range's month's table's name.
pub fn month_table(partition: &str, month: Month) -> String {
    format!("{}_{}", range_table(partition), month.tag())
}

/// The indexes each month's table is made with, `{t}` its name: the ones
/// the table kept before 3d has, over its own rows.
const INDEXES: &[&str] = &[
    "CREATE UNIQUE INDEX {t}_seq ON {t} (dataset, sequence)",
    "CREATE UNIQUE INDEX {t}_version ON {t} (dataset, data_type, row_key, version)",
    "CREATE INDEX {t}_subjects ON {t} USING gin (subjects)",
    "CREATE INDEX {t}_latest ON {t} (dataset, data_type, subject, kind, venue_id, interval_ns, \
     latest_key, valid_from_ns DESC, sequence DESC)",
    "CREATE INDEX {t}_later ON {t} USING gin ((subjects[2:]))",
    "CREATE INDEX {t}_unkeyed ON {t} (dataset, sequence) WHERE latest_key IS NULL",
    "CREATE INDEX {t}_unconverted ON {t} (dataset) WHERE unconverted",
    "CREATE INDEX {t}_closed ON {t} (dataset, sequence) WHERE closed_reason <> 0",
    "CREATE INDEX {t}_listing ON {t} (subject, dataset, row_key, version DESC) \
     INCLUDE (expires, listed_type, latest_key, recorded_at_ns, valid_from_ns, quality, sequence) \
     WHERE data_type = 9",
    "CREATE INDEX {t}_unlisted ON {t} (dataset, sequence) WHERE data_type = 9 AND listed_type IS NULL",
    "CREATE INDEX {t}_unpublished ON {t} (dataset, sequence) \
     WHERE data_type = 6 AND published_at_ns IS NULL",
    "CREATE INDEX {t}_quality ON {t} (dataset, sequence) WHERE quality IN (2, 3)",
    "CREATE INDEX {t}_subject_sequence ON {t} (dataset, data_type, (subject || ''), sequence)",
];

/// A text as an SQL literal: doubled quotes, so any partition ID is one.
fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// Whether `table` exists in the session's schema.
fn exists(client: &mut impl GenericClient, table: &str) -> Result<bool, postgres::Error> {
    Ok(client
        .query_one("SELECT to_regclass($1) IS NOT NULL", &[&table])?
        .get(0))
}

/// Make each of `ranges`' tables not yet made, and attach it to `lake_row`,
/// in `client`'s transaction, which must hold no lock on `lake_row` or its
/// partitions yet: attaching a range takes `lake_row`'s SHARE UPDATE
/// EXCLUSIVE lock, which reads and writes pass, and, while the table kept
/// before 3d is its default partition, that table's ACCESS EXCLUSIVE lock --
/// waited for at most two seconds, and asked again by the next batch, so no
/// read is held behind it for long. Answers how many it made.
pub fn ensure_ranges(
    client: &mut impl GenericClient,
    ranges: &BTreeSet<String>,
) -> Result<usize, postgres::Error> {
    let mut missing = Vec::new();
    for partition in ranges {
        if !exists(client, &range_table(partition))? {
            missing.push(partition);
        }
    }
    if missing.is_empty() {
        return Ok(0);
    }
    client.batch_execute("SET LOCAL lock_timeout = '2s'")?;
    client.execute("SELECT pg_advisory_xact_lock($1)", &[&LEAVES_LOCK])?;
    let mut made = 0;
    for partition in missing {
        let range = range_table(partition);
        if exists(client, &range)? {
            continue;
        }
        client.batch_execute(&format!(
            "CREATE TABLE {range} (LIKE lake_row INCLUDING DEFAULTS)
                 PARTITION BY RANGE (valid_from_ns);
             ALTER TABLE lake_row ATTACH PARTITION {range} FOR VALUES IN ({});",
            literal(partition)
        ))?;
        made += 1;
    }
    Ok(made)
}

/// Whether `partition`'s table is made: a batch places rows only in one
/// that is ([`ensure_ranges`], before it begins).
pub fn range_made(
    client: &mut impl GenericClient,
    partition: &str,
) -> Result<bool, postgres::Error> {
    exists(client, &range_table(partition))
}

/// Make each of `needed`'s month tables not yet made -- empty, with its
/// indexes, then attached to its range's table, under that table's SHARE
/// UPDATE EXCLUSIVE lock alone, which reads and writes pass -- in
/// `client`'s transaction. Every range named must be made already.
/// Answers how many it made.
pub fn ensure_months(
    client: &mut impl GenericClient,
    needed: &BTreeSet<(String, Month)>,
) -> Result<usize, postgres::Error> {
    let mut missing = Vec::new();
    for need in needed {
        if !exists(client, &month_table(&need.0, need.1))? {
            missing.push(need);
        }
    }
    if missing.is_empty() {
        return Ok(0);
    }
    // Each range's own lock: never the ranges' (which a range's attaching
    // holds while it waits on the table kept before 3d, which this batch may
    // hold).
    let mut locks: Vec<i64> = missing.iter().map(|(p, _)| month_lock(p)).collect();
    locks.sort_unstable();
    locks.dedup();
    for lock in locks {
        client.execute("SELECT pg_advisory_xact_lock($1)", &[&lock])?;
    }
    let mut made = 0;
    for (partition, month) in missing {
        let leaf = month_table(partition, *month);
        if exists(client, &leaf)? {
            continue;
        }
        let range = range_table(partition);
        let mut sql = format!("CREATE TABLE {leaf} (LIKE lake_row INCLUDING DEFAULTS);\n");
        for index in INDEXES {
            sql.push_str(&index.replace("{t}", &leaf));
            sql.push_str(";\n");
        }
        let (from, to) = month.bounds();
        sql.push_str(&format!(
            "ALTER TABLE {range} ATTACH PARTITION {leaf} FOR VALUES FROM ({from}) TO ({to});"
        ));
        client.batch_execute(&sql)?;
        made += 1;
    }
    Ok(made)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_month_holds_its_valid_times_and_its_bounds_meet_the_next() {
        // 2026-11-13T00:00:00Z.
        let at = 1_794_528_000_000_000_000_i64;
        let month = Month::of(at);
        assert_eq!((month.year, month.month), (2026, 11));
        let (from, to) = month.bounds();
        assert_eq!(from, "1793491200000000000");
        assert_eq!(to, "1796083200000000000");
        assert_eq!(Month::of(1_796_083_200_000_000_000).bounds().0, to);
        assert_eq!(Month::of(1_796_083_200_000_000_000 - 1), month);
        let epoch = Month::of(0);
        assert_eq!((epoch.year, epoch.month), (1970, 1));
        let before = Month::of(-1);
        assert_eq!((before.year, before.month), (1969, 12));
        assert_eq!(before.bounds().1, "0");
        assert_eq!(Month::of(i64::MIN).bounds().0, "MINVALUE");
        assert_eq!(Month::of(i64::MAX).bounds().1, "MAXVALUE");
        assert_eq!(month.tag(), "202611");
    }

    #[test]
    fn a_ranges_tables_are_named_by_a_digest_of_its_id() {
        let name = range_table("truefx:live/0000-7fff");
        assert!(
            name.starts_with("lake_row_") && name.len() == 9 + 16,
            "{name}"
        );
        assert_ne!(name, range_table("truefx:live/8000-ffff"));
        let leaf = month_table("truefx:live", Month::of(0));
        assert!(leaf.ends_with("_197001") && leaf.len() < 63, "{leaf}");
        assert_eq!(literal("a'b"), "'a''b'");
    }
}
