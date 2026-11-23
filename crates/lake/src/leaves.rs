//! `lake_row`'s partitions (step 3d; spec/a-deployment-scales-out-and-fails-
//! over-by-configuration section 6.2): LIST by the row's lake partition (a
//! dataset's range), then RANGE by month of valid time. A row key fixes its
//! valid time (the lake's rule 5), so a row never moves across months.
//!
//! Each range's table and each of its months' are made by the writer as it
//! first needs them: made empty, given their indexes while empty, then
//! attached, so nothing is ever read to attach it and no index is built over
//! rows. Their names are a digest of the range's ID, which a table name
//! could not always carry (63 bytes; any character). The writer asks; the
//! functions migration 23 made, run as `lake_row`'s owner, make them, so
//! the lake's own login needs no right to create in the schema.
//!
//! # The lock budget (v31a.17), and every statement of `lake_row`
//!
//! Postgres locks each relation a statement touches and holds the lock to
//! its transaction's end, not its statement's; a month's table is 14
//! relations (16 with TOAST); the lock table is the cluster's, 64 a
//! connection by default. A statement that names its months as values known
//! before it runs locks those alone. One that finds them as it runs -- by
//! another table's rows, a lateral join -- or names none locks every month
//! it could read. Measured on Postgres 16 over 360 to 424 months where a
//! figure is given, and classed by reading the statement where none is.
//! "Statements" counts each naming of `lake_row` in the file's SQL: 59 in
//! `postgres.rs` and 6 in `postgres/moves.rs`, of which 6 are bounded, 1 is
//! latest, 34 are a range's gap and 24 the lake's.
//!
//! The classes: **bounded** (its months named, or few by rule), **latest**
//! (served by `lake_latest` or the catalogue; no month, or the months it
//! names), and **KNOWN GAP**, either *range* (14 x the months of the ranges
//! the statement reads) or *lake* (14 x every month the lake keeps). None
//! is walked a month a transaction yet.
//!
//! | Statement of `lake_row` (`postgres.rs`) | Statements | Class | Locks |
//! |---|---|---|---|
//! | months made (`make_months`: write, move, legacy conversion) | by function | bounded | eight a transaction, about 19 each, before the rows |
//! | rows per dataset on retention's timer (`rows_in_force`) | none | latest | the catalogue's, 2 |
//! | a batch's keys' latest versions (`latest_of_each`, every key held) | 1 | latest | 14 a month those versions are in |
//! | a read by its window (`EVERY_VERSION`, `EVERY_VERSION_BY_KEY`, `IN_FORCE`, `IN_FORCE_PUBLISHED`) | 6 | bounded | 14 a month of the window; with no window, range |
//! | a batch's rows written (`insert_all`) | 1 | KNOWN GAP, range | 16 a month written, to commit: a batch is not held to a number of months |
//! | a legacy page converted (`convert_page`) | 1 | KNOWN GAP, range | 16 a month of the page's rows, 4,096 rows at most |
//! | a subject's last sequence (`previous_of_each`) | 1 | KNOWN GAP, range | every write; M4's subject head |
//! | a batch's keys' latest versions before `lake_latest` holds every key | 1 | KNOWN GAP, range | until the conversion ends |
//! | the latest read (`LATEST`, `LATEST_BY_PUBLISHED`, `LATEST_NOW`) | 7 | KNOWN GAP, range | a row naming the subject after its first is found in the rows alone: `lake_latest` keeps no later subject |
//! | a series listing (`LISTING`) | 2 | KNOWN GAP, range | |
//! | whether a counted row is superseded, before `lake_latest` holds every key (`SUPERSEDED`) | 1 | KNOWN GAP, lake | within the counts below |
//! | rows after a watermark (`read_each`, `When::After`) | 3 | KNOWN GAP, lake | its ranges are named by a join; it cuts at a sequence |
//! | rows at their places (`rows_at`) | 1 | KNOWN GAP, lake | names no range |
//! | `counts` | 2 | KNOWN GAP, lake | no timer reads it now |
//! | `unconverted_counts`, `closed_day_counts` | 4 | KNOWN GAP, lake | the datasets' listing; the catalogue keeps no such figure |
//! | `quality_counts` | 3 | KNOWN GAP, lake | 5,986 over 424 months; `lake_latest` keeps each key's quality in force, and would serve it by an index it lacks |
//! | `lowest_kept`, `dated_range`, `dated_on`, `row_version` | 5 | KNOWN GAP, range | |
//! | a dataset read whole (`read_dataset`, `read_dataset_each`, `read_dataset_versions`) | 4 | KNOWN GAP, range | |
//! | the change log's page (`read_changes`) | 1 | KNOWN GAP, range | one range; it cuts at a sequence |
//! | retention (`remove_before`) | 5 | KNOWN GAP, range | by recorded time, not valid time |
//! | the backfills at every start (`backfill_latest_keys`, `backfill_listing`, `backfill_published`) | 6 | KNOWN GAP, lake | |
//! | the scrub (`scrub_rows`) | 4 | KNOWN GAP, lake | |
//!
//! | Statement of `lake_row` (`postgres/moves.rs`) | Statements | Class | Locks |
//! |---|---|---|---|
//! | a range's last row, and a span's digest | 2 | KNOWN GAP, range | one range |
//! | a move's copy read from its source | 3 | KNOWN GAP, range | one snapshot of the source, so nothing later is copied ahead of an earlier row |
//! | a move's copy written | 1 | KNOWN GAP, range | 16 a month of the range, in the copy's one transaction |
//!
//! A statement split in two inside one transaction holds both halves'
//! locks: only a transaction of its own, or a month named, bounds them.

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

    #[cfg(test)]
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

    #[cfg(test)]
    fn start_ns(self) -> i128 {
        i128::from(days_from_civil(self.year, self.month, 1)) * NS_PER_DAY
    }

    /// Its bounds as a RANGE partition's: `FROM (...) TO (...)`, as
    /// migration 23's lake_make_month computes them.
    #[cfg(test)]
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

    /// Its `YYYY-MM`, as migration 23's lake_make_month takes it.
    pub fn label(self) -> String {
        format!("{:04}-{:02}", self.year, self.month)
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

#[cfg(test)]
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
        // Made by lake_row's owner (migration 23): the lake's own login may
        // not create in the schema. False where another made it first.
        let fresh: bool = client
            .query_one("SELECT lake_make_range($1)", &[partition])?
            .get(0);
        made += usize::from(fresh);
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
        // Made by lake_row's owner (migration 23), with the indexes the
        // table kept before 3d has, while it is empty, then attached.
        // Its bounds computed there from the month, never passed.
        let fresh: bool = client
            .query_one(
                "SELECT lake_make_month($1, $2)",
                &[partition, &month.label()],
            )?
            .get(0);
        made += usize::from(fresh);
    }
    Ok(made)
}

/// The most month tables one transaction makes (v31a.17, the lake's lock
/// budget): making one takes a lock on each of its relations -- the table,
/// its TOAST table and its 13 indexes, about 19 with its range's -- held
/// until the transaction commits, and the lock table is the whole
/// cluster's. So a history's months are made a few a transaction, before
/// the rows that need them, and never as many as the history is long.
pub const MONTHS_A_TRANSACTION: usize = 8;

/// Those of `needed`'s month tables not yet made.
pub fn months_missing(
    client: &mut impl GenericClient,
    needed: &BTreeSet<(String, Month)>,
) -> Result<BTreeSet<(String, Month)>, postgres::Error> {
    let mut missing = BTreeSet::new();
    for need in needed {
        if !exists(client, &month_table(&need.0, need.1))? {
            missing.insert(need.clone());
        }
    }
    Ok(missing)
}

/// Make each of `needed`'s month tables not yet made, at most
/// [`MONTHS_A_TRANSACTION`] a transaction of `client`'s own, each
/// committed before the next begins: `client` is in no transaction, and
/// holds no lock from one month's making while it makes the next eight. A
/// month another made meanwhile -- a release before this one, making it in
/// its batch's transaction during a rolling upgrade -- is found made under
/// its range's lock and left. Answers how many it made.
pub fn make_months(
    client: &mut impl GenericClient,
    needed: &BTreeSet<(String, Month)>,
) -> Result<usize, postgres::Error> {
    let missing: Vec<(String, Month)> = months_missing(client, needed)?.into_iter().collect();
    let mut made = 0;
    for some in missing.chunks(MONTHS_A_TRANSACTION) {
        let mut tx = client.transaction()?;
        made += ensure_months(&mut tx, &some.iter().cloned().collect())?;
        tx.commit()?;
    }
    Ok(made)
}

/// `partition`'s `month` table analysed by lake_row's owner (migration 24),
/// where it holds rows and was never analysed: whether it was.
pub fn analyse_month(
    client: &mut impl GenericClient,
    partition: &str,
    month: Month,
) -> Result<bool, postgres::Error> {
    Ok(client
        .query_one(
            "SELECT lake_analyze_month($1, $2)",
            &[&partition, &month.label()],
        )?
        .get(0))
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
    }
}
