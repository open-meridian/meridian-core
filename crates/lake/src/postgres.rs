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

use crate::memory::{as_of, number, number_parts, served_of};
use crate::migrations;
use crate::row::{DataType, Observation};
use crate::store::{
    as_assessed, as_flagged, closed_reason, listed_of, Assessment, ClosedDayAmendment, Listed,
    Query, Recorded, Removal, Result, Served, Stale, Store, StoreError, WantChange, WantChangeKind,
    When, WriteAheadBatch, WriteAheadPosition,
};

type Pool = r2d2::Pool<PostgresConnectionManager<meridian_changes::tls::DatabaseTls>>;
type Connection =
    r2d2::PooledConnection<PostgresConnectionManager<meridian_changes::tls::DatabaseTls>>;

thread_local! {
    /// Whether a read on this thread is to say the heads of its own snapshot
    /// (contract v30, stage C; the resilience spec's section 5): a record
    /// read on a read replica, stamped from the snapshot it answered from.
    static STAMPING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The heads its snapshots read, the lowest of each where it took more
    /// than one.
    static SNAPSHOT_HEADS: std::cell::RefCell<Option<BTreeMap<String, u64>>> =
        const { std::cell::RefCell::new(None) };
}

/// The next reads on this thread say the heads of the snapshot each answers
/// from ([`snapshot_heads`]): a read replica's record read (contract v30,
/// stage C; the resilience spec's section 5). Its watermark is then exact,
/// with no write position mapped: the head rows replicate with the rows.
pub fn stamp_from_snapshot() {
    STAMPING.with(|stamping| stamping.set(true));
    SNAPSHOT_HEADS.with(|heads| heads.borrow_mut().take());
}

/// The heads the reads since [`stamp_from_snapshot`] answered from, each the
/// lowest any of them read; none where none read any. Stops the stamping.
pub fn snapshot_heads() -> Option<BTreeMap<String, u64>> {
    STAMPING.with(|stamping| stamping.set(false));
    SNAPSHOT_HEADS.with(|heads| heads.borrow_mut().take())
}

/// In a read's repeatable-read transaction, where its thread asked: the
/// head row of each of `datasets` from the same snapshot the read answers
/// from, kept for [`snapshot_heads`].
fn note_snapshot(tx: &mut Transaction<'_>, datasets: &[String]) -> Result<()> {
    if !STAMPING.with(std::cell::Cell::get) {
        return Ok(());
    }
    let rows = tx
        .query(
            "SELECT dataset, head FROM lake_partition WHERE dataset = ANY($1)",
            &[&datasets],
        )
        .map_err(unavailable)?;
    SNAPSHOT_HEADS.with(|heads| {
        let mut heads = heads.borrow_mut();
        let heads = heads.get_or_insert_with(BTreeMap::new);
        for row in rows {
            let dataset: String = row.get(0);
            let head = row.get::<_, i64>(1) as u64;
            let at = heads.entry(dataset).or_insert(head);
            *at = (*at).min(head);
        }
    });
    Ok(())
}

/// Names the schema lock: distinct from every other store's. `migrate`
/// holds it, its backfills included, so whatever else takes it waits for a
/// migrate under way to finish and keeps the next from starting.
pub const SCHEMA_LOCK: i64 = 0x6c61_6b65_0000_0001_u64 as i64;

pub struct PostgresStore {
    pool: Pool,
    /// The epoch this writer's batches commit under (step 3a).
    fence: meridian_changes::Fence,
    /// Its database's identity as its connections found it (contract v30,
    /// stage C): a new timeline raises the epoch before a batch there.
    watch: std::sync::Arc<meridian_changes::timeline::Watch>,
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
        let watch = std::sync::Arc::new(meridian_changes::timeline::Watch::default());
        let pool = r2d2::Pool::builder()
            .max_size(pool_size.max(1))
            .connection_customizer(Box::new(meridian_changes::timeline::Watching(
                std::sync::Arc::clone(&watch),
            )))
            .build(manager)
            .map_err(unavailable)?;
        Ok(Self {
            pool,
            fence: meridian_changes::Fence::new(),
            watch,
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
            log_finding,
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
        let mut conn = self.pool.get().map_err(unavailable)?;
        // A connection opened on a new database timeline: the epoch raised
        // before this one commits anything there (contract v30, stage C).
        if self.watch.moved() {
            if let Err(failed) = self.follow_on(&mut conn) {
                self.watch.unsettled();
                return Err(failed);
            }
        }
        Ok(conn)
    }

    /// Follow the database's timeline on `conn` for this writer (contract
    /// v30, stage C): on a new one the epoch raised as a takeover raises it,
    /// adopted, and the takeover recorded with that cause. Nothing where it
    /// writes under no epoch.
    fn follow_on(&self, conn: &mut Connection) -> Result<meridian_changes::timeline::Timeline> {
        use meridian_changes::timeline::Timeline;
        let _alone = self.watch.alone();
        let mine = self.fence.epoch();
        if mine == 0 {
            return Ok(Timeline::Same);
        }
        let by = meridian_changes::TookOver {
            member: meridian_changes::member().to_string(),
            previous: String::new(),
            why: meridian_changes::timeline::DATABASE_TIMELINE.to_string(),
            at_ns: meridian_changes::now_ns(),
        };
        let mut tx = conn.transaction().map_err(unavailable)?;
        let (found, identity) = meridian_changes::timeline::follow(
            &mut tx,
            "lake_change_head",
            meridian_changes::CONTROL,
            mine,
            |tx| raise(tx, &by),
        )
        .map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        if let Timeline::Moved(epoch) = found {
            self.fence.adopt(epoch);
            tracing::warn!(
                epoch,
                identity,
                "the lake's database is on a new timeline: the epoch is raised, and followers past it reload"
            );
        }
        self.watch.settled(&identity);
        Ok(found)
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

    /// Each gap's state kept, one row a change (W10.14; decisions/031).
    fn gaps_kept(tx: &mut Transaction<'_>, gaps: &[meridian_domain::v1::DatasetGap]) -> Result<()> {
        for gap in gaps {
            tx.execute(
                "INSERT INTO lake_dataset_gap (gap_id, dataset, state, recorded_at_ns, body)
                 VALUES ($1, $2, $3, $4, $5)",
                &[
                    &gap.gap_id,
                    &gap.dataset,
                    &(gap.state as i16),
                    &gap.recorded_at_ns,
                    &gap.encode_to_vec(),
                ],
            )
            .map_err(unavailable)?;
        }
        Ok(())
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

/// Each row key's latest version held, for a whole batch in one look
/// (ruled 2026-11-10): one probe of `lake_row_version` a key, inside one
/// statement, rather than one round trip a key.
fn latest_of_each(
    client: &mut impl GenericClient,
    dataset: &str,
    rows: &[Observation],
) -> Result<BTreeMap<(i16, String), Observation>> {
    let mut asked: BTreeMap<(i16, String), DataType> = BTreeMap::new();
    for row in rows {
        asked.insert(
            (row.data_type().code(), row.meta().row_key.clone()),
            row.data_type(),
        );
    }
    let (types, keys): (Vec<i16>, Vec<String>) = asked.keys().cloned().unzip();
    let found = client
        .query(
            "SELECT k.data_type, k.row_key, r.body
               FROM unnest($2::smallint[], $3::text[]) AS k(data_type, row_key)
              CROSS JOIN LATERAL (
                    SELECT body FROM lake_row
                     WHERE dataset = $1 AND data_type = k.data_type AND row_key = k.row_key
                     ORDER BY version DESC LIMIT 1) AS r",
            &[&dataset, &types, &keys],
        )
        .map_err(unavailable)?;
    let mut latest = BTreeMap::new();
    for row in found {
        let key = (row.get::<_, i16>(0), row.get::<_, String>(1));
        let Some(data_type) = asked.get(&key).copied() else {
            continue;
        };
        let body: &[u8] = row.get(2);
        latest.insert(
            key,
            Observation::decode(data_type, body).map_err(unreadable)?,
        );
    }
    Ok(latest)
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

/// Each subject's last sequence in the dataset, for a whole batch in one
/// look (ruled 2026-11-10), by `lake_row_subject_sequence` (migration 13):
/// the top of each subject's own entries, one probe a subject. The subject
/// is named as that index keys it, `subject || ''`, which no other read
/// names, so a read of the latest is never planned on it.
fn previous_of_each(
    client: &mut impl GenericClient,
    dataset: &str,
    rows: &[Observation],
) -> Result<BTreeMap<(i16, String), u64>> {
    let asked: std::collections::BTreeSet<(i16, String)> = rows
        .iter()
        .map(|row| (row.data_type().code(), row.subject().to_string()))
        .collect();
    let (types, subjects): (Vec<i16>, Vec<String>) = asked.into_iter().unzip();
    let found = client
        .query(
            "SELECT k.data_type, k.subject,
                    (SELECT max(sequence) FROM lake_row
                      WHERE dataset = $1 AND data_type = k.data_type
                        AND (subject || '') = k.subject)
               FROM unnest($2::smallint[], $3::text[]) AS k(data_type, subject)",
            &[&dataset, &types, &subjects],
        )
        .map_err(unavailable)?;
    Ok(found
        .iter()
        .filter_map(|row| {
            let last: Option<i64> = row.get(2);
            last.map(|last| ((row.get(0), row.get(1)), last as u64))
        })
        .collect())
}

/// The batch's rows written in one COPY (ruled 2026-11-10), rather than one
/// INSERT a row: what each row keeps is unchanged. COPY's binary rows are
/// sent a few KiB at a time, so no connection keeps a buffer the size of a
/// batch, as a multi-row INSERT's one message would have it keep.
fn insert_all(
    tx: &mut Transaction<'_>,
    dataset: &str,
    rows: &[Observation],
    epoch: u64,
) -> Result<()> {
    use postgres::binary_copy::BinaryCopyInWriter;
    use postgres::types::Type;

    if rows.is_empty() {
        return Ok(());
    }
    let sink = tx
        .copy_in(
            "COPY lake_row
                  (dataset, sequence, data_type, row_key, version, subject, subjects, kind,
                   interval_ns, venue_id, valid_from_ns, valid_until_ns, business_date,
                   recorded_at_ns, previous_sequence, unconverted, body, closed_reason,
                   latest_key, quality, expires, listed_type, published_at_ns, epoch, digest)
             FROM STDIN (FORMAT binary)",
        )
        .map_err(unavailable)?;
    let types = [
        Type::TEXT,
        Type::INT8,
        Type::INT2,
        Type::TEXT,
        Type::INT8,
        Type::TEXT,
        Type::TEXT_ARRAY,
        Type::INT2,
        Type::INT8,
        Type::TEXT,
        Type::INT8,
        Type::INT8,
        Type::DATE,
        Type::INT8,
        Type::INT8,
        Type::BOOL,
        Type::BYTEA,
        Type::INT2,
        Type::TEXT,
        Type::INT2,
        Type::DATE,
        Type::INT2,
        Type::INT8,
        Type::INT8,
        Type::BYTEA,
    ];
    let mut writer = BinaryCopyInWriter::new(sink, &types);
    let epoch = epoch as i64;
    for row in rows {
        let meta = row.meta();
        let date = pg_date(&meta.business_date)?;
        // A series row's expiry and listed type (migration 7); none for any
        // other.
        let listing = row.listing();
        let expires = match listing.as_ref().and_then(|(expires, _)| expires.as_deref()) {
            Some(text) => pg_date(text)?,
            None => None,
        };
        let listed_type: Option<i16> = listing.map(|(_, kind)| kind as i16);
        // Its digest, kept beside it (step 3b, integrity at rest).
        let body = row.encode();
        let digest = meridian_changes::integrity::digest(&body);
        writer
            .write(&[
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
                &epoch,
                &digest,
            ])
            .map_err(unavailable)?;
    }
    writer.finish().map_err(unavailable)?;
    Ok(())
}

/// A plain date (YYYY-MM-DD) as COPY's binary DATE takes it; none where the
/// text is empty. A date that does not read is refused, as Postgres refused
/// it read from its text before.
fn pg_date(text: &str) -> Result<Option<PgDate>> {
    if text.is_empty() {
        return Ok(None);
    }
    let date = Date::parse(text).map_err(|_| {
        StoreError::Unavailable(format!(
            "db error: ERROR: invalid input syntax for type date: {text:?}"
        ))
    })?;
    // Postgres counts a DATE's days from 2000-01-01, 10,957 days after 1970's.
    Ok(Some(PgDate((date.days_since_epoch() - 10_957) as i32)))
}

/// A DATE in Postgres's binary form: days since 2000-01-01.
#[derive(Debug, Clone, Copy)]
struct PgDate(i32);

impl postgres::types::ToSql for PgDate {
    fn to_sql(
        &self,
        _: &postgres::types::Type,
        out: &mut postgres::types::private::BytesMut,
    ) -> std::result::Result<postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>>
    {
        out.extend_from_slice(&self.0.to_be_bytes());
        Ok(postgres::types::IsNull::No)
    }

    postgres::types::accepts!(DATE);
    postgres::types::to_sql_checked!();
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

/// The same refusal again, for each part of a batch.
fn again(failed: &StoreError) -> StoreError {
    match failed {
        StoreError::Fenced(fenced) => StoreError::Fenced(*fenced),
        StoreError::PositionMoved(words) => StoreError::PositionMoved(words.clone()),
        StoreError::SchemaAhead(words) => StoreError::SchemaAhead(words.clone()),
        StoreError::Unavailable(words) => StoreError::Unavailable(words.clone()),
    }
}

impl PostgresStore {
    /// Batches of one dataset recorded in one transaction (step 3d, group
    /// commit; section 6.5), numbered in turn as one, each answered with what
    /// it did: the epoch checked once, every row's latest version and every
    /// subject's last sequence read in one look each, and the rows written
    /// in one INSERT (ruled 2026-11-10).
    fn record_together(
        &self,
        dataset: &str,
        parts: Vec<Vec<Observation>>,
        now_ns: i64,
    ) -> Result<Vec<Recorded>> {
        // Adopted before a connection is taken: one store never waits on
        // itself for a second (step 3a).
        self.epoch_now()?;
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        let mut head = self.head(&mut tx, dataset)?;
        let epoch = self.fence.epoch();
        // Read what each row is decided against first, then number: the
        // closures cannot borrow the transaction the inserts need.
        let every: Vec<Observation> = parts.iter().flatten().cloned().collect();
        let latest = latest_of_each(&mut tx, dataset, &every)?;
        let previous = previous_of_each(&mut tx, dataset, &every)?;
        drop(every);
        let done = number_parts(
            parts,
            &mut head,
            now_ns,
            |row| {
                latest
                    .get(&(row.data_type().code(), row.meta().row_key.clone()))
                    .cloned()
            },
            |row| {
                previous
                    .get(&(row.data_type().code(), row.subject().to_string()))
                    .copied()
                    .unwrap_or(0)
            },
        );
        let numbered: Vec<Observation> = done
            .iter()
            .flat_map(|part| part.rows.iter().cloned())
            .collect();
        insert_all(&mut tx, dataset, &numbered, epoch)?;
        // Each row numbered version 1 is a key the partition held no
        // version of; every other replaces its key's last.
        let new_keys: u64 = done.iter().map(|part| u64::from(part.recorded)).sum();
        Self::move_head(&mut tx, dataset, head, new_keys)?;
        tx.commit().map_err(unavailable)?;
        Ok(done)
    }
}

impl Store for PostgresStore {
    fn record(&self, dataset: &str, rows: Vec<Observation>, now_ns: i64) -> Result<Recorded> {
        Ok(self
            .record_together(dataset, vec![rows], now_ns)?
            .pop()
            .unwrap_or_default())
    }

    fn record_parts(
        &self,
        dataset: &str,
        parts: Vec<Vec<Observation>>,
        now_ns: i64,
    ) -> Vec<Result<Recorded>> {
        if parts.len() < 2 {
            return parts
                .into_iter()
                .map(|rows| self.record(dataset, rows, now_ns))
                .collect();
        }
        // Kept to be tried again alone: a batch is at most 512 KiB.
        let each = parts.clone();
        match self.record_together(dataset, parts, now_ns) {
            Ok(done) => done.into_iter().map(Ok).collect(),
            // An unexpected database error: each part tried again alone
            // (section 6.5), so it is refused only for the part it was. A
            // part that did commit is decided unchanged the second time.
            Err(StoreError::Unavailable(failed)) => {
                tracing::warn!(
                    dataset,
                    parts = each.len(),
                    failed,
                    "a batch's transaction failed: each of its parts is tried alone"
                );
                each.into_iter()
                    .map(|rows| self.record(dataset, rows, now_ns))
                    .collect()
            }
            Err(failed) => each.iter().map(|_| Err(again(&failed))).collect(),
        }
    }

    fn write_ahead_position(&self, stream: &str) -> Result<Option<WriteAheadPosition>> {
        let mut conn = self.conn()?;
        Ok(conn
            .query_opt(
                "SELECT sequence, at_ns FROM lake_write_ahead WHERE stream = $1",
                &[&stream],
            )
            .map_err(unavailable)?
            .map(|row| WriteAheadPosition {
                sequence: row.get::<_, i64>(0) as u64,
                at_ns: row.get(1),
            }))
    }

    fn record_write_ahead(&self, batch: WriteAheadBatch, now_ns: i64) -> Result<Vec<Recorded>> {
        self.epoch_now()?;
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        // The stream's batch is written whatever its size, and waits on its
        // position's lock: without the serving session's statement_timeout
        // (the resilience spec's section 10a).
        meridian_changes::serving::unbounded(&mut tx).map_err(unavailable)?;
        // The position first, under its lock: a batch that does not follow
        // the position the store holds is refused, never recorded past a
        // gap -- after a failover lost the commits since the one it holds,
        // the writer goes back to it, so nothing it acknowledged is lost.
        let at = tx
            .query_opt(
                "SELECT sequence, at_ns FROM lake_write_ahead WHERE stream = $1 FOR UPDATE",
                &[&batch.stream],
            )
            .map_err(unavailable)?
            .map(|row| WriteAheadPosition {
                sequence: row.get::<_, i64>(0) as u64,
                at_ns: row.get(1),
            });
        if at != batch.after {
            return Err(StoreError::PositionMoved(format!(
                "the lake's place in {} is {at:?}, not {:?}",
                batch.stream, batch.after
            )));
        }
        let epoch = self.fence.epoch();
        let mut answered = vec![Recorded::default(); batch.parts.len()];
        // Each dataset's parts as one batch of it, the datasets taken in
        // their names' order: every batch takes the heads' locks in one order.
        let mut by_dataset: BTreeMap<String, Vec<(usize, Vec<Observation>)>> = BTreeMap::new();
        for (place, (dataset, rows)) in batch.parts.into_iter().enumerate() {
            by_dataset.entry(dataset).or_default().push((place, rows));
        }
        for (dataset, parts) in by_dataset {
            let mut head = self.head(&mut tx, &dataset)?;
            let (places, rows): (Vec<usize>, Vec<Vec<Observation>>) = parts.into_iter().unzip();
            let every: Vec<Observation> = rows.iter().flatten().cloned().collect();
            let latest = latest_of_each(&mut tx, &dataset, &every)?;
            let previous = previous_of_each(&mut tx, &dataset, &every)?;
            drop(every);
            let done = number_parts(
                rows,
                &mut head,
                now_ns,
                |row| {
                    latest
                        .get(&(row.data_type().code(), row.meta().row_key.clone()))
                        .cloned()
                },
                |row| {
                    previous
                        .get(&(row.data_type().code(), row.subject().to_string()))
                        .copied()
                        .unwrap_or(0)
                },
            );
            let numbered: Vec<Observation> = done
                .iter()
                .flat_map(|part| part.rows.iter().cloned())
                .collect();
            insert_all(&mut tx, &dataset, &numbered, epoch)?;
            let new_keys: u64 = done.iter().map(|part| u64::from(part.recorded)).sum();
            Self::move_head(&mut tx, &dataset, head, new_keys)?;
            for (place, part) in places.into_iter().zip(done) {
                answered[place] = part;
            }
        }
        // What the batch found the stream dropped, each dataset's latest
        // valid time it recorded, and each gap it found: with its rows and
        // its position, or not at all (W10.14).
        for dropped in &batch.dropped {
            tx.execute(
                "INSERT INTO lake_write_ahead_dropped
                        (stream, first_sequence, last_sequence, after_at_ns, until_at_ns, noted_at_ns,
                         gap_id)
                 VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT DO NOTHING",
                &[
                    &batch.stream,
                    &(dropped.first as i64),
                    &(dropped.last as i64),
                    &dropped.after_at_ns,
                    &dropped.until_at_ns,
                    &now_ns,
                    &dropped.gap_id,
                ],
            )
            .map_err(unavailable)?;
        }
        for (dataset, valid_ns) in &batch.valid {
            tx.execute(
                "INSERT INTO lake_write_ahead_dataset (stream, dataset, valid_ns) VALUES ($1, $2, $3)
                 ON CONFLICT (stream, dataset) DO UPDATE
                    SET valid_ns = greatest(lake_write_ahead_dataset.valid_ns, EXCLUDED.valid_ns)",
                &[&batch.stream, dataset, valid_ns],
            )
            .map_err(unavailable)?;
        }
        Self::gaps_kept(&mut tx, &batch.gaps)?;
        let logged: Vec<(&str, Vec<u8>, i64)> = batch
            .gaps
            .iter()
            .map(|gap| {
                (
                    crate::store::CONTROL_GAP,
                    gap.encode_to_vec(),
                    gap.recorded_at_ns,
                )
            })
            .collect();
        self.log_control(&mut tx, &logged)?;
        tx.execute(
            "INSERT INTO lake_write_ahead (stream, sequence, at_ns, committed_at_ns)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (stream) DO UPDATE
                SET sequence = EXCLUDED.sequence, at_ns = EXCLUDED.at_ns,
                    committed_at_ns = EXCLUDED.committed_at_ns",
            &[
                &batch.stream,
                &(batch.through.sequence as i64),
                &batch.through.at_ns,
                &now_ns,
            ],
        )
        .map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        Ok(answered)
    }

    fn write_ahead_found(
        &self,
        stream: &str,
    ) -> Result<(Vec<crate::store::Dropped>, BTreeMap<String, i64>)> {
        let mut conn = self.conn()?;
        let dropped = conn
            .query(
                "SELECT first_sequence, last_sequence, after_at_ns, until_at_ns, gap_id
                   FROM lake_write_ahead_dropped WHERE stream = $1 ORDER BY first_sequence",
                &[&stream],
            )
            .map_err(unavailable)?
            .iter()
            .map(|row| crate::store::Dropped {
                first: row.get::<_, i64>(0) as u64,
                last: row.get::<_, i64>(1) as u64,
                after_at_ns: row.get(2),
                until_at_ns: row.get(3),
                gap_id: row.get(4),
            })
            .collect();
        let valid = conn
            .query(
                "SELECT dataset, valid_ns FROM lake_write_ahead_dataset WHERE stream = $1",
                &[&stream],
            )
            .map_err(unavailable)?
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();
        Ok((dropped, valid))
    }

    fn record_gap(&self, gap: &meridian_domain::v1::DatasetGap) -> Result<()> {
        self.epoch_now()?;
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        Self::gaps_kept(&mut tx, std::slice::from_ref(gap))?;
        self.log_control(
            &mut tx,
            &[(
                crate::store::CONTROL_GAP,
                gap.encode_to_vec(),
                gap.recorded_at_ns,
            )],
        )?;
        tx.commit().map_err(unavailable)
    }

    fn record_switches(&self, switches: &[meridian_domain::v1::SourceSwitched]) -> Result<()> {
        if switches.is_empty() {
            return Ok(());
        }
        self.epoch_now()?;
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        let logged: Vec<(&str, Vec<u8>, i64)> = switches
            .iter()
            .map(|switch| {
                (
                    crate::store::CONTROL_SWITCH,
                    switch.encode_to_vec(),
                    switch.switched_at_ns,
                )
            })
            .collect();
        self.log_control(&mut tx, &logged)?;
        tx.commit().map_err(unavailable)
    }

    fn source_switches(&self) -> Result<Vec<meridian_domain::v1::SourceSwitched>> {
        self.conn()?
            .query(
                "SELECT body FROM lake_control_change WHERE kind = $1 ORDER BY sequence",
                &[&crate::store::CONTROL_SWITCH],
            )
            .map_err(unavailable)?
            .iter()
            .map(|row| {
                let body: Vec<u8> = row.get(0);
                meridian_domain::v1::SourceSwitched::decode(&body[..]).map_err(unreadable)
            })
            .collect()
    }

    fn dataset_gaps(&self) -> Result<Vec<meridian_domain::v1::DatasetGap>> {
        self.conn()?
            .query("SELECT body FROM lake_dataset_gap ORDER BY change_id", &[])
            .map_err(unavailable)?
            .iter()
            .map(|row| {
                let body: Vec<u8> = row.get(0);
                meridian_domain::v1::DatasetGap::decode(&body[..]).map_err(unreadable)
            })
            .collect()
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
        // Its heads from this snapshot, where its thread asked (stage C).
        note_snapshot(&mut tx, &query.datasets)?;
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
        // Its heads from this snapshot, where its thread asked (stage C).
        note_snapshot(
            &mut tx,
            &places.iter().map(|(d, _)| d.clone()).collect::<Vec<_>>(),
        )?;
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
        // Its heads from this snapshot, where its thread asked (stage C).
        note_snapshot(&mut tx, datasets)?;
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
        // A dataset's rows before a time, however many: without the
        // serving session's statement_timeout (section 10a).
        meridian_changes::serving::unbounded(&mut tx).map_err(unavailable)?;
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
        let epoch = raise(&mut tx, by).map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        self.fence.adopt(epoch);
        Ok(epoch)
    }

    fn follow_timeline(&self) -> Result<meridian_changes::timeline::Timeline> {
        let mut conn = self.pool.get().map_err(unavailable)?;
        self.follow_on(&mut conn)
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

/// Raise the epoch of the `control` partition and of every dataset's head
/// in `tx`, and record who raised it and why (step 3b, `WriterTookOver`):
/// answer the epoch. What a takeover does, and a writer on a new database
/// timeline (contract v30, stage C).
fn raise(
    tx: &mut Transaction<'_>,
    by: &meridian_changes::TookOver,
) -> std::result::Result<u64, postgres::Error> {
    // The control partition first: a dataset a deposed writer is beginning
    // holds it shared until it commits, and is then raised with the rest.
    let control: i64 = tx
        .query_one(
            "UPDATE lake_change_head SET epoch = epoch + 1 WHERE partition = 'control'
             RETURNING epoch",
            &[],
        )?
        .get(0);
    let highest: Option<i64> = tx
        .query_one("SELECT max(epoch) FROM lake_partition", &[])?
        .get(0);
    let epoch = control.max(highest.unwrap_or(0) + 1);
    tx.execute(
        "UPDATE lake_change_head SET epoch = $1 WHERE partition = 'control'",
        &[&epoch],
    )?;
    tx.execute("UPDATE lake_partition SET epoch = $1", &[&epoch])?;
    // Who raised it and why, in the same transaction (step 3b).
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
    )?;
    // And in the control partition, in the same transaction (contract v30,
    // stage D): the record the conductor reads.
    let partitions: Vec<String> = tx
        .query_one(
            "SELECT array_append(coalesce(array_agg(dataset ORDER BY dataset), '{}'), 'control')
               FROM lake_partition",
            &[],
        )?
        .get(0);
    log_record(
        tx,
        &meridian_changes::records::Record::TookOver(meridian_changes::records::took_over(
            "lake",
            by,
            partitions,
            epoch as u64,
        )),
        by.at_ns,
    )?;
    Ok(epoch as u64)
}

/// A finding of the lake's, kept for the first time, appended to its
/// `control` partition in `tx` as its `IntegrityFinding` (contract v30,
/// stage D): the scrub's, and the replica scrub Job's.
pub fn log_finding(
    tx: &mut Transaction<'_>,
    finding: &meridian_changes::integrity::Finding,
    found_at_ns: i64,
) -> std::result::Result<(), postgres::Error> {
    log_record(
        tx,
        &meridian_changes::records::Record::Finding(meridian_changes::records::finding(
            "lake",
            meridian_domain::v1::IntegrityCheck::RowDigest,
            finding,
            found_at_ns,
        )),
        found_at_ns,
    )
}

/// Append one of the lake's own records to its `control` partition in `tx`
/// (contract v30, stage D; [`meridian_changes::records`]): the next
/// sequence from the control head row, under that row's lock and its epoch
/// now, its body the record's protobuf, as the lake's other control kinds.
/// Its followers pass over the kind; the conductor reads it.
fn log_record(
    tx: &mut Transaction<'_>,
    record: &meridian_changes::records::Record,
    at_ns: i64,
) -> std::result::Result<(), postgres::Error> {
    let (kind, body) = meridian_changes::records::to_bytes(record);
    tx.execute(
        "WITH next AS (
            UPDATE lake_change_head SET sequence = sequence + 1
             WHERE partition = 'control' RETURNING sequence, epoch)
         INSERT INTO lake_control_change (sequence, epoch, committed_at_ns, kind, body)
         SELECT next.sequence, next.epoch, $1, $2, $3 FROM next",
        &[&at_ns, &kind, &body],
    )?;
    Ok(())
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

fn unavailable(failed: impl std::error::Error + 'static) -> StoreError {
    // With each cause, because this driver's own message for most failures
    // is the words "db error" and everything useful is one level down; and
    // its SQLSTATE, so a query cancelled at the serving session's
    // statement_timeout is answered with this component's own refusal for
    // load (the resilience spec's section 10a).
    StoreError::Unavailable(meridian_changes::serving::described(&failed))
}
