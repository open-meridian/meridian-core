//! A plugin shard's fence (step 3d, checkpoint 5, item 32; the resilience
//! spec's section 7).
//!
//! A store fences its own writer by its partition's epoch ([`crate::Fence`]).
//! A plugin's shard is fenced the same way, by the shard's epoch: a sidecar
//! that takes a shard's Lease first has the lake raise the shard's epoch
//! (`TakeShard`: max(held + 1, the Lease's transitions)), and stamps it on
//! every write it sends for its plugin (`MessageMeta.shard`,
//! `MessageMeta.shard_epoch`). Each writer group's database keeps the
//! highest epoch it has seen of each shard -- `lake_shard_fence` and
//! `book_shard_fence`, (instance, shard, epoch) -- and checks a write's in
//! the write's own transaction, as a Kafka partition leader checks a
//! producer's epoch:
//! - **lower** than the one held: refused, `FAILED_PRECONDITION`,
//!   `REFUSAL_REASON_SHARD_FENCED`, [`WORDS`]; the sidecar then exits, as on a
//!   lost Lease;
//! - **equal**: it passes;
//! - **higher**: it raises the one held, and passes.
//!
//! A write stamped with no epoch -- a one-pod instance's, which takes no
//! Lease -- is checked by nothing.
//!
//! **The check holds the shard's row for the transaction.** An equal epoch
//! passes holding the row `FOR SHARE`, so a raise waits for every batch
//! that passed under the old epoch to commit, and none passes under it
//! after. A raise is an `UPDATE` that touches the row only where it is
//! below, so a batch never upgrades a lock it holds shared. A batch checks
//! its shards in one order, before any other lock it takes.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// The refusal's words (approved 2026-11-15).
pub const WORDS: &str = "fenced: this shard's epoch is no longer the instance's";

/// A store's writer group `group`'s ask to raise a shard's epoch (step 3d,
/// checkpoint 5, item 32; core-internal): `domain` is `lake` or `book`. The
/// lake's control writer asks each group's on its own, with the
/// `ShardTookOver`, and answers a take only once every group has answered.
pub fn raise_topic(domain: &str, group: u32) -> String {
    format!("platform.{domain}.internal.raise-shard.{group}")
}

/// Whether `topic` is a writer group's ask to raise a shard's epoch.
pub fn is_raise_topic(topic: &str) -> bool {
    topic.rsplit_once('.').is_some_and(|(_, group)| {
        group.parse::<u32>().is_ok_and(|group| {
            ["lake", "book"]
                .iter()
                .any(|domain| raise_topic(domain, group) == topic)
        })
    })
}

/// How long the lake's control writer gives every writer group to confirm a
/// take, asking again any that has not: inside the 10 s takeover, after a
/// Lease's 6 s lapse.
pub const CONFIRM_WITHIN: std::time::Duration = std::time::Duration::from_millis(2_500);

/// How long one group's ask waits before it is asked again.
pub const ASK_EACH: std::time::Duration = std::time::Duration::from_millis(800);

/// The lake's table.
pub const LAKE: &str = "lake_shard_fence";
/// The book's table.
pub const BOOK: &str = "book_shard_fence";

/// A write's shard and epoch, as its sidecar stamped them.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShardStamp {
    /// The plugin instance the sidecar serves (`publisher_instance_id`).
    pub instance: String,
    pub shard: u32,
    /// Never 0.
    pub epoch: u64,
}

impl ShardStamp {
    /// The stamp on a write: none where the sidecar stamped no epoch, which
    /// no fence checks.
    pub fn of(instance: &str, shard: u32, epoch: u64) -> Option<Self> {
        (epoch > 0 && !instance.is_empty()).then(|| Self {
            instance: instance.to_string(),
            shard,
            epoch,
        })
    }

    fn key(&self) -> (String, u32) {
        (self.instance.clone(), self.shard)
    }
}

/// A write refused by its shard's epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardFenced {
    pub stamp: ShardStamp,
    /// The epoch the database holds, above the write's.
    pub held: u64,
}

impl std::fmt::Display for ShardFenced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(WORDS)
    }
}

impl std::error::Error for ShardFenced {}

impl ShardFenced {
    /// What a log says of it: the shard, and both epochs.
    pub fn detail(&self) -> String {
        format!(
            "instance {} shard {}: the write's epoch {}, the epoch held {}",
            self.stamp.instance, self.stamp.shard, self.stamp.epoch, self.held
        )
    }
}

/// Each shard's highest epoch among `stamps`, in one order: what a batch
/// checks, once each.
fn highest<'a>(stamps: impl IntoIterator<Item = &'a ShardStamp>) -> BTreeMap<(String, u32), u64> {
    let mut highest: BTreeMap<(String, u32), u64> = BTreeMap::new();
    for stamp in stamps {
        let at = highest.entry(stamp.key()).or_insert(stamp.epoch);
        *at = (*at).max(stamp.epoch);
    }
    highest
}

/// What each of `stamps` is answered, given each shard's epoch held after
/// the batch's raises.
fn answers(
    stamps: &[Option<&ShardStamp>],
    held: &BTreeMap<(String, u32), u64>,
) -> Vec<Result<(), ShardFenced>> {
    stamps
        .iter()
        .map(|stamp| match stamp {
            None => Ok(()),
            Some(stamp) => {
                let held = held.get(&stamp.key()).copied().unwrap_or(0);
                if stamp.epoch < held {
                    Err(ShardFenced {
                        stamp: (*stamp).clone(),
                        held,
                    })
                } else {
                    Ok(())
                }
            }
        })
        .collect()
}

/// The writes' stamps checked in `tx`, the batch's own transaction, against
/// `table` ([`LAKE`] or [`BOOK`]): each answered in turn, a write with no
/// stamp passing. A shard's row is held for the transaction ([module
/// docs](self)).
pub fn check(
    tx: &mut postgres::Transaction<'_>,
    table: &'static str,
    stamps: &[Option<&ShardStamp>],
) -> Result<Vec<Result<(), ShardFenced>>, postgres::Error> {
    let wanted = highest(stamps.iter().flatten().copied());
    let mut held = BTreeMap::new();
    for ((instance, shard), epoch) in wanted {
        let now = hold(tx, table, &instance, shard, epoch)?;
        held.insert((instance, shard), now);
    }
    Ok(answers(stamps, &held))
}

/// One shard's row raised to `epoch` where it is below, made where there is
/// none, and held shared: the epoch it holds after.
fn hold(
    tx: &mut postgres::Transaction<'_>,
    table: &'static str,
    instance: &str,
    shard: u32,
    epoch: u64,
) -> Result<u64, postgres::Error> {
    let raise =
        format!("UPDATE {table} SET epoch = $3 WHERE instance = $1 AND shard = $2 AND epoch < $3");
    let read = format!("SELECT epoch FROM {table} WHERE instance = $1 AND shard = $2 FOR SHARE");
    let (shard, epoch) = (i64::from(shard), epoch as i64);
    tx.execute(&raise, &[&instance, &shard, &epoch])?;
    if let Some(row) = tx.query_opt(&read, &[&instance, &shard])? {
        return Ok(row.get::<_, i64>(0) as u64);
    }
    tx.execute(
        &format!(
            "INSERT INTO {table} (instance, shard, epoch) VALUES ($1, $2, $3)
             ON CONFLICT (instance, shard) DO NOTHING"
        ),
        &[&instance, &shard, &epoch],
    )?;
    // Made by another at once: raised where it is below ours.
    tx.execute(&raise, &[&instance, &shard, &epoch])?;
    let row = tx.query_one(&read, &[&instance, &shard])?;
    Ok(row.get::<_, i64>(0) as u64)
}

/// A shard taken (`TakeShard`, in the lake's control writer's transaction):
/// its epoch raised to max(held + 1, `lease_transitions`), at least 1, and
/// answered.
pub fn take(
    tx: &mut postgres::Transaction<'_>,
    table: &'static str,
    instance: &str,
    shard: u32,
    lease_transitions: u64,
) -> Result<u64, postgres::Error> {
    let row = tx.query_one(
        &format!(
            "INSERT INTO {table} AS held (instance, shard, epoch)
             VALUES ($1, $2::bigint, GREATEST(1::bigint, $3::bigint))
             ON CONFLICT (instance, shard)
             DO UPDATE SET epoch = GREATEST(held.epoch + 1, EXCLUDED.epoch)
             RETURNING epoch"
        ),
        &[&instance, &i64::from(shard), &(lease_transitions as i64)],
    )?;
    Ok(row.get::<_, i64>(0) as u64)
}

/// A shard's epoch raised to `epoch` where it is below (a take heard from
/// the lake's control writer): the epoch held after.
pub fn raise_to(
    tx: &mut postgres::Transaction<'_>,
    table: &'static str,
    instance: &str,
    shard: u32,
    epoch: u64,
) -> Result<u64, postgres::Error> {
    hold(tx, table, instance, shard, epoch)
}

/// The epoch `table` holds of each shard of `instance` ("" for every
/// instance), by instance and shard.
pub fn held(
    client: &mut impl postgres::GenericClient,
    table: &'static str,
    instance: &str,
) -> Result<BTreeMap<(String, u32), u64>, postgres::Error> {
    Ok(client
        .query(
            &format!(
                "SELECT instance, shard, epoch FROM {table}
                  WHERE $1 = '' OR instance = $1 ORDER BY instance, shard"
            ),
            &[&instance],
        )?
        .into_iter()
        .map(|row| {
            (
                (row.get::<_, String>(0), row.get::<_, i64>(1) as u32),
                row.get::<_, i64>(2) as u64,
            )
        })
        .collect())
}

/// The fence a store held in memory keeps: its tests', and a store with no
/// database.
#[derive(Debug, Default)]
pub struct Memory {
    held: Mutex<BTreeMap<(String, u32), u64>>,
}

impl Memory {
    /// [`check`], all at once under one lock.
    pub fn check(&self, stamps: &[Option<&ShardStamp>]) -> Vec<Result<(), ShardFenced>> {
        let mut held = self.held.lock().expect("shard fence poisoned");
        for (key, epoch) in highest(stamps.iter().flatten().copied()) {
            let at = held.entry(key).or_insert(epoch);
            *at = (*at).max(epoch);
        }
        answers(stamps, &held)
    }

    /// [`take`].
    pub fn take(&self, instance: &str, shard: u32, lease_transitions: u64) -> u64 {
        let mut held = self.held.lock().expect("shard fence poisoned");
        let at = held.entry((instance.to_string(), shard)).or_insert(0);
        *at = (*at + 1).max(lease_transitions).max(1);
        *at
    }

    /// [`raise_to`].
    pub fn raise_to(&self, instance: &str, shard: u32, epoch: u64) -> u64 {
        let mut held = self.held.lock().expect("shard fence poisoned");
        let at = held.entry((instance.to_string(), shard)).or_insert(epoch);
        *at = (*at).max(epoch);
        *at
    }

    /// The epoch held of a shard; 0 where none.
    pub fn epoch(&self, instance: &str, shard: u32) -> u64 {
        self.held
            .lock()
            .expect("shard fence poisoned")
            .get(&(instance.to_string(), shard))
            .copied()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(epoch: u64) -> ShardStamp {
        ShardStamp::of("feed-1", 1, epoch).unwrap()
    }

    #[test]
    fn a_raise_topic_is_a_store_s_and_a_group_s() {
        assert!(is_raise_topic(&raise_topic("lake", 3)));
        assert!(is_raise_topic(&raise_topic("book", 0)));
        assert!(!is_raise_topic(&raise_topic("street", 0)));
        assert!(!is_raise_topic("platform.lake.internal.record"));
        assert!(!is_raise_topic(&format!("{}x", raise_topic("lake", 1))));
    }

    #[test]
    fn no_epoch_is_no_stamp() {
        assert_eq!(ShardStamp::of("feed-1", 0, 0), None);
        assert_eq!(ShardStamp::of("", 0, 3), None);
    }

    #[test]
    fn lower_is_refused_equal_passes_higher_raises() {
        let fence = Memory::default();
        assert_eq!(fence.take("feed-1", 1, 0), 1);
        // Equal passes.
        assert_eq!(fence.check(&[Some(&stamp(1))]), vec![Ok(())]);
        // Higher raises, and passes.
        assert_eq!(fence.check(&[Some(&stamp(3))]), vec![Ok(())]);
        assert_eq!(fence.epoch("feed-1", 1), 3);
        // Lower is refused, with the words.
        let refused = fence.check(&[Some(&stamp(2))]);
        assert_eq!(refused[0].as_ref().unwrap_err().held, 3);
        assert_eq!(refused[0].as_ref().unwrap_err().to_string(), WORDS);
        // A batch holding both: the higher raises, the lower is refused.
        let both = fence.check(&[Some(&stamp(3)), None, Some(&stamp(4))]);
        assert_eq!(both[1], Ok(()));
        assert_eq!(both[2], Ok(()));
        assert!(both[0].is_err());
    }

    #[test]
    fn a_take_raises_past_the_held_epoch_and_the_lease_s_transitions() {
        let fence = Memory::default();
        assert_eq!(fence.take("feed-1", 0, 0), 1);
        assert_eq!(fence.take("feed-1", 0, 0), 2);
        assert_eq!(fence.take("feed-1", 0, 7), 7);
        assert_eq!(fence.take("feed-1", 0, 3), 8);
        assert_eq!(fence.raise_to("feed-1", 0, 5), 8);
        assert_eq!(fence.epoch("feed-1", 2), 0);
    }
}
