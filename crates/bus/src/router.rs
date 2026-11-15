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
#[derive(Debug, Clone, Default, PartialEq)]
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

    /// The prepared change the person made the command from (W4.9, W6.27,
    /// contract v20): its `PC-` ID as the assertion named it, so the store
    /// records what prepared the change. Empty otherwise, and always beside
    /// a person.
    pub prepared_call_id: String,

    /// The kind of user `acting_for_subject` names (W4.9, contract v21): the
    /// claims' `subject_kind`, `SUBJECT_KIND_SERVICE_ACCOUNT` for a service
    /// account, unspecified (0) for a person and for nobody.
    pub acting_for_kind: i32,

    /// The person answerable for the service account a command was sent
    /// for, beside `acting_for_kind` (W4.9, W9.8, contract v21): the claims'
    /// `owner_subject`. Empty for a person, and for nobody.
    pub owner_subject: String,

    /// The algorithm the service account's call named, beside
    /// `owner_subject` (W4.9, contract v22): the claims' `algorithm_id`, after
    /// the sidecar checked it against the algorithms the plugin's version
    /// declares. Empty for a person, for nobody, and where the call named
    /// none.
    pub algorithm_id: String,

    /// On a placement (contract v22, W11.8): whether the connectivity plugin
    /// it names is among those the claims say the person or service account
    /// may route through from the sending plugin (the claims'
    /// connectivity_instance_ids), which the book refuses and records
    /// REFUSAL_REASON_CONNECTIVITY_NOT_ALLOWED by when false. False
    /// everywhere else.
    pub connectivity_allowed: bool,

    /// On a placement, and on an accept naming a new order (contract v26,
    /// W11.8, W11.23): whether the connectivity plugin it names, or the
    /// reported fills', is among the claims' demo_connectivity_instance_ids
    /// -- its environment setting and its session's report both demo -- which
    /// the book refuses an agent's act by while the deployment's
    /// agent_orders_demo_only is on. False everywhere else.
    pub connectivity_demo: bool,

    /// The accounts the plugin may read, stamped with the mark that they
    /// apply (W4.11): `Some`, an empty one included, is a plugin's read, which
    /// a store answers only within, and an empty one with nothing. `None` is
    /// a core component reading as itself. From contract v22 also a command
    /// naming a record by its ID alone: the accounts the sending instance may
    /// write, narrowed to the person's write access where it acts for one
    /// (W4.9), which the store holding the record checks its account against.
    pub account_scope: Option<Vec<String>>,

    /// The client instance the analytics component reads for (contract
    /// v26, W12.2): set by that component alone, on its reads for a job,
    /// beside the account scope the client's sidecar stamped on the job. A
    /// store reads it only from the analytics component; empty everywhere
    /// else.
    pub on_behalf_of_instance_id: String,

    /// The watermark a read must not be answered behind (contract v30, W4.4):
    /// stamped by the sidecar's session on its plugin's reads (see
    /// [`crate::read_after`]), never by a plugin. `None` reads from the
    /// nearest member, eventually consistent.
    pub read_after: Option<meridian_pb::v1::Watermark>,
}

/// Who a request is asked by, for its component's fair shares (the
/// resilience spec's section 10a): its calling instance -- a plugin's, by
/// its sidecar -- and, where the call names one, the delegation or else the
/// person it is asked for, and the client instance the analytics component
/// reads for. A browser's calls are keyed by the person; a delegation's by
/// the delegation.
pub fn caller_of(meta: Option<&MessageMeta>) -> String {
    let Some(meta) = meta else {
        return String::new();
    };
    let mut caller = meta.publisher_instance_id.clone();
    let narrower = [
        ("d", &meta.acting_through_delegation),
        ("p", &meta.acting_for_subject),
        ("i", &meta.on_behalf_of_instance_id),
    ]
    .into_iter()
    .find(|(_, said)| !said.is_empty());
    if let Some((kind, said)) = narrower {
        caller.push('|');
        caller.push_str(kind);
        caller.push(':');
        caller.push_str(said);
    }
    caller
}

/// Requests dropped because their caller's deadline passed while they
/// waited for their turn, since the process started: counted apart from
/// the refusals for load (the research batch's item 5).
static EXPIRED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// [`EXPIRED`]'s count.
pub fn expired_requests() -> u64 {
    EXPIRED.load(std::sync::atomic::Ordering::Relaxed)
}

/// A request dropped, its caller gone: counted and said, and answered to
/// nobody waiting.
fn expired(topic: &str, caller: &str) -> String {
    EXPIRED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    tracing::info!(
        topic,
        caller,
        "a request dropped: its caller's deadline passed while it waited for its turn"
    );
    crate::backend::refusal_with_status(
        crate::backend::RefusalStatus::Unavailable,
        &[],
        format!("{topic} was dropped: its caller's deadline passed while it waited for its turn"),
    )
}

/// Admit a request on `topic` at this process's gate and budget, and run
/// `handler` on the blocking pool within its reservation; refused for load,
/// with its retry-after, where it waited its while (section 10a).
pub(crate) async fn admitted(
    topic: &str,
    envelope: Envelope,
    handler: Handler,
    deadline: Option<std::time::Instant>,
) -> std::result::Result<HandlerReply, tokio::task::JoinError> {
    let class = meridian_budget::Class::of_topic(topic);
    let caller = caller_of(envelope.meta.as_ref());
    let bytes = envelope.payload.len() as u64;
    let what = format!(
        "{topic} for {}",
        if caller.is_empty() { "itself" } else { &caller }
    );
    let admit = meridian_budget::process().admit(class, &caller, bytes, &what);
    // A request whose caller has stopped waiting is dropped, never run
    // (the research batch's item 5, approved 2026-11-10): while it waits
    // for its turn, or as its turn comes.
    let admitted = match deadline {
        Some(deadline) => match tokio::time::timeout_at(deadline.into(), admit).await {
            Ok(admitted) => admitted,
            Err(_) => return Ok(Err(expired(topic, &caller))),
        },
        None => admit.await,
    };
    match admitted {
        Ok(_) if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) => {
            Ok(Err(expired(topic, &caller)))
        }
        Err(over) => {
            tracing::info!(
                topic,
                caller,
                retry_after_seconds = over.retry_after_seconds,
                "{}",
                over.words
            );
            Ok(Err(crate::backend::overloaded(&over)))
        }
        Ok(admitted) => {
            let answered = tokio::task::spawn_blocking(move || {
                admitted
                    .within(|| crate::read_after::within_deadline(deadline, || handler(envelope)))
            })
            .await;
            Ok(cancelled_for_load(topic, answered?))
        }
    }
}

/// What a store's error carries in its words when the database cancelled
/// its query at the serving session's `statement_timeout` (SQLSTATE 57014;
/// meridian_changes::serving).
pub const CANCELLED_QUERY: &str = "[sqlstate 57014]";

/// How long a request whose query was cancelled is told to wait.
const CANCELLED_RETRY_AFTER_SECONDS: u32 = 5;

/// An answer whose query the database cancelled at its statement_timeout:
/// this component's own refusal for load, with a retry-after (the
/// resilience spec's section 10a), never the database's failure.
pub(crate) fn cancelled_for_load(topic: &str, answered: HandlerReply) -> HandlerReply {
    match answered {
        Err(said) if said.contains(CANCELLED_QUERY) => {
            tracing::info!(
                topic,
                "a query past its statement_timeout was cancelled: {said}"
            );
            meridian_budget::count_refusal();
            Err(crate::backend::overloaded(&meridian_budget::Overloaded {
                retry_after_seconds: CANCELLED_RETRY_AFTER_SECONDS,
                words: format!(
                    "overloaded: {topic}'s query ran past its statement_timeout and was cancelled"
                ),
            }))
        }
        answered => answered,
    }
}

/// The bus.
///
/// Owns routing, identity stamping and request-reply. Backends own transport
/// and nothing else, so a second backend inherits all of this.
pub struct Bus {
    backends: HashMap<String, Arc<dyn Backend>>,
    rules: Vec<RouteRule>,
    default_backend: String,
    handlers: Arc<RwLock<HashMap<String, Handler>>>,
    instance_id: String,
    default_timeout: Duration,

    /// The deployment's clock, which stamps every message's time. Given, not
    /// read from the wall: a bus that read its own would stamp a replay with
    /// the time it was replayed.
    clock: Arc<dyn Clock>,

    /// Told of every call this bus asks, its topic and how long it took
    /// (ruling 798): a sidecar's front door names the calls its plugin made
    /// while a page was being answered.
    observer: RwLock<Option<CallObserver>>,

    /// `read_after` (contract v30, stage B): the positions of the stores this
    /// process serves the reads of, and a sidecar's session.
    reads: Arc<crate::read_after::Reads>,

    /// A reader member's serve mode (contract v30, stage B; the resilience
    /// spec's section 1): the topics it serves, every other declined. Unset,
    /// as every writer and every other component, everything is served.
    serves_only: RwLock<Option<Serves>>,

    /// Where this member hands a write its writer group does not own (step
    /// 3d; the resilience spec's section 6.7): unset, as with one group,
    /// nothing is handed on.
    routing: Arc<RwLock<Option<Routing>>>,
}

/// A member's hand-off of writes to their owners.
#[derive(Clone)]
struct Routing {
    /// The store's internal topic a write is handed on.
    internal: String,
    /// Which of its topics are writes.
    writes: Serves,
    /// The group a write is its owner's by, read from the request before
    /// its handler runs any of its work: none where the request does not say
    /// cheaply, and its handler finds out (step 3d; the ownership pre-check
    /// decided 2026-11-13).
    owner_of: Option<OwnerOf>,
    /// This member's writer group.
    group: u32,
    backend: Arc<dyn Backend>,
    timeout: Duration,
}

/// What a write's owning group is, read from its request (step 3d).
pub type OwnerOf = Arc<dyn Fn(&str, &Envelope) -> Option<u32> + Send + Sync>;

/// What a write handed to its owner leaves of its caller's deadline for its
/// answer's way back through this member.
const HANDED_BACK: Duration = Duration::from_millis(250);

/// Which topics a reader member serves.
type Serves = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// What [`Bus::observe_calls`] is told of each call: its topic, when it was
/// asked and how long its answer took. Never its payload.
pub type CallObserver = Arc<dyn Fn(&str, std::time::Instant, Duration) + Send + Sync>;

/// One call a request made, as its timing line names it (ruling 798).
#[derive(Debug, Clone, PartialEq)]
pub struct TimedCall {
    pub topic: String,
    pub took: Duration,
}

tokio::task_local! {
    static TIMED: Arc<std::sync::Mutex<Vec<TimedCall>>>;
}

/// Run `work`, and say every bus call it asked while it ran on this task
/// (ruling 798): a dashboard request's timing line. A call asked from a task
/// it spawned is not this one's.
pub async fn timed<F: std::future::Future>(work: F) -> (F::Output, Vec<TimedCall>) {
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let output = TIMED.scope(Arc::clone(&calls), work).await;
    let calls = std::mem::take(&mut *calls.lock().expect("timed calls poisoned"));
    (output, calls)
}

/// The calls as one field of a line: `topic:ms` each, in the order asked,
/// at most `most` of them and how many more.
pub fn calls_said(calls: &[TimedCall], most: usize) -> String {
    let mut said: Vec<String> = calls
        .iter()
        .take(most)
        .map(|call| format!("{}:{:.0}", call.topic, call.took.as_secs_f64() * 1e3))
        .collect();
    if calls.len() > most {
        said.push(format!("+{} more", calls.len() - most));
    }
    said.join(",")
}

/// The calls as one field of a line, totalled per topic (ruling 816): each
/// topic asked, in the order it was first asked, as `topic:count:ms`, its
/// calls' count and their time together. Every call is counted, however
/// many: a request that paged a read twenty times says so in one entry.
pub fn calls_by_topic(calls: &[TimedCall]) -> String {
    let mut totals: Vec<(&str, usize, Duration)> = Vec::new();
    for call in calls {
        match totals.iter_mut().find(|(topic, _, _)| *topic == call.topic) {
            Some((_, count, took)) => {
                *count += 1;
                *took += call.took;
            }
            None => totals.push((&call.topic, 1, call.took)),
        }
    }
    totals
        .into_iter()
        .map(|(topic, count, took)| format!("{topic}:{count}:{:.0}", took.as_secs_f64() * 1e3))
        .collect::<Vec<_>>()
        .join(",")
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
            handlers: Arc::default(),
            instance_id: instance_id.into(),
            default_timeout: Duration::from_secs(5),
            clock,
            observer: RwLock::new(None),
            reads: Arc::default(),
            serves_only: RwLock::new(None),
            routing: Arc::default(),
        }
    }

    /// Route the topics `rules` match to the backends they name, beside the
    /// rules already given: each transport adds its own routes -- the lake's
    /// write-ahead stream's in `ha`, a second data-plane transport's later --
    /// and none takes another's away. The first rule matching a topic wins.
    pub fn with_rules(mut self, rules: Vec<RouteRule>) -> Self {
        self.rules.extend(rules);
        self
    }

    /// A second backend, by name, for the rules that name it: a transport
    /// beside the default one, as the lake's write-ahead stream is in `ha`
    /// ([`crate::write_ahead`]). Everything above the backends -- stamping,
    /// `read_after`, the gate -- is the bus's, whichever carries a message.
    pub fn with_backend(mut self, name: impl Into<String>, backend: Arc<dyn Backend>) -> Self {
        self.backends.insert(name.into(), backend);
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

    /// Subscribe, every subscriber hearing every message: what a copy of a
    /// component needs for itself -- a view it keeps, a cache, a plugin's
    /// delivery -- whatever its other copies do. A caller says which it
    /// means (contract v25): this, or [`Bus::subscribe_shared`].
    pub fn subscribe_every(&self, pattern: &str) -> Subscription {
        self.backend_for(pattern).subscribe(pattern)
    }

    /// Subscribe as one of us (contract v25; Q15): each message reaches one
    /// copy of this component among those subscribed for the same `work`, so
    /// work that writes a shared store or asks the platform is done once, not
    /// once per copy. The group is this bus's instance (its component) and
    /// `work`, so two subscriptions of one process to one topic for
    /// different work each hear it. At most once.
    pub fn subscribe_shared(&self, pattern: &str, work: &str) -> Subscription {
        // A reader member does none of its component's shared work: each
        // message would otherwise reach it rather than the writer, which
        // does that work (contract v30, stage B). Its subscription ends at
        // once.
        if self.serves_only.read().expect("serves poisoned").is_some() {
            let (_, rx) = tokio::sync::mpsc::channel(1);
            return Subscription::new(rx, pattern, Default::default());
        }
        // A store of several writer groups (step 3d): each group shares the
        // work among its own members, and does it for its own partitions.
        let group = match self.routing.read().expect("routing poisoned").as_ref() {
            Some(route) => format!("{}.{work}.group-{}", self.instance_id, route.group),
            None => format!("{}.{work}", self.instance_id),
        };
        self.backend_for(pattern).subscribe_shared(pattern, &group)
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
        if self.declines(topic) {
            return;
        }
        let handler = self.held_to_read_after(topic, Arc::new(handler));
        let handler = self.routed(topic, handler);
        self.handlers
            .write()
            .expect("handler lock poisoned")
            .insert(topic.to_string(), Arc::clone(&handler));

        // And offered to other processes, where the backend has somewhere to
        // offer it, every copy of this component one group (contract v25),
        // so a request is answered once. The in-process backend does nothing
        // here, because the map above is already the answer.
        // A write another writer group owns, as the request says, passed to
        // its owner before it is admitted here (step 3d): never a slot of
        // this member's held while the owner answers, nor admitted twice.
        let routing = Arc::clone(&self.routing);
        let asked = topic.to_string();
        let route: crate::backend::PreRoute = Arc::new(move |envelope: &Envelope| {
            let route = routing.read().expect("routing poisoned").clone()?;
            if !(route.writes)(&asked) {
                return None;
            }
            let group = (route.owner_of.as_ref()?)(&asked, envelope)?;
            (group != route.group).then(|| (route.internal.clone(), group))
        });
        self.backend_for(topic)
            .serve_routed(topic, &self.instance_id, handler, route);
    }

    /// This process's part in `read_after` (contract v30, stage B): a store's
    /// member keeps its position here, a sidecar its session.
    pub fn reads_after(&self) -> &Arc<crate::read_after::Reads> {
        &self.reads
    }

    /// Hand each write this member's writer group `group` does not own to
    /// its owner (step 3d; the resilience spec's section 6.7): every topic
    /// `writes` says is a write is answered as ever, but where its store
    /// says, by [`crate::owners::hand_to`], that the write is another
    /// group's, the request is handed on `internal`, unchanged and
    /// addressed to that group, and answered with the owner's reply. And
    /// `internal` offered to this group alone: a write handed here is
    /// answered by its public topic's own handler, never handed on again.
    /// With one writer group nothing calls this, and nothing is handed on.
    pub fn hand_writes_on(
        &self,
        internal: &str,
        group: u32,
        writes: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) {
        let writes: Serves = Arc::new(writes);
        let owner_of = self
            .routing
            .read()
            .expect("routing poisoned")
            .as_ref()
            .and_then(|routing| routing.owner_of.clone());
        *self.routing.write().expect("routing poisoned") = Some(Routing {
            internal: internal.to_string(),
            writes: Arc::clone(&writes),
            owner_of,
            group,
            backend: Arc::clone(self.backend_for(internal)),
            timeout: self.default_timeout,
        });
        let handlers = Arc::clone(&self.handlers);
        let handed_on: Handler = Arc::new(move |envelope: Envelope| {
            let topic = envelope
                .meta
                .as_ref()
                .map(|meta| meta.topic.clone())
                .unwrap_or_default();
            if !writes(&topic) {
                return Err(format!(
                    "{topic:?} is no write of this store: only a write is handed on"
                ));
            }
            let handler = handlers
                .read()
                .expect("handler lock poisoned")
                .get(&topic)
                .cloned()
                .ok_or_else(|| format!("{topic:?} is not served here"))?;
            crate::owners::arriving();
            handler(envelope)
        });
        self.backend_for(internal).serve_group(
            internal,
            &format!("{}.group-{group}", self.instance_id),
            group,
            handed_on,
        );
    }

    /// `envelope`, a write of `topic`, handed to writer group `group` on the
    /// internal topic [`Bus::hand_writes_on`] named, and answered with the
    /// owner's reply (step 3d): what a member does with each other group's
    /// part of a write spanning several. Called from a handler, off the
    /// runtime's own threads. Refused where no hand-off is configured.
    pub fn hand_to_owner(&self, group: u32, topic: &str, envelope: Envelope) -> HandlerReply {
        let route = self.routing.read().expect("routing poisoned").clone();
        match route {
            Some(route) => hand(&route, group, topic, envelope),
            None => Err(format!(
                "{topic} is writer group {group}'s to write, and this member hands nothing on"
            )),
        }
    }

    /// Decide a write's owning group from its request before its handler
    /// runs (step 3d; section 6.7; the ownership pre-check decided
    /// 2026-11-13): `owner_of` answers the group where the request says it
    /// cheaply -- the book's account's placement, the lake's datasets' -- and
    /// a write of another group's is handed to it at once, its handler
    /// never run here. Where it answers none, the handler finds out as
    /// before. Set after [`Bus::hand_writes_on`].
    pub fn route_writes_by(
        &self,
        owner_of: impl Fn(&str, &Envelope) -> Option<u32> + Send + Sync + 'static,
    ) {
        if let Some(routing) = self.routing.write().expect("routing poisoned").as_mut() {
            routing.owner_of = Some(Arc::new(owner_of));
        }
    }

    /// A write's handler, handed to its owner where its store says so.
    fn routed(&self, topic: &str, handler: Handler) -> Handler {
        let routing = Arc::clone(&self.routing);
        let topic = topic.to_string();
        Arc::new(move |envelope: Envelope| {
            let route = routing
                .read()
                .expect("routing poisoned")
                .clone()
                .filter(|route| (route.writes)(&topic));
            let Some(route) = route else {
                return handler(envelope);
            };
            let handed = crate::owners::take_arriving();
            // The owner decided from the request first: another group's
            // write handed on before any of its handler's work.
            if !handed {
                if let Some(owner_of) = route.owner_of.as_ref() {
                    if let Some(group) = owner_of(&topic, &envelope) {
                        if group != route.group {
                            return hand(&route, group, &topic, envelope);
                        }
                    }
                }
            }
            crate::owners::begin(handed);
            let kept = envelope.clone();
            let answered = handler(envelope);
            let named = crate::owners::take_named();
            match crate::owners::take_hand_to() {
                None => answered,
                Some(_) if handed && !named => Err(crate::refusal_with_status(
                    crate::RefusalStatus::Unavailable,
                    &[],
                    crate::owners::NOT_THIS_GROUPS,
                )),
                Some(group) => hand(&route, group, &topic, kept),
            }
        })
    }

    /// Serve only the topics `serves` says, from now on: a store's reader
    /// member, the same image in a reads-only mode the chart sets (contract
    /// v30, stage B; the resilience spec's section 1). Every other topic its
    /// component would serve is left to the writer -- a member not in a
    /// topic's queue group is never asked it -- and its shared work too.
    pub fn serve_only(&self, serves: impl Fn(&str) -> bool + Send + Sync + 'static) {
        *self.serves_only.write().expect("serves poisoned") = Some(Arc::new(serves));
    }

    /// Whether this process is a reader member, serving reads alone.
    pub fn reads_only(&self) -> bool {
        self.serves_only.read().expect("serves poisoned").is_some()
    }

    fn declines(&self, topic: &str) -> bool {
        let serves = self.serves_only.read().expect("serves poisoned");
        match serves.as_ref() {
            Some(serves) if !serves(topic) => {
                tracing::debug!(topic, "a reader: left to the writer");
                true
            }
            _ => false,
        }
    }

    /// A store's read held to its `read_after` (W4.4): never answered from
    /// behind the watermark it names, whichever member answers it. Every
    /// other topic's handler as it was.
    fn held_to_read_after(&self, topic: &str, handler: Handler) -> Handler {
        if !crate::read_after::is_read(topic) {
            return handler;
        }
        let reads = Arc::clone(&self.reads);
        let topic = topic.to_string();
        Arc::new(move |envelope: Envelope| {
            crate::read_after::begin_read();
            match envelope.meta.as_ref().and_then(|m| m.read_after.clone()) {
                Some(asked) => reads.answer(&topic, &asked, || handler(envelope)),
                None => handler(envelope),
            }
        })
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
        if self.declines(topic) {
            return;
        }
        let handler = self.held_to_read_after(topic, Arc::new(handler));
        self.handlers
            .write()
            .expect("handler lock poisoned")
            .insert(topic.to_string(), Arc::clone(&handler));

        self.backend_for(topic)
            .serve_delivered(topic, &self.instance_id, handler, delivered);
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
        let started = std::time::Instant::now();
        // A sidecar's session (contract v30, stage B): each read of a store
        // names the highest it has seen of that store for its instance, and
        // each answer is seen.
        let session = self.reads.session();
        let stamped;
        let mut stamp = stamp;
        if let Some(session) = &session {
            if stamp.read_after.is_none() && crate::read_after::is_read(topic) {
                if let Some(seen) = crate::read_after::domain_of(topic)
                    .and_then(|domain| session.read_after(domain))
                {
                    stamped = Stamp {
                        read_after: Some(seen),
                        ..stamp.clone()
                    };
                    stamp = &stamped;
                }
            }
        }
        let answered = self
            .asked(topic, payload_type, payload, correlation_id, timeout, stamp)
            .await;
        if let (Some(session), Ok((reply_type, reply))) = (&session, &answered) {
            session.answered(topic, reply_type, reply);
        }
        let took = started.elapsed();
        let _ = TIMED.try_with(|calls| {
            calls.lock().expect("timed calls poisoned").push(TimedCall {
                topic: topic.to_string(),
                took,
            })
        });
        let observer = self.observer.read().expect("observer poisoned").clone();
        if let Some(observer) = observer {
            observer(topic, started, took);
        }
        answered
    }

    /// Hand `envelope` on, unchanged, to whoever serves `topic` in another
    /// process, and answer with their answer: one member of a component
    /// handing a request to the member that must answer it (topics.md,
    /// "Component topics": a lake reader's read that depends on the writer's
    /// own acts, contract v30 stage B). Nothing is stamped: who asked, their
    /// scope and their `read_after` stay the original caller's, and its
    /// topic the public one it was asked on. Never answered in this process,
    /// which is the one handing it on.
    pub async fn hand_on(
        &self,
        topic: &str,
        envelope: Envelope,
        timeout: Option<Duration>,
    ) -> crate::Result<(String, Vec<u8>)> {
        let timeout = timeout.unwrap_or(self.default_timeout);
        let answered = self
            .backend_for(topic)
            .request(topic, envelope, timeout)
            .await?;
        Ok((answered.payload_type, answered.payload))
    }

    /// Tell `observer` of every call this bus asks from now on (ruling 798).
    pub fn observe_calls(&self, observer: CallObserver) {
        *self.observer.write().expect("observer poisoned") = Some(observer);
    }

    async fn asked(
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
        // not be polled. A process asking itself is not admitted at its own
        // gate: its callers are other processes, which the broker's path
        // admits (section 10a), and a question it asks itself while
        // answering one would otherwise wait on its own turn.
        // Its deadline known to it, so a read held to its read_after waits no
        // longer than its caller does (ruled 2026-11-10).
        let deadline = std::time::Instant::now() + timeout;
        let asked_topic = topic_owned.clone();
        let joined = tokio::time::timeout(
            timeout,
            tokio::task::spawn_blocking(move || {
                // A query cancelled at its statement_timeout is this
                // process's own refusal for load, as on the broker's path.
                cancelled_for_load(
                    &asked_topic,
                    crate::read_after::within_deadline(Some(deadline), || handler(envelope)),
                )
            }),
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
            prepared_call_id: stamp.prepared_call_id.clone(),
            acting_for_kind: stamp.acting_for_kind,
            owner_subject: stamp.owner_subject.clone(),
            algorithm_id: stamp.algorithm_id.clone(),
            connectivity_allowed: stamp.connectivity_allowed,
            connectivity_demo: stamp.connectivity_demo,
            // The analytics component's own reads (W12.2), never a sidecar's.
            on_behalf_of_instance_id: stamp.on_behalf_of_instance_id.clone(),
            // A read's scope is the sidecar's to stamp, for a plugin, marked
            // as applying; a core component reads as itself, unmarked.
            account_scope: stamp.account_scope.clone().unwrap_or_default(),
            account_scope_applies: stamp.account_scope.is_some(),
            // Contract v30's read_after (W4.4): a sidecar's session stamps
            // its plugin's reads (stage B); unset reads from the nearest member.
            read_after: stamp.read_after.clone(),
        }
    }
}

#[cfg(test)]
mod expired_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// The research batch's item 5 (approved 2026-11-10): a request whose
    /// caller's deadline has passed by its turn is dropped, never run, and
    /// counted apart from the refusals for load.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_request_whose_caller_stopped_waiting_is_dropped_and_never_run() {
        let ran = Arc::new(AtomicBool::new(false));
        let running = Arc::clone(&ran);
        let handler: Handler = Arc::new(move |_| {
            running.store(true, Ordering::SeqCst);
            Ok((String::new(), Vec::new()))
        });
        let before = expired_requests();
        let refused_before = meridian_budget::process().accounting().refused;
        let gone = std::time::Instant::now() - Duration::from_millis(1);
        let answered = admitted(
            "platform.lake.command.record-prices",
            Envelope::default(),
            handler,
            Some(gone),
        )
        .await
        .unwrap();
        assert!(answered.unwrap_err().contains("dropped"));
        assert!(!ran.load(Ordering::SeqCst), "never run");
        assert_eq!(expired_requests(), before + 1);
        assert_eq!(
            meridian_budget::process().accounting().refused,
            refused_before
        );
    }
}

#[cfg(test)]
mod reads_only_tests {
    use super::*;
    use crate::MemoryBackend;
    use meridian_clock::ManualClock;

    #[tokio::test]
    async fn a_reader_serves_only_its_reads_and_does_no_shared_work() {
        let bus = Bus::single(
            "lake-1",
            Arc::new(MemoryBackend::new()),
            Arc::new(ManualClock::at(1_790_553_600_000_000_000)),
        );
        bus.serve_only(crate::read_after::is_read);
        assert!(bus.reads_only());
        bus.serve("platform.lake.query.list-quotes", |_| {
            Ok(("t".into(), Vec::new()))
        });
        bus.serve("platform.lake.command.record-prices", |_| {
            Ok(("t".into(), Vec::new()))
        });
        assert!(bus
            .call(
                "platform.lake.query.list-quotes",
                "t",
                Vec::new(),
                None,
                None
            )
            .await
            .is_ok());
        assert!(matches!(
            bus.call(
                "platform.lake.command.record-prices",
                "t",
                Vec::new(),
                None,
                None
            )
            .await,
            Err(BusError::NoHandler(_))
        ));
        let mut shared = bus.subscribe_shared("platform.reference.event.*", "work");
        assert!(shared.recv().await.is_none(), "ended at once");
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_query_cancelled_at_its_statement_timeout_is_refused_for_load_by_its_component() {
        let said = "the lake's store is unavailable: db error: ERROR: canceling statement due to statement timeout [sqlstate 57014]";
        let refused =
            super::cancelled_for_load("platform.lake.query.list-prices", Err(said.into()))
                .unwrap_err();
        assert_eq!(crate::backend::refusal_retry_after(&refused), Some(5));
        assert!(refused.contains("statement_timeout"), "{refused}");
        // Any other failure is answered as it was.
        let other = super::cancelled_for_load("t", Err("db error [sqlstate 23505]".into()));
        assert_eq!(other.unwrap_err(), "db error [sqlstate 23505]");
        assert!(super::cancelled_for_load("t", Ok(("k".into(), vec![1]))).is_ok());
    }
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
    async fn a_request_names_each_call_it_asked_and_an_observer_hears_them() {
        let bus = bus();
        bus.serve("platform.lake.query.list-quotes", |_| {
            Ok(("R".into(), vec![]))
        });
        let heard = Arc::new(std::sync::Mutex::new(Vec::new()));
        let hearing = Arc::clone(&heard);
        bus.observe_calls(Arc::new(move |topic, _, _| {
            hearing.lock().unwrap().push(topic.to_string())
        }));
        let ((), calls) = super::timed(async {
            for _ in 0..2 {
                bus.call("platform.lake.query.list-quotes", "Q", vec![], None, None)
                    .await
                    .unwrap();
            }
            // Nobody serves it: still a call, and still timed.
            let _ = bus
                .call("platform.lake.query.nobody", "Q", vec![], None, None)
                .await;
        })
        .await;
        assert_eq!(
            calls.iter().map(|c| c.topic.as_str()).collect::<Vec<_>>(),
            [
                "platform.lake.query.list-quotes",
                "platform.lake.query.list-quotes",
                "platform.lake.query.nobody"
            ]
        );
        assert_eq!(heard.lock().unwrap().len(), 3);
        // Outside a timed request, nothing is gathered, and nothing fails.
        bus.call("platform.lake.query.list-quotes", "Q", vec![], None, None)
            .await
            .unwrap();
        assert_eq!(heard.lock().unwrap().len(), 4);
        let said = super::calls_said(&calls, 2);
        assert!(
            said.starts_with("platform.lake.query.list-quotes:"),
            "{said}"
        );
        assert!(said.ends_with(",+1 more"), "{said}");
        // Per topic, every call counted, in the order first asked.
        let by_topic = super::calls_by_topic(&calls);
        assert!(
            by_topic.starts_with("platform.lake.query.list-quotes:2:"),
            "{by_topic}"
        );
        assert!(
            by_topic.contains(",platform.lake.query.nobody:1:"),
            "{by_topic}"
        );
    }

    #[test]
    fn calls_are_totalled_per_topic_however_many() {
        let calls: Vec<TimedCall> = (0..30)
            .map(|i| TimedCall {
                topic: if i % 10 == 9 {
                    "platform.lake.query.list-quotes".into()
                } else {
                    "platform.lake.query.list-series".into()
                },
                took: Duration::from_millis(10),
            })
            .collect();
        assert_eq!(
            super::calls_by_topic(&calls),
            "platform.lake.query.list-series:27:270,platform.lake.query.list-quotes:3:30"
        );
        assert_eq!(super::calls_by_topic(&[]), "");
    }

    #[tokio::test]
    async fn the_time_stamped_is_the_clocks_it_was_given() {
        let clock = Arc::new(ManualClock::at(NOW));
        let bus = Bus::single("core-1", Arc::new(MemoryBackend::new()), clock.clone());
        let mut sub = bus.subscribe_every("platform.street.**");

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
        let mut sub = bus.subscribe_every("platform.street.**");
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
        let mut sub = bus.subscribe_every("platform.street.**");
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
        let mut sub = bus.subscribe_every("platform.street.**");
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

    /// Two transports beside the default, each with its own routes, as a
    /// second data-plane transport would stand beside the write-ahead
    /// stream (contract v30, stage E): a topic goes to the backend its rule
    /// names, whichever was given first, and the rest to the default.
    #[tokio::test]
    async fn each_transport_adds_its_routes_and_none_takes_anothers_away() {
        let default_backend = Arc::new(MemoryBackend::new());
        let writes = Arc::new(MemoryBackend::new());
        let hot = Arc::new(MemoryBackend::new());
        let bus = Bus::single(
            "core-1",
            default_backend.clone(),
            Arc::new(ManualClock::at(NOW)),
        )
        .with_backend("writes", writes.clone())
        .with_rules(vec![RouteRule {
            pattern: "platform.lake.command.record-trades".into(),
            backend: "writes".into(),
        }])
        .with_backend("hot", hot.clone())
        .with_rules(vec![RouteRule {
            pattern: "platform.lake.*.event.trades-recorded".into(),
            backend: "hot".into(),
        }]);
        let mut on_writes = writes.subscribe("platform.lake.command.record-trades");
        let mut on_hot = hot.subscribe("platform.lake.*.event.trades-recorded");
        let mut on_default = default_backend.subscribe("platform.**");
        bus.publish(
            "platform.lake.command.record-trades",
            "t",
            vec![],
            None,
            None,
        )
        .unwrap();
        bus.publish(
            "platform.lake.v:live.event.trades-recorded",
            "t",
            vec![],
            None,
            None,
        )
        .unwrap();
        bus.publish(
            "platform.lake.v:live.event.prices-recorded",
            "t",
            vec![],
            None,
            None,
        )
        .unwrap();
        assert!(on_writes.recv().await.is_some());
        assert!(on_hot.recv().await.is_some());
        let heard = on_default.recv().await.unwrap();
        assert_eq!(
            heard.envelope.meta.unwrap().topic,
            "platform.lake.v:live.event.prices-recorded"
        );
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
            handlers: Arc::default(),
            instance_id: "core-1".into(),
            default_timeout: Duration::from_secs(5),
            clock: Arc::new(ManualClock::at(NOW)),
            observer: RwLock::new(None),
            reads: Arc::default(),
            serves_only: RwLock::new(None),
            routing: Arc::default(),
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

/// `envelope`, a write of `topic`, handed to writer group `group` on its
/// store's internal topic, and answered with the owner's reply: its answer,
/// or its refusal word for word. An owner that does not answer -- none
/// serving, mid-failover, or past the caller's deadline -- is refused
/// UNAVAILABLE, which the caller may ask again.
fn hand(route: &Routing, group: u32, topic: &str, envelope: Envelope) -> HandlerReply {
    let unavailable = |why: String| {
        crate::refusal_with_status(
            crate::RefusalStatus::Unavailable,
            &[],
            format!("{topic} is writer group {group}'s to write, and {why}; ask again"),
        )
    };
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| unavailable("this member has no runtime to hand it on".into()))?;
    let timeout = match crate::read_after::deadline() {
        Some(deadline) => deadline
            .saturating_duration_since(std::time::Instant::now())
            .saturating_sub(HANDED_BACK),
        None => route.timeout,
    };
    if timeout.is_zero() {
        return Err(unavailable(
            "the caller's deadline left no time to hand it on".into(),
        ));
    }
    let started = std::time::Instant::now();
    let answered = handle.block_on(route.backend.request_to_group(
        &route.internal,
        envelope,
        timeout,
        group,
    ));
    let took = started.elapsed();
    let ms = format!("{:.1}", took.as_secs_f64() * 1e3);
    if matches!(answered, Ok(_) | Err(BusError::HandlerFailed { .. })) {
        crate::owners::note_handed(took);
    }
    match answered {
        Ok(answer) => {
            tracing::debug!(topic, group, ms, "a write handed to its owner: answered");
            Ok((answer.payload_type, answer.payload))
        }
        Err(BusError::HandlerFailed { detail, .. }) => {
            tracing::debug!(topic, group, ms, "a write handed to its owner: refused");
            Err(detail)
        }
        Err(failed) => {
            tracing::warn!(topic, group, ms, %failed, "a write handed to its owner: not answered");
            Err(unavailable(format!("it did not answer ({failed})")))
        }
    }
}
