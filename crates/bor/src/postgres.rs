//! The book in Postgres, behind the same trait the in-memory store answers.
//!
//! Synchronous, as the street store is: the bus runs request handlers on a
//! blocking pool already.
//!
//! # One writer per partition
//!
//! Every command takes its account's partition lock -- an advisory lock for
//! the transaction, then the partition head's row -- before it reads the
//! account's journal, so two commands on one partition are decided one after
//! the other, each against what the other journalled, and a rolling update
//! running two `meridian-bor` processes still has one writer per partition
//! at a time (W9.8). The head's row is moved in the same transaction, so a
//! rollback takes its numbers back with it: no holes.
//!
//! # The journal is the record
//!
//! A command is decided against the account replayed from its journal, never
//! against a projection; the projections are written beside the entry, read
//! by the reads, and rebuilt from the journal whole by [`Store::rebuild`].

use postgres::{GenericClient, IsolationLevel, NoTls, Row, Transaction};
use prost::Message;
use r2d2_postgres::PostgresConnectionManager;

use meridian_domain::v1::{
    AccountAttributes, AccountFigures, BookPosition, Break, BreakState, ChangeCause, EntryMeta,
};

use crate::book::{agreement_key, Book, Changes};
use crate::journal::{Body, Entry};
use crate::migrations;
use crate::reads;
use crate::store::{
    Acted, AttributesRead, BreaksRead, Decide, FiguresRead, Mark, Page, PositionsRead, Result,
    Scope, Store, StoreError, FIRST_PARTITION,
};

type Pool = r2d2::Pool<PostgresConnectionManager<NoTls>>;
type Connection = r2d2::PooledConnection<PostgresConnectionManager<NoTls>>;

/// Names the schema lock: distinct from every other store's.
const SCHEMA_LOCK: i64 = 0x626f_6f6b_0000_0001_u64 as i64;

/// The partition locks' space: a partition's lock is this with the
/// partition's hash in its low half.
const PARTITION_LOCKS: i64 = 0x626f_6f6b_0000_0000_u64 as i64;

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
    /// release by `meridian-bor migrate`, never by a starting process.
    pub fn migrate(&self, clock: &dyn meridian_clock::Clock) -> Result<()> {
        let mut conn = self.conn()?;
        conn.execute("SELECT pg_advisory_lock($1)", &[&SCHEMA_LOCK])
            .map_err(unavailable)?;
        let outcome = apply_migrations(&mut conn, clock);
        let _ = conn.execute("SELECT pg_advisory_unlock($1)", &[&SCHEMA_LOCK]);
        outcome
    }

    /// What a start does instead: one read, no lock.
    pub fn verify(&self) -> Result<()> {
        let mut conn = self.conn()?;
        let exists = conn
            .query_opt(
                "SELECT 1 FROM information_schema.tables
                  WHERE table_schema = current_schema() AND table_name = 'book_schema_migration'",
                &[],
            )
            .map_err(unavailable)?
            .is_some();
        let applied = if exists {
            conn.query_one("SELECT max(version) FROM book_schema_migration", &[])
                .map_err(unavailable)?
                .get::<_, Option<i64>>(0)
        } else {
            None
        };
        migrations::verify(applied)
    }

    fn conn(&self) -> Result<Connection> {
        self.pool.get().map_err(unavailable)
    }

    /// A read's transaction: every record and the heads as of one snapshot.
    fn reading<T>(&self, read: impl FnOnce(&mut Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut conn = self.conn()?;
        let mut tx = conn
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .map_err(unavailable)?;
        let out = read(&mut tx)?;
        tx.commit().map_err(unavailable)?;
        Ok(out)
    }
}

const ENTRY_COLUMNS: &str = "entry_id, account_id, partition, first_sequence, last_sequence,
        message_id, idempotency_key, meta, cause, body";

fn entry_of(row: &Row) -> Result<Entry> {
    let meta: Vec<u8> = row.get(7);
    let cause: Vec<u8> = row.get(8);
    let body: Vec<u8> = row.get(9);
    Ok(Entry {
        entry_id: row.get(0),
        account_id: row.get(1),
        partition: row.get(2),
        first_sequence: row.get::<_, i64>(3) as u64,
        last_sequence: row.get::<_, i64>(4) as u64,
        message_id: row.get(5),
        idempotency_key: row.get(6),
        meta: EntryMeta::decode(meta.as_slice()).map_err(unreadable)?,
        cause: ChangeCause::decode(cause.as_slice()).map_err(unreadable)?,
        body: Body::from_bytes(&body)?,
    })
}

fn journal_of(client: &mut impl GenericClient, account_id: &str) -> Result<Vec<Entry>> {
    client
        .query(
            &format!(
                "SELECT {ENTRY_COLUMNS} FROM book_entry WHERE account_id = $1
                  ORDER BY first_sequence"
            ),
            &[&account_id],
        )
        .map_err(unavailable)?
        .iter()
        .map(entry_of)
        .collect()
}

fn heads_of(client: &mut impl GenericClient) -> Result<Mark> {
    Ok(client
        .query("SELECT partition, sequence FROM book_partition_head", &[])
        .map_err(unavailable)?
        .into_iter()
        .map(|row| (row.get::<_, String>(0), row.get::<_, i64>(1).max(0) as u64))
        .collect())
}

/// A 32-bit hash of a partition's name, stable across releases (FNV-1a).
fn partition_lock(partition: &str) -> i64 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in partition.bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    PARTITION_LOCKS | hash as i64
}

/// A number as Postgres keeps it: text, read by `::numeric` at the scale it
/// was stated with.
fn numeric(value: &Option<meridian_pb::v1::Decimal>) -> Option<String> {
    value
        .as_ref()
        .and_then(|wire| meridian_domain::exact::Exact::from_wire(wire).ok())
        .map(|exact| exact.to_string())
}

/// Who answered for an act, as a person reads it.
pub fn actor_named(actor: Option<&meridian_domain::v1::Actor>) -> String {
    use meridian_domain::v1::actor::Kind;
    match actor.and_then(|actor| actor.kind.as_ref()) {
        Some(Kind::Person(person)) => person.subject.clone(),
        Some(Kind::System(system)) if !system.instance_id.is_empty() => {
            format!("instance {}", system.instance_id)
        }
        _ => "book".into(),
    }
}

/// A break's subject, as a person reads it.
fn subject_named(record: &Break) -> (String, String) {
    match record.subject.as_ref() {
        Some(meridian_domain::v1::r#break::Subject::Position(key)) => (
            key.instrument_id.clone(),
            format!(
                "{}|{}",
                key.instrument_id,
                crate::book::side_named(key.side)
            ),
        ),
        Some(meridian_domain::v1::r#break::Subject::Figure(key)) => (
            key.instrument_id.clone(),
            format!(
                "figure {}{}",
                key.figure,
                if key.instrument_id.is_empty() {
                    String::new()
                } else {
                    format!(" of {}", key.instrument_id)
                }
            ),
        ),
        None => (String::new(), String::new()),
    }
}

/// Write the records an entry changed into the projections.
fn project(tx: &mut Transaction<'_>, account_id: &str, changes: &Changes) -> Result<()> {
    for (record, _) in &changes.positions {
        let change = record.last_change.clone().unwrap_or_default();
        tx.execute(
            "INSERT INTO book_position
                    (account_id, instrument_id, side, partition, sequence, removed,
                     trade_date_quantity, settled_quantity, not_stated_quantity, effective_date,
                     record)
             VALUES ($1, $2, $3, $4, $5, $6, $7::text::numeric, $8::text::numeric,
                     $9::text::numeric, $10, $11)
             ON CONFLICT (account_id, instrument_id, side) DO UPDATE
                SET partition = EXCLUDED.partition, sequence = EXCLUDED.sequence,
                    removed = EXCLUDED.removed,
                    trade_date_quantity = EXCLUDED.trade_date_quantity,
                    settled_quantity = EXCLUDED.settled_quantity,
                    not_stated_quantity = EXCLUDED.not_stated_quantity,
                    effective_date = EXCLUDED.effective_date, record = EXCLUDED.record",
            &[
                &account_id,
                &record.instrument_id,
                &record.side,
                &change.partition,
                &(change.sequence as i64),
                &record.removed,
                &numeric(&record.trade_date_quantity).unwrap_or_else(|| "0".into()),
                &numeric(&record.settled_quantity),
                &numeric(&record.not_stated_quantity).unwrap_or_else(|| "0".into()),
                &record.effective_date,
                &record.encode_to_vec(),
            ],
        )
        .map_err(unavailable)?;
        tx.execute(
            "DELETE FROM book_lot WHERE account_id = $1 AND instrument_id = $2 AND side = $3",
            &[&account_id, &record.instrument_id, &record.side],
        )
        .map_err(unavailable)?;
        tx.execute(
            "DELETE FROM book_pending WHERE account_id = $1 AND instrument_id = $2 AND side = $3",
            &[&account_id, &record.instrument_id, &record.side],
        )
        .map_err(unavailable)?;
        for (ordinal, lot) in record.lots.iter().enumerate() {
            let terms = lot.terms.clone().unwrap_or_default();
            let cost = terms.cost.as_ref();
            tx.execute(
                "INSERT INTO book_lot
                        (account_id, instrument_id, side, lot_id, ordinal, open_quantity,
                         original_quantity, cost, cost_currency, acquired_date, source)
                 VALUES ($1, $2, $3, $4, $5, $6::text::numeric, $7::text::numeric,
                         $8::text::numeric, $9, $10, $11)",
                &[
                    &account_id,
                    &record.instrument_id,
                    &record.side,
                    &lot.lot_id,
                    &(ordinal as i32),
                    &numeric(&lot.open_quantity).unwrap_or_else(|| "0".into()),
                    &numeric(&lot.original_quantity).unwrap_or_else(|| "0".into()),
                    &cost.and_then(|cost| numeric(&cost.amount)),
                    &cost.map(|cost| cost.currency_code.clone()),
                    &terms.acquired_date,
                    &terms.source,
                ],
            )
            .map_err(unavailable)?;
        }
        for pending in &record.pending {
            tx.execute(
                "INSERT INTO book_pending
                        (account_id, instrument_id, side, value_date, quantity, failing)
                 VALUES ($1, $2, $3, $4, $5::text::numeric, $6)",
                &[
                    &account_id,
                    &record.instrument_id,
                    &record.side,
                    &pending.value_date,
                    &numeric(&pending.quantity).unwrap_or_else(|| "0".into()),
                    &pending.state.as_ref().is_some_and(|state| state.failing),
                ],
            )
            .map_err(unavailable)?;
        }
    }
    for record in &changes.breaks {
        let change = record.last_change.clone().unwrap_or_default();
        let (instrument, subject) = subject_named(record);
        let resolution = match record.resolution.as_ref() {
            _ if record.state == BreakState::Open as i32 => "",
            Some(held) if held.cleared_at.is_some() => "cleared",
            Some(held) if !held.explanation.is_empty() => "explanation",
            Some(_) => "entries",
            None => "",
        };
        tx.execute(
            "INSERT INTO book_break
                    (account_id, break_id, state, instrument_id, partition, sequence, category,
                     subject, first_seen, last_seen, recorded_by, cause, resolution, record)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
             ON CONFLICT (account_id, break_id) DO UPDATE
                SET state = EXCLUDED.state, instrument_id = EXCLUDED.instrument_id,
                    partition = EXCLUDED.partition, sequence = EXCLUDED.sequence,
                    category = EXCLUDED.category, subject = EXCLUDED.subject,
                    first_seen = EXCLUDED.first_seen, last_seen = EXCLUDED.last_seen,
                    recorded_by = EXCLUDED.recorded_by, cause = EXCLUDED.cause,
                    resolution = EXCLUDED.resolution, record = EXCLUDED.record",
            &[
                &account_id,
                &record.break_id,
                &record.state,
                &instrument,
                &change.partition,
                &(change.sequence as i64),
                &record.category,
                &subject,
                &record.first_seen_date,
                &record.last_seen_date,
                &actor_named(record.recorded_by.as_ref()),
                &record
                    .confirmed_cause
                    .as_ref()
                    .map(|cause| cause.category)
                    .unwrap_or_default(),
                &resolution,
                &record.encode_to_vec(),
            ],
        )
        .map_err(unavailable)?;
    }
    for record in &changes.figures {
        let change = record.last_change.clone().unwrap_or_default();
        tx.execute(
            "INSERT INTO book_figures
                    (account_id, agreement_key, business_date, partition, sequence, statement_id,
                     record)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (account_id, agreement_key, business_date) DO UPDATE
                SET partition = EXCLUDED.partition, sequence = EXCLUDED.sequence,
                    statement_id = EXCLUDED.statement_id, record = EXCLUDED.record",
            &[
                &account_id,
                &agreement_key(record.agreement.as_ref()),
                &record.business_date,
                &change.partition,
                &(change.sequence as i64),
                &record
                    .source
                    .as_ref()
                    .map(|source| source.statement_id.clone())
                    .unwrap_or_default(),
                &record.encode_to_vec(),
            ],
        )
        .map_err(unavailable)?;
    }
    if let Some(record) = &changes.attributes {
        let change = record.last_change.clone().unwrap_or_default();
        tx.execute(
            "INSERT INTO book_attributes
                    (account_id, partition, sequence, base_currency_code, lot_relief_default,
                     opening_as_of, record)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (account_id) DO UPDATE
                SET partition = EXCLUDED.partition, sequence = EXCLUDED.sequence,
                    base_currency_code = EXCLUDED.base_currency_code,
                    lot_relief_default = EXCLUDED.lot_relief_default,
                    opening_as_of = EXCLUDED.opening_as_of, record = EXCLUDED.record",
            &[
                &account_id,
                &change.partition,
                &(change.sequence as i64),
                &record.base_currency_code,
                &record.lot_relief_default,
                &record
                    .opening_balance
                    .as_ref()
                    .map(|opening| opening.as_of_date.clone())
                    .unwrap_or_default(),
                &record.encode_to_vec(),
            ],
        )
        .map_err(unavailable)?;
    }
    Ok(())
}

/// Every record of an account, as one replay of its journal makes them: what
/// a rebuild writes.
fn all_changes(book: &Book) -> Result<Changes> {
    let mut changes = Changes::default();
    for position in book.positions.values() {
        changes.positions.push((
            position.record(&book.account_id)?,
            meridian_domain::exact::Exact::ZERO,
        ));
    }
    changes.breaks = book.breaks.values().cloned().collect();
    changes.figures = book.figures.values().cloned().collect();
    changes.attributes = book.attributes.clone();
    Ok(changes)
}

/// The scope as a parameter: `None` for everything, else the accounts.
fn scope_param(scope: &Scope) -> Option<Vec<String>> {
    match scope {
        Scope::Everything => None,
        Scope::Within(accounts) => Some(accounts.iter().cloned().collect()),
    }
}

/// A watermark as two arrays, for `unnest`.
fn mark_params(mark: &Mark) -> (Vec<String>, Vec<i64>) {
    (
        mark.keys().cloned().collect(),
        mark.values().map(|sequence| *sequence as i64).collect(),
    )
}

const ABOVE: &str =
    "sequence > COALESCE((SELECT w.s FROM unnest($3::text[], $4::bigint[]) AS w(p, s)
                                         WHERE w.p = partition), 0)";

/// Without a watermark the arrays are still named, or Postgres cannot type
/// the parameters.
const UNUSED_MARK: &str = "$3::text[] IS NOT NULL AND $4::bigint[] IS NOT NULL";
const UNUSED_MARK_AND_NOT_REMOVED: &str =
    "$3::text[] IS NOT NULL AND $4::bigint[] IS NOT NULL AND NOT removed";

fn decoded<M: Message + Default>(rows: Vec<Row>, column: usize) -> Result<Vec<M>> {
    rows.into_iter()
        .map(|row| {
            let bytes: Vec<u8> = row.get(column);
            M::decode(bytes.as_slice()).map_err(unreadable)
        })
        .collect()
}

impl Store for PostgresStore {
    fn act(
        &self,
        account_id: &str,
        message_id: &str,
        idempotency_key: &str,
        request: &[u8],
        decide: &mut Decide<'_>,
    ) -> Result<Acted> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        let partition: String = tx
            .query_opt(
                "SELECT partition FROM book_account WHERE account_id = $1",
                &[&account_id],
            )
            .map_err(unavailable)?
            .map(|row| row.get(0))
            .unwrap_or_else(|| FIRST_PARTITION.to_string());
        tx.execute(
            "SELECT pg_advisory_xact_lock($1)",
            &[&partition_lock(&partition)],
        )
        .map_err(unavailable)?;
        let head: i64 = tx
            .query_one(
                "SELECT sequence FROM book_partition_head WHERE partition = $1 FOR UPDATE",
                &[&partition],
            )
            .map_err(unavailable)?
            .get(0);

        // A duplicate is answered with the first's reply, nothing applied:
        // the same message, or the same command under the same key. Another
        // command under a key the account holds is refused (Q12).
        if !message_id.is_empty() {
            if let Some(row) = tx
                .query_opt(
                    "SELECT reply FROM book_entry WHERE message_id = $1",
                    &[&message_id],
                )
                .map_err(unavailable)?
            {
                return Ok(Acted::Duplicate(row.get(0)));
            }
        }
        if !idempotency_key.is_empty() {
            if let Some(row) = tx
                .query_opt(
                    "SELECT reply, request FROM book_entry
                      WHERE account_id = $1 AND idempotency_key = $2",
                    &[&account_id, &idempotency_key],
                )
                .map_err(unavailable)?
            {
                let held: Vec<u8> = row.get(1);
                if held != request {
                    return Err(StoreError::conflict(idempotency_key, account_id));
                }
                return Ok(Acted::Duplicate(row.get(0)));
            }
        }

        let entries = journal_of(&mut tx, account_id)?;
        let book = Book::replay(account_id, &partition, &entries)?;
        let decided = decide(&book, head.max(0) as u64)?;
        let entry = &decided.entry;

        tx.execute(
            "INSERT INTO book_account (account_id, partition) VALUES ($1, $2)
             ON CONFLICT (account_id) DO NOTHING",
            &[&account_id, &partition],
        )
        .map_err(unavailable)?;
        tx.execute(
            "INSERT INTO book_entry (entry_id, account_id, partition, first_sequence, last_sequence,
                                     kind, effective_date, message_id, idempotency_key,
                                     committed_at_ns, meta, cause, body, reply, actor, request)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
            &[
                &entry.entry_id,
                &entry.account_id,
                &entry.partition,
                &(entry.first_sequence as i64),
                &(entry.last_sequence as i64),
                &entry.meta.kind,
                &entry.meta.effective_date,
                &entry.message_id,
                &entry.idempotency_key,
                &entry.cause.committed_at_ns,
                &entry.meta.encode_to_vec(),
                &entry.cause.encode_to_vec(),
                &entry.body.to_bytes(),
                &decided.reply,
                &actor_named(entry.meta.actor.as_ref()),
                &request,
            ],
        )
        .map_err(unavailable)?;
        tx.execute(
            "UPDATE book_partition_head SET sequence = $1 WHERE partition = $2",
            &[&(entry.last_sequence as i64), &entry.partition],
        )
        .map_err(unavailable)?;
        project(&mut tx, account_id, &decided.changes)?;
        tx.commit().map_err(unavailable)?;
        Ok(Acted::Committed(Box::new(decided)))
    }

    fn journal(&self, account_id: &str) -> Result<Vec<Entry>> {
        journal_of(&mut *self.conn()?, account_id)
    }

    fn accounts(&self) -> Result<Vec<String>> {
        Ok(self
            .conn()?
            .query(
                "SELECT account_id FROM book_account ORDER BY account_id COLLATE \"C\"",
                &[],
            )
            .map_err(unavailable)?
            .into_iter()
            .map(|row| row.get(0))
            .collect())
    }

    fn heads(&self) -> Result<Mark> {
        heads_of(&mut *self.conn()?)
    }

    fn accounts_holding(&self, instrument_id: &str) -> Result<Vec<String>> {
        Ok(self
            .conn()?
            .query(
                "SELECT account_id FROM book_position WHERE instrument_id = $1 AND NOT removed
                 UNION
                 SELECT account_id FROM book_break WHERE instrument_id = $1 AND state = $2
                 ORDER BY 1",
                &[&instrument_id, &(BreakState::Open as i32)],
            )
            .map_err(unavailable)?
            .into_iter()
            .map(|row| row.get(0))
            .collect())
    }

    fn instruments_held(&self) -> Result<Vec<String>> {
        Ok(self
            .conn()?
            .query(
                "SELECT instrument_id FROM book_position WHERE NOT removed
                 UNION
                 SELECT instrument_id FROM book_break WHERE instrument_id <> '' AND state = $1
                 ORDER BY 1",
                &[&(BreakState::Open as i32)],
            )
            .map_err(unavailable)?
            .into_iter()
            .map(|row| row.get(0))
            .collect())
    }

    fn positions(&self, read: &PositionsRead) -> Result<Page<BookPosition>> {
        read.scope.admit(&read.account_id)?;
        if !read.business_date.is_empty() || read.at.is_some() {
            return reads::positions_as_of(self, read);
        }
        let after = if read.cursor.is_empty() {
            (String::new(), String::new(), -1)
        } else {
            reads::from_position_cursor(&read.cursor)?
        };
        let (partitions, sequences) = mark_params(&read.since.clone().unwrap_or_default());
        let filter = if read.since.is_some() {
            ABOVE
        } else {
            UNUSED_MARK_AND_NOT_REMOVED
        };
        self.reading(|tx| {
            let rows = tx
                .query(
                    &format!(
                        "SELECT record FROM book_position
                          WHERE ($1::text[] IS NULL OR account_id = ANY($1))
                            AND ($2 = '' OR account_id = $2)
                            AND {filter}
                            AND (account_id COLLATE \"C\", instrument_id COLLATE \"C\", side)
                                > ($5 COLLATE \"C\", $6 COLLATE \"C\", $7)
                          ORDER BY account_id COLLATE \"C\", instrument_id COLLATE \"C\", side
                          LIMIT $8"
                    ),
                    &[
                        &scope_param(&read.scope),
                        &read.account_id,
                        &partitions,
                        &sequences,
                        &after.0,
                        &after.1,
                        &after.2,
                        &(read.limit as i64 + 1),
                    ],
                )
                .map_err(unavailable)?;
            let records = decoded::<BookPosition>(rows, 0)?;
            Ok(reads::paged(
                records,
                read.limit,
                reads::position_cursor,
                heads_of(tx)?,
            ))
        })
    }

    fn breaks(&self, read: &BreaksRead) -> Result<Page<Break>> {
        read.scope.admit(&read.account_id)?;
        let after = if read.cursor.is_empty() {
            (String::new(), String::new())
        } else {
            let held = reads::parts(&read.cursor, 2)?;
            (held[0].clone(), held[1].clone())
        };
        let (partitions, sequences) = mark_params(&read.since.clone().unwrap_or_default());
        let filter = if read.since.is_some() {
            ABOVE
        } else {
            UNUSED_MARK
        };
        let states: Vec<i32> = read.states.iter().map(|state| *state as i32).collect();
        self.reading(|tx| {
            let rows = tx
                .query(
                    &format!(
                        "SELECT record FROM book_break
                          WHERE ($1::text[] IS NULL OR account_id = ANY($1))
                            AND ($2 = '' OR account_id = $2)
                            AND {filter}
                            AND (cardinality($5::integer[]) = 0 OR state = ANY($5))
                            AND (account_id COLLATE \"C\", break_id COLLATE \"C\")
                                > ($6 COLLATE \"C\", $7 COLLATE \"C\")
                          ORDER BY account_id COLLATE \"C\", break_id COLLATE \"C\"
                          LIMIT $8"
                    ),
                    &[
                        &scope_param(&read.scope),
                        &read.account_id,
                        &partitions,
                        &sequences,
                        &states,
                        &after.0,
                        &after.1,
                        &(read.limit as i64 + 1),
                    ],
                )
                .map_err(unavailable)?;
            let records = decoded::<Break>(rows, 0)?;
            Ok(reads::paged(
                records,
                read.limit,
                reads::break_cursor,
                heads_of(tx)?,
            ))
        })
    }

    fn figures(&self, read: &FiguresRead) -> Result<Page<AccountFigures>> {
        read.scope.admit(&read.account_id)?;
        if read.at.is_some() {
            return reads::figures_as_of(self, read);
        }
        let after = if read.cursor.is_empty() {
            (String::new(), String::new(), String::new())
        } else {
            let held = reads::parts(&read.cursor, 3)?;
            (held[0].clone(), held[1].clone(), held[2].clone())
        };
        let (partitions, sequences) = mark_params(&read.since.clone().unwrap_or_default());
        let filter = if read.since.is_some() {
            ABOVE
        } else {
            UNUSED_MARK
        };
        let agreement = agreement_key(read.agreement.as_ref());
        self.reading(|tx| {
            let rows = tx
                .query(
                    &format!(
                        "SELECT record FROM book_figures
                          WHERE ($1::text[] IS NULL OR account_id = ANY($1))
                            AND ($2 = '' OR account_id = $2)
                            AND {filter}
                            AND ($5 = '' OR agreement_key = $5)
                            AND ($6 = '' OR business_date >= $6)
                            AND ($7 = '' OR business_date <= $7)
                            AND (account_id COLLATE \"C\", agreement_key COLLATE \"C\",
                                 business_date COLLATE \"C\")
                                > ($8 COLLATE \"C\", $9 COLLATE \"C\", $10 COLLATE \"C\")
                          ORDER BY account_id COLLATE \"C\", agreement_key COLLATE \"C\",
                                   business_date COLLATE \"C\"
                          LIMIT $11"
                    ),
                    &[
                        &scope_param(&read.scope),
                        &read.account_id,
                        &partitions,
                        &sequences,
                        &agreement,
                        &read.from_date,
                        &read.to_date,
                        &after.0,
                        &after.1,
                        &after.2,
                        &(read.limit as i64 + 1),
                    ],
                )
                .map_err(unavailable)?;
            let records = decoded::<AccountFigures>(rows, 0)?;
            Ok(reads::paged(
                records,
                read.limit,
                reads::figures_cursor,
                heads_of(tx)?,
            ))
        })
    }

    fn attributes(&self, read: &AttributesRead) -> Result<Page<AccountAttributes>> {
        read.scope.admit(&read.account_id)?;
        let after = if read.cursor.is_empty() {
            String::new()
        } else {
            reads::parts(&read.cursor, 1)?.remove(0)
        };
        let (partitions, sequences) = mark_params(&read.since.clone().unwrap_or_default());
        let filter = if read.since.is_some() {
            ABOVE
        } else {
            UNUSED_MARK
        };
        self.reading(|tx| {
            let rows = tx
                .query(
                    &format!(
                        "SELECT record FROM book_attributes
                          WHERE ($1::text[] IS NULL OR account_id = ANY($1))
                            AND ($2 = '' OR account_id = $2)
                            AND {filter}
                            AND account_id COLLATE \"C\" > $5 COLLATE \"C\"
                          ORDER BY account_id COLLATE \"C\"
                          LIMIT $6"
                    ),
                    &[
                        &scope_param(&read.scope),
                        &read.account_id,
                        &partitions,
                        &sequences,
                        &after,
                        &(read.limit as i64 + 1),
                    ],
                )
                .map_err(unavailable)?;
            let records = decoded::<AccountAttributes>(rows, 0)?;
            Ok(reads::paged(
                records,
                read.limit,
                reads::attributes_cursor,
                heads_of(tx)?,
            ))
        })
    }

    fn rebuild(&self) -> Result<usize> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        // Every partition's writer held off for the length of it.
        let partitions: Vec<String> = tx
            .query(
                "SELECT partition FROM book_partition_head ORDER BY partition",
                &[],
            )
            .map_err(unavailable)?
            .into_iter()
            .map(|row| row.get(0))
            .collect();
        for partition in &partitions {
            tx.execute(
                "SELECT pg_advisory_xact_lock($1)",
                &[&partition_lock(partition)],
            )
            .map_err(unavailable)?;
        }
        tx.execute("SELECT 1 FROM book_partition_head FOR UPDATE", &[])
            .map_err(unavailable)?;
        tx.batch_execute(
            "DELETE FROM book_position; DELETE FROM book_lot; DELETE FROM book_pending;
             DELETE FROM book_break; DELETE FROM book_figures; DELETE FROM book_attributes;",
        )
        .map_err(unavailable)?;
        let accounts: Vec<(String, String)> = tx
            .query("SELECT account_id, partition FROM book_account", &[])
            .map_err(unavailable)?
            .into_iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();
        let mut replayed = 0;
        for (account, partition) in accounts {
            let entries = journal_of(&mut tx, &account)?;
            replayed += entries.len();
            let book = Book::replay(&account, &partition, &entries)?;
            project(&mut tx, &account, &all_changes(&book)?)?;
        }
        // The heads stand where the journal ends: a head behind its journal
        // would number a change twice.
        let ends: Vec<(String, i64, i64)> = tx
            .query(
                "SELECT h.partition, h.sequence, COALESCE(max(e.last_sequence), 0)
                   FROM book_partition_head h LEFT JOIN book_entry e USING (partition)
                  GROUP BY h.partition, h.sequence",
                &[],
            )
            .map_err(unavailable)?
            .into_iter()
            .map(|row| (row.get(0), row.get(1), row.get(2)))
            .collect();
        for (partition, head, end) in ends {
            if head != end {
                return Err(StoreError::Unavailable(format!(
                    "partition {partition}'s head is {head} and its journal ends at {end}"
                )));
            }
        }
        tx.commit().map_err(unavailable)?;
        Ok(replayed)
    }

    fn cash_instruments(&self) -> Result<Vec<meridian_domain::money::Resolution>> {
        Ok(self
            .conn()?
            .query(
                "SELECT currency_code, instrument_id, resolved_at_ns, backfilled
                   FROM book_cash_instrument ORDER BY currency_code",
                &[],
            )
            .map_err(unavailable)?
            .iter()
            .map(|row| meridian_domain::money::Resolution {
                code: row.get(0),
                instrument_id: row.get(1),
                resolved_at_ns: row.get(2),
                backfilled: row.get(3),
            })
            .collect())
    }

    fn keep_cash_instrument(&self, resolution: &meridian_domain::money::Resolution) -> Result<()> {
        self.conn()?
            .execute(
                "INSERT INTO book_cash_instrument
                        (currency_code, instrument_id, resolved_at_ns, backfilled)
                 VALUES ($1, $2, $3, $4) ON CONFLICT (currency_code) DO NOTHING",
                &[
                    &resolution.code,
                    &resolution.instrument_id,
                    &resolution.resolved_at_ns,
                    &resolution.backfilled,
                ],
            )
            .map_err(unavailable)?;
        Ok(())
    }

    fn currency_codes(&self) -> Result<Vec<String>> {
        use meridian_domain::money::{codes, is_iso4217};
        let mut conn = self.conn()?;
        let mut found = std::collections::BTreeSet::new();
        for row in conn
            .query(
                "SELECT cost_currency FROM book_lot WHERE cost_currency IS NOT NULL
                 UNION SELECT base_currency_code FROM book_attributes",
                &[],
            )
            .map_err(unavailable)?
        {
            let code: String = row.get(0);
            if is_iso4217(&code) {
                found.insert(code);
            }
        }
        for row in conn
            .query("SELECT record FROM book_figures", &[])
            .map_err(unavailable)?
        {
            let bytes: Vec<u8> = row.get(0);
            found.extend(codes(
                &AccountFigures::decode(bytes.as_slice()).map_err(unreadable)?,
            ));
        }
        for row in conn
            .query("SELECT record FROM book_break", &[])
            .map_err(unavailable)?
        {
            let bytes: Vec<u8> = row.get(0);
            found.extend(codes(&Break::decode(bytes.as_slice()).map_err(unreadable)?));
        }
        Ok(found.into_iter().collect())
    }
}

fn apply_migrations(conn: &mut Connection, clock: &dyn meridian_clock::Clock) -> Result<()> {
    conn.batch_execute(migrations::HISTORY)
        .map_err(unavailable)?;
    let applied: Option<i64> = conn
        .query_one("SELECT max(version) FROM book_schema_migration", &[])
        .map_err(unavailable)?
        .get(0);
    for migration in migrations::MIGRATIONS {
        if applied.is_some_and(|at| at >= migration.version) {
            continue;
        }
        let mut tx = conn.transaction().map_err(unavailable)?;
        tx.batch_execute(migration.sql).map_err(unavailable)?;
        migrations::record(&mut tx, migration, clock.now_ns())?;
        tx.commit().map_err(unavailable)?;
    }
    Ok(())
}

fn unreadable(failed: prost::DecodeError) -> StoreError {
    StoreError::Unavailable(format!("a record of the book did not read: {failed}"))
}

fn unavailable(failed: impl std::error::Error) -> StoreError {
    // With the cause: this driver's own message for most failures is "db
    // error", and everything useful is one level down. A refusal the trigger
    // raised is said as it is.
    let mut detail = failed.to_string();
    let mut cause = failed.source();
    while let Some(next) = cause {
        detail.push_str(": ");
        detail.push_str(&next.to_string());
        cause = next.source();
    }
    StoreError::Unavailable(detail)
}
