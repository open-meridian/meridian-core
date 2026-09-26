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

use meridian_sidecar::front_door::{self, FrontDoor, Verifier};
use meridian_sidecar::{
    Identity, PluginOperationsServer, Sidecar, SidecarServiceServer, DEFAULT_BIND,
};

/// Where the chart mounts the dashboard's public keys, one file per key id.
const DASHBOARD_KEYS: &str = "/etc/meridian/dashboard-keys";

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

    // The dashboard's public keys, one file per key id, read when an id is
    // first met. The front door verifies each page request with them, and a
    // command sent for a person (W4.9) is verified with the same ones.
    let verifier = Some(identity.instance_id.clone())
        .filter(|id| !id.is_empty())
        .map(|instance| {
            let keys = var("MERIDIAN_DASHBOARD_KEYS_DIR").unwrap_or_else(|| DASHBOARD_KEYS.into());
            Arc::new(Verifier::new(instance, keys))
        });

    // The front door (decisions/014, 021): where the dashboard sends a
    // person's requests for this plugin's page. Off loopback by necessity,
    // since the dashboard is another pod, and closed to everything else by the
    // chart's NetworkPolicy; unset, there is none, and nobody reaches the page.
    let front = match var("MERIDIAN_FRONT_DOOR_ADDRESS") {
        None => None,
        Some(address) => {
            let Some(verifier) = verifier.clone() else {
                return Err(
                    "a front door needs MERIDIAN_PLUGIN_INSTANCE_ID: it admits only \
                     assertions for this instance"
                        .into(),
                );
            };
            let at: std::net::SocketAddr = address
                .parse()
                .map_err(|failed| format!("{address} is not an address: {failed}"))?;
            Some((at, verifier))
        }
    };

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
            let mut sidecar = Sidecar::new(bus, &deployment_id, identity);
            if let Some(verifier) = verifier {
                sidecar = sidecar.with_verifier(verifier);
            }
            let sidecar = Arc::new(sidecar);

            tracing::info!(instance_id, %listening, "the sidecar is serving");

            // What it knows of its plugin, for the conductor and the
            // dashboard (W4.8): now, on each registration, and every 30s.
            tokio::spawn(meridian_sidecar::report::report_forever(Arc::clone(&sidecar)));

            let door = match front {
                None => None,
                Some((at, verifier)) => {
                    let door = FrontDoor::new(Arc::clone(&sidecar), verifier)?;
                    let listener = tokio::net::TcpListener::bind(at)
                        .await
                        .map_err(|failed| format!("the front door could not bind {at}: {failed}"))?;
                    tracing::info!(%at, "the front door is open to the dashboard");
                    Some(axum::serve(listener, front_door::router(door)))
                }
            };
            let front_door = async {
                match door {
                    Some(serving) => serving
                        .await
                        .map_err(|failed| format!("the front door stopped: {failed}")),
                    None => std::future::pending().await,
                }
            };

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
                // The page is how a person reaches the plugin, so a door that
                // stopped is a sidecar to restart, as the surface is.
                stopped = front_door => stopped,
                _ = shutdown() => {
                    tracing::info!("stopping");
                    Ok(())
                }
            }
        })
}
