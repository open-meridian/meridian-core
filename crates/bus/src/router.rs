//! The bus as consumers see it: routing, stamping, and request-reply.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use meridian_clock::Clock;
use meridian_domain::v1::Envelope;
use meridian_pb::v1::MessageMeta;
use tokio::sync::Notify;

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
use crate::backend::Handler;
pub use crate::backend::HandlerReply;

/// What a sidecar stamps on a question it asks for its plugin, and a core
/// component never does (decisions/014): the person it is asked for, and the
/// plugin's read scope.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stamp {
    /// The person a command is sent for; empty when the plugin acts as itself.
    pub acting_for_subject: String,

    /// The delegation that person acted through, when the dashboard's
    /// assertion named one: the CLI, their agent, an MCP client (W4.9,
    /// decisions/029, contract v9). Empty for a person at the dashboard in a
    /// browser, and always beside a person, never alone.
    pub acting_through_delegation: String,

    /// The client's registered name, beside the delegation (W4.9, contract
    /// v10), so a store records "the person, through that client" without a
    /// second read. Empty whenever the delegation is.
    pub acting_through_client: String,

    /// The accounts the plugin may read, stamped with the mark that they
    /// apply (W4.11): `Some`, an empty one included, is a plugin's read, which
    /// a store answers only within, and an empty one with nothing. `None` is
    /// a core component reading as itself.
    pub account_scope: Option<Vec<String>>,
}

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

    /// The deployment's clock, which stamps every message's time. Given, not
    /// read from the wall: a bus that read its own would stamp a replay with
    /// the time it was replayed.
    clock: Arc<dyn Clock>,
}

impl Bus {
    /// A bus with one backend and no routing rules, which is the shape of a
    /// single-host deployment.
    pub fn single(
        instance_id: impl Into<String>,
        backend: Arc<dyn Backend>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let mut backends = HashMap::new();
        backends.insert("default".to_string(), backend);
        Self {
            backends,
            rules: Vec::new(),
            default_backend: "default".to_string(),
            handlers: RwLock::new(HashMap::new()),
            instance_id: instance_id.into(),
            default_timeout: Duration::from_secs(5),
            clock,
        }
    }

    pub fn with_rules(mut self, rules: Vec<RouteRule>) -> Self {
        self.rules = rules;
        self
    }

    /// How long a question waits when its asker states no bound of its own.
    ///
    /// Five seconds by default, which suits a question answered from memory
    /// and suits nothing else. A caller with slower work to wait on passes
    /// its own bound to `call`; this exists so that a test can shorten the
    /// default and prove the caller's bound is the one being used.
    pub fn with_default_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = timeout;
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
            meta: Some(self.meta(
                &message_id,
                topic,
                correlation_id,
                causation_id,
                &Stamp::default(),
            )),
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
        let handler: Handler = Arc::new(handler);
        self.handlers
            .write()
            .expect("handler lock poisoned")
            .insert(topic.to_string(), Arc::clone(&handler));

        // And offered to other processes, where the backend has somewhere to
        // offer it. The in-process backend does nothing here, because the map
        // above is already the answer.
        self.backend_for(topic).serve(topic, handler);
    }

    /// Register a handler, and signal once each answer has been delivered.
    ///
    /// For the one component that replies and then stops: first run applies a
    /// configuration, answers the wizard and ends. Exiting when the handler
    /// returns lost the answer often enough to be seen once under load, and
    /// the wizard then reported a timeout for work that had completed --
    /// every Secret written and the Job's own rights given up.
    pub fn serve_delivered<F>(&self, topic: &str, handler: F, delivered: Arc<Notify>)
    where
        F: Fn(Envelope) -> HandlerReply + Send + Sync + 'static,
    {
        let handler: Handler = Arc::new(handler);
        self.handlers
            .write()
            .expect("handler lock poisoned")
            .insert(topic.to_string(), Arc::clone(&handler));

        self.backend_for(topic)
            .serve_delivered(topic, handler, delivered);
    }

    /// Ask a question and wait for the answer, bounded in time.
    ///
    /// Answered here when something in this process serves the topic, and by
    /// the backend when nothing does. The local path is not an optimisation:
    /// a deployment that runs everything in one process should not need a
    /// broker to ask itself a question, and the tests that hold this contract
    /// run without one.
    ///
    /// There is still no reply topic and no reply address in the envelope. How
    /// an answer finds its way back is the transport's business, which is what
    /// keeps a plugin from ever addressing one.
    pub async fn call(
        &self,
        topic: &str,
        payload_type: &str,
        payload: Vec<u8>,
        correlation_id: Option<&str>,
        timeout: Option<Duration>,
    ) -> crate::Result<(String, Vec<u8>)> {
        self.call_for(topic, payload_type, payload, correlation_id, timeout, "")
            .await
    }

    /// [`Bus::call`], on behalf of a person.
    ///
    /// For a core component that acts for somebody signed in -- the dashboard,
    /// asking the conductor to change what a deployment admin changed. The
    /// person is stamped as `acting_for_subject` so the answering component
    /// can record who did it. A plugin never reaches this: its sidecar stamps
    /// the person itself, from an assertion it has verified (decisions/014).
    /// An empty subject is a call on nobody's behalf.
    pub async fn call_for(
        &self,
        topic: &str,
        payload_type: &str,
        payload: Vec<u8>,
        correlation_id: Option<&str>,
        timeout: Option<Duration>,
        acting_for_subject: &str,
    ) -> crate::Result<(String, Vec<u8>)> {
        let stamp = Stamp {
            acting_for_subject: acting_for_subject.to_string(),
            ..Stamp::default()
        };
        self.call_stamped(
            topic,
            payload_type,
            payload,
            correlation_id,
            timeout,
            &stamp,
        )
        .await
    }

    /// [`Bus::call`], with what a sidecar stamps for its plugin: the person,
    /// and the plugin's read scope marked as applying (W4.11), so the
    /// answering store reads within it and an empty one as nothing.
    pub async fn call_stamped(
        &self,
        topic: &str,
        payload_type: &str,
        payload: Vec<u8>,
        correlation_id: Option<&str>,
        timeout: Option<Duration>,
        stamp: &Stamp,
    ) -> crate::Result<(String, Vec<u8>)> {
        let handler = self
            .handlers
            .read()
            .expect("handler lock poisoned")
            .get(topic)
            .cloned();

        let message_id = uuid::Uuid::new_v4().to_string();
        let envelope = Envelope {
            meta: Some(self.meta(&message_id, topic, correlation_id, None, stamp)),
            payload_type: payload_type.to_string(),
            payload,
        };

        let timeout = timeout.unwrap_or(self.default_timeout);
        let topic_owned = topic.to_string();

        let Some(handler) = handler else {
            // Nobody here serves it. Ask whoever does, wherever they are; a
            // backend with nowhere to ask answers NoHandler, which is what the
            // caller would have been told a moment ago anyway.
            let answered = self
                .backend_for(topic)
                .request(topic, envelope, timeout)
                .await?;
            return Ok((answered.payload_type, answered.payload));
        };

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

    /// The clock this bus stamps with, which is the deployment's: a component
    /// handed the bus reads the same time the messages it publishes carry.
    pub fn clock(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.clock)
    }

    /// Identity, time and the message id, stamped here and never by a caller.
    fn meta(
        &self,
        message_id: &str,
        topic: &str,
        correlation_id: Option<&str>,
        causation_id: Option<&str>,
        stamp: &Stamp,
    ) -> MessageMeta {
        MessageMeta {
            message_id: message_id.to_string(),
            // An empty correlation starts a new causal chain, and this message
            // is its root.
            correlation_id: correlation_id.unwrap_or(message_id).to_string(),
            causation_id: causation_id.unwrap_or_default().to_string(),
            publisher_instance_id: self.instance_id.clone(),
            topic: topic.to_string(),
            schema_version: "v1".to_string(),
            published_at_ns: self.clock.now_ns(),
            acting_for_subject: stamp.acting_for_subject.clone(),
            acting_through_delegation: stamp.acting_through_delegation.clone(),
            acting_through_client: stamp.acting_through_client.clone(),
            // A read's scope is the sidecar's to stamp, for a plugin, marked
            // as applying; a core component reads as itself, unmarked.
            account_scope: stamp.account_scope.clone().unwrap_or_default(),
            account_scope_applies: stamp.account_scope.is_some(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryBackend;
    use meridian_clock::ManualClock;

    /// A fixed time, so a stamp can be checked exactly.
    const NOW: i64 = 1_790_553_600_000_000_000;

    fn bus() -> Bus {
        Bus::single(
            "core-1",
            Arc::new(MemoryBackend::new()),
            Arc::new(ManualClock::at(NOW)),
        )
    }

    #[tokio::test]
    async fn the_time_stamped_is_the_clocks_it_was_given() {
        let clock = Arc::new(ManualClock::at(NOW));
        let bus = Bus::single("core-1", Arc::new(MemoryBackend::new()), clock.clone());
        let mut sub = bus.subscribe("platform.street.**");

        bus.publish(
            "platform.street.event.position-updated",
            "t",
            vec![],
            None,
            None,
        )
        .unwrap();
        clock.advance(1_000);
        bus.publish(
            "platform.street.event.position-updated",
            "t",
            vec![],
            None,
            None,
        )
        .unwrap();

        let first = sub.recv().await.unwrap().envelope.meta.unwrap();
        let second = sub.recv().await.unwrap().envelope.meta.unwrap();
        assert_eq!(first.published_at_ns, NOW);
        assert_eq!(second.published_at_ns, NOW + 1_000);
        assert_eq!(
            bus.clock().now_ns(),
            NOW + 1_000,
            "the bus hands out the same clock"
        );
    }

    #[tokio::test]
    async fn publish_stamps_identity_and_time() {
        let bus = bus();
        let mut sub = bus.subscribe("platform.street.**");
        bus.publish(
            "platform.street.event.position-updated",
            "meridian.v1.PositionUpdatedEvent",
            vec![1, 2, 3],
            None,
            None,
        )
        .unwrap();

        let meta = sub.recv().await.unwrap().envelope.meta.unwrap();
        assert_eq!(meta.publisher_instance_id, "core-1");
        assert_eq!(meta.topic, "platform.street.event.position-updated");
        assert_eq!(meta.published_at_ns, NOW);
        assert!(!meta.message_id.is_empty());
    }

    #[tokio::test]
    async fn a_message_with_no_correlation_becomes_its_own_root() {
        let bus = bus();
        let mut sub = bus.subscribe("platform.street.**");
        bus.publish(
            "platform.street.event.position-updated",
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
        let mut sub = bus.subscribe("platform.street.**");
        bus.publish(
            "platform.street.event.position-updated",
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
            clock: Arc::new(ManualClock::at(NOW)),
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

#[cfg(test)]
mod acting_for {
    use std::sync::Arc;

    use super::Bus;
    use crate::MemoryBackend;

    /// The subject the answering component sees, when asked with and without one.
    async fn subject_seen(acting_for: Option<&str>) -> String {
        let bus = Bus::single(
            "dashboard-1",
            Arc::new(MemoryBackend::new()),
            Arc::new(meridian_clock::SystemClock),
        );
        bus.serve("platform.config.command.define-account", |envelope| {
            let meta = envelope.meta.unwrap_or_default();
            Ok((String::new(), meta.acting_for_subject.into_bytes()))
        });

        let topic = "platform.config.command.define-account";
        let (_, subject) = match acting_for {
            Some(person) => {
                bus.call_for(topic, "", Vec::new(), None, None, person)
                    .await
            }
            None => bus.call(topic, "", Vec::new(), None, None).await,
        }
        .expect("answered");
        String::from_utf8(subject).expect("utf-8")
    }

    /// The scope and its mark the answering store sees, asked with a stamp
    /// or without one.
    async fn scope_seen(scope: Option<Vec<String>>) -> (String, String) {
        let bus = Bus::single(
            "sidecar-1",
            Arc::new(MemoryBackend::new()),
            Arc::new(meridian_clock::SystemClock),
        );
        let topic = "platform.street.query.list-custodial-positions";
        bus.serve(topic, |envelope| {
            let meta = envelope.meta.unwrap_or_default();
            Ok((
                meta.account_scope_applies.to_string(),
                meta.account_scope.join(",").into_bytes(),
            ))
        });
        let (applies, scope) = match scope {
            Some(accounts) => {
                let stamp = super::Stamp {
                    account_scope: Some(accounts),
                    ..super::Stamp::default()
                };
                bus.call_stamped(topic, "", Vec::new(), None, None, &stamp)
                    .await
            }
            None => bus.call(topic, "", Vec::new(), None, None).await,
        }
        .expect("answered");
        (applies, String::from_utf8(scope).expect("utf-8"))
    }

    #[tokio::test]
    async fn a_plugins_read_carries_its_scope_marked_and_an_empty_one_still_applies() {
        // W4.11: without the mark an empty scope and a core component's read
        // of everything would look the same to a store.
        assert_eq!(
            scope_seen(Some(vec!["ACC-1".into(), "ACC-2".into()])).await,
            ("true".into(), "ACC-1,ACC-2".into())
        );
        assert_eq!(
            scope_seen(Some(vec![])).await,
            ("true".into(), String::new())
        );
        assert_eq!(scope_seen(None).await, ("false".into(), String::new()));
    }

    #[tokio::test]
    async fn a_person_through_a_client_carries_the_delegation_beside_them() {
        // W4.9 (contract v9): the sidecar stamps the delegation the
        // assertion named beside the person, and a store reads both.
        let bus = Bus::single(
            "sidecar-1",
            Arc::new(MemoryBackend::new()),
            Arc::new(meridian_clock::SystemClock),
        );
        let topic = "platform.book.command.record-opening-balance";
        bus.serve(topic, |envelope| {
            let meta = envelope.meta.unwrap_or_default();
            Ok((
                meta.acting_for_subject,
                meta.acting_through_delegation.into_bytes(),
            ))
        });
        let stamp = super::Stamp {
            acting_for_subject: "https://directory.example.org|8812".into(),
            acting_through_delegation: "DLG-1".into(),
            ..super::Stamp::default()
        };
        let (person, delegation) = bus
            .call_stamped(topic, "", Vec::new(), None, None, &stamp)
            .await
            .expect("answered");
        assert_eq!(person, "https://directory.example.org|8812");
        assert_eq!(String::from_utf8(delegation).expect("utf-8"), "DLG-1");
    }

    #[tokio::test]
    async fn a_call_for_a_person_carries_them_and_a_plain_call_carries_nobody() {
        assert_eq!(
            subject_seen(Some("https://directory.example.org|8812")).await,
            "https://directory.example.org|8812"
        );
        assert_eq!(subject_seen(None).await, "");
    }
}
