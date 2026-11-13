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

/// Where a dataset's writes go: its instance and its key, one subject a
/// dataset; a plugin's credential publishes on its own instance's alone.
/// Spelled as the topic registry names it.
pub fn subject(instance: &str, key: &str) -> String {
    format!("platform.lake.internal.write-ahead.{instance}.{key}")
}

/// The dataset a write's subject names (`<instance>:<key>`, as
/// [`subject_of`] spelled it); none for a subject of no dataset's.
pub fn dataset_of_subject(subject: &str) -> Option<String> {
    let rest = subject.strip_prefix("platform.lake.internal.write-ahead.")?;
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

    async fn written(
        &self,
        topic: &str,
        subject: String,
        envelope: Envelope,
        timeout: Duration,
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
        headers.insert(TOPIC_HEADER, topic);
        headers.insert(REPLY_HEADER, reply_to.as_str());
        headers.insert(
            crate::nats::TIMEOUT_HEADER,
            timeout.as_millis().to_string().as_str(),
        );
        if let Some(meta) = envelope.meta.as_ref().filter(|m| !m.message_id.is_empty()) {
            headers.insert(MESSAGE_ID_HEADER, meta.message_id.as_str());
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
/// refusing it at a bound, `discard: new` -- the plugin's words.
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
    full.then(|| {
        "overloaded: the lake's write-ahead stream is full and holds no more writes until the \
         lake records what it holds"
            .to_string()
    })
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
            Some(subject) => Box::pin(self.written(topic, subject, envelope, timeout)),
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
