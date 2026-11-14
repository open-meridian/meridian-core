//! A book partition moved to another writer group (step 3d; spec/a-
//! deployment-scales-out-and-fails-over-by-configuration section 6.8; W6.39;
//! `MovePartition`, `PartitionMoved`, `PartitionMoveAborted` and
//! `MoveAbortCause` approved by 1057 and 2026-11-12; `writer_group` ruled
//! 2026-11-14; fixtures/book/move-book-partition.yaml).
//!
//! The control partition's group runs every move, one at a time: it holds
//! a store over every database and the control journal the records go to.
//!
//! 1. **Warm.** Within one database the target reads the same rows. Across
//!    databases the target's database first copies the partition (its
//!    entries, and every account it places), then its change log.
//! 2. **Gate.** The switch starts only while the target's lag is under
//!    `<store>.readyLagMs`; not within `moves.switchSeconds`, the move
//!    aborts, cause lag, nothing held.
//! 3. **Hold.** The switch holds the partition's lock: the source's batch in
//!    flight finishes first, and its next waits at most `moves.holdSeconds`
//!    (then OVERLOADED with a retry-after).
//! 4. **Switch.** In one transaction under that lock: the source's close at
//!    its head N, the target's digest at N compared with the source's, and
//!    `writer_group` set to the target, at the target's epoch. The held
//!    writes then find the target named and are handed to it, never
//!    fenced. Not finished within `moves.switchSeconds` of its start, it
//!    aborts, cause timeout; digests that differ abort it, cause digest,
//!    with an `IntegrityFinding`.
//!
//! The record -- `PartitionMoved` or `PartitionMoveAborted` -- is the
//! book's own act on `~control`, keyed by the move.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use meridian_changes::groups::Groups;
use meridian_domain::v1::{
    IntegrityFinding, MoveAbortCause, MovePartitionReply, MovePartitionRequest,
    PartitionMoveAborted, PartitionMoved,
};
use prost::Message;

use crate::postgres::{PostgresStore, Switched};
use crate::store::{Result, Store, StoreError};

/// The act's topic (W6.39; approved 2026-11-09).
pub const MOVE_PARTITION: &str = "platform.book.command.move-partition";

/// `moves.switchSeconds`'s default (section 6.8, Vitess's `--timeout`).
pub const SWITCH_SECONDS: u64 = 30;

/// The variable the chart sets from `moves.switchSeconds` (approved
/// 2026-11-14).
pub const SWITCH_SECONDS_VAR: &str = "MERIDIAN_MOVES_SWITCH_SECONDS";

/// CI's injected lag (approved 2026-11-14, CI only): a file whose presence
/// is a lag -- its number of milliseconds, else one past every bound.
pub const LAG_FILE_VAR: &str = "MERIDIAN_MOVE_LAG_FILE";

/// CI's injected digest mismatch (approved 2026-11-14, CI only): a file
/// whose presence makes the target's digest at the switch differ.
pub const DIGEST_FILE_VAR: &str = "MERIDIAN_MOVE_DIGEST_FILE";

/// The record kinds a move journals on `~control`.
pub const MOVED: &str = "meridian.v1.PartitionMoved";
pub const ABORTED: &str = "meridian.v1.PartitionMoveAborted";

/// A move's bounds, from the chart.
#[derive(Debug, Clone)]
pub struct Settings {
    /// `moves.switchSeconds`: the gate's and the switch's bound each.
    pub switch: Duration,
    /// `<store>.readyLagMs`: the lag the gate opens under.
    pub ready_lag: Duration,
    /// CI's injected lag, where set.
    pub lag_file: Option<PathBuf>,
    /// CI's injected digest mismatch, where set.
    pub digest_file: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            switch: Duration::from_secs(SWITCH_SECONDS),
            ready_lag: Duration::from_millis(5_000),
            lag_file: None,
            digest_file: None,
        }
    }
}

impl Settings {
    /// As the chart sets them; each default where unset.
    pub fn from_env() -> Self {
        let number = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|said| said.trim().parse::<u64>().ok())
        };
        let file = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|said| !said.trim().is_empty())
                .map(PathBuf::from)
        };
        let fallback = Self::default();
        Self {
            switch: number(SWITCH_SECONDS_VAR)
                .map(Duration::from_secs)
                .unwrap_or(fallback.switch),
            ready_lag: number("MERIDIAN_READY_LAG_MS")
                .map(Duration::from_millis)
                .unwrap_or(fallback.ready_lag),
            lag_file: file(LAG_FILE_VAR),
            digest_file: file(DIGEST_FILE_VAR),
        }
    }

    /// The lag CI injects, where its file is there.
    fn injected_lag(&self) -> Option<Duration> {
        let path = self.lag_file.as_ref()?;
        let said = std::fs::read_to_string(path).ok()?;
        Some(
            said.trim()
                .parse::<u64>()
                .map(Duration::from_millis)
                .unwrap_or(Duration::MAX),
        )
    }

    /// Whether CI makes the digests differ.
    fn digest_differs(&self) -> bool {
        self.digest_file.as_ref().is_some_and(|path| path.exists())
    }
}

/// How a move ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Ended {
    Moved(PartitionMoved),
    Aborted(PartitionMoveAborted, Option<IntegrityFinding>),
}

/// Who asked, and why: empty for a move by a changed count.
#[derive(Debug, Clone, Default)]
pub struct Asked {
    pub by_hand: bool,
    pub moved_by: String,
    pub reason: String,
}

/// The book's moves, run by the control partition's group over every
/// database's store, one at a time.
pub struct Mover {
    stores: Vec<Arc<PostgresStore>>,
    groups: Groups,
    settings: Settings,
    /// The partitions a move is under way of.
    under_way: Mutex<BTreeSet<String>>,
    /// One move at a time (section 6.8).
    one: Mutex<()>,
}

impl Mover {
    pub fn new(stores: Vec<Arc<PostgresStore>>, groups: Groups, settings: Settings) -> Self {
        assert!(!stores.is_empty(), "a book has a database");
        Self {
            stores,
            groups,
            settings,
            under_way: Mutex::default(),
            one: Mutex::new(()),
        }
    }

    fn database_of(&self, group: u32) -> &Arc<PostgresStore> {
        &self.stores
            [meridian_changes::databases::of_group(group, self.stores.len() as u32) as usize]
    }

    /// The group that owns `partition` now, as its source database's head
    /// names it.
    fn owner_now(&self, partition: &str) -> Result<u32> {
        let guess = self.groups.owner(partition);
        let tip = self.database_of(guess).tip(partition)?;
        let named = tip.and_then(|tip| tip.writer_group);
        if let Some(group) = named.filter(|group| *group < self.groups.count()) {
            self.groups.assign(partition, Some(group));
            return Ok(group);
        }
        Ok(guess)
    }

    /// The move's request checked (fixtures/book/move-book-partition.yaml):
    /// answers the group that owns the partition now.
    pub fn check(&self, request: &MovePartitionRequest) -> Result<u32> {
        let partition = request.partition.as_str();
        let count = self.stores[0].partition_count().max(1);
        if partition == crate::store::CONTROL_PARTITION
            || !(0..count).any(|n| crate::store::partition_named(n) == partition)
        {
            return Err(invalid(
                "partition",
                format!(
                    "the book has no partition {partition:?} to move: P0 to P{}",
                    count - 1
                ),
            ));
        }
        if request.target_group >= self.groups.count() {
            return Err(invalid(
                "target_group",
                format!(
                    "the book has {} writer group(s), numbered from 0: there is no group {}",
                    self.groups.count(),
                    request.target_group
                ),
            ));
        }
        let from = self.owner_now(partition)?;
        if from == request.target_group {
            return Err(invalid(
                "target_group",
                format!("writer group {from} owns {partition} already"),
            ));
        }
        if self
            .under_way
            .lock()
            .expect("moves poisoned")
            .contains(partition)
        {
            return Err(StoreError::Precondition {
                field: "partition",
                words: format!("a move of {partition} is under way: ask again once it ends"),
            });
        }
        Ok(from)
    }

    /// `partition` marked as under way: refused another move until
    /// [`Mover::run`] ends.
    pub fn begin(&self, partition: &str) -> bool {
        self.under_way
            .lock()
            .expect("moves poisoned")
            .insert(partition.to_string())
    }

    /// The move, start to end, after [`Mover::check`] and [`Mover::begin`]:
    /// how it ended, its record made, not yet journalled.
    pub fn run(&self, partition: &str, target: u32, asked: &Asked) -> Result<Ended> {
        let _alone = self.one.lock().expect("moves poisoned");
        let ended = self.run_alone(partition, target, asked);
        self.under_way
            .lock()
            .expect("moves poisoned")
            .remove(partition);
        ended
    }

    fn run_alone(&self, partition: &str, target: u32, asked: &Asked) -> Result<Ended> {
        let from = self.owner_now(partition)?;
        let (source, to) = (self.database_of(from), self.database_of(target));
        let across = !Arc::ptr_eq(source, to);
        let aborted = |cause: MoveAbortCause, finding: Option<IntegrityFinding>| {
            Ended::Aborted(
                PartitionMoveAborted {
                    component: "bor".into(),
                    partition: partition.to_string(),
                    target_group: target,
                    account_id: String::new(),
                    target_partition: String::new(),
                    cause: cause as i32,
                    aborted_at_ns: now_ns(),
                },
                finding,
            )
        };
        // Warm: across databases, the partition copied, then its change log.
        let mut copied = if across {
            to.copy_partition_from(source, partition, 0)?
        } else {
            0
        };
        // Gate: the target's lag under readyLagMs within switchSeconds.
        let gate = Instant::now();
        loop {
            let lag = match self.settings.injected_lag() {
                Some(lag) => lag,
                None if !across => Duration::ZERO,
                None => {
                    let (s, t) = (
                        source.tip(partition)?.unwrap_or_default(),
                        to.tip(partition)?.unwrap_or_default(),
                    );
                    if t.held >= s.held {
                        Duration::ZERO
                    } else {
                        Duration::from_nanos((s.held_at_ns - t.held_at_ns).max(1) as u64)
                    }
                }
            };
            if lag < self.settings.ready_lag {
                break;
            }
            if gate.elapsed() >= self.settings.switch {
                tracing::warn!(
                    partition,
                    target,
                    lag_ms = lag.as_millis() as u64,
                    "a partition's move did not switch: its target's lag stayed above readyLagMs"
                );
                return Ok(aborted(MoveAbortCause::Lag, None));
            }
            if across {
                copied = to.copy_partition_from(source, partition, copied)?;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // Hold and switch, under the partition's lock.
        let switched = source.switch_partition(
            to,
            partition,
            target,
            self.settings.switch,
            self.settings.digest_differs(),
        )?;
        match switched {
            Switched::Moved { closing, epoch } => {
                if across {
                    let gone = source.forget_partition(partition)?;
                    tracing::info!(
                        partition,
                        accounts = gone,
                        "a partition moved out of a database: its projections let go, its entries kept"
                    );
                }
                tracing::info!(
                    partition,
                    from,
                    target,
                    closing,
                    epoch,
                    "a partition moved to another writer group"
                );
                Ok(Ended::Moved(PartitionMoved {
                    component: "bor".into(),
                    partition: partition.to_string(),
                    from_group: from,
                    to_group: target,
                    epoch,
                    by_hand: asked.by_hand,
                    moved_by: asked.moved_by.clone(),
                    reason: asked.reason.clone(),
                    moved_at_ns: now_ns(),
                }))
            }
            Switched::TimedOut => Ok(aborted(MoveAbortCause::Timeout, None)),
            Switched::DigestDiffers {
                closing,
                source: theirs,
                target: ours,
                at_ns,
            } => {
                let found = meridian_changes::integrity::Finding {
                    partition: partition.to_string(),
                    sequence: closing,
                    committed_at_ns: at_ns,
                    what: format!(
                        "a move of {partition} to writer group {target}: the target's digest at \
                         sequence {closing} differs from the source's ({} against {})",
                        short(&ours),
                        short(&theirs)
                    ),
                };
                let mut finding = meridian_changes::records::finding(
                    "bor",
                    meridian_domain::v1::IntegrityCheck::MemberDigest,
                    &found,
                    now_ns(),
                );
                finding.check = meridian_domain::v1::IntegrityCheck::MemberDigest as i32;
                Ok(aborted(MoveAbortCause::Digest, Some(finding)))
            }
        }
    }
}

/// The deployment's time; the system's where no clock is set (a tool's, a
/// test's).
fn now_ns() -> i64 {
    match meridian_changes::now_ns() {
        0 => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos() as i64),
        now => now,
    }
}

fn short(digest: &[u8]) -> String {
    digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

fn invalid(field: &str, words: String) -> StoreError {
    StoreError::Malformed {
        fields: vec![field.to_string()],
        words,
    }
}

/// How a move ended journalled on `~control`, the book's own act, each
/// record once by its key.
pub fn journal(bus: &meridian_bus::Bus, store: &dyn Store, ended: &Ended) {
    let mut owed = Vec::new();
    match ended {
        Ended::Moved(moved) => owed.push(crate::control::Owed {
            key: format!(
                "control-record:partition-moved:{}:{}",
                moved.partition, moved.epoch
            ),
            kind: MOVED,
            body: moved.encode_to_vec(),
            reason: format!(
                "partition {} moved from writer group {} to {}",
                moved.partition, moved.from_group, moved.to_group
            ),
        }),
        Ended::Aborted(aborted, finding) => {
            owed.push(crate::control::Owed {
                key: format!(
                    "control-record:partition-move-aborted:{}:{}",
                    aborted.partition, aborted.aborted_at_ns
                ),
                kind: ABORTED,
                body: aborted.encode_to_vec(),
                reason: format!(
                    "a move of partition {} to writer group {} did not switch",
                    aborted.partition, aborted.target_group
                ),
            });
            if let Some(finding) = finding {
                owed.push(crate::control::Owed {
                    key: format!(
                        "control-record:finding:move:{}:{}",
                        finding.partition, aborted.aborted_at_ns
                    ),
                    kind: meridian_changes::records::FINDING,
                    body: finding.encode_to_vec(),
                    reason: format!("an integrity finding: {}", finding.detail),
                });
            }
        }
    }
    for record in &owed {
        if let Err(failed) = crate::control::journal_one(bus, store, record) {
            tracing::warn!(key = record.key, %failed, "a move's record was not journalled on control");
        }
    }
}

/// Serve [`MOVE_PARTITION`] (the control partition's group's member):
/// checked and answered at once, then run in the background, its record
/// journalled when it ends.
pub fn serve(bus: &Arc<meridian_bus::Bus>, store: &Arc<dyn Store>, mover: &Arc<Mover>) {
    let (bus2, store, mover) = (bus.clone(), store.clone(), mover.clone());
    bus.serve(MOVE_PARTITION, move |envelope| {
        let request: MovePartitionRequest =
            crate::service::decode(&envelope, "meridian.v1.MovePartitionRequest")?;
        let meta = envelope.meta.clone().unwrap_or_default();
        if meta.acting_for_subject.is_empty() {
            return Err(meridian_bus::refusal_with_status(
                meridian_bus::RefusalStatus::PermissionDenied,
                &[],
                "a move is a deployment admin's to make, and this is sent for nobody",
            ));
        }
        if request.reason.trim().is_empty() {
            return Err(meridian_bus::refusal_naming(
                meridian_pb::v1::RefusalReason::ReasonRequired as i32,
                &["reason".to_string()],
                "reason: a move says why",
            ));
        }
        if request.reason.chars().count() > 2_000 {
            return Err(meridian_bus::refusal_with_status(
                meridian_bus::RefusalStatus::InvalidArgument,
                &["reason".to_string()],
                "reason: at most 2,000 characters",
            ));
        }
        let from = mover
            .check(&request)
            .map_err(|failed| failed.on_the_bus())?;
        if !mover.begin(&request.partition) {
            return Err(StoreError::Precondition {
                field: "partition",
                words: format!(
                    "a move of {} is under way: ask again once it ends",
                    request.partition
                ),
            }
            .on_the_bus());
        }
        let started_at_ns = bus2.clock().now_ns();
        let asked = Asked {
            by_hand: true,
            moved_by: meta.acting_for_subject.clone(),
            reason: request.reason.clone(),
        };
        let (bus3, store, mover) = (bus2.clone(), store.clone(), mover.clone());
        std::thread::spawn(move || {
            match mover.run(&request.partition, request.target_group, &asked) {
                Ok(ended) => journal(&bus3, store.as_ref(), &ended),
                Err(failed) => tracing::warn!(
                    partition = request.partition,
                    %failed,
                    "a partition's move stopped: the source still owns it"
                ),
            }
        });
        Ok((
            "meridian.v1.MovePartitionReply".to_string(),
            MovePartitionReply {
                from_group: from,
                started_at_ns,
            }
            .encode_to_vec(),
        ))
    });
}

/// The moves a changed count of writer groups calls for (section 6.8,
/// Triggers; question 10): each partition whose head names another group
/// than its rendezvous owner at the count now, moved to it one at a time,
/// warm-up first, by_hand false; each record journalled. A partition a move
/// is under way of is left to it. Says what each move ended as.
pub fn rebalance(bus: &meridian_bus::Bus, store: &dyn Store, mover: &Mover) -> Vec<Ended> {
    let count = mover.stores[0].partition_count().max(1);
    let mut ended = Vec::new();
    for n in 0..count {
        let partition = crate::store::partition_named(n);
        let wanted = meridian_changes::groups::owner(&partition, mover.groups.count());
        let now = match mover.owner_now(&partition) {
            Ok(now) => now,
            Err(failed) => {
                tracing::warn!(partition, %failed, "a partition's owner was not read");
                continue;
            }
        };
        if now == wanted || !mover.begin(&partition) {
            continue;
        }
        match mover.run(&partition, wanted, &Asked::default()) {
            Ok(done) => {
                journal(bus, store, &done);
                ended.push(done);
            }
            Err(failed) => tracing::warn!(
                partition,
                %failed,
                "a partition's move by the changed count stopped: the source still owns it"
            ),
        }
    }
    ended
}
