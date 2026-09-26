//! The plugins launched after install, as the broker learns them
//! (spec/the-local-plugin-registry, decision 3, ruled 2026-09-26).
//!
//! The launcher makes a Deployment for a plugin and nothing else
//! (decisions/019). Its sidecar needs a broker credential, and the broker has
//! to admit it with its roles' topics, without anything restarting. So the
//! broker's own process watches: it reads the Deployments the launcher
//! labelled, keeps one credential per instance in a Secret the chart made for
//! them -- adding one for a plugin that appeared, taking out one whose plugin
//! has gone -- and writes the broker a configuration admitting each, by a
//! bcrypt hash of its password, then has it reload.
//!
//! Everything here is derived from what exists: the Deployments, and the
//! credentials already in the Secret, which are reused rather than minted
//! again. A broker that restarts, or a watch that missed a launch, arrives at
//! the same answer.

use std::collections::BTreeMap;

use crate::broker::Instance;

/// The label the launcher sets on every plugin it launches, and nothing
/// else carries.
pub const LAUNCHED_SELECTOR: &str = "meridian.dev/launched=true";
/// The instance a launched plugin's Deployment is for.
pub const INSTANCE_LABEL: &str = "meridian.dev/instance";
/// Its roles, comma-separated, as the conductor approved them (W8.3).
pub const ROLES_ANNOTATION: &str = "meridian.dev/roles";

/// A plugin the launcher launched: which instance, holding which roles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launched {
    pub instance_id: String,
    pub roles: Vec<String>,
}

impl Launched {
    pub fn instance(&self) -> Instance {
        let roles: Vec<&str> = self.roles.iter().map(String::as_str).collect();
        Instance::plugin(&self.instance_id, &roles)
    }
}

/// An instance's name as a host label has it, which is all the launcher
/// makes, and all that can name a key here unambiguously.
fn is_instance(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// The launched plugins in a Deployment list, as the API returns one. A
/// Deployment with no instance label, or one that is not a name, is not a
/// plugin this admits.
pub fn from_deployments(list: &serde_json::Value) -> Vec<Launched> {
    let mut launched: Vec<Launched> = list["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|deployment| {
            let metadata = &deployment["metadata"];
            let instance_id = metadata["labels"][INSTANCE_LABEL].as_str()?;
            if !is_instance(instance_id) {
                return None;
            }
            let roles = metadata["annotations"][ROLES_ANNOTATION]
                .as_str()
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|role| !role.is_empty())
                .map(String::from)
                .collect();
            Some(Launched {
                instance_id: instance_id.to_string(),
                roles,
            })
        })
        .collect();
    launched.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
    launched.dedup_by(|a, b| a.instance_id == b.instance_id);
    launched
}

/// The Secret's key for an instance's password, and for the connection
/// string its sidecar reads. A dot cannot be in an instance's name, so no
/// instance's key can be another's.
pub fn password_key(instance_id: &str) -> String {
    format!("{instance_id}.password")
}
pub fn url_key(instance_id: &str) -> String {
    format!("{instance_id}.url")
}

/// What the Secret should hold, against what it does.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// Each launched instance's password, whether it was held or just made.
    pub passwords: BTreeMap<String, String>,
    /// Keys to set, and keys to remove (None), in one patch.
    pub changes: BTreeMap<String, Option<String>>,
}

/// Credentials for exactly the plugins launched: those held kept, one made
/// for each that has none, and each for a plugin that has gone taken out.
/// `broker` is where a sidecar reaches the broker, `host:port`.
pub fn reconcile(
    launched: &[Launched],
    held: &BTreeMap<String, String>,
    broker: &str,
    mut mint: impl FnMut() -> String,
) -> Reconciled {
    let mut reconciled = Reconciled::default();
    for plugin in launched {
        let instance = &plugin.instance_id;
        let password = match held.get(&password_key(instance)) {
            Some(password) if !password.is_empty() => password.clone(),
            _ => {
                let password = mint();
                reconciled
                    .changes
                    .insert(password_key(instance), Some(password.clone()));
                password
            }
        };
        let url = format!("nats://{instance}:{password}@{broker}");
        if held.get(&url_key(instance)) != Some(&url) {
            reconciled.changes.insert(url_key(instance), Some(url));
        }
        reconciled.passwords.insert(instance.clone(), password);
    }
    for key in held.keys() {
        let instance = key
            .strip_suffix(".password")
            .or_else(|| key.strip_suffix(".url"))
            .unwrap_or(key);
        if !reconciled.passwords.contains_key(instance) {
            reconciled.changes.insert(key.clone(), None);
        }
    }
    reconciled
}

/// The broker's configuration: the chart's instances, whose passwords it
/// reads from its environment, and each launched plugin, admitted by a hash
/// of its password. A launched plugin whose roles the contract refuses, or
/// whose instance the chart already runs, is left out and named, rather than
/// taking the rest of the broker's configuration down with it.
pub fn configuration(
    header: &str,
    contract: &meridian_sidecar::Contract,
    chart: &[Instance],
    launched: &[Launched],
    hashes: &BTreeMap<String, String>,
) -> Result<(String, Vec<String>), String> {
    use crate::broker::{instance_user, rendered, users, Password};
    let mut admitted = users(contract, chart)?;
    let mut left_out = Vec::new();
    for plugin in launched {
        let id = &plugin.instance_id;
        if admitted.iter().any(|user| &user.user == id) {
            left_out.push(format!("{id} is already one of the chart's instances"));
            continue;
        }
        let Some(hash) = hashes.get(id) else {
            left_out.push(format!("{id} has no credential yet"));
            continue;
        };
        match instance_user(contract, &plugin.instance(), Password::Bcrypt(hash.clone())) {
            Ok(user) => admitted.push(user),
            Err(refusal) => left_out.push(refusal),
        }
    }
    Ok((rendered(header, &admitted), left_out))
}

/// A password the broker will read as a string: a letter first, since its
/// configuration reads a leading digit as the start of a number (the chart's
/// passwords learned this), then 31 more.
pub fn mint() -> String {
    use rand::Rng;
    const LETTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    const ALPHANUMERIC: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut random = rand::rngs::OsRng;
    let mut password = String::with_capacity(32);
    password.push(LETTERS[random.gen_range(0..LETTERS.len())] as char);
    for _ in 0..31 {
        password.push(ALPHANUMERIC[random.gen_range(0..ALPHANUMERIC.len())] as char);
    }
    password
}

#[cfg(test)]
mod tests;
