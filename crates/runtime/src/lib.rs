//! What every component of a deployment needs, and none should write twice.
//!
//! The runtime was one process holding the street store, the instrument store and a sidecar.
//! Decision 010 gave them a bus that crosses a process boundary, and
//! `design/split-the-runtime-into-services` ruled that they are separate
//! processes upgraded on their own schedules. This is what they share: the
//! bus they connect to, the key they present, the grant table, and the
//! environment they read.
//!
//! Each binary is in `src/bin`, and each is small enough to read in one
//! sitting, which is the point of them being separate.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meridian_bus::{Backend, Bus, MemoryBackend, NatsBackend};
use meridian_conductor::platform::ComponentReport;
use meridian_conductor::{DeploymentKey, Platform};
use meridian_sidecar::GrantTable;

/// How long a component waits on the platform before giving up on one attempt.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How often a component tells the platform what it is running. W5.19.
///
/// Often enough that a page is not stale after a deploy, rare enough that a
/// thousand deployments are not a load.
pub const REPORT_EVERY: Duration = Duration::from_secs(300);

/// How often a component says the same thing on the bus, for the instrument store to
/// carry. W5.20.
///
/// Much more often, because this one is a few dozen bytes inside a cluster and
/// because the instrument store learns what is running only by hearing it. At the
/// outward interval, a reporter that restarted under-reported the deployment
/// for up to five minutes, and the page said a component had gone when it had
/// not. Found by restarting one.
pub const REPORT_INWARD_EVERY: Duration = Duration::from_secs(20);

/// How long after starting a component reports again.
///
/// The first report leaves immediately, so a deployment appears as soon as it
/// is up, and carries only this component: nothing has been heard from the
/// others yet, and at-most-once delivery means what they said before this
/// process subscribed is gone. Long enough that they have said it again.
pub const REPORT_AGAIN_AFTER: Duration = Duration::from_secs(45);

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
    use meridian_conductor::{Config, HttpTransport};

    let address = required("MERIDIAN_PLATFORM_ADDRESS")?;
    let deployment_id = required("MERIDIAN_DEPLOYMENT_ID")?;
    let transport = HttpTransport::new(REQUEST_TIMEOUT).map_err(|failed| failed.to_string())?;

    Ok(Arc::new(Platform::new(
        Config::new(&address, &deployment_id),
        key,
        Arc::new(transport),
    )))
}

/// Where a component says what it is running, inside the deployment. W5.20.
pub const COMPONENT_REPORT_TOPIC: &str = "platform.deployment.event.component-report";

/// Say what this component is running, on the bus, for the instrument store to collect.
///
/// Inward rather than to the platform, because reporting outward needs the
/// deployment's key and a component holding one is a second thing able to
/// authenticate as the whole deployment. That is a large thing to grant for a
/// version string.
pub async fn report_inward_forever(bus: Arc<Bus>, component: &'static str, schema: i64) {
    use prost::Message as _;

    let started_at_ns = now_ns();
    let version = var("MERIDIAN_VERSION").unwrap_or_else(|| env!("CARGO_PKG_VERSION").into());

    loop {
        let report = meridian_domain::v1::ComponentReport {
            component: component.to_string(),
            version: version.clone(),
            schema_version: schema,
            health: meridian_domain::v1::ComponentHealth::Serving as i32,
            detail: String::new(),
            started_at_ns,
        };

        if let Err(failed) = bus.publish(
            COMPONENT_REPORT_TOPIC,
            "meridian.v1.ComponentReport",
            report.encode_to_vec(),
            None,
            None,
        ) {
            // Not a warning. A deployment whose components cannot say what
            // they run still runs them, and the platform shows an age rather
            // than inferring failure from silence.
            tracing::debug!(%failed, "could not report inward");
        }

        tokio::time::sleep(REPORT_INWARD_EVERY).await;
    }
}

/// What every other component has said about itself, newest per component.
///
/// A current picture rather than a history, for the reason the platform keeps
/// one: a history here would be a time series of a customer's estate that
/// nobody asked for.
///
/// Returned rather than kept inside the reporting loop, so what a report ends
/// up carrying can be tested without a platform to receive it.
pub fn collect_inward(bus: Arc<Bus>) -> Arc<Mutex<BTreeMap<String, ComponentReport>>> {
    use prost::Message as _;

    let heard: Arc<Mutex<BTreeMap<String, ComponentReport>>> =
        Arc::new(Mutex::new(BTreeMap::new()));
    let collecting = Arc::clone(&heard);
    let mut inward = bus.subscribe(COMPONENT_REPORT_TOPIC);

    tokio::spawn(async move {
        while let Some(delivery) = inward.recv().await {
            let Ok(report) =
                meridian_domain::v1::ComponentReport::decode(&delivery.envelope.payload[..])
            else {
                tracing::warn!("a component report did not decode");
                continue;
            };

            collecting.lock().expect("report lock poisoned").insert(
                report.component.clone(),
                ComponentReport {
                    component: report.component,
                    version: report.version,
                    schema_version: report.schema_version,
                    health: "COMPONENT_HEALTH_SERVING",
                    detail: report.detail,
                    started_at_ns: report.started_at_ns,
                },
            );
        }
    });

    heard
}

/// Tell the platform what this component is running, now and every interval.
///
/// One component per process now, where the runtime reported two. Failing to
/// report changes nothing about running, so this is spawned, never awaited,
/// and logged at debug.
pub async fn report_forever(
    platform: Arc<Platform>,
    bus: Arc<Bus>,
    component: &'static str,
    schema: i64,
) {
    let started_at_ns = now_ns();
    let version = var("MERIDIAN_VERSION").unwrap_or_else(|| env!("CARGO_PKG_VERSION").into());

    let heard = collect_inward(bus);

    // Immediately, then once the others have had a chance to say what they
    // are, then at the ordinary interval. Without the middle one the platform
    // showed a component as gone for five minutes after the instrument store restarted,
    // which is a page saying something untrue rather than something stale.
    let mut wait = REPORT_AGAIN_AFTER;

    loop {
        // This component's own, which never travels: it is the one holding the
        // key, so it has no reason to tell itself over a broker.
        let mut reports = vec![ComponentReport::serving(
            component,
            &version,
            schema,
            started_at_ns,
        )];
        reports.extend(
            heard
                .lock()
                .expect("report lock poisoned")
                .values()
                .cloned(),
        );

        if let Err(failed) = platform.report_components(&reports, now_ns()).await {
            tracing::debug!(%failed, "could not report components");
        }

        tokio::time::sleep(wait).await;
        wait = REPORT_EVERY;
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

/// The platform, as the configuration store needs it (W5.22, W5.23), over the
/// conductor's own client and key.
///
/// The configuration store calls it from a bus handler, which runs on a
/// blocking thread, so this blocks on the runtime it was made on rather than
/// making the store async for two calls.
pub struct PlatformUpstream {
    platform: Arc<Platform>,
    runtime: tokio::runtime::Handle,
}

impl PlatformUpstream {
    /// Call inside the runtime the handlers will block on.
    pub fn new(platform: Arc<Platform>) -> Self {
        Self {
            platform,
            runtime: tokio::runtime::Handle::current(),
        }
    }
}

impl meridian_config::Upstream for PlatformUpstream {
    fn honour_claim_code(
        &self,
        code: &str,
        purpose: i32,
    ) -> Result<meridian_domain::v1::RedeemClaimCodeReply, String> {
        self.runtime
            .block_on(self.platform.honour_claim_code(code, purpose, now_ns()))
            .map_err(|failed| format!("the platform could not be asked: {failed}"))
    }

    fn submit_diagnostic_bundle(
        &self,
        bundle: &meridian_domain::v1::DiagnosticBundle,
    ) -> Result<meridian_domain::v1::DiagnosticBundleReceipt, String> {
        self.runtime
            .block_on(self.platform.submit_diagnostic_bundle(bundle, now_ns()))
            .map_err(|failed| format!("the platform could not be sent the bundle: {failed}"))
    }
}

pub fn var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

pub fn required(name: &str) -> Result<String, String> {
    var(name).ok_or_else(|| format!("{name} is not set"))
}

/// Let the serving role read and write what the migration just made.
///
/// The two roles are the point of having two: the migrating one may create a
/// table and the serving one may not (spec/installation-and-first-run,
/// requirement 13). What follows from that, and what nothing did until a
/// cluster trial on 2026-09-22 found it, is that the tables belong to the
/// migrating role and the serving role can read none of them. Every component
/// then reports that the database has no schema, which is true from where it
/// is standing and is not what is wrong.
///
/// So the migration grants what it owns: the tables and sequences it just
/// made, and default privileges so the next release's are readable without
/// another grant.
///
/// What it cannot grant is `USAGE` on the schema, unless it happens to own
/// that too: Postgres lets only an owner pass on a privilege, and warns
/// rather than failing when somebody else tries. The schema belongs to
/// whoever made the database, so usage on it is theirs to grant, and the
/// wizard's database check refuses an answer whose serving role does not have
/// it and names the statement that fixes it.
///
/// Idempotent, and a no-op when both roles are the same, which is what an
/// administrator who supplied one connection has.
pub fn grant_serving(migrating_url: &str, serving_url: &str) -> Result<(), String> {
    let serving: postgres::Config = serving_url
        .parse()
        .map_err(|failed| format!("the serving database URL is unreadable: {failed}"))?;
    let migrating: postgres::Config = migrating_url
        .parse()
        .map_err(|failed| format!("the migrating database URL is unreadable: {failed}"))?;

    let (Some(role), Some(migrator)) = (serving.get_user(), migrating.get_user()) else {
        return Err("a database URL names no role".into());
    };
    if role == migrator {
        return Ok(());
    }

    // Quoted as an identifier, and refused if it is not one. A role name
    // arrives from a wizard somebody typed into, and this is the one place a
    // name becomes SQL.
    if !role
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!("{role} is not a role name this can grant to"));
    }
    let role = format!("\"{role}\"");

    let mut client = migrating
        .connect(postgres::NoTls)
        .map_err(|failed| format!("the migrating role could not connect: {failed}"))?;

    // The schema the migration just wrote into, rather than `public` by name:
    // a deployment whose URL sets a search_path keeps its tables there, and
    // granting on the wrong schema grants nothing and says it worked.
    let schema: String = client
        // Cast, because `current_schema()` is a `name` and this wants text.
        .query_one("select current_schema()::text", &[])
        .and_then(|row| row.try_get(0))
        .map_err(|failed| format!("the migrating role's schema could not be read: {failed}"))?;
    let schema = format!("\"{schema}\"");

    for statement in [
        format!("grant usage on schema {schema} to {role}"),
        format!("grant select, insert, update, delete on all tables in schema {schema} to {role}"),
        format!("grant usage, select on all sequences in schema {schema} to {role}"),
        format!(
            "alter default privileges in schema {schema} \
             grant select, insert, update, delete on tables to {role}"
        ),
        format!("alter default privileges in schema {schema} grant usage, select on sequences to {role}"),
    ] {
        client.batch_execute(&statement).map_err(|failed| {
            // The crate's own word for every failure is "db error", and what
            // an operator needs is underneath it.
            let mut said = failed.to_string();
            let mut source = std::error::Error::source(&failed);
            while let Some(cause) = source {
                said = format!("{said}: {cause}");
                source = cause.source();
            }
            format!("{statement}: {said}")
        })?;
    }

    tracing::info!(%role, %schema, "the serving role may read and write what was migrated");
    Ok(())
}
