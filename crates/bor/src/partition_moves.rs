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

pub use meridian_changes::moves::{
    Asked, Settings, DIGEST_FILE_VAR, LAG_FILE_VAR, SWITCH_SECONDS, SWITCH_SECONDS_VAR,
};

/// The record kinds a move journals on `~control`.
pub const MOVED: &str = "meridian.v1.PartitionMoved";
pub const ABORTED: &str = "meridian.v1.PartitionMoveAborted";

/// How a move ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Ended {
    Moved(PartitionMoved),
    Aborted(PartitionMoveAborted, Option<IntegrityFinding>),
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
    /// The deployment's clock, its records' times.
    clock: Arc<dyn meridian_clock::Clock>,
    /// Where each move's record is journalled before the move is let go of
    /// (the review's C8), where set.
    journalling: Option<(Arc<meridian_bus::Bus>, Arc<dyn Store>)>,
    /// The revision this member runs and how recently a member must have
    /// been seen to be waited on (step 3d; ruled 2026-11-15): a move home
    /// by a raised database count waits until every member seen within
    /// that long runs it. None set, nothing is waited on.
    rollout: Option<(String, std::time::Duration)>,
}

impl Mover {
    pub fn new(
        stores: Vec<Arc<PostgresStore>>,
        groups: Groups,
        settings: Settings,
        clock: Arc<dyn meridian_clock::Clock>,
    ) -> Self {
        assert!(!stores.is_empty(), "a book has a database");
        Self {
            stores,
            groups,
            settings,
            under_way: Mutex::default(),
            one: Mutex::new(()),
            clock,
            journalling: None,
            rollout: None,
        }
    }

    /// Each move's record journalled through `bus` on `store`'s `~control`
    /// as it ends, before the partition is free to move again (the review's
    /// C8: never a window where a sweep finds a move without its record).
    pub fn journalling(mut self, bus: Arc<meridian_bus::Bus>, store: Arc<dyn Store>) -> Self {
        self.journalling = Some((bus, store));
        self
    }

    /// A partition is moved home to another database by a raised count only
    /// once every member of the book seen within `within` runs `revision`,
    /// this member's own (step 3d; ruled 2026-11-15, item 9).
    pub fn waiting_for_rollout(mut self, revision: String, within: std::time::Duration) -> Self {
        self.rollout = Some((revision, within));
        self
    }

    /// Whether the book's rollout is complete: no member seen lately runs
    /// another revision than this one. Not known -- its members not read --
    /// it is not.
    fn rolled_out(&self, partition: &str) -> bool {
        let Some((revision, within)) = &self.rollout else {
            return true;
        };
        match self.stores[0].members_behind(revision, self.clock.now_ns(), within.as_nanos() as i64)
        {
            Ok(behind) if behind.is_empty() => true,
            Ok(behind) => {
                tracing::info!(
                    partition,
                    revision,
                    behind = behind.len(),
                    member = behind[0].member,
                    runs = behind[0].revision,
                    "a partition is not moved home yet: a member seen lately runs another \
                     revision, and reads where the partition is now"
                );
                false
            }
            Err(failed) => {
                tracing::warn!(partition, %failed, "the members were not read: no partition moved home");
                false
            }
        }
    }

    /// A partition's move by hand to another database waits here until the
    /// book's rollout is complete (step 3d; ruled 2026-11-15): its reply
    /// said the move had begun, and it runs once no member seen lately runs
    /// another revision -- one that reads fewer databases would answer for
    /// the partition where it was as though it held nothing. Said once.
    fn wait_for_rollout(&self, partition: &str) {
        let Some((revision, within)) = &self.rollout else {
            return;
        };
        let mut said = false;
        loop {
            let behind = self.stores[0].members_behind(
                revision,
                self.clock.now_ns(),
                within.as_nanos() as i64,
            );
            match behind {
                Ok(behind) if behind.is_empty() => return,
                Ok(behind) if !said => tracing::info!(
                    partition,
                    revision,
                    member = behind[0].member,
                    runs = behind[0].revision,
                    "a partition's move to another database waits for the rollout: a member \
                     seen lately runs another revision"
                ),
                Err(failed) if !said => tracing::warn!(
                    partition,
                    %failed,
                    "a partition's move to another database waits: the members were not read"
                ),
                _ => {}
            }
            said = true;
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    fn database_of(&self, group: u32) -> &Arc<PostgresStore> {
        &self.stores
            [meridian_changes::databases::of_group(group, self.stores.len() as u32) as usize]
    }

    /// The database `partition` is to be opened in its owner's home from:
    /// another holding it further on than home, named its owner -- the
    /// database count was raised since it was written, or a move between
    /// databases let it go there and its target's open is owed (the
    /// review's C3) -- or home itself, where its head there is a move's
    /// pointer to its owner level with every other (step 3d; section 6.8).
    fn misplaced(&self, partition: &str, owner: u32) -> Result<Option<usize>> {
        let home = meridian_changes::databases::of_group(owner, self.stores.len() as u32) as usize;
        let at_home = self.stores[home].tip(partition)?;
        let here = at_home.as_ref().map_or(0, |tip| tip.head);
        let named = |tip: &crate::postgres::Tip| {
            tip.writer_group
                .unwrap_or_else(|| meridian_changes::groups::owner(partition, self.groups.count()))
        };
        for (d, store) in self.stores.iter().enumerate() {
            if d == home {
                continue;
            }
            let Some(tip) = store.tip(partition)? else {
                continue;
            };
            if named(&tip) == owner && tip.head > here {
                return Ok(Some(d));
            }
        }
        Ok(at_home
            .filter(|tip| tip.epoch == meridian_changes::moves::POINTER && named(tip) == owner)
            .map(|_| home))
    }

    /// `partition`'s accounts let go in each database whose head of it is
    /// a move's pointer, once its owner's home has opened it: what a move
    /// copied there before an abort (the review's M4), or left behind
    /// before its source forgot them (C5). Says how many accounts.
    fn let_go_copies(&self, partition: &str) -> Result<usize> {
        let owner = self.owner_now(partition)?;
        let home = meridian_changes::databases::of_group(owner, self.stores.len() as u32) as usize;
        let opened = self.stores[home]
            .tip(partition)?
            .filter(|tip| tip.epoch != meridian_changes::moves::POINTER)
            .map(|tip| tip.head);
        let Some(opened) = opened else {
            return Ok(0);
        };
        let mut gone = 0;
        for (d, store) in self.stores.iter().enumerate() {
            if d == home {
                continue;
            }
            let pointer = store.tip(partition)?.is_some_and(|tip| {
                tip.epoch == meridian_changes::moves::POINTER && tip.head <= opened
            });
            if pointer {
                gone += store.forget_partition(partition)?;
            }
        }
        Ok(gone)
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
        // To another database than the one it is written in: once every
        // member reads both.
        if self.rollout.is_some() && self.stores.len() > 1 {
            let from = self.owner_now(partition)?;
            let written = match self.misplaced(partition, from)? {
                Some(d) => &self.stores[d],
                None => self.database_of(from),
            };
            if !Arc::ptr_eq(written, self.database_of(target)) {
                self.wait_for_rollout(partition);
            }
        }
        self.run_from(partition, target, asked, None)
    }

    /// [`Self::run`], from database `from_database` where its records are
    /// there rather than in its owner's (a database count raised: a move to
    /// the owner's own database, its group unchanged).
    fn run_from(
        &self,
        partition: &str,
        target: u32,
        asked: &Asked,
        from_database: Option<usize>,
    ) -> Result<Ended> {
        let _alone = self.one.lock().expect("moves poisoned");
        let ended = self.run_alone(partition, target, asked, from_database);
        if let (Ok(done), Some((bus, store))) = (&ended, &self.journalling) {
            journal(bus, store.as_ref(), done);
        }
        self.under_way
            .lock()
            .expect("moves poisoned")
            .remove(partition);
        ended
    }

    fn run_alone(
        &self,
        partition: &str,
        target: u32,
        asked: &Asked,
        from_database: Option<usize>,
    ) -> Result<Ended> {
        let from = self.owner_now(partition)?;
        let source = from_database.map_or_else(|| self.database_of(from), |d| &self.stores[d]);
        let to = self.database_of(target);
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
                    aborted_at_ns: self.clock.now_ns(),
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
                    // Against the copy's own watermark, never the most the
                    // target holds (the review's C1).
                    let s = source.tip(partition)?.unwrap_or_default();
                    if copied >= s.held {
                        Duration::ZERO
                    } else {
                        let t = to.held_at(partition, copied)?;
                        Duration::from_nanos((s.held_at_ns - t).max(1) as u64)
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
            copied,
        );
        let switched = match switched {
            Ok(switched) => switched,
            Err(failed) => {
                if across {
                    self.let_go_aborted(to, partition);
                }
                return Err(failed);
            }
        };
        if across && !matches!(switched, Switched::Moved { .. } | Switched::LetGo { .. }) {
            self.let_go_aborted(to, partition);
        }
        match switched {
            Switched::Moved { closing, epoch } | Switched::LetGo { closing, epoch } => {
                if across && matches!(switched, Switched::Moved { .. }) {
                    // Moved, whatever comes of this: never an error that
                    // reads as the source still owning (the review's C8).
                    match source.forget_partition(partition) {
                        Ok(gone) => tracing::info!(
                            partition,
                            accounts = gone,
                            "a partition moved out of a database: its projections let go, its entries kept"
                        ),
                        Err(failed) => tracing::warn!(
                            partition,
                            %failed,
                            "a partition moved out of a database: its projections are let go on the sweep"
                        ),
                    }
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
                    moved_at_ns: self.clock.now_ns(),
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
                        meridian_changes::moves::short(&ours),
                        meridian_changes::moves::short(&theirs)
                    ),
                };
                let mut finding = meridian_changes::records::finding(
                    "bor",
                    meridian_domain::v1::IntegrityCheck::MemberDigest,
                    &found,
                    self.clock.now_ns(),
                );
                finding.check = meridian_domain::v1::IntegrityCheck::MemberDigest as i32;
                Ok(aborted(MoveAbortCause::Digest, Some(finding)))
            }
        }
    }
}

impl Mover {
    /// What an aborted move copied into `to` of `partition`'s accounts let
    /// go there, where its head is not written (the review's M4, C5).
    fn let_go_aborted(&self, to: &PostgresStore, partition: &str) {
        let written = to.tip(partition).ok().flatten().is_some_and(|tip| {
            tip.epoch != meridian_changes::moves::POINTER
                && tip.writer_group == Some(self.owner_now(partition).unwrap_or(u32::MAX))
        });
        if written {
            return;
        }
        if let Err(failed) = to.forget_partition(partition) {
            tracing::warn!(partition, %failed, "an aborted move's copies are let go on the sweep");
        }
    }
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
            // Keyed by when it moved, so a move back and forth is recorded
            // each time (the review's C8).
            key: format!(
                "control-record:partition-moved:{}:{}",
                moved.partition, moved.moved_at_ns
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
    let at_ns = match ended {
        Ended::Moved(moved) => moved.moved_at_ns,
        Ended::Aborted(aborted, _) => aborted.aborted_at_ns,
    };
    for record in &owed {
        match crate::control::journal_one(bus, store, record) {
            Ok(_) => {}
            // A member of a group past the first (an account's move aborted
            // there): kept where it writes, for the control partition's
            // group to journal (step 3d; `book_control_owed`).
            Err(StoreError::NotOwned { .. }) => {
                if let Err(failed) = store.owe_control(record, at_ns) {
                    tracing::warn!(key = record.key, %failed, "a move's record was not kept for control");
                }
            }
            Err(failed) => {
                tracing::warn!(key = record.key, %failed, "a move's record was not journalled on control")
            }
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
                Ok(_) if mover.journalling.is_some() => {}
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
    // Each head made since the start and not named yet -- a raised
    // `bor.partitions`, made by a member after this group's start -- named
    // its rendezvous owner at the count now, so a write handed for it is
    // handed on by the record's word, never refused as a guess (CI
    // 43636763392: "handed to this writer group, which does not own its
    // partition").
    for database in &mover.stores {
        if let Err(failed) = database.pin_owners() {
            tracing::warn!(%failed, "the partitions not named yet were not named");
        }
    }
    for n in 0..count {
        let partition = crate::store::partition_named(n);
        if !mover.begin(&partition) {
            continue;
        }
        // Decided and run under the one-at-a-time lock, the moves by hand
        // read then, so a move by hand that ended a moment ago is never
        // undone (the review's C8).
        let alone = mover.one.lock().expect("moves poisoned");
        let done = rebalance_one(mover, &partition);
        if let Some(Ok(done)) = &done {
            journal(bus, store, done);
        }
        drop(alone);
        mover
            .under_way
            .lock()
            .expect("moves poisoned")
            .remove(&partition);
        match done {
            Some(Ok(done)) => ended.push(done),
            Some(Err(failed)) => tracing::warn!(
                partition,
                %failed,
                "a partition's move by the changed count stopped: the source still owns it"
            ),
            None => {}
        }
        if let Err(failed) = mover.let_go_copies(&partition) {
            tracing::warn!(partition, %failed, "a moved partition's copies were not let go");
        }
    }
    // Each account's move closed and never opened, opened: a close older
    // than two switches' bound, so a move under way finishes its own.
    let before = mover.clock.now_ns() - 2 * mover.settings.switch.as_nanos() as i64;
    crate::moves::finish_opens(bus, store, &mover.stores, &mover.settings, before);
    ended
}

/// `partition` moved where the count now places it, or opened in its
/// owner's home: its move's ending, or none where it stays.
fn rebalance_one(mover: &Mover, partition: &str) -> Option<Result<Ended>> {
    // Where a deployment admin moved a partition by hand, it stays (section
    // 6.8): only a count lowered past its group moves it again.
    let by_hand = match mover.stores[0].moved_by_hand() {
        Ok(by_hand) => by_hand,
        Err(failed) => {
            tracing::warn!(%failed, "the moves by hand were not read: no partition moved by the count");
            return None;
        }
    };
    let wanted = meridian_changes::groups::owner(partition, mover.groups.count());
    let now = match mover.owner_now(partition) {
        Ok(now) => now,
        Err(failed) => {
            tracing::warn!(partition, %failed, "a partition's owner was not read");
            return None;
        }
    };
    let placed = by_hand
        .get(partition)
        .is_some_and(|group| *group == now && *group < mover.groups.count());
    // Its owner kept, its records in another database than its owner's (a
    // raised database count, or a move's open owed): moved there, its
    // group unchanged.
    let away = match mover.misplaced(partition, now) {
        Ok(away) => away,
        Err(failed) => {
            tracing::warn!(partition, %failed, "a partition's databases were not read");
            return None;
        }
    };
    if (now == wanted || placed) && away.is_none() {
        return None;
    }
    // Home from a database it is still written in -- the count of databases
    // raised -- only once every member reads both (ruled 2026-11-15). A
    // move that let it go there already, its open owed, is finished.
    if let Some(d) = away {
        let written_there = mover.stores[d]
            .tip(partition)
            .ok()
            .flatten()
            .is_some_and(|tip| tip.epoch != meridian_changes::moves::POINTER);
        if written_there && !mover.rolled_out(partition) {
            return None;
        }
    }
    let target = if away.is_some() { now } else { wanted };
    Some(mover.run_alone(partition, target, &Asked::default(), away))
}
