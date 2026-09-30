//! Where terminal sessions are kept: the dashboard's own table in the
//! deployment's database, so a restart or an upgrade leaves them standing
//! (W6.13, ruled 2026-09-30), or this process's memory for tests and for a
//! dashboard given no database.
//!
//! Keyed by [`super::hashed`] and never the token. A session that lapses or
//! is ended leaves a row naming why until it would have lapsed anyway, so a
//! refusal can say `lapsed` or `ended` on whichever replica meets it, after
//! whatever restart.
//!
//! The bounds are decisions/015's, passed in rather than written in a
//! query, so both stores enforce the same two numbers.

use std::collections::HashMap;
use std::sync::Mutex;

use super::{Person, Refusal};
use crate::database::{said, Database};
use crate::session::{ABSOLUTE_NS, IDLE_NS};

/// A store for terminal sessions. Blocking: [`super::Terminals`] calls it off
/// the async runtime.
pub trait TerminalSessions: Send + Sync {
    /// Keep a session just issued, used for the first time now.
    fn keep(&self, key: &str, person: &Person, now_ns: i64) -> Result<(), String>;

    /// The person behind a live session, which this touches; or why not. A
    /// session found past a bound is removed, leaving its reason.
    fn find(&self, key: &str, now_ns: i64) -> Result<Result<Person, Refusal>, String>;

    /// Whether a session is live, without touching it.
    fn is_live(&self, key: &str, now_ns: i64) -> Result<bool, String>;

    /// End one session, if it is held. Ending one already gone is nothing.
    fn end(&self, key: &str) -> Result<(), String>;

    /// End every session a person holds, and say how many.
    fn end_person(&self, subject: &str) -> Result<usize, String>;

    /// Who holds a live session, by subject, with a name and a count, sorted.
    fn holders(&self, now_ns: i64) -> Result<Vec<(String, String, usize)>, String>;

    /// Remove every session past a bound, leaving its reason, and every
    /// reason past the moment its session would have lapsed anyway.
    fn sweep(&self, now_ns: i64) -> Result<(), String>;
}

fn live(signed_in_at_ns: i64, last_seen_at_ns: i64, now_ns: i64) -> bool {
    now_ns - last_seen_at_ns <= IDLE_NS && now_ns - signed_in_at_ns <= ABSOLUTE_NS
}

// ── In memory ────────────────────────────────────────────────────────────────

struct Held {
    person: Person,
    last_seen_at_ns: i64,
}

#[derive(Default)]
struct Kept {
    sessions: HashMap<String, Held>,
    /// Why a session ended, until it would have lapsed anyway.
    gone: HashMap<String, (Refusal, i64)>,
}

impl Kept {
    fn leave(&mut self, key: String, why: Refusal) {
        if let Some(held) = self.sessions.remove(&key) {
            let until = held.person.signed_in_at_ns + ABSOLUTE_NS;
            self.gone.insert(key, (why, until));
        }
    }
}

/// For tests, and for a dashboard given no database, where a restart ends
/// every terminal session as it ends every browser's.
#[derive(Default)]
pub struct InMemory {
    kept: Mutex<Kept>,
}

impl InMemory {
    fn lock(&self) -> std::sync::MutexGuard<'_, Kept> {
        self.kept.lock().expect("terminal sessions lock poisoned")
    }

    #[cfg(test)]
    pub(crate) fn keys(&self) -> (Vec<String>, Vec<String>) {
        let kept = self.lock();
        (
            kept.sessions.keys().cloned().collect(),
            kept.gone.keys().cloned().collect(),
        )
    }
}

impl TerminalSessions for InMemory {
    fn keep(&self, key: &str, person: &Person, now_ns: i64) -> Result<(), String> {
        self.lock().sessions.insert(
            key.to_string(),
            Held {
                person: person.clone(),
                last_seen_at_ns: now_ns,
            },
        );
        Ok(())
    }

    fn find(&self, key: &str, now_ns: i64) -> Result<Result<Person, Refusal>, String> {
        let mut kept = self.lock();
        if let Some(held) = kept.sessions.get_mut(key) {
            if live(held.person.signed_in_at_ns, held.last_seen_at_ns, now_ns) {
                held.last_seen_at_ns = now_ns;
                return Ok(Ok(held.person.clone()));
            }
            kept.leave(key.to_string(), Refusal::Lapsed);
            return Ok(Err(Refusal::Lapsed));
        }
        Ok(Err(kept
            .gone
            .get(key)
            .map(|(why, _)| *why)
            .unwrap_or(Refusal::Unknown)))
    }

    fn is_live(&self, key: &str, now_ns: i64) -> Result<bool, String> {
        Ok(self
            .lock()
            .sessions
            .get(key)
            .is_some_and(|held| live(held.person.signed_in_at_ns, held.last_seen_at_ns, now_ns)))
    }

    fn end(&self, key: &str) -> Result<(), String> {
        self.lock().leave(key.to_string(), Refusal::Ended);
        Ok(())
    }

    fn end_person(&self, subject: &str) -> Result<usize, String> {
        let mut kept = self.lock();
        let theirs: Vec<String> = kept
            .sessions
            .iter()
            .filter(|(_, held)| held.person.subject == subject)
            .map(|(key, _)| key.clone())
            .collect();
        let ended = theirs.len();
        for key in theirs {
            kept.leave(key, Refusal::Ended);
        }
        Ok(ended)
    }

    fn holders(&self, now_ns: i64) -> Result<Vec<(String, String, usize)>, String> {
        let kept = self.lock();
        let mut counted: HashMap<&str, (&str, usize)> = HashMap::new();
        for held in kept.sessions.values() {
            if live(held.person.signed_in_at_ns, held.last_seen_at_ns, now_ns) {
                let entry = counted
                    .entry(held.person.subject.as_str())
                    .or_insert((held.person.display_name.as_str(), 0));
                // The same name Postgres picks, whatever order these came in.
                entry.0 = entry.0.min(held.person.display_name.as_str());
                entry.1 += 1;
            }
        }
        let mut holders: Vec<_> = counted
            .into_iter()
            .map(|(subject, (name, n))| (subject.to_string(), name.to_string(), n))
            .collect();
        holders.sort();
        Ok(holders)
    }

    fn sweep(&self, now_ns: i64) -> Result<(), String> {
        let mut kept = self.lock();
        let lapsed: Vec<String> = kept
            .sessions
            .iter()
            .filter(|(_, held)| !live(held.person.signed_in_at_ns, held.last_seen_at_ns, now_ns))
            .map(|(key, _)| key.clone())
            .collect();
        for key in lapsed {
            kept.leave(key, Refusal::Lapsed);
        }
        kept.gone.retain(|_, (_, until)| now_ns <= *until);
        Ok(())
    }
}

// ── In Postgres ──────────────────────────────────────────────────────────────

/// Terminal sessions in `dashboard_terminal_session`, and why each ended in
/// `dashboard_terminal_session_gone`. Every change is one statement, so two
/// replicas -- or a request and the sweep -- cannot interleave inside one.
pub struct InPostgres {
    database: Database,
}

impl InPostgres {
    /// On a database already verified at start.
    pub fn on(database: Database) -> Self {
        Self { database }
    }
}

/// Where a session is live, in the terms `$now`, `$idle` and `$absolute`
/// name by position. Written once so every query means the same thing.
macro_rules! live_at {
    ($now:literal, $idle:literal, $absolute:literal) => {
        concat!(
            "(",
            $now,
            "::bigint - last_seen_at_ns <= ",
            $idle,
            "::bigint AND ",
            $now,
            "::bigint - signed_in_at_ns <= ",
            $absolute,
            "::bigint)"
        )
    };
}

/// Move the sessions a `DELETE ... WHERE` names into the reasons, with why:
/// one statement, so a session is never in neither table.
macro_rules! leave_where {
    ($reason:literal, $until:literal, $($where:tt)+) => {
        concat!(
            "WITH left_now AS (
                 DELETE FROM dashboard_terminal_session WHERE ",
            $($where)+,
            " RETURNING session_hash, signed_in_at_ns)
             INSERT INTO dashboard_terminal_session_gone (session_hash, reason, until_ns)
             SELECT session_hash, '",
            $reason,
            "', signed_in_at_ns + ",
            $until,
            "::bigint FROM left_now
             ON CONFLICT (session_hash) DO UPDATE
                SET reason = excluded.reason, until_ns = excluded.until_ns"
        )
    };
}

impl TerminalSessions for InPostgres {
    fn keep(&self, key: &str, person: &Person, now_ns: i64) -> Result<(), String> {
        self.database
            .conn()?
            .execute(
                "INSERT INTO dashboard_terminal_session
                     (session_hash, subject, display_name, directory_groups,
                      signed_in_at_ns, last_seen_at_ns)
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &[
                    &key,
                    &person.subject,
                    &person.display_name,
                    &person.directory_groups,
                    &person.signed_in_at_ns,
                    &now_ns,
                ],
            )
            .map_err(said)?;
        Ok(())
    }

    fn find(&self, key: &str, now_ns: i64) -> Result<Result<Person, Refusal>, String> {
        let mut conn = self.database.conn()?;
        // Touched where live. GREATEST, so a replica whose clock is a little
        // behind another's never moves a session's last use backwards.
        let touched = conn
            .query_opt(
                concat!(
                    "UPDATE dashboard_terminal_session
                        SET last_seen_at_ns = GREATEST(last_seen_at_ns, $2::bigint)
                      WHERE session_hash = $1 AND ",
                    live_at!("$2", "$3", "$4"),
                    " RETURNING subject, display_name, directory_groups, signed_in_at_ns"
                ),
                &[&key, &now_ns, &IDLE_NS, &ABSOLUTE_NS],
            )
            .map_err(said)?;
        if let Some(row) = touched {
            return Ok(Ok(Person {
                subject: row.get(0),
                display_name: row.get(1),
                directory_groups: row.get(2),
                signed_in_at_ns: row.get(3),
            }));
        }
        // Past a bound: removed now, on use, rather than left for the sweep.
        let lapsed = conn
            .execute(
                leave_where!(
                    "lapsed",
                    "$5",
                    concat!("session_hash = $1 AND NOT ", live_at!("$2", "$3", "$4"))
                ),
                &[&key, &now_ns, &IDLE_NS, &ABSOLUTE_NS, &ABSOLUTE_NS],
            )
            .map_err(said)?;
        if lapsed > 0 {
            return Ok(Err(Refusal::Lapsed));
        }
        let why: Option<String> = conn
            .query_opt(
                "SELECT reason FROM dashboard_terminal_session_gone WHERE session_hash = $1",
                &[&key],
            )
            .map_err(said)?
            .map(|row| row.get(0));
        Ok(Err(match why.as_deref() {
            Some("lapsed") => Refusal::Lapsed,
            Some("ended") => Refusal::Ended,
            _ => Refusal::Unknown,
        }))
    }

    fn is_live(&self, key: &str, now_ns: i64) -> Result<bool, String> {
        Ok(self
            .database
            .conn()?
            .query_one(
                concat!(
                    "SELECT EXISTS (SELECT 1 FROM dashboard_terminal_session
                                     WHERE session_hash = $1 AND ",
                    live_at!("$2", "$3", "$4"),
                    ")"
                ),
                &[&key, &now_ns, &IDLE_NS, &ABSOLUTE_NS],
            )
            .map_err(said)?
            .get(0))
    }

    fn end(&self, key: &str) -> Result<(), String> {
        self.database
            .conn()?
            .execute(
                leave_where!("ended", "$2", "session_hash = $1"),
                &[&key, &ABSOLUTE_NS],
            )
            .map_err(said)?;
        Ok(())
    }

    fn end_person(&self, subject: &str) -> Result<usize, String> {
        let ended = self
            .database
            .conn()?
            .execute(
                leave_where!("ended", "$2", "subject = $1"),
                &[&subject, &ABSOLUTE_NS],
            )
            .map_err(said)?;
        Ok(ended as usize)
    }

    fn holders(&self, now_ns: i64) -> Result<Vec<(String, String, usize)>, String> {
        let rows = self
            .database
            .conn()?
            .query(
                concat!(
                    "SELECT subject, min(display_name), count(*)
                       FROM dashboard_terminal_session
                      WHERE ",
                    live_at!("$1", "$2", "$3"),
                    " GROUP BY subject ORDER BY subject"
                ),
                &[&now_ns, &IDLE_NS, &ABSOLUTE_NS],
            )
            .map_err(said)?;
        Ok(rows
            .iter()
            .map(|row| {
                let n: i64 = row.get(2);
                (row.get(0), row.get(1), n as usize)
            })
            .collect())
    }

    fn sweep(&self, now_ns: i64) -> Result<(), String> {
        let mut conn = self.database.conn()?;
        conn.execute(
            leave_where!("lapsed", "$4", concat!("NOT ", live_at!("$1", "$2", "$3"))),
            &[&now_ns, &IDLE_NS, &ABSOLUTE_NS, &ABSOLUTE_NS],
        )
        .map_err(said)?;
        conn.execute(
            "DELETE FROM dashboard_terminal_session_gone WHERE until_ns < $1::bigint",
            &[&now_ns],
        )
        .map_err(said)?;
        Ok(())
    }
}
