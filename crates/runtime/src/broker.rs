//! The broker's permissions, from the contract.
//!
//! Decision 010: a broker carries the bus, and what it enforces has to be the
//! same policy the sidecars enforce. Decision 020: that policy is the
//! contract's -- a role holds a topic exactly when the matrix names it, and a
//! component likewise -- compiled into this image as the sidecar's is, so the
//! two cannot disagree and nobody writes either down.
//!
//! It lives in the runtime image rather than in a script beside the chart
//! because a bundled broker has to generate its own configuration at start,
//! from the instances it was configured with. A chart that computed the same
//! thing in templates would be a second policy.
//!
//! One credential per plugin instance, never per role. Two plugins sharing a
//! role would otherwise publish as each other: the contract writes
//! `platform.custody.*.event.sync-status`, and that `*` is the instance
//! segment, so a role-wide credential carries the right to speak as every
//! instance of it.

use std::collections::BTreeSet;

use meridian_analytics::queue as units;
use meridian_bus::{inbox, write_ahead};
use meridian_sidecar::Contract;

/// Where a reply lands: each user's own inboxes, `_INBOX.<user>.`, which its
/// client makes its inboxes under (v31a, item 2; [`inbox`]). A caller hears
/// its own answers and no other's. Who else may publish into them, to answer
/// it, is [`answer_grants`]'s to say.
fn own_inbox(user: &str) -> String {
    inbox::grant(user)
}

/// What a credential is, for the inboxes it answers on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Answering {
    /// The runtime's credential, which answers every other.
    Runtime,
    /// A component's own credential, or a component instance's.
    Core,
    /// A plugin instance's, which answers none.
    Plugin,
}

/// Each credential's publish, never subscribe, on the inboxes of those it
/// answers (v31a, item 2, as restated 2026-11-20; the proposal on answers
/// refused across a broker reload, option A, approved by consensus).
///
/// - A non-runtime core credential -- an own-credential component
///   (first-run, instrument, analytics) or a component instance (the
///   dashboard's, the launcher's) -- gets `_INBOX.<asker>.>` for every
///   credential the contract allows to send a request it serves: each
///   command or query row whose subscriber column names it, every
///   credential its publisher column names. From those two columns alone,
///   never from the rendered grants: a component topic is granted publish
///   and subscribe alike, which would read every holder as answering every
///   other.
/// - The runtime's credential answers every one of them already, so it gets
///   every other credential's: the lake's writer also answers a write it
///   took from the write-ahead stream on the inbox the write named (W10.4),
///   carried there by no request, and answers only an inbox of the instance
///   whose subject the write came on (`meridian_lake::write_ahead`).
/// - A plugin credential gets none: no plugin publishes into another's
///   inboxes.
///
/// So an answer never needs the broker's `allow_responses`, the one answer
/// it admits on a request's reply subject whatever the grants: a
/// configuration reload -- every plugin launch, removal and entitlement
/// change makes one -- registers each client's user again and forgets the
/// replies it was owed, and an answer it would admit leaves a reply entry
/// on the answering connection until it expires when it is never sent.
/// `allow_responses` stays, as the fallback ([`ANSWERED_WITHIN`]): `ha`'s
/// brokers reload one after another. A standing rule, not an interim;
/// reply rights across a reload are an input to v32's accounts spike
/// (decision 12).
///
/// Subscribe stays each credential's own: only X hears `_INBOX.X.>`, and no
/// grant holds `_INBOX.>`. Called once every user is made, with the
/// instances they were made for, and again whenever one is added.
pub fn answer_grants(contract: &Contract, instances: &[Instance], users: &mut [User]) {
    // Each user's kind, and the contract's names it holds: the runtime its
    // components, a component its own name, a plugin instance its roles and
    // its sidecar's traffic. A user made for none of these answers nobody.
    let held: Vec<Option<(Answering, Vec<String>)>> = users
        .iter()
        .map(|user| {
            let user = user.user.as_str();
            if user == "runtime" {
                return Some((
                    Answering::Runtime,
                    RUNTIME_COMPONENTS.map(String::from).to_vec(),
                ));
            }
            if OWN_CREDENTIAL.contains(&user) {
                return Some((Answering::Core, vec![user.to_string()]));
            }
            match instances.iter().find(|instance| instance.id() == user)? {
                Instance::Plugin { roles, .. } => {
                    let mut names = meridian_domain::roles_now(roles);
                    names.push("sidecar".to_string());
                    Some((Answering::Plugin, names))
                }
                Instance::Component { component, .. } => {
                    Some((Answering::Core, vec![component.clone()]))
                }
            }
        })
        .collect();
    let named = |index: usize, column: &[String]| {
        held[index]
            .as_ref()
            .is_some_and(|(_, names)| names.iter().any(|name| column.contains(name)))
    };
    let kind = |index: usize| held[index].as_ref().map(|(kind, _)| *kind);
    // Whom each core credential answers: by index into `users`.
    let mut askers: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); users.len()];
    for (asking, serving) in contract.requests() {
        let sending: Vec<usize> = (0..users.len())
            .filter(|&index| named(index, asking))
            .collect();
        for responder in (0..users.len()).filter(|&index| named(index, serving)) {
            if kind(responder) == Some(Answering::Core) {
                askers[responder].extend(&sending);
            }
        }
    }
    let inboxes: Vec<String> = users.iter().map(|user| own_inbox(&user.user)).collect();
    for (index, user) in users.iter_mut().enumerate() {
        let theirs = (0..inboxes.len()).filter(|&other| {
            other != index
                && match kind(index) {
                    Some(Answering::Runtime) => true,
                    Some(Answering::Core) => askers[index].contains(&other),
                    Some(Answering::Plugin) | None => false,
                }
        });
        // In the users' order, so the file reads as the list does.
        for other in theirs {
            if !user.publish.contains(&inboxes[other]) {
                user.publish.push(inboxes[other].clone());
            }
        }
    }
}

/// The components this deployment's runtime hosts, sharing one credential.
const RUNTIME_COMPONENTS: [&str; 4] = ["street", "bor", "lake", "conductor"];

/// The segment a lake row's subject names its dataset by (W10.5; topics.md,
/// "Pattern"): the lake publishes on every dataset's, and a plugin subscribes
/// to the datasets it is entitled to alone (contract v18, W10.1).
const DATASET: &str = "{dataset}";

/// Components holding a credential of their own rather than the runtime's.
///
/// First run holds the only right in a deployment to change the cluster, and
/// the wizard seals every credential it sends to it: a shared credential would
/// let any component receive those, and would give that Job every other
/// component's rights (spec/installation-and-first-run, ruling 5).
///
/// The instrument store holds one too (contract v20, approved by the product
/// owner 2026-10-11): it reads the launch records, the plugin catalogue, to
/// know which roles a plugin was launched with, and that read is granted to
/// it by its own column in the contract, not borrowed from the runtime's.
///
/// The analytics component holds one too (contract v26, W12): its rows are
/// its own column, and its coordinator and workers share their own topic,
/// below, which no other credential carries.
const OWN_CREDENTIAL: [&str; 3] = ["first-run", "instrument", "analytics"];

/// A component's own topics, which are not contract rows (topics.md,
/// "Analytics": "component topics, not rows"): the analytics coordinator
/// asking its worker pool for a unit (ruling 315, 2026-10-18), granted to
/// that component's credential alone, publish and subscribe.
///
/// From step 3a, each store's change log (topics.md, "Component topics";
/// spec/one-data-access-model section 3): its `read-changes` query and its
/// tail, `changes.{partition}`, held by the store's own members -- the
/// runtime's credential for the lake, the book, the street store and the
/// conductor, the instrument store's own for its -- and the lake's and the
/// book's by the analytics component too, so a standing job can catch up a
/// gap. Published and subscribed alike: a member asks and answers, tells
/// and hears.
///
/// From contract v30 (W12.4; the step 3 plan's question 13), the analytics
/// coordinators' own: each says it is live on `members` and hears the
/// others there, and is handed a standing job's question by another on
/// `standing.{member}`, its pod's name (approved 2026-11-10); and the lake's
/// `read-from-writer`, on which a lake reader hands its writer a read that
/// depends on the writer's own acts (stage B, approved 2026-11-10).
///
/// From step 3d (the resilience spec's section 6.7), a write handed to the
/// writer group owning its partition: the lake's `internal.record` and the
/// book's `internal.command`, each to its store's members alone.
///
/// From step 3d's checkpoint 5 (item 32), a plugin shard's epoch raised in
/// each writer group: `raise-shard.{group}`, the lake's and the book's,
/// asked by the lake's control writer of every group before it answers a
/// take.
const COMPONENT_TOPICS: [(&str, &str); 18] = [
    ("analytics", "platform.analytics.internal.compute-unit"),
    ("analytics", "platform.analytics.internal.members"),
    ("analytics", "platform.analytics.internal.standing.*"),
    ("lake", "platform.lake.internal.read-changes"),
    ("lake", "platform.lake.internal.changes.*"),
    ("lake", "platform.lake.internal.read-from-writer"),
    ("lake", "platform.lake.internal.record"),
    ("lake", "platform.lake.internal.raise-shard.*"),
    ("bor", "platform.book.internal.read-changes"),
    ("bor", "platform.book.internal.changes.*"),
    ("bor", "platform.book.internal.command"),
    ("bor", "platform.book.internal.raise-shard.*"),
    ("instrument", "platform.reference.internal.read-changes"),
    ("instrument", "platform.reference.internal.changes.*"),
    ("street", "platform.street.internal.read-changes"),
    ("street", "platform.street.internal.changes.*"),
    ("conductor", "platform.config.internal.read-changes"),
    ("conductor", "platform.config.internal.changes.*"),
];

/// Who else holds a component's topic (topics.md's "Who else is granted
/// it"): the analytics component, the lake's and the book's change logs;
/// and the lake, the book's `raise-shard.{group}` (step 3d, item 32).
const COMPONENT_TOPICS_ALSO: [(&str, &str); 5] = [
    ("analytics", "platform.lake.internal.read-changes"),
    ("analytics", "platform.lake.internal.changes.*"),
    ("analytics", "platform.book.internal.read-changes"),
    ("analytics", "platform.book.internal.changes.*"),
    ("lake", "platform.book.internal.raise-shard.*"),
];

/// Every component topic a credential holding `components` is granted.
fn component_topics(components: &[&str]) -> Vec<&'static str> {
    COMPONENT_TOPICS
        .iter()
        .chain(COMPONENT_TOPICS_ALSO.iter())
        .filter(|(owner, _)| components.contains(owner))
        .map(|(_, topic)| *topic)
        .collect()
}

/// JetStream's API and acknowledgements a credential holding `components`
/// is granted: each component's own stream's, and only the calls it makes.
/// Published alone, never subscribed: the server answers each call on the
/// caller's inbox and delivers a consumer's messages there, so no client
/// hears these subjects, and one that could would hear other clients' calls
/// and could answer for the server.
///
/// Every credential is in one account, so a grant over JetStream's whole API
/// is a grant over every stream in the deployment: the analytics component
/// holding `$JS.API.>` could delete or purge the lake's write-ahead stream
/// (review C, 2026-11-20, X1). So each grant names its stream.
///
/// From step 3b, with JetStream at three for the analytics units' work queue
/// (spec/a-deployment-scales-out-and-fails-over-by-configuration section 4),
/// to the analytics component's credential: its coordinators find or make
/// the units' stream and put units on it (its subject is the component's
/// own topic, above), and its workers find or make their one consumer, take
/// from it and acknowledge each unit taken (`meridian_analytics::queue`).
///
/// From contract v30's stage E, in `ha`, the lake's write-ahead stream
/// (the resilience spec's section 4), to the lake's credential, so its writer
/// makes the stream and reads it (`meridian_lake::write_ahead`): never its
/// deletion or purge, which a lake never asks for. A data plugin's
/// credential publishes to the stream's subjects of its own instance alone
/// ([`write_ahead::instance_subjects`]), in [`permissions_entitled`].
fn jetstream_grants(components: &[&str]) -> Vec<String> {
    let mut granted = Vec::new();
    if components.contains(&"analytics") {
        let (stream, consumer) = (units::STREAM, units::CONSUMER);
        granted.extend([
            // `get_or_create_stream` and `get_stream`: asked, made if not there.
            format!("$JS.API.STREAM.INFO.{stream}"),
            format!("$JS.API.STREAM.CREATE.{stream}"),
            // `get_or_create_consumer`, and asked again by a pull on a
            // reconnect.
            format!("$JS.API.CONSUMER.INFO.{stream}.{consumer}"),
            format!("$JS.API.CONSUMER.CREATE.{stream}.{consumer}"),
            // A worker's pull.
            format!("$JS.API.CONSUMER.MSG.NEXT.{stream}.{consumer}"),
            // A unit's acknowledgement, on the subject it was delivered with.
            format!("$JS.ACK.{stream}.{consumer}.>"),
        ]);
    }
    if components.contains(&"lake") {
        let stream = write_ahead::STREAM;
        granted.extend([
            // Its place (`place`), and the stream made or brought to this
            // release's bounds (`get_stream`, then `update_stream` or
            // `create_stream`).
            format!("$JS.API.STREAM.INFO.{stream}"),
            format!("$JS.API.STREAM.CREATE.{stream}"),
            format!("$JS.API.STREAM.UPDATE.{stream}"),
            // Its reader: an ephemeral consumer, made unnamed, so the
            // server names it, and only then asked, pulled from and let go
            // by that name -- the one `*`, the server's name for it.
            format!("$JS.API.CONSUMER.CREATE.{stream}"),
            format!("$JS.API.CONSUMER.INFO.{stream}.*"),
            format!("$JS.API.CONSUMER.MSG.NEXT.{stream}.*"),
            format!("$JS.API.CONSUMER.DELETE.{stream}.*"),
        ]);
    }
    granted
}

/// Why a component holds a credential of its own, as its user's note says.
fn own_credential_note(component: &str) -> String {
    match component {
        "first-run" => format!(
            "{component}, its own credential rather than the runtime's: \
             what the wizard seals reaches it and nothing else."
        ),
        _ => format!(
            "{component}, its own credential rather than the runtime's: \
             its own column in the contract, and nothing the runtime's components share."
        ),
    }
}

/// Something the broker admits by instance: a plugin's sidecar, holding a
/// set of roles, or a component with a credential of its own under an
/// instance name -- the dashboard, which is one (decisions/020).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Instance {
    Plugin {
        instance_id: String,
        roles: Vec<String>,
        /// The datasets it is entitled to (contract v18): the only dataset
        /// subjects it may subscribe to.
        datasets: Vec<String>,
    },
    Component {
        instance_id: String,
        component: String,
    },
}

impl Instance {
    pub fn plugin(instance_id: impl Into<String>, roles: &[&str]) -> Self {
        Instance::Plugin {
            instance_id: instance_id.into(),
            roles: roles.iter().map(|role| role.to_string()).collect(),
            datasets: Vec::new(),
        }
    }

    /// The same plugin, entitled to these datasets.
    pub fn with_datasets(self, entitled: Vec<String>) -> Self {
        match self {
            Instance::Plugin {
                instance_id, roles, ..
            } => Instance::Plugin {
                instance_id,
                roles,
                datasets: entitled,
            },
            component => component,
        }
    }

    pub fn component(instance_id: impl Into<String>, component: impl Into<String>) -> Self {
        Instance::Component {
            instance_id: instance_id.into(),
            component: component.into(),
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Instance::Plugin { instance_id, .. } | Instance::Component { instance_id, .. } => {
                instance_id
            }
        }
    }
}

/// Where the broker finds a user's password. Neither puts one in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Password {
    /// An environment variable the broker reads at start: the components and
    /// the plugins the chart runs, whose passwords the chart made.
    Env(String),
    /// A bcrypt hash of it, which the broker checks a password against: a
    /// plugin launched after the broker started, whose password reached the
    /// broker's environment too late to be read from there
    /// (spec/the-local-plugin-registry, decision 3).
    Bcrypt(String),
}

/// One user the broker admits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub user: String,
    pub password: Password,
    pub publish: Vec<String>,
    pub subscribe: Vec<String>,
    pub note: String,
}

/// Our pattern in the broker's grammar.
///
/// `*` is one segment in both. `**` is a tail of one or more, which is `>`,
/// and the grammar already restricts it to the tail, so this cannot produce a
/// subject meaning something else.
fn subject(pattern: &str) -> String {
    let pattern = &pattern.replace(DATASET, "*");
    if pattern == "**" {
        return ">".to_string();
    }
    match pattern.strip_suffix(".**") {
        Some(head) => format!("{head}.>"),
        None => pattern.to_string(),
    }
}

/// The instance's own version of a topic it publishes.
///
/// Two conditions, the second learned by getting it wrong: the domain must be
/// one of the instance's own roles. A dashboard subscribes to
/// `platform.custody.*.event.sync-status` to hear every connector, and
/// rewriting that to its own identifier leaves it subscribed to a topic nobody
/// publishes. You may speak only as yourself; you may listen to everyone your
/// grants allow.
fn scoped(pattern: &str, instance_id: &str, roles: &[String]) -> String {
    let segments: Vec<&str> = pattern.split('.').collect();
    if segments.len() == 5 && segments[2] == "*" && roles.iter().any(|role| role == segments[1]) {
        return format!(
            "{}.{}.{}.{}.{}",
            segments[0], segments[1], instance_id, segments[3], segments[4]
        );
    }
    pattern.to_string()
}

/// A component's topics, in the broker's grammar.
fn component_subjects(contract: &Contract, names: &[&str]) -> (Vec<String>, Vec<String>) {
    let mut publish = BTreeSet::new();
    let mut subscribe = BTreeSet::new();
    for name in names {
        let grants = contract.component(name);
        publish.extend(grants.publish.iter().map(|topic| subject(topic)));
        subscribe.extend(grants.subscribe.iter().map(|topic| subject(topic)));
    }
    (
        publish.into_iter().collect(),
        subscribe.into_iter().collect(),
    )
}

/// What one plugin instance may publish and subscribe to: its roles' topics,
/// with what it publishes in its own domains narrowed to itself, and every
/// sidecar's own traffic -- asking for its configuration and access,
/// reporting its plugin -- which is the sidecar's rather than the plugin's
/// and which every plugin credential carries (topics.md, Config).
pub fn permissions_for(
    contract: &Contract,
    roles: &[String],
    instance_id: &str,
) -> Result<(Vec<String>, Vec<String>), String> {
    permissions_entitled(contract, roles, instance_id, &[])
}

/// [`permissions_for`], with the datasets the instance is entitled to
/// (contract v18, W10.1): a row naming a dataset in its topic is granted on
/// each of those datasets' subjects, and on no other.
pub fn permissions_entitled(
    contract: &Contract,
    roles: &[String],
    instance_id: &str,
    datasets: &[String],
) -> Result<(Vec<String>, Vec<String>), String> {
    let granted = contract.grants_for(roles)?;
    let sidecar = contract.component("sidecar");
    let mut publish = BTreeSet::new();
    let mut subscribe = BTreeSet::new();
    for topic in granted.publish.iter().chain(&sidecar.publish) {
        publish.insert(subject(&scoped(topic, instance_id, roles)));
    }
    // A data plugin's rows go by the lake's write-ahead stream in `ha`: on
    // its own instance's subjects alone, so it writes no other's datasets.
    if roles.iter().any(|role| role == "data") && dataset_reads(instance_id) {
        publish.insert(write_ahead::instance_subjects(instance_id));
        // And the last write of each of its own datasets read back from the
        // stream, by subject, as a restarted sidecar goes on with its chain
        // (contract v30, stage C): its own instance's subjects alone.
        publish.insert(format!(
            "$JS.API.DIRECT.GET.{}.{}",
            write_ahead::STREAM,
            write_ahead::instance_subjects(instance_id)
        ));
    }
    for topic in granted.subscribe.iter().chain(&sidecar.subscribe) {
        if topic.contains(DATASET) {
            for dataset in datasets.iter().filter(|d| dataset_reads(d)) {
                subscribe.insert(subject(&topic.replace(DATASET, dataset)));
            }
            continue;
        }
        subscribe.insert(subject(topic));
    }
    Ok((
        publish.into_iter().collect(),
        subscribe.into_iter().collect(),
    ))
}

/// Whether a dataset's ID can stand in a subject: one segment, no wildcard
/// and no space.
fn dataset_reads(dataset: &str) -> bool {
    !dataset.is_empty()
        && dataset
            .bytes()
            .all(|b| b.is_ascii_graphic() && !matches!(b, b'.' | b'*' | b'>'))
}

fn variable(user: &str) -> String {
    format!("MERIDIAN_NATS_{}", user.to_uppercase().replace('-', "_"))
}

/// Every user this deployment's broker admits, in the order they are written.
///
/// Refuses an instance launched with a name that is not a role, or as a name
/// that is not one of the deployment's components: said here, where the
/// broker is configured, rather than as a connector that connects and is
/// refused every topic.
pub fn users(contract: &Contract, instances: &[Instance]) -> Result<Vec<User>, String> {
    let mut users = Vec::new();

    for component in OWN_CREDENTIAL {
        let (mut publish, mut subscribe) = component_subjects(contract, &[component]);
        if publish.is_empty() && subscribe.is_empty() {
            continue;
        }
        for topic in component_topics(&[component]) {
            publish.push(topic.to_string());
            subscribe.push(topic.to_string());
        }
        publish.extend(jetstream_grants(&[component]));
        publish.push(own_inbox(component));
        subscribe.push(own_inbox(component));
        users.push(User {
            user: component.to_string(),
            password: Password::Env(variable(component)),
            publish,
            subscribe,
            note: own_credential_note(component),
        });
    }

    let (mut publish, mut subscribe) = component_subjects(contract, &RUNTIME_COMPONENTS);
    for topic in component_topics(&RUNTIME_COMPONENTS) {
        publish.push(topic.to_string());
        subscribe.push(topic.to_string());
    }
    publish.extend(jetstream_grants(&RUNTIME_COMPONENTS));
    publish.push(own_inbox("runtime"));
    subscribe.push(own_inbox("runtime"));
    users.push(User {
        user: "runtime".to_string(),
        password: Password::Env(variable("runtime")),
        publish,
        subscribe,
        note: "The runtime's components, from the contract's own columns.".to_string(),
    });

    for instance in instances {
        users.push(instance_user(
            contract,
            instance,
            Password::Env(variable(instance.id())),
        )?);
    }
    answer_grants(contract, instances, &mut users);

    Ok(users)
}

/// One instance's user: a plugin's roles' topics, or a component's own.
pub fn instance_user(
    contract: &Contract,
    instance: &Instance,
    password: Password,
) -> Result<User, String> {
    let (mut publish, mut subscribe, note) = match instance {
        Instance::Plugin {
            instance_id,
            roles,
            datasets,
        } => {
            let (publish, subscribe) = permissions_entitled(contract, roles, instance_id, datasets)
                .map_err(|refusal| format!("{instance_id} is launched with {refusal}"))?;
            let held = if roles.is_empty() {
                "no role, so no topics but its sidecar's own".to_string()
            } else {
                roles.join(", ")
            };
            (
                publish,
                subscribe,
                format!("{instance_id}, launched as {held}"),
            )
        }
        Instance::Component {
            instance_id,
            component,
        } => {
            if !contract.is_component(component) {
                return Err(format!(
                    "{instance_id} is configured as the component `{component}`, which the \
                     contract does not have"
                ));
            }
            let (publish, subscribe) = component_subjects(contract, &[component]);
            (
                publish,
                subscribe,
                format!("{instance_id}, the {component} component, with its own credential"),
            )
        }
    };
    publish.push(own_inbox(instance.id()));
    subscribe.push(own_inbox(instance.id()));
    Ok(User {
        user: instance.id().to_string(),
        password,
        publish,
        subscribe,
        note,
    })
}

/// How long a responder may send its one answer to a request it was
/// delivered, whatever its grants say (`allow_responses`): the broker's own
/// default, two minutes. The fallback alone: every answer is granted by
/// name ([`answer_grants`]), the longest waits on the bus among them -- an
/// install or an update of a plugin or a pack, which the conductor answers
/// on the dashboard's inbox -- so none rides it, as a run of the real-NATS
/// suite on a broker with no `allow_responses` at all showed (v31a, item 2
/// as restated 2026-11-20). It stays for `ha`'s brokers, which reload one
/// after another. A reply entry it leaves on a connection for a request
/// never answered lasts as long as this; it was fifteen minutes while
/// answers rode it (the review of v31a-core-2, F2).
pub const ANSWERED_WITHIN: std::time::Duration = std::time::Duration::from_secs(120);

/// The users, as the broker's own configuration.
pub fn rendered(header: &str, users: &[User]) -> String {
    let mut out = String::from(header);
    out.push_str("\nauthorization {\n  users = [\n");
    for user in users {
        out.push_str(&format!("    # {}\n", user.note));
        out.push_str(&format!(
            "    {{ user: {}, password: {}, permissions: {{ publish: {{ allow: {} }}, \
             subscribe: {{ allow: {} }}, allow_responses: {{ max: 1, expires: \"{}s\" }} }} }}\n",
            user.user,
            match &user.password {
                Password::Env(variable) => format!("${variable}"),
                // Quoted, so the `$` a hash begins with is not read as a
                // variable.
                Password::Bcrypt(hash) => format!("\"{hash}\""),
            },
            list(&user.publish),
            list(&user.subscribe),
            ANSWERED_WITHIN.as_secs(),
        ));
    }
    out.push_str("  ]\n}\n");
    out
}

/// A subject list, spelled as the broker's own examples do.
fn list(subjects: &[String]) -> String {
    let quoted: Vec<String> = subjects
        .iter()
        .map(|subject| format!("\"{subject}\""))
        .collect();
    format!("[{}]", quoted.join(", "))
}

#[cfg(test)]
#[path = "broker/tests.rs"]
mod tests;
