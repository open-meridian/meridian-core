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

use meridian_domain::v1::{EntitlementsChangedEvent, ObservationsWantedEvent, SourcePriority};

use crate::memory::{as_of, number, served_of};
use crate::migrations;
use crate::row::Observation;
use crate::store::{
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

    fn move_head(tx: &mut Transaction<'_>, dataset: &str, head: u64) -> Result<()> {
        tx.execute(
            "UPDATE lake_partition SET head = $2 WHERE dataset = $1",
            &[&dataset, &(head as i64)],
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
    tx.execute(
        "INSERT INTO lake_row
                (dataset, sequence, data_type, row_key, version, subject, subjects, kind,
                 interval_ns, venue_id, valid_from_ns, valid_until_ns, business_date,
                 recorded_at_ns, previous_sequence, unconverted, body)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13::text::date, $14, $15,
                 $16, $17)",
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
        ],
    )
    .map_err(unavailable)?;
    Ok(())
}

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
        Self::move_head(&mut tx, dataset, head)?;
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
        Self::move_head(&mut tx, dataset, head)?;
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
        };
        let rows = tx
            .query(
                "SELECT body FROM lake_row
                  WHERE dataset = ANY($1) AND subjects && $2 AND data_type = $3
                    AND (cardinality($4::smallint[]) = 0 OR kind = ANY($4))
                    AND ($5 = 0 OR interval_ns = $5)
                    AND recorded_at_ns <= $6
                    AND ($7::text IS NULL OR business_date = $7::text::date)
                    AND valid_from_ns >= $8 AND valid_from_ns < $9 AND valid_from_ns <= $10
                  ORDER BY dataset, sequence",
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
        tx.commit().map_err(unavailable)?;
        Ok(as_of(out, query.as_of_ns))
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
        let removed = tx
            .execute(
                "DELETE FROM lake_row WHERE dataset = $1 AND recorded_at_ns < $2",
                &[&dataset, &recorded_before_ns],
            )
            .map_err(unavailable)?;
        if removed > 0 {
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
