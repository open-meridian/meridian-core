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
use meridian_first_run::{
    BroughtServer, DatabaseProbe, FirstRun, Names, Provision, Provisioner, SealingKey,
};
use meridian_runtime::{bus_from_env, required, shutdown, var};
use prost::Message;

const SEALING_KEY: &str = "platform.config.query.first-run-sealing-key";
const CHECK: &str = "platform.config.query.check-first-run-answer";
const APPLY: &str = "platform.config.command.apply-first-run-configuration";

/// How long to wait for the answer to reach the broker before giving up and
/// exiting anyway. Generous, because the work is already done by this point
/// and the only thing left is a write; bounded, because a Job that does not
/// exit is a first run that does not finish.
const ANSWER_ON_THE_WIRE: std::time::Duration = std::time::Duration::from_secs(30);

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
        provisioner: Box::new(Postgres),
        brought: brought_server(),
        bundled_directory: var("MERIDIAN_FIRST_RUN_BUNDLED_DIRECTORY").as_deref() == Some("true"),
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
            // Signalled when the answer is on the wire, which is not the same
            // moment as the handler returning: the handler composes the reply
            // and the bus sends it afterwards. Exiting on the first lost the
            // reply often enough to be seen under load, and the wizard then
            // reported a timeout for a first run that had entirely succeeded.
            let delivered = Arc::new(tokio::sync::Notify::new());
            let applying = Arc::clone(&first_run);
            bus.serve_delivered(
                APPLY,
                move |envelope| {
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
                },
                Arc::clone(&delivered),
            );

            tracing::info!(instance_id, "first run is waiting for its wizard");

            tokio::select! {
                applied = finished.recv() => {
                    if let Some(applied) = applied {
                        tracing::info!(steps = ?applied.steps, "the configuration is applied");
                    }
                    // Bounded, because a Job that never exits is a first run
                    // that never finishes. Reaching the bound means the answer
                    // did not make it, and the wizard is about to say so, so
                    // it is worth one line saying the work was done anyway.
                    if tokio::time::timeout(ANSWER_ON_THE_WIRE, delivered.notified())
                        .await
                        .is_err()
                    {
                        tracing::warn!(
                            "the answer was not confirmed sent within {}s; the wizard will \
                             report a timeout for work that is already applied",
                            ANSWER_ON_THE_WIRE.as_secs()
                        );
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

impl Provisioner for Postgres {
    /// On a thread of its own, for the reason the check below gives.
    fn provision(
        &self,
        privileged: &DatabaseLogin,
        password: &[u8],
        plan: &Provision,
    ) -> Result<(), String> {
        std::thread::scope(|threads| {
            threads
                .spawn(|| connect_and_provision(privileged, password, plan))
                .join()
                .unwrap_or_else(|_| Err("the database could not be made".into()))
        })
    }
}

/// Make the database and its roles, as the privileged login.
///
/// Idempotent throughout, because applying twice is how a partial apply is
/// completed: a database or a role that is already there is kept, and only
/// the password and the grants are set again.
///
/// Every name here becomes SQL, and DDL takes no parameters, so each one is
/// checked before it is quoted. A name that is not an identifier is refused
/// rather than escaped: this is a wizard somebody types into.
fn connect_and_provision(
    privileged: &DatabaseLogin,
    password: &[u8],
    plan: &Provision,
) -> Result<(), String> {
    let database = identifier(&plan.database)?;
    let mut server = connect(privileged, password, &privileged.database)?;

    let exists: bool = server
        .query_one(
            "select exists(select 1 from pg_database where datname = $1)",
            &[&plan.database],
        )
        .map_err(|failed| format!("the server could not be asked about {database}: {failed}"))?
        .get(0);
    if !exists {
        // Outside a transaction, which is what CREATE DATABASE requires.
        server
            .batch_execute(&format!("create database {database}"))
            .map_err(|failed| format!("{database} could not be created: {failed}"))?;
    }

    for role in &plan.roles {
        let name = identifier(&role.name)?;
        let secret = literal(&role.password)?;
        let held: bool = server
            .query_one(
                "select exists(select 1 from pg_roles where rolname = $1)",
                &[&role.name],
            )
            .map_err(|failed| format!("the server could not be asked about {name}: {failed}"))?
            .get(0);
        let statement = match held {
            // The password is set either way: a second apply with a
            // regenerated one must still leave a deployment that can connect.
            true => format!("alter role {name} with login password {secret}"),
            false => format!("create role {name} with login password {secret}"),
        };
        server
            .batch_execute(&statement)
            .map_err(|failed| format!("{name} could not be made: {failed}"))?;
        server
            .batch_execute(&format!("grant connect on database {database} to {name}"))
            .map_err(|failed| format!("{name} could not be let into {database}: {failed}"))?;
        if role.owns_database {
            server
                .batch_execute(&format!("alter database {database} owner to {name}"))
                .map_err(|failed| format!("{name} could not be given {database}: {failed}"))?;
        }
    }

    // The schema's grants are made from inside the database, by the only role
    // entitled to make them. Postgres warns rather than refuses when somebody
    // else tries, which is why this connects again rather than hoping.
    let mut inside = connect(privileged, password, &plan.database)?;
    for role in &plan.roles {
        let name = identifier(&role.name)?;
        inside
            .batch_execute(&format!("grant usage on schema public to {name}"))
            .map_err(|failed| format!("{name} could not be given the schema: {failed}"))?;
        let schema = match role.may_create {
            true => format!("grant create on schema public to {name}"),
            // And from PUBLIC, which is where a role gets it without anybody
            // granting anything on Postgres 14 and older.
            false => format!("revoke create on schema public from {name}, public"),
        };
        inside.batch_execute(&schema).map_err(|failed| {
            format!("{name}'s rights on the schema could not be set: {failed}")
        })?;
    }

    Ok(())
}

/// The database this chart brings, when it renders one.
///
/// Its passwords are generated by the chart and mounted here. They never
/// cross the bus and nobody types them, so unlike every credential the wizard
/// collects there is nothing to seal: what would be protected in transit
/// never travels.
///
/// Absent when the chart renders no database, and then choosing to bring one
/// is refused rather than half-done.
fn brought_server() -> Option<BroughtServer> {
    Some(BroughtServer {
        workload: var("MERIDIAN_FIRST_RUN_BROUGHT_WORKLOAD")?,
        host: var("MERIDIAN_FIRST_RUN_BROUGHT_HOST")?,
        port: var("MERIDIAN_FIRST_RUN_BROUGHT_PORT")
            .and_then(|port| port.parse().ok())
            .unwrap_or(5432),
        superuser: var("MERIDIAN_FIRST_RUN_BROUGHT_SUPERUSER").unwrap_or_else(|| "postgres".into()),
        superuser_password: var("MERIDIAN_FIRST_RUN_BROUGHT_SUPERUSER_PASSWORD")?,
        serving_password: var("MERIDIAN_FIRST_RUN_BROUGHT_SERVING_PASSWORD")?,
        migrating_password: var("MERIDIAN_FIRST_RUN_BROUGHT_MIGRATING_PASSWORD")?,
        zitadel_password: var("MERIDIAN_FIRST_RUN_BROUGHT_ZITADEL_PASSWORD")?,
    })
}

/// One connection, to whichever database is named.
///
/// The same shape the check builds, and separate from it because provisioning
/// connects twice -- once to the server to make the database, and once inside
/// it to grant on the schema, which only a role entitled to may do.
fn connect(
    login: &DatabaseLogin,
    password: &[u8],
    database: &str,
) -> Result<postgres::Client, String> {
    let mut config = postgres::Config::new();
    config
        .host(&login.host)
        .port(if login.port == 0 {
            5432
        } else {
            login.port as u16
        })
        .dbname(database)
        .user(&login.role)
        .password(String::from_utf8_lossy(password).as_ref())
        .connect_timeout(std::time::Duration::from_secs(10));

    config
        .connect(postgres::NoTls)
        .map_err(|failed| format!("{} could not connect to {database}: {failed}", login.role))
}

/// A name, if it is one. Refused rather than escaped.
fn identifier(name: &str) -> Result<String, String> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    match ok {
        true => Ok(format!("\"{name}\"")),
        false => Err(format!("{name} is not a name this can make")),
    }
}

/// A string literal for a statement that cannot take a parameter.
fn literal(value: &str) -> Result<String, String> {
    if value.contains('\0') {
        return Err("a password with a null in it is not one Postgres takes".into());
    }
    Ok(format!("'{}'", value.replace('\'', "''")))
}

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
