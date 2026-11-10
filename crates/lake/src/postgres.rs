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

use postgres::{GenericClient, IsolationLevel, Transaction};
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

type Pool = r2d2::Pool<PostgresConnectionManager<meridian_changes::tls::DatabaseTls>>;
type Connection =
    r2d2::PooledConnection<PostgresConnectionManager<meridian_changes::tls::DatabaseTls>>;

/// Names the schema lock: distinct from every other store's. `migrate`
/// holds it, its backfills included, so whatever else takes it waits for a
/// migrate under way to finish and keeps the next from starting.
pub const SCHEMA_LOCK: i64 = 0x6c61_6b65_0000_0001_u64 as i64;

pub struct PostgresStore {
    pool: Pool,
    /// The epoch this writer's batches commit under (step 3a).
    fence: meridian_changes::Fence,
}

impl PostgresStore {
    pub fn connect(url: &str, pool_size: u32) -> Result<Self> {
        let mut config = meridian_changes::database_config(url).map_err(unavailable)?;
        // The lake's batches commit locally, never waiting on a synchronous
        // standby (step 3b, spec section 4, question 5): its rows are
        // re-fetchable from their sources, and its standby's lag is bounded
        // and alarmed instead. The book's, the street store's, the
        // instrument store's and the configuration's commits wait for it.
        // Harmless on a database with no synchronous standby.
        // Beside whatever options the URL names (a search_path, say), never
        // in their place.
        let options = match config.get_options() {
            Some(named) if !named.trim().is_empty() => {
                format!("{named} -c synchronous_commit=local")
            }
            _ => "-c synchronous_commit=local".to_string(),
        };
        config.options(&options);
        let manager = PostgresConnectionManager::new(
            config,
            meridian_changes::tls::database_tls().map_err(unavailable)?,
        );
        let pool = r2d2::Pool::builder()
            .max_size(pool_size.max(1))
            .build(manager)
            .map_err(unavailable)?;
        Ok(Self {
            pool,
            fence: meridian_changes::Fence::new(),
        })
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

    /// Re-read the record (step 3b, integrity at rest; plan 5j): rows kept
    /// before digests given theirs a batch at a time, a pause between, so
    /// the writer's own work goes first; then every row's digest checked
    /// against its body, a dataset whose rows no longer match named by its
    /// first bad sequence and when it was recorded. Postgres verifies each
    /// page's checksum as it reads it from disk.
    pub fn scrub(&self) -> Result<meridian_changes::integrity::Scrubbed> {
        let mut conn = self.conn()?;
        match scrub_rows(&mut *conn) {
            Err(failed) => match meridian_changes::integrity::page_finding(
                &failed,
                "a page of lake_row failed its checksum",
            ) {
                Some(finding) => Ok(meridian_changes::integrity::Scrubbed {
                    findings: vec![finding],
                    ..Default::default()
                }),
                None => Err(unavailable(failed)),
            },
            Ok(scrubbed) => Ok(scrubbed),
        }
    }

    /// Keep a scrub's findings and its run (`IntegrityFinding`).
    pub fn record_scrub(
        &self,
        scrubbed: &meridian_changes::integrity::Scrubbed,
        member: &str,
        started_at_ns: i64,
        finished_at_ns: i64,
    ) -> Result<()> {
        let mut conn = self.conn()?;
        meridian_changes::integrity::record(
            &mut *conn,
            "lake",
            scrubbed,
            member,
            started_at_ns,
            finished_at_ns,
        )
        .map_err(unavailable)
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

    /// The writer's epoch, adopted at its first batch where it was not
    /// started as one.
    fn epoch_now(&self) -> Result<u64> {
        match self.fence.epoch() {
            0 => self.adopt(),
            epoch => Ok(epoch),
        }
    }

    /// Log what is not a dataset's row in the `control` partition, in this
    /// transaction (step 3b): its head taken under its lock, the writer's
    /// epoch checked against it as a batch's is, one change each.
    fn log_control(
        &self,
        tx: &mut Transaction<'_>,
        entries: &[(&str, Vec<u8>, i64)],
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let row = tx
            .query_one(
                "SELECT sequence, epoch FROM lake_change_head WHERE partition = 'control' FOR UPDATE",
                &[],
            )
            .map_err(unavailable)?;
        let epoch: i64 = row.get(1);
        self.fence.admits(epoch as u64)?;
        let mut sequence: i64 = row.get(0);
        for (kind, body, at_ns) in entries {
            sequence += 1;
            tx.execute(
                "INSERT INTO lake_control_change (sequence, epoch, committed_at_ns, kind, body)
                 VALUES ($1, $2, $3, $4, $5)",
                &[&sequence, &epoch, at_ns, kind, body],
            )
            .map_err(unavailable)?;
        }
        tx.execute(
            "UPDATE lake_change_head SET sequence = $1 WHERE partition = 'control'",
            &[&sequence],
        )
        .map_err(unavailable)?;
        Ok(())
    }

    /// The partition's head, taken under its lock, its epoch checked against
    /// the writer's (step 3a): a batch commits only while the head row's
    /// epoch is its own. A dataset begun here takes the writer's epoch, the
    /// `control` partition's checked first, so a takeover that raised it is
    /// never undone by a dataset its deposed writer begins.
    fn head(&self, tx: &mut Transaction<'_>, dataset: &str) -> Result<u64> {
        let epoch = self.epoch_now()?;
        let begun = tx
            .execute(
                "INSERT INTO lake_partition (dataset, head, epoch, origin) VALUES ($1, 0, $2, $3)
                 ON CONFLICT DO NOTHING",
                &[&dataset, &(epoch as i64), &meridian_changes::origin()],
            )
            .map_err(unavailable)?;
        if begun == 1 {
            let control: i64 = tx
                .query_one(
                    "SELECT epoch FROM lake_change_head WHERE partition = 'control' FOR SHARE",
                    &[],
                )
                .map_err(unavailable)?
                .get(0);
            self.fence.admits(control as u64)?;
        }
        let row = tx
            .query_one(
                "SELECT head, epoch FROM lake_partition WHERE dataset = $1 FOR UPDATE",
                &[&dataset],
            )
            .map_err(unavailable)?;
        self.fence.admits(row.get::<_, i64>(1) as u64)?;
        Ok(row.get::<_, i64>(0) as u64)
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

/// Every closed-day re-check of these rows of one dataset, in order.
fn amendments_at(
    client: &mut impl GenericClient,
    dataset: &str,
    sequences: &[i64],
) -> Result<Vec<ClosedDayAmendment>> {
    if sequences.is_empty() {
        return Ok(Vec::new());
    }
    let rows = client
        .query(
            "SELECT dataset, sequence, closed_day, why, at_ns FROM lake_closed_day
              WHERE dataset = $1 AND sequence = ANY($2) ORDER BY amendment_id",
            &[&dataset, &sequences],
        )
        .map_err(unavailable)?;
    rows.iter().map(amendment_of).collect()
}

/// The closed-day re-checks of these rows alone, in order (stage M; the
/// store memory audit of 2026-11-09, item 12): read beside each chunk, as
/// its assessments are, never every re-check its datasets ever had.
fn amendments_for(
    client: &mut impl GenericClient,
    rows: &[Observation],
) -> Result<Vec<ClosedDayAmendment>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let (datasets, sequences): (Vec<String>, Vec<i64>) = rows
        .iter()
        .map(|row| (row.dataset().to_string(), row.meta().sequence as i64))
        .unzip();
    let found = client
        .query(
            "SELECT c.dataset, c.sequence, c.closed_day, c.why, c.at_ns
               FROM lake_closed_day c
               JOIN (SELECT DISTINCT * FROM unnest($1::text[], $2::bigint[])) AS asked(dataset, sequence)
                 ON c.dataset = asked.dataset AND c.sequence = asked.sequence
              ORDER BY c.amendment_id",
            &[&datasets, &sequences],
        )
        .map_err(unavailable)?;
    found.iter().map(amendment_of).collect()
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

fn insert(tx: &mut Transaction<'_>, dataset: &str, row: &Observation, epoch: u64) -> Result<()> {
    let meta = row.meta();
    let date: Option<String> = (!meta.business_date.is_empty()).then(|| meta.business_date.clone());
    // A series row's expiry and listed type (migration 7); none for any other.
    let listing = row.listing();
    let expires: Option<String> = listing.as_ref().and_then(|(expires, _)| expires.clone());
    let listed_type: Option<i16> = listing.map(|(_, kind)| kind as i16);
    // Its digest, kept beside it (step 3b, integrity at rest).
    let body = row.encode();
    let digest = meridian_changes::integrity::digest(&body);
    tx.execute(
        "INSERT INTO lake_row
                (dataset, sequence, data_type, row_key, version, subject, subjects, kind,
                 interval_ns, venue_id, valid_from_ns, valid_until_ns, business_date,
                 recorded_at_ns, previous_sequence, unconverted, body, closed_reason,
                 latest_key, quality, expires, listed_type, published_at_ns, epoch, digest)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13::text::date, $14, $15,
                 $16, $17, $18, $19, $20, $21::text::date, $22, $23, $24, $25)",
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
            &body,
            &closed_reason(meta.closed_day.as_ref()),
            &row.latest_key(),
            &(meta.quality as i16),
            &expires,
            &listed_type,
            // A rate's published time, its versions' order in force
            // (ruling 1006 (b); migration 8).
            &crate::store::published_order(row),
            &(epoch as i64),
            &digest,
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
/// latest version of its row key among the rows the read would load, a
/// rate's by published time ([`LATEST_BY_PUBLISHED`]). Each part is written
/// so a table without statistics yet (a fresh deployment, the gate's) is
/// read by the same indexes.
static LATEST: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| latest(false));

/// [`LATEST`] for rates (ruling 1006 (b)): a row key's version in force is
/// its latest by published time, then by version
/// ([`crate::store::force_order`]), read from `lake_row.published_at_ns`
/// (migration 8), so a late-arriving older publication never takes force.
/// A rate row not yet given its published time orders first.
static LATEST_BY_PUBLISHED: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| latest(true));

fn latest(by_published: bool) -> String {
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
    let in_force = if by_published {
        format!(
            "r.version = (SELECT v.version FROM lake_row v
                           WHERE v.dataset = r.dataset AND v.data_type = r.data_type
                             AND v.row_key = r.row_key
                             AND v.subjects && $2::text[] AND {}
                           ORDER BY COALESCE(v.published_at_ns, 0) DESC, v.version DESC
                           LIMIT 1)",
            narrowed("v")
        )
    } else {
        format!(
            "r.version = (SELECT max(v.version) FROM lake_row v
                           WHERE v.dataset = r.dataset AND v.data_type = r.data_type
                             AND v.row_key = r.row_key
                             AND v.subjects && $2::text[] AND {})",
            narrowed("v")
        )
    };
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
}

/// Whether the row `r` is no longer its row key's version in force (ruling
/// 1006 (b)): a version of its key is kept that a default read would serve
/// in its place -- later by published time, then by version, for a rate;
/// later by version for any other data type, whose `published_at_ns` is
/// NULL ([`crate::store::force_order`]; migration 8). Read by
/// `lake_row_version`, a key's few versions; what every count of rows in
/// force tests a counted row by.
const SUPERSEDED: &str = "EXISTS (
    SELECT 1 FROM lake_row n
     WHERE n.dataset = r.dataset AND n.data_type = r.data_type AND n.row_key = r.row_key
       AND (COALESCE(n.published_at_ns, 0), n.version)
           > (COALESCE(r.published_at_ns, 0), r.version))";

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

/// Fill in `published_at_ns` where a rate row has none (migration 8; ruling
/// 1006 (b)): its published time, read from its own body, batch by batch
/// as [`backfill_latest_keys`] does, by `lake_row_unpublished`. A row whose
/// body does not read is passed over, ordered first among its key's
/// versions. Run by `migrate`, every time.
fn backfill_published(conn: &mut Connection) -> Result<u64> {
    let mut after: (String, i64) = (String::new(), 0);
    let mut filled = 0u64;
    loop {
        let mut tx = conn.transaction().map_err(unavailable)?;
        let rows = tx
            .query(
                "SELECT dataset, sequence, body FROM lake_row
                  WHERE (dataset, sequence) > ($1, $2)
                    AND data_type = 6 AND published_at_ns IS NULL
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
        let (mut datasets, mut sequences, mut published) = (Vec::new(), Vec::new(), Vec::new());
        for row in &rows {
            let body: Vec<u8> = row.get(2);
            let Some(at) = Observation::decode(DataType::Rate, &body)
                .ok()
                .and_then(|observation| crate::store::published_order(&observation))
            else {
                continue;
            };
            datasets.push(row.get::<_, String>(0));
            sequences.push(row.get::<_, i64>(1));
            published.push(at);
        }
        filled += tx
            .execute(
                "UPDATE lake_row r SET published_at_ns = f.published_at_ns
                   FROM unnest($1::text[], $2::bigint[], $3::bigint[])
                        AS f(dataset, sequence, published_at_ns)
                  WHERE r.dataset = f.dataset AND r.sequence = f.sequence",
                &[&datasets, &sequences, &published],
            )
            .map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        tracing::info!(filled, "rate rows given their published time so far");
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

/// How many rows a read takes from the database at once, decoded and
/// handed on before the next ([`crate::store::READ_CHUNK`]).
const READ_CHUNK: i32 = crate::store::READ_CHUNK as i32;

/// A read's rows, every version the read would load, in the order
/// recorded: where every version is answered (`every_version`).
const EVERY_VERSION: &str = "
SELECT body FROM lake_row
 WHERE dataset = ANY($1) AND subjects && $2 AND data_type = $3
   AND (cardinality($4::smallint[]) = 0 OR kind = ANY($4))
   AND ($5::bigint = 0 OR interval_ns = $5)
   AND recorded_at_ns <= $6
   AND ($7::text IS NULL OR business_date = $7::text::date)
   AND valid_from_ns >= $8 AND valid_from_ns < $9 AND valid_from_ns <= $10
 ORDER BY dataset, sequence";

/// A read's rows, each row key at its version in force among those the read
/// would load: its latest by published time, then by version
/// ([`crate::store::force_order`]; `lake_row.published_at_ns`, which only a
/// rate's row carries), as `as_of` chooses of every version. Chosen by the
/// database from the keys' columns, the bodies read only of the rows in
/// force (found on dev 2026-11-09: a chain's statistics over its dates, 49,412
/// rows in force of 358,688 versions, every version read and decoded to
/// choose them, a page at a time, took the lake past its 1 GiB twice).
const IN_FORCE: &str = "
SELECT r.body FROM lake_row r
  JOIN (SELECT DISTINCT ON (dataset, row_key) dataset, sequence
          FROM lake_row
         WHERE dataset = ANY($1) AND subjects && $2 AND data_type = $3
           AND (cardinality($4::smallint[]) = 0 OR kind = ANY($4))
           AND ($5::bigint = 0 OR interval_ns = $5)
           AND recorded_at_ns <= $6
           AND ($7::text IS NULL OR business_date = $7::text::date)
           AND valid_from_ns >= $8 AND valid_from_ns < $9 AND valid_from_ns <= $10
         ORDER BY dataset, row_key, COALESCE(published_at_ns, 0) DESC, version DESC) AS f
    ON r.dataset = f.dataset AND r.sequence = f.sequence
 ORDER BY r.dataset, r.sequence";

/// [`IN_FORCE`] as published by a time (contract v27, W10.6): a rate's row
/// keys each at its version in force among those published by `$11`, by
/// `lake_row.published_at_ns` -- its published time, else when the lake
/// recorded it, as [`crate::read::published_at`] reads it -- then by
/// version, as [`crate::read::as_published`] chooses. Chosen by the
/// database, the bodies read only of the rows in force (found 2026-11-09:
/// every version in the read's window was read, decoded and held to
/// choose them; Treasury's par curve keeps about 100,000 rows for its one
/// subject).
const IN_FORCE_PUBLISHED: &str = "
SELECT r.body FROM lake_row r
  JOIN (SELECT DISTINCT ON (dataset, row_key) dataset, sequence
          FROM lake_row
         WHERE dataset = ANY($1) AND subjects && $2 AND data_type = $3
           AND (cardinality($4::smallint[]) = 0 OR kind = ANY($4))
           AND ($5::bigint = 0 OR interval_ns = $5)
           AND recorded_at_ns <= $6
           AND ($7::text IS NULL OR business_date = $7::text::date)
           AND valid_from_ns >= $8 AND valid_from_ns < $9 AND valid_from_ns <= $10
           AND published_at_ns <= $11
         ORDER BY dataset, row_key, published_at_ns DESC, version DESC) AS f
    ON r.dataset = f.dataset AND r.sequence = f.sequence
 ORDER BY r.dataset, r.sequence";

/// [`EVERY_VERSION`] a row key at a time, each key's versions together:
/// what a read as published by a time chooses from where the published
/// time is in the row's body alone (every data type but a rate), keeping
/// one row a key as it reads ([`crate::read::ByPublished`]).
const EVERY_VERSION_BY_KEY: &str = "
SELECT body FROM lake_row
 WHERE dataset = ANY($1) AND subjects && $2 AND data_type = $3
   AND (cardinality($4::smallint[]) = 0 OR kind = ANY($4))
   AND ($5::bigint = 0 OR interval_ns = $5)
   AND recorded_at_ns <= $6
   AND ($7::text IS NULL OR business_date = $7::text::date)
   AND valid_from_ns >= $8 AND valid_from_ns < $9 AND valid_from_ns <= $10
 ORDER BY dataset, row_key, version";

impl Store for PostgresStore {
    fn record(&self, dataset: &str, rows: Vec<Observation>, now_ns: i64) -> Result<Recorded> {
        // Adopted before a connection is taken: one store never waits on
        // itself for a second (step 3a).
        self.epoch_now()?;
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        let mut head = self.head(&mut tx, dataset)?;
        let epoch = self.fence.epoch();
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
            insert(&mut tx, dataset, row, epoch)?;
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
        self.epoch_now()?;
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        let mut head = self.head(&mut tx, dataset)?;
        let epoch = self.fence.epoch();
        let first = head + 1;
        let done = number(rows, &mut head, now_ns, |_| None, |_| 0);
        if !done.rows.is_empty() {
            let served = served_of(dataset, &done.rows, first, head, readers, now_ns);
            tx.execute(
                "INSERT INTO lake_served
                        (dataset, first_sequence, last_sequence, subjects, fields, readers, at_ns,
                         epoch)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                &[
                    &served.dataset,
                    &(served.first_sequence as i64),
                    &(served.last_sequence as i64),
                    &served.subjects,
                    &served.fields,
                    &served.readers,
                    &served.at_ns,
                    &(epoch as i64),
                ],
            )
            .map_err(unavailable)?;
        }
        Self::move_head(&mut tx, dataset, head, 0)?;
        tx.commit().map_err(unavailable)?;
        Ok(done)
    }

    fn read(&self, query: &Query) -> Result<Vec<Observation>> {
        let mut out = Vec::new();
        self.read_each(query, &mut |row| {
            out.push(row);
            Ok(())
        })?;
        Ok(out)
    }

    fn read_each(
        &self,
        query: &Query,
        each: &mut dyn FnMut(Observation) -> Result<()>,
    ) -> Result<()> {
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
            // A chunk at a time from one portal, in the order the rows were
            // recorded, each handed on as it is decoded (stage M; the store
            // memory audit of 2026-11-09, item 4): a catch-up from an old
            // watermark is the whole dataset, and was read whole, decoded
            // and sorted before the first row was handed on. The version in
            // force of each row key among those after the watermark is
            // chosen by the database, by the order versions take force in
            // (ruling 1006 (b): published time, then version), as
            // `memory::as_of` chose it; every version where the read asks
            // for every one, by the time each was recorded.
            let chosen = if query.published_as_of_ns != 0 {
                "SELECT body FROM lake_row
                   JOIN unnest($1::text[], $2::bigint[]) AS asked(dataset, after)
                     ON lake_row.dataset = asked.dataset AND lake_row.sequence > asked.after
                  WHERE subjects && $3 AND data_type = $4 AND recorded_at_ns <= $5
                  ORDER BY lake_row.dataset, row_key, version"
            } else if crate::store::every_version(query) {
                "SELECT body FROM lake_row
                   JOIN unnest($1::text[], $2::bigint[]) AS asked(dataset, after)
                     ON lake_row.dataset = asked.dataset AND lake_row.sequence > asked.after
                  WHERE subjects && $3 AND data_type = $4 AND recorded_at_ns <= $5
                  ORDER BY lake_row.dataset, sequence"
            } else {
                "SELECT body FROM (
                     SELECT DISTINCT ON (lake_row.dataset, row_key) body,
                            lake_row.dataset AS in_dataset, sequence AS at_sequence
                       FROM lake_row
                       JOIN unnest($1::text[], $2::bigint[]) AS asked(dataset, after)
                         ON lake_row.dataset = asked.dataset AND lake_row.sequence > asked.after
                      WHERE subjects && $3 AND data_type = $4 AND recorded_at_ns <= $5
                      ORDER BY lake_row.dataset, row_key,
                               COALESCE(published_at_ns, 0) DESC, version DESC
                 ) AS in_force
                 ORDER BY in_dataset, at_sequence"
            };
            let data_type = query.data_type.code();
            let params: [&(dyn postgres::types::ToSql + Sync); 5] =
                [&datasets, &after, &query.subjects, &data_type, &cut_off];
            let portal = tx.bind(chosen, &params).map_err(unavailable)?;
            // As published by a time (contract v27, W10.6): each row key's
            // versions read together, the one in force by then kept as they
            // are read ([`crate::read::ByPublished`]), as `as_published`
            // chose from every version held.
            let mut by_published = (query.published_as_of_ns != 0)
                .then(|| crate::read::ByPublished::new(query.published_as_of_ns));
            loop {
                let rows = tx.query_portal(&portal, READ_CHUNK).map_err(unavailable)?;
                if rows.is_empty() {
                    break;
                }
                let mut chunk = Vec::with_capacity(rows.len());
                for row in rows {
                    let body: &[u8] = row.get(0);
                    chunk.push(Observation::decode(query.data_type, body).map_err(unreadable)?);
                }
                let assessments = assessments_of(&mut tx, &chunk)?;
                let amendments = amendments_for(&mut tx, &chunk)?;
                as_flagged(&mut chunk, &amendments, query.as_of_ns);
                as_assessed(&mut chunk, &assessments, query.as_of_ns);
                for row in chunk {
                    match by_published.as_mut() {
                        Some(choosing) => {
                            if let Some(chosen) = choosing.admit(row) {
                                each(chosen)?;
                            }
                        }
                        None => each(row)?,
                    }
                }
            }
            drop(portal);
            tx.commit().map_err(unavailable)?;
            if let Some(chosen) = by_published.and_then(crate::read::ByPublished::finish) {
                each(chosen)?;
            }
            return Ok(());
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
        let every_version = crate::store::every_version(query);
        let latest =
            query.latest_only && matches!(query.when, When::Latest { .. }) && !every_version;
        if latest {
            // Its plan is a few hundred index probes, estimated as if each
            // ran a key's whole history: compiled, it took a second more
            // than it ran (ruling 770's bench), so never compiled.
            tx.batch_execute("SET LOCAL jit = off")
                .map_err(unavailable)?;
        }
        // Every row the database answers is a row key's version in force,
        // or one of every version where every one is asked: none is chosen
        // here, so each is handed on as it is read.
        // A read as published by a time: a rate's rows in force by then
        // chosen by the database; any other's, whose published time only
        // its body holds, chosen here a row key at a time as they are read.
        let published = query.published_as_of_ns != 0;
        let mut by_published = (published && query.data_type != DataType::Rate)
            .then(|| crate::read::ByPublished::new(query.published_as_of_ns));
        let cut_published = published && by_published.is_none();
        let sql = if latest && query.data_type == DataType::Rate {
            LATEST_BY_PUBLISHED.as_str()
        } else if latest {
            LATEST.as_str()
        } else if cut_published {
            IN_FORCE_PUBLISHED
        } else if published {
            EVERY_VERSION_BY_KEY
        } else if every_version {
            EVERY_VERSION
        } else {
            IN_FORCE
        };
        let code = query.data_type.code();
        let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![
            &query.datasets,
            &query.subjects,
            &code,
            &kinds,
            &query.interval_ns,
            &cut_off,
            &date,
            &from,
            &until,
            &at,
        ];
        if cut_published {
            params.push(&query.published_as_of_ns);
        }
        // A chunk at a time from one portal, each chunk's assessments read
        // beside it, each row handed on as it is decoded: never the whole
        // result held, as the database sent it or decoded (found on dev
        // 2026-11-09: the lake OOMKilled at 1 GiB, twice, by a chain's
        // statistics read a page at a time, each page every version).
        let portal = tx.bind(sql, &params[..]).map_err(unavailable)?;
        loop {
            let rows = tx.query_portal(&portal, READ_CHUNK).map_err(unavailable)?;
            if rows.is_empty() {
                break;
            }
            let mut chunk = Vec::with_capacity(rows.len());
            for row in rows {
                let body: &[u8] = row.get(0);
                chunk.push(Observation::decode(query.data_type, body).map_err(unreadable)?);
            }
            let assessments = assessments_of(&mut tx, &chunk)?;
            let amendments = amendments_for(&mut tx, &chunk)?;
            as_flagged(&mut chunk, &amendments, query.as_of_ns);
            as_assessed(&mut chunk, &assessments, query.as_of_ns);
            for row in chunk {
                match by_published.as_mut() {
                    Some(choosing) => {
                        if let Some(chosen) = choosing.admit(row) {
                            each(chosen)?;
                        }
                    }
                    None => each(row)?,
                }
            }
        }
        drop(portal);
        tx.commit().map_err(unavailable)?;
        if let Some(chosen) = by_published.and_then(crate::read::ByPublished::finish) {
            each(chosen)?;
        }
        Ok(())
    }

    fn rows_at(
        &self,
        data_type: DataType,
        places: &[(String, u64)],
        as_of_ns: i64,
    ) -> Result<Vec<Observation>> {
        if places.is_empty() {
            return Ok(Vec::new());
        }
        let (datasets, sequences): (Vec<String>, Vec<i64>) = places
            .iter()
            .map(|(dataset, sequence)| (dataset.clone(), *sequence as i64))
            .unzip();
        let mut conn = self.conn()?;
        let mut tx = conn
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .map_err(unavailable)?;
        let code = data_type.code();
        let found = tx
            .query(
                "SELECT r.body
                   FROM unnest($1::text[], $2::bigint[]) WITH ORDINALITY AS p(dataset, sequence, n)
                   JOIN lake_row r ON r.dataset = p.dataset AND r.sequence = p.sequence
                  WHERE r.data_type = $3
                  ORDER BY p.n",
                &[&datasets, &sequences, &code],
            )
            .map_err(unavailable)?;
        let mut out = Vec::with_capacity(found.len());
        for row in found {
            let body: &[u8] = row.get(0);
            out.push(Observation::decode(data_type, body).map_err(unreadable)?);
        }
        let mut named: Vec<String> = datasets;
        named.sort();
        named.dedup();
        let amendments = amendments_for(&mut tx, &out)?;
        let assessments = assessments_of(&mut tx, &out)?;
        tx.commit().map_err(unavailable)?;
        as_flagged(&mut out, &amendments, as_of_ns);
        as_assessed(&mut out, &assessments, as_of_ns);
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
        // (migration 6), each counted while it is its row key's version in
        // force (ruling 1006 (c)).
        Ok(self
            .conn()?
            .query(
                &format!(
                    "SELECT r.dataset, count(*) FROM lake_row r
                      WHERE r.unconverted AND NOT {SUPERSEDED}
                      GROUP BY r.dataset"
                ),
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
        self.epoch_now()?;
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
        self.log_control(
            &mut tx,
            &[(
                crate::store::CONTROL_PRIORITY,
                priority.encode_to_vec(),
                priority.updated_at_ns,
            )],
        )?;
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
        self.epoch_now()?;
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        // The partition held as a batch holds it, so no row key's next
        // version is recorded between finding a key left with none and
        // counting it out of the rows in force (contract v29, ruling 619
        // (2); migration 8).
        let held = tx
            .query_opt(
                "SELECT epoch FROM lake_partition WHERE dataset = $1 FOR UPDATE",
                &[&dataset],
            )
            .map_err(unavailable)?;
        if let Some(held) = held {
            self.fence.admits(held.get::<_, i64>(0) as u64)?;
        }
        // The rows removed, and the row keys left with no version kept: a
        // key whose later version stays is still in force at it.
        let gone = tx
            .query_one(
                "WITH gone AS (
                    DELETE FROM lake_row WHERE dataset = $1 AND recorded_at_ns < $2
                    RETURNING data_type, row_key, sequence)
                 SELECT (SELECT count(*) FROM gone), (SELECT max(sequence) FROM gone),
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
        let last_gone: Option<i64> = gone.get(1);
        let keys_gone = gone.get::<_, i64>(2);
        if removed > 0 {
            // The log reaches no lower than the rows removed (step 3a): a
            // follower behind them reloads the partition at its head.
            tx.execute(
                "UPDATE lake_partition SET in_force = greatest(in_force - $2, 0),
                        kept_from = greatest(kept_from, $3)
                  WHERE dataset = $1",
                &[&dataset, &keys_gone, &(last_gone.unwrap_or(0) + 1)],
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
        self.epoch_now()?;
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        let kept = tx
            .execute(
                "INSERT INTO lake_alias (replaced, stays, heard_at_ns) VALUES ($1, $2, $3)
                 ON CONFLICT (replaced) DO NOTHING",
                &[&replaced, &stays, &at_ns],
            )
            .map_err(unavailable)?;
        // Logged once, when first kept: an alias heard again changes nothing.
        if kept == 1 {
            self.log_control(
                &mut tx,
                &[(
                    crate::store::CONTROL_ALIAS,
                    crate::store::alias_body(replaced, stays, at_ns),
                    at_ns,
                )],
            )?;
        }
        tx.commit().map_err(unavailable)?;
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
        published_as_of_ns: i64,
    ) -> Result<Vec<Observation>> {
        let cut_off = if as_of_ns == 0 { i64::MAX } else { as_of_ns };
        let mut conn = self.conn()?;
        if published_as_of_ns != 0 {
            // Each row key's versions together, a chunk at a time, each
            // key's version in force by the published-time cut kept as
            // they are read: never every version held (found 2026-11-09).
            let mut tx = conn
                .build_transaction()
                .isolation_level(IsolationLevel::RepeatableRead)
                .read_only(true)
                .start()
                .map_err(unavailable)?;
            let code = data_type.code();
            let portal = tx
                .bind(
                    "SELECT body FROM lake_row
                      WHERE dataset = $1 AND data_type = $2 AND recorded_at_ns <= $3
                      ORDER BY row_key, version",
                    &[&dataset, &code, &cut_off],
                )
                .map_err(unavailable)?;
            let mut choosing = crate::read::ByPublished::new(published_as_of_ns);
            let mut out = Vec::new();
            loop {
                let rows = tx.query_portal(&portal, READ_CHUNK).map_err(unavailable)?;
                if rows.is_empty() {
                    break;
                }
                for row in rows {
                    let body: &[u8] = row.get(0);
                    let row = Observation::decode(data_type, body).map_err(unreadable)?;
                    out.extend(choosing.admit(row));
                }
            }
            drop(portal);
            out.extend(choosing.finish());
            out.sort_by_key(|row| row.meta().sequence);
            let assessments = assessments_of(&mut tx, &out)?;
            tx.commit().map_err(unavailable)?;
            as_assessed(&mut out, &assessments, as_of_ns);
            return Ok(out);
        }
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

    fn read_dataset_each(
        &self,
        dataset: &str,
        data_type: crate::row::DataType,
        as_of_ns: i64,
        each: &mut dyn FnMut(Observation) -> Result<()>,
    ) -> Result<()> {
        // Each row key at its version in force by the cut-off, chosen by
        // the database by the order versions take force in (ruling 1006
        // (b)), as `as_of` chose it; read a chunk at a time from one portal,
        // each chunk's assessments beside it (stage M).
        let cut_off = if as_of_ns == 0 { i64::MAX } else { as_of_ns };
        let code = data_type.code();
        let mut conn = self.conn()?;
        let mut tx = conn
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .map_err(unavailable)?;
        let portal = tx
            .bind(
                "SELECT body FROM (
                     SELECT DISTINCT ON (row_key) body, sequence AS at_sequence
                       FROM lake_row
                      WHERE dataset = $1 AND data_type = $2 AND recorded_at_ns <= $3
                      ORDER BY row_key, COALESCE(published_at_ns, 0) DESC, version DESC
                 ) AS in_force
                 ORDER BY at_sequence",
                &[&dataset, &code, &cut_off],
            )
            .map_err(unavailable)?;
        loop {
            let rows = tx.query_portal(&portal, READ_CHUNK).map_err(unavailable)?;
            if rows.is_empty() {
                break;
            }
            let mut chunk = Vec::with_capacity(rows.len());
            for row in rows {
                let body: &[u8] = row.get(0);
                chunk.push(Observation::decode(data_type, body).map_err(unreadable)?);
            }
            let assessments = assessments_of(&mut tx, &chunk)?;
            as_assessed(&mut chunk, &assessments, as_of_ns);
            for row in chunk {
                each(row)?;
            }
        }
        tx.commit().map_err(unavailable)?;
        Ok(())
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

    fn dated_on(
        &self,
        dataset: &str,
        dates: &[String],
        after: u64,
        most: usize,
    ) -> Result<Vec<Observation>> {
        let mut conn = self.conn()?;
        let mut tx = conn
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .map_err(unavailable)?;
        // By the key, from where the last chunk ended: each chunk an index
        // range, never the dataset sorted again.
        let rows = tx
            .query(
                "SELECT data_type, body FROM lake_row
                  WHERE dataset = $1 AND sequence > $2
                    AND business_date = ANY($3::text[]::date[])
                  ORDER BY sequence
                  LIMIT $4",
                &[
                    &dataset,
                    &(after as i64),
                    &dates,
                    &i64::try_from(most).unwrap_or(i64::MAX),
                ],
            )
            .map_err(unavailable)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let code: i16 = row.get(0);
            let body: &[u8] = row.get(1);
            let data_type = crate::row::DataType::from_code(code)
                .ok_or_else(|| StoreError::Unavailable(format!("data type {code} unknown")))?;
            out.push(Observation::decode(data_type, body).map_err(unreadable)?);
        }
        // The re-checks of these rows alone, never every one the dataset's
        // rows have had.
        let sequences: Vec<i64> = out.iter().map(|row| row.meta().sequence as i64).collect();
        let amendments = amendments_at(&mut tx, dataset, &sequences)?;
        tx.commit().map_err(unavailable)?;
        as_flagged(&mut out, &amendments, 0);
        Ok(out)
    }

    fn amend_closed_day(&self, amendments: &[ClosedDayAmendment]) -> Result<()> {
        self.epoch_now()?;
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
        let logged: Vec<(&str, Vec<u8>, i64)> = amendments
            .iter()
            .map(|a| {
                (
                    crate::store::CONTROL_CLOSED_DAY,
                    crate::store::amendment_body(a),
                    a.at_ns,
                )
            })
            .collect();
        self.log_control(&mut tx, &logged)?;
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
        // (lake_quality_assessment), each counted only while it is its row
        // key's version in force (SUPERSEDED, by lake_row_version): never
        // every row kept (ruling 816). One snapshot, so the two agree.
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
                &format!(
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
                  WHERE NOT {SUPERSEDED}
                  GROUP BY r.dataset"
                ),
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
        // only those are read, never every row kept (ruling 816); each while
        // it is its row key's version in force (ruling 1006 (c)).
        Ok(self
            .conn()?
            .query(
                &format!(
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
                         WHERE r.closed_reason <> 0 AND NOT {SUPERSEDED}
                        UNION ALL
                        (SELECT DISTINCT ON (c.dataset, c.sequence) c.dataset, c.reason
                           FROM lake_closed_day c
                           JOIN lake_row r ON r.dataset = c.dataset AND r.sequence = c.sequence
                          WHERE r.closed_reason = 0 AND NOT {SUPERSEDED}
                          ORDER BY c.dataset, c.sequence, c.amendment_id DESC)
                   ) flagged
                  GROUP BY dataset"
                ),
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
        self.epoch_now()?;
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
        let logged: Vec<(&str, Vec<u8>, i64)> = assessments
            .iter()
            .map(|a| {
                (
                    crate::store::CONTROL_ASSESSMENT,
                    crate::store::assessment_body(a),
                    a.at_ns,
                )
            })
            .collect();
        self.log_control(&mut tx, &logged)?;
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

    fn adopt(&self) -> Result<u64> {
        let mut conn = self.conn()?;
        conn.execute(
            "UPDATE lake_change_head SET origin = $1 WHERE origin = '' AND $1 <> ''",
            &[&meridian_changes::origin()],
        )
        .map_err(unavailable)?;
        let epoch: i64 = conn
            .query_one(
                "SELECT epoch FROM lake_change_head WHERE partition = 'control'",
                &[],
            )
            .map_err(unavailable)?
            .get(0);
        Ok(self.fence.adopt_once(epoch as u64))
    }

    fn epoch(&self) -> u64 {
        self.fence.epoch()
    }

    fn take_over(&self, by: &meridian_changes::TookOver) -> Result<u64> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        // A deposed writer stopped mid-batch holds the rows it took.
        meridian_changes::end_deposed_writers(&mut tx, &["lake_change_head", "lake_partition"])
            .map_err(unavailable)?;
        // The control partition first: a dataset a deposed writer is
        // beginning holds it shared until it commits, and is then raised
        // with the rest.
        let control: i64 = tx
            .query_one(
                "UPDATE lake_change_head SET epoch = epoch + 1 WHERE partition = 'control'
                 RETURNING epoch",
                &[],
            )
            .map_err(unavailable)?
            .get(0);
        let highest: Option<i64> = tx
            .query_one("SELECT max(epoch) FROM lake_partition", &[])
            .map_err(unavailable)?
            .get(0);
        let epoch = control.max(highest.unwrap_or(0) + 1);
        tx.execute(
            "UPDATE lake_change_head SET epoch = $1 WHERE partition = 'control'",
            &[&epoch],
        )
        .map_err(unavailable)?;
        tx.execute("UPDATE lake_partition SET epoch = $1", &[&epoch])
            .map_err(unavailable)?;
        // Who took it over and why, in the same transaction (step 3b).
        tx.execute(
            "INSERT INTO lake_writer_took_over (epoch, member, partitions, why, origin, at_ns)
             SELECT $1, $2,
                    array_append(coalesce(array_agg(dataset ORDER BY dataset), '{}'), 'control'),
                    $3, $4, $5
               FROM lake_partition",
            &[
                &epoch,
                &by.member,
                &by.why,
                &meridian_changes::origin(),
                &by.at_ns,
            ],
        )
        .map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        self.fence.adopt(epoch as u64);
        Ok(epoch as u64)
    }

    fn head_epoch(&self) -> Result<u64> {
        let epoch: i64 = self
            .conn()?
            .query_one(
                "SELECT epoch FROM lake_change_head WHERE partition = 'control'",
                &[],
            )
            .map_err(unavailable)?
            .get(0);
        Ok(epoch as u64)
    }

    fn read_changes(
        &self,
        partition: &str,
        after: u64,
        most_bytes: usize,
    ) -> std::result::Result<meridian_changes::ReadChangesReply, meridian_changes::ReadError> {
        use meridian_changes::{ReadChangesReply, ReadError};
        let failed = |failed: StoreError| ReadError::Unavailable(failed.to_string());
        let mut conn = self.conn().map_err(failed)?;
        let mut tx = conn
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .map_err(|e| failed(unavailable(e)))?;
        if partition == meridian_changes::CONTROL {
            let row = tx
                .query_one(
                    "SELECT sequence, epoch, origin FROM lake_change_head WHERE partition = 'control'",
                    &[],
                )
                .map_err(|e| failed(unavailable(e)))?;
            let head = row.get::<_, i64>(0) as u64;
            let own_origin: String = row.get(2);
            // Kept for good: the log reaches from its first change.
            let lowest_kept = u64::from(head > 0);
            meridian_changes::check_kept(after, lowest_kept, head)?;
            let mut page = meridian_changes::Page::new(most_bytes);
            let mut from = after.min(i64::MAX as u64) as i64;
            'pages: while (from as u64) < head {
                let rows = tx
                    .query(
                        "SELECT sequence, epoch, origin, committed_at_ns, kind, body
                           FROM lake_control_change WHERE sequence > $1 ORDER BY sequence LIMIT 512",
                        &[&from],
                    )
                    .map_err(|e| failed(unavailable(e)))?;
                if rows.is_empty() {
                    break;
                }
                for row in rows {
                    let origin: Option<String> = row.get(2);
                    from = row.get(0);
                    let change = meridian_changes::Change {
                        partition: meridian_changes::CONTROL.to_string(),
                        sequence: from as u64,
                        epoch: row.get::<_, i64>(1) as u64,
                        origin: origin.unwrap_or_else(|| own_origin.clone()),
                        committed_at_ns: row.get(3),
                        kind: row.get(4),
                        digest: meridian_changes::integrity::digest(&row.get::<_, Vec<u8>>(5)),
                        body: row.get(5),
                    };
                    if !page.take(change) {
                        break 'pages;
                    }
                }
                if page.is_full() {
                    break;
                }
            }
            let _ = tx.commit();
            return Ok(ReadChangesReply {
                changes: page.changes,
                head,
                lowest_kept,
                epoch: row.get::<_, i64>(1) as u64,
                after_sequence: 0,
            });
        }
        let row = tx
            .query_opt(
                "SELECT head, epoch, origin, kept_from FROM lake_partition WHERE dataset = $1",
                &[&partition],
            )
            .map_err(|e| failed(unavailable(e)))?
            .ok_or_else(|| ReadError::UnknownPartition(partition.to_string()))?;
        let head = row.get::<_, i64>(0) as u64;
        let epoch = row.get::<_, i64>(1) as u64;
        let own_origin: String = row.get(2);
        let kept_from = row.get::<_, i64>(3) as u64;
        let lowest_kept = if head >= kept_from { kept_from } else { 0 };
        meridian_changes::check_kept(after, lowest_kept, head)?;
        // Read a window of sequences at a time: its rows and the ranges it
        // served, merged in sequence order, until the page is full.
        const WINDOW: i64 = 512;
        let mut page = meridian_changes::Page::new(most_bytes);
        let mut from = after.max(kept_from.saturating_sub(1));
        let mut full = false;
        while from < head && !full {
            let until = (from as i64).saturating_add(WINDOW);
            let mut window: Vec<meridian_changes::Change> = Vec::new();
            for row in tx
                .query(
                    "SELECT sequence, epoch, origin, recorded_at_ns, data_type, body, digest FROM lake_row
                      WHERE dataset = $1 AND sequence > $2 AND sequence <= $3 ORDER BY sequence",
                    &[&partition, &(from as i64), &until],
                )
                .map_err(|e| failed(unavailable(e)))?
            {
                let origin: Option<String> = row.get(2);
                let kind = DataType::from_code(row.get(4)).map_or("row", DataType::name);
                window.push(meridian_changes::Change {
                    partition: partition.to_string(),
                    sequence: row.get::<_, i64>(0) as u64,
                    epoch: row.get::<_, i64>(1) as u64,
                    origin: origin.unwrap_or_else(|| own_origin.clone()),
                    committed_at_ns: row.get(3),
                    kind: kind.to_string(),
                    body: row.get(5),
                    digest: row.get::<_, Option<Vec<u8>>>(6).unwrap_or_default(),
                });
            }
            for row in tx
                .query(
                    "SELECT first_sequence, last_sequence, subjects, fields, readers, at_ns, epoch,
                            origin
                       FROM lake_served
                      WHERE dataset = $1 AND last_sequence > $2 AND last_sequence <= $3",
                    &[&partition, &(from as i64), &until],
                )
                .map_err(|e| failed(unavailable(e)))?
            {
                let served = Served {
                    dataset: partition.to_string(),
                    first_sequence: row.get::<_, i64>(0) as u64,
                    last_sequence: row.get::<_, i64>(1) as u64,
                    subjects: row.get(2),
                    fields: row.get(3),
                    readers: row.get(4),
                    at_ns: row.get(5),
                };
                let origin: Option<String> = row.get(7);
                window.push(crate::store::served_change(
                    &served,
                    row.get::<_, i64>(6) as u64,
                    &origin.unwrap_or_else(|| own_origin.clone()),
                ));
            }
            window.sort_by_key(|change| change.sequence);
            from = until as u64;
            for change in window {
                if !page.take(change) {
                    full = true;
                    break;
                }
            }
            full = full || page.is_full();
        }
        let _ = tx.commit();
        Ok(ReadChangesReply {
            changes: page.changes,
            head,
            lowest_kept,
            epoch,
            after_sequence: 0,
        })
    }
}

/// [`PostgresStore::scrub`]'s reading, its errors Postgres's own so a page
/// failing its checksum is told apart.
fn scrub_rows(
    conn: &mut impl postgres::GenericClient,
) -> std::result::Result<meridian_changes::integrity::Scrubbed, postgres::Error> {
    let mut filled = 0u64;
    loop {
        let done = conn.execute(
            "UPDATE lake_row SET digest = sha256(body)
                      WHERE (dataset, sequence) IN (
                          SELECT dataset, sequence FROM lake_row WHERE digest IS NULL LIMIT 2000)",
            &[],
        )?;
        filled += done;
        if done == 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let checked = conn
        .query_one("SELECT count(*) FROM lake_row", &[])?
        .get::<_, i64>(0) as u64;
    let findings = conn
        .query(
            "SELECT dataset, min(sequence), min(recorded_at_ns) FROM lake_row
                  WHERE digest <> sha256(body) GROUP BY dataset ORDER BY dataset",
            &[],
        )?
        .iter()
        .map(|row| meridian_changes::integrity::Finding {
            partition: row.get(0),
            sequence: row.get::<_, i64>(1) as u64,
            committed_at_ns: row.get(2),
            what: "a row does not match its digest: the record changed beneath the lake"
                .to_string(),
        })
        .collect();
    Ok(meridian_changes::integrity::Scrubbed {
        checked,
        filled,
        findings,
    })
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
    let published = backfill_published(conn)?;
    if published > 0 {
        tracing::info!(published, "rate rows given their published time");
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
