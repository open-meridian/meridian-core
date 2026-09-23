//! The first-run Job: the only thing in a deployment that may change the
//! cluster, for as long as it takes to configure one.
//!
//! It answers three things from the dashboard, and does nothing else:
//!
//! - **the sealing key** (W7.4), made in memory when this starts, so every
//!   credential the wizard sends is sealed to it and the broker sees
//!   ciphertext;
//! - **a check** (W7.4), which tests an answer and writes nothing;
//! - **apply** (W7.5), which writes the named Secrets, patches the one named
//!   NetworkPolicy, restarts the named Deployments, and then deletes its own
//!   RoleBinding.
//!
//! After that it exits, and nothing in the deployment holds a right to change
//! the cluster (decisions/016). A deployment already configured starts this,
//! finds its work done, gives the rights up and exits, which is what makes a
//! second install of the same release harmless.
//!
//! Every name it may touch comes from the chart, because the Role names the
//! same ones: a name this invented would be refused by RBAC anyway.

use std::sync::Arc;

use meridian_domain::v1::{
    DatabaseLogin, FirstRunApplied, FirstRunCheckReply, FirstRunCheckRequest,
    FirstRunConfiguration, FirstRunSealingKey,
};
use meridian_first_run::cluster::ApiServer;
use meridian_first_run::{DatabaseProbe, FirstRun, Names, SealingKey};
use meridian_runtime::{bus_from_env, required, shutdown, var};
use prost::Message;

const SEALING_KEY: &str = "platform.config.query.first-run-sealing-key";
const CHECK: &str = "platform.config.query.check-first-run-answer";
const APPLY: &str = "platform.config.command.apply-first-run-configuration";

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    if let Err(failed) = run() {
        tracing::error!(%failed, "the first-run job stopped");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let instance_id = var("MERIDIAN_INSTANCE_ID").unwrap_or_else(|| "first-run-1".into());
    let names = Names {
        database_secret: required("MERIDIAN_FIRST_RUN_DATABASE_SECRET")?,
        zitadel_database_secret: required("MERIDIAN_FIRST_RUN_ZITADEL_DATABASE_SECRET")?,
        dashboard_oidc_secret: required("MERIDIAN_FIRST_RUN_OIDC_SECRET")?,
        ldap_bind_secret: required("MERIDIAN_FIRST_RUN_LDAP_SECRET")?,
        addresses_secret: required("MERIDIAN_FIRST_RUN_ADDRESSES_SECRET")?,
        zitadel_egress_policy: required("MERIDIAN_FIRST_RUN_EGRESS_POLICY")?,
        own_binding: required("MERIDIAN_FIRST_RUN_BINDING")?,
        restart: list("MERIDIAN_FIRST_RUN_RESTART"),
        bundled_identity: list("MERIDIAN_FIRST_RUN_BUNDLED_IDENTITY"),
    };

    let first_run = Arc::new(FirstRun {
        // Named for this Job, so a credential says which one it was sealed to
        // and a restarted Job asks the dashboard to seal again rather than
        // failing to read it.
        key: SealingKey::new(instance_id.clone()),
        names,
        cluster: Box::new(ApiServer::in_cluster().map_err(|failed| failed.to_string())?),
        probe: Box::new(Postgres),
    });

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|failed| failed.to_string())?
        .block_on(async {
            // A deployment somebody has already configured has no work here.
            // Holding the rights while waiting for a wizard nobody will open
            // is the state decisions/016 exists to prevent.
            if first_run.already_configured().await? {
                first_run.stand_down().await?;
                tracing::info!("this deployment is already configured; the rights are given up");
                return Ok(());
            }

            let bus = bus_from_env(&instance_id).await?;

            let answering = Arc::clone(&first_run);
            bus.serve(SEALING_KEY, move |envelope| {
                expect(
                    &envelope.payload_type,
                    "meridian.v1.FirstRunSealingKeyRequest",
                )?;
                let key = FirstRunSealingKey {
                    public_key: answering.key.public_key(),
                    key_id: answering.key.key_id.clone(),
                };
                Ok((
                    "meridian.v1.FirstRunSealingKey".to_string(),
                    key.encode_to_vec(),
                ))
            });

            let checking = Arc::clone(&first_run);
            bus.serve(CHECK, move |envelope| {
                expect(&envelope.payload_type, "meridian.v1.FirstRunCheckRequest")?;
                let request = FirstRunCheckRequest::decode(&envelope.payload[..])
                    .map_err(|failed| format!("undecodable check: {failed}"))?;
                let reply: FirstRunCheckReply = checking.check(&request);
                Ok((
                    "meridian.v1.FirstRunCheckReply".to_string(),
                    reply.encode_to_vec(),
                ))
            });

            // Applying is the one thing here that changes anything, and the
            // one thing that ends this Job: it replies, then stops serving.
            let (done, mut finished) = tokio::sync::mpsc::channel::<FirstRunApplied>(1);
            let applying = Arc::clone(&first_run);
            bus.serve(APPLY, move |envelope| {
                expect(&envelope.payload_type, "meridian.v1.FirstRunConfiguration")?;
                let configuration = FirstRunConfiguration::decode(&envelope.payload[..])
                    .map_err(|failed| format!("undecodable configuration: {failed}"))?;

                let applying = Arc::clone(&applying);
                let done = done.clone();
                let applied = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current()
                        .block_on(async move { applying.apply(&configuration).await })
                });

                if applied.applied {
                    let _ = done.try_send(applied.clone());
                }
                Ok((
                    "meridian.v1.FirstRunApplied".to_string(),
                    applied.encode_to_vec(),
                ))
            });

            tracing::info!(instance_id, "first run is waiting for its wizard");

            tokio::select! {
                applied = finished.recv() => {
                    if let Some(applied) = applied {
                        tracing::info!(steps = ?applied.steps, "the configuration is applied");
                    }
                }
                () = shutdown() => tracing::info!("stopping before the configuration was applied"),
            }
            Ok(())
        })
}

fn expect(presented: &str, wanted: &str) -> Result<(), String> {
    if presented == wanted {
        Ok(())
    } else {
        Err(format!("expected {wanted}, and this is {presented}"))
    }
}

fn list(name: &str) -> Vec<String> {
    var(name)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(String::from)
        .collect()
}

/// What a database answer is tested against: the connection is made, and the
/// one privilege that matters either way is read back.
///
/// No TLS, like every other Postgres connection this runtime makes. The
/// answer's `sslmode` travels into the Secret and is what the components
/// themselves present; making this connection match them is its own task.
struct Postgres;

impl DatabaseProbe for Postgres {
    /// On a thread of its own, because the synchronous Postgres client builds
    /// an async runtime inside itself and doing that on a thread already
    /// driving one panics. Found by applying a configuration on a cluster:
    /// the check passed, the apply re-checked, and the Job took the panic.
    fn check(&self, login: &DatabaseLogin, password: &[u8], may_create: bool) -> Vec<String> {
        std::thread::scope(|threads| {
            threads
                .spawn(|| self.connect_and_check(login, password, may_create))
                .join()
                .unwrap_or_else(|_| vec![format!("{} could not be checked", login.role)])
        })
    }
}

impl Postgres {
    fn connect_and_check(
        &self,
        login: &DatabaseLogin,
        password: &[u8],
        may_create: bool,
    ) -> Vec<String> {
        let mut config = postgres::Config::new();
        config
            .host(&login.host)
            .port(if login.port == 0 {
                5432
            } else {
                login.port as u16
            })
            .dbname(&login.database)
            .user(&login.role)
            .password(String::from_utf8_lossy(password).as_ref())
            .connect_timeout(std::time::Duration::from_secs(10));

        let mut client = match config.connect(postgres::NoTls) {
            Ok(client) => client,
            Err(failed) => {
                return vec![format!(
                    "{}@{}:{}/{} could not be reached: {}",
                    login.role,
                    login.host,
                    login.port,
                    login.database,
                    // The crate's own Display is "db error" and the sentence
                    // an administrator needs is in its source. A finding that
                    // does not say what is wrong is a finding they cannot act
                    // on.
                    detail(&failed)
                )];
            }
        };

        let allowed: bool = match client
            .query_one(
                "select has_schema_privilege(current_user, 'public', 'CREATE')",
                &[],
            )
            .and_then(|row| row.try_get(0))
        {
            Ok(allowed) => allowed,
            Err(failed) => {
                return vec![format!(
                    "{} could not be checked: {}",
                    login.role,
                    detail(&failed)
                )]
            }
        };

        // Requirement 13 asks for read and write as well, and the cluster
        // trial showed why: a serving role that may not even use the schema
        // is one every component reports as an empty database. What it may do
        // to tables the migration has not made yet cannot be tested here; the
        // migration grants that, and this is what can be checked now.
        let usable: bool = match client
            .query_one(
                "select has_schema_privilege(current_user, 'public', 'USAGE')",
                &[],
            )
            .and_then(|row| row.try_get(0))
        {
            Ok(usable) => usable,
            Err(failed) => {
                return vec![format!(
                    "{} could not be checked: {}",
                    login.role,
                    detail(&failed)
                )]
            }
        };
        if !usable {
            return vec![format!(
                "{} may not use schema public, so it could read nothing: \
                 grant USAGE on schema public to it",
                login.role
            )];
        }

        match (may_create, allowed) {
            // The finding worth having: a serving role that may create tables
            // is one that can migrate a customer's database by restarting.
            (false, true) => vec![format!(
                "{} may create tables, and serving roles must not: revoke CREATE on schema public from it",
                login.role
            )],
            (true, false) => vec![format!(
                "{} may not create tables, and migrating needs to: grant CREATE on schema public to it",
                login.role
            )],
            _ => Vec::new(),
        }
    }
}

/// What actually went wrong, rather than the wrapper's word for it.
fn detail(failed: &postgres::Error) -> String {
    let mut said = failed.to_string();
    let mut source = std::error::Error::source(failed);
    while let Some(cause) = source {
        said = format!("{said}: {cause}");
        source = cause.source();
    }
    said
}
