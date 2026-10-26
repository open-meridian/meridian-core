//! The configuration store in memory, for tests and for a deployment with no
//! database configured. Its atomic operations hold one lock across the check
//! and the write, which is what makes them atomic.

use std::sync::Mutex;

use meridian_domain::v1::{
    AccessGroup, AccountGroup, AccountRecord, ExternalAccountLink, Hold, LinkableAccounts,
    MoveRecord, Permission, PluginArchive, PluginLaunch, PluginLaunchState, PluginVersion,
    SettingLastChange, SignInRecord, UserGroup,
};

use meridian_domain::v1::VenueCatalogue;
use meridian_pb::v1::{ConnectivityLink, SettingDeclaration, StoredSpan, VenueAccountMapping};

use crate::store::{
    archived_spans, group_change, known_plugins, permission_change, redaction_note, redeclared,
    AccessChangeRecord, Author, ChangeKind, ConnectivityLinkChange, Ending, Held, HeldMapping,
    KnownPlugin, LastChange, LaunchAct, LaunchNote, LinkableAccountsChange, RecordedMove, Result,
    SettingChange, SettingChangeRecord, SettingsAuthor, Snapshot, Store, StoredSetting,
    VenueAccountMapChange, Withdrawal, REDACTED_BY_REDECLARATION, REDACTED_BY_SEALING,
};
use crate::DEPLOYMENT_ADMIN;

/// A deployment setting's change as kept: its name, value, author and time
/// (contract v28).
type SettingKept = (String, Vec<u8>, Author, i64);

pub struct MemoryStore {
    state: Mutex<Snapshot>,
    /// Each settings change's own record (W6.11), as the Postgres store keeps
    /// them, so the rule is tested on both.
    changes: Mutex<Vec<SettingChangeRecord>>,
    /// Each grant change's own record (W6.7, W6.8), likewise.
    access_changes: Mutex<Vec<AccessChangeRecord>>,
    /// Each hold's and each archive's change, its own record, in order, with
    /// its note (W6.25, W8.7; the note from contract v17).
    holds: Mutex<Vec<(Hold, String)>>,
    /// Contract v24 (W3.28): the usage setting's changes, and the reports.
    reports_usage: Mutex<Vec<(bool, Author, i64)>>,
    settings: Mutex<Vec<SettingKept>>,
    /// Contract v26 (W6.35): the time zone's changes.
    time_zones: Mutex<Vec<(String, Author, i64)>>,
    fx_pivots: Mutex<Vec<(Vec<String>, Author, i64)>>,
    /// Contract v26 (W12.6): the routing table's changes.
    model_routing: Mutex<Vec<crate::store::ModelRoutingChange>>,
    usage_reports: Mutex<Vec<meridian_domain::v1::SentUsageReport>>,
    archives: Mutex<Vec<(PluginArchive, String)>>,
    /// Each launch's and stop's note, in order (contract v17).
    launch_notes: Mutex<Vec<LaunchNote>>,
    /// Each move of raw records (W4.13), numbered from 1 in order.
    moves: Mutex<Vec<RecordedMove>>,
    /// Each licence and entitlement change, in order (contract v18).
    licences: Mutex<Vec<meridian_pb::v1::DatasetLicence>>,
    entitlements: Mutex<Vec<meridian_domain::v1::DatasetEntitlement>>,
    /// Each linkable-accounts change, in order (contract v20).
    linkable_changes: Mutex<Vec<LinkableAccountsChange>>,
    /// Each link and map change, in order, and each venue catalogue as kept,
    /// latest per plugin and code with every change (contract v22).
    link_changes: Mutex<Vec<ConnectivityLinkChange>>,
    map_changes: Mutex<Vec<VenueAccountMapChange>>,
    catalogues: Mutex<Vec<(i64, VenueCatalogue)>>,
    /// Each version's documentation as uploaded (W8.1, contract v25).
    documentation: Mutex<
        std::collections::BTreeMap<(String, String), Vec<meridian_domain::v1::DocumentationText>>,
    >,
    /// Each declaration of the licensed schemes, in order (W3.28, v25).
    licensed_schemes: Mutex<Vec<(Vec<String>, Author, i64)>>,
    /// Who retired each version, through what (W8.15, contract v25).
    retirements: Mutex<std::collections::BTreeMap<(String, String), Author>>,
    installations: Mutex<std::collections::BTreeMap<(String, String), Author>>,
    /// The pack each install record names (W8.18, v28).
    install_packs: Mutex<std::collections::BTreeMap<(String, String), String>>,
    /// Each change to a starter pack's record, in order (W8.18, W8.19).
    pack_changes: Mutex<Vec<crate::store::PackChange>>,
    /// The registries (contract v25): the catalogue's copy, each listed
    /// version's statement, the usage setting's changes and the reports.
    registry: Mutex<Option<crate::store::HeldCatalogue>>,
    statements: Mutex<std::collections::BTreeMap<(String, String), String>>,
    reports_plugin_usage: Mutex<Vec<(bool, Author, i64)>>,
    plugin_usage_reports: Mutex<Vec<meridian_domain::v1::SentPluginUsageReport>>,
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
            holds: Mutex::new(Vec::new()),
            reports_usage: Mutex::new(Vec::new()),
            settings: Mutex::new(Vec::new()),
            time_zones: Mutex::new(Vec::new()),
            fx_pivots: Mutex::new(Vec::new()),
            model_routing: Mutex::new(Vec::new()),
            usage_reports: Mutex::new(Vec::new()),
            registry: Mutex::new(None),
            install_packs: Mutex::new(Default::default()),
            pack_changes: Mutex::new(Vec::new()),
            statements: Mutex::new(Default::default()),
            reports_plugin_usage: Mutex::new(Vec::new()),
            plugin_usage_reports: Mutex::new(Vec::new()),
            archives: Mutex::new(Vec::new()),
            launch_notes: Mutex::new(Vec::new()),
            moves: Mutex::new(Vec::new()),
            licences: Mutex::new(Vec::new()),
            entitlements: Mutex::new(Vec::new()),
            linkable_changes: Mutex::new(Vec::new()),
            link_changes: Mutex::new(Vec::new()),
            map_changes: Mutex::new(Vec::new()),
            catalogues: Mutex::new(Vec::new()),
            retirements: Mutex::new(Default::default()),
            installations: Mutex::new(Default::default()),
            licensed_schemes: Mutex::new(Vec::new()),
            documentation: Mutex::new(Default::default()),
        }
    }

    /// Each unit's latest move, for one instance.
    fn latest_moves(&self, instance_id: &str) -> Vec<RecordedMove> {
        let moves = self.moves.lock().expect("store lock poisoned");
        let mut latest: Vec<RecordedMove> = Vec::new();
        for held in moves.iter().rev() {
            if held.instance_id != instance_id {
                continue;
            }
            let (kind, unit) = unit_of(&held.record);
            if !latest
                .iter()
                .any(|seen| unit_of(&seen.record) == (kind, unit))
            {
                latest.push(held.clone());
            }
        }
        latest
    }
}

/// A move's kind and unit.
fn unit_of(record: &MoveRecord) -> (&str, &str) {
    record
        .r#move
        .as_ref()
        .map(|m| (m.record_kind.as_str(), m.unit.as_str()))
        .unwrap_or_default()
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
        client: String::new(),
        at_ns,
        backfilled: false,
        note,
        prepared_call_id: String::new(),
        hint: String::new(),
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
        // Each role's latest hold, a cleared one left out; each instance's
        // latest archive.
        for (hold, note) in self.holds.lock().expect("store lock poisoned").iter() {
            snapshot.holds.retain(|held| held.role != hold.role);
            if hold.days > 0 {
                snapshot.holds.push(Hold {
                    note: note.clone(),
                    ..hold.clone()
                });
            }
        }
        snapshot.holds.sort_by(|a, b| a.role.cmp(&b.role));
        for (archive, note) in self.archives.lock().expect("store lock poisoned").iter() {
            snapshot
                .archives
                .retain(|held| held.instance_id != archive.instance_id);
            snapshot.archives.push(PluginArchive {
                note: note.clone(),
                ..archive.clone()
            });
        }
        snapshot
            .archives
            .sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
        // Each dataset's latest licence, and each entitlement's latest.
        for licence in self.licences.lock().expect("store lock poisoned").iter() {
            snapshot
                .licences
                .retain(|held| held.dataset != licence.dataset);
            snapshot.licences.push(licence.clone());
        }
        snapshot.licences.sort_by(|a, b| a.dataset.cmp(&b.dataset));
        for entitlement in self
            .entitlements
            .lock()
            .expect("store lock poisoned")
            .iter()
        {
            snapshot.entitlements.retain(|held| {
                (held.dataset.as_str(), held.instance.as_str())
                    != (entitlement.dataset.as_str(), entitlement.instance.as_str())
            });
            snapshot.entitlements.push(entitlement.clone());
        }
        snapshot.entitlements.sort_by(|a, b| {
            (a.dataset.as_str(), a.instance.as_str())
                .cmp(&(b.dataset.as_str(), b.instance.as_str()))
        });
        for change in self.changes.lock().expect("store lock poisoned").iter() {
            if matches!(change.kind, ChangeKind::Set | ChangeKind::Cleared) {
                snapshot.settings_changed.insert(
                    change.plugin_instance_id.clone(),
                    LastChange {
                        by: change.by.clone(),
                        at_ns: change.at_ns,
                    },
                );
                // Each setting's latest, in name order (contract v17).
                let changes = snapshot
                    .setting_changes
                    .entry(change.plugin_instance_id.clone())
                    .or_default();
                changes.retain(|held| held.name != change.name);
                changes.push(SettingLastChange {
                    name: change.name.clone(),
                    changed_by: change.by.clone(),
                    changed_at_ns: change.at_ns,
                    acting_through_delegation: change.delegation.clone(),
                    client_name: change.client.clone(),
                    note: change.note.clone(),
                    prepared_call_id: change.prepared_call_id.clone(),
                    hint: change.hint.clone(),
                });
                changes.sort_by(|a, b| a.name.cmp(&b.name));
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
                client: author.client.clone(),
                at_ns,
                prepared_call_id: author.prepared_call_id.clone(),
                hint: if matches!(change.held, Some(Held::Sealed(_))) {
                    change.hint.clone()
                } else {
                    String::new()
                },
                backfilled: false,
                note: author.note.clone(),
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

    fn record_plugin_version_documented(
        &self,
        version: &PluginVersion,
        documentation: &[meridian_domain::v1::DocumentationText],
    ) -> Result<bool> {
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
        self.documentation
            .lock()
            .expect("store lock poisoned")
            .insert((metadata.name, metadata.version), documentation.to_vec());
        Ok(true)
    }

    fn plugin_documentation(
        &self,
        name: &str,
        version: &str,
    ) -> Result<Vec<meridian_domain::v1::DocumentationText>> {
        Ok(self
            .documentation
            .lock()
            .expect("store lock poisoned")
            .get(&(name.to_string(), version.to_string()))
            .cloned()
            .unwrap_or_default())
    }

    fn retire_plugin_version(
        &self,
        name: &str,
        version: &str,
        author: &Author,
        at_ns: i64,
    ) -> Result<Option<PluginVersion>> {
        let mut state = self.state.lock().expect("store lock poisoned");
        let Some(held) = state.catalogue.versions.iter_mut().find(|held| {
            held.metadata
                .as_ref()
                .is_some_and(|m| m.name == name && m.version == version)
        }) else {
            return Ok(None);
        };
        if held.retired_at_ns != 0 {
            return Ok(None);
        }
        held.retired_by = author.by.clone();
        held.retired_at_ns = at_ns;
        held.retired_note = author.note.clone();
        let retired = held.clone();
        self.retirements
            .lock()
            .expect("store lock poisoned")
            .insert((name.to_string(), version.to_string()), author.clone());
        Ok(Some(retired))
    }

    fn retirement(&self, name: &str, version: &str) -> Result<Option<Author>> {
        Ok(self
            .retirements
            .lock()
            .expect("store lock poisoned")
            .get(&(name.to_string(), version.to_string()))
            .cloned())
    }

    fn begin_launch(&self, launch: &PluginLaunch, note: &str) -> Result<bool> {
        let mut state = self.state.lock().expect("store lock poisoned");
        let live = state.catalogue.launches.iter().any(|held| {
            held.instance_id == launch.instance_id
                && held.state == PluginLaunchState::Launched as i32
        });
        if live {
            return Ok(false);
        }
        state.catalogue.launches.push(PluginLaunch {
            note: note.to_string(),
            ..launch.clone()
        });
        self.launch_notes
            .lock()
            .expect("store lock poisoned")
            .push(LaunchNote {
                instance_id: launch.instance_id.clone(),
                launched_at_ns: launch.launched_at_ns,
                act: LaunchAct::Launched,
                note: note.to_string(),
                gap: false,
                at_ns: launch.launched_at_ns,
            });
        Ok(true)
    }

    fn end_launch(&self, instance_id: &str, ending: &Ending) -> Result<Option<PluginLaunch>> {
        let mut state = self.state.lock().expect("store lock poisoned");
        let live = state.catalogue.launches.iter_mut().find(|held| {
            held.instance_id == instance_id && held.state == PluginLaunchState::Launched as i32
        });
        let ended = live.map(|launch| {
            *launch = ending.applied_to(launch);
            launch.clone()
        });
        if let Some(launch) = ended
            .as_ref()
            .filter(|_| ending.state == PluginLaunchState::Stopped)
        {
            self.launch_notes
                .lock()
                .expect("store lock poisoned")
                .push(LaunchNote {
                    instance_id: launch.instance_id.clone(),
                    launched_at_ns: launch.launched_at_ns,
                    act: LaunchAct::Stopped,
                    note: ending.note.clone(),
                    gap: false,
                    at_ns: ending.at_ns,
                });
        }
        Ok(ended)
    }

    fn launch_notes(&self, instance_id: &str) -> Result<Vec<LaunchNote>> {
        Ok(self
            .launch_notes
            .lock()
            .expect("store lock poisoned")
            .iter()
            .filter(|held| held.instance_id == instance_id)
            .cloned()
            .collect())
    }

    fn set_hold(&self, hold: &Hold, note: &str) -> Result<()> {
        self.holds
            .lock()
            .expect("store lock poisoned")
            .push((hold.clone(), note.to_string()));
        Ok(())
    }

    fn hold_changes(&self) -> Result<Vec<(Hold, String)>> {
        Ok(self.holds.lock().expect("store lock poisoned").clone())
    }

    fn put_archive(&self, archive: &PluginArchive, note: &str) -> Result<()> {
        self.archives
            .lock()
            .expect("store lock poisoned")
            .push((archive.clone(), note.to_string()));
        Ok(())
    }

    fn archive_changes(&self, instance_id: &str) -> Result<Vec<(PluginArchive, String)>> {
        Ok(self
            .archives
            .lock()
            .expect("store lock poisoned")
            .iter()
            .filter(|(archive, _)| archive.instance_id == instance_id)
            .cloned()
            .collect())
    }

    fn record_move(&self, instance_id: &str, record: &MoveRecord) -> Result<bool> {
        let (kind, unit) = unit_of(record);
        let again = self
            .latest_move(instance_id, kind, unit)?
            .and_then(|latest| latest.r#move)
            .zip(record.r#move.as_ref())
            .is_some_and(|(latest, now)| latest.outcome == now.outcome);
        if again {
            return Ok(false);
        }
        let mut moves = self.moves.lock().expect("store lock poisoned");
        let move_id = moves.len() as i64 + 1;
        moves.push(RecordedMove {
            move_id,
            instance_id: instance_id.to_string(),
            record: record.clone(),
        });
        Ok(true)
    }

    fn latest_move(
        &self,
        instance_id: &str,
        record_kind: &str,
        unit: &str,
    ) -> Result<Option<MoveRecord>> {
        Ok(self
            .moves
            .lock()
            .expect("store lock poisoned")
            .iter()
            .rev()
            .find(|held| {
                held.instance_id == instance_id && unit_of(&held.record) == (record_kind, unit)
            })
            .map(|held| held.record.clone()))
    }

    fn moves(&self, instance_id: &str, before: i64, limit: usize) -> Result<Vec<RecordedMove>> {
        Ok(self
            .moves
            .lock()
            .expect("store lock poisoned")
            .iter()
            .rev()
            .filter(|held| {
                held.instance_id == instance_id && (before == 0 || held.move_id < before)
            })
            .take(limit)
            .cloned()
            .collect())
    }

    fn archived(&self, instance_id: &str) -> Result<Vec<StoredSpan>> {
        let latest = self.latest_moves(instance_id);
        Ok(archived_spans(
            latest.iter().filter_map(|held| held.record.r#move.as_ref()),
        ))
    }

    fn record_catalogue(
        &self,
        instance_id: &str,
        catalogue: &meridian_pb::v1::Catalogue,
        _at_ns: i64,
    ) -> Result<bool> {
        let mut state = self.state.lock().expect("store lock poisoned");
        if state.catalogues.get(instance_id) == Some(catalogue) {
            return Ok(false);
        }
        state
            .catalogues
            .insert(instance_id.to_string(), catalogue.clone());
        Ok(true)
    }

    fn set_dataset_licence(&self, licence: &meridian_pb::v1::DatasetLicence) -> Result<()> {
        self.licences
            .lock()
            .expect("store lock poisoned")
            .push(licence.clone());
        Ok(())
    }

    fn set_dataset_entitlement(
        &self,
        entitlement: &meridian_domain::v1::DatasetEntitlement,
    ) -> Result<()> {
        self.entitlements
            .lock()
            .expect("store lock poisoned")
            .push(entitlement.clone());
        Ok(())
    }

    fn dataset_changes(
        &self,
        dataset: &str,
    ) -> Result<(
        Vec<meridian_pb::v1::DatasetLicence>,
        Vec<meridian_domain::v1::DatasetEntitlement>,
    )> {
        Ok((
            self.licences
                .lock()
                .expect("store lock poisoned")
                .iter()
                .filter(|l| l.dataset == dataset)
                .cloned()
                .collect(),
            self.entitlements
                .lock()
                .expect("store lock poisoned")
                .iter()
                .filter(|e| e.dataset == dataset)
                .cloned()
                .collect(),
        ))
    }

    fn close_account_withdrawing_grants(
        &self,
        account: &AccountRecord,
        author: &Author,
        at_ns: i64,
    ) -> Result<Vec<String>> {
        let mut state = self.state.lock().expect("store lock poisoned");
        let mut changes = self.linkable_changes.lock().expect("store lock poisoned");
        let mut withdrawn_from = Vec::new();
        for (instance, held) in state.linkable.iter_mut() {
            let Some(at) = held
                .account_ids
                .iter()
                .position(|a| *a == account.account_id)
            else {
                continue;
            };
            held.account_ids.remove(at);
            held.updated_by = author.by.clone();
            held.updated_at_ns = at_ns;
            held.acting_through_delegation = author.delegation.clone();
            held.client_name = author.client.clone();
            changes.push(LinkableAccountsChange {
                plugin_instance_id: instance.clone(),
                granted: Vec::new(),
                withdrawn: vec![account.account_id.clone()],
                by: author.by.clone(),
                delegation: author.delegation.clone(),
                client: author.client.clone(),
                at_ns,
                note: author.note.clone(),
            });
            withdrawn_from.push(instance.clone());
        }
        withdrawn_from.sort();
        upsert(&mut state.records.accounts, account, |a| {
            a.account_id == account.account_id
        });
        Ok(withdrawn_from)
    }

    fn set_linkable_accounts(
        &self,
        plugin_instance_id: &str,
        grant: &[String],
        withdraw: &[String],
        author: &Author,
        at_ns: i64,
    ) -> Result<LinkableAccounts> {
        let mut state = self.state.lock().expect("store lock poisoned");
        let held = state
            .linkable
            .entry(plugin_instance_id.to_string())
            .or_insert_with(|| LinkableAccounts {
                plugin_instance_id: plugin_instance_id.to_string(),
                ..LinkableAccounts::default()
            });
        let mut granted = Vec::new();
        for account in grant {
            if !held.account_ids.contains(account) && !granted.contains(account) {
                held.account_ids.push(account.clone());
                granted.push(account.clone());
            }
        }
        let mut withdrawn = Vec::new();
        for account in withdraw {
            if let Some(at) = held.account_ids.iter().position(|a| a == account) {
                held.account_ids.remove(at);
                withdrawn.push(account.clone());
            }
        }
        held.account_ids.sort();
        if !granted.is_empty() || !withdrawn.is_empty() {
            held.updated_by = author.by.clone();
            held.updated_at_ns = at_ns;
            held.acting_through_delegation = author.delegation.clone();
            held.client_name = author.client.clone();
            self.linkable_changes
                .lock()
                .expect("store lock poisoned")
                .push(LinkableAccountsChange {
                    plugin_instance_id: plugin_instance_id.to_string(),
                    granted,
                    withdrawn,
                    by: author.by.clone(),
                    delegation: author.delegation.clone(),
                    client: author.client.clone(),
                    at_ns,
                    note: author.note.clone(),
                });
        }
        let now = held.clone();
        if now.account_ids.is_empty() && now.updated_at_ns == 0 {
            state.linkable.remove(plugin_instance_id);
        }
        Ok(now)
    }

    fn linkable_accounts_changes(
        &self,
        plugin_instance_id: &str,
    ) -> Result<Vec<LinkableAccountsChange>> {
        Ok(self
            .linkable_changes
            .lock()
            .expect("store lock poisoned")
            .iter()
            .filter(|change| change.plugin_instance_id == plugin_instance_id)
            .cloned()
            .collect())
    }

    fn set_connectivity_link(
        &self,
        link: &ConnectivityLink,
        author: &Author,
        at_ns: i64,
    ) -> Result<ConnectivityLink> {
        let mut state = self.state.lock().expect("store lock poisoned");
        let instance = &link.connectivity_instance_id;
        let from = state.connectivity.get(instance).cloned();
        let standing = from.clone().unwrap_or_else(|| ConnectivityLink {
            connectivity_instance_id: instance.clone(),
            ..Default::default()
        });
        if standing == *link {
            return Ok(standing);
        }
        state.connectivity.insert(instance.clone(), link.clone());
        self.link_changes
            .lock()
            .expect("store lock poisoned")
            .push(ConnectivityLinkChange {
                connectivity_instance_id: instance.clone(),
                from_link: from,
                to_link: link.clone(),
                by: author.by.clone(),
                delegation: author.delegation.clone(),
                client: author.client.clone(),
                at_ns,
                note: author.note.clone(),
            });
        Ok(link.clone())
    }

    fn connectivity_link_changes(&self, instance_id: &str) -> Result<Vec<ConnectivityLinkChange>> {
        Ok(self
            .link_changes
            .lock()
            .expect("store lock poisoned")
            .iter()
            .filter(|change| change.connectivity_instance_id == instance_id)
            .cloned()
            .collect())
    }

    fn map_venue_account(
        &self,
        ems_instance_id: &str,
        from: Option<&VenueAccountMapping>,
        to: Option<&VenueAccountMapping>,
        author: &Author,
        at_ns: i64,
    ) -> Result<()> {
        let Some(key) = to.or(from).map(|held| {
            (
                held.connectivity_instance_id.clone(),
                held.venue_account_code.clone(),
            )
        }) else {
            return Ok(());
        };
        let mut state = self.state.lock().expect("store lock poisoned");
        let at = state.venue_mappings.iter().position(|held| {
            held.mapping.connectivity_instance_id == key.0
                && held.mapping.venue_account_code == key.1
        });
        let standing = at.map(|at| state.venue_mappings[at].clone());
        let unchanged = match (&standing, to) {
            (None, None) => true,
            (Some(held), Some(mapping)) => {
                held.ems_instance_id == ems_instance_id && held.mapping == *mapping
            }
            _ => false,
        };
        if unchanged {
            return Ok(());
        }
        if let Some(at) = at {
            state.venue_mappings.remove(at);
        }
        if let Some(mapping) = to {
            state.venue_mappings.push(HeldMapping {
                ems_instance_id: ems_instance_id.to_string(),
                mapping: mapping.clone(),
            });
        }
        self.map_changes
            .lock()
            .expect("store lock poisoned")
            .push(VenueAccountMapChange {
                ems_instance_id: ems_instance_id.to_string(),
                from_mapping: standing.map(|held| held.mapping),
                to_mapping: to.cloned(),
                by: author.by.clone(),
                delegation: author.delegation.clone(),
                client: author.client.clone(),
                at_ns,
                note: author.note.clone(),
            });
        Ok(())
    }

    fn venue_account_map_changes(
        &self,
        ems_instance_id: &str,
    ) -> Result<Vec<VenueAccountMapChange>> {
        Ok(self
            .map_changes
            .lock()
            .expect("store lock poisoned")
            .iter()
            .filter(|change| change.ems_instance_id == ems_instance_id)
            .cloned()
            .collect())
    }

    fn keep_venue_catalogue(&self, catalogue: &VenueCatalogue, at_ns: i64) -> Result<bool> {
        let latest = self.venue_catalogue(
            &catalogue.connectivity_instance_id,
            &catalogue.venue_account_code,
        )?;
        if latest.as_ref() == Some(catalogue) {
            return Ok(false);
        }
        self.catalogues
            .lock()
            .expect("store lock poisoned")
            .push((at_ns, catalogue.clone()));
        Ok(true)
    }

    fn venue_catalogue(&self, instance_id: &str, code: &str) -> Result<Option<VenueCatalogue>> {
        Ok(self
            .catalogues
            .lock()
            .expect("store lock poisoned")
            .iter()
            .rev()
            .find(|(_, held)| {
                held.connectivity_instance_id == instance_id && held.venue_account_code == code
            })
            .map(|(_, held)| held.clone()))
    }

    fn set_deployment_time_zone(&self, zone: &str, author: &Author, at_ns: i64) -> Result<()> {
        self.time_zones.lock().expect("store lock poisoned").push((
            zone.to_string(),
            author.clone(),
            at_ns,
        ));
        Ok(())
    }

    fn set_model_routing(
        &self,
        model_id: &str,
        implementation: &str,
        author: &Author,
        at_ns: i64,
    ) -> Result<()> {
        self.model_routing
            .lock()
            .expect("store lock poisoned")
            .push(crate::store::ModelRoutingChange {
                model_id: model_id.to_string(),
                implementation: implementation.to_string(),
                changed_at_ns: at_ns,
                changed_by: author.by.clone(),
                note: author.note.clone(),
            });
        Ok(())
    }

    fn model_routing_changes(&self) -> Result<Vec<crate::store::ModelRoutingChange>> {
        Ok(self
            .model_routing
            .lock()
            .expect("store lock poisoned")
            .clone())
    }

    fn deployment_time_zone(&self) -> Result<Option<crate::store::TimeZoneChange>> {
        Ok(self
            .time_zones
            .lock()
            .expect("store lock poisoned")
            .last()
            .map(|(zone, author, at)| crate::store::TimeZoneChange {
                zone: zone.clone(),
                changed_at_ns: *at,
                changed_by: author.by.clone(),
            }))
    }

    fn set_fx_pivot_currencies(
        &self,
        pivots: &[String],
        author: &Author,
        at_ns: i64,
    ) -> Result<()> {
        self.fx_pivots.lock().expect("store lock poisoned").push((
            pivots.to_vec(),
            author.clone(),
            at_ns,
        ));
        Ok(())
    }

    fn fx_pivot_currencies(&self) -> Result<Option<crate::store::FxPivotChange>> {
        Ok(self
            .fx_pivots
            .lock()
            .expect("store lock poisoned")
            .last()
            .map(|(pivots, author, at)| crate::store::FxPivotChange {
                fx_pivot_currencies: pivots.clone(),
                changed_at_ns: *at,
                changed_by: author.by.clone(),
            }))
    }

    fn set_setting(&self, setting: &str, value: &[u8], author: &Author, at_ns: i64) -> Result<()> {
        self.settings.lock().expect("store lock poisoned").push((
            setting.to_string(),
            value.to_vec(),
            author.clone(),
            at_ns,
        ));
        Ok(())
    }

    fn setting(&self, setting: &str) -> Result<Option<crate::store::DeploymentSettingChange>> {
        Ok(self
            .settings
            .lock()
            .expect("store lock poisoned")
            .iter()
            .rev()
            .find(|(name, ..)| name == setting)
            .map(
                |(_, value, author, at)| crate::store::DeploymentSettingChange {
                    value: value.clone(),
                    changed_at_ns: *at,
                    changed_by: author.by.clone(),
                },
            ))
    }

    fn set_reports_usage(&self, reports_usage: bool, author: &Author, at_ns: i64) -> Result<()> {
        self.reports_usage
            .lock()
            .expect("store lock poisoned")
            .push((reports_usage, author.clone(), at_ns));
        Ok(())
    }

    fn reports_usage(&self) -> Result<bool> {
        Ok(self
            .reports_usage
            .lock()
            .expect("store lock poisoned")
            .last()
            .is_none_or(|(on, _, _)| *on))
    }

    fn set_licensed_schemes(&self, schemes: &[String], author: &Author, at_ns: i64) -> Result<()> {
        self.licensed_schemes
            .lock()
            .expect("store lock poisoned")
            .push((schemes.to_vec(), author.clone(), at_ns));
        Ok(())
    }

    fn licensed_schemes(&self) -> Result<Vec<String>> {
        Ok(self
            .licensed_schemes
            .lock()
            .expect("store lock poisoned")
            .last()
            .map(|(schemes, _, _)| schemes.clone())
            .unwrap_or_default())
    }

    fn record_usage_report(&self, report: &meridian_domain::v1::SentUsageReport) -> Result<()> {
        self.usage_reports
            .lock()
            .expect("store lock poisoned")
            .push(report.clone());
        Ok(())
    }

    fn usage_reports(&self, limit: usize) -> Result<Vec<meridian_domain::v1::SentUsageReport>> {
        Ok(self
            .usage_reports
            .lock()
            .expect("store lock poisoned")
            .iter()
            .rev()
            .take(limit)
            .cloned()
            .collect())
    }

    fn registry_catalogue(&self) -> Result<Option<crate::store::HeldCatalogue>> {
        #[cfg(test)]
        blocks_like_postgres();
        Ok(self.registry.lock().expect("store lock poisoned").clone())
    }

    fn put_registry_catalogue(&self, held: &crate::store::HeldCatalogue) -> Result<()> {
        #[cfg(test)]
        blocks_like_postgres();
        *self.registry.lock().expect("store lock poisoned") = Some(held.clone());
        Ok(())
    }

    fn record_listed_version(
        &self,
        version: &PluginVersion,
        documentation: &[meridian_domain::v1::DocumentationText],
        listing_statement: &str,
        by: &Author,
        pack: &str,
    ) -> Result<bool> {
        let recorded = self.record_plugin_version_documented(version, documentation)?;
        if recorded {
            let metadata = version.metadata.clone().unwrap_or_default();
            let key = (metadata.name, metadata.version);
            self.install_packs
                .lock()
                .expect("store lock poisoned")
                .insert(key.clone(), pack.to_string());
            self.statements
                .lock()
                .expect("store lock poisoned")
                .insert(key.clone(), listing_statement.to_string());
            self.installations
                .lock()
                .expect("store lock poisoned")
                .insert(key, by.clone());
        }
        Ok(recorded)
    }

    fn install_pack(&self, name: &str, version: &str) -> Result<String> {
        Ok(self
            .install_packs
            .lock()
            .expect("store lock poisoned")
            .get(&(name.to_string(), version.to_string()))
            .cloned()
            .unwrap_or_default())
    }

    fn record_pack_change(&self, change: &crate::store::PackChange) -> Result<()> {
        #[cfg(test)]
        blocks_like_postgres();
        self.pack_changes
            .lock()
            .expect("store lock poisoned")
            .push(change.clone());
        Ok(())
    }

    fn pack_changes(&self, pack: &str) -> Result<Vec<crate::store::PackChange>> {
        #[cfg(test)]
        blocks_like_postgres();
        Ok(self
            .pack_changes
            .lock()
            .expect("store lock poisoned")
            .iter()
            .filter(|change| pack.is_empty() || change.pack == pack)
            .cloned()
            .collect())
    }

    fn installation(&self, name: &str, version: &str) -> Result<Option<Author>> {
        Ok(self
            .installations
            .lock()
            .expect("store lock poisoned")
            .get(&(name.to_string(), version.to_string()))
            .cloned())
    }

    fn listing_statement(&self, name: &str, version: &str) -> Result<String> {
        Ok(self
            .statements
            .lock()
            .expect("store lock poisoned")
            .get(&(name.to_string(), version.to_string()))
            .cloned()
            .unwrap_or_default())
    }

    fn acknowledge_pull(
        &self,
        instance_id: &str,
        acknowledgement: &meridian_domain::v1::PullAcknowledgement,
    ) -> Result<Option<PluginLaunch>> {
        let mut state = self.state.lock().expect("store lock poisoned");
        Ok(state
            .catalogue
            .launches
            .iter_mut()
            .find(|held| {
                held.instance_id == instance_id && held.state == PluginLaunchState::Launched as i32
            })
            .map(|launch| {
                launch.pull_acknowledgement = Some(acknowledgement.clone());
                launch.clone()
            }))
    }

    fn set_reports_plugin_usage(&self, on: bool, author: &Author, at_ns: i64) -> Result<()> {
        self.reports_plugin_usage
            .lock()
            .expect("store lock poisoned")
            .push((on, author.clone(), at_ns));
        Ok(())
    }

    fn reports_plugin_usage(&self) -> Result<bool> {
        Ok(self
            .reports_plugin_usage
            .lock()
            .expect("store lock poisoned")
            .last()
            .is_none_or(|(on, _, _)| *on))
    }

    fn record_plugin_usage_report(
        &self,
        report: &meridian_domain::v1::SentPluginUsageReport,
    ) -> Result<()> {
        self.plugin_usage_reports
            .lock()
            .expect("store lock poisoned")
            .push(report.clone());
        Ok(())
    }

    fn plugin_usage_reports(
        &self,
        limit: usize,
    ) -> Result<Vec<meridian_domain::v1::SentPluginUsageReport>> {
        Ok(self
            .plugin_usage_reports
            .lock()
            .expect("store lock poisoned")
            .iter()
            .rev()
            .take(limit)
            .cloned()
            .collect())
    }
}

/// What the Postgres store does on a thread driving async tasks: its client
/// starts a runtime of its own, which panics there. The registries' copy is
/// read on async paths (the hourly pull, a fetch the bus's handler waits
/// on), so the memory store does the same with it in core's tests, and a
/// call that would block a runtime in production fails here first.
#[cfg(test)]
fn blocks_like_postgres() {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a runtime")
        .block_on(async {});
}
