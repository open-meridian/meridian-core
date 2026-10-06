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
//! **Per role of a plugin** (contract v15, decisions/033): an access entry
//! names a plugin, one role it holds and a level, `read`, `write` or `admin`,
//! the same for every plugin and role. The roles are the deployment's fixed
//! list, so a plugin still names no parts of itself for access (decisions/026,
//! 027). A plugin holding no role is granted as a whole, its entries naming
//! none. An entry that does not match the plugin's roles as the records know
//! them -- a role the plugin no longer holds, a role-less entry on a plugin
//! that now holds roles -- holds nothing, and is never read as every role
//! ([`entry_holds`]). Write on one role and read on another is exactly that:
//! a role's grant reaches nothing of another.
//!
//! **Levels** (W6.7, the product owner's rulings of 2026-09-30). A person may
//! hold `admin` on a plugin and, independently, one data level, the higher
//! one granted: `write` includes `read`, so every account a person may write
//! is also one they may read. The levels are agnostic of accounts -- holding
//! `write` gives Open, whatever the account group -- and the account groups
//! bound what each reaches. `admin` configures the plugin and reaches no
//! account.
//!
//! **A session carries one button** (W6.9): Manage at `admin` with no account,
//! Open at `write` with the read set and the write set, View at `read` with
//! the read set alone, each set the union over the plugin's roles; and from
//! v15 the person's level on each role within that button
//! ([`PluginHeld::session`]). Nothing is held for a button the person does not
//! hold.
//!
//! A closed account stays readable and is never writable. Its history remains,
//! and nothing may change it.
//!
//! **Built in** are deployment admin, which holds the dashboard's own
//! capabilities and no plugin and no account; All plugins (admin), which
//! grants `admin` on every role of every plugin, those launched later and
//! roles gained later included, and no account; and All accounts, an account group holding every account the
//! records hold, those opened later included. An account no group lists is
//! reached only by a permission naming All accounts, or by a plugin's link;
//! nobody reaches it by being an admin.
//!
//! There are no deny rules. Access only adds up.

use std::collections::{BTreeMap, BTreeSet};

use meridian_domain::v1::{
    AccessRecords, AccountRecord, AccountState, ExternalAccountLink, Permission, UserGroup,
};
pub use meridian_pb::v1::AccessLevel;
use meridian_pb::v1::{PersonAccess, PluginAccessReply, RoleAccess, UserGroupAccess};

/// The built-in access group of the dashboard's own capabilities. Reaches no
/// plugin and no account; cannot be edited, deleted, or left without a
/// permission.
pub const DEPLOYMENT_ADMIN: &str = "deployment-admin";

/// The built-in access group granting `admin` on every plugin, those launched
/// later included, and no account: All plugins (admin). First run and a claim
/// code link the deployment admins' user group to it (W6.2, W7.6); any
/// permission to it may be withdrawn.
pub const ALL_PLUGINS_ADMIN: &str = "all-plugins-admin";

/// The built-in account group holding every account: All accounts (W6.6).
pub const ALL_ACCOUNTS: &str = "all-accounts";

/// Whether an access group is one of the two built in.
pub fn is_built_in_access_group(access_group_id: &str) -> bool {
    access_group_id == DEPLOYMENT_ADMIN || access_group_id == ALL_PLUGINS_ADMIN
}

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

/// What a person holds on one role of a plugin, or on a plugin holding none
/// as a whole: whether they administer it, their data level, and the accounts
/// that level reaches. [`Access::held`] answers the same for a plugin, the
/// union over its roles.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Held {
    /// `admin`: configures the plugin, reaching no account.
    pub admin: bool,
    /// The data level, the higher one granted: `read` or `write`, or none.
    pub data: Option<AccessLevel>,
    /// The accounts the data level reaches, read and write.
    pub accounts: Levels,
}

impl Held {
    fn add(&mut self, other: &Held) {
        self.admin |= other.admin;
        self.data = higher(self.data, other.data);
        self.accounts.add(&other.accounts);
    }

    /// Whether this holds any level at all.
    pub fn holds_any(&self) -> bool {
        self.admin || self.data.is_some()
    }

    /// The levels held, as the home's buttons offer them, in their order:
    /// Manage for `admin`, Open for `write`, View for `read` -- a writer gets
    /// View too, a read-only way in (W6.9).
    pub fn levels(&self) -> Vec<AccessLevel> {
        let mut levels = Vec::new();
        if self.admin {
            levels.push(AccessLevel::Admin);
        }
        if self.data == Some(AccessLevel::Write) {
            levels.push(AccessLevel::Write);
        }
        if self.data.is_some() {
            levels.push(AccessLevel::Read);
        }
        levels
    }

    /// Whether a session may be opened at `level`.
    pub fn holds(&self, level: AccessLevel) -> bool {
        self.levels().contains(&level)
    }

    /// The accounts a session at `level` carries, cut to it, or None when
    /// the level is not held: under `admin` none, under `write` the read set
    /// and the write set, under `read` the read set alone (W6.9).
    pub fn session(&self, level: AccessLevel) -> Option<Levels> {
        if !self.holds(level) {
            return None;
        }
        Some(match level {
            AccessLevel::Write => self.accounts.clone(),
            AccessLevel::Read => Levels {
                read: self.accounts.read.clone(),
                write: BTreeSet::new(),
            },
            AccessLevel::Admin | AccessLevel::Unspecified => Levels::default(),
        })
    }
}

/// The higher of two data levels, write including read.
fn higher(one: Option<AccessLevel>, other: Option<AccessLevel>) -> Option<AccessLevel> {
    match (one, other) {
        (Some(AccessLevel::Write), _) | (_, Some(AccessLevel::Write)) => Some(AccessLevel::Write),
        (Some(AccessLevel::Read), _) | (_, Some(AccessLevel::Read)) => Some(AccessLevel::Read),
        _ => None,
    }
}

/// A level as the home's button names it, and as a request names it.
pub fn button(level: AccessLevel) -> &'static str {
    match level {
        AccessLevel::Admin => "Manage",
        AccessLevel::Write => "Open",
        AccessLevel::Read => "View",
        AccessLevel::Unspecified => "",
    }
}

/// A level as it travels in an address or a terminal's request: `admin`,
/// `write` or `read`.
pub fn level_name(level: AccessLevel) -> &'static str {
    match level {
        AccessLevel::Admin => "admin",
        AccessLevel::Write => "write",
        AccessLevel::Read => "read",
        AccessLevel::Unspecified => "",
    }
}

/// A level from its name or its button's, in any case; None for anything
/// else.
pub fn parse_level(named: &str) -> Option<AccessLevel> {
    match named.trim().to_ascii_lowercase().as_str() {
        "admin" | "manage" => Some(AccessLevel::Admin),
        "write" | "open" => Some(AccessLevel::Write),
        "read" | "view" => Some(AccessLevel::Read),
        _ => None,
    }
}

/// Plugin instance, then what is held on each of its roles.
pub type PluginLevels = BTreeMap<String, PluginHeld>;

/// What a person holds on one plugin, role by role (contract v15): keyed by
/// role, the empty role for a plugin holding none -- or one whose roles the
/// records do not know, from a conductor before v15, whose entries are read
/// as written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginHeld {
    pub roles: BTreeMap<String, Held>,
}

/// A person's level and accounts on one role within a session's button
/// (W6.9): what the claims' per-role entry carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleSession {
    pub role: String,
    pub level: AccessLevel,
    pub accounts: Levels,
}

/// A session at one button: the accounts it reaches, the union over the
/// roles, and each role's level and accounts within it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Session {
    pub accounts: Levels,
    pub roles: Vec<RoleSession>,
}

impl PluginHeld {
    fn add(&mut self, role: &str, held: &Held) {
        self.roles.entry(role.to_string()).or_default().add(held);
    }

    /// The union over the roles: what the home's buttons are offered by, and
    /// what a plugin reading no role is served (W6.9).
    pub fn union(&self) -> Held {
        let mut union = Held::default();
        for held in self.roles.values() {
            union.add(held);
        }
        union
    }

    /// Whether they administer this role.
    pub fn administers(&self, role: &str) -> bool {
        self.roles.get(role).is_some_and(|held| held.admin)
    }

    /// Whether they administer every one of `roles`: what a setting serving
    /// several roles, and every act on the plugin as a whole, needs
    /// (decisions/033, point 2). For no role, the plugin as a whole.
    pub fn administers_every(&self, roles: &[String]) -> bool {
        if roles.is_empty() {
            return self.administers("");
        }
        roles.iter().all(|role| self.administers(role))
    }

    /// Whether they administer any of `roles` (any role at all for none).
    pub fn administers_any_of(&self, roles: &[String]) -> bool {
        if roles.is_empty() {
            return self.roles.values().any(|held| held.admin);
        }
        roles.iter().any(|role| self.administers(role))
    }

    /// The session at `level`'s button, or None when it is not held: the
    /// union of the accounts cut to it, and one entry per role held within
    /// it -- under Manage each role administered at `admin` with no account,
    /// under Open each role with a data level at that level, under View each
    /// at `read` with its read accounts (W6.9). A role-less plugin's entry
    /// is the empty role's, which the claims do not carry.
    pub fn session(&self, level: AccessLevel) -> Option<Session> {
        let accounts = self.union().session(level)?;
        let roles = self
            .roles
            .iter()
            .filter_map(|(role, held)| {
                let (at, accounts) = match level {
                    AccessLevel::Admin if held.admin => (AccessLevel::Admin, Levels::default()),
                    AccessLevel::Write => match held.data {
                        Some(AccessLevel::Write) => (AccessLevel::Write, held.accounts.clone()),
                        Some(_) => (
                            AccessLevel::Read,
                            Levels {
                                read: held.accounts.read.clone(),
                                write: BTreeSet::new(),
                            },
                        ),
                        None => return None,
                    },
                    AccessLevel::Read if held.data.is_some() => (
                        AccessLevel::Read,
                        Levels {
                            read: held.accounts.read.clone(),
                            write: BTreeSet::new(),
                        },
                    ),
                    _ => return None,
                };
                Some(RoleSession {
                    role: role.clone(),
                    level: at,
                    accounts,
                })
            })
            .collect();
        Some(Session { accounts, roles })
    }
}

/// The roles the records know a plugin holds, as its sidecar last reported
/// them (W6.1, contract v15); None for a plugin they do not list, which only
/// a conductor before v15 answers.
pub fn known_roles<'a>(records: &'a AccessRecords, plugin_instance_id: &str) -> Option<&'a [String]> {
    records
        .known_plugins
        .iter()
        .find(|known| known.plugin_instance_id == plugin_instance_id)
        .map(|known| known.roles.as_slice())
}

/// Whether an entry naming `role` on a plugin holds anything (W6.7; the
/// plan's Q4): a role the plugin holds, or no role on a plugin holding none.
/// An entry naming a role the plugin no longer holds, or none on a plugin
/// that now holds roles, holds nothing and is flagged, never read as every
/// role. A plugin the records do not list -- a conductor before v15 -- is
/// read as written.
pub fn entry_holds(records: &AccessRecords, plugin_instance_id: &str, role: &str) -> bool {
    match known_roles(records, plugin_instance_id) {
        None => true,
        Some([]) => role.is_empty(),
        Some(roles) => !role.is_empty() && roles.iter().any(|held| held == role),
    }
}

/// The roles a grant on a plugin is held on: those the records know, the
/// empty role for a plugin holding none or one they do not list.
fn roles_of(known: &BTreeMap<String, Vec<String>>, plugin_instance_id: &str) -> Vec<String> {
    match known.get(plugin_instance_id) {
        Some(roles) if !roles.is_empty() => roles.clone(),
        _ => vec![String::new()],
    }
}

/// One person's access, as the dashboard evaluates it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Access {
    /// Holds a permission to [`DEPLOYMENT_ADMIN`]: the dashboard's own
    /// capabilities, and nothing on any plugin by that.
    pub deployment_admin: bool,

    /// Holds a permission to [`ALL_PLUGINS_ADMIN`]: `admin` on every role of
    /// every plugin, those launched later and roles gained later included.
    pub all_plugins_admin: bool,

    pub user_group_ids: BTreeSet<String>,

    /// What is held on each role of each plugin an access entry names.
    pub plugins: PluginLevels,

    /// Each known plugin's roles, as the records carried them (contract
    /// v15): what All plugins (admin) is held on, role by role.
    pub known_roles: BTreeMap<String, Vec<String>>,
}

impl Access {
    /// What this person holds on one plugin, the union over its roles, All
    /// plugins (admin) included: the home's buttons, and what a plugin
    /// reading no role is served.
    pub fn held(&self, plugin_instance_id: &str) -> Held {
        self.plugin(plugin_instance_id).union()
    }

    /// What this person holds on each role of one plugin, All plugins
    /// (admin) marking admin on every role it holds, or on the plugin as a
    /// whole where it holds none.
    pub fn plugin(&self, plugin_instance_id: &str) -> PluginHeld {
        let mut held = self
            .plugins
            .get(plugin_instance_id)
            .cloned()
            .unwrap_or_default();
        if self.all_plugins_admin {
            for role in roles_of(&self.known_roles, plugin_instance_id) {
                held.roles.entry(role).or_default().admin = true;
            }
        }
        held
    }

    /// The accounts this person's data level on one plugin reaches: what a
    /// session under `write` carries (use [`Held::session`] for a session).
    pub fn on_plugin(&self, plugin_instance_id: &str) -> Levels {
        self.held(plugin_instance_id).accounts
    }

    /// Whether they administer the plugin.
    pub fn administers(&self, plugin_instance_id: &str) -> bool {
        self.held(plugin_instance_id).admin
    }

    /// Whether they administer any plugin: every plugin through All plugins
    /// (admin), or one an entry names.
    pub fn administers_any(&self) -> bool {
        self.all_plugins_admin
            || self
                .plugins
                .values()
                .any(|plugin| plugin.roles.values().any(|held| held.admin))
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
        known_roles: records
            .known_plugins
            .iter()
            .map(|known| (known.plugin_instance_id.clone(), known.roles.clone()))
            .collect(),
        ..Access::default()
    };
    for permission in records
        .permissions
        .iter()
        .filter(|permission| user_group_ids.contains(&permission.user_group_id))
    {
        match permission.access_group_id.as_str() {
            DEPLOYMENT_ADMIN => access.deployment_admin = true,
            ALL_PLUGINS_ADMIN => access.all_plugins_admin = true,
            _ => fold(records, permission, &mut access.plugins),
        }
    }
    access
}

/// One permission's contribution: its own accounts, at its own level, to the
/// plugin and role each entry names. Nothing crosses from one permission to
/// another, nor from one role to another, and two entries naming one plugin
/// and role come to the higher of their levels, which is what the union of
/// them is. An `admin` entry marks the role administered and adds no
/// account. An entry that does not match the plugin's roles holds nothing
/// ([`entry_holds`]).
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
        let mut held = Held::default();
        match AccessLevel::try_from(entry.level) {
            Ok(AccessLevel::Admin) => held.admin = true,
            Ok(level @ (AccessLevel::Read | AccessLevel::Write)) => {
                held.data = Some(level);
                for account in &accounts {
                    held.accounts.read.insert(account.account_id.clone());
                    let writable =
                        level == AccessLevel::Write && account.state != AccountState::Closed as i32;
                    if writable {
                        held.accounts.write.insert(account.account_id.clone());
                    }
                }
            }
            // An entry naming no level holds nothing.
            _ => continue,
        }
        if !entry_holds(records, &entry.plugin_instance_id, &entry.role) {
            continue;
        }
        into.entry(entry.plugin_instance_id.clone())
            .or_default()
            .add(&entry.role, &held);
    }
}

/// The accounts an account group reaches that exist: every account the
/// records hold for All accounts, and for any other the ones it lists. An
/// identifier naming no account reaches nothing, rather than something that
/// may be created later.
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
    if group.built_in {
        return records.accounts.iter().collect();
    }
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

/// Every account the named account groups hold, All accounts as every
/// account the records hold: what a delegation narrowed to those groups may
/// reach (decisions/029), by the rule a permission reaches its accounts by.
pub fn accounts_in_groups<'a>(
    records: &AccessRecords,
    account_group_ids: impl IntoIterator<Item = &'a String>,
) -> BTreeSet<String> {
    account_group_ids
        .into_iter()
        .flat_map(|id| accounts_of_group(records, id))
        .map(|account| account.account_id.clone())
        .collect()
}

/// The account groups a person's permissions name: those a delegation of
/// theirs may be narrowed to (spec/clients-act-on-a-persons-delegation,
/// requirement 3). The built-in access groups name none.
pub fn account_groups_named(records: &AccessRecords, access: &Access) -> BTreeSet<String> {
    records
        .permissions
        .iter()
        .filter(|p| access.user_group_ids.contains(&p.user_group_id))
        .filter(|p| !is_built_in_access_group(&p.access_group_id))
        .filter(|p| !p.account_group_id.is_empty())
        .map(|p| p.account_group_id.clone())
        .collect()
}

/// A plugin's account scope: every account anybody may read through it, and
/// every account anybody may write through it, on any of its roles; and
/// every account one of its external accounts is linked to. `admin` adds
/// none.
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
        if !is_built_in_access_group(&permission.access_group_id) {
            fold(records, permission, &mut all);
        }
    }
    // One scope per plugin, whatever its roles: the union over them (W4.11,
    // decisions/033 point 5).
    let mut scope = all
        .remove(plugin_instance_id)
        .map(|held| held.union().accounts)
        .unwrap_or_default();
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
/// through the groups they were in at their last sign-in. Each carries its
/// read and write accounts, the union over the plugin's roles, and from v15
/// one entry per role of this plugin it holds a data level on, its accounts
/// as positions in its read accounts; none for `admin`, which the table does
/// not list, and none on a plugin holding no role.
pub fn plugin_access_table(records: &AccessRecords, plugin_instance_id: &str) -> PluginAccessReply {
    let user_groups = records
        .user_groups
        .iter()
        .filter_map(|group| {
            let held = access_of_groups(records, &BTreeSet::from([group.user_group_id.clone()]));
            let plugin = held.plugins.get(plugin_instance_id).cloned().unwrap_or_default();
            let access = plugin.union().accounts;
            (!access.is_empty()).then(|| UserGroupAccess {
                user_group_id: group.user_group_id.clone(),
                name: group.name.clone(),
                read_account_ids: access.read_account_ids(),
                write_account_ids: access.write_account_ids(),
                roles: table_roles(&plugin, &access),
            })
        })
        .collect();

    let people = records
        .people
        .iter()
        .filter_map(|person| {
            let held = person_access(records, &person.subject, &person.directory_groups);
            let plugin = held.plugins.get(plugin_instance_id).cloned().unwrap_or_default();
            let access = plugin.union().accounts;
            (!access.is_empty()).then(|| PersonAccess {
                subject: person.subject.clone(),
                display_name: person.display_name.clone(),
                user_group_ids: held.user_group_ids.into_iter().collect(),
                last_signed_in_at_ns: person.signed_in_at_ns,
                read_account_ids: access.read_account_ids(),
                write_account_ids: access.write_account_ids(),
                roles: table_roles(&plugin, &access),
            })
        })
        .collect();

    PluginAccessReply {
        user_groups,
        people,
    }
}

/// Each role's data level in the access table, its accounts as positions in
/// the union's read accounts (W4.10): only named roles, never the empty one
/// of a plugin holding none, and none for admin alone.
fn table_roles(plugin: &PluginHeld, union: &Levels) -> Vec<RoleAccess> {
    plugin
        .roles
        .iter()
        .filter(|(role, _)| !role.is_empty())
        .filter_map(|(role, held)| {
            let level = held.data?;
            Some(role_access(role, level, &held.accounts, union))
        })
        .collect()
}

/// One role's level and accounts, its accounts as positions in `union`'s
/// read accounts, sorted as the wire lists them (contract v15, the plan's
/// Q2): never repeated, so a role costs a few bytes an account.
pub fn role_access(role: &str, level: AccessLevel, accounts: &Levels, union: &Levels) -> RoleAccess {
    let positions = |of: &BTreeSet<String>| -> Vec<u32> {
        union
            .read
            .iter()
            .enumerate()
            .filter(|(_, account)| of.contains(*account))
            .map(|(at, _)| at as u32)
            .collect()
    };
    RoleAccess {
        role: role.to_string(),
        level: level as i32,
        read_positions: positions(&accounts.read),
        write_positions: positions(&accounts.write),
    }
}

#[cfg(test)]
mod tests;
