//! What every component of a deployment needs, and none should write twice.
//!
//! The runtime was one process holding the street store, the instrument store and a sidecar.
//! Decision 010 gave them a bus that crosses a process boundary, and
//! `design/split-the-runtime-into-services` ruled that they are separate
//! processes upgraded on their own schedules. This is what they share: the
//! bus they connect to, the key they present, and the
//! environment they read.
//!
//! Each binary is in `src/bin`, and each is small enough to read in one
//! sitting, which is the point of them being separate.

pub mod broker;
pub mod launched;
pub mod launcher;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meridian_bus::{Backend, Bus, MemoryBackend, NatsBackend};
use meridian_conductor::platform::ComponentReport;
use meridian_conductor::{DeploymentKey, Platform};

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

/// How often a component waiting for something it needs looks again.
pub const WAIT_AGAIN_AFTER: Duration = Duration::from_secs(2);

/// How often a waiting component says, again, what it is waiting for: often
/// enough that somebody reading the log during an upgrade sees it, rarely
/// enough that a minute's migration is not a page of the same line.
pub const WAIT_SAY_EVERY: Duration = Duration::from_secs(20);

/// Why something a component needs is not there to be used.
#[derive(Debug)]
pub enum Wait {
    /// Not yet, and time fixes it: a database not answering, a schema the
    /// migration Job has not reached, a broker still starting. An upgrade
    /// starts every component beside the Job that migrates for it, so each
    /// meets this on every release (task kernel/upgrading-a-deployment-in-place).
    NotYet(String),
    /// Never, by waiting: a schema newer than this binary understands, which
    /// only another release fixes. Refused, with the sentence that says so.
    Refused(String),
}

/// A component waiting, and saying so.
struct Waiting<'a> {
    what: &'a str,
    since: std::time::Instant,
    said: Option<std::time::Instant>,
}

impl<'a> Waiting<'a> {
    fn new(what: &'a str) -> Self {
        Self {
            what,
            since: std::time::Instant::now(),
            said: None,
        }
    }

    /// Said at once and then every [`WAIT_SAY_EVERY`], so the first line is
    /// the one that explains a pod that is running and not ready.
    fn not_yet(&mut self, why: &str) {
        let now = std::time::Instant::now();
        if self
            .said
            .is_none_or(|said| now.duration_since(said) >= WAIT_SAY_EVERY)
        {
            tracing::info!(
                waited_s = self.since.elapsed().as_secs(),
                "waiting for {}: {why}",
                self.what
            );
            self.said = Some(now);
        }
    }

    fn over(&self) {
        if self.said.is_some() {
            tracing::info!(
                waited_s = self.since.elapsed().as_secs(),
                "{} is there; no longer waiting",
                self.what
            );
        }
    }
}

/// Wait, on this thread, until `attempt` succeeds, or refuses in a way time
/// does not fix.
///
/// For what a component needs before its runtime starts: a store's database
/// and schema, which the blocking Postgres client reaches, and which must be
/// reached outside the runtime for the reason [`on_runtime`] gives.
///
/// Waiting rather than exiting is the point. A component that exits because
/// its migration has not finished is restarted by Kubernetes after a
/// back-off, and the upgrade that started it reads as a restart; one that
/// waits, not ready, is what a rollout waits for (task
/// kernel/upgrading-a-deployment-in-place, faults 3 and 4).
pub fn wait_until<T>(what: &str, attempt: impl FnMut() -> Result<T, Wait>) -> Result<T, String> {
    wait_within(what, None, WAIT_AGAIN_AFTER, attempt)
}

/// [`wait_until`], giving up once `at_most` has passed, when there is a
/// limit. Waiting and looking again are parameters so the tests below need
/// not sleep for real.
fn wait_within<T>(
    what: &str,
    at_most: Option<Duration>,
    every: Duration,
    mut attempt: impl FnMut() -> Result<T, Wait>,
) -> Result<T, String> {
    let mut waiting = Waiting::new(what);
    loop {
        match attempt() {
            Ok(done) => {
                waiting.over();
                return Ok(done);
            }
            Err(Wait::Refused(why)) => return Err(why),
            Err(Wait::NotYet(why)) => {
                if at_most.is_some_and(|limit| waiting.since.elapsed() >= limit) {
                    return Err(format!(
                        "gave up waiting for {what} after {}s: {why}",
                        waiting.since.elapsed().as_secs()
                    ));
                }
                waiting.not_yet(&why);
            }
        }
        std::thread::sleep(every);
    }
}

/// How long a migration waits for its database to answer before it fails.
///
/// Long enough for a database restarting beside the migration Job to come
/// back, which is what an upgrade that changes the database's pod does; short
/// enough that a database that is not coming back fails the release within
/// minutes rather than never.
pub const MIGRATION_WAITS_AT_MOST: Duration = Duration::from_secs(300);

/// A store's schema applied: once its database answers, and then once.
///
/// The migration Job starts beside everything else an upgrade changes, and on
/// 2026-09-29 that included the database the chart brings, restarting under
/// it: the migration raced the restart with a single attempt and lost, the Job
/// failed, and every component waited for a schema that never came (CI run
/// 36631603926). So the connection is waited for, as a component waits for
/// its own, for at most [`MIGRATION_WAITS_AT_MOST`].
///
/// Only the connection. A migration that fails once it is connected fails
/// at once: the Job runs with no retries so that a failed migration stops the
/// release rather than being tried again into it, and waiting on one would
/// undo that.
pub fn migrate_once_it_answers<S>(
    what: &str,
    url: &str,
    connect: impl Fn(&str) -> Result<S, String>,
    migrate: impl FnOnce(S) -> Result<(), String>,
) -> Result<(), String> {
    migrate_once_connected(
        what,
        MIGRATION_WAITS_AT_MOST,
        WAIT_AGAIN_AFTER,
        || {
            database_answers(url)?;
            connect(url).map_err(Wait::NotYet)
        },
        migrate,
    )
}

fn migrate_once_connected<S>(
    what: &str,
    at_most: Duration,
    every: Duration,
    connect: impl FnMut() -> Result<S, Wait>,
    migrate: impl FnOnce(S) -> Result<(), String>,
) -> Result<(), String> {
    let store = wait_within(what, Some(at_most), every, connect)?;
    migrate(store)
}

/// Whether the database at `url` answers, by one connection made and closed.
///
/// Asked before a store builds its pool, because the pool is the wrong tool
/// for the question: it spends thirty seconds failing, logging every attempt
/// as an error, and says only that it timed out. This fails in seconds and
/// says why.
pub fn database_answers(url: &str) -> Result<(), Wait> {
    let mut config: postgres::Config = url
        .parse()
        .map_err(|failed| Wait::Refused(format!("the database URL is unreadable: {failed}")))?;
    config.connect_timeout(Duration::from_secs(5));
    config
        .connect(postgres::NoTls)
        .map(drop)
        .map_err(|failed| Wait::NotYet(failed.to_string()))
}

/// A store, once its database answers and holds the schema this binary
/// expects.
///
/// `connect` builds the store's pool and `verify` reads its schema, saying
/// [`Wait::Refused`] for one newer than this binary. The pool is built once
/// and kept across attempts, so waiting for a migration is one read every
/// [`WAIT_AGAIN_AFTER`] rather than a pool made and thrown away.
pub fn wait_for_store<S>(
    what: &str,
    url: &str,
    connect: impl Fn(&str) -> Result<S, String>,
    verify: impl Fn(&S) -> Result<(), Wait>,
) -> Result<S, String> {
    let mut connected: Option<S> = None;
    wait_until(what, || {
        database_answers(url)?;
        let store = match connected.take() {
            Some(store) => store,
            None => connect(url).map_err(Wait::NotYet)?,
        };
        match verify(&store) {
            Ok(()) => Ok(store),
            Err(failed) => {
                connected = Some(store);
                Err(failed)
            }
        }
    })
}

/// Run a component's asynchronous half to its end, on a runtime of its own
/// that is stopped before this returns.
///
/// The blocking Postgres client closes a connection by running a runtime of
/// its own, and that panics on a thread already driving one. So a store
/// dropped inside the runtime -- by its last task ending, or by an early
/// return taking the async block's captures with it -- panicked on every
/// shutdown, and aborted as the pool's other connections were dropped while
/// unwinding ("panic in a destructor during cleanup"). The dashboard did that
/// on a cluster when it started before its database and broker were there
/// (task kernel/upgrading-a-deployment-in-place, fault 5).
///
/// The rule this keeps: a store is made before this is called and held by the
/// caller, the future borrows it, and the runtime is gone before the caller's
/// last reference is. Only a drop outside any runtime closes a connection.
pub fn on_runtime<F>(serving: F) -> Result<(), String>
where
    F: std::future::Future<Output = Result<(), String>>,
{
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|failed| failed.to_string())?;
    let served = runtime.block_on(serving);
    // Every task, and every store reference a task held, goes here, while the
    // caller still holds its own.
    drop(runtime);
    served
}

/// Whether this component serves, as the file its readiness probe looks for.
///
/// A component that waits for what it needs has to be seen waiting: without a
/// readiness that says otherwise, its pod counts as ready the moment it
/// starts, the rollout stops the old one, and an upgrade reports success over
/// a component still waiting for its schema. The components with no port say
/// it with a file (`MERIDIAN_READY_FILE`, which the chart sets and probes);
/// with the variable unset -- compose, a developer's shell -- this does
/// nothing.
pub struct Ready(Option<std::path::PathBuf>);

impl Ready {
    /// Not ready, whatever an earlier container in this pod said: the file
    /// is on a volume that outlives a restarted container, and a new process
    /// waiting for its schema must not inherit the old one's word.
    pub fn from_env() -> Self {
        let path = var("MERIDIAN_READY_FILE").map(std::path::PathBuf::from);
        if let Some(path) = &path {
            let _ = std::fs::remove_file(path);
        }
        Self(path)
    }

    /// Serving: said once everything this component needs is there and its
    /// handlers are registered.
    pub fn serving(&self) {
        let Some(path) = &self.0 else { return };
        match std::fs::write(path, b"serving\n") {
            Ok(()) => tracing::debug!(path = %path.display(), "marked ready"),
            Err(failed) => tracing::warn!(
                path = %path.display(),
                %failed,
                "this component serves and could not say so; its readiness probe will fail"
            ),
        }
    }
}

impl Drop for Ready {
    /// Stopping is no longer serving.
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The bus this component talks on.
///
/// A broker when one is configured, and in-process when none is. Both are
/// real: a developer running one binary needs no broker, and a deployment with
/// separate components cannot work without one. What decides it is a single
/// setting rather than a build flag, so the same image does both.
///
/// A broker that does not answer yet is waited for, and never a reason to
/// exit: in an upgrade the broker restarts beside everything that connects to
/// it, and a component that exited on it was restarted once per release.
pub async fn bus_from_env(instance_id: &str) -> Result<Arc<Bus>, String> {
    let backend: Arc<dyn Backend> = match var("MERIDIAN_BROKER_URL") {
        Some(url) => {
            let mut waiting = Waiting::new("the broker");
            let broker = loop {
                match NatsBackend::connect(&url).await {
                    Ok(broker) => break broker,
                    Err(failed) => waiting.not_yet(&format!("it could not be reached: {failed}")),
                }
                tokio::time::sleep(WAIT_AGAIN_AFTER).await;
            };
            waiting.over();
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

/// Names from one comma-separated value, blanks dropped: a sidecar's roles.
///
/// Empty and unset are the same thing here: a sidecar with no roles is a
/// plugin admitted with no topics
/// (decisions/020). v1 read this from the environment too, and a null value
/// there meant a plugin that registered and then had every publish denied, so
/// the sidecar logs what it was launched with rather than leaving an operator
/// to infer it from refusals.
pub fn names_from(raw: Option<String>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::time::Duration;

    use super::{migrate_once_connected, Wait};

    const EVERY: Duration = Duration::from_millis(1);
    const AT_MOST: Duration = Duration::from_secs(5);

    #[test]
    fn a_database_that_does_not_answer_yet_is_waited_for_and_then_migrated() {
        let tried = Cell::new(0);
        let migrated = Cell::new(0);
        let done = migrate_once_connected(
            "the database",
            AT_MOST,
            EVERY,
            || {
                tried.set(tried.get() + 1);
                if tried.get() < 3 {
                    Err(Wait::NotYet("error connecting to server".into()))
                } else {
                    Ok("connected")
                }
            },
            |store| {
                assert_eq!(store, "connected");
                migrated.set(migrated.get() + 1);
                Ok(())
            },
        );
        assert_eq!(done, Ok(()));
        assert_eq!(tried.get(), 3, "it did not try again until it answered");
        assert_eq!(migrated.get(), 1);
    }

    #[test]
    fn a_migration_that_fails_once_connected_fails_at_once() {
        // The Job runs with no retries so a failed migration stops the
        // release. Waiting on one here would be retrying it anyway.
        let tried = Cell::new(0);
        let migrated = Cell::new(0);
        let done = migrate_once_connected(
            "the database",
            AT_MOST,
            EVERY,
            || {
                tried.set(tried.get() + 1);
                Ok(())
            },
            |()| {
                migrated.set(migrated.get() + 1);
                Err("column \"note\" is of the wrong type".into())
            },
        );
        assert_eq!(done, Err("column \"note\" is of the wrong type".into()));
        assert_eq!(
            tried.get(),
            1,
            "it connected again after the migration failed"
        );
        assert_eq!(migrated.get(), 1, "it migrated again after failing");
    }

    #[test]
    fn a_database_that_never_answers_is_given_up_on() {
        let tried = Cell::new(0);
        let done = migrate_once_connected(
            "the database",
            Duration::from_millis(30),
            EVERY,
            || -> Result<(), Wait> {
                tried.set(tried.get() + 1);
                Err(Wait::NotYet("error connecting to server".into()))
            },
            |()| panic!("it migrated without a connection"),
        );
        let said = done.expect_err("it waited for ever");
        assert!(
            said.starts_with("gave up waiting for the database after ")
                && said.ends_with(": error connecting to server"),
            "it did not say what it gave up on, or why: {said}"
        );
        assert!(tried.get() > 1, "it gave up without trying again");
    }

    #[test]
    fn a_url_that_cannot_be_read_is_not_waited_for() {
        let tried = Cell::new(0);
        let done = migrate_once_connected(
            "the database",
            AT_MOST,
            EVERY,
            || -> Result<(), Wait> {
                tried.set(tried.get() + 1);
                Err(Wait::Refused("the database URL is unreadable".into()))
            },
            |()| panic!("it migrated without a connection"),
        );
        assert_eq!(done, Err("the database URL is unreadable".into()));
        assert_eq!(tried.get(), 1);
    }
}
