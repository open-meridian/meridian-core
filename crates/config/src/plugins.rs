//! Bringing a plugin into a deployment: the catalogue, and launching and
//! stopping (W8, spec/the-local-plugin-registry).
//!
//! The conductor decides and records; the launcher only acts, and only when
//! the conductor asks (decisions/019). A version arrives with its metadata --
//! roles from the deployment's fixed list, its own tags -- and its image's
//! digest; it is recorded once and never replaced. A launch runs a recorded
//! version as an instance, with exactly the roles and tags it declares, which
//! is what the administrator was shown and approved: an approval of anything
//! else is no approval. Its grants are its roles', generated from the matrix,
//! and nothing here writes a grant or a topic.
//!
//! The dashboard asks, for a deployment admin, and is the only one the broker
//! lets ask; who that was comes from the envelope, and is what is recorded.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use meridian_bus::{Bus, Envelope};
use meridian_domain::v1::{
    CreatePluginReply, CreatePluginRequest, LaunchPluginRequest, PluginCatalogue,
    PluginCatalogueRequest, PluginLaunch, PluginLaunchState, PluginMetadata, PluginVersion,
    RecordPluginUploadRequest, RemovePluginReply, RemovePluginRequest, StopPluginRequest,
};
use meridian_sidecar::Contract;
use prost::Message;

use crate::service::{answer_on, subject, Clock};
use crate::store::{Ending, Snapshot, Store};

pub const RECORD_PLUGIN_UPLOAD: &str = "platform.config.command.record-plugin-upload";
pub const PLUGIN_CATALOGUE: &str = "platform.config.query.plugin-catalogue";
pub const LAUNCH_PLUGIN: &str = "platform.config.command.launch-plugin";
pub const STOP_PLUGIN: &str = "platform.config.command.stop-plugin";
pub const CREATE_PLUGIN: &str = "platform.deployment.command.create-plugin";
pub const REMOVE_PLUGIN: &str = "platform.deployment.command.remove-plugin";

/// How long the launcher has to make or remove a plugin's workload: it asks
/// the cluster's API and waits for nothing to start.
const LAUNCHER: Duration = Duration::from_secs(20);

struct Plugins {
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    /// The deployment's own registry, as each node reaches it:
    /// `localhost:<port>`. Every launch runs an image from here, by digest.
    registry: String,
}

// ── The rules ───────────────────────────────────────────────────────────────

/// Lowercase letters, digits and single hyphens, a letter first and no
/// hyphen last: a plugin's name, its registry repository, and an instance.
pub fn is_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes[bytes.len() - 1] != b'-'
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && !name.contains("--")
}

fn is_digest(digest: &str) -> bool {
    digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn is_version(version: &str) -> bool {
    (1..=64).contains(&version.len())
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}

/// What an upload declares, held to the rules the CLI already checked:
/// the CLI is a convenience, and this is the rule (W8.1).
pub fn upload(
    contract: &Contract,
    request: &RecordPluginUploadRequest,
) -> Result<PluginMetadata, String> {
    let metadata = request
        .metadata
        .clone()
        .ok_or("an upload carries no metadata")?;
    if !is_name(&metadata.name) {
        return Err(format!(
            "`{}` is not a plugin's name: lowercase letters, digits and single hyphens, \
             starting with a letter",
            metadata.name
        ));
    }
    if !is_version(&metadata.version) {
        return Err(format!("`{}` is not a version", metadata.version));
    }
    let mut roles = BTreeSet::new();
    for role in &metadata.roles {
        if contract.is_component(role) {
            return Err(format!(
                "`{role}` is one of the deployment's own components, which a plugin never declares"
            ));
        }
        contract
            .grants_for(std::slice::from_ref(role))
            .map_err(|_| {
                format!("`{role}` is not a role; a plugin's roles are from matrix/roles.tsv")
            })?;
        if !roles.insert(role) {
            return Err(format!("`{role}` is declared twice"));
        }
    }
    let mut tags = BTreeSet::new();
    for tag in &metadata.tags {
        if !is_name(tag) {
            return Err(format!(
                "`{tag}` is not a tag: lowercase letters, digits and single hyphens"
            ));
        }
        if !tags.insert(tag) {
            return Err(format!("the tag `{tag}` is declared twice"));
        }
    }
    if metadata.sdk_version.is_empty() {
        return Err("an upload names no SDK version".into());
    }
    if !is_digest(&request.image_digest) {
        return Err(format!(
            "`{}` is not an image digest; a version is recorded by `sha256:<hex>`",
            request.image_digest
        ));
    }
    Ok(metadata)
}

fn set(items: &[String]) -> BTreeSet<&str> {
    items.iter().map(String::as_str).collect()
}

fn listed(items: &BTreeSet<&str>) -> String {
    if items.is_empty() {
        "none".into()
    } else {
        items.iter().copied().collect::<Vec<_>>().join(", ")
    }
}

/// The version a launch runs, if it is recorded and the approval is exactly
/// what it declares (W8.3).
pub fn launch<'a>(
    snapshot: &'a Snapshot,
    request: &LaunchPluginRequest,
) -> Result<&'a PluginVersion, String> {
    if !is_name(&request.instance_id) {
        return Err(format!(
            "`{}` is not an instance: lowercase letters, digits and single hyphens, starting \
             with a letter, at most 63",
            request.instance_id
        ));
    }
    let version = snapshot
        .catalogue
        .versions
        .iter()
        .find(|held| {
            held.metadata
                .as_ref()
                .is_some_and(|m| m.name == request.name && m.version == request.version)
        })
        .ok_or_else(|| {
            format!(
                "{} {} is not in the catalogue",
                request.name, request.version
            )
        })?;
    let metadata = version.metadata.as_ref().expect("found by it");
    for (what, approved, declared) in [
        ("roles", &request.approved_roles, &metadata.roles),
        ("tags", &request.approved_tags, &metadata.tags),
    ] {
        let (approved, declared) = (set(approved), set(declared));
        if approved != declared {
            return Err(format!(
                "the approval names {what} {}, and {} {} declares {}; an approval of \
                 something other than what runs is no approval",
                listed(&approved),
                request.name,
                request.version,
                listed(&declared)
            ));
        }
    }
    Ok(version)
}

// ── Serving them ────────────────────────────────────────────────────────────

impl Plugins {
    fn snapshot(&self) -> Result<Snapshot, String> {
        self.store.snapshot().map_err(|failed| failed.to_string())
    }

    /// Ask the launcher, from a handler's blocking thread.
    fn ask_launcher<Q: Message, A: Message + Default>(
        &self,
        topic: &str,
        payload_type: &str,
        request: Q,
    ) -> Result<A, String> {
        let asking = self.bus.call(
            topic,
            payload_type,
            request.encode_to_vec(),
            None,
            Some(LAUNCHER),
        );
        let (_, answer) = tokio::runtime::Handle::current()
            .block_on(asking)
            .map_err(|failed| failed.to_string())?;
        A::decode(answer.as_slice())
            .map_err(|failed| format!("the launcher's answer did not read: {failed}"))
    }

    fn record_upload(
        &self,
        request: RecordPluginUploadRequest,
        envelope: &Envelope,
    ) -> Result<PluginVersion, String> {
        let metadata = upload(Contract::embedded(), &request)?;
        let version = PluginVersion {
            metadata: Some(metadata.clone()),
            image_digest: request.image_digest,
            uploaded_by: subject(envelope),
            uploaded_at_ns: self.clock.now_ns(),
        };
        if !self
            .store
            .record_plugin_version(&version)
            .map_err(|f| f.to_string())?
        {
            return Err(format!(
                "{} {} is already recorded; an uploaded version is never replaced, so a change \
                 is a new version",
                metadata.name, metadata.version
            ));
        }
        tracing::info!(
            plugin = metadata.name,
            version = metadata.version,
            by = version.uploaded_by,
            "a plugin version recorded"
        );
        Ok(version)
    }

    fn launch(
        &self,
        request: LaunchPluginRequest,
        envelope: &Envelope,
    ) -> Result<PluginLaunch, String> {
        let snapshot = self.snapshot()?;
        let version = launch(&snapshot, &request)?;
        let metadata = version.metadata.clone().expect("found by it");
        let launched = PluginLaunch {
            instance_id: request.instance_id.clone(),
            name: metadata.name.clone(),
            version: metadata.version.clone(),
            image_digest: version.image_digest.clone(),
            roles: metadata.roles.clone(),
            tags: metadata.tags.clone(),
            launched_by: subject(envelope),
            launched_at_ns: self.clock.now_ns(),
            state: PluginLaunchState::Launched as i32,
            ..Default::default()
        };
        // Recorded before the launcher is asked, so what runs is never
        // something the conductor has no record of.
        if !self
            .store
            .begin_launch(&launched)
            .map_err(|f| f.to_string())?
        {
            return Err(format!(
                "{} is already launched; stop it first",
                request.instance_id
            ));
        }
        let create = CreatePluginRequest {
            instance_id: request.instance_id.clone(),
            image: format!(
                "{}/plugins/{}@{}",
                self.registry, metadata.name, version.image_digest
            ),
            roles: metadata.roles.clone(),
            tags: metadata.tags.clone(),
            interface: metadata.interface,
        };
        match self.ask_launcher::<_, CreatePluginReply>(
            CREATE_PLUGIN,
            "meridian.v1.CreatePluginRequest",
            create,
        ) {
            Ok(made) => {
                tracing::info!(
                    instance = launched.instance_id,
                    plugin = launched.name,
                    version = launched.version,
                    workload = made.workload,
                    by = launched.launched_by,
                    "a plugin launched"
                );
                Ok(launched)
            }
            Err(failed) => {
                // Recorded as failed, which frees the instance to try again.
                let ending = Ending {
                    state: PluginLaunchState::Failed,
                    by: String::new(),
                    at_ns: self.clock.now_ns(),
                    failure: failed.clone(),
                };
                self.store
                    .end_launch(&request.instance_id, &ending)
                    .map_err(|f| f.to_string())?;
                Err(format!(
                    "the launcher did not create {}: {failed}",
                    request.instance_id
                ))
            }
        }
    }

    fn stop(
        &self,
        request: StopPluginRequest,
        envelope: &Envelope,
    ) -> Result<PluginLaunch, String> {
        let live = self.snapshot()?.catalogue.launches.iter().any(|held| {
            held.instance_id == request.instance_id
                && held.state == PluginLaunchState::Launched as i32
        });
        if !live {
            return Err(format!("no launch of {} is live", request.instance_id));
        }
        // Removed first, and recorded as stopped once it is: a launcher that
        // could not be reached leaves the launch live, and stopping can be
        // asked again, rather than recorded stopped while it runs.
        let removed: RemovePluginReply = self.ask_launcher(
            REMOVE_PLUGIN,
            "meridian.v1.RemovePluginRequest",
            RemovePluginRequest {
                instance_id: request.instance_id.clone(),
            },
        )?;
        let ending = Ending {
            state: PluginLaunchState::Stopped,
            by: subject(envelope),
            at_ns: self.clock.now_ns(),
            failure: String::new(),
        };
        let stopped = self
            .store
            .end_launch(&request.instance_id, &ending)
            .map_err(|f| f.to_string())?
            .ok_or_else(|| format!("no launch of {} is live", request.instance_id))?;
        tracing::info!(
            instance = stopped.instance_id,
            removed = removed.removed,
            by = stopped.stopped_by,
            "a plugin stopped"
        );
        Ok(stopped)
    }
}

/// Serve W8's commands and query. `registry` is where a node reaches the
/// deployment's own registry, `localhost:<port>`.
pub fn serve_plugins(
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    registry: String,
) {
    let plugins = Arc::new(Plugins {
        bus: Arc::clone(&bus),
        store,
        clock,
        registry,
    });
    answer_on(
        &bus,
        &plugins,
        RECORD_PLUGIN_UPLOAD,
        (
            "meridian.v1.RecordPluginUploadRequest",
            "meridian.v1.PluginVersion",
        ),
        |plugins, request: RecordPluginUploadRequest, envelope| {
            plugins.record_upload(request, envelope)
        },
    );
    answer_on(
        &bus,
        &plugins,
        PLUGIN_CATALOGUE,
        (
            "meridian.v1.PluginCatalogueRequest",
            "meridian.v1.PluginCatalogue",
        ),
        |plugins, _: PluginCatalogueRequest, _| -> Result<PluginCatalogue, String> {
            Ok(plugins.snapshot()?.catalogue)
        },
    );
    answer_on(
        &bus,
        &plugins,
        LAUNCH_PLUGIN,
        (
            "meridian.v1.LaunchPluginRequest",
            "meridian.v1.PluginLaunch",
        ),
        |plugins, request: LaunchPluginRequest, envelope| plugins.launch(request, envelope),
    );
    answer_on(
        &bus,
        &plugins,
        STOP_PLUGIN,
        ("meridian.v1.StopPluginRequest", "meridian.v1.PluginLaunch"),
        |plugins, request: StopPluginRequest, envelope| plugins.stop(request, envelope),
    );
}

#[cfg(test)]
mod tests;
