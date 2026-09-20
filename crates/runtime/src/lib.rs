//! What every component of a deployment needs, and none should write twice.
//!
//! The runtime was one process holding the ledger, the replica and a sidecar.
//! Decision 010 gave them a bus that crosses a process boundary, and
//! `design/split-the-runtime-into-services` ruled that they are separate
//! processes upgraded on their own schedules. This is what they share: the
//! bus they connect to, the key they present, the grant table, and the
//! environment they read.
//!
//! Each binary is in `src/bin`, and each is small enough to read in one
//! sitting, which is the point of them being separate.

use std::sync::Arc;
use std::time::Duration;

use meridian_bus::{Backend, Bus, MemoryBackend, NatsBackend};
use meridian_reference::platform::ComponentReport;
use meridian_reference::{DeploymentKey, Platform};
use meridian_sidecar::GrantTable;

/// How long a component waits on the platform before giving up on one attempt.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How often a component says what it is running. W5.19.
pub const REPORT_EVERY: Duration = Duration::from_secs(300);

/// Where the grant table is mounted. A file rather than a setting, because it
/// decides access and a deployment should be able to read what it granted.
pub const GRANTS_PATH: &str = "/etc/meridian/grants.json";

/// The bus this component talks on.
///
/// A broker when one is configured, and in-process when none is. Both are
/// real: a developer running one binary needs no broker, and a deployment with
/// separate components cannot work without one. What decides it is a single
/// setting rather than a build flag, so the same image does both.
pub async fn bus_from_env(instance_id: &str) -> Result<Arc<Bus>, String> {
    let backend: Arc<dyn Backend> = match var("MERIDIAN_BROKER_URL") {
        Some(url) => {
            let broker = NatsBackend::connect(&url)
                .await
                .map_err(|failed| format!("the broker could not be reached: {failed}"))?;
            tracing::info!("connected to the broker");
            Arc::new(broker)
        }
        None => {
            // Said out loud. A component that cannot hear another component is
            // a deployment that half works, and the quiet version of that is
            // an afternoon with a packet capture.
            tracing::warn!(
                "no MERIDIAN_BROKER_URL: this component talks only to itself, \
                 which is a development arrangement rather than a deployment"
            );
            Arc::new(MemoryBackend::new())
        }
    };

    Ok(Arc::new(Bus::single(instance_id, backend)))
}

/// The platform client, for a component that talks to the platform.
pub fn platform_from_env(key: DeploymentKey) -> Result<Arc<Platform>, String> {
    use meridian_reference::{Config, HttpTransport};

    let address = required("MERIDIAN_PLATFORM_ADDRESS")?;
    let deployment_id = required("MERIDIAN_DEPLOYMENT_ID")?;
    let transport = HttpTransport::new(REQUEST_TIMEOUT).map_err(|failed| failed.to_string())?;

    Ok(Arc::new(Platform::new(
        Config::new(&address, &deployment_id),
        key,
        Arc::new(transport),
    )))
}

/// Tell the platform what this component is running, now and every interval.
///
/// One component per process now, where the runtime reported two. Failing to
/// report changes nothing about running, so this is spawned, never awaited,
/// and logged at debug.
pub async fn report_forever(platform: Arc<Platform>, component: &'static str, schema: i64) {
    let started_at_ns = now_ns();
    let version = var("MERIDIAN_VERSION").unwrap_or_else(|| env!("CARGO_PKG_VERSION").into());

    loop {
        let report = ComponentReport::serving(component, &version, schema, started_at_ns);
        if let Err(failed) = platform.report_components(&[report], now_ns()).await {
            tracing::debug!(%failed, "could not report components");
        }
        tokio::time::sleep(REPORT_EVERY).await;
    }
}

pub fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos() as i64)
        .unwrap_or_default()
}

/// Tags from one comma-separated value, blanks dropped.
///
/// Empty and unset are the same thing here: a sidecar with no tags, which is
/// the ordinary case. v1 read this from the environment too, and a null value
/// there meant a plugin that registered and then had every publish denied, so
/// the sidecar logs what it was launched with rather than leaving an operator
/// to infer it from refusals.
pub fn tags_from(raw: Option<String>) -> Vec<String> {
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
pub fn grants_at(path: &str) -> Result<GrantTable, String> {
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
pub fn key_at(path: &str) -> Result<DeploymentKey, String> {
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
pub async fn shutdown() {
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

pub fn var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

pub fn required(name: &str) -> Result<String, String> {
    var(name).ok_or_else(|| format!("{name} is not set"))
}
