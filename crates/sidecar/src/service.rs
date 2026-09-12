//! The six operations, implemented over the bus.
//!
//! One sidecar serves exactly one plugin. That is why no request after
//! `Register` carries an instance id: there is only one plugin it could be, and
//! a field a caller fills in is a field a caller can get wrong. Identity comes
//! from the registration, and the registration comes from the process the
//! sidecar was started for.

use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use meridian_bus::{Bus, BusError};
use meridian_pb::v1::sidecar_service_server::SidecarService;
use meridian_pb::v1::{
    CallFailure, CallReply, CallRequest, Delivery, HeartbeatReply, HeartbeatRequest, LeaveReply,
    LeaveRequest, PublishReply, PublishRequest, RegisterReply, RegisterRequest, SubscribeRequest,
};
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status};

use crate::grants::{GrantTable, Grants};

/// What the sidecar knows about the plugin it serves, once admitted.
#[derive(Debug, Clone)]
pub struct Registration {
    pub instance_id: String,
    pub role: String,
    pub grants: Grants,
    pub healthy: bool,
    pub last_heartbeat_ns: i64,
    pub departed: bool,
}

pub struct Sidecar {
    bus: Arc<Bus>,
    deployment_id: String,
    schema_version: String,

    /// `None` until access control has loaded.
    ///
    /// Distinct from an empty table on purpose: an empty table denies
    /// everything and is a decision, while a missing table means nothing is
    /// known yet, and admitting a plugin then would admit it unenforced.
    grants: Arc<RwLock<Option<GrantTable>>>,

    state: Arc<RwLock<Option<Registration>>>,
}

impl Sidecar {
    pub fn new(
        bus: Arc<Bus>,
        deployment_id: impl Into<String>,
        schema_version: impl Into<String>,
    ) -> Self {
        Self {
            bus,
            deployment_id: deployment_id.into(),
            schema_version: schema_version.into(),
            grants: Arc::new(RwLock::new(None)),
            state: Arc::new(RwLock::new(None)),
        }
    }

    /// Make access control available. Nothing is admitted before this.
    pub fn load_grants(&self, table: GrantTable) {
        *self.grants.write().expect("grant lock poisoned") = Some(table);
    }

    pub fn registration(&self) -> Option<Registration> {
        self.state.read().expect("state lock poisoned").clone()
    }

    // clippy would have this box the error. Every method on the service trait
    // already returns `Result<_, Status>` because tonic requires it, so boxing
    // here alone would buy nothing and cost an unbox at each of the six call
    // sites.
    #[allow(clippy::result_large_err)]
    fn admitted(&self) -> Result<Registration, Status> {
        match self.state.read().expect("state lock poisoned").clone() {
            Some(r) if !r.departed => Ok(r),
            Some(_) => Err(Status::failed_precondition(
                "this instance has already left",
            )),
            None => Err(Status::failed_precondition(
                "not registered: call Register before any other operation",
            )),
        }
    }
}

type DeliveryStream = Pin<Box<dyn Stream<Item = Result<Delivery, Status>> + Send>>;

#[tonic::async_trait]
impl SidecarService for Sidecar {
    async fn register(
        &self,
        request: Request<RegisterRequest>,
    ) -> Result<Response<RegisterReply>, Status> {
        let req = request.into_inner();

        // Fail closed. A plugin that starts before access control has loaded is
        // refused rather than admitted unenforced, because the second failure
        // mode is invisible: it publishes successfully and nobody finds out.
        let table = match self.grants.read().expect("grant lock poisoned").clone() {
            Some(t) => t,
            None => {
                tracing::warn!(
                    instance = req.instance_id,
                    "refused: access control not loaded"
                );
                return Ok(Response::new(RegisterReply {
                    admitted: false,
                    refusal_reason: "access control not loaded".into(),
                    ..Default::default()
                }));
            }
        };

        // Refused at the door rather than discovered later in a decode failure,
        // where the symptom would be a corrupt-looking message rather than a
        // version mismatch.
        if !req.schema_version.is_empty() && req.schema_version != self.schema_version {
            return Ok(Response::new(RegisterReply {
                admitted: false,
                refusal_reason: format!("schema version {} not supported", req.schema_version),
                ..Default::default()
            }));
        }

        // One sidecar serves one plugin, and this is where that stops being a
        // description and becomes enforcement.
        //
        // The registration is a single slot because v1 ran a sidecar container
        // per plugin, so there was never a second plugin to hold. A runtime
        // that exposes one shared port breaks that assumption, and the failure
        // is silent and in the wrong direction: nothing here identifies the
        // caller on a later request, so every plugin operates under whichever
        // registration was written last. A read-only role registering before a
        // connector would inherit the connector's write grants.
        //
        // Refusing the second plugin makes that a startup failure an operator
        // reads instead of an escalation nobody sees. Carrying a caller
        // identity on every request is the real fix and it changes the
        // contract, so it goes through a queued task rather than an edit here.
        if let Some(live) = self.state.read().expect("state lock poisoned").as_ref() {
            if !live.departed && live.instance_id != req.instance_id {
                tracing::warn!(
                    instance = req.instance_id,
                    held_by = live.instance_id,
                    "refused: this sidecar already serves another plugin"
                );
                return Ok(Response::new(RegisterReply {
                    admitted: false,
                    refusal_reason: format!(
                        "this sidecar already serves `{}`; one sidecar serves one plugin",
                        live.instance_id
                    ),
                    ..Default::default()
                }));
            }
        }

        let grants = table.resolve(&req.role, &req.tags);
        if grants.publish.is_empty() && grants.subscribe.is_empty() {
            return Ok(Response::new(RegisterReply {
                admitted: false,
                refusal_reason: format!("role `{}` has no grants", req.role),
                ..Default::default()
            }));
        }

        *self.state.write().expect("state lock poisoned") = Some(Registration {
            instance_id: req.instance_id.clone(),
            role: req.role.clone(),
            grants: grants.clone(),
            healthy: true,
            last_heartbeat_ns: now_ns(),
            departed: false,
        });

        tracing::info!(instance = req.instance_id, role = req.role, "admitted");

        // Grants come back so a plugin can fail at startup rather than at its
        // first refused publish, which moves the failure to where an operator
        // is already looking.
        Ok(Response::new(RegisterReply {
            admitted: true,
            deployment_id: self.deployment_id.clone(),
            refusal_reason: String::new(),
            publish_grants: grants.publish,
            subscribe_grants: grants.subscribe,
        }))
    }

    async fn publish(
        &self,
        request: Request<PublishRequest>,
    ) -> Result<Response<PublishReply>, Status> {
        let registration = self.admitted()?;
        let req = request.into_inner();

        if !registration.grants.may_publish(&req.topic) {
            return Ok(Response::new(PublishReply {
                accepted: false,
                message_id: String::new(),
                refusal_reason: format!("no publish grant for {}", req.topic),
            }));
        }

        let correlation = non_empty(&req.correlation_id);
        let causation = non_empty(&req.causation_id);

        match self.bus.publish(
            &req.topic,
            &req.payload_type,
            req.payload,
            correlation.as_deref(),
            causation.as_deref(),
        ) {
            Ok(message_id) => Ok(Response::new(PublishReply {
                accepted: true,
                message_id,
                refusal_reason: String::new(),
            })),
            Err(BusError::NotPublishable(topic)) => Ok(Response::new(PublishReply {
                accepted: false,
                message_id: String::new(),
                refusal_reason: format!("`{topic}` is not a publishable topic"),
            })),
            Err(other) => Err(Status::internal(other.to_string())),
        }
    }

    type SubscribeStream = DeliveryStream;

    async fn subscribe(
        &self,
        request: Request<SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        let registration = self.admitted()?;
        let pattern = request.into_inner().pattern;

        // Checked once, here, rather than per delivered message. A subscription
        // is a standing decision; re-deciding it on every message would cost
        // the same answer thousands of times.
        if !registration.grants.may_subscribe(&pattern) {
            return Err(Status::permission_denied(format!(
                "no subscribe grant for {pattern}"
            )));
        }

        let stream = self.bus.subscribe(&pattern).into_stream().map(|d| {
            Ok(Delivery {
                envelope: Some(d.envelope),
            })
        });

        Ok(Response::new(Box::pin(stream) as Self::SubscribeStream))
    }

    async fn call(&self, request: Request<CallRequest>) -> Result<Response<CallReply>, Status> {
        let registration = self.admitted()?;
        let req = request.into_inner();

        // A call publishes a question, so it needs the publish grant. Treating
        // it as a read would let a plugin reach any handler in the deployment.
        if !registration.grants.may_publish(&req.topic) {
            return Ok(Response::new(failed_call(
                CallFailure::Refused,
                format!("no grant for {}", req.topic),
            )));
        }

        let timeout = match req.timeout_ms {
            ms if ms > 0 => Some(Duration::from_millis(ms as u64)),
            // Zero means the sidecar's default, never unbounded: the surface
            // offers no way to wait forever.
            _ => None,
        };

        let result = self
            .bus
            .call(
                &req.topic,
                &req.payload_type,
                req.payload,
                non_empty(&req.correlation_id).as_deref(),
                timeout,
            )
            .await;

        Ok(Response::new(match result {
            Ok((payload_type, payload)) => CallReply {
                ok: true,
                payload_type,
                payload,
                failure: CallFailure::Unspecified as i32,
                failure_detail: String::new(),
            },
            // Each of these leads the caller somewhere different, which is why
            // they are distinct rather than one error string.
            Err(BusError::NoHandler(topic)) => {
                failed_call(CallFailure::NoHandler, format!("nothing serves {topic}"))
            }
            Err(e @ BusError::Timeout { .. }) => failed_call(CallFailure::Timeout, e.to_string()),
            Err(BusError::HandlerFailed { detail, .. }) => {
                failed_call(CallFailure::HandlerError, detail)
            }
            Err(other) => failed_call(CallFailure::HandlerError, other.to_string()),
        }))
    }

    async fn heartbeat(
        &self,
        request: Request<HeartbeatRequest>,
    ) -> Result<Response<HeartbeatReply>, Status> {
        self.admitted()?;
        let req = request.into_inner();

        if let Some(state) = self.state.write().expect("state lock poisoned").as_mut() {
            state.healthy = req.healthy;
            state.last_heartbeat_ns = now_ns();
        }

        if !req.healthy {
            tracing::warn!(detail = req.detail, "plugin reports itself unhealthy");
        }

        Ok(Response::new(HeartbeatReply {}))
    }

    async fn leave(&self, request: Request<LeaveRequest>) -> Result<Response<LeaveReply>, Status> {
        let registration = self.admitted()?;
        let reason = request.into_inner().reason;

        if let Some(state) = self.state.write().expect("state lock poisoned").as_mut() {
            state.departed = true;
            state.healthy = false;
        }

        tracing::info!(instance = registration.instance_id, reason, "plugin left");
        Ok(Response::new(LeaveReply {}))
    }
}

fn failed_call(failure: CallFailure, detail: String) -> CallReply {
    CallReply {
        ok: false,
        payload_type: String::new(),
        payload: Vec::new(),
        failure: failure as i32,
        failure_detail: detail,
    }
}

/// An empty string in a proto field means "not set". Turning it into `None`
/// here keeps that translation in one place instead of at every call site.
fn non_empty(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
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
    use meridian_bus::MemoryBackend;

    const GRANTS: &str = r#"{
      "roles": {
        "custody": {
          "publish": [
            "platform.kernel.command.record-holding",
            "platform.reference.event.instrument-missing",
            "platform.reference.query.resolve-identifier",
            "platform.custody.*.event.sync-status"
          ],
          "subscribe": ["platform.reference.event.instrument-applied"]
        },
        "dashboard": {
          "publish": [],
          "subscribe": ["platform.kernel.event.*"]
        }
      }
    }"#;

    fn sidecar(load_grants: bool) -> Sidecar {
        let bus = Arc::new(Bus::single(
            "sidecar-custody-1",
            Arc::new(MemoryBackend::new()),
        ));
        let sc = Sidecar::new(bus, "dep-local-1", "v1");
        if load_grants {
            sc.load_grants(GrantTable::from_json(GRANTS).unwrap());
        }
        sc
    }

    fn register_req() -> RegisterRequest {
        RegisterRequest {
            instance_id: "custody-snaptrade-1".into(),
            role: "custody".into(),
            tags: vec![],
            schema_version: "v1".into(),
        }
    }

    async fn admitted_sidecar() -> Sidecar {
        let sc = sidecar(true);
        let reply = sc
            .register(Request::new(register_req()))
            .await
            .unwrap()
            .into_inner();
        assert!(reply.admitted);
        sc
    }

    #[tokio::test]
    async fn admission_is_refused_before_access_control_loads() {
        let sc = sidecar(false);
        let reply = sc
            .register(Request::new(register_req()))
            .await
            .unwrap()
            .into_inner();

        assert!(!reply.admitted);
        assert_eq!(reply.refusal_reason, "access control not loaded");
        // Refused, not deferred: nothing was admitted unenforced.
        assert!(sc.registration().is_none());
    }

    #[tokio::test]
    async fn admission_is_refused_on_a_schema_mismatch() {
        let sc = sidecar(true);
        let mut req = register_req();
        req.schema_version = "v0".into();

        let reply = sc.register(Request::new(req)).await.unwrap().into_inner();
        assert!(!reply.admitted);
        assert!(reply.refusal_reason.contains("v0"));
    }

    #[tokio::test]
    async fn admission_is_refused_for_a_role_with_no_grants() {
        let sc = sidecar(true);
        let mut req = register_req();
        req.role = "nonexistent".into();

        let reply = sc.register(Request::new(req)).await.unwrap().into_inner();
        assert!(!reply.admitted);
        assert!(reply.refusal_reason.contains("no grants"));
    }

    #[tokio::test]
    async fn admission_returns_the_grants_so_a_plugin_can_fail_at_startup() {
        let sc = sidecar(true);
        let reply = sc
            .register(Request::new(register_req()))
            .await
            .unwrap()
            .into_inner();

        assert!(reply.admitted);
        assert_eq!(reply.deployment_id, "dep-local-1");
        assert!(reply
            .publish_grants
            .contains(&"platform.kernel.command.record-holding".to_string()));
        assert_eq!(
            reply.subscribe_grants,
            vec!["platform.reference.event.instrument-applied".to_string()]
        );
    }

    #[tokio::test]
    async fn a_second_plugin_is_refused_rather_than_taking_over_the_first() {
        let sc = admitted_sidecar().await;

        // A read-only role arrives at the same sidecar. Nothing on a later
        // request says who is calling, so admitting this would not give it its
        // own identity: it would replace the connector's, and every subsequent
        // call from either plugin would be judged against whichever grants were
        // written last. Refusal at the door is the only place this is visible.
        let reply = sc
            .register(Request::new(RegisterRequest {
                instance_id: "dashboard-1".into(),
                role: "dashboard".into(),
                tags: vec![],
                schema_version: "v1".into(),
            }))
            .await
            .unwrap()
            .into_inner();

        assert!(!reply.admitted);
        assert!(
            reply.refusal_reason.contains("already serves"),
            "unhelpful refusal: {}",
            reply.refusal_reason
        );

        // The first plugin is untouched, rather than left holding a half-
        // replaced registration.
        let held = sc.registration().unwrap();
        assert_eq!(held.instance_id, "custody-snaptrade-1");
        assert!(held
            .grants
            .may_publish("platform.kernel.command.record-holding"));
    }

    #[tokio::test]
    async fn the_same_plugin_may_register_again_after_a_restart() {
        let sc = admitted_sidecar().await;

        // A plugin that restarted reconnects under the identity it already
        // holds. Refusing that would leave a sidecar permanently occupied by a
        // plugin that no longer exists.
        let reply = sc
            .register(Request::new(register_req()))
            .await
            .unwrap()
            .into_inner();
        assert!(reply.admitted);
    }

    #[tokio::test]
    async fn a_sidecar_is_free_again_once_its_plugin_leaves() {
        let sc = admitted_sidecar().await;
        sc.leave(Request::new(LeaveRequest::default()))
            .await
            .unwrap();

        let reply = sc
            .register(Request::new(RegisterRequest {
                instance_id: "dashboard-1".into(),
                role: "dashboard".into(),
                tags: vec![],
                schema_version: "v1".into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(
            reply.admitted,
            "refused after the first plugin left: {}",
            reply.refusal_reason
        );
    }

    #[tokio::test]
    async fn nothing_works_before_registering() {
        let sc = sidecar(true);
        let err = sc
            .publish(Request::new(PublishRequest {
                topic: "platform.kernel.command.record-holding".into(),
                ..Default::default()
            }))
            .await
            .unwrap_err();

        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn publishing_an_ungranted_topic_names_the_missing_grant() {
        let sc = admitted_sidecar().await;
        let reply = sc
            .publish(Request::new(PublishRequest {
                topic: "platform.kernel.command.record-statement".into(),
                payload_type: "meridian.v1.RecordHoldingsStatementRequest".into(),
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();

        assert!(!reply.accepted);
        assert!(reply.refusal_reason.contains("record-statement"));
    }

    #[tokio::test]
    async fn a_granted_publish_reaches_the_bus_stamped_by_the_sidecar() {
        let sc = admitted_sidecar().await;
        let mut sub = sc.bus.subscribe("platform.kernel.**");

        let reply = sc
            .publish(Request::new(PublishRequest {
                topic: "platform.kernel.command.record-holding".into(),
                payload_type: "meridian.v1.RecordHoldingRequest".into(),
                payload: vec![1, 2, 3],
                correlation_id: "corr-1".into(),
                causation_id: "msg-0".into(),
            }))
            .await
            .unwrap()
            .into_inner();

        assert!(reply.accepted);

        let meta = sub.recv().await.unwrap().envelope.meta.unwrap();
        // The plugin supplied neither of these and could not have.
        assert_eq!(meta.publisher_instance_id, "sidecar-custody-1");
        assert!(meta.published_at_ns > 0);
        // The causal links it did supply were carried through.
        assert_eq!(meta.correlation_id, "corr-1");
        assert_eq!(meta.causation_id, "msg-0");
    }

    #[tokio::test]
    async fn an_instance_scoped_topic_is_covered_by_its_wildcard_grant() {
        let sc = admitted_sidecar().await;
        let reply = sc
            .publish(Request::new(PublishRequest {
                topic: "platform.custody.custody-snaptrade-1.event.sync-status".into(),
                payload_type: "meridian.v1.SyncStatusEvent".into(),
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();

        assert!(reply.accepted, "{}", reply.refusal_reason);
    }

    #[tokio::test]
    async fn an_ungranted_subscription_is_refused_rather_than_silently_empty() {
        let sc = admitted_sidecar().await;
        // A boxed stream is not Debug, so unwrap_err is unavailable here.
        match sc
            .subscribe(Request::new(SubscribeRequest {
                pattern: "platform.kernel.**".into(),
            }))
            .await
        {
            Err(status) => assert_eq!(status.code(), tonic::Code::PermissionDenied),
            Ok(_) => panic!("an ungranted subscription was accepted"),
        }
    }

    #[tokio::test]
    async fn a_granted_subscription_delivers() {
        let sc = admitted_sidecar().await;
        let response = sc
            .subscribe(Request::new(SubscribeRequest {
                pattern: "platform.reference.event.instrument-applied".into(),
            }))
            .await
            .unwrap();

        sc.bus
            .publish(
                "platform.reference.event.instrument-applied",
                "meridian.v1.InstrumentAppliedEvent",
                vec![4, 2],
                None,
                None,
            )
            .unwrap();

        let mut stream = response.into_inner();
        let delivery = stream.next().await.unwrap().unwrap();
        assert_eq!(delivery.envelope.unwrap().payload, vec![4, 2]);
    }

    #[tokio::test]
    async fn a_call_reaches_its_handler() {
        let sc = admitted_sidecar().await;
        sc.bus
            .serve("platform.reference.query.resolve-identifier", |_| {
                Ok(("meridian.v1.ResolveIdentifierReply".to_string(), vec![7]))
            });

        let reply = sc
            .call(Request::new(CallRequest {
                topic: "platform.reference.query.resolve-identifier".into(),
                payload_type: "meridian.v1.ResolveIdentifierRequest".into(),
                payload: vec![],
                correlation_id: String::new(),
                timeout_ms: 1000,
            }))
            .await
            .unwrap()
            .into_inner();

        assert!(reply.ok);
        assert_eq!(reply.payload, vec![7]);
    }

    #[tokio::test]
    async fn call_failures_are_distinguished() {
        let sc = admitted_sidecar().await;

        // Nothing serving: distinct from a timeout, because the caller's next
        // move differs.
        let reply = sc
            .call(Request::new(CallRequest {
                topic: "platform.reference.query.resolve-identifier".into(),
                timeout_ms: 100,
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(reply.failure, CallFailure::NoHandler as i32);

        // Not granted.
        let reply = sc
            .call(Request::new(CallRequest {
                topic: "platform.kernel.query.list-positions".into(),
                timeout_ms: 100,
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(reply.failure, CallFailure::Refused as i32);

        // Slow handler.
        sc.bus
            .serve("platform.reference.query.resolve-identifier", |_| {
                std::thread::sleep(Duration::from_millis(200));
                Ok(("t".to_string(), vec![]))
            });
        let reply = sc
            .call(Request::new(CallRequest {
                topic: "platform.reference.query.resolve-identifier".into(),
                timeout_ms: 20,
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(reply.failure, CallFailure::Timeout as i32);
    }

    #[tokio::test]
    async fn heartbeat_records_what_the_plugin_reports() {
        let sc = admitted_sidecar().await;
        sc.heartbeat(Request::new(HeartbeatRequest {
            healthy: false,
            detail: "brokerage credentials rejected".into(),
        }))
        .await
        .unwrap();

        let state = sc.registration().unwrap();
        assert!(!state.healthy);
        assert!(state.last_heartbeat_ns > 0);
    }

    #[tokio::test]
    async fn leaving_closes_the_instance_to_further_work() {
        let sc = admitted_sidecar().await;
        sc.leave(Request::new(LeaveRequest {
            reason: "shutting down for redeploy".into(),
        }))
        .await
        .unwrap();

        assert!(sc.registration().unwrap().departed);

        let err = sc
            .publish(Request::new(PublishRequest {
                topic: "platform.kernel.command.record-holding".into(),
                ..Default::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }
}
