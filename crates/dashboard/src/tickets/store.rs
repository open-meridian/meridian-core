//! Where tickets, their notes, notices and inbox places are kept: the
//! dashboard's own tables (migration 0005), or this process's memory, for
//! tests and for a dashboard given no database.
//!
//! Notes are insert-only, and every change is one transaction with the note
//! of kind change recording it, on the ticket's row locked, so a person's
//! two acts, or an act and a note, cannot interleave. A repeat folds into
//! the open ticket from the same instance under the same key, held by a
//! unique index rather than by asking first.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use super::{Author, Change, Note, Notice, Provenance, Reference, State, Subject, Ticket};
use crate::database::{said, Database};

/// What filing a new ticket came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inserted {
    Made,
    /// An open ticket from the same instance already holds the key: the
    /// filing is a repeat, to be folded into it.
    KeyHeld(String),
}

/// A store for tickets. Blocking: [`super::Tickets`] calls it off the async
/// runtime.
pub trait TicketStore: Send + Sync {
    /// Keep a new ticket, with the notes it was filed with (the rules'
    /// advice) and the notices that tell people of it.
    fn insert(&self, ticket: &Ticket, notices: &[Notice]) -> Result<Inserted, String>;

    /// A repeat: the seen count and last seen, and the seen text with its
    /// quarantine where it changed. The count after.
    fn fold(
        &self,
        ticket_id: &str,
        seen: Option<(&str, bool, &[String])>,
        now_ns: i64,
    ) -> Result<i64, String>;

    /// The tickets one instance filed under one key, newest first, without
    /// their notes.
    fn by_key(&self, instance: &str, key: &str) -> Result<Vec<Ticket>, String>;

    /// One ticket, with its notes.
    fn ticket(&self, ticket_id: &str) -> Result<Option<Ticket>, String>;

    /// Every ticket, without notes, newest first: who may see each is
    /// computed per read, from the access fold.
    fn tickets(&self) -> Result<Vec<Ticket>, String>;

    /// How many tickets a person filed, at a page or through a client, since
    /// `since_ns`: their rate (requirement 60).
    fn filed_since(&self, subject: &str, since_ns: i64) -> Result<usize, String>;

    /// Append a note, numbered after the last, and its notices. Its number.
    fn add_note(&self, ticket_id: &str, note: &Note, notices: &[Notice]) -> Result<i32, String>;

    /// A person's act: the ticket's state, owner and due date as `change`
    /// says, a released text, and the note of kind change recording it, in
    /// one transaction -- refused when the ticket has gained a note since
    /// `against_notes`, so an act is never taken on a ticket its taker has
    /// not seen. False when it had.
    fn change(
        &self,
        ticket_id: &str,
        against_notes: i32,
        change: &Change,
        note: &Note,
        notices: &[Notice],
    ) -> Result<bool, String>;

    /// A person's notices after `after` (a notice id; empty for all),
    /// oldest first, at most `limit`.
    fn notices(&self, subject: &str, after: &str, limit: usize) -> Result<Vec<Notice>, String>;

    /// A person's latest notices, newest first, at most `limit`.
    fn latest_notices(&self, subject: &str, limit: usize) -> Result<Vec<Notice>, String>;

    /// Where a reader -- a delegation, or empty for the person's pages -- has
    /// read a person's inbox to; empty before its first read.
    fn cursor(&self, subject: &str, reader: &str) -> Result<String, String>;

    fn advance(&self, subject: &str, reader: &str, to: &str) -> Result<(), String>;

    /// Mark a person's notices of these tickets read. How many were unread.
    fn mark_read(&self, subject: &str, ticket_ids: &[String]) -> Result<usize, String>;

    fn unread(&self, subject: &str) -> Result<Vec<Notice>, String>;

    /// What one instance filed, ticket after `after`, at most `limit`.
    fn filed_by(&self, instance: &str, after: &str, limit: usize) -> Result<Vec<Ticket>, String>;

    /// Remove notices older than `before_ns`. How many.
    fn sweep(&self, before_ns: i64) -> Result<usize, String>;
}

// ── In memory ────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Kept {
    tickets: BTreeMap<String, Ticket>,
    notices: Vec<Notice>,
    cursors: HashMap<(String, String), String>,
}

/// For tests, and for a dashboard given no database, where a restart
/// forgets every ticket.
#[derive(Default)]
pub struct InMemory {
    kept: Mutex<Kept>,
}

impl InMemory {
    fn lock(&self) -> std::sync::MutexGuard<'_, Kept> {
        self.kept.lock().expect("ticket store lock poisoned")
    }
}

fn without_notes(ticket: &Ticket) -> Ticket {
    Ticket {
        notes: Vec::new(),
        ..ticket.clone()
    }
}

impl TicketStore for InMemory {
    fn insert(&self, ticket: &Ticket, notices: &[Notice]) -> Result<Inserted, String> {
        let mut kept = self.lock();
        if !ticket.idempotency_key.is_empty() {
            if let Some(held) = kept.tickets.values().find(|held| {
                held.state == State::Open
                    && held.filed_by.instance == ticket.filed_by.instance
                    && held.idempotency_key == ticket.idempotency_key
            }) {
                return Ok(Inserted::KeyHeld(held.ticket_id.clone()));
            }
        }
        let mut kept_ticket = ticket.clone();
        kept_ticket.note_count = ticket.notes.len() as i32;
        kept.tickets.insert(ticket.ticket_id.clone(), kept_ticket);
        kept.notices.extend(notices.iter().cloned());
        Ok(Inserted::Made)
    }

    fn fold(
        &self,
        ticket_id: &str,
        seen: Option<(&str, bool, &[String])>,
        now_ns: i64,
    ) -> Result<i64, String> {
        let mut kept = self.lock();
        let ticket = kept
            .tickets
            .get_mut(ticket_id)
            .ok_or_else(|| format!("no ticket {ticket_id}"))?;
        ticket.seen_count += 1;
        ticket.last_seen_ns = now_ns;
        if let Some((text, suspect, rules)) = seen {
            ticket.seen = text.to_string();
            ticket.suspect = suspect;
            ticket.matched_rules = rules.to_vec();
        }
        Ok(ticket.seen_count)
    }

    fn by_key(&self, instance: &str, key: &str) -> Result<Vec<Ticket>, String> {
        let kept = self.lock();
        let mut found: Vec<Ticket> = kept
            .tickets
            .values()
            .filter(|t| t.filed_by.instance == instance && t.idempotency_key == key)
            .map(without_notes)
            .collect();
        found.sort_by(|a, b| b.ticket_id.cmp(&a.ticket_id));
        Ok(found)
    }

    fn ticket(&self, ticket_id: &str) -> Result<Option<Ticket>, String> {
        Ok(self.lock().tickets.get(ticket_id).cloned())
    }

    fn tickets(&self) -> Result<Vec<Ticket>, String> {
        Ok(self
            .lock()
            .tickets
            .values()
            .rev()
            .map(without_notes)
            .collect())
    }

    fn filed_since(&self, subject: &str, since_ns: i64) -> Result<usize, String> {
        Ok(self
            .lock()
            .tickets
            .values()
            .filter(|t| {
                t.filed_by.subject == subject
                    && t.filed_by.provenance != Provenance::Plugin
                    && t.filed_at_ns >= since_ns
            })
            .count())
    }

    fn add_note(&self, ticket_id: &str, note: &Note, notices: &[Notice]) -> Result<i32, String> {
        let mut kept = self.lock();
        let ticket = kept
            .tickets
            .get_mut(ticket_id)
            .ok_or_else(|| format!("no ticket {ticket_id}"))?;
        let number = ticket.notes.len() as i32 + 1;
        ticket.notes.push(Note {
            number,
            ..note.clone()
        });
        ticket.note_count = number;
        kept.notices.extend(notices.iter().cloned());
        Ok(number)
    }

    fn change(
        &self,
        ticket_id: &str,
        against_notes: i32,
        change: &Change,
        note: &Note,
        notices: &[Notice],
    ) -> Result<bool, String> {
        let mut kept = self.lock();
        let open_key_held = |kept: &Kept, ticket: &Ticket| {
            change.state == State::Open
                && ticket.state != State::Open
                && !ticket.idempotency_key.is_empty()
                && kept.tickets.values().any(|other| {
                    other.ticket_id != ticket.ticket_id
                        && other.state == State::Open
                        && other.filed_by.instance == ticket.filed_by.instance
                        && other.idempotency_key == ticket.idempotency_key
                })
        };
        let held = kept
            .tickets
            .get(ticket_id)
            .ok_or_else(|| format!("no ticket {ticket_id}"))?
            .clone();
        if held.notes.len() as i32 != against_notes {
            return Ok(false);
        }
        if open_key_held(&kept, &held) {
            return Err(super::KEY_HELD.into());
        }
        let ticket = kept.tickets.get_mut(ticket_id).expect("held above");
        change.apply(ticket);
        let number = ticket.notes.len() as i32 + 1;
        ticket.notes.push(Note {
            number,
            ..note.clone()
        });
        ticket.note_count = number;
        kept.notices.extend(notices.iter().cloned());
        Ok(true)
    }

    fn notices(&self, subject: &str, after: &str, limit: usize) -> Result<Vec<Notice>, String> {
        let kept = self.lock();
        let mut found: Vec<Notice> = kept
            .notices
            .iter()
            .filter(|n| n.subject == subject && n.notice_id.as_str() > after)
            .cloned()
            .collect();
        found.sort_by(|a, b| a.notice_id.cmp(&b.notice_id));
        found.truncate(limit);
        Ok(found)
    }

    fn latest_notices(&self, subject: &str, limit: usize) -> Result<Vec<Notice>, String> {
        let kept = self.lock();
        let mut found: Vec<Notice> = kept
            .notices
            .iter()
            .filter(|n| n.subject == subject)
            .cloned()
            .collect();
        found.sort_by(|a, b| b.notice_id.cmp(&a.notice_id));
        found.truncate(limit);
        Ok(found)
    }

    fn cursor(&self, subject: &str, reader: &str) -> Result<String, String> {
        Ok(self
            .lock()
            .cursors
            .get(&(subject.to_string(), reader.to_string()))
            .cloned()
            .unwrap_or_default())
    }

    fn advance(&self, subject: &str, reader: &str, to: &str) -> Result<(), String> {
        let mut kept = self.lock();
        let at = kept
            .cursors
            .entry((subject.to_string(), reader.to_string()))
            .or_default();
        if to > at.as_str() {
            *at = to.to_string();
        }
        Ok(())
    }

    fn mark_read(&self, subject: &str, ticket_ids: &[String]) -> Result<usize, String> {
        let mut kept = self.lock();
        let mut marked = 0;
        for notice in kept
            .notices
            .iter_mut()
            .filter(|n| n.subject == subject && !n.read && ticket_ids.contains(&n.ticket_id))
        {
            notice.read = true;
            marked += 1;
        }
        Ok(marked)
    }

    fn unread(&self, subject: &str) -> Result<Vec<Notice>, String> {
        Ok(self
            .lock()
            .notices
            .iter()
            .filter(|n| n.subject == subject && !n.read)
            .cloned()
            .collect())
    }

    fn filed_by(&self, instance: &str, after: &str, limit: usize) -> Result<Vec<Ticket>, String> {
        let kept = self.lock();
        Ok(kept
            .tickets
            .values()
            .filter(|t| {
                t.filed_by.provenance == Provenance::Plugin
                    && t.filed_by.instance == instance
                    && t.ticket_id.as_str() > after
            })
            .take(limit)
            .map(without_notes)
            .collect())
    }

    fn sweep(&self, before_ns: i64) -> Result<usize, String> {
        let mut kept = self.lock();
        let before = kept.notices.len();
        kept.notices.retain(|n| n.changed_ns >= before_ns);
        Ok(before - kept.notices.len())
    }
}

// ── In Postgres ──────────────────────────────────────────────────────────────

pub struct InPostgres {
    database: Database,
}

impl InPostgres {
    /// On a database already verified at start.
    pub fn on(database: Database) -> Self {
        Self { database }
    }
}

const TICKET_COLUMNS: &str = "ticket_id, title, seen, kind, concerns_kind, concerns_instance, \
     concerns_plugin, concerns_version, step, operation, reason, paths, filed_provenance, \
     filed_subject, filed_person, filed_delegation, filed_client, filed_instance, \
     idempotency_key, state, resolution, cites, owner, owner_name, due, suspect, \
     matched_rules, fingerprint, seen_count, first_seen_ns, last_seen_ns, filed_at_ns, \
     (SELECT count(*) FROM dashboard_ticket_note n WHERE n.ticket_id = t.ticket_id)::integer";

fn provenance(said: &str) -> Provenance {
    match said {
        "client" => Provenance::Client,
        "plugin" => Provenance::Plugin,
        "rules" => Provenance::Rules,
        _ => Provenance::Person,
    }
}

fn state(said: &str) -> State {
    match said {
        "resolved" => State::Resolved,
        "closed" => State::Closed,
        _ => State::Open,
    }
}

fn ticket_of(row: &postgres::Row) -> Ticket {
    let filed: String = row.get(12);
    let held: String = row.get(19);
    Ticket {
        ticket_id: row.get(0),
        title: row.get(1),
        seen: row.get(2),
        kind: row.get(3),
        concerns: Subject {
            kind: row.get(4),
            instance: row.get(5),
            plugin: row.get(6),
            version: row.get(7),
        },
        step: row.get(8),
        operation: row.get(9),
        reason: row.get(10),
        paths: row.get(11),
        references: Vec::new(),
        filed_by: Author {
            provenance: provenance(&filed),
            subject: row.get(13),
            person: row.get(14),
            delegation_id: row.get(15),
            client_name: row.get(16),
            instance: row.get(17),
        },
        idempotency_key: row.get(18),
        state: state(&held),
        resolution: row.get(20),
        cites: row.get(21),
        owner: row.get(22),
        owner_name: row.get(23),
        due: row.get(24),
        suspect: row.get(25),
        matched_rules: row.get(26),
        fingerprint: row.get(27),
        seen_count: row.get(28),
        first_seen_ns: row.get(29),
        last_seen_ns: row.get(30),
        filed_at_ns: row.get(31),
        notes: Vec::new(),
        note_count: row.get(32),
    }
}

fn author_of(row: &postgres::Row, from: usize) -> Author {
    let said: String = row.get(from);
    Author {
        provenance: provenance(&said),
        subject: row.get(from + 1),
        person: row.get(from + 2),
        delegation_id: row.get(from + 3),
        client_name: row.get(from + 4),
        instance: row.get(from + 5),
    }
}

const NOTICE_COLUMNS: &str = "notice_id, subject, ticket_id, kind, author_provenance, \
     author_subject, author_person, author_delegation, author_client, author_instance, \
     changed_ns, read";

fn notice_of(row: &postgres::Row) -> Notice {
    Notice {
        notice_id: row.get(0),
        subject: row.get(1),
        ticket_id: row.get(2),
        kind: row.get(3),
        author: author_of(row, 4),
        changed_ns: row.get(10),
        read: row.get(11),
    }
}

/// The references of these tickets, in place, by one read.
fn with_references(
    conn: &mut impl postgres::GenericClient,
    tickets: &mut [Ticket],
) -> Result<(), String> {
    if tickets.is_empty() {
        return Ok(());
    }
    let ids: Vec<String> = tickets.iter().map(|t| t.ticket_id.clone()).collect();
    let rows = conn
        .query(
            "SELECT ticket_id, kind, value, account_id, found_in_text
               FROM dashboard_ticket_reference WHERE ticket_id = ANY($1)
              ORDER BY ticket_id, position",
            &[&ids],
        )
        .map_err(said)?;
    let mut by_ticket: HashMap<String, Vec<Reference>> = HashMap::new();
    for row in rows {
        by_ticket.entry(row.get(0)).or_default().push(Reference {
            kind: row.get(1),
            value: row.get(2),
            account_id: row.get(3),
            found: row.get(4),
        });
    }
    for ticket in tickets.iter_mut() {
        ticket.references = by_ticket.remove(&ticket.ticket_id).unwrap_or_default();
    }
    Ok(())
}

fn insert_note(
    tx: &mut postgres::Transaction<'_>,
    ticket_id: &str,
    number: i32,
    note: &Note,
) -> Result<(), String> {
    let a = &note.author;
    tx.execute(
        "INSERT INTO dashboard_ticket_note (ticket_id, number, kind, author_provenance,
             author_subject, author_person, author_delegation, author_client, author_instance,
             noted_ns, note, suspect, matched_rules)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        &[
            &ticket_id,
            &number,
            &note.kind,
            &a.provenance.as_str(),
            &a.subject,
            &a.person,
            &a.delegation_id,
            &a.client_name,
            &a.instance,
            &note.noted_ns,
            &note.note,
            &note.suspect,
            &note.matched_rules,
        ],
    )
    .map_err(said)?;
    Ok(())
}

fn insert_notices(tx: &mut postgres::Transaction<'_>, notices: &[Notice]) -> Result<(), String> {
    for notice in notices {
        let a = &notice.author;
        tx.execute(
            "INSERT INTO dashboard_notice (notice_id, subject, ticket_id, kind,
                 author_provenance, author_subject, author_person, author_delegation,
                 author_client, author_instance, changed_ns, read)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, false)",
            &[
                &notice.notice_id,
                &notice.subject,
                &notice.ticket_id,
                &notice.kind,
                &a.provenance.as_str(),
                &a.subject,
                &a.person,
                &a.delegation_id,
                &a.client_name,
                &a.instance,
                &notice.changed_ns,
            ],
        )
        .map_err(said)?;
    }
    Ok(())
}

impl TicketStore for InPostgres {
    fn insert(&self, ticket: &Ticket, notices: &[Notice]) -> Result<Inserted, String> {
        let mut conn = self.database.conn()?;
        let mut tx = conn.transaction().map_err(said)?;
        let f = &ticket.filed_by;
        let made = tx
            .execute(
                "INSERT INTO dashboard_ticket (ticket_id, title, seen, kind, concerns_kind,
                     concerns_instance, concerns_plugin, concerns_version, step, operation,
                     reason, paths, filed_provenance, filed_subject, filed_person,
                     filed_delegation, filed_client, filed_instance, idempotency_key, state,
                     resolution, cites, owner, owner_name, due, suspect, matched_rules,
                     fingerprint, seen_count, first_seen_ns, last_seen_ns, filed_at_ns)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                         $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, $28,
                         $29, $30, $31, $32)
                 ON CONFLICT (filed_instance, idempotency_key)
                     WHERE state = 'open' AND idempotency_key <> '' DO NOTHING",
                &[
                    &ticket.ticket_id,
                    &ticket.title,
                    &ticket.seen,
                    &ticket.kind,
                    &ticket.concerns.kind,
                    &ticket.concerns.instance,
                    &ticket.concerns.plugin,
                    &ticket.concerns.version,
                    &ticket.step,
                    &ticket.operation,
                    &ticket.reason,
                    &ticket.paths,
                    &f.provenance.as_str(),
                    &f.subject,
                    &f.person,
                    &f.delegation_id,
                    &f.client_name,
                    &f.instance,
                    &ticket.idempotency_key,
                    &ticket.state.as_str(),
                    &ticket.resolution,
                    &ticket.cites,
                    &ticket.owner,
                    &ticket.owner_name,
                    &ticket.due,
                    &ticket.suspect,
                    &ticket.matched_rules,
                    &ticket.fingerprint,
                    &ticket.seen_count,
                    &ticket.first_seen_ns,
                    &ticket.last_seen_ns,
                    &ticket.filed_at_ns,
                ],
            )
            .map_err(said)?;
        if made == 0 {
            let held = tx
                .query_one(
                    "SELECT ticket_id FROM dashboard_ticket
                      WHERE filed_instance = $1 AND idempotency_key = $2 AND state = 'open'",
                    &[&f.instance, &ticket.idempotency_key],
                )
                .map_err(said)?;
            return Ok(Inserted::KeyHeld(held.get(0)));
        }
        for (position, reference) in ticket.references.iter().enumerate() {
            tx.execute(
                "INSERT INTO dashboard_ticket_reference
                     (ticket_id, position, kind, value, account_id, found_in_text)
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &[
                    &ticket.ticket_id,
                    &(position as i32),
                    &reference.kind,
                    &reference.value,
                    &reference.account_id,
                    &reference.found,
                ],
            )
            .map_err(said)?;
        }
        for (n, note) in ticket.notes.iter().enumerate() {
            insert_note(&mut tx, &ticket.ticket_id, n as i32 + 1, note)?;
        }
        insert_notices(&mut tx, notices)?;
        tx.commit().map_err(said)?;
        Ok(Inserted::Made)
    }

    fn fold(
        &self,
        ticket_id: &str,
        seen: Option<(&str, bool, &[String])>,
        now_ns: i64,
    ) -> Result<i64, String> {
        let mut conn = self.database.conn()?;
        let row = match seen {
            None => conn.query_one(
                "UPDATE dashboard_ticket SET seen_count = seen_count + 1, last_seen_ns = $2
                  WHERE ticket_id = $1 RETURNING seen_count",
                &[&ticket_id, &now_ns],
            ),
            Some((text, suspect, rules)) => conn.query_one(
                "UPDATE dashboard_ticket SET seen_count = seen_count + 1, last_seen_ns = $2,
                        seen = $3, suspect = $4, matched_rules = $5
                  WHERE ticket_id = $1 RETURNING seen_count",
                &[&ticket_id, &now_ns, &text, &suspect, &rules],
            ),
        }
        .map_err(said)?;
        Ok(row.get(0))
    }

    fn by_key(&self, instance: &str, key: &str) -> Result<Vec<Ticket>, String> {
        let mut conn = self.database.conn()?;
        let rows = conn
            .query(
                &format!(
                    "SELECT {TICKET_COLUMNS} FROM dashboard_ticket t
                      WHERE filed_instance = $1 AND idempotency_key = $2
                      ORDER BY ticket_id DESC"
                ),
                &[&instance, &key],
            )
            .map_err(said)?;
        let mut found: Vec<Ticket> = rows.iter().map(ticket_of).collect();
        with_references(&mut *conn, &mut found)?;
        Ok(found)
    }

    fn ticket(&self, ticket_id: &str) -> Result<Option<Ticket>, String> {
        let mut conn = self.database.conn()?;
        let Some(row) = conn
            .query_opt(
                &format!("SELECT {TICKET_COLUMNS} FROM dashboard_ticket t WHERE ticket_id = $1"),
                &[&ticket_id],
            )
            .map_err(said)?
        else {
            return Ok(None);
        };
        let mut ticket = [ticket_of(&row)];
        with_references(&mut *conn, &mut ticket)?;
        let [mut ticket] = ticket;
        let notes = conn
            .query(
                "SELECT number, kind, author_provenance, author_subject, author_person,
                        author_delegation, author_client, author_instance, noted_ns, note,
                        suspect, matched_rules
                   FROM dashboard_ticket_note WHERE ticket_id = $1 ORDER BY number",
                &[&ticket_id],
            )
            .map_err(said)?;
        ticket.notes = notes
            .iter()
            .map(|row| Note {
                number: row.get(0),
                kind: row.get(1),
                author: author_of(row, 2),
                noted_ns: row.get(8),
                note: row.get(9),
                suspect: row.get(10),
                matched_rules: row.get(11),
            })
            .collect();
        Ok(Some(ticket))
    }

    fn tickets(&self) -> Result<Vec<Ticket>, String> {
        let mut conn = self.database.conn()?;
        let rows = conn
            .query(
                &format!("SELECT {TICKET_COLUMNS} FROM dashboard_ticket t ORDER BY ticket_id DESC"),
                &[],
            )
            .map_err(said)?;
        let mut found: Vec<Ticket> = rows.iter().map(ticket_of).collect();
        with_references(&mut *conn, &mut found)?;
        Ok(found)
    }

    fn filed_since(&self, subject: &str, since_ns: i64) -> Result<usize, String> {
        let mut conn = self.database.conn()?;
        let row = conn
            .query_one(
                "SELECT count(*) FROM dashboard_ticket
                  WHERE filed_subject = $1 AND filed_provenance <> 'plugin' AND filed_at_ns >= $2",
                &[&subject, &since_ns],
            )
            .map_err(said)?;
        Ok(row.get::<_, i64>(0) as usize)
    }

    fn add_note(&self, ticket_id: &str, note: &Note, notices: &[Notice]) -> Result<i32, String> {
        let mut conn = self.database.conn()?;
        let mut tx = conn.transaction().map_err(said)?;
        tx.query_one(
            "SELECT ticket_id FROM dashboard_ticket WHERE ticket_id = $1 FOR UPDATE",
            &[&ticket_id],
        )
        .map_err(said)?;
        let number: i32 = tx
            .query_one(
                "SELECT coalesce(max(number), 0) + 1 FROM dashboard_ticket_note WHERE ticket_id = $1",
                &[&ticket_id],
            )
            .map_err(said)?
            .get(0);
        insert_note(&mut tx, ticket_id, number, note)?;
        insert_notices(&mut tx, notices)?;
        tx.commit().map_err(said)?;
        Ok(number)
    }

    fn change(
        &self,
        ticket_id: &str,
        against_notes: i32,
        change: &Change,
        note: &Note,
        notices: &[Notice],
    ) -> Result<bool, String> {
        let mut conn = self.database.conn()?;
        let mut tx = conn.transaction().map_err(said)?;
        tx.query_one(
            "SELECT ticket_id FROM dashboard_ticket WHERE ticket_id = $1 FOR UPDATE",
            &[&ticket_id],
        )
        .map_err(said)?;
        let notes: i32 = tx
            .query_one(
                "SELECT count(*)::integer FROM dashboard_ticket_note WHERE ticket_id = $1",
                &[&ticket_id],
            )
            .map_err(said)?
            .get(0);
        if notes != against_notes {
            return Ok(false);
        }
        let changed = tx.execute(
            "UPDATE dashboard_ticket SET state = $2, resolution = $3, cites = $4, owner = $5,
                    owner_name = $6, due = $7,
                    suspect = CASE WHEN $8 THEN false ELSE suspect END,
                    matched_rules = CASE WHEN $8 THEN '{}'::text[] ELSE matched_rules END
              WHERE ticket_id = $1",
            &[
                &ticket_id,
                &change.state.as_str(),
                &change.resolution,
                &change.cites,
                &change.owner,
                &change.owner_name,
                &change.due,
                &change.release_ticket,
            ],
        );
        if let Err(failed) = changed {
            // Reopening a plugin's ticket while another, filed since under
            // the same key, is open: the index holds one open per key.
            if failed.code() == Some(&postgres::error::SqlState::UNIQUE_VIOLATION) {
                return Err(super::KEY_HELD.into());
            }
            return Err(said(failed));
        }
        if let Some(number) = change.release_note {
            tx.execute(
                "UPDATE dashboard_ticket_note SET suspect = false, matched_rules = '{}'
                  WHERE ticket_id = $1 AND number = $2",
                &[&ticket_id, &number],
            )
            .map_err(said)?;
        }
        insert_note(&mut tx, ticket_id, notes + 1, note)?;
        insert_notices(&mut tx, notices)?;
        tx.commit().map_err(said)?;
        Ok(true)
    }

    fn notices(&self, subject: &str, after: &str, limit: usize) -> Result<Vec<Notice>, String> {
        let mut conn = self.database.conn()?;
        let rows = conn
            .query(
                &format!(
                    "SELECT {NOTICE_COLUMNS} FROM dashboard_notice
                      WHERE subject = $1 AND notice_id > $2 ORDER BY notice_id LIMIT $3"
                ),
                &[&subject, &after, &(limit as i64)],
            )
            .map_err(said)?;
        Ok(rows.iter().map(notice_of).collect())
    }

    fn latest_notices(&self, subject: &str, limit: usize) -> Result<Vec<Notice>, String> {
        let mut conn = self.database.conn()?;
        let rows = conn
            .query(
                &format!(
                    "SELECT {NOTICE_COLUMNS} FROM dashboard_notice
                      WHERE subject = $1 ORDER BY notice_id DESC LIMIT $2"
                ),
                &[&subject, &(limit as i64)],
            )
            .map_err(said)?;
        Ok(rows.iter().map(notice_of).collect())
    }

    fn cursor(&self, subject: &str, reader: &str) -> Result<String, String> {
        let mut conn = self.database.conn()?;
        Ok(conn
            .query_opt(
                "SELECT notice_id FROM dashboard_inbox_cursor WHERE subject = $1 AND reader = $2",
                &[&subject, &reader],
            )
            .map_err(said)?
            .map(|row| row.get(0))
            .unwrap_or_default())
    }

    fn advance(&self, subject: &str, reader: &str, to: &str) -> Result<(), String> {
        let mut conn = self.database.conn()?;
        conn.execute(
            "INSERT INTO dashboard_inbox_cursor (subject, reader, notice_id) VALUES ($1, $2, $3)
             ON CONFLICT (subject, reader) DO UPDATE SET notice_id = excluded.notice_id
              WHERE dashboard_inbox_cursor.notice_id < excluded.notice_id",
            &[&subject, &reader, &to],
        )
        .map_err(said)?;
        Ok(())
    }

    fn mark_read(&self, subject: &str, ticket_ids: &[String]) -> Result<usize, String> {
        let mut conn = self.database.conn()?;
        let marked = conn
            .execute(
                "UPDATE dashboard_notice SET read = true
                  WHERE subject = $1 AND NOT read AND ticket_id = ANY($2)",
                &[&subject, &ticket_ids],
            )
            .map_err(said)?;
        Ok(marked as usize)
    }

    fn unread(&self, subject: &str) -> Result<Vec<Notice>, String> {
        let mut conn = self.database.conn()?;
        let rows = conn
            .query(
                &format!(
                    "SELECT {NOTICE_COLUMNS} FROM dashboard_notice
                      WHERE subject = $1 AND NOT read ORDER BY notice_id"
                ),
                &[&subject],
            )
            .map_err(said)?;
        Ok(rows.iter().map(notice_of).collect())
    }

    fn filed_by(&self, instance: &str, after: &str, limit: usize) -> Result<Vec<Ticket>, String> {
        let mut conn = self.database.conn()?;
        let rows = conn
            .query(
                &format!(
                    "SELECT {TICKET_COLUMNS} FROM dashboard_ticket t
                      WHERE filed_provenance = 'plugin' AND filed_instance = $1 AND ticket_id > $2
                      ORDER BY ticket_id LIMIT $3"
                ),
                &[&instance, &after, &(limit as i64)],
            )
            .map_err(said)?;
        Ok(rows.iter().map(ticket_of).collect())
    }

    fn sweep(&self, before_ns: i64) -> Result<usize, String> {
        let mut conn = self.database.conn()?;
        let swept = conn
            .execute(
                "DELETE FROM dashboard_notice WHERE changed_ns < $1",
                &[&before_ns],
            )
            .map_err(said)?;
        Ok(swept as usize)
    }
}
