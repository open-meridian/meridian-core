//! The operations W4 declares, implemented over the bus.
//!
//! One sidecar serves exactly one plugin. That is why no request carries an
//! instance id, `Register` included: there is only one plugin it could be, and
//! a field a caller fills in is a field a caller can get wrong. Identity comes
//! from the sidecar's launch configuration, which is to say from whoever
//! deployed it, and the plugin is told what it is rather than asked.

use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use meridian_bus::{Bus, BusError};
use meridian_pb::v1::sidecar_service_server::SidecarService;
use meridian_pb::v1::{
    AccountScopeDelivery, CallFailure, CallReply, CallRequest, Delivery, HeartbeatReply,
    HeartbeatRequest, LeaveReply, LeaveRequest, PluginAccessReply, PluginAccessRequest,
    PublishReply, PublishRequest, RegisterReply, RegisterRequest, SettingsDelivery,
    SubscribeRequest, WatchAccountScopeRequest, WatchSettingsRequest,
};
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status};

use crate::grants::{Contract, Grants};

/// What the sidecar knows about the plugin it serves, once admitted.
#[derive(Debug, Clone)]
pub struct Registration {
    pub instance_id: String,
    pub roles: Vec<String>,
    pub grants: Grants,
    pub healthy: bool,
    pub last_heartbeat_ns: i64,
    pub departed: bool,
    /// The loopback port the plugin serves its page on, if it declared one
    /// (decisions/014). The front door forwards there and nowhere else.
    pub interface_port: Option<u16>,
    /// The plugin's own reason when it last said it was unhealthy.
    pub health_detail: String,
    /// The contract version it registered with.
    pub contract_version: String,
}

/// Who a sidecar was launched to serve.
///
/// Supplied when the sidecar is built, which is to say by whoever deployed it,
/// and never by the plugin. The instance and the roles decide the plugin's
/// topic access: a plugin that named its own roles would be choosing its own
/// privileges, and one that named its own instance could publish as a
/// sibling, because grants are written with instance wildcards so an
/// instance-scoped topic needs no grant minted per instance. The tags decide
/// nothing here; they are for people (decisions/020).
#[derive(Debug, Clone)]
pub struct Identity {
    pub instance_id: String,
    pub roles: Vec<String>,
    pub tags: Vec<String>,
}

impl Identity {
    pub fn new(instance_id: impl Into<String>, roles: Vec<String>) -> Self {
        Self {
            instance_id: instance_id.into(),
            roles,
            tags: Vec::new(),
        }
    }

    pub fn with_tags(mut self, tags: Vec<String>) -> Self {
        self.tags = tags;
        self
    }
}

pub struct Sidecar {
    pub(crate) bus: Arc<Bus>,
    deployment_id: String,
    pub(crate) identity: Identity,

    /// Decided once, at launch, from the contract compiled in: the union of
    /// the roles' grants, or why the roles were refused. Nothing loads later,
    /// so there is no moment when a plugin could register unenforced.
    grants: Result<Grants, String>,

    state: Arc<RwLock<Option<Registration>>>,

    /// The plugin's external accounts and the accounts they are linked to
    /// (W6.4), read from the conductor when a typed operation first needs
    /// them and forgotten whenever the conductor says the configuration
    /// changed. `None` until then.
    pub(crate) links: crate::typed::Links,

    /// Grants refused, and the latest reason, for the plugin report (W4.8):
    /// the sidecar sees refusals the plugin cannot report about itself.
    pub(crate) refusals: Arc<std::sync::Mutex<(i64, String)>>,
    /// Woken when registration changes, so a report goes out at once rather
    /// than at the next interval.
    pub(crate) changed: Arc<tokio::sync::Notify>,

    /// The dashboard's keys, to verify the person a command is sent for
    /// (W4.9). None where the sidecar was given none: such a command is
    /// refused, since nobody can be vouched for.
    pub(crate) verifier: Option<Arc<crate::front_door::Verifier>>,
    /// The plugin's write scope, as the conductor last said it.
    pub(crate) scope: crate::scope::Scope,
}

impl Sidecar {
    pub fn new(bus: Arc<Bus>, deployment_id: impl Into<String>, identity: Identity) -> Self {
        Self::under(Contract::embedded(), bus, deployment_id, identity)
    }

    /// Against a contract other than the one compiled in, for a test that
    /// wants roles the contract does not have yet.
    pub fn under(
        contract: &Contract,
        bus: Arc<Bus>,
        deployment_id: impl Into<String>,
        identity: Identity,
    ) -> Self {
        let grants = contract.grants_for(&identity.roles);
        if let Err(refusal) = &grants {
            tracing::warn!(
                instance = identity.instance_id,
                "launched with roles that are refused: {refusal}"
            );
        }
        Self {
            bus,
            deployment_id: deployment_id.into(),
            identity,
            grants,
            state: Arc::new(RwLock::new(None)),
            links: crate::typed::Links::default(),
            refusals: Arc::default(),
            changed: Arc::default(),
            verifier: None,
            scope: crate::scope::Scope::default(),
        }
    }

    /// Verifying the person a command is sent for with the dashboard's keys,
    /// the same ones the front door holds.
    pub fn with_verifier(mut self, verifier: Arc<crate::front_door::Verifier>) -> Self {
        self.verifier = Some(verifier);
        self
    }

    pub fn registration(&self) -> Option<Registration> {
        self.state.read().expect("state lock poisoned").clone()
    }

    // clippy would have this box the error. Every method on the service trait
    // already returns `Result<_, Status>` because tonic requires it, so boxing
    // here alone would buy nothing and cost an unbox at each of the six call
    // sites.
    #[allow(clippy::result_large_err)]
    pub(crate) fn admitted(&self) -> Result<Registration, Status> {
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
type SettingsStream = Pin<Box<dyn Stream<Item = Result<SettingsDelivery, Status>> + Send>>;
type ScopeStream = Pin<Box<dyn Stream<Item = Result<AccountScopeDelivery, Status>> + Send>>;

/// What a v1 sidecar says to the operations contract v2 adds (W4.7, W4.10,
/// W4.11). A plugin built against v1 never calls them, and one built against
/// v2 is refused at registration until this sidecar admits v2, so this answer
/// is reached only by a plugin that skipped registering.
fn not_until_v2(operation: &str) -> Status {
    Status::unimplemented(format!(
        "{operation} is contract v2, and this sidecar admits v1; it arrives with the \
         dashboard's settings slice (kernel/dashboard-health-settings-and-bundle)"
    ))
}

#[tonic::async_trait]
impl SidecarService for Sidecar {
    async fn register(
        &self,
        request: Request<RegisterRequest>,
    ) -> Result<Response<RegisterReply>, Status> {
        let req = request.into_inner();

        // Fail closed. A sidecar launched with a name that is not a role is
        // refused rather than admitted with nothing, where the failure would
        // be invisible until the first refused publish.
        let grants = match &self.grants {
            Ok(grants) => grants.clone(),
            Err(refusal) => {
                return Ok(Response::new(RegisterReply {
                    admitted: false,
                    refusal_reason: format!("this sidecar was launched with {refusal}"),
                    ..Default::default()
                }));
            }
        };

        // Refused at the door rather than discovered later in a decode failure,
        // where the symptom would be a corrupt-looking message rather than a
        // version mismatch. A range, not a match: see `contract`.
        if let Err(refusal_reason) = crate::contract::admit(&req.schema_version) {
            return Ok(Response::new(RegisterReply {
                admitted: false,
                refusal_reason,
                ..Default::default()
            }));
        }

        // A page, if the plugin serves one, on a port that is a port. Refused
        // here rather than when the first person opens it, where the failure
        // would look like the plugin being down.
        let interface_port = match &req.interface {
            None => None,
            Some(declared) => match u16::try_from(declared.loopback_port) {
                Ok(port) if port != 0 => Some(port),
                _ => {
                    return Ok(Response::new(RegisterReply {
                        admitted: false,
                        refusal_reason: format!(
                            "the interface's loopback_port {} is not a port",
                            declared.loopback_port
                        ),
                        ..Default::default()
                    }));
                }
            },
        };

        // Grants come from what this sidecar was launched as, never from the
        // request. The request has nothing in it that could decide them.
        //
        // A second caller on this endpoint is therefore no longer an
        // escalation: it gets the same identity and the same grants, because
        // there is only one set to get. It is still not separable from the
        // first, and separating them is what one sidecar per plugin is for.
        //
        // No grants at all is admitted: a plugin holding no role, or only
        // roles no row names yet, registers and is refused every topic
        // (decisions/020). The reference plugin is one.

        *self.state.write().expect("state lock poisoned") = Some(Registration {
            instance_id: self.identity.instance_id.clone(),
            roles: self.identity.roles.clone(),
            grants: grants.clone(),
            healthy: true,
            last_heartbeat_ns: now_ns(),
            departed: false,
            interface_port,
            health_detail: String::new(),
            contract_version: req.schema_version.clone(),
        });
        self.changed.notify_one();

        tracing::info!(
            instance = self.identity.instance_id,
            roles = self.identity.roles.join(","),
            "admitted"
        );

        // Grants come back so a plugin can fail at startup rather than at its
        // first refused publish, which moves the failure to where an operator
        // is already looking.
        Ok(Response::new(RegisterReply {
            admitted: true,
            deployment_id: self.deployment_id.clone(),
            refusal_reason: String::new(),
            publish_grants: grants.publish,
            subscribe_grants: grants.subscribe,
            // Returned so a plugin can log what it is and stop when that is not
            // what it expected to be. Learning it is not declaring it.
            instance_id: self.identity.instance_id.clone(),
            roles: self.identity.roles.clone(),
            tags: self.identity.tags.clone(),
        }))
    }

    async fn publish(
        &self,
        request: Request<PublishRequest>,
    ) -> Result<Response<PublishReply>, Status> {
        let registration = self.admitted()?;
        let req = request.into_inner();

        if !registration.grants.may_publish(&req.topic) {
            let refusal_reason = format!("no publish grant for {}", req.topic);
            self.note_refusal(&refusal_reason);
            return Ok(Response::new(PublishReply {
                accepted: false,
                message_id: String::new(),
                refusal_reason,
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
            let refusal = format!("no subscribe grant for {pattern}");
            self.note_refusal(&refusal);
            return Err(Status::permission_denied(refusal));
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
            let refusal = format!("no grant for {}", req.topic);
            self.note_refusal(&refusal);
            return Ok(Response::new(failed_call(CallFailure::Refused, refusal)));
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
            state.health_detail = if req.healthy {
                String::new()
            } else {
                req.detail.clone()
            };
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
        self.changed.notify_one();

        tracing::info!(instance = registration.instance_id, reason, "plugin left");
        Ok(Response::new(LeaveReply {}))
    }

    type WatchSettingsStream = SettingsStream;

    async fn watch_settings(
        &self,
        _request: Request<WatchSettingsRequest>,
    ) -> Result<Response<Self::WatchSettingsStream>, Status> {
        Err(not_until_v2("WatchSettings"))
    }

    async fn plugin_access(
        &self,
        _request: Request<PluginAccessRequest>,
    ) -> Result<Response<PluginAccessReply>, Status> {
        Err(not_until_v2("PluginAccess"))
    }

    type WatchAccountScopeStream = ScopeStream;

    async fn watch_account_scope(
        &self,
        _request: Request<WatchAccountScopeRequest>,
    ) -> Result<Response<Self::WatchAccountScopeStream>, Status> {
        Err(not_until_v2("WatchAccountScope"))
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

    /// A contract of the shape the real one has, with a read-only role the
    /// real one has no rows for yet.
    fn contract() -> Contract {
        Contract::parse(
            "topic\tkind\tpublisher\tsubscriber\n\
             platform.street.command.record-holding\tcommand\tcustody\tstreet\n\
             platform.reference.event.instrument-missing\tevent\tcustody\tinstrument\n\
             platform.reference.query.resolve-identifier\tquery\tcustody\tinstrument\n\
             platform.custody.*.event.sync-status\tevent\tcustody\tdashboard\n\
             platform.reference.event.instrument-applied\tevent\tinstrument\tcustody\n\
             platform.street.event.*\tevent\tstreet\treporting\n",
            "name\tkind\ncustody\trole\nreporting\trole\noms\trole\nstreet\tcomponent\n",
        )
        .unwrap()
    }

    fn roles(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    fn sidecar() -> Sidecar {
        launched_as(Identity::new("custody-snaptrade-1", roles(&["custody"])))
    }

    /// A sidecar deployed to serve one particular plugin.
    fn launched_as(identity: Identity) -> Sidecar {
        let bus = Arc::new(Bus::single(
            "sidecar-custody-1",
            Arc::new(MemoryBackend::new()),
        ));
        Sidecar::under(&contract(), bus, "dep-local-1", identity)
    }

    fn register_req() -> RegisterRequest {
        RegisterRequest {
            schema_version: "v1".into(),
            ..Default::default()
        }
    }

    async fn admitted_sidecar() -> Sidecar {
        let sc = sidecar();
        let reply = sc
            .register(Request::new(register_req()))
            .await
            .unwrap()
            .into_inner();
        assert!(reply.admitted);
        sc
    }

    #[tokio::test]
    async fn a_sidecar_launched_with_a_name_that_is_not_a_role_admits_nobody() {
        // Refused, not admitted with nothing: a misspelt role would otherwise
        // be a plugin that registers and then fails at every publish.
        for launched in [roles(&["custdy"]), roles(&["custody", "street"])] {
            let sc = launched_as(Identity::new("mystery-1", launched.clone()));
            let reply = sc
                .register(Request::new(register_req()))
                .await
                .unwrap()
                .into_inner();
            assert!(!reply.admitted, "{launched:?} was admitted");
            assert!(reply.refusal_reason.contains("launched with"));
            assert!(sc.registration().is_none());
        }
    }

    #[tokio::test]
    async fn a_plugin_holding_no_role_is_admitted_with_no_topics() {
        // The reference plugin: registered, and refused everything it asks.
        for held in [roles(&[]), roles(&["oms"])] {
            let sc = launched_as(Identity::new("reference-1", held.clone()));
            let reply = sc
                .register(Request::new(register_req()))
                .await
                .unwrap()
                .into_inner();
            assert!(reply.admitted, "{held:?}: {}", reply.refusal_reason);
            assert!(reply.publish_grants.is_empty() && reply.subscribe_grants.is_empty());
            let refused = sc
                .publish(Request::new(PublishRequest {
                    topic: "platform.street.command.record-holding".into(),
                    ..Default::default()
                }))
                .await
                .unwrap()
                .into_inner();
            assert!(!refused.accepted);
        }
    }

    #[tokio::test]
    async fn admission_is_refused_for_a_contract_outside_the_range() {
        for declared in ["v0", "v2"] {
            let sc = sidecar();
            let mut req = register_req();
            req.schema_version = declared.into();

            let reply = sc.register(Request::new(req)).await.unwrap().into_inner();
            assert!(!reply.admitted, "{declared} was admitted");
            // Both halves: what was declared, and what would be accepted.
            assert!(reply.refusal_reason.contains(declared));
            assert!(reply.refusal_reason.contains("v1 through v1"));
            assert!(sc.registration().is_none());
        }
    }

    #[tokio::test]
    async fn a_plugin_that_declares_no_contract_is_no_longer_admitted() {
        // It was, until 2026-09-21, which made omitting the version the safest
        // thing a vendor could do.
        let sc = sidecar();
        let mut req = register_req();
        req.schema_version = String::new();

        let reply = sc.register(Request::new(req)).await.unwrap().into_inner();
        assert!(!reply.admitted);
        assert!(reply
            .refusal_reason
            .contains("declared no contract version"));
    }

    #[tokio::test]
    async fn admission_returns_the_grants_so_a_plugin_can_fail_at_startup() {
        let sc = sidecar();
        let reply = sc
            .register(Request::new(register_req()))
            .await
            .unwrap()
            .into_inner();

        assert!(reply.admitted);
        assert_eq!(reply.deployment_id, "dep-local-1");
        assert!(reply
            .publish_grants
            .contains(&"platform.street.command.record-holding".to_string()));
        assert_eq!(
            reply.subscribe_grants,
            vec!["platform.reference.event.instrument-applied".to_string()]
        );
    }

    #[tokio::test]
    async fn a_plugin_cannot_ask_to_be_a_role_it_was_not_launched_as() {
        // The hole this closed: the plugin used to supply role and tags, so a
        // read-only plugin could ask to be a connector and be admitted with
        // write grants. There is now nothing in the request that could ask.
        let sc = launched_as(Identity::new("reporting-1", roles(&["reporting"])));

        let reply = sc
            .register(Request::new(register_req()))
            .await
            .unwrap()
            .into_inner();

        assert!(reply.admitted);
        assert_eq!(reply.roles, vec!["reporting".to_string()]);
        assert_eq!(reply.instance_id, "reporting-1");
        assert!(reply.publish_grants.is_empty());
        assert!(!sc
            .registration()
            .unwrap()
            .grants
            .may_publish("platform.street.command.record-holding"));
    }

    #[tokio::test]
    async fn the_reply_tells_a_plugin_what_it_was_launched_as() {
        // So a plugin can stop at startup when it is not what it expected to
        // be, rather than running as something else and finding out by refusal.
        let sc = launched_as(
            Identity::new("custody-snaptrade-1", roles(&["custody", "reporting"]))
                .with_tags(vec!["holdings".to_string()]),
        );

        let reply = sc
            .register(Request::new(register_req()))
            .await
            .unwrap()
            .into_inner();

        assert!(reply.admitted);
        assert_eq!(reply.instance_id, "custody-snaptrade-1");
        assert_eq!(reply.roles, roles(&["custody", "reporting"]));
        assert_eq!(reply.tags, vec!["holdings".to_string()]);
        // Both roles' grants, the union the plugin will actually be held to.
        assert!(reply
            .publish_grants
            .contains(&"platform.street.command.record-holding".to_string()));
        assert!(reply
            .subscribe_grants
            .contains(&"platform.street.event.*".to_string()));
    }

    #[tokio::test]
    async fn a_tag_grants_nothing_on_the_bus() {
        // decisions/020: tags divide a plugin among people. A tag named like a
        // role adds none of that role's topics.
        let sc = launched_as(
            Identity::new("custody-snaptrade-1", roles(&["custody"]))
                .with_tags(vec!["reporting".to_string()]),
        );
        let reply = sc
            .register(Request::new(register_req()))
            .await
            .unwrap()
            .into_inner();
        assert!(reply.admitted);
        assert!(!reply
            .subscribe_grants
            .contains(&"platform.street.event.*".to_string()));
    }

    #[tokio::test]
    async fn the_same_plugin_may_register_again_after_a_restart() {
        let sc = admitted_sidecar().await;

        // A plugin that restarted reconnects. There is nothing for it to
        // reassert, so this is idempotent by construction.
        let reply = sc
            .register(Request::new(register_req()))
            .await
            .unwrap()
            .into_inner();
        assert!(reply.admitted);
        assert_eq!(reply.instance_id, "custody-snaptrade-1");
    }

    #[tokio::test]
    async fn a_sidecar_is_free_again_once_its_plugin_leaves() {
        let sc = admitted_sidecar().await;
        sc.leave(Request::new(LeaveRequest::default()))
            .await
            .unwrap();

        let reply = sc
            .register(Request::new(register_req()))
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
        let sc = sidecar();
        let err = sc
            .publish(Request::new(PublishRequest {
                topic: "platform.street.command.record-holding".into(),
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
                topic: "platform.street.command.record-statement".into(),
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
        let mut sub = sc.bus.subscribe("platform.street.**");

        let reply = sc
            .publish(Request::new(PublishRequest {
                topic: "platform.street.command.record-holding".into(),
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
                pattern: "platform.street.**".into(),
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
                ..Default::default()
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
                topic: "platform.street.query.list-positions".into(),
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
                topic: "platform.street.command.record-holding".into(),
                ..Default::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }
}
