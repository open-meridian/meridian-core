//! The lake in Postgres, behind the same trait the in-memory store answers.
//!
//! Synchronous, as every store here is: the bus runs request handlers on a
//! blocking pool already.
//!
//! # One writer per partition
//!
//! A batch takes its dataset's partition head with `FOR UPDATE`, decides
//! each row against what the partition holds, and moves the head in the
//! same transaction: a rollback takes its numbers back with it, so there are
//! no holes, and two lakes running at once during a rolling update still
//! number one partition one batch at a time.

use std::collections::BTreeMap;

use postgres::{GenericClient, IsolationLevel, NoTls, Transaction};
use prost::Message;
use r2d2_postgres::PostgresConnectionManager;

use meridian_domain::date::Date;
use meridian_domain::v1::{
    EntitlementsChangedEvent, InstrumentType, ObservationsWantedEvent, SourcePriority,
};

use crate::memory::{as_of, number, served_of};
use crate::migrations;
use crate::row::{DataType, Observation};
use crate::store::{
    as_assessed, as_flagged, closed_reason, listed_of, Assessment, ClosedDayAmendment, Listed,
    Query, Recorded, Removal, Result, Served, Stale, Store, StoreError, WantChange, WantChangeKind,
    When,
};

type Pool = r2d2::Pool<PostgresConnectionManager<NoTls>>;
type Connection = r2d2::PooledConnection<PostgresConnectionManager<NoTls>>;

/// Names the schema lock: distinct from every other store's.
const SCHEMA_LOCK: i64 = 0x6c61_6b65_0000_0001_u64 as i64;

pub struct PostgresStore {
    pool: Pool,
}

impl PostgresStore {
    pub fn connect(url: &str, pool_size: u32) -> Result<Self> {
        let config: postgres::Config = url.parse().map_err(unavailable)?;
        let manager = PostgresConnectionManager::new(config, NoTls);
        let pool = r2d2::Pool::builder()
            .max_size(pool_size.max(1))
            .build(manager)
            .map_err(unavailable)?;
        Ok(Self { pool })
    }

    /// Apply every migration not yet recorded, under a lock. Run once per
    /// release by `meridian-lake migrate`, never by a starting process.
    pub fn migrate(&self, clock: &dyn meridian_clock::Clock) -> Result<()> {
        let mut conn = self.conn()?;
        conn.execute("SELECT pg_advisory_lock($1)", &[&SCHEMA_LOCK])
            .map_err(unavailable)?;
        let outcome = apply_migrations(&mut conn, clock);
        let _ = conn.execute("SELECT pg_advisory_unlock($1)", &[&SCHEMA_LOCK]);
        outcome
    }

    /// What a start does instead: one read, no lock.
    pub fn verify(&self) -> Result<()> {
        let mut conn = self.conn()?;
        let exists = conn
            .query_opt(
                "SELECT 1 FROM information_schema.tables
                  WHERE table_schema = current_schema() AND table_name = 'lake_schema_migration'",
                &[],
            )
            .map_err(unavailable)?
            .is_some();
        let applied = if exists {
            conn.query_one("SELECT max(version) FROM lake_schema_migration", &[])
                .map_err(unavailable)?
                .get::<_, Option<i64>>(0)
        } else {
            None
        };
        migrations::verify(applied)
    }

    fn conn(&self) -> Result<Connection> {
        self.pool.get().map_err(unavailable)
    }

    fn head(tx: &mut Transaction<'_>, dataset: &str) -> Result<u64> {
        tx.execute(
            "INSERT INTO lake_partition (dataset, head) VALUES ($1, 0) ON CONFLICT DO NOTHING",
            &[&dataset],
        )
        .map_err(unavailable)?;
        let head: i64 = tx
            .query_one(
                "SELECT head FROM lake_partition WHERE dataset = $1 FOR UPDATE",
                &[&dataset],
            )
            .map_err(unavailable)?
            .get(0);
        Ok(head as u64)
    }

    /// Moves the partition's head, and its rows in force by the row keys
    /// the batch kept that it held no version of (contract v29, ruling 619
    /// (2); migration 8): a new version of a key held replaces its last, and
    /// a row served unkept is never in force.
    fn move_head(tx: &mut Transaction<'_>, dataset: &str, head: u64, new_keys: u64) -> Result<()> {
        tx.execute(
            "UPDATE lake_partition SET in_force = in_force + $3, head = $2 WHERE dataset = $1",
            &[&dataset, &(head as i64), &(new_keys as i64)],
        )
        .map_err(unavailable)?;
        Ok(())
    }
}

fn latest_of(
    client: &mut impl GenericClient,
    dataset: &str,
    row: &Observation,
) -> Result<Option<Observation>> {
    let found = client
        .query_opt(
            "SELECT body FROM lake_row
              WHERE dataset = $1 AND data_type = $2 AND row_key = $3
              ORDER BY version DESC LIMIT 1",
            &[&dataset, &row.data_type().code(), &row.meta().row_key],
        )
        .map_err(unavailable)?;
    found
        .map(|r| {
            let body: Vec<u8> = r.get(0);
            Observation::decode(row.data_type(), &body).map_err(unreadable)
        })
        .transpose()
}

/// Every closed-day re-check of the datasets named, in order (contract v23).
fn amendments_of(
    client: &mut impl GenericClient,
    datasets: &[String],
) -> Result<Vec<ClosedDayAmendment>> {
    let rows = client
        .query(
            "SELECT dataset, sequence, closed_day, why, at_ns FROM lake_closed_day
              WHERE dataset = ANY($1) ORDER BY amendment_id",
            &[&datasets],
        )
        .map_err(unavailable)?;
    rows.iter().map(amendment_of).collect()
}

/// The assessments of these rows (contract v28, W10.12), in the order
/// journalled.
fn assessments_of(
    client: &mut impl GenericClient,
    rows: &[Observation],
) -> Result<Vec<Assessment>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let (datasets, sequences): (Vec<String>, Vec<i64>) = rows
        .iter()
        .map(|row| (row.dataset().to_string(), row.meta().sequence as i64))
        .unzip();
    let found = client
        .query(
            "SELECT a.dataset, a.sequence, a.row_key, a.version, a.state, a.reasons,
                    a.by_instance, a.at_ns
               FROM lake_quality_assessment a
               JOIN (SELECT DISTINCT * FROM unnest($1::text[], $2::bigint[])) AS asked(dataset, sequence)
                 ON a.dataset = asked.dataset AND a.sequence = asked.sequence
              ORDER BY a.assessment_id",
            &[&datasets, &sequences],
        )
        .map_err(unavailable)?;
    Ok(found.iter().map(assessment_of).collect())
}

fn assessment_of(row: &postgres::Row) -> Assessment {
    Assessment {
        dataset: row.get(0),
        sequence: row.get::<_, i64>(1) as u64,
        row_key: row.get(2),
        version: row.get::<_, i64>(3) as u64,
        state: i32::from(row.get::<_, i16>(4)),
        reasons: row
            .get::<_, Vec<i16>>(5)
            .into_iter()
            .map(i32::from)
            .collect(),
        by_instance: row.get(6),
        at_ns: row.get(7),
    }
}

fn amendment_of(row: &postgres::Row) -> Result<ClosedDayAmendment> {
    let body: Option<Vec<u8>> = row.get(2);
    Ok(ClosedDayAmendment {
        dataset: row.get(0),
        sequence: row.get::<_, i64>(1) as u64,
        closed_day: body
            .map(|body| meridian_domain::v1::DateCheck::decode(&body[..]).map_err(unreadable))
            .transpose()?,
        why: row.get(3),
        at_ns: row.get(4),
    })
}

fn previous_of(client: &mut impl GenericClient, dataset: &str, row: &Observation) -> Result<u64> {
    let found: Option<i64> = client
        .query_one(
            "SELECT max(sequence) FROM lake_row
              WHERE subject = $1 AND data_type = $2 AND dataset = $3",
            &[&row.subject(), &row.data_type().code(), &dataset],
        )
        .map_err(unavailable)?
        .get(0);
    Ok(found.unwrap_or(0) as u64)
}

fn insert(tx: &mut Transaction<'_>, dataset: &str, row: &Observation) -> Result<()> {
    let meta = row.meta();
    let date: Option<String> = (!meta.business_date.is_empty()).then(|| meta.business_date.clone());
    // A series row's expiry and listed type (migration 7); none for any other.
    let listing = row.listing();
    let expires: Option<String> = listing.as_ref().and_then(|(expires, _)| expires.clone());
    let listed_type: Option<i16> = listing.map(|(_, kind)| kind as i16);
    tx.execute(
        "INSERT INTO lake_row
                (dataset, sequence, data_type, row_key, version, subject, subjects, kind,
                 interval_ns, venue_id, valid_from_ns, valid_until_ns, business_date,
                 recorded_at_ns, previous_sequence, unconverted, body, closed_reason,
                 latest_key, quality, expires, listed_type)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13::text::date, $14, $15,
                 $16, $17, $18, $19, $20, $21::text::date, $22)",
        &[
            &dataset,
            &(meta.sequence as i64),
            &row.data_type().code(),
            &meta.row_key,
            &(meta.version as i64),
            &row.subject(),
            &row.subjects(),
            &(row.kind() as i16),
            &row.interval_ns(),
            &row.venue(),
            &meta.valid_from_ns,
            &meta.valid_until_ns,
            &date,
            &meta.recorded_at_ns,
            &(meta.previous_sequence as i64),
            &!meta.unconverted.is_empty(),
            &row.encode(),
            &closed_reason(meta.closed_day.as_ref()),
            &row.latest_key(),
            &(meta.quality as i16),
            &expires,
            &listed_type,
        ],
    )
    .map_err(unavailable)?;
    Ok(())
}

/// A read of the latest, chosen in SQL (found on dev 2026-10-26): of the
/// rows the read would load, each row key at its version in force, then per
/// subject, dataset, kind, venue, length and `latest_key` the latest by
/// valid time and sequence, and the latest of those not invalid -- its own
/// state or an assessment by the cut-off (ruling 619) -- so the read's own
/// choice, made again over these, is the one it makes over every row. A
/// row without a `latest_key` is a key of its own.
///
/// Served by an index, never a scan of a busy subject's history (ruling
/// 770; C3 part 2's bench: 5.1 s at 400,000 rows of one underlying). The
/// keys a subject holds are found by skipping through `lake_row_latest`,
/// one probe a key, and each key's latest -- and latest not invalid -- is
/// the first row in force down its own entries. A row naming an asked
/// subject after its first is found by `lake_row_by_later_subjects` and
/// chosen as before; a row not yet given its key (`lake_row_unkeyed`) is
/// one of its own. "In force" is the same test the whole read applies: the
/// latest version of its row key among the rows the read would load. Each
/// part is written so a table without statistics yet (a fresh deployment,
/// the gate's) is read by the same indexes.
static LATEST: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    // The read's own conditions on a row, as the whole read applies them.
    let window = |a: &str| {
        format!(
            "{a}.recorded_at_ns <= $6
             AND ($7::text IS NULL OR {a}.business_date = $7::text::date)
             AND {a}.valid_from_ns >= $8 AND {a}.valid_from_ns < $9 AND {a}.valid_from_ns <= $10"
        )
    };
    let narrowed = |a: &str| {
        format!(
            "{a}.data_type = $3
             AND (cardinality($4::smallint[]) = 0 OR {a}.kind = ANY($4))
             AND ($5::bigint = 0 OR {a}.interval_ns = $5)
             AND {}",
            window(a)
        )
    };
    // A scalar subquery, never a join: each row's own key bound, so the
    // planner reads its row key's versions by `lake_row_version` whatever
    // the statistics say (a fresh table's had it scan the dataset a row).
    let in_force = format!(
        "r.version = (SELECT max(v.version) FROM lake_row v
                       WHERE v.dataset = r.dataset AND v.data_type = r.data_type
                         AND v.row_key = r.row_key
                         AND v.subjects && $2::text[] AND {})",
        narrowed("v")
    );
    let invalid = "(COALESCE(r.quality, 0) = 3
          OR EXISTS (SELECT 1 FROM lake_quality_assessment a
                      WHERE a.dataset = r.dataset AND a.sequence = r.sequence
                        AND a.state = 3 AND a.at_ns <= $6))";
    let down_the_key = |also: &str| {
        format!(
            "(SELECT r.dataset, r.sequence FROM lake_row r
               WHERE r.dataset = k.dataset AND r.data_type = $3 AND r.subject = k.subject
                 AND r.kind = k.kind AND r.venue_id = k.venue_id
                 AND r.interval_ns = k.interval_ns AND r.latest_key = k.latest_key
                 AND {} AND {in_force} {also}
               ORDER BY r.valid_from_ns DESC, r.sequence DESC
               LIMIT 1)",
            window("r")
        )
    };
    // The next key of a dataset's subject past `after`, by the index's order.
    let next_key = |dataset: &str, subject: &str, after: &str| {
        format!(
            "SELECT r.dataset, r.subject, r.kind, r.venue_id, r.interval_ns, r.latest_key
               FROM lake_row r
              WHERE r.dataset = {dataset} AND r.data_type = $3 AND r.subject = {subject}
                AND {after}
              ORDER BY r.kind, r.venue_id, r.interval_ns, r.latest_key
              LIMIT 1"
        )
    };
    format!(
        "
WITH RECURSIVE keys (dataset, subject, kind, venue_id, interval_ns, latest_key) AS (
    SELECT first.*
      FROM (SELECT DISTINCT unnest($1::text[])) AS d (dataset)
     CROSS JOIN (SELECT DISTINCT unnest($2::text[])) AS s (subject)
     CROSS JOIN LATERAL ({first}) AS first
  UNION ALL
    SELECT next.*
      FROM keys k
     CROSS JOIN LATERAL ({next}) AS next
),
-- Found by the later subjects' index alone: the datasets and the data type
-- asked are kept apart, so no index they lead is scanned beside it.
named_later AS MATERIALIZED (
    SELECT r.dataset, r.data_type, r.sequence, r.subject, r.kind, r.venue_id,
           r.interval_ns, r.latest_key, r.valid_from_ns, {invalid} AS invalid
      FROM lake_row r
     WHERE (r.subjects[2:]) && $2::text[]
       AND NOT (r.subject = ANY($2)) AND r.latest_key IS NOT NULL
       AND (cardinality($4::smallint[]) = 0 OR r.kind = ANY($4))
       AND ($5::bigint = 0 OR r.interval_ns = $5)
       AND {window_r} AND {in_force}
),
later AS (
    SELECT * FROM named_later WHERE dataset = ANY($1) AND data_type = $3
),
chosen AS (
    SELECT c.dataset, c.sequence
      FROM keys k
     CROSS JOIN LATERAL ({latest} UNION ALL {latest_valid}) AS c
     WHERE (cardinality($4::smallint[]) = 0 OR k.kind = ANY($4))
       AND ($5::bigint = 0 OR k.interval_ns = $5)
  UNION ALL
    SELECT r.dataset, r.sequence
      FROM lake_row r
     WHERE r.dataset = ANY($1) AND r.latest_key IS NULL AND r.subjects && $2::text[]
       AND {narrowed_r} AND {in_force}
  UNION ALL
    (SELECT DISTINCT ON (dataset, subject, kind, venue_id, interval_ns, latest_key)
            dataset, sequence
       FROM later
      ORDER BY dataset, subject, kind, venue_id, interval_ns, latest_key,
               valid_from_ns DESC, sequence DESC)
  UNION ALL
    (SELECT DISTINCT ON (dataset, subject, kind, venue_id, interval_ns, latest_key)
            dataset, sequence
       FROM later
      WHERE NOT invalid
      ORDER BY dataset, subject, kind, venue_id, interval_ns, latest_key,
               valid_from_ns DESC, sequence DESC)
)
SELECT r.body FROM lake_row r
  JOIN (SELECT DISTINCT dataset, sequence FROM chosen) c
    ON r.dataset = c.dataset AND r.sequence = c.sequence
 ORDER BY r.dataset, r.sequence",
        first = next_key("d.dataset", "s.subject", "r.latest_key IS NOT NULL"),
        next = next_key(
            "k.dataset",
            "k.subject",
            "(r.kind, r.venue_id, r.interval_ns, r.latest_key)
                 > (k.kind, k.venue_id, k.interval_ns, k.latest_key)"
        ),
        latest = down_the_key(""),
        latest_valid = down_the_key(&format!("AND NOT {invalid}")),
        narrowed_r = narrowed("r"),
        window_r = window("r"),
    )
});

/// How many rows a backfill of `latest_key` and `quality` reads at a time.
const BACKFILL_BATCH: i64 = 2000;

/// Fill in `latest_key` and `quality` where a row has none (migration 4):
/// read from its own body, batch by batch, each its own transaction, so a
/// large lake is never locked whole. A row whose body does not read is
/// passed over, a key of its own as before. Run by `migrate`, every time,
/// so a row a release before this recorded during a rolling upgrade is
/// filled in by the next.
fn backfill_latest_keys(conn: &mut Connection) -> Result<u64> {
    let mut after: (String, i64) = (String::new(), 0);
    let mut filled = 0u64;
    loop {
        let mut tx = conn.transaction().map_err(unavailable)?;
        let rows = tx
            .query(
                "SELECT dataset, sequence, data_type, body FROM lake_row
                  WHERE (dataset, sequence) > ($1, $2) AND latest_key IS NULL
                  ORDER BY dataset, sequence
                  LIMIT $3",
                &[&after.0, &after.1, &BACKFILL_BATCH],
            )
            .map_err(unavailable)?;
        let Some(last) = rows.last() else {
            tx.commit().map_err(unavailable)?;
            break;
        };
        after = (last.get(0), last.get(1));
        let (mut datasets, mut sequences, mut keys, mut states) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for row in &rows {
            let code: i16 = row.get(2);
            let body: Vec<u8> = row.get(3);
            let Some(observation) = crate::row::DataType::from_code(code)
                .and_then(|data_type| Observation::decode(data_type, &body).ok())
            else {
                continue;
            };
            datasets.push(row.get::<_, String>(0));
            sequences.push(row.get::<_, i64>(1));
            keys.push(observation.latest_key());
            states.push(observation.meta().quality as i16);
        }
        filled += tx
            .execute(
                "UPDATE lake_row r SET latest_key = f.latest_key, quality = f.quality
                   FROM unnest($1::text[], $2::bigint[], $3::text[], $4::smallint[])
                        AS f(dataset, sequence, latest_key, quality)
                  WHERE r.dataset = f.dataset AND r.sequence = f.sequence",
                &[&datasets, &sequences, &keys, &states],
            )
            .map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        // Progress, batch by batch (as the listing's backfill logs it).
        tracing::info!(
            filled,
            "rows given the key their latest is chosen by so far"
        );
    }
    Ok(filled)
}

/// Fill in `expires` and `listed_type` where a series row has neither
/// (migration 7): read from its own body, batch by batch as
/// [`backfill_latest_keys`] does, by `lake_row_unlisted`. A row whose body
/// does not read is passed over, and read whole by an expirations read as
/// before. Run by `migrate`, every time.
fn backfill_listing(conn: &mut Connection) -> Result<u64> {
    let mut after: (String, i64) = (String::new(), 0);
    let mut filled = 0u64;
    loop {
        let mut tx = conn.transaction().map_err(unavailable)?;
        let rows = tx
            .query(
                "SELECT dataset, sequence, body FROM lake_row
                  WHERE (dataset, sequence) > ($1, $2)
                    AND data_type = 9 AND listed_type IS NULL
                  ORDER BY dataset, sequence
                  LIMIT $3",
                &[&after.0, &after.1, &BACKFILL_BATCH],
            )
            .map_err(unavailable)?;
        let Some(last) = rows.last() else {
            tx.commit().map_err(unavailable)?;
            break;
        };
        after = (last.get(0), last.get(1));
        let (mut datasets, mut sequences, mut expiries, mut types) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for row in &rows {
            let body: Vec<u8> = row.get(2);
            let Some((expires, kind)) = Observation::decode(DataType::Series, &body)
                .ok()
                .and_then(|observation| observation.listing())
            else {
                continue;
            };
            datasets.push(row.get::<_, String>(0));
            sequences.push(row.get::<_, i64>(1));
            expiries.push(expires);
            types.push(kind as i16);
        }
        filled += tx
            .execute(
                "UPDATE lake_row r SET expires = f.expires::date, listed_type = f.listed_type
                   FROM unnest($1::text[], $2::bigint[], $3::text[], $4::smallint[])
                        AS f(dataset, sequence, expires, listed_type)
                  WHERE r.dataset = f.dataset AND r.sequence = f.sequence",
                &[&datasets, &sequences, &expiries, &types],
            )
            .map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        // Progress, batch by batch, so a large lake's first migrate after
        // migration 7 says how far it has come.
        tracing::info!(
            filled,
            "series rows given their expiry and listed type so far"
        );
    }
    Ok(filled)
}

/// The lake's listing, from `lake_row_listing` alone (contract v28, amended
/// 2026-10-28; ruling 856): each series row key's version in force by the
/// cut-off -- its latest version recorded by then, as every read takes it --
/// left out where its effective state is invalid, its own or an assessment's
/// by then (ruling 619). A body is read only for a row not yet given its
/// listing (one a release before migration 7 recorded during a rolling
/// upgrade), which is decoded as before.
///
/// Read per underlying and dataset, each in the index's own order, so a row
/// key's versions come newest first with no sort (found 2026-10-30: dev's
/// chains are re-listed each pass, several versions a series; one query
/// over every underlying and dataset asked sorted every version). Measured
/// on core's bench after VACUUM ANALYZE, the median of 15: 1,440 series at
/// one version 10.9 ms before, 0.8 ms now; at ten versions 7.4 ms before,
/// 3.5 ms now. A skip scan taking only each key's latest was slower at both
/// (8.7 and 11.9 ms), one probe a key costing more than the versions it
/// skips at the counts dev holds.
const LISTING: &str = "
SELECT r.dataset, r.latest_key, r.expires::text, r.listed_type,
       CASE WHEN r.listed_type IS NULL OR r.latest_key IS NULL
            THEN (SELECT b.body FROM lake_row b
                   WHERE b.dataset = r.dataset AND b.sequence = r.sequence) END
  FROM (SELECT DISTINCT unnest($2::text[])) AS s (subject)
 CROSS JOIN (SELECT DISTINCT unnest($1::text[])) AS d (dataset)
 CROSS JOIN LATERAL (
    SELECT DISTINCT ON (l.row_key)
           l.dataset, l.sequence, l.latest_key, l.expires, l.listed_type, l.quality
      FROM lake_row l
     WHERE l.data_type = 9 AND l.subject = s.subject AND l.dataset = d.dataset
       AND l.recorded_at_ns <= $3
     ORDER BY l.row_key, l.version DESC) AS r
 WHERE COALESCE(r.quality, 0) <> 3
   AND NOT EXISTS (SELECT 1 FROM lake_quality_assessment a
                    WHERE a.dataset = r.dataset AND a.sequence = r.sequence
                      AND a.state = 3 AND a.at_ns <= $3)";

impl Store for PostgresStore {
    fn record(&self, dataset: &str, rows: Vec<Observation>, now_ns: i64) -> Result<Recorded> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        let mut head = Self::head(&mut tx, dataset)?;
        // Read what each row is decided against first, then number: the
        // closures cannot borrow the transaction the inserts need.
        let mut latest = BTreeMap::new();
        let mut previous = BTreeMap::new();
        for row in &rows {
            let key = (row.data_type().code(), row.meta().row_key.clone());
            if let std::collections::btree_map::Entry::Vacant(held) = latest.entry(key) {
                held.insert(latest_of(&mut tx, dataset, row)?);
            }
            let subject = (row.data_type().code(), row.subject().to_string());
            if let std::collections::btree_map::Entry::Vacant(held) = previous.entry(subject) {
                held.insert(previous_of(&mut tx, dataset, row)?);
            }
        }
        let done = number(
            rows,
            &mut head,
            now_ns,
            |row| {
                latest
                    .get(&(row.data_type().code(), row.meta().row_key.clone()))
                    .cloned()
                    .flatten()
            },
            |row| {
                previous
                    .get(&(row.data_type().code(), row.subject().to_string()))
                    .copied()
                    .unwrap_or(0)
            },
        );
        for row in &done.rows {
            insert(&mut tx, dataset, row)?;
        }
        // Each row numbered version 1 is a key the partition held no
        // version of; every other replaces its key's last.
        Self::move_head(&mut tx, dataset, head, u64::from(done.recorded))?;
        tx.commit().map_err(unavailable)?;
        Ok(done)
    }

    fn serve_unkept(
        &self,
        dataset: &str,
        rows: Vec<Observation>,
        readers: &[String],
        now_ns: i64,
    ) -> Result<Recorded> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        let mut head = Self::head(&mut tx, dataset)?;
        let first = head + 1;
        let done = number(rows, &mut head, now_ns, |_| None, |_| 0);
        if !done.rows.is_empty() {
            let served = served_of(dataset, &done.rows, first, head, readers, now_ns);
            tx.execute(
                "INSERT INTO lake_served
                        (dataset, first_sequence, last_sequence, subjects, fields, readers, at_ns)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
                &[
                    &served.dataset,
                    &(served.first_sequence as i64),
                    &(served.last_sequence as i64),
                    &served.subjects,
                    &served.fields,
                    &served.readers,
                    &served.at_ns,
                ],
            )
            .map_err(unavailable)?;
        }
        Self::move_head(&mut tx, dataset, head, 0)?;
        tx.commit().map_err(unavailable)?;
        Ok(done)
    }

    fn read(&self, query: &Query) -> Result<Vec<Observation>> {
        let mut conn = self.conn()?;
        let mut tx = conn
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .map_err(unavailable)?;
        if let When::After(watermark) = &query.when {
            // The rows each partition named recorded after its sequence
            // there, by the partition's own key (contract v19, the plan's Q33).
            let (datasets, after): (Vec<String>, Vec<i64>) = watermark
                .iter()
                .filter(|(dataset, _)| query.datasets.contains(dataset))
                .map(|(dataset, sequence)| (dataset.clone(), *sequence as i64))
                .unzip();
            let cut_off = if query.as_of_ns == 0 {
                i64::MAX
            } else {
                query.as_of_ns
            };
            let rows = tx
                .query(
                    "SELECT body FROM lake_row
                       JOIN unnest($1::text[], $2::bigint[]) AS asked(dataset, after)
                         ON lake_row.dataset = asked.dataset AND lake_row.sequence > asked.after
                      WHERE subjects && $3 AND data_type = $4 AND recorded_at_ns <= $5
                      ORDER BY lake_row.dataset, sequence",
                    &[
                        &datasets,
                        &after,
                        &query.subjects,
                        &query.data_type.code(),
                        &cut_off,
                    ],
                )
                .map_err(unavailable)?;
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let body: Vec<u8> = row.get(0);
                out.push(Observation::decode(query.data_type, &body).map_err(unreadable)?);
            }
            let amendments = amendments_of(&mut tx, &query.datasets)?;
            let mut out = if crate::store::every_version(query) {
                crate::store::recorded_by(out, query.as_of_ns)
            } else {
                as_of(out, query.as_of_ns)
            };
            let assessments = assessments_of(&mut tx, &out)?;
            tx.commit().map_err(unavailable)?;
            as_flagged(&mut out, &amendments, query.as_of_ns);
            as_assessed(&mut out, &assessments, query.as_of_ns);
            return Ok(out);
        }
        let kinds: Vec<i16> = query.kinds.iter().map(|k| *k as i16).collect();
        let cut_off = if query.as_of_ns == 0 {
            i64::MAX
        } else {
            query.as_of_ns
        };
        let (date, from, until, at): (Option<String>, i64, i64, i64) = match &query.when {
            When::Latest { at_ns } => (
                None,
                i64::MIN,
                i64::MAX,
                if *at_ns == 0 { i64::MAX } else { *at_ns },
            ),
            When::BusinessDate(date) => (Some(date.to_string()), i64::MIN, i64::MAX, i64::MAX),
            When::Range { from_ns, until_ns } => (
                None,
                *from_ns,
                if *until_ns == 0 { i64::MAX } else { *until_ns },
                i64::MAX,
            ),
            When::After(_) => unreachable!("answered above"),
        };
        // The latest chosen in SQL where the read lets it (found on dev
        // 2026-10-26: every row a chain's dataset kept was loaded to choose
        // a few hundred); never where every version is answered.
        let latest = query.latest_only
            && matches!(query.when, When::Latest { .. })
            && !crate::store::every_version(query);
        if latest {
            // Its plan is a few hundred index probes, estimated as if each
            // ran a key's whole history: compiled, it took a second more
            // than it ran (ruling 770's bench), so never compiled.
            tx.batch_execute("SET LOCAL jit = off")
                .map_err(unavailable)?;
        }
        let rows = tx
            .query(
                if latest {
                    LATEST.as_str()
                } else {
                    "SELECT body FROM lake_row
                  WHERE dataset = ANY($1) AND subjects && $2 AND data_type = $3
                    AND (cardinality($4::smallint[]) = 0 OR kind = ANY($4))
                    AND ($5::bigint = 0 OR interval_ns = $5)
                    AND recorded_at_ns <= $6
                    AND ($7::text IS NULL OR business_date = $7::text::date)
                    AND valid_from_ns >= $8 AND valid_from_ns < $9 AND valid_from_ns <= $10
                  ORDER BY dataset, sequence"
                },
                &[
                    &query.datasets,
                    &query.subjects,
                    &query.data_type.code(),
                    &kinds,
                    &query.interval_ns,
                    &cut_off,
                    &date,
                    &from,
                    &until,
                    &at,
                ],
            )
            .map_err(unavailable)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let body: Vec<u8> = row.get(0);
            out.push(Observation::decode(query.data_type, &body).map_err(unreadable)?);
        }
        let amendments = amendments_of(&mut tx, &query.datasets)?;
        let mut out = if crate::store::every_version(query) {
            crate::store::recorded_by(out, query.as_of_ns)
        } else {
            as_of(out, query.as_of_ns)
        };
        let assessments = assessments_of(&mut tx, &out)?;
        tx.commit().map_err(unavailable)?;
        as_flagged(&mut out, &amendments, query.as_of_ns);
        as_assessed(&mut out, &assessments, query.as_of_ns);
        Ok(out)
    }

    fn listing(
        &self,
        datasets: &[String],
        subjects: &[String],
        as_of_ns: i64,
    ) -> Result<Vec<Listed>> {
        let mut conn = self.conn()?;
        let mut tx = conn
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .map_err(unavailable)?;
        let cut_off = if as_of_ns == 0 { i64::MAX } else { as_of_ns };
        let rows = tx
            .query(LISTING, &[&datasets, &subjects, &cut_off])
            .map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let body: Option<Vec<u8>> = row.get(4);
            if let Some(body) = body {
                let observation =
                    Observation::decode(DataType::Series, &body).map_err(unreadable)?;
                out.extend(listed_of(&observation));
                continue;
            }
            let expires: Option<String> = row.get(2);
            let kind: Option<i16> = row.get(3);
            out.push(Listed {
                dataset: row.get(0),
                series: row.get::<_, Option<String>>(1).unwrap_or_default(),
                expires: expires.and_then(|date| Date::parse(&date).ok()),
                listed_type: InstrumentType::try_from(i32::from(kind.unwrap_or(0)))
                    .unwrap_or(InstrumentType::ListedOption),
            });
        }
        Ok(out)
    }

    fn heads(&self) -> Result<BTreeMap<String, u64>> {
        Ok(self
            .conn()?
            .query(
                "SELECT dataset, head FROM lake_partition ORDER BY dataset",
                &[],
            )
            .map_err(unavailable)?
            .iter()
            .map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1) as u64))
            .collect())
    }

    fn counts(&self) -> Result<BTreeMap<String, (u64, u64)>> {
        Ok(self
            .conn()?
            .query(
                "SELECT dataset, count(*), count(*) FILTER (WHERE unconverted)
                   FROM lake_row GROUP BY dataset",
                &[],
            )
            .map_err(unavailable)?
            .iter()
            .map(|r| {
                (
                    r.get::<_, String>(0),
                    (r.get::<_, i64>(1) as u64, r.get::<_, i64>(2) as u64),
                )
            })
            .collect())
    }

    fn unconverted_counts(&self) -> Result<BTreeMap<String, u64>> {
        // Read from lake_row_unconverted, which holds only these rows
        // (migration 6).
        Ok(self
            .conn()?
            .query(
                "SELECT dataset, count(*) FROM lake_row WHERE unconverted GROUP BY dataset",
                &[],
            )
            .map_err(unavailable)?
            .iter()
            .map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1) as u64))
            .collect())
    }

    fn lowest_kept(&self, dataset: &str) -> Result<Option<u64>> {
        let found: Option<i64> = self
            .conn()?
            .query_one(
                "SELECT min(sequence) FROM lake_row WHERE dataset = $1",
                &[&dataset],
            )
            .map_err(unavailable)?
            .get(0);
        Ok(found.map(|sequence| sequence as u64))
    }

    fn set_priority(
        &self,
        priority: &SourcePriority,
        against_updated_at_ns: i64,
    ) -> Result<std::result::Result<SourcePriority, Stale>> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        // One change to a priority at a time, so two against one read cannot
        // both pass the guard.
        tx.execute("SELECT pg_advisory_xact_lock($1)", &[&(SCHEMA_LOCK + 1)])
            .map_err(unavailable)?;
        let standing = tx
            .query_opt(
                "SELECT priority FROM lake_priority_change
                  WHERE data_type = $1 AND kind = $2 ORDER BY change_id DESC LIMIT 1",
                &[&priority.data_type, &priority.kind],
            )
            .map_err(unavailable)?
            .map(|r| {
                let body: Vec<u8> = r.get(0);
                SourcePriority::decode(&body[..]).map_err(unreadable)
            })
            .transpose()?
            .map_or(0, |p| p.updated_at_ns);
        if standing != against_updated_at_ns {
            return Ok(Err(Stale {
                standing_at_ns: standing,
            }));
        }
        tx.execute(
            "INSERT INTO lake_priority_change (data_type, kind, priority, changed_at_ns)
             VALUES ($1, $2, $3, $4)",
            &[
                &priority.data_type,
                &priority.kind,
                &priority.encode_to_vec(),
                &priority.updated_at_ns,
            ],
        )
        .map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        Ok(Ok(priority.clone()))
    }

    fn priorities(&self) -> Result<Vec<SourcePriority>> {
        self.conn()?
            .query(
                "SELECT DISTINCT ON (data_type, kind) priority FROM lake_priority_change
                  ORDER BY data_type, kind, change_id DESC",
                &[],
            )
            .map_err(unavailable)?
            .iter()
            .map(|r| {
                let body: Vec<u8> = r.get(0);
                SourcePriority::decode(&body[..]).map_err(unreadable)
            })
            .collect()
    }

    fn priority_changes(&self) -> Result<Vec<SourcePriority>> {
        self.conn()?
            .query(
                "SELECT priority FROM lake_priority_change ORDER BY change_id",
                &[],
            )
            .map_err(unavailable)?
            .iter()
            .map(|r| {
                let body: Vec<u8> = r.get(0);
                SourcePriority::decode(&body[..]).map_err(unreadable)
            })
            .collect()
    }

    fn record_want(&self, change: &WantChange) -> Result<()> {
        let want = change.want.as_ref().map(|w| w.encode_to_vec());
        self.conn()?
            .execute(
                "INSERT INTO lake_want_change
                        (want_id, dataset, change, subjects, reason, by_whom, want, at_ns)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                &[
                    &change.want_id,
                    &change.dataset,
                    &change.kind.code(),
                    &change.subjects,
                    &change.reason,
                    &change.by,
                    &want,
                    &change.at_ns,
                ],
            )
            .map_err(unavailable)?;
        Ok(())
    }

    fn want_changes(&self) -> Result<Vec<WantChange>> {
        self.conn()?
            .query(
                "SELECT want_id, dataset, change, subjects, reason, by_whom, want, at_ns
                   FROM lake_want_change ORDER BY change_id",
                &[],
            )
            .map_err(unavailable)?
            .iter()
            .map(|r| {
                let want: Option<Vec<u8>> = r.get(6);
                Ok(WantChange {
                    want_id: r.get(0),
                    dataset: r.get(1),
                    kind: WantChangeKind::from_code(r.get(2)).ok_or_else(|| {
                        StoreError::Unavailable("a want change of no kind".into())
                    })?,
                    subjects: r.get(3),
                    reason: r.get(4),
                    by: r.get(5),
                    want: want
                        .map(|w| ObservationsWantedEvent::decode(&w[..]).map_err(unreadable))
                        .transpose()?,
                    at_ns: r.get(7),
                })
            })
            .collect()
    }

    fn served(&self) -> Result<Vec<Served>> {
        Ok(self
            .conn()?
            .query(
                "SELECT dataset, first_sequence, last_sequence, subjects, fields, readers, at_ns
                   FROM lake_served ORDER BY served_id",
                &[],
            )
            .map_err(unavailable)?
            .iter()
            .map(|r| Served {
                dataset: r.get(0),
                first_sequence: r.get::<_, i64>(1) as u64,
                last_sequence: r.get::<_, i64>(2) as u64,
                subjects: r.get(3),
                fields: r.get(4),
                readers: r.get(5),
                at_ns: r.get(6),
            })
            .collect())
    }

    fn remove_before(
        &self,
        dataset: &str,
        recorded_before_ns: i64,
        why: &str,
        now_ns: i64,
    ) -> Result<u64> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        // The partition held as a batch holds it, so no row key's next
        // version is recorded between finding a key left with none and
        // counting it out of the rows in force (contract v29, ruling 619
        // (2); migration 8).
        tx.execute(
            "SELECT 1 FROM lake_partition WHERE dataset = $1 FOR UPDATE",
            &[&dataset],
        )
        .map_err(unavailable)?;
        // The rows removed, and the row keys left with no version kept: a
        // key whose later version stays is still in force at it.
        let gone = tx
            .query_one(
                "WITH gone AS (
                    DELETE FROM lake_row WHERE dataset = $1 AND recorded_at_ns < $2
                    RETURNING data_type, row_key)
                 SELECT (SELECT count(*) FROM gone),
                        (SELECT count(*)
                           FROM (SELECT DISTINCT data_type, row_key FROM gone) g
                          WHERE NOT EXISTS (
                                SELECT 1 FROM lake_row k
                                 WHERE k.dataset = $1 AND k.data_type = g.data_type
                                   AND k.row_key = g.row_key AND k.recorded_at_ns >= $2))",
                &[&dataset, &recorded_before_ns],
            )
            .map_err(unavailable)?;
        let removed = gone.get::<_, i64>(0) as u64;
        let keys_gone = gone.get::<_, i64>(1);
        if removed > 0 {
            tx.execute(
                "UPDATE lake_partition SET in_force = greatest(in_force - $2, 0) WHERE dataset = $1",
                &[&dataset, &keys_gone],
            )
            .map_err(unavailable)?;
            tx.execute(
                "INSERT INTO lake_removal (dataset, rows_removed, recorded_before_ns, why, at_ns)
                 VALUES ($1, $2, $3, $4, $5)",
                &[
                    &dataset,
                    &(removed as i64),
                    &recorded_before_ns,
                    &why,
                    &now_ns,
                ],
            )
            .map_err(unavailable)?;
        }
        tx.commit().map_err(unavailable)?;
        Ok(removed)
    }

    fn removals(&self) -> Result<Vec<Removal>> {
        Ok(self
            .conn()?
            .query(
                "SELECT dataset, rows_removed, recorded_before_ns, why, at_ns
                   FROM lake_removal ORDER BY removal_id",
                &[],
            )
            .map_err(unavailable)?
            .iter()
            .map(|r| Removal {
                dataset: r.get(0),
                rows: r.get::<_, i64>(1) as u64,
                recorded_before_ns: r.get(2),
                why: r.get(3),
                at_ns: r.get(4),
            })
            .collect())
    }

    fn count_miss(&self, instance: &str, at_ns: i64) -> Result<()> {
        self.conn()?
            .execute(
                "INSERT INTO lake_miss (instance, misses, last_at_ns) VALUES ($1, 1, $2)
                 ON CONFLICT (instance) DO UPDATE
                    SET misses = lake_miss.misses + 1, last_at_ns = excluded.last_at_ns",
                &[&instance, &at_ns],
            )
            .map_err(unavailable)?;
        Ok(())
    }

    fn miss_counts(&self) -> Result<BTreeMap<String, u64>> {
        Ok(self
            .conn()?
            .query("SELECT instance, misses FROM lake_miss", &[])
            .map_err(unavailable)?
            .iter()
            .map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1) as u64))
            .collect())
    }

    fn keep_alias(&self, replaced: &str, stays: &str, at_ns: i64) -> Result<()> {
        self.conn()?
            .execute(
                "INSERT INTO lake_alias (replaced, stays, heard_at_ns) VALUES ($1, $2, $3)
                 ON CONFLICT (replaced) DO NOTHING",
                &[&replaced, &stays, &at_ns],
            )
            .map_err(unavailable)?;
        Ok(())
    }

    fn aliases(&self) -> Result<BTreeMap<String, String>> {
        Ok(self
            .conn()?
            .query("SELECT replaced, stays FROM lake_alias", &[])
            .map_err(unavailable)?
            .iter()
            .map(|r| (r.get::<_, String>(0), r.get::<_, String>(1)))
            .collect())
    }

    fn keep_configuration(&self, event: &EntitlementsChangedEvent) -> Result<()> {
        self.conn()?
            .execute(
                "INSERT INTO lake_configuration (one, event, heard_at_ns) VALUES (true, $1, $2)
                 ON CONFLICT (one) DO UPDATE SET event = excluded.event,
                        heard_at_ns = excluded.heard_at_ns",
                &[&event.encode_to_vec(), &event.changed_at_ns],
            )
            .map_err(unavailable)?;
        Ok(())
    }

    fn configuration(&self) -> Result<Option<EntitlementsChangedEvent>> {
        self.conn()?
            .query_opt("SELECT event FROM lake_configuration", &[])
            .map_err(unavailable)?
            .map(|r| {
                let body: Vec<u8> = r.get(0);
                EntitlementsChangedEvent::decode(&body[..]).map_err(unreadable)
            })
            .transpose()
    }

    fn dated_range(&self, dataset: &str) -> Result<Option<(String, String)>> {
        let row = self
            .conn()?
            .query_one(
                "SELECT min(business_date)::text, max(business_date)::text FROM lake_row
                  WHERE dataset = $1 AND business_date IS NOT NULL",
                &[&dataset],
            )
            .map_err(unavailable)?;
        let first: Option<String> = row.get(0);
        let last: Option<String> = row.get(1);
        Ok(first.zip(last))
    }

    fn read_dataset(
        &self,
        dataset: &str,
        data_type: crate::row::DataType,
        as_of_ns: i64,
    ) -> Result<Vec<Observation>> {
        let cut_off = if as_of_ns == 0 { i64::MAX } else { as_of_ns };
        let mut conn = self.conn()?;
        let rows = conn
            .query(
                "SELECT body FROM lake_row
                  WHERE dataset = $1 AND data_type = $2 AND recorded_at_ns <= $3
                  ORDER BY sequence",
                &[&dataset, &data_type.code(), &cut_off],
            )
            .map_err(unavailable)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let body: Vec<u8> = row.get(0);
            out.push(Observation::decode(data_type, &body).map_err(unreadable)?);
        }
        let mut out = as_of(out, as_of_ns);
        let assessments = assessments_of(&mut *conn, &out)?;
        as_assessed(&mut out, &assessments, as_of_ns);
        Ok(out)
    }

    fn read_dataset_versions(
        &self,
        dataset: &str,
        data_type: crate::row::DataType,
    ) -> Result<Vec<Observation>> {
        let mut conn = self.conn()?;
        let rows = conn
            .query(
                "SELECT body FROM lake_row
                  WHERE dataset = $1 AND data_type = $2
                  ORDER BY sequence",
                &[&dataset, &data_type.code()],
            )
            .map_err(unavailable)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let body: Vec<u8> = row.get(0);
            out.push(Observation::decode(data_type, &body).map_err(unreadable)?);
        }
        let assessments = assessments_of(&mut *conn, &out)?;
        as_assessed(&mut out, &assessments, 0);
        Ok(out)
    }

    fn dated_on(&self, dataset: &str, dates: &[String]) -> Result<Vec<Observation>> {
        let mut conn = self.conn()?;
        let mut tx = conn
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .map_err(unavailable)?;
        let rows = tx
            .query(
                "SELECT data_type, body FROM lake_row
                  WHERE dataset = $1 AND business_date = ANY($2::text[]::date[])
                  ORDER BY sequence",
                &[&dataset, &dates],
            )
            .map_err(unavailable)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let code: i16 = row.get(0);
            let body: Vec<u8> = row.get(1);
            let data_type = crate::row::DataType::from_code(code)
                .ok_or_else(|| StoreError::Unavailable(format!("data type {code} unknown")))?;
            out.push(Observation::decode(data_type, &body).map_err(unreadable)?);
        }
        let amendments = amendments_of(&mut tx, &[dataset.to_string()])?;
        tx.commit().map_err(unavailable)?;
        as_flagged(&mut out, &amendments, 0);
        Ok(out)
    }

    fn amend_closed_day(&self, amendments: &[ClosedDayAmendment]) -> Result<()> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        for amendment in amendments {
            tx.execute(
                "INSERT INTO lake_closed_day (dataset, sequence, reason, closed_day, why, at_ns)
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &[
                    &amendment.dataset,
                    &(amendment.sequence as i64),
                    &closed_reason(amendment.closed_day.as_ref()),
                    &amendment
                        .closed_day
                        .as_ref()
                        .map(|check| check.encode_to_vec()),
                    &amendment.why,
                    &amendment.at_ns,
                ],
            )
            .map_err(unavailable)?;
        }
        tx.commit().map_err(unavailable)?;
        Ok(())
    }

    fn closed_day_amendments(&self) -> Result<Vec<ClosedDayAmendment>> {
        self.conn()?
            .query(
                "SELECT dataset, sequence, closed_day, why, at_ns FROM lake_closed_day
                  ORDER BY amendment_id",
                &[],
            )
            .map_err(unavailable)?
            .iter()
            .map(amendment_of)
            .collect()
    }

    fn quality_counts(&self) -> Result<BTreeMap<String, (u64, u64, u64)>> {
        // The rows in force from lake_partition, the flagged from the rows
        // recorded flagged (lake_row_quality) and the escalations
        // (lake_quality_assessment), each counted only while no later
        // version of its row key is kept (lake_row_version): never every
        // row kept (ruling 816). One snapshot, so the two agree.
        let mut conn = self.conn()?;
        let mut tx = conn
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .map_err(unavailable)?;
        let mut counts: BTreeMap<String, (u64, u64, u64)> = BTreeMap::new();
        for row in tx
            .query(
                "SELECT dataset, in_force FROM lake_partition WHERE in_force > 0",
                &[],
            )
            .map_err(unavailable)?
        {
            counts.insert(row.get::<_, String>(0), (row.get::<_, i64>(1) as u64, 0, 0));
        }
        for row in tx
            .query(
                "SELECT r.dataset,
                        count(*) FILTER (WHERE e.state = 2),
                        count(*) FILTER (WHERE e.state = 3)
                   FROM (SELECT dataset, sequence, max(state) AS state
                           FROM (SELECT dataset, sequence, quality AS state
                                   FROM lake_row WHERE quality IN (2, 3)
                                 UNION ALL
                                 SELECT dataset, sequence, state
                                   FROM lake_quality_assessment) flagged
                          GROUP BY dataset, sequence) e
                   JOIN lake_row r ON r.dataset = e.dataset AND r.sequence = e.sequence
                  WHERE NOT EXISTS (
                        SELECT 1 FROM lake_row n
                         WHERE n.dataset = r.dataset AND n.data_type = r.data_type
                           AND n.row_key = r.row_key AND n.version > r.version)
                  GROUP BY r.dataset",
                &[],
            )
            .map_err(unavailable)?
        {
            let dataset = row.get::<_, String>(0);
            let (suspicious, invalid) = (row.get::<_, i64>(1) as u64, row.get::<_, i64>(2) as u64);
            let count = counts.entry(dataset).or_default();
            count.0 = count.0.saturating_sub(suspicious + invalid);
            count.1 = suspicious;
            count.2 = invalid;
        }
        tx.commit().map_err(unavailable)?;
        Ok(counts)
    }

    fn closed_day_counts(&self) -> Result<BTreeMap<String, (u64, u64)>> {
        // Each row's flag as known now: its latest re-check, else the reason
        // it was recorded with. Only a row recorded flagged (lake_row_closed,
        // migration 6) or re-checked since (lake_closed_day) can count, so
        // only those are read, never every row kept (ruling 816).
        Ok(self
            .conn()?
            .query(
                "SELECT dataset,
                        count(*) FILTER (WHERE reason IN (1, 2)),
                        count(*) FILTER (WHERE reason = 3)
                   FROM (
                        SELECT r.dataset, coalesce(a.reason, r.closed_reason) AS reason
                          FROM lake_row r
                          LEFT JOIN LATERAL (
                               SELECT c.reason FROM lake_closed_day c
                                WHERE c.dataset = r.dataset AND c.sequence = r.sequence
                                ORDER BY c.amendment_id DESC LIMIT 1) a ON true
                         WHERE r.closed_reason <> 0
                        UNION ALL
                        (SELECT DISTINCT ON (c.dataset, c.sequence) c.dataset, c.reason
                           FROM lake_closed_day c
                           JOIN lake_row r ON r.dataset = c.dataset AND r.sequence = c.sequence
                          WHERE r.closed_reason = 0
                          ORDER BY c.dataset, c.sequence, c.amendment_id DESC)
                   ) flagged
                  GROUP BY dataset",
                &[],
            )
            .map_err(unavailable)?
            .iter()
            .map(|r| {
                (
                    r.get::<_, String>(0),
                    (r.get::<_, i64>(1) as u64, r.get::<_, i64>(2) as u64),
                )
            })
            .collect())
    }

    fn row_version(&self, dataset: &str, row_key: &str, version: u64) -> Result<Vec<Observation>> {
        let mut conn = self.conn()?;
        let rows = conn
            .query(
                "SELECT data_type, body FROM lake_row
                  WHERE dataset = $1 AND row_key = $2 AND version = $3
                  ORDER BY sequence",
                &[&dataset, &row_key, &(version as i64)],
            )
            .map_err(unavailable)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let code: i16 = row.get(0);
            let body: Vec<u8> = row.get(1);
            let data_type = crate::row::DataType::from_code(code)
                .ok_or_else(|| StoreError::Unavailable(format!("data type {code} unknown")))?;
            out.push(Observation::decode(data_type, &body).map_err(unreadable)?);
        }
        let assessments = assessments_of(&mut *conn, &out)?;
        as_assessed(&mut out, &assessments, 0);
        Ok(out)
    }

    fn assess(&self, assessments: &[Assessment]) -> Result<()> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        for assessment in assessments {
            let reasons: Vec<i16> = assessment.reasons.iter().map(|r| *r as i16).collect();
            tx.execute(
                "INSERT INTO lake_quality_assessment
                        (dataset, sequence, row_key, version, state, reasons, by_instance, at_ns)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                &[
                    &assessment.dataset,
                    &(assessment.sequence as i64),
                    &assessment.row_key,
                    &(assessment.version as i64),
                    &(assessment.state as i16),
                    &reasons,
                    &assessment.by_instance,
                    &assessment.at_ns,
                ],
            )
            .map_err(unavailable)?;
        }
        tx.commit().map_err(unavailable)?;
        Ok(())
    }

    fn assessments(&self) -> Result<Vec<Assessment>> {
        Ok(self
            .conn()?
            .query(
                "SELECT dataset, sequence, row_key, version, state, reasons, by_instance, at_ns
                   FROM lake_quality_assessment ORDER BY assessment_id",
                &[],
            )
            .map_err(unavailable)?
            .iter()
            .map(assessment_of)
            .collect())
    }
}

fn apply_migrations(conn: &mut Connection, clock: &dyn meridian_clock::Clock) -> Result<()> {
    conn.batch_execute(migrations::HISTORY)
        .map_err(unavailable)?;
    let applied: Option<i64> = conn
        .query_one("SELECT max(version) FROM lake_schema_migration", &[])
        .map_err(unavailable)?
        .get(0);
    for migration in migrations::MIGRATIONS {
        if applied.is_some_and(|at| at >= migration.version) {
            continue;
        }
        let mut tx = conn.transaction().map_err(unavailable)?;
        tx.batch_execute(migration.sql).map_err(unavailable)?;
        migrations::record(&mut tx, migration, clock.now_ns())?;
        tx.commit().map_err(unavailable)?;
    }
    let filled = backfill_latest_keys(conn)?;
    if filled > 0 {
        tracing::info!(filled, "rows given the key their latest is chosen by");
    }
    let listed = backfill_listing(conn)?;
    if listed > 0 {
        tracing::info!(listed, "series rows given their expiry and listed type");
    }
    Ok(())
}

fn unreadable(failed: prost::DecodeError) -> StoreError {
    StoreError::Unavailable(format!("a row of the lake did not read: {failed}"))
}

fn unavailable(failed: impl std::error::Error) -> StoreError {
    let mut detail = failed.to_string();
    let mut cause = failed.source();
    while let Some(next) = cause {
        detail.push_str(": ");
        detail.push_str(&next.to_string());
        cause = next.source();
    }
    StoreError::Unavailable(detail)
}
