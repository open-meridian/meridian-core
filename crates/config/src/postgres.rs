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
    AccessEntry, AccessGroup, AccountGroup, AccountRecord, ExternalAccountLink, Hold,
    LinkableAccounts, MoveRecord, Permission, PluginArchive, PluginLaunch, PluginMetadata,
    PluginVersion, SettingLastChange, SignInRecord, UserGroup,
};

use meridian_pb::v1::{PluginDeclaration, RecordMoveRequest, SettingDeclaration, StoredSpan};
use prost::Message;

use crate::migrations;
use crate::store::{
    archived_spans, group_change, known_plugins, permission_change, redeclared, AccessChangeKind,
    AccessChangeRecord, Author, ChangeKind, Ending, Held, KnownPlugin, LastChange, LaunchAct,
    LaunchNote, LinkableAccountsChange, RecordedMove, Result, SettingChange, SettingChangeRecord,
    SettingsAuthor, Snapshot, Store, StoreError, StoredSetting, Withdrawal,
    REDACTED_BY_REDECLARATION, REDACTED_BY_SEALING,
};
use crate::store::{ConnectivityLinkChange, HeldMapping, VenueAccountMapChange};
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
    ///
    /// Each migration is recorded at the deployment's time, from `clock`.
    pub fn migrate(&self, clock: &dyn meridian_clock::Clock) -> Result<()> {
        let mut conn = self.conn()?;
        conn.execute("SELECT pg_advisory_lock($1)", &[&SCHEMA_LOCK])
            .map_err(unavailable)?;
        let outcome = apply_migrations(&mut conn, clock);
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
                "SELECT account_id, name, state, created_at_ns,
                        custodian, account_type, owner, note
                   FROM config_account
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
                custodian: row.get::<_, Option<String>>(4).unwrap_or_default(),
                account_type: row.get::<_, Option<String>>(5).unwrap_or_default(),
                owner: row.get::<_, Option<String>>(6).unwrap_or_default(),
                note: row.get::<_, Option<String>>(7).unwrap_or_default(),
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
                "SELECT account_group_id, name, account_ids, built_in FROM config_account_group
                  ORDER BY account_group_id",
                &[],
            )
            .map_err(unavailable)?
        {
            records.account_groups.push(AccountGroup {
                account_group_id: row.get(0),
                name: row.get(1),
                account_ids: row.get(2),
                built_in: row.get(3),
            });
        }

        let entries = tx
            .query(
                "SELECT access_group_id, plugin_instance_id, level, role FROM config_access_entry
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
                    level: i32::from(entry.get::<_, i16>(2)),
                    role: entry.get(3),
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
                "SELECT plugin_instance_id, roles, last_reported_at_ns
                   FROM config_known_plugin ORDER BY plugin_instance_id",
                &[],
            )
            .map_err(unavailable)?
        {
            snapshot.plugins.push(KnownPlugin {
                plugin_instance_id: row.get(0),
                roles: row.get(1),
                last_reported_at_ns: row.get(2),
            });
        }
        // Each known plugin's roles on the records, so access is folded per
        // plugin and role wherever it is evaluated (W6.1, contract v15).
        snapshot.records.known_plugins = known_plugins(&snapshot.plugins);

        for row in tx
            .query(
                "SELECT plugin_instance_id, name, type, required, secret, description, declared
                   FROM config_plugin_setting_declaration
                  ORDER BY plugin_instance_id, position",
                &[],
            )
            .map_err(unavailable)?
        {
            // Whole where it was kept whole; from the columns for a row
            // written before it was, which the plugin's next report replaces.
            let whole = row
                .get::<_, Option<Vec<u8>>>(6)
                .map(|bytes| SettingDeclaration::decode(&bytes[..]))
                .transpose()
                .map_err(|failed| {
                    StoreError::Unavailable(format!(
                        "a setting declaration does not decode: {failed}"
                    ))
                })?;
            let declaration = whole.unwrap_or_else(|| SettingDeclaration {
                name: row.get(1),
                r#type: i32::from(row.get::<_, i16>(2)),
                required: row.get(3),
                secret: row.get(4),
                description: row.get(5),
                ..Default::default()
            });
            snapshot
                .declared_settings
                .entry(row.get(0))
                .or_default()
                .push(declaration);
        }

        for row in tx
            .query(
                "SELECT plugin_instance_id, name, value, sealed, set_by, set_at_ns
                   FROM config_plugin_setting ORDER BY plugin_instance_id, name",
                &[],
            )
            .map_err(unavailable)?
        {
            let held = match (
                row.get::<_, Option<String>>(2),
                row.get::<_, Option<Vec<u8>>>(3),
            ) {
                (_, Some(sealed)) => Held::Sealed(sealed),
                (value, None) => Held::Plain(value.unwrap_or_default()),
            };
            snapshot.settings.push(StoredSetting {
                plugin_instance_id: row.get(0),
                name: row.get(1),
                held,
                set_by: row.get(4),
                set_at_ns: row.get(5),
            });
        }

        // Each plugin's latest change, a clear included: not a gap, which
        // changes nothing, nor a redaction, which blanks a record's value.
        for row in tx
            .query(
                "SELECT DISTINCT ON (plugin_instance_id) plugin_instance_id, changed_by,
                        changed_at_ns
                   FROM config_plugin_setting_change
                  WHERE action IN (1, 2)
                  ORDER BY plugin_instance_id, change_id DESC",
                &[],
            )
            .map_err(unavailable)?
        {
            snapshot.settings_changed.insert(
                row.get(0),
                LastChange {
                    by: row.get(1),
                    at_ns: row.get(2),
                },
            );
        }

        // And each setting's latest, a secret's included, never a value
        // (contract v17).
        for row in tx
            .query(
                "SELECT DISTINCT ON (plugin_instance_id, name) plugin_instance_id, name,
                        changed_by, changed_at_ns, through_delegation, client_name, note,
                        prepared_call_id, hint
                   FROM config_plugin_setting_change
                  WHERE action IN (1, 2)
                  ORDER BY plugin_instance_id, name, change_id DESC",
                &[],
            )
            .map_err(unavailable)?
        {
            snapshot
                .setting_changes
                .entry(row.get(0))
                .or_default()
                .push(SettingLastChange {
                    name: row.get(1),
                    changed_by: row.get(2),
                    changed_at_ns: row.get(3),
                    acting_through_delegation: row.get(4),
                    client_name: row.get(5),
                    note: row.get(6),
                    prepared_call_id: row.get(7),
                    hint: row.get(8),
                });
        }

        for row in tx
            .query(
                "SELECT name, version, roles, interface, sdk_version, image_digest,
                        uploaded_by, uploaded_at_ns, declaration, source_registry, listing,
                        key_status_issued_at_ns, retired_by, retired_at_ns, retired_note,
                        uploaded_note
                   FROM config_plugin_version ORDER BY name, version",
                &[],
            )
            .map_err(unavailable)?
        {
            snapshot.catalogue.versions.push(PluginVersion {
                metadata: Some(PluginMetadata {
                    name: row.get(0),
                    version: row.get(1),
                    roles: row.get(2),
                    interface: row.get(3),
                    sdk_version: row.get(4),
                    declaration: row
                        .get::<_, Option<Vec<u8>>>(8)
                        .map(|bytes| PluginDeclaration::decode(bytes.as_slice()))
                        .transpose()
                        .map_err(|failed| {
                            StoreError::Unavailable(format!(
                                "a version's declaration did not read: {failed}"
                            ))
                        })?,
                }),
                image_digest: row.get(5),
                uploaded_by: row.get(6),
                uploaded_at_ns: row.get(7),
                source_registry: row.get(9),
                listing: row.get(10),
                key_status_issued_at_ns: row.get(11),
                retired_by: row.get(12),
                retired_at_ns: row.get(13),
                retired_note: row.get(14),
                uploaded_note: row.get(15),
            });
        }

        for row in tx
            .query(
                &format!("SELECT {LAUNCH_COLUMNS} FROM config_plugin_launch ORDER BY launch_id"),
                &[],
            )
            .map_err(unavailable)?
        {
            snapshot.catalogue.launches.push(launch_from(&row));
        }

        // Each role's latest hold, a cleared one left out, and each
        // instance's latest archive (W6.25, W8.7, contract v16).
        for row in tx
            .query(
                &format!(
                    "SELECT {HOLD_COLUMNS} FROM (
                         SELECT DISTINCT ON (role) * FROM config_hold_change
                          ORDER BY role, change_id DESC) latest
                      WHERE days > 0 ORDER BY role"
                ),
                &[],
            )
            .map_err(unavailable)?
        {
            snapshot.holds.push(hold_from(&row).0);
        }
        for row in tx
            .query(
                &format!(
                    "SELECT DISTINCT ON (instance_id) {ARCHIVE_COLUMNS}
                       FROM config_archive_change ORDER BY instance_id, change_id DESC"
                ),
                &[],
            )
            .map_err(unavailable)?
        {
            snapshot.archives.push(archive_from(&row).0);
        }
        // The data configuration (contract v18): each catalogue, each
        // dataset's latest licence and each entitlement's latest.
        for row in tx
            .query(
                "SELECT plugin_instance_id, catalogue FROM config_plugin_catalogue
                  ORDER BY plugin_instance_id",
                &[],
            )
            .map_err(unavailable)?
        {
            let bytes: Vec<u8> = row.get(1);
            snapshot.catalogues.insert(
                row.get(0),
                meridian_pb::v1::Catalogue::decode(&bytes[..]).map_err(unavailable)?,
            );
        }
        for row in tx
            .query(
                "SELECT DISTINCT ON (dataset) licence FROM config_dataset_licence_change
                  ORDER BY dataset, change_id DESC",
                &[],
            )
            .map_err(unavailable)?
        {
            let bytes: Vec<u8> = row.get(0);
            snapshot
                .licences
                .push(meridian_pb::v1::DatasetLicence::decode(&bytes[..]).map_err(unavailable)?);
        }
        for row in tx
            .query(
                "SELECT DISTINCT ON (dataset, instance) entitlement
                   FROM config_dataset_entitlement_change
                  ORDER BY dataset, instance, change_id DESC",
                &[],
            )
            .map_err(unavailable)?
        {
            let bytes: Vec<u8> = row.get(0);
            snapshot.entitlements.push(
                meridian_domain::v1::DatasetEntitlement::decode(&bytes[..]).map_err(unavailable)?,
            );
        }

        // Each instance's linkable accounts and its latest change (W6.30,
        // contract v20).
        for row in tx
            .query(
                "SELECT DISTINCT ON (plugin_instance_id) plugin_instance_id, changed_by,
                        changed_at_ns, through_delegation, client_name
                   FROM config_linkable_accounts_change
                  ORDER BY plugin_instance_id, change_id DESC",
                &[],
            )
            .map_err(unavailable)?
        {
            let instance: String = row.get(0);
            snapshot.linkable.insert(
                instance.clone(),
                LinkableAccounts {
                    plugin_instance_id: instance,
                    account_ids: Vec::new(),
                    updated_by: row.get(1),
                    updated_at_ns: row.get(2),
                    acting_through_delegation: row.get(3),
                    client_name: row.get(4),
                },
            );
        }
        for row in tx
            .query(
                "SELECT plugin_instance_id, account_id FROM config_linkable_account
                  ORDER BY plugin_instance_id, account_id",
                &[],
            )
            .map_err(unavailable)?
        {
            let instance: String = row.get(0);
            snapshot
                .linkable
                .entry(instance.clone())
                .or_insert_with(|| LinkableAccounts {
                    plugin_instance_id: instance,
                    ..LinkableAccounts::default()
                })
                .account_ids
                .push(row.get(1));
        }

        // Each connectivity plugin's link and each venue account mapping
        // standing (W11.21, W11.22, contract v22).
        for row in tx
            .query(
                "SELECT connectivity_instance_id, link FROM config_connectivity_link
                  ORDER BY connectivity_instance_id",
                &[],
            )
            .map_err(unavailable)?
        {
            let bytes: Vec<u8> = row.get(1);
            snapshot.connectivity.insert(
                row.get(0),
                meridian_pb::v1::ConnectivityLink::decode(&bytes[..]).map_err(unavailable)?,
            );
        }
        for row in tx
            .query(
                "SELECT ems_instance_id, connectivity_instance_id, venue_account_code, account_id
                   FROM config_venue_account_mapping
                  ORDER BY connectivity_instance_id, venue_account_code",
                &[],
            )
            .map_err(unavailable)?
        {
            snapshot.venue_mappings.push(HeldMapping {
                ems_instance_id: row.get(0),
                mapping: meridian_pb::v1::VenueAccountMapping {
                    connectivity_instance_id: row.get(1),
                    venue_account_code: row.get(2),
                    account_id: row.get(3),
                },
            });
        }

        tx.commit().map_err(unavailable)?;
        Ok(snapshot)
    }

    fn put_account(&self, account: &AccountRecord) -> Result<()> {
        self.conn()?
            .execute(
                "INSERT INTO config_account
                        (account_id, name, state, created_at_ns,
                         custodian, account_type, owner, note)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                 ON CONFLICT (account_id) DO UPDATE
                    SET name = excluded.name, state = excluded.state,
                        custodian = excluded.custodian, account_type = excluded.account_type,
                        owner = excluded.owner, note = excluded.note",
                &[
                    &account.account_id,
                    &account.name,
                    &(account.state as i16),
                    &account.created_at_ns,
                    &unless_empty(&account.custodian),
                    &unless_empty(&account.account_type),
                    &unless_empty(&account.owner),
                    &unless_empty(&account.note),
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

    fn put_access_group(&self, group: &AccessGroup, author: &Author, at_ns: i64) -> Result<()> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        // The group as it stood, read in the transaction that replaces it, so
        // its record says what it was.
        let before = read_access_group(&mut tx, &group.access_group_id)?;
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
                        (access_group_id, position, plugin_instance_id, level, role)
                 VALUES ($1, $2, $3, $4, $5)",
                &[
                    &group.access_group_id,
                    &(position as i32),
                    &entry.plugin_instance_id,
                    &(entry.level as i16),
                    &entry.role,
                ],
            )
            .map_err(unavailable)?;
        }
        if let Some(record) = group_change(before.as_ref(), group, author, at_ns) {
            insert_access_change(&mut tx, &record)?;
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

    fn put_account_and_link(
        &self,
        account: &AccountRecord,
        link: &ExternalAccountLink,
    ) -> Result<()> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        tx.execute(
            "INSERT INTO config_account
                    (account_id, name, state, created_at_ns,
                     custodian, account_type, owner, note)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            &[
                &account.account_id,
                &account.name,
                &(account.state as i16),
                &account.created_at_ns,
                &unless_empty(&account.custodian),
                &unless_empty(&account.account_type),
                &unless_empty(&account.owner),
                &unless_empty(&account.note),
            ],
        )
        .map_err(unavailable)?;
        tx.execute(
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
        tx.commit().map_err(unavailable)?;
        Ok(())
    }

    fn add_permission(&self, permission: &Permission, author: &Author, at_ns: i64) -> Result<()> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        insert_permission(&mut tx, permission)?;
        insert_access_change(&mut tx, &permission_change(permission, true, author, at_ns))?;
        tx.commit().map_err(unavailable)
    }

    fn withdraw_permission(
        &self,
        permission_id: &str,
        author: &Author,
        at_ns: i64,
    ) -> Result<Withdrawal> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        tx.batch_execute("LOCK TABLE config_permission IN SHARE ROW EXCLUSIVE MODE")
            .map_err(unavailable)?;

        let Some(row) = tx
            .query_opt(
                "SELECT access_group_id, user_group_id, account_group_id FROM config_permission
                  WHERE permission_id = $1",
                &[&permission_id],
            )
            .map_err(unavailable)?
        else {
            return Ok(Withdrawal::Unknown);
        };
        let access_group: String = row.get(0);
        let withdrawn = Permission {
            permission_id: permission_id.to_string(),
            user_group_id: row.get(1),
            account_group_id: row.get::<_, Option<String>>(2).unwrap_or_default(),
            access_group_id: access_group.clone(),
        };
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
        insert_access_change(
            &mut tx,
            &permission_change(&withdrawn, false, author, at_ns),
        )?;
        tx.commit().map_err(unavailable)?;
        Ok(Withdrawal::Withdrawn)
    }

    fn install_first_admin(
        &self,
        group: &UserGroup,
        permissions: &[Permission],
        author: &Author,
        at_ns: i64,
    ) -> Result<bool> {
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
        for permission in permissions {
            insert_permission(&mut tx, permission)?;
            insert_access_change(&mut tx, &permission_change(permission, true, author, at_ns))?;
        }
        tx.commit().map_err(unavailable)?;
        Ok(true)
    }

    fn access_changes(&self, access_group_id: &str) -> Result<Vec<AccessChangeRecord>> {
        let rows = self
            .conn()?
            .query(
                "SELECT access_group_id, kind, was, became, permission_id, changed_by,
                        through_delegation, changed_at_ns, note
                   FROM config_access_change
                  WHERE access_group_id = $1
                  ORDER BY change_id",
                &[&access_group_id],
            )
            .map_err(unavailable)?;
        rows.iter()
            .map(|row| {
                let code: i16 = row.get(1);
                Ok(AccessChangeRecord {
                    access_group_id: row.get(0),
                    kind: AccessChangeKind::from_code(code).ok_or_else(|| {
                        StoreError::Unavailable(format!("an access change of kind {code}"))
                    })?,
                    was: row.get(2),
                    became: row.get(3),
                    permission_id: row.get(4),
                    by: row.get(5),
                    delegation: row.get(6),
                    at_ns: row.get(7),
                    note: row.get(8),
                })
            })
            .collect()
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
                "INSERT INTO config_known_plugin (plugin_instance_id, roles, last_reported_at_ns)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (plugin_instance_id) DO UPDATE
                    SET roles = excluded.roles,
                        last_reported_at_ns = excluded.last_reported_at_ns",
                &[
                    &plugin.plugin_instance_id,
                    &plugin.roles,
                    &plugin.last_reported_at_ns,
                ],
            )
            .map_err(unavailable)?;
        Ok(())
    }

    fn record_declared_settings(
        &self,
        plugin_instance_id: &str,
        declared: &[SettingDeclaration],
        at_ns: i64,
    ) -> Result<Vec<String>> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        // What it declared before, as far as a held value is concerned: each
        // setting's type and whether it is secret.
        let before: Vec<SettingDeclaration> = tx
            .query(
                "SELECT name, type, secret FROM config_plugin_setting_declaration
                  WHERE plugin_instance_id = $1",
                &[&plugin_instance_id],
            )
            .map_err(unavailable)?
            .iter()
            .map(|row| SettingDeclaration {
                name: row.get(0),
                r#type: i32::from(row.get::<_, i16>(1)),
                secret: row.get(2),
                ..Default::default()
            })
            .collect();
        let held: Vec<String> = tx
            .query(
                "SELECT name FROM config_plugin_setting WHERE plugin_instance_id = $1 FOR UPDATE",
                &[&plugin_instance_id],
            )
            .map_err(unavailable)?
            .iter()
            .map(|row| row.get(0))
            .collect();
        tx.execute(
            "DELETE FROM config_plugin_setting_declaration WHERE plugin_instance_id = $1",
            &[&plugin_instance_id],
        )
        .map_err(unavailable)?;
        for (position, declaration) in declared.iter().enumerate() {
            tx.execute(
                "INSERT INTO config_plugin_setting_declaration
                        (plugin_instance_id, position, name, type, required, secret, description,
                         declared)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                &[
                    &plugin_instance_id,
                    &(position as i32),
                    &declaration.name,
                    &(declaration.r#type as i16),
                    &declaration.required,
                    &declaration.secret,
                    &declaration.description,
                    &declaration.encode_to_vec(),
                ],
            )
            .map_err(unavailable)?;
        }
        let mut cleared = Vec::new();
        for now in declared {
            let earlier = before.iter().find(|d| d.name == now.name);
            if let (true, Some(why)) = (held.contains(&now.name), redeclared(earlier, now)) {
                tx.execute(
                    "DELETE FROM config_plugin_setting WHERE plugin_instance_id = $1 AND name = $2",
                    &[&plugin_instance_id, &now.name],
                )
                .map_err(unavailable)?;
                tx.execute(
                    "INSERT INTO config_plugin_setting_change
                            (plugin_instance_id, name, action, changed_by, changed_at_ns, note)
                     VALUES ($1, $2, $3, '', $4, $5)",
                    &[
                        &plugin_instance_id,
                        &now.name,
                        &ChangeKind::Cleared.code(),
                        &at_ns,
                        &why,
                    ],
                )
                .map_err(unavailable)?;
                cleared.push(now.name.clone());
            }
            if now.secret {
                migrations::redact_values(
                    &mut tx,
                    plugin_instance_id,
                    &now.name,
                    at_ns,
                    REDACTED_BY_REDECLARATION,
                )?;
            }
        }
        tx.commit().map_err(unavailable)?;
        Ok(cleared)
    }

    fn put_plugin_settings(
        &self,
        plugin_instance_id: &str,
        changes: &[SettingChange],
        author: &SettingsAuthor,
        at_ns: i64,
    ) -> Result<()> {
        let by = author.by.as_str();
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        for change in changes {
            let (kind, recorded, secret): (ChangeKind, Option<&str>, bool) = match &change.held {
                None => {
                    tx.execute(
                        "DELETE FROM config_plugin_setting
                          WHERE plugin_instance_id = $1 AND name = $2",
                        &[&plugin_instance_id, &change.name],
                    )
                    .map_err(unavailable)?;
                    (ChangeKind::Cleared, None, false)
                }
                Some(held) => {
                    let (value, sealed) = match held {
                        Held::Plain(value) => (Some(value.as_str()), None),
                        Held::Sealed(sealed) => (None, Some(sealed.as_slice())),
                    };
                    tx.execute(
                        "INSERT INTO config_plugin_setting
                                (plugin_instance_id, name, value, sealed, set_by, set_at_ns)
                         VALUES ($1, $2, $3, $4, $5, $6)
                         ON CONFLICT (plugin_instance_id, name) DO UPDATE
                            SET value = excluded.value, sealed = excluded.sealed,
                                set_by = excluded.set_by, set_at_ns = excluded.set_at_ns",
                        &[
                            &plugin_instance_id,
                            &change.name,
                            &value,
                            &sealed,
                            &by,
                            &at_ns,
                        ],
                    )
                    .map_err(unavailable)?;
                    // The value of a setting that is not secret; a secret's,
                    // never: only that one was set.
                    (ChangeKind::Set, value, sealed.is_some())
                }
            };
            tx.execute(
                "INSERT INTO config_plugin_setting_change
                        (plugin_instance_id, name, action, changed_by, changed_at_ns,
                         value, secret, through_delegation, client_name, note,
                         prepared_call_id, hint)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
                &[
                    &plugin_instance_id,
                    &change.name,
                    &kind.code(),
                    &by,
                    &at_ns,
                    &recorded,
                    &secret,
                    &author.delegation,
                    &author.client,
                    &author.note,
                    &author.prepared_call_id,
                    // A secret's hint, only where one was set.
                    &if secret { change.hint.as_str() } else { "" },
                ],
            )
            .map_err(unavailable)?;
            if secret {
                migrations::redact_values(
                    &mut tx,
                    plugin_instance_id,
                    &change.name,
                    at_ns,
                    REDACTED_BY_SEALING,
                )?;
            }
        }
        tx.commit().map_err(unavailable)
    }

    fn plugin_setting_changes(&self, plugin_instance_id: &str) -> Result<Vec<SettingChangeRecord>> {
        let rows = self
            .conn()?
            .query(
                "SELECT name, action, value, secret, changed_by, through_delegation,
                        changed_at_ns, backfilled, note, client_name, prepared_call_id, hint
                   FROM config_plugin_setting_change
                  WHERE plugin_instance_id = $1
                  ORDER BY change_id",
                &[&plugin_instance_id],
            )
            .map_err(unavailable)?;
        rows.iter()
            .map(|row| {
                let action: i16 = row.get(1);
                Ok(SettingChangeRecord {
                    plugin_instance_id: plugin_instance_id.to_string(),
                    name: row.get(0),
                    kind: ChangeKind::from_code(action).ok_or_else(|| {
                        StoreError::Unavailable(format!(
                            "a settings change records action {action}, which this binary does not know"
                        ))
                    })?,
                    value: row.get(2),
                    secret: row.get(3),
                    by: row.get(4),
                    delegation: row.get(5),
                    client: row.get(9),
                    at_ns: row.get(6),
                    backfilled: row.get(7),
                    note: row.get(8),
                    prepared_call_id: row.get(10),
                    hint: row.get(11),
                })
            })
            .collect()
    }

    fn record_plugin_version_documented(
        &self,
        version: &PluginVersion,
        documentation: &[meridian_domain::v1::DocumentationText],
    ) -> Result<bool> {
        let metadata = version.metadata.clone().unwrap_or_default();
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        let written = tx
            .execute(
                "INSERT INTO config_plugin_version
                        (name, version, roles, interface, sdk_version, image_digest,
                         uploaded_by, uploaded_at_ns, declaration, source_registry, listing,
                         key_status_issued_at_ns)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
                 ON CONFLICT (name, version) DO NOTHING",
                &[
                    &metadata.name,
                    &metadata.version,
                    &metadata.roles,
                    &metadata.interface,
                    &metadata.sdk_version,
                    &version.image_digest,
                    &version.uploaded_by,
                    &version.uploaded_at_ns,
                    &metadata.declaration.as_ref().map(Message::encode_to_vec),
                    &if version.source_registry.is_empty() {
                        "local"
                    } else {
                        version.source_registry.as_str()
                    },
                    &version.listing,
                    &version.key_status_issued_at_ns,
                ],
            )
            .map_err(unavailable)?;
        if written != 1 {
            return Ok(false);
        }
        // The vendor's texts, in the order given, with the version (W8.1,
        // contract v25); never replaced, since the version is not.
        for (ordinal, text) in documentation.iter().enumerate() {
            tx.execute(
                "INSERT INTO config_plugin_documentation (name, version, section, ordinal, markdown)
                 VALUES ($1, $2, $3, $4, $5)",
                &[
                    &metadata.name,
                    &metadata.version,
                    &text.section,
                    &(ordinal as i32),
                    &text.markdown,
                ],
            )
            .map_err(unavailable)?;
        }
        tx.commit().map_err(unavailable)?;
        Ok(true)
    }

    fn plugin_documentation(
        &self,
        name: &str,
        version: &str,
    ) -> Result<Vec<meridian_domain::v1::DocumentationText>> {
        Ok(self
            .conn()?
            .query(
                "SELECT section, markdown FROM config_plugin_documentation
                  WHERE name = $1 AND version = $2 ORDER BY ordinal",
                &[&name, &version],
            )
            .map_err(unavailable)?
            .into_iter()
            .map(|row| meridian_domain::v1::DocumentationText {
                section: row.get(0),
                markdown: row.get(1),
            })
            .collect())
    }

    fn retire_plugin_version(
        &self,
        name: &str,
        version: &str,
        author: &Author,
        at_ns: i64,
    ) -> Result<Option<PluginVersion>> {
        // Written once: a version retired already is left as it is.
        let written = self
            .conn()?
            .execute(
                "UPDATE config_plugin_version
                    SET retired_by = $3, retired_at_ns = $4, retired_note = $5,
                        retired_through_delegation = $6, retired_client_name = $7,
                        retired_prepared_call_id = $8
                  WHERE name = $1 AND version = $2 AND retired_at_ns = 0",
                &[
                    &name,
                    &version,
                    &author.by,
                    &at_ns,
                    &author.note,
                    &author.delegation,
                    &author.client,
                    &author.prepared_call_id,
                ],
            )
            .map_err(unavailable)?;
        if written != 1 {
            return Ok(None);
        }
        Ok(self
            .snapshot()?
            .catalogue
            .versions
            .into_iter()
            .find(|held| {
                held.metadata
                    .as_ref()
                    .is_some_and(|m| m.name == name && m.version == version)
            }))
    }

    fn retirement(&self, name: &str, version: &str) -> Result<Option<Author>> {
        Ok(self
            .conn()?
            .query_opt(
                "SELECT retired_by, retired_through_delegation, retired_client_name,
                        retired_note, retired_prepared_call_id
                   FROM config_plugin_version
                  WHERE name = $1 AND version = $2 AND retired_at_ns <> 0",
                &[&name, &version],
            )
            .map_err(unavailable)?
            .map(|row| Author {
                by: row.get(0),
                delegation: row.get(1),
                client: row.get(2),
                note: row.get(3),
                prepared_call_id: row.get(4),
            }))
    }

    fn begin_launch(&self, launch: &PluginLaunch, note: &str) -> Result<bool> {
        let written = self
            .conn()?
            .execute(
                "INSERT INTO config_plugin_launch
                        (instance_id, name, version, image_digest, roles, launched_by,
                         launched_at_ns, state, live, launched_through_delegation,
                         launched_client_name, launch_note, prepared_call_id)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, 1, $8, $9, $10, $11, $12)
                 ON CONFLICT (instance_id) WHERE state = 1 DO NOTHING",
                &[
                    &launch.instance_id,
                    &launch.name,
                    &launch.version,
                    &launch.image_digest,
                    &launch.roles,
                    &launch.launched_by,
                    &launch.launched_at_ns,
                    &launch.live,
                    &launch.acting_through_delegation,
                    &launch.client_name,
                    &note,
                    &launch.prepared_call_id,
                ],
            )
            .map_err(unavailable)?;
        Ok(written == 1)
    }

    fn end_launch(&self, instance_id: &str, ending: &Ending) -> Result<Option<PluginLaunch>> {
        let state = ending.state as i16;
        let ended = self
            .conn()?
            .query_opt(
                &format!(
                    "UPDATE config_plugin_launch
                        SET state = $2, stopped_by = $3, stopped_at_ns = $4, failure = $5,
                            stopped_through_delegation = $6, stopped_client_name = $7,
                            stop_note = $8, stopped_prepared_call_id = $9
                      WHERE instance_id = $1 AND state = 1
                  RETURNING {LAUNCH_COLUMNS}"
                ),
                &[
                    &instance_id,
                    &state,
                    &ending.by,
                    &ending.at_ns,
                    &ending.failure,
                    &ending.delegation,
                    &ending.client,
                    &ending.note,
                    &ending.prepared_call_id,
                ],
            )
            .map_err(unavailable)?;
        Ok(ended.as_ref().map(launch_from))
    }

    fn launch_notes(&self, instance_id: &str) -> Result<Vec<LaunchNote>> {
        let rows = self
            .conn()?
            .query(
                "SELECT launched_at_ns, act, note, gap, at_ns FROM (
                     SELECT launched_at_ns, 1::smallint AS act, launch_note AS note, false AS gap,
                            launched_at_ns AS at_ns, launch_id, 0::bigint AS gap_id
                       FROM config_plugin_launch WHERE instance_id = $1
                     UNION ALL
                     SELECT launched_at_ns, 2::smallint, stop_note, false, stopped_at_ns,
                            launch_id, 0
                       FROM config_plugin_launch WHERE instance_id = $1 AND state = 2
                     UNION ALL
                     SELECT launched_at_ns, act, note, true, noted_at_ns, launch_id, gap_id
                       FROM config_plugin_launch_gap WHERE instance_id = $1) notes
                  ORDER BY at_ns, launch_id, act, gap_id",
                &[&instance_id],
            )
            .map_err(unavailable)?;
        rows.iter()
            .map(|row| {
                let act: i16 = row.get(1);
                Ok(LaunchNote {
                    instance_id: instance_id.to_string(),
                    launched_at_ns: row.get(0),
                    act: LaunchAct::from_code(act).ok_or_else(|| {
                        StoreError::Unavailable(format!(
                            "a launch's record names act {act}, which this binary does not know"
                        ))
                    })?,
                    note: row.get(2),
                    gap: row.get(3),
                    at_ns: row.get(4),
                })
            })
            .collect()
    }

    fn set_hold(&self, hold: &Hold, note: &str) -> Result<()> {
        let days = i32::try_from(hold.days).map_err(|_| {
            StoreError::Unavailable(format!("a hold of {} days does not fit", hold.days))
        })?;
        self.conn()?
            .execute(
                "INSERT INTO config_hold_change
                        (role, days, write_once, changed_by, through_delegation, changed_at_ns,
                         client_name, note, prepared_call_id)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                &[
                    &hold.role,
                    &days,
                    &hold.write_once,
                    &hold.updated_by,
                    &hold.acting_through_delegation,
                    &hold.updated_at_ns,
                    &hold.client_name,
                    &note,
                    &hold.prepared_call_id,
                ],
            )
            .map_err(unavailable)?;
        Ok(())
    }

    fn hold_changes(&self) -> Result<Vec<(Hold, String)>> {
        Ok(self
            .conn()?
            .query(
                &format!("SELECT {HOLD_COLUMNS} FROM config_hold_change ORDER BY change_id"),
                &[],
            )
            .map_err(unavailable)?
            .iter()
            .map(hold_from)
            .collect())
    }

    fn archive_changes(&self, instance_id: &str) -> Result<Vec<(PluginArchive, String)>> {
        Ok(self
            .conn()?
            .query(
                &format!(
                    "SELECT {ARCHIVE_COLUMNS} FROM config_archive_change
                      WHERE instance_id = $1 ORDER BY change_id"
                ),
                &[&instance_id],
            )
            .map_err(unavailable)?
            .iter()
            .map(archive_from)
            .collect())
    }

    fn put_archive(&self, archive: &PluginArchive, note: &str) -> Result<()> {
        let most = i64::try_from(archive.most_bytes).map_err(|_| {
            StoreError::Unavailable(format!(
                "a bound of {} bytes does not fit",
                archive.most_bytes
            ))
        })?;
        self.conn()?
            .execute(
                "INSERT INTO config_archive_change
                        (instance_id, allowed, most_bytes, changed_by, through_delegation,
                         changed_at_ns, client_name, note, prepared_call_id)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                &[
                    &archive.instance_id,
                    &archive.allowed,
                    &most,
                    &archive.updated_by,
                    &archive.acting_through_delegation,
                    &archive.updated_at_ns,
                    &archive.client_name,
                    &note,
                    &archive.prepared_call_id,
                ],
            )
            .map_err(unavailable)?;
        Ok(())
    }

    fn record_move(&self, instance_id: &str, record: &MoveRecord) -> Result<bool> {
        let moved = record.r#move.clone().unwrap_or_default();
        let count = i64::try_from(moved.record_count).map_err(|_| {
            StoreError::Unavailable(format!("a count of {} does not fit", moved.record_count))
        })?;
        let outcome = i16::try_from(moved.outcome).unwrap_or_default();
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        // One unit's moves in turn, so a retry and its original never both
        // pass the check.
        tx.execute(
            "SELECT pg_advisory_xact_lock(hashtext($1::text || '/' || $2::text || '/' || $3::text))",
            &[&instance_id, &moved.record_kind, &moved.unit],
        )
        .map_err(unavailable)?;
        let latest = tx
            .query_opt(
                "SELECT outcome FROM config_record_move
                  WHERE instance_id = $1 AND record_kind = $2 AND unit = $3
                  ORDER BY move_id DESC LIMIT 1",
                &[&instance_id, &moved.record_kind, &moved.unit],
            )
            .map_err(unavailable)?
            .map(|row| row.get::<_, i16>(0));
        if latest == Some(outcome) {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO config_record_move
                    (instance_id, record_kind, unit, record_count, first_received_ns,
                     last_received_ns, outcome, rule, person, through_delegation, recorded_at_ns,
                    client_name)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
            &[
                &instance_id,
                &moved.record_kind,
                &moved.unit,
                &count,
                &moved.first_received_ns,
                &moved.last_received_ns,
                &outcome,
                &moved.rule,
                &record.person,
                &record.acting_through_delegation,
                &record.at_ns,
                &record.client_name,
            ],
        )
        .map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        Ok(true)
    }

    fn latest_move(
        &self,
        instance_id: &str,
        record_kind: &str,
        unit: &str,
    ) -> Result<Option<MoveRecord>> {
        Ok(self
            .conn()?
            .query_opt(
                &format!(
                    "SELECT {MOVE_COLUMNS} FROM config_record_move
                      WHERE instance_id = $1 AND record_kind = $2 AND unit = $3
                      ORDER BY move_id DESC LIMIT 1"
                ),
                &[&instance_id, &record_kind, &unit],
            )
            .map_err(unavailable)?
            .map(|row| move_of(&row).record))
    }

    fn moves(&self, instance_id: &str, before: i64, limit: usize) -> Result<Vec<RecordedMove>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        Ok(self
            .conn()?
            .query(
                &format!(
                    "SELECT {MOVE_COLUMNS} FROM config_record_move
                      WHERE instance_id = $1 AND ($2::bigint = 0 OR move_id < $2::bigint)
                      ORDER BY move_id DESC LIMIT $3::bigint"
                ),
                &[&instance_id, &before, &limit],
            )
            .map_err(unavailable)?
            .iter()
            .map(move_of)
            .collect())
    }

    fn archived(&self, instance_id: &str) -> Result<Vec<StoredSpan>> {
        let latest: Vec<RecordMoveRequest> = self
            .conn()?
            .query(
                &format!(
                    "SELECT DISTINCT ON (record_kind, unit) {MOVE_COLUMNS}
                       FROM config_record_move WHERE instance_id = $1
                      ORDER BY record_kind, unit, move_id DESC"
                ),
                &[&instance_id],
            )
            .map_err(unavailable)?
            .iter()
            .filter_map(|row| move_of(row).record.r#move)
            .collect();
        Ok(archived_spans(&latest))
    }

    fn record_catalogue(
        &self,
        instance_id: &str,
        catalogue: &meridian_pb::v1::Catalogue,
        at_ns: i64,
    ) -> Result<bool> {
        let bytes = catalogue.encode_to_vec();
        let changed = self
            .conn()?
            .execute(
                "INSERT INTO config_plugin_catalogue (plugin_instance_id, catalogue, recorded_at_ns)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (plugin_instance_id) DO UPDATE
                    SET catalogue = excluded.catalogue, recorded_at_ns = excluded.recorded_at_ns
                  WHERE config_plugin_catalogue.catalogue IS DISTINCT FROM excluded.catalogue",
                &[&instance_id, &bytes, &at_ns],
            )
            .map_err(unavailable)?;
        Ok(changed > 0)
    }

    fn set_dataset_licence(&self, licence: &meridian_pb::v1::DatasetLicence) -> Result<()> {
        self.conn()?
            .execute(
                "INSERT INTO config_dataset_licence_change (dataset, licence, changed_at_ns)
                 VALUES ($1, $2, $3)",
                &[
                    &licence.dataset,
                    &licence.encode_to_vec(),
                    &licence.updated_at_ns,
                ],
            )
            .map_err(unavailable)?;
        Ok(())
    }

    fn set_dataset_entitlement(
        &self,
        entitlement: &meridian_domain::v1::DatasetEntitlement,
    ) -> Result<()> {
        self.conn()?
            .execute(
                "INSERT INTO config_dataset_entitlement_change
                        (dataset, instance, entitlement, changed_at_ns)
                 VALUES ($1, $2, $3, $4)",
                &[
                    &entitlement.dataset,
                    &entitlement.instance,
                    &entitlement.encode_to_vec(),
                    &entitlement.updated_at_ns,
                ],
            )
            .map_err(unavailable)?;
        Ok(())
    }

    fn dataset_changes(
        &self,
        dataset: &str,
    ) -> Result<(
        Vec<meridian_pb::v1::DatasetLicence>,
        Vec<meridian_domain::v1::DatasetEntitlement>,
    )> {
        let mut conn = self.conn()?;
        let mut licences = Vec::new();
        for row in conn
            .query(
                "SELECT licence FROM config_dataset_licence_change WHERE dataset = $1
                  ORDER BY change_id",
                &[&dataset],
            )
            .map_err(unavailable)?
        {
            let bytes: Vec<u8> = row.get(0);
            licences
                .push(meridian_pb::v1::DatasetLicence::decode(&bytes[..]).map_err(unavailable)?);
        }
        let mut entitlements = Vec::new();
        for row in conn
            .query(
                "SELECT entitlement FROM config_dataset_entitlement_change WHERE dataset = $1
                  ORDER BY change_id",
                &[&dataset],
            )
            .map_err(unavailable)?
        {
            let bytes: Vec<u8> = row.get(0);
            entitlements.push(
                meridian_domain::v1::DatasetEntitlement::decode(&bytes[..]).map_err(unavailable)?,
            );
        }
        Ok((licences, entitlements))
    }

    fn close_account_withdrawing_grants(
        &self,
        account: &AccountRecord,
        author: &Author,
        at_ns: i64,
    ) -> Result<Vec<String>> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        let instances: Vec<String> = tx
            .query(
                "SELECT plugin_instance_id FROM config_linkable_account WHERE account_id = $1
                  ORDER BY plugin_instance_id",
                &[&account.account_id],
            )
            .map_err(unavailable)?
            .iter()
            .map(|row| row.get(0))
            .collect();
        let mut withdrawn_from = Vec::new();
        for instance in &instances {
            // Each instance's grants in turn, as a grant's change takes them.
            tx.execute(
                "SELECT pg_advisory_xact_lock(hashtext('config_linkable_account:' || $1))",
                &[instance],
            )
            .map_err(unavailable)?;
            let removed = tx
                .execute(
                    "DELETE FROM config_linkable_account
                      WHERE plugin_instance_id = $1 AND account_id = $2",
                    &[instance, &account.account_id],
                )
                .map_err(unavailable)?;
            if removed != 1 {
                continue;
            }
            let withdrawn = vec![account.account_id.clone()];
            let granted: Vec<String> = Vec::new();
            tx.execute(
                "INSERT INTO config_linkable_accounts_change
                        (plugin_instance_id, granted, withdrawn, changed_by, through_delegation,
                         client_name, changed_at_ns, note)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                &[
                    instance,
                    &granted,
                    &withdrawn,
                    &author.by,
                    &author.delegation,
                    &author.client,
                    &at_ns,
                    &author.note,
                ],
            )
            .map_err(unavailable)?;
            withdrawn_from.push(instance.clone());
        }
        tx.execute(
            "UPDATE config_account SET state = $2 WHERE account_id = $1",
            &[&account.account_id, &(account.state as i16)],
        )
        .map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        Ok(withdrawn_from)
    }

    fn set_linkable_accounts(
        &self,
        plugin_instance_id: &str,
        grant: &[String],
        withdraw: &[String],
        author: &Author,
        at_ns: i64,
    ) -> Result<LinkableAccounts> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        // One instance's grants in turn, so two changes never interleave.
        tx.execute(
            "SELECT pg_advisory_xact_lock(hashtext('config_linkable_account:' || $1))",
            &[&plugin_instance_id],
        )
        .map_err(unavailable)?;
        let mut granted = Vec::new();
        for account in grant {
            let added = tx
                .execute(
                    "INSERT INTO config_linkable_account (plugin_instance_id, account_id)
                     VALUES ($1, $2) ON CONFLICT DO NOTHING",
                    &[&plugin_instance_id, account],
                )
                .map_err(unavailable)?;
            if added == 1 {
                granted.push(account.clone());
            }
        }
        let mut withdrawn = Vec::new();
        for account in withdraw {
            let removed = tx
                .execute(
                    "DELETE FROM config_linkable_account
                      WHERE plugin_instance_id = $1 AND account_id = $2",
                    &[&plugin_instance_id, account],
                )
                .map_err(unavailable)?;
            if removed == 1 {
                withdrawn.push(account.clone());
            }
        }
        if !granted.is_empty() || !withdrawn.is_empty() {
            tx.execute(
                "INSERT INTO config_linkable_accounts_change
                        (plugin_instance_id, granted, withdrawn, changed_by, through_delegation,
                         client_name, changed_at_ns, note)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                &[
                    &plugin_instance_id,
                    &granted,
                    &withdrawn,
                    &author.by,
                    &author.delegation,
                    &author.client,
                    &at_ns,
                    &author.note,
                ],
            )
            .map_err(unavailable)?;
        }
        let account_ids: Vec<String> = tx
            .query(
                "SELECT account_id FROM config_linkable_account WHERE plugin_instance_id = $1
                  ORDER BY account_id",
                &[&plugin_instance_id],
            )
            .map_err(unavailable)?
            .iter()
            .map(|row| row.get(0))
            .collect();
        let latest = tx
            .query_opt(
                "SELECT changed_by, changed_at_ns, through_delegation, client_name
                   FROM config_linkable_accounts_change WHERE plugin_instance_id = $1
                  ORDER BY change_id DESC LIMIT 1",
                &[&plugin_instance_id],
            )
            .map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        Ok(LinkableAccounts {
            plugin_instance_id: plugin_instance_id.to_string(),
            account_ids,
            updated_by: latest.as_ref().map(|r| r.get(0)).unwrap_or_default(),
            updated_at_ns: latest.as_ref().map(|r| r.get(1)).unwrap_or_default(),
            acting_through_delegation: latest.as_ref().map(|r| r.get(2)).unwrap_or_default(),
            client_name: latest.as_ref().map(|r| r.get(3)).unwrap_or_default(),
        })
    }

    fn linkable_accounts_changes(
        &self,
        plugin_instance_id: &str,
    ) -> Result<Vec<LinkableAccountsChange>> {
        Ok(self
            .conn()?
            .query(
                "SELECT granted, withdrawn, changed_by, through_delegation, client_name,
                        changed_at_ns, note
                   FROM config_linkable_accounts_change WHERE plugin_instance_id = $1
                  ORDER BY change_id",
                &[&plugin_instance_id],
            )
            .map_err(unavailable)?
            .iter()
            .map(|row| LinkableAccountsChange {
                plugin_instance_id: plugin_instance_id.to_string(),
                granted: row.get(0),
                withdrawn: row.get(1),
                by: row.get(2),
                delegation: row.get(3),
                client: row.get(4),
                at_ns: row.get(5),
                note: row.get(6),
            })
            .collect())
    }

    fn set_connectivity_link(
        &self,
        link: &meridian_pb::v1::ConnectivityLink,
        author: &Author,
        at_ns: i64,
    ) -> Result<meridian_pb::v1::ConnectivityLink> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        let instance = &link.connectivity_instance_id;
        // One instance's link in turn, so two changes never interleave.
        tx.execute(
            "SELECT pg_advisory_xact_lock(hashtext('config_connectivity_link:' || $1))",
            &[instance],
        )
        .map_err(unavailable)?;
        let from: Option<Vec<u8>> = tx
            .query_opt(
                "SELECT link FROM config_connectivity_link WHERE connectivity_instance_id = $1",
                &[instance],
            )
            .map_err(unavailable)?
            .map(|row| row.get(0));
        let standing = match &from {
            Some(bytes) => {
                meridian_pb::v1::ConnectivityLink::decode(&bytes[..]).map_err(unavailable)?
            }
            None => meridian_pb::v1::ConnectivityLink {
                connectivity_instance_id: instance.clone(),
                ..Default::default()
            },
        };
        if standing == *link {
            tx.commit().map_err(unavailable)?;
            return Ok(standing);
        }
        let to = link.encode_to_vec();
        tx.execute(
            "INSERT INTO config_connectivity_link (connectivity_instance_id, link)
             VALUES ($1, $2)
             ON CONFLICT (connectivity_instance_id) DO UPDATE SET link = excluded.link",
            &[instance, &to],
        )
        .map_err(unavailable)?;
        tx.execute(
            "INSERT INTO config_connectivity_link_change
                    (connectivity_instance_id, from_link, to_link, changed_by,
                     through_delegation, client_name, changed_at_ns, note)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            &[
                instance,
                &from,
                &to,
                &author.by,
                &author.delegation,
                &author.client,
                &at_ns,
                &author.note,
            ],
        )
        .map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        Ok(link.clone())
    }

    fn connectivity_link_changes(&self, instance_id: &str) -> Result<Vec<ConnectivityLinkChange>> {
        self.conn()?
            .query(
                "SELECT from_link, to_link, changed_by, through_delegation, client_name,
                        changed_at_ns, note
                   FROM config_connectivity_link_change WHERE connectivity_instance_id = $1
                  ORDER BY change_id",
                &[&instance_id],
            )
            .map_err(unavailable)?
            .iter()
            .map(|row| {
                let from: Option<Vec<u8>> = row.get(0);
                let to: Vec<u8> = row.get(1);
                Ok(ConnectivityLinkChange {
                    connectivity_instance_id: instance_id.to_string(),
                    from_link: from
                        .map(|bytes| meridian_pb::v1::ConnectivityLink::decode(&bytes[..]))
                        .transpose()
                        .map_err(unavailable)?,
                    to_link: meridian_pb::v1::ConnectivityLink::decode(&to[..])
                        .map_err(unavailable)?,
                    by: row.get(2),
                    delegation: row.get(3),
                    client: row.get(4),
                    at_ns: row.get(5),
                    note: row.get(6),
                })
            })
            .collect()
    }

    fn map_venue_account(
        &self,
        ems_instance_id: &str,
        from: Option<&meridian_pb::v1::VenueAccountMapping>,
        to: Option<&meridian_pb::v1::VenueAccountMapping>,
        author: &Author,
        at_ns: i64,
    ) -> Result<()> {
        let Some((instance, code)) = to.or(from).map(|held| {
            (
                held.connectivity_instance_id.clone(),
                held.venue_account_code.clone(),
            )
        }) else {
            return Ok(());
        };
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        tx.execute(
            "SELECT pg_advisory_xact_lock(hashtext('config_venue_account_mapping:' || $1))",
            &[&instance],
        )
        .map_err(unavailable)?;
        let standing = tx
            .query_opt(
                "SELECT ems_instance_id, account_id FROM config_venue_account_mapping
                  WHERE connectivity_instance_id = $1 AND venue_account_code = $2",
                &[&instance, &code],
            )
            .map_err(unavailable)?
            .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)));
        let unchanged = match (&standing, to) {
            (None, None) => true,
            (Some((ems, account)), Some(mapping)) => {
                ems == ems_instance_id && account == &mapping.account_id
            }
            _ => false,
        };
        if unchanged {
            tx.commit().map_err(unavailable)?;
            return Ok(());
        }
        match to {
            Some(mapping) => tx.execute(
                "INSERT INTO config_venue_account_mapping
                        (connectivity_instance_id, venue_account_code, ems_instance_id, account_id)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (connectivity_instance_id, venue_account_code)
                 DO UPDATE SET ems_instance_id = excluded.ems_instance_id,
                               account_id = excluded.account_id",
                &[&instance, &code, &ems_instance_id, &mapping.account_id],
            ),
            None => tx.execute(
                "DELETE FROM config_venue_account_mapping
                  WHERE connectivity_instance_id = $1 AND venue_account_code = $2",
                &[&instance, &code],
            ),
        }
        .map_err(unavailable)?;
        let from_account = standing.map(|(_, account)| account).unwrap_or_default();
        let to_account = to
            .map(|mapping| mapping.account_id.clone())
            .unwrap_or_default();
        tx.execute(
            "INSERT INTO config_venue_account_map_change
                    (ems_instance_id, connectivity_instance_id, venue_account_code,
                     from_account_id, to_account_id, changed_by, through_delegation,
                     client_name, changed_at_ns, note)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            &[
                &ems_instance_id,
                &instance,
                &code,
                &from_account,
                &to_account,
                &author.by,
                &author.delegation,
                &author.client,
                &at_ns,
                &author.note,
            ],
        )
        .map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        Ok(())
    }

    fn venue_account_map_changes(
        &self,
        ems_instance_id: &str,
    ) -> Result<Vec<VenueAccountMapChange>> {
        Ok(self
            .conn()?
            .query(
                "SELECT connectivity_instance_id, venue_account_code, from_account_id,
                        to_account_id, changed_by, through_delegation, client_name,
                        changed_at_ns, note
                   FROM config_venue_account_map_change WHERE ems_instance_id = $1
                  ORDER BY change_id",
                &[&ems_instance_id],
            )
            .map_err(unavailable)?
            .iter()
            .map(|row| {
                let mapping = |account: String| {
                    (!account.is_empty()).then(|| meridian_pb::v1::VenueAccountMapping {
                        connectivity_instance_id: row.get(0),
                        venue_account_code: row.get(1),
                        account_id: account,
                    })
                };
                VenueAccountMapChange {
                    ems_instance_id: ems_instance_id.to_string(),
                    from_mapping: mapping(row.get(2)),
                    to_mapping: mapping(row.get(3)),
                    by: row.get(4),
                    delegation: row.get(5),
                    client: row.get(6),
                    at_ns: row.get(7),
                    note: row.get(8),
                }
            })
            .collect())
    }

    fn keep_venue_catalogue(
        &self,
        catalogue: &meridian_domain::v1::VenueCatalogue,
        at_ns: i64,
    ) -> Result<bool> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        tx.execute(
            "SELECT pg_advisory_xact_lock(hashtext('config_venue_catalogue:' || $1))",
            &[&catalogue.connectivity_instance_id],
        )
        .map_err(unavailable)?;
        let bytes = catalogue.encode_to_vec();
        let latest: Option<Vec<u8>> = tx
            .query_opt(
                "SELECT catalogue FROM config_venue_catalogue
                  WHERE connectivity_instance_id = $1 AND venue_account_code = $2
                  ORDER BY catalogue_id DESC LIMIT 1",
                &[
                    &catalogue.connectivity_instance_id,
                    &catalogue.venue_account_code,
                ],
            )
            .map_err(unavailable)?
            .map(|row| row.get(0));
        if latest.as_deref() == Some(bytes.as_slice()) {
            tx.commit().map_err(unavailable)?;
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO config_venue_catalogue
                    (connectivity_instance_id, venue_account_code, catalogue, heard_at_ns)
             VALUES ($1, $2, $3, $4)",
            &[
                &catalogue.connectivity_instance_id,
                &catalogue.venue_account_code,
                &bytes,
                &at_ns,
            ],
        )
        .map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        Ok(true)
    }

    fn venue_catalogue(
        &self,
        instance_id: &str,
        code: &str,
    ) -> Result<Option<meridian_domain::v1::VenueCatalogue>> {
        self.conn()?
            .query_opt(
                "SELECT catalogue FROM config_venue_catalogue
                  WHERE connectivity_instance_id = $1 AND venue_account_code = $2
                  ORDER BY catalogue_id DESC LIMIT 1",
                &[&instance_id, &code],
            )
            .map_err(unavailable)?
            .map(|row| {
                let bytes: Vec<u8> = row.get(0);
                meridian_domain::v1::VenueCatalogue::decode(&bytes[..]).map_err(unavailable)
            })
            .transpose()
    }

    fn set_deployment_time_zone(&self, zone: &str, author: &Author, at_ns: i64) -> Result<()> {
        self.conn()?
            .execute(
                "INSERT INTO config_time_zone_change
                        (deployment_time_zone, changed_by, through_delegation, client_name,
                         prepared_call_id, changed_at_ns, note)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
                &[
                    &zone,
                    &author.by,
                    &author.delegation,
                    &author.client,
                    &author.prepared_call_id,
                    &at_ns,
                    &author.note,
                ],
            )
            .map_err(unavailable)?;
        Ok(())
    }

    fn set_model_routing(
        &self,
        model_id: &str,
        implementation: &str,
        author: &Author,
        at_ns: i64,
    ) -> Result<()> {
        self.conn()?
            .execute(
                "INSERT INTO config_model_routing_change
                        (model_id, implementation, changed_by, through_delegation, client_name,
                         prepared_call_id, changed_at_ns, note)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                &[
                    &model_id,
                    &implementation,
                    &author.by,
                    &author.delegation,
                    &author.client,
                    &author.prepared_call_id,
                    &at_ns,
                    &author.note,
                ],
            )
            .map_err(unavailable)?;
        Ok(())
    }

    fn model_routing_changes(&self) -> Result<Vec<crate::store::ModelRoutingChange>> {
        Ok(self
            .conn()?
            .query(
                "SELECT model_id, implementation, changed_at_ns, changed_by, note
                   FROM config_model_routing_change ORDER BY change_id",
                &[],
            )
            .map_err(unavailable)?
            .into_iter()
            .map(|row| crate::store::ModelRoutingChange {
                model_id: row.get(0),
                implementation: row.get(1),
                changed_at_ns: row.get(2),
                changed_by: row.get(3),
                note: row.get(4),
            })
            .collect())
    }

    fn deployment_time_zone(&self) -> Result<Option<crate::store::TimeZoneChange>> {
        Ok(self
            .conn()?
            .query_opt(
                "SELECT deployment_time_zone, changed_at_ns, changed_by FROM config_time_zone_change
                  ORDER BY change_id DESC LIMIT 1",
                &[],
            )
            .map_err(unavailable)?
            .map(|row| crate::store::TimeZoneChange {
                zone: row.get(0),
                changed_at_ns: row.get(1),
                changed_by: row.get(2),
            }))
    }

    fn set_fx_pivot_currencies(
        &self,
        pivots: &[String],
        author: &Author,
        at_ns: i64,
    ) -> Result<()> {
        self.conn()?
            .execute(
                "INSERT INTO config_fx_pivot_change
                        (fx_pivot_currencies, changed_by, through_delegation, client_name,
                         prepared_call_id, changed_at_ns, note)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
                &[
                    &pivots,
                    &author.by,
                    &author.delegation,
                    &author.client,
                    &author.prepared_call_id,
                    &at_ns,
                    &author.note,
                ],
            )
            .map_err(unavailable)?;
        Ok(())
    }

    fn fx_pivot_currencies(&self) -> Result<Option<crate::store::FxPivotChange>> {
        Ok(self
            .conn()?
            .query_opt(
                "SELECT fx_pivot_currencies, changed_at_ns, changed_by FROM config_fx_pivot_change
                  ORDER BY change_id DESC LIMIT 1",
                &[],
            )
            .map_err(unavailable)?
            .map(|row| crate::store::FxPivotChange {
                fx_pivot_currencies: row.get(0),
                changed_at_ns: row.get(1),
                changed_by: row.get(2),
            }))
    }

    fn set_setting(&self, setting: &str, value: &[u8], author: &Author, at_ns: i64) -> Result<()> {
        self.conn()?
            .execute(
                "INSERT INTO config_setting_change
                        (setting, value, changed_by, through_delegation, client_name,
                         prepared_call_id, changed_at_ns, note)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                &[
                    &setting,
                    &value,
                    &author.by,
                    &author.delegation,
                    &author.client,
                    &author.prepared_call_id,
                    &at_ns,
                    &author.note,
                ],
            )
            .map_err(unavailable)?;
        Ok(())
    }

    fn setting(&self, setting: &str) -> Result<Option<crate::store::DeploymentSettingChange>> {
        Ok(self
            .conn()?
            .query_opt(
                "SELECT value, changed_at_ns, changed_by FROM config_setting_change
                  WHERE setting = $1 ORDER BY change_id DESC LIMIT 1",
                &[&setting],
            )
            .map_err(unavailable)?
            .map(|row| crate::store::DeploymentSettingChange {
                value: row.get(0),
                changed_at_ns: row.get(1),
                changed_by: row.get(2),
            }))
    }

    fn set_reports_usage(&self, reports_usage: bool, author: &Author, at_ns: i64) -> Result<()> {
        self.conn()?
            .execute(
                "INSERT INTO config_reports_usage_change
                        (reports_usage, changed_by, through_delegation, client_name,
                         prepared_call_id, changed_at_ns, note)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
                &[
                    &reports_usage,
                    &author.by,
                    &author.delegation,
                    &author.client,
                    &author.prepared_call_id,
                    &at_ns,
                    &author.note,
                ],
            )
            .map_err(unavailable)?;
        Ok(())
    }

    fn reports_usage(&self) -> Result<bool> {
        Ok(self
            .conn()?
            .query_opt(
                "SELECT reports_usage FROM config_reports_usage_change
                  ORDER BY change_id DESC LIMIT 1",
                &[],
            )
            .map_err(unavailable)?
            .is_none_or(|row| row.get::<_, bool>(0)))
    }

    fn set_licensed_schemes(&self, schemes: &[String], author: &Author, at_ns: i64) -> Result<()> {
        self.conn()?
            .execute(
                "INSERT INTO config_licensed_schemes_change
                        (licensed_schemes, changed_by, through_delegation, client_name,
                         prepared_call_id, changed_at_ns, note)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
                &[
                    &schemes,
                    &author.by,
                    &author.delegation,
                    &author.client,
                    &author.prepared_call_id,
                    &at_ns,
                    &author.note,
                ],
            )
            .map_err(unavailable)?;
        Ok(())
    }

    fn licensed_schemes(&self) -> Result<Vec<String>> {
        Ok(self
            .conn()?
            .query_opt(
                "SELECT licensed_schemes FROM config_licensed_schemes_change
                  ORDER BY change_id DESC LIMIT 1",
                &[],
            )
            .map_err(unavailable)?
            .map(|row| row.get::<_, Vec<String>>(0))
            .unwrap_or_default())
    }

    fn record_usage_report(&self, report: &meridian_domain::v1::SentUsageReport) -> Result<()> {
        use prost::Message;
        self.conn()?
            .execute(
                "INSERT INTO config_usage_report (sent_at_ns, report) VALUES ($1, $2)",
                &[&report.sent_at_ns, &report.encode_to_vec()],
            )
            .map_err(unavailable)?;
        Ok(())
    }

    fn usage_reports(&self, limit: usize) -> Result<Vec<meridian_domain::v1::SentUsageReport>> {
        use prost::Message;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.conn()?
            .query(
                "SELECT report FROM config_usage_report ORDER BY report_id DESC LIMIT $1",
                &[&limit],
            )
            .map_err(unavailable)?
            .iter()
            .map(|row| {
                let bytes: Vec<u8> = row.get(0);
                meridian_domain::v1::SentUsageReport::decode(&bytes[..]).map_err(unavailable)
            })
            .collect()
    }

    fn registry_catalogue(&self) -> Result<Option<crate::store::HeldCatalogue>> {
        use prost::Message;
        let Some(row) = self
            .conn()?
            .query_opt(
                "SELECT listings, sequence, key_status, pulled_at_ns, pull_status
                   FROM config_registry_catalogue WHERE id = 1",
                &[],
            )
            .map_err(unavailable)?
        else {
            return Ok(None);
        };
        let listings: Vec<u8> = row.get(0);
        let held =
            meridian_domain::v1::PullCatalogueReply::decode(&listings[..]).map_err(unavailable)?;
        Ok(Some(crate::store::HeldCatalogue {
            listings: held.listings,
            sequence: u64::try_from(row.get::<_, i64>(1)).unwrap_or_default(),
            key_status: row.get(2),
            pulled_at_ns: row.get(3),
            pull_status: row.get(4),
            // The packs kept in the same message (contract v27, field 5).
            packs: held.packs,
        }))
    }

    fn put_registry_catalogue(&self, held: &crate::store::HeldCatalogue) -> Result<()> {
        use prost::Message;
        let listings = meridian_domain::v1::PullCatalogueReply {
            listings: held.listings.clone(),
            packs: held.packs.clone(),
            ..Default::default()
        }
        .encode_to_vec();
        self.conn()?
            .execute(
                "INSERT INTO config_registry_catalogue
                        (id, listings, sequence, key_status, pulled_at_ns, pull_status)
                 VALUES (1, $1, $2, $3, $4, $5)
                 ON CONFLICT (id) DO UPDATE SET listings = $1, sequence = $2, key_status = $3,
                        pulled_at_ns = $4, pull_status = $5",
                &[
                    &listings,
                    &i64::try_from(held.sequence).unwrap_or(i64::MAX),
                    &held.key_status,
                    &held.pulled_at_ns,
                    &held.pull_status,
                ],
            )
            .map_err(unavailable)?;
        Ok(())
    }

    fn record_listed_version(
        &self,
        version: &PluginVersion,
        documentation: &[meridian_domain::v1::DocumentationText],
        listing_statement: &str,
        by: &Author,
        pack: &str,
    ) -> Result<bool> {
        if !self.record_plugin_version_documented(version, documentation)? {
            return Ok(false);
        }
        let metadata = version.metadata.clone().unwrap_or_default();
        self.conn()?
            .execute(
                "UPDATE config_plugin_version
                    SET listing_statement = $3, uploaded_through_delegation = $4,
                        uploaded_client_name = $5, uploaded_prepared_call_id = $6,
                        uploaded_note = $7, installed_by_pack = $8
                  WHERE name = $1 AND version = $2",
                &[
                    &metadata.name,
                    &metadata.version,
                    &listing_statement,
                    &by.delegation,
                    &by.client,
                    &by.prepared_call_id,
                    &by.note,
                    &pack,
                ],
            )
            .map_err(unavailable)?;
        Ok(true)
    }

    fn install_pack(&self, name: &str, version: &str) -> Result<String> {
        Ok(self
            .conn()?
            .query_opt(
                "SELECT installed_by_pack FROM config_plugin_version
                  WHERE name = $1 AND version = $2",
                &[&name, &version],
            )
            .map_err(unavailable)?
            .map(|row| row.get(0))
            .unwrap_or_default())
    }

    fn record_pack_change(&self, change: &crate::store::PackChange) -> Result<()> {
        use prost::Message;
        let members = meridian_domain::v1::PackStatus {
            members: change.members.clone(),
            ..Default::default()
        }
        .encode_to_vec();
        self.conn()?
            .execute(
                "INSERT INTO config_pack_change
                        (pack, kind, revision, held, members, changed_by, through_delegation,
                         client_name, prepared_call_id, changed_at_ns, note)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
                &[
                    &change.pack,
                    &change.kind.name(),
                    &i64::try_from(change.revision).unwrap_or(i64::MAX),
                    &change.held.encode_to_vec(),
                    &members,
                    &change.by.by,
                    &change.by.delegation,
                    &change.by.client,
                    &change.by.prepared_call_id,
                    &change.at_ns,
                    &change.by.note,
                ],
            )
            .map_err(unavailable)?;
        Ok(())
    }

    fn pack_changes(&self, pack: &str) -> Result<Vec<crate::store::PackChange>> {
        use prost::Message;
        self.conn()?
            .query(
                "SELECT pack, kind, revision, held, members, changed_by, through_delegation,
                        client_name, prepared_call_id, changed_at_ns, note
                   FROM config_pack_change
                  WHERE $1 = '' OR pack = $1
                  ORDER BY change_id",
                &[&pack],
            )
            .map_err(unavailable)?
            .iter()
            .map(|row| {
                let kind: String = row.get(1);
                let held: Vec<u8> = row.get(3);
                let members: Vec<u8> = row.get(4);
                Ok(crate::store::PackChange {
                    pack: row.get(0),
                    kind: crate::store::PackChangeKind::from_name(&kind).ok_or_else(|| {
                        StoreError::Unavailable(format!("a pack change of kind {kind}"))
                    })?,
                    revision: u64::try_from(row.get::<_, i64>(2)).unwrap_or_default(),
                    held: meridian_domain::v1::PluginPack::decode(&held[..])
                        .map_err(unavailable)?,
                    members: meridian_domain::v1::PackStatus::decode(&members[..])
                        .map_err(unavailable)?
                        .members,
                    by: Author {
                        by: row.get(5),
                        delegation: row.get(6),
                        client: row.get(7),
                        prepared_call_id: row.get(8),
                        note: row.get(10),
                    },
                    at_ns: row.get(9),
                })
            })
            .collect()
    }

    fn installation(&self, name: &str, version: &str) -> Result<Option<Author>> {
        Ok(self
            .conn()?
            .query_opt(
                "SELECT uploaded_by, uploaded_through_delegation, uploaded_client_name,
                        uploaded_note, uploaded_prepared_call_id
                   FROM config_plugin_version
                  WHERE name = $1 AND version = $2 AND listing <> ''",
                &[&name, &version],
            )
            .map_err(unavailable)?
            .map(|row| Author {
                by: row.get(0),
                delegation: row.get(1),
                client: row.get(2),
                note: row.get(3),
                prepared_call_id: row.get(4),
            }))
    }

    fn listing_statement(&self, name: &str, version: &str) -> Result<String> {
        Ok(self
            .conn()?
            .query_opt(
                "SELECT listing_statement FROM config_plugin_version
                  WHERE name = $1 AND version = $2",
                &[&name, &version],
            )
            .map_err(unavailable)?
            .map(|row| row.get(0))
            .unwrap_or_default())
    }

    fn acknowledge_pull(
        &self,
        instance_id: &str,
        acknowledgement: &meridian_domain::v1::PullAcknowledgement,
    ) -> Result<Option<PluginLaunch>> {
        use prost::Message;
        let row = self
            .conn()?
            .query_opt(
                &format!(
                    "UPDATE config_plugin_launch SET pull_acknowledgement = $2
                      WHERE instance_id = $1 AND state = 1
                  RETURNING {LAUNCH_COLUMNS}"
                ),
                &[&instance_id, &acknowledgement.encode_to_vec()],
            )
            .map_err(unavailable)?;
        Ok(row.as_ref().map(launch_from))
    }

    fn set_reports_plugin_usage(&self, on: bool, author: &Author, at_ns: i64) -> Result<()> {
        self.conn()?
            .execute(
                "INSERT INTO config_reports_plugin_usage_change
                        (reports_plugin_usage, changed_by, through_delegation, client_name,
                         prepared_call_id, changed_at_ns, note)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
                &[
                    &on,
                    &author.by,
                    &author.delegation,
                    &author.client,
                    &author.prepared_call_id,
                    &at_ns,
                    &author.note,
                ],
            )
            .map_err(unavailable)?;
        Ok(())
    }

    fn reports_plugin_usage(&self) -> Result<bool> {
        Ok(self
            .conn()?
            .query_opt(
                "SELECT reports_plugin_usage FROM config_reports_plugin_usage_change
                  ORDER BY change_id DESC LIMIT 1",
                &[],
            )
            .map_err(unavailable)?
            .is_none_or(|row| row.get::<_, bool>(0)))
    }

    fn record_plugin_usage_report(
        &self,
        report: &meridian_domain::v1::SentPluginUsageReport,
    ) -> Result<()> {
        use prost::Message;
        self.conn()?
            .execute(
                "INSERT INTO config_plugin_usage_report (sent_at_ns, report) VALUES ($1, $2)",
                &[&report.sent_at_ns, &report.encode_to_vec()],
            )
            .map_err(unavailable)?;
        Ok(())
    }

    fn plugin_usage_reports(
        &self,
        limit: usize,
    ) -> Result<Vec<meridian_domain::v1::SentPluginUsageReport>> {
        use prost::Message;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.conn()?
            .query(
                "SELECT report FROM config_plugin_usage_report ORDER BY report_id DESC LIMIT $1",
                &[&limit],
            )
            .map_err(unavailable)?
            .iter()
            .map(|row| {
                let bytes: Vec<u8> = row.get(0);
                meridian_domain::v1::SentPluginUsageReport::decode(&bytes[..]).map_err(unavailable)
            })
            .collect()
    }
}

/// An account's optional text as its column holds it: NULL for none, so an
/// account with none of the four reads as it did before they existed.
fn unless_empty(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

const LAUNCH_COLUMNS: &str = "instance_id, name, version, image_digest, roles, \
     launched_by, launched_at_ns, state, stopped_by, stopped_at_ns, failure, live, \
     launched_through_delegation, launched_client_name, stopped_through_delegation, \
     stopped_client_name, launch_note, stop_note, prepared_call_id, stopped_prepared_call_id, \
     pull_acknowledgement";

/// A hold change's columns, in the order [`hold_from`] reads them.
const HOLD_COLUMNS: &str = "role, days, write_once, changed_by, changed_at_ns, \
     through_delegation, client_name, note, prepared_call_id";

/// A hold, its change's note on it (contract v17), and the note.
fn hold_from(row: &postgres::Row) -> (Hold, String) {
    (
        Hold {
            role: row.get(0),
            days: u32::try_from(row.get::<_, i32>(1)).unwrap_or_default(),
            write_once: row.get(2),
            updated_by: row.get(3),
            updated_at_ns: row.get(4),
            acting_through_delegation: row.get(5),
            client_name: row.get(6),
            note: row.get(7),
            prepared_call_id: row.get(8),
        },
        row.get(7),
    )
}

/// An archive change's columns, in the order [`archive_from`] reads them.
const ARCHIVE_COLUMNS: &str = "instance_id, allowed, most_bytes, changed_by, changed_at_ns, \
     through_delegation, client_name, note, prepared_call_id";

/// An archive, its change's note on it (contract v17), and the note.
fn archive_from(row: &postgres::Row) -> (PluginArchive, String) {
    (
        PluginArchive {
            instance_id: row.get(0),
            allowed: row.get(1),
            most_bytes: u64::try_from(row.get::<_, i64>(2)).unwrap_or_default(),
            updated_by: row.get(3),
            updated_at_ns: row.get(4),
            acting_through_delegation: row.get(5),
            client_name: row.get(6),
            note: row.get(7),
            prepared_call_id: row.get(8),
        },
        row.get(7),
    )
}

fn launch_from(row: &postgres::Row) -> PluginLaunch {
    PluginLaunch {
        instance_id: row.get(0),
        name: row.get(1),
        version: row.get(2),
        image_digest: row.get(3),
        roles: row.get(4),
        launched_by: row.get(5),
        launched_at_ns: row.get(6),
        state: i32::from(row.get::<_, i16>(7)),
        stopped_by: row.get(8),
        stopped_at_ns: row.get(9),
        failure: row.get(10),
        live: row.get(11),
        acting_through_delegation: row.get(12),
        client_name: row.get(13),
        stopped_through_delegation: row.get(14),
        stopped_client_name: row.get(15),
        note: row.get(16),
        stopped_note: row.get(17),
        prepared_call_id: row.get(18),
        stopped_prepared_call_id: row.get(19),
        // Kept on the launch when a deployment admin keeps an instance whose
        // version was pulled (W8.12, contract v25).
        pull_acknowledgement: row.get::<_, Option<Vec<u8>>>(20).and_then(|bytes| {
            <meridian_domain::v1::PullAcknowledgement as prost::Message>::decode(&bytes[..]).ok()
        }),
    }
}

/// One access group as it stands, its entries in order; None when there is
/// none by that identifier.
pub(crate) fn read_access_group(
    client: &mut impl postgres::GenericClient,
    access_group_id: &str,
) -> Result<Option<AccessGroup>> {
    let Some(group) = client
        .query_opt(
            "SELECT name, built_in FROM config_access_group WHERE access_group_id = $1",
            &[&access_group_id],
        )
        .map_err(unavailable)?
    else {
        return Ok(None);
    };
    let entries = client
        .query(
            "SELECT plugin_instance_id, level, role FROM config_access_entry
              WHERE access_group_id = $1 ORDER BY position",
            &[&access_group_id],
        )
        .map_err(unavailable)?
        .iter()
        .map(|entry| AccessEntry {
            plugin_instance_id: entry.get(0),
            level: i32::from(entry.get::<_, i16>(1)),
            role: entry.get(2),
        })
        .collect();
    Ok(Some(AccessGroup {
        access_group_id: access_group_id.to_string(),
        name: group.get(0),
        entries,
        built_in: group.get(1),
    }))
}

/// One access change's own record (decisions/031).
pub(crate) fn insert_access_change(
    client: &mut impl postgres::GenericClient,
    record: &AccessChangeRecord,
) -> Result<()> {
    client
        .execute(
            "INSERT INTO config_access_change
                    (access_group_id, kind, was, became, permission_id, changed_by,
                     through_delegation, changed_at_ns, note)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            &[
                &record.access_group_id,
                &record.kind.code(),
                &record.was,
                &record.became,
                &record.permission_id,
                &record.by,
                &record.delegation,
                &record.at_ns,
                &record.note,
            ],
        )
        .map_err(unavailable)?;
    Ok(())
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

/// A move's columns, in the order [`move_of`] reads them.
const MOVE_COLUMNS: &str = "move_id, instance_id, record_kind, unit, record_count, \
     first_received_ns, last_received_ns, outcome, rule, person, through_delegation, recorded_at_ns, \
     client_name";

fn move_of(row: &postgres::Row) -> RecordedMove {
    RecordedMove {
        move_id: row.get(0),
        instance_id: row.get(1),
        record: MoveRecord {
            r#move: Some(RecordMoveRequest {
                record_kind: row.get(2),
                unit: row.get(3),
                record_count: u64::try_from(row.get::<_, i64>(4)).unwrap_or_default(),
                first_received_ns: row.get(5),
                last_received_ns: row.get(6),
                outcome: i32::from(row.get::<_, i16>(7)),
                rule: row.get(8),
            }),
            person: row.get(9),
            at_ns: row.get(11),
            acting_through_delegation: row.get(10),
            client_name: row.get(12),
        },
    }
}

fn apply_migrations(conn: &mut Connection, clock: &dyn meridian_clock::Clock) -> Result<()> {
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
        let at_ns = clock.now_ns();
        if let Some(then) = migration.then {
            then(&mut tx, at_ns)?;
        }
        migrations::record(&mut tx, migration, at_ns)?;
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
