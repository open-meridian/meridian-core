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
    // Its own rows' last: led by the range, then its dataset and sequence,
    // which each month's `_seq` index answers.
    let held = client
        .query_opt(
            "SELECT sequence, digest, recorded_at_ns FROM lake_row
              WHERE partition = $1 AND dataset = $2
              ORDER BY sequence DESC LIMIT 1",
            &[&partition, &crate::range::dataset_of(partition)],
        )
        .map_err(unavailable)?;
    Ok(Some(RangeTip {
        head: row.get::<_, i64>(0).max(0) as u64,
        epoch: row.get::<_, i64>(1) as u64,
        writer_group: row.get::<_, Option<i32>>(2).map(|group| group as u32),
        closed: row.get::<_, Option<i64>>(3).is_some(),
        held: held
            .as_ref()
            .map_or(0, |row| row.get::<_, i64>(0).max(0) as u64),
        held_digest: held
            .as_ref()
            .and_then(|row| row.get::<_, Option<Vec<u8>>>(1))
            .unwrap_or_default(),
        held_at_ns: held.as_ref().map_or(0, |row| row.get(2)),
    }))
}

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
        if !same {
            return Err(StoreError::Unavailable(format!(
                "range {partition}: a move between the lake's databases is not built yet"
            )));
        }
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
        // The target at N: the same rows, in this database.
        let held = source.clone();
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
        let epoch = group_epoch(&mut tx, target)?;
        let switched = tx
            .execute(
                "UPDATE lake_partition SET writer_group = $2, epoch = $3 WHERE dataset = $1",
                &[&partition, &(target as i32), &(epoch as i64)],
            )
            .and_then(|_| tx.commit());
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
