//! What a plugin is allowed to do.
//!
//! Grants are patterns, not topics, because a connector publishing on its own
//! instance-scoped topic would otherwise need a grant minted per instance.
//!
//! The table is loaded from configuration. Until it has loaded there is no
//! table, and that state is distinct from an empty one: an empty table denies
//! everything and is a decision, while a missing table means nothing is known
//! yet and admission is refused rather than guessed.

use std::collections::HashMap;

use meridian_bus::topic;
use serde::{Deserialize, Serialize};

/// The patterns one role may publish on and subscribe to.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Grants {
    #[serde(default)]
    pub publish: Vec<String>,
    #[serde(default)]
    pub subscribe: Vec<String>,
}

impl Grants {
    /// May this plugin publish on `topic`?
    ///
    /// Any matching pattern is enough. Grants are additive by
    /// construction, so there is no deny pattern to check afterwards.
    pub fn may_publish(&self, topic: &str) -> bool {
        self.publish.iter().any(|p| topic::matches(p, topic))
    }

    /// May this plugin subscribe to `pattern`?
    ///
    /// The grant is the widest pattern allowed, so the request must be no wider
    /// than it. Narrowing is always fine; widening never is. Checking the
    /// request by simply matching it against the grant would be wrong in one
    /// direction: a plugin granted one instance's topics could ask for every
    /// instance's, and each concrete topic the wildcard later delivered would
    /// still match the grant.
    pub fn may_subscribe(&self, pattern: &str) -> bool {
        self.subscribe
            .iter()
            .any(|granted| covers(granted, pattern))
    }
}

/// Does `grant` permit everything `request` would deliver?
///
/// Three cases, and the third is the one that matters:
///
/// - identical patterns permit each other
/// - a concrete request is permitted when the grant matches it
/// - a wildcard request is permitted only by a grant at least as wide, which in
///   this grammar means a grant ending in `**` whose literal prefix the request
///   shares
fn covers(grant: &str, request: &str) -> bool {
    if grant == request {
        return true;
    }
    if !request.contains('*') {
        return topic::matches(grant, request);
    }
    match grant.strip_suffix(".**") {
        // The trailing dot matters: without it `platform.kernel.**` would
        // appear to cover `platform.kernelish.*`.
        Some(prefix) => request.starts_with(&format!("{prefix}.")),
        None => false,
    }
}

/// Grants by role. Merged across a plugin's role and its tags.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GrantTable {
    #[serde(default)]
    pub roles: HashMap<String, Grants>,
}

impl GrantTable {
    pub fn from_json(raw: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(raw)
    }

    /// Everything granted to a role plus its tags, merged.
    ///
    /// A plugin serving one role with extra tags gets the union. Denial is by
    /// absence, so there is nothing to subtract and no precedence to resolve.
    pub fn resolve(&self, role: &str, tags: &[String]) -> Grants {
        let mut merged = Grants::default();
        for key in std::iter::once(role).chain(tags.iter().map(String::as_str)) {
            if let Some(grants) = self.roles.get(key) {
                merged.publish.extend(grants.publish.iter().cloned());
                merged.subscribe.extend(grants.subscribe.iter().cloned());
            }
        }
        merged.publish.sort();
        merged.publish.dedup();
        merged.subscribe.sort();
        merged.subscribe.dedup();
        merged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> GrantTable {
        GrantTable::from_json(
            r#"{
              "roles": {
                "custody": {
                  "publish": [
                    "platform.kernel.command.record-holding",
                    "platform.custody.*.event.sync-status"
                  ],
                  "subscribe": ["platform.reference.event.instrument-applied"]
                },
                "diagnostics": { "subscribe": ["platform.kernel.**"] }
              }
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn a_granted_topic_may_be_published() {
        let g = table().resolve("custody", &[]);
        assert!(g.may_publish("platform.kernel.command.record-holding"));
    }

    #[test]
    fn an_ungranted_topic_may_not() {
        let g = table().resolve("custody", &[]);
        assert!(!g.may_publish("platform.kernel.command.record-statement"));
    }

    #[test]
    fn a_wildcard_grant_covers_a_concrete_instance_topic() {
        let g = table().resolve("custody", &[]);
        assert!(g.may_publish("platform.custody.snaptrade-1.event.sync-status"));
    }

    #[test]
    fn tags_add_to_the_role() {
        let g = table().resolve("custody", &["diagnostics".to_string()]);
        assert!(g.may_publish("platform.kernel.command.record-holding"));
        assert!(g.may_subscribe("platform.kernel.**"));
    }

    #[test]
    fn an_unknown_role_gets_nothing() {
        let g = table().resolve("nonexistent", &[]);
        assert!(g.publish.is_empty());
        assert!(g.subscribe.is_empty());
    }

    #[test]
    fn a_subscription_may_not_be_widened_past_its_grant() {
        // Granted one instance's status topics; asking for every instance's
        // must be refused, or the grant means nothing.
        let table = GrantTable::from_json(
            r#"{"roles":{"watcher":{"subscribe":["platform.custody.snaptrade-1.event.sync-status"]}}}"#,
        )
        .unwrap();
        let g = table.resolve("watcher", &[]);

        assert!(g.may_subscribe("platform.custody.snaptrade-1.event.sync-status"));
        assert!(!g.may_subscribe("platform.custody.*.event.sync-status"));
        assert!(!g.may_subscribe("platform.custody.**"));
    }

    #[test]
    fn a_subscription_may_be_narrowed_inside_its_grant() {
        let g = table().resolve("diagnostics", &[]);
        assert!(g.may_subscribe("platform.kernel.**"));
        // Narrower than the grant, in both concrete and wildcard form.
        assert!(g.may_subscribe("platform.kernel.event.position-updated"));
        assert!(g.may_subscribe("platform.kernel.event.*"));
    }

    #[test]
    fn a_wildcard_grant_does_not_leak_into_a_neighbouring_prefix() {
        let table =
            GrantTable::from_json(r#"{"roles":{"w":{"subscribe":["platform.kernel.**"]}}}"#)
                .unwrap();
        let g = table.resolve("w", &[]);
        // Sharing a string prefix is not sharing a topic prefix.
        assert!(!g.may_subscribe("platform.kernelish.*"));
        assert!(!g.may_subscribe("platform.reference.**"));
    }
}
