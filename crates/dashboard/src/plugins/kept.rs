//! Plugin pages' sessions, kept (ruling 855): in the dashboard's own table
//! (migration 13), so a restart or an upgrade leaves a plugin's page open in
//! a browser working, its forms posting and its reads answered.
//!
//! By the SHA-256 of the plugin host's token, never the token, as a
//! browser's session is kept; naming the session it came from by that
//! session's hash or the delegation's id, so no token of any kind is kept.
//! One that came from a browser goes with that browser's row; one that came
//! through a delegation is swept once the delegation has ended.

use super::{Came, Entered};
use meridian_access::AccessLevel;

/// Where plugin pages' sessions are kept. Blocking, as
/// [`crate::session::SessionStore`] is.
pub(crate) trait PluginSessionStore: Send + Sync {
    /// Keep a new one by its token's hash.
    fn keep(&self, hash: &str, entered: &Entered, now_ns: i64) -> Result<(), String>;
    /// The one kept by this hash.
    fn kept(&self, hash: &str) -> Result<Option<Entered>, String>;
    /// The prepared change it was opened from, settled or gone.
    fn forget_prepared(&self, hash: &str) -> Result<(), String>;
    fn end(&self, hash: &str) -> Result<(), String>;
    /// Every one kept, with what it came from, for the sweep.
    fn held(&self) -> Result<Vec<(String, Came)>, String>;
}

fn level_of(name: &str) -> Option<AccessLevel> {
    meridian_access::parse_level(name)
}

/// The plugin pages' sessions in the dashboard's own table (migration 13).
pub(crate) struct InPostgres {
    database: crate::database::Database,
}

impl InPostgres {
    pub(crate) fn on(database: crate::database::Database) -> Self {
        Self { database }
    }
}

fn came_of(browser: Option<String>, delegation: Option<String>) -> Option<Came> {
    match (browser, delegation) {
        (Some(hash), None) => Some(Came::Browser(hash)),
        (None, Some(id)) => Some(Came::Delegation(id)),
        _ => None,
    }
}

impl PluginSessionStore for InPostgres {
    fn keep(&self, hash: &str, entered: &Entered, now_ns: i64) -> Result<(), String> {
        let (browser, delegation) = match &entered.came {
            Came::Browser(hash) => (Some(hash.as_str()), None),
            Came::Delegation(id) => (None, Some(id.as_str())),
        };
        self.database
            .conn()?
            .execute(
                "INSERT INTO dashboard_plugin_session
                     (plugin_session_hash, browser_session_hash, delegation_id,
                      plugin_instance_id, level, prepared_call_id, entered_at_ns)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (plugin_session_hash) DO NOTHING",
                &[
                    &hash,
                    &browser,
                    &delegation,
                    &entered.instance,
                    &meridian_access::level_name(entered.level),
                    &entered.prepared,
                    &now_ns,
                ],
            )
            .map_err(crate::database::said)?;
        Ok(())
    }

    fn kept(&self, hash: &str) -> Result<Option<Entered>, String> {
        let row = self
            .database
            .conn()?
            .query_opt(
                "SELECT browser_session_hash, delegation_id, plugin_instance_id, level,
                        prepared_call_id
                   FROM dashboard_plugin_session WHERE plugin_session_hash = $1",
                &[&hash],
            )
            .map_err(crate::database::said)?;
        Ok(row.and_then(|row| {
            Some(Entered {
                came: came_of(row.get(0), row.get(1))?,
                instance: row.get(2),
                level: level_of(row.get(3))?,
                prepared: row.get(4),
            })
        }))
    }

    fn forget_prepared(&self, hash: &str) -> Result<(), String> {
        self.database
            .conn()?
            .execute(
                "UPDATE dashboard_plugin_session SET prepared_call_id = ''
                  WHERE plugin_session_hash = $1",
                &[&hash],
            )
            .map_err(crate::database::said)?;
        Ok(())
    }

    fn end(&self, hash: &str) -> Result<(), String> {
        self.database
            .conn()?
            .execute(
                "DELETE FROM dashboard_plugin_session WHERE plugin_session_hash = $1",
                &[&hash],
            )
            .map_err(crate::database::said)?;
        Ok(())
    }

    fn held(&self) -> Result<Vec<(String, Came)>, String> {
        let rows = self
            .database
            .conn()?
            .query(
                "SELECT plugin_session_hash, browser_session_hash, delegation_id
                   FROM dashboard_plugin_session",
                &[],
            )
            .map_err(crate::database::said)?;
        Ok(rows
            .iter()
            .filter_map(|row| Some((row.get(0), came_of(row.get(1), row.get(2))?)))
            .collect())
    }
}

/// The plugin pages' sessions in memory, as a store: for tests of what
/// reads the table, and a second dashboard over the same memory.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct InMemory {
    pub(crate) kept: std::sync::Mutex<std::collections::HashMap<String, Entered>>,
}

#[cfg(test)]
impl PluginSessionStore for InMemory {
    fn keep(&self, hash: &str, entered: &Entered, _now_ns: i64) -> Result<(), String> {
        self.kept
            .lock()
            .expect("lock")
            .entry(hash.to_string())
            .or_insert_with(|| entered.clone());
        Ok(())
    }
    fn kept(&self, hash: &str) -> Result<Option<Entered>, String> {
        Ok(self.kept.lock().expect("lock").get(hash).cloned())
    }
    fn forget_prepared(&self, hash: &str) -> Result<(), String> {
        if let Some(held) = self.kept.lock().expect("lock").get_mut(hash) {
            held.prepared.clear();
        }
        Ok(())
    }
    fn end(&self, hash: &str) -> Result<(), String> {
        self.kept.lock().expect("lock").remove(hash);
        Ok(())
    }
    fn held(&self) -> Result<Vec<(String, Came)>, String> {
        Ok(self
            .kept
            .lock()
            .expect("lock")
            .iter()
            .map(|(hash, held)| (hash.clone(), held.came.clone()))
            .collect())
    }
}
