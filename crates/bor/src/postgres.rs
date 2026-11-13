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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use postgres::{GenericClient, IsolationLevel, Row, Transaction};
use prost::Message;
use r2d2_postgres::PostgresConnectionManager;

use meridian_domain::v1::{
    AccountAttributes, AccountFigures, BookPosition, Break, BreakState, ChangeCause, EntryMeta,
};

use crate::book::{agreement_key, Book, Changes};
use crate::group_commit::{Command, Decider, Lane, Pending};
use crate::journal::{Body, Entry};
use crate::migrations;
use crate::order_reads;
use crate::reads;
use crate::store::{
    first_partition_of, Acted, AttributesRead, BreaksRead, Committed, Decide, Decided, FiguresRead,
    HistoryRead, InstructionsRead, LocatesRead, Mark, OrdersPage, OrdersRead, Page, PositionsRead,
    Record, Result, Scope, Store, StoreError, CONTROL_PARTITION, FIRST_PARTITION,
};

thread_local! {
    /// Whether a read on this thread is to say the heads of its own snapshot
    /// (contract v30, stage C2; the resilience spec's section 5): a record
    /// read on a read replica, stamped from the snapshot it answered from.
    static STAMPING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The heads its snapshots read, the lowest of each where it took more
    /// than one.
    static SNAPSHOT_HEADS: std::cell::RefCell<Option<Mark>> = const { std::cell::RefCell::new(None) };
}

/// The next reads on this thread each answer from one repeatable-read
/// snapshot and say its partitions' head rows ([`snapshot_heads`]): a read
/// replica's record read (contract v30, stage C2; the resilience spec's
/// section 5). Its watermark is then exact, with no write position mapped:
/// the head rows replicate with the entries, as the lake's do.
pub fn stamp_from_snapshot() {
    STAMPING.with(|stamping| stamping.set(true));
    SNAPSHOT_HEADS.with(|heads| heads.borrow_mut().take());
}

/// The heads the reads since [`stamp_from_snapshot`] answered from, each the
/// lowest any of them read; none where none read any. Stops the stamping.
pub fn snapshot_heads() -> Option<Mark> {
    STAMPING.with(|stamping| stamping.set(false));
    SNAPSHOT_HEADS.with(|heads| heads.borrow_mut().take())
}

fn stamping() -> bool {
    STAMPING.with(std::cell::Cell::get)
}

/// In a read's repeatable-read transaction: every partition's head row from
/// the same snapshot the read answers from, kept for [`snapshot_heads`].
fn note_snapshot(tx: &mut Transaction<'_>) -> Result<()> {
    let heads = heads_of(tx)?;
    SNAPSHOT_HEADS.with(|noted| {
        let mut noted = noted.borrow_mut();
        let noted = noted.get_or_insert_with(Mark::new);
        for (partition, head) in heads {
            let at = noted.entry(partition).or_insert(head);
            *at = (*at).min(head);
        }
    });
    Ok(())
}

type Pool = r2d2::Pool<PostgresConnectionManager<meridian_changes::tls::DatabaseTls>>;
type Connection =
    r2d2::PooledConnection<PostgresConnectionManager<meridian_changes::tls::DatabaseTls>>;

/// Names the schema lock: distinct from every other store's.
const SCHEMA_LOCK: i64 = 0x626f_6f6b_0000_0001_u64 as i64;

/// The partition locks' space: a partition's lock is this with the
/// partition's hash in its low half.
const PARTITION_LOCKS: i64 = 0x626f_6f6b_0000_0000_u64 as i64;

pub struct PostgresStore {
    pool: Pool,
    /// How many entries an account's journal grows by between snapshots
    /// (contract v25, W9.10; the chart's `bor.snapshotInterval`).
    snapshot_interval: u64,
    /// The epoch this writer's commands commit under (step 3a).
    fence: meridian_changes::Fence,
    /// Its database's identity as its connections found it (contract v30,
    /// stage C): a new timeline raises the epoch before a command there.
    watch: std::sync::Arc<meridian_changes::timeline::Watch>,
    /// Each partition's lane: the commands waiting for its transaction in
    /// flight (step 3d, group commit).
    lanes: Mutex<HashMap<String, Arc<Lane>>>,
    /// The partition each account was last found in.
    placed: Mutex<HashMap<String, String>>,
    /// Told of each batch's commits, in their order, once it commits.
    committed: Mutex<Option<Committed>>,
    /// Its writer group and the book's count (step 3d; section 6.3): the
    /// partitions it may commit to, and those it raises the epoch of.
    groups: meridian_changes::groups::Groups,
    /// One adoption at once where it may raise the epoch (several groups).
    adopting: Mutex<()>,
    /// Its count of partitions (step 3d; `bor.partitions`): where a new
    /// account is placed.
    partitions: u32,
}

impl PostgresStore {
    pub fn connect(url: &str, pool_size: u32) -> Result<Self> {
        let config = meridian_changes::database_config(url).map_err(unavailable)?;
        let manager = PostgresConnectionManager::new(
            config,
            meridian_changes::tls::database_tls().map_err(unavailable)?,
        );
        let watch = std::sync::Arc::new(meridian_changes::timeline::Watch::default());
        let pool = r2d2::Pool::builder()
            .max_size(pool_size.max(1))
            .connection_customizer(Box::new(meridian_changes::timeline::Watching(
                std::sync::Arc::clone(&watch),
            )))
            .build(manager)
            .map_err(unavailable)?;
        Ok(Self {
            pool,
            snapshot_interval: crate::snapshot::DEFAULT_INTERVAL,
            fence: meridian_changes::Fence::new(),
            watch,
            lanes: Mutex::default(),
            placed: Mutex::default(),
            committed: Mutex::default(),
            groups: meridian_changes::groups::Groups::one(),
            adopting: Mutex::new(()),
            partitions: 1,
        })
    }

    /// The same store, placing new accounts over `count` partitions (step
    /// 3d; section 6.3).
    pub fn with_partitions(mut self, count: u32) -> Self {
        self.partitions = count.max(1);
        self
    }

    /// Each of `P0` to `P<count-1>`'s head row made where it is not, at the
    /// control partition's epoch, as `migrate` makes them for
    /// `bor.partitions` (step 3d; section 6.3). Lowering the count removes
    /// none: the CLI's upgrade preflight refuses it.
    pub fn make_partitions(&self, count: u32) -> Result<usize> {
        let mut conn = self.conn()?;
        let names: Vec<String> = (0..count.max(1))
            .map(crate::store::partition_named)
            .collect();
        let made = conn
            .execute(
                "INSERT INTO book_partition_head (partition, sequence, epoch)
                 SELECT p, 0, (SELECT epoch FROM book_partition_head WHERE partition = 'control')
                   FROM unnest($1::text[]) AS p
                 ON CONFLICT (partition) DO NOTHING",
                &[&names],
            )
            .map_err(unavailable)?;
        Ok(made as usize)
    }

    /// The same store, writing for writer group `groups` (step 3d; section
    /// 6.3): its acts commit only to the partitions its group owns, and its
    /// takeover raises only theirs.
    pub fn with_groups(mut self, groups: meridian_changes::groups::Groups) -> Self {
        self.groups = groups;
        self
    }

    /// Its writer group and the book's count.
    pub fn groups(&self) -> meridian_changes::groups::Groups {
        self.groups
    }

    /// The same store, keeping a snapshot every `interval` entries (W9.10).
    pub fn with_snapshot_interval(mut self, interval: u64) -> Self {
        self.snapshot_interval = interval.max(1);
        self
    }

    /// Apply every migration not yet recorded, under a lock. Run once per
    /// release by `meridian-bor migrate`, never by a starting process.
    pub fn migrate(&self, clock: &dyn meridian_clock::Clock) -> Result<()> {
        let mut conn = self.conn()?;
        conn.execute("SELECT pg_advisory_lock($1)", &[&SCHEMA_LOCK])
            .map_err(unavailable)?;
        let outcome = apply_migrations(&mut conn, clock).and_then(|()| chain(&mut conn));
        let _ = conn.execute("SELECT pg_advisory_unlock($1)", &[&SCHEMA_LOCK]);
        outcome
    }

    /// Re-read the record (step 3b, integrity at rest): [`scrub_chain`].
    pub fn scrub(&self) -> Result<meridian_changes::integrity::Scrubbed> {
        scrub_chain(&mut self.conn()?)
    }

    /// Keep a scrub's findings and its run (`IntegrityFinding`).
    pub fn record_scrub(
        &self,
        scrubbed: &meridian_changes::integrity::Scrubbed,
        member: &str,
        started_at_ns: i64,
        finished_at_ns: i64,
    ) -> Result<()> {
        let mut conn = self.conn()?;
        meridian_changes::integrity::record(
            &mut *conn,
            "book",
            scrubbed,
            member,
            started_at_ns,
            finished_at_ns,
            // The book's control partition is its journal: its findings
            // stay in book_integrity_finding alone for now (stage D).
            meridian_changes::integrity::unlogged,
        )
        .map_err(unavailable)
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
        let mut conn = self.pool.get().map_err(unavailable)?;
        // A connection opened on a new database timeline: the epoch raised
        // before this one commits anything there (contract v30, stage C).
        if self.watch.moved() {
            if let Err(failed) = self.follow_on(&mut conn) {
                self.watch.unsettled();
                return Err(failed);
            }
        }
        Ok(conn)
    }

    /// Follow the database's timeline on `conn` for this writer (contract
    /// v30, stage C): on a new one every partition's epoch raised as a
    /// takeover raises it, adopted, and the takeover recorded with that
    /// cause. Nothing where it writes under no epoch.
    fn follow_on(&self, conn: &mut Connection) -> Result<meridian_changes::timeline::Timeline> {
        use meridian_changes::timeline::Timeline;
        let _alone = self.watch.alone();
        let mine = self.fence.epoch();
        if mine == 0 {
            return Ok(Timeline::Same);
        }
        let by = meridian_changes::TookOver {
            member: meridian_changes::member().to_string(),
            previous: String::new(),
            why: meridian_changes::timeline::DATABASE_TIMELINE.to_string(),
            at_ns: meridian_changes::now_ns(),
        };
        let mut tx = conn.transaction().map_err(unavailable)?;
        let anchor = anchor_of(&mut tx, self.groups)?;
        let Some(anchor) = anchor else {
            return Ok(Timeline::Same);
        };
        let groups = self.groups;
        let (found, identity) = meridian_changes::timeline::follow(
            &mut tx,
            "book_partition_head",
            &anchor,
            mine,
            |tx| raise(tx, &by, groups),
        )
        .map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        if let Timeline::Moved(epoch) = found {
            self.fence.adopt(epoch);
            tracing::warn!(
                epoch,
                identity,
                "the book's database is on a new timeline: the epoch is raised, and followers past it reload"
            );
        }
        self.watch.settled(&identity);
        Ok(found)
    }

    /// The start-up's snapshots (v26's core; W9.10's note "when a snapshot
    /// is taken", decided 2026-10-18): every account -- `only` that one --
    /// whose journal since its latest snapshot of this format reaches the
    /// interval, an upgraded account's included, its book kept as at its
    /// latest entry, each in its own transaction under its partition's lock.
    /// Says how many were kept.
    pub fn backfill_snapshots(&self, only: Option<&str>) -> Result<usize> {
        // Every account's, at start: without the serving session's
        // statement_timeout (section 10a).
        let mut conn = self.conn()?;
        let mut looking = conn.transaction().map_err(unavailable)?;
        meridian_changes::serving::unbounded(&mut looking).map_err(unavailable)?;
        let due: Vec<(String, String)> = looking
            .query(
                "SELECT a.account_id, a.partition FROM book_account a
                   LEFT JOIN LATERAL (
                        SELECT s.last_sequence FROM book_snapshot s
                          JOIN book_entry e
                            ON e.entry_id = s.last_entry_id AND e.first_sequence = s.last_sequence
                           AND e.account_id = s.account_id
                         WHERE s.account_id = a.account_id AND s.format = $1
                         ORDER BY s.last_sequence DESC LIMIT 1) s ON true
                  WHERE ($3::text IS NULL OR a.account_id = $3)
                    AND (SELECT count(*) FROM book_entry e
                          WHERE e.account_id = a.account_id
                            AND e.first_sequence > COALESCE(s.last_sequence, -1)) >= $2
                  ORDER BY a.account_id COLLATE \"C\"",
                &[
                    &crate::snapshot::FORMAT,
                    &(self.snapshot_interval as i64),
                    &only,
                ],
            )
            .map_err(unavailable)?
            .into_iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();
        looking.commit().map_err(unavailable)?;
        drop(conn);
        let mut kept = 0;
        // Each writer group its own partitions' (step 3d).
        for (account, partition) in due.into_iter().filter(|(_, p)| self.groups.owns(p)) {
            let mut conn = self.conn()?;
            let mut tx = conn.transaction().map_err(unavailable)?;
            meridian_changes::serving::unbounded(&mut tx).map_err(unavailable)?;
            tx.execute(
                "SELECT pg_advisory_xact_lock($1)",
                &[&partition_lock(&partition)],
            )
            .map_err(unavailable)?;
            let (book, replayed) = book_from(&mut tx, &account, &partition)?;
            if replayed.entries as u64 >= self.snapshot_interval {
                keep_snapshot(&mut tx, &book, replayed.last_committed_at_ns)?;
                kept += 1;
            }
            tx.commit().map_err(unavailable)?;
        }
        Ok(kept)
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
        // Its heads from this snapshot, where its thread asked (stage C2).
        if stamping() {
            note_snapshot(&mut tx)?;
        }
        let out = read(&mut tx)?;
        tx.commit().map_err(unavailable)?;
        Ok(out)
    }
}

/// Follow every partition's chain (step 3b, integrity at rest): each
/// entry's digest taken again of its canonical form after the digest it
/// names, and the digest it names the one before it in its partition, so a
/// changed entry and a missing one are each named by sequence -- the first
/// bad one in each partition.
pub fn scrub_chain(conn: &mut Connection) -> Result<meridian_changes::integrity::Scrubbed> {
    let mut scrubbed = meridian_changes::integrity::Scrubbed::default();
    let partitions: Vec<String> = conn
        .query(
            "SELECT partition FROM book_partition_head ORDER BY partition",
            &[],
        )
        .map_err(unavailable)?
        .iter()
        .map(|row| row.get(0))
        .collect();
    for partition in partitions {
        let mut after: i64 = 0;
        let mut previous: Option<Vec<u8>> = None;
        'partition: loop {
            let rows = conn
                .query(
                    &format!(
                        "SELECT {ENTRY_COLUMNS}, committed_at_ns FROM book_entry
                          WHERE partition = $1 AND first_sequence > $2
                          ORDER BY first_sequence LIMIT 500"
                    ),
                    &[&partition, &after],
                )
                .map_err(unavailable)?;
            if rows.is_empty() {
                break;
            }
            for row in &rows {
                scrubbed.checked += 1;
                after = row.get::<_, i64>(3);
                let last_sequence = row.get::<_, i64>(4) as u64;
                let at: i64 = row.get(12);
                let (_, entry_previous, entry_digest) = stored_of(row);
                let broken = |what: &str| meridian_changes::integrity::Finding {
                    partition: partition.clone(),
                    sequence: last_sequence,
                    committed_at_ns: at,
                    what: what.to_string(),
                };
                if entry_digest.is_empty() {
                    previous = None;
                    continue;
                }
                if previous.as_ref().is_some_and(|p| *p != entry_previous) {
                    scrubbed.findings.push(broken(
                        "an entry does not follow the one before it: one was changed or removed",
                    ));
                    break 'partition;
                }
                if !stored_verifies(row) {
                    scrubbed.findings.push(broken(
                        "an entry does not match its digest: it changed beneath the book",
                    ));
                    break 'partition;
                }
                previous = Some(entry_digest);
            }
        }
    }
    Ok(scrubbed)
}

/// Chain every entry kept before digests (step 3b, integrity at rest;
/// migration 9): each partition's entries in their order, each given the
/// previous one's digest and its own, and the partition's head its last.
/// Run by `migrate`, under its lock; an entry already chained is left as it
/// is, so it may run again, and a partition chained whole is passed over.
fn chain(conn: &mut Connection) -> Result<()> {
    let partitions: Vec<String> = conn
        .query(
            "SELECT DISTINCT partition FROM book_entry WHERE digest IS NULL",
            &[],
        )
        .map_err(unavailable)?
        .iter()
        .map(|row| row.get(0))
        .collect();
    for partition in partitions {
        let mut tx = conn.transaction().map_err(unavailable)?;
        let rows = tx
            .query(
                &format!(
                    "SELECT {ENTRY_COLUMNS} FROM book_entry WHERE partition = $1
                      ORDER BY first_sequence"
                ),
                &[&partition],
            )
            .map_err(unavailable)?;
        let mut previous: Vec<u8> = Vec::new();
        // Each entry as it is stored, never decoded: an entry the book could
        // not read is chained all the same, and never stops the upgrade.
        for row in &rows {
            let entry_id: String = row.get(0);
            let digest: Option<Vec<u8>> = row.get(11);
            let digest = match digest {
                Some(digest) => digest,
                None => {
                    let canonical = crate::journal::Stored {
                        entry_id: &entry_id,
                        account_id: row.get(1),
                        first_sequence: row.get::<_, i64>(3) as u64,
                        last_sequence: row.get::<_, i64>(4) as u64,
                        message_id: row.get(5),
                        idempotency_key: row.get(6),
                        meta: row.get(7),
                        cause: row.get(8),
                        body: row.get(9),
                        previous_digest: &previous,
                    }
                    .canonical();
                    let digest = meridian_changes::integrity::chained(&previous, &canonical);
                    tx.execute(
                        "UPDATE book_entry SET previous_digest = $2, digest = $3 WHERE entry_id = $1",
                        &[&entry_id, &previous, &digest],
                    )
                    .map_err(unavailable)?;
                    digest
                }
            };
            previous = digest;
        }
        tx.execute(
            "UPDATE book_partition_head SET last_digest = $2 WHERE partition = $1",
            &[&partition, &previous],
        )
        .map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
    }
    Ok(())
}

const ENTRY_COLUMNS: &str = "entry_id, account_id, partition, first_sequence, last_sequence,
        message_id, idempotency_key, meta, cause, body, previous_digest, digest";

/// A row of [`ENTRY_COLUMNS`] as its digest is taken, never decoded:
/// its canonical form, its previous digest and its digest.
fn stored_of(row: &Row) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let previous: Vec<u8> = row.get::<_, Option<Vec<u8>>>(10).unwrap_or_default();
    let digest: Vec<u8> = row.get::<_, Option<Vec<u8>>>(11).unwrap_or_default();
    let canonical = crate::journal::Stored {
        entry_id: row.get(0),
        account_id: row.get(1),
        first_sequence: row.get::<_, i64>(3) as u64,
        last_sequence: row.get::<_, i64>(4) as u64,
        message_id: row.get(5),
        idempotency_key: row.get(6),
        meta: row.get(7),
        cause: row.get(8),
        body: row.get(9),
        previous_digest: &previous,
    }
    .canonical();
    (canonical, previous, digest)
}

/// Whether a row is the entry its digest was taken of; one kept before
/// digests is taken as it is.
fn stored_verifies(row: &Row) -> bool {
    let (canonical, previous, digest) = stored_of(row);
    digest.is_empty() || meridian_changes::integrity::chained(&previous, &canonical) == digest
}

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
        previous_digest: row.get::<_, Option<Vec<u8>>>(10).unwrap_or_default(),
        digest: row.get::<_, Option<Vec<u8>>>(11).unwrap_or_default(),
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
        // Each entry replayed is the one its digest was taken of (step 3b):
        // a record changed beneath the book fails the replay loudly.
        .map(|row| {
            if !stored_verifies(row) {
                let (_, _, digest) = stored_of(row);
                return Err(StoreError::Unavailable(format!(
                    "entry {} of partition {} (account {}) does not match its digest {}: the \
                     record changed beneath the book, and it is not replayed from it",
                    row.get::<_, i64>(4),
                    row.get::<_, String>(2),
                    row.get::<_, String>(1),
                    meridian_changes::integrity::short(&digest)
                )));
            }
            entry_of(row)
        })
        .collect()
}

/// The entries of an account's journal numbered above `after`, in order.
fn journal_after(
    client: &mut impl GenericClient,
    account_id: &str,
    after: u64,
) -> Result<Vec<Entry>> {
    client
        .query(
            &format!(
                "SELECT {ENTRY_COLUMNS} FROM book_entry WHERE account_id = $1 AND first_sequence > $2
                  ORDER BY first_sequence"
            ),
            &[&account_id, &(after as i64)],
        )
        .map_err(unavailable)?
        .iter()
        .map(entry_of)
        .collect()
}

/// An account's latest usable snapshot (contract v25, W9.10): of this
/// book's format, whose latest entry the journal still has there, at or
/// below `at` where a read names a watermark, and holding no entry effective
/// after `until` where it names a date. None where none is: the caller
/// replays from the first entry. A snapshot that does not read is passed
/// over, never trusted.
fn snapshot_of(
    client: &mut impl GenericClient,
    account_id: &str,
    at: Option<u64>,
    until: Option<&str>,
) -> Result<Option<Book>> {
    let rows = client
        .query(
            "SELECT s.snapshot FROM book_snapshot s
               JOIN book_entry e
                 ON e.entry_id = s.last_entry_id AND e.first_sequence = s.last_sequence
                AND e.account_id = s.account_id
              WHERE s.account_id = $1 AND s.format = $2 AND s.last_sequence <= $3
                AND ($4::text IS NULL OR s.latest_effective_date <= $4)
              ORDER BY s.last_sequence DESC",
            &[
                &account_id,
                &crate::snapshot::FORMAT,
                &(at.map(|at| at as i64).unwrap_or(i64::MAX)),
                &until,
            ],
        )
        .map_err(unavailable)?;
    for row in rows {
        let bytes: Vec<u8> = row.get(0);
        match crate::snapshot::decode(&bytes) {
            Ok(book) => return Ok(Some(book)),
            Err(failed) => {
                tracing::warn!(account = account_id, %failed, "a snapshot passed over; replayed")
            }
        }
    }
    Ok(None)
}

/// What a build of an account's book replayed: how many entries, and the
/// deployment's time of the last, which a snapshot made of it is taken at.
#[derive(Debug, Clone, Copy, Default)]
struct Replayed {
    entries: usize,
    last_committed_at_ns: i64,
}

impl Replayed {
    fn of(entries: &[Entry]) -> Replayed {
        Replayed {
            entries: entries.len(),
            last_committed_at_ns: entries
                .last()
                .map(|entry| entry.cause.committed_at_ns)
                .unwrap_or_default(),
        }
    }
}

/// An account's book now: its latest snapshot and the journal's tail, or the
/// whole journal; and what was replayed for it.
fn book_from(
    client: &mut impl GenericClient,
    account_id: &str,
    partition: &str,
) -> Result<(Book, Replayed)> {
    match snapshot_of(client, account_id, None, None)? {
        Some(kept) => {
            let tail = journal_after(client, account_id, crate::snapshot::last_sequence(&kept))?;
            Ok((crate::snapshot::resume(kept, &tail)?, Replayed::of(&tail)))
        }
        None => {
            let entries = journal_of(client, account_id)?;
            let replayed = Replayed::of(&entries);
            Ok((
                Book::replay_lean(account_id, partition, &entries)?,
                replayed,
            ))
        }
    }
}

/// A read's snapshot (v26's core; W9.10's note "when a snapshot is taken",
/// decided 2026-10-18): a book a read built by replaying past the interval
/// is kept as at its latest entry, so a quiet account -- one no command
/// writes to -- is not replayed whole on every read. A failure to keep it is
/// logged and the read answered: a snapshot is derived, never the record.
fn keep_read_snapshot(
    client: &mut impl GenericClient,
    book: &Book,
    replayed: Replayed,
    interval: u64,
) {
    if (replayed.entries as u64) < interval {
        return;
    }
    if let Err(failed) = keep_snapshot(client, book, replayed.last_committed_at_ns) {
        tracing::warn!(account = book.account_id, %failed, "a read's snapshot was not kept");
    }
}

/// The bodies of the named entries of an account's journal (contract v26,
/// W9.10), each with its entry: what a decision asked for that the book
/// does not hold.
fn bodies_of(
    client: &mut impl GenericClient,
    account_id: &str,
    entry_ids: &[String],
) -> Result<Vec<(String, Body)>> {
    client
        .query(
            "SELECT entry_id, body FROM book_entry WHERE account_id = $1 AND entry_id = ANY($2)",
            &[&account_id, &entry_ids],
        )
        .map_err(unavailable)?
        .iter()
        .map(|row| {
            let body: Vec<u8> = row.get(1);
            Ok((row.get(0), Body::from_bytes(&body)?))
        })
        .collect()
}

/// Keep a snapshot of an account's book as at its latest entry, and drop all
/// but the latest few (W9.10). One already kept as at the same entry is made
/// again: of an older format (an upgraded account's, contract v26) or
/// unreadable, it would otherwise stand in the new one's place and be passed
/// over on every read.
fn keep_snapshot(client: &mut impl GenericClient, book: &Book, taken_at_ns: i64) -> Result<()> {
    let Some(last) = book.entries.last() else {
        return Ok(());
    };
    client
        .execute(
            "INSERT INTO book_snapshot (account_id, last_sequence, last_entry_id, entries,
                                        latest_effective_date, format, taken_at_ns, snapshot)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (account_id, last_sequence) DO UPDATE
                SET last_entry_id = EXCLUDED.last_entry_id, entries = EXCLUDED.entries,
                    latest_effective_date = EXCLUDED.latest_effective_date,
                    format = EXCLUDED.format, taken_at_ns = EXCLUDED.taken_at_ns,
                    snapshot = EXCLUDED.snapshot",
            &[
                &book.account_id,
                &(last.first.sequence as i64),
                &last.entry_id,
                &(book.entries.len() as i64),
                &crate::snapshot::latest_effective_date(book),
                &crate::snapshot::FORMAT,
                &taken_at_ns,
                &crate::snapshot::encode(book),
            ],
        )
        .map_err(unavailable)?;
    client
        .execute(
            "DELETE FROM book_snapshot WHERE account_id = $1 AND last_sequence NOT IN (
                 SELECT last_sequence FROM book_snapshot WHERE account_id = $1
                  ORDER BY last_sequence DESC LIMIT $2)",
            &[&book.account_id, &(crate::snapshot::KEPT as i64)],
        )
        .map_err(unavailable)?;
    Ok(())
}

/// The accounts the book holds, in order.
fn accounts_of(client: &mut impl GenericClient) -> Result<Vec<String>> {
    Ok(client
        .query(
            "SELECT account_id FROM book_account WHERE account_id NOT LIKE '~%'
              ORDER BY account_id COLLATE \"C\"",
            &[],
        )
        .map_err(unavailable)?
        .into_iter()
        .map(|row| row.get(0))
        .collect())
}

/// An account's book as of `at` (and `until`), from its latest snapshot
/// holding nothing later, with the entries replayed and the sequence it was
/// held to in its partition.
fn book_as_of_in(
    client: &mut impl GenericClient,
    account_id: &str,
    at: &Mark,
    until: Option<&str>,
) -> Result<(Book, (Vec<Entry>, u64))> {
    let partition: String = client
        .query_opt(
            "SELECT partition FROM book_account WHERE account_id = $1",
            &[&account_id],
        )
        .map_err(unavailable)?
        .map(|row| row.get(0))
        .unwrap_or_else(|| FIRST_PARTITION.to_string());
    let at = at.get(&partition).copied().unwrap_or(0);
    let (book, entries) = match snapshot_of(client, account_id, Some(at), until)? {
        Some(kept) => {
            let after = crate::snapshot::last_sequence(&kept);
            let tail = journal_after(client, account_id, after)?;
            (kept.resume_as_of(&tail, Some(at), until)?, tail)
        }
        None => {
            let entries = journal_of(client, account_id)?;
            let book = Book::as_of(account_id, &partition, &entries, Some(at), until)?;
            (book, entries)
        }
    };
    Ok((book, (entries, at)))
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
        Some(Kind::ServiceAccount(account)) => {
            format!(
                "service account {} owned by {}",
                account.subject, account.owner_subject
            )
        }
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
        // An unrecognized security names neither (contract v24, W9.3).
        None => (String::new(), "unrecognized security".into()),
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
                     record, sleeve)
             VALUES ($1, $2, $3, $4, $5, $6, $7::text::numeric, $8::text::numeric,
                     $9::text::numeric, $10, $11, $12)
             ON CONFLICT (account_id, instrument_id, side, sleeve) DO UPDATE
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
                &record.sleeve,
            ],
        )
        .map_err(unavailable)?;
        tx.execute(
            "DELETE FROM book_lot
              WHERE account_id = $1 AND instrument_id = $2 AND side = $3 AND sleeve = $4",
            &[
                &account_id,
                &record.instrument_id,
                &record.side,
                &record.sleeve,
            ],
        )
        .map_err(unavailable)?;
        tx.execute(
            "DELETE FROM book_pending
              WHERE account_id = $1 AND instrument_id = $2 AND side = $3 AND sleeve = $4",
            &[
                &account_id,
                &record.instrument_id,
                &record.side,
                &record.sleeve,
            ],
        )
        .map_err(unavailable)?;
        for (ordinal, lot) in record.lots.iter().enumerate() {
            let terms = lot.terms.clone().unwrap_or_default();
            let cost = terms.cost.as_ref();
            tx.execute(
                "INSERT INTO book_lot
                        (account_id, instrument_id, side, lot_id, ordinal, open_quantity,
                         original_quantity, cost, cost_currency, acquired_date, source, sleeve)
                 VALUES ($1, $2, $3, $4, $5, $6::text::numeric, $7::text::numeric,
                         $8::text::numeric, $9, $10, $11, $12)",
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
                    &record.sleeve,
                ],
            )
            .map_err(unavailable)?;
        }
        for pending in &record.pending {
            tx.execute(
                "INSERT INTO book_pending
                        (account_id, instrument_id, side, value_date, quantity, failing, sleeve)
                 VALUES ($1, $2, $3, $4, $5::text::numeric, $6, $7)",
                &[
                    &account_id,
                    &record.instrument_id,
                    &record.side,
                    &pending.value_date,
                    &numeric(&pending.quantity).unwrap_or_else(|| "0".into()),
                    &pending.state.as_ref().is_some_and(|state| state.failing),
                    &record.sleeve,
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
    project_orders(tx, account_id, changes)?;
    Ok(())
}

/// The order path's records an entry changed (W11.13, contract v22).
fn project_orders(tx: &mut Transaction<'_>, account_id: &str, changes: &Changes) -> Result<()> {
    for (record, _) in &changes.orders {
        let change = record.last_change.clone().unwrap_or_default();
        let status = record.status.clone().unwrap_or_default();
        let ordered = status
            .quantities
            .as_ref()
            .and_then(|quantities| numeric(&quantities.ordered));
        let expires_at = changes
            .order_expiries
            .get(&record.order_id)
            .copied()
            .flatten();
        tx.execute(
            "INSERT INTO book_order
                    (account_id, order_id, partition, sequence, state, instrument_id, side,
                     sleeve, ordered, parent_order_id, expires_at_ns, record)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::text::numeric, $10, $11, $12)
             ON CONFLICT (account_id, order_id) DO UPDATE
                SET partition = EXCLUDED.partition, sequence = EXCLUDED.sequence,
                    state = EXCLUDED.state, ordered = EXCLUDED.ordered,
                    expires_at_ns = EXCLUDED.expires_at_ns, record = EXCLUDED.record",
            &[
                &account_id,
                &record.order_id,
                &change.partition,
                &(change.sequence as i64),
                &status.state,
                &record.instrument_id,
                &record.side,
                &record.sleeve,
                &ordered,
                &record
                    .link
                    .as_ref()
                    .map(|link| link.parent_order_id.clone())
                    .unwrap_or_default(),
                &expires_at,
                &record.encode_to_vec(),
            ],
        )
        .map_err(unavailable)?;
    }
    for (record, _) in &changes.placements {
        let change = record.last_change.clone().unwrap_or_default();
        tx.execute(
            "INSERT INTO book_placement
                    (account_id, placement_id, order_id, connectivity_instance_id, partition,
                     sequence, state, quantity, record)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8::text::numeric, $9)
             ON CONFLICT (account_id, placement_id) DO UPDATE
                SET partition = EXCLUDED.partition, sequence = EXCLUDED.sequence,
                    state = EXCLUDED.state, quantity = EXCLUDED.quantity,
                    record = EXCLUDED.record",
            &[
                &account_id,
                &record.placement_id,
                &record.order_id,
                &record.connectivity_instance_id,
                &change.partition,
                &(change.sequence as i64),
                &record
                    .status
                    .as_ref()
                    .map(|status| status.state)
                    .unwrap_or_default(),
                &numeric(&record.quantity),
                &record.encode_to_vec(),
            ],
        )
        .map_err(unavailable)?;
    }
    for record in &changes.executions {
        let change = record.last_change.clone().unwrap_or_default();
        tx.execute(
            "INSERT INTO book_execution
                    (account_id, execution_id, order_id, placement_id, partition, sequence,
                     quantity, price, trade_date, supersedes, record, state)
             VALUES ($1, $2, $3, $4, $5, $6, $7::text::numeric, $8::text::numeric, $9, $10, $11,
                     $12)
             ON CONFLICT (account_id, execution_id) DO UPDATE
                SET partition = EXCLUDED.partition, sequence = EXCLUDED.sequence,
                    order_id = EXCLUDED.order_id, placement_id = EXCLUDED.placement_id,
                    state = EXCLUDED.state, record = EXCLUDED.record",
            &[
                &account_id,
                &record.execution_id,
                &record.order_id,
                &record.placement_id,
                &change.partition,
                &(change.sequence as i64),
                &numeric(&record.quantity),
                &record
                    .price
                    .as_ref()
                    .and_then(|price| numeric(&price.amount)),
                &record.trade_date,
                &record.supersedes,
                &record.encode_to_vec(),
                &record.state,
            ],
        )
        .map_err(unavailable)?;
    }
    for record in &changes.unplaced {
        let change = record.last_change.clone().unwrap_or_default();
        tx.execute(
            "INSERT INTO book_unplaced
                    (account_id, unplaced_id, partition, sequence, fact_kind, reason,
                     instance_id, placed, record)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (account_id, unplaced_id) DO UPDATE
                SET partition = EXCLUDED.partition, sequence = EXCLUDED.sequence,
                    placed = EXCLUDED.placed, record = EXCLUDED.record",
            &[
                &account_id,
                &record.unplaced_id,
                &change.partition,
                &(change.sequence as i64),
                &record.fact_kind,
                &record.reason,
                &record
                    .raw_record
                    .as_ref()
                    .map(|raw| raw.instance_id.clone())
                    .unwrap_or_default(),
                &record.placed_by.is_some(),
                &record.encode_to_vec(),
            ],
        )
        .map_err(unavailable)?;
    }
    for record in &changes.locates {
        let change = record.last_change.clone().unwrap_or_default();
        tx.execute(
            "INSERT INTO book_locate
                    (account_id, locate_record_id, partition, sequence, locate_id, record)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (account_id, locate_record_id) DO UPDATE
                SET partition = EXCLUDED.partition, sequence = EXCLUDED.sequence,
                    locate_id = EXCLUDED.locate_id, record = EXCLUDED.record",
            &[
                &account_id,
                &record.locate_record_id,
                &change.partition,
                &(change.sequence as i64),
                &record.locate_id,
                &record.encode_to_vec(),
            ],
        )
        .map_err(unavailable)?;
    }
    for record in &changes.instructions {
        let change = record.last_change.clone().unwrap_or_default();
        let target = record.target.clone().unwrap_or_default();
        let level = meridian_domain::v1::SetterLevel::try_from(record.setter_level)
            .unwrap_or(meridian_domain::v1::SetterLevel::Unspecified);
        tx.execute(
            "INSERT INTO book_instruction
                    (instruction_key, account_id, partition, sequence, value, setter_level, record)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (instruction_key) DO UPDATE
                SET partition = EXCLUDED.partition, sequence = EXCLUDED.sequence,
                    value = EXCLUDED.value, record = EXCLUDED.record",
            &[
                &crate::order_decide::instruction_key(&target, level),
                &account_id,
                &change.partition,
                &(change.sequence as i64),
                &record.value,
                &record.setter_level,
                &record.encode_to_vec(),
            ],
        )
        .map_err(unavailable)?;
    }
    // Contract v24: bookings, alerts and a block's members (W11.1, W11.30,
    // W11.35).
    for record in &changes.bookings {
        let change = record.last_change.clone().unwrap_or_default();
        tx.execute(
            "INSERT INTO book_booking
                    (account_id, booking_id, partition, sequence, state, order_id, record)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (account_id, booking_id) DO UPDATE
                SET partition = EXCLUDED.partition, sequence = EXCLUDED.sequence,
                    state = EXCLUDED.state, record = EXCLUDED.record",
            &[
                &account_id,
                &record.booking_id,
                &change.partition,
                &(change.sequence as i64),
                &record.state,
                &record.order_id,
                &record.encode_to_vec(),
            ],
        )
        .map_err(unavailable)?;
    }
    for record in &changes.alerts {
        let change = record.last_change.clone().unwrap_or_default();
        tx.execute(
            "INSERT INTO book_alert
                    (account_id, alert_id, partition, sequence, state, kind, record)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (account_id, alert_id) DO UPDATE
                SET partition = EXCLUDED.partition, sequence = EXCLUDED.sequence,
                    state = EXCLUDED.state, record = EXCLUDED.record",
            &[
                &account_id,
                &record.alert_id,
                &change.partition,
                &(change.sequence as i64),
                &record.state,
                &record.kind,
                &record.encode_to_vec(),
            ],
        )
        .map_err(unavailable)?;
    }
    for (record, _) in &changes.orders {
        // A block's members, and from contract v27 a net's: in no other
        // live block or net (W11.1).
        for member in &crate::booking::joined_members(record) {
            tx.execute(
                "INSERT INTO book_block_member (block_order_id, member_order_id, account_id)
                 VALUES ($1, $2, $3) ON CONFLICT (block_order_id, member_order_id) DO NOTHING",
                &[&record.order_id, member, &account_id],
            )
            .map_err(unavailable)?;
        }
    }
    for (change_request_id, order_id, placement_id) in &changes.change_requests {
        tx.execute(
            "INSERT INTO book_change_request (change_request_id, account_id, order_id, placement_id)
             VALUES ($1, $2, $3, $4) ON CONFLICT (change_request_id) DO NOTHING",
            &[change_request_id, &account_id, order_id, placement_id],
        )
        .map_err(unavailable)?;
    }
    Ok(())
}

/// Each order an entry changed, with the entry: an order's history.
fn project_order_entries(
    tx: &mut Transaction<'_>,
    account_id: &str,
    entry_id: &str,
    first_sequence: u64,
    changes: &Changes,
) -> Result<()> {
    for (record, _) in &changes.orders {
        tx.execute(
            "INSERT INTO book_order_entry (account_id, order_id, entry_id, first_sequence)
             VALUES ($1, $2, $3, $4) ON CONFLICT (order_id, entry_id) DO NOTHING",
            &[
                &account_id,
                &record.order_id,
                &entry_id,
                &(first_sequence as i64),
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
    let ledger = &book.ledger;
    for (id, held) in &ledger.orders {
        changes
            .orders
            .push((ledger.order_record(id, true)?, Vec::new()));
        changes
            .order_expiries
            .insert(id.clone(), crate::orders::due_at(held));
    }
    for id in ledger.placements.keys() {
        changes
            .placements
            .push((ledger.placement_record(id)?, Vec::new()));
    }
    changes.executions = ledger
        .executions
        .values()
        .map(|held| held.execution.clone())
        .collect();
    changes.unplaced = ledger.unplaced.values().cloned().collect();
    for id in ledger.locates.keys() {
        changes.locates.push(ledger.locate_record(id)?);
    }
    changes.instructions = ledger.instructions.values().cloned().collect();
    for id in ledger.bookings.keys() {
        changes.bookings.push(ledger.booking_record(id)?);
    }
    changes.alerts = ledger.alerts.values().cloned().collect();
    changes.change_requests = ledger
        .changes
        .values()
        .map(|request| {
            (
                request.change_request_id.clone(),
                request.order_id.clone(),
                request.placement_id.clone(),
            )
        })
        .collect();
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

/// How many times an act follows its account to another partition before it
/// is refused: a move between the guess and the lock is rare, two rarer.
const MOVES_FOLLOWED: usize = 4;

/// A batch's transaction that did not commit.
enum Unbatched {
    /// Nothing in it can commit now -- no connection, the epoch fenced, the
    /// commit itself failed -- so each command is answered this.
    Whole(StoreError),
    /// An unexpected database error mid-batch: the batch is tried again one
    /// command at a time, so it is refused only for the command it was.
    Broken(StoreError),
}

/// The same refusal again, for each command of a batch.
fn again(failed: &StoreError) -> StoreError {
    match failed {
        StoreError::Fenced(fenced) => StoreError::Fenced(*fenced),
        StoreError::Unavailable(words) => StoreError::Unavailable(words.clone()),
        other => StoreError::Unavailable(other.to_string()),
    }
}

impl PostgresStore {
    /// The partition `account_id` was last found in by this store; where it
    /// has not been, the partition an account starts in.
    fn placed(&self, account_id: &str) -> String {
        self.placed
            .lock()
            .expect("placements poisoned")
            .get(account_id)
            .cloned()
            .unwrap_or_else(|| first_partition_of(account_id).to_string())
    }

    /// The partition the record holds `account_id` in now; where it holds
    /// none, the partition an account starts in.
    fn found(&self, account_id: &str) -> Result<String> {
        Ok(self
            .conn()?
            .query_opt(
                "SELECT partition FROM book_account WHERE account_id = $1",
                &[&account_id],
            )
            .map_err(unavailable)?
            .map(|row| row.get(0))
            .unwrap_or_else(|| first_partition_of(account_id).to_string()))
    }

    fn place(&self, account_id: &str, partition: &str) {
        let mut placed = self.placed.lock().expect("placements poisoned");
        if placed.get(account_id).map(String::as_str) != Some(partition) {
            placed.insert(account_id.to_string(), partition.to_string());
        }
    }

    /// `partition`'s lane: the commands waiting for its transaction in flight.
    fn lane(&self, partition: &str) -> Arc<Lane> {
        Arc::clone(
            self.lanes
                .lock()
                .expect("lanes poisoned")
                .entry(partition.to_string())
                .or_default(),
        )
    }

    /// One batch of `partition`'s commands in one transaction (section 6.5),
    /// each answered its own outcome in order. An unexpected database error
    /// mid-batch tries the batch again one command at a time.
    fn batch(
        &self,
        partition: &str,
        batch: &[Pending],
        decider: &mut Decider<'_, '_>,
    ) -> Vec<Result<Acted>> {
        let outcomes = match self.try_batch(partition, batch, decider) {
            Ok(outcomes) => outcomes,
            Err(Unbatched::Broken(failed)) if batch.len() > 1 => {
                tracing::warn!(
                    partition,
                    commands = batch.len(),
                    %failed,
                    "a batch's transaction failed: each of its commands is tried alone"
                );
                batch
                    .iter()
                    .map(|pending| {
                        match self.try_batch(partition, std::slice::from_ref(pending), decider) {
                            Ok(mut alone) => alone.pop().unwrap_or_else(|| Err(again(&failed))),
                            Err(Unbatched::Whole(failed) | Unbatched::Broken(failed)) => {
                                Err(failed)
                            }
                        }
                    })
                    .collect()
            }
            Err(Unbatched::Whole(failed) | Unbatched::Broken(failed)) => {
                return batch.iter().map(|_| Err(again(&failed))).collect();
            }
        };
        // Its commits told in their order, the batch's whole page at once.
        if let Some(observer) = self.committed.lock().expect("observer poisoned").as_ref() {
            let committed: Vec<&Decided> = outcomes
                .iter()
                .filter_map(|outcome| match outcome {
                    Ok(Acted::Committed(decided)) => Some(decided.as_ref()),
                    _ => None,
                })
                .collect();
            if !committed.is_empty() {
                observer(self.fence.epoch(), &committed);
            }
        }
        outcomes
    }

    fn try_batch(
        &self,
        partition: &str,
        batch: &[Pending],
        decider: &mut Decider<'_, '_>,
    ) -> std::result::Result<Vec<Result<Acted>>, Unbatched> {
        let whole = |failed: postgres::Error| Unbatched::Whole(unavailable(failed));
        let mut conn = self.conn().map_err(Unbatched::Whole)?;
        let mut tx = conn.transaction().map_err(whole)?;
        tx.execute(
            "SELECT pg_advisory_xact_lock($1)",
            &[&partition_lock(partition)],
        )
        .map_err(whole)?;
        // The head under its lock, its epoch the writer's (step 3a), checked
        // once for the batch (section 6.5): a batch commits only while the
        // head row's epoch is its own.
        let held = tx
            .query_opt(
                "SELECT sequence, epoch, last_digest FROM book_partition_head WHERE partition = $1 FOR UPDATE",
                &[&partition],
            )
            .map_err(whole)?
            .ok_or_else(|| {
                Unbatched::Whole(StoreError::Unavailable(format!(
                    "the book has no partition {partition}"
                )))
            })?;
        self.fence
            .admits(held.get::<_, i64>(1) as u64)
            .map_err(|fenced| Unbatched::Whole(fenced.into()))?;
        // The epoch the entries are committed under: the head row's, which
        // the fence has just admitted as the writer's own -- never one read
        // before the connection was taken, which a connection opened on a
        // new database timeline raises (contract v30, stage C; failover-
        // repeat 43015006850, torn-write).
        let epoch = held.get::<_, i64>(1) as u64;
        let mut head = held.get::<_, i64>(0).max(0) as u64;
        // The partition's last digest, its chain's end (step 3b).
        let mut last_digest: Vec<u8> = held.get::<_, Option<Vec<u8>>>(2).unwrap_or_default();
        let began = head;
        let mut outcomes = Vec::with_capacity(batch.len());
        for pending in batch {
            let outcome = self
                .one(
                    &mut tx,
                    partition,
                    epoch,
                    (&mut head, &mut last_digest),
                    pending,
                    decider,
                )
                .map_err(Unbatched::Broken)?;
            outcomes.push(outcome);
        }
        if head != began {
            tx.execute(
                "UPDATE book_partition_head SET sequence = $1, last_digest = $3 WHERE partition = $2",
                &[&(head as i64), &partition, &last_digest],
            )
            .map_err(|failed| Unbatched::Broken(unavailable(failed)))?;
        }
        tx.commit().map_err(whole)?;
        Ok(outcomes)
    }

    /// One command of a batch, in its transaction: its outcome, or the
    /// database error that broke the batch. A refusal is its own and
    /// writes nothing; the batch goes on.
    fn one(
        &self,
        tx: &mut Transaction<'_>,
        partition: &str,
        epoch: u64,
        (head, last_digest): (&mut u64, &mut Vec<u8>),
        pending: &Pending,
        decider: &mut Decider<'_, '_>,
    ) -> Result<Result<Acted>> {
        let Command {
            account_id,
            message_id,
            idempotency_key,
            request,
        } = &pending.command;
        // Its account's partition read again under the partition's lock
        // (section 6.3): an account found in another is acted on there.
        let now: String = tx
            .query_opt(
                "SELECT partition FROM book_account WHERE account_id = $1",
                &[account_id],
            )
            .map_err(unavailable)?
            .map(|row| row.get(0))
            .unwrap_or_else(|| first_partition_of(account_id).to_string());
        if now != partition {
            return Ok(Err(StoreError::Moved(now)));
        }
        // A duplicate is answered with the first's reply, nothing applied:
        // the same message, or the same command under the same key. Another
        // command under a key the account holds is refused (Q12). A batch's
        // earlier command is seen: it is in this transaction.
        if !message_id.is_empty() {
            if let Some(row) = tx
                .query_opt(
                    "SELECT reply FROM book_entry WHERE message_id = $1",
                    &[message_id],
                )
                .map_err(unavailable)?
            {
                return Ok(Ok(Acted::Duplicate(row.get(0))));
            }
        }
        if !idempotency_key.is_empty() {
            if let Some(row) = tx
                .query_opt(
                    "SELECT reply, request FROM book_entry
                      WHERE account_id = $1 AND idempotency_key = $2",
                    &[account_id, idempotency_key],
                )
                .map_err(unavailable)?
            {
                let held: Vec<u8> = row.get(1);
                if &held != request {
                    return Ok(Err(StoreError::conflict(idempotency_key, account_id)));
                }
                return Ok(Ok(Acted::Duplicate(row.get(0))));
            }
        }

        // From its latest snapshot and the journal's tail (contract v25,
        // W9.10), never the whole journal once one is kept.
        let (book, replayed) = book_from(tx, account_id, partition)?;
        // Decided by its own caller, each entry's body the decision asks for
        // read in this transaction (contract v26, W9.10). A read that failed
        // broke the transaction, and so the batch.
        let mut broke = None;
        let decided = pending.decide(decider, book, *head, |account, ids| {
            bodies_of(tx, account, ids).inspect_err(|failed| broke = Some(again(failed)))
        });
        if let Some(failed) = broke {
            return Err(failed);
        }
        let mut decided = match decided {
            Ok(decided) => decided,
            Err(refused) => return Ok(Err(refused)),
        };
        // Its place in the partition's chain (step 3b, integrity at rest).
        decided.entry.previous_digest = last_digest.clone();
        decided.entry.digest = meridian_changes::integrity::chained(
            &decided.entry.previous_digest,
            &decided.entry.canonical(),
        );
        let entry = &decided.entry;

        tx.execute(
            "INSERT INTO book_account (account_id, partition) VALUES ($1, $2)
             ON CONFLICT (account_id) DO NOTHING",
            &[account_id, &partition],
        )
        .map_err(unavailable)?;
        tx.execute(
            "INSERT INTO book_entry (entry_id, account_id, partition, first_sequence, last_sequence,
                                     kind, effective_date, message_id, idempotency_key,
                                     committed_at_ns, meta, cause, body, reply, actor, request,
                                     epoch, previous_digest, digest)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17,
                     $18, $19)",
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
                request,
                &(epoch as i64),
                &entry.previous_digest,
                &entry.digest,
            ],
        )
        .map_err(unavailable)?;
        project(tx, account_id, &decided.changes)?;
        project_order_entries(
            tx,
            account_id,
            &entry.entry_id,
            entry.first_sequence,
            &decided.changes,
        )?;
        // A snapshot once the tail since the last one reaches the interval,
        // as at this entry, in the same transaction.
        if replayed.entries as u64 + 1 >= self.snapshot_interval {
            keep_snapshot(tx, &decided.book, entry.cause.committed_at_ns)?;
        }
        *head = entry.last_sequence;
        last_digest.clone_from(&entry.digest);
        Ok(Ok(Acted::Committed(Box::new(decided))))
    }
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
        // The writer's epoch, adopted before a connection is taken, so a
        // store of one connection never waits on itself (step 3a).
        if self.fence.epoch() == 0 {
            self.adopt()?;
        }
        let command = Command {
            account_id: account_id.to_string(),
            message_id: message_id.to_string(),
            idempotency_key: idempotency_key.to_string(),
            request: request.to_vec(),
        };
        // An account never placed, with several partitions: placed by its
        // owner label first, never by its ID instead (step 3d; ruled
        // 2026-11-13). The book's writer places it before its act.
        if self.placement_needed(account_id)? {
            return Err(StoreError::NotPlaced(format!(
                "account {account_id} is not placed in a partition yet: its owner label was not read"
            )));
        }
        // The partition the account was last found in: a guess its batch
        // checks under the partition's lock (step 3d; section 6.3), and an
        // account found elsewhere there is acted on in its partition.
        let mut partition = self.placed(account_id);
        for _ in 0..MOVES_FOLLOWED {
            // Another writer group's partition: handed to its owner (step
            // 3d; section 6.7), never committed here.
            if !self.groups.owns(&partition) {
                // The guess read again from the record before the act is
                // handed on: the account may have moved to this group's.
                let found = self.found(account_id)?;
                if found != partition {
                    self.place(account_id, &found);
                    partition = found;
                    continue;
                }
                let group = self.groups.owner(&partition);
                meridian_bus::owners::hand_to(group);
                return Err(StoreError::NotOwned { group, partition });
            }
            let lane = self.lane(&partition);
            let acted =
                crate::group_commit::act(&lane, command.clone(), decide, |batch, decider| {
                    self.batch(&partition, batch, decider)
                });
            match acted {
                Err(StoreError::Moved(now)) => {
                    self.place(account_id, &now);
                    partition = now;
                }
                other => {
                    if other.is_ok() {
                        self.place(account_id, &partition);
                    }
                    return other;
                }
            }
        }
        Err(StoreError::Unavailable(format!(
            "account {account_id} was found in another partition {MOVES_FOLLOWED} times while it was acted on"
        )))
    }

    fn writes(&self, account_id: &str) -> bool {
        self.groups.owns(&self.placed(account_id))
    }

    fn control_records_owed(&self) -> Result<Vec<crate::control::Owed>> {
        use meridian_changes::records;
        let mut conn = self.conn()?;
        let journalled: std::collections::BTreeSet<String> = conn
            .query(
                "SELECT idempotency_key FROM book_entry
                  WHERE account_id = $1 AND idempotency_key LIKE 'control-record:%'",
                &[&crate::store::CONTROL_ACCOUNT],
            )
            .map_err(unavailable)?
            .iter()
            .map(|row| row.get(0))
            .collect();
        let mut owed = Vec::new();
        // Each takeover, by its epoch, as it was kept in its own fencing.
        for row in conn
            .query(
                "SELECT epoch, member, partitions, why, at_ns FROM book_writer_took_over
                  ORDER BY epoch",
                &[],
            )
            .map_err(unavailable)?
        {
            let epoch: i64 = row.get(0);
            let key = format!("control-record:took-over:{epoch}");
            if journalled.contains(&key) {
                continue;
            }
            let by = meridian_changes::TookOver {
                member: row.get(1),
                previous: String::new(),
                why: row.get(3),
                at_ns: row.get(4),
            };
            let record = records::took_over("bor", &by, row.get(2), epoch as u64);
            owed.push(crate::control::Owed {
                key,
                kind: records::TOOK_OVER,
                body: record.encode_to_vec(),
                reason: format!("the book's writer took over: {}", by.why),
            });
        }
        // Each finding, by its row, as the scrub kept it.
        for row in conn
            .query(
                "SELECT id, partition, sequence, committed_at_ns, what, found_at_ns
                   FROM book_integrity_finding ORDER BY id",
                &[],
            )
            .map_err(unavailable)?
        {
            let id: i64 = row.get(0);
            let key = format!("control-record:finding:{id}");
            if journalled.contains(&key) {
                continue;
            }
            let found = meridian_changes::integrity::Finding {
                partition: row.get(1),
                sequence: row.get::<_, i64>(2) as u64,
                committed_at_ns: row.get(3),
                what: row.get(4),
            };
            let record = records::finding(
                "bor",
                meridian_domain::v1::IntegrityCheck::JournalChain,
                &found,
                row.get(5),
            );
            owed.push(crate::control::Owed {
                key,
                kind: records::FINDING,
                body: record.encode_to_vec(),
                reason: format!("an integrity finding: {}", found.what),
            });
        }
        // Each partition's writer group, where its last assignment named
        // another (none: group 0).
        let mut assigned: std::collections::BTreeMap<String, u32> =
            std::collections::BTreeMap::new();
        for row in conn
            .query(
                "SELECT idempotency_key FROM book_entry
                  WHERE account_id = $1 AND idempotency_key LIKE 'control-record:owner:%'
                  ORDER BY first_sequence",
                &[&crate::store::CONTROL_ACCOUNT],
            )
            .map_err(unavailable)?
        {
            let key: String = row.get(0);
            let mut said = key
                .trim_start_matches("control-record:owner:")
                .rsplitn(3, ':');
            let (_, group, partition) = (said.next(), said.next(), said.next());
            if let (Some(group), Some(partition)) = (group.and_then(|g| g.parse().ok()), partition)
            {
                assigned.insert(partition.to_string(), group);
            }
        }
        for row in conn
            .query(
                "SELECT partition, epoch FROM book_partition_head ORDER BY partition",
                &[],
            )
            .map_err(unavailable)?
        {
            let (partition, epoch): (String, i64) = (row.get(0), row.get(1));
            let group = self.groups.owner(&partition);
            if assigned.get(&partition).copied().unwrap_or(0) == group {
                continue;
            }
            let record = meridian_domain::v1::PartitionOwner {
                component: "bor".into(),
                partition: partition.clone(),
                group,
                writer: String::new(),
                epoch: (epoch as u64).max(1),
            };
            owed.push(crate::control::Owed {
                key: format!("control-record:owner:{partition}:{group}:{epoch}"),
                kind: crate::control::OWNER,
                body: record.encode_to_vec(),
                reason: format!("partition {partition} assigned to writer group {group}"),
            });
        }
        Ok(owed)
    }

    fn placement_needed(&self, account_id: &str) -> Result<bool> {
        if self.partitions < 2
            || !crate::store::is_account(account_id)
            || self
                .placed
                .lock()
                .expect("placements poisoned")
                .contains_key(account_id)
        {
            return Ok(false);
        }
        let found: Option<String> = self
            .conn()?
            .query_opt(
                "SELECT partition FROM book_account WHERE account_id = $1",
                &[&account_id],
            )
            .map_err(unavailable)?
            .map(|row| row.get(0));
        match found {
            Some(partition) => {
                self.place(account_id, &partition);
                Ok(false)
            }
            None => Ok(true),
        }
    }

    fn partition_count(&self) -> u32 {
        self.partitions
    }

    fn partition_of(&self, account_id: &str) -> Option<String> {
        if let Some(partition) = self
            .placed
            .lock()
            .expect("placements poisoned")
            .get(account_id)
        {
            return Some(partition.clone());
        }
        let found: Option<String> = self
            .conn()
            .ok()?
            .query_opt(
                "SELECT partition FROM book_account WHERE account_id = $1",
                &[&account_id],
            )
            .ok()?
            .map(|row| row.get(0));
        if let Some(partition) = &found {
            self.place(account_id, partition);
        }
        found
    }

    fn place_first(&self, account_id: &str, key: &str) -> Result<String> {
        let partition = crate::store::placement_of(key, self.partitions);
        let mut conn = self.conn()?;
        // Fixed at its creation: an account placed already keeps its own.
        conn.execute(
            "INSERT INTO book_account (account_id, partition, placement_key) VALUES ($1, $2, $3)
             ON CONFLICT (account_id) DO NOTHING",
            &[&account_id, &partition, &key],
        )
        .map_err(unavailable)?;
        let placed: String = conn
            .query_one(
                "SELECT partition FROM book_account WHERE account_id = $1",
                &[&account_id],
            )
            .map_err(unavailable)?
            .get(0);
        self.place(account_id, &placed);
        Ok(placed)
    }

    fn writer_group_of(&self, account_id: &str) -> Option<u32> {
        if !self.groups.several() {
            return None;
        }
        // Where this member has seen it, else where the record holds it,
        // learnt (step 3d): a guess from the partition an account starts in
        // routes it wrong once the book has several partitions. An account
        // not placed yet is no group's to route to: the member it reaches
        // places it, and hands it on where it landed.
        let cached = self
            .placed
            .lock()
            .expect("placements poisoned")
            .get(account_id)
            .cloned();
        let partition = match cached {
            Some(partition) => partition,
            None if !crate::store::is_account(account_id) => {
                first_partition_of(account_id).to_string()
            }
            None => {
                let found: Option<String> = self
                    .conn()
                    .ok()?
                    .query_opt(
                        "SELECT partition FROM book_account WHERE account_id = $1",
                        &[&account_id],
                    )
                    .ok()?
                    .map(|row| row.get(0));
                match found {
                    Some(partition) => {
                        self.place(account_id, &partition);
                        partition
                    }
                    None if self.partitions > 1 => return None,
                    None => first_partition_of(account_id).to_string(),
                }
            }
        };
        Some(self.groups.owner(&partition))
    }

    fn on_commit(&self, observer: Committed) -> bool {
        *self.committed.lock().expect("observer poisoned") = Some(observer);
        true
    }

    fn journal(&self, account_id: &str) -> Result<Vec<Entry>> {
        if stamping() {
            // A read replica's: one snapshot, its heads beside (stage C2).
            return self.reading(|tx| journal_of(tx, account_id));
        }
        journal_of(&mut *self.conn()?, account_id)
    }

    fn adopt(&self) -> Result<u64> {
        let mut conn = self.conn()?;
        conn.execute(
            "UPDATE book_partition_head SET origin = $1 WHERE origin = '' AND $1 <> ''",
            &[&meridian_changes::origin()],
        )
        .map_err(unavailable)?;
        if !self.groups.several() {
            let epoch: i64 = conn
                .query_one(
                    "SELECT epoch FROM book_partition_head WHERE partition = $1",
                    &[&CONTROL_PARTITION],
                )
                .map_err(unavailable)?
                .get(0);
            return Ok(self.fence.adopt_once(epoch as u64));
        }
        // Several groups (step 3d; section 6.3): the epoch its partitions
        // share, where no other group's partition holds it; else they were
        // assigned to it -- the count changed -- and it takes them over, so
        // the writer that held them before is fenced.
        let _alone = self.adopting.lock().expect("adopting poisoned");
        if self.fence.epoch() != 0 {
            return Ok(self.fence.epoch());
        }
        let (mine, others) = epochs_of(&mut *conn, self.groups)?;
        drop(conn);
        if mine.is_empty() {
            // It owns no partition: it hands every act on, and commits none.
            return Ok(0);
        }
        let shared = mine.iter().min() == mine.iter().max();
        match mine.first() {
            Some(epoch) if shared && !others.contains(epoch) => Ok(self.fence.adopt_once(*epoch)),
            _ => self.take_over(&meridian_changes::TookOver::unnamed(
                meridian_changes::groups::ASSIGNED,
            )),
        }
    }

    fn epoch(&self) -> u64 {
        self.fence.epoch()
    }

    fn take_over(&self, by: &meridian_changes::TookOver) -> Result<u64> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        // A deposed writer stopped mid-entry holds the head row it took.
        if self.groups.several() {
            // Only its own group's deposed writer: another group's sessions
            // hold their own partitions, never this group's.
            end_deposed_holders(&mut tx, self.groups).map_err(unavailable)?;
        } else {
            meridian_changes::end_deposed_writers(&mut tx, &["book_partition_head"])
                .map_err(unavailable)?;
        }
        let epoch = raise(&mut tx, by, self.groups).map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        self.fence.adopt(epoch);
        Ok(epoch)
    }

    fn follow_timeline(&self) -> Result<meridian_changes::timeline::Timeline> {
        let mut conn = self.pool.get().map_err(unavailable)?;
        self.follow_on(&mut conn)
    }

    fn head_epoch(&self) -> Result<u64> {
        let mut conn = self.conn()?;
        let anchor = anchor_of(&mut *conn, self.groups)?;
        let Some(anchor) = anchor else {
            return Ok(0);
        };
        let epoch: i64 = conn
            .query_one(
                "SELECT epoch FROM book_partition_head WHERE partition = $1",
                &[&anchor],
            )
            .map_err(unavailable)?
            .get(0);
        Ok(epoch as u64)
    }

    fn read_changes(
        &self,
        partition: &str,
        after: u64,
        most_bytes: usize,
    ) -> std::result::Result<meridian_changes::ReadChangesReply, meridian_changes::ReadError> {
        use meridian_changes::{ReadChangesReply, ReadError};
        let failed = |failed: StoreError| ReadError::Unavailable(failed.to_string());
        let mut conn = self.conn().map_err(failed)?;
        let mut tx = conn
            .build_transaction()
            .isolation_level(postgres::IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .map_err(|e| failed(unavailable(e)))?;
        let held = tx
            .query_opt(
                "SELECT sequence, epoch, origin FROM book_partition_head WHERE partition = $1",
                &[&partition],
            )
            .map_err(|e| failed(unavailable(e)))?
            .ok_or_else(|| ReadError::UnknownPartition(partition.to_string()))?;
        let head = held.get::<_, i64>(0) as u64;
        let epoch = held.get::<_, i64>(1) as u64;
        let own_origin: String = held.get(2);
        // The journal is kept for good: nothing is ever behind it.
        let lowest_kept = u64::from(head > 0);
        const BATCH: i64 = 256;
        let mut page = meridian_changes::Page::new(most_bytes);
        let mut from = after;
        'pages: while from < head {
            let rows = tx
                .query(
                    &format!(
                        "SELECT {ENTRY_COLUMNS}, epoch, origin FROM book_entry
                          WHERE partition = $1 AND last_sequence > $2
                          ORDER BY last_sequence LIMIT $3"
                    ),
                    &[&partition, &(from as i64), &BATCH],
                )
                .map_err(|e| failed(unavailable(e)))?;
            if rows.is_empty() {
                break;
            }
            for row in &rows {
                let entry = entry_of(row).map_err(failed)?;
                let origin: Option<String> = row.get(13);
                let change = entry.to_change(
                    row.get::<_, i64>(12) as u64,
                    &origin.unwrap_or_else(|| own_origin.clone()),
                );
                from = change.sequence;
                if !page.take(change) {
                    break 'pages;
                }
            }
            if page.is_full() {
                break;
            }
        }
        let _ = tx.commit();
        Ok(ReadChangesReply {
            changes: page.changes,
            head,
            lowest_kept,
            epoch,
            after_sequence: 0,
        })
    }

    fn bodies(&self, account_id: &str, entry_ids: &[String]) -> Result<Vec<(String, Body)>> {
        if stamping() {
            return self.reading(|tx| bodies_of(tx, account_id, entry_ids));
        }
        bodies_of(&mut *self.conn()?, account_id, entry_ids)
    }

    fn book_health(
        &self,
        only: Option<&str>,
        after: &str,
        limit: usize,
    ) -> Result<Vec<meridian_domain::v1::AccountBookHealth>> {
        // Each account's journal and its latest snapshot of this format, in
        // the order accounts are listed.
        Ok(self
            .conn()?
            .query(
                "SELECT a.account_id,
                        (SELECT count(*) FROM book_entry e WHERE e.account_id = a.account_id),
                        (SELECT count(*) FROM book_entry e WHERE e.account_id = a.account_id
                            AND e.first_sequence > COALESCE(s.last_sequence, -1)),
                        COALESCE(s.taken_at_ns, 0)
                   FROM book_account a
                   LEFT JOIN LATERAL (
                        SELECT last_sequence, taken_at_ns FROM book_snapshot
                         WHERE account_id = a.account_id AND format = $3
                         ORDER BY last_sequence DESC LIMIT 1) s ON true
                  WHERE a.account_id NOT LIKE '~%'
                    AND a.account_id COLLATE \"C\" > $1
                    AND ($4::text IS NULL OR a.account_id = $4)
                  ORDER BY a.account_id COLLATE \"C\"
                  LIMIT $2",
                &[&after, &(limit as i64), &crate::snapshot::FORMAT, &only],
            )
            .map_err(unavailable)?
            .into_iter()
            .map(|row| meridian_domain::v1::AccountBookHealth {
                account_id: row.get(0),
                journal_entries: row.get::<_, i64>(1).max(0) as u64,
                entries_since_snapshot: row.get::<_, i64>(2).max(0) as u64,
                last_snapshot_at_ns: row.get(3),
            })
            .collect())
    }

    fn book(&self, account_id: &str) -> Result<Book> {
        let mut conn = self.conn()?;
        let partition: String = conn
            .query_opt(
                "SELECT partition FROM book_account WHERE account_id = $1",
                &[&account_id],
            )
            .map_err(unavailable)?
            .map(|row| row.get(0))
            .unwrap_or_else(|| first_partition_of(account_id).to_string());
        let (book, replayed) = book_from(&mut *conn, account_id, &partition)?;
        keep_read_snapshot(&mut *conn, &book, replayed, self.snapshot_interval);
        Ok(book)
    }

    fn book_as_of(&self, account_id: &str, at: &Mark, until: Option<&str>) -> Result<Book> {
        if stamping() {
            // A read replica's: replayed in one snapshot, its heads beside,
            // and no read's snapshot kept -- a replica writes nothing.
            return self
                .reading(|tx| book_as_of_in(tx, account_id, at, until).map(|(book, _)| book));
        }
        let mut conn = self.conn()?;
        let (book, (entries, at)) = book_as_of_in(&mut *conn, account_id, at, until)?;
        // A read that reached the journal's end with no date is the book
        // now, kept as a read's snapshot where it replayed past the interval
        // (v26's core).
        if until.is_none() && entries.last().is_none_or(|last| last.first_sequence <= at) {
            keep_read_snapshot(
                &mut *conn,
                &book,
                Replayed::of(&entries),
                self.snapshot_interval,
            );
        }
        Ok(book)
    }

    fn heads(&self) -> Result<Mark> {
        if stamping() {
            return self.reading(|tx| heads_of(tx));
        }
        heads_of(&mut *self.conn()?)
    }

    fn accounts(&self) -> Result<Vec<String>> {
        if stamping() {
            return self.reading(|tx| accounts_of(tx));
        }
        accounts_of(&mut *self.conn()?)
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
            (String::new(), String::new(), -1, String::new())
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
                            AND (account_id COLLATE \"C\", instrument_id COLLATE \"C\", side,
                                 sleeve COLLATE \"C\")
                                > ($5 COLLATE \"C\", $6 COLLATE \"C\", $7, $8 COLLATE \"C\")
                          ORDER BY account_id COLLATE \"C\", instrument_id COLLATE \"C\", side,
                                   sleeve COLLATE \"C\"
                          LIMIT $9"
                    ),
                    &[
                        &scope_param(&read.scope),
                        &read.account_id,
                        &partitions,
                        &sequences,
                        &after.0,
                        &after.1,
                        &after.2,
                        &after.3,
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
        if self.fence.epoch() == 0 {
            self.adopt()?;
        }
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        // The whole book, however large: without the serving session's
        // statement_timeout (section 10a).
        meridian_changes::serving::unbounded(&mut tx).map_err(unavailable)?;
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
        // Rebuilt only by the writer whose epoch every partition holds.
        let epochs = tx
            .query("SELECT epoch FROM book_partition_head FOR UPDATE", &[])
            .map_err(unavailable)?;
        for row in epochs {
            self.fence.admits(row.get::<_, i64>(0) as u64)?;
        }
        tx.batch_execute(
            "DELETE FROM book_snapshot;
             DELETE FROM book_position; DELETE FROM book_lot; DELETE FROM book_pending;
             DELETE FROM book_break; DELETE FROM book_figures; DELETE FROM book_attributes;
             DELETE FROM book_order; DELETE FROM book_placement; DELETE FROM book_execution;
             DELETE FROM book_unplaced; DELETE FROM book_locate; DELETE FROM book_instruction;
             DELETE FROM book_change_request; DELETE FROM book_order_entry;
             DELETE FROM book_booking; DELETE FROM book_alert; DELETE FROM book_block_member;",
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
            let book = Book::replay_lean(&account, &partition, &entries)?;
            project(&mut tx, &account, &all_changes(&book)?)?;
            // Snapshots made again from the journal (contract v25, W9.10):
            // as at the latest entry, where the journal reaches the interval.
            if entries.len() as u64 >= self.snapshot_interval {
                let taken = entries
                    .last()
                    .map(|entry| entry.cause.committed_at_ns)
                    .unwrap_or_default();
                keep_snapshot(&mut tx, &book, taken)?;
            }
            // Each order's history, entry by entry, as the journal holds it.
            for (order_id, entry_ids) in &book.ledger.order_entries {
                for entry in entries
                    .iter()
                    .filter(|entry| entry_ids.contains(&entry.entry_id))
                {
                    tx.execute(
                        "INSERT INTO book_order_entry (account_id, order_id, entry_id, first_sequence)
                         VALUES ($1, $2, $3, $4) ON CONFLICT (order_id, entry_id) DO NOTHING",
                        &[&account, order_id, &entry.entry_id, &(entry.first_sequence as i64)],
                    )
                    .map_err(unavailable)?;
                }
            }
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

    fn owner(&self, record: Record, id: &str) -> Result<Option<String>> {
        let query = match record {
            Record::Order => "SELECT account_id FROM book_order WHERE order_id = $1",
            Record::Placement => "SELECT account_id FROM book_placement WHERE placement_id = $1",
            Record::ChangeRequest => {
                "SELECT account_id FROM book_change_request WHERE change_request_id = $1"
            }
            Record::Locate => "SELECT account_id FROM book_locate WHERE locate_record_id = $1",
            Record::Execution => "SELECT account_id FROM book_execution WHERE execution_id = $1",
            Record::Booking => "SELECT account_id FROM book_booking WHERE booking_id = $1",
            Record::Alert => "SELECT account_id FROM book_alert WHERE alert_id = $1",
        };
        Ok(self
            .conn()?
            .query_opt(query, &[&id])
            .map_err(unavailable)?
            .map(|row| row.get(0)))
    }

    fn orders(&self, read: &OrdersRead) -> Result<OrdersPage> {
        read.scope.admit(&read.account_id)?;
        let after = if read.cursor.is_empty() {
            (0usize, String::new(), String::new())
        } else {
            order_reads::from_stream_cursor(&read.cursor)?
        };
        let (partitions, sequences) = mark_params(&read.since.clone().unwrap_or_default());
        let filter = if read.since.is_some() {
            ABOVE
        } else {
            UNUSED_MARK
        };
        let tables = [
            ("book_order", "order_id"),
            ("book_placement", "placement_id"),
            ("book_execution", "execution_id"),
            ("book_unplaced", "unplaced_id"),
        ];
        let scope = scope_param(&read.scope);
        // Contract v23 (W11.17): filtered by an execution's state, the read
        // answers executions alone; unmatched, those still waiting for an
        // answer -- neither superseded nor a bust.
        let state_filter = match read.execution_state {
            0 => String::new(),
            1 => "AND state = 1
                  AND NOT (supersedes <> '' AND COALESCE(quantity, 0) = 0)
                  AND NOT EXISTS (SELECT 1 FROM book_execution s
                                   WHERE s.account_id = book_execution.account_id
                                     AND s.supersedes = book_execution.execution_id)"
                .to_string(),
            state => format!("AND state = {state}"),
        };
        self.reading(|tx| {
            let mut items = Vec::new();
            for (kind, (table, id)) in tables.iter().enumerate() {
                if kind < after.0 {
                    continue;
                }
                if read.execution_state != 0 && kind != 2 {
                    continue;
                }
                let state_filter = if kind == 2 { state_filter.as_str() } else { "" };
                let (from_account, from_id) = if kind == after.0 && !read.cursor.is_empty() {
                    (after.1.clone(), after.2.clone())
                } else {
                    (String::new(), String::new())
                };
                let rows = tx
                    .query(
                        &format!(
                            "SELECT account_id, {id}, record FROM {table}
                              WHERE ($1::text[] IS NULL OR account_id = ANY($1))
                                AND ($2 = '' OR account_id = $2)
                                AND {filter}
                                {state_filter}
                                AND (account_id COLLATE \"C\", {id} COLLATE \"C\")
                                    > ($5 COLLATE \"C\", $6 COLLATE \"C\")
                              ORDER BY account_id COLLATE \"C\", {id} COLLATE \"C\"
                              LIMIT $7"
                        ),
                        &[
                            &scope,
                            &read.account_id,
                            &partitions,
                            &sequences,
                            &from_account,
                            &from_id,
                            &(read.limit as i64 + 1),
                        ],
                    )
                    .map_err(unavailable)?;
                for row in rows {
                    let account: String = row.get(0);
                    let record_id: String = row.get(1);
                    let bytes: Vec<u8> = row.get(2);
                    let item = match kind {
                        0 => order_reads::Item::Order(Box::new(
                            meridian_domain::v1::Order::decode(bytes.as_slice())
                                .map_err(unreadable)?,
                        )),
                        1 => order_reads::Item::Placement(Box::new(
                            meridian_domain::v1::Placement::decode(bytes.as_slice())
                                .map_err(unreadable)?,
                        )),
                        2 => order_reads::Item::Execution(Box::new(
                            meridian_domain::v1::Execution::decode(bytes.as_slice())
                                .map_err(unreadable)?,
                        )),
                        _ => order_reads::Item::Unplaced(Box::new(
                            meridian_domain::v1::UnplacedFact::decode(bytes.as_slice())
                                .map_err(unreadable)?,
                        )),
                    };
                    items.push(((kind, account, record_id), item));
                }
                if items.len() > read.limit {
                    break;
                }
            }
            let mut unpaged = read.clone();
            unpaged.cursor = String::new();
            order_reads::orders_page(items, &unpaged, heads_of(tx)?)
        })
    }

    fn order_history(&self, read: &HistoryRead) -> Result<Page<meridian_domain::v1::EntryMeta>> {
        read.scope.admit(&read.account_id)?;
        let scope = scope_param(&read.scope);
        let entries = self.reading(|tx| {
            tx.query(
                "SELECT e.meta FROM book_order_entry o JOIN book_entry e USING (entry_id)
                  WHERE o.order_id = $1
                    AND ($2::text[] IS NULL OR o.account_id = ANY($2))
                    AND ($3 = '' OR o.account_id = $3)
                  ORDER BY e.first_sequence",
                &[&read.order_id, &scope, &read.account_id],
            )
            .map_err(unavailable)
        })?;
        order_reads::history_page(decoded(entries, 0)?, read)
    }

    fn locates(&self, read: &LocatesRead) -> Result<Page<meridian_domain::v1::Locate>> {
        read.scope.admit(&read.account_id)?;
        let (partitions, sequences) = mark_params(&read.since.clone().unwrap_or_default());
        let filter = if read.since.is_some() {
            ABOVE
        } else {
            UNUSED_MARK
        };
        let scope = scope_param(&read.scope);
        // The page's rows alone, after its cursor, in its key's order (stage
        // M; the store memory audit of 2026-11-09, item 7): never every row
        // decoded and sorted for one page.
        let after = order_reads::keyed_after(&read.cursor)?;
        let rows_read = order_reads::keyed_rows(read.limit);
        self.reading(|tx| {
            let rows = tx
                .query(
                    &format!(
                        "SELECT account_id, locate_record_id, record FROM book_locate
                          WHERE ($1::text[] IS NULL OR account_id = ANY($1))
                            AND ($2 = '' OR account_id = $2)
                            AND account_id NOT LIKE '~%'
                            AND {filter}
                            AND ($5 = '' OR (account_id || chr(31) || locate_record_id) COLLATE \"C\" > $5)
                          ORDER BY (account_id || chr(31) || locate_record_id) COLLATE \"C\"
                          LIMIT $6"
                    ),
                    &[&scope, &read.account_id, &partitions, &sequences, &after, &rows_read],
                )
                .map_err(unavailable)?;
            let mut records = Vec::new();
            for row in rows {
                let account: String = row.get(0);
                let id: String = row.get(1);
                let bytes: Vec<u8> = row.get(2);
                records.push((
                    format!("{account}\u{1f}{id}"),
                    meridian_domain::v1::Locate::decode(bytes.as_slice()).map_err(unreadable)?,
                ));
            }
            order_reads::keyed_page(records, &read.cursor, read.limit, heads_of(tx)?)
        })
    }

    fn instructions(
        &self,
        read: &InstructionsRead,
    ) -> Result<Page<meridian_domain::v1::TradingInstruction>> {
        let (partitions, sequences) = mark_params(&read.since.clone().unwrap_or_default());
        let filter = if read.since.is_some() {
            ABOVE.replace("$3", "$1").replace("$4", "$2")
        } else {
            "$1::text[] IS NOT NULL AND $2::bigint[] IS NOT NULL".to_string()
        };
        // The page's rows alone (stage M; audit item 7).
        let after = order_reads::keyed_after(&read.cursor)?;
        let rows_read = order_reads::keyed_rows(read.limit);
        self.reading(|tx| {
            let rows = tx
                .query(
                    &format!(
                        "SELECT instruction_key, record FROM book_instruction
                          WHERE {filter}
                            AND ($3 = '' OR instruction_key COLLATE \"C\" > $3)
                          ORDER BY instruction_key COLLATE \"C\"
                          LIMIT $4"
                    ),
                    &[&partitions, &sequences, &after, &rows_read],
                )
                .map_err(unavailable)?;
            let mut records = Vec::new();
            for row in rows {
                let key: String = row.get(0);
                let bytes: Vec<u8> = row.get(1);
                records.push((
                    key,
                    meridian_domain::v1::TradingInstruction::decode(bytes.as_slice())
                        .map_err(unreadable)?,
                ));
            }
            order_reads::keyed_page(records, &read.cursor, read.limit, heads_of(tx)?)
        })
    }

    fn expiring(&self, now_ns: i64) -> Result<Vec<(String, String)>> {
        Ok(self
            .conn()?
            .query(
                "SELECT account_id, order_id FROM book_order
                  WHERE expires_at_ns IS NOT NULL AND expires_at_ns <= $1 AND state = ANY($2)
                  ORDER BY expires_at_ns, order_id",
                &[
                    &now_ns,
                    &vec![
                        meridian_domain::v1::OrderState::Proposed as i32,
                        meridian_domain::v1::OrderState::Released as i32,
                        meridian_domain::v1::OrderState::Working as i32,
                    ],
                ],
            )
            .map_err(unavailable)?
            .into_iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect())
    }

    fn bookings(
        &self,
        read: &crate::store::BookingsRead,
    ) -> Result<Page<meridian_domain::v1::Booking>> {
        read.scope.admit(&read.account_id)?;
        let (partitions, sequences) = mark_params(&read.since.clone().unwrap_or_default());
        let filter = if read.since.is_some() {
            ABOVE
        } else {
            UNUSED_MARK
        };
        let scope = scope_param(&read.scope);
        // The page's rows alone (stage M; audit item 7).
        let after = order_reads::keyed_after(&read.cursor)?;
        let rows_read = order_reads::keyed_rows(read.limit);
        self.reading(|tx| {
            let rows = tx
                .query(
                    &format!(
                        "SELECT account_id, booking_id, record FROM book_booking
                          WHERE ($1::text[] IS NULL OR account_id = ANY($1))
                            AND ($2 = '' OR account_id = $2)
                            AND account_id NOT LIKE '~%'
                            AND ($5 = 0 OR state = $5)
                            AND {filter}
                            AND ($6 = '' OR (account_id || chr(31) || booking_id) COLLATE \"C\" > $6)
                          ORDER BY (account_id || chr(31) || booking_id) COLLATE \"C\"
                          LIMIT $7"
                    ),
                    &[
                        &scope,
                        &read.account_id,
                        &partitions,
                        &sequences,
                        &read.state,
                        &after,
                        &rows_read,
                    ],
                )
                .map_err(unavailable)?;
            let mut records = Vec::new();
            for row in rows {
                let account: String = row.get(0);
                let id: String = row.get(1);
                let bytes: Vec<u8> = row.get(2);
                records.push((
                    format!("{account}\u{1f}{id}"),
                    meridian_domain::v1::Booking::decode(bytes.as_slice()).map_err(unreadable)?,
                ));
            }
            order_reads::keyed_page(records, &read.cursor, read.limit, heads_of(tx)?)
        })
    }

    fn alerts(&self, read: &crate::store::AlertsRead) -> Result<Page<meridian_domain::v1::Alert>> {
        read.scope.admit(&read.account_id)?;
        let (partitions, sequences) = mark_params(&read.since.clone().unwrap_or_default());
        let filter = if read.since.is_some() {
            ABOVE
        } else {
            UNUSED_MARK
        };
        let scope = scope_param(&read.scope);
        // The page's rows alone (stage M; audit item 7).
        let after = order_reads::keyed_after(&read.cursor)?;
        let rows_read = order_reads::keyed_rows(read.limit);
        self.reading(|tx| {
            let rows = tx
                .query(
                    &format!(
                        "SELECT account_id, alert_id, record FROM book_alert
                          WHERE ($1::text[] IS NULL OR account_id = ANY($1))
                            AND ($2 = '' OR account_id = $2)
                            AND account_id NOT LIKE '~%'
                            AND ($5 = 0 OR state = $5)
                            AND ($6 = 0 OR kind = $6)
                            AND {filter}
                            AND ($7 = '' OR (account_id || chr(31) || alert_id) COLLATE \"C\" > $7)
                          ORDER BY (account_id || chr(31) || alert_id) COLLATE \"C\"
                          LIMIT $8"
                    ),
                    &[
                        &scope,
                        &read.account_id,
                        &partitions,
                        &sequences,
                        &read.state,
                        &read.kind,
                        &after,
                        &rows_read,
                    ],
                )
                .map_err(unavailable)?;
            let mut records = Vec::new();
            for row in rows {
                let account: String = row.get(0);
                let id: String = row.get(1);
                let bytes: Vec<u8> = row.get(2);
                records.push((
                    format!("{account}\u{1f}{id}"),
                    meridian_domain::v1::Alert::decode(bytes.as_slice()).map_err(unreadable)?,
                ));
            }
            order_reads::keyed_page(records, &read.cursor, read.limit, heads_of(tx)?)
        })
    }

    fn blocks_naming(&self, member_order_id: &str) -> Result<Vec<(String, String)>> {
        Ok(self
            .conn()?
            .query(
                "SELECT m.account_id, m.block_order_id
                   FROM book_block_member m
                   JOIN book_order o ON o.order_id = m.block_order_id
                  WHERE m.member_order_id = $1 AND o.state = ANY($2)
                  ORDER BY 2",
                &[
                    &member_order_id,
                    &vec![
                        meridian_domain::v1::OrderState::Proposed as i32,
                        meridian_domain::v1::OrderState::Released as i32,
                        meridian_domain::v1::OrderState::Working as i32,
                    ],
                ],
            )
            .map_err(unavailable)?
            .into_iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect())
    }

    fn live_order_instruments(&self) -> Result<Vec<String>> {
        Ok(self
            .conn()?
            .query(
                "SELECT instrument_id FROM book_order
                  WHERE account_id NOT LIKE '~%' AND state = ANY($1)",
                &[&vec![
                    meridian_domain::v1::OrderState::Proposed as i32,
                    meridian_domain::v1::OrderState::Released as i32,
                    meridian_domain::v1::OrderState::Working as i32,
                ]],
            )
            .map_err(unavailable)?
            .into_iter()
            .map(|row| row.get(0))
            .collect())
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

fn unavailable(failed: impl std::error::Error + 'static) -> StoreError {
    // With each cause, because this driver's own message for most failures
    // is the words "db error" and everything useful is one level down; and
    // its SQLSTATE, so a query cancelled at the serving session's
    // statement_timeout is answered with this component's own refusal for
    // load (the resilience spec's section 10a).
    StoreError::Unavailable(meridian_changes::serving::described(&failed))
}

/// Raise the epoch of every partition `groups`' group owns in `tx` -- every
/// partition, with one group -- past every epoch the book holds, and record
/// who raised it and why (step 3b, `WriterTookOver`): answer the epoch. What
/// a takeover does, and a writer on a new database timeline (contract v30,
/// stage C). Another group's partitions keep theirs (step 3d).
fn raise(
    tx: &mut postgres::Transaction<'_>,
    by: &meridian_changes::TookOver,
    groups: meridian_changes::groups::Groups,
) -> std::result::Result<u64, postgres::Error> {
    let owned: Vec<String> = tx
        .query(
            "SELECT partition FROM book_partition_head ORDER BY partition FOR UPDATE",
            &[],
        )?
        .iter()
        .map(|row| row.get::<_, String>(0))
        .filter(|partition| groups.owns(partition))
        .collect();
    let epoch: i64 = tx
        .query_one(
            "WITH raised AS (
                UPDATE book_partition_head
                   SET epoch = (SELECT max(epoch) + 1 FROM book_partition_head)
                 WHERE partition = ANY($1)
                RETURNING epoch)
             SELECT coalesce(max(epoch), (SELECT max(epoch) + 1 FROM book_partition_head))
               FROM raised",
            &[&owned],
        )?
        .get(0);
    // Who raised it and why, in the same transaction (step 3b).
    tx.execute(
        "INSERT INTO book_writer_took_over (epoch, member, partitions, why, origin, at_ns)
         VALUES ($1, $2, $3, $4, $5, $6)",
        &[
            &epoch,
            &by.member,
            &owned,
            &by.why,
            &meridian_changes::origin(),
            &by.at_ns,
        ],
    )?;
    Ok(epoch as u64)
}

/// The partition `groups`' group reads its epoch from: `control` with one
/// group or for group 0, else the first partition it owns; none where it
/// owns none.
fn anchor_of(
    client: &mut impl GenericClient,
    groups: meridian_changes::groups::Groups,
) -> Result<Option<String>> {
    if groups.holds_control() {
        return Ok(Some(CONTROL_PARTITION.to_string()));
    }
    Ok(client
        .query(
            "SELECT partition FROM book_partition_head ORDER BY partition",
            &[],
        )
        .map_err(unavailable)?
        .iter()
        .map(|row| row.get::<_, String>(0))
        .find(|partition| groups.owns(partition)))
}

/// The epochs of the partitions `groups`' group owns, sorted, and the set of
/// every other group's.
fn epochs_of(
    client: &mut impl GenericClient,
    groups: meridian_changes::groups::Groups,
) -> Result<(Vec<u64>, std::collections::BTreeSet<u64>)> {
    let mut mine = Vec::new();
    let mut others = std::collections::BTreeSet::new();
    for row in client
        .query("SELECT partition, epoch FROM book_partition_head", &[])
        .map_err(unavailable)?
    {
        let (partition, epoch): (String, i64) = (row.get(0), row.get(1));
        if groups.owns(&partition) {
            mine.push(epoch as u64);
        } else {
            others.insert(epoch as u64);
        }
    }
    mine.sort_unstable();
    Ok((mine, others))
}

/// End every other session holding a partition lock of `groups`' group
/// before its takeover raises their epoch (step 3d): its own deposed writer
/// stopped mid-batch, never another group's writer, which holds its own
/// partitions' locks.
fn end_deposed_holders(
    tx: &mut postgres::Transaction<'_>,
    groups: meridian_changes::groups::Groups,
) -> std::result::Result<i64, postgres::Error> {
    let partitions: Vec<String> = tx
        .query("SELECT partition FROM book_partition_head", &[])?
        .iter()
        .map(|row| row.get::<_, String>(0))
        .filter(|partition| groups.owns(partition))
        .collect();
    let keys: Vec<i64> = partitions.iter().map(|p| partition_lock(p)).collect();
    let ended: i64 = tx
        .query_one(
            "SELECT count(*) FILTER (WHERE pg_terminate_backend(pid, $2))
               FROM (SELECT DISTINCT l.pid
                       FROM pg_locks l
                      WHERE l.locktype = 'advisory' AND l.granted
                        AND l.pid <> pg_backend_pid()
                        AND ((l.classid::bigint << 32) | l.objid::bigint) = ANY($1)) AS held",
            &[&keys, &meridian_changes::DEPOSED_GONE_WITHIN_MS],
        )?
        .get(0);
    if ended > 0 {
        tracing::warn!(
            sessions = ended,
            "ended this writer group's deposed writer's sessions still holding its partitions' locks"
        );
    }
    Ok(ended)
}
