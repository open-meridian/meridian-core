//! A plugin's sidecar, as its own process.
//!
//! The one surface a plugin ever touches: gRPC on loopback, which means a
//! container in the same pod. It holds the broker credential the plugin beside
//! it has no mount for, and that filesystem seam is the boundary decisions 004
//! and 010 rest on.
//!
//! One per plugin, which is why it is a process rather than a thread inside
//! the runtime: a shared sidecar holds one registration, and a second plugin
//! admitted on it would inherit the first's grants.
//!
//! Who this serves is launch configuration and never what registers: a plugin
//! that named its own roles would be choosing its own privileges.

use meridian_runtime::{bus_from_env, names_from, required, shutdown, var};
use std::sync::Arc;

use meridian_sidecar::{
    Identity, PluginOperationsServer, Sidecar, SidecarServiceServer, DEFAULT_BIND,
};

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

fn run() -> Result<(), String> {
    let deployment_id = required("MERIDIAN_DEPLOYMENT_ID")?;

    // Roles, never a grant table: what they may do is the contract's, compiled
    // into this binary (decisions/020).
    let identity = Identity::new(
        var("MERIDIAN_PLUGIN_INSTANCE_ID").unwrap_or_default(),
        names_from(var("MERIDIAN_PLUGIN_ROLES")),
    )
    .with_tags(names_from(var("MERIDIAN_PLUGIN_TAGS")));

    let address = var("MERIDIAN_SIDECAR_ADDRESS").unwrap_or_else(|| DEFAULT_BIND.into());
    let listening: std::net::SocketAddr = address
        .parse()
        .map_err(|failed| format!("{address} is not an address: {failed}"))?;

    // An override off loopback is a deployment decision, and it is the one that
    // opens the boundary, so it is said out loud rather than inferred later
    // from a port map.
    if !listening.ip().is_loopback() {
        tracing::warn!(
            %listening,
            "the sidecar is reachable beyond loopback; anything that can route \
             here can register as a plugin"
        );
    }

    // On the bus as its plugin's instance, so what it sends names the plugin
    // and what the conductor answers it -- its configuration, its links -- is
    // that plugin's: the conductor answers for the instance the envelope
    // names. A sidecar launched with no plugin instance falls back to its
    // own name, and has no plugin to answer for.
    let instance_id = Some(identity.instance_id.clone())
        .filter(|id| !id.is_empty())
        .or_else(|| var("MERIDIAN_INSTANCE_ID"))
        .unwrap_or_else(|| "sidecar-1".into());

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|failed| failed.to_string())?
        .block_on(async {
            let bus = bus_from_env(&instance_id).await?;
            let sidecar = Arc::new(Sidecar::new(bus, &deployment_id, identity));

            tracing::info!(instance_id, %listening, "the sidecar is serving");

            let serving = tonic::transport::Server::builder()
                .add_service(SidecarServiceServer::from_arc(Arc::clone(&sidecar)))
                // The typed operations (spec/typed-sidecar-operations).
                .add_service(PluginOperationsServer::from_arc(sidecar))
                .serve(listening);

            tokio::select! {
                // A port already taken lands here, and so does a surface that
                // stopped later. Either way no plugin can reach this sidecar,
                // so it exits non-zero and an orchestrator restarts it rather
                // than leaving it up and unreachable.
                served = serving => {
                    served.map_err(|failed| format!("the sidecar surface at {listening} stopped: {failed}"))
                }
                _ = shutdown() => {
                    tracing::info!("stopping");
                    Ok(())
                }
            }
        })
}
