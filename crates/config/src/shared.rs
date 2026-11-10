//! One snapshot shared per change (stage M; the resilience spec's section
//! 10a; the store memory audit of 2026-11-09, item 8, approved under the
//! working agreement).
//!
//! The conductor reads the whole configuration -- accounts, groups,
//! permissions, plugins, versions with their declarations -- for almost
//! every question it answers, and read it once per question: a burst of
//! questions held as many copies at once. A store wrapped in [`Shared`]
//! answers [`Store::snapshot_shared`] with one snapshot until the next
//! change this process makes, read again at most every [`FRESH_FOR`] all the
//! same for what another process wrote (a migration, a restore), and read
//! by one question at a time while the others wait for it. Every other call
//! goes straight through; a write marks the snapshot read again.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::store::*;
use meridian_domain::v1::DatasetEntitlement;
use meridian_domain::v1::VenueCatalogue;
use meridian_domain::v1::{
    AccessGroup, AccountGroup, AccountRecord, ExternalAccountLink, Hold, LinkableAccounts,
    MoveRecord, Permission, PluginArchive, PluginLaunch, PluginVersion, SignInRecord, UserGroup,
};
use meridian_pb::v1::{
    Catalogue, ConnectivityLink, DatasetLicence, SettingDeclaration, StoredSpan,
    VenueAccountMapping,
};

/// How long a snapshot is answered when this process changed nothing: what
/// another process wrote is seen within it.
pub const FRESH_FOR: Duration = Duration::from_secs(1);

/// A store whose snapshot is shared until it changes.
pub struct Shared {
    inner: Arc<dyn Store>,
    generation: AtomicU64,
    held: Mutex<Option<(u64, Instant, Arc<Snapshot>)>>,
    reading: Mutex<()>,
}

impl Shared {
    pub fn new(inner: Arc<dyn Store>) -> Self {
        Self {
            inner,
            generation: AtomicU64::new(0),
            held: Mutex::new(None),
            reading: Mutex::new(()),
        }
    }

    fn changed(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    fn current(&self) -> Option<Arc<Snapshot>> {
        let generation = self.generation.load(Ordering::Acquire);
        let held = self
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match held.as_ref() {
            Some((at, read_at, snapshot)) if *at == generation && read_at.elapsed() < FRESH_FOR => {
                Some(Arc::clone(snapshot))
            }
            _ => None,
        }
    }

    fn shared(&self) -> Result<Arc<Snapshot>> {
        if let Some(held) = self.current() {
            return Ok(held);
        }
        // One question reads it; the others wait, then share what it read.
        let _reading = self
            .reading
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(held) = self.current() {
            return Ok(held);
        }
        let generation = self.generation.load(Ordering::Acquire);
        let read_at = Instant::now();
        let snapshot = Arc::new(self.inner.snapshot()?);
        *self
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some((generation, read_at, Arc::clone(&snapshot)));
        Ok(snapshot)
    }
}

impl Store for Shared {
    fn snapshot(&self) -> Result<Snapshot> {
        self.snapshot_shared().map(|shared| (*shared).clone())
    }

    fn snapshot_shared(&self) -> Result<Arc<Snapshot>> {
        self.shared()
    }

    fn launches(&self) -> Result<Vec<PluginLaunch>> {
        self.inner.launches()
    }

    fn put_account(&self, account: &AccountRecord) -> Result<()> {
        let said = self.inner.put_account(account);
        self.changed();
        said
    }

    fn put_user_group(&self, group: &UserGroup) -> Result<()> {
        let said = self.inner.put_user_group(group);
        self.changed();
        said
    }

    fn put_account_group(&self, group: &AccountGroup) -> Result<()> {
        let said = self.inner.put_account_group(group);
        self.changed();
        said
    }

    fn put_access_group(&self, group: &AccessGroup, author: &Author, at_ns: i64) -> Result<()> {
        let said = self.inner.put_access_group(group, author, at_ns);
        self.changed();
        said
    }

    fn put_link(&self, link: &ExternalAccountLink) -> Result<()> {
        let said = self.inner.put_link(link);
        self.changed();
        said
    }

    fn put_account_and_link(
        &self,
        account: &AccountRecord,
        link: &ExternalAccountLink,
    ) -> Result<()> {
        let said = self.inner.put_account_and_link(account, link);
        self.changed();
        said
    }

    fn add_permission(&self, permission: &Permission, author: &Author, at_ns: i64) -> Result<()> {
        let said = self.inner.add_permission(permission, author, at_ns);
        self.changed();
        said
    }

    fn withdraw_permission(
        &self,
        permission_id: &str,
        author: &Author,
        at_ns: i64,
    ) -> Result<Withdrawal> {
        let said = self.inner.withdraw_permission(permission_id, author, at_ns);
        self.changed();
        said
    }

    fn install_first_admin(
        &self,
        group: &UserGroup,
        permissions: &[Permission],
        author: &Author,
        at_ns: i64,
    ) -> Result<bool> {
        let said = self
            .inner
            .install_first_admin(group, permissions, author, at_ns);
        self.changed();
        said
    }

    fn install_claimed_admin(
        &self,
        group: &UserGroup,
        permissions: &[Permission],
        author: &Author,
        at_ns: i64,
    ) -> Result<bool> {
        let said = self
            .inner
            .install_claimed_admin(group, permissions, author, at_ns);
        self.changed();
        said
    }

    fn access_changes(&self, access_group_id: &str) -> Result<Vec<AccessChangeRecord>> {
        self.inner.access_changes(access_group_id)
    }

    fn record_sign_in(&self, record: &SignInRecord) -> Result<()> {
        let said = self.inner.record_sign_in(record);
        self.changed();
        said
    }

    fn record_plugin(&self, plugin: &KnownPlugin) -> Result<()> {
        let said = self.inner.record_plugin(plugin);
        self.changed();
        said
    }

    fn record_declared_settings(
        &self,
        plugin_instance_id: &str,
        declared: &[SettingDeclaration],
        at_ns: i64,
    ) -> Result<Vec<String>> {
        let said = self
            .inner
            .record_declared_settings(plugin_instance_id, declared, at_ns);
        self.changed();
        said
    }

    fn put_plugin_settings(
        &self,
        plugin_instance_id: &str,
        changes: &[SettingChange],
        author: &SettingsAuthor,
        at_ns: i64,
    ) -> Result<()> {
        let said = self
            .inner
            .put_plugin_settings(plugin_instance_id, changes, author, at_ns);
        self.changed();
        said
    }

    fn plugin_setting_changes(&self, plugin_instance_id: &str) -> Result<Vec<SettingChangeRecord>> {
        self.inner.plugin_setting_changes(plugin_instance_id)
    }

    fn record_plugin_version(&self, version: &PluginVersion) -> Result<bool> {
        let said = self.inner.record_plugin_version(version);
        self.changed();
        said
    }

    fn record_plugin_version_documented(
        &self,
        version: &PluginVersion,
        documentation: &[meridian_domain::v1::DocumentationText],
    ) -> Result<bool> {
        let said = self
            .inner
            .record_plugin_version_documented(version, documentation);
        self.changed();
        said
    }

    fn plugin_documentation(
        &self,
        name: &str,
        version: &str,
    ) -> Result<Vec<meridian_domain::v1::DocumentationText>> {
        self.inner.plugin_documentation(name, version)
    }

    fn retire_plugin_version(
        &self,
        name: &str,
        version: &str,
        author: &Author,
        at_ns: i64,
    ) -> Result<Option<PluginVersion>> {
        let said = self
            .inner
            .retire_plugin_version(name, version, author, at_ns);
        self.changed();
        said
    }

    fn retirement(&self, name: &str, version: &str) -> Result<Option<Author>> {
        self.inner.retirement(name, version)
    }

    fn begin_launch(&self, launch: &PluginLaunch, note: &str) -> Result<bool> {
        let said = self.inner.begin_launch(launch, note);
        self.changed();
        said
    }

    fn end_launch(&self, instance_id: &str, ending: &Ending) -> Result<Option<PluginLaunch>> {
        let said = self.inner.end_launch(instance_id, ending);
        self.changed();
        said
    }

    fn launch_notes(&self, instance_id: &str) -> Result<Vec<LaunchNote>> {
        self.inner.launch_notes(instance_id)
    }

    fn set_hold(&self, hold: &Hold, note: &str) -> Result<()> {
        let said = self.inner.set_hold(hold, note);
        self.changed();
        said
    }

    fn hold_changes(&self) -> Result<Vec<(Hold, String)>> {
        self.inner.hold_changes()
    }

    fn put_archive(&self, archive: &PluginArchive, note: &str) -> Result<()> {
        let said = self.inner.put_archive(archive, note);
        self.changed();
        said
    }

    fn archive_changes(&self, instance_id: &str) -> Result<Vec<(PluginArchive, String)>> {
        self.inner.archive_changes(instance_id)
    }

    fn record_move(&self, instance_id: &str, record: &MoveRecord) -> Result<bool> {
        let said = self.inner.record_move(instance_id, record);
        self.changed();
        said
    }

    fn latest_move(
        &self,
        instance_id: &str,
        record_kind: &str,
        unit: &str,
    ) -> Result<Option<MoveRecord>> {
        self.inner.latest_move(instance_id, record_kind, unit)
    }

    fn moves(&self, instance_id: &str, before: i64, limit: usize) -> Result<Vec<RecordedMove>> {
        self.inner.moves(instance_id, before, limit)
    }

    fn archived(&self, instance_id: &str) -> Result<Vec<StoredSpan>> {
        self.inner.archived(instance_id)
    }

    fn record_catalogue(
        &self,
        instance_id: &str,
        catalogue: &Catalogue,
        at_ns: i64,
    ) -> Result<bool> {
        let said = self.inner.record_catalogue(instance_id, catalogue, at_ns);
        self.changed();
        said
    }

    fn set_dataset_licence(&self, licence: &DatasetLicence) -> Result<()> {
        let said = self.inner.set_dataset_licence(licence);
        self.changed();
        said
    }

    fn set_dataset_entitlement(&self, entitlement: &DatasetEntitlement) -> Result<()> {
        let said = self.inner.set_dataset_entitlement(entitlement);
        self.changed();
        said
    }

    fn dataset_changes(
        &self,
        dataset: &str,
    ) -> Result<(Vec<DatasetLicence>, Vec<DatasetEntitlement>)> {
        self.inner.dataset_changes(dataset)
    }

    fn set_linkable_accounts(
        &self,
        plugin_instance_id: &str,
        grant: &[String],
        withdraw: &[String],
        author: &Author,
        at_ns: i64,
    ) -> Result<LinkableAccounts> {
        let said =
            self.inner
                .set_linkable_accounts(plugin_instance_id, grant, withdraw, author, at_ns);
        self.changed();
        said
    }

    fn close_account_withdrawing_grants(
        &self,
        account: &AccountRecord,
        author: &Author,
        at_ns: i64,
    ) -> Result<Vec<String>> {
        let said = self
            .inner
            .close_account_withdrawing_grants(account, author, at_ns);
        self.changed();
        said
    }

    fn linkable_accounts_changes(
        &self,
        plugin_instance_id: &str,
    ) -> Result<Vec<LinkableAccountsChange>> {
        self.inner.linkable_accounts_changes(plugin_instance_id)
    }

    fn set_connectivity_link(
        &self,
        link: &ConnectivityLink,
        author: &Author,
        at_ns: i64,
    ) -> Result<ConnectivityLink> {
        let said = self.inner.set_connectivity_link(link, author, at_ns);
        self.changed();
        said
    }

    fn connectivity_link_changes(&self, instance_id: &str) -> Result<Vec<ConnectivityLinkChange>> {
        self.inner.connectivity_link_changes(instance_id)
    }

    fn map_venue_account(
        &self,
        ems_instance_id: &str,
        from: Option<&VenueAccountMapping>,
        to: Option<&VenueAccountMapping>,
        author: &Author,
        at_ns: i64,
    ) -> Result<()> {
        let said = self
            .inner
            .map_venue_account(ems_instance_id, from, to, author, at_ns);
        self.changed();
        said
    }

    fn venue_account_map_changes(
        &self,
        ems_instance_id: &str,
    ) -> Result<Vec<VenueAccountMapChange>> {
        self.inner.venue_account_map_changes(ems_instance_id)
    }

    fn keep_venue_catalogue(&self, catalogue: &VenueCatalogue, at_ns: i64) -> Result<bool> {
        let said = self.inner.keep_venue_catalogue(catalogue, at_ns);
        self.changed();
        said
    }

    fn venue_catalogue(&self, instance_id: &str, code: &str) -> Result<Option<VenueCatalogue>> {
        self.inner.venue_catalogue(instance_id, code)
    }

    fn set_deployment_time_zone(&self, zone: &str, author: &Author, at_ns: i64) -> Result<()> {
        let said = self.inner.set_deployment_time_zone(zone, author, at_ns);
        self.changed();
        said
    }

    fn deployment_time_zone(&self) -> Result<Option<TimeZoneChange>> {
        self.inner.deployment_time_zone()
    }

    fn set_fx_pivot_currencies(
        &self,
        pivots: &[String],
        author: &Author,
        at_ns: i64,
    ) -> Result<()> {
        let said = self.inner.set_fx_pivot_currencies(pivots, author, at_ns);
        self.changed();
        said
    }

    fn fx_pivot_currencies(&self) -> Result<Option<FxPivotChange>> {
        self.inner.fx_pivot_currencies()
    }

    fn set_model_routing(
        &self,
        model_id: &str,
        implementation: &str,
        author: &Author,
        at_ns: i64,
    ) -> Result<()> {
        let said = self
            .inner
            .set_model_routing(model_id, implementation, author, at_ns);
        self.changed();
        said
    }

    fn model_routing_changes(&self) -> Result<Vec<ModelRoutingChange>> {
        self.inner.model_routing_changes()
    }

    fn set_setting(&self, setting: &str, value: &[u8], author: &Author, at_ns: i64) -> Result<()> {
        let said = self.inner.set_setting(setting, value, author, at_ns);
        self.changed();
        said
    }

    fn setting(&self, setting: &str) -> Result<Option<DeploymentSettingChange>> {
        self.inner.setting(setting)
    }

    fn set_reports_usage(&self, reports_usage: bool, author: &Author, at_ns: i64) -> Result<()> {
        let said = self.inner.set_reports_usage(reports_usage, author, at_ns);
        self.changed();
        said
    }

    fn reports_usage(&self) -> Result<bool> {
        self.inner.reports_usage()
    }

    fn set_licensed_schemes(&self, schemes: &[String], author: &Author, at_ns: i64) -> Result<()> {
        let said = self.inner.set_licensed_schemes(schemes, author, at_ns);
        self.changed();
        said
    }

    fn licensed_schemes(&self) -> Result<Vec<String>> {
        self.inner.licensed_schemes()
    }

    fn record_usage_report(&self, report: &meridian_domain::v1::SentUsageReport) -> Result<()> {
        let said = self.inner.record_usage_report(report);
        self.changed();
        said
    }

    fn usage_reports(&self, limit: usize) -> Result<Vec<meridian_domain::v1::SentUsageReport>> {
        self.inner.usage_reports(limit)
    }

    fn registry_catalogue(&self) -> Result<Option<HeldCatalogue>> {
        self.inner.registry_catalogue()
    }

    fn put_registry_catalogue(&self, held: &HeldCatalogue) -> Result<()> {
        let said = self.inner.put_registry_catalogue(held);
        self.changed();
        said
    }

    fn record_listed_version(
        &self,
        version: &PluginVersion,
        documentation: &[meridian_domain::v1::DocumentationText],
        listing_statement: &str,
        by: &Author,
        pack: &str,
    ) -> Result<bool> {
        let said =
            self.inner
                .record_listed_version(version, documentation, listing_statement, by, pack);
        self.changed();
        said
    }

    fn install_pack(&self, name: &str, version: &str) -> Result<String> {
        let said = self.inner.install_pack(name, version);
        self.changed();
        said
    }

    fn record_pack_change(&self, change: &PackChange) -> Result<()> {
        let said = self.inner.record_pack_change(change);
        self.changed();
        said
    }

    fn pack_changes(&self, pack: &str) -> Result<Vec<PackChange>> {
        self.inner.pack_changes(pack)
    }

    fn put_pack_job(&self, job: &PackJob) -> Result<()> {
        let said = self.inner.put_pack_job(job);
        self.changed();
        said
    }

    fn pack_jobs(&self) -> Result<Vec<PackJob>> {
        self.inner.pack_jobs()
    }

    fn end_pack_job(&self, pack: &str) -> Result<()> {
        let said = self.inner.end_pack_job(pack);
        self.changed();
        said
    }

    fn installation(&self, name: &str, version: &str) -> Result<Option<Author>> {
        self.inner.installation(name, version)
    }

    fn listing_statement(&self, name: &str, version: &str) -> Result<String> {
        self.inner.listing_statement(name, version)
    }

    fn acknowledge_pull(
        &self,
        instance_id: &str,
        acknowledgement: &meridian_domain::v1::PullAcknowledgement,
    ) -> Result<Option<PluginLaunch>> {
        let said = self.inner.acknowledge_pull(instance_id, acknowledgement);
        self.changed();
        said
    }

    fn set_reports_plugin_usage(&self, on: bool, author: &Author, at_ns: i64) -> Result<()> {
        let said = self.inner.set_reports_plugin_usage(on, author, at_ns);
        self.changed();
        said
    }

    fn reports_plugin_usage(&self) -> Result<bool> {
        self.inner.reports_plugin_usage()
    }

    fn record_plugin_usage_report(
        &self,
        report: &meridian_domain::v1::SentPluginUsageReport,
    ) -> Result<()> {
        let said = self.inner.record_plugin_usage_report(report);
        self.changed();
        said
    }

    fn plugin_usage_reports(
        &self,
        limit: usize,
    ) -> Result<Vec<meridian_domain::v1::SentPluginUsageReport>> {
        self.inner.plugin_usage_reports(limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryStore;

    fn shared() -> Shared {
        Shared::new(Arc::new(MemoryStore::default()))
    }

    #[test]
    fn one_snapshot_answers_every_read_until_a_change_and_a_change_is_read_at_once() {
        let store = shared();
        let first = store.snapshot_shared().unwrap();
        let again = store.snapshot_shared().unwrap();
        assert!(Arc::ptr_eq(&first, &again), "shared until it changes");
        store
            .put_account(&AccountRecord {
                account_id: "ACC-1".into(),
                name: "One".into(),
                ..Default::default()
            })
            .unwrap();
        let after = store.snapshot_shared().unwrap();
        assert!(!Arc::ptr_eq(&first, &after), "read again after a change");
        assert!(after
            .records
            .accounts
            .iter()
            .any(|a| a.account_id == "ACC-1"));
        // An owned snapshot is the shared one's copy.
        assert_eq!(
            store.snapshot().unwrap().records.accounts.len(),
            after.records.accounts.len()
        );
    }

    #[test]
    fn what_another_process_wrote_is_seen_once_the_snapshot_is_older_than_a_second() {
        let inner = Arc::new(MemoryStore::default());
        let store = Shared::new(Arc::clone(&inner) as Arc<dyn Store>);
        let first = store.snapshot_shared().unwrap();
        // Written past the wrapper, as a migration or a restore would.
        inner
            .put_account(&AccountRecord {
                account_id: "ACC-2".into(),
                ..Default::default()
            })
            .unwrap();
        assert!(Arc::ptr_eq(&first, &store.snapshot_shared().unwrap()));
        std::thread::sleep(FRESH_FOR + Duration::from_millis(50));
        let later = store.snapshot_shared().unwrap();
        assert!(later
            .records
            .accounts
            .iter()
            .any(|a| a.account_id == "ACC-2"));
    }

    #[test]
    fn questions_asked_at_once_share_one_read() {
        let store = Arc::new(shared());
        let reads: Vec<_> = (0..8)
            .map(|_| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || store.snapshot_shared().unwrap())
            })
            .collect();
        let held: Vec<Arc<Snapshot>> = reads.into_iter().map(|r| r.join().unwrap()).collect();
        assert!(held.iter().all(|one| Arc::ptr_eq(one, &held[0])));
    }
}
