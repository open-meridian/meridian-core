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

use meridian_runtime::{
    bus_from_env, bus_from_env_writing_ahead, names_from, required, shutdown, var,
};
use std::sync::Arc;

use meridian_sidecar::front_door::{self, FrontDoor, Verifier};
use meridian_sidecar::share::{Limits, Share, Shared};
use meridian_sidecar::{
    Identity, OlderPagesRead, PluginOperationsServer, Sidecar, SidecarServiceServer, DEFAULT_BIND,
};

/// Where the chart mounts the dashboard's public keys, one file per key id.
const DASHBOARD_KEYS: &str = "/etc/meridian/dashboard-keys";

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

/// The SDK's declaration JSON in `path`, read as an upload's is.
fn declaration_from(path: &str) -> Result<meridian_pb::v1::PluginDeclaration, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|failed| format!("MERIDIAN_PLUGIN_DECLARATION {path}: {failed}"))?;
    let declared: meridian_domain::declaration::Declared =
        serde_json::from_str(&text).map_err(|failed| {
            format!("MERIDIAN_PLUGIN_DECLARATION {path} is not a declaration: {failed}")
        })?;
    meridian_domain::declaration::from_json(&declared)
        .map_err(|why| format!("MERIDIAN_PLUGIN_DECLARATION {path}: {why}"))
}

fn run() -> Result<(), String> {
    let deployment_id = required("MERIDIAN_DEPLOYMENT_ID")?;

    // Roles, never a grant table: what they may do is the contract's, compiled
    // into this binary (decisions/020).
    let identity = Identity::new(
        var("MERIDIAN_PLUGIN_INSTANCE_ID").unwrap_or_default(),
        names_from(var("MERIDIAN_PLUGIN_ROLES")),
    );

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
            // Over TLS where the deployment's internal TLS is on (ruling
            // 1084): the chart's certificate for the sidecars' names, and
            // nothing in the clear.
            let tls = meridian_changes::tls::server_config()?
                .map(|config| tokio_rustls::TlsAcceptor::from(Arc::new(config)));
            Some((at, verifier, tls))
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

    // Whether its plugin records the lake's rows: the data role's (W10.4).
    let writes_rows = identity.roles.iter().any(|role| role == "data");

    // The plugin's share, refused at start where the chart or the launch
    // set it to something that is not one.
    let limits = Limits::from_env()?;

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|failed| failed.to_string())?
        .block_on(async {
            // Its own budget (stage M), inside the desktop footprint's +15 %
            // (1058): the plugin's share keeps a flood off it. The plugin's
            // answers in flight (answerMiB) and its read_after session are
            // held apart, so what is left is the sidecar's own work's,
            // whatever the plugin does; a share that leaves it nothing is
            // refused at start, as the chart refuses it.
            meridian_budget::install(
                "sidecar",
                limits.held_apart(),
                meridian_budget::permits_from_env(),
            )?;
            // A data plugin's lake writes go by the lake's write-ahead stream
            // in `ha` (the resilience spec's section 4): held by the stream
            // before the lake records them, so none it was answered for is
            // lost.
            let bus = if writes_rows {
                bus_from_env_writing_ahead(&instance_id).await?
            } else {
                bus_from_env(&instance_id).await?
            };
            // Its own hops' ages -- a record batch accepted, a row delivered
            // -- to the monitor, under its plugin's instance (W4.16; 606).
            tokio::spawn(meridian_runtime::report_latency_forever(bus.clone()));
            let mut sidecar = Sidecar::new(bus, &deployment_id, identity);
            if let Some(verifier) = verifier {
                sidecar = sidecar.with_verifier(verifier);
            }
            // A harness's sidecar, which no launch made, holds its plugin's
            // registration to the declaration an upload would carry (W4.1,
            // contract v25): the SDK's JSON, as `meridian-declaration`
            // prints it. The launcher never sets it.
            if let Some(path) = var("MERIDIAN_PLUGIN_DECLARATION") {
                sidecar = sidecar.held_to(declaration_from(&path)?);
                tracing::info!(path, "registration held to the declaration in this file");
            }
            let sidecar = Arc::new(sidecar);

            // A live instance of a development deployment: the development
            // endpoint behind the front door (spec/live-plugin-development).
            // Both told, or none.
            if let Some(live) = meridian_sidecar::live::Live::from_env(
                var("MERIDIAN_LIVE_DIR"),
                var("MERIDIAN_DEVELOPMENT").as_deref() == Some("true"),
                sidecar.clock(),
            ) {
                sidecar.go_live(Arc::new(live));
                tracing::info!("live: the development endpoint is on");
            }

            tracing::info!(instance_id, %listening, "the sidecar is serving");

            // What it knows of its plugin, for the conductor and the
            // dashboard (W4.8): now, on each registration, and every 30s.
            tokio::spawn(meridian_sidecar::report::report_forever(Arc::clone(&sidecar)));

            let door = match front {
                None => None,
                Some((at, verifier, tls)) => {
                    let door = FrontDoor::new(Arc::clone(&sidecar), verifier)?;
                    let listener = tokio::net::TcpListener::bind(at)
                        .await
                        .map_err(|failed| format!("the front door could not bind {at}: {failed}"))?;
                    tracing::info!(%at, tls = tls.is_some(), "the front door is open to the dashboard");
                    Some(serve_door(listener, front_door::router(door), tls))
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

            // The plugin's own share (stage M; the resilience spec's section
            // 10a): past it, only this plugin is refused.
            tracing::info!(
                in_flight = limits.in_flight,
                requests_per_second = limits.requests_per_second,
                answer_mib = limits.answer_mib,
                "the plugin's share at its sidecar"
            );
            let share = Share::new(limits);
            let serving = tonic::transport::Server::builder()
                // An older plugin's admin pages, read as pages at `admin`
                // while its contract is accepted (W4.8).
                .add_service(Shared::new(
                    OlderPagesRead::new(SidecarServiceServer::from_arc(Arc::clone(&sidecar))),
                    Arc::clone(&share),
                ))
                // The typed operations (spec/typed-sidecar-operations).
                .add_service(Shared::new(PluginOperationsServer::from_arc(sidecar), share))
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

/// The front door served: over TLS where the deployment's internal TLS is
/// on, each connection's handshake its own task so one slow client holds up
/// no other; in the clear, as axum serves it, otherwise.
fn serve_door(
    listener: tokio::net::TcpListener,
    router: axum::Router,
    tls: Option<tokio_rustls::TlsAcceptor>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send>> {
    let Some(tls) = tls else {
        return Box::pin(async move { axum::serve(listener, router).await });
    };
    Box::pin(async move {
        loop {
            let (stream, _) = listener.accept().await?;
            let tls = tls.clone();
            let service = hyper_util::service::TowerToHyperService::new(router.clone());
            tokio::spawn(async move {
                // A client that does not speak TLS -- plaintext, the
                // refusal internal TLS promises -- ends here.
                let Ok(stream) = tls.accept(stream).await else {
                    return;
                };
                let _ = hyper_util::server::conn::auto::Builder::new(
                    hyper_util::rt::TokioExecutor::new(),
                )
                .serve_connection_with_upgrades(hyper_util::rt::TokioIo::new(stream), service)
                .await;
            });
        }
    })
}
