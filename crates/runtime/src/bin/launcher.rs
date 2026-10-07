//! The launcher, as its own process (decisions/019).
//!
//! Answers the conductor, and only the conductor -- the broker admits nobody
//! else to its two topics: create a plugin from the chart's template, from
//! the deployment's own registry by digest; remove one it made. Its account
//! may touch Deployments in its namespace and nothing else, and it narrows
//! itself further: it makes nothing for an instance that has a workload
//! already, and removes only what carries its label. For an edge plugin it
//! also makes the instance's storage claim, once, and never removes one
//! (decisions/028).

use std::sync::Arc;

use meridian_domain::v1::{
    CreatePluginReply, CreatePluginRequest, RemovePluginReply, RemovePluginRequest,
};
use meridian_first_run::cluster::ApiServer;
use meridian_runtime::launched::{INSTANCE_LABEL, LAUNCHED_SELECTOR};
use meridian_runtime::launcher::{
    checked, claim, manifest, reusable, template_for, with_archive, Shapes, StorageShapes,
    CREATE_PLUGIN, REMOVE_PLUGIN,
};
use meridian_runtime::{bus_from_env, required, shutdown, var, Ready};
use prost::Message;

fn main() {
    meridian_runtime::answer_version();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    if let Err(failed) = run() {
        tracing::error!("{failed}");
        std::process::exit(1);
    }
}

struct Launcher {
    api: ApiServer,
    /// The chart's shapes: the plugin's, the live one on a development
    /// deployment alone, and an edge plugin's with its storage.
    shapes: Shapes,
    development: bool,
    registry: String,
}

impl Launcher {
    fn existing(&self, instance: &str, selector: &str) -> Result<Vec<String>, String> {
        let list = tokio::runtime::Handle::current()
            .block_on(
                self.api
                    .deployments(&format!("{selector}{INSTANCE_LABEL}={instance}")),
            )
            .map_err(|failed| failed.0)?;
        Ok(list["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| item["metadata"]["name"].as_str().map(String::from))
            .collect())
    }

    fn create(&self, request: CreatePluginRequest) -> Result<CreatePluginReply, String> {
        checked(&request, &self.registry)?;
        // Any workload for the instance, the chart's own included: two would
        // share a name, a hostname and a credential.
        if let Some(there) = self.existing(&request.instance_id, "")?.first() {
            return Err(format!(
                "a workload for {} already exists ({there}); the launcher never replaces what it \
                 did not just make",
                request.instance_id
            ));
        }
        let chosen = template_for(&request, self.development, &self.shapes)?;
        // Its archive beside its storage, where a deployment admin allowed
        // it one (W8.7, contract v16).
        let edge = self
            .shapes
            .storage
            .as_ref()
            .map(|storage| storage.edge_roles.clone())
            .unwrap_or_default();
        let made = with_archive(
            manifest(chosen.workload, &request)?,
            self.shapes.archive.as_deref(),
            &request,
            &edge,
        )?;
        if let Some(template) = chosen.claim {
            self.storage(template, &request)?;
        }
        let workload = tokio::runtime::Handle::current()
            .block_on(self.api.create_deployment(&made))
            .map_err(|failed| failed.0)?;
        tracing::info!(
            instance = request.instance_id,
            workload,
            image = request.image,
            live = request.live,
            archive = meridian_runtime::launcher::archive_allowed(&request),
            "a plugin created"
        );
        Ok(CreatePluginReply { workload })
    }

    /// The instance's storage, there before its Deployment is: the claim it
    /// kept, when the same plugin launched as it before, or a new one. Never
    /// another plugin's (decisions/028).
    fn storage(&self, template: &str, request: &CreatePluginRequest) -> Result<(), String> {
        let wanted = claim(template, request)?;
        let name = wanted["metadata"]["name"].as_str().unwrap_or_default();
        let handle = tokio::runtime::Handle::current();
        match handle
            .block_on(self.api.persistent_volume_claim(name))
            .map_err(|failed| failed.0)?
        {
            Some(existing) => {
                reusable(&existing, request)?;
                tracing::info!(
                    instance = request.instance_id,
                    claim = name,
                    "its storage, kept"
                );
            }
            None => {
                let made = handle
                    .block_on(self.api.create_persistent_volume_claim(&wanted))
                    .map_err(|failed| failed.0)?;
                tracing::info!(
                    instance = request.instance_id,
                    claim = made,
                    "its storage, made"
                );
            }
        }
        Ok(())
    }

    fn remove(&self, request: RemovePluginRequest) -> Result<RemovePluginReply, String> {
        let mine = self.existing(&request.instance_id, &format!("{LAUNCHED_SELECTOR},"))?;
        let mut removed = false;
        for workload in mine {
            removed |= tokio::runtime::Handle::current()
                .block_on(self.api.delete_deployment(&workload))
                .map_err(|failed| failed.0)?;
            tracing::info!(instance = request.instance_id, workload, "a plugin removed");
        }
        Ok(RemovePluginReply { removed })
    }
}

fn run() -> Result<(), String> {
    // Not ready until it serves, whatever this pod said before.
    let ready = Ready::from_env();

    let read = |path: String| {
        std::fs::read_to_string(&path)
            .map_err(|failed| format!("{path} could not be read: {failed}"))
    };
    let optional = |name: &str| var(name).map(read).transpose();
    let plain = read(required("MERIDIAN_LAUNCHER_TEMPLATE")?)?;
    let registry = required("MERIDIAN_REGISTRY_ADDRESS")?;
    // A deployment installed for development, and the chart's live shape,
    // which it renders there alone (spec/live-plugin-development).
    let development = var("MERIDIAN_DEVELOPMENT").as_deref() == Some("true");
    let live = optional("MERIDIAN_LAUNCHER_LIVE_TEMPLATE")?;
    // An edge plugin's shapes and its claim, where the chart gives edge
    // plugins storage (decisions/028); the edge roles are the chart's.
    let storage = match optional("MERIDIAN_LAUNCHER_CLAIM_TEMPLATE")? {
        Some(claim) => Some(StorageShapes {
            plain: read(required("MERIDIAN_LAUNCHER_STORAGE_TEMPLATE")?)?,
            live: optional("MERIDIAN_LAUNCHER_LIVE_STORAGE_TEMPLATE")?,
            claim,
            edge_roles: required("MERIDIAN_LAUNCHER_EDGE_ROLES")?
                .split(',')
                .map(str::trim)
                .filter(|role| !role.is_empty())
                .map(String::from)
                .collect(),
        }),
        None => None,
    };
    let instance_id = var("MERIDIAN_INSTANCE_ID").unwrap_or_else(|| "launcher-1".into());
    let api = ApiServer::in_cluster(meridian_runtime::clock()).map_err(|failed| failed.0)?;
    let launcher = Arc::new(Launcher {
        api,
        shapes: Shapes {
            plain,
            live,
            storage,
            // What an allowed instance is given beside its storage, where
            // the chart keeps archives (pluginArchive, contract v16).
            archive: optional("MERIDIAN_LAUNCHER_ARCHIVE_TEMPLATE")?,
        },
        development,
        registry,
    });

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|failed| failed.to_string())?
        .block_on(async {
            // Not, or no longer, a development deployment: nothing it made live
            // stays (spec/live-plugin-development, ruling 2). The launcher
            // restarts when the chart changes, so this is the next moment
            // after `development` is turned off. The catalogue keeps each
            // launch until an administrator stops it, which then succeeds with
            // nothing to remove.
            if !launcher.development {
                let live = launcher
                    .api
                    .deployments(&format!("{LAUNCHED_SELECTOR},meridian.dev/live=true"))
                    .await
                    .map_err(|failed| failed.0)?;
                for workload in live["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|item| item["metadata"]["name"].as_str())
                {
                    launcher
                        .api
                        .delete_deployment(workload)
                        .await
                        .map_err(|failed| failed.0)?;
                    tracing::warn!(
                        workload,
                        "a live plugin removed: this deployment is not for development"
                    );
                }
            }
            let bus = bus_from_env(&instance_id).await?;
            let creating = Arc::clone(&launcher);
            bus.serve(CREATE_PLUGIN, move |envelope| {
                let request = CreatePluginRequest::decode(&envelope.payload[..])
                    .map_err(|failed| format!("undecodable CreatePluginRequest: {failed}"))?;
                let made = creating.create(request)?;
                Ok(("meridian.v1.CreatePluginReply".into(), made.encode_to_vec()))
            });
            let removing = Arc::clone(&launcher);
            bus.serve(REMOVE_PLUGIN, move |envelope| {
                let request = RemovePluginRequest::decode(&envelope.payload[..])
                    .map_err(|failed| format!("undecodable RemovePluginRequest: {failed}"))?;
                let gone = removing.remove(request)?;
                Ok(("meridian.v1.RemovePluginReply".into(), gone.encode_to_vec()))
            });
            tracing::info!(instance_id, "the launcher is serving");
            ready.serving();
            shutdown().await;
            tracing::info!("stopping");
            Ok(())
        })
}
