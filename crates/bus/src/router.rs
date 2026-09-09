//! The bus as consumers see it: routing, stamping, and request-reply.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use meridian_pb::v1::{Envelope, MessageMeta};

use crate::backend::{Backend, BusError, Subscription};
use crate::topic;

/// Send a topic pattern to a named backend. First match wins, so order is
/// meaningful and a later rule cannot shadow an earlier one by accident.
#[derive(Debug, Clone)]
pub struct RouteRule {
    pub pattern: String,
    pub backend: String,
}

/// What a request handler returns: the reply's type name and its bytes.
pub type HandlerReply = std::result::Result<(String, Vec<u8>), String>;

type Handler = Arc<dyn Fn(Envelope) -> HandlerReply + Send + Sync>;

/// The bus.
///
/// Owns routing, identity stamping and request-reply. Backends own transport
/// and nothing else, so a second backend inherits all of this.
pub struct Bus {
    backends: HashMap<String, Arc<dyn Backend>>,
    rules: Vec<RouteRule>,
    default_backend: String,
    handlers: RwLock<HashMap<String, Handler>>,
    instance_id: String,
    default_timeout: Duration,
}

impl Bus {
    /// A bus with one backend and no routing rules, which is the shape of a
    /// single-host deployment.
    pub fn single(instance_id: impl Into<String>, backend: Arc<dyn Backend>) -> Self {
        let mut backends = HashMap::new();
        backends.insert("default".to_string(), backend);
        Self {
            backends,
            rules: Vec::new(),
            default_backend: "default".to_string(),
            handlers: RwLock::new(HashMap::new()),
            instance_id: instance_id.into(),
            default_timeout: Duration::from_secs(5),
        }
    }

    pub fn with_rules(mut self, rules: Vec<RouteRule>) -> Self {
        self.rules = rules;
        self
    }

    fn backend_for(&self, topic: &str) -> &Arc<dyn Backend> {
        for rule in &self.rules {
            if topic::matches(&rule.pattern, topic) {
                if let Some(backend) = self.backends.get(&rule.backend) {
                    return backend;
                }
                // A rule naming a backend that was never registered is a
                // configuration error. Falling through to the default keeps
                // the deployment running, and the warning is what gets it
                // fixed; refusing to publish would take the deployment down
                // for a mistake in a routing table.
                tracing::warn!(
                    backend = rule.backend,
                    pattern = rule.pattern,
                    "route names an unregistered backend; using the default"
                );
            }
        }
        self.backends
            .get(&self.default_backend)
            .expect("default backend is always registered")
    }

    /// Publish one message.
    ///
    /// The caller supplies the topic, the payload, and the causal links it
    /// knows about. Identity, time and the message id are stamped here: a
    /// publisher that could assert its own provenance would make the audit
    /// trail decorative.
    pub fn publish(
        &self,
        topic: &str,
        payload_type: &str,
        payload: Vec<u8>,
        correlation_id: Option<&str>,
        causation_id: Option<&str>,
    ) -> crate::Result<String> {
        let message_id = uuid::Uuid::new_v4().to_string();
        let envelope = Envelope {
            meta: Some(MessageMeta {
                message_id: message_id.clone(),
                // An empty correlation starts a new causal chain, and this
                // message is its root.
                correlation_id: correlation_id.unwrap_or(&message_id).to_string(),
                causation_id: causation_id.unwrap_or_default().to_string(),
                publisher_instance_id: self.instance_id.clone(),
                topic: topic.to_string(),
                schema_version: "v1".to_string(),
                published_at_ns: now_ns(),
            }),
            payload_type: payload_type.to_string(),
            payload,
        };

        self.backend_for(topic).publish(topic, envelope)?;
        Ok(message_id)
    }

    pub fn subscribe(&self, pattern: &str) -> Subscription {
        self.backend_for(pattern).subscribe(pattern)
    }

    /// Register a handler for request-reply on an exact topic.
    ///
    /// Exact rather than a pattern: two handlers matching one topic would make
    /// the answer depend on registration order, and a question with two
    /// answers is a question with none.
    pub fn serve<F>(&self, topic: &str, handler: F)
    where
        F: Fn(Envelope) -> HandlerReply + Send + Sync + 'static,
    {
        self.handlers
            .write()
            .expect("handler lock poisoned")
            .insert(topic.to_string(), Arc::new(handler));
    }

    /// Ask a question and wait for the answer, bounded in time.
    ///
    /// Answered in-process. There is no reply topic and no reply address on the
    /// wire, which is why neither appears in the envelope. Plugins reach this
    /// through their sidecar and never serve calls themselves.
    pub async fn call(
        &self,
        topic: &str,
        payload_type: &str,
        payload: Vec<u8>,
        correlation_id: Option<&str>,
        timeout: Option<Duration>,
    ) -> crate::Result<(String, Vec<u8>)> {
        let handler = self
            .handlers
            .read()
            .expect("handler lock poisoned")
            .get(topic)
            .cloned()
            .ok_or_else(|| BusError::NoHandler(topic.to_string()))?;

        let message_id = uuid::Uuid::new_v4().to_string();
        let envelope = Envelope {
            meta: Some(MessageMeta {
                message_id: message_id.clone(),
                correlation_id: correlation_id.unwrap_or(&message_id).to_string(),
                causation_id: String::new(),
                publisher_instance_id: self.instance_id.clone(),
                topic: topic.to_string(),
                schema_version: "v1".to_string(),
                published_at_ns: now_ns(),
            }),
            payload_type: payload_type.to_string(),
            payload,
        };

        let timeout = timeout.unwrap_or(self.default_timeout);
        let topic_owned = topic.to_string();

        // The handler runs off the async runtime. A handler that blocks is a
        // handler that would otherwise stall unrelated tasks on the same
        // worker, and the timeout below would not fire because the timer could
        // not be polled.
        let joined = tokio::time::timeout(
            timeout,
            tokio::task::spawn_blocking(move || handler(envelope)),
        )
        .await;

        match joined {
            Err(_) => Err(BusError::Timeout {
                topic: topic_owned,
                timeout_ms: timeout.as_millis() as u64,
            }),
            Ok(Err(join_err)) => Err(BusError::HandlerFailed {
                topic: topic_owned,
                detail: format!("handler panicked: {join_err}"),
            }),
            Ok(Ok(Err(detail))) => Err(BusError::HandlerFailed {
                topic: topic_owned,
                detail,
            }),
            Ok(Ok(Ok(reply))) => Ok(reply),
        }
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryBackend;

    fn bus() -> Bus {
        Bus::single("core-1", Arc::new(MemoryBackend::new()))
    }

    #[tokio::test]
    async fn publish_stamps_identity_and_time() {
        let bus = bus();
        let mut sub = bus.subscribe("platform.kernel.**");
        bus.publish(
            "platform.kernel.event.position-updated",
            "meridian.v1.PositionUpdatedEvent",
            vec![1, 2, 3],
            None,
            None,
        )
        .unwrap();

        let meta = sub.recv().await.unwrap().envelope.meta.unwrap();
        assert_eq!(meta.publisher_instance_id, "core-1");
        assert_eq!(meta.topic, "platform.kernel.event.position-updated");
        assert!(meta.published_at_ns > 0);
        assert!(!meta.message_id.is_empty());
    }

    #[tokio::test]
    async fn a_message_with_no_correlation_becomes_its_own_root() {
        let bus = bus();
        let mut sub = bus.subscribe("platform.kernel.**");
        bus.publish(
            "platform.kernel.event.position-updated",
            "t",
            vec![],
            None,
            None,
        )
        .unwrap();

        let meta = sub.recv().await.unwrap().envelope.meta.unwrap();
        assert_eq!(meta.correlation_id, meta.message_id);
        assert!(meta.causation_id.is_empty());
    }

    #[tokio::test]
    async fn correlation_and_causation_are_carried_through() {
        let bus = bus();
        let mut sub = bus.subscribe("platform.kernel.**");
        bus.publish(
            "platform.kernel.event.position-updated",
            "t",
            vec![],
            Some("corr-1"),
            Some("msg-0"),
        )
        .unwrap();

        let meta = sub.recv().await.unwrap().envelope.meta.unwrap();
        assert_eq!(meta.correlation_id, "corr-1");
        assert_eq!(meta.causation_id, "msg-0");
    }

    #[tokio::test]
    async fn call_reaches_its_handler() {
        let bus = bus();
        bus.serve("platform.reference.query.resolve-identifier", |env| {
            assert_eq!(env.payload, vec![7]);
            Ok(("meridian.v1.ResolveIdentifierReply".to_string(), vec![9]))
        });

        let (ty, payload) = bus
            .call(
                "platform.reference.query.resolve-identifier",
                "meridian.v1.ResolveIdentifierRequest",
                vec![7],
                None,
                None,
            )
            .await
            .unwrap();

        assert_eq!(ty, "meridian.v1.ResolveIdentifierReply");
        assert_eq!(payload, vec![9]);
    }

    #[tokio::test]
    async fn call_with_nothing_serving_is_distinct_from_a_timeout() {
        let bus = bus();
        let err = bus
            .call(
                "platform.reference.query.resolve-identifier",
                "t",
                vec![],
                None,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, BusError::NoHandler(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn a_slow_handler_times_out() {
        let bus = bus();
        bus.serve("platform.reference.query.resolve-instrument", |_| {
            std::thread::sleep(Duration::from_millis(200));
            Ok(("t".to_string(), vec![]))
        });

        let err = bus
            .call(
                "platform.reference.query.resolve-instrument",
                "t",
                vec![],
                None,
                Some(Duration::from_millis(20)),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, BusError::Timeout { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn a_failing_handler_reports_why() {
        let bus = bus();
        bus.serve("platform.reference.query.resolve-instrument", |_| {
            Err("replica is empty".into())
        });

        let err = bus
            .call(
                "platform.reference.query.resolve-instrument",
                "t",
                vec![],
                None,
                None,
            )
            .await
            .unwrap_err();
        match err {
            BusError::HandlerFailed { detail, .. } => assert_eq!(detail, "replica is empty"),
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_panicking_handler_does_not_take_down_the_caller() {
        let bus = bus();
        bus.serve("platform.reference.query.resolve-instrument", |_| {
            panic!("bad handler")
        });

        let err = bus
            .call(
                "platform.reference.query.resolve-instrument",
                "t",
                vec![],
                None,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, BusError::HandlerFailed { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn routing_sends_a_topic_to_its_named_backend() {
        let default_backend = Arc::new(MemoryBackend::new());
        let reference_backend = Arc::new(MemoryBackend::new());

        let mut backends: HashMap<String, Arc<dyn Backend>> = HashMap::new();
        backends.insert("default".into(), default_backend.clone());
        backends.insert("reference".into(), reference_backend.clone());

        let bus = Bus {
            backends,
            rules: vec![RouteRule {
                pattern: "platform.reference.**".into(),
                backend: "reference".into(),
            }],
            default_backend: "default".into(),
            handlers: RwLock::new(HashMap::new()),
            instance_id: "core-1".into(),
            default_timeout: Duration::from_secs(5),
        };

        let mut on_reference = reference_backend.subscribe("platform.reference.**");
        bus.publish(
            "platform.reference.event.instrument-applied",
            "t",
            vec![],
            None,
            None,
        )
        .unwrap();
        assert!(on_reference.recv().await.is_some());

        // The default backend saw nothing: the rule diverted it entirely.
        let mut on_default = default_backend.subscribe("platform.reference.**");
        assert!(on_default.rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn publishing_on_a_pattern_is_refused() {
        let bus = bus();
        let err = bus
            .publish(
                "platform.custody.*.event.sync-status",
                "t",
                vec![],
                None,
                None,
            )
            .unwrap_err();
        assert!(matches!(err, BusError::NotPublishable(_)));
    }
}
