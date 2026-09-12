//! A deployment's runtime: the process a customer runs.
//!
//! Three things in one process, sharing one bus.
//!
//! The **replica** answers instrument questions locally and pulls from the
//! platform when it cannot. The **ledger** records holdings statements and the
//! positions behind them. The **sidecar** is the gRPC surface a plugin binds to,
//! and it is the only way into the bus from outside this process.
//!
//! # Why the sidecar is not its own container
//!
//! v1 ran one sidecar container per plugin, and that shape needs a bus a second
//! process can reach. The only backend here is in-process, so a sidecar in its
//! own container would have nothing to connect to. It lives here instead and
//! plugins dial it over gRPC from their own containers, which keeps every grant
//! decision on this side of the boundary and costs a port.
//!
//! When a network backend exists the sidecar can move out, and a plugin will
//! not notice: it already talks gRPC to an address it is told.
//!
//! # One plugin, for now
//!
//! One sidecar per plugin is the intended shape, and this is not it. A sidecar
//! holds one registration and no request after the first says who is calling,
//! because v1 paired a sidecar with its plugin physically. On a shared port
//! that pairing is gone, so a second plugin is refused rather than silently
//! replacing the first and lending it its grants.
//!
//! Restoring the intended shape means a sidecar in its own container, which
//! means a bus it can reach from outside this process:
//! `sdk-contract/sidecar-needs-a-bus-across-a-process-boundary`.
//!
//! Until then a plugin has to share this process's network namespace to reach
//! the sidecar on loopback, which means a container in the same pod.
//!
//! # Two stores, which may be one database
//!
//! The replica and the ledger take separate connection settings and default to
//! the same database. They hold different things with different retention and
//! availability needs, so the split is kept available; taking it is a
//! configuration change rather than a migration.
//!
//! Configuration is one address, this deployment's identifier, a private key
//! and a database. Nothing about the platform's shape, because the platform is
//! allowed to grow, move and be redirected without anything here changing.
//!
//! # Two modes
//!
//! `public-key` makes sure a key pair exists and prints the public half. That
//! is the first step of bringing a deployment up: the private half never leaves
//! this container, and the public half is registered on the platform once.
//!
//! With no arguments it runs the replica: open the store, create the schema,
//! answer resolutions, react to misses, until something stops it.
//!
//! # What it does when things are missing
//!
//! A missing database is fatal, because a replica with nowhere to keep what it
//! is told cannot do its job. An unreachable platform is not: the replica
//! starts, answers from what it holds, and picks the platform up when it comes
//! back. That asymmetry is the whole design in one startup path.

use std::sync::Arc;
use std::time::Duration;

use meridian_bus::{Bus, MemoryBackend};
use meridian_kernel::service::SystemClock as LedgerClock;
use meridian_reference::{
    Config, DeploymentKey, HttpTransport, Platform, PostgresStore, Replica, SystemClock,
};
use meridian_sidecar::{GrantTable, Identity, Sidecar};

/// One attempt against the platform. The retry sequence is longer by design.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Where a plugin dials its sidecar. Loopback, because meridian_sidecar says
/// so: a sidecar reachable from another host is a way around the boundary it
/// exists to enforce.
///
/// A plugin therefore has to share this network namespace, which in Kubernetes
/// means a container in the runtime's pod. That is a narrow deployment, and it
/// is the honest one until a sidecar can run beside its plugin and reach the
/// bus from there.
const SIDECAR_ADDRESS: &str = meridian_sidecar::DEFAULT_BIND;

/// What a plugin may publish and subscribe to, by role.
///
/// Supplied as a file rather than compiled in, because which roles a deployment
/// admits is the deployment's decision. Absent, the sidecar admits nothing,
/// which is the right default for a table that decides access.
const GRANTS_PATH: &str = "/etc/meridian/grants.json";

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    if let Err(failed) = run() {
        // A configuration mistake is a message, not a backtrace. Whoever sees
        // this is reading a container log and wants to know which variable.
        tracing::error!("{failed}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let key_path = var("MERIDIAN_KEY_PATH").unwrap_or_else(|| "/var/lib/meridian/key.pem".into());
    let key = key_at(&key_path)?;

    if std::env::args().nth(1).as_deref() == Some("public-key") {
        println!(
            "{}",
            key.public_key_pem().map_err(|failed| failed.to_string())?
        );
        return Ok(());
    }

    let deployment_id = required("MERIDIAN_DEPLOYMENT_ID")?;
    let address = required("MERIDIAN_PLATFORM_ADDRESS")?;
    let replica_url = required("MERIDIAN_DATABASE_URL")?;
    let instance_id = var("MERIDIAN_INSTANCE_ID").unwrap_or_else(|| "runtime-1".into());
    let sidecar_address = var("MERIDIAN_SIDECAR_ADDRESS").unwrap_or_else(|| SIDECAR_ADDRESS.into());

    // Separate setting, same database by default. The two hold different things
    // with different retention and availability needs, so pointing them at
    // different databases later is configuration rather than a migration.
    let ledger_url = var("MERIDIAN_LEDGER_DATABASE_URL").unwrap_or_else(|| replica_url.clone());

    let replica_store =
        PostgresStore::connect(&replica_url, 8).map_err(|failed| failed.to_string())?;
    replica_store
        .migrate()
        .map_err(|failed| failed.to_string())?;

    let ledger_store = meridian_kernel::PostgresStore::connect(&ledger_url, 8)
        .map_err(|failed| failed.to_string())?;
    ledger_store
        .migrate()
        .map_err(|failed| failed.to_string())?;

    let grants = grants_at(GRANTS_PATH)?;

    let transport = HttpTransport::new(REQUEST_TIMEOUT).map_err(|failed| failed.to_string())?;
    let platform = Platform::new(
        Config::new(&address, &deployment_id),
        key,
        Arc::new(transport),
    );

    let bus = Arc::new(Bus::single(&instance_id, Arc::new(MemoryBackend::new())));

    // The ledger's handlers are registered before anything can call them, and
    // before the sidecar opens a port, so a plugin that connects immediately
    // cannot find a topic that nothing serves yet.
    meridian_kernel::service::serve(bus.clone(), Arc::new(ledger_store), Arc::new(LedgerClock));

    // Who this sidecar serves, from the environment rather than from whatever
    // registers. A plugin that named its own role would be choosing its own
    // privileges. Unset means no role, which resolves to no grants, so the
    // registration is refused and says so.
    let identity = Identity::new(
        var("MERIDIAN_PLUGIN_INSTANCE_ID").unwrap_or_default(),
        var("MERIDIAN_PLUGIN_ROLE").unwrap_or_default(),
    )
    .with_tags(tags_from(var("MERIDIAN_PLUGIN_TAGS")));

    let sidecar = Sidecar::new(bus.clone(), &deployment_id, "v1", identity);
    sidecar.load_grants(grants);

    let replica = Replica::new(
        bus,
        Arc::new(replica_store),
        Arc::new(platform),
        Arc::new(SystemClock),
    );

    tracing::info!(
        deployment_id,
        address,
        instance_id,
        sidecar_address,
        "runtime starting"
    );

    // Registered and subscribed before this returns, so nothing is published
    // into the gap between starting and listening.
    let running = replica.start();

    let listening: std::net::SocketAddr = sidecar_address
        .parse()
        .map_err(|failed| format!("{sidecar_address} is not an address: {failed}"))?;

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

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|failed| failed.to_string())?
        .block_on(async {
            let serving = tonic::transport::Server::builder()
                .add_service(meridian_sidecar::SidecarServiceServer::new(sidecar))
                .serve(listening);

            tokio::select! {
                _ = running => {
                    tracing::warn!("the bus shut down");
                    Ok(())
                }
                // A port already taken lands here, and so does a surface that
                // stopped later. Either way no plugin can reach this runtime,
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
        })?;

    Ok(())
}

/// Tags from one comma-separated value, blanks dropped.
///
/// Empty and unset are the same thing here: a sidecar with no tags, which is
/// the ordinary case. v1 read this from the environment too, and a null value
/// there meant a plugin that registered and then had every publish denied, so
/// the sidecar logs what it was launched with rather than leaving an operator
/// to infer it from refusals.
fn tags_from(raw: Option<String>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(str::to_string)
        .collect()
}

/// The grant table, or an empty one.
///
/// Absent, nothing is granted and every registration is refused, which is the
/// right default for a file that decides access: a deployment that forgot to
/// mount it should admit nobody rather than everybody.
fn grants_at(path: &str) -> Result<GrantTable, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            let table = GrantTable::from_json(&raw)
                .map_err(|failed| format!("the grants at {path} could not be read: {failed}"))?;
            tracing::info!(path, roles = table.roles.len(), "loaded grants");
            Ok(table)
        }
        // Absent is a deployment that admits no plugins, which is a state an
        // operator may well intend. Present and unreadable is a mounted file
        // with the wrong permissions, and starting anyway would turn a fixable
        // mistake into a deployment where nothing registers and the logs say
        // only that nothing was granted.
        Err(failed) if failed.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                path,
                "no grant table; every plugin registration will be refused"
            );
            Ok(GrantTable::default())
        }
        Err(failed) => Err(format!("the grants at {path} could not be read: {failed}")),
    }
}

/// The key at this path, generating and writing one if there is none.
///
/// Generated here rather than handed in, because the private half has no reason
/// to exist anywhere else. The platform never sees one and has nowhere to put
/// one.
fn key_at(path: &str) -> Result<DeploymentKey, String> {
    if let Ok(pem) = std::fs::read_to_string(path) {
        return DeploymentKey::from_pkcs8_pem(&pem)
            .map_err(|failed| format!("the key at {path} could not be read: {failed}"));
    }

    let key = DeploymentKey::generate();
    let pem = key.private_key_pem().map_err(|failed| failed.to_string())?;

    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent)
            .map_err(|failed| format!("could not create {}: {failed}", parent.display()))?;
    }
    std::fs::write(path, &pem).map_err(|failed| format!("could not write {path}: {failed}"))?;
    restrict(path)?;

    tracing::info!(path, "generated a deployment key");
    Ok(key)
}

#[cfg(unix)]
fn restrict(path: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|failed| format!("could not restrict {path}: {failed}"))
}

#[cfg(not(unix))]
fn restrict(_path: &str) -> Result<(), String> {
    Ok(())
}

/// Stop on either signal, because compose sends one and a terminal sends the
/// other, and a process that only handles the terminal's gets killed instead of
/// asked.
async fn shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut term = match signal(SignalKind::terminate()) {
            Ok(term) => term,
            Err(_) => return std::future::pending().await,
        };

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }

    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

fn var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn required(name: &str) -> Result<String, String> {
    var(name).ok_or_else(|| format!("{name} is not set"))
}
