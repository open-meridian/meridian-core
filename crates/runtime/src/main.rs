//! A deployment's runtime: the process a customer runs.
//!
//! Today it holds the replica of the security master. The kernel and the
//! dashboard join it, which is why it is not named for the one thing it does
//! first.
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
use meridian_reference::{
    Config, DeploymentKey, HttpTransport, Platform, PostgresStore, Replica, SystemClock,
};

/// One attempt against the platform. The retry sequence is longer by design.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

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
    let database_url = required("MERIDIAN_DATABASE_URL")?;
    let instance_id = var("MERIDIAN_INSTANCE_ID").unwrap_or_else(|| "reference-1".into());

    let store = PostgresStore::connect(&database_url, 8).map_err(|failed| failed.to_string())?;
    store.migrate().map_err(|failed| failed.to_string())?;

    let transport = HttpTransport::new(REQUEST_TIMEOUT).map_err(|failed| failed.to_string())?;
    let platform = Platform::new(
        Config::new(&address, &deployment_id),
        key,
        Arc::new(transport),
    );

    let bus = Arc::new(Bus::single(&instance_id, Arc::new(MemoryBackend::new())));
    let replica = Replica::new(
        bus,
        Arc::new(store),
        Arc::new(platform),
        Arc::new(SystemClock),
    );

    tracing::info!(deployment_id, address, instance_id, "replica starting");

    // Registered and subscribed before this returns, so nothing is published
    // into the gap between starting and listening.
    let running = replica.start();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|failed| failed.to_string())?
        .block_on(async {
            tokio::select! {
                _ = running => tracing::warn!("the bus shut down"),
                _ = shutdown() => tracing::info!("stopping"),
            }
        });

    Ok(())
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
