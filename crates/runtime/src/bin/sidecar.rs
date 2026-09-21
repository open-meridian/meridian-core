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
//! that named its own role would be choosing its own privileges.

use meridian_runtime::{bus_from_env, grants_at, required, shutdown, tags_from, var, GRANTS_PATH};
use meridian_sidecar::{Identity, Sidecar, DEFAULT_BIND};

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
    let grants = grants_at(&var("MERIDIAN_GRANTS_PATH").unwrap_or_else(|| GRANTS_PATH.into()))?;

    let identity = Identity::new(
        var("MERIDIAN_PLUGIN_INSTANCE_ID").unwrap_or_default(),
        var("MERIDIAN_PLUGIN_ROLE").unwrap_or_default(),
    )
    .with_tags(tags_from(var("MERIDIAN_PLUGIN_TAGS")));

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

    let instance_id = var("MERIDIAN_INSTANCE_ID").unwrap_or_else(|| "sidecar-1".into());

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|failed| failed.to_string())?
        .block_on(async {
            let bus = bus_from_env(&instance_id).await?;
            let sidecar = Sidecar::new(bus, &deployment_id, identity);
            sidecar.load_grants(grants);

            tracing::info!(instance_id, %listening, "the sidecar is serving");

            let serving = tonic::transport::Server::builder()
                .add_service(meridian_sidecar::SidecarServiceServer::new(sidecar))
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
