//! The dashboard's own tables, in the database the deployment already has.
//!
//! Prefixed `dashboard_`, beside the conductor's `config_` ones: two
//! components keeping their own tables in one database is the arrangement
//! this deployment already runs, and two components sharing a table is not
//! supported. Two things are kept here:
//!
//! - the accounts this deployment holds itself ([`crate::accounts`]), used
//!   only where the firm has no directory of its own;
//! - terminal sessions ([`crate::terminal`]), by hash, in every deployment
//!   that signs people in, so a restart or an upgrade leaves them standing
//!   (W6.13, ruled 2026-09-30);
//! - clients, the delegations people make to them, and their tokens by
//!   fingerprint ([`crate::delegation`], decisions/029), for the same reason.
//!
//! The schema is applied by `meridian-dashboard migrate`, once per release,
//! as the migrating role, and a starting dashboard only verifies it: the
//! other stores' rule, for the same reason -- the role the dashboard serves
//! as may not create a table.

type Pool = r2d2::Pool<r2d2_postgres::PostgresConnectionManager<postgres::NoTls>>;
pub(crate) type Connection =
    r2d2::PooledConnection<r2d2_postgres::PostgresConnectionManager<postgres::NoTls>>;

/// Names this store's schema lock, distinct from every other component's, so
/// a deployment migrating them together does not have one wait on another.
const SCHEMA_LOCK: i64 = 0x6461_7368_626f_6172_u64 as i64;

struct Migration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

/// In order, and never reordered or edited after release: the record of what
/// ran names a version, and editing one makes that record a lie.
///
/// The first predates the history table, which is why it -- and every one
/// after it -- is written to be run again harmlessly: a database made by a
/// release before the history existed has the accounts table and no record
/// of it, and is brought up to date by running everything once.
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "local_account",
        sql: include_str!("../migrations/0001_local_account.sql"),
    },
    Migration {
        version: 2,
        name: "terminal_session",
        sql: include_str!("../migrations/0002_terminal_session.sql"),
    },
    Migration {
        version: 3,
        name: "delegation",
        sql: include_str!("../migrations/0003_delegation.sql"),
    },
];

const HISTORY: &str = "\
CREATE TABLE IF NOT EXISTS dashboard_schema_migration (
    version       bigint PRIMARY KEY,
    name          text   NOT NULL,
    applied_at_ns bigint NOT NULL
)";

fn latest() -> i64 {
    MIGRATIONS.last().map(|m| m.version).unwrap_or(0)
}

/// Why a starting dashboard will not use its database yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unverified {
    /// Behind this binary, or unreachable: the migration Job fixes the first
    /// and time the second, so both are waited for.
    NotYet(String),
    /// Ahead of this binary, which is a rollback: only the release that
    /// migrated it, or a restore, fixes it.
    Ahead(String),
}

impl std::fmt::Display for Unverified {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unverified::NotYet(said) | Unverified::Ahead(said) => f.write_str(said),
        }
    }
}

/// A pool on the dashboard's tables, shared by everything kept in them.
#[derive(Clone)]
pub struct Database {
    pool: Pool,
}

impl Database {
    pub fn connect(url: &str, pool_size: u32) -> Result<Self, String> {
        let config: postgres::Config = url.parse().map_err(|failed| format!("{failed}"))?;
        let manager = r2d2_postgres::PostgresConnectionManager::new(config, postgres::NoTls);
        let pool = r2d2::Pool::builder()
            .max_size(pool_size.max(1))
            .build(manager)
            .map_err(|failed| format!("{failed}"))?;
        Ok(Self { pool })
    }

    /// Apply every migration not yet recorded, under an advisory lock, each
    /// in one transaction with the row recording it, at the deployment's time
    /// from `clock`.
    pub fn migrate(&self, clock: &dyn crate::Clock) -> Result<(), String> {
        let mut conn = self.conn()?;
        conn.execute("SELECT pg_advisory_lock($1)", &[&SCHEMA_LOCK])
            .map_err(said)?;
        let outcome = apply(&mut conn, clock);
        let _ = conn.execute("SELECT pg_advisory_unlock($1)", &[&SCHEMA_LOCK]);
        outcome.map_err(|failed| format!("the dashboard's tables could not be made: {failed}"))
    }

    /// What a start does instead: read where the schema is, and refuse to
    /// serve unless this binary recognises it. One read, no lock.
    pub fn verify(&self) -> Result<(), Unverified> {
        let mut conn = self.conn().map_err(Unverified::NotYet)?;
        let there: bool = conn
            .query_one(
                "SELECT to_regclass('dashboard_schema_migration') IS NOT NULL",
                &[],
            )
            .map_err(|failed| Unverified::NotYet(said(failed)))?
            .get(0);
        let applied = if there {
            applied(&mut conn).map_err(Unverified::NotYet)?
        } else {
            None
        };
        let latest = latest();
        match applied {
            None => Err(Unverified::NotYet(format!(
                "the dashboard's database has none of its tables; this binary expects schema \
                 version {latest}. Run `meridian-dashboard migrate` before starting."
            ))),
            Some(at) if at < latest => Err(Unverified::NotYet(format!(
                "the dashboard's tables are at schema version {at} and this binary expects \
                 {latest}. Run `meridian-dashboard migrate` before starting."
            ))),
            Some(at) if at > latest => Err(Unverified::Ahead(format!(
                "the dashboard's tables are at schema version {at}, which is newer than this \
                 binary understands ({latest}). Run the release that migrated them, or restore \
                 a database at {latest}."
            ))),
            Some(_) => Ok(()),
        }
    }

    pub(crate) fn conn(&self) -> Result<Connection, String> {
        self.pool
            .get()
            .map_err(|failed| format!("no connection to the dashboard's database: {failed}"))
    }
}

fn apply(conn: &mut Connection, clock: &dyn crate::Clock) -> Result<(), String> {
    conn.batch_execute(HISTORY).map_err(said)?;
    let at = applied(conn)?;
    for migration in MIGRATIONS {
        if at.is_some_and(|at| at >= migration.version) {
            continue;
        }
        let mut tx = conn.transaction().map_err(said)?;
        tx.batch_execute(migration.sql).map_err(said)?;
        tx.execute(
            "INSERT INTO dashboard_schema_migration (version, name, applied_at_ns)
             VALUES ($1, $2, $3)",
            &[&migration.version, &migration.name, &clock.now_ns()],
        )
        .map_err(said)?;
        tx.commit().map_err(said)?;
    }
    Ok(())
}

fn applied(conn: &mut Connection) -> Result<Option<i64>, String> {
    let row = conn
        .query_one("SELECT max(version) FROM dashboard_schema_migration", &[])
        .map_err(said)?;
    Ok(row.get::<_, Option<i64>>(0))
}

/// A failure with its causes, because this driver's own word for most of
/// them is "db error" and everything useful is one level down.
pub(crate) fn said(failed: impl std::error::Error) -> String {
    let mut detail = failed.to_string();
    let mut cause = failed.source();
    while let Some(next) = cause {
        detail.push_str(": ");
        detail.push_str(&next.to_string());
        cause = next.source();
    }
    detail
}
