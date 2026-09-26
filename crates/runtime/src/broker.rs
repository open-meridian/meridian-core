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

use meridian_sidecar::Contract;

/// Where a reply lands. A caller has to hear its own answer, and a responder
/// has to be able to reply to whoever asked; the broker scopes the second to
/// inboxes it actually received, which is tighter than any list.
const INBOX: &str = "_INBOX.>";

/// The components this deployment's runtime hosts, sharing one credential.
const RUNTIME_COMPONENTS: [&str; 3] = ["instrument", "street", "conductor"];

/// Components holding a credential of their own rather than the runtime's.
///
/// First run holds the only right in a deployment to change the cluster, and
/// the wizard seals every credential it sends to it: a shared credential would
/// let any component receive those, and would give that Job every other
/// component's rights (spec/installation-and-first-run, ruling 5).
const OWN_CREDENTIAL: [&str; 1] = ["first-run"];

/// Something the broker admits by instance: a plugin's sidecar, holding a
/// set of roles, or a component with a credential of its own under an
/// instance name -- the dashboard, which is one (decisions/020).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Instance {
    Plugin {
        instance_id: String,
        roles: Vec<String>,
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
        }
    }

    pub fn component(instance_id: impl Into<String>, component: impl Into<String>) -> Self {
        Instance::Component {
            instance_id: instance_id.into(),
            component: component.into(),
        }
    }

    fn id(&self) -> &str {
        match self {
            Instance::Plugin { instance_id, .. } | Instance::Component { instance_id, .. } => {
                instance_id
            }
        }
    }
}

/// One user the broker admits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub user: String,
    /// The environment variable the broker reads the password from, so a
    /// password is never in a generated file.
    pub password_env: String,
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
    let granted = contract.grants_for(roles)?;
    let sidecar = contract.component("sidecar");
    let mut publish = BTreeSet::new();
    let mut subscribe = BTreeSet::new();
    for topic in granted.publish.iter().chain(&sidecar.publish) {
        publish.insert(subject(&scoped(topic, instance_id, roles)));
    }
    for topic in granted.subscribe.iter().chain(&sidecar.subscribe) {
        subscribe.insert(subject(topic));
    }
    Ok((
        publish.into_iter().collect(),
        subscribe.into_iter().collect(),
    ))
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
        publish.push(INBOX.to_string());
        subscribe.push(INBOX.to_string());
        users.push(User {
            user: component.to_string(),
            password_env: variable(component),
            publish,
            subscribe,
            note: format!(
                "{component}, its own credential rather than the runtime's: \
                 what the wizard seals reaches it and nothing else."
            ),
        });
    }

    let (mut publish, mut subscribe) = component_subjects(contract, &RUNTIME_COMPONENTS);
    publish.push(INBOX.to_string());
    subscribe.push(INBOX.to_string());
    users.push(User {
        user: "runtime".to_string(),
        password_env: variable("runtime"),
        publish,
        subscribe,
        note: "The runtime's components, from the contract's own columns.".to_string(),
    });

    for instance in instances {
        let (mut publish, mut subscribe, note) = match instance {
            Instance::Plugin { instance_id, roles } => {
                let (publish, subscribe) = permissions_for(contract, roles, instance_id)
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
        publish.push(INBOX.to_string());
        subscribe.push(INBOX.to_string());
        users.push(User {
            user: instance.id().to_string(),
            password_env: variable(instance.id()),
            publish,
            subscribe,
            note,
        });
    }

    Ok(users)
}

/// The users, as the broker's own configuration.
pub fn rendered(header: &str, users: &[User]) -> String {
    let mut out = String::from(header);
    out.push_str("\nauthorization {\n  users = [\n");
    for user in users {
        out.push_str(&format!("    # {}\n", user.note));
        out.push_str(&format!(
            "    {{ user: {}, password: ${}, permissions: {{ publish: {{ allow: {} }}, \
             subscribe: {{ allow: {} }}, allow_responses: true }} }}\n",
            user.user,
            user.password_env,
            list(&user.publish),
            list(&user.subscribe),
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
