//! The launcher, as its own process (decisions/019).
//!
//! Answers the conductor, and only the conductor -- the broker admits nobody
//! else to its two topics: create a plugin from the chart's template, from
//! the deployment's own registry by digest; remove one it made. Its account
//! may touch Deployments in its namespace and nothing else, and it narrows
//! itself further: it makes nothing for an instance that has a workload
//! already, and removes only what carries its label. For an edge plugin it
//! also makes the instance's storage claim, once, and never removes one
//! (decisions/028). For an instance of several pods it makes the instance's
//! own account, `<fullname>-shards-<instance>`, with a Role naming its
//! shards' Leases alone and their binding, and the Leases themselves, which
//! it never deletes (step 3d, checkpoint 5, item 31).

use std::sync::Arc;

use meridian_domain::v1::{
    CreatePluginReply, CreatePluginRequest, EntitlementsChangedEvent, RemovePluginReply,
    RemovePluginRequest,
};
use meridian_first_run::cluster::ApiServer;
use meridian_runtime::launched::{DATASETS_ANNOTATION, INSTANCE_LABEL, LAUNCHED_SELECTOR};
use meridian_runtime::launcher::{
    archive_account, checked, claim, identity_of, left_of, make_shards_account, manifest,
    remove_shards_account_once_ended, reusable, several, shards_account_of, shards_objects,
    shards_template, stop_launched, template_for, with_archive, with_limits, with_shards, Kind,
    Objects, Removal, Shapes, StorageShapes, Workloads, CREATE_PLUGIN, REMOVE_PLUGIN,
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

/// The API server, as far as an instance's own account and its Leases
/// need it (item 31).
struct Kube<'a>(&'a ApiServer);

impl Objects for Kube<'_> {
    async fn get(&self, kind: Kind, name: &str) -> Result<Option<serde_json::Value>, String> {
        let (root, plural) = kind.root_and_plural();
        self.0
            .object(root, plural, name)
            .await
            .map_err(|failed| failed.0)
    }
    async fn create(&self, kind: Kind, object: &serde_json::Value) -> Result<bool, String> {
        let (root, plural) = kind.root_and_plural();
        self.0
            .make_object(root, plural, object)
            .await
            .map_err(|failed| failed.0)
    }
    async fn replace(&self, kind: Kind, object: &serde_json::Value) -> Result<(), String> {
        let (root, plural) = kind.root_and_plural();
        self.0
            .replace_object(root, plural, object)
            .await
            .map_err(|failed| failed.0)
    }
    async fn delete(&self, kind: Kind, name: &str) -> Result<bool, String> {
        let (root, plural) = kind.root_and_plural();
        self.0
            .delete_object(root, plural, name)
            .await
            .map_err(|failed| failed.0)
    }
}

/// How often a stopped instance of several pods is looked at for its pods
/// having ended, and for how long, before its own account is removed
/// (review c5b L5): well past a pod's grace period to stop.
const ENDED_EVERY: std::time::Duration = std::time::Duration::from_secs(2);
const ENDED_WITHIN: std::time::Duration = std::time::Duration::from_secs(600);

impl Workloads for Kube<'_> {
    async fn deployments(&self, selector: &str) -> Result<serde_json::Value, String> {
        self.0
            .deployments(selector)
            .await
            .map_err(|failed| failed.0)
    }
    async fn pods(&self, selector: &str) -> Result<serde_json::Value, String> {
        self.0
            .pods_labelled(selector)
            .await
            .map_err(|failed| failed.0)
    }
    async fn delete_deployment(&self, name: &str) -> Result<bool, String> {
        self.0
            .delete_deployment(name)
            .await
            .map_err(|failed| failed.0)
    }
}

struct Launcher {
    api: ApiServer,
    /// The chart's shapes: the plugin's, the live one on a development
    /// deployment alone, and an edge plugin's with its storage.
    shapes: Shapes,
    development: bool,
    registry: String,
    /// Held by a launch while it makes an instance's account and its
    /// Deployment, and by a stopped instance's account's removal while it
    /// looks and removes: a launch between never loses what it made.
    launching: tokio::sync::Mutex<()>,
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
        // On a handler's blocking thread, as every ask of the cluster here.
        let _launching = self.launching.blocking_lock();
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
        // Several pods of the instance, where its launch asked shards or a
        // standby pod (step 3d): the chart's shape for them.
        let chosen = shards_template(chosen, &request, &self.shapes)?;
        // Its archive beside its storage, where a deployment admin allowed
        // it one (W8.7, contract v16).
        let edge = self
            .shapes
            .storage
            .as_ref()
            .map(|storage| storage.edge_roles.clone())
            .unwrap_or_default();
        // Its own share at its sidecar, over the chart's (contract v30).
        let made = with_limits(manifest(chosen.workload, &request)?, &request)?;
        // A pod per shard and one more for a standby, each sidecar told the
        // count, under the instance's own account (item 31).
        let made = with_shards(made, &request)?;
        let made = with_archive(made, self.shapes.archive.as_deref(), &request, &edge)?;
        if several(&request) {
            self.shards_account(&made, &request)?;
        }
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
            shards = request.shards,
            standby = request.standby,
            "a plugin created"
        );
        Ok(CreatePluginReply { workload })
    }

    /// An instance of several pods' own account, Role and binding, and its
    /// shards' Leases, there before its Deployment is (item 31); an allowed
    /// bucket archive's workload identity moved onto the account.
    fn shards_account(
        &self,
        made: &serde_json::Value,
        request: &CreatePluginRequest,
    ) -> Result<(), String> {
        let handle = tokio::runtime::Handle::current();
        let identity = match archive_account(self.shapes.archive.as_deref(), request) {
            None => serde_json::Map::new(),
            Some(account) => {
                let (root, plural) = Kind::ServiceAccount.root_and_plural();
                let held = handle
                    .block_on(self.api.object(root, plural, &account))
                    .map_err(|failed| failed.0)?
                    .ok_or_else(|| {
                        format!(
                            "{} was allowed an archive, and its account {account} \
                             (pluginArchive.serviceAccount) is not there",
                            request.instance_id
                        )
                    })?;
                identity_of(&held)
            }
        };
        let objects = shards_objects(made, request, self.api.namespace(), &identity)?;
        handle.block_on(make_shards_account(&Kube(&self.api), &objects))?;
        tracing::info!(
            instance = request.instance_id,
            account = objects.account["metadata"]["name"]
                .as_str()
                .unwrap_or_default(),
            leases = objects.leases.len(),
            identity = !identity.is_empty(),
            "its own account and its shards' Leases, made"
        );
        Ok(())
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

    /// Each launched plugin's entitled datasets written on its Deployment,
    /// where they differ, for the broker's reconcile to grant their subjects
    /// (contract v18, W10.1; approved 2026-10-10). Nothing restarts: the
    /// annotation is the Deployment's, not its pods'.
    async fn entitle(&self, event: &EntitlementsChangedEvent) -> Result<(), String> {
        let list = self
            .api
            .deployments(LAUNCHED_SELECTOR)
            .await
            .map_err(|failed| failed.0)?;
        for item in list["items"].as_array().into_iter().flatten() {
            let metadata = &item["metadata"];
            let (Some(name), Some(instance)) = (
                metadata["name"].as_str(),
                metadata["labels"][INSTANCE_LABEL].as_str(),
            ) else {
                continue;
            };
            let mut entitled: Vec<&str> = event
                .entitlements
                .iter()
                .filter(|e| e.allowed && e.instance == instance)
                .filter(|e| event.datasets.iter().any(|d| d.dataset == e.dataset))
                .map(|e| e.dataset.as_str())
                .collect();
            entitled.sort();
            entitled.dedup();
            let wanted = entitled.join(",");
            let held = metadata["annotations"][DATASETS_ANNOTATION]
                .as_str()
                .unwrap_or_default();
            if held == wanted {
                continue;
            }
            self.api
                .annotate_deployment(name, DATASETS_ANNOTATION, &wanted)
                .await
                .map_err(|failed| failed.0)?;
            tracing::info!(
                instance,
                datasets = wanted,
                "a plugin's entitled datasets written"
            );
        }
        Ok(())
    }

    fn remove(self: &Arc<Self>, request: RemovePluginRequest) -> Result<RemovePluginReply, String> {
        let handle = tokio::runtime::Handle::current();
        // An instance of several pods has its own account (item 31).
        let account = match shards_account_of(self.shapes.shards.as_deref(), &request.instance_id) {
            Some(account) => handle
                .block_on(Kube(&self.api).get(Kind::ServiceAccount, &account))?
                .map(|_| account),
            None => None,
        };
        // Gone at once, so a launch straight after -- a restart with an
        // archive, a move to another version -- finds none (review c5b2 H1).
        let gone = handle.block_on(stop_launched(&Kube(&self.api), &request.instance_id))?;
        for workload in &gone {
            tracing::info!(instance = request.instance_id, workload, "a plugin removed");
        }
        let removed = !gone.is_empty();
        // Its own account, Role and binding, once its pods have ended --
        // their sidecars release their shards' Leases as they stop, with its
        // token (review c5b L5) -- unless a launch of it is made first, whose
        // account it is then; never its Leases (item 31). The stop is
        // answered meanwhile.
        if let Some(account) = account {
            let launcher = Arc::clone(self);
            let instance = request.instance_id;
            handle.spawn(async move {
                let kube = Kube(&launcher.api);
                let removing = remove_shards_account_once_ended(
                    &kube,
                    &account,
                    &launcher.launching,
                    || left_of(&kube, &instance),
                    ENDED_EVERY,
                    ENDED_WITHIN,
                );
                match removing.await {
                    Ok(Removal::Removed(gone)) => tracing::info!(
                        instance,
                        account,
                        removed = ?gone,
                        "its pods ended; its own account removed, its Leases kept"
                    ),
                    Ok(Removal::Relaunched) => tracing::info!(
                        instance,
                        account,
                        "launched again; its own account is the new launch's"
                    ),
                    Ok(Removal::Stopping) => tracing::warn!(
                        instance,
                        account,
                        "its pods had not ended in {ENDED_WITHIN:?}; its own account is kept, \
                         for a later launch of it to write over"
                    ),
                    Err(failed) => tracing::warn!(
                        instance,
                        account,
                        %failed,
                        "its own account could not be removed; it is kept, for a later launch of \
                         it to write over"
                    ),
                }
            });
        }
        Ok(RemovePluginReply { removed })
    }
}

fn run() -> Result<(), String> {
    // Every path this member needs, probed from inside its pod: what
    // `meridian status` and `doctor` read (step 3b).
    if std::env::args().nth(1).as_deref() == Some("probe") {
        return meridian_runtime::probe::run(&meridian_runtime::probe::Paths {
            component: "launcher",
            domain: None,
            database: None,
            record: None,
        });
    }
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
    // One launcher launches (step 3b): a standby holds no lease and does
    // nothing until it takes it. With no lease, the one launcher launches.
    let member = meridian_runtime::member::Member::from_env("launcher", None)?;
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
            // The plugin shape as several pods of one instance (step 3d).
            shards: optional("MERIDIAN_LAUNCHER_SHARDS_TEMPLATE")?,
        },
        development,
        registry,
        launching: tokio::sync::Mutex::new(()),
    });

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|failed| failed.to_string())?
        .block_on(async {
            let bus = bus_from_env(&instance_id).await?;
            let holding = member.wait_to_write(&bus).await?;
            // The launcher keeps no store, so no epoch: its lease alone says
            // which member launches, and a member that loses it stops.
            holding.keep(|| 0, || Ok(0));
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
            // The data configuration (contract v18, W10.1): each launched
            // plugin's entitled datasets written for the broker.
            let mut configuration =
                bus.subscribe_every("platform.config.event.entitlements-changed");
            let entitling = Arc::clone(&launcher);
            tokio::spawn(async move {
                while let Some(heard) = configuration.recv().await {
                    let Ok(event) = EntitlementsChangedEvent::decode(&heard.envelope.payload[..])
                    else {
                        tracing::warn!("a data configuration did not read");
                        continue;
                    };
                    if let Err(failed) = entitling.entitle(&event).await {
                        tracing::warn!(%failed, "the entitled datasets were not written");
                    }
                }
            });
            tracing::info!(instance_id, "the launcher is serving");
            ready.serving_on(&bus).await;
            shutdown().await;
            tracing::info!("stopping");
            holding.release().await;
            Ok(())
        })
}
