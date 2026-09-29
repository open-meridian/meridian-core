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
//! own level, to the plugin its entry names.
//!
//! Per plugin, and nothing finer: a person's access to a plugin is `read` or
//! `write`, the same for every plugin, and a plugin names no parts of itself
//! for access (decisions/026).
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
    AccessLevel, AccessRecords, AccountRecord, AccountState, ExternalAccountLink, Permission,
    UserGroup,
};
use meridian_pb::v1::{PersonAccess, PluginAccessReply, UserGroupAccess};

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

    /// The accounts that may be read, as the wire lists them.
    pub fn read_account_ids(&self) -> Vec<String> {
        self.read.iter().cloned().collect()
    }

    /// The accounts that may be written, as the wire lists them.
    pub fn write_account_ids(&self) -> Vec<String> {
        self.write.iter().cloned().collect()
    }
}

/// Plugin instance, then the accounts at each level.
pub type PluginLevels = BTreeMap<String, Levels>;

/// One person's access, as the dashboard evaluates it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Access {
    /// Holds a permission to [`DEPLOYMENT_ADMIN`].
    pub deployment_admin: bool,

    pub user_group_ids: BTreeSet<String>,

    pub plugins: PluginLevels,
}

impl Access {
    /// What this person holds on one plugin, as the assertion carries it:
    /// the accounts they may read and the accounts they may write through it.
    pub fn on_plugin(&self, plugin_instance_id: &str) -> Levels {
        self.plugins
            .get(plugin_instance_id)
            .cloned()
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
/// plugin each entry names. Nothing crosses from one permission to another,
/// and two entries naming one plugin come to the higher of their levels,
/// which is what the union of them is.
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
/// every account anybody may write through it; and every account one of its
/// external accounts is linked to.
///
/// Derived from every permission, not from people: the deployment knows no
/// directory, so it cannot ask who is in a group, only what the groups hold.
/// A link is the plugin's right to the one account it names while it stands:
/// the plugin's role grants it the store, the link the account (W4.11, W6.4).
pub fn plugin_scope(
    records: &AccessRecords,
    links: &[ExternalAccountLink],
    plugin_instance_id: &str,
) -> Levels {
    let mut all = PluginLevels::new();
    for permission in &records.permissions {
        if permission.access_group_id != DEPLOYMENT_ADMIN {
            fold(records, permission, &mut all);
        }
    }
    let mut scope = all.remove(plugin_instance_id).unwrap_or_default();
    for link in links
        .iter()
        .filter(|link| link.plugin_instance_id == plugin_instance_id)
    {
        let Some(account) = records
            .accounts
            .iter()
            .find(|account| account.account_id == link.account_id)
        else {
            continue;
        };
        scope.read.insert(account.account_id.clone());
        if account.state != AccountState::Closed as i32 {
            scope.write.insert(account.account_id.clone());
        }
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
                read_account_ids: access.read_account_ids(),
                write_account_ids: access.write_account_ids(),
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
                read_account_ids: access.read_account_ids(),
                write_account_ids: access.write_account_ids(),
            })
        })
        .collect();

    PluginAccessReply {
        user_groups,
        people,
    }
}

#[cfg(test)]
mod tests;
