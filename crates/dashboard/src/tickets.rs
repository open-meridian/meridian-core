//! Tickets inside a deployment (contract v13; W6.21 to W6.24, W4.12;
//! spec/a-problem-seen-in-a-deployment-reaches-someone-who-can-act, slice 1;
//! plans/tickets-inside-a-deployment, accepted and ruled 2026-10-03).
//!
//! A person who sees something wrong presses "Report a problem" on the page
//! they are on; their agent files the same through `/mcp`; a plugin files
//! for the person it is serving, through its sidecar (W4.12), never as
//! itself. Each ticket is seen only by the people who could act on what it
//! concerns, and never by anyone who could not see an account it names
//! ([`visibility`]). The dashboard's rules add advice at once ([`rules`]),
//! and a person's agent may add more; neither changes anything. Only a
//! person, on the ticket's page, assigns, resolves, closes, reopens or
//! releases (W6.23): no tool does, and none exists. Each person has an inbox
//! that tells them, and each of their agents once, what changed. Text that
//! reads like an instruction to an agent is held, and no agent is shown it
//! until a person releases it ([`quarantine`]). Nothing leaves the
//! deployment: no ticket topic reaches the conductor, and nothing here
//! calls out.
//!
//! **Text is data.** Every title, seen text, note and notice here was written
//! by someone other than whoever reads it. Pages show it as plain text,
//! never as HTML or Markdown, with its provenance beside it; tools answer it
//! in fields named as text, beside its author, never spliced into a sentence
//! of the dashboard's (requirements 54 to 57). Provenance is set from the
//! credential, never from text.
//!
//! The rows, each a function here that a page, a tool and the bus share:
//! FileTicket ([`file`]), ListTickets and ReadTicket ([`list`], [`read`]),
//! AddTicketNote ([`add_note`]), CountTickets ([`count`]), WorkTicket
//! ([`work`], a person's at the page alone), ReadInbox and MarkNoticesRead
//! ([`read_inbox`], [`mark_read`]); and the plugin's two bus rows,
//! PluginFilesTicket and ReadFiledTickets ([`plugin`]).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use meridian_access::{person_access, Access};
use meridian_domain::text;
use meridian_domain::v1::AccessRecords;
use meridian_pb::bounds::{
    FILE_TICKET_REQUEST_REFERENCES_COUNT, FILE_TICKET_REQUEST_SEEN_LENGTH,
    FILE_TICKET_REQUEST_TITLE_LENGTH, TICKET_NOTE_NOTE_LENGTH, TICKET_REFERENCE_VALUE_LENGTH,
};
use meridian_pb::v1::{
    FileTicketReply, FileTicketRequest, FiledTicket, ReadFiledTicketsReply,
    ReadFiledTicketsRequest, TicketKind, TicketNoteKind, TicketReference, TicketResolution,
    TicketState,
};
use serde_json::{json, Value};

use crate::web::App;

pub mod plugin;
pub mod quarantine;
pub mod rules;
pub mod store;
pub mod visibility;

pub use store::{InMemory, InPostgres, TicketStore};
pub use visibility::Reader;

use quarantine::WITHHELD;
use store::Inserted;

/// A plugin's ticket, filed with the dashboard (PluginFilesTicket, W4.12):
/// the first topic the dashboard answers.
pub const FILE_TICKET: &str = "platform.config.command.file-ticket";
/// What became of a plugin's tickets (ReadFiledTickets, W4.12).
pub const FILED_TICKETS: &str = "platform.config.query.filed-tickets";

/// What a ticket may concern, and what a reference may name (the
/// dictionary's TicketSubject.kind and TicketReference.kind).
pub const SUBJECTS: [&str; 10] = [
    "plugin",
    "dashboard",
    "bor",
    "street",
    "instrument",
    "conductor",
    "chart",
    "cli",
    "sdk",
    "platform",
];
pub const REFERENCES: [&str; 7] = [
    "account",
    "instrument",
    "break",
    "entry",
    "street_record",
    "tool_call",
    "plugin",
];
const PLACED_BY_ACCOUNT: [&str; 3] = ["break", "entry", "street_record"];

/// A person files at most this many tickets a day, at pages and through
/// their clients together (requirement 60). A plugin's rate is its
/// sidecar's: 20 an hour per instance.
pub const PERSON_FILINGS_A_DAY: usize = 50;
/// Notices are kept 90 days, swept with the call record (W6.24).
pub const NOTICES_KEPT_NS: i64 = 90 * 24 * crate::clock::HOUR_NS;
const DAY_NS: i64 = 24 * crate::clock::HOUR_NS;
/// The most notices one read of the inbox answers.
pub const INBOX_PAGE: usize = 200;
/// The most a plugin's read-back answers at once.
const FILED_PAGE: usize = 100;

/// Why reopening was refused: the plugin has filed again since, under the
/// same key, and that ticket is open.
pub(crate) const KEY_HELD: &str =
    "this plugin has an open ticket under the same key, filed since: work that one instead";

// ── The records ──────────────────────────────────────────────────────────────

/// How the deployment knows who wrote something: from the credential, never
/// from text (requirement 57).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Provenance {
    /// A person at the dashboard's pages, in their own session.
    #[default]
    Person,
    /// A person through a delegation and its client, at `/mcp`.
    Client,
    /// A plugin instance, for the person it acts for.
    Plugin,
    /// The dashboard's rules, which only advise.
    Rules,
}

impl Provenance {
    pub fn as_str(&self) -> &'static str {
        match self {
            Provenance::Person => "person",
            Provenance::Client => "client",
            Provenance::Plugin => "plugin",
            Provenance::Rules => "rules",
        }
    }
}

/// Who wrote it, and how the deployment knows (the dictionary's
/// TicketAuthor). The subject is the deployment's own and is never answered.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Author {
    pub provenance: Provenance,
    pub subject: String,
    pub person: String,
    pub delegation_id: String,
    pub client_name: String,
    pub instance: String,
}

impl Author {
    pub fn rules() -> Author {
        Author {
            provenance: Provenance::Rules,
            ..Author::default()
        }
    }

    /// As every answer carries it: its provenance, and what of the rest it
    /// has.
    pub fn json(&self) -> Value {
        let mut said = json!({"provenance": self.provenance.as_str()});
        for (name, value) in [
            ("person", &self.person),
            ("delegation_id", &self.delegation_id),
            ("client_name", &self.client_name),
            ("instance", &self.instance),
        ] {
            if !value.is_empty() {
                said[name] = Value::String(value.clone());
            }
        }
        said
    }

    /// As a page says it, beside the text: who, and how.
    pub fn said(&self) -> String {
        match self.provenance {
            Provenance::Person => self.person.clone(),
            Provenance::Client => format!("{} through {}", self.person, self.client_name),
            Provenance::Plugin => format!("{} for {}", self.instance, self.person),
            Provenance::Rules => "the dashboard's rules".into(),
        }
    }
}

/// What a ticket concerns: a plugin instance, with the plugin's name and
/// version at filing; a part of core; or the platform.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Subject {
    pub kind: String,
    pub instance: String,
    pub plugin: String,
    pub version: String,
}

impl Subject {
    pub fn json(&self) -> Value {
        let mut said = json!({"kind": self.kind});
        if !self.instance.is_empty() {
            said["instance"] = self.instance.clone().into();
        }
        if !self.version.is_empty() {
            said["version"] = self.version.clone().into();
        }
        said
    }

    /// The instance, or the part of core, or the platform: what a count and
    /// a filter name it by.
    pub fn named(&self) -> &str {
        if self.kind == "plugin" {
            &self.instance
        } else {
            &self.kind
        }
    }
}

/// A record a ticket is about, by value; `found` when the dashboard found it
/// in the ticket's text (requirement 7).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Reference {
    pub kind: String,
    pub value: String,
    pub account_id: String,
    pub found: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum State {
    #[default]
    Open,
    Resolved,
    Closed,
}

impl State {
    pub fn as_str(&self) -> &'static str {
        match self {
            State::Open => "open",
            State::Resolved => "resolved",
            State::Closed => "closed",
        }
    }

    fn wire(&self) -> TicketState {
        match self {
            State::Open => TicketState::Open,
            State::Resolved => TicketState::Resolved,
            State::Closed => TicketState::Closed,
        }
    }

    fn parse(said: &str) -> Option<State> {
        match said.trim().to_ascii_lowercase().as_str() {
            "open" | "ticket_state_open" => Some(State::Open),
            "resolved" | "ticket_state_resolved" => Some(State::Resolved),
            "closed" | "ticket_state_closed" => Some(State::Closed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Note {
    pub number: i32,
    /// meridian.v1.TicketNoteKind.
    pub kind: i32,
    pub author: Author,
    pub noted_ns: i64,
    pub note: String,
    pub suspect: bool,
    pub matched_rules: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Ticket {
    pub ticket_id: String,
    pub title: String,
    pub seen: String,
    /// meridian.v1.TicketKind.
    pub kind: i32,
    pub concerns: Subject,
    pub step: String,
    pub operation: String,
    pub reason: String,
    pub paths: Vec<String>,
    pub references: Vec<Reference>,
    pub filed_by: Author,
    pub idempotency_key: String,
    pub state: State,
    /// meridian.v1.TicketResolution.
    pub resolution: i32,
    pub cites: String,
    pub owner: String,
    pub owner_name: String,
    pub due: String,
    pub suspect: bool,
    pub matched_rules: Vec<String>,
    pub fingerprint: String,
    pub seen_count: i64,
    pub first_seen_ns: i64,
    pub last_seen_ns: i64,
    pub filed_at_ns: i64,
    /// Oldest first; empty where a list read leaves them out.
    pub notes: Vec<Note>,
    pub note_count: i32,
}

/// One change to a ticket, told to one person in their inbox: names the
/// ticket and the change's kind, never its text (requirement 35).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Notice {
    pub notice_id: String,
    pub subject: String,
    pub ticket_id: String,
    pub kind: String,
    pub author: Author,
    pub changed_ns: i64,
    pub read: bool,
}

/// A person's act, as the ticket stands after it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Change {
    pub state: State,
    pub resolution: i32,
    pub cites: String,
    pub owner: String,
    pub owner_name: String,
    pub due: String,
    pub release_ticket: bool,
    pub release_note: Option<i32>,
}

impl Change {
    /// The change as it leaves `ticket` when nothing is acted on.
    fn of(ticket: &Ticket) -> Change {
        Change {
            state: ticket.state,
            resolution: ticket.resolution,
            cites: ticket.cites.clone(),
            owner: ticket.owner.clone(),
            owner_name: ticket.owner_name.clone(),
            due: ticket.due.clone(),
            release_ticket: false,
            release_note: None,
        }
    }

    pub(crate) fn apply(&self, ticket: &mut Ticket) {
        ticket.state = self.state;
        ticket.resolution = self.resolution;
        ticket.cites = self.cites.clone();
        ticket.owner = self.owner.clone();
        ticket.owner_name = self.owner_name.clone();
        ticket.due = self.due.clone();
        if self.release_ticket {
            ticket.suspect = false;
            ticket.matched_rules.clear();
        }
        if let Some(number) = self.release_note {
            if let Some(note) = ticket.notes.iter_mut().find(|n| n.number == number) {
                note.suspect = false;
                note.matched_rules.clear();
            }
        }
    }
}

// ── Words for the wire ───────────────────────────────────────────────────────

/// A kind as an answer names it: `defect`, `discrepancy`, `request`,
/// `question`.
pub fn kind_name(kind: i32) -> &'static str {
    match TicketKind::try_from(kind) {
        Ok(TicketKind::Defect) => "defect",
        Ok(TicketKind::Discrepancy) => "discrepancy",
        Ok(TicketKind::Request) => "request",
        Ok(TicketKind::Question) => "question",
        _ => "",
    }
}

/// A kind from its name or its proto name, in any case.
pub fn parse_kind(said: &str) -> Option<i32> {
    let said = said.trim().to_ascii_lowercase();
    let said = said.strip_prefix("ticket_kind_").unwrap_or(&said);
    [
        TicketKind::Defect,
        TicketKind::Discrepancy,
        TicketKind::Request,
        TicketKind::Question,
    ]
    .into_iter()
    .find(|kind| kind_name(*kind as i32) == said)
    .map(|kind| kind as i32)
}

pub fn resolution_name(resolution: i32) -> &'static str {
    match TicketResolution::try_from(resolution) {
        Ok(TicketResolution::Note) => "note",
        Ok(TicketResolution::Answer) => "answer",
        Ok(TicketResolution::Version) => "version",
        Ok(TicketResolution::Withdrawn) => "withdrawn",
        Ok(TicketResolution::Duplicate) => "duplicate",
        Ok(TicketResolution::NotAProblem) => "not_a_problem",
        _ => "",
    }
}

pub fn parse_resolution(said: &str) -> Option<i32> {
    let said = said.trim().to_ascii_lowercase();
    let said = said.strip_prefix("ticket_resolution_").unwrap_or(&said);
    (1..=6).find(|n| resolution_name(*n) == said)
}

pub fn note_kind_name(kind: i32) -> &'static str {
    match TicketNoteKind::try_from(kind) {
        Ok(TicketNoteKind::Note) => "note",
        Ok(TicketNoteKind::Advice) => "advice",
        Ok(TicketNoteKind::Answer) => "answer",
        Ok(TicketNoteKind::Change) => "change",
        _ => "",
    }
}

pub fn parse_note_kind(said: &str) -> Option<i32> {
    let said = said.trim().to_ascii_lowercase();
    let said = said.strip_prefix("ticket_note_kind_").unwrap_or(&said);
    (1..=4).find(|n| note_kind_name(*n) == said)
}

/// Concerns, version, step, operation, reason and paths: what a likely
/// duplicate shares.
fn fingerprint(ticket: &Ticket) -> String {
    let mut paths = ticket.paths.clone();
    paths.sort();
    [
        ticket.concerns.kind.as_str(),
        ticket.concerns.instance.as_str(),
        ticket.concerns.version.as_str(),
        ticket.step.as_str(),
        ticket.operation.as_str(),
        ticket.reason.as_str(),
        &paths.join(","),
    ]
    .join("|")
}

/// A ticket as an answer carries it. `withhold`: to a tool, or anything
/// that is not the ticket's page, a suspect text is answered as its
/// metadata, provenance and matched rules and the words [`WITHHELD`], never
/// the text. `whole`: with its text, references and notes, as a read.
pub fn ticket_json(ticket: &Ticket, withhold: bool, whole: bool) -> Value {
    let held = withhold && ticket.suspect;
    let mut said = json!({
        "ticket_id": ticket.ticket_id,
        "title": if held { WITHHELD } else { ticket.title.as_str() },
        "kind": kind_name(ticket.kind),
        "concerns": ticket.concerns.json(),
        "state": ticket.state.as_str(),
        "seen_count": ticket.seen_count,
        "suspect": ticket.suspect,
        "filed_by": ticket.filed_by.json(),
    });
    if ticket.suspect {
        said["matched_rules"] = json!(ticket.matched_rules);
    }
    if !ticket.owner_name.is_empty() {
        said["owner"] = ticket.owner_name.clone().into();
    }
    if !ticket.due.is_empty() {
        said["due"] = ticket.due.clone().into();
    }
    if ticket.state != State::Open {
        said["resolution"] = resolution_name(ticket.resolution).into();
        if !ticket.cites.is_empty() {
            said["cites"] = ticket.cites.clone().into();
        }
    }
    if !whole {
        return said;
    }
    said["seen"] = (if held { WITHHELD } else { ticket.seen.as_str() }).into();
    for (name, value) in [
        ("step", &ticket.step),
        ("operation", &ticket.operation),
        ("reason", &ticket.reason),
    ] {
        if !value.is_empty() {
            said[name] = value.clone().into();
        }
    }
    if !ticket.paths.is_empty() {
        said["paths"] = json!(ticket.paths);
    }
    said["references"] = ticket
        .references
        .iter()
        .map(|r| {
            let mut one = json!({"kind": r.kind, "value": r.value});
            if !r.account_id.is_empty() {
                one["account_id"] = r.account_id.clone().into();
            }
            if r.found {
                one["found_in_text"] = true.into();
            }
            one
        })
        .collect();
    said["notes"] = ticket
        .notes
        .iter()
        .map(|note| note_json(note, withhold))
        .collect();
    said["first_seen_ns"] = ticket.first_seen_ns.into();
    said["last_seen_ns"] = ticket.last_seen_ns.into();
    said
}

pub fn note_json(note: &Note, withhold: bool) -> Value {
    let held = withhold && note.suspect;
    let mut said = json!({
        "number": note.number,
        "kind": note_kind_name(note.kind),
        "author": note.author.json(),
        "noted_ns": note.noted_ns,
        "note": if held { WITHHELD } else { note.note.as_str() },
        "suspect": note.suspect,
    });
    if note.suspect {
        said["matched_rules"] = json!(note.matched_rules);
    }
    said
}

/// A notice as the inbox answers it, naming the ticket by its title as it
/// stands -- withheld to a tool while it is suspect -- never quoting the
/// change.
pub fn notice_json(notice: &Notice, ticket: Option<&Ticket>, withhold: bool) -> Value {
    let title = match ticket {
        Some(t) if withhold && t.suspect => WITHHELD,
        Some(t) => t.title.as_str(),
        None => "",
    };
    json!({
        "ticket_id": notice.ticket_id,
        "title": title,
        "kind": notice.kind,
        "author": notice.author.json(),
        "changed_ns": notice.changed_ns,
        "read": notice.read,
    })
}

// ── A refusal ────────────────────────────────────────────────────────────────

/// Why a row refused: an HTTP status, a reason a tool's answer names, each
/// field by its path, and the words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub status: u16,
    pub reason: &'static str,
    pub fields: Vec<(String, String)>,
    pub detail: String,
}

impl Refused {
    fn at(status: u16, reason: &'static str, path: &str, message: impl Into<String>) -> Refused {
        let message = message.into();
        Refused {
            status,
            reason,
            detail: if path.is_empty() {
                message.clone()
            } else {
                format!("{path}: {message}")
            },
            fields: if path.is_empty() {
                Vec::new()
            } else {
                vec![(path.to_string(), message)]
            },
        }
    }

    pub fn invalid(path: &str, message: impl Into<String>) -> Refused {
        Refused::at(400, "invalid_arguments", path, message)
    }

    pub fn forbidden(path: &str, message: impl Into<String>) -> Refused {
        Refused::at(403, "not_permitted", path, message)
    }

    pub fn not_found() -> Refused {
        Refused::at(
            404,
            "not_found",
            "",
            "no such ticket, or not one you may see",
        )
    }

    pub fn unavailable(why: impl Into<String>) -> Refused {
        Refused::at(503, "unavailable", "", why)
    }

    /// As a tool's answer carries it.
    pub fn tool_answer(&self) -> Value {
        crate::mcp::refused(
            self.reason,
            &self.detail,
            self.fields
                .iter()
                .map(|(path, message)| json!({"path": path, "message": message}))
                .collect(),
        )
    }
}

// ── The service ──────────────────────────────────────────────────────────────

/// Tickets, kept, and the identifiers minted for them.
pub struct Tickets {
    store: Arc<dyn TicketStore>,
    /// The last identifier's millisecond and random part, so identifiers
    /// minted in one millisecond still sort in the order they were minted:
    /// an inbox's place is the last notice it read.
    minted: Mutex<(u64, u128)>,
}

impl Default for Tickets {
    fn default() -> Self {
        Tickets::keeping(Arc::new(InMemory::default()))
    }
}

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

impl Tickets {
    pub fn keeping(store: Arc<dyn TicketStore>) -> Self {
        Tickets {
            store,
            minted: Mutex::new((0, 0)),
        }
    }

    /// A ULID at the deployment's time: 48 bits of milliseconds and 80 of
    /// randomness, one more than the last within the same millisecond.
    pub fn ulid(&self, now_ns: i64) -> String {
        use rand::RngCore;
        let ms = (now_ns.max(0) / 1_000_000) as u64;
        let mut minted = self.minted.lock().expect("ulid lock poisoned");
        let random = if ms <= minted.0 {
            minted.1 + 1
        } else {
            let mut bytes = [0u8; 16];
            rand::rngs::OsRng.fill_bytes(&mut bytes);
            u128::from_be_bytes(bytes) >> 49 // room to count up within a millisecond
        };
        let ms = ms.max(minted.0);
        *minted = (ms, random);
        let value: u128 = ((ms as u128) << 80) | (random & ((1u128 << 80) - 1));
        (0..26)
            .rev()
            .map(|i| CROCKFORD[((value >> (i * 5)) & 31) as usize] as char)
            .collect()
    }

    async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce(&dyn TicketStore) -> Result<T, String> + Send + 'static,
    ) -> Result<T, Refused> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || work(store.as_ref()))
            .await
            .map_err(|joined| Refused::unavailable(format!("the ticket store failed: {joined}")))?
            .map_err(|failed| {
                if failed == KEY_HELD {
                    Refused::at(409, "conflict", "act", KEY_HELD)
                } else {
                    tracing::error!(%failed, "the ticket store could not be read or written");
                    Refused::unavailable(
                        "the deployment's tickets could not be read or written; try again shortly",
                    )
                }
            })
    }

    /// Remove notices past 90 days.
    pub async fn sweep(&self, now_ns: i64) -> Result<usize, String> {
        self.blocking(move |store| store.sweep(now_ns - NOTICES_KEPT_NS))
            .await
            .map_err(|refused| refused.detail)
    }

    /// A person's unread notices of tickets they may see now: the header's
    /// count.
    pub async fn unread(&self, reader: &Reader) -> Result<usize, Refused> {
        let subject = reader.subject.clone();
        let unread = self.blocking(move |store| store.unread(&subject)).await?;
        let tickets = self.visible_by_id(reader).await?;
        Ok(unread
            .iter()
            .filter(|n| tickets.contains_key(&n.ticket_id))
            .count())
    }

    /// Every ticket the reader may see, by identifier.
    async fn visible_by_id(&self, reader: &Reader) -> Result<BTreeMap<String, Ticket>, Refused> {
        let all = self.blocking(|store| store.tickets()).await?;
        Ok(all
            .into_iter()
            .filter(|t| visibility::may_see(reader, t))
            .map(|t| (t.ticket_id.clone(), t))
            .collect())
    }
}

// ── Who is acting ────────────────────────────────────────────────────────────

/// Who files, notes or reads: the author the credential makes them, and
/// what they reach now (narrowed, through a delegation).
#[derive(Debug, Clone)]
pub struct Actor {
    pub author: Author,
    pub access: Access,
    pub through_delegation: bool,
}

impl Actor {
    pub fn reader(&self) -> Reader {
        Reader {
            subject: self.author.subject.clone(),
            access: self.access.clone(),
            through_delegation: self.through_delegation,
        }
    }
}

/// A person's whole access from the records, with the groups of their last
/// sign-in: for a plugin's filing, a notice's recipient, an owner.
fn access_of(records: &AccessRecords, subject: &str) -> Access {
    let groups = records
        .people
        .iter()
        .find(|p| p.subject == subject)
        .map(|p| p.directory_groups.clone())
        .unwrap_or_default();
    person_access(records, subject, &groups)
}

/// The name the deployment shows for a person.
pub fn display_name(records: &AccessRecords, subject: &str) -> String {
    records
        .people
        .iter()
        .find(|p| p.subject == subject)
        .map(|p| p.display_name.clone())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| subject.to_string())
}

/// Everyone who has signed in and may work the ticket.
fn workers(records: &AccessRecords, ticket: &Ticket) -> Vec<String> {
    records
        .people
        .iter()
        .filter(|p| visibility::may_work(&p.subject, &access_of(records, &p.subject), ticket))
        .map(|p| p.subject.clone())
        .collect()
}

/// The people a change concerns -- its filer, its owner, those who noted
/// it, and `also` -- who may see it now, the change's own author left out.
fn concerned(
    records: &AccessRecords,
    ticket: &Ticket,
    author: &str,
    also: &[String],
) -> Vec<String> {
    let mut people: BTreeSet<String> = BTreeSet::new();
    people.insert(ticket.filed_by.subject.clone());
    if !ticket.owner.is_empty() {
        people.insert(ticket.owner.clone());
    }
    for note in &ticket.notes {
        if note.author.provenance != Provenance::Rules {
            people.insert(note.author.subject.clone());
        }
    }
    people.extend(also.iter().cloned());
    people
        .into_iter()
        .filter(|subject| !subject.is_empty() && subject != author)
        .filter(|subject| {
            visibility::may_see(
                &Reader {
                    subject: subject.clone(),
                    access: access_of(records, subject),
                    through_delegation: false,
                },
                ticket,
            )
        })
        .collect()
}

fn notices_to(
    tickets: &Tickets,
    people: &[String],
    ticket_id: &str,
    kind: &str,
    author: &Author,
    now: i64,
) -> Vec<Notice> {
    people
        .iter()
        .map(|subject| Notice {
            notice_id: tickets.ulid(now),
            subject: subject.clone(),
            ticket_id: ticket_id.to_string(),
            kind: kind.to_string(),
            author: author.clone(),
            changed_ns: now,
            read: false,
        })
        .collect()
}

// ── Checking a text ──────────────────────────────────────────────────────────

/// A text, kept as NFC and held to its bound and the characters rule.
fn checked_text(path: &str, said: &str, bound: Option<(usize, usize)>) -> Result<String, Refused> {
    let kept = quarantine::nfc(said);
    if let Some(found) = text::refused(&kept) {
        return Err(Refused::invalid(path, found.to_string()));
    }
    if let Some((least, most)) = bound {
        let length = text::characters(&kept);
        if least > 0 && kept.trim().is_empty() {
            return Err(Refused::invalid(
                path,
                format!("is empty; it holds 1 to {most} characters"),
            ));
        }
        if length > most || length < least {
            return Err(Refused::invalid(
                path,
                format!("is {length} characters; it holds at most {most}"),
            ));
        }
    }
    Ok(kept)
}

/// The deployment's account identifiers and external account numbers in a
/// text, each as the account it names: whole tokens of letters, digits,
/// `-` and `_` (requirement 7).
pub fn accounts_in(records: &AccessRecords, said: &str) -> Vec<(String, String)> {
    let ids: BTreeSet<&str> = records
        .accounts
        .iter()
        .map(|a| a.account_id.as_str())
        .collect();
    let mut found: Vec<(String, String)> = Vec::new();
    for token in said.split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_')) {
        let token = token.trim_matches('-');
        if token.is_empty() {
            continue;
        }
        if ids.contains(token) {
            found.push((token.to_string(), token.to_string()));
        }
        for link in records
            .links
            .iter()
            .filter(|l| l.external_account_id == token && !l.account_id.is_empty())
        {
            found.push((token.to_string(), link.account_id.clone()));
        }
    }
    found.sort();
    found.dedup();
    found
}

// ── FileTicket (W6.21) ───────────────────────────────────────────────────────

/// File a ticket: at a page, through `/mcp`, or a plugin's for a person
/// (W4.12, the actor's author a plugin's). A plugin's repeat under an open
/// ticket's key folds into it and is answered `unchanged`.
pub async fn file(
    app: &App,
    actor: &Actor,
    asked: FileTicketRequest,
) -> Result<FileTicketReply, Refused> {
    let now = app.clock.now_ns();
    let records = app
        .records
        .current(now)
        .map_err(|stale| Refused::unavailable(stale.to_string()))?;
    let by_plugin = actor.author.provenance == Provenance::Plugin;

    let title = checked_text(
        "title",
        &asked.title,
        Some((
            FILE_TICKET_REQUEST_TITLE_LENGTH.least,
            FILE_TICKET_REQUEST_TITLE_LENGTH.most,
        )),
    )?;
    let seen = checked_text(
        "seen",
        &asked.seen,
        Some((
            FILE_TICKET_REQUEST_SEEN_LENGTH.least,
            FILE_TICKET_REQUEST_SEEN_LENGTH.most,
        )),
    )?;
    if !TicketKind::try_from(asked.kind).is_ok_and(|k| k != TicketKind::Unspecified) {
        return Err(Refused::invalid(
            "kind",
            "a ticket is a defect, a discrepancy, a request or a question",
        ));
    }
    let step = checked_text("step", &asked.step, None)?;
    let operation = checked_text("operation", &asked.operation, None)?;
    let reason = checked_text("reason", &asked.reason, None)?;
    let mut paths = Vec::new();
    for (n, path) in asked.paths.iter().enumerate() {
        paths.push(checked_text(&format!("paths[{n}]"), path, None)?);
    }
    let key = checked_text("idempotency_key", &asked.idempotency_key, None)?;
    if by_plugin && key.is_empty() {
        return Err(Refused::invalid(
            "idempotency_key",
            "a plugin names each problem by its own key",
        ));
    }

    // What it concerns.
    let Some(concerns) = asked.concerns else {
        return Err(Refused::invalid(
            "concerns",
            "a ticket concerns a plugin instance, a part of core or the platform",
        ));
    };
    if !SUBJECTS.contains(&concerns.kind.as_str()) {
        return Err(Refused::invalid(
            "concerns.kind",
            format!(
                "is {:?}; it is one of {}",
                concerns.kind,
                SUBJECTS.join(", ")
            ),
        ));
    }
    let mut subject = Subject {
        kind: concerns.kind.clone(),
        ..Subject::default()
    };
    if concerns.kind == "plugin" {
        let instance = concerns.instance.trim();
        if by_plugin {
            if !instance.is_empty() && instance != actor.author.instance {
                return Err(Refused::invalid(
                    "concerns.instance",
                    format!(
                        "{instance} is another plugin; a plugin files about itself, a part of \
                         core or the platform"
                    ),
                ));
            }
            subject.instance = actor.author.instance.clone();
        } else {
            if !crate::plugins::is_instance(instance) {
                return Err(Refused::invalid(
                    "concerns.instance",
                    "names the plugin instance the ticket concerns",
                ));
            }
            if !actor.access.held(instance).holds_any() {
                return Err(Refused::forbidden(
                    "concerns.instance",
                    format!(
                        "{instance} is not a plugin you reach{}",
                        if actor.through_delegation {
                            " through this delegation"
                        } else {
                            ""
                        }
                    ),
                ));
            }
            subject.instance = instance.to_string();
        }
        // The plugin and its version at filing, set by the deployment from
        // the instance's launch, never by the filer.
        if let Some(launch) = crate::catalogue::launches(app)
            .await
            .into_iter()
            .find(|launch| launch.instance_id == subject.instance)
        {
            subject.plugin = launch.name;
            subject.version = launch.version;
        }
    } else if !concerns.instance.is_empty() {
        return Err(Refused::invalid(
            "concerns.instance",
            format!("a ticket concerning {} names no plugin", concerns.kind),
        ));
    }

    // The records it is about.
    let count = FILE_TICKET_REQUEST_REFERENCES_COUNT;
    if !count.admits(asked.references.len()) {
        return Err(Refused::invalid(
            "references",
            format!("a ticket names at most {} records", count.most),
        ));
    }
    let concerns_plugin = (subject.kind == "plugin").then_some(subject.instance.as_str());
    let filer_access = if by_plugin {
        access_of(&records, &actor.author.subject)
    } else {
        actor.access.clone()
    };
    let mut references = Vec::new();
    for (n, given) in asked.references.iter().enumerate() {
        let TicketReference {
            kind,
            value,
            account_id,
        } = given;
        if !REFERENCES.contains(&kind.as_str()) {
            return Err(Refused::invalid(
                &format!("references[{n}].kind"),
                format!("is {kind:?}; it is one of {}", REFERENCES.join(", ")),
            ));
        }
        let value = checked_text(
            &format!("references[{n}].value"),
            value,
            Some((
                TICKET_REFERENCE_VALUE_LENGTH.least,
                TICKET_REFERENCE_VALUE_LENGTH.most,
            )),
        )?;
        let mut account = checked_text(&format!("references[{n}].account_id"), account_id, None)?;
        if kind == "account" && account.is_empty() {
            account = value.clone();
        }
        if PLACED_BY_ACCOUNT.contains(&kind.as_str()) && account.is_empty() {
            return Err(Refused::invalid(
                &format!("references[{n}].account_id"),
                format!("a {} names the account it is about", kind.replace('_', " ")),
            ));
        }
        if !account.is_empty() && !visibility::may_name(&filer_access, concerns_plugin, &account) {
            return Err(Refused::forbidden(
                &format!("references[{n}].account_id"),
                format!(
                    "{account} is not an account you may read{}",
                    match concerns_plugin {
                        Some(instance) => format!(" through {instance}"),
                        None => String::new(),
                    }
                ),
            ));
        }
        references.push(Reference {
            kind: kind.clone(),
            value,
            account_id: account,
            found: false,
        });
    }
    // Accounts named in the text become references whether or not the filer
    // may read them: recording one can only narrow who sees the ticket.
    for (value, account) in accounts_in(&records, &format!("{title}\n{seen}")) {
        if !references.iter().any(|r| r.account_id == account) {
            references.push(Reference {
                kind: "account".into(),
                value,
                account_id: account,
                found: true,
            });
        }
    }

    // Held as suspect, never refused for it.
    let matched = quarantine::matched(&format!("{title}\n{seen}"));

    let mut ticket = Ticket {
        ticket_id: String::new(),
        title,
        seen,
        kind: asked.kind,
        concerns: subject,
        step,
        operation,
        reason,
        paths,
        references,
        filed_by: actor.author.clone(),
        idempotency_key: if by_plugin { key } else { String::new() },
        state: State::Open,
        suspect: !matched.is_empty(),
        matched_rules: quarantine::names(&matched),
        seen_count: 1,
        first_seen_ns: now,
        last_seen_ns: now,
        filed_at_ns: now,
        ..Ticket::default()
    };
    ticket.fingerprint = fingerprint(&ticket);

    let tickets = &app.tickets;
    // A repeat folds into the open ticket under its key.
    let mut recurrence_of = None;
    if by_plugin {
        let (instance, key) = (
            ticket.filed_by.instance.clone(),
            ticket.idempotency_key.clone(),
        );
        let under_key = tickets
            .blocking(move |store| store.by_key(&instance, &key))
            .await?;
        if let Some(open) = under_key.iter().find(|t| t.state == State::Open) {
            return fold(app, open, &ticket, now).await;
        }
        recurrence_of = under_key.into_iter().next();
    } else {
        let subject = actor.author.subject.clone();
        let since = now - DAY_NS;
        let filed = tickets
            .blocking(move |store| store.filed_since(&subject, since))
            .await?;
        if filed >= PERSON_FILINGS_A_DAY {
            return Err(Refused::at(
                429,
                "rate_limited",
                "",
                format!(
                    "you have filed {PERSON_FILINGS_A_DAY} tickets today, the most a person \
                     files in a day; add a note to one of them instead"
                ),
            ));
        }
    }

    // The rules' advice, at once.
    let all = tickets.blocking(|store| store.tickets()).await?;
    let filer = Reader {
        subject: actor.author.subject.clone(),
        access: filer_access,
        through_delegation: false,
    };
    let instance = ticket.concerns.instance.clone();
    let context = rules::Context {
        open_seen: all
            .iter()
            .filter(|t| t.state == State::Open && visibility::may_see(&filer, t))
            .collect(),
        recurrence_of: recurrence_of.as_ref(),
        missing_settings: records
            .plugin_settings
            .iter()
            .find(|s| s.plugin_instance_id == instance)
            .map(|record| {
                crate::admin::settings::missing(record, crate::html::is_development())
                    .into_iter()
                    .map(|d| d.name.clone())
                    .collect()
            })
            .unwrap_or_default(),
        unlinked: app
            .custody
            .view()
            .unlinked(&records.links)
            .into_iter()
            .filter(|u| u.plugin_instance_id == instance)
            .map(|u| u.external_account_id)
            .collect(),
    };
    ticket.ticket_id = format!("TKT-{}", tickets.ulid(now));
    ticket.notes = rules::at_filing(&ticket, &context)
        .into_iter()
        .enumerate()
        .map(|(n, advice)| Note {
            number: n as i32 + 1,
            kind: TicketNoteKind::Advice as i32,
            author: Author::rules(),
            noted_ns: now,
            note: advice.note,
            suspect: false,
            matched_rules: Vec::new(),
        })
        .collect();
    ticket.note_count = ticket.notes.len() as i32;
    // A notice of kind filed to everyone who may work it, besides the filer
    // (Q4), so a ticket about a plugin reaches someone who can act.
    let people: Vec<String> = workers(&records, &ticket)
        .into_iter()
        .filter(|s| *s != ticket.filed_by.subject)
        .collect();
    let notices = notices_to(
        tickets,
        &people,
        &ticket.ticket_id,
        "filed",
        &ticket.filed_by,
        now,
    );
    let keeping = ticket.clone();
    match tickets
        .blocking(move |store| store.insert(&keeping, &notices))
        .await?
    {
        Inserted::Made => {
            tracing::info!(
                ticket = ticket.ticket_id,
                concerns = ticket.concerns.named(),
                provenance = ticket.filed_by.provenance.as_str(),
                instance = ticket.filed_by.instance,
                suspect = ticket.suspect,
                "a ticket was filed"
            );
            Ok(FileTicketReply {
                ticket_id: ticket.ticket_id,
                outcome: "made".into(),
                seen_count: 1,
            })
        }
        // Another filing under the key got there first.
        Inserted::KeyHeld(held) => {
            let open = tickets
                .blocking(move |store| store.ticket(&held))
                .await?
                .ok_or_else(Refused::not_found)?;
            fold(app, &open, &ticket, now).await
        }
    }
}

/// A plugin's repeat: the open ticket's seen count and last seen, and its
/// seen text, checked again, where it changed (W4.12).
async fn fold(
    app: &App,
    open: &Ticket,
    again: &Ticket,
    now: i64,
) -> Result<FileTicketReply, Refused> {
    let seen = (again.seen != open.seen).then(|| {
        let matched = quarantine::matched(&format!("{}\n{}", open.title, again.seen));
        (
            again.seen.clone(),
            !matched.is_empty(),
            quarantine::names(&matched),
        )
    });
    let id = open.ticket_id.clone();
    let count = app
        .tickets
        .blocking(move |store| {
            store.fold(
                &id,
                seen.as_ref()
                    .map(|(text, suspect, rules)| (text.as_str(), *suspect, rules.as_slice())),
                now,
            )
        })
        .await?;
    Ok(FileTicketReply {
        ticket_id: open.ticket_id.clone(),
        outcome: "unchanged".into(),
        seen_count: count,
    })
}

// ── ListTickets, ReadTicket, CountTickets (W6.22) ────────────────────────────

/// What a list is cut to, beyond the reader's access.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// An instance, a part of core, or `platform`.
    pub concerns: String,
    pub state: Option<State>,
    /// Only tickets the reader filed or owns.
    pub mine: bool,
    /// Only tickets held as suspect.
    pub suspect: bool,
}

impl Filter {
    /// From a query's or a tool's words; a state not of the three is
    /// refused by path.
    pub fn parse(
        concerns: &str,
        state: &str,
        mine: bool,
        suspect: bool,
    ) -> Result<Filter, Refused> {
        let state = match state.trim() {
            "" => None,
            said => Some(
                State::parse(said)
                    .ok_or_else(|| Refused::invalid("state", "open, resolved or closed"))?,
            ),
        };
        Ok(Filter {
            concerns: concerns.trim().to_string(),
            state,
            mine,
            suspect,
        })
    }

    fn admits(&self, reader: &Reader, ticket: &Ticket) -> bool {
        (self.concerns.is_empty() || ticket.concerns.named() == self.concerns)
            && self.state.is_none_or(|state| ticket.state == state)
            && (!self.mine
                || ticket.filed_by.subject == reader.subject
                || ticket.owner == reader.subject)
            && (!self.suspect || ticket.suspect)
    }
}

/// The tickets the reader may see now, newest first, cut by `filter`.
pub async fn list(app: &App, reader: &Reader, filter: &Filter) -> Result<Vec<Ticket>, Refused> {
    let all = app.tickets.blocking(|store| store.tickets()).await?;
    Ok(all
        .into_iter()
        .filter(|t| visibility::may_see(reader, t) && filter.admits(reader, t))
        .collect())
}

/// One ticket with its notes, if the reader may see it; one they may not is
/// answered as not found, as one that does not exist is.
pub async fn read(app: &App, reader: &Reader, ticket_id: &str) -> Result<Ticket, Refused> {
    let id = ticket_id.to_string();
    match app.tickets.blocking(move |store| store.ticket(&id)).await? {
        Some(ticket) if visibility::may_see(reader, &ticket) => Ok(ticket),
        _ => Err(Refused::not_found()),
    }
}

/// The firm's tally (requirement 45): the tickets the reader may see,
/// counted by what they concern and their state, as asked. Counts carry no
/// text and never leave the deployment.
pub async fn count(app: &App, reader: &Reader, by: &[String]) -> Result<Value, Refused> {
    for (n, field) in by.iter().enumerate() {
        if field != "concerns" && field != "state" {
            return Err(Refused::invalid(&format!("by[{n}]"), "concerns or state"));
        }
    }
    let by_concerns = by.iter().any(|b| b == "concerns");
    let by_state = by.iter().any(|b| b == "state");
    let mut counted: BTreeMap<(String, &'static str), i64> = BTreeMap::new();
    for ticket in list(app, reader, &Filter::default()).await? {
        let key = (
            if by_concerns {
                ticket.concerns.named().to_string()
            } else {
                String::new()
            },
            if by_state { ticket.state.as_str() } else { "" },
        );
        *counted.entry(key).or_default() += 1;
    }
    let counts: Vec<Value> = counted
        .into_iter()
        .map(|((concerns, state), tickets)| {
            let mut one = json!({"tickets": tickets});
            if by_concerns {
                one["concerns"] = concerns.into();
            }
            if by_state {
                one["state"] = state.into();
            }
            one
        })
        .collect();
    Ok(json!({"counts": counts}))
}

// ── AddTicketNote (W6.22) ────────────────────────────────────────────────────

/// A note or advice on a ticket the actor may see. A change is a person's
/// act at the page, never a note: refused naming `kind`.
pub async fn add_note(
    app: &App,
    actor: &Actor,
    ticket_id: &str,
    kind: &str,
    note: &str,
) -> Result<i32, Refused> {
    let kind = match parse_note_kind(kind) {
        Some(k) if k == TicketNoteKind::Note as i32 || k == TicketNoteKind::Advice as i32 => k,
        _ => {
            return Err(Refused::invalid(
                "kind",
                "note or advice: a change is a person's act on the ticket's page, and no tool \
                 acts on a ticket",
            ))
        }
    };
    let note = checked_text(
        "note",
        note,
        Some((TICKET_NOTE_NOTE_LENGTH.least, TICKET_NOTE_NOTE_LENGTH.most)),
    )?;
    let now = app.clock.now_ns();
    let records = app
        .records
        .current(now)
        .map_err(|stale| Refused::unavailable(stale.to_string()))?;
    let ticket = read(app, &actor.reader(), ticket_id).await?;
    let matched = quarantine::matched(&note);
    let kept = Note {
        number: 0,
        kind,
        author: actor.author.clone(),
        noted_ns: now,
        suspect: !matched.is_empty(),
        matched_rules: quarantine::names(&matched),
        note: note.clone(),
    };
    let people = concerned(&records, &ticket, &actor.author.subject, &[]);
    let notice_kind = if kind == TicketNoteKind::Advice as i32 {
        "advised"
    } else {
        "noted"
    };
    let notices = notices_to(
        &app.tickets,
        &people,
        &ticket.ticket_id,
        notice_kind,
        &actor.author,
        now,
    );
    let id = ticket.ticket_id.clone();
    let number = app
        .tickets
        .blocking(move |store| store.add_note(&id, &kept, &notices))
        .await?;
    // On each change, the rules: a credential's shape in the note.
    if let Some(advice) = rules::credential(&note) {
        let advice = Note {
            kind: TicketNoteKind::Advice as i32,
            author: Author::rules(),
            noted_ns: now,
            note: advice.note,
            ..Note::default()
        };
        let people = concerned(
            &records,
            &ticket,
            "",
            std::slice::from_ref(&actor.author.subject),
        );
        let notices = notices_to(
            &app.tickets,
            &people,
            &ticket.ticket_id,
            "advised",
            &Author::rules(),
            now,
        );
        let id = ticket.ticket_id.clone();
        app.tickets
            .blocking(move |store| store.add_note(&id, &advice, &notices))
            .await?;
    }
    Ok(number)
}

// ── WorkTicket (W6.23) ───────────────────────────────────────────────────────

/// One act, as the page posts it (the dictionary's WorkTicketRequest).
#[derive(Debug, Clone, Default)]
pub struct Act {
    pub ticket_id: String,
    pub act: String,
    pub owner: String,
    pub due: String,
    pub resolution: String,
    pub cites: String,
    pub release: String,
    /// The notes the person saw when they acted.
    pub against_notes: Option<i32>,
}

fn is_date(said: &str) -> bool {
    let b = said.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
        && (1..=12).contains(&said[5..7].parse::<u32>().unwrap_or(0))
        && (1..=31).contains(&said[8..10].parse::<u32>().unwrap_or(0))
}

/// A person's act on a ticket, at its page, in their own session -- never a
/// tool's or a delegation's: recorded as a note of kind change in the same
/// transaction (W6.23). The ticket as it stands after.
pub async fn work(
    app: &App,
    subject: &str,
    person: &str,
    access: &Access,
    asked: &Act,
) -> Result<Ticket, Refused> {
    let now = app.clock.now_ns();
    let records = app
        .records
        .current(now)
        .map_err(|stale| Refused::unavailable(stale.to_string()))?;
    let reader = Reader {
        subject: subject.to_string(),
        access: access.clone(),
        through_delegation: false,
    };
    let ticket = read(app, &reader, &asked.ticket_id).await?;
    let works = visibility::may_work(subject, access, &ticket);
    let author = Author {
        provenance: Provenance::Person,
        subject: subject.to_string(),
        person: person.to_string(),
        ..Author::default()
    };
    let mut change = Change::of(&ticket);
    let mut also = Vec::new();
    let refuse_not_worker = || {
        Refused::forbidden(
            "act",
            "you may not work this ticket: it is worked by a person holding write on what it \
             concerns and every account it names, or, naming none, by its admins",
        )
    };
    let (said, notice_kind) = match asked.act.as_str() {
        "assign" => {
            if !works {
                return Err(refuse_not_worker());
            }
            let owner = asked.owner.trim();
            if owner.is_empty() {
                change.owner.clear();
                change.owner_name.clear();
                ("Unassigned.".to_string(), "assigned")
            } else {
                let owner_reader = Reader {
                    subject: owner.to_string(),
                    access: access_of(&records, owner),
                    through_delegation: false,
                };
                let known = records.people.iter().any(|p| p.subject == owner);
                if !known || !visibility::may_see(&owner_reader, &ticket) {
                    return Err(Refused::invalid(
                        "owner",
                        "an owner is a person who may see the ticket",
                    ));
                }
                change.owner = owner.to_string();
                change.owner_name = display_name(&records, owner);
                also.push(owner.to_string());
                (format!("Assigned to {}.", change.owner_name), "assigned")
            }
        }
        "due" => {
            if !works {
                return Err(refuse_not_worker());
            }
            let due = asked.due.trim();
            if !due.is_empty() && !is_date(due) {
                return Err(Refused::invalid("due", "a date, as 2026-10-31"));
            }
            change.due = due.to_string();
            (
                if due.is_empty() {
                    "Due date cleared.".to_string()
                } else {
                    format!("Due {due}.")
                },
                "due",
            )
        }
        "resolve" => {
            if !works {
                return Err(refuse_not_worker());
            }
            if ticket.state != State::Open {
                return Err(Refused::invalid("act", "only an open ticket is resolved"));
            }
            let resolution = parse_resolution(&asked.resolution);
            let cites = asked.cites.trim();
            match resolution {
                Some(r) if r == TicketResolution::Note as i32 => {
                    let number: i32 = cites.parse().unwrap_or(0);
                    if !ticket.notes.iter().any(|n| n.number == number) {
                        return Err(Refused::invalid(
                            "cites",
                            "the number of one of the ticket's notes, from 1",
                        ));
                    }
                }
                Some(r) if r == TicketResolution::Version as i32 => {
                    if cites.is_empty() || text::refused(cites).is_some() {
                        return Err(Refused::invalid("cites", "the version that resolves it"));
                    }
                }
                Some(r) if r == TicketResolution::Answer as i32 => {
                    return Err(Refused::invalid(
                        "resolution",
                        "no answer reaches the deployment yet: resolve citing a note or a version",
                    ))
                }
                _ => {
                    return Err(Refused::invalid(
                        "resolution",
                        "a ticket is resolved citing a note or a version",
                    ))
                }
            }
            change.state = State::Resolved;
            change.resolution = resolution.unwrap_or_default();
            change.cites = cites.to_string();
            (
                format!(
                    "Resolved, citing {} {cites}.",
                    resolution_name(change.resolution)
                ),
                "resolved",
            )
        }
        "close" => {
            if ticket.state != State::Open {
                return Err(Refused::invalid("act", "only an open ticket is closed"));
            }
            let resolution = parse_resolution(&asked.resolution);
            let cites = asked.cites.trim();
            match resolution {
                Some(r) if r == TicketResolution::Withdrawn as i32 => {
                    if !(works || visibility::may_withdraw(subject, &ticket)) {
                        return Err(refuse_not_worker());
                    }
                }
                Some(r) if r == TicketResolution::Duplicate as i32 => {
                    if !works {
                        return Err(refuse_not_worker());
                    }
                    let other = read(app, &reader, cites)
                        .await
                        .map_err(|_| Refused::invalid("cites", "the other ticket's ticket_id"))?;
                    if other.ticket_id == ticket.ticket_id {
                        return Err(Refused::invalid("cites", "another ticket, not this one"));
                    }
                }
                Some(r) if r == TicketResolution::NotAProblem as i32 => {
                    if !works {
                        return Err(refuse_not_worker());
                    }
                }
                _ => {
                    return Err(Refused::invalid(
                        "resolution",
                        "a ticket is closed as withdrawn, a duplicate or not a problem",
                    ))
                }
            }
            change.state = State::Closed;
            change.resolution = resolution.unwrap_or_default();
            change.cites = if change.resolution == TicketResolution::Duplicate as i32 {
                cites.to_string()
            } else {
                String::new()
            };
            (
                match change.resolution {
                    r if r == TicketResolution::Duplicate as i32 => {
                        format!("Closed as a duplicate of {cites}.")
                    }
                    r if r == TicketResolution::Withdrawn as i32 => "Closed as withdrawn.".into(),
                    _ => "Closed as not a problem.".into(),
                },
                "closed",
            )
        }
        "reopen" => {
            if !works {
                return Err(refuse_not_worker());
            }
            if ticket.state == State::Open {
                return Err(Refused::invalid("act", "the ticket is open"));
            }
            change.state = State::Open;
            change.resolution = 0;
            change.cites.clear();
            ("Reopened.".to_string(), "reopened")
        }
        "release" => {
            if !works {
                return Err(refuse_not_worker());
            }
            let which = asked.release.trim();
            let author_of_text = if which == "ticket" {
                if !ticket.suspect {
                    return Err(Refused::invalid("release", "the ticket's text is not held"));
                }
                change.release_ticket = true;
                &ticket.filed_by
            } else {
                let number: i32 = which.parse().unwrap_or(0);
                match ticket.notes.iter().find(|n| n.number == number) {
                    Some(note) if note.suspect => {
                        change.release_note = Some(number);
                        &note.author
                    }
                    _ => {
                        return Err(Refused::invalid(
                            "release",
                            "ticket, or the number of a note held as suspect",
                        ))
                    }
                }
            };
            // Q5: never by the text's own author, so filing and releasing
            // is not two clicks of one person's.
            if author_of_text.subject == subject {
                return Err(Refused::forbidden(
                    "release",
                    "a text is released by a person who may work the ticket and did not \
                     write it",
                ));
            }
            (
                if which == "ticket" {
                    "Released the ticket's text to tools.".to_string()
                } else {
                    format!("Released note {which} to tools.")
                },
                "released",
            )
        }
        _ => {
            return Err(Refused::invalid(
                "act",
                "assign, due, resolve, close, reopen or release",
            ))
        }
    };
    let note = Note {
        number: 0,
        kind: TicketNoteKind::Change as i32,
        author: author.clone(),
        noted_ns: now,
        note: said,
        suspect: false,
        matched_rules: Vec::new(),
    };
    let mut after = ticket.clone();
    change.apply(&mut after);
    let people = concerned(&records, &after, subject, &also);
    let notices = notices_to(
        &app.tickets,
        &people,
        &ticket.ticket_id,
        notice_kind,
        &author,
        now,
    );
    let against = asked.against_notes.unwrap_or(ticket.notes.len() as i32);
    let id = ticket.ticket_id.clone();
    let keeping = change.clone();
    let done = app
        .tickets
        .blocking(move |store| store.change(&id, against, &keeping, &note, &notices))
        .await?;
    if !done {
        return Err(Refused::at(
            409,
            "conflict",
            "",
            "the ticket changed since you opened it: read it again, and act on what it says now",
        ));
    }
    tracing::info!(
        ticket = ticket.ticket_id,
        act = asked.act,
        by = subject,
        "a person worked a ticket"
    );
    // On each change, the rules: a duplicate, when it reopens.
    if asked.act == "reopen" {
        let all = app.tickets.blocking(|store| store.tickets()).await?;
        let context = rules::Context {
            open_seen: all
                .iter()
                .filter(|t| t.state == State::Open && visibility::may_see(&reader, t))
                .collect(),
            ..rules::Context::default()
        };
        if let Some(advice) = rules::duplicate(&after, &context) {
            let advice = Note {
                kind: TicketNoteKind::Advice as i32,
                author: Author::rules(),
                noted_ns: now,
                note: advice.note,
                ..Note::default()
            };
            let id = ticket.ticket_id.clone();
            app.tickets
                .blocking(move |store| store.add_note(&id, &advice, &[]))
                .await?;
        }
    }
    read(app, &reader, &ticket.ticket_id).await
}

// ── ReadInbox, MarkNoticesRead (W6.24) ───────────────────────────────────────

/// What is new in a person's inbox for one reader -- a delegation, or the
/// person's pages (`reader` empty) -- cut to the tickets the reader may see,
/// and the reader's place advanced past it, so each client sees every new
/// notice once. With each notice, its ticket as it stands.
pub async fn read_inbox(
    app: &App,
    reader: &Reader,
    place: &str,
) -> Result<Vec<(Notice, Ticket)>, Refused> {
    let (subject, place_of) = (reader.subject.clone(), place.to_string());
    let notices = app
        .tickets
        .blocking(move |store| {
            let after = store.cursor(&subject, &place_of)?;
            let notices = store.notices(&subject, &after, INBOX_PAGE)?;
            if let Some(last) = notices.last() {
                store.advance(&subject, &place_of, &last.notice_id)?;
            }
            Ok(notices)
        })
        .await?;
    let visible = app.tickets.visible_by_id(reader).await?;
    Ok(notices
        .into_iter()
        .filter_map(|n| visible.get(&n.ticket_id).cloned().map(|t| (n, t)))
        .collect())
}

/// A person's latest notices, newest first, for their Inbox page.
pub async fn latest(app: &App, reader: &Reader) -> Result<Vec<(Notice, Ticket)>, Refused> {
    let subject = reader.subject.clone();
    let notices = app
        .tickets
        .blocking(move |store| store.latest_notices(&subject, INBOX_PAGE))
        .await?;
    let visible = app.tickets.visible_by_id(reader).await?;
    Ok(notices
        .into_iter()
        .filter_map(|n| visible.get(&n.ticket_id).cloned().map(|t| (n, t)))
        .collect())
}

/// Mark the person's own notices of these tickets read. Changes no ticket.
pub async fn mark_read(app: &App, subject: &str, ticket_ids: &[String]) -> Result<usize, Refused> {
    if ticket_ids.is_empty() {
        return Err(Refused::invalid(
            "ticket_ids",
            "names the tickets whose notices to mark read",
        ));
    }
    for (n, id) in ticket_ids.iter().enumerate() {
        if text::refused(id).is_some() || id.is_empty() {
            return Err(Refused::invalid(&format!("ticket_ids[{n}]"), "a ticket_id"));
        }
    }
    let (subject, ids) = (subject.to_string(), ticket_ids.to_vec());
    app.tickets
        .blocking(move |store| store.mark_read(&subject, &ids))
        .await
}

// ── ReadFiledTickets (W4.12) ─────────────────────────────────────────────────

/// What became of the tickets `instance` filed: by the tickets named, by
/// the keys named, or everything since a cursor. State, resolution, seen
/// counts and answers only: never a note, an owner or a due date
/// (requirement 11).
pub async fn filed(
    app: &App,
    instance: &str,
    asked: &ReadFiledTicketsRequest,
) -> Result<ReadFiledTicketsReply, Refused> {
    let theirs =
        |t: &Ticket| t.filed_by.provenance == Provenance::Plugin && t.filed_by.instance == instance;
    let mut found = Vec::new();
    let mut next_cursor = String::new();
    if !asked.ticket_ids.is_empty() {
        for id in &asked.ticket_ids {
            let id = id.clone();
            if let Some(t) = app.tickets.blocking(move |store| store.ticket(&id)).await? {
                if theirs(&t) {
                    found.push(t);
                }
            }
        }
    } else if !asked.idempotency_keys.is_empty() {
        for key in &asked.idempotency_keys {
            let (i, k) = (instance.to_string(), key.clone());
            found.extend(
                app.tickets
                    .blocking(move |store| store.by_key(&i, &k))
                    .await?,
            );
        }
    } else {
        let (i, after) = (instance.to_string(), asked.cursor.clone());
        found = app
            .tickets
            .blocking(move |store| store.filed_by(&i, &after, FILED_PAGE))
            .await?;
        if found.len() == FILED_PAGE {
            next_cursor = found
                .last()
                .map(|t| t.ticket_id.clone())
                .unwrap_or_default();
        }
    }
    Ok(ReadFiledTicketsReply {
        tickets: found
            .iter()
            .filter(|t| theirs(t))
            .map(|t| FiledTicket {
                ticket_id: t.ticket_id.clone(),
                idempotency_key: t.idempotency_key.clone(),
                state: t.state.wire() as i32,
                resolution: t.resolution,
                seen_count: t.seen_count,
                first_seen_ns: t.first_seen_ns,
                last_seen_ns: t.last_seen_ns,
                answers: Vec::new(),
            })
            .collect(),
        next_cursor,
    })
}

#[cfg(test)]
mod tests;
