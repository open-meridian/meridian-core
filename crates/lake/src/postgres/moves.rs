//! A lake range moved to another writer group, as its database holds it
//! (step 3d; spec/a-deployment-scales-out-and-fails-over-by-configuration
//! section 6.8; `writer_group` ruled 2026-11-14): each range's tip, its
//! switch under its row's lock, and each member's view of the ranges'
//! groups. The warm, the gate and the records are [`crate::moves`]'.

use super::*;

/// Where a range stands in one database, as a move reads it: its head, the
/// last row of its own this database holds (its sequence, digest and
/// recording time), the group its row names and its epoch.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RangeTip {
    pub head: u64,
    pub epoch: u64,
    pub writer_group: Option<u32>,
    pub closed: bool,
    pub held: u64,
    pub held_digest: Vec<u8>,
    pub held_at_ns: i64,
}

/// How a range's switch ended (section 6.8).
#[derive(Debug, Clone, PartialEq)]
pub enum Switched {
    /// The target owns the range from `closing` on, under `epoch`.
    Moved { closing: u64, epoch: u64 },
    /// The target's digest at the source's close differs: the source owns.
    DigestDiffers {
        closing: u64,
        source: Vec<u8>,
        target: Vec<u8>,
        at_ns: i64,
    },
    /// The switch did not finish within its bound: the source owns.
    TimedOut,
}

fn range_tip_of(client: &mut impl GenericClient, partition: &str) -> Result<Option<RangeTip>> {
    let Some(row) = client
        .query_opt(
            "SELECT head, epoch, writer_group, closed_at FROM lake_partition WHERE dataset = $1",
            &[&partition],
        )
        .map_err(unavailable)?
    else {
        return Ok(None);
    };
    let (held, held_digest, held_at_ns) = held_of(client, partition)?;
    Ok(Some(RangeTip {
        head: row.get::<_, i64>(0).max(0) as u64,
        epoch: row.get::<_, i64>(1) as u64,
        writer_group: row.get::<_, Option<i32>>(2).map(|group| group as u32),
        closed: row.get::<_, Option<i64>>(3).is_some(),
        held,
        held_digest,
        held_at_ns,
    }))
}

/// The last row of `partition`'s own this database holds: its sequence,
/// digest and recording time; zeros where it holds none. Led by the range,
/// then its dataset and sequence, which each month's `_seq` index answers.
fn held_of(client: &mut impl GenericClient, partition: &str) -> Result<(u64, Vec<u8>, i64)> {
    let held = client
        .query_opt(
            "SELECT sequence, digest, recorded_at_ns FROM lake_row
              WHERE partition = $1 AND dataset = $2
              ORDER BY sequence DESC LIMIT 1",
            &[&partition, &crate::range::dataset_of(partition)],
        )
        .map_err(unavailable)?;
    Ok(held.map_or((0, Vec::new(), 0), |row| {
        (
            row.get::<_, i64>(0).max(0) as u64,
            row.get::<_, Option<Vec<u8>>>(1).unwrap_or_default(),
            row.get(2),
        )
    }))
}

/// A row's anchor in SQL: its own, else -- a row kept before 3d -- the
/// first two bytes of SHA-256 over its first subject, as `range::hash`.
const ANCHOR_SQL: &str =
    "coalesce(anchor, ('x' || left(encode(sha256(convert_to(subject, 'UTF8')), 'hex'), 4))::bit(16)::int)";

/// The epoch writer group `group` commits under in this database: the
/// control partition's for group 0, its `lake_writer_group` row's past it
/// (kept while it owns nothing, so a range moved back carries the epoch its
/// writer holds). A group with no row yet -- its writer never started -- is
/// given one past every epoch, which its writer adopts at its start.
fn group_epoch(client: &mut impl GenericClient, group: u32) -> Result<u64> {
    let found = if group == 0 {
        client.query_opt(
            "SELECT epoch FROM lake_change_head WHERE partition = 'control'",
            &[],
        )
    } else {
        client.query_opt(
            "SELECT epoch FROM lake_writer_group WHERE writer_group = $1",
            &[&(group as i32)],
        )
    }
    .map_err(unavailable)?;
    if let Some(row) = found {
        return Ok(row.get::<_, i64>(0) as u64);
    }
    client
        .execute(
            "INSERT INTO lake_writer_group (writer_group, epoch, origin)
             SELECT $1, greatest(coalesce((SELECT max(epoch) FROM lake_change_head), 0),
                                 coalesce((SELECT max(epoch) FROM lake_partition), 0),
                                 coalesce((SELECT max(epoch) FROM lake_writer_group), 0)) + 1, $2
             ON CONFLICT (writer_group) DO NOTHING",
            &[&(group as i32), &meridian_changes::origin()],
        )
        .map_err(unavailable)?;
    Ok(client
        .query_one(
            "SELECT epoch FROM lake_writer_group WHERE writer_group = $1",
            &[&(group as i32)],
        )
        .map_err(unavailable)?
        .get::<_, i64>(0) as u64)
}

impl PostgresStore {
    /// `partition` (a dataset's full range or one split from it) as this
    /// database holds it; none where it has no row.
    pub fn range_tip(&self, partition: &str) -> Result<Option<RangeTip>> {
        let mut conn = self.conn()?;
        range_tip_of(&mut *conn, partition)
    }

    /// The last row of `partition`'s own this database holds: its sequence
    /// and recording time (a move's target, warming).
    pub fn range_tip_held(&self, partition: &str) -> Result<(u64, i64)> {
        let (held, _, at_ns) = held_of(&mut *self.conn()?, partition)?;
        Ok((held, at_ns))
    }

    /// Every range this database holds, open and closed, with the group
    /// its row names: what a move checks a request against, and what a
    /// changed count moves.
    pub fn ranges(&self) -> Result<Vec<(String, bool, Option<u32>)>> {
        Ok(self
            .conn()?
            .query(
                "SELECT dataset, closed_at IS NOT NULL, writer_group FROM lake_partition
                  ORDER BY dataset",
                &[],
            )
            .map_err(unavailable)?
            .iter()
            .map(|row| {
                (
                    row.get(0),
                    row.get(1),
                    row.get::<_, Option<i32>>(2).map(|group| group as u32),
                )
            })
            .collect())
    }

    /// Every range's `writer_group` this database names, read into the
    /// member's view (step 3d; ruled 2026-11-14): at start, on the sweep,
    /// and where a write found its view stale. Says how many were named.
    pub fn read_owners(&self) -> Result<usize> {
        if !self.groups.several() {
            return Ok(0);
        }
        let rows = self
            .conn()?
            .query(
                "SELECT dataset, writer_group FROM lake_partition WHERE writer_group IS NOT NULL",
                &[],
            )
            .map_err(unavailable)?;
        for row in &rows {
            let group: i32 = row.get(1);
            self.groups
                .assign(row.get::<_, &str>(0), Some(group as u32));
        }
        Ok(rows.len())
    }

    /// Each range whose `writer_group` is NULL given its dataset's
    /// rendezvous owner at the lake's count (step 3d; ruled 2026-11-14):
    /// what the control partition's group does at its start, so a later
    /// change of the count moves nothing until a move moves it. Says how
    /// many.
    pub fn pin_owners(&self) -> Result<u64> {
        let mut conn = self.conn()?;
        let unnamed: Vec<String> = conn
            .query(
                "SELECT dataset FROM lake_partition WHERE writer_group IS NULL",
                &[],
            )
            .map_err(unavailable)?
            .iter()
            .map(|row| row.get(0))
            .collect();
        let mut pinned = 0;
        for partition in unnamed {
            let group = meridian_changes::groups::owner(
                crate::range::dataset_of(&partition),
                self.groups.count(),
            ) as i32;
            pinned += conn
                .execute(
                    "UPDATE lake_partition SET writer_group = $2
                      WHERE dataset = $1 AND writer_group IS NULL",
                    &[&partition, &group],
                )
                .map_err(unavailable)?;
        }
        self.read_owners()?;
        Ok(pinned)
    }

    /// A move's switch of range `partition`, held by this database, to
    /// writer group `target` in `to`'s (step 3d; section 6.8, step 4):
    /// under the range's row, so the source's batch in flight finishes
    /// first and the next leaves the range's parts to wait (the hold) while
    /// its sibling ranges' commit; the source's close at its head N; the
    /// digests at N compared; then the row named the target's at its
    /// group's epoch, so a held part finds it there and is handed on, never
    /// fenced. Within `bound` of its start, else nothing changes. `differ`
    /// makes the digests differ (CI's MERIDIAN_MOVE_DIGEST_FILE).
    pub fn switch_range(
        &self,
        to: &PostgresStore,
        partition: &str,
        target: u32,
        bound: std::time::Duration,
        differ: bool,
    ) -> Result<Switched> {
        let began = std::time::Instant::now();
        let left = |began: std::time::Instant| bound.saturating_sub(began.elapsed());
        let same = std::ptr::eq(self, to);
        let timed_out = |failed: &postgres::Error| {
            matches!(
                failed.code(),
                Some(&postgres::error::SqlState::LOCK_NOT_AVAILABLE)
                    | Some(&postgres::error::SqlState::QUERY_CANCELED)
            )
        };
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        let bounded = left(began).as_millis().max(1).to_string();
        let taken = tx
            .execute(
                "SELECT set_config('lock_timeout', $1, true), set_config('statement_timeout', $1, true)",
                &[&bounded],
            )
            .and_then(|_| {
                tx.query_opt(
                    "SELECT closed_at FROM lake_partition WHERE dataset = $1 FOR UPDATE",
                    &[&partition],
                )
            });
        let row = match taken {
            Ok(row) => row,
            Err(failed) if timed_out(&failed) => return Ok(Switched::TimedOut),
            Err(failed) => return Err(unavailable(failed)),
        };
        match row {
            None => {
                return Err(StoreError::Unavailable(format!(
                    "the lake has no range {partition}"
                )))
            }
            Some(row) if row.get::<_, Option<i64>>(0).is_some() => {
                return Err(StoreError::Unavailable(format!(
                    "range {partition} was split: its children move each on its own"
                )))
            }
            Some(_) => {}
        }
        let source = range_tip_of(&mut tx, partition)?.unwrap_or_default();
        let closing = source.head;
        // The target at N: the same rows where it is this database; else
        // the range's rows copied up to N while the source is held.
        let held = if same {
            source.clone()
        } else {
            let copied = held_of(&mut *to.conn()?, partition)?.0;
            to.copy_range_from(self, partition, copied)?;
            let (held, held_digest, held_at_ns) = held_of(&mut *to.conn()?, partition)?;
            RangeTip {
                head: closing,
                held,
                held_digest,
                held_at_ns,
                ..source.clone()
            }
        };
        if left(began).is_zero() {
            return Ok(Switched::TimedOut);
        }
        let mut target_digest = held.held_digest.clone();
        if differ {
            target_digest.push(0xff);
        }
        if held.head != closing || held.held != source.held || target_digest != source.held_digest {
            return Ok(Switched::DigestDiffers {
                closing,
                source: source.held_digest,
                target: target_digest,
                at_ns: meridian_changes::now_ns(),
            });
        }
        let epoch = if same {
            group_epoch(&mut tx, target)?
        } else {
            // The target's row at the source's close, under its group's
            // epoch, named its own -- before the source lets go, so no
            // write finds neither owning.
            to.open_moved_in(self, partition, closing, target)?
        };
        let epoch_here = if same { epoch } else { source.epoch };
        if !same {
            // The latest versions in the range itself are the target's to
            // keep now: this database's let go, so a latest read here never
            // answers one the target has replaced. Its rows stay, read where
            // it went; a key last written before the split stays answered
            // from its parent here, and the reader keeps the higher version
            // where the target has restated it since.
            tx.execute(
                "DELETE FROM lake_latest WHERE dataset = $1 AND partition = $2",
                &[&crate::range::dataset_of(partition), &partition],
            )
            .map_err(unavailable)?;
        }
        let switched = tx
            .execute(
                "UPDATE lake_partition SET writer_group = $2, epoch = $3 WHERE dataset = $1",
                &[&partition, &(target as i32), &(epoch_here as i64)],
            )
            .and_then(|_| tx.commit());
        if switched.is_err() && !same {
            // The source still owns: the target's row named it again, so
            // neither writes the range twice.
            let from = source.writer_group.unwrap_or_else(|| {
                self.groups
                    .owner_by(partition, crate::range::dataset_of(partition))
            });
            to.conn()?
                .execute(
                    "UPDATE lake_partition SET writer_group = $2 WHERE dataset = $1",
                    &[&partition, &(from as i32)],
                )
                .map_err(unavailable)?;
        }
        match switched {
            Ok(_) => {}
            Err(failed) if timed_out(&failed) => return Ok(Switched::TimedOut),
            Err(failed) => return Err(unavailable(failed)),
        }
        if left(began).is_zero() {
            // Committed past its bound: the target owns, as recorded.
            tracing::warn!(
                partition,
                "a range's switch committed past moves.switchSeconds"
            );
        }
        self.groups.assign(partition, Some(target));
        Ok(Switched::Moved { closing, epoch })
    }
}

impl PostgresStore {
    /// A move's records appended to the lake's `control` partition, in one
    /// transaction (step 3d; section 6.8): `PartitionMoved` or
    /// `PartitionMoveAborted`, and a digest mismatch's `IntegrityFinding`
    /// beside it, each its kind and protobuf as the lake's other control
    /// records. The control partition's group's, under its epoch.
    pub fn log_move(&self, records: &[(&str, Vec<u8>)], at_ns: i64) -> Result<()> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        let entries: Vec<(&str, Vec<u8>, i64)> = records
            .iter()
            .map(|(kind, body)| (*kind, body.clone(), at_ns))
            .collect();
        self.log_control(&mut tx, &entries)?;
        tx.commit().map_err(unavailable)?;
        Ok(())
    }
}

impl PostgresStore {
    /// Range `partition` copied from `from`'s database into this one (step
    /// 3d; section 6.4, "a move between databases copies the data"): from
    /// 0, the snapshot -- its rows, its lineage's ranges and the rows of
    /// theirs its anchors place in it, so each subject's previous sequence
    /// is found here, and its keys' latest versions; after, the change log
    /// -- its own rows past `after`, and the latest versions they made.
    /// Each row by its keys, so a copy made again adds nothing. Answers the
    /// last sequence of the range's own copied.
    pub fn copy_range_from(
        &self,
        from: &PostgresStore,
        partition: &str,
        after: u64,
    ) -> Result<u64> {
        use std::io::{Read, Write};
        let dataset = crate::range::dataset_of(partition);
        let (_, lo, hi) = crate::range::parse(partition);
        let quote = |text: &str| text.replace('\'', "''");
        let mut source = from.conn()?;
        let lineage = lineage(&mut *source, partition)?;
        let ancestors: Vec<String> = lineage.iter().skip(1).cloned().collect();
        let list = |names: &[String]| {
            names
                .iter()
                .map(|name| format!("'{}'", quote(name)))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut rows_sql = format!(
            "SELECT * FROM lake_row WHERE dataset = '{d}' AND partition = '{p}' AND sequence > {after}",
            d = quote(dataset),
            p = quote(partition)
        );
        if after == 0 && !ancestors.is_empty() {
            // The lineage's rows this range's anchors hold: a row kept
            // before 3d is its dataset's full range's.
            let root = ancestors.contains(&dataset.to_string());
            rows_sql.push_str(&format!(
                " UNION ALL SELECT * FROM lake_row WHERE dataset = '{d}'
                    AND (partition IN ({a}){null})
                    AND {ANCHOR_SQL} BETWEEN {lo} AND {hi}",
                d = quote(dataset),
                a = list(&ancestors),
                null = if root { " OR partition IS NULL" } else { "" },
            ));
        }
        let mut copy = |sql: &str| -> Result<Vec<u8>> {
            let mut out = Vec::new();
            source
                .copy_out(&format!("COPY ({sql}) TO STDOUT (FORMAT binary)"))
                .map_err(unavailable)?
                .read_to_end(&mut out)
                .map_err(|failed| StoreError::Unavailable(failed.to_string()))?;
            Ok(out)
        };
        let rows = copy(&rows_sql)?;
        // The lineage's ranges, closed, each named the group it was; and the
        // keys' latest versions the copied rows are.
        let partitions = if after == 0 && !ancestors.is_empty() {
            copy(&format!(
                "SELECT * FROM lake_partition WHERE dataset IN ({})",
                list(&ancestors)
            ))?
        } else {
            Vec::new()
        };
        let latest = copy(&format!(
            "SELECT * FROM lake_latest WHERE dataset = '{d}' AND anchor BETWEEN {lo} AND {hi}{since}",
            d = quote(dataset),
            since = if after == 0 {
                String::new()
            } else {
                format!(" AND partition = '{}' AND sequence > {after}", quote(partition))
            }
        ))?;
        let copied: i64 = source
            .query_one(
                "SELECT coalesce(max(sequence), $3) FROM lake_row
                  WHERE dataset = $1 AND partition = $2",
                &[&dataset, &partition, &(after as i64)],
            )
            .map_err(unavailable)?
            .get(0);
        drop(source);
        let mut conn = self.conn()?;
        let load = |conn: &mut Connection, table: &str, like: &str, bytes: &[u8]| -> Result<()> {
            conn.batch_execute(&format!(
                "CREATE TEMP TABLE IF NOT EXISTS {table} (LIKE {like}); TRUNCATE {table}"
            ))
            .map_err(unavailable)?;
            if bytes.is_empty() {
                return Ok(());
            }
            let mut writer = conn
                .copy_in(&format!("COPY {table} FROM STDIN (FORMAT binary)"))
                .map_err(unavailable)?;
            writer
                .write_all(bytes)
                .map_err(|failed| StoreError::Unavailable(failed.to_string()))?;
            writer.finish().map_err(unavailable)?;
            Ok(())
        };
        load(&mut conn, "moving_rows", "lake_row", &rows)?;
        load(&mut conn, "moving_ranges", "lake_partition", &partitions)?;
        load(&mut conn, "moving_latest", "lake_latest", &latest)?;
        conn.execute(
            "UPDATE moving_rows SET partition = dataset WHERE partition IS NULL",
            &[],
        )
        .map_err(unavailable)?;
        // Each range's and month's table made first, in transactions of
        // their own (`leaves::ensure_ranges` holds no lock on lake_row).
        let mut needed: std::collections::BTreeSet<(String, crate::leaves::Month)> =
            std::collections::BTreeSet::new();
        for row in conn
            .query(
                "SELECT DISTINCT partition, valid_from_ns FROM moving_rows",
                &[],
            )
            .map_err(unavailable)?
        {
            needed.insert((row.get(0), crate::leaves::Month::of(row.get(1))));
        }
        let mut ranges: std::collections::BTreeSet<String> =
            needed.iter().map(|(p, _)| p.clone()).collect();
        ranges.insert(partition.to_string());
        {
            let mut tx = conn.transaction().map_err(unavailable)?;
            crate::leaves::ensure_ranges(&mut tx, &ranges).map_err(unavailable)?;
            tx.commit().map_err(unavailable)?;
        }
        let mut tx = conn.transaction().map_err(unavailable)?;
        crate::leaves::ensure_months(&mut tx, &needed).map_err(unavailable)?;
        tx.batch_execute(
            "INSERT INTO lake_partition SELECT * FROM moving_ranges ON CONFLICT (dataset) DO NOTHING;
             INSERT INTO lake_row SELECT * FROM moving_rows ON CONFLICT DO NOTHING;
             INSERT INTO lake_latest SELECT * FROM moving_latest
             ON CONFLICT (dataset, data_type, row_key) DO UPDATE SET
                partition = EXCLUDED.partition, sequence = EXCLUDED.sequence,
                version = EXCLUDED.version, valid_from_ns = EXCLUDED.valid_from_ns,
                anchor = EXCLUDED.anchor, force_partition = EXCLUDED.force_partition,
                force_sequence = EXCLUDED.force_sequence, force_version = EXCLUDED.force_version,
                force_published = EXCLUDED.force_published,
                force_valid_from = EXCLUDED.force_valid_from, subject = EXCLUDED.subject,
                kind = EXCLUDED.kind, venue_id = EXCLUDED.venue_id,
                interval_ns = EXCLUDED.interval_ns, latest_key = EXCLUDED.latest_key,
                quality = EXCLUDED.quality
              WHERE lake_latest.version < EXCLUDED.version;",
        )
        .map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        conn.batch_execute("DROP TABLE IF EXISTS moving_rows, moving_ranges, moving_latest")
            .map_err(unavailable)?;
        self.forget_made();
        Ok(copied.max(0) as u64)
    }

    /// Range `partition`'s row opened in this database, the target's, at
    /// the source's close `closing` (step 3d; section 6.8): as `from`'s
    /// database holds it, named writer group `target`'s at its epoch here.
    /// Answers the epoch.
    pub fn open_moved_in(
        &self,
        from: &PostgresStore,
        partition: &str,
        closing: u64,
        target: u32,
    ) -> Result<u64> {
        use std::io::{Read, Write};
        let mut row = Vec::new();
        from.conn()?
            .copy_out(&format!(
                "COPY (SELECT * FROM lake_partition WHERE dataset = '{}') TO STDOUT (FORMAT binary)",
                partition.replace('\'', "''")
            ))
            .map_err(unavailable)?
            .read_to_end(&mut row)
            .map_err(|failed| StoreError::Unavailable(failed.to_string()))?;
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        tx.batch_execute(
            "CREATE TEMP TABLE IF NOT EXISTS moving_range (LIKE lake_partition) ON COMMIT DROP",
        )
        .map_err(unavailable)?;
        let mut writer = tx
            .copy_in("COPY moving_range FROM STDIN (FORMAT binary)")
            .map_err(unavailable)?;
        writer
            .write_all(&row)
            .map_err(|failed| StoreError::Unavailable(failed.to_string()))?;
        writer.finish().map_err(unavailable)?;
        let epoch = group_epoch(&mut tx, target)?;
        tx.execute(
            "INSERT INTO lake_partition SELECT * FROM moving_range ON CONFLICT (dataset) DO NOTHING",
            &[],
        )
        .map_err(unavailable)?;
        tx.execute(
            "UPDATE lake_partition SET head = $2, epoch = $3, writer_group = $4, closed_at = NULL
              WHERE dataset = $1",
            &[
                &partition,
                &(closing as i64),
                &(epoch as i64),
                &(target as i32),
            ],
        )
        .map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        Ok(epoch)
    }
}
