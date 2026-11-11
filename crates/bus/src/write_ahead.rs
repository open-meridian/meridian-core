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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_nats::jetstream;
use meridian_domain::v1::Envelope;
use prost::Message as _;
use tokio::sync::{oneshot, Notify};

use crate::backend::{Answer, Backend, BusError, Handler, Subscription};

/// The stream's name.
pub const STREAM: &str = "LAKE_WRITE_AHEAD";

/// Where a dataset's writes go: this, its instance and its key,
/// `platform.lake.internal.write-ahead.{instance}.{key}`, one subject a
/// dataset; a plugin's credential publishes on its own instance's alone.
pub const SUBJECTS: &str = "platform.lake.internal.write-ahead";

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
    (segment(instance) && segment(key)).then(|| format!("{SUBJECTS}.{instance}.{key}"))
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
    context: jetstream::Context,
    max_payload: Box<dyn Fn() -> usize + Send + Sync>,
    fallback: Arc<dyn Backend>,
    replies: Arc<Replies>,
    /// The stream's sequence of the write it held last, by subject.
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
                topic: SUBJECTS.to_string(),
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
        if let Some(after) = self
            .after
            .lock()
            .expect("after poisoned")
            .get(&subject)
            .copied()
        {
            headers.insert(AFTER_HEADER, after.to_string().as_str());
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
                return Err(refused(topic, failed, timed_out));
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

/// The stream's refusal of a write, as the bus says it: no stream yet,
/// nothing serving; out of time, a timeout.
fn refused(
    topic: &str,
    failed: jetstream::context::PublishError,
    timed_out: impl Fn() -> BusError,
) -> BusError {
    use jetstream::context::PublishErrorKind;
    match failed.kind() {
        PublishErrorKind::StreamNotFound => BusError::NoHandler(topic.to_string()),
        PublishErrorKind::TimedOut => timed_out(),
        _ => BusError::HandlerFailed {
            topic: topic.to_string(),
            detail: format!("the lake's write-ahead stream did not hold the write: {failed}"),
        },
    }
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
    fn the_routes_send_every_record_topic_and_nothing_else_to_the_stream() {
        let routes = routes();
        assert_eq!(routes.len(), TOPICS.len());
        assert!(routes.iter().all(|r| r.backend == BACKEND));
        assert!(!TOPICS.contains(&"platform.lake.command.record-liveness"));
        assert!(!TOPICS.contains(&"platform.lake.command.record-quality-assessments"));
    }
}
