//! A lake range moved to another writer group (step 3d; spec/a-deployment-
//! scales-out-and-fails-over-by-configuration section 6.8; W6.39;
//! `MovePartition`, `PartitionMoved`, `PartitionMoveAborted` and
//! `MoveAbortCause` approved by 1057 and 2026-11-12; `writer_group` ruled
//! 2026-11-14; fixtures/lake/move-lake-partition.yaml). A dataset's full
//! range moves whole; once split, each child range moves on its own
//! (ruling 2 of 2026-11-14; the fixture's case "a split range").
//!
//! The control partition's group runs every move, one at a time, over every
//! database's store, and journals its record in the lake's `control`:
//! 1. **Warm.** Within one database the target reads the same rows.
//! 2. **Gate.** The switch starts only while the target's lag is under
//!    `lake.readyLagMs`; not within `moves.switchSeconds`, the move aborts,
//!    cause lag, nothing held.
//! 3. **Hold.** The switch holds the range's row: the source's batch in
//!    flight finishes first, and a batch after it leaves the range's parts
//!    to wait -- at most `moves.holdSeconds` -- while its sibling ranges'
//!    parts commit.
//! 4. **Switch.** Under that row: the source's close at its head N, the
//!    digests at N compared, the row named the target's at its group's
//!    epoch. The held parts then find the target named and are handed to
//!    it, never fenced.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use meridian_changes::groups::Groups;
pub use meridian_changes::moves::{Asked, Settings};
use meridian_domain::v1::{
    IntegrityFinding, MoveAbortCause, MovePartitionReply, MovePartitionRequest,
    PartitionMoveAborted, PartitionMoved,
};
use prost::Message;

use crate::postgres::{PostgresStore, Switched};
use crate::store::{Result, StoreError};

/// The act's topic (W6.39; approved 2026-11-09).
pub const MOVE_PARTITION: &str = "platform.lake.command.move-partition";

/// The record kinds a move logs in the lake's `control`.
pub const MOVED: &str = "meridian.v1.PartitionMoved";
pub const ABORTED: &str = "meridian.v1.PartitionMoveAborted";

/// How a move ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Ended {
    Moved(PartitionMoved),
    Aborted(PartitionMoveAborted, Option<IntegrityFinding>),
}

/// A move's request refused: its status, the field it names, its words.
#[derive(Debug, Clone, PartialEq)]
pub struct Refused {
    pub status: meridian_bus::RefusalStatus,
    pub field: &'static str,
    pub words: String,
}

impl Refused {
    /// As the bus says it.
    pub fn on_the_bus(&self) -> String {
        meridian_bus::refusal_with_status(self.status, &[self.field.to_string()], &self.words)
    }
}

/// The lake's moves, run by the control partition's group over every
/// database's store, one at a time.
pub struct Mover {
    stores: Vec<Arc<PostgresStore>>,
    groups: Groups,
    settings: Settings,
    /// The datasets a move is under way in.
    under_way: Mutex<BTreeSet<String>>,
    /// One move at a time (section 6.8).
    one: Mutex<()>,
    /// The deployment's clock, its records' times.
    clock: Arc<dyn meridian_clock::Clock>,
    /// Whether each move's record is logged as it ends, before its dataset
    /// is free to move again (the review's C8).
    logs: bool,
    /// The revision this member runs and how recently a member must have
    /// been seen to be waited on (step 3d; ruled 2026-11-15): a move home
    /// by a raised database count waits until every member seen within
    /// that long runs it. None set, nothing is waited on.
    rollout: Option<(String, std::time::Duration)>,
}

impl Mover {
    /// Each move's record logged in the lake's control as it ends, before
    /// its dataset is free to move again (the review's C8).
    pub fn logging(mut self) -> Self {
        self.logs = true;
        self
    }

    pub fn new(
        stores: Vec<Arc<PostgresStore>>,
        groups: Groups,
        settings: Settings,
        clock: Arc<dyn meridian_clock::Clock>,
    ) -> Self {
        assert!(!stores.is_empty(), "a lake has a database");
        Self {
            stores,
            groups,
            settings,
            under_way: Mutex::default(),
            one: Mutex::new(()),
            clock,
            logs: false,
            rollout: None,
        }
    }

    /// A range is moved home to another database by a raised count only
    /// once every member of the lake seen within `within` runs `revision`,
    /// this member's own (step 3d; ruled 2026-11-15, item 9).
    pub fn waiting_for_rollout(mut self, revision: String, within: std::time::Duration) -> Self {
        self.rollout = Some((revision, within));
        self
    }

    /// Whether the lake's rollout is complete: no member seen lately runs
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
                    "a range is not moved home yet: a member seen lately runs another \
                     revision, and reads where the range is now"
                );
                false
            }
            Err(failed) => {
                tracing::warn!(partition, %failed, "the members were not read: no range moved home");
                false
            }
        }
    }

    fn database_of(&self, group: u32) -> &Arc<PostgresStore> {
        &self.stores
            [meridian_changes::databases::of_group(group, self.stores.len() as u32) as usize]
    }

    /// The group that owns `partition` now, as its database's row names
    /// it, and where it stands there; none where no database holds it.
    /// A range whose owner's database holds none of it, held in another's
    /// named its owner's (a database count raised; step 3d, section 6.8):
    /// its owner and that database.
    fn misplaced(&self, partition: &str) -> Result<Option<(u32, usize)>> {
        let dataset = crate::range::dataset_of(partition);
        for (d, store) in self.stores.iter().enumerate() {
            let Some(tip) = store.range_tip(partition)? else {
                continue;
            };
            let owner = tip
                .writer_group
                .filter(|group| *group < self.groups.count())
                .unwrap_or_else(|| meridian_changes::groups::owner(dataset, self.groups.count()));
            // Further on than at its owner's home: once moved home, the
            // home holds it level, and it is not moved again. Or level with
            // a home that is a move's pointer to it: its open owed (the
            // review's C3).
            let home = self.database_of(owner);
            let at_home = home.range_tip(partition)?;
            let pointed = at_home
                .as_ref()
                .is_some_and(|at| at.epoch == meridian_changes::moves::POINTER);
            if !Arc::ptr_eq(store, home)
                && !tip.closed
                && (at_home.as_ref().map_or(0, |at| at.head) < tip.head
                    || (pointed && at_home.as_ref().is_some_and(|at| at.head <= tip.head)))
            {
                return Ok(Some((owner, d)));
            }
        }
        Ok(None)
    }

    fn owner_now(&self, partition: &str) -> Result<Option<(u32, crate::postgres::RangeTip)>> {
        let dataset = crate::range::dataset_of(partition);
        let guess = self.groups.owner_by(partition, dataset);
        let mut asked = vec![self.database_of(guess)];
        asked.extend(self.stores.iter());
        for store in asked {
            let Some(tip) = store.range_tip(partition)? else {
                continue;
            };
            self.groups.assign(partition, tip.writer_group);
            let owner = tip
                .writer_group
                .filter(|group| *group < self.groups.count())
                .unwrap_or_else(|| meridian_changes::groups::owner(dataset, self.groups.count()));
            if Arc::ptr_eq(store, self.database_of(owner)) {
                return Ok(Some((owner, tip)));
            }
        }
        Ok(None)
    }

    /// The move's request checked (fixtures/lake/move-lake-partition.yaml):
    /// answers the group that owns the range now.
    pub fn check(&self, request: &MovePartitionRequest) -> std::result::Result<u32, Refused> {
        let partition = request.partition.as_str();
        let invalid = |field: &'static str, words: String| Refused {
            status: meridian_bus::RefusalStatus::InvalidArgument,
            field,
            words,
        };
        if partition == meridian_changes::CONTROL {
            return Err(invalid(
                "partition",
                "the lake's control partition is group 0's, always: it is never moved".into(),
            ));
        }
        if request.target_group >= self.groups.count() {
            return Err(invalid(
                "target_group",
                format!(
                    "the lake has {} writer group(s), numbered from 0: there is no group {}",
                    self.groups.count(),
                    request.target_group
                ),
            ));
        }
        let unavailable = |failed: StoreError| Refused {
            status: meridian_bus::RefusalStatus::Unavailable,
            field: "partition",
            words: failed.to_string(),
        };
        let Some((from, tip)) = self.owner_now(partition).map_err(unavailable)? else {
            return Err(invalid(
                "partition",
                format!(
                    "the lake has no range {partition:?}: a dataset's ID, or one of its ranges"
                ),
            ));
        };
        if tip.closed {
            return Err(invalid(
                "partition",
                format!("{partition} was split: each of its ranges moves on its own"),
            ));
        }
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
            .contains(crate::range::dataset_of(partition))
        {
            return Err(Refused {
                status: meridian_bus::RefusalStatus::FailedPrecondition,
                field: "partition",
                words: format!(
                    "a move in dataset {} is under way: ask again once it ends",
                    crate::range::dataset_of(partition)
                ),
            });
        }
        Ok(from)
    }

    /// `partition`'s dataset marked as under way: refused another move
    /// until [`Mover::run`] ends.
    pub fn begin(&self, partition: &str) -> bool {
        self.under_way
            .lock()
            .expect("moves poisoned")
            .insert(crate::range::dataset_of(partition).to_string())
    }

    /// The move, start to end, after [`Mover::check`] and [`Mover::begin`]:
    /// how it ended, its record made, not yet logged.
    pub fn run(&self, partition: &str, target: u32, asked: &Asked) -> Result<Ended> {
        self.run_from(partition, target, asked, None)
    }

    /// [`Self::run`], from `away` -- the group and database whose rows hold
    /// the range where its owner's database holds none of it (a database
    /// count raised: a move to the owner's own database, its group
    /// unchanged).
    fn run_from(
        &self,
        partition: &str,
        target: u32,
        asked: &Asked,
        away: Option<(u32, usize)>,
    ) -> Result<Ended> {
        let _alone = self.one.lock().expect("moves poisoned");
        let ended = self.run_alone(partition, target, asked, away);
        if let (true, Ok(done)) = (self.logs, &ended) {
            self.log(done);
        }
        self.under_way
            .lock()
            .expect("moves poisoned")
            .remove(crate::range::dataset_of(partition));
        ended
    }

    fn run_alone(
        &self,
        partition: &str,
        target: u32,
        asked: &Asked,
        away: Option<(u32, usize)>,
    ) -> Result<Ended> {
        let (from, source) = match away {
            Some((group, d)) => (group, &self.stores[d]),
            None => {
                let (from, _) = self.owner_now(partition)?.ok_or_else(|| {
                    StoreError::Unavailable(format!("the lake has no range {partition}"))
                })?;
                (from, self.database_of(from))
            }
        };
        let to = self.database_of(target);
        let aborted = |cause: MoveAbortCause, finding: Option<IntegrityFinding>| {
            Ended::Aborted(
                PartitionMoveAborted {
                    component: "lake".into(),
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
        // Warm: within one database the target reads the same rows; across
        // databases its database first copies the range, then its change
        // log (section 6.4).
        let across = !Arc::ptr_eq(source, to);
        let mut copied = if across {
            to.copy_range_from(source, partition, 0)?
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
                    // target holds (the review's C2).
                    let (s, t) = (
                        source.range_tip(partition)?.unwrap_or_default(),
                        to.range_tip_held(partition)?,
                    );
                    if copied >= s.held {
                        Duration::ZERO
                    } else {
                        Duration::from_nanos((s.held_at_ns - t.1).max(1) as u64)
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
                    "a range's move did not switch: its target's lag stayed above readyLagMs"
                );
                return Ok(aborted(MoveAbortCause::Lag, None));
            }
            if across {
                copied = to.copy_range_from(source, partition, copied)?;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // Hold and switch, under the range's row.
        let switched = source.switch_range(
            to,
            partition,
            target,
            self.settings.switch,
            self.settings.digest_differs(),
            copied,
        )?;
        match switched {
            Switched::Moved { closing, epoch } | Switched::LetGo { closing, epoch } => {
                tracing::info!(
                    partition,
                    from,
                    target,
                    closing,
                    epoch,
                    "a range moved to another writer group"
                );
                Ok(Ended::Moved(PartitionMoved {
                    component: "lake".into(),
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
                let finding = meridian_changes::records::finding(
                    "lake",
                    meridian_domain::v1::IntegrityCheck::MemberDigest,
                    &found,
                    self.clock.now_ns(),
                );
                Ok(aborted(MoveAbortCause::Digest, Some(finding)))
            }
        }
    }

    /// The moves a changed count of writer groups calls for (section 6.8,
    /// Triggers; question 10): each open range whose row names another
    /// group than its dataset's rendezvous owner at the count now, moved to
    /// it one at a time, by_hand false; each record logged. A range a move
    /// is under way in is left to it. Says what each move ended as.
    pub fn rebalance(&self) -> Vec<Ended> {
        let mut ended = Vec::new();
        let mut seen = BTreeSet::new();
        for store in &self.stores {
            let ranges = match store.ranges() {
                Ok(ranges) => ranges,
                Err(failed) => {
                    tracing::warn!(%failed, "the lake's ranges were not read");
                    continue;
                }
            };
            for (partition, closed, _) in ranges {
                if closed || !seen.insert(partition.clone()) || !self.begin(&partition) {
                    continue;
                }
                // Decided and run under the one-at-a-time lock, the moves by
                // hand read then, so a move by hand that ended a moment ago
                // is never undone (the review's C8).
                let alone = self.one.lock().expect("moves poisoned");
                let done = self.rebalance_one(&partition);
                if let Some(Ok(done)) = &done {
                    self.log(done);
                }
                drop(alone);
                self.under_way
                    .lock()
                    .expect("moves poisoned")
                    .remove(crate::range::dataset_of(&partition));
                match done {
                    Some(Ok(done)) => ended.push(done),
                    Some(Err(failed)) => tracing::warn!(
                        partition,
                        %failed,
                        "a range's move by the changed count stopped: the source still owns it"
                    ),
                    None => {}
                }
            }
        }
        ended
    }

    /// `partition` moved where the count now places it, or opened in its
    /// owner's home: its move's ending, or none where it stays.
    fn rebalance_one(&self, partition: &str) -> Option<Result<Ended>> {
        // Where a deployment admin moved a range by hand, it stays (section
        // 6.8), and so do the ranges split from it since (C8): only a count
        // lowered past its group moves it again.
        let by_hand = match self.stores[0].moved_by_hand() {
            Ok(by_hand) => by_hand,
            Err(failed) => {
                tracing::warn!(%failed, "the moves by hand were not read: no range moved by the count");
                return None;
            }
        };
        let away = match self.misplaced(partition) {
            Ok(away) => away,
            Err(failed) => {
                tracing::warn!(partition, %failed, "a range's databases were not read");
                return None;
            }
        };
        // Its owner's database holds it behind another, or a move's open is
        // owed there: moved there, its group kept.
        if let Some(away) = away {
            // Home from a database it is still written in -- the count of
            // databases raised -- only once every member reads both (ruled
            // 2026-11-15). A move that let it go there already, its open
            // owed, is finished.
            let written_there = self.stores[away.1]
                .range_tip(partition)
                .ok()
                .flatten()
                .is_some_and(|tip| tip.epoch != meridian_changes::moves::POINTER);
            if written_there && !self.rolled_out(partition) {
                return None;
            }
            return Some(self.run_alone(partition, away.0, &Asked::default(), Some(away)));
        }
        let now = match self.owner_now(partition) {
            Ok(Some((now, _))) => now,
            Ok(None) => return None,
            Err(failed) => {
                tracing::warn!(partition, %failed, "a range's owner was not read");
                return None;
            }
        };
        let wanted = meridian_changes::groups::owner(
            crate::range::dataset_of(partition),
            self.groups.count(),
        );
        // Its lineage as the database it was split in holds it: the longest.
        let lineage = self
            .stores
            .iter()
            .filter_map(|store| store.lineage_of(partition).ok())
            .max_by_key(Vec::len)
            .unwrap_or_else(|| vec![partition.to_string()]);
        let placed = lineage
            .iter()
            .find_map(|range| by_hand.get(range))
            .is_some_and(|group| *group == now && *group < self.groups.count());
        if now == wanted || placed {
            return None;
        }
        Some(self.run_alone(partition, wanted, &Asked::default(), None))
    }

    /// How a move ended logged in the lake's `control` (database 0's), its
    /// finding beside an abort for a digest.
    pub fn log(&self, ended: &Ended) {
        let (records, at_ns) = match ended {
            Ended::Moved(moved) => (vec![(MOVED, moved.encode_to_vec())], moved.moved_at_ns),
            Ended::Aborted(aborted, finding) => {
                let mut records = vec![(ABORTED, aborted.encode_to_vec())];
                if let Some(finding) = finding {
                    records.push((meridian_changes::records::FINDING, finding.encode_to_vec()));
                }
                (records, aborted.aborted_at_ns)
            }
        };
        if let Err(failed) = self.stores[0].log_move(&records, at_ns) {
            tracing::warn!(%failed, "a move's record was not logged in the lake's control");
        }
    }
}

/// Serve [`MOVE_PARTITION`] (the control partition's group's writer):
/// checked and answered at once, then run in the background, its record
/// logged when it ends.
pub fn serve(bus: &Arc<meridian_bus::Bus>, mover: &Arc<Mover>) {
    let (bus2, mover) = (bus.clone(), mover.clone());
    bus.serve(MOVE_PARTITION, move |envelope| {
        if envelope.payload_type != "meridian.v1.MovePartitionRequest" {
            return Err(format!(
                "expected meridian.v1.MovePartitionRequest, got {}",
                envelope.payload_type
            ));
        }
        let request = MovePartitionRequest::decode(envelope.payload.as_slice())
            .map_err(|failed| format!("a MovePartitionRequest did not read: {failed}"))?;
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
            .map_err(|refused| refused.on_the_bus())?;
        if !mover.begin(&request.partition) {
            return Err(meridian_bus::refusal_with_status(
                meridian_bus::RefusalStatus::FailedPrecondition,
                &["partition".to_string()],
                format!(
                    "a move in dataset {} is under way: ask again once it ends",
                    crate::range::dataset_of(&request.partition)
                ),
            ));
        }
        let started_at_ns = bus2.clock().now_ns();
        let asked = Asked {
            by_hand: true,
            moved_by: meta.acting_for_subject.clone(),
            reason: request.reason.clone(),
        };
        let mover = mover.clone();
        std::thread::spawn(move || {
            match mover.run(&request.partition, request.target_group, &asked) {
                Ok(_) if mover.logs => {}
                Ok(ended) => mover.log(&ended),
                Err(failed) => tracing::warn!(
                    partition = request.partition,
                    %failed,
                    "a range's move stopped: the source still owns it"
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
