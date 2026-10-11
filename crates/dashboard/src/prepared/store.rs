//! What the dashboard keeps of prepared changes, the admins' levels and a
//! person's own settings (contract v20), in memory for tests and a
//! deployment with no database, and in its own tables
//! (migrations/0008_an_agents_prepared_change.sql).
//!
//! Every change is its own record (decisions/031): a prepared change's
//! steps, each level's and gate's change, each own setting's. Records are
//! never updated; what stands now is kept beside them in one step.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::database::{said, Database};

/// A prepared change as kept (W6.26).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Kept {
    pub prepared_call_id: String,
    /// The act by its name on the surface: `dashboard__set_hold`,
    /// `sample-ops-1__confirm_opening_balance`.
    pub tool_name: String,
    /// The plugin instance whose act it is; empty for one of core's.
    pub instance: String,
    pub arguments_json: String,
    pub preparer_subject: String,
    pub preparer_name: String,
    /// The delegation and its client where prepared through one; empty for
    /// a person at a page.
    pub through_delegation: String,
    pub client_name: String,
    pub reason: String,
    pub reason_suspect: bool,
    /// meridian.v1.PreparedCallState's number.
    pub state: i32,
    pub accounts: Vec<String>,
    pub checked: String,
    pub prepared_at_ns: i64,
    pub settled_at_ns: Option<i64>,
}

/// One step of a prepared change (W6.28).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    Prepared = 1,
    Shared = 2,
    ShownWithWhatDiffers = 3,
    Made = 4,
    Changed = 5,
    Discarded = 6,
    Withdrawn = 7,
    Lapsed = 8,
}

impl StepKind {
    pub fn from_code(code: i16) -> Option<StepKind> {
        Some(match code {
            1 => StepKind::Prepared,
            2 => StepKind::Shared,
            3 => StepKind::ShownWithWhatDiffers,
            4 => StepKind::Made,
            5 => StepKind::Changed,
            6 => StepKind::Discarded,
            7 => StepKind::Withdrawn,
            8 => StepKind::Lapsed,
            _ => return None,
        })
    }

    /// As a person reads it in the history.
    pub fn said(self) -> &'static str {
        match self {
            StepKind::Prepared => "prepared",
            StepKind::Shared => "shared",
            StepKind::ShownWithWhatDiffers => "shown with what differs",
            StepKind::Made => "made",
            StepKind::Changed => "changed, then made",
            StepKind::Discarded => "discarded",
            StepKind::Withdrawn => "withdrawn",
            StepKind::Lapsed => "lapsed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub prepared_call_id: String,
    pub kind: StepKind,
    pub by_subject: String,
    pub through_delegation: String,
    pub client_name: String,
    pub at_ns: i64,
    pub note: String,
}

/// A notice naming a prepared change (W6.24, widened at v20): to whom, what
/// became of it, who did it, when; never its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedNotice {
    pub notice_id: String,
    pub subject: String,
    pub prepared_call_id: String,
    pub kind: String,
    pub by_subject: String,
    pub by_name: String,
    pub through_delegation: String,
    pub client_name: String,
    pub changed_ns: i64,
    pub read: bool,
}

/// What agents may do with an act at a scope (W6.29, `agents_may`) and its
/// gate (`second_pair_of_eyes`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LevelRow {
    pub act: String,
    pub scope: String,
    /// 1 may make, 2 prepare only, 3 may not.
    pub level: i16,
    pub second_pair_of_eyes: bool,
}

/// One change of a level, a gate or the lapse period (`AgentsMayChange`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LevelChange {
    pub act: String,
    pub scope: String,
    pub from: String,
    pub to: String,
    pub by: String,
    pub delegation: String,
    pub client: String,
    pub at_ns: i64,
    pub note: String,
}

/// One change of a person's own setting (`OwnSettingChange`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OwnChange {
    pub instance: String,
    pub subject: String,
    pub name: String,
    pub value: String,
    pub delegation: String,
    pub client: String,
    pub at_ns: i64,
}

/// The pending states: prepared, shared.
pub const PENDING: [i32; 2] = [1, 2];

pub trait PreparedStore: Send + Sync {
    /// A prepared change kept, its first step and its notices, in one step.
    fn insert(&self, kept: &Kept, step: &Step, notices: &[PreparedNotice]) -> Result<(), String>;
    fn get(&self, id: &str) -> Result<Option<Kept>, String>;
    /// Every pending one, oldest first.
    fn pending(&self) -> Result<Vec<Kept>, String>;
    /// Those prepared through one delegation, newest first, at most `limit`.
    fn by_delegation(&self, delegation: &str, limit: usize) -> Result<Vec<Kept>, String>;
    /// Moved from one of `from` to `to` (settled when `to` is not pending),
    /// its step and notices with it, only if it stands in `from`: false, and
    /// nothing written, when it had moved already -- the first who settles it
    /// settles it for all.
    fn transition(
        &self,
        id: &str,
        from: &[i32],
        to: i32,
        step: &Step,
        notices: &[PreparedNotice],
    ) -> Result<bool, String>;
    /// A step that moves nothing (shown with what differs).
    fn note_step(&self, step: &Step) -> Result<(), String>;
    fn steps(&self, id: &str) -> Result<Vec<Step>, String>;
    /// A person's notices naming a prepared change, newest first.
    fn notices(&self, subject: &str, limit: usize) -> Result<Vec<PreparedNotice>, String>;
    fn mark_notices_read(&self, subject: &str, ids: &[String]) -> Result<usize, String>;
    /// Settled ones, their steps and notices, past `before_ns`.
    fn sweep(&self, before_ns: i64) -> Result<usize, String>;

    fn levels(&self) -> Result<Vec<LevelRow>, String>;
    /// A level set at a scope, or cleared with `None`, its record with it.
    fn set_level(
        &self,
        act: &str,
        scope: &str,
        row: Option<&LevelRow>,
        change: &LevelChange,
    ) -> Result<(), String>;
    fn level_changes(&self, act: &str) -> Result<Vec<LevelChange>, String>;
    fn setting(&self, name: &str) -> Result<Option<String>, String>;
    fn set_setting(&self, name: &str, value: &str, change: &LevelChange) -> Result<(), String>;

    /// A person's own values for one instance.
    fn own(&self, instance: &str, subject: &str) -> Result<BTreeMap<String, String>, String>;
    /// Their values set (an empty value clears it), each its own record.
    fn set_own(&self, changes: &[OwnChange]) -> Result<(), String>;
    fn own_changes(&self, subject: &str, instance: &str) -> Result<Vec<OwnChange>, String>;
}

// ── In memory ───────────────────────────────────────────────────────────────

#[derive(Default)]
struct Held {
    calls: BTreeMap<String, Kept>,
    steps: Vec<Step>,
    notices: Vec<PreparedNotice>,
    levels: BTreeMap<(String, String), LevelRow>,
    level_changes: Vec<LevelChange>,
    settings: BTreeMap<String, String>,
    own: BTreeMap<(String, String, String), String>,
    own_changes: Vec<OwnChange>,
}

#[derive(Default)]
pub struct InMemory {
    held: Mutex<Held>,
}

impl InMemory {
    fn lock(&self) -> std::sync::MutexGuard<'_, Held> {
        self.held.lock().expect("prepared store lock poisoned")
    }
}

impl PreparedStore for InMemory {
    fn insert(&self, kept: &Kept, step: &Step, notices: &[PreparedNotice]) -> Result<(), String> {
        let mut held = self.lock();
        held.calls
            .insert(kept.prepared_call_id.clone(), kept.clone());
        held.steps.push(step.clone());
        held.notices.extend_from_slice(notices);
        Ok(())
    }

    fn get(&self, id: &str) -> Result<Option<Kept>, String> {
        Ok(self.lock().calls.get(id).cloned())
    }

    fn pending(&self) -> Result<Vec<Kept>, String> {
        let mut pending: Vec<Kept> = self
            .lock()
            .calls
            .values()
            .filter(|kept| PENDING.contains(&kept.state))
            .cloned()
            .collect();
        pending.sort_by_key(|kept| kept.prepared_at_ns);
        Ok(pending)
    }

    fn by_delegation(&self, delegation: &str, limit: usize) -> Result<Vec<Kept>, String> {
        let mut found: Vec<Kept> = self
            .lock()
            .calls
            .values()
            .filter(|kept| kept.through_delegation == delegation)
            .cloned()
            .collect();
        found.sort_by_key(|kept| std::cmp::Reverse(kept.prepared_at_ns));
        found.truncate(limit);
        Ok(found)
    }

    fn transition(
        &self,
        id: &str,
        from: &[i32],
        to: i32,
        step: &Step,
        notices: &[PreparedNotice],
    ) -> Result<bool, String> {
        let mut held = self.lock();
        let Some(kept) = held.calls.get_mut(id) else {
            return Ok(false);
        };
        if !from.contains(&kept.state) {
            return Ok(false);
        }
        kept.state = to;
        if !PENDING.contains(&to) {
            kept.settled_at_ns = Some(step.at_ns);
        }
        held.steps.push(step.clone());
        held.notices.extend_from_slice(notices);
        Ok(true)
    }

    fn note_step(&self, step: &Step) -> Result<(), String> {
        self.lock().steps.push(step.clone());
        Ok(())
    }

    fn steps(&self, id: &str) -> Result<Vec<Step>, String> {
        Ok(self
            .lock()
            .steps
            .iter()
            .filter(|step| step.prepared_call_id == id)
            .cloned()
            .collect())
    }

    fn notices(&self, subject: &str, limit: usize) -> Result<Vec<PreparedNotice>, String> {
        let mut found: Vec<PreparedNotice> = self
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

    fn mark_notices_read(&self, subject: &str, ids: &[String]) -> Result<usize, String> {
        let mut held = self.lock();
        let mut marked = 0;
        for notice in held.notices.iter_mut() {
            if notice.subject == subject && ids.contains(&notice.prepared_call_id) && !notice.read {
                notice.read = true;
                marked += 1;
            }
        }
        Ok(marked)
    }

    fn sweep(&self, before_ns: i64) -> Result<usize, String> {
        let mut held = self.lock();
        let gone: Vec<String> = held
            .calls
            .values()
            .filter(|kept| kept.settled_at_ns.is_some_and(|at| at < before_ns))
            .map(|kept| kept.prepared_call_id.clone())
            .collect();
        for id in &gone {
            held.calls.remove(id);
        }
        held.steps
            .retain(|step| !gone.contains(&step.prepared_call_id));
        held.notices
            .retain(|notice| !gone.contains(&notice.prepared_call_id));
        Ok(gone.len())
    }

    fn levels(&self) -> Result<Vec<LevelRow>, String> {
        Ok(self.lock().levels.values().cloned().collect())
    }

    fn set_level(
        &self,
        act: &str,
        scope: &str,
        row: Option<&LevelRow>,
        change: &LevelChange,
    ) -> Result<(), String> {
        let mut held = self.lock();
        let key = (act.to_string(), scope.to_string());
        match row {
            Some(row) => {
                held.levels.insert(key, row.clone());
            }
            None => {
                held.levels.remove(&key);
            }
        }
        held.level_changes.push(change.clone());
        Ok(())
    }

    fn level_changes(&self, act: &str) -> Result<Vec<LevelChange>, String> {
        Ok(self
            .lock()
            .level_changes
            .iter()
            .filter(|change| change.act == act)
            .cloned()
            .collect())
    }

    fn setting(&self, name: &str) -> Result<Option<String>, String> {
        Ok(self.lock().settings.get(name).cloned())
    }

    fn set_setting(&self, name: &str, value: &str, change: &LevelChange) -> Result<(), String> {
        let mut held = self.lock();
        held.settings.insert(name.to_string(), value.to_string());
        held.level_changes.push(change.clone());
        Ok(())
    }

    fn own(&self, instance: &str, subject: &str) -> Result<BTreeMap<String, String>, String> {
        Ok(self
            .lock()
            .own
            .iter()
            .filter(|((i, s, _), _)| i == instance && s == subject)
            .map(|((_, _, name), value)| (name.clone(), value.clone()))
            .collect())
    }

    fn set_own(&self, changes: &[OwnChange]) -> Result<(), String> {
        let mut held = self.lock();
        for change in changes {
            let key = (
                change.instance.clone(),
                change.subject.clone(),
                change.name.clone(),
            );
            if change.value.is_empty() {
                held.own.remove(&key);
            } else {
                held.own.insert(key, change.value.clone());
            }
            held.own_changes.push(change.clone());
        }
        Ok(())
    }

    fn own_changes(&self, subject: &str, instance: &str) -> Result<Vec<OwnChange>, String> {
        Ok(self
            .lock()
            .own_changes
            .iter()
            .filter(|change| change.subject == subject && change.instance == instance)
            .cloned()
            .collect())
    }
}

// ── In Postgres ─────────────────────────────────────────────────────────────

pub struct InPostgres {
    database: Database,
}

impl InPostgres {
    /// On a database already verified at start.
    pub fn on(database: Database) -> Self {
        Self { database }
    }
}

const KEPT_COLUMNS: &str = "prepared_call_id, tool_name, instance, arguments_json, \
     preparer_subject, preparer_name, through_delegation, client_name, reason, reason_suspect, \
     state, accounts, checked, prepared_at_ns, settled_at_ns";

fn kept_of(row: &postgres::Row) -> Kept {
    Kept {
        prepared_call_id: row.get(0),
        tool_name: row.get(1),
        instance: row.get(2),
        arguments_json: row.get(3),
        preparer_subject: row.get(4),
        preparer_name: row.get(5),
        through_delegation: row.get(6),
        client_name: row.get(7),
        reason: row.get(8),
        reason_suspect: row.get(9),
        state: i32::from(row.get::<_, i16>(10)),
        accounts: row.get(11),
        checked: row.get(12),
        prepared_at_ns: row.get(13),
        settled_at_ns: row.get(14),
    }
}

fn insert_step(client: &mut impl postgres::GenericClient, step: &Step) -> Result<(), String> {
    client
        .execute(
            "INSERT INTO dashboard_prepared_call_change
                    (prepared_call_id, kind, by_subject, through_delegation, client_name, at_ns, note)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            &[
                &step.prepared_call_id,
                &(step.kind as i16),
                &step.by_subject,
                &step.through_delegation,
                &step.client_name,
                &step.at_ns,
                &step.note,
            ],
        )
        .map_err(said)?;
    Ok(())
}

fn insert_notices(
    client: &mut impl postgres::GenericClient,
    notices: &[PreparedNotice],
) -> Result<(), String> {
    for notice in notices {
        client
            .execute(
                "INSERT INTO dashboard_notice (notice_id, subject, ticket_id, prepared_call_id, kind,
                     author_provenance, author_subject, author_person, author_delegation,
                     author_client, changed_ns, read)
                 VALUES ($1, $2, NULL, $3, $4, $5, $6, $7, $8, $9, $10, false)",
                &[
                    &notice.notice_id,
                    &notice.subject,
                    &notice.prepared_call_id,
                    &notice.kind,
                    &if notice.through_delegation.is_empty() {
                        "person"
                    } else {
                        "client"
                    },
                    &notice.by_subject,
                    &notice.by_name,
                    &notice.through_delegation,
                    &notice.client_name,
                    &notice.changed_ns,
                ],
            )
            .map_err(said)?;
    }
    Ok(())
}

fn level_change_of(row: &postgres::Row) -> LevelChange {
    LevelChange {
        act: row.get(0),
        scope: row.get(1),
        from: row.get(2),
        to: row.get(3),
        by: row.get(4),
        delegation: row.get(5),
        client: row.get(6),
        at_ns: row.get(7),
        note: row.get(8),
    }
}

fn insert_level_change(
    client: &mut impl postgres::GenericClient,
    change: &LevelChange,
) -> Result<(), String> {
    client
        .execute(
            "INSERT INTO dashboard_agents_may_change
                    (act, scope, from_said, to_said, changed_by, through_delegation, client_name,
                     changed_at_ns, note)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            &[
                &change.act,
                &change.scope,
                &change.from,
                &change.to,
                &change.by,
                &change.delegation,
                &change.client,
                &change.at_ns,
                &change.note,
            ],
        )
        .map_err(said)?;
    Ok(())
}

impl PreparedStore for InPostgres {
    fn insert(&self, kept: &Kept, step: &Step, notices: &[PreparedNotice]) -> Result<(), String> {
        let mut conn = self.database.conn()?;
        let mut tx = conn.transaction().map_err(said)?;
        let state = kept.state as i16;
        tx.execute(
            &format!(
                "INSERT INTO dashboard_prepared_call ({KEPT_COLUMNS})
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)"
            ),
            &[
                &kept.prepared_call_id,
                &kept.tool_name,
                &kept.instance,
                &kept.arguments_json,
                &kept.preparer_subject,
                &kept.preparer_name,
                &kept.through_delegation,
                &kept.client_name,
                &kept.reason,
                &kept.reason_suspect,
                &state,
                &kept.accounts,
                &kept.checked,
                &kept.prepared_at_ns,
                &kept.settled_at_ns,
            ],
        )
        .map_err(said)?;
        insert_step(&mut tx, step)?;
        insert_notices(&mut tx, notices)?;
        tx.commit().map_err(said)
    }

    fn get(&self, id: &str) -> Result<Option<Kept>, String> {
        Ok(self
            .database
            .conn()?
            .query_opt(
                &format!(
                    "SELECT {KEPT_COLUMNS} FROM dashboard_prepared_call WHERE prepared_call_id = $1"
                ),
                &[&id],
            )
            .map_err(said)?
            .as_ref()
            .map(kept_of))
    }

    fn pending(&self) -> Result<Vec<Kept>, String> {
        Ok(self
            .database
            .conn()?
            .query(
                &format!(
                    "SELECT {KEPT_COLUMNS} FROM dashboard_prepared_call WHERE state IN (1, 2)
                      ORDER BY prepared_at_ns"
                ),
                &[],
            )
            .map_err(said)?
            .iter()
            .map(kept_of)
            .collect())
    }

    fn by_delegation(&self, delegation: &str, limit: usize) -> Result<Vec<Kept>, String> {
        let limit = limit as i64;
        Ok(self
            .database
            .conn()?
            .query(
                &format!(
                    "SELECT {KEPT_COLUMNS} FROM dashboard_prepared_call
                      WHERE through_delegation = $1 ORDER BY prepared_at_ns DESC LIMIT $2"
                ),
                &[&delegation, &limit],
            )
            .map_err(said)?
            .iter()
            .map(kept_of)
            .collect())
    }

    fn transition(
        &self,
        id: &str,
        from: &[i32],
        to: i32,
        step: &Step,
        notices: &[PreparedNotice],
    ) -> Result<bool, String> {
        let mut conn = self.database.conn()?;
        let mut tx = conn.transaction().map_err(said)?;
        let from: Vec<i16> = from.iter().map(|state| *state as i16).collect();
        let to_state = to as i16;
        let settled = (!PENDING.contains(&to)).then_some(step.at_ns);
        let moved = tx
            .execute(
                "UPDATE dashboard_prepared_call
                    SET state = $3, settled_at_ns = COALESCE($4, settled_at_ns)
                  WHERE prepared_call_id = $1 AND state = ANY($2)",
                &[&id, &from, &to_state, &settled],
            )
            .map_err(said)?;
        if moved == 0 {
            return Ok(false);
        }
        insert_step(&mut tx, step)?;
        insert_notices(&mut tx, notices)?;
        tx.commit().map_err(said)?;
        Ok(true)
    }

    fn note_step(&self, step: &Step) -> Result<(), String> {
        let mut conn = self.database.conn()?;
        insert_step(&mut *conn, step)
    }

    fn steps(&self, id: &str) -> Result<Vec<Step>, String> {
        Ok(self
            .database
            .conn()?
            .query(
                "SELECT kind, by_subject, through_delegation, client_name, at_ns, note
                   FROM dashboard_prepared_call_change WHERE prepared_call_id = $1
                  ORDER BY change_id",
                &[&id],
            )
            .map_err(said)?
            .iter()
            .filter_map(|row| {
                Some(Step {
                    prepared_call_id: id.to_string(),
                    kind: StepKind::from_code(row.get(0))?,
                    by_subject: row.get(1),
                    through_delegation: row.get(2),
                    client_name: row.get(3),
                    at_ns: row.get(4),
                    note: row.get(5),
                })
            })
            .collect())
    }

    fn notices(&self, subject: &str, limit: usize) -> Result<Vec<PreparedNotice>, String> {
        let limit = limit as i64;
        Ok(self
            .database
            .conn()?
            .query(
                "SELECT notice_id, subject, prepared_call_id, kind, author_subject, author_person,
                        author_delegation, author_client, changed_ns, read
                   FROM dashboard_notice
                  WHERE subject = $1 AND prepared_call_id <> ''
                  ORDER BY notice_id DESC LIMIT $2",
                &[&subject, &limit],
            )
            .map_err(said)?
            .iter()
            .map(|row| PreparedNotice {
                notice_id: row.get(0),
                subject: row.get(1),
                prepared_call_id: row.get(2),
                kind: row.get(3),
                by_subject: row.get(4),
                by_name: row.get(5),
                through_delegation: row.get(6),
                client_name: row.get(7),
                changed_ns: row.get(8),
                read: row.get(9),
            })
            .collect())
    }

    fn mark_notices_read(&self, subject: &str, ids: &[String]) -> Result<usize, String> {
        let marked = self
            .database
            .conn()?
            .execute(
                "UPDATE dashboard_notice SET read = true
                  WHERE subject = $1 AND prepared_call_id = ANY($2) AND NOT read",
                &[&subject, &ids],
            )
            .map_err(said)?;
        Ok(marked as usize)
    }

    fn sweep(&self, before_ns: i64) -> Result<usize, String> {
        let mut conn = self.database.conn()?;
        let mut tx = conn.transaction().map_err(said)?;
        let gone: Vec<String> = tx
            .query(
                "SELECT prepared_call_id FROM dashboard_prepared_call
                  WHERE settled_at_ns IS NOT NULL AND settled_at_ns < $1",
                &[&before_ns],
            )
            .map_err(said)?
            .iter()
            .map(|row| row.get(0))
            .collect();
        if gone.is_empty() {
            return Ok(0);
        }
        for table in [
            "dashboard_notice",
            "dashboard_prepared_call_change",
            "dashboard_prepared_call",
        ] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE prepared_call_id = ANY($1)"),
                &[&gone],
            )
            .map_err(said)?;
        }
        tx.commit().map_err(said)?;
        Ok(gone.len())
    }

    fn levels(&self) -> Result<Vec<LevelRow>, String> {
        Ok(self
            .database
            .conn()?
            .query(
                "SELECT act, scope, level, second_pair_of_eyes FROM dashboard_agents_may
                  ORDER BY act, scope",
                &[],
            )
            .map_err(said)?
            .iter()
            .map(|row| LevelRow {
                act: row.get(0),
                scope: row.get(1),
                level: row.get(2),
                second_pair_of_eyes: row.get(3),
            })
            .collect())
    }

    fn set_level(
        &self,
        act: &str,
        scope: &str,
        row: Option<&LevelRow>,
        change: &LevelChange,
    ) -> Result<(), String> {
        let mut conn = self.database.conn()?;
        let mut tx = conn.transaction().map_err(said)?;
        match row {
            Some(row) => {
                tx.execute(
                    "INSERT INTO dashboard_agents_may (act, scope, level, second_pair_of_eyes)
                     VALUES ($1, $2, $3, $4)
                     ON CONFLICT (act, scope) DO UPDATE
                        SET level = excluded.level,
                            second_pair_of_eyes = excluded.second_pair_of_eyes",
                    &[&act, &scope, &row.level, &row.second_pair_of_eyes],
                )
                .map_err(said)?;
            }
            None => {
                tx.execute(
                    "DELETE FROM dashboard_agents_may WHERE act = $1 AND scope = $2",
                    &[&act, &scope],
                )
                .map_err(said)?;
            }
        }
        insert_level_change(&mut tx, change)?;
        tx.commit().map_err(said)
    }

    fn level_changes(&self, act: &str) -> Result<Vec<LevelChange>, String> {
        Ok(self
            .database
            .conn()?
            .query(
                "SELECT act, scope, from_said, to_said, changed_by, through_delegation,
                        client_name, changed_at_ns, note
                   FROM dashboard_agents_may_change WHERE act = $1 ORDER BY change_id",
                &[&act],
            )
            .map_err(said)?
            .iter()
            .map(level_change_of)
            .collect())
    }

    fn setting(&self, name: &str) -> Result<Option<String>, String> {
        Ok(self
            .database
            .conn()?
            .query_opt(
                "SELECT value FROM dashboard_deployment_setting WHERE name = $1",
                &[&name],
            )
            .map_err(said)?
            .map(|row| row.get(0)))
    }

    fn set_setting(&self, name: &str, value: &str, change: &LevelChange) -> Result<(), String> {
        let mut conn = self.database.conn()?;
        let mut tx = conn.transaction().map_err(said)?;
        tx.execute(
            "INSERT INTO dashboard_deployment_setting (name, value) VALUES ($1, $2)
             ON CONFLICT (name) DO UPDATE SET value = excluded.value",
            &[&name, &value],
        )
        .map_err(said)?;
        insert_level_change(&mut tx, change)?;
        tx.commit().map_err(said)
    }

    fn own(&self, instance: &str, subject: &str) -> Result<BTreeMap<String, String>, String> {
        Ok(self
            .database
            .conn()?
            .query(
                "SELECT name, value FROM dashboard_own_setting WHERE instance = $1 AND subject = $2",
                &[&instance, &subject],
            )
            .map_err(said)?
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect())
    }

    fn set_own(&self, changes: &[OwnChange]) -> Result<(), String> {
        let mut conn = self.database.conn()?;
        let mut tx = conn.transaction().map_err(said)?;
        for change in changes {
            if change.value.is_empty() {
                tx.execute(
                    "DELETE FROM dashboard_own_setting
                      WHERE instance = $1 AND subject = $2 AND name = $3",
                    &[&change.instance, &change.subject, &change.name],
                )
                .map_err(said)?;
            } else {
                tx.execute(
                    "INSERT INTO dashboard_own_setting (instance, subject, name, value)
                     VALUES ($1, $2, $3, $4)
                     ON CONFLICT (instance, subject, name) DO UPDATE SET value = excluded.value",
                    &[
                        &change.instance,
                        &change.subject,
                        &change.name,
                        &change.value,
                    ],
                )
                .map_err(said)?;
            }
            tx.execute(
                "INSERT INTO dashboard_own_setting_change
                        (instance, subject, name, value, through_delegation, client_name,
                         changed_at_ns)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
                &[
                    &change.instance,
                    &change.subject,
                    &change.name,
                    &change.value,
                    &change.delegation,
                    &change.client,
                    &change.at_ns,
                ],
            )
            .map_err(said)?;
        }
        tx.commit().map_err(said)
    }

    fn own_changes(&self, subject: &str, instance: &str) -> Result<Vec<OwnChange>, String> {
        Ok(self
            .database
            .conn()?
            .query(
                "SELECT name, value, through_delegation, client_name, changed_at_ns
                   FROM dashboard_own_setting_change WHERE subject = $1 AND instance = $2
                  ORDER BY change_id",
                &[&subject, &instance],
            )
            .map_err(said)?
            .iter()
            .map(|row| OwnChange {
                instance: instance.to_string(),
                subject: subject.to_string(),
                name: row.get(0),
                value: row.get(1),
                delegation: row.get(2),
                client: row.get(3),
                at_ns: row.get(4),
            })
            .collect())
    }
}
