//! The launcher, as its own process (decisions/019).
//!
//! Answers the conductor, and only the conductor -- the broker admits nobody
//! else to its two topics: create a plugin from the chart's template, from
//! the deployment's own registry by digest; remove one it made. Its account
//! may touch Deployments in its namespace and nothing else, and it narrows
//! itself further: it makes nothing for an instance that has a workload
//! already, and removes only what carries its label.

use std::sync::Arc;

use meridian_domain::v1::{
    CreatePluginReply, CreatePluginRequest, RemovePluginReply, RemovePluginRequest,
};
use meridian_first_run::cluster::ApiServer;
use meridian_runtime::launched::{INSTANCE_LABEL, LAUNCHED_SELECTOR};
use meridian_runtime::launcher::{checked, manifest, template_for, CREATE_PLUGIN, REMOVE_PLUGIN};
use meridian_runtime::{bus_from_env, required, shutdown, var};
use prost::Message;

fn main() {
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
    template: String,
    /// The chart's live shape, rendered only on a development deployment.
    live_template: Option<String>,
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
        let template = template_for(
            &request,
            self.development,
            &self.template,
            self.live_template.as_deref(),
        )?;
        let made = manifest(template, &request)?;
        let workload = tokio::runtime::Handle::current()
            .block_on(self.api.create_deployment(&made))
            .map_err(|failed| failed.0)?;
        tracing::info!(
            instance = request.instance_id,
            workload,
            image = request.image,
            live = request.live,
            "a plugin created"
        );
        Ok(CreatePluginReply { workload })
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
    let template_path = required("MERIDIAN_LAUNCHER_TEMPLATE")?;
    let template = std::fs::read_to_string(&template_path)
        .map_err(|failed| format!("{template_path} could not be read: {failed}"))?;
    let registry = required("MERIDIAN_REGISTRY_ADDRESS")?;
    // A deployment installed for development, and the chart's live shape,
    // which it renders there alone (spec/live-plugin-development).
    let development = var("MERIDIAN_DEVELOPMENT").as_deref() == Some("true");
    let live_template = match var("MERIDIAN_LAUNCHER_LIVE_TEMPLATE") {
        Some(path) => Some(
            std::fs::read_to_string(&path)
                .map_err(|failed| format!("{path} could not be read: {failed}"))?,
        ),
        None => None,
    };
    let instance_id = var("MERIDIAN_INSTANCE_ID").unwrap_or_else(|| "launcher-1".into());
    let api = ApiServer::in_cluster().map_err(|failed| failed.0)?;
    let launcher = Arc::new(Launcher {
        api,
        template,
        live_template,
        development,
        registry,
    });

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|failed| failed.to_string())?
        .block_on(async {
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
            shutdown().await;
            tracing::info!("stopping");
            Ok(())
        })
}
