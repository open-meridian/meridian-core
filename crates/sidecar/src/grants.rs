//! What a plugin is allowed to do, from the contract (decisions/020).
//!
//! Grants are patterns, not topics, because a connector publishing on its own
//! instance-scoped topic would otherwise need a grant minted per instance.
//!
//! A plugin's grants are never written by anybody. It holds one or more roles
//! from a fixed list, and a role holds a topic exactly when the contract's
//! matrix names it as that topic's publisher or subscriber. meridian-design
//! generates both halves into this repository -- `deploy/topics.tsv`, who
//! publishes and subscribes to what, and `deploy/roles.tsv`, which names are
//! roles -- and they are compiled in here, so the grants a sidecar enforces
//! are the ones the runtime was built against, with no file a deployment
//! could forget to mount or edit to disagree.
//!
//! Tags are not here. They divide a plugin among people (W6.7) and grant
//! nothing on the bus.

use std::collections::BTreeSet;
use std::sync::OnceLock;

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
        // The trailing dot matters: without it `platform.street.**` would
        // appear to cover `platform.streetish.*`.
        Some(prefix) => request.starts_with(&format!("{prefix}.")),
        None => false,
    }
}

/// The contract's topic rows and role list.
#[derive(Debug, Clone, Default)]
pub struct Contract {
    rows: Vec<Row>,
    roles: BTreeSet<String>,
    components: BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct Row {
    /// With `*` for an instance segment, as the grant grammar has it.
    topic: String,
    publishers: Vec<String>,
    subscribers: Vec<String>,
}

const TOPICS: &str = include_str!("../../../deploy/topics.tsv");
const ROLES: &str = include_str!("../../../deploy/roles.tsv");

fn lines(file: &str) -> impl Iterator<Item = Vec<&str>> {
    file.lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .skip(1)
        .map(|line| line.split('\t').collect())
}

impl Contract {
    /// The contract this runtime was built against.
    pub fn embedded() -> &'static Contract {
        static EMBEDDED: OnceLock<Contract> = OnceLock::new();
        EMBEDDED.get_or_init(|| {
            Contract::parse(TOPICS, ROLES).expect("the contract compiled into this runtime parses")
        })
    }

    /// A contract from its two generated files.
    pub fn parse(topics: &str, roles: &str) -> Result<Contract, String> {
        let mut contract = Contract::default();
        for columns in lines(roles) {
            match columns[..] {
                [name, "role"] => contract.roles.insert(name.to_string()),
                [name, "component"] => contract.components.insert(name.to_string()),
                _ => {
                    return Err(format!(
                        "a role list line is not a name and a kind: {columns:?}"
                    ))
                }
            };
        }
        for columns in lines(topics) {
            let [topic, _kind, publishers, subscribers] = columns[..] else {
                return Err(format!("a topic line is not four columns: {columns:?}"));
            };
            let names = |column: &str| -> Vec<String> {
                column
                    .split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(String::from)
                    .collect()
            };
            contract.rows.push(Row {
                topic: topic.to_string(),
                publishers: names(publishers),
                subscribers: names(subscribers),
            });
        }
        Ok(contract)
    }

    pub fn is_role(&self, name: &str) -> bool {
        self.roles.contains(name)
    }

    pub fn is_component(&self, name: &str) -> bool {
        self.components.contains(name)
    }

    /// Everything the rows naming any of `names` give them.
    fn named(&self, names: &[&str]) -> Grants {
        let mut publish = BTreeSet::new();
        let mut subscribe = BTreeSet::new();
        for row in &self.rows {
            if row
                .publishers
                .iter()
                .any(|name| names.contains(&name.as_str()))
            {
                publish.insert(row.topic.clone());
            }
            if row
                .subscribers
                .iter()
                .any(|name| names.contains(&name.as_str()))
            {
                subscribe.insert(row.topic.clone());
            }
        }
        Grants {
            publish: publish.into_iter().collect(),
            subscribe: subscribe.into_iter().collect(),
        }
    }

    /// A plugin's grants: the union of its roles'. Refused for a name that is
    /// not a role -- a component's, or one misspelt -- rather than granted
    /// nothing, which would admit it quietly with no bus and nobody the wiser.
    /// No roles at all is a plugin admitted with no topics.
    pub fn grants_for(&self, roles: &[String]) -> Result<Grants, String> {
        if let Some(stranger) = roles.iter().find(|role| !self.is_role(role)) {
            return Err(if self.is_component(stranger) {
                format!("`{stranger}` is one of the deployment's own components, not a role a plugin may hold")
            } else {
                format!(
                    "`{stranger}` is not a role; the roles are {}",
                    self.roles.iter().cloned().collect::<Vec<_>>().join(", ")
                )
            });
        }
        let names: Vec<&str> = roles.iter().map(String::as_str).collect();
        Ok(self.named(&names))
    }

    /// A component's own rows. `sidecar` is every sidecar's.
    pub fn component(&self, name: &str) -> Grants {
        self.named(&[name])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOPICS: &str = "\
# a header comment
topic\tkind\tpublisher\tsubscriber
platform.street.command.record-holding\tcommand\tcustody\tstreet
platform.custody.*.event.sync-status\tevent\tcustody\tdashboard
platform.street.query.list-positions\tquery\treporting,dashboard\tstreet
platform.street.event.position-updated\tevent\tstreet\treporting
platform.config.query.plugin-configuration\tquery\tsidecar\tconductor
";
    const ROLES: &str = "name\tkind\ncustody\trole\noms\trole\nreporting\trole\nstreet\tcomponent\nsidecar\tcomponent\n";

    fn contract() -> Contract {
        Contract::parse(TOPICS, ROLES).unwrap()
    }

    fn roles(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn a_role_holds_exactly_the_rows_naming_it() {
        let g = contract().grants_for(&roles(&["custody"])).unwrap();
        assert!(g.may_publish("platform.street.command.record-holding"));
        assert!(g.may_publish("platform.custody.snaptrade-1.event.sync-status"));
        assert!(!g.may_publish("platform.street.query.list-positions"));
        assert!(!g.may_subscribe("platform.street.event.position-updated"));
    }

    #[test]
    fn several_roles_hold_the_union_of_theirs() {
        let g = contract()
            .grants_for(&roles(&["custody", "reporting"]))
            .unwrap();
        assert!(g.may_publish("platform.street.command.record-holding"));
        assert!(g.may_publish("platform.street.query.list-positions"));
        assert!(g.may_subscribe("platform.street.event.position-updated"));
    }

    #[test]
    fn a_role_no_row_names_holds_nothing_and_so_does_no_role() {
        let oms = contract().grants_for(&roles(&["oms"])).unwrap();
        assert!(oms.publish.is_empty() && oms.subscribe.is_empty());
        let none = contract().grants_for(&[]).unwrap();
        assert!(none.publish.is_empty() && none.subscribe.is_empty());
    }

    #[test]
    fn a_name_that_is_not_a_role_is_refused_not_granted_nothing() {
        let misspelt = contract().grants_for(&roles(&["custdy"])).unwrap_err();
        assert!(misspelt.contains("not a role"), "{misspelt}");
        let component = contract()
            .grants_for(&roles(&["custody", "street"]))
            .unwrap_err();
        assert!(component.contains("components"), "{component}");
    }

    #[test]
    fn a_component_holds_its_own_rows() {
        let street = contract().component("street");
        assert!(street.may_publish("platform.street.event.position-updated"));
        assert!(street.may_subscribe("platform.street.command.record-holding"));
        let sidecar = contract().component("sidecar");
        assert_eq!(
            sidecar.publish,
            vec!["platform.config.query.plugin-configuration"]
        );
    }

    #[test]
    fn the_compiled_in_contract_gives_custody_what_w2_names() {
        // Against the contract this runtime was built with, so a revision of
        // it that took custody's holdings rows away fails here first.
        let custody = Contract::embedded()
            .grants_for(&roles(&["custody"]))
            .unwrap();
        assert!(custody.may_publish("platform.street.command.record-holding"));
        assert!(custody.may_publish("platform.street.command.record-statement"));
        assert!(Contract::embedded().is_component("dashboard"));
        assert!(
            !Contract::embedded().is_role("admin"),
            "decisions/020: admin is no role"
        );
    }

    #[test]
    fn a_subscription_may_not_be_widened_past_its_grant() {
        // Granted one instance's status topics; asking for every instance's
        // must be refused, or the grant means nothing.
        let g = Grants {
            publish: vec![],
            subscribe: vec!["platform.custody.snaptrade-1.event.sync-status".into()],
        };
        assert!(g.may_subscribe("platform.custody.snaptrade-1.event.sync-status"));
        assert!(!g.may_subscribe("platform.custody.*.event.sync-status"));
        assert!(!g.may_subscribe("platform.custody.**"));
    }

    #[test]
    fn a_subscription_may_be_narrowed_inside_its_grant() {
        let g = Grants {
            publish: vec![],
            subscribe: vec!["platform.street.**".into()],
        };
        assert!(g.may_subscribe("platform.street.**"));
        // Narrower than the grant, in both concrete and wildcard form.
        assert!(g.may_subscribe("platform.street.event.position-updated"));
        assert!(g.may_subscribe("platform.street.event.*"));
    }

    #[test]
    fn a_wildcard_grant_does_not_leak_into_a_neighbouring_prefix() {
        let g = Grants {
            publish: vec![],
            subscribe: vec!["platform.street.**".into()],
        };
        // Sharing a string prefix is not sharing a topic prefix.
        assert!(!g.may_subscribe("platform.streetish.*"));
        assert!(!g.may_subscribe("platform.reference.**"));
    }
}
