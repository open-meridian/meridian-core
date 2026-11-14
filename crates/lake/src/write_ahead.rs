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
use std::sync::Arc;
use std::time::Duration;

use async_nats::jetstream::{self, consumer, stream};
use meridian_bus::write_ahead::{
    stream_subjects, AFTER_HEADER, REPLY_HEADER, STREAM, TOPIC_HEADER,
};
use meridian_bus::{Envelope, HandlerReply};
use prost::Message as _;

use crate::service::Lake;
use crate::store::{Dropped, StoreError, WriteAheadBatch, WriteAheadPosition};

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
}

/// Where a writer group keeps its position in the stream (step 3d; section
/// 6.1): group 0 under the stream's own name, as every lake before groups
/// kept it; each group past the first under `<stream>.group-<g>`, as its
/// queue group is named.
pub fn position_key(stream: &str, groups: meridian_changes::groups::Groups) -> String {
    match groups.mine() {
        0 => stream.to_string(),
        group => format!("{stream}.group-{group}"),
    }
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
}

/// What [`crate::service::Lake::record_written`] did, once committed: each
/// write's answer, and each dataset's latest valid time it recorded.
pub struct Recorded {
    pub answers: Vec<HandlerReply>,
    pub valid: Vec<(String, i64)>,
}

/// The writer's place in the stream: the position the store holds, which
/// each batch moves, and what the stream dropped that the lake has found.
pub struct Writer {
    lake: Arc<Lake>,
    stream: String,
    position: Option<WriteAheadPosition>,
    /// The sequence to take from next where it is not the one after the
    /// position: a stream made again, which numbers from its own first.
    from: Option<u64>,
    dropped: Vec<Dropped>,
    valid: BTreeMap<String, i64>,
}

impl Writer {
    /// The writer of `lake`, at the position its store holds in `stream`,
    /// with what it found the stream dropped.
    pub fn start(lake: Arc<Lake>, stream: &str) -> Result<Writer, StoreError> {
        // Each writer group's own position (step 3d; section 6.1).
        let stream = position_key(stream, lake.groups());
        let position = lake.write_ahead_position(&stream)?;
        let (dropped, valid) = lake.write_ahead_found(&stream)?;
        Ok(Writer {
            lake,
            stream,
            position,
            from: None,
            dropped,
            valid,
        })
    }

    /// The position the writer goes on from.
    pub fn position(&self) -> Option<WriteAheadPosition> {
        self.position
    }

    /// The first sequence the writer takes next.
    pub fn next(&self) -> u64 {
        self.from
            .unwrap_or_else(|| self.position.map_or(1, |at| at.sequence + 1))
    }

    /// Take from `sequence` next, whatever the position: the stream's own
    /// first, where the stream is not the one the position was taken in.
    pub fn begin_at(&mut self, sequence: u64) {
        self.from = Some(sequence);
    }

    /// The position read again from the store: after a batch it refused.
    pub fn reread(&mut self) -> Result<(), StoreError> {
        self.position = self.lake.write_ahead_position(&self.stream)?;
        let (dropped, valid) = self.lake.write_ahead_found(&self.stream)?;
        self.dropped = dropped;
        self.valid = valid;
        self.from = None;
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
        let Some(last) = taken.last() else {
            return Ok(Vec::new());
        };
        let through = WriteAheadPosition {
            sequence: last.sequence,
            at_ns: last.at_ns,
        };
        let mut expected = self.next();
        let mut before_at = self.position.map_or(0, |at| at.at_ns);
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
        let taken: Vec<Taken> = taken
            .into_iter()
            .filter(|write| ours(groups, write))
            .collect();
        let mut sent_to = Vec::with_capacity(taken.len());
        let mut writes = Vec::with_capacity(taken.len());
        for write in taken {
            let after_gap = write.after.and_then(|after| {
                self.dropped
                    .iter()
                    .chain(&dropped)
                    .find(|run| run.holds(after))
                    .cloned()
            });
            let answering = answers(groups, &write);
            sent_to.push(write.reply_to.filter(|_| answering));
            writes.push(Written {
                topic: write.topic,
                envelope: write.envelope,
                after_gap,
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
        self.position = Some(through);
        self.from = None;
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
        reply_to: header(REPLY_HEADER).filter(|to| !to.is_empty()),
        after: header(AFTER_HEADER).and_then(|said| said.parse().ok()),
        envelope: Envelope::decode(message.payload.clone()).unwrap_or_default(),
        dataset: meridian_bus::write_ahead::dataset_of_subject(&message.subject)
            .unwrap_or_default(),
    })
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
                headers.insert(meridian_bus::write_ahead::REFUSAL_HEADER, detail.as_str());
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

impl Bounds {
    /// The chart's bounds, `lake.writeAhead.maxAge` as `30s`, `10m` or `1h`
    /// and `lake.writeAhead.maxBytes` as `256MB` or `1GB`, each above 0;
    /// this module's where it says nothing. Refused naming the value.
    pub fn from_said(
        max_age: Option<&str>,
        max_bytes: Option<&str>,
        replicas: usize,
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
pub async fn write_forever(
    lake: Arc<Lake>,
    client: async_nats::Client,
    bounds: Bounds,
    waiting: impl Fn(Option<String>),
) -> Result<(), String> {
    let context = jetstream::new(client.clone());
    let config = stream_config(&bounds);
    // Made, or brought to the bounds this release sets: never with fewer
    // replicas than set, to get past a broker not yet ready (step 3d). Until
    // then the lake takes no write, and says why in its report.
    loop {
        let made = match context.get_stream(STREAM).await {
            Ok(_) => context
                .update_stream(config.clone())
                .await
                .map(|_| ())
                .map_err(|f| f.to_string()),
            Err(_) => context
                .create_stream(config.clone())
                .await
                .map(|_| ())
                .map_err(|f| f.to_string()),
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
    let mut writer = loop {
        let started = tokio::task::block_in_place(|| Writer::start(Arc::clone(&lake), STREAM));
        match started {
            Ok(writer) => break writer,
            Err(failed) => {
                tracing::warn!(%failed, "the write-ahead position could not be read yet");
                tokio::time::sleep(AGAIN_AFTER).await;
            }
        }
    };
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
        // A stream made again -- its volume lost, or JetStream off and on
        // again -- numbers from its own first: the position was taken in
        // another, and what that one held past it is gone.
        if let (Ok(info), Some(at)) = (stream.info().await, writer.position()) {
            let state = &info.state;
            if at.sequence > state.last_sequence && writer.next() > state.first_sequence {
                tracing::error!(
                    position = at.sequence,
                    last = state.last_sequence,
                    "the lake's write-ahead stream is not the one its position was taken \
                     in: going on from its first write; what the stream held past the \
                     position before it was made again is not recorded"
                );
                writer.begin_at(state.first_sequence.max(1));
            }
        }
        let mut messages = match follow(&mut stream, writer.next()).await {
            Ok((messages, _)) => messages,
            Err(failed) => {
                tracing::warn!(%failed, "the write-ahead stream's reader could not be made");
                tokio::time::sleep(AGAIN_AFTER).await;
                continue;
            }
        };
        let mut stall = Stall::new(STALL_AFTER);
        loop {
            let batch = match read(&mut messages, &mut stall, writer.next(), &mut stream).await {
                Read::Batch(batch) => batch,
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
            let took = tokio::task::block_in_place(|| writer.take(batch));
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
                            Ok(answers) => answer(&client, answers).await,
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
                Ok(answers) => answer(&client, answers).await,
                Err(StoreError::Fenced(fenced)) => {
                    return Err(format!("the lake's writer was fenced: {fenced}"));
                }
                Err(failed) => {
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

/// What the reader gave.
#[derive(Debug)]
pub enum Read {
    Batch(Vec<Taken>),
    /// It ended, or failed.
    Stopped(String),
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
/// or lost -- never a quiet stream.
pub async fn read<S, E, L>(messages: &mut S, stall: &mut Stall, next: u64, leader: &mut L) -> Read
where
    S: futures_util::Stream<Item = Result<jetstream::Message, E>> + Unpin,
    E: std::fmt::Display,
    L: Leader,
{
    loop {
        match tokio::time::timeout(STALL_LOOK_EVERY, gather(messages)).await {
            Ok(Ok(batch)) => {
                stall.delivered();
                return Read::Batch(batch);
            }
            Ok(Err(failed)) => return Read::Stopped(failed),
            Err(_) => match leader.last().await {
                Ok(last) if stall.looked(tokio::time::Instant::now(), last, next) => {
                    return Read::Stalled { last };
                }
                Ok(_) => {}
                Err(failed) => {
                    tracing::debug!(%failed, "the write-ahead stream's leader did not say how far it is");
                }
            },
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
) -> Result<(consumer::pull::Stream, bool), String> {
    let mut tries = 0;
    loop {
        tries += 1;
        let leader = stream
            .info()
            .await
            .map_err(|failed| failed.to_string())?
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
                .map(|messages| (messages, on))
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
        };
        let (zero, one) = (Groups::new(0, 2).unwrap(), Groups::new(1, 2).unwrap());
        assert!(ours(zero, &write) && !ours(one, &write), "whole: its owner's");
        for groups in [zero, one] {
            groups.assign(&dataset, Some(0));
            groups.assign(&format!("{dataset}/0000-7fff"), Some(1));
            groups.assign(&format!("{dataset}/8000-ffff"), Some(0));
        }
        assert!(ours(zero, &write) && ours(one, &write), "split: each group's");
        assert!(answers(zero, &write) && !answers(one, &write), "answered once");
    }

    /// The stream's leader, stood in: how far it says the stream reached.
    struct Says(u64);

    impl Leader for Says {
        async fn last(&mut self) -> Result<u64, String> {
            Ok(self.0)
        }
    }

    type Delivered = futures_util::stream::Pending<Result<jetstream::Message, String>>;

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
        match read(&mut stopped, &mut stall, 593, &mut Says(609)).await {
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
            read(&mut quiet, &mut stall, 610, &mut Says(609)),
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
        let (mut messages, on) = follow(&mut stream, 2).await.unwrap();
        assert!(on, "one server: its leader");
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
            let (_, on) = follow(&mut stream, 1).await.unwrap();
            assert!(on, "placed off the stream's leader");
        }
        context.delete_stream(&name).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_reader_that_ended_is_said_to_have_stopped() {
        let mut ended = futures_util::stream::empty::<Result<jetstream::Message, String>>();
        let mut stall = Stall::new(STALL_AFTER);
        assert!(matches!(
            read(&mut ended, &mut stall, 1, &mut Says(0)).await,
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
        let said = Bounds::from_said(Some("90s"), Some("64MB"), 3).unwrap();
        assert_eq!(
            (said.max_age, said.max_bytes),
            (Duration::from_secs(90), 64 << 20)
        );
        assert_eq!(
            Bounds::from_said(Some("2h"), Some("1GB"), 3)
                .unwrap()
                .max_age,
            Duration::from_secs(7_200)
        );
        let none = Bounds::from_said(None, Some(""), 3).unwrap();
        assert_eq!((none.max_age, none.max_bytes), (MAX_AGE, MAX_BYTES));
        for (age, bytes, named) in [
            ("0s", "", "maxAge"),
            ("10", "", "maxAge"),
            ("10d", "", "maxAge"),
            ("", "0MB", "maxBytes"),
            ("", "256Mi", "maxBytes"),
            ("", "lots", "maxBytes"),
        ] {
            let refused = Bounds::from_said(Some(age), Some(bytes), 3).unwrap_err();
            assert!(refused.contains(named), "{age} {bytes}: {refused}");
        }
    }
}
