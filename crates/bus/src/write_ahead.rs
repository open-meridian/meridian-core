//! The lake's write-ahead stream, at the sidecar (the `ha` profile; spec/a-
//! deployment-scales-out-and-fails-over-by-configuration section 4; the
//! rulings of 2026-11-08 and 2026-11-09; decisions/024 as amended
//! 2026-11-09).
//!
//! In `ha` a data plugin's lake writes go to a JetStream stream kept on the
//! broker's three servers, one subject a dataset, before the lake records
//! them -- the market-data convention of a durable log ahead of the store,
//! as a tickerplant logs before its database. The lake's writer takes them
//! from the stream, records each batch with the stream's position in one
//! transaction, and answers each write as it always has (W10.4): the reply
//! is unchanged, what the batch did once it committed. A write is answered
//! only once the stream holds it, so a plugin, a lake member, a broker
//! server or the database dying after its answer loses nothing; the writer
//! that comes next goes on from the position the lake committed.
//!
//! This is a [`Backend`] beside the broker's own: the bus routes the lake's
//! record topics to it ([`TOPICS`], by [`crate::RouteRule`]), and everything
//! else it is asked goes to the backend it stands in front of. Nothing above
//! the bus changes, which is the transport seam decision 010 made: a second
//! data-plane transport is another backend, routed the same way.
//!
//! **Limits** (the research batch's item 5, approved 2026-11-10): the stream
//! refuses a write when it is full (`discard: new`; JetStream's default
//! drops its oldest messages, which here are writes held for the lake, and
//! is never used), and the plugin is refused for load with a retry-after;
//! and a write whose caller has already stopped waiting -- or that the
//! broker could not take now, so its client would queue it -- is dropped
//! here, never sent, so the stream holds no write nobody waits for.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_nats::jetstream;
use meridian_domain::v1::Envelope;
use prost::Message as _;
use tokio::sync::{oneshot, Notify};

use crate::backend::{Answer, Backend, BusError, Handler, Subscription};

/// The stream's name.
pub const STREAM: &str = "LAKE_WRITE_AHEAD";

/// The words the lake's writer says its stream is stuck full with, first in
/// its member's report (`ComponentReport.detail`; v31a item 5, 0-J): what
/// the dashboard's judge files its standing notice from, and `meridian
/// status` shows. The lake's guard says it (`meridian_lake::write_ahead::guard`).
pub const STUCK_FULL: &str = "the lake's write-ahead stream is stuck full";

/// How the stuck-full words say since when, after [`STUCK_FULL`] and for
/// how long: `, since 2026-11-24T10:00:00Z`. The refused words say it the
/// same way, after [`REFUSED`].
const STUCK_SINCE: &str = ", since ";

/// The words the lake's writer says the broker refuses its write-ahead
/// stream with, or does not answer it -- every look its guard took at the
/// stream for 30 s refused or unanswered -- in its member's report
/// (`ComponentReport.detail`), after the stuck-full words where both are
/// said: what the dashboard's judge files `component-write-ahead-refused`
/// from, and `meridian status` shows (INTERIM(v32), the proposal of
/// 2026-11-27, A1: v32 takes the stream off the lake's write path). Why
/// the broker refuses is not inferred here.
pub const REFUSED: &str = "the lake's write-ahead writes are refused or not answered by the broker";

/// The words the lake's writer says its stream is at its rate limit with,
/// first in its member's report (`ComponentReport.detail`): the stream at
/// its byte bound while the lake keeps up with it. A write the lake has
/// recorded leaves the stream only at its age bound, so the stream takes
/// no more than its byte bound every age bound, however fast the lake is.
/// Never said with [`STUCK_FULL`]: stuck full is the stream at a bound with
/// the lake behind. What the dashboard's judge files
/// `component-write-ahead-rate-limited` from, and `meridian status` shows.
/// The name is a candidate, adopted by consensus 2026-11-28.
/// INTERIM(v32): this state and its notices go when v32 takes the stream off the lake's write path.
pub const RATE_LIMITED: &str = "the lake's write-ahead stream is at its rate limit";

/// A plugin's refusal where the stream has no room for its write
/// (`full_words`).
const FULL: &str = "overloaded: the lake's write-ahead stream is full and takes a new write \
                        only as the oldest writes it holds reach its maximum age and leave";

/// The words a stuck-full stream is said with open so: [`STUCK_FULL`], for
/// how long, and the time it went stuck, to the second, UTC. That time
/// names the stuck spell: every copy of the dashboard reads the same one
/// from the same member, and files the spell's notices once
/// ([`stuck_since`]).
pub fn stuck_full_since(stuck_for_s: u64, since_ns: i64) -> String {
    said_since(STUCK_FULL, stuck_for_s, since_ns)
}

/// The words a refused stream is said with open so: [`REFUSED`], for how
/// long, and the time of the first refusal of the spell, to the second,
/// UTC, which names the spell as the stuck-full time names its
/// ([`refused_since`]). INTERIM(v32).
pub fn refused_for(refused_for_s: u64, since_ns: i64) -> String {
    said_since(REFUSED, refused_for_s, since_ns)
}

/// The words a stream at its rate limit is said with open so:
/// [`RATE_LIMITED`], for how long, and the time of the first look that
/// found it so, to the second, UTC, which names the spell as the stuck-full
/// time names its ([`rate_limited_since`]).
pub fn rate_limited_for(limited_for_s: u64, since_ns: i64) -> String {
    said_since(RATE_LIMITED, limited_for_s, since_ns)
}

/// `said`, for how long and since when ([`stuck_full_since`]).
fn said_since(said: &str, for_s: u64, since_ns: i64) -> String {
    let seconds = since_ns.div_euclid(1_000_000_000);
    let (days, of_day) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    // Days since 1970 to a civil date (Howard Hinnant's civil-from-days).
    let shifted = days + 719_468;
    let (era, day_of_era) = (shifted.div_euclid(146_097), shifted.rem_euclid(146_097));
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{said} for {for_s}s{STUCK_SINCE}{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        of_day / 3_600,
        of_day % 3_600 / 60,
        of_day % 60
    )
}

/// The time a member's words say its stream went stuck, ns since the
/// epoch ([`stuck_full_since`]): none where they do not say it stuck full,
/// or say no time that reads.
pub fn stuck_since(detail: &str) -> Option<i64> {
    since_of(detail.strip_prefix(STUCK_FULL)?)
}

/// Whether a member's words say its stream is at its rate limit
/// ([`RATE_LIMITED`]), first in them as the stuck-full words are.
pub fn says_rate_limited(detail: &str) -> bool {
    detail.starts_with(RATE_LIMITED)
}

/// The time a member's words say its stream came to its rate limit, ns
/// since the epoch ([`rate_limited_for`]): none where they do not say so,
/// or say no time that reads.
pub fn rate_limited_since(detail: &str) -> Option<i64> {
    since_of(detail.strip_prefix(RATE_LIMITED)?)
}

/// Whether a member's words say the broker refuses its write-ahead stream,
/// or does not answer it ([`REFUSED`]): first, or after the stuck-full
/// words. INTERIM(v32).
pub fn says_refused(detail: &str) -> bool {
    detail.contains(REFUSED)
}

/// The time a member's words say the broker's refusals began, ns since the
/// epoch ([`refused_for`]): none where they do not say it refused, or say
/// no time that reads. INTERIM(v32).
pub fn refused_since(detail: &str) -> Option<i64> {
    let at = detail.find(REFUSED)?;
    since_of(&detail[at + REFUSED.len()..])
}

/// The time said after `for <n>s` in `said`, the words after what was said.
fn since_of(said: &str) -> Option<i64> {
    let at = said.split_once(STUCK_SINCE)?.1.get(..20)?;
    let digits = |from: usize, to: usize| at.get(from..to)?.parse::<i64>().ok();
    let marks = at.as_bytes();
    if [
        marks[4], marks[7], marks[10], marks[13], marks[16], marks[19],
    ] != *b"--T::Z"
    {
        return None;
    }
    let (year, month, day) = (digits(0, 4)?, digits(5, 7)?, digits(8, 10)?);
    let (hour, minute, second) = (digits(11, 13)?, digits(14, 16)?, digits(17, 19)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    // A civil date to days since 1970 (Howard Hinnant's days-from-civil).
    let year = year - i64::from(month <= 2);
    let (era, year_of_era) = (year.div_euclid(400), year.rem_euclid(400));
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some((days * 86_400 + hour * 3_600 + minute * 60 + second) * 1_000_000_000)
}

/// Where a dataset's writes go: its instance and its key, one subject a
/// dataset; a plugin's credential publishes on its own instance's alone.
/// Spelled as the topic registry names it.
pub fn subject(instance: &str, key: &str) -> String {
    format!("platform.lake.internal.write-ahead.{instance}.{key}")
}

/// The dataset a write's subject names (`<instance>:<key>`, as
/// [`subject_of`] spelled it); none for a subject of no dataset's.
pub fn dataset_of_subject(subject: &str) -> Option<String> {
    // The prefix as [`subject`] spells it, so the topic is named once.
    let prefix = self::subject("", "");
    let rest = subject
        .strip_prefix(prefix.trim_end_matches('.'))?
        .strip_prefix('.')?;
    let (instance, key) = rest.split_once('.')?;
    (!instance.is_empty() && !key.is_empty() && !key.contains('.'))
        .then(|| format!("{instance}:{key}"))
}

/// Every subject of one instance's datasets: what its plugin's credential
/// publishes on, and reads its last writes back from.
pub fn instance_subjects(instance: &str) -> String {
    subject(instance, "*")
}

/// The stream's subject filter: every dataset's subject, NATS's `>` in
/// place of the instance and the key.
pub fn stream_subjects() -> String {
    subject("*", "*").replace(".*.*", ".>")
}

/// The lake topic a write was asked on, beside it in the stream.
pub const TOPIC_HEADER: &str = "Meridian-Topic";

/// Where the lake's writer answers it: the sidecar's inbox for it.
pub const REPLY_HEADER: &str = "Meridian-Reply-To";

/// The stream's sequence of the write this sidecar's stream held last on
/// the same subject, so the writer can tell a write of the dataset the
/// stream dropped before the lake recorded it (a gap, W10.14).
pub const AFTER_HEADER: &str = "Meridian-Write-Ahead-After";

/// Where the writer's refusal of a write travels, as the broker's request
/// path carries one.
pub const REFUSAL_HEADER: &str = crate::nats::REFUSAL_HEADER;

/// How long the write's caller waits for its answer, ms, as the broker's
/// request path carries it: what the lake's writer answers a held write
/// within (step 3d; spec 6.8, the hold).
pub const TIMEOUT_HEADER: &str = crate::nats::TIMEOUT_HEADER;

/// JetStream's own: a write sent again within the stream's duplicate window
/// is held once.
const MESSAGE_ID_HEADER: &str = "Nats-Msg-Id";

/// How long a plugin refused because the stream is full is told to wait:
/// the writer drains it a batch at a time, so a few seconds.
pub const FULL_RETRY_AFTER_SECONDS: u32 = 5;

/// The lake's topics whose rows a data plugin records (W10.4), each a
/// batch of rows the lake keeps: what goes by the stream. A liveness, an
/// assessment or a decline is no row, and goes by the broker as before.
pub const TOPICS: [&str; 17] = [
    "platform.lake.command.record-prices",
    "platform.lake.command.record-bars",
    "platform.lake.command.record-trades",
    "platform.lake.command.record-quotes",
    "platform.lake.command.record-outcomes",
    "platform.lake.command.record-rates",
    "platform.lake.command.record-statistics",
    "platform.lake.command.record-series",
    "platform.lake.command.record-derived-series",
    "platform.lake.command.record-scenarios",
    "platform.lake.command.record-corporate-actions",
    "platform.lake.command.record-filings",
    "platform.lake.command.record-reported-facts",
    "platform.lake.command.record-rating-actions",
    "platform.lake.command.record-holdings-reports",
    "platform.lake.command.record-holdings",
    "platform.lake.command.record-factors",
];

/// The name the bus knows the stream's backend by.
pub const BACKEND: &str = "write-ahead";

/// The routes that send [`TOPICS`] to the stream's backend.
pub fn routes() -> Vec<crate::RouteRule> {
    TOPICS
        .iter()
        .map(|topic| crate::RouteRule {
            pattern: topic.to_string(),
            backend: BACKEND.to_string(),
        })
        .collect()
}

/// The subject a dataset's writes go to: its instance and its key, each one
/// segment; none for an ID that is not `instance:key` or that a subject
/// cannot carry.
pub fn subject_of(dataset: &str) -> Option<String> {
    let (instance, key) = dataset.split_once(':')?;
    let segment = |said: &str| {
        !said.is_empty()
            && said
                .bytes()
                .all(|b| b.is_ascii_graphic() && !matches!(b, b'.' | b'*' | b'>' | b':'))
    };
    (segment(instance) && segment(key)).then(|| subject(instance, key))
}

/// Every record request holds its rows as its first field, and every row
/// its envelope (`ObservationMeta`) as its first: what a write's dataset is
/// read from without knowing which request it is.
#[derive(Clone, PartialEq, prost::Message)]
struct Rows {
    #[prost(message, repeated, tag = "1")]
    rows: Vec<Row>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct Row {
    #[prost(message, optional, tag = "1")]
    meta: Option<meridian_domain::v1::ObservationMeta>,
}

/// The dataset of a write's first row, where it names one.
pub fn dataset_of(envelope: &Envelope) -> Option<String> {
    let rows = Rows::decode(&envelope.payload[..]).ok()?;
    let dataset = rows
        .rows
        .first()?
        .meta
        .as_ref()?
        .source
        .as_ref()?
        .dataset
        .clone();
    (!dataset.is_empty()).then_some(dataset)
}

/// One dataset's rows of a write sent apart: its subject, the write with
/// those rows alone, and where each was among the rows the plugin sent.
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub subject: String,
    pub envelope: Envelope,
    pub places: Vec<usize>,
}

/// A varint read from `bytes` at `at`, and where the next byte is.
fn varint(bytes: &[u8], mut at: usize) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(at)?;
        at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some((value, at));
        }
    }
    None
}

/// A write's rows by their dataset, where it holds rows of more than one
/// and every one is a dataset of `instance`'s own that a subject can name:
/// each dataset's rows as a write of their own, in the plugin's order, with
/// everything else the request says (the want it answers). None for a
/// write of one dataset, which goes whole as it always has, and for one
/// that does not read so or names another instance's dataset, which the
/// lake refuses whole.
///
/// Read from the bytes, as [`dataset_of`] reads a first row: every record
/// request holds its rows as its field 1, whichever request it is.
pub fn apart(instance: &str, envelope: &Envelope) -> Option<Vec<Part>> {
    let payload = &envelope.payload[..];
    let mut rows: Vec<(&[u8], String)> = Vec::new();
    let mut rest: Vec<u8> = Vec::new();
    let mut at = 0;
    while at < payload.len() {
        let (key, after_key) = varint(payload, at)?;
        // Where the field's value begins and the field ends.
        let (value, end) = match key & 7 {
            0 => (after_key, varint(payload, after_key)?.1),
            1 => (after_key, after_key.checked_add(8)?),
            2 => {
                let (length, begins) = varint(payload, after_key)?;
                (begins, begins.checked_add(usize::try_from(length).ok()?)?)
            }
            5 => (after_key, after_key.checked_add(4)?),
            _ => return None,
        };
        let field = payload.get(at..end)?;
        if key >> 3 == 1 && key & 7 == 2 {
            let dataset = Row::decode(payload.get(value..end)?)
                .ok()?
                .meta?
                .source?
                .dataset;
            rows.push((field, dataset));
        } else {
            rest.extend_from_slice(field);
        }
        at = end;
    }
    let mut parts: Vec<(String, Vec<u8>, Vec<usize>)> = Vec::new();
    for (place, (field, dataset)) in rows.iter().enumerate() {
        match parts.iter_mut().find(|(held, _, _)| held == dataset) {
            Some((_, bytes, places)) => {
                bytes.extend_from_slice(field);
                places.push(place);
            }
            None => parts.push((dataset.clone(), field.to_vec(), vec![place])),
        }
    }
    if parts.len() < 2 {
        return None;
    }
    parts
        .into_iter()
        .map(|(dataset, mut bytes, places)| {
            let own = dataset
                .split_once(':')
                .is_some_and(|(owner, _)| !owner.is_empty() && owner == instance);
            let subject = own.then(|| subject_of(&dataset)).flatten()?;
            bytes.extend_from_slice(&rest);
            Some(Part {
                subject,
                envelope: Envelope {
                    meta: envelope.meta.clone(),
                    payload_type: envelope.payload_type.clone(),
                    payload: bytes,
                },
                places,
            })
        })
        .collect()
}

/// The answers of a write's parts as its one answer: every part's counts
/// added and its watermark's partitions joined; or the first part's
/// refusal, each row it names (`prices[1]`) named by its place among the
/// rows the plugin sent.
fn joined<'a>(
    answers: impl Iterator<Item = (&'a Part, Result<Envelope, BusError>)>,
) -> Result<Envelope, BusError> {
    use meridian_domain::v1::RecordObservationsReply;
    let mut all = RecordObservationsReply::default();
    let mut answer: Option<Envelope> = None;
    for (part, answered) in answers {
        let said = match answered {
            Ok(said) => said,
            Err(BusError::HandlerFailed { topic, detail }) => {
                return Err(BusError::HandlerFailed {
                    topic,
                    detail: placed(&detail, &part.places),
                })
            }
            Err(failed) => return Err(failed),
        };
        let reply = RecordObservationsReply::decode(&said.payload[..]).map_err(|failed| {
            BusError::HandlerFailed {
                topic: part.subject.clone(),
                detail: format!("a part's answer did not read: {failed}"),
            }
        })?;
        all.recorded += reply.recorded;
        all.restated += reply.restated;
        all.unchanged += reply.unchanged;
        if let Some(watermark) = reply.watermark {
            all.watermark
                .get_or_insert_with(Default::default)
                .partitions
                .extend(watermark.partitions);
        }
        answer.get_or_insert(said);
    }
    let mut answer = answer.unwrap_or_default();
    answer.payload = all.encode_to_vec();
    Ok(answer)
}

/// A part's refusal with each row it names by its place in the part
/// (`<field>[<n>]`) named by its place among the rows the plugin sent.
fn placed(detail: &str, places: &[usize]) -> String {
    let mut out = String::with_capacity(detail.len());
    let mut rest = detail;
    while let Some(open) = rest.find('[') {
        let (before, from) = rest.split_at(open);
        out.push_str(before);
        let digits = from[1..].bytes().take_while(u8::is_ascii_digit).count();
        let named = before
            .bytes()
            .last()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_');
        let closes = from[1 + digits..].starts_with(']');
        match from[1..1 + digits].parse::<usize>() {
            Ok(n) if named && closes && digits > 0 => {
                let place = places.get(n).copied().unwrap_or(n);
                out.push_str(&format!("[{place}]"));
                rest = &from[2 + digits..];
            }
            _ => {
                out.push('[');
                rest = &from[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Writes dropped here, never sent, because their caller had stopped
/// waiting or the broker could not take them now, since the process
/// started: counted apart from refusals.
static EXPIRED: AtomicU64 = AtomicU64::new(0);

/// [`EXPIRED`]'s count.
pub fn expired() -> u64 {
    EXPIRED.load(Ordering::Relaxed)
}

/// The writer's answers, each to the inbox its write named.
struct Replies {
    prefix: String,
    waiting: Mutex<HashMap<String, oneshot::Sender<async_nats::Message>>>,
}

impl Replies {
    fn wait(&self) -> (String, oneshot::Receiver<async_nats::Message>) {
        let token = uuid::Uuid::new_v4().simple().to_string();
        let (tell, heard) = oneshot::channel();
        self.waiting
            .lock()
            .expect("replies poisoned")
            .insert(token.clone(), tell);
        (format!("{}.{token}", self.prefix), heard)
    }

    fn forget(&self, reply_to: &str) {
        if let Some(token) = reply_to.rsplit('.').next() {
            self.waiting.lock().expect("replies poisoned").remove(token);
        }
    }

    fn heard(&self, message: async_nats::Message) {
        let token = message.subject.rsplit('.').next().unwrap_or_default();
        let waiting = self.waiting.lock().expect("replies poisoned").remove(token);
        if let Some(waiting) = waiting {
            let _ = waiting.send(message);
        }
    }
}

/// The sidecar's backend for the lake's record topics in `ha`: each write
/// held by the stream, then answered by the lake's writer. Everything else
/// goes to `fallback`, the broker's own backend.
pub struct WriteAhead {
    client: async_nats::Client,
    context: jetstream::Context,
    max_payload: Box<dyn Fn() -> usize + Send + Sync>,
    fallback: Arc<dyn Backend>,
    replies: Arc<Replies>,
    /// The stream's sequence of the write it held last, by subject; 0 where
    /// the stream holds none of the subject's.
    after: Mutex<HashMap<String, u64>>,
}

impl WriteAhead {
    /// Over its own client on the broker `url` names, with the sidecar's
    /// credential: the stream is asked to hold each write, and the writer's
    /// answers come back to one inbox of this client's.
    pub async fn connect(url: &str, fallback: Arc<dyn Backend>) -> Result<Self, BusError> {
        let client = crate::NatsBackend::client(url).await?;
        let prefix = client.new_inbox();
        let mut answers = client
            .subscribe(format!("{prefix}.*"))
            .await
            .map_err(|failed| BusError::HandlerFailed {
                topic: stream_subjects(),
                detail: format!("the write-ahead stream's answers could not be heard: {failed}"),
            })?;
        let replies = Arc::new(Replies {
            prefix,
            waiting: Mutex::default(),
        });
        let hearing = Arc::clone(&replies);
        tokio::spawn(async move {
            while let Some(message) = futures_util::StreamExt::next(&mut answers).await {
                hearing.heard(message);
            }
        });
        let sizing = client.clone();
        Ok(Self {
            client: client.clone(),
            context: jetstream::new(client),
            max_payload: Box::new(move || sizing.server_info().max_payload),
            fallback,
            replies,
            after: Mutex::default(),
        })
    }

    /// A write holding rows of several of its instance's datasets (step
    /// 3d; spec 6.1: one subject a dataset; ruled 2026-11-15): each
    /// dataset's rows held on its own subject, a write of their own, and
    /// the write answered once every part is -- each part's counts added
    /// and its watermark's partitions joined, or the first part refused,
    /// naming its row as the plugin sent it. A part answered commits
    /// whatever another part's answer: sent again, its rows are found
    /// unchanged.
    async fn written_apart(
        &self,
        topic: &str,
        parts: Vec<Part>,
        timeout: Duration,
    ) -> Result<Envelope, BusError> {
        let answers = futures_util::future::join_all(parts.iter().map(|part| {
            self.written(
                topic,
                part.subject.clone(),
                part.envelope.clone(),
                timeout,
                Some(&part.subject),
            )
        }))
        .await;
        joined(parts.iter().zip(answers))
    }

    async fn written(
        &self,
        topic: &str,
        subject: String,
        envelope: Envelope,
        timeout: Duration,
        // The part's own subject, where the write was sent apart: beside
        // its message's ID, so the stream holds each part once.
        apart: Option<&str>,
    ) -> Result<Envelope, BusError> {
        let deadline = Instant::now() + timeout;
        let timed_out = || BusError::Timeout {
            topic: topic.to_string(),
            timeout_ms: timeout.as_millis() as u64,
        };
        let bytes = envelope.encode_to_vec();
        let most = (self.max_payload)();
        if most > 0 && bytes.len() > most {
            return Err(BusError::TooLarge {
                topic: topic.to_string(),
                size: bytes.len(),
                most,
            });
        }
        let (reply_to, answer) = self.replies.wait();
        let mut headers = async_nats::HeaderMap::new();
        // Every header's words on one line (crate::nats::header_words).
        headers.insert(TOPIC_HEADER, crate::nats::header_words(topic).as_str());
        headers.insert(REPLY_HEADER, crate::nats::header_words(&reply_to).as_str());
        headers.insert(
            crate::nats::TIMEOUT_HEADER,
            timeout.as_millis().to_string().as_str(),
        );
        if let Some(meta) = envelope.meta.as_ref().filter(|m| !m.message_id.is_empty()) {
            let id = match apart {
                Some(part) => format!("{}/{part}", meta.message_id),
                None => meta.message_id.clone(),
            };
            headers.insert(MESSAGE_ID_HEADER, crate::nats::header_words(&id).as_str());
        }
        if let Some(after) = self.after_of(&subject, deadline).await {
            headers.insert(AFTER_HEADER, after.to_string().as_str());
        }
        // A write whose caller has stopped waiting -- it waited past its
        // deadline before it reached here -- is dropped, never sent: the
        // stream holds nothing nobody will be answered for. So is one the
        // broker cannot take now, which its client would queue and send
        // whenever it reconnects, waited for or not.
        if Instant::now() >= deadline {
            self.replies.forget(&reply_to);
            EXPIRED.fetch_add(1, Ordering::Relaxed);
            tracing::info!(
                topic,
                "a write dropped: its caller stopped waiting before it was sent"
            );
            return Err(timed_out());
        }
        if self.client.connection_state() != async_nats::connection::State::Connected {
            self.replies.forget(&reply_to);
            EXPIRED.fetch_add(1, Ordering::Relaxed);
            return Err(BusError::HandlerFailed {
                topic: topic.to_string(),
                detail: crate::backend::refusal_with_status(
                    crate::backend::RefusalStatus::Unavailable,
                    &[],
                    "the lake's write-ahead stream cannot be reached now; ask again",
                ),
            });
        }
        let held = tokio::time::timeout_at(deadline.into(), async {
            self.context
                .publish_with_headers(subject.clone(), headers, bytes.into())
                .await?
                .await
        })
        .await;
        let held = match held {
            Err(_) => {
                self.replies.forget(&reply_to);
                return Err(timed_out());
            }
            Ok(Err(failed)) => {
                self.replies.forget(&reply_to);
                return Err(refused(topic, failed.kind(), &failed, timed_out));
            }
            Ok(Ok(held)) => held,
        };
        self.after
            .lock()
            .expect("after poisoned")
            .insert(subject, held.sequence);
        // Held by the stream: now the writer's answer, once it committed.
        match tokio::time::timeout_at(deadline.into(), answer).await {
            Ok(Ok(message)) => answered(topic, message),
            _ => {
                self.replies.forget(&reply_to);
                Err(timed_out())
            }
        }
    }
}

impl WriteAhead {
    /// The stream's sequence of the write this sidecar's stream held last on
    /// `subject`, its dataset's chain. Kept as each write is held; and where
    /// this process has held none -- a sidecar restarted -- taken from the
    /// stream, the last message on the subject (contract v30, stage C,
    /// approved 2026-11-11), so a restart never hides a gap from the
    /// writer. None where the stream holds none of it, or could not say in
    /// time (at most a second, within the write's deadline).
    async fn after_of(&self, subject: &str, deadline: Instant) -> Option<u64> {
        if let Some(held) = self.after.lock().expect("after poisoned").get(subject) {
            return (*held > 0).then_some(*held);
        }
        let within = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_secs(1));
        let stream = self.context.get_stream_no_info(STREAM).await.ok()?;
        let last = tokio::time::timeout(within, stream.direct_get_last_for_subject(subject)).await;
        let held = match last {
            Ok(Ok(message)) => message.sequence,
            Ok(Err(failed)) if failed.kind() == jetstream::stream::DirectGetErrorKind::NotFound => {
                0
            }
            Ok(Err(failed)) => {
                tracing::warn!(subject, %failed, "the stream's last write of a dataset could not be read; its chain begins here");
                return None;
            }
            Err(_) => return None,
        };
        let mut after = self.after.lock().expect("after poisoned");
        let held = *after.entry(subject.to_string()).or_insert(held);
        (held > 0).then_some(held)
    }
}

/// The stream's refusal of a write, as the bus says it: full, refused for
/// load with a retry-after; no stream yet, nothing serving; out of time, a
/// timeout.
fn refused(
    topic: &str,
    kind: jetstream::context::PublishErrorKind,
    failed: &(dyn std::error::Error + 'static),
    timed_out: impl Fn() -> BusError,
) -> BusError {
    use jetstream::context::PublishErrorKind;
    let words = full_words(failed);
    match kind {
        PublishErrorKind::StreamNotFound => BusError::NoHandler(topic.to_string()),
        PublishErrorKind::TimedOut => timed_out(),
        _ if words.is_some() => BusError::HandlerFailed {
            topic: topic.to_string(),
            detail: crate::refusal_retrying(
                meridian_pb::v1::RefusalReason::Overloaded as i32,
                FULL_RETRY_AFTER_SECONDS,
                words.unwrap_or_default(),
            ),
        },
        _ => BusError::HandlerFailed {
            topic: topic.to_string(),
            detail: format!("the lake's write-ahead stream did not hold the write: {failed}"),
        },
    }
}

/// Where the stream refused a write because it is full -- JetStream's store
/// refusing it at a bound, `discard: new` -- the plugin's words. True
/// whatever the lake has recorded: a recorded write leaves the stream only
/// at its age bound, so recording frees no room.
fn full_words(failed: &(dyn std::error::Error + 'static)) -> Option<String> {
    let mut said = failed.to_string();
    let mut cause = failed.source();
    while let Some(next) = cause {
        said.push_str(": ");
        said.push_str(&next.to_string());
        cause = next.source();
    }
    let full = said.contains("maximum bytes exceeded")
        || said.contains("maximum messages exceeded")
        || said.contains("insufficient resources")
        || said.contains("resource limits exceeded");
    full.then(|| FULL.to_string())
}

/// The writer's answer, as the broker's request path reads one: a refusal
/// in its header, or the reply.
fn answered(topic: &str, message: async_nats::Message) -> Result<Envelope, BusError> {
    if let Some(detail) = message
        .headers
        .as_ref()
        .and_then(|headers| headers.get(crate::nats::REFUSAL_HEADER))
    {
        return Err(BusError::HandlerFailed {
            topic: topic.to_string(),
            detail: detail.to_string(),
        });
    }
    Envelope::decode(message.payload).map_err(|failed| BusError::HandlerFailed {
        topic: topic.to_string(),
        detail: format!("the answer was not an envelope: {failed}"),
    })
}

impl Backend for WriteAhead {
    fn publish(&self, topic: &str, envelope: Envelope) -> Result<u64, BusError> {
        self.fallback.publish(topic, envelope)
    }

    fn subscribe(&self, pattern: &str) -> Subscription {
        self.fallback.subscribe(pattern)
    }

    fn subscribe_shared(&self, pattern: &str, group: &str) -> Subscription {
        self.fallback.subscribe_shared(pattern, group)
    }

    /// A write of rows of one of its own instance's datasets that a subject
    /// can name goes by the stream; anything else as the broker asks it,
    /// where the lake answers a row naming another instance's dataset with
    /// its own refusal, as it always has.
    fn request<'a>(&'a self, topic: &'a str, envelope: Envelope, timeout: Duration) -> Answer<'a> {
        let instance = envelope
            .meta
            .as_ref()
            .map(|meta| meta.publisher_instance_id.as_str())
            .unwrap_or_default();
        // Rows of several of its datasets: each dataset's on its own
        // subject (spec 6.1). Sent on the first row's alone, the rows of a
        // dataset whose writer group holds no range of the first were
        // committed by nobody.
        if TOPICS.contains(&topic) {
            if let Some(parts) = apart(instance, &envelope) {
                return Box::pin(self.written_apart(topic, parts, timeout));
            }
        }
        let subject = TOPICS
            .contains(&topic)
            .then(|| dataset_of(&envelope))
            .flatten()
            .filter(|dataset| {
                dataset
                    .split_once(':')
                    .is_some_and(|(owner, _)| !owner.is_empty() && owner == instance)
            })
            .and_then(|dataset| subject_of(&dataset));
        match subject {
            Some(subject) => Box::pin(self.written(topic, subject, envelope, timeout, None)),
            None => self.fallback.request(topic, envelope, timeout),
        }
    }

    fn serve(&self, topic: &str, group: &str, handler: Handler) {
        self.fallback.serve(topic, group, handler);
    }

    fn serve_delivered(&self, topic: &str, group: &str, handler: Handler, delivered: Arc<Notify>) {
        self.fallback
            .serve_delivered(topic, group, handler, delivered);
    }

    fn dropped(&self) -> u64 {
        self.fallback.dropped()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn priced(dataset: &str) -> Envelope {
        let request = meridian_domain::v1::RecordPricesRequest {
            prices: vec![meridian_domain::v1::Price {
                meta: Some(meridian_domain::v1::ObservationMeta {
                    row_key: "r-1".into(),
                    source: Some(meridian_domain::v1::Source {
                        dataset: dataset.into(),
                        instance: "truefx".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            want_id: String::new(),
        };
        Envelope {
            meta: None,
            payload_type: "meridian.v1.RecordPricesRequest".into(),
            payload: request.encode_to_vec(),
        }
    }

    #[test]
    fn a_write_s_dataset_is_read_from_its_first_row_whatever_the_request() {
        assert_eq!(
            dataset_of(&priced("truefx:live")).as_deref(),
            Some("truefx:live")
        );
        let empty = Envelope {
            meta: None,
            payload_type: String::new(),
            payload: Vec::new(),
        };
        assert_eq!(dataset_of(&empty), None);
    }

    /// A write of rows of `datasets`, in that order, answering `want_id`.
    fn priced_each(datasets: &[&str], want_id: &str) -> Envelope {
        let rows = datasets
            .iter()
            .enumerate()
            .map(|(n, dataset)| meridian_domain::v1::Price {
                meta: Some(meridian_domain::v1::ObservationMeta {
                    row_key: format!("k{n}"),
                    source: Some(meridian_domain::v1::Source {
                        dataset: dataset.to_string(),
                        instance: "truefx".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .collect();
        Envelope {
            meta: None,
            payload_type: "meridian.v1.RecordPricesRequest".into(),
            payload: meridian_domain::v1::RecordPricesRequest {
                prices: rows,
                want_id: want_id.into(),
            }
            .encode_to_vec(),
        }
    }

    /// A write of rows of several datasets goes to the stream as one write
    /// a dataset, each on its own subject (step 3d; spec 6.1; ruled
    /// 2026-11-15). On its first row's subject alone, the rows of a dataset
    /// whose writer group held no range of the first were committed by
    /// nobody (the lake's a_write_is_a_success_only_when_its_answer_
    /// accounts_for_every_row_it_sent).
    #[test]
    fn a_write_of_several_datasets_is_sent_apart_each_dataset_on_its_own_subject() {
        let write = priced_each(&["truefx:live", "truefx:daily", "truefx:live"], "want-1");
        let parts = apart("truefx", &write).expect("sent apart");
        let said: Vec<(String, Vec<String>, String, Vec<usize>)> = parts
            .iter()
            .map(|part| {
                let request =
                    meridian_domain::v1::RecordPricesRequest::decode(&part.envelope.payload[..])
                        .unwrap();
                (
                    part.subject.clone(),
                    request
                        .prices
                        .iter()
                        .map(|row| row.meta.as_ref().unwrap().row_key.clone())
                        .collect(),
                    request.want_id,
                    part.places.clone(),
                )
            })
            .collect();
        assert_eq!(
            said,
            vec![
                (
                    subject("truefx", "live"),
                    vec!["k0".to_string(), "k2".to_string()],
                    "want-1".to_string(),
                    vec![0, 2]
                ),
                (
                    subject("truefx", "daily"),
                    vec!["k1".to_string()],
                    "want-1".to_string(),
                    vec![1]
                ),
            ]
        );
        // Every part's rows are its subject's dataset's.
        for part in &parts {
            assert_eq!(
                dataset_of(&part.envelope).and_then(|dataset| subject_of(&dataset)),
                Some(part.subject.clone())
            );
        }
        // One dataset: whole, as ever. Another instance's dataset among
        // them: whole, for the lake to refuse.
        assert_eq!(
            apart("truefx", &priced_each(&["truefx:live", "truefx:live"], "")),
            None
        );
        assert_eq!(
            apart("truefx", &priced_each(&["truefx:live", "oanda:live"], "")),
            None
        );
        assert_eq!(apart("truefx", &priced("truefx:live")), None);
    }

    /// The parts' answers are the write's one answer: counts added and
    /// partitions joined; a part refused is the write's refusal, its row
    /// named by its place among the rows the plugin sent.
    #[test]
    fn a_write_sent_apart_is_answered_once_counting_every_part_or_refused_by_its_own_row() {
        use meridian_domain::v1::{PartitionSequence, RecordObservationsReply, Watermark};
        let parts = apart(
            "truefx",
            &priced_each(
                &["truefx:live", "truefx:daily", "truefx:live", "truefx:daily"],
                "",
            ),
        )
        .unwrap();
        let answer = |recorded: u32, partition: &str| {
            Ok(Envelope {
                meta: None,
                payload_type: "meridian.v1.RecordObservationsReply".into(),
                payload: RecordObservationsReply {
                    recorded,
                    watermark: Some(Watermark {
                        partitions: vec![PartitionSequence {
                            partition: partition.into(),
                            sequence: 7,
                        }],
                    }),
                    ..Default::default()
                }
                .encode_to_vec(),
            })
        };
        let whole = joined(
            parts
                .iter()
                .zip([answer(2, "truefx:live"), answer(2, "truefx:daily")]),
        )
        .unwrap();
        let reply = RecordObservationsReply::decode(&whole.payload[..]).unwrap();
        assert_eq!(reply.recorded, 4);
        let named: Vec<String> = reply
            .watermark
            .unwrap()
            .partitions
            .into_iter()
            .map(|at| at.partition)
            .collect();
        assert_eq!(named, ["truefx:live", "truefx:daily"]);
        // The second part's second row is the fourth the plugin sent.
        let refused = joined(parts.iter().zip([
            answer(2, "truefx:live"),
            Err(BusError::HandlerFailed {
                topic: "t".into(),
                detail: "[refusal-status invalid_argument] [refusal-fields prices[1].meta.valid_from_ns] row key \"k\" [sic] fixes its valid time".into(),
            }),
        ]))
        .unwrap_err();
        match refused {
            BusError::HandlerFailed { detail, .. } => assert_eq!(
                detail,
                "[refusal-status invalid_argument] [refusal-fields prices[3].meta.valid_from_ns] row key \"k\" [sic] fixes its valid time"
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_stream_s_filter_and_a_dataset_s_subjects_are_the_bytes_on_the_wire() {
        // Spelled as the topic registry names them, the subjects on the wire,
        // the stream's filter and a credential's own are unchanged.
        assert_eq!(stream_subjects(), "platform.lake.internal.write-ahead.>");
        assert_eq!(
            subject("truefx", "live"),
            "platform.lake.internal.write-ahead.truefx.live"
        );
        assert_eq!(
            instance_subjects("truefx"),
            "platform.lake.internal.write-ahead.truefx.*"
        );
    }

    #[test]
    fn a_dataset_s_subject_names_its_instance_and_its_key_each_one_segment() {
        assert_eq!(
            subject_of("truefx:live").as_deref(),
            Some("platform.lake.internal.write-ahead.truefx.live")
        );
        for bad in [
            "truefx", "a.b:c", "a:b.c", "a:*", ":key", "inst:", "a:b:c", "a b:c",
        ] {
            assert_eq!(subject_of(bad), None, "{bad}");
        }
    }

    /// The time a stream went stuck is said to the second and read back
    /// as it was said; words saying none, or not stuck full, give none.
    #[test]
    fn the_time_a_stream_went_stuck_is_read_back_as_it_was_said() {
        for (seconds, said) in [
            (1_793_000_000, "2026-10-26T07:33:20Z"),
            (1_709_251_199, "2024-02-29T23:59:59Z"),
            (951_782_400, "2000-02-29T00:00:00Z"),
            (0, "1970-01-01T00:00:00Z"),
        ] {
            let words = stuck_full_since(30, seconds * 1_000_000_000 + 999);
            assert_eq!(words, format!("{STUCK_FULL} for 30s, since {said}"));
            let detail = format!("{words}: holds 256.0 MiB of its 256.0 MiB bound");
            assert_eq!(stuck_since(&detail), Some(seconds * 1_000_000_000));
        }
        assert_eq!(stuck_since(&format!("{STUCK_FULL} for 30s: holds")), None);
        assert_eq!(
            stuck_since(&format!("{STUCK_FULL} for 30s, since soon")),
            None
        );
        assert_eq!(stuck_since("ready, since 2026-10-26T07:33:20Z"), None);
    }

    /// The time the broker's refusals began is said as the stuck time is,
    /// and read back first in a member's words or after its stuck-full
    /// words; words not saying it refused give none (INTERIM(v32), A1).
    #[test]
    fn the_time_the_brokers_refusals_began_is_read_back_first_or_after_the_stuck_words() {
        let words = refused_for(35, 1_793_000_000 * 1_000_000_000 + 7);
        assert_eq!(
            words,
            format!("{REFUSED} for 35s, since 2026-10-26T07:33:20Z")
        );
        let alone = format!("{words}: every look at the stream refused or not answered");
        let after = format!("{}: holds 1.0 MiB; {alone}", stuck_full_since(30, 0));
        for detail in [&alone, &after] {
            assert!(says_refused(detail), "{detail}");
            assert_eq!(refused_since(detail), Some(1_793_000_000 * 1_000_000_000));
        }
        assert_eq!(
            stuck_since(&after),
            Some(0),
            "the stuck time first, as ever"
        );
        assert_eq!(stuck_since(&alone), None);
        let stuck = stuck_full_since(30, 0);
        assert!(!says_refused(&stuck));
        assert_eq!(refused_since(&stuck), None);
        assert_eq!(refused_since(&format!("{REFUSED} for 35s: no time")), None);
    }

    /// The time a stream came to its rate limit is said as the stuck time
    /// is and read back; its words are never the stuck-full ones, nor the
    /// stuck-full words its own.
    #[test]
    fn the_time_a_stream_came_to_its_rate_limit_is_read_back_and_is_not_the_stuck_time() {
        let words = rate_limited_for(30, 1_793_000_000 * 1_000_000_000 + 7);
        assert_eq!(
            words,
            format!("{RATE_LIMITED} for 30s, since 2026-10-26T07:33:20Z")
        );
        let detail = format!("{words}: holds 256.0 MiB of its 256.0 MiB bound");
        assert!(says_rate_limited(&detail));
        assert_eq!(
            rate_limited_since(&detail),
            Some(1_793_000_000 * 1_000_000_000)
        );
        assert_eq!(stuck_since(&detail), None);
        let stuck = stuck_full_since(30, 0);
        assert!(!says_rate_limited(&stuck));
        assert_eq!(rate_limited_since(&stuck), None);
    }

    /// The refusal a plugin reads says what frees room in the stream: the
    /// age of the writes it holds, never the lake recording them (a
    /// recorded write stays until its age bound; seen on a real broker,
    /// 2026-11-28).
    #[test]
    fn a_full_streams_refusal_does_not_say_the_lake_recording_frees_room() {
        #[derive(Debug)]
        struct Said;
        impl std::fmt::Display for Said {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("maximum bytes exceeded")
            }
        }
        impl std::error::Error for Said {}
        let said = full_words(&Said).expect("full");
        assert!(!said.contains("until the lake records"), "{said}");
        assert!(said.contains("reach its maximum age"), "{said}");
    }

    #[test]
    fn a_full_stream_is_a_refusal_for_load_with_a_retry_after() {
        use jetstream::context::PublishErrorKind;
        #[derive(Debug)]
        struct Said(&'static str);
        impl std::fmt::Display for Said {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.0)
            }
        }
        impl std::error::Error for Said {}
        let topic = "platform.lake.command.record-prices";
        // JetStream's own words for a stream at its bound with `discard: new`.
        for full in ["maximum bytes exceeded", "maximum messages exceeded"] {
            let BusError::HandlerFailed { detail, .. } =
                refused(topic, PublishErrorKind::Other, &Said(full), || {
                    BusError::ShuttingDown
                })
            else {
                panic!("refused for load")
            };
            assert_eq!(
                crate::read_refusal(&detail).map(|(reason, _)| reason),
                Some(meridian_pb::v1::RefusalReason::Overloaded as i32)
            );
            assert_eq!(
                crate::refusal_retry_after(&detail),
                Some(FULL_RETRY_AFTER_SECONDS)
            );
        }
        let other = refused(
            topic,
            PublishErrorKind::Other,
            &Said("something else"),
            || BusError::ShuttingDown,
        );
        assert!(
            matches!(&other, BusError::HandlerFailed { detail, .. } if crate::refusal_retry_after(detail).is_none())
        );
        assert!(matches!(
            refused(topic, PublishErrorKind::StreamNotFound, &Said(""), || {
                BusError::ShuttingDown
            }),
            BusError::NoHandler(_)
        ));
    }

    #[test]
    fn the_routes_send_every_record_topic_and_nothing_else_to_the_stream() {
        let routes = routes();
        assert_eq!(routes.len(), TOPICS.len());
        assert!(routes.iter().all(|r| r.backend == BACKEND));
        assert!(!TOPICS.contains(&"platform.lake.command.record-liveness"));
        assert!(!TOPICS.contains(&"platform.lake.command.record-quality-assessments"));
    }
}
