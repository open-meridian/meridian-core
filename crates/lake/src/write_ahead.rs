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
use meridian_bus::write_ahead::{AFTER_HEADER, REPLY_HEADER, STREAM, SUBJECTS, TOPIC_HEADER};
use meridian_bus::{Envelope, HandlerReply};
use prost::Message as _;

use crate::service::Lake;
use crate::store::{Dropped, StoreError, WriteAheadBatch, WriteAheadPosition};

/// The most writes taken in one batch.
pub const MOST_WRITES: usize = 256;

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
        let position = lake.write_ahead_position(stream)?;
        let (dropped, valid) = lake.write_ahead_found(stream)?;
        Ok(Writer {
            lake,
            stream: stream.to_string(),
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
                dropped.push(Dropped {
                    first: expected,
                    last: write.sequence - 1,
                    after_at_ns: before_at,
                    until_at_ns: write.at_ns,
                });
            }
            expected = write.sequence + 1;
            before_at = write.at_ns;
        }
        for run in &dropped {
            tracing::warn!(
                first = run.first,
                last = run.last,
                "the lake's write-ahead stream dropped writes before the lake took them: \
                 a writer gone longer than the stream's bound"
            );
        }
        let mut sent_to = Vec::with_capacity(taken.len());
        let mut writes = Vec::with_capacity(taken.len());
        for write in taken {
            let after_gap = write.after.and_then(|after| {
                self.dropped
                    .iter()
                    .chain(&dropped)
                    .find(|run| run.holds(after))
                    .copied()
            });
            sent_to.push(write.reply_to);
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
/// subject of [`SUBJECTS`], kept by its bounds and never past them.
pub fn stream_config(bounds: &Bounds) -> stream::Config {
    stream::Config {
        name: STREAM.to_string(),
        subjects: vec![format!("{SUBJECTS}.>")],
        retention: stream::RetentionPolicy::Limits,
        storage: stream::StorageType::File,
        num_replicas: bounds.replicas,
        max_age: bounds.max_age,
        max_bytes: bounds.max_bytes,
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

/// Take the stream's writes, record them and answer them, for as long as
/// this member writes: from the position the store holds, and back to it
/// whenever a batch is refused. Ends only with an error the writer cannot
/// go on from.
pub async fn write_forever(
    lake: Arc<Lake>,
    client: async_nats::Client,
    bounds: Bounds,
) -> Result<(), String> {
    let context = jetstream::new(client.clone());
    let config = stream_config(&bounds);
    // Made, or brought to the bounds this release sets.
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
            Ok(()) => break,
            Err(failed) => {
                tracing::warn!(%failed, "the lake's write-ahead stream could not be made yet");
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
    match tokio::task::block_in_place(|| lake.resume_gaps()) {
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
        let consumer = match context.get_stream(STREAM).await {
            Ok(mut stream) => {
                // A stream made again -- its volume lost, or JetStream off and
                // on again -- numbers from its own first: the position was
                // taken in another, and what that one held past it is gone.
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
                let from = writer.next();
                stream
                    .create_consumer(consumer::pull::OrderedConfig {
                        deliver_policy: consumer::DeliverPolicy::ByStartSequence {
                            start_sequence: from,
                        },
                        ..Default::default()
                    })
                    .await
            }
            Err(failed) => {
                tracing::warn!(%failed, "the write-ahead stream could not be read");
                tokio::time::sleep(AGAIN_AFTER).await;
                continue;
            }
        };
        let mut messages = match consumer {
            Ok(consumer) => match consumer.messages().await {
                Ok(messages) => messages,
                Err(failed) => {
                    tracing::warn!(%failed, "the write-ahead stream could not be followed");
                    tokio::time::sleep(AGAIN_AFTER).await;
                    continue;
                }
            },
            Err(failed) => {
                tracing::warn!(%failed, "the write-ahead stream's reader could not be made");
                tokio::time::sleep(AGAIN_AFTER).await;
                continue;
            }
        };
        loop {
            let batch = match gather(&mut messages).await {
                Ok(batch) => batch,
                Err(failed) => {
                    tracing::warn!(%failed, "the write-ahead stream stopped; following it again");
                    break;
                }
            };
            let took = tokio::task::block_in_place(|| writer.take(batch));
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

/// The writes the stream has sent, up to [`MOST_WRITES`]: waits for the
/// first, then takes what has come with it.
async fn gather(messages: &mut consumer::pull::Ordered) -> Result<Vec<Taken>, String> {
    use futures_util::StreamExt as _;
    let mut batch = Vec::new();
    let first = messages
        .next()
        .await
        .ok_or("the stream's reader ended")?
        .map_err(|failed| failed.to_string())?;
    batch.push(taken(&first)?);
    // What has already come, and no wait for more: a write alone is
    // recorded at once, and under load the writes that came while the last
    // batch was recorded go together.
    while batch.len() < MOST_WRITES {
        match futures_util::FutureExt::now_or_never(messages.next()) {
            Some(Some(Ok(message))) => batch.push(taken(&message)?),
            Some(Some(Err(failed))) => return Err(failed.to_string()),
            Some(None) => return Err("the stream's reader ended".into()),
            None => break,
        }
    }
    Ok(batch)
}
