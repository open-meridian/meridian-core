//! The lake's write-ahead stream, at the lake's writer (the `ha` profile;
//! spec/a-deployment-scales-out-and-fails-over-by-configuration section 4;
//! the rulings of 2026-11-08 and 2026-11-09; decisions/024 as amended).
//!
//! A data plugin's sidecar puts each lake write in a JetStream stream at
//! three replicas ([`meridian_bus::write_ahead`]). The writer takes them in
//! the stream's order, a batch at a time, and records each batch with the
//! stream's position it reached in one transaction
//! ([`crate::service::Lake::record_written`]) -- the convention of a sink
//! that keeps its offsets in its target. Then it answers each write as the
//! lake always has, once it committed.
//!
//! **Resumed, never skipped.** The position is read from the store when the
//! writer starts -- a takeover, a restart -- and again whenever a batch is
//! refused: the store refuses a batch that does not follow the position it
//! holds, which is what a database failover that lost the last commits
//! leaves. The writer goes back to the position held and takes the writes
//! after it again; a row taken again keeps its row key, which the lake
//! records once. So a write the stream held is recorded, whatever failed
//! after it was answered.
//!
//! The stream is never the lake's record: its Postgres is (decisions/024),
//! and the stream is a bounded buffer ahead of it. In `tolerant` there is no
//! stream and nothing here runs.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_nats::jetstream::{self, consumer, stream};
use meridian_bus::write_ahead::{
    stream_subjects, AFTER_HEADER, REPLY_HEADER, STREAM, TIMEOUT_HEADER, TOPIC_HEADER,
};
use meridian_bus::{Envelope, HandlerReply};
use prost::Message as _;

use crate::service::Lake;
use crate::store::{Dropped, StoreError, WriteAheadBatch, WriteAheadPosition};

pub mod guard;

/// The database connections the writer adds to the lake's pool in `ha`:
/// its batch's transaction and its position's read (ruled 2026-11-08: the
/// brought database's max_connections counts them).
pub const CONNECTIONS: u32 = 2;

/// The most writes taken in one batch.
pub const MOST_WRITES: usize = 256;

/// The most bytes of writes taken in one batch, its last write's taken
/// whole: the change log's page (step 3d, group commit; section 6.5).
pub const BATCH_MOST_BYTES: usize = meridian_changes::group_commit::BATCH_MOST_BYTES;

/// How long the writer waits before it asks again, after a batch the store
/// could not take.
const AGAIN_AFTER: Duration = Duration::from_millis(500);

/// How often the writer, delivered nothing, looks whether a move's switch
/// took its place in the stream back, while one may (step 3d; spec 6.1 and
/// 6.8; PR CI 43810170103, ha): a write it passed, its row another group's,
/// then switched to its own group, is taken again from the place taken back
/// and answered within the hold, never only once another write comes.
pub const TAKEN_BACK_LOOK_EVERY: Duration = Duration::from_millis(250);

/// The stream's place as the broker holds it now -- its last sequence and
/// its identity, its creation time -- what a plugin shard's raise admits
/// the writes the stream held through (review c5a H2, c5a2 N4).
pub async fn place(
    client: &async_nats::Client,
) -> Result<meridian_changes::shard_fence::Admit, String> {
    let context = jetstream::new(client.clone());
    let mut stream = context
        .get_stream(STREAM)
        .await
        .map_err(|failed| format!("the write-ahead stream was not found: {failed}"))?;
    let info = stream
        .info()
        .await
        .map_err(|failed| format!("the write-ahead stream's place was not read: {failed}"))?;
    Ok(meridian_changes::shard_fence::Admit {
        through: info.state.last_sequence,
        stream: identity(info),
    })
}

/// A stream's identity: its creation time, ns.
pub fn identity(info: &stream::Info) -> i64 {
    info.created.unix_timestamp_nanos() as i64
}

/// One write as the stream held it.
#[derive(Debug, Clone)]
pub struct Taken {
    /// The stream's sequence for it.
    pub sequence: u64,
    /// The stream's time for it.
    pub at_ns: i64,
    /// The lake topic it was asked on.
    pub topic: String,
    /// Where its answer goes; none, and it is answered to nobody.
    pub reply_to: Option<String>,
    /// The sequence of the write its sidecar's stream held before it on the
    /// same subject, where it said one.
    pub after: Option<u64>,
    /// The request as its sidecar stamped it.
    pub envelope: Envelope,
    /// The dataset its subject names; empty where it names none.
    pub dataset: String,
    /// How long its caller waits for its answer, as its sidecar said it
    /// (`Meridian-Timeout-Ms`); none where it said none.
    pub timeout_ms: Option<u64>,
}

impl Taken {
    /// When its answer is given by at the latest (step 3d; spec 6.8, the
    /// hold): its caller's wait counted from when the stream took it -- the
    /// broker's time, one hop from the lake (review c5r L2); its sidecar's
    /// stamp only where the stream said none -- less the answer's way back
    /// ([`ANSWERED_BEFORE`]), as `moves::hold_now` bounds a held write; none
    /// where its sidecar said no wait. `now_ns` is now on the lake's clock;
    /// the age is never below none or above the wait.
    ///
    /// [`ANSWERED_BEFORE`]: meridian_changes::moves::ANSWERED_BEFORE
    pub fn answer_by(&self, now_ns: i64) -> Option<tokio::time::Instant> {
        // A day at most: past any hold, and never an overflow.
        let wait = Duration::from_millis(self.timeout_ms?.min(86_400_000));
        let sent = if self.at_ns > 0 {
            self.at_ns
        } else {
            self.envelope
                .meta
                .as_ref()
                .map_or(0, |meta| meta.published_at_ns)
        };
        let age = if sent > 0 {
            Duration::from_nanos(now_ns.saturating_sub(sent).max(0) as u64).min(wait)
        } else {
            Duration::ZERO
        };
        Some(
            tokio::time::Instant::now()
                + wait
                    .saturating_sub(age)
                    .saturating_sub(meridian_changes::moves::ANSWERED_BEFORE),
        )
    }
}

/// A caller's wait as its sidecar said it (`Meridian-Timeout-Ms`, ms): none
/// where it said none or what it said is no count of ms (review c5r L5).
pub fn timeout_of(said: Option<&str>) -> Option<u64> {
    said?.trim().parse().ok()
}

/// Where a writer group keeps its position in the stream (step 3d; section
/// 6.1): group 0 under the stream's key ([`stream_key`]); each group past
/// the first under `<key>.group-<g>`, as its queue group is named.
pub fn position_key(stream: &str, groups: meridian_changes::groups::Groups) -> String {
    group_key(stream, groups.mine())
}

/// The key everything the lake keeps of one write-ahead stream is kept
/// under -- each group's position, the runs it dropped, each dataset's
/// latest valid time and each group's parts of a write -- the stream's
/// name and its identity, `<stream>.<identity>` (item 191, ruled
/// 2026-11-19). A stream made again under the same name -- JetStream's
/// volumes deleted on leaving `ha`, and `ha` taken again -- is another
/// stream, numbered from its own first, and nothing kept of the one before
/// applies to it. Identity 0, none read, is the name alone.
pub fn stream_key(stream: &str, identity: i64) -> String {
    match identity {
        0 => stream.to_string(),
        identity => format!("{stream}.{identity}"),
    }
}

/// Writer group `group`'s position's key in `stream`.
pub fn group_key(stream: &str, group: u32) -> String {
    match group {
        0 => stream.to_string(),
        group => format!("{stream}.group-{group}"),
    }
}

/// The lowest place any writer group takes the stream kept as `base` from
/// next, this group's being `mine` (the review of 4c, fix B item 2): what
/// the stream's age bound is held to, since a write is dropped unrecorded
/// where any group whose rows it carries has not passed it. Each group's
/// position as its database keeps it -- a group with none takes the stream
/// from its first -- and any place kept under a group's key beside a
/// move's pointer in another's: where a switch between databases takes its
/// target back to, at its open. One group, `mine`. Read through
/// [`Lake::group_position`], as an answer spanning groups reads them.
pub fn lowest_place(lake: &Lake, base: &str, mine: u64) -> Result<u64, StoreError> {
    let groups = lake.groups();
    let mut lowest = mine;
    for group in 0..groups.count() {
        let key = group_key(base, group);
        for kept_by in 0..groups.count() {
            if (group, kept_by) == (groups.mine(), groups.mine()) {
                continue;
            }
            let place = match lake.group_position(&key, kept_by)? {
                Some(at) => at.sequence + 1,
                None if kept_by == group => 1,
                None => continue,
            };
            lowest = lowest.min(place);
        }
    }
    Ok(lowest)
}

/// An answer this group gives once other groups have committed their own
/// parts of its write (step 3d; spec 6.7): the answer's place in its batch,
/// the write's stream sequence, and the groups whose positions must reach
/// it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Wait {
    pub at: usize,
    pub sequence: u64,
    pub groups: Vec<u32>,
    /// The write's dataset: its ranges' owners as they are while the answer
    /// waits are waited on too, never only those its taker saw (the
    /// review's C4). None named, only `groups`.
    pub dataset: String,
    /// The rows the write sent, which its answer accounts for (ruled
    /// 2026-11-15); none said, and the answer is held to none.
    pub sent: Sent,
    /// When the answer is given by at the latest, refused if it must be:
    /// before its caller stops waiting ([`Taken::answer_by`]). None, and the
    /// hold alone bounds it.
    pub answer_by: Option<tokio::time::Instant>,
}

/// What a write sent, as the take that answers it saw it (ruled
/// 2026-11-15): the rows of datasets the lake keeps, every writer group's
/// among them, which its answer's counts account for; and the rows of
/// datasets served and not kept this take served, which are in the take's
/// own answer and in no group's part.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sent {
    pub kept: u32,
    pub served: u32,
}

/// The refusal of a write whose answer does not account for every row it
/// sent (ruled 2026-11-15): OVERLOADED with a retry-after, so its plugin
/// sends it again -- a row is recorded once by its key, and a row some
/// group did commit is found unchanged. Never a success: a row no group
/// committed would be lost in silence.
pub fn unaccounted(counted: u32, sent: u32) -> String {
    meridian_bus::refusal_retrying(
        meridian_pb::v1::RefusalReason::Overloaded as i32,
        meridian_changes::moves::HELD_RETRY_SECONDS,
        format!(
            "{counted} of this write's {sent} rows are accounted for: the rest are not \
             committed yet, or their route has changed; ask again"
        ),
    )
}

/// The groups besides this one owning an open range of `dataset`, as this
/// member last read them: those whose parts of a write of it an answer
/// waits for.
fn other_groups(groups: meridian_changes::groups::Groups, dataset: &str) -> Vec<u32> {
    let mut others: Vec<u32> = crate::range::leaves(&groups.ranges_of(dataset))
        .into_iter()
        .map(|(_, group)| group)
        .filter(|group| *group != groups.mine())
        .collect();
    others.sort_unstable();
    others.dedup();
    others
}

/// Whether a write taken from the stream is this writer group's to commit:
/// its dataset's owner is the group; one naming no dataset is group 0's.
///
/// A dataset whose ranges span groups (step 3d; ruling 2 of 2026-11-14) is
/// every such group's: each commits its own ranges' rows of the write and
/// skips the rest, which their owner's consumer commits from the same
/// stream. The group owning the dataset's own partition answers it.
fn ours(groups: meridian_changes::groups::Groups, write: &Taken) -> bool {
    if write.dataset.is_empty() {
        groups.holds_control()
    } else {
        groups.owns(&write.dataset)
            || crate::range::leaves(&groups.ranges_of(&write.dataset))
                .iter()
                .any(|(_, group)| *group == groups.mine())
    }
}

/// Whether this group answers a write it took: the group owning the
/// write's dataset's own partition (its full range, split or not), or
/// group 0 for one naming none.
fn answers(groups: meridian_changes::groups::Groups, write: &Taken) -> bool {
    if write.dataset.is_empty() {
        groups.holds_control()
    } else {
        groups.owns(&write.dataset)
    }
}

/// What a batch did: each write's answer, to where it is sent.
pub type Answers = Vec<(Option<String>, HandlerReply)>;

/// One write as the lake records it from the stream: its topic, its
/// request, and the run the stream dropped that its sidecar's last write of
/// the dataset was in, where it was in one -- the first write of its
/// dataset after a gap (W10.14).
#[derive(Debug, Clone)]
pub struct Written {
    pub topic: String,
    pub envelope: Envelope,
    pub after_gap: Option<Dropped>,
    /// Its sequence in the stream, where its dataset's ranges span writer
    /// groups (step 3d; spec 6.7): what this group's rows of it do is kept
    /// with them, for its answer.
    pub counted: Option<u64>,
    /// Its sequence in the stream: what its plugin shard's fence admits a
    /// write the stream held before the take by (review c5a H2).
    pub sequence: u64,
}

/// What [`crate::service::Lake::record_written`] did, once committed: each
/// write's answer, and each dataset's latest valid time it recorded.
pub struct Recorded {
    pub answers: Vec<HandlerReply>,
    pub valid: Vec<(String, i64)>,
    /// What each write sent, in step with `answers`.
    pub sent: Vec<Sent>,
}

/// The writer's place in the stream: the position the store holds, which
/// each batch moves, and what the stream dropped that the lake has found.
pub struct Writer {
    lake: Arc<Lake>,
    /// The stream's key ([`stream_key`]), as every group's position is
    /// kept by.
    base: String,
    /// The stream's identity, as the writer was started in it.
    identity: i64,
    /// The waits of the answers of the batch taken last (step 3d).
    waits: Vec<Wait>,
    stream: String,
    position: Option<WriteAheadPosition>,
    dropped: Vec<Dropped>,
    valid: BTreeMap<String, i64>,
    /// When it last passed a write whose rows were, in whole or in part,
    /// another group's: a move's switch may take its place back to before
    /// it, for as long as the hold (step 3d; spec 6.1 and 6.8).
    passed_at: Option<tokio::time::Instant>,
    /// How many of its answers still wait on other groups' parts: while
    /// any does, a move's switch may yet take its place back, however long
    /// ago it passed the write (review c5r L3).
    settling: Arc<std::sync::atomic::AtomicUsize>,
}

/// Whether `dataset`'s ranges span writer groups as this member last read
/// them, or may by its answer: another group owns one, or it is split.
fn spans(groups: meridian_changes::groups::Groups, dataset: &str) -> bool {
    !dataset.is_empty()
        && (!other_groups(groups, dataset).is_empty()
            || crate::range::leaves(&groups.ranges_of(dataset)).len() > 1)
}

impl Writer {
    /// The writer of `lake`, at the position its store holds in the stream
    /// named `stream` whose identity is `identity`, with what it found that
    /// stream dropped: no position and nothing found in a stream it has
    /// never committed from (item 191).
    pub fn start(lake: Arc<Lake>, stream: &str, identity: i64) -> Result<Writer, StoreError> {
        // Each writer group's own position (step 3d; section 6.1).
        let base = stream_key(stream, identity);
        let key = position_key(&base, lake.groups());
        // A lake upgraded from one that kept its places by the stream's
        // name alone carries them to its identity first (the review of
        // item 191's finding 1): the stream is the one they were taken in,
        // and the writer goes on from its position, never taking again a
        // write it took. Never where they were taken in a stream made
        // before this one under the same name.
        if base != stream {
            let named = position_key(stream, lake.groups());
            if lake.write_ahead_carried(&named, &key, identity)? {
                tracing::info!(
                    identity,
                    from = named,
                    to = key,
                    "the lake's places in its write-ahead stream, kept by its name, carried to \
                     its identity"
                );
            }
        }
        let stream = key;
        let position = lake.write_ahead_position(&stream)?;
        let (dropped, valid) = lake.write_ahead_found(&stream)?;
        Ok(Writer {
            lake,
            base,
            identity,
            waits: Vec::new(),
            stream,
            position,
            dropped,
            valid,
            passed_at: None,
            settling: Arc::default(),
        })
    }

    /// Whether a move's switch may yet take the writer's place back (step
    /// 3d; spec 6.1 and 6.8): within the hold of a write it passed, its
    /// rows another group's -- all of them, or the rest of a write it took
    /// whose dataset has a range another group owns, the group answering
    /// that dataset among such takers (failover-repeat 43993402238, ha).
    /// A switch takes the target's place back to its
    /// source's committed place, and owes the target only the writes the
    /// source had not committed: those the source's batch was held on, for
    /// at most the hold. Never otherwise, so an idle writer asks nothing.
    pub fn may_be_taken_back(&self) -> bool {
        self.passed_at
            .is_some_and(|at| at.elapsed() < meridian_changes::moves::hold())
            || self.settling.load(std::sync::atomic::Ordering::SeqCst) > 0
    }

    /// How many of its answers still wait on other groups' parts, counted
    /// by [`settle_each`] (review c5r L3).
    pub fn settling(&self) -> Arc<std::sync::atomic::AtomicUsize> {
        Arc::clone(&self.settling)
    }

    /// The earliest any caller of `batch` this group answers stops waiting,
    /// less the answer's way back ([`Taken::answer_by`]): what a batch held
    /// by a move's switch is held to at most (review c5r M2).
    pub fn answer_by(&self, batch: &[Taken]) -> Option<tokio::time::Instant> {
        let now_ns = self.lake.now_ns();
        self.answered(batch)
            .filter_map(|write| write.answer_by(now_ns))
            .min()
    }

    /// The writes of `batch` this group answers: those with somewhere to
    /// answer, of datasets this group answers.
    fn answered<'a>(&self, batch: &'a [Taken]) -> impl Iterator<Item = &'a Taken> {
        let groups = self.lake.groups();
        batch.iter().filter(move |write| {
            write.reply_to.is_some() && ours(groups, write) && answers(groups, write)
        })
    }

    /// The answers of a batch a move's switch held past the hold, or past
    /// the earliest of its callers' waits (spec 6.8; review c5r M2): each
    /// write this group answers refused OVERLOADED with a retry-after, as
    /// a held write is. Nothing of it was committed and the stream keeps
    /// it: taken again, its rows are committed then, and its plugin's
    /// retry finds them unchanged.
    pub fn held_refusals(&self, batch: &[Taken]) -> Answers {
        self.answered(batch)
            .map(|write| {
                (
                    write.reply_to.clone(),
                    Err(meridian_bus::refusal_retrying(
                        meridian_pb::v1::RefusalReason::Overloaded as i32,
                        meridian_changes::moves::HELD_RETRY_SECONDS,
                        "a range of this write is held by a move's switch past the hold or its \
                         caller's wait: nothing of it is committed yet; ask again",
                    )),
                )
            })
            .collect()
    }

    /// Whether the store's place in the stream is no longer the one the
    /// writer holds: a move's switch took it back (spec 6.1).
    pub fn taken_back(&self) -> Result<bool, StoreError> {
        Ok(self.lake.write_ahead_position(&self.stream)? != self.position)
    }

    /// The position the writer goes on from.
    pub fn position(&self) -> Option<WriteAheadPosition> {
        self.position
    }

    /// The first sequence the writer takes next: the stream's first it
    /// still holds where the writer has no position in it.
    pub fn next(&self) -> u64 {
        self.position.map_or(1, |at| at.sequence + 1)
    }

    /// Its position where it has a place in the stream: none before its
    /// first commit in it, or at a place before the stream's first write
    /// that a move's switch took it back to (sequence 0).
    pub fn placed(&self) -> Option<WriteAheadPosition> {
        self.position.filter(|at| at.sequence > 0)
    }

    /// The ranges' writer groups read again into the member's view.
    pub fn read_owners(&self) -> Result<(), StoreError> {
        self.lake.read_owners_again()
    }

    /// The position read again from the store: after a batch it refused.
    pub fn reread(&mut self) -> Result<(), StoreError> {
        // The ranges' owners first: a move's switch takes the target's
        // position back so that it takes the range's rows as its own from
        // there (step 3d; spec 6.1).
        self.lake.read_owners_again()?;
        self.position = self.lake.write_ahead_position(&self.stream)?;
        let (dropped, valid) = self.lake.write_ahead_found(&self.stream)?;
        self.dropped = dropped;
        self.valid = valid;
        Ok(())
    }

    /// A batch taken from the stream, in its order, recorded with the
    /// position it reaches; answered with each write's reply. A sequence
    /// the batch skips past was dropped by the stream before the lake took
    /// it, and is kept as such with the batch; a write whose sidecar's last
    /// write of its dataset was dropped is the first after a gap (W10.14).
    /// Refused whole where the store refused it: the writer reads its
    /// position again before it takes another.
    pub fn take(&mut self, taken: Vec<Taken>) -> Result<Answers, StoreError> {
        self.waits.clear();
        let Some(last) = taken.last() else {
            return Ok(Vec::new());
        };
        let through = WriteAheadPosition {
            sequence: last.sequence,
            at_ns: last.at_ns,
        };
        // A writer group past the first that has never committed in this
        // stream -- no position of its own under its key in any of the
        // lake's databases (a position is committed with its batch's rows)
        // -- takes from the first write the stream still holds, and counts
        // none before it dropped: started after `writerGroups` was raised,
        // its earlier writes were group 0's to take, and were taken (step
        // 3d; section 6.1; CI 43615034712). Writes in the stream are taken
        // again, and each row is recorded once. Group 0 with no position in
        // the stream counts the stream's own sequences from 1 (the review
        // of item 191's finding 5, the coordinator's reading, item 203): a
        // run that aged out before its first take -- a writer down longer
        // than the stream's bound -- is a gap (spec section 4). A stream
        // made again under the same name numbers from its own first: the
        // writes of the one before are never in its numbering, and nothing
        // is counted lost of them (item 191, ruled 2026-11-19). A group that
        // has committed goes on from its position, and a run the stream
        // dropped past it is a gap.
        //
        // A place before the stream's first write, where a move's switch
        // took the group back with no place of the source's to take it to
        // (sequence 0; the review of item 191's findings 3 and 11), is no
        // place in the stream for any group: it holds no time, and counts
        // nothing before the first write as dropped.
        let placed = self.placed();
        let fresh = match self.position {
            None => self.lake.groups().mine() > 0,
            Some(_) => placed.is_none(),
        };
        let mut expected = if fresh {
            taken[0].sequence
        } else {
            self.next()
        };
        let mut before_at = placed.map_or(0, |at| at.at_ns);
        let mut dropped = Vec::new();
        for write in &taken {
            if write.sequence > expected {
                // Found the moment the stream skips (approved 2026-11-11):
                // a gap recorded with the batch, its datasets not yet known.
                dropped.push(Dropped {
                    first: expected,
                    last: write.sequence - 1,
                    after_at_ns: before_at,
                    until_at_ns: write.at_ns,
                    gap_id: self.lake.new_gap_id(),
                });
            }
            expected = write.sequence + 1;
            before_at = write.at_ns;
        }
        for run in &dropped {
            tracing::warn!(
                first = run.first,
                last = run.last,
                gap_id = run.gap_id,
                "the lake's write-ahead stream dropped writes before the lake took them: \
                 a writer gone longer than the stream's bound; a gap recorded at once, its \
                 datasets named as each writes next"
            );
        }
        // Every group reads the whole stream, so each dataset's chain and
        // every run it dropped are seen whole; each commits its own
        // datasets' writes alone, and moves its own position past the rest
        // (step 3d; section 6.1). The rest are their owners' to answer.
        let groups = self.lake.groups();
        let all = taken.len();
        let taken: Vec<Taken> = taken
            .into_iter()
            .filter(|write| ours(groups, write))
            .collect();
        // Passed: a write left whole to its owner, or one taken whose
        // dataset has a range another group owns -- this group commits its
        // own rows of it and moves its place past the rest. A switch of
        // that range to this group takes the place back to before the write
        // (failover-repeat 43993402238, ha run 3: the group answering the
        // driver's dataset took a price whose range was group 0's; the
        // range switched to it 16 ms after the price reached the stream,
        // and with no write after it, the group found its place taken back
        // only when the next came, 6 s on).
        let passed = taken.len() < all
            || taken
                .iter()
                .any(|write| !other_groups(groups, &write.dataset).is_empty());
        let mut sent_to = Vec::with_capacity(taken.len());
        let mut writes = Vec::with_capacity(taken.len());
        let now_ns = self.lake.now_ns();
        for write in taken {
            let after_gap = write.after.and_then(|after| {
                self.dropped
                    .iter()
                    .chain(&dropped)
                    .find(|run| run.holds(after))
                    .cloned()
            });
            let answering = answers(groups, &write);
            // An answer of a write whose dataset's ranges span groups waits
            // until each other group's part has committed (spec 6.7).
            // A split dataset's answer waits even where every range was this
            // group's at the take: one may move before the answer (C4).
            let spans = spans(groups, &write.dataset);
            if answering && write.reply_to.is_some() && spans {
                self.waits.push(Wait {
                    at: sent_to.len(),
                    sequence: write.sequence,
                    groups: other_groups(groups, &write.dataset),
                    dataset: write.dataset.clone(),
                    sent: Sent::default(),
                    answer_by: write.answer_by(now_ns),
                });
            }
            sent_to.push(write.reply_to.filter(|_| answering));
            writes.push(Written {
                topic: write.topic,
                envelope: write.envelope,
                after_gap,
                // Every group's part of such a write is kept, the answering
                // group's too: an answer counts each part as committed,
                // whichever take committed it (spec 6.7).
                counted: spans.then_some(write.sequence),
                sequence: write.sequence,
            });
        }
        let batch = WriteAheadBatch {
            stream: self.stream.clone(),
            after: self.position,
            through,
            dropped: dropped.clone(),
            ..Default::default()
        };
        let recorded = self.lake.record_written(writes, batch, &self.valid)?;
        for wait in &mut self.waits {
            wait.sent = recorded.sent.get(wait.at).copied().unwrap_or_default();
        }
        self.position = Some(through);
        if passed {
            self.passed_at = Some(tokio::time::Instant::now());
        }
        self.dropped.extend(dropped);
        for (dataset, valid_ns) in recorded.valid {
            let at = self.valid.entry(dataset).or_insert(valid_ns);
            *at = (*at).max(valid_ns);
        }
        Ok(sent_to.into_iter().zip(recorded.answers).collect())
    }
}

/// The stream, as the lake's writer makes it where no writer has yet: on
/// file at its replicas -- three, the broker's servers, in `ha` -- every
/// subject of [`stream_subjects`], kept by its bounds and never past them.
///
/// Full, it refuses a new write (`discard: new`), and its plugin is refused
/// for load with a retry-after (approved 2026-11-10, the research batch's
/// item 5). JetStream's default, `discard: old`, drops the oldest writes to
/// take the new one -- writes held for the lake, its plugin already
/// answered or waiting to be -- and is never used. A write older than
/// `max_age` is dropped all the same: a writer gone that long leaves a gap
/// (W10.14), which the next writer finds.
pub fn stream_config(bounds: &Bounds) -> stream::Config {
    stream::Config {
        name: STREAM.to_string(),
        subjects: vec![stream_subjects()],
        retention: stream::RetentionPolicy::Limits,
        storage: stream::StorageType::File,
        num_replicas: bounds.replicas,
        max_age: bounds.max_age,
        max_bytes: bounds.max_bytes,
        discard: stream::DiscardPolicy::New,
        // A sidecar restarted reads the last write of each of its datasets
        // back, by its subject alone, to go on with its chain (contract
        // v30, stage C).
        allow_direct: true,
        ..Default::default()
    }
}

/// Where a write taken from the stream is answered: the inbox its write
/// named, where that is an inbox of the credential that wrote it -- the
/// instance its subject names, which alone may publish there (v31a, item 2;
/// the access audit's point 10). Any other reply-to is refused: the write is
/// recorded and answered to nobody, and said so, so no plugin has the lake
/// publish into another's inbox as the runtime.
pub(crate) fn answered_to(subject: &str, reply_to: Option<String>) -> Option<String> {
    let to = reply_to.filter(|to| !to.is_empty())?;
    let sender = meridian_bus::write_ahead::dataset_of_subject(subject).and_then(|dataset| {
        dataset
            .split_once(':')
            .map(|(instance, _)| instance.to_string())
    });
    if sender.is_some() && meridian_bus::inbox::owner(&to) == sender.as_deref() {
        return Some(to);
    }
    tracing::warn!(
        subject,
        reply_to = to,
        "a write-ahead write named a reply-to outside its sender's inboxes: refused, its rows \
         recorded and answered to nobody"
    );
    None
}

/// A write as the stream sent it, or why it could not be read.
fn taken(message: &jetstream::Message) -> Result<Taken, String> {
    let info = message.info().map_err(|failed| failed.to_string())?;
    let header = |name: &str| {
        message
            .headers
            .as_ref()
            .and_then(|headers| headers.get(name))
            .map(|said| said.to_string())
    };
    Ok(Taken {
        sequence: info.stream_sequence,
        at_ns: info.published.unix_timestamp_nanos() as i64,
        topic: header(TOPIC_HEADER).unwrap_or_default(),
        reply_to: answered_to(&message.subject, header(REPLY_HEADER)),
        after: header(AFTER_HEADER).and_then(|said| said.parse().ok()),
        envelope: Envelope::decode(message.payload.clone()).unwrap_or_default(),
        dataset: meridian_bus::write_ahead::dataset_of_subject(&message.subject)
            .unwrap_or_default(),
        timeout_ms: timeout_of(header(TIMEOUT_HEADER).as_deref()),
    })
}

impl Writer {
    /// The waits of the batch taken last, taken out (step 3d).
    pub fn take_waits(&mut self) -> Vec<Wait> {
        std::mem::take(&mut self.waits)
    }

    /// The stream's key every group's position is kept by ([`stream_key`]).
    pub fn stream_name(&self) -> &str {
        &self.base
    }

    /// The identity of the stream the writer was started in.
    pub fn identity(&self) -> i64 {
        self.identity
    }
}

/// A batch's answers once each wait is over (step 3d; spec 6.7): an answer
/// of a write split across groups given once every group's position has
/// reached the write -- its part committed -- counting every group's part
/// as each kept it with its rows (`lake_write_ahead_part`), its watermark
/// read again so it names each part's partitions at their heads; past
/// `moves.holdSeconds`, or past its caller's wait less the answer's way back
/// where that comes first ([`Wait::answer_by`]), refused OVERLOADED with a
/// retry-after, its rows committed all the same and kept by the stream.
///
/// This group's own position is waited on too, and every position is read
/// twice: a move's switch takes its target's position back before its
/// source passes a write whose rows it no longer commits, so a write every
/// group has passed on two readings is one whose every row is committed --
/// by the range's source before the switch, or by its target taking the
/// write again -- never one its source skipped and its target has yet to
/// take again. Across two databases the switch is not one transaction: the
/// source lets the range go first, and its target opens it, its position
/// taken back with the open, after. Between the two the source passes the
/// write, its rows nobody's yet, and the target's position still says it
/// passed (PR CI 43675491409, ha: two prices answered recorded 0, restated
/// 0, unchanged 0 in the half second after the driver's range switched to
/// database 1). So no answer is given while a range of its dataset is
/// between two databases.
pub async fn settled(
    lake: &Arc<Lake>,
    stream: &str,
    answers: Answers,
    waits: Vec<Wait>,
) -> Answers {
    let mut settled: Vec<Option<(Option<String>, HandlerReply)>> =
        (0..answers.len()).map(|_| None).collect();
    let mut each = settle_each(Arc::clone(lake), stream.to_string(), answers, waits, None);
    while let Some((at, to, answered)) = each.recv().await {
        settled[at] = Some((to, answered));
    }
    settled.into_iter().flatten().collect()
}

/// A batch's answers, each sent on as soon as it is decided, with its place
/// in the batch (review c5r M1): those that wait on no other group at once;
/// each that waits once its own wait is over ([`settled`]'s rules), on a
/// task of its own, by its own bound -- the hold from now, and never past
/// its caller's wait less the answer's way back -- never held for another
/// write's wait. `settling`, where given, counts the waits not yet over.
pub fn settle_each(
    lake: Arc<Lake>,
    stream: String,
    answers: Answers,
    waits: Vec<Wait>,
    settling: Option<Arc<std::sync::atomic::AtomicUsize>>,
) -> tokio::sync::mpsc::UnboundedReceiver<(usize, Option<String>, HandlerReply)> {
    use std::sync::atomic::Ordering;
    let (sent, each) = tokio::sync::mpsc::unbounded_channel();
    let until = tokio::time::Instant::now() + meridian_changes::moves::hold();
    let mut waiting: BTreeMap<usize, (Option<String>, HandlerReply)> = BTreeMap::new();
    let waited: std::collections::BTreeSet<usize> = waits.iter().map(|wait| wait.at).collect();
    for (at, (to, answered)) in answers.into_iter().enumerate() {
        if waited.contains(&at) {
            waiting.insert(at, (to, answered));
        } else {
            let _ = sent.send((at, to, answered));
        }
    }
    for wait in waits {
        let Some((to, answered)) = waiting.remove(&wait.at) else {
            continue;
        };
        if let Some(settling) = &settling {
            settling.fetch_add(1, Ordering::SeqCst);
        }
        let (lake, stream, sent, settling) = (
            Arc::clone(&lake),
            stream.clone(),
            sent.clone(),
            settling.clone(),
        );
        tokio::spawn(async move {
            let answered = settle_one(&lake, &stream, answered, &wait, until).await;
            let _ = sent.send((wait.at, to, answered));
            if let Some(settling) = settling {
                settling.fetch_sub(1, Ordering::SeqCst);
            }
        });
    }
    each
}

/// One write's answer once its wait is over ([`settled`]'s rules), by
/// `until` (the hold) and never past its caller's wait less the answer's
/// way back.
async fn settle_one(
    lake: &Lake,
    stream: &str,
    answered: HandlerReply,
    wait: &Wait,
    until: tokio::time::Instant,
) -> HandlerReply {
    let Ok((_, payload)) = &answered else {
        return answered;
    };
    let mut groups = wait.groups.clone();
    let mine = lake.groups().mine();
    groups.retain(|group| *group != mine);
    // The dataset's ranges' owners now: read at once, again now and then
    // while the answer waits, and once more after every group waited on
    // has passed the write. A range moved to another group since the
    // write was taken is waited on there too (the review's C4): an old
    // owner skips the rows only once the move names the new one, so a
    // read after its position passed the write names the new owner.
    let more = |groups: &mut Vec<u32>| -> bool {
        if wait.dataset.is_empty() {
            return false;
        }
        if tokio::task::block_in_place(|| lake.read_owners_again()).is_err() {
            return true;
        }
        let mut grew = false;
        for group in other_groups(lake.groups(), &wait.dataset) {
            if !groups.contains(&group) {
                groups.push(group);
                grew = true;
            }
        }
        grew
    };
    more(&mut groups);
    // The hold, and never past its caller's wait less the answer's way
    // back: past it, refused OVERLOADED with a retry-after its caller
    // hears, never a timeout (failover-repeat 43993402238, ha run 3: a
    // price's answer could wait the hold from its take, past its
    // caller's 5 s; its caller heard DEADLINE_EXCEEDED).
    let until = wait.answer_by.map_or(until, |by| by.min(until));
    let mut looked = tokio::time::Instant::now();
    let reached = loop {
        if looked.elapsed() >= Duration::from_millis(100) {
            looked = tokio::time::Instant::now();
            more(&mut groups);
        }
        let passed = |groups: &[u32]| {
            tokio::task::block_in_place(|| {
                !lake.range_between(&wait.dataset).unwrap_or(true)
                    && groups.iter().chain([&mine]).all(|group| {
                        lake.group_position(&group_key(stream, *group), *group)
                            .ok()
                            .flatten()
                            .is_some_and(|at| at.sequence >= wait.sequence)
                    })
            })
        };
        if passed(&groups) && !more(&mut groups) && passed(&groups) {
            break true;
        }
        if tokio::time::Instant::now() >= until {
            break false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    if reached {
        tokio::task::block_in_place(|| {
            // Each group's part as it kept it: this group's own in place
            // of what this take alone did, where it kept one.
            let part = |group: u32| {
                lake.write_ahead_part(&group_key(stream, group), group, wait.sequence)
                    .map_err(|failed| failed.to_string())
            };
            let own = part(mine)?;
            // Every other group's, not only those owning a range of it
            // now: one that committed a part and has since moved its
            // range away kept it all the same.
            let mut others = crate::store::Counts::default();
            for group in (0..lake.groups().count()).filter(|group| *group != mine) {
                // Rows of it that group refused by the lake's rule 5:
                // the write's answer, by name, as this group's own
                // part's refusal is (ruled 2026-11-15). Its other
                // parts committed all the same.
                if let Some(refused) = lake
                    .write_ahead_refused(&group_key(stream, group), group, wait.sequence)
                    .map_err(|failed| failed.to_string())?
                {
                    return Err(refused);
                }
                if let Some(theirs) = part(group)? {
                    others.add(theirs);
                }
            }
            // Every row of a kept dataset accounted for, by this
            // group's part or its take and by every other group's part
            // (ruled 2026-11-15): else refused, to be sent again.
            let counted = match own {
                Some(own) => own.all(),
                None => crate::service::counted(payload).saturating_sub(wait.sent.served),
            } + others.all();
            if counted < wait.sent.kept {
                tracing::warn!(
                    sequence = wait.sequence,
                    dataset = %wait.dataset,
                    group = mine,
                    waited_on = ?groups,
                    own_part = own.is_some(),
                    counted,
                    sent = wait.sent.kept,
                    "a write of the stream is refused: its answer does not account for every row it sent"
                );
                return Err(unaccounted(counted, wait.sent.kept));
            }
            let again = lake.answer_again(payload, own, others, &wait.dataset)?;
            // Answered counting no row at all -- neither a part any
            // group kept nor what this take did itself: said with what
            // was waited on, so the member that answered names the path
            // (PR CI 43680214054 and 43684440785, ha: prices answered
            // recorded 0, restated 0, unchanged 0 after a range's move
            // between databases). Not said only because this group kept
            // no part: it keeps none where its rows of the write did
            // nothing it counts, and another group's part may count them.
            if crate::service::counts_nothing(&again) {
                tracing::warn!(
                    sequence = wait.sequence,
                    dataset = %wait.dataset,
                    group = mine,
                    waited_on = ?groups,
                    own_part = own.is_some(),
                    "a write of the stream is answered counting none of its rows"
                );
            }
            Ok::<_, String>(again)
        })
        .map(|again| ("meridian.v1.RecordObservationsReply".to_string(), again))
    } else {
        Err(meridian_bus::refusal_retrying(
            meridian_pb::v1::RefusalReason::Overloaded as i32,
            meridian_changes::moves::HELD_RETRY_SECONDS,
            format!(
                "writer group(s) {:?} had not committed their part of this write within the \
                 hold: its rows are kept and committed in turn; ask again",
                groups
            ),
        ))
    }
}

/// Each answer sent to the inbox its write named, as the broker's request
/// path sends one: a refusal in its header.
async fn answer(client: &async_nats::Client, answers: Answers) {
    for (to, answered) in answers {
        let Some(to) = to else { continue };
        let (payload_type, payload, refusal) = match answered {
            Ok((payload_type, payload)) => (payload_type, payload, None),
            Err(detail) => (String::new(), Vec::new(), Some(detail)),
        };
        let body = Envelope {
            meta: None,
            payload_type,
            payload,
        }
        .encode_to_vec();
        let sent = match refusal {
            None => client.publish(to, body.into()).await,
            Some(detail) => {
                let mut headers = async_nats::HeaderMap::new();
                headers.insert(
                    meridian_bus::write_ahead::REFUSAL_HEADER,
                    meridian_bus::header_words(&detail).as_str(),
                );
                client.publish_with_headers(to, headers, body.into()).await
            }
        };
        if let Err(failed) = sent {
            // Its sidecar times out and its plugin asks again; the rows are
            // recorded, and recorded once.
            tracing::warn!(%failed, "a write's answer did not reach the broker");
        }
    }
}

/// The writer's bounds on the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub max_age: Duration,
    pub max_bytes: i64,
    /// The servers that hold it: three in `ha`.
    pub replicas: usize,
}

/// `lake.writeAhead.maxAge` where the chart says nothing: ten minutes, five
/// times the slowest recovery that stops the writer (a zone's, two
/// minutes), and the bound a write is held within at a low rate.
pub const MAX_AGE: Duration = Duration::from_secs(10 * 60);

/// `lake.writeAhead.maxBytes` where the chart says nothing: 256 MB, about
/// 2.2 million rows at the 117 bytes a row a 100-row batch takes in the
/// stream (measured), so ten minutes at 3,600 rows a second and 3 minutes
/// at the lake's 12,000 -- past the zone's two -- before a plugin is
/// refused for load. The bound a write is held within at a high rate.
pub const MAX_BYTES: i64 = 256 << 20;

/// The least the stream's byte bound holds: this many of the broker's
/// largest message (`broker.limits.maxPayload`). The chart refuses less
/// too (`meridian-runtime.writeAheadMiB`).
pub const LEAST_WRITES: u64 = 16;

impl Bounds {
    /// The chart's bounds, `lake.writeAhead.maxAge` as `30s`, `10m` or `1h`
    /// and `lake.writeAhead.maxBytes` as `256MB` or `1GB`, each above 0;
    /// this module's where it says nothing. Refused naming the value. A
    /// byte bound under [`LEAST_WRITES`] of the broker's largest message
    /// (`max_payload` bytes, as the broker says it) is refused too: a
    /// stream with less room than one write is at its bound for that
    /// write, and one of a few writes is full at once.
    pub fn from_said(
        max_age: Option<&str>,
        max_bytes: Option<&str>,
        replicas: usize,
        max_payload: u64,
    ) -> Result<Bounds, String> {
        let max_age = match max_age.map(str::trim).filter(|said| !said.is_empty()) {
            None => MAX_AGE,
            Some(said) => duration(said).ok_or_else(|| {
                format!(
                    "lake.writeAhead.maxAge is {said:?}; a whole number above 0 of seconds, \
                     minutes or hours, as 30s, 10m or 1h"
                )
            })?,
        };
        let max_bytes = match max_bytes.map(str::trim).filter(|said| !said.is_empty()) {
            None => MAX_BYTES,
            Some(said) => bytes(said).ok_or_else(|| {
                format!(
                    "lake.writeAhead.maxBytes is {said:?}; a whole number above 0 of bytes, KB, \
                     MB or GB, as 256MB"
                )
            })?,
        };
        let least = LEAST_WRITES.saturating_mul(max_payload);
        if u64::try_from(max_bytes).unwrap_or(0) < least {
            return Err(format!(
                "lake.writeAhead.maxBytes is {max_bytes} bytes; at least {least}, {LEAST_WRITES} \
                 times the broker's largest message (broker.limits.maxPayload, {max_payload} \
                 bytes)"
            ));
        }
        Ok(Bounds {
            max_age,
            max_bytes,
            replicas,
        })
    }
}

fn whole(digits: &str) -> Option<u64> {
    digits
        .bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| digits.parse::<u64>().ok())
        .flatten()
        .filter(|n| *n > 0)
}

fn duration(said: &str) -> Option<Duration> {
    let (digits, unit) = said.split_at(said.len().checked_sub(1)?);
    let seconds = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        _ => return None,
    };
    whole(digits).map(|n| Duration::from_secs(n * seconds))
}

fn bytes(said: &str) -> Option<i64> {
    let digits = said.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let factor: u64 = match &said[digits.len()..] {
        "" => 1,
        // As the broker reads its own sizes: KB is 1,024 bytes.
        "KB" => 1 << 10,
        "MB" => 1 << 20,
        "GB" => 1 << 30,
        _ => return None,
    };
    whole(digits).and_then(|n| i64::try_from(n.checked_mul(factor)?).ok())
}

/// Take the stream's writes, record them and answer them, for as long as
/// this member writes: from the position the store holds, and back to it
/// whenever a batch is refused. Ends only with an error the writer cannot
/// go on from.
/// A batch's answers sent: at once where none waits; else once its waits
/// are over, on a task of its own, so the writer takes its next batch
/// meanwhile -- another group's part it waits on may be in that group's
/// next batch, which may wait on this one's (step 3d).
async fn answer_when_settled(
    lake: &Arc<Lake>,
    client: &async_nats::Client,
    writer: &mut Writer,
    answers: Answers,
) {
    let waits = writer.take_waits();
    if waits.is_empty() {
        return answer(client, answers).await;
    }
    // Each sent the moment it is decided, never held for a batch-mate's
    // wait (review c5r M1).
    let mut each = settle_each(
        Arc::clone(lake),
        writer.stream_name().to_string(),
        answers,
        waits,
        Some(Arc::clone(&writer.settling)),
    );
    let client = client.clone();
    tokio::spawn(async move {
        while let Some((_, to, answered)) = each.recv().await {
            answer(&client, vec![(to, answered)]).await;
        }
    });
}

/// A batch taken, tried again while a range of it is held by a move's
/// switch (step 3d; spec 6.8): refused whole while held, nothing committed,
/// and taken again once the switch is done -- at most moves.holdSeconds, and
/// never past the earliest of its callers' waits less the answer's way
/// back -- then refused as held, and the writer answers each write of it
/// that it answers OVERLOADED with a retry-after and goes on from its
/// position (review c5r M2). Every
/// dataset this group takes waits with it: a switch takes milliseconds. A
/// batch refused for a range another group owns since the view was read is
/// taken again at once on the corrected view.
pub async fn taken_past_a_switch(
    writer: &mut Writer,
    batch: Vec<Taken>,
) -> Result<Answers, StoreError> {
    let hold = meridian_changes::moves::hold();
    let began = std::time::Instant::now();
    // Held for at most the hold, and never past the earliest of its
    // callers' waits less the answer's way back (spec 6.8; review c5r M2):
    // past that, refused Held, and the writer answers each such write
    // OVERLOADED with a retry-after ([`Writer::held_refusals`]).
    let held_until = tokio::time::Instant::now() + hold;
    let held_until = writer
        .answer_by(&batch)
        .map_or(held_until, |by| by.min(held_until));
    let mut wait = Duration::from_millis(5);
    loop {
        let took = tokio::task::block_in_place(|| writer.take(batch.clone()));
        match took {
            Err(StoreError::Held(_)) if tokio::time::Instant::now() + wait < held_until => {
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_millis(100));
            }
            // A range another group's since the view was read: the refusal
            // corrected the view, and the batch is split by it again at once
            // -- the range's rows its owner's to take from the stream, the
            // answer waiting for its commit (spec 6.1, ruled 2026-11-14).
            Err(StoreError::NotOwned { .. }) if began.elapsed() < hold => {
                if let Err(failed) = tokio::task::block_in_place(|| writer.read_owners()) {
                    tracing::debug!(%failed, "the ranges' owners were not read again");
                }
            }
            other => {
                if began.elapsed() >= Duration::from_millis(5) && other.is_ok() {
                    tracing::debug!(
                        held_ms = began.elapsed().as_millis() as u64,
                        "a write-ahead batch waited for a move's switch"
                    );
                }
                return other;
            }
        }
    }
}

/// The writer started in the stream whose identity is `identity`, its
/// place read again until the store answers; said where the lake has none
/// in it (item 191, ruled 2026-11-19): a stream made again under its name,
/// or one the lake never took from, is taken from the first write it still
/// holds. Nothing of the stream before it is counted lost: its writes are
/// not in this one's numbering. Group 0 counts this stream's own writes
/// that aged out before its first take (finding 5, item 203).
async fn started(lake: &Arc<Lake>, identity: i64) -> Writer {
    let writer = loop {
        let started =
            tokio::task::block_in_place(|| Writer::start(Arc::clone(lake), STREAM, identity));
        match started {
            Ok(writer) => break writer,
            Err(failed) => {
                tracing::warn!(%failed, "the write-ahead position could not be read yet");
                tokio::time::sleep(AGAIN_AFTER).await;
            }
        }
    };
    lake.with_stream(identity);
    if writer.placed().is_none() {
        tracing::info!(
            identity,
            "the lake has no place in this write-ahead stream: taken from the first write it \
             holds; nothing of a stream before it under its name is counted lost"
        );
    }
    writer
}

pub async fn write_forever(
    lake: Arc<Lake>,
    client: async_nats::Client,
    bounds: Bounds,
    waiting: impl Fn(Option<String>) + Send + Sync + 'static,
) -> Result<(), String> {
    let context = jetstream::new(client.clone());
    let config = stream_config(&bounds);
    // Made where the broker says there is none: never with fewer replicas
    // than set, to get past a broker not yet ready (step 3d), and never
    // over one it could not say of (v31a item 5): a stream made again is
    // another stream, and the writes the one before held would be passed
    // by. Brought to this release's bounds once the writer knows its place
    // (below). Until then the lake takes no write, and says why in its
    // report.
    loop {
        let made = match context.get_stream(STREAM).await {
            Ok(_) => Ok(()),
            Err(failed) if not_found(&failed) => context
                .create_stream(config.clone())
                .await
                .map(|_| ())
                .map_err(|f| f.to_string()),
            Err(failed) => Err(format!(
                "the broker did not say whether it holds it ({failed})"
            )),
        };
        match made {
            Ok(()) => {
                waiting(None);
                break;
            }
            Err(failed) => {
                tracing::warn!(%failed, "the lake's write-ahead stream could not be made yet");
                waiting(Some(format!(
                    "taking no write: its write-ahead stream, kept on {} servers, could not be \
                     made yet ({failed})",
                    config.num_replicas
                )));
                tokio::time::sleep(AGAIN_AFTER * 4).await;
            }
        }
    }
    // The writer starts in the stream as it is now: its identity keys
    // every place the lake keeps in it (item 191).
    let identity = loop {
        match place(&client).await {
            Ok(place) => break place.stream,
            Err(failed) => {
                tracing::warn!(%failed, "the write-ahead stream's identity could not be read yet");
                tokio::time::sleep(AGAIN_AFTER).await;
            }
        }
    };
    let mut writer = started(&lake, identity).await;
    // The room a write needs: the broker's largest message, as it says it.
    let largest_write = u64::try_from(client.server_info().max_payload).unwrap_or(u64::MAX);
    let kept = bring_to_bounds(&lake, &context, &config, &bounds, largest_write, &writer).await;
    let place = Arc::new(AtomicU64::new(writer.next()));
    let guarding = tokio::spawn(guard_forever(
        context.clone(),
        largest_write,
        Arc::clone(&lake),
        Arc::clone(&place),
        writer.stream_name().to_string(),
        kept,
        waiting,
    ));
    let _guarding = AbortOnDrop(guarding);
    // The gaps the last writer was asking for again, asked again by this
    // one (W10.14): their wants were in its memory.
    // Group 0's, which holds the lake's wants (step 3d).
    let resumed = if lake.groups().holds_control() {
        tokio::task::block_in_place(|| lake.resume_gaps())
    } else {
        Ok(0)
    };
    match resumed {
        Ok(0) => {}
        Ok(asked) => tracing::info!(asked, "gaps still refetching asked again"),
        Err(failed) => tracing::warn!(%failed, "the gaps still refetching could not be read"),
    }
    tracing::info!(
        from = writer.next(),
        max_age_s = bounds.max_age.as_secs(),
        max_bytes = bounds.max_bytes,
        "the lake takes its writes from the write-ahead stream"
    );
    loop {
        let mut stream = match context.get_stream(STREAM).await {
            Ok(stream) => stream,
            Err(failed) => {
                tracing::warn!(%failed, "the write-ahead stream could not be read");
                tokio::time::sleep(AGAIN_AFTER).await;
                continue;
            }
        };
        // Its identity, read with the reader that takes its writes, said to
        // the lake for each write's stamp (c5a2 N4, c5a3 L2). A stream made
        // again under its name while the writer ran -- JetStream's volumes
        // deleted on leaving `ha`, and `ha` taken again -- is another
        // stream: the writer starts again in it, from what the lake keeps
        // under its identity (item 191).
        let mut messages = match follow(&mut stream, writer.next()).await {
            Ok((_, _, identity)) if identity != writer.identity() => {
                tracing::info!(
                    was = writer.identity(),
                    identity,
                    "the write-ahead stream was made again under its name: the lake takes it \
                     as another stream"
                );
                writer = started(&lake, identity).await;
                continue;
            }
            Ok((messages, _, identity)) => {
                lake.with_stream(identity);
                messages
            }
            Err(failed) => {
                tracing::warn!(%failed, "the write-ahead stream's reader could not be made");
                tokio::time::sleep(AGAIN_AFTER).await;
                continue;
            }
        };
        let mut stall = Stall::new(STALL_AFTER);
        loop {
            let next = writer.next();
            place.store(next, Ordering::Relaxed);
            let batch = match read(&mut messages, &mut stall, next, &mut stream, &writer).await {
                Read::Batch(batch) => batch,
                Read::TakenBack => {
                    tracing::info!(
                        next,
                        "a move's switch took the lake's place in the write-ahead stream back: \
                         taken again from there"
                    );
                    while let Err(failed) = tokio::task::block_in_place(|| writer.reread()) {
                        tracing::warn!(%failed, "the write-ahead position could not be read yet");
                        tokio::time::sleep(AGAIN_AFTER).await;
                    }
                    break;
                }
                Read::Stopped(failed) => {
                    tracing::warn!(%failed, "the write-ahead stream stopped; following it again");
                    break;
                }
                Read::Stalled { last } => {
                    tracing::warn!(
                        next = writer.next(),
                        last,
                        quiet_s = STALL_AFTER.as_secs(),
                        "the write-ahead stream holds writes past the lake's position and its \
                         reader delivered none: the reader is made again from the position"
                    );
                    break;
                }
            };
            // Kept to be tried again one write at a time (step 3d, group
            // commit; section 6.5): a batch is at most 512 KiB.
            let each = (batch.len() > 1).then(|| batch.clone());
            // Where a move's switch holds it past its bound, each write
            // this group answers is refused for it (review c5r M2).
            let held = writer.held_refusals(&batch);
            let took = taken_past_a_switch(&mut writer, batch).await;
            let took = match (took, each) {
                // An unexpected database error: each write tried alone, in
                // the stream's order, so the batch is held up only by the
                // write it was. One that fails alone too stops here, and the
                // writer goes on from the position the lake holds, as ever.
                (Err(StoreError::Unavailable(failed)), Some(each)) => {
                    tracing::warn!(
                        failed,
                        writes = each.len(),
                        "a batch from the write-ahead stream failed: each of its writes is tried alone"
                    );
                    let mut alone = Ok(Vec::new());
                    for write in each {
                        match tokio::task::block_in_place(|| writer.take(vec![write])) {
                            Ok(answers) => {
                                answer_when_settled(&lake, &client, &mut writer, answers).await
                            }
                            Err(failed) => {
                                alone = Err(failed);
                                break;
                            }
                        }
                    }
                    alone
                }
                (took, _) => took,
            };
            match took {
                Ok(answers) => answer_when_settled(&lake, &client, &mut writer, answers).await,
                Err(StoreError::Fenced(fenced)) => {
                    return Err(format!("the lake's writer was fenced: {fenced}"));
                }
                Err(failed) => {
                    if matches!(failed, StoreError::Held(_)) {
                        answer(&client, held).await;
                    }
                    tracing::warn!(
                        %failed,
                        "a batch from the write-ahead stream was not recorded; going on from the \
                         position the lake holds"
                    );
                    tokio::time::sleep(AGAIN_AFTER).await;
                    while let Err(failed) = tokio::task::block_in_place(|| writer.reread()) {
                        tracing::warn!(%failed, "the write-ahead position could not be read yet");
                        tokio::time::sleep(AGAIN_AFTER).await;
                    }
                    break;
                }
            }
        }
    }
}

/// Whether the broker said it holds no stream of that name: only then is
/// one made (v31a item 5).
fn not_found(failed: &jetstream::context::GetStreamError) -> bool {
    matches!(
        failed.kind(),
        jetstream::context::GetStreamErrorKind::JetStream(said)
            if said.error_code() == jetstream::ErrorCode::STREAM_NOT_FOUND
    )
}

/// The stream as it stands from the lake's place `next`
/// ([`guard::Standing::from_place`]): where its oldest write is one the
/// lake recorded and it holds writes the lake has not, the write at the
/// place is read from it for its time -- the oldest write the lake has not
/// recorded, whose age the age bound is held to.
async fn from_place(
    stream: &stream::Stream,
    standing: guard::Standing,
    next: u64,
) -> Result<guard::Standing, String> {
    if !standing.oldest_is_recorded(next) {
        return Ok(standing);
    }
    let at = stream
        .direct_get(next)
        .await
        .map_err(|failed| failed.to_string())?;
    let at_ns = i64::try_from(at.time.unix_timestamp_nanos()).unwrap_or(0);
    Ok(standing.from_place(next, at_ns))
}

/// The stream brought to this release's bounds where they differ from its
/// own, unless that would drop a write past the writer's place
/// ([`guard::bounds_drop`], v31a item 5), or the broker refuses them: then
/// it is kept as it is, and the refusal said naming it -- here, and in the
/// member's report (the words returned) -- until the writer starts again.
/// A byte bound the stream would have no room for a write under is not
/// applied either ([`guard::bytes_kept`]): the stream keeps its own byte bound, said the
/// same way, and the next writer's start applies it once it fits.
async fn bring_to_bounds(
    lake: &Arc<Lake>,
    context: &jetstream::Context,
    config: &stream::Config,
    bounds: &Bounds,
    largest_write: u64,
    writer: &Writer,
) -> Option<String> {
    let Ok(mut stream) = context.get_stream(STREAM).await else {
        tracing::warn!("the write-ahead stream could not be read to bring it to its bounds");
        return None;
    };
    let info = match stream.info().await {
        Ok(info) => info.clone(),
        Err(failed) => {
            tracing::warn!(%failed, "the write-ahead stream could not be read to bring it to its bounds");
            return None;
        }
    };
    let held = &info.config;
    let same = |config: &stream::Config| {
        (held.max_age, held.max_bytes, held.num_replicas)
            == (config.max_age, config.max_bytes, config.num_replicas)
    };
    if same(config) {
        return None;
    }
    let standing = guard::Standing::of(&info.state);
    // A byte bound under what the stream holds and the broker's largest
    // write is not applied: the stream keeps its own, and the rest of the
    // bounds are brought as ever.
    let mut config = config.clone();
    let bytes_kept = guard::bytes_kept(STREAM, &standing, held.max_bytes, bounds, largest_write);
    if let Some(kept) = &bytes_kept {
        tracing::warn!(stream = STREAM, "{kept}");
        config.max_bytes = held.max_bytes;
        if same(&config) {
            return bytes_kept;
        }
    }
    // The oldest write past the place is the one whose age a lower age
    // bound is held to, where the stream's oldest is one the lake recorded:
    // the lowest place among the writer groups, never this one's alone.
    let lowest =
        tokio::task::block_in_place(|| lowest_place(lake, writer.stream_name(), writer.next()));
    let next = match lowest {
        Ok(next) => next,
        Err(failed) => {
            tracing::warn!(
                %failed,
                "the write-ahead stream kept at its bounds: the writer groups' places could \
                 not be read"
            );
            return bytes_kept;
        }
    };
    let standing = match from_place(&stream, standing, next).await {
        Ok(standing) => standing,
        Err(failed) => {
            tracing::warn!(
                %failed,
                next,
                "the write-ahead stream kept at its bounds: its write at the lake's place \
                 could not be read"
            );
            return bytes_kept;
        }
    };
    if let Some(refused) = guard::bounds_drop(STREAM, &standing, next, bounds, lake.now_ns()) {
        tracing::warn!(stream = STREAM, next, "{refused}");
        return guard::said(Some(refused), bytes_kept.as_deref());
    }
    match context.update_stream(config.clone()).await {
        Ok(_) => {
            tracing::info!(
                max_age_s = config.max_age.as_secs(),
                max_bytes = config.max_bytes,
                "the write-ahead stream brought to this release's bounds"
            );
            bytes_kept
        }
        Err(failed) => {
            tracing::warn!(%failed, "the write-ahead stream kept at its bounds");
            guard::said(
                Some(format!(
                    "the write-ahead stream {STREAM} is kept as it is, not brought to this \
                     release's bounds: the broker refused them ({failed})"
                )),
                bytes_kept.as_deref(),
            )
        }
    }
}

/// A task ended when this is dropped: the guard ends with its writer.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The stream looked at every [`guard::LOOK_EVERY`] against the writer's
/// place (v31a item 5): stuck full, its member says so, degraded, with its
/// figures; drained, it says so no longer. The place is the lowest among
/// the writer groups' ([`lowest_place`]). Judged at each look by the
/// stream's own bounds, as its leader says them then, its age bound held
/// to the oldest write the lake has not recorded ([`from_place`]) -- never
/// the chart's,
/// which the stream does not hold where it was kept as it was (`kept`, the
/// refusal [`bring_to_bounds`] gave, which the member says for as long as
/// this writer runs). A look the broker does not answer changes nothing
/// said of the stream's bounds. Every look for [`guard::REFUSED_AFTER`]
/// refused or not answered, the member says that, its own state, until a
/// look finds a write accepted again ([`guard::Refused`]; INTERIM(v32), the
/// proposal of 2026-11-27, A1): after the stuck-full words where both are
/// said.
async fn guard_forever(
    context: jetstream::Context,
    largest_write: u64,
    lake: Arc<Lake>,
    place: Arc<AtomicU64>,
    base: String,
    kept: Option<String>,
    waiting: impl Fn(Option<String>) + Send + Sync + 'static,
) {
    let mut full = guard::Full::new(guard::STUCK_AFTER, largest_write);
    let mut refused = guard::Refused::new(guard::REFUSED_AFTER);
    // What the member says of the stream: stuck full, and refused.
    let (mut stuck, mut refusal) = (None::<String>, None::<String>);
    if kept.is_some() {
        waiting(kept.clone());
    }
    loop {
        tokio::time::sleep(guard::LOOK_EVERY).await;
        // The lowest place among the writer groups: the age bound drops a
        // write another group has not recorded as it does this one's.
        let mine = place.load(Ordering::Relaxed);
        let next = if lake.groups().several() {
            let (lake, base) = (Arc::clone(&lake), base.clone());
            let lowest =
                tokio::task::spawn_blocking(move || lowest_place(&lake, &base, mine)).await;
            match lowest {
                Ok(Ok(next)) => next,
                _ => {
                    tracing::debug!("the writer groups' places were not read at this look");
                    continue;
                }
            }
        } else {
            mine
        };
        // The stream, and from the lake's place: once the stream has
        // dropped or the lake recorded its oldest, the age held to its
        // bound is the write's at the place.
        let looked = async {
            let mut stream = context
                .get_stream(STREAM)
                .await
                .map_err(|failed| failed.to_string())?;
            let (state, held) = stream
                .info()
                .await
                .map(|info| (info.state.clone(), info.config.clone()))
                .map_err(|failed| failed.to_string())?;
            // Its write at the place not read, it is judged by its bytes
            // alone at this look.
            let standing = guard::Standing::of(&state);
            let standing = match from_place(&stream, standing, next).await {
                Ok(standing) => standing,
                Err(failed) => {
                    tracing::debug!(%failed, next, "the write at the lake's place was not read");
                    standing
                }
            };
            Ok::<_, String>((standing, held))
        };
        let was = (stuck.is_some(), refusal.is_some());
        // When the look was sent: a spell of refusals is dated from then,
        // not from an unanswered look's return at its timeout.
        let (sent, sent_ns) = (tokio::time::Instant::now(), lake.now_ns());
        match looked.await {
            Err(failed) => {
                tracing::debug!(%failed, "the write-ahead stream's guard could not look at it");
                refusal = refused.refused_look(sent, sent_ns, tokio::time::Instant::now(), &failed);
                if let (Some(words), false) = (&refusal, was.1) {
                    tracing::warn!(stream = STREAM, "{words}");
                }
            }
            Ok((standing, held)) => {
                refusal = refused.answered(tokio::time::Instant::now(), standing.last);
                if refusal.is_none() && was.1 {
                    tracing::info!(
                        stream = STREAM,
                        last = standing.last,
                        "the lake's write-ahead stream takes writes again: a write accepted"
                    );
                }
                let bounds = Bounds {
                    max_age: held.max_age,
                    max_bytes: held.max_bytes,
                    replicas: held.num_replicas,
                };
                stuck = full.looked(
                    tokio::time::Instant::now(),
                    &standing,
                    next,
                    &bounds,
                    lake.now_ns(),
                );
                match (&stuck, was.0) {
                    (Some(words), false) => tracing::warn!(stream = STREAM, "{words}"),
                    (None, true) => tracing::info!(
                        stream = STREAM,
                        bytes = standing.bytes,
                        past = standing.past(next),
                        "the lake's write-ahead stream drained: no longer at a bound"
                    ),
                    _ => {}
                }
            }
        }
        // Said while either stands, and once more as the last clears.
        if stuck.is_some() || refusal.is_some() || was.0 || was.1 {
            waiting(guard::said(
                guard::said(stuck.clone(), refusal.as_deref()),
                kept.as_deref(),
            ));
        }
    }
}

/// How often the writer, delivered nothing, asks the stream's leader how far
/// the stream has reached.
pub const STALL_LOOK_EVERY: Duration = Duration::from_secs(5);

/// How long the stream may hold writes past the writer's position, its
/// reader delivering none, before the reader is made again from the
/// position: a reader is never left silent (PR #113's failover-repeat
/// 43340543897, ha run 2: the reader placed on a replica 400 writes behind
/// delivered nothing for five minutes).
pub const STALL_AFTER: Duration = Duration::from_secs(15);

/// How many times the reader is made again to be placed on the stream's
/// leader before it is followed where it was placed, the stall check its
/// safety: NATS places an ephemeral consumer on any of the stream's peers
/// (a cluster of three placed one off the leader two times in three), so
/// 24 tries miss it about once in 17,000.
pub const PLACEMENT_TRIES: usize = 24;

/// The stream's reader, stopped or not: told each delivery, and each look
/// at how far the stream has reached.
#[derive(Debug)]
pub struct Stall {
    after: Duration,
    /// Since when the stream has held writes past the position with none
    /// delivered.
    since: Option<tokio::time::Instant>,
}

impl Stall {
    pub fn new(after: Duration) -> Self {
        Self { after, since: None }
    }

    /// A batch delivered: the reader reads.
    pub fn delivered(&mut self) {
        self.since = None;
    }

    /// The stream's last sequence `last`, as its leader says at `now`, and
    /// the writer's next `next`: whether its reader has delivered none of
    /// the writes past the position for [`Stall`]'s bound.
    pub fn looked(&mut self, now: tokio::time::Instant, last: u64, next: u64) -> bool {
        if last < next {
            self.since = None;
            return false;
        }
        let since = *self.since.get_or_insert(now);
        now.duration_since(since) >= self.after
    }
}

/// Where the stream's last sequence is asked: its leader, which answers a
/// stream's info.
pub trait Leader {
    fn last(&mut self) -> impl std::future::Future<Output = Result<u64, String>> + Send;
}

impl Leader for stream::Stream {
    async fn last(&mut self) -> Result<u64, String> {
        self.info()
            .await
            .map(|info| info.state.last_sequence)
            .map_err(|failed| failed.to_string())
    }
}

/// The writer's place in the stream, as the reader looks at it while it
/// delivers nothing (step 3d; spec 6.1 and 6.8).
pub trait Place {
    /// Whether a move's switch may yet take the place back: only then is
    /// the store asked.
    fn may_be_taken_back(&self) -> bool;
    /// Whether the store's place is no longer the writer's.
    fn taken_back(&self) -> impl std::future::Future<Output = bool> + Send;
}

impl Place for Writer {
    fn may_be_taken_back(&self) -> bool {
        Writer::may_be_taken_back(self)
    }

    async fn taken_back(&self) -> bool {
        match tokio::task::block_in_place(|| Writer::taken_back(self)) {
            Ok(taken_back) => taken_back,
            Err(failed) => {
                tracing::debug!(%failed, "the write-ahead position was not read to look whether a switch took it back");
                false
            }
        }
    }
}

/// What the reader gave.
#[derive(Debug)]
pub enum Read {
    Batch(Vec<Taken>),
    /// It ended, or failed.
    Stopped(String),
    /// Delivered nothing, the writer's place was taken back by a move's
    /// switch: the writer reads it again and takes the stream from there.
    TakenBack,
    /// The stream holds writes past the position, and it delivered none of
    /// them for [`STALL_AFTER`]: `last` is the stream's last sequence.
    Stalled {
        last: u64,
    },
}

/// The next batch the reader `messages` delivers; or, delivered none for
/// [`STALL_LOOK_EVERY`], the stream's `leader` asked how far it has reached,
/// and a reader that has delivered none of the writes past `next` for
/// `stall`'s bound said to have stalled -- placed on a replica left behind,
/// or lost -- never a quiet stream. While a move's switch may take the
/// writer's `place` back, it is looked at every [`TAKEN_BACK_LOOK_EVERY`]
/// the reader delivers nothing, and said taken back once it was (PR CI
/// 43810170103, ha: a price waited 6.65 s for the next write to find it).
pub async fn read<S, E, L, P>(
    messages: &mut S,
    stall: &mut Stall,
    next: u64,
    leader: &mut L,
    place: &P,
) -> Read
where
    S: futures_util::Stream<Item = Result<jetstream::Message, E>> + Unpin,
    E: std::fmt::Display,
    L: Leader,
    P: Place,
{
    let mut leader_at = tokio::time::Instant::now() + STALL_LOOK_EVERY;
    loop {
        let until = if place.may_be_taken_back() {
            leader_at.min(tokio::time::Instant::now() + TAKEN_BACK_LOOK_EVERY)
        } else {
            leader_at
        };
        match tokio::time::timeout_at(until, gather(messages)).await {
            Ok(Ok(batch)) => {
                stall.delivered();
                return Read::Batch(batch);
            }
            Ok(Err(failed)) => return Read::Stopped(failed),
            Err(_) => {
                if place.may_be_taken_back() && place.taken_back().await {
                    return Read::TakenBack;
                }
                if tokio::time::Instant::now() < leader_at {
                    continue;
                }
                leader_at = tokio::time::Instant::now() + STALL_LOOK_EVERY;
                match leader.last().await {
                    Ok(last) if stall.looked(tokio::time::Instant::now(), last, next) => {
                        return Read::Stalled { last };
                    }
                    Ok(_) => {}
                    Err(failed) => {
                        tracing::debug!(%failed, "the write-ahead stream's leader did not say how far it is");
                    }
                }
            }
        }
    }
}

/// Whether a reader placed on `placed` reads from the stream's leader
/// `leader`; with no cluster (one server), there is no other.
pub fn on_leader(leader: Option<&str>, placed: Option<&str>) -> bool {
    match (leader, placed) {
        (Some(leader), Some(placed)) => leader == placed,
        _ => true,
    }
}

/// The stream's reader from `from` (NATS's consumers read from the copy of
/// the stream on the server their leader is on, and an ephemeral one is R1,
/// placed on one of the stream's peers): a pull consumer as an ordered one
/// is -- ephemeral, in memory, no acknowledgements, gone 30 s after it is
/// let go -- made again until it is placed on the stream's leader, which
/// holds every write the stream has acknowledged. A follower left behind
/// would hold back what it has not caught up. After [`PLACEMENT_TRIES`] it
/// is followed where it is, the stall check its safety.
async fn follow(
    stream: &mut stream::Stream,
    from: u64,
) -> Result<(consumer::pull::Stream, bool, i64), String> {
    let mut tries = 0;
    loop {
        tries += 1;
        let info = stream.info().await.map_err(|failed| failed.to_string())?;
        let made_at = identity(info);
        let leader = info
            .cluster
            .as_ref()
            .and_then(|cluster| cluster.leader.clone());
        let mut made = stream
            .create_consumer(consumer::pull::Config {
                deliver_policy: consumer::DeliverPolicy::ByStartSequence {
                    start_sequence: from,
                },
                ack_policy: consumer::AckPolicy::None,
                inactive_threshold: Duration::from_secs(30),
                num_replicas: 1,
                memory_storage: true,
                ..Default::default()
            })
            .await
            .map_err(|failed| failed.to_string())?;
        let info = made.info().await.map_err(|failed| failed.to_string())?;
        let placed = info
            .cluster
            .as_ref()
            .and_then(|cluster| cluster.leader.clone());
        let on = on_leader(leader.as_deref(), placed.as_deref());
        if on || tries >= PLACEMENT_TRIES {
            if !on {
                tracing::warn!(
                    tries,
                    ?leader,
                    ?placed,
                    "the write-ahead stream's reader was not placed on its leader: followed \
                     where it is, the stall check its safety"
                );
            }
            return made
                .messages()
                .await
                .map(|messages| (messages, on, made_at))
                .map_err(|failed| failed.to_string());
        }
        let name = info.name.clone();
        if let Err(failed) = stream.delete_consumer(&name).await {
            tracing::debug!(%failed, "a reader placed off the leader was not let go: it lapses");
        }
        tracing::info!(
            tries,
            ?leader,
            ?placed,
            "the write-ahead stream's reader was placed off its leader: made again"
        );
    }
}

/// The writes the stream has sent, up to [`MOST_WRITES`]: waits for the
/// first, then takes what has come with it.
async fn gather<S, E>(messages: &mut S) -> Result<Vec<Taken>, String>
where
    S: futures_util::Stream<Item = Result<jetstream::Message, E>> + Unpin,
    E: std::fmt::Display,
{
    use futures_util::StreamExt as _;
    let mut batch = Vec::new();
    let first = messages
        .next()
        .await
        .ok_or("the stream's reader ended")?
        .map_err(|failed| failed.to_string())?;
    let mut bytes = first.payload.len();
    batch.push(taken(&first)?);
    // What has already come, and no wait for more: a write alone is
    // recorded at once, and under load the writes that came while the last
    // batch was recorded go together, until they reach 512 KiB (step 3d,
    // group commit; section 6.5): the stream hands a write over whole, so
    // the last one taken may carry the batch past it.
    while batch.len() < MOST_WRITES && bytes < BATCH_MOST_BYTES {
        match futures_util::FutureExt::now_or_never(messages.next()) {
            Some(Some(Ok(message))) => {
                bytes += message.payload.len();
                batch.push(taken(&message)?);
            }
            Some(Some(Err(failed))) => return Err(failed.to_string()),
            Some(None) => return Err("the stream's reader ended".into()),
            None => break,
        }
    }
    Ok(batch)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A caller's wait read from its header: a count of ms, trimmed; none
    /// where it is missing or no count (review c5r L5).
    #[test]
    fn a_callers_wait_is_read_from_its_header() {
        assert_eq!(timeout_of(Some("5000")), Some(5_000));
        assert_eq!(timeout_of(Some(" 2000 ")), Some(2_000));
        assert_eq!(timeout_of(Some("0")), Some(0));
        assert_eq!(timeout_of(Some("five")), None);
        assert_eq!(timeout_of(Some("-1")), None);
        assert_eq!(timeout_of(None), None);
    }

    /// When a write's answer is given by (review c5r L2, L5): its caller's
    /// wait from the stream's time less the way back; the sidecar's stamp
    /// only where the stream gave none; an age never below none or above
    /// the wait; none without a wait; never an overflow.
    #[tokio::test(start_paused = true)]
    async fn a_writes_answer_is_given_by_its_callers_wait_from_the_streams_time() {
        let now_ns: i64 = 1_000_000_000_000;
        let ms = |n: i64| n * 1_000_000;
        let write = |at_ns: i64, stamped: i64, timeout_ms: Option<u64>| Taken {
            sequence: 1,
            at_ns,
            topic: String::new(),
            reply_to: Some("_INBOX.1".into()),
            after: None,
            envelope: Envelope {
                meta: Some(meridian_bus::MessageMeta {
                    published_at_ns: stamped,
                    ..Default::default()
                }),
                ..Default::default()
            },
            dataset: String::new(),
            timeout_ms,
        };
        let left = |write: Taken| {
            write
                .answer_by(now_ns)
                .map(|by| by.duration_since(tokio::time::Instant::now()).as_millis())
        };
        // Taken by the stream 1 s ago, a 5 s caller: 3.5 s left.
        assert_eq!(
            left(write(now_ns - ms(1_000), now_ns - ms(1_000), Some(5_000))),
            Some(3_500)
        );
        // A stamp 10 s behind the stream's time (a sender's clock behind):
        // the stream's time decides.
        assert_eq!(
            left(write(now_ns, now_ns - ms(10_000), Some(5_000))),
            Some(4_500)
        );
        // No stream time: the stamp.
        assert_eq!(left(write(0, now_ns - ms(2_000), Some(5_000))), Some(2_500));
        // Neither: counted from now.
        assert_eq!(left(write(0, 0, Some(5_000))), Some(4_500));
        // From the future (the lake's clock behind): never more than the wait.
        assert_eq!(left(write(now_ns + ms(3_000), 0, Some(5_000))), Some(4_500));
        // Older than its wait, or a wait of 0: now.
        assert_eq!(left(write(now_ns - ms(9_000), 0, Some(5_000))), Some(0));
        assert_eq!(left(write(now_ns, 0, Some(0))), Some(0));
        // No wait said: none, the hold alone.
        assert_eq!(left(write(now_ns, now_ns, None)), None);
        // The largest wait: a day at most, no overflow.
        assert_eq!(
            left(write(now_ns, 0, Some(u64::MAX))),
            Some(86_400_000 - 500)
        );
    }

    /// A dataset whose ranges span groups is taken by each group owning one
    /// of its open ranges, each committing its own rows; the group of its
    /// own partition answers (step 3d; spec 6.1).
    #[test]
    fn a_dataset_split_across_groups_is_taken_by_each_and_answered_by_one() {
        use meridian_changes::groups::{owner, Groups};
        let dataset = (0..)
            .map(|n| format!("kraken-1:w{n}"))
            .find(|dataset| owner(dataset, 2) == 0)
            .unwrap();
        let write = Taken {
            sequence: 1,
            at_ns: 0,
            topic: String::new(),
            reply_to: Some("_INBOX.1".into()),
            after: None,
            envelope: Envelope::default(),
            dataset: dataset.clone(),
            timeout_ms: None,
        };
        let (zero, one) = (Groups::new(0, 2).unwrap(), Groups::new(1, 2).unwrap());
        assert!(
            ours(zero, &write) && !ours(one, &write),
            "whole: its owner's"
        );
        for groups in [zero, one] {
            groups.assign(&dataset, Some(0));
            groups.assign(&format!("{dataset}/0000-7fff"), Some(1));
            groups.assign(&format!("{dataset}/8000-ffff"), Some(0));
        }
        assert!(
            ours(zero, &write) && ours(one, &write),
            "split: each group's"
        );
        assert!(
            answers(zero, &write) && !answers(one, &write),
            "answered once"
        );
    }

    /// The stream's leader, stood in: how far it says the stream reached.
    struct Says(u64);

    impl Leader for Says {
        async fn last(&mut self) -> Result<u64, String> {
            Ok(self.0)
        }
    }

    type Delivered = futures_util::stream::Pending<Result<jetstream::Message, String>>;

    /// A place no move may take back.
    struct Still;

    impl Place for Still {
        fn may_be_taken_back(&self) -> bool {
            false
        }

        async fn taken_back(&self) -> bool {
            panic!("asked while no switch may take it back")
        }
    }

    /// A place a switch may take back, taken back at the `at`th look; it
    /// counts the looks.
    struct Looked {
        may: bool,
        at: usize,
        looks: std::sync::atomic::AtomicUsize,
    }

    impl Place for Looked {
        fn may_be_taken_back(&self) -> bool {
            self.may
        }

        async fn taken_back(&self) -> bool {
            self.looks.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 >= self.at
        }
    }

    /// A reader delivering nothing, its writer's place taken back by a
    /// move's switch, says so within a few looks -- never only once
    /// another write comes, nor at the stall look (PR CI 43810170103, ha: a
    /// price waited 6.65 s for the next write). The store is asked only
    /// while a switch may take the place back.
    #[tokio::test(start_paused = true)]
    async fn a_quiet_reader_whose_place_was_taken_back_says_so_within_its_looks() {
        let mut quiet: Delivered = futures_util::stream::pending();
        let mut stall = Stall::new(STALL_AFTER);
        let place = Looked {
            may: true,
            at: 3,
            looks: Default::default(),
        };
        let began = tokio::time::Instant::now();
        let said = tokio::time::timeout(
            STALL_LOOK_EVERY,
            read(&mut quiet, &mut stall, 10, &mut Says(9), &place),
        )
        .await;
        assert!(matches!(said, Ok(Read::TakenBack)), "{said:?}");
        assert_eq!(began.elapsed(), TAKEN_BACK_LOOK_EVERY * 3);
        assert!(TAKEN_BACK_LOOK_EVERY * 3 < STALL_LOOK_EVERY);
        // No switch may take it back: never asked, the reader reads on.
        let mut quiet: Delivered = futures_util::stream::pending();
        let mut stall = Stall::new(STALL_AFTER);
        let place = Looked {
            may: false,
            at: 1,
            looks: Default::default(),
        };
        let waited = tokio::time::timeout(
            STALL_AFTER * 4,
            read(&mut quiet, &mut stall, 10, &mut Says(9), &place),
        )
        .await;
        assert!(waited.is_err(), "a quiet reader kept reading");
        assert_eq!(place.looks.into_inner(), 0, "the store asked for nothing");
    }

    #[test]
    fn a_reader_is_on_the_leader_only_where_it_was_placed_there() {
        assert!(on_leader(Some("js-0"), Some("js-0")));
        assert!(
            !on_leader(Some("js-0"), Some("js-2")),
            "a follower, maybe left behind"
        );
        assert!(on_leader(None, None), "one server: no other");
    }

    #[test]
    fn a_stall_is_writes_past_the_position_with_none_delivered_for_its_bound() {
        let at = tokio::time::Instant::now();
        let mut stall = Stall::new(Duration::from_secs(15));
        // A quiet stream: nothing past the position, never a stall.
        assert!(!stall.looked(at, 4, 5));
        assert!(!stall.looked(at + Duration::from_secs(60), 4, 5));
        // Writes past it, none delivered: a stall once the bound has passed.
        assert!(!stall.looked(at + Duration::from_secs(61), 9, 5));
        assert!(!stall.looked(at + Duration::from_secs(70), 9, 5));
        assert!(stall.looked(at + Duration::from_secs(76), 9, 5));
        // A delivery starts it again.
        stall.delivered();
        assert!(!stall.looked(at + Duration::from_secs(80), 12, 10));
    }

    /// A reader that stops delivering -- placed on a replica left behind,
    /// as failover-repeat 43340543897 found -- is said to have stalled once
    /// the leader says writes are past the position, within its bound.
    #[tokio::test(start_paused = true)]
    async fn a_reader_delivering_nothing_past_the_position_is_said_to_have_stalled() {
        let mut stopped: Delivered = futures_util::stream::pending();
        let mut stall = Stall::new(STALL_AFTER);
        let began = tokio::time::Instant::now();
        match read(&mut stopped, &mut stall, 593, &mut Says(609), &Still).await {
            Read::Stalled { last } => assert_eq!(last, 609),
            other => panic!("not said to have stalled: {other:?}"),
        }
        let took = began.elapsed();
        assert!(
            took >= STALL_AFTER && took <= STALL_AFTER + STALL_LOOK_EVERY * 2,
            "{took:?}"
        );
        // A quiet stream -- nothing past the position -- is never a stall.
        let mut quiet: Delivered = futures_util::stream::pending();
        let mut stall = Stall::new(STALL_AFTER);
        let waited = tokio::time::timeout(
            STALL_AFTER * 10,
            read(&mut quiet, &mut stall, 610, &mut Says(609), &Still),
        )
        .await;
        assert!(waited.is_err(), "a quiet stream's reader kept reading");
    }

    /// The reader against a real JetStream (by hand: a broker with
    /// JetStream at MERIDIAN_TEST_JETSTREAM_URL): made from a position, it
    /// delivers the writes after it in the stream's order.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a JetStream broker: MERIDIAN_TEST_JETSTREAM_URL"]
    async fn the_reader_delivers_a_real_stream_from_its_position_in_order() {
        let url = std::env::var("MERIDIAN_TEST_JETSTREAM_URL").unwrap();
        let client = async_nats::connect(&url).await.unwrap();
        let context = jetstream::new(client);
        let name = format!("FOLLOW_TEST_{}", std::process::id());
        let mut stream = context
            .create_stream(stream::Config {
                name: name.clone(),
                subjects: vec![format!("follow-test.{}.>", std::process::id())],
                ..Default::default()
            })
            .await
            .unwrap();
        for n in 1..=5 {
            context
                .publish(
                    format!("follow-test.{}.{n}", std::process::id()),
                    vec![n as u8].into(),
                )
                .await
                .unwrap()
                .await
                .unwrap();
        }
        let (mut messages, on, made_at) = follow(&mut stream, 2).await.unwrap();
        assert!(on, "one server: its leader");
        // The identity is read with the reader, never 0 (review c5a3 L2).
        assert_ne!(made_at, 0);
        assert_eq!(made_at, identity(stream.info().await.unwrap()));
        let mut seen = Vec::new();
        while seen.len() < 4 {
            let message = futures_util::StreamExt::next(&mut messages)
                .await
                .unwrap()
                .unwrap();
            seen.push(message.info().unwrap().stream_sequence);
        }
        assert_eq!(seen, vec![2, 3, 4, 5]);
        let mut stall = Stall::new(STALL_AFTER);
        assert!(!stall.looked(tokio::time::Instant::now(), stream.last().await.unwrap(), 6));
        context.delete_stream(&name).await.unwrap();
    }

    /// The reader on a stream of three replicas (by hand: a JetStream
    /// cluster of three at MERIDIAN_TEST_JETSTREAM_CLUSTER_URL): placed on
    /// the stream's leader each time it is made, never on a follower that
    /// may be left behind.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a JetStream cluster of three: MERIDIAN_TEST_JETSTREAM_CLUSTER_URL"]
    async fn the_reader_is_placed_on_the_streams_leader() {
        let url = std::env::var("MERIDIAN_TEST_JETSTREAM_CLUSTER_URL").unwrap();
        let client = async_nats::connect(&url).await.unwrap();
        let context = jetstream::new(client);
        let name = format!("PLACED_TEST_{}", std::process::id());
        let mut stream = context
            .create_stream(stream::Config {
                name: name.clone(),
                subjects: vec![format!("placed-test.{}.>", std::process::id())],
                num_replicas: 3,
                ..Default::default()
            })
            .await
            .unwrap();
        for _ in 0..12 {
            let (_, on, _) = follow(&mut stream, 1).await.unwrap();
            assert!(on, "placed off the stream's leader");
        }
        context.delete_stream(&name).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_reader_that_ended_is_said_to_have_stopped() {
        let mut ended = futures_util::stream::empty::<Result<jetstream::Message, String>>();
        let mut stall = Stall::new(STALL_AFTER);
        assert!(matches!(
            read(&mut ended, &mut stall, 1, &mut Says(0), &Still).await,
            Read::Stopped(_)
        ));
    }

    #[test]
    fn the_stream_refuses_a_write_when_full_never_dropping_its_oldest() {
        let config = stream_config(&Bounds {
            max_age: MAX_AGE,
            max_bytes: MAX_BYTES,
            replicas: 3,
        });
        assert_eq!(config.discard, stream::DiscardPolicy::New);
        assert_eq!(config.retention, stream::RetentionPolicy::Limits);
        assert_eq!(config.storage, stream::StorageType::File);
        assert_eq!((config.num_replicas, config.max_bytes), (3, MAX_BYTES));
        assert_eq!(config.max_age, MAX_AGE);
        assert_eq!(
            config.subjects,
            vec!["platform.lake.internal.write-ahead.>".to_string()]
        );
    }

    #[test]
    fn the_bounds_are_the_charts_or_these_and_a_bound_that_is_none_is_refused() {
        let said = Bounds::from_said(Some("90s"), Some("64MB"), 3, 1 << 20).unwrap();
        assert_eq!(
            (said.max_age, said.max_bytes),
            (Duration::from_secs(90), 64 << 20)
        );
        assert_eq!(
            Bounds::from_said(Some("2h"), Some("1GB"), 3, 1 << 20)
                .unwrap()
                .max_age,
            Duration::from_secs(7_200)
        );
        let none = Bounds::from_said(None, Some(""), 3, 1 << 20).unwrap();
        assert_eq!((none.max_age, none.max_bytes), (MAX_AGE, MAX_BYTES));
        for (age, bytes, named) in [
            ("0s", "", "maxAge"),
            ("10", "", "maxAge"),
            ("10d", "", "maxAge"),
            ("", "0MB", "maxBytes"),
            ("", "256Mi", "maxBytes"),
            ("", "lots", "maxBytes"),
        ] {
            let refused = Bounds::from_said(Some(age), Some(bytes), 3, 1 << 20).unwrap_err();
            assert!(refused.contains(named), "{age} {bytes}: {refused}");
        }
    }

    /// The review of the stream guard, finding 5: a byte bound under 16 of
    /// the broker's largest message is refused, naming both.
    #[test]
    fn a_byte_bound_under_sixteen_of_the_brokers_largest_message_is_refused() {
        for small in ["512KB", "15MB", "16777215"] {
            let refused = Bounds::from_said(None, Some(small), 3, 1 << 20).unwrap_err();
            assert!(
                refused.contains("lake.writeAhead.maxBytes")
                    && refused.contains("broker.limits.maxPayload, 1048576 bytes"),
                "{small}: {refused}"
            );
        }
        let least = Bounds::from_said(None, Some("16MB"), 3, 1 << 20).unwrap();
        assert_eq!(least.max_bytes, 16 << 20);
        // The broker's largest message as the broker says it: 8 MiB takes
        // 128 MiB, and this module's 256 MiB where the chart says nothing.
        assert!(Bounds::from_said(None, Some("64MB"), 3, 8 << 20).is_err());
        assert!(Bounds::from_said(None, None, 3, 8 << 20).is_ok());
        assert!(Bounds::from_said(None, None, 3, 32 << 20).is_err());
    }
}
