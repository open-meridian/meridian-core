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

/// How long a range's switch between databases took, part by part (step
/// 3d; section 6.8): what its hold is made of, said in the member's log
/// and asserted by the crate's tests.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SwitchTimes {
    /// Before the hold: what was warmed compared on both sides.
    pub before: std::time::Duration,
    /// Waiting for the range's row: the source's batch in flight.
    pub waited: std::time::Duration,
    /// Under the row: the rest of the range's rows copied.
    pub tail: std::time::Duration,
    /// Under the row: both sides' digests taken again to the close.
    pub digest: std::time::Duration,
    /// Under the row: the target pointed or opened, and the source let go
    /// -- its latest versions of the range's keys deleted.
    pub let_go: std::time::Duration,
    /// From the row taken to the switch's answer: the hold.
    pub held: std::time::Duration,
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
    /// The source let go at `closing`, its row naming the target, and the
    /// target's row is not opened yet: moved, its open owed to the control
    /// group's sweep (the review's C3), under `epoch` as read before.
    LetGo { closing: u64, epoch: u64 },
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

    /// `dataset`'s rows here, its own and each range's: its head, and
    /// whether it is written (not a move's pointer).
    pub fn rows_of(&self, dataset: &str) -> Result<Vec<(String, u64, bool)>> {
        Ok(self
            .conn()?
            .query(
                "SELECT dataset, head, epoch <> $2 FROM lake_partition
                  WHERE dataset = $1 OR starts_with(dataset, $1 || '/')",
                &[&dataset, &meridian_changes::moves::POINTER_EPOCH],
            )
            .map_err(unavailable)?
            .iter()
            .map(|row| (row.get(0), row.get::<_, i64>(1).max(0) as u64, row.get(2)))
            .collect())
    }

    /// Every range's row here: its group as named, its head, and whether
    /// it is written (not a move's pointer).
    #[allow(clippy::type_complexity)]
    pub fn owner_rows(&self) -> Result<Vec<(String, Option<u32>, u64, bool)>> {
        Ok(self
            .conn()?
            .query(
                "SELECT dataset, writer_group, head, epoch <> $1 FROM lake_partition",
                &[&meridian_changes::moves::POINTER_EPOCH],
            )
            .map_err(unavailable)?
            .iter()
            .map(|row| {
                (
                    row.get(0),
                    row.get::<_, Option<i32>>(1).map(|group| group as u32),
                    row.get::<_, i64>(2).max(0) as u64,
                    row.get(3),
                )
            })
            .collect())
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
    /// writer group `target` in `to`'s (step 3d; section 6.8, step 4), its
    /// target warmed up to `warmed`: under the range's row, so the source's
    /// batch in flight finishes first and the next leaves the range's parts
    /// to wait (the hold) while its sibling ranges' commit; the source's
    /// close at its head N; the digests at N taken again on each side and
    /// compared (the review's C9). Within one database, the row named the
    /// target's at its group's epoch. Across two, never two owners (C3): the
    /// target's row first made a pointer to the target
    /// ([`meridian_changes::moves::POINTER_EPOCH`], under which nothing
    /// commits), then the source lets go -- its row a pointer to the target
    /// -- and is committed; only then is the target's row opened at N under
    /// its group's epoch, its write-ahead position taken back to the
    /// source's in the same transaction (C4). A failure between leaves the
    /// source's pointer, the durable intent the control group's sweep
    /// finishes from ([`Switched::LetGo`]). A move to the same group's own
    /// database opens the target first and makes the source a pointer
    /// after. Within `bound` of its start until the source lets go, else
    /// nothing changes. `differ` makes the digests differ (CI's
    /// MERIDIAN_MOVE_DIGEST_FILE).
    pub fn switch_range(
        &self,
        to: &PostgresStore,
        partition: &str,
        target: u32,
        bound: std::time::Duration,
        differ: bool,
        warmed: u64,
    ) -> Result<Switched> {
        self.switch_range_timed(to, partition, target, bound, differ, warmed)
            .map(|(switched, _)| switched)
    }

    /// [`Self::switch_range`], and how long each part of it took.
    pub fn switch_range_timed(
        &self,
        to: &PostgresStore,
        partition: &str,
        target: u32,
        bound: std::time::Duration,
        differ: bool,
        warmed: u64,
    ) -> Result<(Switched, SwitchTimes)> {
        let mut times = SwitchTimes::default();
        let mut locked = None;
        let switched = self.switch_range_within(
            to,
            partition,
            target,
            bound,
            differ,
            warmed,
            &mut times,
            &mut locked,
        );
        if let Some((locked, let_go)) = locked {
            times.held = locked.elapsed();
            if let Some(let_go) = let_go {
                times.let_go = let_go.elapsed();
            }
        }
        if !std::ptr::eq(self, to) {
            tracing::info!(
                partition,
                target,
                before_ms = times.before.as_millis() as u64,
                waited_ms = times.waited.as_millis() as u64,
                tail_ms = times.tail.as_millis() as u64,
                digest_ms = times.digest.as_millis() as u64,
                let_go_ms = times.let_go.as_millis() as u64,
                held_ms = times.held.as_millis() as u64,
                "a range's switch between databases: its hold, part by part"
            );
        }
        switched.map(|switched| (switched, times))
    }

    #[allow(clippy::too_many_arguments)]
    fn switch_range_within(
        &self,
        to: &PostgresStore,
        partition: &str,
        target: u32,
        bound: std::time::Duration,
        differ: bool,
        warmed: u64,
        times: &mut SwitchTimes,
        locked: &mut Option<(std::time::Instant, Option<std::time::Instant>)>,
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
        // What was warmed compared before the hold: under it, only the rest.
        let verified = if same {
            0
        } else {
            let theirs = range_digest(&mut *self.conn()?, partition, 0, warmed)?;
            let ours = range_digest(&mut *to.conn()?, partition, 0, warmed)?;
            if theirs == ours {
                warmed
            } else {
                0
            }
        };
        times.before = began.elapsed();
        let asked = std::time::Instant::now();
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
        times.waited = asked.elapsed();
        *locked = Some((std::time::Instant::now(), None));
        let source = range_tip_of(&mut tx, partition)?.unwrap_or_default();
        let closing = source.head;
        let from = source.writer_group.unwrap_or_else(|| {
            self.groups
                .owner_by(partition, crate::range::dataset_of(partition))
        });
        // The source group's committed place in the write-ahead stream, as
        // its batches -- which take this row -- left it.
        let theirs_key = crate::write_ahead::group_key(meridian_bus::write_ahead::STREAM, from);
        let ours_key = crate::write_ahead::group_key(meridian_bus::write_ahead::STREAM, target);
        let at = match stream_position(&mut tx, &theirs_key)? {
            Some(at) => Some(at),
            None if !same => stream_position(&mut *to.conn()?, &theirs_key)?,
            None => None,
        };
        if same {
            let mut target_digest = source.held_digest.clone();
            if differ {
                target_digest.push(0xff);
            }
            if target_digest != source.held_digest {
                return Ok(Switched::DigestDiffers {
                    closing,
                    source: source.held_digest,
                    target: target_digest,
                    at_ns: meridian_changes::now_ns(),
                });
            }
            let epoch = group_epoch(&mut tx, target)?;
            // In `ha`, the target takes the range's rows from the source's
            // committed stream position (spec 6.1; see `take_back`).
            if from != target {
                take_back(&mut tx, &ours_key, at)?;
            }
            // Where the source stood, kept with the range (`taken_from`):
            // what the target leaves uncounted of the writes it takes again.
            let taken_from = at.filter(|_| from != target).map(|at| at.sequence as i64);
            let switched = tx
                .execute(
                    "UPDATE lake_partition
                        SET writer_group = $2, epoch = $3, taken_from = coalesce($4, taken_from)
                      WHERE dataset = $1",
                    &[&partition, &(target as i32), &(epoch as i64), &taken_from],
                )
                .and_then(|_| tx.commit());
            return match switched {
                Ok(_) => {
                    self.groups.assign(partition, Some(target));
                    Ok(Switched::Moved { closing, epoch })
                }
                Err(failed) if timed_out(&failed) => Ok(Switched::TimedOut),
                Err(failed) => Err(unavailable(failed)),
            };
        }
        // The target at N: the range's rows copied up to N while the source
        // is held, both sides' digests taken again from what was compared.
        let part = std::time::Instant::now();
        to.copy_range_from(self, partition, warmed)?;
        times.tail = part.elapsed();
        let part = std::time::Instant::now();
        let theirs = range_digest(&mut tx, partition, verified, closing)?;
        let mut ours = range_digest(&mut *to.conn()?, partition, verified, closing)?;
        times.digest = part.elapsed();
        if let Some((_, let_go)) = locked.as_mut() {
            *let_go = Some(std::time::Instant::now());
        }
        if differ {
            ours.1.push(0xff);
        }
        if ours != theirs {
            return Ok(Switched::DigestDiffers {
                closing,
                source: theirs.1,
                target: ours.1,
                at_ns: meridian_changes::now_ns(),
            });
        }
        if left(began).is_zero() {
            return Ok(Switched::TimedOut);
        }
        // The latest versions in the range itself are the target's to keep
        // now: this database's let go, so a latest read here never answers
        // one the target has replaced. Its rows stay, read where it went.
        // A pointer's source keeps, beside it, the place in the stream the
        // target takes the range's rows from (`at`, under the target's own
        // key): what the sweep takes it back to where the target's open was
        // owed (the review's C3 and C4). Its own source's, where this source
        // is a pointer already, the open owed.
        let kept = if source.epoch == meridian_changes::moves::POINTER {
            stream_position(&mut tx, &ours_key)?
        } else {
            None
        };
        let keep = if from != target { at } else { None };
        let let_go = |tx: &mut postgres::Transaction<'_>| -> Result<()> {
            tx.execute(
                "DELETE FROM lake_latest WHERE dataset = $1 AND partition = $2",
                &[&crate::range::dataset_of(partition), &partition],
            )
            .map_err(unavailable)?;
            match keep {
                Some(at) => tx.execute(
                    "INSERT INTO lake_write_ahead (stream, sequence, at_ns, committed_at_ns)
                     VALUES ($1, $2, $3, $4)
                     ON CONFLICT (stream) DO UPDATE SET sequence = EXCLUDED.sequence,
                        at_ns = EXCLUDED.at_ns, committed_at_ns = EXCLUDED.committed_at_ns",
                    &[
                        &ours_key,
                        &(at.sequence as i64),
                        &at.at_ns,
                        &meridian_changes::now_ns(),
                    ],
                ),
                None if kept.is_some() => tx.execute(
                    "DELETE FROM lake_write_ahead WHERE stream = $1",
                    &[&ours_key],
                ),
                None => Ok(0),
            }
            .map_err(unavailable)?;
            tx.execute(
                "UPDATE lake_partition SET writer_group = $2, epoch = $3 WHERE dataset = $1",
                &[
                    &partition,
                    &(target as i32),
                    &meridian_changes::moves::POINTER_EPOCH,
                ],
            )
            .map_err(unavailable)?;
            Ok(())
        };
        if from == target {
            // The same group, home: the target opened first; the source a
            // pointer after, so a write routed there goes home.
            let epoch = to.open_moved_in(
                self,
                partition,
                closing,
                target,
                kept.map(|kept| (ours_key.as_str(), Some(kept))),
            )?;
            let_go(&mut tx)?;
            if let Err(failed) = tx.commit() {
                tracing::warn!(partition, %failed, "a range moved home: its old row stays its group's, level with home");
            }
            return Ok(Switched::Moved { closing, epoch });
        }
        // Another group's: the target's row a pointer to it first, so a
        // write that finds it there waits, never committed or fenced; then
        // the source lets go.
        let epoch = group_epoch(&mut *to.conn()?, target)?;
        to.point_moved_in(self, partition, closing, target)?;
        let_go(&mut tx)?;
        let committed = tx
            .commit()
            .map_err(|failed| (timed_out(&failed), unavailable(failed)))
            .and_then(|_| {
                meridian_changes::moves::points::reached("lake.switch.committed", partition)
                    .map_err(|lost| (false, StoreError::Unavailable(lost)))
            });
        if let Err((timed_out, failed)) = committed {
            // Committed, its answer lost, or not: as the source's row says
            // now (the review's A1), never undone on the target's side.
            let let_go = self.range_tip(partition)?.is_some_and(|tip| {
                tip.writer_group == Some(target) && tip.epoch == meridian_changes::moves::POINTER
            });
            if !let_go {
                // The target's pointer named the source again, so its
                // database never answers the range.
                to.point_moved_in(self, partition, closing, from)?;
                return if timed_out {
                    Ok(Switched::TimedOut)
                } else {
                    Err(failed)
                };
            }
        }
        self.groups.assign(partition, Some(target));
        if let Err(failed) =
            meridian_changes::moves::points::reached("lake.switch.between", partition)
        {
            tracing::warn!(
                partition,
                failed,
                "a range let go: its target opens on the sweep"
            );
            return Ok(Switched::LetGo { closing, epoch });
        }
        match to.open_moved_in(self, partition, closing, target, Some((&ours_key, at))) {
            Ok(epoch) => {
                // Taken back: the point kept beside the pointer let go.
                if at.is_some() {
                    if let Err(failed) = self.conn().and_then(|mut conn| {
                        conn.execute(
                            "DELETE FROM lake_write_ahead WHERE stream = $1",
                            &[&ours_key],
                        )
                        .map_err(unavailable)
                    }) {
                        tracing::warn!(partition, %failed, "a moved range's take-back point stays beside its pointer");
                    }
                }
                Ok(Switched::Moved { closing, epoch })
            }
            Err(failed) => {
                tracing::warn!(partition, %failed, "a range let go: its target opens on the sweep");
                Ok(Switched::LetGo { closing, epoch })
            }
        }
    }
}

/// Range `partition`'s rows in this database numbered past `after` up to
/// `upto`, taken again from their bodies: how many, and one digest over
/// each row's sequence, key and body's digest in their order (step 3d;
/// section 6.8's digest at N; the review's C9) -- a row missing, added or
/// changed on either side makes two databases' differ.
fn range_digest(
    client: &mut impl GenericClient,
    partition: &str,
    after: u64,
    upto: u64,
) -> Result<(i64, Vec<u8>)> {
    let row = client
        .query_one(
            "SELECT count(*),
                    coalesce(sha256(string_agg(int8send(sequence) || convert_to(row_key, 'UTF8')
                                               || sha256(body), ''::bytea
                                               ORDER BY sequence, data_type, row_key)), ''::bytea)
               FROM lake_row
              WHERE dataset = $1 AND partition = $2 AND sequence > $3 AND sequence <= $4",
            &[
                &crate::range::dataset_of(partition),
                &partition,
                &(after as i64),
                &(upto as i64),
            ],
        )
        .map_err(unavailable)?;
    Ok((row.get(0), row.get(1)))
}

/// Each of `ranges` that last moved to its group from another, with the
/// stream sequence its source had committed through then
/// (`lake_partition.taken_from`).
pub(crate) fn taken_from_of(
    client: &mut impl GenericClient,
    ranges: &[String],
) -> Result<BTreeMap<String, u64>> {
    Ok(client
        .query(
            "SELECT dataset, taken_from FROM lake_partition
              WHERE dataset = ANY($1) AND taken_from IS NOT NULL",
            &[&ranges],
        )
        .map_err(unavailable)?
        .iter()
        .map(|row| (row.get(0), row.get::<_, i64>(1) as u64))
        .collect())
}

/// A writer group's committed place in the write-ahead stream, under `key`.
fn stream_position(
    client: &mut impl GenericClient,
    key: &str,
) -> Result<Option<WriteAheadPosition>> {
    Ok(client
        .query_opt(
            "SELECT sequence, at_ns FROM lake_write_ahead WHERE stream = $1",
            &[&key],
        )
        .map_err(unavailable)?
        .map(|row| WriteAheadPosition {
            sequence: row.get::<_, i64>(0) as u64,
            at_ns: row.get(1),
        }))
}

/// A move's target's place in the write-ahead stream, under `key`, taken
/// back to the source's `source` where that is earlier, else back by one
/// write (spec 6.1). Nothing where it holds none: its first start takes from
/// the stream's first.
///
/// The step back by one where the two are equal (or the target's is
/// earlier) is deliberate, not an off-by-one: it is a compare-and-set
/// invalidation. The target's writer holds the position it last committed,
/// and a batch whose `after` is not the stored position is refused
/// `StoreError::PositionMoved` (record_write_ahead_batch). Moving the stored
/// position, always, is what forces that refusal, so the target reads its
/// place and the ranges' owners again before its next batch and never takes
/// the range's rows on a view that still names the source. Left equal, a
/// stale target would go on skipping them. The write it steps back over is
/// taken again: an idempotent repeat.
fn take_back(
    client: &mut impl GenericClient,
    key: &str,
    source: Option<WriteAheadPosition>,
) -> Result<()> {
    let Some(row) = client
        .query_opt(
            "SELECT sequence, at_ns FROM lake_write_ahead WHERE stream = $1 FOR UPDATE",
            &[&key],
        )
        .map_err(unavailable)?
    else {
        return Ok(());
    };
    let (sequence, at_ns): (i64, i64) = (row.get(0), row.get(1));
    let (back, back_at) = match source {
        Some(at) if (at.sequence as i64) < sequence => (at.sequence as i64, at.at_ns),
        _ => ((sequence - 1).max(0), at_ns),
    };
    client
        .execute(
            "UPDATE lake_write_ahead SET sequence = $2, at_ns = $3, committed_at_ns = $4
              WHERE stream = $1",
            &[&key, &back, &back_at, &meridian_changes::now_ns()],
        )
        .map_err(unavailable)?;
    Ok(())
}

impl PostgresStore {
    /// This member seen in the lake's members (step 3d; ruled 2026-11-15):
    /// its row in this database, the lake's database 0, made or brought up
    /// to date.
    pub fn member_seen(
        &self,
        member: &str,
        revision: &str,
        started_at_ns: i64,
        now_ns: i64,
    ) -> Result<()> {
        meridian_changes::members::LAKE
            .seen(&mut *self.conn()?, member, revision, started_at_ns, now_ns)
            .map_err(unavailable)
    }

    /// This member seen, with what it serves as, in one statement (step
    /// 3d, stage T; approved 2026-11-15; the review's finding 9): its
    /// writer group, and its mode once it serves.
    pub fn member_says(
        &self,
        member: &str,
        revision: &str,
        started_at_ns: i64,
        now_ns: i64,
        writer_group: u32,
        mode: &str,
    ) -> Result<()> {
        meridian_changes::members::LAKE
            .says(
                &mut *self.conn()?,
                member,
                revision,
                started_at_ns,
                now_ns,
                writer_group,
                mode,
            )
            .map_err(unavailable)
    }

    /// Every other member's row, as each last said it: what a writer told
    /// to stop watches ([`meridian_changes::members::Watch`]).
    pub fn members_but(&self, member: &str) -> Result<Vec<meridian_changes::members::Said>> {
        meridian_changes::members::LAKE
            .others(&mut *self.conn()?, member)
            .map_err(unavailable)
    }

    /// Commit nothing more through this store (step 3d, stage T's review,
    /// finding 3): every commit from here is refused as a fenced one is.
    /// What the lake's writer does, told to stop, where no lease fences
    /// its group and it serves on beside its replacement
    /// ([`meridian_changes::Fence::stop`]).
    pub fn stop_committing(&self) {
        self.fence.stop();
    }

    /// The members seen within `within_ns` of `now_ns` running another
    /// revision than `revision`: none once the lake's rollout is complete.
    pub fn members_behind(
        &self,
        revision: &str,
        now_ns: i64,
        within_ns: i64,
    ) -> Result<Vec<meridian_changes::members::Behind>> {
        meridian_changes::members::LAKE
            .behind(&mut *self.conn()?, revision, now_ns, within_ns)
            .map_err(unavailable)
    }

    /// Each range's last move logged in this database's control that was by
    /// hand (step 3d; section 6.8): the group the admin moved it to. A range
    /// whose last move was by a changed count is not named.
    /// `partition` and every range it was split from, itself first.
    pub fn lineage_of(&self, partition: &str) -> Result<Vec<String>> {
        lineage(&mut *self.conn()?, partition)
    }

    /// [`taken_from_of`], in this database.
    pub fn taken_from(&self, ranges: &[String]) -> Result<BTreeMap<String, u64>> {
        taken_from_of(&mut *self.conn()?, ranges)
    }

    pub fn moved_by_hand(&self) -> Result<BTreeMap<String, u32>> {
        let mut out = BTreeMap::new();
        for row in self
            .conn()?
            .query(
                "SELECT body FROM lake_control_change WHERE kind = 'meridian.v1.PartitionMoved'
                  ORDER BY sequence",
                &[],
            )
            .map_err(unavailable)?
        {
            let body: Vec<u8> = row.get(0);
            let Ok(moved) = meridian_domain::v1::PartitionMoved::decode(&body[..]) else {
                continue;
            };
            // A move to its own group's database places it nowhere new.
            if moved.from_group == moved.to_group {
                continue;
            }
            if moved.by_hand {
                out.insert(moved.partition, moved.to_group);
            } else {
                out.remove(&moved.partition);
            }
        }
        Ok(out)
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
        // One snapshot of the source, the range's last sequence read first
        // and nothing past it copied: the copy whole up to what it answers,
        // never a later row ahead of an earlier one missed (the review's C2).
        let mut source_conn = from.conn()?;
        let mut source = source_conn
            .build_transaction()
            .isolation_level(postgres::IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .map_err(unavailable)?;
        let copied: i64 = source
            .query_one(
                "SELECT coalesce(max(sequence), $3) FROM lake_row
                  WHERE dataset = $1 AND partition = $2",
                &[&dataset, &partition, &(after as i64)],
            )
            .map_err(unavailable)?
            .get(0);
        meridian_changes::moves::points::reached("lake.copy.read", partition)
            .map_err(StoreError::Unavailable)?;
        let lineage = lineage(&mut source, partition)?;
        let ancestors: Vec<String> = lineage.iter().skip(1).cloned().collect();
        let list = |names: &[String]| {
            names
                .iter()
                .map(|name| format!("'{}'", quote(name)))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut rows_sql = format!(
            "SELECT * FROM lake_row WHERE dataset = '{d}' AND partition = '{p}' AND sequence > {after}
                AND sequence <= {copied}",
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
                format!(" AND (partition <> '{}' OR sequence <= {copied})", quote(partition))
            } else {
                format!(
                    " AND partition = '{}' AND sequence > {after} AND sequence <= {copied}",
                    quote(partition)
                )
            }
        ))?;
        source.commit().map_err(unavailable)?;
        drop(source_conn);
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
    /// database holds it, whole (the review's M4), named writer group
    /// `target`'s at its epoch here; in the same transaction, where given,
    /// the target's write-ahead position taken back to the source's (C4).
    /// Answers the epoch.
    pub fn open_moved_in(
        &self,
        from: &PostgresStore,
        partition: &str,
        closing: u64,
        target: u32,
        taken_back: Option<(&str, Option<WriteAheadPosition>)>,
    ) -> Result<u64> {
        self.moved_in(from, partition, closing, target, None, taken_back)
    }

    /// Range `partition`'s row here a pointer to writer group `group` at
    /// `closing`, as `from`'s database holds it otherwise: nothing commits
    /// under it, and a write that finds it waits (the review's C3).
    pub fn point_moved_in(
        &self,
        from: &PostgresStore,
        partition: &str,
        closing: u64,
        group: u32,
    ) -> Result<()> {
        self.moved_in(
            from,
            partition,
            closing,
            group,
            Some(meridian_changes::moves::POINTER_EPOCH),
            None,
        )?;
        Ok(())
    }

    fn moved_in(
        &self,
        from: &PostgresStore,
        partition: &str,
        closing: u64,
        group: u32,
        epoch: Option<i64>,
        taken_back: Option<(&str, Option<WriteAheadPosition>)>,
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
        let epoch = match epoch {
            Some(epoch) => epoch,
            None => group_epoch(&mut tx, group)? as i64,
        };
        tx.batch_execute(
            "DELETE FROM lake_partition WHERE dataset IN (SELECT dataset FROM moving_range);
             INSERT INTO lake_partition SELECT * FROM moving_range;",
        )
        .map_err(unavailable)?;
        tx.execute(
            "UPDATE lake_partition SET head = $2, epoch = $3, writer_group = $4, closed_at = NULL
              WHERE dataset = $1",
            &[&partition, &(closing as i64), &epoch, &(group as i32)],
        )
        .map_err(unavailable)?;
        if let Some((key, at)) = taken_back {
            take_back(&mut tx, key, at)?;
            // Where the source stood, kept with the range (`taken_from`).
            if let Some(at) = at {
                tx.execute(
                    "UPDATE lake_partition SET taken_from = $2 WHERE dataset = $1",
                    &[&partition, &(at.sequence as i64)],
                )
                .map_err(unavailable)?;
            }
        }
        tx.commit().map_err(unavailable)?;
        Ok(epoch.max(0) as u64)
    }
}
