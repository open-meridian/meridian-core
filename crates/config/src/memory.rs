//! The configuration store in memory, for tests and for a deployment with no
//! database configured. Its atomic operations hold one lock across the check
//! and the write, which is what makes them atomic.

use std::sync::Mutex;

use meridian_domain::v1::{
    AccessGroup, AccountGroup, AccountRecord, ExternalAccountLink, Permission, PluginLaunch,
    PluginLaunchState, PluginVersion, SignInRecord, UserGroup,
};

use meridian_pb::v1::SettingDeclaration;

use crate::store::{
    group_change, known_plugins, permission_change, redaction_note, redeclared, AccessChangeRecord,
    Author, ChangeKind, Ending, Held, KnownPlugin, LastChange, Result, SettingChange,
    SettingChangeRecord, SettingsAuthor, Snapshot, Store, StoredSetting, Withdrawal,
    REDACTED_BY_REDECLARATION, REDACTED_BY_SEALING,
};
use crate::DEPLOYMENT_ADMIN;

pub struct MemoryStore {
    state: Mutex<Snapshot>,
    /// Each settings change's own record (W6.11), as the Postgres store keeps
    /// them, so the rule is tested on both.
    changes: Mutex<Vec<SettingChangeRecord>>,
    /// Each grant change's own record (W6.7, W6.8), likewise.
    access_changes: Mutex<Vec<AccessChangeRecord>>,
}

impl MemoryStore {
    /// Starts as a fresh deployment does: deployment admin, All plugins
    /// (admin) and All accounts exist, and nobody holds any of them.
    pub fn new() -> Self {
        let mut snapshot = Snapshot::default();
        snapshot.records.access_groups.extend([
            crate::deployment_admin(),
            crate::service::all_plugins_admin(),
        ]);
        snapshot
            .records
            .account_groups
            .push(crate::service::all_accounts());
        Self {
            state: Mutex::new(snapshot),
            changes: Mutex::new(Vec::new()),
            access_changes: Mutex::new(Vec::new()),
        }
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

fn upsert<T: Clone>(items: &mut Vec<T>, item: &T, same: impl Fn(&T) -> bool) {
    match items.iter_mut().find(|existing| same(existing)) {
        Some(existing) => *existing = item.clone(),
        None => items.push(item.clone()),
    }
}

/// A record a store writes itself, naming no person: a clear a
/// re-declaration made, or a redaction.
fn unauthored(
    plugin_instance_id: &str,
    name: &str,
    kind: ChangeKind,
    at_ns: i64,
    note: String,
) -> SettingChangeRecord {
    SettingChangeRecord {
        plugin_instance_id: plugin_instance_id.to_string(),
        name: name.to_string(),
        kind,
        value: None,
        secret: kind == ChangeKind::Redacted,
        by: String::new(),
        delegation: String::new(),
        at_ns,
        backfilled: false,
        note,
    }
}

/// Blank every value a setting's records hold, and record the redaction, as
/// the Postgres store does; a record's place, from 1, stands for its id.
fn redact(
    journal: &mut Vec<SettingChangeRecord>,
    plugin_instance_id: &str,
    name: &str,
    at_ns: i64,
    why: &str,
) {
    let mut ids = Vec::new();
    for (at, change) in journal.iter_mut().enumerate() {
        if change.plugin_instance_id == plugin_instance_id
            && change.name == name
            && change.value.is_some()
        {
            change.value = None;
            ids.push(at as i64 + 1);
        }
    }
    if !ids.is_empty() {
        journal.push(unauthored(
            plugin_instance_id,
            name,
            ChangeKind::Redacted,
            at_ns,
            redaction_note(&ids, why),
        ));
    }
}

impl Store for MemoryStore {
    fn snapshot(&self) -> Result<Snapshot> {
        let mut snapshot = self.state.lock().expect("store lock poisoned").clone();
        snapshot.records.known_plugins = known_plugins(&snapshot.plugins);
        for change in self.changes.lock().expect("store lock poisoned").iter() {
            if matches!(change.kind, ChangeKind::Set | ChangeKind::Cleared) {
                snapshot.settings_changed.insert(
                    change.plugin_instance_id.clone(),
                    LastChange {
                        by: change.by.clone(),
                        at_ns: change.at_ns,
                    },
                );
            }
        }
        Ok(snapshot)
    }

    fn put_account(&self, account: &AccountRecord) -> Result<()> {
        let mut state = self.state.lock().expect("store lock poisoned");
        upsert(&mut state.records.accounts, account, |a| {
            a.account_id == account.account_id
        });
        Ok(())
    }

    fn put_user_group(&self, group: &UserGroup) -> Result<()> {
        let mut state = self.state.lock().expect("store lock poisoned");
        upsert(&mut state.records.user_groups, group, |g| {
            g.user_group_id == group.user_group_id
        });
        Ok(())
    }

    fn put_account_group(&self, group: &AccountGroup) -> Result<()> {
        let mut state = self.state.lock().expect("store lock poisoned");
        upsert(&mut state.records.account_groups, group, |g| {
            g.account_group_id == group.account_group_id
        });
        Ok(())
    }

    fn put_access_group(&self, group: &AccessGroup, author: &Author, at_ns: i64) -> Result<()> {
        let mut state = self.state.lock().expect("store lock poisoned");
        let before = state
            .records
            .access_groups
            .iter()
            .find(|g| g.access_group_id == group.access_group_id)
            .cloned();
        upsert(&mut state.records.access_groups, group, |g| {
            g.access_group_id == group.access_group_id
        });
        if let Some(record) = group_change(before.as_ref(), group, author, at_ns) {
            self.access_changes
                .lock()
                .expect("store lock poisoned")
                .push(record);
        }
        Ok(())
    }

    fn put_link(&self, link: &ExternalAccountLink) -> Result<()> {
        let mut state = self.state.lock().expect("store lock poisoned");
        state.links.retain(|l| {
            !(l.plugin_instance_id == link.plugin_instance_id
                && l.external_account_id == link.external_account_id)
        });
        if !link.account_id.is_empty() {
            state.links.push(link.clone());
        }
        Ok(())
    }

    fn put_account_and_link(
        &self,
        account: &AccountRecord,
        link: &ExternalAccountLink,
    ) -> Result<()> {
        let mut state = self.state.lock().expect("store lock poisoned");
        upsert(&mut state.records.accounts, account, |a| {
            a.account_id == account.account_id
        });
        state.links.retain(|l| {
            !(l.plugin_instance_id == link.plugin_instance_id
                && l.external_account_id == link.external_account_id)
        });
        state.links.push(link.clone());
        Ok(())
    }

    fn add_permission(&self, permission: &Permission, author: &Author, at_ns: i64) -> Result<()> {
        let mut state = self.state.lock().expect("store lock poisoned");
        state.records.permissions.push(permission.clone());
        self.access_changes
            .lock()
            .expect("store lock poisoned")
            .push(permission_change(permission, true, author, at_ns));
        Ok(())
    }

    fn withdraw_permission(
        &self,
        permission_id: &str,
        author: &Author,
        at_ns: i64,
    ) -> Result<Withdrawal> {
        let mut state = self.state.lock().expect("store lock poisoned");
        let permissions = &mut state.records.permissions;
        let Some(at) = permissions
            .iter()
            .position(|p| p.permission_id == permission_id)
        else {
            return Ok(Withdrawal::Unknown);
        };
        let admins = permissions
            .iter()
            .filter(|p| p.access_group_id == DEPLOYMENT_ADMIN)
            .count();
        if permissions[at].access_group_id == DEPLOYMENT_ADMIN && admins == 1 {
            return Ok(Withdrawal::LastAdmin);
        }
        let withdrawn = permissions.remove(at);
        self.access_changes
            .lock()
            .expect("store lock poisoned")
            .push(permission_change(&withdrawn, false, author, at_ns));
        Ok(Withdrawal::Withdrawn)
    }

    fn install_first_admin(
        &self,
        group: &UserGroup,
        permissions: &[Permission],
        author: &Author,
        at_ns: i64,
    ) -> Result<bool> {
        let mut state = self.state.lock().expect("store lock poisoned");
        if state
            .records
            .permissions
            .iter()
            .any(|p| p.access_group_id == DEPLOYMENT_ADMIN)
        {
            return Ok(false);
        }
        state.records.user_groups.push(group.clone());
        state
            .records
            .permissions
            .extend(permissions.iter().cloned());
        self.access_changes
            .lock()
            .expect("store lock poisoned")
            .extend(
                permissions
                    .iter()
                    .map(|permission| permission_change(permission, true, author, at_ns)),
            );
        Ok(true)
    }

    fn access_changes(&self, access_group_id: &str) -> Result<Vec<AccessChangeRecord>> {
        Ok(self
            .access_changes
            .lock()
            .expect("store lock poisoned")
            .iter()
            .filter(|change| change.access_group_id == access_group_id)
            .cloned()
            .collect())
    }

    fn record_sign_in(&self, record: &SignInRecord) -> Result<()> {
        let mut state = self.state.lock().expect("store lock poisoned");
        upsert(&mut state.records.people, record, |p| {
            p.subject == record.subject
        });
        Ok(())
    }

    fn record_plugin(&self, plugin: &KnownPlugin) -> Result<()> {
        let mut state = self.state.lock().expect("store lock poisoned");
        upsert(&mut state.plugins, plugin, |p| {
            p.plugin_instance_id == plugin.plugin_instance_id
        });
        Ok(())
    }

    fn record_declared_settings(
        &self,
        plugin_instance_id: &str,
        declared: &[SettingDeclaration],
        at_ns: i64,
    ) -> Result<Vec<String>> {
        let mut state = self.state.lock().expect("store lock poisoned");
        let mut journal = self.changes.lock().expect("store lock poisoned");
        let before = state
            .declared_settings
            .insert(plugin_instance_id.to_string(), declared.to_vec())
            .unwrap_or_default();
        let mut cleared = Vec::new();
        for now in declared {
            let held = state
                .settings
                .iter()
                .any(|s| s.plugin_instance_id == plugin_instance_id && s.name == now.name);
            let earlier = before.iter().find(|d| d.name == now.name);
            if let (true, Some(why)) = (held, redeclared(earlier, now)) {
                state.settings.retain(|s| {
                    !(s.plugin_instance_id == plugin_instance_id && s.name == now.name)
                });
                journal.push(unauthored(
                    plugin_instance_id,
                    &now.name,
                    ChangeKind::Cleared,
                    at_ns,
                    why,
                ));
                cleared.push(now.name.clone());
            }
            if now.secret {
                redact(
                    &mut journal,
                    plugin_instance_id,
                    &now.name,
                    at_ns,
                    REDACTED_BY_REDECLARATION,
                );
            }
        }
        Ok(cleared)
    }

    fn put_plugin_settings(
        &self,
        plugin_instance_id: &str,
        changes: &[SettingChange],
        author: &SettingsAuthor,
        at_ns: i64,
    ) -> Result<()> {
        let mut state = self.state.lock().expect("store lock poisoned");
        let mut journal = self.changes.lock().expect("store lock poisoned");
        for change in changes {
            state.settings.retain(|held| {
                !(held.plugin_instance_id == plugin_instance_id && held.name == change.name)
            });
            if let Some(held) = &change.held {
                state.settings.push(StoredSetting {
                    plugin_instance_id: plugin_instance_id.to_string(),
                    name: change.name.clone(),
                    held: held.clone(),
                    set_by: author.by.clone(),
                    set_at_ns: at_ns,
                });
            }
            journal.push(SettingChangeRecord {
                plugin_instance_id: plugin_instance_id.to_string(),
                name: change.name.clone(),
                kind: if change.held.is_some() {
                    ChangeKind::Set
                } else {
                    ChangeKind::Cleared
                },
                value: match &change.held {
                    Some(Held::Plain(value)) => Some(value.clone()),
                    _ => None,
                },
                secret: matches!(change.held, Some(Held::Sealed(_))),
                by: author.by.clone(),
                delegation: author.delegation.clone(),
                at_ns,
                backfilled: false,
                note: String::new(),
            });
            if matches!(change.held, Some(Held::Sealed(_))) {
                redact(
                    &mut journal,
                    plugin_instance_id,
                    &change.name,
                    at_ns,
                    REDACTED_BY_SEALING,
                );
            }
        }
        Ok(())
    }

    fn plugin_setting_changes(&self, plugin_instance_id: &str) -> Result<Vec<SettingChangeRecord>> {
        Ok(self
            .changes
            .lock()
            .expect("store lock poisoned")
            .iter()
            .filter(|change| change.plugin_instance_id == plugin_instance_id)
            .cloned()
            .collect())
    }

    fn record_plugin_version(&self, version: &PluginVersion) -> Result<bool> {
        let mut state = self.state.lock().expect("store lock poisoned");
        let metadata = version.metadata.clone().unwrap_or_default();
        let recorded = state.catalogue.versions.iter().any(|held| {
            held.metadata
                .as_ref()
                .is_some_and(|m| m.name == metadata.name && m.version == metadata.version)
        });
        if recorded {
            return Ok(false);
        }
        state.catalogue.versions.push(version.clone());
        Ok(true)
    }

    fn begin_launch(&self, launch: &PluginLaunch) -> Result<bool> {
        let mut state = self.state.lock().expect("store lock poisoned");
        let live = state.catalogue.launches.iter().any(|held| {
            held.instance_id == launch.instance_id
                && held.state == PluginLaunchState::Launched as i32
        });
        if live {
            return Ok(false);
        }
        state.catalogue.launches.push(launch.clone());
        Ok(true)
    }

    fn end_launch(&self, instance_id: &str, ending: &Ending) -> Result<Option<PluginLaunch>> {
        let mut state = self.state.lock().expect("store lock poisoned");
        let live = state.catalogue.launches.iter_mut().find(|held| {
            held.instance_id == instance_id && held.state == PluginLaunchState::Launched as i32
        });
        Ok(live.map(|launch| {
            *launch = ending.applied_to(launch);
            launch.clone()
        }))
    }
}
