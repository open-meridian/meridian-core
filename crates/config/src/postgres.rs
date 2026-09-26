//! The configuration store in Postgres, behind the same trait the in-memory
//! store answers.
//!
//! Synchronous, as the other stores are: the bus runs request handlers on a
//! blocking pool, so a handler may block.
//!
//! A snapshot is read in one repeatable-read transaction, so rules and
//! derivations never see half of a change. The two operations that must be
//! atomic -- withdrawing a permission, installing the first deployment admin
//! -- lock the permission table for the length of their transaction, so a
//! concurrent pair cannot both pass the check. Permissions change rarely, and
//! a queue of two is a small price for never having no administrator.

use postgres::{IsolationLevel, NoTls};
use r2d2_postgres::PostgresConnectionManager;

use meridian_domain::v1::{
    AccessEntry, AccessGroup, AccountGroup, AccountRecord, ExternalAccountLink, Permission,
    SignInRecord, UserGroup,
};

use crate::migrations;
use crate::store::{KnownPlugin, Result, Snapshot, Store, StoreError, Withdrawal};
use crate::DEPLOYMENT_ADMIN;

type Pool = r2d2::Pool<PostgresConnectionManager<NoTls>>;
type Connection = r2d2::PooledConnection<PostgresConnectionManager<NoTls>>;

/// Names the schema lock. Distinct from the street and instrument stores', so
/// a deployment migrating them together does not have one wait on another.
const SCHEMA_LOCK: i64 = 0x636f_6e66_6967_0001_u64 as i64;

pub struct PostgresStore {
    pool: Pool,
}

impl PostgresStore {
    pub fn connect(url: &str, pool_size: u32) -> Result<Self> {
        let config: postgres::Config = url.parse().map_err(unavailable)?;
        let manager = PostgresConnectionManager::new(config, NoTls);
        let pool = r2d2::Pool::builder()
            .max_size(pool_size.max(1))
            .build(manager)
            .map_err(unavailable)?;
        Ok(Self { pool })
    }

    /// Apply every migration not yet recorded, under a lock. Run once per
    /// release by `meridian-conductor migrate`, never by a starting process.
    pub fn migrate(&self) -> Result<()> {
        let mut conn = self.conn()?;
        conn.execute("SELECT pg_advisory_lock($1)", &[&SCHEMA_LOCK])
            .map_err(unavailable)?;
        let outcome = apply_migrations(&mut conn);
        let _ = conn.execute("SELECT pg_advisory_unlock($1)", &[&SCHEMA_LOCK]);
        outcome
    }

    /// What a start does instead: read where the database is, and refuse to
    /// serve unless this binary recognises it. One read, no lock.
    pub fn verify(&self) -> Result<()> {
        let mut conn = self.conn()?;
        let applied = if table_exists(&mut conn, "config_schema_migration")? {
            applied_version(&mut conn)?
        } else {
            None
        };
        migrations::verify(applied)
    }

    fn conn(&self) -> Result<Connection> {
        self.pool.get().map_err(unavailable)
    }
}

impl Store for PostgresStore {
    fn snapshot(&self) -> Result<Snapshot> {
        let mut conn = self.conn()?;
        let mut tx = conn
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .map_err(unavailable)?;

        let mut snapshot = Snapshot::default();
        let records = &mut snapshot.records;

        for row in tx
            .query(
                "SELECT account_id, name, state, created_at_ns FROM config_account
                  ORDER BY account_id",
                &[],
            )
            .map_err(unavailable)?
        {
            records.accounts.push(AccountRecord {
                account_id: row.get(0),
                name: row.get(1),
                state: i32::from(row.get::<_, i16>(2)),
                created_at_ns: row.get(3),
            });
        }

        for row in tx
            .query(
                "SELECT user_group_id, name, directory_groups, logins FROM config_user_group
                  ORDER BY user_group_id",
                &[],
            )
            .map_err(unavailable)?
        {
            records.user_groups.push(UserGroup {
                user_group_id: row.get(0),
                name: row.get(1),
                directory_groups: row.get(2),
                logins: row.get(3),
            });
        }

        for row in tx
            .query(
                "SELECT account_group_id, name, account_ids FROM config_account_group
                  ORDER BY account_group_id",
                &[],
            )
            .map_err(unavailable)?
        {
            records.account_groups.push(AccountGroup {
                account_group_id: row.get(0),
                name: row.get(1),
                account_ids: row.get(2),
            });
        }

        let entries = tx
            .query(
                "SELECT access_group_id, plugin_instance_id, tag, level FROM config_access_entry
                  ORDER BY access_group_id, position",
                &[],
            )
            .map_err(unavailable)?;
        for row in tx
            .query(
                "SELECT access_group_id, name, built_in FROM config_access_group
                  ORDER BY access_group_id",
                &[],
            )
            .map_err(unavailable)?
        {
            let id: String = row.get(0);
            let group_entries = entries
                .iter()
                .filter(|entry| entry.get::<_, String>(0) == id)
                .map(|entry| AccessEntry {
                    plugin_instance_id: entry.get(1),
                    tag: entry.get(2),
                    level: i32::from(entry.get::<_, i16>(3)),
                })
                .collect();
            records.access_groups.push(AccessGroup {
                access_group_id: id,
                name: row.get(1),
                entries: group_entries,
                built_in: row.get(2),
            });
        }

        for row in tx
            .query(
                "SELECT permission_id, user_group_id, account_group_id, access_group_id
                   FROM config_permission ORDER BY permission_id",
                &[],
            )
            .map_err(unavailable)?
        {
            records.permissions.push(Permission {
                permission_id: row.get(0),
                user_group_id: row.get(1),
                account_group_id: row.get::<_, Option<String>>(2).unwrap_or_default(),
                access_group_id: row.get(3),
            });
        }

        for row in tx
            .query(
                "SELECT subject, display_name, directory_groups, signed_in_at_ns
                   FROM config_sign_in ORDER BY subject",
                &[],
            )
            .map_err(unavailable)?
        {
            records.people.push(SignInRecord {
                subject: row.get(0),
                display_name: row.get(1),
                directory_groups: row.get(2),
                signed_in_at_ns: row.get(3),
            });
        }

        for row in tx
            .query(
                "SELECT plugin_instance_id, external_account_id, account_id
                   FROM config_external_account_link
                  ORDER BY plugin_instance_id, external_account_id",
                &[],
            )
            .map_err(unavailable)?
        {
            snapshot.links.push(ExternalAccountLink {
                plugin_instance_id: row.get(0),
                external_account_id: row.get(1),
                account_id: row.get(2),
            });
        }

        for row in tx
            .query(
                "SELECT plugin_instance_id, roles, tags, last_reported_at_ns
                   FROM config_known_plugin ORDER BY plugin_instance_id",
                &[],
            )
            .map_err(unavailable)?
        {
            snapshot.plugins.push(KnownPlugin {
                plugin_instance_id: row.get(0),
                roles: row.get(1),
                tags: row.get(2),
                last_reported_at_ns: row.get(3),
            });
        }

        tx.commit().map_err(unavailable)?;
        Ok(snapshot)
    }

    fn put_account(&self, account: &AccountRecord) -> Result<()> {
        self.conn()?
            .execute(
                "INSERT INTO config_account (account_id, name, state, created_at_ns)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (account_id) DO UPDATE SET name = excluded.name, state = excluded.state",
                &[
                    &account.account_id,
                    &account.name,
                    &(account.state as i16),
                    &account.created_at_ns,
                ],
            )
            .map_err(unavailable)?;
        Ok(())
    }

    fn put_user_group(&self, group: &UserGroup) -> Result<()> {
        self.conn()?
            .execute(
                "INSERT INTO config_user_group (user_group_id, name, directory_groups, logins)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (user_group_id) DO UPDATE
                    SET name = excluded.name, directory_groups = excluded.directory_groups,
                        logins = excluded.logins",
                &[
                    &group.user_group_id,
                    &group.name,
                    &group.directory_groups,
                    &group.logins,
                ],
            )
            .map_err(unavailable)?;
        Ok(())
    }

    fn put_account_group(&self, group: &AccountGroup) -> Result<()> {
        self.conn()?
            .execute(
                "INSERT INTO config_account_group (account_group_id, name, account_ids)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (account_group_id) DO UPDATE
                    SET name = excluded.name, account_ids = excluded.account_ids",
                &[&group.account_group_id, &group.name, &group.account_ids],
            )
            .map_err(unavailable)?;
        Ok(())
    }

    fn put_access_group(&self, group: &AccessGroup) -> Result<()> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        tx.execute(
            "INSERT INTO config_access_group (access_group_id, name, built_in)
             VALUES ($1, $2, $3)
             ON CONFLICT (access_group_id) DO UPDATE SET name = excluded.name",
            &[&group.access_group_id, &group.name, &group.built_in],
        )
        .map_err(unavailable)?;
        tx.execute(
            "DELETE FROM config_access_entry WHERE access_group_id = $1",
            &[&group.access_group_id],
        )
        .map_err(unavailable)?;
        for (position, entry) in group.entries.iter().enumerate() {
            tx.execute(
                "INSERT INTO config_access_entry
                        (access_group_id, position, plugin_instance_id, tag, level)
                 VALUES ($1, $2, $3, $4, $5)",
                &[
                    &group.access_group_id,
                    &(position as i32),
                    &entry.plugin_instance_id,
                    &entry.tag,
                    &(entry.level as i16),
                ],
            )
            .map_err(unavailable)?;
        }
        tx.commit().map_err(unavailable)
    }

    fn put_link(&self, link: &ExternalAccountLink) -> Result<()> {
        let mut conn = self.conn()?;
        if link.account_id.is_empty() {
            conn.execute(
                "DELETE FROM config_external_account_link
                  WHERE plugin_instance_id = $1 AND external_account_id = $2",
                &[&link.plugin_instance_id, &link.external_account_id],
            )
            .map_err(unavailable)?;
        } else {
            conn.execute(
                "INSERT INTO config_external_account_link
                        (plugin_instance_id, external_account_id, account_id)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (plugin_instance_id, external_account_id)
                 DO UPDATE SET account_id = excluded.account_id",
                &[
                    &link.plugin_instance_id,
                    &link.external_account_id,
                    &link.account_id,
                ],
            )
            .map_err(unavailable)?;
        }
        Ok(())
    }

    fn add_permission(&self, permission: &Permission) -> Result<()> {
        let mut conn = self.conn()?;
        insert_permission(&mut *conn, permission)
    }

    fn withdraw_permission(&self, permission_id: &str) -> Result<Withdrawal> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        tx.batch_execute("LOCK TABLE config_permission IN SHARE ROW EXCLUSIVE MODE")
            .map_err(unavailable)?;

        let Some(row) = tx
            .query_opt(
                "SELECT access_group_id FROM config_permission WHERE permission_id = $1",
                &[&permission_id],
            )
            .map_err(unavailable)?
        else {
            return Ok(Withdrawal::Unknown);
        };
        let access_group: String = row.get(0);
        if access_group == DEPLOYMENT_ADMIN {
            let admins: i64 = tx
                .query_one(
                    "SELECT count(*) FROM config_permission WHERE access_group_id = $1",
                    &[&DEPLOYMENT_ADMIN],
                )
                .map_err(unavailable)?
                .get(0);
            if admins == 1 {
                return Ok(Withdrawal::LastAdmin);
            }
        }
        tx.execute(
            "DELETE FROM config_permission WHERE permission_id = $1",
            &[&permission_id],
        )
        .map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        Ok(Withdrawal::Withdrawn)
    }

    fn install_first_admin(&self, group: &UserGroup, permission: &Permission) -> Result<bool> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        tx.batch_execute("LOCK TABLE config_permission IN SHARE ROW EXCLUSIVE MODE")
            .map_err(unavailable)?;
        let exists = tx
            .query_opt(
                "SELECT 1 FROM config_permission WHERE access_group_id = $1 LIMIT 1",
                &[&DEPLOYMENT_ADMIN],
            )
            .map_err(unavailable)?
            .is_some();
        if exists {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO config_user_group (user_group_id, name, directory_groups, logins)
             VALUES ($1, $2, $3, $4)",
            &[
                &group.user_group_id,
                &group.name,
                &group.directory_groups,
                &group.logins,
            ],
        )
        .map_err(unavailable)?;
        insert_permission(&mut tx, permission)?;
        tx.commit().map_err(unavailable)?;
        Ok(true)
    }

    fn record_sign_in(&self, record: &SignInRecord) -> Result<()> {
        self.conn()?
            .execute(
                "INSERT INTO config_sign_in (subject, display_name, directory_groups, signed_in_at_ns)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (subject) DO UPDATE
                    SET display_name = excluded.display_name,
                        directory_groups = excluded.directory_groups,
                        signed_in_at_ns = excluded.signed_in_at_ns
                  WHERE config_sign_in.signed_in_at_ns <= excluded.signed_in_at_ns",
                &[
                    &record.subject,
                    &record.display_name,
                    &record.directory_groups,
                    &record.signed_in_at_ns,
                ],
            )
            .map_err(unavailable)?;
        Ok(())
    }

    fn record_plugin(&self, plugin: &KnownPlugin) -> Result<()> {
        self.conn()?
            .execute(
                "INSERT INTO config_known_plugin (plugin_instance_id, roles, tags, last_reported_at_ns)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (plugin_instance_id) DO UPDATE
                    SET roles = excluded.roles, tags = excluded.tags,
                        last_reported_at_ns = excluded.last_reported_at_ns",
                &[
                    &plugin.plugin_instance_id,
                    &plugin.roles,
                    &plugin.tags,
                    &plugin.last_reported_at_ns,
                ],
            )
            .map_err(unavailable)?;
        Ok(())
    }
}

fn insert_permission(
    client: &mut impl postgres::GenericClient,
    permission: &Permission,
) -> Result<()> {
    let account_group =
        (!permission.account_group_id.is_empty()).then_some(permission.account_group_id.as_str());
    client
        .execute(
            "INSERT INTO config_permission
                    (permission_id, user_group_id, account_group_id, access_group_id)
             VALUES ($1, $2, $3, $4)",
            &[
                &permission.permission_id,
                &permission.user_group_id,
                &account_group,
                &permission.access_group_id,
            ],
        )
        .map_err(unavailable)?;
    Ok(())
}

fn apply_migrations(conn: &mut Connection) -> Result<()> {
    conn.batch_execute(migrations::HISTORY)
        .map_err(unavailable)?;
    let applied = applied_version(conn)?;
    for migration in migrations::MIGRATIONS {
        if applied.is_some_and(|at| at >= migration.version) {
            continue;
        }
        // The migration and the row recording it commit together.
        let mut tx = conn.transaction().map_err(unavailable)?;
        tx.batch_execute(migration.sql).map_err(unavailable)?;
        migrations::record(&mut tx, migration, now_ns())?;
        tx.commit().map_err(unavailable)?;
    }
    Ok(())
}

fn applied_version(conn: &mut Connection) -> Result<Option<i64>> {
    let row = conn
        .query_opt("SELECT max(version) FROM config_schema_migration", &[])
        .map_err(unavailable)?;
    Ok(row.and_then(|row| row.get::<_, Option<i64>>(0)))
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos() as i64)
        .unwrap_or_default()
}

fn table_exists(conn: &mut Connection, table: &str) -> Result<bool> {
    Ok(conn
        .query_opt(
            "SELECT 1 FROM information_schema.tables
              WHERE table_schema = current_schema() AND table_name = $1",
            &[&table],
        )
        .map_err(unavailable)?
        .is_some())
}

fn unavailable(failed: impl std::error::Error) -> StoreError {
    // With the cause, because this driver's own message for most failures is
    // "db error" and everything useful is one level down.
    let mut detail = failed.to_string();
    let mut cause = failed.source();
    while let Some(next) = cause {
        detail.push_str(": ");
        detail.push_str(&next.to_string());
        cause = next.source();
    }
    StoreError::Unavailable(detail)
}
