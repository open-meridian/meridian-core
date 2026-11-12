//! Each component's own login on the database, and its session's bounds
//! (the resilience spec's section 10a, "The database"; ruled 2026-11-11).
//!
//! Each component connects as its own login with its own pool, so one
//! component's query spills or is cancelled with its own refusal and never
//! exhausts the database's memory or connections for the rest:
//!
//! - **Its login**, `meridian_<component>` (lake, bor, instrument, street,
//!   conductor, dashboard): a member of the serving role, `meridian_app`,
//!   whose grants it inherits, with the serving role's password. A serving
//!   process given the serving role's URL connects as its own login in its
//!   place ([`serve_as`]). The database this chart brings is given the
//!   logins by first run and by every migration after it; on a database
//!   somebody else runs, its administrator makes them, and the upgrade's
//!   preflight names any missing with the SQL to make it. Where its own is
//!   refused -- not made yet -- it connects as the serving role, as before,
//!   and says so.
//! - **`statement_timeout`**, 5 s for every serving session ([`STATEMENT_TIMEOUT`];
//!   on its login by `ALTER ROLE ... SET` too): a query past it is cancelled
//!   (SQLSTATE 57014), and the component answers its request with its own
//!   refusal for load, `REFUSAL_REASON_OVERLOADED` with a retry-after
//!   ([`described`], [`CANCELLED`]). Work longer than a request -- a rebuild,
//!   a backfill -- sets its own in its transaction ([`unbounded`]).
//!   Migrations run on their own login, without it.
//! - **`work_mem`**, sized from the component's budget ([`work_mem_kib`]):
//!   a sort or a hash past it spills to disk rather than growing.

use std::sync::OnceLock;
use std::time::Duration;

/// The serving role every component's login is a member of.
pub const SERVING_ROLE: &str = "meridian_app";

/// The components that connect as their own login, by the name each
/// installs its budget under.
pub const COMPONENTS: [&str; 6] = [
    "lake",
    "bor",
    "instrument",
    "street",
    "conductor",
    "dashboard",
];

/// How long a serving session's statement runs before it is cancelled.
pub const STATEMENT_TIMEOUT: Duration = Duration::from_secs(5);

/// What a cancelled query's error carries in its words, so the bus answers
/// it with the component's own refusal for load.
pub const CANCELLED: &str = meridian_bus::CANCELLED_QUERY;

/// The least and the most `work_mem` a component's session is given.
const WORK_MEM_LEAST_KIB: u64 = 1024;
const WORK_MEM_MOST_KIB: u64 = 4 * 1024;

/// A component's own login.
pub fn login_of(component: &str) -> String {
    format!("meridian_{component}")
}

/// A component's `work_mem`, from its working budget: a sixty-fourth of it
/// (its pool's connections, each sorting or hashing at once, stay within a
/// fraction of what the component itself may hold), at least 1 MiB and at
/// most the database's own 4 MiB -- so the database's memory, judged as its
/// shared buffers and 4 MiB for every connection it allows, still bounds
/// every component's pools together. No limit known: the database's 4 MiB.
pub fn work_mem_kib(working_bytes: Option<u64>) -> u64 {
    match working_bytes {
        None => WORK_MEM_MOST_KIB,
        Some(working) => (working / 64 / 1024).clamp(WORK_MEM_LEAST_KIB, WORK_MEM_MOST_KIB),
    }
}

/// What a serving process's sessions are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Serving {
    pub component: String,
    pub login: String,
    pub work_mem_kib: u64,
}

static SERVING: OnceLock<Serving> = OnceLock::new();
/// Whether its own login answered: decided at its first connection.
static OWN_LOGIN: OnceLock<bool> = OnceLock::new();

/// This process serves as `component`, its working budget `working_bytes`:
/// its connections to the serving role's URL are its own login's, bounded
/// as above. Called once, by a serving binary, before it connects; never by
/// a migration, a scrub or a test, whose sessions are as their URL says.
pub fn serve_as(component: &str, working_bytes: Option<u64>) {
    let _ = SERVING.set(Serving {
        component: component.to_string(),
        login: login_of(component),
        work_mem_kib: work_mem_kib(working_bytes),
    });
}

/// This process's serving, where it serves.
pub fn serving() -> Option<&'static Serving> {
    SERVING.get()
}

/// The startup options a serving session carries, beside any the URL names.
pub fn options(serving: &Serving, named: Option<&str>) -> String {
    let ours = format!(
        "-c statement_timeout={} -c work_mem={}kB",
        STATEMENT_TIMEOUT.as_millis(),
        serving.work_mem_kib
    );
    match named {
        Some(named) if !named.trim().is_empty() => format!("{named} {ours}"),
        _ => ours,
    }
}

/// `config` as this process serves: its own login in the serving role's
/// place, where it answers, and its session's bounds. Nothing where this
/// process does not serve.
pub fn applied(config: &mut postgres::Config) {
    let Some(serving) = serving() else {
        return;
    };
    let options = options(serving, config.get_options());
    config.options(&options);
    if config.get_user() == Some(SERVING_ROLE) && own_login_answers(config, serving) {
        config.user(&serving.login);
    }
}

/// How long a serving process waits for its own login to be made -- on the
/// database the chart brings, by the migration starting beside it -- before
/// it connects as the serving role.
const OWN_LOGIN_WAIT: Duration = Duration::from_secs(60);

/// Whether `serving`'s own login is let in where `config` points, decided
/// once: refused for its name or its password -- not made yet -- it is
/// asked again every 2 s for [`OWN_LOGIN_WAIT`], then the serving role
/// stands in, said loudly. Anything else -- a database not answering yet --
/// is no answer about the login, and its own is used.
fn own_login_answers(config: &postgres::Config, serving: &Serving) -> bool {
    *OWN_LOGIN.get_or_init(|| {
        let mut probe = config.clone();
        probe.user(&serving.login);
        if probe.get_connect_timeout().is_none() {
            probe.connect_timeout(Duration::from_secs(5));
        }
        let refused = || {
            std::thread::scope(|threads| {
                threads
                    .spawn(|| {
                        // Its TLS mode is the URL's, as database_config held it.
                        let Ok(tls) = crate::tls::database_tls() else {
                            return false;
                        };
                        match probe.connect(tls) {
                            Ok(_) => false,
                            Err(failed) => failed.code().is_some_and(|code| {
                                *code == postgres::error::SqlState::INVALID_PASSWORD
                                    || *code
                                        == postgres::error::SqlState::INVALID_AUTHORIZATION_SPECIFICATION
                            }),
                        }
                    })
                    .join()
                    .unwrap_or(false)
            })
        };
        let until = std::time::Instant::now() + OWN_LOGIN_WAIT;
        loop {
            if !refused() {
                return true;
            }
            if std::time::Instant::now() >= until {
                tracing::warn!(
                    login = serving.login,
                    "this component's own database login is refused (not made yet): it \
                     connects as the serving role, {SERVING_ROLE}, until it restarts with \
                     its login made, as the upgrade's preflight says"
                );
                return false;
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    })
}

/// A database error's words, with each cause's, and its SQLSTATE where the
/// server gave one: a cancelled query's carries [`CANCELLED`].
pub fn described(failed: &(dyn std::error::Error + 'static)) -> String {
    let mut detail = failed.to_string();
    let mut code = None;
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(failed);
    let mut first = true;
    while let Some(next) = cause {
        if let Some(db) = next.downcast_ref::<postgres::error::DbError>() {
            code.get_or_insert_with(|| db.code().code().to_string());
        }
        if let Some(pg) = next.downcast_ref::<postgres::Error>() {
            if let Some(said) = pg.code() {
                code.get_or_insert_with(|| said.code().to_string());
            }
        }
        if !first {
            detail.push_str(": ");
            detail.push_str(&next.to_string());
        }
        first = false;
        cause = next.source();
    }
    if let Some(code) = code {
        detail.push_str(&format!(" [sqlstate {code}]"));
    }
    detail
}

/// Work longer than a request -- a rebuild, a backfill, a snapshot -- on a
/// serving login: its transaction's statements run without the serving
/// session's `statement_timeout`.
pub fn unbounded(transaction: &mut postgres::Transaction<'_>) -> Result<(), postgres::Error> {
    transaction.batch_execute("SET LOCAL statement_timeout = 0")
}

/// Make or keep every component's login on a database this deployment
/// brings, as its superuser on `client`: each a login with the serving
/// role's password, a member of the serving role (whose grants it
/// inherits), let into `database`, and the serving session's
/// `statement_timeout` set on it. Idempotent: a login already there has its
/// password and its setting set again. `serving_role` and `database` are
/// identifiers the caller has checked.
pub fn ensure_logins(
    client: &mut postgres::Client,
    serving_role: &str,
    serving_password: &str,
    database: &str,
) -> Result<(), String> {
    let password = format!("'{}'", serving_password.replace('\'', "''"));
    for component in COMPONENTS {
        let login = login_of(component);
        let held: bool = client
            .query_one(
                "SELECT exists(SELECT 1 FROM pg_roles WHERE rolname = $1)",
                &[&login],
            )
            .map_err(|failed| format!("{login} could not be looked for: {}", described(&failed)))?
            .get(0);
        let made = if held {
            format!("ALTER ROLE {login} WITH LOGIN PASSWORD {password}")
        } else {
            format!("CREATE ROLE {login} WITH LOGIN PASSWORD {password}")
        };
        client
            .batch_execute(&format!(
                "{made}; GRANT {serving_role} TO {login}; \
                 GRANT CONNECT ON DATABASE {database} TO {login}; \
                 ALTER ROLE {login} SET statement_timeout = '{}s'",
                STATEMENT_TIMEOUT.as_secs()
            ))
            .map_err(|failed| format!("{login} could not be made: {}", described(&failed)))?;
    }
    Ok(())
}

/// Which of the components' logins `client`'s database does not hold.
pub fn missing_logins(client: &mut postgres::Client) -> Result<Vec<String>, String> {
    let logins: Vec<String> = COMPONENTS.iter().map(|c| login_of(c)).collect();
    let held: Vec<String> = client
        .query(
            "SELECT rolname FROM pg_roles WHERE rolname = ANY($1)",
            &[&logins],
        )
        .map_err(|failed| described(&failed))?
        .iter()
        .map(|row| row.get(0))
        .collect();
    Ok(logins
        .into_iter()
        .filter(|login| !held.contains(login))
        .collect())
}

/// The SQL that makes the components' logins on a database somebody else
/// runs, for its administrator: each a member of the serving role, with its
/// password, and the serving session's bound on it.
pub fn logins_sql(missing: &[String]) -> String {
    missing
        .iter()
        .map(|login| {
            format!(
                "CREATE ROLE {login} LOGIN PASSWORD '<the serving role''s password>' IN ROLE {SERVING_ROLE}; \
                 ALTER ROLE {login} SET statement_timeout = '{}s';",
                STATEMENT_TIMEOUT.as_secs()
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_mem_is_a_sixty_fourth_of_the_budget_between_one_and_four_mib() {
        let mib = 1024 * 1024;
        assert_eq!(work_mem_kib(None), 4096);
        assert_eq!(work_mem_kib(Some(204 * mib)), 3264);
        assert_eq!(work_mem_kib(Some(1536 * mib)), 4096);
        assert_eq!(work_mem_kib(Some(8 * mib)), 1024);
    }

    #[test]
    fn a_serving_session_carries_its_bounds_beside_the_urls_options() {
        let serving = Serving {
            component: "lake".into(),
            login: login_of("lake"),
            work_mem_kib: 2048,
        };
        assert_eq!(serving.login, "meridian_lake");
        assert_eq!(
            options(&serving, None),
            "-c statement_timeout=5000 -c work_mem=2048kB"
        );
        assert_eq!(
            options(&serving, Some("-c synchronous_commit=local")),
            "-c synchronous_commit=local -c statement_timeout=5000 -c work_mem=2048kB"
        );
    }

    #[test]
    fn the_sql_for_a_missing_login_makes_it_a_member_with_its_bound() {
        let said = logins_sql(&["meridian_lake".into()]);
        assert!(said.contains("CREATE ROLE meridian_lake LOGIN"));
        assert!(said.contains("IN ROLE meridian_app"));
        assert!(said.contains("ALTER ROLE meridian_lake SET statement_timeout = '5s'"));
    }
}
