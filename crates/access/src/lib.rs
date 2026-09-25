//! Who may reach what, evaluated from the records a deployment admin authors.
//!
//! Pure functions over [`AccessRecords`], and nothing else: no store, no bus,
//! no clock. The conductor uses them to derive a plugin's account scope and
//! access table; the dashboard uses them to evaluate a person at sign-in. Both
//! link this crate and neither links the other's store, which is why it is its
//! own crate rather than a module of the configuration store's.
//!
//! # The rules, as the spec states them
//!
//! A person's access is the union of every permission whose user group they
//! belong to, and it is combined **permission by permission, never dimension
//! by dimension**. Write on one account group and read on another is write on
//! the first and read on the second. Folding accounts and levels separately
//! would make it write on both, and that is the mistake this crate exists to
//! make impossible: each permission contributes only its own accounts, at its
//! own level, to the plugin and tag its entry names.
//!
//! `write` includes `read`: every account a person may write is also one they
//! may read.
//!
//! A closed account stays readable and is never writable. Its history remains,
//! and nothing may change it.
//!
//! An account in no account group is reached by no permission, and so by
//! nobody but deployment admins, whose built-in access group reaches every
//! account without naming a group.
//!
//! There are no deny rules. Access only adds up.

use std::collections::{BTreeMap, BTreeSet};

use meridian_domain::v1::{
    AccessLevel, AccessRecords, AccountRecord, AccountState, Permission, UserGroup,
};
use meridian_pb::v1::{PersonAccess, PluginAccessReply, TagAccess, UserGroupAccess};

/// The built-in access group. Holds the dashboard's own capabilities and
/// reaches every account; cannot be edited, deleted, or left without a
/// permission.
pub const DEPLOYMENT_ADMIN: &str = "deployment-admin";

/// The login of an account this deployment holds itself (decisions/018).
///
/// Here because this is where a login is matched: the dashboard signs a
/// person in as this, and first run names the first administrator by it,
/// and when those were two `format!`s in two crates the administrator the
/// wizard named held a permission for `ada` and signed in as `local|ada`.
/// Lowercased, as the account's name is, so `Ada` and `ada` are one person.
pub fn local_login(name: &str) -> String {
    format!("local|{}", name.trim().to_lowercase())
}

/// The accounts reachable at each level.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Levels {
    pub read: BTreeSet<String>,
    pub write: BTreeSet<String>,
}

impl Levels {
    fn add(&mut self, other: &Levels) {
        self.read.extend(other.read.iter().cloned());
        self.write.extend(other.write.iter().cloned());
    }

    pub fn is_empty(&self) -> bool {
        self.read.is_empty() && self.write.is_empty()
    }
}

/// Plugin instance, then tag, then the accounts at each level.
pub type PluginLevels = BTreeMap<String, BTreeMap<String, Levels>>;

/// One person's access, as the dashboard evaluates it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Access {
    /// Holds a permission to [`DEPLOYMENT_ADMIN`].
    pub deployment_admin: bool,

    pub user_group_ids: BTreeSet<String>,

    pub plugins: PluginLevels,
}

impl Access {
    /// What this person holds on one plugin, as the assertion carries it.
    pub fn on_plugin(&self, plugin_instance_id: &str) -> Vec<TagAccess> {
        self.plugins
            .get(plugin_instance_id)
            .map(tag_access)
            .unwrap_or_default()
    }
}

/// The user groups a person belongs to: one of their logins, or at sign-in in
/// one of their directory groups.
pub fn user_groups_of<'a>(
    records: &'a AccessRecords,
    subject: &str,
    directory_groups: &[String],
) -> Vec<&'a UserGroup> {
    records
        .user_groups
        .iter()
        .filter(|group| {
            group.logins.iter().any(|login| login == subject)
                || group
                    .directory_groups
                    .iter()
                    .any(|named| directory_groups.contains(named))
        })
        .collect()
}

/// A person's access, from who they are and the directory groups they
/// presented at this sign-in.
pub fn person_access(
    records: &AccessRecords,
    subject: &str,
    directory_groups: &[String],
) -> Access {
    let groups: BTreeSet<String> = user_groups_of(records, subject, directory_groups)
        .into_iter()
        .map(|group| group.user_group_id.clone())
        .collect();
    access_of_groups(records, &groups)
}

/// Access held through a set of user groups.
fn access_of_groups(records: &AccessRecords, user_group_ids: &BTreeSet<String>) -> Access {
    let mut access = Access {
        user_group_ids: user_group_ids.clone(),
        ..Access::default()
    };
    for permission in records
        .permissions
        .iter()
        .filter(|permission| user_group_ids.contains(&permission.user_group_id))
    {
        if permission.access_group_id == DEPLOYMENT_ADMIN {
            access.deployment_admin = true;
            continue;
        }
        fold(records, permission, &mut access.plugins);
    }
    access
}

/// One permission's contribution: its own accounts, at its own level, to the
/// plugin and tag each entry names. Nothing crosses from one permission to
/// another.
fn fold(records: &AccessRecords, permission: &Permission, into: &mut PluginLevels) {
    let Some(access_group) = records
        .access_groups
        .iter()
        .find(|group| group.access_group_id == permission.access_group_id)
    else {
        return;
    };
    let accounts = accounts_of_group(records, &permission.account_group_id);

    for entry in &access_group.entries {
        let mut levels = Levels::default();
        for account in &accounts {
            levels.read.insert(account.account_id.clone());
            let writable = entry.level == AccessLevel::Write as i32
                && account.state != AccountState::Closed as i32;
            if writable {
                levels.write.insert(account.account_id.clone());
            }
        }
        into.entry(entry.plugin_instance_id.clone())
            .or_default()
            .entry(entry.tag.clone())
            .or_default()
            .add(&levels);
    }
}

/// The accounts an account group lists that exist. An identifier naming no
/// account reaches nothing, rather than something that may be created later.
fn accounts_of_group<'a>(
    records: &'a AccessRecords,
    account_group_id: &str,
) -> Vec<&'a AccountRecord> {
    let Some(group) = records
        .account_groups
        .iter()
        .find(|group| group.account_group_id == account_group_id)
    else {
        return Vec::new();
    };
    group
        .account_ids
        .iter()
        .filter_map(|id| {
            records
                .accounts
                .iter()
                .find(|account| &account.account_id == id)
        })
        .collect()
}

/// A plugin's account scope: every account anybody may read through it, and
/// every account anybody may write through it, across all its tags.
///
/// Derived from every permission, not from people: the deployment knows no
/// directory, so it cannot ask who is in a group, only what the groups hold.
pub fn plugin_scope(records: &AccessRecords, plugin_instance_id: &str) -> Levels {
    let mut all = PluginLevels::new();
    for permission in &records.permissions {
        if permission.access_group_id != DEPLOYMENT_ADMIN {
            fold(records, permission, &mut all);
        }
    }
    let mut scope = Levels::default();
    for levels in all
        .get(plugin_instance_id)
        .into_iter()
        .flat_map(|tags| tags.values())
    {
        scope.add(levels);
    }
    scope
}

/// Who may use a plugin, for shaping its interface (W4.10): each user group
/// holding access to it, and each person who has signed in with access to it
/// through the groups they were in at their last sign-in.
pub fn plugin_access_table(records: &AccessRecords, plugin_instance_id: &str) -> PluginAccessReply {
    let user_groups = records
        .user_groups
        .iter()
        .filter_map(|group| {
            let held = access_of_groups(records, &BTreeSet::from([group.user_group_id.clone()]));
            let access = held.on_plugin(plugin_instance_id);
            (!access.is_empty()).then(|| UserGroupAccess {
                user_group_id: group.user_group_id.clone(),
                name: group.name.clone(),
                access,
            })
        })
        .collect();

    let people = records
        .people
        .iter()
        .filter_map(|person| {
            let held = person_access(records, &person.subject, &person.directory_groups);
            let access = held.on_plugin(plugin_instance_id);
            (!access.is_empty()).then(|| PersonAccess {
                subject: person.subject.clone(),
                display_name: person.display_name.clone(),
                user_group_ids: held.user_group_ids.into_iter().collect(),
                last_signed_in_at_ns: person.signed_in_at_ns,
                access,
            })
        })
        .collect();

    PluginAccessReply {
        user_groups,
        people,
    }
}

/// Tag by tag, as the wire carries it.
pub fn tag_access(tags: &BTreeMap<String, Levels>) -> Vec<TagAccess> {
    tags.iter()
        .filter(|(_, levels)| !levels.is_empty())
        .map(|(tag, levels)| TagAccess {
            tag: tag.clone(),
            read_account_ids: levels.read.iter().cloned().collect(),
            write_account_ids: levels.write.iter().cloned().collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests;
