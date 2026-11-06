//! The units' work queue on JetStream (step 3b; spec/a-deployment-scales-
//! out-and-fails-over-by-configuration section 4, question 6; ruling 1056:
//! JetStream at three only for the analytics units).
//!
//! On a broker of three with `broker.jetstream.replicas: 3`, the coordinator
//! puts each unit on a work queue kept in memory on the three servers,
//! rather than asking the pool's queue group: a worker takes a unit and
//! acknowledges it only once its part is sent, so a worker that dies
//! mid-unit leaves it for another (the queue-group task's "a queue member
//! that dies mid-work loses that message"), and a broker server lost keeps
//! the queue on the other two. Without it, as by default, a unit asked of the
//! queue group is asked again on a timeout, as ever. Never a record and
//! never the order path (decisions/024): a unit is named by its inputs'
//! hash, so one computed twice is the same part.
//!
//! The part goes back by core NATS to the inbox the unit names in its
//! `Meridian-Part-To` header. The stream's subject is the component's own
//! topic, granted to its credential alone, beside JetStream's API.

use std::sync::Arc;
use std::time::Duration;

use async_nats::jetstream::{self, consumer, stream};
use futures_util::StreamExt;
use meridian_analytics_engine::graph::Cache;
use meridian_analytics_engine::measures::Part;
use serde_json::Value;

use crate::topics;

/// The stream the units wait in.
pub const STREAM: &str = "ANALYTICS_UNITS";
/// The workers' one consumer: each unit is taken by one of them.
pub const CONSUMER: &str = "workers";
/// Where a unit's part is sent back.
pub const PART_TO: &str = "Meridian-Part-To";

/// How long a worker has a unit before it goes to another.
const HELD: Duration = Duration::from_secs(30);
/// The most a unit is taken before it is left to the coordinator.
const TAKEN: i64 = 4;
/// What the queue may hold at once, under the servers' 64 MB of memory: a
/// unit past it is refused, and the coordinator computes it itself.
const MOST_BYTES: i64 = 48 * 1024 * 1024;

/// Whether this deployment's units go by JetStream: the chart says so where
/// the broker runs it.
pub fn on() -> bool {
    std::env::var("MERIDIAN_ANALYTICS_UNITS").as_deref() == Ok("jetstream")
}

/// The queue: its client, and JetStream over it.
pub struct Units {
    client: async_nats::Client,
    context: jetstream::Context,
}

impl Units {
    /// Connect, and make the stream if no member has yet (each does the
    /// same, so whichever starts first makes it).
    pub async fn connect(url: &str) -> Result<Arc<Units>, String> {
        let client = meridian_bus::NatsBackend::client(url)
            .await
            .map_err(|failed| failed.to_string())?;
        let context = jetstream::new(client.clone());
        context
            .get_or_create_stream(stream::Config {
                name: STREAM.to_string(),
                subjects: vec![topics::COMPUTE_UNIT.to_string()],
                retention: stream::RetentionPolicy::WorkQueue,
                storage: stream::StorageType::Memory,
                num_replicas: 3,
                max_bytes: MOST_BYTES,
                discard: stream::DiscardPolicy::New,
                max_age: Duration::from_secs(10 * 60),
                ..Default::default()
            })
            .await
            .map_err(|failed| format!("the units' stream could not be made: {failed}"))?;
        Ok(Arc::new(Units { client, context }))
    }

    /// Put a unit on the queue and wait for its part, at most `within`.
    pub async fn ask(&self, payload: Vec<u8>, within: Duration) -> Result<Value, String> {
        let inbox = self.client.new_inbox();
        let mut parts = self
            .client
            .subscribe(inbox.clone())
            .await
            .map_err(|failed| failed.to_string())?;
        let mut headers = async_nats::HeaderMap::new();
        headers.insert(PART_TO, inbox.as_str());
        let queued = self
            .context
            .publish_with_headers(topics::COMPUTE_UNIT, headers, payload.into())
            .await
            .map_err(|failed| format!("the queue did not take a unit: {failed}"))?;
        queued
            .await
            .map_err(|failed| format!("the queue did not keep a unit: {failed}"))?;
        let part = tokio::time::timeout(within, parts.next())
            .await
            .map_err(|_| format!("no worker answered a unit within {} ms", within.as_millis()))?
            .ok_or("the part's inbox closed")?;
        serde_json::from_slice(&part.payload)
            .map_err(|failed| format!("a part did not read: {failed}"))
    }

    /// Take units, one at a time per worker, answering each and
    /// acknowledging it once its part is sent; forever.
    pub async fn work(self: Arc<Self>, cache: Arc<Cache>) -> Result<(), String> {
        let stream = self
            .context
            .get_stream(STREAM)
            .await
            .map_err(|failed| failed.to_string())?;
        let taking = stream
            .get_or_create_consumer(
                CONSUMER,
                consumer::pull::Config {
                    durable_name: Some(CONSUMER.to_string()),
                    ack_policy: consumer::AckPolicy::Explicit,
                    ack_wait: HELD,
                    max_deliver: TAKEN,
                    ..Default::default()
                },
            )
            .await
            .map_err(|failed| failed.to_string())?;
        let mut units = taking
            .messages()
            .await
            .map_err(|failed| failed.to_string())?;
        while let Some(unit) = units.next().await {
            let unit = match unit {
                Ok(unit) => unit,
                Err(failed) => {
                    tracing::warn!(%failed, "the units' queue did not deliver");
                    continue;
                }
            };
            let to = unit
                .headers
                .as_ref()
                .and_then(|headers| headers.get(PART_TO))
                .map(|to| to.to_string());
            let payload = unit.payload.clone();
            let answering = Arc::clone(&cache);
            let answered =
                tokio::task::spawn_blocking(move || crate::worker::answer(&answering, &payload))
                    .await
                    .map_err(|failed| failed.to_string())?;
            match (answered, to) {
                (Ok(part), Some(to)) => {
                    let body = serde_json::to_vec(&Part::to_json(&part)).expect("a part is JSON");
                    if let Err(failed) = self.client.publish(to, body.into()).await {
                        tracing::warn!(%failed, "a unit's part was not sent; it goes to another");
                        continue;
                    }
                }
                (Err(failed), _) => tracing::warn!(%failed, "a unit could not be answered"),
                (Ok(_), None) => tracing::warn!("a unit named nowhere to send its part"),
            }
            if let Err(failed) = unit.ack().await {
                tracing::warn!(%failed, "a unit was not acknowledged; it may be answered again");
            }
        }
        Err("the units' queue ended".into())
    }
}
