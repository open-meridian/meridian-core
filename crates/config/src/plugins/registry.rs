//! The registries' rows the dashboard asks the conductor (W8.10 to W8.12,
//! W5.19; contract v25): Browse's copy of the catalogue, installing a
//! listed version, moving an instance to a newer or a listed version,
//! keeping an instance whose version was pulled, and the plugin usage
//! report's setting and the reports sent.

use std::collections::BTreeSet;

use meridian_bus::Envelope;
use meridian_domain::v1::{
    AcknowledgePullRequest, CreatePluginReply, InstallPluginRequest, ListPluginUsageReportsReply,
    ListPluginUsageReportsRequest, ListRegistryPluginsReply, ListRegistryPluginsRequest,
    PluginLaunch, PluginLaunchState, PluginVersion, PullAcknowledgement, RecordPluginUploadRequest,
    RemovePluginReply, RemovePluginRequest, ReportsPluginUsageSetting,
    SetReportsPluginUsageRequest, UpdatePluginRequest,
};

use super::{archive_allowed, documented, Plugins, CREATE_PLUGIN, REMOVE_PLUGIN};
use crate::registries::{is_prerelease, offered, semver_cmp, Bundle, Hold};
use crate::service::{author, subject};
use crate::store::{note_refused, Author, Ending};

/// W8.10: Browse's copy of the catalogue.
pub const REGISTRY_PLUGINS: &str = "platform.config.query.registry-plugins";
/// W8.10: install a listed version.
pub const INSTALL_PLUGIN: &str = "platform.config.command.install-plugin";
/// W8.11: move an instance to a newer or a listed version.
pub const UPDATE_PLUGIN: &str = "platform.config.command.update-plugin";
/// W8.12: keep an instance whose version was pulled.
pub const ACKNOWLEDGE_PULL: &str = "platform.config.command.acknowledge-pull";
/// W5.19: the plugin usage report's setting.
pub const SET_REPORTS_PLUGIN_USAGE: &str = "platform.config.command.set-reports-plugin-usage";
/// W5.19: the reports sent, the setting and the catalogue's age.
pub const LIST_PLUGIN_USAGE_REPORTS: &str = "platform.config.query.list-plugin-usage-reports";

/// The most reports a page of the Registries tab reads at once.
const MOST_REPORTS: usize = 200;

impl Plugins {
    fn registries(&self) -> Result<&std::sync::Arc<Hold>, String> {
        self.registries.as_ref().ok_or_else(|| {
            "this deployment reaches no registry: it is not enrolled with the platform, or its \
             chart carries no platform key"
                .to_string()
        })
    }

    fn block_on<T>(&self, work: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Handle::current().block_on(work)
    }

    /// The version recorded under a name and a version.
    fn recorded(&self, name: &str, version: &str) -> Result<Option<PluginVersion>, String> {
        Ok(self
            .snapshot()?
            .catalogue
            .versions
            .into_iter()
            .find(|held| {
                held.metadata
                    .as_ref()
                    .is_some_and(|m| m.name == name && m.version == version)
            }))
    }

    pub(super) fn list_registry_plugins(
        &self,
        request: ListRegistryPluginsRequest,
    ) -> Result<ListRegistryPluginsReply, String> {
        let hold = self.registries()?;
        self.block_on(hold.list(&request))
    }

    /// Resolve, pull, verify and record a listed version (W8.10): its
    /// documentation, from the image's verified metadata, held to its
    /// declaration's digests, and its listing statement kept for launch.
    fn install_version(
        &self,
        listing: &str,
        version: &str,
        registry: &str,
        note: &str,
        envelope: &Envelope,
    ) -> Result<PluginVersion, String> {
        let hold = self.registries()?;
        let fetched = self.block_on(hold.fetch(listing, version, registry))?;
        let declared = fetched
            .version
            .metadata
            .as_ref()
            .and_then(|m| m.declaration.as_ref())
            .map(|d| d.documentation.as_slice())
            .unwrap_or_default();
        documented(declared, &fetched.documentation)
            .map_err(|why| format!("the documentation: {why}"))?;
        // The install is its own record (decisions/031): who, through what,
        // and why, written once with the version.
        let by = Author {
            note: note.to_string(),
            ..author(envelope)
        };
        let mut recorded = fetched.version;
        recorded.uploaded_by = by.by.clone();
        recorded.uploaded_at_ns = self.clock.now_ns();
        recorded.uploaded_note = by.note.clone();
        if !self
            .store
            .record_listed_version(&recorded, &fetched.documentation, &fetched.statement, &by)
            .map_err(|failed| failed.to_string())?
        {
            return Err(format!("{listing} {version} is installed already"));
        }
        tracing::info!(
            listing,
            version,
            registry = recorded.source_registry,
            by = recorded.uploaded_by,
            "a version installed from a registry"
        );
        Ok(recorded)
    }

    /// RecordPluginUpload carrying a plugin bundle (W8.14; the spec's
    /// requirement 45): the image and its referrers are in plugins/{listing}
    /// already; verified against the chart's keys as at install, and
    /// recorded under its listing with its source registry, its listing
    /// statement for launch and the age of the key status it was checked
    /// against. What the CLI read from the bundle -- the declaration, the
    /// texts -- must be what the built image attests.
    pub(super) fn carry(
        &self,
        request: RecordPluginUploadRequest,
        envelope: &Envelope,
    ) -> Result<PluginVersion, String> {
        let hold = self.registries.as_ref().ok_or(
            "a plugin bundle is verified against the platform's keys, and this deployment's \
             chart carries none",
        )?;
        let version = request
            .metadata
            .as_ref()
            .map(|m| m.version.clone())
            .unwrap_or_default();
        if self.recorded(&request.listing, &version)?.is_some() {
            return Err(format!(
                "{} {version} is already recorded; a recorded version is never replaced",
                request.listing
            ));
        }
        let fetched = self.block_on(hold.carried(&Bundle {
            listing: &request.listing,
            version: &version,
            registry: &request.source_registry,
            digest: &request.image_digest,
            key_status: &request.key_status,
            listing_statement: &request.listing_statement,
        }))?;
        let attested = fetched.version.metadata.clone().unwrap_or_default();
        if request
            .metadata
            .as_ref()
            .is_some_and(|m| m.declaration.is_some() && m.declaration != attested.declaration)
        {
            return Err(
                "the declaration: the bundle's is not the one the built image carries".into(),
            );
        }
        if !request.documentation.is_empty() && request.documentation != fetched.documentation {
            return Err(
                "the documentation: the bundle's texts are not the ones the built image attests"
                    .into(),
            );
        }
        let declared = attested
            .declaration
            .as_ref()
            .map(|d| d.documentation.as_slice())
            .unwrap_or_default();
        documented(declared, &fetched.documentation)
            .map_err(|why| format!("the documentation: {why}"))?;
        let mut recorded = fetched.version;
        recorded.uploaded_by = subject(envelope);
        recorded.uploaded_at_ns = self.clock.now_ns();
        if !self
            .store
            .record_listed_version(
                &recorded,
                &fetched.documentation,
                &fetched.statement,
                &author(envelope),
            )
            .map_err(|failed| failed.to_string())?
        {
            return Err(format!(
                "{} {version} is already recorded; a recorded version is never replaced",
                request.listing
            ));
        }
        tracing::info!(
            listing = request.listing,
            version,
            registry = recorded.source_registry,
            key_status_issued_at_ns = recorded.key_status_issued_at_ns,
            by = recorded.uploaded_by,
            "a version carried in by a plugin bundle"
        );
        Ok(recorded)
    }

    /// InstallPlugin (W8.10): a deployment admin's act; launches nothing.
    pub(super) fn install(
        &self,
        request: InstallPluginRequest,
        envelope: &Envelope,
    ) -> Result<PluginVersion, String> {
        if let Some(refusal) = note_refused(&request.note) {
            return Err(refusal);
        }
        if self.recorded(&request.listing, &request.version)?.is_some() {
            return Err(format!(
                "{} {} is installed already",
                request.listing, request.version
            ));
        }
        let installed = self.install_version(
            &request.listing,
            &request.version,
            &request.registry,
            &request.note,
            envelope,
        )?;
        Ok(installed)
    }

    /// UpdatePlugin (W8.11): an instance moved to a newer version from its
    /// source registry, or a local version's instance to a listed version
    /// of the same plugin; the version resolved, pulled and verified as an
    /// install is where it is not installed yet, and the roles approved
    /// exactly the new version's. The instance keeps its ID, and with it its
    /// settings, storage and access; the move is its own record.
    pub(super) fn update(
        &self,
        request: UpdatePluginRequest,
        envelope: &Envelope,
    ) -> Result<PluginLaunch, String> {
        if let Some(refusal) = note_refused(&request.note) {
            return Err(refusal);
        }
        let snapshot = self.snapshot()?;
        let running = snapshot
            .catalogue
            .launches
            .iter()
            .find(|held| {
                held.instance_id == request.instance_id
                    && held.state == PluginLaunchState::Launched as i32
            })
            .cloned()
            .ok_or_else(|| format!("no launch of {} is live", request.instance_id))?;
        let current = snapshot
            .catalogue
            .versions
            .iter()
            .find(|held| {
                held.metadata
                    .as_ref()
                    .is_some_and(|m| m.name == running.name && m.version == running.version)
            })
            .cloned()
            .ok_or_else(|| {
                format!(
                    "{} {} is not in the catalogue",
                    running.name, running.version
                )
            })?;
        let hold = self.registries()?;
        let (_, target) = hold.listed(&request.listing, &request.version, "")?;
        if current.listing.is_empty() {
            // A local instance moved to a listing (plan Q9): the listing's
            // plugin part is the local version's bare name.
            let plugin = request.listing.split('/').nth(1).unwrap_or_default();
            if plugin != running.name {
                return Err(format!(
                    "{} runs the local plugin {}, and {} is another plugin; only a listing of \
                     the same plugin takes it",
                    request.instance_id, running.name, request.listing
                ));
            }
        } else {
            if request.listing != current.listing {
                return Err(format!(
                    "{} runs {}; an update is offered from its own listing",
                    request.instance_id, current.listing
                ));
            }
            if target.registry != current.source_registry {
                return Err(format!(
                    "{} runs a {} version; updates are offered from its source registry only, and \
                     moving to {} is an install of that version, checked as a widening",
                    request.instance_id, current.source_registry, target.registry
                ));
            }
            match semver_cmp(&target.version, &running.version) {
                Some(std::cmp::Ordering::Greater) => {}
                _ => {
                    return Err(format!(
                        "{} is not newer than {}, which {} runs",
                        target.version, running.version, request.instance_id
                    ))
                }
            }
            if is_prerelease(&target.version) && !is_prerelease(&running.version) {
                return Err(format!(
                    "{} is a pre-release, never offered as an update to a release",
                    target.version
                ));
            }
        }
        if !offered(&target) {
            return Err(format!(
                "{} {} is not offered: it was withdrawn or pulled",
                request.listing, request.version
            ));
        }
        let version = match self.recorded(&request.listing, &request.version)? {
            Some(held) => {
                let statement = self
                    .store
                    .listing_statement(&request.listing, &request.version)
                    .map_err(|failed| failed.to_string())?;
                self.block_on(hold.reverify(&held, &statement))
                    .map_err(|why| format!("the verification at launch: {why}"))?;
                held
            }
            None => self.install_version(
                &request.listing,
                &request.version,
                "",
                &request.note,
                envelope,
            )?,
        };
        let metadata = version.metadata.clone().unwrap_or_default();
        let approved: BTreeSet<&str> = request.approved_roles.iter().map(String::as_str).collect();
        let declared: BTreeSet<&str> = metadata.roles.iter().map(String::as_str).collect();
        if approved != declared {
            return Err(format!(
                "the approval names roles {}, and {} {} asks for {}; an approval of something \
                 other than what runs is no approval",
                super::listed(&approved),
                request.listing,
                request.version,
                super::listed(&declared)
            ));
        }
        let widening = !declared.is_subset(&running.roles.iter().map(String::as_str).collect());
        // Removed first, then recorded: the old launch ended as stopped,
        // the move noted on it, and the instance begun again on the new
        // version under its own ID.
        let _: RemovePluginReply = self.ask_launcher(
            REMOVE_PLUGIN,
            "meridian.v1.RemovePluginRequest",
            RemovePluginRequest {
                instance_id: request.instance_id.clone(),
            },
        )?;
        let by = author(envelope);
        let ending = Ending {
            state: PluginLaunchState::Stopped,
            by: by.by.clone(),
            delegation: by.delegation.clone(),
            client: by.client.clone(),
            note: format!(
                "moved to {} {}{}",
                request.listing,
                request.version,
                if request.note.is_empty() {
                    String::new()
                } else {
                    format!(": {}", request.note)
                }
            ),
            at_ns: self.clock.now_ns(),
            failure: String::new(),
            prepared_call_id: by.prepared_call_id.clone(),
        };
        self.store
            .end_launch(&request.instance_id, &ending)
            .map_err(|failed| failed.to_string())?;
        let moved = PluginLaunch {
            instance_id: request.instance_id.clone(),
            name: metadata.name.clone(),
            version: metadata.version.clone(),
            image_digest: version.image_digest.clone(),
            roles: metadata.roles.clone(),
            launched_by: by.by,
            launched_at_ns: self.clock.now_ns(),
            state: PluginLaunchState::Launched as i32,
            live: running.live,
            acting_through_delegation: by.delegation,
            client_name: by.client,
            note: request.note.clone(),
            prepared_call_id: by.prepared_call_id,
            ..Default::default()
        };
        self.store
            .begin_launch(&moved, &request.note)
            .map_err(|failed| failed.to_string())?;
        let create = self.create_request(
            &request.instance_id,
            &version,
            running.live,
            archive_allowed(&snapshot, &request.instance_id).cloned(),
        );
        match self.ask_launcher::<_, CreatePluginReply>(
            CREATE_PLUGIN,
            "meridian.v1.CreatePluginRequest",
            create,
        ) {
            Ok(_) => {
                tracing::info!(
                    instance = moved.instance_id,
                    from = format!("{} {}", running.name, running.version),
                    to = format!("{} {}", moved.name, moved.version),
                    widening,
                    by = moved.launched_by,
                    "an instance moved to another version"
                );
                crate::data::publish(&self.bus, self.store.as_ref(), self.clock.now_ns());
                Ok(moved)
            }
            Err(failed) => {
                let ending = Ending::failed(self.clock.now_ns(), failed.clone());
                self.store
                    .end_launch(&request.instance_id, &ending)
                    .map_err(|f| f.to_string())?;
                Err(format!(
                    "the launcher did not create {} on {} {}: {failed}",
                    request.instance_id, request.listing, request.version
                ))
            }
        }
    }

    /// AcknowledgePull (W8.12): a deployment admin keeps an instance whose
    /// version was pulled, with a note; its own record, kept on the launch.
    pub(super) fn acknowledge_pull(
        &self,
        request: AcknowledgePullRequest,
        envelope: &Envelope,
    ) -> Result<PullAcknowledgement, String> {
        if let Some(refusal) = note_refused(&request.note) {
            return Err(refusal);
        }
        let snapshot = self.snapshot()?;
        let running = snapshot
            .catalogue
            .launches
            .iter()
            .find(|held| {
                held.instance_id == request.instance_id
                    && held.state == PluginLaunchState::Launched as i32
            })
            .ok_or_else(|| format!("no launch of {} is live", request.instance_id))?;
        let version = snapshot
            .catalogue
            .versions
            .iter()
            .find(|held| {
                held.metadata
                    .as_ref()
                    .is_some_and(|m| m.name == running.name && m.version == running.version)
            })
            .ok_or_else(|| {
                format!(
                    "{} {} is not in the catalogue",
                    running.name, running.version
                )
            })?;
        if version.listing.is_empty() {
            return Err(format!(
                "{} runs a local version, which no registry pulls",
                request.instance_id
            ));
        }
        let (_, listed) = self.registries()?.listed(
            &version.listing,
            &running.version,
            &version.source_registry,
        )?;
        let pull = listed
            .pull
            .filter(|pull| pull.restored_at_ns == 0)
            .ok_or_else(|| {
                format!(
                    "{} {} is not pulled; there is nothing to acknowledge",
                    version.listing, running.version
                )
            })?;
        let by = author(envelope);
        let acknowledgement = PullAcknowledgement {
            pull: Some(pull),
            acknowledged_by: by.by,
            acknowledged_at_ns: self.clock.now_ns(),
            acting_through_delegation: by.delegation,
            client_name: by.client,
            note: request.note,
        };
        self.store
            .acknowledge_pull(&request.instance_id, &acknowledgement)
            .map_err(|failed| failed.to_string())?
            .ok_or_else(|| format!("no launch of {} is live", request.instance_id))?;
        Ok(acknowledgement)
    }

    /// SetReportsPluginUsage (W5.19): its own record.
    pub(super) fn set_reports_plugin_usage(
        &self,
        request: SetReportsPluginUsageRequest,
        envelope: &Envelope,
    ) -> Result<ReportsPluginUsageSetting, String> {
        if let Some(refusal) = note_refused(&request.reason) {
            return Err(refusal);
        }
        let mut by = author(envelope);
        by.note = request.reason;
        self.store
            .set_reports_plugin_usage(request.reports_plugin_usage, &by, self.clock.now_ns())
            .map_err(|failed| failed.to_string())?;
        Ok(ReportsPluginUsageSetting {
            reports_plugin_usage: request.reports_plugin_usage,
        })
    }

    /// ListPluginUsageReports (W5.19): the Registries tab.
    pub(super) fn list_plugin_usage_reports(
        &self,
        request: ListPluginUsageReportsRequest,
    ) -> Result<ListPluginUsageReportsReply, String> {
        let size = usize::try_from(request.page_size)
            .ok()
            .filter(|size| *size > 0)
            .unwrap_or(50)
            .min(MOST_REPORTS);
        let start: usize = request.cursor.parse().unwrap_or(0);
        let all = self
            .store
            .plugin_usage_reports(start + size + 1)
            .map_err(|failed| failed.to_string())?;
        let next_cursor = if all.len() > start + size {
            (start + size).to_string()
        } else {
            String::new()
        };
        Ok(ListPluginUsageReportsReply {
            reports: all.into_iter().skip(start).take(size).collect(),
            next_cursor,
            reports_plugin_usage: self
                .store
                .reports_plugin_usage()
                .map_err(|failed| failed.to_string())?,
            catalogue_pulled_at_ns: self
                .store
                .registry_catalogue()
                .map_err(|failed| failed.to_string())?
                .map_or(0, |held| held.pulled_at_ns),
        })
    }
}

#[cfg(test)]
mod tests;
