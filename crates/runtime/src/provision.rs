//! A database made and its roles, as its privileged login: what first run
//! does for the database this chart brings, and what `migrate` does for
//! each database past the first the chart brings for a store (step 3d; the
//! resilience spec's section 6.4).

use std::time::{Duration, Instant};

use meridian_domain::v1::DatabaseLogin;
use meridian_first_run::{NewRole, Provision};

use crate::var;

/// Make the database and its roles, as the privileged login.
///
/// Idempotent throughout, because applying twice is how a partial apply is
/// completed: a database or a role that is already there is kept, and only
/// the password and the grants are set again.
///
/// Every name here becomes SQL, and DDL takes no parameters, so each one is
/// checked before it is quoted. A name that is not an identifier is refused
/// rather than escaped: this is a wizard somebody types into.
pub fn connect_and_provision(
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
    }

    // Each component's own login, a member of the serving role with its
    // password, and the serving session's statement_timeout on it (the
    // resilience spec's section 10a).
    if let Some(serving) = plan.roles.iter().find(|role| !role.may_create) {
        meridian_changes::serving::ensure_logins(
            &mut server,
            &identifier(&serving.name)?,
            &serving.password,
            &database,
        )?;
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

/// One connection, to whichever database is named.
///
/// The same shape the check builds, and separate from it because provisioning
/// connects twice -- once to the server to make the database, and once inside
/// it to grant on the schema, which only a role entitled to may do.
pub fn connect(
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

    // Over TLS where the deployment's internal TLS is on (ruling 1084).
    let (config, tls) = meridian_changes::tls::secured(config, false)
        .map_err(|failed| format!("the database's TLS: {failed}"))?;
    config
        .connect(tls)
        .map_err(|failed| format!("{} could not connect to {database}: {failed}", login.role))
}

/// A name, if it is one. Refused rather than escaped.
pub fn identifier(name: &str) -> Result<String, String> {
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
pub fn literal(value: &str) -> Result<String, String> {
    if value.contains('\0') {
        return Err("a password with a null in it is not one Postgres takes".into());
    }
    Ok(format!("'{}'", value.replace('\'', "''")))
}

/// How long `migrate` waits for a brought database to answer its superuser.
const ANSWERS_WITHIN: Duration = Duration::from_secs(300);

/// Where the chart gives `migrate` a brought database's superuser and its
/// generated passwords (`MERIDIAN_FIRST_RUN_BROUGHT_*`, as first run reads
/// them): that database made, its serving and migrating roles made with
/// them -- the roles `serving_url` and `migrating_url` name -- and each
/// component's login, before the store's schema is applied. Idempotent;
/// nothing where the chart gives none. Waits for the server to answer, as a
/// database the release is starting does not yet.
pub fn provision_brought(serving_url: &str, migrating_url: &str) -> Result<(), String> {
    let (Some(host), Some(superuser_password), Some(serving_password), Some(migrating_password)) = (
        var("MERIDIAN_FIRST_RUN_BROUGHT_HOST"),
        var("MERIDIAN_FIRST_RUN_BROUGHT_SUPERUSER_PASSWORD"),
        var("MERIDIAN_FIRST_RUN_BROUGHT_SERVING_PASSWORD"),
        var("MERIDIAN_FIRST_RUN_BROUGHT_MIGRATING_PASSWORD"),
    ) else {
        return Ok(());
    };
    let superuser =
        var("MERIDIAN_FIRST_RUN_BROUGHT_SUPERUSER").unwrap_or_else(|| "postgres".into());
    let read = |url: &str| -> Result<(String, String), String> {
        let config: postgres::Config = url
            .parse()
            .map_err(|failed| format!("a database URL is unreadable: {failed}"))?;
        match (config.get_user(), config.get_dbname()) {
            (Some(user), Some(database)) => Ok((user.to_string(), database.to_string())),
            _ => Err("a database URL names no role or no database".into()),
        }
    };
    let (serving, database) = read(serving_url)?;
    let (migrating, _) = read(migrating_url)?;
    let privileged = DatabaseLogin {
        host,
        port: 5432,
        database: "postgres".into(),
        role: superuser,
        password: None,
        ssl_mode: "disable".into(),
    };
    let plan = Provision {
        database,
        roles: vec![
            NewRole {
                name: serving,
                password: serving_password,
                may_create: false,
            },
            NewRole {
                name: migrating,
                password: migrating_password,
                may_create: true,
            },
        ],
    };
    let deadline = Instant::now() + ANSWERS_WITHIN;
    loop {
        let made = std::thread::scope(|threads| {
            threads
                .spawn(|| connect_and_provision(&privileged, superuser_password.as_bytes(), &plan))
                .join()
                .unwrap_or_else(|_| Err("the database could not be made".into()))
        });
        match made {
            Ok(()) => return Ok(()),
            Err(failed) if failed.contains("could not connect") && Instant::now() < deadline => {
                tracing::info!(%failed, "the brought database does not answer yet");
                std::thread::sleep(Duration::from_secs(2));
            }
            Err(failed) => return Err(failed),
        }
    }
}
