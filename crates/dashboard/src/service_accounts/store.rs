//! What the dashboard keeps of service accounts (contract v21), in memory
//! for tests and a deployment with no database, and in its own tables
//! (migrations/0009_a_service_account.sql).
//!
//! Every change is its own record (decisions/031): an account created, each
//! part of it changed, its state changed, a role's "runs as" chosen, changed
//! or cleared. Records are never updated nor deleted; what stands now is
//! kept beside them in one step.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::database::{said, Database};

/// A service account's setting (W6.32): one of the levels that apply to its
/// call, the strictest winning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Setting {
    /// Every act it would make becomes a prepared change for a person.
    #[default]
    PrepareOnly,
    /// It makes an act where every level that applies lets agents make it
    /// and the act's second pair of eyes is off; otherwise it prepares.
    Act,
}

impl Setting {
    pub fn value(self) -> &'static str {
        match self {
            Setting::PrepareOnly => "prepare_only",
            Setting::Act => "act",
        }
    }

    pub fn parse(said: &str) -> Option<Setting> {
        match said {
            "prepare_only" => Some(Setting::PrepareOnly),
            "act" => Some(Setting::Act),
            _ => None,
        }
    }

    /// As the pages say it.
    pub fn words(self) -> &'static str {
        match self {
            Setting::PrepareOnly => "Prepare only",
            Setting::Act => "Act",
        }
    }
}

/// Whether a service account may act (W6.32).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum State {
    #[default]
    Active,
    Disabled,
    Removed,
}

impl State {
    pub fn value(self) -> &'static str {
        match self {
            State::Active => "active",
            State::Disabled => "disabled",
            State::Removed => "removed",
        }
    }

    pub fn parse(said: &str) -> Option<State> {
        match said {
            "active" => Some(State::Active),
            "disabled" => Some(State::Disabled),
            "removed" => Some(State::Removed),
            _ => None,
        }
    }

    pub fn words(self) -> &'static str {
        match self {
            State::Active => "Active",
            State::Disabled => "Disabled",
            State::Removed => "Removed",
        }
    }
}

/// A service account as kept (`ServiceAccount`, W6.32).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServiceAccount {
    /// `SA-` and a ULID, and its subject.
    pub service_account_id: String,
    pub name: String,
    pub purpose: String,
    pub owner_subject: String,
    pub setting: Setting,
    pub state: State,
    pub created_at_ns: i64,
    pub last_used_ns: Option<i64>,
    pub last_refusal: String,
    pub last_refusal_ns: Option<i64>,
}

/// A role's "runs as" (W6.33): the account and the level, `write` or `read`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunsAs {
    pub instance: String,
    pub role: String,
    pub service_account_id: String,
    pub level: String,
}

/// One change, its own record (`ServiceAccountChange`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Change {
    /// The account it names: the account changed, or the one a role's
    /// choice named after it (before it, when cleared).
    pub service_account_id: String,
    /// Empty for the account itself; `instance:role` for a role's choice.
    pub scope: String,
    pub from: String,
    pub to: String,
    pub by: String,
    pub at_ns: i64,
    pub note: String,
}

/// A notice to an account's owner (W6.24, W6.34): it was disabled, by whom;
/// or one of its prepared changes lapsed with it. Names, never quotes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Notice {
    pub notice_id: String,
    pub subject: String,
    pub service_account_id: String,
    /// `disabled`, `removed` or `lapsed`.
    pub kind: String,
    /// The prepared change that lapsed, for `lapsed`.
    pub prepared_call_id: String,
    pub by_subject: String,
    pub by_name: String,
    pub changed_ns: i64,
}

pub trait ServiceAccountStore: Send + Sync {
    /// Every account, removed ones too, oldest first.
    fn accounts(&self) -> Result<Vec<ServiceAccount>, String>;
    fn account(&self, id: &str) -> Result<Option<ServiceAccount>, String>;
    /// An account kept as it now stands, with the records of what changed
    /// and the notices it owes, in one step.
    fn put(
        &self,
        account: &ServiceAccount,
        changes: &[Change],
        notices: &[Notice],
    ) -> Result<(), String>;
    /// Its last use, and its last refusal where `refusal` is given.
    fn used(&self, id: &str, at_ns: i64, refusal: Option<&str>) -> Result<(), String>;
    fn changes(&self, id: &str) -> Result<Vec<Change>, String>;

    /// Every role's choice.
    fn runs_as(&self) -> Result<Vec<RunsAs>, String>;
    /// A role's choice set, or cleared with `None`, its record with it.
    fn set_runs_as(
        &self,
        instance: &str,
        role: &str,
        to: Option<&RunsAs>,
        change: &Change,
    ) -> Result<(), String>;

    /// A person's notices naming a service account, newest first.
    fn notices(&self, subject: &str, limit: usize) -> Result<Vec<Notice>, String>;
    fn notify(&self, notices: &[Notice]) -> Result<(), String>;
}

// ── In memory ───────────────────────────────────────────────────────────────

#[derive(Default)]
struct Held {
    accounts: BTreeMap<String, ServiceAccount>,
    changes: Vec<Change>,
    runs_as: BTreeMap<(String, String), RunsAs>,
    notices: Vec<Notice>,
}

#[derive(Default)]
pub struct InMemory {
    held: Mutex<Held>,
}

impl InMemory {
    fn lock(&self) -> std::sync::MutexGuard<'_, Held> {
        self.held
            .lock()
            .expect("service account store lock poisoned")
    }
}

impl ServiceAccountStore for InMemory {
    fn accounts(&self) -> Result<Vec<ServiceAccount>, String> {
        let mut all: Vec<ServiceAccount> = self.lock().accounts.values().cloned().collect();
        all.sort_by_key(|account| account.created_at_ns);
        Ok(all)
    }

    fn account(&self, id: &str) -> Result<Option<ServiceAccount>, String> {
        Ok(self.lock().accounts.get(id).cloned())
    }

    fn put(
        &self,
        account: &ServiceAccount,
        changes: &[Change],
        notices: &[Notice],
    ) -> Result<(), String> {
        let mut held = self.lock();
        held.accounts
            .insert(account.service_account_id.clone(), account.clone());
        held.changes.extend_from_slice(changes);
        held.notices.extend_from_slice(notices);
        Ok(())
    }

    fn used(&self, id: &str, at_ns: i64, refusal: Option<&str>) -> Result<(), String> {
        if let Some(account) = self.lock().accounts.get_mut(id) {
            account.last_used_ns = Some(at_ns);
            if let Some(refusal) = refusal {
                account.last_refusal = refusal.to_string();
                account.last_refusal_ns = Some(at_ns);
            }
        }
        Ok(())
    }

    fn changes(&self, id: &str) -> Result<Vec<Change>, String> {
        Ok(self
            .lock()
            .changes
            .iter()
            .filter(|change| change.service_account_id == id)
            .cloned()
            .collect())
    }

    fn runs_as(&self) -> Result<Vec<RunsAs>, String> {
        Ok(self.lock().runs_as.values().cloned().collect())
    }

    fn set_runs_as(
        &self,
        instance: &str,
        role: &str,
        to: Option<&RunsAs>,
        change: &Change,
    ) -> Result<(), String> {
        let mut held = self.lock();
        let key = (instance.to_string(), role.to_string());
        match to {
            Some(chosen) => {
                held.runs_as.insert(key, chosen.clone());
            }
            None => {
                held.runs_as.remove(&key);
            }
        }
        held.changes.push(change.clone());
        Ok(())
    }

    fn notices(&self, subject: &str, limit: usize) -> Result<Vec<Notice>, String> {
        let mut found: Vec<Notice> = self
            .lock()
            .notices
            .iter()
            .filter(|notice| notice.subject == subject)
            .cloned()
            .collect();
        found.sort_by(|a, b| b.notice_id.cmp(&a.notice_id));
        found.truncate(limit);
        Ok(found)
    }

    fn notify(&self, notices: &[Notice]) -> Result<(), String> {
        self.lock().notices.extend_from_slice(notices);
        Ok(())
    }
}

// ── In the dashboard's tables ───────────────────────────────────────────────

pub struct InPostgres {
    database: Database,
}

impl InPostgres {
    /// On a database already verified at start.
    pub fn on(database: Database) -> Self {
        Self { database }
    }
}

const ACCOUNT_COLUMNS: &str = "service_account_id, name, purpose, owner_subject, setting, state, \
     created_at_ns, last_used_ns, last_refusal, last_refusal_ns";

fn account_of(row: &postgres::Row) -> ServiceAccount {
    ServiceAccount {
        service_account_id: row.get(0),
        name: row.get(1),
        purpose: row.get(2),
        owner_subject: row.get(3),
        setting: Setting::parse(row.get::<_, &str>(4)).unwrap_or_default(),
        state: State::parse(row.get::<_, &str>(5)).unwrap_or_default(),
        created_at_ns: row.get(6),
        last_used_ns: row.get(7),
        last_refusal: row.get(8),
        last_refusal_ns: row.get(9),
    }
}

fn insert_change(client: &mut impl postgres::GenericClient, change: &Change) -> Result<(), String> {
    client
        .execute(
            "INSERT INTO dashboard_service_account_change
                    (service_account_id, scope, from_said, to_said, changed_by, changed_at_ns, note)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            &[
                &change.service_account_id,
                &change.scope,
                &change.from,
                &change.to,
                &change.by,
                &change.at_ns,
                &change.note,
            ],
        )
        .map_err(said)?;
    Ok(())
}

fn insert_notice(client: &mut impl postgres::GenericClient, notice: &Notice) -> Result<(), String> {
    client
        .execute(
            "INSERT INTO dashboard_notice (notice_id, subject, ticket_id, prepared_call_id,
                 service_account_id, kind, author_provenance, author_subject, author_person,
                 changed_ns, read)
             VALUES ($1, $2, NULL, $3, $4, $5, 'person', $6, $7, $8, false)",
            &[
                &notice.notice_id,
                &notice.subject,
                &notice.prepared_call_id,
                &notice.service_account_id,
                &notice.kind,
                &notice.by_subject,
                &notice.by_name,
                &notice.changed_ns,
            ],
        )
        .map_err(said)?;
    Ok(())
}

impl ServiceAccountStore for InPostgres {
    fn accounts(&self) -> Result<Vec<ServiceAccount>, String> {
        let mut conn = self.database.conn()?;
        let rows = conn
            .query(
                &format!(
                    "SELECT {ACCOUNT_COLUMNS} FROM dashboard_service_account
                      ORDER BY created_at_ns, service_account_id"
                ),
                &[],
            )
            .map_err(said)?;
        Ok(rows.iter().map(account_of).collect())
    }

    fn account(&self, id: &str) -> Result<Option<ServiceAccount>, String> {
        let mut conn = self.database.conn()?;
        let row = conn
            .query_opt(
                &format!(
                    "SELECT {ACCOUNT_COLUMNS} FROM dashboard_service_account
                      WHERE service_account_id = $1"
                ),
                &[&id],
            )
            .map_err(said)?;
        Ok(row.as_ref().map(account_of))
    }

    fn put(
        &self,
        account: &ServiceAccount,
        changes: &[Change],
        notices: &[Notice],
    ) -> Result<(), String> {
        let mut conn = self.database.conn()?;
        let mut tx = conn.transaction().map_err(said)?;
        tx.execute(
            "INSERT INTO dashboard_service_account (service_account_id, name, purpose,
                 owner_subject, setting, state, created_at_ns, last_used_ns, last_refusal,
                 last_refusal_ns)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (service_account_id) DO UPDATE SET
                 name = excluded.name, purpose = excluded.purpose,
                 owner_subject = excluded.owner_subject, setting = excluded.setting,
                 state = excluded.state",
            &[
                &account.service_account_id,
                &account.name,
                &account.purpose,
                &account.owner_subject,
                &account.setting.value(),
                &account.state.value(),
                &account.created_at_ns,
                &account.last_used_ns,
                &account.last_refusal,
                &account.last_refusal_ns,
            ],
        )
        .map_err(said)?;
        for change in changes {
            insert_change(&mut tx, change)?;
        }
        for notice in notices {
            insert_notice(&mut tx, notice)?;
        }
        tx.commit().map_err(said)
    }

    fn used(&self, id: &str, at_ns: i64, refusal: Option<&str>) -> Result<(), String> {
        let mut conn = self.database.conn()?;
        match refusal {
            Some(refusal) => conn.execute(
                "UPDATE dashboard_service_account
                    SET last_used_ns = $2, last_refusal = $3, last_refusal_ns = $2
                  WHERE service_account_id = $1",
                &[&id, &at_ns, &refusal],
            ),
            None => conn.execute(
                "UPDATE dashboard_service_account SET last_used_ns = $2
                  WHERE service_account_id = $1",
                &[&id, &at_ns],
            ),
        }
        .map_err(said)?;
        Ok(())
    }

    fn changes(&self, id: &str) -> Result<Vec<Change>, String> {
        let mut conn = self.database.conn()?;
        let rows = conn
            .query(
                "SELECT service_account_id, scope, from_said, to_said, changed_by, changed_at_ns,
                        note
                   FROM dashboard_service_account_change
                  WHERE service_account_id = $1 ORDER BY change_id",
                &[&id],
            )
            .map_err(said)?;
        Ok(rows
            .iter()
            .map(|row| Change {
                service_account_id: row.get(0),
                scope: row.get(1),
                from: row.get(2),
                to: row.get(3),
                by: row.get(4),
                at_ns: row.get(5),
                note: row.get(6),
            })
            .collect())
    }

    fn runs_as(&self) -> Result<Vec<RunsAs>, String> {
        let mut conn = self.database.conn()?;
        let rows = conn
            .query(
                "SELECT instance, role, service_account_id, level FROM dashboard_runs_as
                  ORDER BY instance, role",
                &[],
            )
            .map_err(said)?;
        Ok(rows
            .iter()
            .map(|row| RunsAs {
                instance: row.get(0),
                role: row.get(1),
                service_account_id: row.get(2),
                level: row.get(3),
            })
            .collect())
    }

    fn set_runs_as(
        &self,
        instance: &str,
        role: &str,
        to: Option<&RunsAs>,
        change: &Change,
    ) -> Result<(), String> {
        let mut conn = self.database.conn()?;
        let mut tx = conn.transaction().map_err(said)?;
        match to {
            Some(chosen) => tx.execute(
                "INSERT INTO dashboard_runs_as (instance, role, service_account_id, level)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (instance, role) DO UPDATE SET
                     service_account_id = excluded.service_account_id, level = excluded.level",
                &[&instance, &role, &chosen.service_account_id, &chosen.level],
            ),
            None => tx.execute(
                "DELETE FROM dashboard_runs_as WHERE instance = $1 AND role = $2",
                &[&instance, &role],
            ),
        }
        .map_err(said)?;
        insert_change(&mut tx, change)?;
        tx.commit().map_err(said)
    }

    fn notices(&self, subject: &str, limit: usize) -> Result<Vec<Notice>, String> {
        let mut conn = self.database.conn()?;
        let rows = conn
            .query(
                "SELECT notice_id, subject, service_account_id, kind, prepared_call_id,
                        author_subject, author_person, changed_ns
                   FROM dashboard_notice
                  WHERE subject = $1 AND service_account_id <> ''
                  ORDER BY notice_id DESC LIMIT $2",
                &[&subject, &(limit as i64)],
            )
            .map_err(said)?;
        Ok(rows
            .iter()
            .map(|row| Notice {
                notice_id: row.get(0),
                subject: row.get(1),
                service_account_id: row.get(2),
                kind: row.get(3),
                prepared_call_id: row.get(4),
                by_subject: row.get(5),
                by_name: row.get(6),
                changed_ns: row.get(7),
            })
            .collect())
    }

    fn notify(&self, notices: &[Notice]) -> Result<(), String> {
        let mut conn = self.database.conn()?;
        let mut tx = conn.transaction().map_err(said)?;
        for notice in notices {
            insert_notice(&mut tx, notice)?;
        }
        tx.commit().map_err(said)
    }
}
