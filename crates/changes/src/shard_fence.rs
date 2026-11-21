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
//! **A write the stream held before the take is admitted** (review c5a H2,
//! ruled 2026-11-16). Each writer group's raise keeps, beside the epoch,
//! the lake's write-ahead stream's last sequence as the raise found it
//! (`admit_through`): a write taken from the stream at or below it was sent
//! before the take reached that group -- a write answered already and taken
//! again after a database failover lost its commit, or one the writer had
//! not reached -- and is admitted whatever its epoch, so a write the lake
//! answered as recorded is never refused on its replay. A write past it, or
//! one not from the stream, is checked as above.
//!
//! **Each group keeps a take's place once, with the take's own epoch**
//! (review c5a3 H1, M1, ruled 2026-11-16). A group's row reaches a take's
//! epoch only through that take: database 0's by the take's numbering,
//! which keeps the place the control writer found before it, and every
//! other by the take's raise. A raise asked again of an epoch held keeps
//! the place the first found, so it never admits a write sent after it.
//!
//! **The epochs outlive a database's failover** (reviews c5a H1, c5a2 N1,
//! N2). The lake commits its batches locally, but every fence transaction --
//! a take's numbering and record, a group's raise and its stream's place --
//! commits on the synchronous standby, so an answered take survives, and a
//! row below database 0's is a take still being raised, or one that failed.
//! A take is numbered by its row, which holds the shard's highest epoch; on
//! a new database timeline, and as a member starts, a shard a database has
//! no row of is made there from database 0's ([`seat`]): a database added
//! later. A row it has is never raised there: an honest write's replay
//! before the take's own raise reaches it passes at its epoch (c5a3 H1). A
//! stream made again admits nothing by an older one's place (c5a2 N4). A
//! take whose Lease's transitions are below the highest a take of the shard
//! was numbered with, on the same Lease, is refused (review c5a M2): a slow
//! pod's late take never fences the holder that took the Lease after it. A
//! Lease made again -- another uid, its transitions from 0 -- starts that
//! floor afresh (c5a2 N5).
//!
//! **The check holds the shard's row for the transaction.** An equal epoch
//! passes holding the row `FOR SHARE`, so a raise waits for every batch
//! that passed under the old epoch to commit, and none passes under it
//! after. A raise is an `UPDATE` that touches the row only where it is
//! below, so a batch never upgrades a lock it holds shared. A batch checks
//! its shards in one order, before any other lock it takes.

use std::collections::btree_map::Entry;
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

/// How long a sidecar waits for its take's answer: the lake's confirm
/// window, which starts no round it cannot end inside, and a second for the
/// take's numbering, its record and the way there and back (review c5a M3).
/// Inside a shard Lease's 4 s renew deadline, as the sidecar renews just
/// before it asks.
pub const TAKE_WITHIN: std::time::Duration =
    CONFIRM_WITHIN.saturating_add(std::time::Duration::from_secs(1));

/// The most a shard's epoch is (the review of v31a-core-2, F1(b)): a Lease's
/// `leaseTransitions` is an int32, so a take is numbered within it, and a
/// write stamped past it raises nothing and is refused. Every epoch then
/// keeps its value through the tables' bigint and compares there as it does
/// here; a take that would number past it is refused, never wrapped.
pub const EPOCH_MOST: u64 = i32::MAX as u64;

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
    /// The write-ahead stream's sequence of the write it came with; 0 for a
    /// write not taken from the stream.
    pub in_stream: u64,
    /// That stream's identity (its creation time, ns): a sequence of another
    /// stream is admitted by nothing (review c5a2 N4).
    pub stream: i64,
}

impl ShardStamp {
    /// The stamp on a write: none where the sidecar stamped no epoch, which
    /// no fence checks.
    pub fn of(instance: &str, shard: u32, epoch: u64) -> Option<Self> {
        (epoch > 0 && !instance.is_empty()).then(|| Self {
            instance: instance.to_string(),
            shard,
            epoch,
            in_stream: 0,
            stream: 0,
        })
    }

    /// The same stamp, on a write taken at `sequence` from the write-ahead
    /// stream `stream` (its identity, [`Admit::stream`]).
    pub fn in_stream(mut self, sequence: u64, stream: i64) -> Self {
        self.in_stream = sequence;
        self.stream = stream;
        self
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

/// The write-ahead stream's place a take's raise found: its last sequence,
/// and the stream's identity (its creation time, ns), so a stream made again
/// -- numbering from 1 -- admits nothing by an older one's place (review
/// c5a2 N4). The default admits nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Admit {
    pub through: u64,
    pub stream: i64,
}

/// What a database holds of a shard: its epoch, and the write-ahead
/// stream's place the raise to it found ([module docs](self)).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Held {
    pub epoch: u64,
    pub admit: Admit,
}

impl Held {
    /// Whether `stamp` is refused against it.
    fn refuses(&self, stamp: &ShardStamp) -> bool {
        let admitted = stamp.in_stream > 0
            && stamp.stream != 0
            && stamp.stream == self.admit.stream
            && stamp.in_stream <= self.admit.through;
        stamp.epoch < self.epoch && !admitted
    }
}

/// A fence transaction commits on the synchronous standby, whatever the
/// connection's default (review c5a2 N1, N2: the lake commits locally, and
/// a take answered must survive its database's failover).
fn durably(tx: &mut postgres::Transaction<'_>) -> Result<(), postgres::Error> {
    tx.batch_execute("SET LOCAL synchronous_commit = on")
}

/// The stamps among `stamps` that may raise a fence: those within
/// [`EPOCH_MOST`].
fn within<'a>(stamps: &'a [Option<&'a ShardStamp>]) -> impl Iterator<Item = &'a ShardStamp> {
    stamps
        .iter()
        .flatten()
        .copied()
        .filter(|stamp| stamp.epoch <= EPOCH_MOST)
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
    held: &BTreeMap<(String, u32), Held>,
) -> Vec<Result<(), ShardFenced>> {
    stamps
        .iter()
        .map(|stamp| match stamp {
            None => Ok(()),
            Some(stamp) => {
                let held = held.get(&stamp.key()).copied().unwrap_or_default();
                if stamp.epoch > EPOCH_MOST || held.refuses(stamp) {
                    Err(ShardFenced {
                        stamp: (*stamp).clone(),
                        held: held.epoch,
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
/// stamp passing, and one stamped past [`EPOCH_MOST`] refused, raising
/// nothing but answered with the epoch its shard holds. A shard's row is
/// held for the transaction ([module docs](self)).
pub fn check(
    tx: &mut postgres::Transaction<'_>,
    table: &'static str,
    stamps: &[Option<&ShardStamp>],
) -> Result<Vec<Result<(), ShardFenced>>, postgres::Error> {
    let wanted = highest(within(stamps));
    let mut held = BTreeMap::new();
    for ((instance, shard), epoch) in wanted {
        // A write raises the epoch alone: only a take's raise knows the
        // stream's place.
        let now = hold(tx, table, &instance, shard, epoch, None)?;
        held.insert((instance, shard), now);
    }
    // A shard stamped only past the most is read, not raised, so its
    // refusal names the epoch it holds (review-core-2c, finding 4).
    for stamp in stamps.iter().flatten() {
        if let Entry::Vacant(at) = held.entry(stamp.key()) {
            at.insert(held_now(tx, table, &stamp.instance, stamp.shard)?.unwrap_or_default());
        }
    }
    Ok(answers(stamps, &held))
}

/// One shard's row, held shared, where there is one.
fn held_now(
    tx: &mut postgres::Transaction<'_>,
    table: &'static str,
    instance: &str,
    shard: u32,
) -> Result<Option<Held>, postgres::Error> {
    Ok(tx
        .query_opt(&read_held(table), &[&instance, &i64::from(shard)])?
        .map(held_of))
}

/// A shard's row read and held shared, `$1` its instance and `$2` its shard.
fn read_held(table: &str) -> String {
    format!(
        "SELECT epoch, admit_through, admit_stream FROM {table}
          WHERE instance = $1 AND shard = $2 FOR SHARE"
    )
}

/// What a row read by [`read_held`] holds.
fn held_of(row: postgres::Row) -> Held {
    Held {
        epoch: row.get::<_, i64>(0) as u64,
        admit: Admit {
            through: row.get::<_, i64>(1) as u64,
            stream: row.get::<_, i64>(2),
        },
    }
}

/// One shard's row raised to `epoch` where it is below, made where there is
/// none, and held shared: what it holds after. A take's raise (`admit`)
/// keeps the stream's place with the epoch -- raised within one stream,
/// replaced by another's -- and a write's raises the epoch alone.
fn hold(
    tx: &mut postgres::Transaction<'_>,
    table: &'static str,
    instance: &str,
    shard: u32,
    epoch: u64,
    admit: Option<Admit>,
) -> Result<Held, postgres::Error> {
    let raise = match admit {
        None => format!(
            "UPDATE {table} SET epoch = $3
              WHERE instance = $1 AND shard = $2 AND epoch < $3 AND $4::bigint >= 0
                AND $5::bigint >= 0"
        ),
        Some(_) => format!(
            "UPDATE {table} SET epoch = $3,
                    admit_through = CASE WHEN admit_stream = $5 THEN GREATEST(admit_through, $4)
                                         ELSE $4 END,
                    admit_stream = $5
              WHERE instance = $1 AND shard = $2 AND epoch < $3"
        ),
    };
    let read = read_held(table);
    let given = admit.unwrap_or_default();
    let (shard, epoch) = (i64::from(shard), epoch as i64);
    let (through, stream) = (given.through as i64, given.stream);
    tx.execute(&raise, &[&instance, &shard, &epoch, &through, &stream])?;
    if let Some(row) = tx.query_opt(&read, &[&instance, &shard])? {
        return Ok(held_of(row));
    }
    tx.execute(
        &format!(
            "INSERT INTO {table} (instance, shard, epoch, admit_through, admit_stream)
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (instance, shard) DO NOTHING"
        ),
        &[&instance, &shard, &epoch, &through, &stream],
    )?;
    // Made by another at once: raised where it is below ours.
    tx.execute(&raise, &[&instance, &shard, &epoch, &through, &stream])?;
    Ok(held_of(tx.query_one(&read, &[&instance, &shard])?))
}

/// A take's numbering, refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TakeRefused {
    /// The Lease's transitions it was sent with are below `seen`, the
    /// highest a take of the shard was numbered with (review c5a M2).
    Behind { seen: u64 },
    /// The Lease's transitions it was sent with are past [`EPOCH_MOST`]
    /// (the review of v31a-core-2, F1(b)): no Lease's.
    Beyond,
    /// The shard's epoch `held` is at [`EPOCH_MOST`] or past it, so no take
    /// numbers past it (the second review of v31a-core-2, finding 3): only
    /// a write stamped at the most can have put it there, and a person must
    /// reset the shard's fence before it is taken again.
    AtTheMost { held: u64 },
}

/// What a take is numbered from: the Lease as its sidecar took it, and the
/// write-ahead stream's place as the control writer found it before the
/// numbering (none where there is no stream).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Taking {
    /// The Lease's `leaseTransitions`: the epoch's floor.
    pub lease_transitions: u64,
    /// The Lease's `metadata.uid`: the floor is the one seen on it.
    pub lease_uid: String,
    pub admit: Option<Admit>,
}

/// A shard taken (`TakeShard`, in the lake's control writer's transaction):
/// its epoch numbered max(held + 1, `lease_transitions`), at least 1, kept
/// with the stream's place the control writer found (review c5a3 M1), and
/// answered; refused where `lease_transitions` is below the highest a take
/// of it was numbered with on the same Lease (M2; another Lease's uid
/// starts the floor afresh, c5a2 N5), and where it, or the epoch, would be
/// past [`EPOCH_MOST`]. The row is made first, so a first
/// take racing another waits on it (review c5a2 L2), and the transaction
/// commits on the synchronous standby (N1): the row is the shard's durable
/// epoch. Only the lake's table keeps the Lease.
pub fn take(
    tx: &mut postgres::Transaction<'_>,
    table: &'static str,
    instance: &str,
    shard: u32,
    taking: &Taking,
) -> Result<Result<u64, TakeRefused>, postgres::Error> {
    if taking.lease_transitions > EPOCH_MOST {
        return Ok(Err(TakeRefused::Beyond));
    }
    durably(tx)?;
    let shard = i64::from(shard);
    tx.execute(
        &format!(
            "INSERT INTO {table} (instance, shard, epoch) VALUES ($1, $2, 0)
             ON CONFLICT (instance, shard) DO NOTHING"
        ),
        &[&instance, &shard],
    )?;
    let row = tx.query_one(
        &format!(
            "SELECT epoch, lease_transitions, lease_uid FROM {table}
              WHERE instance = $1 AND shard = $2 FOR UPDATE"
        ),
        &[&instance, &shard],
    )?;
    let held = row.get::<_, i64>(0) as u64;
    let seen = floor(
        row.get::<_, i64>(1) as u64,
        &row.get::<_, String>(2),
        &taking.lease_uid,
    );
    if taking.lease_transitions < seen {
        return Ok(Err(TakeRefused::Behind { seen }));
    }
    let Some(epoch) = numbered(held, taking.lease_transitions) else {
        return Ok(Err(TakeRefused::AtTheMost { held }));
    };
    tx.execute(
        &format!(
            "UPDATE {table} SET epoch = $3,
                    lease_transitions = CASE WHEN lease_uid = $5
                                             THEN GREATEST(lease_transitions, $4) ELSE $4 END,
                    lease_uid = $5
              WHERE instance = $1 AND shard = $2"
        ),
        &[
            &instance,
            &shard,
            &(epoch as i64),
            &(taking.lease_transitions as i64),
            &taking.lease_uid,
        ],
    )?;
    if let Some(admit) = taking.admit {
        tx.execute(
            &place_kept(table),
            &[&instance, &shard, &(admit.through as i64), &admit.stream],
        )?;
    }
    Ok(Ok(epoch))
}

/// The stream's place kept with an epoch: raised within one stream,
/// replaced by another's ($1 instance, $2 shard, $3 through, $4 stream).
fn place_kept(table: &str) -> String {
    format!(
        "UPDATE {table} SET admit_through = CASE WHEN admit_stream = $4
                                               THEN GREATEST(admit_through, $3) ELSE $3 END,
                            admit_stream = $4
          WHERE instance = $1 AND shard = $2"
    )
}

/// The Lease transitions a take is held to: those a take was numbered with
/// on the same Lease, none on another (a Lease made again, c5a2 N5).
fn floor(seen: u64, seen_on: &str, lease_uid: &str) -> u64 {
    if seen_on == lease_uid {
        seen
    } else {
        0
    }
}

/// A take's epoch: past the one held, at least the Lease's transitions, at
/// least 1; none past [`EPOCH_MOST`], nor where the one held is at the most
/// a u64 holds.
fn numbered(held: u64, lease_transitions: u64) -> Option<u64> {
    held.checked_add(1)
        .map(|next| next.max(lease_transitions).max(1))
        .filter(|epoch| *epoch <= EPOCH_MOST)
}

/// A shard's epoch raised to `epoch` where it is below (a take heard from
/// the lake's control writer), with the write-ahead stream's place the raise
/// found (`None` where there is no stream), committed on the synchronous
/// standby (review c5a2 N2): the epoch held after.
pub fn raise_to(
    tx: &mut postgres::Transaction<'_>,
    table: &'static str,
    instance: &str,
    shard: u32,
    epoch: u64,
    admit: Option<Admit>,
) -> Result<u64, postgres::Error> {
    durably(tx)?;
    Ok(hold(tx, table, instance, shard, epoch, admit)?.epoch)
}

/// Every shard of `numbered` (each shard's epoch and place as database 0,
/// which numbers the takes, holds them) made where `table` holds none: on a
/// new database timeline, as a member starts, and so in a database added
/// later (review c5a L1). A row it holds is never raised: every raise
/// commits on the synchronous standby, so a row below database 0's is a
/// take still being raised, whose own raise keeps its place with its epoch,
/// or one that failed (review c5a3 H1, ruled 2026-11-16). Answers how many
/// were made.
pub fn seat(
    tx: &mut postgres::Transaction<'_>,
    table: &'static str,
    numbered: &BTreeMap<(String, u32), Held>,
) -> Result<usize, postgres::Error> {
    durably(tx)?;
    let mut made = 0;
    for ((instance, shard), held) in numbered {
        let changed = tx.execute(
            &format!(
                "INSERT INTO {table} (instance, shard, epoch, admit_through, admit_stream)
                 VALUES ($1, $2, $3, $4, $5) ON CONFLICT (instance, shard) DO NOTHING"
            ),
            &[
                instance,
                &i64::from(*shard),
                &(held.epoch as i64),
                &(held.admit.through as i64),
                &held.admit.stream,
            ],
        )?;
        made += changed as usize;
    }
    Ok(made)
}

/// Each shard's highest epoch among takes' records (`ShardTookOver`
/// bodies).
pub fn highest_recorded<'a>(
    bodies: impl IntoIterator<Item = &'a [u8]>,
) -> BTreeMap<(String, u32), u64> {
    use prost::Message as _;
    let mut highest: BTreeMap<(String, u32), u64> = BTreeMap::new();
    for body in bodies {
        let Ok(took) = meridian_domain::v1::ShardTookOver::decode(body) else {
            continue;
        };
        let at = highest
            .entry((took.instance, took.shard))
            .or_insert(took.epoch);
        *at = (*at).max(took.epoch);
    }
    highest
}

/// The epoch and place `table` holds of each shard of `instance` ("" for
/// every instance), by instance and shard.
pub fn held(
    client: &mut impl postgres::GenericClient,
    table: &'static str,
    instance: &str,
) -> Result<BTreeMap<(String, u32), Held>, postgres::Error> {
    Ok(client
        .query(
            &format!(
                "SELECT instance, shard, epoch, admit_through, admit_stream FROM {table}
                  WHERE $1 = '' OR instance = $1 ORDER BY instance, shard"
            ),
            &[&instance],
        )?
        .into_iter()
        .map(|row| {
            (
                (row.get::<_, String>(0), row.get::<_, i64>(1) as u32),
                Held {
                    epoch: row.get::<_, i64>(2) as u64,
                    admit: Admit {
                        through: row.get::<_, i64>(3) as u64,
                        stream: row.get::<_, i64>(4),
                    },
                },
            )
        })
        .collect())
}

/// [`place_kept`] in memory.
fn kept(held: Admit, found: Admit) -> Admit {
    if held.stream == found.stream {
        Admit {
            through: held.through.max(found.through),
            stream: found.stream,
        }
    } else {
        found
    }
}

/// The fence a store held in memory keeps: its tests', and a store with no
/// database.
#[derive(Debug, Default)]
pub struct Memory {
    held: Mutex<BTreeMap<(String, u32), Held>>,
    /// Each shard's highest Lease transitions a take was numbered with, and
    /// the Lease's uid they were seen on.
    seen: Mutex<BTreeMap<(String, u32), (String, u64)>>,
}

impl Memory {
    /// [`check`], all at once under one lock.
    pub fn check(&self, stamps: &[Option<&ShardStamp>]) -> Vec<Result<(), ShardFenced>> {
        let mut held = self.held.lock().expect("shard fence poisoned");
        for (key, epoch) in highest(within(stamps)) {
            let at = held.entry(key).or_insert(Held {
                epoch,
                admit: Admit::default(),
            });
            at.epoch = at.epoch.max(epoch);
        }
        answers(stamps, &held)
    }

    /// [`take`].
    pub fn take(&self, instance: &str, shard: u32, taking: &Taking) -> Result<u64, TakeRefused> {
        if taking.lease_transitions > EPOCH_MOST {
            return Err(TakeRefused::Beyond);
        }
        let key = (instance.to_string(), shard);
        let mut seen = self.seen.lock().expect("shard fence poisoned");
        let at_seen = seen.entry(key.clone()).or_default();
        let floor = floor(at_seen.1, &at_seen.0, &taking.lease_uid);
        if taking.lease_transitions < floor {
            return Err(TakeRefused::Behind { seen: floor });
        }
        let mut held = self.held.lock().expect("shard fence poisoned");
        let at = held.entry(key).or_default();
        at.epoch = numbered(at.epoch, taking.lease_transitions)
            .ok_or(TakeRefused::AtTheMost { held: at.epoch })?;
        *at_seen = (
            taking.lease_uid.clone(),
            floor.max(taking.lease_transitions),
        );
        if let Some(admit) = taking.admit {
            at.admit = kept(at.admit, admit);
        }
        Ok(at.epoch)
    }

    /// [`raise_to`], with no stream.
    pub fn raise_to(&self, instance: &str, shard: u32, epoch: u64) -> u64 {
        self.raise_admitting(instance, shard, epoch, Admit::default())
    }

    /// [`raise_to`].
    pub fn raise_admitting(&self, instance: &str, shard: u32, epoch: u64, admit: Admit) -> u64 {
        let mut held = self.held.lock().expect("shard fence poisoned");
        let at = held.entry((instance.to_string(), shard)).or_default();
        if at.epoch < epoch {
            at.epoch = epoch;
            at.admit = kept(at.admit, admit);
        }
        at.epoch
    }

    /// [`seat`].
    pub fn seat(&self, numbered: &BTreeMap<(String, u32), Held>) -> usize {
        let mut held = self.held.lock().expect("shard fence poisoned");
        numbered
            .iter()
            .filter(|(key, numbered)| {
                let made = !held.contains_key(*key);
                if made {
                    held.insert((*key).clone(), **numbered);
                }
                made
            })
            .count()
    }

    /// The epoch held of a shard; 0 where none.
    pub fn epoch(&self, instance: &str, shard: u32) -> u64 {
        self.held
            .lock()
            .expect("shard fence poisoned")
            .get(&(instance.to_string(), shard))
            .map_or(0, |held| held.epoch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stream's identity, and its place.
    const STREAM: i64 = 1_790_000_000_000_000_000;
    fn at(through: u64) -> Admit {
        Admit {
            through,
            stream: STREAM,
        }
    }

    fn stamp(epoch: u64) -> ShardStamp {
        ShardStamp::of("feed-1", 1, epoch).unwrap()
    }

    /// A take of the Lease `uid-1` at `transitions`.
    fn lease(transitions: u64) -> Taking {
        Taking {
            lease_transitions: transitions,
            lease_uid: "uid-1".into(),
            admit: None,
        }
    }

    /// Review c5a2 N5: a Lease made again -- another uid, its transitions
    /// from 0 -- is taken at a new epoch, never refused by the floor the
    /// old Lease's takes raised; the new Lease's takes keep their own.
    #[test]
    fn a_lease_made_again_starts_the_floor_afresh() {
        let fence = Memory::default();
        assert_eq!(fence.take("feed-1", 0, &lease(7)), Ok(7));
        let again = Taking {
            lease_uid: "uid-2".into(),
            ..lease(0)
        };
        assert_eq!(fence.take("feed-1", 0, &again), Ok(8));
        // On the new Lease, its own floor holds.
        let moved = Taking {
            lease_transitions: 2,
            ..again.clone()
        };
        assert_eq!(fence.take("feed-1", 0, &moved), Ok(9));
        assert_eq!(
            fence.take("feed-1", 0, &again),
            Err(TakeRefused::Behind { seen: 2 })
        );
    }

    /// Review c5a3 M1: the take's numbering keeps the stream's place the
    /// control writer found, so group 0 -- whose row the numbering raised --
    /// admits the backlog the stream held before the take, as every other
    /// group does; and its raise at that epoch keeps the place.
    #[test]
    fn the_numbering_keeps_the_take_s_place() {
        let fence = Memory::default();
        assert_eq!(fence.take("feed-1", 1, &lease(0)), Ok(1));
        let taking = Taking {
            admit: Some(at(105)),
            ..lease(0)
        };
        assert_eq!(fence.take("feed-1", 1, &taking), Ok(2));
        assert_eq!(fence.raise_admitting("feed-1", 1, 2, at(200)), 2);
        let backlog = stamp(1).in_stream(100, STREAM);
        assert_eq!(fence.check(&[Some(&backlog)]), vec![Ok(())]);
        let late = stamp(1).in_stream(150, STREAM);
        assert!(fence.check(&[Some(&late)])[0].is_err());
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
        assert_eq!(fence.take("feed-1", 1, &lease(0)), Ok(1));
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
        assert_eq!(fence.take("feed-1", 0, &lease(0)), Ok(1));
        assert_eq!(fence.take("feed-1", 0, &lease(0)), Ok(2));
        assert_eq!(fence.take("feed-1", 0, &lease(7)), Ok(7));
        // Below the transitions a take was numbered with: refused (M2).
        assert_eq!(
            fence.take("feed-1", 0, &lease(3)),
            Err(TakeRefused::Behind { seen: 7 })
        );
        assert_eq!(fence.take("feed-1", 0, &lease(7)), Ok(8));
        assert_eq!(fence.take("feed-1", 0, &lease(7)), Ok(9));
        assert_eq!(fence.raise_to("feed-1", 0, 5), 9);
        assert_eq!(fence.epoch("feed-1", 2), 0);
    }

    /// The review of v31a-core-2, F1(b): no epoch is numbered or raised past
    /// [`EPOCH_MOST`]. A take past it is refused and keeps no floor that
    /// refuses the Lease's next; a write stamped past it is refused and
    /// raises nothing; and at the most, a take is refused, never wrapped.
    #[test]
    fn no_epoch_is_numbered_or_raised_past_the_most() {
        let fence = Memory::default();
        assert_eq!(
            fence.take("feed-1", 1, &lease(EPOCH_MOST + 1)),
            Err(TakeRefused::Beyond)
        );
        assert_eq!(fence.take("feed-1", 1, &lease(5)), Ok(5));
        for epoch in [EPOCH_MOST + 1, 1 << 63, u64::MAX] {
            let refused = fence.check(&[Some(&stamp(epoch))]);
            assert_eq!(refused[0].as_ref().unwrap_err().held, 5, "{epoch}");
        }
        assert_eq!(fence.epoch("feed-1", 1), 5, "nothing raised it");
        assert_eq!(fence.check(&[Some(&stamp(EPOCH_MOST))]), vec![Ok(())]);
        assert_eq!(
            fence.take("feed-1", 1, &lease(5)),
            Err(TakeRefused::AtTheMost { held: EPOCH_MOST })
        );
        assert_eq!(fence.epoch("feed-1", 1), EPOCH_MOST);
        assert_eq!(numbered(u64::MAX, 0), None);
        assert_eq!(numbered(EPOCH_MOST - 1, 0), Some(EPOCH_MOST));
    }

    /// A write the stream held before the take reached the group is
    /// admitted whatever its epoch; one past it, or not from the stream, is
    /// checked (review c5a H2).
    #[test]
    fn a_write_the_stream_held_before_the_raise_is_admitted() {
        let fence = Memory::default();
        assert_eq!(fence.raise_admitting("feed-1", 1, 2, at(40)), 2);
        let old = |sequence| stamp(1).in_stream(sequence, STREAM);
        assert_eq!(fence.check(&[Some(&old(40))]), vec![Ok(())]);
        assert!(fence.check(&[Some(&old(41))])[0].is_err());
        assert!(fence.check(&[Some(&stamp(1))])[0].is_err());
        // A raise asked again keeps the place the first found.
        assert_eq!(fence.raise_admitting("feed-1", 1, 2, at(90)), 2);
        assert!(fence.check(&[Some(&old(41))])[0].is_err());
    }

    #[test]
    fn the_records_highest_epoch_is_each_shard_s() {
        use prost::Message as _;
        let took = |shard, epoch| {
            meridian_domain::v1::ShardTookOver {
                instance: "feed-1".into(),
                shard,
                epoch,
                ..Default::default()
            }
            .encode_to_vec()
        };
        let bodies = [took(0, 3), took(0, 2), took(1, 5)];
        let highest = highest_recorded(bodies.iter().map(Vec::as_slice));
        assert_eq!(highest[&("feed-1".to_string(), 0)], 3);
        assert_eq!(highest[&("feed-1".to_string(), 1)], 5);
    }

    /// Review c5a2 N4: the stream made again numbers from 1, so a place in
    /// an older stream admits nothing of the new one; and a raise from the
    /// new stream replaces the place, never raised past it by the old.
    #[test]
    fn a_place_in_another_stream_admits_nothing() {
        let fence = Memory::default();
        assert_eq!(fence.raise_admitting("feed-1", 1, 2, at(1_000_000)), 2);
        let again = STREAM + 1;
        let stale = stamp(1).in_stream(50, again);
        assert!(
            fence.check(&[Some(&stale)])[0].is_err(),
            "a write of a stream made again was admitted by the old stream's place"
        );
        let raised = Admit {
            through: 60,
            stream: again,
        };
        assert_eq!(fence.raise_admitting("feed-1", 1, 3, raised), 3);
        assert!(fence.check(&[Some(&stamp(2).in_stream(50, again))])[0].is_ok());
        assert!(fence.check(&[Some(&stamp(2).in_stream(70, again))])[0].is_err());
    }
}
