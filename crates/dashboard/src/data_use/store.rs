//! What the dashboard keeps of data use (contract v28), in memory for tests
//! and a deployment with no database, and in its own tables
//! (migrations/0011_data_use.sql): each person's answer to the professional
//! test with each answer's own record, and who could use each dataset in
//! each month. Records are never updated nor deleted; what stands now is
//! kept beside them in one step.

use std::collections::BTreeMap;
use std::sync::Mutex;

use super::{Kind, Status};
use crate::database::{said, Database};

/// An answer that stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Standing {
    pub subject: String,
    pub test: String,
    pub status: Status,
    pub set_at_ns: i64,
}

/// An answer, or a change of one: its own record, `professional_status_set`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusChange {
    pub subject: String,
    pub test: String,
    /// The answer it replaced; none for a first answer.
    pub from: Option<Status>,
    pub to: Status,
    /// Who set it: the person, or a deployment admin.
    pub by: String,
    pub through_delegation: String,
    pub client_name: String,
    pub at_ns: i64,
    pub note: String,
}

/// One who could use one dataset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub dataset: String,
    pub kind: Kind,
    /// A person's or service account's subject, or a plugin instance.
    pub who: String,
    /// A person's professional status where the terms split, a plugin's
    /// category; empty otherwise.
    pub classification: String,
    pub underlying_source: String,
}

/// One seen in a month, and when first and last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenRow {
    pub seen: Seen,
    pub first_seen_ns: i64,
    pub last_seen_ns: i64,
}

/// Where data use is kept.
pub trait DataUseStore: Send + Sync {
    /// Every answer standing.
    fn statuses(&self) -> Result<Vec<Standing>, String>;
    /// Keeps an answer: what stands, and its record, in one step.
    fn set_status(&self, change: &StatusChange) -> Result<(), String>;
    /// A person's answers' records, newest first.
    fn status_changes(&self, subject: &str) -> Result<Vec<StatusChange>, String>;
    /// Notes each seen at `at_ns` in `month`: first seen if new, last seen
    /// and its classification otherwise.
    fn note(&self, month: &str, seen: &[Seen], at_ns: i64) -> Result<(), String>;
    /// A month's rows.
    fn month(&self, month: &str) -> Result<Vec<SeenRow>, String>;
    /// Every month kept.
    fn months(&self) -> Result<Vec<String>, String>;
}

#[derive(Default)]
pub struct InMemory {
    standing: Mutex<BTreeMap<(String, String), Standing>>,
    changes: Mutex<Vec<StatusChange>>,
    seen: Mutex<BTreeMap<(String, String, Kind, String), SeenRow>>,
}

fn locked<T>(mutex: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>, String> {
    mutex
        .lock()
        .map_err(|_| "the data-use store's lock".to_string())
}

impl DataUseStore for InMemory {
    fn statuses(&self) -> Result<Vec<Standing>, String> {
        Ok(locked(&self.standing)?.values().cloned().collect())
    }

    fn set_status(&self, change: &StatusChange) -> Result<(), String> {
        locked(&self.standing)?.insert(
            (change.subject.clone(), change.test.clone()),
            Standing {
                subject: change.subject.clone(),
                test: change.test.clone(),
                status: change.to,
                set_at_ns: change.at_ns,
            },
        );
        locked(&self.changes)?.push(change.clone());
        Ok(())
    }

    fn status_changes(&self, subject: &str) -> Result<Vec<StatusChange>, String> {
        let mut found: Vec<StatusChange> = locked(&self.changes)?
            .iter()
            .filter(|c| c.subject == subject)
            .cloned()
            .collect();
        found.reverse();
        Ok(found)
    }

    fn note(&self, month: &str, seen: &[Seen], at_ns: i64) -> Result<(), String> {
        let mut held = locked(&self.seen)?;
        for one in seen {
            let key = (
                month.to_string(),
                one.dataset.clone(),
                one.kind,
                one.who.clone(),
            );
            held.entry(key)
                .and_modify(|row| {
                    row.last_seen_ns = row.last_seen_ns.max(at_ns);
                    row.seen.classification = one.classification.clone();
                    row.seen.underlying_source = one.underlying_source.clone();
                })
                .or_insert_with(|| SeenRow {
                    seen: one.clone(),
                    first_seen_ns: at_ns,
                    last_seen_ns: at_ns,
                });
        }
        Ok(())
    }

    fn month(&self, month: &str) -> Result<Vec<SeenRow>, String> {
        Ok(locked(&self.seen)?
            .iter()
            .filter(|((m, ..), _)| m == month)
            .map(|(_, row)| row.clone())
            .collect())
    }

    fn months(&self) -> Result<Vec<String>, String> {
        let mut months: Vec<String> = locked(&self.seen)?
            .keys()
            .map(|(m, ..)| m.clone())
            .collect();
        months.dedup();
        Ok(months)
    }
}

pub struct InPostgres {
    database: Database,
}

impl InPostgres {
    pub fn on(database: Database) -> Self {
        Self { database }
    }
}

fn status_of(said: &str) -> Result<Status, String> {
    Status::parse(said)
        .ok_or_else(|| format!("a professional status the dashboard does not know: {said:?}"))
}

impl DataUseStore for InPostgres {
    fn statuses(&self) -> Result<Vec<Standing>, String> {
        let mut conn = self.database.conn()?;
        let rows = conn
            .query(
                "SELECT subject, test, status, set_at_ns FROM dashboard_professional_status",
                &[],
            )
            .map_err(said)?;
        rows.iter()
            .map(|row| {
                Ok(Standing {
                    subject: row.get(0),
                    test: row.get(1),
                    status: status_of(row.get(2))?,
                    set_at_ns: row.get(3),
                })
            })
            .collect()
    }

    fn set_status(&self, change: &StatusChange) -> Result<(), String> {
        let mut conn = self.database.conn()?;
        let mut tx = conn.transaction().map_err(said)?;
        tx.execute(
            "INSERT INTO dashboard_professional_status (subject, test, status, set_at_ns)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (subject, test) DO UPDATE SET status = $3, set_at_ns = $4",
            &[
                &change.subject,
                &change.test,
                &change.to.value(),
                &change.at_ns,
            ],
        )
        .map_err(said)?;
        tx.execute(
            "INSERT INTO dashboard_professional_status_change (subject, test, from_said, to_said,
                 changed_by, through_delegation, client_name, changed_at_ns, note)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            &[
                &change.subject,
                &change.test,
                &change.from.map(Status::value).unwrap_or_default(),
                &change.to.value(),
                &change.by,
                &change.through_delegation,
                &change.client_name,
                &change.at_ns,
                &change.note,
            ],
        )
        .map_err(said)?;
        tx.commit().map_err(said)
    }

    fn status_changes(&self, subject: &str) -> Result<Vec<StatusChange>, String> {
        let mut conn = self.database.conn()?;
        let rows = conn
            .query(
                "SELECT subject, test, from_said, to_said, changed_by, through_delegation,
                        client_name, changed_at_ns, note
                   FROM dashboard_professional_status_change
                  WHERE subject = $1 ORDER BY change_id DESC",
                &[&subject],
            )
            .map_err(said)?;
        rows.iter()
            .map(|row| {
                let from: String = row.get(2);
                Ok(StatusChange {
                    subject: row.get(0),
                    test: row.get(1),
                    from: Status::parse(&from),
                    to: status_of(row.get(3))?,
                    by: row.get(4),
                    through_delegation: row.get(5),
                    client_name: row.get(6),
                    at_ns: row.get(7),
                    note: row.get(8),
                })
            })
            .collect()
    }

    fn note(&self, month: &str, seen: &[Seen], at_ns: i64) -> Result<(), String> {
        let mut conn = self.database.conn()?;
        let mut tx = conn.transaction().map_err(said)?;
        for one in seen {
            tx.execute(
                "INSERT INTO dashboard_data_use (month, dataset, kind, who, classification,
                     underlying_source, first_seen_ns, last_seen_ns)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $7)
                 ON CONFLICT (month, dataset, kind, who) DO UPDATE
                    SET classification = $5, underlying_source = $6,
                        last_seen_ns = GREATEST(dashboard_data_use.last_seen_ns, $7)",
                &[
                    &month,
                    &one.dataset,
                    &one.kind.value(),
                    &one.who,
                    &one.classification,
                    &one.underlying_source,
                    &at_ns,
                ],
            )
            .map_err(said)?;
        }
        tx.commit().map_err(said)
    }

    fn month(&self, month: &str) -> Result<Vec<SeenRow>, String> {
        let mut conn = self.database.conn()?;
        let rows = conn
            .query(
                "SELECT dataset, kind, who, classification, underlying_source, first_seen_ns,
                        last_seen_ns
                   FROM dashboard_data_use WHERE month = $1
                  ORDER BY dataset, kind, who",
                &[&month],
            )
            .map_err(said)?;
        rows.iter()
            .map(|row| {
                let kind: String = row.get(1);
                Ok(SeenRow {
                    seen: Seen {
                        dataset: row.get(0),
                        kind: Kind::parse(&kind).ok_or_else(|| {
                            format!("a kind of data use the dashboard does not know: {kind:?}")
                        })?,
                        who: row.get(2),
                        classification: row.get(3),
                        underlying_source: row.get(4),
                    },
                    first_seen_ns: row.get(5),
                    last_seen_ns: row.get(6),
                })
            })
            .collect()
    }

    fn months(&self) -> Result<Vec<String>, String> {
        let mut conn = self.database.conn()?;
        let rows = conn
            .query(
                "SELECT DISTINCT month FROM dashboard_data_use ORDER BY month",
                &[],
            )
            .map_err(said)?;
        Ok(rows.iter().map(|row| row.get(0)).collect())
    }
}
