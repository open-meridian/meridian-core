//! The broker's permissions, from the grant table and the topic registry.
//!
//! Decision 010: a broker carries the bus, and what it enforces has to be the
//! same policy the grant table states. A permission list maintained beside the
//! grants it mirrors is two policies that must agree, and v1's recorded
//! failures are what that looks like when they stop.
//!
//! So this generates one from the other, and it lives in the runtime image
//! rather than in a script beside the chart because a bundled broker has to
//! generate its own configuration at start, from the grants the deployment
//! was given and the instances it was configured with. A chart that computed
//! the same thing in templates would be the second policy this exists to
//! avoid.
//!
//! One credential per plugin instance, never per role. Two plugins sharing a
//! role would otherwise publish as each other: the grant table writes
//! `platform.custody.*.event.sync-status`, and that `*` is the instance
//! segment, so a role-wide credential carries the right to speak as every
//! instance of it.

use std::collections::BTreeSet;

use serde_json::Value;

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

/// A plugin instance, as the deployment launches it.
#[derive(Debug, Clone)]
pub struct Instance {
    pub instance_id: String,
    pub role: String,
    pub tags: Vec<String>,
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
/// the instance's own. A dashboard subscribes to
/// `platform.custody.*.event.sync-status` to hear every connector, and
/// rewriting that to its own identifier leaves it subscribed to a topic nobody
/// publishes. You may speak only as yourself; you may listen to everyone your
/// grants allow.
fn scoped(pattern: &str, instance_id: &str, role: &str) -> String {
    let segments: Vec<&str> = pattern.split('.').collect();
    if segments.len() == 5 && segments[2] == "*" && segments[1] == role {
        return format!(
            "{}.{}.{}.{}.{}",
            segments[0], segments[1], instance_id, segments[3], segments[4]
        );
    }
    pattern.to_string()
}

fn listed(table: &Value, key: &str) -> Vec<String> {
    table
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// What one instance may publish and subscribe to.
///
/// A role and each of its tags are looked up in the one table and unioned,
/// which is what the grant table's own note says: a tag is an entry there like
/// any other.
pub fn permissions_for(
    grants: &Value,
    role: &str,
    tags: &[String],
    instance_id: &str,
) -> (Vec<String>, Vec<String>) {
    let roles = grants.get("roles").cloned().unwrap_or(Value::Null);
    let mut publish = BTreeSet::new();
    let mut subscribe = BTreeSet::new();

    for name in std::iter::once(role).chain(tags.iter().map(String::as_str)) {
        let Some(table) = roles.get(name) else {
            continue;
        };
        // Publishing is narrowed to this instance; subscribing is not,
        // because hearing every instance of another role is what a dashboard
        // is for.
        for topic in listed(table, "publish") {
            publish.insert(subject(&scoped(&topic, instance_id, role)));
        }
        for topic in listed(table, "subscribe") {
            subscribe.insert(subject(&topic));
        }
    }

    (
        publish.into_iter().collect(),
        subscribe.into_iter().collect(),
    )
}

/// The registry's own publisher and subscriber columns, for one set of
/// components.
pub fn from_manifest(manifest: &str, components: &[&str]) -> (Vec<String>, Vec<String>) {
    let mut publish = BTreeSet::new();
    let mut subscribe = BTreeSet::new();

    for line in manifest.lines() {
        if line.starts_with('#') || line.starts_with("topic\t") || line.trim().is_empty() {
            continue;
        }
        let columns: Vec<&str> = line.split('\t').collect();
        let [topic, _kind, publisher, subscriber] = columns[..] else {
            continue;
        };
        let named = |column: &str| {
            column
                .split(',')
                .map(str::trim)
                .any(|name| components.contains(&name))
        };
        if named(publisher) {
            publish.insert(subject(topic));
        }
        if named(subscriber) {
            subscribe.insert(subject(topic));
        }
    }

    (
        publish.into_iter().collect(),
        subscribe.into_iter().collect(),
    )
}

fn variable(user: &str) -> String {
    format!("MERIDIAN_NATS_{}", user.to_uppercase().replace('-', "_"))
}

/// Every user this deployment's broker admits, in the order they are written.
///
/// Refuses an instance launched as a role the grant table does not define: an
/// instance with no grants has no bus, and saying so here is better than a
/// connector that connects and is refused every topic.
pub fn users(manifest: &str, grants: &Value, instances: &[Instance]) -> Result<Vec<User>, String> {
    let mut users = Vec::new();

    for component in OWN_CREDENTIAL {
        let (mut publish, mut subscribe) = from_manifest(manifest, &[component]);
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

    let (mut publish, mut subscribe) = from_manifest(manifest, &RUNTIME_COMPONENTS);
    publish.push(INBOX.to_string());
    subscribe.push(INBOX.to_string());
    users.push(User {
        user: "runtime".to_string(),
        password_env: variable("runtime"),
        publish,
        subscribe,
        note: "The runtime's components, from the registry's own columns.".to_string(),
    });

    let roles = grants.get("roles").cloned().unwrap_or(Value::Null);
    for instance in instances {
        let known = |name: &str| roles.get(name).is_some();
        if !instance.role.is_empty()
            && !known(&instance.role)
            && !instance.tags.iter().any(|tag| known(tag))
        {
            return Err(format!(
                "{} is launched as `{}`, which the grant table does not define. \
                 An instance with no grants has no bus.",
                instance.instance_id, instance.role
            ));
        }

        let (mut publish, mut subscribe) = permissions_for(
            grants,
            &instance.role,
            &instance.tags,
            &instance.instance_id,
        );
        publish.push(INBOX.to_string());
        subscribe.push(INBOX.to_string());

        let tags = if instance.tags.is_empty() {
            String::new()
        } else {
            format!(" with {}", instance.tags.join(", "))
        };
        users.push(User {
            user: instance.instance_id.clone(),
            password_env: variable(&instance.instance_id),
            publish,
            subscribe,
            note: format!(
                "{}, launched as {}{tags}",
                instance.instance_id, instance.role
            ),
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
