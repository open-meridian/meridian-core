//! The configuration store's migration history.
//!
//! Written from the first commit, because the argument against the conductor
//! holding a store was that nobody would write its migrations. Same shape as
//! the street store's: [`apply`](crate::PostgresStore::migrate) runs once per
//! release from `meridian-conductor migrate`, and starting only verifies.
//!
//! The history table is `config_schema_migration`, not the street store's
//! `schema_migration`, because a deployment may put both stores in one schema.

use postgres::Transaction;

use crate::store::{Result, StoreError};

pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static str,
    /// What the migration writes that needs the deployment's time
    /// (decisions/024): a gap record it owes the past (decisions/031), stamped
    /// at the moment it ran. In the same transaction as `sql`.
    pub then: Option<fn(&mut Transaction<'_>, i64) -> Result<()>>,
}

/// In order, and never reordered or edited after release: the record of what
/// ran names a version, and editing one makes that record a lie.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "config",
        sql: include_str!("../migrations/0001_config.sql"),
        then: None,
    },
    Migration {
        version: 2,
        name: "plugin_roles",
        sql: include_str!("../migrations/0002_plugin_roles.sql"),
        then: None,
    },
    Migration {
        version: 3,
        name: "plugin_catalogue",
        sql: include_str!("../migrations/0003_plugin_catalogue.sql"),
        then: None,
    },
    Migration {
        version: 4,
        name: "plugin_launch_live",
        sql: include_str!("../migrations/0004_plugin_launch_live.sql"),
        then: None,
    },
    Migration {
        version: 5,
        name: "plugin_settings",
        sql: include_str!("../migrations/0005_plugin_settings.sql"),
        then: None,
    },
    Migration {
        version: 6,
        name: "setting_declaration_whole",
        sql: include_str!("../migrations/0006_setting_declaration_whole.sql"),
        then: None,
    },
    Migration {
        version: 7,
        name: "access_is_read_or_write",
        sql: include_str!("../migrations/0007_access_is_read_or_write.sql"),
        then: None,
    },
    Migration {
        version: 8,
        name: "account_attributes",
        sql: include_str!("../migrations/0008_account_attributes.sql"),
        then: None,
    },
    Migration {
        version: 9,
        name: "a_plugin_has_admins",
        sql: include_str!("../migrations/0009_a_plugin_has_admins.sql"),
        then: None,
    },
    Migration {
        version: 10,
        name: "plugin_declaration",
        sql: include_str!("../migrations/0010_plugin_declaration.sql"),
        then: None,
    },
    Migration {
        version: 11,
        name: "each_setting_change_its_own_record",
        sql: include_str!("../migrations/0011_each_setting_change_its_own_record.sql"),
        then: Some(settings_not_known_before),
    },
    Migration {
        version: 12,
        name: "a_secret_settings_earlier_values_redacted",
        sql: include_str!("../migrations/0012_a_secret_settings_earlier_values_redacted.sql"),
        then: Some(secret_settings_redacted),
    },
    Migration {
        version: 13,
        name: "access_is_granted_per_role",
        sql: include_str!("../migrations/0013_access_is_granted_per_role.sql"),
        then: Some(access_per_role),
    },
    Migration {
        version: 14,
        name: "an_edge_plugins_older_records_move_to_the_archive",
        sql: include_str!(
            "../migrations/0014_an_edge_plugins_older_records_move_to_the_archive.sql"
        ),
        then: None,
    },
];

/// Migration 13 (contract v15): each access group's gap record, then the
/// one-time rewrite of the entries naming no role, each group it rewrote its
/// own record, all at `at_ns`, the moment it ran.
fn access_per_role(tx: &mut Transaction<'_>, at_ns: i64) -> Result<()> {
    access_not_known_before(tx, at_ns)?;
    rewrite_entries_to_name_their_role(tx, at_ns)?;
    Ok(())
}

/// One gap record per access group (decisions/031, point 4): its history --
/// its entries, and the permissions to it -- is not known before `at_ns`. A
/// group given one already is left as it is.
fn access_not_known_before(tx: &mut Transaction<'_>, at_ns: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO config_access_change (access_group_id, kind, changed_at_ns, note)
         SELECT access_group_id, 6, $1::bigint, $2::text
           FROM config_access_group grp
          WHERE NOT EXISTS (SELECT 1 FROM config_access_change gap
                             WHERE gap.access_group_id = grp.access_group_id AND gap.kind = 6)
          ORDER BY access_group_id",
        &[&at_ns, &crate::store::ACCESS_NOT_KNOWN_BEFORE],
    )
    .map_err(|failed| StoreError::Unavailable(failed.to_string()))?;
    Ok(())
}

/// The one-time rewrite (W6.7; the spec's requirement 27, the plan's Q3):
/// every entry naming no role whose plugin holds exactly one -- as its
/// sidecar last reported, or where none has, as its latest launch said --
/// is rewritten to name it, and each access group rewritten is recorded,
/// what it was and what it became, by no person. Only entries naming no role
/// are read, so it is idempotent: run again, it changes nothing. Returns the
/// groups it rewrote.
pub fn rewrite_entries_to_name_their_role(
    tx: &mut Transaction<'_>,
    at_ns: i64,
) -> Result<Vec<String>> {
    let unavailable = |failed: postgres::Error| StoreError::Unavailable(failed.to_string());
    let reported: std::collections::BTreeMap<String, Vec<String>> = tx
        .query(
            "SELECT plugin_instance_id, roles FROM config_known_plugin",
            &[],
        )
        .map_err(unavailable)?
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    let launched: std::collections::BTreeMap<String, Vec<String>> = tx
        .query(
            "SELECT DISTINCT ON (instance_id) instance_id, roles FROM config_plugin_launch
              ORDER BY instance_id, launch_id DESC",
            &[],
        )
        .map_err(unavailable)?
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    let groups: Vec<String> = tx
        .query(
            "SELECT DISTINCT access_group_id FROM config_access_entry WHERE role = ''
              ORDER BY access_group_id",
            &[],
        )
        .map_err(unavailable)?
        .iter()
        .map(|row| row.get(0))
        .collect();
    let mut rewritten = Vec::new();
    for group in groups {
        let entries: Vec<(i32, String, i16, String)> = tx
            .query(
                "SELECT position, plugin_instance_id, level, role FROM config_access_entry
                  WHERE access_group_id = $1 ORDER BY position",
                &[&group],
            )
            .map_err(unavailable)?
            .iter()
            .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3)))
            .collect();
        let name: String = tx
            .query_one(
                "SELECT name FROM config_access_group WHERE access_group_id = $1",
                &[&group],
            )
            .map_err(unavailable)?
            .get(0);
        let as_group = |entries: &[(i32, String, i16, String)]| meridian_domain::v1::AccessGroup {
            access_group_id: group.clone(),
            name: name.clone(),
            entries: entries
                .iter()
                .map(
                    |(_, plugin, level, role)| meridian_domain::v1::AccessEntry {
                        plugin_instance_id: plugin.clone(),
                        level: i32::from(*level),
                        role: role.clone(),
                    },
                )
                .collect(),
            built_in: false,
        };
        let was = as_group(&entries);
        let mut became = entries.clone();
        let mut changed = false;
        for (position, plugin, _, role) in &mut became {
            if !role.is_empty() {
                continue;
            }
            let Some(one) = crate::store::the_one_role(
                reported.get(plugin.as_str()).map(Vec::as_slice),
                launched.get(plugin.as_str()).map(Vec::as_slice),
            ) else {
                continue;
            };
            tx.execute(
                "UPDATE config_access_entry SET role = $3
                  WHERE access_group_id = $1 AND position = $2 AND role = ''",
                &[&group, &*position, &one],
            )
            .map_err(unavailable)?;
            *role = one.to_string();
            changed = true;
        }
        if !changed {
            continue;
        }
        let record = crate::store::AccessChangeRecord {
            access_group_id: group.clone(),
            kind: crate::store::AccessChangeKind::Rewritten,
            was: crate::store::described_group(&was),
            became: crate::store::described_group(&as_group(&became)),
            permission_id: String::new(),
            by: String::new(),
            delegation: String::new(),
            at_ns,
            note: crate::store::REWRITTEN_TO_NAME_ITS_ROLE.to_string(),
        };
        crate::postgres::insert_access_change(tx, &record)?;
        tracing::info!(
            access_group = group,
            was = record.was,
            became = record.became,
            "access group rewritten to name each entry's role (contract v15)"
        );
        rewritten.push(group);
    }
    Ok(rewritten)
}

/// Migration 12's redactions: each setting declared secret now, or held
/// sealed, whose earlier change records still hold a value.
fn secret_settings_redacted(tx: &mut Transaction<'_>, at_ns: i64) -> Result<()> {
    let secret = tx
        .query(
            "SELECT plugin_instance_id, name FROM config_plugin_setting_declaration
              WHERE secret
             UNION
             SELECT plugin_instance_id, name FROM config_plugin_setting
              WHERE sealed IS NOT NULL
             ORDER BY 1, 2",
            &[],
        )
        .map_err(|failed| StoreError::Unavailable(failed.to_string()))?;
    for row in secret {
        let (plugin, name): (String, String) = (row.get(0), row.get(1));
        redact_values(
            tx,
            &plugin,
            &name,
            at_ns,
            "the setting is secret; by migration 12, no person",
        )?;
    }
    Ok(())
}

/// Blank every value one setting's change records hold, and record the
/// redaction as its own change (action 4) naming the records and `why`, by
/// no person, at `at_ns`: the product owner's one exception to a record never
/// changing (2026-10-05), and only its value. Nothing when none holds one.
pub(crate) fn redact_values(
    tx: &mut Transaction<'_>,
    plugin_instance_id: &str,
    name: &str,
    at_ns: i64,
    why: &str,
) -> Result<Vec<i64>> {
    let unavailable = |failed: postgres::Error| StoreError::Unavailable(failed.to_string());
    let mut ids: Vec<i64> = tx
        .query(
            "UPDATE config_plugin_setting_change SET value = NULL
              WHERE plugin_instance_id = $1 AND name = $2 AND value IS NOT NULL
             RETURNING change_id",
            &[&plugin_instance_id, &name],
        )
        .map_err(unavailable)?
        .iter()
        .map(|row| row.get(0))
        .collect();
    ids.sort_unstable();
    if !ids.is_empty() {
        tx.execute(
            "INSERT INTO config_plugin_setting_change
                    (plugin_instance_id, name, action, changed_by, changed_at_ns, secret, note)
             VALUES ($1, $2, 4, '', $3, true, $4)",
            &[
                &plugin_instance_id,
                &name,
                &at_ns,
                &crate::store::redaction_note(&ids, why),
            ],
        )
        .map_err(unavailable)?;
    }
    Ok(ids)
}

/// Migration 11's gap records (decisions/031, point 4): for each setting of
/// each plugin with a change recorded before it, one record saying the
/// earlier changes' values are not known before
/// `at_ns`, the moment the migration ran, by the deployment's clock. A
/// setting already given one is left as it is.
fn settings_not_known_before(tx: &mut Transaction<'_>, at_ns: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO config_plugin_setting_change
                (plugin_instance_id, name, action, changed_by, changed_at_ns, note)
         SELECT DISTINCT change.plugin_instance_id, change.name, 3, '', $1::bigint,
                'not known before: until migration 11 a settings change recorded who and when, '
                || 'never the value it set; each change since records it'
           FROM config_plugin_setting_change change
          WHERE change.action IN (1, 2)
            AND NOT EXISTS (SELECT 1 FROM config_plugin_setting_change gap
                             WHERE gap.plugin_instance_id = change.plugin_instance_id
                               AND gap.name = change.name
                               AND gap.action = 3)",
        &[&at_ns],
    )
    .map_err(|failed| StoreError::Unavailable(failed.to_string()))?;
    Ok(())
}

pub const HISTORY: &str = "\
CREATE TABLE IF NOT EXISTS config_schema_migration (
    version     bigint PRIMARY KEY,
    name        text   NOT NULL,
    applied_at_ns bigint NOT NULL
)";

pub fn latest() -> i64 {
    MIGRATIONS.last().map(|m| m.version).unwrap_or(0)
}

/// What a start does. Reads one table, takes no lock, and answers with a
/// sentence naming the fix rather than failing later on a query.
pub fn verify(applied: Option<i64>) -> Result<()> {
    let latest = latest();
    match applied {
        None => Err(StoreError::Unavailable(format!(
            "the configuration store's database has no schema; it is at no version and this binary \
             expects {latest}. Run `meridian-conductor migrate` before starting."
        ))),
        Some(at) if at < latest => Err(StoreError::Unavailable(format!(
            "the configuration store's database is at schema version {at} and this binary expects \
             {latest}. Run `meridian-conductor migrate` before starting."
        ))),
        // Ahead, which is a rollback: the database has been migrated by a newer
        // release. Refused rather than tolerated, because this binary does not
        // know what that release changed and its queries may already be wrong.
        Some(at) if at > latest => Err(StoreError::SchemaAhead(format!(
            "the configuration store's database is at schema version {at}, which is newer than this \
             binary understands ({latest}). Run the release that migrated it, or restore \
             a database at {latest}."
        ))),
        Some(_) => Ok(()),
    }
}

/// Record a migration as applied, in the same transaction that ran it.
pub fn record(tx: &mut Transaction<'_>, migration: &Migration, at_ns: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO config_schema_migration (version, name, applied_at_ns) VALUES ($1, $2, $3)",
        &[&migration.version, &migration.name, &at_ns],
    )
    .map_err(|failed| StoreError::Unavailable(failed.to_string()))?;
    Ok(())
}
